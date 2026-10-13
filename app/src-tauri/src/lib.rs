mod api;
mod audio;
mod cleanup;
mod cloud;
mod hotkey;
mod platform;
mod settings;
mod stt;

use audio::{LevelStats, Quality};
use hotkey::{Act, Ev};
use serde::Serialize;
use settings::{HistoryEntry, Settings};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;

const MIN_SAMPLES: usize = audio::RATE as usize * 3 / 10; // under 0.3 s there is nothing to transcribe
/// A recording this long stops itself and is transcribed as usual (toggle mode left on, a stuck key).
// ponytail: the whole clip stays in memory, ~460 MB at 30 min of 48 kHz f32 plus the 16 kHz copy;
// stream-resample into the 16 kHz buffer if hands-free recordings get that long in practice.
const MAX_RECORDING: Duration = Duration::from_secs(30 * 60);
/// Windows (50 ms) of level stats skipped after Echotype's own start/lock sound (<= 120 ms plus output latency).
const SOUND_WINDOWS: u32 = 4;

struct AppState {
    settings: Mutex<Settings>,
    dictation: mpsc::Sender<Cmd>,
    /// Running local API: port, the token it accepts, and the sender whose drop shuts it down.
    api: Mutex<Option<(u16, String, tokio::sync::oneshot::Sender<()>)>>,
    overlay_gen: AtomicU64,
    /// The last recording whose transcription failed, kept so `retry_last` can run it again.
    // ponytail: only the last one, in memory; a failed-recordings folder if users need more than one retry.
    last_failed: Mutex<Option<Vec<f32>>>,
    /// The last dictated text, for paste-last and copy-last. Memory only, so it works with history
    /// off; cleared with history and when history is switched off.
    last_output: Mutex<Option<String>>,
    /// Model downloads in flight: id → fraction done, so a press can say "Model downloading 42%".
    downloads: Mutex<HashMap<String, f64>>,
    /// Shortcuts that failed to register at launch, shown in Settings.
    shortcut_errors: Mutex<Vec<String>>,
}

enum Cmd {
    Key(Ev),
    Retry,
    /// Recording `n`'s microphone went away: transcribe what it captured.
    Lost(u64),
}

#[derive(Serialize, Clone, Default)]
struct DictationState {
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    /// The recording is kept and `retry_last` can transcribe it again.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    retry: bool,
    /// `text` went to the clipboard instead of being pasted.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    copied: bool,
    /// State "hint": a short notice ("Didn't catch that", "Hold ⌥ Space to dictate", ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Secondary chip, one at most: "too_quiet" > "quiet" > "no_cleanup".
    #[serde(skip_serializing_if = "Option::is_none")]
    chip: Option<&'static str>,
    /// A hold-mode recording locked hands-free (double-tap or click).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    locked: bool,
    /// What the settings window offers to fix it: "microphone" | "accessibility" | "model" | "cleanup" | "settings".
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<&'static str>,
}

fn state(app: &AppHandle) -> tauri::State<'_, AppState> {
    app.state::<AppState>()
}

pub(crate) fn current_settings(app: &AppHandle) -> Settings {
    state(app).settings.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------- dictation ----------

fn emit_state(app: &AppHandle, s: DictationState) {
    let busy = matches!(s.state, "recording" | "transcribing" | "cleaning");
    // A hint can come with no recording before it ("Nothing to paste yet"), so anything but idle shows.
    let show = s.state != "idle";
    // A chip is a second thing to read, so the result stays up longer.
    let linger = Duration::from_millis(if s.chip.is_some() { 2500 } else { 1200 });
    let _ = app.emit("dictation-state", s);
    let st = state(app);
    let generation = st.overlay_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let Some(overlay) = app.get_webview_window("overlay") else { return };
    if show {
        let _ = overlay.show();
    }
    if !busy {
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(linger);
            if state(&app).overlay_gen.load(Ordering::SeqCst) == generation {
                let _ = overlay.hide();
            }
        });
    }
}

fn simple(state: &'static str) -> DictationState {
    DictationState { state, ..Default::default() }
}

fn hint(message: impl Into<String>, action: Option<&'static str>) -> DictationState {
    DictationState { state: "hint", message: Some(message.into()), action, ..Default::default() }
}

/// The D2 secondary chip: too quiet beats quiet voice beats no cleanup.
fn chip(quality: Option<Quality>, no_cleanup: bool) -> Option<&'static str> {
    match quality {
        Some(Quality::TooQuiet) => Some("too_quiet"),
        Some(Quality::Quiet) => Some("quiet"),
        None => no_cleanup.then_some("no_cleanup"),
    }
}

/// Which window page fixes an error, from the error text the pipeline produced.
fn error_action(e: &str) -> Option<&'static str> {
    if e.starts_with("Not enough free memory") || e.contains("could not load") || e.contains("Choose a speech model") || e.contains("multilingual model") {
        Some("model")
    } else {
        None
    }
}

/// "⌥ Space", "fn", "Right ⌘", "Ctrl+Alt+K": how copy names the key the user dictates with.
fn key_label(s: &Settings, mac: bool) -> String {
    let side = |k: &str| format!("Right {k}");
    match (mac, s.talk_key.as_str()) {
        (true, "fn") if hotkey::read_globe_setting() && talk_tap_active() => return "fn".into(),
        (true, "right_option") if talk_tap_active() => return side("⌥"),
        (true, "right_command") if talk_tap_active() => return side("⌘"),
        (true, "right_control") if talk_tap_active() => return side("⌃"),
        _ => {}
    }
    accel_label(&s.hotkey, mac)
}

fn accel_label(accel: &str, mac: bool) -> String {
    let part = |k: &str| -> String {
        let k = k.strip_prefix("Key").or_else(|| k.strip_prefix("Digit")).unwrap_or(k);
        match (mac, k) {
            (true, "CommandOrControl" | "CmdOrCtrl" | "Command" | "Cmd" | "Super" | "Meta") => "⌘".into(),
            (true, "Control" | "Ctrl") => "⌃".into(),
            (true, "Alt" | "Option") => "⌥".into(),
            (true, "Shift") => "⇧".into(),
            (false, "CommandOrControl" | "CmdOrCtrl" | "Control") => "Ctrl".into(),
            (false, "Super" | "Meta") => "Win".into(),
            _ => k.to_string(),
        }
    };
    accel.split('+').map(part).collect::<Vec<_>>().join(if mac { " " } else { "+" })
}

