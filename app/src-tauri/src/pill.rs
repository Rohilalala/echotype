//! The always-on pill. One owner decides what the overlay shows and whether it is on screen:
//! a pure model (visibility, message precedence, notice timers, hover dwell, hit testing in
//! points, Dock offset, click debounce, dragging) and a ~15 Hz loop that applies it to the window
//! (and, in menu-bar mode, to the status item instead).

use crate::settings::Settings;
use crate::tray::Glyph;
use crate::{DictationState, lock, state};
use serde::Serialize;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::menu::Menu;
use tauri::{AppHandle, Emitter, LogicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder, Wry};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

/// The overlay page is laid out in CSS pixels (every size below but MARGIN) and shown `zoom()`
/// times larger (webview zoom, Settings → Pill size), so orb, capsule and card scale together;
/// the idle ball also has its own scale. The loop sets both from settings; the placement maths
/// reads them everywhere, hence one static.
static SIZE: std::sync::Mutex<(f64, f64)> = std::sync::Mutex::new((1.5, 1.0));

pub fn zoom() -> f64 {
    lock(&SIZE).0
}

fn ball() -> f64 {
    lock(&SIZE).1
}
/// The overlay page: the "nowhere to paste" card plus room for its soft shadow (CSS px). Every
/// capsule sits on one line ANCHOR above the page's bottom edge; the card grows up from it.
const PAGE: (f64, f64) = (260.0, 100.0);
/// The overlay window in points.
pub fn win() -> (f64, f64) {
    (PAGE.0 * zoom(), PAGE.1 * zoom())
}
const ANCHOR: f64 = 16.0;
/// The card: as wide as the widest notice; why it could not paste on one 15 px line, the text on
/// one or two 15 px lines, then a 22 px row (overlay.html).
const CARD_W: f64 = MAX_W;
// ponytail: ~7 pt a character at 11 px; a long word wraps sooner, so this errs towards two lines.
const CARD_LINE: usize = 26;
/// Gap between the tallest capsule and the bottom of the visible frame (or the auto-hidden Dock), in points.
const MARGIN: f64 = 10.0;
const LINE_H: f64 = 22.0;
const MAX_W: f64 = 240.0;
/// The idle ball is 10 px across (times its own scale) to look at and at least 24x22 to hit.
fn idle_hit() -> (f64, f64) {
    let d = 10.0 * ball();
    ((d + 14.0).max(24.0), (d + 12.0).max(22.0))
}
/// The thinking orb (recording, transcribing, cleaning); overlay.js draws it this size.
const ORB: f64 = 36.0;
/// While recording, ✕ and ✓ sit either side of the orb, so its hit area is this wide.
const REC_W: f64 = 96.0;
/// The idle ball brightens while the cursor is this close to it (points, centre to cursor).
const NEAR: f64 = 80.0;
/// Hover must rest this long before the idle bar opens into its hint (D1), so passing over it
/// on the way to the Dock does nothing.
const DWELL: Duration = Duration::from_millis(200);
/// A click this soon after the last one is the same click: a double-click must not start and finish.
const DEBOUNCE: Duration = Duration::from_millis(400);
pub const HIDE_FOR: Duration = Duration::from_secs(3600);
/// The card's countdown ring (overlay.html runs the same 6 s).
const CARD_FOR: u64 = 6000;
/// A detected call's offer stays longer (overlay.js sets its ring to the same 15 s).
const OFFER_FOR: u64 = 15000;
/// A press that moves further than this (points) is a drag: it moves the pill and is never a click.
const DRAG_SLOP: f64 = 4.0;
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
    /// ...showing Home, which shows the live state itself. Onboarding wants the real pill (its "try it" step).
    pub main_home: bool,
    /// Show Echotype in the menu bar: the status item is the indicator and the pill never shows.
    pub menubar: bool,
}

impl Ctx {
    fn hidden_for_now(&self) -> bool {
        self.now_ms < self.hidden_until
    }
}

