//! Persistent, content-addressed cache for fetched `smudgy://` packages.
//!
//! Published versions are immutable (a `name@1.4.0`'s content never changes), so once
//! fetched they can be cached **permanently** — like deno's module cache or npm's. This
//! caches two things under `<smudgy_home>/cache/packages/`:
//!
//! - **blobs** (`blobs/<hash[0:2]>/<hash[2:4]>/<hash>`): module bodies keyed by their
//!   SHA-256, so the provider only re-downloads bodies it doesn't already have, and
//!   identical bodies dedupe across packages/versions.
//! - **metadata** (`meta/<owner>/<name>/<version>.json`): the manifest + module
//!   list for a concrete version, so a *pinned* package resolves fully offline.
//!
//! Bodies are written only after the provider verified their hash on fetch, and the
//! cache is content-addressed, so reads are trusted without re-hashing.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use smudgy_cloud::{ResolvedDependency, package_api::ResolvedModuleWire};
pub use smudgy_script::PackageKey;
use smudgy_script::PackageManifest;

use crate::get_smudgy_home;
use crate::models::persistence::write_atomic;

/// A cached resolution of a concrete package version (no presigned URLs — those are
/// ephemeral; bodies live in the blob cache, keyed by `content_hash`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedResolution {
    pub version: String,
    pub integrity: String,
    pub manifest: PackageManifest,
    pub modules: Vec<CachedModule>,
    /// The version's locked `smudgy://` deps, so an offline load can repopulate
    /// referrer-aware version selection. `default` keeps older cache files readable.
    #[serde(default)]
    pub dependencies: Vec<ResolvedDependency>,
}

/// One module's metadata within a [`CachedResolution`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedModule {
    pub subpath: String,
    pub content_hash: String,
    /// The published media type. `default` keeps pre-field cache files readable — they
    /// deserialize as `text/plain`, i.e. code, which matches how they were treated when
    /// written. Distinguishes code (eagerly loaded into the module graph) from assets
    /// (lazily fetched by the image side-channel).
    #[serde(default = "default_cached_media_type")]
    pub media_type: String,
}

fn default_cached_media_type() -> String {
    "text/plain".to_string()
}

/// Whether a module belongs in the module graph (code/text, eagerly loaded as UTF-8) as
/// opposed to a binary asset (image and friends — lazily byte-fetched via the image
/// side-channel, never fed to V8).
///
/// `application/octet-stream` is the publish-side fallback for any unmapped extension, so
/// it is ambiguous: pre-PR2, every octet-stream module that *worked* was UTF-8 code with
/// an exotic extension (binaries failed the whole package load), so octet-stream stays
/// code unless the subpath's extension is unmistakably binary media — those become lazy
/// assets. A misclassified-as-code binary fails exactly as it always did (a per-module
/// UTF-8 error); misclassified-as-asset code merely fails its import with a clear message.
#[must_use]
pub fn is_code_module(media_type: &str, subpath: &str) -> bool {
    if media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/typescript" | "application/javascript" | "application/json"
        )
    {
        return true;
    }
    if media_type == "application/octet-stream" {
        return !has_binary_media_extension(subpath);
    }
    false
}

/// Extensions that are unmistakably binary media (never importable code), for
/// classifying the ambiguous `application/octet-stream` publish fallback.
fn has_binary_media_extension(subpath: &str) -> bool {
    let ext = std::path::Path::new(subpath)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "avif"
            | "bmp"
            | "ico"
            | "tiff"
            | "tif"
            | "svg"
            | "svgz"
            | "mp3"
            | "ogg"
            | "wav"
            | "flac"
            | "mp4"
            | "webm"
            | "mkv"
            | "avi"
            | "mov"
            | "woff"
            | "woff2"
            | "ttf"
            | "otf"
            | "eot"
            | "zip"
            | "gz"
            | "tar"
            | "pdf"
    )
}

/// A deterministic package-level integrity fingerprint over the per-module content
/// hashes (each is already a SHA-256). Detects any module change for the lockfile.
/// One recipe for every writer — the engine's provider and the background update
/// checker stamp identical metas.
#[must_use]
pub fn package_integrity(modules: &[ResolvedModuleWire]) -> String {
    let mut entries: Vec<String> = modules
        .iter()
        .map(|module| format!("{}={}", module.subpath, module.content_hash))
        .collect();
    entries.sort();
    entries.join(";")
}

/// Disk cache rooted at `<smudgy_home>/cache/packages/`.
#[derive(Debug, Clone)]
pub struct PackageCache {
    root: PathBuf,
}

