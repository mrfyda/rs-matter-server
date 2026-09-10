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
//! against, which is why one is built here — an empty one. A controller serves
//! no clusters, so its node has no endpoints, and a device that reads or
//! invokes on it is told the endpoint does not exist rather than being left to
//! time out. What the data model is really for is the two things that come
//! with it:
//!
//! * the Interaction Model's *report* side, which is how a controller consumes
//!   the `ReportData` its subscriptions produce — rs-matter hands each one to
//!   [`ReportReceiver`] with the `(fabric, peer, subscription id)` it belongs
//!   to, which is the only way to know which node a report came from;
//! * the Secure Channel handler, which lets a device establish CASE *to* this
//!   node — needed by anything that calls back, an OTA requestor and a camera
//!   among them.
//!
//! Endpoints get added to this node when there is something to serve on them.

use std::path::PathBuf;
use std::sync::Arc;

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::net_comm::NetworkType;
use rs_matter::dm::networks::eth::EthNetwork;
use rs_matter::dm::networks::wireless::NoopWirelessNetCtl;
use rs_matter::dm::{EmptyHandler, Node};
use rs_matter::im::{InteractionModel, InteractionModelState};
use rs_matter::persist::DirKvBlobStore;
use rs_matter::respond::Responder;
use rs_matter::transport::exchange::MatterBuffers;
use rs_matter::Matter;

use crate::api::ServerContext;

use super::reports::ReportReceiver;

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

/// The controller's own data model: a node with no endpoints.
///
/// Being addressable is the point, not being useful. Anything sent here is
/// answered by the Interaction Model with "no such endpoint", which is the
/// truth and is what a client SDK expects; the alternative was a timeout.
type ControllerDataModel = (Node<'static>, EmptyHandler);

fn controller_data_model() -> ControllerDataModel {
    (Node::new(&[]), EmptyHandler)
}

/// Accept and answer device-initiated exchanges, forever.
///
/// Runs alongside the transport on the Matter thread. It never returns: a
/// failed exchange is logged by the responder and the next one is accepted.
///
/// `storage_path` is where the Interaction Model would persist state of its
/// own. It has none to persist while this node serves no clusters, but the
/// store is real rather than a stub so that adding one later does not change
/// where its data lives.
pub async fn run<'a, C: Crypto>(
    matter: &'a Matter<'a>,
    crypto: C,
    storage_path: PathBuf,
    context: Arc<ServerContext>,
) {
    let buffers: MatterBuffers<BUFFER_POOL> = MatterBuffers::new();
    let state: InteractionModelState<EthNetwork<'_>, SUBSCRIPTIONS, EVENTS_BUFFER> =
        InteractionModelState::new(EthNetwork::new_default());
    let kv = matter.kv(DirKvBlobStore::new(storage_path));

    let reports = ReportReceiver::new(context);
    let data_model = InteractionModel::new_with_reports(
        matter,
        crypto,
        &buffers,
        controller_data_model(),
        &kv,
        // A controller does not commission itself onto a network, so the
        // NetworkCommissioning side of the Interaction Model has nothing to
        // drive.
        NoopWirelessNetCtl::new(NetworkType::Ethernet),
        &reports,
        &state,
    );

    let responder = Responder::new_default(&data_model);
    if let Err(error) = responder.run::<HANDLERS>().await {
        log::error!("The responder stopped: {:?}", error);
    }
}
