//! Self-update: checking GitHub for a newer release, and installing it.
//!
//! Two very different processes use this module:
//!
//! - The normal per-session tray app (a standard user) checks GitHub once a
//!   day and tells the admin via the bots when a newer release exists, runs
//!   the `update` bot command (see `remote_commands::cmd_update`), and
//!   announces once after an update that the new version is running.
//! - `screen-time-manager.exe --update`, run as SYSTEM by the
//!   `ScreenTimeManagerUpdate` Scheduled Task (registered by the installer so
//!   standard users may start it with `schtasks /Run`). It downloads the
//!   latest installer, checks it against the SHA-256 GitHub publishes for
//!   it, and runs it silently. Setup then stops every running instance in
//!   every session, replaces the exe, re-registers the tasks and touches
//!   `updating.marker`, which tells the watchdog (see watchdog.rs) to
//!   relaunch the app without a tamper alert.
//!
//! The SYSTEM side never creates, writes or opens-for-write anything outside
//! `<exe dir>\updates\` (admin-only): `%ProgramData%\ScreenTimeManager` and
//! user profiles are writable by standard users, so writing there as SYSTEM
//! would be a link-following privilege-escalation risk. That includes the
//! shared SQLite database - `--update` never opens it.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::database;
use crate::discord;
use crate::i18n;
use crate::remote_commands::join_words;
use crate::telegram;

pub const UPDATE_ARG: &str = "--update";

/// The on-demand SYSTEM task that runs `--update`, as registered by the installer.
pub const UPDATE_TASK_NAME: &str = "ScreenTimeManagerUpdate";

const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/dhiorns15/screen-time-manager-for-windows/releases/latest";
pub const RELEASES_PAGE_URL: &str = "https://github.com/dhiorns15/screen-time-manager-for-windows/releases";

/// How the installer is named among a release's downloads.
const INSTALLER_SUFFIX: &str = "-windows-x64-setup.exe";
const SETUP_SILENT_ARGS: [&str; 3] = ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"];

/// Debug builds only: overrides the latest-release URL, so CI can serve a
/// fake release from localhost and run `--update` end to end. Deliberately
/// compiled out of release builds - the SYSTEM task must only ever install
/// what GitHub publishes.
#[cfg(debug_assertions)]
pub const FEED_ENV: &str = "STM_UPDATE_FEED";

/// Generic on purpose - GitHub's API requires some User-Agent, nothing more.
const USER_AGENT: &str = concat!("screen-time-manager/", env!("CARGO_PKG_VERSION"));

/// Files next to the exe / under `<exe dir>\updates\` (see the module docs).
const UPDATES_DIR: &str = "updates";
const LOG_FILE: &str = "update.log";
const OLD_LOG_FILE: &str = "update.log.old";
/// Inno Setup's own log of the last unattended install (see `setup_args`).
const SETUP_LOG_FILE: &str = "setup.log";
const MARKER_FILE: &str = "updating.marker";

/// update.log is rotated to update.log.old past this size, so the two
/// together never exceed twice this.
const MAX_LOG_BYTES: u64 = 64 * 1024;

const MAX_RELEASE_JSON_BYTES: u64 = 1024 * 1024;
const MAX_INSTALLER_BYTES: u64 = 256 * 1024 * 1024;

/// Shared-database keys (see `database::swap_shared_setting`) - shared, not
/// per-account, so with several accounts signed in only one of them
/// announces a given version.
const UPDATE_NOTIFIED_VERSION_KEY: &str = "update_notified_version";
const LAST_RUNNING_VERSION_KEY: &str = "last_running_version";

/// A release version: major.minor.patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parses "1.2.3" or "v1.2.3". Anything else (pre-release suffixes
    /// included) is rejected - better to install nothing than something
    /// this code doesn't understand.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let parts: Vec<u32> = s
            .split('.')
            .map(|p| {
                if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) {
                    p.parse().ok()
                } else {
                    None
                }
            })
            .collect::<Option<_>>()?;
        match parts[..] {
            [major, minor, patch] => Some(Version { major, minor, patch }),
            _ => None,
        }
    }

    /// This build's version, from Cargo.toml.
    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).expect("the package version is x.y.z")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: Version,
    /// The release's web page.
    pub page: String,
    /// The Windows installer, if the release has one.
    pub installer: Option<Asset>,
}

