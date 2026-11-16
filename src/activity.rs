//! Signs that the PC is in use other than keyboard and mouse input, for idle
//! auto-pause (see `mini_overlay::check_idle_state`). Watching a video or
//! playing with a controller used to read as idle after the timeout, which
//! stopped the clock and gave free screen time. Any of these now counts as
//! use:
//!
//! - sound actually playing on the default output device (its measured peak
//!   level, so a silent or muted stream doesn't count);
//! - an app keeping the display on (`ES_DISPLAY_REQUIRED` in the system's
//!   execution state - video players and browsers playing video ask for it);
//! - a game controller being used (XInput: a button, a trigger or a stick
//!   moved since the previous sample - not merely connected, and not stick
//!   drift);
//! - a full-screen app, game or presentation in front.
//!
//! The probes run on their own thread, once a second, so a slow or hung
//! audio or controller driver can't stall the UI thread (whose heartbeat the
//! watchdog watches). The thread only publishes the moment a sign of use was
//! last seen; the accounting tick combines that with the keyboard/mouse idle
//! time (`effective_idle_secs`). A probe that fails - no audio device, no
//! XInput DLL, an API error - simply reads as "no sign", and is logged once
//! per failure, not every second.
//!
//! Signs are only gathered while the tick says they count
//! (`set_counting`: idle detection on and the session not locked) and while
//! this session is the one at the console: sound, the display request and
//! controllers are machine-wide, so under Fast User Switching another
//! account's video must not keep this account's clock running.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use windows::{
    core::{s, w},
    Win32::{
        Foundation::FARPROC,
        Media::Audio::{
            eConsole, eRender, Endpoints::IAudioMeterInformation, IMMDeviceEnumerator,
            MMDeviceEnumerator,
        },
        System::{
            Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED},
            LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32},
            Power::{CallNtPowerInformation, SystemExecutionState, ES_DISPLAY_REQUIRED},
            SystemInformation::GetTickCount64,
        },
        UI::Shell::{
            SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE,
            QUNS_RUNNING_D3D_FULL_SCREEN,
        },
    },
};

// ============================================================================
// Decision core (pure)
// ============================================================================

/// What one sample saw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Signals {
    /// The default output device is playing something audible.
    pub sound: bool,
    /// Some app is keeping the display on.
    pub display_required: bool,
    /// A controller was used since the previous sample.
    pub controller: bool,
    /// A full-screen app, game or presentation is in front.
    pub full_screen: bool,
}

/// Peak level (0 to 1) above which the output counts as playing sound: about
/// -50 dB, well below quiet speech, well above the zero a silent or muted
/// stream reports.
const SOUND_PEAK_THRESHOLD: f32 = 0.003;

/// Sound counts only once it has been heard in `SOUND_MIN_HEARD` of the last
/// `SOUND_WINDOW` samples. A short beep (a second or two) - including the
/// app's own warning and lock-screen sounds, which play once per event -
/// never does, so they can't hold the clock running; a video or music easily
/// does, quiet moments and all (a looping chime with gaps between rings read
/// heard, heard, quiet, heard, heard on this PC).
const SOUND_WINDOW: u32 = 5;
const SOUND_MIN_HEARD: u32 = 3;

/// Whether a peak level reading is audible sound.
fn peak_is_sound(peak: f32) -> bool {
    peak.is_finite() && peak > SOUND_PEAK_THRESHOLD
}

/// When a sign of use was last seen, from a series of samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActivityClock {
    /// `GetTickCount64` at the last sample that showed a sign of use.
    last_signal_ms: Option<u64>,
    /// Which of the recent samples heard sound, newest in the lowest bit
    /// (see `SOUND_WINDOW`).
    sound_heard: u32,
}

impl ActivityClock {
    pub const fn new() -> Self {
        ActivityClock { last_signal_ms: None, sound_heard: 0 }
    }

