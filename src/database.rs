//! Database module for Screen Time Manager
//! Handles SQLite database initialization and settings management

use std::path::PathBuf;
use std::sync::Mutex;
use rusqlite::{Connection, params};
use windows::core::PCWSTR;

/// Global database connection (thread-safe)
pub static DB_CONNECTION: Mutex<Option<Connection>> = Mutex::new(None);

/// Shared machine-wide database connection, used for settings that should be
/// the same no matter which Windows account is running the app (currently
/// just the Telegram/Discord bot config). `None` if the shared location
/// couldn't be created/opened (e.g. missing permissions) - callers fall back
/// to the per-user database in that case.
pub static SHARED_DB_CONNECTION: Mutex<Option<Connection>> = Mutex::new(None);

/// Weekday keys for database
pub const WEEKDAY_KEYS: [&str; 7] = [
    "limit_monday", "limit_tuesday", "limit_wednesday", "limit_thursday",
    "limit_friday", "limit_saturday", "limit_sunday"
];

/// Get the path to the database file in a hidden location
pub fn get_database_path() -> PathBuf {
    let data_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".screen-time-manager");

    if !data_dir.exists() {
        let _ = std::fs::create_dir_all(&data_dir);

        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let path: Vec<u16> = data_dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
            unsafe {
                let _ = windows::Win32::Storage::FileSystem::SetFileAttributesW(
                    PCWSTR(path.as_ptr()),
                    windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN,
                );
            }
        }
    }

    data_dir.join("data.db")
}

/// Initialize the SQLite database
pub fn init_database() -> Result<(), Box<dyn std::error::Error>> {
    let db_path = get_database_path();
    let conn = Connection::open(&db_path)?;
    install_settings_connection(conn)
}

/// Create the settings table and default rows on `conn`, then make it the
/// per-account connection. Split out of `init_database` so the startup
/// recovery (see `init_database_with_recovery`) can fall back to an
/// in-memory database with the same defaults.
fn install_settings_connection(conn: Connection) -> Result<(), Box<dyn std::error::Error>> {
    prepare_settings_connection(&conn)?;
    *DB_CONNECTION.lock().unwrap() = Some(conn);
    Ok(())
}

fn prepare_settings_connection(conn: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
        [],
    )?;

    // Default settings to initialize
    let defaults = [
        ("passcode", "0000"),
        // Daily limits in minutes (default 120 = 2 hours)
        ("limit_monday", "120"),
        ("limit_tuesday", "120"),
        ("limit_wednesday", "120"),
        ("limit_thursday", "120"),
        ("limit_friday", "120"),
        ("limit_saturday", "120"),
        ("limit_sunday", "120"),
        // First warning (minutes before limit)
        ("warning1_minutes", "10"),
        ("warning1_message", "10 minutes remaining!"),
        // Second warning (minutes before limit)
        ("warning2_minutes", "5"),
        ("warning2_message", "5 minutes remaining!"),
        // Blocking message
        ("blocking_message", "Your screen time limit has been reached."),
        // Pause mode settings
        ("pause_enabled", "1"),              // 1 = enabled, 0 = disabled
        ("pause_daily_budget", "45"),        // Total pause minutes per day
        ("pause_max_duration", "20"),        // Max minutes per single pause
        ("pause_cooldown", "15"),            // Minutes between pauses
        ("pause_min_active_time", "10"),     // Min minutes before first pause allowed
        // Lock screen timeout (seconds before shutdown, default 10 minutes)
        ("lock_screen_timeout", "600"),
        // Idle detection settings
        // Off unless a parent turns it on: "no keyboard/mouse input" also
        // describes controller gaming and watching videos, which would
        // otherwise stop the clock. Only written when the key is missing, so
        // an install that already has a stored value keeps it.
        ("idle_enabled", "0"),              // 1 = enabled, 0 = disabled
        ("idle_timeout_minutes", "5"),      // Minutes of inactivity before auto-pause
    ];

    for (key, value) in defaults {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM settings WHERE key = ?1)",
            params![key],
            |row| row.get(0),
        )?;

        if !exists {
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
        }
    }

    Ok(())
}

/// Get the path to the shared machine-wide database, under %ProgramData%.
/// Not tied to any Windows account, unlike `get_database_path()`.
fn get_shared_database_path() -> Option<PathBuf> {
    let program_data = std::env::var_os("ProgramData")?;
    Some(PathBuf::from(program_data).join("ScreenTimeManager").join("bot_config.db"))
}

/// Initialize the shared machine-wide database used for bot configuration.
/// Safe to fail (e.g. the current account lacks permission to create the
/// folder/file) - callers of `get_shared_setting`/`set_shared_setting` fall
/// back to the per-user database when this hasn't succeeded.
pub fn init_shared_database() {
    let Some(db_path) = get_shared_database_path() else { return };

    if let Some(dir) = db_path.parent() {
        if !dir.exists() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("[Database] Could not create shared config directory: {e}");
                return;
            }
        }
    }

    let conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[Database] Could not open shared config database: {e}");
            return;
        }
    };

    if let Err(e) = conn.execute(
        "CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
        [],
    ) {
        eprintln!("[Database] Could not initialize shared config database: {e}");
        return;
    }

    *SHARED_DB_CONNECTION.lock().unwrap() = Some(conn);

    // One-time migration: carry over bot settings and the passcode/rotating
    // PIN from the old per-user storage so existing setups (already
    // configured before these became shared) don't have to be redone. Only
    // the first account to launch after this migrates its local value in -
    // every other account then defers to that same shared value, which is
    // the point: one passcode/PIN that works no matter which Windows
    // account is logged in, instead of each account's own separate one.
    const MIGRATED_KEYS: [&str; 10] = [
        TELEGRAM_BOT_TOKEN, TELEGRAM_ADMIN_CHAT_ID, TELEGRAM_ENABLED,
        DISCORD_BOT_TOKEN, DISCORD_CHANNEL_ID, DISCORD_ADMIN_USER_ID, DISCORD_ENABLED,
        "passcode", ROTATING_PIN_ENABLED, ROTATING_PIN_SECRET,
    ];
    for key in MIGRATED_KEYS {
        if get_shared_setting(key).is_none() {
            if let Some(legacy_value) = get_setting(key) {
                set_shared_setting(key, &legacy_value);
            }
        }
    }
}

/// Get a setting from the shared machine-wide database, falling back to the
/// per-user database if the shared one isn't available.
pub fn get_shared_setting(key: &str) -> Option<String> {
    if let Ok(guard) = SHARED_DB_CONNECTION.lock() {
        if let Some(conn) = guard.as_ref() {
            if let Ok(value) = conn.query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            ) {
                return Some(value);
            }
            return None;
        }
    }
    get_setting(key)
}

/// Set a setting in the shared machine-wide database, falling back to the
/// per-user database if the shared one isn't available.
pub fn set_shared_setting(key: &str, value: &str) -> bool {
    if let Ok(guard) = SHARED_DB_CONNECTION.lock() {
        if let Some(conn) = guard.as_ref() {
            return conn.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                params![key, value],
            ).is_ok();
        }
    }
    set_setting(key, value)
}

