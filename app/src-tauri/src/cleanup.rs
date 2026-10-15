//! Dictionary replacements, the cleanup LLM call, and the pipeline that ties STT and cleanup together.

use crate::llm;
use crate::settings::{self, Replacement, Settings};
use crate::stt;
use serde::Serialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const PROMPT: &str = include_str!("cleanup_prompt.txt");

/// The cleanup system prompt. The `echotype` provider is our fine-tuned model, which was trained
/// on a short prompt (model/gen.py system_prompt) and gets that instead of cleanup_prompt.txt.
pub fn system_prompt(s: &Settings) -> String {
    let (dictionary, style, custom) = (&s.dictionary, s.cleanup.style.as_str(), s.cleanup.custom_instructions.as_str());
    let words: Vec<&str> = dictionary.iter().map(|w| w.trim()).filter(|w| !w.is_empty()).collect();
    if s.cleanup.provider == "echotype" {
        let mut p = "Clean up the dictated transcript. Output only what the speaker meant to type.".to_string();
        if !words.is_empty() {
            p += &format!("\nDictionary: {}", words.join(", "));
        }
        if s.translate_to_english {
            p += "\nTranslate to English: yes";
        }
        return p;
    }
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
/// `"content": null` (mlx_lm.server's empty completion) reads as "".
pub fn parse_reply(v: &Value) -> Option<String> {
    let c = match v["choices"][0]["message"].get("content")? {
        Value::Null => "",
        c => c.as_str()?,
    };
    let c = c.rsplit_once("</think>").map_or(c, |(_, after)| after);
    Some(c.trim().to_string())
}

pub fn cleanup(s: &Settings, text: &str) -> Result<String, String> {
    let system = system_prompt(s);
    if s.cleanup.provider != "echotype" {
        return chat(s, &system, text, Duration::from_secs(30));
    }
    // The built-in model has a small context (llm::CONTEXT): a long dictation is cleaned a few
    // sentences at a time and joined, never cut off.
    let budget = (llm::context(s) as usize).saturating_sub(64 + tokens(&system)) / 2;
    let mut out = String::new();
    for (piece, gap) in pieces(text, budget)? {
        let c = chat(s, &system, piece, Duration::from_secs(30))?;
        // A piece of filler is rightly nothing; a short answer the model dropped ("No.") is kept.
        let c = if c.is_empty() && !only_filler(piece) { piece.trim().to_string() } else { c };
        if !c.is_empty() {
            out.push_str(&c);
            out.push_str(gap);
        }
    }
    Ok(out.trim_end().to_string())
}

/// Tokens a text may take, overestimated: about 4.5 bytes a token in English, 3 (one character)
/// in Devanagari or CJK.
fn tokens(text: &str) -> usize {
    text.len().div_ceil(3)
}

/// `text` cut at sentence ends (. ! ? … or a line break) into pieces of at most `budget` tokens,
/// each with the gap that followed it ("\n" for a line break, else " "). One sentence over the
/// budget is an error: cleanup is then skipped with a notice rather than cut.
fn pieces(text: &str, budget: usize) -> Result<Vec<(&str, &'static str)>, String> {
    let mut sentences = vec![];
    let mut start = 0;
    let mut it = text.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let end = c == '\n' || (matches!(c, '.' | '!' | '?' | '…') && it.peek().is_none_or(|(_, n)| n.is_whitespace()));
        if end {
            let mut j = i + c.len_utf8();
            let mut breaks = c == '\n';
            while let Some(&(k, w)) = it.peek().filter(|(_, w)| w.is_whitespace()) {
                breaks |= w == '\n';
                j = k + w.len_utf8();
                it.next();
            }
            sentences.push((&text[start..j], breaks));
            start = j;
        }
    }
    if start < text.len() {
        sentences.push((&text[start..], false));
    }
    let mut out: Vec<(&str, &'static str)> = vec![];
    let mut piece: Option<(usize, usize, bool)> = None; // byte range in `text`, ends with a line break
    for (sentence, breaks) in sentences {
        if tokens(sentence.trim()) > budget {
            return Err(format!("a sentence of {} words is too long for it to clean in one go", sentence.split_whitespace().count()));
        }
        let at = sentence.as_ptr() as usize - text.as_ptr() as usize;
        piece = match piece {
            Some((a, _, _)) if tokens(text[a..at + sentence.len()].trim()) <= budget => Some((a, at + sentence.len(), breaks)),
            Some((a, b, br)) => {
                out.push((text[a..b].trim(), if br { "\n" } else { " " }));
                Some((at, at + sentence.len(), breaks))
            }
            None => Some((at, at + sentence.len(), breaks)),
        };
    }
    if let Some((a, b, br)) = piece {
        out.push((text[a..b].trim(), if br { "\n" } else { " " }));
    }
    out.retain(|(p, _)| !p.is_empty());
    Ok(out)
}

/// One chat completion with the configured cleanup provider: `system` as the system prompt and
/// `text` inside <transcript> tags. Also used for meeting summaries.
pub fn chat(s: &Settings, system: &str, text: &str, timeout: Duration) -> Result<String, String> {
    let c = &s.cleanup;
    let (base, key) = if c.provider == "echotype" {
        let (url, key) = llm::ensure(s)?; // the built-in model's server, started on first use
        (url, Some(key))
    } else {
        (settings::base_url(&c.provider, &c.base_url)?, settings::provider_key("llm", &c.provider))
    };
    let model = if c.model.trim().is_empty() { settings::preset(&c.provider).map(|p| p.default_cleanup_model).unwrap_or_default() } else { c.model.trim() };
    let client = reqwest::blocking::Client::builder().timeout(timeout).build().map_err(|e| e.to_string())?;
    let send = |off: bool| {
        let mut req = client.post(format!("{base}/chat/completions")).json(&request_body(model, system, text, off));
        if let Some(k) = &key {
            req = req.bearer_auth(k);
        }
        req.send().map_err(|e| format!("request failed: {e}"))
    };
    let off = reasoning_off(&c.provider);
    let mut resp = send(off)?;
    if off && resp.status() == reqwest::StatusCode::BAD_REQUEST {
        resp = send(false)?; // server rejected reasoning_effort; retry once without it
    }
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        if c.provider == "cloud" {
            return Err(crate::cloud::error_message(status.as_u16(), &body));
        }
        return Err(format!("{status}: {}", body.chars().take(300).collect::<String>()));
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| "it returned a non-JSON response".to_string())?;
    if c.provider == "echotype" {
        llm::touch();
        if v["choices"][0]["finish_reason"] == "length" {
            return Err("its answer ran out of room and was cut off".into()); // never paste a cut-off text
        }
    }
    parse_reply(&v).ok_or_else(|| "its response had no message content".to_string())
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
    /// Cleanup did not run because it is off or its provider has no key (shown as "No cleanup").
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_cleanup: bool,
    /// The cleanup the guard rejected (`text` is the words as spoken): "Use cleaned version" copies it.
    #[serde(skip)]
    pub cleaned: Option<String>,
}

