//! Call detection (macOS 14.2+): when a call app has held the microphone for a few seconds,
//! Echotype offers meeting notes, and when that call ends it offers to stop them (or stops them,
//! if the user chose that). It looks only at which apps have the microphone or speakers open
//! (CoreAudio's process list, once a second) and, for a browser, at its window titles. It never
//! reads audio and never starts notes by itself: Start on the card or in the menu does.
// ponytail: macOS only. Windows: the CapabilityAccessManager ConsentStore keys
// (HKCU\...\ConsentStore\microphone\<app>\LastUsedTimeStop == 0 while in use); Linux: PipeWire/Pulse
// source-outputs with their application.process.binary. Add when meetings ship there.

use crate::{DictationState, lock, state};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::AppHandle;

const POLL: Duration = Duration::from_secs(1);
/// An app must keep the microphone this long to count as a call: a voice note, Siri or a mic
/// test passes unnoticed.
const SETTLE: Duration = Duration::from_secs(6);
/// A call has ended once its app has used neither the microphone nor the speakers for this long,
/// so muting yourself or a quiet stretch is not an end.
const END_AFTER: Duration = Duration::from_secs(20);

/// Call apps by bundle id; a helper's id starts with its app's (com.tinyspeck.slackmacgap.helper).
/// The name is the meeting's title. An id that is wrong only means that app is never offered.
const APPS: &[(&str, &str)] = &[
    ("us.zoom.xos", "Zoom call"),
    ("com.microsoft.teams2", "Teams call"),
    ("com.microsoft.teams", "Teams call"),
    ("com.tinyspeck.slackmacgap", "Slack huddle"),
    ("net.whatsapp.WhatsApp", "WhatsApp call"),
    ("desktop.WhatsApp", "WhatsApp call"),
    ("com.apple.FaceTime", "FaceTime call"),
    // FaceTime's audio runs in this daemon, not in the FaceTime app.
    ("com.apple.avconferenced", "FaceTime call"),
    ("com.hnc.Discord", "Discord call"),
    ("com.cisco.webexmeetingsapp", "Webex meeting"),
    ("Cisco-Systems.Spark", "Webex meeting"),
    ("com.skype.skype", "Skype call"),
    ("org.whispersystems.signal-desktop", "Signal call"),
    ("ru.keepcoder.Telegram", "Telegram call"),
    ("org.telegram.desktop", "Telegram call"),
    ("co.teamport.around", "Around call"),
    ("app.tuple.app", "Tuple call"),
    ("com.gather.Gather", "Gather call"),
    ("com.amazon.Amazon-Chime", "Chime meeting"),
    ("com.logmein.GoToMeeting", "GoTo meeting"),
];

/// Browsers: a meeting site's window title names the call, else the browser does.
const BROWSERS: &[(&str, &str)] = &[
    ("com.google.Chrome", "Chrome"),
    ("company.thebrowser.Browser", "Arc"),
    ("company.thebrowser.dia", "Dia"),
    ("com.apple.Safari", "Safari"),
    ("com.microsoft.edgemac", "Edge"),
    ("com.brave.Browser", "Brave"),
    ("org.mozilla.firefox", "Firefox"),
    ("com.operasoftware.Opera", "Opera"),
    ("com.vivaldi.Vivaldi", "Vivaldi"),
    ("org.chromium.Chromium", "Chromium"),
];

/// Meeting sites by a browser window's title (its front tab).
const SITES: &[(&str, &str)] = &[
    ("meet.google.com", "Google Meet call"),
    ("Meet - ", "Google Meet call"),
    ("Meet – ", "Google Meet call"),
    ("Microsoft Teams", "Teams call"),
    ("Zoom", "Zoom call"),
    ("Whereby", "Whereby call"),
    ("Jitsi", "Jitsi call"),
    ("Webex", "Webex meeting"),
    ("Huddle", "Slack huddle"),
    ("Discord", "Discord call"),
];

// ---------- the pure parts ----------