/// Set a shared setting (same fallback as `set_shared_setting`) and report
/// what it was, but only if this call actually changed it: `Some(previous)`
/// (`previous` being `None` if the key didn't exist), or `None` if it
/// already held `value` or couldn't be written. Done inside one immediate
/// transaction, so when several sessions/accounts race to claim the same
/// change (e.g. "announce this update once"), exactly one of them sees it.
pub fn swap_shared_setting(key: &str, value: &str) -> Option<Option<String>> {
    if let Ok(mut guard) = SHARED_DB_CONNECTION.lock() {
        if let Some(conn) = guard.as_mut() {
            return swap_setting_in(conn, key, value).ok().flatten();
        }
    }
    let mut guard = DB_CONNECTION.lock().ok()?;
    swap_setting_in(guard.as_mut()?, key, value).ok().flatten()
}

fn swap_setting_in(conn: &mut Connection, key: &str, value: &str) -> rusqlite::Result<Option<Option<String>>> {
    use rusqlite::OptionalExtension;

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous: Option<String> = tx
        .query_row("SELECT value FROM settings WHERE key = ?1", params![key], |row| row.get(0))
        .optional()?;
    if previous.as_deref() == Some(value) {
        return Ok(None);
    }
    tx.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)", params![key, value])?;
    tx.commit()?;
    Ok(Some(previous))
}

/// Path to the marker written when the app quits via the passcode-protected
/// Quit menu item. A plain file rather than a DB row deliberately - the
/// watchdog's check (a brand-new, short-lived process) shouldn't depend on
/// successfully opening a SQLite connection just to read one flag.
fn intentional_quit_marker_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".screen-time-manager")
        .join("intentional_quit")
}

/// Record that the app is quitting via the passcode-protected Quit menu item,
/// so the watchdog task doesn't mistake a sanctioned stop for tampering.
pub fn mark_intentional_quit() {
    let path = intentional_quit_marker_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, b"");
}

/// Clear the intentional-quit marker - called once at the start of every
/// normal (non-watchdog) launch, so the marker only ever covers the single
/// gap between a sanctioned Quit and the app's next real start. Without this,
/// a time-based grace window would need to guess how long the parent might
/// want the app to stay closed; clearing it on start means "closed via Quit"
/// suppresses the watchdog indefinitely, while any kill *after* this point
/// still has no marker and is caught normally.
pub fn clear_intentional_quit_marker() {
    let _ = std::fs::remove_file(intentional_quit_marker_path());
    let _ = std::fs::remove_file(session_ending_marker_path());
}

/// Path to the marker written on an ordinary WM_QUERYENDSESSION (shutdown/
/// logoff/restart/sign-out), as opposed to the passcode-protected Quit above.
/// Kept separate from `intentional_quit_marker_path` - and, unlike it,
/// time-bounded (see `session_ending_recently`) rather than cleared only by
/// the app's next real start. A shutdown/logoff ends the session the app was
/// running in, so "the app's next real start" is a *new* session's `AtLogOn`
/// launch, which can be delayed or occasionally not fire at all (e.g. under
/// Fast User Switching); an indefinite marker left over from the *previous*
/// session would then wrongly convince the watchdog in the new session that
/// this, too, was sanctioned, and it would never relaunch - stuck until
/// someone notices and starts the app by hand.
fn session_ending_marker_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".screen-time-manager")
        .join("session_ending")
}

/// Record that this session is ending via WM_QUERYENDSESSION, so a watchdog
/// check that happens to race the process's teardown doesn't mistake it for
/// tampering. Time-bounded (see `session_ending_recently`), so it can only
/// ever suppress a check for a couple of minutes around the actual shutdown -
/// unlike the passcode-Quit marker, it must never be able to permanently
/// block the watchdog if the following session's own auto-launch misfires.
pub fn mark_session_ending() {
    let path = session_ending_marker_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, wall_clock_now_secs().to_string());
}

/// How long after `mark_session_ending` the marker still counts as covering
/// an in-progress shutdown - just needs to bridge the gap between
/// WM_QUERYENDSESSION and the process actually exiting, not an entire
/// session.
const SESSION_ENDING_MARKER_MAX_AGE_SECS: i64 = 120;

fn session_ending_recently() -> bool {
    std::fs::read_to_string(session_ending_marker_path())
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .is_some_and(|written_at| session_ending_marker_is_recent(wall_clock_now_secs(), written_at))
}

/// How far in the future a `session_ending` timestamp may be and still
/// count: only enough to absorb a small clock correction between writing and
/// reading it. A marker further ahead than this used to be accepted (a
/// negative age passed the age check), so a hand-written future timestamp
/// could suppress the watchdog indefinitely.
const SESSION_ENDING_MARKER_MAX_SKEW_SECS: i64 = 30;

fn session_ending_marker_is_recent(now: i64, written_at: i64) -> bool {
    let age = now - written_at;
    (-SESSION_ENDING_MARKER_MAX_SKEW_SECS..SESSION_ENDING_MARKER_MAX_AGE_SECS).contains(&age)
}

/// Whether the app's most recent stop was either the passcode-protected Quit
/// menu item, or an ordinary shutdown/logoff still within its brief grace
/// window - rather than being killed out from under the timer. The
/// passcode-Quit marker is cleared on every real app start (see
/// `clear_intentional_quit_marker`), so its mere presence - no time window
/// needed - means nothing has run since that sanctioned quit; the session-
/// ending marker instead ages out on its own (see `session_ending_recently`)
/// so it can never get stuck suppressing the watchdog past the shutdown it
/// was meant to cover.
pub fn quit_was_intentional() -> bool {
    intentional_quit_marker_path().exists() || session_ending_recently()
}

/// Path to the marker recording which session's logon (see
/// `session::logon_token`) the watchdog last confirmed the main app running
/// under. Lets it tell "hasn't started yet this session - be patient" apart
/// from "was running earlier this session, isn't now - that's a real
/// problem" regardless of how long a slow session start takes. A plain file
/// rather than a DB row, same reasoning as the intentional-quit marker above.
fn confirmed_running_marker_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".screen-time-manager")
        .join("confirmed_running_logon_token")
}

/// Record that the watchdog just observed the app running under the given
/// session logon token.
pub fn record_confirmed_running(logon_token: i64) {
    let path = confirmed_running_marker_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, logon_token.to_string());
}

/// Whether the app has already been confirmed running at some point under
/// the given session logon token (i.e. this exact session, not a previous
/// one that happened to leave a stale marker behind).
pub fn was_confirmed_running_this_session(logon_token: i64) -> bool {
    std::fs::read_to_string(confirmed_running_marker_path())
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        == Some(logon_token)
}

/// Path to the file the app touches on every real start (see
/// `record_app_started`). Only its modification time matters - the watchdog
/// compares it against the installer's `updating.marker` (see
/// `update::update_marker_modified`) to tell "not yet restarted after an
/// install/update" apart from "restarted since, and then stopped again".
fn app_started_marker_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".screen-time-manager")
        .join("app_started")
}

/// Record that the app just started (in this account).
pub fn record_app_started() {
    let path = app_started_marker_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, wall_clock_now_secs().to_string());
}

/// When the app last started in this account, if ever recorded.
pub fn app_last_started() -> Option<std::time::SystemTime> {
    std::fs::metadata(app_started_marker_path()).ok()?.modified().ok()
}

