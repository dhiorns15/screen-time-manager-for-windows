//! Platform-agnostic remote command implementations
//! Shared by both the Telegram and Discord bot integrations

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::blocking;
use crate::database;
use crate::i18n;
use crate::mini_overlay;
use crate::overlay;
use crate::update;

/// Windows account currently running the app - useful when the bot config is
/// shared across multiple accounts/children, so replies say whose data it is.
pub fn current_windows_username() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "?".to_string())
}

/// Format a duration in seconds as "Xh Ym" (or just "Ym" under an hour) -
/// used throughout bot-facing text instead of raw MM:SS, for a cleaner,
/// easier-to-scan report.
pub fn format_hm(total_seconds: i32) -> String {
    let total_seconds = total_seconds.max(0);
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
}

/// Join non-empty word fragments with single spaces - for composing
/// "prefix amount suffix" phrases where a language may need words on either
/// side of the number (German's "um...zu" constructions) or only one side
/// (English), without stray double-spaces when a fragment is empty.
pub fn join_words(parts: &[&str]) -> String {
    parts.iter().filter(|p| !p.is_empty()).copied().collect::<Vec<_>>().join(" ")
}

/// The "👤 User: <account>" line. Limits, the lock-screen message and the
/// timer are per Windows account, and a command acts on whichever account
/// owns the console when it arrives - so replies that read or change them
/// say which account that was.
pub fn user_line() -> String {
    format!("👤 {}: {}", i18n::t("tg.status.user"), current_windows_username())
}

/// `reply` with the `user_line` added - see `reply_names_account`.
pub fn with_account(reply: String) -> String {
    format!("{reply}\n{}", user_line())
}

/// Whether the reply to the bot command `name` (lowercase, no prefix) ends
/// with the account it acted on (`with_account`). `status` and `history`
/// include it in their own text already.
pub fn reply_names_account(name: &str) -> bool {
    matches!(
        name,
        "time" | "extend" | "e30" | "e60" | "e120" | "reduce" | "reset" | "lock" | "stop" | "unlock"
            | "pause" | "resume" | "setlimit" | "setmessage"
    )
}

/// What the timer is doing right now, for `status`. Mirrors the order in
/// mini_overlay.rs's `tick_mode`: the app's lock screen freezes everything,
/// then a manual pause, then idle or the Windows lock screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerState {
    /// Counting down.
    Running,
    /// Paused by hand (counts against the pause budget).
    Paused,
    /// Stopped because nobody is using the PC.
    Idle,
    /// Stopped because Windows is locked (or another account has the screen).
    WindowsLocked,
    /// The app's lock screen is up because the time ran out.
    OutOfTime,
    /// The app's lock screen is up although there is time left (`lock`).
    Locked,
}

/// `TimerState` from the individual flags. `manual_lock` says whether the
/// app's lock screen was put up by hand (`lock`) rather than by running out
/// of time; `None` when that isn't known, and then a lock screen with time
/// left counts as a manual lock.
pub fn timer_state(
    blocked: bool,
    manual_lock: Option<bool>,
    remaining: i32,
    paused: bool,
    idle_paused: bool,
    windows_locked: bool,
) -> TimerState {
    if blocked {
        if manual_lock.unwrap_or(remaining > 0) {
            TimerState::Locked
        } else {
            TimerState::OutOfTime
        }
    } else if paused {
        TimerState::Paused
    } else if idle_paused {
        TimerState::Idle
    } else if windows_locked {
        TimerState::WindowsLocked
    } else {
        TimerState::Running
    }
}

/// The timer's state right now. A parent's lock (`blocking::is_manual_lock`)
/// reads as `Locked` whether or not time is left; a lock screen without one
/// as `OutOfTime`.
pub fn current_timer_state() -> TimerState {
    timer_state(
        blocking::is_blocking_overlay_visible(),
        Some(blocking::is_manual_lock()),
        blocking::get_remaining_seconds(),
        mini_overlay::is_paused(),
        mini_overlay::is_idle_paused(),
        mini_overlay::IS_LOCK_PAUSED.load(Ordering::SeqCst),
    )
}

/// The i18n key of `status`'s text for `state`.
fn timer_state_key(state: TimerState) -> &'static str {
    match state {
        TimerState::Running => "bot.status.running",
        TimerState::Paused => "bot.status.paused",
        TimerState::Idle => "bot.status.idle",
        TimerState::WindowsLocked => "bot.status.windows_locked",
        TimerState::OutOfTime => "bot.status.out_of_time",
        TimerState::Locked => "bot.status.locked",
    }
}

fn remaining_emoji(remaining: i32) -> &'static str {
    if remaining <= 60 {
        "🔴"
    } else if remaining <= 300 {
        "🟠"
    } else {
        "🟢"
    }
}

pub fn cmd_status() -> String {
    status_text(current_timer_state())
}

/// `status`'s reply, with the timer's state given by the caller.
pub fn status_text(state: TimerState) -> String {
    let remaining = blocking::get_remaining_seconds();
    let pause_budget = mini_overlay::get_remaining_pause_budget();

    format!(
        "{}\n\
         ━━━━━━━━━━━━━━━━━━\n\
         {}\n\
         {} {}: {}\n\
         ⏱ {}: {}\n\
         🔋 {}: {}",
        i18n::t("tg.status.header"),
        user_line(),
        remaining_emoji(remaining),
        i18n::t("tg.status.remaining"),
        format_hm(remaining),
        i18n::t("bot.status.timer"),
        i18n::t(timer_state_key(state)),
        i18n::t("tg.status.pause_budget"),
        format_hm(pause_budget)
    )
}

pub fn cmd_time() -> String {
    let remaining = blocking::get_remaining_seconds();
    format!(
        "{} {}",
        remaining_emoji(remaining),
        i18n::t("bot.time.remaining").replace("{time}", &format_hm(remaining))
    )
}

/// Split the text after a command prefix ("/" or "!") into the command name
/// and its arguments (trimmed): "extend  30" -> ("extend", "30"). The name
/// ends at the first whitespace of any kind, so "msg\nHello" works too.
pub fn split_command(rest: &str) -> (&str, &str) {
    match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => (rest, ""),
    }
}

/// A command's minutes argument: a whole number, possibly negative (the
/// command itself explains the allowed range). `None` if missing or not a
/// number ("30min", "thirty").
pub fn minutes_arg(args: &str) -> Option<i32> {
    args.trim().parse().ok()
}

/// Run `f` with the minutes in `args`, or explain how to give them, with
/// `example_command` (e.g. "/extend") as the example.
pub fn dispatch_minutes(args: &str, example_command: &str, f: impl FnOnce(i32) -> String) -> String {
    match minutes_arg(args) {
        Some(minutes) => f(minutes),
        None => format!("{} {example_command} 30", i18n::t("bot.error.specify_minutes")),
    }
}

