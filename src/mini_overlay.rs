//! Mini overlay module
//! Small, always-visible display showing remaining time

use std::mem::zeroed;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use windows::{
    core::w,
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, FillRect,
            InvalidateRect, SelectObject, SetBkMode, SetTextColor, DrawTextW,
            DT_CENTER, DT_SINGLELINE, DT_VCENTER, FW_BOLD, PAINTSTRUCT, TRANSPARENT,
        },
        System::SystemInformation::{GetTickCount, GetTickCount64},
        UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
        UI::WindowsAndMessaging::*,
    },
};

use crate::blocking::REMAINING_SECONDS;
use crate::constants::*;
use crate::database;
use crate::dpi::scale;

/// Global state for mini overlay window
pub static MINI_OVERLAY_HWND: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
pub static MINI_OVERLAY_VISIBLE: AtomicBool = AtomicBool::new(false);

// Pause state tracking
pub static IS_PAUSED: AtomicBool = AtomicBool::new(false);
pub static PAUSE_START_TIMESTAMP: AtomicI64 = AtomicI64::new(0);
pub static CURRENT_PAUSE_DURATION: AtomicI32 = AtomicI32::new(0);
/// The current pause's maximum (`get_max_pause_duration`) as last worked
/// out by the tick, for the mini overlay's WM_PAINT - which used to work it
/// out itself, looking up today's date in the database on every repaint.
static PAUSE_MAX_SECONDS: AtomicI32 = AtomicI32::new(0);
pub static SESSION_ACTIVE_SECONDS: AtomicI32 = AtomicI32::new(0);

// Idle detection state (independent from manual pause)
pub static IS_IDLE_PAUSED: AtomicBool = AtomicBool::new(false);

/// Whether the Windows session is currently locked (independent from manual
/// pause and idle detection - always active regardless of the idle-detection
/// setting, since a locked screen means the account genuinely can't be in
/// use, not just that nobody's touched the keyboard/mouse recently). Set
/// directly from `tray::window_proc`'s WM_WTSSESSION_CHANGE handler, not
/// polled every tick the way idle detection is - the OS tells us the exact
/// moment this changes, so there's no reason to guess at it - though the
/// message is checked against `session::query_session_locked` first, since
/// any process in the session can post it (see `on_session_lock_message`),
/// and Windows is asked again every few ticks either way
/// (`recheck_session_lock`), in case a message was refused or never came.
pub static IS_LOCK_PAUSED: AtomicBool = AtomicBool::new(false);

/// Timer ID of the always-on accounting tick, owned by the hidden main window
/// (handled in `tray::window_proc`). It drives the countdown, pause accrual,
/// day rollover, idle detection, bot hand-off and the mini overlay's redraw,
/// whether or not the mini overlay or the lock overlay is showing. (This used
/// to be the mini overlay's own timer, which the lock overlay killed when it
/// hid the mini overlay - stopping rollover and bot hand-off while locked.)
pub const TIMER_ACCOUNTING_TICK: usize = 30;
const TICK_INTERVAL_MS: u32 = 1000;

/// A gap between ticks up to this long is credited in full (a UI-thread
/// stall must not be free time). A longer gap means the process wasn't
/// running - sleep, hibernate, or suspended - and is credited as a single
/// one-second tick, exactly as the one-second-per-tick accounting always did.
const MAX_CREDITED_GAP_MS: u64 = 5000;

/// Save remaining/active time and persist the clock anchor this often (ticks).
const PERIODIC_SAVE_TICKS: u32 = 30;
/// While paused, save the pause so far this often (seconds of pause).
const PAUSE_PERSIST_SECS: i32 = 10;
/// Re-ask Windows whether this session is locked this often (ticks), locked
/// or not - see `recheck_session_lock`.
const LOCK_RECHECK_TICKS: u32 = 5;

/// `GetTickCount64` at the previous tick, the not-yet-credited milliseconds,
/// a tick counter, and whether counted time is waiting to be saved. UI thread.
static LAST_TICK_MS: AtomicU64 = AtomicU64::new(0);
static CREDIT_CARRY_MS: AtomicU64 = AtomicU64::new(0);
static TICK_COUNT: AtomicU32 = AtomicU32::new(0);
static ACCOUNTING_DIRTY: AtomicBool = AtomicBool::new(false);

/// Mini overlay base dimensions (at 96 DPI / 100% scaling)
const MINI_WIDTH_BASE: i32 = 140;
const MINI_HEIGHT_BASE: i32 = 36;
const MINI_MARGIN_BASE: i32 = 10;

/// Create the mini overlay window
pub unsafe fn create_mini_overlay(hinstance: windows::Win32::Foundation::HMODULE) {
    let class_name = w!("ScreenTimeMiniOverlayClass");

    // Apply DPI scaling to dimensions
    let mini_width = scale(MINI_WIDTH_BASE);
    let mini_height = scale(MINI_HEIGHT_BASE);

    let (x, y) = mini_overlay_position();

    let ex_style = WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT;

    let hwnd = CreateWindowExW(
        ex_style,
        class_name,
        w!("Screen Time"),
        WS_POPUP,
        x,
        y,
        mini_width,
        mini_height,
        None,
        None,
        hinstance,
        None,
    )
    .expect("Failed to create mini overlay window");

    // Set transparency (slightly see-through)
    SetLayeredWindowAttributes(hwnd, COLORREF(0), 200, LWA_ALPHA)
        .expect("Failed to set layered window attributes");

    MINI_OVERLAY_HWND.store(hwnd.0, Ordering::SeqCst);
}

