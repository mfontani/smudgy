//! The process-global image store: resolved sources → decoded iced image handles.
//!
//! One store serves every session (entries are keyed by *content source*, not by creator or
//! session, so sessions share fetches, decodes, and GPU uploads). It is `Send + Sync` and
//! deliberately NOT a UI-thread-local store like `MapStore`/`TextEditorStore`: a canvas
//! `SceneProgram` is built on the script thread, loader tasks live on the store's own tokio
//! runtime, and render closures read on the UI thread.
//!
//! ## Hot-path contract (plan D3)
//!
//! `createWidget()` runs every frame in some scripts, so the steady-state read path takes
//! **no locks, makes no syscalls, and does no parsing**: the map is an
//! [`ArcSwap`]`<HashMap<..>>` (lock-free load; copy-on-write swaps under a small writer
//! mutex, exactly the [`crate::WidgetRoot`] idiom) and each entry is an [`ImageEntryCell`]
//! — an `ArcSwap<EntryState>` in the `StoreBindingCell` shape. Build ops call
//! [`ImageStore::ensure`] once per (memoized) source and capture the returned `Arc` cell;
//! per-frame renders do one `cell.load()`.
//!
//! LRU recency is a relaxed atomic stamp touched on load, so reads never write shared
//! state; eviction sweeps run on the store runtime and, because a displayed image's stamp
//! is hot by construction, can only ever evict entries nothing is drawing.
//!
//! Handles are minted HERE and only here: `Handle::from_rgba` mints a unique id per call,
//! so a per-frame mint would re-upload the texture every frame.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime};

use arc_swap::ArcSwap;
use smudgy_cloud::image_source::{ImageSourcePolicy, ResolvedImageSource};
use tokio::sync::watch;

/// In-memory decoded-bytes cap (a **CPU-side** bound — GPU residency is `iced_wgpu`'s
/// hit-based atlas trim). The user-facing `image_cache_max_mb` setting bounds the *disk*
/// cache instead; this constant is deliberately not configurable.
const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;

/// How long a *transient*-failed entry stays sticky before an `ensure` may retry it.
/// Permanent failures (policy denials, decode errors — [`FetchError::transient`] false)
/// never retry: the input can't get better on its own.
const FAILED_RETRY_TTL: Duration = Duration::from_secs(30);

/// Minimum interval between freshness re-stats of a local-file entry. The re-stat itself
/// runs on the store runtime — reads only compare this stamp (no frame-thread syscalls).
const LOCAL_RECHECK_INTERVAL_MS: u64 = 1_000;

type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// mtime + size of an on-disk source at decode time, for cheap staleness probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub mtime: SystemTime,
    pub size: u64,
}

/// What a fetch produces: straight (non-premultiplied) RGBA8, already EXIF-oriented and
/// limit-checked by the fetcher. The store mints the iced handle from this.
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Present for local-file-backed sources; drives the freshness re-probe.
    pub file_stamp: Option<FileStamp>,
}

/// Why a fetch failed, and whether waiting could help. `transient` (network/transport,
/// 5xx-class) failures retry after [`FAILED_RETRY_TTL`]; permanent ones (policy denial,
/// 4xx, decode/SVG rejection, size caps) stay `Failed` until a cache clear.
#[derive(Debug, Clone)]
pub struct FetchError {
    pub reason: String,
    pub transient: bool,
}

impl FetchError {
    #[must_use]
    pub fn permanent(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            transient: false,
        }
    }

    #[must_use]
    pub fn transient(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            transient: true,
        }
    }
}

/// The side that knows how to load bytes and decode them (implemented in `smudgy_ui`,
/// which can name `core` + `cloud`). Implementations own their decode-concurrency bound
/// and all I/O policy (HTTP cache, redirects, size caps); the store owns lifecycle,
/// memoization, and handle identity.
pub trait ImageFetcher: Send + Sync + 'static {
    /// Load + decode one source. `policy` is the *first requester's* isolate policy: its
    /// `server_name` attributes the fetch to that server's HTTP-cache namespace (plan D10),
    /// its `net_grants` re-validate every redirect hop for sandboxed loads, and its
    /// `read_grants` are re-checked post-canonicalization for local files. Entries are
    /// content-keyed, so a later requester with a different policy shares the result —
    /// its own policy already validated the *initial* URL/path at resolve time.
    fn fetch(
        &self,
        source: ResolvedImageSource,
        policy: Arc<ImageSourcePolicy>,
    ) -> BoxFut<Result<DecodedImage, FetchError>>;

    /// Cheap staleness probe for local-file-backed sources (`None` for others or on
    /// error). `policy` locates policy-relative sources (a local dev-override package's
    /// on-disk asset dir).
    fn probe(
        &self,
        source: ResolvedImageSource,
        policy: Arc<ImageSourcePolicy>,
    ) -> BoxFut<Option<FileStamp>>;
}

