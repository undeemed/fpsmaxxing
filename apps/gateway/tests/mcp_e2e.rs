//! End-to-end MCP transport coverage for the gateway's mock path.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

use rusqlite::Connection;
use serde_json::{Value, json};

#[test]
fn mcp_client_discovers_capabilities_and_runs_the_journaled_lifecycle() {
    let temp = tempfile::tempdir().expect("temporary directory should exist");
    let journal = temp.path().join("journal.sqlite");
    let mut child = Command::new(env!("CARGO_BIN_EXE_fpsmaxxing-gateway"))
        .arg("--journal")
        .arg(&journal)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("gateway should start");
    let mut stdin = child.stdin.take().expect("gateway stdin should exist");
    let stdout = child.stdout.take().expect("gateway stdout should exist");
    let mut reader = BufReader::new(stdout);

    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fpsmaxxing.capabilities","arguments":{}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fpsmaxxing.run_mock_lifecycle","arguments":{"value":42,"lease_seconds":30}}}),
    ] {
        writeln!(stdin, "{request}").expect("request should write");
        let mut response = String::new();
        reader
            .read_line(&mut response)
            .expect("response should read");
        let response: Value = serde_json::from_str(&response).expect("response should be JSON");
        assert!(
            response.get("error").is_none(),
            "unexpected MCP error: {response}"
        );
        if response["id"] == 2 {
            assert_eq!(
                response["result"]["tools"]
                    .as_array()
                    .expect("tools array")
                    .len(),
                2
            );
        }
        if response["id"] == 3 {
            assert!(
                response["result"]["content"][0]["text"]
                    .as_str()
                    .expect("text")
                    .contains("mock.value")
            );
        }
        if response["id"] == 4 {
            assert!(
                response["result"]["content"][0]["text"]
                    .as_str()
                    .expect("text")
                    .contains("rolled_back")
            );
        }
    }
    drop(stdin);
    assert!(child.wait().expect("gateway should exit").success());
    let connection = Connection::open(journal).expect("journal should open");
    let stages = connection
        .prepare("SELECT stage FROM experiment_journal ORDER BY sequence")
        .expect("query should prepare")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query should execute")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows should read");
    assert_eq!(
        stages,
        [
            "snapshot",
            "preview",
            "apply",
            "verify",
            "rollback",
            "rollback-verify"
        ]
    );
}