/// Show the mini overlay (redrawn by the accounting tick while visible)
pub unsafe fn show_mini_overlay() {
    let hwnd = HWND(MINI_OVERLAY_HWND.load(Ordering::SeqCst));
    if hwnd.0.is_null() {
        return;
    }

    MINI_OVERLAY_VISIBLE.store(true, Ordering::SeqCst);

    // Re-place as well as show: the primary monitor may have changed size
    // since the window was created.
    let (x, y) = mini_overlay_position();
    let _ = InvalidateRect(hwnd, None, true);
    let _ = SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        x, y, 0, 0,
        SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
}

/// Hide the mini overlay
pub unsafe fn hide_mini_overlay() {
    let hwnd = HWND(MINI_OVERLAY_HWND.load(Ordering::SeqCst));
    if hwnd.0.is_null() {
        return;
    }

    MINI_OVERLAY_VISIBLE.store(false, Ordering::SeqCst);

    let _ = ShowWindow(hwnd, SW_HIDE);
}

/// Where the mini overlay sits: the top-right corner of the primary monitor
/// at its current size.
unsafe fn mini_overlay_position() -> (i32, i32) {
    let margin = scale(MINI_MARGIN_BASE);
    (GetSystemMetrics(SM_CXSCREEN) - scale(MINI_WIDTH_BASE) - margin, margin)
}

/// Update the mini overlay display
pub unsafe fn update_mini_overlay() {
    let hwnd = HWND(MINI_OVERLAY_HWND.load(Ordering::SeqCst));
    if hwnd.0.is_null() {
        return;
    }

    let _ = InvalidateRect(hwnd, None, true);
}

/// Format seconds into a compact string (e.g., "1:30:45" or "30:45")
fn format_time_compact(seconds: i32) -> String {
    if seconds < 0 {
        return String::from("--:--");
    }

    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;

    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, secs)
    } else {
        format!("{}:{:02}", minutes, secs)
    }
}

/// Get color based on remaining time
fn get_time_color(seconds: i32) -> u32 {
    if seconds < 0 {
        COLOR_TEXT_LIGHT
    } else if seconds <= 60 {
        // Red when less than 1 minute
        0x004444FF
    } else if seconds <= 300 {
        // Orange when less than 5 minutes
        COLOR_ACCENT
    } else {
        // White otherwise
        COLOR_TEXT_WHITE
    }
}

// ============================================================================
// Pause Mode Functions
// ============================================================================

/// Check if timer is currently paused
pub fn is_paused() -> bool {
    IS_PAUSED.load(Ordering::SeqCst)
}

/// Reason why pause is not available
#[derive(Debug, Clone)]
pub enum PauseBlockedReason {
    Disabled,
    BudgetExhausted,
    CooldownActive { seconds_remaining: i32 },
    MinActiveTimeNotMet { seconds_remaining: i32 },
    TimeTooLow,
}

/// Check if pause is currently available and return reason if not
pub fn can_pause() -> Result<(), PauseBlockedReason> {
    // Already paused - can always unpause
    if IS_PAUSED.load(Ordering::SeqCst) {
        return Ok(());
    }

    // Check if pause feature is enabled
    if !database::is_pause_enabled() {
        return Err(PauseBlockedReason::Disabled);
    }

    let config = database::get_pause_config();

    // Check if remaining time is too low (< 1 minute)
    let remaining = REMAINING_SECONDS.load(Ordering::SeqCst);
    if remaining < 60 {
        return Err(PauseBlockedReason::TimeTooLow);
    }

    // Check daily budget
    let pause_used = database::get_pause_used_today();
    let budget_seconds = (config.daily_budget_minutes * 60) as i32;
    if pause_used >= budget_seconds {
        return Err(PauseBlockedReason::BudgetExhausted);
    }

    // Check cooldown
    let remaining_cooldown = cooldown_remaining_secs(
        database::get_current_timestamp(),
        database::get_last_pause_end(),
        (config.cooldown_minutes * 60) as i64,
    );
    if remaining_cooldown > 0 {
        return Err(PauseBlockedReason::CooldownActive {
            seconds_remaining: remaining_cooldown as i32,
        });
    }

    // Check minimum active time
    let session_active = SESSION_ACTIVE_SECONDS.load(Ordering::SeqCst);
    let min_active_seconds = (config.min_active_time_minutes * 60) as i32;

    if session_active < min_active_seconds {
        let remaining_active = min_active_seconds - session_active;
        return Err(PauseBlockedReason::MinActiveTimeNotMet {
            seconds_remaining: remaining_active,
        });
    }

    Ok(())
}

/// How far ahead of now a stored pause-end time may be and still be taken at
/// face value (ordinary clock corrections).
const MAX_FUTURE_PAUSE_END_SECS: i64 = 60;

/// Seconds of pause cooldown left, from real epoch seconds. A last-pause-end
/// time of 0 (never paused) or further in the future than any clock
/// correction explains can't be a real end time from this clock, so it
/// doesn't hold pause back - it must never lock pause out. Values saved by
/// older versions (a 30-days-per-month approximation) are always more than a
/// week behind epoch time, so they read as an expired cooldown. The result is
/// never more than one full cooldown.
fn cooldown_remaining_secs(now: i64, last_pause_end: i64, cooldown_secs: i64) -> i64 {
    if last_pause_end <= 0 || cooldown_secs <= 0 {
        return 0;
    }
    let elapsed = now - last_pause_end;
    if elapsed < -MAX_FUTURE_PAUSE_END_SECS {
        return 0;
    }
    (cooldown_secs - elapsed.max(0)).clamp(0, cooldown_secs)
}

/// Pause budget left today in seconds. 0 while pausing is switched off
/// (Settings saves a daily pause of 0 as `pause_enabled=0`), whatever the
/// stored budget says.
pub fn get_remaining_pause_budget() -> i32 {
    let config = database::get_pause_config();
    remaining_pause_budget(
        database::is_pause_enabled(),
        (config.daily_budget_minutes * 60) as i32,
        database::get_pause_used_today(),
    )
}

