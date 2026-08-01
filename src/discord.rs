//! Discord bot module for Screen Time Manager
//! Provides remote monitoring and control via a Discord bot listening in one channel

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serenity::all::{ChannelId, GatewayIntents, Http, Message, Ready};
use serenity::async_trait;
use serenity::client::{Client, Context, EventHandler};

use crate::database;
use crate::i18n;
use crate::remote_commands::{
    cmd_extend, cmd_getpin, cmd_history, cmd_lock, cmd_msg, cmd_pause, cmd_reduce, cmd_reset,
    cmd_resume, cmd_rotatingpin, cmd_setlimit, cmd_setmessage, cmd_setpin, cmd_status, cmd_time,
    cmd_unlock, cmd_weekly,
};
use crate::time_request;

/// Shutdown signal for graceful termination
pub static BOT_SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Whether a bot thread is currently running (or starting up) - guards
/// start_bot_thread/stop_bot_thread so repeated calls (e.g. from the
/// per-second active-session check in session.rs) are cheap no-ops once
/// already in the desired state.
static BOT_RUNNING: AtomicBool = AtomicBool::new(false);

/// HTTP handle for sending notifications outside of the gateway event loop.
/// A Mutex rather than a OnceLock because the bot can be stopped and
/// restarted within the same process - e.g. when this session loses/regains
/// console ownership under Fast User Switching (see session.rs) - and
/// OnceLock can only ever be set once.
static BOT_HTTP: Mutex<Option<Arc<Http>>> = Mutex::new(None);

/// Handle for the most recently spawned bot thread, if any. `start_bot_thread`
/// joins this before spawning a new one (see there for why) - BOT_SHUTDOWN is
/// a single flag shared across every generation of the bot thread, so without
/// this join, a new generation could reset it to `false` while the *previous*
/// generation's shutdown-watcher task and Serenity client are still mid-
/// teardown, leaving two overlapping Discord gateway clients alive at once.
static BOT_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// Channel notifications are posted to
static NOTIFY_CHANNEL_ID: OnceLock<u64> = OnceLock::new();

struct Handler {
    channel_id: u64,
    admin_user_id: Option<u64>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        eprintln!("[Discord] Bot connected as {}", ready.user.name);
        // Fires on every (re)connect, including a session-handoff reconnect
        // under Fast User Switching (see session.rs) - so this doubles as an
        // "active user changed" notice, not just a genuine app startup one.
        let text = format!(
            "{}\n👤 {}: {}",
            i18n::t("tg.notify.started"),
            i18n::t("tg.status.user"),
            crate::remote_commands::current_windows_username(),
        );
        let _ = ChannelId::new(self.channel_id).say(&ctx.http, text).await;
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        if msg.channel_id.get() != self.channel_id {
            return;
        }
        let Some(rest) = msg.content.strip_prefix('!') else {
            return;
        };
        let mut parts = rest.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("").to_lowercase();
        let args = parts.next().unwrap_or("").trim();

        // Always answer !whoami so a user can discover their own ID during setup,
        // even before an admin user is configured.
        if cmd == "whoami" {
            let _ = msg
                .channel_id
                .say(&ctx.http, format!("{} {}", i18n::t("dc.whoami.your_id"), msg.author.id))
                .await;
            return;
        }

        match self.admin_user_id {
            None => {
                let _ = msg.channel_id.say(&ctx.http, i18n::t("dc.error.no_admin")).await;
                return;
            }
            Some(admin_id) if admin_id != msg.author.id.get() => {
                // Silently ignore other users' commands - this is a shared channel,
                // not a 1:1 DM like Telegram, so we don't want to spam it with
                // "unauthorized" replies to every message from unrelated members.
                return;
            }
            Some(_) => {}
        }

