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
        // On unless a parent turns it off: a child who walks away shouldn't
        // keep losing time. Only written when the key is missing, so an
        // install that already has a stored value keeps it (see
        // `idle_enabled_from` for missing and empty values).
        ("idle_enabled", "1"),              // 1 = enabled, 0 = disabled
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

/// Get (creating if needed) the secret used to derive the daily rotating
/// PIN, from the shared database (or the per-account one when the shared
/// one isn't open, like the other shared settings). `None` if it could not
/// be read: a busy or failing database must never be taken for a missing
/// secret, since writing a new one would invalidate a PIN the parent has
/// already been given.
fn get_or_create_rotating_secret() -> Option<String> {
    match SHARED_DB_CONNECTION.lock() {
        Ok(guard) => {
            if let Some(conn) = guard.as_ref() {
                return get_or_create_rotating_secret_in(conn, new_rotating_secret);
            }
        }
        Err(_) => return None,
    }
    let guard = DB_CONNECTION.lock().ok()?;
    get_or_create_rotating_secret_in(guard.as_ref()?, new_rotating_secret)
}

/// `get_or_create_rotating_secret` against one database: a new secret is
/// only created when the read succeeded and found no row. Created with
/// INSERT OR IGNORE and read back, so two processes creating it at once
/// both end up with the same one.
fn get_or_create_rotating_secret_in(conn: &Connection, new_secret: impl FnOnce() -> String) -> Option<String> {
    use rusqlite::OptionalExtension;

    let read = |conn: &Connection| -> rusqlite::Result<Option<String>> {
        conn.query_row("SELECT value FROM settings WHERE key = ?1", params![ROTATING_PIN_SECRET], |row| row.get(0))
            .optional()
    };
    match read(conn) {
        Ok(Some(secret)) => return Some(secret),
        Ok(None) => {}
        Err(_) => return None,
    }
    conn.execute(
        "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
        params![ROTATING_PIN_SECRET, new_secret()],
    )
    .ok()?;
    read(conn).ok().flatten()
}

fn new_rotating_secret() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    RandomState::new().build_hasher().finish().to_string()
}

/// Today's rotating 4-digit PIN - deterministic for the day (so it can be
/// verified without storing it anywhere), changes at the next date rollover.
/// Not cryptographically secure - it just needs to be different day to day,
/// not withstand a determined attacker, so a simple FNV hash is enough here.
/// `None` if its secret could not be read (nothing then matches it).
pub fn rotating_pin() -> Option<String> {
    let secret = get_or_create_rotating_secret()?;
    Some(rotating_pin_for(&secret, &get_today_date()))
}

fn rotating_pin_for(secret: &str, date: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325; // FNV-1a offset basis
    for byte in secret.bytes().chain(date.bytes()) {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3); // FNV prime
    }
    format!("{:04}", hash % 10000)
}

/// Today's rotating PIN for showing to the admin (`/getpin`), or a note
/// that it could not be read right now.
pub fn get_rotating_pin() -> String {
    rotating_pin().unwrap_or_else(|| crate::i18n::t("tg.getpin.unavailable").to_string())
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
        _ => return DEFAULT_DAILY_LIMIT_MINUTES,
    };
    minutes_or(get_setting(key).as_deref(), DEFAULT_DAILY_LIMIT_MINUTES)
}

/// First-run defaults that the readers also fall back to when a stored
/// value is missing, empty or not a number - the two must agree.
const DEFAULT_DAILY_LIMIT_MINUTES: u32 = 120;
const DEFAULT_WARNING1_MINUTES: u32 = 10;
const DEFAULT_WARNING2_MINUTES: u32 = 5;

/// Default minutes for warning 1 or 2 (see `get_warning_config`).
fn default_warning_minutes(warning_num: u32) -> u32 {
    if warning_num == 1 { DEFAULT_WARNING1_MINUTES } else { DEFAULT_WARNING2_MINUTES }
}

/// A stored minutes value, or `default` if it's missing, empty or not a
/// number.
fn minutes_or(stored: Option<&str>, default: u32) -> u32 {
    stored.and_then(|s| s.parse().ok()).unwrap_or(default)
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

    // Used to default to 5 for both, so a missing or empty first warning
    // fired together with the second.
    let minutes = minutes_or(get_setting(&minutes_key).as_deref(), default_warning_minutes(warning_num));
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
            for prefix in ["remaining_time_", "session_active_", "pause_used_", "pause_log_", "limit_applied_"] {
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

/// Where the wall clock and `GetTickCount64` were at the last check that
/// trusted the clock, the date then, and the logon session
/// (`session::logon_token`) that check ran in. Kept in the per-account
/// database, so with Fast User Switching each account has its own.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClockAnchor {
    wall_secs: i64,
    tick_ms: i64,
    date: String,
    /// `None` for an anchor written by a version before 1.0.1, or while the
    /// session couldn't be identified.
    logon_token: Option<i64>,
}

/// The clock-tamper episode state: whether one is going on, whether the
/// admin was alerted about it, and how far the clock was moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ClockTamper {
    active: bool,
    alerted: bool,
    drift_secs: Option<i64>,
}

/// The clocks as read now.
#[derive(Debug, Clone, Copy)]
struct ClockReading {
    wall_secs: i64,
    tick_ms: i64,
    /// This session's `session::logon_token`; `None` if it can't be read.
    logon_token: Option<i64>,
}

/// What `check_clock` decided: the date "today" is, the anchor to store
/// (`None` to keep the stored one), and the tamper state to store.
#[derive(Debug, PartialEq, Eq)]
struct ClockCheck {
    date: String,
    anchor: Option<ClockAnchor>,
    tamper: ClockTamper,
}

const CLOCK_ANCHOR_WALL_KEY: &str = "clock_anchor_wall_secs";
const CLOCK_ANCHOR_TICK_KEY: &str = "clock_anchor_tick_ms";
const CLOCK_ANCHOR_DATE_KEY: &str = "clock_anchor_date";
const CLOCK_ANCHOR_LOGON_KEY: &str = "clock_anchor_logon_token";
const CLOCK_TAMPER_ACTIVE_KEY: &str = "clock_tamper_active";
const CLOCK_TAMPER_ALERTED_KEY: &str = "clock_tamper_alerted";
const CLOCK_TAMPER_DRIFT_KEY: &str = "clock_tamper_drift_secs";

/// Judge the wall clock against the stored anchor (see `effective_today_date`).
///
/// `GetTickCount64` only means something within one boot, and a reboot can't
/// be told from the tick alone: an app started later in the new boot than
/// the anchor was taken in the old one sees the tick still ahead of it, and
/// used to read the whole time it was not running as the clock being moved -
/// "today" stayed pinned to that old date from then on. So the anchor is
/// only compared within the logon session that took it, which lies within
/// one boot. In any other session (after a sign-out or a reboot, including a
/// Fast Startup one, which signs everyone out), for an anchor without a
/// session (an older version's),
/// with the tick gone backwards, or when this session can't be identified,
/// the clock is accepted as it is: re-anchored, and any tamper episode ended
/// without an alert - the same as the reboot case always was.
fn check_clock(anchor: Option<&ClockAnchor>, tamper: ClockTamper, now: ClockReading, today: &str) -> ClockCheck {
    let comparable = anchor.filter(|a| {
        now.logon_token.is_some() && a.logon_token == now.logon_token && now.tick_ms >= a.tick_ms
    });
    if let Some(a) = comparable {
        // Saturating: the anchor comes from the database, which can be edited.
        let expected_wall = a.wall_secs.saturating_add(now.tick_ms.saturating_sub(a.tick_ms) / 1000);
        let drift = now.wall_secs.saturating_sub(expected_wall);
        if drift.abs() > CLOCK_DRIFT_TOLERANCE_SECS {
            return ClockCheck {
                date: a.date.clone(),
                anchor: None,
                // A new episode is alerted on even if a stale "alerted" was left behind.
                tamper: ClockTamper { active: true, alerted: tamper.active && tamper.alerted, drift_secs: Some(drift) },
            };
        }
    }
    ClockCheck {
        date: today.to_string(),
        anchor: Some(ClockAnchor {
            wall_secs: now.wall_secs,
            tick_ms: now.tick_ms,
            date: today.to_string(),
            logon_token: now.logon_token,
        }),
        tamper: ClockTamper { active: false, alerted: false, drift_secs: tamper.drift_secs },
    }
}

/// The drift to alert the admin about, if a tamper episode is going on and
/// hasn't been alerted on yet.
fn pending_clock_tamper_alert(tamper: &ClockTamper) -> Option<i64> {
    if tamper.active && !tamper.alerted {
        tamper.drift_secs
    } else {
        None
    }
}

fn load_clock_anchor() -> Option<ClockAnchor> {
    let parse = |key: &str| get_setting(key).and_then(|s| s.parse::<i64>().ok());
    Some(ClockAnchor {
        wall_secs: parse(CLOCK_ANCHOR_WALL_KEY)?,
        tick_ms: parse(CLOCK_ANCHOR_TICK_KEY)?,
        date: get_setting(CLOCK_ANCHOR_DATE_KEY)?,
        logon_token: parse(CLOCK_ANCHOR_LOGON_KEY),
    })
}

/// The settings rows that store `anchor`.
fn clock_anchor_rows(anchor: &ClockAnchor) -> Vec<(&'static str, String)> {
    vec![
        (CLOCK_ANCHOR_WALL_KEY, anchor.wall_secs.to_string()),
        (CLOCK_ANCHOR_TICK_KEY, anchor.tick_ms.to_string()),
        (CLOCK_ANCHOR_DATE_KEY, anchor.date.clone()),
        (CLOCK_ANCHOR_LOGON_KEY, anchor.logon_token.map(|t| t.to_string()).unwrap_or_default()),
    ]
}

/// How long an accepted anchor may go without being rewritten. Below the
/// accounting tick's 30-second periodic check, so that check still
/// refreshes it; far below `CLOCK_DRIFT_TOLERANCE_SECS`, so comparing with
/// an anchor this old reaches the same verdict as with a fresh one.
const ANCHOR_REWRITE_MS: i64 = 25_000;