/// `verbatim` keeps the recognizer's words and skips the LLM entirely.
pub fn wants_cleanup(s: &Settings, text: &str) -> bool {
    s.cleanup.enabled && s.cleanup.style != "verbatim" && !text.trim().is_empty()
}

/// True when the transcript holds at least one letter or digit in any script. Whisper turns
/// noise into "." or "...", which must count as nothing said.
pub fn has_speech(text: &str) -> bool {
    text.chars().any(char::is_alphanumeric)
}

/// The numbers in a text ("12", "3.5", "10:30" counts as 10 and 30), as written.
/// Thousands commas are joined ("1,500" and "1500" are one number) and all-zero parts ("5.00") skipped.
fn numbers(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let joined: String = chars.iter().enumerate().filter(|&(i, c)| !(*c == ',' && i > 0 && chars[i - 1].is_ascii_digit() && chars.get(i + 1).is_some_and(char::is_ascii_digit))).map(|(_, c)| *c).collect();
    joined.split(|c: char| !c.is_ascii_digit()).map(|n| n.trim_start_matches('0')).filter(|n| !n.is_empty()).map(String::from).collect()
}

/// A cleanup that loses a number the speaker said changed what they meant (the 1.5B model turns
/// "12 blueberries, 3 chocolates" into a list and drops the amounts as if they were counters).
/// Dropping one is fine only after a spoken correction ("12, no actually 3").
// ponytail: a word-list heuristic for corrections; replace when the model stops dropping amounts.
pub fn lost_a_number(raw: &str, cleaned: &str) -> bool {
    const CORRECTIONS: [&str; 10] = ["no actually", "actually", "no wait", "wait", "i mean", "scratch that", "sorry", "make that", "rather", "nahi"];
    let low = raw.to_lowercase();
    if CORRECTIONS.iter().any(|c| low.contains(c)) {
        return false;
    }
    let mut left = numbers(cleaned);
    numbers(raw).iter().any(|n| match left.iter().position(|m| m == n) {
        Some(i) => {
            left.swap_remove(i);
            false
        }
        None => true,
    })
}

