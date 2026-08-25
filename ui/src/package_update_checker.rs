//! The background package-update checker: once per session open, sweep the server's
//! lockfile, ask the registry one batched question, and act on each answer — staging
//! within-consent (or trusted) updates (cache first, lockfile last), offering
//! needs-permissions updates as toasts, and uninstalling definitively-deleted packages.
//!
//! The checker runs UI-side and never touches a session's package provider — no
//! `record_resolution`, no `loaded_packages()`, nothing on the `track` seam. Local
//! overrides and disabled installs are skipped entirely. What the process-global TTL
//! cache (`smudgy_core::models::package_updates`) parks is **registry facts** — the
//! batch response, never a decision: every session-open check evaluates its own
//! server's lockfile entries against those facts ([`evaluate_entry`] is pure over the
//! response + the entry + a cache view), so consent baselines, pins, dismissals, and
//! staged versions always come from the server being checked, and only entries the
//! facts cannot answer cost the network.

use std::collections::{HashMap, HashSet};

use smudgy_cloud::CloudError;
use smudgy_cloud::package_api::{
    CheckUpdatesEntry, CheckUpdatesHave, CheckUpdatesResult, PackageApiClient, UpdateCheckLatest,
};
use smudgy_core::models::local_packages;
use smudgy_core::models::package_updates::{self, PackageVersionRef, facts_key};
use smudgy_core::models::shared_packages::{
    self, LockedPackage, PackageManifest, PackagePermissions, SmudgyVersionFloor,
};
use smudgy_core::session::runtime::package_cache::{
    CachedModule, CachedResolution, PackageCache, PackageKey, is_code_module, package_integrity,
};

use crate::components::toast::{Toast, UpdateOffer};
use crate::windows::automations_window::model::{package_display_name, parse_specifier};

/// The server's request cap on `entries`; a larger lockfile checks in chunks.
const ENTRY_CAP: usize = 64;
/// The server's request cap on `have`; anything beyond is simply not elided.
const HAVE_CAP: usize = 512;

/// What one session's check needs to know about its world.
pub struct CheckContext {
    pub server_name: String,
    /// The signed-in account's nickname, for recognizing local dev-overrides
    /// installed under it (mirroring the engine's owner-segment rule).
    pub account_nickname: Option<String>,
}

/// What one check cycle produced for the owning session: toasts for its window
/// (updates-ready first, then per-package offers) and terminal notice lines.
#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    pub toasts: Vec<Toast>,
    pub notices: Vec<String>,
}

/// Run the whole once-per-open check for one server. Never fails outward — a broken
/// lockfile, an old server (no check-updates route), or a transport error just ends
/// the cycle quietly; installed packages are unaffected either way.
pub async fn run_session_check(client: PackageApiClient, ctx: CheckContext) -> CheckReport {
    let lock = match shared_packages::load_lock(&ctx.server_name) {
        Ok(lock) => lock,
        Err(e) => {
            log::warn!("package update check skipped for {}: {e}", ctx.server_name);
            return CheckReport::default();
        }
    };
    let cache = PackageCache::new().ok();
    let meta_view = |owner: &str, name: &str, version: &str| -> Option<CachedResolution> {
        cache.as_ref()?.read_meta(
            &PackageKey {
                owner: owner.to_string(),
                name: name.to_string(),
            },
            version,
        )
    };
    let facts = package_updates::global();
    let running = shared_packages::running_smudgy_release();

    let mut cycle = CheckCycle {
        client: &client,
        cache: cache.as_ref(),
        server_name: &ctx.server_name,
        ready_count: 0,
        offers: Vec::new(),
        notices: Vec::new(),
    };
    // Entries that need the network this cycle, with their parsed owner/name.
    let mut to_check: Vec<(LockedPackage, String, String)> = Vec::new();

    for entry in &lock.packages {
        if !entry.enabled {
            continue;
        }
        let Some((owner, name)) = parse_specifier(&entry.specifier) else {
            continue;
        };
        if is_local_override(&ctx, &owner, &name) {
            continue;
        }
        // Fresh facts settle the network question, but never the decision: THIS
        // server's entry (its consent, staged version, pin, dismissal) is evaluated
        // against them exactly as it would be against a live response.
        match facts.get_fresh(&facts_key(&owner, &name), entry.staged_version()) {
            Some(result) => {
                let plan = evaluate_entry(&ctx.server_name, entry, &result, &meta_view, &running);
                cycle.execute(entry, &owner, &name, plan).await;
            }
            None => to_check.push((entry.clone(), owner, name)),
        }
    }

    'cycles: for chunk in to_check.chunks(ENTRY_CAP) {
        let entries: Vec<CheckUpdatesEntry> = chunk
            .iter()
            .map(|(entry, owner, name)| CheckUpdatesEntry {
                owner: owner.clone(),
                name: name.clone(),
                installed: entry.staged_version().map(str::to_string),
            })
            .collect();
        let have = cached_have(chunk, &meta_view);
        let response = match client.check_updates(&entries, &have).await {
            Ok(response) => response,
            Err(CloudError::NotFoundOrNoAccess) => {
                // The server predates the route. Offer nothing this cycle —
                // installed packages are unaffected, and the next check after a
                // server deploy picks everything up.
                log::info!("this server has no check-updates route; skipping the update check");
                break 'cycles;
            }
            Err(e) => {
                log::warn!("package update check failed for {}: {e}", ctx.server_name);
                break 'cycles;
            }
        };
        // Results come back in request order, but match defensively by identity.
        let by_key: HashMap<(String, String), &CheckUpdatesResult> = response
            .results
            .iter()
            .map(|result| {
                (
                    (
                        result.owner.to_ascii_lowercase(),
                        result.name.to_ascii_lowercase(),
                    ),
                    result,
                )
            })
            .collect();
        for (entry, owner, name) in chunk {
            let Some(result) = by_key.get(&(owner.to_ascii_lowercase(), name.to_ascii_lowercase()))
            else {
                continue;
            };
            // Park the registry facts first: they hold regardless of what this
            // server's evaluation decides, or whether its staging succeeds.
            facts.put(&facts_key(owner, name), entry.staged_version(), result);
            let plan = evaluate_entry(&ctx.server_name, entry, result, &meta_view, &running);
            cycle.execute(entry, owner, name, plan).await;
        }
    }

    let mut toasts = Vec::new();
    if cycle.ready_count >= 1 {
        toasts.push(Toast::UpdatesReady {
            server_name: ctx.server_name.clone(),
            count: cycle.ready_count,
        });
    }
    toasts.extend(
        cycle
            .offers
            .into_iter()
            .map(|offer| Toast::NeedsPermissions(Box::new(offer))),
    );
    CheckReport {
        toasts,
        notices: cycle.notices,
    }
}

