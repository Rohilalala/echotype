mod api;
mod audio;
mod calls;
mod capture;
mod cleanup;
mod cloud;
mod hotkey;
mod learn;
mod llm;
mod meeting;
mod music;
mod notices;
mod pill;
mod platform;
mod settings;
mod stats;
mod speakers;
mod stt;
mod telemetry;
mod tray;
mod wispr;

use audio::{LevelStats, Quality};
use hotkey::{Act, Ev};
use serde::Serialize;
use settings::{HistoryEntry, Settings};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Listener, Manager, RunEvent, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;

/// Below this much rise above the room (dB) a clip is noise, not speech (normal speech is 30-45).
// ponytail: one threshold from the meter's dB stats; tune with recorded noise clips if it bites.
const NOISE_SNR_DB: f32 = 15.0;
const MIN_SAMPLES: usize = audio::RATE as usize * 3 / 10; // under 0.3 s there is nothing to transcribe
/// A recording this long stops itself and is transcribed as usual (toggle mode left on, a stuck key).
// ponytail: the whole clip stays in memory, ~460 MB at 30 min of 48 kHz f32 plus the 16 kHz copy;
// stream-resample into the 16 kHz buffer if hands-free recordings get that long in practice.
const MAX_RECORDING: Duration = Duration::from_secs(30 * 60);
/// Windows (50 ms) of level stats skipped after Echotype's own start/lock sound (<= 120 ms plus output latency).
const SOUND_WINDOWS: u32 = 4;
/// A shortcut-started recording shows itself (pill, start sound) only after this long without
/// another key; the mic opens at once, so no words are lost. A modifier talk key opens the mic
/// only after it has been held alone this long, so ⌃C or ⌘-click never touch it.
const SHOW_AFTER: Duration = Duration::from_millis(150);
/// A pill click shows its recording this much later (the mic opens at once), so the second
/// click of a double-click (put the pill back) can take it back before anything shows or plays.
const CLICK_SHOW_AFTER: Duration = Duration::from_millis(250);
/// Live words while recording (Settings → Pill → Show words as you speak, local model only): the
/// first partial once this much is recorded, then one every PARTIAL_EVERY, each on the last
/// PARTIAL_TAIL of audio. Partials are never cleaned up or pasted; the final run is unchanged.
const PARTIAL_FROM: f32 = 1.5;
const PARTIAL_EVERY: Duration = Duration::from_secs(1);
// ponytail: a fixed 6 s tail (~100 ms a run on an M5 Pro with turbo Q5), plenty for the ~2 s of
// speech the pill shows; transcribe more and stitch if the pill ever shows more than one line.
pub(crate) const PARTIAL_TAIL: Duration = Duration::from_secs(6);
/// The partial's last characters the pill shows (pill.rs sizes the capsule from what is sent).
const PARTIAL_CHARS: usize = 24;

struct AppState {
    settings: Mutex<Settings>,
    dictation: mpsc::Sender<Cmd>,
    /// Running local API: port, the token it accepts, and the sender whose drop shuts it down.
    api: Mutex<Option<(u16, String, tokio::sync::oneshot::Sender<()>)>>,
    /// What the pill shows; `emit_state` feeds it and the pill loop ticks it.
    pill: Mutex<pill::Pill>,
    /// Wakes the pill loop after a state, settings or focus change.
    pill_wake: mpsc::Sender<()>,
    /// Echotype's settings window is the key window (D14: the pill steps aside).
    main_key: AtomicBool,
    /// The settings window shows Home (main.js emits `main-page` on every page change).
    main_home: AtomicBool,
    /// The first-run film has the settings window (film_window); the pill waits for its dots.
    film: AtomicBool,
    /// The film's card in screen points: clicks reach the window only here, the rest falls through.
    film_hit: Mutex<Option<pill::Rect>>,
    clicks: Mutex<pill::Debounce>,
    /// The press on the pill that may be a drag (pill.rs moves the window while it is held).
    drag: Mutex<Option<pill::Drag>>,
    /// The tray's hide/show pill item (macOS), relabelled by the pill loop.
    tray_pill: Mutex<Option<MenuItem<tauri::Wry>>>,
    /// The tray's own menu, put back when menu-bar mode ends.
    tray_menu: Mutex<Option<Menu<tauri::Wry>>>,
    /// The last dictation's audio, so `retry_last` can run it again when it failed and Re-run last
    /// can run it with another language, model or no cleanup. Memory only, never written to disk,
    /// so it is gone on quit.
    // ponytail: only the last one, in memory; a failed-recordings folder if users need more than one retry.
    last_clip: Mutex<Option<Kept>>,
    /// The cleanup the guard rejected for the last dictation, for "Use cleaned version". Memory
    /// only, cleared with last_output.
    last_cleaned: Mutex<Option<String>>,
    /// The last dictated text, for paste-last and copy-last. Memory only, so it works with history
    /// off; cleared with history and when history is switched off.
    last_output: Mutex<Option<String>>,
    /// The app the last dictation went to, with "Match the app you're typing in" on (Cleanup page: add it).
    last_app: Mutex<Option<platform::FrontApp>>,
    /// Model downloads in flight: id → fraction done, so a press can say "Model downloading 42%".
    downloads: Mutex<HashMap<String, f64>>,
    /// The mic is open, so paste-last must not replace the recording pill with its notice.
    recording: AtomicBool,
}

enum Cmd {
    Key(Ev),
    /// The Command Mode shortcut went down or up: the same talk-key rules, but the recording is an instruction.
    Command(Ev),
    /// A click on the pill: Ev::Toggle, with the recording shown after CLICK_SHOW_AFTER.
    Click,
    Retry,
    /// Re-run with one change: the last dictation (None) or a History entry's kept audio.
    Rerun(Rerun, Option<u64>),
    /// Recording `n`'s microphone went away: transcribe what it captured.
    Lost(u64),
    /// Recording `n` survived SHOW_AFTER: show it and play the start (or, once locked, the lock) sound.
    Show(u64, bool),
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
    /// "done" with nowhere to paste: the pill opens into a card with the text and Copy.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    card: bool,
    /// The paste-last shortcut as the card names it ("⌘ ⌃ V"); None when it is unbound.
    #[serde(skip_serializing_if = "Option::is_none")]
    paste_key: Option<String>,
    /// A card offering meeting notes for a detected call: its button ("Start notes" | "Stop notes").
    #[serde(skip_serializing_if = "Option::is_none")]
    offer: Option<&'static str>,
    /// The guard pasted the words as spoken: a card with the rejected cleanup and "Copy cleaned".
    #[serde(skip_serializing_if = "Option::is_none")]
    cleaned: Option<String>,
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

/// Every window hears every state, except that the pill keeps a more important notice still on
/// screen (D2): then only the settings window gets the new one. The pill loop does the rest.
fn emit_state(app: &AppHandle, s: DictationState) {
    let st = state(app);
    let to_pill = lock(&st.pill).on_state(&s, Instant::now());
    let _ = if to_pill { app.emit("dictation-state", &s) } else { app.emit_to("main", "dictation-state", &s) };
    let _ = st.pill_wake.send(());
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

/// A manual model pick (advanced settings) that would push the Mac into swap right now: the
/// notice offers "Switch to automatic" rather than overriding the pick silently.
fn memory_action(s: &Settings, text: &str) -> Option<&'static str> {
    (s.advanced && text.contains("Not enough free memory")).then_some("automatic")
}

/// "⌥ Space", "fn", "Right ⌘", "Ctrl+Alt+K": how copy names the key the user dictates with.
fn key_label(s: &Settings, mac: bool) -> String {
    // The tap runs for fn only after the Globe-key check passed (apply_talk_key).
    match hotkey::label(&s.talk_key) {
        Some(l) if mac && talk_tap_active() => l.into(),
        _ => accel_label(&s.hotkey, mac),
    }
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
    id: u64,
    /// The pill and start sound have been shown for it.
    shown: bool,
    /// Hands-free from the start (a click in hold mode), so the pill shows the lock when it shows.
    locked: bool,
    rec: audio::Recorder,
    stats: Arc<Mutex<LevelStats>>,
    lost: Arc<AtomicBool>,
    /// The app in front as recording started, when the style follows it (the user may switch away while it transcribes).
    front: Option<platform::FrontApp>,
    /// Started by the Command Mode shortcut: the words are an instruction for the selected text.
    command: bool,
    /// Set as the recording ends: the partials loop stops and a partial in flight aborts.
    partials: Arc<AtomicBool>,
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
            let command = matches!(cmd, Cmd::Command(_));
            let (ev, click) = match cmd {
                Cmd::Command(ev) => (ev, false),
                Cmd::Key(ev) => {
                    // Onboarding presses its keycaps down with the real keys.
                    if matches!(ev, Ev::Press | Ev::ModPress | Ev::Release) {
                        let _ = app.emit_to("main", "hotkey-state", ev != Ev::Release);
                    }
                    (ev, false)
                }
                Cmd::Click => (Ev::Toggle, true),
                Cmd::Lost(id) => (Ev::AutoStop(id), false),
                Cmd::Show(id, locked) => {
                    if let Some(l) = live.as_mut().filter(|l| l.id == id && (locked || !l.shown)) {
                        if !l.shown && s.pause_music {
                            music::set(true);
                        }
                        l.shown = true;
                        if s.sounds {
                            lock(&l.stats).exclude(SOUND_WINDOWS);
                            platform::play_sound(&app, if locked { "lock" } else { "start" });
                        }
                        emit_state(&app, DictationState { state: "recording", locked: locked || l.locked, ..Default::default() });
                    }
                    continue;
                }
                Cmd::Retry => {
                    if live.is_none() && !busy.load(Ordering::SeqCst) {
                        if let Some(k) = lock(&state(&app).last_clip).take_if(|k| k.failed) {
                            process(&app, k.pcm, busy.clone(), Clip { entry: k.entry, ..Default::default() }, false, None);
                        }
                    }
                    continue;
                }
                Cmd::Rerun(r, entry) => {
                    if live.is_none() && !busy.load(Ordering::SeqCst) {
                        let kept = match entry {
                            Some(ts) => std::fs::read(settings::clip_path(ts)).ok().and_then(|b| audio::decode(b, Some("wav")).ok()).map(|pcm| Kept { pcm, failed: false, entry }),
                            None => lock(&state(&app).last_clip).take(),
                        };
                        if let Some(k) = kept {
                            process(&app, k.pcm, busy.clone(), Clip { entry: k.entry, ..Default::default() }, false, Some(r));
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
                // Command Mode with a provider that can't follow instructions says so before the mic opens.
                Act::Start { id, locked } => match command_ready(&s, command).and_then(|()| start_recording(&app, &s, id)) {
                    Ok(mut l) => {
                        l.command = command;
                        llm::prewarm(&s); // warm while the user talks, not after
                        // Recording before anything shows, so the pill loop takes Esc for it (pill.rs owns Esc).
                        state(&app).recording.store(true, Ordering::SeqCst);
                        let _ = state(&app).pill_wake.send(());
                        l.locked = locked && !toggle_mode;
                        if s.cleanup.match_app {
                            l.front = platform::frontmost_app(&app);
                        }
                        if click {
                            show_later(&app, id, false, CLICK_SHOW_AFTER);
                        } else if ev != Ev::Press {
                            // Toggle, or a talk key already held alone (HoldReady) or double-tapped (Release).
                            l.shown = true;
                            if s.pause_music {
                                music::set(true);
                            }
                            if s.sounds {
                                lock(&l.stats).exclude(SOUND_WINDOWS);
                                platform::play_sound(&app, if ev == Ev::Release && !toggle_mode { "lock" } else { "start" });
                            }
                            emit_state(&app, DictationState { state: "recording", locked: locked && !toggle_mode, ..Default::default() });
                        } else {
                            show_later(&app, id, false, SHOW_AFTER);
                        }
                        live = Some(l);
                    }
                    Err(e) => {
                        fsm.start_failed();
                        emit_state(&app, e);
                    }
                },
                // Deferred like Show: a lock press that turns out to be a chord (tap, then Right ⌘+V) stays silent.
                Act::Lock => {
                    if let Some(l) = &live {
                        show_later(&app, l.id, true, SHOW_AFTER);
                    }
                }
                Act::ArmTap { id, seq } => {
                    let a = app.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(hotkey::DOUBLE_TAP);
                        send(&a, Cmd::Key(Ev::TapExpired(id, seq)));
                    });
                }
                Act::ArmHold { seq } => {
                    let a = app.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(SHOW_AFTER);
                        send(&a, Cmd::Key(Ev::HoldReady(seq)));
                    });
                }
                Act::Discard { hint: show } => {
                    let edit = live.as_ref().is_some_and(|l| l.command);
                    if let Some(l) = live.take() {
                        l.partials.store(true, Ordering::SeqCst);
                        l.rec.stop();
                        music::set(false);
                    }
                    state(&app).recording.store(false, Ordering::SeqCst);
                    let _ = state(&app).pill_wake.send(());
                    let verb = if toggle_mode { "Press" } else { "Hold" };
                    let mac = cfg!(target_os = "macos");
                    let tip = if edit { format!("{verb} {} to edit text", accel_label(&s.command_hotkey, mac)) } else { format!("{verb} {} to dictate", key_label(&s, mac)) };
                    emit_state(&app, if show { hint(tip, None) } else { simple("idle") });
                }
                Act::Finish => {
                    let Some(l) = live.take() else { continue };
                    l.partials.store(true, Ordering::SeqCst); // first: a partial in flight lets go of the model
                    state(&app).recording.store(false, Ordering::SeqCst);
                    let pcm = l.rec.stop();
                    music::set(false); // the mic is closed: the music needn't wait for transcription
                    let _ = state(&app).pill_wake.send(());
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
                    let clip = Clip { quality: st.quality(), muted_seen: st.muted_seen(), warning, speaking_ms: st.speaking_ms(), snr_db: st.floor_and_speech().map(|(floor, speech)| speech - floor), front: l.front.clone(), entry: None };
                    drop(st);
                    if l.command {
                        process_command(&app, pcm, busy.clone());
                    } else {
                        process(&app, pcm, busy.clone(), clip, true, None);
                    }
                }
            }
        }
    });
    tx
}

