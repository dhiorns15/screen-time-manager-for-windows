//! Cross-platform "requesting more time" state, and other lock-screen /
//! passcode notifications pushed out to the configured bots.
//!
//! Tracks a single pending request so that whichever bot (Telegram or
//! Discord) the parent replies from first resolves it, and the other one
//! gets told it's already been handled instead of staying silent - see the
//! "advisory" design: nothing blocks a second reply, but the second parent
//! checking the other app knows not to bother.

use std::sync::Mutex;

use crate::database;
use crate::discord;
use crate::i18n;
use crate::remote_commands::{current_windows_username, format_hm, join_words};
use crate::telegram;

static PENDING: Mutex<bool> = Mutex::new(false);

/// Called when the kid clicks "Request More Time" on the lock screen.
/// Sends a notification to every enabled bot.
pub fn request_more_time(note: Option<String>, remaining_seconds: i32) {
    *PENDING.lock().unwrap() = true;

    let username = current_windows_username();

    let mut body = format!(
        "⏰ {} {}\n⏳ {}: {}",
        username,
        i18n::t("request.notify.header"),
        i18n::t("tg.status.remaining"),
        format_hm(remaining_seconds),
    );
    if let Some(note) = note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        body.push_str(&format!("\n💬 {}", note));
    }

    let tg = database::get_telegram_config();
    let dc = database::get_discord_config();

    if tg.enabled {
        telegram::notify_admin(&format!("{}\n\n{}", body, i18n::t("request.notify.reply_tg")));
    }
    if dc.enabled {
        discord::notify_admin(&format!("{}\n\n{}", body, i18n::t("request.notify.reply_dc")));
    }
}

/// Called after a grant-worthy command (extend/reset/unlock) succeeds. If a
/// request was pending, clears it and lets the *other* platform(s) know it's
/// already been handled, so a second reply there isn't necessary.
pub fn resolve_if_pending(granted_via: &str, detail: &str) {
    let mut pending = PENDING.lock().unwrap();
    if !*pending {
        return;
    }
    *pending = false;
    drop(pending);

    let text = format!("✅ {} {} ({})", i18n::t("request.notify.resolved"), detail, granted_via);

    let tg = database::get_telegram_config();
    let dc = database::get_discord_config();

    if granted_via != "Telegram" && tg.enabled {
        telegram::notify_admin(&text);
    }
    if granted_via != "Discord" && dc.enabled {
        discord::notify_admin(&text);
    }
}

/// Called whenever the passcode is used locally (lock screen or tray menu)
/// to add time, so the parent finds out even if they weren't the one who
/// entered it - the passcode being used is worth knowing about either way.
pub fn notify_passcode_extend(source_key: &str, minutes: i32, remaining_seconds: i32) {
    let username = current_windows_username();

    // German needs a trailing infinitive ("...um 30m hinzuzufügen") to
    // complete the "um...zu" clause started by the header; English has
    // nothing to add after the amount, so that fragment is empty for it.
    let amount = join_words(&[&format_hm(minutes * 60), i18n::t("passcode_extend.notify.minutes")]);

    let text = format!(
        "🔓 {} {} {} ({})\n⏳ {}: {}",
        username,
        i18n::t("passcode_extend.notify.header"),
        amount,
        i18n::t(source_key),
        i18n::t("tg.status.remaining"),
        format_hm(remaining_seconds),
    );

    let tg = database::get_telegram_config();
    let dc = database::get_discord_config();

    if tg.enabled {
        telegram::notify_admin(&text);
    }
    if dc.enabled {
        discord::notify_admin(&text);
    }
}

/// Called whenever idle detection flips the timer between paused and running
/// (see `mini_overlay::check_idle_state`), so the parent can see activity
/// without having to poll the `status` bot command themselves.
pub fn notify_activity_status(is_idle: bool, remaining_seconds: i32) {
    let username = current_windows_username();
    let (icon, header_key) = if is_idle {
        ("💤", "activity.notify.idle")
    } else {
        ("▶️", "activity.notify.active")
    };

    let text = format!(
        "{} {} {}\n⏳ {}: {}",
        icon,
        username,
        i18n::t(header_key),
        i18n::t("tg.status.remaining"),
        format_hm(remaining_seconds),
    );

    let tg = database::get_telegram_config();
    let dc = database::get_discord_config();

    if tg.enabled {
        telegram::notify_admin(&text);
    }
    if dc.enabled {
        discord::notify_admin(&text);
    }
}

/// Called when remaining time crosses one of the configured warning
/// thresholds (the same ones that trigger the kid-facing overlay), so the
/// parent gets the same heads-up without needing to be looking at `status`.
pub fn notify_low_time(minutes: u32, remaining_seconds: i32) {
    let username = current_windows_username();

    let text = format!(
        "⚠️ {} {} {} {}\n⏳ {}: {}",
        username,
        i18n::t("warning.notify.header"),
        format_hm((minutes * 60) as i32),
        i18n::t("warning.notify.minutes"),
        i18n::t("tg.status.remaining"),
        format_hm(remaining_seconds),
    );

    let tg = database::get_telegram_config();
    let dc = database::get_discord_config();

    if tg.enabled {
        telegram::notify_admin(&text);
    }
    if dc.enabled {
        discord::notify_admin(&text);
    }
}

/// Called the moment remaining time hits zero and the blocking overlay
/// triggers - the actual enforcement event, so worth a push regardless of
/// whether either warning threshold was configured/reached first.
pub fn notify_out_of_time() {
    let username = current_windows_username();
    let text = format!("⏰ {} {}", username, i18n::t("outoftime.notify.header"));

    let tg = database::get_telegram_config();
    let dc = database::get_discord_config();

    if tg.enabled {
        telegram::notify_admin(&text);
    }
    if dc.enabled {
        discord::notify_admin(&text);
    }
}

/// Called when `database::take_pending_clock_tamper_alert` reports a newly
/// detected system-clock jump - the app is continuing to enforce today's
/// limit as already tracked rather than granting a fresh allowance for
/// whatever date the clock now shows.
pub fn notify_clock_tamper(drift_secs: i64) {
    let username = current_windows_username();
    let direction = if drift_secs >= 0 {
        i18n::t("clock_tamper.notify.forward")
    } else {
        i18n::t("clock_tamper.notify.backward")
    };

    let text = format!(
        "🕒 {} {}\n{} {}\n{}",
        username,
        i18n::t("clock_tamper.notify.header"),
        direction,
        format_hm(drift_secs.unsigned_abs() as i32),
        i18n::t("clock_tamper.notify.suffix"),
    );

    let tg = database::get_telegram_config();
    let dc = database::get_discord_config();

    if tg.enabled {
        telegram::notify_admin(&text);
    }
    if dc.enabled {
        discord::notify_admin(&text);
    }
}
