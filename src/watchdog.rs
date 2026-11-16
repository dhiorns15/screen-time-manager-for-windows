//! Watchdog check - invoked as `screen-time-manager.exe --watchdog-check`
//! by a Scheduled Task that fires every minute (`ScreenTimeManagerWatchdog`,
//! registered by the installer - `installer/screen-time-manager.iss`, from
//! the `installer/task-watchdog.xml` template).
//!
//! Each run is a brief, separate process: it checks whether the main app's
//! single-instance mutex is currently held, and if not - and the app wasn't
//! stopped intentionally via the passcode-protected Quit menu item - relaunches
//! it (through the main app's own scheduled task, confirming it came up - see
//! `relaunch_app`) and alerts the configured admin via Telegram/Discord. This exists
//! because a standard Windows user can always kill their own processes
//! (admin rights aren't required to end-task something running in your own
//! session), so the enforcement can't rely solely on the tray app staying alive.
//!
//! When the mutex *is* held, the app's heartbeat (see `write_heartbeat`)
//! must also be recent: a suspended or frozen app, or some other process
//! squatting on the mutex name, holds the mutex without doing anything. See
//! `check_heartbeat` for how that avoids false alerts around sleep, Modern
//! Standby, reboots, a locked screen and Fast User Switching.
//!
//! Alerts are queued in a file and retried on later runs until delivered
//! (bounded in number and age), so one raised while offline isn't lost.

use windows::{
    core::{w, PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, BOOL, ERROR_FILE_NOT_FOUND, LPARAM, WPARAM},
        System::Threading::{
            GetCurrentProcessId, OpenMutexW, OpenProcess, QueryFullProcessImageNameW,
            TerminateProcess, WaitForSingleObject, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
            SYNCHRONIZATION_SYNCHRONIZE,
        },
        UI::WindowsAndMessaging::{
            FindWindowW, GetWindowThreadProcessId, SendMessageTimeoutW, SMTO_BLOCK, WM_NULL,
        },
    },
};

use crate::constants::MUTEX_NAME;
use crate::database;
use crate::i18n;
use crate::remote_commands::current_windows_username;

pub const WATCHDOG_ARG: &str = "--watchdog-check";

/// The main task's argument (installer/task-main.xml): a copy started through
/// the task that finds the app already running in its session exits without
/// the "already running" box (see main.rs).
pub const QUIET_IF_RUNNING_ARG: &str = "--quiet-if-running";

/// How long after this session's own logon to hold off on relaunch/alert if
/// the app has *never once* been confirmed running yet this session. The
/// main app task's own -AtLogOn trigger (see installer/task-main.xml) is what's actually
/// supposed to start it on every session start (a fresh sign-in, a restart,
/// Fast User Switching creating a brand-new session for an account that's
/// never logged on before) - it just hasn't necessarily finished doing so
/// yet by the time this check runs. A first-ever logon for a brand-new
/// Windows account in particular can take several minutes of Windows' own
/// profile setup before that trigger even fires, so this is deliberately
/// generous - it's safe to be, since it only applies before the app has ever
/// been seen running this session (see `was_confirmed_running_this_session`
/// below): once it *has* been seen running, any later "not running" alerts
/// immediately regardless of this window, so a quick kill right after a fast
/// start is still caught right away.
const STARTUP_GRACE_SECONDS: i64 = 600;

/// Check whether the app is running and relaunch/alert if it was killed
/// outside of a sanctioned Quit, then retry any alerts not yet delivered.
/// Safe to call repeatedly (e.g. every minute).
pub fn run_check() {
    check_app();
    flush_pending_alerts();
}

fn check_app() {
    let logon_token = crate::session::logon_token();

    if unsafe { app_is_running() } {
        if let Some(token) = logon_token {
            database::record_confirmed_running(token);
        }
        // Whatever this watchdog ended earlier has been started again.
        clear_watchdog_ended_record();
        if let Some(pending) = read_pending_relaunch() {
            clear_pending_relaunch();
            if let Some(age) = unbiased_now().and_then(|now| pending.age_secs(now)) {
                log(&format!("the app a watchdog run asked Task Scheduler to start {age}s ago is running"));
            }
        }
        check_heartbeat(logon_token);
        return;
    }

    // Not running: whatever a stale heartbeat was about is over.
    clear_stale_tracker();

    // An earlier run asked Task Scheduler to start the app and it hadn't
    // come up by the time that run stopped waiting. Give it a while longer
    // before starting another copy or reporting anything.
    if let Some(pending) = read_pending_relaunch() {
        let started_wall = database::app_last_started().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
        match pending.state(logon_token, unbiased_now(), read_heartbeat().as_ref(), started_wall) {
            PendingState::Waiting { age_secs } => {
                log_state("relaunch_pending", &format!(
                    "app not running - a watchdog run asked Task Scheduler to start it {age_secs}s ago and it may still be starting; not starting another copy or alerting for up to {RELAUNCH_PENDING_MAX_SECS}s"
                ));
                return;
            }
            PendingState::Expired { age_secs } => {
                clear_pending_relaunch();
                log(&format!("the app a watchdog run asked Task Scheduler to start {age_secs}s ago never came up"));
            }
            PendingState::CameUp | PendingState::NotThisSession => clear_pending_relaunch(),
        }
    }

    // Just installed or updated: setup stopped the app (or it never ran)
    // and isn't what starts it again - this check is. Comes before the
    // startup-grace logic below, which would otherwise either sit out its
    // whole window waiting for an -AtLogOn launch that isn't coming (the
    // session logged on before setup ran) or, past it, mistake the update
    // for tampering.
    if let Some(marker_age) = pending_post_install_relaunch() {
        if database::quit_was_intentional() {
            log_state("post_install_quit", &format!(
                "app not running - install/update finished {marker_age}s ago, but the last stop was an intentional quit, not relaunching"
            ));
            return;
        }
        let relaunch = relaunch_app();
        let relaunched = relaunch.outcome.word();
        log_state(&format!("post_install_relaunch_{relaunched}"), &format!(
            "app not running - install/update finished {marker_age}s ago, relaunched={relaunched} ({}), no tamper alert",
            relaunch.detail
        ));
        return;
    }

    let already_confirmed_this_session = logon_token
        .is_some_and(database::was_confirmed_running_this_session);

    if !already_confirmed_this_session {
        if let Some(elapsed) = crate::session::seconds_since_logon() {
            if elapsed < STARTUP_GRACE_SECONDS {
                log_state("startup_grace", &format!(
                    "app not running - only {elapsed}s since this session's logon and never yet confirmed running this session (< {STARTUP_GRACE_SECONDS}s grace) - assuming it's still starting, not alerting"
                ));
                return;
            }
        }
    }

    if database::quit_was_intentional() {
        log_state("intentional_quit", "app not running - last stop was an intentional quit, not alerting");
        return;
    }

    // This watchdog ended the app itself (it had stopped responding) and it
    // hasn't started since: the restart failed, and the admin was already
    // told so. Keep trying, but don't report our own action as a kill.
    let ended = std::fs::read_to_string(watchdog_ended_path()).ok().and_then(|s| WatchdogEnded::parse(&s));
    if let Some(age) = ended.and_then(|e| e.applies(wall_now(), logon_token)) {
        let relaunch = relaunch_app();
        let relaunched = relaunch.outcome.word();
        log_state(&format!("watchdog_ended_relaunch_{relaunched}"), &format!(
            "app not running - this watchdog ended it {age}s ago (unresponsive) and it hasn't started since, relaunched={relaunched} ({}), no tamper alert",
            relaunch.detail
        ));
        return;
    }

    let relaunch = relaunch_app();
    ensure_shared_database();
    log(&format!("app not running - relaunched={} ({}), sending tamper alert", relaunch.outcome.word(), relaunch.detail));
    set_last_state("tamper");
    send_tamper_alert(relaunch.outcome);
}

/// How long after setup touches `updating.marker` (next to the exe, see
/// update.rs) the watchdog treats "not running" as "waiting to be started
/// after the install/update" rather than as tampering.
const UPDATE_MARKER_MAX_AGE_SECS: u64 = 300;

/// If the app being down is explained by an install/update that just
/// finished, the marker's age in seconds. Narrower than the marker's whole
/// 5-minute window, so it can't be used to kill the app unnoticed:
/// - once the app has started again since the marker (see
///   `database::record_app_started`), a stop is a real one again - so a
///   kill a minute after the update still alerts;
/// - a session that logged on after the marker gets its own -AtLogOn launch,
///   so the ordinary startup grace below applies to it instead (relaunching
///   here too could race that launch into an "Already running" message).
fn pending_post_install_relaunch() -> Option<u64> {
    let (marker, text) = crate::update::read_update_marker()?;
    let stamp = InstallStamp::parse(&text);
    let boot = current_boot_id();
    let now_unbiased = unbiased_now();
    // A marker "from the future" (clock moved back) has no wall-clock age.
    let wall_age = std::time::SystemTime::now().duration_since(marker).ok().map(|d| d.as_secs());
    let age = install_marker_age(stamp, boot, now_unbiased, wall_age)?;
    if age >= UPDATE_MARKER_MAX_AGE_SECS {
        return None;
    }
    if database::app_last_started().is_some_and(|started| started >= marker)
        || started_since_install(stamp, boot, now_unbiased, read_heartbeat().as_ref())
    {
        return None;
    }
    if crate::session::seconds_since_logon().is_some_and(|since_logon| since_logon >= 0 && (since_logon as u64) < age) {
        return None;
    }
    Some(age)
}

/// When setup wrote `updating.marker`, in terms no clock change affects:
/// Windows' boot counter and the PC's awake time (QueryUnbiasedInterruptTime)
/// at that moment. Written by setup (from the release that added this on)
/// as `boot=<n> unbiased=<n>` after a readable date; markers without it -
/// older setups, or setup's crash recovery task - are judged by file time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InstallStamp {
    boot_id: u32,
    unbiased: u64,
}

impl InstallStamp {
    fn parse(text: &str) -> Option<Self> {
        let field = |name: &str| text.split_whitespace().find_map(|w| w.strip_prefix(name));
        Some(InstallStamp { boot_id: field("boot=")?.parse().ok()?, unbiased: field("unbiased=")?.parse().ok()? })
    }
}

/// How old the install marker is, in seconds. From its boot stamp when it
/// was written in this boot (awake time since then - a clock set forward or
/// back right after the install changes nothing); none at all when it was
/// written in an earlier boot (the app is started at sign-in then, not by
/// this); otherwise (no stamp, or the boot can't be told) by its file time.
fn install_marker_age(stamp: Option<InstallStamp>, boot: Option<u32>, now_unbiased: Option<u64>, wall_age: Option<u64>) -> Option<u64> {
    match (stamp, boot, now_unbiased) {
        (Some(s), Some(b), Some(now)) if s.boot_id == b => now.checked_sub(s.unbiased).map(|d| d / TICKS_PER_SEC),
        (Some(s), Some(b), _) if s.boot_id != b => None,
        _ => wall_age,
    }
}

/// Whether the app has written a heartbeat since the install marker's stamp
/// (same boot, later awake time): it has started since the install. Read
/// alongside `database::app_last_started`, which goes by the wall clock.
fn started_since_install(stamp: Option<InstallStamp>, boot: Option<u32>, now_unbiased: Option<u64>, hb: Option<&Heartbeat>) -> bool {
    match (stamp, boot, now_unbiased, hb) {
        (Some(s), Some(b), Some(now), Some(hb)) => s.boot_id == b && hb.unbiased >= s.unbiased && hb.unbiased <= now,
        _ => false,
    }
}

/// Windows' count of boots (`BootId`, the same value setup reads for the
/// marker). Readable by every account.
fn current_boot_id() -> Option<u32> {
    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            key: isize,
            sub_key: PCWSTR,
            value: PCWSTR,
            flags: u32,
            kind: *mut u32,
            data: *mut core::ffi::c_void,
            size: *mut u32,
        ) -> u32;
    }
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
    const RRF_RT_REG_DWORD: u32 = 0x10;
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters"),
            w!("BootId"),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut value as *mut u32 as *mut core::ffi::c_void,
            &mut size,
        )
    };
    (status == 0).then_some(value)
}

