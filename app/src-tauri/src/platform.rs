//! Paste into the focused app, and OS permission checks.

use std::sync::Mutex;
use std::time::Duration;
use tauri::AppHandle;

/// One clipboard for the life of the app. On Linux (X11) the Clipboard object itself
/// serves what was copied, so a per-call instance would take the text with it when dropped.
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

/// The keys that paste, as the user presses them.
pub const PASTE_KEYS: &str = if cfg!(target_os = "macos") { "⌘V" } else { "Ctrl+V" };

/// Clipboard + Cmd/Ctrl+V, then restore the previous clipboard text.
/// Never loses text: without Accessibility, or if the keystroke fails (a secure input field),
/// the text stays on the clipboard. The error says whether it did: `(message, on_clipboard)`.
pub fn paste(app: &AppHandle, text: &str, restore: bool) -> Result<(), (String, bool)> {
    let mut slot = clipboard().map_err(|e| (e, false))?;
    let cb = slot.as_mut().expect("set by clipboard()");
    if !accessibility_granted() {
        set_private(cb, text).map_err(|e| (format!("Clipboard write failed: {e}"), false))?;
        return Err(("Accessibility is off, so Echotype copied instead of pasting".into(), true));
    }
    let saved = if restore { cb.get_text().ok() } else { None };
    set_private(cb, text).map_err(|e| (format!("Clipboard write failed: {e}"), false))?;
    // Delays follow Handy (MIT): 60 ms for the pasteboard to settle, 100 ms modifier hold, 60 ms before restoring.
    // ponytail: on Wayland the clipboard crosses the XWayland bridge first, so 250 ms; tune if a slow app pastes the old text.
    let settle = Duration::from_millis(if wayland() { 250 } else { 60 });
    std::thread::sleep(settle);
    let (tx, rx) = std::sync::mpsc::channel();
    // Keyboard-layout lookups (TIS) must run on the macOS main thread.
    app.run_on_main_thread(move || {
        let _ = tx.send(send_command_key('v'));
    })
    .map_err(|e| (e.to_string(), true))?;
    rx.recv().map_err(|e| (e.to_string(), true))?.map_err(|e| (format!("{e}. The text is on your clipboard"), true))?;
    std::thread::sleep(settle);
    if let Some(prev) = saved {
        let _ = set_private(cb, &prev);
    }
    // ponytail: restores text only; an image on the clipboard is replaced by the transcript. Handy's image path is the upgrade.
    Ok(())
}

/// The text selected in the app in front, read with ⌘C; the clipboard is put back afterwards.
/// None when nothing is selected (the app copied nothing). Needs Accessibility, like paste.
// ponytail: some editors (VS Code, JetBrains) copy the whole line on ⌘C with no selection, so that
// line is edited; read AXSelectedText first if that surprises people. Text only, like paste().
pub fn copy_selection(app: &AppHandle) -> Result<Option<String>, String> {
    let mut slot = clipboard()?;
    let cb = slot.as_mut().expect("set by clipboard()");
    let saved = cb.get_text().ok();
    // Cleared first, so "nothing selected" is an empty clipboard, not what was copied before.
    cb.clear().map_err(|e| format!("Clipboard write failed: {e}"))?;
    let (tx, rx) = std::sync::mpsc::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(send_command_key('c'));
    })
    .map_err(|e| e.to_string())?;
    let sent = rx.recv().map_err(|e| e.to_string()).and_then(|r| r);
    // Apps write the pasteboard in their own time: give the copy up to 300 ms to land.
    let mut got = None;
    for _ in 0..15 {
        if sent.is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
        got = cb.get_text().ok().filter(|t| !t.is_empty());
        if got.is_some() {
            break;
        }
    }
    let _ = match saved {
        Some(prev) => set_private(cb, &prev),
        None => cb.clear(),
    };
    sent.map(|()| got)
}

/// Presses Return in the app in front ("send it"), after a paste: on the main thread, as paste's ⌘V.
pub fn press_return(app: &AppHandle) -> Result<(), String> {
    // The app reads the pasteboard in its own time: let the paste land before the message is sent.
    std::thread::sleep(Duration::from_millis(100));
    let (tx, rx) = std::sync::mpsc::channel();
    app.run_on_main_thread(move || {
        use enigo::{Direction, Enigo, Key, Keyboard, Settings};
        let _ = tx.send(Enigo::new(&Settings::default()).map_err(|e| e.to_string()).and_then(|mut e| e.key(Key::Return, Direction::Click).map_err(|e| e.to_string())));
    })
    .map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())?
}

