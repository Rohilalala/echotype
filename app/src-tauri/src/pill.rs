//! The always-on pill. One owner decides what the overlay shows and whether it is on screen:
//! a pure model (visibility, message precedence, notice timers, hover dwell, hit testing in
//! points, Dock offset, click debounce) and a ~15 Hz loop that applies it to the window.

use crate::settings::Settings;
use crate::{DictationState, lock, state};
use serde::Serialize;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, LogicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

/// The overlay window: the widest notice plus room for its soft shadow (points).
pub const WIN: (f64, f64) = (260.0, 32.0);
/// Gap between the tallest capsule and the bottom of the visible frame (or the auto-hidden Dock).
const MARGIN: f64 = 10.0;
const LINE_H: f64 = 22.0;
const MAX_W: f64 = 240.0;
/// The idle bar is 32x4 to look at and 48x14 to hit.
const IDLE_HIT: (f64, f64) = (48.0, 14.0);
/// Recording capsule widths; overlay.html sets the same.
const REC_W: f64 = 100.0;
const REC_LOCKED_W: f64 = 112.0;
/// Hover must rest this long before the idle bar opens into its hint (D1), so passing over it
/// on the way to the Dock does nothing.
const DWELL: Duration = Duration::from_millis(200);
/// A click this soon after the last one is the same click: a double-click must not start and finish.
const DEBOUNCE: Duration = Duration::from_millis(400);
pub const HIDE_FOR: Duration = Duration::from_secs(3600);
const CHIPS: [(&str, &str); 3] = [("too_quiet", "Too quiet, move the mic closer"), ("quiet", "Quiet voice"), ("no_cleanup", "No cleanup")];

// ---------- the pure model ----------

/// Everything outside the dictation that decides whether the pill is on screen.
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    /// The idle pill exists on macOS only this release.
    pub mac: bool,
    pub onboarded: bool,
    pub idle_on: bool,
    pub hidden_until: u64,
    pub now_ms: u64,
    /// Echotype's own window is the key window (D14).
    pub main_key: bool,
    /// ...showing Home or onboarding, which show the live state themselves.
    pub main_home: bool,
}

impl Ctx {
    fn hidden_for_now(&self) -> bool {
        self.now_ms < self.hidden_until
    }
}

/// On screen while dictating or showing a notice, and when idle only if the idle pill is on,
/// onboarding is done and it isn't hidden for the hour. Over Echotype's own focused window only
/// the live states show, and not on Home or onboarding, which show them already (D14).
pub fn visible(c: &Ctx, active: bool) -> bool {
    if c.main_key {
        return active && !c.main_home;
    }
    active || (c.mac && c.onboarded && c.idle_on && !c.hidden_for_now())
}

/// What the tray's pill item does now (D12/X5).
#[derive(Debug, PartialEq)]
pub enum TrayAct {
    /// "Show pill when idle": the setting is off; turns it on.
    ShowIdle,
    /// "Pill hidden until HH:MM · Show pill now": clears the hour.
    ShowNow,
    /// "Hide pill for 1 hour".
    Hide,
}

pub fn tray_act(c: &Ctx) -> TrayAct {
    if !c.idle_on {
        TrayAct::ShowIdle
    } else if c.hidden_for_now() {
        TrayAct::ShowNow
    } else {
        TrayAct::Hide
    }
}

/// The words overlay.js `primary()` puts on the pill for a state (plus the chip); only their
/// length matters here, for the hit area.
fn line(s: &DictationState) -> String {
    let main = match s.state {
        "error" => s.error.as_deref().unwrap_or("Something went wrong").lines().next().unwrap_or_default().to_string(),
        "hint" => s.message.clone().unwrap_or_default(),
        "done" if s.copied => "Copied, ⌘V to paste".into(),
        "done" => "Pasted".into(),
        "transcribing" => "Transcribing… 10s".into(),
        "cleaning" => "Cleaning up… 10s".into(),
        _ => String::new(),
    };
    match s.chip.and_then(|c| CHIPS.iter().find(|(k, _)| *k == c)) {
        Some((_, chip)) => format!("{main} · {chip}"),
        None => main,
    }
}

/// One line of 11 px SF Pro: about 6 pt a character plus 10 pt of padding each side.
// ponytail: estimated, not measured; have overlay.js report its measured size if the edges ever feel off.
fn text_w(chars: usize) -> f64 {
    (20.0 + 6.0 * chars as f64).min(MAX_W)
}

