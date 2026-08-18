use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use super::SessionId;
use super::runtime::Runtime;

/// Cached, data-only identity for a script-visible session handle. It can be
/// carried with a destruction notice after the runtime leaves the registry.
#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub profile_name: Arc<String>,
    pub profile_subtext: Arc<String>,
    pub connected: bool,
}

impl SessionSnapshot {
    #[must_use]
    pub fn to_json(&self, tombstone: bool) -> String {
        serde_json::json!({
            "id": u32::from(self.id),
            "profile": {
                "name": self.profile_name.as_str(),
                "subtext": self.profile_subtext.as_str(),
            },
            "connected": self.connected && !tombstone,
            "tombstone": tombstone,
        })
        .to_string()
    }
}

/// Per-session v8 inspector endpoints (set once a session's runtime is built, when
/// debugging is enabled). Kept separate from the `Runtime` entry because the bound
/// address isn't known until after the runtime thread constructs its script engine.
static INSPECTOR_ADDRESSES: OnceLock<Mutex<HashMap<SessionId, SocketAddr>>> = OnceLock::new();

fn inspector_addresses() -> &'static Mutex<HashMap<SessionId, SocketAddr>> {
    INSPECTOR_ADDRESSES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record the v8 inspector endpoint for a session.
///
/// # Panics
///
/// Panics if the inspector-address mutex is poisoned.
pub fn set_inspector_address(session_id: SessionId, addr: SocketAddr) {
    inspector_addresses()
        .lock()
        .unwrap()
        .insert(session_id, addr);
}

/// Get the v8 inspector endpoint for a session, if one is listening (debug mode).
/// The UI's "Show Inspector" affordance spawns `smudgy_inspector <addr>` with this.
///
/// # Panics
///
/// Panics if the inspector-address mutex is poisoned.
#[must_use]
pub fn get_inspector_address(session_id: SessionId) -> Option<SocketAddr> {
    inspector_addresses()
        .lock()
        .unwrap()
        .get(&session_id)
        .copied()
}

/// Shared map of active session runtimes, keyed by session id.
type SessionRegistry = Arc<Mutex<HashMap<SessionId, Arc<Runtime>>>>;

/// Global registry of all active sessions
static SESSION_REGISTRY: OnceLock<SessionRegistry> = OnceLock::new();
static BROADCAST_CHANNELS: OnceLock<
    Mutex<HashMap<String, smudgy_script::InMemoryBroadcastChannel>>,
> = OnceLock::new();

/// Get the global session registry
pub fn get_registry() -> SessionRegistry {
    SESSION_REGISTRY
        .get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
        .clone()
}

/// Register a new session in the global registry
///
/// # Panics
///
/// Panics if the session-registry mutex is poisoned.
pub fn register_session(session_id: SessionId, runtime: Arc<Runtime>) {
    let server = Arc::clone(&runtime.server_name);
    let registry = get_registry();
    let mut sessions = registry.lock().unwrap();
    sessions.insert(session_id, Arc::clone(&runtime));
    drop(sessions);
    log::info!("Registered session {session_id} in global registry");
    if let Some(snapshot) = snapshot(session_id) {
        broadcast_lifecycle(&server, "created", &snapshot, false);
    }
    runtime.start();
}

/// Return the shared standard `BroadcastChannel` backend for one configured server entry.
///
/// # Panics
///
/// Panics if the broadcast-channel mutex is poisoned.
#[must_use]
pub fn broadcast_channel_for_server(server: &str) -> smudgy_script::InMemoryBroadcastChannel {
    BROADCAST_CHANNELS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .entry(server.to_string())
        .or_default()
        .clone()
}

