use ::fixt::fixt;
use holo_hash::fixt::ActionHashFixturator;
use holochain_p2p::event::*;
use holochain_p2p::*;
use holochain_types::fixt::CreateLinkAction;
use holochain_types::op::ChainOp;
use holochain_types::prelude::*;
use kitsune2_api::*;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Handler {
    pub calls: Arc<Mutex<Vec<String>>>,
    get_response: WireOps,
    get_response_delay: Option<Duration>,
}

impl Handler {
    pub(crate) fn new(get_response: WireOps, get_response_delay: Option<Duration>) -> Self {
        Self {
            calls: Default::default(),
            get_response,
            get_response_delay,
        }
    }
}

impl Default for Handler {
    fn default() -> Self {
        Handler {
            calls: Arc::new(Mutex::new(Vec::new())),
            get_response: WireOps::Entry(WireEntryOps::new()),
            get_response_delay: None,
        }
    }
}

impl HcP2pHandler for Handler {
    fn handle_call_remote(
        &self,
        _dna_hash: DnaHash,
        _to_agent: AgentPubKey,
        zome_call_params_serialized: ExternIO,
        _signature: Signature,
    ) -> BoxFut<'_, HolochainP2pResult<SerializedBytes>> {
        Box::pin(async move {
            let respond = format!(
                "got_call_remote: {}",
                String::from_utf8_lossy(&zome_call_params_serialized.0),
            );
            self.calls.lock().unwrap().push(respond.clone());
            Ok(UnsafeBytes::from(respond.into_bytes()).into())
        })
    }

    fn handle_remote_signal_direct(
        &self,
        _dna_hash: DnaHash,
        _to_agent: AgentPubKey,
        signal: Vec<u8>,
        _from_agent: AgentPubKey,
        _signature: Signature,
    ) -> BoxFut<'_, HolochainP2pResult<()>> {
        Box::pin(async move {
            let respond = format!(
                "got_remote_signal_direct: {}",
                String::from_utf8_lossy(&signal),
            );
            self.calls.lock().unwrap().push(respond.clone());
            Ok(())
        })
    }

    fn handle_publish(
        &self,
        _dna_hash: DnaHash,
        _ops: Vec<(holochain_types::op::DhtOp, bool)>,
    ) -> BoxFut<'_, HolochainP2pResult<()>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push("publish".into());
            Ok(())
        })
    }

    fn handle_get(
        &self,
        _dna_hash: DnaHash,
        _to_agent: AgentPubKey,
        _dht_hash: holo_hash::AnyDhtHash,
    ) -> BoxFut<'_, HolochainP2pResult<WireOps>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push("get".into());
            if let Some(delay) = self.get_response_delay {
                tokio::time::sleep(delay).await;
            }
            Ok(self.get_response.clone())
        })
    }

    fn handle_get_links(
        &self,
        _dna_hash: DnaHash,
        _to_agent: AgentPubKey,
        _link_key: WireLinkKey,
        _options: GetLinksOptions,
    ) -> BoxFut<'_, HolochainP2pResult<WireLinkOps>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push("get_links".into());
            let action = fixt!(Action, CreateLinkAction);
            Ok(WireLinkOps {
                creates: vec![Judged::new(
                    SignedAction::new(action, fixt!(Signature)),
                    ValidationStatus::Valid,
                )],
                deletes: Vec::new(),
                warrants: Vec::new(),
            })
        })
    }

    fn handle_count_links(
        &self,
        _dna_hash: DnaHash,
        _to_agent: AgentPubKey,
        _query: WireLinkQuery,
    ) -> BoxFut<'_, HolochainP2pResult<CountLinksResponse>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push("count_links".into());
            Ok(CountLinksResponse::new(vec![fixt!(ActionHash)]))
        })
    }

    fn handle_get_agent_activity(
        &self,
        _dna_hash: DnaHash,
        _to_agent: AgentPubKey,
        _agent: AgentPubKey,
        _query: ChainQueryFilter,
        _options: GetActivityOptions,
    ) -> BoxFut<'_, HolochainP2pResult<AgentActivityResponse>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push("get_agent_activity".into());
            Ok(AgentActivityResponse {
                agent: AgentPubKey::from_raw_36(vec![2; 36]),
                valid_activity: ChainItems::NotRequested,
                rejected_activity: ChainItems::NotRequested,
                status: ChainStatus::Valid(ChainHead {
                    action_seq: fixt!(Action).action_seq(),
                    hash: fixt!(ActionHash),
                }),
                highest_observed: None,
                warrants: Vec::new(),
            })
        })
    }

    fn handle_must_get_agent_activity(
        &self,
        _dna_hash: DnaHash,
        _to_agent: AgentPubKey,
        _author: AgentPubKey,
        _filter: ChainFilter,
    ) -> BoxFut<'_, HolochainP2pResult<MustGetAgentActivityResponse>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push("must_get_agent_activity".into());
            Ok(MustGetAgentActivityResponse::activity(Vec::new()))
        })
    }

    fn handle_validation_receipts_received(
        &self,
        _dna_hash: DnaHash,
        _to_agent: AgentPubKey,
        _receipts: ValidationReceiptBundle,
    ) -> BoxFut<'_, HolochainP2pResult<()>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push("validation_receipts".into());
            Ok(())
        })
    }

    fn handle_publish_countersign(
        &self,
        _dna_hash: DnaHash,
        _op: ChainOp,
    ) -> BoxFut<'_, HolochainP2pResult<()>> {
        Box::pin(async move { todo!() })
    }

    fn handle_countersigning_session_negotiation(
        &self,
        _dna_hash: DnaHash,
        _to_agent: AgentPubKey,
        _message: CountersigningSessionNegotiationMessage,
    ) -> BoxFut<'_, HolochainP2pResult<()>> {
        Box::pin(async move { todo!() })
    }
}