/// Waits (up to `max`) until Control, Option and Shift are up, so a ⌘V sent now is not ⌘⌃V.
/// False when they are still down after `max`.
pub fn wait_for_modifiers_up(max: Duration) -> bool {
    #[cfg(target_os = "macos")]
    {
        let t = std::time::Instant::now();
        while mac::control_or_option_down() {
            if t.elapsed() >= max {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let _ = max;
    true
}

/// Plays bundled `sounds/<pack>/<name>.wav` (start, stop, lock, nopaste) from the chosen pack.
pub fn play_sound(app: &AppHandle, name: &'static str) {
    play_from(app, &crate::current_settings(app).sound_pack, name);
}

pub fn play_from(app: &AppHandle, pack: &str, name: &'static str) {
    #[cfg(target_os = "macos")]
    {
        use tauri::Manager;
        let Ok(path) = app.path().resource_dir().map(|d| d.join("sounds").join(pack).join(format!("{name}.wav"))) else { return };
        let _ = app.run_on_main_thread(move || mac::play(&path));
    }
    // ponytail: a thread and an output stream per chime (~110 ms each); keep one stream open if they ever lag.
    #[cfg(not(target_os = "macos"))]
    {
        use tauri::Manager;
        let Ok(path) = app.path().resource_dir().map(|d| d.join("sounds").join(pack).join(format!("{name}.wav"))) else { return };
        std::thread::spawn(move || {
            if let Err(e) = crate::audio::play(&path) {
                eprintln!("echotype: could not play {name}: {e}");
            }
        });
    }
}

fn clipboard() -> Result<std::sync::MutexGuard<'static, Option<arboard::Clipboard>>, String> {
    let mut slot = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(arboard::Clipboard::new().map_err(|e| format!("Clipboard unavailable: {e}"))?);
    }
    Ok(slot)
}

/// Puts text on the clipboard without pasting it.
pub fn copy(text: &str) -> Result<(), String> {
    let mut slot = clipboard()?;
    set_private(slot.as_mut().expect("set by clipboard()"), text).map_err(|e| format!("Clipboard write failed: {e}"))
}

/// Writes text marked "don't record" for clipboard-history apps (nspasteboard.org
/// on macOS, the history flag on Windows/Linux): transcripts are private, and the
/// restored clipboard may be a password copied from a password manager.
fn set_private(cb: &mut arboard::Clipboard, text: &str) -> Result<(), arboard::Error> {
    #[cfg(target_os = "macos")]
    use arboard::SetExtApple;
    #[cfg(target_os = "windows")]
    use arboard::SetExtWindows;
    #[cfg(target_os = "linux")]
    use arboard::SetExtLinux;
    cb.set().exclude_from_history().text(text)
}

/// A Wayland session: apps can't send keystrokes to each other there (X11 apps under XWayland excepted).
pub fn wayland() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty())
}

/// The command-line tool that can press Ctrl+V on Wayland, if one is installed: wtype (wlroots
/// compositors, KDE) or ydotool (any compositor; its ydotoold daemon must be running).
pub fn paste_tool() -> Option<&'static str> {
    let on_path = |tool: &str| std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()));
    ["wtype", "ydotool"].into_iter().find(|t| wayland() && on_path(t))
}

/// Ctrl+V (or Ctrl+C) through wtype or ydotool. Without either, the text stays on the clipboard and the card says so.
fn wayland_key(key: char) -> Result<(), String> {
    let tool = paste_tool().ok_or("On Wayland, Echotype needs wtype or ydotool to paste")?;
    // ydotool takes Linux input event codes: 29 Left Ctrl, 47 V, 46 C; :1 press, :0 release.
    let (k, code) = if key == 'c' { ("c", 46) } else { ("v", 47) };
    let (down, up) = (format!("{code}:1"), format!("{code}:0"));
    let args: Vec<&str> = if tool == "wtype" { vec!["-M", "ctrl", "-k", k, "-m", "ctrl"] } else { vec!["key", "29:1", &down, &up, "29:0"] };
    match std::process::Command::new(tool).args(args).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!("{tool} could not paste: {}", String::from_utf8_lossy(&o.stderr).trim())),
        Err(e) => Err(format!("{tool} could not paste: {e}")),
    }
}

/// ⌘ (Ctrl elsewhere) plus `key` ('v' or 'c'), as the user's keyboard layout types it.
fn send_command_key(key: char) -> Result<(), String> {
    if wayland() {
        return wayland_key(key);
    }
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};
    let mut e = Enigo::new(&Settings::default()).map_err(|e| format!("Keyboard injection unavailable: {e}"))?;
    #[cfg(target_os = "macos")]
    let (modifier, v) = (Key::Meta, Key::Other(mac::command_keycode(key) as u32));
    #[cfg(target_os = "windows")]
    let (modifier, v) = (Key::Control, Key::Other(key.to_ascii_uppercase() as u32)); // VK_V, VK_C
    #[cfg(target_os = "linux")]
    let (modifier, v) = (Key::Control, Key::Unicode(key));
    let err = |e: enigo::InputError| format!("Keystroke failed: {e}");
    e.key(modifier, Direction::Press).map_err(err)?;
    let clicked = e.key(v, Direction::Click).map_err(err);
    std::thread::sleep(Duration::from_millis(100));
    e.key(modifier, Direction::Release).map_err(err)?;
    clicked
}

/// Where a finished transcript goes: pasted with ⌘V, or shown on the pill's card for the user
/// to copy, because there is nothing to paste into.
#[derive(Debug, PartialEq)]
pub enum Target {
    Paste,
    Card,
}

/// What Accessibility says about the focused element of the frontmost app.
#[derive(Debug, Default)]
pub struct Focused {
    pub role: String,
    /// AXEditable is true (editable web content).
    pub editable: bool,
    /// AXValue is text and settable.
    pub text_value: bool,
    /// It has an AXSelectedTextRange: a caret or a selection somewhere text can go.
    pub text_range: bool,
}

const TEXT_ROLES: [&str; 4] = ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"];
/// Focus that plainly takes no text. Anything else without a text signal (a web area Chrome has
/// not filled in yet, an Electron app before it turns its tree on, a terminal's custom view)
/// counts as "can't tell" and is pasted into as before.
/// (Cells and tables are not here: a spreadsheet takes a paste.)
const NOT_TEXT: [&str; 20] = [
    "AXList", "AXOutline", "AXRow", "AXButton", "AXCheckBox", "AXRadioButton", "AXImage", "AXScrollArea", "AXSlider", "AXPopUpButton",
    "AXMenuButton", "AXDisclosureTriangle", "AXWindow", "AXApplication", "AXStaticText", "AXLink", "AXToolbar", "AXTabGroup",
    "AXSplitGroup", "AXMenuItem",
];
/// Focus that is usually a page or a pane, not a text box, but that a browser or Electron app may
/// report for a moment before it reports the box itself: asked again once before the card shows.
const UNSURE: [&str; 3] = ["AXWebArea", "AXGroup", "AXLayoutArea"];

