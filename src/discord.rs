//! Discord bot module for Screen Time Manager
//! Provides remote monitoring and control via a Discord bot listening in one channel

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serenity::all::{ChannelId, CreateMessage, GatewayIntents, Http, Message, Ready};
use serenity::async_trait;
use serenity::client::{Client, Context, EventHandler};

use crate::database;
use crate::i18n;
use crate::remote_commands::{
    cmd_extend, cmd_getpin, cmd_history, cmd_lock, cmd_msg, cmd_pause, cmd_reduce, cmd_reset,
    cmd_resume, cmd_rotatingpin, cmd_setlimit, cmd_setmessage, cmd_setpin, cmd_status, cmd_time,
    cmd_unlock, cmd_update, cmd_weekly, BotLifecycle, StartAction, WeeklyMarkup,
};
use crate::time_request;

/// Whether a bot thread is running, whether it's been told to stop, and
/// whether a start is queued behind a stop that's still in progress - see
/// `BotLifecycle`. Makes start_bot_thread/stop_bot_thread cheap no-ops once
/// already in the desired state.
static LIFECYCLE: BotLifecycle = BotLifecycle::new();

/// HTTP handle for sending notifications outside of the gateway event loop.
/// A Mutex rather than a OnceLock because the bot can be stopped and
/// restarted within the same process - e.g. when this session loses/regains
/// console ownership under Fast User Switching (see session.rs) - and
/// OnceLock can only ever be set once.
static BOT_HTTP: Mutex<Option<Arc<Http>>> = Mutex::new(None);

/// Handle for the most recently spawned bot thread, if any. `start_bot_thread`
/// joins this before spawning a new one (see there for why) - the shutdown
/// flag is a single flag shared across every generation of the bot thread, so
/// without this join, a new generation could reset it to `false` while the *previous*
/// generation's shutdown-watcher task and Serenity client are still mid-
/// teardown, leaving two overlapping Discord gateway clients alive at once.
static BOT_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// Channel notifications are posted to. A Mutex rather than a OnceLock for
/// the same reason as BOT_HTTP above - it also needs to track the current
/// config, not just the first-ever value, since the admin could change the
/// configured channel in Settings while a bot generation from before that
/// change is still using the old one, or across a session-handoff restart.
static NOTIFY_CHANNEL_ID: Mutex<Option<u64>> = Mutex::new(None);

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
            "weekly" => cmd_weekly(WeeklyMarkup::DiscordMarkdown),
            "msg" => cmd_msg(args),
            "lock" | "stop" => cmd_lock(),
            "unlock" => cmd_unlock(),
            "reset" => cmd_reset(),
            "setmessage" => cmd_setmessage(args),
            "setpin" => setpin_and_delete(&ctx, &msg, args).await,
            "setlimit" => cmd_setlimit(args),
            "rotatingpin" => cmd_rotatingpin(args),
            "getpin" => getpin_by_dm(&ctx, &msg).await,
            // Talks to GitHub (and schtasks) - off this single-threaded
            // runtime, so the gateway connection isn't stalled meanwhile.
            "update" => tokio::task::spawn_blocking(cmd_update)
                .await
                .unwrap_or_else(|e| format!("❌ {e}")),
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

/// `!setpin`: the new passcode is in the admin's own message, which everyone
/// in the channel can read, so delete it first. That needs the Manage
/// Messages permission in the channel, which the setup wizard doesn't ask
/// for - if it's missing, say so, so the admin can delete it by hand.
async fn setpin_and_delete(ctx: &Context, msg: &Message, args: &str) -> String {
    let deleted = match msg.delete(ctx).await {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[Discord] Couldn't delete the !setpin message: {e}");
            false
        }
    };
    let mut response = cmd_setpin(args);
    if !deleted {
        response.push_str("\n⚠️ ");
        response.push_str(i18n::t("dc.setpin.not_deleted"));
    }
    response
}