/// Whether the accepted anchor `new` must be written over `stored`: always
/// when the date or the logon session changed (or there's none stored, or
/// the tick went backwards), otherwise only once the stored one is
/// `ANCHOR_REWRITE_MS` old. While paused the date is looked up about twice a
/// second, and every lookup used to rewrite the anchor.
fn anchor_needs_write(stored: Option<&ClockAnchor>, new: &ClockAnchor) -> bool {
    let Some(stored) = stored else { return true };
    stored.date != new.date
        || stored.logon_token != new.logon_token
        || new.tick_ms < stored.tick_ms
        || new.tick_ms - stored.tick_ms >= ANCHOR_REWRITE_MS
}

fn load_clock_tamper() -> ClockTamper {
    ClockTamper {
        active: get_setting(CLOCK_TAMPER_ACTIVE_KEY).as_deref() == Some("1"),
        alerted: get_setting(CLOCK_TAMPER_ALERTED_KEY).as_deref() == Some("1"),
        drift_secs: get_setting(CLOCK_TAMPER_DRIFT_KEY).and_then(|s| s.parse().ok()),
    }
}

/// The rows for the parts of `new` that differ from `old` (what's stored now).
fn clock_tamper_rows(old: &ClockTamper, new: &ClockTamper) -> Vec<(&'static str, String)> {
    let flag = |on: bool| if on { "1" } else { "0" }.to_string();
    let mut rows = Vec::new();
    if new.active != old.active {
        rows.push((CLOCK_TAMPER_ACTIVE_KEY, flag(new.active)));
    }
    if new.alerted != old.alerted {
        rows.push((CLOCK_TAMPER_ALERTED_KEY, flag(new.alerted)));
    }
    if let Some(drift) = new.drift_secs.filter(|_| new.drift_secs != old.drift_secs) {
        rows.push((CLOCK_TAMPER_DRIFT_KEY, drift.to_string()));
    }
    rows
}

/// Write several settings rows in one transaction: all of them or none.
fn set_settings(rows: &[(&str, String)]) -> bool {
    if rows.is_empty() {
        return true;
    }
    let Ok(mut guard) = DB_CONNECTION.lock() else { return false };
    let Some(conn) = guard.as_mut() else { return false };
    set_settings_in(conn, rows).is_ok()
}

fn set_settings_in(conn: &mut Connection, rows: &[(&str, String)]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for (key, value) in rows {
        tx.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)", params![key, value])?;
    }
    tx.commit()
}

/// The date to use for all "today"-scoped enforcement state (remaining time,
/// pause allowance, session tracking), gated against system-clock manipulation.
///
/// Plain `get_today_date()` is exactly what a kid can move to get a fresh
/// daily allowance: change the date, then get the app to restart (kill it -
/// the watchdog relaunches it within a minute anyway) so the loaded state
/// finds no row for the "new" date and grants a full day. To catch that, a
/// monotonic anchor (wall-clock time + `GetTickCount64` uptime, which the
/// Date & Time settings can't move) is persisted alongside every save,
/// together with the logon session it was taken in. If, in that same
/// session, the real wall clock ever strays more than
/// `CLOCK_DRIFT_TOLERANCE_SECS` from where the anchor says it should be,
/// that's not elapsed time - it's the clock being moved - so "today" stays
/// pinned to the last known-good date until the real clock drifts back
/// within tolerance on its own. In any other session the clock is accepted
/// (see `check_clock`).
///
/// A time-zone change moves the local date without moving the clock, so the
/// anchor can't see it; the date this starts from is already the one in the
/// time zone the PC is supposed to be in (`remembered_zone_today_date`), so
/// a changed zone - before or after a reboot - moves nothing here.
pub fn effective_today_date() -> String {
    let today = remembered_zone_today_date();
    let now = ClockReading {
        wall_secs: wall_clock_now_secs(),
        tick_ms: monotonic_now_ms(),
        // 0 isn't a logon time: treat it as unknown, like the watchdog does.
        logon_token: crate::session::logon_token().filter(|&t| t != 0),
    };
    let tamper = load_clock_tamper();
    let stored = load_clock_anchor();
    let check = check_clock(stored.as_ref(), tamper, now, &today);
    // One transaction, so a kill can't leave half an anchor behind (say the
    // new wall time with the old tick, which reads as the clock moved).
    let mut rows = match &check.anchor {
        Some(anchor) if anchor_needs_write(stored.as_ref(), anchor) => clock_anchor_rows(anchor),
        _ => Vec::new(),
    };
    rows.extend(clock_tamper_rows(&tamper, &check.tamper));
    set_settings(&rows);
    check.date
}

/// Returns the drift (signed seconds) the first time a clock-tamper episode
/// is observed, so callers can send exactly one alert per episode instead of
/// re-notifying on every periodic save while the clock stays off. `None` both
/// while no episode is active and after one has already been alerted on.
pub fn take_pending_clock_tamper_alert() -> Option<i64> {
    let drift = pending_clock_tamper_alert(&load_clock_tamper())?;
    set_setting(CLOCK_TAMPER_ALERTED_KEY, "1");
    Some(drift)
}

/// The date the running app's day counters (remaining time, active time -
/// `blocking::REMAINING_SECONDS`, `mini_overlay::SESSION_ACTIVE_SECONDS`)
/// belong to: the date they were loaded for at startup, then each new day's
/// at the rollover (`mini_overlay::check_day_rollover`). `None` until
/// startup loads them.
///
/// Saves of those counters go under this date, not under "today" as worked
/// out at the moment of the save: between midnight and the rollover that
/// follows it (about a second, up to 30 while the time zone differs from the
/// remembered one) that used to file the old day's numbers under the new
/// day, and the rollover then loaded them as the new day's allowance - a 0
/// saved that way locked the new day.
static COUNTERS_DATE: Mutex<Option<String>> = Mutex::new(None);

/// Run `f` with the day counters' date held, so a save can't interleave
/// with the rollover switching the counters to the next day. Never call
/// back into anything that takes this lock (`f` runs with it held).
pub fn with_counters_date<R>(f: impl FnOnce(&mut Option<String>) -> R) -> R {
    let mut guard = COUNTERS_DATE.lock().unwrap();
    f(&mut guard)
}

/// The date to save the day counters under: the date they belong to, or
/// (before startup has loaded them) today's.
pub fn counters_save_date(tracked: Option<&str>, today: impl FnOnce() -> String) -> String {
    tracked.map(str::to_string).unwrap_or_else(today)
}

/// Save the remaining time for `date`.
pub fn save_remaining_time_for(date: &str, seconds: i32) {
    set_setting(&format!("remaining_time_{}", date), &seconds.to_string());
}

/// The remaining time saved for `date`, if any.
pub fn load_remaining_time_for(date: &str) -> Option<i32> {
    get_setting(&format!("remaining_time_{}", date)).and_then(|s| s.parse().ok())
}

/// The daily limit (minutes) that `date`'s remaining time currently
/// reflects - what a later change of the day's limit is measured against
/// (see `blocking::apply_todays_limit_change`).
pub fn get_applied_limit(date: &str) -> Option<u32> {
    get_setting(&format!("limit_applied_{}", date)).and_then(|s| s.parse().ok())
}

pub fn set_applied_limit(date: &str, minutes: u32) {
    set_setting(&format!("limit_applied_{}", date), &minutes.to_string());
}

/// Get the current weekday (0 = Monday, 6 = Sunday) - of the day in the time
/// zone the PC is supposed to be in (see `remembered_zone_today_date`), so a
/// changed time zone can't pick the next day's limit either.
#[allow(dead_code)]
pub fn get_current_weekday() -> u32 {
    weekday_of_date(&remembered_zone_today_date()).unwrap_or_else(local_weekday)
}

/// Weekday (0 = Monday, 6 = Sunday) of a "YYYY-MM-DD" date.
fn weekday_of_date(date: &str) -> Option<u32> {
    let parts: Vec<i64> = date.splitn(3, '-').filter_map(|p| p.parse().ok()).collect();
    let [y, m, d] = parts[..] else { return None };
    // 1970-01-01 was a Thursday (3, counting from Monday = 0).
    Some((days_from_civil(y, m, d) + 3).rem_euclid(7) as u32)
}

/// Weekday (0 = Monday, 6 = Sunday) by the local clock.
fn local_weekday() -> u32 {
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

/// The shortest lock-screen timeout. Settings used to accept 0 minutes, which
/// shut the PC down a second after every block (including at each logon with
/// no time left); anything shorter that is already stored is read as one
/// minute - negative values too (the app never writes one), so there is no
/// "never shut down" value: the lock screen's countdown treats a negative
/// count as "no countdown running", and always starts from this function.
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
    get_pause_used_for(&effective_today_date())
}

