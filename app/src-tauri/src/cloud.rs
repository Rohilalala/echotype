//! Echotype Cloud account (server/ in this repo): sign-in through Supabase (Google, Apple or an
//! emailed code), usage, Stripe checkout and portal. Supabase vouches for the email; the server
//! then issues its own API key, which lives only in the keychain as `cloud`; the UI never sees it.

use crate::settings;
use base64::Engine;
use reqwest::Method;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::time::{Duration, Instant};

/// The Supabase project that signs people in. The anon key is public by design: it only lets
/// people sign in, and the project exposes no tables. ECHOTYPE_SUPABASE_URL/_ANON_KEY override both (tests).
const SUPABASE_URL: &str = "https://njozixvvvygsbawbyets.supabase.co";
const SUPABASE_ANON_KEY: &str = "sb_publishable_12buu7IYCStk5rVbl_RCFA_8TBKafr7";

const SECRET: &str = "cloud";

/// Moves on with every sign-in, Cancel and Sign out, so an older browser sign-in that is still
/// waiting (or still finishing) stops instead of storing its key over the one the user has now.
static ATTEMPT: AtomicU64 = AtomicU64::new(0);
const CANCELLED: &str = "Sign-in cancelled.";

/// Ends any browser sign-in still in progress; returns the number of the attempt that starts now.
pub fn cancel() -> u64 {
    ATTEMPT.fetch_add(1, SeqCst) + 1
}
/// The error every command returns when there is no usable key. The UI matches on it.
pub const SIGNED_OUT: &str = "signed out";

/// The server's own message from its OpenAI-shaped error body. Those messages are written for
/// people ("That code is wrong or has expired.", the 402 quota text), so they pass through.
pub fn error_message(status: u16, body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    if v["error"]["type"] == "invalid_api_key" {
        return "You are signed out of Echotype Cloud. Sign in again on the Speech model page.".into();
    }
    match v["error"]["message"].as_str().map(str::trim) {
        Some(m) if !m.is_empty() => m.chars().take(300).collect(),
        _ => format!("Echotype Cloud answered with an error ({status}). Try again in a minute."),
    }
}