fn data_dir() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".screen-time-manager")
}

/// Small append-only log, since a Scheduled-Task-launched process has no
/// visible console to eprintln into - useful for diagnosing why a given run
/// did or didn't relaunch/alert.
fn log(msg: &str) {
    use std::io::Write;

    let path = data_dir().join("watchdog.log");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    trim_old_entries(&path, now);

    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{now}] {msg}");
    }
}

/// Drop log entries older than ~30 days, so the file doesn't grow forever.
/// This only logs when the watchdog's view of the app changes (see
/// `log_state`) or on a detected kill, not every per-minute tick, so it stays
/// small under normal use regardless - this just bounds it in the
/// crash-loop/repeated-tampering case too. Runs on every log() call rather
/// than needing separate "last cleaned up" state, since re-scanning a file
/// this size is cheap.
fn trim_old_entries(path: &std::path::Path, now: u64) {
    const MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;

    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    let cutoff = now.saturating_sub(MAX_AGE_SECS);
    let kept: String = contents
        .lines()
        .filter(|line| entry_timestamp(line).is_none_or(|t| t >= cutoff))
        .map(|line| format!("{line}\n"))
        .collect();
    if kept.len() != contents.len() {
        let _ = std::fs::write(path, kept);
    }
}

/// Parses the `[1234567890]` unix-timestamp prefix each log line starts with.
fn entry_timestamp(line: &str) -> Option<u64> {
    line.strip_prefix('[')?.split(']').next()?.parse().ok()
}

// ============================================================================
// State-change logging
// ============================================================================

/// What the previous run concluded (e.g. "running", "intentional_quit"), so
/// a condition that persists for hours is logged once, not once a minute.
fn state_path() -> std::path::PathBuf {
    data_dir().join("watchdog.state")
}

fn last_state() -> String {
    std::fs::read_to_string(state_path()).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn set_last_state(key: &str) {
    if last_state() != key {
        let _ = std::fs::write(state_path(), key);
    }
}

/// Log `msg` only if this run's conclusion `key` differs from the last run's.
fn log_state(key: &str, msg: &str) {
    if last_state() != key {
        log(msg);
        let _ = std::fs::write(state_path(), key);
    }
}

/// Whether the app holds its single-instance mutex in this session.
unsafe fn app_is_running() -> bool {
    named_mutex_exists(MUTEX_NAME)
}

/// Whether a mutex called `name` exists in this session's namespace. Only
/// opens it, never creates it: creating it (even for a moment) would make an
/// app starting at that instant find it taken and exit with "Already
/// running" - which matters now that the relaunch below polls this.
unsafe fn named_mutex_exists(name: &str) -> bool {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wide.as_ptr())) {
        Ok(h) => {
            let _ = CloseHandle(h);
            true
        }
        // Couldn't tell (e.g. access denied: it exists but isn't ours) -
        // assume running rather than risk a duplicate instance or a false
        // alert.
        Err(e) => e.code() != ERROR_FILE_NOT_FOUND.to_hresult(),
    }
}

/// Name of the main app's Scheduled Task, as registered by the installer
/// (installer/screen-time-manager.iss, from installer/task-main.xml).
const MAIN_TASK_NAME: &str = "ScreenTimeManager";

/// How long a relaunch waits for the app to take its mutex before this run
/// stops waiting. The app takes it first thing; Task Scheduler usually starts
/// it within a second or two. Bounded well inside the watchdog task's
/// 5-minute execution limit, and short enough that the next per-minute run
/// doesn't start while this one is still waiting. An app slower than this
/// isn't written off: see `PendingRelaunch`.
const RELAUNCH_CONFIRM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const RELAUNCH_CONFIRM_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// How long (awake time) after asking Task Scheduler to start the app later
/// runs wait for it to come up before trying again or reporting it: neither
/// a second copy nor an alert for an app that is merely slow to start.
const RELAUNCH_PENDING_MAX_SECS: u64 = 3 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelaunchOutcome {
    /// The app was seen holding its mutex in this session afterwards - the
    /// only thing reported as "restarted".
    Running,
    /// Task Scheduler accepted the start, but the app wasn't running yet when
    /// this run stopped waiting; later runs give it `RELAUNCH_PENDING_MAX_SECS`.
    Requested,
    /// The task couldn't be started at all.
    Failed,
}

impl RelaunchOutcome {
    /// For the log: `relaunched=true|pending|false`.
    fn word(self) -> &'static str {
        match self {
            RelaunchOutcome::Running => "true",
            RelaunchOutcome::Requested => "pending",
            RelaunchOutcome::Failed => "false",
        }
    }
}

/// What a relaunch did, and the log's account of it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Relaunch {
    outcome: RelaunchOutcome,
    detail: String,
}

/// Start the app through its own scheduled task and wait (bounded) until it
/// is running in this session.
///
/// Never a direct spawn: a child of this watchdog check runs inside the
/// watchdog task's job and is ended with it - first by the task's execution
/// limit (the 2026-07 relaunch/die loop), and in 1.0.1 a moment after the
/// check finished (the 2026-11-16 field report: "relaunched=true" with no app
/// running). CREATE_BREAKAWAY_FROM_JOB fails outright there. The main task has
/// its own job and no execution limit. If the task can't be started (not
/// registered, e.g. a dev build run without the installer, or not runnable by
/// this account), the relaunch fails and says so.
///
/// Once Task Scheduler has accepted the start, a `PendingRelaunch` is
/// recorded until the app is seen, so a run overlapping this one (the
/// after-logon run and the per-minute one) or the next minute's run doesn't
/// start a second copy or raise a second alert while it is still starting.
fn relaunch_app() -> Relaunch {
    let token = crate::session::logon_token();
    let relaunch = relaunch_with(
        || {
            let started = start_main_task()?;
            if let (Some(logon_token), Some(unbiased)) = (token, unbiased_now()) {
                let pending = PendingRelaunch { logon_token, unbiased, wall: wall_now() };
                let _ = std::fs::write(pending_relaunch_path(), pending.serialize());
            }
            Ok(started)
        },
        || wait_until(RELAUNCH_CONFIRM_TIMEOUT, RELAUNCH_CONFIRM_POLL, || unsafe { app_is_running() }),
    );
    if relaunch.outcome == RelaunchOutcome::Running {
        clear_pending_relaunch();
    }
    relaunch
}

/// `relaunch_app`'s decision, given how to start the task (`Ok` says how it
/// was asked to start, `Err` why it couldn't be) and how to wait for the app
/// (time it took to come up, or `None`).
fn relaunch_with(
    start: impl FnOnce() -> Result<String, String>,
    wait_for_app: impl FnOnce() -> Option<std::time::Duration>,
) -> Relaunch {
    match start() {
        Err(why) => Relaunch {
            outcome: RelaunchOutcome::Failed,
            detail: format!("could not start the {MAIN_TASK_NAME} task: {why}"),
        },
        Ok(how) => match wait_for_app() {
            Some(after) => Relaunch {
                outcome: RelaunchOutcome::Running,
                detail: format!("{how}; the app was running {:.1}s later", after.as_secs_f32()),
            },
            None => Relaunch {
                outcome: RelaunchOutcome::Requested,
                detail: format!(
                    "{how}, but the app was not running in this session yet {}s later; the next runs give it until {RELAUNCH_PENDING_MAX_SECS}s before trying again",
                    RELAUNCH_CONFIRM_TIMEOUT.as_secs()
                ),
            },
        },
    }
}

/// Poll `ready` every `poll` until it's true (returns how long that took) or
/// `timeout` has passed (`None`). Checks once more at the deadline.
fn wait_until(
    timeout: std::time::Duration,
    poll: std::time::Duration,
    mut ready: impl FnMut() -> bool,
) -> Option<std::time::Duration> {
    let started = std::time::Instant::now();
    loop {
        if ready() {
            return Some(started.elapsed());
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return None;
        }
        std::thread::sleep(poll.min(timeout - elapsed));
    }
}

/// A start of the app that Task Scheduler accepted and that hasn't been
/// seen running yet (`relaunch_pending`, per account).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingRelaunch {
    /// `session::logon_token` of the session it was started for.
    logon_token: i64,
    /// `QueryUnbiasedInterruptTime` when it was asked for.
    unbiased: u64,
    /// Wall-clock time (Unix seconds) when it was asked for.
    wall: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingState {
    /// Still within `RELAUNCH_PENDING_MAX_SECS`: leave it be.
    Waiting { age_secs: u64 },
    /// It never came up: carry on as if there had been none.
    Expired { age_secs: u64 },
    /// The app has started since (and stopped again).
    CameUp,
    /// Asked for in another session or boot.
    NotThisSession,
}

impl PendingRelaunch {
    fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split_whitespace();
        if parts.next()? != "1" {
            return None;
        }
        let p = PendingRelaunch {
            logon_token: parts.next()?.parse().ok()?,
            unbiased: parts.next()?.parse().ok()?,
            wall: parts.next()?.parse().ok()?,
        };
        parts.next().is_none().then_some(p)
    }

    fn serialize(&self) -> String {
        format!("1 {} {} {}", self.logon_token, self.unbiased, self.wall)
    }

    /// Awake seconds since it was asked for, if `now_unbiased` is from the same boot.
    fn age_secs(&self, now_unbiased: u64) -> Option<u64> {
        now_unbiased.checked_sub(self.unbiased).map(|d| d / TICKS_PER_SEC)
    }

    /// Where it stands, given this session, the awake time now, the app's
    /// heartbeat and when the app last started (Unix seconds).
    fn state(&self, logon_token: Option<i64>, now_unbiased: Option<u64>, hb: Option<&Heartbeat>, app_started_wall: Option<i64>) -> PendingState {
        let Some(age_secs) = now_unbiased.and_then(|now| self.age_secs(now)) else { return PendingState::NotThisSession };
        if logon_token != Some(self.logon_token) {
            return PendingState::NotThisSession;
        }
        let heartbeat_since = hb.is_some_and(|hb| hb.logon_token == self.logon_token && hb.unbiased >= self.unbiased);
        if heartbeat_since || app_started_wall.is_some_and(|t| t >= self.wall) {
            return PendingState::CameUp;
        }
        if age_secs < RELAUNCH_PENDING_MAX_SECS {
            PendingState::Waiting { age_secs }
        } else {
            PendingState::Expired { age_secs }
        }
    }
}

fn pending_relaunch_path() -> std::path::PathBuf {
    data_dir().join("relaunch_pending")
}

fn read_pending_relaunch() -> Option<PendingRelaunch> {
    std::fs::read_to_string(pending_relaunch_path()).ok().and_then(|s| PendingRelaunch::parse(&s))
}

fn clear_pending_relaunch() {
    let _ = std::fs::remove_file(pending_relaunch_path());
}

/// One way of asking Task Scheduler to start the main task.
struct RunRequest {
    /// How the log names it.
    label: String,
    flags: i32,
    session: i32,
    /// `RunEx`'s user argument; `None` passes none.
    user: Option<String>,
}