/// Pause time used on `date` (in seconds)
pub fn get_pause_used_for(date: &str) -> i32 {
    let key = format!("pause_used_{}", date);
    get_setting(&key)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Save the pause time used on `date` (in seconds)
pub fn save_pause_used_for(date: &str, seconds: i32) {
    let key = format!("pause_used_{}", date);
    set_setting(&key, &seconds.to_string());
    mirror_shared_daily_stat("pause", date, seconds);
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

/// The time actively used on `date` (in seconds)
pub fn get_session_active_time_for(date: &str) -> i32 {
    let key = format!("session_active_{}", date);
    get_setting(&key)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Save the time actively used on `date` (in seconds)
pub fn save_session_active_time_for(date: &str, seconds: i32) {
    let key = format!("session_active_{}", date);
    set_setting(&key, &seconds.to_string());
    mirror_shared_daily_stat("active", date, seconds);
}

/// Log a pause event under `date` - the date its time was counted under
/// (`save_pause_used_for`), so the log and the usage always agree. (The log
/// used the raw local date, while usage follows `effective_today_date`.)
pub fn log_pause_event_for(date: &str, duration_seconds: i32) {
    use windows::Win32::System::SystemInformation::GetLocalTime;

    let st = unsafe { GetLocalTime() };
    let time_str = format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond);

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

/// Get pause log for today (the same date as `get_pause_used_today`)
pub fn get_pause_log_today() -> Vec<String> {
    let date = effective_today_date();
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
    idle_enabled_from(get_setting("idle_enabled").as_deref())
}

/// Idle auto-pause is on unless "off" was saved: a missing or empty value
/// (a new install, a recreated database) means on, like the first-run
/// default. Settings saves "1" or "0".
fn idle_enabled_from(stored: Option<&str>) -> bool {
    match stored.map(str::trim) {
        None | Some("") => true,
        Some(value) => value == "1",
    }
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
        channel_id: get_shared_setting(DISCORD_CHANNEL_ID).and_then(|s| parse_discord_id(&s)),
        admin_user_id: get_shared_setting(DISCORD_ADMIN_USER_ID).and_then(|s| parse_discord_id(&s)),
        enabled: get_shared_setting(DISCORD_ENABLED)
            .map(|s| s == "true")
            .unwrap_or(false),
    }
}

/// A stored Discord ID. Never 0: serenity's `ChannelId::new`/`UserId::new`
/// panic on 0, and a panic aborts the app (release builds use
/// `panic = "abort"`), so "0" typed into Settings counts as not configured.
fn parse_discord_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|&id| id != 0)
}

#[cfg(test)]
mod discord_id_tests {
    use super::parse_discord_id;

    #[test]
    fn zero_and_junk_are_not_ids() {
        assert_eq!(parse_discord_id("123456789012345678"), Some(123_456_789_012_345_678));
        assert_eq!(parse_discord_id("0"), None);
        assert_eq!(parse_discord_id("000"), None);
        assert_eq!(parse_discord_id(""), None);
        assert_eq!(parse_discord_id("-5"), None);
        assert_eq!(parse_discord_id("abc"), None);
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
    if is_rotating_pin_enabled() && rotating_pin().is_some_and(|pin| entered == pin) {
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

/// Remaining time for a day whose limit is `limit_secs` and of which
/// `active_secs` have already been used. Extensions granted earlier that day
/// aren't known, so this can come out lower than it really was - never
/// higher.
fn remaining_after_use(limit_secs: i32, active_secs: i32) -> i32 {
    limit_secs.saturating_sub(active_secs.max(0)).max(0)
}

/// What `restore_today_from_shared_mirror` sets for the day.
#[derive(Debug, PartialEq, Eq)]
struct MirrorRestore {
    active: i32,
    paused: i32,
    remaining: i32,
}

/// Whether and how to start a day from the shared mirror: only when the day
/// has no remaining time stored but the mirror shows use. The used and
/// paused times are the larger of what's stored here and in the mirror (the
/// two are written together, so normally equal) - never added, and never
/// lowered - and the remaining time is the limit minus the time used.
fn mirror_restore(
    stored_remaining: Option<i32>,
    local: (i32, i32),
    mirrored: (i32, i32),
    limit_secs: i32,
) -> Option<MirrorRestore> {
    if stored_remaining.is_some() || (mirrored.0 <= 0 && mirrored.1 <= 0) {
        return None;
    }
    let active = local.0.max(mirrored.0).max(0);
    let paused = local.1.max(mirrored.1).max(0);
    Some(MirrorRestore { active, paused, remaining: remaining_after_use(limit_secs, active) })
}

/// At startup, if `date` has no remaining time stored but this account's
/// usage as mirrored in the shared database (see `mirror_shared_daily_stat`,
/// refreshed every 30 seconds) shows time used that day, start the day from
/// that usage instead of from nothing: the used time and pause time are
/// carried over, and the remaining time is the day's limit minus the time
/// used. So deleting or damaging the settings file can't be used to get a
/// fresh day's allowance (or pause budget), and the mirror isn't
/// overwritten with smaller numbers. Returns the remaining time it set, or
/// `None` if there was nothing to carry over (a fresh day, or one that
/// already has a remaining time).
pub fn restore_today_from_shared_mirror(date: &str) -> Option<i32> {
    let username = crate::remote_commands::current_windows_username();
    let mirrored = |kind: &str| {
        get_shared_setting(&shared_user_stat_key(kind, &username, date))
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0)
    };
    let limit_minutes = get_daily_limit(get_current_weekday());
    let restore = mirror_restore(
        load_remaining_time_for(date),
        (get_session_active_time_for(date), get_pause_used_for(date)),
        (mirrored("active"), mirrored("pause")),
        (limit_minutes * 60) as i32,
    )?;

    save_session_active_time_for(date, restore.active);
    save_pause_used_for(date, restore.paused);
    save_remaining_time_for(date, restore.remaining);
    set_applied_limit(date, limit_minutes);
    Some(restore.remaining)
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

/// The time zone the PC is supposed to be in (`TimeZone::serialize`), kept
/// machine-wide in the shared database. "Today" always follows this zone -
/// see `remembered_zone_today_date`. It's recorded when there is none yet
/// (first run, including the first run after upgrading from a version
/// without it) and replaced only by saving Settings, which is behind the
/// passcode (`adopt_current_time_zone`) - never by a reboot or a restart.
/// (Replaces the old `clock_anchor_utc_offset_mins` baseline, which was
/// re-recorded at every reboot, so "change the zone, then reboot" worked,
/// and which let a zone change of up to an hour through as daylight saving.)
const REMEMBERED_TIME_ZONE_KEY: &str = "time_zone_remembered";

/// The changed time zone the admin was last alerted about
/// (`TimeZone::identity`), or "" while the PC is in the remembered zone - so
/// each change is alerted once, not on every check or every start, and only
/// by one of the accounts running the app.
const TIME_ZONE_ALERTED_KEY: &str = "time_zone_alerted";

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

/// "YYYY-MM-DD" of a SYSTEMTIME.
fn systemtime_date(st: &windows::Win32::Foundation::SYSTEMTIME) -> String {
    format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay)
}

/// The kernel32 time-zone calls, declared here because the `windows` crate is
/// built without its `Win32_System_Time` feature.
mod tz_api {
    use windows::Win32::Foundation::{BOOL, SYSTEMTIME};

    /// TIME_ZONE_INFORMATION.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct TimeZoneInformation {
        pub bias: i32,
        pub standard_name: [u16; 32],
        pub standard_date: SYSTEMTIME,
        pub standard_bias: i32,
        pub daylight_name: [u16; 32],
        pub daylight_date: SYSTEMTIME,
        pub daylight_bias: i32,
    }

    /// DYNAMIC_TIME_ZONE_INFORMATION, which starts with the same fields.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct DynamicTimeZoneInformation {
        pub tzi: TimeZoneInformation,
        pub time_zone_key_name: [u16; 128],
        pub dynamic_daylight_time_disabled: u8,
    }

    pub const TIME_ZONE_ID_INVALID: u32 = u32::MAX;

    #[allow(non_snake_case)]
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetDynamicTimeZoneInformation(info: *mut DynamicTimeZoneInformation) -> u32;
        pub fn SystemTimeToTzSpecificLocalTime(
            zone: *const TimeZoneInformation,
            utc: *const SYSTEMTIME,
            local: *mut SYSTEMTIME,
        ) -> BOOL;
    }
}

/// A Windows time zone: what identifies it, and its rules, so the local date
/// in it can be worked out after the PC has moved to another zone.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TimeZone {
    /// Windows' key name, e.g. "W. Europe Standard Time" (can be empty).
    key: String,
    /// UTC minus local standard time, in minutes (Windows' `Bias`: -60 for
    /// Central European Time, 210 for Newfoundland, -345 for Nepal).
    bias: i32,
    /// "Adjust for daylight saving time automatically" is off.
    dst_disabled: bool,
    standard_bias: i32,
    daylight_bias: i32,
    /// When standard and daylight time begin (SYSTEMTIME fields in order,
    /// Windows' rule format). Month 0 means no daylight saving time.
    standard_date: [u16; 8],
    daylight_date: [u16; 8],
}

/// Biases further than a day from UTC aren't real zones (an edited value).
const MAX_ZONE_BIAS_MINS: i32 = 24 * 60;

fn systemtime_fields(st: &windows::Win32::Foundation::SYSTEMTIME) -> [u16; 8] {
    [st.wYear, st.wMonth, st.wDayOfWeek, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds]
}

fn systemtime_from_fields(f: [u16; 8]) -> windows::Win32::Foundation::SYSTEMTIME {
    windows::Win32::Foundation::SYSTEMTIME {
        wYear: f[0], wMonth: f[1], wDayOfWeek: f[2], wDay: f[3],
        wHour: f[4], wMinute: f[5], wSecond: f[6], wMilliseconds: f[7],
    }
}

/// "UTC+05:45", "UTC-03:30", "UTC+00:00" for an offset in minutes.
fn format_utc_offset(offset_mins: i32) -> String {
    let sign = if offset_mins < 0 { '-' } else { '+' };
    let m = offset_mins.unsigned_abs();
    format!("UTC{}{:02}:{:02}", sign, m / 60, m % 60)
}

/// A time zone as named in the time-zone alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeZoneLabel {
    /// Key name and standard offset, e.g. "Pacific Standard Time (UTC-08:00)".
    pub name: String,
    /// Automatic daylight saving time is switched off in it.
    pub dst_off: bool,
}

impl TimeZone {
    fn from_api(info: &tz_api::DynamicTimeZoneInformation) -> Self {
        let key_len = info.time_zone_key_name.iter().position(|&c| c == 0).unwrap_or(info.time_zone_key_name.len());
        TimeZone {
            key: String::from_utf16_lossy(&info.time_zone_key_name[..key_len]),
            bias: info.tzi.bias,
            dst_disabled: info.dynamic_daylight_time_disabled != 0,
            standard_bias: info.tzi.standard_bias,
            daylight_bias: info.tzi.daylight_bias,
            standard_date: systemtime_fields(&info.tzi.standard_date),
            daylight_date: systemtime_fields(&info.tzi.daylight_date),
        }
    }