pub fn cmd_extend(minutes: i32) -> String {
    if minutes <= 0 {
        return i18n::t("tg.extend.specify_positive").to_string();
    }
    if minutes > 120 {
        return i18n::t("tg.extend.max_120").to_string();
    }

    blocking::extend_time(minutes);

    // Hide the blocking overlay if it's showing
    unsafe {
        blocking::hide_blocking_overlay();
    }

    // Get new remaining time
    let remaining = blocking::get_remaining_seconds();

    format!("✅ {}\n{}: {}",
        join_words(&[i18n::t("tg.extend.success"), &format_hm(minutes * 60), i18n::t("tg.extend.success_suffix")]),
        i18n::t("tg.status.remaining"),
        format_hm(remaining))
}

pub fn cmd_reduce(minutes: i32) -> String {
    if minutes <= 0 {
        return i18n::t("tg.reduce.specify_positive").to_string();
    }
    if minutes > 120 {
        return i18n::t("tg.reduce.max_120").to_string();
    }

    let current = blocking::get_remaining_seconds();
    let reduction_seconds = minutes * 60;

    if reduction_seconds >= current {
        return format!("{} ({})",
            i18n::t("tg.reduce.not_enough"),
            format_hm(current));
    }

    blocking::reduce_time(minutes);

    // Get new remaining time
    let remaining = blocking::get_remaining_seconds();

    format!("⏬ {}\n{}: {}",
        join_words(&[i18n::t("tg.reduce.success"), &format_hm(minutes * 60), i18n::t("tg.reduce.success_suffix")]),
        i18n::t("tg.status.remaining"),
        format_hm(remaining))
}

pub fn cmd_pause() -> String {
    if mini_overlay::is_paused() {
        return format!("⏸ {}", i18n::t("tg.pause.already_paused"));
    }
    if mini_overlay::is_idle_paused() {
        return format!("⏸ {}", i18n::t("tg.pause.idle_paused"));
    }

    match mini_overlay::toggle_pause() {
        Ok(true) => format!("⏸ {}", i18n::t("tg.pause.success")),
        Ok(false) => i18n::t("tg.pause.failed").to_string(),
        Err(reason) => format!("{} {}", i18n::t("tg.pause.cannot"), format_pause_reason(reason)),
    }
}

pub fn cmd_resume() -> String {
    if mini_overlay::is_idle_paused() {
        return format!("▶️ {}", i18n::t("tg.resume.idle_auto"));
    }
    if !mini_overlay::is_paused() {
        return format!("▶️ {}", i18n::t("tg.resume.not_paused"));
    }

    match mini_overlay::toggle_pause() {
        Ok(false) => format!("▶️ {}", i18n::t("tg.resume.success")),
        Ok(true) => i18n::t("tg.resume.failed").to_string(),
        Err(reason) => format!("{} {}", i18n::t("tg.resume.cannot"), format_pause_reason(reason)),
    }
}

/// Which chat markup `cmd_weekly` formats for. The table has to be sent as
/// a monospace block to line up, and each platform spells that differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WeeklyMarkup {
    /// Discord message markdown: a ``` code block.
    DiscordMarkdown,
    /// Telegram `ParseMode::Html`: a `<pre>` block. HTML rather than
    /// MarkdownV2 because MarkdownV2 rejects the whole message if any of
    /// `_*[]()~`>#+-=|{}.!` appears unescaped outside the block - which the
    /// English header ("2-Week Usage") and usernames like "anna.b" do.
    TelegramHtml,
}

