//! Attempt limiting for the passcode, shared by every place a code is typed
//! (the tray's passcode dialog, the lock screen).
//!
//! The first few wrong codes cost nothing (typos happen); after that each
//! further wrong code locks passcode entry for an escalating time, capped at
//! `MAX_LOCKOUT_SECS`, and the admin gets one bot alert per run of wrong
//! codes. That turns "try all 10,000 codes in seconds" into weeks. A correct
//! code resets everything; so does a day with no wrong codes.
//!
//! The state lives in the shared database (so all accounts on the PC count
//! together) with an in-process copy as a fallback if the row disappears.
//! Like everything else stored where the child's account can write, a
//! determined child could reset it by editing the database; this is a speed
//! bump against guessing, not a security boundary.

use std::sync::Mutex;

use crate::database;

/// Result of one passcode attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// The code is right (passcode or rotating PIN).
    Correct,
    /// The code is wrong. `locked_for_secs` is how long entry is now locked
    /// (0 while still within the free attempts).
    Wrong { locked_for_secs: u64 },
    /// Entry is locked from earlier wrong codes; the code was not checked.
    LockedOut { secs_remaining: u64 },
    /// The stored passcode could not be read, so nothing is unlocked.
    Unavailable,
}

impl Attempt {
    // For the lock screen (blocking.rs), which is wired to this separately.
    #[allow(dead_code)]
    pub fn is_correct(self) -> bool {
        self == Attempt::Correct
    }
}

/// Wrong codes allowed before the first lockout.
const FREE_ATTEMPTS: u32 = 5;
/// First lockout, doubled for each further wrong code.
const FIRST_LOCKOUT_SECS: u64 = 30;
/// Longest single lockout. Also bounds how long a lockout can last if the
/// clock is moved.
const MAX_LOCKOUT_SECS: u64 = 15 * 60;
/// A run of wrong codes is forgotten after this long without another one.
const FAILURE_MEMORY_SECS: i64 = 24 * 60 * 60;
/// The admin is alerted once a run reaches this many wrong codes.
const ALERT_AFTER_FAILURES: u32 = FREE_ATTEMPTS;

const STATE_KEY: &str = "passcode_guard";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct GuardState {
    failures: u32,
    last_failure: i64,
    locked_until: i64,
    alerted: bool,
}

impl GuardState {
    fn parse(s: &str) -> Option<Self> {
        let mut parts = s.trim().split(',');
        let state = GuardState {
            failures: parts.next()?.parse().ok()?,
            last_failure: parts.next()?.parse().ok()?,
            locked_until: parts.next()?.parse().ok()?,
            alerted: parts.next()? == "1",
        };
        parts.next().is_none().then_some(state)
    }

    fn serialize(&self) -> String {
        format!("{},{},{},{}", self.failures, self.last_failure, self.locked_until, u8::from(self.alerted))
    }

    /// The state as it applies at `now`: a run older than the failure memory
    /// (or one dated implausibly far in the future) is forgotten.
    fn current(self, now: i64) -> Self {
        if (now - self.last_failure).abs() > FAILURE_MEMORY_SECS {
            GuardState::default()
        } else {
            self
        }
    }

    /// Seconds left on the lockout at `now`, never more than the longest
    /// lockout (so a clock set back can't stretch it).
    fn lockout_remaining(&self, now: i64) -> u64 {
        let remaining = self.locked_until.saturating_sub(now).max(0) as u64;
        remaining.min(MAX_LOCKOUT_SECS)
    }

    /// Record one more wrong code at `now`.
    fn with_failure(self, now: i64) -> Self {
        let failures = self.failures.saturating_add(1);
        GuardState {
            failures,
            last_failure: now,
            locked_until: now + lockout_secs(failures) as i64,
            alerted: self.alerted,
        }
    }
}

/// How long entry is locked after the `failures`-th wrong code in a row.
fn lockout_secs(failures: u32) -> u64 {
    if failures < FREE_ATTEMPTS {
        return 0;
    }
    let doublings = (failures - FREE_ATTEMPTS).min(16);
    (FIRST_LOCKOUT_SECS << doublings).min(MAX_LOCKOUT_SECS)
}

/// In-process copy, used when the stored row is missing.
static MIRROR: Mutex<Option<GuardState>> = Mutex::new(None);

