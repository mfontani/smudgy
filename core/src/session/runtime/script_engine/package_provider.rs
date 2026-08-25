//! The concrete [`PackageProvider`]: resolves `smudgy://` packages over the cloud package
//! API, verifies integrity, honors the per-server lockfile (auto-latest by default, opt-in
//! pin), and caches fetched module sets.
//!
//! **Per-isolate** (see `script/PACKAGE-ISOLATES-RESOLUTION.md`). One provider instance
//! serves one isolate: the engine builds a base provider for the main isolate and [`fork`]s a
//! sibling for each sandboxed package isolate. The forks share the expensive,
//! isolate-independent bits (the HTTP `client`, the content-addressed `disk_cache`, the
//! per-server `lock`) but each owns its solve state, so every isolate resolves its own closure
//! independently — within an isolate the collapse/coexist/pin rules apply, but
//! across isolates there is no collapse (main may run `util@1.4` while a sandbox runs `util@1.2`).
//!
//! Runs on the session thread under deno's event loop (driven by
//! `ModuleLoadResponse::Async` in `smudgy_script`), never under a nested `block_on` —
//! the HTTP it does is async (`PackageApiClient`).
//!
//! [`fork`]: SmudgyPackageProvider::fork

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smudgy_cloud::{
    CloudError, PackageApiClient, ResolvedDependency, ResolvedModuleWire, ResolvedPackageWire,
};
use smudgy_script::{
    PackageError, PackageKey, PackageManifest, PackageModuleSource, PackageParameter,
    PackagePermissions, PackageProvider, ReferrerRef, ResolvedPackage, canonical_url,
};

use super::package_cache::{
    CachedModule, CachedResolution, PackageCache, is_code_module, package_integrity,
};
use super::package_solver::{self, DepEdge, DepRequirement, Solve};
use crate::models::shared_packages::{self, LockedPackage, SharedPackageLock, UpdateMode};

/// Builds the **main isolate's** package provider from an optional cloud client (the engine
/// [`fork`](SmudgyPackageProvider::fork)s a sibling per sandboxed isolate). Returns `None`
/// (disabling `smudgy://` imports) when the session has no cloud client. Returns the **concrete**
/// type (not `Rc<dyn PackageProvider>`) so the engine can run the per-isolate solve + drain
/// auto-update notices after load; coerce to the trait object for the runtime via `as`.
#[must_use]
pub fn build_package_provider(
    client: Option<PackageApiClient>,
    server_name: Arc<String>,
) -> Option<Rc<SmudgyPackageProvider>> {
    client.map(|client| Rc::new(SmudgyPackageProvider::new(client, server_name)))
}

/// One auto-update notice: a package whose resolved version changed since last load.
pub type VersionChange = (String, String, String);

/// Why [`cap_version`](SmudgyPackageProvider::cap_version) refused to pick a version — the engine
/// picks the session notice from this. The three refusals need different user guidance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapRefusal {
    /// Candidate versions exist, but none's closure permission union fits the consented grant:
    /// the user must review and grant the update.
    Permissions,
    /// At least one candidate fits the grant but its closure `min_smudgy_version` floor is
    /// above this smudgy. Carries the newest such candidate's
    /// [`SmudgyVersionFloor::refusal`](shared_packages::SmudgyVersionFloor::refusal) reason.
    NeedsNewerSmudgy(String),
    /// No candidate version could be enumerated at all. The causes are alternatives: the
    /// specifier doesn't parse; the cloud doesn't know the package (never published, deleted,
    /// or a reserved owner such as a stale `smudgy://local/…` install whose folder is gone) or
    /// can't be reached about it *and* no prior resolution is cached to fall back on; or every
    /// published version is deleted/yanked. There is nothing a grant could unlock, so unlike
    /// [`Permissions`](Self::Permissions) the fix is to remove or reinstall the package.
    NoVersions,
}

/// One resolved package instance's locked dependencies: dependency package →
/// `(locked version, is_exact_pin)`.
type LockedDeps = HashMap<PackageKey, (String, bool)>;

/// Staleness bound on the wire-metadata memo: the wire carries *presigned* module URLs
/// (900 s server TTL), so a memo hit older than this refetches rather than hand an
/// expired URL to a blob-cache-miss body fetch. Every hit within one engine build — the
/// burst the memo exists to collapse — lands far inside the bound.
const WIRE_MEMO_TTL: Duration = Duration::from_mins(10);

/// Wire-metadata memo entries: the fetch instant (for the [`WIRE_MEMO_TTL`] staleness
/// bound) and the resolve wire itself, keyed by `(package, concrete version)`.
type WireMemo = HashMap<(PackageKey, String), (Instant, Rc<ResolvedPackageWire>)>;

/// The slice of a resolve wire the metadata walks consume: the concrete version, the
/// parsed manifest, and the locked dependency edges. Divorcing the walks from the full
/// wire lets a cached [`CachedResolution`] serve them without the network — see
/// [`fetch_walk_meta`](SmudgyPackageProvider::fetch_walk_meta).
struct WalkMeta {
    version: String,
    manifest: Option<PackageManifest>,
    dependencies: Vec<ResolvedDependency>,
}

/// Resolves and fetches `smudgy://` packages over the cloud API for **one isolate**. Built once
/// for the main isolate, then [`fork`](Self::fork)ed per sandboxed package isolate; the forks
/// share `client` / `disk_cache` / `lock` and each owns its solve state (`PACKAGE-ISOLATES-RESOLUTION.md`).
pub struct SmudgyPackageProvider {
    client: PackageApiClient,
    server_name: Arc<String>,
    /// In-memory view of the server lockfile (modes + last-resolved versions). **Shared** across
    /// every isolate's provider (one `Rc` per session) so partitioned lockfile writes stay
    /// consistent: each install belongs to exactly one isolate (`PACKAGE-ISOLATES-RESOLUTION.md`),
    /// and a shared view means one fork's `record_resolution` can't clobber another's via a
    /// stale snapshot.
    lock: Rc<RefCell<SharedPackageLock>>,
    /// Auto packages whose resolved version changed vs the lockfile this load —
    /// `(specifier, from_version, to_version)` — drained by the engine for a session line.
    /// Naturally quiet under staged serving (a load that serves what the lockfile stages
    /// records no change): it fires only where a network resolve actually moves the
    /// version, such as a never-resolved install's first solve or a fallback path.
    version_changes: RefCell<Vec<VersionChange>>,
    /// Fetched module sets, keyed by `(package, resolved version)`.
    cache: RefCell<HashMap<(PackageKey, String), Rc<ResolvedPackage>>>,
    /// The version each package resolved to in this isolate (so repeated imports within it agree).
    resolved_versions: RefCell<HashMap<PackageKey, String>>,
    /// Each resolved package *instance*'s locked deps: importing `(package, version)` →
    /// (dependency package → `(locked version, is_exact_pin)`). Keyed by version too so two
    /// coexisting versions of one importer keep distinct dep maps; the pin flag (author
    /// `@=x`) marks deps exempt from the closure's upgrade-collapse. Populated from each
    /// resolve's wire `dependencies`; drives referrer-aware selection.
    locked_deps: RefCell<HashMap<(PackageKey, String), LockedDeps>>,
    /// This isolate's cross-tree coexistence solve, computed by the `solve_closure`
    /// pre-pass over *this isolate's* closure before module loading. `None` until the pre-pass
    /// runs (or when it's skipped). Per-isolate: no collapse across isolates (`fork`).
    solve: RefCell<Option<Solve>>,
    /// The solved version each top-level install resolves to (auto installs collapse to the
    /// highest compatible locked version; user-pins keep their exact version). Lets a
    /// top-level / user import adopt the closure solve too, not just transitive edges.
    top_level_solved: RefCell<HashMap<PackageKey, String>>,
    /// Packages this isolate's solve found at ≥2 distinct versions this load — drained by the
    /// engine for a duplicate-version warning (session line + inspect note). Per-isolate, so the
    /// warning means an **intra-isolate** collision; a cross-isolate duplicate never appears here
    /// because it lives in two different providers' closures (`PACKAGE-ISOLATES-RESOLUTION.md`).
    duplicate_warnings: RefCell<Vec<(PackageKey, Vec<String>)>>,
    /// Each top-level install's declared params `(specifier, params)`, collected during the
    /// `solve_closure` pre-pass so the host can run the required-param load-gate before
    /// evaluation. Published installs only; a local dev package is the author's own.
    installed_params: RefCell<Vec<(String, Vec<PackageParameter>)>>,
    /// Each top-level install's declared `min_smudgy_version` `(specifier, raw floor)`,
    /// collected during the `solve_closure` pre-pass so the host can run the version-floor
    /// load-gate per package (refuse + notice) instead of letting the resolution-time gate
    /// fail the whole isolate load. Roots that declare no floor are absent.
    installed_min_versions: RefCell<Vec<(String, String)>>,
    /// The deno-native permission union over this isolate's closure, folded during
    /// `solve_closure` from every closure package's manifest `permissions`
    /// (`PACKAGE-ISOLATES-ENFORCEMENT.md`). The engine reads it via
    /// [`closure_permissions`](PackageProvider::closure_permissions) to build the sandboxed
    /// isolate's restricted container. Per-isolate, like the rest of the solve state.
    closure_permission_union: RefCell<PackagePermissions>,
    /// Persistent, content-addressed on-disk cache (immutable versions cached forever;
    /// enables offline + skips re-downloading bodies). `None` if it couldn't be opened.
    disk_cache: Option<PackageCache>,
    /// In-memory memo of resolve wires by `(package, concrete version)`. A published
    /// version's wire metadata is immutable, and one engine build asks for the same node
    /// from several angles (`cap_version`'s candidate walk, the closure solves,
    /// `resolve_impl`), so each node is fetched at most once per build. Entries age out
    /// after [`WIRE_MEMO_TTL`] — the wire's presigned module URLs must stay live.
    /// **Shared** across forks like `disk_cache`: the memoized facts are
    /// isolate-independent even though each isolate solves its closure independently.
    wire_memo: Rc<RefCell<WireMemo>>,
    /// The current account's nickname, for the local-package dev-override
    /// (resolve `smudgy://<yourhandle>/<name>` to a local folder). `None` when
    /// logged out / no handle allocated.
    account_nickname: Option<String>,
    /// The packages whose interop home is this provider's isolate (interop.md §3), folded
    /// like the home registry's keys. `None` until the engine calls
    /// [`set_home_packages`](PackageProvider::set_home_packages): every load is home, no
    /// scrub. Per-isolate (a fork starts unset).
    home_packages: RefCell<Option<std::collections::HashSet<PackageKey>>>,
    /// Non-home loads that had interop-handle exports scrubbed — read by the engine to
    /// dress a subsequent link failure with the scheme-import hint.
    scrubbed: RefCell<Vec<PackageKey>>,
    /// User-level (`file://`-referred) code imports of packages — read by the engine to
    /// warn when the target declares interop handles (interop.md §1/§3 residual).
    user_imports: RefCell<Vec<PackageKey>>,
    /// The isolate's image-policy membership handle
    /// ([`smudgy_cloud::image_source::HostedPackages`]): every successful **code-load**
    /// resolve inserts its identity here, so `<Image>` creator registration — which runs at
    /// module evaluation, strictly after the graph resolves — sees the package as
    /// legitimately hosted. `None` until the engine wires it (test overrides never do).
    image_hosted: RefCell<Option<smudgy_cloud::image_source::HostedPackages>>,
}

impl SmudgyPackageProvider {
    /// Creates a provider, seeding the lockfile view from disk.
    #[must_use]
    pub fn new(client: PackageApiClient, server_name: Arc<String>) -> Self {
        let lock = shared_packages::load_lock(&server_name).unwrap_or_else(|err| {
            warn!("Failed to load package lockfile for {server_name}: {err:#}");
            SharedPackageLock::default()
        });
        Self {
            client,
            server_name,
            lock: Rc::new(RefCell::new(lock)),
            version_changes: RefCell::new(Vec::new()),
            cache: RefCell::new(HashMap::new()),
            resolved_versions: RefCell::new(HashMap::new()),
            locked_deps: RefCell::new(HashMap::new()),
            solve: RefCell::new(None),
            top_level_solved: RefCell::new(HashMap::new()),
            duplicate_warnings: RefCell::new(Vec::new()),
            installed_params: RefCell::new(Vec::new()),
            installed_min_versions: RefCell::new(Vec::new()),
            closure_permission_union: RefCell::new(PackagePermissions::default()),
            disk_cache: PackageCache::new().ok(),
            wire_memo: Rc::new(RefCell::new(HashMap::new())),
            account_nickname: crate::models::auth::load_account().and_then(|a| a.nickname),
            home_packages: RefCell::new(None),
            scrubbed: RefCell::new(Vec::new()),
            user_imports: RefCell::new(Vec::new()),
            image_hosted: RefCell::new(None),
        }
    }

    /// Wire this isolate's image-policy membership handle: every subsequent code-load
    /// resolve records its identity (and local-override-ness) there. Call before module
    /// loading — resolution happens during graph load, evaluation (and thus `<Image>`
    /// creator registration) strictly after.
    pub fn set_image_hosted(&self, hosted: smudgy_cloud::image_source::HostedPackages) {
        *self.image_hosted.borrow_mut() = Some(hosted);
    }

