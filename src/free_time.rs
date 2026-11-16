//! Free time: a parent suspends the countdown for a set length of time (bot
//! `free <minutes>`, or the tray menu with the passcode). While it runs the
//! PC is usable and no screen time is used up: the daily countdown doesn't
//! move, nothing is recorded as used, the low-time warnings don't fire, and
//! an out-of-time lock screen comes down until it ends (see
//! `mini_overlay::tick_mode` and `enforce_zero_time`).
//!
//! It is measured in real time, as a parent means it ("free until about
//! 4 pm"): it keeps running down while the PC is idle, locked, asleep, or
//! behind a parent's lock. A parent's lock wins over it - `free` is refused
//! while one is on, and a lock during free time locks at once while the free
//! time keeps running down underneath.
//!
//! A child must not be able to make it last longer:
//! - while the app runs it is counted down with `GetTickCount64`, which the
//!   Date & Time settings can't move and which goes on through sleep;
//! - what is left is saved (with the wall clock, the tick and the logon
//!   session at the time) every few seconds and per Windows account, so a
//!   restart, a kill, a reboot or midnight carry it over;
//! - across a time the app wasn't running, the gap since the save is taken
//!   off: by the tick within the same logon session (a kill and relaunch),
//!   otherwise by the wall clock, and only if that moved forward - a clock
//!   set back takes nothing off, but never adds anything either.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::database;

/// The longest free time a parent can give at once: a day.
pub const MAX_FREE_MINUTES: i32 = 24 * 60;

/// The child is warned this long before free time ends, if they'll have
/// little or no screen time afterwards (see `mini_overlay::run_down_free_time`).
pub const END_WARNING_SECS: i64 = 5 * 60;

/// Milliseconds of free time left; 0 while there is none.
static LEFT_MS: AtomicI64 = AtomicI64::new(0);

/// Serializes saves, and each save reads `LEFT_MS` while holding it - so the
/// last value written is always the latest one, and a save racing an end
/// can't bring an ended free time back at the next start.
static SAVE: Mutex<()> = Mutex::new(());

/// Per-account setting: "<seconds left>|<wall clock epoch secs>|<GetTickCount64
/// ms>|<logon token>" at the last save; empty when there's no free time.
const FREE_TIME_KEY: &str = "free_time";

/// Save while free time runs down this often (ms of free time).
const SAVE_EVERY_MS: i64 = 10_000;

/// Whether free time is on.
pub fn is_active() -> bool {
    LEFT_MS.load(Ordering::SeqCst) > 0
}

/// Whole seconds of free time left, rounded up (so it never reads 0:00 while
/// still on); 0 when off.
pub fn seconds_left() -> i32 {
    secs_rounded_up(LEFT_MS.load(Ordering::SeqCst))
}

fn secs_rounded_up(ms: i64) -> i32 {
    i32::try_from((ms.max(0) + 999) / 1000).unwrap_or(i32::MAX)
}

/// Why free time wasn't started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartRefused {
    /// Not 1 to `MAX_FREE_MINUTES` minutes.
    OutOfRange,
    /// A parent's lock is on: it has to be lifted first.
    ManualLock,
}

/// Whether free time of `minutes` may start, given whether a parent's lock
/// is on.
pub fn check_start(minutes: i32, manual_lock: bool) -> Result<(), StartRefused> {
    if !(1..=MAX_FREE_MINUTES).contains(&minutes) {
        Err(StartRefused::OutOfRange)
    } else if manual_lock {
        Err(StartRefused::ManualLock)
    } else {
        Ok(())
    }
}

/// Start free time of `minutes` (replacing any already on), saved at once.
/// A pause in progress ends and is counted as it stands - free time needs
/// no pause, and a pause must not eat the pause allowance meanwhile. An
/// out-of-time lock screen comes down. Any thread.
pub fn start(minutes: i32) -> Result<(), StartRefused> {
    check_start(minutes, crate::blocking::is_manual_lock())?;
    LEFT_MS.store(i64::from(minutes) * 60_000, Ordering::SeqCst);
    save();
    crate::mini_overlay::end_pause_for_free_time();
    unsafe {
        crate::blocking::enforce_free_time();
        crate::mini_overlay::update_mini_overlay();
    }
    Ok(())
}