fn talk_tap_active() -> bool {
    #[cfg(target_os = "macos")]
    return hotkey::tap::active();
    #[cfg(not(target_os = "macos"))]
    false
}

fn escape() -> Shortcut {
    Shortcut::new(None, Code::Escape)
}

fn send(app: &AppHandle, cmd: Cmd) {
    let _ = state(app).dictation.send(cmd);
}

/// A live recording and its level statistics.
struct Live {
    rec: audio::Recorder,
    stats: Arc<Mutex<LevelStats>>,
    lost: Arc<AtomicBool>,
}

/// One thread owns the recorder and the talk-key state machine, so every key, click, Esc and
/// timer is handled strictly in order.
fn spawn_dictation(app: AppHandle) -> mpsc::Sender<Cmd> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    std::thread::spawn(move || {
        let busy = Arc::new(AtomicBool::new(false));
        let mut fsm = hotkey::Fsm::default();
        let mut live: Option<Live> = None;
        for cmd in rx {
            let s = current_settings(&app);
            let ev = match cmd {
                Cmd::Key(ev) => ev,
                Cmd::Lost(id) => Ev::AutoStop(id),
                Cmd::Retry => {
                    if live.is_none() && !busy.load(Ordering::SeqCst) {
                        if let Some(pcm) = lock(&state(&app).last_failed).take() {
                            process(&app, pcm, busy.clone(), Clip::default(), false);
                        }
                    }
                    continue;
                }
            };
            let toggle_mode = s.mode == "toggle";
            let act = fsm.step(ev, Instant::now(), busy.load(Ordering::SeqCst), toggle_mode);
            #[cfg(debug_assertions)]
            if act != Act::None {
                eprintln!("echotype: talk key {ev:?} -> {act:?}");
            }
            match act {
                Act::None => {}
                Act::Start { id, locked } => match start_recording(&app, &s, id) {
                    Ok(l) => {
                        let _ = app.global_shortcut().register(escape());
                        if s.sounds {
                            lock(&l.stats).exclude(SOUND_WINDOWS);
                            platform::play_sound(&app, "start");
                        }
                        live = Some(l);
                        emit_state(&app, DictationState { state: "recording", locked: locked && !toggle_mode, ..Default::default() });
                    }
                    Err(e) => {
                        fsm.start_failed();
                        emit_state(&app, e);
                    }
                },
                Act::Lock => {
                    if let Some(l) = &live
                        && s.sounds
                    {
                        lock(&l.stats).exclude(SOUND_WINDOWS);
                        platform::play_sound(&app, "lock");
                    }
                    emit_state(&app, DictationState { state: "recording", locked: true, ..Default::default() });
                }
                Act::ArmTap { id, seq } => {
                    let a = app.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(hotkey::DOUBLE_TAP);
                        send(&a, Cmd::Key(Ev::TapExpired(id, seq)));
                    });
                }
                Act::Discard { hint: show } => {
                    if let Some(l) = live.take() {
                        l.rec.stop();
                    }
                    let _ = app.global_shortcut().unregister(escape());
                    let verb = if toggle_mode { "Press" } else { "Hold" };
                    emit_state(&app, if show { hint(format!("{verb} {} to dictate", key_label(&s, cfg!(target_os = "macos"))), None) } else { simple("idle") });
                }
                Act::Finish => {
                    let Some(l) = live.take() else { continue };
                    let pcm = l.rec.stop();
                    let _ = app.global_shortcut().unregister(escape());
                    if s.sounds {
                        platform::play_sound(&app, "stop");
                    }
                    let st = lock(&l.stats);
                    if st.silent() {
                        // Nothing above -70 dBFS for the whole clip: Whisper would only invent text.
                        emit_state(&app, hint("No audio. Check your mic", Some("microphone")));
                        continue;
                    }
                    let warning = l.lost.load(Ordering::SeqCst).then(|| "Microphone disconnected. Transcribed what was captured.".to_string());
                    let clip = Clip { quality: st.quality(), muted_seen: st.muted_seen(), warning };
                    drop(st);
                    process(&app, pcm, busy.clone(), clip, true);
                }
            }
        }
    });
    tx
}

/// Opens the mic for recording `id`, after checking a model or key is ready (no point recording
/// what can't be transcribed). The error is the state to show.
fn start_recording(app: &AppHandle, s: &Settings, id: u64) -> Result<Live, DictationState> {
    if let Err(e) = stt::ready(s) {
        let downloading = lock(&state(app).downloads).get(&stt::model_id(&s.stt.local_model)).copied();
        let msg = match downloading {
            Some(f) if s.stt.provider == "local" => format!("Model downloading {}%", (f * 100.0).round()),
            _ => e,
        };
        let action = if s.stt.provider == "local" { "model" } else { "settings" };
        return Err(hint(msg, Some(action)));
    }
    let stats = Arc::new(Mutex::new(LevelStats::default()));
    let lost = Arc::new(AtomicBool::new(false));
    let (a, st, lost2, started, sent) = (app.clone(), stats.clone(), lost.clone(), Instant::now(), AtomicBool::new(false));
    let rec = audio::Recorder::start(s.input_device.clone(), move |db, gone| {
        let level = lock(&st).push(db);
        let _ = a.emit("audio-level", level);
        if gone && !lost2.swap(true, Ordering::SeqCst) {
            send(&a, Cmd::Lost(id));
        }
        if started.elapsed() >= MAX_RECORDING && !sent.swap(true, Ordering::SeqCst) {
            send(&a, Cmd::Key(Ev::AutoStop(id)));
        }
    });
    match rec {
        Ok(rec) => Ok(Live { rec, stats, lost }),
        Err(_) if platform::microphone_status() == "denied" => {
            Err(DictationState { state: "error", error: Some("Microphone access is off".into()), action: Some("microphone"), ..Default::default() })
        }
        Err(e) => Err(DictationState { state: "error", error: Some(e), action: Some("microphone"), ..Default::default() }),
    }
}

