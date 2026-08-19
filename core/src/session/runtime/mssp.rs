//! The host-side MSSP **producer**: the session-thread half that merges the
//! connection layer's decoded variables (`session::connection::mssp`) into the
//! session store under the `mssp` platform producer and catalogues each variable
//! for the automations window's Store tab.
//!
//! Smaller still than its `msdp` sibling: MSSP has no negotiation lifecycle the
//! runtime sees (the server volunteers one subnegotiation on connect, maybe
//! re-sent on change) and no send surface at all — scripts consume
//! `smudgy:state/mssp` read-only and the `updated` event fires after each merge.
//! The snapshot is last-write-wins per variable across payloads; a fresh connect
//! clears it (fresh server, fresh truth), and it stays readable post-disconnect
//! like the `gmcp`/`msdp` trees. Variable names are the raw spec names — spaces
//! included (`MINIMUM AGE`) — so each writes at a single-segment path consumers
//! spell bracket-quoted.

use std::collections::BTreeMap;

use serde_json::Value;

use super::IsolateId;
use super::catalogue::{CatalogueKind, SharedCatalogue};
use super::store::{PlatformProducer, ProducerKey, SessionStore, StorePath};
use crate::models::observed::ObservedValue;
use crate::session::connection::mssp::{MsspValue, TlsOffer, tls_upgrade_port};

/// Variable names past this length are never legitimate spec variables and
/// are excluded from the catalogue, whose entries are permanent for the
/// session and bounded in count but not in key length — an uncapped name
/// would let a hostile server park megabytes per admitted entry.
const MAX_CATALOGUED_NAME_BYTES: usize = 256;

/// What one ingested payload asks the dispatch arm to do beyond the store writes.
#[derive(Default)]
pub(super) struct IngestEffects {
    /// Session-notice lines to echo (the one-time budget notice).
    pub echoes: Vec<String>,
    /// Whether the merge changed the sidecar-visible snapshot. A re-sent
    /// identical payload leaves it false, letting the dispatch arm skip the
    /// persist — a re-advertisement storm must not become an attacker-paced
    /// disk-write loop.
    pub observed_changed: bool,
}

pub(super) struct MsspProducer {
    /// The catalogue producer key (`"mssp"`), interned once.
    producer_display: std::sync::Arc<str>,
    /// Whether the one-time budget-refusal session notice went out.
    budget_noticed: bool,
    /// The connection-scoped merged snapshot in its persisted (sidecar) form:
    /// what `observed.json` should say about the current connection's MSSP
    /// variables. Mirrors the store subtree — cleared on connect, merged
    /// last-write-wins per variable, growth bounded by the same store budget
    /// (a refused write is left out here too). The dispatch arm persists it
    /// after each merge that changes it.
    observed: BTreeMap<String, ObservedValue>,
    /// The endpoint the current connection dialed — host, port, whether the
    /// transport is encrypted — latched at connect for the TLS upgrade-offer
    /// guard. `None` until a connect happens (paths that inject variables
    /// without one never offer) and after disconnect.
    dialed: Option<(std::sync::Arc<String>, u16, bool)>,
    /// The raw value of the last `TLS`/`SSL` variable received this
    /// connection (the names are aliases; last write wins across the pair
    /// like any repeated variable). Never persisted — the advertised port
    /// dies with the connection.
    advertised_tls: Option<String>,
    /// The raw `HOSTNAME` variable received this connection, for the offer
    /// guard's host-match check.
    advertised_hostname: Option<String>,
    /// Whether the upgrade banner was already offered on this connection —
    /// the latch that keeps a re-advertisement storm to one banner.
    tls_offer_shown: bool,
}

impl MsspProducer {
    pub fn new() -> Self {
        Self {
            producer_display: std::sync::Arc::from(PlatformProducer::Mssp.as_str()),
            budget_noticed: false,
            observed: BTreeMap::new(),
            dialed: None,
            advertised_tls: None,
            advertised_hostname: None,
            tls_offer_shown: false,
        }
    }

    /// The current connection's merged variables in sidecar form, for the
    /// dispatch arm's persist after a merge.
    pub fn observed_snapshot(&self) -> &BTreeMap<String, ObservedValue> {
        &self.observed
    }

