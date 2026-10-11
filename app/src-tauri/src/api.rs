//! Opt-in local OpenAI-compatible API on 127.0.0.1: POST /v1/audio/transcriptions, GET /v1/models.

use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;
use tauri::AppHandle;

/// Same cap as OpenAI's transcription endpoint.
pub const MAX_BODY: usize = 25 * 1024 * 1024;

#[derive(Clone)]
struct ApiState {
    app: AppHandle,
    token: String,
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

/// Binds now (so "port in use" surfaces immediately) and serves until the task is aborted.
pub fn start(app: AppHandle, port: u16, token: String) -> Result<tauri::async_runtime::JoinHandle<()>, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("Local API could not use port {port}: {e}"))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let router = Router::new()
        .route("/v1/audio/transcriptions", post(transcribe))
        .route("/v1/models", get(models))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(ApiState { app, token });
    Ok(tauri::async_runtime::spawn(async move {
        match tokio::net::TcpListener::from_std(listener) {
            Ok(l) => {
                if let Err(e) = axum::serve(l, router).await {
                    eprintln!("echotype: local API stopped: {e}");
                }
            }
            Err(e) => eprintln!("echotype: local API listener failed: {e}"),
        }
    }))
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
        settings.language = l;
    }
    if let Some(p) = prompt.filter(|p| !p.is_empty()) {
        settings.dictionary.push(p);
    }
    let job = tauri::async_runtime::spawn_blocking(move || {
        let pcm = crate::audio::decode(file, ext.as_deref()).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        crate::cleanup::run(&settings, &pcm, || {}).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))
    });
    match job.await {
        Ok(Ok(out)) if format.as_deref() == Some("text") => out.text.into_response(),
        Ok(Ok(out)) => axum::Json(json!({ "text": out.text })).into_response(),
        Ok(Err((status, e))) => error(status, if status.is_client_error() { "invalid_request_error" } else { "server_error" }, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