fn load() -> GuardState {
    database::get_shared_setting(STATE_KEY)
        .and_then(|s| GuardState::parse(&s))
        .or_else(|| MIRROR.lock().ok().and_then(|m| *m))
        .unwrap_or_default()
}

fn store(state: GuardState) {
    if let Ok(mut mirror) = MIRROR.lock() {
        *mirror = Some(state);
    }
    database::set_shared_setting(STATE_KEY, &state.serialize());
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Check an entered code, with attempt limiting. Every place a passcode is
/// typed should call this instead of `database::verify_passcode`.
pub fn attempt(entered: &str) -> Attempt {
    let now = now_secs();
    let state = load().current(now);

    let locked = state.lockout_remaining(now);
    if locked > 0 {
        return Attempt::LockedOut { secs_remaining: locked };
    }

    // Pressing OK/Enter on an empty field isn't a guess.
    if entered.trim().is_empty() {
        return Attempt::Wrong { locked_for_secs: 0 };
    }

    match database::check_passcode(entered) {
        Some(true) => {
            if state != GuardState::default() {
                store(GuardState::default());
            }
            Attempt::Correct
        }
        Some(false) => {
            let mut next = state.with_failure(now);
            if next.failures >= ALERT_AFTER_FAILURES && !next.alerted {
                next.alerted = true;
                crate::time_request::notify_passcode_lockout(next.failures);
            }
            store(next);
            Attempt::Wrong { locked_for_secs: lockout_secs(next.failures) }
        }
        None => Attempt::Unavailable,
    }
}

/// Seconds left on the current lockout, 0 if entry is open. For showing a
/// countdown without making an attempt.
#[allow(dead_code)]
pub fn lockout_remaining() -> u64 {
    let now = now_secs();
    load().current(now).lockout_remaining(now)
}

/// "m:ss" for a lockout message.
pub fn format_wait(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_escalates_and_caps() {
        let schedule: Vec<u64> = (1..=12).map(lockout_secs).collect();
        assert_eq!(schedule, [0, 0, 0, 0, 30, 60, 120, 240, 480, 900, 900, 900]);
        assert_eq!(lockout_secs(u32::MAX), MAX_LOCKOUT_SECS);
    }

    #[test]
    fn brute_force_takes_weeks() {
        // Wall-clock time to get through all 10,000 codes when each attempt
        // is made the moment the previous lockout ends.
        let total: u64 = (1..=10_000).map(lockout_secs).sum();
        assert!(total > 90 * 24 * 3600, "{total}");
    }

    #[test]
    fn failures_lock_then_expire() {
        let t = 1_000_000;
        let mut s = GuardState::default();
        for i in 0..4 {
            s = s.with_failure(t + i);
            assert_eq!(s.lockout_remaining(t + i), 0);
        }
        s = s.with_failure(t + 10);
        assert_eq!(s.failures, 5);
        assert_eq!(s.lockout_remaining(t + 10), 30);
        assert_eq!(s.lockout_remaining(t + 39), 1);
        assert_eq!(s.lockout_remaining(t + 40), 0);
    }

    #[test]
    fn clock_set_back_does_not_stretch_a_lockout() {
        let s = GuardState { failures: 10, last_failure: 5_000, locked_until: 5_900, alerted: true };
        assert_eq!(s.lockout_remaining(5_000 - 3_600), MAX_LOCKOUT_SECS);
    }

    #[test]
    fn old_runs_are_forgotten() {
        let s = GuardState { failures: 9, last_failure: 0, locked_until: 480, alerted: true };
        assert_eq!(s.current(FAILURE_MEMORY_SECS), s);
        assert_eq!(s.current(FAILURE_MEMORY_SECS + 1), GuardState::default());
        assert_eq!(s.current(-FAILURE_MEMORY_SECS - 1), GuardState::default());
    }

    #[test]
    fn state_round_trips() {
        let s = GuardState { failures: 7, last_failure: 1_700_000_000, locked_until: 1_700_000_120, alerted: true };
        assert_eq!(GuardState::parse(&s.serialize()), Some(s));
        assert_eq!(GuardState::parse("1,2,3"), None);
        assert_eq!(GuardState::parse("1,2,3,0,9"), None);
        assert_eq!(GuardState::parse("x,2,3,0"), None);
    }

    #[test]
    fn wait_is_formatted() {
        assert_eq!(format_wait(0), "0:00");
        assert_eq!(format_wait(65), "1:05");
        assert_eq!(format_wait(900), "15:00");
    }
}
