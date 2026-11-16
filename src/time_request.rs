//! Cross-platform "requesting more time" state, and other lock-screen /
//! passcode notifications pushed out to the configured bots.
//!
//! Tracks a single pending request so that whichever bot (Telegram or
//! Discord) the parent replies from first resolves it, and the other one
//! gets told it's already been handled instead of staying silent - see the
//! "advisory" design: nothing blocks a second reply, but the second parent
//! checking the other app knows not to bother.
//!
//! Two ways out: the watchdog's alert queue (`watchdog::raise_alert`), which
//! sends straight away by plain HTTP whether or not this session's bot is
//! connected and, if the PC is offline, retries for up to a day - used for
//! everything a parent needs to hear about even late; and the running bots
//! (`notify_admin`), which only reach the parent if this session's bot is
//! connected right now - used for routine notices that are only worth
//! anything the moment they happen (idle, Windows lock/unlock, low time)
//! and would just be noise a day later, and which mustn't crowd the queue's
//! 20 places.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::database;
use crate::discord;
use crate::i18n;
use crate::remote_commands::{
    current_windows_username, discord_literal, format_hm, join_words, telegram_literal,
};
use crate::telegram;
use crate::watchdog;

/// An unanswered request stops counting as pending this long after it (or
/// its latest reminder) went out - a grant after that is taken as unrelated,
/// so the other bot isn't told "already granted". It also ends at the next
/// day.
const REQUEST_EXPIRY: Duration = Duration::from_secs(60 * 60);

/// While a request is pending, pressing the button again sends a reminder
/// only if the last message went out at least this long ago. Each one takes
/// a place in the alert queue (20 at most, oldest dropped) while the PC is
/// offline; without this, pressing the button every few minutes offline
/// could push a tamper alert out of the queue before it's delivered.
const REQUEST_RESEND_INTERVAL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingRequest {
    /// When the latest message about it (first request or reminder) went
    /// out.
    last_sent: Instant,
    /// The local date it was made on.
    day: String,
    /// Which bots it was sent to.
    telegram: bool,
    discord: bool,
}

impl PendingRequest {
    fn is_live(&self, now: Instant, today: &str) -> bool {
        self.day == today && now.saturating_duration_since(self.last_sent) < REQUEST_EXPIRY
    }
}

static PENDING: Mutex<Option<PendingRequest>> = Mutex::new(None);

/// What became of a press of the lock screen's "Request More Time" button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestOutcome {
    /// Handed to the alert queue for every bot that is set up: sent now if
    /// the PC is online, otherwise as soon as it is (for up to a day).
    Sent,
    /// An earlier request is still unanswered and went out less than
    /// `REQUEST_RESEND_INTERVAL` ago, so nothing new was sent - the parent
    /// has it already.
    AlreadySent,
    /// No bot is set up (enabled, with a token and a chat/channel), so
    /// nothing went out and nothing was recorded.
    NotSent,
}

/// What `request_more_time` does, given the pending request (if any), the
/// time, today's date and which bots are set up.
fn request_action(
    pending: Option<&PendingRequest>,
    now: Instant,
    today: &str,
    telegram: bool,
    discord: bool,
) -> RequestOutcome {
    if !telegram && !discord {
        return RequestOutcome::NotSent;
    }
    match pending {
        Some(p) if p.is_live(now, today) && now.saturating_duration_since(p.last_sent) < REQUEST_RESEND_INTERVAL => {
            RequestOutcome::AlreadySent
        }
        _ => RequestOutcome::Sent,
    }
}

/// The pending request after one was sent now: the bots it has gone to so
/// far (a reminder may reach a bot set up since), dated now.
fn after_sending(pending: Option<&PendingRequest>, now: Instant, today: &str, telegram: bool, discord: bool) -> PendingRequest {
    let earlier = pending.filter(|p| p.is_live(now, today));
    PendingRequest {
        last_sent: now,
        day: today.to_string(),
        telegram: telegram || earlier.is_some_and(|p| p.telegram),
        discord: discord || earlier.is_some_and(|p| p.discord),
    }
}

