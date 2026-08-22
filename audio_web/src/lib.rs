//! Hosted Web Audio authorities and output adapter for Smudgy's shared mixer.

use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock,
    atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering},
};
#[cfg(test)]
use std::sync::{Barrier, mpsc};
#[cfg(test)]
use std::thread::{self, ThreadId};

pub use deno_audio::AudioHostLimits;
use deno_audio::{
    AudioExtensionOptions, AudioHost, AudioHostUsage, AudioOutputConfig, AudioOutputDeathReason,
    AudioOutputEndpointShutdown, AudioOutputError, AudioOutputErrorKind, AudioOutputEventSink,
    AudioOutputFactory, AudioOutputRequest, AudioOutputStartFailure, AudioPermissions,
    AudioRenderCallback, AudioRenderFormat, AudioRenderStatus, PreparedAudioOutput,
    RunningAudioOutput, SilentAudioOutput,
};
use deno_error::JsErrorBox;
use smudgy_audio::{
    AudioSessionId, MixerControlError, MixerFailureObserver, MixerFrame, MixerGainState,
    MixerInput, MixerInputReservation, MixerInputShutdown, MixerInputStartFailure,
    MixerInputStatus, MixerNativeBusHandle, MixerOutputFailure, MixerRetirementError,
    MixerScriptBusHandle, MixerSessionGainAuthority, MixerSessionOwner, MixerSessionRetirement,
    MixerSpeechBusHandle, RunningMixerInput,
};

const CHANNELS: usize = 2;
const FRAMES: usize = 128;
const INTERLEAVED_SAMPLES: usize = CHANNELS * FRAMES;

/// Maximum number of versionless sandbox-root gain scopes retained by one
/// exact session registration.
///
/// Successfully committed entries are never evicted or reused during the
/// registration lifetime; a failed pending bind is rolled back. A package
/// version reload therefore finds the same root entry, while this fixed
/// 256-root metadata bound prevents package identity state from growing without
/// limit. It is deliberately independent from the mixer's 32 simultaneous
/// Script inputs: a root costs no input until it opens a context.
pub const MAX_PACKAGE_AUDIO_SCOPES: usize = 256;

static NEXT_SCOPE_GENERATION: AtomicU64 = AtomicU64::new(1);
static NEXT_PACKAGE_SCOPE_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct LifecycleGate {
    open: bool,
}

impl LifecycleGate {
    const fn new() -> Self {
        Self { open: true }
    }
}

fn lock_gate(gate: &Mutex<LifecycleGate>) -> MutexGuard<'_, LifecycleGate> {
    gate.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct ApplicationAudioState {
    host: Arc<AudioHost>,
    gate: Mutex<LifecycleGate>,
    #[cfg(test)]
    prepare_hook: Mutex<Option<Arc<TestPrepareHook>>>,
}

impl fmt::Debug for ApplicationAudioState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationAudioState")
            .field("host", &self.host)
            .field("open", &lock_gate(&self.gate).open)
            .finish_non_exhaustive()
    }
}

/// Unique owner of application-wide Web Audio admission.
///
/// The owner is deliberately not cloneable. Its cloneable registrar can mint
/// session authorities while this owner remains open; sealing or dropping the
/// owner rejects later registration and audible online-context preparation.
pub struct ApplicationAudioOwner {
    state: Arc<ApplicationAudioState>,
}

impl fmt::Debug for ApplicationAudioOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationAudioOwner")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl ApplicationAudioOwner {
    /// Creates one bounded application accounting and lifecycle domain.
    #[must_use]
    pub fn new(limits: AudioHostLimits) -> Self {
        Self {
            state: Arc::new(ApplicationAudioState {
                host: Arc::new(AudioHost::new(limits)),
                gate: Mutex::new(LifecycleGate::new()),
                #[cfg(test)]
                prepare_hook: Mutex::new(None),
            }),
        }
    }

    /// Returns a cloneable session registrar sharing this exact host and gate.
    #[must_use]
    pub fn registrar(&self) -> ApplicationAudioRegistrar {
        ApplicationAudioRegistrar {
            state: Arc::clone(&self.state),
        }
    }

    /// Returns a point-in-time, non-authoritative aggregate usage snapshot.
    #[must_use]
    pub fn usage(&self) -> AudioHostUsage {
        self.state.host.usage()
    }

    /// Absorbingly rejects new sessions and online output preparation.
    ///
    /// Returns `true` only for the transition from open to sealed.
    pub fn seal(&mut self) -> bool {
        let mut gate = lock_gate(&self.state.gate);
        let was_open = gate.open;
        gate.open = false;
        was_open
    }
}

impl Drop for ApplicationAudioOwner {
    fn drop(&mut self) {
        self.seal();
    }
}

/// Cloneable authority that registers exact mixer sessions with one app host.
#[derive(Clone)]
pub struct ApplicationAudioRegistrar {
    state: Arc<ApplicationAudioState>,
}

impl fmt::Debug for ApplicationAudioRegistrar {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationAudioRegistrar")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// Stable classification for a rejected session registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionAudioRegistrationError {
    /// The unique application owner has sealed registration.
    ApplicationSealed,
    /// The process-wide non-wrapping scope identity space is exhausted.
    GenerationExhausted,
}

/// Opaque identity of one exact hosted session-audio registration.
///
/// The process-wide non-wrapping generation prevents a delayed application
/// command from controlling a later registration that reuses the same public
/// session id. This key carries no mixer, output, or script authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionAudioControlKey {
    session_id: AudioSessionId,
    generation: u64,
}

/// Remembered and effective gain for one sandbox-root package scope.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PackageAudioGainState {
    linear: f32,
    muted: bool,
}

impl PackageAudioGainState {
    /// Remembered finite linear gain in `0..=1`.
    #[must_use]
    pub const fn linear(self) -> f32 {
        self.linear
    }

    /// Whether this package scope is muted.
    #[must_use]
    pub const fn is_muted(self) -> bool {
        self.muted
    }

    /// Gain applied between the Web Audio graph and the session Script bus.
    #[must_use]
    pub const fn effective_linear(self) -> f32 {
        if self.muted { 0.0 } else { self.linear }
    }
}

impl Default for PackageAudioGainState {
    fn default() -> Self {
        Self {
            linear: 1.0,
            muted: false,
        }
    }
}

/// Stable failure to bind one sandbox-root package output scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PackageAudioScopeError {
    /// The owner or package name was empty.
    InvalidIdentity,
    /// The exact session registration has begun shutdown.
    SessionClosed,
    /// The bounded versionless root registry is full.
    Capacity,
    /// This root already has an uncommitted isolate construction.
    AlreadyBinding,
    /// The requested remembered gain was non-finite or outside `0..=1`.
    InvalidGain,
    /// The process-wide non-wrapping root-generation space is exhausted.
    GenerationExhausted,
}

/// Failure while atomically staging a complete persisted session policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionAudioPolicyError {
    /// The session mixer rejected the complete gain state.
    Mixer(MixerControlError),
    /// One sandbox-root identity or gain was invalid.
    Package(PackageAudioScopeError),
}

/// Stable package-control lookup or mutation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PackageAudioControlError {
    /// No sandbox root with this versionless identity was bound.
    UnknownPackage,
    /// The key belongs to another exact session registration.
    StaleSession,
    /// The root entry generation no longer matches the key.
    StalePackage,
    /// The exact session registration has begun shutdown.
    SessionClosed,
    /// The requested linear value was non-finite or outside `0..=1`.
    InvalidGain,
    /// The shared physical output has failed.
    OutputFailed,
}

/// Opaque identity for one versionless sandbox-root entry in an exact session.
///
/// The private non-wrapping root generation prevents a delayed controller from
/// reaching a replacement if the bounded registry ever gains reclamation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageAudioControlKey {
    session: SessionAudioControlKey,
    owner: Arc<str>,
    name: Arc<str>,
    root_generation: u64,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PackageAudioRoot {
    owner: Arc<str>,
    name: Arc<str>,
}

impl PackageAudioRoot {
    fn new(owner: &str, name: &str) -> Result<Self, PackageAudioScopeError> {
        if owner.is_empty() || name.is_empty() {
            return Err(PackageAudioScopeError::InvalidIdentity);
        }
        Ok(Self {
            owner: Arc::from(owner.to_ascii_lowercase()),
            name: Arc::from(name.to_ascii_lowercase()),
        })
    }
}

#[derive(Debug)]
struct PackageGainState {
    remembered: Mutex<PackageAudioGainState>,
    effective_bits: AtomicU32,
}

impl PackageGainState {
    fn new() -> Self {
        let state = PackageAudioGainState::default();
        Self {
            remembered: Mutex::new(state),
            effective_bits: AtomicU32::new(state.effective_linear().to_bits()),
        }
    }

    fn with_remembered(linear: f32, muted: bool) -> Result<Self, PackageAudioScopeError> {
        if !linear.is_finite() || !(0.0..=1.0).contains(&linear) {
            return Err(PackageAudioScopeError::InvalidGain);
        }
        let state = PackageAudioGainState {
            linear: if linear == 0.0 { 0.0 } else { linear },
            muted,
        };
        Ok(Self {
            remembered: Mutex::new(state),
            effective_bits: AtomicU32::new(state.effective_linear().to_bits()),
        })
    }

    fn effective_linear(&self) -> f32 {
        f32::from_bits(self.effective_bits.load(Ordering::Relaxed))
    }

    fn update_linear(
        &self,
        linear: f32,
    ) -> Result<PackageAudioGainState, PackageAudioControlError> {
        let linear = validate_package_gain(linear)?;
        let mut state = self
            .remembered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.linear = linear;
        self.effective_bits
            .store(state.effective_linear().to_bits(), Ordering::Release);
        Ok(*state)
    }

    fn update_muted(&self, muted: bool) -> PackageAudioGainState {
        let mut state = self
            .remembered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.muted = muted;
        self.effective_bits
            .store(state.effective_linear().to_bits(), Ordering::Release);
        *state
    }

    fn update_state(
        &self,
        linear: f32,
        muted: bool,
    ) -> Result<PackageAudioGainState, PackageAudioControlError> {
        let linear = validate_package_gain(linear)?;
        let mut state = self
            .remembered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = PackageAudioGainState { linear, muted };
        self.effective_bits
            .store(state.effective_linear().to_bits(), Ordering::Release);
        Ok(*state)
    }