/// The requests to try, in order, for session `session` (> 0) and this
/// process's user SID, if known.
///
/// Task Scheduler's rules (MS-TSCH, SchRpcRun) for a caller without
/// administrative privileges - the watchdog always runs non-elevated: a
/// child's standard account, or an administrator's filtered token - are that
/// it must be the task's registered user unless it asks to run the task as
/// itself (TASK_RUN_AS_SELF), and that with TASK_RUN_USE_SESSION_ID it must
/// be the user signed in to that session (with AS_SELF: the caller itself;
/// otherwise: the user it names). The main task's principal is the Users
/// group, not this user, so:
/// 1. AS_SELF | USE_SESSION_ID in our own session: run as ourselves, where
///    we are signed in - the request a non-elevated member of the group is
///    entitled to make;
/// 2. USER_SID | USE_SESSION_ID naming our own SID: the same, said the other
///    way the rules allow;
/// 3. USE_SESSION_ID alone (1.0.2's request): run as whoever is signed in to
///    that session, which only an administrator may ask - kept for an
///    elevated watchdog (e.g. with UAC turned off).
///
/// `start_main_task` falls back to `schtasks /Run` after these.
fn run_requests(session: u32, sid: Option<&str>) -> Vec<RunRequest> {
    use windows::Win32::System::TaskScheduler::{TASK_RUN_AS_SELF, TASK_RUN_USER_SID, TASK_RUN_USE_SESSION_ID};
    let Ok(id) = i32::try_from(session) else { return Vec::new() };
    let mut requests = vec![RunRequest {
        label: format!("RunEx(AS_SELF|USE_SESSION_ID, session {session})"),
        flags: TASK_RUN_AS_SELF.0 | TASK_RUN_USE_SESSION_ID.0,
        session: id,
        user: None,
    }];
    if let Some(sid) = sid {
        requests.push(RunRequest {
            label: format!("RunEx(USER_SID|USE_SESSION_ID, {sid}, session {session})"),
            flags: TASK_RUN_USER_SID.0 | TASK_RUN_USE_SESSION_ID.0,
            session: id,
            user: Some(sid.to_string()),
        });
    }
    requests.push(RunRequest {
        label: format!("RunEx(USE_SESSION_ID, session {session})"),
        flags: TASK_RUN_USE_SESSION_ID.0,
        session: id,
        user: None,
    });
    requests
}

/// Ask Task Scheduler to start the main task in this process's own session,
/// trying `run_requests` in order and then `schtasks /Run` (a plain `Run`,
/// whose session is up to Task Scheduler - the caller confirms by the mutex
/// in this session either way, and a copy that lands in a session that has
/// one already leaves quietly, see `QUIET_IF_RUNNING_ARG`). Says which
/// request was accepted and how each one before it failed, and whether this
/// process is elevated (the rules differ). Running the task at all needs the
/// execute right setup grants standard accounts on it (installer/task-main.xml).
fn start_main_task() -> Result<String, String> {
    let who = if crate::update::is_elevated() { "an elevated" } else { "a non-elevated" };
    let mut failed = Vec::new();
    match crate::session::own_session_id() {
        Some(id) if id > 0 => {
            let requests = run_requests(id, own_user_sid().as_deref());
            match task_scheduler::run_first_accepted(MAIN_TASK_NAME, &requests) {
                Ok((Some(accepted), before)) => {
                    failed.extend(before);
                    let tried = if failed.is_empty() { String::new() } else { format!(" after {}", failed.join("; ")) };
                    return Ok(format!(
                        "started the {MAIN_TASK_NAME} task as {who} caller with {}{tried}",
                        requests[accepted].label
                    ));
                }
                Ok((None, all)) => failed.extend(all),
                Err(e) => failed.push(format!("could not open the task: {e}")),
            }
        }
        Some(id) => failed.push(format!("not trying RunEx from session {id}")),
        None => failed.push("not trying RunEx: this session's id is unknown".to_string()),
    }
    match schtasks_run(MAIN_TASK_NAME) {
        Ok(()) => Ok(format!(
            "started the {MAIN_TASK_NAME} task as {who} caller with schtasks /Run, after {}",
            failed.join("; ")
        )),
        Err(e) => {
            failed.push(e);
            Err(format!("as {who} caller: {}", failed.join("; ")))
        }
    }
}

/// This process's user, as a SID string (S-1-5-21-...), for `run_requests`.
fn own_user_sid() -> Option<String> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertSidToStringSidW(sid: *const core::ffi::c_void, string_sid: *mut *mut u16) -> BOOL;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(mem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }

    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    let mut len = 0u32;
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut len) };
    // u64s, so the buffer is aligned for TOKEN_USER.
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    let read = unsafe { GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len) };
    let _ = unsafe { CloseHandle(token) };
    read.ok()?;
    let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
    let mut text: *mut u16 = std::ptr::null_mut();
    if !unsafe { ConvertSidToStringSidW(user.User.Sid.0, &mut text) }.as_bool() || text.is_null() {
        return None;
    }
    let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    unsafe { LocalFree(text.cast()) };
    Some(sid)
}

fn schtasks_run(task: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    // schtasks is a console program and this process has no console -
    // without this a console window flashes up in the user's session.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let output = std::process::Command::new("schtasks")
        .args(["/Run", "/TN", task])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("schtasks: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    let said = said.trim();
    let said = if said.is_empty() { String::from_utf8_lossy(&output.stdout).trim().to_string() } else { said.to_string() };
    Err(format!("schtasks /Run failed ({}): {said}", output.status))
}

/// The Task Scheduler's COM interface, for what `schtasks` can't do: start a
/// task in a given session, as a given user.
mod task_scheduler {
    use windows::core::{BSTR, VARIANT};
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoInitializeSecurity, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_PKT_PRIVACY, RPC_C_IMP_LEVEL_IMPERSONATE,
    };
    use windows::Win32::System::TaskScheduler::{IRegisteredTask, ITaskService, TaskScheduler};

    use super::RunRequest;

    /// COM for this thread, for as long as it lives.
    struct Com {
        initialized: bool,
    }

    impl Com {
        fn init() -> Self {
            let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
            if initialized {
                // As in Microsoft's Task Scheduler samples. Fails harmlessly
                // if this process already set (or used) COM security.
                let _ = unsafe {
                    CoInitializeSecurity(
                        PSECURITY_DESCRIPTOR::default(),
                        -1,
                        None,
                        None,
                        RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
                        RPC_C_IMP_LEVEL_IMPERSONATE,
                        None,
                        EOAC_NONE,
                        None,
                    )
                };
            }
            Com { initialized }
        }
    }

    impl Drop for Com {
        fn drop(&mut self) {
            if self.initialized {
                unsafe { CoUninitialize() };
            }
        }
    }

    fn describe(e: &windows::core::Error) -> String {
        format!("{} (0x{:08X})", e.message(), e.code().0)
    }

    /// Look up the task called `name` in the root folder and hand it to `f`.
    /// The COM objects are all released before COM is uninitialized.
    pub(super) fn with_task<R>(name: &str, f: impl FnOnce(&IRegisteredTask) -> windows::core::Result<R>) -> Result<R, String> {
        let _com = Com::init();
        let run = || -> windows::core::Result<R> {
            let service: ITaskService = unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER) }?;
            let empty = VARIANT::default();
            unsafe { service.Connect(&empty, &empty, &empty, &empty) }?;
            let folder = unsafe { service.GetFolder(&BSTR::from("\\")) }?;
            let task = unsafe { folder.GetTask(&BSTR::from(name)) }?;
            f(&task)
        };
        run().map_err(|e| describe(&e))
    }

    /// Try `requests` in order on the task `name` until Task Scheduler
    /// accepts one: its index (or `None` if none was), and how each one
    /// before it failed. Accepted doesn't mean it ran (a disabled task, say,
    /// isn't run and may still be accepted) - the caller looks for the app.
    pub(super) fn run_first_accepted(name: &str, requests: &[RunRequest]) -> Result<(Option<usize>, Vec<String>), String> {
        with_task(name, |task| {
            let mut failed = Vec::new();
            for (i, request) in requests.iter().enumerate() {
                let user = request.user.as_deref().map(BSTR::from).unwrap_or_default();
                match unsafe { task.RunEx(&VARIANT::default(), request.flags, request.session, &user) } {
                    Ok(_) => return Ok((Some(i), failed)),
                    Err(e) => failed.push(format!("{} refused: {}", request.label, describe(&e))),
                }
            }
            Ok((None, failed))
        })
    }
}

fn send_tamper_alert(outcome: RelaunchOutcome) {
    let username = current_windows_username();
    let status = match outcome {
        RelaunchOutcome::Running => i18n::t("watchdog.alert.restarted"),
        RelaunchOutcome::Requested => i18n::t("watchdog.restart_pending"),
        RelaunchOutcome::Failed => i18n::t("watchdog.alert.restart_failed"),
    };
    let text = format!(
        "⚠️ {} {} {}\n{}",
        i18n::t("watchdog.alert.prefix"),
        username,
        status,
        i18n::t("watchdog.alert.suffix"),
    );
    queue_alert(&text);
}

// ============================================================================
// Heartbeat
// ============================================================================

/// How often the app rewrites its heartbeat (see main.rs).
pub const HEARTBEAT_INTERVAL_MS: u32 = 30_000;

/// QueryUnbiasedInterruptTime ticks (100 ns) per second.
const TICKS_PER_SEC: u64 = 10_000_000;

/// A heartbeat older than this (in time the PC was awake) counts as stale.
/// Ten missed beats: generous, since a false alarm here is far worse than
/// noticing a frozen app a few minutes later.
const HEARTBEAT_STALE_AFTER_SECS: u64 = 5 * 60;

/// A stale heartbeat must be seen by this many consecutive runs...
const STALE_CONFIRM_RUNS: u32 = 3;
/// ...spanning at least this much awake time...
const STALE_CONFIRM_SPAN_SECS: u64 = 2 * 60;
/// ...with no gap between two runs longer than this (the task runs every
/// minute; a longer gap means the PC or the session was not running
/// normally in between, so the count starts over).
const STALE_MAX_RUN_GAP_SECS: u64 = 3 * 60;
/// Wall-clock and awake time between the first and the latest stale
/// observation may differ by at most this much; more means the PC slept in
/// between (awake time stops, the wall clock doesn't), so start over.
const STALE_MAX_CLOCK_MISMATCH_SECS: i64 = 60;

#[link(name = "kernel32")]
extern "system" {
    /// Time since boot in 100 ns units, *excluding* time spent asleep or
    /// hibernating - so a heartbeat written just before a sleep isn't seen as
    /// hours old right after resume. Same value for every process and session,
    /// and reset by a reboot (the heartbeat records its session's logon so a
    /// reading from a previous boot or session is never compared).
    fn QueryUnbiasedInterruptTime(unbiased_time: *mut u64) -> BOOL;
}

fn unbiased_now() -> Option<u64> {
    let mut t = 0u64;
    unsafe { QueryUnbiasedInterruptTime(&mut t) }.as_bool().then_some(t)
}

fn wall_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Heartbeat {
    /// `session::logon_token` of the session the app runs in (0 if unknown).
    logon_token: i64,
    /// `QueryUnbiasedInterruptTime` when written.
    unbiased: u64,
    /// The app's process id.
    pid: u32,
}

impl Heartbeat {
    fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split_whitespace();
        if parts.next()? != "1" {
            return None;
        }
        let hb = Heartbeat {
            logon_token: parts.next()?.parse().ok()?,
            unbiased: parts.next()?.parse().ok()?,
            pid: parts.next()?.parse().ok()?,
        };
        parts.next().is_none().then_some(hb)
    }

    fn serialize(&self) -> String {
        format!("1 {} {} {}", self.logon_token, self.unbiased, self.pid)
    }
}

fn heartbeat_path() -> std::path::PathBuf {
    data_dir().join("heartbeat")
}