impl PackageCache {
    /// Open (or locate) the cache under the smudgy home directory.
    ///
    /// # Errors
    /// Returns an error if the smudgy home directory cannot be determined.
    pub fn new() -> Result<Self> {
        let root = get_smudgy_home()
            .context("locate smudgy home for package cache")?
            .join("cache")
            .join("packages");
        Ok(Self { root })
    }

    /// A cache rooted at an explicit directory, for callers that must not touch the
    /// process-global smudgy home (tests, tooling).
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    fn blob_path(&self, content_hash: &str) -> PathBuf {
        let a = content_hash.get(0..2).unwrap_or("00");
        let b = content_hash.get(2..4).unwrap_or("00");
        self.root.join("blobs").join(a).join(b).join(content_hash)
    }

    fn meta_path(&self, key: &PackageKey, version: &str) -> PathBuf {
        self.root
            .join("meta")
            .join(&key.owner)
            .join(&key.name)
            .join(format!("{version}.json"))
    }

    /// Whether a module body is already cached — presence is truth (bodies are
    /// verified before they are written), so writers use this to skip a fetch
    /// without reading the file.
    #[must_use]
    pub fn has_blob(&self, content_hash: &str) -> bool {
        self.blob_path(content_hash).exists()
    }

    /// A cached module body, if present (content-addressed; trusted without re-hashing).
    #[must_use]
    pub fn read_blob(&self, content_hash: &str) -> Option<String> {
        fs::read_to_string(self.blob_path(content_hash)).ok()
    }

    /// Byte twin of [`read_blob`](Self::read_blob), for binary (asset) bodies.
    #[must_use]
    pub fn read_blob_bytes(&self, content_hash: &str) -> Option<Vec<u8>> {
        fs::read(self.blob_path(content_hash)).ok()
    }

    /// Store a module body under its content hash. Best-effort; a write failure is not
    /// fatal (the provider keeps the in-memory copy and just re-downloads next time).
    ///
    /// # Errors
    /// Returns an error if the cache directory cannot be created or the file written.
    pub fn write_blob(&self, content_hash: &str, body: &str) -> Result<()> {
        self.write_blob_bytes(content_hash, body.as_bytes())
    }

