use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::{fs, io};

use super::{
    naming,
    profile_activation::{ProfileActivation, resolve_activation},
};

/// Helper function for serde to default boolean fields to true.
fn default_true() -> bool {
    true
}

/// Represents a node in the package hierarchy.
///
/// Each node corresponds to a package name (like "combat" or "defense").
/// It stores its own enabled status and any child packages.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PackageNode {
    /// Whether this specific package is enabled.
    /// If false, all items and sub-packages within it are implicitly disabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Profile-aware activation. Absent data uses the legacy `enabled` value.
    ///
    /// `enabled` remains a fail-closed mirror for older clients: it is true only
    /// when this value is [`ProfileActivation::All`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<ProfileActivation>,
    /// A map of child package names to their corresponding `PackageNode` definitions.
    #[serde(default)] // Default to an empty map if missing
    pub children: HashMap<String, PackageNode>,
}

/// Represents the entire package hierarchy for a server, loaded from `packages.json`.
///
/// This is a map from top-level package names to their `PackageNode` definitions.
pub type PackageTree = HashMap<String, PackageNode>;

fn matching_key(map: &HashMap<String, PackageNode>, component: &str) -> Option<String> {
    if map.contains_key(component) {
        return Some(component.to_string());
    }
    let mut matches = map
        .keys()
        .filter(|key| naming::names_conflict(key, component));
    let first = matches.next()?.clone();
    matches.next().is_none().then_some(first)
}

fn matching_node<'a>(
    map: &'a HashMap<String, PackageNode>,
    component: &str,
) -> Option<(&'a str, &'a PackageNode)> {
    let key = matching_key(map, component)?;
    map.get_key_value(&key)
        .map(|(key, node)| (key.as_str(), node))
}

fn matching_node_mut<'a>(
    map: &'a mut HashMap<String, PackageNode>,
    component: &str,
) -> Option<&'a mut PackageNode> {
    let key = matching_key(map, component)?;
    map.get_mut(&key)
}

use crate::get_smudgy_home;
use anyhow::{Context, Result};

use super::persistence::write_atomic;

/// Loads the package hierarchy definition from `packages.json` for a given server.
///
/// If `packages.json` does not exist, returns an empty `PackageTree` successfully.
///
/// # Arguments
///
/// * `server_name` - The name of the server whose package tree should be loaded.
///
/// # Errors
///
/// Returns an error if the server directory cannot be accessed, or if `packages.json`
/// exists but cannot be read or parsed.
pub fn load_packages(server_name: &str) -> Result<PackageTree> {
    let smudgy_dir = get_smudgy_home()?;
    let server_path = smudgy_dir.join(server_name);
    let packages_path = server_path.join("packages.json");

    match fs::read_to_string(&packages_path) {
        Ok(content) => {
            let tree: PackageTree = serde_json::from_str(&content).context(format!(
                "Failed to parse packages.json for server '{server_name}'"
            ))?;
            Ok(tree)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // File not found is okay, just return an empty tree
            Ok(PackageTree::new())
        }
        Err(e) => {
            // Other read errors are propagated
            Err(e).context(format!(
                "Failed to read packages.json for server '{server_name}'"
            ))
        }
    }
}

/// Saves the package hierarchy definition to `packages.json` for a given server.
///
/// This will overwrite the existing file if it exists.
///
/// # Arguments
///
/// * `server_name` - The name of the server whose package tree should be saved.
/// * `tree` - The `PackageTree` data structure to save.
///
/// # Errors
///
/// Returns an error if the server directory cannot be accessed, or if `packages.json`
/// cannot be written.
pub fn save_packages(server_name: &str, tree: &PackageTree) -> Result<()> {
    let smudgy_dir = get_smudgy_home()?;
    let server_path = smudgy_dir.join(server_name);

    // Ensure the server directory exists (optional, but good practice)
    if !server_path.is_dir() {
        return Err(anyhow::anyhow!(
            "Server directory not found or not a directory: {:?}",
            server_path
        ));
    }

    let packages_path = server_path.join("packages.json");

    let json_content = serde_json::to_string_pretty(tree).context(format!(
        "Failed to serialize package tree for server '{server_name}'"
    ))?;

    write_atomic(&packages_path, json_content.as_bytes()).context(format!(
        "Failed to write packages.json for server '{server_name}' at {}",
        packages_path.display()
    ))?;

    Ok(())
}

