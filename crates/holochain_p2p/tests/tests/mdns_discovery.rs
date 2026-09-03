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
//! rarely have, so the test only runs when `K2_MDNS_IT` is set.

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

/// A relay url that accepts no connections (the TCP discard port).
const UNREACHABLE_RELAY: &str = "https://127.0.0.1:9/relay";

/// How long to wait for the mDNS announce/browse/dial round trip. Probe
/// tiebreaking between responders sharing one host can delay the first
/// announcement by ~20s, so this is generous.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(120);

const WAIT_BETWEEN_POLLS: Duration = Duration::from_millis(250);

fn should_run() -> bool {
    std::env::var("K2_MDNS_IT").is_ok()
}

/// A service type private to this test run, so that neither another node on
/// the same LAN nor a previous run's lingering records can take part. The
/// label has to stay within the 15-byte mDNS limit.
fn per_run_service_type() -> String {
    let tag: u32 = rand::random();
    format!("_hcmdns{tag:08x}._udp.local.")
}

async fn spawn_mdns_node(dna_hash: DnaHash, service_type: &str) -> (AgentPubKey, actor::DynHcP2p) {
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
            "relayUrl": UNREACHABLE_RELAY,
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

#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_discover_each_other_over_mdns_without_bootstrap_or_relay() {
    if !should_run() {
        eprintln!("skipping mdns test (set K2_MDNS_IT=1 on a host with multicast)");
        return;
    }
    test_run();

    let service_type = per_run_service_type();
    tracing::info!(%service_type, "mdns e2e service type");

    let dna_hash = fixt!(DnaHash);
    let space_id = dna_hash.to_k2_space();

    let (agent_a, hc_a) = spawn_mdns_node(dna_hash.clone(), &service_type).await;
    let (agent_b, hc_b) = spawn_mdns_node(dna_hash.clone(), &service_type).await;

    let store_a = hc_a
        .test_kitsune()
        .space(space_id.clone(), None)
        .await
        .unwrap()
        .peer_store()
        .clone();
    let store_b = hc_b
        .test_kitsune()
        .space(space_id.clone(), None)
        .await
        .unwrap()
        .peer_store()
        .clone();

    let k2_agent_a = agent_a.to_k2_agent();
    let k2_agent_b = agent_b.to_k2_agent();

    let deadline = std::time::Instant::now() + DISCOVERY_TIMEOUT;
    loop {
        let a_sees_b = store_a.get(k2_agent_b.clone()).await.unwrap().is_some();
        let b_sees_a = store_b.get(k2_agent_a.clone()).await.unwrap().is_some();
        if a_sees_b && b_sees_a {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "timed out waiting for mdns discovery: a_sees_b={a_sees_b}, b_sees_a={b_sees_a}"
            );
        }
        tokio::time::sleep(WAIT_BETWEEN_POLLS).await;
    }

    // Knowing the peer is not the same as being allowed to talk to it: the
    // hello/PoK exchange over the mDNS-dialled connection is what makes the
    // discovery usable.
    wait_for_access_grants(&hc_a, dna_hash.clone(), 1).await;
    wait_for_access_grants(&hc_b, dna_hash.clone(), 1).await;
}