    /// Byte twin of [`write_blob`](Self::write_blob), for binary (asset) bodies.
    ///
    /// The write is atomic (temp sibling + rename): presence-gating trusts a blob file
    /// forever, so a crash or full disk mid-write must never leave a torn body at a
    /// content-addressed path — and a completed write replaces any torn file a previous
    /// interrupted attempt left behind.
    ///
    /// # Errors
    /// Returns an error if the cache directory cannot be created or the file written.
    pub fn write_blob_bytes(&self, content_hash: &str, body: &[u8]) -> Result<()> {
        let path = self.blob_path(content_hash);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create blob cache dir {}", parent.display()))?;
        }
        write_atomic(&path, body).with_context(|| format!("write blob {}", path.display()))
    }

    /// Whether resolution metadata for a concrete version is already cached. Versions
    /// are immutable, so presence means the write-once entry is final — writers use
    /// this to skip a redundant serialize + write.
    #[must_use]
    pub fn has_meta(&self, key: &PackageKey, version: &str) -> bool {
        self.meta_path(key, version).exists()
    }

    /// The cached resolution metadata for a concrete version, if present.
    #[must_use]
    pub fn read_meta(&self, key: &PackageKey, version: &str) -> Option<CachedResolution> {
        let content = fs::read_to_string(self.meta_path(key, version)).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Whether every **code** module body for a cached resolution is present (so the
    /// module graph can be served fully offline). Asset bodies (images) are lazy — they
    /// were possibly never downloaded, and must not hold the whole package hostage.
    #[must_use]
    pub fn has_all_code_blobs(&self, resolution: &CachedResolution) -> bool {
        resolution
            .modules
            .iter()
            .filter(|m| is_code_module(&m.media_type, &m.subpath))
            .all(|m| self.blob_path(&m.content_hash).exists())
    }

    /// Persist resolution metadata for a concrete version (immutable, so write-once).
    ///
    /// The write is atomic (temp sibling + rename), so readers never see partial JSON
    /// and a completed write replaces any torn file an interrupted attempt left behind.
    ///
    /// # Errors
    /// Returns an error if the cache directory cannot be created or the file written.
    pub fn write_meta(
        &self,
        key: &PackageKey,
        version: &str,
        resolution: &CachedResolution,
    ) -> Result<()> {
        let path = self.meta_path(key, version);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create meta cache dir {}", parent.display()))?;
        }
        let json = serde_json::to_string(resolution).context("serialize cached resolution")?;
        write_atomic(&path, json.as_bytes())
            .with_context(|| format!("write meta {}", path.display()))
    }

    /// Persist resolution metadata unless an equally-complete copy is already cached —
    /// the self-healing form of [`write_meta`](Self::write_meta). Versions are
    /// immutable, so the common case skips a redundant serialize + write; but a cache
    /// file written before `CachedResolution` grew a field deserializes with that field
    /// defaulted (a pre-`dependencies` file folds to a root-only closure; a
    /// pre-`media_type` file misclassifies modules), and freezing it forever would
    /// silently under-report the version's true facts. So a fresh resolution that
    /// carries dependency edges or modules the cached copy lacks — or finds the cached
    /// file unreadable (torn, corrupt) — overwrites it instead of skipping.
    ///
    /// # Errors
    /// Returns an error if the cache directory cannot be created or the file written.
    pub fn refresh_meta(
        &self,
        key: &PackageKey,
        version: &str,
        resolution: &CachedResolution,
    ) -> Result<()> {
        if let Some(cached) = self.read_meta(key, version) {
            let missing_deps =
                cached.dependencies.is_empty() && !resolution.dependencies.is_empty();
            let missing_modules = cached.modules.len() < resolution.modules.len();
            if !missing_deps && !missing_modules {
                return Ok(());
            }
        }
        self.write_meta(key, version, resolution)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_in(dir: &std::path::Path) -> PackageCache {
        PackageCache {
            root: dir.join("cache").join("packages"),
        }
    }

    fn key() -> PackageKey {
        PackageKey {
            owner: "wbk".into(),
            name: "mapper".into(),
        }
    }

    #[test]
    fn blob_round_trips_and_dedupes_by_hash() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(dir.path());
        assert!(cache.read_blob("abc123").is_none());
        cache.write_blob("abc123", "export const x = 1;").unwrap();
        assert_eq!(
            cache.read_blob("abc123").as_deref(),
            Some("export const x = 1;")
        );
        // Byte twins share the same content-addressed pool.
        assert_eq!(
            cache.read_blob_bytes("abc123").as_deref(),
            Some(b"export const x = 1;".as_slice())
        );
        cache
            .write_blob_bytes("bin1", &[0u8, 159, 146, 150])
            .unwrap();
        assert_eq!(
            cache.read_blob_bytes("bin1").as_deref(),
            Some([0u8, 159, 146, 150].as_slice())
        );
        // A binary blob is not silently lossy-read as a string.
        assert!(cache.read_blob("bin1").is_none());
    }

    #[test]
    fn meta_round_trips_and_offline_readiness() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(dir.path());
        let resolution = CachedResolution {
            version: "1.4.0".into(),
            integrity: "sum".into(),
            manifest: PackageManifest::parse(r#"{ "name": "mapper", "version": "1.4.0" }"#)
                .unwrap(),
            modules: vec![
                CachedModule {
                    subpath: "index.ts".into(),
                    content_hash: "deadbeef".into(),
                    media_type: "application/typescript".into(),
                },
                // An asset module: lazily fetched, so its blob must NOT gate offline use.
                CachedModule {
                    subpath: "assets/logo.png".into(),
                    content_hash: "feedface".into(),
                    media_type: "image/png".into(),
                },
            ],
            dependencies: Vec::new(),
        };
        assert!(cache.read_meta(&key(), "1.4.0").is_none());
        cache.write_meta(&key(), "1.4.0", &resolution).unwrap();
        let loaded = cache.read_meta(&key(), "1.4.0").expect("meta round-trips");
        assert_eq!(loaded.version, "1.4.0");

        // Not offline-ready until the CODE body is cached; the asset never gates.
        assert!(!cache.has_all_code_blobs(&loaded));
        cache.write_blob("deadbeef", "export const x = 1;").unwrap();
        assert!(cache.has_all_code_blobs(&loaded));
    }

    #[test]
    fn a_fresh_write_replaces_a_torn_file() {
        // Presence-gating trusts cached files forever, so the atomic writers must be
        // able to REPLACE a torn file left by an interrupted write — the old
        // write-once early return would have trusted the garbage indefinitely.
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(dir.path());
        let resolution = CachedResolution {
            version: "1.4.0".into(),
            integrity: "sum".into(),
            manifest: PackageManifest::parse(r#"{ "name": "mapper", "version": "1.4.0" }"#)
                .unwrap(),
            modules: Vec::new(),
            dependencies: Vec::new(),
        };
        // Plant torn files at the final content-addressed paths.
        let meta_path = cache.meta_path(&key(), "1.4.0");
        fs::create_dir_all(meta_path.parent().unwrap()).unwrap();
        fs::write(&meta_path, r#"{"version":"1.4"#).unwrap();
        assert!(
            cache.read_meta(&key(), "1.4.0").is_none(),
            "torn JSON is unreadable"
        );
        let blob_path = cache.blob_path("abc123");
        fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
        fs::write(&blob_path, "export const trunc").unwrap();

        // A completed write (rename over the final path) heals both.
        cache.refresh_meta(&key(), "1.4.0", &resolution).unwrap();
        assert_eq!(
            cache.read_meta(&key(), "1.4.0").map(|m| m.version),
            Some("1.4.0".to_string())
        );
        cache.write_blob("abc123", "export const x = 1;").unwrap();
        assert_eq!(
            cache.read_blob("abc123").as_deref(),
            Some("export const x = 1;")
        );
    }

    #[test]
    fn refresh_meta_heals_a_legacy_deps_less_file_but_never_regresses_one() {
        // A cache file written before the `dependencies` field deserializes with an
        // empty dep list; folding it yields a root-only closure union. A fresh
        // resolution carrying the version's true edges must overwrite it.
        let dir = tempfile::tempdir().unwrap();
        let cache = cache_in(dir.path());
        let manifest =
            PackageManifest::parse(r#"{ "name": "mapper", "version": "1.4.0" }"#).unwrap();
        let legacy = CachedResolution {
            version: "1.4.0".into(),
            integrity: "sum".into(),
            manifest: manifest.clone(),
            modules: Vec::new(),
            dependencies: Vec::new(),
        };
        cache.write_meta(&key(), "1.4.0", &legacy).unwrap();
        let full = CachedResolution {
            dependencies: vec![ResolvedDependency {
                owner_nickname: "wbk".into(),
                name: "base".into(),
                range: "^1".into(),
                resolved_version: "1.0.0".into(),
            }],
            ..legacy.clone()
        };
        cache.refresh_meta(&key(), "1.4.0", &full).unwrap();
        assert_eq!(
            cache
                .read_meta(&key(), "1.4.0")
                .map(|m| m.dependencies.len()),
            Some(1),
            "the legacy file was healed with the wire's dependency edges"
        );
        // The reverse never regresses: a deps-less resolution (a degraded wire)
        // must not clobber the healed copy.
        cache.refresh_meta(&key(), "1.4.0", &legacy).unwrap();
        assert_eq!(
            cache
                .read_meta(&key(), "1.4.0")
                .map(|m| m.dependencies.len()),
            Some(1),
            "an equally-or-less-complete resolution leaves the cached copy alone"
        );
    }

    #[test]
    fn media_type_classification() {
        for (code, subpath) in [
            ("application/typescript", "index.ts"),
            ("application/javascript", "lib/x.js"),
            ("application/json", "data.json"),
            ("text/plain", "shader.wgsl"),
            ("text/css", "style.css"),
            // The publish fallback for an unmapped extension: UTF-8 code pre-PR2 —
            // must keep loading as code (only unmistakably-binary extensions are lazy).
            ("application/octet-stream", "lib/helper.cts"),
            ("application/octet-stream", "LICENSE"),
        ] {
            assert!(
                is_code_module(code, subpath),
                "{code} {subpath} should be code"
            );
        }
        for (asset, subpath) in [
            ("image/png", "assets/logo.png"),
            ("image/svg+xml", "assets/icon.svg"),
            ("application/octet-stream", "assets/photo.avif"),
            ("application/octet-stream", "sounds/hit.ogg"),
            ("application/wasm", "lib/fast.wasm"),
            ("audio/ogg", "sounds/hit.ogg"),
            ("font/woff2", "fonts/ui.woff2"),
        ] {
            assert!(
                !is_code_module(asset, subpath),
                "{asset} {subpath} should be an asset"
            );
        }
        // Pre-field cache files deserialize to text/plain, i.e. code — matching how they
        // were treated when written.
        let old: CachedModule =
            serde_json::from_str(r#"{"subpath":"index.ts","content_hash":"aa"}"#).unwrap();
        assert!(is_code_module(&old.media_type, &old.subpath));
    }
}