/// Checks whether every folder on `path_str` is active for `profile_name`.
#[must_use]
pub fn is_package_effectively_enabled_for(
    path_str: &str,
    tree: &PackageTree,
    profile_name: &str,
) -> bool {
    let mut current_level = tree;
    for component in path_str
        .split('/')
        .filter(|component| !component.is_empty())
    {
        let Some((_, node)) = matching_node(current_level, component) else {
            return false;
        };
        if !resolve_activation(node.activation.as_ref(), node.enabled).is_enabled_for(profile_name)
        {
            return false;
        }
        current_level = &node.children;
    }
    true
}

/// Returns the first disabled ancestor for a folder in one profile.
#[must_use]
pub fn disabled_ancestor_for(
    path_str: &str,
    tree: &PackageTree,
    profile_name: &str,
) -> Option<String> {
    let mut current_level = tree;
    let mut current_path = Vec::new();
    let components = path_str
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        let (canonical_component, node) = matching_node(current_level, component)?;
        current_path.push(canonical_component);
        if !resolve_activation(node.activation.as_ref(), node.enabled).is_enabled_for(profile_name)
        {
            return Some(current_path.join("/"));
        }
        current_level = &node.children;
    }
    None
}

/// Collects every folder path in the tree as full slash-joined paths, sorted.
///
/// Parent folders sort before their children (e.g. `combat` before
/// `combat/healing`) since `'/'` sorts after alphanumerics.
#[must_use]
pub fn collect_folder_paths(tree: &PackageTree) -> Vec<String> {
    fn walk(map: &HashMap<String, PackageNode>, prefix: &str, out: &mut Vec<String>) {
        for (name, node) in map {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            out.push(path.clone());
            walk(&node.children, &path, out);
        }
    }
    let mut out = Vec::new();
    walk(tree, "", &mut out);
    out.sort();
    out
}

/// Returns the stored spelling for an exact or unambiguous case-insensitive folder path.
///
/// Exact matches preserve legacy trees that contain case-only siblings. If a component has more
/// than one case-insensitive candidate and no exact match, the path is ambiguous and returns
/// `None` rather than selecting a row nondeterministically.
#[must_use]
pub fn canonical_folder_path(tree: &PackageTree, path: &str) -> Option<String> {
    let mut current = tree;
    let mut canonical = Vec::new();
    for component in path.split('/').filter(|component| !component.is_empty()) {
        let key = matching_key(current, component)?;
        canonical.push(key.clone());
        current = &current.get(&key)?.children;
    }
    Some(canonical.join("/"))
}

/// Ensures a folder (and all of its ancestors) exists in the tree, enabled.
/// Existing nodes along the path are left untouched (enabled state preserved).
pub fn insert_folder(tree: &mut PackageTree, path: &str) {
    let mut current = tree;
    for component in path.split('/').filter(|c| !c.is_empty()) {
        let key = if let Some(key) = matching_key(current, component) {
            key
        } else if current
            .keys()
            .any(|key| naming::names_conflict(key, component))
        {
            // More than one legacy case-only sibling matches. Do not create a third variant or
            // choose one nondeterministically; an exact rename must disambiguate the data first.
            return;
        } else {
            component.to_owned()
        };
        current = &mut current
            .entry(key)
            .or_insert_with(|| PackageNode {
                enabled: true,
                activation: None,
                children: HashMap::new(),
            })
            .children;
    }
}

