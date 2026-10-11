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

struct AppState {
    settings: Mutex<Settings>,
    dictation: mpsc::Sender<Cmd>,
    api: Mutex<Option<(u16, tauri::async_runtime::JoinHandle<()>)>>,
    overlay_gen: AtomicU64,
}

enum Cmd {
    Press,
    Release,
    Toggle,
    Cancel,
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
        for cmd in rx {
            let toggle_mode = current_settings(&app).mode == "toggle";
            match (cmd, rec.is_some()) {
                (Cmd::Cancel, true) => {
                    rec.take().expect("checked").stop();
                    let _ = app.global_shortcut().unregister(escape());
                    emit_state(&app, simple("idle"));
                }
                (Cmd::Toggle, true) => finish(&app, &mut rec, &busy),
                (Cmd::Press, true) if toggle_mode => finish(&app, &mut rec, &busy),
                (Cmd::Release, true) if !toggle_mode => finish(&app, &mut rec, &busy),
                (Cmd::Press | Cmd::Toggle, false) if !busy.load(Ordering::SeqCst) => {
                    let a = app.clone();
                    match audio::Recorder::start(current_settings(&app).input_device, move |level| {
                        let _ = a.emit("audio-level", serde_json::json!({ "level": level }));
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

/// STT + cleanup + paste + history on a worker thread, so the hotkey stays responsive.
fn process(app: &AppHandle, pcm: Vec<f32>, busy: Arc<AtomicBool>) {
    if pcm.len() < MIN_SAMPLES {
        emit_state(app, simple("idle"));
        return;
    }
    busy.store(true, Ordering::SeqCst);
    let app = app.clone();
    std::thread::spawn(move || {
        let s = current_settings(&app);
        emit_state(&app, simple("transcribing"));
        let result = cleanup::run(&s, &pcm, || emit_state(&app, simple("cleaning")));
        let ev = match result {
            Ok(out) if out.text.trim().is_empty() => simple("idle"),
            Ok(out) => {
                record_history(&s, &out);
                match platform::paste(&app, &out.text, s.restore_clipboard) {
                    Ok(()) => DictationState { state: "done", text: Some(out.text), warning: out.warning, ..Default::default() },
                    Err(e) => DictationState { state: "error", text: Some(out.text), error: Some(e), warning: out.warning },
                }
            }
            Err(e) => DictationState { state: "error", error: Some(e), ..Default::default() },
        };
        busy.store(false, Ordering::SeqCst);
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
    let running_port = api.as_ref().map(|(p, _)| *p);
    if s.local_api.enabled && running_port == Some(s.local_api.port) {
        return Ok(());
    }
    if let Some((_, handle)) = api.take() {
        handle.abort();
    }
    if s.local_api.enabled {
        let token = local_api_token()?;
        *api = Some((s.local_api.port, api::start(app.clone(), s.local_api.port, token)?));
    }
    Ok(())
}

fn local_api_token() -> Result<String, String> {
    if let Some(t) = settings::get_secret("local_api") {
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

#[tauri::command]
async fn save_settings(app: AppHandle, settings: Settings) -> R<()> {
    let old = current_settings(&app);
    swap_hotkey(&app, Some(&old.hotkey), &settings.hotkey)?;
    settings.save()?;
    *state(&app).settings.lock().unwrap_or_else(|e| e.into_inner()) = settings.clone();
    if (settings.stt.local_model != old.stt.local_model || settings.stt.provider != old.stt.provider) && settings.stt.provider == "local" {
        std::thread::spawn(move || stt::warm_up(&settings));
    }
    apply_local_api(&app, &current_settings(&app))
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
    let token = if s.local_api.enabled { Some(local_api_token()?) } else { settings::get_secret("local_api") };
    Ok(serde_json::json!({ "enabled": s.local_api.enabled, "port": s.local_api.port, "token": token }))
}

#[tauri::command]
async fn toggle_dictation(app: AppHandle) -> R<()> {
    state(&app).dictation.send(Cmd::Toggle).map_err(|e| e.to_string())
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
            });
            create_overlay(app)?;
            create_tray(app)?;
            if let Err(e) = swap_hotkey(app.handle(), None, &s.hotkey) {
                eprintln!("echotype: {e}");
            }
            if let Err(e) = apply_local_api(app.handle(), &s) {
                eprintln!("echotype: {e}");
            }
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
    use super::parse_hotkey;

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
}
