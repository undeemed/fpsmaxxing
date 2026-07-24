//! LLM-facing MCP gateway process for the Linux-safe mock path.

use std::{env, io};

fn main() -> Result<(), fpsmaxxing_gateway::GatewayError> {
    let journal_path = env::args()
        .skip(1)
        .collect::<Vec<_>>()
        .windows(2)
        .find(|pair| pair[0] == "--journal")
        .map(|pair| pair[1].clone())
        .or_else(|| env::var("FPSMAXXING_JOURNAL_PATH").ok())
        .unwrap_or_else(|| "fpsmaxxing-journal.sqlite".to_owned());
    let provider = Box::new(fpsmaxxing_mock_provider::MockProvider::new(0));
    let plane = fpsmaxxing_control_plane::ControlPlane::open(provider, journal_path)?;
    fpsmaxxing_gateway::serve(io::stdin().lock(), io::stdout().lock(), plane)
}