    /// Build a sibling provider for another isolate (`PACKAGE-ISOLATES-RESOLUTION.md`). Each
    /// isolate resolves its closure independently, so the fork starts with
    /// its **own** empty solve state; the expensive, isolate-independent bits are shared by cheap
    /// clone / `Rc`:
    ///
    /// - `client` — clones share the connection pool; fetching bytes is isolate-independent.
    /// - `disk_cache` — content-addressed immutable blobs; a hit in one isolate serves all.
    /// - `wire_memo` — immutable per-version resolve wires; one fetch serves every
    ///   isolate's metadata walk.
    /// - `lock` — one in-memory view per session (`Rc`), so partitioned lockfile writes across
    ///   isolates stay consistent.
    ///
    /// Within the fork's isolate the collapse/coexist/pin rules apply; across isolates
    /// there is no collapse (main may land `util@1.4` while a sandboxed isolate lands `util@1.2` —
    /// different heaps, nothing to collapse). Each instance runs the same solver over its own closure.
    #[must_use]
    pub fn fork(&self) -> Self {
        Self {
            client: self.client.clone(),
            server_name: Arc::clone(&self.server_name),
            lock: Rc::clone(&self.lock),
            disk_cache: self.disk_cache.clone(),
            // Shared like the disk cache: wire metadata is an isolate-independent,
            // immutable fact, so one fetch serves every isolate's walk.
            wire_memo: Rc::clone(&self.wire_memo),
            account_nickname: self.account_nickname.clone(),
            // Per-isolate solve state — each starts empty and solves its own closure.
            version_changes: RefCell::new(Vec::new()),
            cache: RefCell::new(HashMap::new()),
            resolved_versions: RefCell::new(HashMap::new()),
            locked_deps: RefCell::new(HashMap::new()),
            solve: RefCell::new(None),
            top_level_solved: RefCell::new(HashMap::new()),
            duplicate_warnings: RefCell::new(Vec::new()),
            installed_params: RefCell::new(Vec::new()),
            installed_min_versions: RefCell::new(Vec::new()),
            closure_permission_union: RefCell::new(PackagePermissions::default()),
            // Interop-home + diagnostic tracking is per-isolate too: the engine configures
            // each fork for the isolate it serves.
            home_packages: RefCell::new(None),
            scrubbed: RefCell::new(Vec::new()),
            user_imports: RefCell::new(Vec::new()),
            // Per-isolate: the engine wires each fork to its own isolate's image policy.
            image_hosted: RefCell::new(None),
        }
    }

    /// If the current account is authoring a local package matching `key`, resolve it
    /// from `<server>/packages/<name>/` (npm-link-style override) so it's tested under
    /// its real specifier before publishing. On a code load (`track`) it's cached like a
    /// normal resolution so install + import share one instance; a stub fetch
    /// (`track == false`) leaves the cache untouched, so consuming a local producer over
    /// `smudgy:state|events/…` records no code-load footprint (`loaded_packages()` /
    /// the stumble diagnostic stays quiet) — the same contract the network + offline
    /// branches keep.
    ///
    /// Known gap: a local package's `locked_deps` are **not** populated here — its
    /// manifest carries dependency *ranges*, not the concrete versions a published resolve
    /// supplies, so deriving them would need the same async range-resolution `publish`
    /// does. So a locally-developed package's transitive `smudgy://` imports resolve via
    /// the lockfile / latest rather than referrer-locked versions until it is published.
    /// The `resolved_versions` write is left to the caller so it can gate on top-level.
    fn try_local_override(&self, key: &PackageKey, track: bool) -> Option<Rc<ResolvedPackage>> {
        if !self.is_local_owner_segment(&key.owner) {
            return None;
        }
        let local = crate::models::local_packages::load_local_package(&self.server_name, &key.name)
            .ok()
            .flatten()?;
        let version = local.manifest.version.clone();
        let modules = local
            .modules
            .into_iter()
            // Local modules are loaded as text; a binary local module (any bytes are stored, but
            // loading binaries is out of scope) is SKIPPED rather than fed lossy garbage to v8.
            .filter_map(|m| {
                String::from_utf8(m.content)
                    .ok()
                    .map(|text| PackageModuleSource {
                        subpath: m.subpath,
                        text,
                    })
            })
            .collect();
        let resolved = Rc::new(ResolvedPackage {
            key: key.clone(),
            resolved_version: version.clone(),
            manifest: local.manifest,
            integrity: "local".to_string(),
            modules,
        });
        // Only a code load records the served set: a stub fetch (`track == false`) of a
        // local producer must leave no code-load footprint, exactly as the network +
        // offline branches gate their inserts on `track`. Otherwise consuming a
        // locally-authored producer over `smudgy:events/…` would land it in this isolate's
        // `cache`, and the code-import stumble diagnostic would misfire on a consumer that
        // never imported the producer's code.
        if track {
            self.cache
                .borrow_mut()
                .insert((key.clone(), version.clone()), resolved.clone());
        }
        Some(resolved)
    }

    /// Whether `key` resolves to a **local dev-override** — the account (or, signed out, the
    /// reserved [`LOCAL_OWNER`](crate::models::local_packages::LOCAL_OWNER) placeholder)
    /// authoring its own package under `<server>/packages/<name>/` (the [`try_local_override`]
    /// shadow path). The engine reads this to (a) skip version-capping — the local folder is the
    /// version on disk — and (b) source the isolate's enforced grant from the package's OWN
    /// on-disk manifest rather than a consented closure union (a local package has no consent
    /// record; the manifest IS its grant table). A local package therefore still runs
    /// **sandboxed to its manifest**, NOT allow-all — `PACKAGE-ISOLATES-ENFORCEMENT.md`. Allow-all
    /// is opt-in only, via the separate **trust** escape hatch, which promotes the package to the
    /// main isolate and never reaches this path.
    ///
    /// [`try_local_override`]: Self::try_local_override
    #[must_use]
    pub fn is_local_override(&self, key: &PackageKey) -> bool {
        self.is_local_owner_segment(&key.owner)
            && crate::models::local_packages::load_local_package(&self.server_name, &key.name)
                .ok()
                .flatten()
                .is_some()
    }

    /// The owner segment local packages run under: the account nickname when signed in, else the
    /// reserved [`LOCAL_OWNER`](crate::models::local_packages::LOCAL_OWNER) placeholder so local
    /// packages still resolve, enable, and run while signed out (matching the UI's
    /// `local_own_spec`).
    fn local_owner(&self) -> &str {
        self.account_nickname
            .as_deref()
            .unwrap_or(crate::models::local_packages::LOCAL_OWNER)
    }

    /// Whether `owner` addresses this account's own local packages: the current
    /// [`local_owner`](Self::local_owner), or the reserved `local` placeholder regardless of
    /// sign-in state. An install written signed out (`smudgy://local/<name>`) must keep
    /// resolving to its folder after the account gains a nickname — the owner segment records
    /// the sign-in state at install time, not a different package. The placeholder is reserved
    /// server-side, so accepting it never shadows a real cloud package.
    fn is_local_owner_segment(&self, owner: &str) -> bool {
        owner == crate::models::local_packages::LOCAL_OWNER || owner == self.local_owner()
    }

    /// Build a resolved package entirely from the on-disk cache (for offline use). `None`
    /// unless the version's metadata and every **code** module body are cached (assets are
    /// lazy — see the resolve loop — and must not hold offline loads hostage).
    fn build_from_cache(&self, key: &PackageKey, version: &str) -> Option<Rc<ResolvedPackage>> {
        let cache = self.disk_cache.as_ref()?;
        let meta = cache.read_meta(key, version)?;
        if !cache.has_all_code_blobs(&meta) {
            return None;
        }
        // Repopulate this instance's locked deps so its transitive imports stay
        // referrer-aware offline, matching the network path. Empty for cache
        // entries written before the field existed -> graceful referrer-blind fallback.
        self.store_locked_deps(key, &meta.version, &meta.dependencies);
        let mut modules = Vec::with_capacity(meta.modules.len());
        for module in &meta.modules {
            if !is_code_module(&module.media_type, &module.subpath) {
                continue;
            }
            modules.push(PackageModuleSource {
                subpath: module.subpath.clone(),
                text: cache.read_blob(&module.content_hash)?,
            });
        }
        Some(Rc::new(ResolvedPackage {
            key: key.clone(),
            resolved_version: meta.version.clone(),
            manifest: meta.manifest.clone(),
            integrity: meta.integrity.clone(),
            modules,
        }))
    }

    /// Resolve `key`'s wire metadata at `version` (`None` = latest) over the network,
    /// memoized by concrete version. The solve pre-pass, `cap_version`'s candidate walk,
    /// and `resolve_impl` all ask for the same nodes; a published version's metadata is
    /// immutable, so one fetch serves them all (entries age out after [`WIRE_MEMO_TTL`]
    /// to keep the wire's presigned module URLs live). Every successful fetch also
    /// persists the version's [`CachedResolution`] — metadata walks warm the offline
    /// cache, not just code loads. `None` asks the network unconditionally: what
    /// "latest" means is a mutable fact, though the concrete answer still lands in the
    /// memo for later versioned asks.
    async fn fetch_wire(
        &self,
        key: &PackageKey,
        version: Option<&str>,
    ) -> Result<Rc<ResolvedPackageWire>, CloudError> {
        if let Some(version) = version {
            let memoized = self
                .wire_memo
                .borrow()
                .get(&(key.clone(), version.to_string()))
                .filter(|(fetched_at, _)| fetched_at.elapsed() < WIRE_MEMO_TTL)
                .map(|(_, wire)| Rc::clone(wire));
            if let Some(wire) = memoized {
                return Ok(wire);
            }
        }
        let wire = Rc::new(
            self.client
                .resolve_package(&key.owner, &key.name, version)
                .await?,
        );
        self.write_meta_for_wire(key, &wire);
        self.wire_memo.borrow_mut().insert(
            (key.clone(), wire.version.clone()),
            (Instant::now(), Rc::clone(&wire)),
        );
        Ok(wire)
    }

    /// Persist a resolve wire's metadata to the disk cache, making the version
    /// offline-reconstructable (once its code blobs are cached too) and its transitive
    /// imports referrer-aware offline. Runs on every successful network resolve — the
    /// solve/cap metadata walks warm the cache alongside code loads. Write-once in the
    /// common case (versions are immutable), but self-healing: a cache file written
    /// before `CachedResolution` carried dependency edges (or with fewer modules than
    /// the wire) is refreshed rather than frozen — see
    /// [`PackageCache::refresh_meta`]. Best-effort: no cache, an unparseable manifest,
    /// or a failed write costs only the cache entry (the code-load path surfaces
    /// `InvalidManifest` itself).
    fn write_meta_for_wire(&self, key: &PackageKey, wire: &ResolvedPackageWire) {
        let Some(cache) = &self.disk_cache else {
            return;
        };
        let Ok(manifest) = PackageManifest::parse(&wire.manifest.to_string()) else {
            return;
        };
        let meta = CachedResolution {
            version: wire.version.clone(),
            integrity: package_integrity(&wire.modules),
            manifest,
            modules: wire
                .modules
                .iter()
                .map(|module| CachedModule {
                    subpath: module.subpath.clone(),
                    content_hash: module.content_hash.clone(),
                    media_type: module.media_type.clone(),
                })
                .collect(),
            dependencies: wire.dependencies.clone(),
        };
        let _ = cache.refresh_meta(key, &wire.version, &meta);
    }

    /// Resolve the metadata slice the closure walks consume — the concrete version, the
    /// parsed manifest (`None` when it won't parse; the walk degrades, never aborts),
    /// and the locked dependency edges — **cache-first**: a concrete `version` whose
    /// [`CachedResolution`] is on disk is served from it, because a published version's
    /// metadata is immutable and the cached copy IS the network's answer. Only a cache
    /// gap, or `version == None` (what "latest" means is a mutable question), reaches
    /// [`fetch_wire`](Self::fetch_wire). Module URLs are the one thing the cache cannot
    /// supply, and the walks never read them — code loads go through `resolve_impl`,
    /// which fetches its own wire when the blob cache has a gap.
    async fn fetch_walk_meta(
        &self,
        key: &PackageKey,
        version: Option<&str>,
    ) -> Result<WalkMeta, CloudError> {
        if let Some(version) = version
            && let Some(meta) = self
                .disk_cache
                .as_ref()
                .and_then(|cache| cache.read_meta(key, version))
        {
            return Ok(WalkMeta {
                version: meta.version,
                manifest: Some(meta.manifest),
                dependencies: meta.dependencies,
            });
        }
        let wire = self.fetch_wire(key, version).await?;
        Ok(WalkMeta {
            version: wire.version.clone(),
            manifest: PackageManifest::parse(&wire.manifest.to_string()).ok(),
            dependencies: wire.dependencies.clone(),
        })
    }

