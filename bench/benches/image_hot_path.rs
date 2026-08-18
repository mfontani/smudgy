//! Pins the `<Image>`/canvas-image **steady-state frame budget** (plan D3): under
//! createWidget-per-frame usage, each image's per-frame cost after the first resolution is
//! a memoized probe — no locks, no syscalls, no URL parsing, no payload hashing. The
//! groups measure the layers that per-frame path is made of, plus the cold costs the
//! resolve memo exists to keep OFF that path:
//!
//! - `ensure_keyed_hit/N*`: the store probe a static `<Image>`'s build op pays per frame
//!   with the `(source, key)` memo hit in hand — one borrowed map lookup + relaxed
//!   recency touch + one entry-state load (`maybe_refresh`'s stamp compare). The whole
//!   budget class is ~100 ns; a lock, stat, or parse creeping in shows up as a 10–100×
//!   regression here.
//! - `state_read`: the render closure's per-frame share — one lock-free `cell.state()`
//!   (ArcSwap load + touch). Canvas draw arms and `<Image>` view closures pay exactly this.
//! - `memo_key/*`: the per-frame hash that keys the resolve memo — bounded by design
//!   (inline ≤ 512 B; larger srcs digest first/last 4 KiB, never the full payload).
//! - `resolve_cold/*`: the memo-MISS costs for contrast — a full grammar+policy resolve
//!   (URL parse for https; a full-payload SHA-256 for `data:`). The 2 MiB data-URI cell
//!   documents why re-resolving per frame was a reviewed defect, not a nitpick.
//!
//! No V8, no I/O: the fetcher is a stub that decodes nothing and returns a 1×1 image, so
//! the numbers isolate the store/grammar layers the frame path actually touches.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use smudgy_cloud::image_source::{
    HostedPackages, ImageSourcePolicy, NetGrants, RegisteredImageCreator, memo_key,
    register_creator, resolve_src,
};
use smudgy_widgets::{DecodedImage, FetchError, FileStamp, ImageFetcher, ImageStore};

/// Immediate 1×1 stub: the benches measure store/grammar layers, never decode or I/O.
struct StubFetcher;

impl ImageFetcher for StubFetcher {
    fn fetch(
        &self,
        _source: smudgy_cloud::image_source::ResolvedImageSource,
        _policy: Arc<ImageSourcePolicy>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<DecodedImage, FetchError>> + Send + 'static>>
    {
        Box::pin(async {
            Ok(DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![0u8; 4],
                file_stamp: None,
            })
        })
    }

    fn probe(
        &self,
        _source: smudgy_cloud::image_source::ResolvedImageSource,
        _policy: Arc<ImageSourcePolicy>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Option<FileStamp>> + Send + 'static>> {
        Box::pin(async { None })
    }
}

fn trusted_policy() -> Arc<ImageSourcePolicy> {
    Arc::new(ImageSourcePolicy {
        trusted: true,
        server_name: Arc::from("bench"),
        hosted_packages: HostedPackages::default(),
        net_grants: NetGrants::default(),
        read_grants: Vec::new(),
        modules_root: std::path::PathBuf::from("/bench/modules"),
        packages_root: std::path::PathBuf::from("/bench/packages"),
    })
}