/// Record that the app is alive. Called by the app right after it takes the
/// single-instance mutex and then every `HEARTBEAT_INTERVAL_MS` from a timer
/// on its UI thread, so a frozen UI thread or a suspended process stops it.
/// Cheap, never panics, and failures are ignored (the watchdog checks that
/// the app's window still responds before acting on a stale heartbeat).
pub fn write_heartbeat() {
    let Some(unbiased) = unbiased_now() else { return };
    let hb = Heartbeat {
        logon_token: crate::session::logon_token().unwrap_or(0),
        unbiased,
        pid: unsafe { GetCurrentProcessId() },
    };
    if std::fs::write(heartbeat_path(), hb.serialize()).is_err() {
        // First start in a new account: the folder may not exist yet.
        let _ = std::fs::create_dir_all(data_dir());
        let _ = std::fs::write(heartbeat_path(), hb.serialize());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeartbeatStatus {
    /// Nothing comparable: no heartbeat (e.g. an older app version that
    /// doesn't write one), one from another session or boot, or no clock.
    Unknown,
    Fresh,
    Stale { age_secs: u64 },
}

fn heartbeat_status(hb: Option<&Heartbeat>, logon_token: i64, now_unbiased: Option<u64>) -> HeartbeatStatus {
    let (Some(hb), Some(now)) = (hb, now_unbiased) else { return HeartbeatStatus::Unknown };
    if hb.logon_token == 0 || hb.logon_token != logon_token || hb.unbiased > now {
        return HeartbeatStatus::Unknown;
    }
    let age_secs = (now - hb.unbiased) / TICKS_PER_SEC;
    if age_secs < HEARTBEAT_STALE_AFTER_SECS {
        HeartbeatStatus::Fresh
    } else {
        HeartbeatStatus::Stale { age_secs }
    }
}

/// Consecutive watchdog runs that saw the same stale heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StaleTracker {
    logon_token: i64,
    hb_unbiased: u64,
    first_unbiased: u64,
    first_wall: i64,
    last_unbiased: u64,
    runs: u32,
    /// The incident was already acted on (alerted/recovered) - once per
    /// incident, not once a minute.
    handled: bool,
}

impl StaleTracker {
    fn parse(s: &str) -> Option<Self> {
        let v: Vec<&str> = s.split_whitespace().collect();
        let [token, hb, first_u, first_w, last_u, runs, handled] = v[..] else { return None };
        Some(StaleTracker {
            logon_token: token.parse().ok()?,
            hb_unbiased: hb.parse().ok()?,
            first_unbiased: first_u.parse().ok()?,
            first_wall: first_w.parse().ok()?,
            last_unbiased: last_u.parse().ok()?,
            runs: runs.parse().ok()?,
            handled: handled == "1",
        })
    }

    fn serialize(&self) -> String {
        format!(
            "{} {} {} {} {} {} {}",
            self.logon_token, self.hb_unbiased, self.first_unbiased, self.first_wall,
            self.last_unbiased, self.runs, u8::from(self.handled)
        )
    }

    /// Fold this run's observation of the stale heartbeat `hb` into `prev`.
    fn observe(prev: Option<StaleTracker>, hb: &Heartbeat, now_unbiased: u64, now_wall: i64) -> StaleTracker {
        if let Some(p) = prev {
            let same_incident = p.logon_token == hb.logon_token && p.hb_unbiased == hb.unbiased;
            let gap_ok = now_unbiased >= p.last_unbiased
                && (now_unbiased - p.last_unbiased) / TICKS_PER_SEC <= STALE_MAX_RUN_GAP_SECS;
            let awake = (now_unbiased.saturating_sub(p.first_unbiased) / TICKS_PER_SEC) as i64;
            let clocks_agree = ((now_wall - p.first_wall) - awake).abs() <= STALE_MAX_CLOCK_MISMATCH_SECS;
            if same_incident && gap_ok && clocks_agree {
                return StaleTracker { last_unbiased: now_unbiased, runs: p.runs.saturating_add(1), ..p };
            }
            if same_incident && p.handled {
                // Already acted on; don't start counting towards a second
                // alert for the very same heartbeat.
                return StaleTracker { last_unbiased: now_unbiased, ..p };
            }
        }
        StaleTracker {
            logon_token: hb.logon_token,
            hb_unbiased: hb.unbiased,
            first_unbiased: now_unbiased,
            first_wall: now_wall,
            last_unbiased: now_unbiased,
            runs: 1,
            handled: false,
        }
    }

    fn confirmed(&self) -> bool {
        self.runs >= STALE_CONFIRM_RUNS
            && (self.last_unbiased - self.first_unbiased) / TICKS_PER_SEC >= STALE_CONFIRM_SPAN_SECS
    }
}

fn stale_tracker_path() -> std::path::PathBuf {
    data_dir().join("heartbeat_stale")
}

fn clear_stale_tracker() {
    let _ = std::fs::remove_file(stale_tracker_path());
}

/// The app holds the mutex; check its heartbeat. A stale one has to be seen
/// on several consecutive runs over a couple of minutes of awake time (see
/// `StaleTracker`) before anything happens, and only while someone is using
/// the session (see `someone_using_session`). Then the app's window is
/// probed, twice with a pause in between: if it answers, or the heartbeat
/// moves on meanwhile, nothing is done. Only an app that stays unresponsive
/// is ended and restarted, and the admin is told once per incident. Nothing
/// is done either while the app is being quit (or the session is ending),
/// only one run acts on an incident, and after `MAX_HUNG_RESTARTS` restarts
/// within an hour the app is left alone (see `RestartHistory`).
fn check_heartbeat(logon_token: Option<i64>) {
    let Some(token) = logon_token else {
        set_last_state("running");
        return;
    };
    let hb = read_heartbeat();
    let now_unbiased = unbiased_now();

    match heartbeat_status(hb.as_ref(), token, now_unbiased) {
        HeartbeatStatus::Unknown => {
            clear_stale_tracker();
            set_last_state("running");
        }
        HeartbeatStatus::Fresh => {
            clear_stale_tracker();
            if last_state().starts_with("hb_") {
                log("app heartbeat is fresh again");
            }
            set_last_state("running");
        }
        HeartbeatStatus::Stale { age_secs } => {
            let (Some(hb), Some(now_unbiased)) = (hb, now_unbiased) else { return };
            if database::quit_was_intentional() {
                // Quitting via the passcode-protected Quit (or the session
                // is ending) and still holding the mutex while it exits:
                // not a frozen app to end and restart against that Quit.
                log_state("hb_stale_quitting", &format!(
                    "app holds its mutex but its heartbeat is {age_secs}s old - the last stop was an intentional quit or the session is ending, not acting"
                ));
                return;
            }
            let prev = read_stale_tracker();
            let mut tracker = StaleTracker::observe(prev, &hb, now_unbiased, wall_now());
            if tracker.confirmed() && !tracker.handled {
                let attended = someone_using_session(
                    crate::session::is_active_console_session(),
                    crate::session::query_session_locked(),
                    session_idle_secs(),
                    age_secs,
                );
                if !attended {
                    // Not handled yet: acted on at a later run if someone
                    // starts using the PC while it's still stuck.
                    let _ = std::fs::write(stale_tracker_path(), tracker.serialize());
                    log_state("hb_stale_unattended", &format!(
                        "app holds its mutex but its heartbeat is {age_secs}s old - not acting while nobody is using this session (no keyboard/mouse input since, locked, or not at the console)"
                    ));
                    return;
                }
                // Only one run acts on an incident: the per-minute run and
                // the delayed logon run can overlap, and a second run acting
                // on an incident the first just dealt with would find the
                // relaunched app without a window yet and raise a false alert.
                let Some(_incident) = FileLock::try_acquire(incident_lock_path(), INCIDENT_LOCK_STALE_SECS) else {
                    log_state("hb_incident_busy", &format!(
                        "heartbeat {age_secs}s old - another watchdog run is dealing with it, not acting"
                    ));
                    return;
                };
                let still_stands = incident_still_stands(
                    read_stale_tracker(),
                    read_heartbeat(),
                    &hb,
                    unsafe { app_is_running() },
                );
                if !still_stands {
                    log_state("hb_incident_over", &format!(
                        "heartbeat {age_secs}s old, but by the time this run could act the incident was already dealt with or over - not acting"
                    ));
                    return;
                }
                tracker.handled = true;
                let _ = std::fs::write(stale_tracker_path(), tracker.serialize());
                handle_unresponsive_app(&hb, age_secs);
            } else {
                let _ = std::fs::write(stale_tracker_path(), tracker.serialize());
                if !tracker.handled {
                    log_state("hb_stale", &format!(
                        "app holds its mutex but its heartbeat is {age_secs}s old - watching (run {} of {STALE_CONFIRM_RUNS})",
                        tracker.runs
                    ));
                }
            }
        }
    }
}

/// Keyboard or mouse input has to have come at least this long after the
/// app's last heartbeat for a stuck app to count as one someone is using.
const INPUT_AFTER_HEARTBEAT_MARGIN_SECS: u64 = 10;

/// Whether someone is evidently using this session while its app is stuck:
/// it's the session at the console, it isn't locked, and there has been
/// keyboard or mouse input since the app's last heartbeat. Only then can a
/// stuck app be costing screen time, so only then is it ended and the admin
/// alerted. Everything else that can stop the heartbeat without anyone
/// tampering - a PC left alone whose display went off (Modern Standby
/// suspends desktop apps), a session switched away from or locked - has no
/// such input, and waits: if someone starts using the PC while the app is
/// still stuck, the next run acts. If the input time can't be read, nothing
/// is done (a missed detection is better than a false alert).
fn someone_using_session(is_console: bool, locked: Option<bool>, idle_secs: Option<u64>, heartbeat_age_secs: u64) -> bool {
    is_console
        && locked != Some(true)
        && idle_secs.is_some_and(|idle| idle.saturating_add(INPUT_AFTER_HEARTBEAT_MARGIN_SECS) < heartbeat_age_secs)
}

/// Seconds since this session's last keyboard or mouse input.
fn session_idle_secs() -> Option<u64> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    unsafe { GetLastInputInfo(&mut info) }
        .as_bool()
        .then(|| u64::from(unsafe { GetTickCount() }.wrapping_sub(info.dwTime) / 1000))
}

/// Class name of the app's hidden main window (main.rs).
const MAIN_WINDOW_CLASS: PCWSTR = w!("ScreenTimeManagerClass");

/// How long to wait for the app's window to answer a probe.
const PROBE_TIMEOUT_MS: u32 = 5000;
/// Before acting on an app that didn't answer, look again after this long.
const SECOND_LOOK_DELAY: std::time::Duration = std::time::Duration::from_secs(15);

/// Whether the window answers a WM_NULL within `PROBE_TIMEOUT_MS`. Not
/// SMTO_ABORTIFHUNG: an app that has only just been resumed (e.g. from
/// standby) still counts as "hung" to Windows until it pumps messages again,
/// so it's given the full timeout to do so.
fn window_answers(hwnd: windows::Win32::Foundation::HWND) -> bool {
    let mut result = 0usize;
    let answered = unsafe {
        SendMessageTimeoutW(hwnd, WM_NULL, WPARAM(0), LPARAM(0), SMTO_BLOCK, PROBE_TIMEOUT_MS, Some(&mut result))
    };
    answered.0 != 0
}

/// Whether the app has written a different heartbeat than `hb` since.
fn heartbeat_moved_on(hb: &Heartbeat) -> bool {
    read_heartbeat().is_some_and(|now| now != *hb)
}