    /// Take in the sample taken at `now_ms`.
    pub fn observe(&mut self, now_ms: u64, signals: Signals) {
        let window = (1u32 << SOUND_WINDOW) - 1;
        self.sound_heard = ((self.sound_heard << 1) | u32::from(signals.sound)) & window;
        // The newest sample must hear it too: sound that has stopped isn't
        // use now.
        let sound = signals.sound && self.sound_heard.count_ones() >= SOUND_MIN_HEARD;
        if sound || signals.display_required || signals.controller || signals.full_screen {
            self.last_signal_ms = Some(now_ms);
        }
    }

    /// Forget everything seen so far (signs stopped counting for a while).
    pub fn reset(&mut self) {
        *self = ActivityClock::new();
    }

    pub fn last_signal_ms(&self) -> Option<u64> {
        self.last_signal_ms
    }
}

/// How long the session has been idle, in seconds: the time since the last
/// keyboard/mouse input or since the last sign of use, whichever is more
/// recent. Idle auto-pause compares this with the timeout.
pub fn effective_idle_secs(input_idle_secs: u32, last_signal_ms: Option<u64>, now_ms: u64) -> u32 {
    match last_signal_ms {
        Some(at) => {
            let since_signal = u32::try_from(now_ms.saturating_sub(at) / 1000).unwrap_or(u32::MAX);
            input_idle_secs.min(since_signal)
        }
        None => input_idle_secs,
    }
}

/// XInput's XINPUT_GAMEPAD.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gamepad {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub thumb_lx: i16,
    pub thumb_ly: i16,
    pub thumb_rx: i16,
    pub thumb_ry: i16,
}

/// How far (of 32767) a stick axis has to move between samples to count as
/// use. Resting sticks jitter and drift by a few hundred to a couple of
/// thousand; a thumb moving a stick goes much further.
const STICK_MOVE: i32 = 4000;
/// How far (of 255) a trigger has to move between samples to count.
const TRIGGER_MOVE: i32 = 30;

/// Whether the controller was used between two samples: a button changed, or
/// a trigger or stick moved further than noise or drift explains. The packet
/// number is not looked at - a drifting stick changes it all the time.
pub fn gamepad_used(prev: &Gamepad, cur: &Gamepad) -> bool {
    let moved = |a: i32, b: i32, limit: i32| (a - b).abs() > limit;
    prev.buttons != cur.buttons
        || moved(prev.left_trigger.into(), cur.left_trigger.into(), TRIGGER_MOVE)
        || moved(prev.right_trigger.into(), cur.right_trigger.into(), TRIGGER_MOVE)
        || moved(prev.thumb_lx.into(), cur.thumb_lx.into(), STICK_MOVE)
        || moved(prev.thumb_ly.into(), cur.thumb_ly.into(), STICK_MOVE)
        || moved(prev.thumb_rx.into(), cur.thumb_rx.into(), STICK_MOVE)
        || moved(prev.thumb_ry.into(), cur.thumb_ry.into(), STICK_MOVE)
}

// ============================================================================
// Shared state between the probe thread and the accounting tick
// ============================================================================

/// `GetTickCount64` when the probe thread last saw a sign of use; 0 for none.
static LAST_SIGNAL_MS: AtomicU64 = AtomicU64::new(0);

/// Whether signs of use count right now - set by the accounting tick every
/// tick. While off, the thread samples nothing and forgets what it saw.
static COUNTING: AtomicBool = AtomicBool::new(false);

/// When a sign of use was last seen (`GetTickCount64`), if one has been
/// since signs last started counting.
pub fn last_signal_ms() -> Option<u64> {
    match LAST_SIGNAL_MS.load(Ordering::SeqCst) {
        0 => None,
        at => Some(at),
    }
}

/// Whether signs of use should be gathered: idle detection is on and the
/// session isn't locked. Called by the accounting tick.
pub fn set_counting(on: bool) {
    COUNTING.store(on, Ordering::SeqCst);
}

