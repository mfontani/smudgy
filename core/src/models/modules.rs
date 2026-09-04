use crate::get_smudgy_home;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use super::persistence::write_atomic;
use super::profile_activation::{ProfileActivation, resolve_activation};
use super::state_lock::{self, StateLockGuard};

const SETTINGS_FILE: &str = "modules.json";

fn guard(server_name: &str) -> StateLockGuard {
    state_lock::acquire(&format!("module-state:{server_name}"))
}

fn default_true() -> bool {
    true
}

fn checked_module_relative_path(subpath: &str) -> Result<&Path> {
    if subpath.is_empty() || subpath.contains('\\') {
        anyhow::bail!("invalid module path: {subpath}");
    }
    let relative = Path::new(subpath);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        anyhow::bail!("invalid module path: {subpath}");
    }
    Ok(relative)
}

/// Represents a discovered module file within a server's `modules` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleFile {
    /// The module's path relative to the server's `modules/` directory,
    /// forward-slashed (e.g. "`auto_healer.ts`" or "`combat/healer.ts`").
    pub subpath: String,
    /// The full path to the module file.
    pub path: PathBuf,
}

/// Result of replacing an existing module source with an exact compare-and-swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleFileWriteOutcome {
    Saved,
    /// The file no longer contains the text the editor opened, so nothing was written.
    Conflict,
}

impl ModuleFile {}

/// The module files discovered under a server's `modules/` directory, plus one diagnostic line
/// per entry that discovery skipped.
///
/// Discovery degrades per entry rather than per server: an unreadable directory entry or a pair
/// of files whose names differ only by letter case is left out of `files` and described in
/// `warnings`, while every other module remains available. Callers that run modules surface each
/// warning to the user so a skipped file is never silent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleInventory {
    /// The loadable modules, sorted lexically by `subpath` (parents before children).
    pub files: Vec<ModuleFile>,
    /// Human-readable descriptions of skipped entries, each naming the path involved.
    pub warnings: Vec<String>,
}

/// Profile activation metadata for one module file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModulePolicy {
    /// Fail-closed mirror for clients that do not understand `activation`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<ProfileActivation>,
}

impl ModulePolicy {
    fn from_activation(activation: ProfileActivation) -> Self {
        Self {
            enabled: activation.legacy_enabled(),
            activation: Some(activation),
        }
    }

    fn resolved(&self) -> ProfileActivation {
        resolve_activation(self.activation.as_ref(), self.enabled)
    }
}

/// The server's sparse module activation file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModuleSettings {
    pub version: u32,
    pub modules: BTreeMap<String, ModulePolicy>,
}

impl Default for ModuleSettings {
    fn default() -> Self {
        Self {
            version: 1,
            modules: BTreeMap::new(),
        }
    }
}

/// Lists the module files found within a server's `modules` directory, recursing into
/// subdirectories. Each entry's `subpath` is forward-slashed and relative to the
/// `modules/` root, sorted lexically (parents before children). The smudgy-generated
/// `tsconfig.json` editor pointer is excluded — it's a VS Code artifact, not a module — as is
/// every dot-prefixed name (editor state, atomic-write temporaries such as `.smudgy-write-*`).
///
/// Entries that cannot be read and module files that differ only by letter case are skipped and
/// reported in [`ModuleInventory::warnings`] instead of failing discovery for the whole server.
///
/// # Errors
///
/// Returns an error if the server directory cannot be located or the `modules` directory itself
/// cannot be read. If the `modules` directory doesn't exist, an empty inventory is returned
/// successfully.
pub fn list_modules(server_name: &str) -> Result<ModuleInventory> {
    let _guard = guard(server_name);
    list_modules_in(&get_smudgy_home()?.join(server_name))
}

fn list_modules_in(server_dir: &Path) -> Result<ModuleInventory> {
    let (files, mut warnings) = discover_module_files(server_dir)?;
    let files = prune_case_collisions(files, &mut warnings);
    Ok(ModuleInventory { files, warnings })
}

