//! Relaying a camera's WebRTC signalling.
//!
//! Matter does not carry video. What it carries is the negotiation: a client
//! asks a camera for a stream through the WebRTC Transport *Provider* cluster
//! on the camera, and the camera answers by invoking on the WebRTC Transport
//! *Requestor* cluster — on the controller. Both directions are Matter
//! commands; the media then flows peer-to-peer, nowhere near this server.
//!
//! So this is the half that arrives: the requestor cluster this node hosts,
//! turning each command the camera invokes into the `webrtc_callback` event a
//! client is waiting for. The outgoing half is `api::webrtc`.
//!
//! **Why an SDP needs the TCP transport.** An offer or answer carries a
//! session description of several kilobytes, and MRP's payload is about one.
//! A peer only sends the large form to a node that advertises TCP support,
//! which is why `CONTROLLER_DEV_DET` sets `tcp_supported` and `ws::run` binds
//! a TCP listener beside the UDP socket.

use std::sync::Arc;

use rs_matter::dm::clusters::decl::globals::{
    ICECandidateStruct, WebRTCSessionStructArrayBuilder, WebRTCSessionStructBuilder,
};
use rs_matter::dm::clusters::decl::web_rtc_transport_requestor::{
    AnswerRequest, ClusterAsyncHandler, EndRequest, HandlerAsyncAdaptor, ICECandidatesRequest,
    OfferRequest, FULL_CLUSTER,
};
use rs_matter::dm::{ArrayAttributeRead, Cluster, Dataver, InvokeContext, ReadContext};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::tlv::TLVBuilderParent;
use rs_matter::with;
use serde_json::{json, Map, Value};

use crate::api::ServerContext;
use crate::protocol::events::Event;

/// The endpoint this server hosts the requestor cluster on.
///
/// The same one the OTA provider is on: a controller has one endpoint, and
/// what is announced to a camera is a node id, not a layout.
pub const WEBRTC_REQUESTOR_ENDPOINT: u16 = 0;

/// The camera-facing side of a WebRTC session, as this node hosts it.
pub struct WebRtcRequestor {
    context: Arc<ServerContext>,
    dataver: Dataver,
}

impl WebRtcRequestor {
    pub fn new(context: Arc<ServerContext>, dataver: Dataver) -> Self {
        Self { context, dataver }
    }

    /// Adapt this to the generic `AsyncHandler` the data model dispatches on,
    /// as rs-matter's own cluster handlers do.
    pub const fn adapt(self) -> HandlerAsyncAdaptor<Self> {
        HandlerAsyncAdaptor(self)
    }

    /// Publish one signalling step to the clients watching for it.
    ///
    /// `data` is `null` when the command's payload could not be read: the
    /// reference's own model makes it nullable, and a client learning that a
    /// camera answered is worth more than silence over an unreadable field.
    fn publish(&self, ctx: &impl InvokeContext, session_id: u16, kind: &str, data: Value) {
        let fabric_index = ctx.cmd().fab_idx;
        let node_id = self.caller(ctx);

        self.context.events.publish(Event::webrtc_callback(json!({
            "webrtc_session_id": session_id,
            "node_id": node_id,
            "endpoint_id": ctx.cmd().endpoint_id,
            "fabric_index": fabric_index,
            "event_type": kind,
            "data": data,
        })));
    }

    /// Which node invoked this.
    ///
    /// The accessor knows the subject that authenticated, but only offers to
    /// *match* it — so every node this server knows is offered to it, and the
    /// one it recognises is the caller. A camera that is not a commissioned
    /// node cannot have got this far, so a miss means the node list and the
    /// fabric disagree; the event still goes out, naming no node, because a
    /// client that sees the session id can still act on it.
    fn caller(&self, ctx: &impl InvokeContext) -> Option<u64> {
        let accessor = ctx.accessor().ok()?;
        let subjects = accessor.subjects();
        self.context
            .nodes
            .all()
            .into_iter()
            .map(|node| node.node_id)
            .find(|node_id| subjects.matches(*node_id))
    }
}

