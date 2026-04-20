//! Integration test: verify the full IPC data flow in router mode.
//!
//! 1. Start `hdcd-telegram --router` as a subprocess
//! 2. Send `initialize` on stdin → expect a response
//! 3. Write a message to its inbox file → expect MCP notification on stdout
//! 4. Send `tools/call reply` on stdin → expect outbox file written
//! 5. Send stdin EOF → expect disconnect marker in registration file

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn binary_path() -> PathBuf {
    // cargo test builds to target/debug/
    let mut path = std::env::current_exe()
        .unwrap()
        .parent() // deps/
        .unwrap()
        .parent() // debug/
        .unwrap()
        .to_path_buf();
    path.push("hdcd-telegram");
    if cfg!(windows) {
        path.set_extension("exe");
    }
    path
}

/// Read one JSON line from stdout, with a timeout.
fn read_line_timeout(reader: &mut BufReader<std::process::ChildStdout>, timeout: Duration) -> Option<String> {
    // Use a polling approach since BufReader doesn't support timeouts directly.
    let start = std::time::Instant::now();
    let mut line = String::new();
    loop {
        match reader.read_line(&mut line) {
            Ok(0) => return None, // EOF
            Ok(_) => {
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
                line.clear();
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    if start.elapsed() > timeout {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                return None;
            }
        }
    }
}

#[test]
fn router_mode_ipc_flow() {
    let bin = binary_path();
    if !bin.exists() {
        panic!(
            "binary not found at {}. Run `cargo build --bin hdcd-telegram` first.",
            bin.display()
        );
    }

    // Use a temp dir as router state.
    let state_dir = tempfile::tempdir().unwrap();
    let state_path = state_dir.path();

    // Create required subdirs (ensure_dirs is called by the binary, but
    // we need to set ROUTER_STATE_DIR).
    std::fs::create_dir_all(state_path.join("inbox")).unwrap();
    std::fs::create_dir_all(state_path.join("outbox")).unwrap();
    std::fs::create_dir_all(state_path.join("register")).unwrap();

    // Router mode now requires config.json. This test exercises the IPC
    // file protocol, not a live router peer — `HDCD_SKIP_ROUTER_LAUNCH`
    // (below) prevents the MCP worker from spawning a real hdcd-router.
    std::fs::write(
        state_path.join("config.json"),
        r#"{"bot_token":"test-token","supergroup_id":"-1001234567890"}"#,
    )
    .unwrap();

    // Start the binary in router mode. Strip CLAUDE_CODE_ENTRYPOINT so the
    // VS Code refusal guard doesn't trip when this test is run from inside
    // a Claude Code session that has the env var set.
    let mut child = Command::new(&bin)
        .arg("--router")
        .env("ROUTER_STATE_DIR", state_path.as_os_str())
        .env("RUST_LOG", "hdcd_telegram=debug")
        .env("HDCD_SKIP_SESSION_DISCOVERY", "1")
        .env("HDCD_SKIP_ROUTER_LAUNCH", "1")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn hdcd-telegram --router");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // Give it a moment to start up and write registration.
    std::thread::sleep(Duration::from_millis(500));

    // --- Step 1: Verify registration file was created ---
    let register_dir = state_path.join("register");
    let reg_files: Vec<_> = std::fs::read_dir(&register_dir)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                == Some("json")
        })
        .collect();
    assert_eq!(
        reg_files.len(),
        1,
        "expected exactly one registration file, found {}",
        reg_files.len()
    );

    let reg_path = reg_files[0].path();
    let reg_raw = std::fs::read_to_string(&reg_path).unwrap();
    let reg: serde_json::Value = serde_json::from_str(&reg_raw).unwrap();
    assert_eq!(reg["disconnected"], false);
    let session_id = reg["session_id"].as_str().unwrap().to_string();
    eprintln!("session_id = {session_id}");

    // --- Step 2: Send initialize → expect response ---
    let init_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0.0.1" }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&init_req).unwrap()).unwrap();
    stdin.flush().unwrap();

    let resp_line = read_line_timeout(&mut reader, Duration::from_secs(3))
        .expect("no response to initialize");
    let resp: serde_json::Value = serde_json::from_str(&resp_line).unwrap();
    assert_eq!(resp["id"], 1);
    assert!(resp["result"]["protocolVersion"].is_string());
    assert!(resp["result"]["capabilities"]["experimental"]["claude/channel"].is_object());
    eprintln!("initialize OK: {}", resp["result"]["serverInfo"]);

    // --- Step 3: Send tools/list → check tool schemas ---
    let list_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    writeln!(stdin, "{}", serde_json::to_string(&list_req).unwrap()).unwrap();
    stdin.flush().unwrap();

    let resp_line = read_line_timeout(&mut reader, Duration::from_secs(3))
        .expect("no response to tools/list");
    let resp: serde_json::Value = serde_json::from_str(&resp_line).unwrap();
    assert_eq!(resp["id"], 2);
    let tool_names: Vec<&str> = resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(tool_names.contains(&"reply"), "missing reply tool");
    assert!(tool_names.contains(&"react"), "missing react tool");
    assert!(tool_names.contains(&"edit_message"), "missing edit_message tool");
    eprintln!("tools/list OK: {tool_names:?}");

    // --- Step 4: Write to inbox → expect MCP notification on stdout ---
    let inbox_path = state_path
        .join("inbox")
        .join(format!("{session_id}.jsonl"));
    let inbox_msg = serde_json::json!({
        "text": "hello from test",
        "user": "tester",
        "user_id": "999",
        "chat_id": "-100999",
        "message_id": 42,
        "ts": "2026-04-11T16:00:00Z"
    });
    let mut inbox_line = serde_json::to_string(&inbox_msg).unwrap();
    inbox_line.push('\n');
    std::fs::write(&inbox_path, &inbox_line).unwrap();

    // Wait for the inbox poller (500ms interval) + processing time.
    let notification = read_line_timeout(&mut reader, Duration::from_secs(3))
        .expect("no notification from inbox message");
    let notif: serde_json::Value = serde_json::from_str(&notification).unwrap();
    assert_eq!(notif["method"], "notifications/claude/channel");
    assert_eq!(notif["params"]["content"], "hello from test");
    assert_eq!(notif["params"]["meta"]["user"], "tester");
    assert_eq!(notif["params"]["meta"]["message_id"], "42");
    eprintln!("inbox → notification OK");

    // --- Step 5: Send tools/call reply → expect outbox file ---
    let reply_req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "reply",
            "arguments": {
                "chat_id": "-100999",
                "text": "hello back from claude"
            }
        }
    });
    writeln!(stdin, "{}", serde_json::to_string(&reply_req).unwrap()).unwrap();
    stdin.flush().unwrap();

    let resp_line = read_line_timeout(&mut reader, Duration::from_secs(3))
        .expect("no response to reply tool call");
    let resp: serde_json::Value = serde_json::from_str(&resp_line).unwrap();
    assert_eq!(resp["id"], 3);
    let reply_text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        reply_text.contains("via router"),
        "expected 'via router' in response, got: {reply_text}"
    );
    eprintln!("reply tool call OK: {reply_text}");

    // Verify outbox file was written.
    let outbox_path = state_path
        .join("outbox")
        .join(format!("{session_id}.jsonl"));
    assert!(outbox_path.exists(), "outbox file not created");
    let outbox_content = std::fs::read_to_string(&outbox_path).unwrap();
    let outbox_msg: serde_json::Value =
        serde_json::from_str(outbox_content.trim()).unwrap();
    assert_eq!(outbox_msg["text"], "hello back from claude");
    eprintln!("outbox file OK: {}", outbox_msg["text"]);

    // --- Step 6: Close stdin → expect disconnect marker ---
    drop(stdin);

    // Wait for the process to exit (with manual timeout).
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait().unwrap() {
            Some(s) => break s,
            None => {
                if start.elapsed() > Duration::from_secs(5) {
                    child.kill().unwrap();
                    panic!("process did not exit after stdin close within 5s");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    assert!(status.success(), "process exited with non-zero status: {status}");

    // Check disconnect marker.
    let reg_raw2 = std::fs::read_to_string(&reg_path).unwrap();
    let reg2: serde_json::Value = serde_json::from_str(&reg_raw2).unwrap();
    assert_eq!(reg2["disconnected"], true);
    assert_eq!(reg2["session_id"].as_str().unwrap(), session_id);
    eprintln!("disconnect marker OK");

    eprintln!("\n=== ALL CHECKS PASSED ===");
}
