//! Self-update: checking GitHub for a newer release, and installing it.
//!
//! Two very different processes use this module:
//!
//! - The normal per-session tray app (a standard user) checks GitHub once a
//!   day and tells the admin via the bots when a newer release exists (and
//!   reminds them once a day until it is installed), runs
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
//!
//! A third: `--secure-folders` (`folders` below), which setup runs elevated
//! to set up the shared config folder and check the install folder - the
//! only elevated code that touches `%ProgramData%\ScreenTimeManager`, and it
//! never follows a link there.

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
/// The local date (YYYY-MM-DD) the last "update available" message - the
/// first announcement of a version or a daily reminder - was sent on: at
/// most one a day per PC.
const UPDATE_MESSAGE_DAY_KEY: &str = "update_message_day";

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

/// `updating.marker` next to the exe, read by the watchdog: its modification
/// time and text. Setup (re)writes it once the old instances are gone
/// (before it re-enables the watchdog task), and also when it fails or is
/// cancelled after disabling the tasks; its crash recovery task does too.
/// Setup writes the boot it
/// was written in and the awake time then (`boot=<n> unbiased=<n>`), so the
/// watchdog can tell its age without trusting the wall clock (see watchdog.rs).
pub fn read_update_marker() -> Option<(SystemTime, String)> {
    let path = exe_dir()?.join(MARKER_FILE);
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    // Small; an unreadable text just means "no boot stamp".
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    Some((modified, text))
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
pub(crate) fn is_elevated() -> bool {
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

/// First look this long after startup, then every `TICK`: GitHub is asked
/// once per calendar day (see `check_due`), so a PC that's only on for an
/// hour or two a day still checks - and reminds - every day it's on.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(2 * 60);
const TICK: Duration = Duration::from_secs(10 * 60);
/// After a failed check (no network, GitHub error), wait at least this long
/// before asking GitHub again.
const RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// No check, and so no update message, before this local hour: a PC left on
/// overnight would otherwise ping the admin just after midnight.
const EARLIEST_HOUR: u16 = 7;

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
                    if let Some(text) = version_change_text(&previous, Version::current()) {
                        send_claimed_announcement(&text);
                    }
                }
                return;
            }
        }
    }
}

/// The admin's notice that the running version changed from `previous` (as
/// stored in the shared database) to `current`, if there's anything to say.
/// The stored version survives an uninstall, so a reinstall of an older
/// release lands here too: that is said as it is, not as an update.
fn version_change_text(previous: &str, current: Version) -> Option<String> {
    match Version::parse(previous) {
        Some(p) if p == current => None,
        Some(p) if p > current => Some(format!("⬇️ {} v{p} → v{current}", i18n::t("update.notify.downgraded"))),
        _ => Some(format!("✅ {} v{} → v{current}", i18n::t("update.notify.updated"), previous.trim())),
    }
}

fn daily_update_check() {
    std::thread::sleep(FIRST_CHECK_DELAY);
    // The local date this process last heard back from GitHub on.
    let mut checked_day: Option<String> = None;
    let mut last_failure: Option<Instant> = None;
    loop {
        let (today, hour) = local_day_and_hour();
        let failed_recently = last_failure.is_some_and(|t| t.elapsed() < RETRY_INTERVAL);
        if !failed_recently
            && check_due(&today, hour, checked_day.as_deref(), database::get_shared_setting(UPDATE_MESSAGE_DAY_KEY).as_deref())
        {
            match bot_state() {
                BotState::NoneEnabled | BotState::NotReady => {}
                BotState::Ready => match check_for_newer() {
                    Ok(newer) => {
                        checked_day = Some(today.clone());
                        last_failure = None;
                        if let Some(release) = newer {
                            notify_update_available(&release, &today);
                        }
                    }
                    Err(e) => {
                        last_failure = Some(Instant::now());
                        eprintln!("[Update] Daily check failed: {e}");
                    }
                },
            }
        }
        std::thread::sleep(TICK);
    }
}

/// Today's local date (YYYY-MM-DD) and hour, from one reading of the clock.
fn local_day_and_hour() -> (String, u16) {
    let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay), st.wHour)
}

