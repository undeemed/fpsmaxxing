//! Operator CLI for diagnostics and local development.

use std::env;
use std::path::{Path, PathBuf};

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
        journal_report(&journal)
    } else {
        "missing (no experiments journaled yet)".to_owned()
    };
    println!("FPSMaxxing diagnostics");
    println!("  contracts: ready");
    println!("  provider SDK: ready");
    println!("  gateway: MCP mock path available");
    println!("  journal: {journal_status} ({})", journal.display());
    println!("  hardware writes: disabled");
    println!("  status: read-only alpha mock path ready");
}

fn journal_report(journal: &Path) -> String {
    let dangling =
        rusqlite::Connection::open_with_flags(journal, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .and_then(|connection| {
                let tables: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'table' AND name = 'experiment_journal'",
                    [],
                    |row| row.get(0),
                )?;
                if tables == 0 {
                    return Ok(None);
                }
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM (
                             SELECT experiment_id FROM experiment_journal
                             GROUP BY experiment_id
                             HAVING SUM(stage = 'apply-intent') > 0
                                AND SUM(stage IN ('completed', 'failed')) = 0
                         )",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map(Some)
            });
    match dangling {
        Ok(Some(0)) => "ready".to_owned(),
        Ok(Some(count)) => {
            format!("ready ({count} dangling experiment(s) missing a terminal outcome)")
        }
        Ok(None) => "unavailable (experiment_journal table missing)".to_owned(),
        Err(error) => format!("unavailable ({error})"),
    }
}