/// One check cycle's mutable state: what the executed plans accumulate for the
/// owning session's report.
struct CheckCycle<'a> {
    client: &'a PackageApiClient,
    cache: Option<&'a PackageCache>,
    server_name: &'a str,
    ready_count: usize,
    offers: Vec<UpdateOffer>,
    notices: Vec<String>,
}

impl CheckCycle<'_> {
    /// Execute one entry's [`EntryPlan`]: nothing, queue an offer, uninstall a dead
    /// entry, or stage an update (prefetch first, lockfile last).
    async fn execute(&mut self, entry: &LockedPackage, owner: &str, name: &str, plan: EntryPlan) {
        log::debug!("update check {}: {:?}", entry.specifier, plan.outcome);
        match plan.action {
            EntryAction::None => {}
            EntryAction::Offer(offer) => self.offers.push(*offer),
            EntryAction::Uninstall => {
                match shared_packages::uninstall_package(self.server_name, &entry.specifier) {
                    Ok(()) => {
                        // The uninstall is scoped to THIS server's lock entry; every
                        // cached meta and blob stays. The cache is machine-global —
                        // another server still installing this package keeps its
                        // "runs from cache" promise — and unreferenced content falls
                        // to the future cache GC, not to this cleanup.
                        self.notices.push(crate::i18n::t!(
                            "notice-package-deleted-uninstalled",
                            "name" => name
                        ));
                    }
                    Err(e) => {
                        log::warn!("failed to uninstall deleted {}: {e}", entry.specifier);
                    }
                }
            }
            EntryAction::Stage(plan) => {
                let StagePlan {
                    from,
                    to,
                    closure,
                    shrink_consent,
                } = *plan;
                let Some(cache) = self.cache else {
                    // No cache directory — staging cannot make the version
                    // serveable, so leave the lockfile alone.
                    return;
                };
                match stage_update(
                    self.client,
                    cache,
                    self.server_name,
                    &entry.specifier,
                    owner,
                    name,
                    &to,
                    &closure,
                )
                .await
                {
                    Ok(()) => {
                        // The shrink auto-accept lands only once the smaller
                        // union's version is actually staged, so consent never
                        // narrows under a still-staged older version.
                        if let Some(union) = shrink_consent
                            && let Err(e) = shared_packages::record_consent(
                                self.server_name,
                                &entry.specifier,
                                &union,
                            )
                        {
                            log::warn!(
                                "failed to record the shrunk consent for {}: {e}",
                                entry.specifier
                            );
                        }
                        self.notices.push(match &from {
                            Some(from) => crate::i18n::t!(
                                "notice-package-update-ready",
                                "name" => name,
                                "from" => from.as_str(),
                                "to" => to.as_str()
                            ),
                            None => crate::i18n::t!(
                                "notice-package-staged-ready",
                                "name" => name,
                                "to" => to.as_str()
                            ),
                        });
                        self.ready_count += 1;
                    }
                    Err(e) => {
                        // The lockfile was left unmoved, so the next cycle's
                        // evaluation reaches the same plan and retries.
                        log::warn!("failed to stage {} {to}: {e}", entry.specifier);
                    }
                }
            }
        }
    }
}

/// Whether an installed specifier is a local dev-override for this account — the
/// engine's rule replicated from UI-visible facts: the owner segment is the reserved
/// `local` placeholder or the signed-in nickname, AND the package's own folder exists
/// under `<server>/packages/<name>/`. Local overrides resolve purely from disk and are
/// never checked.
fn is_local_override(ctx: &CheckContext, owner: &str, name: &str) -> bool {
    let owner_is_local = owner == local_packages::LOCAL_OWNER
        || ctx
            .account_nickname
            .as_deref()
            .is_some_and(|nickname| owner == nickname);
    owner_is_local
        && local_packages::load_local_package(&ctx.server_name, name)
            .ok()
            .flatten()
            .is_some()
}

// ---------------------------------------------------------------------------
// Pure decision logic (testable without the network or the iced shell)
// ---------------------------------------------------------------------------

/// The classification half of an [`EntryPlan`] — one entry's verdict against one
/// server's lockfile state. Logged per entry (the action alone doesn't say *why*
/// nothing happened) and pinned by the decision-logic tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckOutcome {
    /// The latest live version is the staged one; nothing to do.
    UpToDate,
    /// A newer version fits (within consent, or the entry is trusted): stage it.
    StagedUpdate { from: Option<String>, to: String },
    /// A newer version asks for permissions beyond the consented grant, so it needs
    /// an explicit user decision (grant, pin, or dismiss).
    NeedsPermissions { latest: String },
    /// A newer version (or its closure) requires a newer smudgy than this one.
    /// Passive: the Automations pane already surfaces version-floor refusals.
    NeedsSmudgy { latest: String, required: String },
    /// The installed version was yanked but the package still has live versions. The
    /// install keeps running from cache; yank is the routine, reversible hide.
    Yanked,
    /// The installed version was hard-deleted and no live versions remain — the one
    /// definitive deletion signal. The entry is uninstalled.
    Dead,
    /// `status: "not_found"` (absent OR not visible right now, indistinguishable by
    /// design) or an unverifiable answer. The install is left alone.
    Unknown,
}

/// What the checker should do for one entry, alongside the outcome it logs.
#[derive(Debug)]
pub(crate) struct EntryPlan {
    pub(crate) outcome: CheckOutcome,
    pub(crate) action: EntryAction,
}

/// The action half of an [`EntryPlan`].
#[derive(Debug)]
pub(crate) enum EntryAction {
    /// Nothing to execute (statuses, suppressed offers, unknowns).
    None,
    /// A staging to execute (boxed: the plan dwarfs the other variants).
    Stage(Box<StagePlan>),
    /// A needs-permissions update: queue the offer toast.
    Offer(Box<UpdateOffer>),
    /// The definitive deletion signal: uninstall this server's lock entry.
    Uninstall,
}

/// One staging to execute: prefetch `to` + `closure`, then advance the lockfile.
/// `shrink_consent` carries the smaller union to record when the new closure asks for
/// strictly less than the consented baseline (the pane's silent auto-accept); always
/// `None` for a trusted entry, whose consent is moot and never touched.
#[derive(Debug)]
pub(crate) struct StagePlan {
    pub(crate) from: Option<String>,
    pub(crate) to: String,
    pub(crate) closure: Vec<PackageVersionRef>,
    pub(crate) shrink_consent: Option<PackagePermissions>,
}

