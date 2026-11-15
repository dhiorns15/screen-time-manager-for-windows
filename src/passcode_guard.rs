//! Attempt limiting for the passcode, shared by every place a code is typed
//! (the tray's passcode dialog, the lock screen).
//!
//! The first few wrong codes cost nothing (typos happen); after that each
//! further wrong code locks passcode entry for an escalating time, capped at
//! `MAX_LOCKOUT_SECS`. The admin gets a bot alert when a run of wrong codes
//! first causes a lockout, and again (at most hourly) each time a lockout of
//! the full length is imposed. That turns "try all 10,000 codes in seconds"
//! into weeks. A correct code resets everything; so does a day with no wrong
//! codes. While locked, even the right code is refused - on every account -
//! so the admin can clear a lockout remotely (`clear_lockout`, the bots'
//! `clearlockout` command).
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
/// After that, an alert for a lockout of the full `MAX_LOCKOUT_SECS` is sent
/// at most this often, however many are imposed (one wrong code each time
/// the last one expires would otherwise mean an alert every 15 minutes).
const MAX_LOCKOUT_ALERT_INTERVAL_SECS: i64 = 60 * 60;

const STATE_KEY: &str = "passcode_guard";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct GuardState {
    failures: u32,
    last_failure: i64,
    locked_until: i64,
    alerted: bool,
    /// When the admin was last alerted about a full-length lockout (0 =
    /// not in this run).
    last_max_alert: i64,
}

impl GuardState {
    /// Accepts the current five-field form and the older four-field one
    /// (stored before full-length lockout alerts existed).
    fn parse(s: &str) -> Option<Self> {
        let mut parts = s.trim().split(',');
        let mut state = GuardState {
            failures: parts.next()?.parse().ok()?,
            last_failure: parts.next()?.parse().ok()?,
            locked_until: parts.next()?.parse().ok()?,
            alerted: parts.next()? == "1",
            last_max_alert: 0,
        };
        if let Some(last_max_alert) = parts.next() {
            state.last_max_alert = last_max_alert.parse().ok()?;
        }
        parts.next().is_none().then_some(state)
    }

    fn serialize(&self) -> String {
        format!(
            "{},{},{},{},{}",
            self.failures,
            self.last_failure,
            self.locked_until,
            u8::from(self.alerted),
            self.last_max_alert
        )
    }

    /// Whether the admin should be alerted that this state (just after a
    /// wrong code at `now`) imposes a full-length lockout - rate-limited, so
    /// it can't be used to flood the admin. A clock moved either way counts
    /// as the interval having passed.
    fn max_lockout_alert_due(&self, now: i64) -> bool {
        lockout_secs(self.failures) == MAX_LOCKOUT_SECS
            && (self.last_max_alert == 0 || (now - self.last_max_alert).abs() >= MAX_LOCKOUT_ALERT_INTERVAL_SECS)
    }

    /// The same run of wrong codes, with entry open again. The count is
    /// kept, so the next wrong code locks for the full time again rather
    /// than starting over with free attempts.
    fn without_lockout(self) -> Self {
        GuardState { locked_until: 0, ..self }
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
            last_max_alert: self.last_max_alert,
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
/// typed calls this (the tray's passcode dialog and the lock screen); there
/// is deliberately no unlimited check for UI code to use instead.
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
            } else if next.max_lockout_alert_due(now) {
                next.last_max_alert = now;
                notify_max_lockout(next.failures);
            }
            store(next);
            Attempt::Wrong { locked_for_secs: lockout_secs(next.failures) }
        }
        None => Attempt::Unavailable,
    }
}

/// Alert for a full-length lockout: someone is still entering wrong codes
/// (or keeping entry locked on purpose), and while it lasts even the right
/// code is refused. Sent like the first lockout alert (see
/// `time_request::notify_passcode_lockout`), and says how to clear it.
fn notify_max_lockout(failures: u32) {
    use crate::i18n::t;
    let text = format!(
        "🔐 {} {} {}\n⏳ {} {}\n{}",
        crate::remote_commands::current_windows_username(),
        t("passcode_guard.max.header"),
        failures,
        t("passcode_guard.max.locked"),
        crate::remote_commands::format_hm(MAX_LOCKOUT_SECS as i32),
        t("passcode_guard.max.suffix"),
    );
    crate::watchdog::raise_alert(&text);
}