    fn snapshot(&self) -> PackageAudioGainState {
        *self
            .remembered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn restore(&self, state: PackageAudioGainState) {
        let mut remembered = self
            .remembered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *remembered = state;
        self.effective_bits
            .store(state.effective_linear().to_bits(), Ordering::Release);
    }
}

fn validate_package_gain(linear: f32) -> Result<f32, PackageAudioControlError> {
    if !linear.is_finite() || !(0.0..=1.0).contains(&linear) {
        return Err(PackageAudioControlError::InvalidGain);
    }
    Ok(if linear == 0.0 { 0.0 } else { linear })
}

#[derive(Debug)]
struct PackageAudioEntry {
    gain: Arc<PackageGainState>,
    active_generation: Option<u64>,
    pending_generation: Option<u64>,
    ever_committed: bool,
}

#[derive(Debug)]
struct PackageAudioRegistry {
    session: SessionAudioControlKey,
    bus: MixerScriptBusHandle,
    app: Arc<ApplicationAudioState>,
    session_gate: Arc<Mutex<LifecycleGate>>,
    entries: Mutex<BTreeMap<PackageAudioRoot, PackageAudioEntry>>,
}

struct PreparedPackagePolicy {
    updates: Vec<(PackageAudioRoot, f32, bool)>,
}

struct PackagePolicyRollback {
    previous: Vec<(
        PackageAudioRoot,
        Arc<PackageGainState>,
        PackageAudioGainState,
    )>,
    inserted: Vec<PackageAudioRoot>,
}

impl PackageAudioRegistry {
    fn prepare_policy(
        &self,
        packages: impl IntoIterator<Item = (Arc<str>, Arc<str>, f32, bool)>,
    ) -> Result<PreparedPackagePolicy, PackageAudioScopeError> {
        let mut requested = BTreeMap::new();
        for (owner, name, linear, muted) in packages {
            let root = PackageAudioRoot::new(&owner, &name)?;
            if !linear.is_finite() || !(0.0..=1.0).contains(&linear) {
                return Err(PackageAudioScopeError::InvalidGain);
            }
            requested.insert(root, (if linear == 0.0 { 0.0 } else { linear }, muted));
        }

        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut available = MAX_PACKAGE_AUDIO_SCOPES.saturating_sub(entries.len());
        let updates = requested
            .into_iter()
            .filter_map(|(root, (linear, muted))| {
                if entries.contains_key(&root) {
                    Some((root, linear, muted))
                } else if linear == 1.0 && !muted {
                    // Default policy needs no dormant metadata, but an
                    // existing active/dormant root above still receives it
                    // so a reload can reset a formerly non-default gain.
                    None
                } else if available > 0 {
                    available -= 1;
                    Some((root, linear, muted))
                } else {
                    // A full registry fails additional package scopes closed;
                    // it never blocks the owning terminal session or reload.
                    None
                }
            })
            .collect();
        Ok(PreparedPackagePolicy { updates })
    }

    fn apply_prepared_policy(&self, policy: PreparedPackagePolicy) -> PackagePolicyRollback {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut previous = Vec::new();
        let mut inserted = Vec::new();
        for (root, linear, muted) in policy.updates {
            if let Some(entry) = entries.get_mut(&root) {
                let state = entry.gain.snapshot();
                previous.push((root, Arc::clone(&entry.gain), state));
                entry
                    .gain
                    .update_state(linear, muted)
                    .expect("prepared package gain is valid");
            } else {
                let gain = PackageGainState::with_remembered(linear, muted)
                    .expect("prepared package gain is valid");
                inserted.push(root.clone());
                entries.insert(
                    root,
                    PackageAudioEntry {
                        gain: Arc::new(gain),
                        active_generation: None,
                        pending_generation: None,
                        ever_committed: false,
                    },
                );
            }
        }
        PackagePolicyRollback { previous, inserted }
    }

    fn rollback_policy(&self, rollback: PackagePolicyRollback) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (root, expected_gain, state) in rollback.previous {
            if let Some(entry) = entries.get(&root)
                && Arc::ptr_eq(&entry.gain, &expected_gain)
            {
                entry.gain.restore(state);
            }
        }
        for root in rollback.inserted {
            if entries.get(&root).is_some_and(|entry| {
                !entry.ever_committed
                    && entry.active_generation.is_none()
                    && entry.pending_generation.is_none()
            }) {
                entries.remove(&root);
            }
        }
    }

    /// Seed a dormant root before its isolate begins construction. A later
    /// bind receives the already-effective snapshot, so top-level script code
    /// cannot briefly render at the default gain.
    fn seed(
        &self,
        owner: &str,
        name: &str,
        linear: f32,
        muted: bool,
    ) -> Result<PackageAudioGainState, PackageAudioScopeError> {
        let root = PackageAudioRoot::new(owner, name)?;
        let gain = PackageGainState::with_remembered(linear, muted)?;
        let remembered = *gain
            .remembered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !entries.contains_key(&root) && entries.len() >= MAX_PACKAGE_AUDIO_SCOPES {
            return Err(PackageAudioScopeError::Capacity);
        }
        match entries.get_mut(&root) {
            Some(entry) => {
                entry
                    .gain
                    .update_state(linear, muted)
                    .map_err(|_| PackageAudioScopeError::InvalidGain)?;
            }
            None => {
                entries.insert(
                    root,
                    PackageAudioEntry {
                        gain: Arc::new(gain),
                        active_generation: None,
                        pending_generation: None,
                        ever_committed: false,
                    },
                );
            }
        }
        Ok(remembered)
    }

    #[cfg(test)]
    fn bind(
        self: &Arc<Self>,
        owner: &str,
        name: &str,
    ) -> Result<(Arc<PackageGainState>, PackageAudioScopeBinding), PackageAudioScopeError> {
        let root = PackageAudioRoot::new(owner, name)?;
        self.bind_root(root)
    }

    fn bind_root(
        self: &Arc<Self>,
        root: PackageAudioRoot,
    ) -> Result<(Arc<PackageGainState>, PackageAudioScopeBinding), PackageAudioScopeError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !entries.contains_key(&root) && entries.len() >= MAX_PACKAGE_AUDIO_SCOPES {
            return Err(PackageAudioScopeError::Capacity);
        }
        if entries
            .get(&root)
            .is_some_and(|entry| entry.pending_generation.is_some())
        {
            return Err(PackageAudioScopeError::AlreadyBinding);
        }
        let Ok(generation) = NEXT_PACKAGE_SCOPE_GENERATION.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |generation| generation.checked_add(1),
        ) else {
            return Err(PackageAudioScopeError::GenerationExhausted);
        };
        let entry = entries
            .entry(root.clone())
            .or_insert_with(|| PackageAudioEntry {
                gain: Arc::new(PackageGainState::new()),
                active_generation: None,
                pending_generation: None,
                ever_committed: false,
            });
        entry.pending_generation = Some(generation);
        let gain = Arc::clone(&entry.gain);
        drop(entries);
        Ok((
            gain,
            PackageAudioScopeBinding {
                registry: Arc::clone(self),
                root,
                generation,
                committed: false,
            },
        ))
    }

    fn commit(
        &self,
        root: &PackageAudioRoot,
        generation: u64,
    ) -> Result<(), PackageAudioScopeError> {
        let app = lock_gate(&self.app.gate);
        let session = lock_gate(&self.session_gate);
        if !app.open || !session.open {
            return Err(PackageAudioScopeError::SessionClosed);
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries
            .get_mut(root)
            .expect("a live package binding retains its registry entry");
        assert_eq!(
            entry.pending_generation,
            Some(generation),
            "a package binding commits its own pending generation"
        );
        entry.pending_generation = None;
        entry.active_generation = Some(generation);
        entry.ever_committed = true;
        drop(entries);
        drop(session);
        drop(app);
        Ok(())
    }

    fn release(&self, root: &PackageAudioRoot, generation: u64, committed: bool) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = entries.get_mut(root) else {
            return;
        };
        if committed {
            if entry.active_generation == Some(generation) {
                entry.active_generation = None;
            }
        } else if entry.pending_generation == Some(generation) {
            entry.pending_generation = None;
        }
        if !entry.ever_committed
            && entry.active_generation.is_none()
            && entry.pending_generation.is_none()
        {
            entries.remove(root);
        }
    }

    fn control_key(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<PackageAudioControlKey, PackageAudioControlError> {
        let root = PackageAudioRoot::new(owner, name)
            .map_err(|_| PackageAudioControlError::UnknownPackage)?;
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries
            .get(&root)
            .ok_or(PackageAudioControlError::UnknownPackage)?;
        let root_generation = entry
            .active_generation
            .ok_or(PackageAudioControlError::UnknownPackage)?;
        Ok(PackageAudioControlKey {
            session: self.session,
            owner: root.owner,
            name: root.name,
            root_generation,
        })
    }

    fn validate_entry<'a>(
        &'a self,
        entries: &'a mut BTreeMap<PackageAudioRoot, PackageAudioEntry>,
        key: &PackageAudioControlKey,
    ) -> Result<&'a mut PackageAudioEntry, PackageAudioControlError> {
        if key.session != self.session {
            return Err(PackageAudioControlError::StaleSession);
        }
        let root = PackageAudioRoot {
            owner: Arc::clone(&key.owner),
            name: Arc::clone(&key.name),
        };
        let entry = entries
            .get_mut(&root)
            .ok_or(PackageAudioControlError::UnknownPackage)?;
        if entry.active_generation != Some(key.root_generation) {
            return Err(PackageAudioControlError::StalePackage);
        }
        Ok(entry)
    }

    fn update_linear(
        &self,
        key: &PackageAudioControlKey,
        linear: f32,
        mut output_failed: impl FnMut() -> bool,
    ) -> Result<PackageAudioGainState, PackageAudioControlError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = Self::validate_entry(self, &mut entries, key)?;
        if output_failed() {
            return Err(PackageAudioControlError::OutputFailed);
        }
        let state = entry.gain.update_linear(linear)?;
        if output_failed() {
            Err(PackageAudioControlError::OutputFailed)
        } else {
            Ok(state)
        }
    }

    fn update_muted(
        &self,
        key: &PackageAudioControlKey,
        muted: bool,
        mut output_failed: impl FnMut() -> bool,
    ) -> Result<PackageAudioGainState, PackageAudioControlError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = Self::validate_entry(self, &mut entries, key)?;
        if output_failed() {
            return Err(PackageAudioControlError::OutputFailed);
        }
        let state = entry.gain.update_muted(muted);
        if output_failed() {
            Err(PackageAudioControlError::OutputFailed)
        } else {
            Ok(state)
        }
    }

    fn update_state(
        &self,
        key: &PackageAudioControlKey,
        linear: f32,
        muted: bool,
        mut output_failed: impl FnMut() -> bool,
    ) -> Result<PackageAudioGainState, PackageAudioControlError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = Self::validate_entry(self, &mut entries, key)?;
        if output_failed() {
            return Err(PackageAudioControlError::OutputFailed);
        }
        let state = entry.gain.update_state(linear, muted)?;
        if output_failed() {
            Err(PackageAudioControlError::OutputFailed)
        } else {
            Ok(state)
        }
    }
}

/// Exact active lease for one sandbox-root isolate construction.
///
/// Core commits this lease only after the package entry loads successfully and
/// retains it with that isolate. Dropping a failed pending lease rolls back its
/// never-committed entry. Dropping a committed lease only deactivates its exact
/// generation; remembered state stays bounded to the session registration so a
/// successful version/reload replacement can reuse it.
pub struct PackageAudioScopeBinding {
    registry: Arc<PackageAudioRegistry>,
    root: PackageAudioRoot,
    generation: u64,
    committed: bool,
}