/// D2 primary-line priority: error > mic problem > result > hint.
fn rank(s: &DictationState) -> u8 {
    match (s.state, s.action) {
        ("error", _) => 4,
        ("hint", Some("microphone")) => 3,
        ("done", _) => 2,
        _ => 1,
    }
}

#[derive(Debug, Clone, Copy)]
struct Active {
    recording: bool,
    locked: bool,
    /// Characters on the line (notices, transcribing, cleaning).
    chars: usize,
    /// A notice: its rank and when it ends. None while recording/transcribing/cleaning.
    notice: Option<(u8, Instant)>,
}

/// What the pill is showing, its timers and hover. Lives in AppState: `emit_state` feeds it and
/// the loop ticks it.
#[derive(Debug, Default)]
pub struct Pill {
    active: Option<Active>,
    /// The live recording reads "Is your mic muted?" instead of the waveform.
    muted: bool,
    hover_since: Option<Instant>,
    last_tick: Option<Instant>,
}

impl Pill {
    /// Takes the next dictation state. False when the pill keeps what it shows: a lingering notice
    /// outranks it, or it is "idle" while a notice is still up. Recording, transcribing and
    /// cleaning always take over.
    pub fn on_state(&mut self, s: &DictationState, now: Instant) -> bool {
        let lingering = match self.active {
            Some(Active { notice: Some((r, until)), .. }) if now < until => Some(r),
            _ => None,
        };
        let chars = line(s).chars().count();
        self.active = match s.state {
            "idle" if lingering.is_some() => return false,
            "idle" => None,
            "recording" => Some(Active { recording: true, locked: s.locked, chars, notice: None }),
            "transcribing" | "cleaning" => Some(Active { recording: false, locked: false, chars, notice: None }),
            _ => {
                let r = rank(s);
                if lingering.is_some_and(|cur| cur > r) {
                    return false;
                }
                // A chip is a second thing to read, so the result stays up longer.
                let linger = Duration::from_millis(if s.chip.is_some() { 2500 } else { 1200 });
                Some(Active { recording: false, locked: false, chars, notice: Some((r, now + linger)) })
            }
        };
        if s.state != "recording" {
            self.muted = false;
        }
        true
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted && self.active.is_some_and(|a| a.recording);
    }

    /// Advances the timers with the cursor `inside` the capsule or not. A notice ends after its
    /// linger time, which stands still while the cursor rests on it (D2); the idle bar opens
    /// into its hint after DWELL of hover. Returns (active, expanded).
    pub fn tick(&mut self, now: Instant, inside: bool) -> (bool, bool) {
        let dt = self.last_tick.map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        self.last_tick = Some(now);
        if let Some(Active { notice: Some((_, until)), .. }) = &mut self.active {
            if inside {
                *until += dt;
            }
            if now >= *until {
                self.active = None;
            }
        }
        self.hover_since = if inside { Some(self.hover_since.unwrap_or(now)) } else { None };
        let expanded = self.active.is_none() && self.hover_since.is_some_and(|t| now.duration_since(t) >= DWELL);
        (self.active.is_some(), expanded)
    }

    /// When the showing notice ends (the loop must wake then even with the pill hidden).
    pub fn deadline(&self) -> Option<Instant> {
        self.active.and_then(|a| a.notice).map(|(_, until)| until)
    }

    /// The capsule's size in points, from the same state that picks the words (E-A3).
    pub fn capsule(&self, expanded: bool, hint_chars: usize) -> (f64, f64) {
        match self.active {
            None if expanded => (text_w(hint_chars), LINE_H),
            None => IDLE_HIT,
            // ✕ and ✓ (16 pt each) stay either side of the words.
            Some(a) if a.recording && self.muted => (text_w("Is your mic muted?".len()) + 36.0, LINE_H),
            Some(a) if a.recording => (if a.locked { REC_LOCKED_W } else { REC_W }, 20.0),
            Some(a) => (text_w(a.chars), LINE_H),
        }
    }
}

/// Pill clicks closer together than DEBOUNCE count once.
#[derive(Debug, Default)]
pub struct Debounce(Option<Instant>);

impl Debounce {
    pub fn accept(&mut self, now: Instant) -> bool {
        if self.0.is_some_and(|t| now.saturating_duration_since(t) < DEBOUNCE) {
            return false;
        }
        self.0 = Some(now);
        true
    }
}