/// One image entry's current state. Swapped whole through the cell's `ArcSwap`.
#[derive(Debug, Clone)]
pub enum EntryState {
    /// Fetch in flight (or queued). Renders as the placeholder.
    Loading,
    /// Decoded and displayable. `handle` is the single minted identity for this content.
    Ready {
        handle: iced::widget::image::Handle,
        width: u32,
        height: u32,
        decoded_bytes: u64,
        file_stamp: Option<FileStamp>,
    },
    /// Load failed. Sticky: re-`ensure` is side-effect-free until `retry_at` (if any).
    Failed {
        reason: Arc<str>,
        retry_at: Option<Instant>,
    },
}

/// One source's live slot: state cell + recency/freshness stamps. Render closures and
/// resolve memos hold `Arc<ImageEntryCell>` and load per frame, lock-free.
#[derive(Debug)]
pub struct ImageEntryCell {
    state: ArcSwap<EntryState>,
    /// Millis since the store epoch of the last read — the LRU recency stamp. Relaxed:
    /// approximate order is all eviction needs.
    touched_ms: AtomicU64,
    /// Millis since epoch of the last freshness re-probe enqueue (local files only).
    last_probe_ms: AtomicU64,
    /// Set (under the writer lock) when the eviction sweep removes this cell from the
    /// map. Memo holders re-`ensure` on sight; live per-frame draws keep their stamp hot,
    /// so an evicted cell is by construction one nothing was drawing.
    evicted: AtomicBool,
    /// Abort handle for the in-flight fetch, if any. Cold path only.
    abort: Mutex<Option<tokio::task::AbortHandle>>,
    /// The owning store's epoch, so reads can stamp recency without reaching back to the
    /// store. `Instant` is `Copy`; every cell of one store carries the same epoch.
    epoch: Instant,
}

impl ImageEntryCell {
    fn new(state: EntryState, now_ms: u64, epoch: Instant) -> Self {
        Self {
            state: ArcSwap::from_pointee(state),
            touched_ms: AtomicU64::new(now_ms),
            last_probe_ms: AtomicU64::new(now_ms),
            evicted: AtomicBool::new(false),
            abort: Mutex::new(None),
            epoch,
        }
    }

    /// The current state (lock-free), stamping LRU recency — a drawn image is a hot image,
    /// even when its widget was built once and only re-renders (plan D3: "touched on
    /// load"), so `evict_cold` can never evict what a live render closure is displaying.
    /// One relaxed store + one monotonic-clock read; no locks, no syscalls.
    #[must_use]
    pub fn state(&self) -> Arc<EntryState> {
        self.touch();
        self.state.load_full()
    }

    /// Stamp recency without reading the state.
    pub fn touch(&self) {
        let now = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.touched_ms.store(now, Ordering::Relaxed);
    }

    /// Whether the eviction sweep dropped this cell from the map (memo holders must
    /// re-`ensure` instead of trusting a stale `Arc`).
    #[must_use]
    pub fn is_evicted(&self) -> bool {
        self.evicted.load(Ordering::Relaxed)
    }
}

type EntryMap = HashMap<String, Arc<ImageEntryCell>>;