impl fmt::Debug for PackageAudioScopeBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackageAudioScopeBinding")
            .field("owner", &self.root.owner)
            .field("name", &self.root.name)
            .field("generation", &self.generation)
            .field("committed", &self.committed)
            .finish_non_exhaustive()
    }
}

impl PackageAudioScopeBinding {
    /// Publishes this exact root generation as controllable.
    ///
    /// # Errors
    ///
    /// Returns [`PackageAudioScopeError::SessionClosed`] without publishing
    /// the root if application/session admission sealed during construction.
    pub fn commit(&mut self) -> Result<(), PackageAudioScopeError> {
        if !self.committed {
            self.registry.commit(&self.root, self.generation)?;
            self.committed = true;
        }
        Ok(())
    }
}

impl Drop for PackageAudioScopeBinding {
    fn drop(&mut self) {
        self.registry
            .release(&self.root, self.generation, self.committed);
    }
}

/// Registration failure that returns the unconsumed mixer-session owner.
pub struct SessionAudioRegistrationFailure {
    error: SessionAudioRegistrationError,
    owner: MixerSessionOwner,
}

impl fmt::Debug for SessionAudioRegistrationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionAudioRegistrationFailure")
            .field("error", &self.error)
            .field("owner", &self.owner)
            .finish()
    }
}

impl SessionAudioRegistrationFailure {
    /// Stable reason registration was rejected.
    #[must_use]
    pub const fn error(&self) -> SessionAudioRegistrationError {
        self.error
    }

    /// Returns the exact unconsumed mixer-session owner.
    #[must_use]
    pub fn into_owner(self) -> MixerSessionOwner {
        self.owner
    }
}

impl ApplicationAudioRegistrar {
    /// Consumes one exact mixer session and publishes its scoped authorities.
    ///
    /// # Errors
    ///
    /// Returns the owner unchanged when application admission is sealed.
    pub fn register_session(
        &self,
        owner: MixerSessionOwner,
    ) -> Result<SessionAudioRegistration, SessionAudioRegistrationFailure> {
        let app_gate = lock_gate(&self.state.gate);
        if !app_gate.open {
            return Err(SessionAudioRegistrationFailure {
                error: SessionAudioRegistrationError::ApplicationSealed,
                owner,
            });
        }

        let session_id = owner.session_id();
        let gain = owner.gain_authority();
        let script = owner.script_bus();
        let native = owner.native_bus();
        let speech = owner.speech_bus();
        let session_gate = Arc::new(Mutex::new(LifecycleGate::new()));
        let Ok(generation) =
            NEXT_SCOPE_GENERATION.fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
        else {
            return Err(SessionAudioRegistrationFailure {
                error: SessionAudioRegistrationError::GenerationExhausted,
                owner,
            });
        };
        let session_key = SessionAudioControlKey {
            session_id,
            generation,
        };
        let permissions: Arc<dyn AudioPermissions> = Arc::new(SessionAudioPermissions {
            app: Arc::clone(&self.state),
            session: Arc::clone(&session_gate),
        });
        let force_emulated = Arc::new(AtomicBool::new(false));
        let output: Arc<dyn AudioOutputFactory> = Arc::new(GatedSessionAudioOutputFactory {
            app: Arc::clone(&self.state),
            session: Arc::clone(&session_gate),
            delegate: SessionAudioOutputFactory::with_force_emulated(
                script.clone(),
                Arc::clone(&force_emulated),
            ),
        });
        let package_audio = Arc::new(PackageAudioRegistry {
            session: session_key,
            bus: script,
            app: Arc::clone(&self.state),
            session_gate: Arc::clone(&session_gate),
            entries: Mutex::new(BTreeMap::new()),
        });
        let scope = SessionAudioScope {
            inner: Arc::new(SessionAudioScopeInner {
                session_id,
                generation,
                app: Arc::clone(&self.state),
                session_gate,
                permissions,
                output,
                package_audio: Some(package_audio),
                force_emulated: Some(force_emulated),
            }),
        };
        drop(app_gate);

        Ok(SessionAudioRegistration {
            owner: Some(owner),
            gain,
            native,
            speech,
            scope,
        })
    }

    /// Publishes one real Web Audio session scope without a physical mixer.
    ///
    /// The exact system-start cause is retained by the default-sink factory.
    /// `sinkId: "none"` remains a joinable system-silent endpoint; no mixer
    /// session, bus, slot, or physical format is created by this path.
    ///
    /// # Errors
    ///
    /// Returns a stable registration error when application admission is
    /// sealed or the non-wrapping scope identity space is exhausted.
    pub fn register_unavailable_session(
        &self,
        session_id: AudioSessionId,
        cause: UnavailableAudioOutputCause,
    ) -> Result<UnavailableSessionAudioRegistration, SessionAudioRegistrationError> {
        let app_gate = lock_gate(&self.state.gate);
        if !app_gate.open {
            return Err(SessionAudioRegistrationError::ApplicationSealed);
        }

        let session_gate = Arc::new(Mutex::new(LifecycleGate::new()));
        let permissions: Arc<dyn AudioPermissions> = Arc::new(SessionAudioPermissions {
            app: Arc::clone(&self.state),
            session: Arc::clone(&session_gate),
        });
        let output: Arc<dyn AudioOutputFactory> = Arc::new(GatedUnavailableAudioOutputFactory {
            app: Arc::clone(&self.state),
            session: Arc::clone(&session_gate),
            delegate: UnavailableSessionAudioOutputFactory::new(cause),
        });
        let Ok(generation) =
            NEXT_SCOPE_GENERATION.fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
        else {
            return Err(SessionAudioRegistrationError::GenerationExhausted);
        };
        let scope = SessionAudioScope {
            inner: Arc::new(SessionAudioScopeInner {
                session_id,
                generation,
                app: Arc::clone(&self.state),
                session_gate,
                permissions,
                output,
                package_audio: None,
                force_emulated: None,
            }),
        };
        drop(app_gate);

        Ok(UnavailableSessionAudioRegistration { scope })
    }
}

struct SessionAudioScopeInner {
    session_id: AudioSessionId,
    generation: u64,
    app: Arc<ApplicationAudioState>,
    session_gate: Arc<Mutex<LifecycleGate>>,
    permissions: Arc<dyn AudioPermissions>,
    output: Arc<dyn AudioOutputFactory>,
    package_audio: Option<Arc<PackageAudioRegistry>>,
    force_emulated: Option<Arc<AtomicBool>>,
}

/// Opaque cloneable session audio authority passed to script runtimes.
///
/// Clones share one application host, one exact session generation and its
/// playback gate. They cannot retire the mixer session or access Native/Speech
/// buses. `OfflineAudioContext` is still governed by shared and per-isolate
/// quotas but is device-free and does not consult lifecycle permissions in the
/// pinned `deno_audio` revision.
#[derive(Clone)]
pub struct SessionAudioScope {
    inner: Arc<SessionAudioScopeInner>,
}

impl fmt::Debug for SessionAudioScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionAudioScope")
            .field("session_id", &self.inner.session_id)
            .field("generation", &self.inner.generation)
            .finish_non_exhaustive()
    }
}

impl SessionAudioScope {
    /// Stable numeric session id used by core before thread publication.
    #[must_use]
    pub fn session_id(&self) -> u64 {
        self.inner.session_id.0
    }

    /// Permanently routes new default contexts in this exact scope to hosted
    /// emulated output. Mixer-free scopes are already emulated.
    pub fn force_emulated_output(&self) {
        if let Some(force_emulated) = self.inner.force_emulated.as_ref() {
            force_emulated.store(true, Ordering::Release);
        }
    }

    /// Builds fresh isolate options over this scope's unchanged authorities.
    #[must_use]
    pub fn extension_options(&self) -> AudioExtensionOptions {
        AudioExtensionOptions::new(Arc::clone(&self.inner.app.host))
            .permissions(Arc::clone(&self.inner.permissions))
            .output_factory(Arc::clone(&self.inner.output))
    }

    /// Builds isolate options bound to one versionless sandbox root.
    ///
    /// The returned lease must be committed only after the sandbox entry loads
    /// successfully and then retained for the exact isolate lifetime. Physical
    /// sessions receive a package-gained Script-bus factory; mixer-free
    /// unavailable sessions return their unchanged truthful factory and no
    /// package state or lease.
    ///
    /// # Errors
    ///
    /// Returns a typed identity, lifecycle, or bounded-capacity failure. It
    /// never falls back to the Main/session factory for a physical sandbox.
    pub fn extension_options_for_sandbox_root(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<(AudioExtensionOptions, Option<PackageAudioScopeBinding>), PackageAudioScopeError>
    {
        self.extension_options_for_sandbox_root_inner(owner, name, None)
    }

    /// Builds sandbox-root options and reports the first successfully
    /// observed online context for this isolate generation.
    ///
    /// The observer runs on the script thread, after output preparation has
    /// succeeded and before the context is returned to JavaScript. It must be
    /// quick and nonblocking; returning `true` confirms that the observation
    /// was accepted. A `false` result is retried after the next successful
    /// context preparation, so bounded-channel pressure cannot permanently
    /// hide a package that uses audio.
    pub fn extension_options_for_sandbox_root_with_usage_observer(
        &self,
        owner: &str,
        name: &str,
        on_audio_used: Arc<dyn Fn() -> bool + Send + Sync + 'static>,
    ) -> Result<(AudioExtensionOptions, Option<PackageAudioScopeBinding>), PackageAudioScopeError>
    {
        self.extension_options_for_sandbox_root_inner(owner, name, Some(on_audio_used))
    }

    fn extension_options_for_sandbox_root_inner(
        &self,
        owner: &str,
        name: &str,
        on_audio_used: Option<Arc<dyn Fn() -> bool + Send + Sync + 'static>>,
    ) -> Result<(AudioExtensionOptions, Option<PackageAudioScopeBinding>), PackageAudioScopeError>
    {
        let app = lock_gate(&self.inner.app.gate);
        let session = lock_gate(&self.inner.session_gate);
        if !app.open || !session.open {
            return Err(PackageAudioScopeError::SessionClosed);
        }
        let root = PackageAudioRoot::new(owner, name)?;
        let Some(registry) = &self.inner.package_audio else {
            let output = observe_audio_use(Arc::clone(&self.inner.output), on_audio_used);
            let options = AudioExtensionOptions::new(Arc::clone(&self.inner.app.host))
                .permissions(Arc::clone(&self.inner.permissions))
                .output_factory(output);
            return Ok((options, None));
        };
        let (gain, binding) = registry.bind_root(root)?;
        let output: Arc<dyn AudioOutputFactory> = Arc::new(GatedSessionAudioOutputFactory {
            app: Arc::clone(&self.inner.app),
            session: Arc::clone(&self.inner.session_gate),
            delegate: SessionAudioOutputFactory::with_package_gain(
                registry.bus.clone(),
                gain,
                Arc::clone(
                    self.inner
                        .force_emulated
                        .as_ref()
                        .expect("physical package scope shares the session emulation latch"),
                ),
            ),
        });
        let output = observe_audio_use(output, on_audio_used);
        let options = AudioExtensionOptions::new(Arc::clone(&self.inner.app.host))
            .permissions(Arc::clone(&self.inner.permissions))
            .output_factory(output);
        drop(session);
        drop(app);
        Ok((options, Some(binding)))
    }
}

struct ObservedAudioOutputFactory {
    delegate: Arc<dyn AudioOutputFactory>,
    observed: AtomicBool,
    on_audio_used: Arc<dyn Fn() -> bool + Send + Sync + 'static>,
}

impl AudioOutputFactory for ObservedAudioOutputFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        let prepared = self.delegate.prepare(request)?;
        if !self.observed.load(Ordering::Acquire) && (self.on_audio_used)() {
            self.observed.store(true, Ordering::Release);
        }
        Ok(prepared)
    }
}

