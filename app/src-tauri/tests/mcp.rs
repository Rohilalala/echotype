//! `echotype --mcp` end to end: the built binary, over pipes, against a temp data folder
//! (ECHOTYPE_CONFIG_DIR), the way Claude Desktop or Cursor runs it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn the_binary_answers_initialize_tools_list_and_a_tool_call_over_stdio() {
    let dir = std::env::temp_dir().join(format!("echotype-mcp-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let entry = |ts: u64, text: &str| format!(r#"{{"ts":{ts},"raw":"{text}","text":"{text}","stt_ms":1,"cleanup_ms":0,"stt_provider":"local","cleanup_provider":null}}"#);
    std::fs::write(dir.join("history.jsonl"), format!("{}\n{}\n", entry(1, "Book the flight to Goa"), entry(2, "Call the bank"))).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_echotype"))
        .arg("--mcp")
        .env("ECHOTYPE_CONFIG_DIR", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ask = |msg: &str| -> serde_json::Value {
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON-RPC ({e}): {line:?}"))
    };

    let init = ask(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#);
    assert_eq!(init["result"]["serverInfo"]["name"], "echotype");
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    // A notification gets no reply, so the next line read is the answer to tools/list.
    let tools = ask("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}");
    assert_eq!(tools["id"], 2);
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 7);
    let call = ask(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_history","arguments":{"query":"flight"}}}"#);
    assert_eq!(call["result"]["isError"], false);
    let hits: serde_json::Value = serde_json::from_str(call["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["text"], "Book the flight to Goa");

    drop(stdin); // the client closes the pipe: the server exits cleanly
    assert!(child.wait().unwrap().success());
    let files: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(files, ["history.jsonl"], "read-only: nothing written");
}