fn remaining_pause_budget(enabled: bool, budget_seconds: i32, used_seconds: i32) -> i32 {
    if enabled {
        (budget_seconds - used_seconds).max(0)
    } else {
        0
    }
}

/// Get maximum pause duration for current pause (considering budget and
/// config). 0 while pausing is off, so a pause in progress when it's switched
/// off ends at the next tick (see `accrue_pause`).
pub fn get_max_pause_duration() -> i32 {
    let config = database::get_pause_config();
    let max_single = (config.max_duration_minutes * 60) as i32;
    let remaining_budget = get_remaining_pause_budget();
    max_single.min(remaining_budget)
}

/// Toggle pause state - returns true if now paused, false if resumed
pub fn toggle_pause() -> Result<bool, PauseBlockedReason> {
    if IS_PAUSED.load(Ordering::SeqCst) {
        // Currently paused - resume
        resume_timer();
        Ok(false)
    } else {
        // Not paused - try to pause
        can_pause()?;
        pause_timer();
        Ok(true)
    }
}

/// Pause the timer
fn pause_timer() {
    let timestamp = database::get_current_timestamp();
    PAUSE_START_TIMESTAMP.store(timestamp, Ordering::SeqCst);
    CURRENT_PAUSE_DURATION.store(0, Ordering::SeqCst);
    PAUSE_MAX_SECONDS.store(get_max_pause_duration(), Ordering::SeqCst);
    IS_PAUSED.store(true, Ordering::SeqCst);

    // Update display immediately
    unsafe {
        let hwnd = HWND(MINI_OVERLAY_HWND.load(Ordering::SeqCst));
        if !hwnd.0.is_null() {
            let _ = InvalidateRect(hwnd, None, true);
        }
    }
}

/// Resume the timer (end pause). Only the caller that actually flips
/// IS_PAUSED off records the pause, so a bot's resume racing the tick's
/// auto-resume can't count it twice.
fn resume_timer() {
    if !IS_PAUSED.swap(false, Ordering::SeqCst) {
        return;
    }
    let pause_duration = CURRENT_PAUSE_DURATION.swap(0, Ordering::SeqCst);

    // Count it, and log it, under one date: today's when it ends. (A pause
    // that spans midnight is counted against the new day as a whole, as
    // before; the log used to put it under the raw local date instead.)
    let date = database::effective_today_date();
    let total_used = database::get_pause_used_for(&date) + pause_duration;
    database::save_pause_used_for(&date, total_used);
    database::log_pause_event_for(&date, pause_duration);

    // Save last pause end timestamp
    let timestamp = database::get_current_timestamp();
    database::save_last_pause_end(timestamp);

    // Now counted in today's usage - drop the crash-safety copy (see
    // `accrue_pause`). Done after the save above: a kill in between counts
    // the pause twice rather than not at all.
    database::clear_inflight_pause();

    // Reset pause state
    PAUSE_START_TIMESTAMP.store(0, Ordering::SeqCst);

    // Update display immediately
    unsafe {
        let hwnd = HWND(MINI_OVERLAY_HWND.load(Ordering::SeqCst));
        if !hwnd.0.is_null() {
            let _ = InvalidateRect(hwnd, None, true);
        }
    }
}

/// Force resume (called when max duration reached)
fn force_resume() {
    resume_timer();
}

// ============================================================================
// Idle Detection Functions
// ============================================================================

/// Get the number of seconds since the last user input (mouse/keyboard)
fn get_idle_seconds() -> u32 {
    unsafe {
        let mut lii: LASTINPUTINFO = zeroed();
        lii.cbSize = std::mem::size_of::<LASTINPUTINFO>() as u32;
        if GetLastInputInfo(&mut lii).as_bool() {
            let now = GetTickCount();
            now.wrapping_sub(lii.dwTime) / 1000
        } else {
            0
        }
    }
}

/// Start the day counters at startup: `remaining` and `active` were loaded
/// for `date`, which from now on is the date they're saved under, and the
/// one the rollover compares with (see `database::with_counters_date`). An
/// app started in the last seconds before midnight used to record whatever
/// date its first tick saw as "not a rollover", carrying the old day's
/// leftover into the new day.
pub fn start_day(date: &str, remaining: i32, active: i32) {
    database::with_counters_date(|tracked| {
        REMAINING_SECONDS.store(remaining, Ordering::SeqCst);
        SESSION_ACTIVE_SECONDS.store(active, Ordering::SeqCst);
        *tracked = Some(date.to_string());
    });
}

/// Save the remaining time now, under the date it belongs to. Any thread.
/// Read inside the lock, so a rollover can't swap in the next day's value
/// between the read and the write.
pub fn persist_remaining() {
    database::with_counters_date(|tracked| {
        let date = database::counters_save_date(tracked.as_deref(), database::effective_today_date);
        database::save_remaining_time_for(&date, REMAINING_SECONDS.load(Ordering::SeqCst));
    });
}

/// Save the remaining and the active time now, as `persist_remaining`.
pub fn persist_day_counters() {
    database::with_counters_date(|tracked| {
        let date = database::counters_save_date(tracked.as_deref(), database::effective_today_date);
        database::save_remaining_time_for(&date, REMAINING_SECONDS.load(Ordering::SeqCst));
        database::save_session_active_time_for(&date, SESSION_ACTIVE_SECONDS.load(Ordering::SeqCst));
    });
}

/// Whether "today" has moved on from the date the counters belong to.
/// Without a date yet (startup hasn't loaded the counters) there's nothing
/// to roll over from.
fn is_new_day(tracked: Option<&str>, today: &str) -> bool {
    tracked.is_some_and(|t| t != today)
}