/// What the recording itself said about the clip, carried to the done state.
#[derive(Default)]
struct Clip {
    quality: Option<Quality>,
    muted_seen: bool,
    warning: Option<String>,
}

/// Clears the busy flag when dropped, so even a panicking worker can't leave dictation stuck.
struct Busy(Arc<AtomicBool>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn main_window_focused(app: &AppHandle) -> bool {
    app.get_webview_window("main").is_some_and(|w| w.is_focused().unwrap_or(false))
}

/// STT + cleanup + paste + history on a worker thread, so the hotkey stays responsive.
/// Paste vs copy is decided here, when the text is ready: with Echotype's own window in front
/// (or onboarding not finished, or a retry from the window) pasting would type into Echotype,
/// so the text goes to the clipboard instead.
fn process(app: &AppHandle, pcm: Vec<f32>, busy: Arc<AtomicBool>, clip: Clip, paste: bool) {
    if pcm.len() < MIN_SAMPLES {
        emit_state(app, hint("Didn't catch that", None));
        return;
    }
    busy.store(true, Ordering::SeqCst);
    let app = app.clone();
    std::thread::spawn(move || {
        let guard = Busy(busy);
        let s = current_settings(&app);
        let chip_q = chip(clip.quality, false);
        emit_state(&app, DictationState { state: "transcribing", chip: chip_q, ..Default::default() });
        // A panic in a decoder, whisper-rs or the HTTP stack becomes an error state, not a silent loss.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cleanup::run(&s, &pcm, || emit_state(&app, DictationState { state: "cleaning", chip: chip_q, ..Default::default() }))
        }))
        .unwrap_or_else(|_| Err("Transcription crashed unexpectedly".into()));
        let failed = result.is_err();
        // Keep the audio of a failed transcription for retry_last; a success frees the old one.
        *lock(&state(&app).last_failed) = failed.then_some(pcm);
        let ev = match result {
            Err(e) => DictationState { state: "error", action: error_action(&e), error: Some(e), retry: true, ..Default::default() },
            Ok(out) if out.text.is_empty() => {
                hint(if clip.muted_seen { "No audio. Check your mic" } else { "Didn't catch that" }, clip.muted_seen.then_some("microphone"))
            }
            Ok(out) => {
                record_history(&s, &out);
                *lock(&state(&app).last_output) = Some(out.text.clone());
                let paste = paste && s.onboarded && !main_window_focused(&app);
                let warning = out.warning.clone().or(clip.warning);
                let chip = chip(clip.quality, out.no_cleanup && out.warning.is_none());
                let res = if paste { platform::paste(&app, &out.text, s.restore_clipboard) } else { platform::copy(&out.text).map_err(|e| (e, false)) };
                match res {
                    Ok(()) => DictationState { state: "done", text: Some(out.text), warning, copied: !paste, chip, ..Default::default() },
                    // Accessibility off or the keystroke blocked (secure input): the text is on the clipboard.
                    Err((e, true)) => DictationState {
                        state: "done",
                        text: Some(out.text),
                        warning: Some(e),
                        copied: true,
                        chip,
                        action: (!platform::accessibility_granted()).then_some("accessibility"),
                        ..Default::default()
                    },
                    Err((e, false)) => DictationState { state: "error", text: Some(out.text), error: Some(e), warning, ..Default::default() },
                }
            }
        };
        drop(guard);
        emit_state(&app, ev);
    });
}

/// Paste-last / copy-last: the last dictated text again, after the shortcut's own keys are up so
/// the synthetic ⌘V is not read as ⌘⌃V (which would be this shortcut again).
fn paste_last(app: &AppHandle, paste: bool) {
    let app = app.clone();
    std::thread::spawn(move || {
        let Some(text) = lock(&state(&app).last_output).clone() else {
            emit_state(&app, hint("Nothing to paste yet", None));
            return;
        };
        let s = current_settings(&app);
        let paste = paste && !main_window_focused(&app) && platform::wait_for_modifiers_up(Duration::from_millis(500));
        let res = if paste { platform::paste(&app, &text, s.restore_clipboard) } else { platform::copy(&text).map_err(|e| (e, false)) };
        emit_state(&app, match res {
            Ok(()) if paste => hint("Pasted", None),
            Ok(()) | Err((_, true)) => hint("Copied, ⌘V to paste", None),
            Err((e, false)) => DictationState { state: "error", error: Some(e), ..Default::default() },
        });
    });
}

fn record_history(s: &Settings, out: &cleanup::Output) {
    if !s.history_enabled {
        return;
    }
    let cleaned = cleanup::wants_cleanup(s, &out.raw) && !out.no_cleanup;
    let entry = HistoryEntry {
        ts: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
        raw: out.raw.clone(),
        text: out.text.clone(),
        stt_ms: out.stt_ms,
        cleanup_ms: out.cleanup_ms,
        stt_provider: if s.stt.provider == "local" { format!("local:{}", stt::model_id(&s.stt.local_model)) } else { s.stt.provider.clone() },
        cleanup_provider: cleaned.then(|| s.cleanup.provider.clone()),
        no_cleanup: out.no_cleanup,
    };
    if let Err(e) = settings::append_history(&entry) {
        eprintln!("echotype: could not write history: {e}");
    }
}

/// Installs, retargets or removes the fn / right-modifier tap for `s.talk_key`. Returns why the
/// talk key is not active when it was asked for; the shortcut keeps working either way.
fn apply_talk_key(app: &AppHandle, s: &Settings) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let code = hotkey::keycode(&s.talk_key);
        let blocked = match code {
            Some(63) if !hotkey::read_globe_setting() => Some("Set “Press 🌐 key to” to “Do Nothing” in System Settings, Keyboard, to use fn".to_string()),
            Some(_) if !hotkey::tap::allowed() => Some(format!("Using {}: this talk key needs Input Monitoring", accel_label(&s.hotkey, true))),
            _ => None,
        };
        let tx = state(app).dictation.clone();
        let res = hotkey::tap::set(code.filter(|_| blocked.is_none()), move |e| {
            let ev = match e {
                hotkey::TapEv::Down => Ev::Press,
                hotkey::TapEv::Up => Ev::Release,
                hotkey::TapEv::Other => Ev::OtherKey,
            };
            let _ = tx.send(Cmd::Key(ev));
        });
        if blocked.is_some() || res.is_err() {
            eprintln!("echotype: talk key {} inactive: {}", s.talk_key, blocked.clone().or(res.clone().err()).unwrap_or_default());
        }
        blocked.or(res.err())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, s);
        None
    }
}

