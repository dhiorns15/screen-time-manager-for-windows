//! Telegram bot module for Screen Time Manager
//! Provides remote monitoring and control via Telegram commands

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::dispatching::DefaultKey;
use teloxide::error_handlers::LoggingErrorHandler;
use teloxide::types::{Me, ParseMode, UserId};
use teloxide::update_listeners::Polling;
use teloxide::RequestError;

use crate::database;
use crate::i18n;
use crate::remote_commands::{
    cmd_clearlockout, cmd_extend, cmd_free, cmd_getpin, cmd_history, cmd_lock, cmd_msg, cmd_pause,
    cmd_reduce, cmd_reset, cmd_resume, cmd_rotatingpin, cmd_setlimit, cmd_setmessage, cmd_setpin,
    cmd_status, cmd_time, cmd_unlock, cmd_update, cmd_weekly, dispatch_minutes, free_grant_detail,
    measure_server_clock,
    minutes_arg, reply_names_account, retry_delay, split_command, split_message, stale_age,
    stale_note, with_account, BotLifecycle, ChatMarkup, RateLimit, StartAction, WeeklyMarkup,
    STALE_NOTE_INTERVAL, TELEGRAM_MAX_MESSAGE, TELEGRAM_TIME_URL,
};
use crate::time_request;

/// At most one "ignored, too old" reply a minute - see `stale_note`.
static STALE_NOTES: RateLimit = RateLimit::new(STALE_NOTE_INTERVAL);

/// Whether a bot thread is running, whether it's been told to stop, and
/// whether a start is queued behind a stop that's still in progress - see
/// `BotLifecycle`. Makes start_bot_thread/stop_bot_thread cheap no-ops once
/// already in the desired state.
static LIFECYCLE: BotLifecycle = BotLifecycle::new();

/// Bot instance for sending notifications. A Mutex rather than a OnceLock
/// because the bot can be stopped and restarted within the same process -
/// e.g. when this session loses/regains console ownership under Fast User
/// Switching (see session.rs) - and OnceLock can only ever be set once.
static BOT_INSTANCE: Mutex<Option<Bot>> = Mutex::new(None);

/// Handle for the most recently spawned bot thread, if any. `start_bot_thread`
/// joins this before spawning a new one (see there for why) - the shutdown
/// flag is a single flag shared across every generation of the bot thread,
/// so without this join, a new generation could reset it to `false` while the
/// *previous* generation's shutdown-watcher task is still mid-teardown,
/// leaving two overlapping bot instances alive at once.
static BOT_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// Admin chat ID for notifications. A Mutex rather than a OnceLock for the
/// same reason as BOT_INSTANCE above - it also needs to track the current
/// config, not just the first-ever value, since the admin could change the
/// configured chat ID in Settings while a bot generation from before that
/// change is still using the old one, or across a session-handoff restart.
static ADMIN_CHAT_ID: Mutex<Option<i64>> = Mutex::new(None);

/// Whether the current bot generation has reached Telegram (its `get_me`
/// succeeded) - so `can_notify` doesn't report a bot that is still trying to
/// connect, e.g. offline at startup, whose messages would just be lost.
static CONNECTED: AtomicBool = AtomicBool::new(false);

/// A bot command. Parsed by `parse_command` rather than teloxide's
/// `BotCommands` derive, which matches command names case-sensitively and
/// parses a number from the untrimmed rest of the message - so `/Extend 30`,
/// `/extend  30` or `/extend` alone were all answered "Unknown command".
/// Arguments are kept as typed (trimmed) and checked by the command itself,
/// the same as on Discord.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Command {
    Start,
    Status,
    Time,
    Extend(String),
    Reduce(String),
    Pause,
    Resume,
    History,
    Weekly,
    Msg(String),
    /// `/lock`, or its alias `/stop`.
    Lock,
    Unlock,
    /// `/free <minutes>`, `/free off` or `/free`.
    Free(String),
    Reset,
    Setmessage(String),
    Setpin(String),
    Setlimit(String),
    Rotatingpin(String),
    Getpin,
    Clearlockout,
    Update,
    E30,
    E60,
    E120,
    Chatid,
    Help,
}

/// What a message's text is, as far as commands go.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ParsedText {
    /// Doesn't start with "/": shown on screen (see `build_dispatcher`).
    NotACommand,
    /// "/cmd@other_bot" in a group: for another bot, so not answered.
    ForAnotherBot,
    /// A "/" command this bot doesn't know.
    Unknown,
    /// The command, and the name it was sent as (lowercase - "stop" for
    /// `/stop`, which is `Command::Lock`).
    Command(Command, String),
}