fn observe_audio_use(
    delegate: Arc<dyn AudioOutputFactory>,
    observer: Option<Arc<dyn Fn() -> bool + Send + Sync + 'static>>,
) -> Arc<dyn AudioOutputFactory> {
    observer.map_or(delegate.clone(), |on_audio_used| {
        Arc::new(ObservedAudioOutputFactory {
            delegate,
            observed: AtomicBool::new(false),
            on_audio_used,
        })
    })
}

/// Unique lifetime owner for one registered mixer-session generation.
pub struct SessionAudioRegistration {
    owner: Option<MixerSessionOwner>,
    gain: MixerSessionGainAuthority,
    native: MixerNativeBusHandle,
    speech: MixerSpeechBusHandle,
    scope: SessionAudioScope,
}

impl fmt::Debug for SessionAudioRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionAudioRegistration")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl SessionAudioRegistration {
    /// Permanently routes new default contexts in this exact unpublished/live
    /// generation to hosted emulated output. Existing contexts are unchanged.
    pub fn force_emulated_output(&self) {
        self.scope
            .inner
            .force_emulated
            .as_ref()
            .expect("physical registration owns an emulation latch")
            .store(true, Ordering::Release);
    }

    /// Opaque application-control identity for this exact registration.
    #[must_use]
    pub fn control_key(&self) -> SessionAudioControlKey {
        SessionAudioControlKey {
            session_id: self.scope.inner.session_id,
            generation: self.scope.inner.generation,
        }
    }

    /// Applies a remembered linear session gain on the mixer owner.
    ///
    /// # Errors
    ///
    /// Returns the lower bounded-control outcome without exposing its raw
    /// authority to the application or script runtime.
    pub fn set_gain_linear(&self, linear: f32) -> Result<MixerGainState, MixerControlError> {
        self.gain.set_linear(linear)
    }

    /// Applies the independent session mute on the mixer owner.
    ///
    /// # Errors
    ///
    /// Returns the lower bounded-control outcome without exposing its raw
    /// authority to the application or script runtime.
    pub fn set_gain_muted(&self, muted: bool) -> Result<MixerGainState, MixerControlError> {
        self.gain.set_muted(muted)
    }

    /// Atomically replaces remembered linear gain and mute for this session.
    pub fn set_gain_state(
        &self,
        linear: f32,
        muted: bool,
    ) -> Result<MixerGainState, MixerControlError> {
        self.gain.set_state(linear, muted)
    }

    /// Atomically stage a complete persisted session and sandbox-root policy.
    ///
    /// Package identities and gains are preflighted before the mixer changes.
    /// The session gain is published as one owner command. If a later output
    /// failure is observed, package state and the previously acknowledged
    /// session state are restored on a best-effort basis before the failure is
    /// returned. Registry capacity rejects only excess package scopes; it does
    /// not reject the owning terminal session.
    pub fn stage_gain_policy(
        &self,
        linear: f32,
        muted: bool,
        previous_linear: f32,
        previous_muted: bool,
        packages: impl IntoIterator<Item = (Arc<str>, Arc<str>, f32, bool)>,
    ) -> Result<(), SessionAudioPolicyError> {
        let linear = validate_package_gain(linear)
            .map_err(|_| SessionAudioPolicyError::Mixer(MixerControlError::InvalidGain))?;
        let previous_linear = validate_package_gain(previous_linear)
            .map_err(|_| SessionAudioPolicyError::Mixer(MixerControlError::InvalidGain))?;
        let app_gate = lock_gate(&self.scope.inner.app.gate);
        let session_gate = lock_gate(&self.scope.inner.session_gate);
        if !app_gate.open || !session_gate.open {
            return Err(SessionAudioPolicyError::Package(
                PackageAudioScopeError::SessionClosed,
            ));
        }
        let registry =
            self.scope
                .inner
                .package_audio
                .as_ref()
                .ok_or(SessionAudioPolicyError::Package(
                    PackageAudioScopeError::SessionClosed,
                ))?;
        let prepared = registry
            .prepare_policy(packages)
            .map_err(SessionAudioPolicyError::Package)?;
        let state = self
            .gain
            .set_state(linear, muted)
            .map_err(SessionAudioPolicyError::Mixer)?;
        debug_assert_eq!(state.linear(), linear);
        debug_assert_eq!(state.is_muted(), muted);
        let rollback = registry.apply_prepared_policy(prepared);
        if self.gain.output_failure().is_some() {
            registry.rollback_policy(rollback);
            let _ = self.gain.set_state(previous_linear, previous_muted);
            Err(SessionAudioPolicyError::Mixer(
                MixerControlError::OwnerStopped,
            ))
        } else {
            Ok(())
        }
    }

    /// Seed one versionless sandbox root before runtime construction.
    ///
    /// The dormant entry consumes only bounded metadata. Its first output
    /// factory observes this state before package top-level evaluation.
    ///
    /// # Errors
    ///
    /// Returns a typed identity, capacity, lifecycle, or gain failure. The
    /// Existing active/pending roots update their shared snapshot in place;
    /// absent roots allocate dormant bounded metadata for their first bind.
    pub fn seed_package_gain(
        &self,
        owner: &str,
        name: &str,
        linear: f32,
        muted: bool,
    ) -> Result<PackageAudioGainState, PackageAudioScopeError> {
        let _app_gate = lock_gate(&self.scope.inner.app.gate);
        let session_gate = lock_gate(&self.scope.inner.session_gate);
        if !session_gate.open {
            return Err(PackageAudioScopeError::SessionClosed);
        }
        self.scope
            .inner
            .package_audio
            .as_ref()
            .ok_or(PackageAudioScopeError::SessionClosed)?
            .seed(owner, name, linear, muted)
    }

    /// Looks up the exact active controller identity for one sandbox root.
    ///
    /// Package identities are ASCII-folded and versionless. A committed entry
    /// remains remembered across reload, but is controllable only while its
    /// exact isolate-generation lease is active.
    ///
    /// # Errors
    ///
    /// Returns a typed closed-session or unknown/inactive-package result.
    pub fn package_control_key(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<PackageAudioControlKey, PackageAudioControlError> {
        let app_gate = lock_gate(&self.scope.inner.app.gate);
        let session_gate = lock_gate(&self.scope.inner.session_gate);
        if !app_gate.open || !session_gate.open {
            return Err(PackageAudioControlError::SessionClosed);
        }
        self.scope
            .inner
            .package_audio
            .as_ref()
            .ok_or(PackageAudioControlError::UnknownPackage)?
            .control_key(owner, name)
    }

    /// Applies a remembered linear gain to one exact active sandbox root.
    ///
    /// # Errors
    ///
    /// Returns typed stale/closed/invalid/failure outcomes without mutating a
    /// different session or root generation.
    pub fn set_package_gain_linear(
        &self,
        key: &PackageAudioControlKey,
        linear: f32,
    ) -> Result<PackageAudioGainState, PackageAudioControlError> {
        let linear = validate_package_gain(linear)?;
        let app_gate = lock_gate(&self.scope.inner.app.gate);
        let session_gate = lock_gate(&self.scope.inner.session_gate);
        if !app_gate.open || !session_gate.open {
            return Err(PackageAudioControlError::SessionClosed);
        }
        let registry = self
            .scope
            .inner
            .package_audio
            .as_ref()
            .ok_or(PackageAudioControlError::UnknownPackage)?;
        registry.update_linear(key, linear, || self.gain.output_failure().is_some())
    }

    /// Applies mute independently from remembered linear gain for one root.
    ///
    /// # Errors
    ///
    /// Returns typed stale/closed/failure outcomes without mutating a
    /// different session or root generation.
    pub fn set_package_gain_muted(
        &self,
        key: &PackageAudioControlKey,
        muted: bool,
    ) -> Result<PackageAudioGainState, PackageAudioControlError> {
        let app_gate = lock_gate(&self.scope.inner.app.gate);
        let session_gate = lock_gate(&self.scope.inner.session_gate);
        if !app_gate.open || !session_gate.open {
            return Err(PackageAudioControlError::SessionClosed);
        }
        let registry = self
            .scope
            .inner
            .package_audio
            .as_ref()
            .ok_or(PackageAudioControlError::UnknownPackage)?;
        registry.update_muted(key, muted, || self.gain.output_failure().is_some())
    }

    /// Atomically replaces remembered linear gain and mute for one exact,
    /// active sandbox-root generation.
    pub fn set_package_gain_state(
        &self,
        key: &PackageAudioControlKey,
        linear: f32,
        muted: bool,
    ) -> Result<PackageAudioGainState, PackageAudioControlError> {
        let app_gate = lock_gate(&self.scope.inner.app.gate);
        let session_gate = lock_gate(&self.scope.inner.session_gate);
        if !app_gate.open || !session_gate.open {
            return Err(PackageAudioControlError::SessionClosed);
        }
        let registry = self
            .scope
            .inner
            .package_audio
            .as_ref()
            .ok_or(PackageAudioControlError::UnknownPackage)?;
        registry.update_state(key, linear, muted, || self.gain.output_failure().is_some())
    }

    /// Exact first-writer process-output failure observed by this authority.
    #[must_use]
    pub fn gain_output_failure(&self) -> Option<MixerOutputFailure> {
        self.gain.output_failure()
    }

    /// Cloneable opaque authority for this exact session generation.
    #[must_use]
    pub fn scope(&self) -> SessionAudioScope {
        self.scope.clone()
    }

    /// Exact Native-bus authority retained for the UI/application owner.
    #[must_use]
    pub fn native_bus(&self) -> MixerNativeBusHandle {
        self.native.clone()
    }

    /// Exact Speech-bus authority retained for the UI/application owner.
    #[must_use]
    pub fn speech_bus(&self) -> MixerSpeechBusHandle {
        self.speech.clone()
    }

    /// Absorbingly rejects new online output preparation for this session.
    ///
    /// The application gate is acquired before the session gate, matching the
    /// permission and final factory checks. Returning therefore proves that an
    /// already-admitted `prepare` delegation has finished and no later one can
    /// begin. The mixer-session owner remains retained for explicit runtime
    /// join followed by [`retire`](Self::retire).
    ///
    /// Returns `true` only for the transition from open to sealed.
    pub fn seal(&mut self) -> bool {
        self.seal_session()
    }

    /// Seals online preparation before beginning exact mixer retirement.
    ///
    /// # Panics
    ///
    /// Panics only if the registration's private unique-owner invariant was
    /// corrupted inside this crate.
    #[must_use = "session retirement is complete only after awaiting this receipt"]
    pub fn retire(mut self) -> MixerSessionRetirement {
        let _ = self.seal();
        self.owner
            .take()
            .expect("session audio registration retires exactly once")
            .retire()
    }

    fn seal_session(&self) -> bool {
        let _app_gate = lock_gate(&self.scope.inner.app.gate);
        let mut session_gate = lock_gate(&self.scope.inner.session_gate);
        let was_open = session_gate.open;
        session_gate.open = false;
        was_open
    }
}

