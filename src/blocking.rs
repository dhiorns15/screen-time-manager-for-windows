//! Blocking overlay module
//! Full-screen overlay that requires passcode to dismiss

use std::mem::zeroed;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU8, Ordering};
use std::sync::Mutex;
use windows::{
    core::w,
    Win32::{
        Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM, CloseHandle},
        Graphics::Gdi::{
            BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
            CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint,
            EnumDisplayMonitors, FillRect, InvalidateRect, RoundRect, SelectObject, SetBkMode,
            SetTextColor, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, FW_BOLD, FW_NORMAL,
            HDC, HFONT, HGDIOBJ, HMONITOR, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
        },
        Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC},
        System::LibraryLoader::GetModuleHandleW,
        System::Shutdown::{ExitWindowsEx, EWX_LOGOFF, EWX_SHUTDOWN, SHUTDOWN_REASON},
        System::Threading::{GetCurrentProcess, GetCurrentThreadId, OpenProcessToken},
        Security::{
            AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
            TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
        },
        UI::{
            Controls::*,
            Input::KeyboardAndMouse::{EnableWindow, SetFocus, VK_RETURN},
            WindowsAndMessaging::*,
        },
    },
};

use windows::core::PCWSTR;

use crate::constants::*;
use crate::database::{get_discord_config, get_telegram_config};
use crate::dpi::scale;
use crate::i18n;
use crate::passcode_guard::{self, Attempt};

/// Initiates a Windows shutdown with proper privilege handling
unsafe fn initiate_shutdown() -> bool {
    // Open the process token
    let mut token_handle = std::mem::zeroed();
    if OpenProcessToken(
        GetCurrentProcess(),
        TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
        &mut token_handle,
    ).is_err() {
        return false;
    }

    // Look up the shutdown privilege
    let mut luid = std::mem::zeroed();
    if LookupPrivilegeValueW(None, w!("SeShutdownPrivilege"), &mut luid).is_err() {
        let _ = CloseHandle(token_handle);
        return false;
    }

    // Enable the privilege
    let tp = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [windows::Win32::Security::LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };

    if AdjustTokenPrivileges(token_handle, false, Some(&tp), 0, None, None).is_err() {
        let _ = CloseHandle(token_handle);
        return false;
    }

    let _ = CloseHandle(token_handle);

    // Now perform the shutdown
    ExitWindowsEx(EWX_SHUTDOWN, SHUTDOWN_REASON(0)).is_ok()
}

/// Secondary monitor blanker windows (raw HWNDs as isize for Send+Sync) and
/// the non-primary monitor layout they were built for. `layout` records every
/// monitor rect enumerated at build time, whether or not its window could be
/// created, so `sync_monitor_layout` only rebuilds when the monitors
/// themselves change - not twice a second because one create failed.
///
/// Only touched on the UI thread: show/hide_blocking_overlay marshal their
/// window work there (see `WM_APP_SHOW_OVERLAY`), so no bot thread ever holds
/// this lock across a cross-thread ShowWindow/SetWindowPos - which would
/// deadlock against the UI thread blocking on the same lock in its timer.
struct SecondaryOverlays {
    hwnds: Vec<isize>,
    layout: Vec<RECT>,
}
static SECONDARY_OVERLAYS: Mutex<SecondaryOverlays> =
    Mutex::new(SecondaryOverlays { hwnds: Vec::new(), layout: Vec::new() });

/// Fonts owned by the blocking overlay's child controls (shared bold button
/// font, large passcode font, small request-note font), as raw HFONTs. Kept
/// so a DPI change can replace them and free the old ones.
static CHILD_FONTS: Mutex<[isize; 3]> = Mutex::new([0; 3]);

/// Monitor information collected during enumeration
struct MonitorInfo {
    rect: RECT,
    is_primary: bool,
}

/// Global state for blocking overlay
pub static BLOCKING_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
pub static BLOCKING_TEXT: Mutex<Option<String>> = Mutex::new(None);
pub static BLOCKING_EDIT_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// What the line under the lock screen's passcode field says after the last
/// attempt (a `PasscodeNotice` as u8).
static PASSCODE_NOTICE: AtomicU8 = AtomicU8::new(PasscodeNotice::None as u8);

/// The lock screen's passcode feedback, from the last attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum PasscodeNotice {
    None = 0,
    Incorrect = 1,
    /// Entry is locked after too many wrong codes; the time left is read
    /// from `passcode_guard` whenever it's drawn.
    LockedOut = 2,
    /// The passcode couldn't be read, so nothing was unlocked.
    Unavailable = 3,
}

impl PasscodeNotice {
    fn after(attempt: Attempt) -> Self {
        match attempt {
            Attempt::Correct => PasscodeNotice::None,
            Attempt::Wrong { locked_for_secs: 0 } => PasscodeNotice::Incorrect,
            Attempt::Wrong { .. } | Attempt::LockedOut { .. } => PasscodeNotice::LockedOut,
            Attempt::Unavailable => PasscodeNotice::Unavailable,
        }
    }

    fn current() -> Self {
        match PASSCODE_NOTICE.load(Ordering::SeqCst) {
            1 => PasscodeNotice::Incorrect,
            2 => PasscodeNotice::LockedOut,
            3 => PasscodeNotice::Unavailable,
            _ => PasscodeNotice::None,
        }
    }

    fn set(self) {
        PASSCODE_NOTICE.store(self as u8, Ordering::SeqCst);
    }

    /// The text to show, given the lockout seconds left right now. A lockout
    /// that has run out shows nothing: the next code will be checked.
    fn text(self, lockout_left: u64) -> Option<String> {
        match self {
            PasscodeNotice::None => None,
            PasscodeNotice::Incorrect => Some(i18n::t("blocking.incorrect").to_string()),
            PasscodeNotice::LockedOut if lockout_left == 0 => None,
            PasscodeNotice::LockedOut => Some(format!(
                "{} {}",
                i18n::t("passcode.locked"),
                passcode_guard::format_wait(lockout_left)
            )),
            PasscodeNotice::Unavailable => Some(i18n::t("passcode.unavailable").to_string()),
        }
    }
}

/// "Request more time" button/edit, and how many cooldown seconds remain
/// before it can be used again (0 = ready)
static REQUEST_TIME_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static REQUEST_NOTE_EDIT_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static REQUEST_COOLDOWN_REMAINING: AtomicI32 = AtomicI32::new(0);
const REQUEST_COOLDOWN_SECONDS: i32 = 300;

/// Remaining time in seconds (negative means no limit/extension active)
pub static REMAINING_SECONDS: AtomicI32 = AtomicI32::new(-1);

/// Shutdown countdown in seconds (negative means inactive)
pub static SHUTDOWN_COUNTDOWN_SECONDS: AtomicI32 = AtomicI32::new(-1);

/// Get remaining time in seconds
pub fn get_remaining_seconds() -> i32 {
    REMAINING_SECONDS.load(Ordering::SeqCst)
}

/// Posted to the blocking window by show/hide_blocking_overlay when they're
/// called off the UI thread (the Discord/Telegram command handlers). The
/// caller updates the shared state synchronously - so e.g. cmd_unlock's
/// "is it locked?" check sees the result immediately - and the window work
/// runs when the UI thread picks the message up. Handlers apply whatever the
/// state says *then*, so a show and a hide posted back to back converge on
/// the last one.
const WM_APP_SHOW_OVERLAY: u32 = WM_APP + 1;
const WM_APP_HIDE_OVERLAY: u32 = WM_APP + 2;

/// Timer IDs
pub const TIMER_REASSERT_TOPMOST: usize = 2;
pub const TIMER_COUNTDOWN: usize = 3;
pub const TIMER_REQUEST_COOLDOWN: usize = 4;

/// Control IDs
const ID_PASSCODE_EDIT: i32 = 101;
const ID_UNLOCK_BUTTON: i32 = 102;
const ID_EXTEND_15: i32 = 103;
const ID_EXTEND_30: i32 = 104;
const ID_EXTEND_60: i32 = 105;
const ID_SHUTDOWN_BUTTON: i32 = 106;
const ID_REQUEST_NOTE_EDIT: i32 = 107;
const ID_REQUEST_TIME_BUTTON: i32 = 108;

