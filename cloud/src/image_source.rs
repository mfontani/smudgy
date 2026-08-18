//! Image `src` grammar, per-isolate policy, and source normalization for the script-facing
//! `<Image>` widget and the canvas `image` shape.
//!
//! Lives **here** for the same crate-DAG reason as [`crate::WidgetsEnabled`]: the consumers
//! are the leaf `smudgy_widgets` build ops (which cannot name `core`) and the resolution
//! inputs come from `core` (which must not depend on `smudgy_widgets`). `core` seeds an
//! [`ImageSourcePolicy`] into each isolate's `OpState`; the widget ops register creators
//! against it once per `smudgy:widgets` instance and normalize raw `src` strings into
//! [`ResolvedImageSource`]s with [`resolve_src`].
//!
//! ## Hot-path contract
//!
//! `createWidget()` is called every frame by some scripts, so everything in this module is
//! **pure and lexical**: no filesystem access, no syscalls, no locks. Canonicalization and
//! existence checks happen in the ui-side fetcher, where I/O is sanctioned; a source that
//! passes here and fails there degrades to the widget's broken state. Callers are expected
//! to memoize `resolve_src` results per `(creator, raw)` (see [`memo_key`]) — the functions
//! here are cheap, but URL parsing per frame is not free.
//!
//! ## Security model (plan D1/D2, adversarially reviewed)
//!
//! - Relative srcs resolve against the **defining module's directory**; `..` is permitted
//!   only for trusted user modules, never for packages (descend-only, so a package src can
//!   never escape its membership-validated root) and never for **store-bound** values
//!   (a binding's producer is not the widget's author — an unclamped `..` would let a
//!   hostile game server display arbitrary local files).
//! - `@/` addresses the creator's root (package root, or `<server>/modules/`).
//! - Relative and `@/` forms are never URL-parsed or percent-decoded: `%2e%2e` is a weird
//!   literal filename, not a traversal.
//! - `http(s)` srcs are gated on the package's consented `net` grants with deno-equivalent
//!   descriptor semantics ([`NetGrants`]); hosts come from a parsed [`Url`], never the raw
//!   string (`https://ok.com@evil.com/x` must resolve to `evil.com`).
//! - Absolute paths are trusted-only (or within consented `read` grants), and always denied
//!   for bound values.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use sha2::{Digest, Sha256};
use url::Url;

/// Byte cap for a whole `data:` URI src (encoded form). Scene budgets and the `OpState` memo
/// assume srcs are bounded; this is the bound.
pub const MAX_DATA_URI_BYTES: usize = 2 * 1024 * 1024;

/// Above this length, [`memo_key`] digests instead of cloning the raw src — hashing a 2 MiB
/// `data:` URI per frame is exactly the cost the hot-path review said to avoid.
const MEMO_INLINE_MAX: usize = 512;

// ---------------------------------------------------------------------------------------------
// Policy (seeded per isolate by core)

/// One consented `net` grant, pre-parsed by `core` from the manifest allowlist so the
/// per-src check is a cheap comparison. Mirrors deno's `NetDescriptor` containment:
/// a bare host covers any port, `host:port` covers only that port, `*.host` covers proper
/// subdomains only, `*` covers every host/port, and `*:port` covers that port on every host.
/// Named hosts are stored IDNA/ASCII-lowercased without any trailing dot.
///
/// Unsupported grant spellings (CIDR subnets, malformed hosts) must be **dropped by the
/// parser** ([`NetGrant::parse`] returns `None`) — fail-closed: the package simply cannot
/// image-load those hosts (its own `fetch()` still can; divergence is logged at parse).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetGrant {
    /// IDNA-normalized host (or IP literal, lowercased; IPv6 without brackets). Empty for the
    /// special any-host `*` grant.
    host: String,
    /// `true` for `*` / `*:port`: every host matches (the optional port still applies).
    any_host: bool,
    /// `true` for a `*.host` grant: matches proper subdomains of `host`, not `host` itself.
    wildcard: bool,
    /// `Some(port)` restricts to that port; `None` covers any port.
    port: Option<u16>,
}

impl NetGrant {
    /// Parse one manifest `net` entry (`host`, `host:port`, `*.host`, `*.host:port`, `*`,
    /// `*:port`, IP literals, `[v6]:port`). Returns `None` for anything it cannot faithfully
    /// represent.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let (raw, wildcard) = match raw.strip_prefix("*.") {
            Some(rest) => (rest, true),
            None => (raw, false),
        };
        // Split a trailing `:port`, careful with IPv6: `[::1]:443` / bare `::1`.
        let (host_part, port) = if let Some(rest) = raw.strip_prefix('[') {
            let (v6, after) = rest.split_once(']')?;
            let port = match after.strip_prefix(':') {
                Some(p) => Some(p.parse::<u16>().ok()?),
                None if after.is_empty() => None,
                None => return None,
            };
            (v6.to_string(), port)
        } else if raw.matches(':').count() > 1 {
            // Bracket-less IPv6 literal: all colons belong to the address.
            (raw.to_string(), None)
        } else if let Some((h, p)) = raw.rsplit_once(':') {
            (h.to_string(), Some(p.parse::<u16>().ok()?))
        } else {
            (raw.to_string(), None)
        };
        let any_host = host_part == "*";
        let host = if any_host {
            String::new()
        } else {
            normalize_host(&host_part)?
        };
        Some(Self {
            host,
            any_host,
            wildcard,
            port,
        })
    }

    fn covers(&self, host: &str, port: u16) -> bool {
        if let Some(granted) = self.port
            && granted != port
        {
            return false;
        }
        if self.any_host {
            true
        } else if self.wildcard {
            // Proper subdomains only, matching deno: `*.example.com` covers
            // `a.example.com` but not `example.com`.
            host.len() > self.host.len() + 1
                && host.ends_with(&self.host)
                && host.as_bytes()[host.len() - self.host.len() - 1] == b'.'
        } else {
            host == self.host
        }
    }
}

/// IDNA/ASCII-normalize a host the same way a parsed [`Url`]'s `host_str()` presents it:
/// lowercase, punycoded, no trailing dot, IPv6 without brackets.
fn normalize_host(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('.');
    if trimmed.is_empty() {
        return None;
    }
    // `url::Host::parse` wants IPv6 bracketed; grant spellings arrive bare.
    if trimmed.contains(':') {
        return trimmed
            .parse::<std::net::Ipv6Addr>()
            .ok()
            .map(|ip| ip.to_string());
    }
    match url::Host::parse(trimmed) {
        Ok(url::Host::Domain(d)) => Some(d.to_ascii_lowercase()),
        Ok(url::Host::Ipv4(ip)) => Some(ip.to_string()),
        Ok(url::Host::Ipv6(ip)) => Some(ip.to_string()),
        Err(_) => None,
    }
}

/// The consented `net` allowlist, pre-parsed. Also used by the ui fetcher to re-validate
/// every redirect hop (a client-level reqwest policy cannot carry per-package state).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetGrants(Vec<NetGrant>);