/// Takes the pending request, if there is one that hasn't expired.
fn take_live(pending: &mut Option<PendingRequest>, now: Instant, today: &str) -> Option<PendingRequest> {
    pending.take().filter(|p| p.is_live(now, today))
}

/// Which bots alerts can go to: enabled, with a token and somewhere to post
/// (the same test the alert queue makes).
fn configured_bots() -> (bool, bool) {
    let tg = database::get_telegram_config();
    let dc = database::get_discord_config();
    let telegram = tg.enabled && tg.bot_token.as_deref().is_some_and(|t| !t.is_empty()) && tg.admin_chat_id.is_some();
    let discord = dc.enabled && dc.bot_token.as_deref().is_some_and(|t| !t.is_empty()) && dc.channel_id.is_some();
    (telegram, discord)
}

/// Called when the kid clicks "Request More Time" on the lock screen. Sends
/// the request to every bot that is set up, through the alert queue (so it
/// isn't lost if this session's bot isn't connected, or the PC is offline
/// for a moment), and says what happened - see `RequestOutcome`. Only a
/// request that went out is recorded as pending.
pub fn request_more_time(note: Option<String>, remaining_seconds: i32) -> RequestOutcome {
    let (telegram, discord) = configured_bots();
    let today = database::local_today_date();
    let now = Instant::now();

    let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
    let outcome = request_action(pending.as_ref(), now, &today, telegram, discord);
    if outcome != RequestOutcome::Sent {
        return outcome;
    }

    let body = format!(
        "⏰ {} {}\n⏳ {}: {}",
        current_windows_username(),
        i18n::t("request.notify.header"),
        i18n::t("tg.status.remaining"),
        format_hm(remaining_seconds),
    );
    // The note is the child's own text: posted so that nothing in it is
    // read as formatting, a link or a mention.
    let note = note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let note_line = |literal: fn(&str) -> String| {
        note.map(literal).filter(|n| !n.is_empty()).map(|n| format!("\n💬 {n}")).unwrap_or_default()
    };
    let tg_text = format!("{body}{}\n\n{}", note_line(telegram_literal), i18n::t("request.notify.reply_tg"));
    let dc_text = format!("{body}{}\n\n{}", note_line(discord_literal), i18n::t("request.notify.reply_dc"));
    watchdog::raise_alert_per_bot(&tg_text, &dc_text);

    *pending = Some(after_sending(pending.as_ref(), now, &today, telegram, discord));
    outcome
}

/// Called after a grant-worthy command (extend/reset/unlock) succeeds. If a
/// request is pending (and hasn't expired), clears it and lets the *other*
/// platform it went to know it's already been handled, so a second reply
/// there isn't necessary.
pub fn resolve_if_pending(granted_via: &str, detail: &str) {
    let today = database::local_today_date();
    let Some(request) = take_live(&mut PENDING.lock().unwrap_or_else(PoisonError::into_inner), Instant::now(), &today)
    else {
        return;
    };

    let text = format!("✅ {} {} ({})", i18n::t("request.notify.resolved"), detail, granted_via);

    if granted_via != "Telegram" && request.telegram && database::get_telegram_config().enabled {
        telegram::notify_admin(&text);
    }
    if granted_via != "Discord" && request.discord && database::get_discord_config().enabled {
        discord::notify_admin(&text);
    }
}

/// Called whenever the passcode is used locally (lock screen or tray menu)
/// to add time, so the parent finds out even if they weren't the one who
/// entered it - the passcode being used is worth knowing about either way.
/// Through the alert queue, so it isn't lost if no bot is connected in this
/// session or the PC is offline.
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
    watchdog::raise_alert(&text);
}

/// Called once per run of wrong passcodes, when it reaches the point where
/// entry starts being locked (see `passcode_guard`) - someone may be
/// guessing. Sent through the watchdog's alert queue rather than the running
/// bots, like the tamper alerts: it goes out even if the bot hasn't
/// connected yet (or the PC is offline - then it's retried), and only once.
pub fn notify_passcode_lockout(failures: u32) {
    let username = current_windows_username();
    let text = format!(
        "🔐 {} {} {}\n{}",
        username,
        i18n::t("passcode_guard.notify.header"),
        failures,
        i18n::t("passcode_guard.notify.suffix"),
    );
    watchdog::raise_alert(&text);
}