/// On screen while dictating or showing a notice, and when idle only if the idle pill is on,
/// onboarding is done and it isn't hidden for the hour. Over Echotype's own focused window only
/// the live states show, and not on Home, which shows them already (D14).
pub fn visible(c: &Ctx, active: bool) -> bool {
    if c.mac && c.menubar {
        return false;
    }
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

/// The words overlay.js `primary()` puts on the pill for a state, plus the chip: what the
/// menu-bar menu says on its first line.
fn said(s: &DictationState) -> String {
    let main = match s.state {
        "error" => s.error.as_deref().unwrap_or("Something went wrong").lines().next().unwrap_or_default().to_string(),
        "hint" => s.message.clone().unwrap_or_default(),
        "done" if s.card => {
            let t = s.text.as_deref().unwrap_or_default().lines().next().unwrap_or_default();
            if t.chars().count() > 60 { format!("{}…", t.chars().take(59).collect::<String>()) } else { t.to_string() }
        }
        "done" if s.copied => "Copied, ⌘V to paste".into(),
        "done" => "Pasted".into(),
        "transcribing" => "Transcribing…".into(),
        "cleaning" => "Cleaning up…".into(),
        _ => String::new(),
    };
    match s.chip.and_then(|c| CHIPS.iter().find(|(k, _)| *k == c)) {
        Some((_, chip)) => format!("{main} · {chip}"),
        None => main,
    }
}

/// The same words as wide as the pill can get (seconds after "Transcribing…"); only their
/// length matters here, for the hit area.
fn line(s: &DictationState) -> String {
    let words = said(s);
    match s.state {
        "transcribing" | "cleaning" => words.replacen('…', "… 10s", 1),
        _ => words,
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
    /// Characters on the line (notices, transcribing, cleaning).
    chars: usize,
    /// A notice: its rank and when it ends. None while recording/transcribing/cleaning.
    notice: Option<(u8, Instant)>,
    /// The notice is the "nowhere to paste" card; `chars` counts its transcript.
    card: bool,
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
    /// The live input level, 0..1 (the menu-bar waveform).
    pub meter: f32,
    /// What the pill says, for the menu-bar menu.
    said: Option<Said>,
}

#[derive(Debug)]
struct Said {
    text: String,
    rank: u8,
    card: bool,
}

impl Said {
    /// A problem (rank 3+) or a transcript with nowhere to go stays in the menu after the pill
    /// lets go of it, until the next dictation.
    fn sticks(&self) -> bool {
        self.rank >= 3 || self.card
    }
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
            "recording" => Some(Active { recording: true, chars, notice: None, card: false }),
            "transcribing" | "cleaning" => Some(Active { recording: false, chars, notice: None, card: false }),
            _ => {
                let r = rank(s);
                if lingering.is_some_and(|cur| cur > r) {
                    return false;
                }
                // A chip is a second thing to read, so the result stays up longer; the card has
                // a whole transcript and a button.
                let linger = Duration::from_millis(if s.offer.is_some() { OFFER_FOR } else if s.card { CARD_FOR } else if s.chip.is_some() { 2500 } else { 1200 });
                // Wide characters (CJK, emoji) count double: about two Latin letters each.
                let chars = if s.card { s.text.as_deref().unwrap_or_default().chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum() } else { chars };
                Some(Active { recording: false, chars, notice: Some((r, now + linger)), card: s.card })
            }
        };
        if s.state != "recording" {
            self.muted = false;
        }
        self.said = match s.state {
            "idle" => self.said.take().filter(|t| t.sticks()),
            "recording" => None,
            "transcribing" | "cleaning" => Some(Said { text: said(s), rank: 0, card: false }),
            // An offer's card has no transcript, so the menu offers no copy for it.
            _ => Some(Said { text: said(s), rank: rank(s), card: s.card && s.offer.is_none() }),
        };
        true
    }

    pub fn set_level(&mut self, muted: bool, meter: f32) {
        self.muted = muted && self.active.is_some_and(|a| a.recording);
        self.meter = meter;
    }

    /// The status item's glyph in menu-bar mode: the same state the pill would show, so the check
    /// and the warning stay exactly as long as the pill's notice would.
    pub fn glyph(&self) -> Glyph {
        match self.active {
            None => Glyph::Idle,
            Some(a) if a.recording => Glyph::Wave,
            Some(Active { notice: None, .. }) => Glyph::Working,
            // Nothing was pasted: the menu has the text and a copy item.
            Some(Active { card: true, .. }) => Glyph::Warning,
            Some(Active { notice: Some((2, _)), .. }) => Glyph::Done,
            Some(Active { notice: Some((r, _)), .. }) if r >= 3 => Glyph::Warning,
            Some(_) => Glyph::Idle,
        }
    }

    /// The menu-bar menu's first line: what the pill says now, or the last problem.
    pub fn menu_line(&self) -> Option<&str> {
        self.said.as_ref().map(|t| t.text.as_str()).filter(|t| !t.is_empty())
    }

    /// The last transcript had nowhere to go: the menu-bar menu offers to copy it.
    pub fn menu_copy(&self) -> bool {
        self.said.as_ref().is_some_and(|t| t.card)
    }

    pub fn card_up(&self) -> bool {
        self.active.is_some_and(|a| a.card)
    }

    /// ✕ or Esc on the card: back to the idle pill at once.
    pub fn close_card(&mut self) {
        if self.card_up() {
            self.active = None;
        }
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
                self.said = self.said.take().filter(|t| t.sticks());
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
            None => idle_hit(),
            // ✕ and ✓ (16 pt each) stay either side of the words.
            Some(a) if a.recording && self.muted => (text_w("Is your mic muted?".len()) + 36.0, LINE_H),
            Some(a) if a.recording => (REC_W, ORB),
            Some(Active { notice: None, .. }) => (ORB, ORB),
            Some(a) if a.card => (CARD_W, 57.0 + 15.0 * if a.chars > CARD_LINE { 2.0 } else { 1.0 }),
            Some(a) => (text_w(a.chars), LINE_H),
        }
    }
}

/// Pill clicks closer together than DEBOUNCE count once.
#[derive(Debug, Default)]
pub struct Debounce {
    last: Option<Instant>,
    /// When the last accepted click started a dictation, so a double-click can take it back.
    started: Option<Instant>,
}

impl Debounce {
    pub fn accept(&mut self, now: Instant) -> bool {
        if self.last.is_some_and(|t| now.saturating_duration_since(t) < DEBOUNCE) {
            return false;
        }
        self.last = Some(now);
        self.started = None;
        true
    }

    pub fn started(&mut self, now: Instant) {
        self.started = Some(now);
    }

    /// A double-click (reset the pill's place) takes back the dictation its first click started.
    pub fn undo_start(&mut self, now: Instant) -> bool {
        self.started.take().is_some_and(|t| now.saturating_duration_since(t) < DEBOUNCE)
    }
}

/// One press on the pill: where the cursor went down (points), the window's top-left then
/// (filled in by the loop), whether it has moved past DRAG_SLOP and when the button came up.
#[derive(Debug, Clone, Copy)]
pub struct Drag {
    from: (f64, f64),
    at: Option<(f64, f64)>,
    moved: bool,
    ended: Option<Instant>,
}

impl Drag {
    pub fn new(from: (f64, f64)) -> Drag {
        Drag { from, at: None, moved: false, ended: None }
    }

    /// The mouse-up that ends this press is a drop, not a click: it moved past DRAG_SLOP (seen
    /// by the loop, or by the cursor now if the loop has not looked yet). Only for a second
    /// after it ends, so a stale drag never eats a later click on ✓.
    pub fn is_drop(&self, now: Instant, cursor: Option<(f64, f64)>) -> bool {
        match self.ended {
            None => self.moved || cursor.is_some_and(|c| is_drag(self.from, c)),
            Some(t) => self.moved && now.saturating_duration_since(t) < Duration::from_secs(1),
        }
    }
}