fn show_later(app: &AppHandle, id: u64, locked: bool, after: Duration) {
    let a = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(after);
        send(&a, Cmd::Show(id, locked));
    });
}

/// Opens the mic for recording `id`, after checking a model or key is ready (no point recording
/// what can't be transcribed). The error is the state to show.
fn start_recording(app: &AppHandle, s: &Settings, id: u64) -> Result<Live, DictationState> {
    platform::wake_accessibility(); // ready by the time the words are pasted
    learn::interrupt(); // the last paste's field is read once more before these words land
    if notices::blocked() {
        return Err(hint("This version of Echotype is retired. Update it in Account to keep dictating.", Some("update")));
    }
    // ponytail: a remote provider costs one keychain read per press (~ms); cache key presence if it shows up in start latency.
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
    let device = audio::pick_input(s.input_device.clone(), s.prefer_builtin_mic, audio::quick_inputs);
    let rec = audio::Recorder::start(device, move |db, gone| {
        let level = lock(&st).push(db);
        lock(&state(&a).pill).set_level(level.muted, level.meter);
        let _ = a.emit("audio-level", level);
        if gone && !lost2.swap(true, Ordering::SeqCst) {
            send(&a, Cmd::Lost(id));
        }
        if started.elapsed() >= MAX_RECORDING && !sent.swap(true, Ordering::SeqCst) {
            send(&a, Cmd::Key(Ev::AutoStop(id)));
        }
    });
    match rec {
        Ok(rec) => {
            let partials = Arc::new(AtomicBool::new(false));
            if s.live_text && s.stt.provider == "local" {
                spawn_partials(app, s, rec.tap(), partials.clone());
            }
            Ok(Live { id, shown: false, locked: false, rec, stats, lost, front: None, command: false, partials })
        }
        Err(_) if platform::microphone_status() == "denied" => {
            Err(DictationState { state: "error", error: Some("Microphone access is off".into()), action: Some("microphone"), ..Default::default() })
        }
        Err(e) => Err(DictationState { state: "error", error: Some(e), action: Some("microphone"), ..Default::default() }),
    }
}

/// Whether to start a partial now, with `secs` recorded and the last one started `since` ago
/// (None: none yet). One loop runs them in turn, so a slow one delays the next, never overlaps it.
fn partial_due(secs: f32, since: Option<Duration>) -> bool {
    secs >= PARTIAL_FROM && since.is_none_or(|d| d >= PARTIAL_EVERY)
}

/// The end of `text` in at most `max` characters, from the start of a word, "…" first when cut.
fn last_words(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_string();
    }
    let tail: String = text.chars().skip(n - max).collect();
    // Unspaced scripts (CJK) have no word start to find: cut anywhere.
    let tail = tail.split_once(' ').map_or(tail.as_str(), |(_, rest)| rest).trim_start();
    format!("…{tail}")
}

/// While `stop` is unset, transcribes the recording's tail about once a second and sends the last
/// words to the pill (`dictation-partial`). Same model, language and dictionary as the final run.
fn spawn_partials(app: &AppHandle, s: &Settings, tap: audio::Tap, stop: Arc<AtomicBool>) {
    let (app, s) = (app.clone(), s.clone());
    std::thread::spawn(move || {
        let Ok((model, _)) = stt::pick_local_model(&s) else { return };
        let (prompt, vad) = (stt::prompt(&s), speakers::vad());
        let mut last: Option<Instant> = None;
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
            if !partial_due(tap.secs(), last.map(|t| t.elapsed())) {
                continue;
            }
            last = Some(Instant::now());
            let pcm = tap.tail(PARTIAL_TAIL.as_secs_f32());
            let Ok(text) = stt::transcribe_partial(&model, &pcm, &stt::languages(&s), s.translate_to_english, &prompt, vad.as_deref(), &stop) else { continue };
            // Silence keeps the words already up.
            if stop.load(Ordering::SeqCst) || !cleanup::has_speech(&text) {
                continue;
            }
            let text = last_words(&stt::collapse_loops(&text), PARTIAL_CHARS);
            lock(&state(&app).pill).set_partial(text.chars().count());
            let _ = app.emit_to("overlay", "dictation-partial", &text);
            let _ = state(&app).pill_wake.send(());
        }
    });
}

/// What the recording itself said about the clip, carried to the done state.
#[derive(Default)]
struct Clip {
    quality: Option<Quality>,
    muted_seen: bool,
    warning: Option<String>,
    /// Recording time minus the silence around the speech, for words per minute.
    speaking_ms: Option<u64>,
    /// How far the loudest part rose above the room (dB): low means noise, not a voice.
    snr_db: Option<f32>,
    /// Live::front. A retry has none, so it gets the user's style.
    front: Option<platform::FrontApp>,
    /// The history entry this audio was recorded as: a re-run replaces its result.
    entry: Option<u64>,
}

/// A dictation's audio kept in memory (AppState last_clip).
struct Kept {
    pcm: Vec<f32>,
    failed: bool,
    /// Clip::entry.
    entry: Option<u64>,
}

/// A successful dictation is kept for Re-run up to 5 minutes (16 kHz f32, about 19 MB); a failed
/// one whatever its length, as before, so Retry never loses it.
// ponytail: longer clips are simply not offered for Re-run; keep them too if anyone misses it.
const KEEP_SAMPLES: usize = 5 * 60 * audio::RATE as usize;

fn keep(pcm: Vec<f32>, failed: bool, entry: Option<u64>) -> Option<Kept> {
    (failed || pcm.len() <= KEEP_SAMPLES).then_some(Kept { pcm, failed, entry })
}

/// Re-run last: the kept clip through the whole pipeline again with one thing changed. Its menu
/// id is "pill-rerun:" plus `id()`.
#[derive(Debug, Clone, PartialEq)]
pub enum Rerun {
    /// A language code, or "auto".
    Lang(String),
    /// A downloaded local speech model.
    Model(String),
    NoCleanup,
}

impl Rerun {
    pub fn id(&self) -> String {
        match self {
            Rerun::Lang(c) => format!("lang:{c}"),
            Rerun::Model(m) => format!("model:{m}"),
            Rerun::NoCleanup => "raw".into(),
        }
    }

    pub fn parse(id: &str) -> Option<Rerun> {
        match id.split_once(':') {
            Some(("lang", c)) => Some(Rerun::Lang(c.into())),
            Some(("model", m)) => Some(Rerun::Model(m.into())),
            None if id == "raw" => Some(Rerun::NoCleanup),
            _ => None,
        }
    }