/// Returns the JSON body (Null for 204), or (status, readable message). Status 0 = no answer.
fn call(method: Method, path: &str, key: Option<&str>, body: Option<Value>) -> Result<Value, (u16, String)> {
    let base = settings::base_url("cloud", "").map_err(|e| (0, e))?;
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).build().map_err(|e| (0, e.to_string()))?;
    let mut req = client.request(method, format!("{base}{path}"));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().map_err(|e| (0, format!("Could not reach Echotype Cloud: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp.text().unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err((status, error_message(status, &text)));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|_| (status, "Echotype Cloud sent a response Echotype could not read.".into()))
}

/// A call with the stored key. No key, or a key the server rejects (401), is SIGNED_OUT,
/// and a rejected key is removed so the app stops sending it.
fn authed(method: Method, path: &str, body: Option<Value>) -> Result<Value, String> {
    let key = settings::secret(SECRET)?.ok_or(SIGNED_OUT)?;
    match call(method, path, Some(&key), body) {
        Err((401, _)) => {
            settings::delete_secret(SECRET)?;
            Err(SIGNED_OUT.into())
        }
        res => res.map_err(|(_, e)| e),
    }
}

fn supabase_url() -> String {
    std::env::var("ECHOTYPE_SUPABASE_URL").unwrap_or_else(|_| SUPABASE_URL.into()).trim_end_matches('/').to_string()
}

/// POST to Supabase Auth (`path` under /auth/v1). Its errors read like "Token has expired or is
/// invalid", so a 4xx passes its message through.
fn supabase(path: &str, body: Value) -> Result<Value, String> {
    let key = std::env::var("ECHOTYPE_SUPABASE_ANON_KEY").unwrap_or_else(|_| SUPABASE_ANON_KEY.into());
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).build().map_err(|e| e.to_string())?;
    let resp = client.post(format!("{}/auth/v1{path}", supabase_url())).header("apikey", key).json(&body).send().map_err(|e| format!("Could not reach the sign-in service: {e}"))?;
    let status = resp.status().as_u16();
    let v: Value = resp.json().unwrap_or_default();
    if (200..300).contains(&status) {
        return Ok(v);
    }
    let msg = [&v["msg"], &v["error_description"], &v["message"]].into_iter().find_map(|m| m.as_str()).unwrap_or("").trim().to_string();
    Err(match status {
        429 => "Too many sign-in attempts. Wait a minute and try again.".into(),
        400..=499 if !msg.is_empty() => msg,
        _ => format!("The sign-in service answered with an error ({status}). Try again in a minute."),
    })
}

/// Trades a Supabase session for an Echotype Cloud key, stores the key, and returns `{email, plan}` (never the key).
fn redeem(session: &Value, attempt: u64) -> Result<Value, String> {
    let token = session["access_token"].as_str().filter(|t| !t.is_empty()).ok_or("The sign-in service sent no session. Try again.")?;
    let v = call(Method::POST, "/auth/supabase", None, Some(json!({ "access_token": token }))).map_err(|(_, e)| e)?;
    let key = v["api_key"].as_str().filter(|k| !k.is_empty()).ok_or("Echotype Cloud did not return a key. Try signing in again.")?;
    if ATTEMPT.load(SeqCst) != attempt {
        return Err(CANCELLED.into());
    }
    settings::set_secret(SECRET, key)?;
    Ok(json!({ "email": v["email"], "plan": v["plan"] }))
}

/// Emails a 6-digit code (Supabase's "Magic Link" template sends {{ .Token }}).
pub fn start_login(email: &str) -> Result<(), String> {
    supabase("/otp", json!({ "email": email.trim(), "create_user": true })).map(drop)
}

/// Checks the emailed code and signs in.
pub fn verify(email: &str, code: &str) -> Result<Value, String> {
    let attempt = cancel();
    redeem(&supabase("/verify", json!({ "type": "email", "email": email.trim(), "token": code.trim() }))?, attempt)
}

/// What a browser sign-in waits for before giving up.
const BROWSER_WAIT: Duration = Duration::from_secs(300);

/// Google or Apple through the browser: Supabase redirects back to a one-off listener on
/// 127.0.0.1, and PKCE ties the returned code to this app, so a code caught by anyone else is useless.
/// `open` opens the sign-in page.
pub fn sign_in_with(provider: &str, open: impl FnOnce(String) -> Result<(), String>) -> Result<Value, String> {
    if !matches!(provider, "google" | "apple") {
        return Err(format!("Unknown sign-in provider '{provider}'"));
    }
    let attempt = cancel();
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("Could not start sign-in: {e}"))?;
    let redirect = format!("http://127.0.0.1:{}/callback", listener.local_addr().map_err(|e| e.to_string())?.port());
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| e.to_string())?;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let verifier = b64.encode(raw);
    let challenge = b64.encode(Sha256::digest(&verifier));
    let url = reqwest::Url::parse_with_params(
        &format!("{}/auth/v1/authorize", supabase_url()),
        [("provider", provider), ("redirect_to", &redirect), ("code_challenge", &challenge), ("code_challenge_method", "s256")],
    )
    .map_err(|e| e.to_string())?;
    open(url.to_string())?;
    let code = wait_for_code(&listener, Instant::now() + BROWSER_WAIT, attempt)?;
    redeem(&supabase("/token?grant_type=pkce", json!({ "auth_code": code, "code_verifier": verifier }))?, attempt)
}

/// Answers browser requests on the listener until one brings `?code=` (or Supabase's `?error=`),
/// or another attempt, Cancel or Sign out ends this one.
fn wait_for_code(listener: &TcpListener, until: Instant, attempt: u64) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    loop {
        let mut stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if ATTEMPT.load(SeqCst) != attempt {
                    return Err(CANCELLED.into());
                }
                if Instant::now() > until {
                    return Err("Sign-in timed out. Try again.".into());
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut line = String::new();
        let _ = BufReader::new(&stream).read_line(&mut line);
        // "GET /callback?code=...&state=... HTTP/1.1"
        let target = line.split_whitespace().nth(1).unwrap_or("");
        let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else { continue };
        if url.path() != "/callback" {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            continue;
        }
        let param = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        let (code, page) = match (param("code"), param("error_description").or_else(|| param("error"))) {
            (Some(c), _) if !c.is_empty() => (Ok(c), "You are signed in to Echotype. You can close this tab."),
            (_, Some(e)) => (Err(format!("Sign-in did not finish: {e}")), "Sign-in did not finish. Go back to Echotype and try again."),
            _ => (Err("Sign-in did not finish. Try again.".into()), "Sign-in did not finish. Go back to Echotype and try again."),
        };
        let html = format!("<!doctype html><meta charset=utf-8><title>Echotype</title><body style=\"font:16px -apple-system,sans-serif;text-align:center;padding-top:20vh\">{page}</body>");
        let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len()).as_bytes());
        return code;
    }
}

