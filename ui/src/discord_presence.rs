//! Discord Rich Presence: publishes a "Playing smudgy" activity on the user's
//! Discord profile the whole time the app is open — bare while no session is
//! connected, naming the server being played once one is.
//!
//! Everything here is local IPC against a Discord client on the same machine
//! (`\\.\pipe\discord-ipc-N` / `$XDG_RUNTIME_DIR/discord-ipc-N`); onward
//! distribution is the Discord client's department, governed by the user's
//! Discord privacy settings. The feature is master-switched by
//! [`Settings::discord_rich_presence`], on by default (Preferences is the
//! opt-out).
//!
//! The pipe protocol is synchronous (every frame waits for Discord's reply), so
//! all IPC lives on one dedicated worker thread; the daemon talks to it through
//! a channel of [`Presence`] snapshots. The worker owns reconnection: while an
//! activity is wanted and Discord is unreachable (not running, restarting), it
//! retries on a slow tick, and a mid-session Discord launch picks the activity
//! up on the next tick without any daemon involvement.
//!
//! [`Settings::discord_rich_presence`]: smudgy_core::models::settings::Settings::discord_rich_presence

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use discord_rich_presence::activity::{Activity, ActivityType, Timestamps};
use discord_rich_presence::{DiscordIpc, DiscordIpcClient};

/// The Discord application id activities are published under — the portal
/// application named "smudgy", whose registered name is what Discord displays
/// as the game title. Public information (the IPC handshake only identifies
/// which app the activity belongs to; no secret is involved).
const DISCORD_APP_ID: &str = "1523107132310949958";

/// How often the worker retries reaching Discord while an activity is wanted
/// but the IPC pipe is unavailable (Discord not running, or restarting).
const RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// What the published activity should say. This enum is the entire
/// vocabulary of what smudgy will tell Discord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    /// The client is open with no session connected: a bare "Playing smudgy"
    /// whose elapsed counter runs from app launch.
    Idle,
    /// A session is connected. How many is nobody's business but the
    /// player's, so a single label covers every multi-session arrangement.
    Playing {
        /// The activity's detail line, from the longest-connected session:
        /// its hostname, or its display name when the host is an IP or
        /// localhost (see [`server_label`]).
        server_label: String,
        /// Unix time in milliseconds the primary session connected; drives
        /// Discord's elapsed-time counter.
        connected_at_ms: i64,
    },
}

enum Command {
    /// Replace the desired activity.
    Publish(Presence),
    /// Clear the activity, close the pipe, and exit the worker.
    Shutdown,
}

/// The daemon-side handle: owns the worker thread while the setting is on and
/// change-gates outgoing snapshots so the (frequent) session-event recomputes
/// cost nothing when the answer hasn't changed.
pub struct DiscordPresence {
    /// The last snapshot handed to [`Self::publish`] — kept even while
    /// disabled, so toggling the setting on mid-session seeds the fresh
    /// worker with the current game immediately. There is always a desired
    /// activity while the app runs; `Idle` is the no-sessions shape.
    desired: Presence,
    /// Unix time in milliseconds the controller was created (app launch),
    /// driving the `Idle` activity's elapsed counter.
    launched_at_ms: i64,
    worker: Option<Worker>,
}

struct Worker {
    tx: mpsc::Sender<Command>,
    join: JoinHandle<()>,
}

impl DiscordPresence {
    pub fn new(enabled: bool) -> Self {
        let mut this = Self {
            desired: Presence::Idle,
            launched_at_ms: unix_now_ms(),
            worker: None,
        };
        this.set_enabled(enabled);
        this
    }

    /// Starts or stops the worker to match the setting. Disabling clears the
    /// activity from the user's profile before the worker exits.
    pub fn set_enabled(&mut self, enabled: bool) {
        if enabled == self.worker.is_some() {
            return;
        }
        if enabled {
            let (tx, rx) = mpsc::channel();
            let launched_at_ms = self.launched_at_ms;
            match std::thread::Builder::new()
                .name("smudgy-discord-presence".into())
                .spawn(move || worker_loop(&rx, launched_at_ms))
            {
                Ok(join) => {
                    let _ = tx.send(Command::Publish(self.desired.clone()));
                    self.worker = Some(Worker { tx, join });
                }
                Err(err) => log::warn!("failed to spawn Discord presence worker: {err}"),
            }
        } else {
            self.stop_worker();
        }
    }

    /// Replaces the published activity. Change-gated: repeated identical
    /// snapshots send nothing to the worker.
    pub fn publish(&mut self, presence: Presence) {
        if presence == self.desired {
            return;
        }
        self.desired = presence;
        if let Some(worker) = &self.worker {
            let _ = worker.tx.send(Command::Publish(self.desired.clone()));
        }
    }

    fn stop_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.tx.send(Command::Shutdown);
            // The worker's slowest exit path is one clear+close exchange with
            // Discord — bounded, so joining here doesn't stall the daemon.
            let _ = worker.join.join();
        }
    }
}

