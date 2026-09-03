//! Two holochain_p2p nodes on one host find each other over mDNS while both
//! the bootstrap server and the relay are unreachable.
//!
//! This is the whole LAN-discovery chain end to end: the mDNS bootstrap
//! announces this node's peer url and dials the peer it hears announcing the
//! same space fingerprint, iroh's own LAN address lookup turns that relay-only
//! url into a direct path, the preflight exchange carries the agent infos into
//! both peer stores, and the hello/PoK access module grants the peer. Every
//! other route to the peer is closed off: the bootstrap url and the relay url
//! both point at addresses that accept no connections.
//!
//! mDNS needs a real multicast-capable interface, which sandboxed CI runners
//! rarely have, so these tests are gated on `KITSUNE2_LAN_TEST` — the same
//! switch kitsune2's own LAN tests use (`kitsune2/tests/mdns_lan.rs`,
//! `transport_iroh/tests/offline_relay.rs`), so one env var turns on every
//! test in the stack that needs multicast.

use crate::tests::common::{wait_for_access_grants, Handler};
use ::fixt::fixt;
use holo_hash::fixt::DnaHashFixturator;
use holochain_keystore::*;
use holochain_p2p::*;
use holochain_state::data::PeerMetaStore;
use holochain_trace::test_run;
use holochain_types::prelude::*;
use std::{sync::Arc, time::Duration};

/// A bootstrap server url that accepts no connections.
const UNREACHABLE_BOOTSTRAP: &str = "http://127.0.0.1:1";

/// Relay urls that accept no connections (the TCP discard port). Two of them,
/// so that a node's announced peer url can be made to name a relay the other
/// node does not share.
const UNREACHABLE_RELAY_A: &str = "https://127.0.0.1:9/relay";
const UNREACHABLE_RELAY_B: &str = "https://127.0.0.2:9/relay";

/// How long to wait for the mDNS announce/browse/dial round trip. Probe
/// tiebreaking between responders sharing one host can delay the first
/// announcement by ~20s, so this is generous.
const DISCOVERY_TIMEOUT_MS: u64 = 120_000;

const POLL_INTERVAL_MS: u64 = 250;

/// A service type private to this test run, so that neither another node on
/// the same LAN nor a previous run's lingering records can take part. The
/// label has to stay within the 15-byte mDNS limit.
fn per_run_service_type() -> String {
    let tag: u32 = rand::random();
    format!("_hcmdns{tag:08x}._udp.local.")
}

async fn spawn_mdns_node(
    dna_hash: DnaHash,
    service_type: &str,
    relay_url: &str,
) -> (AgentPubKey, actor::DynHcP2p) {
    let db_peer_meta = holochain_state::peer_metadata_store::PeerMetaStore::new(
        holochain_state::data::test_open_db(PeerMetaStore::new(Arc::new(dna_hash.clone())))
            .await
            .unwrap(),
    );
    let dht_store = holochain_state::DhtStore::new_test(holochain_state::data::Dht::new(Arc::new(
        dna_hash.clone(),
    )))
    .await
    .unwrap();
    let conductor_store = holochain_state::conductor::ConductorStore::new_test()
        .await
        .unwrap();
    let lair_client = test_keystore();

    let agent = lair_client.new_sign_keypair_random().await.unwrap();

    let network_config = serde_json::json!({
        "coreBootstrap": {
            "serverUrl": UNREACHABLE_BOOTSTRAP,
        },
        "mdnsBootstrap": {
            "enabled": true,
            "serviceType": service_type,
            // A peer is dialled when its announcement is first heard; this
            // only paces the retries if that first dial races the transport
            // learning the peer's address.
            "redialIntervalMs": 5_000,
        },
        "irohTransport": {
            "relayUrl": relay_url,
            "enableLanDiscovery": true,
        }
    });

    let hc = spawn_holochain_p2p(
        HolochainP2pConfig {
            get_db_peer_meta: Arc::new(move |_| {
                let db_peer_meta = db_peer_meta.clone();
                Box::pin(async move { Ok(db_peer_meta.clone()) })
            }),
            get_dht_store: Arc::new(move |_| {
                let dht_store = dht_store.clone();
                Box::pin(async move { Ok(dht_store) })
            }),
            get_conductor_store: Arc::new(move || {
                let conductor_store = conductor_store.clone();
                Box::pin(async move { conductor_store })
            }),
            network_config: Some(network_config),
            request_timeout: Duration::from_secs(10),
            ..Default::default()
        },
        lair_client.clone(),
    )
    .await
    .unwrap();

    hc.register_handler(Arc::new(Handler::default()))
        .await
        .unwrap();

    hc.join(dna_hash.clone(), agent.clone(), None, None)
        .await
        .unwrap();

    (agent, hc)
}

