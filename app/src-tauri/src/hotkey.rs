//! The talk key: a pure press/release state machine shared by the global shortcut and the
//! macOS fn / modifier-key event tap, plus the tap itself and the Globe-key check.

use std::time::{Duration, Instant};

// ponytail: fixed timings; a Settings slider if people ask for slower double-taps.
/// A hold shorter than this is a tap: it may become a double-tap lock, never a dictation.
pub const TAP: Duration = Duration::from_millis(300);
/// A second press this soon after a tap locks the recording hands-free.
pub const DOUBLE_TAP: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ev {
    /// The shortcut went down: the mic opens at once.
    Press,
    /// The talk key (fn or a single modifier) went down alone. It counts once it has been held
    /// alone until `HoldReady`, or as a clean tap: two of them within DOUBLE_TAP go hands-free.
    ModPress,
    /// The shortcut or talk key went up.
    Release,
    /// The talk key press `seq` is still down, alone, after the hold delay (lib.rs SHOW_AFTER).
    HoldReady(u64),
    /// Pill click, tray item or the window's button: start hands-free, or finish.
    Toggle,
    /// Esc or ✕.
    Cancel,
    /// Another key went down while the talk key was held (fn+arrow, Right ⌘+C).
    OtherKey,
    /// A mouse click while the talk key was held (Right ⌘-click).
    Click,
    /// Recording `id` hit the 30-minute cap.
    AutoStop(u64),
    /// The double-tap window of recording `id`, tap `seq`, ran out.
    TapExpired(u64, u64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Act {
    None,
    /// Open the mic for recording `id`.
    Start { id: u64, locked: bool },
    /// Close the mic and transcribe.
    Finish,
    /// Close the mic and drop the audio; `hint` shows "Hold <key> to dictate".
    Discard { hint: bool },
    /// A double-tap locked the recording hands-free (lock sound).
    Lock,
    /// Send `TapExpired(id, seq)` after DOUBLE_TAP.
    ArmTap { id: u64, seq: u64 },
    /// Send `HoldReady(seq)` after the hold delay.
    ArmHold { seq: u64 },
}

#[derive(Debug, Clone, Copy)]
struct Rec {
    id: u64,
    started: Instant,
    /// When the talk-key press that started or locked it went down (the click-chord window).
    pressed: Instant,
    /// Hands-free: the next press finishes. Toggle mode and click-started recordings are always locked.
    locked: bool,
    /// The release of the press that started or locked it is not a "let go to finish".
    ignore_release: bool,
    /// Released early in hold mode: waiting DOUBLE_TAP for a second press.
    tap: Option<u64>,
    seq: u64,
    /// Started (or locked) by the talk-key press that is still down, so another key undoes it (fn+arrow).
    by_press: bool,
    /// The talk key went down on a hands-free recording: it finishes when the key comes up,
    /// unless another key turns the press into a chord (Right ⌘+V must not end a dictation).
    finishing: bool,
}

/// A talk-key press that has not started anything yet.
#[derive(Debug, Clone, Copy)]
struct Pending {
    seq: u64,
    pressed: Instant,
    /// Another key or a click joined (⌃C, ⌘-click): the press is a shortcut, not a talk key.
    chord: bool,
}

#[derive(Debug, Default)]
pub struct Fsm {
    down: bool,
    rec: Option<Rec>,
    next_id: u64,
    pending: Option<Pending>,
    presses: u64,
    /// When the last clean quick tap came up, for the double-tap.
    last_tap: Option<Instant>,
}

impl Fsm {
    #[cfg(test)]
    pub fn recording(&self) -> bool {
        self.rec.is_some()
    }

    /// The mic could not open: forget the recording that never started.
    pub fn start_failed(&mut self) {
        self.rec = None;
    }

    /// `busy`: a transcription is running, so nothing new may start.
    pub fn step(&mut self, ev: Ev, now: Instant, busy: bool, toggle_mode: bool) -> Act {
        match ev {
            Ev::Press | Ev::ModPress => {
                if self.down {
                    return Act::None; // key repeat, or a second talk key while one is held
                }
                self.down = true;
                match &mut self.rec {
                    None if busy => Act::None,
                    // A modifier opens nothing yet: ⌃C or ⌘-click must not touch the mic.
                    None if ev == Ev::ModPress => {
                        self.presses += 1;
                        self.pending = Some(Pending { seq: self.presses, pressed: now, chord: false });
                        Act::ArmHold { seq: self.presses }
                    }
                    None => self.start(now, toggle_mode, toggle_mode, true),
                    Some(r) if r.tap.is_some() => {
                        r.tap = None;
                        r.locked = true;
                        r.ignore_release = true;
                        r.by_press = true;
                        r.pressed = now;
                        Act::Lock
                    }
                    Some(r) if r.locked => {
                        r.finishing = true;
                        Act::None
                    }
                    Some(_) => Act::None,
                }
            }
            Ev::Release => {
                self.down = false;
                if let Some(p) = self.pending.take() {
                    let tap = self.last_tap.take();
                    if p.chord || busy || self.rec.is_some() {
                        return Act::None;
                    }
                    if toggle_mode {
                        return self.start(now, true, false, false);
                    }
                    // A clean quick tap: the first does nothing, a second one this soon goes hands-free.
                    return match tap {
                        Some(t) if p.pressed.duration_since(t) <= DOUBLE_TAP => self.start(now, true, false, false),
                        _ => {
                            self.last_tap = Some(now);
                            Act::None
                        }
                    };
                }
                let Some(r) = &mut self.rec else { return Act::None };
                r.by_press = false;
                if r.finishing {
                    self.rec = None;
                    return Act::Finish;
                }
                if r.ignore_release {
                    r.ignore_release = false;
                    return Act::None;
                }
                if r.locked || r.tap.is_some() {
                    return Act::None;
                }
                if now.duration_since(r.started) >= TAP {
                    self.rec = None;
                    return Act::Finish;
                }
                r.seq += 1;
                r.tap = Some(r.seq);
                Act::ArmTap { id: r.id, seq: r.seq }
            }
            Ev::Toggle => match self.rec {
                Some(_) => {
                    self.rec = None;
                    Act::Finish
                }
                None if busy => Act::None,
                None => self.start(now, true, false, false),
            },
            Ev::HoldReady(seq) => match self.pending {
                Some(p) if p.seq == seq && !p.chord && !busy && self.rec.is_none() => {
                    self.pending = None;
                    // Timed from the press, so a release before TAP is still a tap (double-tap lock).
                    self.start(p.pressed, toggle_mode, toggle_mode, true)
                }
                _ => Act::None,
            },
            Ev::OtherKey | Ev::Click if self.pending.is_some() => {
                if let Some(p) = &mut self.pending {
                    p.chord = true;
                }
                Act::None
            }
            Ev::Cancel => match self.rec.take() {
                Some(_) => Act::Discard { hint: false },
                None => Act::None,
            },
            Ev::OtherKey => match &mut self.rec {
                Some(r) if self.down && r.finishing => {
                    r.finishing = false; // a chord, not a finish: keep recording, ignore its release
                    r.ignore_release = true;
                    Act::None
                }
                Some(r) if self.down && r.by_press => {
                    self.rec = None;
                    Act::Discard { hint: false }
                }
                _ => Act::None,
            },
            // A modifier-click is a chord only right after the key went down; later, someone
            // dictating with the key held may click around, and that must not drop their words.
            Ev::Click => match &mut self.rec {
                Some(r) if self.down && r.finishing => {
                    r.finishing = false; // Right ⌘-click during a hands-free recording: a chord, keep recording
                    r.ignore_release = true;
                    Act::None
                }
                Some(r) if self.down && r.by_press && now.duration_since(r.pressed) < TAP => {
                    self.rec = None;
                    Act::Discard { hint: false }
                }
                _ => Act::None,
            },
            Ev::AutoStop(id) => match self.rec {
                Some(r) if r.id == id => {
                    self.rec = None;
                    Act::Finish
                }
                _ => Act::None,
            },
            Ev::TapExpired(id, seq) => match self.rec {
                Some(r) if r.id == id && r.tap == Some(seq) => {
                    self.rec = None;
                    Act::Discard { hint: true }
                }
                _ => Act::None,
            },
        }
    }

    fn start(&mut self, now: Instant, locked: bool, ignore_release: bool, by_press: bool) -> Act {
        self.next_id += 1;
        self.rec = Some(Rec { id: self.next_id, started: now, pressed: now, locked, ignore_release, tap: None, seq: 0, by_press, finishing: false });
        Act::Start { id: self.next_id, locked }
    }
}

// ---------- fn / modifier keys ----------

/// The keys that can be a talk key on their own: setting value, macOS virtual keycode, the key's
/// own bit in CGEventFlags (NX_DEVICE*KEYMASK per side, SecondaryFn for fn), label.
// ponytail: no Caps Lock. It is a hardware toggle: macOS sends one flagsChanged per press with the
// lock state, not a down/up pair, so there is no hold to time. A HID-level IOHIDManager watch if asked.
const KEYS: [(&str, u16, u64, &str); 9] = [
    ("fn", 63, 0x0080_0000, "fn"),
    ("left_control", 59, 0x0000_0001, "Left ⌃"),
    ("right_control", 62, 0x0000_2000, "Right ⌃"),
    ("left_option", 58, 0x0000_0020, "Left ⌥"),
    ("right_option", 61, 0x0000_0040, "Right ⌥"),
    ("left_command", 55, 0x0000_0008, "Left ⌘"),
    ("right_command", 54, 0x0000_0010, "Right ⌘"),
    ("left_shift", 56, 0x0000_0002, "Left ⇧"),
    ("right_shift", 60, 0x0000_0004, "Right ⇧"),
];
/// Every per-side ⌃ ⌥ ⌘ ⇧ bit: one of them besides the talk key's own means it is not alone.
const SIDE_BITS: u64 = 0x207F;

pub fn keycode(talk_key: &str) -> Option<u16> {
    KEYS.iter().find(|k| k.0 == talk_key).map(|k| k.1)
}

/// "Right ⌘", "fn": how copy names the talk key.
pub fn label(talk_key: &str) -> Option<&'static str> {
    KEYS.iter().find(|k| k.0 == talk_key).map(|k| k.3)
}

/// The talk key on a macOS keycode (59: "left_control"), for importing another app's shortcut.
pub fn talk_key_for(code: u16) -> Option<&'static str> {
    KEYS.iter().find(|k| k.1 == code).map(|k| k.0)
}