/// Decide one entry's fate from the batch response, the lockfile entry, and the meta
/// cache. Pure: no I/O beyond the injected `cached_meta` view.
pub(crate) fn evaluate_entry(
    server_name: &str,
    entry: &LockedPackage,
    result: &CheckUpdatesResult,
    cached_meta: &impl Fn(&str, &str, &str) -> Option<CachedResolution>,
    running: &semver::Version,
) -> EntryPlan {
    // Uniform not-found: absent OR not visible right now — never destructive.
    if result.status != "ok" {
        return EntryPlan {
            outcome: CheckOutcome::Unknown,
            action: EntryAction::None,
        };
    }
    let installed_deleted = result.installed.is_some_and(|i| i.deleted);
    let installed_yanked = result.installed.is_some_and(|i| i.yanked);
    let Some(latest) = &result.latest else {
        if installed_deleted {
            // Hard-deleted AND no live versions remain — the one definitive signal.
            return EntryPlan {
                outcome: CheckOutcome::Dead,
                action: EntryAction::Uninstall,
            };
        }
        let outcome = if installed_yanked {
            CheckOutcome::Yanked
        } else {
            CheckOutcome::Unknown
        };
        return EntryPlan {
            outcome,
            action: EntryAction::None,
        };
    };
    let staged = entry.staged_version();
    // Pinned entries get statuses only: no folds, no offers — the "pinning ends the
    // expensive grant checks" promise.
    if entry.pinned_version().is_some() {
        let outcome = if installed_yanked {
            CheckOutcome::Yanked
        } else {
            CheckOutcome::UpToDate
        };
        return EntryPlan {
            outcome,
            action: EntryAction::None,
        };
    }
    // Update candidacy: a strictly newer live version — or ANY live version when the
    // installed one was hard-deleted (the update replaces the dead version).
    let is_candidate = match staged {
        None => true,
        Some(staged) => {
            staged != latest.version
                && (installed_deleted || version_is_newer(&latest.version, staged))
        }
    };
    if !is_candidate {
        let outcome = if installed_yanked {
            CheckOutcome::Yanked
        } else {
            CheckOutcome::UpToDate
        };
        return EntryPlan {
            outcome,
            action: EntryAction::None,
        };
    }
    // A TRUSTED entry runs allow-all on the main isolate, so consent is moot (the
    // pane's rule): a newer live latest stages directly — no closure union fold, no
    // coverage requirement, and never a needs-permissions offer. Only the root
    // manifest's own version floor is still honored, since the load gate would
    // refuse a too-new staged version and strand the install; transitive floors are
    // caught by the same per-manifest gate at load.
    if entry.trusted {
        let Ok(root_manifest) = serde_json::from_value::<PackageManifest>(latest.manifest.clone())
        else {
            // Unparseable manifest: nothing can be floor-checked (and a load would
            // refuse it as invalid anyway) — leave the entry alone.
            return EntryPlan {
                outcome: CheckOutcome::Unknown,
                action: EntryAction::None,
            };
        };
        let mut floor = SmudgyVersionFloor::default();
        floor.fold(&result.name, root_manifest.min_smudgy_version.as_deref());
        if let Some(required) = floor.refusal(running) {
            return EntryPlan {
                outcome: CheckOutcome::NeedsSmudgy {
                    latest: latest.version.clone(),
                    required,
                },
                action: EntryAction::None,
            };
        }
        return EntryPlan {
            outcome: CheckOutcome::StagedUpdate {
                from: staged.map(str::to_string),
                to: latest.version.clone(),
            },
            action: EntryAction::Stage(Box::new(StagePlan {
                from: staged.map(str::to_string),
                to: latest.version.clone(),
                // Best-effort: every closure triple the response + cache can name is
                // prefetched at stage time; a node beyond an uncovered frontier
                // simply fetches lazily at load. The OFFER decision needed no
                // coverage, so a gap must not block the staging.
                closure: collect_closure_refs(result, latest, cached_meta),
                shrink_consent: None,
            })),
        };
    }
    // Fold the offered version's whole-closure union + version floor. Any node
    // covered by neither the response nor the cache means the union cannot be
    // trusted: no offer, no staging.
    let Some(fold) = fold_offer_closure(result, latest, cached_meta) else {
        return EntryPlan {
            outcome: CheckOutcome::Unknown,
            action: EntryAction::None,
        };
    };
    if let Some(reason) = fold.floor.refusal(running) {
        // Version-floor refusals are passive: the pane card already covers them.
        return EntryPlan {
            outcome: CheckOutcome::NeedsSmudgy {
                latest: latest.version.clone(),
                required: reason,
            },
            action: EntryAction::None,
        };
    }
    let consented = entry.consented_permissions.clone().unwrap_or_default();
    let added = fold.union.added_since(&consented);
    if added.is_empty() {
        // Within consent — stage in the background. When the union actually SHRANK,
        // silently adopt the smaller one so the baseline tracks the manifest and
        // never over-grants (the pane's auto-accept, mirrored).
        let removed = consented.added_since(&fold.union);
        let shrink_consent = (!removed.is_empty()).then(|| fold.union.clone());
        return EntryPlan {
            outcome: CheckOutcome::StagedUpdate {
                from: staged.map(str::to_string),
                to: latest.version.clone(),
            },
            action: EntryAction::Stage(Box::new(StagePlan {
                from: staged.map(str::to_string),
                to: latest.version.clone(),
                closure: fold.closure,
                shrink_consent,
            })),
        };
    }
    // New asks beyond consent: an explicit user decision. The outcome names the ask
    // either way; the action respects the persisted dismissal.
    let outcome = CheckOutcome::NeedsPermissions {
        latest: latest.version.clone(),
    };
    let suppressed = entry.dismissed_update_version.as_deref() == Some(latest.version.as_str());
    let action = if suppressed {
        EntryAction::None
    } else {
        EntryAction::Offer(Box::new(UpdateOffer {
            server_name: server_name.to_string(),
            specifier: entry.specifier.clone(),
            name: package_display_name(&entry.specifier).to_string(),
            current: staged.map(str::to_string),
            latest: latest.version.clone(),
            added,
            new_union: fold.union,
            closure: fold.closure,
            needs_smudgy: None,
        }))
    };
    EntryPlan { outcome, action }
}

/// A folded offer closure: the whole-closure permission union, the folded
/// `min_smudgy_version` floor, and every reachable node (root excluded).
struct ClosureFold {
    union: PackagePermissions,
    floor: SmudgyVersionFloor,
    closure: Vec<PackageVersionRef>,
}