/// Whether `new_path` would collide with the existing tree or move a folder into itself.
///
/// Comparisons fold case because folder paths become filesystem path components. `old_path` may
/// be supplied for a rename; the source itself is then ignored so a case-only rename remains
/// possible. Existing ancestor spelling must still match exactly. Renaming a child must not create
/// a second, differently-cased copy of one of its parents.
#[must_use]
pub fn folder_destination_conflicts(
    tree: &PackageTree,
    new_path: &str,
    old_path: Option<&str>,
) -> bool {
    let old_path = old_path.map(|path| path.trim_matches('/'));
    let new_path = new_path.trim_matches('/');

    if let Some(old_path) = old_path {
        let old_folded = old_path.to_lowercase();
        let new_folded = new_path.to_lowercase();
        if new_folded.starts_with(&format!("{old_folded}/")) {
            return true;
        }
    }

    let existing_paths = collect_folder_paths(tree);
    if existing_paths.iter().any(|existing| {
        naming::names_conflict(existing, new_path)
            && !old_path.is_some_and(|old| naming::names_conflict(existing, old))
    }) {
        return true;
    }

    let components = new_path.split('/').collect::<Vec<_>>();
    for prefix_len in 1..components.len() {
        let prefix = components[..prefix_len].join("/");
        if existing_paths
            .iter()
            .any(|existing| naming::names_conflict(existing, &prefix) && existing != &prefix)
        {
            return true;
        }
    }

    false
}

/// Detaches the folder node at `path` from the tree, returning it (with its
/// children) if present.
fn detach_folder(tree: &mut PackageTree, path: &str) -> Option<PackageNode> {
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let (last, parents) = components.split_last()?;
    let mut current = tree;
    for component in parents {
        current = &mut matching_node_mut(current, component)?.children;
    }
    let key = matching_key(current, last)?;
    current.remove(&key)
}

/// Attaches `node` at `path`, creating any missing ancestor folders.
fn attach_folder(tree: &mut PackageTree, path: &str, node: PackageNode) {
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let Some((last, parents)) = components.split_last() else {
        return;
    };
    let mut current = tree;
    for component in parents {
        let key = matching_key(current, component).unwrap_or_else(|| (*component).to_owned());
        current = &mut current
            .entry(key)
            .or_insert_with(|| PackageNode {
                enabled: true,
                activation: None,
                children: HashMap::new(),
            })
            .children;
    }
    current.insert((*last).to_owned(), node);
}

/// Removes the folder node (and its descendants) at `path`. Returns whether a
/// node was removed.
pub fn remove_folder(tree: &mut PackageTree, path: &str) -> bool {
    detach_folder(tree, path).is_some()
}

/// Moves/renames the folder subtree at `old_path` to `new_path`, preserving its
/// children and enabled state. Returns whether the source existed.
pub fn rename_folder(tree: &mut PackageTree, old_path: &str, new_path: &str) -> bool {
    if old_path == new_path {
        return collect_folder_paths(tree)
            .iter()
            .any(|path| path == old_path);
    }
    if folder_destination_conflicts(tree, new_path, Some(old_path)) {
        return false;
    }
    match detach_folder(tree, old_path) {
        Some(node) => {
            attach_folder(tree, new_path, node);
            true
        }
        None => false,
    }
}

/// The parent path of a folder path, or `None` for a top-level folder.
/// e.g. `combat/healing` -> `Some("combat")`, `combat` -> `None`.
#[must_use]
pub fn parent_path(path: &str) -> Option<String> {
    path.rsplit_once('/').map(|(parent, _)| parent.to_owned())
}

#[cfg(test)]
mod profile_activation_tests {
    use super::*;

    #[test]
    fn disabled_ancestor_masks_but_does_not_erase_child_intent() {
        let mut tree = PackageTree::new();
        insert_folder(&mut tree, "combat/healing");
        set_folder_activation(&mut tree, "combat", ProfileActivation::None);
        set_folder_activation(&mut tree, "combat/healing", ProfileActivation::All);

        assert!(!is_package_effectively_enabled_for(
            "combat/healing",
            &tree,
            "main"
        ));
        assert_eq!(
            disabled_ancestor_for("combat/healing", &tree, "main").as_deref(),
            Some("combat")
        );
        assert_eq!(
            folder_activation(&tree, "combat/healing"),
            ProfileActivation::All
        );
    }