/// Set the passcode. Shared machine-wide (see `SHARED_DB_CONNECTION`) so the
/// same passcode works no matter which Windows account is logged in, rather
/// than each account needing its own set separately. Read it with
/// `read_passcode`/`effective_passcode`; codes typed by a user are checked
/// through `passcode_guard::attempt`.
pub fn set_passcode(code: &str) -> bool {
    set_shared_setting("passcode", code)
}

/// Settings keys for the optional rotating daily PIN
const ROTATING_PIN_ENABLED: &str = "rotating_pin_enabled";
const ROTATING_PIN_SECRET: &str = "rotating_pin_secret";

/// Whether the rotating daily PIN is enabled (off by default) - when on, it
/// works everywhere the regular passcode does, as an additional valid code
/// alongside it (the regular passcode always keeps working too, so there's
/// no lockout risk if the rotating code isn't at hand). Shared machine-wide,
/// same as the passcode - see `set_passcode`.
pub fn is_rotating_pin_enabled() -> bool {
    get_shared_setting(ROTATING_PIN_ENABLED).map(|s| s == "true").unwrap_or(false)
}

/// Enable/disable the rotating daily PIN
pub fn set_rotating_pin_enabled(enabled: bool) {
    set_shared_setting(ROTATING_PIN_ENABLED, if enabled { "true" } else { "false" });
}

/// Get (creating if needed) the secret used to derive the daily rotating PIN
fn get_or_create_rotating_secret() -> String {
    if let Some(secret) = get_shared_setting(ROTATING_PIN_SECRET) {
        return secret;
    }
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let seed = RandomState::new().build_hasher().finish();
    let secret = seed.to_string();
    set_shared_setting(ROTATING_PIN_SECRET, &secret);
    secret
}

/// Today's rotating 4-digit PIN - deterministic for the day (so it can be
/// verified without storing it anywhere), changes at the next date rollover.
/// Not cryptographically secure - it just needs to be different day to day,
/// not withstand a determined attacker, so a simple FNV hash is enough here.
pub fn get_rotating_pin() -> String {
    let secret = get_or_create_rotating_secret();
    let date = get_today_date();

    let mut hash: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
    for byte in secret.bytes().chain(date.bytes()) {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
    }
    format!("{:04}", hash % 10000)
}

/// Get a setting value from the database
pub fn get_setting(key: &str) -> Option<String> {
    let guard = DB_CONNECTION.lock().ok()?;
    guard.as_ref()?.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |row| row.get(0),
    ).ok()
}

/// Set a setting value in the database
pub fn set_setting(key: &str, value: &str) -> bool {
    if let Ok(guard) = DB_CONNECTION.lock() {
        if let Some(conn) = guard.as_ref() {
            return conn.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
                params![key, value],
            ).is_ok();
        }
    }
    false
}

/// Get daily limit for a specific weekday (0 = Monday, 6 = Sunday)
pub fn get_daily_limit(weekday: u32) -> u32 {
    let key = match weekday {
        0 => "limit_monday",
        1 => "limit_tuesday",
        2 => "limit_wednesday",
        3 => "limit_thursday",
        4 => "limit_friday",
        5 => "limit_saturday",
        6 => "limit_sunday",
        _ => return 120,
    };
    get_setting(key)
        .and_then(|s| s.parse().ok())
        .unwrap_or(120)
}

/// Set daily limit for a specific weekday (0 = Monday, 6 = Sunday)
pub fn set_daily_limit(weekday: u32, minutes: u32) -> bool {
    let key = match weekday {
        0 => "limit_monday",
        1 => "limit_tuesday",
        2 => "limit_wednesday",
        3 => "limit_thursday",
        4 => "limit_friday",
        5 => "limit_saturday",
        6 => "limit_sunday",
        _ => return false,
    };
    set_setting(key, &minutes.to_string())
}

/// Get warning configuration
#[allow(dead_code)]
pub fn get_warning_config(warning_num: u32) -> (u32, String) {
    let minutes_key = format!("warning{}_minutes", warning_num);
    let message_key = format!("warning{}_message", warning_num);

    let minutes = get_setting(&minutes_key)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let message = get_setting(&message_key)
        .unwrap_or_else(|| format!("{} minutes remaining!", minutes));

    (minutes, message)
}

/// Get blocking message
#[allow(dead_code)]
pub fn get_blocking_message() -> String {
    get_setting("blocking_message")
        .unwrap_or_else(|| "Your screen time limit has been reached.".to_string())
}

/// Get the current local date as a string (YYYY-MM-DD)
fn get_today_date() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;

    let st = unsafe { GetLocalTime() };

    format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay)
}

/// Days since 1970-01-01 for a (proleptic Gregorian) date - Howard Hinnant's
/// well-known `days_from_civil` algorithm. Exact for every month/year length,
/// unlike the flat-30-days-per-month approximation `get_current_timestamp`
/// uses (fine for that function's short-duration cooldown math, but wrong by
/// design for walking back real calendar dates here).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of `days_from_civil`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// The date `n` days before `date` (a "YYYY-MM-DD" string), computed exactly.
fn date_n_days_before(date: &str, n: i64) -> String {
    let parts: Vec<i64> = date.splitn(3, '-').filter_map(|p| p.parse().ok()).collect();
    let [y, m, d] = parts[..] else { return date.to_string() };
    let (y, m, d) = civil_from_days(days_from_civil(y, m, d) - n);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// How many days of per-day enforcement/usage data to keep around.
const DAILY_DATA_RETENTION_DAYS: i64 = 14;

/// One day of usage: (date, active minutes, paused minutes).
pub type DailyUsage = (String, i32, i32);

/// Usage for the last `days` calendar days (oldest first): date, minutes
/// actively used, minutes spent paused. Used for the bot's multi-day report.
pub fn get_daily_usage_history(days: u32) -> Vec<DailyUsage> {
    let today = effective_today_date();
    (0..days as i64)
        .rev()
        .map(|n| {
            let date = date_n_days_before(&today, n);
            let active = get_setting(&format!("session_active_{}", date))
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0)
                / 60;
            let paused = get_setting(&format!("pause_used_{}", date))
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0)
                / 60;
            (date, active, paused)
        })
        .collect()
}

/// Shared-database registry of Windows account names with mirrored daily
/// stats (see `mirror_shared_daily_stat`) - comma-separated, since the
/// shared database is a flat key-value store. Kept as an explicit list
/// rather than parsed out of key names, since a username can itself contain
/// underscores/digits that would make splitting a `user_active_<name>_<date>`
/// key ambiguous.
const SHARED_KNOWN_USERS_KEY: &str = "known_usernames";

fn shared_user_stat_key(kind: &str, username: &str, date: &str) -> String {
    format!("user_{kind}_{username}_{date}")
}

/// Register `username` in the shared accounts registry, if not already
/// present.
fn register_shared_username(username: &str) {
    let existing = get_shared_setting(SHARED_KNOWN_USERS_KEY).unwrap_or_default();
    if existing.split(',').any(|u| u == username) {
        return;
    }
    let updated = if existing.is_empty() {
        username.to_string()
    } else {
        format!("{existing},{username}")
    };
    set_shared_setting(SHARED_KNOWN_USERS_KEY, &updated);
}

/// Mirror one of today's per-day usage counters (`kind` is "active" or
/// "pause") into the shared database under this Windows account's name, so
/// bot commands like !weekly can report every account's usage - not just
/// whichever one happens to be running the command - without needing
/// filesystem access to other accounts' per-user databases (which a
/// standard Windows account can't read).
fn mirror_shared_daily_stat(kind: &str, date: &str, seconds: i32) {
    let username = crate::remote_commands::current_windows_username();
    register_shared_username(&username);
    set_shared_setting(&shared_user_stat_key(kind, &username, date), &seconds.to_string());
}

