//! The controller actor.
//!
//! `Matter` is `!Send`: it carries a `dyn DeviceAttestation` and its transport
//! state, so it must stay on the executor thread that polls it. Every WebSocket
//! connection therefore reaches Matter through this one channel, which also
//! makes the serialization the Interaction Model wants (one exchange at a time)
//! fall out of the design rather than needing a lock.
//!
//! The op set is deliberately small — read, write, invoke, and the lifecycle
//! operations that need the commissioner. Everything else the protocol exposes
//! (fabric lists, ACLs, bindings, ICD registration, decommissioning) is a
//! cluster operation composed from these three in the `api` layer, so the
//! actor never grows a case per protocol command.

use std::collections::BTreeMap;
use std::net::{SocketAddr, SocketAddrV6};
use std::num::NonZeroU8;
use std::path::PathBuf;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use serde_json::Value;

use rs_matter::crypto::{CanonPkcSecretKey, Crypto};
use rs_matter::im::AttrPath;
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::Address;
use rs_matter::Matter;

use crate::protocol::error::{ApiError, ErrorCode};
use crate::protocol::model::{AttributesData, CommissionableNodeData};

use super::commissioning::commission_at_address;
use super::controller::persist_fabric;
use super::interaction;
use super::tlv_json::TlvNode;

/// How long a caller waits for the actor before giving up. Matter's own
/// retransmit budget for an unreachable node is around 30 s, so this sits just
/// above it: long enough that a slow-but-alive device still answers, short
/// enough that a wedged operation cannot pin a WebSocket request forever.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(45);
/// Commissioning runs PASE, AddNOC, CASE and CommissioningComplete end to end.
const COMMISSIONING_TIMEOUT: Duration = Duration::from_secs(180);
/// An interview reads every attribute of every endpoint.
const INTERVIEW_TIMEOUT: Duration = Duration::from_secs(120);

/// Identity of the controller's own fabric.
#[derive(Clone, Debug)]
pub struct FabricInfo {
    pub fabric_id: u64,
    pub compressed_fabric_id: u64,
    pub fabric_index: u8,
    pub node_id: u64,
    pub vendor_id: u16,
    pub label: String,
}

/// How to reach the device being commissioned.
#[derive(Clone, Debug)]
pub enum CommissionTarget {
    /// An address supplied by the caller; no discovery is performed.
    Address { address: String },
    /// Discover the device over mDNS using a commissionable filter.
    Discovered {
        filter: CommissionableFilter,
        timeout_ms: u32,
    },
}

/// A unit of Matter work.
#[derive(Debug)]
pub enum MatterOp {
    FabricInfo,
    SetFabricLabel {
        label: String,
    },
    Commission {
        target: CommissionTarget,
        passcode: u32,
        node_id: u64,
    },
    DiscoverCommissionable {
        filter: CommissionableFilter,
        timeout_ms: u32,
        limit: usize,
    },
    ReadAttributes {
        node_id: u64,
        paths: Vec<AttrPath>,
        fabric_filtered: bool,
    },
    WriteAttribute {
        node_id: u64,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        value: TlvNode,
        timed_timeout_ms: Option<u16>,
    },
    Invoke {
        node_id: u64,
        endpoint: u16,
        cluster: u32,
        command: u32,
        payload: TlvNode,
        timed_timeout_ms: Option<u16>,
        response_names: BTreeMap<u32, String>,
    },
    Interview {
        node_id: u64,
    },
    Ping {
        node_id: u64,
    },
    /// Derive a PAKE verifier for an enhanced commissioning window. It needs
    /// the actor's crypto backend, but touches no radio.
    PakeVerifier {
        passcode: u32,
        salt: Vec<u8>,
        iterations: u32,
    },
}

impl MatterOp {
    fn timeout(&self) -> Duration {
        match self {
            Self::Commission { .. } => COMMISSIONING_TIMEOUT,
            Self::Interview { .. } => INTERVIEW_TIMEOUT,
            Self::DiscoverCommissionable { timeout_ms, .. } => {
                // Discovery is bounded by its own browse window; allow a little
                // slack for the responder to hand results over.
                Duration::from_millis(*timeout_ms as u64) + Duration::from_secs(5)
            }
            _ => DEFAULT_TIMEOUT,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::FabricInfo => "fabric_info",
            Self::SetFabricLabel { .. } => "set_fabric_label",
            Self::Commission { .. } => "commission",
            Self::DiscoverCommissionable { .. } => "discover",
            Self::ReadAttributes { .. } => "read",
            Self::WriteAttribute { .. } => "write",
            Self::Invoke { .. } => "invoke",
            Self::Interview { .. } => "interview",
            Self::Ping { .. } => "ping",
            Self::PakeVerifier { .. } => "pake_verifier",
        }
    }
}

