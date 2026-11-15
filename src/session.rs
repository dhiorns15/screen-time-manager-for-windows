//! Tracks whether this process's Windows session currently owns the console
//! (i.e. is the foreground user under Fast User Switching), so per-session
//! features that can't tolerate multiple concurrent owners - namely the
//! Telegram/Discord bots, which share one bot token/channel across every
//! Windows account - only run in one session at a time.

use windows::core::PWSTR;
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSEnumerateSessionsW, WTSFreeMemory, WTSGetActiveConsoleSessionId,
    WTSQuerySessionInformationW, WTSSessionInfo, WTSUserName,
    WTSINFOW, WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION,
    WTS_SESSION_INFOW,
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

/// Whether any session other than `own` has a signed-in user, from
/// (session id, has a user name) pairs.
fn any_other_user(own: u32, sessions: &[(u32, bool)]) -> bool {
    sessions.iter().any(|&(id, has_user)| id != own && has_user)
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
        let sessions: Vec<(u32, bool)> = std::slice::from_raw_parts(list, count as usize)
            .iter()
            .map(|s| (s.SessionId, session_has_user(s.SessionId)))
            .collect();
        WTSFreeMemory(list.cast());
        any_other_user(own, &sessions)
    }
}

/// Whether a session has a signed-in user (a non-empty WTSUserName).
unsafe fn session_has_user(session_id: u32) -> bool {
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
        return false;
    }
    let has_user = bytes_returned >= 4 && *buffer.0 != 0;
    WTSFreeMemory(buffer.0.cast());
    has_user
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
/// the mini overlay's WM_TIMER handler - briefly blocks waiting on that join
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_users_ignore_own_and_userless_sessions() {
        // Services session 0 (no user), our session 3, listener 65536
        assert!(!any_other_user(3, &[(0, false), (3, true), (65536, false)]));
        assert!(any_other_user(3, &[(0, false), (3, true), (5, true)]));
        assert!(!any_other_user(3, &[]));
    }
}
