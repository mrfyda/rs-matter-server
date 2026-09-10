//! A one-shot mDNS browser.
//!
//! rs-matter's responder browses only the Matter commissionable service and
//! hands back an address, not the TXT record. Three things need more than
//! that: `get_thread_border_routers` browses `_meshcop._udp`, which rs-matter
//! does not know about at all; `discover` is supposed to report the vendor,
//! product and device name a commissionable device advertises; and
//! `get_node_ip_addresses` needs a *commissioned* node's operational address,
//! which rs-matter resolves internally and does not expose.
//!
//! This is a *legacy* (one-shot) browser: the query goes out from an ephemeral
//! port, so responders answer by unicast and no multicast group has to be
//! joined — which matters because the Matter responder may already own port
//! 5353.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use async_io::Async;

/// The IPv4 mDNS group and port.
const MDNS_GROUP_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

/// Record types this browser understands.
const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_AAAA: u16 = 28;
const TYPE_SRV: u16 = 33;

/// The `IN` class with the unicast-response bit set, which is what makes a
/// one-shot query practical: responders answer this browser directly instead
/// of multicasting to a port it does not own.
const CLASS_IN_UNICAST: u16 = 0x8001;

/// One discovered service instance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceInstance {
    /// The instance label, without the service suffix.
    pub instance_name: String,
    /// The SRV target, with the trailing dot removed.
    pub host_name: Option<String>,
    pub port: Option<u16>,
    pub addresses: Vec<IpAddr>,
    /// TXT key/value pairs. Values are raw bytes because MeshCoP packs binary
    /// identifiers (the extended PAN id, the extended address) into them.
    pub txt: BTreeMap<String, Vec<u8>>,
}

impl ServiceInstance {
    /// A TXT value interpreted as UTF-8.
    pub fn txt_str(&self, key: &str) -> Option<String> {
        let value = self.txt.get(key)?;
        std::str::from_utf8(value).ok().map(str::to_string)
    }

    /// A TXT value rendered as uppercase hex, which is how the protocol
    /// reports the binary MeshCoP identifiers.
    pub fn txt_hex(&self, key: &str) -> Option<String> {
        let value = self.txt.get(key)?;
        if value.is_empty() {
            return None;
        }
        Some(value.iter().map(|byte| format!("{:02X}", byte)).collect())
    }

    /// A TXT value parsed as a decimal or hex integer.
    pub fn txt_u32(&self, key: &str) -> Option<u32> {
        let text = self.txt_str(key)?;
        let text = text.trim();
        if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            return u32::from_str_radix(hex, 16).ok();
        }
        text.parse().ok()
    }
}

/// Browse a service type (for example `_meshcop._udp.local`) for `timeout`.
///
/// Answers are aggregated across every response received in the window, since
/// a responder may split the PTR, SRV, TXT and address records across packets.
pub async fn browse(service: &str, timeout: Duration) -> std::io::Result<Vec<ServiceInstance>> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_nonblocking(true)?;
    // Keep the query on the local link, as mDNS requires.
    socket.set_multicast_ttl_v4(255)?;
    let socket = Async::new(socket)?;

    let query = build_query(service);
    socket
        .send_to(&query, SocketAddr::from((MDNS_GROUP_V4, MDNS_PORT)))
        .await?;

    let mut instances: BTreeMap<String, ServiceInstance> = BTreeMap::new();
    let mut buffer = vec![0u8; 4096];

    let deadline = async {
        async_io::Timer::after(timeout).await;
    };
    let collect = async {
        loop {
            let Ok((len, _from)) = socket.recv_from(&mut buffer).await else {
                continue;
            };
            collect_answers(&buffer[..len], service, &mut instances);
        }
    };
    futures_lite::future::or(deadline, collect).await;

    // An instance with neither an address nor a port was only referenced by a
    // PTR record; it is not usable, so it is not reported.
    Ok(instances
        .into_values()
        .filter(|instance| instance.port.is_some() || !instance.addresses.is_empty())
        .collect())
}