/// Usage for the last `days` calendar days, for every Windows account that's
/// used this app on this machine (username, then oldest-first per-day date/
/// active-minutes/paused-minutes) - the multi-user counterpart of
/// `get_daily_usage_history`, built from the shared database's mirrored
/// per-user stats rather than any single account's own local history.
pub fn get_all_users_daily_usage_history(days: u32) -> Vec<(String, Vec<DailyUsage>)> {
    let today = effective_today_date();
    let mut usernames: Vec<String> = get_shared_setting(SHARED_KNOWN_USERS_KEY)
        .unwrap_or_default()
        .split(',')
        .filter(|u| !u.is_empty())
        .map(|u| u.to_string())
        .collect();
    usernames.sort();

    usernames
        .into_iter()
        .map(|username| {
            let history = (0..days as i64)
                .rev()
                .map(|n| {
                    let date = date_n_days_before(&today, n);
                    let active = get_shared_setting(&shared_user_stat_key("active", &username, &date))
                        .and_then(|s| s.parse::<i32>().ok())
                        .unwrap_or(0)
                        / 60;
                    let paused = get_shared_setting(&shared_user_stat_key("pause", &username, &date))
                        .and_then(|s| s.parse::<i32>().ok())
                        .unwrap_or(0)
                        / 60;
                    (date, active, paused)
                })
                .collect();
            (username, history)
        })
        .collect()
}

/// Delete per-day enforcement/usage rows older than
/// `DAILY_DATA_RETENTION_DAYS`, so the settings table doesn't grow forever -
/// every "today"-scoped key (remaining time, pause usage, session tracking,
/// pause log) accumulates one row per calendar day with nothing else ever
/// cleaning them up. Safe to call often; cheap no-op once nothing qualifies.
pub fn prune_old_daily_data() {
    let cutoff = date_n_days_before(&effective_today_date(), DAILY_DATA_RETENTION_DAYS);

    if let Ok(guard) = DB_CONNECTION.lock() {
        if let Some(conn) = guard.as_ref() {
            for prefix in ["remaining_time_", "session_active_", "pause_used_", "pause_log_"] {
                let pattern = format!("{}%", prefix);
                let _ = conn.execute(
                    "DELETE FROM settings WHERE key LIKE ?1 AND substr(key, ?2) < ?3",
                    params![pattern, (prefix.len() + 1) as i64, cutoff],
                );
            }
        }
    }

    // Same retention for the shared per-user mirrored stats (see
    // `mirror_shared_daily_stat`) - their keys end in the date
    // (`user_<kind>_<username>_<date>`) rather than starting with it, since
    // the username in the middle is variable-length, so this uses substr's
    // negative-offset "last N characters" form to pull the date back out
    // instead of the prefix-length math the per-account keys above use.
    // Call this only after `init_shared_database` has run, or it's a no-op.
    if let Ok(guard) = SHARED_DB_CONNECTION.lock() {
        if let Some(conn) = guard.as_ref() {
            let _ = conn.execute(
                "DELETE FROM settings WHERE key LIKE 'user_%' AND substr(key, -10) < ?1",
                params![cutoff],
            );
        }
    }
}

/// How far the wall clock is allowed to stray from where the monotonic
/// anchor says it should be before it's treated as tampering rather than
/// legitimate drift (NTP correction, a clock that was slightly wrong, etc).
const CLOCK_DRIFT_TOLERANCE_SECS: i64 = 15 * 60;

fn wall_clock_now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn monotonic_now_ms() -> i64 {
    use windows::Win32::System::SystemInformation::GetTickCount64;
    unsafe { GetTickCount64() as i64 }
}

/// Re-point the clock anchor at the current (trusted) wall-clock/date, and
/// clear any tamper flag - called whenever the wall clock is judged trustworthy.
fn resync_clock_anchor(now_wall: i64, now_tick: i64, date: &str) {
    set_setting("clock_anchor_wall_secs", &now_wall.to_string());
    set_setting("clock_anchor_tick_ms", &now_tick.to_string());
    set_setting("clock_anchor_date", date);
    if get_setting("clock_tamper_active").as_deref() == Some("1") {
        set_setting("clock_tamper_active", "0");
        set_setting("clock_tamper_alerted", "0");
    }
}

/// The date to use for all "today"-scoped enforcement state (remaining time,
/// pause allowance, session tracking), gated against system-clock manipulation.
///
/// Plain `get_today_date()` is exactly what a kid can move to get a fresh
/// daily allowance: change the date, then get the app to restart (kill it -
/// the watchdog relaunches it within a minute anyway) so the loaded state
/// finds no row for the "new" date and grants a full day. To catch that, a
/// monotonic anchor (wall-clock time + `GetTickCount64` uptime, which only
/// moves via an actual reboot, not the Date & Time settings) is persisted
/// alongside every save. If the real wall clock ever strays more than
/// `CLOCK_DRIFT_TOLERANCE_SECS` from where the anchor says it should be,
/// that's not elapsed time - it's the clock being moved - so "today" stays
/// pinned to the last known-good date until the real clock drifts back
/// within tolerance on its own.
pub fn effective_today_date() -> String {
    let today = get_today_date();
    let now_wall = wall_clock_now_secs();
    let now_tick = monotonic_now_ms();

    let anchor = get_setting("clock_anchor_wall_secs")
        .and_then(|s| s.parse::<i64>().ok())
        .zip(get_setting("clock_anchor_tick_ms").and_then(|s| s.parse::<i64>().ok()))
        .zip(get_setting("clock_anchor_date"));

    let Some(((anchor_wall, anchor_tick), anchor_date)) = anchor else {
        // First run ever - nothing to compare against yet.
        reset_utc_offset_baseline();
        resync_clock_anchor(now_wall, now_tick, &today);
        return today;
    };

    if now_tick < anchor_tick {
        // The machine rebooted since the last anchor, so the tick counter
        // reset and can't sanity-check the wall clock across that gap.
        // Accept the current time and start fresh from here.
        reset_utc_offset_baseline();
        resync_clock_anchor(now_wall, now_tick, &today);
        return today;
    }

    let expected_wall = anchor_wall + (now_tick - anchor_tick) / 1000;
    let drift = now_wall - expected_wall;
    // A time-zone change moves the local date without moving UTC, so the
    // drift check above can't see it - see `utc_offset_tamper_secs`.
    let tz_shift = utc_offset_tamper_secs();

    if drift.abs() <= CLOCK_DRIFT_TOLERANCE_SECS && tz_shift.is_none() {
        resync_clock_anchor(now_wall, now_tick, &today);
        today
    } else {
        let reported = if drift.abs() > CLOCK_DRIFT_TOLERANCE_SECS { drift } else { tz_shift.unwrap_or(drift) };
        set_setting("clock_tamper_active", "1");
        set_setting("clock_tamper_drift_secs", &reported.to_string());
        anchor_date
    }
}