/// Escape text for Telegram's HTML parse mode: only `&`, `<` and `>` are
/// special there (see https://core.telegram.org/bots/api#html-style).
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// Backslash-escape the characters Discord markdown would otherwise read as
/// formatting mid-line (so a username like "max_the_great" isn't italicised).
pub fn escape_discord_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '_' | '~' | '`' | '|') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Text someone other than the parent typed (the child's note on a time
/// request), made safe to post on Discord: shown as an inline code span, in
/// which Discord renders no formatting, headings, quotes, links, masked
/// links, emoji or mentions. Backticks (which would end the span) and line
/// breaks or other control characters are replaced. Pings are blocked
/// separately, by the allowed mentions every message is sent with.
pub fn discord_literal(text: &str) -> String {
    let inner: String = text
        .chars()
        .map(|c| match c {
            '`' => '\'',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let inner = inner.trim();
    if inner.is_empty() {
        return String::new();
    }
    format!("`{inner}`")
}

/// The same text for Telegram, which posts it as plain text (no parse
/// mode), so only line breaks and other control characters are replaced -
/// the note is meant to be one line.
pub fn telegram_literal(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

/// 14-day usage table (minutes active / paused per day) for one user, sent
/// as a monospace code block so it lines up as an actual table in
/// Telegram/Discord.
fn weekly_table_for(username: &str, history: &[(String, i32, i32)], markup: WeeklyMarkup) -> String {
    let mut table = format!("{:<6}{:>8}{:>9}\n", "Date", "Used", "Paused");
    for (date, active_min, pause_min) in history {
        let short_date = date.get(5..).unwrap_or(date); // MM-DD
        table.push_str(&format!(
            "{:<6}{:>8}{:>9}\n",
            short_date,
            format_hm(active_min * 60),
            format_hm(pause_min * 60)
        ));
    }
    match markup {
        WeeklyMarkup::DiscordMarkdown => {
            format!("👤 {}\n```\n{}```", escape_discord_markdown(username), table)
        }
        WeeklyMarkup::TelegramHtml => {
            format!("👤 {}\n<pre>{}</pre>", escape_html(username), escape_html(&table))
        }
    }
}

/// 14-day usage table for every Windows account that's used this app on
/// this machine - not just whoever happens to be running the command -
/// built from the shared database's mirrored per-user stats (see
/// `database::mirror_shared_daily_stat`). Falls back to this account's own
/// local history alone if the shared registry is empty (e.g. the shared
/// database is unavailable, or this is the first run since the per-user
/// mirroring was added and no shared history has accumulated yet).
pub fn cmd_weekly(markup: WeeklyMarkup) -> String {
    let all_users = database::get_all_users_daily_usage_history(14);

    let body = if all_users.is_empty() {
        weekly_table_for(&current_windows_username(), &database::get_daily_usage_history(14), markup)
    } else {
        all_users
            .iter()
            .map(|(username, history)| weekly_table_for(username, history, markup))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let header = match markup {
        WeeklyMarkup::DiscordMarkdown => i18n::t("tg.weekly.header").to_string(),
        WeeklyMarkup::TelegramHtml => escape_html(i18n::t("tg.weekly.header")),
    };
    format!("📊 {}\n{}", header, body)
}

pub fn cmd_history() -> String {
    let log = database::get_pause_log_today();
    let pause_used = database::get_pause_used_today();
    let pause_config = database::get_pause_config();
    let session_active = mini_overlay::SESSION_ACTIVE_SECONDS.load(Ordering::SeqCst);

    let mut response = format!(
        "📊 {}\n━━━━━━━━━━━━━━━━━━\n👤 {}: {}\n",
        i18n::t("tg.history.header"),
        i18n::t("tg.status.user"),
        current_windows_username(),
    );

    response.push_str(&format!("⏱ {} {}\n", i18n::t("tg.history.uptime"), format_hm(session_active)));

    response.push_str(&format!(
        "⏸ {} {} / {}\n\n",
        i18n::t("tg.history.pause_used"),
        format_hm(pause_used),
        format_hm((pause_config.daily_budget_minutes * 60) as i32)
    ));

    if log.is_empty() {
        response.push_str(i18n::t("tg.history.no_events"));
    } else {
        response.push_str(&format!("{}:\n", i18n::t("stats.log")));
        for entry in log {
            response.push_str(&format!("• {}\n", entry));
        }
    }

    response
}

pub fn cmd_msg(text: &str) -> String {
    if text.is_empty() {
        return i18n::t("tg.msg.provide").to_string();
    }

    unsafe {
        overlay::show_overlay(text, 10);
    }

    format!("📢 {}: \"{}\"", i18n::t("tg.msg.shown"), text)
}

pub fn cmd_setmessage(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return i18n::t("tg.setmessage.usage").to_string();
    }

    database::set_setting("blocking_message", text);

    format!("📝 {}", i18n::t("tg.setmessage.success"))
}

pub fn cmd_setpin(args: &str) -> String {
    let pin = args.trim();
    if pin.len() != 4 || !pin.chars().all(|c| c.is_ascii_digit()) {
        return i18n::t("tg.setpin.invalid").to_string();
    }

    database::set_passcode(pin);

    // Deliberately don't echo the new pin back into chat history.
    format!("🔑 {}", i18n::t("tg.setpin.success"))
}

pub fn cmd_rotatingpin(args: &str) -> String {
    match args.trim().to_lowercase().as_str() {
        "on" | "enable" | "enabled" => {
            database::set_rotating_pin_enabled(true);
            i18n::t("tg.rotatingpin.enabled").to_string()
        }
        "off" | "disable" | "disabled" => {
            database::set_rotating_pin_enabled(false);
            i18n::t("tg.rotatingpin.disabled").to_string()
        }
        _ => i18n::t("tg.rotatingpin.usage").to_string(),
    }
}

pub fn cmd_getpin() -> String {
    if !database::is_rotating_pin_enabled() {
        return i18n::t("tg.getpin.not_enabled").to_string();
    }
    format!(
        "🔢 {} {}\n{}",
        i18n::t("tg.getpin.header"),
        database::get_rotating_pin(),
        i18n::t("tg.getpin.note"),
    )
}

/// Clear the passcode lockout (see `passcode_guard`), so the next code typed
/// on the PC is checked. Admin-only, like every command but the setup ones:
/// otherwise a child could keep entry locked indefinitely, even against the
/// parent's correct code.
pub fn cmd_clearlockout() -> String {
    match crate::passcode_guard::clear_lockout() {
        Some(_) => format!("🔓 {}", i18n::t("bot.clearlockout.cleared")),
        None => format!("🔓 {}", i18n::t("bot.clearlockout.not_locked")),
    }
}

/// Parse a weekday name/abbreviation into the app's 0=Monday..6=Sunday index.
/// English only, like the rest of the bot command vocabulary.
fn parse_weekday_arg(s: &str) -> Option<u32> {
    match s.to_lowercase().as_str() {
        "mon" | "monday" => Some(0),
        "tue" | "tues" | "tuesday" => Some(1),
        "wed" | "weds" | "wednesday" => Some(2),
        "thu" | "thur" | "thurs" | "thursday" => Some(3),
        "fri" | "friday" => Some(4),
        "sat" | "saturday" => Some(5),
        "sun" | "sunday" => Some(6),
        _ => None,
    }
}

pub fn cmd_setlimit(args: &str) -> String {
    let args = args.trim();
    if args.is_empty() {
        return i18n::t("tg.setlimit.usage").to_string();
    }

    let parts: Vec<&str> = args.split_whitespace().collect();
    let (weekday, minutes_str) = if parts.len() == 1 {
        (database::get_current_weekday(), parts[0])
    } else {
        let Some(day) = parse_weekday_arg(parts[0]) else {
            return i18n::t("tg.setlimit.invalid_day").to_string();
        };
        (day, parts[parts.len() - 1])
    };

    let Ok(minutes) = minutes_str.parse::<u32>() else {
        return i18n::t("tg.setlimit.invalid_minutes").to_string();
    };
    if minutes > 1440 {
        return i18n::t("tg.setlimit.max_1440").to_string();
    }

    database::set_daily_limit(weekday, minutes);

    // If that changed today's limit, move the remaining time by the
    // difference - the same adjustment the Settings dialog makes on Save.
    // Another weekday's limit leaves today's time alone.
    blocking::apply_todays_limit_change();

    format!(
        "📅 {} {} {} {}",
        i18n::t("tg.setlimit.success"),
        i18n::weekday(weekday as usize),
        i18n::t("tg.setlimit.to"),
        format_hm((minutes * 60) as i32)
    )
}

pub fn cmd_reset() -> String {
    let daily_limit_seconds = blocking::reset_to_daily_limit();
    let daily_limit_minutes = (daily_limit_seconds / 60) as u32;

    unsafe {
        // Hide the blocking overlay if it's showing
        blocking::hide_blocking_overlay();
    }

    format!(
        "🔄 {} ({})\n{} {}",
        i18n::t("tg.reset.success"),
        format_hm((daily_limit_minutes * 60) as i32),
        i18n::t("tg.reset.remaining"),
        format_hm(daily_limit_seconds)
    )
}

pub fn cmd_lock() -> String {
    let message = database::get_blocking_message();

    // Remembered until a parent ends it (see blocking::MANUAL_LOCK)
    unsafe {
        blocking::start_manual_lock(&message);
    }

    format!("🔒 {}", i18n::t("tg.lock.success"))
}

/// Dismiss the blocking overlay without granting or resetting any time -
/// for undoing a manual lock while time is still left. Refuses when time has
/// actually run out, since dismissing with 0 remaining would leave the
/// countdown timer stopped with nothing left to re-trigger the lock screen.
pub fn cmd_unlock() -> String {
    // Ends a manual lock in every case; with no time left the out-of-time
    // lock stays, as before.
    blocking::end_manual_lock();
    if !blocking::is_blocking_overlay_visible() {
        return i18n::t("tg.unlock.not_locked").to_string();
    }

    let remaining = blocking::get_remaining_seconds();
    if remaining <= 0 {
        return i18n::t("tg.unlock.no_time").to_string();
    }

    unsafe {
        blocking::hide_blocking_overlay();
    }

    format!("🔓 {}", i18n::t("tg.unlock.success"))
}

/// Check GitHub and, if a newer release exists, start the SYSTEM update task
/// (see update.rs) - which installs it and restarts the app in every
/// session. Blocking (network + schtasks), so the bots run it off their
/// async runtime.
pub fn cmd_update() -> String {
    let current = update::Version::current();
    let release = match update::fetch_latest_release() {
        Ok(r) => r,
        Err(e) => return format!("❌ {} {e}", i18n::t("update.cmd.check_failed")),
    };
    if release.version <= current {
        return format!("✅ {} (v{current})", i18n::t("update.cmd.up_to_date"));
    }
    if let Err(e) = update::installer_asset(&release) {
        return format!(
            "❌ v{}: {} {e}\n{} {}",
            release.version,
            i18n::t("update.cmd.not_installable"),
            i18n::t("update.cmd.manual"),
            release.page
        );
    }
    match update::start_update_task() {
        Ok(()) => {
            update::watch_for_stalled_update(release.version);
            update::installing_text(release.version)
        }
        Err(e) => format!(
            "❌ {} {e}\n{} {}",
            i18n::t("update.cmd.start_failed"),
            i18n::t("update.cmd.manual"),
            release.page
        ),
    }
}

/// Format pause blocked reason for display
pub fn format_pause_reason(reason: mini_overlay::PauseBlockedReason) -> String {
    match reason {
        mini_overlay::PauseBlockedReason::Disabled => i18n::t("pause.disabled").to_string(),
        mini_overlay::PauseBlockedReason::BudgetExhausted => i18n::t("pause.budget_exhausted").to_string(),
        mini_overlay::PauseBlockedReason::CooldownActive { seconds_remaining } => {
            format!("{} ({})", i18n::t("pause.cooldown"), format_hm(seconds_remaining))
        }
        mini_overlay::PauseBlockedReason::MinActiveTimeNotMet { seconds_remaining } => {
            format!("{} ({})", i18n::t("pause.min_active"), format_hm(seconds_remaining))
        }
        mini_overlay::PauseBlockedReason::TimeTooLow => i18n::t("pause.time_too_low").to_string(),
    }
}

// ============================================================================
// Bot connection lifecycle, shared by telegram.rs and discord.rs
// ============================================================================

/// How long to wait before the next connection attempt after `failures`
/// consecutive failures (1 = the first): 2s, 4s, 8s, 16s, 32s, then once a
/// minute. Short enough that a laptop coming back online gets its bot back
/// within a minute, long enough not to hammer the API with a bad token.
pub fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs((1u64 << failures.clamp(1, 6)).min(60))
}

/// Commands older than this when they reach the app are dropped rather
/// than run. Telegram keeps undelivered messages for up to a day (and
/// Discord's gateway replays messages missed during a reconnect), so without
/// this a "/e60" sent while the PC was asleep or offline would run whenever
/// it next connected - for whichever account happened to be logged in then.
///
/// Measured against the chat service's clock (`server_now`), not the PC's
/// alone: a PC clock that is a few minutes fast used to make every incoming
/// message look stale and silently disable remote control.
pub const STALE_COMMAND_SECS: i64 = 5 * 60;

/// Whether a message sent at `sent_unix` (seconds since the Unix epoch, from
/// the chat service) is too old to act on at `now_unix`. A message dated in
/// the future (the PC's clock is behind) is never stale.
pub fn is_stale(sent_unix: i64, now_unix: i64) -> bool {
    now_unix.saturating_sub(sent_unix) > STALE_COMMAND_SECS
}

/// Seconds since the Unix epoch by the PC's clock (0 if the clock is set
/// before 1970, which makes nothing look stale).
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

// ============================================================================
// The chat services' clock, for judging how old a message is
// ============================================================================

/// Server time minus PC time, in seconds, as last measured from a chat
/// service's HTTP `Date` header (0 until measured, i.e. trust the PC clock).
/// Shared by both bots: both services keep accurate time.
static SERVER_CLOCK_OFFSET: AtomicI64 = AtomicI64::new(0);

/// When a stale-looking message last triggered a fresh measurement - see
/// `stale_age`.
static LAST_STALE_REMEASURE: Mutex<Option<Instant>> = Mutex::new(None);
const STALE_REMEASURE_INTERVAL: Duration = Duration::from_secs(60);

/// Where each bot reads the server time from. Any response carries a `Date`
/// header; these don't need the bot token.
pub const TELEGRAM_TIME_URL: &str = "https://api.telegram.org/";
pub const DISCORD_TIME_URL: &str = "https://discord.com/api/v10/gateway";

/// Seconds since the Unix epoch by the chat services' clock: the PC's clock
/// corrected by the last measured offset.
pub fn server_now() -> i64 {
    now_unix().saturating_add(SERVER_CLOCK_OFFSET.load(Ordering::SeqCst))
}

/// Parse an HTTP `Date` header (IMF-fixdate, e.g. "Sun, 06 Nov 1994
/// 08:49:37 GMT") into seconds since the Unix epoch.
pub fn parse_http_date(s: &str) -> Option<i64> {
    let mut parts = s.split_whitespace();
    let _weekday = parts.next()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1, "Feb" => 2, "Mar" => 3, "Apr" => 4, "May" => 5, "Jun" => 6,
        "Jul" => 7, "Aug" => 8, "Sep" => 9, "Oct" => 10, "Nov" => 11, "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (hms.next()??, hms.next()??, hms.next()??);
    if parts.next()? != "GMT" || parts.next().is_some() || hms.next().is_some() {
        return None;
    }
    if !(1..=31).contains(&day) || !(0..24).contains(&hour) || !(0..60).contains(&minute)
        || !(0..=60).contains(&second) || !(1970..=9999).contains(&year)
    {
        return None;
    }
    // Days from 1970-01-01 to the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400; // y >= 1969, so no negative-era rounding needed
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Server-minus-PC offset from a server timestamp taken somewhere between
/// the PC times `local_before` and `local_after` (request sent / response
/// received): compared with the middle of that window.
pub fn clock_offset(server: i64, local_before: i64, local_after: i64) -> i64 {
    server - (local_before + (local_after - local_before) / 2)
}

/// One blocking measurement of the offset against `url`.
fn probe_clock_offset(url: &str) -> Option<i64> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(10))
        .build();
    let before = now_unix();
    let response = match agent.head(url).call() {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(e) => {
            eprintln!("[Bots] Couldn't read the server time from {url}: {e}");
            return None;
        }
    };
    let after = now_unix();
    let server = parse_http_date(response.header("date")?)?;
    Some(clock_offset(server, before, after))
}

/// Measure the PC clock's offset against the chat service at `url` and use
/// it from now on. Keeps the previous offset if the measurement fails. Runs
/// on its own short-lived thread, so a bot that is told to stop meanwhile
/// isn't held up by it (the request has its own timeout).
pub async fn measure_server_clock(url: &'static str) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("clock-probe".into())
        .spawn(move || {
            let _ = tx.send(probe_clock_offset(url));
        });
    if spawned.is_err() {
        return;
    }
    if let Ok(Some(offset)) = rx.await {
        let previous = SERVER_CLOCK_OFFSET.swap(offset, Ordering::SeqCst);
        if offset.abs() > 60 && (offset - previous).abs() > 5 {
            eprintln!("[Bots] The PC clock is {}s off the chat service's - correcting for it", -offset);
        }
    }
}

/// How old (in seconds) a message sent at `sent_unix` by the server's clock
/// is, if it is too old to act on - `None` if it is fresh. A message that
/// looks stale triggers a fresh clock measurement first (at most once a
/// minute), in case the PC clock was changed since the bot connected.
pub async fn stale_age(sent_unix: i64, time_url: &'static str) -> Option<i64> {
    if !is_stale(sent_unix, server_now()) {
        return None;
    }
    let remeasure = LAST_STALE_REMEASURE
        .lock()
        .map(|mut last| {
            let due = last.is_none_or(|t| t.elapsed() >= STALE_REMEASURE_INTERVAL);
            if due {
                *last = Some(Instant::now());
            }
            due
        })
        .unwrap_or(false);
    if remeasure {
        measure_server_clock(time_url).await;
    }
    let now = server_now();
    is_stale(sent_unix, now).then(|| now.saturating_sub(sent_unix))
}

/// Allows something at most once per `interval` - e.g. the "ignored, too
/// old" note, so a burst of queued messages gets one reply, not one each.
pub struct RateLimit {
    last: Mutex<Option<Instant>>,
    interval: Duration,
}

impl RateLimit {
    pub const fn new(interval: Duration) -> Self {
        Self { last: Mutex::new(None), interval }
    }

    /// True (and counts as used) if the interval has passed since the last
    /// time this returned true.
    pub fn allow(&self) -> bool {
        self.allow_at(Instant::now())
    }

    fn allow_at(&self, now: Instant) -> bool {
        let Ok(mut last) = self.last.lock() else { return false };
        if last.is_some_and(|t| now.saturating_duration_since(t) < self.interval) {
            return false;
        }
        *last = Some(now);
        true
    }
}

/// How often at most a bot replies that it ignored a stale message.
pub const STALE_NOTE_INTERVAL: Duration = Duration::from_secs(60);

/// Reply to a message that was ignored for being too old (`age_secs` by the
/// server's clock).
pub fn stale_note(age_secs: i64) -> String {
    let age = i32::try_from(age_secs).unwrap_or(i32::MAX);
    format!(
        "⏳ {}",
        join_words(&[i18n::t("bot.stale.prefix"), &format_hm(age), i18n::t("bot.stale.suffix")])
    )
}

// ============================================================================
// Splitting long replies to fit the chat services' message size limits
// ============================================================================

/// Longest message Telegram accepts (characters after entity parsing).
pub const TELEGRAM_MAX_MESSAGE: usize = 4096;
/// Longest message Discord accepts (characters).
pub const DISCORD_MAX_MESSAGE: usize = 2000;

/// How a reply is formatted, for `split_message`: where code blocks start
/// and end, and which characters must not be cut apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatMarkup {
    Plain,
    /// Telegram `ParseMode::Html` with `<pre>` blocks (see `WeeklyMarkup`).
    TelegramHtml,
    /// Discord markdown with ``` code blocks.
    DiscordMarkdown,
}

impl ChatMarkup {
    fn block_open(self) -> &'static str {
        match self {
            ChatMarkup::Plain => "",
            ChatMarkup::TelegramHtml => "<pre>",
            ChatMarkup::DiscordMarkdown => "```\n",
        }
    }

    /// Text that closes an open code block after `piece`.
    fn block_close(self, piece: &str) -> &'static str {
        match self {
            ChatMarkup::Plain => "",
            ChatMarkup::TelegramHtml => "</pre>",
            ChatMarkup::DiscordMarkdown if piece.ends_with('\n') => "```",
            ChatMarkup::DiscordMarkdown => "\n```",
        }
    }

    /// Whether a code block is open after `piece`, given whether one was
    /// open before it.
    fn open_after(self, piece: &str, open_before: bool) -> bool {
        match self {
            ChatMarkup::Plain => false,
            ChatMarkup::TelegramHtml => match (piece.rfind("<pre>"), piece.rfind("</pre>")) {
                (None, None) => open_before,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some(open), Some(close)) => open > close,
            },
            ChatMarkup::DiscordMarkdown => open_before ^ (piece.matches("```").count() % 2 == 1),
        }
    }
}