impl ClusterAsyncHandler for WebRtcRequestor {
    const CLUSTER: Cluster<'static> = FULL_CLUSTER.with_attrs(with!(required));

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    /// The sessions this node is party to: none that it tracks.
    ///
    /// A session lives between the client and the camera; this server relays
    /// the signalling and keeps no state of its own, so there is nothing to
    /// report. The attribute is mandatory, so it is answered rather than
    /// omitted.
    async fn current_sessions<P: TLVBuilderParent>(
        &self,
        _ctx: impl ReadContext,
        builder: ArrayAttributeRead<
            WebRTCSessionStructArrayBuilder<P>,
            WebRTCSessionStructBuilder<P>,
        >,
    ) -> Result<P, Error> {
        match builder {
            ArrayAttributeRead::ReadAll(array) => array.end(),
            // Every index is out of range on an empty list, which is what the
            // spec's constraint error means.
            ArrayAttributeRead::ReadOne(_, _) => Err(ErrorCode::ConstraintError.into()),
            ArrayAttributeRead::ReadNone(builder) => builder.end(),
        }
    }

    async fn handle_offer(
        &self,
        ctx: impl InvokeContext,
        request: OfferRequest<'_>,
    ) -> Result<(), Error> {
        let session_id = request.web_rtc_session_id()?;
        let data = offer_data(&request);
        self.publish(&ctx, session_id, "offer", data);
        Ok(())
    }

    async fn handle_answer(
        &self,
        ctx: impl InvokeContext,
        request: AnswerRequest<'_>,
    ) -> Result<(), Error> {
        let session_id = request.web_rtc_session_id()?;
        let data = match request.sdp() {
            Ok(sdp) => json!({ "sdp": sdp }),
            Err(_) => Value::Null,
        };
        self.publish(&ctx, session_id, "answer", data);
        Ok(())
    }

    async fn handle_ice_candidates(
        &self,
        ctx: impl InvokeContext,
        request: ICECandidatesRequest<'_>,
    ) -> Result<(), Error> {
        let session_id = request.web_rtc_session_id()?;
        let data = match request.ice_candidates() {
            Ok(candidates) => json!({
                "ice_candidates": candidates
                    .iter()
                    .filter_map(|candidate| candidate.ok().map(|c| ice_candidate(&c)))
                    .collect::<Vec<_>>(),
            }),
            Err(_) => Value::Null,
        };
        self.publish(&ctx, session_id, "ice_candidates", data);
        Ok(())
    }

    async fn handle_end(
        &self,
        ctx: impl InvokeContext,
        request: EndRequest<'_>,
    ) -> Result<(), Error> {
        let session_id = request.web_rtc_session_id()?;
        let data = match request.reason() {
            Ok(reason) => json!({ "reason": reason as u8 }),
            Err(_) => Value::Null,
        };
        self.publish(&ctx, session_id, "end", data);
        Ok(())
    }
}

/// An offer's payload: the session description, plus the ICE configuration the
/// camera wants used when one is offered.
fn offer_data(request: &OfferRequest<'_>) -> Value {
    let Ok(sdp) = request.sdp() else {
        return Value::Null;
    };

    let mut data = Map::new();
    data.insert("sdp".into(), Value::String(sdp.to_string()));

    // Both are optional on the wire. The reference types the server list as
    // opaque, so each entry is reported by its TLV tags rather than being
    // given names this server would be inventing.
    if let Ok(Some(servers)) = request.ice_servers() {
        let servers: Vec<Value> = servers
            .iter()
            .filter_map(|server| server.ok())
            .filter_map(|server| crate::matter::tlv_json::to_json(server.tlv_element()).ok())
            .collect();
        data.insert("ice_servers".into(), Value::Array(servers));
    }
    if let Ok(Some(policy)) = request.ice_transport_policy() {
        data.insert(
            "ice_transport_policy".into(),
            Value::String(policy.to_string()),
        );
    }
    Value::Object(data)
}