impl NetGrants {
    /// Parse a manifest allowlist, dropping (fail-closed) entries [`NetGrant::parse`]
    /// cannot represent. The caller logs the drops once at seed time.
    #[must_use]
    pub fn parse(entries: &[String]) -> (Self, Vec<String>) {
        let mut grants = Vec::with_capacity(entries.len());
        let mut dropped = Vec::new();
        for entry in entries {
            match NetGrant::parse(entry) {
                Some(grant) => grants.push(grant),
                None => dropped.push(entry.clone()),
            }
        }
        (Self(grants), dropped)
    }

    /// Whether a parsed URL is covered. The host comes from the `Url` (never the raw
    /// string) and the port is `port_or_known_default` so a bare-`https` URL checks as 443.
    #[must_use]
    pub fn allows_url(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        // `host_str` brackets IPv6 (`[::1]`); grants store the bare form.
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let Some(port) = url.port_or_known_default() else {
            return false;
        };
        self.0.iter().any(|g| g.covers(&host, port))
    }
}

/// A package identity legitimately hosted by this isolate — the membership set that stops a
/// co-resident module forging another package's `__creator` to read its assets. Owner/name
/// are ASCII-folded (the `PackageKey::folded` convention); version is exact.
pub type PackageIdentity = (String, String, String);

/// The engine-updatable half of an [`ImageSourcePolicy`]: which package identities this
/// isolate legitimately hosts, and which of them are local dev-overrides (their assets read
/// from `<server>/packages/<name>/…` on disk, not the published blob cache).
///
/// A shared handle rather than a snapshot because packages resolve **after** the isolate's
/// extensions are built: main's trusted packages and a sandbox's dependency closure load
/// inside `load_modules`, where the package provider inserts each identity as it resolves —
/// before any module (and thus any `makeWidgets` registration) evaluates. Clones share the
/// underlying sets; reads are lock-free (`ArcSwap` loads), inserts are rare COW swaps (once
/// per package load per isolate).
#[derive(Debug, Clone, Default)]
pub struct HostedPackages {
    identities: Arc<ArcSwap<HashSet<PackageIdentity>>>,
    /// Folded `(owner, name)` pairs that are local dev-overrides for this server.
    local: Arc<ArcSwap<HashSet<(String, String)>>>,
}

impl HostedPackages {
    /// Record that this isolate hosts `owner/name@version` (folding owner/name; version
    /// exact). `local_override` marks a local dev-override package.
    pub fn insert(&self, owner: &str, name: &str, version: &str, local_override: bool) {
        let identity = (
            owner.to_ascii_lowercase(),
            name.to_ascii_lowercase(),
            version.to_string(),
        );
        if self.identities.load().contains(&identity)
            && (!local_override
                || self
                    .local
                    .load()
                    .contains(&(identity.0.clone(), identity.1.clone())))
        {
            return;
        }
        self.identities.rcu(|set| {
            let mut set = HashSet::clone(set);
            set.insert(identity.clone());
            set
        });
        if local_override {
            let key = (identity.0, identity.1);
            self.local.rcu(|set| {
                let mut set = HashSet::clone(set);
                set.insert(key.clone());
                set
            });
        }
    }

    /// Membership check for creator registration (lock-free load + hash lookup).
    #[must_use]
    pub fn contains(&self, identity: &PackageIdentity) -> bool {
        self.identities.load().contains(identity)
    }

    /// Whether `owner/name` (any case) is a local dev-override on this server.
    #[must_use]
    pub fn is_local_override(&self, owner: &str, name: &str) -> bool {
        self.local
            .load()
            .contains(&(owner.to_ascii_lowercase(), name.to_ascii_lowercase()))
    }
}

/// Per-isolate image-source policy, seeded into `OpState` by `core` beside
/// [`crate::WidgetsEnabled`]. Plain data: everything here is resolved once at isolate
/// build (grants parsed, paths `$DATA`-expanded) so per-src checks stay allocation-light.
#[derive(Debug, Clone)]
pub struct ImageSourcePolicy {
    /// Main/trusted isolate: user scripts + trusted packages. Trusted may load any host and
    /// any absolute path; `{kind:"user"}` / `{kind:"module"}` creators pass categorically.
    pub trusted: bool,
    /// The hosting server's name (per-server HTTP-cache namespace attribution, plan D10).
    pub server_name: Arc<str>,
    /// Folded package identities this isolate legitimately hosts (root + dependency
    /// closure for a sandbox; every trusted package for main), plus which are local
    /// dev-overrides. A live shared handle — see [`HostedPackages`].
    pub hosted_packages: HostedPackages,
    /// Consented `net` grants (empty for main — `trusted` short-circuits).
    pub net_grants: NetGrants,
    /// Consented `read` roots, `$DATA`-expanded — a sandboxed absolute-path src must fall
    /// under one. Empty for main (`trusted` short-circuits).
    pub read_grants: Vec<PathBuf>,
    /// `<server>/modules/` — the root `@/` addresses for non-package creators, and the
    /// module dir for creators with no module (inline aliases/triggers, REPL, https
    /// modules, which all coarsen to `{kind:"user"}`).
    pub modules_root: PathBuf,
    /// `<server>/packages/` — where local dev-override package assets live on disk
    /// (`<packages_root>/<name>/<subpath>`). The fetcher confines reads to the package's
    /// directory post-canonicalization.
    pub packages_root: PathBuf,
}

impl ImageSourcePolicy {
    /// Whether a sandboxed absolute path is covered by a consented `read` grant
    /// (component-boundary prefix, mirroring deno's path descriptors). Lexical only — the
    /// fetcher re-checks post-canonicalization.
    #[must_use]
    pub fn read_grant_covers(&self, path: &Path) -> bool {
        self.read_grants.iter().any(|root| path.starts_with(root))
    }
}

// ---------------------------------------------------------------------------------------------
// Registered creators

/// What a creator's `//`-free world resolves against, produced by the one-time
/// registration op after membership validation. `module_dir` is where *relative* srcs
/// resolve; the root is where `@/` resolves.
#[derive(Debug, Clone, PartialEq)]
pub enum CreatorRoot {
    /// A package identity. `module_dir` is the in-package directory of the importing module
    /// (`""` for the entry / package root), already component-validated. `verified` is whether
    /// the identity is in the isolate's membership set — gates **only** `PackageAsset`
    /// resolution (`@/` and relative-in-package srcs), so a forged package `__creator` cannot
    /// read another package's assets, while `http(s)`/`data:`/absolute srcs (which don't
    /// select an asset root) stay usable.
    Package {
        owner: String,
        name: String,
        version: String,
        module_dir: String,
        verified: bool,
    },
    /// A trusted filesystem world (user modules / inline): `@/` is `modules_root`,
    /// relative srcs resolve against `module_dir` (the defining module's directory, or
    /// `modules_root` itself for creators with no module).
    Modules {
        modules_root: PathBuf,
        module_dir: PathBuf,
    },
}

/// A creator registered for image-src resolution: its root plus the policy it was
/// validated against. Handed to [`resolve_src`] per call; minted once per synthesized
/// `smudgy:widgets` instance by the registration op.
#[derive(Debug, Clone)]
pub struct RegisteredImageCreator {
    pub root: CreatorRoot,
    pub policy: Arc<ImageSourcePolicy>,
}