/// Send a routine notice through whichever bots are connected in this
/// session right now; dropped otherwise (see the module comment).
fn notify_live(text: &str) {
    if database::get_telegram_config().enabled {
        telegram::notify_admin(text);
    }
    if database::get_discord_config().enabled {
        discord::notify_admin(text);
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
    notify_live(&text);
}

/// Called whenever the Windows session locks/unlocks (see
/// `mini_overlay::set_session_locked`), same pattern as
/// `notify_activity_status` above but for an actual lock rather than the
/// idle-detection heuristic - so the parent can tell the two apart.
pub fn notify_session_lock(is_locked: bool, remaining_seconds: i32) {
    let username = current_windows_username();
    let (icon, header_key) = if is_locked {
        ("🔒", "activity.notify.locked")
    } else {
        ("🔓", "activity.notify.unlocked")
    };

    let text = format!(
        "{} {} {}\n⏳ {}: {}",
        icon,
        username,
        i18n::t(header_key),
        i18n::t("tg.status.remaining"),
        format_hm(remaining_seconds),
    );
    notify_live(&text);
}

/// Called once per time-zone change (`database::take_pending_time_zone_alert`):
/// the day keeps following the remembered zone until a parent saves Settings.
/// Sent through the watchdog's alert queue, like the passcode lockout alert,
/// so it isn't lost if the bot hasn't connected yet or this session isn't the
/// one running it - it's only raised once.
pub fn notify_time_zone_change(old_zone: &database::TimeZoneLabel, new_zone: &database::TimeZoneLabel) {
    let describe = |zone: &database::TimeZoneLabel| {
        if zone.dst_off {
            format!("{}, {}", zone.name, i18n::t("time_zone.notify.dst_off"))
        } else {
            zone.name.clone()
        }
    };
    let text = format!(
        "🌐 {} {}\n{} {}\n{} {}\n{}",
        current_windows_username(),
        i18n::t("time_zone.notify.header"),
        i18n::t("time_zone.notify.old"),
        describe(old_zone),
        i18n::t("time_zone.notify.new"),
        describe(new_zone),
        i18n::t("time_zone.notify.suffix"),
    );
    watchdog::raise_alert(&text);
}

/// Called when remaining time crosses one of the configured warning
/// thresholds (the same ones that trigger the kid-facing overlay), so the
/// parent gets the same heads-up without needing to be looking at `status`.
/// Live only: a "10 minutes left" delivered later is stale, and "out of
/// time" (queued) follows anyway.
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
    notify_live(&text);
}

/// Called the moment remaining time hits zero and the blocking overlay
/// triggers - the actual enforcement event, so worth a push regardless of
/// whether either warning threshold was configured/reached first. Through
/// the alert queue: a parent relying on it should get it even if no bot is
/// connected in this session at that moment (it says when it happened if
/// it arrives late).
pub fn notify_out_of_time() {
    let username = current_windows_username();
    let text = format!("⏰ {} {}", username, i18n::t("outoftime.notify.header"));
    watchdog::raise_alert(&text);
}

/// How far the clock moved, as "Xh Ym". Saturates instead of wrapping for a
/// jump of 2^31 seconds (68 years) or more.
fn clock_drift_hm(drift_secs: i64) -> String {
    format_hm(i32::try_from(drift_secs.unsigned_abs()).unwrap_or(i32::MAX))
}

/// Called when `database::take_pending_clock_tamper_alert` reports a newly
/// detected system-clock jump - the app is continuing to enforce today's
/// limit as already tracked rather than granting a fresh allowance for
/// whatever date the clock now shows. That call marks it as reported, so
/// it goes through the alert queue: kept and retried if it can't be sent
/// right now.
pub fn notify_clock_tamper(drift_secs: i64) {
    let direction = if drift_secs >= 0 {
        i18n::t("clock_tamper.notify.forward")
    } else {
        i18n::t("clock_tamper.notify.backward")
    };

    let text = format!(
        "🕒 {}\n{} {}\n{}",
        i18n::t("clock_tamper.notify.title").replace("{user}", &current_windows_username()),
        direction,
        clock_drift_hm(drift_secs),
        i18n::t("clock_tamper.notify.suffix"),
    );
    watchdog::raise_alert(&text);
}