/// How often the probes sample.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Start the probe thread. `is_console` says whether this session is the
/// one at the console (see the module comment). If the thread can't be
/// started, idle detection goes on with keyboard and mouse alone.
pub fn start(is_console: fn() -> bool) {
    let spawned = std::thread::Builder::new()
        .name("activity-probes".to_string())
        .spawn(move || run(is_console));
    if let Err(e) = spawned {
        eprintln!("[Activity] Couldn't start the probe thread ({e}) - idle detection uses keyboard and mouse only");
    }
}

fn run(is_console: fn() -> bool) {
    // COM for the audio meter, for the life of this thread (the life of the
    // app). If it fails, the audio probe reports its error once and the
    // other probes carry on.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let mut probes = Probes::new();
    let mut clock = ActivityClock::new();
    loop {
        std::thread::sleep(SAMPLE_INTERVAL);
        if !COUNTING.load(Ordering::SeqCst) || !is_console() {
            clock.reset();
            probes.forget_baselines();
            LAST_SIGNAL_MS.store(0, Ordering::SeqCst);
            continue;
        }
        let signals = probes.sample();
        let now = unsafe { GetTickCount64() };
        clock.observe(now, signals);
        if let Some(at) = clock.last_signal_ms() {
            LAST_SIGNAL_MS.store(at.max(1), Ordering::SeqCst);
        }
    }
}

// ============================================================================
// Probes (thin Win32 wrappers)
// ============================================================================

/// Logs a probe's failure once, and its recovery once, rather than every
/// second.
struct FailureLog {
    name: &'static str,
    failing: bool,
}

impl FailureLog {
    const fn new(name: &'static str) -> Self {
        FailureLog { name, failing: false }
    }

    fn failed(&mut self, what: &dyn std::fmt::Display) {
        if !self.failing {
            self.failing = true;
            eprintln!("[Activity] {} unavailable: {what} - counted as no sign of use", self.name);
        }
    }

    fn worked(&mut self) {
        if self.failing {
            self.failing = false;
            eprintln!("[Activity] {} working again", self.name);
        }
    }
}

pub struct Probes {
    audio: AudioProbe,
    controllers: ControllerProbe,
    power_log: FailureLog,
    full_screen_log: FailureLog,
}

impl Probes {
    pub fn new() -> Self {
        Probes {
            audio: AudioProbe::new(),
            controllers: ControllerProbe::new(),
            power_log: FailureLog::new("Display request check"),
            full_screen_log: FailureLog::new("Full-screen check"),
        }
    }

    /// One sample of every sign.
    pub fn sample(&mut self) -> Signals {
        Signals {
            sound: self.audio.sample(),
            display_required: display_required(&mut self.power_log),
            controller: self.controllers.sample(),
            full_screen: full_screen(&mut self.full_screen_log),
        }
    }

    /// Controllers start from a fresh baseline next time (nothing compared
    /// across a time signs didn't count).
    pub fn forget_baselines(&mut self) {
        self.controllers.forget_baselines();
    }
}

/// Re-read which device is the default output this often (samples), so a
/// switch to headphones, a TV or a USB headset is followed.
const AUDIO_REFRESH_SAMPLES: u32 = 15;
/// After a failure (no output device, the audio service restarting), try
/// again after this many samples.
const AUDIO_RETRY_SAMPLES: u32 = 15;

struct AudioProbe {
    enumerator: Option<IMMDeviceEnumerator>,
    meter: Option<IAudioMeterInformation>,
    /// Samples since the meter was acquired.
    age: u32,
    /// Samples to wait before trying to acquire a meter again.
    retry_wait: u32,
    log: FailureLog,
}

impl AudioProbe {
    fn new() -> Self {
        AudioProbe { enumerator: None, meter: None, age: 0, retry_wait: 0, log: FailureLog::new("Sound check") }
    }