    /// A new connection is starting: fresh server, fresh truth — the snapshot is
    /// cleared by one root write. MSSP has no negotiation-on signal the runtime
    /// sees (the data simply arrives), so the connect itself is the reset point —
    /// for the TLS upgrade-offer guard state too, which latches the endpoint
    /// being dialed here and forgets everything a previous connection advertised.
    pub fn on_connect(
        &mut self,
        store: &mut SessionStore,
        host: std::sync::Arc<String>,
        port: u16,
        encrypted: bool,
    ) {
        self.observed.clear();
        self.dialed = Some((host, port, encrypted));
        self.advertised_tls = None;
        self.advertised_hostname = None;
        self.tls_offer_shown = false;
        store
            .set(
                ProducerKey::Platform(PlatformProducer::Mssp),
                StorePath::root(),
                Value::Object(serde_json::Map::new()),
                IsolateId::Main,
                0,
            )
            .ok();
    }

    /// The connection is gone: the advertised TLS port, hostname claim, and
    /// banner latch die with it — a stale answer must never act on a
    /// different connection's offer. The store subtree stays readable
    /// post-disconnect like the `gmcp`/`msdp` trees.
    pub fn on_disconnect(&mut self) {
        self.dialed = None;
        self.advertised_tls = None;
        self.advertised_hostname = None;
        self.tls_offer_shown = false;
    }

    /// The port the guard would offer right now were there no persisted
    /// refusal — the cheap pre-check that lets the dispatch arm skip loading
    /// the sidecar on the (overwhelming) paths where no offer is pending.
    /// Does not latch.
    pub fn tls_upgrade_candidate(&self) -> Option<u16> {
        let (host, port, encrypted) = self.dialed.as_ref()?;
        tls_upgrade_port(&TlsOffer {
            encrypted: *encrypted,
            dialed_host: host,
            dialed_port: *port,
            advertised: self.advertised_tls.as_deref()?,
            advertised_hostname: self.advertised_hostname.as_deref(),
            offer_declined: false,
            already_offered: self.tls_offer_shown,
        })
    }

    /// Evaluate the TLS upgrade-offer guard against the current connection
    /// and the variables received so far, latching the banner so at most one
    /// offer is made per connection. `offer_declined` is the persisted
    /// per-server refusal from `observed.json`. Returns the port to offer.
    pub fn tls_upgrade_offer(&mut self, offer_declined: bool) -> Option<u16> {
        if offer_declined {
            return None;
        }
        let offered = self.tls_upgrade_candidate()?;
        self.tls_offer_shown = true;
        Some(offered)
    }

