//! Bundled loading of session configuration from disk, shaped as
//! ready-to-send [`RuntimeAction`]s.
//!
//! These load fresh on every call by design: the UI invokes them again on
//! session reload and reconnect so that edits to the on-disk configuration
//! take effect without restarting the application.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::models::{
    aliases::{AliasDefinition, load_aliases},
    hotkeys::{HotkeyDefinition, load_hotkeys},
    packages::{PackageTree, load_packages},
    profile::load_profile,
    server::load_server,
    triggers::{TriggerDefinition, load_triggers},
};

use super::runtime::{IsolateId, Origin, RuntimeAction};

/// The persisted user-authored automations and legacy folder enablement for one server.
///
/// Each runtime retains its previous snapshot and reconciles only definitions whose effective
/// runtime form changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct UserAutomations {
    aliases: HashMap<String, AliasDefinition>,
    hotkeys: HashMap<String, HotkeyDefinition>,
    triggers: HashMap<String, TriggerDefinition>,
    packages: PackageTree,
}

/// Load one complete persisted user-automation snapshot.
///
/// Categories remain failure-isolated, matching the former startup behavior: a malformed file is
/// logged and treated as an empty category rather than preventing the other automation kinds from
/// loading. Folder metadata follows the same rule.
#[must_use]
pub(crate) fn load_user_automations(server_name: &str) -> UserAutomations {
    UserAutomations {
        aliases: load_aliases(server_name).unwrap_or_else(|error| {
            log::warn!("Failed to load aliases for server {server_name}: {error:?}");
            HashMap::new()
        }),
        hotkeys: load_hotkeys(server_name).unwrap_or_else(|error| {
            log::warn!("Failed to load hotkeys for server {server_name}: {error:?}");
            HashMap::new()
        }),
        triggers: load_triggers(server_name).unwrap_or_else(|error| {
            log::warn!("Failed to load triggers for server {server_name}: {error:?}");
            HashMap::new()
        }),
        packages: load_packages(server_name).unwrap_or_else(|error| {
            log::warn!("Failed to load automation folders for server {server_name}: {error:?}");
            PackageTree::new()
        }),
    }
}

fn runtime_alias(definition: &AliasDefinition, packages: &PackageTree) -> AliasDefinition {
    let mut runtime = definition.clone();
    runtime.enabled = definition.is_effectively_enabled(packages);
    // Folder placement has no runtime meaning once effective enablement has been resolved.
    runtime.package = None;
    runtime
}

fn runtime_hotkey(definition: &HotkeyDefinition, packages: &PackageTree) -> HotkeyDefinition {
    let mut runtime = definition.clone();
    runtime.enabled = definition.is_effectively_enabled(packages);
    runtime.package = None;
    runtime
}

fn runtime_trigger(definition: &TriggerDefinition, packages: &PackageTree) -> TriggerDefinition {
    let mut runtime = definition.clone();
    runtime.enabled = definition.is_effectively_enabled(packages);
    runtime.package = None;
    runtime
}

fn reconcile_alias_actions(
    previous: &UserAutomations,
    desired: &UserAutomations,
) -> Vec<RuntimeAction> {
    let mut actions = Vec::new();

    for (name, definition) in &previous.aliases {
        let old = runtime_alias(definition, &previous.packages);
        let new = desired
            .aliases
            .get(name)
            .map(|definition| runtime_alias(definition, &desired.packages));
        if old.enabled
            && new
                .as_ref()
                .is_none_or(|definition| !definition.enabled || definition != &old)
        {
            actions.push(RuntimeAction::RemoveAlias(
                IsolateId::Main,
                Origin::User,
                Arc::new(name.clone()),
            ));
        }
    }
    for (name, definition) in &desired.aliases {
        let new = runtime_alias(definition, &desired.packages);
        if !new.enabled {
            continue;
        }
        let unchanged = previous
            .aliases
            .get(name)
            .map(|definition| runtime_alias(definition, &previous.packages))
            .is_some_and(|old| old == new);
        if !unchanged {
            actions.push(RuntimeAction::AddAlias {
                isolate: IsolateId::Main,
                origin: Origin::User,
                name: Arc::new(name.clone()),
                alias: new,
                fire_limit: None,
            });
        }
    }

    actions
}