/// Returns the drift (signed seconds) the first time a clock-tamper episode
/// is observed, so callers can send exactly one alert per episode instead of
/// re-notifying on every periodic save while the clock stays off. `None` both
/// while no episode is active and after one has already been alerted on.
pub fn take_pending_clock_tamper_alert() -> Option<i64> {
    if get_setting("clock_tamper_active").as_deref() != Some("1") {
        return None;
    }
    if get_setting("clock_tamper_alerted").as_deref() == Some("1") {
        return None;
    }
    let drift: i64 = get_setting("clock_tamper_drift_secs")?.parse().ok()?;
    set_setting("clock_tamper_alerted", "1");
    Some(drift)
}

/// Save remaining time to database (associated with current date)
pub fn save_remaining_time(seconds: i32) {
    let date = effective_today_date();
    let key = format!("remaining_time_{}", date);
    set_setting(&key, &seconds.to_string());
}

/// Load remaining time from database for today
#[allow(dead_code)]
pub fn load_remaining_time() -> Option<i32> {
    let date = effective_today_date();
    let key = format!("remaining_time_{}", date);
    get_setting(&key).and_then(|s| s.parse().ok())
}

/// Get the current weekday (0 = Monday, 6 = Sunday)
#[allow(dead_code)]
pub fn get_current_weekday() -> u32 {
    use windows::Win32::System::SystemInformation::GetLocalTime;

    let st = unsafe { GetLocalTime() };

    // Windows: wDayOfWeek is 0 = Sunday, 1 = Monday, ..., 6 = Saturday
    // We want: 0 = Monday, 1 = Tuesday, ..., 6 = Sunday
    if st.wDayOfWeek == 0 {
        6 // Sunday
    } else {
        (st.wDayOfWeek - 1) as u32
    }
}

// ============================================================================
// Lock Screen Timeout Functions
// ============================================================================

/// Lock screen timeout in seconds (how long the lock screen counts down
/// before shutting down or signing out), never less than
/// `MIN_LOCK_SCREEN_TIMEOUT_SECS`. The one place the stored value is read:
/// the lock screen uses it as is, and Settings refuses to save less.
pub fn get_lock_screen_timeout() -> i32 {
    let secs = get_setting("lock_screen_timeout")
        .and_then(|s| s.parse().ok())
        .unwrap_or(600); // 10 minutes default
    clamp_lock_screen_timeout(secs)
}

/// The shortest lock-screen timeout, in seconds. Settings used to accept 0
/// minutes, which shut the PC down a second after every block (including at
/// each logon with no time left); anything shorter that is already stored -
/// or negative, which the app never writes - is read as one minute.
pub const MIN_LOCK_SCREEN_TIMEOUT_MINS: i32 = 1;
const MIN_LOCK_SCREEN_TIMEOUT_SECS: i32 = MIN_LOCK_SCREEN_TIMEOUT_MINS * 60;

fn clamp_lock_screen_timeout(secs: i32) -> i32 {
    secs.max(MIN_LOCK_SCREEN_TIMEOUT_SECS)
}

// ============================================================================
// Pause Mode Functions
// ============================================================================

/// Check if pause mode is enabled
pub fn is_pause_enabled() -> bool {
    get_setting("pause_enabled")
        .map(|s| s == "1")
        .unwrap_or(true)
}

/// Get pause configuration
pub struct PauseConfig {
    pub daily_budget_minutes: u32,
    pub max_duration_minutes: u32,
    pub cooldown_minutes: u32,
    pub min_active_time_minutes: u32,
}

pub fn get_pause_config() -> PauseConfig {
    PauseConfig {
        daily_budget_minutes: get_setting("pause_daily_budget")
            .and_then(|s| s.parse().ok())
            .unwrap_or(45),
        max_duration_minutes: get_setting("pause_max_duration")
            .and_then(|s| s.parse().ok())
            .unwrap_or(20),
        cooldown_minutes: get_setting("pause_cooldown")
            .and_then(|s| s.parse().ok())
            .unwrap_or(15),
        min_active_time_minutes: get_setting("pause_min_active_time")
            .and_then(|s| s.parse().ok())
            .unwrap_or(10),
    }
}

