//! The built-in cleanup model: llama.cpp's llama-server, shipped inside the app as a sidecar
//! (bundle.externalBin, built by scripts/build-llama-server.sh), runs the Echotype GGUF on
//! 127.0.0.1 behind a random key. It starts on the first cleanup (or as soon as the provider is
//! chosen), starts again if it died, and stops when the provider changes away or Echotype exits.
//! A separate process because whisper-rs already links its own ggml and a second copy collides.

use crate::settings::Settings;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Q4_K_M of the fine-tuned Qwen2.5-1.5B: same exact-match score as the MLX 4-bit build on
/// model/eval/handwritten.jsonl (70.7%), ~100 ms a dictation on Apple silicon.
pub const MODEL_FILE: &str = "echotype-clean-q4_k_m.gguf";
pub const MODEL_MB: u64 = 940;
// ponytail: not downloadable yet. Once the GGUF is published at a pinned Hugging Face revision
// (like stt::HF_COMMIT), add its URL and fetch it with stt::fetch(url, &model_path(), sha256, ..)
// the way stt::download does; its SHA-256 is
// 95481186c2dc0197833e3c4e5759c09f30a29f172c4035b3602666651a73b798
/// Resident once loaded (weights, 2k context, Metal buffers), measured at ~1.1-1.2 GB.
const RAM_MB: u64 = 1300;
const START_TIMEOUT: Duration = Duration::from_secs(60);

pub fn model_path() -> PathBuf {
    crate::stt::models_dir().join(MODEL_FILE)
}

/// (installed, size in MB) for the Cleanup page: the file's size, or the size it will be.
pub fn status() -> (bool, u64) {
    std::fs::metadata(model_path()).ok().filter(|m| m.is_file()).map_or((false, MODEL_MB), |m| (true, m.len() / 1_048_576))
}

/// Whether these settings use the built-in model, so its server should be running.
pub fn wanted(s: &Settings) -> bool {
    s.cleanup.enabled && s.cleanup.provider == "echotype"
}

/// After a settings change, or at startup: start the server ahead of the first dictation, or free its memory.
pub fn apply(s: &Settings) {
    if !wanted(s) {
        return stop();
    }
    let t = Instant::now();
    match ensure() {
        Ok(_) => eprintln!("echotype: cleanup model ready in {} ms", t.elapsed().as_millis()),
        Err(e) => eprintln!("echotype: cleanup model not started: {e}"),
    }
}

struct Server {
    child: Child,
    port: u16,
    key: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ponytail: one server, one request at a time (-np 1); cleanup is one short request per dictation.
static SERVER: Mutex<Option<Server>> = Mutex::new(None);

/// Base URL and bearer key of the running server, starting it first if it is not running.
pub fn ensure() -> Result<(String, String), String> {
    let mut g = SERVER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = g.as_mut()
        && matches!(s.child.try_wait(), Ok(None))
    {
        return Ok((url(s.port), s.key.clone()));
    }
    *g = None; // died (or never started): reap it before starting another
    let model = model_path();
    let bin = sidecar();
    preflight(bin.as_deref(), &model, crate::stt::ram_mb().1)?;
    let s = start(&bin.expect("checked by preflight"), &model, &crate::settings::data_dir().join("llama-server.log"))?;
    let out = (url(s.port), s.key.clone());
    *g = Some(s);
    Ok(out)
}

/// Stops the server; it starts again on the next cleanup. Also called on exit.
pub fn stop() {
    *SERVER.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Next to Echotype's own executable: Contents/MacOS/ in the app bundle, target/<profile>/ in a
/// cargo build (tauri-build copies it there), one level up from a test binary in target/<profile>/deps/.
fn sidecar() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors().skip(1).take(2).map(|d| d.join("llama-server")).find(|p| p.is_file())
}

fn preflight(sidecar: Option<&Path>, model: &Path, free_mb: u64) -> Result<(), String> {
    if !model.is_file() {
        return Err(format!("The Echotype model isn't installed. Put {MODEL_FILE} in {} or pick another cleanup provider", model.parent().unwrap_or(model).display()));
    }
    if sidecar.is_none() {
        return Err("The Echotype model runner (llama-server) is missing from this copy of Echotype. Reinstall Echotype".into());
    }
    if RAM_MB > free_mb {
        return Err(format!("Not enough free memory for the Echotype model: it needs about {RAM_MB} MB and {free_mb} MB is free. Close some apps or pick another cleanup provider"));
    }
    Ok(())
}

fn url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

/// llama-server flags for llama.cpp b11619. The GGUF carries Qwen's chat template, used as is.
// ponytail: 2048 tokens of context fits a few minutes of dictation plus its cleanup; longer ones
// fail over to the raw transcript. Raise -c (~28 KB of KV cache a token) if that shows up.
fn args(model: &Path, port: u16) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec!["-m".into(), model.into()];
    for s in ["--host", "127.0.0.1", "--port", &port.to_string(), "-c", "2048", "-ngl", "99", "-np", "1", "--no-webui", "--offline"] {
        a.push(s.into());
    }
    a
}

/// A free localhost port. ponytail: another process could take it before the server binds; that
/// start then fails and the next cleanup picks a new port.
fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free local port: {e}"))?;
    l.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

