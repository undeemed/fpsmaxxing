//! Single-owner-per-knob enforcement.
//!
//! The safety invariant "only one provider owns a setting at a time" is
//! enforced here: a knob (a capability id) can be held by at most one owner at
//! once. A second owner is refused, fail-closed, until the first releases the
//! knob. Ownership is released automatically when its [`OwnershipGuard`] is
//! dropped, so a completed or panicking lifecycle never leaks a lease.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use thiserror::Error;

/// Returned when a knob is already owned by another owner.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("capability {capability_id} is already owned by {held_by}")]
pub struct OwnerConflict {
    /// The contested capability id.
    pub capability_id: String,
    /// The owner that currently holds the knob.
    pub held_by: String,
}

/// Tracks which owner holds each knob and enforces a single owner per knob.
#[derive(Debug, Default)]
pub struct OwnershipLedger {
    owners: Mutex<HashMap<String, String>>,
}

impl OwnershipLedger {
    /// Builds an empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquires exclusive ownership of `capability_id` for `owner`.
    ///
    /// The returned [`OwnershipGuard`] releases the knob when dropped.
    ///
    /// # Errors
    ///
    /// Returns [`OwnerConflict`] if another owner already holds the knob.
    pub fn acquire(
        self: &Arc<Self>,
        capability_id: &str,
        owner: &str,
    ) -> Result<OwnershipGuard, OwnerConflict> {
        let mut owners = self.lock();
        if let Some(held_by) = owners.get(capability_id) {
            return Err(OwnerConflict {
                capability_id: capability_id.to_owned(),
                held_by: held_by.clone(),
            });
        }
        owners.insert(capability_id.to_owned(), owner.to_owned());
        Ok(OwnershipGuard {
            ledger: Arc::clone(self),
            capability_id: capability_id.to_owned(),
        })
    }

    /// Returns the current owner of a knob, if any.
    #[must_use]
    pub fn owner_of(&self, capability_id: &str) -> Option<String> {
        self.lock().get(capability_id).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Holds a knob's ownership and releases it on drop.
#[derive(Debug)]
pub struct OwnershipGuard {
    ledger: Arc<OwnershipLedger>,
    capability_id: String,
}

impl OwnershipGuard {
    /// Returns the capability id this guard holds.
    #[must_use]
    pub fn capability_id(&self) -> &str {
        &self.capability_id
    }
}

impl Drop for OwnershipGuard {
    fn drop(&mut self) {
        self.ledger.lock().remove(&self.capability_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::OwnershipLedger;

    #[test]
    fn first_owner_acquires_and_second_owner_is_refused() {
        let ledger = Arc::new(OwnershipLedger::new());
        let guard = ledger
            .acquire("mock.value", "owner-a")
            .expect("first owner should acquire");
        assert_eq!(ledger.owner_of("mock.value").as_deref(), Some("owner-a"));

        let conflict = ledger
            .acquire("mock.value", "owner-b")
            .expect_err("second owner should be refused");
        assert_eq!(conflict.held_by, "owner-a");
        assert_eq!(conflict.capability_id, "mock.value");

        drop(guard);
        assert!(ledger.owner_of("mock.value").is_none());
    }

    #[test]
    fn distinct_knobs_have_independent_owners() {
        let ledger = Arc::new(OwnershipLedger::new());
        let _first = ledger
            .acquire("mock.value", "owner-a")
            .expect("first knob should acquire");
        let _second = ledger
            .acquire("power.scheme", "owner-b")
            .expect("a different knob is independently ownable");
    }

    #[test]
    fn releasing_a_knob_lets_a_new_owner_take_it() {
        let ledger = Arc::new(OwnershipLedger::new());
        {
            let _guard = ledger
                .acquire("mock.value", "owner-a")
                .expect("first owner should acquire");
        }
        let _reacquired = ledger
            .acquire("mock.value", "owner-b")
            .expect("a released knob should be re-acquirable");
    }
}
