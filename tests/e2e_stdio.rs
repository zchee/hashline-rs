// Copyright 2026 The hashline-rs Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//! End-to-end transport test: the real `hashline-mcp` binary over stdio.
//!
//! Drives newline-delimited JSON-RPC through a spawned child process —
//! initialize, tools/list, and a write → glob → grep → read → edit
//! round-trip — and asserts that stdout carries only MCP frames (R018).

use std::{process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

const IO_DEADLINE: Duration = Duration::from_secs(20);

struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl StdioClient {
    async fn spawn(root: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hashline-mcp"))
            .arg("--root")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn hashline-mcp");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 0,
        }
    }

    async fn send(&mut self, message: Value) {
        let mut frame = message.to_string();
        frame.push('\n');
        timeout(IO_DEADLINE, self.stdin.write_all(frame.as_bytes()))
            .await
            .expect("stdin write within deadline")
            .expect("stdin write");
    }

    /// Send a request and return its matching response, asserting along the
    /// way that every stdout line is a parseable JSON object (R018: stdout
    /// carries MCP transport only, diagnostics stay on stderr).
    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let line = timeout(IO_DEADLINE, self.lines.next_line())
                .await
                .expect("stdout read within deadline")
                .expect("stdout read")
                .expect("stdout open until response arrives");
            if line.is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("non-JSON frame on stdout: {error}: {line:?}"));
            assert!(message.is_object(), "non-object frame on stdout: {line:?}");
            if message.get("id") == Some(&json!(id)) {
                assert!(
                    message.get("error").is_none(),
                    "JSON-RPC error response: {message}"
                );
                return message["result"].clone();
            }
        }
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> (bool, String) {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("text content expected: {result}"))
            .to_owned();
        (is_error, text)
    }
}

fn parse_json_payload(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("structured JSON: {error}: {text}"))
}

#[tokio::test]
async fn stdio_round_trip_covers_all_five_tools() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let mut client = StdioClient::spawn(tmp.path()).await;

    let init = client
        .request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "hashline-e2e", "version": "0.0.0"}
            }),
        )
        .await;
    assert_eq!(init["serverInfo"]["name"], "hashline", "{init}");
    client
        .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;

    let listing = client.request("tools/list", json!({})).await;
    let tools = listing["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(
        names,
        ["read", "edit", "write", "grep", "glob"],
        "{listing}"
    );
    for tool in tools {
        let annotations = &tool["annotations"];
        assert_eq!(
            annotations["openWorldHint"],
            json!(false),
            "{}",
            tool["name"]
        );
        match tool["name"].as_str().expect("tool name") {
            "read" | "grep" | "glob" => {
                assert_eq!(annotations["readOnlyHint"], json!(true), "{}", tool["name"]);
            }
            "edit" | "write" => {
                assert_eq!(
                    annotations["destructiveHint"],
                    json!(true),
                    "{}",
                    tool["name"]
                );
            }
            other => panic!("unexpected tool {other}"),
        }
    }

    // write: exclusive create.
    let (is_error, text) = client
        .call_tool(
            "write",
            json!({
                "file_path": "src/demo.rs",
                "content": "alpha\nbeta\ngamma\n",
                "expect": "absent"
            }),
        )
        .await;
    assert!(!is_error, "{text}");
    let created = parse_json_payload(&text);
    assert_eq!(created["created"], json!(true), "{text}");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("src/demo.rs")).expect("created file"),
        "alpha\nbeta\ngamma\n"
    );

    // glob: the new file is discoverable.
    let (is_error, text) = client
        .call_tool("glob", json!({"pattern": "**/*.rs"}))
        .await;
    assert!(!is_error, "{text}");
    assert!(text.contains("src/demo.rs"), "{text}");
    assert!(
        text.contains("[hashline files=1 truncated=false]"),
        "{text}"
    );

    // grep discovery mode: the file set without content.
    let (is_error, text) = client
        .call_tool(
            "grep",
            json!({"pattern": "beta", "output_mode": "files_with_matches"}),
        )
        .await;
    assert!(!is_error, "{text}");
    assert!(text.contains("src/demo.rs"), "{text}");
    assert!(!text.contains("[hashline snapshot="), "{text}");

    // read: harvest the snapshot and positions for the edit.
    let (is_error, text) = client
        .call_tool("read", json!({"path": "src/demo.rs", "start_line": 2}))
        .await;
    assert!(!is_error, "{text}");
    let header = text.lines().next().expect("snapshot header");
    let snapshot = header
        .split_whitespace()
        .find_map(|part| part.strip_prefix("snapshot="))
        .expect("snapshot field");
    let start = text
        .lines()
        .find(|line| line.starts_with("2@"))
        .expect("line 2 rendered")
        .split('|')
        .next()
        .expect("position token");
    let end = text
        .lines()
        .find(|line| line.starts_with("3@"))
        .expect("line 3 rendered")
        .split('|')
        .next()
        .expect("position token");

    // edit: replace line 2 against the exact snapshot the read named.
    let (is_error, text) = client
        .call_tool(
            "edit",
            json!({
                "file_path": "src/demo.rs",
                "snapshot": snapshot,
                "edits": [{"op": "replace", "start": start, "end": end, "content": "BETA\n"}]
            }),
        )
        .await;
    assert!(!is_error, "{text}");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("src/demo.rs")).expect("edited file"),
        "alpha\nBETA\ngamma\n"
    );

    // Stale write must fail closed now that the bytes changed.
    let (is_error, text) = client
        .call_tool(
            "write",
            json!({
                "file_path": "src/demo.rs",
                "content": "clobber\n",
                "expect": snapshot
            }),
        )
        .await;
    assert!(is_error, "stale overwrite must conflict: {text}");
    let conflict = parse_json_payload(&text);
    assert_eq!(
        conflict["error"]["code"],
        json!("snapshot_conflict"),
        "{text}"
    );

    drop(client.stdin);
    let status = timeout(IO_DEADLINE, client.child.wait())
        .await
        .expect("child exit within deadline")
        .expect("child wait");
    assert!(status.success(), "clean shutdown on closed stdin: {status}");
}

/// A client negotiating protocol revision `2026-07-28` validates `tools/list`
/// against a schema where SEP-2549's `ttlMs` and `cacheScope` are required;
/// omitting either one costs the session every hashline tool.
#[tokio::test]
async fn tools_list_carries_cache_directives_on_the_modern_revision() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let mut client = StdioClient::spawn(tmp.path()).await;

    let init = client
        .request(
            "initialize",
            json!({
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": {"name": "hashline-e2e", "version": "0.0.0"}
            }),
        )
        .await;
    assert_eq!(init["protocolVersion"], json!("2026-07-28"), "{init}");
    client
        .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;

    let listing = client.request("tools/list", json!({})).await;
    assert_eq!(listing["resultType"], json!("complete"), "{listing}");
    assert!(
        listing["ttlMs"].is_u64(),
        "ttlMs must be a number: {listing}"
    );
    assert!(
        matches!(listing["cacheScope"].as_str(), Some("public" | "private")),
        "{listing}"
    );

    drop(client.stdin);
    let status = timeout(IO_DEADLINE, client.child.wait())
        .await
        .expect("child exit within deadline")
        .expect("child wait");
    assert!(status.success(), "clean shutdown on closed stdin: {status}");
}