/// Whether to ask GitHub now: not before `EARLIEST_HOUR`, at most once a day
/// per process (`checked_day`: the day this process last got an answer),
/// and not at all once today's update message has gone out from any
/// session (`message_day`, shared) - nothing more could be sent today.
fn check_due(today: &str, hour: u16, checked_day: Option<&str>, message_day: Option<&str>) -> bool {
    hour >= EARLIEST_HOUR && checked_day != Some(today) && message_day != Some(today)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateMessage {
    /// First news of this version.
    Announcement,
    /// The admin was told about this version on an earlier day.
    Reminder,
}

/// Claims today's update message about `latest`, with `swap` behaving like
/// `database::swap_shared_setting` (atomic; `Some(previous)` only when it
/// changed the value). `None` if one was already sent today - by any
/// session, account or earlier run on this PC. Otherwise an announcement if
/// no session has announced `latest` yet, else a reminder. A version first
/// seen on a day that already had its message is announced the next day.
fn claim_update_message(
    mut swap: impl FnMut(&str, &str) -> Option<Option<String>>,
    today: &str,
    latest: Version,
) -> Option<UpdateMessage> {
    swap(UPDATE_MESSAGE_DAY_KEY, today)?;
    Some(match swap(UPDATE_NOTIFIED_VERSION_KEY, &latest.to_string()) {
        Some(_) => UpdateMessage::Announcement,
        None => UpdateMessage::Reminder,
    })
}

/// The Telegram and Discord texts for an update message. Each names only
/// its own bot's command; Discord's tags the admin user, when one is set,
/// so it pings them (see watchdog.rs's `discord_allowed_mentions`).
/// Telegram's has no tag: the admin chat is normally the private chat with
/// the bot, where every message notifies.
fn update_message_texts(kind: UpdateMessage, latest: Version, installed: Version, discord_admin: Option<u64>) -> (String, String) {
    let headline = match kind {
        UpdateMessage::Announcement => format!("⬆️ {}", i18n::t("update.notify.available")),
        UpdateMessage::Reminder => format!("⏰ {}", i18n::t("update.notify.reminder")),
    };
    let body = format!("{headline} v{latest}\n{}: v{installed}\n", i18n::t("update.notify.current"));
    let tag = discord_admin.filter(|&id| id != 0).map(|id| format!("<@{id}> ")).unwrap_or_default();
    (
        format!("{body}{}", i18n::t("update.notify.how_tg")),
        format!("{tag}{body}{}", i18n::t("update.notify.how_dc")),
    )
}

/// Tells the admin about `release` - once a day per PC, until it's
/// installed (`check_for_newer` stops finding it then).
fn notify_update_available(release: &Release, today: &str) {
    if let Err(e) = installer_asset(release) {
        // The update command would only fail on it - don't advertise it.
        eprintln!("[Update] Not announcing v{}: {e}", release.version);
        return;
    }
    let Some(kind) = claim_update_message(database::swap_shared_setting, today, release.version) else {
        return;
    };
    let (tg_text, dc_text) =
        update_message_texts(kind, release.version, Version::current(), database::get_discord_config().admin_user_id);
    // Claimed, so through the alert queue like `send_claimed_announcement`,
    // but with each bot's own text.
    crate::watchdog::raise_alert_per_bot(&tg_text, &dc_text);
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
        // Through the alert queue: kept and retried if no bot is connected
        // in this session or the PC is offline right now.
        crate::watchdog::raise_alert(&text);
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

// ============================================================================
// `--secure-folders` (run elevated by setup, as an administrator or SYSTEM)
// ============================================================================

/// The permissions of the folders setup creates or relies on, applied by
/// setup through `screen-time-manager.exe --secure-folders --shared <dir>
/// --install <dir>` (run from setup's own temporary folder, not from the
/// install folder it checks).
///
/// The rule throughout: an elevated process acts only on an object it opened
/// itself *without following links* and checked is what it expected - never
/// on a path that a standard account could have pointed somewhere else. In
/// 1.0.2 setup ran `icacls <ProgramData>\ScreenTimeManager /grant Users:M`:
/// a standard account could delete that folder (Users had Modify on the
/// folder itself), put a junction of the same name to the install folder in
/// its place (ProgramData lets Users create folders), and the next setup -
/// which also runs as SYSTEM for updates - followed it and gave Users Modify
/// on the install folder, whose exe the SYSTEM update task runs.
pub mod folders {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, BOOL, BOOLEAN, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, GetAce, GetAclInformation, GetKernelObjectSecurity, GetSecurityDescriptorDacl,
        GetSecurityDescriptorOwner, InitializeAcl, LookupPrivilegeValueW, SetKernelObjectSecurity,
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
        DACL_SECURITY_INFORMATION, LUID_AND_ATTRIBUTES, OBJECT_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
        TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, FileAttributeTagInfo, FileDispositionInfo, GetFileInformationByHandleEx,
        SetFileInformationByHandle, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, READ_CONTROL,
        WRITE_DAC, WRITE_OWNER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub const ARG: &str = "--secure-folders";

    /// The shared config folder (`%ProgramData%\ScreenTimeManager`), owned by
    /// Administrators, not inheriting from ProgramData (which lets Users
    /// create folders and gives creators full control):
    /// - SYSTEM and Administrators: full control, inherited by everything in it;
    /// - Users, on the folder itself: list, traverse, read its attributes and
    ///   permissions, and create files (0x1200AB) - not delete, rename or
    ///   re-permission it, and not create subfolders (nothing in it needs one);
    /// - Users, on the files in it (inherit only): Modify (0x1301BF), so the
    ///   app in every account can create, write and delete `bot_config.db`
    ///   and SQLite's journal files, whoever created them.
    pub const SHARED_DIR_SDDL: &str =
        "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;0x1200ab;;;BU)(A;OIIO;0x1301bf;;;BU)";

    const SE_FILE_OBJECT: i32 = 1;
    const SDDL_REVISION_1: u32 = 1;
    const UNPROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x2000_0000;
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const INHERIT_ONLY_ACE: u8 = 0x08;
    const INHERITED_ACE: u8 = 0x10;
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    const ERROR_PATH_NOT_FOUND: u32 = 3;
    const ERROR_ALREADY_EXISTS: u32 = 183;

    /// Rights that let whoever has them change an object, its contents or
    /// its permissions: write/append data (= add file/subfolder on a
    /// folder), write extended attributes and attributes, delete child,
    /// delete, change permissions, take ownership, generic all/write.
    const WRITE_RIGHTS: u32 = 0x2 | 0x4 | 0x10 | 0x40 | 0x100 | 0x1_0000 | 0x4_0000 | 0x8_0000 | 0x1000_0000 | 0x4000_0000;

    const SYSTEM_SID: &str = "S-1-5-18";
    const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
    const TRUSTED_INSTALLER_SID: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
    const CREATOR_OWNER_SID: &str = "S-1-3-0";

    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl: PCWSTR,
            revision: u32,
            sd: *mut *mut c_void,
            size: *mut u32,
        ) -> BOOL;
        fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd: *const c_void,
            revision: u32,
            information: u32,
            sddl: *mut *mut u16,
            len: *mut u32,
        ) -> BOOL;
        fn ConvertSidToStringSidW(sid: *const c_void, string_sid: *mut *mut u16) -> BOOL;
        fn SetNamedSecurityInfoW(
            name: PCWSTR,
            object_type: i32,
            information: u32,
            owner: *const c_void,
            group: *const c_void,
            dacl: *const c_void,
            sacl: *const c_void,
        ) -> u32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// The Win32 error code inside a `windows` error (they are HRESULTs).
    fn win32_code(e: &windows::core::Error) -> u32 {
        let hr = e.code().0 as u32;
        if hr & 0xFFFF_0000 == 0x8007_0000 { hr & 0xFFFF } else { hr }
    }

    /// Memory the security functions allocate with LocalAlloc.
    struct LocalMem(*mut c_void);

    impl Drop for LocalMem {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0) };
            }
        }
    }

    fn sd_from_sddl(sddl: &str) -> Result<LocalMem, String> {
        let text: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(text.as_ptr()), SDDL_REVISION_1, &mut sd, std::ptr::null_mut())
        };
        if ok.as_bool() {
            Ok(LocalMem(sd))
        } else {
            Err(format!("bad security descriptor {sddl}: {}", std::io::Error::last_os_error()))
        }
    }

    /// The owner and DACL of a security descriptor, as SDDL.
    fn sddl_of(sd: *const c_void) -> Option<String> {
        let mut text = std::ptr::null_mut();
        let mut len = 0u32;
        let info = OWNER_SECURITY_INFORMATION.0 | DACL_SECURITY_INFORMATION.0;
        let ok = unsafe { ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, SDDL_REVISION_1, info, &mut text, &mut len) };
        if !ok.as_bool() || text.is_null() {
            return None;
        }
        let _free = LocalMem(text as *mut c_void);
        let chars = unsafe { std::slice::from_raw_parts(text, len as usize) };
        Some(String::from_utf16_lossy(chars).trim_end_matches('\0').to_string())
    }

    fn sid_to_string(sid: *const c_void) -> Option<String> {
        if sid.is_null() {
            return None;
        }
        let mut text = std::ptr::null_mut();
        if !unsafe { ConvertSidToStringSidW(sid, &mut text) }.as_bool() || text.is_null() {
            return None;
        }
        let _free = LocalMem(text as *mut c_void);
        let len = (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count();
        Some(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) }))
    }

    /// This process's user, as a SID string (S-1-5-21-...): the tests'
    /// stand-in for the administrators.
    #[cfg(test)]
    fn own_user_sid() -> Option<String> {
        use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_USER};
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
        let token = Handle(token);
        let mut len = 0u32;
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut len) };
        // u64s, so the buffer is aligned for TOKEN_USER.
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        unsafe { GetTokenInformation(token.0, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len) }.ok()?;
        let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
        sid_to_string(user.User.Sid.0)
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    /// Opens `path` itself - a link is opened as the link, never followed.
    /// Every share mode is allowed, so the app's own open files don't get in
    /// the way. With the backup and restore privileges enabled (see
    /// `enable_privileges`), access is granted whatever the object's DACL says.
    fn open_no_follow(path: &Path, access: u32) -> Result<Handle, windows::core::Error> {
        let name = wide(path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                HANDLE::default(),
            )
        }?;
        Ok(Handle(handle))
    }

    fn attributes(handle: &Handle) -> Result<FILE_ATTRIBUTE_TAG_INFO, String> {
        let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
        unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                FileAttributeTagInfo,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        }
        .map_err(|e| format!("could not read its attributes: {e}"))?;
        Ok(info)
    }

    fn is_link(info: &FILE_ATTRIBUTE_TAG_INFO) -> bool {
        info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    }

    fn is_dir(info: &FILE_ATTRIBUTE_TAG_INFO) -> bool {
        info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0
    }

    /// Deletes the object `handle` was opened on (a link itself, not its
    /// target) once the handle is closed.
    fn delete_on_close(handle: &Handle) -> Result<(), String> {
        let info = FILE_DISPOSITION_INFO { DeleteFile: BOOLEAN(1) };
        unsafe {
            SetFileInformationByHandle(
                handle.0,
                FileDispositionInfo,
                &info as *const _ as *const c_void,
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        }
        .map_err(|e| format!("could not delete it: {e}"))
    }

    /// Owner and DACL of the object `handle` was opened on (READ_CONTROL).
    fn read_security(handle: &Handle) -> Result<Vec<u64>, String> {
        let info = OWNER_SECURITY_INFORMATION.0 | DACL_SECURITY_INFORMATION.0;
        let mut needed = 0u32;
        let _ = unsafe { GetKernelObjectSecurity(handle.0, info, PSECURITY_DESCRIPTOR::default(), 0, &mut needed) };
        if needed == 0 {
            return Err(format!("could not read its permissions: {}", std::io::Error::last_os_error()));
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        unsafe { GetKernelObjectSecurity(handle.0, info, PSECURITY_DESCRIPTOR(buf.as_mut_ptr().cast()), needed, &mut needed) }
            .map_err(|e| format!("could not read its permissions: {e}"))?;
        Ok(buf)
    }

    /// Enables backup, restore and take-ownership for this process, so an
    /// object a standard account made (and may have locked administrators
    /// out of) can still be opened and re-permissioned. Names the ones that
    /// couldn't be enabled; elevated administrators and SYSTEM have them all.
    fn enable_privileges() -> Vec<&'static str> {
        let mut missing = Vec::new();
        let mut token = HANDLE::default();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) }.is_err() {
            return vec!["(token not adjustable)"];
        }
        let token = Handle(token);
        for name in ["SeBackupPrivilege", "SeRestorePrivilege", "SeTakeOwnershipPrivilege"] {
            let text: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let mut luid = LUID::default();
            let enabled = unsafe { LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(text.as_ptr()), &mut luid) }.is_ok() && {
                let privileges = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
                };
                unsafe { AdjustTokenPrivileges(token.0, false, Some(&privileges), 0, None, None) }.is_ok()
                    && unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED
            };
            if !enabled {
                missing.push(name);
            }
        }
        missing
    }

    /// Makes `path` a real folder with exactly the security in `sddl` (owner
    /// too, if it has an `O:` part), whatever was there before:
    /// - nothing: created, with that security from the start;
    /// - a link (junction, symbolic link, any reparse point): the link itself
    ///   is deleted - never followed - and a folder created in its place;
    /// - a file: deleted, and a folder created;
    /// - a folder: its owner and DACL are replaced through the handle it was
    ///   checked on, without being passed on to what is inside (nothing in it
    ///   is touched, so nothing in it can redirect this either).
    ///
    /// Returns what it did.
    pub fn secure_dir(path: &Path, sddl: &str) -> Result<String, String> {
        let sd = sd_from_sddl(sddl)?;
        let with_owner = sddl.starts_with("O:");
        let mut info = DACL_SECURITY_INFORMATION.0;
        let mut access = READ_CONTROL.0 | WRITE_DAC.0 | FILE_READ_ATTRIBUTES.0;
        if with_owner {
            info |= OWNER_SECURITY_INFORMATION.0;
            access |= WRITE_OWNER.0;
        }
        let mut done = Vec::new();
        // A standard account may keep putting things back in the way; a few
        // rounds are plenty for anything but that, which then fails safely.
        for _ in 0..5 {
            let handle = match open_no_follow(path, access) {
                Ok(h) => h,
                Err(e) if matches!(win32_code(&e), ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => {
                    let attributes = SECURITY_ATTRIBUTES {
                        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                        lpSecurityDescriptor: sd.0,
                        bInheritHandle: BOOL(0),
                    };
                    let name = wide(path);
                    match unsafe { CreateDirectoryW(PCWSTR(name.as_ptr()), Some(&attributes)) } {
                        Ok(()) => done.push("created it".to_string()),
                        Err(e) if win32_code(&e) == ERROR_ALREADY_EXISTS => {}
                        Err(e) => return Err(format!("could not create {}: {e}", path.display())),
                    }
                    continue;
                }
                Err(e) => return Err(format!("could not open {}: {e}", path.display())),
            };
            let found = attributes(&handle)?;
            if is_link(&found) || !is_dir(&found) {
                drop(handle);
                // Opened again for deleting, and checked again: it may have
                // changed in between.
                let doomed = open_no_follow(path, DELETE.0 | FILE_READ_ATTRIBUTES.0)
                    .map_err(|e| format!("could not open {} to remove it: {e}", path.display()))?;
                let again = attributes(&doomed)?;
                if is_link(&again) || !is_dir(&again) {
                    delete_on_close(&doomed).map_err(|e| format!("{}: {e}", path.display()))?;
                    done.push(if is_link(&again) {
                        format!("removed a link (reparse tag 0x{:08X}) that was in its place, without following it", again.ReparseTag)
                    } else {
                        "removed a file that was in its place".to_string()
                    });
                }
                continue;
            }
            unsafe { SetKernelObjectSecurity(handle.0, OBJECT_SECURITY_INFORMATION(info), PSECURITY_DESCRIPTOR(sd.0)) }
                .map_err(|e| format!("could not set the permissions of {}: {e}", path.display()))?;
            done.push("set its owner and permissions".to_string());
            return Ok(done.join(", "));
        }
        Err(format!("{} kept being replaced while it was being set up; nothing in its place was followed", path.display()))
    }

    /// Something in the install folder a standard account could use.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Problem {
        /// A link; nothing in the install folder is supposed to be one.
        Link,
        /// Owned by someone other than SYSTEM, Administrators or
        /// TrustedInstaller: created by a standard account.
        ForeignOwner(String),
        /// An allow entry giving `sid` rights to change it (`mask`).
        ForeignWrite { sid: String, mask: u32 },
        /// No DACL at all: everyone has full access.
        NoDacl,
        Unreadable(String),
    }

    /// The SIDs the install folder may belong to or grant write access to.
    fn admin_sid(sid: &str) -> bool {
        [SYSTEM_SID, ADMINISTRATORS_SID, TRUSTED_INSTALLER_SID].contains(&sid)
    }

    /// What's wrong with one object's owner and DACL (`sd`), given who may
    /// own or change it.
    fn problems_in(sd: &[u64], trusted: &dyn Fn(&str) -> bool) -> Vec<Problem> {
        let sd = PSECURITY_DESCRIPTOR(sd.as_ptr() as *mut c_void);
        let mut problems = Vec::new();
        let mut owner = PSID::default();
        let mut defaulted = BOOL(0);
        if unsafe { GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted) }.is_ok() {
            match sid_to_string(owner.0) {
                Some(sid) if trusted(&sid) => {}
                Some(sid) => problems.push(Problem::ForeignOwner(sid)),
                None => problems.push(Problem::Unreadable("owner".into())),
            }
        }
        let mut present = BOOL(0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        if unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) }.is_err() {
            problems.push(Problem::Unreadable("DACL".into()));
            return problems;
        }
        if !present.as_bool() || dacl.is_null() {
            problems.push(Problem::NoDacl);
            return problems;
        }
        let mut size = ACL_SIZE_INFORMATION::default();
        if unsafe {
            GetAclInformation(dacl, &mut size as *mut _ as *mut c_void, std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32, AclSizeInformation)
        }
        .is_err()
        {
            problems.push(Problem::Unreadable("DACL size".into()));
            return problems;
        }
        for index in 0..size.AceCount {
            let mut ace: *mut c_void = std::ptr::null_mut();
            if unsafe { GetAce(dacl, index, &mut ace) }.is_err() || ace.is_null() {
                continue;
            }
            let header = unsafe { &*(ace as *const ACE_HEADER) };
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
                continue;
            }
            let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            if allowed.Mask & WRITE_RIGHTS == 0 {
                continue;
            }
            // Inherited entries come from a parent: inside the tree that
            // parent is checked itself (and resetting it passes the result
            // down); above it is Program Files, which isn't ours to judge.
            if header.AceFlags & INHERITED_ACE != 0 {
                continue;
            }
            let Some(sid) = sid_to_string(&allowed.SidStart as *const u32 as *const c_void) else { continue };
            // An explicit "CREATOR OWNER: full control" for whatever an
            // administrator creates inside, as Program Files has.
            let creator_template = sid == CREATOR_OWNER_SID && header.AceFlags & INHERIT_ONLY_ACE != 0;
            if !trusted(&sid) && !creator_template {
                problems.push(Problem::ForeignWrite { sid, mask: allowed.Mask });
            }
        }
        problems
    }

    /// Every problem in the folder `root` and everything below it, without
    /// following links (a link is reported, not entered).
    fn audit(root: &Path, trusted: &dyn Fn(&str) -> bool) -> Vec<(PathBuf, Problem)> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            let handle = match open_no_follow(&path, READ_CONTROL.0 | FILE_READ_ATTRIBUTES.0) {
                Ok(h) => h,
                Err(e) => {
                    found.push((path, Problem::Unreadable(e.to_string())));
                    continue;
                }
            };
            let info = match attributes(&handle) {
                Ok(i) => i,
                Err(e) => {
                    found.push((path, Problem::Unreadable(e)));
                    continue;
                }
            };
            if is_link(&info) {
                found.push((path, Problem::Link));
                continue;
            }
            match read_security(&handle) {
                Ok(sd) => found.extend(problems_in(&sd, trusted).into_iter().map(|p| (path.clone(), p))),
                Err(e) => found.push((path.clone(), Problem::Unreadable(e))),
            }
            if is_dir(&info) {
                match std::fs::read_dir(&path) {
                    Ok(entries) => pending.extend(entries.flatten().map(|e| e.path())),
                    Err(e) => found.push((path, Problem::Unreadable(e.to_string()))),
                }
            }
        }
        found
    }

    /// Deletes `path` itself: a link without following it, a folder with
    /// everything in it (std's remove_dir_all doesn't follow links either).
    fn remove_no_follow(path: &Path) -> Result<(), String> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            let handle = open_no_follow(path, DELETE.0 | FILE_READ_ATTRIBUTES.0).map_err(|e| e.to_string())?;
            return delete_on_close(&handle);
        }
        if meta.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) }.map_err(|e| e.to_string())
    }

    /// Drops every entry of `path`'s DACL that isn't inherited, so it has
    /// only what its parent passes on, and passes that on to everything
    /// inside it (which `audit` has checked has no links left).
    fn reset_to_inherited(path: &Path) -> Result<(), String> {
        let mut empty = [0u32; 4];
        unsafe { InitializeAcl(empty.as_mut_ptr().cast(), std::mem::size_of::<ACL>() as u32, ACL_REVISION) }
            .map_err(|e| e.to_string())?;
        let name = wide(path);
        let status = unsafe {
            SetNamedSecurityInfoW(
                PCWSTR(name.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION.0 | UNPROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null(),
                std::ptr::null(),
                empty.as_ptr().cast(),
                std::ptr::null(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status as i32).to_string())
        }
    }

    fn describe(problems: &[(PathBuf, Problem)]) -> String {
        problems.iter().map(|(p, what)| format!("{}: {what:?}", p.display())).collect::<Vec<_>>().join("; ")
    }

    /// Checks that standard accounts can't change anything in the install
    /// folder `root` - setup's install folder, whose exe the SYSTEM update
    /// task runs - and puts it right if they can. Normally there is nothing
    /// to do: setup creates it under Program Files and it inherits that
    /// folder's permissions. A 1.0.x install whose shared config folder was
    /// replaced by a link to it (see the module docs) has an entry giving
    /// Users Modify, and maybe files and links a standard account put there.
    /// Those are removed (anything inside owned by someone other than
    /// SYSTEM, Administrators or TrustedInstaller was created by a standard
    /// account, so it's deleted), then the folder is set to inherit only from
    /// its parent, then each object that still has an entry of its own.
    pub fn check_install_dir(root: &Path, trusted: &dyn Fn(&str) -> bool) -> Result<String, String> {
        let found = audit(root, trusted);
        if found.is_empty() {
            return Ok("standard accounts can't change anything in it; nothing to do".into());
        }
        let first = describe(&found);
        if found.iter().any(|(p, problem)| p == root && *problem == Problem::Link) {
            return Err(format!("it is a link: {first}"));
        }
        for (path, problem) in &found {
            let planted = matches!(problem, Problem::Link | Problem::ForeignOwner(_));
            if path != root && planted && std::fs::symlink_metadata(path).is_ok() {
                remove_no_follow(path).map_err(|e| format!("could not remove {}: {e}", path.display()))?;
            }
        }
        if found.iter().any(|(p, problem)| p == root && matches!(problem, Problem::ForeignOwner(_))) {
            secure_owner(root)?;
        }
        reset_to_inherited(root).map_err(|e| format!("could not reset the permissions of {}: {e}", root.display()))?;
        for (path, _) in audit(root, trusted) {
            if path != *root {
                reset_to_inherited(&path).map_err(|e| format!("could not reset the permissions of {}: {e}", path.display()))?;
            }
        }
        let left = audit(root, trusted);
        if left.is_empty() {
            Ok(format!("repaired - it had: {first}"))
        } else {
            Err(format!("still not safe after repairing ({first}): {}", describe(&left)))
        }
    }

    /// Gives `path` (not a link - checked by the caller's audit) to Administrators.
    fn secure_owner(path: &Path) -> Result<(), String> {
        let handle = open_no_follow(path, WRITE_OWNER.0 | FILE_READ_ATTRIBUTES.0).map_err(|e| e.to_string())?;
        if is_link(&attributes(&handle)?) {
            return Err(format!("{} is a link", path.display()));
        }
        let sd = sd_from_sddl("O:BA")?;
        unsafe { SetKernelObjectSecurity(handle.0, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR(sd.0)) }
            .map_err(|e| format!("could not take ownership of {}: {e}", path.display()))
    }

    /// The owner and DACL of `path` (not following a link), as SDDL.
    pub fn sddl_of_path(path: &Path) -> Result<String, String> {
        let handle = open_no_follow(path, READ_CONTROL.0 | FILE_READ_ATTRIBUTES.0).map_err(|e| e.to_string())?;
        let sd = read_security(&handle)?;
        sddl_of(sd.as_ptr().cast()).ok_or_else(|| "could not convert its permissions".into())
    }

    /// `--secure-folders --shared <dir> [--install <dir>]`. Prints what it
    /// did, one line each, for setup's log. Exit code: 0 done; bit 1 set if
    /// the shared folder couldn't be set up, bit 2 if the install folder
    /// isn't safe (setup then doesn't register the SYSTEM update task); 64
    /// for bad arguments.
    pub fn run(args: &[String]) -> i32 {
        // Not println!, which panics (= aborts, in release) if stdout is gone.
        let say = |line: String| {
            use std::io::Write;
            let _ = writeln!(std::io::stdout(), "{line}");
        };
        let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(PathBuf::from);
        let (Some(shared), install) = (value("--shared"), value("--install")) else {
            say(format!("usage: {ARG} --shared <dir> [--install <dir>]"));
            return 64;
        };
        let missing = enable_privileges();
        if !missing.is_empty() {
            say(format!("could not enable {} (not elevated?)", missing.join(", ")));
        }
        let now = |path: &Path| sddl_of_path(path).unwrap_or_else(|e| format!("unreadable: {e}"));
        let mut code = 0;
        match secure_dir(&shared, SHARED_DIR_SDDL) {
            Ok(done) => say(format!("shared config folder {}: {done}; now {}", shared.display(), now(&shared))),
            Err(e) => {
                say(format!("ERROR: shared config folder {}: {e}", shared.display()));
                code |= 1;
            }
        }
        if let Some(install) = install {
            match check_install_dir(&install, &admin_sid) {
                Ok(done) => say(format!("install folder {}: {done}; now {}", install.display(), now(&install))),
                Err(e) => {
                    say(format!("ERROR: install folder {}: {e}", install.display()));
                    code |= 2;
                }
            }
        }
        code
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("stm-folders-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn junction(link: &Path, target: &Path) {
            // Needs no privilege, unlike a symbolic link: what a standard account would use.
            let out = std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(out.status.success(), "mklink /J: {}", String::from_utf8_lossy(&out.stderr));
        }

        /// The shared folder's security without the owner, which only an
        /// elevated process may set to Administrators.
        fn test_sddl() -> String {
            SHARED_DIR_SDDL.replacen("O:BA", "", 1)
        }

        fn dacl(path: &Path) -> String {
            let sddl = sddl_of_path(path).unwrap();
            sddl[sddl.find("D:").unwrap()..].to_string()
        }

        #[test]
        fn refuses_to_run_without_a_shared_folder() {
            let args: Vec<String> = ["screen-time-manager.exe", ARG, "--install", r"C:\x"].map(String::from).to_vec();
            assert_eq!(run(&args), 64);
        }

        #[test]
        fn shared_folder_sddl_parses_and_says_what_it_means() {
            assert!(sd_from_sddl(SHARED_DIR_SDDL).is_ok());
            // Users may create files but not delete, rename or re-permission
            // the folder, or add subfolders.
            let folder_itself = 0x1200ab;
            assert_eq!(folder_itself & (0x4 | 0x40 | 0x1_0000 | 0x4_0000 | 0x8_0000), 0);
            assert_eq!(folder_itself & 0x2, 0x2);
            // ...and Modify, including delete, on the files in it.
            assert_eq!(0x1301bf & 0x1_0000, 0x1_0000);
        }

        #[test]
        fn creates_a_missing_folder_with_its_permissions() {
            let base = scratch("missing");
            let dir = base.join("ScreenTimeManager");
            let done = secure_dir(&dir, &test_sddl()).unwrap();
            assert!(done.starts_with("created it"), "{done}");
            assert!(std::fs::symlink_metadata(&dir).unwrap().is_dir());
            assert_eq!(dacl(&dir), "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;;0x1200ab;;;BU)(A;OIIO;0x1301bf;;;BU)");
            std::fs::remove_dir_all(&base).unwrap();
        }

        #[test]
        fn replaces_a_junction_without_touching_its_target() {
            let base = scratch("junction");
            let target = base.join("Program Files ScreenTimeManager");
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("screen-time-manager.exe"), b"exe").unwrap();
            let target_before = sddl_of_path(&target).unwrap();
            let exe_before = sddl_of_path(&target.join("screen-time-manager.exe")).unwrap();

            let dir = base.join("ScreenTimeManager");
            junction(&dir, &target);
            let done = secure_dir(&dir, &test_sddl()).unwrap();
            assert!(done.contains("removed a link (reparse tag 0xA0000003)"), "{done}");

            let meta = std::fs::symlink_metadata(&dir).unwrap();
            assert!(meta.is_dir() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0, "a real folder now");
            assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "a new, empty folder");
            assert_eq!(sddl_of_path(&target).unwrap(), target_before, "the link's target is untouched");
            assert_eq!(sddl_of_path(&target.join("screen-time-manager.exe")).unwrap(), exe_before);
            assert_eq!(std::fs::read(target.join("screen-time-manager.exe")).unwrap(), b"exe");
            std::fs::remove_dir_all(&base).unwrap();
        }

        #[test]
        fn replaces_a_file_and_keeps_an_existing_folders_contents() {
            let base = scratch("file");
            let dir = base.join("ScreenTimeManager");
            std::fs::write(&dir, b"in the way").unwrap();
            let done = secure_dir(&dir, &test_sddl()).unwrap();
            assert!(done.contains("removed a file"), "{done}");
            assert!(dir.is_dir());

            // An existing folder (an earlier version's, with the database in
            // it): re-permissioned, contents left exactly as they were.
            std::fs::write(dir.join("bot_config.db"), b"settings").unwrap();
            let db_before = sddl_of_path(&dir.join("bot_config.db")).unwrap();
            let done = secure_dir(&dir, &test_sddl()).unwrap();
            assert_eq!(done, "set its owner and permissions");
            assert_eq!(std::fs::read(dir.join("bot_config.db")).unwrap(), b"settings");
            assert_eq!(sddl_of_path(&dir.join("bot_config.db")).unwrap(), db_before, "not passed on to what's inside");
            assert!(dacl(&dir).starts_with("D:P("), "doesn't inherit from its parent: {}", dacl(&dir));
            std::fs::remove_dir_all(&base).unwrap();
        }

        #[test]
        fn install_folder_check_removes_links_and_foreign_write_access() {
            let me = own_user_sid().expect("own SID");
            assert!(me.starts_with("S-1-5-"), "{me}");
            // Here the test account plays the administrators.
            let trusted = |sid: &str| admin_sid(sid) || sid == me;

            let base = scratch("install");
            let app = base.join("ScreenTimeManager");
            std::fs::create_dir(&app).unwrap();
            std::fs::create_dir(app.join("updates")).unwrap();
            std::fs::write(app.join("screen-time-manager.exe"), b"exe").unwrap();
            assert_eq!(check_install_dir(&app, &trusted).unwrap(), "standard accounts can't change anything in it; nothing to do");

            // What 1.0.2's icacls did through a junction, plus a link put inside.
            let outside = base.join("elsewhere");
            std::fs::create_dir(&outside).unwrap();
            let outside_before = sddl_of_path(&outside).unwrap();
            junction(&app.join("updates").join("link"), &outside);
            let out = std::process::Command::new("icacls").arg(&app).args(["/grant", "*S-1-5-32-545:(OI)(CI)M"]).output().unwrap();
            assert!(out.status.success());
            let problems = audit(&app, &trusted);
            assert!(problems.iter().any(|(p, what)| *p == app && matches!(what, Problem::ForeignWrite { sid, .. } if sid == "S-1-5-32-545")), "{problems:?}");
            assert!(problems.iter().any(|(_, what)| *what == Problem::Link), "{problems:?}");

            let done = check_install_dir(&app, &trusted).unwrap();
            assert!(done.starts_with("repaired"), "{done}");
            assert!(audit(&app, &trusted).is_empty());
            assert!(!app.join("updates").join("link").exists() && std::fs::symlink_metadata(app.join("updates").join("link")).is_err());
            assert_eq!(sddl_of_path(&outside).unwrap(), outside_before, "the link's target is untouched");
            assert_eq!(std::fs::read(app.join("screen-time-manager.exe")).unwrap(), b"exe");
            assert!(!sddl_of_path(&app.join("screen-time-manager.exe")).unwrap().contains(";;;BU)"), "Users' Modify is gone from the files too");
            std::fs::remove_dir_all(&base).unwrap();
        }

        #[test]
        fn install_folder_that_is_a_link_is_refused() {
            let base = scratch("install-link");
            let target = base.join("target");
            std::fs::create_dir(&target).unwrap();
            let app = base.join("ScreenTimeManager");
            junction(&app, &target);
            let me = own_user_sid().unwrap();
            let err = check_install_dir(&app, &|sid: &str| admin_sid(sid) || sid == me).unwrap_err();
            assert!(err.starts_with("it is a link"), "{err}");
            assert!(std::fs::symlink_metadata(&app).is_ok(), "left alone for an administrator to look at");
            std::fs::remove_dir_all(&base).unwrap();
        }
    }
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
    fn checks_github_once_a_day_from_seven() {
        let day = "2026-11-16";
        assert!(check_due(day, 7, None, None), "just started");
        assert!(check_due(day, 19, None, Some("2026-11-15")), "evening-only PC: yesterday's message doesn't count");
        assert!(check_due(day, 8, Some("2026-11-15"), None), "running since yesterday: asks again today");
        assert!(!check_due(day, 12, Some(day), None), "this process already asked today");
        assert!(!check_due(day, 12, None, Some(day)), "today's message already sent from somewhere on this PC");
        assert!(!check_due(day, 0, Some("2026-11-15"), None), "left on overnight: not just after midnight");
        assert!(!check_due(day, EARLIEST_HOUR - 1, None, None));
    }

    /// `database::swap_shared_setting` over a map, for `claim_update_message`.
    fn shared_settings() -> std::cell::RefCell<std::collections::HashMap<String, String>> {
        Default::default()
    }

    fn claim(db: &std::cell::RefCell<std::collections::HashMap<String, String>>, today: &str, latest: &str) -> Option<UpdateMessage> {
        let swap = |key: &str, value: &str| {
            let previous = db.borrow_mut().insert(key.to_string(), value.to_string());
            (previous.as_deref() != Some(value)).then_some(previous)
        };
        claim_update_message(swap, today, v(latest))
    }

    #[test]
    fn one_update_message_a_day_until_installed() {
        use UpdateMessage::*;
        let db = shared_settings();
        // v1.1.0 appears: announced, and that's the day's message - other
        // accounts, sessions and restarts that day stay quiet.
        assert_eq!(claim(&db, "2026-11-16", "1.1.0"), Some(Announcement));
        assert_eq!(claim(&db, "2026-11-16", "1.1.0"), None);
        assert_eq!(claim(&db, "2026-11-16", "1.1.0"), None);
        // Every later day the PC is on: one reminder, whatever the gap.
        assert_eq!(claim(&db, "2026-11-17", "1.1.0"), Some(Reminder));
        assert_eq!(claim(&db, "2026-11-17", "1.1.0"), None);
        assert_eq!(claim(&db, "2026-11-20", "1.1.0"), Some(Reminder));
        // A newer release is announced on its first day...
        assert_eq!(claim(&db, "2026-11-21", "1.2.0"), Some(Announcement));
        assert_eq!(claim(&db, "2026-11-22", "1.2.0"), Some(Reminder));
        // ...or, if that day's message has gone out already, the next day.
        assert_eq!(claim(&db, "2026-11-22", "1.3.0"), None);
        assert_eq!(claim(&db, "2026-11-23", "1.3.0"), Some(Announcement));

        // Released versions recorded the announcement before this existed:
        // the next day's message is a reminder, not a second announcement.
        let db = shared_settings();
        db.borrow_mut().insert(UPDATE_NOTIFIED_VERSION_KEY.into(), "1.1.0".into());
        assert_eq!(claim(&db, "2026-11-16", "1.1.0"), Some(Reminder));

        // The shared database can't be written: nothing is sent.
        assert_eq!(claim_update_message(|_: &str, _: &str| None, "2026-11-16", v("1.1.0")), None);
    }

    #[test]
    fn update_messages_name_each_bots_own_command_and_tag_only_on_discord() {
        let (latest, installed) = (v("1.1.0"), v("1.0.1"));
        for kind in [UpdateMessage::Announcement, UpdateMessage::Reminder] {
            let (tg, dc) = update_message_texts(kind, latest, installed, Some(123_456_789_012_345_678));
            for text in [&tg, &dc] {
                assert!(text.contains("v1.1.0") && text.contains("Installed: v1.0.1"), "{text}");
            }
            assert!(tg.ends_with("Send /update to install it.") && !tg.contains("!update"), "{tg}");
            assert!(dc.ends_with("Send !update to install it.") && !dc.contains("/update"), "{dc}");
            assert!(!tg.contains('<') && !tg.contains('@'), "no Discord mention on Telegram: {tg}");
            assert!(dc.starts_with("<@123456789012345678> "), "{dc}");
            assert_eq!(dc.matches('@').count(), 1, "nothing else that could ping: {dc}");
            assert_ne!(tg, dc, "the queue tells same-second alerts apart by text");

            // No admin user configured: no tag at all.
            let (_, untagged) = update_message_texts(kind, latest, installed, None);
            assert!(!untagged.contains('@') && !untagged.contains('<'), "{untagged}");
            assert_eq!(update_message_texts(kind, latest, installed, Some(0)).1, untagged);
        }
        let (announcement, _) = update_message_texts(UpdateMessage::Announcement, latest, installed, None);
        let (reminder, _) = update_message_texts(UpdateMessage::Reminder, latest, installed, None);
        assert!(announcement.starts_with("⬆️ A new version"), "{announcement}");
        assert_eq!(reminder, "⏰ Reminder: Screen Time Manager has an update waiting: v1.1.0\nInstalled: v1.0.1\nSend /update to install it.");
    }

    #[test]
    fn version_change_is_worded_by_direction() {
        let now = v("1.0.3");
        assert_eq!(version_change_text("1.0.2", now).as_deref(), Some("✅ Screen Time Manager was updated: v1.0.2 → v1.0.3"));
        // The recorded version outlives an uninstall: a reinstall of an
        // older release isn't an update.
        assert_eq!(
            version_change_text("1.1.0", now).as_deref(),
            Some("⬇️ Screen Time Manager was changed to an older version: v1.1.0 → v1.0.3")
        );
        assert_eq!(version_change_text("1.0.3", now), None);
        // Something unrecognisable recorded: said as before.
        assert_eq!(version_change_text("dev", now).as_deref(), Some("✅ Screen Time Manager was updated: vdev → v1.0.3"));
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