/// Why a cleanup must not be pasted, if it changed what the speaker said: the words as spoken
/// are pasted instead. Owner tests on 2026-10-16 (history.jsonl) caught the 1.5B model turning
/// "don't forget to not include the tax" into "...to include the tax", "never not worth asking"
/// into "never worth asking", "I was" into "I'll be", and finishing a cut-off "The only thing I'd..."
/// with a sentence nobody said.
fn rejected(s: &Settings, raw: &str, cleaned: &str) -> Option<&'static str> {
    if lost_a_number(raw, cleaned) {
        return Some("Cleanup left out a number you said, so Echotype pasted your words as spoken.");
    }
    // "5" for a spoken "paanch" is fine; "5:30" for it is a guess.
    if numbers(cleaned).len() > numbers(raw).len() + words(raw).iter().filter(|w| NUMBER_WORDS.contains(&w.as_str())).count() {
        return Some("Cleanup added a number you did not say, so Echotype pasted your words as spoken.");
    }
    if negations(cleaned) < negations(raw) {
        return Some("Cleanup dropped a \"not\" you said, so Echotype pasted your words as spoken.");
    }
    // A translation is all new words; only its negations can be checked.
    if !s.translate_to_english && added_words(raw, cleaned, &s.dictionary) {
        return Some("Cleanup added words you did not say, so Echotype pasted your words as spoken.");
    }
    None
}

/// Lower-case words, with contractions spelled out ("don't" -> "do not", "I'll" -> "I will") so
/// that contracting or expanding one is not a change. Hyphens split ("non-members" -> "non").
fn words(text: &str) -> Vec<String> {
    let sep = |c: char| c.is_whitespace() || (c.is_ascii_punctuation() && c != '\'') || "“”…—–।॥«»¿¡₹€£¥".contains(c);
    let mut out = vec![];
    for w in text.to_lowercase().replace(['’', '‘'], "'").split(sep).map(|w| w.trim_matches('\'')).filter(|w| !w.is_empty()) {
        let Some((stem, tail)) = w.split_once('\'') else {
            match w {
                "gonna" => out.extend(["going".into(), "to".into()]),
                "wanna" => out.extend(["want".into(), "to".into()]),
                "gotta" => out.extend(["got".into(), "to".into()]),
                "okay" => out.push("ok".into()),
                _ => out.push(w.to_string()),
            }
            continue;
        };
        match tail {
            "t" => {
                out.push(match stem {
                    "won" => "will".into(),
                    "can" => "can".into(),
                    "ain" => "is".into(),
                    _ => stem.strip_suffix('n').unwrap_or(stem).to_string(),
                });
                out.push("not".into());
            }
            "ll" | "re" | "ve" | "m" | "d" => {
                out.push(stem.to_string());
                out.push(match tail { "ll" => "will", "re" => "are", "ve" => "have", "m" => "am", _ => "would" }.into());
            }
            _ => out.push(stem.to_string()), // 's: possessive, is or has; the stem is the word
        }
    }
    out
}

/// How many times the text says no. "No" that starts a spoken correction ("Tuesday, no wait,
/// Thursday") is not a negation: cleanup rightly removes it with the words it corrects.
// ponytail: English and Hindi/Hinglish word list; add a language's negations when it gets tested.
fn negations(text: &str) -> usize {
    const NEG: [&str; 21] = ["not", "no", "never", "nor", "none", "nothing", "nobody", "nowhere", "neither", "cannot", "non", "nahi", "nahin", "nai", "mat", "na", "नहीं", "नही", "मत", "ना", "न"];
    const CORRECTION: [&str; 8] = ["wait", "sorry", "actually", "i", "scratch", "hold", "make", "no"];
    let w = words(text);
    (0..w.len()).filter(|&i| NEG.contains(&w[i].as_str()) && !(w[i] == "no" && w.get(i + 1).is_some_and(|n| CORRECTION.contains(&n.as_str())))).count()
}

/// Spoken numbers, which cleanup may write as digits: English, then Hinglish and Hindi.
// ponytail: "do" (2 in Hindi) also counts the English verb, a slightly looser number check.
const NUMBER_WORDS: [&str; 77] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety", "hundred", "thousand", "million", "dozen", "half",
    "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth", "tenth",
    "ek", "do", "teen", "char", "chaar", "paanch", "panch", "chhe", "saat", "aath", "nau", "das", "sau", "hazaar", "hazar", "lakh", "crore",
    "एक", "दो", "तीन", "चार", "पांच", "पाँच", "पाच", "छह", "सात", "आठ", "नौ", "दस", "सौ", "हजार", "हज़ार", "लाख", "करोड़",
];

/// True when the cleanup holds a word the speaker never said and the Dictionary does not name.
/// Numbers in digits or words, list punctuation, articles and small prepositions ("open on two
/// sides") may appear.
fn added_words(raw: &str, cleaned: &str, dictionary: &[String]) -> bool {
    const FREE: [&str; 11] = ["a", "an", "the", "and", "is", "on", "in", "of", "to", "for", "at"];
    let said: std::collections::HashSet<String> = words(raw).into_iter().chain(dictionary.iter().flat_map(|d| words(d))).collect();
    words(cleaned).iter().any(|w| !said.contains(w) && !FREE.contains(&w.as_str()) && !NUMBER_WORDS.contains(&w.as_str()) && !w.starts_with(|c: char| c.is_ascii_digit()))
}