/// Seconds left on the current lockout, 0 if entry is open. For showing a
/// countdown without making an attempt.
pub fn lockout_remaining() -> u64 {
    let now = now_secs();
    load().current(now).lockout_remaining(now)
}

/// Lift the current lockout, for the bots' admin-only `clearlockout`
/// command. Returns the seconds that were left on it, or `None` if entry
/// wasn't locked (then nothing changes).
pub fn clear_lockout() -> Option<u64> {
    let now = now_secs();
    let state = load().current(now);
    let remaining = state.lockout_remaining(now);
    if remaining == 0 {
        return None;
    }
    store(state.without_lockout());
    Some(remaining)
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
        let s = GuardState { failures: 10, last_failure: 5_000, locked_until: 5_900, alerted: true, last_max_alert: 0 };
        assert_eq!(s.lockout_remaining(5_000 - 3_600), MAX_LOCKOUT_SECS);
    }

    #[test]
    fn old_runs_are_forgotten() {
        let s = GuardState { failures: 9, last_failure: 0, locked_until: 480, alerted: true, last_max_alert: 0 };
        assert_eq!(s.current(FAILURE_MEMORY_SECS), s);
        assert_eq!(s.current(FAILURE_MEMORY_SECS + 1), GuardState::default());
        assert_eq!(s.current(-FAILURE_MEMORY_SECS - 1), GuardState::default());
    }

    #[test]
    fn state_round_trips() {
        let s = GuardState {
            failures: 7,
            last_failure: 1_700_000_000,
            locked_until: 1_700_000_120,
            alerted: true,
            last_max_alert: 1_699_999_000,
        };
        assert_eq!(GuardState::parse(&s.serialize()), Some(s));
        assert_eq!(GuardState::parse("1,2,3"), None);
        assert_eq!(GuardState::parse("1,2,3,0,9,9"), None);
        assert_eq!(GuardState::parse("1,2,3,0,x"), None);
        assert_eq!(GuardState::parse("x,2,3,0"), None);
    }

    #[test]
    fn state_stored_by_older_versions_still_reads() {
        assert_eq!(
            GuardState::parse("7,1700000000,1700000120,1"),
            Some(GuardState {
                failures: 7,
                last_failure: 1_700_000_000,
                locked_until: 1_700_000_120,
                alerted: true,
                last_max_alert: 0,
            })
        );
    }

    /// A child entering one wrong code each time the 15-minute lockout ends:
    /// the admin hears about it again, but at most once an hour.
    #[test]
    fn full_length_lockouts_alert_at_most_hourly() {
        let mut now = 1_000_000;
        let mut s = GuardState::default();
        let mut alerts = Vec::new();
        for _ in 0..30 {
            s = s.with_failure(now);
            if s.failures >= ALERT_AFTER_FAILURES && !s.alerted {
                s.alerted = true;
            } else if s.max_lockout_alert_due(now) {
                s.last_max_alert = now;
                alerts.push((s.failures, now));
            }
            now += s.lockout_remaining(now) as i64;
        }
        // The first full-length lockout (the 10th wrong code) alerts at once.
        assert_eq!(alerts.first().map(|a| a.0), Some(10));
        assert!(alerts.len() >= 3, "{alerts:?}");
        for pair in alerts.windows(2) {
            assert!(pair[1].1 - pair[0].1 >= MAX_LOCKOUT_ALERT_INTERVAL_SECS, "{alerts:?}");
        }
        // Shorter lockouts never use this alert.
        let short = GuardState { failures: 9, ..GuardState::default() };
        assert!(!short.max_lockout_alert_due(now));
    }

    #[test]
    fn clearing_a_lockout_opens_entry_but_keeps_the_count() {
        let t = 2_000_000;
        let mut s = GuardState::default();
        for i in 0..12 {
            s = s.with_failure(t + i);
        }
        assert_eq!(s.lockout_remaining(t + 12), MAX_LOCKOUT_SECS - 1);

        let cleared = s.without_lockout();
        assert_eq!(cleared.lockout_remaining(t + 12), 0, "the next code is checked");
        assert_eq!(cleared.failures, 12);
        assert_eq!(
            cleared.with_failure(t + 20).lockout_remaining(t + 20),
            MAX_LOCKOUT_SECS,
            "another wrong code locks for the full time again"
        );
    }

    #[test]
    fn wait_is_formatted() {
        assert_eq!(format_wait(0), "0:00");
        assert_eq!(format_wait(65), "1:05");
        assert_eq!(format_wait(900), "15:00");
    }
}