/// Length as both services may count it: UTF-16 code units, which is never
/// less than the number of characters.
fn chat_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Byte index at which to end the first part of `s` (which is longer than
/// `max`): at most `max` long, not inside an HTML tag or entity or a run of
/// backticks, preferably just after whitespace. Always > 0.
fn cut_point(s: &str, max: usize, markup: ChatMarkup) -> usize {
    let (mut used, mut in_tag, mut in_entity) = (0, false, false);
    let (mut last_fit, mut last_safe, mut last_space) = (0, 0, 0);
    let mut prev: Option<char> = None;
    for (i, c) in s.char_indices() {
        if i > 0 {
            last_fit = i;
            let splits_backticks = markup == ChatMarkup::DiscordMarkdown && prev == Some('`') && c == '`';
            if !in_tag && !in_entity && !splits_backticks {
                last_safe = i;
                if prev.is_some_and(char::is_whitespace) {
                    last_space = i;
                }
            }
        }
        used += c.len_utf16();
        if used > max {
            break;
        }
        if markup == ChatMarkup::TelegramHtml {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                '&' => in_entity = true,
                ';' => in_entity = false,
                _ => {}
            }
        }
        prev = Some(c);
    }
    let cut = if last_space > 0 && last_space * 4 >= last_safe * 3 {
        last_space
    } else if last_safe > 0 {
        last_safe
    } else {
        last_fit
    };
    if cut > 0 {
        cut
    } else {
        s.chars().next().map_or(s.len(), char::len_utf8)
    }
}