struct StoreInner {
    map: ArcSwap<EntryMap>,
    /// Serializes map swaps (insert/evict/clear). Never held on a read path.
    writer: Mutex<()>,
    fetcher: Arc<dyn ImageFetcher>,
    /// Bumped on every entry state *transition* (completion, failure, refresh, clear) —
    /// the canvas geometry cache compares this to know a redraw's inputs changed.
    completion_generation: AtomicU64,
    /// Bumped on clear-cache. In-flight completions stamped with an older generation are
    /// discarded instead of resurrecting flushed entries.
    flush_generation: AtomicU64,
    /// Sum of `decoded_bytes` across `Ready` entries in the map.
    total_ready_bytes: AtomicU64,
    /// Repaint wakers, one per live session (weak: sessions die, the store doesn't).
    wakers: Mutex<Vec<Weak<watch::Sender<()>>>>,
    epoch: Instant,
    runtime: OnceLock<tokio::runtime::Runtime>,
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        // A spawned fetch task holds a store clone; when it finishes LAST (tests build
        // short-lived stores; production's is global), this Drop runs on a runtime worker,
        // where a blocking Runtime::drop panics. Shutdown in the background instead.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// The global store handle (cheap to clone; all clones share one inner).
#[derive(Clone)]
pub struct ImageStore {
    inner: Arc<StoreInner>,
}

impl std::fmt::Debug for ImageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ImageStore {{ entries: {}, ready_bytes: {} }}",
            self.inner.map.load().len(),
            self.inner.total_ready_bytes.load(Ordering::Relaxed)
        )
    }
}

impl ImageStore {
    #[must_use]
    pub fn new(fetcher: Arc<dyn ImageFetcher>) -> Self {
        Self {
            inner: Arc::new(StoreInner {
                map: ArcSwap::from_pointee(EntryMap::new()),
                writer: Mutex::new(()),
                fetcher,
                completion_generation: AtomicU64::new(0),
                flush_generation: AtomicU64::new(0),
                total_ready_bytes: AtomicU64::new(0),
                wakers: Mutex::new(Vec::new()),
                epoch: Instant::now(),
                runtime: OnceLock::new(),
            }),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.inner.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn runtime(&self) -> &tokio::runtime::Runtime {
        self.inner.runtime.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("smudgy-images")
                .enable_all()
                .build()
                .expect("the image-loader runtime always builds")
        })
    }

    /// Register a session's repaint waker (the `WidgetRoot`'s watch sender). Weak — a
    /// closed session's entry is pruned on the next poke.
    ///
    /// # Panics
    /// If the waker registry mutex is poisoned (a registrant panicked mid-push).
    pub fn register_waker(&self, waker: Weak<watch::Sender<()>>) {
        self.inner.wakers.lock().expect("waker lock").push(waker);
    }

    /// Wake every live session (loads landed; repaint). Snapshot outside any store lock.
    fn poke_all(&self) {
        let snapshot: Vec<_> = {
            let mut wakers = self.inner.wakers.lock().expect("waker lock");
            wakers.retain(|w| w.strong_count() > 0);
            wakers.clone()
        };
        for waker in snapshot {
            if let Some(tx) = waker.upgrade() {
                tx.send(()).ok();
            }
        }
    }

    /// Monotonic state-transition counter (canvas cache invalidation). Acquire: pairs
    /// with `complete()`'s Release bump so an observer of generation N+1 also observes
    /// the entry state that transition published — a canvas that clears its geometry
    /// cache on the bump must re-record the *new* state, not a stale `Loading`.
    #[must_use]
    pub fn completion_generation(&self) -> u64 {
        self.inner.completion_generation.load(Ordering::Acquire)
    }

    /// The existing cell for `key`, if any — one lock-free map load, no touch, no spawn.
    /// Lets a parse path distinguish already-known sources from ones whose `ensure`
    /// would spawn a brand-new fetch (the bound-scene new-source budget).
    #[must_use]
    pub fn peek(&self, key: &str) -> Option<Arc<ImageEntryCell>> {
        self.inner.map.load().get(key).cloned()
    }

    /// Total decoded bytes currently held (diagnostics / future UI).
    #[must_use]
    pub fn ready_bytes(&self) -> u64 {
        self.inner.total_ready_bytes.load(Ordering::Relaxed)
    }

    /// The entry cell for `source`, spawning a fetch exactly once per distinct source.
    /// Steady state (entry exists): one lock-free map load + one relaxed stamp store.
    /// `policy` drives a *newly spawned* fetch (cache namespace, hop grants — see
    /// [`ImageFetcher::fetch`]); an existing entry ignores it.
    #[must_use]
    pub fn ensure(
        &self,
        source: &ResolvedImageSource,
        policy: &Arc<ImageSourcePolicy>,
    ) -> Arc<ImageEntryCell> {
        self.ensure_keyed(&source.store_key(policy), source, policy)
    }