/// `label`, or the setting value itself.
pub fn talk_key_label(talk_key: &str) -> &str {
    label(talk_key).unwrap_or(talk_key)
}

fn down_bit(code: u16) -> u64 {
    KEYS.iter().find(|k| k.1 == code).map_or(0, |k| k.2)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TapEv {
    Down,
    Up,
    Other,
    Click,
}

/// What one tapped event means for talk key `code`. The key is identified by its keycode on a
/// flagsChanged event, never by a flag: arrow keys carry SecondaryFn too, and fn+arrow must
/// not start dictation. Direction comes from the key's own device-side bit. Another modifier
/// changing is another key (Left ⌘ then ⇧ is a shortcut), and the talk key going down while
/// another modifier is held is not a press at all (⇧ held, then ⌘).
pub fn classify(flags_changed: bool, event_keycode: u16, flags: u64, code: u16) -> Option<TapEv> {
    if !flags_changed || event_keycode != code {
        return Some(TapEv::Other); // a keyDown, or another modifier
    }
    let bit = down_bit(code);
    match (flags & bit != 0, flags & SIDE_BITS & !bit != 0) {
        (false, _) => Some(TapEv::Up),
        (true, false) => Some(TapEv::Down),
        (true, true) => None,
    }
}

/// `defaults read com.apple.HIToolbox AppleFnUsageType` output: fn is usable as a talk key only
/// when "Press 🌐 key to" is "Do Nothing" (0). Absent counts as not set: macOS then shows emoji
/// or starts Dictation on every press.
pub fn globe_does_nothing(defaults_output: Option<&str>) -> bool {
    defaults_output.is_some_and(|o| o.trim() == "0")
}

pub fn read_globe_setting() -> bool {
    let out = std::process::Command::new("defaults").args(["read", "com.apple.HIToolbox", "AppleFnUsageType"]).output();
    let text = out.ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    globe_does_nothing(text.as_deref())
}

#[cfg(target_os = "macos")]
pub mod tap {
    //! Listen-only CGEventTap for flagsChanged + keyDown on its own CFRunLoop thread. The
    //! callback only turns events into talk-key signals; no key data is stored or logged.
    use super::{TapEv, classify};
    use core_foundation::base::TCFType;
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes, kCFRunLoopDefaultMode};
    use core_graphics::event::{CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType, CallbackResult, EventField};
    use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    unsafe extern "C" {
        fn CGPreflightListenEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
        fn CGEventTapEnable(tap: *mut std::ffi::c_void, enable: bool);
    }

    /// Input Monitoring granted (no prompt).
    pub fn allowed() -> bool {
        // SAFETY: no arguments; reads this process's TCC state.
        unsafe { CGPreflightListenEventAccess() }
    }

    /// Shows the system prompt once and lists Echotype under Input Monitoring.
    pub fn request() -> bool {
        // SAFETY: as above.
        unsafe { CGRequestListenEventAccess() }
    }

    /// Keycode the running tap watches; 0 = none (fn chosen but the Globe key still acts).
    static CODE: AtomicU16 = AtomicU16::new(0);
    /// Generation of the running tap; 0 = none. A tap thread exits once its generation is no
    /// longer current, so stopping needs no pointer to another thread's run loop.
    static RUNNING: Mutex<u64> = Mutex::new(0);
    static NEXT: AtomicU64 = AtomicU64::new(1);

    pub fn active() -> bool {
        *RUNNING.lock().unwrap_or_else(|e| e.into_inner()) != 0
    }

    /// Watches `code` (None removes the tap). Installs the tap only when it is not running yet,
    /// so switching between talk keys needs no reinstall. Err when macOS refuses
    /// the tap (Input Monitoring off, or granted but not yet effective until a relaunch).
    pub fn set(code: Option<u16>, send: impl Fn(TapEv) + Send + Sync + 'static) -> Result<(), String> {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        CODE.store(code.unwrap_or(0), Ordering::SeqCst);
        if code.is_none() {
            if *running != 0 {
                *running = 0; // the tap thread sees this within half a second and exits
                eprintln!("echotype: talk-key tap removed");
            }
            return Ok(());
        }
        if *running != 0 {
            return Ok(());
        }
        let generation = NEXT.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        std::thread::spawn(move || {
            let port = Arc::new(AtomicUsize::new(0));
            let held = AtomicBool::new(false);
            let p = port.clone();
            let tap = CGEventTap::new(
                CGEventTapLocation::Session,
                CGEventTapPlacement::TailAppendEventTap,
                CGEventTapOptions::ListenOnly,
                // Clicks too, so a Right ⌘-click is not a dictation; scrolls are not (trackpad momentum
                // keeps arriving after the finger lifts).
                vec![CGEventType::FlagsChanged, CGEventType::KeyDown, CGEventType::LeftMouseDown, CGEventType::RightMouseDown, CGEventType::OtherMouseDown],
                move |_, ty, ev| {
                    match ty {
                        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                            let raw = p.load(Ordering::SeqCst);
                            if raw != 0 {
                                // SAFETY: the mach port outlives the run loop that calls this callback.
                                unsafe { CGEventTapEnable(raw as *mut _, true) };
                            }
                        }
                        _ => {
                            let code = CODE.load(Ordering::SeqCst);
                            let flags_changed = matches!(ty, CGEventType::FlagsChanged);
                            let click = !flags_changed && !matches!(ty, CGEventType::KeyDown);
                            let kc = if flags_changed { ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16 } else { 0 };
                            let what = (code != 0).then(|| classify(flags_changed, kc, ev.get_flags().bits(), code)).flatten();
                            match what.map(|w| if click { TapEv::Click } else { w }) {
                                Some(TapEv::Down) if !held.swap(true, Ordering::SeqCst) => send(TapEv::Down),
                                Some(TapEv::Up) if held.swap(false, Ordering::SeqCst) => send(TapEv::Up),
                                // Only while the talk key is down, and only that it happened, not which key.
                                Some(e @ (TapEv::Other | TapEv::Click)) if held.load(Ordering::SeqCst) => send(e),
                                _ => {}
                            }
                        }
                    }
                    CallbackResult::Keep
                },
            );
            let Ok(tap) = tap else {
                let _ = tx.send(false);
                return;
            };
            port.store(tap.mach_port().as_concrete_TypeRef() as usize, Ordering::SeqCst);
            let Ok(src) = tap.mach_port().create_runloop_source(0) else {
                let _ = tx.send(false);
                return;
            };
            // SAFETY: kCFRunLoopCommonModes / kCFRunLoopDefaultMode are constant CFStrings.
            CFRunLoop::get_current().add_source(&src, unsafe { kCFRunLoopCommonModes });
            tap.enable();
            let _ = tx.send(true);
            while *RUNNING.lock().unwrap_or_else(|e| e.into_inner()) == generation {
                CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, std::time::Duration::from_millis(500), false);
            }
            drop(tap); // invalidates the mach port
        });
        *running = generation; // before the thread's first check, so it does not exit at once
        match rx.recv() {
            Ok(true) => {
                eprintln!("echotype: talk-key tap installed");
                Ok(())
            }
            _ => {
                *running = 0;
                Err(if allowed() { "Relaunch Echotype to finish turning on the talk key".into() } else { "Echotype needs Input Monitoring for this talk key".into() })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct T {
        f: Fsm,
        t0: Instant,
        busy: bool,
        toggle: bool,
    }
    impl T {
        fn hold() -> Self {
            T { f: Fsm::default(), t0: Instant::now(), busy: false, toggle: false }
        }
        fn toggle() -> Self {
            T { toggle: true, ..Self::hold() }
        }
        fn at(&mut self, ms: u64, ev: Ev) -> Act {
            self.f.step(ev, self.t0 + Duration::from_millis(ms), self.busy, self.toggle)
        }
    }

    // ----- regression contract: what lib.rs did before the FSM -----

    #[test]
    fn hold_press_release_dictates() {
        let mut t = T::hold();
        assert_eq!(t.at(0, Ev::Press), Act::Start { id: 1, locked: false });
        assert_eq!(t.at(1200, Ev::Release), Act::Finish);
        assert!(!t.f.recording());
    }

    #[test]
    fn toggle_press_press_dictates_and_releases_are_ignored() {
        let mut t = T::toggle();
        assert_eq!(t.at(0, Ev::Press), Act::Start { id: 1, locked: true });
        assert_eq!(t.at(80, Ev::Release), Act::None);
        assert_eq!(t.at(2000, Ev::Press), Act::None, "the second press finishes when it comes up");
        assert_eq!(t.at(2100, Ev::Release), Act::Finish);
        // A quick toggle tap simply starts recording; no hint, no tap window.
        assert_eq!(t.at(3000, Ev::Press), Act::Start { id: 2, locked: true });
        assert_eq!(t.at(3050, Ev::Release), Act::None);
    }

    #[test]
    fn esc_cancels_hold_toggle_and_locked_recordings() {
        let mut t = T::hold();
        t.at(0, Ev::Press);
        assert_eq!(t.at(500, Ev::Cancel), Act::Discard { hint: false });
        assert_eq!(t.at(600, Ev::Release), Act::None);
        assert_eq!(t.at(700, Ev::Cancel), Act::None);
        t.at(1000, Ev::Toggle);
        assert_eq!(t.at(5000, Ev::Cancel), Act::Discard { hint: false });
    }

    #[test]
    fn auto_stop_finishes_only_its_own_recording() {
        let mut t = T::hold();
        t.at(0, Ev::Toggle); // id 1
        assert_eq!(t.at(10, Ev::AutoStop(1)), Act::Finish);
        t.at(20, Ev::Toggle); // id 2
        assert_eq!(t.at(30, Ev::AutoStop(1)), Act::None, "a stale auto-stop");
        assert_eq!(t.at(40, Ev::AutoStop(2)), Act::Finish);
    }

    #[test]
    fn busy_transcription_ignores_presses_and_toggles() {
        let mut t = T::hold();
        t.busy = true;
        assert_eq!(t.at(0, Ev::Press), Act::None);
        assert_eq!(t.at(500, Ev::Release), Act::None);
        assert_eq!(t.at(600, Ev::Toggle), Act::None);
        t.busy = false;
        assert_eq!(t.at(700, Ev::Press), Act::Start { id: 1, locked: false });
    }

    // ----- taps, double-tap lock, X3 hint -----

    #[test]
    fn a_short_hold_tap_waits_then_discards_with_the_hint() {
        let mut t = T::hold();
        t.at(0, Ev::Press);
        assert_eq!(t.at(299, Ev::Release), Act::ArmTap { id: 1, seq: 1 });
        assert!(t.f.recording(), "keeps recording through the double-tap window");
        assert_eq!(t.at(799, Ev::TapExpired(1, 1)), Act::Discard { hint: true });
    }

    #[test]
    fn exactly_three_hundred_ms_is_a_hold_not_a_tap() {
        let mut t = T::hold();
        t.at(0, Ev::Press);
        assert_eq!(t.at(300, Ev::Release), Act::Finish);
    }

    #[test]
    fn double_tap_locks_and_ignores_the_locking_release() {
        let mut t = T::hold();
        t.at(0, Ev::Press);
        t.at(100, Ev::Release);
        assert_eq!(t.at(450, Ev::Press), Act::Lock);
        assert_eq!(t.at(500, Ev::Release), Act::None, "release of the locking press");
        assert_eq!(t.at(600, Ev::TapExpired(1, 1)), Act::None, "stale expiry after the second press");
        assert!(t.f.recording());
        assert_eq!(t.at(9000, Ev::Press), Act::None, "the next press finishes...");
        assert_eq!(t.at(9100, Ev::Release), Act::Finish, "...on its release");
        assert_eq!(t.at(9200, Ev::Press), Act::Start { id: 2, locked: false });
    }

    #[test]
    fn the_double_tap_window_is_the_sleeper_not_the_clock() {
        // The FSM has no clock for the window: a second press before TapExpired arrives locks,
        // one after the expiry starts a new recording.
        let mut t = T::hold();
        t.at(0, Ev::Press);
        t.at(100, Ev::Release);
        assert_eq!(t.at(600, Ev::TapExpired(1, 1)), Act::Discard { hint: true });
        assert_eq!(t.at(601, Ev::Press), Act::Start { id: 2, locked: false });
    }

    #[test]
    fn locked_recordings_end_with_check_cancel_and_auto_stop() {
        let mut t = T::hold();
        let lock = |t: &mut T, at: u64| {
            t.at(at, Ev::Press);
            t.at(at + 50, Ev::Release);
            t.at(at + 200, Ev::Press);
            t.at(at + 250, Ev::Release);
        };
        lock(&mut t, 0);
        assert_eq!(t.at(1000, Ev::Toggle), Act::Finish, "pill ✓");
        lock(&mut t, 2000);
        assert_eq!(t.at(3000, Ev::Cancel), Act::Discard { hint: false });
        lock(&mut t, 4000);
        assert_eq!(t.at(5000, Ev::AutoStop(3)), Act::Finish);
    }

    #[test]
    fn click_started_recordings_count_as_locked() {
        let mut t = T::hold();
        assert_eq!(t.at(0, Ev::Toggle), Act::Start { id: 1, locked: true });
        assert_eq!(t.at(100, Ev::Toggle), Act::Finish, "a quick second click finishes, no hint");
        t.at(200, Ev::Toggle);
        assert_eq!(t.at(5000, Ev::Press), Act::None);
        assert_eq!(t.at(5100, Ev::Release), Act::Finish, "the talk key finishes a clicked recording");
        assert!(!t.f.recording());
    }

    #[test]
    fn a_press_while_the_key_is_down_is_ignored() {
        let mut t = T::hold();
        t.at(0, Ev::Press);
        assert_eq!(t.at(50, Ev::Press), Act::None, "key repeat");
        assert_eq!(t.at(100, Ev::Press), Act::None);
        assert_eq!(t.at(900, Ev::Release), Act::Finish);
    }

    #[test]
    fn talk_key_plus_another_key_cancels_silently() {
        let mut t = T::hold();
        t.at(0, Ev::Press);
        assert_eq!(t.at(120, Ev::OtherKey), Act::Discard { hint: false });
        assert_eq!(t.at(200, Ev::Release), Act::None);
        // A tap, then a chord (Right ⌘ alone, then Right ⌘+V): not a double-tap lock.
        t.at(1000, Ev::Press);
        t.at(1100, Ev::Release);
        assert_eq!(t.at(1300, Ev::Press), Act::Lock);
        assert_eq!(t.at(1350, Ev::OtherKey), Act::Discard { hint: false });
        assert_eq!(t.at(1400, Ev::Release), Act::None);
        assert!(!t.f.recording());
        // Typing during a locked recording (key not held) changes nothing.
        t.at(1000, Ev::Toggle);
        assert_eq!(t.at(1100, Ev::OtherKey), Act::None);
        assert!(t.f.recording());
    }

    #[test]
    fn a_click_in_the_tap_window_finishes_and_a_failed_start_resets() {
        let mut t = T::hold();
        t.at(0, Ev::Press);
        t.at(100, Ev::Release); // tap window open
        assert_eq!(t.at(200, Ev::Toggle), Act::Finish, "the pill ✓ / tray finishes, the clip is not dropped");
        assert_eq!(t.at(700, Ev::TapExpired(1, 1)), Act::None);
        assert_eq!(t.at(800, Ev::Press), Act::Start { id: 2, locked: false });
        t.f.start_failed(); // the mic did not open
        assert_eq!(t.at(900, Ev::Release), Act::None);
        assert_eq!(t.at(1000, Ev::Press), Act::Start { id: 3, locked: false }, "nothing is stuck");
        t.at(1050, Ev::Release);
        assert_eq!(t.at(1600, Ev::TapExpired(2, 1)), Act::None, "an expiry for another recording");
        assert_eq!(t.at(1601, Ev::TapExpired(3, 1)), Act::Discard { hint: true });
    }

    #[test]
    fn a_chord_during_a_hands_free_recording_does_not_end_it() {
        let mut t = T::hold();
        t.at(0, Ev::Toggle); // hands-free
        assert_eq!(t.at(3000, Ev::Press), Act::None); // Right ⌘ down...
        assert_eq!(t.at(3050, Ev::OtherKey), Act::None); // ...+V: a paste, not a finish
        assert_eq!(t.at(3100, Ev::Release), Act::None);
        assert!(t.f.recording());
        t.at(5000, Ev::Press);
        assert_eq!(t.at(5100, Ev::Release), Act::Finish);
    }

    #[test]
    fn a_click_cancels_only_right_after_the_press() {
        let mut t = T::hold();
        t.at(0, Ev::Press);
        assert_eq!(t.at(120, Ev::Click), Act::Discard { hint: false }, "Right ⌘-click");
        t.at(150, Ev::Release);
        t.at(1000, Ev::Press);
        assert_eq!(t.at(5000, Ev::Click), Act::None, "clicking around while dictating keeps the words");
        assert_eq!(t.at(9000, Ev::Release), Act::Finish);
        // A Right ⌘-click during a hands-free recording is a chord: the recording keeps going.
        t.at(10_000, Ev::Toggle);
        t.at(20_000, Ev::Press);
        assert_eq!(t.at(20_050, Ev::Click), Act::None);
        assert_eq!(t.at(20_100, Ev::Release), Act::None);
        assert!(t.f.recording());
        // A tap, then Right ⌘-click: the click is timed from the lock press, so it is a chord.
        let mut t = T::hold();
        t.at(0, Ev::Press);
        t.at(100, Ev::Release);
        assert_eq!(t.at(450, Ev::Press), Act::Lock);
        assert_eq!(t.at(500, Ev::Click), Act::Discard { hint: false });
    }

    #[test]
    fn toggle_mode_chords_cancel_too() {
        // fn+arrow in toggle mode: the fn press starts a (hands-free) recording, the arrow cancels it.
        let mut t = T::toggle();
        assert_eq!(t.at(0, Ev::Press), Act::Start { id: 1, locked: true });
        assert_eq!(t.at(80, Ev::OtherKey), Act::Discard { hint: false });
        assert_eq!(t.at(120, Ev::Release), Act::None);
        // Once the key is up, typing during the recording is fine.
        t.at(1000, Ev::Press);
        t.at(1100, Ev::Release);
        assert_eq!(t.at(1200, Ev::OtherKey), Act::None);
    }

    #[test]
    fn fn_is_keycode_63_not_the_secondary_fn_flag() {
        const SECONDARY_FN: u64 = 0x80_0000;
        let fn_code = keycode("fn").unwrap();
        assert_eq!(classify(true, 63, SECONDARY_FN, fn_code), Some(TapEv::Down));
        assert_eq!(classify(true, 63, 0, fn_code), Some(TapEv::Up));
        // An arrow keyDown carries SecondaryFn: it is "another key", never a press.
        assert_eq!(classify(false, 126, SECONDARY_FN, fn_code), Some(TapEv::Other));
        // Left Option (58) changing flags is not the right-Option talk key: it is another key.
        let ropt = keycode("right_option").unwrap();
        assert_eq!(classify(true, 58, 0x20 | 0x80000, ropt), Some(TapEv::Other));
        assert_eq!(classify(true, 61, 0x40 | 0x80000, ropt), Some(TapEv::Down));
        // Right Option released while Left Option is still held: the device bit is clear.
        assert_eq!(classify(true, 61, 0x20 | 0x80000, ropt), Some(TapEv::Up));
        assert_eq!(keycode("right_command"), Some(54));
        assert_eq!(keycode("right_control"), Some(62));
        assert_eq!(keycode("accelerator"), None);
    }

    #[test]
    fn every_side_of_every_modifier_by_keycode_and_device_bit() {
        // (setting, keycode, NX_DEVICE*KEYMASK, device-independent mask)
        for (key, code, bit, mask) in [
            ("left_control", 59, 0x01, 0x40000),
            ("right_control", 62, 0x2000, 0x40000),
            ("left_shift", 56, 0x02, 0x20000),
            ("right_shift", 60, 0x04, 0x20000),
            ("left_command", 55, 0x08, 0x100000),
            ("right_command", 54, 0x10, 0x100000),
            ("left_option", 58, 0x20, 0x80000),
            ("right_option", 61, 0x40, 0x80000),
        ] {
            assert_eq!(keycode(key), Some(code), "{key}");
            assert!(label(key).is_some_and(|l| l.starts_with(if key.starts_with("left") { "Left " } else { "Right " })));
            assert_eq!(classify(true, code, bit | mask, code), Some(TapEv::Down), "{key} down");
            assert_eq!(classify(true, code, 0, code), Some(TapEv::Up), "{key} up");
        }
        let (lcmd, rcmd) = (keycode("left_command").unwrap(), keycode("right_command").unwrap());
        // Left ⌘ released while Right ⌘ is still held: the shared ⌘ mask stays set, the side bit does not.
        assert_eq!(classify(true, lcmd, 0x10 | 0x100000, lcmd), Some(TapEv::Up));
        // Right ⌘ down while Left ⌘ is held is not "alone", and is never Left ⌘'s press.
        assert_eq!(classify(true, rcmd, 0x08 | 0x10 | 0x100000, rcmd), None);
        assert_eq!(classify(true, rcmd, 0x08 | 0x10 | 0x100000, lcmd), Some(TapEv::Other));
        // ⇧ held, then Left ⌘: a shortcut in the making, not a talk-key press.
        assert_eq!(classify(true, lcmd, 0x08 | 0x02 | 0x120000, lcmd), None);
        // ⌘ held, then ⇧ joins (⌘⇧4): another key.
        assert_eq!(classify(true, 56, 0x08 | 0x02 | 0x120000, lcmd), Some(TapEv::Other));
        // Caps Lock on (AlphaShift, no side bit) does not stop Left ⌃ from being alone.
        let lctl = keycode("left_control").unwrap();
        assert_eq!(classify(true, lctl, 0x01 | 0x40000 | 0x10000, lctl), Some(TapEv::Down));
        assert_eq!(label("fn"), Some("fn"));
        assert_eq!(label("caps_lock"), None);
    }

    // ----- a modifier talk key (ModPress): alone only, hold delay, clean double-tap -----

    #[test]
    fn modifier_held_alone_starts_after_the_hold_delay() {
        let mut t = T::hold();
        assert_eq!(t.at(0, Ev::ModPress), Act::ArmHold { seq: 1 });
        assert!(!t.f.recording(), "nothing opens on the press");
        assert_eq!(t.at(150, Ev::HoldReady(1)), Act::Start { id: 1, locked: false });
        assert_eq!(t.at(2000, Ev::Release), Act::Finish);
        assert!(!t.f.recording());
    }

    #[test]
    fn a_shortcut_with_the_modifier_never_starts() {
        let mut t = T::hold();
        t.at(0, Ev::ModPress); // ⌃ down...
        assert_eq!(t.at(60, Ev::OtherKey), Act::None, "...C: ⌃C");
        assert_eq!(t.at(150, Ev::HoldReady(1)), Act::None, "the hold delay finds a chord");
        assert_eq!(t.at(400, Ev::Release), Act::None);
        assert!(!t.f.recording());
        // ⌘-click.
        t.at(1000, Ev::ModPress);
        assert_eq!(t.at(1040, Ev::Click), Act::None);
        assert_eq!(t.at(1150, Ev::HoldReady(2)), Act::None);
        assert_eq!(t.at(1200, Ev::Release), Act::None);
        // Two chords in a row are not a double-tap.
        t.at(1300, Ev::ModPress);
        t.at(1320, Ev::OtherKey);
        assert_eq!(t.at(1350, Ev::Release), Act::None);
        assert!(!t.f.recording());
    }

    #[test]
    fn a_key_joining_after_the_hold_delay_drops_the_recording_silently() {
        let mut t = T::hold();
        t.at(0, Ev::ModPress);
        t.at(150, Ev::HoldReady(1));
        assert_eq!(t.at(400, Ev::OtherKey), Act::Discard { hint: false });
        assert_eq!(t.at(500, Ev::Release), Act::None);
        assert!(!t.f.recording());
    }

    #[test]
    fn a_single_quick_tap_does_nothing() {
        let mut t = T::hold();
        t.at(0, Ev::ModPress);
        assert_eq!(t.at(90, Ev::Release), Act::None, "no mic, no hint, no window to wait out");
        assert_eq!(t.at(150, Ev::HoldReady(1)), Act::None, "the delay of a released press");
        assert!(!t.f.recording());
        // Much later, another quick tap is a first tap again.
        t.at(5000, Ev::ModPress);
        assert_eq!(t.at(5080, Ev::Release), Act::None);
        assert!(!t.f.recording());
    }

    #[test]
    fn two_clean_quick_taps_go_hands_free_and_one_tap_ends_it() {
        let mut t = T::hold();
        t.at(0, Ev::ModPress);
        t.at(80, Ev::Release);
        assert_eq!(t.at(300, Ev::ModPress), Act::ArmHold { seq: 2 });
        assert_eq!(t.at(380, Ev::Release), Act::Start { id: 1, locked: true }, "starts on the second clean release");
        assert_eq!(t.at(450, Ev::HoldReady(2)), Act::None);
        assert!(t.f.recording());
        // Typing and shortcuts while hands-free keep the recording (the key is up).
        assert_eq!(t.at(2000, Ev::OtherKey), Act::None);
        // ⌃C during the recording: a chord, not the stop.
        assert_eq!(t.at(3000, Ev::ModPress), Act::None);
        assert_eq!(t.at(3050, Ev::OtherKey), Act::None);
        assert_eq!(t.at(3100, Ev::Release), Act::None);
        assert!(t.f.recording());
        // One clean tap finishes.
        assert_eq!(t.at(9000, Ev::ModPress), Act::None);
        assert_eq!(t.at(9080, Ev::Release), Act::Finish);
        // The second tap of a "stop" double-tap is just a first tap.
        t.at(9200, Ev::ModPress);
        assert_eq!(t.at(9280, Ev::Release), Act::None);
        assert!(!t.f.recording());
    }

    #[test]
    fn the_double_tap_window_runs_from_the_first_release_to_the_second_press() {
        let mut t = T::hold();
        t.at(0, Ev::ModPress);
        t.at(80, Ev::Release);
        t.at(580, Ev::ModPress); // exactly DOUBLE_TAP after the release
        assert_eq!(t.at(640, Ev::Release), Act::Start { id: 1, locked: true });
        let mut t = T::hold();
        t.at(0, Ev::ModPress);
        t.at(80, Ev::Release);
        t.at(581, Ev::ModPress); // too slow: a first tap again...
        assert_eq!(t.at(640, Ev::Release), Act::None);
        t.at(900, Ev::ModPress); // ...so this one pairs with it
        assert_eq!(t.at(960, Ev::Release), Act::Start { id: 1, locked: true });
    }

    #[test]
    fn a_tap_then_a_shortcut_is_not_a_double_tap() {
        let mut t = T::hold();
        t.at(0, Ev::ModPress);
        t.at(80, Ev::Release);
        t.at(200, Ev::ModPress);
        t.at(230, Ev::OtherKey); // ⌘ tap, then ⌘V
        assert_eq!(t.at(260, Ev::Release), Act::None);
        t.at(400, Ev::ModPress); // the chord broke the pair
        assert_eq!(t.at(460, Ev::Release), Act::None);
        assert!(!t.f.recording());
    }

    #[test]
    fn a_medium_tap_after_the_delay_still_locks_on_the_next_press() {
        // Held past the delay but let go before TAP: the existing tap window and lock.
        let mut t = T::hold();
        t.at(0, Ev::ModPress);
        assert_eq!(t.at(150, Ev::HoldReady(1)), Act::Start { id: 1, locked: false });
        assert_eq!(t.at(250, Ev::Release), Act::ArmTap { id: 1, seq: 1 }, "TAP counts from the press, not the start");
        assert_eq!(t.at(500, Ev::ModPress), Act::Lock);
        assert_eq!(t.at(560, Ev::Release), Act::None);
        assert!(t.f.recording());
        // Alone, the medium tap ends with the hint like any tap.
        let mut t = T::hold();
        t.at(0, Ev::ModPress);
        t.at(150, Ev::HoldReady(1));
        t.at(250, Ev::Release);
        assert_eq!(t.at(750, Ev::TapExpired(1, 1)), Act::Discard { hint: true });
    }

    #[test]
    fn modifier_in_toggle_mode_starts_on_a_clean_tap_or_hold_and_stops_on_the_next() {
        let mut t = T::toggle();
        t.at(0, Ev::ModPress);
        assert_eq!(t.at(80, Ev::Release), Act::Start { id: 1, locked: true });
        assert_eq!(t.at(2000, Ev::ModPress), Act::None);
        assert_eq!(t.at(2080, Ev::Release), Act::Finish);
        // Held: starts at the delay, its own release is ignored.
        t.at(3000, Ev::ModPress);
        assert_eq!(t.at(3150, Ev::HoldReady(2)), Act::Start { id: 2, locked: true });
        assert_eq!(t.at(3400, Ev::Release), Act::None);
        assert!(t.f.recording());
        // A chord starts nothing.
        let mut t = T::toggle();
        t.at(0, Ev::ModPress);
        t.at(40, Ev::OtherKey);
        assert_eq!(t.at(80, Ev::Release), Act::None);
        assert!(!t.f.recording());
    }

    #[test]
    fn modifier_presses_while_transcribing_start_nothing() {
        let mut t = T::hold();
        t.busy = true;
        assert_eq!(t.at(0, Ev::ModPress), Act::None);
        assert_eq!(t.at(80, Ev::Release), Act::None);
        t.busy = false;
        t.at(200, Ev::ModPress);
        assert_eq!(t.at(260, Ev::Release), Act::None, "the busy tap was not a first tap");
        t.busy = true;
        t.at(400, Ev::ModPress);
        assert_eq!(t.at(460, Ev::Release), Act::None, "a second tap while busy");
        t.busy = false;
        t.at(1000, Ev::ModPress);
        t.busy = true;
        assert_eq!(t.at(1150, Ev::HoldReady(2)), Act::None, "busy by the time the delay ran out");
    }

    #[test]
    fn globe_key_must_do_nothing_before_fn_is_a_talk_key() {
        assert!(!globe_does_nothing(None), "absent key = system default action");
        assert!(globe_does_nothing(Some("0\n")));
        assert!(!globe_does_nothing(Some("1\n")));
        assert!(!globe_does_nothing(Some("2\n")));
        assert!(!globe_does_nothing(Some("3")));
    }
}
