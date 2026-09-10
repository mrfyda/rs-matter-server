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
//! This is the accept side. `rs-matter`'s [`Responder`] owns the loop — accept
//! an exchange, hand it to a handler, log what happened, go again — with a
//! fixed number of handlers running concurrently as one future, so it needs no
//! executor of its own and can share the thread that owns the `!Send` `Matter`.
//!
//! What each protocol gets is a routing decision, kept in [`Disposition`] so it
//! can be read and tested in one place. Phase by phase the arms move from
//! "answer honestly that we cannot" to real handling: the subscription
//! receiver, the ICD check-in tracker, and the hosted OTA Provider and WebRTC
//! Requestor clusters each replace one of them.
//!
//! **Why answer at all, rather than keep ignoring them.** A status response is
//! what the peer is entitled to: `Busy` names a condition it can retry after,
//! and `InvalidSubscription` tells a device its subscription is gone so it can
//! stop reporting into a void. Silence says the same thing only after a
//! retransmit budget expires, and says it less precisely.

use rs_matter::error::Error;
use rs_matter::im::busy::BusyInteractionModel;
use rs_matter::im::{IMStatusCode, OpCode, StatusResp, PROTO_ID_INTERACTION_MODEL};
use rs_matter::respond::{ExchangeHandler, Responder};
use rs_matter::sc::busy::BusySecureChannel;
use rs_matter::sc::{OpCode as ScOpCode, PROTO_ID_SECURE_CHANNEL};
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

/// How many exchanges may be handled at once.
///
/// Each handler holds one accepted exchange, and a device is limited to five
/// exchanges per session anyway. Four is enough for a few devices reporting at
/// the same moment and small enough that the futures stay cheap; the work
/// behind each one is short.
const HANDLERS: usize = 4;

/// What to do with an exchange a device opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// An ongoing subscription's `ReportData`. Nothing here subscribes yet, so
    /// any report is for a subscription this server does not have — most
    /// likely one belonging to the matterjs-server installation whose fabric
    /// was imported, which the device still believes in.
    UnknownSubscription,
    /// An ICD check-in: a sleepy device saying it is briefly awake. Sent
    /// sessionlessly and unreliably, so it wants no answer.
    CheckIn,
    /// An Interaction Model request. A controller hosts no clusters yet, so
    /// there is nothing to read, write or invoke on it.
    BusyInteractionModel,
    /// A Secure Channel handshake. A device establishing CASE to this node
    /// wants to reach a cluster server that does not exist yet.
    BusySecureChannel,
    /// A protocol this node does not speak at all.
    Ignore,
}

/// Route an exchange by the protocol and opcode of its first message.
///
/// Split out from the handler because it is the whole policy: everything else
/// in this module is plumbing, and this is the part worth reading and
/// asserting on.
pub fn disposition(proto_id: u16, opcode: u8) -> Disposition {
    match proto_id {
        PROTO_ID_INTERACTION_MODEL if opcode == OpCode::ReportData as u8 => {
            Disposition::UnknownSubscription
        }
        PROTO_ID_INTERACTION_MODEL => Disposition::BusyInteractionModel,
        PROTO_ID_SECURE_CHANNEL if opcode == ScOpCode::CheckIn as u8 => Disposition::CheckIn,
        PROTO_ID_SECURE_CHANNEL => Disposition::BusySecureChannel,
        _ => Disposition::Ignore,
    }
}

/// Applies [`disposition`] to each accepted exchange.
pub struct ControllerExchangeHandler;

impl ExchangeHandler for ControllerExchangeHandler {
    async fn handle(&self, mut exchange: Exchange<'_>) -> Result<(), Error> {
        // Peek without consuming: the handlers below fetch the same message
        // again, which is how rs-matter's own chained handler works.
        exchange.recv_fetch().await?;
        let meta = exchange.rx()?.meta();

        match disposition(meta.proto_id, meta.proto_opcode) {
            Disposition::UnknownSubscription => {
                log::debug!(
                    "Exchange {}: a report arrived for a subscription this server does not have",
                    exchange.id()
                );
                // Naming the reason is what lets the device tear the
                // subscription down instead of reporting into a void until its
                // own timeout.
                status(exchange, IMStatusCode::InvalidSubscription).await
            }
            Disposition::CheckIn => {
                log::debug!("Exchange {}: an ICD check-in", exchange.id());
                // Unreliable and sessionless: dropping the exchange is the
                // whole of the protocol's expectation.
                Ok(())
            }
            Disposition::BusyInteractionModel => {
                BusyInteractionModel::new().handle(exchange).await
            }
            Disposition::BusySecureChannel => BusySecureChannel::new().handle(exchange).await,
            Disposition::Ignore => {
                log::debug!(
                    "Exchange {}: protocol {:#06x} is not one this node speaks",
                    exchange.id(),
                    meta.proto_id
                );
                Ok(())
            }
        }
    }
}

/// Send a bare Interaction Model status and end the exchange.
async fn status(mut exchange: Exchange<'_>, status: IMStatusCode) -> Result<(), Error> {
    exchange
        .send_with(|_, wb| {
            StatusResp::write(wb, status)?;
            Ok(Some(OpCode::StatusResponse.meta()))
        })
        .await
}

/// Accept and answer device-initiated exchanges, forever.
///
/// Runs alongside the transport on the Matter thread. It never returns: a
/// failed exchange is logged by the responder and the next one is accepted.
pub async fn run(matter: &Matter<'_>) {
    let responder = Responder::new("controller", ControllerExchangeHandler, matter, 0);
    if let Err(error) = responder.run::<HANDLERS>().await {
        log::error!("The responder stopped: {:?}", error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_are_recognised_as_belonging_to_no_subscription() {
        assert_eq!(
            disposition(PROTO_ID_INTERACTION_MODEL, OpCode::ReportData as u8),
            Disposition::UnknownSubscription
        );
    }

    #[test]
    fn interaction_model_requests_are_answered_busy() {
        for opcode in [
            OpCode::ReadRequest,
            OpCode::WriteRequest,
            OpCode::InvokeRequest,
            OpCode::SubscribeRequest,
        ] {
            assert_eq!(
                disposition(PROTO_ID_INTERACTION_MODEL, opcode as u8),
                Disposition::BusyInteractionModel,
                "{:?}",
                opcode
            );
        }
    }

    #[test]
    fn a_check_in_is_told_apart_from_a_handshake() {
        assert_eq!(
            disposition(PROTO_ID_SECURE_CHANNEL, ScOpCode::CheckIn as u8),
            Disposition::CheckIn
        );
        assert_eq!(
            disposition(PROTO_ID_SECURE_CHANNEL, ScOpCode::CASESigma1 as u8),
            Disposition::BusySecureChannel
        );
        assert_eq!(
            disposition(PROTO_ID_SECURE_CHANNEL, ScOpCode::PBKDFParamRequest as u8),
            Disposition::BusySecureChannel
        );
    }

    /// BDX and the User Directed Commissioning protocol both exist and neither
    /// is spoken here; nothing should be sent back to one.
    #[test]
    fn an_unknown_protocol_is_left_alone() {
        assert_eq!(disposition(0x0002, 0x01), Disposition::Ignore);
        assert_eq!(disposition(0x0003, 0x00), Disposition::Ignore);
    }
}