fn reconcile_trigger_actions(
    previous: &UserAutomations,
    desired: &UserAutomations,
) -> Vec<RuntimeAction> {
    let mut actions = Vec::new();

    for (name, definition) in &previous.triggers {
        let old = runtime_trigger(definition, &previous.packages);
        let new = desired
            .triggers
            .get(name)
            .map(|definition| runtime_trigger(definition, &desired.packages));
        if old.enabled
            && new
                .as_ref()
                .is_none_or(|definition| !definition.enabled || definition != &old)
        {
            actions.push(RuntimeAction::RemoveTrigger(
                IsolateId::Main,
                Origin::User,
                Arc::new(name.clone()),
            ));
        }
    }
    for (name, definition) in &desired.triggers {
        let new = runtime_trigger(definition, &desired.packages);
        if !new.enabled {
            continue;
        }
        let unchanged = previous
            .triggers
            .get(name)
            .map(|definition| runtime_trigger(definition, &previous.packages))
            .is_some_and(|old| old == new);
        if !unchanged {
            actions.push(RuntimeAction::AddTrigger {
                isolate: IsolateId::Main,
                origin: Origin::User,
                name: Arc::new(name.clone()),
                trigger: new,
                fire_limit: None,
                line_limit: None,
            });
        }
    }

    actions
}

fn reconcile_hotkey_actions(
    previous: &UserAutomations,
    desired: &UserAutomations,
) -> Vec<RuntimeAction> {
    let mut actions = Vec::new();

    for (name, definition) in &previous.hotkeys {
        let old = runtime_hotkey(definition, &previous.packages);
        let new = desired
            .hotkeys
            .get(name)
            .map(|definition| runtime_hotkey(definition, &desired.packages));
        if old.enabled
            && new
                .as_ref()
                .is_none_or(|definition| !definition.enabled || definition != &old)
        {
            actions.push(RuntimeAction::RemoveHotkey(
                IsolateId::Main,
                Origin::User,
                Arc::new(name.clone()),
            ));
        }
    }
    for (name, definition) in &desired.hotkeys {
        let new = runtime_hotkey(definition, &desired.packages);
        if !new.enabled {
            continue;
        }
        let unchanged = previous
            .hotkeys
            .get(name)
            .map(|definition| runtime_hotkey(definition, &previous.packages))
            .is_some_and(|old| old == new);
        if !unchanged {
            actions.push(RuntimeAction::AddHotkey {
                isolate: IsolateId::Main,
                origin: Origin::User,
                name: Arc::new(name.clone()),
                hotkey: new,
                function_id: None,
            });
        }
    }

    actions
}

/// Diff two persisted snapshots into the existing fine-grained runtime action vocabulary.
///
/// Only effectively enabled definitions are installed. Disabling therefore removes matcher and
/// hotkey registrations completely; enabling adds them back without replacing the script engine.
/// A changed active alias/trigger is removed before replacement so a failed regex or script
/// compile cannot leave the obsolete definition active.
pub(crate) fn reconcile_automation_actions(
    previous: &UserAutomations,
    desired: &UserAutomations,
) -> Vec<RuntimeAction> {
    let mut actions = reconcile_alias_actions(previous, desired);
    actions.extend(reconcile_trigger_actions(previous, desired));
    actions.extend(reconcile_hotkey_actions(previous, desired));
    actions
}