/// End free time now, if it's on. Returns whether it was. The countdown
/// carries on from where it was; with no time left the accounting tick
/// brings the lock screen back within a second. Any thread.
pub fn end() -> bool {
    let was_on = LEFT_MS.swap(0, Ordering::SeqCst) > 0;
    save();
    unsafe { crate::mini_overlay::update_mini_overlay() };
    was_on
}

/// What running free time down did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunDown {
    /// Nothing worth acting on (still running, or off).
    Nothing,
    /// Crossed `END_WARNING_SECS` before the end.
    EndsSoon,
    /// Ran out just now.
    Ended,
}

/// Free time left after `elapsed_ms` more, and what that crossed.
fn run_down_step(left_ms: i64, elapsed_ms: i64) -> (i64, RunDown) {
    if left_ms <= 0 {
        return (0, RunDown::Nothing);
    }
    let new = left_ms.saturating_sub(elapsed_ms.max(0)).max(0);
    let warn_at = END_WARNING_SECS * 1000;
    let outcome = if new == 0 {
        RunDown::Ended
    } else if left_ms > warn_at && new <= warn_at {
        RunDown::EndsSoon
    } else {
        RunDown::Nothing
    };
    (new, outcome)
}

/// Run free time down by `elapsed_ms` of real time (every accounting tick,
/// whatever else the timer is doing). Saved every `SAVE_EVERY_MS` and at the
/// end. Only the caller that actually takes it to 0 is told it ended - a
/// parent's `free off` at the same moment wins, and then this says nothing.
pub fn run_down(elapsed_ms: u64) -> RunDown {
    let elapsed = i64::try_from(elapsed_ms).unwrap_or(i64::MAX);
    let Ok(prev) = LEFT_MS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
        (left > 0).then(|| run_down_step(left, elapsed).0)
    }) else {
        return RunDown::Nothing;
    };
    let (new, outcome) = run_down_step(prev, elapsed);
    if new == 0 || prev / SAVE_EVERY_MS != new / SAVE_EVERY_MS {
        save();
    }
    outcome
}

/// Load free time saved by the previous run, less the time since it was
/// saved (see the module comment). Call once at startup, before deciding
/// whether the lock screen comes up. Free time that ran out while the app
/// wasn't running is simply cleared - nothing changed for the child, and a
/// parent told "free time ended" the next morning would only be puzzled.
pub fn load_at_start() {
    let Some(raw) = database::get_setting(FREE_TIME_KEY) else { return };
    let Some(saved) = parse_saved(&raw) else {
        if !raw.is_empty() {
            eprintln!("[FreeTime] Ignoring an unreadable saved free time");
            database::set_setting(FREE_TIME_KEY, "");
        }
        return;
    };
    let left = left_after_gap(&saved, &reading_now());
    LEFT_MS.store(left.saturating_mul(1000), Ordering::SeqCst);
    save();
    if left > 0 {
        eprintln!("[FreeTime] {left}s of free time carried over from the previous run");
    }
}

/// Save what's left now (or that there's none). Any thread.
pub fn save() {
    let _serial = SAVE.lock().unwrap_or_else(PoisonError::into_inner);
    let left_ms = LEFT_MS.load(Ordering::SeqCst);
    let value = if left_ms > 0 {
        let now = reading_now();
        SavedFree {
            left_secs: i64::from(secs_rounded_up(left_ms)),
            wall_secs: now.wall_secs,
            tick_ms: now.tick_ms,
            logon_token: now.logon_token,
        }
        .serialize()
    } else {
        String::new()
    };
    database::set_setting(FREE_TIME_KEY, &value);
}

/// The clocks, as read now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reading {
    wall_secs: i64,
    tick_ms: i64,
    /// `session::logon_token`; `None` if it can't be read.
    logon_token: Option<i64>,
}

fn reading_now() -> Reading {
    use windows::Win32::System::SystemInformation::GetTickCount64;
    Reading {
        wall_secs: database::get_current_timestamp(),
        tick_ms: i64::try_from(unsafe { GetTickCount64() }).unwrap_or(i64::MAX),
        // 0 isn't a logon time: unknown, as in database::effective_today_date.
        logon_token: crate::session::logon_token().filter(|&t| t != 0),
    }
}