    /// Merge one decoded payload into the snapshot: catalogue each variable
    /// (name granularity, occurrence sample) and write the store at each name —
    /// a repeat replaces, so the subtree is the last-write-wins union across
    /// payloads. The store flush — and with it watcher/binding delivery — is the
    /// run loop's normal per-turn flush, so the `gmcp` wire-order guarantee
    /// holds here identically.
    pub fn ingest(
        &mut self,
        store: &mut SessionStore,
        catalogue: &SharedCatalogue,
        pairs: &[(String, MsspValue)],
    ) -> IngestEffects {
        let mut effects = IngestEffects::default();
        for (name, value) in pairs {
            // The upgrade-offer guard's session-side facts, tracked in wire
            // order and outside the store budget (two tiny strings): `TLS`
            // and `SSL` are aliases for the advertised port, and an array
            // value follows the `PORT` convention (preferred value last).
            match name.as_str() {
                "TLS" | "SSL" => self.advertised_tls = Some(value.preferred_scalar().to_string()),
                "HOSTNAME" => {
                    self.advertised_hostname = Some(value.preferred_scalar().to_string());
                }
                _ => {}
            }
            let json = match value {
                MsspValue::Text(text) => Value::String(text.clone()),
                MsspValue::List(items) => {
                    Value::Array(items.iter().cloned().map(Value::String).collect())
                }
            };
            // Sampled before the budget outcome: presence and history don't depend on
            // the store having room. Absurdly long names are excluded — catalogue
            // entries live for the session and their keys are otherwise unbounded.
            if name.len() <= MAX_CATALOGUED_NAME_BYTES {
                let sample = json.to_string();
                catalogue.borrow_mut().sample_dynamic(
                    &self.producer_display,
                    CatalogueKind::State,
                    name,
                    PlatformProducer::Mssp.as_str(),
                    &sample,
                );
            }

            // Single-segment path: an MSSP name is one key however it is spelled —
            // spaces and dots alike are name text, never path structure.
            let Ok(path) = StorePath::from_segments([name.as_str()]) else {
                log::warn!("MSSP variable name {name:?} does not map to a store path; dropped");
                continue;
            };
            match store.set(
                ProducerKey::Platform(PlatformProducer::Mssp),
                path,
                json,
                IsolateId::Main,
                0,
            ) {
                Ok(_) => {
                    let observed_value = match value {
                        MsspValue::Text(text) => ObservedValue::Text(text.clone()),
                        MsspValue::List(items) => ObservedValue::List(items.clone()),
                    };
                    if self.observed.get(name) != Some(&observed_value) {
                        self.observed.insert(name.clone(), observed_value);
                        effects.observed_changed = true;
                    }
                }
                Err(err) => {
                    // The refusal warn latches with the echo: a payload at the
                    // cap can carry tens of thousands of refused variables, and
                    // a per-variable warn is itself a log-flooding lever.
                    if !self.budget_noticed {
                        self.budget_noticed = true;
                        log::warn!("MSSP write refused: {err}");
                        effects.echoes.push(format!(
                            "MSSP: the server's data exceeded the session store budget and is \
                             no longer being retained ({err}). Existing state is intact."
                        ));
                    }
                }
            }
        }
        effects
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use serde_json::{Value, json};

    use super::super::catalogue::RuntimeCatalogue;
    use super::*;

    fn harness() -> (MsspProducer, SessionStore, SharedCatalogue) {
        (
            MsspProducer::new(),
            SessionStore::new(),
            Rc::new(RefCell::new(RuntimeCatalogue::new())),
        )
    }

    fn read(store: &SessionStore, path: &str) -> Option<Value> {
        store.get(
            &ProducerKey::Platform(PlatformProducer::Mssp),
            &StorePath::parse(path).unwrap(),
            &IsolateId::Main,
        )
    }

    fn text(name: &str, value: &str) -> (String, MsspValue) {
        (name.to_string(), MsspValue::Text(value.to_string()))
    }

    /// Latch a plain-transport connection to a DNS name, the baseline the
    /// upgrade-offer tests start from.
    fn connect(mssp: &mut MsspProducer, store: &mut SessionStore) {
        mssp.on_connect(
            store,
            std::sync::Arc::new("mud.example.org".to_string()),
            4000,
            false,
        );
    }

    #[test]
    fn variables_write_at_their_raw_names_and_arrays_stay_arrays() {
        let (mut mssp, mut store, catalogue) = harness();
        mssp.ingest(
            &mut store,
            &catalogue,
            &[
                text("NAME", "ArcticMUD"),
                (
                    "PORT".to_string(),
                    MsspValue::List(vec!["2700".to_string(), "6667".to_string()]),
                ),
                text("MINIMUM AGE", "13"),
            ],
        );
        store.flush();
        assert_eq!(read(&store, "NAME"), Some(json!("ArcticMUD")));
        assert_eq!(read(&store, "PORT"), Some(json!(["2700", "6667"])));
        // A spec name with a space is one bracket-quoted key, not path structure.
        assert_eq!(read(&store, r#"["MINIMUM AGE"]"#), Some(json!("13")));
    }

    #[test]
    fn merges_are_last_write_wins_per_variable_across_payloads() {
        let (mut mssp, mut store, catalogue) = harness();
        mssp.ingest(
            &mut store,
            &catalogue,
            &[text("NAME", "ArcticMUD"), text("PLAYERS", "52")],
        );
        mssp.ingest(&mut store, &catalogue, &[text("PLAYERS", "53")]);
        store.flush();
        assert_eq!(
            read(&store, "NAME"),
            Some(json!("ArcticMUD")),
            "a variable absent from a later payload survives"
        );
        assert_eq!(read(&store, "PLAYERS"), Some(json!("53")));
    }

    #[test]
    fn the_sidecar_snapshot_mirrors_the_merge_and_clears_on_connect() {
        let (mut mssp, mut store, catalogue) = harness();
        mssp.ingest(
            &mut store,
            &catalogue,
            &[
                text("PLAYERS", "52"),
                (
                    "PORT".to_string(),
                    MsspValue::List(vec!["2700".to_string(), "6667".to_string()]),
                ),
            ],
        );
        mssp.ingest(&mut store, &catalogue, &[text("PLAYERS", "53")]);
        let observed = mssp.observed_snapshot();
        assert_eq!(
            observed.get("PLAYERS"),
            Some(&ObservedValue::Text("53".to_string()))
        );
        assert_eq!(
            observed.get("PORT"),
            Some(&ObservedValue::List(vec![
                "2700".to_string(),
                "6667".to_string()
            ]))
        );

        connect(&mut mssp, &mut store);
        assert!(mssp.observed_snapshot().is_empty());
    }

    #[test]
    fn a_fresh_connect_clears_the_snapshot() {
        let (mut mssp, mut store, catalogue) = harness();
        mssp.ingest(&mut store, &catalogue, &[text("NAME", "ArcticMUD")]);
        store.flush();
        assert_eq!(read(&store, "NAME"), Some(json!("ArcticMUD")));

        connect(&mut mssp, &mut store);
        store.flush();
        assert_eq!(read(&store, "NAME"), None, "fresh server, fresh truth");
    }

    #[test]
    fn the_upgrade_offer_latches_to_one_banner_per_connection() {
        let (mut mssp, mut store, catalogue) = harness();
        connect(&mut mssp, &mut store);
        mssp.ingest(&mut store, &catalogue, &[text("SSL", "6667")]);
        assert_eq!(mssp.tls_upgrade_offer(false), Some(6667));
        // A re-advertisement storm re-evaluates but never re-offers.
        mssp.ingest(&mut store, &catalogue, &[text("SSL", "6667")]);
        assert_eq!(mssp.tls_upgrade_offer(false), None);

        // The latch (and the advertised port) die with the connection; the
        // next connect starts clean and may offer again.
        mssp.on_disconnect();
        assert_eq!(mssp.tls_upgrade_offer(false), None, "nothing advertised");
        connect(&mut mssp, &mut store);
        mssp.ingest(&mut store, &catalogue, &[text("TLS", "6667")]);
        assert_eq!(mssp.tls_upgrade_offer(false), Some(6667));
    }

    #[test]
    fn tls_and_ssl_alias_one_advertised_port_with_hostname_guarding() {
        let (mut mssp, mut store, catalogue) = harness();
        connect(&mut mssp, &mut store);
        // Aliases are last-write-wins across the pair, in wire order — and a
        // support flag overwriting a port withdraws the offer.
        mssp.ingest(
            &mut store,
            &catalogue,
            &[text("SSL", "6667"), text("TLS", "1")],
        );
        assert_eq!(mssp.tls_upgrade_offer(false), None);

        // A HOSTNAME naming a different host blocks the offer even when a
        // later payload re-advertises a usable port.
        mssp.ingest(
            &mut store,
            &catalogue,
            &[text("TLS", "6667"), text("HOSTNAME", "other.example.org")],
        );
        assert_eq!(mssp.tls_upgrade_offer(false), None);
    }

    #[test]
    fn advertised_port_lies_never_offer() {
        // Through the full producer path: "0" is a support flag, "99999" is
        // out of range, and the port already dialed offers nothing.
        for lie in ["0", "99999", "4000"] {
            let (mut mssp, mut store, catalogue) = harness();
            connect(&mut mssp, &mut store);
            mssp.ingest(&mut store, &catalogue, &[text("SSL", lie)]);
            assert_eq!(mssp.tls_upgrade_offer(false), None, "SSL {lie:?}");
        }
    }

    #[test]
    fn a_re_advertised_identical_payload_merges_idempotently() {
        let (mut mssp, mut store, catalogue) = harness();
        connect(&mut mssp, &mut store);
        let payload = [
            text("NAME", "ArcticMUD"),
            (
                "PORT".to_string(),
                MsspValue::List(vec!["2700".to_string(), "6667".to_string()]),
            ),
        ];
        let effects = mssp.ingest(&mut store, &catalogue, &payload);
        assert!(effects.observed_changed, "the first merge is a change");
        let first = mssp.observed_snapshot().clone();
        let effects = mssp.ingest(&mut store, &catalogue, &payload);
        assert_eq!(
            mssp.observed_snapshot(),
            &first,
            "a byte-identical re-advertisement changes nothing"
        );
        assert!(
            !effects.observed_changed,
            "an unchanged merge reports no change, so nothing re-persists"
        );
        store.flush();
        assert_eq!(read(&store, "NAME"), Some(json!("ArcticMUD")));
        assert_eq!(read(&store, "PORT"), Some(json!(["2700", "6667"])));
    }

    #[test]
    fn the_persisted_refusal_and_missing_connect_suppress_the_offer() {
        let (mut mssp, mut store, catalogue) = harness();
        // Variables without a latched connection (injection paths) never offer.
        mssp.ingest(&mut store, &catalogue, &[text("SSL", "6667")]);
        assert_eq!(mssp.tls_upgrade_offer(false), None);

        connect(&mut mssp, &mut store);
        mssp.ingest(&mut store, &catalogue, &[text("SSL", "6667")]);
        assert_eq!(
            mssp.tls_upgrade_offer(true),
            None,
            "declined stays declined"
        );
        assert_eq!(
            mssp.tls_upgrade_offer(false),
            Some(6667),
            "a refusal is not the latch — nothing was shown yet"
        );
    }
}