/// Every readable module file under `modules/`, sorted, before case-collision pruning.
///
/// Metadata writers use this complete list: a colliding file still occupies its name, so a
/// create must not reuse it and an activation write must not prune its metadata entry.
fn discover_module_files(server_dir: &Path) -> Result<(Vec<ModuleFile>, Vec<String>)> {
    let modules_dir = server_dir.join("modules");
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    collect_module_files(&modules_dir, &modules_dir, &mut files, &mut warnings)
        .with_context(|| format!("Failed to read modules in {}", modules_dir.display()))?;
    files.sort_by(|a, b| a.subpath.cmp(&b.subpath));
    Ok((files, warnings))
}

/// Drops every file whose subpath collides with another by letter case only, appending one
/// warning per collision group. Neither spelling can be trusted to own the metadata key, so
/// both are excluded; the remaining files are unaffected.
fn prune_case_collisions(files: Vec<ModuleFile>, warnings: &mut Vec<String>) -> Vec<ModuleFile> {
    let mut groups = BTreeMap::<String, Vec<String>>::new();
    for file in &files {
        groups
            .entry(file.subpath.to_ascii_lowercase())
            .or_default()
            .push(file.subpath.clone());
    }
    let colliding: std::collections::BTreeSet<String> = groups
        .into_values()
        .filter(|names| names.len() > 1)
        .inspect(|names| {
            warnings.push(format!(
                "[module] Skipped {}: these module files differ only by letter case; rename or remove one before they can load",
                names.join(" and ")
            ));
        })
        .flatten()
        .collect();
    if colliding.is_empty() {
        return files;
    }
    files
        .into_iter()
        .filter(|file| !colliding.contains(&file.subpath))
        .collect()
}

/// The default auto-load scope for a module that has no metadata.
///
/// Top-level files preserve the legacy auto-load behavior. Nested files are
/// importable helpers and do not auto-load until the user selects a scope.
#[must_use]
pub fn default_activation(subpath: &str) -> ProfileActivation {
    if subpath.contains('/') {
        ProfileActivation::None
    } else {
        ProfileActivation::All
    }
}

/// Loads the sparse module activation file. A missing file yields defaults.
///
/// # Errors
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_settings(server_name: &str) -> Result<ModuleSettings> {
    let _guard = guard(server_name);
    load_settings_in(&get_smudgy_home()?.join(server_name))
}

/// Loads one consistent module inventory and activation snapshot, including the per-entry
/// discovery warnings so the caller can surface each skipped module.
///
/// # Errors
/// Returns an error if the `modules` directory or the settings file cannot be read.
pub fn load_module_inventory(server_name: &str) -> Result<(ModuleInventory, ModuleSettings)> {
    let _guard = guard(server_name);
    let server_dir = get_smudgy_home()?.join(server_name);
    Ok((
        list_modules_in(&server_dir)?,
        load_settings_in(&server_dir)?,
    ))
}

/// Loads one consistent module inventory and activation snapshot.
///
/// Discovery warnings are logged rather than returned; callers that can show them to the user
/// use [`load_module_inventory`].
///
/// # Errors
/// Returns an error if the `modules` directory or the settings file cannot be read.
pub fn load_module_state(server_name: &str) -> Result<(Vec<ModuleFile>, ModuleSettings)> {
    let (inventory, settings) = load_module_inventory(server_name)?;
    for warning in &inventory.warnings {
        log::warn!("{server_name}: {warning}");
    }
    Ok((inventory.files, settings))
}

fn load_settings_in(server_dir: &Path) -> Result<ModuleSettings> {
    let path = server_dir.join(SETTINGS_FILE);
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ModuleSettings::default()),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn save_settings_in(server_dir: &Path, settings: &ModuleSettings) -> Result<()> {
    fs::create_dir_all(server_dir)
        .with_context(|| format!("Failed to create {}", server_dir.display()))?;
    let path = server_dir.join(SETTINGS_FILE);
    let json = serde_json::to_string_pretty(settings).context("serialize module settings")?;
    write_atomic(&path, json.as_bytes())
        .with_context(|| format!("Failed to write {}", path.display()))
}