/// GET /v1/me: `{email, plan, words_used, words_limit, resets_at, audio_seconds_used, audio_seconds_limit}`.
pub fn account() -> Result<Value, String> {
    authed(Method::GET, "/me", None)
}

/// Pro: meeting notes for a "Name: words" transcript (POST /v1/summary, which holds the prompt).
pub fn summary(transcript: &str) -> Result<String, String> {
    let v = authed(Method::POST, "/summary", Some(json!({ "transcript": transcript })))?;
    v["summary"].as_str().map(String::from).ok_or_else(|| "Echotype Cloud sent no summary. Try again.".into())
}

/// Pro: an answer to `question` from a "Name: words" transcript (POST /v1/ask).
pub fn ask(transcript: &str, question: &str) -> Result<String, String> {
    let v = authed(Method::POST, "/ask", Some(json!({ "transcript": transcript, "question": question })))?;
    v["answer"].as_str().map(String::from).ok_or_else(|| "Echotype Cloud sent no answer. Try again.".into())
}

/// Stripe Checkout (`plan` = "pro_monthly" | "pro_yearly") or the billing portal: the URL to open.
pub fn billing_url(plan: Option<&str>) -> Result<String, String> {
    let v = match plan {
        Some(p @ ("pro_monthly" | "pro_yearly")) => authed(Method::POST, "/billing/checkout", Some(json!({ "plan": p })))?,
        Some(p) => return Err(format!("Unknown plan '{p}'")),
        None => authed(Method::POST, "/billing/portal", None)?,
    };
    v["url"].as_str().and_then(stripe_url).ok_or_else(|| "Echotype Cloud returned a billing link that is not a Stripe page, so Echotype did not open it.".into())
}

/// Only https pages on Stripe's checkout and billing hosts are opened in the browser.
pub fn stripe_url(raw: &str) -> Option<String> {
    let u = reqwest::Url::parse(raw).ok()?;
    let ok = u.scheme() == "https"
        && matches!(u.host_str(), Some("checkout.stripe.com" | "billing.stripe.com"))
        && u.port().is_none()
        && u.username().is_empty()
        && u.password().is_none();
    ok.then(|| u.to_string())
}

