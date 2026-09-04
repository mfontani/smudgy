//! Serialized multi-file commits for user automations and their folder tree.
//!
//! Aliases, hotkeys, triggers, and `packages.json` are separate files. A folder rename or delete
//! changes several of them together, so this module reads and writes all four under one
//! per-server lock and lets an editor commit against the exact snapshot it loaded.

use std::collections::HashMap;

use anyhow::{Context, Result};

use super::aliases::{self, AliasDefinition};
use super::hotkeys::{self, HotkeyDefinition};
use super::packages::{self, PackageTree};
use super::state_lock::{self, StateLockGuard};
use super::triggers::{self, TriggerDefinition};

/// Whether a commit was applied or another writer changed the snapshot first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    Applied,
    /// Another writer changed the persisted snapshot after the caller loaded it.
    Conflict,
}

/// One complete on-disk automation state.
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationStateSnapshot {
    pub packages: PackageTree,
    pub aliases: HashMap<String, AliasDefinition>,
    pub hotkeys: HashMap<String, HotkeyDefinition>,
    pub triggers: HashMap<String, TriggerDefinition>,
}

impl AutomationStateSnapshot {
    #[must_use]
    pub fn new(
        packages: PackageTree,
        aliases: HashMap<String, AliasDefinition>,
        hotkeys: HashMap<String, HotkeyDefinition>,
        triggers: HashMap<String, TriggerDefinition>,
    ) -> Self {
        Self {
            packages,
            aliases,
            hotkeys,
            triggers,
        }
    }
}

pub(crate) fn guard(server_name: &str) -> StateLockGuard {
    state_lock::acquire(&format!("automation-state:{server_name}"))
}

fn load_locked(server_name: &str) -> Result<AutomationStateSnapshot> {
    Ok(AutomationStateSnapshot::new(
        packages::load_packages(server_name)?,
        aliases::load_aliases(server_name)?,
        hotkeys::load_hotkeys(server_name)?,
        triggers::load_triggers(server_name)?,
    ))
}

fn apply_snapshot(server_name: &str, snapshot: &AutomationStateSnapshot) -> Result<()> {
    let server_dir = crate::get_smudgy_home()?.join(server_name);
    for kind in ["aliases", "hotkeys", "triggers"] {
        std::fs::create_dir_all(server_dir.join(kind))
            .with_context(|| format!("create {kind} directory for {server_name}"))?;
    }
    aliases::save_aliases(server_name, &snapshot.aliases)?;
    hotkeys::save_hotkeys(server_name, &snapshot.hotkeys)?;
    triggers::save_triggers(server_name, &snapshot.triggers)?;
    packages::save_packages(server_name, &snapshot.packages)
}

/// Loads all four files while holding the same lock as writers, so a reader never combines
/// categories from two different commits.
///
/// # Errors
/// Returns an error if any file cannot be read or parsed.
pub fn load(server_name: &str) -> Result<AutomationStateSnapshot> {
    let _guard = guard(server_name);
    load_locked(server_name)
}

/// Commits `desired` only if the complete persisted snapshot still equals `expected`.
///
/// A conflict is a normal stale-editor result: another window or a script changed the
/// automations after the editor loaded them, and nothing is written.
///
/// # Errors
/// Returns an error if the comparison read or a write fails.
pub fn commit_if_unchanged(
    server_name: &str,
    expected: &AutomationStateSnapshot,
    desired: &AutomationStateSnapshot,
) -> Result<CommitOutcome> {
    let _guard = guard(server_name);
    if load_locked(server_name)? != *expected {
        return Ok(CommitOutcome::Conflict);
    }
    apply_snapshot(server_name, desired)?;
    Ok(CommitOutcome::Applied)
}

/// Performs one read-modify-write operation under the automation lock.
///
/// The callback returns `(value, changed)`. A no-op writes nothing.
///
/// # Errors
/// Returns an error if loading, the callback, or a write fails.
pub fn mutate<T>(
    server_name: &str,
    operation: impl FnOnce(&mut AutomationStateSnapshot) -> Result<(T, bool)>,
) -> Result<T> {
    let _guard = guard(server_name);
    let mut snapshot = load_locked(server_name)?;
    let (value, changed) = operation(&mut snapshot)?;
    if changed {
        apply_snapshot(server_name, &snapshot)?;
    }
    Ok(value)
}