/// Roll the counters over now if the day has changed, from any thread -
/// before a parent's change to the remaining time, so a change made between
/// midnight and the tick's own rollover counts for the new day rather than
/// being replaced by it a moment later.
pub fn roll_over_if_due() {
    unsafe { check_day_rollover(true) }
}

/// The raw local date (`database::local_today_date`) at the previous check -
/// polled every tick without touching the database, so the authoritative
/// (and database-writing) `effective_today_date` only runs when the local
/// date changed, or when `force` asks for it.
static LAST_LOCAL_DATE: Mutex<Option<String>> = Mutex::new(None);

/// Detect the day changing under a still-running process and reset the
/// countdown/session-active counters for the new day, mirroring what
/// `main.rs` already does at a fresh startup. `force` re-checks the
/// authoritative date (and persists the clock anchor) even if the local date
/// looks unchanged - the accounting tick does this every 30 seconds. That's
/// also what rolls the day over while the PC is in a time zone other than the
/// remembered one (see `database::effective_today_date`): the day ends at
/// midnight in the remembered zone, when the local date doesn't change, so it
/// rolls over up to 30 seconds after that midnight.
unsafe fn check_day_rollover(force: bool) {
    let local = database::local_today_date();
    {
        let mut last = LAST_LOCAL_DATE.lock().unwrap();
        let changed = last.as_deref() != Some(local.as_str());
        if changed {
            *last = Some(local);
        } else if !force {
            return;
        }
    }

    // The switch happens under the counters' date lock, so no save can
    // interleave and file one day's numbers under the other.
    let new_day = database::with_counters_date(|tracked| {
        let today = database::effective_today_date();
        if !is_new_day(tracked.as_deref(), &today) {
            if tracked.is_none() {
                *tracked = Some(today);
            }
            return false;
        }
        // The old day's last counted seconds go under the old day.
        if let Some(old) = tracked.as_deref() {
            database::save_remaining_time_for(old, REMAINING_SECONDS.load(Ordering::SeqCst));
            database::save_session_active_time_for(old, SESSION_ACTIVE_SECONDS.load(Ordering::SeqCst));
        }
        let remaining = database::load_remaining_time_for(&today).unwrap_or_else(|| {
            let weekday = database::get_current_weekday();
            (database::get_daily_limit(weekday) * 60) as i32
        });
        REMAINING_SECONDS.store(remaining, Ordering::SeqCst);
        SESSION_ACTIVE_SECONDS.store(database::get_session_active_time_for(&today), Ordering::SeqCst);
        *tracked = Some(today);
        true
    });
    if !new_day {
        return;
    }
    persist_day_counters();
    // The new day's remaining time follows a limit changed since it was
    // saved (or records the limit it reflects).
    crate::blocking::reconcile_todays_limit();
    let remaining = REMAINING_SECONDS.load(Ordering::SeqCst);

    update_mini_overlay();
    if remaining > 0 {
        crate::blocking::hide_blocking_overlay();
    }
}

/// Check idle state and update IS_IDLE_PAUSED accordingly
fn check_idle_state() {
    // Skip if idle detection is disabled
    if !database::is_idle_enabled() {
        // If it was idle-paused but feature got disabled, resume
        if IS_IDLE_PAUSED.load(Ordering::SeqCst) {
            IS_IDLE_PAUSED.store(false, Ordering::SeqCst);
        }
        return;
    }

    let idle_seconds = get_idle_seconds();
    let threshold_seconds = database::get_idle_timeout_minutes() * 60;
    let currently_idle_paused = IS_IDLE_PAUSED.load(Ordering::SeqCst);

    if idle_seconds >= threshold_seconds {
        // User is idle - pause if not already paused (manual or idle)
        if !currently_idle_paused && !IS_PAUSED.load(Ordering::SeqCst) {
            IS_IDLE_PAUSED.store(true, Ordering::SeqCst);
            let remaining = REMAINING_SECONDS.load(Ordering::SeqCst);
            crate::time_request::notify_activity_status(true, remaining);
        }
    } else if currently_idle_paused {
        // User is back - resume from idle pause
        IS_IDLE_PAUSED.store(false, Ordering::SeqCst);
        let remaining = REMAINING_SECONDS.load(Ordering::SeqCst);
        crate::time_request::notify_activity_status(false, remaining);
    }
}

/// Check if timer is currently idle-paused
pub fn is_idle_paused() -> bool {
    IS_IDLE_PAUSED.load(Ordering::SeqCst)
}

/// Record a session lock/unlock transition (see IS_LOCK_PAUSED) and notify
/// the admin bot(s), same as an idle-state transition. A no-op if this
/// doesn't actually change the current state, so it's safe to call from
/// WM_WTSSESSION_CHANGE without tracking whether we've already seen this
/// particular lock/unlock.
pub unsafe fn set_session_locked(locked: bool) {
    let was_locked = IS_LOCK_PAUSED.swap(locked, Ordering::SeqCst);
    if was_locked == locked {
        return;
    }
    let remaining = REMAINING_SECONDS.load(Ordering::SeqCst);
    crate::time_request::notify_session_lock(locked, remaining);
    update_mini_overlay();
}

/// Handle a WM_WTSSESSION_CHANGE lock/unlock message. Any process in this
/// session can post that message to our window, so a lock is checked
/// against what Windows itself reports first (see
/// `session::verified_lock_state`) - otherwise a posted fake lock would
/// freeze the countdown.
pub unsafe fn on_session_lock_message(message_says_locked: bool) {
    let locked = crate::session::verified_lock_state(
        message_says_locked,
        crate::session::query_session_locked(),
        crate::session::is_active_console_session(),
    );
    set_session_locked(locked);
}