        // Commands that dismiss/grant time on the lock screen resolve any
        // pending "request more time" from the blocking overlay - figure out
        // beforehand whether this one actually qualifies (mirrors the
        // success conditions the command itself checks internally).
        let parsed_extend_mins = if cmd == "extend" { args.parse::<i32>().ok() } else { None };
        let grant_detail: Option<String> = match cmd.as_str() {
            "extend" => parsed_extend_mins
                .filter(|m| *m > 0 && *m <= 120)
                .map(|m| format!("+{m} min")),
            "e30" => Some("+30 min".to_string()),
            "e60" => Some("+60 min".to_string()),
            "e120" => Some("+120 min".to_string()),
            "reset" => Some("reset to daily limit".to_string()),
            "unlock" if crate::blocking::is_blocking_overlay_visible()
                && crate::blocking::get_remaining_seconds() > 0 => Some("unlocked".to_string()),
            _ => None,
        };

        let response = match cmd.as_str() {
            "status" => cmd_status(),
            "time" => cmd_time(),
            "extend" => dispatch_minutes(args, cmd_extend),
            "reduce" => dispatch_minutes(args, cmd_reduce),
            "pause" => cmd_pause(),
            "resume" => cmd_resume(),
            "history" => cmd_history(),
            "weekly" => cmd_weekly(),
            "msg" => cmd_msg(args),
            "lock" | "stop" => cmd_lock(),
            "unlock" => cmd_unlock(),
            "reset" => cmd_reset(),
            "setmessage" => cmd_setmessage(args),
            "setpin" => cmd_setpin(args),
            "setlimit" => cmd_setlimit(args),
            "rotatingpin" => cmd_rotatingpin(args),
            "getpin" => cmd_getpin(),
            "e30" => cmd_extend(30),
            "e60" => cmd_extend(60),
            "e120" => cmd_extend(120),
            "help" => help_text(),
            _ => i18n::t("dc.error.unknown_cmd").to_string(),
        };

        if let Some(detail) = grant_detail {
            time_request::resolve_if_pending("Discord", &detail);
        }

        let _ = msg.channel_id.say(&ctx.http, response).await;
    }
}

fn dispatch_minutes(args: &str, f: impl Fn(i32) -> String) -> String {
    match args.parse::<i32>() {
        Ok(mins) => f(mins),
        Err(_) => i18n::t("dc.error.specify_number").to_string(),
    }
}

fn help_text() -> String {
    let commands = [
        ("!status", i18n::t("dc.cmd.status")),
        ("!time", i18n::t("dc.cmd.time")),
        ("!extend <mins>", i18n::t("dc.cmd.extend")),
        ("!reduce <mins>", i18n::t("dc.cmd.reduce")),
        ("!pause", i18n::t("dc.cmd.pause")),
        ("!resume", i18n::t("dc.cmd.resume")),
        ("!history", i18n::t("dc.cmd.history")),
        ("!weekly", i18n::t("dc.cmd.weekly")),
        ("!msg <text>", i18n::t("dc.cmd.msg")),
        ("!lock", i18n::t("dc.cmd.lock")),
        ("!unlock", i18n::t("dc.cmd.unlock")),
        ("!reset", i18n::t("dc.cmd.reset")),
        ("!setmessage <text>", i18n::t("dc.cmd.setmessage")),
        ("!setpin <pin>", i18n::t("dc.cmd.setpin")),
        ("!setlimit <mins>", i18n::t("dc.cmd.setlimit")),
        ("!rotatingpin <on|off>", i18n::t("dc.cmd.rotatingpin")),
        ("!getpin", i18n::t("dc.cmd.getpin")),
        ("!e30", i18n::t("dc.cmd.e30")),
        ("!e60", i18n::t("dc.cmd.e60")),
        ("!e120", i18n::t("dc.cmd.e120")),
        ("!whoami", i18n::t("dc.cmd.whoami")),
        ("!help", i18n::t("dc.cmd.help")),
    ];

    let mut response = format!("{}\n", i18n::t("dc.help.header"));
    for (cmd, desc) in commands {
        response.push_str(&format!("{} - {}\n", cmd, desc));
    }
    response
}