/// Validate a JS-supplied creator descriptor against a policy and build its
/// [`RegisteredImageCreator`], or `None` if the descriptor is forged / not legitimately
/// hosted by this isolate (the caller returns a denied token; build ops then degrade to the
/// broken state). This is the membership gate: a co-resident module cannot forge another
/// package's `__creator` to reach its assets.
///
/// `creator_json` is the descriptor `smudgy:widgets` bakes per importer
/// (`{"kind":"package","owner","name","version"}` / `{"kind":"module","referrer"}` /
/// `{"kind":"user"}`). `module_subpath` is the importing module's in-package path (the
/// `?mod=` value) for packages — used as the module-relative base; ignored for others.
///
/// Membership (plan D2): on a trusted isolate, `user`/`module` creators pass categorically
/// and a `package` creator must be a trusted package (in `package_identities`); a sandboxed
/// isolate hosts only packages, so `user`/`module` are rejected outright and `package` must
/// be in the closure identity set.
#[must_use]
pub fn register_creator(
    creator_json: &str,
    module_subpath: Option<&str>,
    policy: Arc<ImageSourcePolicy>,
) -> Option<RegisteredImageCreator> {
    let value: serde_json::Value = serde_json::from_str(creator_json).ok()?;
    let kind = value.get("kind")?.as_str()?;
    match kind {
        "package" => {
            let owner = value.get("owner")?.as_str()?;
            let name = value.get("name")?.as_str()?;
            let version = value.get("version")?.as_str()?;
            let identity = (
                owner.to_ascii_lowercase(),
                name.to_ascii_lowercase(),
                version.to_string(),
            );
            // Membership gates asset reads, not the whole creator: a non-member (forged, or a
            // not-yet-enumerated legit) package can still use http(s)/data:/absolute srcs, but
            // its `@/`/relative asset lookups are denied.
            let verified = policy.hosted_packages.contains(&identity);
            let module_dir = match module_subpath.map(dir_of_validated).transpose() {
                Ok(dir) => dir.unwrap_or_default(),
                // A hostile `?mod=` (containing `..`) invalidates the base entirely.
                Err(_) => return None,
            };
            Some(RegisteredImageCreator {
                root: CreatorRoot::Package {
                    owner: owner.to_string(),
                    name: name.to_string(),
                    version: version.to_string(),
                    module_dir,
                    verified,
                },
                policy,
            })
        }
        "module" => {
            // A user file module: trusted isolates only. Its module dir is the referrer's
            // parent directory; a non-`file://` referrer coarsens to the modules root.
            if !policy.trusted {
                return None;
            }
            let module_dir = value
                .get("referrer")
                .and_then(|r| r.as_str())
                .and_then(module_dir_from_referrer)
                .unwrap_or_else(|| policy.modules_root.clone());
            let modules_root = policy.modules_root.clone();
            Some(RegisteredImageCreator {
                root: CreatorRoot::Modules {
                    modules_root,
                    module_dir,
                },
                policy,
            })
        }
        "user" => {
            // Inline aliases/triggers, REPL, jsx-runtime: trusted only, no defining module,
            // so relative srcs resolve against the modules root.
            if !policy.trusted {
                return None;
            }
            let modules_root = policy.modules_root.clone();
            Some(RegisteredImageCreator {
                root: CreatorRoot::Modules {
                    module_dir: modules_root.clone(),
                    modules_root,
                },
                policy,
            })
        }
        _ => None,
    }
}

/// The parent directory (as a validated in-package subpath) of a package module path like
/// `lib/hud.tsx` → `lib`. Every component is strictly validated (the `?mod=` value is
/// JS-supplied); `..` is rejected outright — a forged base still cannot escape the root.
fn dir_of_validated(module_subpath: &str) -> Result<String, SrcError> {
    let mut parts: Vec<&str> = module_subpath
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    parts.pop(); // drop the filename
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        if part == "." {
            continue;
        }
        if part == ".." {
            return Err(SrcError::ParentEscape);
        }
        validate_component(part)?;
        out.push(part);
    }
    Ok(out.join("/"))
}

/// The parent directory of a `file://` referrer URL. `None` for a non-`file` scheme (which
/// coarsens the creator to the modules root).
fn module_dir_from_referrer(referrer: &str) -> Option<PathBuf> {
    let url = Url::parse(referrer).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    let path = url.to_file_path().ok()?;
    path.parent().map(Path::to_path_buf)
}

// ---------------------------------------------------------------------------------------------
// Resolved sources

/// A normalized, policy-checked image source. `PartialEq`/`Hash` via [`Self::cache_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedImageSource {
    /// An asset inside a (membership-validated) package: fetched via the package
    /// provider's byte side-channel, cached content-addressed forever.
    PackageAsset {
        owner: String,
        name: String,
        version: String,
        subpath: String,
    },
    /// An on-disk file (trusted, or read-grant covered). The fetcher canonicalizes and
    /// re-checks confinement before reading.
    LocalFile(PathBuf),
    /// A remote image, HTTP-cached per RFC-9111 subset. Sandboxed loads re-validate every
    /// redirect hop against the policy's [`NetGrants`].
    Remote(Url),
    /// An inline `data:image/...` URI (≤ [`MAX_DATA_URI_BYTES`]), decoded in the fetcher.
    /// The SHA-256 of the full URI is computed once at resolve time: [`cache_key`] must be
    /// callable per frame (the hot path re-`ensure`s), and hashing megabytes there is the
    /// exact cost the resolve memo exists to avoid.
    ///
    /// [`cache_key`]: ResolvedImageSource::cache_key
    Data { uri: Arc<str>, digest: [u8; 32] },
}

impl ResolvedImageSource {
    /// The **store-map** key for this source under `policy` — [`cache_key`] plus the one
    /// policy-dependent case: a package asset whose package is a **local dev-override**
    /// keys per server and working folder (`pkg-local://<server>/<name>/<subpath>`),
    /// because its bytes come from that server's working tree, not the published
    /// content-addressed blob. Without the split, a published install and the author's
    /// live folder (or two servers' folders) collide on one process-global entry and pin
    /// each other's bytes. Cold-path only (resolve memos cache the result): one lock-free
    /// set load + a folded lookup.
    ///
    /// [`cache_key`]: Self::cache_key
    #[must_use]
    pub fn store_key(&self, policy: &ImageSourcePolicy) -> String {
        if let Self::PackageAsset {
            owner,
            name,
            subpath,
            ..
        } = self
            && policy.hosted_packages.is_local_override(owner, name)
        {
            return format!(
                "pkg-local://{}/{}/{subpath}",
                policy.server_name,
                name.to_ascii_lowercase()
            );
        }
        self.cache_key()
    }

