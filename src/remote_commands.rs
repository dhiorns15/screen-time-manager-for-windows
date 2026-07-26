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

pub fn cmd_status() -> String {
    let remaining = blocking::get_remaining_seconds();
    let paused = mini_overlay::is_paused();
    let idle_paused = mini_overlay::is_idle_paused();
    let pause_budget = mini_overlay::get_remaining_pause_budget();

    let mins = remaining / 60;
    let secs = remaining % 60;

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
         {} {}: {}:{:02}\n\
         ⏸ {}: {}\n\
         🔋 {}: {} min",
        i18n::t("tg.status.header"),
        i18n::t("tg.status.user"),
        current_windows_username(),
        status_emoji,
        i18n::t("tg.status.remaining"),
        mins, secs,
        i18n::t("tg.status.paused"),
        pause_status,
        i18n::t("tg.status.pause_budget"),
        pause_budget / 60
    )
}

pub fn cmd_time() -> String {
    let remaining = blocking::get_remaining_seconds();
    let mins = remaining / 60;
    let secs = remaining % 60;

    let emoji = if remaining <= 60 {
        "🔴"
    } else if remaining <= 300 {
        "🟠"
    } else {
        "🟢"
    };

    format!("{} {}:{:02} remaining", emoji, mins, secs)
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
    let new_mins = remaining / 60;
    let new_secs = remaining % 60;

    format!("✅ {} {} min\n{} {}:{:02}",
        i18n::t("tg.extend.success").replace("{}", ""),
        minutes,
        i18n::t("tg.status.remaining"),
        new_mins, new_secs)
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
        return format!("{} ({}:{:02})",
            i18n::t("tg.reduce.not_enough"),
            current / 60, current % 60);
    }

    blocking::reduce_time(minutes);

    // Get new remaining time
    let remaining = blocking::get_remaining_seconds();
    let new_mins = remaining / 60;
    let new_secs = remaining % 60;

    format!("⏬ {} {} min\n{} {}:{:02}",
        i18n::t("tg.reduce.success").replace("{}", ""),
        minutes,
        i18n::t("tg.status.remaining"),
        new_mins, new_secs)
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

/// 14-day usage table (minutes active / paused per day), sent as a monospace
/// code block so it lines up as an actual table in Telegram/Discord.
pub fn cmd_weekly() -> String {
    let history = database::get_daily_usage_history(14);

    let mut table = format!("{:<6}{:>6}{:>8}\n", "Date", "Used", "Paused");
    for (date, active_min, pause_min) in &history {
        let short_date = date.get(5..).unwrap_or(date); // MM-DD
        table.push_str(&format!("{:<6}{:>5}m{:>7}m\n", short_date, active_min, pause_min));
    }

    format!(
        "📊 {}\n👤 {}: {}\n```\n{}```",
        i18n::t("tg.weekly.header"),
        i18n::t("tg.status.user"),
        current_windows_username(),
        table,
    )
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

    // Format uptime
    let hours = session_active / 3600;
    let minutes = (session_active % 3600) / 60;
    let seconds = session_active % 60;
    if hours > 0 {
        response.push_str(&format!("⏱ {} {}h {}m {}s\n", i18n::t("tg.history.uptime"), hours, minutes, seconds));
    } else {
        response.push_str(&format!("⏱ {} {}m {}s\n", i18n::t("tg.history.uptime"), minutes, seconds));
    }

    response.push_str(&format!(
        "⏸ {} {} / {} min\n\n",
        i18n::t("tg.history.pause_used"),
        pause_used / 60,
        pause_config.daily_budget_minutes
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
        "📅 {} {} {} {} min",
        i18n::t("tg.setlimit.success"),
        i18n::weekday(weekday as usize),
        i18n::t("tg.setlimit.to"),
        minutes
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
        "🔄 {} ({} min)\n{} {}:{:02}",
        i18n::t("tg.reset.success"),
        daily_limit_minutes,
        i18n::t("tg.reset.remaining"),
        daily_limit_seconds / 60,
        daily_limit_seconds % 60
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
            format!("{} ({}s)", i18n::t("pause.cooldown"), seconds_remaining)
        }
        mini_overlay::PauseBlockedReason::MinActiveTimeNotMet { seconds_remaining } => {
            format!("{} ({}s)", i18n::t("pause.min_active"), seconds_remaining)
        }
        mini_overlay::PauseBlockedReason::TimeTooLow => i18n::t("pause.time_too_low").to_string(),
    }
}