impl Drop for SessionAudioRegistration {
    fn drop(&mut self) {
        let _ = self.seal();
        // MixerSessionOwner::drop begins cancellation-independent retirement.
        self.owner.take();
    }
}

/// Unique mixer-free lifetime owner for one unavailable session generation.
pub struct UnavailableSessionAudioRegistration {
    scope: SessionAudioScope,
}

impl fmt::Debug for UnavailableSessionAudioRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UnavailableSessionAudioRegistration")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// Exact logical close receipt for a session that never owned mixer state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnavailableSessionAudioRetirement {
    session_id: u64,
    generation: u64,
}

impl UnavailableSessionAudioRetirement {
    /// The exact mixer-free session identity whose admission was sealed.
    #[must_use]
    pub const fn session_id(self) -> u64 {
        self.session_id
    }

    /// Exact non-wrapping registration generation that was sealed.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

impl UnavailableSessionAudioRegistration {
    /// Opaque application-control identity for this mixer-free registration.
    #[must_use]
    pub fn control_key(&self) -> SessionAudioControlKey {
        SessionAudioControlKey {
            session_id: self.scope.inner.session_id,
            generation: self.scope.inner.generation,
        }
    }

    /// Cloneable opaque authority for this exact mixer-free generation.
    #[must_use]
    pub fn scope(&self) -> SessionAudioScope {
        self.scope.clone()
    }

    /// Absorbingly rejects new online output preparation for this session.
    pub fn seal(&mut self) -> bool {
        self.seal_session()
    }

    /// Seals this exact mixer-free generation and returns its logical receipt.
    #[must_use]
    pub fn retire(mut self) -> UnavailableSessionAudioRetirement {
        let _ = self.seal();
        UnavailableSessionAudioRetirement {
            session_id: self.scope.session_id(),
            generation: self.scope.inner.generation,
        }
    }

    fn seal_session(&self) -> bool {
        let _app_gate = lock_gate(&self.scope.inner.app.gate);
        let mut session_gate = lock_gate(&self.scope.inner.session_gate);
        let was_open = session_gate.open;
        session_gate.open = false;
        was_open
    }
}

impl Drop for UnavailableSessionAudioRegistration {
    fn drop(&mut self) {
        let _ = self.seal();
    }
}

struct SessionAudioPermissions {
    app: Arc<ApplicationAudioState>,
    session: Arc<Mutex<LifecycleGate>>,
}

impl AudioPermissions for SessionAudioPermissions {
    fn check_playback(&self, api_name: &'static str) -> Result<(), JsErrorBox> {
        let app = lock_gate(&self.app.gate);
        let session = lock_gate(&self.session);
        if app.open && session.open {
            Ok(())
        } else {
            Err(JsErrorBox::generic(format!(
                "InvalidStateError: {api_name} playback admission is closed"
            )))
        }
    }

    fn check_capture(&self, api_name: &'static str) -> Result<(), JsErrorBox> {
        Err(JsErrorBox::generic(format!(
            "NotAllowedError: {api_name} audio capture is not permitted"
        )))
    }
}

struct GatedSessionAudioOutputFactory {
    app: Arc<ApplicationAudioState>,
    session: Arc<Mutex<LifecycleGate>>,
    delegate: SessionAudioOutputFactory,
}

struct GatedUnavailableAudioOutputFactory {
    app: Arc<ApplicationAudioState>,
    session: Arc<Mutex<LifecycleGate>>,
    delegate: UnavailableSessionAudioOutputFactory,
}

impl AudioOutputFactory for GatedUnavailableAudioOutputFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        let app = lock_gate(&self.app.gate);
        let session = lock_gate(&self.session);
        if !app.open || !session.open {
            return Err(output_error(
                AudioOutputErrorKind::Shutdown,
                "Smudgy Web Audio playback admission is closed",
            ));
        }
        #[cfg(test)]
        if let Some(hook) = self
            .app
            .prepare_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            hook.entered.wait();
            hook.release.wait();
        }
        self.delegate.prepare(request)
    }
}

/// Hardware-neutral diagnostic retained by a mixer-free default-sink route.
#[derive(Clone, Debug)]
pub struct UnavailableAudioOutputCause(Arc<str>);

impl UnavailableAudioOutputCause {
    #[must_use]
    pub fn new(detail: impl Into<Arc<str>>) -> Self {
        Self(detail.into())
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug)]
struct UnavailableSessionAudioOutputFactory {
    _cause: UnavailableAudioOutputCause,
    silent: SilentAudioOutput,
}

impl UnavailableSessionAudioOutputFactory {
    const fn new(cause: UnavailableAudioOutputCause) -> Self {
        Self {
            _cause: cause,
            silent: SilentAudioOutput::new(),
        }
    }
}

impl AudioOutputFactory for UnavailableSessionAudioOutputFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        match request.sink_id() {
            "" | "none" => self.silent.prepare(request),
            _ => Err(output_error(
                AudioOutputErrorKind::NotSupported,
                "Smudgy Web Audio supports only the default and none output sinks",
            )),
        }
    }
}

impl AudioOutputFactory for GatedSessionAudioOutputFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        let app = lock_gate(&self.app.gate);
        let session = lock_gate(&self.session);
        if !app.open || !session.open {
            return Err(output_error(
                AudioOutputErrorKind::Shutdown,
                "Smudgy Web Audio playback admission is closed",
            ));
        }
        #[cfg(test)]
        if let Some(hook) = self
            .app
            .prepare_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            hook.entered.wait();
            hook.release.wait();
        }
        // Keep both guards across the last check and delegate preparation so
        // application/session sealing cannot pass the permission check and
        // race mixer mutation.
        self.delegate.prepare(request)
    }
}

#[cfg(test)]
struct TestPrepareHook {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

/// Routes one session's Web Audio contexts to its shared Script bus or to a
/// private silent endpoint.
///
/// The default sink consumes one bounded Script-bus input. The exact `"none"`
/// sink delegates to [`SilentAudioOutput`]'s joinable silent implementation and
/// never mutates mixer capacity. Other sink identifiers are rejected before
/// either delegate is invoked.
#[derive(Clone, Debug)]
pub struct SessionAudioOutputFactory {
    script: ScriptBusAudioOutputFactory,
    silent: SilentAudioOutput,
    force_emulated: Arc<AtomicBool>,
}

impl SessionAudioOutputFactory {
    /// Binds context construction to one session's scoped Script bus.
    #[must_use]
    pub fn new(bus: MixerScriptBusHandle) -> Self {
        Self::with_force_emulated(bus, Arc::new(AtomicBool::new(false)))
    }

    fn with_force_emulated(bus: MixerScriptBusHandle, force_emulated: Arc<AtomicBool>) -> Self {
        Self {
            script: ScriptBusAudioOutputFactory::new(bus),
            silent: SilentAudioOutput::new(),
            force_emulated,
        }
    }

    fn with_package_gain(
        bus: MixerScriptBusHandle,
        gain: Arc<PackageGainState>,
        force_emulated: Arc<AtomicBool>,
    ) -> Self {
        Self {
            script: ScriptBusAudioOutputFactory::with_package_gain(bus, gain),
            silent: SilentAudioOutput::new(),
            force_emulated,
        }
    }

    #[cfg(test)]
    fn with_failure_hook(bus: MixerScriptBusHandle, hook: Arc<TestFailureHook>) -> Self {
        Self {
            script: ScriptBusAudioOutputFactory::with_failure_hook(bus, hook),
            silent: SilentAudioOutput::new(),
            force_emulated: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl AudioOutputFactory for SessionAudioOutputFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        match request.sink_id() {
            "" => {
                // A generation forced emulated before construction has no
                // audible hardware contract. Preserve the requested logical
                // context rate instead of negotiating against the fixed
                // 48 kHz process mixer. The later start-time latch check still
                // covers a force transition after physical preparation.
                if self.force_emulated.load(Ordering::Acquire) {
                    return self.silent.prepare(request);
                }
                if self.script.output_failure().is_some() {
                    return self.silent.prepare(request);
                }
                match self.script.prepare_parts(
                    request.sink_id(),
                    request.requested_sample_rate(),
                    request.number_of_channels(),
                ) {
                    Ok(physical) => {
                        let silent = self
                            .silent
                            .prepare_with_config(request, physical.config())?;
                        Ok(Box::new(PhysicalOrEmulatedPreparedOutput {
                            physical: Some(physical),
                            silent: Some(silent),
                            force_emulated: Arc::clone(&self.force_emulated),
                        }))
                    }
                    Err(error) if error.kind() == AudioOutputErrorKind::DeviceUnavailable => {
                        self.silent.prepare(request)
                    }
                    Err(error) => Err(error),
                }
            }
            "none" => self.silent.prepare(request),
            _ => Err(output_error(
                AudioOutputErrorKind::NotSupported,
                "Smudgy Web Audio supports only the default and none output sinks",
            )),
        }
    }
}

/// A default-sink endpoint that prefers the session mixer but can hand the
/// still-unpublished callback to the emulated endpoint when physical start
/// loses its owner after successful preparation.
struct PhysicalOrEmulatedPreparedOutput {
    physical: Option<Box<ScriptBusPreparedOutput>>,
    silent: Option<Box<dyn PreparedAudioOutput>>,
    force_emulated: Arc<AtomicBool>,
}

impl PreparedAudioOutput for PhysicalOrEmulatedPreparedOutput {
    fn config(&self) -> &AudioOutputConfig {
        self.physical
            .as_ref()
            .expect("physical-or-emulated output is single-use")
            .config()
    }

