//! The controller's responder.
//!
//! Everything else in this server *initiates*: a read, a write, an invoke, a
//! commissioning flow. But a Matter node is reachable in both directions, and
//! four things a controller wants arrive as exchanges the *device* starts —
//! subscription reports, ICD check-ins, an OTA requestor's `QueryImage`, a
//! camera's WebRTC answer. Until this module existed the server ran
//! `matter.run` and nothing else, so those exchanges were accepted by nobody:
//! the device retransmitted its message until MRP gave up, and learned nothing
//! from the silence.
//!
//! This is the accept side, and it is rs-matter's own: [`Responder`] owns the
//! loop (accept, hand to a handler, log, repeat, several handlers at once as a
//! single future, so it shares the thread that owns the `!Send` `Matter`), and
//! [`Responder::new_default`] pairs the Interaction Model with the Secure
//! Channel handler exactly as an accessory would.
//!
//! **A controller is a node too.** That pairing needs a data model to answer
//! against, so one is built here. It is deliberately small: a controller is not
//! a commissionable device, and hosting the clusters one would need
//! (Operational Credentials, Administrator Commissioning, Network
//! Commissioning) would be surface serving nobody. What it hosts is what a
//! device has to reach to talk *to* a controller — the OTA Software Update
//! Provider for an update, the WebRTC Transport Requestor for a camera's
//! answer — plus the Descriptor every endpoint owes. Anything else a device
//! asks for is answered "no such endpoint", which is the truth and is what a
//! client SDK expects; the alternative was a timeout.
//!
//! The rest of what the data model is for comes with it:
//!
//! * the Interaction Model's *report* side, which is how a controller consumes
//!   the `ReportData` its subscriptions produce — rs-matter hands each one to
//!   [`ReportReceiver`] with the `(fabric, peer, subscription id)` it belongs
//!   to, which is the only way to know which node a report came from;
//! * the Secure Channel handler, which lets a device establish CASE *to* this
//!   node — needed by anything that calls back, an OTA requestor and a camera
//!   among them — and which carries the hook rs-matter provides for the one
//!   Secure Channel message an accessory would drop and a controller wants:
//!   an ICD's check-in.
//!

use std::path::PathBuf;
use std::sync::Arc;

use rs_matter::crypto::Crypto;
use rand_core::OsRng;
use rs_matter::bdx::{Bdx, BdxBuffer, PROTO_ID_BDX};
use rs_matter::dm::clusters::desc::{ClusterHandler as _, DescHandler};
use rs_matter::dm::clusters::net_comm::NetworkType;
use rs_matter::dm::clusters::app::webrtc_req;
use rs_matter::dm::clusters::ota_prov::{self, OtaBdxHandler, OtaProviderHandler};
use rs_matter::dm::devices::DEV_TYPE_OTA_PROVIDER;
use rs_matter::dm::networks::eth::EthNetwork;
use rs_matter::dm::networks::wireless::NoopWirelessNetCtl;
use rs_matter::dm::{Async, Dataver, EmptyHandler, Endpoint, EpClMatcher, Node};
use rs_matter::im::{InteractionModel, InteractionModelState, PROTO_ID_INTERACTION_MODEL};
use rs_matter::utils::storage::pooled::PooledBuffers;
use rs_matter::{clusters, devices};
use rs_matter::persist::DirKvBlobStore;
use rs_matter::respond::{ChainedExchangeHandler, Responder};
use rs_matter::sc::SecureChannel;
use rs_matter::transport::exchange::MatterBuffers;
use rs_matter::Matter;

use crate::api::ServerContext;

use super::checkin::CheckInReceiver;
use super::ota_provider::{self, ImageStore};
use super::reports::ReportReceiver;
use super::webrtc::{self, WebRtcRequestor};

/// How many exchanges may be handled at once.
///
/// Each handler holds one accepted exchange, and a device is limited to five
/// exchanges per session anyway. Four is enough for a few devices reporting at
/// the same moment and small enough that the futures stay cheap; the work
/// behind each one is short.
const HANDLERS: usize = 4;

/// Exchange-sized buffers to keep for the Interaction Model. Two per handler:
/// one for the message being read and one for the answer being written.
/// rs-matter's own default is ten, sized for an accessory that also serves
/// subscriptions to several controllers at once, which this node does not.
const BUFFER_POOL: usize = HANDLERS * 2;

/// How many subscriptions this node would serve as a publisher: none. A
/// controller subscribes, it is not subscribed to.
const SUBSCRIPTIONS: usize = 0;

