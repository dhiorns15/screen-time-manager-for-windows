//! Tracks whether this process's Windows session currently owns the console
//! (i.e. is the foreground user under Fast User Switching), so per-session
//! features that can't tolerate multiple concurrent owners - namely the
//! Telegram/Discord bots, which share one bot token/channel across every
//! Windows account - only run in one session at a time.

use windows::core::PWSTR;
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSEnumerateSessionsW, WTSFreeMemory, WTSGetActiveConsoleSessionId,
    WTSActive, WTSDisconnected, WTSQuerySessionInformationW, WTSSessionInfo, WTSSessionInfoEx,
    WTSUserName, WTSINFOEXW, WTSINFOW, WTS_CONNECTSTATE_CLASS, WTS_CURRENT_SERVER_HANDLE,
    WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTS_SESSIONSTATE_UNLOCK, WTS_SESSION_INFOW,
};
use windows::Win32::System::Threading::GetCurrentProcessId;

/// This process's session id, if it can be determined.
fn own_session_id() -> Option<u32> {
    let mut my_session_id = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut my_session_id) }.ok()?;
    Some(my_session_id)
}

/// True if this process's session is the one currently attached to the
/// console (the physical display) - i.e. the session a Fast-User-Switching
/// user sees when they're the one sitting in front of the machine. A session
/// that's been switched away from (but is still logged on and running in the
/// background) returns false here, even though it's still a fully live
/// Windows session.
pub fn is_active_console_session() -> bool {
    // Couldn't determine our own session - default to true rather than risk
    // silencing the bots on an ordinary single-user machine where this check
    // should never actually fail.
    own_session_id().is_none_or(|id| unsafe { WTSGetActiveConsoleSessionId() } == id)
}

/// Map WTSINFOEX_LEVEL1's SessionFlags to locked (true) / unlocked (false),
/// or `None` for WTS_SESSIONSTATE_UNKNOWN or anything else. (Windows 7 and
/// Server 2008 R2 report these two flags swapped; the app needs Windows 10+.)
fn lock_state_from_flags(flags: i32) -> Option<bool> {
    match flags as u32 {
        WTS_SESSIONSTATE_LOCK => Some(true),
        WTS_SESSIONSTATE_UNLOCK => Some(false),
        _ => None,
    }
}

/// Whether this session is locked right now, asked of Windows directly rather
/// than taken from a WM_WTSSESSION_CHANGE message (which any process in the
/// session can post to our window). `None` if Windows won't say.
pub fn query_session_locked() -> Option<bool> {
    unsafe {
        let mut buffer = PWSTR::null();
        let mut bytes_returned: u32 = 0;
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes_returned,
        )
        .ok()?;
        if buffer.is_null() {
            return None;
        }
        let state = if bytes_returned as usize >= std::mem::size_of::<WTSINFOEXW>() {
            let info = *(buffer.0 as *const WTSINFOEXW);
            if info.Level == 1 {
                lock_state_from_flags(info.Data.WTSInfoExLevel1.SessionFlags)
            } else {
                None
            }
        } else {
            None
        };
        WTSFreeMemory(buffer.0.cast());
        state
    }
}

/// Decide the lock state from a WM_WTSSESSION_CHANGE lock/unlock message,
/// given what Windows reports (`wts`, see `query_session_locked`) and whether
/// this session owns the console. A lock message is only refused when Windows
/// positively says this session, sitting at the console, is unlocked - that's
/// a posted fake meant to freeze the countdown. A switched-away session can't
/// be in use, so its lock is always believed. When Windows can't say, the
/// message is believed, as before. Either way `periodic_lock_state` asks
/// Windows again a few seconds later and corrects it.
pub fn verified_lock_state(message_says_locked: bool, wts: Option<bool>, is_console: bool) -> bool {
    if message_says_locked {
        !(wts == Some(false) && is_console)
    } else {
        wts == Some(true)
    }
}

/// What the periodic lock check (every few accounting ticks, whatever the
/// current state) sets the lock state to, given what Windows reports (`wts`,
/// see `query_session_locked`) and whether this session owns the console:
/// `Some(state)`, or `None` to leave it as it is.
///
/// Windows saying locked always lock-pauses - that catches a real lock whose
/// WM_WTSSESSION_CHANGE was refused because the flags hadn't caught up yet,
/// or never arrived (`WTSRegisterSessionNotification` failed). Windows saying
/// unlocked unpauses only at the console: a switched-away session can't be
/// in use, so it stays paused. An unknown answer changes nothing.
pub fn periodic_lock_state(wts: Option<bool>, is_console: bool) -> Option<bool> {
    match wts {
        Some(true) => Some(true),
        Some(false) if is_console => Some(false),
        _ => None,
    }
}