fn user_creator(policy: Arc<ImageSourcePolicy>) -> RegisteredImageCreator {
    register_creator(r#"{"kind":"user"}"#, None, policy)
        .expect("a user creator registers on a trusted policy")
}

/// The memoized per-frame probe: resolve once (untimed), then measure `ensure_keyed`
/// round-robin over N distinct images — the shape of N static `<Image>`s rebuilt every
/// frame by `createWidget`.
fn bench_ensure_keyed_hit(c: &mut Criterion) {
    let mut group = c.benchmark_group("image_ensure_keyed_hit");
    for n in [1usize, 64] {
        let store = ImageStore::new(Arc::new(StubFetcher));
        let policy = trusted_policy();
        let creator = user_creator(policy.clone());
        let resolved: Vec<_> = (0..n)
            .map(|i| {
                let source = resolve_src(&format!("assets/img_{i}.png"), &creator, false)
                    .expect("relative src resolves");
                let key: Arc<str> = Arc::from(source.store_key(&policy));
                let _ = store.ensure_keyed(&key, &source, &policy);
                (key, source)
            })
            .collect();
        let mut i = 0usize;
        group.bench_function(format!("N{n}"), |b| {
            b.iter(|| {
                let (key, source) = &resolved[i % resolved.len()];
                i = i.wrapping_add(1);
                black_box(store.ensure_keyed(black_box(key), source, &policy))
            });
        });
    }
    group.finish();
}

/// The render closure's per-frame read: one lock-free state load + recency touch.
fn bench_state_read(c: &mut Criterion) {
    let store = ImageStore::new(Arc::new(StubFetcher));
    let policy = trusted_policy();
    let creator = user_creator(policy.clone());
    let source = resolve_src("assets/one.png", &creator, false).expect("resolves");
    let cell = store.ensure(&source, &policy);
    c.bench_function("image_state_read", |b| {
        b.iter(|| black_box(cell.state()));
    });
}

/// The bounded memo-key hash: inline strings clone-hash; large srcs digest 8 KiB max.
fn bench_memo_key(c: &mut Criterion) {
    let mut group = c.benchmark_group("image_memo_key");
    let inline = "assets/portraits/character_042.png";
    group.bench_function("inline_34b", |b| {
        b.iter(|| black_box(memo_key(black_box(inline))));
    });
    let large = format!("data:image/png;base64,{}", "A".repeat(2 * 1024 * 1024));
    group.bench_function("data_2mb", |b| {
        b.iter(|| black_box(memo_key(black_box(large.as_str()))));
    });
    group.finish();
}

/// Memo-MISS costs, for contrast: what the per-frame path would pay WITHOUT the memo.
/// `data_2mb` runs a full-payload SHA-256 per call — the reviewed per-frame defect.
fn bench_resolve_cold(c: &mut Criterion) {
    let mut group = c.benchmark_group("image_resolve_cold");
    // Full resolves re-run per iteration by design (this is the cold path).
    group.sample_size(30);
    let policy = trusted_policy();
    let creator = user_creator(policy);
    group.bench_function("relative", |b| {
        b.iter(|| black_box(resolve_src(black_box("assets/img.png"), &creator, false)));
    });
    group.bench_function("https", |b| {
        b.iter(|| {
            black_box(resolve_src(
                black_box("https://cdn.example.com/textures/atlas_01.png"),
                &creator,
                false,
            ))
        });
    });
    // Just under the 2 MiB encoded cap: the resolve digests the whole payload (the cost
    // the memo keeps off the frame path). Anything over the cap would measure the cheap
    // rejection instead.
    let data = format!("data:image/png;base64,{}", "B".repeat(2 * 1024 * 1024 - 64));
    assert!(
        resolve_src(&data, &creator, false).is_ok(),
        "bench data URI must resolve, not hit the size cap"
    );
    group.bench_function("data_2mb", |b| {
        b.iter(|| black_box(resolve_src(black_box(data.as_str()), &creator, false)));
    });
    group.finish();
}

fn sanity_check() {
    // The probe really is a hit (same cell back), and completions land Ready.
    let store = ImageStore::new(Arc::new(StubFetcher));
    let policy = trusted_policy();
    let creator = user_creator(policy.clone());
    let source = resolve_src("assets/sanity.png", &creator, false).expect("resolves");
    let key: Arc<str> = Arc::from(source.store_key(&policy));
    let first = store.ensure_keyed(&key, &source, &policy);
    let second = store.ensure_keyed(&key, &source, &policy);
    assert!(
        Arc::ptr_eq(&first, &second),
        "steady-state probe must hit the same cell"
    );
    for _ in 0..500 {
        if !matches!(&*first.state(), smudgy_widgets::EntryState::Loading) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("stub fetch never completed");
}

fn image_hot_path(c: &mut Criterion) {
    eprintln!(
        "image hot path: ensure_keyed round-robin over N pre-resolved srcs (the memoized \
         per-frame probe), one lock-free state read (the render closure), the bounded \
         memo-key hash, and the cold resolve costs the memo keeps off the frame path"
    );
    if std::env::var("SMUDGY_BENCH_SKIP_SANITY").is_err() {
        sanity_check();
    }
    bench_ensure_keyed_hit(c);
    bench_state_read(c);
    bench_memo_key(c);
    bench_resolve_cold(c);
}

criterion_group!(benches, image_hot_path);
criterion_main!(benches);