// ---------- geometry, all in global points (top-left origin) ----------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    /// A monitor's or window's physical rect as Tauri reports it, back in points: Tauri
    /// multiplies their point coordinates by their OWN scale factor.
    pub fn from_phys(x: f64, y: f64, w: f64, h: f64, scale: f64) -> Rect {
        Rect { x: x / scale, y: y / scale, w: w / scale, h: h / scale }
    }

    pub fn contains(&self, (px, py): (f64, f64)) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

/// The cursor in points. Tauri multiplies the cursor's point position by the PRIMARY monitor's
/// scale, whichever monitor it is on, so on a Retina + 1x pair this differs from the window's scale.
pub fn cursor_pt((x, y): (f64, f64), primary_scale: f64) -> (f64, f64) {
    (x / primary_scale, y / primary_scale)
}

/// The capsule, centred in the window whose top-left is `win`.
pub fn capsule_rect(win: (f64, f64), (w, h): (f64, f64)) -> Rect {
    Rect { x: win.0 + (WIN.0 - w) / 2.0, y: win.1 + (WIN.1 - h) / 2.0, w, h }
}

/// Window top-left: bottom-centre of the visible frame `work`, MARGIN above it plus `dock`.
pub fn place(work: Rect, dock: f64) -> (f64, f64) {
    (work.x + (work.w - WIN.0) / 2.0, work.y + work.h - MARGIN - dock - (WIN.1 + LINE_H) / 2.0)
}

/// An auto-hidden Dock is not part of the visible frame, so the pill adds its height to stay out
/// of the reveal zone (D1). A visible Dock is already excluded.
// ponytail: tile size plus a fixed 16 pt for the Dock's own padding; read the Dock window's frame if it is ever off.
pub fn dock_offset(autohide: bool, orientation: Option<&str>, tilesize: Option<f64>) -> f64 {
    if !autohide || orientation.is_some_and(|o| o != "bottom") {
        return 0.0;
    }
    tilesize.unwrap_or(48.0) + 16.0
}