/// Parse a message's text as a command for the bot `bot_username` (without
/// the "@"; `None` if unknown, and then only commands without "@bot" are
/// taken). Command names and the "@bot" part are matched without regard to
/// case; arguments are everything after the first whitespace, trimmed.
fn parse_command(text: &str, bot_username: Option<&str>) -> ParsedText {
    let Some(rest) = text.strip_prefix('/') else {
        return ParsedText::NotACommand;
    };
    let (head, args) = split_command(rest);
    let (name, addressee) = match head.split_once('@') {
        Some((name, bot)) => (name, Some(bot)),
        None => (head, None),
    };
    if let Some(addressee) = addressee {
        if !bot_username.is_some_and(|me| me.eq_ignore_ascii_case(addressee)) {
            return ParsedText::ForAnotherBot;
        }
    }
    let name = name.to_ascii_lowercase();
    let args = args.to_string();
    let command = match name.as_str() {
        "start" => Command::Start,
        "status" => Command::Status,
        "time" => Command::Time,
        "extend" => Command::Extend(args),
        "reduce" => Command::Reduce(args),
        "pause" => Command::Pause,
        "resume" => Command::Resume,
        "history" => Command::History,
        "weekly" => Command::Weekly,
        "msg" => Command::Msg(args),
        "lock" | "stop" => Command::Lock,
        "unlock" => Command::Unlock,
        "free" => Command::Free(args),
        "reset" => Command::Reset,
        "setmessage" => Command::Setmessage(args),
        "setpin" => Command::Setpin(args),
        "setlimit" => Command::Setlimit(args),
        "rotatingpin" => Command::Rotatingpin(args),
        "getpin" => Command::Getpin,
        "clearlockout" => Command::Clearlockout,
        "update" => Command::Update,
        "e30" => Command::E30,
        "e60" => Command::E60,
        "e120" => Command::E120,
        "chatid" => Command::Chatid,
        "help" => Command::Help,
        _ => return ParsedText::Unknown,
    };
    ParsedText::Command(command, name)
}

