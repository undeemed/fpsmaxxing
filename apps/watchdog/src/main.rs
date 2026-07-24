//! Independent lease and safety watchdog process for the Linux-safe mock path.
//!
//! The process runs an interval-driven scan (`tick`) that restores any leaked
//! experiment through its provider. `tick` is deliberately a single blocking
//! call so a later Windows service wrapper can drive it from a service timer
//! without changing the recovery logic. This build wires the Linux-safe mock
//! provider; a real deployment wires the provider that owns the leaked knob.

use std::{env, path::PathBuf, thread, time::Duration};

use fpsmaxxing_mock_provider::MockProvider;
use fpsmaxxing_watchdog::{ReclaimPolicy, ReclaimReason, Restoration, Watchdog, WatchdogError};

const DEFAULT_JOURNAL: &str = "fpsmaxxing-journal.sqlite";
const DEFAULT_INTERVAL_SECONDS: u64 = 5;

fn main() -> Result<(), WatchdogError> {
    let config = Config::from_args(env::args().skip(1));
    let mut watchdog = Watchdog::open(MockProvider::new(0), &config.journal_path)?;
    let policy = config.policy();

    if config.once {
        return tick(&mut watchdog, policy);
    }

    println!(
        "fpsmaxxing-watchdog: polling {} every {}s ({})",
        config.journal_path.display(),
        config.interval.as_secs(),
        policy_label(policy),
    );
    loop {
        if let Err(error) = tick(&mut watchdog, policy) {
            // A transient journal lock must not take the safety watchdog down;
            // log and retry on the next interval.
            eprintln!("fpsmaxxing-watchdog: reclaim failed, retrying next interval: {error}");
        }
        thread::sleep(config.interval);
    }
}

/// One scan-and-restore pass. This is the unit a service timer would call.
fn tick(watchdog: &mut Watchdog<MockProvider>, policy: ReclaimPolicy) -> Result<(), WatchdogError> {
    for restoration in watchdog.reclaim(policy)? {
        report(&restoration);
    }
    Ok(())
}

fn report(restoration: &Restoration) {
    if restoration.restored {
        println!(
            "fpsmaxxing-watchdog: restored experiment {} ({}, lease {}s)",
            restoration.experiment_id,
            reason_label(restoration.reason),
            restoration.lease_seconds,
        );
    } else {
        eprintln!(
            "fpsmaxxing-watchdog: could not restore experiment {} ({}): {}",
            restoration.experiment_id,
            reason_label(restoration.reason),
            restoration.error.as_deref().unwrap_or("unknown error"),
        );
    }
}

fn reason_label(reason: ReclaimReason) -> &'static str {
    match reason {
        ReclaimReason::LeaseExpired => "lease expired",
        ReclaimReason::CrashRecovery => "crash recovery",
    }
}

fn policy_label(policy: ReclaimPolicy) -> &'static str {
    match policy {
        ReclaimPolicy::AllUnclosed => "recovering all unclosed experiments",
        ReclaimPolicy::ExpiredLeasesOnly => "reclaiming expired leases",
    }
}

struct Config {
    journal_path: PathBuf,
    interval: Duration,
    once: bool,
    recover_all: bool,
}

impl Config {
    fn from_args(args: impl Iterator<Item = String>) -> Self {
        let mut journal_path = None;
        let mut interval = Duration::from_secs(DEFAULT_INTERVAL_SECONDS);
        let mut once = false;
        let mut recover_all = false;
        let mut args = args;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--journal" => journal_path = args.next().map(PathBuf::from),
                "--interval" => {
                    if let Some(seconds) = args.next().and_then(|value| value.parse::<u64>().ok()) {
                        interval = Duration::from_secs(seconds.max(1));
                    }
                }
                "--once" => once = true,
                "--recover-all" => recover_all = true,
                _ => {}
            }
        }
        once = once || recover_all;
        let journal_path = journal_path
            .or_else(|| env::var("FPSMAXXING_JOURNAL_PATH").ok().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_JOURNAL));
        Self {
            journal_path,
            interval,
            once,
            recover_all,
        }
    }

    fn policy(&self) -> ReclaimPolicy {
        if self.recover_all {
            ReclaimPolicy::AllUnclosed
        } else {
            ReclaimPolicy::ExpiredLeasesOnly
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(args: &[&str]) -> Config {
        Config::from_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn recover_all_forces_single_pass() {
        let config = config(&["--recover-all"]);
        assert!(config.once);
        assert_eq!(config.policy(), ReclaimPolicy::AllUnclosed);
    }

    #[test]
    fn steady_state_poll_stays_expired_leases_only() {
        let config = config(&[]);
        assert!(!config.once);
        assert_eq!(config.policy(), ReclaimPolicy::ExpiredLeasesOnly);
    }
}