    /// The policy-independent store key: stable, bounded-size, content-addressed for
    /// `data:` URIs (keying a map on a 2 MiB string would make every probe hash 2 MiB).
    /// Store callers use [`store_key`](Self::store_key), which layers the one
    /// policy-dependent case on top.
    #[must_use]
    pub fn cache_key(&self) -> String {
        match self {
            Self::PackageAsset {
                owner,
                name,
                version,
                subpath,
            } => format!("pkg://{owner}/{name}@{version}/{subpath}"),
            Self::LocalFile(path) => format!("file://{}", path.display()),
            Self::Remote(url) => url.as_str().to_string(),
            Self::Data { digest, .. } => {
                let mut key = String::with_capacity(12 + 64);
                key.push_str("data:sha256:");
                for b in digest {
                    use std::fmt::Write as _;
                    let _ = write!(key, "{b:02x}");
                }
                key
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Errors

/// Why a raw src failed normalization. Rendered into the one-per-source warn and the
/// widget's `onError` reason — author-facing text, so it names the rule, not the internals.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SrcError {
    #[error("src is empty or names no file")]
    Empty,
    #[error("src component {0:?} is invalid (empty, ':', '\\', or trailing dot/space)")]
    BadComponent(String),
    #[error("'..' is not allowed here (packages and store-bound srcs are descend-only)")]
    ParentEscape,
    #[error("src escapes the creator's root")]
    RootEscape,
    #[error("data: srcs are capped at 2 MiB and must be data:image/...;base64,...")]
    BadDataUri,
    #[error("this url is not a valid http(s) address")]
    BadUrl,
    #[error("this package's 'net' permission does not cover {0}")]
    NetDenied(String),
    #[error("absolute-path srcs require the main (trusted) isolate or a 'read' grant")]
    PathDenied,
    #[error("absolute paths and '..' cannot come from store bindings")]
    BoundPathDenied,
    #[error("smudgy:// cross-package srcs are not supported yet — use '@/' for own assets")]
    CrossPackage,
    #[error("relative srcs resolve against the defining module; this creator has none ({0})")]
    NoBase(String),
}

// ---------------------------------------------------------------------------------------------
// Normalization

/// Normalize + policy-check one raw `src`. Pure and lexical (see the module docs); `bound`
/// marks a value that arrived through a store binding (descend-only, no absolute paths).
///
/// # Errors
/// [`SrcError`] naming the violated rule; callers warn once and render the broken state.
pub fn resolve_src(
    raw: &str,
    creator: &RegisteredImageCreator,
    bound: bool,
) -> Result<ResolvedImageSource, SrcError> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "." || raw == "./" {
        return Err(SrcError::Empty);
    }

    if let Some(rest) = strip_scheme(raw, "data:") {
        return resolve_data(raw, rest);
    }
    if strip_scheme(raw, "smudgy://").is_some() {
        return Err(SrcError::CrossPackage);
    }
    if strip_scheme(raw, "http://").is_some() || strip_scheme(raw, "https://").is_some() {
        return resolve_remote(raw, &creator.policy, bound);
    }
    if strip_scheme(raw, "file://").is_some() {
        // A real URL parse (unlike relative/`@/` forms, which are never URL-decoded):
        // `to_file_path` percent-decodes, maps `file:///C:/x` to a drive path on Windows,
        // accepts `file://localhost/x`, and rejects foreign hosts — a naive prefix strip
        // got all four wrong (a `file://host/path` remainder is a *relative* PathBuf that
        // would canonicalize against the process CWD in the fetcher).
        let path = Url::parse(raw)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .ok_or(SrcError::BadUrl)?;
        return resolve_absolute(&path, &creator.policy, bound);
    }
    if is_absolute_path(raw) {
        return resolve_absolute(Path::new(raw), &creator.policy, bound);
    }
    if let Some(rest) = raw.strip_prefix("@/") {
        return resolve_rooted(rest, creator, bound);
    }
    if raw.starts_with('@') {
        // `@foo` without the slash is a typo of the root marker, not a filename.
        return Err(SrcError::BadComponent(raw.to_string()));
    }
    resolve_relative(raw, creator, bound)
}

/// Case-insensitive scheme strip (URL schemes are case-insensitive; `DATA:` is `data:`).
/// Compares *bytes*: str-slicing `raw[..n]` panics when byte `n` splits a multibyte char
/// (srcs are arbitrary author/game strings — "日本語.png" must resolve, not crash). After a
/// successful match the first `n` bytes are ASCII, so the remainder slice is boundary-safe.
fn strip_scheme<'a>(raw: &'a str, scheme: &str) -> Option<&'a str> {
    let n = scheme.len();
    if raw.len() >= n && raw.as_bytes()[..n].eq_ignore_ascii_case(scheme.as_bytes()) {
        Some(&raw[n..])
    } else {
        None
    }
}