/// Creates a module with its activation policy.
///
/// The metadata is written before the source so a newly created nested module never auto-loads
/// under the default policy between the two writes. The source is written with no-clobber
/// semantics, so a race cannot truncate an existing module.
///
/// # Errors
///
/// Returns an error for an unsafe or occupied path, unreadable metadata, or a failed write.
pub fn create_module(
    server_name: &str,
    subpath: &str,
    content: &str,
    activation: ProfileActivation,
) -> Result<()> {
    let _guard = guard(server_name);
    create_module_in(
        &get_smudgy_home()?.join(server_name),
        subpath,
        content,
        activation,
    )
}

fn create_module_in(
    server_dir: &Path,
    subpath: &str,
    content: &str,
    activation: ProfileActivation,
) -> Result<()> {
    let relative = checked_module_relative_path(subpath)?;
    // The unpruned list: a file skipped for a case collision still occupies its name.
    let (existing_files, _) = discover_module_files(server_dir)?;
    if let Some(existing) = existing_files
        .iter()
        .find(|file| file.subpath.eq_ignore_ascii_case(subpath))
    {
        if existing.subpath == subpath {
            anyhow::bail!("a module named {subpath} already exists");
        }
        anyhow::bail!(
            "a module named {} already exists; module names cannot differ only by letter case",
            existing.subpath
        );
    }
    let target = server_dir.join("modules").join(relative);

    let mut settings = load_settings_in(server_dir)?;
    settings
        .modules
        .retain(|path, _| !path.eq_ignore_ascii_case(subpath));
    if activation != default_activation(subpath) {
        settings.modules.insert(
            subpath.to_string(),
            ModulePolicy::from_activation(activation),
        );
    }
    save_settings_in(server_dir, &settings)?;

    let parent = target
        .parent()
        .context("module destination has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".smudgy-module-")
        .tempfile_in(parent)
        .with_context(|| {
            format!(
                "Failed to create a temporary module in {}",
                parent.display()
            )
        })?;
    std::io::Write::write_all(&mut temporary, content.as_bytes())
        .context("Failed to write module source")?;
    temporary
        .as_file()
        .sync_all()
        .context("Failed to sync module source")?;
    temporary
        .persist_noclobber(&target)
        .map_err(|error| error.error)
        .with_context(|| format!("Failed to create {}", target.display()))?;
    Ok(())
}

/// Replaces one existing module only when it still contains the text the editor opened.
///
/// # Errors
/// Returns an error for an invalid path or a failed read or write.
pub fn save_module_if_unchanged(
    server_name: &str,
    subpath: &str,
    expected: &str,
    content: &str,
) -> Result<ModuleFileWriteOutcome> {
    let _guard = guard(server_name);
    save_module_if_unchanged_in(
        &get_smudgy_home()?.join(server_name),
        subpath,
        expected,
        content,
    )
}

fn save_module_if_unchanged_in(
    server_dir: &Path,
    subpath: &str,
    expected: &str,
    content: &str,
) -> Result<ModuleFileWriteOutcome> {
    let relative = checked_module_relative_path(subpath)?;
    let path = server_dir.join("modules").join(relative);
    let current = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {} before saving", path.display()))?;
    if current != expected {
        return Ok(ModuleFileWriteOutcome::Conflict);
    }
    write_atomic(&path, content.as_bytes())
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(ModuleFileWriteOutcome::Saved)
}

/// Returns one module's direct activation, including its path-based default.
#[must_use]
pub fn activation(settings: &ModuleSettings, subpath: &str) -> ProfileActivation {
    if let Some(policy) = settings.modules.get(subpath) {
        return policy.resolved();
    }

    let mut folded = settings
        .modules
        .iter()
        .filter(|(stored, _)| stored.eq_ignore_ascii_case(subpath));
    match (folded.next(), folded.next()) {
        (Some((_, policy)), None) => policy.resolved(),
        // Conflicting legacy keys cannot safely choose an activation. In particular, a top-level
        // module must never fall back to its default-on policy merely because casing became
        // ambiguous.
        (Some(_), Some(_)) => ProfileActivation::None,
        (None, _) => default_activation(subpath),
    }
}

/// Saves one module's complete activation and removes stale metadata entries.
///
/// # Errors
/// Returns an error if the module does not exist or the settings file cannot be written.
pub fn set_activation(server_name: &str, subpath: &str, target: ProfileActivation) -> Result<()> {
    let _guard = guard(server_name);
    set_activation_in(&get_smudgy_home()?.join(server_name), subpath, target)
}

fn set_activation_in(server_dir: &Path, subpath: &str, target: ProfileActivation) -> Result<()> {
    let mut settings = load_settings_in(server_dir)?;
    // `files` is the unpruned list so the metadata of a case-colliding pair survives pruning
    // below; `loadable` decides which names may take an activation at all.
    let (files, mut warnings) = discover_module_files(server_dir)?;
    let loadable = prune_case_collisions(files.clone(), &mut warnings);
    let canonical_subpath = loadable
        .iter()
        .find(|file| file.subpath == subpath)
        .or_else(|| {
            loadable
                .iter()
                .find(|file| file.subpath.eq_ignore_ascii_case(subpath))
        })
        .map(|file| file.subpath.clone())
        .with_context(|| {
            if files
                .iter()
                .any(|file| file.subpath.eq_ignore_ascii_case(subpath))
            {
                format!(
                    "module {subpath} cannot be configured because another module differs from it only by letter case"
                )
            } else {
                format!("module {subpath} does not exist")
            }
        })?;
    // Prune entries for files that no longer exist, and replace every legacy spelling of this
    // module with the inventory's exact spelling.
    settings.modules.retain(|path, _| {
        files
            .iter()
            .any(|file| file.subpath.eq_ignore_ascii_case(path))
            && !path.eq_ignore_ascii_case(&canonical_subpath)
    });
    if target != default_activation(&canonical_subpath) {
        settings
            .modules
            .insert(canonical_subpath, ModulePolicy::from_activation(target));
    }
    save_settings_in(server_dir, &settings)
}

/// Removes one deleted profile from module activation metadata without consulting other profiles.
///
/// # Errors
///
/// Returns an error if module metadata cannot be loaded, serialized, or written.
pub fn remove_profile_activation(server_name: &str, profile_name: &str) -> Result<()> {
    let _guard = guard(server_name);
    remove_profile_activation_in(&get_smudgy_home()?.join(server_name), profile_name)
}

fn remove_profile_activation_in(server_dir: &Path, profile_name: &str) -> Result<()> {
    let mut settings = load_settings_in(server_dir)?;
    let mut changed = false;
    for policy in settings.modules.values_mut() {
        let activation = policy.resolved();
        let updated = activation.clone().without_profile(profile_name);
        changed |= updated != activation;
        *policy = ModulePolicy::from_activation(updated);
    }
    if !changed {
        return Ok(());
    }
    save_settings_in(&server_dir, &settings)
}

/// Whether a module is an auto-load root for `profile_name`.
#[must_use]
pub fn is_enabled_for(settings: &ModuleSettings, subpath: &str, profile_name: &str) -> bool {
    activation(settings, subpath).is_enabled_for(profile_name)
}

/// Whether this file type can be evaluated as a module root.
#[must_use]
pub fn is_script_module(subpath: &str) -> bool {
    let lower = subpath.to_ascii_lowercase();
    if [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|extension| lower.ends_with(extension))
    {
        return false;
    }
    [".js", ".ts", ".jsx", ".tsx"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// Walks `dir` (a descendant of, or equal to, `root`) appending module files to `out`.
///
/// Only the root directory's read failure is an error. Anything below it degrades per entry:
/// an unreadable entry, an entry whose type cannot be determined, or an unreadable
/// subdirectory is described in `warnings` and skipped so one bad entry cannot hide every
/// other module. Dot-prefixed names are skipped silently; they are never modules (editor
/// state, atomic-write temporaries). Symlinks are not followed.
fn collect_module_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<ModuleFile>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        // A missing `modules` directory is not an error — just no modules.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!(
                    "[module] Skipped an unreadable entry in {}: {error}",
                    dir.display()
                ));
                continue;
            }
        };
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                warnings.push(format!(
                    "[module] Skipped {}: its file type could not be read: {error}",
                    path.display()
                ));
                continue;
            }
        };
        if file_type.is_dir() {
            if let Err(error) = collect_module_files(root, &path, out, warnings) {
                warnings.push(format!(
                    "[module] Skipped directory {}: {error:#}",
                    path.display()
                ));
            }
        } else if file_type.is_file() {
            // Skip the smudgy-generated `modules/tsconfig.json` — a thin VS Code project pointer
            // (see `script_typings`) that lives alongside real modules but isn't one. Excluding it
            // here keeps it out of both the sidebar list and the module count.
            if path.file_name().and_then(|n| n.to_str()) == Some("tsconfig.json") {
                continue;
            }
            let subpath = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(ModuleFile { subpath, path });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn selected(profiles: &[&str]) -> ProfileActivation {
        ProfileActivation::Selected {
            profiles: profiles.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    #[test]
    fn top_level_defaults_on_and_nested_defaults_off() {
        let settings = ModuleSettings::default();
        assert!(is_enabled_for(&settings, "healer.ts", "Main"));
        assert!(!is_enabled_for(&settings, "combat/healer.ts", "Main"));
    }

    #[test]
    fn legacy_enabled_false_disables_everywhere() {
        let mut settings = ModuleSettings::default();
        settings.modules.insert(
            "healer.ts".into(),
            ModulePolicy {
                enabled: false,
                activation: None,
            },
        );
        assert!(!is_enabled_for(&settings, "healer.ts", "Main"));
    }

    #[test]
    fn removing_a_profile_prunes_module_activation() {
        let dir = tempfile::tempdir().unwrap();
        create_module_in(
            dir.path(),
            "combat/healer.ts",
            "export {};",
            selected(&["Healer", "Main"]),
        )
        .unwrap();
        create_module_in(dir.path(), "solo.ts", "export {};", selected(&["Healer"])).unwrap();

        remove_profile_activation_in(dir.path(), "Healer").unwrap();

        let settings = load_settings_in(dir.path()).unwrap();
        assert_eq!(
            activation(&settings, "combat/healer.ts"),
            selected(&["Main"])
        );
        assert_eq!(
            activation(&settings, "solo.ts"),
            ProfileActivation::None,
            "an emptied selection stays off rather than reverting to the top-level default"
        );
    }

    #[test]
    fn create_and_activation_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        create_module_in(
            dir.path(),
            "combat/healer.ts",
            "export {};",
            selected(&["Healer"]),
        )
        .unwrap();
        let settings = load_settings_in(dir.path()).unwrap();
        assert_eq!(
            activation(&settings, "combat/healer.ts"),
            selected(&["Healer"])
        );
        assert!(
            dir.path()
                .join("modules")
                .join("combat")
                .join("healer.ts")
                .is_file()
        );

        // Creating a top-level module with the default policy writes no entry.
        create_module_in(dir.path(), "main.ts", "export {};", ProfileActivation::All).unwrap();
        let settings = load_settings_in(dir.path()).unwrap();
        assert!(!settings.modules.contains_key("main.ts"));

        set_activation_in(dir.path(), "main.ts", ProfileActivation::None).unwrap();
        let settings = load_settings_in(dir.path()).unwrap();
        assert_eq!(activation(&settings, "main.ts"), ProfileActivation::None);

        // Returning to the default removes the entry again.
        set_activation_in(dir.path(), "main.ts", ProfileActivation::All).unwrap();
        let settings = load_settings_in(dir.path()).unwrap();
        assert!(!settings.modules.contains_key("main.ts"));
    }

    #[test]
    fn create_rejects_duplicates_and_unsafe_paths() {
        let dir = tempfile::tempdir().unwrap();
        create_module_in(dir.path(), "a.ts", "", ProfileActivation::All).unwrap();
        assert!(create_module_in(dir.path(), "a.ts", "", ProfileActivation::All).is_err());
        assert!(create_module_in(dir.path(), "A.ts", "", ProfileActivation::All).is_err());
        assert!(create_module_in(dir.path(), "../x.ts", "", ProfileActivation::All).is_err());
    }

    #[test]
    fn save_if_unchanged_detects_external_edits() {
        let dir = tempfile::tempdir().unwrap();
        create_module_in(dir.path(), "a.ts", "one", ProfileActivation::All).unwrap();
        assert_eq!(
            save_module_if_unchanged_in(dir.path(), "a.ts", "one", "two").unwrap(),
            ModuleFileWriteOutcome::Saved
        );
        assert_eq!(
            save_module_if_unchanged_in(dir.path(), "a.ts", "one", "three").unwrap(),
            ModuleFileWriteOutcome::Conflict
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("modules").join("a.ts")).unwrap(),
            "two"
        );
    }

    #[test]
    fn set_activation_prunes_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        create_module_in(dir.path(), "a.ts", "", ProfileActivation::None).unwrap();
        create_module_in(dir.path(), "b.ts", "", ProfileActivation::None).unwrap();
        fs::remove_file(dir.path().join("modules").join("b.ts")).unwrap();
        set_activation_in(dir.path(), "a.ts", selected(&["Main"])).unwrap();
        let settings = load_settings_in(dir.path()).unwrap();
        assert_eq!(
            settings.modules.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from(["a.ts".to_string()])
        );
    }

    #[test]
    fn discovery_skips_dotfiles_and_tsconfig() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("modules");
        fs::create_dir_all(modules.join("combat").join(".git")).unwrap();
        fs::write(modules.join("main.ts"), "").unwrap();
        fs::write(modules.join(".smudgy-write-1234"), "").unwrap();
        fs::write(modules.join(".hidden.ts"), "").unwrap();
        fs::write(modules.join("tsconfig.json"), "{}").unwrap();
        fs::write(modules.join("combat").join("healer.ts"), "").unwrap();
        fs::write(modules.join("combat").join(".smudgy-module-abcd"), "").unwrap();
        fs::write(modules.join("combat").join(".git").join("x.ts"), "").unwrap();

        let inventory = list_modules_in(dir.path()).unwrap();
        assert_eq!(
            inventory
                .files
                .iter()
                .map(|file| file.subpath.as_str())
                .collect::<Vec<_>>(),
            ["combat/healer.ts", "main.ts"]
        );
        assert!(inventory.warnings.is_empty());
    }

    #[test]
    fn case_collisions_are_skipped_with_a_warning_and_the_rest_load() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("modules");
        fs::create_dir_all(modules.join("combat")).unwrap();
        fs::write(modules.join("main.ts"), "").unwrap();
        fs::write(modules.join("combat").join("healer.ts"), "").unwrap();
        // A case-insensitive filesystem cannot hold both spellings; discovery is exercised on
        // the collected list directly so the pruning rule is tested everywhere.
        let files = vec![
            ModuleFile {
                subpath: "Healer.ts".into(),
                path: modules.join("Healer.ts"),
            },
            ModuleFile {
                subpath: "combat/healer.ts".into(),
                path: modules.join("combat").join("healer.ts"),
            },
            ModuleFile {
                subpath: "healer.ts".into(),
                path: modules.join("healer.ts"),
            },
            ModuleFile {
                subpath: "main.ts".into(),
                path: modules.join("main.ts"),
            },
        ];
        let mut warnings = Vec::new();
        let loadable = prune_case_collisions(files, &mut warnings);
        assert_eq!(
            loadable
                .iter()
                .map(|file| file.subpath.as_str())
                .collect::<Vec<_>>(),
            ["combat/healer.ts", "main.ts"]
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Healer.ts"));
        assert!(warnings[0].contains("healer.ts"));
        assert!(warnings[0].contains("letter case"));

        // Discovery of a real tree without collisions carries no warnings.
        let inventory = list_modules_in(dir.path()).unwrap();
        assert_eq!(inventory.files.len(), 2);
        assert!(inventory.warnings.is_empty());
    }

    #[test]
    fn missing_modules_directory_is_an_empty_inventory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            list_modules_in(dir.path()).unwrap(),
            ModuleInventory::default()
        );
    }

    #[test]
    fn ambiguous_legacy_case_keys_fail_closed() {
        let mut settings = ModuleSettings::default();
        for key in ["Healer.ts", "healer.ts"] {
            settings.modules.insert(
                key.into(),
                ModulePolicy {
                    enabled: true,
                    activation: Some(ProfileActivation::All),
                },
            );
        }
        assert_eq!(activation(&settings, "HEALER.ts"), ProfileActivation::None);
    }
}