/// The focus is a page or pane with no sign of a caret (see UNSURE).
fn unsure(f: &Focused) -> bool {
    UNSURE.contains(&f.role.as_str()) && !(f.editable || f.text_value || f.text_range)
}
/// kAXErrorAPIDisabled: Accessibility is off for Echotype.
pub const AX_DISABLED: i32 = -25211;

/// Asks the frontmost app to build its accessibility tree (Electron and Chromium apps), so the
/// paste check can see its text box. Call when a recording starts.
pub fn wake_accessibility() {
    #[cfg(target_os = "macos")]
    mac::wake_accessibility();
}

/// The text either side of the caret in the focused element of the app in front, at most `before`
/// and `after` characters (UTF-16 units) of it: only that window is read, never a whole document.
/// Comes back as (before, selected, after): the selection, if any, apart. None for a password
/// field, while secure input is on (a password being typed anywhere), no text element,
/// Accessibility off, or no answer within 250 ms.
pub fn caret_text(before: usize, after: usize) -> Option<(String, String, String)> {
    #[cfg(target_os = "macos")]
    return if mac::secure_input() { None } else { mac::caret_text(before, after) };
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (before, after);
        None
    }
}

/// The paste decision. `focus`: Ok(None) when nothing has focus, Err(AXError) when the API
/// could not tell, in which case Echotype pastes as it always has. Without Accessibility there
/// is no ⌘V at all, so that is the card too.
pub fn target(trusted: bool, focus: Result<Option<Focused>, i32>) -> Target {
    match focus {
        _ if !trusted => Target::Card,
        Err(AX_DISABLED) | Ok(None) => Target::Card,
        Err(_) => Target::Paste,
        Ok(Some(f)) if TEXT_ROLES.contains(&f.role.as_str()) || f.editable || f.text_value || f.text_range => Target::Paste,
        Ok(Some(f)) if NOT_TEXT.contains(&f.role.as_str()) => Target::Card,
        Ok(Some(_)) => Target::Paste,
    }
}

/// Whether the frontmost app's focused element takes text, asked right before pasting.
pub fn paste_target() -> Target {
    #[cfg(target_os = "macos")]
    return mac_paste_target();
    #[cfg(not(target_os = "macos"))]
    Target::Paste
}

#[cfg(target_os = "macos")]
fn mac_paste_target() -> Target {
    let trusted = accessibility_granted();
    let first = mac::focused();
    if !matches!(&first, Ok(Some(f)) if unsure(f)) {
        return target(trusted, first);
    }
    // A page with no caret: give the app a moment to report the box that has focus.
    std::thread::sleep(std::time::Duration::from_millis(120));
    match mac::focused() {
        Ok(Some(f)) if unsure(&f) => Target::Card, // still a page: nothing takes text
        again => target(trusted, again),
    }
}

/// The app in front (macOS), so learn.rs reads back only the app it pasted into.
pub fn front_pid() -> Option<i32> {
    #[cfg(target_os = "macos")]
    return mac::front_pid();
    #[cfg(not(target_os = "macos"))]
    None
}

/// The text of app `pid`'s focused field while that app is still in front (learn.rs). None for a
/// password field, a field over `max` characters, or focus that isn't text.
pub fn focused_text(pid: i32, max: usize) -> Option<String> {
    #[cfg(target_os = "macos")]
    return (mac::front_pid() == Some(pid)).then(|| mac::focused_text(pid, max)).flatten();
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (pid, max);
        None
    }
}

/// The trackpad's alignment click (macOS), as when a dragged object snaps into place.
pub fn snap_feedback() {
    #[cfg(target_os = "macos")]
    mac::haptic_alignment();
}

pub fn accessibility_granted() -> bool {
    #[cfg(target_os = "macos")]
    return mac::accessibility();
    #[cfg(not(target_os = "macos"))]
    true
}

/// "granted" | "denied" | "unknown" (not asked yet, or not knowable on this OS).
pub fn microphone_status() -> &'static str {
    #[cfg(target_os = "macos")]
    return match mac::microphone() {
        3 => "granted",
        1 | 2 => "denied",
        _ => "unknown",
    };
    #[cfg(not(target_os = "macos"))]
    "unknown"
}

/// An administrator (MDM, Screen Time) decides microphone access: nothing in Echotype can change it.
pub fn microphone_restricted() -> bool {
    #[cfg(target_os = "macos")]
    return mac::microphone() == 1;
    #[cfg(not(target_os = "macos"))]
    false
}

/// Shows the system's microphone prompt now (onboarding asks up front, not mid-dictation). The
/// answer arrives later; callers poll `microphone_status`. Already answered: no prompt.
pub fn request_microphone() {
    #[cfg(target_os = "macos")]
    mac::request_microphone();
}

/// The app in front: its bundle id ("" when it has none) and name.
#[derive(serde::Serialize, Clone, Debug)]
pub struct FrontApp {
    pub id: String,
    pub name: String,
}

/// The app in front, unless it is Echotype itself. Asked on the main thread.
pub fn frontmost_app(app: &AppHandle) -> Option<FrontApp> {
    #[cfg(target_os = "macos")]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        app.run_on_main_thread(move || {
            let _ = tx.send(mac::frontmost_app());
        })
        .ok()?;
        rx.recv_timeout(Duration::from_secs(1)).ok().flatten()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        None
    }
}