/// Walk the offered version's dependency closure over `kind = "dependency"` edges,
/// resolving each node's manifest from the response's closure nodes first, then the
/// meta cache (the server elides nodes the request's `have` covered — and may also
/// filter nodes this viewer cannot see). `None` when any reachable node is covered by
/// neither source, or a manifest fails to parse: an unverifiable union must not drive
/// staging or an offer.
fn fold_offer_closure(
    result: &CheckUpdatesResult,
    latest: &UpdateCheckLatest,
    cached_meta: &impl Fn(&str, &str, &str) -> Option<CachedResolution>,
) -> Option<ClosureFold> {
    let root_manifest: PackageManifest = serde_json::from_value(latest.manifest.clone()).ok()?;
    let mut union = root_manifest.permissions.clone();
    let mut floor = SmudgyVersionFloor::default();
    floor.fold(&result.name, root_manifest.min_smudgy_version.as_deref());

    let by_key: HashMap<
        (String, String, String),
        &smudgy_cloud::package_api::UpdateCheckClosureNode,
    > = result
        .closure
        .iter()
        .map(|node| (folded_triple(&node.owner, &node.name, &node.version), node))
        .collect();
    let mut stack: Vec<(String, String, String)> = latest
        .dependencies
        .iter()
        .filter(|dep| dep.kind == "dependency")
        .map(|dep| {
            (
                dep.owner.clone(),
                dep.name.clone(),
                dep.resolved_version.clone(),
            )
        })
        .collect();
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    let mut closure = Vec::new();
    while let Some((owner, name, version)) = stack.pop() {
        let key = folded_triple(&owner, &name, &version);
        if !seen.insert(key.clone()) {
            continue;
        }
        if let Some(node) = by_key.get(&key) {
            let manifest: PackageManifest = serde_json::from_value(node.manifest.clone()).ok()?;
            union.merge(&manifest.permissions);
            floor.fold(&node.name, manifest.min_smudgy_version.as_deref());
            for dep in node
                .dependencies
                .iter()
                .filter(|dep| dep.kind == "dependency")
            {
                stack.push((
                    dep.owner.clone(),
                    dep.name.clone(),
                    dep.resolved_version.clone(),
                ));
            }
        } else {
            // A node covered by neither the response nor the cache makes the whole
            // closure unverifiable.
            let meta = cached_meta(&owner, &name, &version)?;
            union.merge(&meta.manifest.permissions);
            floor.fold(&name, meta.manifest.min_smudgy_version.as_deref());
            // Cached edges carry no relation kind: they are the resolve wire's
            // locked import-closure edges, exactly the ones this walk follows.
            for dep in &meta.dependencies {
                stack.push((
                    dep.owner_nickname.clone(),
                    dep.name.clone(),
                    dep.resolved_version.clone(),
                ));
            }
        }
        closure.push(PackageVersionRef {
            owner,
            name,
            version,
        });
    }
    Some(ClosureFold {
        union,
        floor,
        closure,
    })
}

/// The reachable closure triples of the offered version, best-effort: the same walk
/// as [`fold_offer_closure`] but collecting identities only, so a node covered by
/// neither the response nor the cache still joins the list (its triple is named by
/// the edge that reached it) — its own edges just cannot be walked further. The
/// trusted staging path uses this: it needs no manifests, only what to prefetch.
fn collect_closure_refs(
    result: &CheckUpdatesResult,
    latest: &UpdateCheckLatest,
    cached_meta: &impl Fn(&str, &str, &str) -> Option<CachedResolution>,
) -> Vec<PackageVersionRef> {
    let by_key: HashMap<
        (String, String, String),
        &smudgy_cloud::package_api::UpdateCheckClosureNode,
    > = result
        .closure
        .iter()
        .map(|node| (folded_triple(&node.owner, &node.name, &node.version), node))
        .collect();
    let mut stack: Vec<(String, String, String)> = latest
        .dependencies
        .iter()
        .filter(|dep| dep.kind == "dependency")
        .map(|dep| {
            (
                dep.owner.clone(),
                dep.name.clone(),
                dep.resolved_version.clone(),
            )
        })
        .collect();
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    let mut closure = Vec::new();
    while let Some((owner, name, version)) = stack.pop() {
        let key = folded_triple(&owner, &name, &version);
        if !seen.insert(key.clone()) {
            continue;
        }
        if let Some(node) = by_key.get(&key) {
            for dep in node
                .dependencies
                .iter()
                .filter(|dep| dep.kind == "dependency")
            {
                stack.push((
                    dep.owner.clone(),
                    dep.name.clone(),
                    dep.resolved_version.clone(),
                ));
            }
        } else if let Some(meta) = cached_meta(&owner, &name, &version) {
            for dep in &meta.dependencies {
                stack.push((
                    dep.owner_nickname.clone(),
                    dep.name.clone(),
                    dep.resolved_version.clone(),
                ));
            }
        }
        closure.push(PackageVersionRef {
            owner,
            name,
            version,
        });
    }
    closure
}

/// Case-folded closure-walk key: owner nicknames and package names are
/// case-insensitive identities on the registry.
fn folded_triple(owner: &str, name: &str, version: &str) -> (String, String, String) {
    (
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase(),
        version.to_string(),
    )
}

/// Whether `candidate` names a strictly newer version than `baseline` — semver order
/// when both parse, else the conservative "different means newer" (matching the
/// lockfile's dismissal rule), so an oddly-versioned latest still surfaces rather
/// than silently never updating.
fn version_is_newer(candidate: &str, baseline: &str) -> bool {
    match (
        semver::Version::parse(candidate),
        semver::Version::parse(baseline),
    ) {
        (Ok(candidate), Ok(baseline)) => candidate > baseline,
        _ => candidate != baseline,
    }
}

/// The `have` list for one request chunk: every distinct closure-node triple
/// reachable from each entry's staged version through cached metas (nodes without a
/// cached meta are neither sent nor walked — the server must inline them). Truncated
/// to the server's cap; the overflow merely goes un-elided.
pub(crate) fn cached_have(
    entries: &[(LockedPackage, String, String)],
    cached_meta: &impl Fn(&str, &str, &str) -> Option<CachedResolution>,
) -> Vec<CheckUpdatesHave> {
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    let mut have = Vec::new();
    for (entry, owner, name) in entries {
        let Some(staged) = entry.staged_version() else {
            continue;
        };
        let mut stack = vec![(owner.clone(), name.clone(), staged.to_string())];
        while let Some((owner, name, version)) = stack.pop() {
            if !seen.insert(folded_triple(&owner, &name, &version)) {
                continue;
            }
            let Some(meta) = cached_meta(&owner, &name, &version) else {
                continue;
            };
            for dep in &meta.dependencies {
                stack.push((
                    dep.owner_nickname.clone(),
                    dep.name.clone(),
                    dep.resolved_version.clone(),
                ));
            }
            have.push(CheckUpdatesHave {
                owner,
                name,
                version,
            });
        }
    }
    have.truncate(HAVE_CAP);
    have
}

