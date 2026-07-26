//! Internationalization (i18n) module for Screen Time Manager
//! Supports English (default) and German

use crate::database;
use windows::core::PCWSTR;

/// Convert a translated string to a Windows wide string (null-terminated Vec<u16>)
/// The returned Vec must be kept alive while the PCWSTR is in use
pub fn wide(key: &str) -> Vec<u16> {
    t(key).encode_utf16().chain(std::iter::once(0)).collect()
}

/// Convert a raw string to a Windows wide string
#[allow(dead_code)]
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Get PCWSTR from a wide string Vec (helper for cleaner code)
#[allow(dead_code)]
pub fn pcwstr(wide: &[u16]) -> PCWSTR {
    PCWSTR(wide.as_ptr())
}

/// Supported languages
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Language {
    English,
    German,
}

impl Language {
    /// Get the language code (for storage)
    pub fn code(&self) -> &'static str {
        match self {
            Language::English => "en",
            Language::German => "de",
        }
    }

    /// Create Language from code string
    pub fn from_code(code: &str) -> Self {
        match code {
            "de" => Language::German,
            _ => Language::English,
        }
    }

    /// Get the display name in the native language
    pub fn name(&self) -> &'static str {
        match self {
            Language::English => "English",
            Language::German => "Deutsch",
        }
    }

    /// Get all supported languages
    pub fn all() -> &'static [Language] {
        &[Language::English, Language::German]
    }
}

/// Get the current language from settings
pub fn current() -> Language {
    Language::from_code(&database::get_setting("language").unwrap_or_default())
}

/// Set the current language
pub fn set_language(lang: Language) {
    database::set_setting("language", lang.code());
}

/// Main translation function - returns static string for the given key
pub fn t(key: &str) -> &'static str {
    match current() {
        Language::English => en(key),
        Language::German => de(key),
    }
}

/// Get weekday name by index (0 = Monday, 6 = Sunday)
pub fn weekday(index: usize) -> &'static str {
    const EN_DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    const DE_DAYS: [&str; 7] = ["Montag", "Dienstag", "Mittwoch", "Donnerstag", "Freitag", "Samstag", "Sonntag"];

    let days = match current() {
        Language::English => &EN_DAYS,
        Language::German => &DE_DAYS,
    };
    days.get(index).unwrap_or(&"Unknown")
}

/// Get short weekday name by index (0 = Monday, 6 = Sunday)
#[allow(dead_code)]
pub fn weekday_short(index: usize) -> &'static str {
    const EN_DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const DE_DAYS: [&str; 7] = ["Mo", "Di", "Mi", "Do", "Fr", "Sa", "So"];

    let days = match current() {
        Language::English => &EN_DAYS,
        Language::German => &DE_DAYS,
    };
    days.get(index).unwrap_or(&"?")
}

// ============================================================================
// English strings
// ============================================================================