#[derive(Clone, Debug)]
pub struct Asset {
    pub name: String,
    pub url: String,
    /// Lowercase hex SHA-256, from GitHub's `digest` field - `None` if
    /// GitHub gave none (or something that isn't a SHA-256).
    pub sha256: Option<String>,
}

/// Reads GitHub's description of a release (the `releases/latest` reply).
pub fn parse_release(json: &[u8]) -> Result<Release, String> {
    let v: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| format!("unexpected reply from GitHub: {e}"))?;
    let tag = v["tag_name"].as_str().ok_or("unexpected reply from GitHub: no tag_name")?;
    let version = Version::parse(tag).ok_or_else(|| format!("unrecognised release version {tag}"))?;
    let page = v["html_url"].as_str().unwrap_or(RELEASES_PAGE_URL).to_string();
    let installer = v["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(INSTALLER_SUFFIX)))
        .and_then(|a| {
            Some(Asset {
                name: a["name"].as_str()?.to_string(),
                url: a["browser_download_url"].as_str()?.to_string(),
                sha256: a["digest"]
                    .as_str()
                    .and_then(|d| d.strip_prefix("sha256:"))
                    .map(str::to_ascii_lowercase)
                    .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())),
            })
        });
    Ok(Release { version, page, installer })
}

/// The release's installer and its SHA-256 - an error if either is missing,
/// since nothing gets installed without a checksum to verify it against.
pub fn installer_asset(release: &Release) -> Result<(&Asset, &str), String> {
    let asset = release
        .installer
        .as_ref()
        .ok_or_else(|| format!("release v{} has no *{INSTALLER_SUFFIX} asset", release.version))?;
    let sha256 = asset
        .sha256
        .as_deref()
        .ok_or_else(|| format!("GitHub gave no SHA-256 digest for {}", asset.name))?;
    Ok((asset, sha256))
}

/// What to install, given the latest release and the running version:
/// nothing unless the release is strictly newer (so never a reinstall or a
/// downgrade), else its installer and checksum.
pub fn plan(release: &Release, current: Version) -> Result<Option<(&Asset, &str)>, String> {
    if release.version <= current {
        return Ok(None);
    }
    installer_asset(release).map(Some)
}

/// Lowercase hex SHA-256, the form GitHub reports.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    let actual = sha256_hex(bytes);
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!("checksum mismatch (expected {expected}, got {actual})"))
    }
}

fn latest_release_url() -> String {
    #[cfg(debug_assertions)]
    if let Some(url) = std::env::var(FEED_ENV).ok().filter(|u| !u.is_empty()) {
        return url;
    }
    LATEST_RELEASE_URL.to_string()
}

/// GET `url` into memory, refusing anything over `limit` bytes. Release
/// builds refuse plain http (including an https->http redirect); debug
/// builds allow it, for the localhost feed CI uses (see `FEED_ENV`).
fn get(url: &str, accept: Option<&str>, overall_timeout: Duration, limit: u64) -> Result<Vec<u8>, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .timeout(overall_timeout)
        .https_only(!cfg!(debug_assertions))
        .user_agent(USER_AGENT)
        .build();
    let mut request = agent.get(url);
    if let Some(accept) = accept {
        request = request.set("Accept", accept);
    }
    let response = request.call().map_err(|e| match e {
        // GitHub answers releases/latest with 404 while nothing is published.
        ureq::Error::Status(404, _) => format!("no published release found ({url} replied HTTP 404)"),
        ureq::Error::Status(code, _) => format!("{url} replied HTTP {code}"),
        ureq::Error::Transport(t) => format!("couldn't reach {url}: {t}"),
    })?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(limit + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("download from {url} failed: {e}"))?;
    if body.len() as u64 > limit {
        return Err(format!("{url} is larger than {limit} bytes"));
    }
    Ok(body)
}

/// The latest published release, from GitHub (or, in debug builds, the
/// `FEED_ENV` override).
pub fn fetch_latest_release() -> Result<Release, String> {
    fetch_release_from(&latest_release_url())
}