/// A process with the microphone or the speakers open, as CoreAudio lists it, and the app
/// responsible for it (a browser for its audio helper, Safari for WebKit's GPU process).
#[derive(Debug, Clone, Default)]
pub struct AudioUser {
    pub pid: i32,
    pub app_pid: i32,
    pub bundle: String,
    pub app_bundle: String,
    pub mic: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// A call app: the call's name.
    App(&'static str),
    /// A browser: its name, until a window title says which meeting.
    Browser(&'static str),
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::App(n) | Kind::Browser(n) => n,
        }
    }
}

fn find(table: &[(&str, &'static str)], bundle: &str) -> Option<&'static str> {
    let b = bundle.to_ascii_lowercase();
    table.iter().find(|(id, _)| b.strip_prefix(&id.to_ascii_lowercase()).is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))).map(|(_, n)| *n)
}

/// Which call app or browser this is, by the responsible app first (Teams' WebKit audio process
/// is com.apple.WebKit.GPU), then the process itself. `me` is Echotype's pid: its own dictation
/// and meeting notes never count.
pub fn kind(u: &AudioUser, me: i32) -> Option<Kind> {
    if u.pid == me || u.app_pid == me {
        return None;
    }
    [&u.app_bundle, &u.bundle].into_iter().find_map(|b| find(APPS, b).map(Kind::App).or_else(|| find(BROWSERS, b).map(Kind::Browser)))
}

/// The meeting a browser's window titles show, if any.
pub fn site(titles: &[String]) -> Option<&'static str> {
    titles.iter().find_map(|t| SITES.iter().find(|(needle, _)| t.contains(needle)).map(|(_, n)| *n))
}

#[derive(Debug, PartialEq)]
pub enum Event {
    /// This app has held the microphone for SETTLE and no notes are running: offer them.
    Offer(String),
    /// This app's call is over. `ours`: meeting notes started during it, so they were its notes.
    Ended { app: String, ours: bool },
}

#[derive(Debug)]
struct Seen {
    since: Instant,
    last: Instant,
    call: bool,
    /// Notes were already running when the app took the microphone: they are not this call's.
    notes_before: bool,
    ours: bool,
}

/// One prompt per call: an app becomes a call after SETTLE on the microphone, is offered once,
/// and is not offered again until its call has ended.
#[derive(Debug, Default)]
pub struct Detector {
    apps: BTreeMap<String, Seen>,
}

impl Detector {
    /// One look at the apps using audio: (app, has the microphone). `notes`: meeting notes are
    /// recording. `busy`: a dictation is, so an offer waits until it is done.
    pub fn step(&mut self, now: Instant, using: &[(String, bool)], notes: bool, busy: bool) -> Vec<Event> {
        for (app, mic) in using {
            match self.apps.get_mut(app) {
                // Before it is a call only the microphone counts; a call lives on through either.
                Some(s) if s.call || *mic => s.last = now,
                Some(_) => {}
                None if *mic => {
                    self.apps.insert(app.clone(), Seen { since: now, last: now, call: false, notes_before: notes, ours: false });
                }
                None => {}
            }
        }
        let mut out = vec![];
        self.apps.retain(|app, s| {
            if !s.call {
                return s.last == now; // a gap on the way to SETTLE starts the wait over
            }
            if now.duration_since(s.last) < END_AFTER {
                return true;
            }
            out.push(Event::Ended { app: app.clone(), ours: s.ours });
            false
        });
        for (app, s) in &mut self.apps {
            if !s.call && now.duration_since(s.since) >= SETTLE && (notes || !busy) {
                s.call = true;
                if !notes {
                    out.push(Event::Offer(app.clone()));
                }
            }
            s.ours |= s.call && notes && !s.notes_before;
        }
        out
    }
}

// ---------- the offer ----------

#[derive(Debug, Clone, PartialEq)]
enum Offer {
    /// Start notes titled this, for this app's call.
    Start { app: String, title: String },
    Stop { app: String },
}

/// What Start on the card (or the menu's meeting item) does now.
static OFFER: Mutex<Option<Offer>> = Mutex::new(None);

/// The title the meeting item starts notes with while a call is on offer.
pub fn offered_title() -> Option<String> {
    match lock(&OFFER).as_ref() {
        Some(Offer::Start { title, .. }) => Some(title.clone()),
        _ => None,
    }
}