/// The service a commissioned node announces itself on.
pub const OPERATIONAL_SERVICE: &str = "_matter._tcp.local";

/// The instance a node announces under, as the Matter spec spells it:
/// two 64-bit ids as uppercase, zero-padded hex.
pub fn operational_instance_name(compressed_fabric_id: u64, node_id: u64) -> String {
    format!("{:016X}-{:016X}", compressed_fabric_id, node_id)
}

/// Resolve one commissioned node's operational addresses.
///
/// This is a browse of `_matter._tcp` filtered to the node's own instance
/// rather than a targeted SRV query: every commissioned device answers the
/// same query, and picking the one instance out of the answers costs nothing
/// next to a second query type to maintain.
///
/// Addresses come back in the order Matter itself prefers to dial them —
/// link-local IPv6 first — so a caller reporting only the first one reports
/// the useful one.
pub async fn resolve_operational(
    compressed_fabric_id: u64,
    node_id: u64,
    timeout: Duration,
) -> std::io::Result<Vec<IpAddr>> {
    let wanted = operational_instance_name(compressed_fabric_id, node_id);
    let instances = browse(OPERATIONAL_SERVICE, timeout).await?;
    Ok(operational_addresses(instances, &wanted))
}

/// Pick one instance's addresses out of a browse and order them for dialling.
fn operational_addresses(instances: Vec<ServiceInstance>, wanted: &str) -> Vec<IpAddr> {
    let mut addresses: Vec<IpAddr> = instances
        .into_iter()
        .filter(|instance| instance.instance_name.eq_ignore_ascii_case(wanted))
        .flat_map(|instance| instance.addresses)
        .collect();
    addresses.sort_by_key(|address| {
        // Descending by Matter's own preference, then by address so the
        // answer does not depend on the order packets happened to arrive.
        (
            u8::MAX - rs_matter::transport::network::mdns::score_ip_address(address),
            address.to_string(),
        )
    });
    addresses.dedup();
    addresses
}

/// Build a PTR query with the unicast-response bit set.
fn build_query(service: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(64);
    message.extend_from_slice(&0u16.to_be_bytes()); // id: 0, as mDNS requires
    message.extend_from_slice(&0u16.to_be_bytes()); // flags: standard query
    message.extend_from_slice(&1u16.to_be_bytes()); // one question
    message.extend_from_slice(&0u16.to_be_bytes()); // no answers
    message.extend_from_slice(&0u16.to_be_bytes()); // no authority records
    message.extend_from_slice(&0u16.to_be_bytes()); // no additional records
    encode_name(&mut message, service);
    message.extend_from_slice(&TYPE_PTR.to_be_bytes());
    message.extend_from_slice(&CLASS_IN_UNICAST.to_be_bytes());
    message
}

fn encode_name(out: &mut Vec<u8>, name: &str) {
    for label in name.trim_end_matches('.').split('.') {
        let bytes = label.as_bytes();
        out.push(bytes.len().min(63) as u8);
        out.extend_from_slice(&bytes[..bytes.len().min(63)]);
    }
    out.push(0);
}