pub unsafe fn create_blocking_overlay(hinstance: windows::Win32::Foundation::HMODULE) {
    let class_name = w!("ScreenTimeBlockingClass");

    let screen_width = GetSystemMetrics(SM_CXSCREEN);
    let screen_height = GetSystemMetrics(SM_CYSCREEN);

    let ex_style = WS_EX_TOPMOST | WS_EX_TOOLWINDOW;

    let window_title = i18n::wide("window.blocking");
    let hwnd = CreateWindowExW(
        ex_style,
        class_name,
        PCWSTR(window_title.as_ptr()),
        WS_POPUP | WS_CLIPCHILDREN,  // CLIPCHILDREN prevents drawing over child controls
        0,
        0,
        screen_width,
        screen_height,
        None,
        None,
        hinstance,
        None,
    )
    .expect("Failed to create blocking overlay");

    BLOCKING_HWND.store(hwnd.0, Ordering::SeqCst);
}

/// Shows the full-screen blocking overlay
pub unsafe fn show_blocking_overlay(text: &str) {
    show_blocking_overlay_with_time(text, -1);
}

/// Shows the full-screen blocking overlay with optional remaining time in seconds.
/// Safe to call from any thread: the state changes happen here, the window
/// work on the UI thread (see `WM_APP_SHOW_OVERLAY`).
pub unsafe fn show_blocking_overlay_with_time(text: &str, remaining_seconds: i32) {
    let hwnd = HWND(BLOCKING_HWND.load(Ordering::SeqCst));
    if hwnd.0.is_null() {
        return;
    }
    set_overlay_shown(text, remaining_seconds);
    present_overlay(hwnd);
}

/// Shows the overlay because the time has run out - unless time arrived in
/// the meantime. A bot's extension landing between the caller's "no time
/// left" check and the show used to leave the overlay up with time remaining:
/// the bot's hide ran before the overlay counted as shown, so it hid nothing.
/// Returns whether the overlay is up. Same threading as
/// `show_blocking_overlay_with_time`.
pub unsafe fn show_out_of_time_overlay(text: &str) -> bool {
    let hwnd = HWND(BLOCKING_HWND.load(Ordering::SeqCst));
    if hwnd.0.is_null() {
        return false;
    }
    let shown = show_unless_time_arrived(
        || set_overlay_shown(text, -1),
        // A read-modify-write rather than a load: it reads the latest value,
        // and an extension after it (`extend_time` is one too) synchronizes
        // with it - so either this sees the extension, or the extension's
        // hide sees the overlay as shown and hides it.
        || REMAINING_SECONDS.fetch_add(0, Ordering::SeqCst),
        set_overlay_hidden,
    );
    if shown {
        present_overlay(hwnd);
    }
    shown
}

/// Mark the overlay shown (`show_state`), then look at the remaining time
/// again: still none, it stays shown (true); any other value - an extension,
/// or "no limit" - and it's unmarked (`undo`) before any window shows, so
/// nothing flickers, and false. Nothing is retried: the next tick's
/// `zero_time_lock_due` check sees the time and doesn't try again.
pub(crate) fn show_unless_time_arrived(
    show_state: impl FnOnce(),
    remaining_now: impl FnOnce() -> i32,
    undo: impl FnOnce(),
) -> bool {
    show_state();
    if remaining_now() == 0 {
        true
    } else {
        undo();
        false
    }
}

/// The state side of showing the overlay (any thread).
fn set_overlay_shown(text: &str, remaining_seconds: i32) {
    *BLOCKING_TEXT.lock().unwrap() = Some(text.to_string());
    PasscodeNotice::None.set();
    if remaining_seconds >= 0 {
        REMAINING_SECONDS.store(remaining_seconds, Ordering::SeqCst);
    }

    // Initialize shutdown countdown from database setting (never under a
    // minute - see database::get_lock_screen_timeout)
    SHUTDOWN_COUNTDOWN_SECONDS.store(crate::database::get_lock_screen_timeout(), Ordering::SeqCst);
    COUNTDOWN_SIGNS_OUT.store(
        countdown_end_action(crate::session::other_users_logged_on()) == CountdownEnd::SignOut,
        Ordering::SeqCst,
    );
}

/// Undo `set_overlay_shown` before the window was shown.
fn set_overlay_hidden() {
    *BLOCKING_TEXT.lock().unwrap() = None;
    SHUTDOWN_COUNTDOWN_SECONDS.store(-1, Ordering::SeqCst);
}

/// Show the window now (UI thread) or have the UI thread do it.
unsafe fn present_overlay(hwnd: HWND) {
    if !on_ui_thread(hwnd) {
        let _ = PostMessageW(hwnd, WM_APP_SHOW_OVERLAY, WPARAM(0), LPARAM(0));
        return;
    }
    apply_show_overlay(hwnd);
}

/// Whether the calling thread owns the given window - i.e. is the UI thread,
/// the only one that may create, destroy, show or hide our windows without a
/// synchronous cross-thread SendMessage underneath.
pub(crate) unsafe fn on_ui_thread(hwnd: HWND) -> bool {
    GetWindowThreadProcessId(hwnd, None) == GetCurrentThreadId()
}

/// The window side of showing the overlay. UI thread only.
unsafe fn apply_show_overlay(hwnd: HWND) {
    if !is_blocking_overlay_visible() {
        // A hide was requested after this show was posted; it wins.
        return;
    }

    // Hide the mini overlay while blocking screen is shown (time keeps being
    // tracked by the main window's accounting tick - see mini_overlay)
    crate::mini_overlay::hide_mini_overlay();

    let edit_ptr = BLOCKING_EDIT_HWND.load(Ordering::SeqCst);
    if !edit_ptr.is_null() {
        SetWindowTextW(HWND(edit_ptr), w!("")).ok();
    }

    let note_ptr = REQUEST_NOTE_EDIT_HWND.load(Ordering::SeqCst);
    if !note_ptr.is_null() {
        SetWindowTextW(HWND(note_ptr), w!("")).ok();
    }

    let _ = InvalidateRect(hwnd, None, false);

    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        0, 0, 0, 0,
        SWP_SHOWWINDOW | SWP_NOMOVE | SWP_NOSIZE,
    ).ok();

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);

    if !edit_ptr.is_null() {
        let _ = SetFocus(HWND(edit_ptr));
    }

    let _ = PlaySoundW(w!("SystemHand"), None, SND_ALIAS | SND_ASYNC);
    let _ = SetTimer(hwnd, TIMER_REASSERT_TOPMOST, 500, None);

    // Start countdown timer (updates every second)
    let _ = SetTimer(hwnd, TIMER_COUNTDOWN, 1000, None);

    // Show secondary monitor overlays (blanks other monitors)
    show_secondary_overlays();
}

/// Remaining time after extending `current` by `additional_seconds`.
fn extended(current: i32, additional_seconds: i32) -> i32 {
    if current < 0 {
        // No timer was running, start fresh
        additional_seconds
    } else {
        // Add to existing time
        current.saturating_add(additional_seconds)
    }
}

/// Remaining time after reducing `current` by `reduction_seconds`.
fn reduced(current: i32, reduction_seconds: i32) -> i32 {
    current.saturating_sub(reduction_seconds).max(0)
}

/// Extend the remaining time by the specified minutes. One atomic update, so
/// the countdown tick on the UI thread can't overwrite an extension made from
/// a bot thread at the same moment. Saved at once: while nothing is counting
/// (paused, idle, locked) nothing else would save it, and a kill, crash or
/// update then brought the old value back.
pub fn extend_time(minutes: i32) {
    crate::mini_overlay::roll_over_if_due();
    let additional_seconds = minutes * 60;
    let _ = REMAINING_SECONDS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
        Some(extended(current, additional_seconds))
    });
    crate::mini_overlay::persist_remaining();
    unsafe { crate::mini_overlay::update_mini_overlay() };
}

/// Reduce the remaining time by the specified minutes (atomically, and
/// saved at once, as above)
pub fn reduce_time(minutes: i32) {
    crate::mini_overlay::roll_over_if_due();
    let reduction_seconds = minutes * 60;
    let _ = REMAINING_SECONDS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
        Some(reduced(current, reduction_seconds))
    });
    crate::mini_overlay::persist_remaining();
    unsafe { crate::mini_overlay::update_mini_overlay() };
}

/// Set the remaining time back to today's full limit (bot `reset`, Stats
/// "Reset Timer"), saved at once. Returns the limit in seconds.
pub fn reset_to_daily_limit() -> i32 {
    crate::mini_overlay::roll_over_if_due();
    let limit_minutes = crate::database::get_daily_limit(crate::database::get_current_weekday());
    let limit_seconds = (limit_minutes * 60) as i32;
    REMAINING_SECONDS.store(limit_seconds, Ordering::SeqCst);
    crate::mini_overlay::persist_remaining();
    unsafe { crate::mini_overlay::update_mini_overlay() };
    limit_seconds
}

