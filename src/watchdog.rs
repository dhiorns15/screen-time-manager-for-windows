//! Watchdog check - invoked as `screen-time-manager.exe --watchdog-check`
//! by a Scheduled Task that fires every minute (registered by install.ps1).
//!
//! Each run is a brief, separate process: it checks whether the main app's
//! single-instance mutex is currently held, and if not - and the app wasn't
//! stopped intentionally via the passcode-protected Quit menu item - relaunches
//! it and alerts the configured admin via Telegram/Discord. This exists
//! because a standard Windows user can always kill their own processes
//! (admin rights aren't required to end-task something running in your own
//! session), so the enforcement can't rely solely on the tray app staying alive.

use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS},
        System::Threading::CreateMutexW,
    },
};

use crate::constants::MUTEX_NAME;
use crate::database;
use crate::i18n;
use crate::remote_commands::current_windows_username;

pub const WATCHDOG_ARG: &str = "--watchdog-check";

/// Check whether the app is running and relaunch/alert if it was killed
/// outside of a sanctioned Quit. Safe to call repeatedly (e.g. every minute).
pub fn run_check() {
    if unsafe { app_is_running() } {
        return;
    }

    if database::recent_intentional_quit() {
        log("app not running - recent intentional quit, not alerting");
        return;
    }

    let relaunched = relaunch_app();
    database::init_shared_database();
    log(&format!("app not running - relaunched={relaunched}, sending tamper alert"));
    send_tamper_alert(relaunched);
}

/// Small append-only log, since a Scheduled-Task-launched process has no
/// visible console to eprintln into - useful for diagnosing why a given run
/// did or didn't relaunch/alert.
fn log(msg: &str) {
    use std::io::Write;

    let path = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".screen-time-manager")
        .join("watchdog.log");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{now}] {msg}");
    }
}

unsafe fn app_is_running() -> bool {
    let mutex_name: Vec<u16> = MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    match CreateMutexW(None, BOOL::from(true), PCWSTR(mutex_name.as_ptr())) {
        Ok(h) => {
            let already_running = GetLastError() == ERROR_ALREADY_EXISTS;
            let _ = CloseHandle(h);
            already_running
        }
        Err(_) => {
            // Couldn't even attempt the check - assume running rather than
            // risk spawning a duplicate instance or a false alert.
            true
        }
    }
}

/// Name of the main app's Scheduled Task, as registered by install.ps1.
const MAIN_TASK_NAME: &str = "ScreenTimeManager";

fn relaunch_app() -> bool {
    // Ask Task Scheduler to start the app's own registered task rather than
    // spawning it as a direct child of this watchdog check: a direct child
    // inherits the watchdog task's Job Object (and any policy Windows
    // applies to it), and CREATE_BREAKAWAY_FROM_JOB only works if that job
    // explicitly allows breakaway - it doesn't here, so CreateProcess just
    // fails outright. The main task has its own separate job with no
    // execution time limit, so this sidesteps the problem entirely.
    if relaunch_via_scheduled_task() {
        return true;
    }
    // Fallback for the unlikely case the scheduled task isn't registered
    // (e.g. install.ps1 was never run) - not ideal if this process happens
    // to be job-limited, but better than not relaunching at all.
    relaunch_by_spawning()
}

fn relaunch_via_scheduled_task() -> bool {
    std::process::Command::new("schtasks")
        .args(["/Run", "/TN", MAIN_TASK_NAME])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn relaunch_by_spawning() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    std::process::Command::new(exe).spawn().is_ok()
}

fn send_tamper_alert(relaunched: bool) {
    let username = current_windows_username();
    let status = if relaunched {
        i18n::t("watchdog.alert.restarted")
    } else {
        i18n::t("watchdog.alert.restart_failed")
    };
    let text = format!(
        "⚠️ {} {} {}\n{}",
        i18n::t("watchdog.alert.prefix"),
        username,
        status,
        i18n::t("watchdog.alert.suffix"),
    );

    let tg = database::get_telegram_config();
    let dc = database::get_discord_config();

    if tg.enabled {
        if let (Some(token), Some(chat_id)) = (tg.bot_token, tg.admin_chat_id) {
            send_telegram_alert(&token, chat_id, &text);
        }
    }
    if dc.enabled {
        if let (Some(token), Some(channel_id)) = (dc.bot_token, dc.channel_id) {
            send_discord_alert(&token, channel_id, &text);
        }
    }
}

/// One-off Telegram message via plain HTTP - no bot/dispatcher needed for a
/// single send, same approach as the setup wizard's test message.
fn send_telegram_alert(token: &str, chat_id: i64, text: &str) {
    let url = format!(
        "https://api.telegram.org/bot{}/sendMessage?chat_id={}&text={}",
        token,
        chat_id,
        urlencoding::encode(text)
    );
    let _ = ureq::get(&url).timeout(std::time::Duration::from_secs(10)).call();
}

/// One-off Discord message via the REST API - doesn't need the Gateway
/// connection the main app's bot uses, same approach as the setup wizard.
fn send_discord_alert(token: &str, channel_id: u64, text: &str) {
    let Ok(rt) = tokio::runtime::Runtime::new() else { return };
    rt.block_on(async {
        let http = serenity::http::Http::new(token);
        let _ = serenity::model::id::ChannelId::new(channel_id).say(&http, text).await;
    });
}