/// Every LOCK_RECHECK_TICKS, whatever the current state: lock-pause if
/// Windows says this session is locked, unpause if it says unlocked and this
/// session is at the console (see `session::periodic_lock_state`). Catches a
/// real lock that was refused or never arrived, a missed unlock, and a lock
/// that couldn't be checked when it arrived.
unsafe fn recheck_session_lock() {
    let wts = crate::session::query_session_locked();
    let is_console = crate::session::is_active_console_session();
    if let Some(locked) = crate::session::periodic_lock_state(wts, is_console) {
        set_session_locked(locked);
    }
}

// ============================================================================
// Accounting tick (always on, owned by the hidden main window)
// ============================================================================

/// Start the accounting tick on the hidden main window. Call once at startup,
/// after the remaining/active time have been loaded: also folds a pause the
/// previous run was in the middle of into that day's usage, and starts
/// lock-paused if the session is already locked (no lock event would come).
pub unsafe fn start_accounting(main_hwnd: HWND) {
    if let Some(seconds) = database::recover_inflight_pause() {
        eprintln!("[Accounting] Counted {seconds}s of pause left over from the previous run");
    }
    if crate::session::query_session_locked() == Some(true) {
        IS_LOCK_PAUSED.store(true, Ordering::SeqCst);
    }
    LAST_TICK_MS.store(GetTickCount64(), Ordering::SeqCst);
    if SetTimer(main_hwnd, TIMER_ACCOUNTING_TICK, TICK_INTERVAL_MS, None) == 0 {
        eprintln!("[Accounting] SetTimer failed - time will not be counted");
    }
}

/// End a pause in progress so it's counted now. For WM_DESTROY (sign-out,
/// shutdown, Quit), since a restarted app never resumes a pause.
/// Also saves the day's counters: the periodic save runs only every 30
/// seconds, and the active time used to be saved nowhere else.
pub fn flush_on_exit() {
    if IS_PAUSED.load(Ordering::SeqCst) {
        resume_timer();
    }
    persist_day_counters();
}

/// What a tick does with the time that passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TickMode {
    /// Nothing counts: the lock overlay is up, or (not manually paused) the
    /// session is idle or locked.
    Frozen,
    /// Manually paused: the time counts against the pause.
    Pause,
    /// Running: the time counts down the remaining allowance.
    Countdown,
}

fn tick_mode(blocked: bool, paused: bool, idle_paused: bool, locked: bool) -> TickMode {
    if blocked {
        // The lock overlay always froze both: it used to stop this timer.
        TickMode::Frozen
    } else if paused {
        TickMode::Pause
    } else if idle_paused || locked {
        TickMode::Frozen
    } else {
        TickMode::Countdown
    }
}

/// Milliseconds to credit for the gap since the previous tick - see
/// MAX_CREDITED_GAP_MS.
fn tick_credit_ms(gap_ms: u64) -> u64 {
    if gap_ms > MAX_CREDITED_GAP_MS {
        TICK_INTERVAL_MS as u64
    } else {
        gap_ms
    }
}

/// Split credited milliseconds into whole seconds to count now and the
/// remainder to carry to the next tick.
fn split_whole_seconds(carry_ms: u64) -> (i32, u64) {
    ((carry_ms / 1000) as i32, carry_ms % 1000)
}

/// Whether counting down from `prev` to `new` passed a warning threshold.
/// Equivalent to the old "landed exactly on it" check when counting one
/// second at a time, but can't be skipped over by a multi-second step.
fn crossed_threshold(prev: i32, new: i32, threshold: i32) -> bool {
    prev > threshold && new <= threshold
}

/// Whether the lock overlay has to come up: no time left and not already
/// locked. Checked every tick, not just when a countdown step reaches zero,
/// so a limit cut to 0 (bot or Settings), a reset to a 0-minute day, or a
/// rollover onto one locks too. Negative remaining means "no limit".
fn zero_time_lock_due(remaining: i32, blocked: bool) -> bool {
    remaining == 0 && !blocked
}

/// The pause duration after counting `secs` more, and whether the pause has
/// hit its maximum and must end. On ending it's capped at the maximum, so a
/// multi-second step can't overshoot, but never below what was already
/// counted.
fn pause_step(prev: i32, secs: i32, max: i32) -> (i32, bool) {
    let new = prev + secs;
    if new >= max {
        (new.min(max).max(prev), true)
    } else {
        (new, false)
    }
}

/// The tick itself (WM_TIMER TIMER_ACCOUNTING_TICK on the main window).
pub unsafe fn on_accounting_tick() {
    let now = GetTickCount64();
    let last = LAST_TICK_MS.swap(now, Ordering::SeqCst);
    let credit = tick_credit_ms(now.saturating_sub(last));
    let tick = TICK_COUNT.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    let periodic = tick.is_multiple_of(PERIODIC_SAVE_TICKS);

    check_day_rollover(periodic);

    if tick.is_multiple_of(LOCK_RECHECK_TICKS) {
        recheck_session_lock();
    }

    let mode = tick_mode(
        crate::blocking::is_blocking_overlay_visible(),
        IS_PAUSED.load(Ordering::SeqCst),
        IS_IDLE_PAUSED.load(Ordering::SeqCst),
        IS_LOCK_PAUSED.load(Ordering::SeqCst),
    );
    let secs = if mode == TickMode::Frozen {
        CREDIT_CARRY_MS.store(0, Ordering::SeqCst);
        0
    } else {
        let (secs, rest) = split_whole_seconds(CREDIT_CARRY_MS.load(Ordering::SeqCst) + credit);
        CREDIT_CARRY_MS.store(rest, Ordering::SeqCst);
        secs
    };
    match mode {
        TickMode::Pause => accrue_pause(secs),
        TickMode::Countdown => count_down(secs),
        TickMode::Frozen => {}
    }

    enforce_zero_time();
    check_idle_state();
    crate::session::sync_remote_bots();

    if periodic {
        periodic_save();
    }

    let mini = HWND(MINI_OVERLAY_HWND.load(Ordering::SeqCst));
    if !mini.0.is_null() && MINI_OVERLAY_VISIBLE.load(Ordering::SeqCst) {
        crate::blocking::push_to_topmost(mini, false);
        let _ = InvalidateRect(mini, None, true);
    }
}