fn fetch_release_from(url: &str) -> Result<Release, String> {
    let json = get(url, Some("application/vnd.github+json"), Duration::from_secs(20), MAX_RELEASE_JSON_BYTES)?;
    parse_release(&json)
}

/// The latest release, if it is newer than this build.
pub fn check_for_newer() -> Result<Option<Release>, String> {
    let release = fetch_latest_release()?;
    Ok((release.version > Version::current()).then_some(release))
}

/// Downloads an installer into memory and checks it against `sha256`
/// before anything is written anywhere.
fn download_verified(asset: &Asset, sha256: &str) -> Result<Vec<u8>, String> {
    let bytes = get(&asset.url, None, Duration::from_secs(10 * 60), MAX_INSTALLER_BYTES)?;
    verify_sha256(&bytes, sha256)?;
    Ok(bytes)
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

/// `<exe dir>\updates\` - admin-only, see the module docs.
pub fn updates_dir() -> Option<PathBuf> {
    exe_dir().map(|d| d.join(UPDATES_DIR))
}

/// Where `--update` logs - shown to the admin when an update stalls.
pub fn log_path() -> Option<PathBuf> {
    updates_dir().map(|d| d.join(LOG_FILE))
}

/// Where setup logs the install `--update` started - also shown to the
/// admin when an update stalls.
pub fn setup_log_path() -> Option<PathBuf> {
    updates_dir().map(|d| d.join(SETUP_LOG_FILE))
}

/// When setup last (re)wrote `updating.marker` next to the exe: once the old
/// instances are gone (before it re-enables the watchdog task), and also
/// when it fails or is cancelled after disabling the tasks. Read by the
/// watchdog.
pub fn update_marker_modified() -> Option<SystemTime> {
    let path = exe_dir()?.join(MARKER_FILE);
    std::fs::metadata(path).ok()?.modified().ok()
}

// ============================================================================
// `--update` (SYSTEM, from the ScreenTimeManagerUpdate task)
// ============================================================================

/// Appends to `<exe dir>\updates\update.log`, rotating it to update.log.old
/// past `MAX_LOG_BYTES`. Also echoed to stderr, for CI runs that capture it.
fn log(msg: &str) {
    use std::io::Write;

    eprintln!("[Update] {msg}");
    let Some(dir) = updates_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(LOG_FILE);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = std::fs::rename(&path, dir.join(OLD_LOG_FILE));
    }
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{now}] {msg}");
    }
}

