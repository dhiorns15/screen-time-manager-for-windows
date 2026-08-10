//! Platform-agnostic remote command implementations
//! Shared by both the Telegram and Discord bot integrations

use std::sync::atomic::Ordering;

use crate::blocking;
use crate::database;
use crate::i18n;
use crate::mini_overlay;
use crate::overlay;

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

/// 14-day usage table (minutes active / paused per day) for one user, sent
/// as a monospace code block so it lines up as an actual table in
/// Telegram/Discord.
fn weekly_table_for(username: &str, history: &[(String, i32, i32)]) -> String {
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
    format!("👤 {}\n```\n{}```", username, table)
}

/// 14-day usage table for every Windows account that's used this app on
/// this machine - not just whoever happens to be running the command -
/// built from the shared database's mirrored per-user stats (see
/// `database::mirror_shared_daily_stat`). Falls back to this account's own
/// local history alone if the shared registry is empty (e.g. the shared
/// database is unavailable, or this is the first run since the per-user
/// mirroring was added and no shared history has accumulated yet).
pub fn cmd_weekly() -> String {
    let all_users = database::get_all_users_daily_usage_history(14);

    let body = if all_users.is_empty() {
        weekly_table_for(&current_windows_username(), &database::get_daily_usage_history(14))
    } else {
        all_users
            .iter()
            .map(|(username, history)| weekly_table_for(username, history))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!("📊 {}\n{}", i18n::t("tg.weekly.header"), body)
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
        let new_limit_seconds = (minutes * 60) as i32;
        let remaining = blocking::get_remaining_seconds();
        if remaining > new_limit_seconds {
            blocking::REMAINING_SECONDS.store(new_limit_seconds, Ordering::SeqCst);
            database::save_remaining_time(new_limit_seconds);
            unsafe {
                mini_overlay::update_mini_overlay();
            }
        }
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