    /// Persists a package's resolved version — and, for a network-verified load, its
    /// integrity — to the lockfile (for offline reuse and reproducibility).
    /// `verified_integrity` is `Some` only when this load verified the content against
    /// the registry (the network resolve path); a cache-first serve passes `None` and
    /// leaves any existing stamp untouched — `integrity` records what was most recently
    /// *verified*, and reading a trusted cache file verifies nothing. Best-effort: a
    /// write failure is logged, not fatal.
    ///
    /// Persistence is an entry-level read-modify-write against the **on-disk** lock — never a
    /// flush of this session's whole in-memory view, which can be seconds stale by the time a
    /// resolve completes and would silently clobber concurrent Automations-window writes (an
    /// uninstalled entry would resurrect; a fresh enable/disable would revert). An entry that
    /// was installed when this session loaded but is gone from disk was uninstalled meanwhile:
    /// its resolution metadata dies with it.
    ///
    /// One write is refused outright: an on-disk entry staging a DIFFERENT version with
    /// `integrity` unstamped is a background stage this load did not serve (staging
    /// clears the stamp; only a load that serves the staged version re-stamps it).
    /// Recording this load's engine-build-start decision over it would silently revert
    /// the pending stage, so the entry — disk and in-memory view alike — is left
    /// exactly as staged. A stamped entry records as always, downgrades included.
    fn record_resolution(&self, specifier: &str, version: &str, verified_integrity: Option<&str>) {
        let fresh_entry = || LockedPackage {
            specifier: specifier.to_string(),
            mode: UpdateMode::Auto,
            last_resolved_version: Some(version.to_string()),
            integrity: verified_integrity.map(str::to_string),
            dismissed_update_version: None,
            trusted: false,
            consented_permissions: None,
            enabled: true,
            installed_as_requirement: false,
            audio_used: false,
        };
        // How an existing entry adopts this resolution: the version always lands; the
        // integrity stamp only from a verified load. An unverified serve that MOVES
        // the version clears the old version's stamp rather than letting it lie
        // (integrity describes `last_resolved_version`, nothing else).
        let apply = |entry: &mut LockedPackage| {
            let moved = entry.last_resolved_version.as_deref() != Some(version);
            entry.last_resolved_version = Some(version.to_string());
            match verified_integrity {
                Some(integrity) => entry.integrity = Some(integrity.to_string()),
                None if moved => entry.integrity = None,
                None => {}
            }
        };
        let known_install = self
            .lock
            .borrow()
            .packages
            .iter()
            .any(|p| p.specifier == specifier);
        let mut stage_pending = false;
        let persisted = shared_packages::mutate_lock(&self.server_name, |disk| {
            if let Some(entry) = disk.packages.iter_mut().find(|p| p.specifier == specifier) {
                let staged_elsewhere = entry.integrity.is_none()
                    && entry
                        .last_resolved_version
                        .as_deref()
                        .is_some_and(|staged| staged != version);
                if staged_elsewhere {
                    stage_pending = true;
                    return Ok(((), false));
                }
                apply(entry);
                Ok(((), true))
            } else if known_install {
                // Uninstalled since this session loaded — don't resurrect it just to stamp
                // resolution metadata on it.
                Ok(((), false))
            } else {
                // Not an install at all (a top-level resolve outside the lockfile): record it
                // on disk the same way it is recorded in memory.
                disk.upsert(fresh_entry());
                Ok(((), true))
            }
        });
        if let Err(err) = persisted {
            warn!("Failed to persist package lock for {specifier}: {err:#}");
        }
        if stage_pending {
            return;
        }
        let mut lock = self.lock.borrow_mut();
        if let Some(entry) = lock.packages.iter_mut().find(|p| p.specifier == specifier) {
            // An AUTO package that resolved to a new version since last load: record a
            // notice (a pin, or a first-ever resolve with no prior, never notifies).
            if matches!(entry.mode, UpdateMode::Auto)
                && let Some(prior) = &entry.last_resolved_version
                && prior != version
            {
                self.version_changes.borrow_mut().push((
                    specifier.to_string(),
                    prior.clone(),
                    version.to_string(),
                ));
            }
            apply(entry);
        } else {
            lock.upsert(fresh_entry());
        }
    }

    /// Record a resolved package instance's locked deps (keyed by `(package, version)`), so
    /// a later import made from inside *that version* resolves at the version it locked.
    /// Each dep's declared range is classified into an exact-pin flag (author `@=x`).
    fn store_locked_deps(&self, importer: &PackageKey, version: &str, deps: &[ResolvedDependency]) {
        if deps.is_empty() {
            return;
        }
        let map: LockedDeps = deps
            .iter()
            .filter_map(|dep| {
                let key = dep_package_key(&dep.owner_nickname, &dep.name)?;
                Some((
                    key,
                    (
                        dep.resolved_version.clone(),
                        package_solver::is_exact_pin(&dep.range),
                    ),
                ))
            })
            .collect();
        if !map.is_empty() {
            self.locked_deps
                .borrow_mut()
                .insert((importer.clone(), version.to_string()), map);
        }
    }

    /// The `(locked version, is_exact_pin)` the `referrer` instance recorded for the
    /// dependency `target`, if any.
    fn referrer_locked_version(
        &self,
        referrer: &ReferrerRef,
        target: &PackageKey,
    ) -> Option<(String, bool)> {
        self.locked_deps
            .borrow()
            .get(&(referrer.key.clone(), referrer.version.clone()))?
            .get(target)
            .cloned()
    }

    /// Apply the cross-tree solve to a referrer edge's locked version: a non-pin
    /// dep collapses to the highest compatible version any dependent locked; a pin keeps
    /// its exact version. With no solve (pre-pass skipped), the locked version is returned.
    fn solve_resolve(&self, target: &PackageKey, version: &str, is_pin: bool) -> String {
        self.solve.borrow().as_ref().map_or_else(
            || version.to_string(),
            |solve| solve.resolve(target, version, is_pin),
        )
    }

    /// Pre-pass: walk the install closure to gather every requirement on each
    /// shared package, solve the cross-tree collapse/coexistence, and stash the result so
    /// both the referrer-aware `resolve_package` and the top-level installs read solved
    /// versions. Records the duplicate-version warning set over the *actually-loaded*
    /// closure. Best-effort: a package that can't be resolved (offline / missing) is
    /// skipped, degrading that subtree to per-edge selection.
    ///
    /// Single-pass over the locked closure: the collapsed version is always one of the
    /// locked versions, so it is discovered and later cached by the lazy load.
    pub async fn solve_closure(&self, installs: &[String]) {
        self.solve_closure_inner(installs, &HashMap::new()).await;
    }

    /// Like [`solve_closure`](Self::solve_closure), but each listed install resolves its **root** at
    /// the given capped version — the highest version whose closure permission union fits the user's
    /// consented grant (`script/PACKAGE-ISOLATES-CONSENT-TRUST.md`) — instead of latest. Used for
    /// sandboxed isolates so a newer version that demands more access than was granted is never
    /// loaded (the package stays at the capped version; if nothing fits the engine doesn't load it
    /// at all). Trusted packages run allow-all and are never capped, so the main isolate keeps using
    /// the plain `solve_closure`.
    pub async fn solve_closure_capped(&self, installs: &[(String, String)]) {
        let forced: HashMap<String, String> = installs.iter().cloned().collect();
        let specifiers: Vec<String> = installs.iter().map(|(spec, _)| spec.clone()).collect();
        self.solve_closure_inner(&specifiers, &forced).await;
    }

    #[allow(clippy::too_many_lines)]
    async fn solve_closure_inner(
        &self,
        installs: &[String],
        forced_root_versions: &HashMap<String, String>,
    ) {
        let mut requirements: Vec<DepRequirement> = Vec::new();
        let mut roots: Vec<DepRequirement> = Vec::new();
        let mut edges: Vec<DepEdge> = Vec::new();
        let mut installed_params: Vec<(String, Vec<PackageParameter>)> = Vec::new();
        let mut installed_min_versions: Vec<(String, String)> = Vec::new();
        // The deno-native permission union over the whole closure — folded from each
        // distinct closure package's manifest, read by the engine to sandbox this isolate.
        let mut permissions_union = PackagePermissions::default();
        let mut seen: HashSet<(PackageKey, String)> = HashSet::new();
        // (package, forced version | None = latest/user-pin, is_pin, is_top_level).
        let mut stack: Vec<(PackageKey, Option<String>, bool, bool)> = Vec::new();
        for specifier in installs {
            let Ok(spec) = smudgy_script::SmudgySpecifier::parse(specifier) else {
                continue;
            };
            // A permission-capped root resolves at exactly its capped version (treated as a pin for
            // this load, exempt from collapse). Otherwise honor a user install-pin. An Auto root
            // with a staged prior resolution solves AT that staged version (still collapsible, not
            // a pin): session start loads what the lockfile stages, and version movement belongs
            // to whatever advances `last_resolved_version`, not to this walk. Only a
            // never-resolved Auto root still asks the network what latest means.
            let (forced, is_pin) = if let Some(version) = forced_root_versions.get(specifier) {
                (Some(version.clone()), true)
            } else {
                let lock = self.lock.borrow();
                let entry = lock.find(specifier);
                match entry.and_then(|locked| locked.pinned_version().map(str::to_string)) {
                    Some(pin) => (Some(pin), true),
                    None => (
                        entry.and_then(|locked| locked.last_resolved_version.clone()),
                        false,
                    ),
                }
            };
            stack.push((spec.package_key(), forced, is_pin, true));
        }

        while let Some((key, forced, is_pin, is_top_level)) = stack.pop() {
            // Dedup BEFORE the fetch when the node arrives with a concrete version
            // (every dep edge does): a diamond reached by N paths costs one fetch, not
            // N. The duplicate requirement is still counted — the solver weighs every
            // edge. Top-level roots fall through: they also register as roots and
            // collect params/floors from the manifest (their fetch is a cache or memo
            // hit when a dep edge got there first).
            if !is_top_level
                && let Some(version) = &forced
                && seen.contains(&(key.clone(), version.clone()))
            {
                requirements.push(DepRequirement {
                    package: key.clone(),
                    version: version.clone(),
                    is_pin,
                });
                continue;
            }
            let Ok(meta) = self.fetch_walk_meta(&key, forced.as_deref()).await else {
                continue;
            };
            let version = meta.version.clone();
            let requirement = DepRequirement {
                package: key.clone(),
                version: version.clone(),
                is_pin,
            };
            requirements.push(requirement.clone());
            // The top-level params gate and the closure permission union both read the
            // manifest (an unparseable one degrades both, never aborts the walk).
            let manifest = meta.manifest;
            if is_top_level {
                roots.push(requirement);
                if let Some(manifest) = &manifest {
                    // Collect the install's declared params for the required-param load-gate.
                    if !manifest.params.is_empty() {
                        installed_params.push((key.to_user_specifier(), manifest.params.clone()));
                    }
                    // And its declared version floor for the version-floor load-gate.
                    if let Some(min) = &manifest.min_smudgy_version {
                        installed_min_versions.push((key.to_user_specifier(), min.clone()));
                    }
                }
            }
            // Walk each distinct (package, version) once, but count every requirement.
            if !seen.insert((key.clone(), version.clone())) {
                continue;
            }
            // Fold this closure package's declared permissions into the isolate union:
            // every distinct closure package contributes (root and transitive deps alike).
            if let Some(manifest) = &manifest {
                permissions_union.merge(&manifest.permissions);
            }
            for dep in &meta.dependencies {
                let Some(dep_key) = dep_package_key(&dep.owner_nickname, &dep.name) else {
                    continue;
                };
                let dep_pin = package_solver::is_exact_pin(&dep.range);
                requirements.push(DepRequirement {
                    package: dep_key.clone(),
                    version: dep.resolved_version.clone(),
                    is_pin: dep_pin,
                });
                edges.push(DepEdge {
                    importer: key.clone(),
                    importer_version: version.clone(),
                    dep: dep_key.clone(),
                    dep_version: dep.resolved_version.clone(),
                    dep_is_pin: dep_pin,
                });
                stack.push((dep_key, Some(dep.resolved_version.clone()), dep_pin, false));
            }
        }

        let solve = package_solver::solve(&requirements);
        // Each top-level install loads at its solved version (auto -> collapsed-highest;
        // user-pin -> exact), so it joins the same instance as transitive edges in its
        // class instead of floating to non-yanked latest.
        let top_level_solved = roots
            .iter()
            .map(|root| {
                (
                    root.package.clone(),
                    solve.resolve(&root.package, &root.version, root.is_pin),
                )
            })
            .collect();
        // Warn over the ACTUALLY-loaded closure (BFS from solved roots), so deps of a
        // collapsed-away version don't produce a phantom coexistence warning.
        *self.duplicate_warnings.borrow_mut() = solve.loaded_duplicates(&roots, &edges);
        *self.top_level_solved.borrow_mut() = top_level_solved;
        *self.installed_params.borrow_mut() = installed_params;
        *self.installed_min_versions.borrow_mut() = installed_min_versions;
        *self.closure_permission_union.borrow_mut() = permissions_union;
        *self.solve.borrow_mut() = Some(solve);
    }