/// Remaining time capped to a new daily limit, or `None` if it's already
/// within it (including "no limit running", which is negative).
fn capped(current: i32, limit_seconds: i32) -> Option<i32> {
    (current > limit_seconds).then_some(limit_seconds)
}

/// Cap the remaining time to `limit_seconds` (a daily limit that was just
/// lowered), atomically as above, so a countdown tick or an extension on
/// another thread can't be lost between the check and the store. Saves and
/// redraws if it changed anything; returns whether it did.
pub fn cap_remaining_time(limit_seconds: i32) -> bool {
    let changed = REMAINING_SECONDS
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| capped(current, limit_seconds))
        .is_ok();
    if changed {
        crate::mini_overlay::persist_remaining();
        unsafe {
            crate::mini_overlay::update_mini_overlay();
        }
    }
    changed
}

/// The countdown only runs while this session is the one at the console and
/// unlocked - i.e. while someone can actually see it. A session switched
/// away under Fast User Switching, or sitting at the Windows lock screen,
/// waits.
fn countdown_should_run(is_console_session: bool, session_locked: bool) -> bool {
    is_console_session && !session_locked
}

/// What happens when the lock-screen countdown runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CountdownEnd {
    /// Nobody else is signed in: shut the machine down, as always.
    Shutdown,
    /// Another account is signed in (Fast User Switching): sign only this
    /// session out, rather than shutting down on top of someone else's work.
    SignOut,
}

fn countdown_end_action(other_users_logged_on: bool) -> CountdownEnd {
    if other_users_logged_on {
        CountdownEnd::SignOut
    } else {
        CountdownEnd::Shutdown
    }
}

/// Whether the countdown will sign out rather than shut down, as last
/// checked by TIMER_COUNTDOWN - read by WM_PAINT for the countdown label.
static COUNTDOWN_SIGNS_OUT: AtomicBool = AtomicBool::new(false);

/// Sign this session out (the countdown's end when others are signed in).
unsafe fn initiate_sign_out() -> bool {
    ExitWindowsEx(EWX_LOGOFF, SHUTDOWN_REASON(0)).is_ok()
}

/// Format seconds into a human-readable string (e.g., "1h 30m 45s")
fn format_time(seconds: i32) -> String {
    if seconds < 0 {
        return String::from("--:--");
    }

    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;

    if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, secs)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, secs)
    } else {
        format!("{}s", secs)
    }
}

/// Hides the blocking overlay. Safe to call from any thread, like
/// `show_blocking_overlay_with_time`.
pub unsafe fn hide_blocking_overlay() {
    let hwnd = HWND(BLOCKING_HWND.load(Ordering::SeqCst));
    if hwnd.0.is_null() {
        return;
    }

    *BLOCKING_TEXT.lock().unwrap() = None;

    // Reset shutdown countdown
    SHUTDOWN_COUNTDOWN_SECONDS.store(-1, Ordering::SeqCst);

    // Save remaining time to database
    crate::mini_overlay::persist_remaining();

    if !on_ui_thread(hwnd) {
        let _ = PostMessageW(hwnd, WM_APP_HIDE_OVERLAY, WPARAM(0), LPARAM(0));
        return;
    }
    apply_hide_overlay(hwnd);
}

/// The window side of hiding the overlay. UI thread only.
unsafe fn apply_hide_overlay(hwnd: HWND) {
    if is_blocking_overlay_visible() {
        // A show was requested after this hide was posted; it wins.
        return;
    }

    let _ = KillTimer(hwnd, TIMER_REASSERT_TOPMOST);
    let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
    let _ = ShowWindow(hwnd, SW_HIDE);

    // Hide secondary monitor overlays
    hide_secondary_overlays();

    // Show mini overlay again if there's remaining time
    if REMAINING_SECONDS.load(Ordering::SeqCst) > 0 {
        crate::mini_overlay::show_mini_overlay();
    }
}

/// Whether the blocking overlay is currently shown
pub fn is_blocking_overlay_visible() -> bool {
    BLOCKING_TEXT.lock().unwrap().is_some()
}

/// Check the code typed on the lock screen - attempt-limited like the tray's
/// passcode prompt (see `passcode_guard`) - and set the notice under the
/// field to match.
unsafe fn check_blocking_passcode() -> bool {
    let edit_ptr = BLOCKING_EDIT_HWND.load(Ordering::SeqCst);
    if edit_ptr.is_null() {
        return false;
    }
    let mut buffer = [0u16; 16];
    let len = GetWindowTextW(HWND(edit_ptr), &mut buffer);
    let entered: String = String::from_utf16_lossy(&buffer[..len as usize]);

    let attempt = passcode_guard::attempt(&entered);
    PasscodeNotice::after(attempt).set();
    attempt.is_correct()
}

/// After a code that didn't unlock: show the notice, clear the field for
/// the next try and beep.
unsafe fn reject_passcode(hwnd: HWND) {
    let _ = InvalidateRect(hwnd, None, false);
    let edit_ptr = BLOCKING_EDIT_HWND.load(Ordering::SeqCst);
    if !edit_ptr.is_null() {
        let edit = HWND(edit_ptr);
        SetWindowTextW(edit, w!("")).ok();
        let _ = SetFocus(edit);
    }
    let _ = PlaySoundW(w!("SystemExclamation"), None, SND_ALIAS | SND_ASYNC);
}

/// Where the passcode notice is drawn, for a panel at (`panel_x`, `panel_y`).
fn passcode_notice_rect(panel_x: i32, panel_y: i32, panel_width: i32, panel_height: i32) -> RECT {
    RECT {
        left: panel_x,
        top: panel_y + panel_height - scale(40),
        right: panel_x + panel_width,
        bottom: panel_y + panel_height - scale(15),
    }
}

// The passcode EDIT control owns keyboard focus once created, so WM_KEYDOWN
// never reaches blocking_overlay_proc below it (Win32 routes key messages to
// the focused window, not its parent) - the VK_RETURN handling further down
// would otherwise be dead code. Subclassing the edit control to relay just
// that key up to the parent lets the existing handler do the rest unchanged.
unsafe extern "system" fn passcode_edit_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_KEYDOWN && wparam.0 == VK_RETURN.0 as usize {
        if let Ok(parent) = GetParent(hwnd) {
            SendMessageW(parent, msg, wparam, lparam);
        }
        return LRESULT(0);
    }
    let orig_proc: WNDPROC = std::mem::transmute(GetWindowLongPtrW(hwnd, GWLP_USERDATA));
    CallWindowProcW(orig_proc, hwnd, msg, wparam, lparam)
}