/// Build the action that asks a session to load and reconcile the server's current persisted
/// aliases, triggers, hotkeys, and folder enablement.
#[must_use]
pub fn load_automation_actions(_server_name: &str) -> Vec<RuntimeAction> {
    vec![RuntimeAction::SyncUserAutomations]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ScriptLang;
    use crate::models::packages::PackageNode;

    fn alias(package: Option<&str>, enabled: bool) -> AliasDefinition {
        AliasDefinition {
            pattern: "^go$".to_string(),
            script: Some("north".to_string()),
            package: package.map(str::to_string),
            enabled,
            priority: 0,
            fallthrough: true,
            allow_self_match: false,
            language: ScriptLang::Plaintext,
            matcher: None,
        }
    }

    fn trigger(package: Option<&str>, enabled: bool) -> TriggerDefinition {
        TriggerDefinition {
            patterns: Some(vec!["^ready$".to_string()]),
            script: Some("look".to_string()),
            package: package.map(str::to_string),
            enabled,
            ..TriggerDefinition::default()
        }
    }

    fn hotkey(package: Option<&str>, enabled: bool) -> HotkeyDefinition {
        HotkeyDefinition {
            key: "F1".to_string(),
            modifiers: Vec::new(),
            script: Some("score".to_string()),
            package: package.map(str::to_string),
            language: ScriptLang::Plaintext,
            enabled,
        }
    }

    fn folder(enabled: bool) -> PackageTree {
        HashMap::from([(
            "combat".to_string(),
            PackageNode {
                enabled,
                children: HashMap::new(),
            },
        )])
    }

    #[test]
    fn unchanged_snapshot_emits_no_actions() {
        let snapshot = UserAutomations {
            aliases: HashMap::from([("go".to_string(), alias(None, true))]),
            hotkeys: HashMap::from([("score".to_string(), hotkey(None, true))]),
            triggers: HashMap::from([("ready".to_string(), trigger(None, true))]),
            packages: PackageTree::new(),
        };

        assert!(reconcile_automation_actions(&snapshot, &snapshot).is_empty());
    }

    #[test]
    fn changed_active_alias_is_removed_before_replacement() {
        let previous = UserAutomations {
            aliases: HashMap::from([("go".to_string(), alias(None, true))]),
            ..UserAutomations::default()
        };
        let mut replacement = alias(None, true);
        replacement.pattern = "^run$".to_string();
        let desired = UserAutomations {
            aliases: HashMap::from([("go".to_string(), replacement)]),
            ..UserAutomations::default()
        };

        let actions = reconcile_automation_actions(&previous, &desired);
        assert_eq!(actions.len(), 2);
        assert!(matches!(actions[0], RuntimeAction::RemoveAlias(..)));
        assert!(matches!(actions[1], RuntimeAction::AddAlias { .. }));
    }

    #[test]
    fn disabling_folder_removes_each_effectively_disabled_kind() {
        let previous = UserAutomations {
            aliases: HashMap::from([("go".to_string(), alias(Some("combat"), true))]),
            hotkeys: HashMap::from([("score".to_string(), hotkey(Some("combat"), true))]),
            triggers: HashMap::from([("ready".to_string(), trigger(Some("combat"), true))]),
            packages: folder(true),
        };
        let desired = UserAutomations {
            packages: folder(false),
            ..previous.clone()
        };

        let actions = reconcile_automation_actions(&previous, &desired);
        assert_eq!(actions.len(), 3);
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, RuntimeAction::RemoveAlias(..)))
        );
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, RuntimeAction::RemoveTrigger(..)))
        );
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, RuntimeAction::RemoveHotkey(..)))
        );
    }

    #[test]
    fn enabling_leaf_adds_each_kind() {
        let previous = UserAutomations {
            aliases: HashMap::from([("go".to_string(), alias(None, false))]),
            hotkeys: HashMap::from([("score".to_string(), hotkey(None, false))]),
            triggers: HashMap::from([("ready".to_string(), trigger(None, false))]),
            packages: PackageTree::new(),
        };
        let desired = UserAutomations {
            aliases: HashMap::from([("go".to_string(), alias(None, true))]),
            hotkeys: HashMap::from([("score".to_string(), hotkey(None, true))]),
            triggers: HashMap::from([("ready".to_string(), trigger(None, true))]),
            packages: PackageTree::new(),
        };

        let actions = reconcile_automation_actions(&previous, &desired);
        assert_eq!(actions.len(), 3);
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, RuntimeAction::AddAlias { .. }))
        );
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, RuntimeAction::AddTrigger { .. }))
        );
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, RuntimeAction::AddHotkey { .. }))
        );
    }
}

/// Build the [`RuntimeAction::Connect`] for a server/profile from their saved
/// configurations.
///
/// # Errors
///
/// Returns an error if the profile or server configuration fails to load
/// (missing or malformed config file).
pub fn load_connect_action(server_name: &str, profile_name: &str) -> Result<RuntimeAction> {
    let profile =
        load_profile(server_name, profile_name).context("Failed to load profile config")?;
    let server = load_server(server_name).context("Failed to load server config")?;

    // Substitute the $PASSWORD token (if present) with the password stored in the
    // OS keyring for this profile, and collect the secret(s) to redact from the
    // client's view and the session log when the auto-login text is echoed. The
    // token — not the password — is what lives in profile.json.
    let (send_on_connect, send_on_connect_redactions) = if profile.config.send_on_connect.is_empty()
    {
        (None, Vec::new())
    } else {
        let (text, redactions) = crate::models::profile::substitute_password_with_redactions(
            server_name,
            profile_name,
            &profile.config.send_on_connect,
        );
        (Some(Arc::new(text)), redactions)
    };

    let mccp4_compression = server.config.accepts_mccp4_compression();
    Ok(RuntimeAction::Connect {
        host: server.config.host.into(),
        port: server.config.port,
        send_on_connect,
        send_on_connect_redactions: Arc::new(send_on_connect_redactions),
        encoding: server.config.encoding.map(Arc::new),
        compression: crate::session::connection::InboundCompression::new(
            server.config.compression,
            mccp4_compression,
        ),
        tls: crate::session::connection::TlsMode::from_settings(
            server.config.tls,
            server.config.tls_verify,
        ),
    })
}