/// What an op produced.
#[derive(Debug)]
pub enum MatterOutcome {
    Empty,
    Json(Value),
    Bytes(Vec<u8>),
    Attributes(AttributesData),
    Fabric(FabricInfo),
    WriteStatus(u16),
    Commissioned {
        node_id: u64,
        address: String,
        /// The fabric slot the *device* assigned to us, needed later to
        /// remove ourselves from it.
        device_fabric_index: u8,
    },
    Discovered(Vec<CommissionableNodeData>),
}

/// One queued op and where its answer goes. Opaque to callers: it exists so
/// the actor's channel type can be named, not to be constructed elsewhere.
pub struct Request {
    op: MatterOp,
    reply: Sender<Result<MatterOutcome, ApiError>>,
}

/// The client half of the actor. Cheap to clone; safe to hold on any thread.
#[derive(Clone)]
pub struct MatterHandle {
    tx: Sender<Request>,
}

impl MatterHandle {
    /// Submit an op and wait for its reply.
    ///
    /// A timeout here means the actor is stuck on Matter I/O, not that the
    /// caller's request was invalid, so it maps to the SDK error rather than
    /// an argument error.
    pub async fn call(&self, op: MatterOp) -> Result<MatterOutcome, ApiError> {
        let timeout = op.timeout();
        let name = op.name();
        let (reply_tx, reply_rx) = async_channel::bounded(1);
        self.tx
            .send(Request {
                op,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ApiError::sdk("Matter controller is not running"))?;

        let receive = async { reply_rx.recv().await.ok() };
        let expire = async {
            async_io::Timer::after(timeout).await;
            None
        };
        match futures_lite::future::or(receive, expire).await {
            Some(result) => result,
            None => Err(ApiError::sdk(format!(
                "Matter operation '{}' timed out after {}s",
                name,
                timeout.as_secs()
            ))),
        }
    }

    pub async fn fabric_info(&self) -> Result<FabricInfo, ApiError> {
        match self.call(MatterOp::FabricInfo).await? {
            MatterOutcome::Fabric(info) => Ok(info),
            other => Err(unexpected(other)),
        }
    }

    pub async fn set_fabric_label(&self, label: &str) -> Result<(), ApiError> {
        self.call(MatterOp::SetFabricLabel {
            label: label.to_string(),
        })
        .await
        .map(|_| ())
    }

    pub async fn read_attributes(
        &self,
        node_id: u64,
        paths: Vec<AttrPath>,
        fabric_filtered: bool,
    ) -> Result<AttributesData, ApiError> {
        match self
            .call(MatterOp::ReadAttributes {
                node_id,
                paths,
                fabric_filtered,
            })
            .await?
        {
            MatterOutcome::Attributes(attributes) => Ok(attributes),
            other => Err(unexpected(other)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn write_attribute(
        &self,
        node_id: u64,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        value: TlvNode,
        timed_timeout_ms: Option<u16>,
    ) -> Result<u16, ApiError> {
        match self
            .call(MatterOp::WriteAttribute {
                node_id,
                endpoint,
                cluster,
                attribute,
                value,
                timed_timeout_ms,
            })
            .await?
        {
            MatterOutcome::WriteStatus(status) => Ok(status),
            other => Err(unexpected(other)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn invoke(
        &self,
        node_id: u64,
        endpoint: u16,
        cluster: u32,
        command: u32,
        payload: TlvNode,
        timed_timeout_ms: Option<u16>,
        response_names: BTreeMap<u32, String>,
    ) -> Result<Value, ApiError> {
        match self
            .call(MatterOp::Invoke {
                node_id,
                endpoint,
                cluster,
                command,
                payload,
                timed_timeout_ms,
                response_names,
            })
            .await?
        {
            MatterOutcome::Json(value) => Ok(value),
            other => Err(unexpected(other)),
        }
    }

    pub async fn interview(&self, node_id: u64) -> Result<AttributesData, ApiError> {
        match self.call(MatterOp::Interview { node_id }).await? {
            MatterOutcome::Attributes(attributes) => Ok(attributes),
            other => Err(unexpected(other)),
        }
    }

    pub async fn ping(&self, node_id: u64) -> Result<(), ApiError> {
        self.call(MatterOp::Ping { node_id }).await.map(|_| ())
    }

    pub async fn commission(
        &self,
        target: CommissionTarget,
        passcode: u32,
        node_id: u64,
    ) -> Result<(u64, String, u8), ApiError> {
        match self
            .call(MatterOp::Commission {
                target,
                passcode,
                node_id,
            })
            .await?
        {
            MatterOutcome::Commissioned {
                node_id,
                address,
                device_fabric_index,
            } => Ok((node_id, address, device_fabric_index)),
            other => Err(unexpected(other)),
        }
    }

    pub async fn discover(
        &self,
        filter: CommissionableFilter,
        timeout_ms: u32,
        limit: usize,
    ) -> Result<Vec<CommissionableNodeData>, ApiError> {
        match self
            .call(MatterOp::DiscoverCommissionable {
                filter,
                timeout_ms,
                limit,
            })
            .await?
        {
            MatterOutcome::Discovered(nodes) => Ok(nodes),
            other => Err(unexpected(other)),
        }
    }

    pub async fn pake_verifier(
        &self,
        passcode: u32,
        salt: Vec<u8>,
        iterations: u32,
    ) -> Result<Vec<u8>, ApiError> {
        match self
            .call(MatterOp::PakeVerifier {
                passcode,
                salt,
                iterations,
            })
            .await?
        {
            MatterOutcome::Bytes(verifier) => Ok(verifier),
            other => Err(unexpected(other)),
        }
    }
}

/// The actor answered with a shape its op never produces, which can only be a
/// bug in this module.
fn unexpected(outcome: MatterOutcome) -> ApiError {
    ApiError::new(
        ErrorCode::SdkStackError,
        format!(
            "Matter controller returned an unexpected result: {:?}",
            outcome
        ),
    )
}

/// Create the channel pair. The receiver is driven by [`run`].
pub fn channel() -> (MatterHandle, Receiver<Request>) {
    let (tx, rx) = async_channel::bounded(8);
    (MatterHandle { tx }, rx)
}

/// Everything the actor loop needs that is not `Matter` itself.
pub struct ActorContext<'a, C: Crypto> {
    pub matter: &'a Matter<'a>,
    pub crypto: C,
    pub icac_private_key: &'a CanonPkcSecretKey,
    pub storage_path: PathBuf,
}

/// Run the actor until the channel closes.
///
/// This future must be polled on the same executor thread as the Matter
/// transport; it is never spawned onto a thread pool.
pub async fn run<C: Crypto + Clone>(context: ActorContext<'_, C>, rx: Receiver<Request>) {
    while let Ok(request) = rx.recv().await {
        let name = request.op.name();
        let outcome = execute(&context, request.op).await;
        if let Err(error) = &outcome {
            log::debug!("Matter op '{}' failed: {}", name, error);
        }
        // A caller that timed out or disconnected has dropped its receiver;
        // that is normal and must not stop the actor.
        let _ = request.reply.send(outcome).await;
    }
    log::info!("Matter controller stopped");
}

async fn execute<C: Crypto + Clone>(
    context: &ActorContext<'_, C>,
    op: MatterOp,
) -> Result<MatterOutcome, ApiError> {
    let matter = context.matter;
    let fabric_index = fabric_index(matter)?;

    match op {
        MatterOp::FabricInfo => Ok(MatterOutcome::Fabric(read_fabric_info(matter)?)),

        MatterOp::SetFabricLabel { label } => {
            matter
                .with_state(|state| state.fabrics.update_label(fabric_index, &label).map(|_| ()))
                .map_err(|e| {
                    ApiError::invalid_args(format!("Invalid fabric label: {:?}", e.code()))
                })?;
            persist_fabric(matter, &context.storage_path).map_err(|e| {
                ApiError::sdk(format!("Failed to persist the fabric label: {:?}", e))
            })?;
            Ok(MatterOutcome::Empty)
        }

        MatterOp::ReadAttributes {
            node_id,
            paths,
            fabric_filtered,
        } => interaction::read_attributes(
            matter,
            context.crypto.clone(),
            fabric_index,
            node_id,
            paths,
            fabric_filtered,
        )
        .await
        .map(MatterOutcome::Attributes),

        MatterOp::WriteAttribute {
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
            timed_timeout_ms,
        } => interaction::write_attribute(
            matter,
            context.crypto.clone(),
            fabric_index,
            node_id,
            endpoint,
            cluster,
            attribute,
            value,
            timed_timeout_ms,
        )
        .await
        .map(MatterOutcome::WriteStatus),

        MatterOp::Invoke {
            node_id,
            endpoint,
            cluster,
            command,
            payload,
            timed_timeout_ms,
            response_names,
        } => interaction::invoke(
            matter,
            context.crypto.clone(),
            fabric_index,
            node_id,
            endpoint,
            cluster,
            command,
            payload,
            timed_timeout_ms,
            &response_names,
        )
        .await
        .map(MatterOutcome::Json),

        MatterOp::Interview { node_id } => interaction::read_attributes(
            matter,
            context.crypto.clone(),
            fabric_index,
            node_id,
            interaction::interview_paths(),
            false,
        )
        .await
        .map_err(|error| {
            // A failed interview has its own protocol error code, but a node
            // we could not reach at all keeps the more specific reason.
            if matches!(
                error.code,
                ErrorCode::NodeNotResolving | ErrorCode::NodeNotReady
            ) {
                error
            } else {
                ApiError::interview_failed(error.details)
            }
        })
        .map(MatterOutcome::Attributes),

        MatterOp::Ping { node_id } => {
            interaction::ping(matter, context.crypto.clone(), fabric_index, node_id)
                .await
                .map(|_| MatterOutcome::Empty)
        }

        MatterOp::PakeVerifier {
            passcode,
            salt,
            iterations,
        } => {
            super::spake2p_verifier::compute_verifier(&context.crypto, passcode, &salt, iterations)
                .map(MatterOutcome::Bytes)
        }

        MatterOp::DiscoverCommissionable {
            filter,
            timeout_ms,
            limit,
        } => discover(matter, filter, timeout_ms, limit).await,

        MatterOp::Commission {
            target,
            passcode,
            node_id,
        } => commission(context, fabric_index, target, passcode, node_id).await,
    }
}

fn fabric_index(matter: &Matter<'_>) -> Result<NonZeroU8, ApiError> {
    matter
        .with_state(|state| state.fabrics.iter().next().map(|fabric| fabric.fab_idx()))
        .ok_or_else(|| ApiError::sdk("No fabric is installed on the controller"))
}

fn read_fabric_info(matter: &Matter<'_>) -> Result<FabricInfo, ApiError> {
    matter
        .with_state(|state| {
            state.fabrics.iter().next().map(|fabric| FabricInfo {
                fabric_id: fabric.fabric_id(),
                compressed_fabric_id: fabric.compressed_fabric_id(),
                fabric_index: fabric.fab_idx().get(),
                node_id: fabric.node_id(),
                vendor_id: fabric.vendor_id(),
                label: fabric.label().to_string(),
            })
        })
        .ok_or_else(|| ApiError::sdk("No fabric is installed on the controller"))
}

/// Browse for commissionable devices.
///
/// rs-matter's browse is a rendezvous that yields one match at a time, so
/// enumerating means asking repeatedly while excluding the ids already seen.
/// The TXT record details the protocol can carry (vendor, product, device name)
/// are not surfaced by that API, so entries report what discovery does give:
/// the instance id and the address it answered on.
async fn discover(
    matter: &Matter<'_>,
    filter: CommissionableFilter,
    timeout_ms: u32,
    limit: usize,
) -> Result<MatterOutcome, ApiError> {
    let mut found = Vec::new();
    let mut seen: Vec<u64> = Vec::new();

    while found.len() < limit {
        match matter
            .transport()
            .browse_commissionable(&filter, &seen, timeout_ms)
            .await
        {
            Ok((address, instance_id)) => {
                let (ip, port) = match address {
                    Address::Udp(socket) => (socket.ip().to_string(), socket.port()),
                    other => (format!("{:?}", other), 0),
                };
                found.push(CommissionableNodeData {
                    instance_name: Some(format!("{:016X}", instance_id)),
                    port: Some(port),
                    addresses: Some(vec![ip]),
                    long_discriminator: filter.discriminator,
                    vendor_id: filter.vendor_id,
                    product_id: filter.product_id,
                    device_type: filter.device_type,
                    ..CommissionableNodeData::default()
                });
                seen.push(instance_id);
            }
            // Nothing further matched within the browse window: the
            // enumeration is complete, not failed.
            Err(_) => break,
        }
    }

    Ok(MatterOutcome::Discovered(found))
}

async fn commission<C: Crypto + Clone>(
    context: &ActorContext<'_, C>,
    fabric_index: NonZeroU8,
    target: CommissionTarget,
    passcode: u32,
    node_id: u64,
) -> Result<MatterOutcome, ApiError> {
    let address = match target {
        CommissionTarget::Address { address } => parse_address(&address)?,
        CommissionTarget::Discovered { filter, timeout_ms } => {
            let (address, _) = context
                .matter
                .transport()
                .browse_commissionable(&filter, &[], timeout_ms)
                .await
                .map_err(|_| {
                    ApiError::commission_failed(
                        "No commissionable device matching the pairing code was found on the network",
                    )
                })?;
            address
        }
    };

    let result = commission_at_address(
        context.matter,
        &context.crypto,
        context.icac_private_key,
        address,
        passcode,
        node_id,
    )
    .await
    .map_err(|error| ApiError::commission_failed(format!("{}", error)))?;

    persist_fabric(context.matter, &context.storage_path)
        .map_err(|e| ApiError::sdk(format!("Failed to persist the fabric: {:?}", e)))?;

    let _ = fabric_index;
    Ok(MatterOutcome::Commissioned {
        node_id: result.node_id,
        address: format_address(&address),
        device_fabric_index: result.device_fabric_index,
    })
}

fn parse_address(address: &str) -> Result<Address, ApiError> {
    // Callers may give a bare IP or an ip:port pair; Matter's commissioning
    // port is the default when none is supplied.
    const DEFAULT_MATTER_PORT: u16 = 5540;
    let socket: SocketAddr = address
        .parse()
        .or_else(|_| format!("{}:{}", address, DEFAULT_MATTER_PORT).parse())
        .or_else(|_| format!("[{}]:{}", address, DEFAULT_MATTER_PORT).parse())
        .map_err(|_| ApiError::invalid_args(format!("Invalid IP address '{}'", address)))?;
    let v6 = match socket {
        SocketAddr::V6(v6) => v6,
        SocketAddr::V4(v4) => SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0),
    };
    Ok(Address::Udp(v6.into()))
}

fn format_address(address: &Address) -> String {
    match address {
        Address::Udp(socket) => socket.ip().to_string(),
        other => format!("{:?}", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_accept_bare_ips_and_ip_port_pairs() {
        assert!(parse_address("192.168.1.10").is_ok());
        assert!(parse_address("192.168.1.10:5540").is_ok());
        assert!(parse_address("fd00::1").is_ok());
        assert!(parse_address("[fd00::1]:5540").is_ok());
        assert!(parse_address("not-an-address").is_err());
    }

    #[test]
    fn op_timeouts_reflect_how_long_the_work_takes() {
        assert_eq!(MatterOp::FabricInfo.timeout(), DEFAULT_TIMEOUT);
        assert_eq!(
            MatterOp::Interview { node_id: 1 }.timeout(),
            INTERVIEW_TIMEOUT
        );
        assert_eq!(
            MatterOp::Commission {
                target: CommissionTarget::Address {
                    address: "fd00::1".into()
                },
                passcode: 1,
                node_id: 1,
            }
            .timeout(),
            COMMISSIONING_TIMEOUT
        );
    }

    #[test]
    fn a_stopped_controller_reports_an_sdk_error() {
        let (handle, rx) = channel();
        drop(rx);
        let error = futures_lite::future::block_on(handle.call(MatterOp::FabricInfo)).unwrap_err();
        assert_eq!(error.code.as_i64(), 7);
        assert!(error.details.contains("not running"));
    }
}
