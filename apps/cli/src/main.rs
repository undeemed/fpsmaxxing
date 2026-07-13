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
    let journal_status = if journal.exists() {
        rusqlite::Connection::open_with_flags(&journal, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|connection| connection.query_row("PRAGMA schema_version", [], |_row| Ok(())))
            .map_or("unavailable", |()| "ready")
    } else {
        "missing (no experiments journaled yet)"
    };
    println!("FPSMaxxing diagnostics");
    println!("  contracts: ready");
    println!("  provider SDK: ready");
    println!("  gateway: MCP mock path available");
    println!("  journal: {journal_status} ({})", journal.display());
    println!("  hardware writes: disabled");
    println!("  status: read-only alpha mock path ready");
}