/// Fold every record in one response into the instance map.
pub fn collect_answers(
    message: &[u8],
    service: &str,
    instances: &mut BTreeMap<String, ServiceInstance>,
) {
    let Some(records) = parse_records(message) else {
        return;
    };
    let suffix = format!(".{}", service.trim_end_matches('.'));

    // Two passes: PTR and SRV/TXT establish instances keyed by their full
    // name, then A/AAAA records attach addresses by host name.
    let mut host_to_instance: BTreeMap<String, String> = BTreeMap::new();

    for record in &records {
        match record.record_type {
            TYPE_PTR => {
                if !record
                    .name
                    .eq_ignore_ascii_case(service.trim_end_matches('.'))
                {
                    continue;
                }
                if let Some(target) = parse_name(message, record.data_offset).map(|(name, _)| name)
                {
                    instances
                        .entry(target.clone())
                        .or_insert_with(|| ServiceInstance {
                            instance_name: strip_suffix(&target, &suffix),
                            ..ServiceInstance::default()
                        });
                }
            }
            TYPE_SRV => {
                if !record
                    .name
                    .to_ascii_lowercase()
                    .ends_with(&suffix.to_ascii_lowercase())
                {
                    continue;
                }
                let data = record.data(message);
                if data.len() < 6 {
                    continue;
                }
                let port = u16::from_be_bytes([data[4], data[5]]);
                let target = parse_name(message, record.data_offset + 6).map(|(name, _)| name);

                let entry =
                    instances
                        .entry(record.name.clone())
                        .or_insert_with(|| ServiceInstance {
                            instance_name: strip_suffix(&record.name, &suffix),
                            ..ServiceInstance::default()
                        });
                entry.port = Some(port);
                if let Some(target) = target {
                    host_to_instance.insert(target.to_ascii_lowercase(), record.name.clone());
                    entry.host_name = Some(target.trim_end_matches('.').to_string());
                }
            }
            TYPE_TXT => {
                if !record
                    .name
                    .to_ascii_lowercase()
                    .ends_with(&suffix.to_ascii_lowercase())
                {
                    continue;
                }
                let entry =
                    instances
                        .entry(record.name.clone())
                        .or_insert_with(|| ServiceInstance {
                            instance_name: strip_suffix(&record.name, &suffix),
                            ..ServiceInstance::default()
                        });
                entry.txt.extend(parse_txt(record.data(message)));
            }
            _ => {}
        }
    }

    for record in &records {
        let address = match record.record_type {
            TYPE_A => {
                let data = record.data(message);
                if data.len() < 4 {
                    continue;
                }
                IpAddr::from([data[0], data[1], data[2], data[3]])
            }
            TYPE_AAAA => {
                let data = record.data(message);
                if data.len() < 16 {
                    continue;
                }
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&data[..16]);
                IpAddr::from(octets)
            }
            _ => continue,
        };

        // Match the address to the instance whose SRV named this host. Fall
        // back to any instance in this message when the host is not known,
        // which some responders require.
        let host = record.name.to_ascii_lowercase();
        let target = host_to_instance
            .get(&host)
            .cloned()
            .or_else(|| (instances.len() == 1).then(|| instances.keys().next().unwrap().clone()));
        if let Some(target) = target {
            if let Some(entry) = instances.get_mut(&target) {
                if !entry.addresses.contains(&address) {
                    entry.addresses.push(address);
                }
            }
        }
    }
}

fn strip_suffix(name: &str, suffix: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(&suffix.to_ascii_lowercase()) {
        name[..name.len() - suffix.len()].to_string()
    } else {
        name.to_string()
    }
}

/// A record header plus where its data starts.
#[derive(Clone, Debug)]
struct Record {
    name: String,
    record_type: u16,
    data_offset: usize,
    data_len: usize,
}

impl Record {
    fn data<'a>(&self, message: &'a [u8]) -> &'a [u8] {
        let end = (self.data_offset + self.data_len).min(message.len());
        message.get(self.data_offset..end).unwrap_or(&[])
    }
}

/// Parse every answer, authority and additional record.
fn parse_records(message: &[u8]) -> Option<Vec<Record>> {
    if message.len() < 12 {
        return None;
    }
    let questions = u16::from_be_bytes([message[4], message[5]]) as usize;
    let answers = u16::from_be_bytes([message[6], message[7]]) as usize;
    let authority = u16::from_be_bytes([message[8], message[9]]) as usize;
    let additional = u16::from_be_bytes([message[10], message[11]]) as usize;

    let mut offset = 12;
    for _ in 0..questions {
        let (_, next) = parse_name(message, offset)?;
        offset = next.checked_add(4)?; // type + class
    }

    let mut records = Vec::new();
    for _ in 0..(answers + authority + additional) {
        let (name, next) = parse_name(message, offset)?;
        if next + 10 > message.len() {
            break;
        }
        let record_type = u16::from_be_bytes([message[next], message[next + 1]]);
        let data_len = u16::from_be_bytes([message[next + 8], message[next + 9]]) as usize;
        let data_offset = next + 10;
        if data_offset + data_len > message.len() {
            break;
        }
        records.push(Record {
            name,
            record_type,
            data_offset,
            data_len,
        });
        offset = data_offset + data_len;
    }
    Some(records)
}