/// How much room to keep for events this node would emit. It emits none —
/// there are no clusters here to emit them.
const EVENTS_BUFFER: usize = 0;

/// How many image downloads may run at once.
///
/// Each holds one staging buffer for the BDX blocks it is sending. Updates are
/// something a user starts one at a time; two is enough that a second device
/// asking mid-transfer is not turned away.
const CONCURRENT_DOWNLOADS: usize = 2;

/// The controller as a Matter node: one endpoint, serving what a device must
/// reach to be updated.
const CONTROLLER_NODE: Node<'static> = Node {
    endpoints: &[Endpoint::new(
        ota_provider::OTA_PROVIDER_ENDPOINT,
        devices!(DEV_TYPE_OTA_PROVIDER),
        clusters!(
            DescHandler::CLUSTER,
            ota_prov::FULL_CLUSTER,
            webrtc_req::FULL_CLUSTER
        ),
    )],
};

/// Accept and answer device-initiated exchanges, forever.
///
/// Runs alongside the transport on the Matter thread. It never returns: a
/// failed exchange is logged by the responder and the next one is accepted.
///
/// `storage_path` is where the Interaction Model would persist state of its
/// own. It has none to persist while this node serves no clusters, but the
/// store is real rather than a stub so that adding one later does not change
/// where its data lives.
pub async fn run<'a, C: Crypto + Clone>(
    matter: &'a Matter<'a>,
    crypto: C,
    storage_path: PathBuf,
    context: Arc<ServerContext>,
) {
    let buffers: MatterBuffers<BUFFER_POOL> = MatterBuffers::new();
    let state: InteractionModelState<EthNetwork<'_>, SUBSCRIPTIONS, EVENTS_BUFFER> =
        InteractionModelState::new(EthNetwork::new_default());
    let kv = matter.kv(DirKvBlobStore::new(storage_path));

    let reports = ReportReceiver::new(context.clone());
    let check_ins = CheckInReceiver::new(context.clone(), crypto.clone());
    let webrtc_context = context.clone();
    let images = ImageStore::new(context);

    // Every endpoint owes a Descriptor, and the provider cluster is what a
    // device invokes `QueryImage` on.
    let handlers = EmptyHandler
        .chain(
            EpClMatcher::new(
                Some(ota_provider::OTA_PROVIDER_ENDPOINT),
                Some(DescHandler::CLUSTER.id),
            ),
            Async(DescHandler::new(Dataver::new_rand(&mut OsRng)).adapt()),
        )
        .chain(
            EpClMatcher::new(
                Some(ota_provider::OTA_PROVIDER_ENDPOINT),
                Some(ota_prov::FULL_CLUSTER.id),
            ),
            OtaProviderHandler::new(Dataver::new_rand(&mut OsRng), &images).adapt(),
        )
        .chain(
            EpClMatcher::new(
                Some(webrtc::WEBRTC_REQUESTOR_ENDPOINT),
                Some(webrtc_req::FULL_CLUSTER.id),
            ),
            WebRtcRequestor::new(webrtc_context, Dataver::new_rand(&mut OsRng)).adapt(),
        );

    let data_model = InteractionModel::new_with_reports(
        matter,
        crypto.clone(),
        &buffers,
        (CONTROLLER_NODE, handlers),
        &kv,
        // A controller does not commission itself onto a network, so the
        // NetworkCommissioning side of the Interaction Model has nothing to
        // drive.
        NoopWirelessNetCtl::new(NetworkType::Ethernet),
        &reports,
        &state,
    );

    // `Responder::new_default` builds this same pair, but with a Secure
    // Channel handler that drops check-ins. This is that construction with the
    // controller's handler in place of the accessory's silence.
    // The image bytes go out over BDX, a third protocol on the same responder:
    // the device opens a BDX exchange once the cluster has told it where to
    // look.
    let download_buffers: PooledBuffers<BdxBuffer, CONCURRENT_DOWNLOADS> = PooledBuffers::new();
    let handler = ChainedExchangeHandler::new(
        PROTO_ID_INTERACTION_MODEL,
        &data_model,
        SecureChannel::new_with_handler(crypto, &data_model, check_ins),
    )
    .chain(
        PROTO_ID_BDX,
        Bdx::new(OtaBdxHandler::new(&download_buffers, &images)),
    );

    let responder = Responder::new("controller", handler, matter, 0);
    if let Err(error) = responder.run::<HANDLERS>().await {
        log::error!("The responder stopped: {:?}", error);
    }
}