#[cfg(target_os = "macos")]
fn read_dock() -> f64 {
    let read = |key: &str| {
        let out = std::process::Command::new("defaults").args(["read", "com.apple.dock", key]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    dock_offset(read("autohide").as_deref() == Some("1"), read("orientation").as_deref(), read("tilesize").and_then(|t| t.parse().ok()))
}

#[cfg(not(target_os = "macos"))]
fn read_dock() -> f64 {
    0.0
}

// ---------- the window ----------

#[cfg(target_os = "macos")]
mod panel {
    tauri_nspanel::tauri_panel! {
        panel!(PillPanel {
            config: { can_become_key_window: false, can_become_main_window: false, is_floating_panel: true }
        })
    }
}

/// Kept out of screen capture unless the user asked to see it there. The toggle lives in the
/// macOS-only Pill settings, so elsewhere the overlay stays capturable as before.
fn protect(s: &Settings) -> bool {
    cfg!(target_os = "macos") && !s.pill_in_recordings
}

/// The overlay: transparent, click-through except over the capsule. On macOS a non-activating
/// NSPanel on every Space and over full-screen apps, so clicking it never takes focus from the
/// app being dictated into. Elsewhere a plain always-on-top window (dictating only).
pub fn create(app: &tauri::App, s: &Settings) -> tauri::Result<WebviewWindow> {
    let win = WebviewWindowBuilder::new(app, "overlay", WebviewUrl::App("overlay.html".into()))
        .title("Echotype")
        .inner_size(WIN.0, WIN.1)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .focusable(false)
        .accept_first_mouse(true)
        .content_protected(protect(s))
        .visible(false)
        .build()?;
    #[cfg(target_os = "macos")]
    {
        use tauri_nspanel::{CollectionBehavior, PanelLevel, StyleMask, WebviewWindowExt};
        let panel = win.to_panel::<panel::PillPanel>()?;
        panel.add_style_mask(StyleMask::empty().nonactivating_panel().into()).map_err(std::io::Error::other)?;
        panel.set_level(PanelLevel::Status.value() + 1); // above the menu bar and status items
        panel.set_collection_behavior(CollectionBehavior::new().can_join_all_spaces().full_screen_auxiliary().stationary().into());
        panel.set_hides_on_deactivate(false);
    }
    win.set_ignore_cursor_events(true)?;
    Ok(win)
}

/// What the overlay renders besides the dictation state itself.
#[derive(Serialize, Clone, PartialEq, Debug)]
struct View {
    /// Showing a dictation state; false = the idle bar.
    active: bool,
    /// The cursor is on the capsule (✕/✓ show, the notice timer pauses).
    hover: bool,
    /// The idle bar has opened into its hint.
    expand: bool,
    hint: String,
}

struct Screen {
    rect: Rect,
    work: Rect,
}

fn screens(app: &AppHandle) -> (Vec<Screen>, f64) {
    let primary = app.primary_monitor().ok().flatten().map_or(1.0, |m| m.scale_factor());
    let list = app.available_monitors().unwrap_or_default().into_iter().map(|m| {
        let (p, z, s, wa) = (m.position(), m.size(), m.scale_factor(), m.work_area());
        Screen {
            rect: Rect::from_phys(p.x as f64, p.y as f64, z.width as f64, z.height as f64, s),
            work: Rect::from_phys(wa.position.x as f64, wa.position.y as f64, wa.size.width as f64, wa.size.height as f64, s),
        }
    });
    (list.collect(), primary)
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn ctx(app: &AppHandle, s: &Settings) -> Ctx {
    Ctx {
        mac: cfg!(target_os = "macos"),
        onboarded: s.onboarded,
        idle_on: s.pill_idle,
        hidden_until: s.pill_hidden_until,
        now_ms: now_ms(),
        main_key: state(app).main_key.load(std::sync::atomic::Ordering::SeqCst),
        main_home: state(app).main_home.load(std::sync::atomic::Ordering::SeqCst) || !s.onboarded,
    }
}

/// "Hold ⌥ Space or click to dictate" (D10: the verb follows the mode).
fn hint(s: &Settings) -> String {
    let verb = if s.mode == "toggle" { "Press" } else { "Hold" };
    format!("{verb} {} or click to dictate", crate::key_label(s, cfg!(target_os = "macos")))
}

/// The loop: woken by every dictation state, settings change and main-window focus change, and
/// polling the cursor at ~15 Hz while the pill is on screen (4 Hz when the cursor is far away).
/// `ECHOTYPE_PILL_SCREEN_X` (tests) pins the pill to the monitor whose left edge is at that x
/// (global points); while that monitor is absent the pill never shows.
pub fn run(app: AppHandle, win: WebviewWindow, wake: Receiver<()>) {
    std::thread::spawn(move || {
        let pinned: Option<f64> = std::env::var("ECHOTYPE_PILL_SCREEN_X").ok().and_then(|x| x.parse().ok());
        let (mut shown, mut inside, mut taking_clicks) = (false, false, false);
        let (mut at, mut placed_on): ((f64, f64), Option<Rect>) = ((0.0, 0.0), None);
        let (mut emitted, mut protected, mut tray_text): (Option<View>, Option<bool>, String) = (None, None, String::new());
        let mut monitors: Option<(Instant, Vec<Screen>, f64)> = None;
        // The Dock's settings cost three `defaults` processes, so they are re-read once a minute at most.
        let mut dock: Option<(Instant, f64)> = None;
        loop {
            let now = Instant::now();
            let s = crate::current_settings(&app);
            let c = ctx(&app, &s);
            let hint = hint(&s);
            let (active, expanded, size, deadline) = {
                let st = state(&app);
                let mut p = lock(&st.pill);
                let (active, expanded) = p.tick(now, inside);
                (active, expanded, p.capsule(expanded, hint.chars().count()), p.deadline())
            };
            let mut want = visible(&c, active);
            let mut near = false;
            inside = false;
            if want {
                if monitors.as_ref().is_none_or(|(t, ..)| !shown || now.duration_since(*t) > Duration::from_secs(2)) {
                    let (list, primary) = screens(&app);
                    monitors = Some((now, list, primary));
                }
                let (_, list, primary) = monitors.as_ref().expect("just filled");
                let cursor = app.cursor_position().ok().map(|p| cursor_pt((p.x, p.y), *primary));
                // The primary monitor's point origin is (0, 0) on every platform.
                let primary_screen = || list.iter().find(|m| m.rect.x == 0.0 && m.rect.y == 0.0);
                let target = match pinned {
                    Some(x) => list.iter().find(|m| (m.rect.x - x).abs() < 1.0),
                    // ponytail: Windows/Linux (dictating-only pill) stay on the primary monitor; their
                    // cursor and monitor coordinates don't follow the macOS points maths above.
                    None if !cfg!(target_os = "macos") => primary_screen(),
                    None => cursor.and_then(|c| list.iter().find(|m| m.rect.contains(c))).or_else(|| placed_on.is_none().then(primary_screen).flatten()),
                };
                if pinned.is_some() && target.is_none() {
                    placed_on = None; // the pinned monitor went away
                }
                // F4: follow the cursor's display while idle; a recording or notice stays put.
                if let Some(t) = target
                    && (!shown || (!active && !expanded))
                    && placed_on != Some(t.work)
                {
                    if dock.is_none_or(|(t, _)| now.duration_since(t) > Duration::from_secs(60)) {
                        dock = Some((now, read_dock()));
                    }
                    at = place(t.work, dock.map_or(0.0, |(_, d)| d));
                    let _ = win.set_position(LogicalPosition::new(at.0, at.1));
                    placed_on = Some(t.work);
                }
                // Never shown where it was not placed (a pinned monitor that is gone).
                want = placed_on.is_some();
                let r = capsule_rect(at, size);
                if let Some(c) = cursor {
                    inside = r.contains(c);
                    near = (c.0 - (r.x + r.w / 2.0)).abs() < 200.0 && (c.1 - (r.y + r.h / 2.0)).abs() < 200.0;
                }
            }
            if inside != taking_clicks {
                let _ = win.set_ignore_cursor_events(!inside);
                taking_clicks = inside;
            }
            // Hide before the view drops to idle, so a finished notice never flashes the idle bar.
            if want != shown {
                let _ = if want { win.show() } else { win.hide() };
                shown = want;
            }
            let view = View { active, hover: inside, expand: expanded, hint };
            if emitted.as_ref() != Some(&view) {
                let _ = app.emit_to("overlay", "pill", &view);
                emitted = Some(view);
            }
            if protected != Some(protect(&s)) {
                protected = Some(protect(&s));
                let _ = win.set_content_protected(protect(&s));
            }
            let text = tray_label(&c);
            if text != tray_text {
                if let Some(item) = lock(&state(&app).tray_pill).as_ref() {
                    let _ = item.set_text(&text);
                }
                tray_text = text;
            }
            let wait = if want && (active || near) {
                Duration::from_millis(66)
            } else if want {
                Duration::from_millis(250)
            } else {
                // Hidden: sleep until something changes, the notice ends or the hour is up.
                let mut w = Duration::from_secs(60);
                if let Some(d) = deadline {
                    w = w.min(d.saturating_duration_since(now));
                }
                if c.hidden_until > c.now_ms {
                    w = w.min(Duration::from_millis(c.hidden_until - c.now_ms));
                }
                w
            };
            if let Err(RecvTimeoutError::Disconnected) = wake.recv_timeout(wait) {
                return;
            }
        }
    });
}

// ---------- tray item and right-click menu ----------

fn tray_label(c: &Ctx) -> String {
    match tray_act(c) {
        TrayAct::ShowIdle => "Show pill when idle".into(),
        TrayAct::ShowNow => format!("Pill hidden until {} · Show pill now", hhmm(c.hidden_until)),
        TrayAct::Hide => "Hide pill for 1 hour".into(),
    }
}

/// Local wall-clock "14:05" for a time in ms since the epoch.
fn hhmm(ms: u64) -> String {
    #[cfg(target_os = "macos")]
    {
        let t = (ms / 1000) as libc::time_t;
        // SAFETY: localtime_r only writes the tm we own.
        let tm = unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&t, &mut tm);
            tm
        };
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let m = ms / 60_000;
        format!("{:02}:{:02} UTC", (m / 60) % 24, m % 60)
    }
}

/// Commits a settings change from a menu. Off the main thread, where menu events arrive, so
/// nothing the commit does can wait on the main thread.
fn set(app: &AppHandle, f: impl FnOnce(&mut Settings) + Send + 'static) {
    let app = app.clone();
    std::thread::spawn(move || {
        if let Err(e) = crate::commit_settings(&app, |old| {
            let mut s = old.clone();
            f(&mut s);
            s
        }) {
            eprintln!("echotype: {e}");
        }
    });
}

pub fn on_tray(app: &AppHandle) {
    let s = crate::current_settings(app);
    match tray_act(&ctx(app, &s)) {
        TrayAct::ShowIdle => set(app, |s| s.pill_idle = true),
        TrayAct::ShowNow => set(app, |s| s.pill_hidden_until = 0),
        TrayAct::Hide => hide_for_hour(app),
    }
}

fn hide_for_hour(app: &AppHandle) {
    set(app, |s| s.pill_hidden_until = now_ms() + HIDE_FOR.as_millis() as u64);
}

/// The languages Settings offers (main.js LANGS), for the pill menu.
const LANGS: [(&str, &str); 28] = [
    ("auto", "Auto-detect"), ("en", "English"), ("hi", "Hindi"), ("ar", "Arabic"), ("bn", "Bengali"), ("zh", "Chinese"), ("nl", "Dutch"),
    ("fr", "French"), ("de", "German"), ("gu", "Gujarati"), ("id", "Indonesian"), ("it", "Italian"), ("ja", "Japanese"), ("kn", "Kannada"),
    ("ko", "Korean"), ("ml", "Malayalam"), ("mr", "Marathi"), ("pa", "Punjabi"), ("pl", "Polish"), ("pt", "Portuguese"), ("ru", "Russian"),
    ("es", "Spanish"), ("ta", "Tamil"), ("te", "Telugu"), ("tr", "Turkish"), ("uk", "Ukrainian"), ("ur", "Urdu"), ("vi", "Vietnamese"),
];

/// The idle pill's right-click menu, native and handled here, so the overlay needs no settings commands (F3).
pub fn show_menu(app: &AppHandle, win: &WebviewWindow) -> tauri::Result<()> {
    use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
    let s = crate::current_settings(app);
    let mics = Submenu::with_id(app, "pill-mics", "Microphone", true)?;
    mics.append(&CheckMenuItem::with_id(app, "pill-mic:", "System default", true, s.input_device.is_none(), None::<&str>)?)?;
    for d in crate::audio::input_devices() {
        let on = s.input_device.as_deref() == Some(d.name.as_str());
        mics.append(&CheckMenuItem::with_id(app, format!("pill-mic:{}", d.name), &d.name, true, on, None::<&str>)?)?;
    }
    let langs = Submenu::with_id(app, "pill-langs", "Language", true)?;
    for (code, name) in LANGS {
        langs.append(&CheckMenuItem::with_id(app, format!("pill-lang:{code}"), name, true, s.language == code, None::<&str>)?)?;
    }
    let has_last = lock(&state(app).last_output).is_some();
    let menu = Menu::with_items(app, &[
        &mics,
        &langs,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::with_id(app, "pill-paste", "Paste last transcript", has_last, None::<&str>)?,
        &MenuItem::with_id(app, "pill-history", "History…", true, None::<&str>)?,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::with_id(app, "pill-hide", "Hide for 1 hour", true, None::<&str>)?,
        &MenuItem::with_id(app, "pill-settings", "Settings…", true, None::<&str>)?,
    ])?;
    win.popup_menu(&menu)
}

pub fn on_menu(app: &AppHandle, id: &str) {
    if let Some(name) = id.strip_prefix("pill-mic:") {
        let name = name.to_string();
        set(app, move |s| s.input_device = (!name.is_empty()).then_some(name));
    } else if let Some(code) = id.strip_prefix("pill-lang:") {
        let code = code.to_string();
        set(app, move |s| s.language = code);
    } else {
        match id {
            "pill-paste" => crate::paste_last(app, true),
            "pill-history" => crate::open_page(app, "history"),
            "pill-settings" => crate::open_page(app, "settings"),
            "pill-hide" => hide_for_hour(app),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(state: &'static str) -> DictationState {
        DictationState { state, ..Default::default() }
    }

    fn ctx() -> Ctx {
        Ctx { mac: true, onboarded: true, idle_on: true, hidden_until: 0, now_ms: 1_000_000, main_key: false, main_home: false }
    }

    #[test]
    fn visibility_follows_every_input() {
        let c = ctx();
        assert!(visible(&c, false), "idle pill on by default");
        assert!(!visible(&Ctx { idle_on: false, ..c }, false), "idle off: nothing while idle");
        assert!(visible(&Ctx { idle_on: false, ..c }, true), "idle off still shows while dictating");
        let hidden = Ctx { hidden_until: c.now_ms + 1, ..c };
        assert!(!visible(&hidden, false) && visible(&hidden, true), "hide for an hour hides the idle pill only");
        assert!(visible(&Ctx { now_ms: c.now_ms + 1, ..hidden }, false), "and the hour ends by itself");
        let home = Ctx { main_key: true, main_home: true, ..c };
        assert!(!visible(&home, false) && !visible(&home, true), "Echotype's Home shows the live state itself (D14)");
        let page = Ctx { main_key: true, ..c };
        assert!(!visible(&page, false), "no idle bar over Echotype's own window");
        assert!(visible(&page, true), "but a recording or notice still shows over its other pages");
        assert!(!visible(&Ctx { onboarded: false, ..c }, false), "no idle pill before onboarding finishes");
        assert!(!visible(&Ctx { mac: false, ..c }, false) && visible(&Ctx { mac: false, ..c }, true), "elsewhere only while dictating");
    }

    #[test]
    fn the_tray_item_says_what_it_will_do() {
        let c = ctx();
        assert_eq!(tray_act(&c), TrayAct::Hide);
        assert_eq!(tray_act(&Ctx { hidden_until: c.now_ms + 60_000, ..c }), TrayAct::ShowNow);
        assert_eq!(tray_act(&Ctx { idle_on: false, hidden_until: c.now_ms + 60_000, ..c }), TrayAct::ShowIdle);
        assert!(tray_label(&Ctx { hidden_until: c.now_ms + 60_000, ..c }).starts_with("Pill hidden until "));
    }

    #[test]
    fn a_notice_lingers_pauses_on_hover_and_then_goes_back_to_idle() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut p = Pill::default();
        assert!(p.on_state(&DictationState { state: "done", ..Default::default() }, t0));
        assert_eq!(p.tick(ms(1000), false), (true, false));
        assert_eq!(p.tick(ms(1199), false).0, true);
        assert_eq!(p.tick(ms(1200), false).0, false, "1.2 s without a chip");

        let mut p = Pill::default();
        p.on_state(&DictationState { state: "done", chip: Some("no_cleanup"), ..Default::default() }, t0);
        assert!(p.tick(ms(2000), false).0 && !p.tick(ms(2500), false).0, "2.5 s with a chip");

        let mut p = Pill::default();
        p.on_state(&st("done"), t0);
        p.tick(ms(500), false);
        p.tick(ms(1500), true); // a second on it
        assert!(p.tick(ms(2100), true).0, "paused while hovered");
        assert!(p.tick(ms(2300), false).0 && !p.tick(ms(2900), false).0, "the remaining time runs once the cursor leaves");
        assert!(p.deadline().is_none());
    }

    #[test]
    fn message_precedence_keeps_the_more_important_line() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_millis(300);
        let err = DictationState { state: "error", error: Some("Microphone access is off".into()), ..Default::default() };
        let nothing = DictationState { state: "hint", message: Some("Nothing to paste yet".into()), ..Default::default() };
        let mut p = Pill::default();
        assert!(p.on_state(&err, t0));
        assert!(!p.on_state(&nothing, later), "a hint does not replace an error");
        assert!(!p.on_state(&st("done"), later), "nor does a result");
        assert!(!p.on_state(&st("idle"), later), "idle waits for the notice to end");
        assert!(p.on_state(&st("recording"), later), "a new recording always takes over");
        assert!(p.on_state(&st("idle"), later), "and Esc ends it at once");

        let muted = DictationState { state: "hint", message: Some("No audio. Check your mic".into()), action: Some("microphone"), ..Default::default() };
        let mut p = Pill::default();
        p.on_state(&st("done"), t0);
        assert!(p.on_state(&muted, later), "mic problem > result");
        assert!(!p.on_state(&st("done"), later));
        assert!(p.on_state(&err, later), "error > mic problem");
        assert!(p.on_state(&nothing, t0 + Duration::from_secs(5)), "anything once the notice is over");

        // Copied + No cleanup + Too quiet: one line, one chip (the chip itself is ranked in lib.rs).
        let s = DictationState { state: "done", copied: true, chip: Some("too_quiet"), ..Default::default() };
        assert_eq!(line(&s), "Copied, ⌘V to paste · Too quiet, move the mic closer");
        assert_eq!(line(&err), "Microphone access is off");
    }

    #[test]
    fn hover_must_dwell_before_the_idle_bar_opens() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut p = Pill::default();
        assert_eq!(p.tick(t0, true), (false, false));
        assert_eq!(p.tick(ms(150), true), (false, false));
        assert_eq!(p.tick(ms(200), true), (false, true));
        assert_eq!(p.capsule(true, 32), (text_w(32), LINE_H));
        assert_eq!(p.tick(ms(260), false), (false, false), "leaving closes it");
        assert_eq!(p.capsule(false, 32), IDLE_HIT);
        p.on_state(&st("recording"), ms(300));
        assert_eq!(p.tick(ms(600), true), (true, false), "only the idle bar expands (D11)");
        assert_eq!(p.capsule(false, 32), (REC_W, 20.0));
        p.set_muted(true);
        assert!(p.capsule(false, 32).0 > REC_LOCKED_W);
        p.on_state(&DictationState { state: "recording", locked: true, ..Default::default() }, ms(700));
        assert!(p.capsule(false, 32).0 > REC_LOCKED_W, "locking keeps the muted line");
        p.set_muted(false);
        assert_eq!(p.capsule(false, 32), (REC_LOCKED_W, 20.0));
        p.on_state(&st("transcribing"), ms(800));
        p.set_muted(true);
        assert_eq!(p.capsule(false, 32).1, LINE_H, "muted only applies to a live recording");
    }

    #[test]
    fn hit_testing_in_points_on_a_retina_plus_1x_pair() {
        // Retina primary (scale 2) at the origin, 1512x982 pt; a 1x 2560x1600 monitor to its right.
        let retina = Rect::from_phys(0.0, 0.0, 3024.0, 1964.0, 2.0);
        let ext = Rect::from_phys(1512.0, -700.0, 2560.0, 1600.0, 1.0);
        assert_eq!(retina, Rect { x: 0.0, y: 0.0, w: 1512.0, h: 982.0 });
        // The pill bottom-centre of the external visible frame (menu bar 25 pt, no Dock).
        let at = place(Rect { x: 1512.0, y: -675.0, w: 2560.0, h: 1575.0 }, 0.0);
        assert_eq!(at, (2662.0, 1575.0 - 675.0 - 10.0 - 27.0));
        let r = capsule_rect(at, IDLE_HIT);
        assert_eq!((r.x, r.w), (2768.0, 48.0));
        // The cursor on the capsule centre, as Tauri reports it: points x PRIMARY scale (2), not 1.
        let centre = (r.x + 24.0, r.y + 7.0);
        let phys = (centre.0 * 2.0, centre.1 * 2.0);
        assert!(r.contains(cursor_pt(phys, 2.0)) && ext.contains(cursor_pt(phys, 2.0)));
        assert!(!r.contains(cursor_pt(phys, 1.0)), "using the 1x monitor's own scale misses the capsule");
        // Beside the capsule passes through.
        assert!(!r.contains((r.x - 1.0, centre.1)) && !r.contains((r.x + r.w, centre.1)) && !r.contains((centre.0, r.y + r.h)));
        // On a scale-1 primary the same maths holds.
        assert!(r.contains(cursor_pt(centre, 1.0)));
    }

    #[test]
    fn an_auto_hidden_dock_at_the_bottom_lifts_the_pill() {
        assert_eq!(dock_offset(false, None, Some(49.0)), 0.0, "a visible Dock is outside the visible frame already");
        assert_eq!(dock_offset(true, None, Some(49.0)), 65.0, "no orientation key means bottom");
        assert_eq!(dock_offset(true, Some("bottom"), None), 64.0);
        assert_eq!(dock_offset(true, Some("left"), Some(49.0)), 0.0);
        let work = Rect { x: 0.0, y: 25.0, w: 1512.0, h: 957.0 };
        assert_eq!(place(work, 65.0).1, place(work, 0.0).1 - 65.0);
    }

    #[test]
    fn the_menu_offers_the_languages_settings_offers() {
        let js = include_str!("../../src/main.js");
        let list = js.split("const LANGS = [").nth(1).unwrap().split("];").next().unwrap();
        let codes: Vec<&str> = list.split("['").skip(1).map(|e| e.split('\'').next().unwrap()).collect();
        assert_eq!(codes, LANGS.map(|(c, _)| c).to_vec());
    }

    #[test]
    fn pill_clicks_are_debounced() {
        let t0 = Instant::now();
        let mut d = Debounce::default();
        assert!(d.accept(t0));
        assert!(!d.accept(t0 + Duration::from_millis(150)), "a double-click is one click");
        assert!(d.accept(t0 + Duration::from_millis(450)));
        assert!(!d.accept(t0 + Duration::from_millis(500)));
    }
}
