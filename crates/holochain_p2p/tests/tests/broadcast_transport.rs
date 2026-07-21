//! End-to-end holochain_p2p messaging over the broadcast transport, and a
//! runtime switch between transport backends.
//!
//! Uses the broadcast transport's in-process `mem` medium (a process-global
//! shared "air"), so these tests need no real networking beyond the local
//! test bootstrap server and run un-gated. The udp multicast medium is
//! exercised by `K2_BCAST_IT`-gated tests in the
//! `kitsune2_transport_broadcast` crate.

use crate::tests::common::{spawn_test_bootstrap, Handler};
use holochain_keystore::*;
use holochain_p2p::event::DynHcP2pHandler;
use holochain_p2p::*;
use holochain_state::data::PeerMetaStore;
use holochain_trace::test_run;
use holochain_types::prelude::*;
use std::net::SocketAddr;
use std::{sync::Arc, time::Duration};

const WAIT_BETWEEN_CALLS: Duration = Duration::from_millis(10);
const PEER_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawn a holochain_p2p node whose active transport backend is the
/// broadcast transport on the in-process `mem` medium. The iroh relay is
/// still configured so a runtime switch to the iroh backend can succeed.
async fn spawn_broadcast_node(
    dna_hash: DnaHash,
    handler: DynHcP2pHandler,
    bootstrap_addr: &SocketAddr,
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
            network_config: Some(serde_json::json!({
                "coreBootstrap": {
                    "serverUrl": format!("http://{bootstrap_addr}"),
                },
                "switchTransport": {
                    "active": "broadcast",
                },
                "broadcastTransport": {
                    "medium": "mem",
                },
                "irohTransport": {
                    "relayUrl": format!("http://{bootstrap_addr}"),
                    "relayAllowPlainText": true,
                }
            })),
            request_timeout: Duration::from_secs(10),
            ..Default::default()
        },
        lair_client.clone(),
    )
    .await
    .unwrap();

    hc.register_handler(handler).await.unwrap();

    hc.join(dna_hash.clone(), agent.clone(), None, None)
        .await
        .unwrap();

    (agent, hc)
}

/// Wait until `hc`'s peer store for `dna_hash` knows about at least
/// `expected_count` agents.
async fn wait_for_peers(hc: &actor::DynHcP2p, dna_hash: DnaHash, expected_count: usize) {
    tokio::time::timeout(PEER_DISCOVERY_TIMEOUT, async {
        loop {
            if hc
                .peer_store(dna_hash.clone())
                .await
                .unwrap()
                .get_all()
                .await
                .unwrap()
                .len()
                >= expected_count
            {
                break;
            }
            tokio::time::sleep(WAIT_BETWEEN_CALLS).await;
        }
    })
    .await
    .expect("peer discovery timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn call_remote_over_broadcast_transport() {
    test_run();

    let dna_hash = DnaHash::from_raw_36(vec![0xbc; 36]);
    let handler = Arc::new(Handler::default());

    let (_bootstrap_srv, addr) = spawn_test_bootstrap().await.unwrap();
    let (agent1, hc1) = spawn_broadcast_node(dna_hash.clone(), handler.clone(), &addr).await;
    let (_agent2, hc2) = spawn_broadcast_node(dna_hash.clone(), handler, &addr).await;

    // Both nodes report the broadcast backend as active, with a
    // broadcast-scheme url.
    let stats = hc1.dump_network_stats().await.unwrap();
    assert_eq!(stats.transport_stats.backend, "broadcast-mem");
    assert!(
        stats.transport_stats.peer_urls[0]
            .as_str()
            .starts_with("ws://mem.bcast:1/"),
        "unexpected url: {}",
        stats.transport_stats.peer_urls[0]
    );

    // Agent infos flow via the (HTTP) bootstrap server; the message itself
    // rides the shared mem air.
    wait_for_peers(&hc2, dna_hash.clone(), 2).await;

    let resp = hc2
        .call_remote(
            dna_hash,
            agent1,
            ExternIO(b"over the air".to_vec()),
            Signature([0; 64]),
            None,
        )
        .await
        .unwrap();
    let resp: Vec<u8> = UnsafeBytes::from(resp).into();
    assert_eq!(
        "got_call_remote: over the air",
        String::from_utf8_lossy(&resp)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn runtime_switch_from_broadcast_to_iroh() {
    test_run();

    let dna_hash = DnaHash::from_raw_36(vec![0xbd; 36]);
    let handler = Arc::new(Handler::default());

    let (_bootstrap_srv, addr) = spawn_test_bootstrap().await.unwrap();
    let (_agent, hc) = spawn_broadcast_node(dna_hash.clone(), handler, &addr).await;

    let stats = hc.dump_network_stats().await.unwrap();
    assert_eq!(stats.transport_stats.backend, "broadcast-mem");

    // Switch at runtime — the same path the SwitchNetworkTransport admin
    // call uses. The swap is async; poll the stats until the new backend
    // reports.
    hc.switch_transport_backend("iroh".into()).await.unwrap();

    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let stats = hc.dump_network_stats().await.unwrap();
            if stats.transport_stats.backend == "iroh" {
                break;
            }
            tokio::time::sleep(WAIT_BETWEEN_CALLS).await;
        }
    })
    .await
    .expect("transport backend never switched to iroh");

    // An unknown backend is rejected by the switch and the current backend
    // is kept.
    hc.switch_transport_backend("no-such-backend".into())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stats = hc.dump_network_stats().await.unwrap();
    assert_eq!(stats.transport_stats.backend, "iroh");
}