// ---------- hotkey + local API wiring ----------

fn parse_hotkey(s: &str) -> Result<Shortcut, String> {
    s.parse::<Shortcut>().map_err(|e| format!("Invalid hotkey '{s}': {e}"))
}

/// Moves one global shortcut from `old` to `new`; "" means unbound. A failed registration
/// re-registers `old`, so a bad key never leaves the user with no shortcut.
fn swap_hotkey(app: &AppHandle, old: &str, new: &str) -> Result<(), String> {
    let gs = app.global_shortcut();
    let new_sc = if new.is_empty() { None } else { Some(parse_hotkey(new)?) };
    if new_sc.is_some_and(|n| gs.is_registered(n)) {
        return Ok(());
    }
    let old_sc = (!old.is_empty()).then(|| parse_hotkey(old).ok()).flatten();
    if let Some(o) = old_sc {
        let _ = gs.unregister(o);
    }
    let Some(n) = new_sc else { return Ok(()) };
    gs.register(n).map_err(|e| {
        if let Some(o) = old_sc {
            let _ = gs.register(o);
        }
        format!("Could not register hotkey '{new}': {e}")
    })
}

const SHORTCUT_NAMES: [&str; 3] = ["dictation", "paste last", "copy last"];

fn shortcuts(s: &Settings) -> [String; 3] {
    [s.hotkey.clone(), s.paste_last_hotkey.clone(), s.copy_last_hotkey.clone()]
}

fn same_shortcut(a: &str, b: &str) -> bool {
    !a.is_empty() && match (parse_hotkey(a), parse_hotkey(b)) {
        (Ok(x), Ok(y)) => x.id() == y.id(),
        _ => a.eq_ignore_ascii_case(b),
    }
}

/// Launch at login is offered only from /Applications and never from a translocated copy:
/// a login item pointing into Downloads or a random translocation path breaks on the next move.
fn autostart_location_ok(exe: &str) -> bool {
    !cfg!(target_os = "macos") || (exe.starts_with("/Applications/") && !exe.contains("/AppTranslocation/"))
}

fn can_autostart() -> bool {
    std::env::current_exe().is_ok_and(|p| autostart_location_ok(&p.to_string_lossy()))
}

fn set_autostart(app: &AppHandle, on: bool) -> R<()> {
    use tauri_plugin_autostart::ManagerExt;
    if on && !can_autostart() {
        return Err("Launch at login needs Echotype in your Applications folder".into());
    }
    let l = app.autolaunch();
    if on { l.enable() } else { l.disable() }.map_err(|e| format!("Could not change launch at login: {e}"))
}

fn apply_local_api(app: &AppHandle, s: &Settings) -> Result<(), String> {
    let st = state(app);
    let mut api = st.api.lock().unwrap_or_else(|e| e.into_inner());
    let running_port = api.as_ref().map(|(p, ..)| *p);
    if s.local_api.enabled && running_port == Some(s.local_api.port) {
        return Ok(());
    }
    api.take(); // dropping the sender stops the old server
    if s.local_api.enabled {
        let token = local_api_token()?;
        let stop = api::start(app.clone(), s.local_api.port, token.clone())?;
        *api = Some((s.local_api.port, token, stop));
    }
    Ok(())
}