fn handle_unresponsive_app(hb: &Heartbeat, age_secs: u64) {
    let window = unsafe { FindWindowW(MAIN_WINDOW_CLASS, PCWSTR::null()) }.ok().filter(|h| !h.0.is_null());

    if window.is_some_and(window_answers) {
        log_state("hb_responsive", &format!(
            "heartbeat {age_secs}s old but the app's window still responds - heartbeat file not updating? not alerting"
        ));
        return;
    }

    // Second look before doing anything drastic: an app that was merely
    // paused and has just resumed catches up within seconds, starting with
    // its overdue heartbeat.
    std::thread::sleep(SECOND_LOOK_DELAY);
    if heartbeat_moved_on(hb) || window.is_some_and(window_answers) {
        log_state("hb_recovered", &format!(
            "heartbeat {age_secs}s old and the app didn't answer at first, but it's running again - not alerting"
        ));
        return;
    }

    // A Quit (or sign-out) during the probes: the app is on its way out.
    if database::quit_was_intentional() {
        log_state("hb_stale_quitting", &format!(
            "heartbeat {age_secs}s old and the app didn't answer, but it is being quit or the session is ending - not acting"
        ));
        return;
    }

    // The process to end: the one that owns the unresponsive window, or
    // (no window yet/anymore) the one that wrote the heartbeat.
    let target_pid = window
        .map(|hwnd| {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            pid
        })
        .filter(|&pid| pid != 0)
        .unwrap_or(hb.pid);

    ensure_shared_database();
    let username = current_windows_username();
    let now = wall_now();

    let history_path = hung_restarts_path();
    let mut history = std::fs::read_to_string(&history_path)
        .map(|s| RestartHistory::parse(&s))
        .unwrap_or_default();
    match history.decide(now) {
        HungDecision::Restart => {}
        decision => {
            // It keeps freezing again after each restart: stop ending it
            // (each restart would be another kill and another alert every
            // few minutes) and say so once.
            if decision == HungDecision::GiveUpAndAlert {
                history.gave_up_alerted = Some(now);
                let _ = std::fs::write(&history_path, history.serialize());
                queue_alert(&format!(
                    "⚠️ {} {} {}\n{}",
                    i18n::t("watchdog.hung.prefix"),
                    username,
                    i18n::t("watchdog.hung.gave_up"),
                    i18n::t("watchdog.hung.suffix"),
                ));
            }
            log_state("hb_gave_up", &format!(
                "heartbeat {age_secs}s old and the app didn't respond, but it was already restarted {MAX_HUNG_RESTARTS} times within {HUNG_RESTART_WINDOW_SECS}s - leaving it alone (alerted once)"
            ));
            return;
        }
    }

    // Written before the process is ended, so that the relaunched app
    // (which clears it as it starts) can't start before it exists.
    let ended = WatchdogEnded { wall: now, logon_token: hb.logon_token };
    let _ = std::fs::write(watchdog_ended_path(), ended.serialize());

    if unsafe { end_own_app_process(target_pid) } {
        history.record_restart(now);
        let _ = std::fs::write(&history_path, history.serialize());
        let relaunch = relaunch_app();
        log_state("hb_restarted", &format!(
            "heartbeat {age_secs}s old and the app didn't respond - ended pid {target_pid}, relaunched={} ({}), alerting",
            relaunch.outcome.word(),
            relaunch.detail
        ));
        let status = match relaunch.outcome {
            RelaunchOutcome::Running => i18n::t("watchdog.hung.restarted"),
            RelaunchOutcome::Requested => i18n::t("watchdog.restart_pending"),
            RelaunchOutcome::Failed => i18n::t("watchdog.hung.restart_failed"),
        };
        queue_alert(&format!(
            "⚠️ {} {} {}\n{}",
            i18n::t("watchdog.hung.prefix"),
            username,
            status,
            i18n::t("watchdog.hung.suffix"),
        ));
        return;
    }
    clear_watchdog_ended_record();

    // Nothing of ours to end. Before blaming something else for holding
    // the mutex, make sure the app didn't just exit or come back.
    if !unsafe { app_is_running() } || heartbeat_moved_on(hb) || database::quit_was_intentional() {
        log_state("hb_recovered", &format!(
            "heartbeat {age_secs}s old and the app didn't answer, but it has exited or is running again since - not alerting"
        ));
        return;
    }
    log_state("hb_blocked", &format!(
        "heartbeat {age_secs}s old, mutex held, and no unresponsive app process (pid {target_pid}) to end - something else holds the mutex, alerting"
    ));
    queue_alert(&format!(
        "⚠️ {} {} {}",
        i18n::t("watchdog.blocked.prefix"),
        username,
        i18n::t("watchdog.blocked.suffix"),
    ));
}

/// Whether the stale-heartbeat incident about `hb` still needs acting on,
/// checked again once this run has the incident lock: the heartbeat on disk
/// must still be `hb` (unreadable counts as moved on), no other run may have
/// handled it already, and the app must still hold its mutex.
fn incident_still_stands(
    on_disk: Option<StaleTracker>,
    current_hb: Option<Heartbeat>,
    hb: &Heartbeat,
    app_running: bool,
) -> bool {
    let handled_elsewhere = on_disk
        .is_some_and(|t| t.handled && t.logon_token == hb.logon_token && t.hb_unbiased == hb.unbiased);
    app_running && current_hb.as_ref() == Some(hb) && !handled_elsewhere
}

fn read_heartbeat() -> Option<Heartbeat> {
    std::fs::read_to_string(heartbeat_path()).ok().and_then(|s| Heartbeat::parse(&s))
}

fn read_stale_tracker() -> Option<StaleTracker> {
    std::fs::read_to_string(stale_tracker_path()).ok().and_then(|s| StaleTracker::parse(&s))
}

/// Held while a run acts on a stale-heartbeat incident.
fn incident_lock_path() -> std::path::PathBuf {
    data_dir().join("heartbeat_incident.lock")
}

/// Acting on an incident takes about a minute at most (two 5 s probes, a
/// 15 s pause, a 5 s wait for the process to end, the relaunch and up to
/// `RELAUNCH_CONFIRM_TIMEOUT` waiting for the app); a lock older than this
/// was left by a run that died.
const INCIDENT_LOCK_STALE_SECS: u64 = 3 * 60;

// ============================================================================
// What the watchdog did itself: ended the app, and how often
// ============================================================================

/// How long a record of the watchdog ending the app keeps a following "not
/// running" from counting as a kill, if the app doesn't start again before
/// (its start clears the record, see `clear_watchdog_ended_record`).
const WATCHDOG_ENDED_MAX_AGE_SECS: i64 = 12 * 60 * 60;

/// Record that this watchdog ended the (unresponsive) app itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WatchdogEnded {
    /// Wall-clock time it was ended (Unix seconds).
    wall: i64,
    /// `session::logon_token` of the session it ran in (0 if unknown).
    logon_token: i64,
}

impl WatchdogEnded {
    fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split_whitespace();
        if parts.next()? != "1" {
            return None;
        }
        let ended = WatchdogEnded { wall: parts.next()?.parse().ok()?, logon_token: parts.next()?.parse().ok()? };
        parts.next().is_none().then_some(ended)
    }

    fn serialize(&self) -> String {
        format!("1 {} {}", self.wall, self.logon_token)
    }

    /// The record's age in seconds if it still explains the app being down
    /// in session `logon_token`: the same session (or the session can't be
    /// told right now) and not too old. A clock moved back counts the same
    /// as forward, so a correction can't turn the watchdog's own action into
    /// a tamper alert.
    fn applies(&self, now_wall: i64, logon_token: Option<i64>) -> Option<i64> {
        let age = now_wall.saturating_sub(self.wall);
        let same_session = logon_token.is_none_or(|t| t == self.logon_token);
        (same_session && age.saturating_abs() < WATCHDOG_ENDED_MAX_AGE_SECS).then_some(age)
    }
}

fn watchdog_ended_path() -> std::path::PathBuf {
    data_dir().join("watchdog_ended")
}

/// Forget that the watchdog ended the app: called by the app as it starts
/// (main.rs) and by the watchdog when it sees the app running, so the record
/// only ever covers the gap until the app is running again.
pub fn clear_watchdog_ended_record() {
    let _ = std::fs::remove_file(watchdog_ended_path());
}

/// At most this many restarts of a frozen app within
/// `HUNG_RESTART_WINDOW_SECS`; after that it's left alone (one alert).
const MAX_HUNG_RESTARTS: usize = 3;
const HUNG_RESTART_WINDOW_SECS: i64 = 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HungDecision {
    Restart,
    /// Too many restarts lately: leave it, and tell the admin.
    GiveUpAndAlert,
    /// Too many restarts lately, and the admin was already told.
    GiveUpQuietly,
}

/// When the watchdog recently restarted a frozen app (`hung_restarts`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RestartHistory {
    /// Wall-clock times of the restarts (Unix seconds).
    restarts: Vec<i64>,
    /// When the admin was last told the watchdog stopped restarting it.
    gave_up_alerted: Option<i64>,
}

/// Whether wall-clock time `t` is within the restart window of `now`, in
/// either direction (a clock moved back keeps them counting rather than
/// allowing more restarts).
fn within_restart_window(t: i64, now: i64) -> bool {
    now.saturating_sub(t).saturating_abs() < HUNG_RESTART_WINDOW_SECS
}

impl RestartHistory {
    /// Lines `r <time>` and `g <time>`; anything else is ignored.
    fn parse(s: &str) -> Self {
        let mut history = RestartHistory::default();
        for line in s.lines() {
            let mut parts = line.split_whitespace();
            let (Some(kind), Some(Ok(t)), None) = (parts.next(), parts.next().map(str::parse::<i64>), parts.next()) else {
                continue;
            };
            match kind {
                "r" => history.restarts.push(t),
                "g" => history.gave_up_alerted = Some(t),
                _ => {}
            }
        }
        history
    }

    fn serialize(&self) -> String {
        let mut out: String = self.restarts.iter().map(|t| format!("r {t}\n")).collect();
        if let Some(t) = self.gave_up_alerted {
            out.push_str(&format!("g {t}\n"));
        }
        out
    }

    fn decide(&self, now: i64) -> HungDecision {
        let recent = self.restarts.iter().filter(|&&t| within_restart_window(t, now)).count();
        if recent < MAX_HUNG_RESTARTS {
            HungDecision::Restart
        } else if self.gave_up_alerted.is_some_and(|t| within_restart_window(t, now)) {
            HungDecision::GiveUpQuietly
        } else {
            HungDecision::GiveUpAndAlert
        }
    }

    /// Note a restart at `now`, forgetting restarts outside the window.
    fn record_restart(&mut self, now: i64) {
        self.restarts.retain(|&t| within_restart_window(t, now));
        self.restarts.push(now);
    }
}

fn hung_restarts_path() -> std::path::PathBuf {
    data_dir().join("hung_restarts")
}

/// End process `pid` if it is a running copy of this same executable (and
/// not this watchdog process itself). Returns whether it was ended.
unsafe fn end_own_app_process(pid: u32) -> bool {
    if pid == 0 || pid == GetCurrentProcessId() {
        return false;
    }
    let Ok(own_exe) = std::env::current_exe() else { return false };
    let Ok(handle) = OpenProcess(
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
        false,
        pid,
    ) else {
        return false;
    };

    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32;
    let same_exe = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut len).is_ok()
        && String::from_utf16_lossy(&buffer[..len as usize])
            .eq_ignore_ascii_case(&own_exe.to_string_lossy());

    let ended = same_exe && TerminateProcess(handle, 1).is_ok();
    if ended {
        // The mutex is released once the process is gone.
        let _ = WaitForSingleObject(handle, 5000);
    }
    let _ = CloseHandle(handle);
    ended
}

// ============================================================================
// Alert queue: deliver, and retry what couldn't be delivered
// ============================================================================

/// At most this many undelivered alerts are kept (the oldest are dropped).
const MAX_PENDING_ALERTS: usize = 20;
/// Undelivered alerts older than this are dropped.
const MAX_PENDING_ALERT_AGE_SECS: i64 = 24 * 60 * 60;
/// An alert delivered later than this after it was raised says so.
const LATE_DELIVERY_NOTE_SECS: i64 = 5 * 60;
/// A flush lock older than this belongs to a run that died; take it over.
const FLUSH_LOCK_STALE_SECS: u64 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingAlert {
    created: i64,
    /// Local time it was raised, for the "delivered late" note.
    created_local: String,
    telegram: bool,
    discord: bool,
    text: String,
}

fn escape_field(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r")
}

fn unescape_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

impl PendingAlert {
    fn parse(line: &str) -> Option<Self> {
        let mut parts = line.split('\t');
        let alert = PendingAlert {
            created: parts.next()?.parse().ok()?,
            created_local: unescape_field(parts.next()?),
            telegram: parts.next()? == "1",
            discord: parts.next()? == "1",
            text: unescape_field(parts.next()?),
        };
        parts.next().is_none().then_some(alert)
    }