/// Split `text` into messages of at most `limit` (as `chat_len` counts it),
/// so a long reply (e.g. `/weekly` with many accounts) goes out as several
/// messages instead of being refused. Splits between lines where it can,
/// keeping a code block together with the line just before it (its
/// heading); a code block that has to be split is closed at the end of one
/// message and reopened in the next, so every message is valid markup on
/// its own. A text that fits is returned unchanged.
pub fn split_message(text: &str, limit: usize, markup: ChatMarkup) -> Vec<String> {
    if chat_len(text) <= limit {
        return vec![text.to_string()];
    }
    // Room for reopening and closing a block around any one piece.
    let piece_max = limit.saturating_sub(16).max(1);

    struct Piece {
        text: String,
        open_before: bool,
        open_after: bool,
    }
    let mut pieces = Vec::new();
    let mut open = false;
    for line in text.split_inclusive('\n') {
        let mut rest = line;
        while !rest.is_empty() {
            let cut = if chat_len(rest) > piece_max { cut_point(rest, piece_max, markup) } else { rest.len() };
            let (part, tail) = rest.split_at(cut);
            let open_after = markup.open_after(part, open);
            pieces.push(Piece { text: part.to_string(), open_before: open, open_after });
            open = open_after;
            rest = tail;
        }
    }

    let close_after = |p: &Piece| if p.open_after { markup.block_close(&p.text) } else { "" };
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < pieces.len() {
        let open_text = if pieces[start].open_before { markup.block_open() } else { "" };
        let mut used = chat_len(open_text);
        let mut end = start;
        // Last place to end this message that is outside any code block
        // and doesn't separate a block from the line before it.
        let mut good_end = None;
        while end < pieces.len() {
            let piece = &pieces[end];
            let len = chat_len(&piece.text);
            if end > start && used + len + chat_len(close_after(piece)) > limit {
                break;
            }
            used += len;
            end += 1;
            let next_opens_block = pieces.get(end).is_some_and(|n| !n.open_before && n.open_after);
            if !piece.open_after && !next_opens_block {
                good_end = Some(end);
            }
        }
        if end < pieces.len() {
            end = good_end.unwrap_or(end);
        }
        let mut chunk = open_text.to_string();
        for piece in &pieces[start..end] {
            chunk.push_str(&piece.text);
        }
        chunk.push_str(close_after(&pieces[end - 1]));
        let chunk = chunk.trim_end_matches('\n');
        if !chunk.trim().is_empty() {
            chunks.push(chunk.to_string());
        }
        start = end;
    }
    chunks
}