    /// [`ensure`](Self::ensure) with the cache key already in hand. The per-frame resolve
    /// memo stores `(source, key)` and calls this, so the steady-state frame path never
    /// re-derives the key (for a `data:` src that derivation once hashed megabytes) and
    /// never allocates: borrowed lookup, relaxed touch, one state load.
    #[must_use]
    pub fn ensure_keyed(
        &self,
        key: &str,
        source: &ResolvedImageSource,
        policy: &Arc<ImageSourcePolicy>,
    ) -> Arc<ImageEntryCell> {
        let now = self.now_ms();
        if let Some(cell) = self.inner.map.load().get(key) {
            cell.touched_ms.store(now, Ordering::Relaxed);
            self.maybe_refresh(key, cell, source, policy, now);
            return cell.clone();
        }
        self.insert_and_spawn(key, source, policy, now)
    }

    /// Slow path: insert a `Loading` cell (double-checked under the writer lock) and spawn
    /// its fetch on the store runtime. The flush generation is captured *inside* the lock
    /// scope, so a `clear()` serialized after this insert is guaranteed to observe a newer
    /// generation than the spawned task carries — its completion is then discarded.
    fn insert_and_spawn(
        &self,
        key: &str,
        source: &ResolvedImageSource,
        policy: &Arc<ImageSourcePolicy>,
        now: u64,
    ) -> Arc<ImageEntryCell> {
        let (cell, flush_gen) = {
            let _writer = self.inner.writer.lock().expect("writer lock");
            if let Some(existing) = self.inner.map.load().get(key) {
                existing.touched_ms.store(now, Ordering::Relaxed);
                return existing.clone();
            }
            let cell = Arc::new(ImageEntryCell::new(
                EntryState::Loading,
                now,
                self.inner.epoch,
            ));
            let mut map: EntryMap = (**self.inner.map.load()).clone();
            map.insert(key.to_owned(), cell.clone());
            self.inner.map.store(Arc::new(map));
            (cell, self.inner.flush_generation.load(Ordering::Acquire))
        };
        self.spawn_fetch(
            key.to_owned(),
            &cell,
            source.clone(),
            policy.clone(),
            flush_gen,
        );
        cell
    }

    fn spawn_fetch(
        &self,
        key: String,
        cell: &Arc<ImageEntryCell>,
        source: ResolvedImageSource,
        policy: Arc<ImageSourcePolicy>,
        flush_gen: u64,
    ) {
        let store = self.clone();
        let fut = self.inner.fetcher.fetch(source, policy);
        let task_cell = cell.clone();
        let task = self.runtime().spawn(async move {
            let result = fut.await;
            store.complete(&key, &task_cell, flush_gen, result);
        });
        *cell.abort.lock().expect("abort lock") = Some(task.abort_handle());
    }

    /// Land a fetch result: swap the cell state, account bytes, maybe evict, wake.
    ///
    /// State swap + byte accounting happen under the writer lock, serialized against
    /// `clear()` and `evict_cold()`: a completion whose flush generation is stale, or whose
    /// cell is no longer the map's current cell for `key` (evicted mid-flight, or cleared
    /// and re-ensured), still lands its state (a render closure may hold the cell and can
    /// show the image) but is never *accounted* — `total_ready_bytes` tracks map-resident
    /// `Ready` cells exactly, so the cap can neither leak phantom bytes nor double-subtract.
    fn complete(
        &self,
        key: &str,
        cell: &Arc<ImageEntryCell>,
        flush_gen: u64,
        result: Result<DecodedImage, FetchError>,
    ) {
        let state = match result {
            Ok(decoded) => {
                let decoded_bytes = decoded.rgba.len() as u64;
                let handle = iced::widget::image::Handle::from_rgba(
                    decoded.width,
                    decoded.height,
                    decoded.rgba,
                );
                EntryState::Ready {
                    handle,
                    width: decoded.width,
                    height: decoded.height,
                    decoded_bytes,
                    file_stamp: decoded.file_stamp,
                }
            }
            Err(err) => {
                log::warn!("smudgy images: {key} failed to load: {}", err.reason);
                EntryState::Failed {
                    reason: Arc::from(err.reason),
                    retry_at: err.transient.then(|| Instant::now() + FAILED_RETRY_TTL),
                }
            }
        };
        let over_cap = {
            let _writer = self.inner.writer.lock().expect("writer lock");
            if self.inner.flush_generation.load(Ordering::Acquire) != flush_gen {
                return;
            }
            let current = self
                .inner
                .map
                .load()
                .get(key)
                .is_some_and(|mapped| Arc::ptr_eq(mapped, cell));
            let previous_bytes = match &**cell.state.load() {
                EntryState::Ready { decoded_bytes, .. } => *decoded_bytes,
                _ => 0,
            };
            let new_bytes = match &state {
                EntryState::Ready { decoded_bytes, .. } => *decoded_bytes,
                _ => 0,
            };
            cell.state.store(Arc::new(state));
            if current {
                if new_bytes > 0 {
                    self.inner
                        .total_ready_bytes
                        .fetch_add(new_bytes, Ordering::Relaxed);
                }
                if previous_bytes > 0 {
                    self.inner
                        .total_ready_bytes
                        .fetch_sub(previous_bytes, Ordering::Relaxed);
                }
            }
            *cell.abort.lock().expect("abort lock") = None;
            self.inner
                .completion_generation
                .fetch_add(1, Ordering::Release);
            self.inner.total_ready_bytes.load(Ordering::Relaxed) > MAX_DECODED_BYTES
        };
        // Outside the writer scope: evict_cold re-takes the (non-reentrant) writer lock.
        if over_cap {
            self.evict_cold();
        }
        self.poke_all();
    }