// ---------------------------------------------------------------------------
// Staging (shared by the checker and the review modal's grant path)
// ---------------------------------------------------------------------------

/// Stage an accepted update offer: prefetch the offered version and its whole closure
/// into the cache, then advance the lockfile's staged version. The caller records
/// consent before staging (the staged closure union must sit within the consented
/// grant).
///
/// # Errors
/// Returns a display-ready message when any resolve, fetch, cache write, or the
/// lockfile advance fails; the lockfile is left unmoved on any prefetch failure.
pub async fn stage_offer(client: PackageApiClient, offer: UpdateOffer) -> Result<(), String> {
    let cache = PackageCache::new().map_err(|e| format!("package cache unavailable: {e}"))?;
    let (owner, name) = parse_specifier(&offer.specifier)
        .ok_or_else(|| format!("not a package specifier: {}", offer.specifier))?;
    stage_update(
        &client,
        &cache,
        &offer.server_name,
        &offer.specifier,
        &owner,
        &name,
        &offer.latest,
        &offer.closure,
    )
    .await
}

/// The staging primitive: prefetch the root version and every closure node, then
/// advance the lockfile. Ordering is the safety — the lockfile only ever points at
/// content the cache can already serve.
#[allow(clippy::too_many_arguments)]
async fn stage_update(
    client: &PackageApiClient,
    cache: &PackageCache,
    server_name: &str,
    specifier: &str,
    owner: &str,
    name: &str,
    version: &str,
    closure: &[PackageVersionRef],
) -> Result<(), String> {
    prefetch_version(client, cache, owner, name, version).await?;
    for node in closure {
        prefetch_version(client, cache, &node.owner, &node.name, &node.version).await?;
    }
    shared_packages::stage_resolved_version(server_name, specifier, version)
        .map_err(|e| format!("failed to stage {specifier} {version}: {e}"))?;
    Ok(())
}