    /// What tells one zone from another: its name, its standard offset and
    /// whether automatic DST is off. A daylight-saving transition changes
    /// none of these - it changes only the offset in effect - so it's never
    /// taken for a zone change, while every real zone change is, including
    /// one to a zone just an hour away and turning automatic DST on or off.
    fn identity(&self) -> String {
        format!("{}|{}|{}", self.bias, u8::from(self.dst_disabled), self.key)
    }

    fn same_zone(&self, other: &TimeZone) -> bool {
        self.identity() == other.identity()
    }

    fn serialize(&self) -> String {
        let fields = |f: &[u16; 8]| f.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(",");
        format!(
            "1|{}|{}|{}|{}|{}|{}|{}",
            self.bias,
            u8::from(self.dst_disabled),
            self.standard_bias,
            self.daylight_bias,
            fields(&self.standard_date),
            fields(&self.daylight_date),
            self.key,
        )
    }

    /// Inverse of `serialize`; `None` for anything malformed or absurd.
    fn parse(value: &str) -> Option<Self> {
        fn fields(s: &str) -> Option<[u16; 8]> {
            let parsed: Vec<u16> = s.split(',').map(|v| v.parse().ok()).collect::<Option<_>>()?;
            parsed.try_into().ok()
        }
        let mut parts = value.splitn(8, '|');
        if parts.next()? != "1" {
            return None;
        }
        let bias: i32 = parts.next()?.parse().ok()?;
        let dst_disabled = match parts.next()? {
            "0" => false,
            "1" => true,
            _ => return None,
        };
        let standard_bias: i32 = parts.next()?.parse().ok()?;
        let daylight_bias: i32 = parts.next()?.parse().ok()?;
        let standard_date = fields(parts.next()?)?;
        let daylight_date = fields(parts.next()?)?;
        let key = parts.next()?.to_string();
        let sane = [bias, standard_bias, daylight_bias].iter().all(|b| b.abs() <= MAX_ZONE_BIAS_MINS)
            && (bias + standard_bias).abs() <= MAX_ZONE_BIAS_MINS
            && (bias + daylight_bias).abs() <= MAX_ZONE_BIAS_MINS;
        sane.then_some(TimeZone { key, bias, dst_disabled, standard_bias, daylight_bias, standard_date, daylight_date })
    }

    fn label(&self) -> TimeZoneLabel {
        let offset = format_utc_offset(-(self.bias + self.standard_bias));
        let name = if self.key.is_empty() { offset } else { format!("{} ({})", self.key, offset) };
        TimeZoneLabel { name, dst_off: self.dst_disabled }
    }

    fn to_api(&self) -> tz_api::TimeZoneInformation {
        // No daylight saving time when it's switched off: month 0 in both.
        let (standard_date, daylight_date) = if self.dst_disabled {
            ([0; 8], [0; 8])
        } else {
            (self.standard_date, self.daylight_date)
        };
        tz_api::TimeZoneInformation {
            bias: self.bias,
            standard_name: [0; 32],
            standard_date: systemtime_from_fields(standard_date),
            standard_bias: self.standard_bias,
            daylight_name: [0; 32],
            daylight_date: systemtime_from_fields(daylight_date),
            daylight_bias: self.daylight_bias,
        }
    }

    /// The local date in this zone at the UTC instant `utc`, daylight saving
    /// included. Falls back to standard time if Windows rejects the rules.
    fn date_at(&self, utc: &windows::Win32::Foundation::SYSTEMTIME) -> String {
        let zone = self.to_api();
        let mut local = windows::Win32::Foundation::SYSTEMTIME::default();
        if unsafe { tz_api::SystemTimeToTzSpecificLocalTime(&zone, utc, &mut local) }.as_bool() {
            return systemtime_date(&local);
        }
        let local_secs = systemtime_secs(utc) - i64::from(self.bias + self.standard_bias) * 60;
        let (y, m, d) = civil_from_days(local_secs.div_euclid(86400));
        format!("{:04}-{:02}-{:02}", y, m, d)
    }
}

/// The time zone Windows is in now, or `None` if it won't say.
fn current_time_zone() -> Option<TimeZone> {
    let mut info: tz_api::DynamicTimeZoneInformation = unsafe { std::mem::zeroed() };
    let result = unsafe { tz_api::GetDynamicTimeZoneInformation(&mut info) };
    (result != tz_api::TIME_ZONE_ID_INVALID).then(|| TimeZone::from_api(&info))
}

fn remembered_time_zone() -> Option<TimeZone> {
    get_shared_setting(REMEMBERED_TIME_ZONE_KEY).and_then(|s| TimeZone::parse(&s))
}

/// Today's date, given the remembered zone, the zone Windows is in now and
/// the date by the local clock - and whether to (re)store the current zone
/// as the remembered one.
#[derive(Debug, PartialEq, Eq)]
struct ZoneDay {
    date: String,
    record_current: bool,
}

/// While the PC is in the remembered zone, today is the local date. While it
/// isn't - a child ran `tzutil /s` to borrow tomorrow, or the family is
/// travelling - today is the date in the *remembered* zone at the same UTC
/// instant: the day still rolls over at midnight there, and changing the
/// zone, rebooting or both gains and loses nothing. Nothing is frozen, so
/// midnight still comes. With no remembered zone yet (first run, or first
/// run after an upgrade) the current one is recorded silently; while it's
/// the same zone, newer rules for it (a Windows update) are recorded too.
fn zone_day(
    remembered: Option<&TimeZone>,
    current: Option<&TimeZone>,
    utc: &windows::Win32::Foundation::SYSTEMTIME,
    local_date: &str,
) -> ZoneDay {
    match (remembered, current) {
        (Some(r), Some(c)) if r.same_zone(c) => ZoneDay { date: local_date.to_string(), record_current: r != c },
        // Moved to another zone, or the current one can't be read.
        (Some(r), _) => ZoneDay { date: r.date_at(utc), record_current: false },
        (None, c) => ZoneDay { date: local_date.to_string(), record_current: c.is_some() },
    }
}

/// Today's date in the time zone the PC is supposed to be in - see
/// `zone_day`. The starting point of `effective_today_date`.
fn remembered_zone_today_date() -> String {
    use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
    let (local, utc) = unsafe { (GetLocalTime(), GetSystemTime()) };
    let current = current_time_zone();
    let day = zone_day(remembered_time_zone().as_ref(), current.as_ref(), &utc, &systemtime_date(&local));
    if day.record_current {
        if let Some(current) = &current {
            set_shared_setting(REMEMBERED_TIME_ZONE_KEY, &current.serialize());
        }
    }
    day.date
}

/// Whether to alert the admin now that the PC is in `current` rather than
/// `remembered`: once per change. `stored` is TIME_ZONE_ALERTED_KEY's value;
/// `swap` stores a new one and says whether that changed it, so when several
/// accounts' apps see the same change only one of them alerts.
fn claim_time_zone_alert(
    remembered: &TimeZone,
    current: &TimeZone,
    stored: Option<&str>,
    swap: impl FnOnce(&str) -> bool,
) -> bool {
    let marker = if remembered.same_zone(current) { String::new() } else { current.identity() };
    if stored.unwrap_or("") == marker {
        return false;
    }
    swap(&marker) && !marker.is_empty()
}

/// The remembered and the current time zone, the first time the PC is seen
/// in a zone other than the remembered one (see `claim_time_zone_alert`).
/// `None` otherwise, including before a zone has been remembered.
pub fn take_pending_time_zone_alert() -> Option<(TimeZoneLabel, TimeZoneLabel)> {
    let remembered = remembered_time_zone()?;
    let current = current_time_zone()?;
    let stored = get_shared_setting(TIME_ZONE_ALERTED_KEY);
    let alert = claim_time_zone_alert(&remembered, &current, stored.as_deref(), |marker| {
        swap_shared_setting(TIME_ZONE_ALERTED_KEY, marker).is_some()
    });
    alert.then(|| (remembered.label(), current.label()))
}

/// Make the zone Windows is in now the one the PC is supposed to be in.
/// Called when Settings is saved (behind the passcode) - the way a parent
/// accepts a time-zone change, as the alert tells them. If the new zone's
/// date differs from the remembered zone's, the day changes with it at the
/// next rollover check.
pub fn adopt_current_time_zone() {
    let Some(current) = current_time_zone() else { return };
    set_shared_setting(REMEMBERED_TIME_ZONE_KEY, &current.serialize());
    set_shared_setting(TIME_ZONE_ALERTED_KEY, "");
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

/// Longest in-flight pause that's believed: a day. A pause can't really run
/// longer than its configured maximum; anything bigger is an edited value.
const MAX_INFLIGHT_PAUSE_SECS: i32 = 24 * 60 * 60;

/// Latest believable save time: 9999-12-31 23:59:59 UTC. Bounding it keeps
/// the arithmetic on it from overflowing (an edited value used to panic in a
/// debug build and wrap in release).
const MAX_INFLIGHT_SAVED_AT: i64 = 253_402_300_799;

/// Parse a `PAUSE_INFLIGHT_KEY` value: (date, seconds, saved-at epoch secs).
/// `None` for anything malformed or out of range.
fn parse_inflight_pause(value: &str) -> Option<(String, i32, i64)> {
    let mut parts = value.splitn(3, '|');
    let date = parts.next()?;
    let seconds: i32 = parts.next()?.parse().ok()?;
    let saved_at: i64 = parts.next()?.parse().ok()?;
    let date_ok = date.len() == 10
        && date.bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 { b == b'-' } else { b.is_ascii_digit() });
    let seconds_ok = (1..=MAX_INFLIGHT_PAUSE_SECS).contains(&seconds);
    let saved_at_ok = (0..=MAX_INFLIGHT_SAVED_AT).contains(&saved_at);
    (date_ok && seconds_ok && saved_at_ok).then(|| (date.to_string(), seconds, saved_at))
}

