//! Telegram bot module for Screen Time Manager
//! Provides remote monitoring and control via Telegram commands

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use teloxide::prelude::*;
use teloxide::error_handlers::LoggingErrorHandler;
use teloxide::utils::command::BotCommands;

use crate::database;
use crate::i18n;
use crate::remote_commands::{
    cmd_extend, cmd_history, cmd_lock, cmd_msg, cmd_pause, cmd_reduce, cmd_reset, cmd_resume,
    cmd_status, cmd_time,
};

/// Shutdown signal for graceful termination
pub static BOT_SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Bot instance for sending notifications
static BOT_INSTANCE: OnceLock<Bot> = OnceLock::new();

/// Admin chat ID for notifications
static ADMIN_CHAT_ID: OnceLock<i64> = OnceLock::new();

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
    #[command(description = "Show a message on screen (e.g., /msg Do your homework!)")]
    Msg(String),
    #[command(description = "Lock the screen")]
    Lock,
    #[command(description = "Lock the screen (alias)")]
    Stop,
    #[command(description = "Reset timer to daily limit")]
    Reset,
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

/// Start the Telegram bot in a background thread
pub fn start_bot_thread() {
    let config = database::get_telegram_config();

    if !config.enabled {
        eprintln!("[Telegram] Bot is disabled in settings");
        return;
    }

    let Some(token) = config.bot_token else {
        eprintln!("[Telegram] Bot enabled but no token configured");
        return;
    };

    if token.is_empty() {
        eprintln!("[Telegram] Bot token is empty");
        return;
    }

    let admin_chat_id = config.admin_chat_id;

    // Store admin chat ID for notifications
    if let Some(id) = admin_chat_id {
        let _ = ADMIN_CHAT_ID.set(id);
    }

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        rt.block_on(async {
            run_bot(token, admin_chat_id).await;
        });
    });
}

/// Signal the bot to shut down gracefully
pub fn signal_shutdown() {
    BOT_SHUTDOWN.store(true, Ordering::SeqCst);

    // Send shutdown notification if possible
    if let (Some(bot), Some(&chat_id)) = (BOT_INSTANCE.get(), ADMIN_CHAT_ID.get()) {
        let bot = bot.clone();
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
    let _ = BOT_INSTANCE.set(bot.clone());

    // Send startup notification
    if let Some(chat_id) = admin_chat_id {
        let _ = bot.send_message(ChatId(chat_id), i18n::t("tg.notify.started")).await;
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

    let response = match cmd {
        Command::Start => unreachable!(), // Handled above
        Command::Status => cmd_status(),
        Command::Time => cmd_time(),
        Command::Extend(mins) => cmd_extend(mins),
        Command::Reduce(mins) => cmd_reduce(mins),
        Command::Pause => cmd_pause(),
        Command::Resume => cmd_resume(),
        Command::History => cmd_history(),
        Command::Msg(text) => cmd_msg(&text),
        Command::Lock => cmd_lock(),
        Command::Stop => cmd_lock(),
        Command::Reset => cmd_reset(),
        Command::E30 => cmd_extend(30),
        Command::E60 => cmd_extend(60),
        Command::E120 => cmd_extend(120),
        Command::Chatid => unreachable!(), // Handled above
        Command::Help => Command::descriptions().to_string(),
    };

    bot.send_message(msg.chat.id, response).await?;
    Ok(())
}

