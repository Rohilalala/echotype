mod api;
mod audio;
mod cleanup;
mod platform;
mod settings;
mod stt;

use serde::Serialize;
use settings::{HistoryEntry, Settings};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_opener::OpenerExt;

const MIN_SAMPLES: usize = audio::RATE as usize * 3 / 10; // recordings under 0.3 s are taps
/// A recording this long stops itself and is transcribed as usual (toggle mode left on, a stuck key).
const MAX_RECORDING: Duration = Duration::from_secs(30 * 60);

struct AppState {
    settings: Mutex<Settings>,
    dictation: mpsc::Sender<Cmd>,
    /// Running local API: port, the token it accepts, and the sender whose drop shuts it down.
    api: Mutex<Option<(u16, String, tokio::sync::oneshot::Sender<()>)>>,
    overlay_gen: AtomicU64,
    /// The last recording whose transcription failed, kept so `retry_last` can run it again.
    // ponytail: only the last one, in memory; a failed-recordings folder if users need more than one retry.
    last_failed: Mutex<Option<Vec<f32>>>,
}

enum Cmd {
    Press,
    Release,
    Toggle,
    Cancel,
    Retry,
    /// Sent by recording `n` when it reaches MAX_RECORDING.
    AutoStop(u64),
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
}

fn state(app: &AppHandle) -> tauri::State<'_, AppState> {
    app.state::<AppState>()
}

pub(crate) fn current_settings(app: &AppHandle) -> Settings {
    state(app).settings.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

// ---------- dictation ----------

fn emit_state(app: &AppHandle, s: DictationState) {
    let busy = matches!(s.state, "recording" | "transcribing" | "cleaning");
    let _ = app.emit("dictation-state", s);
    let st = state(app);
    let generation = st.overlay_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let Some(overlay) = app.get_webview_window("overlay") else { return };
    if busy {
        let _ = overlay.show();
    } else {
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1200));
            if state(&app).overlay_gen.load(Ordering::SeqCst) == generation {
                let _ = overlay.hide();
            }
        });
    }
}

fn simple(state: &'static str) -> DictationState {
    DictationState { state, ..Default::default() }
}

fn escape() -> Shortcut {
    Shortcut::new(None, Code::Escape)
}

/// One thread owns the recorder so press/release/toggle/cancel are handled strictly in order.
fn spawn_dictation(app: AppHandle) -> mpsc::Sender<Cmd> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    std::thread::spawn(move || {
        let busy = Arc::new(AtomicBool::new(false));
        let mut rec: Option<audio::Recorder> = None;
        let mut rec_id = 0u64;
        for cmd in rx {
            let toggle_mode = current_settings(&app).mode == "toggle";
            match (cmd, rec.is_some()) {
                (Cmd::Cancel, true) => {
                    rec.take().expect("checked").stop();
                    let _ = app.global_shortcut().unregister(escape());
                    emit_state(&app, simple("idle"));
                }
                (Cmd::Toggle, true) => finish(&app, &mut rec, &busy),
                (Cmd::AutoStop(id), true) if id == rec_id => finish(&app, &mut rec, &busy),
                (Cmd::Press, true) if toggle_mode => finish(&app, &mut rec, &busy),
                (Cmd::Release, true) if !toggle_mode => finish(&app, &mut rec, &busy),
                (Cmd::Retry, false) if !busy.load(Ordering::SeqCst) => {
                    let pcm = state(&app).last_failed.lock().unwrap_or_else(|e| e.into_inner()).take();
                    if let Some(pcm) = pcm {
                        process(&app, pcm, busy.clone());
                    }
                }
                (Cmd::Press | Cmd::Toggle, false) if !busy.load(Ordering::SeqCst) => {
                    rec_id += 1;
                    let (a, id, started, sent) = (app.clone(), rec_id, std::time::Instant::now(), AtomicBool::new(false));
                    match audio::Recorder::start(current_settings(&app).input_device, move |level| {
                        let _ = a.emit("audio-level", serde_json::json!({ "level": level }));
                        if started.elapsed() >= MAX_RECORDING && !sent.swap(true, Ordering::SeqCst) {
                            let _ = state(&a).dictation.send(Cmd::AutoStop(id));
                        }
                    }) {
                        Ok(r) => {
                            rec = Some(r);
                            let _ = app.global_shortcut().register(escape());
                            emit_state(&app, simple("recording"));
                        }
                        Err(e) => emit_state(&app, DictationState { state: "error", error: Some(e), ..Default::default() }),
                    }
                }
                _ => {}
            }
        }
    });
    tx
}