/// Unregister a session from the global registry
///
/// # Panics
///
/// Panics if the session-registry or inspector-address mutex is poisoned.
pub fn unregister_session(session_id: SessionId) {
    let Some(mut snapshot) = snapshot(session_id) else {
        log::warn!("Attempted to unregister non-existent session {session_id}");
        return;
    };
    let Some(runtime) = get_runtime(session_id) else {
        return;
    };
    let server = Arc::clone(&runtime.server_name);
    if runtime
        .connected
        .swap(false, std::sync::atomic::Ordering::AcqRel)
    {
        snapshot.connected = false;
        broadcast_lifecycle(&server, "disconnected", &snapshot, false);
    }
    snapshot.connected = false;
    broadcast_lifecycle(&server, "destroyed", &snapshot, true);
    for other in get_runtimes_for_server(&server) {
        if other.session_id != session_id {
            for key in super::runtime::input::purge_session_input_interop(
                &other.input_word_sets,
                &other.pane_input_callbacks,
                session_id,
            ) {
                let _ = other
                    .tx
                    .send(super::runtime::RuntimeAction::InputWordSetsChanged { key });
            }
        }
    }
    let registry = get_registry();
    let mut sessions = registry.lock().unwrap();
    inspector_addresses().lock().unwrap().remove(&session_id);
    if sessions.remove(&session_id).is_some() {
        log::info!("Unregistered session {session_id} from global registry");
    } else {
        log::warn!("Attempted to unregister non-existent session {session_id}");
    }
}

/// Broadcast one non-replaying lifecycle occurrence to every currently
/// registered session on the same server entry.
pub fn broadcast_lifecycle(server: &str, kind: &str, snapshot: &SessionSnapshot, tombstone: bool) {
    let canonical: Arc<str> = Arc::from(format!("sessions:{kind}"));
    let payload: Arc<str> = Arc::from(snapshot.to_json(tombstone));
    for runtime in get_runtimes_for_server(server) {
        let _ = runtime
            .tx
            .send(super::runtime::RuntimeAction::InteropEvent {
                canonical: Arc::clone(&canonical),
                stamped: Arc::clone(&canonical),
                payload: Arc::clone(&payload),
                source: snapshot.clone(),
                depth: 0,
            });
    }
}

/// Active sessions on one configured server entry.
///
/// # Panics
///
/// Panics if the session-registry mutex is poisoned.
#[must_use]
pub fn get_session_ids_for_server(server: &str) -> Vec<SessionId> {
    let registry = get_registry();
    let sessions = registry.lock().unwrap();
    let mut ids: Vec<SessionId> = sessions
        .iter()
        .filter_map(|(id, runtime)| (runtime.server_name.as_str() == server).then_some(*id))
        .collect();
    ids.sort_unstable();
    ids
}

/// Runtime handles on one configured server entry, cloned out before callers
/// route actions so the global registry lock is never held across a send.
///
/// # Panics
///
/// Panics if the session-registry mutex is poisoned.
#[must_use]
pub fn get_runtimes_for_server(server: &str) -> Vec<Arc<Runtime>> {
    let registry = get_registry();
    let sessions = registry.lock().unwrap();
    sessions
        .values()
        .filter(|runtime| runtime.server_name.as_str() == server)
        .cloned()
        .collect()
}

#[must_use]
pub fn snapshot(session_id: SessionId) -> Option<SessionSnapshot> {
    let runtime = get_runtime(session_id)?;
    Some(SessionSnapshot {
        id: session_id,
        profile_name: Arc::clone(&runtime.profile_name),
        profile_subtext: Arc::clone(&runtime.profile_subtext),
        connected: runtime.connected.load(std::sync::atomic::Ordering::Acquire),
    })
}

/// Get a specific runtime by session ID
///
/// # Panics
///
/// Panics if the session-registry mutex is poisoned.
#[must_use]
pub fn get_runtime(session_id: SessionId) -> Option<Arc<Runtime>> {
    let registry = get_registry();
    let sessions = registry.lock().unwrap();
    sessions.get(&session_id).cloned()
}

/// Get the number of active sessions
///
/// # Panics
///
/// Panics if the session-registry mutex is poisoned.
#[must_use]
pub fn session_count() -> usize {
    let registry = get_registry();
    let sessions = registry.lock().unwrap();
    sessions.len()
}
