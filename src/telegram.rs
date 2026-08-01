//! Telegram bot module for Screen Time Manager
//! Provides remote monitoring and control via Telegram commands

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use teloxide::prelude::*;
use teloxide::error_handlers::LoggingErrorHandler;
use teloxide::types::ParseMode;
use teloxide::utils::command::BotCommands;

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

/// Bot instance for sending notifications. A Mutex rather than a OnceLock
/// because the bot can be stopped and restarted within the same process -
/// e.g. when this session loses/regains console ownership under Fast User
/// Switching (see session.rs) - and OnceLock can only ever be set once.
static BOT_INSTANCE: Mutex<Option<Bot>> = Mutex::new(None);

/// Handle for the most recently spawned bot thread, if any. `start_bot_thread`
/// joins this before spawning a new one (see there for why) - BOT_SHUTDOWN is
/// a single flag shared across every generation of the bot thread, so without
/// this join, a new generation could reset it to `false` while the *previous*
/// generation's shutdown-watcher task is still mid-teardown, leaving two
/// overlapping bot instances alive at once.
static BOT_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// Admin chat ID for notifications. A Mutex rather than a OnceLock for the
/// same reason as BOT_INSTANCE above - it also needs to track the current
/// config, not just the first-ever value, since the admin could change the
/// configured chat ID in Settings while a bot generation from before that
/// change is still using the old one, or across a session-handoff restart.
static ADMIN_CHAT_ID: Mutex<Option<i64>> = Mutex::new(None);

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "Screen Time Manager commands:")]
enum Command {
    #[command(description = "Start the bot")]
    Start,
    #[command(description = "Show remaining time and status")]
    Status,
    #[command(description = "Quick time check")]
    Time,
    #[command(description = "Extend time by minutes (e.g., /extend 30)")]
    Extend(i32),
    #[command(description = "Reduce time by minutes (e.g., /reduce 30)")]
    Reduce(i32),
    #[command(description = "Pause the timer")]
    Pause,
    #[command(description = "Resume the timer")]
    Resume,
    #[command(description = "Show today's pause activity")]
    History,
    #[command(description = "Show a 2-week usage table")]
    Weekly,
    #[command(description = "Show a message on screen (e.g., /msg Do your homework!)")]
    Msg(String),
    #[command(description = "Lock the screen")]
    Lock,
    #[command(description = "Lock the screen (alias)")]
    Stop,
    #[command(description = "Unlock without changing remaining time (only if time is left)")]
    Unlock,
    #[command(description = "Reset timer to daily limit")]
    Reset,
    #[command(description = "Change the blocking screen message (e.g., /setmessage Time for a break!)")]
    Setmessage(String),
    #[command(description = "Change the passcode (e.g., /setpin 1234)")]
    Setpin(String),
    #[command(description = "Set daily limit in minutes (e.g., /setlimit 90 or /setlimit saturday 180)")]
    Setlimit(String),
    #[command(description = "Toggle the rotating daily PIN on/off (e.g., /rotatingpin on)")]
    Rotatingpin(String),
    #[command(description = "Get today's rotating PIN (if enabled)")]
    Getpin,
    #[command(description = "Extend by 30 minutes")]
    E30,
    #[command(description = "Extend by 60 minutes")]
    E60,
    #[command(description = "Extend by 120 minutes")]
    E120,
    #[command(description = "Get your chat ID for setup")]
    Chatid,
    #[command(description = "Show this help message")]
    Help,
}