fn en(key: &str) -> &'static str {
    match key {
        // ----- Window Titles -----
        "window.settings" => "Screen Time Settings",
        "window.passcode" => "Enter Passcode",
        "window.stats" => "Today's Stats",
        "window.blocking" => "Screen Time - Time's Up!",
        "window.about" => "About",

        // ----- Settings Dialog - Section Titles -----
        "settings.daily_limits" => "Daily Time Limits (minutes)",
        "settings.warning1" => "First Warning",
        "settings.warning2" => "Second Warning",
        "settings.blocking_message" => "Blocking Screen Message",
        "settings.passcode" => "Change Passcode (leave blank to keep)",
        "settings.telegram" => "Telegram Bot",
        "settings.discord" => "Discord Bot",
        "settings.lock_screen" => "Lock Screen",
        "settings.idle" => "Idle Detection",
        "settings.language" => "Language",

        // ----- Settings Dialog - Labels -----
        "settings.minutes_before" => "Minutes before:",
        "settings.message" => "Message:",
        "settings.current" => "Current:",
        "settings.new" => "New:",
        "settings.confirm" => "Confirm:",
        "settings.enable_rotating_pin" => "Enable rotating daily PIN (via Telegram/Discord, works alongside your passcode)",
        "settings.enable_telegram" => "Enable Telegram Bot",
        "settings.bot_token" => "Bot Token:",
        "settings.chat_id" => "Chat ID:",
        "settings.setup_wizard" => "Setup Wizard...",
        "settings.enable_discord" => "Enable Discord Bot",
        "settings.channel_id" => "Channel ID:",
        "settings.discord_user_id" => "Your User ID:",
        "settings.shutdown_timeout" => "Shutdown timeout:",
        "settings.auto_pause_idle" => "Auto-pause when idle",
        "settings.idle_timeout" => "Idle timeout (min):",

        // ----- Settings Dialog - Buttons -----
        "button.save" => "Save",
        "button.cancel" => "Cancel",
        "button.ok" => "OK",
        "button.close" => "Close",
        "button.reset_timer" => "Reset Timer",

        // ----- Settings Dialog - Messages -----
        "settings.error.current_incorrect" => "Current passcode is incorrect!",
        "settings.error.passcode_length" => "New passcode must be exactly 4 digits!",
        "settings.error.passcode_mismatch" => "New passcode and confirmation do not match!",
        "settings.success.saved" => "Settings saved successfully!",
        "settings.error" => "Error",
        "settings.success" => "Settings",

        // ----- Passcode Dialog -----
        "passcode.subtitle" => "Enter 4-digit code to continue",
        "passcode.incorrect" => "Incorrect passcode",

        // ----- Stats Dialog -----
        "stats.title" => "Today's Statistics",
        "stats.day" => "Day:",
        "stats.daily_limit" => "Daily Limit:",
        "stats.time_used" => "Time Used:",
        "stats.time_remaining" => "Time Remaining:",
        "stats.pause_mode" => "Pause Mode",
        "stats.pause_used" => "Pause Used:",
        "stats.pause_remaining" => "Pause Remaining:",
        "stats.pauses_today" => "Pauses Today:",
        "stats.log" => "Log:",
        "stats.pause_disabled" => "Pause feature is disabled",
        "stats.timer_reset" => "Timer has been reset to the daily limit.",
        "stats.timer_reset_title" => "Timer Reset",

        // ----- Tray Menu -----
        "tray.tooltip" => "Screen Time Manager",
        "tray.stats" => "Today's Stats...",
        "tray.settings" => "Settings...",
        "tray.extend_15" => "Extend +15 min",
        "tray.extend_45" => "Extend +45 min",
        "tray.resume" => "Resume Timer",
        "tray.pause_idle" => "Pause (Idle paused)",
        "tray.pause_disabled" => "Pause (Disabled)",
        "tray.pause_budget_used" => "Pause (Budget used)",
        "tray.pause_time_low" => "Pause (Time too low)",
        "tray.idle_paused" => "Idle: Paused",
        "tray.show_warning" => "Show Warning (5s)",
        "tray.show_blocking" => "Show Blocking Overlay",
        "tray.about" => "About",
        "tray.quit" => "Quit",

        // ----- Blocking Screen -----
        "blocking.times_up" => "Time's Up!",
        "blocking.limit_reached" => "Screen time limit reached",
        "blocking.extend_label" => "Extend time (requires passcode):",
        "blocking.passcode_label" => "Enter passcode to unlock:",
        "blocking.incorrect" => "Incorrect passcode!",
        "blocking.shutdown_in" => "Shutdown in:",
        "blocking.shutdown_now" => "SHUTDOWN IN:",
        "blocking.time_exceeded" => "Time limit exceeded",
        "blocking.extend_15" => "+15 min",
        "blocking.extend_30" => "+30 min",
        "blocking.extend_60" => "+60 min",
        "blocking.unlock" => "Unlock",
        "blocking.shutdown" => "Shut Down",
        "blocking.confirm_shutdown" => "Are you sure you want to shut down the computer?",
        "blocking.confirm_title" => "Confirm Shutdown",
        "blocking.screen_locked" => "Screen Locked",
        "blocking.request_note_label" => "Reason (optional):",
        "blocking.request_time_button" => "📨 Request More Time",
        "blocking.request_sent" => "✅ Sent! Wait",
        "blocking.request_unavailable" => "Enable Telegram or Discord in Settings to request more time here",

        // ----- About Dialog -----
        "about.text" => "Screen Time Manager v1.0.40\n\nA parental control application for managing screen time.\n\n(c) Simon Pamies",

        // ----- Pause Reasons -----
        "pause.disabled" => "Pause feature is disabled",
        "pause.budget_exhausted" => "Daily pause budget exhausted",
        "pause.cooldown" => "Cooldown active",
        "pause.min_active" => "Need more active time",
        "pause.time_too_low" => "Time is too low to pause",

        // ----- Telegram Bot - Command Descriptions -----
        "tg.cmd.start" => "Start the bot",
        "tg.cmd.status" => "Show remaining time and status",
        "tg.cmd.time" => "Quick time check",
        "tg.cmd.extend" => "Extend time by minutes (e.g., /extend 30)",
        "tg.cmd.reduce" => "Reduce time by minutes (e.g., /reduce 30)",
        "tg.cmd.pause" => "Pause the timer",
        "tg.cmd.resume" => "Resume the timer",
        "tg.cmd.history" => "Show today's pause activity",
        "tg.cmd.msg" => "Show a message on screen (e.g., /msg Do your homework!)",
        "tg.cmd.lock" => "Lock the screen",
        "tg.cmd.stop" => "Lock the screen (alias)",
        "tg.cmd.reset" => "Reset timer to daily limit",
        "tg.cmd.e30" => "Extend by 30 minutes",
        "tg.cmd.e60" => "Extend by 60 minutes",
        "tg.cmd.e120" => "Extend by 120 minutes",
        "tg.cmd.chatid" => "Get your chat ID for setup",
        "tg.cmd.help" => "Show this help message",

        // ----- Telegram Bot - Responses -----
        "tg.status.header" => "Screen Time Status",
        "tg.status.user" => "User",
        "tg.status.remaining" => "Remaining:",
        "tg.status.paused" => "Paused:",
        "tg.status.pause_budget" => "Pause budget:",
        "tg.status.yes" => "Yes",
        "tg.status.no" => "No",
        "tg.status.idle" => "Yes (idle)",

        "tg.extend.specify_positive" => "Please specify a positive number of minutes",
        "tg.extend.max_120" => "Maximum extension is 120 minutes",
        "tg.extend.success" => "Extended by",
        "tg.extend.success_suffix" => "",

        "tg.reduce.specify_positive" => "Please specify a positive number of minutes",
        "tg.reduce.max_120" => "Maximum reduction is 120 minutes",
        "tg.reduce.not_enough" => "Cannot reduce - not enough time remaining",
        "tg.reduce.success" => "Reduced by",
        "tg.reduce.success_suffix" => "",

        "tg.pause.already_paused" => "Timer is already paused. Use /resume to continue.",
        "tg.pause.idle_paused" => "Timer is already paused (idle). It will resume automatically when input is detected.",
        "tg.pause.success" => "Timer paused",
        "tg.pause.failed" => "Timer was not paused (unexpected state)",
        "tg.pause.cannot" => "Cannot pause:",

        "tg.resume.idle_auto" => "Timer is idle-paused. It will resume automatically when input is detected.",
        "tg.resume.not_paused" => "Timer is not paused",
        "tg.resume.success" => "Timer resumed",
        "tg.resume.failed" => "Timer is still paused (unexpected state)",
        "tg.resume.cannot" => "Cannot resume:",

        "tg.history.header" => "Today's Activity",
        "tg.history.uptime" => "Uptime:",
        "tg.history.pause_used" => "Pause used:",
        "tg.history.no_events" => "No pause events today",

        "tg.weekly.header" => "2-Week Usage",

        "tg.msg.provide" => "Please provide a message, e.g. /msg Do your homework!",
        "tg.msg.shown" => "Message shown:",

        "tg.setmessage.usage" => "Please provide a message, e.g. /setmessage Time to take a break!",
        "tg.setmessage.success" => "Blocking screen message updated",

        "tg.setpin.invalid" => "Please provide exactly 4 digits, e.g. /setpin 1234",
        "tg.setpin.success" => "Passcode updated",

        "tg.rotatingpin.usage" => "Please specify on or off, e.g. /rotatingpin on",
        "tg.rotatingpin.enabled" => "🔁 Rotating daily PIN enabled. It works alongside your passcode (which still always works too) everywhere a code is needed. Use /getpin to check today's code.",
        "tg.rotatingpin.disabled" => "🔁 Rotating daily PIN disabled. Only your passcode works now.",

        "tg.getpin.not_enabled" => "The rotating PIN isn't enabled. Turn it on with /rotatingpin on",
        "tg.getpin.header" => "Today's code:",
        "tg.getpin.note" => "Works anywhere your passcode does. Changes tomorrow.",

        "tg.setlimit.usage" => "Please specify minutes, e.g. /setlimit 90 or /setlimit saturday 180",
        "tg.setlimit.invalid_day" => "Unrecognized day - use monday, tuesday, wednesday, thursday, friday, saturday or sunday",
        "tg.setlimit.invalid_minutes" => "Please specify a whole number of minutes",
        "tg.setlimit.max_1440" => "Maximum daily limit is 1440 minutes (24 hours)",
        "tg.setlimit.success" => "Daily limit for",
        "tg.setlimit.to" => "set to",

        "tg.reset.success" => "Timer reset to daily limit",
        "tg.reset.remaining" => "Remaining:",

        "tg.lock.success" => "Screen locked",

        "tg.unlock.not_locked" => "The screen isn't locked right now.",
        "tg.unlock.no_time" => "No time left - use /extend or /reset instead.",
        "tg.unlock.success" => "Screen unlocked",

        "tg.error.unknown_cmd" => "Unknown command. Use /help to see available commands.",
        "tg.error.unauthorized" => "Unauthorized. This bot is configured for a specific user.",
        "tg.error.no_admin" => "No admin configured. Please set your chat ID in settings.",
        "tg.chatid.your_id" => "Your chat ID is:",

        "tg.notify.started" => "Screen Time Manager started",
        "tg.notify.shutdown" => "Screen Time Manager is shutting down",

        // ----- Discord Bot - Command Descriptions -----
        "dc.help.header" => "Screen Time Manager Discord Bot commands:",
        "dc.cmd.status" => "Show remaining time and status",
        "dc.cmd.time" => "Quick time check",
        "dc.cmd.extend" => "Extend time by minutes (e.g., !extend 30)",
        "dc.cmd.reduce" => "Reduce time by minutes (e.g., !reduce 30)",
        "dc.cmd.pause" => "Pause the timer",
        "dc.cmd.resume" => "Resume the timer",
        "dc.cmd.history" => "Show today's pause activity",
        "dc.cmd.weekly" => "Show a 2-week usage table",
        "dc.cmd.msg" => "Show a message on screen (e.g., !msg Do your homework!)",
        "dc.cmd.lock" => "Lock the screen",
        "dc.cmd.unlock" => "Unlock without changing remaining time (only if time is left)",
        "dc.cmd.reset" => "Reset timer to daily limit",
        "dc.cmd.setmessage" => "Change the blocking screen message (e.g., !setmessage Time for a break!)",
        "dc.cmd.setpin" => "Change the passcode (e.g., !setpin 1234)",
        "dc.cmd.setlimit" => "Set daily limit in minutes (e.g., !setlimit 90 or !setlimit saturday 180)",
        "dc.cmd.rotatingpin" => "Toggle the rotating daily PIN on/off (e.g., !rotatingpin on)",
        "dc.cmd.getpin" => "Get today's rotating PIN (if enabled)",
        "dc.cmd.e30" => "Extend by 30 minutes",
        "dc.cmd.e60" => "Extend by 60 minutes",
        "dc.cmd.e120" => "Extend by 120 minutes",
        "dc.cmd.whoami" => "Get your Discord user ID for setup",
        "dc.cmd.help" => "Show this help message",

        "dc.error.unknown_cmd" => "Unknown command. Use !help to see available commands.",
        "dc.error.no_admin" => "No admin configured. Please set your Discord user ID in settings.",
        "dc.error.specify_number" => "Please specify a number of minutes, e.g. !extend 30",
        "dc.whoami.your_id" => "Your Discord user ID is:",

        // ----- Time Request Notifications (blocking overlay -> bots) -----
        "request.notify.header" => "is requesting more time!",
        "request.notify.reply_tg" => "Reply /extend 30 to grant, or /unlock if there's time left.",
        "request.notify.reply_dc" => "Reply !extend 30 to grant, or !unlock if there's time left.",
        "request.notify.resolved" => "Time request already granted:",

        // ----- Passcode-based Extend Notifications -----
        "passcode_extend.notify.header" => "used the passcode to add",
        "passcode_extend.notify.minutes" => "",
        "passcode_extend.source.lock_screen" => "lock screen",
        "passcode_extend.source.tray_menu" => "tray menu",

        // ----- Watchdog Tamper Alert -----
        "watchdog.alert.prefix" => "Screen Time Manager stopped unexpectedly on",
        "watchdog.alert.restarted" => "and was just restarted.",
        "watchdog.alert.restart_failed" => "and the automatic restart failed - please check on it.",
        "watchdog.alert.suffix" => "If you didn't do this yourself (e.g. via Quit), this may be an attempt to bypass the time limit.",

        // ----- Clock Tamper Alert -----
        "clock_tamper.notify.header" => "the system clock was just changed on",
        "clock_tamper.notify.forward" => "Moved forward by",
        "clock_tamper.notify.backward" => "Moved back by",
        "clock_tamper.notify.suffix" => "Continuing to enforce today's remaining time as already tracked - the new date is being ignored until the clock is back in sync.",

        // ----- Activity / Low-time / Out-of-time Alerts -----
        "activity.notify.idle" => "went idle - timer paused",
        "activity.notify.active" => "is active again - timer resumed",
        "warning.notify.header" => "has about",
        "warning.notify.minutes" => "left",
        "outoftime.notify.header" => "is out of screen time - locked now",

        // ----- Telegram Setup Wizard -----
        "wizard.title" => "Telegram Setup Wizard",
        "wizard.step" => "Step",
        "wizard.of" => "of",
        "wizard.next" => "Next",
        "wizard.back" => "Back",
        "wizard.finish" => "Finish",
        "wizard.cancel" => "Cancel",
        "wizard.skip" => "Skip",

        // Step 1: Welcome
        "wizard.welcome.title" => "Remote Control via Telegram",
        "wizard.welcome.desc1" => "Control Screen Time Manager from your phone!",
        "wizard.welcome.desc2" => "With Telegram you can:",
        "wizard.welcome.feature1" => "Check remaining time",
        "wizard.welcome.feature2" => "Extend or reduce time remotely",
        "wizard.welcome.feature3" => "Lock the screen instantly",
        "wizard.welcome.feature4" => "Receive notifications",
        "wizard.welcome.ready" => "Let's set it up in 3 easy steps!",

        // Step 2: Create Bot
        "wizard.bot.title" => "Create Your Bot",
        "wizard.bot.step1" => "1. Open Telegram on your phone",
        "wizard.bot.step2" => "2. Search for  @BotFather",
        "wizard.bot.step3" => "3. Send the message:  /newbot",
        "wizard.bot.step4" => "4. Choose a name (e.g. \"My Screen Time\")",
        "wizard.bot.step5" => "5. Choose a username ending in 'bot'",
        "wizard.bot.step6" => "6. BotFather will give you a token - copy it!",
        "wizard.bot.hint" => "The token looks like: 123456789:ABCdef...",

        // Step 3: Enter Token
        "wizard.token.title" => "Enter Your Bot Token",
        "wizard.token.label" => "Paste the token from BotFather:",
        "wizard.token.placeholder" => "123456789:ABCdefGHI...",
        "wizard.token.invalid" => "This doesn't look like a valid token",
        "wizard.token.valid" => "Token looks good!",

        // Step 4: Connect
        "wizard.connect.title" => "Connect to Your Bot",
        "wizard.connect.step1" => "1. Open Telegram",
        "wizard.connect.step2" => "2. Search for your new bot",
        "wizard.connect.step3" => "3. Press START or send any message",
        "wizard.connect.waiting" => "Waiting for your message...",
        "wizard.connect.detected" => "Connection detected!",
        "wizard.connect.chatid" => "Your Chat ID:",

        // Step 5: Success
        "wizard.success.title" => "Setup Complete!",
        "wizard.success.desc" => "Your Telegram bot is ready to use.",
        "wizard.success.test" => "A test message was sent to your phone.",
        "wizard.success.commands" => "Try these commands in Telegram:",
        "wizard.success.cmd1" => "/status - Check remaining time",
        "wizard.success.cmd2" => "/extend 30 - Add 30 minutes",
        "wizard.success.cmd3" => "/lock - Lock the screen",
        "wizard.success.cmd4" => "/help - See all commands",

        // ----- Discord Setup Wizard -----
        "wizard.dc.title" => "Discord Setup Wizard",

        // Step 1: Welcome
        "wizard.dc.welcome.title" => "Remote Control via Discord",
        "wizard.dc.welcome.desc1" => "Control Screen Time Manager from Discord!",
        "wizard.dc.welcome.desc2" => "With Discord you can:",
        "wizard.dc.welcome.ready" => "Let's set it up in a few steps!",

        // Step 2: Create Bot
        "wizard.dc.bot.title" => "Create Your Bot",
        "wizard.dc.bot.step1" => "1. Go to discord.com/developers/applications",
        "wizard.dc.bot.step2" => "2. Click \"New Application\" and give it a name",
        "wizard.dc.bot.step3" => "3. Open the \"Bot\" tab",
        "wizard.dc.bot.step4" => "4. Enable \"Message Content Intent\"",
        "wizard.dc.bot.step5" => "5. Click \"Reset Token\" and copy it",
        "wizard.dc.bot.step6" => "6. Go to \"OAuth2 > URL Generator\"",
        "wizard.dc.bot.step7" => "7. Check \"bot\" + Send Messages/View Channels/Read History, open the URL",
        "wizard.dc.bot.hint" => "Keep this token secret - anyone with it can control the bot",

        // Step 3: Enter Token
        "wizard.dc.token.title" => "Enter Your Bot Token",
        "wizard.dc.token.label" => "Paste the token from the Developer Portal:",
        "wizard.dc.token.invalid" => "This doesn't look like a valid token",
        "wizard.dc.token.valid" => "Token looks good!",

        // Step 4: Channel & User ID
        "wizard.dc.ids.title" => "Channel & User ID",
        "wizard.dc.ids.step1" => "1. In Discord, enable Developer Mode (User Settings > Advanced)",
        "wizard.dc.ids.step2" => "2. Right-click the channel to use, then \"Copy Channel ID\"",
        "wizard.dc.ids.step3" => "3. Right-click your own username, then \"Copy User ID\"",
        "wizard.dc.ids.channel_label" => "Channel ID:",
        "wizard.dc.ids.user_label" => "Your User ID:",
        "wizard.dc.ids.invalid" => "Please enter both IDs (numbers only)",

        // Step 5: Success
        "wizard.dc.success.title" => "Setup Complete!",
        "wizard.dc.success.desc" => "Your Discord bot is ready to use.",
        "wizard.dc.success.test" => "A test message was sent to your channel.",
        "wizard.dc.success.commands" => "Try these commands in Discord:",
        "wizard.dc.success.cmd1" => "!status - Check remaining time",
        "wizard.dc.success.cmd2" => "!extend 30 - Add 30 minutes",
        "wizard.dc.success.cmd3" => "!lock - Lock the screen",
        "wizard.dc.success.cmd4" => "!help - See all commands",

        // Fallback - return empty string for unknown keys (should not happen in practice)
        _ => "",
    }
}

