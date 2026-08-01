//! System tray module for Screen Time Manager
//! Handles the system tray icon and context menu

use std::mem::zeroed;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
                NOTIFYICONDATAW,
            },
            WindowsAndMessaging::*,
        },
    },
};

use crate::blocking::{extend_time, get_remaining_seconds, hide_blocking_overlay, show_blocking_overlay, BLOCKING_HWND};
use crate::constants::*;
use crate::database::{get_blocking_message, get_warning_config, is_pause_enabled};
use crate::dialogs::{show_settings_dialog, show_stats_dialog, verify_passcode_for_quit};
use crate::i18n;
use crate::mini_overlay::{is_paused, is_idle_paused, can_pause, toggle_pause, PauseBlockedReason, get_remaining_pause_budget};
use crate::overlay::{show_overlay, OVERLAY_HWND};
use crate::telegram;
use std::sync::atomic::Ordering;

/// Global state for the notification icon data
pub static mut NOTIFY_ICON_DATA: Option<NOTIFYICONDATAW> = None;

/// Registered id for the "TaskbarCreated" message, broadcast to every
/// top-level window whenever Explorer's shell (re)starts. Listened for in
/// `window_proc` to re-add the tray icon if it wasn't there yet - see
/// `add_tray_icon`'s retry comment for why that happens.
pub static mut WM_TASKBARCREATED: u32 = 0;

/// How many times to retry `Shell_NotifyIconW(NIM_ADD, ...)` before giving up
/// on this attempt (a later "TaskbarCreated" message - see WM_TASKBARCREATED
/// above - still retries again after that).
const TRAY_ICON_ADD_RETRIES: u32 = 5;