    fn sample(&mut self) -> bool {
        self.age = self.age.saturating_add(1);
        if self.age > AUDIO_REFRESH_SAMPLES {
            self.meter = None;
        }
        if self.meter.is_none() {
            if self.retry_wait > 0 {
                self.retry_wait -= 1;
                return false;
            }
            match self.acquire() {
                Ok(meter) => {
                    self.meter = Some(meter);
                    self.age = 0;
                }
                Err(e) => {
                    self.retry_wait = AUDIO_RETRY_SAMPLES;
                    self.log.failed(&e);
                    return false;
                }
            }
        }
        let Some(meter) = self.meter.as_ref() else { return false };
        match unsafe { meter.GetPeakValue() } {
            Ok(peak) => {
                self.log.worked();
                peak_is_sound(peak)
            }
            Err(e) => {
                // The device went away or changed: acquire again next time.
                self.meter = None;
                self.log.failed(&e);
                false
            }
        }
    }

    fn acquire(&mut self) -> windows::core::Result<IAudioMeterInformation> {
        let enumerator = match &self.enumerator {
            Some(e) => e.clone(),
            None => {
                let e: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
                self.enumerator = Some(e.clone());
                e
            }
        };
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }?;
        unsafe { device.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None) }
    }
}

/// Whether any app is keeping the display on. Works for a standard account:
/// it reads the combined execution state, not the list of requests (which
/// `powercfg /requests` shows, and which needs elevation).
fn display_required(log: &mut FailureLog) -> bool {
    let mut state: u32 = 0;
    let status = unsafe {
        CallNtPowerInformation(
            SystemExecutionState,
            None,
            0,
            Some(&mut state as *mut u32 as *mut core::ffi::c_void),
            std::mem::size_of::<u32>() as u32,
        )
    };
    if status.is_err() {
        log.failed(&format_args!("NTSTATUS 0x{:08X}", status.0));
        return false;
    }
    log.worked();
    state & ES_DISPLAY_REQUIRED.0 != 0
}

/// Whether a full-screen app, a game or a presentation is in front.
fn full_screen(log: &mut FailureLog) -> bool {
    match unsafe { SHQueryUserNotificationState() } {
        Ok(state) => {
            log.worked();
            state == QUNS_BUSY || state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE
        }
        Err(e) => {
            log.failed(&e);
            false
        }
    }
}

/// XInput's XINPUT_STATE.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct XInputState {
    packet_number: u32,
    gamepad: Gamepad,
}

type XInputGetStateFn = unsafe extern "system" fn(user_index: u32, state: *mut XInputState) -> u32;

/// XInput supports four controllers.
const XINPUT_SLOTS: usize = 4;
const ERROR_SUCCESS: u32 = 0;
/// An empty slot is only looked at again after this many samples: asking an
/// empty slot is the slow part of XInput.
const EMPTY_SLOT_WAIT: u32 = 5;

struct ControllerProbe {
    /// `XInputGetState`, once loaded; `None` if no XInput DLL could be.
    get_state: Option<XInputGetStateFn>,
    loaded: bool,
    previous: [Option<Gamepad>; XINPUT_SLOTS],
    skip: [u32; XINPUT_SLOTS],
}

impl ControllerProbe {
    fn new() -> Self {
        ControllerProbe { get_state: None, loaded: false, previous: [None; XINPUT_SLOTS], skip: [0; XINPUT_SLOTS] }
    }