    fn serialize(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}",
            self.created,
            escape_field(&self.created_local),
            u8::from(self.telegram),
            u8::from(self.discord),
            escape_field(&self.text)
        )
    }

    /// Same alert as `other`, ignoring which channels are still pending.
    fn same_alert(&self, other: &PendingAlert) -> bool {
        self.created == other.created && self.text == other.text
    }

    fn text_for_delivery(&self, now: i64) -> String {
        if now - self.created > LATE_DELIVERY_NOTE_SECS {
            format!("{}\n🕒 {} {}", self.text, i18n::t("watchdog.alert.delayed"), self.created_local)
        } else {
            self.text.clone()
        }
    }
}

fn parse_queue(contents: &str) -> Vec<PendingAlert> {
    contents.lines().filter_map(PendingAlert::parse).collect()
}

/// Drop delivered, expired (or implausibly future-dated) alerts, and keep
/// only the newest `MAX_PENDING_ALERTS`.
fn prune_queue(mut alerts: Vec<PendingAlert>, now: i64) -> Vec<PendingAlert> {
    alerts.retain(|a| (a.telegram || a.discord) && (now - a.created).abs() <= MAX_PENDING_ALERT_AGE_SECS);
    if alerts.len() > MAX_PENDING_ALERTS {
        alerts.drain(..alerts.len() - MAX_PENDING_ALERTS);
    }
    alerts
}

fn serialize_queue(alerts: &[PendingAlert]) -> String {
    alerts.iter().map(|a| a.serialize() + "\n").collect()
}

fn queue_path() -> std::path::PathBuf {
    data_dir().join("pending_alerts")
}

fn local_time_string() -> String {
    let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute)
}

/// Add an alert for every bot that is enabled and configured right now; it
/// is sent by the next `flush_pending_alerts` (and retried until delivered).
/// Needs the shared database open (`database::init_shared_database`).
fn queue_alert(text: &str) {
    queue_alert_to(text, true, true);
}

/// `queue_alert` for only the bots asked for. Same queue line as any other
/// alert (one still pending for just one bot looks exactly like this), so
/// every watchdog version reads it.
fn queue_alert_to(text: &str, for_telegram: bool, for_discord: bool) {
    use std::io::Write;

    let tg = database::get_telegram_config();
    let dc = database::get_discord_config();
    let telegram = for_telegram && tg.enabled && tg.bot_token.is_some() && tg.admin_chat_id.is_some();
    let discord = for_discord && dc.enabled && dc.bot_token.is_some() && dc.channel_id.is_some();
    if !telegram && !discord {
        return;
    }
    let alert = PendingAlert {
        created: wall_now(),
        created_local: local_time_string(),
        telegram,
        discord,
        text: text.to_string(),
    };
    let _ = std::fs::create_dir_all(data_dir());
    // Not while a flush is rewriting the file, or the rewrite would drop
    // this alert. Appended anyway if the lock can't be had (better a rare
    // race than a lost alert).
    let _queue = queue_lock();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(queue_path()) {
        let _ = writeln!(f, "{}", alert.serialize());
    }
}

/// For the app itself (not the watchdog): queue an alert and try to send it
/// now on a background thread. Whatever can't be delivered is retried by the
/// watchdog's next runs. Needs the shared database open.
pub fn raise_alert(text: &str) {
    queue_alert(text);
    std::thread::spawn(flush_pending_alerts);
}

/// `raise_alert` with its own text for each bot (a Discord mention, or the
/// command syntax). The two texts must differ - the queue tells alerts
/// raised in the same second apart by their text - so equal texts go out as
/// one alert to both.
pub fn raise_alert_per_bot(telegram_text: &str, discord_text: &str) {
    if telegram_text == discord_text {
        queue_alert(telegram_text);
    } else {
        queue_alert_to(telegram_text, true, false);
        queue_alert_to(discord_text, false, true);
    }
    std::thread::spawn(flush_pending_alerts);
}

/// A lock shared between processes (watchdog runs and the app): a file
/// created exclusively, removed when dropped. One left behind by a process
/// that died is taken over once it is `stale_after_secs` old.
struct FileLock(std::path::PathBuf);