/// What `BotLifecycle::request_start` decided.
#[derive(Debug, PartialEq, Eq)]
pub enum StartAction {
    /// Nothing is running: the caller should spawn a bot thread.
    Spawn,
    /// A bot is already up (or starting) and wasn't asked to stop.
    AlreadyRunning,
    /// A bot is still tearing down after a stop. The bot thread will start
    /// a fresh generation itself once that teardown finishes.
    Queued,
}

#[derive(Default)]
struct LifecycleState {
    /// A bot thread exists and hasn't yet called `finish_generation` for the
    /// last time.
    running: bool,
    /// A start arrived while the running generation was shutting down.
    restart_queued: bool,
}

/// Start/stop bookkeeping for one bot (Telegram or Discord).
///
/// Starts and stops come from session.rs's edge-triggered
/// `sync_remote_bots`, which fires exactly once per change of console
/// ownership. Teardown of a bot generation takes a moment (a last HTTP
/// round trip, closing the gateway), so a quick switch away and back used to
/// land the "start" while the old generation was still running: start saw
/// it running and did nothing, the old generation then exited, and no
/// further edge ever came - leaving the session with no bot. Here such a
/// start is queued instead, and the bot thread runs it as soon as the old
/// generation has fully finished. Because the restart happens on the same
/// thread, two generations still never overlap (see BOT_THREAD in each bot
/// module for why that matters).
///
/// The shutdown flag that the bot's async tasks poll is only ever written
/// under the state lock, so a stop can't be lost between "restart decided"
/// and "flag cleared".
pub struct BotLifecycle {
    state: Mutex<LifecycleState>,
    shutdown: AtomicBool,
}

