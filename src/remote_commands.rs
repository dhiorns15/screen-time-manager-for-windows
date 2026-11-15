//! Platform-agnostic remote command implementations
//! Shared by both the Telegram and Discord bot integrations

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

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

pub fn cmd_status() -> String {
    let remaining = blocking::get_remaining_seconds();
    let paused = mini_overlay::is_paused();
    let idle_paused = mini_overlay::is_idle_paused();
    let pause_budget = mini_overlay::get_remaining_pause_budget();

    let status_emoji = if remaining <= 60 {
        "🔴"
    } else if remaining <= 300 {
        "🟠"
    } else {
        "🟢"
    };

    let pause_status = if paused {
        i18n::t("tg.status.yes")
    } else if idle_paused {
        i18n::t("tg.status.idle")
    } else {
        i18n::t("tg.status.no")
    };

    format!(
        "{}\n\
         ━━━━━━━━━━━━━━━━━━\n\
         👤 {}: {}\n\
         {} {}: {}\n\
         ⏸ {}: {}\n\
         🔋 {}: {}",
        i18n::t("tg.status.header"),
        i18n::t("tg.status.user"),
        current_windows_username(),
        status_emoji,
        i18n::t("tg.status.remaining"),
        format_hm(remaining),
        i18n::t("tg.status.paused"),
        pause_status,
        i18n::t("tg.status.pause_budget"),
        format_hm(pause_budget)
    )
}

pub fn cmd_time() -> String {
    let remaining = blocking::get_remaining_seconds();

    let emoji = if remaining <= 60 {
        "🔴"
    } else if remaining <= 300 {
        "🟠"
    } else {
        "🟢"
    };

    format!("{} {} remaining", emoji, format_hm(remaining))
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

    // If today's limit was just lowered, cap remaining time to match -
    // mirrors the same adjustment the Settings dialog makes on Save.
    if weekday == database::get_current_weekday() {
        blocking::cap_remaining_time((minutes * 60) as i32);
    }

    format!(
        "📅 {} {} {} {}",
        i18n::t("tg.setlimit.success"),
        i18n::weekday(weekday as usize),
        i18n::t("tg.setlimit.to"),
        format_hm((minutes * 60) as i32)
    )
}

pub fn cmd_reset() -> String {
    let weekday = database::get_current_weekday();
    let daily_limit_minutes = database::get_daily_limit(weekday);
    let daily_limit_seconds = (daily_limit_minutes * 60) as i32;

    blocking::REMAINING_SECONDS.store(daily_limit_seconds, Ordering::SeqCst);
    database::save_remaining_time(daily_limit_seconds);

    unsafe {
        mini_overlay::update_mini_overlay();
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

    unsafe {
        blocking::show_blocking_overlay(&message);
    }

    format!("🔒 {}", i18n::t("tg.lock.success"))
}

/// Dismiss the blocking overlay without granting or resetting any time -
/// for undoing a manual lock while time is still left. Refuses when time has
/// actually run out, since dismissing with 0 remaining would leave the
/// countdown timer stopped with nothing left to re-trigger the lock screen.
pub fn cmd_unlock() -> String {
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
/// than run. Telegram keeps undelivered messages for up to a day, so without
/// this a "/e60" sent while the PC was asleep or offline would run whenever
/// it next connected - for whichever account happened to be logged in then.
///
/// Measured against the PC's own clock. A child moving the clock forward
/// could make fresh commands look stale, but that gains nothing over simply
/// unplugging the network, which already blocks every remote command.
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