/// `tccutil reset <service> <bundle id>`.
/// Apps a style can be picked for without dictating into them first: the bundles in the
/// Applications folders, by name. Other platforms: none, so the list grows by dictating.
pub fn installed_apps() -> Vec<FrontApp> {
    #[cfg(target_os = "macos")]
    {
        use core_foundation::{bundle::CFBundle, string::CFString, url::CFURL};
        let home = dirs::home_dir().unwrap_or_default().join("Applications");
        let roots = ["/Applications", "/Applications/Utilities", "/System/Applications", "/System/Applications/Utilities"].map(std::path::PathBuf::from).into_iter().chain([home]);
        let mut apps: Vec<FrontApp> = roots
            .filter_map(|d| std::fs::read_dir(d).ok())
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "app"))
            .filter_map(|p| {
                let info = CFBundle::new(CFURL::from_path(&p, true)?)?.info_dictionary();
                let id = info.find(&CFString::from_static_string("CFBundleIdentifier"))?.downcast::<CFString>()?.to_string();
                Some(FrontApp { id, name: p.file_stem()?.to_string_lossy().into() })
            })
            .collect();
        apps.sort_by_key(|a| a.name.to_lowercase());
        apps
    }
    #[cfg(not(target_os = "macos"))]
    Vec::new()
}

fn tcc_reset_args<'a>(service: &'a str, bundle_id: &'a str) -> [&'a str; 3] {
    ["reset", service, bundle_id]
}

/// Forgets this app's Accessibility entry, so the next request adds a fresh one. Ad-hoc signed
/// builds change their code signature on every update, and the old entry then shows "on" in
/// System Settings while macOS no longer trusts the new build.
pub fn reset_accessibility(bundle_id: &str) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let out = std::process::Command::new("/usr/bin/tccutil").args(tcc_reset_args("Accessibility", bundle_id)).output().map_err(|e| format!("Couldn't run tccutil: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(format!("tccutil failed: {}", String::from_utf8_lossy(&out.stderr).trim())) }
}

pub fn settings_url(kind: &str) -> Option<&'static str> {
    match (std::env::consts::OS, kind) {
        ("macos", "microphone") => Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"),
        ("macos", "accessibility") => Some("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"),
        ("macos", "input_monitoring") => Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"),
        ("macos", "keyboard") => Some("x-apple.systempreferences:com.apple.Keyboard-Settings.extension"),
        ("macos", "screen_recording") => Some("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"),
        ("windows", "microphone") => Some("ms-settings:privacy-microphone"),
        _ => None,
    }
}