/// Free time as saved, with the clocks at the time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SavedFree {
    left_secs: i64,
    wall_secs: i64,
    tick_ms: i64,
    logon_token: Option<i64>,
}

/// Latest believable save time: 9999-12-31 23:59:59 UTC (as for the
/// in-flight pause), so the arithmetic on an edited value can't overflow.
const MAX_SAVED_WALL_SECS: i64 = 253_402_300_799;

impl SavedFree {
    fn serialize(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.left_secs,
            self.wall_secs,
            self.tick_ms,
            self.logon_token.map(|t| t.to_string()).unwrap_or_default()
        )
    }
}

/// Parse a `FREE_TIME_KEY` value. `None` for none, or anything malformed or
/// out of range - more than a day left can only be an edited value.
fn parse_saved(value: &str) -> Option<SavedFree> {
    let mut parts = value.split('|');
    let left_secs: i64 = parts.next()?.parse().ok()?;
    let wall_secs: i64 = parts.next()?.parse().ok()?;
    let tick_ms: i64 = parts.next()?.parse().ok()?;
    let token = parts.next()?;
    let logon_token = if token.is_empty() { None } else { Some(token.parse().ok()?) };
    if parts.next().is_some()
        || !(1..=i64::from(MAX_FREE_MINUTES) * 60).contains(&left_secs)
        || !(0..=MAX_SAVED_WALL_SECS).contains(&wall_secs)
        || tick_ms < 0
    {
        return None;
    }
    Some(SavedFree { left_secs, wall_secs, tick_ms, logon_token })
}

/// Seconds known to have passed since `saved` was written. In the same
/// logon session (the tick can't have restarted) by the tick, which the
/// clock settings can't move; otherwise by the wall clock, if it moved
/// forward - set back, nothing has passed as far as this can tell.
fn elapsed_since_save(saved: &SavedFree, now: &Reading) -> i64 {
    let same_session = saved.logon_token.is_some() && saved.logon_token == now.logon_token;
    if same_session && now.tick_ms >= saved.tick_ms {
        (now.tick_ms - saved.tick_ms) / 1000
    } else {
        now.wall_secs.saturating_sub(saved.wall_secs).max(0)
    }
}