    fn start(
        mut self: Box<Self>,
        callback: AudioRenderCallback,
        events: AudioOutputEventSink,
    ) -> Result<Box<dyn RunningAudioOutput>, AudioOutputStartFailure> {
        let physical = self
            .physical
            .take()
            .expect("physical-or-emulated output owns one physical endpoint");
        let silent = self
            .silent
            .take()
            .expect("physical-or-emulated output owns one silent endpoint");
        if self.force_emulated.load(Ordering::Acquire) {
            let cleanup = physical.abort();
            return match silent.start(callback, events) {
                Ok(running) => Ok(Box::new(RunningWithPriorCleanup::new(running, cleanup))),
                Err(failure) => {
                    let (error, silent_cleanup) = failure.into_parts();
                    Err(AudioOutputStartFailure::new(
                        error,
                        combine_endpoint_shutdown(cleanup, silent_cleanup),
                    ))
                }
            };
        }
        match physical.start_recoverable(callback, events.clone()) {
            RecoverablePhysicalStart::Running(running) => {
                let unused_silent = silent.abort();
                Ok(Box::new(RunningWithPriorCleanup::new(
                    running,
                    unused_silent,
                )))
            }
            RecoverablePhysicalStart::DeviceUnavailable { callback, cleanup } => {
                match silent.start(callback, events) {
                    Ok(running) => Ok(Box::new(RunningWithPriorCleanup::new(running, cleanup))),
                    Err(failure) => {
                        let (error, silent_cleanup) = failure.into_parts();
                        Err(AudioOutputStartFailure::new(
                            error,
                            combine_endpoint_shutdown(cleanup, silent_cleanup),
                        ))
                    }
                }
            }
            RecoverablePhysicalStart::Failed(failure) => {
                let unused_silent = silent.abort();
                let (error, physical_cleanup) = failure.into_parts();
                Err(AudioOutputStartFailure::new(
                    error,
                    combine_endpoint_shutdown(physical_cleanup, unused_silent),
                ))
            }
        }
    }

    fn abort(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        let physical = self
            .physical
            .take()
            .expect("physical-or-emulated output owns one physical endpoint");
        let silent = self
            .silent
            .take()
            .expect("physical-or-emulated output owns one silent endpoint");
        combine_endpoint_shutdown(physical.abort(), silent.abort())
    }
}

/// Creates one private hosted Web Audio output on a session's Script mixer bus.
///
/// Clones retain only the scoped bus control handle. The application-owned
/// mixer service and its physical backend remain outside this adapter.
#[derive(Clone, Debug)]
pub struct ScriptBusAudioOutputFactory {
    bus: MixerScriptBusHandle,
    package_gain: Option<Arc<PackageGainState>>,
    #[cfg(test)]
    render_hook: Option<Arc<TestRenderHook>>,
    #[cfg(test)]
    panic_start: bool,
    #[cfg(test)]
    failure_hook: Option<Arc<TestFailureHook>>,
}

impl ScriptBusAudioOutputFactory {
    /// Binds hosted Web Audio context construction to one scoped Script bus.
    #[must_use]
    pub const fn new(bus: MixerScriptBusHandle) -> Self {
        Self {
            bus,
            package_gain: None,
            #[cfg(test)]
            render_hook: None,
            #[cfg(test)]
            panic_start: false,
            #[cfg(test)]
            failure_hook: None,
        }
    }

    fn output_failure(&self) -> Option<MixerOutputFailure> {
        self.bus.output_failure()
    }

    fn with_package_gain(bus: MixerScriptBusHandle, package_gain: Arc<PackageGainState>) -> Self {
        Self {
            bus,
            package_gain: Some(package_gain),
            #[cfg(test)]
            render_hook: None,
            #[cfg(test)]
            panic_start: false,
            #[cfg(test)]
            failure_hook: None,
        }
    }

    #[cfg(test)]
    fn with_render_hook(bus: MixerScriptBusHandle, render_hook: Arc<TestRenderHook>) -> Self {
        Self {
            bus,
            package_gain: None,
            render_hook: Some(render_hook),
            panic_start: false,
            failure_hook: None,
        }
    }

    #[cfg(test)]
    fn with_start_panic(bus: MixerScriptBusHandle, render_hook: Arc<TestRenderHook>) -> Self {
        Self {
            bus,
            package_gain: None,
            render_hook: Some(render_hook),
            panic_start: true,
            failure_hook: None,
        }
    }

    #[cfg(test)]
    fn with_failure_hook(bus: MixerScriptBusHandle, hook: Arc<TestFailureHook>) -> Self {
        Self {
            bus,
            package_gain: None,
            render_hook: None,
            panic_start: false,
            failure_hook: Some(hook),
        }
    }

    #[cfg(test)]
    fn preboxed_input(&self, callback: Arc<RecoverableRenderCallback>) -> Box<CallbackMixerInput> {
        Box::new(CallbackMixerInput::with_render_hook(
            callback,
            self.render_hook.clone(),
            self.package_gain.clone(),
            self.failure_hook.clone(),
        ))
    }

    fn prepare_parts(
        &self,
        sink_id: &str,
        requested_sample_rate: Option<f32>,
        number_of_channels: usize,
    ) -> Result<Box<ScriptBusPreparedOutput>, AudioOutputError> {
        if !sink_id.is_empty() {
            return Err(output_error(
                AudioOutputErrorKind::NotSupported,
                "the shared Smudgy mixer supports only the default output sink",
            ));
        }
        if number_of_channels != CHANNELS {
            return Err(output_error(
                AudioOutputErrorKind::NotSupported,
                "the shared Smudgy mixer requires stereo output",
            ));
        }

        let mixer_format = self.bus.format().map_err(control_error)?;
        // Web Audio publishes sample rates as f32 while the mixer publishes an
        // integer device rate. AudioRenderFormat validates the converted value.
        #[allow(clippy::cast_precision_loss)]
        let sample_rate = mixer_format.sample_rate() as f32;
        #[allow(clippy::float_cmp)] // the output contract requires an exact requested rate
        if requested_sample_rate.is_some_and(|requested| requested != sample_rate) {
            return Err(output_error(
                AudioOutputErrorKind::NotSupported,
                "the requested sample rate does not match the shared Smudgy mixer",
            ));
        }
        if mixer_format.number_of_channels() != CHANNELS
            || mixer_format.max_frames_per_callback() != FRAMES
        {
            return Err(output_error(
                AudioOutputErrorKind::BackendSpecific,
                "the shared Smudgy mixer published an incompatible logical format",
            ));
        }

        let format = AudioRenderFormat::new(sample_rate, CHANNELS, FRAMES)?;
        let config = AudioOutputConfig::new(
            format,
            String::new(),
            128.0 / f64::from(mixer_format.sample_rate()),
        )?;

        // Every heap object, including the outer trait object, callback
        // scratch, and running owner, exists before the bounded reservation
        // mutates capacity. Reservation insertion is then infallible.
        let callback = Arc::new(RecoverableRenderCallback::new());
        #[cfg(test)]
        let input = self.preboxed_input(Arc::clone(&callback));
        #[cfg(not(test))]
        let input = Box::new(CallbackMixerInput::new(
            Arc::clone(&callback),
            self.package_gain.clone(),
        ));
        let mut prepared = Box::new(ScriptBusPreparedOutput {
            config,
            callback,
            reservation: None,
            input: Some(input),
            running: Some(Box::new(ScriptBusRunningOutput {
                input: None,
                #[cfg(test)]
                render_hook: self.render_hook.clone(),
            })),
            #[cfg(test)]
            panic_start: self.panic_start,
        });
        let reservation = self.bus.try_reserve_input().map_err(control_error)?;
        prepared.reservation = Some(reservation);
        Ok(prepared)
    }
}

impl AudioOutputFactory for ScriptBusAudioOutputFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        self.prepare_parts(
            request.sink_id(),
            request.requested_sample_rate(),
            request.number_of_channels(),
        )
        .map(|prepared| prepared as Box<dyn PreparedAudioOutput>)
    }
}

struct ScriptBusPreparedOutput {
    config: AudioOutputConfig,
    callback: Arc<RecoverableRenderCallback>,
    reservation: Option<MixerInputReservation>,
    input: Option<Box<CallbackMixerInput>>,
    running: Option<Box<ScriptBusRunningOutput>>,
    #[cfg(test)]
    panic_start: bool,
}

impl PreparedAudioOutput for ScriptBusPreparedOutput {
    fn config(&self) -> &AudioOutputConfig {
        &self.config
    }

    fn start(
        self: Box<Self>,
        callback: AudioRenderCallback,
        events: AudioOutputEventSink,
    ) -> Result<Box<dyn RunningAudioOutput>, AudioOutputStartFailure> {
        match self.start_inner(callback, events, false) {
            RecoverablePhysicalStart::Running(running) => Ok(running),
            RecoverablePhysicalStart::Failed(failure) => Err(failure),
            RecoverablePhysicalStart::DeviceUnavailable { .. } => {
                unreachable!("direct Script bus starts do not recover their callback")
            }
        }
    }

    fn abort(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        let reservation = self
            .reservation
            .take()
            .expect("prepared output owns one reservation");
        // This shell never received a callback, so its synchronous destruction
        // is not part of the proof-bearing retirement.
        self.input.take();
        self.running.take();
        endpoint_shutdown(reservation.abort())
    }
}

enum RecoverablePhysicalStart {
    Running(Box<dyn RunningAudioOutput>),
    DeviceUnavailable {
        callback: AudioRenderCallback,
        cleanup: AudioOutputEndpointShutdown,
    },
    Failed(AudioOutputStartFailure),
}

impl ScriptBusPreparedOutput {
    fn start_recoverable(
        self: Box<Self>,
        callback: AudioRenderCallback,
        events: AudioOutputEventSink,
    ) -> RecoverablePhysicalStart {
        self.start_inner(callback, events, true)
    }