/// Whether this process runs with a full (elevated) token - true for SYSTEM
/// and for an elevated admin, false for a standard user or a filtered admin.
/// Also accepts an enabled Administrators membership (IsUserAnAdmin), which
/// SYSTEM's token has too, in case TokenElevation ever reports SYSTEM's
/// token as "not elevated".
fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::UI::Shell::IsUserAnAdmin;

    unsafe {
        if IsUserAnAdmin().as_bool() {
            return true;
        }
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Entry point for `--update`; returns the process exit code: 0 when setup
/// was started or nothing newer exists, 1 on any failure, 2 when not
/// elevated.
pub fn run_update_mode() -> i32 {
    // Replaces main's panic hook, which logs under the profile directory -
    // keep everything this process writes under updates\.
    std::panic::set_hook(Box::new(|info| log(&format!("panic: {info}"))));

    let current = Version::current();
    log(&format!("--update started, running v{current}"));
    #[cfg(debug_assertions)]
    if let Some(feed) = std::env::var(FEED_ENV).ok().filter(|u| !u.is_empty()) {
        log(&format!("debug build: release feed overridden by {FEED_ENV}={feed}"));
    }

    if !is_elevated() {
        log("refusing: not elevated (run via the ScreenTimeManagerUpdate task)");
        return 2;
    }

    let release = match fetch_latest_release() {
        Ok(r) => r,
        Err(e) => {
            log(&format!("check failed: {e}"));
            return 1;
        }
    };
    let (asset, sha256) = match plan(&release, current) {
        Ok(Some(x)) => x,
        Ok(None) => {
            log(&format!("latest release is v{}, not newer than v{current} - nothing to do", release.version));
            return 0;
        }
        Err(e) => {
            log(&format!("not installing v{}: {e}", release.version));
            return 1;
        }
    };

    log(&format!("downloading v{} from {}", release.version, asset.url));
    let bytes = match download_verified(asset, sha256) {
        Ok(b) => b,
        Err(e) => {
            log(&format!("download failed: {e}"));
            return 1;
        }
    };

    let Some(dir) = updates_dir() else {
        log("couldn't determine the exe's folder");
        return 1;
    };
    let setup = match save_installer(&dir, &bytes, sha256, release.version) {
        Ok(p) => p,
        Err(e) => {
            log(&format!("couldn't save the installer: {e}"));
            return 1;
        }
    };

    // Setup's exit code isn't seen here (this process exits right away, and
    // setup kills it if it hasn't after 30s) - a failed or incomplete
    // install (exit code 10: files copied, task registration or the
    // ProgramData ACL failed) shows up in setup.log instead.
    let args = setup_args(&dir.join(SETUP_LOG_FILE));
    match spawn_setup(&setup, &args) {
        Ok(how) => {
            let shown: Vec<_> = args.iter().map(|a| a.to_string_lossy()).collect();
            log(&format!("started {} {} ({how})", setup.display(), shown.join(" ")));
            0
        }
        Err(e) => {
            log(&format!("couldn't start {}: {e}", setup.display()));
            1
        }
    }
}

/// Writes the verified installer into `dir` (updates\) with create-new
/// semantics, after clearing out earlier downloads, then re-reads it and
/// checks the hash once more - what's on disk is what runs.
fn save_installer(dir: &Path, bytes: &[u8], sha256: &str, version: Version) -> Result<PathBuf, String> {
    use std::io::Write;

    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten() {
        let name = entry.file_name();
        if name == LOG_FILE || name == OLD_LOG_FILE || name == SETUP_LOG_FILE {
            continue;
        }
        let path = entry.path();
        // symlink_metadata, so a link (were one ever to appear in this
        // admin-only folder) is removed itself rather than followed.
        let is_dir = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
        let removed = if is_dir { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        if let Err(e) = removed {
            log(&format!("couldn't remove stale {}: {e}", path.display()));
        }
    }

    let path = dir.join(format!("screen-time-manager-v{version}{INSTALLER_SUFFIX}"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(bytes).and_then(|_| file.sync_all()).map_err(|e| format!("{}: {e}", path.display()))?;
    drop(file);

    let verified = std::fs::read(&path)
        .map_err(|e| e.to_string())
        .and_then(|written| verify_sha256(&written, sha256));
    if let Err(e) = verified {
        let _ = std::fs::remove_file(&path);
        return Err(format!("{} after saving: {e}", path.display()));
    }
    Ok(path)
}

/// Setup's command-line arguments: the silent switches, plus `/LOG=<path>`
/// so a failed unattended install leaves evidence behind.
///
/// `/LOG=<path>` is deliberately one argument with no quotes of its own:
/// the install dir contains a space, so std quotes the whole argument
/// (`"/LOG=C:\Program Files\...\setup.log"` - backslashes are only escaped
/// before a quote, and the path ends in a file name), and Inno Setup's
/// command-line parsing (Delphi `ParamStr` rules, which strip quotes
/// wherever they appear in a parameter) hands `/LOG=C:\Program Files\...`
/// on intact - the same thing its documented `/LOG="filename"` form becomes.
fn setup_args(setup_log: &Path) -> Vec<std::ffi::OsString> {
    let mut log_arg = std::ffi::OsString::from("/LOG=");
    log_arg.push(setup_log.as_os_str());
    SETUP_SILENT_ARGS.iter().map(std::ffi::OsString::from).chain([log_arg]).collect()
}

/// Starts setup and returns without waiting - setup replaces this very exe,
/// so `--update` has to be gone promptly.
///
/// Task Scheduler runs each task instance's process inside a Job Object
/// (see watchdog.rs's `relaunch_app` for how that bit the watchdog), and a
/// plain child lands in the same job. Stopping the task instance - its
/// execution time limit, a "stop existing instance" policy, or the task
/// being stopped while setup re-registers it - terminates the whole job,
/// setup included. So ask for CREATE_BREAKAWAY_FROM_JOB first; that is
/// refused with ERROR_ACCESS_DENIED unless the job allows breakaway, in
/// which case setup starts inside the job instead, and the task's settings
/// (no short time limit, no StopExisting) have to keep it alive.
fn spawn_setup(setup: &Path, args: &[std::ffi::OsString]) -> Result<&'static str, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;

    let spawn = |flags: u32| {
        std::process::Command::new(setup)
            .args(args)
            .current_dir(setup.parent().unwrap_or(Path::new(".")))
            .creation_flags(flags)
            .spawn()
    };
    match spawn(CREATE_BREAKAWAY_FROM_JOB) {
        Ok(_) => Ok("broke away from the task's job, or wasn't in one"),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => spawn(0)
            .map(|_| "inside the task's job - breakaway not permitted")
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

// ============================================================================
// Tray app side: daily check, version-change notice, `update` command
// ============================================================================

/// First check this long after startup, then once a day.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(2 * 60);
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Retry interval when a check couldn't happen (no network, no bot
/// connected in this session yet).
const RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// How long to wait for this session's bots to connect before giving up on
/// announcing a version change (another session may announce it instead).
const ANNOUNCE_POLL: Duration = Duration::from_secs(10);
const ANNOUNCE_MAX_POLLS: u32 = 60;

/// After the `update` command starts the task, if this process is still
/// alive this much later the update didn't happen: setup stops every
/// running instance before replacing the exe, so a successful update
/// always takes this process down well before then.
const STALL_CHECK_AFTER: Duration = Duration::from_secs(10 * 60);
static STALL_WATCH_PENDING: AtomicBool = AtomicBool::new(false);

enum BotState {
    /// No bot is enabled at all - nobody to tell.
    NoneEnabled,
    /// A bot is enabled but not connected in this session (yet) - only the
    /// session that owns the console runs the bots (see session.rs).
    NotReady,
    Ready,
}

fn bot_state() -> BotState {
    let tg = database::get_telegram_config().enabled;
    let dc = database::get_discord_config().enabled;
    if !tg && !dc {
        BotState::NoneEnabled
    } else if (tg && telegram::can_notify()) || (dc && discord::can_notify()) {
        BotState::Ready
    } else {
        BotState::NotReady
    }
}

/// Same fan-out as time_request.rs's notifications, with per-platform text
/// where the command syntax differs.
fn notify_bots(tg_text: &str, dc_text: &str) {
    if database::get_telegram_config().enabled {
        telegram::notify_admin(tg_text);
    }
    if database::get_discord_config().enabled {
        discord::notify_admin(dc_text);
    }
}

/// Starts the tray app's background update work, off the UI thread.
pub fn start_background_tasks() {
    std::thread::spawn(announce_version_change);
    std::thread::spawn(daily_update_check);
}

/// Tells the admin once that a new version is now running. The last running
/// version lives in the shared database, so whichever session gets there
/// first claims the change (atomically - see `swap_shared_setting`) and the
/// rest stay quiet. Only claimed once this session can actually send it.
fn announce_version_change() {
    let current = Version::current().to_string();
    for _ in 0..ANNOUNCE_MAX_POLLS {
        std::thread::sleep(ANNOUNCE_POLL);
        match bot_state() {
            BotState::NotReady => continue,
            BotState::NoneEnabled => {
                // Still record it, so enabling a bot later doesn't announce
                // an update from long ago.
                database::swap_shared_setting(LAST_RUNNING_VERSION_KEY, &current);
                return;
            }
            BotState::Ready => {
                // Nothing recorded yet = a first install (or the first
                // version with this feature) - not an update.
                if let Some(Some(previous)) = database::swap_shared_setting(LAST_RUNNING_VERSION_KEY, &current) {
                    let text = format!("✅ {} v{previous} → v{current}", i18n::t("update.notify.updated"));
                    send_claimed_announcement(&text);
                }
                return;
            }
        }
    }
}

fn daily_update_check() {
    std::thread::sleep(FIRST_CHECK_DELAY);
    let mut last_checked: Option<Instant> = None;
    loop {
        if last_checked.is_none_or(|t| t.elapsed() >= CHECK_INTERVAL) {
            match bot_state() {
                BotState::NoneEnabled => last_checked = Some(Instant::now()),
                BotState::NotReady => {}
                BotState::Ready => match check_for_newer() {
                    Ok(newer) => {
                        last_checked = Some(Instant::now());
                        if let Some(release) = newer {
                            notify_update_available(&release);
                        }
                    }
                    Err(e) => eprintln!("[Update] Daily check failed: {e}"),
                },
            }
        }
        std::thread::sleep(RETRY_INTERVAL);
    }
}

/// Tells the admin about `release`, unless some session already told them
/// about this exact version.
fn notify_update_available(release: &Release) {
    if let Err(e) = installer_asset(release) {
        // The update command would only fail on it - don't advertise it.
        eprintln!("[Update] Not announcing v{}: {e}", release.version);
        return;
    }
    if database::swap_shared_setting(UPDATE_NOTIFIED_VERSION_KEY, &release.version.to_string()).is_none() {
        return;
    }
    let body = format!(
        "⬆️ {} v{}\n{}: v{}\n",
        i18n::t("update.notify.available"),
        release.version,
        i18n::t("update.notify.current"),
        Version::current(),
    );
    // One text for both bots (see `send_claimed_announcement`), so it names
    // the command for each one that's enabled.
    let how = match (database::get_telegram_config().enabled, database::get_discord_config().enabled) {
        (true, false) => i18n::t("update.notify.how_tg").to_string(),
        (false, true) => i18n::t("update.notify.how_dc").to_string(),
        _ => format!("{}\n{}", i18n::t("update.notify.how_tg"), i18n::t("update.notify.how_dc")),
    };
    send_claimed_announcement(&format!("{body}{how}"));
}

/// Send an announcement this session has just claimed in the shared
/// database (so no other session will send it). Through the watchdog's
/// alert queue rather than straight through the bots: a send through the
/// bots can fail (offline, Telegram/Discord unreachable) with nobody
/// noticing, and then the claimed announcement would be lost for good; the
/// queue retries until it's delivered (for up to a day), to every enabled
/// bot, whether or not that bot has connected in this session.
fn send_claimed_announcement(text: &str) {
    crate::watchdog::raise_alert(text);
}

/// Starts the SYSTEM update task. Standard users are allowed to run it -
/// the installer registers it that way.
pub fn start_update_task() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    // schtasks is a console program and this app has no console - without
    // this a console window would flash up.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let output = std::process::Command::new("schtasks")
        .args(["/Run", "/TN", UPDATE_TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("schtasks: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if detail.is_empty() { String::from_utf8_lossy(&output.stdout).trim().to_string() } else { detail };
    Err(format!("schtasks /Run /TN {UPDATE_TASK_NAME} failed ({}): {detail}", output.status))
}

/// After the `update` command, tells the admin if this (old) process is
/// still running `STALL_CHECK_AFTER` later - see there.
pub fn watch_for_stalled_update(target: Version) {
    if STALL_WATCH_PENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(STALL_CHECK_AFTER);
        STALL_WATCH_PENDING.store(false, Ordering::SeqCst);
        let shown = |path: Option<PathBuf>, name: &str| path.map(|p| p.display().to_string()).unwrap_or_else(|| name.to_string());
        let text = format!(
            "⚠️ {}\n{}: v{target}\n{}: v{}\n{}:\n{}\n{}",
            i18n::t("update.notify.stalled"),
            i18n::t("update.notify.target"),
            i18n::t("update.notify.running"),
            Version::current(),
            i18n::t("update.notify.log"),
            shown(log_path(), LOG_FILE),
            shown(setup_log_path(), SETUP_LOG_FILE),
        );
        notify_bots(&text, &text);
    });
}

/// The `update` bot command's text for an update that was just started.
pub fn installing_text(target: Version) -> String {
    format!(
        "⬆️ {} ({}: v{})\n{}",
        join_words(&[
            i18n::t("update.cmd.installing"),
            &format!("v{target}"),
            i18n::t("update.cmd.installing_suffix"),
        ]),
        i18n::t("update.notify.current"),
        Version::current(),
        i18n::t("update.cmd.restart_note"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parses_versions() {
        assert_eq!(Version::parse("1.2.3"), Some(Version { major: 1, minor: 2, patch: 3 }));
        assert_eq!(Version::parse("v1.2.3"), Some(Version { major: 1, minor: 2, patch: 3 }));
        assert_eq!(Version::parse(" v10.0.7 "), Some(Version { major: 10, minor: 0, patch: 7 }));
        for bad in ["", "v", "1", "1.2", "1.2.3.4", "1.2.x", "1.2.3-rc1", "1.2.3+build", "+1.2.3", "1..3", "vv1.2.3", "-1.2.3"] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
        assert_eq!(v("v1.20.3").to_string(), "1.20.3");
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn orders_versions_numerically() {
        assert!(v("1.0.10") > v("1.0.9"));
        assert!(v("1.10.0") > v("1.9.99"));
        assert!(v("2.0.0") > v("1.99.99"));
        assert!(v("1.0.0") == v("v1.0.0"));
        assert!(v("0.9.9") < v("1.0.0"));
    }

    fn release_json(tag: &str, assets: &str) -> String {
        format!(
            r#"{{"tag_name": "{tag}", "html_url": "https://github.com/x/y/releases/tag/{tag}", "assets": [{assets}]}}"#
        )
    }

    const DIGEST: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn installer(digest: Option<&str>) -> String {
        let digest = digest.map(|d| format!(r#", "digest": "{d}""#)).unwrap_or_default();
        format!(
            r#"{{"name": "screen-time-manager-v1.2.0-windows-x64-setup.exe", "browser_download_url": "https://example.com/setup"{digest}}}"#
        )
    }

    #[test]
    fn parses_releases() {
        let other = r#"{"name": "screen-time-manager.exe", "browser_download_url": "https://example.com/exe", "digest": "sha256:00"}"#;
        let json = release_json("v1.2.0", &format!("{other}, {}", installer(Some(&format!("sha256:{}", DIGEST.to_uppercase())))));
        let r = parse_release(json.as_bytes()).unwrap();
        assert_eq!(r.version, v("1.2.0"));
        assert!(r.page.ends_with("/tag/v1.2.0"));
        let (asset, sha) = installer_asset(&r).unwrap();
        assert_eq!(asset.url, "https://example.com/setup");
        assert_eq!(sha, DIGEST, "lowercased");

        // No html_url falls back to the releases page.
        let r = parse_release(br#"{"tag_name": "v1.2.0"}"#).unwrap();
        assert_eq!(r.page, RELEASES_PAGE_URL);

        assert!(parse_release(br#"{"message": "Not Found"}"#).is_err());
        assert!(parse_release(br#"{"tag_name": "nightly"}"#).is_err());
        assert!(parse_release(b"<html>").is_err());
    }

    #[test]
    fn refuses_a_release_without_installer_or_digest() {
        let missing_asset = parse_release(release_json("v9.0.0", "").as_bytes()).unwrap();
        assert!(installer_asset(&missing_asset).unwrap_err().contains("no *-windows-x64-setup.exe asset"));
        assert!(plan(&missing_asset, v("1.0.0")).is_err());

        let no_digest = parse_release(release_json("v9.0.0", &installer(None)).as_bytes()).unwrap();
        assert!(installer_asset(&no_digest).unwrap_err().contains("no SHA-256"));
        assert!(plan(&no_digest, v("1.0.0")).is_err());

        for odd in ["sha512:abcd", "sha256:xyz", "sha256:abc", DIGEST] {
            let r = parse_release(release_json("v9.0.0", &installer(Some(odd))).as_bytes()).unwrap();
            assert!(installer_asset(&r).is_err(), "{odd}");
        }
    }

    #[test]
    fn installs_only_strictly_newer_releases() {
        let r = parse_release(release_json("v1.2.0", &installer(Some(&format!("sha256:{DIGEST}")))).as_bytes()).unwrap();
        assert!(plan(&r, v("1.1.9")).unwrap().is_some());
        assert!(plan(&r, v("1.2.0")).unwrap().is_none(), "not newer means no update");
        assert!(plan(&r, v("1.2.1")).unwrap().is_none(), "never a downgrade");
        assert!(plan(&r, v("2.0.0")).unwrap().is_none(), "never a downgrade");

        // Not newer wins over a broken release: nothing to install, no error.
        let broken = parse_release(release_json("v1.0.0", "").as_bytes()).unwrap();
        assert!(plan(&broken, v("1.0.0")).unwrap().is_none());
    }

    /// Serves `routes` (path -> body) over plain http on localhost, one
    /// request per connection, each route once, in order.
    fn serve(mut routes: Vec<(&'static str, Vec<u8>)>) -> String {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            while !routes.is_empty() {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                    request.push(byte[0]);
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let (status, body) = match routes.iter().position(|(p, _)| *p == path) {
                    Some(i) => ("200 OK", routes.remove(i).1),
                    None => ("404 Not Found", Vec::new()),
                };
                let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        base
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stm-update-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The `--update` pipeline short of elevation and starting setup: fetch
    /// the release, download and verify the installer, save it.
    #[test]
    fn downloads_verifies_and_saves_a_release() {
        let installer_bytes = b"pretend installer".to_vec();
        let digest = sha256_hex(&installer_bytes);
        // The asset URL isn't known until the server is up, so serve the JSON
        // from a second server that points at the first.
        let files = serve(vec![("/setup.exe", installer_bytes.clone()), ("/setup.exe", b"tampered".to_vec())]);
        let asset = format!(
            r#"{{"name": "screen-time-manager-v99.0.0-windows-x64-setup.exe", "browser_download_url": "{files}/setup.exe", "digest": "sha256:{digest}"}}"#
        );
        let feed = serve(vec![("/latest", release_json("v99.0.0", &asset).into_bytes())]);

        let release = fetch_release_from(&format!("{feed}/latest")).unwrap();
        let (asset, sha256) = plan(&release, Version::current()).unwrap().expect("v99 is newer");
        let bytes = download_verified(asset, sha256).unwrap();
        assert_eq!(bytes, installer_bytes);
        // Second request gets different bytes: refused, nothing returned.
        assert!(download_verified(asset, sha256).unwrap_err().contains("checksum mismatch"));

        let dir = scratch_dir("save");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old-setup.exe"), b"stale").unwrap();
        std::fs::write(dir.join(LOG_FILE), b"kept").unwrap();
        std::fs::write(dir.join(SETUP_LOG_FILE), b"kept").unwrap();
        let saved = save_installer(&dir, &bytes, sha256, release.version).unwrap();
        assert_eq!(saved, dir.join("screen-time-manager-v99.0.0-windows-x64-setup.exe"));
        assert_eq!(std::fs::read(&saved).unwrap(), installer_bytes);
        assert!(!dir.join("old-setup.exe").exists(), "stale downloads are cleared first");
        assert_eq!(std::fs::read(dir.join(LOG_FILE)).unwrap(), b"kept");
        assert_eq!(std::fs::read(dir.join(SETUP_LOG_FILE)).unwrap(), b"kept", "setup's log survives the sweep");
        // Saving again clears the earlier copy rather than failing on it.
        assert!(save_installer(&dir, &bytes, sha256, release.version).is_ok());
        // Bytes that don't match the checksum never survive on disk.
        assert!(save_installer(&dir, b"tampered", sha256, release.version).is_err());
        assert!(!saved.exists());
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(fetch_release_from(&format!("{feed}/latest")).is_err(), "server is gone - an error, not a hang");
    }

    #[test]
    fn passes_the_setup_log_as_one_quoted_argument() {
        let log = Path::new(r"C:\Program Files\ScreenTimeManager\updates\setup.log");
        let args = setup_args(log);
        let shown: Vec<_> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            shown,
            ["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", r"/LOG=C:\Program Files\ScreenTimeManager\updates\setup.log"]
        );
        // The command line std builds from them - cmd's echo prints its
        // arguments exactly as they arrived: the space-containing one is
        // quoted whole, nothing else changes.
        let out = std::process::Command::new("cmd").args(["/c", "echo"]).args(&args).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            r#"/VERYSILENT /SUPPRESSMSGBOXES /NORESTART "/LOG=C:\Program Files\ScreenTimeManager\updates\setup.log""#
        );
    }

    #[test]
    fn checks_sha256() {
        assert_eq!(sha256_hex(b"abc"), DIGEST);
        assert!(verify_sha256(b"abc", DIGEST).is_ok());
        assert!(verify_sha256(b"abc", &DIGEST.to_uppercase()).is_ok());
        assert!(verify_sha256(b"abd", DIGEST).unwrap_err().contains("checksum mismatch"));
        assert!(verify_sha256(b"abc", "").is_err());
    }
}