/// Start the Telegram bot in a background thread. A no-op if a bot thread is
/// already running/starting - safe to call repeatedly (e.g. from the
/// per-second active-session check in session.rs).
pub fn start_bot_thread() {
    if BOT_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }

    let config = database::get_telegram_config();

    if !config.enabled {
        eprintln!("[Telegram] Bot is disabled in settings");
        BOT_RUNNING.store(false, Ordering::SeqCst);
        return;
    }

    let Some(token) = config.bot_token else {
        eprintln!("[Telegram] Bot enabled but no token configured");
        BOT_RUNNING.store(false, Ordering::SeqCst);
        return;
    };

    if token.is_empty() {
        eprintln!("[Telegram] Bot token is empty");
        BOT_RUNNING.store(false, Ordering::SeqCst);
        return;
    }

    let admin_chat_id = config.admin_chat_id;

    // Store admin chat ID for notifications - always overwrite, in case it
    // changed in Settings since the last time a bot generation started.
    *ADMIN_CHAT_ID.lock().unwrap() = admin_chat_id;

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
        // Unlike a one-shot startup failure, this runs on every bot-thread
        // generation (each Fast-User-Switching hand-off spawns a fresh one -
        // see session.rs) - panicking here would be the exact mistake
        // add_tray_icon used to make, just with a resource-exhaustion
        // trigger instead of a shell-not-ready one. BOT_INSTANCE/BOT_RUNNING
        // still need resetting on failure so a bad runtime creation can't
        // wedge them true/Some forever and block every later restart.
        match tokio::runtime::Runtime::new() {
            Ok(rt) => rt.block_on(async {
                run_bot(token, admin_chat_id).await;
            }),
            Err(e) => eprintln!("[Telegram] Failed to create tokio runtime: {e}"),
        }
        *BOT_INSTANCE.lock().unwrap() = None;
        BOT_RUNNING.store(false, Ordering::SeqCst);
    });
    *BOT_THREAD.lock().unwrap() = Some(handle);
}

/// Stop the Telegram bot thread, if one is running. A no-op otherwise - safe
/// to call repeatedly. Unlike `signal_shutdown`, this doesn't send a
/// notification: it's used for routine active-session handoffs (see
/// session.rs), not an actual app shutdown.
pub fn stop_bot_thread() {
    if BOT_RUNNING.load(Ordering::SeqCst) {
        BOT_SHUTDOWN.store(true, Ordering::SeqCst);
    }
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
            let rt = tokio::runtime::Runtime::new().ok();
            if let Some(rt) = rt {
                rt.block_on(async {
                    let _ = bot.send_message(ChatId(chat_id), text).await;
                });
            }
        });
    }
}