// ============================================================================
// German strings
// ============================================================================

fn de(key: &str) -> &'static str {
    match key {
        // ----- Window Titles -----
        "window.settings" => "Bildschirmzeit Einstellungen",
        "window.passcode" => "Code eingeben",
        "window.stats" => "Heutige Statistik",
        "window.blocking" => "Bildschirmzeit - Zeit abgelaufen!",
        "window.about" => "Info",

        // ----- Settings Dialog - Section Titles -----
        "settings.daily_limits" => "Tägliche Zeitlimits (Minuten)",
        "settings.warning1" => "Erste Warnung",
        "settings.warning2" => "Zweite Warnung",
        "settings.blocking_message" => "Sperrbildschirm-Nachricht",
        "settings.passcode" => "Code ändern (leer lassen zum Behalten)",
        "settings.telegram" => "Telegram Bot",
        "settings.discord" => "Discord Bot",
        "settings.lock_screen" => "Bildschirmsperre",
        "settings.idle" => "Leerlauferkennung",
        "settings.language" => "Sprache",

        // ----- Settings Dialog - Labels -----
        "settings.minutes_before" => "Minuten vorher:",
        "settings.message" => "Nachricht:",
        "settings.current" => "Aktuell:",
        "settings.new" => "Neu:",
        "settings.confirm" => "Bestätigen:",
        "settings.enable_rotating_pin" => "Rotierenden Tages-Code aktivieren (über Telegram/Discord, zusätzlich zu Ihrem Passcode)",
        "settings.enable_telegram" => "Telegram Bot aktivieren",
        "settings.bot_token" => "Bot Token:",
        "settings.chat_id" => "Chat ID:",
        "settings.setup_wizard" => "Einrichtungsassistent...",
        "settings.enable_discord" => "Discord Bot aktivieren",
        "settings.channel_id" => "Kanal-ID:",
        "settings.discord_user_id" => "Deine Benutzer-ID:",
        "settings.shutdown_timeout" => "Abschaltzeit:",
        "settings.auto_pause_idle" => "Auto-Pause bei Leerlauf",
        "settings.idle_timeout" => "Leerlaufzeit (Min):",

        // ----- Settings Dialog - Buttons -----
        "button.save" => "Speichern",
        "button.cancel" => "Abbrechen",
        "button.ok" => "OK",
        "button.close" => "Schließen",
        "button.reset_timer" => "Timer zurücksetzen",

        // ----- Settings Dialog - Messages -----
        "settings.error.current_incorrect" => "Aktueller Code ist falsch!",
        "settings.error.passcode_length" => "Neuer Code muss genau 4 Ziffern haben!",
        "settings.error.passcode_mismatch" => "Neuer Code und Bestätigung stimmen nicht überein!",
        "settings.success.saved" => "Einstellungen erfolgreich gespeichert!",
        "settings.error" => "Fehler",
        "settings.success" => "Einstellungen",

        // ----- Passcode Dialog -----
        "passcode.subtitle" => "4-stelligen Code eingeben",
        "passcode.incorrect" => "Falscher Code",

        // ----- Stats Dialog -----
        "stats.title" => "Heutige Statistik",
        "stats.day" => "Tag:",
        "stats.daily_limit" => "Tageslimit:",
        "stats.time_used" => "Zeit genutzt:",
        "stats.time_remaining" => "Zeit verbleibend:",
        "stats.pause_mode" => "Pause-Modus",
        "stats.pause_used" => "Pause genutzt:",
        "stats.pause_remaining" => "Pause verbleibend:",
        "stats.pauses_today" => "Pausen heute:",
        "stats.log" => "Protokoll:",
        "stats.pause_disabled" => "Pause-Funktion ist deaktiviert",
        "stats.timer_reset" => "Timer wurde auf das Tageslimit zurückgesetzt.",
        "stats.timer_reset_title" => "Timer zurückgesetzt",

        // ----- Tray Menu -----
        "tray.tooltip" => "Bildschirmzeit Manager",
        "tray.stats" => "Heutige Statistik...",
        "tray.settings" => "Einstellungen...",
        "tray.extend_15" => "+15 Min verlängern",
        "tray.extend_45" => "+45 Min verlängern",
        "tray.resume" => "Timer fortsetzen",
        "tray.pause_idle" => "Pause (Leerlauf)",
        "tray.pause_disabled" => "Pause (Deaktiviert)",
        "tray.pause_budget_used" => "Pause (Budget aufgebraucht)",
        "tray.pause_time_low" => "Pause (Zeit zu niedrig)",
        "tray.idle_paused" => "Leerlauf: Pausiert",
        "tray.show_warning" => "Warnung anzeigen (5s)",
        "tray.show_blocking" => "Sperrbildschirm anzeigen",
        "tray.about" => "Info",
        "tray.quit" => "Beenden",

        // ----- Blocking Screen -----
        "blocking.times_up" => "Zeit abgelaufen!",
        "blocking.limit_reached" => "Bildschirmzeit-Limit erreicht",
        "blocking.extend_label" => "Zeit verlängern (Code erforderlich):",
        "blocking.passcode_label" => "Code zum Entsperren eingeben:",
        "blocking.incorrect" => "Falscher Code!",
        "blocking.shutdown_in" => "Herunterfahren in:",
        "blocking.shutdown_now" => "HERUNTERFAHREN IN:",
        "blocking.time_exceeded" => "Zeitlimit überschritten",
        "blocking.extend_15" => "+15 Min",
        "blocking.extend_30" => "+30 Min",
        "blocking.extend_60" => "+60 Min",
        "blocking.unlock" => "Entsperren",
        "blocking.shutdown" => "Herunterfahren",
        "blocking.confirm_shutdown" => "Möchten Sie den Computer wirklich herunterfahren?",
        "blocking.confirm_title" => "Herunterfahren bestätigen",
        "blocking.screen_locked" => "Bildschirm gesperrt",
        "blocking.request_note_label" => "Grund (optional):",
        "blocking.request_time_button" => "📨 Mehr Zeit anfragen",
        "blocking.request_sent" => "✅ Gesendet! Warten",
        "blocking.request_unavailable" => "Aktivieren Sie Telegram oder Discord in den Einstellungen, um hier mehr Zeit anzufragen",

        // ----- About Dialog -----
        "about.text" => "Bildschirmzeit Manager v1.0.40\n\nEine Kindersicherungs-App zur Verwaltung der Bildschirmzeit.\n\n(c) Simon Pamies",

        // ----- Pause Reasons -----
        "pause.disabled" => "Pause-Funktion ist deaktiviert",
        "pause.budget_exhausted" => "Tägliches Pause-Budget aufgebraucht",
        "pause.cooldown" => "Abklingzeit aktiv",
        "pause.min_active" => "Mehr aktive Zeit erforderlich",
        "pause.time_too_low" => "Zeit zu niedrig für Pause",

        // ----- Telegram Bot - Command Descriptions -----
        "tg.cmd.start" => "Bot starten",
        "tg.cmd.status" => "Verbleibende Zeit und Status anzeigen",
        "tg.cmd.time" => "Schnelle Zeitabfrage",
        "tg.cmd.extend" => "Zeit verlängern (z.B. /extend 30)",
        "tg.cmd.reduce" => "Zeit verringern (z.B. /reduce 30)",
        "tg.cmd.pause" => "Timer pausieren",
        "tg.cmd.resume" => "Timer fortsetzen",
        "tg.cmd.history" => "Heutige Pause-Aktivität anzeigen",
        "tg.cmd.msg" => "Nachricht anzeigen (z.B. /msg Mach deine Hausaufgaben!)",
        "tg.cmd.lock" => "Bildschirm sperren",
        "tg.cmd.stop" => "Bildschirm sperren (Alias)",
        "tg.cmd.reset" => "Timer auf Tageslimit zurücksetzen",
        "tg.cmd.e30" => "Um 30 Minuten verlängern",
        "tg.cmd.e60" => "Um 60 Minuten verlängern",
        "tg.cmd.e120" => "Um 120 Minuten verlängern",
        "tg.cmd.chatid" => "Chat-ID für Einrichtung abrufen",
        "tg.cmd.help" => "Diese Hilfe anzeigen",

        // ----- Telegram Bot - Responses -----
        "tg.status.header" => "Bildschirmzeit Status",
        "tg.status.user" => "Benutzer",
        "tg.status.remaining" => "Verbleibend:",
        "tg.status.paused" => "Pausiert:",
        "tg.status.pause_budget" => "Pause-Budget:",
        "tg.status.yes" => "Ja",
        "tg.status.no" => "Nein",
        "tg.status.idle" => "Ja (Leerlauf)",

        "tg.extend.specify_positive" => "Bitte geben Sie eine positive Minutenzahl an",
        "tg.extend.max_120" => "Maximale Verlängerung ist 120 Minuten",
        "tg.extend.success" => "Um",
        "tg.extend.success_suffix" => "verlängert",

        "tg.reduce.specify_positive" => "Bitte geben Sie eine positive Minutenzahl an",
        "tg.reduce.max_120" => "Maximale Verringerung ist 120 Minuten",
        "tg.reduce.not_enough" => "Kann nicht verringern - nicht genug Zeit verbleibend",
        "tg.reduce.success" => "Um",
        "tg.reduce.success_suffix" => "verringert",

        "tg.pause.already_paused" => "Timer ist bereits pausiert. Verwenden Sie /resume zum Fortsetzen.",
        "tg.pause.idle_paused" => "Timer ist bereits pausiert (Leerlauf). Er wird automatisch fortgesetzt, wenn Eingabe erkannt wird.",
        "tg.pause.success" => "Timer pausiert",
        "tg.pause.failed" => "Timer wurde nicht pausiert (unerwarteter Zustand)",
        "tg.pause.cannot" => "Kann nicht pausieren:",

        "tg.resume.idle_auto" => "Timer ist im Leerlauf pausiert. Er wird automatisch fortgesetzt, wenn Eingabe erkannt wird.",
        "tg.resume.not_paused" => "Timer ist nicht pausiert",
        "tg.resume.success" => "Timer fortgesetzt",
        "tg.resume.failed" => "Timer ist noch pausiert (unerwarteter Zustand)",
        "tg.resume.cannot" => "Kann nicht fortsetzen:",

        "tg.history.header" => "Heutige Aktivität",
        "tg.history.uptime" => "Laufzeit:",
        "tg.history.pause_used" => "Pause genutzt:",
        "tg.history.no_events" => "Keine Pause-Ereignisse heute",

        "tg.weekly.header" => "Nutzung der letzten 2 Wochen",

        "tg.msg.provide" => "Bitte geben Sie eine Nachricht an, z.B. /msg Mach deine Hausaufgaben!",
        "tg.msg.shown" => "Nachricht angezeigt:",

        "tg.setmessage.usage" => "Bitte geben Sie eine Nachricht an, z.B. /setmessage Zeit für eine Pause!",
        "tg.setmessage.success" => "Sperrbildschirm-Nachricht aktualisiert",

        "tg.setpin.invalid" => "Bitte geben Sie genau 4 Ziffern an, z.B. /setpin 1234",
        "tg.setpin.success" => "Code aktualisiert",

        "tg.rotatingpin.usage" => "Bitte geben Sie on oder off an, z.B. /rotatingpin on",
        "tg.rotatingpin.enabled" => "🔁 Rotierender Tages-Code aktiviert. Er funktioniert zusätzlich zu Ihrem Passcode (der weiterhin immer funktioniert) überall, wo ein Code benötigt wird. Verwenden Sie /getpin, um den heutigen Code abzurufen.",
        "tg.rotatingpin.disabled" => "🔁 Rotierender Tages-Code deaktiviert. Nur Ihr Passcode funktioniert jetzt.",

        "tg.getpin.not_enabled" => "Der rotierende Code ist nicht aktiviert. Aktivieren Sie ihn mit /rotatingpin on",
        "tg.getpin.header" => "Heutiger Code:",
        "tg.getpin.note" => "Funktioniert überall wie Ihr Passcode. Ändert sich morgen.",

        "tg.setlimit.usage" => "Bitte geben Sie Minuten an, z.B. /setlimit 90 oder /setlimit saturday 180",
        "tg.setlimit.invalid_day" => "Unbekannter Tag - verwenden Sie monday, tuesday, wednesday, thursday, friday, saturday oder sunday",
        "tg.setlimit.invalid_minutes" => "Bitte geben Sie eine ganze Anzahl Minuten an",
        "tg.setlimit.max_1440" => "Maximales Tageslimit ist 1440 Minuten (24 Stunden)",
        "tg.setlimit.success" => "Tageslimit für",
        "tg.setlimit.to" => "gesetzt auf",

        "tg.reset.success" => "Timer auf Tageslimit zurückgesetzt",
        "tg.reset.remaining" => "Verbleibend:",

        "tg.lock.success" => "Bildschirm gesperrt",

        "tg.unlock.not_locked" => "Der Bildschirm ist momentan nicht gesperrt.",
        "tg.unlock.no_time" => "Keine Zeit mehr übrig - verwenden Sie /extend oder /reset.",
        "tg.unlock.success" => "Bildschirm entsperrt",

        "tg.error.unknown_cmd" => "Unbekannter Befehl. Verwenden Sie /help für verfügbare Befehle.",
        "tg.error.unauthorized" => "Nicht autorisiert. Dieser Bot ist für einen bestimmten Benutzer konfiguriert.",
        "tg.error.no_admin" => "Kein Admin konfiguriert. Bitte setzen Sie Ihre Chat-ID in den Einstellungen.",
        "tg.chatid.your_id" => "Ihre Chat-ID ist:",

        "tg.notify.started" => "Bildschirmzeit Manager gestartet",
        "tg.notify.shutdown" => "Bildschirmzeit Manager wird heruntergefahren",

        // ----- Discord Bot - Befehlsbeschreibungen -----
        "dc.help.header" => "Bildschirmzeit Manager Discord Bot Befehle:",
        "dc.cmd.status" => "Verbleibende Zeit und Status anzeigen",
        "dc.cmd.time" => "Schnelle Zeitabfrage",
        "dc.cmd.extend" => "Zeit verlängern (z.B. !extend 30)",
        "dc.cmd.reduce" => "Zeit verringern (z.B. !reduce 30)",
        "dc.cmd.pause" => "Timer pausieren",
        "dc.cmd.resume" => "Timer fortsetzen",
        "dc.cmd.history" => "Heutige Pause-Aktivität anzeigen",
        "dc.cmd.weekly" => "2-Wochen-Nutzungstabelle anzeigen",
        "dc.cmd.msg" => "Nachricht anzeigen (z.B. !msg Mach deine Hausaufgaben!)",
        "dc.cmd.lock" => "Bildschirm sperren",
        "dc.cmd.unlock" => "Entsperren ohne verbleibende Zeit zu ändern (nur wenn noch Zeit übrig ist)",
        "dc.cmd.reset" => "Timer auf Tageslimit zurücksetzen",
        "dc.cmd.setmessage" => "Sperrbildschirm-Nachricht ändern (z.B. !setmessage Zeit für eine Pause!)",
        "dc.cmd.setpin" => "Code ändern (z.B. !setpin 1234)",
        "dc.cmd.setlimit" => "Tageslimit in Minuten setzen (z.B. !setlimit 90 oder !setlimit saturday 180)",
        "dc.cmd.rotatingpin" => "Rotierenden Tages-Code ein-/ausschalten (z.B. !rotatingpin on)",
        "dc.cmd.getpin" => "Heutigen rotierenden Code abrufen (falls aktiviert)",
        "dc.cmd.e30" => "Um 30 Minuten verlängern",
        "dc.cmd.e60" => "Um 60 Minuten verlängern",
        "dc.cmd.e120" => "Um 120 Minuten verlängern",
        "dc.cmd.whoami" => "Ihre Discord-Benutzer-ID zur Einrichtung abrufen",
        "dc.cmd.help" => "Diese Hilfemeldung anzeigen",

        "dc.error.unknown_cmd" => "Unbekannter Befehl. Verwenden Sie !help für verfügbare Befehle.",
        "dc.error.no_admin" => "Kein Admin konfiguriert. Bitte setzen Sie Ihre Discord-Benutzer-ID in den Einstellungen.",
        "dc.error.specify_number" => "Bitte geben Sie eine Anzahl Minuten an, z.B. !extend 30",
        "dc.whoami.your_id" => "Ihre Discord-Benutzer-ID ist:",

        // ----- Time Request Notifications (Sperrbildschirm -> Bots) -----
        "request.notify.header" => "fordert mehr Zeit an!",
        "request.notify.reply_tg" => "Antworten Sie mit /extend 30 zum Gewähren, oder /unlock falls noch Zeit übrig ist.",
        "request.notify.reply_dc" => "Antworten Sie mit !extend 30 zum Gewähren, oder !unlock falls noch Zeit übrig ist.",
        "request.notify.resolved" => "Zeitanfrage bereits gewährt:",

        // ----- Passcode-basierte Verlängerungs-Benachrichtigungen -----
        "passcode_extend.notify.header" => "hat den Code verwendet, um",
        "passcode_extend.notify.minutes" => "hinzuzufügen",
        "passcode_extend.source.lock_screen" => "Sperrbildschirm",
        "passcode_extend.source.tray_menu" => "Taskleisten-Menü",

        // ----- Watchdog Manipulationswarnung -----
        "watchdog.alert.prefix" => "Bildschirmzeit Manager wurde unerwartet beendet auf",
        "watchdog.alert.restarted" => "und wurde soeben neu gestartet.",
        "watchdog.alert.restart_failed" => "und der automatische Neustart ist fehlgeschlagen - bitte überprüfen.",
        "watchdog.alert.suffix" => "Wenn Sie das nicht selbst getan haben (z.B. über Beenden), könnte dies ein Versuch sein, das Zeitlimit zu umgehen.",

        // ----- Uhrzeit-Manipulationswarnung -----
        "clock_tamper.notify.header" => "die Systemuhr wurde soeben geändert auf",
        "clock_tamper.notify.forward" => "Vorgestellt um",
        "clock_tamper.notify.backward" => "Zurückgestellt um",
        "clock_tamper.notify.suffix" => "Das heutige Restguthaben wird wie bisher verfolgt - das neue Datum wird ignoriert, bis die Uhr wieder synchron ist.",

        // ----- Aktivitäts-/Restzeit-/Zeitablauf-Benachrichtigungen -----
        "activity.notify.idle" => "ist inaktiv - Timer pausiert",
        "activity.notify.active" => "ist wieder aktiv - Timer läuft weiter",
        "warning.notify.header" => "hat noch etwa",
        "warning.notify.minutes" => "übrig",
        "outoftime.notify.header" => "hat keine Bildschirmzeit mehr - jetzt gesperrt",

        // ----- Telegram Setup Wizard -----
        "wizard.title" => "Telegram Einrichtungsassistent",
        "wizard.step" => "Schritt",
        "wizard.of" => "von",
        "wizard.next" => "Weiter",
        "wizard.back" => "Zurück",
        "wizard.finish" => "Fertig",
        "wizard.cancel" => "Abbrechen",
        "wizard.skip" => "Überspringen",

        // Step 1: Welcome
        "wizard.welcome.title" => "Fernsteuerung via Telegram",
        "wizard.welcome.desc1" => "Steuern Sie den Bildschirmzeit Manager vom Handy!",
        "wizard.welcome.desc2" => "Mit Telegram können Sie:",
        "wizard.welcome.feature1" => "Verbleibende Zeit prüfen",
        "wizard.welcome.feature2" => "Zeit ferngesteuert verlängern oder verkürzen",
        "wizard.welcome.feature3" => "Bildschirm sofort sperren",
        "wizard.welcome.feature4" => "Benachrichtigungen erhalten",
        "wizard.welcome.ready" => "Richten wir es in 3 einfachen Schritten ein!",

        // Step 2: Create Bot
        "wizard.bot.title" => "Erstellen Sie Ihren Bot",
        "wizard.bot.step1" => "1. Öffnen Sie Telegram auf Ihrem Handy",
        "wizard.bot.step2" => "2. Suchen Sie nach  @BotFather",
        "wizard.bot.step3" => "3. Senden Sie die Nachricht:  /newbot",
        "wizard.bot.step4" => "4. Wählen Sie einen Namen (z.B. \"Meine Bildschirmzeit\")",
        "wizard.bot.step5" => "5. Wählen Sie einen Benutzernamen mit 'bot' am Ende",
        "wizard.bot.step6" => "6. BotFather gibt Ihnen einen Token - kopieren Sie ihn!",
        "wizard.bot.hint" => "Der Token sieht so aus: 123456789:ABCdef...",

        // Step 3: Enter Token
        "wizard.token.title" => "Bot-Token eingeben",
        "wizard.token.label" => "Fügen Sie den Token von BotFather ein:",
        "wizard.token.placeholder" => "123456789:ABCdefGHI...",
        "wizard.token.invalid" => "Das sieht nicht wie ein gültiger Token aus",
        "wizard.token.valid" => "Token sieht gut aus!",

        // Step 4: Connect
        "wizard.connect.title" => "Mit Ihrem Bot verbinden",
        "wizard.connect.step1" => "1. Öffnen Sie Telegram",
        "wizard.connect.step2" => "2. Suchen Sie nach Ihrem neuen Bot",
        "wizard.connect.step3" => "3. Drücken Sie START oder senden Sie eine Nachricht",
        "wizard.connect.waiting" => "Warte auf Ihre Nachricht...",
        "wizard.connect.detected" => "Verbindung erkannt!",
        "wizard.connect.chatid" => "Ihre Chat-ID:",

        // Step 5: Success
        "wizard.success.title" => "Einrichtung abgeschlossen!",
        "wizard.success.desc" => "Ihr Telegram-Bot ist einsatzbereit.",
        "wizard.success.test" => "Eine Testnachricht wurde an Ihr Handy gesendet.",
        "wizard.success.commands" => "Probieren Sie diese Befehle in Telegram:",
        "wizard.success.cmd1" => "/status - Verbleibende Zeit prüfen",
        "wizard.success.cmd2" => "/extend 30 - 30 Minuten hinzufügen",
        "wizard.success.cmd3" => "/lock - Bildschirm sperren",
        "wizard.success.cmd4" => "/help - Alle Befehle anzeigen",

        // ----- Discord Einrichtungsassistent -----
        "wizard.dc.title" => "Discord Einrichtungsassistent",

        // Schritt 1: Willkommen
        "wizard.dc.welcome.title" => "Fernsteuerung via Discord",
        "wizard.dc.welcome.desc1" => "Steuern Sie den Bildschirmzeit Manager über Discord!",
        "wizard.dc.welcome.desc2" => "Mit Discord können Sie:",
        "wizard.dc.welcome.ready" => "Richten wir es in wenigen Schritten ein!",

        // Schritt 2: Bot erstellen
        "wizard.dc.bot.title" => "Erstellen Sie Ihren Bot",
        "wizard.dc.bot.step1" => "1. Gehen Sie zu discord.com/developers/applications",
        "wizard.dc.bot.step2" => "2. Klicken Sie auf \"New Application\" und vergeben Sie einen Namen",
        "wizard.dc.bot.step3" => "3. Öffnen Sie den Tab \"Bot\"",
        "wizard.dc.bot.step4" => "4. Aktivieren Sie \"Message Content Intent\"",
        "wizard.dc.bot.step5" => "5. Klicken Sie auf \"Reset Token\" und kopieren Sie ihn",
        "wizard.dc.bot.step6" => "6. Gehen Sie zu \"OAuth2 > URL Generator\"",
        "wizard.dc.bot.step7" => "7. Wählen Sie \"bot\" + Send Messages/View Channels/Read History, öffnen Sie die URL",
        "wizard.dc.bot.hint" => "Halten Sie diesen Token geheim - damit kann jeder den Bot steuern",

        // Schritt 3: Token eingeben
        "wizard.dc.token.title" => "Bot-Token eingeben",
        "wizard.dc.token.label" => "Fügen Sie den Token aus dem Developer Portal ein:",
        "wizard.dc.token.invalid" => "Das sieht nicht wie ein gültiger Token aus",
        "wizard.dc.token.valid" => "Token sieht gut aus!",

        // Schritt 4: Kanal- & Benutzer-ID
        "wizard.dc.ids.title" => "Kanal- & Benutzer-ID",
        "wizard.dc.ids.step1" => "1. Aktivieren Sie den Entwicklermodus in Discord (Benutzereinstellungen > Erweitert)",
        "wizard.dc.ids.step2" => "2. Rechtsklick auf den gewünschten Kanal, dann \"ID kopieren\"",
        "wizard.dc.ids.step3" => "3. Rechtsklick auf Ihren eigenen Benutzernamen, dann \"ID kopieren\"",
        "wizard.dc.ids.channel_label" => "Kanal-ID:",
        "wizard.dc.ids.user_label" => "Ihre Benutzer-ID:",
        "wizard.dc.ids.invalid" => "Bitte geben Sie beide IDs ein (nur Zahlen)",

        // Schritt 5: Erfolg
        "wizard.dc.success.title" => "Einrichtung abgeschlossen!",
        "wizard.dc.success.desc" => "Ihr Discord-Bot ist einsatzbereit.",
        "wizard.dc.success.test" => "Eine Testnachricht wurde an Ihren Kanal gesendet.",
        "wizard.dc.success.commands" => "Probieren Sie diese Befehle in Discord:",
        "wizard.dc.success.cmd1" => "!status - Verbleibende Zeit prüfen",
        "wizard.dc.success.cmd2" => "!extend 30 - 30 Minuten hinzufügen",
        "wizard.dc.success.cmd3" => "!lock - Bildschirm sperren",
        "wizard.dc.success.cmd4" => "!help - Alle Befehle anzeigen",

        // Fallback to English
        _ => en(key),
    }
}