pub unsafe extern "system" fn blocking_overlay_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinstance = GetModuleHandleW(None).unwrap();
            let button = WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32);

            // Controls are created unplaced; layout_children positions and
            // sizes them (and does so again after display/DPI changes).
            // Layout: Icon -> Title -> Countdown -> Message -> Extend label -> Extend buttons -> Separator -> Passcode section

            // Extend time buttons
            create_child(hwnd, hinstance, w!("BUTTON"), &i18n::wide("blocking.extend_15"), button, ID_EXTEND_15);
            create_child(hwnd, hinstance, w!("BUTTON"), &i18n::wide("blocking.extend_30"), button, ID_EXTEND_30);
            create_child(hwnd, hinstance, w!("BUTTON"), &i18n::wide("blocking.extend_60"), button, ID_EXTEND_60);

            // Passcode edit control
            if let Some(e) = create_child(
                hwnd,
                hinstance,
                w!("EDIT"),
                &[0],
                WS_CHILD | WS_VISIBLE | WS_BORDER
                    | WINDOW_STYLE(ES_CENTER as u32 | ES_PASSWORD as u32 | ES_NUMBER as u32),
                ID_PASSCODE_EDIT,
            ) {
                BLOCKING_EDIT_HWND.store(e.0, Ordering::SeqCst);
                SendMessageW(e, EM_SETLIMITTEXT, WPARAM(4), LPARAM(0));
                let orig = SetWindowLongPtrW(e, GWLP_WNDPROC, passcode_edit_subclass_proc as *const () as isize);
                SetWindowLongPtrW(e, GWLP_USERDATA, orig);
            }

            // Unlock and shutdown buttons
            create_child(hwnd, hinstance, w!("BUTTON"), &i18n::wide("blocking.unlock"), button, ID_UNLOCK_BUTTON);
            create_child(hwnd, hinstance, w!("BUTTON"), &i18n::wide("blocking.shutdown"), button, ID_SHUTDOWN_BUTTON);

            // "Request more time" section - no passcode needed, only shown
            // when a bot is actually configured to receive the notification.
            // Layout: separator (WM_PAINT) -> reason label (WM_PAINT) ->
            // note edit -> request button. Must match WM_PAINT.
            if get_telegram_config().enabled || get_discord_config().enabled {
                if let Some(e) = create_child(
                    hwnd,
                    hinstance,
                    w!("EDIT"),
                    &[0],
                    WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                    ID_REQUEST_NOTE_EDIT,
                ) {
                    REQUEST_NOTE_EDIT_HWND.store(e.0, Ordering::SeqCst);
                    SendMessageW(e, EM_SETLIMITTEXT, WPARAM(100), LPARAM(0));
                }
                if let Some(h) = create_child(
                    hwnd,
                    hinstance,
                    w!("BUTTON"),
                    &i18n::wide("blocking.request_time_button"),
                    button,
                    ID_REQUEST_TIME_BUTTON,
                ) {
                    REQUEST_TIME_HWND.store(h.0, Ordering::SeqCst);
                }
            }

            apply_fonts(hwnd);
            layout_children(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let hdc_screen = BeginPaint(hwnd, &mut ps);

            let mut rect: RECT = zeroed();
            GetClientRect(hwnd, &mut rect).ok();

            // Double buffering to prevent flicker
            let hdc_mem = CreateCompatibleDC(hdc_screen);
            let hbm = CreateCompatibleBitmap(hdc_screen, rect.right, rect.bottom);
            let hbm_old = SelectObject(hdc_mem, hbm);
            let hdc = hdc_mem;

            // Draw to memory DC
            let bg_brush = CreateSolidBrush(COLORREF(COLOR_OVERLAY_BG));
            FillRect(hdc, &rect, bg_brush);
            let _ = DeleteObject(bg_brush);

            let screen_width = rect.right;
            let screen_height = rect.bottom;

            // Panel dimensions
            let panel_width = scale(480);
            let panel_height = scale(660);
            let panel_x = (screen_width - panel_width) / 2;
            let panel_y = (screen_height - panel_height) / 2;

            // Panel with rounded corners and accent border
            let panel_brush = CreateSolidBrush(COLORREF(COLOR_PANEL_BG));
            let old_brush = SelectObject(hdc, panel_brush);
            let pen = CreatePen(PS_SOLID, scale(2), COLORREF(COLOR_ACCENT));
            let old_pen = SelectObject(hdc, pen);

            let _ = RoundRect(hdc, panel_x, panel_y, panel_x + panel_width, panel_y + panel_height, scale(24), scale(24));

            SelectObject(hdc, old_brush);
            SelectObject(hdc, old_pen);
            let _ = DeleteObject(panel_brush);
            let _ = DeleteObject(pen);

            SetBkMode(hdc, TRANSPARENT);

            // Large icon at top
            let icon_font = CreateFontW(
                scale(64), 0, 0, 0,
                FW_NORMAL.0 as i32,
                0, 0, 0, 0, 0, 0, 5, 0,
                w!("Segoe UI Emoji"),
            );
            let old_font = SelectObject(hdc, icon_font);
            SetTextColor(hdc, COLORREF(0x006060FF)); // Orange-red
            let mut icon_rect = RECT {
                left: panel_x,
                top: panel_y + scale(20),
                right: panel_x + panel_width,
                bottom: panel_y + scale(90),
            };
            DrawTextW(hdc, &mut "⏰".encode_utf16().collect::<Vec<_>>(), &mut icon_rect, DT_CENTER | DT_SINGLELINE);

            // Title
            let title_font = CreateFontW(
                scale(36), 0, 0, 0,
                FW_BOLD.0 as i32,
                0, 0, 0, 0, 0, 0, 5, 0,
                w!("Segoe UI"),
            );
            SelectObject(hdc, title_font);
            SetTextColor(hdc, COLORREF(COLOR_TEXT_WHITE));

            let mut title_rect = RECT {
                left: panel_x,
                top: panel_y + scale(90),
                right: panel_x + panel_width,
                bottom: panel_y + scale(130),
            };
            DrawTextW(
                hdc,
                &mut i18n::t("blocking.times_up").encode_utf16().collect::<Vec<_>>(),
                &mut title_rect,
                DT_CENTER | DT_SINGLELINE,
            );

            // Shutdown countdown
            let shutdown_countdown = SHUTDOWN_COUNTDOWN_SECONDS.load(Ordering::SeqCst);
            let time_font = CreateFontW(
                scale(28), 0, 0, 0,
                FW_BOLD.0 as i32,
                0, 0, 0, 0, 0, 0, 5, 0,
                w!("Segoe UI"),
            );
            SelectObject(hdc, time_font);

            let (now_key, in_key) = if COUNTDOWN_SIGNS_OUT.load(Ordering::SeqCst) {
                ("blocking.signout_now", "blocking.signout_in")
            } else {
                ("blocking.shutdown_now", "blocking.shutdown_in")
            };
            let time_str = if shutdown_countdown >= 0 {
                if shutdown_countdown <= 60 {
                    SetTextColor(hdc, COLORREF(0x004040FF)); // Red
                    format!("{} {}s", i18n::t(now_key), shutdown_countdown)
                } else {
                    SetTextColor(hdc, COLORREF(COLOR_ACCENT));
                    format!("{} {}", i18n::t(in_key), format_time(shutdown_countdown))
                }
            } else {
                SetTextColor(hdc, COLORREF(COLOR_ACCENT));
                i18n::t("blocking.time_exceeded").to_string()
            };
            let mut time_rect = RECT {
                left: panel_x,
                top: panel_y + scale(135),
                right: panel_x + panel_width,
                bottom: panel_y + scale(170),
            };
            DrawTextW(hdc, &mut time_str.encode_utf16().collect::<Vec<_>>(), &mut time_rect, DT_CENTER | DT_SINGLELINE);

            // Message
            let msg_font = CreateFontW(
                scale(16), 0, 0, 0,
                FW_NORMAL.0 as i32,
                0, 0, 0, 0, 0, 0, 5, 0,
                w!("Segoe UI"),
            );
            SelectObject(hdc, msg_font);
            SetTextColor(hdc, COLORREF(COLOR_TEXT_LIGHT));

            let blocking_text_guard = BLOCKING_TEXT.lock().unwrap();
            let message = blocking_text_guard.as_ref().map(|s| s.as_str()).unwrap_or(i18n::t("blocking.limit_reached"));
            let mut msg_rect = RECT {
                left: panel_x + scale(20),
                top: panel_y + scale(175),
                right: panel_x + panel_width - scale(20),
                bottom: panel_y + scale(200),
            };
            DrawTextW(hdc, &mut message.encode_utf16().collect::<Vec<_>>(), &mut msg_rect, DT_CENTER | DT_SINGLELINE);
            drop(blocking_text_guard);

            // "Extend time:" label
            let label_font = CreateFontW(
                scale(14), 0, 0, 0,
                FW_NORMAL.0 as i32,
                0, 0, 0, 0, 0, 0, 5, 0,
                w!("Segoe UI"),
            );
            SelectObject(hdc, label_font);
            SetTextColor(hdc, COLORREF(0x00AAAAAA));

            let mut extend_label_rect = RECT {
                left: panel_x,
                top: panel_y + scale(200),
                right: panel_x + panel_width,
                bottom: panel_y + scale(218),
            };
            DrawTextW(
                hdc,
                &mut i18n::t("blocking.extend_label").encode_utf16().collect::<Vec<_>>(),
                &mut extend_label_rect,
                DT_CENTER | DT_SINGLELINE,
            );

            // Separator line before passcode section
            draw_with(hdc, CreatePen(PS_SOLID, 1, COLORREF(0x00444444)).into(), || {
                let _ = windows::Win32::Graphics::Gdi::MoveToEx(hdc, panel_x + scale(40), panel_y + scale(265), None);
                let _ = windows::Win32::Graphics::Gdi::LineTo(hdc, panel_x + panel_width - scale(40), panel_y + scale(265));
            });

            // "Enter passcode:" label
            let mut passcode_label_rect = RECT {
                left: panel_x,
                top: panel_y + scale(275),
                right: panel_x + panel_width,
                bottom: panel_y + scale(295),
            };
            DrawTextW(
                hdc,
                &mut i18n::t("blocking.passcode_label").encode_utf16().collect::<Vec<_>>(),
                &mut passcode_label_rect,
                DT_CENTER | DT_SINGLELINE,
            );

            // Separator + "request more time" section (no passcode needed)
            draw_with(hdc, CreatePen(PS_SOLID, 1, COLORREF(0x00444444)).into(), || {
                let _ = windows::Win32::Graphics::Gdi::MoveToEx(hdc, panel_x + scale(40), panel_y + scale(476), None);
                let _ = windows::Win32::Graphics::Gdi::LineTo(hdc, panel_x + panel_width - scale(40), panel_y + scale(476));
            });

            SelectObject(hdc, label_font);
            SetTextColor(hdc, COLORREF(0x00AAAAAA));

            if get_telegram_config().enabled || get_discord_config().enabled {
                let mut note_label_rect = RECT {
                    left: panel_x,
                    top: panel_y + scale(486),
                    right: panel_x + panel_width,
                    bottom: panel_y + scale(504),
                };
                DrawTextW(
                    hdc,
                    &mut i18n::t("blocking.request_note_label").encode_utf16().collect::<Vec<_>>(),
                    &mut note_label_rect,
                    DT_CENTER | DT_SINGLELINE,
                );
            } else {
                let mut hint_rect = RECT {
                    left: panel_x + scale(50),
                    top: panel_y + scale(486),
                    right: panel_x + panel_width - scale(50),
                    bottom: panel_y + scale(586),
                };
                DrawTextW(
                    hdc,
                    &mut i18n::t("blocking.request_unavailable").encode_utf16().collect::<Vec<_>>(),
                    &mut hint_rect,
                    DT_CENTER | DT_WORDBREAK,
                );
            }

            // Passcode notice at the bottom (wrong code, locked out with the
            // time left, or the passcode couldn't be read)
            let notice = PasscodeNotice::current();
            let lockout_left = if notice == PasscodeNotice::LockedOut {
                passcode_guard::lockout_remaining()
            } else {
                0
            };
            if let Some(text) = notice.text(lockout_left) {
                SetTextColor(hdc, COLORREF(COLOR_ERROR));
                let error_font = CreateFontW(
                    scale(15), 0, 0, 0,
                    FW_BOLD.0 as i32,
                    0, 0, 0, 0, 0, 0, 5, 0,
                    w!("Segoe UI"),
                );
                draw_with(hdc, error_font.into(), || {
                    let mut error_rect = passcode_notice_rect(panel_x, panel_y, panel_width, panel_height);
                    DrawTextW(
                        hdc,
                        &mut text.encode_utf16().collect::<Vec<_>>(),
                        &mut error_rect,
                        DT_CENTER | DT_SINGLELINE,
                    );
                });
            }

            // Cleanup fonts
            SelectObject(hdc, old_font);
            let _ = DeleteObject(icon_font);
            let _ = DeleteObject(title_font);
            let _ = DeleteObject(time_font);
            let _ = DeleteObject(msg_font);
            let _ = DeleteObject(label_font);

            // Blit from memory DC to screen (no flicker)
            let _ = BitBlt(hdc_screen, 0, 0, rect.right, rect.bottom, hdc_mem, 0, 0, SRCCOPY);

            // Cleanup double buffer
            SelectObject(hdc_mem, hbm_old);
            let _ = DeleteObject(hbm);
            let _ = DeleteDC(hdc_mem);

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            let notification = ((wparam.0 >> 16) & 0xFFFF) as u32;

            if notification == BN_CLICKED {
                match id {
                    ID_UNLOCK_BUTTON => {
                        if check_blocking_passcode() {
                            // Only add time if there wasn't any left - if this was a
                            // manual lock with time still remaining, just dismiss it
                            // (matches what the !unlock/unlock bot command does).
                            if REMAINING_SECONDS.load(Ordering::SeqCst) <= 0 {
                                extend_time(15);
                                crate::time_request::notify_passcode_extend(
                                    "passcode_extend.source.lock_screen",
                                    15,
                                    REMAINING_SECONDS.load(Ordering::SeqCst),
                                );
                            }
                            hide_blocking_overlay();
                        } else {
                            reject_passcode(hwnd);
                        }
                    }
                    ID_EXTEND_15 | ID_EXTEND_30 | ID_EXTEND_60 => {
                        // Require passcode for extension
                        if check_blocking_passcode() {
                            let minutes = match id {
                                ID_EXTEND_15 => 15,
                                ID_EXTEND_30 => 30,
                                ID_EXTEND_60 => 60,
                                _ => 0,
                            };
                            extend_time(minutes);
                            crate::time_request::notify_passcode_extend(
                                "passcode_extend.source.lock_screen",
                                minutes,
                                REMAINING_SECONDS.load(Ordering::SeqCst),
                            );

                            // Clear the passcode field
                            let edit_ptr = BLOCKING_EDIT_HWND.load(Ordering::SeqCst);
                            if !edit_ptr.is_null() {
                                SetWindowTextW(HWND(edit_ptr), w!("")).ok();
                            }

                            // Hide overlay and let the user continue
                            hide_blocking_overlay();
                        } else {
                            reject_passcode(hwnd);
                        }
                    }
                    ID_SHUTDOWN_BUTTON => {
                        // Show confirmation dialog
                        let confirm_msg = i18n::wide("blocking.confirm_shutdown");
                        let confirm_title = i18n::wide("blocking.confirm_title");
                        let result = MessageBoxW(
                            hwnd,
                            PCWSTR(confirm_msg.as_ptr()),
                            PCWSTR(confirm_title.as_ptr()),
                            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
                        );
                        if result == IDYES {
                            // Initiate system shutdown with proper privilege
                            initiate_shutdown();
                        }
                    }
                    ID_REQUEST_TIME_BUTTON
                        if REQUEST_COOLDOWN_REMAINING.load(Ordering::SeqCst) <= 0 => {
                            let note_ptr = REQUEST_NOTE_EDIT_HWND.load(Ordering::SeqCst);
                            let mut note = String::new();
                            if !note_ptr.is_null() {
                                let mut buffer = [0u16; 128];
                                let len = GetWindowTextW(HWND(note_ptr), &mut buffer);
                                note = String::from_utf16_lossy(&buffer[..len as usize]);
                            }
                            let note = note.trim();
                            let remaining = REMAINING_SECONDS.load(Ordering::SeqCst).max(0);

                            crate::time_request::request_more_time(
                                if note.is_empty() { None } else { Some(note.to_string()) },
                                remaining,
                            );

                            REQUEST_COOLDOWN_REMAINING.store(REQUEST_COOLDOWN_SECONDS, Ordering::SeqCst);
                            let btn_ptr = REQUEST_TIME_HWND.load(Ordering::SeqCst);
                            if !btn_ptr.is_null() {
                                let btn = HWND(btn_ptr);
                                let _ = EnableWindow(btn, false);
                                let text = i18n::wide("blocking.request_sent");
                                SetWindowTextW(btn, PCWSTR(text.as_ptr())).ok();
                            }
                            if !note_ptr.is_null() {
                                let _ = EnableWindow(HWND(note_ptr), false);
                            }
                            SetTimer(hwnd, TIMER_REQUEST_COOLDOWN, 1000, None);
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                TIMER_REASSERT_TOPMOST => {
                    // Belt and braces for WM_DISPLAYCHANGE: a monitor plugged
                    // in or rearranged while locked. Cheap when nothing has
                    // changed (a few metrics and one monitor enumeration).
                    sync_monitor_layout(hwnd);
                    reassert_blocking_window(hwnd);
                    // The secondary blankers need re-asserting too, or an app
                    // dragged onto another monitor would be usable during
                    // lockout.
                    show_secondary_overlays();
                }
                TIMER_COUNTDOWN => {
                    // Count down only while someone can see it (see
                    // countdown_should_run) - a blocked session that was
                    // switched away from used to shut the machine down under
                    // whoever was using it by then.
                    let running = countdown_should_run(
                        crate::session::is_active_console_session(),
                        crate::mini_overlay::IS_LOCK_PAUSED.load(Ordering::SeqCst),
                    );
                    let shutdown_remaining = SHUTDOWN_COUNTDOWN_SECONDS.load(Ordering::SeqCst);
                    if running && shutdown_remaining >= 0 {
                        // Who else is signed in: refreshed every 5 seconds for
                        // the label, and always right before acting on it.
                        if shutdown_remaining == 0 || shutdown_remaining % 5 == 0 {
                            let action = countdown_end_action(crate::session::other_users_logged_on());
                            COUNTDOWN_SIGNS_OUT.store(action == CountdownEnd::SignOut, Ordering::SeqCst);
                        }
                        if shutdown_remaining > 0 {
                            SHUTDOWN_COUNTDOWN_SECONDS.store(shutdown_remaining - 1, Ordering::SeqCst);
                        } else {
                            let signs_out = COUNTDOWN_SIGNS_OUT.load(Ordering::SeqCst);
                            match countdown_end_action(signs_out) {
                                // Trigger shutdown with proper privilege
                                CountdownEnd::Shutdown => { initiate_shutdown(); }
                                CountdownEnd::SignOut => { initiate_sign_out(); }
                            }
                        }
                    }

                    // Only invalidate the countdown region, not the entire window
                    // This prevents child controls (buttons, edit) from flickering
                    let mut client_rect: RECT = zeroed();
                    GetClientRect(hwnd, &mut client_rect).ok();
                    let screen_width = client_rect.right;
                    let screen_height = client_rect.bottom;
                    let panel_width = scale(480);
                    let panel_height = scale(660);
                    let panel_x = (screen_width - panel_width) / 2;
                    let panel_y = (screen_height - panel_height) / 2;

                    // Countdown region (from panel_y + scale(90) to scale(200) covers title + time + message)
                    let countdown_rect = RECT {
                        left: panel_x,
                        top: panel_y + scale(90),
                        right: panel_x + panel_width,
                        bottom: panel_y + scale(200),
                    };
                    let _ = InvalidateRect(hwnd, Some(&countdown_rect), false);

                    // A passcode lockout counts down under the field too;
                    // once it's over the notice goes away.
                    if PasscodeNotice::current() == PasscodeNotice::LockedOut {
                        if passcode_guard::lockout_remaining() == 0 {
                            PasscodeNotice::None.set();
                        }
                        let notice_rect = passcode_notice_rect(panel_x, panel_y, panel_width, panel_height);
                        let _ = InvalidateRect(hwnd, Some(&notice_rect), false);
                    }
                }
                TIMER_REQUEST_COOLDOWN => {
                    let remaining = REQUEST_COOLDOWN_REMAINING.fetch_sub(1, Ordering::SeqCst) - 1;
                    let btn_ptr = REQUEST_TIME_HWND.load(Ordering::SeqCst);
                    if remaining <= 0 {
                        REQUEST_COOLDOWN_REMAINING.store(0, Ordering::SeqCst);
                        let _ = KillTimer(hwnd, TIMER_REQUEST_COOLDOWN);
                        if !btn_ptr.is_null() {
                            let btn = HWND(btn_ptr);
                            let _ = EnableWindow(btn, true);
                            let text = i18n::wide("blocking.request_time_button");
                            SetWindowTextW(btn, PCWSTR(text.as_ptr())).ok();
                        }
                        let note_ptr = REQUEST_NOTE_EDIT_HWND.load(Ordering::SeqCst);
                        if !note_ptr.is_null() {
                            let _ = EnableWindow(HWND(note_ptr), true);
                        }
                    } else if !btn_ptr.is_null() {
                        let text = format!(
                            "{} ({}:{:02})",
                            i18n::t("blocking.request_sent"),
                            remaining / 60, remaining % 60
                        );
                        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                        SetWindowTextW(HWND(btn_ptr), PCWSTR(wide.as_ptr())).ok();
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // Return non-zero to indicate we handle background erasing (prevents flickering)
            LRESULT(1)
        }
        WM_CLOSE => {
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 == VK_RETURN.0 as usize {
                if check_blocking_passcode() {
                    // Only add time if there wasn't any left - if this was a
                    // manual lock with time still remaining, just dismiss it
                    // (matches what the !unlock/unlock bot command does).
                    if REMAINING_SECONDS.load(Ordering::SeqCst) <= 0 {
                        extend_time(15);
                        crate::time_request::notify_passcode_extend(
                            "passcode_extend.source.lock_screen",
                            15,
                            REMAINING_SECONDS.load(Ordering::SeqCst),
                        );
                    }
                    hide_blocking_overlay();
                } else {
                    reject_passcode(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            sync_monitor_layout(hwnd);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DPICHANGED => {
            // The primary monitor's DPI changed (a different primary, or the
            // scale slider). scale() reads a process-wide cache, so refresh
            // it first, then rebuild fonts and re-lay out; WM_PAINT scales on
            // the fly. The suggested rect in lparam is ignored: the overlay
            // always covers the whole primary screen, which
            // sync_monitor_layout measures directly.
            crate::dpi::set_dpi((wparam.0 & 0xFFFF) as u32);
            apply_fonts(hwnd);
            sync_monitor_layout(hwnd);
            layout_children(hwnd);
            let _ = InvalidateRect(hwnd, None, true);
            LRESULT(0)
        }
        WM_APP_SHOW_OVERLAY => {
            apply_show_overlay(hwnd);
            LRESULT(0)
        }
        WM_APP_HIDE_OVERLAY => {
            apply_hide_overlay(hwnd);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            let edit_ptr = BLOCKING_EDIT_HWND.load(Ordering::SeqCst);
            if !edit_ptr.is_null() {
                let _ = SetFocus(HWND(edit_ptr));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Select `object` (a pen, font or brush made for this one drawing step)
/// into `hdc`, run `draw`, put back whatever was selected before, then delete
/// `object`. Returns whether the delete worked. The documentation says not
/// to delete an object while it is still selected into a DC (DeleteObject
/// then fails); the lock screen's WM_PAINT used to do that with its two
/// separator pens and the passcode-notice font on every repaint, once a
/// second. (On Windows 11 a test of that pattern showed no handles left
/// behind - GDI defers such a delete until the object is deselected - but
/// that isn't something to rely on.)
unsafe fn draw_with(hdc: HDC, object: HGDIOBJ, draw: impl FnOnce()) -> bool {
    let previous = SelectObject(hdc, object);
    draw();
    SelectObject(hdc, previous);
    DeleteObject(object).as_bool()
}

/// Create one child control of the blocking overlay, unplaced (see
/// `layout_children`).
unsafe fn create_child(
    parent: HWND,
    hinstance: windows::Win32::Foundation::HMODULE,
    class: PCWSTR,
    text: &[u16],
    style: WINDOW_STYLE,
    id: i32,
) -> Option<HWND> {
    CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(text.as_ptr()),
        style,
        0, 0, 0, 0,
        parent,
        HMENU(id as _),
        hinstance,
        None,
    )
    .ok()
}

/// Create the child controls' fonts at the current DPI and hand them out,
/// freeing the previous set. Called from WM_CREATE and WM_DPICHANGED.
unsafe fn apply_fonts(hwnd: HWND) {
    let make = |height: i32, weight: i32| {
        CreateFontW(scale(height), 0, 0, 0, weight, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"))
    };
    let btn_font = make(16, FW_BOLD.0 as i32);
    let edit_font = make(28, FW_BOLD.0 as i32);
    let note_font = make(14, FW_NORMAL.0 as i32);

    let set = |id: i32, font: HFONT| {
        if let Ok(child) = GetDlgItem(hwnd, id) {
            SendMessageW(child, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        }
    };
    for id in [
        ID_EXTEND_15,
        ID_EXTEND_30,
        ID_EXTEND_60,
        ID_UNLOCK_BUTTON,
        ID_SHUTDOWN_BUTTON,
        ID_REQUEST_TIME_BUTTON,
    ] {
        set(id, btn_font);
    }
    set(ID_PASSCODE_EDIT, edit_font);
    set(ID_REQUEST_NOTE_EDIT, note_font);

    let mut fonts = CHILD_FONTS.lock().unwrap();
    for old in fonts.iter().filter(|&&f| f != 0) {
        let _ = DeleteObject(HFONT(*old as *mut std::ffi::c_void));
    }
    *fonts = [btn_font.0 as isize, edit_font.0 as isize, note_font.0 as isize];
}

/// Position and size every child control for the overlay's current client
/// size and DPI. Mirrors the panel geometry WM_PAINT draws - which recenters
/// on the client rect by itself - so the two must stay in step. Called from
/// WM_CREATE, after a display change resizes the window, and after a DPI
/// change.
unsafe fn layout_children(hwnd: HWND) {
    let mut client: RECT = zeroed();
    if GetClientRect(hwnd, &mut client).is_err() {
        return;
    }
    let width = client.right;
    let height = client.bottom;

    // Panel dimensions - must match WM_PAINT
    let panel_height = scale(660);
    let panel_y = (height - panel_height) / 2;

    let place = |id: i32, x: i32, y: i32, w: i32, h: i32| {
        if let Ok(child) = GetDlgItem(hwnd, id) {
            let _ = SetWindowPos(child, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        }
    };

    // Extend time buttons - below the "Extend time:" label (at scale(200))
    let extend_btn_width = scale(90);
    let extend_btn_height = scale(36);
    let extend_y = panel_y + scale(220);
    let extend_spacing = scale(15);
    let total_extend_width = extend_btn_width * 3 + extend_spacing * 2;
    let extend_start_x = (width - total_extend_width) / 2;
    for (i, id) in [ID_EXTEND_15, ID_EXTEND_30, ID_EXTEND_60].into_iter().enumerate() {
        place(
            id,
            extend_start_x + (extend_btn_width + extend_spacing) * i as i32,
            extend_y,
            extend_btn_width,
            extend_btn_height,
        );
    }

    // Passcode edit
    let edit_width = scale(180);
    let edit_height = scale(44);
    let edit_y = panel_y + scale(310);
    place(ID_PASSCODE_EDIT, (width - edit_width) / 2, edit_y, edit_width, edit_height);

    // Unlock and shutdown buttons
    let btn_width = scale(180);
    let btn_height = scale(40);
    let btn_x = (width - btn_width) / 2;
    let btn_y = edit_y + edit_height + scale(12);
    place(ID_UNLOCK_BUTTON, btn_x, btn_y, btn_width, btn_height);
    place(ID_SHUTDOWN_BUTTON, btn_x, btn_y + btn_height + scale(10), btn_width, btn_height);

    // "Request more time" note and button (absent when no bot is configured)
    let request_width = scale(300);
    let request_x = (width - request_width) / 2;
    place(ID_REQUEST_NOTE_EDIT, request_x, panel_y + scale(508), request_width, scale(30));
    place(ID_REQUEST_TIME_BUTTON, request_x, panel_y + scale(548), request_width, scale(38));
}

pub unsafe fn register_blocking_class(hinstance: windows::Win32::Foundation::HMODULE) {
    let blocking_class_name = w!("ScreenTimeBlockingClass");
    let blocking_wnd_class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(blocking_overlay_proc),
        hInstance: hinstance.into(),
        lpszClassName: blocking_class_name,
        hbrBackground: CreateSolidBrush(COLORREF(COLOR_OVERLAY_BG)),
        hCursor: LoadCursorW(None, IDC_ARROW).ok().unwrap_or_default(),
        ..zeroed()
    };

    if RegisterClassW(&blocking_wnd_class) == 0 {
        panic!("Failed to register blocking overlay window class");
    }

    // Register secondary overlay class (simpler, no controls)
    let secondary_class_name = w!("ScreenTimeSecondaryBlockingClass");
    let secondary_wnd_class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(secondary_overlay_proc),
        hInstance: hinstance.into(),
        lpszClassName: secondary_class_name,
        hbrBackground: CreateSolidBrush(COLORREF(COLOR_OVERLAY_BG)),
        hCursor: LoadCursorW(None, IDC_ARROW).ok().unwrap_or_default(),
        ..zeroed()
    };

    if RegisterClassW(&secondary_wnd_class) == 0 {
        panic!("Failed to register secondary blocking overlay window class");
    }
}

/// Window procedure for secondary monitor overlays (simple blank screen)
pub unsafe extern "system" fn secondary_overlay_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);

            let mut rect: RECT = zeroed();
            GetClientRect(hwnd, &mut rect).ok();

            // Fill with dark background
            let bg_brush = CreateSolidBrush(COLORREF(COLOR_OVERLAY_BG));
            FillRect(hdc, &rect, bg_brush);
            let _ = DeleteObject(bg_brush);

            // Draw "Screen Locked" text in center (DPI scaled, ClearType quality = 5)
            let font = CreateFontW(
                scale(48), 0, 0, 0,
                FW_BOLD.0 as i32,
                0, 0, 0, 0, 0, 0, 5, 0,
                w!("Segoe UI"),
            );
            let old_font = SelectObject(hdc, font);
            SetTextColor(hdc, COLORREF(COLOR_TEXT_LIGHT));
            SetBkMode(hdc, TRANSPARENT);

            DrawTextW(
                hdc,
                &mut i18n::t("blocking.screen_locked").encode_utf16().collect::<Vec<_>>(),
                &mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );

            SelectObject(hdc, old_font);
            let _ = DeleteObject(font);

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_CLOSE => LRESULT(0), // Prevent closing
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Callback for EnumDisplayMonitors to collect monitor information
unsafe extern "system" fn monitor_enum_callback(
    _hmonitor: HMONITOR,
    _hdc: HDC,
    lprect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = &mut *(lparam.0 as *mut Vec<MonitorInfo>);

    if let Some(rect) = lprect.as_ref() {
        // Check if this is the primary monitor (origin at 0,0)
        let is_primary = rect.left == 0 && rect.top == 0;

        monitors.push(MonitorInfo {
            rect: *rect,
            is_primary,
        });
    }

    BOOL::from(true) // Continue enumeration
}

/// Enumerate all monitors and return their information
unsafe fn enumerate_monitors() -> Vec<MonitorInfo> {
    let mut monitors: Vec<MonitorInfo> = Vec::new();

    let _ = EnumDisplayMonitors(
        None,
        None,
        Some(monitor_enum_callback),
        LPARAM(&mut monitors as *mut Vec<MonitorInfo> as isize),
    );

    monitors
}

/// Create blanker windows for all non-primary monitors (hidden until the
/// overlay is shown). Replaces any existing set.
pub unsafe fn create_secondary_overlays(hinstance: windows::Win32::Foundation::HMODULE) {
    build_secondary_overlays(hinstance, enumerate_monitors());
}

unsafe fn build_secondary_overlays(
    hinstance: windows::Win32::Foundation::HMODULE,
    monitors: Vec<MonitorInfo>,
) {
    destroy_secondary_overlays();

    let class_name = w!("ScreenTimeSecondaryBlockingClass");
    let locked_title = i18n::wide("blocking.screen_locked");
    let mut built = SecondaryOverlays { hwnds: Vec::new(), layout: Vec::new() };

    for monitor in monitors {
        // Skip the primary monitor (main blocking overlay handles it)
        if monitor.is_primary {
            continue;
        }
        built.layout.push(monitor.rect);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class_name,
            PCWSTR(locked_title.as_ptr()),
            WS_POPUP,
            monitor.rect.left,
            monitor.rect.top,
            monitor.rect.right - monitor.rect.left,
            monitor.rect.bottom - monitor.rect.top,
            None,
            None,
            hinstance,
            None,
        );
        match hwnd {
            Ok(h) => built.hwnds.push(h.0 as isize),
            Err(e) => eprintln!(
                "[Blocking] Failed to create secondary overlay for {:?}: {e}",
                monitor.rect
            ),
        }
    }

    *SECONDARY_OVERLAYS.lock().unwrap() = built;
}

/// Destroy the secondary blankers and forget the layout they were built for.
unsafe fn destroy_secondary_overlays() {
    let old = std::mem::replace(
        &mut *SECONDARY_OVERLAYS.lock().unwrap(),
        SecondaryOverlays { hwnds: Vec::new(), layout: Vec::new() },
    );
    for hwnd in old.hwnds {
        let _ = DestroyWindow(HWND(hwnd as *mut std::ffi::c_void));
    }
}

/// The current secondary blanker handles, copied out so no lock is held
/// across window calls.
fn secondary_overlay_hwnds() -> Vec<HWND> {
    SECONDARY_OVERLAYS
        .lock()
        .unwrap()
        .hwnds
        .iter()
        .map(|&h| HWND(h as *mut std::ffi::c_void))
        .collect()
}

/// Bring the blocking overlays in line with the current monitor layout.
/// Monitors are otherwise only measured once at startup, so a monitor plugged
/// in (or a resolution/arrangement change) while the app is running would
/// leave that screen uncovered during lockout, or covered by a wrongly-sized
/// blanker. UI thread only - it creates and destroys windows, which then
/// belong to the calling thread - so it's driven from WM_DISPLAYCHANGE,
/// WM_DPICHANGED and TIMER_REASSERT_TOPMOST.
unsafe fn sync_monitor_layout(blocking_hwnd: HWND) {
    // Primary monitor: resize the main overlay and re-lay out its controls
    // (WM_PAINT already draws from the client rect, so the panel recenters
    // on its own).
    let new_width = GetSystemMetrics(SM_CXSCREEN);
    let new_height = GetSystemMetrics(SM_CYSCREEN);
    let mut client: RECT = zeroed();
    if GetClientRect(blocking_hwnd, &mut client).is_ok()
        && (client.right != new_width || client.bottom != new_height)
    {
        let _ = SetWindowPos(
            blocking_hwnd,
            None,
            0, 0,
            new_width,
            new_height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        layout_children(blocking_hwnd);
        let _ = InvalidateRect(blocking_hwnd, None, false);
    }

    // Secondary monitors: rebuild the blankers if the set of non-primary
    // monitor rects no longer matches what they were built for.
    let monitors = enumerate_monitors();
    let current: Vec<RECT> = monitors
        .iter()
        .filter(|m| !m.is_primary)
        .map(|m| m.rect)
        .collect();
    if SECONDARY_OVERLAYS.lock().unwrap().layout == current {
        return;
    }

    if let Ok(hinstance) = GetModuleHandleW(None) {
        build_secondary_overlays(hinstance, monitors);
    }
    show_secondary_overlays();
}

/// Push a window back to the top of the topmost band without activating it,
/// optionally showing it as well. WS_EX_TOPMOST alone isn't enough: recent
/// Windows 11 builds can leave a window with the topmost style still set but
/// actually z-ordered *below* ordinary windows, so anything dragged over it
/// hides it. The overlays therefore re-assert on a timer.
pub(crate) unsafe fn push_to_topmost(hwnd: HWND, show: bool) {
    let mut flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
    if show {
        flags |= SWP_SHOWWINDOW;
    }
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags);
}

/// Put the main lock overlay back the way it should be, on every
/// TIMER_REASSERT_TOPMOST tick: shown (another process in the session can
/// hide or minimize it with ShowWindow, and it used to stay hidden), at the
/// primary monitor's origin (or moved off screen with SetWindowPos), and at
/// the top of the topmost band. Skipped once a hide has been requested, so a
/// tick between a bot's hide and its posted WM_APP_HIDE_OVERLAY can't
/// re-show it.
unsafe fn reassert_blocking_window(hwnd: HWND) {
    if !is_blocking_overlay_visible() {
        return;
    }
    if IsIconic(hwnd).as_bool() {
        let _ = ShowWindow(hwnd, SW_RESTORE);
    }
    let mut window: RECT = zeroed();
    if GetWindowRect(hwnd, &mut window).is_ok() && (window.left != 0 || window.top != 0) {
        let _ = SetWindowPos(
            hwnd,
            None,
            0, 0,
            GetSystemMetrics(SM_CXSCREEN),
            GetSystemMetrics(SM_CYSCREEN),
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        layout_children(hwnd);
    }
    push_to_topmost(hwnd, true);
}

/// Show every secondary blanker and push it back to the top of the topmost
/// band. Runs on show and on every TIMER_REASSERT_TOPMOST tick: besides the
/// z-order repair, blankers rebuilt mid display-mode change can end up
/// hidden (the show is lost while the display is still switching) and
/// nothing else would show them again. Skipped when the overlay isn't up,
/// so a tick can't re-show blankers an unlock has just hidden.
unsafe fn show_secondary_overlays() {
    if !is_blocking_overlay_visible() {
        return;
    }
    for hwnd in secondary_overlay_hwnds() {
        push_to_topmost(hwnd, true);
    }
}

/// Hide all secondary monitor overlays
unsafe fn hide_secondary_overlays() {
    for hwnd in secondary_overlay_hwnds() {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extend_adds_or_starts_fresh() {
        assert_eq!(extended(100, 900), 1000);
        assert_eq!(extended(0, 900), 900);
        assert_eq!(extended(-1, 900), 900, "no limit running: start fresh");
        assert_eq!(extended(i32::MAX - 1, 900), i32::MAX);
    }

    #[test]
    fn out_of_time_overlay_stays_up_when_no_time_arrived() {
        use std::cell::Cell;
        let shown = Cell::new(false);
        let up = show_unless_time_arrived(|| shown.set(true), || 0, || shown.set(false));
        assert!(up);
        assert!(shown.get());
    }

    #[test]
    fn extension_in_the_instant_time_runs_out_takes_the_overlay_down() {
        use std::cell::Cell;
        // The bot extends (and its hide finds nothing shown yet) right as
        // the overlay is marked shown.
        let shown = Cell::new(false);
        let remaining = Cell::new(0);
        let undos = Cell::new(0);
        let up = show_unless_time_arrived(
            || {
                remaining.set(extended(0, 15 * 60));
                shown.set(true);
            },
            || remaining.get(),
            || {
                shown.set(false);
                undos.set(undos.get() + 1);
            },
        );
        assert!(!up);
        assert!(!shown.get(), "no lock overlay with time remaining");
        assert_eq!(undos.get(), 1, "undone once, never retried");
        // "No limit" isn't "no time left" either
        assert!(!show_unless_time_arrived(|| {}, || -1, || {}));
    }

    #[test]
    fn reduce_never_goes_below_zero() {
        assert_eq!(reduced(1000, 900), 100);
        assert_eq!(reduced(100, 900), 0);
        assert_eq!(reduced(-1, 900), 0);
    }

    #[test]
    fn extensions_from_other_threads_are_not_lost() {
        // Stand-in for REMAINING_SECONDS: one thread counting down, others
        // extending, all through the same fetch_update pattern.
        use std::sync::atomic::AtomicI32;
        static REMAINING: AtomicI32 = AtomicI32::new(1_000_000);
        let countdown = std::thread::spawn(|| {
            for _ in 0..10_000 {
                let _ = REMAINING.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| {
                    (c > 0).then(|| (c - 1).max(0))
                });
            }
        });
        let extenders: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..1_000 {
                        let _ = REMAINING.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| {
                            Some(extended(c, 60))
                        });
                    }
                })
            })
            .collect();
        countdown.join().unwrap();
        for e in extenders {
            e.join().unwrap();
        }
        assert_eq!(REMAINING.load(Ordering::SeqCst), 1_000_000 - 10_000 + 4 * 1_000 * 60);
    }

    #[test]
    fn lowering_the_limit_caps_remaining_time() {
        assert_eq!(capped(7200, 3600), Some(3600));
        assert_eq!(capped(3600, 3600), None);
        assert_eq!(capped(1800, 3600), None);
        assert_eq!(capped(-1, 3600), None, "no limit running is left alone");
        assert_eq!(capped(600, 0), Some(0), "a 0-minute day locks");
    }

    #[test]
    fn passcode_notice_follows_the_attempt() {
        assert_eq!(PasscodeNotice::after(Attempt::Correct), PasscodeNotice::None);
        assert_eq!(PasscodeNotice::after(Attempt::Wrong { locked_for_secs: 0 }), PasscodeNotice::Incorrect);
        assert_eq!(PasscodeNotice::after(Attempt::Wrong { locked_for_secs: 30 }), PasscodeNotice::LockedOut);
        assert_eq!(PasscodeNotice::after(Attempt::LockedOut { secs_remaining: 5 }), PasscodeNotice::LockedOut);
        assert_eq!(PasscodeNotice::after(Attempt::Unavailable), PasscodeNotice::Unavailable);
    }

    #[test]
    fn lockout_notice_shows_the_time_left_until_it_runs_out() {
        assert_eq!(PasscodeNotice::None.text(30), None);
        let locked = PasscodeNotice::LockedOut.text(65).unwrap();
        assert!(locked.ends_with(" 1:05"), "{locked}");
        assert!(locked.starts_with(i18n::t("passcode.locked")), "{locked}");
        assert_eq!(PasscodeNotice::LockedOut.text(0), None, "lockout over: nothing to show");
        assert!(PasscodeNotice::Incorrect.text(0).is_some());
        assert!(PasscodeNotice::Unavailable.text(0).is_some());
    }

    #[test]
    fn countdown_runs_only_at_the_unlocked_console() {
        assert!(countdown_should_run(true, false));
        assert!(!countdown_should_run(true, true), "Windows lock screen");
        assert!(!countdown_should_run(false, false), "switched away");
        assert!(!countdown_should_run(false, true));
    }

    #[test]
    fn paint_objects_are_freed_after_drawing() {
        use windows::Win32::System::Threading::{GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS};
        unsafe {
            let hdc = CreateCompatibleDC(None);
            assert!(!hdc.is_invalid());
            let gdi_objects = || GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS);

            let before = gdi_objects();
            for _ in 0..500 {
                assert!(draw_with(hdc, CreatePen(PS_SOLID, 1, COLORREF(0)).into(), || {}));
                let font = CreateFontW(15, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0, 0, 0, 0, 5, 0, w!("Segoe UI"));
                assert!(draw_with(hdc, font.into(), || {}));
            }
            assert_eq!(gdi_objects(), before, "a thousand paint objects later");
            let _ = DeleteDC(hdc);
        }
    }

    #[test]
    fn countdown_signs_out_when_others_are_signed_in() {
        assert_eq!(countdown_end_action(true), CountdownEnd::SignOut);
        assert_eq!(countdown_end_action(false), CountdownEnd::Shutdown);
    }
}
