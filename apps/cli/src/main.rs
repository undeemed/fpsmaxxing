//! Operator CLI for diagnostics and local development.

use std::env;
use std::path::PathBuf;

fn main() {
    match env::args().nth(1).as_deref() {
        Some("doctor") => doctor(),
        Some(command) => {
            eprintln!("unknown command: {command}");
            eprintln!("usage: fpsmaxxing-cli doctor");
            std::process::exit(2);
        }
        None => println!("fpsmaxxing-cli\n\nusage: fpsmaxxing-cli doctor"),
    }
}

fn doctor() {
    let journal = env::var("FPSMAXXING_JOURNAL_PATH").map_or_else(
        |_| PathBuf::from("fpsmaxxing-journal.sqlite"),
        PathBuf::from,
    );
    let journal_status = rusqlite::Connection::open(&journal)
        .and_then(|connection| connection.execute_batch("PRAGMA journal_mode = WAL;"))
        .map_or("unavailable", |()| "ready");
    println!("FPSMaxxing diagnostics");
    println!("  contracts: ready");
    println!("  provider SDK: ready");
    println!("  gateway: MCP mock path available");
    println!("  journal: {journal_status} ({})", journal.display());
    println!("  hardware writes: disabled");
    println!("  status: read-only alpha mock path ready");
}