/// What whisper writes for noise or silence ("Thank you.", "Thanks for watching!"): its training
/// subtitles. Dropped only when the clip also had no voice in it (lib.rs NOISE_SNR_DB).
pub fn whisper_filler(text: &str) -> bool {
    const PHRASES: [&str; 12] = ["thank you", "thank you very much", "thanks", "thanks for watching", "thank you for watching", "thank you so much for watching", "please subscribe", "you", "bye", "music", "applause", "subtitles by the amaraorg community"];
    let t: String = text.to_lowercase().chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    PHRASES.contains(&t.as_str())
}

/// Nothing but filler ("um", "uh, you know", "Hmm."): what the Echotype model rightly answers with
/// nothing. "No", "yes", "nope" and "ok" are answers, not filler.
pub fn only_filler(text: &str) -> bool {
    const FILLER: [&str; 17] = ["um", "umm", "uh", "uhh", "er", "erm", "ah", "hmm", "hm", "mm", "like", "you", "know", "i", "mean", "so", "well"];
    text.split(|c: char| !c.is_alphanumeric() && c != '\'').filter(|w| !w.is_empty()).all(|w| FILLER.contains(&w.to_lowercase().as_str()))
}

/// Spoken "new line" / "new paragraph" become line breaks without the LLM. Whole phrase, any case;
/// the spaces around it and the punctuation Whisper adds after it ("New line.") are absorbed.
pub fn spoken_breaks(text: &str) -> String {
    let t: Vec<char> = text.chars().collect();
    let boundary = |i: Option<&char>| i.is_none_or(|c| !c.is_alphanumeric());
    let at = |i: usize, p: &str| {
        let p: Vec<char> = p.chars().collect();
        i + p.len() <= t.len()
            && t[i..i + p.len()].iter().zip(&p).all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
            && boundary(if i == 0 { None } else { t.get(i - 1) })
            && boundary(t.get(i + p.len()))
    };
    let (mut out, mut i) = (String::new(), 0);
    while i < t.len() {
        let hit = [("new paragraph", "\n\n"), ("new line", "\n")].into_iter().find(|(p, _)| at(i, p));
        let Some((phrase, br)) = hit else {
            out.push(t[i]);
            i += 1;
            continue;
        };
        out.truncate(out.trim_end_matches([' ', '\t']).len());
        out.push_str(br);
        i += phrase.chars().count();
        while i < t.len() && matches!(t[i], '.' | ',' | ';' | ':' | '!' | '?') {
            i += 1;
        }
        while i < t.len() && matches!(t[i], ' ' | '\t') {
            i += 1;
        }
    }
    out
}

/// Whether cleanup is wanted but cannot run because its provider needs a key that is not saved.
pub fn keyless(s: &Settings) -> bool {
    let c = &s.cleanup;
    settings::preset(&c.provider).is_some_and(|p| p.needs_key) && settings::provider_key("llm", &c.provider).is_none()
}

/// STT → replacements → cleanup. Cleanup failure never loses text: the raw (replaced)
/// transcript is returned with a warning. `on_cleaning` fires before the LLM call. `dictated`:
/// live dictation, where spoken "new line" is a command; a voice note or API upload keeps its words.
pub fn run(s: &Settings, pcm: &[f32], dictated: bool, on_cleaning: impl FnOnce()) -> Result<Output, String> {
    let t = Instant::now();
    let prompt = stt::prompt(&s.dictionary);
    let (raw, warning) = if s.stt.provider == "local" {
        let (model, warning) = stt::pick_local_model(s)?;
        (stt::transcribe_local(&model, pcm, &s.languages, s.translate_to_english, &prompt, crate::speakers::vad().as_deref())?, warning)
    } else {
        (stt::transcribe_remote(s, pcm, &prompt)?, None)
    };
    let stt_ms = t.elapsed().as_millis() as u64;
    let keyless = s.cleanup.enabled && keyless(s);
    Ok(after_stt(s, raw, stt_ms, warning, keyless, dictated, on_cleaning, cleanup))
}