/// Seconds into the local day at epoch second `epoch_secs`, for a UTC offset
/// in minutes.
fn local_secs_of_day(epoch_secs: i64, offset_mins: i64) -> i64 {
    epoch_secs.saturating_add(offset_mins.saturating_mul(60)).rem_euclid(86400)
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
    let local = local_secs_of_day(saved_at, current_utc_offset_mins());
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

    // ----- Time zones -----

    /// US rules: DST from the 2nd Sunday of March 02:00 to the 1st Sunday of
    /// November 02:00. EU rules: last Sunday of March to last Sunday of October.
    const US_STANDARD: [u16; 8] = [0, 11, 0, 1, 2, 0, 0, 0];
    const US_DAYLIGHT: [u16; 8] = [0, 3, 0, 2, 2, 0, 0, 0];

    fn zone(key: &str, bias: i32, rules: Option<([u16; 8], [u16; 8])>) -> TimeZone {
        let (standard_date, daylight_date, daylight_bias) = match rules {
            Some((s, d)) => (s, d, -60),
            None => ([0; 8], [0; 8], 0),
        };
        TimeZone {
            key: key.to_string(),
            bias,
            dst_disabled: false,
            standard_bias: 0,
            daylight_bias,
            standard_date,
            daylight_date,
        }
    }
    fn pacific() -> TimeZone { zone("Pacific Standard Time", 480, Some((US_STANDARD, US_DAYLIGHT))) }
    fn eastern() -> TimeZone { zone("Eastern Standard Time", 300, Some((US_STANDARD, US_DAYLIGHT))) }
    fn line_islands() -> TimeZone { zone("Line Islands Standard Time", -840, None) }
    fn central_europe() -> TimeZone {
        zone("W. Europe Standard Time", -60, Some(([0, 10, 0, 5, 3, 0, 0, 0], [0, 3, 0, 5, 2, 0, 0, 0])))
    }
    fn eastern_europe() -> TimeZone {
        zone("GTB Standard Time", -120, Some(([0, 10, 0, 5, 4, 0, 0, 0], [0, 3, 0, 5, 3, 0, 0, 0])))
    }
    fn arabia() -> TimeZone { zone("Arab Standard Time", -180, None) }
    fn nepal() -> TimeZone { zone("Nepal Standard Time", -345, None) }
    fn india() -> TimeZone { zone("India Standard Time", -330, None) }
    fn newfoundland() -> TimeZone { zone("Newfoundland Standard Time", 210, Some((US_STANDARD, US_DAYLIGHT))) }

    fn utc(y: u16, mo: u16, d: u16, h: u16, mi: u16) -> windows::Win32::Foundation::SYSTEMTIME {
        // Sunday = 0; 1970-01-01 was a Thursday.
        let dow = (days_from_civil(y as i64, mo as i64, d as i64) + 4).rem_euclid(7) as u16;
        systemtime_from_fields([y, mo, dow, d, h, mi, 0, 0])
    }

    /// "Today" with `remembered` remembered while Windows is in `current`.
    fn today(remembered: &TimeZone, current: &TimeZone, at: &windows::Win32::Foundation::SYSTEMTIME) -> String {
        let local = current.date_at(at);
        zone_day(Some(remembered), Some(current), at, &local).date
    }

    #[test]
    fn time_zone_api_structs_match_windows() {
        assert_eq!(std::mem::size_of::<tz_api::TimeZoneInformation>(), 172);
        assert_eq!(std::mem::size_of::<tz_api::DynamicTimeZoneInformation>(), 432);
    }

    #[test]
    fn dates_in_a_zone_follow_its_daylight_saving_rules() {
        // Pacific, winter and summer, either side of midnight
        assert_eq!(pacific().date_at(&utc(2026, 11, 16, 7, 30)), "2026-11-15"); // 23:30 PST
        assert_eq!(pacific().date_at(&utc(2026, 11, 16, 8, 30)), "2026-11-16"); // 00:30 PST
        assert_eq!(pacific().date_at(&utc(2026, 7, 2, 6, 30)), "2026-07-01"); // 23:30 PDT
        assert_eq!(pacific().date_at(&utc(2026, 7, 2, 7, 30)), "2026-07-02"); // 00:30 PDT
        // The days DST starts (8 March 2026) and ends (1 November 2026)
        assert_eq!(pacific().date_at(&utc(2026, 3, 9, 7, 30)), "2026-03-09"); // 00:30 PDT, 23:30 PST
        assert_eq!(pacific().date_at(&utc(2026, 11, 2, 7, 30)), "2026-11-01"); // 23:30 PST, 00:30 PDT
        // Central Europe
        assert_eq!(central_europe().date_at(&utc(2026, 1, 1, 22, 30)), "2026-01-01"); // 23:30 CET
        assert_eq!(central_europe().date_at(&utc(2026, 7, 1, 22, 30)), "2026-07-02"); // 00:30 CEST
    }

    #[test]
    fn half_and_three_quarter_hour_zones_work() {
        // Nepal +5:45
        assert_eq!(nepal().date_at(&utc(2026, 11, 15, 18, 10)), "2026-11-15"); // 23:55
        assert_eq!(nepal().date_at(&utc(2026, 11, 15, 18, 20)), "2026-11-16"); // 00:05
        // Newfoundland -3:30, and -2:30 in summer
        assert_eq!(newfoundland().date_at(&utc(2026, 11, 16, 3, 25)), "2026-11-15"); // 23:55 NST
        assert_eq!(newfoundland().date_at(&utc(2026, 11, 16, 3, 35)), "2026-11-16"); // 00:05 NST
        assert_eq!(newfoundland().date_at(&utc(2026, 7, 16, 2, 25)), "2026-07-15"); // 23:55 NDT
        assert_eq!(newfoundland().date_at(&utc(2026, 7, 16, 2, 35)), "2026-07-16"); // 00:05 NDT
        // Remembered Nepal, now India (+5:30): the day still follows Nepal
        assert_eq!(today(&nepal(), &india(), &utc(2026, 11, 15, 18, 20)), "2026-11-16");
        assert_eq!(today(&nepal(), &india(), &utc(2026, 11, 15, 18, 10)), "2026-11-15");
        assert_eq!(nepal().label().name, "Nepal Standard Time (UTC+05:45)");
        assert_eq!(newfoundland().label().name, "Newfoundland Standard Time (UTC-03:30)");
        assert_eq!(format_utc_offset(0), "UTC+00:00");
    }

    #[test]
    fn changing_the_zone_and_rebooting_gains_nothing() {
        // 22:10 in California with no time left; tzutil to UTC+14 makes it
        // 20:10 *tomorrow* locally. Today stays the 15th - the remembered zone
        // is stored, and nothing at a reboot or restart replaces it.
        let at = utc(2026, 11, 16, 6, 10);
        assert_eq!(line_islands().date_at(&at), "2026-11-16");
        let day = zone_day(Some(&pacific()), Some(&line_islands()), &at, "2026-11-16");
        assert_eq!(day, ZoneDay { date: "2026-11-15".into(), record_current: false });
        // ...and the day still rolls over at midnight in California
        assert_eq!(today(&pacific(), &line_islands(), &utc(2026, 11, 16, 7, 59)), "2026-11-15");
        assert_eq!(today(&pacific(), &line_islands(), &utc(2026, 11, 16, 8, 0)), "2026-11-16");
        // Moving the zone backwards doesn't take a day away either
        assert_eq!(today(&line_islands(), &pacific(), &utc(2026, 11, 16, 6, 10)), "2026-11-16");
    }

    #[test]
    fn travelling_keeps_the_day_rolling_until_the_new_zone_is_accepted() {
        // Flew from California to New York: midnight is at 03:00 local now
        assert_eq!(today(&pacific(), &eastern(), &utc(2026, 11, 16, 5, 30)), "2026-11-15"); // 00:30 EST
        assert_eq!(today(&pacific(), &eastern(), &utc(2026, 11, 16, 8, 30)), "2026-11-16"); // 03:30 EST
        // Accepted (Settings saved): the local date again
        let at = utc(2026, 11, 16, 5, 30);
        let day = zone_day(Some(&eastern()), Some(&eastern()), &at, "2026-11-16");
        assert_eq!(day, ZoneDay { date: "2026-11-16".into(), record_current: false });
    }

    #[test]
    fn daylight_saving_transitions_are_not_zone_changes() {
        // Windows reports the same zone all year: the offset in effect changes,
        // the identity doesn't, so the local date is used.
        for at in [utc(2026, 3, 9, 7, 30), utc(2026, 11, 2, 7, 30), utc(2026, 7, 2, 7, 30)] {
            let local = pacific().date_at(&at);
            let day = zone_day(Some(&pacific()), Some(&pacific()), &at, &local);
            assert_eq!(day, ZoneDay { date: local, record_current: false });
        }
        // New rules for the same zone (a Windows update) are recorded quietly
        let mut updated = pacific();
        updated.daylight_date = [0, 3, 0, 3, 2, 0, 0, 0];
        assert!(pacific().same_zone(&updated));
        let day = zone_day(Some(&pacific()), Some(&updated), &utc(2026, 7, 2, 7, 30), "2026-07-02");
        assert_eq!(day, ZoneDay { date: "2026-07-02".into(), record_current: true });
    }

    #[test]
    fn a_zone_an_hour_away_does_not_end_the_day_early() {
        // 23:10 in Berlin, switched to Athens (+1h): it's 00:10 there
        let winter = utc(2026, 11, 15, 22, 10);
        assert_eq!(eastern_europe().date_at(&winter), "2026-11-16");
        assert!(!central_europe().same_zone(&eastern_europe()));
        assert_eq!(today(&central_europe(), &eastern_europe(), &winter), "2026-11-15");
        // 23:10 in Berlin in summer (UTC+2), switched to UTC+3
        let summer = utc(2026, 7, 15, 21, 10);
        assert_eq!(arabia().date_at(&summer), "2026-07-16");
        assert_eq!(today(&central_europe(), &arabia(), &summer), "2026-07-15");
        // Switching automatic daylight saving time on in summer when it was off
        let mut no_dst = central_europe();
        no_dst.dst_disabled = true;
        assert!(!no_dst.same_zone(&central_europe()));
        let at = utc(2026, 7, 15, 22, 30); // 23:30 without DST, 00:30 with it
        assert_eq!(no_dst.date_at(&at), "2026-07-15");
        assert_eq!(today(&no_dst, &central_europe(), &at), "2026-07-15");
    }

    #[test]
    fn a_missing_remembered_zone_is_recorded_silently() {
        let at = utc(2026, 11, 16, 6, 10);
        // First run, or the first run after upgrading: the local date, and
        // the current zone is stored...
        let day = zone_day(None, Some(&line_islands()), &at, "2026-11-16");
        assert_eq!(day, ZoneDay { date: "2026-11-16".into(), record_current: true });
        // ...with nothing to alert about (take_pending_time_zone_alert needs
        // a remembered zone)
        assert!(!claim_time_zone_alert(&line_islands(), &line_islands(), None, |_| true));
        // Windows won't say: the local date with nothing remembered...
        assert_eq!(zone_day(None, None, &at, "2026-11-16"), ZoneDay { date: "2026-11-16".into(), record_current: false });
        // ...the remembered zone's date otherwise
        assert_eq!(zone_day(Some(&pacific()), None, &at, "2026-11-16").date, "2026-11-15");
    }

    #[test]
    fn each_zone_change_is_alerted_once() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)", []).unwrap();
        let mut check = |remembered: &TimeZone, current: &TimeZone| {
            let stored: Option<String> = conn
                .query_row("SELECT value FROM settings WHERE key = ?1", params![TIME_ZONE_ALERTED_KEY], |r| r.get(0))
                .ok();
            claim_time_zone_alert(remembered, current, stored.as_deref(), |marker| {
                swap_setting_in(&mut conn, TIME_ZONE_ALERTED_KEY, marker).ok().flatten().is_some()
            })
        };
        assert!(!check(&pacific(), &pacific()), "no change");
        assert!(check(&pacific(), &line_islands()), "changed");
        assert!(!check(&pacific(), &line_islands()), "already alerted - every later check and start");
        assert!(check(&pacific(), &eastern()), "changed again");
        assert!(!check(&pacific(), &pacific()), "back to the remembered zone");
        assert!(check(&pacific(), &eastern()), "a new change");
        // Accepted (adopt_current_time_zone clears the marker): quiet
        assert!(!check(&eastern(), &eastern()));
        // A swap that doesn't go through (database busy) alerts next time
        assert!(!claim_time_zone_alert(&eastern(), &pacific(), Some(""), |_| false));
    }

    #[test]
    fn remembered_zone_survives_storage_and_rejects_edits() {
        let mut z = newfoundland();
        z.key = "Odd|Name".into();
        z.dst_disabled = true;
        assert_eq!(TimeZone::parse(&z.serialize()), Some(z.clone()));
        assert_eq!(TimeZone::parse(&pacific().serialize()), Some(pacific()));
        assert_eq!(TimeZone::parse(""), None);
        assert_eq!(TimeZone::parse("2|480|0|0|-60|0,11,0,1,2,0,0,0|0,3,0,2,2,0,0,0|X"), None, "unknown version");
        assert_eq!(TimeZone::parse("1|480|0|0|-60|0,11,0,1,2,0,0|0,3,0,2,2,0,0,0|X"), None, "7 fields");
        assert_eq!(TimeZone::parse("1|99999|0|0|-60|0,11,0,1,2,0,0,0|0,3,0,2,2,0,0,0|X"), None, "absurd bias");
        assert_eq!(TimeZone::parse("1|480|2|0|-60|0,11,0,1,2,0,0,0|0,3,0,2,2,0,0,0|X"), None);
    }

    #[test]
    fn the_current_zone_reads_and_converts_like_windows() {
        use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};
        let current = current_time_zone().expect("time zone");
        assert_eq!(TimeZone::parse(&current.serialize()), Some(current.clone()));
        let (local, now) = unsafe { (GetLocalTime(), GetSystemTime()) };
        // (Could only differ if the test ran across local midnight.)
        assert_eq!(current.date_at(&now), systemtime_date(&local));
    }

    #[test]
    fn weekdays_from_dates() {
        assert_eq!(weekday_of_date("1970-01-01"), Some(3), "Thursday");
        assert_eq!(weekday_of_date("2026-11-15"), Some(6), "Sunday");
        assert_eq!(weekday_of_date("2026-11-16"), Some(0), "Monday");
        assert_eq!(weekday_of_date("2024-02-29"), Some(3), "Thursday");
        assert_eq!(weekday_of_date("garbage"), None);
    }

    #[test]
    fn pause_log_time_never_overflows() {
        assert_eq!(local_secs_of_day(civil_to_secs(2026, 11, 15, 23, 30, 0), 60), 30 * 60);
        assert_eq!(local_secs_of_day(civil_to_secs(2026, 11, 15, 1, 0, 0), -120), 23 * 3600);
        // Bounded by the parser, but saturating anyway
        let _ = local_secs_of_day(i64::MAX, 840);
        let _ = local_secs_of_day(i64::MIN, -840);
    }

    #[test]
    fn inflight_pause_parses_only_valid_values() {
        assert_eq!(parse_inflight_pause("2026-11-15|130|9223372036854775807"), None, "saved in year 292 billion");
        assert_eq!(parse_inflight_pause("2026-11-15|130|-1"), None);
        assert_eq!(parse_inflight_pause("2026-11-15|2147483647|1794750000"), None, "68 years of pause");
        assert_eq!(
            parse_inflight_pause("2026-11-15|86400|1794750000"),
            Some(("2026-11-15".to_string(), 86400, 1794750000))
        );
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
mod clock_anchor_tests {
    use super::*;

    /// The stored anchor and tamper state, updated the way
    /// `effective_today_date` and `take_pending_clock_tamper_alert` update
    /// the database.
    #[derive(Default, Clone)]
    struct Stored {
        anchor: Option<ClockAnchor>,
        tamper: ClockTamper,
    }

    impl Stored {
        fn today(&mut self, now: ClockReading, today: &str) -> String {
            let check = check_clock(self.anchor.as_ref(), self.tamper, now, today);
            if let Some(anchor) = check.anchor {
                self.anchor = Some(anchor);
            }
            self.tamper = check.tamper;
            check.date
        }

        fn take_alert(&mut self) -> Option<i64> {
            let drift = pending_clock_tamper_alert(&self.tamper)?;
            self.tamper.alerted = true;
            Some(drift)
        }

        /// `today`, but storing an accepted anchor only when
        /// `anchor_needs_write` says so, as `effective_today_date` does.
        fn today_lazily(&mut self, now: ClockReading, today: &str) -> String {
            let check = check_clock(self.anchor.as_ref(), self.tamper, now, today);
            if let Some(anchor) = check.anchor {
                if anchor_needs_write(self.anchor.as_ref(), &anchor) {
                    self.anchor = Some(anchor);
                }
            }
            self.tamper = check.tamper;
            check.date
        }
    }

    #[test]
    fn the_anchor_is_rewritten_only_when_it_matters() {
        let token = logon_at(secs(2026, 11, 16, 7, 0, 0));
        let stored = anchor(1_000, 60_000, "2026-11-16", token);
        let same = |tick_ms: i64| anchor(1_000 + (tick_ms - 60_000) / 1000, tick_ms, "2026-11-16", token);
        assert!(anchor_needs_write(None, &same(60_500)));
        assert!(!anchor_needs_write(Some(&stored), &same(60_500)), "half a second later");
        assert!(!anchor_needs_write(Some(&stored), &same(60_000 + ANCHOR_REWRITE_MS - 1)));
        assert!(anchor_needs_write(Some(&stored), &same(60_000 + ANCHOR_REWRITE_MS)));
        assert!(anchor_needs_write(Some(&stored), &anchor(1_001, 61_000, "2026-11-17", token)), "new date");
        assert!(anchor_needs_write(Some(&stored), &anchor(1_001, 61_000, "2026-11-16", logon_at(5))), "new session");
        assert!(anchor_needs_write(Some(&stored), &anchor(1_001, 59_000, "2026-11-16", token)), "tick went back");
    }

    #[test]
    fn rewriting_the_anchor_less_often_reaches_the_same_dates() {
        // A paused afternoon (a check every half second), then the clock is
        // moved forward a day mid-session, then moved back.
        let token = logon_at(secs(2026, 11, 16, 7, 0, 0));
        let start = secs(2026, 11, 16, 23, 50, 0);
        let mut eager = Stored::default();
        let mut lazy = Stored::default();
        let mut moved = 0;
        for step in 0..4_000i64 {
            let tick_ms = 3_600_000 + step * 500;
            if step == 1_000 {
                moved = DAY;
            }
            if step == 3_000 {
                moved = 0;
            }
            let wall = start + step / 2 + moved;
            let (y, m, d) = civil_from_days(wall.div_euclid(DAY));
            let today = format!("{y:04}-{m:02}-{d:02}");
            let now = reading(wall, tick_ms, token);
            assert_eq!(lazy.today_lazily(now, &today), eager.today(now, &today), "step {step}");
            // The same alerts; the drift they report may differ by the
            // second lost to rounding over an older anchor.
            match (lazy.take_alert(), eager.take_alert()) {
                (Some(a), Some(b)) => assert!((a - b).abs() <= 1, "step {step}: {a} vs {b}"),
                (a, b) => assert_eq!(a, b, "step {step}"),
            }
        }
    }

    const DAY: i64 = 86_400;
    const HOUR_MS: i64 = 3_600_000;

    fn secs(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
        civil_to_secs(y, mo, d, h, mi, s)
    }

    /// A session's logon token: its logon time as a FILETIME.
    fn logon_at(epoch_secs: i64) -> Option<i64> {
        Some((epoch_secs + 11_644_473_600) * 10_000_000)
    }

    fn reading(wall_secs: i64, tick_ms: i64, logon_token: Option<i64>) -> ClockReading {
        ClockReading { wall_secs, tick_ms, logon_token }
    }

    fn anchor(wall_secs: i64, tick_ms: i64, date: &str, logon_token: Option<i64>) -> ClockAnchor {
        ClockAnchor { wall_secs, tick_ms, date: date.into(), logon_token }
    }

    // The field case: the anchor was last written on 2026-11-01 at tick
    // 42,406 ms (the app started 42 s after boot and was quit soon after).
    // The PC rebooted during the two weeks the app stayed quit, and the app
    // was started again on 2026-11-15 hours into the new boot.
    const FIELD_ANCHOR_TICK: i64 = 42_406;
    const FIELD_DRIFT: i64 = 1_222_712;
    fn field_boot_1() -> i64 { secs(2026, 11, 1, 8, 59, 18) }
    fn field_anchor_wall() -> i64 { field_boot_1() + FIELD_ANCHOR_TICK / 1000 }
    fn field_now_tick() -> i64 { 6 * HOUR_MS + 1_234 }
    fn field_now_wall() -> i64 { field_anchor_wall() + (field_now_tick() - FIELD_ANCHOR_TICK) / 1000 + FIELD_DRIFT }

    #[test]
    fn field_case_numbers() {
        // Hours into a boot on 2026-11-15, with the tick above the anchor's.
        assert!(field_now_tick() > FIELD_ANCHOR_TICK);
        let (y, m, d) = civil_from_days(field_now_wall().div_euclid(DAY));
        assert_eq!((y, m, d), (2026, 11, 15));
        // Compared as one boot, the gap is the reported "clock moved forward".
        let same_session = check_clock(
            Some(&anchor(field_anchor_wall(), FIELD_ANCHOR_TICK, "2026-11-01", logon_at(field_boot_1() + 30))),
            ClockTamper::default(),
            reading(field_now_wall(), field_now_tick(), logon_at(field_boot_1() + 30)),
            "2026-11-15",
        );
        assert_eq!(same_session.tamper.drift_secs, Some(FIELD_DRIFT));
        assert_eq!(same_session.date, "2026-11-01");
    }

    #[test]
    fn a_later_start_in_a_new_boot_is_not_taken_for_a_clock_change() {
        let boot_2_logon = field_now_wall() - field_now_tick() / 1000 + 25;
        let mut stored = Stored {
            anchor: Some(anchor(field_anchor_wall(), FIELD_ANCHOR_TICK, "2026-11-01", logon_at(field_boot_1() + 30))),
            ..Default::default()
        };
        let now = reading(field_now_wall(), field_now_tick(), logon_at(boot_2_logon));
        assert_eq!(stored.today(now, "2026-11-15"), "2026-11-15");
        assert_eq!(stored.tamper, ClockTamper::default());
        assert_eq!(stored.take_alert(), None);
        assert_eq!(stored.anchor, Some(anchor(field_now_wall(), field_now_tick(), "2026-11-15", logon_at(boot_2_logon))));

        // Midnight comes as usual from there.
        let tomorrow = reading(field_now_wall() + 12 * 3600, field_now_tick() + 12 * HOUR_MS, logon_at(boot_2_logon));
        assert_eq!(stored.today(tomorrow, "2026-11-16"), "2026-11-16");
        assert_eq!(stored.take_alert(), None);
    }

    #[test]
    fn an_install_stuck_by_1_0_0_is_released_on_the_first_check() {
        // What 1.0.0 left behind: the 2026-11-01 anchor without a session,
        // and an alerted tamper episode that pinned the date.
        let mut stored = Stored {
            anchor: Some(anchor(field_anchor_wall(), FIELD_ANCHOR_TICK, "2026-11-01", None)),
            tamper: ClockTamper { active: true, alerted: true, drift_secs: Some(FIELD_DRIFT) },
        };
        let logon = logon_at(field_now_wall() - field_now_tick() / 1000 + 25);
        let now_wall = field_now_wall() + DAY;
        let now_tick = field_now_tick() + DAY * 1000;

        assert_eq!(stored.today(reading(now_wall, now_tick, logon), "2026-11-16"), "2026-11-16");
        assert!(!stored.tamper.active && !stored.tamper.alerted);
        assert_eq!(stored.take_alert(), None, "the stale episode is never alerted on");
        assert_eq!(stored.anchor, Some(anchor(now_wall, now_tick, "2026-11-16", logon)));

        // Later checks in this session stay on the real date, with no alert.
        let later = reading(now_wall + 600, now_tick + 600_000, logon);
        assert_eq!(stored.today(later, "2026-11-16"), "2026-11-16");
        assert_eq!(stored.take_alert(), None);
    }

    #[test]
    fn small_drift_in_the_same_session_is_accepted() {
        let logon = logon_at(secs(2026, 11, 16, 7, 0, 0));
        let wall = secs(2026, 11, 16, 9, 0, 0);
        for drift in [0, 600, -600, CLOCK_DRIFT_TOLERANCE_SECS, -CLOCK_DRIFT_TOLERANCE_SECS] {
            let mut stored = Stored { anchor: Some(anchor(wall, 2 * HOUR_MS, "2026-11-16", logon)), ..Default::default() };
            let now = reading(wall + 3600 + drift, 3 * HOUR_MS, logon);
            assert_eq!(stored.today(now, "2026-11-16"), "2026-11-16", "drift {drift}");
            assert!(!stored.tamper.active);
            assert_eq!(stored.anchor, Some(anchor(wall + 3600 + drift, 3 * HOUR_MS, "2026-11-16", logon)), "re-anchored");
        }
    }

    #[test]
    fn a_clock_moved_in_the_same_session_pins_the_date_and_alerts_once() {
        let logon = logon_at(secs(2026, 11, 16, 7, 0, 0));
        let wall = secs(2026, 11, 16, 20, 0, 0);
        let stored_anchor = anchor(wall, 13 * HOUR_MS, "2026-11-16", logon);
        for (drift, new_date) in [(2 * DAY, "2026-11-18"), (CLOCK_DRIFT_TOLERANCE_SECS + 1, "2026-11-16"), (-DAY, "2026-11-15")] {
            let mut stored = Stored { anchor: Some(stored_anchor.clone()), ..Default::default() };
            let now = reading(wall + 60 + drift, 13 * HOUR_MS + 60_000, logon);
            assert_eq!(stored.today(now, new_date), "2026-11-16", "drift {drift}");
            assert_eq!(stored.anchor, Some(stored_anchor.clone()), "the anchor is kept");
            assert_eq!(stored.take_alert(), Some(drift));
            assert_eq!(stored.take_alert(), None, "one alert per episode");

            // Still pinned while the clock stays moved; released once it's put back.
            let later = reading(wall + 120 + drift, 13 * HOUR_MS + 120_000, logon);
            assert_eq!(stored.today(later, new_date), "2026-11-16");
            assert_eq!(stored.take_alert(), None);
            let put_back = reading(wall + 180, 13 * HOUR_MS + 180_000, logon);
            assert_eq!(stored.today(put_back, "2026-11-16"), "2026-11-16");
            assert_eq!(stored.tamper, ClockTamper { active: false, alerted: false, drift_secs: Some(drift) });
        }
    }

    #[test]
    fn restarting_the_app_in_the_same_session_keeps_a_moved_clock_caught() {
        let logon = logon_at(secs(2026, 11, 16, 7, 0, 0));
        let wall = secs(2026, 11, 16, 20, 0, 0);
        let mut stored = Stored { anchor: Some(anchor(wall, 13 * HOUR_MS, "2026-11-16", logon)), ..Default::default() };

        // Moved a day forward, then the app is killed before the alert goes out.
        let moved = reading(wall + 30 + DAY, 13 * HOUR_MS + 30_000, logon);
        assert_eq!(stored.today(moved, "2026-11-17"), "2026-11-16");

        // The restarted app: same session, later tick.
        let restarted = reading(wall + 90 + DAY, 13 * HOUR_MS + 90_000, logon);
        assert_eq!(stored.today(restarted, "2026-11-17"), "2026-11-16");
        assert_eq!(stored.take_alert(), Some(DAY));

        // And again after the alert went out: still pinned, not alerted again.
        let again = reading(wall + 150 + DAY, 13 * HOUR_MS + 150_000, logon);
        assert_eq!(stored.today(again, "2026-11-17"), "2026-11-16");
        assert_eq!(stored.take_alert(), None);
    }

    #[test]
    fn a_new_session_is_accepted_whatever_the_tick() {
        let old_logon = logon_at(secs(2026, 11, 16, 7, 0, 0));
        let wall = secs(2026, 11, 16, 20, 0, 0);
        let stored_anchor = anchor(wall, 2 * HOUR_MS, "2026-11-16", old_logon);
        let moved = Stored {
            anchor: Some(stored_anchor.clone()),
            tamper: ClockTamper { active: true, alerted: true, drift_secs: Some(DAY) },
        };
        // After a reboot, below and above the anchor's tick; after a sign-out
        // and back in (same boot, tick still running).
        for (new_logon, tick) in [
            (secs(2026, 11, 17, 8, 0, 0), 40_000),
            (secs(2026, 11, 17, 8, 0, 0), 9 * HOUR_MS),
            (secs(2026, 11, 16, 21, 0, 0), 3 * HOUR_MS + 60_000),
        ] {
            for start in [Stored { anchor: Some(stored_anchor.clone()), ..Default::default() }, moved.clone()] {
                let mut stored = start;
                let now = reading(new_logon + 120, tick, logon_at(new_logon));
                let today = if new_logon > secs(2026, 11, 17, 0, 0, 0) { "2026-11-17" } else { "2026-11-16" };
                assert_eq!(stored.today(now, today), today);
                assert!(!stored.tamper.active && !stored.tamper.alerted);
                assert_eq!(stored.take_alert(), None);
                assert_eq!(stored.anchor, Some(anchor(new_logon + 120, tick, today, logon_at(new_logon))));
            }
        }
    }

    #[test]
    fn a_tick_gone_backwards_is_accepted() {
        let logon = logon_at(secs(2026, 11, 16, 7, 0, 0));
        let wall = secs(2026, 11, 16, 20, 0, 0);
        let mut stored = Stored { anchor: Some(anchor(wall, 13 * HOUR_MS, "2026-11-16", logon)), ..Default::default() };
        let now = reading(wall + 2 * DAY, 60_000, logon);
        assert_eq!(stored.today(now, "2026-11-18"), "2026-11-18");
        assert!(!stored.tamper.active);
    }

    #[test]
    fn without_a_session_the_clock_is_accepted() {
        let logon = logon_at(secs(2026, 11, 16, 7, 0, 0));
        let wall = secs(2026, 11, 16, 20, 0, 0);
        let mut stored = Stored {
            anchor: Some(anchor(wall, 13 * HOUR_MS, "2026-11-16", logon)),
            tamper: ClockTamper { active: true, alerted: false, drift_secs: Some(DAY) },
        };

        // This session can't be identified now: accepted, anchored without one.
        let now = reading(wall + DAY + 60, 13 * HOUR_MS + 60_000, None);
        assert_eq!(stored.today(now, "2026-11-17"), "2026-11-17");
        assert_eq!(stored.take_alert(), None);
        assert_eq!(stored.anchor, Some(anchor(wall + DAY + 60, 13 * HOUR_MS + 60_000, "2026-11-17", None)));

        // An anchor without a session is accepted again once it can be.
        let later = reading(wall + 3 * DAY, 13 * HOUR_MS + 120_000, logon);
        assert_eq!(stored.today(later, "2026-11-19"), "2026-11-19");
        assert_eq!(stored.anchor.as_ref().and_then(|a| a.logon_token), logon);

        // No anchor at all (first run, or a recreated database): accepted.
        let mut fresh = Stored::default();
        assert_eq!(fresh.today(reading(wall, 13 * HOUR_MS, logon), "2026-11-16"), "2026-11-16");
        assert_eq!(fresh.anchor, Some(anchor(wall, 13 * HOUR_MS, "2026-11-16", logon)));
    }

    #[test]
    fn a_stale_alerted_flag_does_not_hide_a_new_episode() {
        let logon = logon_at(secs(2026, 11, 16, 7, 0, 0));
        let wall = secs(2026, 11, 16, 20, 0, 0);
        let mut stored = Stored {
            anchor: Some(anchor(wall, 13 * HOUR_MS, "2026-11-16", logon)),
            tamper: ClockTamper { active: false, alerted: true, drift_secs: Some(5 * DAY) },
        };
        let moved = reading(wall + 60 + DAY, 13 * HOUR_MS + 60_000, logon);
        assert_eq!(stored.today(moved, "2026-11-17"), "2026-11-16");
        assert_eq!(stored.take_alert(), Some(DAY));
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
    fn rotating_secret_is_created_only_when_missing_never_on_a_read_error() {
        let conn = Connection::open_in_memory().unwrap();
        // No settings table: every read fails, as with a busy or broken
        // database. Nothing may be created (it used to write a new secret).
        assert_eq!(get_or_create_rotating_secret_in(&conn, || panic!("must not create")), None);

        conn.execute("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)", []).unwrap();
        assert_eq!(get_or_create_rotating_secret_in(&conn, || "first".into()).as_deref(), Some("first"));
        // An existing secret is kept, whoever wrote it.
        assert_eq!(get_or_create_rotating_secret_in(&conn, || panic!("must not create")).as_deref(), Some("first"));
        conn.execute("DELETE FROM settings", []).unwrap();
        conn.execute("INSERT INTO settings VALUES (?1, 'other')", params![ROTATING_PIN_SECRET]).unwrap();
        assert_eq!(get_or_create_rotating_secret_in(&conn, || "mine".into()).as_deref(), Some("other"));
    }

    #[test]
    fn rotating_pin_is_four_digits_and_changes_with_the_date() {
        let a = rotating_pin_for("12345", "2026-11-15");
        assert_eq!(a.len(), 4);
        assert!(a.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(a, rotating_pin_for("12345", "2026-11-15"));
        assert_ne!(
            (1..=9).map(|d| rotating_pin_for("12345", &format!("2026-11-1{d}"))).collect::<std::collections::HashSet<_>>().len(),
            1
        );
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
        assert_eq!(clamp_lock_screen_timeout(i32::MIN), 60, "negative never means \"never\"");
        assert_eq!(clamp_lock_screen_timeout(i32::MAX), i32::MAX);
    }

    #[test]
    fn settings_rows_are_written_all_or_nothing() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)", []).unwrap();
        let read = |conn: &Connection, key: &str| -> Option<String> {
            conn.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0)).ok()
        };

        set_settings_in(&mut conn, &[("a", "1".into()), ("b", "2".into())]).unwrap();
        assert_eq!(read(&conn, "a").as_deref(), Some("1"));
        assert_eq!(read(&conn, "b").as_deref(), Some("2"));

        // A failure part-way (here, a trigger refusing the second row)
        // leaves nothing written.
        conn.execute(
            "CREATE TRIGGER fail_on_c BEFORE INSERT ON settings WHEN NEW.key = 'c' BEGIN SELECT RAISE(ABORT, 'no'); END",
            [],
        )
        .unwrap();
        assert!(set_settings_in(&mut conn, &[("a", "9".into()), ("c", "3".into())]).is_err());
        assert_eq!(read(&conn, "a").as_deref(), Some("1"), "the first row was rolled back");
    }

    #[test]
    fn readers_fall_back_to_the_first_run_defaults() {
        let conn = Connection::open_in_memory().unwrap();
        prepare_settings_connection(&conn).unwrap();
        let stored = |key: &str| -> String {
            conn.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0)).unwrap()
        };
        for key in WEEKDAY_KEYS {
            assert_eq!(minutes_or(Some(&stored(key)), 0), DEFAULT_DAILY_LIMIT_MINUTES, "{key}");
        }
        assert_eq!(minutes_or(Some(&stored("warning1_minutes")), 0), default_warning_minutes(1));
        assert_eq!(minutes_or(Some(&stored("warning2_minutes")), 0), default_warning_minutes(2));
        assert_ne!(default_warning_minutes(1), default_warning_minutes(2), "the warnings don't fire together");

        // Missing, empty or junk values read as those defaults
        assert_eq!(minutes_or(None, default_warning_minutes(1)), 10);
        assert_eq!(minutes_or(Some(""), default_warning_minutes(1)), 10);
        assert_eq!(minutes_or(Some(""), DEFAULT_DAILY_LIMIT_MINUTES), 120);
        assert_eq!(minutes_or(Some("abc"), default_warning_minutes(2)), 5);
        assert_eq!(minutes_or(Some("45"), DEFAULT_DAILY_LIMIT_MINUTES), 45);
    }

    #[test]
    fn idle_pause_is_on_unless_switched_off() {
        assert!(idle_enabled_from(None), "never chosen");
        assert!(idle_enabled_from(Some("")), "empty");
        assert!(idle_enabled_from(Some("1")));
        assert!(!idle_enabled_from(Some("0")), "a saved \"off\" is respected");

        // A new install (or a recreated database) starts with it on, 5 minutes.
        let conn = Connection::open_in_memory().unwrap();
        prepare_settings_connection(&conn).unwrap();
        let stored = |key: &str| -> String {
            conn.query_row("SELECT value FROM settings WHERE key = ?1", params![key], |r| r.get(0)).unwrap()
        };
        assert!(idle_enabled_from(Some(&stored("idle_enabled"))));
        assert_eq!(stored("idle_timeout_minutes"), "5");
    }

    #[test]
    fn recovered_day_starts_from_the_time_already_used() {
        assert_eq!(remaining_after_use(7200, 3000), 4200);
        assert_eq!(remaining_after_use(7200, 9000), 0, "used more than the limit (extensions)");
        assert_eq!(remaining_after_use(7200, 0), 7200);
        assert_eq!(remaining_after_use(0, 100), 0);
        assert_eq!(remaining_after_use(7200, -5), 7200);
    }

    #[test]
    fn a_deleted_database_does_not_start_the_day_over() {
        // The file was deleted: nothing stored, the mirror shows 50 minutes
        // used and 10 paused, limit 2 h.
        assert_eq!(
            mirror_restore(None, (0, 0), (3000, 600), 7200),
            Some(MirrorRestore { active: 3000, paused: 600, remaining: 4200 })
        );
        // A fresh day (nothing in the mirror either) isn't touched...
        assert_eq!(mirror_restore(None, (0, 0), (0, 0), 7200), None);
        // ...nor a day that already has its remaining time (a normal start)
        assert_eq!(mirror_restore(Some(5000), (2200, 0), (2200, 0), 7200), None);
        // Restoring twice changes nothing the second time: by then the
        // remaining time is stored.
        assert_eq!(mirror_restore(Some(4200), (3000, 600), (3000, 600), 7200), None);
        // Stored and mirrored usage are never added up, and never lowered
        assert_eq!(
            mirror_restore(None, (3100, 600), (3000, 700), 7200),
            Some(MirrorRestore { active: 3100, paused: 700, remaining: 4100 })
        );
        // Used more than the limit (extensions): no time left, not negative
        assert_eq!(mirror_restore(None, (0, 0), (9000, 0), 7200).map(|r| r.remaining), Some(0));
    }

    #[test]
    fn counters_are_saved_under_the_day_they_belong_to() {
        // Loaded for the 16th; a save after midnight but before the
        // rollover still goes under the 16th.
        assert_eq!(counters_save_date(Some("2026-11-16"), || "2026-11-17".into()), "2026-11-16");
        assert_eq!(counters_save_date(None, || "2026-11-17".into()), "2026-11-17", "before startup loaded them");
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