fn set_offer(app: &AppHandle, o: Option<Offer>) {
    *lock(&OFFER) = o;
    if !crate::meeting::recording() {
        crate::meeting::refresh_tray(app, None); // "Start notes · Zoom call" in the menu
    }
}

/// The card: what was detected on top, the question, Not now and Start (or Stop).
fn card(line: &str, question: &str, button: &'static str) -> DictationState {
    DictationState {
        state: "hint",
        message: Some(format!("{line} · {button}?")),
        warning: Some(line.to_string()),
        text: Some(question.to_string()),
        card: true,
        offer: Some(button),
        ..Default::default()
    }
}

/// Shows an offer on the pill, or as a notification in menu-bar mode, where there is no pill.
fn show(app: &AppHandle, line: &str, question: &str, button: &'static str) {
    crate::emit_state(app, card(line, question, button));
    if crate::pill::in_menu_bar(&crate::current_settings(app)) {
        #[cfg(target_os = "macos")]
        mac::notify(app, line, &format!("{button} from the Echotype menu bar icon."));
    }
}

/// Start (or Stop) on the card. Without an offer (it was taken from the menu) it only closes.
#[tauri::command]
pub async fn pill_offer(app: AppHandle) -> Result<(), String> {
    crate::pill::dismiss_card(&app);
    std::thread::spawn(move || {
        let taken = lock(&OFFER).clone();
        set_offer(&app, None);
        let res = match taken {
            Some(Offer::Start { title, .. }) => crate::meeting::start(&app, Some(title)).map(|_| ()),
            Some(Offer::Stop { .. }) if crate::meeting::recording() => crate::meeting::stop(&app),
            _ => Ok(()),
        };
        if let Err(e) = res {
            crate::emit_state(&app, DictationState { state: "error", error: Some(e), ..Default::default() });
        }
    });
    Ok(())
}

// ---------- the loop ----------

