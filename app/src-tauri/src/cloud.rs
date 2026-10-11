//! Echotype Cloud account (server/ in this repo): email-code sign-in, usage, Stripe checkout and portal.
//! The API key lives only in the keychain as `cloud`; the UI never sees it.

use crate::settings;
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

const SECRET: &str = "cloud";
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

pub fn start_login(email: &str) -> Result<(), String> {
    call(Method::POST, "/auth/start", None, Some(json!({ "email": email.trim() }))).map(drop).map_err(|(_, e)| e)
}

/// Redeems the code, stores the new key, and returns `{email, plan}` (never the key).
pub fn verify(email: &str, code: &str) -> Result<Value, String> {
    let v = call(Method::POST, "/auth/verify", None, Some(json!({ "email": email.trim(), "code": code.trim() }))).map_err(|(_, e)| e)?;
    let key = v["api_key"].as_str().filter(|k| !k.is_empty()).ok_or("Echotype Cloud did not return a key. Try signing in again.")?;
    settings::set_secret(SECRET, key)?;
    Ok(json!({ "email": v["email"], "plan": v["plan"] }))
}

/// GET /v1/me: `{email, plan, words_used, words_limit, resets_at, audio_seconds_used, audio_seconds_limit}`.
pub fn account() -> Result<Value, String> {
    authed(Method::GET, "/me", None)
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

    /// Against a real server/ in dev mode (SMTP_HOST empty, so it logs each code as JSON):
    /// ECHOTYPE_CLOUD_URL=http://127.0.0.1:8790/v1 ECHOTYPE_TEST_EMAIL=a@example.com
    /// ECHOTYPE_TEST_SERVER_LOG=<server stdout file> [ECHOTYPE_TEST_WAV=<speech.wav>] cargo test live_sign_in -- --ignored --nocapture
    /// It leaves the key in the keychain (for a CLI run afterwards) unless ECHOTYPE_TEST_LOGOUT=1.
    #[test]
    #[ignore = "needs a running dev-mode server"]
    fn live_sign_in() {
        let email = std::env::var("ECHOTYPE_TEST_EMAIL").unwrap();
        let log = std::env::var("ECHOTYPE_TEST_SERVER_LOG").unwrap();
        assert_eq!(account().unwrap_err(), SIGNED_OUT);
        start_login(&email).unwrap();
        let code = std::fs::read_to_string(&log).unwrap().lines().rev()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["email"] == email.as_str() && v["code"].is_string())
            .map(|v| v["code"].as_str().unwrap().to_string())
            .expect("no dev-mode code in the server log");
        assert!(verify(&email, "000000").is_err() || code == "000000");
        let who = verify(&email, &code).unwrap();
        assert_eq!(who, json!({ "email": email, "plan": "free" }));
        assert!(settings::secret(SECRET).unwrap().is_some_and(|k| k.starts_with("et_live_")));
        let me = account().unwrap();
        println!("account: {me}");
        assert_eq!(me["plan"], "free");
        assert_eq!(me["words_limit"], 2000);
        assert_eq!(billing_url(None).unwrap_err(), "Billing is not set up on this server.");
        // The dictation pipeline with both stages on the cloud provider, using the one `cloud` key.
        if let Ok(wav) = std::env::var("ECHOTYPE_TEST_WAV") {
            let mut s = settings::Settings::default();
            s.stt.provider = "cloud".into();
            s.cleanup.enabled = true;
            s.cleanup.provider = "cloud".into();
            let pcm = crate::audio::decode(std::fs::read(wav).unwrap(), Some("wav")).unwrap();
            let out = crate::cleanup::run(&s, &pcm, || {}).unwrap();
            println!("pipeline: raw {:?}, text {:?}, warning {:?}", out.raw, out.text, out.warning);
            assert_eq!(out.raw, "hello world");
            let after = account().unwrap();
            println!("account after: {after}");
            assert_eq!(after["words_used"].as_i64().unwrap(), me["words_used"].as_i64().unwrap() + 2);
        }
        if std::env::var("ECHOTYPE_TEST_LOGOUT").is_ok_and(|v| v == "1") {
            logout().unwrap();
            assert_eq!(account().unwrap_err(), SIGNED_OUT);
        }
    }
}