    /// Permission- and version-floor-aware version selection
    /// (`script/PACKAGE-ISOLATES-CONSENT-TRUST.md`): the highest version of `specifier` whose
    /// **closure** permission union fits the user's `consented` grant *and* whose closure
    /// `min_smudgy_version` floor this smudgy satisfies. `Err` is a [`CapRefusal`] saying why the
    /// package must not load — [`CapRefusal::Permissions`] when candidate versions exist but every
    /// one demands more access than was granted, [`CapRefusal::NeedsNewerSmudgy`] when one fits the
    /// grant but needs a newer smudgy, or [`CapRefusal::NoVersions`] when no candidate version
    /// could be enumerated at all (a grant can't fix that). The caller feeds the chosen version to
    /// [`solve_closure_capped`](Self::solve_closure_capped).
    ///
    /// - A user **install-pin** is exact: the only candidate is the pinned version (it loads iff its
    ///   closure fits, else refused).
    /// - Otherwise (auto): walk the package's published, non-deleted, non-yanked versions newest-first
    ///   and return the first whose closure union `is_within` consent and whose closure floor is
    ///   satisfied. Walking by semver-descending order means the package auto-upgrades as far as
    ///   the grant and this smudgy allow, and otherwise stays at the highest fitting (typically
    ///   the previously-consented / previously-loadable) version.
    ///
    /// Each candidate's closure union + floor is computed by
    /// [`closure_union_for`](Self::closure_union_for); the common case (latest already fits)
    /// costs a single check.
    pub async fn cap_version(
        &self,
        specifier: &str,
        consented: &PackagePermissions,
    ) -> Result<String, CapRefusal> {
        let Ok(spec) = smudgy_script::SmudgySpecifier::parse(specifier) else {
            return Err(CapRefusal::NoVersions);
        };
        let key = spec.package_key();

        let pin = self
            .lock
            .borrow()
            .find(specifier)
            .and_then(|locked| locked.pinned_version().map(str::to_string));
        let candidates: Vec<String> = if let Some(pin) = pin {
            vec![pin]
        } else {
            // Resolve once to learn the package id (and a latest-version fallback), then list its
            // versions newest-first. If listing fails, fall back to just the latest.
            match self.fetch_wire(&key, None).await {
                Ok(latest) => match self.client.list_versions(latest.package_id).await {
                    Ok(list) => {
                        let mut versions: Vec<semver::Version> = list
                            .into_iter()
                            // Skip hard-deleted (content gone, would 404) and yanked numbers, matching
                            // normal auto-resolution — a yanked version drops out of latest/auto and is
                            // only reachable by an exact pin (which takes the `pin` branch above).
                            .filter(|v| !v.deleted && !v.yanked)
                            .filter_map(|v| semver::Version::parse(&v.version).ok())
                            .collect();
                        versions.sort();
                        versions.reverse();
                        versions.into_iter().map(|v| v.to_string()).collect()
                    }
                    Err(_) => vec![latest.version.clone()],
                },
                // Offline, or signed out and the package isn't public (the anonymous viewer
                // can't see it): we can't shop for a newer version, so fall back to the last
                // version we resolved. It's cached and already consented, so an installed
                // auto-update package keeps running without the cloud instead of silently
                // dropping out. (Its closure union is recomputed below; when the cloud is
                // unreachable that resolves to the empty set, which fits the prior consent,
                // so the cached version loads with exactly the permissions already granted.)
                Err(_) => self
                    .lock
                    .borrow()
                    .find(specifier)
                    .and_then(|locked| locked.last_resolved_version.clone())
                    .into_iter()
                    .collect(),
            }
        };

        // Nothing to consider at all — the install target no longer exists anywhere (and no
        // grant or smudgy update could change that), distinct from candidates that exist but
        // don't fit.
        if candidates.is_empty() {
            return Err(CapRefusal::NoVersions);
        }
        // The newest consent-fitting candidate refused only by its version floor, if any —
        // the actionable refusal ("update smudgy") when nothing loads.
        let running = shared_packages::running_smudgy_release();
        let mut floor_refusal: Option<String> = None;
        for candidate in candidates {
            let (union, floor) = self.closure_union_for(&key, &candidate).await;
            if !union.is_within(consented) {
                continue;
            }
            if let Some(reason) = floor.refusal(&running) {
                if floor_refusal.is_none() {
                    floor_refusal = Some(reason);
                }
                continue;
            }
            return Ok(candidate);
        }
        Err(floor_refusal.map_or(CapRefusal::Permissions, CapRefusal::NeedsNewerSmudgy))
    }

    /// The deno-native permission union and `min_smudgy_version` floor over the closure rooted
    /// at `root_key@root_version` — the same fold [`solve_closure`](Self::solve_closure) does,
    /// but for a *specific* root version and without mutating solve state, so
    /// [`cap_version`](Self::cap_version) can evaluate candidate versions. Best-effort (a dep
    /// that won't resolve is skipped) and dedups by `(package, version)` so diamonds/cycles
    /// terminate. Each dep is resolved at its locked `resolved_version`. A manifest that won't
    /// parse contributes to neither fold; the resolution-time `InvalidManifest` refusal covers
    /// that package if it is actually loaded.
    async fn closure_union_for(
        &self,
        root_key: &PackageKey,
        root_version: &str,
    ) -> (PackagePermissions, shared_packages::SmudgyVersionFloor) {
        let mut union = PackagePermissions::default();
        let mut floor = shared_packages::SmudgyVersionFloor::default();
        let mut seen: HashSet<(PackageKey, String)> = HashSet::new();
        let mut stack: Vec<(PackageKey, String)> =
            vec![(root_key.clone(), root_version.to_string())];
        while let Some((key, version)) = stack.pop() {
            if !seen.insert((key.clone(), version.clone())) {
                continue;
            }
            let Ok(wire) = self.fetch_wire(&key, Some(&version)).await else {
                continue;
            };
            if let Ok(manifest) = PackageManifest::parse(&wire.manifest.to_string()) {
                union.merge(&manifest.permissions);
                floor.fold(&key.name, manifest.min_smudgy_version.as_deref());
            }
            for dep in &wire.dependencies {
                if let Some(dep_key) = dep_package_key(&dep.owner_nickname, &dep.name) {
                    stack.push((dep_key, dep.resolved_version.clone()));
                }
            }
        }
        (union, floor)
    }

    /// The fold of [`closure_union_for`](Self::closure_union_for) computed **entirely
    /// from the disk cache** — the staged-version consent verification at session
    /// start. Walks `root_key@root_version`'s dependency closure over cached
    /// [`CachedResolution`]s at their locked versions, unioning each distinct node's
    /// manifest permissions and folding its `min_smudgy_version` floor. Returns `None`
    /// on ANY missing meta (or no disk cache at all): an incomplete fold proves
    /// nothing, and the caller must fall back to the network path, which keeps the
    /// [`CapRefusal`] semantics and their session notices. Unlike the network fold this
    /// one fails closed on gaps — best-effort skipping is only sound where the network
    /// was actually asked.
    #[must_use]
    pub fn closure_union_from_cache(
        &self,
        root_key: &PackageKey,
        root_version: &str,
    ) -> Option<(PackagePermissions, shared_packages::SmudgyVersionFloor)> {
        let cache = self.disk_cache.as_ref()?;
        let mut union = PackagePermissions::default();
        let mut floor = shared_packages::SmudgyVersionFloor::default();
        let mut seen: HashSet<(PackageKey, String)> = HashSet::new();
        let mut stack: Vec<(PackageKey, String)> =
            vec![(root_key.clone(), root_version.to_string())];
        while let Some((key, version)) = stack.pop() {
            if !seen.insert((key.clone(), version.clone())) {
                continue;
            }
            let meta = cache.read_meta(&key, &version)?;
            union.merge(&meta.manifest.permissions);
            floor.fold(&key.name, meta.manifest.min_smudgy_version.as_deref());
            for dep in &meta.dependencies {
                if let Some(dep_key) = dep_package_key(&dep.owner_nickname, &dep.name) {
                    stack.push((dep_key, dep.resolved_version.clone()));
                }
            }
        }
        Some((union, floor))
    }

    /// Each top-level install's `(specifier, declared params)`, collected by the last
    /// `solve_closure` — the required-param load-gate's input.
    #[must_use]
    pub fn installed_params(&self) -> Vec<(String, Vec<PackageParameter>)> {
        self.installed_params.borrow().clone()
    }

    /// Each top-level install's `(specifier, declared min_smudgy_version)`, collected by the
    /// last `solve_closure` — the version-floor load-gate's input. Roots with no floor are
    /// absent.
    #[must_use]
    pub fn installed_min_versions(&self) -> Vec<(String, String)> {
        self.installed_min_versions.borrow().clone()
    }

    /// Drain the auto-update notices collected this load (the engine surfaces them as a
    /// session line — auto-update is silent except for this nudge).
    pub fn take_version_changes(&self) -> Vec<VersionChange> {
        self.version_changes.borrow_mut().drain(..).collect()
    }

    /// Drain the duplicate-version warnings the solve found this load (a package resolved
    /// to ≥2 coexisting versions — the shared-isolate side-effect-collision risk).
    pub fn take_duplicate_warnings(&self) -> Vec<(PackageKey, Vec<String>)> {
        self.duplicate_warnings.borrow_mut().drain(..).collect()
    }
}

/// Why `manifest`'s own `min_smudgy_version` floor refuses to run on this smudgy, if it
/// does — the single-manifest form of the closure fold in `closure_union_for`, used where a
/// package is gated one manifest at a time (each closure member passes through
/// `resolve_package` itself, so per-manifest checks still cover the whole closure).
fn manifest_floor_refusal(name: &str, manifest: &PackageManifest) -> Option<String> {
    let mut floor = shared_packages::SmudgyVersionFloor::default();
    floor.fold(name, manifest.min_smudgy_version.as_deref());
    floor.refusal(&shared_packages::running_smudgy_release())
}

/// Build a [`PackageKey`] from a resolve dependency's owner nickname + name.
fn dep_package_key(owner_nickname: &str, name: &str) -> Option<PackageKey> {
    if owner_nickname.is_empty() {
        return None;
    }
    Some(PackageKey {
        owner: owner_nickname.to_string(),
        name: name.to_string(),
    })
}