    fn start_inner(
        mut self: Box<Self>,
        callback: AudioRenderCallback,
        events: AudioOutputEventSink,
        recover_device_unavailable: bool,
    ) -> RecoverablePhysicalStart {
        let reservation = self
            .reservation
            .take()
            .expect("prepared output owns one reservation");
        let mut input = self.input.take().expect("prepared output owns one input");
        let running = self
            .running
            .take()
            .expect("prepared output owns one preboxed running endpoint");
        debug_assert!(running.input.is_none());
        let expected = self.config.format();
        let actual = callback.format();
        self.callback.install(callback);
        input.install(events.clone());
        let reporter = Arc::clone(&input.events);
        let source: Box<dyn MixerInput> = input;

        if actual != expected {
            reporter.disarm();
            let _ = events.report_endpoint_death(AudioOutputDeathReason::CallbackProtocolViolation);
            return RecoverablePhysicalStart::Failed(abort_start(
                output_error(
                    AudioOutputErrorKind::BackendSpecific,
                    "the Web Audio callback format did not match the prepared mixer endpoint",
                ),
                reservation,
                source,
            ));
        }

        #[cfg(test)]
        let panic_start = self.panic_start;
        let mut pending_start = Some((reservation, source));
        let start = panic::catch_unwind(AssertUnwindSafe(|| {
            #[cfg(test)]
            assert!(!panic_start, "injected mixer start panic");
            let (reservation, source) = pending_start
                .take()
                .expect("start authorities are consumed exactly once");
            reservation.start_preboxed(source)
        }));
        match start {
            Ok(Ok(input)) => {
                let running = publish_running(running, input);
                if self.callback.try_activate() {
                    reporter.arm();
                    RecoverablePhysicalStart::Running(running)
                } else if recover_device_unavailable {
                    reporter.disarm();
                    let cleanup = running.shutdown();
                    let callback = self.callback.take_after_failed_start();
                    RecoverablePhysicalStart::DeviceUnavailable { callback, cleanup }
                } else {
                    // Only the composite default route is authorized to move
                    // an unpublished callback to emulated output.
                    reporter.arm();
                    RecoverablePhysicalStart::Running(running)
                }
            }
            Ok(Err(MixerInputStartFailure::Rejected(failure))) => {
                let (error, reservation, source) = failure.into_parts();
                if recover_device_unavailable && error == MixerControlError::OwnerStopped {
                    reporter.disarm();
                    let failure = abort_start(control_error(error), reservation, source);
                    let (_, cleanup) = failure.into_parts();
                    let callback = self.callback.take_after_failed_start();
                    return RecoverablePhysicalStart::DeviceUnavailable { callback, cleanup };
                }
                let reason = if error == MixerControlError::OwnerStopped {
                    AudioOutputDeathReason::FactoryShutdown
                } else {
                    AudioOutputDeathReason::BackendFailure
                };
                reporter.disarm();
                let _ = events.report_endpoint_death(reason);
                RecoverablePhysicalStart::Failed(abort_start(
                    control_error(error),
                    reservation,
                    source,
                ))
            }
            Ok(Err(MixerInputStartFailure::Cleanup { error, shutdown })) => {
                if recover_device_unavailable && error == MixerControlError::OwnerStopped {
                    reporter.disarm();
                    let callback = self.callback.take_after_failed_start();
                    return RecoverablePhysicalStart::DeviceUnavailable {
                        callback,
                        cleanup: endpoint_shutdown(shutdown),
                    };
                }
                reporter.disarm();
                let _ = events.report_endpoint_death(AudioOutputDeathReason::BackendFailure);
                RecoverablePhysicalStart::Failed(AudioOutputStartFailure::new(
                    output_error(
                        AudioOutputErrorKind::BackendSpecific,
                        "the mixer could not publish the installed Web Audio callback",
                    ),
                    endpoint_shutdown(shutdown),
                ))
            }
            Err(payload) => {
                std::mem::forget(payload);
                reporter.disarm();
                let _ = events.report_endpoint_death(AudioOutputDeathReason::BackendFailure);
                if let Some((reservation, source)) = pending_start.take() {
                    return RecoverablePhysicalStart::Failed(panicked_start_with_owned_cleanup(
                        reservation,
                        source,
                    ));
                }
                // A panic after the authorities crossed into smudgy_audio can
                // no longer recover their exact cleanup observer. Their Drop
                // paths remain fail-closed, but this result must be explicitly
                // unconfirmed.
                RecoverablePhysicalStart::Failed(AudioOutputStartFailure::new(
                    output_error(
                        AudioOutputErrorKind::BackendSpecific,
                        "the mixer start transaction panicked",
                    ),
                    AudioOutputEndpointShutdown::ready(Err(output_error(
                        AudioOutputErrorKind::Shutdown,
                        "mixer start cleanup could not be confirmed after a panic",
                    ))),
                ))
            }
        }
    }
}

struct CallbackMixerInput {
    callback: Arc<RecoverableRenderCallback>,
    events: Arc<OutputFailureReporter>,
    scratch: [f32; INTERLEAVED_SAMPLES],
    package_gain: Option<Arc<PackageGainState>>,
    #[cfg(test)]
    render_hook: Option<Arc<TestRenderHook>>,
}

impl CallbackMixerInput {
    #[cfg(not(test))]
    fn new(
        callback: Arc<RecoverableRenderCallback>,
        package_gain: Option<Arc<PackageGainState>>,
    ) -> Self {
        let events = Arc::new(OutputFailureReporter::new(Arc::clone(&callback)));
        Self {
            callback,
            events,
            scratch: [0.0; INTERLEAVED_SAMPLES],
            package_gain,
            #[cfg(test)]
            render_hook: None,
        }
    }

    #[cfg(test)]
    fn with_render_hook(
        callback: Arc<RecoverableRenderCallback>,
        render_hook: Option<Arc<TestRenderHook>>,
        package_gain: Option<Arc<PackageGainState>>,
        failure_hook: Option<Arc<TestFailureHook>>,
    ) -> Self {
        let events = Arc::new(OutputFailureReporter::new(
            Arc::clone(&callback),
            failure_hook,
        ));
        Self {
            callback,
            events,
            scratch: [0.0; INTERLEAVED_SAMPLES],
            package_gain,
            render_hook,
        }
    }

    fn install(&mut self, events: AudioOutputEventSink) {
        self.events.install(events);
    }
}

struct RecoverableRenderCallback {
    callback: UnsafeCell<Option<AudioRenderCallback>>,
    phase: AtomicU8,
}

const CALLBACK_EMPTY: u8 = 0;
const CALLBACK_PENDING: u8 = 1;
const CALLBACK_ACTIVE: u8 = 2;
const CALLBACK_RECOVERABLE: u8 = 3;
const CALLBACK_TAKEN: u8 = 4;

impl RecoverableRenderCallback {
    const fn new() -> Self {
        Self {
            callback: UnsafeCell::new(None),
            phase: AtomicU8::new(CALLBACK_EMPTY),
        }
    }

    fn install(&self, callback: AudioRenderCallback) {
        // SAFETY: installation happens before the source can be published to
        // the mixer. The cell begins empty and has one single-use owner.
        let slot = unsafe { &mut *self.callback.get() };
        assert!(
            slot.replace(callback).is_none(),
            "callback is installed once"
        );
        assert_eq!(
            self.phase
                .compare_exchange(
                    CALLBACK_EMPTY,
                    CALLBACK_PENDING,
                    Ordering::Release,
                    Ordering::Acquire,
                )
                .unwrap(),
            CALLBACK_EMPTY,
            "callback phase is installed once"
        );
    }

    fn try_activate(&self) -> bool {
        self.phase
            .compare_exchange(
                CALLBACK_PENDING,
                CALLBACK_ACTIVE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn mark_device_unavailable(&self) {
        let _ = self.phase.compare_exchange(
            CALLBACK_PENDING,
            CALLBACK_RECOVERABLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn is_active(&self) -> bool {
        self.phase.load(Ordering::Acquire) == CALLBACK_ACTIVE
    }

    fn render_active(&self, output: &mut [f32]) -> AudioRenderStatus {
        // SAFETY: only the uniquely borrowed MixerInput renders after a
        // successful Pending -> Active publication. Recovery can only consume
        // Pending or Recoverable, so it can never overlap this access.
        unsafe { (&mut *self.callback.get()).as_mut() }
            .map_or(AudioRenderStatus::Stop, |callback| {
                callback.render_interleaved_f32(output)
            })
    }

    fn take_after_failed_start(&self) -> AudioRenderCallback {
        let mut phase = self.phase.load(Ordering::Acquire);
        loop {
            assert!(
                matches!(phase, CALLBACK_PENDING | CALLBACK_RECOVERABLE),
                "only an unpublished callback can be recovered"
            );
            match self.phase.compare_exchange_weak(
                phase,
                CALLBACK_TAKEN,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => phase = actual,
            }
        }
        // SAFETY: the phase transition proves the callback was never Active.
        // The physical source is synchronously closed before every caller
        // consumes this single-owner value.
        unsafe { (&mut *self.callback.get()).take() }
            .expect("failed physical start retains the unpublished callback")
    }
}

// SAFETY: access follows the exclusive lifecycle documented on each method:
// installation precedes publication, render access belongs to the unique
// MixerInput, and recovery is possible only after failed start closed it.
unsafe impl Send for RecoverableRenderCallback {}
// SAFETY: see the `Send` invariant above. External clones are recovery tokens,
// not concurrent callback access authorities.
unsafe impl Sync for RecoverableRenderCallback {}

struct OutputFailureReporter {
    events: OnceLock<AudioOutputEventSink>,
    callback: Arc<RecoverableRenderCallback>,
    state: AtomicU8,
    #[cfg(test)]
    failure_hook: Option<Arc<TestFailureHook>>,
}

const REPORT_PENDING: u8 = 0;
const REPORT_PENDING_CALLBACK_PANICKED: u8 = 1;
const REPORT_PENDING_PROTOCOL: u8 = 2;
const REPORT_PENDING_BACKEND: u8 = 3;
const REPORT_ARMED: u8 = 4;
const REPORT_DISARMED: u8 = 5;
const REPORT_REPORTED: u8 = 6;

impl OutputFailureReporter {
    #[cfg(not(test))]
    fn new(callback: Arc<RecoverableRenderCallback>) -> Self {
        Self {
            events: OnceLock::new(),
            callback,
            state: AtomicU8::new(REPORT_PENDING),
        }
    }

    #[cfg(test)]
    fn new(
        callback: Arc<RecoverableRenderCallback>,
        failure_hook: Option<Arc<TestFailureHook>>,
    ) -> Self {
        Self {
            events: OnceLock::new(),
            callback,
            state: AtomicU8::new(REPORT_PENDING),
            failure_hook,
        }
    }

    fn install(&self, events: AudioOutputEventSink) {
        assert!(
            self.events.set(events).is_ok(),
            "output event reporter is installed exactly once"
        );
    }

    fn report(&self, reason: AudioOutputDeathReason) {
        let pending = match reason {
            AudioOutputDeathReason::CallbackPanicked => REPORT_PENDING_CALLBACK_PANICKED,
            AudioOutputDeathReason::CallbackProtocolViolation => REPORT_PENDING_PROTOCOL,
            _ => REPORT_PENDING_BACKEND,
        };
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            match state {
                REPORT_PENDING => match self.state.compare_exchange_weak(
                    REPORT_PENDING,
                    pending,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return,
                    Err(actual) => state = actual,
                },
                REPORT_ARMED => match self.state.compare_exchange_weak(
                    REPORT_ARMED,
                    REPORT_REPORTED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        if let Some(events) = self.events.get() {
                            let _ = events.report_endpoint_death(reason);
                        }
                        return;
                    }
                    Err(actual) => state = actual,
                },
                _ => return,
            }
        }
    }

    fn arm(&self) {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            let (next, pending) = match state {
                REPORT_PENDING => (REPORT_ARMED, None),
                REPORT_PENDING_CALLBACK_PANICKED => (
                    REPORT_REPORTED,
                    Some(AudioOutputDeathReason::CallbackPanicked),
                ),
                REPORT_PENDING_PROTOCOL => (
                    REPORT_REPORTED,
                    Some(AudioOutputDeathReason::CallbackProtocolViolation),
                ),
                REPORT_PENDING_BACKEND => (
                    REPORT_REPORTED,
                    Some(AudioOutputDeathReason::BackendFailure),
                ),
                REPORT_ARMED | REPORT_DISARMED | REPORT_REPORTED => return,
                _ => unreachable!("invalid output reporter state"),
            };
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    if let Some(reason) = pending
                        && let Some(events) = self.events.get()
                    {
                        let _ = events.report_endpoint_death(reason);
                    }
                    return;
                }
                Err(actual) => state = actual,
            }
        }
    }

    fn disarm(&self) {
        self.state.store(REPORT_DISARMED, Ordering::Release);
    }
}

impl MixerFailureObserver for OutputFailureReporter {
    fn output_failed(&self, _failure: MixerOutputFailure) {
        #[cfg(test)]
        if let Some(hook) = self.failure_hook.as_ref() {
            let _ = hook.entered.try_send(());
            hook.release.wait();
        }
        self.callback.mark_device_unavailable();
        self.report(AudioOutputDeathReason::BackendFailure);
    }
}