pub fn logout() -> Result<(), String> {
    cancel();
    settings::delete_secret(SECRET)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_stripe_checkout_and_billing_pages_open() {
        assert!(stripe_url("https://checkout.stripe.com/c/pay/cs_test_a1#fid").is_some());
        assert!(stripe_url("https://billing.stripe.com/p/session/test_123").is_some());
        for bad in [
            "http://checkout.stripe.com/c/pay/x",
            "https://checkout.stripe.com.evil.example/c",
            "https://evil.example/?u=https://checkout.stripe.com",
            "https://stripe.com/",
            "https://dashboard.stripe.com/",
            "https://checkout.stripe.com:8443/c",
            "https://user@checkout.stripe.com/c",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "",
        ] {
            assert!(stripe_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn server_errors_read_like_sentences() {
        let quota = r#"{"error":{"message":"You have used this week's 2000 free words.","type":"quota_exceeded","code":"quota_exceeded"}}"#;
        assert_eq!(error_message(402, quota), "You have used this week's 2000 free words.");
        assert!(error_message(401, r#"{"error":{"message":"Missing or invalid API key.","type":"invalid_api_key"}}"#).contains("signed out"));
        assert_eq!(error_message(401, r#"{"error":{"message":"That code is wrong or has expired.","type":"invalid_code"}}"#), "That code is wrong or has expired.");
        assert!(error_message(502, "<html>Bad gateway</html>").contains("(502)"));
    }

    /// A one-request HTTP server: answers `status` + `reply` and hands back what it was sent.
    fn fake_http(status: u16, reply: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut r = BufReader::new(&s);
            let (mut head, mut len) = (String::new(), 0);
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; len];
            std::io::Read::read_exact(&mut r, &mut body).unwrap();
            (&s).write_all(format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).unwrap();
            tx.send(head + &String::from_utf8(body).unwrap()).unwrap();
        });
        (url, rx)
    }

    /// Browser sign-ins share ATTEMPT, so one test's sign-in would cancel another's.
    static BROWSER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn browser_sign_in_ties_the_code_to_this_app() {
        let _one = BROWSER.lock().unwrap_or_else(|e| e.into_inner());
        let (supa, sent) = fake_http(400, r#"{"error":"invalid_grant","error_description":"Invalid code"}"#);
        // SAFETY: only this test reads the variable while it runs (the other browser test never reaches Supabase).
        unsafe { std::env::set_var("ECHOTYPE_SUPABASE_URL", &supa) };
        let mut seen = None;
        let err = sign_in_with("google", |page| {
            let page = reqwest::Url::parse(&page).unwrap();
            let q: std::collections::HashMap<_, _> = page.query_pairs().into_owned().collect();
            assert_eq!((page.path(), q["provider"].as_str(), q["code_challenge_method"].as_str()), ("/auth/v1/authorize", "google", "s256"));
            let back = q["redirect_to"].clone();
            assert!(back.starts_with("http://127.0.0.1:") && back.ends_with("/callback"), "{back}");
            // The browser: a stray request first, then the redirect with the code.
            let browser = std::thread::spawn(move || {
                assert_eq!(reqwest::blocking::get(back.replace("/callback", "/favicon.ico")).unwrap().status(), 404);
                reqwest::blocking::get(format!("{back}?code=abc-123")).unwrap().text().unwrap()
            });
            seen = Some((q["code_challenge"].clone(), browser));
            Ok(())
        })
        .unwrap_err();
        assert_eq!(err, "Invalid code", "Supabase's own message passes through");
        let (challenge, browser) = seen.unwrap();
        assert!(browser.join().unwrap().contains("signed in"));
        let req = sent.recv().unwrap();
        assert!(req.starts_with("POST /auth/v1/token?grant_type=pkce "), "{req}");
        let body: Value = serde_json::from_str(&req[req.find("\r\n\r\n").unwrap() + 4..]).unwrap();
        assert_eq!(body["auth_code"], "abc-123");
        let verifier = body["code_verifier"].as_str().unwrap();
        assert!(verifier.len() >= 43, "PKCE wants 43+ characters");
        assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier)), challenge, "the verifier answers the challenge");
    }

    #[test]
    fn browser_sign_in_reports_a_refusal() {
        let _one = BROWSER.lock().unwrap_or_else(|e| e.into_inner());
        let err = sign_in_with("apple", |page| {
            let q: std::collections::HashMap<_, _> = reqwest::Url::parse(&page).unwrap().query_pairs().into_owned().collect();
            let back = q["redirect_to"].clone();
            std::thread::spawn(move || reqwest::blocking::get(format!("{back}?error=access_denied&error_description=The+user+cancelled")));
            Ok(())
        })
        .unwrap_err();
        assert_eq!(err, "Sign-in did not finish: The user cancelled");
        assert!(sign_in_with("github", |_| Ok(())).is_err());
    }

    #[test]
    fn cancel_ends_a_waiting_browser_sign_in() {
        let _one = BROWSER.lock().unwrap_or_else(|e| e.into_inner());
        let started = Instant::now();
        let err = sign_in_with("google", |_| {
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                cancel(); // the Cancel button, a newer sign-in or Sign out
            });
            Ok(())
        })
        .unwrap_err();
        assert_eq!(err, CANCELLED);
        assert!(started.elapsed() < Duration::from_secs(5), "stops at once, not after BROWSER_WAIT");
    }
}