/// Add the system tray icon. Failure here is expected to happen occasionally
/// and is *not* fatal: `Shell_NotifyIconW(NIM_ADD, ...)` can transiently fail
/// right after logon if Explorer's shell notification area hasn't finished
/// starting up yet - most likely on a session that just started (a fresh
/// sign-in, especially the first-ever logon for an account, or one competing
/// for resources under Fast User Switching) rather than a desktop that's
/// been up for a while. This used to `panic!` on that failure, which - since
/// this crate builds with `panic = "abort"` - killed the *entire app*, not
/// just the icon, over what's normally a purely cosmetic, retryable hiccup:
/// screen-time enforcement has nothing to do with whether the tray icon
/// exists. Retries a few times immediately, and if it still hasn't worked,
/// logs and leaves the app running iconless rather than crashing it - the
/// WM_TASKBARCREATED handler in `window_proc` gets another chance later.
pub unsafe fn add_tray_icon(hwnd: HWND) {
    let hinstance = GetModuleHandleW(None).expect("Failed to get module handle");

    let hicon = LoadIconW(hinstance, PCWSTR(1 as *const u16))
        .or_else(|_| LoadIconW(None, IDI_APPLICATION))
        .expect("Failed to load icon");

    let tooltip = i18n::t("tray.tooltip");
    let mut tip_buffer: [u16; 128] = [0; 128];
    for (i, c) in tooltip.encode_utf16().enumerate() {
        if i >= 127 { break; }
        tip_buffer[i] = c;
    }

    let mut nid: NOTIFYICONDATAW = zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAYICON;
    nid.hIcon = hicon;
    nid.szTip = tip_buffer;

    for attempt in 1..=TRAY_ICON_ADD_RETRIES {
        if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            NOTIFY_ICON_DATA = Some(nid);
            return;
        }
        if attempt < TRAY_ICON_ADD_RETRIES {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
    eprintln!("[Tray] Shell_NotifyIconW(NIM_ADD) failed after {TRAY_ICON_ADD_RETRIES} attempts - continuing without a tray icon until TaskbarCreated fires");
}

/// Remove the system tray icon
pub unsafe fn remove_tray_icon() {
    if let Some(ref nid) = NOTIFY_ICON_DATA {
        let _ = Shell_NotifyIconW(NIM_DELETE, nid);
        NOTIFY_ICON_DATA = None;
    }
}

/// Show the context menu when right-clicking the tray icon. Every Win32 call
/// in here (and in show_context_menu_with_pause) used to `.expect()`/`panic!`
/// on failure - the same mistake that made a transient Shell_NotifyIconW
/// failure fatal (see add_tray_icon) - except this path runs on every single
/// right-click, not just once at startup, so a transient GDI/shell hiccup
/// here could kill the whole app at any time during normal use. Now these
/// just skip showing the menu for this click and log, since a missed menu
/// open is recoverable (the user can just click again) in a way that losing
/// the whole app is not.
pub unsafe fn show_context_menu(hwnd: HWND) {
    let Ok(hmenu) = CreatePopupMenu() else {
        eprintln!("[Tray] Failed to create context menu");
        return;
    };

    // Determine pause menu item text and state
    let paused = is_paused();
    let pause_enabled = is_pause_enabled();

    let (pause_text, pause_flags) = if paused {
        // Currently manually paused - show resume option (always available)
        (i18n::t("tray.resume"), MF_BYPOSITION | MF_STRING)
    } else if is_idle_paused() {
        // Currently idle-paused - grey out manual pause (already paused via idle)
        (i18n::t("tray.pause_idle"), MF_BYPOSITION | MF_STRING | MF_GRAYED)
    } else if !pause_enabled {
        // Pause feature disabled
        (i18n::t("tray.pause_disabled"), MF_BYPOSITION | MF_STRING | MF_GRAYED)
    } else {
        // Check if pause is available
        match can_pause() {
            Ok(()) => {
                let budget_mins = get_remaining_pause_budget() / 60;
                let text = format!("Pause Timer ({}m left)", budget_mins);
                // Need to leak the string for the menu (will be cleaned up with menu)
                let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                let ptr = wide.as_ptr();
                std::mem::forget(wide);
                return show_context_menu_with_pause(hwnd, hmenu, PCWSTR(ptr), MF_BYPOSITION | MF_STRING);
            }
            Err(PauseBlockedReason::BudgetExhausted) => {
                (i18n::t("tray.pause_budget_used"), MF_BYPOSITION | MF_STRING | MF_GRAYED)
            }
            Err(PauseBlockedReason::CooldownActive { seconds_remaining }) => {
                let mins = (seconds_remaining + 59) / 60; // Round up
                let text = format!("Pause ({}m cooldown)", mins);
                let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                let ptr = wide.as_ptr();
                std::mem::forget(wide);
                return show_context_menu_with_pause(hwnd, hmenu, PCWSTR(ptr), MF_BYPOSITION | MF_STRING | MF_GRAYED);
            }
            Err(PauseBlockedReason::MinActiveTimeNotMet { seconds_remaining }) => {
                let mins = (seconds_remaining + 59) / 60;
                let text = format!("Pause (wait {}m)", mins);
                let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                let ptr = wide.as_ptr();
                std::mem::forget(wide);
                return show_context_menu_with_pause(hwnd, hmenu, PCWSTR(ptr), MF_BYPOSITION | MF_STRING | MF_GRAYED);
            }
            Err(PauseBlockedReason::TimeTooLow) => {
                (i18n::t("tray.pause_time_low"), MF_BYPOSITION | MF_STRING | MF_GRAYED)
            }
            Err(PauseBlockedReason::Disabled) => {
                (i18n::t("tray.pause_disabled"), MF_BYPOSITION | MF_STRING | MF_GRAYED)
            }
        }
    };

    let pause_wide: Vec<u16> = pause_text.encode_utf16().chain(std::iter::once(0)).collect();
    show_context_menu_with_pause(hwnd, hmenu, PCWSTR(pause_wide.as_ptr()), pause_flags);
}

/// Helper to show context menu with pause item. See `show_context_menu` for
/// why failures here bail out (log + clean up + return) instead of panicking.
unsafe fn show_context_menu_with_pause(hwnd: HWND, hmenu: HMENU, pause_text: PCWSTR, pause_flags: MENU_ITEM_FLAGS) {
    // Bails out of the enclosing function on failure, after tearing down the
    // partially-built menu - a macro rather than a helper fn since an early
    // `return` needs to unwind this specific call site, not just report a bool.
    macro_rules! insert_or_bail {
        ($pos:expr, $flags:expr, $id:expr, $text:expr) => {
            if InsertMenuW(hmenu, $pos, $flags, $id, $text).is_err() {
                eprintln!("[Tray] Failed to build context menu - skipping this open");
                let _ = DestroyMenu(hmenu);
                return;
            }
        };
    }

    let stats_text = i18n::wide("tray.stats");
    insert_or_bail!(0, MF_BYPOSITION | MF_STRING, IDM_TODAYS_STATS as usize, PCWSTR(stats_text.as_ptr()));
    let settings_text = i18n::wide("tray.settings");
    insert_or_bail!(1, MF_BYPOSITION | MF_STRING, IDM_SETTINGS as usize, PCWSTR(settings_text.as_ptr()));
    insert_or_bail!(2, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
    let extend15_text = i18n::wide("tray.extend_15");
    insert_or_bail!(3, MF_BYPOSITION | MF_STRING, IDM_EXTEND_15 as usize, PCWSTR(extend15_text.as_ptr()));
    let extend45_text = i18n::wide("tray.extend_45");
    insert_or_bail!(4, MF_BYPOSITION | MF_STRING, IDM_EXTEND_45 as usize, PCWSTR(extend45_text.as_ptr()));
    insert_or_bail!(5, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());

    // Pause menu item with dynamic text
    insert_or_bail!(6, pause_flags, IDM_PAUSE_TOGGLE as usize, pause_text);

    let mut idx = 7;

    // Show idle status if idle-paused
    if is_idle_paused() {
        let idle_text = i18n::wide("tray.idle_paused");
        insert_or_bail!(idx, MF_BYPOSITION | MF_STRING | MF_GRAYED, 0, PCWSTR(idle_text.as_ptr()));
        idx += 1;
    }

    insert_or_bail!(idx, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
    idx += 1;
    let warning_text = i18n::wide("tray.show_warning");
    insert_or_bail!(idx, MF_BYPOSITION | MF_STRING, IDM_SHOW_OVERLAY as usize, PCWSTR(warning_text.as_ptr()));
    idx += 1;
    let blocking_text = i18n::wide("tray.show_blocking");
    insert_or_bail!(idx, MF_BYPOSITION | MF_STRING, IDM_SHOW_BLOCKING as usize, PCWSTR(blocking_text.as_ptr()));
    idx += 1;
    insert_or_bail!(idx, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
    idx += 1;
    let about_text = i18n::wide("tray.about");
    insert_or_bail!(idx, MF_BYPOSITION | MF_STRING, IDM_ABOUT as usize, PCWSTR(about_text.as_ptr()));
    idx += 1;
    let quit_text = i18n::wide("tray.quit");
    insert_or_bail!(idx, MF_BYPOSITION | MF_STRING, IDM_QUIT as usize, PCWSTR(quit_text.as_ptr()));

    let mut point = zeroed();
    if GetCursorPos(&mut point).is_err() {
        eprintln!("[Tray] Failed to get cursor position - skipping this open");
        let _ = DestroyMenu(hmenu);
        return;
    }

    let _ = SetForegroundWindow(hwnd);

    let _ = TrackPopupMenu(
        hmenu,
        TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
        point.x,
        point.y,
        0,
        hwnd,
        None,
    );

    DestroyMenu(hmenu).ok();
}

/// Main window procedure for handling tray events
pub unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if WM_TASKBARCREATED != 0 && msg == WM_TASKBARCREATED {
        // Explorer's shell just (re)started - re-add the icon whether it's
        // recovering from an Explorer restart or from add_tray_icon's own
        // initial attempts having exhausted their retries (see there).
        add_tray_icon(hwnd);
        return LRESULT(0);
    }

    match msg {
        WM_TRAYICON => {
            let event = lparam.0 as u32;
            match event {
                WM_RBUTTONUP | WM_LBUTTONUP => {
                    show_context_menu(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let menu_id = (wparam.0 & 0xFFFF) as u16;
            match menu_id {
                IDM_PAUSE_TOGGLE => {
                    // Toggle pause state (no passcode required - it's a child feature)
                    match toggle_pause() {
                        Ok(_is_now_paused) => {
                            // Success - UI will update automatically
                        }
                        Err(_reason) => {
                            // Should not happen since menu item should be grayed out
                            // But just in case, do nothing
                        }
                    }
                }
                IDM_SHOW_OVERLAY => {
                    let (minutes, message) = get_warning_config(1);
                    show_overlay(&message, minutes);
                }
                IDM_SHOW_BLOCKING => {
                    let message = get_blocking_message();
                    show_blocking_overlay(&message);
                }
                IDM_TODAYS_STATS => {
                    if verify_passcode_for_quit(hwnd) {
                        show_stats_dialog(hwnd);
                    }
                }
                IDM_SETTINGS => {
                    if verify_passcode_for_quit(hwnd) {
                        show_settings_dialog(hwnd);
                    }
                }
                IDM_EXTEND_15 => {
                    if verify_passcode_for_quit(hwnd) {
                        extend_time(15);
                        crate::time_request::notify_passcode_extend(
                            "passcode_extend.source.tray_menu",
                            15,
                            get_remaining_seconds(),
                        );
                    }
                }
                IDM_EXTEND_45 => {
                    if verify_passcode_for_quit(hwnd) {
                        extend_time(45);
                        crate::time_request::notify_passcode_extend(
                            "passcode_extend.source.tray_menu",
                            45,
                            get_remaining_seconds(),
                        );
                    }
                }
                IDM_ABOUT => {
                    let about_msg = i18n::wide("about.text");
                    let about_title = i18n::wide("window.about");
                    MessageBoxW(
                        hwnd,
                        PCWSTR(about_msg.as_ptr()),
                        PCWSTR(about_title.as_ptr()),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
                IDM_QUIT => {
                    if verify_passcode_for_quit(hwnd) {
                        // Tell the watchdog task this is a sanctioned stop, not
                        // the app being killed out from under the timer.
                        crate::database::mark_intentional_quit();
                        DestroyWindow(hwnd).ok();
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_QUERYENDSESSION => {
            // Windows is asking whether it's OK to shut down/log off. Mark
            // this session as ending right away, before allowing it to
            // proceed - a fast/forced shutdown can kill this process before
            // WM_ENDSESSION (or our own cleanup below) gets to run, and
            // without this marker the watchdog would otherwise treat an
            // ordinary shutdown or sign-out exactly like tampering and send
            // a false "unexpectedly stopped" alert instead of no alert at all.
            // Deliberately the time-bounded `mark_session_ending`, not the
            // indefinite `mark_intentional_quit` used by the passcode-Quit
            // menu item below - this session is ending regardless, so unlike
            // a deliberate Quit there's no "stay off until told otherwise"
            // intent to preserve, and an indefinite marker left over from
            // this shutdown could otherwise wrongly suppress the watchdog in
            // a *later* session too (see `database::mark_session_ending`).
            crate::database::mark_session_ending();
            LRESULT(1) // TRUE - allow the shutdown/logoff to proceed
        }
        WM_ENDSESSION => {
            // wParam is nonzero only if the session is actually ending (not
            // vetoed by some other app) - tear down the same way the
            // passcode-protected Quit does, so the admin chat gets the same
            // graceful "shutting down" notification instead of nothing (or,
            // without the WM_QUERYENDSESSION marker above, a tamper alert).
            if wparam.0 != 0 {
                DestroyWindow(hwnd).ok();
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // Signal Telegram bot to shut down (sends shutdown notification)
            telegram::signal_shutdown();

            // Signal Discord bot to shut down (sends shutdown notification)
            crate::discord::signal_shutdown();

            let overlay_hwnd = HWND(OVERLAY_HWND.load(Ordering::SeqCst));
            if !overlay_hwnd.0.is_null() {
                DestroyWindow(overlay_hwnd).ok();
            }
            let blocking_hwnd = HWND(BLOCKING_HWND.load(Ordering::SeqCst));
            if !blocking_hwnd.0.is_null() {
                hide_blocking_overlay();
                DestroyWindow(blocking_hwnd).ok();
            }
            remove_tray_icon();
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