    /// Re-spawn behavior on the fast path: retry expired network failures, and throttle
    /// freshness probes for local-file entries. Never blocks, never stats — probes run on
    /// the store runtime.
    fn maybe_refresh(
        &self,
        key: &str,
        cell: &Arc<ImageEntryCell>,
        source: &ResolvedImageSource,
        policy: &Arc<ImageSourcePolicy>,
        now: u64,
    ) {
        match &**cell.state.load() {
            EntryState::Failed {
                retry_at: Some(at), ..
            } if *at <= Instant::now() => {
                // One winner: CAS-like via the writer lock (cold — a failed entry).
                let respawn = {
                    let _writer = self.inner.writer.lock().expect("writer lock");
                    if let EntryState::Failed {
                        retry_at: Some(at), ..
                    } = &**cell.state.load()
                        && *at <= Instant::now()
                    {
                        cell.state.store(Arc::new(EntryState::Loading));
                        Some(self.inner.flush_generation.load(Ordering::Acquire))
                    } else {
                        None
                    }
                };
                if let Some(flush_gen) = respawn {
                    self.spawn_fetch(
                        key.to_string(),
                        cell,
                        source.clone(),
                        policy.clone(),
                        flush_gen,
                    );
                }
            }
            EntryState::Ready {
                file_stamp: Some(stamp),
                ..
            } => {
                let last = cell.last_probe_ms.load(Ordering::Relaxed);
                if now.saturating_sub(last) >= LOCAL_RECHECK_INTERVAL_MS
                    && cell
                        .last_probe_ms
                        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    let store = self.clone();
                    let cell = cell.clone();
                    let source = source.clone();
                    let policy = policy.clone();
                    let key = key.to_string();
                    let expected = *stamp;
                    let probe = self.inner.fetcher.probe(source.clone(), policy.clone());
                    self.runtime().spawn(async move {
                        if probe.await.is_some_and(|current| current != expected) {
                            let flush_gen = store.inner.flush_generation.load(Ordering::Acquire);
                            store.spawn_fetch(key, &cell, source, policy, flush_gen);
                        }
                    });
                }
            }
            _ => {}
        }
    }

    /// Evict least-recently-touched `Ready` entries until under the byte cap. Runs under
    /// the writer lock (rare: only when a completion crosses the cap). A cell being drawn
    /// every frame has a hot stamp and is effectively unevictable.
    fn evict_cold(&self) {
        let _writer = self.inner.writer.lock().expect("writer lock");
        let mut map: EntryMap = (**self.inner.map.load()).clone();
        let mut ready: Vec<(u64, String, u64)> = map
            .iter()
            .filter_map(|(key, cell)| match &**cell.state.load() {
                EntryState::Ready { decoded_bytes, .. } => Some((
                    cell.touched_ms.load(Ordering::Relaxed),
                    key.clone(),
                    *decoded_bytes,
                )),
                _ => None,
            })
            .collect();
        ready.sort_unstable();
        let mut total = self.inner.total_ready_bytes.load(Ordering::Relaxed);
        for (_touched, key, bytes) in ready {
            if total <= MAX_DECODED_BYTES {
                break;
            }
            if let Some(cell) = map.remove(&key) {
                cell.evicted.store(true, Ordering::Relaxed);
                total = total.saturating_sub(bytes);
                self.inner
                    .total_ready_bytes
                    .fetch_sub(bytes, Ordering::Relaxed);
            }
        }
        self.inner.map.store(Arc::new(map));
    }

    /// Clear everything: abort in-flight fetches, drop all entries, bump both generations
    /// (so late completions are discarded and canvases redraw), and wake sessions. The
    /// disk cache is the fetcher's business — this clears memory only.
    ///
    /// # Panics
    /// If the store's writer mutex is poisoned (a writer panicked mid-swap).
    pub fn clear(&self) {
        let old = {
            let _writer = self.inner.writer.lock().expect("writer lock");
            self.inner.flush_generation.fetch_add(1, Ordering::AcqRel);
            let old = self.inner.map.load_full();
            self.inner.map.store(Arc::new(EntryMap::new()));
            self.inner.total_ready_bytes.store(0, Ordering::Relaxed);
            old
        };
        for cell in old.values() {
            cell.evicted.store(true, Ordering::Relaxed);
            if let Some(abort) = cell.abort.lock().expect("abort lock").take() {
                abort.abort();
            }
        }
        self.inner
            .completion_generation
            .fetch_add(1, Ordering::Release);
        self.poke_all();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A store over an immediate 2×2 mock fetcher, for other modules' tests (canvas).
    pub(crate) fn test_store() -> (ImageStore, Arc<MockFetcher>) {
        let fetcher = MockFetcher::ok(2, 2);
        (ImageStore::new(fetcher.clone()), fetcher)
    }

    /// A fetcher that resolves immediately (or once `gate` releases it), counting calls.
    pub(crate) struct MockFetcher {
        pub(crate) calls: AtomicUsize,
        result: Mutex<Result<(u32, u32), FetchError>>,
        /// When present, fetches wait for a permit before resolving.
        gate: Option<Arc<tokio::sync::Semaphore>>,
    }

    impl MockFetcher {
        pub(crate) fn ok(width: u32, height: u32) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                result: Mutex::new(Ok((width, height))),
                gate: None,
            })
        }

        fn gated(width: u32, height: u32) -> (Arc<Self>, Arc<tokio::sync::Semaphore>) {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let fetcher = Arc::new(Self {
                calls: AtomicUsize::new(0),
                result: Mutex::new(Ok((width, height))),
                gate: Some(gate.clone()),
            });
            (fetcher, gate)
        }
    }

    impl ImageFetcher for MockFetcher {
        fn fetch(
            &self,
            _source: ResolvedImageSource,
            _policy: Arc<ImageSourcePolicy>,
        ) -> BoxFut<Result<DecodedImage, FetchError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = self.result.lock().unwrap().clone();
            let gate = self.gate.clone();
            Box::pin(async move {
                if let Some(gate) = gate {
                    let _permit = gate.acquire().await.expect("gate open");
                }
                result.map(|(width, height)| DecodedImage {
                    width,
                    height,
                    rgba: vec![0u8; (width * height * 4) as usize],
                    file_stamp: None,
                })
            })
        }

        fn probe(
            &self,
            _source: ResolvedImageSource,
            _policy: Arc<ImageSourcePolicy>,
        ) -> BoxFut<Option<FileStamp>> {
            Box::pin(async { None })
        }
    }

    fn src(name: &str) -> ResolvedImageSource {
        ResolvedImageSource::PackageAsset {
            owner: "o".into(),
            name: "p".into(),
            version: "1".into(),
            subpath: name.into(),
        }
    }

    pub(crate) fn test_policy() -> Arc<ImageSourcePolicy> {
        Arc::new(ImageSourcePolicy {
            trusted: true,
            server_name: Arc::from("s"),
            hosted_packages: smudgy_cloud::image_source::HostedPackages::default(),
            net_grants: smudgy_cloud::image_source::NetGrants::default(),
            read_grants: Vec::new(),
            modules_root: std::path::PathBuf::from("/m"),
            packages_root: std::path::PathBuf::from("/p"),
        })
    }

    fn wait_ready(cell: &ImageEntryCell) -> Arc<EntryState> {
        for _ in 0..500 {
            let state = cell.state();
            if !matches!(&*state, EntryState::Loading) {
                return state;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("entry never left Loading");
    }

    /// Wait until the completion generation passes `after`. `wait_ready` observes the
    /// lock-free state swap, which lands *before* byte accounting inside `complete()`'s
    /// writer scope; the generation bump is a `Release` increment after both, so once it
    /// is visible the accounting is too.
    fn wait_generation(store: &ImageStore, after: u64) {
        for _ in 0..500 {
            if store.completion_generation() > after {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("completion generation never advanced past {after}");
    }

    #[test]
    fn ensure_dedups_and_completes() {
        let fetcher = MockFetcher::ok(2, 2);
        let store = ImageStore::new(fetcher.clone());
        let server = test_policy();
        let a = store.ensure(&src("x.png"), &server);
        let b = store.ensure(&src("x.png"), &server);
        assert!(Arc::ptr_eq(&a, &b), "same source shares one cell");
        let state = wait_ready(&a);
        match &*state {
            EntryState::Ready {
                width,
                height,
                decoded_bytes,
                ..
            } => {
                assert_eq!((*width, *height), (2, 2));
                assert_eq!(*decoded_bytes, 16);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1, "in-flight dedup");
        wait_generation(&store, 0);
        assert_eq!(store.ready_bytes(), 16);
    }

    #[test]
    fn failures_are_sticky_until_ttl() {
        let fetcher = MockFetcher::ok(1, 1);
        *fetcher.result.lock().unwrap() = Err(FetchError::transient("nope"));
        let store = ImageStore::new(fetcher.clone());
        let server = test_policy();
        let cell = store.ensure(&src("bad.png"), &server);
        let state = wait_ready(&cell);
        assert!(
            matches!(
                &*state,
                EntryState::Failed {
                    retry_at: Some(_),
                    ..
                }
            ),
            "transient failures carry a retry deadline"
        );
        // Re-ensure within the TTL is side-effect-free.
        let _ = store.ensure(&src("bad.png"), &server);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1, "no retry storm");
    }

    #[test]
    fn permanent_failures_never_retry() {
        let fetcher = MockFetcher::ok(1, 1);
        *fetcher.result.lock().unwrap() = Err(FetchError::permanent("svg rejected"));
        let store = ImageStore::new(fetcher.clone());
        let server = test_policy();
        let cell = store.ensure(&src("no.svg"), &server);
        let state = wait_ready(&cell);
        assert!(
            matches!(&*state, EntryState::Failed { retry_at: None, .. }),
            "permanent failures have no retry deadline"
        );
    }

    #[test]
    fn clear_mid_flight_never_accounts_the_late_completion() {
        let (fetcher, gate) = MockFetcher::gated(64, 64);
        let store = ImageStore::new(fetcher);
        let server = test_policy();
        let cell = store.ensure(&src("late.png"), &server);
        // Flush while the fetch is parked on the gate, then release it.
        store.clear();
        gate.add_permits(1);
        // The late completion must be discarded: no resurrected entry, no phantom bytes.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            store.ready_bytes(),
            0,
            "late completion accounted after clear"
        );
        assert!(cell.is_evicted());
        assert!(store.inner.map.load().is_empty(), "no resurrected entry");
    }

    #[test]
    fn clear_discards_and_bumps_generations() {
        let fetcher = MockFetcher::ok(1, 1);
        let store = ImageStore::new(fetcher);
        let server = test_policy();
        let cell = store.ensure(&src("y.png"), &server);
        wait_ready(&cell);
        let generation = store.completion_generation();
        store.clear();
        assert!(cell.is_evicted());
        assert_eq!(store.ready_bytes(), 0);
        assert!(store.completion_generation() > generation);
        // A fresh ensure after clear starts a new fetch on a new cell.
        let fresh = store.ensure(&src("y.png"), &server);
        assert!(!Arc::ptr_eq(&cell, &fresh));
    }

    #[test]
    fn wakers_fire_on_completion_and_prune_dead() {
        let fetcher = MockFetcher::ok(1, 1);
        let store = ImageStore::new(fetcher);
        let server = test_policy();
        let tx = Arc::new(watch::channel(()).0);
        let rx = tx.subscribe();
        store.register_waker(Arc::downgrade(&tx));
        let dead = Arc::new(watch::channel(()).0);
        store.register_waker(Arc::downgrade(&dead));
        drop(dead);
        let cell = store.ensure(&src("w.png"), &server);
        wait_ready(&cell);
        // `poke_all` runs after the writer scope that publishes the state, so the poke
        // gets the same bounded grace as the state itself.
        for _ in 0..500 {
            if rx.has_changed().unwrap() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("completion never poked the live waker");
    }
}
