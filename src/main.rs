//! Screen Time Manager - A system tray application for Windows
//!
//! This application runs in the background with only a system tray icon visible.
//! Right-clicking the icon shows a context menu with options including quit.

#![windows_subsystem = "windows"]

mod blocking;
mod constants;
mod database;
mod dialogs;
mod discord;
mod dpi;
mod i18n;
mod mini_overlay;
mod overlay;
mod remote_commands;
mod session;
mod telegram;
mod time_request;
mod tray;
mod watchdog;

use std::mem::zeroed;
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::{BOOL, GetLastError, CloseHandle, ERROR_ALREADY_EXISTS},
        System::{
            LibraryLoader::GetModuleHandleW,
            RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION},
            Threading::CreateMutexW,
        },
        UI::HiDpi::{SetProcessDpiAwareness, PROCESS_PER_MONITOR_DPI_AWARE},
        UI::WindowsAndMessaging::*,
    },
};

use blocking::{create_blocking_overlay, create_secondary_overlays, register_blocking_class, REMAINING_SECONDS};
use constants::MUTEX_NAME;
use database::{init_database, load_remaining_time, get_current_weekday, get_daily_limit};
use mini_overlay::{create_mini_overlay, register_mini_overlay_class, show_mini_overlay};
use overlay::{create_overlay_window, register_overlay_class};
use tray::{add_tray_icon, remove_tray_icon, window_proc};
use std::sync::atomic::Ordering;

/// Log a panic's message and source location before the process aborts
/// (`panic = "abort"` in Cargo.toml still runs this hook first - it just
/// skips unwinding after). Temporary diagnostic aid: this is a
/// `windows_subsystem = "windows"` binary with no console, so the default
/// hook's stderr output normally goes nowhere and a panic here is otherwise
/// silent.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let path = dirs::data_local_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".screen-time-manager")
            .join("panic.log");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            use std::io::Write;
            let _ = writeln!(f, "[{:?}] {info}", std::time::SystemTime::now());
        }
    }));
}

fn main() {
    install_panic_hook();

    // A brief, separate invocation from the watchdog Scheduled Task - just
    // check on the real app and exit, no GUI setup at all.
    if std::env::args().any(|a| a == watchdog::WATCHDOG_ARG) {
        watchdog::run_check();
        return;
    }

    unsafe {
        // Set DPI awareness before creating any windows
        let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
        dpi::init_dpi();

        // Check for single instance
        if !ensure_single_instance() {
            MessageBoxW(
                None,
                w!("Screen Time Manager is already running."),
                w!("Already Running"),
                MB_OK | MB_ICONWARNING,
            );
            return;
        }

        // A real start of the app - clear any leftover intentional-quit
        // marker so the watchdog task's suppression only ever covers the gap
        // between a sanctioned Quit and this moment (see database.rs).
        database::clear_intentional_quit_marker();

        // Initialize database
        if let Err(e) = init_database() {
            let msg: Vec<u16> = format!("Failed to initialize database: {}\0", e)
                .encode_utf16()
                .collect();
            MessageBoxW(
                None,
                PCWSTR(msg.as_ptr()),
                w!("Database Error"),
                MB_OK | MB_ICONERROR,
            );
            return;
        }

        // Initialize the shared machine-wide database (bot config, passcode,
        // per-user mirrored stats); safe to fail (e.g. missing permissions),
        // falls back to per-account storage
        database::init_shared_database();

        // Housekeeping: drop per-day usage/enforcement rows older than the
        // retention window now that both databases are open (prunes the
        // shared per-user mirrored stats too - see prune_old_daily_data).
        database::prune_old_daily_data();

        // Get the module handle
        let hinstance = GetModuleHandleW(None).expect("Failed to get module handle");

        // Register main window class
        let class_name = w!("ScreenTimeManagerClass");
        let wnd_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..zeroed()
        };

        if RegisterClassW(&wnd_class) == 0 {
            panic!("Failed to register window class");
        }

        // Register overlay and blocking window classes
        register_overlay_class(hinstance);
        register_blocking_class(hinstance);
        register_mini_overlay_class(hinstance);

        // Create a hidden window for message handling
        let hwnd = CreateWindowExW(
            Default::default(),
            class_name,
            w!("Screen Time Manager"),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            hinstance,
            None,
        )
        .expect("Failed to create window");

        // Subscribe to this session's lock/unlock notifications (delivered
        // as WM_WTSSESSION_CHANGE to `hwnd`, handled in tray::window_proc) so
        // the countdown can pause while the screen is locked - not fatal if
        // it fails, same reasoning as add_tray_icon: this is a defense-in-
        // depth feature, not core functionality, so losing it shouldn't take
        // the whole app down.
        if let Err(e) = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) {
            eprintln!("[Main] Failed to register for session lock notifications: {e}");
        }

        // Create the overlay windows (initially hidden)
        create_overlay_window(hinstance);
        create_blocking_overlay(hinstance);
        create_secondary_overlays(hinstance);  // Create overlays for secondary monitors
        create_mini_overlay(hinstance);

        // Initialize remaining time from database or daily limit
        let remaining = load_remaining_time().unwrap_or_else(|| {
            // No saved time for today, use daily limit
            let weekday = get_current_weekday();
            (get_daily_limit(weekday) * 60) as i32  // Convert minutes to seconds
        });
        REMAINING_SECONDS.store(remaining, Ordering::SeqCst);

        // Initialize session active time from database
        let session_active = database::get_session_active_time();
        mini_overlay::SESSION_ACTIVE_SECONDS.store(session_active, Ordering::SeqCst);

        // Show the mini overlay with remaining time
        show_mini_overlay();

        // If time is already exhausted, show blocking overlay immediately
        if remaining <= 0 {
            let msg = database::get_blocking_message();
            blocking::show_blocking_overlay(&msg);
        }

        // Registered before the first add_tray_icon call so a Shell restart
        // (or the shell simply not being ready yet at this first attempt -
        // see add_tray_icon's retry logic) is caught by window_proc for as
        // long as this window lives, not just after some later point.
        tray::WM_TASKBARCREATED = RegisterWindowMessageW(w!("TaskbarCreated"));

        // Add the system tray icon
        add_tray_icon(hwnd);

        // Start the Telegram/Discord bots, but only if this session currently
        // owns the console - the bot token/channel is shared across every
        // Windows account, so with two sessions active concurrently, each one
        // unconditionally starting its own bot connection makes commands get
        // answered twice (or, for Telegram's single-poller getUpdates, fought
        // over). Called here for a fast start on the common single-session
        // case; the mini overlay's per-second timer (see mini_overlay.rs)
        // repeats this call to react as the active session changes via Fast
        // User Switching - both calls are idempotent, so this isn't a race.
        session::sync_remote_bots();

        // Message loop
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Cleanup: remove the tray icon
        remove_tray_icon();
    }
}

/// Ensures only one instance of the application is running
unsafe fn ensure_single_instance() -> bool {
    let mutex_name: Vec<u16> = MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();

    let handle = CreateMutexW(
        None,
        BOOL::from(true),
        PCWSTR(mutex_name.as_ptr()),
    );

    match handle {
        Ok(h) => {
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(h);
                false
            } else {
                true
            }
        }
        Err(_) => false,
    }
}