/// One ICE candidate, in the spelling the reference's model uses.
fn ice_candidate(candidate: &ICECandidateStruct<'_>) -> Value {
    json!({
        "candidate": candidate.candidate().map(|c| c.to_string()).unwrap_or_default(),
        "sdpMid": candidate
            .sdp_mid()
            .ok()
            .and_then(|mid| mid.into_option())
            .map(|mid| Value::String(mid.to_string()))
            .unwrap_or(Value::Null),
        "sdpMLineIndex": candidate
            .sdpm_line_index()
            .ok()
            .and_then(|index| index.into_option())
            .map(Value::from)
            .unwrap_or(Value::Null),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rs_matter::tlv::{TLVElement, TLVTag, TLVWrite};
    use rs_matter::utils::storage::WriteBuf;

    /// Build one ICE candidate as a camera would send it.
    fn candidate(buf: &mut [u8], mid: Option<&str>, line_index: Option<u16>) -> usize {
        let mut wb = WriteBuf::new(buf);
        wb.start_struct(&TLVTag::Anonymous).unwrap();
        wb.utf8(&TLVTag::Context(0), "candidate:1 1 UDP 2130706431 fe80::1 5000 typ host")
            .unwrap();
        match mid {
            Some(mid) => wb.utf8(&TLVTag::Context(1), mid).unwrap(),
            None => wb.null(&TLVTag::Context(1)).unwrap(),
        }
        match line_index {
            Some(index) => wb.u16(&TLVTag::Context(2), index).unwrap(),
            None => wb.null(&TLVTag::Context(2)).unwrap(),
        }
        wb.end_container().unwrap();
        wb.get_tail()
    }

    #[test]
    fn an_ice_candidate_keeps_the_spelling_the_reference_uses() {
        let mut buf = [0u8; 256];
        let len = candidate(&mut buf, Some("0"), Some(0));
        let value = ice_candidate(&ICECandidateStruct::new(TLVElement::new(&buf[..len])));

        assert_eq!(value["sdpMid"], json!("0"));
        assert_eq!(value["sdpMLineIndex"], json!(0));
        assert!(value["candidate"].as_str().unwrap().starts_with("candidate:1"));
    }

    /// Both are nullable on the wire and null in the reference's model, so a
    /// candidate without them reports null rather than omitting the field.
    #[test]
    fn a_candidate_without_a_media_line_reports_nulls() {
        let mut buf = [0u8; 256];
        let len = candidate(&mut buf, None, None);
        let value = ice_candidate(&ICECandidateStruct::new(TLVElement::new(&buf[..len])));

        assert_eq!(value["sdpMid"], Value::Null);
        assert_eq!(value["sdpMLineIndex"], Value::Null);
    }

    fn offer(buf: &mut [u8], policy: Option<&str>) -> usize {
        let mut wb = WriteBuf::new(buf);
        wb.start_struct(&TLVTag::Anonymous).unwrap();
        wb.u16(&TLVTag::Context(0), 7).unwrap();
        wb.utf8(&TLVTag::Context(1), "v=0\r\no=- 0 0 IN IP6 ::1\r\n")
            .unwrap();
        if let Some(policy) = policy {
            wb.utf8(&TLVTag::Context(3), policy).unwrap();
        }
        wb.end_container().unwrap();
        wb.get_tail()
    }

    #[test]
    fn an_offer_carries_its_session_description() {
        let mut buf = [0u8; 512];
        let len = offer(&mut buf, None);
        let value = offer_data(&OfferRequest::new(TLVElement::new(&buf[..len])));

        assert!(value["sdp"].as_str().unwrap().starts_with("v=0"));
        // Absent on the wire, so absent here rather than null: the reference
        // marks both optional.
        assert!(value.get("ice_transport_policy").is_none());
        assert!(value.get("ice_servers").is_none());
    }

    #[test]
    fn an_offers_transport_policy_comes_through_when_it_is_sent() {
        let mut buf = [0u8; 512];
        let len = offer(&mut buf, Some("relay"));
        let value = offer_data(&OfferRequest::new(TLVElement::new(&buf[..len])));
        assert_eq!(value["ice_transport_policy"], json!("relay"));
    }

    /// An offer with no session description is not one: the reference makes
    /// the whole `data` object nullable for exactly this.
    #[test]
    fn an_offer_with_no_description_reports_null_data() {
        let mut buf = [0u8; 64];
        let mut wb = WriteBuf::new(&mut buf);
        wb.start_struct(&TLVTag::Anonymous).unwrap();
        wb.u16(&TLVTag::Context(0), 7).unwrap();
        wb.end_container().unwrap();
        let len = wb.get_tail();

        let value = offer_data(&OfferRequest::new(TLVElement::new(&buf[..len])));
        assert_eq!(value, Value::Null);
    }
}
