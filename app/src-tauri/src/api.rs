//! Opt-in local OpenAI-compatible API on 127.0.0.1: POST /v1/audio/transcriptions, GET /v1/models.

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;
use std::sync::Arc;
use tauri::AppHandle;
use tokio::sync::Semaphore;

/// Same cap as OpenAI's transcription endpoint.
pub const MAX_BODY: usize = 25 * 1024 * 1024;
/// Transcriptions at once: each can decode up to 60 minutes of audio (~700 MB).
const MAX_JOBS: usize = 2;

#[derive(Clone)]
struct ApiState {
    app: AppHandle,
    token: String,
    jobs: Arc<Semaphore>,
}

pub fn new_token() -> Result<String, String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| e.to_string())?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Authorization: Bearer <token>`, compared in constant time.
pub fn authorized(header: Option<&str>, token: &str) -> bool {
    !token.is_empty() && header.and_then(|h| h.strip_prefix("Bearer ")).is_some_and(|t| ct_eq(t.trim().as_bytes(), token.as_bytes()))
}

fn error(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    (status, axum::Json(json!({ "error": { "message": message.into(), "type": kind, "code": status.as_u16() } }))).into_response()
}

fn check(headers: &HeaderMap, token: &str) -> Result<(), Response> {
    if authorized(headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()), token) {
        Ok(())
    } else {
        Err(error(StatusCode::UNAUTHORIZED, "invalid_request_error", "Missing or invalid bearer token"))
    }
}

pub fn too_large(content_length: Option<&str>) -> bool {
    content_length.and_then(|v| v.parse::<u64>().ok()).is_some_and(|n| n > MAX_BODY as u64)
}

/// Binds now (so "port in use" surfaces immediately) and serves until the returned sender is
/// dropped. Shutdown is graceful: open keep-alive connections close too, not just the listener.
pub fn start(app: AppHandle, port: u16, token: String) -> Result<tokio::sync::oneshot::Sender<()>, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("Local API could not use port {port}: {e}"))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let router = Router::new()
        .route("/v1/audio/transcriptions", post(transcribe))
        .route("/v1/models", get(models))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(ApiState { app, token, jobs: Arc::new(Semaphore::new(MAX_JOBS)) });
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tauri::async_runtime::spawn(async move {
        match tokio::net::TcpListener::from_std(listener) {
            Ok(l) => {
                if let Err(e) = axum::serve(l, router).with_graceful_shutdown(async { let _ = stopped.await; }).await {
                    eprintln!("echotype: local API stopped: {e}");
                }
            }
            Err(e) => eprintln!("echotype: local API listener failed: {e}"),
        }
    });
    Ok(stop)
}

async fn models(State(st): State<ApiState>, headers: HeaderMap) -> Response {
    if let Err(r) = check(&headers, &st.token) {
        return r;
    }
    axum::Json(json!({ "object": "list", "data": [{ "id": "echotype", "object": "model", "owned_by": "echotype" }] })).into_response()
}

async fn transcribe(State(st): State<ApiState>, req: Request) -> Response {
    // Auth and size are checked before a single body byte is read.
    if let Err(r) = check(req.headers(), &st.token) {
        return r;
    }
    if too_large(req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok())) {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "invalid_request_error", "Audio file is larger than 25 MB");
    }
    // Held by the blocking job, so a client that hangs up does not free the slot early.
    let Ok(permit) = st.jobs.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", format!("Echotype is already transcribing {MAX_JOBS} files; try again shortly"));
    };
    let mut mp = match Multipart::from_request(req, &st).await {
        Ok(m) => m,
        Err(e) => return error(e.status(), "invalid_request_error", e.body_text()),
    };
    let (mut file, mut ext, mut language, mut prompt, mut format) = (None, None, None, None, None);
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return error(e.status(), "invalid_request_error", e.body_text()),
        };
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            ext = field.file_name().and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_string());
        }
        let data = match field.bytes().await {
            Ok(d) => d,
            Err(e) => return error(e.status(), "invalid_request_error", e.body_text()),
        };
        let text = || String::from_utf8_lossy(&data).trim().to_string();
        match name.as_str() {
            "file" => file = Some(data.to_vec()),
            "language" => language = Some(text()),
            "prompt" => prompt = Some(text()),
            "response_format" => format = Some(text()),
            _ => {} // model, temperature...: Echotype uses its own settings
        }
    }
    let Some(file) = file else {
        return error(StatusCode::BAD_REQUEST, "invalid_request_error", "Missing multipart field 'file'");
    };
    let mut settings = crate::current_settings(&st.app);
    if let Some(l) = language.filter(|l| !l.is_empty()) {
        // One code, "auto", or up to 3 codes separated by commas ("en,hi").
        settings.languages = l.split(',').map(|c| c.trim().to_string()).collect();
        settings.migrate();
    }
    if let Some(p) = prompt.filter(|p| !p.is_empty()) {
        settings.dictionary.push(p);
    }
    let app = st.app.clone();
    let job = tauri::async_runtime::spawn_blocking(move || {
        let _permit = permit;
        let pcm = crate::audio::decode(file, ext.as_deref()).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        let out = crate::cleanup::run(&settings, &pcm, false, || {}).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        crate::record_stats(&app, &settings, crate::stats::Source::Api, &out.text, 0);
        Ok(out)
    });
    match job.await {
        Ok(Ok(out)) => reply(out, format.as_deref()),
        Ok(Err((status, e))) => error(status, if status.is_client_error() { "invalid_request_error" } else { "server_error" }, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", e.to_string()),
    }
}

/// 200 with the text, also when nothing was said: noise-only audio is `{"text":""}`, not an error.
fn reply(out: crate::cleanup::Output, format: Option<&str>) -> Response {
    if format == Some("text") { out.text.into_response() } else { axum::Json(json!({ "text": out.text })).into_response() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_only_audio_is_an_empty_200_not_an_error() {
        let out = crate::cleanup::Output { raw: String::new(), text: String::new(), stt_ms: 1, cleanup_ms: 0, warning: None, no_cleanup: false, cleaned: None };
        let body = |r: Response| {
            assert_eq!(r.status(), StatusCode::OK);
            tauri::async_runtime::block_on(axum::body::to_bytes(r.into_body(), 1024)).unwrap()
        };
        assert_eq!(&body(reply(out.clone(), None))[..], br#"{"text":""}"#);
        assert_eq!(&body(reply(out, Some("text")))[..], b"");
    }

    #[test]
    fn bearer_auth() {
        let t = new_token().unwrap();
        assert_eq!(t.len(), 64);
        assert_ne!(t, new_token().unwrap());
        assert!(authorized(Some(&format!("Bearer {t}")), &t));
        assert!(!authorized(None, &t));
        assert!(!authorized(Some(&t), &t)); // no scheme
        assert!(!authorized(Some("Bearer "), &t));
        assert!(!authorized(Some(&format!("Bearer {}", &t[..63])), &t));
        assert!(!authorized(Some(&format!("Bearer {t}x")), &t));
        assert!(!authorized(Some("Bearer "), "")); // an empty token never authorizes
    }

    #[test]
    fn upload_cap() {
        assert!(!too_large(None));
        assert!(!too_large(Some("26214400")));
        assert!(too_large(Some("26214401")));
        assert!(!too_large(Some("junk")));
    }
}