/// `!getpin`: today's rotating PIN goes to the admin by direct message, not
/// into the shared channel. Only a short note is posted in the channel. If
/// the rotating PIN is off, the reply has no code in it and is posted as
/// before.
async fn getpin_by_dm(ctx: &Context, msg: &Message) -> String {
    if !database::is_rotating_pin_enabled() {
        return cmd_getpin();
    }
    match msg.author.direct_message(ctx, CreateMessage::new().content(cmd_getpin())).await {
        Ok(_) => format!("📬 {}", i18n::t("dc.getpin.sent_dm")),
        Err(e) => {
            eprintln!("[Discord] Couldn't send the PIN by direct message: {e}");
            format!("❌ {}", i18n::t("dc.getpin.dm_failed"))
        }
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
        ("!update", i18n::t("dc.cmd.update")),
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

/// A single-threaded tokio runtime, for spots that only ever run one task at
/// a time (the bot's own event loop, or a single outgoing notification) and
/// don't need `tokio::runtime::Runtime::new()`'s default multi-threaded
/// runtime - which, with the `rt-multi-thread` feature enabled and no
/// worker-thread cap, spawns one OS thread per CPU core (16 on a typical
/// modern machine) every time it's created. `notify_admin`/`signal_shutdown`
/// each created one of those from scratch just to send a single HTTP
/// request; `start_bot_thread` creates one per bot-thread generation, which
/// now also happens on every Fast-User-Switching session hand-off (see
/// session.rs) instead of just once at startup.
fn light_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()
}

/// Settings one bot generation runs with.
struct Settings {
    token: String,
    channel_id: u64,
    admin_user_id: Option<u64>,
}

/// Read the bot settings for a new generation, if the bot is enabled and
/// fully configured.
fn load_settings() -> Option<Settings> {
    let config = database::get_discord_config();

    if !config.enabled {
        eprintln!("[Discord] Bot is disabled in settings");
        return None;
    }

    let Some(token) = config.bot_token else {
        eprintln!("[Discord] Bot enabled but no token configured");
        return None;
    };
    if token.is_empty() {
        eprintln!("[Discord] Bot token is empty");
        return None;
    }

    let Some(channel_id) = config.channel_id else {
        eprintln!("[Discord] Bot enabled but no channel configured");
        return None;
    };

    Some(Settings { token, channel_id, admin_user_id: config.admin_user_id })
}

/// Start the Discord bot in a background thread. A no-op if a bot thread is
/// already running/starting - safe to call repeatedly (e.g. from the
/// per-second active-session check in session.rs). If the previous bot is
/// still shutting down, the start is queued and that bot's thread starts a
/// fresh one as soon as it has finished (see `BotLifecycle`).
pub fn start_bot_thread() {
    match LIFECYCLE.request_start() {
        StartAction::Spawn => {}
        StartAction::AlreadyRunning => return,
        StartAction::Queued => {
            eprintln!("[Discord] Previous bot still stopping - will restart once it has");
            return;
        }
    }

    let Some(settings) = load_settings() else {
        LIFECYCLE.cancel_start();
        return;
    };

    // Wait for the previous generation's thread to fully exit before
    // spawning a new one - see BOT_THREAD. `Spawn` above already means no
    // other caller can be in this function concurrently and the previous
    // thread has finished its last generation, so this join is short.
    if let Some(handle) = BOT_THREAD.lock().unwrap().take() {
        let _ = handle.join();
    }

    let handle = std::thread::spawn(move || {
        let mut settings = settings;
        loop {
            // Unlike a one-shot startup failure, this runs on every bot-thread
            // generation (each Fast-User-Switching hand-off spawns a fresh one -
            // see session.rs) - panicking here would be the exact mistake
            // add_tray_icon used to make, just with a resource-exhaustion
            // trigger instead of a shell-not-ready one. BOT_HTTP and the
            // lifecycle still need resetting on failure so a bad runtime
            // creation can't wedge them forever and block every later restart.
            match light_runtime() {
                Ok(rt) => rt.block_on(run_bot(settings)),
                Err(e) => eprintln!("[Discord] Failed to create tokio runtime: {e}"),
            }
            *BOT_HTTP.lock().unwrap() = None;

            // A start that arrived while this generation was stopping runs
            // now, on this same thread, so it can't overlap the old one.
            settings = loop {
                if !LIFECYCLE.finish_generation() {
                    return;
                }
                if let Some(settings) = load_settings() {
                    break settings;
                }
            };
        }
    });
    *BOT_THREAD.lock().unwrap() = Some(handle);
}

/// Stop the Discord bot thread, if one is running. A no-op otherwise - safe
/// to call repeatedly. Unlike `signal_shutdown`, this doesn't send a
/// notification: it's used for routine active-session handoffs (see
/// session.rs), not an actual app shutdown.
pub fn stop_bot_thread() {
    LIFECYCLE.request_stop();
}

/// Proactively push a message to the configured channel, outside of any
/// incoming command (e.g. a "requesting more time" notification triggered
/// from the blocking overlay). No-op if the bot isn't connected/configured.
pub fn notify_admin(text: &str) {
    let http = BOT_HTTP.lock().unwrap().clone();
    let channel_id = *NOTIFY_CHANNEL_ID.lock().unwrap();
    if let (Some(http), Some(channel_id)) = (http, channel_id) {
        let text = text.to_string();
        std::thread::spawn(move || {
            let rt = light_runtime().ok();
            if let Some(rt) = rt {
                rt.block_on(async {
                    let _ = ChannelId::new(channel_id).say(&http, text).await;
                });
            }
        });
    }
}

/// Whether `notify_admin` would actually send anything right now - i.e. this
/// session's bot client is up with a channel configured.
pub fn can_notify() -> bool {
    BOT_HTTP.lock().is_ok_and(|h| h.is_some()) && NOTIFY_CHANNEL_ID.lock().is_ok_and(|c| c.is_some())
}

/// Signal the bot to shut down gracefully
pub fn signal_shutdown() {
    LIFECYCLE.request_shutdown();

    let http = BOT_HTTP.lock().unwrap().clone();
    let channel_id = *NOTIFY_CHANNEL_ID.lock().unwrap();
    if let (Some(http), Some(channel_id)) = (http, channel_id) {
        std::thread::spawn(move || {
            let rt = light_runtime().ok();
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

/// Main bot loop. Runs until this generation is told to stop, or Discord
/// refuses the configuration outright (bad token, Message Content intent
/// not enabled) - neither of which can change without an app restart.
///
/// Network trouble needs no retry loop here, unlike telegram.rs: serenity
/// keeps re-queueing the gateway connection every few seconds until it
/// succeeds, and reconnects by itself after a drop. Building the client
/// makes no network calls that can fail it either (`get_gateway` falls back
/// to the default URL). What it does need is a stop that works while still
/// offline: `shutdown_all` returns early without ending `start()` when no
/// shard has connected yet, which used to leave this thread - and so the
/// bot's running state - stuck until the app exited.
async fn run_bot(settings: Settings) {
    let Settings { token, channel_id, admin_user_id } = settings;
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
    // Always overwrite, in case it changed in Settings since the last time a
    // bot generation started - see NOTIFY_CHANNEL_ID.
    *NOTIFY_CHANNEL_ID.lock().unwrap() = Some(channel_id);

    let shard_manager = client.shard_manager.clone();
    let stopped = tokio::select! {
        res = client.start() => {
            if let Err(e) = res {
                eprintln!("[Discord] Client error: {e}");
            }
            false
        }
        () = LIFECYCLE.wait_for_shutdown() => true,
    };
    if stopped {
        // Close any connected shard cleanly. Whatever is left (a connection
        // attempt still waiting for the network) is cancelled when this
        // generation's runtime is dropped.
        shard_manager.shutdown_all().await;
    }
}