/// Looks once a second while "Detect calls" is on; does nothing at all while it is off.
pub fn run(app: AppHandle) {
    #[cfg(target_os = "macos")]
    std::thread::spawn(move || {
        let me = std::process::id() as i32;
        let mut d = Detector::default();
        loop {
            std::thread::sleep(POLL);
            let s = crate::current_settings(&app);
            if !s.detect_meetings {
                if !d.apps.is_empty() {
                    d = Detector::default();
                    set_offer(&app, None);
                }
                continue;
            }
            // None before macOS 14.2: no per-app microphone use to go on.
            let Some(users) = mac::audio_users() else { continue };
            let mut using = vec![];
            let mut found: BTreeMap<&'static str, (Kind, i32)> = BTreeMap::new();
            for u in &users {
                if let Some(k) = kind(u, me) {
                    using.push((k.key().to_string(), u.mic));
                    found.entry(k.key()).or_insert((k, u.app_pid));
                }
            }
            let notes = crate::meeting::recording();
            let busy = state(&app).recording.load(std::sync::atomic::Ordering::SeqCst);
            for ev in d.step(Instant::now(), &using, notes, busy) {
                match ev {
                    Event::Offer(key) => {
                        let Some(&(k, pid)) = found.get(key.as_str()) else { continue };
                        let (title, line) = match k {
                            Kind::App(name) => (name.to_string(), format!("{name} detected")),
                            Kind::Browser(b) => match site(&mac::window_titles(pid)) {
                                Some(name) => (name.to_string(), format!("{name} detected")),
                                None => (format!("{b} call"), format!("{b} is using the microphone")),
                            },
                        };
                        set_offer(&app, Some(Offer::Start { app: key, title }));
                        show(&app, &line, "Take notes of this call? Nothing is recorded until you start.", "Start notes");
                    }
                    Event::Ended { app: key, ours } if ours && crate::meeting::recording() => {
                        let line = format!("{key} ended");
                        if s.meeting_end == "auto" {
                            set_offer(&app, None);
                            if let Err(e) = crate::meeting::stop(&app) {
                                eprintln!("echotype: {e}");
                            }
                            crate::emit_state(&app, crate::hint(format!("{line}. Meeting notes stopped."), None));
                        } else {
                            set_offer(&app, Some(Offer::Stop { app: key }));
                            show(&app, &line, "Stop the meeting notes? They keep going until you do.", "Stop notes");
                        }
                    }
                    Event::Ended { app: key, .. } => {
                        if lock(&OFFER).as_ref().is_some_and(|o| matches!(o, Offer::Start { app, .. } | Offer::Stop { app } if *app == key)) {
                            set_offer(&app, None);
                        }
                    }
                }
            }
            // Notes started (from the card, the menu or the Meetings page): the offer is taken.
            if notes && matches!(*lock(&OFFER), Some(Offer::Start { .. })) {
                *lock(&OFFER) = None;
            }
        }
    });
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

#[cfg(target_os = "macos")]
mod mac {
    use super::AudioUser;
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::{c_char, c_void};

    #[repr(C)]
    struct Address {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyDataSize(id: u32, addr: *const Address, qual_size: u32, qual: *const c_void, size: *mut u32) -> i32;
        fn AudioObjectGetPropertyData(id: u32, addr: *const Address, qual_size: u32, qual: *const c_void, size: *mut u32, out: *mut c_void) -> i32;
    }

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> *const c_void;
        fn sel_registerName(name: *const c_char) -> *const c_void;
        fn objc_msgSend();
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
        fn AXUIElementSetMessagingTimeout(el: CFTypeRef, seconds: f32) -> i32;
        fn AXUIElementCopyAttributeValue(el: CFTypeRef, attr: CFTypeRef, out: *mut CFTypeRef) -> i32;
    }

    const fn code(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }
    const SYSTEM: u32 = 1; // kAudioObjectSystemObject
    const PROCESSES: u32 = code(b"prs#"); // kAudioHardwarePropertyProcessObjectList (macOS 14.2+)
    const PID: u32 = code(b"ppid"); // kAudioProcessPropertyPID
    const BUNDLE: u32 = code(b"pbid"); // kAudioProcessPropertyBundleID
    const MIC: u32 = code(b"piri"); // kAudioProcessPropertyIsRunningInput
    const SPEAKERS: u32 = code(b"piro"); // kAudioProcessPropertyIsRunningOutput

    fn addr(selector: u32) -> Address {
        Address { selector, scope: code(b"glob"), element: 0 }
    }

    /// A fixed-size property (u32, i32, a CF object pointer).
    fn get<T: Copy + Default>(obj: u32, selector: u32) -> Option<T> {
        let mut v = T::default();
        let mut size = std::mem::size_of::<T>() as u32;
        // SAFETY: `v` is a valid T and `size` says exactly how many bytes may be written to it.
        let st = unsafe { AudioObjectGetPropertyData(obj, &addr(selector), 0, std::ptr::null(), &mut size, &mut v as *mut T as *mut c_void) };
        (st == 0 && size as usize == std::mem::size_of::<T>()).then_some(v)
    }

    /// Every process with the microphone or the speakers running. None where CoreAudio has no
    /// process list (before macOS 14.2). About 12 ms of IPC with coreaudiod and no audio.
    pub fn audio_users() -> Option<Vec<AudioUser>> {
        let a = addr(PROCESSES);
        let mut size = 0u32;
        // SAFETY: size out-pointer is a valid local; the list is written into a buffer of that size.
        if unsafe { AudioObjectGetPropertyDataSize(SYSTEM, &a, 0, std::ptr::null(), &mut size) } != 0 {
            return None;
        }
        let mut ids = vec![0u32; size as usize / 4];
        // SAFETY: `ids` holds `size` bytes.
        if unsafe { AudioObjectGetPropertyData(SYSTEM, &a, 0, std::ptr::null(), &mut size, ids.as_mut_ptr() as *mut c_void) } != 0 {
            return None;
        }
        ids.truncate(size as usize / 4);
        let mut out = vec![];
        for id in ids {
            let mic = get::<u32>(id, MIC).unwrap_or(0) != 0;
            if !mic && get::<u32>(id, SPEAKERS).unwrap_or(0) == 0 {
                continue;
            }
            let Some(pid) = get::<i32>(id, PID) else { continue };
            let bundle = get::<usize>(id, BUNDLE).filter(|p| *p != 0).map(|p| {
                // SAFETY: the property is a CFStringRef the caller owns (create rule).
                unsafe { CFString::wrap_under_create_rule(p as CFStringRef) }.to_string()
            });
            let app_pid = responsible(pid);
            let app_bundle = if app_pid == pid { String::new() } else { bundle_of(app_pid) };
            out.push(AudioUser { pid, app_pid, bundle: bundle.unwrap_or_default(), app_bundle, mic });
        }
        Some(out)
    }

    /// The app macOS holds responsible for a process (Chrome for its helpers, Safari or Teams for
    /// a WebKit GPU process). The call is a private but long-stable libsystem export, looked up
    /// at run time; without it a process stands for itself.
    fn responsible(pid: i32) -> i32 {
        // SAFETY: dlsym with a NUL-terminated name; the symbol, when present, is `pid_t f(pid_t)`.
        unsafe {
            let f = libc::dlsym(libc::RTLD_DEFAULT, c"responsibility_get_pid_responsible_for_pid".as_ptr());
            if f.is_null() {
                return pid;
            }
            let f: extern "C" fn(i32) -> i32 = std::mem::transmute(f);
            match f(pid) {
                p if p > 0 => p,
                _ => pid,
            }
        }
    }

    /// [[NSRunningApplication runningApplicationWithProcessIdentifier:pid] bundleIdentifier].
    fn bundle_of(pid: i32) -> String {
        // SAFETY: objc_msgSend is cast to each selector's signature (pid_t in, object out; no
        // arguments, NSString out). The NSString is toll-free bridged to CFString and retained by
        // wrap_under_get_rule before the pool that owns it drains.
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let cls = objc_getClass(c"NSRunningApplication".as_ptr());
            let mut id = String::new();
            if !cls.is_null() {
                let by_pid: extern "C" fn(*const c_void, *const c_void, i32) -> *const c_void = std::mem::transmute(objc_msgSend as *const ());
                let get: extern "C" fn(*const c_void, *const c_void) -> *const c_void = std::mem::transmute(objc_msgSend as *const ());
                let app = by_pid(cls, sel_registerName(c"runningApplicationWithProcessIdentifier:".as_ptr()), pid);
                let s = if app.is_null() { std::ptr::null() } else { get(app, sel_registerName(c"bundleIdentifier".as_ptr())) };
                if !s.is_null() {
                    id = CFString::wrap_under_get_rule(s as CFStringRef).to_string();
                }
            }
            objc_autoreleasePoolPop(pool);
            id
        }
    }

    /// The app's window titles: from the window list when Screen Recording is allowed (titles are
    /// withheld otherwise), else from Accessibility, else none. Neither asks for a permission.
    pub fn window_titles(pid: i32) -> Vec<String> {
        let titles = listed_titles(pid);
        if titles.is_empty() && crate::platform::accessibility_granted() { ax_titles(pid) } else { titles }
    }

    pub fn listed_titles(pid: i32) -> Vec<String> {
        use core_graphics::window::{copy_window_info, kCGNullWindowID, kCGWindowListOptionAll, kCGWindowName, kCGWindowOwnerPID};
        let Some(list) = copy_window_info(kCGWindowListOptionAll, kCGNullWindowID) else { return vec![] };
        // SAFETY: the window list is an array of CFDictionary; the keys are CoreGraphics constants.
        let (owner_key, name_key) = unsafe { (CFString::wrap_under_get_rule(kCGWindowOwnerPID), CFString::wrap_under_get_rule(kCGWindowName)) };
        let list: CFArray<CFDictionary<CFString, CFType>> = unsafe { CFArray::wrap_under_get_rule(list.as_concrete_TypeRef()) };
        list.iter()
            .filter(|w| w.find(&owner_key).and_then(|v| v.downcast::<CFNumber>()).and_then(|n| n.to_i32()) == Some(pid))
            .filter_map(|w| w.find(&name_key).and_then(|v| v.downcast::<CFString>()).map(|s| s.to_string()))
            .filter(|t| !t.is_empty())
            .collect()
    }

    pub fn ax_titles(pid: i32) -> Vec<String> {
        let attr = |name: &'static str| CFString::from_static_string(name);
        // SAFETY: Create/Copy results are owned and wrapped under the create rule; out-pointers are
        // valid locals; a hung app answers within 250 ms or gives no titles.
        unsafe {
            let el = CFType::wrap_under_create_rule(AXUIElementCreateApplication(pid));
            AXUIElementSetMessagingTimeout(el.as_CFTypeRef(), 0.25);
            let mut out: CFTypeRef = std::ptr::null();
            if AXUIElementCopyAttributeValue(el.as_CFTypeRef(), attr("AXWindows").as_CFTypeRef(), &mut out) != 0 || out.is_null() {
                return vec![];
            }
            let Some(windows) = CFType::wrap_under_create_rule(out).downcast::<CFArray>() else { return vec![] };
            windows
                .iter()
                .filter_map(|w| {
                    let mut t: CFTypeRef = std::ptr::null();
                    (AXUIElementCopyAttributeValue(*w, attr("AXTitle").as_CFTypeRef(), &mut t) == 0 && !t.is_null())
                        .then(|| CFType::wrap_under_create_rule(t))
                        .and_then(|t| t.downcast::<CFString>())
                        .map(|s| s.to_string())
                })
                .collect()
        }
    }

    /// A Notification Center banner from Echotype (NSUserNotification: no permission prompt).
    // ponytail: informational only (no Start button on the banner); UNUserNotificationCenter with an
    // action category if menu-bar users want to start from the banner itself.
    pub fn notify(app: &tauri::AppHandle, title: &str, body: &str) {
        let (title, body) = (title.to_string(), body.to_string());
        let _ = app.run_on_main_thread(move || {
            let (title, body) = (CFString::new(&title), CFString::new(&body));
            // SAFETY: objc_msgSend is cast to each selector's signature (no arguments → object;
            // one object argument → nothing). CFStrings are toll-free bridged NSStrings that live
            // until the end of this closure; every class and object is checked for null.
            unsafe {
                let msg0: extern "C" fn(*const c_void, *const c_void) -> *const c_void = std::mem::transmute(objc_msgSend as *const ());
                let msg1: extern "C" fn(*const c_void, *const c_void, *const c_void) = std::mem::transmute(objc_msgSend as *const ());
                let sel = |s: &std::ffi::CStr| sel_registerName(s.as_ptr());
                let (center_cls, note_cls) = (objc_getClass(c"NSUserNotificationCenter".as_ptr()), objc_getClass(c"NSUserNotification".as_ptr()));
                if center_cls.is_null() || note_cls.is_null() {
                    return;
                }
                let center = msg0(center_cls, sel(c"defaultUserNotificationCenter"));
                let note = msg0(msg0(note_cls, sel(c"alloc")), sel(c"init"));
                if center.is_null() || note.is_null() {
                    return;
                }
                msg1(note, sel(c"setTitle:"), title.as_concrete_TypeRef() as *const c_void);
                msg1(note, sel(c"setInformativeText:"), body.as_concrete_TypeRef() as *const c_void);
                msg1(center, sel(c"deliverNotification:"), note);
                msg0(note, sel(c"release"));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(bundle: &str, app_bundle: &str) -> AudioUser {
        AudioUser { pid: 10, app_pid: 11, bundle: bundle.into(), app_bundle: app_bundle.into(), mic: true }
    }

    #[test]
    fn call_apps_and_browsers_are_named_by_bundle_id() {
        let k = |b: &str, a: &str| kind(&user(b, a), 1);
        assert_eq!(k("us.zoom.xos", ""), Some(Kind::App("Zoom call")));
        assert_eq!(k("com.microsoft.teams2", ""), Some(Kind::App("Teams call")));
        assert_eq!(k("com.tinyspeck.slackmacgap.helper", "com.tinyspeck.slackmacgap"), Some(Kind::App("Slack huddle")));
        assert_eq!(k("net.whatsapp.WhatsApp", ""), Some(Kind::App("WhatsApp call")));
        assert_eq!(k("com.apple.avconferenced", ""), Some(Kind::App("FaceTime call")));
        assert_eq!(k("com.google.Chrome.helper", "com.google.Chrome"), Some(Kind::Browser("Chrome")));
        // Dia's helpers carry Arc's (lower-cased) id; the responsible app decides.
        assert_eq!(k("company.thebrowser.browser.helper", "company.thebrowser.dia"), Some(Kind::Browser("Dia")));
        // WebKit's GPU process is whoever is responsible for it: Safari, or Teams' web view.
        assert_eq!(k("com.apple.WebKit.GPU", "com.apple.Safari"), Some(Kind::Browser("Safari")));
        assert_eq!(k("com.apple.WebKit.GPU", "com.microsoft.teams2"), Some(Kind::App("Teams call")));
        assert_eq!(k("com.apple.WebKit.GPU", "com.example.notes"), None, "an unknown web view is no call");
        for other in ["com.apple.QuickTimePlayerX", "com.apple.Siri", "com.apple.corespeechd_system", "", "us.zoom.xosx", "com.google.Chromecast"] {
            assert_eq!(k(other, ""), None, "{other}");
        }
    }

    #[test]
    fn echotype_itself_never_counts() {
        let me = 10;
        assert_eq!(kind(&AudioUser { pid: me, ..user("us.zoom.xos", "") }, me), None);
        assert_eq!(kind(&AudioUser { app_pid: me, ..user("com.apple.WebKit.GPU", "com.apple.Safari") }, me), None, "Echotype's own web view");
    }

    #[test]
    fn a_browser_window_title_names_the_meeting() {
        let t = |s: &[&str]| site(&s.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert_eq!(t(&["Inbox (3)", "Meet - abc-defg-hij"]), Some("Google Meet call"));
        assert_eq!(t(&["Meet – Weekly sync"]), Some("Google Meet call"));
        assert_eq!(t(&["Chat | Microsoft Teams"]), Some("Teams call"));
        assert_eq!(t(&["Zoom Meeting"]), Some("Zoom call"));
        assert_eq!(t(&["YouTube", "Docs"]), None);
        assert_eq!(t(&[]), None);
    }

    fn on(apps: &[(&str, bool)]) -> Vec<(String, bool)> {
        apps.iter().map(|(a, m)| (a.to_string(), *m)).collect()
    }

    /// Steps once a second from `t0` over `secs` seconds with the same apps, collecting events.
    fn run(d: &mut Detector, t0: Instant, from: u64, to: u64, apps: &[(&str, bool)], notes: bool, busy: bool) -> Vec<(u64, Event)> {
        (from..to).flat_map(|s| d.step(t0 + Duration::from_secs(s), &on(apps), notes, busy).into_iter().map(move |e| (s, e))).collect::<Vec<_>>()
    }

    #[test]
    fn a_call_is_offered_once_after_it_settles_and_ends_after_a_quiet_spell() {
        let (mut d, t0) = (Detector::default(), Instant::now());
        assert_eq!(run(&mut d, t0, 0, 6, &[("Zoom call", true)], false, false), vec![], "not before SETTLE");
        assert_eq!(run(&mut d, t0, 6, 60, &[("Zoom call", true)], false, false), vec![(6, Event::Offer("Zoom call".into()))], "once");
        // Muted (speakers only): still the same call, no second offer.
        assert_eq!(run(&mut d, t0, 60, 120, &[("Zoom call", false)], false, false), vec![]);
        // Off the microphone and speakers for END_AFTER: over.
        assert_eq!(run(&mut d, t0, 120, 140, &[], false, false), vec![(139, Event::Ended { app: "Zoom call".into(), ours: false })]);
        // A new call later is offered again.
        assert_eq!(run(&mut d, t0, 200, 207, &[("Zoom call", true)], false, false), vec![(206, Event::Offer("Zoom call".into()))]);
    }

    #[test]
    fn blips_never_settle() {
        let (mut d, t0) = (Detector::default(), Instant::now());
        // Five seconds on, one off, over and over (voice notes, Siri): the wait starts over each time.
        for round in 0..10 {
            let s = round * 6;
            assert_eq!(run(&mut d, t0, s, s + 5, &[("Chrome", true)], false, false), vec![]);
            assert_eq!(run(&mut d, t0, s + 5, s + 6, &[], false, false), vec![]);
        }
        // Speakers alone (music, a video) are never a call.
        assert_eq!(run(&mut d, t0, 100, 200, &[("Chrome", false)], false, false), vec![]);
    }

    #[test]
    fn running_notes_and_dictation_hold_the_offer() {
        let (mut d, t0) = (Detector::default(), Instant::now());
        // Notes already running when the call starts: nothing to offer, and they are not its notes.
        assert_eq!(run(&mut d, t0, 0, 30, &[("Teams call", true)], true, false), vec![]);
        assert_eq!(run(&mut d, t0, 30, 51, &[], true, false), vec![(49, Event::Ended { app: "Teams call".into(), ours: false })]);
        // Dictating when it settles: the offer waits until the dictation is done.
        assert_eq!(run(&mut d, t0, 100, 110, &[("Teams call", true)], false, true), vec![]);
        assert_eq!(run(&mut d, t0, 110, 112, &[("Teams call", true)], false, false), vec![(110, Event::Offer("Teams call".into()))]);
        // Notes started from the offer are this call's: its end may stop them.
        assert_eq!(run(&mut d, t0, 112, 200, &[("Teams call", true)], true, false), vec![]);
        assert_eq!(run(&mut d, t0, 200, 221, &[], true, false), vec![(219, Event::Ended { app: "Teams call".into(), ours: true })]);
    }

    #[test]
    fn two_apps_are_two_calls() {
        let (mut d, t0) = (Detector::default(), Instant::now());
        let got = run(&mut d, t0, 0, 7, &[("Chrome", true), ("Zoom call", true), ("Chrome", false)], false, false);
        assert_eq!(got, vec![(6, Event::Offer("Chrome".into())), (6, Event::Offer("Zoom call".into()))]);
    }

    /// Manual check: `cargo test print_audio_users -- --ignored --nocapture` while something holds
    /// the microphone (e.g. `ffmpeg -f avfoundation -i ":0" -t 5 -f null -`).
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn print_audio_users() {
        let users = mac::audio_users().expect("macOS 14.2+ lists audio processes");
        for u in &users {
            println!("{:>6} {:>6} mic={} {:<40} app={} → {:?}", u.pid, u.app_pid, u.mic, u.bundle, u.app_bundle, kind(u, std::process::id() as i32));
            if matches!(kind(u, 0), Some(Kind::Browser(_))) {
                println!("        titles: {:?}", mac::window_titles(u.app_pid));
            }
        }
        println!("{} using the microphone", users.iter().filter(|u| u.mic).count());
        // Any app's titles, e.g. a browser: ECHOTYPE_TITLES_PID=$(pgrep -x "Google Chrome").
        if let Some(pid) = std::env::var("ECHOTYPE_TITLES_PID").ok().and_then(|p| p.trim().parse().ok()) {
            let ax = crate::platform::accessibility_granted();
            println!("pid {pid} window list titles: {:?}", mac::listed_titles(pid));
            println!("pid {pid} Accessibility ({ax}) titles: {:?}", mac::ax_titles(pid));
        }
    }

    /// The detector on a real microphone user: ffmpeg holds the mic for 10 s, which settles into
    /// an offer at ~6 s; after it lets go the call ends END_AFTER later. ~35 s.
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn live_detector_settles_and_ends_on_a_real_mic_user() {
        let mut ff = std::process::Command::new("/opt/homebrew/bin/ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-f", "avfoundation", "-i", ":0", "-t", "10", "-f", "null", "-"])
            .spawn()
            .expect("ffmpeg");
        let (t0, mut d, mut events) = (Instant::now(), Detector::default(), vec![]);
        while t0.elapsed() < Duration::from_secs(36) {
            // Every microphone user but this test counts, under ffmpeg's own pid.
            let using: Vec<(String, bool)> =
                mac::audio_users().unwrap().into_iter().filter(|u| u.pid == ff.id() as i32).map(|u| ("ffmpeg".to_string(), u.mic)).collect();
            for e in d.step(Instant::now(), &using, false, false) {
                println!("{:>5.1}s {e:?}", t0.elapsed().as_secs_f32());
                events.push((t0.elapsed().as_secs(), e));
            }
            std::thread::sleep(POLL);
        }
        let _ = ff.wait();
        assert!(matches!(events.as_slice(), [(6..=8, Event::Offer(_)), (29..=33, Event::Ended { ours: false, .. })]), "{events:?}");
    }
}
