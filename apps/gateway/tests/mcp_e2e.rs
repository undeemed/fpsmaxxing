//! End-to-end MCP transport coverage for the gateway's mock path.

use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use rusqlite::Connection;
use serde_json::{Value, json};

fn spawn_gateway(journal: &Path) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fpsmaxxing-gateway"))
        .arg("--journal")
        .arg(journal)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("gateway should start");
    let stdin = child.stdin.take().expect("gateway stdin should exist");
    let stdout = child.stdout.take().expect("gateway stdout should exist");
    (child, stdin, BufReader::new(stdout))
}

fn read_response(reader: &mut impl BufRead) -> Value {
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .expect("response should read");
    serde_json::from_str(&response).expect("response should be JSON")
}

fn roundtrip(stdin: &mut impl Write, reader: &mut impl BufRead, request: &Value) -> Value {
    writeln!(stdin, "{request}").expect("request should write");
    read_response(reader)
}

fn journal_stages(journal: &Path) -> Vec<String> {
    let connection = Connection::open(journal).expect("journal should open");
    connection
        .prepare("SELECT stage FROM experiment_journal ORDER BY sequence")
        .expect("query should prepare")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query should execute")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows should read")
}

#[test]
fn mcp_client_discovers_capabilities_and_runs_the_journaled_lifecycle() {
    let temp = tempfile::tempdir().expect("temporary directory should exist");
    let journal = temp.path().join("journal.sqlite");
    let (mut child, mut stdin, mut reader) = spawn_gateway(&journal);

    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"fpsmaxxing.capabilities","arguments":{}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fpsmaxxing.run_mock_lifecycle","arguments":{"value":42,"lease_seconds":30}}}),
    ] {
        let response = roundtrip(&mut stdin, &mut reader, &request);
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
    assert_eq!(
        journal_stages(&journal),
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

#[test]
fn gateway_survives_protocol_errors_and_ignores_notifications() {
    let temp = tempfile::tempdir().expect("temporary directory should exist");
    let journal = temp.path().join("journal.sqlite");
    let (mut child, mut stdin, mut reader) = spawn_gateway(&journal);

    let initialize = roundtrip(
        &mut stdin,
        &mut reader,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    );
    assert!(
        initialize.get("error").is_none(),
        "unexpected MCP error: {initialize}"
    );

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .expect("notification should write");
    let tools = roundtrip(
        &mut stdin,
        &mut reader,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    assert_eq!(
        tools["id"], 2,
        "notifications must not receive responses: {tools}"
    );

    writeln!(stdin, "this line is not json").expect("malformed line should write");
    let parse_error = read_response(&mut reader);
    assert_eq!(parse_error["error"]["code"], -32700);
    assert_eq!(parse_error["id"], Value::Null);

    let unknown_method = roundtrip(
        &mut stdin,
        &mut reader,
        &json!({"jsonrpc":"2.0","id":3,"method":"resources/list","params":{}}),
    );
    assert_eq!(unknown_method["error"]["code"], -32601);

    let denied = roundtrip(
        &mut stdin,
        &mut reader,
        &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"fpsmaxxing.run_mock_lifecycle","arguments":{"value":101,"lease_seconds":30}}}),
    );
    assert!(
        denied.get("error").is_none(),
        "policy denials must be tool results: {denied}"
    );
    assert_eq!(denied["result"]["isError"], true);

    drop(stdin);
    assert!(child.wait().expect("gateway should exit").success());
    assert!(
        journal_stages(&journal).is_empty(),
        "denied requests must not reach the journal"
    );
}
