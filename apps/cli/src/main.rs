//! Operator CLI for diagnostics and local development.

use std::env;

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
    println!("FPSMaxxing scaffold diagnostics");
    println!("  contracts: ready");
    println!("  provider SDK: ready");
    println!("  hardware writes: disabled");
    println!("  status: architecture scaffold only");
}