impl FileLock {
    fn try_acquire(path: std::path::PathBuf, stale_after_secs: u64) -> Option<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        for _ in 0..2 {
            if std::fs::OpenOptions::new().write(true).create_new(true).open(&path).is_ok() {
                return Some(FileLock(path));
            }
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if !lock_is_stale(modified, std::time::SystemTime::now(), stale_after_secs) {
                return None;
            }
            let _ = std::fs::remove_file(&path);
        }
        None
    }

    /// `try_acquire`, retried for up to `wait` - for locks only ever held
    /// for a moment.
    fn acquire_waiting(path: std::path::PathBuf, stale_after_secs: u64, wait: std::time::Duration) -> Option<Self> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            if let Some(lock) = Self::try_acquire(path.clone(), stale_after_secs) {
                return Some(lock);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// How far in the future a lock file's time may be and still count as held
/// (file time resolution, not clock changes).
const LOCK_FUTURE_TOLERANCE_SECS: u64 = 5;

/// Whether a lock file last modified at `modified` was left behind. An
/// unreadable time, or one from the future (the clock was set back after a
/// run was cut off holding it), counts as stale: otherwise such a lock
/// would block everything behind it until the clock caught up.
fn lock_is_stale(modified: Option<std::time::SystemTime>, now: std::time::SystemTime, stale_after_secs: u64) -> bool {
    let Some(modified) = modified else { return true };
    match now.duration_since(modified) {
        Ok(age) => age.as_secs() >= stale_after_secs,
        Err(ahead) => ahead.duration().as_secs() > LOCK_FUTURE_TOLERANCE_SECS,
    }
}

/// Exclusive right to send from the queue, so two runs (or the app and a
/// run) never deliver the same alert twice. Held for the whole flush.
fn flush_lock_path() -> std::path::PathBuf {
    data_dir().join("pending_alerts.lock")
}

/// Held only while the queue file is read, appended to or rewritten, so an
/// alert queued during a flush isn't lost when the flush rewrites the file.
fn queue_lock() -> Option<FileLock> {
    FileLock::acquire_waiting(data_dir().join("pending_alerts.write.lock"), QUEUE_LOCK_STALE_SECS, QUEUE_LOCK_WAIT)
}
const QUEUE_LOCK_STALE_SECS: u64 = 30;
const QUEUE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The queue after a flush: what is in the file now (`latest`, which may
/// have gained alerts while sending), with each alert this flush sent from
/// (`snapshot`) replaced by its outcome (`updated`) - or dropped if it has
/// left the file meanwhile.
fn merge_after_send(latest: Vec<PendingAlert>, snapshot: &[PendingAlert], updated: &[PendingAlert], now: i64) -> Vec<PendingAlert> {
    let merged = latest
        .into_iter()
        .filter_map(|a| match updated.iter().find(|u| u.same_alert(&a)) {
            Some(u) => Some(u.clone()),
            None if snapshot.iter().any(|s| s.same_alert(&a)) => None,
            None => Some(a),
        })
        .collect();
    prune_queue(merged, now)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendOutcome {
    Sent,
    /// Worth trying again later (offline, timeout, rate limit, server error).
    Retry,
    /// Refused for good (bad token/chat) - retrying won't help.
    Rejected,
}

/// Send every queued alert that is still pending. Cheap when the queue is
/// empty. Stops trying a bot for this run after its first failure (most
/// likely offline), leaving the rest for the next run.
pub fn flush_pending_alerts() {
    let path = queue_path();
    // Cheap check first, without any lock.
    if std::fs::read_to_string(&path).map_or(true, |c| c.trim().is_empty()) {
        return;
    }
    let Some(_flush) = FileLock::try_acquire(flush_lock_path(), FLUSH_LOCK_STALE_SECS) else { return };
    // Read again now that this run alone sends: another flush may have
    // delivered and rewritten the queue since the check above.
    let contents = {
        let _queue = queue_lock();
        std::fs::read_to_string(&path).unwrap_or_default()
    };
    if contents.trim().is_empty() {
        return;
    }

    ensure_shared_database();
    let now = wall_now();
    let snapshot = prune_queue(parse_queue(&contents), now);

    let tg = database::get_telegram_config();
    let dc = database::get_discord_config();
    let tg_target = tg.bot_token.zip(tg.admin_chat_id).filter(|_| tg.enabled);
    let dc_admin = dc.admin_user_id;
    let dc_target = dc.bot_token.zip(dc.channel_id).filter(|_| dc.enabled);
    let mut tg_up = true;
    let mut dc_up = true;

    let mut updated = Vec::with_capacity(snapshot.len());
    for mut alert in snapshot.iter().cloned() {
        let text = alert.text_for_delivery(now);
        if alert.telegram {
            match &tg_target {
                None => alert.telegram = false,
                Some(_) if !tg_up => {}
                Some((token, chat_id)) => match send_telegram_alert(token, *chat_id, &text) {
                    SendOutcome::Sent | SendOutcome::Rejected => alert.telegram = false,
                    SendOutcome::Retry => tg_up = false,
                },
            }
        }
        if alert.discord {
            match &dc_target {
                None => alert.discord = false,
                Some(_) if !dc_up => {}
                Some((token, channel_id)) => match send_discord_alert(token, *channel_id, dc_admin, &text) {
                    SendOutcome::Sent | SendOutcome::Rejected => alert.discord = false,
                    SendOutcome::Retry => dc_up = false,
                },
            }
        }
        updated.push(alert);
    }

    // Merge with anything queued while we were sending; appends wait for
    // this rewrite (see queue_alert).
    {
        let _queue = queue_lock();
        let latest = std::fs::read_to_string(&path).map(|s| parse_queue(&s)).unwrap_or_default();
        let merged = merge_after_send(latest, &snapshot, &updated, now);
        if merged.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            let _ = std::fs::write(&path, serialize_queue(&merged));
        }
    }

    // Logged on change only, in its own state file so it doesn't disturb
    // the app-state logging above.
    let delivery_path = data_dir().join("watchdog.delivery");
    let previous = std::fs::read_to_string(&delivery_path).unwrap_or_default();
    let current = if tg_up && dc_up { "ok" } else { "failing" };
    if previous.trim() != current {
        if current == "failing" {
            log("could not deliver an alert - will retry on the next runs");
        }
        let _ = std::fs::write(&delivery_path, current);
    }
}

/// Open the shared database (bot configuration) unless it already is - the
/// app has it open; a watchdog run opens it only when it needs it.
fn ensure_shared_database() {
    if !database::shared_database_is_open() {
        database::init_shared_database();
    }
}

/// One-off Telegram message via plain HTTP - no bot/dispatcher needed for a
/// single send, same approach as the setup wizard's test message.
fn send_telegram_alert(token: &str, chat_id: i64, text: &str) -> SendOutcome {
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage?chat_id={}&text={}",
        token,
        chat_id,
        urlencoding::encode(text)
    );
    match ureq::get(&url).timeout(std::time::Duration::from_secs(10)).call() {
        Ok(_) => SendOutcome::Sent,
        Err(ureq::Error::Status(code, _)) if (400..500).contains(&code) && code != 429 => SendOutcome::Rejected,
        Err(_) => SendOutcome::Retry,
    }
}

/// One-off Discord message via the REST API - doesn't need the Gateway
/// connection the main app's bot uses, same approach as the setup wizard.
fn send_discord_alert(token: &str, channel_id: u64, admin_user_id: Option<u64>, text: &str) -> SendOutcome {
    // `ChannelId::new` panics on 0 (and this build aborts on a panic).
    let Some(channel_id) = std::num::NonZeroU64::new(channel_id) else {
        return SendOutcome::Rejected;
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return SendOutcome::Retry;
    };
    rt.block_on(async {
        let http = serenity::http::Http::new(token);
        let message = serenity::builder::CreateMessage::new()
            .content(text)
            .allowed_mentions(discord_allowed_mentions(admin_user_id));
        let send = serenity::model::id::ChannelId::from(channel_id).send_message(&http, message);
        match tokio::time::timeout(std::time::Duration::from_secs(15), send).await {
            Ok(Ok(_)) => SendOutcome::Sent,
            _ => SendOutcome::Retry,
        }
    })
}

/// Who an alert may ping on Discord: only the configured admin user, when
/// the text tags them (`<@id>`, the update reminder). Without this Discord
/// would turn any user, role, @everyone or @here mention in the text into a
/// ping; with it, nothing else in an alert can ping anyone.
fn discord_allowed_mentions(admin_user_id: Option<u64>) -> serenity::builder::CreateAllowedMentions {
    // `UserId::new` panics on 0, and parse_discord_id never yields 0 anyway.
    let admin = admin_user_id.and_then(std::num::NonZeroU64::new).map(serenity::model::id::UserId::from);
    serenity::builder::CreateAllowedMentions::new().users(admin)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = TICKS_PER_SEC;

    fn hb(token: i64, unbiased: u64) -> Heartbeat {
        Heartbeat { logon_token: token, unbiased, pid: 42 }
    }

    #[test]
    fn heartbeat_round_trips() {
        let h = hb(-133_000_000_000, 987_654_321);
        assert_eq!(Heartbeat::parse(&h.serialize()), Some(h));
        assert_eq!(Heartbeat::parse("2 1 2 3"), None);
        assert_eq!(Heartbeat::parse("1 1 2"), None);
        assert_eq!(Heartbeat::parse(""), None);
    }

    #[test]
    fn heartbeat_staleness() {
        let token = 77;
        let now = 10_000 * S;
        assert_eq!(heartbeat_status(None, token, Some(now)), HeartbeatStatus::Unknown);
        assert_eq!(heartbeat_status(Some(&hb(token, now)), token, None), HeartbeatStatus::Unknown);
        // Another session (or a previous boot's session).
        assert_eq!(heartbeat_status(Some(&hb(78, now - 3600 * S)), token, Some(now)), HeartbeatStatus::Unknown);
        assert_eq!(heartbeat_status(Some(&hb(0, now - 3600 * S)), 0, Some(now)), HeartbeatStatus::Unknown);
        // From after "now" (counter reset by a reboot).
        assert_eq!(heartbeat_status(Some(&hb(token, now + S)), token, Some(now)), HeartbeatStatus::Unknown);
        assert_eq!(heartbeat_status(Some(&hb(token, now - 299 * S)), token, Some(now)), HeartbeatStatus::Fresh);
        assert_eq!(
            heartbeat_status(Some(&hb(token, now - 300 * S)), token, Some(now)),
            HeartbeatStatus::Stale { age_secs: 300 }
        );
    }

    #[test]
    fn stale_heartbeat_needs_three_regular_runs() {
        let h = hb(5, 1_000 * S);
        let mut wall = 1_700_000_000;
        let mut now = 1_400 * S;
        let mut t = StaleTracker::observe(None, &h, now, wall);
        assert!(!t.confirmed());
        for _ in 0..2 {
            now += 60 * S;
            wall += 60;
            t = StaleTracker::observe(Some(t), &h, now, wall);
        }
        assert_eq!(t.runs, 3);
        assert!(t.confirmed());
    }

    #[test]
    fn sleep_between_runs_starts_the_count_over() {
        let h = hb(5, 1_000 * S);
        let wall = 1_700_000_000;
        let now = 1_400 * S;
        let t = StaleTracker::observe(None, &h, now, wall);
        let t = StaleTracker::observe(Some(t), &h, now + 60 * S, wall + 60);
        // Slept for an hour: awake time moved a minute, the wall clock an hour.
        let t = StaleTracker::observe(Some(t), &h, now + 120 * S, wall + 3720);
        assert_eq!(t.runs, 1);
        assert!(!t.confirmed());
    }

    #[test]
    fn long_gap_or_new_heartbeat_starts_over() {
        let h = hb(5, 1_000 * S);
        let t = StaleTracker::observe(None, &h, 1_400 * S, 0);
        let gap = StaleTracker::observe(Some(t), &h, 1_400 * S + 240 * S, 240);
        assert_eq!(gap.runs, 1);
        let newer = StaleTracker::observe(Some(t), &hb(5, 1_100 * S), 1_460 * S, 60);
        assert_eq!(newer.runs, 1);
    }

    #[test]
    fn handled_incident_stays_handled() {
        let h = hb(5, 1_000 * S);
        let mut t = StaleTracker::observe(None, &h, 1_400 * S, 0);
        t.handled = true;
        // Even after a long gap, the same heartbeat doesn't make a new incident.
        let later = StaleTracker::observe(Some(t), &h, 9_000 * S, 99_999);
        assert!(later.handled);
        assert_eq!(StaleTracker::parse(&later.serialize()), Some(later));
    }

    #[test]
    fn stuck_app_is_acted_on_only_while_someone_uses_the_pc() {
        // Suspended, then the PC used: input long after the last heartbeat.
        assert!(someone_using_session(true, Some(false), Some(5), 400));
        assert!(someone_using_session(true, None, Some(60), 400), "lock state unknown");
        // Display off / Modern Standby / walked away: last input before the
        // last heartbeat.
        assert!(!someone_using_session(true, Some(false), Some(900), 400));
        assert!(!someone_using_session(true, Some(false), Some(395), 400), "within the margin");
        // Locked, or switched away under Fast User Switching.
        assert!(!someone_using_session(true, Some(true), Some(5), 400));
        assert!(!someone_using_session(false, Some(false), Some(5), 400));
        // Input time unreadable: do nothing.
        assert!(!someone_using_session(true, Some(false), None, 400));
    }

    #[test]
    fn three_runs_too_close_together_are_not_enough() {
        let h = hb(5, 1_000 * S);
        let t = StaleTracker::observe(None, &h, 1_400 * S, 0);
        let t = StaleTracker::observe(Some(t), &h, 1_420 * S, 20);
        let t = StaleTracker::observe(Some(t), &h, 1_440 * S, 40);
        assert_eq!(t.runs, 3);
        assert!(!t.confirmed());
    }

    fn alert(created: i64, text: &str) -> PendingAlert {
        PendingAlert { created, created_local: "2026-11-15 10:00".into(), telegram: true, discord: false, text: text.into() }
    }

    #[test]
    fn alert_queue_round_trips_awkward_text() {
        let a = PendingAlert { discord: true, ..alert(1_700_000_000, "line 1\nline\t2 \\ end") };
        let parsed = parse_queue(&serialize_queue(&[a.clone(), alert(5, "x")]));
        assert_eq!(parsed, vec![a, alert(5, "x")]);
        assert!(parse_queue("garbage\nmore\tgarbage\n").is_empty());
    }

    #[test]
    fn alert_queue_is_bounded_by_age_and_count() {
        let now = 1_700_000_000;
        let delivered = PendingAlert { telegram: false, ..alert(now, "done") };
        let q = vec![
            alert(now - MAX_PENDING_ALERT_AGE_SECS - 1, "too old"),
            alert(now + MAX_PENDING_ALERT_AGE_SECS + 1, "from the future"),
            delivered,
            alert(now - 10, "keep"),
        ];
        let kept = prune_queue(q, now);
        assert_eq!(kept, vec![alert(now - 10, "keep")]);

        let many: Vec<PendingAlert> = (0..30).map(|i| alert(now - 100 + i, &format!("a{i}"))).collect();
        let kept = prune_queue(many, now);
        assert_eq!(kept.len(), MAX_PENDING_ALERTS);
        assert_eq!(kept[0].text, "a10");
        assert_eq!(kept.last().unwrap().text, "a29");
    }

    #[test]
    fn late_delivery_says_when_it_happened() {
        let a = alert(1_000, "boom");
        assert_eq!(a.text_for_delivery(1_000 + 60), "boom");
        let late = a.text_for_delivery(1_000 + 3600);
        assert!(late.starts_with("boom\n🕒 "));
        assert!(late.ends_with("2026-11-15 10:00"));
    }

    #[test]
    fn lock_from_the_future_or_unreadable_is_stale() {
        use std::time::{Duration, SystemTime};
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let secs = Duration::from_secs;
        assert!(!lock_is_stale(Some(now - secs(10)), now, 300), "held by a live run");
        assert!(lock_is_stale(Some(now - secs(300)), now, 300), "left by a run that died");
        // Clock set back an hour after a run was cut off holding it.
        assert!(lock_is_stale(Some(now + secs(3600)), now, 300));
        assert!(!lock_is_stale(Some(now + secs(2)), now, 300), "file time resolution");
        assert!(lock_is_stale(None, now, 300));
    }

    #[test]
    fn file_lock_is_exclusive_and_released_on_drop() {
        let dir = std::env::temp_dir().join(format!("stm-watchdog-lock-test-{}", std::process::id()));
        let path = dir.join("test.lock");
        let _ = std::fs::remove_file(&path);
        let first = FileLock::try_acquire(path.clone(), 300).expect("free lock");
        assert!(FileLock::try_acquire(path.clone(), 300).is_none(), "already held");
        drop(first);
        assert!(FileLock::try_acquire(path.clone(), 300).is_some(), "released on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn alert_queued_during_a_flush_survives_the_rewrite() {
        let now = 1_700_000_000;
        let snapshot = vec![alert(now - 60, "sent"), alert(now - 30, "offline")];
        let updated = vec![
            PendingAlert { telegram: false, ..alert(now - 60, "sent") },
            alert(now - 30, "offline"),
        ];
        // Re-read after sending: the snapshot plus one appended meanwhile.
        let latest = vec![alert(now - 60, "sent"), alert(now - 30, "offline"), alert(now - 1, "new")];
        let merged = merge_after_send(latest, &snapshot, &updated, now);
        assert_eq!(merged, vec![alert(now - 30, "offline"), alert(now - 1, "new")]);
        // Something another flush already removed stays removed.
        let merged = merge_after_send(vec![alert(now - 1, "new")], &snapshot, &updated, now);
        assert_eq!(merged, vec![alert(now - 1, "new")]);
    }

    /// `raise_alert_per_bot`'s two queue lines: same second, one bot each,
    /// in the format every watchdog version reads - and delivering one
    /// leaves the other pending.
    #[test]
    fn per_bot_alerts_are_ordinary_queue_lines_kept_apart() {
        let now = 1_700_000_000;
        let tg = alert(now, "/update");
        let dc = PendingAlert { telegram: false, discord: true, ..alert(now, "<@42> !update") };
        let line = dc.serialize();
        assert_eq!(line, format!("{now}\t2026-11-15 10:00\t0\t1\t<@42> !update"));
        assert_eq!(parse_queue(&serialize_queue(&[tg.clone(), dc.clone()])), vec![tg.clone(), dc.clone()]);

        let snapshot = vec![tg.clone(), dc.clone()];
        let updated = vec![PendingAlert { telegram: false, ..tg.clone() }, dc.clone()];
        let merged = merge_after_send(snapshot.clone(), &snapshot, &updated, now);
        assert_eq!(merged, vec![dc], "Telegram's copy delivered, Discord's still pending");
    }

    #[test]
    fn discord_alerts_may_ping_only_the_admin() {
        let json = |admin| serde_json::to_value(discord_allowed_mentions(admin)).unwrap();
        // An empty `parse` list: no @everyone/@here, roles or other users,
        // whatever the text says. Only the listed user can be pinged.
        assert_eq!(json(Some(42)), serde_json::json!({"parse": [], "users": ["42"], "roles": []}));
        assert_eq!(json(None), serde_json::json!({"parse": [], "users": [], "roles": []}));
        assert_eq!(json(Some(0)), json(None), "0 isn't a user (and would panic)");
    }

    #[test]
    fn watchdog_ended_record_covers_its_own_session_for_a_while() {
        let rec = WatchdogEnded { wall: 1_700_000_000, logon_token: 77 };
        assert_eq!(WatchdogEnded::parse(&rec.serialize()), Some(rec));
        assert_eq!(WatchdogEnded::parse("1 5"), None);
        assert_eq!(rec.applies(1_700_000_060, Some(77)), Some(60));
        assert_eq!(rec.applies(1_700_000_060, Some(78)), None, "another session");
        assert_eq!(rec.applies(1_700_000_060, None), Some(60), "session unknown right now");
        assert_eq!(rec.applies(1_700_000_000 + WATCHDOG_ENDED_MAX_AGE_SECS, Some(77)), None, "too old");
        // Clock corrected backwards: still the watchdog's own doing.
        assert_eq!(rec.applies(1_700_000_000 - 3600, Some(77)), Some(-3600));
    }

    #[test]
    fn frozen_app_is_restarted_a_few_times_an_hour_then_left_with_one_alert() {
        let t0 = 1_700_000_000;
        let mut h = RestartHistory::default();
        for i in 0..MAX_HUNG_RESTARTS as i64 {
            assert_eq!(h.decide(t0 + i * 480), HungDecision::Restart);
            h.record_restart(t0 + i * 480);
        }
        let later = t0 + 1500;
        assert_eq!(h.decide(later), HungDecision::GiveUpAndAlert);
        h.gave_up_alerted = Some(later);
        assert_eq!(h.decide(later + 600), HungDecision::GiveUpQuietly);
        // Once the restarts are over an hour old, restarting resumes.
        assert_eq!(h.decide(t0 + 2 * 480 + HUNG_RESTART_WINDOW_SECS), HungDecision::Restart);
        // A clock set back doesn't buy more restarts.
        assert_ne!(h.decide(t0 - 600), HungDecision::Restart);
        let parsed = RestartHistory::parse(&h.serialize());
        assert_eq!(parsed, h);
        assert_eq!(RestartHistory::parse("garbage\nr x\nr 5 6\n"), RestartHistory::default());
    }

    #[test]
    fn old_restarts_are_forgotten() {
        let mut h = RestartHistory { restarts: vec![1_000, 2_000], gave_up_alerted: None };
        h.record_restart(1_000 + HUNG_RESTART_WINDOW_SECS + 10);
        assert_eq!(h.restarts, vec![2_000, 1_000 + HUNG_RESTART_WINDOW_SECS + 10]);
    }

    #[test]
    fn relaunch_counts_only_when_the_app_was_seen_running() {
        let started = || Ok("started the task in session 3".to_string());
        let r = relaunch_with(started, || Some(std::time::Duration::from_millis(1500)));
        assert_eq!(r.outcome, RelaunchOutcome::Running);
        assert_eq!(r.outcome.word(), "true", "what smoke-test.ps1 looks for");
        assert_eq!(r.detail, "started the task in session 3; the app was running 1.5s later");

        // Task Scheduler took the request, but the app wasn't up yet (slow,
        // or never: the 1.0.1 field case and a disabled task look like this).
        // Not "failed": later runs wait for it (PendingRelaunch).
        let r = relaunch_with(started, || None);
        assert_eq!(r.outcome, RelaunchOutcome::Requested);
        assert_eq!(r.outcome.word(), "pending");
        assert!(r.detail.contains("was not running in this session yet 30s later"), "{}", r.detail);

        // The task couldn't be started: no point waiting.
        let r = relaunch_with(|| Err("Access is denied.".to_string()), || panic!("waited for an app nobody started"));
        assert_eq!(r.outcome, RelaunchOutcome::Failed);
        assert_eq!(r.detail, "could not start the ScreenTimeManager task: Access is denied.");
    }

    #[test]
    fn a_slow_relaunch_is_waited_for_not_repeated_or_reported() {
        let token = 77;
        let asked = PendingRelaunch { logon_token: token, unbiased: 1_000 * S, wall: 1_700_000_000 };
        assert_eq!(PendingRelaunch::parse(&asked.serialize()), Some(asked));
        assert_eq!(PendingRelaunch::parse("1 2 3"), None);

        // The next minute's run: still starting.
        assert_eq!(asked.state(Some(token), Some(1_060 * S), None, None), PendingState::Waiting { age_secs: 60 });
        // An old heartbeat (from before the kill) doesn't count as coming up.
        let old_hb = Heartbeat { logon_token: token, unbiased: 900 * S, pid: 1 };
        assert_eq!(asked.state(Some(token), Some(1_060 * S), Some(&old_hb), Some(1_699_999_000)), PendingState::Waiting { age_secs: 60 });
        // It came up (heartbeat or recorded start since) and has stopped again.
        let new_hb = Heartbeat { unbiased: 1_045 * S, ..old_hb };
        assert_eq!(asked.state(Some(token), Some(1_060 * S), Some(&new_hb), None), PendingState::CameUp);
        assert_eq!(asked.state(Some(token), Some(1_060 * S), None, Some(1_700_000_040)), PendingState::CameUp);
        // Never came up: tried again (and reported) after the window.
        assert_eq!(
            asked.state(Some(token), Some(1_000 * S + RELAUNCH_PENDING_MAX_SECS * S), None, None),
            PendingState::Expired { age_secs: RELAUNCH_PENDING_MAX_SECS }
        );
        // Another session, or a reboot since (awake time went backwards).
        assert_eq!(asked.state(Some(78), Some(1_060 * S), None, None), PendingState::NotThisSession);
        assert_eq!(asked.state(Some(token), Some(10 * S), None, None), PendingState::NotThisSession);
        assert_eq!(asked.state(None, Some(1_060 * S), None, None), PendingState::NotThisSession);
    }

    #[test]
    fn run_requests_start_with_the_ones_a_standard_user_may_make() {
        let requests = run_requests(3, Some("S-1-5-21-1-2-3-1001"));
        let labels: Vec<&str> = requests.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, [
            "RunEx(AS_SELF|USE_SESSION_ID, session 3)",
            "RunEx(USER_SID|USE_SESSION_ID, S-1-5-21-1-2-3-1001, session 3)",
            "RunEx(USE_SESSION_ID, session 3)",
        ]);
        // TASK_RUN_AS_SELF = 1, TASK_RUN_USE_SESSION_ID = 4, TASK_RUN_USER_SID = 8 (taskschd.h).
        assert_eq!(requests.iter().map(|r| r.flags).collect::<Vec<_>>(), [5, 12, 4]);
        assert!(requests.iter().all(|r| r.session == 3));
        assert_eq!(requests[0].user, None, "AS_SELF: the user argument is ignored, pass none");
        assert_eq!(requests[1].user.as_deref(), Some("S-1-5-21-1-2-3-1001"));
        // Own SID unknown: the SID request is left out.
        assert_eq!(run_requests(3, None).len(), 2);
    }

    #[test]
    fn install_marker_age_does_not_trust_the_wall_clock() {
        let stamp = InstallStamp::parse("2026/11/16 10:00:00 boot=104 unbiased=5000000000\r\n");
        assert_eq!(stamp, Some(InstallStamp { boot_id: 104, unbiased: 500 * S }));
        assert_eq!(InstallStamp::parse("2026/11/16 10:00:00\r\n"), None, "an older setup's marker");

        // Same boot, 60 s of awake time later: 60 s old, whatever the wall
        // clock says (here it was set forward an hour right after the install).
        assert_eq!(install_marker_age(stamp, Some(104), Some(560 * S), Some(3660)), Some(60));
        // ...or back an hour (the marker's file time is in the future).
        assert_eq!(install_marker_age(stamp, Some(104), Some(560 * S), None), Some(60));
        // Written in an earlier boot: not a pending install, whatever its file time.
        assert_eq!(install_marker_age(stamp, Some(105), Some(560 * S), Some(30)), None);
        // No stamp, or the boot can't be read: the file time, as before.
        assert_eq!(install_marker_age(None, Some(104), Some(560 * S), Some(42)), Some(42));
        assert_eq!(install_marker_age(stamp, None, Some(560 * S), Some(42)), Some(42));
        assert_eq!(install_marker_age(None, Some(104), Some(560 * S), None), None, "future file time, no stamp: stale");

        // Started since the install: a heartbeat later in the same boot.
        let hb = |unbiased| Heartbeat { logon_token: 1, unbiased, pid: 2 };
        assert!(started_since_install(stamp, Some(104), Some(600 * S), Some(&hb(520 * S))));
        assert!(!started_since_install(stamp, Some(104), Some(600 * S), Some(&hb(400 * S))), "from before the install");
        assert!(!started_since_install(stamp, Some(105), Some(600 * S), Some(&hb(520 * S))), "another boot");
        assert!(!started_since_install(None, Some(104), Some(600 * S), Some(&hb(520 * S))));
    }

    #[test]
    fn own_sid_is_read() {
        let sid = own_user_sid().expect("own SID");
        assert!(sid.starts_with("S-1-5-"), "{sid}");
    }

    #[test]
    fn boot_id_is_readable() {
        // What setup writes into the marker; readable without elevation.
        assert!(current_boot_id().is_some());
    }

    #[test]
    fn waiting_for_the_app_is_bounded() {
        use std::time::Duration;
        let mut calls = 0;
        let found = wait_until(Duration::from_secs(5), Duration::from_millis(1), || {
            calls += 1;
            calls == 3
        });
        assert!(found.is_some());
        assert_eq!(calls, 3);

        let mut calls = 0;
        let started = std::time::Instant::now();
        let found = wait_until(Duration::from_millis(60), Duration::from_millis(10), || {
            calls += 1;
            false
        });
        assert_eq!(found, None);
        assert!(started.elapsed() >= Duration::from_millis(60));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(calls >= 2, "checked again at the deadline");
    }

    #[test]
    fn mutex_check_only_looks_and_never_creates() {
        use windows::Win32::System::Threading::CreateMutexW;
        let name = format!("ScreenTimeManager_WatchdogTest_{}", std::process::id());
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            assert!(!named_mutex_exists(&name));
            // Looking must not have left one behind for the app to trip on.
            assert!(!named_mutex_exists(&name));
            let held = CreateMutexW(None, BOOL::from(true), PCWSTR(wide.as_ptr())).expect("create test mutex");
            assert!(named_mutex_exists(&name));
            let _ = CloseHandle(held);
            assert!(!named_mutex_exists(&name));
        }
    }

    #[test]
    fn task_scheduler_lookup_reports_a_missing_task() {
        // Read-only: looks a task up, never runs or changes one.
        let missing = task_scheduler::with_task("ScreenTimeManager-no-such-task-for-tests", |_| Ok(()));
        let err = missing.expect_err("found a task that doesn't exist");
        // ERROR_FILE_NOT_FOUND as an HRESULT.
        assert!(err.contains("0x80070002"), "{err}");
    }

    #[test]
    fn incident_is_acted_on_only_if_it_still_stands_under_the_lock() {
        let h = hb(5, 1_000 * S);
        let mut t = StaleTracker::observe(None, &h, 1_400 * S, 0);
        assert!(incident_still_stands(Some(t), Some(h), &h, true));
        assert!(incident_still_stands(None, Some(h), &h, true));
        // Another run handled it while this one waited.
        t.handled = true;
        assert!(!incident_still_stands(Some(t), Some(h), &h, true));
        // ...but a handled earlier incident doesn't block this one.
        let other = StaleTracker { hb_unbiased: 900 * S, ..t };
        assert!(incident_still_stands(Some(other), Some(h), &h, true));
        // The app was restarted meanwhile (new heartbeat), the heartbeat
        // can't be read, or the mutex is free (ended, not yet relaunched).
        assert!(!incident_still_stands(None, Some(hb(5, 1_500 * S)), &h, true));
        assert!(!incident_still_stands(None, None, &h, true));
        assert!(!incident_still_stands(None, Some(h), &h, false));
    }
}