    /// These settings for this one run; nothing is saved.
    fn apply(&self, s: &mut Settings) {
        match self {
            Rerun::Lang(c) if c == "auto" => s.languages.clear(),
            Rerun::Lang(c) => s.languages = vec![c.clone()],
            Rerun::Model(m) => {
                s.stt.provider = "local".into();
                s.stt.local_model = m.clone(); // what history names
                s.stt.rerun_model = Some(m.clone());
            }
            Rerun::NoCleanup => s.cleanup.enabled = false,
        }
    }
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

/// Whether a finished dictation is pasted into the app in front rather than copied: never into
/// Echotype's own window, and during onboarding only from its real-app step (4) on, so practice
/// runs land in the practice field.
fn pastes(s: &Settings, main_focused: bool) -> bool {
    (s.onboarded || s.onboard_step >= 4) && !main_focused
}

/// STT + cleanup + paste + history on a worker thread, so the hotkey stays responsive.
/// Paste vs copy is decided here, when the text is ready: with Echotype's own window in front
/// (or onboarding not finished, or a retry from the window) pasting would type into Echotype,
/// so the text goes to the clipboard instead. A Re-run (`rerun`) is copied and shown on the card.
fn process(app: &AppHandle, pcm: Vec<f32>, busy: Arc<AtomicBool>, clip: Clip, paste: bool, rerun: Option<Rerun>) {
    if pcm.len() < MIN_SAMPLES {
        telemetry::failure(&current_settings(app), telemetry::Failure::NoSpeech);
        emit_state(app, hint("Didn't catch that", None));
        return;
    }
    busy.store(true, Ordering::SeqCst);
    let app = app.clone();
    let speaking_ms = clip.speaking_ms.unwrap_or(pcm.len() as u64 * 1000 / audio::RATE as u64); // a retry has no level stats
    std::thread::spawn(move || {
        let guard = Busy(busy);
        let mut s = current_settings(&app);
        if let Some(f) = clip.front.clone() {
            s.cleanup.style = settings::app_style(&s.cleanup, Some(&f.id)).to_string();
            *lock(&state(&app).last_app) = Some(f);
        }
        if let Some(r) = &rerun {
            r.apply(&mut s); // after the app's style: "Without cleanup" wins
        }
        let chip_q = chip(clip.quality, false);
        emit_state(&app, DictationState { state: "transcribing", chip: chip_q, ..Default::default() });
        // A panic in a decoder, whisper-rs or the HTTP stack becomes an error state, not a silent loss.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cleanup::run(&s, &pcm, true, || emit_state(&app, DictationState { state: "cleaning", chip: chip_q, ..Default::default() }))
        }))
        .unwrap_or_else(|_| Err("Transcription crashed unexpectedly".into()));
        let failed = result.is_err();
        // Keep the audio for retry_last (failed) and Re-run last; it replaces the one before.
        *lock(&state(&app).last_clip) = keep(pcm, failed, clip.entry);
        let ev = match result {
            Err(e) => {
                telemetry::failure(&s, if error_action(&e) == Some("model") { telemetry::Failure::ModelLoad } else { telemetry::Failure::Other });
                DictationState { state: "error", action: memory_action(&s, &e).or(error_action(&e)), error: Some(e), retry: true, ..Default::default() }
            }
            // Whisper invents "Thank you." and the like from noise; with no voice above the room, it's noise.
            Ok(out) if out.text.is_empty() || (cleanup::whisper_filler(&out.raw) && clip.snr_db.is_some_and(|d| d < NOISE_SNR_DB)) => {
                telemetry::failure(&s, telemetry::Failure::NoSpeech);
                hint(if clip.muted_seen { "No audio. Check your mic" } else { "Didn't catch that" }, clip.muted_seen.then_some("microphone"))
            }
            Ok(out) => {
                *lock(&state(&app).last_output) = Some(out.text.clone());
                *lock(&state(&app).last_cleaned) = out.cleaned.clone();
                let paste = paste && pastes(&s, main_window_focused(&app));
                let warning = out.warning.clone().or(clip.warning);
                let chip = chip(clip.quality, out.no_cleanup && out.warning.is_none());
                // Nothing focused takes text (the desktop, a list, Accessibility off): no ⌘V into nowhere.
                let nowhere = paste && platform::paste_target() == platform::Target::Card;
                let res = deliver(&app, &s, &out.text, paste, nowhere);
                if res.is_ok() && paste && !nowhere && rerun.is_none() && s.learn_words {
                    learn::watch(&app, out.text.clone());
                }
                // After the paste, not before: the disk writes took 15-20 ms off the time to text.
                if rerun.is_none() {
                    record_stats(&app, &s, stats::Source::Dictation, &out.text, speaking_ms); // a re-run's words were counted once
                    telemetry::dictation(&s, stats::words(&out.text), out.stt_ms, out.cleanup_ms);
                }
                if out.cleaned.is_some() {
                    telemetry::failure(&s, telemetry::Failure::CleanupRejected);
                }
                if let Err((_, fell_back)) = &res {
                    telemetry::failure(&s, if *fell_back { telemetry::Failure::PasteFellBack } else { telemetry::Failure::Other });
                }
                let recorded = record_history(&s, &out, clip.entry);
                let wav = recorded.and_then(|ts| {
                    let st = state(&app);
                    let mut kept = lock(&st.last_clip);
                    let k = kept.as_mut()?;
                    k.entry = Some(ts);
                    clip.entry.is_none().then(|| (ts, audio::wav_bytes(&k.pcm)))
                });
                // A new entry keeps its audio on disk too, for Re-run in History.
                if let Some(Err(e)) = wav.map(|(ts, wav)| settings::save_clip(ts, &wav)) {
                    eprintln!("echotype: could not keep the audio for history: {e}");
                }
                let action = warning.as_deref().and_then(|w| memory_action(&s, w));
                match res {
                    // A re-run is not pasted: the card shows what it heard this time, with Copy.
                    Ok(()) if rerun.is_some() && out.cleaned.is_none() => DictationState {
                        warning: warning.or(Some(format!("Re-run copied, {} to paste", platform::PASTE_KEYS))),
                        copied: true,
                        action: None, // nothing to fix: a re-run is never pasted
                        ..card(&s, out.text, None, chip)
                    },
                    // The guard pasted the words as spoken: the card offers the cleaned version.
                    Ok(()) if out.cleaned.is_some() => DictationState { state: "done", text: Some(out.text), warning, copied: !paste, chip, action, card: true, cleaned: out.cleaned, ..Default::default() },
                    Ok(()) => DictationState { state: "done", text: Some(out.text), warning, copied: !paste, chip, action, ..Default::default() },
                    // Nowhere to paste, or the keystroke blocked (secure input): the card, with Copy.
                    Err((e, true)) => {
                        // Not pasted anywhere: a sound of its own, so it's noticed without looking.
                        if s.sounds {
                            platform::play_sound(&app, "nopaste");
                        }
                        card(&s, out.text, Some(e), chip)
                    }
                    Err((e, false)) => DictationState { state: "error", text: Some(out.text), error: Some(e), warning, ..Default::default() },
                }
            }
        };
        drop(guard);
        emit_state(&app, ev);
    });
}

/// Pastes `text`, or copies it when `paste` is off. `nowhere`: nothing focused takes text.
/// The error says whether the text is on the clipboard, as platform::paste does.
fn deliver(app: &AppHandle, s: &Settings, text: &str, paste: bool, nowhere: bool) -> Result<(), (String, bool)> {
    if nowhere && !platform::accessibility_granted() {
        // As before: on the clipboard, since there is no way to paste it.
        platform::copy(text).map_err(|e| (e, false)).and(Err(("Accessibility is off, so Echotype copied instead of pasting".into(), true)))
    } else if nowhere {
        Err(("Nowhere to paste".to_string(), true))
    } else if paste {
        platform::paste(app, text, s.restore_clipboard)
    } else {
        platform::copy(text).map_err(|e| (e, false))
    }
}

/// Command Mode can run: Accessibility (to read the selection) and a provider that follows
/// instructions. The hint says what is missing.
fn command_ready(s: &Settings, command: bool) -> Result<(), DictationState> {
    if !command {
        return Ok(());
    }
    if !platform::accessibility_granted() {
        return Err(hint("Command Mode needs Accessibility to read your selection", Some("accessibility")));
    }
    match cleanup::command_blocked(s, cleanup::keyless(s)) {
        Some(why) => Err(hint(why, Some(if s.cleanup.provider == "echotype" { "writer" } else { "cleanup" }))),
        None => Ok(()),
    }
}

/// Command Mode after the key comes up: read the selection, transcribe the instruction, have the
/// cleanup provider apply it and paste the result over the selection. On any failure the user's
/// text stays as it was and the pill says why.
// ponytail: results stay out of history and stats; add them if people want edits counted.
fn process_command(app: &AppHandle, pcm: Vec<f32>, busy: Arc<AtomicBool>) {
    if pcm.len() < MIN_SAMPLES {
        emit_state(app, hint("Didn't catch that", None));
        return;
    }
    busy.store(true, Ordering::SeqCst);
    let app = app.clone();
    std::thread::spawn(move || {
        let guard = Busy(busy);
        let s = current_settings(&app);
        let ev = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_command(&app, &s, &pcm)))
            .unwrap_or_else(|_| DictationState { state: "error", error: Some("Command Mode crashed unexpectedly. Your text is unchanged.".into()), ..Default::default() });
        drop(guard);
        emit_state(&app, ev);
    });
}

fn run_command(app: &AppHandle, s: &Settings, pcm: &[f32]) -> DictationState {
    let error = |e: String| DictationState { state: "error", error: Some(e), ..Default::default() };
    // The selection first, while it is still the one the user meant. Echotype's own window, or
    // focus that takes no text: nothing to read, so the instruction is something to write.
    let paste = pastes(s, main_window_focused(app));
    let nowhere = paste && platform::paste_target() == platform::Target::Card;
    let selection = if !paste || nowhere {
        None
    } else if !platform::wait_for_modifiers_up(Duration::from_millis(500)) {
        return error("Let go of the shortcut to edit. Your text is unchanged.".into()); // a ⌘C now would carry its modifiers
    } else {
        match platform::copy_selection(app) {
            Ok(t) => t,
            Err(e) => return error(format!("Couldn't read the selection: {e}. Your text is unchanged.")),
        }
    };
    emit_state(app, simple("transcribing"));
    // The instruction as spoken: no cleanup pass, which would "clean" it into something else.
    let mut raw = s.clone();
    raw.cleanup.enabled = false;
    let instruction = match cleanup::run(&raw, pcm, false, || {}) {
        Ok(out) if !out.text.is_empty() => out.text,
        Ok(_) => return hint("Didn't catch that", None),
        Err(e) => return DictationState { action: error_action(&e), ..error(e) },
    };
    emit_state(app, simple("cleaning"));
    let provider = settings::preset(&s.cleanup.provider).map_or(s.cleanup.provider.clone(), |p| p.label.to_string());
    let text = match cleanup::chat(s, cleanup::COMMAND_PROMPT, &cleanup::command_input(selection.as_deref(), &instruction), Duration::from_secs(60)) {
        Ok(t) if !t.is_empty() => t,
        Ok(_) => return error(format!("{provider} returned nothing. Your text is unchanged.")),
        Err(e) => return DictationState { action: Some("cleanup"), ..error(format!("Command failed: {provider}: {}. Your text is unchanged.", e.lines().next().unwrap_or_default().trim_end_matches('.'))) },
    };
    *lock(&state(app).last_output) = Some(text.clone());
    if llm::wanted(s) {
        // The edit swapped Echotype Writer in; have the cleaner back before the next dictation.
        let s = s.clone();
        std::thread::spawn(move || llm::apply(&s));
    }
    match deliver(app, s, &text, paste, nowhere) {
        Ok(()) => DictationState { state: "done", text: Some(text), copied: !paste, ..Default::default() },
        Err((e, true)) => {
            if s.sounds {
                platform::play_sound(app, "nopaste");
            }
            card(s, text, Some(e), None)
        }
        Err((e, false)) => DictationState { state: "error", text: Some(text), error: Some(e), ..Default::default() },
    }
}

/// The "nowhere to paste" card. Home offers the Accessibility fix when that is why.
fn card(s: &Settings, text: String, warning: Option<String>, chip: Option<&'static str>) -> DictationState {
    DictationState {
        state: "done",
        text: Some(text),
        warning,
        chip,
        card: true,
        // Paste-last needs Accessibility too, so the card only names it when it would work.
        paste_key: (!s.paste_last_hotkey.is_empty() && platform::accessibility_granted()).then(|| accel_label(&s.paste_last_hotkey, cfg!(target_os = "macos"))),
        action: (!platform::accessibility_granted()).then_some("accessibility"),
        ..Default::default()
    }
}

/// Paste-last / copy-last: the last dictated text again, after the shortcut's own keys are up so
/// the synthetic ⌘V is not read as ⌘⌃V (which would be this shortcut again).
fn paste_last(app: &AppHandle, paste: bool) {
    // One at a time: a synthetic ⌘V that the OS reads as this shortcut again must not loop.
    static BUSY: AtomicBool = AtomicBool::new(false);
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        struct Release;
        impl Drop for Release {
            fn drop(&mut self) {
                BUSY.store(false, Ordering::SeqCst);
            }
        }
        let _release = Release;
        let Some(text) = lock(&state(&app).last_output).clone() else {
            if !state(&app).recording.load(Ordering::SeqCst) {
                emit_state(&app, hint("Nothing to paste yet", None));
            }
            return;
        };
        let s = current_settings(&app);
        let paste = paste && !main_window_focused(&app) && platform::wait_for_modifiers_up(Duration::from_millis(500));
        let res = if paste { platform::paste(&app, &text, s.restore_clipboard) } else { platform::copy(&text).map_err(|e| (e, false)) };
        if res.is_ok() && state(&app).recording.load(Ordering::SeqCst) {
            return; // the pill is showing a live recording; a successful paste is its own feedback
        }
        emit_state(&app, match res {
            Ok(()) if paste => hint("Pasted", None),
            Ok(()) | Err((_, true)) => hint(format!("Copied, {} to paste", platform::PASTE_KEYS), None),
            Err((e, false)) => DictationState { state: "error", error: Some(e), ..Default::default() },
        });
    });
}

