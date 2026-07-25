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

    *DB_CONNECTION.lock().unwrap() = Some(conn);
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

    // One-time migration: carry over bot settings from the old per-user
    // storage so existing setups (Telegram/Discord already configured
    // before this became shared) don't have to be redone.
    const MIGRATED_KEYS: [&str; 7] = [
        TELEGRAM_BOT_TOKEN, TELEGRAM_ADMIN_CHAT_ID, TELEGRAM_ENABLED,
        DISCORD_BOT_TOKEN, DISCORD_CHANNEL_ID, DISCORD_ADMIN_USER_ID, DISCORD_ENABLED,
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
}

/// Whether the app's most recent stop was the passcode-protected Quit menu
/// item, rather than being killed out from under the timer. The marker is
/// cleared on every real app start (see `clear_intentional_quit_marker`), so
/// its mere presence - no time window needed - means nothing has run since
/// that sanctioned quit.
pub fn quit_was_intentional() -> bool {
    intentional_quit_marker_path().exists()
}

/// Get the passcode from the database
pub fn get_passcode() -> Option<String> {
    let guard = DB_CONNECTION.lock().ok()?;
    guard.as_ref()?.query_row(
        "SELECT value FROM settings WHERE key = 'passcode'",
        [],
        |row| row.get(0),
    ).ok()
}

/// Set the passcode in the database
pub fn set_passcode(code: &str) -> bool {
    if let Ok(guard) = DB_CONNECTION.lock() {
        if let Some(conn) = guard.as_ref() {
            return conn.execute(
                "UPDATE settings SET value = ?1 WHERE key = 'passcode'",
                params![code],
            ).is_ok();
        }
    }
    false
}

/// Settings keys for the optional rotating daily PIN
const ROTATING_PIN_ENABLED: &str = "rotating_pin_enabled";
const ROTATING_PIN_SECRET: &str = "rotating_pin_secret";

/// Whether the rotating daily PIN is enabled (off by default) - when on, it
/// works everywhere the regular passcode does, as an additional valid code
/// alongside it (the regular passcode always keeps working too, so there's
/// no lockout risk if the rotating code isn't at hand).
pub fn is_rotating_pin_enabled() -> bool {
    get_setting(ROTATING_PIN_ENABLED).map(|s| s == "true").unwrap_or(false)
}

/// Enable/disable the rotating daily PIN
pub fn set_rotating_pin_enabled(enabled: bool) {
    set_setting(ROTATING_PIN_ENABLED, if enabled { "true" } else { "false" });
}

/// Get (creating if needed) the secret used to derive the daily rotating PIN
fn get_or_create_rotating_secret() -> String {
    if let Some(secret) = get_setting(ROTATING_PIN_SECRET) {
        return secret;
    }
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let seed = RandomState::new().build_hasher().finish();
    let secret = seed.to_string();
    set_setting(ROTATING_PIN_SECRET, &secret);
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

/// Check an entered code against the passcode and (if enabled) today's
/// rotating PIN. Use this instead of comparing to `get_passcode()` directly
/// anywhere a passcode is checked.
pub fn verify_passcode(entered: &str) -> bool {
    if let Some(stored) = get_passcode() {
        if entered == stored {
            return true;
        }
    }
    is_rotating_pin_enabled() && entered == get_rotating_pin()
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

/// Save remaining time to database (associated with current date)
pub fn save_remaining_time(seconds: i32) {
    let date = get_today_date();
    let key = format!("remaining_time_{}", date);
    set_setting(&key, &seconds.to_string());
}

/// Load remaining time from database for today
#[allow(dead_code)]
pub fn load_remaining_time() -> Option<i32> {
    let date = get_today_date();
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

/// Get lock screen timeout in seconds (time before shutdown when lock screen is active)
pub fn get_lock_screen_timeout() -> i32 {
    get_setting("lock_screen_timeout")
        .and_then(|s| s.parse().ok())
        .unwrap_or(600) // 10 minutes default
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
    let date = get_today_date();
    let key = format!("pause_used_{}", date);
    get_setting(&key)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Save pause time used today (in seconds)
pub fn save_pause_used_today(seconds: i32) {
    let date = get_today_date();
    let key = format!("pause_used_{}", date);
    set_setting(&key, &seconds.to_string());
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

/// Get current Unix timestamp
pub fn get_current_timestamp() -> i64 {
    use windows::Win32::System::SystemInformation::GetLocalTime;

    let st = unsafe { GetLocalTime() };

    // Simple conversion - just need relative timestamps for cooldown
    // This is approximate but sufficient for our purposes
    let days_since_epoch = (st.wYear as i64 - 1970) * 365
        + (st.wMonth as i64 - 1) * 30
        + st.wDay as i64;
    let seconds = days_since_epoch * 86400
        + st.wHour as i64 * 3600
        + st.wMinute as i64 * 60
        + st.wSecond as i64;
    seconds
}

/// Get the session start time used today (in seconds) - tracks when timer started today
pub fn get_session_active_time() -> i32 {
    let date = get_today_date();
    let key = format!("session_active_{}", date);
    get_setting(&key)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Save session active time (in seconds)
pub fn save_session_active_time(seconds: i32) {
    let date = get_today_date();
    let key = format!("session_active_{}", date);
    set_setting(&key, &seconds.to_string());
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
        .unwrap_or(true)
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