pub fn is_drag(from: (f64, f64), to: (f64, f64)) -> bool {
    (to.0 - from.0).hypot(to.1 - from.1) > DRAG_SLOP
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

/// The capsule in the window whose top-left is `win`: centred across, centred on the line
/// ANCHOR above the bottom edge, and a card (taller than a line) growing up from that line's bottom.
/// `(w, h)` in CSS px; the rect in points.
pub fn capsule_rect(win: (f64, f64), (w, h): (f64, f64)) -> Rect {
    let (z, (ww, wh)) = (zoom(), self::win());
    let (w, h) = (w * z, h * z);
    let bottom = win.1 + wh - (ANCHOR - LINE_H.min(h / z) / 2.0) * z;
    Rect { x: win.0 + (ww - w) / 2.0, y: bottom - h, w, h }
}

/// Window top-left: bottom-centre of the visible frame `work`, the tallest line MARGIN above it plus `dock`.
pub fn place(work: Rect, dock: f64) -> (f64, f64) {
    let (ww, wh) = win();
    (work.x + (work.w - ww) / 2.0, work.y + work.h - MARGIN - dock - (wh - (ANCHOR - LINE_H / 2.0) * zoom()))
}

/// The window's top-left kept inside `work` (a display's visible frame), so no part of the pill,
/// even the card, can go off-screen, under the menu bar or behind a visible Dock.
// ponytail: so the idle bar stops ~68 pt below the top edge (room for the card above it); move the
// window down while a card is up near the top if that ever bothers anyone.
pub fn clamp(work: Rect, (x, y): (f64, f64)) -> (f64, f64) {
    let (ww, wh) = win();
    (x.clamp(work.x, work.x + (work.w - ww).max(0.0)), y.clamp(work.y, work.y + (work.h - wh).max(0.0)))
}

/// Dragged within this many points of a display's horizontal centre, the pill snaps onto it.
const SNAP: f64 = 24.0;

/// The window's x while dragging on `work`: pulled onto the centre line when close to it.
pub fn snap_x(work: Rect, x: f64) -> f64 {
    let centre = work.x + (work.w - win().0) / 2.0;
    if (x - centre).abs() <= SNAP { centre } else { x }
}

/// Where in `work` the window is, as fractions of the room it has there, so a display that
/// changes resolution or Dock size keeps the pill in the same place relative to its edges.
pub fn normalize(work: Rect, at: (f64, f64)) -> [f64; 2] {
    let (x, y) = clamp(work, at);
    let frac = |d: f64, room: f64| if room > 0.0 { d / room } else { 0.5 };
    let (ww, wh) = win();
    [frac(x - work.x, work.w - ww), frac(y - work.y, work.h - wh)]
}

/// Window top-left on a display: where it was dropped there, else bottom-centre above the Dock.
pub fn spot(work: Rect, saved: Option<[f64; 2]>, dock: f64) -> (f64, f64) {
    match saved {
        Some([nx, ny]) => {
            let (ww, wh) = win();
            clamp(work, (work.x + nx * (work.w - ww), work.y + ny * (work.h - wh)))
        }
        None => place(work, dock),
    }
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
        .inner_size(win().0, win().1)
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
    win.set_zoom(zoom())?;
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
    /// Picked up and moving: the capsule lifts.
    drag: bool,
    /// The cursor is within NEAR of the capsule: the idle ball shows at full strength.
    near: bool,
    /// The idle ball's own scale (Settings → Pill).
    ball: f64,
    /// Fade the idle ball while the pointer is away (Settings → Pill).
    fade: bool,
}

struct Screen {
    /// The display's name, which survives rearranging displays (its key in `pill_positions`).
    // ponytail: two displays with the same name share one saved place; add the serial if that bites.
    key: String,
    rect: Rect,
    work: Rect,
}

fn screens(app: &AppHandle) -> (Vec<Screen>, f64) {
    let primary = app.primary_monitor().ok().flatten().map_or(1.0, |m| m.scale_factor());
    let list = app.available_monitors().unwrap_or_default().into_iter().map(|m| {
        let (p, z, s, wa) = (m.position(), m.size(), m.scale_factor(), m.work_area());
        Screen {
            key: m.name().cloned().unwrap_or_else(|| format!("{}x{}", z.width, z.height)),
            rect: Rect::from_phys(p.x as f64, p.y as f64, z.width as f64, z.height as f64, s),
            work: Rect::from_phys(wa.position.x as f64, wa.position.y as f64, wa.size.width as f64, wa.size.height as f64, s),
        }
    });
    (list.collect(), primary)
}

/// The display the pill goes on. Following the cursor: the one under it (none while it is
/// between displays, so the pill stays where it is, unless it was never placed). Not following:
/// the one it was last dropped on, the primary while that one is unplugged or before any drop.
fn choose<'a>(list: &'a [Screen], cursor: Option<(f64, f64)>, follow: bool, home: Option<&str>, placed: bool) -> Option<&'a Screen> {
    // The primary monitor's point origin is (0, 0) on every platform.
    let primary = || list.iter().find(|m| m.rect.x == 0.0 && m.rect.y == 0.0);
    if !follow {
        return home.and_then(|k| list.iter().find(|m| m.key == k)).or_else(primary);
    }
    cursor.and_then(|c| list.iter().find(|m| m.rect.contains(c))).or_else(|| if placed { None } else { primary() })
}

/// The cursor in global points, or None if the OS won't say.
fn cursor_now(app: &AppHandle) -> Option<(f64, f64)> {
    let primary = app.primary_monitor().ok().flatten().map_or(1.0, |m| m.scale_factor());
    app.cursor_position().ok().map(|p| cursor_pt((p.x, p.y), primary))
}

/// Left button state from the window server, so a drag ends even when the button comes up
/// outside the overlay (the loop moves the window after the cursor, so it can fall behind).
#[cfg(target_os = "macos")]
fn button_down() -> bool {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
    }
    // SAFETY: a pure query; 0 = kCGEventSourceStateCombinedSessionState, 0 = kCGMouseButtonLeft.
    unsafe { CGEventSourceButtonState(0, 0) }
}

#[cfg(not(target_os = "macos"))]
fn button_down() -> bool {
    false
}

/// The button went down on the pill (not on ✕ or ✓): the loop moves the pill with the cursor
/// until it comes up. The dictating-only pill elsewhere stays put.
pub fn grab(app: &AppHandle) {
    if cfg!(target_os = "macos")
        && let Some(c) = cursor_now(app)
    {
        *lock(&state(app).drag) = Some(Drag::new(c));
        let _ = state(app).pill_wake.send(());
    }
}

/// A mouse-up on the pill that ends a drag: not a click.
pub fn is_drop(app: &AppHandle, now: Instant) -> bool {
    let drag = *lock(&state(app).drag);
    drag.is_some_and(|d| d.is_drop(now, cursor_now(app)))
}