impl SmudgyPackageProvider {
    // Genuinely multi-path: in-session dedup, local dev-override, cache-first serve of
    // determined versions, network resolve with offline fallback, content-addressed
    // body cache, and metadata persistence.
    //
    // `track` separates a code load from a kind-scheme stub fetch: a code load records the
    // instance in `cache` (whose keys are `loaded_packages()`, the stumble diagnostic's
    // input) and, top-level, reports the resolution into the lockfile; a stub fetch
    // (`track == false`) must leave no code-load or install footprint — notably,
    // `record_resolution` would UPSERT a lock entry for an unknown package, silently
    // installing a producer someone merely consumed.
    #[allow(clippy::too_many_lines)]
    async fn resolve_impl(
        &self,
        key: &PackageKey,
        referrer: Option<&ReferrerRef>,
        track: bool,
    ) -> Result<Rc<ResolvedPackage>, PackageError> {
        let specifier = key.to_user_specifier();

        // Mode + staged version from the lockfile (don't hold the borrow over the
        // awaits below).
        let (pinned, staged) = {
            let lock = self.lock.borrow();
            let entry = lock.find(&specifier);
            (
                entry.and_then(|p| p.pinned_version().map(str::to_string)),
                entry.and_then(|p| p.staged_version().map(str::to_string)),
            )
        };

        // Version selection, refined by the closure solve:
        //  - a referrer (transitive) edge takes the version *this importer* locked,
        //    collapsed to the highest compatible version any dependent locked (a pin keeps
        //    its exact version);
        //  - a top-level / user import takes its install's solved version (auto installs
        //    also collapse to the class's highest lock — not just non-yanked latest).
        // Either falls back to the lockfile pin, then latest, when the solve has no entry.
        let solved = match referrer {
            Some(r) => self
                .referrer_locked_version(r, key)
                .map(|(version, is_pin)| self.solve_resolve(key, &version, is_pin)),
            None => self.top_level_solved.borrow().get(key).cloned(),
        };
        let selected = solved.or_else(|| pinned.clone());

        // The version this resolve is already DETERMINED to serve, decided from local
        // facts alone (this load's solve + the lockfile, never the network): a referrer
        // edge's locked (solve-collapsed) version, a top-level install's solved version,
        // the user's pin, or the staged version of an Auto install
        // ([`LockedPackage::staged_version`]). `None` only when discovery genuinely
        // needs the cloud — a never-resolved Auto root with no solve entry.
        let determined = selected.clone().or_else(|| staged.clone());

        // Already resolved this version this session → reuse that instance. Keyed by the
        // *selected* version, so two importers that locked different versions coexist (two
        // canonical URLs) while identical selections share one instance. With no explicit
        // selection (auto-latest), fall back to the prior session resolve for this key.
        let dedup_version = selected
            .clone()
            .or_else(|| self.resolved_versions.borrow().get(key).cloned());
        if let Some(version) = dedup_version
            && let Some(package) = self.cache.borrow().get(&(key.clone(), version)).cloned()
        {
            return Ok(package);
        }

        // Local dev-override: a package you're authoring under <server>/packages/<name>/
        // shadows the published one, so you test it under its real specifier first.
        if let Some(local) = self.try_local_override(key, track) {
            if track && referrer.is_none() {
                self.resolved_versions
                    .borrow_mut()
                    .insert(key.clone(), local.resolved_version.clone());
            }
            return Ok(local);
        }

        // Cache-first serve: for a resolve whose version is already determined, the
        // disk cache is the PRIMARY source, not the error fallback — the version
        // names immutable published content, so cached metadata + code blobs ARE the
        // network's answer. A cache gap (or `determined == None`) falls through to the
        // network below. Stub fetches (`track == false`) ride this path too — a stub
        // of an installed producer must see the version the producer's isolate
        // actually runs (its staged/pinned/solved version, not latest) — while their
        // no-footprint contract is kept by the `track` gates below.
        if let Some(version) = &determined
            && let Some(package) = self.build_from_cache(key, version)
        {
            // The cached meta was written by a resolve that passed the version-floor
            // gate — but under a possibly NEWER smudgy since downgraded, so re-check
            // the cached manifest's floor before serving (the same guard the offline
            // fallback below applies).
            if let Some(reason) = manifest_floor_refusal(&key.name, &package.manifest) {
                return Err(PackageError::Other(format!(
                    "{specifier} not loaded: {reason}"
                )));
            }
            // The required-param load-gate the network path applies at resolution time:
            // a cached package with unset required params must not evaluate
            // misconfigured just because it was served from disk.
            let missing = crate::models::shared_packages::missing_required_params(
                &self.server_name,
                &specifier,
                &package.manifest.params,
            );
            if !missing.is_empty() {
                return Err(PackageError::Other(format!(
                    "{specifier} not loaded: required param(s) {} are unset; configure them in settings",
                    missing.join(", ")
                )));
            }
            // Track exactly as the network path would for the same version — the
            // served set (`loaded_packages()`), the reported version, and the
            // lockfile's staged version — EXCEPT the integrity stamp: it records the
            // hash most recently *verified*, and a cache-first serve verifies
            // nothing, so the entry's existing stamp is left untouched (`None` until
            // a network-verified load). A stub fetch records none of it.
            if track {
                self.cache
                    .borrow_mut()
                    .insert((key.clone(), version.clone()), package.clone());
                if referrer.is_none() {
                    self.resolved_versions
                        .borrow_mut()
                        .insert(key.clone(), version.clone());
                    self.record_resolution(&specifier, version, None);
                }
            }
            return Ok(package);
        }

        // The network resolve targets the DETERMINED version where one exists (the
        // cache-first serve above had a gap to fill) — only genuine discovery (a
        // never-resolved Auto root) asks what latest means.
        let wire = match self.fetch_wire(key, determined.as_deref()).await {
            Ok(wire) => wire,
            Err(err) => {
                // Offline: serve from the in-memory session cache, then the persistent
                // disk cache (works for pinned + auto, the latter via the staged
                // version — the paths a cache-first gap lands here with).
                if let Some(version) = determined {
                    if let Some(package) = self
                        .cache
                        .borrow()
                        .get(&(key.clone(), version.clone()))
                        .cloned()
                    {
                        return Ok(package);
                    }
                    if let Some(package) = self.build_from_cache(key, &version) {
                        // The disk cache was written by a resolve that passed the version-floor
                        // gate — but under a possibly NEWER smudgy since downgraded, so re-check
                        // the cached manifest's floor before serving it.
                        if let Some(reason) = manifest_floor_refusal(&key.name, &package.manifest) {
                            return Err(PackageError::Other(format!(
                                "{specifier} not loaded: {reason}"
                            )));
                        }
                        if track {
                            self.cache
                                .borrow_mut()
                                .insert((key.clone(), version.clone()), package.clone());
                            // Only a top-level (referrer-less) edge owns the reported version.
                            if referrer.is_none() {
                                self.resolved_versions
                                    .borrow_mut()
                                    .insert(key.clone(), version);
                            }
                        }
                        return Ok(package);
                    }
                }
                return Err(PackageError::Network(format!(
                    "resolving {specifier}: {err}"
                )));
            }
        };

        let version = wire.version.clone();
        // Record this instance's locked deps so imports IT makes resolve referrer-aware.
        self.store_locked_deps(key, &version, &wire.dependencies);
        if let Some(package) = self
            .cache
            .borrow()
            .get(&(key.clone(), version.clone()))
            .cloned()
        {
            if track && referrer.is_none() {
                self.resolved_versions
                    .borrow_mut()
                    .insert(key.clone(), version);
            }
            return Ok(package);
        }

        let manifest = PackageManifest::parse(&wire.manifest.to_string())
            .map_err(|err| PackageError::InvalidManifest(format!("{specifier}: {err}")))?;

        // Version-floor load-gate, at RESOLUTION time like the required-params gate below, so a
        // too-new package pulled in transitively (its dep edges carry locked versions the
        // pre-pass gates don't walk) is refused with a clear reason instead of evaluating
        // against script APIs this smudgy doesn't have.
        if let Some(reason) = manifest_floor_refusal(&key.name, &manifest) {
            return Err(PackageError::Other(format!(
                "{specifier} not loaded: {reason}"
            )));
        }

        // Required-param load-gate, at RESOLUTION time so it also catches a package
        // pulled in transitively (the top-level gate only prunes the install entry; a
        // blocked package that's also a dependency would otherwise evaluate misconfigured).
        // A package with unmet required params must not evaluate; failing here surfaces a
        // clear load error (and fails any dependent that needs it).
        let missing = crate::models::shared_packages::missing_required_params(
            &self.server_name,
            &specifier,
            &manifest.params,
        );
        if !missing.is_empty() {
            return Err(PackageError::Other(format!(
                "{specifier} not loaded: required param(s) {} are unset; configure them in settings",
                missing.join(", ")
            )));
        }

        let mut modules = Vec::with_capacity(wire.modules.len());
        for module in &wire.modules {
            // Assets (images and other binaries) never enter the module graph: they are
            // fetched lazily, by hash, when something actually displays them (the image
            // side-channel). Eagerly fetching them here both downloaded every published
            // image at load time and garbled the whole package load on the first
            // non-UTF-8 body. They stay in the CachedResolution written below.
            if !is_code_module(&module.media_type, &module.subpath) {
                continue;
            }
            // Content-addressed: a cached body for this hash never changes, so reuse it
            // and only download misses (then cache them).
            let cached = self
                .disk_cache
                .as_ref()
                .and_then(|cache| cache.read_blob(&module.content_hash));
            let text = if let Some(text) = cached {
                text
            } else {
                let text = self
                    .client
                    .fetch_module_body(&module.content_url, &module.content_hash)
                    .await
                    .map_err(|err| fetch_error(&specifier, module, &err))?;
                if let Some(cache) = &self.disk_cache {
                    let _ = cache.write_blob(&module.content_hash, &text);
                }
                text
            };
            modules.push(PackageModuleSource {
                subpath: module.subpath.clone(),
                text,
            });
        }

        let integrity = package_integrity(&wire.modules);
        let resolved = Rc::new(ResolvedPackage {
            key: key.clone(),
            resolved_version: version.clone(),
            manifest,
            integrity: integrity.clone(),
            modules,
        });

        if track {
            self.cache
                .borrow_mut()
                .insert((key.clone(), version.clone()), resolved.clone());
            // The referrer affects version *reads* (selection), not lockfile/report *writes*:
            // only a top-level (referrer-less) edge records the reported version and persists
            // the install's lockfile baseline. A transitive edge leaves both untouched, so it
            // can't clobber the top-level install's entry, integrity, or auto-update notice.
            if referrer.is_none() {
                self.resolved_versions
                    .borrow_mut()
                    .insert(key.clone(), version.clone());
                self.record_resolution(&specifier, &version, Some(&integrity));
            }
        }

        Ok(resolved)
    }
}

#[async_trait::async_trait(?Send)]
impl PackageProvider for SmudgyPackageProvider {
    async fn resolve_package(
        &self,
        key: &PackageKey,
        referrer: Option<&ReferrerRef>,
    ) -> Result<Rc<ResolvedPackage>, PackageError> {
        let resolved = self.resolve_impl(key, referrer, true).await?;
        // A code load makes this isolate legitimately host the package: record the
        // identity for <Image> creator registration (which evaluates strictly after the
        // graph resolves). Stub fetches (`resolve_package_for_stub`) never do — consuming
        // a producer is not hosting it. `integrity == "local"` is the local-dev-override
        // marker `try_local_override` writes; local assets read from disk, not blobs.
        if let Some(hosted) = self.image_hosted.borrow().as_ref() {
            hosted.insert(
                &resolved.key.owner,
                &resolved.key.name,
                &resolved.resolved_version,
                resolved.integrity == "local",
            );
        }
        Ok(resolved)
    }

    /// A stub fetch is a read of the producer's declarations, not a code load: nothing lands
    /// in the served set (`loaded_packages()` / the stumble diagnostic stays quiet) and
    /// nothing is recorded as an install — consuming an uninstalled producer leaves it
    /// uninstalled.
    async fn resolve_package_for_stub(
        &self,
        key: &PackageKey,
    ) -> Result<Rc<ResolvedPackage>, PackageError> {
        self.resolve_impl(key, None, false).await
    }

    fn get_cached(&self, key: &PackageKey, version: &str) -> Option<Rc<ResolvedPackage>> {
        self.cache
            .borrow()
            .get(&(key.clone(), version.to_string()))
            .cloned()
    }

    fn get_resolved(&self, key: &PackageKey) -> Option<Rc<ResolvedPackage>> {
        let version = self.resolved_versions.borrow().get(key).cloned()?;
        self.cache.borrow().get(&(key.clone(), version)).cloned()
    }

    /// Copy the package archives fetched for this isolate into an owned, `Send` source map.
    /// `cache` contains code-load resolutions only (not solve-prepass or interop-stub fetches),
    /// and its versioned keys preserve intra-isolate coexistence.
    fn snapshot_module_sources(&self) -> HashMap<String, String> {
        let mut sources = HashMap::new();
        for ((key, version), package) in self.cache.borrow().iter() {
            for module in &package.modules {
                sources.insert(
                    canonical_url(key, version, &module.subpath).to_string(),
                    module.text.clone(),
                );
            }
        }
        sources
    }

    /// The closure permission union folded by the last `solve_closure` over this isolate's
    /// closure (`PACKAGE-ISOLATES-ENFORCEMENT.md`). Empty (deny-all) until that pre-pass
    /// runs — the engine always runs it on the fork before reading this.
    fn closure_permissions(&self) -> PackagePermissions {
        self.closure_permission_union.borrow().clone()
    }

    /// Every package fetched-for-import through this provider so far. `cache` is only populated
    /// on the resolve paths (an import asked for the package), never by the `solve_closure`
    /// manifest walk, so its keys are this isolate's actually-served package set — what the
    /// engine's code-import stumble diagnostic inspects after module loading.
    fn loaded_packages(&self) -> Vec<PackageKey> {
        let mut keys: Vec<PackageKey> = self
            .cache
            .borrow()
            .keys()
            .map(|(key, _)| key.clone())
            .collect();
        keys.sort_by(|a, b| (&a.owner, &a.name).cmp(&(&b.owner, &b.name)));
        keys.dedup();
        keys
    }

    fn set_home_packages(&self, homes: Vec<PackageKey>) {
        *self.home_packages.borrow_mut() = Some(
            homes
                .iter()
                .map(smudgy_script::PackageKey::folded)
                .collect(),
        );
    }

    fn is_home_load(&self, key: &PackageKey) -> bool {
        self.home_packages
            .borrow()
            .as_ref()
            .is_none_or(|set| set.contains(&key.folded()))
    }

    fn note_scrubbed(&self, key: &PackageKey) {
        let mut scrubbed = self.scrubbed.borrow_mut();
        if !scrubbed.contains(key) {
            scrubbed.push(key.clone());
        }
    }

    fn scrubbed_packages(&self) -> Vec<PackageKey> {
        self.scrubbed.borrow().clone()
    }

    fn note_user_code_import(&self, key: &PackageKey) {
        let mut imports = self.user_imports.borrow_mut();
        if !imports.contains(key) {
            imports.push(key.clone());
        }
    }

    fn user_code_imports(&self) -> Vec<PackageKey> {
        self.user_imports.borrow().clone()
    }
}

