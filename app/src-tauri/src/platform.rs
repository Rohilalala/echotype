//! Paste into the focused app, and OS permission checks.

use std::sync::Mutex;
use std::time::Duration;
use tauri::AppHandle;

/// One clipboard for the life of the app. On Linux (X11) the Clipboard object itself
/// serves what was copied, so a per-call instance would take the text with it when dropped.
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

/// Clipboard + Cmd/Ctrl+V, then restore the previous clipboard text.
/// Never loses text: without Accessibility, or if the keystroke fails, the text stays on the clipboard.
pub fn paste(app: &AppHandle, text: &str, restore: bool) -> Result<(), String> {
    let mut slot = CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(arboard::Clipboard::new().map_err(|e| format!("Clipboard unavailable: {e}"))?);
    }
    let cb = slot.as_mut().expect("set above");
    if !accessibility_granted() {
        set_private(cb, text).map_err(|e| format!("Clipboard write failed: {e}"))?;
        return Err("Accessibility permission is off, so Echotype could not paste. The text is on your clipboard.".into());
    }
    let saved = if restore { cb.get_text().ok() } else { None };
    set_private(cb, text).map_err(|e| format!("Clipboard write failed: {e}"))?;
    // Delays follow Handy (MIT): 60 ms for the pasteboard to settle, 100 ms modifier hold, 60 ms before restoring.
    std::thread::sleep(Duration::from_millis(60));
    let (tx, rx) = std::sync::mpsc::channel();
    // Keyboard-layout lookups (TIS) must run on the macOS main thread.
    app.run_on_main_thread(move || {
        let _ = tx.send(send_paste_keys());
    })
    .map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())?.map_err(|e| format!("{e}. The text is on your clipboard."))?;
    std::thread::sleep(Duration::from_millis(60));
    if let Some(prev) = saved {
        let _ = set_private(cb, &prev);
    }
    // ponytail: restores text only; an image on the clipboard is replaced by the transcript. Handy's image path is the upgrade.
    Ok(())
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
        ("windows", "microphone") => Some("ms-settings:privacy-microphone"),
        _ => None,
    }
}

/// Makes macOS list Echotype in the permission pane: the system only lists apps that asked.
pub fn request(kind: &str) {
    match kind {
        // Opening an input stream triggers the one-time microphone prompt.
        "microphone" if microphone_status() == "unknown" => {
            if let Ok(r) = crate::audio::Recorder::start(None, |_| {}) {
                r.stop();
            }
        }
        // enigo's constructor calls AXIsProcessTrustedWithOptions(prompt: true).
        "accessibility" if !accessibility_granted() => {
            let _ = enigo::Enigo::new(&enigo::Settings::default());
        }
        _ => {}
    }
}

#[cfg(target_os = "macos")]
mod mac {
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