/// One session as `other_users_logged_on` sees it: its id, whether it has a
/// user name (`None` if the name couldn't be read) and its connection state.
type SessionSummary = (u32, Option<bool>, WTS_CONNECTSTATE_CLASS);

/// Whether a session other than `own` has a signed-in user. A session whose
/// user name couldn't be read counts as having one unless it's session 0
/// (services) or not active or disconnected (a listener, say) - a failed
/// query must tip the lock-screen countdown towards signing this session
/// out, not towards shutting down under a switched-away user.
fn is_other_user(own: u32, (id, has_name, state): SessionSummary) -> bool {
    if id == own {
        return false;
    }
    match has_name {
        Some(has_name) => has_name,
        None => id != 0 && (state == WTSActive || state == WTSDisconnected),
    }
}

/// Whether any session other than `own` has a signed-in user.
fn any_other_user(own: u32, sessions: &[SessionSummary]) -> bool {
    sessions.iter().any(|&session| is_other_user(own, session))
}

/// Whether another Windows account is signed in on this machine (in any
/// session state, including switched away under Fast User Switching). Errs
/// towards `true` when it can't tell, since the caller then signs this
/// session out instead of shutting the whole machine down.
pub fn other_users_logged_on() -> bool {
    let Some(own) = own_session_id() else { return true };
    unsafe {
        let mut list: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count: u32 = 0;
        if WTSEnumerateSessionsW(WTS_CURRENT_SERVER_HANDLE, 0, 1, &mut list, &mut count).is_err()
            || list.is_null()
        {
            return true;
        }
        let sessions: Vec<SessionSummary> = std::slice::from_raw_parts(list, count as usize)
            .iter()
            .map(|s| (s.SessionId, session_has_user(s.SessionId), s.State))
            .collect();
        WTSFreeMemory(list.cast());
        any_other_user(own, &sessions)
    }
}

/// Whether a session has a signed-in user (a non-empty WTSUserName), or
/// `None` if Windows won't say.
unsafe fn session_has_user(session_id: u32) -> Option<bool> {
    let mut buffer = PWSTR::null();
    let mut bytes_returned: u32 = 0;
    if WTSQuerySessionInformationW(
        WTS_CURRENT_SERVER_HANDLE,
        session_id,
        WTSUserName,
        &mut buffer,
        &mut bytes_returned,
    )
    .is_err()
        || buffer.is_null()
    {
        return None;
    }
    let has_user = bytes_returned >= 4 && *buffer.0 != 0;
    WTSFreeMemory(buffer.0.cast());
    Some(has_user)
}

/// This session's raw WTS LogonTime and the query's CurrentTime - both
/// Windows FILETIMEs (100ns ticks since 1601-01-01). LogonTime is fixed for
/// the life of the session, so repeated calls during the same session always
/// report the identical value - useful for telling "this specific session"
/// apart from a later one, not just measuring elapsed time. `None` on a WTS
/// lookup failure.
fn session_times() -> Option<(i64, i64)> {
    unsafe {
        let mut buffer = PWSTR::null();
        let mut bytes_returned: u32 = 0;
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            WTS_CURRENT_SESSION,
            WTSSessionInfo,
            &mut buffer,
            &mut bytes_returned,
        )
        .ok()?;

        if buffer.is_null() {
            return None;
        }

        let info = *(buffer.0 as *const WTSINFOW);
        WTSFreeMemory(buffer.0.cast());

        Some((info.LogonTime, info.CurrentTime))
    }
}

/// This session's logon time, as an opaque token that's identical across
/// every call made during the same session and different for any other
/// session - see `session_times`. `None` on a WTS lookup failure.
pub fn logon_token() -> Option<i64> {
    session_times().map(|(logon, _)| logon)
}

/// Seconds since this session's interactive logon began, or `None` if it
/// couldn't be determined (WTS lookup failure - callers should treat that as
/// "no grace period" rather than suppressing alerts forever on a lookup
/// that'll never succeed).
pub fn seconds_since_logon() -> Option<i64> {
    session_times().map(|(logon, current)| (current - logon) / 10_000_000)
}

/// Whether the last `sync_remote_bots` call saw this session owning the
/// console - `None` until the first call. Used to only act on an actual
/// transition (see `sync_remote_bots`) instead of re-issuing start/stop every
/// tick.
static WAS_ACTIVE: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