fn is_absolute_path(raw: &str) -> bool {
    if raw.starts_with('/') {
        return true;
    }
    // Windows drive-rooted forms only (`C:\x`, `C:/x`). Drive-RELATIVE `C:evil.png` is NOT
    // treated as absolute — it falls through to relative resolution, where the `:` in the
    // component rejects it (it would root-replace under `PathBuf::join`).
    let bytes = raw.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn resolve_data(raw: &str, rest: &str) -> Result<ResolvedImageSource, SrcError> {
    if raw.len() > MAX_DATA_URI_BYTES {
        return Err(SrcError::BadDataUri);
    }
    // Require an image payload marker; full syntax/base64 validation happens in the
    // fetcher's decode (which enforces pixel limits anyway).
    if !rest.to_ascii_lowercase().starts_with("image/") || !rest.contains("base64,") {
        return Err(SrcError::BadDataUri);
    }
    Ok(ResolvedImageSource::Data {
        digest: Sha256::digest(raw.as_bytes()).into(),
        uri: Arc::from(raw),
    })
}

fn resolve_remote(
    raw: &str,
    policy: &ImageSourcePolicy,
    _bound: bool,
) -> Result<ResolvedImageSource, SrcError> {
    // http(s) IS allowed for bound values (the documented GMCP-avatar delegation).
    let url = Url::parse(raw).map_err(|_| SrcError::BadUrl)?;
    if url.host_str().is_none() {
        return Err(SrcError::BadUrl);
    }
    if !policy.trusted && !policy.net_grants.allows_url(&url) {
        return Err(SrcError::NetDenied(
            url.host_str().unwrap_or_default().to_string(),
        ));
    }
    Ok(ResolvedImageSource::Remote(url))
}

fn resolve_absolute(
    path: &Path,
    policy: &ImageSourcePolicy,
    bound: bool,
) -> Result<ResolvedImageSource, SrcError> {
    if bound {
        return Err(SrcError::BoundPathDenied);
    }
    if policy.trusted || policy.read_grant_covers(path) {
        Ok(ResolvedImageSource::LocalFile(path.to_path_buf()))
    } else {
        Err(SrcError::PathDenied)
    }
}

/// `@/rest` — root-relative.
fn resolve_rooted(
    rest: &str,
    creator: &RegisteredImageCreator,
    bound: bool,
) -> Result<ResolvedImageSource, SrcError> {
    match &creator.root {
        CreatorRoot::Package {
            owner,
            name,
            version,
            verified,
            ..
        } => {
            if !verified {
                return Err(SrcError::RootEscape);
            }
            // Packages are descend-only from the root: no `..` ever.
            let parts = lexical_resolve(&[], rest, ParentRule::Deny)?;
            Ok(ResolvedImageSource::PackageAsset {
                owner: owner.clone(),
                name: name.clone(),
                version: version.clone(),
                subpath: parts.join("/"),
            })
        }
        CreatorRoot::Modules { modules_root, .. } => {
            resolve_fs(modules_root, rest, &creator.policy, bound)
        }
    }
}

/// A bare relative src — resolves against the defining module's directory.
fn resolve_relative(
    raw: &str,
    creator: &RegisteredImageCreator,
    bound: bool,
) -> Result<ResolvedImageSource, SrcError> {
    match &creator.root {
        CreatorRoot::Package {
            owner,
            name,
            version,
            module_dir,
            verified,
        } => {
            if !verified {
                return Err(SrcError::RootEscape);
            }
            let base: Vec<String> = module_dir
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            // Descend-only inside packages: `..` rejected outright, so the resolved
            // subpath cannot escape the (membership-validated) package root.
            let parts = lexical_resolve(&base, raw, ParentRule::Deny)?;
            Ok(ResolvedImageSource::PackageAsset {
                owner: owner.clone(),
                name: name.clone(),
                version: version.clone(),
                subpath: parts.join("/"),
            })
        }
        CreatorRoot::Modules { module_dir, .. } => {
            resolve_fs(module_dir, raw, &creator.policy, bound)
        }
    }
}

/// Resolve a relative/rooted src against an absolute filesystem base (trusted worlds).
/// `..` is allowed for static author-written srcs (trusted user modules are unclamped —
/// they already hold absolute-path capability) but never for bound values.
fn resolve_fs(
    base: &Path,
    rest: &str,
    policy: &ImageSourcePolicy,
    bound: bool,
) -> Result<ResolvedImageSource, SrcError> {
    let rule = if bound {
        ParentRule::DenyBound
    } else {
        ParentRule::Allow
    };
    let mut stack: Vec<String> = Vec::new();
    let mut prefix = PathBuf::new();
    for comp in base.components() {
        match comp {
            Component::Normal(part) => stack.push(part.to_string_lossy().into_owned()),
            Component::RootDir | Component::Prefix(_) => {
                prefix.push(comp.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                stack.pop();
            }
        }
    }
    let parts = lexical_resolve(&stack, rest, rule)?;
    let mut path = prefix;
    for part in parts {
        path.push(part);
    }
    // The base is trusted-world by construction; sandboxed isolates never get a
    // `Modules` root, but re-check read grants defensively for the non-trusted case.
    if !policy.trusted && !policy.read_grant_covers(&path) {
        return Err(SrcError::PathDenied);
    }
    Ok(ResolvedImageSource::LocalFile(path))
}

/// How `..` components are treated during lexical resolution.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ParentRule {
    /// Trusted static srcs: `..` pops (past the base and beyond — unclamped, an
    /// absolute-path-equivalent capability trusted isolates already hold).
    Allow,
    /// Packages: `..` is an error anywhere.
    Deny,
    /// Bound values: `..` is an error anywhere, with the bound-specific message.
    DenyBound,
}

/// Split `rest` on `/`, validate every component, and resolve `.`/`..` lexically against
/// `base` (a component stack). Never touches the filesystem, never percent-decodes.
fn lexical_resolve(base: &[String], rest: &str, rule: ParentRule) -> Result<Vec<String>, SrcError> {
    let mut stack: Vec<String> = base.to_vec();
    let mut any = false;
    for part in rest.split('/') {
        match part {
            "" => return Err(SrcError::BadComponent(String::new())),
            "." => {}
            ".." => match rule {
                ParentRule::Allow => {
                    stack.pop();
                }
                ParentRule::Deny => return Err(SrcError::ParentEscape),
                ParentRule::DenyBound => return Err(SrcError::BoundPathDenied),
            },
            _ => {
                validate_component(part)?;
                stack.push(part.to_string());
                any = true;
            }
        }
    }
    if !any && stack.len() <= base.len() {
        // `@/`, `./.`, `a/..` — resolved to a directory, not a file.
        return Err(SrcError::Empty);
    }
    if stack.is_empty() {
        return Err(SrcError::Empty);
    }
    Ok(stack)
}

/// One path component: printable, no `:` (Windows drive-relative / NTFS ADS), no `\`
/// (never a separator here), no trailing dot/space (Windows strips them, aliasing two
/// spellings to one file). `.`/`..` are handled by the caller before this.
fn validate_component(part: &str) -> Result<(), SrcError> {
    if part.contains(':') || part.contains('\\') || part.ends_with('.') || part.ends_with(' ') {
        return Err(SrcError::BadComponent(part.to_string()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Memo keys

/// A bounded-size key for per-isolate `(creator, raw_src)` resolve memos. Small srcs key by
/// the string itself; large ones (`data:` URIs) by length + a digest of the first/last 4 KiB
/// — hashing megabytes per frame is what this avoids.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SrcMemoKey {
    Inline(String),
    Large { len: usize, digest: [u8; 32] },
}

#[must_use]
pub fn memo_key(raw: &str) -> SrcMemoKey {
    if raw.len() <= MEMO_INLINE_MAX {
        SrcMemoKey::Inline(raw.to_string())
    } else {
        let bytes = raw.as_bytes();
        let mut hasher = Sha256::new();
        hasher.update(&bytes[..4096.min(bytes.len())]);
        hasher.update(&bytes[bytes.len().saturating_sub(4096)..]);
        hasher.update(bytes.len().to_le_bytes());
        SrcMemoKey::Large {
            len: raw.len(),
            digest: hasher.finalize().into(),
        }
    }
}

// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(trusted: bool, net: &[&str], read: &[&str]) -> Arc<ImageSourcePolicy> {
        policy_with_ids(trusted, net, read, HashSet::new())
    }

    fn policy_with_ids(
        trusted: bool,
        net: &[&str],
        read: &[&str],
        package_identities: HashSet<PackageIdentity>,
    ) -> Arc<ImageSourcePolicy> {
        let (net_grants, _dropped) =
            NetGrants::parse(&net.iter().map(|s| (*s).to_string()).collect::<Vec<_>>());
        let hosted = HostedPackages::default();
        for (owner, name, version) in package_identities {
            hosted.insert(&owner, &name, &version, false);
        }
        Arc::new(ImageSourcePolicy {
            trusted,
            server_name: Arc::from("testserver"),
            hosted_packages: hosted,
            net_grants,
            read_grants: read.iter().map(PathBuf::from).collect(),
            modules_root: PathBuf::from("/home/u/smudgy/testserver/modules"),
            packages_root: PathBuf::from("/home/u/smudgy/testserver/packages"),
        })
    }

    fn pkg_creator(module_dir: &str, policy: Arc<ImageSourcePolicy>) -> RegisteredImageCreator {
        RegisteredImageCreator {
            root: CreatorRoot::Package {
                owner: "wbk".into(),
                name: "mapper".into(),
                version: "1.0.0".into(),
                module_dir: module_dir.into(),
                verified: true,
            },
            policy,
        }
    }

    fn user_creator(module_dir: &str, policy: Arc<ImageSourcePolicy>) -> RegisteredImageCreator {
        let modules_root = policy.modules_root.clone();
        RegisteredImageCreator {
            root: CreatorRoot::Modules {
                modules_root,
                module_dir: PathBuf::from(module_dir),
            },
            policy,
        }
    }

    fn asset_subpath(src: ResolvedImageSource) -> String {
        match src {
            ResolvedImageSource::PackageAsset { subpath, .. } => subpath,
            other => panic!("expected a package asset, got {other:?}"),
        }
    }

    fn local_path(src: ResolvedImageSource) -> PathBuf {
        match src {
            ResolvedImageSource::LocalFile(p) => p,
            other => panic!("expected a local file, got {other:?}"),
        }
    }

    // -- grammar: relative + @/ ---------------------------------------------------------

    #[test]
    fn package_relative_resolves_module_relative() {
        let c = pkg_creator("lib", policy(false, &[], &[]));
        assert_eq!(
            asset_subpath(resolve_src("icons/hp.png", &c, false).unwrap()),
            "lib/icons/hp.png"
        );
        assert_eq!(
            asset_subpath(resolve_src("./hp.png", &c, false).unwrap()),
            "lib/hp.png"
        );
    }

    #[test]
    fn package_root_marker_resolves_from_root() {
        let c = pkg_creator("lib/deep", policy(false, &[], &[]));
        assert_eq!(
            asset_subpath(resolve_src("@/assets/logo.png", &c, false).unwrap()),
            "assets/logo.png"
        );
    }

    #[test]
    fn package_parent_components_are_rejected_everywhere() {
        let c = pkg_creator("lib", policy(false, &[], &[]));
        assert_eq!(
            resolve_src("../assets/x.png", &c, false),
            Err(SrcError::ParentEscape)
        );
        assert_eq!(
            resolve_src("@/../x.png", &c, false),
            Err(SrcError::ParentEscape)
        );
        assert_eq!(
            resolve_src("a/../b.png", &c, false),
            Err(SrcError::ParentEscape),
            "even non-escaping .. is rejected in packages"
        );
    }

    #[test]
    fn user_module_relative_and_parent_navigation() {
        let p = policy(true, &[], &[]);
        let c = user_creator("/home/u/smudgy/testserver/modules/ui", p);
        assert_eq!(
            local_path(resolve_src("icons/hp.png", &c, false).unwrap()),
            PathBuf::from("/home/u/smudgy/testserver/modules/ui/icons/hp.png")
        );
        assert_eq!(
            local_path(resolve_src("../shared/bg.png", &c, false).unwrap()),
            PathBuf::from("/home/u/smudgy/testserver/modules/shared/bg.png")
        );
        // Unclamped: escaping the modules root is permitted for trusted static srcs.
        assert_eq!(
            local_path(resolve_src("../../../elsewhere/x.png", &c, false).unwrap()),
            PathBuf::from("/home/u/smudgy/elsewhere/x.png")
        );
    }

    #[test]
    fn user_root_marker_resolves_against_modules_root() {
        let p = policy(true, &[], &[]);
        let c = user_creator("/home/u/smudgy/testserver/modules/ui", p);
        assert_eq!(
            local_path(resolve_src("@/shared/bg.png", &c, false).unwrap()),
            PathBuf::from("/home/u/smudgy/testserver/modules/shared/bg.png")
        );
    }

    // -- grammar: degenerate forms -------------------------------------------------------

    #[test]
    fn degenerate_srcs_are_rejected() {
        let c = pkg_creator("", policy(false, &[], &[]));
        for bad in [
            "", " ", ".", "./", "@/", "@", "@x", "a//b.png", "a/", "/x/../",
        ] {
            assert!(
                resolve_src(bad, &c, false).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn windows_hostile_components_are_rejected() {
        let c = pkg_creator("", policy(false, &[], &[]));
        for bad in [
            "C:evil.png",
            "logo.png:stream",
            "a\\b.png",
            "trailing./x.png",
            "x/trailing. ",
            "dot./y.png",
        ] {
            assert!(
                matches!(resolve_src(bad, &c, false), Err(SrcError::BadComponent(_))),
                "{bad:?} must be a BadComponent"
            );
        }
    }

    #[test]
    fn percent_encoding_is_not_decoded() {
        let c = pkg_creator("lib", policy(false, &[], &[]));
        // %2e%2e is a literal (weird) filename, not a traversal.
        assert_eq!(
            asset_subpath(resolve_src("%2e%2e/x.png", &c, false).unwrap()),
            "lib/%2e%2e/x.png"
        );
    }

    #[test]
    fn cross_package_srcs_are_deferred() {
        let c = pkg_creator("", policy(false, &[], &[]));
        assert_eq!(
            resolve_src("smudgy://other/pkg/x.png", &c, false),
            Err(SrcError::CrossPackage)
        );
    }

    // -- bound values ---------------------------------------------------------------------

    #[test]
    fn bound_values_are_descend_only_even_for_trusted() {
        let p = policy(true, &[], &[]);
        let c = user_creator("/home/u/smudgy/testserver/modules", p);
        assert_eq!(
            resolve_src("../x.png", &c, true),
            Err(SrcError::BoundPathDenied)
        );
        assert_eq!(
            resolve_src("/etc/passwd.png", &c, true),
            Err(SrcError::BoundPathDenied)
        );
        // A bound file URL is denied on every platform, but the gate differs: a
        // drive-less unix-style URL cannot even become a Windows path (`to_file_path`
        // fails -> BadUrl); a platform-valid URL reaches the bound-path gate.
        #[cfg(not(windows))]
        assert_eq!(
            resolve_src("file:///etc/x.png", &c, true),
            Err(SrcError::BoundPathDenied)
        );
        #[cfg(windows)]
        {
            assert_eq!(
                resolve_src("file:///etc/x.png", &c, true),
                Err(SrcError::BadUrl)
            );
            assert_eq!(
                resolve_src("file:///C:/x.png", &c, true),
                Err(SrcError::BoundPathDenied)
            );
        }
        // Descend-only relative and https stay allowed for bound values.
        assert!(resolve_src("portraits/a.png", &c, true).is_ok());
        let p2 = policy(true, &[], &[]);
        let c2 = user_creator("/x", p2);
        assert!(matches!(
            resolve_src("https://game.example/av.png", &c2, true),
            Ok(ResolvedImageSource::Remote(_))
        ));
    }

    // -- absolute paths -------------------------------------------------------------------

    #[test]
    fn absolute_paths_require_trust_or_read_grant() {
        let trusted = user_creator("/m", policy(true, &[], &[]));
        assert!(resolve_src("/anywhere/x.png", &trusted, false).is_ok());

        let granted = pkg_creator("", policy(false, &[], &["/data/pkg"]));
        assert!(resolve_src("/data/pkg/gen.png", &granted, false).is_ok());
        assert_eq!(
            resolve_src("/data/other/x.png", &granted, false),
            Err(SrcError::PathDenied)
        );
        // Component-boundary: /data/pkgX is NOT under /data/pkg.
        assert_eq!(
            resolve_src("/data/pkgX/x.png", &granted, false),
            Err(SrcError::PathDenied)
        );
    }

    // -- net grants -----------------------------------------------------------------------

    #[test]
    fn net_grants_match_deno_descriptor_semantics() {
        let (grants, dropped) = NetGrants::parse(&[
            "example.com".into(),
            "api.example.org:8443".into(),
            "*.cdn.net".into(),
            "127.0.0.1".into(),
        ]);
        assert!(dropped.is_empty());
        let ok = |u: &str| grants.allows_url(&Url::parse(u).unwrap());
        // Bare host covers any port (including defaults).
        assert!(ok("https://example.com/x.png"));
        assert!(ok("http://example.com:8080/x.png"));
        // Exact host:port covers only that port; https default is 443.
        assert!(ok("https://api.example.org:8443/i.png"));
        assert!(!ok("https://api.example.org/i.png"));
        // Wildcards cover proper subdomains only.
        assert!(ok("https://a.cdn.net/x.png"));
        assert!(ok("https://a.b.cdn.net/x.png"));
        assert!(!ok("https://cdn.net/x.png"));
        // Unrelated hosts and IPs.
        assert!(!ok("https://evil.com/x.png"));
        assert!(ok("http://127.0.0.1:9000/x.png"));
    }

    #[test]
    fn net_grants_support_any_host_with_optional_port() {
        let (any, dropped) = NetGrants::parse(&["*".into()]);
        assert!(dropped.is_empty());
        assert!(any.allows_url(&Url::parse("https://example.com/x.png").unwrap()));
        assert!(any.allows_url(&Url::parse("http://127.0.0.1:9876/x.png").unwrap()));

        let (port_scoped, dropped) = NetGrants::parse(&["*:8443".into()]);
        assert!(dropped.is_empty());
        assert!(port_scoped.allows_url(&Url::parse("https://example.com:8443/x.png").unwrap()));
        assert!(port_scoped.allows_url(&Url::parse("https://other.example:8443/x.png").unwrap()));
        assert!(!port_scoped.allows_url(&Url::parse("https://example.com/x.png").unwrap()));
        assert!(!port_scoped.allows_url(&Url::parse("http://127.0.0.1:8444/x.png").unwrap()));
    }

    #[test]
    fn net_check_uses_parsed_host_not_raw_string() {
        let c = pkg_creator("", policy(false, &["ok.com"], &[]));
        // Userinfo trick: the real host is evil.com.
        assert!(matches!(
            resolve_src("https://ok.com@evil.com/x.png", &c, false),
            Err(SrcError::NetDenied(h)) if h == "evil.com"
        ));
        // Trailing-dot and case variants of a granted host still match.
        assert!(resolve_src("https://OK.com./x.png", &c, false).is_ok());
    }

    #[test]
    fn punycode_hosts_normalize_for_matching() {
        let c = pkg_creator("", policy(false, &["bücher.example"], &[]));
        assert!(
            resolve_src("https://xn--bcher-kva.example/x.png", &c, false).is_ok(),
            "IDNA grant must cover the punycoded host"
        );
    }

    #[test]
    fn ipv6_grants_parse_and_match() {
        let (grants, dropped) = NetGrants::parse(&["[::1]:8080".into(), "::2".into()]);
        assert!(dropped.is_empty());
        assert!(grants.allows_url(&Url::parse("http://[::1]:8080/x").unwrap()));
        assert!(!grants.allows_url(&Url::parse("http://[::1]:9090/x").unwrap()));
        assert!(grants.allows_url(&Url::parse("http://[::2]:5/x").unwrap()));
    }

    #[test]
    fn unparseable_grants_drop_fail_closed() {
        let (grants, dropped) = NetGrants::parse(&["10.0.0.0/8".into(), "bad host".into()]);
        assert_eq!(
            dropped.len(),
            2,
            "CIDR + malformed are dropped, not mis-matched"
        );
        assert!(!grants.allows_url(&Url::parse("http://10.1.2.3/x").unwrap()));
    }

    #[test]
    fn trusted_skips_net_gates() {
        let c = user_creator("/m", policy(true, &[], &[]));
        assert!(resolve_src("https://anywhere.example/x.png", &c, false).is_ok());
    }

    // -- data URIs ------------------------------------------------------------------------

    #[test]
    fn data_uris_validate_shape_and_cap() {
        let c = pkg_creator("", policy(false, &[], &[]));
        assert!(resolve_src("data:image/png;base64,iVBORw0KGgo=", &c, false).is_ok());
        assert!(
            resolve_src("data:image/png;base64,AAAA", &c, true).is_ok(),
            "bound data: ok"
        );
        assert_eq!(
            resolve_src("data:text/html;base64,PGI+", &c, false),
            Err(SrcError::BadDataUri)
        );
        let huge = format!("data:image/png;base64,{}", "A".repeat(MAX_DATA_URI_BYTES));
        assert_eq!(resolve_src(&huge, &c, false), Err(SrcError::BadDataUri));
    }

    // -- creator registration / membership -----------------------------------------------

    fn ids(entries: &[(&str, &str, &str)]) -> HashSet<PackageIdentity> {
        entries
            .iter()
            .map(|(o, n, v)| ((*o).to_string(), (*n).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn package_membership_gates_asset_reads_only() {
        let identities = ids(&[("wbk", "mapper", "1.0.0")]);
        let pol = policy_with_ids(false, &["cdn.example"], &[], identities);
        let json = r#"{"kind":"package","owner":"wbk","name":"mapper","version":"1.0.0"}"#;
        let creator = register_creator(json, Some("lib/hud.tsx"), pol.clone()).unwrap();
        // A member reads its own assets (module_subpath's dir is the relative base).
        assert_eq!(
            asset_subpath(resolve_src("icon.png", &creator, false).unwrap()),
            "lib/icon.png"
        );

        // A co-resident forging a different package registers (so it can still use
        // http(s)/data:) but its ASSET reads are denied — it cannot read the forged
        // package's files.
        let forged = r#"{"kind":"package","owner":"evil","name":"pkg","version":"1.0.0"}"#;
        let forger = register_creator(forged, None, pol.clone()).unwrap();
        assert_eq!(
            resolve_src("@/secret.png", &forger, false),
            Err(SrcError::RootEscape)
        );
        assert_eq!(
            resolve_src("sibling.png", &forger, false),
            Err(SrcError::RootEscape)
        );
        // ...but a granted http(s) host still works for the forger (harmless — it could use
        // its own legit creator for that anyway).
        assert!(resolve_src("https://cdn.example/x.png", &forger, false).is_ok());
    }

    #[test]
    fn forged_module_subpath_cannot_escape_package_root() {
        let pol = policy_with_ids(false, &[], &[], ids(&[("wbk", "mapper", "1.0.0")]));
        let json = r#"{"kind":"package","owner":"wbk","name":"mapper","version":"1.0.0"}"#;
        // A hostile ?mod= with .. invalidates the creator entirely.
        assert!(register_creator(json, Some("../../etc/hud.tsx"), pol.clone()).is_none());
        // And even a legitimate deep module can only descend from its own dir.
        let creator = register_creator(json, Some("a/b/hud.tsx"), pol).unwrap();
        assert_eq!(
            resolve_src("../../../x.png", &creator, false),
            Err(SrcError::ParentEscape)
        );
    }

    #[test]
    fn user_and_module_creators_are_trusted_only() {
        let trusted = policy(true, &[], &[]);
        let sandbox = policy(false, &[], &[]);
        let user = r#"{"kind":"user"}"#;
        // Referrers must be platform-valid file URLs: a drive-less unix path fails
        // `to_file_path` on Windows and would coarsen the module dir to the modules root.
        #[cfg(not(windows))]
        let module = r#"{"kind":"module","referrer":"file:///home/u/smudgy/testserver/modules/ui/bars.tsx"}"#;
        #[cfg(windows)]
        let module = r#"{"kind":"module","referrer":"file:///C:/home/u/smudgy/testserver/modules/ui/bars.tsx"}"#;
        assert!(register_creator(user, None, trusted.clone()).is_some());
        assert!(register_creator(module, None, trusted.clone()).is_some());
        // A sandbox hosts only packages — user/module descriptors are forgery there.
        assert!(register_creator(user, None, sandbox.clone()).is_none());
        assert!(register_creator(module, None, sandbox).is_none());
        // The module's referrer dir becomes the relative base.
        let creator = register_creator(module, None, trusted).unwrap();
        #[cfg(not(windows))]
        let expected = PathBuf::from("/home/u/smudgy/testserver/modules/ui/icon.png");
        #[cfg(windows)]
        let expected = PathBuf::from("C:/home/u/smudgy/testserver/modules/ui/icon.png");
        assert_eq!(
            local_path(resolve_src("icon.png", &creator, false).unwrap()),
            expected
        );
    }

    #[test]
    fn non_file_referrer_coarsens_to_modules_root() {
        let pol = policy(true, &[], &[]);
        let https_mod = r#"{"kind":"module","referrer":"https://jsr.io/x/mod.ts"}"#;
        let creator = register_creator(https_mod, None, pol).unwrap();
        assert_eq!(
            local_path(resolve_src("icon.png", &creator, false).unwrap()),
            PathBuf::from("/home/u/smudgy/testserver/modules/icon.png")
        );
    }

    // -- keys -----------------------------------------------------------------------------

    #[test]
    fn cache_keys_are_stable_and_bounded() {
        // The digest is minted once at resolve time; cache_key only formats it (the hot
        // path calls cache_key per ensure — it must never re-hash the payload).
        let raw = format!("data:image/png;base64,{}", "B".repeat(100_000));
        let pol = policy(true, &[], &[]);
        let c = user_creator("/x", pol);
        let resolved = resolve_src(&raw, &c, false).unwrap();
        let key = resolved.cache_key();
        assert!(key.starts_with("data:sha256:") && key.len() < 100);
        assert_eq!(key, resolve_src(&raw, &c, false).unwrap().cache_key());

        let a = memo_key("short");
        assert_eq!(a, SrcMemoKey::Inline("short".into()));
        let big = "x".repeat(600);
        assert!(matches!(memo_key(&big), SrcMemoKey::Large { len: 600, .. }));
    }

    // -- hosted-package membership ----------------------------------------------------------

    #[test]
    fn hosted_packages_fold_and_share_live() {
        let hosted = HostedPackages::default();
        let policy = Arc::new(ImageSourcePolicy {
            trusted: true,
            server_name: Arc::from("testserver"),
            hosted_packages: hosted.clone(),
            net_grants: NetGrants::default(),
            read_grants: Vec::new(),
            modules_root: PathBuf::from("/m"),
            packages_root: PathBuf::from("/p"),
        });
        let creator_json = r#"{"kind":"package","owner":"WBK","name":"Mapper","version":"1.0.0"}"#;

        // Registered before the identity lands: not verified — asset srcs break.
        let before = register_creator(creator_json, None, policy.clone()).unwrap();
        assert!(matches!(
            resolve_src("@/a.png", &before, false),
            Err(SrcError::RootEscape)
        ));

        // The provider inserts as the package resolves (any case; folded), THEN modules
        // evaluate and register — the clone inside the policy sees the same live set.
        hosted.insert("wbk", "MAPPER", "1.0.0", true);
        assert!(hosted.contains(&("wbk".into(), "mapper".into(), "1.0.0".into())));
        assert!(
            !hosted.contains(&("wbk".into(), "mapper".into(), "2.0.0".into())),
            "version exact"
        );
        assert!(hosted.is_local_override("WbK", "mapper"));
        assert!(!hosted.is_local_override("wbk", "other"));

        let after = register_creator(creator_json, None, policy).unwrap();
        assert!(matches!(
            resolve_src("@/a.png", &after, false),
            Ok(ResolvedImageSource::PackageAsset { .. })
        ));
    }

    #[test]
    fn local_override_assets_key_per_server_never_by_content_identity() {
        // A local dev-override's bytes come from the server's working tree; a published
        // install's from the immutable blob. Same (owner, name, version, subpath) must NOT
        // share one process-global store entry — the author's edits would stop repainting
        // (published entry has no file stamp) or another server would display this one's
        // working-tree file.
        let source = ResolvedImageSource::PackageAsset {
            owner: "WBK".into(),
            name: "Hud".into(),
            version: "1.0.0".into(),
            subpath: "assets/logo.png".into(),
        };
        let published = policy(true, &[], &[]);
        assert_eq!(source.store_key(&published), source.cache_key());

        let local = policy(true, &[], &[]);
        local.hosted_packages.insert("wbk", "hud", "1.0.0", true);
        let key = source.store_key(&local);
        assert_eq!(key, "pkg-local://testserver/hud/assets/logo.png");
        assert_ne!(key, source.cache_key());
    }

    // -- panic safety ---------------------------------------------------------------------

    #[test]
    fn multibyte_srcs_never_panic() {
        // strip_scheme byte-compares: a multibyte char straddling a scheme-length byte
        // offset (5 for "data:", 7/8 for http(s), 9 for smudgy://) must resolve or reject
        // normally, never panic on a str-slice boundary. Bound and static alike.
        let pol = policy(true, &[], &[]);
        let c = user_creator("/x", pol);
        for raw in [
            "日本語.png",       // 3-byte chars from byte 0
            "dat€.png",         // € straddles byte 5 ("data:" length)
            "aaaaaaé.png",      // é straddles byte 7 ("http://" length)
            "assets/ölogo.png", // ö straddles byte 8 ("https://" length)
            "smudgyö//x.png",   // ö straddles byte 9 ("smudgy://" length)
            "é",
        ] {
            let _ = resolve_src(raw, &c, false);
            let _ = resolve_src(raw, &c, true);
        }
        // And a multibyte name still resolves as a normal relative file.
        assert!(matches!(
            resolve_src("画像.png", &c, false),
            Ok(ResolvedImageSource::LocalFile(_))
        ));
    }

    // -- file:// URLs ---------------------------------------------------------------------

    #[test]
    #[cfg(not(windows))]
    fn file_urls_parse_as_urls() {
        let pol = policy(true, &[], &[]);
        let c = user_creator("/x", pol);
        // Plain absolute form.
        assert_eq!(
            local_path(resolve_src("file:///etc/x.png", &c, false).unwrap()),
            PathBuf::from("/etc/x.png")
        );
        // `localhost` is the local host; percent-encoding decodes (file URLs are real
        // URLs, unlike relative/`@/` forms which are never decoded).
        assert_eq!(
            local_path(resolve_src("file://localhost/tmp/a%20b.png", &c, false).unwrap()),
            PathBuf::from("/tmp/a b.png")
        );
        // A foreign host cannot name a local path.
        assert_eq!(
            resolve_src("file://fileserver/share/x.png", &c, false),
            Err(SrcError::BadUrl)
        );
    }
}
