// Models for observed server state: facts recorded by the client about a
// server — MSSP metadata the game volunteered, connection timestamps — as
// opposed to the user intent held in `server.json`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::get_smudgy_home;

use super::persistence::write_atomic;

/// One observed MSSP variable value: scalar text, or the multi-`VAL` array
/// form (`PORT`, `CHARSET`, `REFERRAL`, …). Values stay strings end-to-end;
/// numeric interpretation (player counts, uptime) happens at the display
/// edge only, and a value that doesn't parse renders as absent.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum ObservedValue {
    Text(String),
    List(Vec<String>),
}

/// Observed server state, serialized to/from `observed.json` beside
/// `server.json` in the server's directory. A sidecar by design:
/// `server.json` is user intent (host, port, TLS, trust grants) and every
/// field here is either server-supplied — attacker-controlled display/advisory
/// metadata, never connection behavior — or a client-recorded timestamp.
/// Absence of the file means nothing has been observed yet, and consumers
/// must render exactly as if the feature didn't exist.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct ObservedServer {
    /// The merged MSSP snapshot of the most recent connection that sent any:
    /// last-write-wins per variable across payloads, arrays preserved. Keys
    /// are the raw spec variable names, spaces included (`MINIMUM AGE`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mssp: BTreeMap<String, ObservedValue>,
    /// When the latest payload merged into `mssp` arrived. Rendered alongside
    /// point-in-time values (a player count is honest only with its age).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mssp_received_at: Option<DateTime<Utc>>,
    /// When a connection to this server last succeeded — stamped on every
    /// successful connect, whether or not the server speaks MSSP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected_at: Option<DateTime<Utc>>,
    /// The user declined this server's TLS upgrade offer. "No" means "stop
    /// asking for this server": once set, the offer banner never returns.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tls_offer_declined: bool,
}

/// The `observed.json` path inside the named server's directory.
///
/// # Errors
///
/// Returns an error if the smudgy home directory cannot be resolved.
pub fn observed_path(server_name: &str) -> Result<PathBuf> {
    Ok(get_smudgy_home()?.join(server_name).join("observed.json"))
}

/// Loads the named server's observed sidecar. `None` when the file is absent
/// (nothing observed yet) — and, with a logged warning, when it is unreadable
/// or unparsable: observed state is expendable display data, so corruption
/// degrades to "no metadata" rather than an error the caller must handle.
#[must_use]
pub fn load_observed(server_name: &str) -> Option<ObservedServer> {
    let path = observed_path(server_name).ok()?;
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            log::warn!("Failed to read {}: {err}", path.display());
            return None;
        }
    };
    match serde_json::from_str(&contents) {
        Ok(observed) => Some(observed),
        Err(err) => {
            log::warn!("Failed to parse {}: {err}", path.display());
            None
        }
    }
}

/// Persists the named server's observed sidecar via the standard atomic
/// replace, so a concurrently reading connect screen never sees a torn file.
///
/// # Errors
///
/// Returns an error if the smudgy home cannot be resolved, the contents fail
/// to serialize, or the atomic write fails.
pub fn save_observed(server_name: &str, observed: &ObservedServer) -> Result<()> {
    let path = observed_path(server_name)?;
    let json = serde_json::to_string_pretty(observed).context(format!(
        "Failed to serialize observed state for server '{server_name}'"
    ))?;
    write_atomic(&path, json.as_bytes()).context(format!(
        "Failed to write observed.json for server '{server_name}' at {}",
        path.display()
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_state_round_trips() {
        let observed = ObservedServer {
            mssp: BTreeMap::from([
                (
                    "NAME".to_string(),
                    ObservedValue::Text("ArcticMUD".to_string()),
                ),
                (
                    "PORT".to_string(),
                    ObservedValue::List(vec!["2700".to_string(), "6667".to_string()]),
                ),
                (
                    "MINIMUM AGE".to_string(),
                    ObservedValue::Text("13".to_string()),
                ),
            ]),
            mssp_received_at: DateTime::from_timestamp(1_760_000_000, 0),
            last_connected_at: DateTime::from_timestamp(1_760_000_100, 0),
            tls_offer_declined: true,
        };
        let json = serde_json::to_string(&observed).unwrap();
        assert_eq!(
            serde_json::from_str::<ObservedServer>(&json).unwrap(),
            observed
        );
    }

    #[test]
    fn values_serialize_transparently_as_strings_and_arrays() {
        // The untagged representation keeps the file shaped like the wire
        // data: a scalar is a JSON string, an array a JSON array of strings.
        let observed: ObservedServer =
            serde_json::from_str(r#"{"mssp":{"NAME":"ArcticMUD","PORT":["2700","6667"]}}"#)
                .unwrap();
        assert_eq!(
            observed.mssp.get("NAME"),
            Some(&ObservedValue::Text("ArcticMUD".to_string()))
        );
        assert_eq!(
            observed.mssp.get("PORT"),
            Some(&ObservedValue::List(vec![
                "2700".to_string(),
                "6667".to_string()
            ]))
        );
    }

    #[test]
    fn an_empty_file_deserializes_to_the_default() {
        let observed: ObservedServer = serde_json::from_str("{}").unwrap();
        assert_eq!(observed, ObservedServer::default());
        // A default serializes without noise fields, so a connect-stamp-only
        // sidecar stays a one-line record.
        let stamped = ObservedServer {
            last_connected_at: DateTime::from_timestamp(1_760_000_000, 0),
            ..ObservedServer::default()
        };
        let json = serde_json::to_string(&stamped).unwrap();
        assert!(!json.contains("mssp"));
        assert!(!json.contains("tls_offer_declined"));
        assert_eq!(
            serde_json::from_str::<ObservedServer>(&json).unwrap(),
            stamped
        );
    }
}
