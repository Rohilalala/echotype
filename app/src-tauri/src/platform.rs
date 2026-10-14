//! Paste into the focused app, and OS permission checks.

use std::sync::Mutex;
use std::time::Duration;
use tauri::AppHandle;

/// One clipboard for the life of the app. On Linux (X11) the Clipboard object itself
/// serves what was copied, so a per-call instance would take the text with it when dropped.
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

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
    std::thread::sleep(Duration::from_millis(60));
    let (tx, rx) = std::sync::mpsc::channel();
    // Keyboard-layout lookups (TIS) must run on the macOS main thread.
    app.run_on_main_thread(move || {
        let _ = tx.send(send_paste_keys());
    })
    .map_err(|e| (e.to_string(), true))?;
    rx.recv().map_err(|e| (e.to_string(), true))?.map_err(|e| (format!("{e}. The text is on your clipboard"), true))?;
    std::thread::sleep(Duration::from_millis(60));
    if let Some(prev) = saved {
        let _ = set_private(cb, &prev);
    }
    // ponytail: restores text only; an image on the clipboard is replaced by the transcript. Handy's image path is the upgrade.
    Ok(())
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

/// Plays bundled `sounds/<name>.wav` (start, stop, lock).
// ponytail: macOS only (NSSound); Windows/Linux get sounds when someone wants them (cpal output or rodio).
pub fn play_sound(app: &AppHandle, name: &'static str) {
    #[cfg(target_os = "macos")]
    {
        use tauri::Manager;
        let Ok(path) = app.path().resource_dir().map(|d| d.join("sounds").join(format!("{name}.wav"))) else { return };
        let _ = app.run_on_main_thread(move || mac::play(name, &path));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, name);
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

fn send_paste_keys() -> Result<(), String> {
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};
    let mut e = Enigo::new(&Settings::default()).map_err(|e| format!("Keyboard injection unavailable: {e}"))?;
    #[cfg(target_os = "macos")]
    let (modifier, v) = (Key::Meta, Key::Other(mac::command_v_keycode() as u32));
    #[cfg(target_os = "windows")]
    let (modifier, v) = (Key::Control, Key::Other(0x56)); // VK_V
    #[cfg(target_os = "linux")]
    let (modifier, v) = (Key::Control, Key::Unicode('v'));
    let err = |e: enigo::InputError| format!("Paste keystroke failed: {e}");
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
const NOT_TEXT: [&str; 12] = [
    "AXList", "AXOutline", "AXRow", "AXButton", "AXCheckBox", "AXRadioButton", "AXImage", "AXScrollArea", "AXSlider", "AXPopUpButton",
    "AXMenuButton", "AXDisclosureTriangle",
];
/// kAXErrorAPIDisabled: Accessibility is off for Echotype.
pub const AX_DISABLED: i32 = -25211;

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
    return target(accessibility_granted(), mac::focused());
    #[cfg(not(target_os = "macos"))]
    Target::Paste
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
        fn AXUIElementSetMessagingTimeout(el: CFTypeRef, seconds: f32) -> i32;
        fn AXUIElementCopyAttributeValue(el: CFTypeRef, attr: CFTypeRef, out: *mut CFTypeRef) -> i32;
        fn AXUIElementIsAttributeSettable(el: CFTypeRef, attr: CFTypeRef, out: *mut u8) -> i32;
    }

    /// kAXErrorNoValue: nothing is focused.
    const AX_NO_VALUE: i32 = -25212;

    /// The focused element system-wide, or the AXError that stopped the question. A hung app
    /// answers within 250 ms or counts as "can't tell".
    pub fn focused() -> Result<Option<super::Focused>, i32> {
        let attr = |name: &'static str| CFString::from_static_string(name);
        // SAFETY: every Copy/Create result is owned and wrapped under the create rule (released
        // on drop); out-pointers are valid locals; attribute names are CFStrings kept alive meanwhile.
        unsafe {
            let sys = CFType::wrap_under_create_rule(AXUIElementCreateSystemWide());
            AXUIElementSetMessagingTimeout(sys.as_CFTypeRef(), 0.25);
            let mut out: CFTypeRef = std::ptr::null();
            match AXUIElementCopyAttributeValue(sys.as_CFTypeRef(), attr("AXFocusedUIElement").as_CFTypeRef(), &mut out) {
                0 if !out.is_null() => {}
                0 | AX_NO_VALUE => return Ok(None),
                e => return Err(e),
            }
            let el = CFType::wrap_under_create_rule(out);
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

    pub fn accessibility() -> bool {
        // SAFETY: no arguments; reads the TCC state for this process.
        unsafe { AXIsProcessTrusted() }
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

    /// One NSSound per name, created on first use and kept (three tiny sounds, no release needed).
    /// Main thread only.
    pub fn play(name: &'static str, path: &std::path::Path) {
        use std::collections::HashMap;
        use std::sync::Mutex;
        static SOUNDS: Mutex<Option<HashMap<&'static str, usize>>> = Mutex::new(None);
        let mut g = SOUNDS.lock().unwrap_or_else(|e| e.into_inner());
        let sounds = g.get_or_insert_with(HashMap::new);
        // SAFETY: objc_msgSend is cast to each selector's exact signature: alloc/play/stop take no
        // arguments, initWithContentsOfFile:byReference: takes an NSString and a BOOL, and
        // stringWithUTF8String: takes a NUL-terminated C string. Every pointer is checked for null.
        unsafe {
            let msg0: extern "C" fn(*const c_void, *const c_void) -> *const c_void = std::mem::transmute(objc_msgSend as *const ());
            let sel = |s: &std::ffi::CStr| sel_registerName(s.as_ptr());
            let sound = match sounds.get(name) {
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
                    sounds.insert(name, s as usize);
                    s
                }
            };
            msg0(sound, sel(c"stop"));
            msg0(sound, sel(c"play"));
        }
    }

    const ANSI_V: u16 = 9;

    /// Must run on the main thread.
    pub fn command_v_keycode() -> u16 {
        // SAFETY: the input source is retained until CFRelease below; layout bytes live as long as it does;
        // all out-pointers are valid locals of the declared sizes.
        unsafe {
            let src = TISCopyCurrentKeyboardLayoutInputSource();
            if src.is_null() {
                return ANSI_V;
            }
            let data = TISGetInputSourceProperty(src, kTISPropertyUnicodeKeyLayoutData);
            let layout = if data.is_null() { std::ptr::null() } else { CFDataGetBytePtr(data) };
            let mut found = ANSI_V;
            if !layout.is_null() {
                let kbd = LMGetKbdType() as u32;
                for code in 0..128u16 {
                    let (mut dead, mut len, mut out) = (0u32, 0usize, [0u16; 4]);
                    // Modifier state 1 = Command (Carbon cmdKey >> 8): non-Latin layouts map Cmd shortcuts to ANSI keys.
                    let st = UCKeyTranslate(layout, code, 3, 1, kbd, 1, &mut dead, out.len(), &mut len, out.as_mut_ptr());
                    if st == 0 && len == 1 && out[0] == b'v' as u16 {
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
        for role in ["AXList", "AXOutline", "AXButton", "AXScrollArea", "AXCheckBox"] {
            assert_eq!(paste(Ok(Some(el(role)))), Target::Card, "{role}");
        }
        // Focus that may still take text once the app fills in its tree: paste as before.
        for role in ["AXWebArea", "AXGroup", "AXCell", "AXUnknown", ""] {
            assert_eq!(paste(Ok(Some(el(role)))), Target::Paste, "{role}");
        }
        assert_eq!(paste(Ok(None)), Target::Card, "nothing focused (the desktop)");
        assert_eq!(paste(Err(AX_DISABLED)), Target::Card, "Accessibility switched off");
        assert_eq!(target(false, Ok(Some(el("AXTextArea")))), Target::Card, "no Accessibility, no ⌘V");
        // The API can't tell (a hung app, an attribute it doesn't implement): paste as before.
        for e in [-25204, -25205, -25208, -25200] {
            assert_eq!(paste(Err(e)), Target::Paste, "{e}");
        }
    }
}
