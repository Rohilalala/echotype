//! Dictionary replacements, the cleanup LLM call, and the pipeline that ties STT and cleanup together.

use crate::settings::{self, Replacement, Settings};
use crate::stt;
use serde::Serialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const PROMPT: &str = include_str!("cleanup_prompt.txt");

pub fn system_prompt(dictionary: &[String], style: &str, custom: &str) -> String {
    let words: Vec<&str> = dictionary.iter().map(|w| w.trim()).filter(|w| !w.is_empty()).collect();
    let dict = if words.is_empty() { String::new() } else { format!("\nSpell these names and terms exactly as written: {}.", words.join(", ")) };
    let style = match style {
        "casual" => "\nStyle: relaxed and conversational. Use contractions and light punctuation.",
        "formal" => "\nStyle: polished and professional. Write complete, well-formed sentences.",
        _ => "",
    };
    let custom = if custom.trim().is_empty() { String::new() } else { format!("\n{}", custom.trim()) };
    PROMPT.replace("{{DICTIONARY}}", &dict).replace("{{STYLE}}", style).replace("{{CUSTOM}}", &custom).trim_end().to_string()
}

/// Local servers (Ollama, LM Studio) think for seconds unless reasoning is switched off.
pub fn reasoning_off(provider: &str) -> bool {
    matches!(provider, "ollama" | "lmstudio")
}

pub fn request_body(model: &str, system: &str, transcript: &str, reasoning_off: bool) -> Value {
    let mut body = json!({
        "model": model,
        "temperature": 0,
        "stream": false,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": format!("<transcript>{transcript}</transcript>") },
        ],
    });
    if reasoning_off {
        body["reasoning_effort"] = json!("none");
    }
    body
}

/// Content of the first choice, minus any leaked <think> block.
pub fn parse_reply(v: &Value) -> Option<String> {
    let c = v["choices"][0]["message"]["content"].as_str()?;
    let c = c.rsplit_once("</think>").map_or(c, |(_, after)| after);
    Some(c.trim().to_string())
}

pub fn cleanup(s: &Settings, text: &str) -> Result<String, String> {
    let c = &s.cleanup;
    let base = settings::base_url(&c.provider, &c.base_url)?;
    let model = if c.model.trim().is_empty() { settings::preset(&c.provider).map(|p| p.default_cleanup_model).unwrap_or_default() } else { c.model.trim() };
    let system = system_prompt(&s.dictionary, &c.style, &c.custom_instructions);
    let key = settings::get_secret(&format!("llm:{}", c.provider));
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).build().map_err(|e| e.to_string())?;
    let send = |off: bool| {
        let mut req = client.post(format!("{base}/chat/completions")).json(&request_body(model, &system, text, off));
        if let Some(k) = &key {
            req = req.bearer_auth(k);
        }
        req.send().map_err(|e| format!("Cleanup request failed: {e}"))
    };
    let off = reasoning_off(&c.provider);
    let mut resp = send(off)?;
    if off && resp.status() == reqwest::StatusCode::BAD_REQUEST {
        resp = send(false)?; // server rejected reasoning_effort; retry once without it
    }
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Cleanup failed ({status}): {}", body.chars().take(300).collect::<String>()));
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| "Cleanup returned a non-JSON response".to_string())?;
    parse_reply(&v).ok_or_else(|| "Cleanup response had no message content".to_string())
}

/// Case-insensitive, whole-word (or whole-phrase) replacements.
pub fn apply_replacements(text: &str, reps: &[Replacement]) -> String {
    let mut out = text.to_string();
    for r in reps.iter().filter(|r| !r.from.trim().is_empty()) {
        out = replace_word(&out, r.from.trim(), &r.to);
    }
    out
}

fn replace_word(text: &str, from: &str, to: &str) -> String {
    let t: Vec<char> = text.chars().collect();
    let f: Vec<char> = from.chars().collect();
    let eq = |a: char, b: char| a == b || a.to_lowercase().eq(b.to_lowercase());
    let boundary = |i: Option<&char>| i.is_none_or(|c| !c.is_alphanumeric());
    let (mut out, mut i) = (String::new(), 0);
    while i < t.len() {
        let hit = i + f.len() <= t.len()
            && t[i..i + f.len()].iter().zip(&f).all(|(a, b)| eq(*a, *b))
            && boundary(if i == 0 { None } else { t.get(i - 1) })
            && boundary(t.get(i + f.len()));
        if hit {
            out.push_str(to);
            i += f.len();
        } else {
            out.push(t[i]);
            i += 1;
        }
    }
    out
}

// ---------- the pipeline (shared by dictation, file drop, CLI and local API) ----------

