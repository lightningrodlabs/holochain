//! End-to-end test: two SweetConductors gossiping over Reticulum.
//!
//! Uses the in-process `SweetReticulumRendezvous` loopback bridge to
//! wire two `rns_transport::Transport` instances together, then builds
//! two `SweetConductor`s each with their own pre-built `ReticulumNode`.
//! Install a simple DNA on both, commit an entry on A, and assert B
//! can read it — proving the full kitsune2 + holochain stack works
//! over Reticulum end-to-end.

#![cfg(feature = "transport-reticulum")]

use hdk::prelude::*;
use holochain::sweettest::*;
use holochain::test_utils::inline_zomes::simple_create_read_zome;
use holochain_conductor_api::conductor::{
    ConductorConfig, NetworkConfig, ReticulumInterfaceConfig, ReticulumTransportConfig,
};
use holochain::conductor::ConductorBuilder;

/// End-to-end: two `SweetConductor`s gossip a committed entry over
/// an in-process `rns_transport` loopback bridge. Passes in ~2-3s.
///
/// Peer discovery is bootstrapped via `exchange_peer_info` rather
/// than Reticulum's announce path. Announce-based discovery works in
/// `kitsune2_transport_reticulum`'s own integration test with its
/// fixture `AgentInfoSigned`, but holochain's `AgentInfoSigned` —
/// with `ret://` URL, full storage arc, etc. — sometimes exceeds
/// the ~316-byte compressed-announce budget by a few bytes; when that
/// happens the publisher logs a warning and keeps the last good
/// value, so discovery degrades but doesn't block. Pre-exchanging
/// peer info sidesteps that variability and keeps this test
/// deterministic.
#[tokio::test(flavor = "multi_thread")]
async fn two_conductors_over_reticulum() {
    holochain_trace::test_run();

    let rendezvous = SweetReticulumRendezvous::new(2).await;

    // When we inject a pre-built ReticulumNode, ReticulumNode::from_config
    // is bypassed -- but k2's factory still runs validate_config at
    // builder.build() time, which rejects an empty interfaces list.
    // Provide a placeholder interface that's never actually started
    // (mirroring k2's own two_node_data.rs test harness).
    let mk_config = || {
        let network = NetworkConfig {
            reticulum: Some(ReticulumTransportConfig {
                interfaces: vec![ReticulumInterfaceConfig::TcpClient {
                    target: "0.0.0.0:0".into(),
                }],
                identity_path: None,
                max_frame_bytes: 1024 * 1024,
                connect_timeout_s: 10,
                announce_interval_s: 1,
                link_idle_timeout_s: 60,
            }),
            ..Default::default()
        }
        // The default initiate interval is 120s -- way longer than our
        // test timeout. Tight gossip timings so consistency can be
        // reached within seconds on the in-process loopback bridge.
        .with_gossip_initiate_interval_ms(500)
        .with_gossip_initiate_jitter_ms(10)
        .with_gossip_min_initiate_interval_ms(100);

        ConductorConfig {
            network,
            ..Default::default()
        }
    };

    let conductor_a = SweetConductor::from_builder(
        ConductorBuilder::new()
            .config(mk_config())
            .with_reticulum_node(rendezvous.node(0)),
    )
    .await;
    let conductor_b = SweetConductor::from_builder(
        ConductorBuilder::new()
            .config(mk_config())
            .with_reticulum_node(rendezvous.node(1)),
    )
    .await;

    let mut conductors =
        SweetConductorBatch::new(vec![conductor_a, conductor_b]);

    let dna_file =
        SweetDnaFile::unique_from_inline_zomes(("simple", simple_create_read_zome()))
            .await
            .0;
    let apps = conductors
        .setup_app("app", &[dna_file])
        .await
        .unwrap();
    let ((alice,), (bobbo,)) = apps.into_tuples();

    // Full storage arcs on both peers.
    conductors[0]
        .declare_full_storage_arcs(alice.dna_hash())
        .await;
    conductors[1]
        .declare_full_storage_arcs(bobbo.dna_hash())
        .await;

    // Exchange peer info so each side learns about the other (normally
    // done via the bootstrap/announce layer; we trigger it explicitly
    // to avoid waiting on the per-space announce interval).
    conductors.exchange_peer_info().await;

    // Commit an entry on A. simple_create_read_zome's `create` takes ()
    // and returns an ActionHash for a unit-valued entry.
    let hash: ActionHash = conductors[0]
        .call(&alice.zome("simple"), "create", ())
        .await;

    // Wait for B to see it via gossip, then read back.
    await_consistency_s(30, [&alice, &bobbo]).await.unwrap();

    let record: Option<Record> = conductors[1]
        .call(&bobbo.zome("simple"), "read", hash.clone())
        .await;
    assert!(
        record.is_some(),
        "bobbo should see alice's entry after consistency"
    );
}