fn finish(app: &AppHandle, rec: &mut Option<audio::Recorder>, busy: &Arc<AtomicBool>) {
    let pcm = rec.take().map(audio::Recorder::stop).unwrap_or_default();
    let _ = app.global_shortcut().unregister(escape());
    process(app, pcm, busy.clone());
}

/// Clears the busy flag when dropped, so even a panicking worker can't leave dictation stuck.
struct Busy(Arc<AtomicBool>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// STT + cleanup + paste + history on a worker thread, so the hotkey stays responsive.
fn process(app: &AppHandle, pcm: Vec<f32>, busy: Arc<AtomicBool>) {
    if pcm.len() < MIN_SAMPLES {
        emit_state(app, simple("idle"));
        return;
    }
    busy.store(true, Ordering::SeqCst);
    let app = app.clone();
    std::thread::spawn(move || {
        let guard = Busy(busy);
        let s = current_settings(&app);
        emit_state(&app, simple("transcribing"));
        // A panic in a decoder, whisper-rs or the HTTP stack becomes an error state, not a silent loss.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cleanup::run(&s, &pcm, || emit_state(&app, simple("cleaning")))))
            .unwrap_or_else(|_| Err("Transcription crashed unexpectedly".into()));
        let failed = result.is_err();
        // Keep the audio of a failed transcription for retry_last; a success frees the old one.
        *state(&app).last_failed.lock().unwrap_or_else(|e| e.into_inner()) = failed.then_some(pcm);
        let ev = match result {
            Err(e) => DictationState { state: "error", error: Some(e), retry: true, ..Default::default() },
            Ok(out) if out.text.trim().is_empty() => simple("idle"),
            Ok(out) => {
                record_history(&s, &out);
                match platform::paste(&app, &out.text, s.restore_clipboard) {
                    Ok(()) => DictationState { state: "done", text: Some(out.text), warning: out.warning, ..Default::default() },
                    Err(e) => DictationState { state: "error", text: Some(out.text), error: Some(e), warning: out.warning, ..Default::default() },
                }
            }
        };
        drop(guard);
        emit_state(&app, ev);
    });
}

fn record_history(s: &Settings, out: &cleanup::Output) {
    if !s.history_enabled {
        return;
    }
    let cleaned = cleanup::wants_cleanup(s, &out.raw);
    let entry = HistoryEntry {
        ts: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
        raw: out.raw.clone(),
        text: out.text.clone(),
        stt_ms: out.stt_ms,
        cleanup_ms: out.cleanup_ms,
        stt_provider: if s.stt.provider == "local" { format!("local:{}", stt::model_id(&s.stt.local_model)) } else { s.stt.provider.clone() },
        cleanup_provider: cleaned.then(|| s.cleanup.provider.clone()),
    };
    if let Err(e) = settings::append_history(&entry) {
        eprintln!("echotype: could not write history: {e}");
    }
}

// ---------- hotkey + local API wiring ----------

fn parse_hotkey(s: &str) -> Result<Shortcut, String> {
    s.parse::<Shortcut>().map_err(|e| format!("Invalid hotkey '{s}': {e}"))
}

fn swap_hotkey(app: &AppHandle, old: Option<&str>, new: &str) -> Result<(), String> {
    let new_sc = parse_hotkey(new)?;
    let gs = app.global_shortcut();
    if gs.is_registered(new_sc) {
        return Ok(());
    }
    if let Some(old_sc) = old.and_then(|o| parse_hotkey(o).ok()) {
        let _ = gs.unregister(old_sc);
    }
    gs.register(new_sc).map_err(|e| {
        if let Some(o) = old.and_then(|o| parse_hotkey(o).ok()) {
            let _ = gs.register(o);
        }
        format!("Could not register hotkey '{new}': {e}")
    })
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

/// Saves `new`, keeping the hotkey registration in step with what is on disk. The hotkey is
/// swapped only when it changed. A key that cannot be registered never blocks the rest: the
/// other settings are saved with the old key and the reason comes back as a warning. A failed
/// save swaps the key back, so the registration, the file and memory never disagree.
fn commit(old: &Settings, mut new: Settings, swap: impl Fn(&str, &str) -> R<()>, save: impl Fn(&Settings) -> R<()>) -> R<(Settings, Option<String>)> {
    let mut warning = None;
    let swapped = new.hotkey != old.hotkey
        && match swap(&old.hotkey, &new.hotkey) {
            Ok(()) => true,
            Err(e) => {
                warning = Some(format!("{e}. Your other settings were saved."));
                new.hotkey = old.hotkey.clone();
                false
            }
        };
    if let Err(e) = save(&new) {
        if swapped {
            let _ = swap(&new.hotkey, &old.hotkey);
        }
        return Err(e);
    }
    Ok((new, warning))
}

#[tauri::command]
async fn save_settings(app: AppHandle, settings: Settings) -> R<()> {
    if settings.local_api.port < settings::MIN_PORT {
        return Err(format!("The local API port must be between {} and 65535", settings::MIN_PORT));
    }
    // One save at a time: hotkey swap, file write, in-memory update and API restart happen together.
    static SAVE: Mutex<()> = Mutex::new(());
    let _g = SAVE.lock().unwrap_or_else(|e| e.into_inner());
    let old = current_settings(&app);
    let (new, warning) = commit(&old, settings, |o, n| swap_hotkey(&app, Some(o), n), Settings::save)?;
    *state(&app).settings.lock().unwrap_or_else(|e| e.into_inner()) = new.clone();
    apply_local_api(&app, &new)?;
    if (new.stt.local_model != old.stt.local_model || new.stt.provider != old.stt.provider) && new.stt.provider == "local" {
        std::thread::spawn(move || stt::warm_up(&new));
    }
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
async fn list_input_devices() -> R<Vec<String>> {
    Ok(audio::input_devices())
}

#[tauri::command]
async fn get_history(limit: Option<usize>) -> R<Vec<HistoryEntry>> {
    Ok(settings::get_history(limit.unwrap_or(100)))
}

#[tauri::command]
async fn clear_history() -> R<()> {
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
}

#[tauri::command]
async fn get_system_info() -> R<SystemInfo> {
    let (total, avail) = stt::ram_mb();
    let gb = |mb: u64| (mb as f64 / 1024.0 * 10.0).round() / 10.0;
    Ok(SystemInfo { total_ram_gb: gb(total), available_ram_gb: gb(avail), os: std::env::consts::OS, arch: std::env::consts::ARCH })
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
    state(&app).dictation.send(Cmd::Toggle).map_err(|e| e.to_string())
}

/// Runs the last failed recording through STT, cleanup and paste again.
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
    if !matches!(kind.as_str(), "microphone" | "accessibility") {
        return Err(format!("Unknown permission '{kind}'"));
    }
    let k = kind.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || platform::request(&k)).await;
    if let Some(url) = platform::settings_url(&kind) {
        app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------- app ----------

fn create_overlay(app: &tauri::App) -> tauri::Result<()> {
    let (w, h) = (220.0, 56.0);
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
        overlay.set_position(tauri::LogicalPosition::new(pos.x + (size.width - w) / 2.0, pos.y + size.height - h - 96.0))?;
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
        "toggle" => {
            let _ = state(app).dictation.send(Cmd::Toggle);
        }
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
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, sc, ev| {
                    let st = state(app);
                    let cmd = if sc.id() == escape().id() {
                        (ev.state == ShortcutState::Pressed).then_some(Cmd::Cancel)
                    } else if parse_hotkey(&current_settings(app).hotkey).is_ok_and(|h| h.id() == sc.id()) {
                        Some(if ev.state == ShortcutState::Pressed { Cmd::Press } else { Cmd::Release })
                    } else {
                        None
                    };
                    if let Some(c) = cmd {
                        let _ = st.dictation.send(c);
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
            });
            create_overlay(app)?;
            create_tray(app)?;
            if let Err(e) = swap_hotkey(app.handle(), None, &s.hotkey) {
                eprintln!("echotype: {e}");
            }
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
            retry_last,
            check_permissions,
            open_permission_settings,
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
    match result {
        Ok(out) => {
            println!("{}", out.text);
            if let Some(w) = &out.warning {
                eprintln!("warning: {w}");
            }
            eprintln!("stt {} ms, cleanup {} ms, total {} ms", out.stt_ms, out.cleanup_ms, t.elapsed().as_millis());
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Settings, commit, parse_hotkey};
    use std::cell::RefCell;

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
        let (kept, warning) = commit(&old, new, |_, _| panic!("swapped an unchanged hotkey"), |s| Ok(*saved.borrow_mut() = Some(s.clone()))).unwrap();
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
        let (kept, warning) = commit(&old, new, |_, n| Err(format!("Could not register hotkey '{n}'")), |s| Ok(*saved.borrow_mut() = Some(s.clone()))).unwrap();
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
        let err = commit(&old, new, |o, n| Ok(swaps.borrow_mut().push((o.to_string(), n.to_string()))), |_| Err("disk full".into())).unwrap_err();
        assert_eq!(err, "disk full");
        let k = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(swaps.into_inner(), vec![k("Alt+Space", "Control+Alt+KeyK"), k("Control+Alt+KeyK", "Alt+Space")]);
    }
}