#[derive(Serialize, Debug, Clone)]
pub struct Output {
    pub raw: String,
    pub text: String,
    pub stt_ms: u64,
    pub cleanup_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// `verbatim` keeps the recognizer's words and skips the LLM entirely.
pub fn wants_cleanup(s: &Settings, text: &str) -> bool {
    s.cleanup.enabled && s.cleanup.style != "verbatim" && !text.trim().is_empty()
}

/// STT → replacements → cleanup. Cleanup failure never loses text: the raw (replaced)
/// transcript is returned with a warning. `on_cleaning` fires before the LLM call.
pub fn run(s: &Settings, pcm: &[f32], on_cleaning: impl FnOnce()) -> Result<Output, String> {
    let t = Instant::now();
    let prompt = stt::prompt(&s.dictionary);
    let (raw, mut warning) = if s.stt.provider == "local" {
        let (model, warning) = stt::pick_local_model(s)?;
        let lang = (s.language != "auto" && !s.language.is_empty()).then_some(s.language.as_str());
        (stt::transcribe_local(&model, pcm, lang, s.translate_to_english, &prompt)?, warning)
    } else {
        (stt::transcribe_remote(s, pcm, &prompt)?, None)
    };
    let stt_ms = t.elapsed().as_millis() as u64;

    let mut text = apply_replacements(&raw, &s.replacements);
    let mut cleanup_ms = 0;
    if wants_cleanup(s, &text) {
        on_cleaning();
        let t = Instant::now();
        match cleanup(s, &text) {
            Ok(c) if !c.is_empty() => text = c,
            Ok(_) => {}
            Err(e) => warning = Some(format!("{e}. Pasted the raw transcript instead.")),
        }
        cleanup_ms = t.elapsed().as_millis() as u64;
    }
    Ok(Output { raw, text, stt_ms, cleanup_ms, warning })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_fills_every_placeholder() {
        let p = system_prompt(&["Arnav".into(), "Xoxoday".into()], "casual", "  Use British spelling. ");
        assert!(p.starts_with("You clean up dictated text."));
        assert!(p.contains("\nSpell these names and terms exactly as written: Arnav, Xoxoday."));
        assert!(p.contains("contractions and light punctuation"));
        assert!(p.ends_with("\nUse British spelling."));
        assert!(!p.contains("{{"));

        let plain = system_prompt(&[], "default", "");
        assert!(plain.ends_with("If the transcript is empty or only filler, output nothing."));
        assert!(system_prompt(&[], "formal", "").contains("complete, well-formed sentences"));
    }

    #[test]
    fn request_body_shape() {
        let b = request_body("gemma3:1b", "SYS", "um hello", true);
        assert_eq!(b["temperature"], 0);
        assert_eq!(b["model"], "gemma3:1b");
        assert_eq!(b["messages"][0]["content"], "SYS");
        assert_eq!(b["messages"][1]["content"], "<transcript>um hello</transcript>");
        assert_eq!(b["reasoning_effort"], "none");
        assert!(request_body("m", "s", "t", false).get("reasoning_effort").is_none());
        assert!(reasoning_off("ollama") && reasoning_off("lmstudio") && !reasoning_off("groq"));
    }

    #[test]
    fn reply_parsing() {
        let v = json!({"choices":[{"message":{"content":"<think>hmm</think>\n Hello. "}}]});
        assert_eq!(parse_reply(&v).unwrap(), "Hello.");
        assert!(parse_reply(&json!({"error":"x"})).is_none());
    }

    #[test]
    fn replacements_are_whole_word_case_insensitive() {
        let reps = vec![
            Replacement { from: "my email".into(), to: "me@example.com".into() },
            Replacement { from: "AI".into(), to: "A.I.".into() },
        ];
        assert_eq!(apply_replacements("Send it to My Email, thanks", &reps), "Send it to me@example.com, thanks");
        assert_eq!(apply_replacements("ai and said and AI.", &reps), "A.I. and said and A.I..");
        assert_eq!(apply_replacements("myemail stays", &reps), "myemail stays");
        assert_eq!(apply_replacements("Ünïcode ok", &[Replacement { from: "ünïcode".into(), to: "U".into() }]), "U ok");
        assert_eq!(apply_replacements("x", &[Replacement::default()]), "x");
    }

    #[test]
    fn verbatim_and_disabled_cleanup_skip_the_llm() {
        let mut s = Settings::default();
        assert!(!wants_cleanup(&s, "hello"));
        s.cleanup.enabled = true;
        assert!(wants_cleanup(&s, "hello"));
        assert!(!wants_cleanup(&s, "  "));
        s.cleanup.style = "verbatim".into();
        assert!(!wants_cleanup(&s, "hello"));
    }
}