/// Makes macOS list Echotype in the permission pane: the system only lists apps that asked.
pub fn request(kind: &str) {
    match kind {
        // Opening an input stream triggers the one-time microphone prompt.
        "microphone" if microphone_status() == "unknown" => {
            if let Ok(r) = crate::audio::Recorder::start(None, |_, _| {}) {
                r.stop();
            }
        }
        // enigo's constructor calls AXIsProcessTrustedWithOptions(prompt: true).
        "accessibility" if !accessibility_granted() => {
            let _ = enigo::Enigo::new(&enigo::Settings::default());
        }
        "screen_recording" if !crate::capture::screen_recording_allowed() => crate::capture::request_screen_recording(),
        #[cfg(target_os = "macos")]
        "input_monitoring" if !crate::hotkey::tap::allowed() => {
            crate::hotkey::tap::request();
        }
        _ => {}
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use std::ffi::{c_char, c_void};

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        static AVMediaTypeAudio: *const c_void;
    }

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> *const c_void;
        fn sel_registerName(name: *const c_char) -> *const c_void;
        fn objc_msgSend();
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCreateSystemWide() -> CFTypeRef;
        fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
        fn AXUIElementSetMessagingTimeout(el: CFTypeRef, seconds: f32) -> i32;
        fn AXUIElementCopyAttributeValue(el: CFTypeRef, attr: CFTypeRef, out: *mut CFTypeRef) -> i32;
        fn AXUIElementIsAttributeSettable(el: CFTypeRef, attr: CFTypeRef, out: *mut u8) -> i32;
        fn AXUIElementSetAttributeValue(el: CFTypeRef, attr: CFTypeRef, value: CFTypeRef) -> i32;
        fn AXUIElementCopyParameterizedAttributeValue(el: CFTypeRef, attr: CFTypeRef, param: CFTypeRef, out: *mut CFTypeRef) -> i32;
        fn AXValueCreate(kind: u32, value: *const c_void) -> CFTypeRef;
        fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> bool;
    }

    /// kAXValueCFRangeType.
    const AX_RANGE: u32 = 4;
    /// A text whose length Accessibility can't give is read whole only up to this; past it, not at all.
    const WHOLE_VALUE_MAX: isize = 20_000;

    /// kAXErrorNoValue: nothing is focused.
    const AX_NO_VALUE: i32 = -25212;
    /// kAXErrorCannotComplete: "can't tell", which pastes as Echotype always has.
    const AX_CANNOT_TELL: i32 = -25204;

    /// The app with keyboard focus (system-wide AXFocusedApplication), owned. Chrome can leave
    /// that question unanswered (-25204) while answering on its own element, so the frontmost
    /// app stands in.
    unsafe fn focused_app(sys: &CFType) -> Option<CFType> {
        let mut out: CFTypeRef = std::ptr::null();
        // SAFETY: as in focused(): an owned result under the create rule, a valid out-pointer.
        unsafe {
            (AXUIElementCopyAttributeValue(sys.as_CFTypeRef(), CFString::from_static_string("AXFocusedApplication").as_CFTypeRef(), &mut out) == 0 && !out.is_null())
                .then(|| CFType::wrap_under_create_rule(out))
                .or_else(|| front_pid().map(|pid| CFType::wrap_under_create_rule(AXUIElementCreateApplication(pid))))
        }
    }

    /// [[NSWorkspace sharedWorkspace] frontmostApplication].processIdentifier, None without AppKit.
    pub fn front_pid() -> Option<i32> {
        // SAFETY: objc_msgSend cast to each signature: two methods returning objects, one returning
        // pid_t; a nil receiver answers nil/0, and a missing class is checked.
        unsafe {
            let cls = objc_getClass(c"NSWorkspace".as_ptr());
            if cls.is_null() {
                return None;
            }
            let get: extern "C" fn(*const c_void, *const c_void) -> *const c_void = std::mem::transmute(objc_msgSend as *const ());
            let app = get(get(cls, sel_registerName(c"sharedWorkspace".as_ptr())), sel_registerName(c"frontmostApplication".as_ptr()));
            if app.is_null() {
                return None;
            }
            let pid: extern "C" fn(*const c_void, *const c_void) -> i32 = std::mem::transmute(objc_msgSend as *const ());
            Some(pid(app, sel_registerName(c"processIdentifier".as_ptr()))).filter(|p| *p > 0)
        }
    }

    /// Chromium and Electron apps (VS Code, Slack, Chrome, Claude...) keep their accessibility tree
    /// off until something asks: until then they report no focused element, and Echotype took
    /// their text boxes for nowhere to paste. AXManualAccessibility turns the tree on; it takes about
    /// a second to build, so this runs when a recording starts. Harmless for other apps.
    pub fn wake_accessibility() {
        // SAFETY: owned CF objects kept alive across the calls; the value is a static CFBoolean.
        unsafe {
            let sys = CFType::wrap_under_create_rule(AXUIElementCreateSystemWide());
            AXUIElementSetMessagingTimeout(sys.as_CFTypeRef(), 0.25);
            if let Some(app) = focused_app(&sys) {
                AXUIElementSetAttributeValue(app.as_CFTypeRef(), CFString::from_static_string("AXManualAccessibility").as_CFTypeRef(), core_foundation::boolean::CFBoolean::true_value().as_CFTypeRef());
            }
        }
    }

    /// The focused element system-wide, owned, or the AXError that stopped the question. A hung app
    /// answers within 250 ms or counts as "can't tell".
    fn focused_element() -> Result<Option<CFType>, i32> {
        let attr = |name: &'static str| CFString::from_static_string(name);
        // SAFETY: every Copy/Create result is owned and wrapped under the create rule (released
        // on drop); out-pointers are valid locals; attribute names are CFStrings kept alive meanwhile.
        unsafe {
            let sys = CFType::wrap_under_create_rule(AXUIElementCreateSystemWide());
            AXUIElementSetMessagingTimeout(sys.as_CFTypeRef(), 0.25);
            let mut out: CFTypeRef = std::ptr::null();
            let el = match AXUIElementCopyAttributeValue(sys.as_CFTypeRef(), attr("AXFocusedUIElement").as_CFTypeRef(), &mut out) {
                0 if !out.is_null() => CFType::wrap_under_create_rule(out),
                0 | AX_NO_VALUE => {
                    // Electron apps answer only on their own element (and system-wide stays empty
                    // even with their tree on): ask the focused app. One with a window but no answer
                    // can't tell; no app or no window is nowhere to paste.
                    let Some(app) = focused_app(&sys) else { return Ok(None) };
                    AXUIElementSetMessagingTimeout(app.as_CFTypeRef(), 0.25);
                    let ask = |name: &'static str| {
                        let mut v: CFTypeRef = std::ptr::null();
                        (AXUIElementCopyAttributeValue(app.as_CFTypeRef(), attr(name).as_CFTypeRef(), &mut v) == 0 && !v.is_null()).then(|| CFType::wrap_under_create_rule(v))
                    };
                    match ask("AXFocusedUIElement") {
                        Some(el) => el,
                        None if ask("AXFocusedWindow").is_some() => return Err(AX_CANNOT_TELL),
                        None => return Ok(None),
                    }
                }
                e => return Err(e),
            };
            Ok(Some(el))
        }
    }

    /// What Accessibility says about the focused element (focused_element).
    pub fn focused() -> Result<Option<super::Focused>, i32> {
        let attr = |name: &'static str| CFString::from_static_string(name);
        let Some(el) = focused_element()? else { return Ok(None) };
        // SAFETY: as in focused_element().
        unsafe {
            let copy = |name: &'static str| {
                let mut v: CFTypeRef = std::ptr::null();
                (AXUIElementCopyAttributeValue(el.as_CFTypeRef(), attr(name).as_CFTypeRef(), &mut v) == 0 && !v.is_null()).then(|| CFType::wrap_under_create_rule(v))
            };
            let mut f = super::Focused {
                role: copy("AXRole").and_then(|r| r.downcast::<CFString>()).map(|r| r.to_string()).unwrap_or_default(),
                editable: copy("AXEditable").and_then(|b| b.downcast::<CFBoolean>()).is_some_and(bool::from),
                ..Default::default()
            };
            // Cheapest first: a text role or AXEditable settles it before a long document's AXValue is copied.
            if super::TEXT_ROLES.contains(&f.role.as_str()) || f.editable {
                return Ok(Some(f));
            }
            f.text_range = copy("AXSelectedTextRange").is_some();
            if !f.text_range {
                let mut settable = 0u8;
                f.text_value = AXUIElementIsAttributeSettable(el.as_CFTypeRef(), attr("AXValue").as_CFTypeRef(), &mut settable) == 0
                    && settable != 0
                    && copy("AXValue").is_some_and(|v| v.instance_of::<CFString>());
            }
            Ok(Some(f))
        }
    }

    /// See super::focused_text.
    pub fn focused_text(pid: i32, max: usize) -> Option<String> {
        let attr = |name: &'static str| CFString::from_static_string(name);
        // SAFETY: as in focused(): owned results under the create rule, valid out-pointers.
        unsafe {
            let app = CFType::wrap_under_create_rule(AXUIElementCreateApplication(pid));
            AXUIElementSetMessagingTimeout(app.as_CFTypeRef(), 0.25);
            let copy = |el: &CFType, name: &'static str| {
                let mut v: CFTypeRef = std::ptr::null();
                (AXUIElementCopyAttributeValue(el.as_CFTypeRef(), attr(name).as_CFTypeRef(), &mut v) == 0 && !v.is_null()).then(|| CFType::wrap_under_create_rule(v))
            };
            let el = copy(&app, "AXFocusedUIElement")?;
            let text = |name: &'static str| copy(&el, name).and_then(|v| v.downcast::<CFString>()).map(|s| s.to_string()).unwrap_or_default();
            if text("AXRole") == "AXSecureTextField" || text("AXSubrole") == "AXSecureTextField" {
                return None; // a password field is never read
            }
            // The length first, so a long document's text is never copied.
            let len = copy(&el, "AXNumberOfCharacters").and_then(|n| n.downcast::<CFNumber>()).and_then(|n| n.to_i64());
            if len.is_some_and(|n| n as usize > max) {
                return None;
            }
            let v = copy(&el, "AXValue")?.downcast::<CFString>()?.to_string();
            (v.chars().count() <= max).then_some(v)
        }
    }

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn IsSecureEventInputEnabled() -> bool;
    }

    /// A password field somewhere has secure input on (Terminal's sudo prompt, a browser's
    /// password box): nothing on screen is read meanwhile.
    pub fn secure_input() -> bool {
        // SAFETY: no arguments; reads a process-wide flag.
        unsafe { IsSecureEventInputEnabled() }
    }

    /// super::caret_text: the window around the caret through AXStringForRange, or out of AXValue
    /// when the app has no AXStringForRange and the text is short.
    pub fn caret_text(before: usize, after: usize) -> Option<(String, String, String)> {
        use core_foundation::base::CFRange;
        use core_foundation::number::CFNumber;
        let attr = |name: &'static str| CFString::from_static_string(name);
        let el = focused_element().ok()??;
        // SAFETY: as in focused_element(); AXValueGetValue writes one CFRange into a local CFRange,
        // AXValueCreate copies one from a local.
        unsafe {
            let copy = |name: &'static str| {
                let mut v: CFTypeRef = std::ptr::null();
                (AXUIElementCopyAttributeValue(el.as_CFTypeRef(), attr(name).as_CFTypeRef(), &mut v) == 0 && !v.is_null()).then(|| CFType::wrap_under_create_rule(v))
            };
            let text = |v: CFType| v.downcast::<CFString>().map(|s| s.to_string());
            if [copy("AXRole"), copy("AXSubrole")].into_iter().flatten().filter_map(text).any(|r| r == "AXSecureTextField") {
                return None; // a password
            }
            let mut sel = CFRange::init(0, 0);
            if !AXValueGetValue(copy("AXSelectedTextRange")?.as_CFTypeRef(), AX_RANGE, &mut sel as *mut CFRange as *mut c_void) {
                return None;
            }
            let n = copy("AXNumberOfCharacters").and_then(|v| v.downcast::<CFNumber>()).and_then(|n| n.to_i64()).map(|n| n as isize);
            let start = (sel.location - before as isize).max(0);
            let end = n.map(|n| (sel.location + sel.length + after as isize).min(n));
            let ranged = end.filter(|e| *e > start).and_then(|end| {
                let range = CFType::wrap_under_create_rule(AXValueCreate(AX_RANGE, &CFRange::init(start, end - start) as *const CFRange as *const c_void));
                let mut v: CFTypeRef = std::ptr::null();
                (AXUIElementCopyParameterizedAttributeValue(el.as_CFTypeRef(), attr("AXStringForRange").as_CFTypeRef(), range.as_CFTypeRef(), &mut v) == 0 && !v.is_null())
                    .then(|| CFType::wrap_under_create_rule(v))
                    .and_then(text)
            });
            let (window, from) = match ranged {
                Some(w) => (w.encode_utf16().collect::<Vec<u16>>(), start),
                None if n.is_some_and(|n| n <= WHOLE_VALUE_MAX) => {
                    let all: Vec<u16> = text(copy("AXValue")?)?.encode_utf16().collect();
                    let end = ((sel.location + sel.length) as usize + after).min(all.len());
                    (all[(start as usize).min(end)..end].to_vec(), start)
                }
                None => return None,
            };
            let cut = ((sel.location - from).max(0) as usize).min(window.len());
            let end = (cut + sel.length.max(0) as usize).min(window.len());
            let part = |r: std::ops::Range<usize>| String::from_utf16_lossy(&window[r]);
            Some((part(0..cut), part(cut..end), part(end..window.len())))
        }
    }

    pub fn accessibility() -> bool {
        // SAFETY: no arguments; reads the TCC state for this process.
        unsafe { AXIsProcessTrusted() }
    }

    /// [[NSHapticFeedbackManager defaultPerformer] performFeedbackPattern:Alignment performanceTime:Now].
    pub fn haptic_alignment() {
        // SAFETY: a class method returning an object, then an instance method taking two NSIntegers
        // (NSHapticFeedbackPatternAlignment = 1, NSHapticFeedbackPerformanceTimeNow = 1); objc_msgSend
        // is cast to each signature. AppKit is loaded by the app; a missing class is a no-op.
        unsafe {
            let cls = objc_getClass(c"NSHapticFeedbackManager".as_ptr());
            if cls.is_null() {
                return;
            }
            let get: extern "C" fn(*const c_void, *const c_void) -> *const c_void = std::mem::transmute(objc_msgSend as *const ());
            let performer = get(cls, sel_registerName(c"defaultPerformer".as_ptr()));
            if performer.is_null() {
                return;
            }
            let perform: extern "C" fn(*const c_void, *const c_void, isize, isize) = std::mem::transmute(objc_msgSend as *const ());
            perform(performer, sel_registerName(c"performFeedbackPattern:performanceTime:".as_ptr()), 1, 1);
        }
    }

    /// AVAuthorizationStatus: 0 not determined, 1 restricted, 2 denied, 3 authorized.
    pub fn microphone() -> isize {
        // SAFETY: [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio] is a class
        // method taking one NSString and returning an NSInteger; objc_msgSend is cast to that signature.
        unsafe {
            let cls = objc_getClass(c"AVCaptureDevice".as_ptr());
            if cls.is_null() {
                return 0;
            }
            let sel = sel_registerName(c"authorizationStatusForMediaType:".as_ptr());
            let send: extern "C" fn(*const c_void, *const c_void, *const c_void) -> isize = std::mem::transmute(objc_msgSend as *const ());
            send(cls, sel, AVMediaTypeAudio)
        }
    }

    /// [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio completionHandler:^(BOOL){}].
    pub fn request_microphone() {
        // SAFETY: a class method taking an NSString and a block; objc_msgSend is cast to that
        // signature. AVFoundation copies the block, so the RcBlock may be dropped after the call.
        unsafe {
            let cls = objc_getClass(c"AVCaptureDevice".as_ptr());
            if cls.is_null() {
                return;
            }
            let done = block2::RcBlock::new(|_granted: u8| {}); // BOOL: a byte on every Mac ABI
            let send: extern "C" fn(*const c_void, *const c_void, *const c_void, *const block2::Block<dyn Fn(u8)>) = std::mem::transmute(objc_msgSend as *const ());
            send(cls, sel_registerName(c"requestAccessForMediaType:completionHandler:".as_ptr()), AVMediaTypeAudio, &*done);
        }
    }

    /// [[NSWorkspace sharedWorkspace] frontmostApplication]'s bundleIdentifier and localizedName,
    /// unless it is this process. Main thread only (the result is autoreleased into the main run loop's pool).
    pub fn frontmost_app() -> Option<super::FrontApp> {
        // SAFETY: each objc_msgSend cast matches its selector (no arguments; returning an object, an
        // NSInteger-sized pid_t or a C string); every returned pointer is checked for null.
        unsafe {
            let msg0: extern "C" fn(*const c_void, *const c_void) -> *const c_void = std::mem::transmute(objc_msgSend as *const ());
            let sel = |s: &std::ffi::CStr| sel_registerName(s.as_ptr());
            let ws = objc_getClass(c"NSWorkspace".as_ptr());
            if ws.is_null() {
                return None;
            }
            let front = msg0(msg0(ws, sel(c"sharedWorkspace")), sel(c"frontmostApplication"));
            if front.is_null() {
                return None;
            }
            let pid: extern "C" fn(*const c_void, *const c_void) -> i32 = std::mem::transmute(objc_msgSend as *const ());
            if pid(front, sel(c"processIdentifier")) == std::process::id() as i32 {
                return None;
            }
            let utf8: extern "C" fn(*const c_void, *const c_void) -> *const c_char = std::mem::transmute(objc_msgSend as *const ());
            let text = |s: *const c_void| {
                let p = if s.is_null() { std::ptr::null() } else { utf8(s, sel(c"UTF8String")) };
                (!p.is_null()).then(|| std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
            };
            let name = text(msg0(front, sel(c"localizedName")))?;
            Some(super::FrontApp { id: text(msg0(front, sel(c"bundleIdentifier"))).unwrap_or_default(), name })
        }
    }

    // Layout-aware Cmd+V keycode, adapted from Handy (MIT): Dvorak/AZERTY users get the key that types "v".
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardLayoutInputSource() -> *const c_void;
        fn TISGetInputSourceProperty(source: *const c_void, key: *const c_void) -> *const c_void;
        static kTISPropertyUnicodeKeyLayoutData: *const c_void;
        fn UCKeyTranslate(layout: *const u8, code: u16, action: u16, mods: u32, kbd: u32, opts: u32, dead: *mut u32, max: usize, len: *mut usize, out: *mut u16) -> i32;
        fn LMGetKbdType() -> u8;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
        fn CFRelease(v: *const c_void);
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceFlagsState(state: i32) -> u64;
    }

    /// Control, Option or Shift physically held (HID state, so our own synthetic events don't
    /// count). ⌘ may stay down: ⌘V is what we send anyway.
    pub fn control_or_option_down() -> bool {
        // SAFETY: plain query; 1 = kCGEventSourceStateHIDSystemState.
        unsafe { CGEventSourceFlagsState(1) & (0x0002_0000 | 0x0004_0000 | 0x0008_0000) != 0 }
    }

    /// One NSSound per file, created on first use and kept (at most 20 tiny sounds, no release needed).
    /// Main thread only.
    pub fn play(path: &std::path::Path) {
        use std::collections::HashMap;
        use std::sync::Mutex;
        static SOUNDS: Mutex<Option<HashMap<std::path::PathBuf, usize>>> = Mutex::new(None);
        let mut g = SOUNDS.lock().unwrap_or_else(|e| e.into_inner());
        let sounds = g.get_or_insert_with(HashMap::new);
        // SAFETY: objc_msgSend is cast to each selector's exact signature: alloc/play/stop take no
        // arguments, initWithContentsOfFile:byReference: takes an NSString and a BOOL, and
        // stringWithUTF8String: takes a NUL-terminated C string. Every pointer is checked for null.
        unsafe {
            let msg0: extern "C" fn(*const c_void, *const c_void) -> *const c_void = std::mem::transmute(objc_msgSend as *const ());
            let sel = |s: &std::ffi::CStr| sel_registerName(s.as_ptr());
            let sound = match sounds.get(path) {
                Some(&p) => p as *const c_void,
                None => {
                    let Ok(cpath) = std::ffi::CString::new(path.to_string_lossy().as_bytes()) else { return };
                    let str_fn: extern "C" fn(*const c_void, *const c_void, *const c_char) -> *const c_void = std::mem::transmute(objc_msgSend as *const ());
                    let ns_path = str_fn(objc_getClass(c"NSString".as_ptr()), sel(c"stringWithUTF8String:"), cpath.as_ptr());
                    let cls = objc_getClass(c"NSSound".as_ptr());
                    if ns_path.is_null() || cls.is_null() {
                        return;
                    }
                    let init: extern "C" fn(*const c_void, *const c_void, *const c_void, bool) -> *const c_void = std::mem::transmute(objc_msgSend as *const ());
                    let s = init(msg0(cls, sel(c"alloc")), sel(c"initWithContentsOfFile:byReference:"), ns_path, false);
                    if s.is_null() {
                        eprintln!("echotype: could not load sound {}", path.display());
                        return;
                    }
                    sounds.insert(path.to_path_buf(), s as usize);
                    s
                }
            };
            msg0(sound, sel(c"stop"));
            msg0(sound, sel(c"play"));
        }
    }

    /// The keycode for ⌘`key` ('v' or 'c'). Must run on the main thread.
    pub fn command_keycode(key: char) -> u16 {
        let ansi = if key == 'c' { 8 } else { 9 }; // ANSI C and V
        // SAFETY: the input source is retained until CFRelease below; layout bytes live as long as it does;
        // all out-pointers are valid locals of the declared sizes.
        unsafe {
            let src = TISCopyCurrentKeyboardLayoutInputSource();
            if src.is_null() {
                return ansi;
            }
            let data = TISGetInputSourceProperty(src, kTISPropertyUnicodeKeyLayoutData);
            let layout = if data.is_null() { std::ptr::null() } else { CFDataGetBytePtr(data) };
            let mut found = ansi;
            if !layout.is_null() {
                let kbd = LMGetKbdType() as u32;
                for code in 0..128u16 {
                    let (mut dead, mut len, mut out) = (0u32, 0usize, [0u16; 4]);
                    // Modifier state 1 = Command (Carbon cmdKey >> 8): non-Latin layouts map Cmd shortcuts to ANSI keys.
                    let st = UCKeyTranslate(layout, code, 3, 1, kbd, 1, &mut dead, out.len(), &mut len, out.as_mut_ptr());
                    if st == 0 && len == 1 && out[0] == key as u16 {
                        found = code;
                        break;
                    }
                }
            }
            CFRelease(src);
            found
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(role: &str) -> Focused {
        Focused { role: role.into(), ..Default::default() }
    }

    #[cfg(target_os = "macos")]
    /// The app in front (run from a terminal in VS Code: an Electron app) after its tree is woken:
    /// `cargo test -- --ignored --nocapture electron_focus`. Needs Accessibility for the terminal.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn electron_focus_is_seen_once_woken() {
        let before = mac::focused();
        wake_accessibility();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let after = mac::focused();
        eprintln!("before: {before:?}\nafter: {after:?}\ntarget: {:?}", paste_target());
        assert_eq!(paste_target(), Target::Paste);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn installed_apps_are_named_by_bundle_id_in_name_order() {
        let apps = installed_apps();
        assert!(apps.iter().any(|a| a.id == "com.apple.TextEdit" && a.name == "TextEdit"), "TextEdit ships with macOS");
        assert!(apps.windows(2).all(|w| w[0].name.to_lowercase() <= w[1].name.to_lowercase()));
    }

    #[test]
    fn reset_and_ask_again_resets_only_echotypes_accessibility_entry() {
        assert_eq!(tcc_reset_args("Accessibility", "app.echotype.desktop"), ["reset", "Accessibility", "app.echotype.desktop"]);
    }

    #[test]
    fn a_transcript_is_pasted_only_where_text_can_go() {
        let paste = |f| target(true, f);
        for role in ["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"] {
            assert_eq!(paste(Ok(Some(el(role)))), Target::Paste, "{role}");
        }
        // Web content: an editable element, or the caret of a web area (Chrome, Electron, VS Code).
        assert_eq!(paste(Ok(Some(Focused { editable: true, ..el("AXGroup") }))), Target::Paste);
        assert_eq!(paste(Ok(Some(Focused { text_range: true, ..el("AXWebArea") }))), Target::Paste);
        assert_eq!(paste(Ok(Some(Focused { text_value: true, ..el("AXUnknown") }))), Target::Paste, "a settable text value");
        // Nowhere to paste: the card.
        for role in ["AXList", "AXOutline", "AXButton", "AXScrollArea", "AXCheckBox", "AXWindow", "AXStaticText", "AXLink"] {
            assert_eq!(paste(Ok(Some(el(role)))), Target::Card, "{role}");
        }
        // Focus that may still take text once the app fills in its tree: paste on the first look;
        // a page or pane is asked again once (paste_target) and only a second blank look is the card.
        for role in ["AXWebArea", "AXGroup", "AXCell", "AXUnknown", ""] {
            assert_eq!(paste(Ok(Some(el(role)))), Target::Paste, "{role}");
        }
        assert!(unsure(&el("AXWebArea")) && unsure(&el("AXGroup")));
        assert!(!unsure(&Focused { text_range: true, ..el("AXWebArea") }) && !unsure(&el("AXCell")) && !unsure(&el("AXTextArea")));
        assert_eq!(paste(Ok(None)), Target::Card, "nothing focused (the desktop)");
        assert_eq!(paste(Err(AX_DISABLED)), Target::Card, "Accessibility switched off");
        assert_eq!(target(false, Ok(Some(el("AXTextArea")))), Target::Card, "no Accessibility, no ⌘V");
        // The API can't tell (a hung app, an attribute it doesn't implement): paste as before.
        for e in [-25204, -25205, -25208, -25200] {
            assert_eq!(paste(Err(e)), Target::Paste, "{e}");
        }
    }
}
