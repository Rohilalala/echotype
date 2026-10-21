//! `echotype --mcp`: a read-only MCP server on stdin/stdout (JSON-RPC 2.0, one message a line), so
//! Claude Desktop, Cursor and other AI apps can look through what Echotype keeps on this computer:
//! dictation history, meeting notes, Notes, the Dictionary and usage stats. It reads the files itself, so
//! the app need not be running, and it never writes them.

use crate::meeting::{self, Meeting};
use crate::notes;
use crate::settings::{self, HistoryEntry, Settings};
use crate::stats;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// The newest MCP revision this speaks; a client asking for an older one listed here gets that one.
const PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_LIMIT: u64 = 200;

/// Where the files are: settings::data_dir and config_dir (ECHOTYPE_CONFIG_DIR moves both).
struct Dirs {
    data: PathBuf,
    config: PathBuf,
}

/// Answers each request line until stdin closes. Returns the exit code.
pub fn serve(input: impl BufRead, mut out: impl Write) -> i32 {
    let dirs = Dirs { data: settings::data_dir(), config: settings::config_dir() };
    for line in input.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle(&dirs, &line) {
            if writeln!(out, "{reply}").and_then(|()| out.flush()).is_err() {
                break; // the client went away
            }
        }
    }
    0
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The reply to one message; None for a notification (or a client's reply), which gets none.
fn handle(dirs: &Dirs, line: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Some(error(Value::Null, -32700, &format!("Parse error: {e}"))),
    };
    let id = msg.get("id").filter(|id| !id.is_null()).cloned()?;
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return (msg.get("result").is_none() && msg.get("error").is_none()).then(|| error(id, -32600, "Invalid request"));
    };
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str);
            let version = PROTOCOLS.iter().find(|v| Some(**v) == asked).unwrap_or(&PROTOCOLS[0]);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "echotype", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "Read-only access to Echotype, the user's voice dictation app: what they dictated (history), their meeting notes, their Notes (a scratchpad), their Dictionary and their usage stats.",
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call(dirs, &params),
        _ => Err((-32601, format!("Method not found: {method}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => error(id, code, &message),
    })
}

fn tools() -> Value {
    let text = |d: &str| json!({ "type": "string", "description": d });
    let limit = |def: u64| json!({ "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": format!("At most this many (default {def}).") });
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": { "type": "object", "properties": properties, "required": required },
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
        })
    };
    json!([
        tool("search_history", "Search what the user dictated with Echotype, newest first. Every word of the query must appear (any case).", json!({ "query": text("The words to find."), "limit": limit(20) }), &["query"]),
        tool("recent_dictations", "The user's latest dictations with Echotype, newest first.", json!({ "limit": limit(10) }), &[]),
        tool("search_meetings", "Search the user's Echotype meeting notes (title, transcript and summary). Every word must appear. Returns meeting ids with the matching lines.", json!({ "query": text("The words to find.") }), &["query"]),
        tool("get_meeting_transcript", "One meeting's notes as Markdown: title, length, summary and the transcript by speaker.", json!({ "id": text("A meeting id from search_meetings.") }), &["id"]),
        tool("search_notes", "Search the user's Echotype Notes (their scratchpad), newest edit first. Every word must appear (any case); an empty query lists them all.", json!({ "query": text("The words to find."), "limit": limit(20) }), &["query"]),
        tool("dictionary_words", "The user's Echotype Dictionary (names and terms they taught it) and their replacement rules.", json!({}), &[]),
        tool("stats", "The user's dictation stats: words today, this week and in total, dictations, speaking speed and streaks.", json!({}), &[]),
    ])
}