/// Decode a possibly-compressed name, returning it and the offset just past
/// the encoded form.
fn parse_name(message: &[u8], mut offset: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut end_of_name = None;
    // A compression loop would otherwise hang the parser on a crafted packet.
    let mut jumps = 0;

    loop {
        let length = *message.get(offset)?;
        if length & 0xC0 == 0xC0 {
            let pointer = (((length & 0x3F) as usize) << 8) | *message.get(offset + 1)? as usize;
            end_of_name.get_or_insert(offset + 2);
            jumps += 1;
            if jumps > 16 {
                return None;
            }
            offset = pointer;
            continue;
        }
        if length == 0 {
            end_of_name.get_or_insert(offset + 1);
            break;
        }
        let start = offset + 1;
        let end = start + length as usize;
        labels.push(
            std::str::from_utf8(message.get(start..end)?)
                .ok()?
                .to_string(),
        );
        offset = end;
    }

    Some((labels.join("."), end_of_name?))
}

/// TXT data is a sequence of length-prefixed `key=value` strings.
fn parse_txt(data: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut offset = 0;
    while offset < data.len() {
        let length = data[offset] as usize;
        offset += 1;
        if length == 0 || offset + length > data.len() {
            break;
        }
        let entry = &data[offset..offset + length];
        offset += length;
        match entry.iter().position(|byte| *byte == b'=') {
            Some(split) => {
                let Ok(key) = std::str::from_utf8(&entry[..split]) else {
                    continue;
                };
                out.insert(key.to_string(), entry[split + 1..].to_vec());
            }
            // A bare key with no value is legal and means "present".
            None => {
                if let Ok(key) = std::str::from_utf8(entry) {
                    out.insert(key.to_string(), Vec::new());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plain `IN` class, which responses carry.
    const CLASS_IN: u16 = 1;

    fn instance(name: &str, addresses: &[&str]) -> ServiceInstance {
        ServiceInstance {
            instance_name: name.to_string(),
            addresses: addresses.iter().map(|a| a.parse().unwrap()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn operational_instances_are_named_as_the_spec_spells_them() {
        assert_eq!(
            operational_instance_name(0x1234_5678_9ABC_DEF0, 1),
            "123456789ABCDEF0-0000000000000001"
        );
        assert_eq!(
            operational_instance_name(0, u64::MAX),
            "0000000000000000-FFFFFFFFFFFFFFFF"
        );
    }

    #[test]
    fn only_the_wanted_node_contributes_addresses() {
        let instances = vec![
            instance("123456789ABCDEF0-0000000000000001", &["fd00::1"]),
            instance("123456789ABCDEF0-0000000000000002", &["fd00::2"]),
        ];
        assert_eq!(
            operational_addresses(instances, "123456789ABCDEF0-0000000000000001"),
            vec!["fd00::1".parse::<IpAddr>().unwrap()]
        );
    }

    /// Responders differ on the case of hex in an instance name, and mDNS
    /// names are case-insensitive.
    #[test]
    fn the_instance_match_ignores_hex_case() {
        let instances = vec![instance("123456789abcdef0-0000000000000001", &["fd00::1"])];
        assert_eq!(
            operational_addresses(instances, "123456789ABCDEF0-0000000000000001").len(),
            1
        );
    }

    #[test]
    fn addresses_come_back_in_the_order_matter_dials_them() {
        let instances = vec![
            instance(
                "A-1",
                &["192.168.1.7", "2001:db8::1", "fe80::1", "fd00::1"],
            ),
            // The same address seen twice, as two packets can report it.
            instance("A-1", &["fe80::1"]),
        ];
        let addresses: Vec<String> = operational_addresses(instances, "A-1")
            .iter()
            .map(|address| address.to_string())
            .collect();
        assert_eq!(
            addresses,
            vec!["fe80::1", "fd00::1", "2001:db8::1", "192.168.1.7"]
        );
    }

    /// Build a response carrying PTR, SRV, TXT and A records for one instance.
    fn sample_response(service: &str, instance: &str, txt: &[(&str, &[u8])]) -> Vec<u8> {
        let mut message = Vec::new();
        message.extend_from_slice(&0u16.to_be_bytes()); // id
        message.extend_from_slice(&0x8400u16.to_be_bytes()); // authoritative response
        message.extend_from_slice(&0u16.to_be_bytes()); // no questions
        message.extend_from_slice(&4u16.to_be_bytes()); // four answers
        message.extend_from_slice(&0u16.to_be_bytes());
        message.extend_from_slice(&0u16.to_be_bytes());

        let full = format!("{}.{}", instance, service);
        let host = "br-1.local";

        let mut record = |name: &str, record_type: u16, data: &[u8]| {
            encode_name(&mut message, name);
            message.extend_from_slice(&record_type.to_be_bytes());
            message.extend_from_slice(&CLASS_IN.to_be_bytes());
            message.extend_from_slice(&120u32.to_be_bytes());
            message.extend_from_slice(&(data.len() as u16).to_be_bytes());
            message.extend_from_slice(data);
        };

        let mut ptr_data = Vec::new();
        encode_name(&mut ptr_data, &full);
        record(service, TYPE_PTR, &ptr_data);

        let mut srv_data = Vec::new();
        srv_data.extend_from_slice(&0u16.to_be_bytes()); // priority
        srv_data.extend_from_slice(&0u16.to_be_bytes()); // weight
        srv_data.extend_from_slice(&49191u16.to_be_bytes()); // port
        encode_name(&mut srv_data, host);
        record(&full, TYPE_SRV, &srv_data);

        let mut txt_data = Vec::new();
        for (key, value) in txt {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value);
            txt_data.push(entry.len() as u8);
            txt_data.extend_from_slice(&entry);
        }
        record(&full, TYPE_TXT, &txt_data);

        record(host, TYPE_A, &[192, 168, 1, 50]);
        message
    }

    #[test]
    fn a_full_response_produces_one_usable_instance() {
        let service = "_meshcop._udp.local";
        let message = sample_response(
            service,
            "OpenThread BorderRouter",
            &[
                ("nn", b"MyThreadNet"),
                ("xp", &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]),
                ("vn", b"OpenThread"),
            ],
        );

        let mut instances = BTreeMap::new();
        collect_answers(&message, service, &mut instances);
        assert_eq!(instances.len(), 1);

        let instance = instances.values().next().unwrap();
        assert_eq!(instance.instance_name, "OpenThread BorderRouter");
        assert_eq!(instance.host_name.as_deref(), Some("br-1.local"));
        assert_eq!(instance.port, Some(49191));
        assert_eq!(instance.addresses, vec![IpAddr::from([192, 168, 1, 50])]);
        assert_eq!(instance.txt_str("nn").as_deref(), Some("MyThreadNet"));
        assert_eq!(instance.txt_hex("xp").as_deref(), Some("1122334455667788"));
    }

    #[test]
    fn txt_values_decode_as_text_hex_and_numbers() {
        let mut instance = ServiceInstance::default();
        instance.txt.insert("nn".into(), b"Net".to_vec());
        instance.txt.insert("xa".into(), vec![0xAA, 0xBB]);
        instance.txt.insert("d".into(), b"3840".to_vec());
        instance.txt.insert("vp".into(), b"0xFFF1".to_vec());
        instance.txt.insert("empty".into(), Vec::new());

        assert_eq!(instance.txt_str("nn").as_deref(), Some("Net"));
        assert_eq!(instance.txt_hex("xa").as_deref(), Some("AABB"));
        assert_eq!(instance.txt_u32("d"), Some(3840));
        assert_eq!(instance.txt_u32("vp"), Some(0xFFF1));
        assert_eq!(instance.txt_hex("empty"), None);
        assert_eq!(instance.txt_str("absent"), None);
    }

    #[test]
    fn compressed_names_are_followed() {
        // "a.local" at offset 12, then a pointer back to it.
        let mut message = vec![0u8; 12];
        message[5] = 0; // no questions
        message[7] = 0; // no answers
        let start = message.len();
        encode_name(&mut message, "a.local");
        let (name, next) = parse_name(&message, start).unwrap();
        assert_eq!(name, "a.local");

        let pointer_offset = message.len();
        message.push(0xC0);
        message.push(start as u8);
        let (name, after) = parse_name(&message, pointer_offset).unwrap();
        assert_eq!(name, "a.local");
        assert_eq!(after, pointer_offset + 2);
        assert_eq!(next, pointer_offset);
    }

    #[test]
    fn a_compression_loop_does_not_hang_the_parser() {
        // A pointer at offset 12 that points at itself.
        let mut message = vec![0u8; 12];
        message.push(0xC0);
        message.push(12);
        assert!(parse_name(&message, 12).is_none());
    }

    #[test]
    fn truncated_and_malformed_messages_are_ignored() {
        let mut instances = BTreeMap::new();
        collect_answers(&[], "_meshcop._udp.local", &mut instances);
        collect_answers(&[0u8; 8], "_meshcop._udp.local", &mut instances);
        collect_answers(&[0xFF; 64], "_meshcop._udp.local", &mut instances);
        assert!(instances.is_empty());
    }

    #[test]
    fn an_instance_with_no_address_or_port_is_not_reported() {
        // A PTR-only response names an instance nothing can connect to.
        let service = "_meshcop._udp.local";
        let mut message = Vec::new();
        message.extend_from_slice(&0u16.to_be_bytes());
        message.extend_from_slice(&0x8400u16.to_be_bytes());
        message.extend_from_slice(&0u16.to_be_bytes());
        message.extend_from_slice(&1u16.to_be_bytes());
        message.extend_from_slice(&0u16.to_be_bytes());
        message.extend_from_slice(&0u16.to_be_bytes());
        let mut ptr_data = Vec::new();
        encode_name(&mut ptr_data, &format!("lonely.{}", service));
        encode_name(&mut message, service);
        message.extend_from_slice(&TYPE_PTR.to_be_bytes());
        message.extend_from_slice(&CLASS_IN.to_be_bytes());
        message.extend_from_slice(&120u32.to_be_bytes());
        message.extend_from_slice(&(ptr_data.len() as u16).to_be_bytes());
        message.extend_from_slice(&ptr_data);

        let mut instances = BTreeMap::new();
        collect_answers(&message, service, &mut instances);
        assert_eq!(instances.len(), 1);
        // ...but browse() filters it out.
        let usable: Vec<_> = instances
            .into_values()
            .filter(|instance| instance.port.is_some() || !instance.addresses.is_empty())
            .collect();
        assert!(usable.is_empty());
    }

    #[test]
    fn a_query_asks_for_a_unicast_ptr_answer() {
        let query = build_query("_meshcop._udp.local");
        assert_eq!(u16::from_be_bytes([query[4], query[5]]), 1, "one question");
        let class = u16::from_be_bytes([query[query.len() - 2], query[query.len() - 1]]);
        assert_eq!(class, CLASS_IN_UNICAST);
        let record_type = u16::from_be_bytes([query[query.len() - 4], query[query.len() - 3]]);
        assert_eq!(record_type, TYPE_PTR);
    }
}