impl BotLifecycle {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(LifecycleState { running: false, restart_queued: false }),
            shutdown: AtomicBool::new(false),
        }
    }

    fn state(&self) -> MutexGuard<'_, LifecycleState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether the current generation has been told to stop.
    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    pub fn request_start(&self) -> StartAction {
        let mut state = self.state();
        if !state.running {
            state.running = true;
            self.shutdown.store(false, Ordering::SeqCst);
            StartAction::Spawn
        } else if self.shutdown.load(Ordering::SeqCst) {
            state.restart_queued = true;
            StartAction::Queued
        } else {
            StartAction::AlreadyRunning
        }
    }

    /// After `Spawn`, the caller found nothing to run (bot disabled, no
    /// token, ...) and didn't spawn a thread.
    pub fn cancel_start(&self) {
        let mut state = self.state();
        state.running = false;
        state.restart_queued = false;
    }

    /// Stop the running generation, if any, and forget any queued restart.
    pub fn request_stop(&self) {
        let mut state = self.state();
        state.restart_queued = false;
        if state.running {
            self.shutdown.store(true, Ordering::SeqCst);
        }
    }

    /// App exit: like `request_stop`, but raises the flag even if nothing
    /// is running yet, so a generation that's about to start sees it.
    pub fn request_shutdown(&self) {
        let mut state = self.state();
        state.restart_queued = false;
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Called by the bot thread when a generation has ended and its runtime
    /// is gone. Returns true if a start was queued meanwhile: the thread
    /// should then run a new generation (the shutdown flag is already
    /// cleared). Returns false when the thread should exit; it no longer
    /// counts as running from that point.
    pub fn finish_generation(&self) -> bool {
        let mut state = self.state();
        if state.restart_queued {
            state.restart_queued = false;
            self.shutdown.store(false, Ordering::SeqCst);
            true
        } else {
            state.running = false;
            false
        }
    }

    /// Resolves once the current generation has been told to stop.
    pub async fn wait_for_shutdown(&self) {
        while !self.shutdown_requested() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Sleep for `delay`, cut short by a stop. Returns false if stopped.
    pub async fn sleep_unless_shutdown(&self, delay: Duration) -> bool {
        tokio::select! {
            () = tokio::time::sleep(delay) => true,
            () = self.wait_for_shutdown() => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_backs_off_to_a_minute() {
        let secs: Vec<u64> = (1..=8).map(|n| retry_delay(n).as_secs()).collect();
        assert_eq!(secs, [2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(retry_delay(0).as_secs(), 2);
        assert_eq!(retry_delay(u32::MAX).as_secs(), 60);
    }

    #[test]
    fn staleness_window() {
        let now = 1_800_000_000;
        assert!(!is_stale(now, now));
        assert!(!is_stale(now - STALE_COMMAND_SECS, now), "exactly at the limit still runs");
        assert!(is_stale(now - STALE_COMMAND_SECS - 1, now));
        assert!(is_stale(now - 3 * 3600, now), "queued for hours while the PC slept");
        assert!(!is_stale(now + 600, now), "PC clock behind the server's");
        assert!(!is_stale(now, 0), "PC clock before 1970");
        assert!(!is_stale(i64::MIN, i64::MIN));
        assert!(is_stale(i64::MIN, i64::MAX), "no overflow");
    }

    #[test]
    fn http_dates_parse() {
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
        assert_eq!(parse_http_date("Tue, 29 Feb 2028 23:59:59 GMT"), Some(1_835_481_599));
        assert_eq!(parse_http_date("Sun, 15 Nov 2026 12:00:00 GMT"), Some(1_794_744_000));
        assert_eq!(parse_http_date("Sun, 15 Nov 2026 12:00:00 CET"), None);
        assert_eq!(parse_http_date("Sun, 15 Foo 2026 12:00:00 GMT"), None);
        assert_eq!(parse_http_date("Sun, 15 Nov 2026 25:00:00 GMT"), None);
        assert_eq!(parse_http_date("Sun, 15 Nov 2026 12:00 GMT"), None);
        assert_eq!(parse_http_date("Sun, 15 Nov 2026 12:00:00:00 GMT"), None);
        assert_eq!(parse_http_date("Sun, 15 Nov 2026 12:00:00 GMT extra"), None);
        assert_eq!(parse_http_date(""), None);
    }

    /// A PC clock 10 minutes fast: measured against the server, a message
    /// sent a moment ago is fresh, and one sent 6 minutes ago is not.
    #[test]
    fn fast_pc_clock_is_corrected_for() {
        let server = 1_800_000_000;
        let pc = server + 600;
        let offset = clock_offset(server, pc - 1, pc + 1);
        assert_eq!(offset, -600);
        let corrected_now = pc + offset;
        assert!(is_stale(server - 10, pc), "by the PC clock alone everything looks stale");
        assert!(!is_stale(server - 10, corrected_now));
        assert!(is_stale(server - 360, corrected_now));
    }

    #[test]
    fn rate_limit_allows_once_per_interval() {
        let limit = RateLimit::new(Duration::from_secs(60));
        let t = Instant::now();
        assert!(limit.allow_at(t));
        assert!(!limit.allow_at(t + Duration::from_secs(1)));
        assert!(!limit.allow_at(t + Duration::from_secs(59)));
        assert!(limit.allow_at(t + Duration::from_secs(60)));
        assert!(!limit.allow_at(t + Duration::from_secs(61)));
    }

    #[test]
    fn short_replies_are_not_split() {
        for markup in [ChatMarkup::Plain, ChatMarkup::TelegramHtml, ChatMarkup::DiscordMarkdown] {
            assert_eq!(split_message("hello\nworld\n", 100, markup), ["hello\nworld\n"]);
        }
    }

    /// A `/weekly`-shaped reply for `users` accounts.
    fn weekly_like(users: usize, markup: WeeklyMarkup) -> String {
        let history: Vec<(String, i32, i32)> =
            (1..=14).map(|d| (format!("2026-11-{d:02}"), 60 + d, d)).collect();
        let body = (0..users)
            .map(|i| weekly_table_for(&format!("child_{i}<&>"), &history, markup))
            .collect::<Vec<_>>()
            .join("\n");
        format!("📊 2-Week Usage\n{body}")
    }

    fn assert_balanced(chunk: &str, open: &str, close: &str) {
        assert_eq!(chunk.matches(open).count(), chunk.matches(close).count(), "{chunk}");
    }

    #[test]
    fn weekly_for_many_accounts_splits_between_tables_on_telegram() {
        let text = weekly_like(15, WeeklyMarkup::TelegramHtml);
        assert!(chat_len(&text) > TELEGRAM_MAX_MESSAGE);
        let parts = split_message(&text, TELEGRAM_MAX_MESSAGE, ChatMarkup::TelegramHtml);
        assert!(parts.len() >= 2, "{}", parts.len());
        for part in &parts {
            assert!(chat_len(part) <= TELEGRAM_MAX_MESSAGE);
            assert_balanced(part, "<pre>", "</pre>");
            // Each message ends after a whole table, and every table keeps
            // its heading.
            assert!(part.ends_with("</pre>"), "{part}");
            assert_eq!(part.matches("👤").count(), part.matches("<pre>").count(), "{part}");
        }
        assert_eq!(parts.concat().matches("<pre>").count(), 15);
        assert_eq!(parts.concat().matches("child_14&lt;&amp;&gt;").count(), 1);
    }

    #[test]
    fn weekly_for_many_accounts_splits_between_tables_on_discord() {
        let text = weekly_like(9, WeeklyMarkup::DiscordMarkdown);
        assert!(chat_len(&text) > DISCORD_MAX_MESSAGE);
        let parts = split_message(&text, DISCORD_MAX_MESSAGE, ChatMarkup::DiscordMarkdown);
        assert!(parts.len() >= 2);
        for part in &parts {
            assert!(chat_len(part) <= DISCORD_MAX_MESSAGE);
            assert_eq!(part.matches("```").count() % 2, 0, "{part}");
            assert!(part.ends_with("```"), "{part}");
        }
        assert_eq!(parts.concat().matches("👤").count(), 9);
    }

    #[test]
    fn a_code_block_too_long_for_one_message_is_closed_and_reopened() {
        let table: String = (0..300).map(|i| format!("row {i:03} &amp; more\n")).collect();
        let text = format!("head\n<pre>{table}</pre>\ntail");
        let parts = split_message(&text, 1000, ChatMarkup::TelegramHtml);
        assert!(parts.len() >= 6);
        for part in &parts {
            assert!(chat_len(part) <= 1000);
            assert_balanced(part, "<pre>", "</pre>");
        }
        let rows: usize = parts.iter().map(|p| p.matches("row ").count()).sum();
        assert_eq!(rows, 300);

        let table: String = (0..300).map(|i| format!("row {i:03}\n")).collect();
        let text = format!("head\n```\n{table}```\ntail");
        let parts = split_message(&text, 500, ChatMarkup::DiscordMarkdown);
        for part in &parts {
            assert!(chat_len(part) <= 500);
            assert_eq!(part.matches("```").count() % 2, 0, "{part}");
        }
        assert_eq!(parts.iter().map(|p| p.matches("row ").count()).sum::<usize>(), 300);
    }

    #[test]
    fn an_overlong_line_is_cut_without_breaking_entities_or_words() {
        let line = "word &amp; ".repeat(100);
        let parts = split_message(&line, 120, ChatMarkup::TelegramHtml);
        assert!(parts.len() > 1);
        for part in &parts {
            assert!(chat_len(part) <= 120);
            assert_eq!(part.matches('&').count(), part.matches("&amp;").count(), "{part}");
        }
        assert_eq!(parts.concat().replace(' ', ""), line.replace(' ', ""));

        let plain = "x".repeat(5000);
        let parts = split_message(&plain, TELEGRAM_MAX_MESSAGE, ChatMarkup::Plain);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts.concat(), plain);

        // Emoji count double (UTF-16), so this can't overshoot either way
        // the service counts.
        let emoji = "📊".repeat(1500);
        for part in split_message(&emoji, DISCORD_MAX_MESSAGE, ChatMarkup::DiscordMarkdown) {
            assert!(chat_len(&part) <= DISCORD_MAX_MESSAGE);
        }
    }

    #[test]
    fn html_escaping() {
        assert_eq!(escape_html("2-Week Usage"), "2-Week Usage");
        assert_eq!(escape_html("a<b>&c"), "a&lt;b&gt;&amp;c");
        assert_eq!(escape_html("anna.b_x-1"), "anna.b_x-1");
    }

    #[test]
    fn discord_markdown_escaping() {
        assert_eq!(escape_discord_markdown("max_the_great"), "max\\_the\\_great");
        assert_eq!(escape_discord_markdown("a*b~c`d|e"), "a\\*b\\~c\\`d\\|e");
        assert_eq!(escape_discord_markdown("anna.b-1"), "anna.b-1");
    }

    #[test]
    fn child_text_is_posted_literally() {
        assert_eq!(discord_literal("saving my game"), "`saving my game`");
        // Mentions, markdown, masked links and headings all sit inside one
        // code span, which Discord shows as typed.
        let nasty = "@everyone <@&123> **hi** [free](https://x.example) # big";
        assert_eq!(discord_literal(nasty), format!("`{nasty}`"));
        // A backtick can't end the span early; line breaks can't start a
        // heading or quote on a new line.
        assert_eq!(discord_literal("a`b``c"), "`a'b''c`");
        assert_eq!(discord_literal("one\n> two\r\n# three"), "`one > two  # three`");
        assert_eq!(discord_literal("  padded  "), "`padded`");
        assert_eq!(discord_literal(" ` "), "`'`");
        assert_eq!(discord_literal("\n"), "");
        assert!(!discord_literal("x`y`z").trim_matches('`').contains('`'));

        assert_eq!(telegram_literal("hi\nthere <b>"), "hi there <b>");
    }

    #[test]
    fn commands_split_into_name_and_trimmed_arguments() {
        assert_eq!(split_command("extend 30"), ("extend", "30"));
        assert_eq!(split_command("extend  30 "), ("extend", "30"));
        assert_eq!(split_command("extend"), ("extend", ""));
        assert_eq!(split_command("msg\nDinner!"), ("msg", "Dinner!"));
        assert_eq!(split_command("setlimit saturday 180"), ("setlimit", "saturday 180"));
        assert_eq!(split_command(""), ("", ""));
    }

    #[test]
    fn minutes_arguments() {
        assert_eq!(minutes_arg("30"), Some(30));
        assert_eq!(minutes_arg(" 30 "), Some(30));
        assert_eq!(minutes_arg("-5"), Some(-5), "the command explains the range");
        assert_eq!(minutes_arg("500"), Some(500));
        assert_eq!(minutes_arg(""), None);
        assert_eq!(minutes_arg("30min"), None);
        assert_eq!(minutes_arg("thirty"), None);
        assert_eq!(minutes_arg("99999999999"), None);
    }

    #[test]
    fn replies_that_name_the_account() {
        for name in [
            "time", "extend", "e30", "e60", "e120", "reduce", "reset", "lock", "stop", "unlock", "pause",
            "resume", "setlimit", "setmessage",
        ] {
            assert!(reply_names_account(name), "{name}");
        }
        // status/history have it already; the rest aren't per account.
        for name in ["status", "history", "weekly", "msg", "setpin", "getpin", "update", "help", "Extend"] {
            assert!(!reply_names_account(name), "{name}");
        }
    }

    #[test]
    fn timer_state_reports_what_the_countdown_is_doing() {
        use TimerState::*;
        let state = |blocked, manual, remaining, paused, idle, locked| {
            timer_state(blocked, manual, remaining, paused, idle, locked)
        };
        assert_eq!(state(false, None, 600, false, false, false), Running);
        assert_eq!(state(false, None, 600, true, false, false), Paused);
        assert_eq!(state(false, None, 600, false, true, false), Idle);
        assert_eq!(state(false, None, 600, false, false, true), WindowsLocked);
        // A manual pause wins over idle/locked, as in `tick_mode`.
        assert_eq!(state(false, None, 600, true, true, true), Paused);
        // The app's lock screen freezes everything.
        assert_eq!(state(true, None, 0, true, true, true), OutOfTime);
        assert_eq!(state(true, None, 600, false, false, false), Locked, "time left: locked by hand");
        // A known manual-lock flag decides, whatever the time left.
        assert_eq!(state(true, Some(true), 0, false, false, false), Locked);
        assert_eq!(state(true, Some(false), 600, false, false, false), OutOfTime);
        assert_eq!(state(false, Some(true), 600, false, false, false), Running);
        // Each state has its own text.
        let keys: std::collections::HashSet<_> =
            [Running, Paused, Idle, WindowsLocked, OutOfTime, Locked].map(timer_state_key).into_iter().collect();
        assert_eq!(keys.len(), 6);
    }

    #[test]
    fn weekly_table_markup() {
        let history = vec![("2026-11-14".to_string(), 95, 10)];

        let tg = weekly_table_for("anna.b_<x>", &history, WeeklyMarkup::TelegramHtml);
        assert!(tg.starts_with("👤 anna.b_&lt;x&gt;\n<pre>"), "{tg}");
        assert!(tg.ends_with("</pre>"), "{tg}");
        assert!(tg.contains("11-14"), "{tg}");
        assert!(tg.contains("1h 35m"), "{tg}");
        assert!(!tg.contains("```"), "{tg}");

        let dc = weekly_table_for("max_k", &history, WeeklyMarkup::DiscordMarkdown);
        assert!(dc.starts_with("👤 max\\_k\n```\n"), "{dc}");
        assert!(dc.ends_with("```"), "{dc}");
        assert!(dc.contains("1h 35m"), "{dc}");
    }

    #[test]
    fn lifecycle_plain_start_and_stop() {
        let lc = BotLifecycle::new();
        assert_eq!(lc.request_start(), StartAction::Spawn);
        assert!(!lc.shutdown_requested());
        assert_eq!(lc.request_start(), StartAction::AlreadyRunning);

        lc.request_stop();
        assert!(lc.shutdown_requested());
        assert!(!lc.finish_generation(), "nothing queued: thread exits");

        assert_eq!(lc.request_start(), StartAction::Spawn, "can start again afterwards");
        assert!(!lc.shutdown_requested(), "flag cleared for the new generation");
    }

    #[test]
    fn lifecycle_start_during_teardown_is_queued_not_lost() {
        let lc = BotLifecycle::new();
        assert_eq!(lc.request_start(), StartAction::Spawn);
        lc.request_stop(); // switched away...
        assert_eq!(lc.request_start(), StartAction::Queued); // ...and back, mid-teardown

        assert!(lc.finish_generation(), "old generation done: run the queued one");
        assert!(!lc.shutdown_requested(), "new generation must not see the old stop");
        assert_eq!(lc.request_start(), StartAction::AlreadyRunning);

        lc.request_stop();
        assert!(!lc.finish_generation());
    }

    #[test]
    fn lifecycle_stop_cancels_queued_restart() {
        let lc = BotLifecycle::new();
        lc.request_start();
        lc.request_stop();
        assert_eq!(lc.request_start(), StartAction::Queued);
        lc.request_stop(); // switched away again before teardown finished
        assert!(lc.shutdown_requested());
        assert!(!lc.finish_generation(), "the queued start was withdrawn");
    }

    #[test]
    fn lifecycle_stop_after_restart_decision_is_kept() {
        let lc = BotLifecycle::new();
        lc.request_start();
        lc.request_stop();
        lc.request_start();
        assert!(lc.finish_generation());
        lc.request_stop(); // arrives while the new generation is starting up
        assert!(lc.shutdown_requested());
        assert!(!lc.finish_generation());
    }

    #[test]
    fn lifecycle_app_shutdown_and_cancelled_start() {
        let lc = BotLifecycle::new();
        lc.request_shutdown();
        assert!(lc.shutdown_requested(), "raised even with nothing running");

        assert_eq!(lc.request_start(), StartAction::Spawn);
        lc.cancel_start(); // e.g. bot disabled in settings
        assert_eq!(lc.request_start(), StartAction::Spawn);

        lc.request_stop();
        lc.request_start();
        lc.request_shutdown();
        assert!(!lc.finish_generation(), "app exit withdraws a queued restart");
    }

    #[test]
    fn lifecycle_stop_when_idle_is_a_no_op() {
        let lc = BotLifecycle::new();
        lc.request_stop();
        assert!(!lc.shutdown_requested());
        assert_eq!(lc.request_start(), StartAction::Spawn);
    }
}
