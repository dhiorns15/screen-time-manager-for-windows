//! Tracks whether this process's Windows session currently owns the console
//! (i.e. is the foreground user under Fast User Switching), so per-session
//! features that can't tolerate multiple concurrent owners - namely the
//! Telegram/Discord bots, which share one bot token/channel across every
//! Windows account - only run in one session at a time.

use windows::core::PWSTR;
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSFreeMemory, WTSGetActiveConsoleSessionId,
    WTSQuerySessionInformationW, WTSSessionInfo, WTSINFOW, WTS_CURRENT_SERVER_HANDLE,
    WTS_CURRENT_SESSION,
};
use windows::Win32::System::Threading::GetCurrentProcessId;

/// True if this process's session is the one currently attached to the
/// console (the physical display) - i.e. the session a Fast-User-Switching
/// user sees when they're the one sitting in front of the machine. A session
/// that's been switched away from (but is still logged on and running in the
/// background) returns false here, even though it's still a fully live
/// Windows session.
fn is_active_console_session() -> bool {
    unsafe {
        let mut my_session_id = 0u32;
        if ProcessIdToSessionId(GetCurrentProcessId(), &mut my_session_id).is_err() {
            // Couldn't determine our own session - default to true rather
            // than risk silencing the bots on an ordinary single-user
            // machine where this check should never actually fail.
            return true;
        }
        WTSGetActiveConsoleSessionId() == my_session_id
    }
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