/// ✕ or Esc on the "nowhere to paste" card.
pub fn dismiss_card(app: &AppHandle) {
    lock(&state(app).pill).close_card();
    let _ = state(app).pill_wake.send(());
}

/// Double-click: back to bottom-centre on this display.
pub fn reset(app: &AppHandle) {
    let Some(c) = cursor_now(app) else { return };
    if let Some(m) = screens(app).0.into_iter().find(|m| m.rect.contains(c)) {
        set(app, move |s| {
            s.pill_positions.remove(&m.key);
        });
    }
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
        main_home: state(app).main_home.load(std::sync::atomic::Ordering::SeqCst),
        menubar: in_menu_bar(s),
    }
}

pub fn in_menu_bar(s: &Settings) -> bool {
    cfg!(target_os = "macos") && s.show_in == "menubar"
}

/// "Hold ⌥ Space or click to dictate" (D10: the verb follows the mode).
fn hint(s: &Settings) -> String {
    let verb = if s.mode == "toggle" { "Press" } else { "Hold" };
    format!("{verb} {} or click to dictate", crate::key_label(s, cfg!(target_os = "macos")))
}

/// The loop: woken by every dictation state, settings change and main-window focus change, and
/// polling the cursor at ~15 Hz while the pill is on screen (4 Hz when the cursor is far away,
/// ~60 Hz while it is being dragged). In menu-bar mode it drives the status item instead, ~8 Hz
/// while listening or working.
/// `ECHOTYPE_PILL_SCREEN_X` (tests) pins the pill to the monitor whose left edge is at that x
/// (global points); while that monitor is absent the pill never shows.
pub fn run(app: AppHandle, win: WebviewWindow, wake: Receiver<()>) {
    std::thread::spawn(move || {
        let pinned: Option<f64> = std::env::var("ECHOTYPE_PILL_SCREEN_X").ok().and_then(|x| x.parse().ok());
        let (mut shown, mut inside, mut taking_clicks) = (false, false, false);
        // Where the window is, the visible frame it was placed in and the saved place it used.
        let (mut at, mut placed_on, mut placed_saved): ((f64, f64), Option<Rect>, Option<[f64; 2]>) = ((0.0, 0.0), None, None);
        let (mut emitted, mut protected, mut tray_text): (Option<View>, Option<bool>, String) = (None, None, String::new());
        let mut monitors: Option<(Instant, Vec<Screen>, f64)> = None;
        // The Dock's settings cost three `defaults` processes, so they are re-read once a minute at most.
        let mut dock: Option<(Instant, f64)> = None;
        let mut tray = crate::tray::Tray::default();
        let mut esc = false;
        loop {
            let now = Instant::now();
            let s = crate::current_settings(&app);
            // Settings → Pill size changed: resize and re-zoom the window, then place it afresh.
            if (s.pill_size, s.ball_size) != *lock(&SIZE) {
                *lock(&SIZE) = (s.pill_size, s.ball_size);
                let (w, h) = self::win();
                let _ = win.set_size(tauri::LogicalSize::new(w, h));
                let _ = win.set_zoom(s.pill_size);
                placed_on = None;
            }
            let c = ctx(&app, &s);
            let hint = hint(&s);
            let (active, expanded, size, deadline, glyph, meter, line, copy, card) = {
                let st = state(&app);
                let mut p = lock(&st.pill);
                let (active, expanded) = p.tick(now, inside);
                let line = p.menu_line().map(String::from);
                (active, expanded, p.capsule(expanded, hint.chars().count()), p.deadline(), p.glyph(), p.meter, line, p.menu_copy(), p.card_up())
            };
            // Esc cancels a recording and closes the card, so it is Echotype's only while one is
            // running or the other is on screen. This loop is its only owner.
            let want_esc = state(&app).recording.load(std::sync::atomic::Ordering::SeqCst) || (card && visible(&c, true));
            if want_esc != esc {
                let gs = app.global_shortcut();
                let _ = if want_esc { gs.register(crate::escape()) } else { gs.unregister(crate::escape()) };
                esc = want_esc;
            }
            let mut want = visible(&c, active);
            let mut near = false;
            let mut close = false;
            let mut held = false;
            let mut dropped: Option<(String, [f64; 2])> = None;
            inside = false;
            if want {
                if monitors.as_ref().is_none_or(|(t, ..)| !shown || now.duration_since(*t) > Duration::from_secs(2)) {
                    let (list, primary) = screens(&app);
                    monitors = Some((now, list, primary));
                }
                let (_, list, primary) = monitors.as_ref().expect("just filled");
                let cursor = app.cursor_position().ok().map(|p| cursor_pt((p.x, p.y), *primary));
                let target = match pinned {
                    Some(x) => list.iter().find(|m| (m.rect.x - x).abs() < 1.0),
                    // ponytail: Windows/Linux (dictating-only pill) stay on the primary monitor; their
                    // cursor and monitor coordinates don't follow the macOS points maths above.
                    None if !cfg!(target_os = "macos") => choose(list, None, false, None, false),
                    None => choose(list, cursor, s.pill_follow_cursor, s.pill_screen.as_deref(), placed_on.is_some()),
                };
                if pinned.is_some() && target.is_none() {
                    placed_on = None; // the pinned monitor went away
                }
                let st = state(&app);
                let mut drag = lock(&st.drag);
                if let Some(d) = drag.as_mut().filter(|d| d.ended.is_none()) {
                    // Dragging: the window follows the cursor on whichever display it is over (the
                    // pinned one in tests), clamped to that display's visible frame.
                    held = true;
                    let start = *d.at.get_or_insert(at);
                    if let Some(c) = cursor {
                        d.moved |= is_drag(d.from, c);
                        let on = if pinned.is_some() { target } else { list.iter().find(|m| m.rect.contains(c)) };
                        if let Some(m) = on.filter(|_| d.moved) {
                            let next = clamp(m.work, (start.0 + c.0 - d.from.0, start.1 + c.1 - d.from.1));
                            let snapped = snap_x(m.work, next.0);
                            if snapped != next.0 && at.0 != snapped {
                                let _ = app.run_on_main_thread(crate::platform::snap_feedback); // a click under the finger as it lands on the centre
                            }
                            at = (snapped, next.1);
                            let _ = win.set_position(LogicalPosition::new(at.0, at.1));
                            placed_on = Some(m.work);
                        }
                    }
                    if !button_down() {
                        d.ended = Some(now);
                        held = false;
                        if d.moved
                            && let Some(m) = list.iter().find(|m| Some(m.work) == placed_on)
                        {
                            dropped = Some((m.key.clone(), normalize(m.work, at)));
                        }
                    }
                } else if let Some(t) = target {
                    // F4: follow the cursor's display while idle; a recording or notice stays put.
                    // A new saved place for this display (a drop, a double-click) applies at once.
                    let saved = if cfg!(target_os = "macos") { s.pill_positions.get(&t.key).copied() } else { None };
                    let other = placed_on != Some(t.work);
                    if (other && (!shown || (!active && !expanded))) || (!other && saved != placed_saved) {
                        if dock.is_none_or(|(t, _)| now.duration_since(t) > Duration::from_secs(60)) {
                            dock = Some((now, read_dock()));
                        }
                        at = spot(t.work, saved, dock.map_or(0.0, |(_, d)| d));
                        let _ = win.set_position(LogicalPosition::new(at.0, at.1));
                        eprintln!("echotype: pill on {} at {:.0},{:.0}", t.key, at.0, at.1);
                        placed_on = Some(t.work);
                        placed_saved = saved;
                    }
                }
                drop(drag);
                // Never shown where it was not placed (a pinned monitor that is gone).
                want = placed_on.is_some();
                let r = capsule_rect(at, size);
                if let Some(c) = cursor {
                    // While held the overlay keeps the mouse, so the mouse-up (and a double-click) reach it.
                    inside = held || r.contains(c);
                    let (dx, dy) = (c.0 - (r.x + r.w / 2.0), c.1 - (r.y + r.h / 2.0));
                    near = dx.abs() < 200.0 && dy.abs() < 200.0;
                    close = dx.hypot(dy) <= NEAR;
                }
            }
            if !want
                && let Some(d) = lock(&state(&app).drag).as_mut()
            {
                d.ended.get_or_insert(now); // the pill went away mid-press
            }
            if let Some((key, n)) = dropped {
                // Saved here, before the next pass reads settings, so the pill never jumps back.
                placed_saved = Some(n);
                if let Err(e) = crate::commit_settings(&app, |old| {
                    let mut s = old.clone();
                    s.pill_positions.insert(key.clone(), n);
                    s.pill_screen = Some(key);
                    s
                }) {
                    eprintln!("echotype: {e}");
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
            // Picked up, the idle bar is a bar again: the hint is for hovering, not carrying.
            let view = View { active, hover: inside, expand: expanded && !held, hint, drag: held && lock(&state(&app).drag).is_some_and(|d| d.moved), near: close || held, ball: s.ball_size, fade: s.pill_fade };
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
            let animating = matches!(glyph, Glyph::Wave | Glyph::Working); // the menu-bar icon moves in both modes
            if c.mac {
                tray.sync(&app, &s, c.menubar, glyph, meter, line.as_deref(), copy);
            }
            let wait = if held {
                Duration::from_millis(16)
            } else if want && (active || near) {
                Duration::from_millis(66)
            } else if want {
                Duration::from_millis(250)
            } else if animating {
                Duration::from_millis(125)
            } else {
                // Hidden: sleep until something changes, the notice ends or the hour is up (and
                // in the menu bar look for new microphones now and then).
                let mut w = Duration::from_secs(if c.menubar { 5 } else { 60 });
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
    let mics: Vec<String> = crate::audio::input_devices().into_iter().map(|d| d.name).collect();
    win.popup_menu(&menu(app, &mics, None)?)
}

/// What the menu-bar menu adds: its first line (what the pill would say), whether a dictation
/// is running, and a copy item when the last transcript had nowhere to go.
pub struct Bar<'a> {
    pub line: Option<&'a str>,
    pub recording: bool,
    pub copy: bool,
}

/// The pill's menu. In menu-bar mode it is the status item's menu too: what the pill would say
/// on top, dictation first and Quit last, and no "Hide for 1 hour" (there is no pill to hide).
pub fn menu(app: &AppHandle, mics: &[String], bar: Option<Bar>) -> tauri::Result<Menu<Wry>> {
    use tauri::menu::{CheckMenuItem, MenuItem, PredefinedMenuItem, Submenu};
    let s = crate::current_settings(app);
    let menu = Menu::new(app)?;
    if let Some(b) = &bar {
        if let Some(line) = b.line {
            menu.append(&MenuItem::with_id(app, "bar-line", line, false, None::<&str>)?)?;
            if b.copy {
                menu.append(&MenuItem::with_id(app, "pill-copy", "Copied to clipboard? Click to copy", true, None::<&str>)?)?;
            }
            menu.append(&PredefinedMenuItem::separator(app)?)?;
        }
        // Ids the tray's own handler already answers (lib.rs create_tray).
        menu.append(&MenuItem::with_id(app, "toggle", if b.recording { "Finish dictation" } else { "Start dictation" }, true, None::<&str>)?)?;
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }
    let list = Submenu::with_id(app, "pill-mics", "Microphone", true)?;
    list.append(&CheckMenuItem::with_id(app, "pill-mic:", "System default", true, s.input_device.is_none(), None::<&str>)?)?;
    for name in mics {
        let on = s.input_device.as_deref() == Some(name.as_str());
        list.append(&CheckMenuItem::with_id(app, format!("pill-mic:{name}"), name, true, on, None::<&str>)?)?;
    }
    let langs = Submenu::with_id(app, "pill-langs", "Language", true)?;
    for (code, name) in LANGS {
        langs.append(&CheckMenuItem::with_id(app, format!("pill-lang:{code}"), name, true, s.language == code, None::<&str>)?)?;
    }
    let has_last = lock(&state(app).last_output).is_some();
    menu.append_items(&[
        &list,
        &langs,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::with_id(app, "pill-paste", "Paste last transcript", has_last, None::<&str>)?,
        &MenuItem::with_id(app, "pill-history", "History…", true, None::<&str>)?,
        &MenuItem::with_id(app, "pill-meeting", crate::meeting::menu_label(), true, None::<&str>)?,
        &PredefinedMenuItem::separator(app)?,
    ])?;
    if bar.is_none() {
        menu.append(&MenuItem::with_id(app, "pill-hide", "Hide for 1 hour", true, None::<&str>)?)?;
    }
    menu.append(&MenuItem::with_id(app, "pill-settings", "Settings…", true, None::<&str>)?)?;
    if bar.is_some() {
        menu.append(&MenuItem::with_id(app, "quit", "Quit Echotype", true, None::<&str>)?)?;
    }
    Ok(menu)
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
            "pill-copy" => {
                if let Err(e) = crate::copy_last(app) {
                    eprintln!("echotype: {e}");
                }
            }
            "pill-history" => crate::open_page(app, "history"),
            "pill-meeting" => crate::meeting::toggle(app),
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
        Ctx { mac: true, onboarded: true, idle_on: true, hidden_until: 0, now_ms: 1_000_000, main_key: false, main_home: false, menubar: false }
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
    fn in_menu_bar_mode_the_pill_never_shows() {
        let bar = Ctx { menubar: true, ..ctx() };
        assert!(!visible(&bar, false) && !visible(&bar, true), "not idle, not while dictating or showing a notice");
        assert!(!visible(&Ctx { main_key: true, ..bar }, true));
        assert!(visible(&Ctx { mac: false, ..bar }, true), "elsewhere there is no menu-bar mode: the pill still shows while dictating");
        let mut s = Settings::default();
        assert!(!in_menu_bar(&s));
        s.show_in = "menubar".into();
        assert_eq!(in_menu_bar(&s), cfg!(target_os = "macos"));
    }

    #[test]
    fn the_status_item_shows_the_state_the_pill_would() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut p = Pill::default();
        assert_eq!(p.glyph(), Glyph::Idle);
        p.on_state(&st("recording"), t0);
        assert_eq!(p.glyph(), Glyph::Wave);
        p.set_level(false, 0.7);
        assert_eq!(p.meter, 0.7);
        p.on_state(&st("transcribing"), ms(10));
        assert_eq!((p.glyph(), p.menu_line()), (Glyph::Working, Some("Transcribing…")));
        p.on_state(&st("cleaning"), ms(20));
        assert_eq!((p.glyph(), p.menu_line()), (Glyph::Working, Some("Cleaning up…")));
        p.on_state(&st("done"), ms(30));
        assert_eq!((p.glyph(), p.menu_line()), (Glyph::Done, Some("Pasted")));
        p.tick(ms(1300), false);
        assert_eq!((p.glyph(), p.menu_line()), (Glyph::Idle, None), "the check is brief, like the pill's notice");

        let err = DictationState { state: "error", error: Some("Microphone access is off\nmore".into()), ..Default::default() };
        p.on_state(&err, ms(2000));
        assert_eq!((p.glyph(), p.menu_line()), (Glyph::Warning, Some("Microphone access is off")));
        p.tick(ms(3300), false);
        assert_eq!(p.glyph(), Glyph::Idle);
        assert_eq!(p.menu_line(), Some("Microphone access is off"), "a problem stays in the menu after the glyph settles");
        p.on_state(&st("idle"), ms(3400));
        assert_eq!(p.menu_line(), Some("Microphone access is off"));
        p.on_state(&st("recording"), ms(3500));
        assert_eq!(p.menu_line(), None, "until the next dictation");

        let muted = DictationState { state: "hint", message: Some("No audio. Check your mic".into()), action: Some("microphone"), ..Default::default() };
        let mut p = Pill::default();
        p.on_state(&muted, t0);
        assert_eq!(p.glyph(), Glyph::Warning, "a mic problem warns too");
        let mut p = Pill::default();
        p.on_state(&DictationState { state: "hint", message: Some("Didn't catch that".into()), ..Default::default() }, t0);
        assert_eq!((p.glyph(), p.menu_line()), (Glyph::Idle, Some("Didn't catch that")), "a plain hint is only the menu line");
    }

    #[test]
    fn a_press_that_moves_more_than_4_points_is_a_drag_not_a_click() {
        let t0 = Instant::now();
        let from = (100.0, 100.0);
        assert!(!is_drag(from, (104.0, 100.0)) && !is_drag(from, (102.8, 102.8)), "4 pt of wobble is still a click");
        assert!(is_drag(from, (104.1, 100.0)) && is_drag(from, (97.0, 97.0)));
        // On a Retina primary the cursor arrives in pixels: 6 px is 3 pt, not a drag.
        assert!(!is_drag(from, cursor_pt((206.0, 200.0), 2.0)));
        assert!(is_drag(from, cursor_pt((210.0, 200.0), 2.0)));

        let mut d = Drag::new(from);
        assert!(!d.is_drop(t0, Some((101.0, 99.0))), "a click");
        assert!(d.is_drop(t0, Some((140.0, 100.0))), "moved, even before the loop saw it");
        let mut click = Drag::new(from);
        click.ended = Some(t0);
        assert!(!click.is_drop(t0 + Duration::from_millis(500), Some((140.0, 100.0))), "a click that ended is no drop, wherever the cursor went (✓ after a click)");
        d.moved = true;
        d.ended = Some(t0);
        assert!(d.is_drop(t0 + Duration::from_millis(300), Some(from)), "the drop's own mouse-up");
        assert!(!d.is_drop(t0 + Duration::from_secs(2), Some(from)), "never a later click");
    }

    #[test]
    fn the_card_counts_down_6_s_and_holds_while_hovered() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let card = |text: &str| DictationState { state: "done", text: Some(text.into()), card: true, ..Default::default() };
        let mut p = Pill::default();
        assert!(p.on_state(&card("Hello there."), t0));
        assert!(p.card_up());
        assert_eq!(p.capsule(false, 0), (CARD_W, 72.0), "the reason and one line of text");
        assert!(p.tick(ms(5999), false).0, "still up just before 6 s");
        assert!(!p.tick(ms(6000), false).0, "and gone at 6 s");
        assert!(p.menu_copy() && p.menu_line() == Some("Hello there."), "the menu-bar menu keeps it to copy");

        let mut p = Pill::default();
        p.on_state(&card(&"word ".repeat(30)), t0);
        assert_eq!(p.capsule(false, 0), (CARD_W, 87.0), "two lines, then an ellipsis");
        let mut wide = Pill::default();
        wide.on_state(&card(&"漢字".repeat(8)), t0);
        assert_eq!(wide.capsule(false, 0).1, 87.0, "16 wide characters fill two lines");
        p.tick(ms(2000), false);
        for t in (2066..=5000).step_by(66) {
            assert!(p.tick(ms(t), true).0, "paused while hovered (at {t} ms)");
        }
        // The cursor rested on it from 2 s to ~5 s, so ~4 s are left once it moves away.
        assert!(p.tick(ms(8900), false).0);
        assert!(!p.tick(ms(9100), false).0);

        let mut p = Pill::default();
        p.on_state(&card("Hi"), t0);
        p.close_card();
        assert!(!p.tick(ms(10), false).0, "✕ or Esc closes it at once");
        assert!(!p.on_state(&DictationState { state: "hint", message: Some("Nothing to paste yet".into()), ..Default::default() }, ms(20)) || !p.card_up());
        let mut p = Pill::default();
        p.on_state(&card("Hi"), t0);
        assert!(!p.on_state(&st("idle"), ms(10)), "the card outlasts the idle that follows it");
        assert!(p.on_state(&st("recording"), ms(20)) && !p.card_up(), "a new dictation takes over");
    }

    #[test]
    fn a_dragged_pill_stays_on_screen_and_out_from_under_the_menu_bar() {
        // Retina primary (scale 2) at the origin; a 1x 2560x1440 display to its right whose
        // visible frame starts under a 25 pt menu bar and stops above a 70 pt Dock.
        let ext = Rect::from_phys(1512.0, -300.0, 2560.0, 1440.0, 1.0);
        let work = Rect { x: ext.x, y: ext.y + 25.0, w: ext.w, h: ext.h - 25.0 - 70.0 };
        // A drag that started on the capsule and ran past the top-left corner: the cursor comes in
        // pixels at the PRIMARY scale (2), so (3000, -2000) px is (1500, -1000) pt.
        let (from, start) = ((2800.0, 800.0), (2662.0, 1020.0));
        let c = cursor_pt((3000.0, -2000.0), 2.0);
        assert_eq!(c, (1500.0, -1000.0));
        let at = clamp(work, (start.0 + c.0 - from.0, start.1 + c.1 - from.1));
        assert_eq!(at, (1512.0, -275.0), "pinned to the visible frame's corner, below the menu bar");
        // Past the bottom-right: the whole window, so even the widest notice, stays above the Dock.
        let at = clamp(work, (5000.0, 5000.0));
        assert_eq!(at, (1512.0 + 2560.0 - win().0, -300.0 + 1440.0 - 70.0 - win().1));
        let r = capsule_rect(at, (MAX_W, LINE_H));
        assert!(r.x + r.w <= ext.x + ext.w && r.y + r.h <= work.y + work.h);
        // Inside the frame nothing changes.
        assert_eq!(clamp(work, (2000.0, 300.0)), (2000.0, 300.0));
    }

    #[test]
    fn each_display_remembers_where_the_pill_was_dropped() {
        let laptop = Rect { x: 0.0, y: 25.0, w: 1512.0, h: 957.0 };
        let portrait = Rect { x: -800.0, y: 80.0, w: 800.0, h: 1255.0 };
        assert_eq!(spot(laptop, None, 0.0), place(laptop, 0.0), "no saved place: bottom-centre");
        assert_eq!(spot(laptop, None, 65.0), place(laptop, 65.0), "...above an auto-hidden Dock");
        let dropped = (-500.0, 300.0);
        let n = normalize(portrait, dropped);
        assert_eq!(spot(portrait, Some(n), 0.0), dropped, "restored exactly where it was dropped");
        assert_eq!(normalize(portrait, (-9999.0, 9999.0)), [0.0, 1.0], "a drop is saved on screen");
        // The same fractions on a display that got bigger keep the pill as far across it.
        let bigger = Rect { h: 1555.0, ..portrait };
        let again = spot(bigger, Some(n), 0.0);
        assert_eq!(again.0, dropped.0);
        assert!(((again.1 - bigger.y) / (bigger.h - win().1) - n[1]).abs() < 1e-9);
        // A saved place never puts the window off-screen, even from a hand-edited file.
        assert_eq!(spot(laptop, Some([3.0, -1.0]), 0.0), (1512.0 - win().0, 25.0));
        // Double-click forgets this display's place; the others keep theirs.
        let mut s = Settings::default();
        s.pill_positions.insert("Bimawen-16".into(), n);
        s.pill_positions.insert("Built-in Retina Display".into(), [0.1, 0.1]);
        s.pill_positions.remove("Bimawen-16");
        assert_eq!(spot(portrait, s.pill_positions.get("Bimawen-16").copied(), 0.0), place(portrait, 0.0));
        assert_eq!(spot(laptop, s.pill_positions.get("Built-in Retina Display").copied(), 0.0), clamp(laptop, (0.1 * (1512.0 - win().0), 25.0 + 0.1 * (957.0 - win().1))));
    }

    #[test]
    fn the_pill_follows_the_cursor_or_stays_on_its_own_screen() {
        let screen = |key: &str, x: f64, y: f64, w: f64, h: f64| Screen { key: key.into(), rect: Rect { x, y, w, h }, work: Rect { x, y, w, h } };
        let list = vec![screen("main", 0.0, 0.0, 2560.0, 1440.0), screen("laptop", 2560.0, 779.0, 1512.0, 982.0), screen("portrait", -800.0, 105.0, 800.0, 1280.0)];
        let key = |m: Option<&Screen>| m.map(|m| m.key.clone());
        let on_laptop = Some((3000.0, 1000.0));
        // Following (the default): the display under the cursor, with that display's saved place.
        assert_eq!(key(choose(&list, on_laptop, true, Some("portrait"), true)), Some("laptop".into()));
        assert_eq!(key(choose(&list, Some((-10.0, 200.0)), true, None, true)), Some("portrait".into()));
        assert_eq!(key(choose(&list, Some((5000.0, 0.0)), true, None, true)), None, "between displays: stays put");
        assert_eq!(key(choose(&list, None, true, None, false)), Some("main".into()), "never placed: the primary");
        // Not following: the display it was last dropped on, wherever the cursor goes.
        assert_eq!(key(choose(&list, on_laptop, false, Some("portrait"), true)), Some("portrait".into()));
        assert_eq!(key(choose(&list, Some((100.0, 100.0)), false, Some("portrait"), true)), Some("portrait".into()));
        assert_eq!(key(choose(&list, on_laptop, false, None, true)), Some("main".into()), "never dragged: the primary");
        // That display unplugged: the primary, and back once it returns.
        let unplugged = vec![screen("main", 0.0, 0.0, 2560.0, 1440.0), screen("laptop", 2560.0, 779.0, 1512.0, 982.0)];
        assert_eq!(key(choose(&unplugged, on_laptop, false, Some("portrait"), true)), Some("main".into()));
        assert_eq!(key(choose(&list, on_laptop, false, Some("portrait"), true)), Some("portrait".into()));
    }

    #[test]
    fn a_double_click_takes_back_the_dictation_its_first_click_started() {
        let t0 = Instant::now();
        let mut d = Debounce::default();
        assert!(d.accept(t0));
        d.started(t0);
        assert!(d.undo_start(t0 + Duration::from_millis(300)));
        assert!(!d.undo_start(t0 + Duration::from_millis(310)), "only once");
        assert!(d.accept(t0 + Duration::from_millis(500)));
        d.started(t0 + Duration::from_millis(500));
        assert!(!d.undo_start(t0 + Duration::from_millis(1000)), "a slow second click is a new click, not a double-click");
        assert!(d.accept(t0 + Duration::from_millis(2000)));
        assert!(!d.undo_start(t0 + Duration::from_millis(2100)), "a click that finished a dictation is not taken back");
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
        assert_eq!(p.capsule(false, 32), idle_hit());
        p.on_state(&st("recording"), ms(300));
        assert_eq!(p.tick(ms(600), true), (true, false), "only the idle bar expands (D11)");
        assert_eq!(p.capsule(false, 32), (REC_W, ORB), "the orb with ✕ and ✓ beside it");
        p.set_level(true, 0.0);
        assert_eq!(p.capsule(false, 32).1, LINE_H, "muted: a line of text instead");
        p.on_state(&DictationState { state: "recording", locked: true, ..Default::default() }, ms(700));
        assert_eq!(p.capsule(false, 32).1, LINE_H, "locking keeps the muted line");
        p.set_level(false, 0.0);
        assert_eq!(p.capsule(false, 32), (REC_W, ORB));
        p.on_state(&st("transcribing"), ms(800));
        p.set_level(true, 0.0);
        assert_eq!(p.capsule(false, 32), (ORB, ORB), "thinking is the orb alone; muted only applies to a live recording");
    }

    #[test]
    fn hit_testing_in_points_on_a_retina_plus_1x_pair() {
        // Retina primary (scale 2) at the origin, 1512x982 pt; a 1x 2560x1600 monitor to its right.
        let retina = Rect::from_phys(0.0, 0.0, 3024.0, 1964.0, 2.0);
        let ext = Rect::from_phys(1512.0, -700.0, 2560.0, 1600.0, 1.0);
        assert_eq!(retina, Rect { x: 0.0, y: 0.0, w: 1512.0, h: 982.0 });
        // The pill bottom-centre of the external visible frame (menu bar 25 pt, no Dock).
        let at = place(Rect { x: 1512.0, y: -675.0, w: 2560.0, h: 1575.0 }, 0.0);
        assert_eq!(at, (2597.0, 1575.0 - 675.0 - 10.0 - 142.5), "a 390x150 pt window (260x100 at 1.5x)");
        let r = capsule_rect(at, idle_hit());
        assert_eq!((r.x, r.w, r.h), (2774.0, 36.0, 33.0), "hit areas scale with the zoom");
        // Every line capsule's bottom sits MARGIN above the visible frame; the card grows up from it.
        let line_bottom = |size: (f64, f64)| { let r = capsule_rect(at, size); r.y + r.h };
        assert_eq!(line_bottom((MAX_W, LINE_H)), 1575.0 - 675.0 - 10.0);
        assert_eq!(line_bottom((CARD_W, 87.0)), 1575.0 - 675.0 - 10.0);
        assert_eq!(capsule_rect(at, idle_hit()).y, capsule_rect(at, (MAX_W, LINE_H)).y, "every line centred on one line");
        assert_eq!(line_bottom((ORB, ORB)), 1575.0 - 675.0 - 10.0, "the orb grows up from it too");
        assert!(capsule_rect(at, (CARD_W, 87.0)).y >= at.1, "the card fits in the window");
        // The cursor on the capsule centre, as Tauri reports it: points x PRIMARY scale (2), not 1.
        let centre = (r.x + 18.0, r.y + 16.5);
        let phys = (centre.0 * 2.0, centre.1 * 2.0);
        assert!(r.contains(cursor_pt(phys, 2.0)) && ext.contains(cursor_pt(phys, 2.0)));
        assert!(!r.contains(cursor_pt(phys, 1.0)), "using the 1x monitor's own scale misses the capsule");
        // Beside the capsule passes through.
        assert!(!r.contains((r.x - 1.0, centre.1)) && !r.contains((r.x + r.w, centre.1)) && !r.contains((centre.0, r.y + r.h)));
        // On a scale-1 primary the same maths holds.
        assert!(r.contains(cursor_pt(centre, 1.0)));
    }

    #[test]
    fn dragging_near_the_middle_snaps_to_the_centre() {
        let work = Rect { x: 0.0, y: 25.0, w: 1512.0, h: 900.0 };
        let centre = (1512.0 - win().0) / 2.0;
        assert_eq!(snap_x(work, centre + 20.0), centre);
        assert_eq!(snap_x(work, centre - 24.0), centre);
        assert_eq!(snap_x(work, centre + 30.0), centre + 30.0, "further out it moves freely");
        assert_eq!(snap_x(work, centre), place(work, 0.0).0, "the same spot the pill starts in");
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