/// Start the Discord bot in a background thread. A no-op if a bot thread is
/// already running/starting - safe to call repeatedly (e.g. from the
/// per-second active-session check in session.rs).
pub fn start_bot_thread() {
    if BOT_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }

    let config = database::get_discord_config();

    if !config.enabled {
        eprintln!("[Discord] Bot is disabled in settings");
        BOT_RUNNING.store(false, Ordering::SeqCst);
        return;
    }

    let Some(token) = config.bot_token else {
        eprintln!("[Discord] Bot enabled but no token configured");
        BOT_RUNNING.store(false, Ordering::SeqCst);
        return;
    };
    if token.is_empty() {
        eprintln!("[Discord] Bot token is empty");
        BOT_RUNNING.store(false, Ordering::SeqCst);
        return;
    }

    let Some(channel_id) = config.channel_id else {
        eprintln!("[Discord] Bot enabled but no channel configured");
        BOT_RUNNING.store(false, Ordering::SeqCst);
        return;
    };

    let admin_user_id = config.admin_user_id;

    // Wait for the previous generation's thread to fully exit before
    // resetting BOT_SHUTDOWN and spawning a new one - see BOT_THREAD.
    // BOT_RUNNING having just gone false->true above already means no other
    // caller can be in this function concurrently, so this handle (if any)
    // is exactly the generation we need to wait out.
    if let Some(handle) = BOT_THREAD.lock().unwrap().take() {
        let _ = handle.join();
    }

    BOT_SHUTDOWN.store(false, Ordering::SeqCst);

    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        rt.block_on(async {
            run_bot(token, channel_id, admin_user_id).await;
        });
        *BOT_HTTP.lock().unwrap() = None;
        BOT_RUNNING.store(false, Ordering::SeqCst);
    });
    *BOT_THREAD.lock().unwrap() = Some(handle);
}

/// Stop the Discord bot thread, if one is running. A no-op otherwise - safe
/// to call repeatedly. Unlike `signal_shutdown`, this doesn't send a
/// notification: it's used for routine active-session handoffs (see
/// session.rs), not an actual app shutdown.
pub fn stop_bot_thread() {
    if BOT_RUNNING.load(Ordering::SeqCst) {
        BOT_SHUTDOWN.store(true, Ordering::SeqCst);
    }
}

/// Proactively push a message to the configured channel, outside of any
/// incoming command (e.g. a "requesting more time" notification triggered
/// from the blocking overlay). No-op if the bot isn't connected/configured.
pub fn notify_admin(text: &str) {
    let http = BOT_HTTP.lock().unwrap().clone();
    if let (Some(http), Some(&channel_id)) = (http, NOTIFY_CHANNEL_ID.get()) {
        let text = text.to_string();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().ok();
            if let Some(rt) = rt {
                rt.block_on(async {
                    let _ = ChannelId::new(channel_id).say(&http, text).await;
                });
            }
        });
    }
}

/// Signal the bot to shut down gracefully
pub fn signal_shutdown() {
    BOT_SHUTDOWN.store(true, Ordering::SeqCst);

    let http = BOT_HTTP.lock().unwrap().clone();
    if let (Some(http), Some(&channel_id)) = (http, NOTIFY_CHANNEL_ID.get()) {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().ok();
            if let Some(rt) = rt {
                rt.block_on(async {
                    let _ = ChannelId::new(channel_id)
                        .say(&http, i18n::t("tg.notify.shutdown"))
                        .await;
                });
            }
        });
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Main bot loop
async fn run_bot(token: String, channel_id: u64, admin_user_id: Option<u64>) {
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    let handler = Handler { channel_id, admin_user_id };

    let mut client = match Client::builder(&token, intents).event_handler(handler).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[Discord] Failed to create client: {e}");
            return;
        }
    };

    *BOT_HTTP.lock().unwrap() = Some(client.http.clone());
    let _ = NOTIFY_CHANNEL_ID.set(channel_id);

    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        while !BOT_SHUTDOWN.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        shard_manager.shutdown_all().await;
    });

    if let Err(e) = client.start().await {
        eprintln!("[Discord] Client error: {e}");
    }
}