    fn selected(profiles: &[&str]) -> ProfileActivation {
        ProfileActivation::Selected {
            profiles: profiles.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    #[test]
    fn legacy_enabled_booleans_deserialize_to_all_and_none() {
        let tree: PackageTree = serde_json::from_str(
            r#"{"combat":{"enabled":true,"children":{"idle":{"enabled":false}}},"off":{"enabled":false}}"#,
        )
        .unwrap();
        assert_eq!(folder_activation(&tree, "combat"), ProfileActivation::All);
        assert_eq!(
            folder_activation(&tree, "combat/idle"),
            ProfileActivation::None
        );
        assert_eq!(folder_activation(&tree, "off"), ProfileActivation::None);
        assert!(is_package_effectively_enabled_for("combat", &tree, "Main"));
        assert!(!is_package_effectively_enabled_for("off", &tree, "Main"));

        // Untouched legacy nodes round-trip without gaining an activation field.
        let text = serde_json::to_string(&tree).unwrap();
        assert!(!text.contains("activation"), "{text}");
    }

    #[test]
    fn selected_activation_mirrors_legacy_enabled_false() {
        let mut tree = PackageTree::new();
        insert_folder(&mut tree, "combat");
        set_folder_activation(&mut tree, "combat", selected(&["Main"]));
        let text = serde_json::to_string(&tree).unwrap();
        let back: PackageTree = serde_json::from_str(&text).unwrap();
        assert!(!back["combat"].enabled, "an older client fails closed");
        assert_eq!(folder_activation(&back, "combat"), selected(&["Main"]));
    }

    #[test]
    fn rename_moves_subtree_activation_unchanged() {
        let mut tree = PackageTree::new();
        insert_folder(&mut tree, "combat/healing");
        set_folder_activation(&mut tree, "combat", selected(&["Alt"]));
        set_folder_activation(&mut tree, "combat/healing", selected(&["Main"]));

        assert!(rename_folder(&mut tree, "combat", "fight"));

        assert!(!collect_folder_paths(&tree).contains(&"combat".to_string()));
        assert_eq!(folder_activation(&tree, "fight"), selected(&["Alt"]));
        assert_eq!(
            folder_activation(&tree, "fight/healing"),
            selected(&["Main"])
        );
        assert!(is_package_effectively_enabled_for("fight", &tree, "Alt"));
        assert!(
            !is_package_effectively_enabled_for("fight/healing", &tree, "Main"),
            "the moved child is still masked by its parent's scope"
        );
    }

    #[test]
    fn removing_a_profile_prunes_every_folder_activation() {
        let mut tree = PackageTree::new();
        insert_folder(&mut tree, "combat/healing");
        insert_folder(&mut tree, "utility");
        set_folder_activation(&mut tree, "combat", selected(&["Main", "Alt"]));
        set_folder_activation(&mut tree, "combat/healing", selected(&["Alt"]));
        set_folder_activation(&mut tree, "utility", ProfileActivation::All);

        assert!(remove_profile_from_tree(&mut tree, "Alt"));

        assert_eq!(folder_activation(&tree, "combat"), selected(&["Main"]));
        assert_eq!(
            folder_activation(&tree, "combat/healing"),
            ProfileActivation::None,
            "an emptied selection is no profiles, never every profile"
        );
        assert_eq!(folder_activation(&tree, "utility"), ProfileActivation::All);
        assert!(!remove_profile_from_tree(&mut tree, "Alt"), "idempotent");
    }

    #[test]
    fn rename_refuses_to_replace_an_existing_subtree() {
        let mut tree = PackageTree::new();
        insert_folder(&mut tree, "source/child");
        insert_folder(&mut tree, "destination/kept");

        assert!(!rename_folder(&mut tree, "source", "destination"));
        assert!(collect_folder_paths(&tree).contains(&"source/child".to_string()));
        assert!(collect_folder_paths(&tree).contains(&"destination/kept".to_string()));
    }

    #[test]
    fn case_only_rename_is_allowed_but_parent_case_cannot_be_duplicated() {
        let mut tree = PackageTree::new();
        insert_folder(&mut tree, "Combat/Healing");

        assert!(rename_folder(&mut tree, "Combat/Healing", "Combat/healing"));
        assert!(collect_folder_paths(&tree).contains(&"Combat/healing".to_string()));
        assert!(folder_destination_conflicts(&tree, "combat/new", None));
        assert!(folder_destination_conflicts(
            &tree,
            "combat/healing",
            Some("Combat/healing")
        ));
    }

    #[test]
    fn rename_refuses_to_move_a_folder_inside_itself() {
        let mut tree = PackageTree::new();
        insert_folder(&mut tree, "combat/healing");

        assert!(!rename_folder(&mut tree, "combat", "combat/archive"));
        assert!(collect_folder_paths(&tree).contains(&"combat/healing".to_string()));
    }

    #[test]
    fn legacy_folder_casing_uses_one_activation_path() {
        let mut tree = PackageTree::new();
        insert_folder(&mut tree, "Combat/Healing");

        assert!(is_package_effectively_enabled_for(
            "combat/healing",
            &tree,
            "Main"
        ));
        assert!(set_folder_activation(
            &mut tree,
            "combat/healing",
            ProfileActivation::None
        ));
        assert_eq!(
            folder_activation(&tree, "COMBAT/HEALING"),
            ProfileActivation::None
        );

        insert_folder(&mut tree, "combat/New");
        let paths = collect_folder_paths(&tree);
        assert!(paths.contains(&"Combat/New".to_string()));
        assert!(!paths.contains(&"combat".to_string()));
    }

    #[test]
    fn legacy_case_duplicate_rows_keep_their_exact_activation() {
        let mut upper = PackageTree::new();
        insert_folder(&mut upper, "Combat/Attack");
        let mut lower = PackageTree::new();
        insert_folder(&mut lower, "combat/Heal");
        set_folder_activation(&mut lower, "combat", ProfileActivation::None);
        upper.insert("combat".to_string(), lower.remove("combat").unwrap());

        assert_eq!(folder_activation(&upper, "Combat"), ProfileActivation::All);
        assert_eq!(folder_activation(&upper, "combat"), ProfileActivation::None);
        assert_eq!(canonical_folder_path(&upper, "COMBAT"), None);
        assert!(is_package_effectively_enabled_for(
            "Combat/Attack",
            &upper,
            "Main"
        ));
        assert!(!is_package_effectively_enabled_for(
            "combat/Heal",
            &upper,
            "Main"
        ));
    }
}

/// The folder node's direct profile activation. Missing nodes default to `All`.
#[must_use]
pub fn folder_activation(tree: &PackageTree, path: &str) -> ProfileActivation {
    let mut current = tree;
    let mut activation = ProfileActivation::All;
    for component in path.split('/').filter(|component| !component.is_empty()) {
        let Some((_, node)) = matching_node(current, component) else {
            return ProfileActivation::All;
        };
        activation = resolve_activation(node.activation.as_ref(), node.enabled);
        current = &node.children;
    }
    activation
}

/// Whether the folder node's direct activation includes `profile_name`.
#[must_use]
pub fn folder_enabled_for(tree: &PackageTree, path: &str, profile_name: &str) -> bool {
    folder_activation(tree, path).is_enabled_for(profile_name)
}

/// Sets a folder node's direct activation. Returns whether the node existed.
pub fn set_folder_activation(
    tree: &mut PackageTree,
    path: &str,
    activation: ProfileActivation,
) -> bool {
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let Some((last, parents)) = components.split_last() else {
        return false;
    };
    let mut current = tree;
    for component in parents {
        let Some(node) = matching_node_mut(current, component) else {
            return false;
        };
        current = &mut node.children;
    }
    match matching_node_mut(current, last) {
        Some(node) => {
            node.enabled = activation.legacy_enabled();
            node.activation = Some(activation);
            true
        }
        None => false,
    }
}

/// Removes one deleted profile from every folder activation without consulting the rest of the
/// profile inventory.
///
/// # Errors
///
/// Returns an error if the folder tree cannot be loaded or saved.
pub fn remove_profile_activation(server_name: &str, profile_name: &str) -> Result<()> {
    super::automation_transaction::mutate(server_name, |snapshot| {
        let changed = remove_profile_from_tree(&mut snapshot.packages, profile_name);
        Ok(((), changed))
    })
}

/// Drops `profile_name` from every explicit activation in the tree. Returns whether any node
/// changed. Nodes still on legacy `enabled` data are left alone: `All` and `None` name no
/// profiles.
fn remove_profile_from_tree(tree: &mut PackageTree, profile_name: &str) -> bool {
    let mut changed = false;
    for node in tree.values_mut() {
        if let Some(activation) = node.activation.take() {
            let updated = activation.clone().without_profile(profile_name);
            changed |= updated != activation;
            node.enabled = updated.legacy_enabled();
            node.activation = Some(updated);
        }
        changed |= remove_profile_from_tree(&mut node.children, profile_name);
    }
    changed
}
