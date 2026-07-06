//! Two holochain_p2p nodes on one host discover each other via mDNS while
//! the bootstrap server is unreachable.
//!
//! Like the `kitsune2_bootstrap_mdns` integration tests, this is gated
//! behind the `K2_MDNS_IT` env var because it needs a host where real mDNS
//! multicast works — sandboxed CI environments would fail spuriously.
//!
//! The in-memory test bootstrap is explicitly disabled and the core
//! bootstrap URL points at an unreachable address, so the only way the two
//! nodes can learn about each other is the mDNS LAN exchange.

use crate::tests::common::{spawn_test_servers, Handler, TestServers};
use ::fixt::fixt;
use holo_hash::fixt::DnaHashFixturator;
use holochain_keystore::*;
use holochain_p2p::*;
use holochain_trace::test_run;
use holochain_types::db::{DbKindCache, DbKindConductor, DbKindDht, DbKindPeerMetaStore, DbWrite};
use holochain_types::prelude::*;
use std::{sync::Arc, time::Duration};

fn should_run() -> bool {
    std::env::var("K2_MDNS_IT").is_ok()
}

async fn spawn_mdns_node(
    dna_hash: DnaHash,
    service_type: &str,
    servers: &TestServers,
) -> (AgentPubKey, actor::DynHcP2p) {
    let db_peer_meta =
        DbWrite::test_in_mem(DbKindPeerMetaStore(Arc::new(dna_hash.clone()))).unwrap();
    let db_op = DbWrite::test_in_mem(DbKindDht(Arc::new(dna_hash.clone()))).unwrap();
    let db_cache = DbWrite::test_in_mem(DbKindCache(Arc::new(dna_hash.clone()))).unwrap();
    let conductor_db = DbWrite::test_in_mem(DbKindConductor).unwrap();
    let lair_client = test_keystore();

    let agent = lair_client.new_sign_keypair_random().await.unwrap();

    // The bootstrap server URL is unreachable on purpose; only the signal /
    // relay servers are real so the transport can come up.
    #[cfg(feature = "transport-tx5-backend-go-pion")]
    let network_config = {
        let signal_addr = &servers.bootstrap_addr;
        serde_json::json!({
            "coreBootstrap": {
                "serverUrl": "http://127.0.0.1:1",
            },
            "mdnsBootstrap": {
                "enabled": true,
                "serviceType": service_type,
            },
            "tx5Transport": {
                "serverUrl": format!("ws://{signal_addr}"),
                "signalAllowPlainText": true,
                "timeoutS": 30,
                "webrtcConnectTimeoutS": 25,
            }
        })
    };
    #[cfg(all(
        feature = "transport-iroh",
        not(feature = "transport-tx5-backend-go-pion")
    ))]
    let network_config = serde_json::json!({
        "coreBootstrap": {
            "serverUrl": "http://127.0.0.1:1",
        },
        "mdnsBootstrap": {
            "enabled": true,
            "serviceType": service_type,
        },
        "irohTransport": {
            "relayUrl": format!("http://{}", servers.relay_addr),
            "relayAllowPlainText": true,
        }
    });

    let hc = spawn_holochain_p2p(
        HolochainP2pConfig {
            get_db_peer_meta: Arc::new(move |_| {
                let db_peer_meta = db_peer_meta.clone();
                Box::pin(async move { Ok(db_peer_meta.clone()) })
            }),
            get_db_op_store: Arc::new(move |_| {
                let db_op = db_op.clone();
                Box::pin(async move { Ok(db_op.clone()) })
            }),
            get_db_cache: Arc::new(move |_| {
                let db_cache = db_cache.clone();
                Box::pin(async move { Ok(db_cache) })
            }),
            get_conductor_db: Arc::new(move || {
                let conductor_db = conductor_db.clone();
                Box::pin(async move { conductor_db })
            }),
            network_config: Some(network_config),
            // The in-memory test bootstrap would let the nodes find each
            // other through a process-global map, which would defeat the
            // point of this test. Force the real (unreachable) bootstrap.
            mem_bootstrap: false,
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
async fn two_nodes_discover_each_other_via_mdns_with_unreachable_bootstrap() {
    if !should_run() {
        eprintln!("skipping mdns test (set K2_MDNS_IT=1 on a host with multicast)");
        return;
    }
    test_run();

    // Randomise the service type per run so stale announcements from a
    // previous run don't poison the browse. Label must stay <= 15 bytes.
    let tag: u32 = rand::random();
    let service_type = format!("_hcmdns{tag:08x}._udp.local.");

    let servers = spawn_test_servers().await.unwrap();
    let dna_hash = fixt!(DnaHash);
    let space_id = dna_hash.to_k2_space();

    let (agent_a, hc_a) = spawn_mdns_node(dna_hash.clone(), &service_type, &servers).await;
    let (agent_b, hc_b) = spawn_mdns_node(dna_hash.clone(), &service_type, &servers).await;

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

    // mDNS probe tiebreaking can delay the first announcement by ~20s when
    // several responders share one host, so allow a generous deadline.
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        let a_sees_b = store_a.get(k2_agent_b.clone()).await.unwrap().is_some();
        let b_sees_a = store_b.get(k2_agent_a.clone()).await.unwrap().is_some();
        if a_sees_b && b_sees_a {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("timeout: a_sees_b={a_sees_b}, b_sees_a={b_sees_a} within 90s");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