/// Resolve one concrete version and persist everything the cache needs to serve it
/// offline: the metadata entry plus every missing **code** blob. Asset blobs stay
/// lazy, exactly as at load time — they never gate offline readiness. A version the
/// cache can already serve (meta + all code blobs) costs **zero network**: published
/// content is immutable, so presence needs no confirmation — this is what lets one
/// server's staging ride entirely on another's prefetch.
///
/// # Errors
/// Returns a display-ready message when the resolve, a body fetch, or a cache write
/// fails, or the manifest does not parse — staging must not advance onto a version the
/// cache cannot actually serve.
pub async fn prefetch_version(
    client: &PackageApiClient,
    cache: &PackageCache,
    owner: &str,
    name: &str,
    version: &str,
) -> Result<(), String> {
    let key = PackageKey {
        owner: owner.to_string(),
        name: name.to_string(),
    };
    if let Some(meta) = cache.read_meta(&key, version)
        && cache.has_all_code_blobs(&meta)
    {
        return Ok(());
    }
    let wire = client
        .resolve_package(owner, name, Some(version))
        .await
        .map_err(|e| format!("failed to resolve {owner}/{name}@{version}: {e}"))?;
    let manifest = PackageManifest::parse(&wire.manifest.to_string())
        .map_err(|e| format!("invalid manifest for {owner}/{name}@{version}: {e}"))?;
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
    // Write-once in the common case, healing a legacy or torn cache file — the same
    // rule as the engine provider's meta writes (`PackageCache::refresh_meta`).
    cache
        .refresh_meta(&key, &wire.version, &meta)
        .map_err(|e| format!("failed to cache {owner}/{name}@{version}: {e}"))?;
    for module in wire
        .modules
        .iter()
        .filter(|module| is_code_module(&module.media_type, &module.subpath))
    {
        if cache.has_blob(&module.content_hash) {
            continue;
        }
        let body = client
            .fetch_module_bytes(&module.content_url, &module.content_hash)
            .await
            .map_err(|e| {
                format!(
                    "failed to fetch {owner}/{name}@{version} {}: {e}",
                    module.subpath
                )
            })?;
        cache
            .write_blob_bytes(&module.content_hash, &body)
            .map_err(|e| {
                format!(
                    "failed to cache {owner}/{name}@{version} {}: {e}",
                    module.subpath
                )
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use smudgy_cloud::package_api::{
        CheckUpdatesResult, UpdateCheckClosureNode, UpdateCheckDependency, UpdateCheckInstalled,
        UpdateCheckLatest,
    };
    use smudgy_cloud::{Credential, CredentialSource};
    use smudgy_core::models::shared_packages::UpdateMode;

    use super::*;

    fn entry(staged: Option<&str>) -> LockedPackage {
        let mut entry = LockedPackage::new("smudgy://wbk/mapper", UpdateMode::Auto);
        entry.last_resolved_version = staged.map(str::to_string);
        entry.consented_permissions = Some(PackagePermissions::default());
        entry
    }

    fn manifest_json(net: &[&str], min_smudgy: Option<&str>) -> serde_json::Value {
        let mut manifest = serde_json::json!({
            "name": "mapper",
            "version": "9.9.9",
            "permissions": { "net": net }
        });
        if let Some(min) = min_smudgy {
            manifest["min_smudgy_version"] = serde_json::Value::String(min.to_string());
        }
        manifest
    }

    fn latest(version: &str, net: &[&str]) -> UpdateCheckLatest {
        UpdateCheckLatest {
            version: version.to_string(),
            published_at: chrono::Utc::now(),
            manifest: manifest_json(net, None),
            modules: Vec::new(),
            dependencies: Vec::new(),
        }
    }

    fn ok_result(latest: Option<UpdateCheckLatest>) -> CheckUpdatesResult {
        CheckUpdatesResult {
            owner: "wbk".into(),
            name: "mapper".into(),
            status: "ok".into(),
            installed: Some(UpdateCheckInstalled {
                yanked: false,
                deleted: false,
            }),
            latest,
            closure: Vec::new(),
        }
    }

    fn no_meta(_: &str, _: &str, _: &str) -> Option<CachedResolution> {
        None
    }

    fn running() -> semver::Version {
        semver::Version::new(1, 0, 0)
    }

    #[test]
    fn up_to_date_when_latest_equals_staged() {
        let plan = evaluate_entry(
            "arctic",
            &entry(Some("1.2.0")),
            &ok_result(Some(latest("1.2.0", &[]))),
            &no_meta,
            &running(),
        );
        assert_eq!(plan.outcome, CheckOutcome::UpToDate);
        assert!(matches!(plan.action, EntryAction::None));
    }

    #[test]
    fn within_consent_update_stages() {
        let mut e = entry(Some("1.2.0"));
        e.consented_permissions =
            Some(serde_json::from_value(serde_json::json!({ "net": ["example.com"] })).unwrap());
        let plan = evaluate_entry(
            "arctic",
            &e,
            &ok_result(Some(latest("1.3.0", &["example.com"]))),
            &no_meta,
            &running(),
        );
        assert_eq!(
            plan.outcome,
            CheckOutcome::StagedUpdate {
                from: Some("1.2.0".into()),
                to: "1.3.0".into()
            }
        );
        match plan.action {
            EntryAction::Stage(plan) => {
                assert_eq!(plan.to, "1.3.0");
                assert!(plan.shrink_consent.is_none(), "same union — nothing shrank");
            }
            other => panic!("expected Stage, got {other:?}"),
        }
    }

    #[test]
    fn shrunk_union_is_silently_adopted() {
        let mut e = entry(Some("1.2.0"));
        e.consented_permissions = Some(
            serde_json::from_value(serde_json::json!({ "net": ["example.com", "old.example"] }))
                .unwrap(),
        );
        let plan = evaluate_entry(
            "arctic",
            &e,
            &ok_result(Some(latest("1.3.0", &["example.com"]))),
            &no_meta,
            &running(),
        );
        match plan.action {
            EntryAction::Stage(plan) => {
                let union = plan.shrink_consent.expect("the smaller union is recorded");
                assert_eq!(union.net, vec!["example.com".to_string()]);
            }
            other => panic!("expected Stage, got {other:?}"),
        }
    }

    #[test]
    fn new_asks_produce_an_offer() {
        let plan = evaluate_entry(
            "arctic",
            &entry(Some("1.2.0")),
            &ok_result(Some(latest("1.3.0", &["evil.example"]))),
            &no_meta,
            &running(),
        );
        assert!(matches!(
            plan.outcome,
            CheckOutcome::NeedsPermissions { .. }
        ));
        match plan.action {
            EntryAction::Offer(offer) => {
                assert_eq!(offer.specifier, "smudgy://wbk/mapper");
                assert_eq!(offer.latest, "1.3.0");
                assert_eq!(offer.added.net, vec!["evil.example".to_string()]);
            }
            other => panic!("expected Offer, got {other:?}"),
        }
    }

    #[test]
    fn dismissal_suppresses_the_offer_but_not_the_outcome() {
        let mut e = entry(Some("1.2.0"));
        e.dismissed_update_version = Some("1.3.0".into());
        let plan = evaluate_entry(
            "arctic",
            &e,
            &ok_result(Some(latest("1.3.0", &["evil.example"]))),
            &no_meta,
            &running(),
        );
        assert!(matches!(
            plan.outcome,
            CheckOutcome::NeedsPermissions { .. }
        ));
        assert!(matches!(plan.action, EntryAction::None));
    }

    #[test]
    fn pinned_entries_never_fold_or_offer() {
        let mut e = entry(Some("1.2.0"));
        e.mode = UpdateMode::Pinned {
            version: "1.2.0".into(),
        };
        // The closure is deliberately uncoverable: a pinned entry must not even
        // reach the fold.
        let mut result = ok_result(Some(UpdateCheckLatest {
            dependencies: vec![UpdateCheckDependency {
                owner: "wbk".into(),
                name: "core".into(),
                range: "^1".into(),
                resolved_version: "1.0.0".into(),
                kind: "dependency".into(),
            }],
            ..latest("2.0.0", &["evil.example"])
        }));
        result.closure = Vec::new();
        let plan = evaluate_entry("arctic", &e, &result, &no_meta, &running());
        assert_eq!(plan.outcome, CheckOutcome::UpToDate);
        assert!(matches!(plan.action, EntryAction::None));
    }

    #[test]
    fn trusted_updates_stage_without_a_consent_fold() {
        // The pane's rule: trusted ⇒ consent is moot. A permission-adding update —
        // whose closure is not even coverable — stages directly: no union fold, no
        // coverage requirement, never an offer. Consent is left untouched (no
        // shrink), and the closure refs are still collected best-effort for the
        // prefetch.
        let mut e = entry(Some("1.2.0"));
        e.trusted = true;
        let result = ok_result(Some(UpdateCheckLatest {
            dependencies: vec![UpdateCheckDependency {
                owner: "wbk".into(),
                name: "core".into(),
                range: "^1".into(),
                resolved_version: "1.0.0".into(),
                kind: "dependency".into(),
            }],
            ..latest("1.3.0", &["evil.example"])
        }));
        let plan = evaluate_entry("arctic", &e, &result, &no_meta, &running());
        assert_eq!(
            plan.outcome,
            CheckOutcome::StagedUpdate {
                from: Some("1.2.0".into()),
                to: "1.3.0".into()
            }
        );
        match plan.action {
            EntryAction::Stage(plan) => {
                assert!(
                    plan.shrink_consent.is_none(),
                    "trusted consent is never touched"
                );
                assert_eq!(
                    plan.closure,
                    vec![PackageVersionRef {
                        owner: "wbk".into(),
                        name: "core".into(),
                        version: "1.0.0".into(),
                    }],
                    "the uncovered dep's triple still joins the prefetch list"
                );
            }
            other => panic!("expected Stage, got {other:?}"),
        }
    }

    #[test]
    fn trusted_with_no_consent_record_never_offers() {
        // `consented_permissions: None` folds to the empty union, which every added
        // permission exceeds — a trusted entry must stage regardless, never toast.
        let mut e = entry(Some("1.2.0"));
        e.trusted = true;
        e.consented_permissions = None;
        let plan = evaluate_entry(
            "arctic",
            &e,
            &ok_result(Some(latest("1.3.0", &["evil.example"]))),
            &no_meta,
            &running(),
        );
        assert!(matches!(plan.outcome, CheckOutcome::StagedUpdate { .. }));
        assert!(matches!(plan.action, EntryAction::Stage(_)));
    }

    #[test]
    fn trusted_updates_still_honor_the_root_version_floor() {
        // Trust waives consent, not the load gate: staging a version whose own
        // manifest floor exceeds this smudgy would strand the install at load.
        let mut e = entry(Some("1.2.0"));
        e.trusted = true;
        let result = ok_result(Some(UpdateCheckLatest {
            manifest: manifest_json(&[], Some("999.0.0")),
            ..latest("1.3.0", &[])
        }));
        let plan = evaluate_entry("arctic", &e, &result, &no_meta, &running());
        assert!(matches!(plan.outcome, CheckOutcome::NeedsSmudgy { .. }));
        assert!(matches!(plan.action, EntryAction::None));
    }

    #[test]
    fn shared_facts_evaluate_per_server_pin_and_offer() {
        // The facts cache stores the registry's ANSWER; each server's entry decides
        // for itself. One response: server A pinned its entry (statuses only, no
        // offer), server B's Auto entry still gets its offer — with B's OWN staged
        // version as `current`, so "Pin current" pins B's version.
        let result = ok_result(Some(latest("1.3.0", &["evil.example"])));
        let mut pinned = entry(Some("1.2.0"));
        pinned.mode = UpdateMode::Pinned {
            version: "1.2.0".into(),
        };
        let plan_a = evaluate_entry("server-a", &pinned, &result, &no_meta, &running());
        assert_eq!(plan_a.outcome, CheckOutcome::UpToDate);
        assert!(matches!(plan_a.action, EntryAction::None));

        let plan_b = evaluate_entry(
            "server-b",
            &entry(Some("1.2.0")),
            &result,
            &no_meta,
            &running(),
        );
        match plan_b.action {
            EntryAction::Offer(offer) => {
                assert_eq!(offer.server_name, "server-b");
                assert_eq!(offer.current.as_deref(), Some("1.2.0"));
                assert_eq!(offer.latest, "1.3.0");
            }
            other => panic!("expected B's offer, got {other:?}"),
        }
    }

    #[test]
    fn a_second_server_stages_from_anothers_cached_facts() {
        // Server A's network check parked the registry facts; server B opens within
        // the TTL with the same staged version. B's sweep is answered from the
        // cache — no batch call — and B's OWN entry evaluates to the same staging
        // plan. The prefetch side is zero-network too once A's staging warmed the
        // content cache (`prefetch_of_a_fully_cached_version_touches_no_network`).
        let facts = package_updates::FactsCache::with_ttl(std::time::Duration::from_mins(5));
        let key = facts_key("wbk", "mapper");
        facts.put(&key, Some("1.2.0"), &ok_result(Some(latest("1.3.0", &[]))));

        let for_b = facts
            .get_fresh(&key, Some("1.2.0"))
            .expect("B's sweep is answered without the network");
        let plan = evaluate_entry(
            "server-b",
            &entry(Some("1.2.0")),
            &for_b,
            &no_meta,
            &running(),
        );
        assert_eq!(
            plan.outcome,
            CheckOutcome::StagedUpdate {
                from: Some("1.2.0".into()),
                to: "1.3.0".into()
            }
        );
        assert!(matches!(plan.action, EntryAction::Stage(_)));
    }

    #[test]
    fn dead_facts_uninstall_each_servers_own_entry() {
        // The definitive deletion signal, evaluated per server: the same cached
        // facts drive an uninstall of EACH server's entry (the action is scoped to
        // the entry — cached content is never purged).
        let mut result = ok_result(None);
        result.installed = Some(UpdateCheckInstalled {
            yanked: false,
            deleted: true,
        });
        for server in ["server-a", "server-b"] {
            let plan = evaluate_entry(server, &entry(Some("1.2.0")), &result, &no_meta, &running());
            assert_eq!(plan.outcome, CheckOutcome::Dead, "{server}");
            assert!(matches!(plan.action, EntryAction::Uninstall), "{server}");
        }
    }

    #[test]
    fn uncoverable_closure_skips_without_action() {
        // Also the over-cap contract's client path: the server answers an over-cap
        // closure with `closure: []` and intact status/installed/latest, which — with
        // no cached coverage — is exactly this shape: cannot evaluate, no offer, no
        // staging, and the rest of the batch is unaffected (entries are independent).
        let result = ok_result(Some(UpdateCheckLatest {
            dependencies: vec![UpdateCheckDependency {
                owner: "wbk".into(),
                name: "core".into(),
                range: "^1".into(),
                resolved_version: "1.0.0".into(),
                kind: "dependency".into(),
            }],
            ..latest("1.3.0", &[])
        }));
        let plan = evaluate_entry(
            "arctic",
            &entry(Some("1.2.0")),
            &result,
            &no_meta,
            &running(),
        );
        assert_eq!(plan.outcome, CheckOutcome::Unknown);
        assert!(matches!(plan.action, EntryAction::None));
    }

    #[test]
    fn requires_edges_are_not_closure_edges() {
        // A lone kind="requires" edge is a co-install, not an import: the closure
        // fold must ignore it entirely rather than declare the closure uncoverable.
        let result = ok_result(Some(UpdateCheckLatest {
            dependencies: vec![UpdateCheckDependency {
                owner: "wbk".into(),
                name: "companion".into(),
                range: "^1".into(),
                resolved_version: "1.0.0".into(),
                kind: "requires".into(),
            }],
            ..latest("1.3.0", &[])
        }));
        let plan = evaluate_entry(
            "arctic",
            &entry(Some("1.2.0")),
            &result,
            &no_meta,
            &running(),
        );
        assert!(matches!(plan.outcome, CheckOutcome::StagedUpdate { .. }));
    }

    #[test]
    fn closure_union_folds_response_and_cache_nodes() {
        // latest → dep-a (inlined in the response) → dep-b (elided; cached meta).
        let result = CheckUpdatesResult {
            closure: vec![UpdateCheckClosureNode {
                owner: "wbk".into(),
                name: "dep-a".into(),
                version: "1.0.0".into(),
                manifest: serde_json::json!({
                    "name": "dep-a", "version": "1.0.0",
                    "permissions": { "net": ["a.example"] }
                }),
                dependencies: vec![UpdateCheckDependency {
                    owner: "wbk".into(),
                    name: "dep-b".into(),
                    range: "^2".into(),
                    resolved_version: "2.0.0".into(),
                    kind: "dependency".into(),
                }],
            }],
            ..ok_result(Some(UpdateCheckLatest {
                dependencies: vec![UpdateCheckDependency {
                    owner: "wbk".into(),
                    name: "dep-a".into(),
                    range: "^1".into(),
                    resolved_version: "1.0.0".into(),
                    kind: "dependency".into(),
                }],
                ..latest("1.3.0", &[])
            }))
        };
        let cached = |owner: &str, name: &str, version: &str| -> Option<CachedResolution> {
            (owner == "wbk" && name == "dep-b" && version == "2.0.0").then(|| CachedResolution {
                version: "2.0.0".into(),
                integrity: "i".into(),
                manifest: PackageManifest::parse(
                    r#"{ "name": "dep-b", "version": "2.0.0",
                         "permissions": { "net": ["b.example"] } }"#,
                )
                .unwrap(),
                modules: Vec::new(),
                dependencies: Vec::new(),
            })
        };
        let plan = evaluate_entry(
            "arctic",
            &entry(Some("1.2.0")),
            &result,
            &cached,
            &running(),
        );
        match plan.action {
            EntryAction::Offer(offer) => {
                let mut nets = offer.new_union.net.clone();
                nets.sort();
                assert_eq!(nets, ["a.example", "b.example"]);
                let mut closure: Vec<String> = offer
                    .closure
                    .iter()
                    .map(|node| format!("{}@{}", node.name, node.version))
                    .collect();
                closure.sort();
                assert_eq!(closure, ["dep-a@1.0.0", "dep-b@2.0.0"]);
            }
            other => panic!("expected Offer, got {other:?}"),
        }
    }

    #[test]
    fn needs_smudgy_is_passive() {
        let result = ok_result(Some(UpdateCheckLatest {
            manifest: manifest_json(&[], Some("999.0.0")),
            ..latest("1.3.0", &[])
        }));
        let plan = evaluate_entry(
            "arctic",
            &entry(Some("1.2.0")),
            &result,
            &no_meta,
            &running(),
        );
        assert!(matches!(plan.outcome, CheckOutcome::NeedsSmudgy { .. }));
        assert!(matches!(plan.action, EntryAction::None));
    }

    #[test]
    fn dead_uninstalls_but_not_found_leaves_alone() {
        // deleted + no live versions: the definitive signal.
        let mut result = ok_result(None);
        result.installed = Some(UpdateCheckInstalled {
            yanked: false,
            deleted: true,
        });
        let plan = evaluate_entry(
            "arctic",
            &entry(Some("1.2.0")),
            &result,
            &no_meta,
            &running(),
        );
        assert_eq!(plan.outcome, CheckOutcome::Dead);
        assert!(matches!(plan.action, EntryAction::Uninstall));

        // Uniform not_found (signed out, lapsed grant, or truly gone): untouchable.
        let mut result = ok_result(None);
        result.status = "not_found".into();
        result.installed = None;
        let plan = evaluate_entry(
            "arctic",
            &entry(Some("1.2.0")),
            &result,
            &no_meta,
            &running(),
        );
        assert_eq!(plan.outcome, CheckOutcome::Unknown);
        assert!(matches!(plan.action, EntryAction::None));
    }

    #[test]
    fn deleted_installed_with_live_latest_updates_even_downward() {
        // The author hard-deleted the staged newest; the older live version
        // replaces the dead one.
        let mut result = ok_result(Some(latest("1.1.0", &[])));
        result.installed = Some(UpdateCheckInstalled {
            yanked: false,
            deleted: true,
        });
        let plan = evaluate_entry(
            "arctic",
            &entry(Some("1.2.0")),
            &result,
            &no_meta,
            &running(),
        );
        assert!(matches!(plan.outcome, CheckOutcome::StagedUpdate { .. }));
    }

    #[test]
    fn yanked_installed_without_newer_is_status_only() {
        let mut result = ok_result(Some(latest("1.1.0", &[])));
        result.installed = Some(UpdateCheckInstalled {
            yanked: true,
            deleted: false,
        });
        let plan = evaluate_entry(
            "arctic",
            &entry(Some("1.2.0")),
            &result,
            &no_meta,
            &running(),
        );
        assert_eq!(plan.outcome, CheckOutcome::Yanked);
        assert!(matches!(plan.action, EntryAction::None));
    }

    #[test]
    fn cached_have_walks_dependency_edges() {
        let cached = |owner: &str, name: &str, version: &str| -> Option<CachedResolution> {
            let (deps, known) = match (owner, name, version) {
                ("wbk", "mapper", "1.2.0") => (
                    vec![smudgy_cloud::ResolvedDependency {
                        owner_nickname: "wbk".into(),
                        name: "dep-a".into(),
                        range: "^1".into(),
                        resolved_version: "1.0.0".into(),
                    }],
                    true,
                ),
                ("wbk", "dep-a", "1.0.0") => (Vec::new(), true),
                _ => (Vec::new(), false),
            };
            known.then(|| CachedResolution {
                version: version.to_string(),
                integrity: "i".into(),
                manifest: PackageManifest::parse(r#"{ "name": "x", "version": "0.0.0" }"#).unwrap(),
                modules: Vec::new(),
                dependencies: deps,
            })
        };
        let entries = vec![(
            entry(Some("1.2.0")),
            "wbk".to_string(),
            "mapper".to_string(),
        )];
        let mut have: Vec<String> = cached_have(&entries, &cached)
            .into_iter()
            .map(|h| format!("{}/{}@{}", h.owner, h.name, h.version))
            .collect();
        have.sort();
        assert_eq!(have, ["wbk/dep-a@1.0.0", "wbk/mapper@1.2.0"]);
    }

    #[tokio::test]
    async fn prefetch_of_a_fully_cached_version_touches_no_network() {
        // The zero-call side of "one server's staging rides another's prefetch": a
        // version whose meta + code blobs are already cached short-circuits before
        // the resolve. The client points at a dead address, so ANY network attempt
        // fails the prefetch.
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = PackageCache::with_root(dir.path().to_path_buf());
        let key = PackageKey {
            owner: "wbk".into(),
            name: "mapper".into(),
        };
        cache.write_blob("aa11", "export const x = 1;").unwrap();
        cache
            .write_meta(
                &key,
                "1.3.0",
                &CachedResolution {
                    version: "1.3.0".into(),
                    integrity: "i".into(),
                    manifest: PackageManifest::parse(r#"{ "name": "mapper", "version": "1.3.0" }"#)
                        .unwrap(),
                    modules: vec![CachedModule {
                        subpath: "index.ts".into(),
                        content_hash: "aa11".into(),
                        media_type: "application/typescript".into(),
                    }],
                    dependencies: Vec::new(),
                },
            )
            .unwrap();
        let client = PackageApiClient::new(
            "http://127.0.0.1:0",
            CredentialSource::new(Some(Credential::ApiKey("test".into()))),
        );
        prefetch_version(&client, &cache, "wbk", "mapper", "1.3.0")
            .await
            .expect("a fully cached version prefetches without the network");
    }
}