/// Count `secs` of manual pause; end the pause at its maximum. The pause so
/// far is saved every PAUSE_PERSIST_SECS, so a sign-out, crash, kill or update
/// mid-pause can't refund it (`database::recover_inflight_pause` counts it at
/// the next start).
fn accrue_pause(secs: i32) {
    if secs <= 0 {
        return;
    }
    let prev = CURRENT_PAUSE_DURATION.load(Ordering::SeqCst);
    let max = get_max_pause_duration();
    PAUSE_MAX_SECONDS.store(max, Ordering::SeqCst);
    let (duration, ends) = pause_step(prev, secs, max);
    CURRENT_PAUSE_DURATION.store(duration, Ordering::SeqCst);

    if ends {
        // Auto-resume
        force_resume();
    } else if prev / PAUSE_PERSIST_SECS != duration / PAUSE_PERSIST_SECS {
        persist_inflight_pause(
            duration,
            database::save_inflight_pause,
            // A read-modify-write, not a load: if it still reads "paused", a
            // resume after it (a swap) synchronizes with it, so the resume's
            // clear comes after the save above it.
            || IS_PAUSED.fetch_or(false, Ordering::SeqCst),
            database::clear_inflight_pause,
        );
    }
}

/// Save the pause so far (`save`), if it's still running. A resume from a
/// bot thread can land between that check and the save: it has counted the
/// pause and cleared the saved copy already, so the save put back a copy the
/// next start would count a second time. So the paused flag is checked again
/// after the save, and the copy cleared if the pause has ended.
fn persist_inflight_pause(
    duration: i32,
    save: impl FnOnce(i32),
    mut still_paused: impl FnMut() -> bool,
    clear: impl FnOnce(),
) {
    if !still_paused() {
        return;
    }
    save(duration);
    if !still_paused() {
        clear();
    }
}

/// Count `secs` off the remaining allowance, and fire the warnings it passes.
/// One atomic update, so an extension from a bot thread can't be lost
/// between reading and writing the remaining time.
unsafe fn count_down(secs: i32) {
    if secs <= 0 {
        return;
    }
    let Ok(prev) = REMAINING_SECONDS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
        (current > 0).then(|| (current - secs).max(0))
    }) else {
        return;
    };
    let new_time = (prev - secs).max(0);

    // Increment session active time
    SESSION_ACTIVE_SECONDS.fetch_add(prev - new_time, Ordering::SeqCst);
    ACCOUNTING_DIRTY.store(true, Ordering::SeqCst);

    // Warnings 1 and 2 (e.g. 10 and 5 minutes remaining)
    for warning in 1..=2 {
        let (warn_mins, warn_msg) = database::get_warning_config(warning);
        if crossed_threshold(prev, new_time, (warn_mins * 60) as i32) {
            crate::overlay::show_overlay(&warn_msg, 10);
            crate::time_request::notify_low_time(warn_mins, new_time);
        }
    }
}

/// Bring up the lock overlay if there's no time left (see `zero_time_lock_due`).
/// A manual pause in progress ends here and is counted.
unsafe fn enforce_zero_time() {
    let remaining = REMAINING_SECONDS.load(Ordering::SeqCst);
    if !zero_time_lock_due(remaining, crate::blocking::is_blocking_overlay_visible()) {
        return;
    }
    resume_timer(); // no-op unless paused
    persist_remaining();
    let msg = database::get_blocking_message();
    if !crate::blocking::show_out_of_time_overlay(&msg) {
        // A bot extended the time in the meantime: no lock. Save the new
        // time, in case the 0 above landed after the bot's own save.
        persist_remaining();
        return;
    }
    crate::time_request::notify_out_of_time();
}

/// Every PERIODIC_SAVE_TICKS: save counted time (the save also persists the
/// clock anchor), and send a clock-tamper or time-zone alert if one is
/// pending - whether or not time is currently being counted.
fn periodic_save() {
    if ACCOUNTING_DIRTY.swap(false, Ordering::SeqCst) {
        persist_day_counters();
    }
    if let Some(drift) = database::take_pending_clock_tamper_alert() {
        crate::time_request::notify_clock_tamper(drift);
    }
    if let Some((remembered, current)) = database::take_pending_time_zone_alert() {
        crate::time_request::notify_time_zone_change(&remembered, &current);
    }
}