fn start(bin: &Path, model: &Path, log: &Path) -> Result<Server, String> {
    let (port, key) = (free_port()?, crate::api::new_token()?);
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let stderr = std::fs::File::create(log).map(Stdio::from).unwrap_or_else(|_| Stdio::null());
    let child = Command::new(bin)
        .args(args(model, port))
        .env("LLAMA_API_KEY", &key) // in the environment, not argv, so `ps` never shows it
        // The pinned llama-server's router-child mode: it exits when its stdin closes, so it can
        // never outlive Echotype, even after a crash or kill -9. We hold the pipe and never write to it.
        .env("LLAMA_SERVER_ROUTER_PORT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|e| format!("The Echotype model could not start: {e}"))?;
    let mut s = Server { child, port, key };
    wait_ready(&mut s, START_TIMEOUT).map_err(|e| format!("The Echotype model did not start: {e} (see {})", log.display()))?;
    Ok(s) // on error `s` drops above, which kills the process
}

/// Polls GET /health (503 while the model loads, 200 once it answers) until ready, dead or timed out.
fn wait_ready(s: &mut Server, timeout: Duration) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(2)).build().map_err(|e| e.to_string())?;
    let health = format!("http://127.0.0.1:{}/health", s.port);
    let t = Instant::now();
    loop {
        if let Ok(Some(status)) = s.child.try_wait() {
            return Err(format!("it exited ({status})"));
        }
        if client.get(&health).send().is_ok_and(|r| r.status().is_success()) {
            return Ok(());
        }
        if t.elapsed() > timeout {
            return Err(format!("it was not ready after {} s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_args_bind_localhost_on_the_given_port() {
        let a = args(Path::new("/m/echotype-clean-q4_k_m.gguf"), 4321);
        let want = ["-m", "/m/echotype-clean-q4_k_m.gguf", "--host", "127.0.0.1", "--port", "4321", "-c", "2048", "-ngl", "99", "-np", "1", "--no-webui", "--offline"];
        assert_eq!(a, want.map(OsString::from));
        assert!(!a.iter().any(|x| x.to_string_lossy().contains("api-key")), "the key goes in the environment, never argv");
        assert_eq!(url(4321), "http://127.0.0.1:4321/v1");
    }

    #[test]
    fn free_port_is_bindable() {
        let p = free_port().unwrap();
        assert!(p > 0);
        std::net::TcpListener::bind(("127.0.0.1", p)).unwrap();
    }

    #[test]
    fn preflight_says_what_is_missing_first() {
        let dir = std::env::temp_dir().join(format!("echotype-llm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (model, bin) = (dir.join(MODEL_FILE), dir.join("llama-server"));
        let err = preflight(Some(&bin), &model, 64_000).unwrap_err();
        assert!(err.starts_with("The Echotype model isn't installed. Put echotype-clean-q4_k_m.gguf in "), "{err}");
        std::fs::write(&model, b"x").unwrap();
        assert!(preflight(None, &model, 64_000).unwrap_err().contains("runner (llama-server) is missing"));
        let err = preflight(Some(&bin), &model, 1000).unwrap_err();
        assert!(err.contains("needs about 1300 MB and 1000 MB is free"), "{err}");
        assert!(preflight(Some(&bin), &model, RAM_MB).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_enabled_echotype_cleanup_wants_the_server() {
        let mut s = Settings::default();
        s.cleanup.provider = "echotype".into();
        assert!(!wanted(&s), "cleanup off");
        s.cleanup.enabled = true;
        assert!(wanted(&s));
        s.cleanup.provider = "ollama".into();
        assert!(!wanted(&s));
    }

    /// The real sidecar and model: `cargo test -- --ignored llm` after scripts/build-llama-server.sh,
    /// with the GGUF in the models dir. Starts the server, cleans one transcript, stops it.
    #[test]
    #[ignore]
    fn real_sidecar_cleans_a_transcript() {
        if !model_path().is_file() {
            eprintln!("skipped: no {}", model_path().display());
            return;
        }
        let mut s = Settings::default();
        s.cleanup.enabled = true;
        s.cleanup.provider = "echotype".into();
        s.dictionary = vec!["Arnav".into()];
        let out = crate::cleanup::cleanup(&s, "um so arnav said the meeting is at uh five").unwrap();
        assert!(out.contains("Arnav") && !out.contains("um ") && !out.contains("uh"), "{out}");
        assert_eq!(crate::cleanup::cleanup(&s, "um").unwrap(), "", "filler only is no speech");
        let (base, _) = ensure().unwrap();
        let unauthorized = reqwest::blocking::Client::new().post(format!("{base}/chat/completions")).json(&crate::cleanup::request_body("x", "s", "t", false)).send().unwrap();
        assert_eq!(unauthorized.status(), 401, "other local processes cannot use it without the key");
        let pid = || SERVER.lock().unwrap().as_ref().unwrap().child.id();
        let kill = |sig: &str, pid: u32| Command::new("kill").args([sig, &pid.to_string()]).stderr(Stdio::null()).status().unwrap().success();
        let first = pid();
        assert!(kill("-9", first));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(crate::cleanup::cleanup(&s, "um").unwrap(), "", "a dead server is started again on the next cleanup");
        let second = pid();
        assert_ne!(first, second);
        stop();
        assert!(!kill("-0", second), "stop() ended the server");
    }
}