fn local_api_token() -> Result<String, String> {
    if let Some(t) = settings::secret("local_api")? {
        return Ok(t);
    }
    let t = api::new_token()?;
    settings::set_secret("local_api", &t)?;
    Ok(t)
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

// ---------- commands (the UI contract) ----------

type R<T> = Result<T, String>;

#[tauri::command]
async fn get_settings(app: AppHandle) -> R<Settings> {
    Ok(current_settings(&app))
}

/// Saves `new`, keeping the three global shortcuts and launch at login in step with what is on
/// disk. Each changes only when it differs. A shortcut that cannot register (or is already one
/// of the other two) never blocks the rest: it keeps its old key and the reason comes back as a
/// warning; the same for launch at login. A failed save undoes every swap, so the registrations,
/// the file and memory never disagree. The dictation shortcut can't be unbound; the others can ("").
fn commit(old: &Settings, mut new: Settings, swap: impl Fn(&str, &str) -> R<()>, autostart: impl Fn(bool) -> R<()>, save: impl Fn(&Settings) -> R<()>) -> R<(Settings, Option<String>)> {
    let olds = shortcuts(old);
    let mut keys = shortcuts(&new);
    if keys[0].trim().is_empty() {
        keys[0] = olds[0].clone();
    }
    let (mut warnings, mut swapped) = (vec![], vec![]);
    for i in 0..3 {
        if keys[i] == olds[i] {
            continue;
        }
        let clash = (0..3).find(|&j| j != i && same_shortcut(&keys[i], &keys[j]));
        let res = match clash {
            Some(j) => Err(format!("{} is already the {} shortcut", keys[i], SHORTCUT_NAMES[j])),
            None => swap(&olds[i], &keys[i]),
        };
        match res {
            Ok(()) => swapped.push(i),
            Err(e) => {
                warnings.push(format!("{e}. Your other settings were saved."));
                keys[i] = olds[i].clone();
            }
        }
    }
    [new.hotkey, new.paste_last_hotkey, new.copy_last_hotkey] = keys.clone();
    let autostart_changed = new.autostart != old.autostart
        && match autostart(new.autostart) {
            Ok(()) => true,
            Err(e) => {
                warnings.push(format!("{e}."));
                new.autostart = old.autostart;
                false
            }
        };
    if let Err(e) = save(&new) {
        for &i in swapped.iter().rev() {
            let _ = swap(&keys[i], &olds[i]);
        }
        if autostart_changed {
            let _ = autostart(old.autostart);
        }
        return Err(e);
    }
    Ok((new, (!warnings.is_empty()).then(|| warnings.join(" "))))
}

/// The one way settings change, for the settings window and for Rust-side writers alike: one
/// at a time, shortcuts/login item/file/memory/local API/talk key updated together, then every
/// window gets `settings-changed` with the full result.
fn commit_settings(app: &AppHandle, change: impl FnOnce(&Settings) -> Settings) -> R<Option<String>> {
    static SAVE: Mutex<()> = Mutex::new(());
    let _g = SAVE.lock().unwrap_or_else(|e| e.into_inner());
    let old = current_settings(app);
    let new = change(&old);
    if new.local_api.port < settings::MIN_PORT {
        return Err(format!("The local API port must be between {} and 65535", settings::MIN_PORT));
    }
    let (new, warning) = commit(&old, new, |o, n| swap_hotkey(app, o, n), |on| set_autostart(app, on), Settings::save)?;
    *state(app).settings.lock().unwrap_or_else(|e| e.into_inner()) = new.clone();
    if (new.stt.local_model != old.stt.local_model || new.stt.provider != old.stt.provider) && new.stt.provider == "local" {
        let s = new.clone();
        std::thread::spawn(move || stt::warm_up(&s));
    }
    if !new.history_enabled {
        *lock(&state(app).last_output) = None;
    }
    if new.talk_key != old.talk_key {
        apply_talk_key(app, &new);
    }
    let _ = app.emit("settings-changed", &new);
    apply_local_api(app, &new)?;
    Ok(warning)
}

/// `base` is the copy the window last received; with it, a stale window only applies its own edits.
#[tauri::command]
async fn save_settings(app: AppHandle, settings: Settings, base: Option<Settings>) -> R<()> {
    let warning = commit_settings(&app, |cur| match &base {
        Some(b) => settings::merge(cur, b, &settings),
        None => settings,
    })?;
    warning.map_or(Ok(()), Err)
}

#[tauri::command]
async fn get_presets() -> R<Vec<settings::Preset>> {
    Ok(settings::presets())
}

#[tauri::command]
async fn set_secret(name: String, value: String) -> R<()> {
    settings::set_secret(&name, &value)
}

#[tauri::command]
async fn has_secret(name: String) -> R<bool> {
    Ok(settings::get_secret(&name).is_some())
}

#[tauri::command]
async fn delete_secret(name: String) -> R<()> {
    settings::delete_secret(&name)
}

#[tauri::command]
async fn list_models() -> R<Vec<stt::ModelStatus>> {
    Ok(stt::list_models(stt::ram_mb().1))
}

#[tauri::command]
async fn download_model(app: AppHandle, id: String) -> R<()> {
    let job = tauri::async_runtime::spawn_blocking(move || {
        let emit = |downloaded: u64, total: Option<u64>, done: bool, error: Option<&str>| {
            {
                let st = state(&app);
                let mut dl = lock(&st.downloads);
                if done {
                    dl.remove(&id);
                } else {
                    dl.insert(id.clone(), total.map_or(0.0, |t| downloaded as f64 / t.max(1) as f64));
                }
            }
            let _ = app.emit("model-download", serde_json::json!({ "id": id, "downloaded": downloaded, "total": total, "done": done, "error": error }));
        };
        let res = stt::download(&id, |d, t| emit(d, t, false, None));
        match &res {
            Ok(()) => emit(0, None, true, None),
            Err(e) => emit(0, None, true, Some(e)),
        }
        res
    });
    job.await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn delete_model(id: String) -> R<()> {
    stt::delete(&id)
}

#[tauri::command]
async fn list_input_devices() -> R<Vec<audio::InputDevice>> {
    Ok(audio::input_devices())
}

#[tauri::command]
async fn get_history(limit: Option<usize>) -> R<Vec<HistoryEntry>> {
    Ok(settings::get_history(limit.unwrap_or(100)))
}

#[tauri::command]
async fn clear_history(app: AppHandle) -> R<()> {
    *lock(&state(&app).last_output) = None;
    settings::clear_history()
}

#[tauri::command]
async fn transcribe_file(app: AppHandle, path: String) -> R<cleanup::Output> {
    let s = current_settings(&app);
    tauri::async_runtime::spawn_blocking(move || {
        let out = transcribe_path(&s, &path)?;
        record_history(&s, &out);
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn transcribe_path(s: &Settings, path: &str) -> R<cleanup::Output> {
    let bytes = std::fs::read(path).map_err(|e| format!("Could not read {path}: {e}"))?;
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str());
    let pcm = audio::decode(bytes, ext)?;
    cleanup::run(s, &pcm, || {})
}

#[derive(Serialize)]
struct SystemInfo {
    total_ram_gb: f64,
    available_ram_gb: f64,
    os: &'static str,
    arch: &'static str,
    /// Launch at login can be offered (running from /Applications, not translocated).
    can_autostart: bool,
}

#[tauri::command]
async fn get_system_info() -> R<SystemInfo> {
    let (total, avail) = stt::ram_mb();
    let gb = |mb: u64| (mb as f64 / 1024.0 * 10.0).round() / 10.0;
    Ok(SystemInfo { total_ram_gb: gb(total), available_ram_gb: gb(avail), os: std::env::consts::OS, arch: std::env::consts::ARCH, can_autostart: can_autostart() })
}

#[tauri::command]
async fn local_api_status(app: AppHandle) -> R<serde_json::Value> {
    let s = current_settings(&app);
    if s.local_api.enabled {
        apply_local_api(&app, &s)?; // no-op when running; otherwise starts it or says why it can't
    }
    // The running server's own port and token, so the UI never shows a token the server rejects.
    let running = state(&app).api.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|(p, t, _)| (*p, t.clone()));
    let (port, token) = running.map_or((s.local_api.port, None), |(p, t)| (p, Some(t)));
    Ok(serde_json::json!({ "enabled": s.local_api.enabled, "port": port, "token": token }))
}

#[tauri::command]
async fn toggle_dictation(app: AppHandle) -> R<()> {
    state(&app).dictation.send(Cmd::Key(Ev::Toggle)).map_err(|e| e.to_string())
}

/// Talk key and shortcut health for Settings and onboarding, re-checked each time it is asked
/// (on window focus and when Settings opens), so a grant or a Globe-key change applies at once.
#[tauri::command]
async fn shortcut_status(app: AppHandle) -> R<serde_json::Value> {
    let s = current_settings(&app);
    let talk_key_error = apply_talk_key(&app, &s);
    #[cfg(target_os = "macos")]
    let input_monitoring = hotkey::tap::allowed();
    #[cfg(not(target_os = "macos"))]
    let input_monitoring = true;
    Ok(serde_json::json!({
        "talk_key_active": talk_tap_active(),
        "talk_key_error": talk_key_error,
        "input_monitoring": input_monitoring,
        "globe_does_nothing": cfg!(target_os = "macos") && hotkey::read_globe_setting(),
        "needs_relaunch": input_monitoring && hotkey::keycode(&s.talk_key).is_some() && !talk_tap_active() && talk_key_error.is_some(),
        "shortcut_errors": lock(&state(&app).shortcut_errors).clone(),
        "key_label": key_label(&s, cfg!(target_os = "macos")),
    }))
}

/// Restarts Echotype: a fresh Input Monitoring grant only takes effect in a new process.
#[tauri::command]
async fn relaunch(app: AppHandle) -> R<()> {
    app.restart()
}

/// Runs the last failed recording through STT and cleanup again. It is started from the UI,
/// so the result is copied, not pasted.
#[tauri::command]
async fn retry_last(app: AppHandle) -> R<()> {
    let st = state(&app);
    if st.last_failed.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
        return Err("There is no failed recording to retry".into());
    }
    st.dictation.send(Cmd::Retry).map_err(|e| e.to_string())
}

#[tauri::command]
async fn check_permissions() -> R<serde_json::Value> {
    Ok(serde_json::json!({ "microphone": platform::microphone_status(), "accessibility": platform::accessibility_granted() }))
}

#[tauri::command]
async fn open_permission_settings(app: AppHandle, kind: String) -> R<()> {
    if !matches!(kind.as_str(), "microphone" | "accessibility" | "input_monitoring" | "keyboard") {
        return Err(format!("Unknown permission '{kind}'"));
    }
    let k = kind.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || platform::request(&k)).await;
    if let Some(url) = platform::settings_url(&kind) {
        app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------- Echotype Cloud ----------

/// The cloud calls are blocking HTTP and keychain work, so they run off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> R<T> + Send + 'static) -> R<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn cloud_start_login(email: String) -> R<()> {
    blocking(move || cloud::start_login(&email)).await
}

#[tauri::command]
async fn cloud_verify(email: String, code: String) -> R<serde_json::Value> {
    blocking(move || cloud::verify(&email, &code)).await
}

#[tauri::command]
async fn cloud_account() -> R<serde_json::Value> {
    blocking(cloud::account).await
}

#[tauri::command]
async fn cloud_checkout(app: AppHandle, plan: String) -> R<()> {
    let url = blocking(move || cloud::billing_url(Some(&plan))).await?;
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
async fn cloud_portal(app: AppHandle) -> R<()> {
    let url = blocking(|| cloud::billing_url(None)).await?;
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
async fn cloud_logout() -> R<()> {
    blocking(cloud::logout).await
}

// ---------- app ----------

fn create_overlay(app: &tauri::App) -> tauri::Result<()> {
    let (w, h) = (260.0, 32.0);
    let overlay = WebviewWindowBuilder::new(app, "overlay", WebviewUrl::App("overlay.html".into()))
        .title("Echotype")
        .inner_size(w, h)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .focusable(false)
        .visible(false)
        .build()?;
    if let Some(m) = overlay.primary_monitor()? {
        let scale = m.scale_factor();
        let size = m.size().to_logical::<f64>(scale);
        let pos = m.position().to_logical::<f64>(scale);
        overlay.set_position(tauri::LogicalPosition::new(pos.x + (size.width - w) / 2.0, pos.y + size.height - h - 84.0))?;
    }
    overlay.set_ignore_cursor_events(true)?;
    Ok(())
}

fn create_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Echotype", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", "Start/Stop dictation", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &toggle, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main").tooltip("Echotype").menu(&menu).on_menu_event(|app, e| match e.id.as_ref() {
        "open" => show_main(app),
        "toggle" => send(app, Cmd::Key(Ev::Toggle)),
        "quit" => app.exit(0),
        _ => {}
    });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    stt::quiet_whisper_logs();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show_main(app)))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, sc, ev| {
                    let s = current_settings(app);
                    let is = |k: &str| !k.is_empty() && parse_hotkey(k).is_ok_and(|h| h.id() == sc.id());
                    let pressed = ev.state == ShortcutState::Pressed;
                    if sc.id() == escape().id() {
                        if pressed {
                            send(app, Cmd::Key(Ev::Cancel));
                        }
                    } else if is(&s.hotkey) {
                        send(app, Cmd::Key(if pressed { Ev::Press } else { Ev::Release }));
                    } else if is(&s.paste_last_hotkey) && !pressed {
                        paste_last(app, true);
                    } else if is(&s.copy_last_hotkey) && !pressed {
                        paste_last(app, false);
                    }
                })
                .build(),
        )
        .setup(|app| {
            let s = Settings::load();
            app.manage(AppState {
                settings: Mutex::new(s.clone()),
                dictation: spawn_dictation(app.handle().clone()),
                api: Mutex::new(None),
                overlay_gen: AtomicU64::new(0),
                last_failed: Mutex::new(None),
                last_output: Mutex::new(None),
                downloads: Mutex::new(HashMap::new()),
                shortcut_errors: Mutex::new(vec![]),
            });
            create_overlay(app)?;
            create_tray(app)?;
            for key in shortcuts(&s).iter().filter(|k| !k.is_empty()) {
                if let Err(e) = swap_hotkey(app.handle(), "", key) {
                    eprintln!("echotype: {e}");
                    lock(&state(app.handle()).shortcut_errors).push(e);
                }
            }
            apply_talk_key(app.handle(), &s);
            // Off the main thread: the keychain read can wait on an access prompt (an unsigned
            // rebuild is a new identity to macOS), which must not freeze startup.
            let (handle, api_settings) = (app.handle().clone(), s.clone());
            std::thread::spawn(move || {
                if let Err(e) = apply_local_api(&handle, &api_settings) {
                    eprintln!("echotype: {e}");
                }
            });
            if !s.onboarded {
                show_main(app.handle());
            }
            std::thread::spawn(move || stt::warm_up(&s));
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == "main"
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            get_presets,
            set_secret,
            has_secret,
            delete_secret,
            list_models,
            download_model,
            delete_model,
            list_input_devices,
            get_history,
            clear_history,
            transcribe_file,
            get_system_info,
            local_api_status,
            toggle_dictation,
            shortcut_status,
            relaunch,
            retry_last,
            check_permissions,
            open_permission_settings,
            cloud_start_login,
            cloud_verify,
            cloud_account,
            cloud_checkout,
            cloud_portal,
            cloud_logout,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Echotype");
    app.run(|app, event| {
        #[cfg(target_os = "macos")]
        if let RunEvent::Reopen { .. } = event {
            show_main(app);
        }
        if let RunEvent::Exit = event {
            stt::unload();
        }
        let _ = (app, event);
    });
}

/// `echotype --transcribe <file> [--model <ggml path>] [--translate]`: the full pipeline
/// minus paste, headless, with the saved settings. Returns the process exit code.
pub fn cli(args: &[String]) -> i32 {
    let mut s = Settings::load();
    let mut file = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--transcribe" => file = it.next().cloned(),
            "--model" => match it.next() {
                Some(m) => {
                    s.stt.provider = "local".into();
                    s.stt.local_model = std::fs::canonicalize(m).map_or(m.clone(), |p| p.to_string_lossy().into_owned());
                }
                None => file = None,
            },
            "--translate" => s.translate_to_english = true,
            other => {
                eprintln!("echotype: unknown argument '{other}'");
                file = None;
                break;
            }
        }
    }
    let Some(file) = file else {
        eprintln!("usage: echotype --transcribe <audio file> [--model <path to ggml .bin>] [--translate]");
        return 2;
    };
    stt::quiet_whisper_logs();
    let t = std::time::Instant::now();
    let result = transcribe_path(&s, &file);
    stt::unload();
    let (out, err, code) = cli_report(result, t.elapsed().as_millis());
    if !out.is_empty() {
        println!("{out}");
    }
    eprint!("{err}");
    code
}

