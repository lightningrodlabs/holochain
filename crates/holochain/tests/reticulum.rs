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

/// Blocked on two k2-side issues found while bringing this up:
///
/// 1. **Preflight race on fresh inbound links.** When A's preflight +
///    first data frame arrive on B's data-router task *before* B's
///    links-router task has run `start_preflight` (which flips
///    `PeerState.preflight_state.local_sent`), the data frame is
///    dropped with `"data frame before preflight ready -- dropping"`.
///    The `route_links` handler is async and yields before calling
///    `start_preflight`; meanwhile the data router can already be
///    draining events from the same link. Fix likely needs either:
///    buffer-until-ready on the data router, or set `local_sent`
///    synchronously in the links router before any await.
///
/// 2. **Announce `OutOfMemory` at the rns packet boundary.**
///    `compress_app_data` deflate-compresses the canonical-JSON
///    `AgentInfoSigned`, but holochain's AgentInfoSigned (ret:// URL,
///    agent hash, etc.) compresses to ~330+ bytes -- above the
///    ~316 bytes of `app_data` available after rns announce header
///    overhead. The announce publisher logs
///    `"Failed to announce destination ... OutOfMemory"` every cycle
///    and peer discovery via announce never completes. Our test works
///    around this via `SweetConductorBatch::exchange_peer_info`,
///    injecting peer info directly, but production deployments
///    depend on the announce path.
///
/// Both issues exposed by `link_data` traffic actually reaching the
/// wire and being dropped at state-machine gates rather than network
/// failures, so the fundamental transport-on-loopback pipeline works;
/// these are state-machine / encoding issues in
/// `kitsune2_transport_reticulum`.
#[ignore = "blocked on k2-side preflight race + announce payload size; \
    kept as end-to-end wiring check for when those land"]
#[tokio::test(flavor = "multi_thread")]
async fn two_conductors_over_reticulum() {
    holochain_trace::test_run();

    let rendezvous = SweetReticulumRendezvous::new(2).await;

    // When we inject a pre-built ReticulumNode, ReticulumNode::from_config
    // is bypassed -- but k2's factory still runs validate_config at
    // builder.build() time, which rejects an empty interfaces list.
    // Provide a placeholder interface that's never actually started
    // (mirroring k2's own two_node_data.rs test harness).
    let mk_config = || ConductorConfig {
        network: NetworkConfig {
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
        },
        ..Default::default()
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