    /// Load XInput from System32 on first use - at run time, not linked, so
    /// a PC without it still starts the app. xinput1_4 is part of Windows
    /// 8 and later; xinput9_1_0 is the older one that's always there.
    fn load(&mut self) {
        self.loaded = true;
        for dll in [w!("xinput1_4.dll"), w!("xinput9_1_0.dll")] {
            let Ok(module) = (unsafe { LoadLibraryExW(dll, None, LOAD_LIBRARY_SEARCH_SYSTEM32) }) else {
                continue;
            };
            let proc: FARPROC = unsafe { GetProcAddress(module, s!("XInputGetState")) };
            if let Some(f) = proc {
                // SAFETY: XInputGetState has exactly this signature.
                self.get_state = Some(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, XInputGetStateFn>(f) });
                return;
            }
        }
        eprintln!("[Activity] No XInput DLL - controllers aren't counted as use");
    }

    fn sample(&mut self) -> bool {
        if !self.loaded {
            self.load();
        }
        let Some(get_state) = self.get_state else { return false };
        let mut used = false;
        for slot in 0..XINPUT_SLOTS {
            if self.skip[slot] > 0 {
                self.skip[slot] -= 1;
                continue;
            }
            let mut state = XInputState::default();
            let result = unsafe { get_state(slot as u32, &mut state) };
            if result != ERROR_SUCCESS {
                // Empty (or failing) slot: no baseline, look again later.
                self.previous[slot] = None;
                self.skip[slot] = EMPTY_SLOT_WAIT;
                continue;
            }
            if let Some(prev) = &self.previous[slot] {
                used |= gamepad_used(prev, &state.gamepad);
            }
            self.previous[slot] = Some(state.gamepad);
        }
        used
    }

    fn forget_baselines(&mut self) {
        self.previous = [None; XINPUT_SLOTS];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOUND: Signals = Signals { sound: true, display_required: false, controller: false, full_screen: false };
    const NONE: Signals = Signals { sound: false, display_required: false, controller: false, full_screen: false };

    #[test]
    fn keyboard_and_mouse_alone_as_before() {
        assert_eq!(effective_idle_secs(0, None, 1_000_000), 0);
        assert_eq!(effective_idle_secs(301, None, 1_000_000), 301);
    }

    #[test]
    fn a_recent_sign_of_use_restarts_the_idle_clock() {
        let now = 10_000_000;
        // No input for 10 minutes, but a video was playing 4 s ago
        assert_eq!(effective_idle_secs(600, Some(now - 4_000), now), 4);
        // Input more recent than the last sign
        assert_eq!(effective_idle_secs(2, Some(now - 60_000), now), 2);
        // The last sign was 6 minutes ago: idle for 6 minutes
        assert_eq!(effective_idle_secs(900, Some(now - 360_000), now), 360);
        // A sign "in the future" (can't happen with one tick source) reads as now
        assert_eq!(effective_idle_secs(900, Some(now + 5_000), now), 0);
    }

    #[test]
    fn each_sign_counts_on_its_own() {
        for signals in [
            Signals { display_required: true, ..NONE },
            Signals { controller: true, ..NONE },
            Signals { full_screen: true, ..NONE },
        ] {
            let mut clock = ActivityClock::new();
            clock.observe(5_000, signals);
            assert_eq!(clock.last_signal_ms(), Some(5_000), "{signals:?}");
        }
        let mut clock = ActivityClock::new();
        clock.observe(5_000, NONE);
        assert_eq!(clock.last_signal_ms(), None);
    }

    /// Feed `pattern` (true = sound heard) one sample a second from 1 s on.
    fn hear(clock: &mut ActivityClock, pattern: &[bool]) {
        for (i, &heard) in pattern.iter().enumerate() {
            clock.observe((i as u64 + 1) * 1_000, if heard { SOUND } else { NONE });
        }
    }

    #[test]
    fn a_short_beep_is_not_use() {
        // The app's own warning sound: a second or two of sound
        let mut clock = ActivityClock::new();
        hear(&mut clock, &[true, true, false, false, false, false]);
        assert_eq!(clock.last_signal_ms(), None);
        // Two beeps a few seconds apart (a warning, then a wrong passcode)
        let mut clock = ActivityClock::new();
        hear(&mut clock, &[true, true, false, false, false, true, false, false]);
        assert_eq!(clock.last_signal_ms(), None);
    }

    #[test]
    fn lasting_sound_is_use() {
        let mut clock = ActivityClock::new();
        hear(&mut clock, &[true, true, true]);
        assert_eq!(clock.last_signal_ms(), Some(3_000));
        hear(&mut clock, &[true, true, true, true]);
        assert_eq!(clock.last_signal_ms(), Some(4_000));
    }

    #[test]
    fn sound_with_quiet_moments_is_use() {
        // A looping chime as sampled on this PC: heard, heard, quiet, heard, heard
        let mut clock = ActivityClock::new();
        hear(&mut clock, &[true, true, false, true]);
        assert_eq!(clock.last_signal_ms(), Some(4_000), "counts from the third ring");
        let mut clock = ActivityClock::new();
        hear(&mut clock, &[true, true, false, true, true]);
        assert_eq!(clock.last_signal_ms(), Some(5_000));
        // Speech with pauses
        let mut clock = ActivityClock::new();
        hear(&mut clock, &[true, false, true, false, true]);
        assert_eq!(clock.last_signal_ms(), Some(5_000));
    }

    #[test]
    fn sound_that_stopped_is_not_use_now() {
        let mut clock = ActivityClock::new();
        hear(&mut clock, &[true, true, true, true, false, false]);
        // The last sign is the last sample that heard it
        assert_eq!(clock.last_signal_ms(), Some(4_000));
    }

    #[test]
    fn reset_forgets_the_last_sign() {
        let mut clock = ActivityClock::new();
        clock.observe(1_000, Signals { full_screen: true, ..NONE });
        clock.observe(2_000, SOUND);
        clock.observe(3_000, SOUND);
        clock.reset();
        assert_eq!(clock, ActivityClock::new());
        // What was heard before starts over too
        clock.observe(4_000, SOUND);
        assert_eq!(clock.last_signal_ms(), None);
    }

    #[test]
    fn silent_and_muted_streams_are_not_sound() {
        assert!(!peak_is_sound(0.0));
        assert!(!peak_is_sound(0.001));
        assert!(!peak_is_sound(f32::NAN));
        assert!(!peak_is_sound(-1.0));
        assert!(peak_is_sound(0.01));
        assert!(peak_is_sound(1.0));
    }

    #[test]
    fn buttons_and_triggers_count() {
        let rest = Gamepad::default();
        assert!(!gamepad_used(&rest, &rest));
        assert!(gamepad_used(&rest, &Gamepad { buttons: 0x1000, ..rest }), "A pressed");
        assert!(gamepad_used(&Gamepad { buttons: 0x1000, ..rest }, &rest), "A released");
        assert!(gamepad_used(&rest, &Gamepad { right_trigger: 200, ..rest }));
        assert!(!gamepad_used(&rest, &Gamepad { left_trigger: 10, ..rest }), "trigger noise");
    }

    #[test]
    fn stick_drift_is_not_use() {
        let drifting = Gamepad { thumb_lx: 3000, thumb_ly: -2500, ..Gamepad::default() };
        let jitter = Gamepad { thumb_lx: 3400, thumb_ly: -2100, ..drifting };
        assert!(!gamepad_used(&drifting, &jitter));
        // A stick held still at full tilt isn't movement either
        let held = Gamepad { thumb_rx: 32767, ..Gamepad::default() };
        assert!(!gamepad_used(&held, &held));
    }

    #[test]
    fn stick_movement_is_use() {
        let rest = Gamepad::default();
        assert!(gamepad_used(&rest, &Gamepad { thumb_lx: 20000, ..rest }));
        assert!(gamepad_used(&rest, &Gamepad { thumb_ry: -9000, ..rest }));
        assert!(gamepad_used(&Gamepad { thumb_lx: i16::MIN, ..rest }, &Gamepad { thumb_lx: i16::MAX, ..rest }));
    }

    #[test]
    fn gamepad_layout_matches_xinput() {
        // XINPUT_GAMEPAD is 12 bytes, XINPUT_STATE 16.
        assert_eq!(std::mem::size_of::<Gamepad>(), 12);
        assert_eq!(std::mem::size_of::<XInputState>(), 16);
    }
}