#[cfg(test)]
mod tests {
    use super::*;

    const TODAY: &str = "2026-11-16";

    fn sent_at(t: Instant, telegram: bool, discord: bool) -> PendingRequest {
        PendingRequest { last_sent: t, day: TODAY.to_string(), telegram, discord }
    }

    #[test]
    fn nothing_is_sent_or_recorded_without_a_bot() {
        let t = Instant::now();
        assert_eq!(request_action(None, t, TODAY, false, false), RequestOutcome::NotSent);
        assert_eq!(request_action(None, t, TODAY, true, false), RequestOutcome::Sent);
        assert_eq!(request_action(None, t, TODAY, false, true), RequestOutcome::Sent);
    }

    #[test]
    fn pressing_again_soon_does_not_resend() {
        let t = Instant::now();
        let p = sent_at(t, true, true);
        let at = |mins: u64| t + Duration::from_secs(mins * 60);
        assert_eq!(request_action(Some(&p), at(5), TODAY, true, true), RequestOutcome::AlreadySent);
        assert_eq!(request_action(Some(&p), at(14), TODAY, true, true), RequestOutcome::AlreadySent);
        assert_eq!(request_action(Some(&p), at(15), TODAY, true, true), RequestOutcome::Sent, "reminder");
        assert_eq!(request_action(Some(&p), at(5), "2026-11-17", true, true), RequestOutcome::Sent, "new day");
        // Bots switched off meanwhile: nothing goes out.
        assert_eq!(request_action(Some(&p), at(30), TODAY, false, false), RequestOutcome::NotSent);
    }

    #[test]
    fn a_reminder_keeps_the_bots_already_reached() {
        let t = Instant::now();
        let p = sent_at(t, true, false);
        let later = t + Duration::from_secs(20 * 60);
        assert_eq!(after_sending(Some(&p), later, TODAY, false, true), sent_at(later, true, true));
        // An expired one is forgotten.
        let much_later = t + REQUEST_EXPIRY;
        assert_eq!(after_sending(Some(&p), much_later, TODAY, false, true), sent_at(much_later, false, true));
        assert_eq!(after_sending(None, t, TODAY, true, false), sent_at(t, true, false));
    }

    #[test]
    fn a_pending_request_expires_after_a_while_and_at_the_next_day() {
        let t = Instant::now();
        let mut pending = Some(sent_at(t, true, true));
        assert_eq!(take_live(&mut pending.clone(), t + Duration::from_secs(59 * 60), TODAY), pending.clone());
        assert_eq!(take_live(&mut pending.clone(), t + REQUEST_EXPIRY, TODAY), None, "expired");
        assert_eq!(take_live(&mut pending.clone(), t, "2026-11-17"), None, "day rolled over");

        // Taking it clears it, live or not: a later grant finds nothing.
        assert!(take_live(&mut pending, t, TODAY).is_some());
        assert_eq!(pending, None);
        assert_eq!(take_live(&mut pending, t, TODAY), None);
        let mut expired = Some(sent_at(t, true, true));
        assert_eq!(take_live(&mut expired, t + REQUEST_EXPIRY, TODAY), None);
        assert_eq!(expired, None);
    }

    #[test]
    fn huge_clock_jumps_do_not_wrap() {
        assert_eq!(clock_drift_hm(3 * 3600 + 120), "3h 2m");
        assert_eq!(clock_drift_hm(-(3 * 3600 + 120)), "3h 2m");
        assert_eq!(clock_drift_hm(1 << 31), "596523h 14m");
        assert_eq!(clock_drift_hm(i64::MIN), "596523h 14m");
        assert_eq!(clock_drift_hm(i64::MAX), "596523h 14m");
    }
}