/// tools/call. A tool that runs but fails (no such meeting) is a result with isError, as MCP asks.
fn call(dirs: &Dirs, params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).ok_or((-32602, "Missing tool name".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let query = || args.get("query").and_then(Value::as_str).unwrap_or_default();
    let limit = |def: u64| args.get("limit").and_then(Value::as_u64).unwrap_or(def).clamp(1, MAX_LIMIT) as usize;
    let out = match name {
        "search_history" => Ok(json!(search_history(&dirs.data, query(), limit(20)))),
        "recent_dictations" => Ok(json!(search_history(&dirs.data, "", limit(10)))),
        "search_meetings" => Ok(json!(search_meetings(&dirs.data, query()))),
        "get_meeting_transcript" => meeting_markdown(&dirs.data, args.get("id").and_then(Value::as_str).unwrap_or_default()).map(Value::String),
        "search_notes" => search_notes(&dirs.data, query(), limit(20)),
        "dictionary_words" => Ok(dictionary(&dirs.config)),
        "stats" => Ok(usage(&dirs.data)),
        _ => return Err((-32602, format!("Unknown tool: {name}"))),
    };
    let (text, failed) = match out {
        Ok(Value::String(s)) => (s, false),
        Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
        Err(e) => (e, true),
    };
    Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": failed }))
}

/// Local time as "2026-10-22 14:03".
fn when(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

fn fold(s: &str) -> String {
    s.to_lowercase()
}

/// Dictations holding every word of `query` (all of them for ""), newest first. A recovered
/// dictation not transcribed yet has no text, so it is left out.
// ponytail: reads all of history.jsonl (500 entries at most) per call.
fn search_history(data: &Path, query: &str, limit: usize) -> Vec<Value> {
    let words: Vec<String> = query.split_whitespace().map(fold).collect();
    let file = std::fs::read_to_string(data.join("history.jsonl")).unwrap_or_default();
    file.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<HistoryEntry>(l).ok())
        .filter(|e| !e.text.is_empty() && words.iter().all(|w| fold(&e.text).contains(w.as_str())))
        .take(limit)
        .map(|e| json!({ "when": when(e.ts), "ts": e.ts, "text": e.text }))
        .collect()
}

fn meetings(data: &Path) -> Vec<Meeting> {
    let Ok(rd) = std::fs::read_dir(data.join("meetings")) else { return vec![] };
    rd.flatten().filter_map(|e| serde_json::from_slice(&std::fs::read(e.path().join("meeting.json")).ok()?).ok()).collect()
}

fn search_meetings(data: &Path, query: &str) -> Vec<Value> {
    let words: Vec<String> = query.split_whitespace().map(fold).collect();
    let mut hits: Vec<(u64, Value)> = meetings(data)
        .into_iter()
        .filter_map(|m| {
            let lines = meeting::search_meeting(&m, &words)?;
            let lines: Vec<Value> = lines.into_iter().map(|s| json!({ "at": meeting::clock(s.start), "text": s.text })).collect();
            Some((m.started_ms, json!({ "id": m.id, "title": m.title, "when": when(m.started_ms), "lines": lines })))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.into_iter().map(|(_, v)| v).collect()
}

/// Meeting `id`'s notes. Ids are the folder names Echotype made, so nothing else is read.
fn meeting_markdown(data: &Path, id: &str) -> Result<String, String> {
    let ok = !id.is_empty() && id.len() <= 80 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let bytes = std::fs::read(data.join("meetings").join(id).join("meeting.json")).ok().filter(|_| ok).ok_or_else(|| format!("No meeting with id {id:?}. Use search_meetings to find one."))?;
    let m: Meeting = serde_json::from_slice(&bytes).map_err(|e| format!("That meeting's file is damaged: {e}"))?;
    Ok(format!("{}\nStarted {}\n", meeting::markdown(&m).trim_end(), when(m.started_ms)))
}

fn search_notes(data: &Path, query: &str, limit: usize) -> Result<Value, String> {
    let all = notes::load(data)?;
    let hits = notes::search(&all, query).into_iter().take(limit);
    Ok(json!(hits.map(|n| json!({ "title": notes::title(&n.text), "edited": when(n.updated_ms), "text": n.text })).collect::<Vec<_>>()))
}

fn dictionary(config: &Path) -> Value {
    let mut s: Settings = std::fs::read_to_string(config.join("settings.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    s.migrate();
    json!({ "words": s.dictionary, "replacements": s.replacements.iter().map(|r| json!({ "from": r.from, "to": r.to })).collect::<Vec<_>>() })
}

fn usage(data: &Path) -> Value {
    let v = stats::peek(&data.join("stats.json"), 1);
    json!({
        "words_today": v.words_today,
        "words_this_week": v.words_week,
        "words_total": v.words_total,
        "dictations_total": v.count_total,
        "average_wpm": v.avg_wpm.map(f64::round),
        "average_wpm_this_week": v.avg_wpm_week.map(f64::round),
        "best_wpm": v.best_wpm.map(|b| json!({ "date": b.date, "wpm": b.value.round() })),
        "best_day": v.best_day.map(|b| json!({ "date": b.date, "words": b.value })),
        "streak_days": v.streak,
        "longest_streak_days": v.longest_streak,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> Dirs {
        let dir = std::env::temp_dir().join(format!("echotype-mcp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("meetings/2026-10-20-090000-standup")).unwrap();
        let entry = |ts: u64, text: &str| serde_json::to_string(&HistoryEntry { ts, raw: text.to_lowercase(), text: text.into(), stt_ms: 1, cleanup_ms: 0, stt_provider: "local".into(), cleanup_provider: None, no_cleanup: false, cleaned: None, original: None, recovered: false }).unwrap();
        let recovered = serde_json::to_string(&HistoryEntry { recovered: true, ..serde_json::from_str(&entry(4000, "")).unwrap() }).unwrap();
        std::fs::write(dir.join("history.jsonl"), [entry(1000, "Ping Midhun about the invoice"), entry(2000, "Lunch at noon?"), entry(3000, "Send the invoice to Arnav"), recovered].join("\n") + "\n").unwrap();
        let m = Meeting {
            id: "2026-10-20-090000-standup".into(),
            title: "Standup".into(),
            started_ms: 5000,
            duration_s: 65.0,
            state: "done".into(),
            segments: vec![meeting::Segment { start: 3.0, end: 5.0, speaker: "you".into(), text: "The release ships Friday.".into() }],
            speakers: [("you".to_string(), "You".to_string())].into(),
            summary: Some("## Summary\nRelease on Friday.".into()),
            ..Default::default()
        };
        std::fs::write(dir.join("meetings/2026-10-20-090000-standup/meeting.json"), serde_json::to_vec(&m).unwrap()).unwrap();
        let s = Settings { dictionary: vec!["Xoxoday".into()], replacements: vec![settings::Replacement { from: "echo type".into(), to: "Echotype".into() }], ..Default::default() };
        std::fs::write(dir.join("settings.json"), serde_json::to_string(&s).unwrap()).unwrap();
        Dirs { data: dir.clone(), config: dir }
    }

    fn rpc(dirs: &Dirs, method: &str, params: Value) -> Value {
        handle(dirs, &json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }).to_string()).unwrap()
    }

    /// A tools/call's text, parsed as JSON when it is.
    fn tool(dirs: &Dirs, name: &str, args: Value) -> (Value, bool) {
        let r = rpc(dirs, "tools/call", json!({ "name": name, "arguments": args }));
        let text = r["result"]["content"][0]["text"].as_str().unwrap().to_string();
        (serde_json::from_str(&text).unwrap_or(Value::String(text)), r["result"]["isError"].as_bool().unwrap())
    }

    #[test]
    fn json_rpc_initialize_list_ping_and_errors() {
        let d = scratch("rpc");
        let init = rpc(&d, "initialize", json!({ "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } }));
        assert_eq!((init["id"].as_i64(), init["result"]["protocolVersion"].as_str()), (Some(7), Some("2025-03-26")), "the client's version when it is one we speak");
        assert_eq!(init["result"]["serverInfo"]["name"], "echotype");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        assert_eq!(rpc(&d, "initialize", json!({ "protocolVersion": "1999-01-01" }))["result"]["protocolVersion"], PROTOCOLS[0], "otherwise ours");
        let names: Vec<String> = rpc(&d, "tools/list", json!({}))["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, ["search_history", "recent_dictations", "search_meetings", "get_meeting_transcript", "search_notes", "dictionary_words", "stats"]);
        assert_eq!(rpc(&d, "ping", json!({}))["result"], json!({}));
        assert_eq!(rpc(&d, "resources/list", json!({}))["error"]["code"], -32601);
        assert_eq!(rpc(&d, "tools/call", json!({ "name": "delete_history" }))["error"]["code"], -32602);
        assert_eq!(handle(&d, "{not json").unwrap()["error"]["code"], -32700);
        assert_eq!(handle(&d, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), None, "notifications get no reply");
        assert_eq!(handle(&d, r#"{"jsonrpc":"2.0","id":3,"result":{}}"#), None, "nor do replies");
        let mut out = vec![];
        let input = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":\"b\",\"method\":\"ping\"}\n";
        assert_eq!(serve(input.as_bytes(), &mut out), 0);
        let lines: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.iter().map(|l| l["id"].clone()).collect::<Vec<_>>(), [json!(1), json!("b")], "one line per request, in order");
    }

    #[test]
    fn history_tools_search_newest_first_and_skip_untranscribed_audio() {
        let d = scratch("history");
        let (hits, err) = tool(&d, "search_history", json!({ "query": "INVOICE" }));
        assert!(!err);
        let texts: Vec<&str> = hits.as_array().unwrap().iter().map(|h| h["text"].as_str().unwrap()).collect();
        assert_eq!(texts, ["Send the invoice to Arnav", "Ping Midhun about the invoice"]);
        assert_eq!(tool(&d, "search_history", json!({ "query": "invoice midhun" })).0.as_array().unwrap().len(), 1, "every word");
        assert_eq!(tool(&d, "search_history", json!({ "query": "invoice", "limit": 1 })).0[0]["ts"], 3000);
        let (recent, _) = tool(&d, "recent_dictations", json!({ "limit": 2 }));
        assert_eq!(recent.as_array().unwrap().iter().map(|h| h["ts"].as_u64().unwrap()).collect::<Vec<_>>(), [3000, 2000], "the recovered entry has no text yet");
        assert!(recent[0]["when"].as_str().unwrap().len() >= 16);
    }

    #[test]
    fn meeting_tools_find_and_read_notes_and_refuse_other_paths() {
        let d = scratch("meetings");
        let (hits, _) = tool(&d, "search_meetings", json!({ "query": "friday release" }));
        assert_eq!(hits[0]["id"], "2026-10-20-090000-standup");
        assert_eq!(hits[0]["lines"][0], json!({ "at": "00:03", "text": "The release ships Friday." }));
        assert_eq!(tool(&d, "search_meetings", json!({ "query": "budget" })).0, json!([]));
        let (notes, err) = tool(&d, "get_meeting_transcript", json!({ "id": "2026-10-20-090000-standup" }));
        let notes = notes.as_str().unwrap();
        assert!(!err && notes.starts_with("# Standup") && notes.contains("**You** (00:03): The release ships Friday.") && notes.contains("Release on Friday."));
        for bad in ["nope", "../meetings/2026-10-20-090000-standup", ""] {
            assert!(tool(&d, "get_meeting_transcript", json!({ "id": bad })).1, "{bad:?} is a tool error");
        }
    }

    #[test]
    fn notes_tool_searches_without_writing() {
        let d = scratch("notes");
        assert_eq!(tool(&d, "search_notes", json!({ "query": "" })), (json!([]), false), "no notes.json: none");
        notes::save(&d.data, None, "Launch plan\nShip Friday", false, 1000).unwrap();
        notes::save(&d.data, None, "Groceries", false, 2000).unwrap();
        let before = std::fs::read(d.data.join("notes.json")).unwrap();
        let (hits, err) = tool(&d, "search_notes", json!({ "query": "FRIDAY" }));
        assert!(!err);
        assert_eq!((hits[0]["title"].as_str(), hits[0]["text"].as_str(), hits.as_array().unwrap().len()), (Some("Launch plan"), Some("Launch plan\nShip Friday"), 1));
        assert_eq!(tool(&d, "search_notes", json!({ "query": "", "limit": 1 })).0[0]["title"], "Groceries", "newest edit first");
        assert_eq!(std::fs::read(d.data.join("notes.json")).unwrap(), before);
    }

    #[test]
    fn dictionary_and_stats_tools() {
        let d = scratch("dict");
        let (dict, _) = tool(&d, "dictionary_words", json!({}));
        assert_eq!(dict, json!({ "words": ["Xoxoday"], "replacements": [{ "from": "echo type", "to": "Echotype" }] }));
        let (st, err) = tool(&d, "stats", json!({}));
        assert!(!err);
        assert_eq!((st["words_total"].as_u64(), st["streak_days"].as_u64()), (Some(0), Some(0)), "no stats.json yet: zeros, and nothing written");
        assert!(!d.data.join("stats.json").exists());
    }
}
