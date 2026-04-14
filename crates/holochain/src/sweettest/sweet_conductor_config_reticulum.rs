//! In-process Reticulum rendezvous for multi-conductor sweettests.
//!
//! Builds N `rns_transport::Transport` instances, each wrapped in a
//! `kitsune2_transport_reticulum::ReticulumNode`, and wires them into a
//! full-mesh loopback bridge so announces and link traffic flow between
//! them without needing real network interfaces.
//!
//! This mirrors the harness used in
//! `kitsune2-lrl/crates/transport_reticulum/tests/two_node_*.rs` but
//! scaled up to the conductor level: each `SweetConductor` is
//! instantiated via `ConductorBuilder::with_reticulum_node(node)` with
//! its own `Arc<ReticulumNode>`, and the p2p actor uses that node
//! instead of constructing a new one from the YAML config.
//!
//! Discovery is announce-driven (as in production Reticulum), so once
//! all conductors are up and have registered at least one space, they
//! learn about each other over the bridge and the normal kitsune2 +
//! holochain flows take over.

use kitsune2_transport_reticulum::ReticulumNode;
use rns_transport::identity::PrivateIdentity;
use rns_transport::iface::{RxMessage, TxMessage};
use rns_transport::transport::{Transport as RnsTransport, TransportConfig};
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

/// Rendezvous owning a set of `rns_transport::Transport` handles and
/// the bridge tasks that wire them together.
pub struct SweetReticulumRendezvous {
    /// One ReticulumNode per conductor, in construction order.
    nodes: Vec<Arc<ReticulumNode>>,
    /// JoinHandles for the forwarding tasks, kept alive for the
    /// rendezvous's lifetime.
    _bridge_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl SweetReticulumRendezvous {
    /// Build a rendezvous for `n` conductors, wiring every pair with a
    /// bidirectional loopback channel (full-mesh).
    ///
    /// Small `n` only — the bridge is O(n²) tasks.
    pub async fn new(n: usize) -> Self {
        assert!(n >= 1, "need at least one conductor");

        // Create one rns Transport per conductor.
        let mut tps: Vec<(Arc<TokioMutex<RnsTransport>>, PrivateIdentity)> = Vec::with_capacity(n);
        for i in 0..n {
            tps.push(make_rns_transport(&format!("sweet-reticulum-{i}")));
        }

        // Full-mesh bridge.
        let mut bridge_handles = Vec::new();
        for i in 0..n {
            for j in (i + 1)..n {
                let handles = wire_loopback(tps[i].0.clone(), tps[j].0.clone()).await;
                bridge_handles.extend(handles);
            }
        }

        // Let interface managers settle.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Wrap each transport in a ReticulumNode.
        let mut nodes = Vec::with_capacity(n);
        for (tp, id) in &tps {
            let node = ReticulumNode::from_rns_transport(tp.clone(), id.clone())
                .await
                .expect("from_rns_transport");
            nodes.push(node);
        }

        Self {
            nodes,
            _bridge_handles: bridge_handles,
        }
    }

    /// Get the node for conductor index `i`. Pass this into
    /// `ConductorBuilder::with_reticulum_node(...)`.
    pub fn node(&self, i: usize) -> Arc<ReticulumNode> {
        self.nodes[i].clone()
    }
}

fn make_rns_transport(name: &str) -> (Arc<TokioMutex<RnsTransport>>, PrivateIdentity) {
    let identity = PrivateIdentity::new_from_rand(rand_core::OsRng);
    let mut cfg = TransportConfig::new(name, &identity, true);
    cfg.set_link_proof_timeout_secs(5);
    cfg.set_link_idle_timeout_secs(60);
    let tp = RnsTransport::new(cfg);
    (Arc::new(TokioMutex::new(tp)), identity)
}

async fn wire_loopback(
    tp_a: Arc<TokioMutex<RnsTransport>>,
    tp_b: Arc<TokioMutex<RnsTransport>>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let (a_iface_addr, mut a_tx_recv, a_rx_send) = {
        let tp = tp_a.lock().await;
        let mgr = tp.iface_manager();
        let mut mgr = mgr.lock().await;
        let ch = mgr.new_channel(256);
        (ch.address, ch.tx_channel, ch.rx_channel)
    };
    let (b_iface_addr, mut b_tx_recv, b_rx_send) = {
        let tp = tp_b.lock().await;
        let mgr = tp.iface_manager();
        let mut mgr = mgr.lock().await;
        let ch = mgr.new_channel(256);
        (ch.address, ch.tx_channel, ch.rx_channel)
    };

    let b_rx_send_clone = b_rx_send.clone();
    let h1 = tokio::spawn(async move {
        while let Some(TxMessage { packet, .. }) = a_tx_recv.recv().await {
            let _ = b_rx_send_clone
                .send(RxMessage {
                    address: b_iface_addr,
                    packet,
                })
                .await;
        }
    });
    let a_rx_send_clone = a_rx_send.clone();
    let h2 = tokio::spawn(async move {
        while let Some(TxMessage { packet, .. }) = b_tx_recv.recv().await {
            let _ = a_rx_send_clone
                .send(RxMessage {
                    address: a_iface_addr,
                    packet,
                })
                .await;
        }
    });
    vec![h1, h2]
}