impl MixerInput for CallbackMixerInput {
    fn output_failure_observer(&self) -> Option<Arc<dyn MixerFailureObserver>> {
        Some(Arc::clone(&self.events) as Arc<dyn MixerFailureObserver>)
    }

    fn render(&mut self, output: &mut [MixerFrame]) -> MixerInputStatus {
        if !self.callback.is_active() {
            output.fill(MixerFrame::ZERO);
            return MixerInputStatus::Active;
        }
        let events = &self.events;
        let package_gain = self
            .package_gain
            .as_ref()
            .map_or(1.0, |gain| gain.effective_linear());
        let status = render_fixed(
            output,
            &mut self.scratch,
            package_gain,
            |scratch| self.callback.render_active(scratch),
            |reason| {
                events.report(reason);
            },
        );
        #[cfg(test)]
        if let Some(hook) = self
            .render_hook
            .as_ref()
            .filter(|hook| hook.block_once.swap(false, Ordering::AcqRel))
        {
            let _ = hook.entered.try_send(thread::current().id());
            hook.release.wait();
        }
        status
    }
}

#[cfg(test)]
impl Drop for CallbackMixerInput {
    fn drop(&mut self) {
        if let Some(hook) = self.render_hook.as_ref() {
            let _ = hook.dropped.try_send(thread::current().id());
            assert!(
                !hook.panic_on_drop,
                "injected callback shell destructor panic"
            );
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
struct TestRenderHook {
    entered: mpsc::SyncSender<ThreadId>,
    release: Arc<Barrier>,
    dropped: mpsc::SyncSender<ThreadId>,
    shutdown: mpsc::SyncSender<()>,
    block_once: AtomicBool,
    panic_on_drop: bool,
}

#[cfg(test)]
#[derive(Debug)]
struct TestFailureHook {
    entered: mpsc::SyncSender<()>,
    release: Arc<Barrier>,
}

fn render_fixed(
    output: &mut [MixerFrame],
    scratch: &mut [f32; INTERLEAVED_SAMPLES],
    effective_gain: f32,
    mut render: impl FnMut(&mut [f32]) -> AudioRenderStatus,
    mut report_death: impl FnMut(AudioOutputDeathReason),
) -> MixerInputStatus {
    output.fill(MixerFrame::ZERO);
    if output.is_empty() || output.len() > FRAMES {
        report_death(AudioOutputDeathReason::CallbackProtocolViolation);
        return MixerInputStatus::Finished;
    }

    let sample_count = output.len() * CHANNELS;
    match panic::catch_unwind(AssertUnwindSafe(|| render(&mut scratch[..sample_count]))) {
        Ok(AudioRenderStatus::Continue) => {
            if effective_gain == 0.0 {
                // The callback still advances, but mute must dominate hostile
                // non-finite graph samples (`NaN * 0` is not silent).
                output.fill(MixerFrame::ZERO);
            } else {
                for (frame, samples) in output
                    .iter_mut()
                    .zip(scratch[..sample_count].chunks_exact(2))
                {
                    *frame =
                        MixerFrame::new(samples[0] * effective_gain, samples[1] * effective_gain);
                }
            }
            MixerInputStatus::Active
        }
        Ok(_) => {
            scratch[..sample_count].fill(0.0);
            MixerInputStatus::Finished
        }
        Err(payload) => {
            std::mem::forget(payload);
            scratch[..sample_count].fill(0.0);
            report_death(AudioOutputDeathReason::CallbackPanicked);
            MixerInputStatus::Finished
        }
    }
}

struct ScriptBusRunningOutput {
    input: Option<RunningMixerInput>,
    #[cfg(test)]
    render_hook: Option<Arc<TestRenderHook>>,
}

impl RunningAudioOutput for ScriptBusRunningOutput {
    fn resume(&mut self) -> Result<(), AudioOutputError> {
        transition(self.input.as_ref(), false)
    }

    fn suspend(&mut self) -> Result<(), AudioOutputError> {
        transition(self.input.as_ref(), true)
    }

    fn shutdown(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        #[cfg(test)]
        if let Some(hook) = self.render_hook.as_ref() {
            let _ = hook.shutdown.try_send(());
        }
        let Some(input) = self.input.take() else {
            return AudioOutputEndpointShutdown::ready(Err(output_error(
                AudioOutputErrorKind::Shutdown,
                "the Web Audio mixer endpoint had no running input",
            )));
        };
        endpoint_shutdown(input.shutdown())
    }
}

struct RunningWithPriorCleanup {
    running: Option<Box<dyn RunningAudioOutput>>,
    prior_cleanup: Option<AudioOutputEndpointShutdown>,
}

impl RunningWithPriorCleanup {
    fn new(
        running: Box<dyn RunningAudioOutput>,
        prior_cleanup: AudioOutputEndpointShutdown,
    ) -> Self {
        Self {
            running: Some(running),
            prior_cleanup: Some(prior_cleanup),
        }
    }
}

impl RunningAudioOutput for RunningWithPriorCleanup {
    fn resume(&mut self) -> Result<(), AudioOutputError> {
        self.running
            .as_mut()
            .expect("combined running endpoint is open")
            .resume()
    }

    fn suspend(&mut self) -> Result<(), AudioOutputError> {
        self.running
            .as_mut()
            .expect("combined running endpoint is open")
            .suspend()
    }

    fn shutdown(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        let running = self
            .running
            .take()
            .expect("combined running endpoint is open");
        let prior_cleanup = self
            .prior_cleanup
            .take()
            .expect("combined running endpoint owns prior cleanup");
        combine_endpoint_shutdown(prior_cleanup, running.shutdown())
    }
}

fn combine_endpoint_shutdown(
    mut first: AudioOutputEndpointShutdown,
    mut second: AudioOutputEndpointShutdown,
) -> AudioOutputEndpointShutdown {
    AudioOutputEndpointShutdown::from_future(async move {
        let first_result = (&mut first).await;
        let second_result = (&mut second).await;
        match (first_result, second_result) {
            (Err(error), _) | (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    })
}

fn publish_running(
    mut owner: Box<ScriptBusRunningOutput>,
    input: RunningMixerInput,
) -> Box<dyn RunningAudioOutput> {
    owner.input = Some(input);
    owner
}

fn transition(input: Option<&RunningMixerInput>, suspended: bool) -> Result<(), AudioOutputError> {
    let Some(input) = input else {
        return Err(output_error(
            AudioOutputErrorKind::Shutdown,
            "the Web Audio mixer endpoint is already closed",
        ));
    };
    let applied = if suspended {
        input.suspend()
    } else {
        input.resume()
    };
    if applied {
        Ok(())
    } else if input.output_failure().is_some() {
        // The exact process-output cause is delivered by the preinstalled
        // failure observer. A close-time suspend after that global death is a
        // no-op, not a loss of callback-retirement proof.
        Ok(())
    } else if input.is_failed() {
        Err(output_error(
            AudioOutputErrorKind::CallbackDied,
            "the Web Audio mixer callback has failed closed",
        ))
    } else {
        Err(output_error(
            AudioOutputErrorKind::Shutdown,
            "the Web Audio mixer endpoint is closing",
        ))
    }
}

fn abort_start(
    error: AudioOutputError,
    reservation: MixerInputReservation,
    source: Box<dyn MixerInput>,
) -> AudioOutputStartFailure {
    let shutdown = reservation.abort();
    AudioOutputStartFailure::new(
        error,
        AudioOutputEndpointShutdown::from_future(async move {
            let retirement = shutdown.await;
            let dropped = panic::catch_unwind(AssertUnwindSafe(|| drop(source)));
            let destructor_panicked = match dropped {
                Ok(()) => false,
                Err(payload) => {
                    std::mem::forget(payload);
                    true
                }
            };
            if destructor_panicked {
                return Err(output_error(
                    AudioOutputErrorKind::Shutdown,
                    "the rejected Web Audio callback destructor panicked",
                ));
            }
            retirement_result(retirement)
        }),
    )
}

fn panicked_start_with_owned_cleanup(
    reservation: MixerInputReservation,
    source: Box<dyn MixerInput>,
) -> AudioOutputStartFailure {
    let shutdown = reservation.abort();
    AudioOutputStartFailure::new(
        output_error(
            AudioOutputErrorKind::BackendSpecific,
            "the mixer start transaction panicked",
        ),
        AudioOutputEndpointShutdown::from_future(async move {
            let retirement = shutdown.await;
            let dropped = panic::catch_unwind(AssertUnwindSafe(|| drop(source)));
            let destructor_panicked = match dropped {
                Ok(()) => false,
                Err(payload) => {
                    std::mem::forget(payload);
                    true
                }
            };
            let cleanup = retirement_result(retirement);
            let message = if destructor_panicked {
                "panicked start cleanup also encountered a callback destructor panic"
            } else if cleanup.is_err() {
                "panicked start cleanup could not be proven"
            } else {
                "panicked start was cleaned but remains an unconfirmed startup failure"
            };
            Err(output_error(AudioOutputErrorKind::Shutdown, message))
        }),
    )
}

fn endpoint_shutdown(shutdown: MixerInputShutdown) -> AudioOutputEndpointShutdown {
    AudioOutputEndpointShutdown::from_future(async move { retirement_result(shutdown.await) })
}

fn retirement_result(
    result: Result<smudgy_audio::MixerInputRetirement, MixerRetirementError>,
) -> Result<(), AudioOutputError> {
    match result {
        Ok(retirement) if retirement.source_destructor_panicked => Err(output_error(
            AudioOutputErrorKind::Shutdown,
            "the Web Audio mixer callback destructor panicked",
        )),
        Ok(retirement)
            if retirement.failed_before_retirement && retirement.output_failure.is_none() =>
        {
            Err(output_error(
                AudioOutputErrorKind::CallbackDied,
                "the Web Audio mixer callback failed before retirement",
            ))
        }
        // A global output cause was separately published through the exact
        // input's event sink. Proven off-render destruction confirms cleanup;
        // the operational failure remains in the context report.
        Ok(_) => Ok(()),
        Err(error) => Err(retirement_error(error)),
    }
}

fn control_error(error: MixerControlError) -> AudioOutputError {
    let kind = match error {
        MixerControlError::OwnerStopped => AudioOutputErrorKind::DeviceUnavailable,
        MixerControlError::InputCapacity | MixerControlError::SessionCapacity => {
            AudioOutputErrorKind::DeviceUnavailable
        }
        _ => AudioOutputErrorKind::BackendSpecific,
    };
    AudioOutputError::new(
        kind,
        format!("shared Smudgy mixer rejected output: {error:?}"),
    )
}

fn retirement_error(error: MixerRetirementError) -> AudioOutputError {
    AudioOutputError::new(
        AudioOutputErrorKind::Shutdown,
        format!("shared Smudgy mixer could not prove output retirement: {error:?}"),
    )
}

fn output_error(kind: AudioOutputErrorKind, message: &'static str) -> AudioOutputError {
    AudioOutputError::new(kind, message)
}

#[cfg(test)]
mod tests;