/// Get pause time used today (in seconds)
pub fn get_pause_used_today() -> i32 {
    let date = effective_today_date();
    let key = format!("pause_used_{}", date);
    get_setting(&key)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Save pause time used today (in seconds)
pub fn save_pause_used_today(seconds: i32) {
    let date = effective_today_date();
    let key = format!("pause_used_{}", date);
    set_setting(&key, &seconds.to_string());
    mirror_shared_daily_stat("pause", &date, seconds);
}

/// Get timestamp of last pause end (Unix timestamp)
pub fn get_last_pause_end() -> i64 {
    get_setting("pause_last_end_timestamp")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Save timestamp of last pause end
pub fn save_last_pause_end(timestamp: i64) {
    set_setting("pause_last_end_timestamp", &timestamp.to_string());
}

/// Current Unix timestamp (real seconds since 1970-01-01 UTC), used for the
/// pause cooldown. Used to be a local-time approximation counting every
/// month as 30 days, which made the 31st and the 1st collide; values stored
/// in that old format are always more than a week behind real epoch time, so
/// they read as a long-expired cooldown (see `mini_overlay::cooldown_remaining_secs`).
pub fn get_current_timestamp() -> i64 {
    wall_clock_now_secs()
}

/// Get the session start time used today (in seconds) - tracks when timer started today
pub fn get_session_active_time() -> i32 {
    let date = effective_today_date();
    let key = format!("session_active_{}", date);
    get_setting(&key)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Save session active time (in seconds)
pub fn save_session_active_time(seconds: i32) {
    let date = effective_today_date();
    let key = format!("session_active_{}", date);
    set_setting(&key, &seconds.to_string());
    mirror_shared_daily_stat("active", &date, seconds);
}

/// Log a pause event for today
pub fn log_pause_event(duration_seconds: i32) {
    use windows::Win32::System::SystemInformation::GetLocalTime;

    let st = unsafe { GetLocalTime() };
    let time_str = format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond);

    let date = get_today_date();
    let key = format!("pause_log_{}", date);

    let existing = get_setting(&key).unwrap_or_default();
    let new_entry = format!("{}:{}s", time_str, duration_seconds);

    let updated = if existing.is_empty() {
        new_entry
    } else {
        format!("{},{}", existing, new_entry)
    };

    set_setting(&key, &updated);
}

/// Get pause log for today
pub fn get_pause_log_today() -> Vec<String> {
    let date = get_today_date();
    let key = format!("pause_log_{}", date);

    get_setting(&key)
        .map(|s| s.split(',').map(|e| e.to_string()).collect())
        .unwrap_or_default()
}

// ============================================================================
// Idle Detection Functions
// ============================================================================

/// Check if idle detection is enabled
pub fn is_idle_enabled() -> bool {
    get_setting("idle_enabled")
        .map(|s| s == "1")
        .unwrap_or(false)
}

/// Get idle timeout in minutes (minimum 1)
pub fn get_idle_timeout_minutes() -> u32 {
    get_setting("idle_timeout_minutes")
        .and_then(|s| s.parse().ok())
        .unwrap_or(5)
        .max(1)
}

// ============================================================================
// Telegram Bot Configuration
// ============================================================================

/// Settings keys for Telegram bot
const TELEGRAM_BOT_TOKEN: &str = "telegram_bot_token";
const TELEGRAM_ADMIN_CHAT_ID: &str = "telegram_admin_chat_id";
const TELEGRAM_ENABLED: &str = "telegram_enabled";

/// Telegram bot configuration
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramConfig {
    pub bot_token: Option<String>,
    pub admin_chat_id: Option<i64>,
    pub enabled: bool,
}

/// Get Telegram bot configuration (shared machine-wide, not per-account)
pub fn get_telegram_config() -> TelegramConfig {
    TelegramConfig {
        bot_token: get_shared_setting(TELEGRAM_BOT_TOKEN),
        admin_chat_id: get_shared_setting(TELEGRAM_ADMIN_CHAT_ID)
            .and_then(|s| s.parse::<i64>().ok()),
        enabled: get_shared_setting(TELEGRAM_ENABLED)
            .map(|s| s == "true")
            .unwrap_or(false),
    }
}

/// Save Telegram bot configuration (shared machine-wide, not per-account)
pub fn set_telegram_config(token: &str, chat_id: &str, enabled: bool) {
    set_shared_setting(TELEGRAM_BOT_TOKEN, token);
    set_shared_setting(TELEGRAM_ADMIN_CHAT_ID, chat_id);
    set_shared_setting(TELEGRAM_ENABLED, if enabled { "true" } else { "false" });
}

// ============================================================================
// Discord Bot Configuration
// ============================================================================

/// Settings keys for Discord bot
const DISCORD_BOT_TOKEN: &str = "discord_bot_token";
const DISCORD_CHANNEL_ID: &str = "discord_channel_id";
const DISCORD_ADMIN_USER_ID: &str = "discord_admin_user_id";
const DISCORD_ENABLED: &str = "discord_enabled";

/// Discord bot configuration
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordConfig {
    pub bot_token: Option<String>,
    pub channel_id: Option<u64>,
    pub admin_user_id: Option<u64>,
    pub enabled: bool,
}

/// Get Discord bot configuration (shared machine-wide, not per-account)
pub fn get_discord_config() -> DiscordConfig {
    DiscordConfig {
        bot_token: get_shared_setting(DISCORD_BOT_TOKEN),
        channel_id: get_shared_setting(DISCORD_CHANNEL_ID)
            .and_then(|s| s.parse::<u64>().ok()),
        admin_user_id: get_shared_setting(DISCORD_ADMIN_USER_ID)
            .and_then(|s| s.parse::<u64>().ok()),
        enabled: get_shared_setting(DISCORD_ENABLED)
            .map(|s| s == "true")
            .unwrap_or(false),
    }
}

/// Save Discord bot configuration (shared machine-wide, not per-account)
pub fn set_discord_config(token: &str, channel_id: &str, admin_user_id: &str, enabled: bool) {
    set_shared_setting(DISCORD_BOT_TOKEN, token);
    set_shared_setting(DISCORD_CHANNEL_ID, channel_id);
    set_shared_setting(DISCORD_ADMIN_USER_ID, admin_user_id);
    set_shared_setting(DISCORD_ENABLED, if enabled { "true" } else { "false" });
}

// ============================================================================
// Passcode reading that fails closed, and startup database recovery
// ============================================================================

/// Whether `init_shared_database` has opened the shared database in this
/// process.
pub fn shared_database_is_open() -> bool {
    SHARED_DB_CONNECTION.lock().is_ok_and(|g| g.is_some())
}

/// The passcode a fresh install starts with (see `init_database`'s defaults).
pub const DEFAULT_PASSCODE: &str = "0000";

/// What `read_passcode` found.
#[derive(Debug, Clone, PartialEq)]
pub enum PasscodeRead {
    /// A passcode is stored.
    Set(String),
    /// Neither database has a (non-blank) passcode row, so the default
    /// `DEFAULT_PASSCODE` applies - the same assumption Settings has always
    /// made when checking the current passcode.
    NotConfigured,
    /// A database could not be queried (busy beyond the 5-second busy
    /// timeout, I/O error, not open...). Callers must refuse, not allow.
    Unreadable,
}

/// Read the passcode, telling "nothing stored" apart from "could not read".
/// Unlike the old `get_passcode` (which returned `None` for both, and which
/// the passcode dialog treated as "no passcode, let them in"), a query error
/// here is reported as `Unreadable`. The shared database wins when it
/// has the row; when it doesn't (e.g. its one-time migration could not
/// write), the per-account value is used, as `init_shared_database`'s
/// migration intends.
pub fn read_passcode() -> PasscodeRead {
    use rusqlite::OptionalExtension;

    fn query(conn: &Connection) -> rusqlite::Result<Option<String>> {
        conn.query_row("SELECT value FROM settings WHERE key = 'passcode'", [], |row| row.get(0))
            .optional()
    }
    fn classify(result: rusqlite::Result<Option<String>>) -> Option<PasscodeRead> {
        match result {
            Ok(Some(code)) if !code.trim().is_empty() => Some(PasscodeRead::Set(code)),
            Ok(_) => None,
            Err(_) => Some(PasscodeRead::Unreadable),
        }
    }

    match SHARED_DB_CONNECTION.lock() {
        Ok(guard) => {
            if let Some(conn) = guard.as_ref() {
                if let Some(found) = classify(query(conn)) {
                    return found;
                }
            }
        }
        Err(_) => return PasscodeRead::Unreadable,
    }

    let Ok(guard) = DB_CONNECTION.lock() else { return PasscodeRead::Unreadable };
    let Some(conn) = guard.as_ref() else { return PasscodeRead::Unreadable };
    classify(query(conn)).unwrap_or(PasscodeRead::NotConfigured)
}

/// The passcode currently in force, or `None` if it could not be read (in
/// which case nothing may be unlocked with it).
pub fn effective_passcode() -> Option<String> {
    match read_passcode() {
        PasscodeRead::Set(code) => Some(code),
        PasscodeRead::NotConfigured => Some(DEFAULT_PASSCODE.to_string()),
        PasscodeRead::Unreadable => None,
    }
}

/// Whether the passcode in force is still the factory default, for the
/// warning in Settings. `false` if it could not be read.
pub fn passcode_is_default() -> bool {
    effective_passcode().as_deref() == Some(DEFAULT_PASSCODE)
}

/// Check an entered code without any attempt limiting, failing closed:
/// `Some(true)` if it is the passcode or (when enabled) today's rotating
/// PIN, `Some(false)` if not, `None` if the passcode could not be read and
/// the rotating PIN did not match either. Attempt limiting lives in
/// `passcode_guard`, which is what UI code should call.
pub fn check_passcode(entered: &str) -> Option<bool> {
    if is_rotating_pin_enabled() && entered == get_rotating_pin() {
        return Some(true);
    }
    effective_passcode().map(|code| entered == code)
}

/// What `init_database_with_recovery` had to do to get a per-account
/// database open.
pub enum DatabaseStartup {
    /// Opened normally (possibly after a retry).
    Opened,
    /// The file was unreadable as a database; it was renamed to `moved_to`
    /// and a fresh one with default settings created.
    Recreated { moved_to: PathBuf, error: String },
    /// The file could not be opened and was left alone (e.g. locked by
    /// another program, or a permissions/I/O problem); the app runs on an
    /// in-memory database with default settings that is lost on exit.
    Temporary { error: String },
    /// Not even an in-memory database could be set up.
    Failed { error: String },
}

/// Open the per-account database for the app's startup without ever
/// leaving it stuck: a corrupt file is moved aside and recreated; any other
/// failure is retried briefly and then replaced by a temporary in-memory
/// database. Only files SQLite itself reports as corrupt or not a database
/// are moved aside, so a file that is merely locked by another program
/// can't be used to wipe the parent's settings.
pub fn init_database_with_recovery() -> DatabaseStartup {
    const RETRIES: u32 = 2;

    let first_error = match init_database() {
        Ok(()) => return DatabaseStartup::Opened,
        Err(e) => e,
    };

    if is_corruption_error(first_error.as_ref()) {
        let db_path = get_database_path();
        return match move_database_aside(&db_path) {
            Ok(moved_to) => match init_database() {
                Ok(()) => DatabaseStartup::Recreated { moved_to, error: first_error.to_string() },
                Err(e) => start_temporary_database(e.to_string()),
            },
            Err(e) => start_temporary_database(format!("{first_error}; could not move it aside: {e}")),
        };
    }

    let mut last_error = first_error;
    for _ in 0..RETRIES {
        std::thread::sleep(std::time::Duration::from_secs(2));
        match init_database() {
            Ok(()) => return DatabaseStartup::Opened,
            Err(e) => last_error = e,
        }
    }
    start_temporary_database(last_error.to_string())
}

fn start_temporary_database(error: String) -> DatabaseStartup {
    let installed = Connection::open_in_memory()
        .map_err(|e| e.into())
        .and_then(install_settings_connection);
    match installed {
        Ok(()) => DatabaseStartup::Temporary { error },
        Err(e) => DatabaseStartup::Failed { error: format!("{error}; in-memory fallback: {e}") },
    }
}

/// Whether SQLite reported the file as damaged or not a database at all.
fn is_corruption_error(error: &(dyn std::error::Error + 'static)) -> bool {
    use rusqlite::ErrorCode;
    matches!(
        error.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(e, _))
            if matches!(e.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

/// Rename `db_path` (and any journal/WAL files next to it) to
/// `<name>.broken-<unix time>`, keeping it for inspection. Returns the new
/// path of the main file.
fn move_database_aside(db_path: &std::path::Path) -> std::io::Result<PathBuf> {
    let suffix = format!(".broken-{}", wall_clock_now_secs());
    let mut moved_to = db_path.as_os_str().to_owned();
    moved_to.push(&suffix);
    let moved_to = PathBuf::from(moved_to);
    std::fs::rename(db_path, &moved_to)?;
    for extra in ["-journal", "-wal", "-shm"] {
        let mut from = db_path.as_os_str().to_owned();
        from.push(extra);
        let mut to = db_path.as_os_str().to_owned();
        to.push(extra);
        to.push(&suffix);
        let _ = std::fs::rename(PathBuf::from(from), PathBuf::from(to));
    }
    Ok(moved_to)
}

// ============================================================================
// Time-zone tamper detection, local date, and in-flight pause persistence
// ============================================================================

/// The UTC offset (local minus UTC, minutes) in effect when the clock anchor
/// was last created - at first run or after a reboot, deliberately *not* on
/// every resync, so a time zone can't be walked a little at a time.
const UTC_OFFSET_BASELINE_KEY: &str = "clock_anchor_utc_offset_mins";

/// The largest UTC-offset change, relative to the baseline, that counts as a
/// daylight-saving transition rather than a time-zone change. DST moves the
/// offset by 60 minutes (30 in a few places) and back again, so it never
/// strays further than this from the baseline.
const MAX_DST_SHIFT_MINS: i64 = 60;

/// Seconds since 1970-01-01 for a civil date and time (no time zone).
fn civil_to_secs(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + s
}

fn systemtime_secs(st: &windows::Win32::Foundation::SYSTEMTIME) -> i64 {
    civil_to_secs(
        st.wYear as i64, st.wMonth as i64, st.wDay as i64,
        st.wHour as i64, st.wMinute as i64, st.wSecond as i64,
    )
}

/// Local minus UTC in whole minutes, given the same instant read both ways
/// (in seconds since the epoch, as if both were UTC). Rounded to the nearest
/// minute, since the two readings can straddle a second boundary.
fn utc_offset_mins_from(local_secs: i64, utc_secs: i64) -> i64 {
    (local_secs - utc_secs + 30).div_euclid(60)
}

/// The UTC offset currently in effect, in minutes (e.g. 60 for CET, 120 for CEST).
fn current_utc_offset_mins() -> i64 {
    use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
    let (local, utc) = unsafe { (GetLocalTime(), GetSystemTime()) };
    utc_offset_mins_from(systemtime_secs(&local), systemtime_secs(&utc))
}

/// How far the offset moved beyond a DST step from the baseline, in minutes,
/// or `None` if it's within one (including no change at all).
fn utc_offset_shift_beyond_dst(baseline_mins: i64, current_mins: i64) -> Option<i64> {
    let shift = current_mins - baseline_mins;
    (shift.abs() > MAX_DST_SHIFT_MINS).then_some(shift)
}

fn reset_utc_offset_baseline() {
    set_setting(UTC_OFFSET_BASELINE_KEY, &current_utc_offset_mins().to_string());
}

/// `Some(shift in seconds)` while the time zone differs from the baseline by
/// more than a DST step - a standard user can run `tzutil /s` to move "today"
/// to tomorrow's date without touching the clock itself. Installs from before
/// this check have no baseline yet; the first call records one.
fn utc_offset_tamper_secs() -> Option<i64> {
    let current = current_utc_offset_mins();
    match get_setting(UTC_OFFSET_BASELINE_KEY).and_then(|s| s.parse::<i64>().ok()) {
        Some(baseline) => utc_offset_shift_beyond_dst(baseline, current).map(|m| m * 60),
        None => {
            set_setting(UTC_OFFSET_BASELINE_KEY, &current.to_string());
            None
        }
    }
}

/// Today's date by the local clock, without the clock-tamper gate or any
/// database access - cheap enough to poll every second to notice that the
/// date may have changed, before asking `effective_today_date` for the
/// authoritative answer.
pub fn local_today_date() -> String {
    get_today_date()
}

/// The in-progress pause, saved periodically while paused so that a sign-out,
/// crash, kill or update mid-pause can't refund it: "<date>|<seconds>|<epoch
/// seconds when saved>". Cleared when the pause ends normally.
const PAUSE_INFLIGHT_KEY: &str = "pause_inflight";

/// Record how long the current pause has run so far.
pub fn save_inflight_pause(seconds: i32) {
    let value = format!("{}|{}|{}", effective_today_date(), seconds, wall_clock_now_secs());
    set_setting(PAUSE_INFLIGHT_KEY, &value);
}

/// Forget the in-progress pause (it ended and was added to the day's usage).
pub fn clear_inflight_pause() {
    if let Ok(guard) = DB_CONNECTION.lock() {
        if let Some(conn) = guard.as_ref() {
            let _ = conn.execute("DELETE FROM settings WHERE key = ?1", params![PAUSE_INFLIGHT_KEY]);
        }
    }
}

/// Parse a `PAUSE_INFLIGHT_KEY` value: (date, seconds, saved-at epoch secs).
fn parse_inflight_pause(value: &str) -> Option<(String, i32, i64)> {
    let mut parts = value.splitn(3, '|');
    let date = parts.next()?;
    let seconds: i32 = parts.next()?.parse().ok()?;
    let saved_at: i64 = parts.next()?.parse().ok()?;
    let date_ok = date.len() == 10
        && date.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() });
    (date_ok && seconds > 0).then(|| (date.to_string(), seconds, saved_at))
}

/// Fold a pause left over from a previous run (the app stopped mid-pause) into
/// that day's pause usage and pause log, and start the cooldown from when it
/// was last saved. Returns the seconds recovered. Call once at startup.
pub fn recover_inflight_pause() -> Option<i32> {
    let raw = get_setting(PAUSE_INFLIGHT_KEY)?;
    let Some((date, seconds, saved_at)) = parse_inflight_pause(&raw) else {
        clear_inflight_pause();
        return None;
    };

    let used_key = format!("pause_used_{}", date);
    let used: i32 = get_setting(&used_key).and_then(|s| s.parse().ok()).unwrap_or(0);
    let total = used.saturating_add(seconds);
    set_setting(&used_key, &total.to_string());
    mirror_shared_daily_stat("pause", &date, total);

    // Same "HH:MM:SS:<n>s" entry log_pause_event writes, timed at the save.
    let local = (saved_at + current_utc_offset_mins() * 60).rem_euclid(86400);
    let entry = format!("{:02}:{:02}:{:02}:{}s", local / 3600, (local % 3600) / 60, local % 60, seconds);
    let log_key = format!("pause_log_{}", date);
    let existing = get_setting(&log_key).unwrap_or_default();
    let updated = if existing.is_empty() { entry } else { format!("{},{}", existing, entry) };
    set_setting(&log_key, &updated);

    if saved_at > get_last_pause_end() {
        save_last_pause_end(saved_at);
    }
    clear_inflight_pause();
    Some(seconds)
}

#[cfg(test)]
mod timekeeping_tests {
    use super::*;

    #[test]
    fn utc_offset_rounds_to_whole_minutes() {
        let utc = civil_to_secs(2026, 3, 29, 0, 59, 59);
        // CET (+1h), read a second apart across the boundary
        assert_eq!(utc_offset_mins_from(civil_to_secs(2026, 3, 29, 2, 0, 0), utc), 60);
        // Across a date boundary: local 00:30 next day vs UTC 23:30 = +60
        assert_eq!(
            utc_offset_mins_from(civil_to_secs(2026, 1, 1, 0, 30, 0), civil_to_secs(2025, 12, 31, 23, 30, 0)),
            60
        );
        // West of UTC, including half hours (Newfoundland -3:30)
        assert_eq!(
            utc_offset_mins_from(civil_to_secs(2026, 1, 1, 8, 30, 0), civil_to_secs(2026, 1, 1, 12, 0, 1)),
            -210
        );
        // Month end
        assert_eq!(
            utc_offset_mins_from(civil_to_secs(2026, 3, 1, 9, 0, 0), civil_to_secs(2026, 2, 28, 23, 0, 0)),
            600
        );
    }

    #[test]
    fn dst_transitions_are_not_tampering() {
        // CET baseline: summer time and back
        assert_eq!(utc_offset_shift_beyond_dst(60, 120), None);
        assert_eq!(utc_offset_shift_beyond_dst(120, 60), None);
        assert_eq!(utc_offset_shift_beyond_dst(60, 60), None);
        // Lord Howe's 30-minute DST
        assert_eq!(utc_offset_shift_beyond_dst(630, 660), None);
    }

    #[test]
    fn time_zone_jumps_are_tampering() {
        // tzutil from UTC-8 to UTC+14 to borrow tomorrow
        assert_eq!(utc_offset_shift_beyond_dst(-480, 840), Some(1320));
        // Backwards too
        assert_eq!(utc_offset_shift_beyond_dst(60, -600), Some(-660));
        // Just past a DST step, or a DST step on top of a zone change
        assert_eq!(utc_offset_shift_beyond_dst(60, 180), Some(120));
        assert_eq!(utc_offset_shift_beyond_dst(60, 135), Some(75));
    }

    #[test]
    fn inflight_pause_parses_only_valid_values() {
        assert_eq!(
            parse_inflight_pause("2026-11-15|130|1794750000"),
            Some(("2026-11-15".to_string(), 130, 1794750000))
        );
        assert_eq!(parse_inflight_pause("2026-11-15|0|1794750000"), None, "nothing to fold");
        assert_eq!(parse_inflight_pause("2026-11-15|-5|1794750000"), None);
        assert_eq!(parse_inflight_pause("2026-11-15|abc|1794750000"), None);
        assert_eq!(parse_inflight_pause("2026-11-15|130"), None);
        assert_eq!(parse_inflight_pause("garbage|130|1794750000"), None);
        assert_eq!(parse_inflight_pause(""), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_reports_only_actual_changes() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)", []).unwrap();

        assert_eq!(swap_setting_in(&mut conn, "k", "1.0.0").unwrap(), Some(None), "new key");
        assert_eq!(swap_setting_in(&mut conn, "k", "1.0.0").unwrap(), None, "unchanged");
        assert_eq!(swap_setting_in(&mut conn, "k", "1.1.0").unwrap(), Some(Some("1.0.0".into())));
        let stored: String = conn.query_row("SELECT value FROM settings WHERE key = 'k'", [], |r| r.get(0)).unwrap();
        assert_eq!(stored, "1.1.0");
    }

    #[test]
    fn session_ending_marker_rejects_future_and_old_timestamps() {
        let now = 1_000_000;
        assert!(session_ending_marker_is_recent(now, now));
        assert!(session_ending_marker_is_recent(now, now - 119));
        assert!(!session_ending_marker_is_recent(now, now - 120));
        // A small clock correction is tolerated...
        assert!(session_ending_marker_is_recent(now, now + 30));
        // ...a timestamp from the future is not (it used to count forever).
        assert!(!session_ending_marker_is_recent(now, now + 31));
        assert!(!session_ending_marker_is_recent(now, now + 10 * 365 * 86_400));
    }

    #[test]
    fn lock_screen_timeout_is_at_least_a_minute() {
        assert_eq!(clamp_lock_screen_timeout(0), 60);
        assert_eq!(clamp_lock_screen_timeout(59), 60);
        assert_eq!(clamp_lock_screen_timeout(60), 60);
        assert_eq!(clamp_lock_screen_timeout(600), 600);
        assert_eq!(clamp_lock_screen_timeout(-1), 60);
        assert_eq!(clamp_lock_screen_timeout(-600), 60);
    }

    #[test]
    fn garbage_file_is_reported_as_corrupt_but_a_good_one_opens() {
        let dir = std::env::temp_dir().join(format!("stm-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let garbage = dir.join("garbage.db");
        std::fs::write(&garbage, vec![0x5Au8; 8192]).unwrap();
        let conn = Connection::open(&garbage).unwrap();
        let err = prepare_settings_connection(&conn).unwrap_err();
        assert!(is_corruption_error(err.as_ref()), "{err}");
        drop(conn);

        let moved = move_database_aside(&garbage).unwrap();
        assert!(!garbage.exists());
        assert!(moved.exists());

        let good = Connection::open(dir.join("good.db")).unwrap();
        prepare_settings_connection(&good).unwrap();
        let code: String = good.query_row("SELECT value FROM settings WHERE key = 'passcode'", [], |r| r.get(0)).unwrap();
        assert_eq!(code, DEFAULT_PASSCODE);
        drop(good);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
