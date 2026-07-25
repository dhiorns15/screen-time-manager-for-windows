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
use crate::remote_commands::current_windows_username;
use crate::telegram;

static PENDING: Mutex<bool> = Mutex::new(false);

/// Called when the kid clicks "Request More Time" on the lock screen.
/// Sends a notification to every enabled bot.
pub fn request_more_time(note: Option<String>, remaining_seconds: i32) {
    *PENDING.lock().unwrap() = true;

    let username = current_windows_username();
    let mins = remaining_seconds / 60;
    let secs = remaining_seconds % 60;

    let mut body = format!(
        "⏰ {} {}\n⏳ {}: {}:{:02}",
        username,
        i18n::t("request.notify.header"),
        i18n::t("tg.status.remaining"),
        mins, secs,
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
    let mins = remaining_seconds / 60;
    let secs = remaining_seconds % 60;

    let text = format!(
        "🔓 {} {} {} {} ({})\n⏳ {}: {}:{:02}",
        username,
        i18n::t("passcode_extend.notify.header"),
        minutes,
        i18n::t("passcode_extend.notify.minutes"),
        i18n::t(source_key),
        i18n::t("tg.status.remaining"),
        mins, secs,
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