/// Everything after STT, with the LLM call passed in so tests never touch the network.
#[allow(clippy::too_many_arguments)]
fn after_stt(s: &Settings, raw: String, stt_ms: u64, mut warning: Option<String>, keyless: bool, dictated: bool, on_cleaning: impl FnOnce(), llm: impl Fn(&Settings, &str) -> Result<String, String>) -> Output {
    if !has_speech(&raw) {
        return Output { raw: String::new(), text: String::new(), stt_ms, cleanup_ms: 0, warning, no_cleanup: false, cleaned: None };
    }
    let replaced = apply_replacements(&raw, &s.replacements);
    let mut text = if dictated { spoken_breaks(&replaced) } else { replaced };
    let (mut cleanup_ms, mut cleaned) = (0, None);
    // "No cleanup": switched off, or on with no key to call the provider. Verbatim is a choice, not raw.
    let no_cleanup = !s.cleanup.enabled || keyless;
    if !keyless && wants_cleanup(s, &text) {
        on_cleaning();
        let t = Instant::now();
        match llm(s, &text) {
            Ok(c) if !c.is_empty() => match rejected(s, &text, &c) {
                Some(w) => (warning, cleaned) = (Some(w.into()), Some(c)),
                None => text = c,
            },
            // The Echotype model was trained to answer filler-only input with nothing: no speech.
            // It also drops some one-word answers ("No.", "Yes.", "Nope."), so real words are kept.
            Ok(_) if s.cleanup.provider == "echotype" && only_filler(&text) => text.clear(),
            Ok(_) => {}
            Err(e) => {
                let first = e.lines().next().unwrap_or_default().trim_end_matches('.');
                let provider = settings::preset(&s.cleanup.provider).map_or(s.cleanup.provider.as_str(), |p| p.label);
                warning = Some(format!("Cleanup failed: {provider}: {first}. Pasted the raw transcript instead."));
            }
        }
        cleanup_ms = t.elapsed().as_millis() as u64;
    }
    Output { raw, text, stt_ms, cleanup_ms, warning, no_cleanup, cleaned }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_fills_every_placeholder() {
        let prompt = |dict: &[&str], style: &str, custom: &str| {
            let mut s = Settings::default();
            s.dictionary = dict.iter().map(|w| w.to_string()).collect();
            s.cleanup.style = style.into();
            s.cleanup.custom_instructions = custom.into();
            system_prompt(&s)
        };
        let p = prompt(&["Arnav", "Xoxoday"], "casual", "  Use British spelling. ");
        assert!(p.starts_with("You clean up dictated text."));
        assert!(p.contains("\nSpell these names and terms exactly as written: Arnav, Xoxoday."));
        assert!(p.contains("contractions and light punctuation"));
        assert!(p.ends_with("\nUse British spelling."));
        assert!(!p.contains("{{"));
        assert!(p.contains("Keep the line breaks already in the transcript."));

        let plain = prompt(&[], "default", "");
        assert!(plain.ends_with("If the transcript is empty or only filler, output nothing."));
        assert!(prompt(&[], "formal", "").contains("complete, well-formed sentences"));
    }

    #[test]
    fn echotype_model_gets_its_trained_prompt() {
        let mut s = Settings::default();
        s.cleanup.provider = "echotype".into();
        s.cleanup.style = "formal".into();
        s.cleanup.custom_instructions = "Use British spelling.".into();
        let head = "Clean up the dictated transcript. Output only what the speaker meant to type.";
        assert_eq!(system_prompt(&s), head);
        s.dictionary = vec!["Arnav".into(), " ".into(), " Xoxoday ".into()];
        assert_eq!(system_prompt(&s), format!("{head}\nDictionary: Arnav, Xoxoday"));
        s.translate_to_english = true;
        assert_eq!(system_prompt(&s), format!("{head}\nDictionary: Arnav, Xoxoday\nTranslate to English: yes"));
        s.dictionary.clear();
        assert_eq!(system_prompt(&s), format!("{head}\nTranslate to English: yes"));
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
        assert_eq!(parse_reply(&json!({"choices":[{"message":{"role":"assistant","content":null}}]})).unwrap(), "");
        assert!(parse_reply(&json!({"choices":[{"message":{"content":7}}]})).is_none());
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

    fn post(s: &Settings, raw: &str) -> Output {
        after_stt(s, raw.into(), 1, None, false, true, || {}, |_, t| Ok(format!("«{t}»")))
    }

    #[test]
    fn transcripts_without_letters_or_digits_are_empty() {
        let mut s = Settings::default();
        s.cleanup.enabled = true;
        let w = after_stt(&s, ".".into(), 1, Some("medium was used".into()), false, true, || {}, |_, _| unreachable!());
        assert_eq!(w.warning.as_deref(), Some("medium was used"), "a model-fallback note survives an empty result");
        for noise in [".", " , ", "...", "", " - ! "] {
            let o = post(&s, noise);
            assert_eq!((o.text.as_str(), o.raw.as_str(), o.no_cleanup), ("", "", false), "{noise:?}");
        }
        for kept in ["a.", "2", "नमस्ते"] {
            assert!(post(&s, kept).text.starts_with('«'), "{kept}");
        }
    }

    #[test]
    fn an_empty_reply_is_no_speech_only_for_the_echotype_model() {
        let mut s = Settings::default();
        s.cleanup.enabled = true;
        let empty = |s: &Settings| after_stt(s, "um uh you know".into(), 1, None, false, true, || {}, |_, _| Ok(String::new()));
        assert_eq!(empty(&s).text, "um uh you know", "other providers keep the raw text");
        s.cleanup.provider = "echotype".into();
        let o = empty(&s);
        assert_eq!((o.text.as_str(), o.warning, o.no_cleanup), ("", None, false));
        // ...but a short answer the model dropped is still typed.
        for word in ["No.", "Yes.", "Nope.", "OK", "Bye."] {
            let o = after_stt(&s, word.into(), 1, None, false, true, || {}, |_, _| Ok(String::new()));
            assert_eq!(o.text, word);
        }
    }

    #[test]
    fn whisper_noise_phrases_are_recognised() {
        for t in ["Thank you.", "thank you", " Thanks for watching! ", "You", "[Music]".trim_matches(['[', ']'])] {
            assert!(whisper_filler(t), "{t}");
        }
        for t in ["Thank you, Priya.", "Thank you for the notes.", "Yes", "No thanks, I'm good."] {
            assert!(!whisper_filler(t), "{t}");
        }
    }

    #[test]
    fn a_cleanup_that_drops_an_amount_is_not_used() {
        let raw = "Create a list of 12 blueberries, 3 chocolates and 15 mobile phones.";
        assert!(lost_a_number(raw, "Create a list of:\n1. Blueberries\n2. Chocolates\n3. Mobile phones"));
        assert!(!lost_a_number(raw, "Create a list of:\n- 12 blueberries\n- 3 chocolates\n- 15 mobile phones"));
        assert!(!lost_a_number("I need 12 blueberries, no actually just 3", "I need 3 blueberries."), "a correction may drop one");
        assert!(!lost_a_number("meet at 10:30 on the 5th", "Meet at 10:30 on the 5th."));
        assert!(!lost_a_number("it costs 1,500 dollars or $5.00 each", "It costs $1500, or $5 each."), "formatting is not loss");
        let mut s = Settings::default();
        s.cleanup.enabled = true;
        let o = after_stt(&s, raw.into(), 1, None, false, true, || {}, |_, _| Ok("1. Blueberries".into()));
        assert_eq!(o.text, raw, "the words as spoken");
        assert!(o.warning.is_some_and(|w| w.contains("number")));
    }

    // Raw whisper text and the 1.5B model's cleanup, from the owner's history.jsonl on 2026-10-16.
    #[test]
    fn a_cleanup_that_changes_the_meaning_is_not_used() {
        let mut s = Settings::default();
        s.dictionary = ["Arnav", "Xoxoday", "IP whitelist", "Midhun", "Remotify"].map(String::from).to_vec();
        let bad = [
            ("Don't forget to not include the tax in the final invoice.", "Don't forget to include the tax in the final invoice.", "\"not\""),
            ("It's never not worth asking.", "It's never worth asking.", "\"not\""),
            ("Invite everyone, members and non-members.", "Invite everyone.", "\"not\""),
            ("The only thing I'd...", "The only thing I'd say is that we need to work on improving our IP.", "added"),
            ("In five minutes, I was in a new world.", "In five minutes, I'll be in a new world.", "added"),
            ("Ping Arnav about the Zozo Day thing.", "Ping Arnav about the Xoxoday launch.", "added"),
            ("I can't not go to this wedding.", "I can't go to this wedding.", "\"not\""),
            ("Tell Midhun that the", "Tell Midhun that the message is for him.", "added"),
            ("The flight is at 6.5 am on the 3rd, gate 22B.", "The flight is at 6:50 AM on the 3rd, gate 22B.", "number"),
            ("मत भूनना, पाच कुजे तक रिपोर्ट चाहिए.", "मत भूनना, 5:30 कुजे तक रिपोर्ट चाहिए.", "number"),
        ];
        for (raw, cleaned, why) in bad {
            assert!(rejected(&s, raw, cleaned).is_some_and(|w| w.contains(why)), "{raw:?} -> {cleaned:?}");
        }
        let good = [
            ("In 5 minutes I was in a new world.", "In 5 minutes, I was in a new world."),
            ("The meeting has three agenda items. First, budget. Second, hiring. Third, the launch date.", "The meeting has three agenda items:\n1. Budget\n2. Hiring\n3. The launch date"),
            ("Let's meet on Tuesday. No wait Thursday at 3. Actually make it 4.", "Let's meet on Thursday at 4:00."),
            ("Things to buy, um, eggs, uh, milk, two loaves of bread, and like, a dozen bananas.", "Things to buy:\n- Eggs\n- Milk\n- Two loaves of bread\n- A dozen bananas"),
            ("Ping Arnav and Midan about the Zozo Day thing on remotify.", "Ping Arnav and Midan about the Xoxoday thing on Remotify."),
            ("Kal meeting nahi hogi. Parsa hogi. Teen baje call karo. Not 4.", "Kal meeting nahi hogi, Parsa hogi. 3 baje call karo. Not 4."),
            ("कल मीटिंग नहीं होगी, परसो होगी।", "कल मीटिंग नहीं होगी, परसो होगी."),
            ("I am gonna do it, I do not know when", "I'm going to do it, I don't know when."),
            ("Yeah okay.", "OK."),
            ("No.", "No."),
            ("Transfer, 2 lakh 50,000 rupees, no wait, 2 lakh only.", "Transfer ₹2 lakh only."),
            ("मत बूलना, पांच बचे तक रिपोर्ट चाहिए.", "मत बूलना, 5 बचे तक रिपोर्ट चाहिए."),
            ("My number is nine eight seven six five four three two one zero.", "My number is 9876543210."),
        ];
        for (raw, cleaned) in good {
            assert_eq!(rejected(&s, raw, cleaned), None, "{raw:?} -> {cleaned:?}");
        }
        s.translate_to_english = true;
        assert_eq!(rejected(&s, "Kal meeting nahi hogi.", "There is no meeting tomorrow."), None, "a translation may use new words");
        assert!(rejected(&s, "Kal meeting nahi hogi.", "There is a meeting tomorrow.").is_some(), "but keeps its negation");

        s.translate_to_english = false;
        s.cleanup.enabled = true;
        let raw = "Don't forget to not include the tax.";
        let o = after_stt(&s, raw.into(), 1, None, false, true, || {}, |_, _| Ok("Don't forget to include the tax.".into()));
        assert_eq!(o.text, raw, "the words as spoken");
        assert_eq!(o.cleaned.as_deref(), Some("Don't forget to include the tax."), "kept for Use cleaned version");
        assert!(o.warning.is_some_and(|w| w.contains("not")));
        let used = after_stt(&s, raw.into(), 1, None, false, true, || {}, |_, _| Ok("Don't forget to not include the tax!".into()));
        assert_eq!((used.text.as_str(), used.cleaned), ("Don't forget to not include the tax!", None), "an accepted cleanup has no alternative");
    }

    #[test]
    fn long_dictations_split_at_sentence_ends_and_lose_nothing() {
        let text = "One two three. Four five six! Seven eight?\nNine ten. Eleven";
        let p = pieces(text, 6).unwrap();
        assert_eq!(p, [("One two three.", " "), ("Four five six!", " "), ("Seven eight?", "\n"), ("Nine ten. Eleven", " ")]);
        let whole = pieces(text, 1000).unwrap();
        assert_eq!(whole, [(text, " ")], "short enough: one piece");
        assert!(pieces("Mr. Smith and 3.5 kg", 1000).unwrap().len() == 1);
        assert!(pieces("one very long sentence without any full stop at all", 5).unwrap_err().contains("10 words"));
        // 600 words: every word arrives in exactly one piece, each within the budget.
        let sentence = "so we walked down to the river and talked about the plan for next week.";
        let long: String = std::iter::repeat_n(sentence, 600 / 15).collect::<Vec<_>>().join(" ");
        assert_eq!(long.split_whitespace().count(), 600);
        let budget = (CONTEXT_FOR_TEST - 64 - tokens(&system_prompt(&echotype()))) / 2;
        let p = pieces(&long, budget).unwrap();
        assert!(p.len() > 1 && p.iter().all(|(x, _)| tokens(x) <= budget));
        let joined: Vec<&str> = p.iter().flat_map(|(x, _)| x.split_whitespace()).collect();
        assert_eq!(joined, long.split_whitespace().collect::<Vec<_>>());
    }

    const CONTEXT_FOR_TEST: usize = crate::llm::CONTEXT as usize;

    fn echotype() -> Settings {
        let mut s = Settings::default();
        s.cleanup.enabled = true;
        s.cleanup.provider = "echotype".into();
        s
    }

    /// The real sidecar on a long dictation: ECHOTYPE_TEST_TEXT=<600-word .txt>
    /// `cargo test -- --ignored --nocapture long_dictation`. Every piece comes back; nothing is cut.
    #[test]
    #[ignore]
    fn long_dictation_cleans_without_truncation() {
        let Ok(path) = std::env::var("ECHOTYPE_TEST_TEXT") else { return eprintln!("skipped: no ECHOTYPE_TEST_TEXT") };
        let long = std::fs::read_to_string(path).unwrap();
        let s = echotype();
        let budget = (crate::llm::context(&s) as usize - 64 - tokens(&system_prompt(&s))) / 2;
        let t = Instant::now();
        let out = cleanup(&s, long.trim()).unwrap();
        let (n_in, n_out) = (long.split_whitespace().count(), out.split_whitespace().count());
        eprintln!("{n_in} words in {} pieces, {n_out} out, {} ms\n{out}", pieces(long.trim(), budget).unwrap().len(), t.elapsed().as_millis());
        assert!(n_out * 10 >= n_in * 8, "most words survive cleanup");
        let tail: Vec<&str> = long.split_whitespace().rev().take(3).collect();
        assert!(out.trim_end_matches('.').ends_with(&tail.iter().rev().copied().collect::<Vec<_>>().join(" ").trim_end_matches('.').to_string()), "the end is there: {out}");
        crate::llm::stop();
    }

    /// Dictation latency the way the app sees it, for the memory-pressure runs: models loaded once,
    /// two warm dictations, then ECHOTYPE_TEST_IDLE_S seconds idle (an outside memhog.py applies
    /// pressure meanwhile; the cleaner's resident MB is printed every 5 s), then the first dictation
    /// after idle. ECHOTYPE_TEST_WAV=<speech> ECHOTYPE_TEST_MEMORY=save|ready
    /// `cargo test --release -- --ignored --nocapture latency_after_idle`
    #[test]
    #[ignore]
    fn latency_after_idle() {
        let Ok(wav) = std::env::var("ECHOTYPE_TEST_WAV") else { return eprintln!("skipped: no ECHOTYPE_TEST_WAV") };
        let pcm = crate::audio::decode(std::fs::read(wav).unwrap(), Some("wav")).unwrap();
        let mut s = echotype();
        s.advanced = true;
        s.stt.local_model = stt::local_model(&Settings::default());
        s.cleaner_memory = std::env::var("ECHOTYPE_TEST_MEMORY").unwrap_or("save".into());
        let idle: u64 = std::env::var("ECHOTYPE_TEST_IDLE_S").ok().and_then(|v| v.parse().ok()).unwrap_or(30);
        stt::warm_up(&s);
        llm::apply(&s);
        let run = |label: &str| {
            let t = Instant::now();
            let o = run(&s, &pcm, true, || {}).unwrap();
            eprintln!("{label}: stt {} ms, cleanup {} ms, total {} ms{}", o.stt_ms, o.cleanup_ms, t.elapsed().as_millis(), o.warning.map(|w| format!(" ({w})")).unwrap_or_default());
        };
        run("warm 1");
        run("warm 2");
        let server_rss = || {
            let out = std::process::Command::new("pgrep").args(["-P", &std::process::id().to_string(), "llama-server"]).output().unwrap();
            let pid = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let rss = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output().unwrap();
            String::from_utf8_lossy(&rss.stdout).trim().parse::<u64>().unwrap_or(0) / 1024
        };
        eprintln!("IDLE {idle} s, cleaner resident {} MB", server_rss());
        for _ in 0..idle / 5 {
            std::thread::sleep(Duration::from_secs(5));
            eprintln!("  cleaner resident {} MB, free {} MB", server_rss(), stt::ram_mb().1);
        }
        run("first after idle");
        run("warm again");
        llm::stop();
        stt::unload();
    }

    #[test]
    fn spoken_new_line_and_paragraph_become_breaks() {
        assert_eq!(spoken_breaks("a new line b"), "a\nb");
        assert_eq!(spoken_breaks("A. New line. B"), "A.\nB");
        assert_eq!(spoken_breaks("one new paragraph two"), "one\n\ntwo");
        assert_eq!(spoken_breaks("New Paragraph, next"), "\n\nnext");
        assert_eq!(spoken_breaks("renewal linen"), "renewal linen");
        assert_eq!(spoken_breaks("a newline b"), "a newline b");
        // Replacements run first (so a replacement can produce the phrase), then the break rule,
        // and the LLM receives the breaks.
        let mut s = Settings::default();
        s.cleanup.enabled = true;
        s.replacements = vec![Replacement { from: "next".into(), to: "new line".into() }];
        let seen = std::cell::RefCell::new(String::new());
        let out = after_stt(&s, "Hi next there".into(), 1, None, false, true, || {}, |_, t| {
            *seen.borrow_mut() = t.to_string();
            Ok(t.to_string())
        });
        assert_eq!(seen.into_inner(), "Hi\nthere");
        assert_eq!(out.text, "Hi\nthere");
        // A voice note or API upload keeps "new line" as words.
        let file = after_stt(&Settings::default(), "our new line of products".into(), 1, None, false, false, || {}, |_, t| Ok(t.into()));
        assert_eq!(file.text, "our new line of products");
    }

    #[test]
    fn no_cleanup_marks_off_and_keyless_but_not_verbatim_or_failures() {
        let mut s = Settings::default();
        let off = post(&s, "hello");
        assert!(off.no_cleanup && off.text == "hello");
        s.cleanup.enabled = true;
        let keyless = after_stt(&s, "hello".into(), 1, None, true, true, || panic!("called the LLM"), |_, _| panic!("called the LLM"));
        assert!(keyless.no_cleanup && keyless.text == "hello");
        s.cleanup.style = "verbatim".into();
        assert!(!post(&s, "hello").no_cleanup);
        s.cleanup.style = "default".into();
        s.cleanup.provider = "groq".into();
        let failed = after_stt(&s, "hello".into(), 1, None, false, true, || {}, |_, _| Err("401 Unauthorized: bad key\nmore".into()));
        assert!(!failed.no_cleanup && failed.text == "hello");
        assert_eq!(failed.warning.unwrap(), "Cleanup failed: Groq: 401 Unauthorized: bad key. Pasted the raw transcript instead.");
    }
}