/// Start or stop the remote-control bots to match whether this session
/// currently owns the console. Cheap to call on every tick - a no-op unless
/// active-session ownership actually changed since the last call.
///
/// Deliberately edge-triggered rather than calling start/stop unconditionally
/// every tick: start_bot_thread/stop_bot_thread join the *previous* bot-
/// thread generation before doing anything (see discord.rs/telegram.rs), so
/// an unconditional per-second call would mean the UI thread - this runs on
/// the main window's always-on accounting tick (`mini_overlay::on_accounting_tick`,
/// which keeps running while the lock overlay is up) - briefly blocks waiting on that join
/// every single tick. Only calling on a real transition keeps that (rare,
/// at most a few hundred ms) blocking to genuine Fast-User-Switching
/// hand-offs instead of every tick.
pub fn sync_remote_bots() {
    let active = is_active_console_session();
    let mut was_active = WAS_ACTIVE.lock().unwrap();
    if *was_active == Some(active) {
        return;
    }
    *was_active = Some(active);
    drop(was_active);

    if active {
        crate::telegram::start_bot_thread();
        crate::discord::start_bot_thread();
    } else {
        crate::telegram::stop_bot_thread();
        crate::discord::stop_bot_thread();
    }
}

/// Restart this session's Telegram and/or Discord bot so it picks up a bot
/// configuration that was just saved (Settings, or a setup wizard) - a bot
/// only reads its settings when it starts, so without this a bot set up or
/// changed in Settings stayed off (or on the old settings) until the next
/// sign-in. Only if this session is the one running the bots (see
/// `sync_remote_bots`); a stop followed by a start goes through
/// `BotLifecycle`, which queues the start until the old bot has finished,
/// and a bot that's now disabled simply doesn't come back.
pub fn restart_remote_bots(telegram: bool, discord: bool) {
    if *WAS_ACTIVE.lock().unwrap() != Some(true) {
        return;
    }
    if telegram {
        crate::telegram::stop_bot_thread();
        crate::telegram::start_bot_thread();
    }
    if discord {
        crate::discord::stop_bot_thread();
        crate::discord::start_bot_thread();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_flags_map_to_states() {
        assert_eq!(lock_state_from_flags(0), Some(true));
        assert_eq!(lock_state_from_flags(1), Some(false));
        assert_eq!(lock_state_from_flags(-1), None, "WTS_SESSIONSTATE_UNKNOWN");
    }

    #[test]
    fn posted_fake_lock_is_refused_at_the_console() {
        assert!(!verified_lock_state(true, Some(false), true));
    }

    #[test]
    fn real_and_unverifiable_locks_are_believed() {
        assert!(verified_lock_state(true, Some(true), true));
        assert!(verified_lock_state(true, None, true));
        // Switched away under Fast User Switching: can't be in use
        assert!(verified_lock_state(true, Some(false), false));
    }

    #[test]
    fn unlock_only_when_windows_does_not_say_locked() {
        assert!(!verified_lock_state(false, Some(false), true));
        assert!(!verified_lock_state(false, None, true));
        assert!(verified_lock_state(false, Some(true), true));
    }

    #[test]
    fn periodic_check_locks_whenever_windows_says_locked() {
        // A real lock refused (flags not caught up) or never delivered
        assert_eq!(periodic_lock_state(Some(true), true), Some(true));
        assert_eq!(periodic_lock_state(Some(true), false), Some(true));
    }

    #[test]
    fn periodic_check_unlocks_only_at_the_console() {
        // A missed unlock, or a posted fake lock that was believed
        assert_eq!(periodic_lock_state(Some(false), true), Some(false));
        // Switched away under Fast User Switching: stays as it is (paused)
        assert_eq!(periodic_lock_state(Some(false), false), None);
    }

    #[test]
    fn periodic_check_ignores_an_unknown_answer() {
        assert_eq!(periodic_lock_state(None, true), None);
        assert_eq!(periodic_lock_state(None, false), None);
    }

    const LISTEN: WTS_CONNECTSTATE_CLASS = WTS_CONNECTSTATE_CLASS(6); // WTSListen

    #[test]
    fn other_users_ignore_own_and_userless_sessions() {
        // Services session 0 (no user), our session 3, listener 65536
        let base = [(0, Some(false), WTSDisconnected), (3, Some(true), WTSActive), (65536, Some(false), LISTEN)];
        assert!(!any_other_user(3, &base));
        let mut with_other = base.to_vec();
        with_other.push((5, Some(true), WTSDisconnected));
        assert!(any_other_user(3, &with_other));
        assert!(!any_other_user(3, &[]));
    }

    #[test]
    fn a_session_whose_user_name_cannot_be_read_counts_as_a_user() {
        // A switched-away parent's session, name query failed: sign out, don't shut down
        assert!(any_other_user(3, &[(3, Some(true), WTSActive), (2, None, WTSDisconnected)]));
        assert!(any_other_user(3, &[(3, Some(true), WTSActive), (2, None, WTSActive)]));
        // ...but not session 0, a listener, or our own session
        assert!(!any_other_user(3, &[(0, None, WTSDisconnected), (65536, None, LISTEN), (3, None, WTSActive)]));
    }
}