/// Maps a module-body fetch error onto a [`PackageError`], distinguishing an integrity
/// failure (never serve unverified bytes) from a transport error.
fn fetch_error(specifier: &str, module: &ResolvedModuleWire, err: &CloudError) -> PackageError {
    let message = err.to_string();
    if message.contains("integrity mismatch") {
        PackageError::IntegrityMismatch {
            specifier: format!("{specifier}/{}", module.subpath),
            expected: module.content_hash.clone(),
            actual: message,
        }
    } else {
        PackageError::Network(format!(
            "fetching {} for {specifier}: {message}",
            module.subpath
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smudgy_cloud::{Credential, CredentialSource};

    fn pkg_key(name: &str) -> PackageKey {
        PackageKey {
            owner: "wbk".into(),
            name: name.into(),
        }
    }

    fn referrer(name: &str, version: &str) -> ReferrerRef {
        ReferrerRef {
            key: pkg_key(name),
            version: version.into(),
        }
    }

    fn dep(name: &str, range: &str, version: &str) -> ResolvedDependency {
        ResolvedDependency {
            owner_nickname: "wbk".into(),
            name: name.into(),
            range: range.into(),
            resolved_version: version.into(),
        }
    }

    /// A provider with a non-functional client (no calls are made in these tests). The
    /// constructor reads a (missing) lockfile and opens the on-disk cache; both no-op
    /// gracefully, and the referrer mapping under test is independent of either.
    fn test_provider() -> SmudgyPackageProvider {
        let client = PackageApiClient::new(
            "http://127.0.0.1:0",
            CredentialSource::new(Some(Credential::ApiKey("test".into()))),
        );
        SmudgyPackageProvider::new(client, Arc::new("ReferrerProviderTest".to_string()))
    }

    #[test]
    fn dep_package_key_builds_from_nickname_and_name() {
        assert_eq!(dep_package_key("wbk", "util"), Some(pkg_key("util")));
        // An empty owner nickname is rejected.
        assert!(dep_package_key("", "util").is_none());
    }

    #[test]
    fn referrer_locked_version_scopes_selection_to_each_importer() {
        let provider = test_provider();
        let util = pkg_key("util");

        // app@1.0.0 locked util@1.3.0 (a range); other@1.0.0 pinned util@=2.0.0.
        provider.store_locked_deps(&pkg_key("app"), "1.0.0", &[dep("util", "^1.3", "1.3.0")]);
        provider.store_locked_deps(
            &pkg_key("other"),
            "1.0.0",
            &[dep("util", "=2.0.0", "2.0.0")],
        );

        // The heart of referrer-aware resolution: each importer selects the version IT
        // locked, with the declared range classified into the exact-pin flag.
        assert_eq!(
            provider.referrer_locked_version(&referrer("app", "1.0.0"), &util),
            Some(("1.3.0".to_string(), false))
        );
        assert_eq!(
            provider.referrer_locked_version(&referrer("other", "1.0.0"), &util),
            Some(("2.0.0".to_string(), true)),
            "an author =x dep is captured as an exact pin"
        );
        // An importer with no lock for the target falls through (None -> lockfile/latest).
        assert_eq!(
            provider.referrer_locked_version(&referrer("app", "1.0.0"), &pkg_key("absent")),
            None
        );
        assert_eq!(
            provider.referrer_locked_version(&referrer("unknown", "1.0.0"), &util),
            None
        );
    }

    #[tokio::test]
    async fn cap_version_offline_falls_back_to_last_resolved() {
        // `test_provider`'s client points at a dead address, so every resolve fails —
        // standing in for "signed out and the package isn't public" (or simply offline).
        let provider = test_provider();
        let locked: LockedPackage = serde_json::from_str(
            r#"{"specifier":"smudgy://wbk/util","mode":{"mode":"auto"},"last_resolved_version":"1.2.0","enabled":true}"#,
        )
        .expect("locked package");
        provider.lock.borrow_mut().packages.push(locked);

        // An installed auto-update package whose latest can't be resolved falls back to its
        // cached, already-consented last-resolved version instead of refusing to load. (Its
        // closure union and version floor resolve to empty/none while offline, which fit the
        // prior consent and any smudgy.)
        let capped = provider
            .cap_version("smudgy://wbk/util", &PackagePermissions::default())
            .await;
        assert_eq!(capped.as_deref(), Ok("1.2.0"));
    }

    #[tokio::test]
    async fn cap_version_reports_no_versions_for_an_unknown_uncached_package() {
        // The dead-address client stands in for "the cloud has no such package"; with no
        // `last_resolved_version` cached either, there is nothing to consider — the denial
        // must say so instead of claiming a permission problem. This is the stale lockfile
        // entry left when a `smudgy://local/…` install's folder is deleted.
        let provider = test_provider();
        let locked: LockedPackage = serde_json::from_str(
            r#"{"specifier":"smudgy://local/duo","mode":{"mode":"auto"},"enabled":true}"#,
        )
        .expect("locked package");
        provider.lock.borrow_mut().packages.push(locked);

        let capped = provider
            .cap_version("smudgy://local/duo", &PackagePermissions::default())
            .await;
        assert_eq!(capped, Err(CapRefusal::NoVersions));
    }

    fn util_req(version: &str, is_pin: bool) -> DepRequirement {
        DepRequirement {
            package: pkg_key("util"),
            version: version.into(),
            is_pin,
        }
    }

    #[test]
    fn fork_shares_lock_but_not_solve_state() {
        // A fork shares the expensive isolate-independent bits — crucially the lockfile view, so
        // partitioned lockfile writes across isolates stay consistent — but starts
        // with its own empty solve state: the whole point of per-isolate resolution.
        let base = test_provider();
        let fork = base.fork();
        assert!(
            Rc::ptr_eq(&base.lock, &fork.lock),
            "the per-server lockfile view is shared across isolates"
        );
        *base.solve.borrow_mut() = Some(package_solver::solve(&[util_req("1.0.0", false)]));
        assert!(base.solve.borrow().is_some());
        assert!(
            fork.solve.borrow().is_none(),
            "a fork's solve state is its own, independent of the base"
        );
    }

    #[test]
    fn forks_record_resolutions_into_one_shared_lock_without_clobber() {
        // Lockfile partition: main and a sandboxed isolate each top-level-install a
        // DIFFERENT package. Because their providers share one lock (`Rc<RefCell<…>>`), each fork's
        // `record_resolution` writes its own entry into the SAME in-memory lockfile, so neither
        // clobbers the other. The shared lock is what makes this safe: with
        // per-fork copies, each `record_resolution` would persist a stale snapshot missing the
        // other's entry (`PACKAGE-ISOLATES-RESOLUTION.md`). Disk persistence itself
        // (`save_lock` / `load_lock`) is covered in `shared_packages`.
        let main = test_provider();
        let sandbox = main.fork();

        main.record_resolution("smudgy://wbk/mapper", "1.4.0", Some("main-integrity"));
        sandbox.record_resolution("smudgy://cor/combat", "2.0.0", Some("sandbox-integrity"));

        // One shared lock holds BOTH installs at their own versions — partitioned, not clobbered.
        for provider in [&main, &sandbox] {
            let lock = provider.lock.borrow();
            assert_eq!(
                lock.find("smudgy://wbk/mapper")
                    .and_then(|p| p.last_resolved_version.as_deref()),
                Some("1.4.0"),
                "main's install survives the sandbox's later write into the shared lock"
            );
            assert_eq!(
                lock.find("smudgy://cor/combat")
                    .and_then(|p| p.last_resolved_version.as_deref()),
                Some("2.0.0"),
                "the sandbox's install is recorded into the same shared lock"
            );
        }
    }

    #[test]
    fn forked_isolates_resolve_a_shared_dep_independently() {
        // Each isolate has its OWN provider (a fork sharing only
        // client/cache/lock), so the SAME dependency can resolve to a different version per isolate
        // — no cross-isolate collapse (`PACKAGE-ISOLATES-RESOLUTION.md`). `solve_closure`'s
        // network walk is covered by the integration suite; here we feed its pure solver directly
        // (as `solve_closure` does after the walk) to assert the per-isolate state is independent.
        let base = test_provider();
        let main = base.fork();
        let sandbox = base.fork();
        let util = pkg_key("util");

        // main's closure locked util@1.4.0; the sandboxed isolate's closure locked util@1.2.0.
        *main.solve.borrow_mut() = Some(package_solver::solve(&[util_req("1.4.0", false)]));
        *sandbox.solve.borrow_mut() = Some(package_solver::solve(&[util_req("1.2.0", false)]));
        main.top_level_solved
            .borrow_mut()
            .insert(util.clone(), "1.4.0".into());
        sandbox
            .top_level_solved
            .borrow_mut()
            .insert(util.clone(), "1.2.0".into());

        // Each isolate resolves the dep at its own collapsed version.
        assert_eq!(main.solve_resolve(&util, "1.4.0", false), "1.4.0");
        assert_eq!(sandbox.solve_resolve(&util, "1.2.0", false), "1.2.0");
        // Independence is load-bearing: were the solve shared, the sandbox's 1.2.0 would collapse
        // UP to 1.4.0 (same compat class). It does not — the sandbox keeps 1.2.0...
        assert_eq!(sandbox.solve_resolve(&util, "1.2.0", false), "1.2.0");
        // ...while main, asked about 1.2.0, collapses to ITS OWN 1.4.0 — two distinct solve heaps.
        assert_eq!(main.solve_resolve(&util, "1.2.0", false), "1.4.0");
        // Top-level reads are isolate-local too.
        assert_eq!(
            main.top_level_solved
                .borrow()
                .get(&util)
                .map(String::as_str),
            Some("1.4.0")
        );
        assert_eq!(
            sandbox
                .top_level_solved
                .borrow()
                .get(&util)
                .map(String::as_str),
            Some("1.2.0")
        );
    }

    /// Point the process-global smudgy home at a temp directory (first caller wins;
    /// another lib test may have won already — either way the home is disposable).
    /// Tests that assert on-disk lockfile state call this before touching it.
    fn use_temp_smudgy_home() {
        static TEST_HOME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        TEST_HOME.get_or_init(|| {
            let dir = std::env::temp_dir()
                .join(format!("smudgy-provider-test-home-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create temp home");
            crate::set_smudgy_home(dir.clone());
            dir
        });
    }

    /// A provider over `server` whose in-memory lockfile view is exactly `packages` —
    /// the snapshot a session holds, independent of what is on disk by now.
    fn provider_with_view(server: &str, packages: Vec<LockedPackage>) -> SmudgyPackageProvider {
        let client = PackageApiClient::new(
            "http://127.0.0.1:0",
            CredentialSource::new(Some(Credential::ApiKey("test".into()))),
        );
        let provider = SmudgyPackageProvider::new(client, Arc::new(server.to_string()));
        provider.lock.borrow_mut().packages = packages;
        provider
    }

    #[test]
    fn record_resolution_preserves_a_pending_background_stage() {
        // The checker staged 1.3.0 mid-reload (staging clears `integrity`); the
        // reload's record — decided from its engine-build-start snapshot of 1.2.0 —
        // must NOT revert the pending stage, on disk or in the session's view.
        use_temp_smudgy_home();
        let server = "RecordResolutionStageGuardTest";
        let specifier = "smudgy://wbk/mapper";
        shared_packages::mutate_lock(server, |lock| {
            let mut entry = LockedPackage::new(specifier, UpdateMode::Auto);
            entry.last_resolved_version = Some("1.3.0".into());
            entry.integrity = None;
            lock.upsert(entry);
            Ok(((), true))
        })
        .expect("seed the staged entry");
        let mut loaded = LockedPackage::new(specifier, UpdateMode::Auto);
        loaded.last_resolved_version = Some("1.2.0".into());
        loaded.integrity = Some("stamped-at-load".into());
        let provider = provider_with_view(server, vec![loaded]);

        provider.record_resolution(specifier, "1.2.0", Some("net-integrity"));

        let disk = shared_packages::load_lock(server).expect("lock loads");
        let entry = disk.find(specifier).expect("the entry survives");
        assert_eq!(
            entry.last_resolved_version.as_deref(),
            Some("1.3.0"),
            "the pending stage is preserved, not reverted to the load's version"
        );
        assert_eq!(
            entry.integrity, None,
            "the staged version still awaits its first serving load"
        );
        assert_eq!(
            provider
                .lock
                .borrow()
                .find(specifier)
                .and_then(|e| e.last_resolved_version.clone())
                .as_deref(),
            Some("1.2.0"),
            "the in-memory view keeps describing what this load served — no clobber"
        );
        assert!(
            provider.take_version_changes().is_empty(),
            "a refused record raises no auto-update notice"
        );
    }

    #[test]
    fn record_resolution_still_records_stamped_and_initial_entries() {
        use_temp_smudgy_home();
        let server = "RecordResolutionNormalTest";
        let specifier = "smudgy://wbk/mapper";
        // A stamped entry records as always — a legitimate downgrade included: the
        // guard keys on the unstamped fresh-stage signature, nothing else.
        shared_packages::mutate_lock(server, |lock| {
            let mut entry = LockedPackage::new(specifier, UpdateMode::Auto);
            entry.last_resolved_version = Some("1.3.0".into());
            entry.integrity = Some("verified-1.3.0".into());
            lock.upsert(entry);
            Ok(((), true))
        })
        .expect("seed the stamped entry");
        let mut loaded = LockedPackage::new(specifier, UpdateMode::Auto);
        loaded.last_resolved_version = Some("1.3.0".into());
        loaded.integrity = Some("verified-1.3.0".into());
        let provider = provider_with_view(server, vec![loaded]);

        provider.record_resolution(specifier, "1.2.0", Some("verified-1.2.0"));

        let disk = shared_packages::load_lock(server).expect("lock loads");
        let entry = disk.find(specifier).expect("the entry survives");
        assert_eq!(entry.last_resolved_version.as_deref(), Some("1.2.0"));
        assert_eq!(entry.integrity.as_deref(), Some("verified-1.2.0"));

        // An initial install — version and integrity both None — records too.
        let fresh = "smudgy://wbk/fresh";
        shared_packages::mutate_lock(server, |lock| {
            lock.upsert(LockedPackage::new(fresh, UpdateMode::Auto));
            Ok(((), true))
        })
        .expect("seed the fresh install");
        provider.record_resolution(fresh, "0.1.0", Some("verified-0.1.0"));
        let disk = shared_packages::load_lock(server).expect("lock loads");
        let entry = disk.find(fresh).expect("the entry survives");
        assert_eq!(entry.last_resolved_version.as_deref(), Some("0.1.0"));
        assert_eq!(entry.integrity.as_deref(), Some("verified-0.1.0"));
    }

    #[test]
    fn duplicate_warning_is_intra_isolate_only() {
        // The duplicate-version warning is intra-isolate under per-isolate providers
        // (`PACKAGE-ISOLATES-RESOLUTION.md`): each provider computes `loaded_duplicates` over its
        // OWN closure, so a cross-isolate duplicate never appears in one provider's closure.

        // Seed a provider's solve + duplicate-warning set exactly as `solve_closure` does after its
        // network walk: each importer is a top-level root at 1.0.0 depending on util at `version`.
        fn seed(provider: &SmudgyPackageProvider, importers: &[(&str, &str)]) {
            let util = pkg_key("util");
            let mut requirements = Vec::new();
            let mut roots = Vec::new();
            let mut edges = Vec::new();
            for (name, version) in importers {
                let root = DepRequirement {
                    package: pkg_key(name),
                    version: "1.0.0".into(),
                    is_pin: false,
                };
                requirements.push(root.clone());
                roots.push(root);
                requirements.push(DepRequirement {
                    package: util.clone(),
                    version: (*version).into(),
                    is_pin: false,
                });
                edges.push(DepEdge {
                    importer: pkg_key(name),
                    importer_version: "1.0.0".into(),
                    dep: util.clone(),
                    dep_version: (*version).into(),
                    dep_is_pin: false,
                });
            }
            let solve = package_solver::solve(&requirements);
            *provider.duplicate_warnings.borrow_mut() = solve.loaded_duplicates(&roots, &edges);
            *provider.solve.borrow_mut() = Some(solve);
        }

        let base = test_provider();

        // Cross-isolate: main's closure has only util@1.4.0, the sandboxed isolate's only util@1.2.0.
        // Each is a single version within its own closure → neither warns.
        let main = base.fork();
        let sandbox = base.fork();
        seed(&main, &[("app", "1.4.0")]);
        seed(&sandbox, &[("combat", "1.2.0")]);
        assert!(
            main.take_duplicate_warnings().is_empty(),
            "one util version in main's closure → no warning"
        );
        assert!(
            sandbox.take_duplicate_warnings().is_empty(),
            "the sandbox runs a different util version, but that cross-isolate duplicate is benign → no warning"
        );

        // Intra-isolate: a SINGLE isolate whose closure pulls two incompatible majors still warns.
        let mixed = base.fork();
        seed(&mixed, &[("a", "1.4.0"), ("b", "2.0.1")]);
        let warnings = mixed.take_duplicate_warnings();
        assert_eq!(
            warnings.len(),
            1,
            "two coexisting util majors in ONE isolate is a real collision → a warning"
        );
        assert_eq!(warnings[0].0, pkg_key("util"));
        assert_eq!(warnings[0].1, vec!["1.4.0", "2.0.1"]);
    }

    #[test]
    fn locked_deps_are_keyed_per_importer_version() {
        // Two coexisting versions of the SAME importer lock different dep versions; the
        // map must keep them distinct (else their transitive imports would collapse).
        let provider = test_provider();
        let util = pkg_key("util");
        provider.store_locked_deps(&pkg_key("app"), "1.0.0", &[dep("util", "^2", "2.0.0")]);
        provider.store_locked_deps(&pkg_key("app"), "2.0.0", &[dep("util", "^2.5", "2.5.0")]);

        assert_eq!(
            provider.referrer_locked_version(&referrer("app", "1.0.0"), &util),
            Some(("2.0.0".to_string(), false))
        );
        assert_eq!(
            provider.referrer_locked_version(&referrer("app", "2.0.0"), &util),
            Some(("2.5.0".to_string(), false))
        );
    }

    // ------------------------------------------------------------------
    // Counting mock registry — pins exactly how much network a resolve
    // path generates (cache-first serve + the dedup/memo waste fixes).
    // ------------------------------------------------------------------

    use std::io::{Read as _, Write as _};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One published package the mock registry serves: a single `index.ts` whose body is
    /// `body`, a manifest assembled from `manifest_extra` (verbatim JSON appended inside
    /// the manifest object), and locked deps as `(owner, name, range, resolved_version)`.
    /// The last entry declared for an `(owner, name)` is also its `latest`.
    struct MockPackage {
        owner: &'static str,
        name: &'static str,
        version: &'static str,
        manifest_extra: &'static str,
        deps: &'static [(&'static str, &'static str, &'static str, &'static str)],
        body: &'static str,
    }

    /// Handle on a [`spawn_registry`] server: `resolve_hits` counts `/packages/resolve`
    /// calls, `body_hits` counts module-body fetches — the two network costs the
    /// provider's cache/memo/dedup layers exist to eliminate.
    struct MockRegistry {
        base_url: String,
        resolve_hits: Arc<AtomicUsize>,
        body_hits: Arc<AtomicUsize>,
    }

    fn sha256_hex(body: &str) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(body.as_bytes()))
    }

    /// Serve `packages` over real HTTP on an OS-assigned port, counting hits per route
    /// class. Sequential accept loop with `Connection: close` per response — matching
    /// the sequential resolve traffic one provider generates (the same shape as
    /// `package_isolates_enforcement.rs`'s servers).
    fn spawn_registry(packages: &[MockPackage]) -> MockRegistry {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let port = listener.local_addr().expect("local_addr").port();
        let base_url = format!("http://127.0.0.1:{port}");

        let mut resolves: HashMap<(String, String, String), String> = HashMap::new();
        let mut bodies: HashMap<String, String> = HashMap::new();
        for package in packages {
            let hash = sha256_hex(package.body);
            bodies.insert(hash.clone(), package.body.to_string());
            let deps = package
                .deps
                .iter()
                .map(|(owner, name, range, version)| {
                    format!(
                        r#"{{"owner_nickname":"{owner}","name":"{name}","range":"{range}","resolved_version":"{version}"}}"#
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            let (owner, name, version) = (package.owner, package.name, package.version);
            let extra = package.manifest_extra;
            let wire = format!(
                r#"{{"data":{{"package_id":"00000000-0000-0000-0000-000000000001","owner_nickname":"{owner}","name":"{name}","version":"{version}","manifest":{{"name":"{name}","version":"{version}"{extra}}},"modules":[{{"subpath":"index.ts","content_hash":"{hash}","media_type":"application/typescript","content_url":"{base_url}/blob/{hash}"}}],"dependencies":[{deps}]}}}}"#
            );
            resolves.insert((owner.into(), name.into(), version.into()), wire.clone());
            resolves.insert((owner.into(), name.into(), "latest".into()), wire);
        }

        let resolve_hits = Arc::new(AtomicUsize::new(0));
        let body_hits = Arc::new(AtomicUsize::new(0));
        let resolve_count = Arc::clone(&resolve_hits);
        let body_count = Arc::clone(&body_hits);
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
                let mut request = Vec::new();
                let mut buf = [0u8; 512];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            request.extend_from_slice(&buf[..n]);
                            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let target = request.split_whitespace().nth(1).unwrap_or_default();
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                let payload = if path == "/packages/resolve" {
                    resolve_count.fetch_add(1, Ordering::SeqCst);
                    let (mut owner, mut name, mut version) = ("", "", "latest");
                    for pair in query.split('&') {
                        match pair.split_once('=') {
                            Some(("owner", value)) => owner = value,
                            Some(("name", value)) => name = value,
                            Some(("version", value)) => version = value,
                            _ => {}
                        }
                    }
                    resolves
                        .get(&(owner.to_string(), name.to_string(), version.to_string()))
                        .cloned()
                } else if let Some(hash) = path.strip_prefix("/blob/") {
                    body_count.fetch_add(1, Ordering::SeqCst);
                    bodies.get(hash).cloned()
                } else {
                    None
                };
                let (status, body) = match payload {
                    Some(body) => ("200 OK", body),
                    None => ("404 Not Found", r#"{"error":"not found"}"#.to_string()),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        MockRegistry {
            base_url,
            resolve_hits,
            body_hits,
        }
    }

    /// A provider backed by a [`spawn_registry`] mock, scrubbed of ambient process
    /// state: the smudgy home is a process-global another lib test may (or may not)
    /// have pointed at a reused directory, so the constructor's opened disk cache and
    /// loaded lockfile are dropped — hit counting must start from a blank slate. Tests
    /// that need a disk cache or lock entries inject their own.
    fn provider_for(registry: &MockRegistry) -> SmudgyPackageProvider {
        let client = PackageApiClient::new(
            registry.base_url.clone(),
            CredentialSource::new(Some(Credential::ApiKey("test".into()))),
        );
        let mut provider =
            SmudgyPackageProvider::new(client, Arc::new("CacheFirstProviderTest".to_string()));
        provider.disk_cache = None;
        provider.lock.borrow_mut().packages.clear();
        provider
    }

    /// A ready-to-serve disk cache entry for `owner/name@version`: the meta (with the
    /// given locked dep edges) plus its single `index.ts` blob, so `build_from_cache`
    /// succeeds. Returns the cache.
    fn warm_cache(
        dir: &std::path::Path,
        key: &PackageKey,
        version: &str,
        manifest_extra: &str,
        body: &str,
        deps: &[ResolvedDependency],
    ) -> PackageCache {
        let cache = PackageCache::with_root(dir.to_path_buf());
        let hash = sha256_hex(body);
        cache.write_blob(&hash, body).expect("write blob");
        let manifest_json = format!(
            r#"{{"name":"{name}","version":"{version}"{manifest_extra}}}"#,
            name = key.name
        );
        let meta = CachedResolution {
            version: version.to_string(),
            integrity: "cached-integrity".to_string(),
            manifest: PackageManifest::parse(&manifest_json).expect("valid manifest"),
            modules: vec![CachedModule {
                subpath: "index.ts".to_string(),
                content_hash: hash,
                media_type: "application/typescript".to_string(),
            }],
            dependencies: deps.to_vec(),
        };
        cache.write_meta(key, version, &meta).expect("write meta");
        cache
    }

    /// An installed Auto lock entry whose staged (`last_resolved_version`) is `version`.
    fn staged_lock_entry(specifier: &str, version: &str) -> LockedPackage {
        let mut entry = LockedPackage::new(specifier, UpdateMode::Auto);
        entry.last_resolved_version = Some(version.to_string());
        entry
    }

    #[tokio::test]
    async fn solve_closure_fetches_each_distinct_node_once_for_a_diamond() {
        // app -> left -> base and app -> right -> base: base is reached by two paths but
        // is one distinct (package, version) node, so the walk costs four fetches — the
        // dedup runs BEFORE the fetch (and the wire memo backstops any re-ask).
        let registry = spawn_registry(&[
            MockPackage {
                owner: "wbk",
                name: "base",
                version: "1.0.0",
                manifest_extra: "",
                deps: &[],
                body: "export const base = 1;",
            },
            MockPackage {
                owner: "wbk",
                name: "left",
                version: "1.0.0",
                manifest_extra: "",
                deps: &[("wbk", "base", "^1", "1.0.0")],
                body: "export const left = 1;",
            },
            MockPackage {
                owner: "wbk",
                name: "right",
                version: "1.0.0",
                manifest_extra: "",
                deps: &[("wbk", "base", "^1", "1.0.0")],
                body: "export const right = 1;",
            },
            MockPackage {
                owner: "wbk",
                name: "app",
                version: "1.0.0",
                manifest_extra: "",
                deps: &[
                    ("wbk", "left", "^1", "1.0.0"),
                    ("wbk", "right", "^1", "1.0.0"),
                ],
                body: "export const app = 1;",
            },
        ]);
        let provider = provider_for(&registry);

        provider
            .solve_closure(&["smudgy://wbk/app".to_string()])
            .await;

        assert!(provider.solve.borrow().is_some(), "the walk completed");
        assert_eq!(
            registry.resolve_hits.load(Ordering::SeqCst),
            4,
            "one fetch per distinct closure node — the diamond's shared base is not re-fetched per path"
        );
        assert_eq!(
            registry.body_hits.load(Ordering::SeqCst),
            0,
            "a metadata walk fetches no code bodies"
        );
    }

    #[tokio::test]
    async fn engine_walks_share_one_wire_fetch_per_version() {
        // The full sandboxed-load sequence — cap_version, the capped closure solve, and
        // the code-load resolve — asks about the same (package, version) from three
        // angles; the wire memo collapses them to cap_version's single latest probe.
        let registry = spawn_registry(&[MockPackage {
            owner: "wbk",
            name: "solo",
            version: "1.0.0",
            manifest_extra: "",
            deps: &[],
            body: "export const solo = 1;",
        }]);
        let provider = provider_for(&registry);

        let capped = provider
            .cap_version("smudgy://wbk/solo", &PackagePermissions::default())
            .await
            .expect("caps to the only version");
        assert_eq!(capped, "1.0.0");
        provider
            .solve_closure_capped(&[("smudgy://wbk/solo".to_string(), capped)])
            .await;
        let resolved = provider
            .resolve_package(&pkg_key("solo"), None)
            .await
            .expect("resolves");

        assert_eq!(resolved.resolved_version, "1.0.0");
        assert_eq!(
            registry.resolve_hits.load(Ordering::SeqCst),
            1,
            "cap_version's latest probe is the only wire fetch; the closure fold, the \
             capped solve, and the code load all reuse it"
        );
        assert_eq!(
            registry.body_hits.load(Ordering::SeqCst),
            1,
            "the single module body is fetched once (then blob-cached where a disk cache exists)"
        );
    }

    #[tokio::test]
    async fn metadata_walks_write_the_offline_meta_cache() {
        // A solve pre-pass alone — no code load — persists every walked version's
        // metadata, so the offline cache warms even for packages never imported this
        // session (blob presence still gates offline serving).
        let registry = spawn_registry(&[
            MockPackage {
                owner: "wbk",
                name: "base",
                version: "1.0.0",
                manifest_extra: "",
                deps: &[],
                body: "export const base = 1;",
            },
            MockPackage {
                owner: "wbk",
                name: "app",
                version: "1.0.0",
                manifest_extra: "",
                deps: &[("wbk", "base", "^1", "1.0.0")],
                body: "export const app = 1;",
            },
        ]);
        let mut provider = provider_for(&registry);
        let dir = tempfile::tempdir().expect("tempdir");
        provider.disk_cache = Some(PackageCache::with_root(dir.path().to_path_buf()));

        provider
            .solve_closure(&["smudgy://wbk/app".to_string()])
            .await;

        let cache = provider.disk_cache.as_ref().expect("cache");
        assert!(
            cache.read_meta(&pkg_key("app"), "1.0.0").is_some(),
            "the walked root's resolve persisted its metadata"
        );
        assert!(
            cache.read_meta(&pkg_key("base"), "1.0.0").is_some(),
            "transitive nodes persist too"
        );
        assert_eq!(registry.body_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_staged_version_serves_from_cache_with_zero_network() {
        // The cache-first contract: a tracked resolve whose version is staged in the
        // lockfile and fully cached (meta + code blobs) touches the network not at all —
        // the registry would serve a newer 9.9.9, and must never even be asked.
        let registry = spawn_registry(&[MockPackage {
            owner: "wbk",
            name: "app",
            version: "9.9.9",
            manifest_extra: "",
            deps: &[],
            body: "export const newer = 1;",
        }]);
        let mut provider = provider_for(&registry);
        let dir = tempfile::tempdir().expect("tempdir");
        provider.disk_cache = Some(warm_cache(
            dir.path(),
            &pkg_key("app"),
            "1.2.0",
            "",
            "export const x = 1;",
            &[],
        ));
        provider
            .lock
            .borrow_mut()
            .packages
            .push(staged_lock_entry("smudgy://wbk/app", "1.2.0"));

        let resolved = provider
            .resolve_package(&pkg_key("app"), None)
            .await
            .expect("serves from cache");

        assert_eq!(
            resolved.resolved_version, "1.2.0",
            "the staged version is served, not the registry's newer latest"
        );
        assert_eq!(registry.resolve_hits.load(Ordering::SeqCst), 0);
        assert_eq!(registry.body_hits.load(Ordering::SeqCst), 0);
        // Tracking parity with the network path: the served set, the reported version,
        // and the lockfile's staged version all recorded the load.
        assert_eq!(provider.loaded_packages(), vec![pkg_key("app")]);
        assert_eq!(
            provider
                .resolved_versions
                .borrow()
                .get(&pkg_key("app"))
                .map(String::as_str),
            Some("1.2.0")
        );
        assert_eq!(
            provider
                .lock
                .borrow()
                .find("smudgy://wbk/app")
                .and_then(|entry| entry.last_resolved_version.as_deref()),
            Some("1.2.0")
        );
        assert_eq!(
            provider
                .lock
                .borrow()
                .find("smudgy://wbk/app")
                .and_then(|entry| entry.integrity.as_deref()),
            None,
            "integrity records what a load VERIFIED; a cache-first serve verifies \
             nothing, so the stamp stays absent until a network-verified load"
        );
    }

    #[tokio::test]
    async fn a_cache_first_serve_leaves_a_prior_integrity_stamp_untouched() {
        // The entry's `integrity` records the hash most recently VERIFIED. A
        // cache-first serve of the same staged version verifies nothing, so a stamp
        // left by an earlier network-verified load must survive it unchanged.
        let registry = spawn_registry(&[]);
        let mut provider = provider_for(&registry);
        let dir = tempfile::tempdir().expect("tempdir");
        provider.disk_cache = Some(warm_cache(
            dir.path(),
            &pkg_key("app"),
            "1.2.0",
            "",
            "export const x = 1;",
            &[],
        ));
        let mut entry = staged_lock_entry("smudgy://wbk/app", "1.2.0");
        entry.integrity = Some("verified-earlier".into());
        provider.lock.borrow_mut().packages.push(entry);

        provider
            .resolve_package(&pkg_key("app"), None)
            .await
            .expect("serves from cache");

        assert_eq!(
            provider
                .lock
                .borrow()
                .find("smudgy://wbk/app")
                .and_then(|e| e.integrity.as_deref().map(str::to_string)),
            Some("verified-earlier".into()),
            "the last VERIFIED stamp survives an unverified cache serve"
        );
    }

    #[tokio::test]
    async fn cache_first_serve_still_enforces_the_version_floor() {
        // The floor re-check that guards the offline fallback guards the cache-first
        // serve too: a cached version whose manifest floor exceeds this smudgy is
        // refused, not served (and the network is still never consulted — the floor
        // gate refuses before discovery could matter).
        let registry = spawn_registry(&[]);
        let mut provider = provider_for(&registry);
        let dir = tempfile::tempdir().expect("tempdir");
        provider.disk_cache = Some(warm_cache(
            dir.path(),
            &pkg_key("app"),
            "1.2.0",
            r#","min_smudgy_version":"999.0.0""#,
            "export const x = 1;",
            &[],
        ));
        provider
            .lock
            .borrow_mut()
            .packages
            .push(staged_lock_entry("smudgy://wbk/app", "1.2.0"));

        let err = provider
            .resolve_package(&pkg_key("app"), None)
            .await
            .expect_err("the floor refuses the cached serve");

        assert!(
            err.to_string().contains("requires smudgy 999.0.0"),
            "the refusal names the floor: {err}"
        );
        assert_eq!(registry.resolve_hits.load(Ordering::SeqCst), 0);
        assert!(
            provider.loaded_packages().is_empty(),
            "a refused serve leaves no code-load footprint"
        );
    }

    #[tokio::test]
    async fn a_stub_fetch_serves_the_staged_version_from_cache_with_no_footprint() {
        // A stub fetch (track == false) of an installed producer resolves the SAME
        // determined version the producer's isolate runs — its staged version, from
        // the cache, with zero network (the registry's newer 9.9.9 must never be
        // asked about, and a handle set from latest would be a phantom) — while
        // keeping the no-footprint contract exactly: no served set, no reported
        // version, no lockfile touch.
        let registry = spawn_registry(&[MockPackage {
            owner: "wbk",
            name: "app",
            version: "9.9.9",
            manifest_extra: "",
            deps: &[],
            body: "export const newer = 1;",
        }]);
        let mut provider = provider_for(&registry);
        let dir = tempfile::tempdir().expect("tempdir");
        provider.disk_cache = Some(warm_cache(
            dir.path(),
            &pkg_key("app"),
            "1.2.0",
            "",
            "export const x = 1;",
            &[],
        ));
        provider
            .lock
            .borrow_mut()
            .packages
            .push(staged_lock_entry("smudgy://wbk/app", "1.2.0"));

        let resolved = provider
            .resolve_package_for_stub(&pkg_key("app"))
            .await
            .expect("the cache serves the stub");

        assert_eq!(
            resolved.resolved_version, "1.2.0",
            "the stub resolves the staged version the producer's isolate runs"
        );
        assert_eq!(registry.resolve_hits.load(Ordering::SeqCst), 0);
        assert_eq!(registry.body_hits.load(Ordering::SeqCst), 0);
        assert!(
            provider.loaded_packages().is_empty(),
            "a stub fetch lands nothing in the served set"
        );
        assert!(
            provider.resolved_versions.borrow().is_empty(),
            "a stub fetch reports no resolved version"
        );
        assert_eq!(
            provider
                .lock
                .borrow()
                .find("smudgy://wbk/app")
                .and_then(|entry| entry.integrity.as_deref()),
            None,
            "a stub fetch stamps no resolution metadata"
        );
    }

    #[tokio::test]
    async fn solve_closure_serves_staged_roots_from_cached_meta_with_zero_network() {
        // The trusted/main pre-pass contract: an Auto root with a staged version solves
        // AT that version, and the whole walk — root and dep edges alike — reads cached
        // metas instead of the wire. The registry would serve a newer 9.9.9 latest, and
        // must never even be asked.
        let registry = spawn_registry(&[MockPackage {
            owner: "wbk",
            name: "app",
            version: "9.9.9",
            manifest_extra: "",
            deps: &[],
            body: "export const newer = 1;",
        }]);
        let mut provider = provider_for(&registry);
        let dir = tempfile::tempdir().expect("tempdir");
        warm_cache(
            dir.path(),
            &pkg_key("base"),
            "1.0.0",
            "",
            "export const base = 1;",
            &[],
        );
        provider.disk_cache = Some(warm_cache(
            dir.path(),
            &pkg_key("app"),
            "1.2.0",
            "",
            "export const x = 1;",
            &[dep("base", "^1", "1.0.0")],
        ));
        provider
            .lock
            .borrow_mut()
            .packages
            .push(staged_lock_entry("smudgy://wbk/app", "1.2.0"));

        provider
            .solve_closure(&["smudgy://wbk/app".to_string()])
            .await;

        assert!(provider.solve.borrow().is_some(), "the walk completed");
        assert_eq!(
            provider
                .top_level_solved
                .borrow()
                .get(&pkg_key("app"))
                .map(String::as_str),
            Some("1.2.0"),
            "the Auto root solved at its staged version, not the registry's latest"
        );
        assert_eq!(
            registry.resolve_hits.load(Ordering::SeqCst),
            0,
            "a fully cached staged closure is walked without a single wire fetch"
        );
        assert_eq!(registry.body_hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn closure_union_from_cache_folds_the_closure_and_fails_closed_on_gaps() {
        let registry = spawn_registry(&[]);
        let mut provider = provider_for(&registry);
        let dir = tempfile::tempdir().expect("tempdir");
        // app@1.2.0 -> base@1.0.0, both metas cached: base grants `net`, app declares a
        // version floor — the fold must union the one and carry the other.
        warm_cache(
            dir.path(),
            &pkg_key("base"),
            "1.0.0",
            r#","permissions":{"net":["example.com"]}"#,
            "export const base = 1;",
            &[],
        );
        provider.disk_cache = Some(warm_cache(
            dir.path(),
            &pkg_key("app"),
            "1.2.0",
            r#","min_smudgy_version":"0.1.0""#,
            "export const x = 1;",
            &[dep("base", "^1", "1.0.0")],
        ));

        let (union, floor) = provider
            .closure_union_from_cache(&pkg_key("app"), "1.2.0")
            .expect("a fully cached closure folds");
        assert_eq!(
            union.net,
            vec!["example.com".to_string()],
            "the dep's manifest permissions joined the union"
        );
        assert_eq!(
            floor.refusal(&semver::Version::new(0, 0, 1)).as_deref(),
            Some(
                "app requires smudgy 0.1.0 or newer \u{2014} this smudgy is 0.0.1; \
                 update smudgy to use it"
            ),
            "the root's floor was folded"
        );

        // A dep edge whose meta is NOT cached voids the whole fold — an incomplete
        // union proves nothing, so the caller must fall back to the network path.
        provider.disk_cache = Some(warm_cache(
            dir.path(),
            &pkg_key("gappy"),
            "1.0.0",
            "",
            "export const g = 1;",
            &[dep("ghost", "^1", "1.0.0")],
        ));
        assert_eq!(
            provider
                .closure_union_from_cache(&pkg_key("gappy"), "1.0.0")
                .map(|_| ()),
            None,
            "a missing dep meta fails the fold closed"
        );
        assert_eq!(
            registry.resolve_hits.load(Ordering::SeqCst),
            0,
            "the disk fold never touches the network, complete or not"
        );
    }
}