/// Counts one result in the local usage stats (words and speed, never text) when they are on.
/// Before record_history, so the one-time history import can't count it twice.
pub(crate) fn record_stats(app: &AppHandle, s: &Settings, source: stats::Source, text: &str, speaking_ms: u64) {
    let Some(words) = stats::to_count(s, text) else { return };
    match stats::record(source, words, speaking_ms) {
        Ok(()) => {
            let _ = app.emit_to("main", "stats-updated", ());
        }
        Err(e) => eprintln!("echotype: could not write stats: {e}"),
    }
}

/// Adds `out` to history, or, re-running entry `rerun_of`, replaces that entry's result. The
/// entry's ts when it was written.
fn record_history(s: &Settings, out: &cleanup::Output, rerun_of: Option<u64>) -> Option<u64> {
    if !s.history_enabled {
        return None;
    }
    let cleaned = cleanup::wants_cleanup(s, &out.raw) && !out.no_cleanup;
    let entry = HistoryEntry {
        ts: rerun_of.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)),
        raw: out.raw.clone(),
        text: out.text.clone(),
        stt_ms: out.stt_ms,
        cleanup_ms: out.cleanup_ms,
        // The model that ran (Automatic's pick, a re-run's), so Re-run offers the others.
        stt_provider: if s.stt.provider == "local" { format!("local:{}", stt::model_id(&stt::local_model(s))) } else { s.stt.provider.clone() },
        cleanup_provider: cleaned.then(|| s.cleanup.provider.clone()),
        no_cleanup: out.no_cleanup,
        cleaned: out.cleaned.clone(),
        original: None,
    };
    let res = if rerun_of.is_some() { settings::rerun_history(&entry) } else { settings::append_history(&entry) };
    res.map(|()| entry.ts).inspect_err(|e| eprintln!("echotype: could not write history: {e}")).ok()
}

/// Installs, retargets or removes the fn / modifier tap for `s.talk_key`. Returns why the
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
                hotkey::TapEv::Down => Ev::ModPress,
                hotkey::TapEv::Up => Ev::Release,
                hotkey::TapEv::Other => Ev::OtherKey,
                hotkey::TapEv::Click => Ev::Click,
            };
            let _ = tx.send(Cmd::Key(ev));
        });
        let why = blocked.or(res.err());
        static LAST: Mutex<Option<String>> = Mutex::new(None);
        let mut last = lock(&LAST);
        if *last != why {
            eprintln!("echotype: talk key {}: {}", s.talk_key, why.as_deref().unwrap_or("active"));
            last.clone_from(&why);
        }
        why
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

const SHORTCUT_NAMES: [&str; 4] = ["dictation", "paste last", "copy last", "Command Mode"];

fn shortcuts(s: &Settings) -> [String; 4] {
    [s.hotkey.clone(), s.paste_last_hotkey.clone(), s.copy_last_hotkey.clone(), s.command_hotkey.clone()]
}