/// `/help`'s reply, in the app's language.
fn help_text() -> String {
    let commands = [
        ("/start", i18n::t("tg.cmd.start")),
        ("/status", i18n::t("tg.cmd.status")),
        ("/time", i18n::t("tg.cmd.time")),
        ("/extend <mins>", i18n::t("tg.cmd.extend")),
        ("/reduce <mins>", i18n::t("tg.cmd.reduce")),
        ("/pause", i18n::t("tg.cmd.pause")),
        ("/resume", i18n::t("tg.cmd.resume")),
        ("/history", i18n::t("tg.cmd.history")),
        ("/weekly", i18n::t("tg.cmd.weekly")),
        ("/msg <text>", i18n::t("tg.cmd.msg")),
        ("/lock", i18n::t("tg.cmd.lock")),
        ("/stop", i18n::t("tg.cmd.stop")),
        ("/unlock", i18n::t("tg.cmd.unlock")),
        ("/free <mins|off>", i18n::t("tg.cmd.free")),
        ("/reset", i18n::t("tg.cmd.reset")),
        ("/setmessage <text>", i18n::t("tg.cmd.setmessage")),
        ("/setpin <pin>", i18n::t("tg.cmd.setpin")),
        ("/setlimit <mins>", i18n::t("tg.cmd.setlimit")),
        ("/rotatingpin <on|off>", i18n::t("tg.cmd.rotatingpin")),
        ("/getpin", i18n::t("tg.cmd.getpin")),
        ("/clearlockout", i18n::t("tg.cmd.clearlockout")),
        ("/update", i18n::t("tg.cmd.update")),
        ("/e30", i18n::t("tg.cmd.e30")),
        ("/e60", i18n::t("tg.cmd.e60")),
        ("/e120", i18n::t("tg.cmd.e120")),
        ("/chatid", i18n::t("tg.cmd.chatid")),
        ("/help", i18n::t("tg.cmd.help")),
    ];

    let mut response = format!("{}\n", i18n::t("tg.help.header"));
    for (cmd, desc) in commands {
        response.push_str(&format!("{cmd} - {desc}\n"));
    }
    response.push('\n');
    response.push_str(i18n::t("tg.help.plain_text"));
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

/// Read the bot settings for a new generation: the token, if the bot is
/// enabled and has one. Also records the admin chat ID for notifications -
/// always overwritten, in case it changed in Settings since the last time a
/// bot generation started.
fn load_settings() -> Option<(String, Option<i64>)> {
    let config = database::get_telegram_config();

    if !config.enabled {
        eprintln!("[Telegram] Bot is disabled in settings");
        return None;
    }

    let Some(token) = config.bot_token else {
        eprintln!("[Telegram] Bot enabled but no token configured");
        return None;
    };

    if token.is_empty() {
        eprintln!("[Telegram] Bot token is empty");
        return None;
    }

    *ADMIN_CHAT_ID.lock().unwrap() = config.admin_chat_id;
    Some((token, config.admin_chat_id))
}

/// Start the Telegram bot in a background thread. A no-op if a bot thread is
/// already running/starting - safe to call repeatedly (e.g. from the
/// per-second active-session check in session.rs). If the previous bot is
/// still shutting down, the start is queued and that bot's thread starts a
/// fresh one as soon as it has finished (see `BotLifecycle`).
pub fn start_bot_thread() {
    match LIFECYCLE.request_start() {
        StartAction::Spawn => {}
        StartAction::AlreadyRunning => return,
        StartAction::Queued => {
            eprintln!("[Telegram] Previous bot still stopping - will restart once it has");
            return;
        }
    }

    let Some((token, admin_chat_id)) = load_settings() else {
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
        let (mut token, mut admin_chat_id) = (token, admin_chat_id);
        loop {
            // Unlike a one-shot startup failure, this runs on every bot-thread
            // generation (each Fast-User-Switching hand-off spawns a fresh one -
            // see session.rs) - panicking here would be the exact mistake
            // add_tray_icon used to make, just with a resource-exhaustion
            // trigger instead of a shell-not-ready one. BOT_INSTANCE and the
            // lifecycle still need resetting on failure so a bad runtime
            // creation can't wedge them forever and block every later restart.
            match light_runtime() {
                Ok(rt) => rt.block_on(run_bot(token, admin_chat_id)),
                Err(e) => eprintln!("[Telegram] Failed to create tokio runtime: {e}"),
            }
            CONNECTED.store(false, Ordering::SeqCst);
            *BOT_INSTANCE.lock().unwrap() = None;

            // A start that arrived while this generation was stopping runs
            // now, on this same thread, so it can't overlap the old one.
            (token, admin_chat_id) = loop {
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

/// Stop the Telegram bot thread, if one is running. A no-op otherwise - safe
/// to call repeatedly. Unlike `signal_shutdown`, this doesn't send a
/// notification: it's used for routine active-session handoffs (see
/// session.rs), not an actual app shutdown.
pub fn stop_bot_thread() {
    LIFECYCLE.request_stop();
}

/// Proactively push a message to the configured admin chat, outside of any
/// incoming command (e.g. a "requesting more time" notification triggered
/// from the blocking overlay). No-op if the bot isn't connected/configured.
pub fn notify_admin(text: &str) {
    let bot = BOT_INSTANCE.lock().unwrap().clone();
    let chat_id = *ADMIN_CHAT_ID.lock().unwrap();
    if let (Some(bot), Some(chat_id)) = (bot, chat_id) {
        let text = text.to_string();
        std::thread::spawn(move || {
            let rt = light_runtime().ok();
            if let Some(rt) = rt {
                rt.block_on(async {
                    let _ = bot.send_message(ChatId(chat_id), text).await;
                });
            }
        });
    }
}

/// Whether `notify_admin` would actually reach anyone right now - i.e. this
/// session's bot is running, has connected to Telegram, and has an admin
/// chat configured. (update.rs waits for this before claiming a one-off
/// announcement, so it isn't spent on a bot that's still offline.)
pub fn can_notify() -> bool {
    CONNECTED.load(Ordering::SeqCst)
        && BOT_INSTANCE.lock().is_ok_and(|b| b.is_some())
        && ADMIN_CHAT_ID.lock().is_ok_and(|c| c.is_some())
}

/// Signal the bot to shut down gracefully
pub fn signal_shutdown() {
    LIFECYCLE.request_shutdown();

    // Send shutdown notification if possible
    let bot = BOT_INSTANCE.lock().unwrap().clone();
    let chat_id = *ADMIN_CHAT_ID.lock().unwrap();
    if let (Some(bot), Some(chat_id)) = (bot, chat_id) {
        std::thread::spawn(move || {
            let rt = light_runtime().ok();
            if let Some(rt) = rt {
                rt.block_on(async {
                    let _ = bot.send_message(ChatId(chat_id), i18n::t("tg.notify.shutdown")).await;
                });
            }
        });
        // Give a moment for the message to send
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Main bot loop. Runs until this generation is told to stop. Nothing in
/// here may panic on a network or token problem: the release build aborts
/// on panic, so it would take the whole app (and enforcement) down with it.
async fn run_bot(token: String, admin_chat_id: Option<i64>) {
    let mut failures = 0u32;

    // `Bot::new` would panic if the HTTP client couldn't be built; build the
    // identical client ourselves so a failure is just another retry.
    let bot = loop {
        match teloxide::net::default_reqwest_settings().build() {
            Ok(client) => break Bot::with_client(token, client),
            Err(e) => {
                failures += 1;
                eprintln!("[Telegram] Couldn't create HTTP client: {e}");
                if !LIFECYCLE.sleep_unless_shutdown(retry_delay(failures)).await {
                    return;
                }
            }
        }
    };

    // Store bot instance for notifications
    *BOT_INSTANCE.lock().unwrap() = Some(bot.clone());

    let mut dispatcher = build_dispatcher(bot.clone(), admin_chat_id);
    let shutdown_token = dispatcher.shutdown_token();
    let mut announced = false;

    // Connect, and reconnect after failures until told to stop. Once
    // connected, the polling listener rides out later network drops itself
    // (with its own backoff), so this loop only repeats when connecting
    // fails: no network at startup, Telegram unreachable, or a bad token.
    while !LIFECYCLE.shutdown_requested() {
        let outcome = {
            let attempt =
                connect_and_dispatch(&bot, &mut dispatcher, admin_chat_id, &mut announced, &mut failures);
            tokio::pin!(attempt);
            let outcome = tokio::select! {
                res = &mut attempt => Some(res),
                () = LIFECYCLE.wait_for_shutdown() => None,
            };
            if outcome.is_none() {
                // Told to stop. If the dispatcher is running, stop it
                // gracefully: polling then confirms the last update it
                // handed out, so Telegram doesn't deliver it again next
                // time, and in-flight commands finish. If it isn't running
                // yet (still connecting), there's nothing to confirm, and
                // dropping the attempt cancels the pending request.
                if shutdown_token.shutdown().is_ok() {
                    let _ = attempt.await;
                }
            }
            outcome
        };

        match outcome {
            None => break,
            Some(Ok(())) if LIFECYCLE.shutdown_requested() => break,
            Some(Ok(())) => {
                failures += 1;
                eprintln!("[Telegram] Update listener stopped unexpectedly - reconnecting");
            }
            Some(Err(e)) => {
                failures += 1;
                eprintln!("[Telegram] Couldn't connect (attempt {failures}): {e}");
            }
        }
        if !LIFECYCLE.sleep_unless_shutdown(retry_delay(failures)).await {
            break;
        }
    }
}

/// One connection attempt: check the token works, announce, and run the
/// dispatcher until it's shut down. Errors only for a failed connection
/// (`try_dispatch_with_listener` rather than `dispatch`, which panics if
/// its initial `get_me` call fails).
async fn connect_and_dispatch(
    bot: &Bot,
    dispatcher: &mut Dispatcher<Bot, RequestError, DefaultKey>,
    admin_chat_id: Option<i64>,
    announced: &mut bool,
    failures: &mut u32,
) -> Result<(), RequestError> {
    bot.get_me().await?;
    *failures = 0;
    CONNECTED.store(true, Ordering::SeqCst);

    // Same listener `dispatch()` would build (deleting any webhook first, or
    // getUpdates below would be refused).
    let listener = Polling::builder(bot.clone())
        .timeout(Duration::from_secs(10))
        .delete_webhook()
        .await
        .build();

    // Drop whatever queued up while no bot was connected (the PC was off,
    // asleep, offline, or another account had the console): those commands
    // were meant for then, not for whoever happens to be logged in now.
    // Done here, before the "started" notice below, rather than with the
    // listener's `drop_pending_updates`, which only does it on its first
    // poll - after the notice, so a command sent in reply to the notice was
    // discarded along with the backlog.
    drop_backlog(bot).await?;

    // Message ages are judged by Telegram's clock, not the PC's alone.
    measure_server_clock(TELEGRAM_TIME_URL).await;

    // Send startup notification, once per bot generation and only once
    // actually connected. A generation starts whenever the bot (re)starts,
    // including a session-handoff restart under Fast User Switching (see
    // session.rs) - not just a genuine app startup - so it doubles as an
    // "active user changed" notice: whichever account's session currently
    // owns the console is the one whose bot is live, and this says who that is.
    if !*announced {
        *announced = true;
        if let Some(chat_id) = admin_chat_id {
            let text = format!(
                "{}\n👤 {}: {}",
                i18n::t("tg.notify.started"),
                i18n::t("tg.status.user"),
                crate::remote_commands::current_windows_username(),
            );
            let _ = bot.send_message(ChatId(chat_id), text).await;
        }
    }

    dispatcher
        .try_dispatch_with_listener(
            listener,
            LoggingErrorHandler::with_custom_text("[Telegram] Update listener error"),
        )
        .await
}

/// Make Telegram forget every update waiting for this bot. Offset -1 returns
/// just the newest one and forgets the rest; asking again from the one after
/// it confirms that one too. Anything arriving in between is kept (it's
/// unconfirmed, so the listener's first poll gets it).
async fn drop_backlog(bot: &Bot) -> Result<(), RequestError> {
    let newest = bot.get_updates().offset(-1).limit(1).timeout(0).await?;
    if let Some(update) = newest.last() {
        bot.get_updates().offset(update.id.as_offset()).limit(1).timeout(0).await?;
    }
    Ok(())
}

/// Wrapper type for the dispatcher's dependency map: how old (seconds) a
/// message that is too old to act on is.
#[derive(Clone, Copy, Debug)]
struct StaleAge(i64);

/// The command a message starts with ("/Start@my_bot extra" -> "start"),
/// lowercased.
fn command_name(text: &str) -> Option<String> {
    let command = text.split_whitespace().next()?.strip_prefix('/')?;
    Some(command.split('@').next().unwrap_or("").to_ascii_lowercase())
}

/// Whether `text` is one of the setup commands answered for anyone, which
/// are never ignored as too old: they change nothing, and are exactly what
/// someone tries when the bot seems not to respond.
fn is_setup_command(text: &str) -> bool {
    command_name(text).is_some_and(|name| name == "start" || name == "chatid")
}

/// Send a plain-text reply, split into several messages if it's too long
/// for one.
async fn send_plain(bot: &Bot, chat_id: ChatId, text: &str) -> ResponseResult<()> {
    for part in split_message(text, TELEGRAM_MAX_MESSAGE, ChatMarkup::Plain) {
        bot.send_message(chat_id, part).await?;
    }
    Ok(())
}

fn build_dispatcher(bot: Bot, admin_chat_id: Option<i64>) -> Dispatcher<Bot, RequestError, DefaultKey> {
    // Anything that queued up while the connection was down mid-session
    // (e.g. the PC slept) arrives in a burst on reconnect; dropping
    // pending updates at connect doesn't cover that, so old messages are
    // also filtered here, before any other handler sees them. The admin
    // chat gets a short note (at most one a minute) instead of silence.
    let stale_handler = Update::filter_message()
        .filter_map_async(|msg: Message| async move {
            if msg.text().is_some_and(is_setup_command) {
                return None;
            }
            stale_age(msg.date.timestamp(), TELEGRAM_TIME_URL).await.map(StaleAge)
        })
        .endpoint(move |bot: Bot, msg: Message, age: StaleAge| async move {
            eprintln!(
                "[Telegram] Ignoring message {} from {} - sent {}s ago",
                msg.id, msg.date, age.0
            );
            if Some(msg.chat.id.0) != admin_chat_id {
                return Ok(());
            }
            let is_setpin = msg.text().and_then(command_name).is_some_and(|name| name == "setpin");
            if is_setpin && !msg.chat.is_private() {
                // Not run, but its passcode is still readable in the group
                // (see `setpin_and_delete`).
                let _ = bot.delete_message(msg.chat.id, msg.id).await;
            }
            if STALE_NOTES.allow() {
                bot.send_message(msg.chat.id, stale_note(age.0)).await?;
            }
            Ok(())
        });

    // Command handler: every text starting with "/" (`Me` is this bot's own
    // account, which the dispatcher fetches when it starts).
    let command_handler = Update::filter_message()
        .filter_map(|msg: Message, me: Me| match parse_command(msg.text()?, me.user.username.as_deref()) {
            ParsedText::NotACommand => None,
            parsed => Some(parsed),
        })
        .endpoint(move |bot: Bot, msg: Message, parsed: ParsedText| async move {
            match parsed {
                ParsedText::Command(cmd, name) => handle_command(bot, msg, cmd, &name, admin_chat_id).await,
                ParsedText::Unknown => {
                    bot.send_message(msg.chat.id, i18n::t("tg.error.unknown_cmd")).await?;
                    Ok(())
                }
                ParsedText::ForAnotherBot | ParsedText::NotACommand => Ok(()),
            }
        });

    // Fallback handler: show plain text as on-screen message (authorized users only)
    let fallback_handler = Update::filter_message()
        .endpoint(move |bot: Bot, msg: Message| async move {
            if let Some(text) = msg.text() {
                if !text.is_empty() {
                    // Check authorization (silently ignored otherwise)
                    if authorize(&bot, &msg, admin_chat_id).await == AuthDecision::Allow {
                        unsafe {
                            crate::overlay::show_overlay(text, 10);
                        }
                        send_plain(
                            &bot,
                            msg.chat.id,
                            &format!("📢 {}: \"{}\"", i18n::t("tg.msg.shown"), text),
                        ).await?;
                    }
                }
            }
            Ok(())
        });

    // Combine handlers - stale messages dropped first, then commands, then fallback
    let handler = dptree::entry()
        .branch(stale_handler)
        .branch(command_handler)
        .branch(fallback_handler);

    // Create dispatcher with default error handler that logs errors
    Dispatcher::builder(bot, handler)
        .default_handler(|upd| async move {
            eprintln!("[Telegram] Unhandled update: {:?}", upd);
        })
        .error_handler(LoggingErrorHandler::with_custom_text("[Telegram] Error in handler"))
        .build()
}

/// Outcome of the authorisation check for a command or on-screen message.
#[derive(Debug, PartialEq, Eq)]
enum AuthDecision {
    Allow,
    Deny,
    /// No admin chat configured at all.
    NoAdmin,
    /// From a member of the configured group: allowed only if Telegram says
    /// that user is the group's owner or an administrator.
    CheckGroupAdmin(u64),
}

/// Who may control the bot. The rule:
///
/// - The message must come from the configured admin chat.
/// - If that chat is a private chat with the bot (the normal setup), that's
///   enough: the only person in it is the one who set it up.
/// - If it's a group, the sender must also be one of the group's owner or
///   administrators - so a child added to a family group can't send
///   commands just by being a member. A message an administrator posts
///   anonymously "as the group" (sender chat = the group itself; only
///   administrators can do that) counts; one posted on behalf of any other
///   chat or channel doesn't.
fn auth_decision(
    admin_chat_id: Option<i64>,
    chat_id: i64,
    chat_is_private: bool,
    sender_user_id: Option<u64>,
    sender_chat_id: Option<i64>,
) -> AuthDecision {
    let Some(admin_chat_id) = admin_chat_id else {
        return AuthDecision::NoAdmin;
    };
    if chat_id != admin_chat_id {
        return AuthDecision::Deny;
    }
    if chat_is_private {
        return AuthDecision::Allow;
    }
    match (sender_chat_id, sender_user_id) {
        (Some(sender_chat), _) if sender_chat == chat_id => AuthDecision::Allow,
        (Some(_), _) | (None, None) => AuthDecision::Deny,
        (None, Some(user_id)) => AuthDecision::CheckGroupAdmin(user_id),
    }
}

/// `auth_decision`, with the group-administrator lookup done (fails closed:
/// if Telegram can't be asked, the command is refused). Never returns
/// `CheckGroupAdmin`.
async fn authorize(bot: &Bot, msg: &Message, admin_chat_id: Option<i64>) -> AuthDecision {
    let decision = auth_decision(
        admin_chat_id,
        msg.chat.id.0,
        msg.chat.is_private(),
        msg.from.as_ref().map(|user| user.id.0),
        msg.sender_chat.as_ref().map(|chat| chat.id.0),
    );
    let AuthDecision::CheckGroupAdmin(user_id) = decision else {
        return decision;
    };
    match bot.get_chat_member(msg.chat.id, UserId(user_id)).await {
        Ok(member) if member.kind.is_privileged() => AuthDecision::Allow,
        Ok(_) => AuthDecision::Deny,
        Err(e) => {
            // The sender isn't named: who it was is no use here, and ids stay out of the log.
            eprintln!("[Telegram] Couldn't check whether the sender is a group admin: {e}");
            AuthDecision::Deny
        }
    }
}

/// Handle incoming commands
async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    name: &str,
    admin_chat_id: Option<i64>,
) -> ResponseResult<()> {
    let sender_id = msg.chat.id.0;

    // For /start and /chatid commands, always respond (helps with setup)
    match &cmd {
        Command::Start => {
            let welcome = format!(
                "{}\n\n{} {}\n\n{}",
                i18n::t("tg.start.welcome"),
                i18n::t("tg.chatid.your_id"),
                sender_id,
                i18n::t("tg.start.help_hint"),
            );
            bot.send_message(msg.chat.id, welcome).await?;
            return Ok(());
        }
        Command::Chatid => {
            bot.send_message(msg.chat.id, format!("{} {}", i18n::t("tg.chatid.your_id"), sender_id)).await?;
            return Ok(());
        }
        _ => {}
    }

    // Authorization check for all other commands - see `auth_decision`
    match authorize(&bot, &msg, admin_chat_id).await {
        AuthDecision::Allow => {}
        AuthDecision::NoAdmin => {
            // No admin configured - reject all commands except /start and /chatid
            bot.send_message(msg.chat.id, i18n::t("tg.error.no_admin")).await?;
            return Ok(());
        }
        AuthDecision::Deny | AuthDecision::CheckGroupAdmin(_) => {
            bot.send_message(msg.chat.id, i18n::t("tg.error.unauthorized")).await?;
            return Ok(());
        }
    }

    // Commands that dismiss/grant time on the lock screen resolve any
    // pending "request more time" from the blocking overlay - figure out
    // beforehand (while `cmd` is still borrowable) whether this one qualifies.
    let grant_detail: Option<String> = match &cmd {
        Command::Extend(args) => minutes_arg(args).filter(|m| *m > 0 && *m <= 120).map(|m| format!("+{m} min")),
        Command::E30 => Some("+30 min".to_string()),
        Command::E60 => Some("+60 min".to_string()),
        Command::E120 => Some("+120 min".to_string()),
        Command::Reset => Some("reset to daily limit".to_string()),
        Command::Unlock if crate::blocking::is_blocking_overlay_visible()
            && (crate::blocking::get_remaining_seconds() > 0 || crate::free_time::is_active()) => {
            Some("unlocked".to_string())
        }
        Command::Free(args) => free_grant_detail(args),
        _ => None,
    };

    // The weekly table is sent as an HTML <pre> block so it actually lines
    // up as a table client-side (cmd_weekly escapes everything around it) -
    // every other command's response is sent as plain text, so this needs
    // to be known before `cmd` is consumed by the match below.
    let is_weekly = matches!(&cmd, Command::Weekly);

    let response = match cmd {
        Command::Start => unreachable!(), // Handled above
        Command::Status => cmd_status(),
        Command::Time => cmd_time(),
        Command::Extend(args) => dispatch_minutes(&args, "/extend", cmd_extend),
        Command::Reduce(args) => dispatch_minutes(&args, "/reduce", cmd_reduce),
        Command::Pause => cmd_pause(),
        Command::Resume => cmd_resume(),
        Command::History => cmd_history(),
        Command::Weekly => cmd_weekly(WeeklyMarkup::TelegramHtml),
        Command::Msg(text) => cmd_msg(&text),
        Command::Lock => cmd_lock(),
        Command::Unlock => cmd_unlock(),
        Command::Free(args) => cmd_free(&args, "/free"),
        Command::Reset => cmd_reset(),
        Command::Setmessage(text) => cmd_setmessage(&text),
        Command::Setpin(pin) => setpin_and_delete(&bot, &msg, &pin).await,
        Command::Setlimit(args) => cmd_setlimit(&args),
        Command::Rotatingpin(args) => cmd_rotatingpin(&args),
        Command::Getpin => getpin_privately(&bot, &msg).await,
        Command::Clearlockout => cmd_clearlockout(),
        // Talks to GitHub (and schtasks) - off this single-threaded
        // runtime, so the bot keeps answering meanwhile.
        Command::Update => tokio::task::spawn_blocking(cmd_update)
            .await
            .unwrap_or_else(|e| format!("❌ {e}")),
        Command::E30 => cmd_extend(30),
        Command::E60 => cmd_extend(60),
        Command::E120 => cmd_extend(120),
        Command::Chatid => unreachable!(), // Handled above
        Command::Help => help_text(),
    };
    let response = if reply_names_account(name) { with_account(response) } else { response };

    if let Some(detail) = grant_detail {
        time_request::resolve_if_pending("Telegram", &detail);
    }

    // Long replies (`/weekly` with many accounts) go out as several messages.
    if is_weekly {
        for part in split_message(&response, TELEGRAM_MAX_MESSAGE, ChatMarkup::TelegramHtml) {
            bot.send_message(msg.chat.id, part).parse_mode(ParseMode::Html).await?;
        }
    } else {
        send_plain(&bot, msg.chat.id, &response).await?;
    }
    Ok(())
}

/// Where `/getpin` may put today's code.
#[derive(Debug, PartialEq, Eq)]
enum PinDestination {
    /// The private chat it was asked in - only the admin reads it.
    ThisChat,
    /// A group: privately to the user who asked, never into the group.
    PrivateChatWith(u64),
    /// A group, asked by an administrator posting anonymously as the group
    /// (or by a bot): there's nobody to message privately.
    Nobody,
}

fn pin_destination(chat_is_private: bool, sender_user: Option<(u64, bool)>, sent_as_chat: bool) -> PinDestination {
    if chat_is_private {
        return PinDestination::ThisChat;
    }
    match sender_user {
        Some((user_id, false)) if !sent_as_chat => PinDestination::PrivateChatWith(user_id),
        _ => PinDestination::Nobody,
    }
}

/// `/getpin`: in the usual private chat, the code is the reply. In a group
/// (which the child may be in too) it is never posted: it goes to the admin
/// who asked, in their private chat with the bot - which only works once
/// they have started one, since Telegram doesn't let bots message people
/// first - and the group only gets a short note. If the rotating PIN is off,
/// the reply has no code in it and is posted as before.
async fn getpin_privately(bot: &Bot, msg: &Message) -> String {
    if !database::is_rotating_pin_enabled() {
        return cmd_getpin();
    }
    let destination = pin_destination(
        msg.chat.is_private(),
        msg.from.as_ref().map(|user| (user.id.0, user.is_bot)),
        msg.sender_chat.is_some(),
    );
    match destination {
        PinDestination::ThisChat => cmd_getpin(),
        PinDestination::Nobody => format!("❌ {}", i18n::t("tg.getpin.anonymous")),
        PinDestination::PrivateChatWith(user_id) => {
            match bot.send_message(UserId(user_id), cmd_getpin()).await {
                Ok(_) => format!("📬 {}", i18n::t("tg.getpin.sent_private")),
                Err(e) => {
                    eprintln!("[Telegram] Couldn't send the PIN privately: {e}");
                    format!("❌ {}", i18n::t("tg.getpin.private_failed"))
                }
            }
        }
    }
}

/// `/setpin`: in a group, the new passcode is in the admin's own message,
/// which every member can read, so delete it first. That needs the bot to be
/// a group administrator allowed to delete messages - if it isn't, say so,
/// so the admin can delete it by hand. (In the private chat only the admin
/// sees it, so it's left alone there.)
async fn setpin_and_delete(bot: &Bot, msg: &Message, args: &str) -> String {
    if msg.chat.is_private() {
        return cmd_setpin(args);
    }
    let deleted = match bot.delete_message(msg.chat.id, msg.id).await {
        Ok(_) => true,
        Err(e) => {
            eprintln!("[Telegram] Couldn't delete the /setpin message: {e}");
            false
        }
    };
    let mut response = cmd_setpin(args);
    if !deleted {
        response.push_str("\n⚠️ ");
        response.push_str(i18n::t("tg.setpin.not_deleted"));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: i64 = 111_222_333;
    const GROUP: i64 = -1_001_234_567_890;

    #[test]
    fn private_chat_with_the_bot_is_the_admin() {
        assert_eq!(auth_decision(Some(PARENT), PARENT, true, Some(PARENT as u64), None), AuthDecision::Allow);
    }

    #[test]
    fn other_chats_are_refused() {
        assert_eq!(auth_decision(Some(PARENT), 999, true, Some(999), None), AuthDecision::Deny);
        assert_eq!(auth_decision(Some(PARENT), GROUP, false, Some(PARENT as u64), None), AuthDecision::Deny);
    }

    #[test]
    fn nothing_configured() {
        assert_eq!(auth_decision(None, PARENT, true, Some(PARENT as u64), None), AuthDecision::NoAdmin);
    }

    #[test]
    fn group_members_need_admin_status() {
        assert_eq!(
            auth_decision(Some(GROUP), GROUP, false, Some(42), None),
            AuthDecision::CheckGroupAdmin(42)
        );
    }

    #[test]
    fn group_senders_without_a_user_or_as_another_chat() {
        // An administrator posting anonymously as the group itself.
        assert_eq!(
            auth_decision(Some(GROUP), GROUP, false, Some(1_087_968_824), Some(GROUP)),
            AuthDecision::Allow
        );
        // Posting on behalf of some other channel.
        assert_eq!(
            auth_decision(Some(GROUP), GROUP, false, Some(136_817_688), Some(-100_777)),
            AuthDecision::Deny
        );
        assert_eq!(auth_decision(Some(GROUP), GROUP, false, None, None), AuthDecision::Deny);
    }

    #[test]
    fn pin_is_never_posted_in_a_group() {
        assert_eq!(pin_destination(true, Some((PARENT as u64, false)), false), PinDestination::ThisChat);
        assert_eq!(
            pin_destination(false, Some((PARENT as u64, false)), false),
            PinDestination::PrivateChatWith(PARENT as u64)
        );
        // Anonymous administrator: `from` is Telegram's GroupAnonymousBot,
        // `sender_chat` is the group.
        assert_eq!(pin_destination(false, Some((1_087_968_824, true)), true), PinDestination::Nobody);
        assert_eq!(pin_destination(false, Some((42, false)), true), PinDestination::Nobody);
        assert_eq!(pin_destination(false, None, false), PinDestination::Nobody);
    }

    const ME: Option<&str> = Some("family_screen_bot");

    fn command(text: &str) -> Option<(Command, String)> {
        match parse_command(text, ME) {
            ParsedText::Command(cmd, name) => Some((cmd, name)),
            _ => None,
        }
    }

    #[test]
    fn commands_are_parsed_forgivingly() {
        let extend = |args: &str| Some((Command::Extend(args.to_string()), "extend".to_string()));
        assert_eq!(command("/extend 30"), extend("30"));
        assert_eq!(command("/Extend 30"), extend("30"));
        assert_eq!(command("/EXTEND 30"), extend("30"));
        assert_eq!(command("/extend  30"), extend("30"));
        assert_eq!(command("/extend 30 "), extend("30"));
        assert_eq!(command("/extend\n30"), extend("30"));
        // Not a number: the command itself asks for one (see dispatch_minutes).
        assert_eq!(command("/extend 30min"), extend("30min"));
        assert_eq!(command("/extend"), extend(""));
        assert_eq!(minutes_arg("30min"), None);
        assert_eq!(minutes_arg(""), None);

        assert_eq!(command("/Status"), Some((Command::Status, "status".to_string())));
        assert_eq!(command("/stop"), Some((Command::Lock, "stop".to_string())));
        assert_eq!(command("/E30"), Some((Command::E30, "e30".to_string())));
        assert_eq!(
            command("/setlimit  Saturday 180"),
            Some((Command::Setlimit("Saturday 180".to_string()), "setlimit".to_string()))
        );
        assert_eq!(
            command("/msg Dinner is ready!"),
            Some((Command::Msg("Dinner is ready!".to_string()), "msg".to_string()))
        );
        assert_eq!(command("/msg"), Some((Command::Msg(String::new()), "msg".to_string())));
        assert_eq!(command("/Free 90"), Some((Command::Free("90".to_string()), "free".to_string())));
        assert_eq!(command("/free off"), Some((Command::Free("off".to_string()), "free".to_string())));
        assert_eq!(command("/free"), Some((Command::Free(String::new()), "free".to_string())));
    }

    #[test]
    fn commands_addressed_to_a_bot() {
        assert_eq!(command("/status@family_screen_bot"), Some((Command::Status, "status".to_string())));
        assert_eq!(command("/Status@Family_Screen_Bot"), Some((Command::Status, "status".to_string())));
        assert_eq!(
            command("/extend@family_screen_bot 30"),
            Some((Command::Extend("30".to_string()), "extend".to_string()))
        );
        assert_eq!(parse_command("/status@some_other_bot", ME), ParsedText::ForAnotherBot);
        assert_eq!(parse_command("/frobnicate@some_other_bot", ME), ParsedText::ForAnotherBot);
        assert_eq!(parse_command("/status@", ME), ParsedText::ForAnotherBot);
        // Own name unknown: only commands without "@bot" are taken.
        assert_eq!(parse_command("/status@family_screen_bot", None), ParsedText::ForAnotherBot);
        assert_eq!(parse_command("/status", None), ParsedText::Command(Command::Status, "status".to_string()));
    }

    #[test]
    fn other_texts() {
        assert_eq!(parse_command("/frobnicate", ME), ParsedText::Unknown);
        assert_eq!(parse_command("/", ME), ParsedText::Unknown);
        assert_eq!(parse_command("/statusx", ME), ParsedText::Unknown);
        assert_eq!(parse_command("Dinner is ready", ME), ParsedText::NotACommand);
        assert_eq!(parse_command("please /extend 30", ME), ParsedText::NotACommand);
        assert_eq!(parse_command("", ME), ParsedText::NotACommand);
    }

    #[test]
    fn every_command_is_parsed_and_in_the_help() {
        let help = help_text();
        for name in [
            "start", "status", "time", "extend", "reduce", "pause", "resume", "history", "weekly", "msg", "lock",
            "stop", "unlock", "free", "reset", "setmessage", "setpin", "setlimit", "rotatingpin", "getpin",
            "clearlockout", "update", "e30", "e60", "e120", "chatid", "help",
        ] {
            assert!(command(&format!("/{name}")).is_some(), "/{name} isn't parsed");
            assert!(
                help.lines().any(|l| l.starts_with(&format!("/{name} "))),
                "/{name} missing from:\n{help}"
            );
        }
    }

    #[test]
    fn setup_commands_are_recognised() {
        assert!(is_setup_command("/start"));
        assert!(is_setup_command("/chatid"));
        assert!(is_setup_command("/ChatID@my_family_bot"));
        assert!(is_setup_command("/start extra words"));
        assert!(!is_setup_command("/status"));
        assert!(!is_setup_command("/startx"));
        assert!(!is_setup_command("start"));
        assert!(!is_setup_command(""));
        assert!(!is_setup_command("please /start"));
    }
}