/// Seconds of free time left at `now` (0: over).
fn left_after_gap(saved: &SavedFree, now: &Reading) -> i64 {
    saved.left_secs.saturating_sub(elapsed_since_save(saved, now)).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: Option<i64> = Some(133_000_000_000_000_000);

    fn saved(left_secs: i64) -> SavedFree {
        SavedFree { left_secs, wall_secs: 1_800_000_000, tick_ms: 50_000_000, logon_token: TOKEN }
    }

    /// `secs` after `saved(..)` was written, in the same logon session, with
    /// the wall clock moved by `wall_shift` on top.
    fn later_same_session(secs: i64, wall_shift: i64) -> Reading {
        Reading { wall_secs: 1_800_000_000 + secs + wall_shift, tick_ms: 50_000_000 + secs * 1000, logon_token: TOKEN }
    }

    /// After a reboot or sign-out (a new logon session, the tick restarted).
    fn after_reboot(wall_secs_later: i64) -> Reading {
        Reading { wall_secs: 1_800_000_000 + wall_secs_later, tick_ms: 90_000, logon_token: Some(133_000_000_999_000_000) }
    }

    #[test]
    fn a_relaunch_takes_off_the_time_the_app_was_gone() {
        // Killed and relaunched by the watchdog a minute later
        assert_eq!(left_after_gap(&saved(1800), &later_same_session(70, 0)), 1730);
    }

    #[test]
    fn setting_the_clock_back_never_lengthens_it() {
        // Same session: the tick decides, the clock moved back an hour is ignored
        assert_eq!(left_after_gap(&saved(1800), &later_same_session(70, -3600)), 1730);
        // After a reboot with the clock set back: nothing taken off, nothing added
        assert_eq!(left_after_gap(&saved(1800), &after_reboot(-7200)), 1800);
        assert!(left_after_gap(&saved(1800), &after_reboot(-7200)) <= saved(1800).left_secs);
    }

    #[test]
    fn setting_the_clock_forward_in_the_same_session_does_not_end_it() {
        assert_eq!(left_after_gap(&saved(1800), &later_same_session(10, 3600)), 1790);
    }

    #[test]
    fn a_reboot_takes_off_the_wall_clock_gap() {
        assert_eq!(left_after_gap(&saved(3600), &after_reboot(1200)), 2400);
        // Off for longer than was left: over
        assert_eq!(left_after_gap(&saved(3600), &after_reboot(8 * 3600)), 0);
    }

    #[test]
    fn without_a_session_to_compare_the_wall_clock_is_used() {
        let mut s = saved(1800);
        s.logon_token = None;
        let now = Reading { logon_token: None, ..later_same_session(100, 0) };
        assert_eq!(left_after_gap(&s, &now), 1700);
        // Tick gone backwards in what claims to be the same session
        let back = Reading { tick_ms: 1_000, ..later_same_session(100, 0) };
        assert_eq!(left_after_gap(&saved(1800), &back), 1700);
    }

    #[test]
    fn saved_values_round_trip_and_bad_ones_are_dropped() {
        let s = saved(1234);
        assert_eq!(parse_saved(&s.serialize()), Some(s));
        let no_token = SavedFree { logon_token: None, ..s };
        assert_eq!(parse_saved(&no_token.serialize()), Some(no_token));
        for bad in [
            "",
            "abc",
            "100|1800000000|5000",
            "100|1800000000|5000|x",
            "0|1800000000|5000|",
            "-5|1800000000|5000|",
            "86401|1800000000|5000|",
            "100|-1|5000|",
            "100|999999999999999|5000|",
            "100|1800000000|-1|",
            "100|1800000000|5000||extra",
        ] {
            assert_eq!(parse_saved(bad), None, "{bad:?}");
        }
        assert!(parse_saved("86400|1800000000|5000|").is_some(), "a whole day");
    }

    #[test]
    fn runs_down_and_ends_once() {
        assert_eq!(run_down_step(10_000, 1_000), (9_000, RunDown::Nothing));
        assert_eq!(run_down_step(1_000, 1_016), (0, RunDown::Ended));
        assert_eq!(run_down_step(0, 1_000), (0, RunDown::Nothing), "already off");
        // An hour asleep with 20 minutes left: over on waking
        assert_eq!(run_down_step(20 * 60_000, 3_600_000), (0, RunDown::Ended));
    }

    #[test]
    fn warns_once_shortly_before_the_end() {
        let warn = END_WARNING_SECS * 1000;
        assert_eq!(run_down_step(warn + 500, 1_000), (warn - 500, RunDown::EndsSoon));
        assert_eq!(run_down_step(warn - 500, 1_000).1, RunDown::Nothing, "already past");
        assert_eq!(run_down_step(warn + 10_000, 1_000).1, RunDown::Nothing);
        // Started with less than the warning time: no warning, just the end
        assert_eq!(run_down_step(3 * 60_000, 1_000).1, RunDown::Nothing);
    }

    #[test]
    fn seconds_left_round_up() {
        assert_eq!(secs_rounded_up(0), 0);
        assert_eq!(secs_rounded_up(1), 1);
        assert_eq!(secs_rounded_up(1_000), 1);
        assert_eq!(secs_rounded_up(1_001), 2);
        assert_eq!(secs_rounded_up(-5), 0);
    }

    #[test]
    fn starting_needs_a_sensible_length_and_no_parent_lock() {
        assert_eq!(check_start(30, false), Ok(()));
        assert_eq!(check_start(1, false), Ok(()));
        assert_eq!(check_start(MAX_FREE_MINUTES, false), Ok(()));
        assert_eq!(check_start(0, false), Err(StartRefused::OutOfRange));
        assert_eq!(check_start(-10, false), Err(StartRefused::OutOfRange));
        assert_eq!(check_start(MAX_FREE_MINUTES + 1, false), Err(StartRefused::OutOfRange));
        assert_eq!(check_start(30, true), Err(StartRefused::ManualLock));
    }
}