/// The settings fields whose shortcut is set but not registered right now: taken by another app
/// at launch, and fixed by picking another key. Asked live, so a fixed one stops being reported.
fn unregistered_shortcuts(s: &Settings, registered: impl Fn(&str) -> bool) -> Vec<&'static str> {
    let fields = ["hotkey", "paste_last_hotkey", "copy_last_hotkey", "command_hotkey"];
    shortcuts(s).iter().zip(fields).filter(|(k, _)| !k.is_empty() && !registered(k)).map(|(_, f)| f).collect()
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
    // On-screen tests drive the pill without ever putting the settings window over the desktop.
    if std::env::var_os("ECHOTYPE_NO_MAIN").is_some() {
        return;
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Light, dark or the system's appearance for the settings window. The page's light/dark styles
/// follow the window's appearance, so this is all it takes.
pub(crate) fn apply_theme(app: &AppHandle, s: &Settings) {
    let theme = match s.look.as_str() {
        "light" => Some(tauri::Theme::Light),
        "dark" => Some(tauri::Theme::Dark),
        _ => None,
    };
    for label in ["main", "meeting"] {
        if let Some(w) = app.get_webview_window(label) {
            let _ = w.set_theme(theme);
        }
    }
}

/// Opens the settings window on `page` (pill menu, notice actions).
fn open_page(app: &AppHandle, page: &str) {
    show_main(app);
    let _ = app.emit_to("main", "navigate", page);
}

// ---------- commands (the UI contract) ----------

type R<T> = Result<T, String>;

#[tauri::command]
async fn get_settings(app: AppHandle) -> R<Settings> {
    Ok(current_settings(&app))
}

/// Saves `new`, keeping the global shortcuts and launch at login in step with what is on
/// disk. Each changes only when it differs. A shortcut that cannot register (or is already one
/// of the others) never blocks the rest: it keeps its old key and the reason comes back as a
/// warning; the same for launch at login. A failed save undoes every swap, so the registrations,
/// the file and memory never disagree. The dictation shortcut can't be unbound; the others can ("").
fn commit(old: &Settings, mut new: Settings, swap: impl Fn(&str, &str) -> R<()>, autostart: impl Fn(bool) -> R<()>, save: impl Fn(&Settings) -> R<()>) -> R<(Settings, Option<String>)> {
    let olds = shortcuts(old);
    let mut keys = shortcuts(&new);
    if keys[0].trim().is_empty() {
        keys[0] = olds[0].clone();
    }
    let (mut warnings, mut swapped) = (vec![], vec![]);
    // Slots moving onto a key another slot is leaving go last, after that key is unregistered.
    let mut order = [0, 1, 2, 3];
    order.sort_by_key(|&i| olds.iter().any(|o| same_shortcut(&keys[i], o)));
    for i in order {
        if keys[i] == olds[i] {
            continue;
        }
        let clash = (0..4).find(|&j| j != i && same_shortcut(&keys[i], &keys[j]));
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
    [new.hotkey, new.paste_last_hotkey, new.copy_last_hotkey, new.command_hotkey] = keys.clone();
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

/// Held while settings change or the talk key is re-applied, so a status check can never
/// install the tap for a talk key a concurrent save is replacing.
static SAVE: Mutex<()> = Mutex::new(());

/// The one way settings change: one at a time, shortcuts/login item/file/memory/local API/talk
/// key updated together, then every window gets `settings-changed` with the full result. In R1
/// the settings window is its only writer; the R2 pill menu and tray will commit through it too.
fn commit_settings(app: &AppHandle, change: impl FnOnce(&Settings) -> Settings) -> R<Option<String>> {
    let _g = lock(&SAVE);
    let old = current_settings(app);
    let mut new = change(&old);
    new.migrate(); // never save a provider this build hides
    if new.local_api.port < settings::MIN_PORT {
        return Err(format!("The local API port must be between {} and 65535", settings::MIN_PORT));
    }
    let (new, warning) = commit(&old, new, |o, n| swap_hotkey(app, o, n), |on| set_autostart(app, on), Settings::save)?;
    *state(app).settings.lock().unwrap_or_else(|e| e.into_inner()) = new.clone();
    if stt::local_model(&new) != stt::local_model(&old) || (new.stt.provider != old.stt.provider && new.stt.provider == "local") {
        let s = new.clone();
        std::thread::spawn(move || stt::warm_up(&s));
    }
    if llm::wanted(&new) != llm::wanted(&old) {
        let s = new.clone();
        std::thread::spawn(move || llm::apply(&s)); // start the built-in cleanup model, or free its memory
    }
    if old.history_enabled && !new.history_enabled {
        *lock(&state(app).last_output) = None; // switching history off also forgets the in-memory copy
        *lock(&state(app).last_cleaned) = None;
        settings::clear_clips();
    }
    if new.talk_key != old.talk_key {
        apply_talk_key(app, &new);
    }
    if new.look != old.look {
        apply_theme(app, &new);
    }
    let _ = app.emit("settings-changed", &new);
    let _ = state(app).pill_wake.send(());
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
    let (total, avail) = stt::ram_mb();
    Ok(stt::list_models(total, avail))
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
        let res = if llm::find(&id).is_some() { llm::download(&id, |d, t| emit(d, t, false, None)) } else { stt::download(&id, |d, t| emit(d, t, false, None)) };
        match &res {
            Ok(()) => emit(0, None, true, None),
            Err(e) => emit(0, None, true, Some(e)),
        }
        res
    });
    job.await.map_err(|e| e.to_string())?
}

#[tauri::command]
fn cancel_download(id: String) {
    stt::cancel_download(&id);
}

#[tauri::command]
async fn delete_model(id: String) -> R<()> {
    if llm::find(&id).is_some() { llm::delete(&id) } else { stt::delete(&id) }
}

#[tauri::command]
async fn list_input_devices() -> R<Vec<audio::InputDevice>> {
    Ok(audio::input_devices())
}

#[tauri::command]
async fn get_history(limit: Option<usize>) -> R<Vec<HistoryEntry>> {
    Ok(settings::get_history(limit.unwrap_or(100)))
}

/// What Echotype would bring over from Wispr Flow on this machine (None: not installed).
#[tauri::command]
async fn wispr_import_preview() -> Option<wispr::Found> {
    tauri::async_runtime::spawn_blocking(wispr::detect).await.ok().flatten().filter(|f| !f.is_empty())
}

/// Imports it into the settings; returns what changed ("9 dictionary words", ...).
#[tauri::command]
async fn wispr_import(app: AppHandle) -> R<Vec<String>> {
    let found = tauri::async_runtime::spawn_blocking(wispr::detect).await.map_err(|e| e.to_string())?.ok_or("Wispr Flow is not on this Mac")?;
    let mut done = vec![];
    commit_settings(&app, |old| {
        let mut s = old.clone();
        done = wispr::apply(&mut s, &found);
        s
    })?;
    Ok(done)
}

#[tauri::command]
async fn update_history(app: AppHandle, ts: u64, text: String) -> R<()> {
    let was = settings::update_history(ts, &text)?;
    // Fixing the newest dictation: Paste last / Copy last give the corrected text too.
    let st = state(&app);
    let mut last = lock(&st.last_output);
    if last.as_deref() == Some(was.as_str()) {
        *last = Some(text);
    }
    Ok(())
}

#[tauri::command]
async fn clear_history(app: AppHandle) -> R<()> {
    *lock(&state(&app).last_output) = None;
    *lock(&state(&app).last_cleaned) = None;
    settings::clear_history()
}

#[tauri::command]
async fn transcribe_file(app: AppHandle, path: String) -> R<cleanup::Output> {
    let s = current_settings(&app);
    tauri::async_runtime::spawn_blocking(move || {
        let out = transcribe_path(&s, &path)?;
        record_stats(&app, &s, stats::Source::File, &out.text, 0);
        record_history(&s, &out, None);
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn transcribe_path(s: &Settings, path: &str) -> R<cleanup::Output> {
    let bytes = std::fs::read(path).map_err(|e| format!("Could not read {path}: {e}"))?;
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str());
    let pcm = audio::decode(bytes, ext)?;
    cleanup::run(s, &pcm, false, || {})
}

/// The Stats page: totals, speeds, streaks and a grid of `range` weeks (default a year).
#[tauri::command]
async fn get_stats(range: Option<u32>) -> R<stats::View> {
    blocking(move || Ok(stats::view(range.unwrap_or(stats::DEFAULT_WEEKS)))).await
}

#[tauri::command]
async fn clear_stats(app: AppHandle) -> R<()> {
    blocking(stats::clear).await?;
    let _ = app.emit_to("main", "stats-updated", ());
    Ok(())
}

#[derive(Serialize)]
struct SystemInfo {
    total_ram_gb: f64,
    available_ram_gb: f64,
    os: &'static str,
    arch: &'static str,
    /// Launch at login can be offered (running from /Applications, not translocated).
    can_autostart: bool,
    /// The built-in cleanup model is on disk, and its size (or download size) in MB.
    echotype_model_installed: bool,
    echotype_model_mb: u64,
    /// The Echotype cleaners with their RAM, for the Cleanup page's picker (advanced settings).
    cleaners: Vec<llm::CleanerStatus>,
    /// Echotype Writer, for the Models page (Command Mode with the built-in provider).
    writer: llm::CleanerStatus,
    /// 8 GB or less: Automatic picks the light setup.
    low_ram: bool,
    /// Echotype Cloud is in this build (settings::CLOUD).
    cloud: bool,
}

#[tauri::command]
async fn get_system_info(app: AppHandle) -> R<SystemInfo> {
    let (total, avail) = stt::ram_mb();
    let gb = |mb: u64| (mb as f64 / 1024.0 * 10.0).round() / 10.0;
    let (echotype_model_installed, echotype_model_mb) = llm::status(&current_settings(&app));
    Ok(SystemInfo {
        total_ram_gb: gb(total),
        available_ram_gb: gb(avail),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        can_autostart: can_autostart(),
        echotype_model_installed,
        echotype_model_mb,
        cleaners: llm::cleaners(total),
        writer: llm::writer(total),
        low_ram: stt::low_ram(total),
        cloud: settings::CLOUD,
    })
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

// ---------- the pill (the only commands the overlay may call) ----------

/// A click on the idle pill starts a hands-free recording; on a recording (or ✓) it finishes.
/// The mouse-up that ends a drag is not a click.
#[tauri::command]
async fn pill_click(app: AppHandle) -> R<()> {
    let (st, now) = (state(&app), Instant::now());
    if pill::is_drop(&app, now) {
        return Ok(());
    }
    let mut clicks = lock(&st.clicks);
    if clicks.accept(now) {
        if !st.recording.load(Ordering::SeqCst) {
            clicks.started(now);
        }
        send(&app, Cmd::Click);
    }
    Ok(())
}

/// The button went down on the pill: it may be the start of a drag.
#[tauri::command]
async fn pill_drag(app: AppHandle) -> R<()> {
    pill::grab(&app);
    Ok(())
}

/// Double-click on the pill: back to bottom-centre of this display, and the dictation its
/// first click started is taken back.
#[tauri::command]
async fn pill_reset(app: AppHandle) -> R<()> {
    if pill::is_drop(&app, Instant::now()) {
        return Ok(()); // the second press of a "double-click" was a drag
    }
    if lock(&state(&app).clicks).undo_start(Instant::now()) {
        send(&app, Cmd::Key(Ev::Cancel));
    }
    pill::reset(&app);
    Ok(())
}

/// ✕ on the recording pill.
#[tauri::command]
async fn pill_cancel(app: AppHandle) -> R<()> {
    send(&app, Cmd::Key(Ev::Cancel));
    Ok(())
}

/// A notice with an action: Home shows it with the button that fixes it. Shares the click
/// debounce, so the second half of a double-click that started dictation and got an error
/// notice back never opens the settings window.
#[tauri::command]
async fn pill_open(app: AppHandle) -> R<()> {
    if !pill::is_drop(&app, Instant::now()) && lock(&state(&app).clicks).accept(Instant::now()) {
        open_page(&app, "home");
    }
    Ok(())
}

/// The real pill acts out a dictation: it listens to a made-up voice, thinks, then shows the card.
/// No microphone, nothing pasted, the last dictation untouched. Settings → Pill "Show on screen"
/// (and ECHOTYPE_TEST_DEMO for on-screen checks). A real dictation always wins.
fn pill_demo(app: &AppHandle) {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    let recording = |h: &AppHandle| state(h).recording.load(Ordering::SeqCst);
    if recording(app) || RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let h = app.clone();
    std::thread::spawn(move || {
        let s = current_settings(&h);
        emit_state(&h, DictationState { state: "recording", ..Default::default() });
        for i in 0..50 {
            if recording(&h) {
                return RUNNING.store(false, Ordering::SeqCst);
            }
            let meter = (0.35 + 0.3 * (i as f32 / 3.0).sin()).clamp(0.0, 1.0);
            let _ = h.emit("audio-level", audio::Level { meter, ..Default::default() });
            std::thread::sleep(Duration::from_millis(60));
        }
        emit_state(&h, DictationState { state: "transcribing", ..Default::default() });
        std::thread::sleep(Duration::from_millis(1400));
        if !recording(&h) {
            emit_state(&h, card(&s, "Let's move the standup to 10:30 tomorrow.".into(), Some("Nowhere to paste".into()), None));
        }
        RUNNING.store(false, Ordering::SeqCst);
    });
}

#[tauri::command]
async fn show_pill_demo(app: AppHandle) -> R<()> {
    pill_demo(&app);
    Ok(())
}

/// Settings → Sounds: plays a pack's start, stop and not-pasted sounds in turn.
#[tauri::command]
async fn preview_sounds(app: AppHandle, pack: String) -> R<()> {
    let pack = settings::SOUND_PACKS.iter().find(|p| **p == pack).ok_or("Unknown sound pack")?;
    std::thread::spawn(move || {
        for (name, wait) in [("start", 450), ("stop", 650), ("nopaste", 0)] {
            platform::play_from(&app, pack, name);
            std::thread::sleep(Duration::from_millis(wait));
        }
    });
    Ok(())
}

/// Copy on the card (and the menu-bar menu's copy item): the last transcript to the clipboard.
#[tauri::command]
async fn pill_copy(app: AppHandle) -> R<()> {
    copy_last(&app)
}

fn copy_last(app: &AppHandle) -> R<()> {
    let text = lock(&state(app).last_output).clone().ok_or("Nothing to copy yet")?;
    platform::copy(&text)
}

/// "Copy cleaned" on the card the guard shows: the cleanup it rejected, to the clipboard. Not
/// ⌘Z and paste: undo is not reliable across apps (it can undo the user's own typing, or only
/// part of the paste), and the app in front may have changed since.
#[tauri::command]
async fn pill_copy_cleaned(app: AppHandle) -> R<()> {
    copy_cleaned(&app)
}

pub(crate) fn copy_cleaned(app: &AppHandle) -> R<()> {
    let text = lock(&state(app).last_cleaned).clone().ok_or("No cleaned version to copy")?;
    platform::copy(&text)
}

/// Re-run last (pill menu, History): the kept clip again with one change. Copied, never pasted.
pub(crate) fn rerun_last(app: &AppHandle, r: Rerun) {
    send(app, Cmd::Rerun(r, None));
}

/// Settings as History entry `e` ran, for the Re-run choices that would change its result: the
/// speech model it used, and no "Without cleanup" when it had none. Languages are not recorded.
fn as_ran(mut s: Settings, e: &HistoryEntry) -> Settings {
    match e.stt_provider.strip_prefix("local:") {
        Some(m) => {
            s.stt.provider = "local".into();
            s.stt.rerun_model = Some(m.into());
        }
        None => s.stt.provider = e.stt_provider.clone(),
    }
    s.cleanup.enabled &= e.cleanup_provider.is_some();
    s
}

/// The audio of the last dictation (`ts` None) or of History entry `ts` is still kept.
fn has_clip(app: &AppHandle, ts: Option<u64>) -> bool {
    ts.map_or_else(|| lock(&state(app).last_clip).is_some(), |ts| settings::clip_path(ts).is_file())
}

/// What Re-run offers for the last dictation or History entry `ts`: [{id, label}], empty when
/// its audio is not kept.
#[tauri::command]
async fn rerun_options(app: AppHandle, ts: Option<u64>) -> R<Vec<serde_json::Value>> {
    if !has_clip(&app, ts) {
        return Ok(vec![]);
    }
    let mut s = current_settings(&app);
    if let Some(ts) = ts {
        let Some(e) = settings::get_history(usize::MAX).into_iter().find(|e| e.ts == ts) else { return Ok(vec![]) };
        s = as_ran(s, &e);
    }
    Ok(pill::rerun_options(&s, |id| stt::model_file(id).is_some()).into_iter().map(|(r, label)| serde_json::json!({ "id": r.id(), "label": label })).collect())
}

/// A History entry's re-run replaces its text (the first one is kept) and, like Re-run last, is
/// copied and shown on the card, never pasted.
#[tauri::command]
async fn rerun(app: AppHandle, id: String, ts: Option<u64>) -> R<()> {
    let r = Rerun::parse(&id).ok_or("Unknown re-run")?;
    if !has_clip(&app, ts) {
        return Err("There is no recording to re-run".into());
    }
    send(&app, Cmd::Rerun(r, ts));
    Ok(())
}

/// ✕ on the card.
#[tauri::command]
async fn pill_dismiss(app: AppHandle) -> R<()> {
    pill::dismiss_card(&app);
    Ok(())
}

/// Right-click on the idle pill: the native menu, built and handled in Rust.
#[tauri::command]
async fn pill_menu(app: AppHandle, window: tauri::WebviewWindow) -> R<()> {
    pill::show_menu(&app, &window).map_err(|e| e.to_string())
}

/// Talk key and shortcut health for Settings and onboarding, re-checked each time it is asked
/// (on window focus and when Settings opens), so a grant or a Globe-key change applies at once.
#[tauri::command]
async fn shortcut_status(app: AppHandle) -> R<serde_json::Value> {
    blocking(move || shortcut_status_now(&app)).await
}

fn shortcut_status_now(app: &AppHandle) -> R<serde_json::Value> {
    let (s, talk_key_error) = {
        let _g = lock(&SAVE);
        let s = current_settings(app);
        let e = apply_talk_key(app, &s);
        (s, e)
    };
    let app = app.clone();
    let globe = cfg!(target_os = "macos") && hotkey::read_globe_setting();
    #[cfg(target_os = "macos")]
    let input_monitoring = hotkey::tap::allowed();
    #[cfg(not(target_os = "macos"))]
    let input_monitoring = true;
    Ok(serde_json::json!({
        "talk_key_active": talk_tap_active(),
        "talk_key_error": talk_key_error,
        "input_monitoring": input_monitoring,
        "globe_does_nothing": globe,
        // Granted but macOS still refuses the tap: only a new process picks the grant up.
        "needs_relaunch": input_monitoring && hotkey::keycode(&s.talk_key).is_some() && (s.talk_key != "fn" || globe) && !talk_tap_active() && talk_key_error.is_some(),
        "unregistered_shortcuts": unregistered_shortcuts(&s, |k| parse_hotkey(k).is_ok_and(|sc| app.global_shortcut().is_registered(sc))),
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
    if st.last_clip.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_none_or(|k| !k.failed) {
        return Err("There is no failed recording to retry".into());
    }
    st.dictation.send(Cmd::Retry).map_err(|e| e.to_string())
}

#[tauri::command]
async fn check_permissions() -> R<serde_json::Value> {
    Ok(serde_json::json!({
        "microphone": platform::microphone_status(),
        "microphone_restricted": platform::microphone_restricted(),
        "accessibility": platform::accessibility_granted(),
        "screen_recording": capture::screen_recording_allowed(),
        "wayland": platform::wayland(),
        "paste_tool": platform::paste_tool(),
    }))
}

// ---------- onboarding ----------

/// Onboarding's one microphone button: the system prompt now, not halfway through a first
/// dictation. Returns the status as it stands; the window polls for the answer.
#[tauri::command]
async fn request_microphone() -> R<&'static str> {
    platform::request_microphone();
    Ok(platform::microphone_status())
}

/// Brings the settings window forward (a permission was just granted in System Settings, or a
/// dictation was just pasted into another app). Returns the app that was in front, if not Echotype.
#[tauri::command]
async fn focus_main(app: AppHandle) -> R<Option<String>> {
    let front = platform::frontmost_app(&app).map(|f| f.name);
    show_main(&app);
    Ok(front)
}

/// The app the last dictation went to (with "Match the app you're typing in" on) and its
/// built-in style, "" for the user's own: the Cleanup page offers to add it to its list.
#[tauri::command]
async fn last_app(app: AppHandle) -> R<Option<serde_json::Value>> {
    let front = lock(&state(&app).last_app).clone();
    Ok(front.map(|f| serde_json::json!({ "style": settings::builtin_style(&f.id).unwrap_or(""), "id": f.id, "name": f.name })))
}

/// "Style per app" → Add an app: the installed apps, each with its built-in style ("" for the user's own).
#[tauri::command]
async fn installed_apps() -> R<Vec<serde_json::Value>> {
    blocking(|| Ok(platform::installed_apps().into_iter().map(|f| serde_json::json!({ "style": settings::builtin_style(&f.id).unwrap_or(""), "id": f.id, "name": f.name })).collect())).await
}

/// Settings → Updates: this version and the newer one on GitHub Releases, if any. The update's
/// signature is checked against tauri.conf.json's updater pubkey before it installs.
#[tauri::command]
async fn check_update(app: AppHandle) -> R<serde_json::Value> {
    use tauri_plugin_updater::UpdaterExt;
    let current = app.package_info().version.to_string();
    let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| format!("Couldn't check for updates: {e}"))?;
    Ok(serde_json::json!({ "current": current, "version": update.as_ref().map(|u| u.version.clone()), "notes": update.and_then(|u| u.body) }))
}

/// Downloads and installs the newer version, then restarts into it. Never mid-meeting.
#[tauri::command]
async fn install_update(app: AppHandle) -> R<()> {
    use tauri_plugin_updater::UpdaterExt;
    if meeting::recording() {
        return Err("Stop the meeting notes first; the update restarts Echotype.".into());
    }
    let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| format!("Couldn't check for updates: {e}"))?;
    let Some(update) = update else { return Err("Echotype is up to date.".into()) };
    update.download_and_install(|_, _| {}, || {}).await.map_err(|e| format!("The update didn't install: {e}"))?;
    app.restart()
}

/// Windows (50 ms each) a sound check runs before it stops by itself: two minutes.
// ponytail: the recorder keeps its audio in memory (~23 MB at 48 kHz for two minutes); the cap is the bound.
const MIC_CHECK_WINDOWS: u32 = 2400;

/// Onboarding's sound check: the microphone's level as `mic-level` (20 Hz, like `audio-level`)
/// without a dictation, so the orb can listen before a speech model is ready. `on: false` stops it.
#[tauri::command]
async fn mic_check(app: AppHandle, on: bool) -> R<()> {
    static MIC: Mutex<Option<audio::Recorder>> = Mutex::new(None);
    let old = lock(&MIC).take();
    if let Some(r) = old {
        blocking(move || {
            r.stop();
            Ok(())
        })
        .await?;
    }
    if !on {
        return Ok(());
    }
    let s = current_settings(&app);
    let device = audio::pick_input(s.input_device, s.prefer_builtin_mic, audio::quick_inputs);
    let (mut stats, mut n) = (LevelStats::default(), 0u32);
    let rec = blocking(move || {
        audio::Recorder::start(device, move |db, _| {
            let _ = app.emit_to("main", "mic-level", stats.push(db));
            n += 1;
            if n == MIC_CHECK_WINDOWS {
                std::thread::spawn(|| lock(&MIC).take().map(audio::Recorder::stop));
            }
        })
    })
    .await?;
    *lock(&MIC) = Some(rec);
    Ok(())
}

/// A success moment's sound, from the chosen pack (when sounds are on).
#[tauri::command]
async fn play_sound(app: AppHandle, name: String) -> R<()> {
    let name = ["start", "stop", "lock"].into_iter().find(|n| *n == name).ok_or("Unknown sound")?;
    if current_settings(&app).sounds {
        platform::play_sound(&app, name);
    }
    Ok(())
}

/// "Reset and ask again": drops a stale Accessibility entry, asks again and opens the pane.
#[tauri::command]
async fn reset_accessibility(app: AppHandle) -> R<()> {
    let id = app.config().identifier.clone();
    blocking(move || platform::reset_accessibility(&id)).await?;
    open_permission_settings(app, "accessibility".into()).await
}

/// The first-run film (macOS): the settings window becomes a clear, borderless stage over the
/// whole main display for the stars, the setup card and the fall home, with clicks falling through
/// everywhere but the card (`film_hit`). `on: false` gives it its normal frame back, shown only if
/// `show`, and lets the pill appear where the dots landed.
#[tauri::command]
async fn film_window(app: AppHandle, on: bool, show: bool) -> R<()> {
    let w = app.get_webview_window("main").ok_or("no settings window")?;
    let st = state(&app);
    #[cfg(target_os = "macos")]
    {
        use tauri::utils::config::WindowEffectsConfig;
        use tauri::window::{Effect, EffectState};
        use tauri::{LogicalPosition, LogicalSize, TitleBarStyle};
        let e = |r: tauri::Result<()>| r.map_err(|e| e.to_string());
        if on {
            let m = w.primary_monitor().map_err(|e| e.to_string())?.ok_or("no display")?;
            let (pos, size) = (m.position().to_logical::<f64>(m.scale_factor()), m.size().to_logical::<f64>(m.scale_factor()));
            e(w.set_effects(None::<WindowEffectsConfig>))?;
            e(w.set_decorations(false))?;
            e(w.set_shadow(false))?;
            e(w.set_resizable(false))?;
            e(w.set_min_size(None::<LogicalSize<f64>>))?;
            e(w.set_size(LogicalSize::new(size.width, size.height)))?;
            e(w.set_position(LogicalPosition::new(pos.x, pos.y)))?;
            e(w.set_always_on_top(true))?; // over other apps' windows until setup needs them (film_float)
            *lock(&st.film_hit) = None;
            e(w.set_ignore_cursor_events(true))?;
            if !st.film.swap(true, Ordering::SeqCst) {
                // Clicks reach the window only over the card: the cursor is polled, as the pill does.
                // ponytail: 30 Hz poll; an NSTrackingArea if it ever shows up in a profile.
                let h = app.clone();
                std::thread::spawn(move || {
                    let mut over = false;
                    while state(&h).film.load(Ordering::SeqCst) {
                        let hit = *lock(&state(&h).film_hit);
                        let now = matches!((hit, pill::cursor_now(&h)), (Some(r), Some(c)) if r.contains(c));
                        if now != over {
                            over = now;
                            eprintln!("echotype: film card {}", if over { "takes clicks" } else { "lets clicks through" });
                            if let Some(w) = h.get_webview_window("main") {
                                let _ = w.set_ignore_cursor_events(!over);
                            }
                        }
                        std::thread::sleep(Duration::from_millis(33));
                    }
                });
            }
            let _ = w.show();
            let _ = w.set_focus();
        } else {
            // The pill first, over the dots that just landed where it shows (its panel floats
            // above this window); then the stage hides, so it never flashes back into a window.
            st.film.store(false, Ordering::SeqCst);
            let _ = st.pill_wake.send(());
            std::thread::sleep(Duration::from_millis(150));
            e(w.hide())?;
            e(w.set_ignore_cursor_events(false))?;
            e(w.set_always_on_top(false))?;
            // The title bar comes back from tao's own state, applied later on the main thread: resizable
            // first (or the zoom button comes back dimmed), then the overlay style once it has landed
            // (or the traffic lights sit in a strip above the window instead of over it).
            // ponytail: a fixed 150 ms wait for tao's async style mask; hook its completion if it ever races.
            e(w.set_resizable(true))?;
            e(w.set_decorations(true))?;
            std::thread::sleep(Duration::from_millis(150));
            e(w.set_title_bar_style(TitleBarStyle::Overlay))?;
            e(w.set_shadow(true))?;
            e(w.set_effects(WindowEffectsConfig { effects: vec![Effect::Sidebar], state: Some(EffectState::FollowsWindowActiveState), radius: None, color: None, interactive: false }))?;
            e(w.set_min_size(Some(LogicalSize::new(720.0, 520.0))))?;
            e(w.set_size(LogicalSize::new(800.0, 600.0)))?;
            e(w.center())?;
            if show {
                show_main(&app);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (on, show, &w); // the film is macOS-only; elsewhere onboarding keeps its window
    let _ = st.pill_wake.send(());
    Ok(())
}

/// The stage floats over other apps' windows, except while setup needs them in front
/// (Accessibility in System Settings, a real paste in TextEdit).
#[tauri::command]
async fn film_float(app: AppHandle, on: bool) -> R<()> {
    let w = app.get_webview_window("main").ok_or("no settings window")?;
    if state(&app).film.load(Ordering::SeqCst) {
        w.set_always_on_top(on).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The stage's top-left on screen (points). The page can't tell: WebKit's window.screenY counts
/// from the bottom of the screen, so the card's rect and the pill's place go through here.
fn film_origin(app: &AppHandle) -> (f64, f64) {
    app.get_webview_window("main")
        .and_then(|w| Some(w.outer_position().ok()?.to_logical::<f64>(w.scale_factor().ok()?)))
        .map_or((0.0, 0.0), |p| (p.x, p.y))
}

/// The film's card in page px, or None while nothing on the stage takes clicks.
#[tauri::command]
async fn film_hit(app: AppHandle, rect: Option<[f64; 4]>) -> R<()> {
    let (ox, oy) = film_origin(&app);
    *lock(&state(&app).film_hit) = rect.map(|[x, y, w, h]| pill::Rect { x: ox + x, y: oy + y, w, h });
    Ok(())
}

/// Where the film's dots land: the idle ball's centre and radius in the stage's page px.
#[tauri::command]
async fn film_home(app: AppHandle) -> R<Option<[f64; 3]>> {
    let (ox, oy) = film_origin(&app);
    Ok(lock(&pill::HOME).map(|(x, y, r)| [x - ox, y - oy, r]))
}

/// Onboarding's Quit, before there is a pill or a reason to stay in the menu bar.
#[tauri::command]
async fn quit_app(app: AppHandle) -> R<()> {
    app.exit(0);
    Ok(())
}

#[tauri::command]
async fn open_permission_settings(app: AppHandle, kind: String) -> R<()> {
    if !matches!(kind.as_str(), "microphone" | "accessibility" | "input_monitoring" | "keyboard" | "screen_recording") {
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
async fn cloud_sign_in_with(app: AppHandle, provider: String) -> R<serde_json::Value> {
    blocking(move || cloud::sign_in_with(&provider, |url| app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string()))).await
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

fn create_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Echotype", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", "Start/Stop dictation", true, None::<&str>)?;
    let meeting = MenuItem::with_id(app, "meeting", meeting::menu_label(), true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &toggle, &meeting, &quit])?;
    meeting::set_tray_item(meeting);
    // The idle pill is macOS-only, and so is its hide/show item; the pill loop sets its label.
    if cfg!(target_os = "macos") {
        let item = MenuItem::with_id(app, "pill", "Hide pill for 1 hour", true, None::<&str>)?;
        menu.insert_items(&[&item, &PredefinedMenuItem::separator(app)?], 2)?;
        *lock(&state(app.handle()).tray_pill) = Some(item);
    }
    *lock(&state(app.handle()).tray_menu) = Some(menu.clone());
    let mut tray = TrayIconBuilder::with_id("main").tooltip("Echotype").menu(&menu).on_menu_event(|app, e| match e.id.as_ref() {
        "open" => show_main(app),
        "toggle" => send(app, Cmd::Key(Ev::Toggle)),
        "pill" => pill::on_tray(app),
        "meeting" => meeting::toggle(app),
        "quit" => app.exit(0),
        _ => {}
    });
    // Menu-bar mode: a left click dictates (pill.rs moves the menu to the right click).
    tray = tray.on_tray_icon_event(|tray, e| {
        use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e
            && pill::in_menu_bar(&current_settings(tray.app_handle()))
        {
            send(tray.app_handle(), Cmd::Key(Ev::Toggle));
        }
    });
    if cfg!(target_os = "macos") {
        // The dotted globe as a template image, so macOS tints it for light and dark menu bars.
        tray = tray.icon(crate::tray::idle()).icon_as_template(true);
    } else if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    stt::quiet_whisper_logs();
    let builder = tauri::Builder::default();
    #[cfg(target_os = "macos")]
    let builder = builder.plugin(tauri_nspanel::init());
    // On-screen tests (ECHOTYPE_NO_MAIN) run beside the installed app instead of opening its window.
    let builder = if std::env::var_os("ECHOTYPE_NO_MAIN").is_some() { builder } else { builder.plugin(tauri_plugin_single_instance::init(|app, args, _| {
        // `echotype --toggle` from a desktop shortcut: on Wayland the global shortcut only fires in X11 apps.
        // `--toggle-meeting` starts or stops meeting notes (Shortcuts, scripts, tests).
        if args.iter().any(|a| a == "--toggle") {
            send(app, Cmd::Key(Ev::Toggle))
        } else if args.iter().any(|a| a == "--toggle-meeting") {
            meeting::toggle(app)
        } else {
            show_main(app)
        }
    })) };
    let app = builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
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
                            pill::dismiss_card(app);
                        }
                    } else if is(&s.hotkey) {
                        send(app, Cmd::Key(if pressed { Ev::Press } else { Ev::Release }));
                    } else if is(&s.paste_last_hotkey) && !pressed {
                        paste_last(app, true);
                    } else if is(&s.copy_last_hotkey) && !pressed {
                        paste_last(app, false);
                    } else if is(&s.command_hotkey) {
                        send(app, Cmd::Command(if pressed { Ev::Press } else { Ev::Release }));
                    }
                })
                .build(),
        )
        .setup(|app| {
            let s = Settings::load();
            telemetry::install_panic_hook(app.package_info().version.to_string());
            let h = app.handle().clone();
            telemetry::start(app.package_info().version.to_string(), move || current_settings(&h));
            notices::start(app.handle());
            let (pill_wake, woken) = mpsc::channel();
            app.manage(AppState {
                settings: Mutex::new(s.clone()),
                dictation: spawn_dictation(app.handle().clone()),
                api: Mutex::new(None),
                pill: Mutex::new(pill::Pill::default()),
                pill_wake,
                main_key: AtomicBool::new(false),
                main_home: AtomicBool::new(true),
                film: AtomicBool::new(false),
                film_hit: Mutex::new(None),
                clicks: Mutex::new(pill::Debounce::default()),
                drag: Mutex::new(None),
                tray_pill: Mutex::new(None),
                tray_menu: Mutex::new(None),
                last_clip: Mutex::new(None),
                last_cleaned: Mutex::new(None),
                last_output: Mutex::new(None),
                last_app: Mutex::new(None),
                downloads: Mutex::new(HashMap::new()),
                recording: AtomicBool::new(false),
            });
            create_tray(app)?;
            apply_theme(app.handle(), &s);
            let h = app.handle().clone();
            app.listen_any("main-page", move |e| {
                let st = state(&h);
                st.main_home.store(e.payload() == "\"home\"", Ordering::SeqCst);
                let _ = st.pill_wake.send(());
            });
            pill::run(app.handle().clone(), pill::create(app, &s)?, woken);
            for key in shortcuts(&s).iter().filter(|k| !k.is_empty()) {
                if let Err(e) = swap_hotkey(app.handle(), "", key) {
                    eprintln!("echotype: {e}");
                }
            }
            apply_talk_key(app.handle(), &s);
            // F2/E-A6: fn must stop the moment the Globe key acts again or Input Monitoring is
            // revoked, also while no Echotype window is open to trigger a re-check.
            // ponytail: 10 s poll of `defaults` + TCC; a distributed-notification observer if it ever matters.
            let h = app.handle().clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(10));
                let _g = lock(&SAVE);
                let s = current_settings(&h);
                if s.talk_key != "accelerator" {
                    apply_talk_key(&h, &s);
                }
            });
            // Off the main thread: the keychain read can wait on an access prompt (an unsigned
            // rebuild is a new identity to macOS), which must not freeze startup.
            let (handle, api_settings) = (app.handle().clone(), s.clone());
            std::thread::spawn(move || {
                if let Err(e) = apply_local_api(&handle, &api_settings) {
                    eprintln!("echotype: {e}");
                }
            });
            if s.usage_stats {
                let h = app.handle().clone();
                std::thread::spawn(move || match stats::backfill_once() {
                    Ok(true) => {
                        let _ = h.emit_to("main", "stats-updated", ());
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("echotype: could not import history into stats: {e}"),
                });
            }
            if !s.onboarded {
                show_main(app.handle());
            }
            // On-screen checks: the card for this text after 2 s, without dictating or typing.
            // On-screen check of the film's window round trip: the stage, then the settings window again.
            if std::env::var_os("ECHOTYPE_TEST_FILM").is_some() {
                let h = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(2));
                    let _ = tauri::async_runtime::block_on(film_window(h.clone(), true, true));
                    std::thread::sleep(Duration::from_secs(2));
                    let _ = tauri::async_runtime::block_on(film_window(h, false, true));
                });
            }
            if let Some(text) = std::env::var("ECHOTYPE_TEST_CARD").ok().filter(|t| !t.is_empty()) {
                let (h, s) = (app.handle().clone(), s.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(2));
                    eprintln!("echotype: paste target {:?}", platform::paste_target());
                    *lock(&state(&h).last_output) = Some(text.clone());
                    emit_state(&h, card(&s, text, None, None));
                });
            }
            // On-screen checks: listening (with a made-up voice level), thinking, then the card,
            // without a microphone, a key press or a paste.
            if std::env::var_os("ECHOTYPE_TEST_DEMO").is_some() {
                let h = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(2));
                    pill_demo(&h);
                });
            }
            calls::run(app.handle().clone());
            // Old meeting audio goes at start, then daily (Settings → Keep meeting audio).
            // ponytail: thread::sleep stops while the Mac sleeps, so "daily" can stretch; launches catch up.
            let h = app.handle().clone();
            std::thread::spawn(move || loop {
                meeting::prune_audio(current_settings(&h).meeting_audio_days);
                std::thread::sleep(Duration::from_secs(86_400));
            });
            std::thread::spawn(move || {
                if s.stt.provider == "local" {
                    let _ = speakers::ensure_vad(); // for installs from before dictation used it
                    let _ = stt::ensure_lid(); // the fast language picker, same
                }
                stt::warm_up(&s);
                llm::apply(&s);
            });
            Ok(())
        })
        .on_menu_event(|app, e| pill::on_menu(app, e.id.as_ref()))
        .on_window_event(|window, event| match event {
            WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                api.prevent_close();
                let _ = window.hide();
            }
            WindowEvent::Focused(on) if window.label() == "main" => {
                let st = state(window.app_handle());
                st.main_key.store(*on, Ordering::SeqCst);
                let _ = st.pill_wake.send(());
            }
            _ => {}
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
            cancel_download,
            list_input_devices,
            get_history,
            update_history,
            wispr_import_preview,
            wispr_import,
            clear_history,
            get_stats,
            clear_stats,
            transcribe_file,
            get_system_info,
            local_api_status,
            toggle_dictation,
            shortcut_status,
            relaunch,
            retry_last,
            rerun_options,
            rerun,
            check_permissions,
            open_permission_settings,
            request_microphone,
            focus_main,
            last_app,
            installed_apps,
            check_update,
            install_update,
            notices::app_notices,
            mic_check,
            play_sound,
            reset_accessibility,
            quit_app,
            film_window,
            film_hit,
            film_home,
            film_float,
            cloud_start_login,
            cloud_verify,
            cloud_sign_in_with,
            cloud_account,
            cloud_checkout,
            cloud_portal,
            cloud_logout,
            pill_click,
            pill_cancel,
            pill_open,
            pill_menu,
            pill_drag,
            pill_reset,
            pill_copy,
            pill_copy_cleaned,
            preview_sounds,
            show_pill_demo,
            pill_dismiss,
            calls::pill_offer,
            meeting::meeting_start,
            meeting::meeting_stop,
            meeting::meeting_status,
            meeting::meeting_list,
            meeting::meeting_get,
            meeting::meeting_search,
            meeting::meeting_rename_speaker,
            meeting::meeting_delete,
            meeting::meeting_export,
            meeting::meeting_save,
            meeting::meeting_summarize,
            meeting::meeting_ask,
            meeting::meeting_window,
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
            llm::stop();
        }
        let _ = (app, event);
    });
}

