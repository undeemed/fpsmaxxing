//! Repository automation entrypoint.

use std::{env, process::Command};

fn main() {
    match env::args().nth(1).as_deref() {
        Some("check") => run("cargo", &["check", "--workspace"]),
        Some("test") => run("cargo", &["test", "--workspace"]),
        Some(command) => {
            eprintln!("unknown xtask: {command}");
            std::process::exit(2);
        }
        None => println!("usage: cargo xtask <check|test>"),
    }
}

fn run(program: &str, args: &[&str]) {
    let status = Command::new(program)
        .args(args)
        .status()
        .unwrap_or_else(|error| panic!("failed to launch {program}: {error}"));
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}