/// Window procedure for the mini overlay
pub unsafe extern "system" fn mini_overlay_proc(
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

            let paused = IS_PAUSED.load(Ordering::SeqCst);
            let idle_paused = IS_IDLE_PAUSED.load(Ordering::SeqCst);
            let locked = IS_LOCK_PAUSED.load(Ordering::SeqCst);

            // Background color changes when paused
            let bg_color = if paused {
                0x00332200 // Brownish when manually paused
            } else if locked {
                0x00332222 // Dark red-ish when locked
            } else if idle_paused {
                0x00333333 // Grey when idle-paused
            } else {
                0x00222222 // Normal
            };
            let bg_brush = CreateSolidBrush(COLORREF(bg_color));
            FillRect(hdc, &rect, bg_brush);
            let _ = DeleteObject(bg_brush);

            // Get remaining time and pause info
            let remaining = REMAINING_SECONDS.load(Ordering::SeqCst);

            let (display_text, color) = if paused {
                // Show pause indicator and remaining pause time
                let pause_duration = CURRENT_PAUSE_DURATION.load(Ordering::SeqCst);
                let max_duration = PAUSE_MAX_SECONDS.load(Ordering::SeqCst);
                let pause_remaining = max_duration - pause_duration;

                // Format: "II 0:45" (pause symbol + remaining pause time)
                let pause_time_str = format_time_compact(pause_remaining);
                (format!("II {}", pause_time_str), 0x0066CCFF_u32) // Cyan/light blue for paused
            } else if locked {
                // Show lock indicator with remaining time - not that anyone
                // can see this while the actual Windows lock screen (a
                // separate secure desktop) is covering it, but it's correct
                // the instant the desktop is visible again too.
                let time_str = format_time_compact(remaining);
                (format!("LOCK {}", time_str), 0x005555FF_u32) // Red-ish for locked
            } else if idle_paused {
                // Show idle indicator with remaining time
                let time_str = format_time_compact(remaining);
                (format!("ZZ {}", time_str), 0x00888888_u32) // Grey/muted for idle
            } else {
                // Normal display
                let time_str = format_time_compact(remaining);
                let color = get_time_color(remaining);
                (time_str, color)
            };

            // Draw time (scaled font, ClearType quality = 5)
            let hfont = CreateFontW(
                scale(22), 0, 0, 0,
                FW_BOLD.0 as i32,
                0, 0, 0, 0, 0, 0, 5, 0,
                w!("Consolas"),
            );

            let old_font = SelectObject(hdc, hfont);
            SetTextColor(hdc, COLORREF(color));
            SetBkMode(hdc, TRANSPARENT);

            let wide_text: Vec<u16> = display_text.encode_utf16().collect();
            DrawTextW(
                hdc,
                &mut wide_text.clone(),
                &mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );

            SelectObject(hdc, old_font);
            let _ = DeleteObject(hfont);

            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            // Keep to the top-right corner of the (possibly resized) primary
            let (x, y) = mini_overlay_position();
            let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Register the mini overlay window class
pub unsafe fn register_mini_overlay_class(hinstance: windows::Win32::Foundation::HMODULE) {
    let class_name = w!("ScreenTimeMiniOverlayClass");
    let wnd_class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(mini_overlay_proc),
        hInstance: hinstance.into(),
        lpszClassName: class_name,
        hbrBackground: CreateSolidBrush(COLORREF(0x00222222)),
        ..zeroed()
    };

    if RegisterClassW(&wnd_class) == 0 {
        panic!("Failed to register mini overlay window class");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_just_before_midnight_still_rolls_over() {
        // Loaded for the 16th at 23:59:59; the first tick is on the 17th.
        assert!(is_new_day(Some("2026-11-16"), "2026-11-17"));
        assert!(!is_new_day(Some("2026-11-17"), "2026-11-17"));
        // Nothing loaded yet: nothing to roll over from
        assert!(!is_new_day(None, "2026-11-17"));
    }

    #[test]
    fn lock_overlay_freezes_everything() {
        assert_eq!(tick_mode(true, false, false, false), TickMode::Frozen);
        assert_eq!(tick_mode(true, true, false, false), TickMode::Frozen);
    }

    #[test]
    fn manual_pause_accrues_even_when_idle_or_locked() {
        assert_eq!(tick_mode(false, true, false, false), TickMode::Pause);
        assert_eq!(tick_mode(false, true, true, true), TickMode::Pause);
    }

    #[test]
    fn idle_and_session_lock_freeze_the_countdown() {
        assert_eq!(tick_mode(false, false, true, false), TickMode::Frozen);
        assert_eq!(tick_mode(false, false, false, true), TickMode::Frozen);
        assert_eq!(tick_mode(false, false, false, false), TickMode::Countdown);
    }

    #[test]
    fn short_stalls_are_credited_in_full() {
        assert_eq!(tick_credit_ms(1000), 1000);
        assert_eq!(tick_credit_ms(1016), 1016);
        assert_eq!(tick_credit_ms(4200), 4200);
        assert_eq!(tick_credit_ms(MAX_CREDITED_GAP_MS), MAX_CREDITED_GAP_MS);
    }

    #[test]
    fn sleep_and_suspension_count_as_one_tick() {
        // Overnight sleep or hibernate: no more than the old one second
        assert_eq!(tick_credit_ms(8 * 3600 * 1000), 1000);
        assert_eq!(tick_credit_ms(MAX_CREDITED_GAP_MS + 1), 1000);
        assert_eq!(tick_credit_ms(0), 0);
    }

    #[test]
    fn credit_carries_fractions() {
        assert_eq!(split_whole_seconds(999), (0, 999));
        assert_eq!(split_whole_seconds(1000), (1, 0));
        assert_eq!(split_whole_seconds(2015), (2, 15));
        // Sixty 1016 ms ticks add up to the real 60.96 s, not 60 x 1 s
        let mut carry = 0;
        let mut total = 0;
        for _ in 0..60 {
            let (s, rest) = split_whole_seconds(carry + 1016);
            total += s;
            carry = rest;
        }
        assert_eq!((total, carry), (60, 960));
    }

    #[test]
    fn warnings_fire_once_when_crossed() {
        assert!(crossed_threshold(601, 600, 600));
        assert!(crossed_threshold(603, 599, 600), "multi-second step");
        assert!(!crossed_threshold(600, 599, 600), "already past");
        assert!(!crossed_threshold(700, 650, 600));
        // A threshold of 0 behaves as before: fires on reaching zero
        assert!(crossed_threshold(1, 0, 0));
    }

    #[test]
    fn zero_time_locks_whenever_not_locked_already() {
        assert!(zero_time_lock_due(0, false));
        assert!(!zero_time_lock_due(0, true));
        assert!(!zero_time_lock_due(1, false));
        assert!(!zero_time_lock_due(-1, false), "negative means no limit");
    }

    #[test]
    fn extended_time_is_not_locked_on_the_next_tick() {
        // After an out-of-time show was undone because an extension arrived
        // (blocking::show_unless_time_arrived), the next tick doesn't retry.
        assert!(!zero_time_lock_due(15 * 60, false));
    }

    /// A stand-in for the saved in-flight pause and the paused flag.
    struct PauseFixture {
        paused: std::cell::Cell<bool>,
        saved: std::cell::Cell<Option<i32>>,
    }

    impl PauseFixture {
        fn new() -> Self {
            PauseFixture { paused: true.into(), saved: None.into() }
        }
        /// What `resume_timer` does to these two (counting the pause aside).
        fn resume(&self) {
            self.paused.set(false);
            self.saved.set(None);
        }
        fn persist(&self, duration: i32, before_save: impl FnOnce()) {
            persist_inflight_pause(
                duration,
                |d| {
                    before_save();
                    self.saved.set(Some(d));
                },
                || self.paused.get(),
                || self.saved.set(None),
            );
        }
    }

    #[test]
    fn running_pause_is_saved() {
        let f = PauseFixture::new();
        f.persist(130, || {});
        assert_eq!(f.saved.get(), Some(130));
    }

    #[test]
    fn ended_pause_is_not_saved() {
        let f = PauseFixture::new();
        f.resume();
        f.persist(130, || {});
        assert_eq!(f.saved.get(), None);
    }

    #[test]
    fn resume_racing_the_save_leaves_no_copy_to_count_twice() {
        // The bot's resume counts the pause and clears the copy just before
        // the tick's save writes it back.
        let f = PauseFixture::new();
        f.persist(130, || f.resume());
        assert_eq!(f.saved.get(), None, "would be counted again at the next start");
        assert!(!f.paused.get());
    }

    #[test]
    fn switched_off_pausing_leaves_no_budget() {
        assert_eq!(remaining_pause_budget(true, 45 * 60, 600), 45 * 60 - 600);
        assert_eq!(remaining_pause_budget(true, 600, 900), 0, "overspent");
        assert_eq!(remaining_pause_budget(false, 45 * 60, 0), 0, "disabled with a budget left");
        assert_eq!(remaining_pause_budget(true, 0, 0), 0, "daily pause of 0");
        // ...so a pause in progress ends on the next tick
        assert_eq!(pause_step(300, 1, remaining_pause_budget(false, 45 * 60, 0)), (300, true));
    }

    #[test]
    fn pause_ends_at_its_maximum() {
        assert_eq!(pause_step(10, 1, 1200), (11, false));
        assert_eq!(pause_step(1199, 1, 1200), (1200, true));
        assert_eq!(pause_step(1198, 4, 1200), (1200, true), "capped, no overshoot");
        // Budget shrank below what's already counted: end, keep what was counted
        assert_eq!(pause_step(500, 1, 300), (500, true));
    }

    /// The pre-fix `get_current_timestamp`: every month 30 days, local time.
    fn old_fake_timestamp(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
        ((y - 1970) * 365 + (mo - 1) * 30 + d) * 86400 + h * 3600 + mi * 60 + s
    }

    /// Real epoch seconds (Hinnant's days_from_civil, as in database.rs).
    fn epoch(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
        let yy = if mo <= 2 { y - 1 } else { y };
        let era = yy.div_euclid(400);
        let yoe = yy - era * 400;
        let mp = (mo + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        (era * 146097 + doe - 719468) * 86400 + h * 3600 + mi * 60 + s
    }

    #[test]
    fn epoch_helper_is_right() {
        assert_eq!(epoch(1970, 1, 1, 0, 0, 0), 0);
        assert_eq!(epoch(2000, 3, 1, 0, 0, 0), 951868800);
    }

    #[test]
    fn cooldown_from_real_timestamps() {
        let now = epoch(2026, 11, 15, 12, 0, 0);
        assert_eq!(cooldown_remaining_secs(now, 0, 900), 0, "never paused");
        assert_eq!(cooldown_remaining_secs(now, now - 100, 900), 800);
        assert_eq!(cooldown_remaining_secs(now, now - 900, 900), 0);
        assert_eq!(cooldown_remaining_secs(now, now - 5000, 900), 0);
        assert_eq!(cooldown_remaining_secs(now, now - 100, 0), 0, "no cooldown configured");
    }

    #[test]
    fn cooldown_spans_month_ends() {
        // Pause ended 23:55 on the 31st; 00:05 on the 1st is ten minutes later
        let end = epoch(2026, 10, 31, 23, 55, 0);
        let now = epoch(2026, 11, 1, 0, 5, 0);
        assert_eq!(cooldown_remaining_secs(now, end, 900), 300);
    }

    #[test]
    fn future_pause_end_never_locks_pause_out() {
        let now = epoch(2026, 11, 15, 12, 0, 0);
        // A small clock correction: at most one full cooldown
        assert_eq!(cooldown_remaining_secs(now, now + 30, 900), 900);
        // Far in the future: not a real end time, ignored
        assert_eq!(cooldown_remaining_secs(now, now + 86400, 900), 0);
        assert_eq!(cooldown_remaining_secs(now, i64::MAX / 2, 900), 0);
    }

    #[test]
    fn old_format_pause_end_reads_as_expired() {
        // A value saved by the old code at any time up to the update must not
        // hold pause back afterwards.
        for y in 2026..=2032 {
            for mo in 1..=12 {
                for d in 1..=28 {
                    let saved = old_fake_timestamp(y, mo, d, 23, 59, 59);
                    let now = epoch(y, mo, d, 0, 0, 0);
                    assert_eq!(cooldown_remaining_secs(now, saved, 3600), 0, "{y}-{mo}-{d}");
                }
            }
        }
    }
}