/// `echotype --transcribe <file> [--model <ggml path>] [--translate]`: the full pipeline
/// minus paste, headless, with the saved settings. Returns the process exit code.
pub fn cli(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--meeting") {
        return meeting::cli(args);
    }
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
                    s.advanced = true; // a model named here is a manual pick
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
    if s.stt.provider == "local" {
        eprintln!("echotype: speech model {}{}", stt::local_model(&s), if s.advanced { "" } else { " (Automatic)" });
    }
    if llm::wanted(&s) {
        eprintln!("echotype: cleanup model {} ({}, {} tokens)", llm::cleaner(&s).id, llm::memory(&s), llm::context(&s));
    }
    let t = std::time::Instant::now();
    let result = transcribe_path(&s, &file);
    stt::unload();
    llm::stop();
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
    use super::{HistoryEntry, KEEP_SAMPLES, Rerun, Settings, accel_label, as_ran, chip, cli_report, commit, keep, last_words, parse_hotkey, partial_due, stt, unregistered_shortcuts};
    use crate::audio::Quality;
    use std::cell::RefCell;
    use std::time::Duration;

    #[test]
    fn a_partial_runs_from_1_5_s_then_once_a_second() {
        let ms = |n| Some(Duration::from_millis(n));
        assert!(!partial_due(1.4, None), "too little audio for whisper to say anything yet");
        assert!(partial_due(1.5, None), "the first one as soon as there is enough");
        assert!(!partial_due(4.0, ms(999)), "less than a second after the last one started");
        assert!(partial_due(4.0, ms(1000)));
        assert!(partial_due(4.0, ms(2600)), "one that ran long is followed at once, never overlapped");
    }

    #[test]
    fn the_pill_gets_the_last_words_cut_at_a_word() {
        assert_eq!(last_words("ask not what", 24), "ask not what", "short enough: as it is");
        assert_eq!(last_words("my fellow Americans, ask not what your country", 24), "…not what your country", "\"sk\" of a cut word goes");
        assert!(last_words("my fellow Americans, ask not what your country", 24).chars().count() <= 25);
        assert_eq!(last_words("我们今天下午三点开会讨论新的设计方案吧好不好呢真的可以", 10), "…案吧好不好呢真的可以", "no spaces: the last characters");
    }

    /// Under the app manifest a command missing from a capability fails only at runtime, in the
    /// window, so check generate_handler!, build.rs COMMANDS and both capability files agree.
    #[test]
    fn the_last_clip_is_kept_within_its_cap() {
        let five_min = vec![0.0; KEEP_SAMPLES];
        assert!(keep(five_min, false, None).is_some_and(|k| !k.failed), "a success is kept for Re-run");
        assert!(keep(vec![0.0; KEEP_SAMPLES + 1], false, None).is_none(), "but not past five minutes");
        assert!(keep(vec![0.0; KEEP_SAMPLES + 1], true, Some(1)).is_some_and(|k| k.failed && k.entry == Some(1)), "a failure is kept at any length for Retry");
    }

    #[test]
    fn a_rerun_changes_one_setting_for_one_run() {
        let mut s = Settings { languages: vec!["en".into(), "hi".into()], ..Settings::default() };
        s.cleanup.enabled = true;
        let base = s.clone();
        let run = |r: Rerun| {
            let mut t = base.clone();
            r.apply(&mut t);
            t
        };
        assert_eq!(run(Rerun::Lang("hi".into())).languages, ["hi"]);
        assert!(run(Rerun::Lang("auto".into())).languages.is_empty());
        assert!(!run(Rerun::NoCleanup).cleanup.enabled);
        let m = run(Rerun::Model("small".into()));
        assert_eq!((m.stt.provider.as_str(), stt::local_model(&m).as_str(), m.advanced), ("local", "small", false), "the model, without switching on advanced settings");
        assert_eq!(serde_json::to_value(&m).unwrap()["stt"].get("rerun_model"), None, "never saved");
    }

    #[test]
    fn a_history_entry_offers_the_pill_choices_that_would_change_it() {
        let mut s = Settings { advanced: true, ..Settings::default() }; // the settings say Large v3 Turbo
        s.cleanup.enabled = true;
        let entry = |stt: &str, cleanup: Option<&str>| HistoryEntry { ts: 1, raw: "r".into(), text: "t".into(), stt_ms: 1, cleanup_ms: 0, stt_provider: stt.into(), cleanup_provider: cleanup.map(String::from), no_cleanup: false, cleaned: None, original: None };
        let ids = |s: &Settings, e: HistoryEntry| crate::pill::rerun_options(&as_ran(s.clone(), &e), |id| ["large-v3-turbo-q5_0", "tiny.en"].contains(&id)).into_iter().map(|(r, _)| r.id()).collect::<Vec<_>>();
        assert_eq!(ids(&s, entry("local:tiny.en", Some("ollama"))), ["model:large-v3-turbo-q5_0", "raw"], "recorded on Tiny: Turbo is the other model");
        assert_eq!(ids(&s, entry("local:large-v3-turbo-q5_0", None)), ["model:tiny.en"], "no cleanup ran, so none to skip");
        assert_eq!(ids(&s, entry("groq", Some("groq"))), ["model:large-v3-turbo-q5_0", "model:tiny.en", "raw"], "a cloud transcript, on any local model");
        s.languages = vec!["en".into(), "hi".into()];
        assert_eq!(ids(&s, entry("local:tiny.en", None))[..3], ["lang:en", "lang:hi", "lang:auto"], "languages as the pill offers them");
        let ran = as_ran(s.clone(), &entry("local:tiny.en", None));
        assert_eq!(serde_json::to_value(&ran).unwrap(), serde_json::to_value(Settings { cleanup: crate::settings::Cleanup { enabled: false, ..s.cleanup.clone() }, ..s.clone() }).unwrap(), "nothing saved changes");
    }

    #[test]
    fn capabilities_list_every_command_and_the_overlay_only_the_pill() {
        let lib = include_str!("lib.rs");
        let block = lib.split("tauri::generate_handler![").nth(1).unwrap().split(']').next().unwrap();
        let handler: Vec<&str> = block.split(',').map(|c| c.trim().rsplit("::").next().unwrap()).filter(|c| !c.is_empty()).collect();
        let build = include_str!("../build.rs");
        let listed = build.split("const COMMANDS: &[&str] = &[").nth(1).unwrap().split("];").next().unwrap();
        let manifest: Vec<&str> = listed.split(',').map(|c| c.trim().trim_matches('"')).filter(|c| !c.is_empty()).collect();
        assert_eq!(handler, manifest, "build.rs COMMANDS must match generate_handler!");
        let perms = |json: &str| -> Vec<String> {
            let v: serde_json::Value = serde_json::from_str(json).unwrap();
            v["permissions"].as_array().unwrap().iter().filter_map(|p| p.as_str().map(String::from)).collect()
        };
        let (main, overlay) = (perms(include_str!("../capabilities/default.json")), perms(include_str!("../capabilities/overlay.json")));
        for c in &handler {
            let p = format!("allow-{}", c.replace('_', "-"));
            // F3: the pill's commands go to the overlay only, everything else to the settings window only.
            let (to, not) = if c.starts_with("pill_") { (&overlay, &main) } else { (&main, &overlay) };
            assert!(to.contains(&p) && !not.contains(&p), "{c} is in the wrong capability file");
        }
        // Listen only: emitting would let the overlay forge events (drag-drop, navigate) at the settings window.
        assert!(overlay.iter().all(|p| ["core:event:allow-listen", "core:event:allow-unlisten"].contains(&p.as_str()) || p.starts_with("allow-pill-")), "the overlay may only listen and call pill commands: {overlay:?}");
    }

    #[test]
    fn one_chip_wins_too_quiet_then_quiet_voice_then_no_cleanup() {
        // Copied (Accessibility off) + no cleanup + too quiet: the primary line says Copied, the chip says too quiet.
        assert_eq!(chip(Some(Quality::TooQuiet), true), Some("too_quiet"));
        assert_eq!(chip(Some(Quality::Quiet), true), Some("quiet"));
        assert_eq!(chip(None, true), Some("no_cleanup"));
        assert_eq!(chip(None, false), None);
    }

    // Regression: a shortcut taken at launch stayed "unavailable" after the user picked one that worked.
    #[test]
    fn unregistered_shortcuts_are_asked_live_and_ignore_unset_ones() {
        let mut s = Settings { hotkey: "Alt+Space".into(), paste_last_hotkey: "CommandOrControl+Control+V".into(), copy_last_hotkey: String::new(), command_hotkey: "Alt+Shift+Space".into(), ..Settings::default() };
        assert_eq!(unregistered_shortcuts(&s, |k| k != "Alt+Space"), ["hotkey"]);
        s.hotkey = "Alt+KeyD".into();
        assert!(unregistered_shortcuts(&s, |k| k != "Alt+Space").is_empty());
        assert_eq!(unregistered_shortcuts(&s, |_| false), ["hotkey", "paste_last_hotkey", "command_hotkey"]);
    }

    #[test]
    fn shortcut_labels_read_like_keycaps() {
        assert_eq!(accel_label("Alt+Space", true), "⌥ Space");
        assert_eq!(accel_label("CommandOrControl+Control+V", true), "⌘ ⌃ V");
        assert_eq!(accel_label("Control+Alt+KeyK", false), "Ctrl+Alt+K");
    }

    #[test]
    fn cli_prints_nothing_and_exits_zero_when_no_speech_was_found() {
        let out = |text: &str| crate::cleanup::Output { raw: text.into(), text: text.into(), stt_ms: 5, cleanup_ms: 0, warning: None, no_cleanup: true, cleaned: None };
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
    fn the_command_mode_shortcut_is_set_like_the_others() {
        let old = Settings { command_hotkey: "Alt+Shift+Space".into(), ..Settings::default() };
        let mut new = old.clone();
        new.command_hotkey = "Alt+Space".into(); // the dictation shortcut's key
        let (kept, warning) = commit(&old, new.clone(), |_, _| panic!("swapped onto a taken key"), |_| Ok(()), |_| Ok(())).unwrap();
        assert_eq!(kept.command_hotkey, "Alt+Shift+Space");
        assert!(warning.unwrap().contains("already the dictation shortcut"));
        new.command_hotkey = "Control+Alt+KeyE".into();
        let swaps = RefCell::new(vec![]);
        let (kept, _) = commit(&old, new, |o, n| {
            swaps.borrow_mut().push(format!("{o}>{n}"));
            Ok(())
        }, |_| Ok(()), |_| Ok(())).unwrap();
        assert_eq!((kept.command_hotkey.as_str(), swaps.into_inner()), ("Control+Alt+KeyE", vec!["Alt+Shift+Space>Control+Alt+KeyE".to_string()]));
    }

    #[test]
    fn a_shortcut_can_take_a_key_another_one_is_leaving() {
        let mut old = Settings::default();
        old.paste_last_hotkey = "CommandOrControl+Control+V".into();
        let mut new = old.clone();
        new.hotkey = "CommandOrControl+Control+V".into(); // dictation takes paste-last's key
        new.paste_last_hotkey = "CommandOrControl+Control+B".into(); // which moves away in the same save
        let swaps = RefCell::new(vec![]);
        let (kept, warning) = commit(&old, new, |o, n| {
            swaps.borrow_mut().push(format!("{o}>{n}"));
            Ok(())
        }, |_| Ok(()), |_| Ok(())).unwrap();
        assert!(warning.is_none(), "{warning:?}");
        assert_eq!((kept.hotkey.as_str(), kept.paste_last_hotkey.as_str()), ("CommandOrControl+Control+V", "CommandOrControl+Control+B"));
        assert_eq!(swaps.into_inner(), ["CommandOrControl+Control+V>CommandOrControl+Control+B", "Alt+Space>CommandOrControl+Control+V"], "the key is freed before it is taken");
    }

    #[test]
    fn onboarding_pastes_only_from_its_real_app_step_and_never_into_echotype() {
        use super::pastes;
        let mut s = Settings::default();
        assert!(!pastes(&s, false), "steps 1-3: practice lands in onboarding's own field");
        s.onboard_step = 3;
        assert!(!pastes(&s, false));
        s.onboard_step = 4;
        assert!(pastes(&s, false), "step 4 tries a real paste in another app");
        assert!(!pastes(&s, true), "but never into Echotype's own window");
        let done = Settings { onboarded: true, ..Default::default() };
        assert!(pastes(&done, false) && !pastes(&done, true));
    }

    #[test]
    fn errors_from_the_speech_stage_point_at_the_model_page() {
        use super::error_action;
        // The exact messages stt.rs produces; if one is reworded this keeps the window's action.
        for e in ["Choose a speech model", "Not enough free memory for medium: it needs about 1800 MB and 900 MB is free.",
            "medium could not load: bad magic", "Translate to English needs a multilingual model (medium or small); large-v3-turbo cannot translate. Download one under Models."] {
            assert_eq!(error_action(e), Some("model"), "{e}");
        }
        assert_eq!(error_action("Speech-to-text failed (500): boom"), None);
        let mut s = Settings { advanced: true, ..Default::default() };
        s.stt.local_model = "not-a-model".into();
        assert_eq!(error_action(&crate::stt::ready(&s).unwrap_err()), Some("model"));
    }

    #[test]
    fn a_manual_pick_refused_for_memory_offers_automatic_and_automatic_does_not() {
        use super::memory_action;
        let mut s = Settings::default();
        let e = "Not enough free memory for medium: it needs about 1869 MB and 900 MB is free.";
        let w = "Cleanup failed: Echotype model (local): Not enough free memory for the Echotype model: it needs about 1113 MB and 900 MB is free. Pasted the raw transcript instead.";
        assert_eq!((memory_action(&s, e), memory_action(&s, w)), (None, None), "already Automatic");
        s.advanced = true;
        assert_eq!((memory_action(&s, e), memory_action(&s, w)), (Some("automatic"), Some("automatic")));
        assert_eq!(memory_action(&s, "Cleanup failed: Groq: 401"), None);
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