/// Stand two nodes up on the given relay urls and assert the full chain:
/// each node's peer store gains the other's agent, and the hello/PoK access
/// module grants the peer on both sides.
async fn assert_two_nodes_discover_each_other(relay_a: &str, relay_b: &str) {
    test_run();

    let service_type = per_run_service_type();
    tracing::info!(%service_type, %relay_a, %relay_b, "mdns e2e service type");

    let dna_hash = fixt!(DnaHash);

    let (agent_a, hc_a) = spawn_mdns_node(dna_hash.clone(), &service_type, relay_a).await;
    let (agent_b, hc_b) = spawn_mdns_node(dna_hash.clone(), &service_type, relay_b).await;

    let k2_agent_a = agent_a.to_k2_agent();
    let k2_agent_b = agent_b.to_k2_agent();

    retry_fn_until_timeout(
        || async {
            let a_sees_b = hc_a
                .peer_store(dna_hash.clone())
                .await
                .unwrap()
                .get(k2_agent_b.clone())
                .await
                .unwrap()
                .is_some();
            let b_sees_a = hc_b
                .peer_store(dna_hash.clone())
                .await
                .unwrap()
                .get(k2_agent_a.clone())
                .await
                .unwrap()
                .is_some();
            tracing::info!(a_sees_b, b_sees_a, "mdns e2e progress");
            a_sees_b && b_sees_a
        },
        Some(DISCOVERY_TIMEOUT_MS),
        Some(POLL_INTERVAL_MS),
    )
    .await
    .expect("timed out waiting for both peer stores to hold the other agent");

    // Knowing the peer is not the same as being allowed to talk to it: the
    // hello/PoK exchange over the mDNS-dialled connection is what makes the
    // discovery usable.
    wait_for_access_grants(&hc_a, dna_hash.clone(), 1).await;
    wait_for_access_grants(&hc_b, dna_hash.clone(), 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_discover_each_other_over_mdns_without_bootstrap_or_relay() {
    if std::env::var("KITSUNE2_LAN_TEST").is_err() {
        eprintln!(
            "skipping two_nodes_discover_each_other_over_mdns_without_bootstrap_or_relay: set KITSUNE2_LAN_TEST=1 to run"
        );
        return;
    }

    assert_two_nodes_discover_each_other(UNREACHABLE_RELAY_A, UNREACHABLE_RELAY_A).await;
}

/// The same chain with the two nodes announcing peer urls derived from
/// DIFFERENT relays. The hello base lets a peer on another relay through
/// preflight, and a real LAN mixes relays — a laptop configured for one
/// deployment's relay next to a laptop configured for another's — so the
/// LAN path has to work without the two agreeing on a relay string.
#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_discover_each_other_over_mdns_on_different_relays() {
    if std::env::var("KITSUNE2_LAN_TEST").is_err() {
        eprintln!(
            "skipping two_nodes_discover_each_other_over_mdns_on_different_relays: set KITSUNE2_LAN_TEST=1 to run"
        );
        return;
    }

    assert_two_nodes_discover_each_other(UNREACHABLE_RELAY_A, UNREACHABLE_RELAY_B).await;
}