/// Signal the bot to shut down gracefully
pub fn signal_shutdown() {
    BOT_SHUTDOWN.store(true, Ordering::SeqCst);

    // Send shutdown notification if possible
    let bot = BOT_INSTANCE.lock().unwrap().clone();
    let chat_id = *ADMIN_CHAT_ID.lock().unwrap();
    if let (Some(bot), Some(chat_id)) = (bot, chat_id) {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().ok();
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

/// Main bot loop
async fn run_bot(token: String, admin_chat_id: Option<i64>) {
    let bot = Bot::new(&token);

    // Store bot instance for notifications
    *BOT_INSTANCE.lock().unwrap() = Some(bot.clone());

    // Send startup notification. This fires whenever the bot (re)connects,
    // including a session-handoff reconnect under Fast User Switching (see
    // session.rs) - not just a genuine app startup - so it doubles as an
    // "active user changed" notice: whichever account's session currently
    // owns the console is the one whose bot is live, and this says who that is.
    if let Some(chat_id) = admin_chat_id {
        let text = format!(
            "{}\n👤 {}: {}",
            i18n::t("tg.notify.started"),
            i18n::t("tg.status.user"),
            crate::remote_commands::current_windows_username(),
        );
        let _ = bot.send_message(ChatId(chat_id), text).await;
    }

    // Command handler
    let command_handler = Update::filter_message()
        .filter_command::<Command>()
        .endpoint(move |bot: Bot, msg: Message, cmd: Command| {
            handle_command(bot, msg, cmd, admin_chat_id)
        });

    // Fallback handler: show plain text as on-screen message (authorized users only)
    let fallback_handler = Update::filter_message()
        .endpoint(move |bot: Bot, msg: Message| async move {
            if let Some(text) = msg.text() {
                if text.starts_with('/') {
                    bot.send_message(
                        msg.chat.id,
                        i18n::t("tg.error.unknown_cmd")
                    ).await?;
                } else if !text.is_empty() {
                    // Check authorization
                    let authorized = admin_chat_id
                        .map(|id| msg.chat.id.0 == id)
                        .unwrap_or(false);
                    if authorized {
                        unsafe {
                            crate::overlay::show_overlay(text, 10);
                        }
                        bot.send_message(
                            msg.chat.id,
                            format!("📢 {}: \"{}\"", i18n::t("tg.msg.shown"), text)
                        ).await?;
                    }
                }
            }
            Ok(())
        });

    // Combine handlers - commands first, then fallback
    let handler = dptree::entry()
        .branch(command_handler)
        .branch(fallback_handler);

    // Create dispatcher with default error handler that logs errors
    let mut dispatcher = Dispatcher::builder(bot, handler)
        .default_handler(|upd| async move {
            eprintln!("[Telegram] Unhandled update: {:?}", upd);
        })
        .error_handler(LoggingErrorHandler::with_custom_text("[Telegram] Error in handler"))
        .build();

    // Get shutdown token for graceful shutdown
    let shutdown_token = dispatcher.shutdown_token();

    // Spawn a task to monitor shutdown signal
    tokio::spawn(async move {
        while !BOT_SHUTDOWN.load(Ordering::SeqCst) {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        shutdown_token.shutdown().ok();
    });

    // Run dispatcher
    dispatcher.dispatch().await;
}

/// Handle incoming commands
async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    admin_chat_id: Option<i64>,
) -> ResponseResult<()> {
    let sender_id = msg.chat.id.0;

    // For /start and /chatid commands, always respond (helps with setup)
    match &cmd {
        Command::Start => {
            let welcome = format!(
                "Welcome to Screen Time Manager Bot!\n\n\
                 Your chat ID is: {}\n\n\
                 Use /help to see available commands.",
                sender_id
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

    // Authorization check for all other commands
    if let Some(admin_id) = admin_chat_id {
        if sender_id != admin_id {
            bot.send_message(msg.chat.id, i18n::t("tg.error.unauthorized")).await?;
            return Ok(());
        }
    } else {
        // No admin configured - reject all commands except /start and /chatid
        bot.send_message(msg.chat.id, i18n::t("tg.error.no_admin")).await?;
        return Ok(());
    }

    // Commands that dismiss/grant time on the lock screen resolve any
    // pending "request more time" from the blocking overlay - figure out
    // beforehand (while `cmd` is still borrowable) whether this one qualifies.
    let grant_detail: Option<String> = match &cmd {
        Command::Extend(mins) if *mins > 0 && *mins <= 120 => Some(format!("+{mins} min")),
        Command::E30 => Some("+30 min".to_string()),
        Command::E60 => Some("+60 min".to_string()),
        Command::E120 => Some("+120 min".to_string()),
        Command::Reset => Some("reset to daily limit".to_string()),
        Command::Unlock if crate::blocking::is_blocking_overlay_visible()
            && crate::blocking::get_remaining_seconds() > 0 => Some("unlocked".to_string()),
        _ => None,
    };

    // The weekly table is sent as a MarkdownV2 code block so it actually
    // lines up as a table client-side - every other command's response is
    // sent as plain text, so this needs to be known before `cmd` is consumed
    // by the match below.
    let is_weekly = matches!(&cmd, Command::Weekly);

    let response = match cmd {
        Command::Start => unreachable!(), // Handled above
        Command::Status => cmd_status(),
        Command::Time => cmd_time(),
        Command::Extend(mins) => cmd_extend(mins),
        Command::Reduce(mins) => cmd_reduce(mins),
        Command::Pause => cmd_pause(),
        Command::Resume => cmd_resume(),
        Command::History => cmd_history(),
        Command::Weekly => cmd_weekly(),
        Command::Msg(text) => cmd_msg(&text),
        Command::Lock => cmd_lock(),
        Command::Stop => cmd_lock(),
        Command::Unlock => cmd_unlock(),
        Command::Reset => cmd_reset(),
        Command::Setmessage(text) => cmd_setmessage(&text),
        Command::Setpin(pin) => cmd_setpin(&pin),
        Command::Setlimit(args) => cmd_setlimit(&args),
        Command::Rotatingpin(args) => cmd_rotatingpin(&args),
        Command::Getpin => cmd_getpin(),
        Command::E30 => cmd_extend(30),
        Command::E60 => cmd_extend(60),
        Command::E120 => cmd_extend(120),
        Command::Chatid => unreachable!(), // Handled above
        Command::Help => Command::descriptions().to_string(),
    };

    if let Some(detail) = grant_detail {
        time_request::resolve_if_pending("Telegram", &detail);
    }

    if is_weekly {
        bot.send_message(msg.chat.id, response).parse_mode(ParseMode::MarkdownV2).await?;
    } else {
        bot.send_message(msg.chat.id, response).await?;
    }
    Ok(())
}