impl Drop for DiscordPresence {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

/// Label for the activity's "on \<label\>" line. Normally the configured
/// host, but an IP literal, localhost, or an empty host (a failed config
/// load) falls back to the server's display name: "on ArcticMUD" reads
/// better than "on 192.168.1.50", and a home IP has no business on a
/// Discord profile.
pub fn server_label(host: &str, display_name: &str) -> String {
    let bare = host.trim().trim_start_matches('[').trim_end_matches(']');
    if bare.is_empty()
        || bare.eq_ignore_ascii_case("localhost")
        || bare.parse::<std::net::IpAddr>().is_ok()
    {
        display_name.to_string()
    } else {
        host.trim().to_string()
    }
}

/// Current Unix time in milliseconds (the resolution Discord's activity
/// timestamps use). A pre-epoch clock clamps to 0 rather than panicking.
pub fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

fn worker_loop(rx: &mpsc::Receiver<Command>, launched_at_ms: i64) {
    log::info!("Discord presence worker started (app id {DISCORD_APP_ID})");
    let mut client: Option<DiscordIpcClient> = None;
    let mut desired: Option<Presence> = None;
    // Log the first failure of a streak at info (it answers "why isn't my
    // status showing"); the 30s retries behind it stay at debug.
    let mut failure_announced = false;

    loop {
        // Idle-block on the channel; while an activity is wanted but Discord
        // is unreachable, wake periodically to retry the connection instead.
        let command = if desired.is_some() && client.is_none() {
            match rx.recv_timeout(RETRY_INTERVAL) {
                Ok(command) => Some(command),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(command) => Some(command),
                Err(_) => break,
            }
        };
        match command {
            Some(Command::Publish(presence)) => desired = Some(presence),
            Some(Command::Shutdown) => break,
            // A retry tick: nothing new wanted, just another go at Discord.
            None => {}
        }
        sync(
            &mut client,
            desired.as_ref(),
            launched_at_ms,
            &mut failure_announced,
        );
    }

    if let Some(mut client) = client {
        let _ = client.clear_activity();
        let _ = client.close();
    }
    log::info!("Discord presence worker stopped");
}

/// Makes the Discord side match `desired`: connects on demand, then sets the
/// activity (`desired` is `None` only before the first publish arrives). Any
/// IPC failure drops the connection; while an activity is wanted, the
/// caller's retry tick re-establishes it.
fn sync(
    client: &mut Option<DiscordIpcClient>,
    desired: Option<&Presence>,
    launched_at_ms: i64,
    failure_announced: &mut bool,
) {
    let Some(presence) = desired else { return };

    if client.is_none() {
        let mut fresh = DiscordIpcClient::new(DISCORD_APP_ID);
        match fresh.connect() {
            Ok(()) => {
                log::info!("connected to Discord IPC");
                *failure_announced = false;
                *client = Some(fresh);
            }
            Err(err) => {
                if *failure_announced {
                    log::debug!("Discord IPC still unavailable, will retry: {err}");
                } else {
                    log::info!(
                        "can't reach Discord IPC (is Discord running?); retrying every 30s: {err}"
                    );
                    *failure_announced = true;
                }
                return;
            }
        }
    }

    let mut activity = Activity::new().activity_type(ActivityType::Playing);
    match presence {
        // Bare "Playing smudgy", elapsed since launch.
        Presence::Idle => {
            activity = activity.timestamps(Timestamps::new().start(launched_at_ms));
        }
        Presence::Playing {
            server_label,
            connected_at_ms,
        } => {
            activity = activity
                .details(format!("on {server_label}"))
                .timestamps(Timestamps::new().start(*connected_at_ms));
        }
    }

    let describe = match presence {
        Presence::Idle => "idle (no session connected)".to_string(),
        Presence::Playing { server_label, .. } => format!("on {server_label}"),
    };
    let Some(active) = client.as_mut() else {
        return;
    };
    // The crate's `set_activity` is send-only; read Discord's response frame
    // ourselves — it carries the verdict ("evt":"ERROR" plus a message when
    // the payload is rejected), and leaving it unread would let replies pile
    // up in the pipe.
    match active.set_activity(activity).and_then(|()| active.recv()) {
        Ok((_, reply)) if reply.get("evt").and_then(|evt| evt.as_str()) == Some("ERROR") => {
            log::warn!("Discord rejected activity update ({describe}): {reply}");
        }
        Ok(_) => log::info!("Discord activity updated: {describe}"),
        Err(err) => {
            log::info!("Discord activity update failed ({describe}); will reconnect: {err}");
            *client = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::server_label;

    #[test]
    fn label_prefers_the_hostname() {
        assert_eq!(
            server_label("mud.arctic.org", "ArcticMUD"),
            "mud.arctic.org"
        );
        assert_eq!(
            server_label(" tdome.nukefire.org ", "Thunderdome"),
            "tdome.nukefire.org"
        );
    }

    #[test]
    fn label_falls_back_to_display_name_for_ips_and_localhost() {
        for host in [
            "",
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "192.168.1.50",
            "::1",
            "[::1]",
            "2001:db8::7334",
        ] {
            assert_eq!(
                server_label(host, "ArcticMUD"),
                "ArcticMUD",
                "host {host:?}"
            );
        }
    }
}