/// (stdout, stderr, exit code) for `--transcribe`. Nothing said is a success with empty stdout.
fn cli_report(result: R<cleanup::Output>, total_ms: u128) -> (String, String, i32) {
    match result {
        Ok(out) => {
            let mut err = String::new();
            if out.text.is_empty() {
                err.push_str("echotype: no speech found\n");
            }
            if let Some(w) = &out.warning {
                err.push_str(&format!("warning: {w}\n"));
            }
            err.push_str(&format!("stt {} ms, cleanup {} ms, total {total_ms} ms\n", out.stt_ms, out.cleanup_ms));
            (out.text, err, 0)
        }
        Err(e) => (String::new(), format!("error: {e}\n"), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::{Settings, accel_label, chip, cli_report, commit, parse_hotkey};
    use crate::audio::Quality;
    use std::cell::RefCell;

    /// Under the app manifest a command missing from a capability fails only at runtime, in the
    /// window, so check generate_handler!, build.rs COMMANDS and both capability files agree.
    #[test]
    fn capabilities_list_every_command_and_the_overlay_none() {
        let lib = include_str!("lib.rs");
        let block = lib.split("tauri::generate_handler![").nth(1).unwrap().split(']').next().unwrap();
        let handler: Vec<&str> = block.split(',').map(str::trim).filter(|c| !c.is_empty()).collect();
        let build = include_str!("../build.rs");
        let listed = build.split("const COMMANDS: &[&str] = &[").nth(1).unwrap().split("];").next().unwrap();
        let manifest: Vec<&str> = listed.split(',').map(|c| c.trim().trim_matches('"')).filter(|c| !c.is_empty()).collect();
        assert_eq!(handler, manifest, "build.rs COMMANDS must match generate_handler!");
        let perms = |json: &str| -> Vec<String> {
            let v: serde_json::Value = serde_json::from_str(json).unwrap();
            v["permissions"].as_array().unwrap().iter().filter_map(|p| p.as_str().map(String::from)).collect()
        };
        let main = perms(include_str!("../capabilities/default.json"));
        for c in &handler {
            assert!(main.contains(&format!("allow-{}", c.replace('_', "-"))), "capabilities/default.json misses {c}");
        }
        let overlay = perms(include_str!("../capabilities/overlay.json"));
        assert!(overlay.iter().all(|p| p.starts_with("core:event:")), "the overlay may only listen to events: {overlay:?}");
    }

    #[test]
    fn one_chip_wins_too_quiet_then_quiet_voice_then_no_cleanup() {
        // Copied (Accessibility off) + no cleanup + too quiet: the primary line says Copied, the chip says too quiet.
        assert_eq!(chip(Some(Quality::TooQuiet), true), Some("too_quiet"));
        assert_eq!(chip(Some(Quality::Quiet), true), Some("quiet"));
        assert_eq!(chip(None, true), Some("no_cleanup"));
        assert_eq!(chip(None, false), None);
    }

    #[test]
    fn shortcut_labels_read_like_keycaps() {
        assert_eq!(accel_label("Alt+Space", true), "⌥ Space");
        assert_eq!(accel_label("CommandOrControl+Control+V", true), "⌘ ⌃ V");
        assert_eq!(accel_label("Control+Alt+KeyK", false), "Ctrl+Alt+K");
    }

    #[test]
    fn cli_prints_nothing_and_exits_zero_when_no_speech_was_found() {
        let out = |text: &str| crate::cleanup::Output { raw: text.into(), text: text.into(), stt_ms: 5, cleanup_ms: 0, warning: None, no_cleanup: true };
        let (stdout, stderr, code) = cli_report(Ok(out("")), 9);
        assert_eq!((stdout.as_str(), code), ("", 0));
        assert!(stderr.starts_with("echotype: no speech found\n"), "{stderr}");
        let (stdout, stderr, code) = cli_report(Ok(out("Hello.")), 9);
        assert_eq!((stdout.as_str(), code), ("Hello.", 0));
        assert!(!stderr.contains("no speech"));
        assert_eq!(cli_report(Err("boom".into()), 1).2, 1);
    }

    #[test]
    fn hotkeys_the_ui_sends_parse_and_junk_is_a_readable_error() {
        for ok in ["Alt+Space", "CommandOrControl+Shift+D", "Control+Alt+KeyK", "Super+F5", "Shift+Digit1"] {
            assert!(parse_hotkey(ok).is_ok(), "{ok}");
        }
        assert_eq!(parse_hotkey("Alt+Space").unwrap(), parse_hotkey("alt+space").unwrap());
        let e = parse_hotkey("Alt+Nope").unwrap_err();
        assert!(e.starts_with("Invalid hotkey 'Alt+Nope'"), "{e}");
        assert!(parse_hotkey("").is_err());
    }

    #[test]
    fn saving_with_an_unchanged_hotkey_never_touches_the_registration() {
        let old = Settings::default();
        let mut new = old.clone();
        new.onboarded = true;
        let saved = RefCell::new(None);
        let (kept, warning) = commit(&old, new, |_, _| panic!("swapped an unchanged hotkey"), |_| panic!("touched launch at login"), |s| {
            *saved.borrow_mut() = Some(s.clone());
            Ok(())
        }).unwrap();
        assert!(kept.onboarded && warning.is_none());
        assert_eq!(saved.into_inner(), Some(kept));
    }

    #[test]
    fn a_hotkey_that_cannot_register_still_saves_everything_else() {
        let old = Settings::default();
        let mut new = old.clone();
        new.hotkey = "Control+Alt+KeyK".into();
        new.history_enabled = false;
        let saved = RefCell::new(None);
        let (kept, warning) = commit(&old, new, |_, n| Err(format!("Could not register hotkey '{n}'")), |_| Ok(()), |s| {
            *saved.borrow_mut() = Some(s.clone());
            Ok(())
        }).unwrap();
        assert_eq!(kept.hotkey, "Alt+Space");
        assert!(!kept.history_enabled);
        assert!(warning.unwrap().contains("other settings were saved"));
        assert_eq!(saved.into_inner(), Some(kept));
    }

    #[test]
    fn a_failed_save_swaps_the_hotkey_back() {
        let old = Settings::default();
        let mut new = old.clone();
        new.hotkey = "Control+Alt+KeyK".into();
        let swaps = RefCell::new(vec![]);
        let err = commit(&old, new, |o, n| {
            swaps.borrow_mut().push((o.to_string(), n.to_string()));
            Ok(())
        }, |_| Ok(()), |_| Err("disk full".into())).unwrap_err();
        assert_eq!(err, "disk full");
        let k = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(swaps.into_inner(), vec![k("Alt+Space", "Control+Alt+KeyK"), k("Control+Alt+KeyK", "Alt+Space")]);
    }

    #[test]
    fn three_shortcuts_swap_reject_duplicates_and_treat_empty_as_unbound() {
        let old = Settings::default();
        let mut new = old.clone();
        new.paste_last_hotkey = "CommandOrControl+Control+V".into();
        new.copy_last_hotkey = "Alt+Space".into(); // already dictation
        let swaps = RefCell::new(vec![]);
        let (kept, warning) = commit(&old, new, |o, n| {
            swaps.borrow_mut().push((o.to_string(), n.to_string()));
            Ok(())
        }, |_| Ok(()), |_| Ok(())).unwrap();
        assert_eq!(swaps.into_inner(), vec![(String::new(), "CommandOrControl+Control+V".to_string())]);
        assert_eq!((kept.paste_last_hotkey.as_str(), kept.copy_last_hotkey.as_str()), ("CommandOrControl+Control+V", ""));
        assert!(warning.unwrap().contains("already the dictation shortcut"));

        // Back to None unregisters; the dictation shortcut can't be cleared.
        let mut none = kept.clone();
        none.paste_last_hotkey = String::new();
        none.hotkey = String::new();
        let swaps = RefCell::new(vec![]);
        let (kept2, _) = commit(&kept, none, |o, n| {
            swaps.borrow_mut().push((o.to_string(), n.to_string()));
            Ok(())
        }, |_| Ok(()), |_| Ok(())).unwrap();
        assert_eq!(swaps.into_inner(), vec![("CommandOrControl+Control+V".to_string(), String::new())]);
        assert_eq!((kept2.hotkey.as_str(), kept2.paste_last_hotkey.as_str()), ("Alt+Space", ""));
    }

    #[test]
    fn launch_at_login_failures_are_warnings_and_a_failed_save_undoes_everything() {
        let old = Settings::default();
        let mut new = old.clone();
        new.autostart = true;
        new.history_enabled = false;
        let (kept, warning) = commit(&old, new.clone(), |_, _| Ok(()), |_| Err("Launch at login needs Echotype in your Applications folder".into()), |_| Ok(())).unwrap();
        assert!(!kept.autostart && !kept.history_enabled);
        assert!(warning.unwrap().contains("Applications folder"));

        new.copy_last_hotkey = "CommandOrControl+Control+C".into();
        let calls = RefCell::new(vec![]);
        let err = commit(&old, new, |o, n| {
            calls.borrow_mut().push(format!("swap {o}>{n}"));
            Ok(())
        }, |on| {
            calls.borrow_mut().push(format!("autostart {on}"));
            Ok(())
        }, |_| Err("disk full".into())).unwrap_err();
        assert_eq!(err, "disk full");
        assert_eq!(calls.into_inner(), ["swap >CommandOrControl+Control+C", "autostart true", "swap CommandOrControl+Control+C>", "autostart false"]);
    }

    #[test]
    fn launch_at_login_only_from_applications() {
        let ok = super::autostart_location_ok;
        assert!(ok("/Applications/Echotype.app/Contents/MacOS/echotype"));
        if cfg!(target_os = "macos") {
            assert!(!ok("/Users/me/Downloads/Echotype.app/Contents/MacOS/echotype"));
            assert!(!ok("/private/var/folders/x/AppTranslocation/ABC/d/Echotype.app/Contents/MacOS/echotype"));
            assert!(!ok("/Applications/../Users/x/AppTranslocation/Echotype.app"));
        }
    }
}