/// Wait until `hc`'s space for `dna_hash` has recorded an
/// [`AccessDecision::Granted`] for at least `expected_remote_grants` distinct
/// remote peer urls.
///
/// This is the access-gate sibling of `wait_for_peers`. That helper covers the
/// iroh peer-discovery race, where a test that sends immediately can outrun the
/// transport learning a peer's url. This one covers a second and entirely
/// separate gate introduced by the hello/PoK access layer: knowing a peer is
/// not the same as being allowed to talk to it. Non-hello traffic toward a peer
/// with no `Granted` decision is dropped, and a fire-and-forget send dropped
/// that way is gone for good — the drop only kicks off the handshake, it does
/// not queue the message. Waiting here makes the grant, not merely discovery,
/// the precondition for the traffic that follows.
///
/// Note that the local agent's own info is in the peer store too and never
/// carries a grant, so `expected_remote_grants` counts remote peers only: it is
/// 1 in a two-node test, not 2.
pub(crate) async fn wait_for_access_grants(
    hc: &holochain_p2p::actor::DynHcP2p,
    dna_hash: DnaHash,
    expected_remote_grants: usize,
) {
    const ACCESS_GRANT_TIMEOUT: Duration = Duration::from_secs(30);
    const WAIT_BETWEEN_POLLS: Duration = Duration::from_millis(10);

    let space_id = dna_hash.to_k2_space();
    tokio::time::timeout(ACCESS_GRANT_TIMEOUT, async {
        loop {
            if let Some(space) = hc.test_kitsune().space_if_exists(space_id.clone()).await {
                let granted = space
                    .peer_store()
                    .get_all()
                    .await
                    .unwrap()
                    .iter()
                    .filter_map(|info| info.url.clone())
                    .filter(|url| {
                        matches!(
                            space.peer_access_state().get_access_decision(url.clone()),
                            Ok(Some(PeerAccess {
                                decision: AccessDecision::Granted,
                                ..
                            }))
                        )
                    })
                    .collect::<std::collections::HashSet<_>>()
                    .len();
                if granted >= expected_remote_grants {
                    break;
                }
            }
            tokio::time::sleep(WAIT_BETWEEN_POLLS).await;
        }
    })
    .await
    .expect("timed out waiting for hello/PoK access grants");
}

pub(crate) async fn spawn_test_bootstrap(
) -> std::io::Result<(kitsune2_bootstrap_srv::BootstrapSrv, SocketAddr)> {
    // We have mixed features between ring and aws_lc so the "lookup by crate features" doesn't
    // return a default.
    // If this is called twice due to parallel tests, ignore result, because it'll fail.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut config = kitsune2_bootstrap_srv::Config::testing();
    config.listen_address_list = vec![SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))];

    let bootstrap = tokio::task::spawn_blocking(|| {
        tracing::info!("Starting bootstrap server");
        kitsune2_bootstrap_srv::BootstrapSrv::new(config)
    })
    .await
    .unwrap()?;

    let addr = *bootstrap.listen_addrs().first().unwrap();
    tracing::info!(?addr, "Bootstrap server started");

    Ok((bootstrap, addr))
}
