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
mod passcode_guard;
mod remote_commands;
mod session;
mod telegram;
mod time_request;
mod tray;
mod update;
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

use blocking::{create_blocking_overlay, create_secondary_overlays, register_blocking_class};
use constants::MUTEX_NAME;
use database::{load_remaining_time_for, get_current_weekday, get_daily_limit};
use mini_overlay::{create_mini_overlay, register_mini_overlay_class, show_mini_overlay};
use overlay::{create_overlay_window, register_overlay_class};
use tray::{add_tray_icon, remove_tray_icon, window_proc};

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
    // The SYSTEM-side updater run by the ScreenTimeManagerUpdate task -
    // handled before anything else (the panic hook included, which writes
    // under the profile directory): it must only ever write under
    // `<exe dir>\updates\`, see update.rs.
    if std::env::args().any(|a| a == update::UPDATE_ARG) {
        std::process::exit(update::run_update_mode());
    }

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
            // Started through the ScreenTimeManager task (its logon trigger,
            // a watchdog relaunch) while a copy already runs in this
            // session: that's the state wanted, so leave without a word.
            // Two starts can meet - e.g. the per-minute and the after-logon
            // watchdog runs - and a box there would be shown to the child.
            if std::env::args().any(|a| a == watchdog::QUIET_IF_RUNNING_ARG) {
                return;
            }
            MessageBoxW(
                None,
                w!("Screen Time Manager is already running."),
                w!("Already Running"),
                MB_OK | MB_ICONWARNING,
            );
            return;
        }

        // First heartbeat as soon as the mutex is ours, so the watchdog
        // never sees the mutex held with only a previous session's
        // heartbeat; the timer set up below keeps it fresh.
        watchdog::write_heartbeat();

        // A real start of the app - clear any leftover intentional-quit
        // marker so the watchdog task's suppression only ever covers the gap
        // between a sanctioned Quit and this moment (see database.rs).
        database::clear_intentional_quit_marker();
        // Likewise the watchdog's note that it ended a frozen copy: from
        // here on, the app stopping is a stop again (see watchdog.rs).
        watchdog::clear_watchdog_ended_record();

        // Lets the watchdog tell "not restarted yet after an install/update"
        // apart from "restarted since, then stopped again" (see watchdog.rs).
        database::record_app_started();

        // Initialize database. Never stops in a message box: that used to
        // leave the app holding the single-instance mutex while doing
        // nothing, which the watchdog took for a healthy app. A corrupt file
        // is moved aside and recreated; anything else falls back to a
        // temporary in-memory database; the admin is told either way (below,
        // once the bot settings are readable).
        let db_startup = database::init_database_with_recovery();
        if let database::DatabaseStartup::Failed { error } = &db_startup {
            // Exiting releases the mutex, so the watchdog notices and alerts.
            eprintln!("[Main] Could not set up any settings database: {error}");
            return;
        }

        // Initialize the shared machine-wide database (bot config, passcode,
        // per-user mirrored stats); safe to fail (e.g. missing permissions),
        // falls back to per-account storage
        database::init_shared_database();

        report_database_recovery(&db_startup);

        // The one date today's counters are loaded for, saved under, and
        // rolled over from (see database::with_counters_date).
        let today = database::effective_today_date();

        // Don't start today over on a fresh database - recreated, temporary,
        // or simply deleted - if the shared mirror shows today's use (see
        // there; a no-op when today already has its remaining time).
        if let Some(remaining) = database::restore_today_from_shared_mirror(&today) {
            eprintln!("[Main] Carried today's usage over from the shared database: {remaining}s left");
        }

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

        // Keep the watchdog's heartbeat fresh from this (UI) thread, so it
        // stops if the UI thread hangs or the process is suspended. The
        // timer calls heartbeat_timer_proc directly from any message loop
        // on this thread (including the dialogs' nested ones), so
        // tray::window_proc doesn't see it.
        if SetTimer(hwnd, HEARTBEAT_TIMER_ID, watchdog::HEARTBEAT_INTERVAL_MS, Some(heartbeat_timer_proc)) == 0 {
            eprintln!("[Main] Failed to start the heartbeat timer");
        }

        // Create the overlay windows (initially hidden)
        create_overlay_window(hinstance);
        create_blocking_overlay(hinstance);
        create_secondary_overlays(hinstance);  // Create overlays for secondary monitors
        create_mini_overlay(hinstance);

        // Initialize remaining time from database or daily limit, and the
        // session active time, both for `today`
        let remaining = load_remaining_time_for(&today).unwrap_or_else(|| {
            // No saved time for today, use daily limit
            let weekday = get_current_weekday();
            (get_daily_limit(weekday) * 60) as i32  // Convert minutes to seconds
        });
        let session_active = database::get_session_active_time_for(&today);
        mini_overlay::start_day(&today, remaining, session_active);
        // Follow a limit changed after today's time was saved (or record the
        // limit it reflects - see there).
        blocking::reconcile_todays_limit();
        let remaining = blocking::get_remaining_seconds();

        // The always-on per-second tick (countdown, rollover, idle, bots)
        mini_overlay::start_accounting(hwnd);

        // Show the mini overlay with remaining time
        show_mini_overlay();

        // If time is already exhausted, or a parent's lock is still on (it
        // outlasts restarts, reboots and updates - see blocking::MANUAL_LOCK),
        // show blocking overlay immediately
        blocking::load_manual_lock();
        if blocking::lock_screen_due_at_start(blocking::is_manual_lock(), remaining) {
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
        // case; the per-second accounting tick (see mini_overlay.rs)
        // repeats this call to react as the active session changes via Fast
        // User Switching - both calls are idempotent, so this isn't a race.
        session::sync_remote_bots();

        // Daily update check and the one-off "updated to vN" notice, on
        // background threads (see update.rs).
        update::start_background_tasks();

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

/// Timer id of the heartbeat timer on the main window ("HB").
const HEARTBEAT_TIMER_ID: usize = 0x4842;

unsafe extern "system" fn heartbeat_timer_proc(_hwnd: windows::Win32::Foundation::HWND, _msg: u32, _id: usize, _time: u32) {
    watchdog::write_heartbeat();
}

/// Tell the admin if the per-account database had to be recreated or
/// replaced by a temporary one (see `database::init_database_with_recovery`).
fn report_database_recovery(startup: &database::DatabaseStartup) {
    let (error, outcome) = match startup {
        database::DatabaseStartup::Recreated { moved_to, error } => {
            eprintln!("[Main] Settings database was unreadable ({error}); moved to {}", moved_to.display());
            (error, i18n::t("db.alert.recreated"))
        }
        database::DatabaseStartup::Temporary { error } => {
            eprintln!("[Main] Settings database could not be opened ({error}); using a temporary one");
            (error, i18n::t("db.alert.temporary"))
        }
        _ => return,
    };
    let text = format!(
        "⚠️ {} {} {} ({})\n{}",
        i18n::t("db.alert.prefix"),
        remote_commands::current_windows_username(),
        i18n::t("db.alert.cannot_open"),
        error,
        outcome,
    );
    watchdog::raise_alert(&text);
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
