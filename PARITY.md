# matterjs-server parity

The contract this server implements is the WebSocket API of
[`matter-js/matterjs-server`](https://github.com/matter-js/matterjs-server)
at **schema version 13** (minimum supported 11) — the same API Home Assistant's
Matter integration speaks. Shapes below come from the reference's own
[`docs/websockets_api.md`](https://github.com/matter-js/matterjs-server/blob/main/docs/websockets_api.md)
and
[`packages/ws-client/src/models/model.ts`](https://github.com/matter-js/matterjs-server/blob/main/packages/ws-client/src/models/model.ts)
rather than being inferred from traffic. Both are public: anything below that
says a shape is unknown is a shape nobody looked up.

"Parity" here means the wire contract: message shapes, command names, error
codes, lifecycle events, persistence, and observable behaviour. It does not
mean duplicating matter.js internals.

## Commands

All 43 commands the reference dispatches are routed; none falls through to
"unknown command", and none is advertised that the reference does not have.
The `every_advertised_command_is_routed` contract test enforces that against
`api::COMMANDS`.

| Command | Status | Notes |
|---|---|---|
| `server_info` | ✅ | Real fabric id, compressed fabric id, fabric index and controller node id from the installed fabric. |
| `diagnostics` | ✅ | `{ info, nodes, events }` — the documented key names, with the last 25 Matter events. |
| `get_loglevel` / `set_loglevel` | ✅ | Accepts the matter.js aliases `fatal` and `warn`; applies the level to the running process. |
| `start_listening` | ✅ | Returns all nodes and turns on this connection's event stream. |
| `get_nodes` / `get_node` | ✅ | `only_available` honoured. |
| `get_node_ip_addresses` | ✅ | `prefer_cache` answers from the record; without it the node's operational instance (`<compressed-fabric-id>-<node-id>._matter._tcp`) is resolved over mDNS, ordered as Matter dials them — link-local IPv6 first — and stored. A resolve nothing answers falls back to the record and a reachability check, so a node answering CASE is not reported as address-less. |
| `remove_node` | ✅ | Sends `RemoveFabric` to the device, then forgets it locally. A node that cannot be reached is still removed locally (and logged), so an unplugged device is not undeletable. |
| `interview_node` | ✅ | Wildcard read of every endpoint; publishes the attribute, endpoint and node events it produced. |
| `ping_node` | ✅ | CASE probe with `attempts`; keyed by the node's known addresses. Updates availability. |
| `import_test_node` | ✅ | All three Home Assistant dump shapes; ids allocated from `0xFFFF_FFFE_0000_0000`. |
| `get_vendor_names` | ✅ | Serves the reference's static vendor table (1245 entries, decimal-keyed). A `filter_vendors` id the table does not cover is looked up in the ledger, which is where vendor ids are assigned — 161 assigned ids are missing from that table. Cached for an hour, negative answers included; an unreachable ledger drops that one id rather than failing the call. An unfiltered request answers from the table alone, as the reference does. |
| `set_wifi_credentials` | ✅ | Named lists, write-only secrets, and the "omit the password only for an unchanged SSID" rule. |
| `set_thread_dataset` | ✅ | Hex validation; the dataset is decoded for the credential summary. |
| `remove_wifi_credentials` / `remove_thread_dataset` | ✅ | Clearing `default` zeroes it but keeps it listed. |
| `get_all_credentials` | ✅ | `{ wifi: [{id, ssid}], thread: [{id, networkName, extPanId}] }`, `default` always present. |
| `set_default_fabric_label` | ✅ | Per-connection ownership, `--default-fabric-label` pinning, null/blank resets to `HomeAssistant`, answers `null`. The label is pushed to commissioned nodes so other ecosystems display it. |
| `get_fabric_label` | ✅ | `{ fabric_label }`. |
| `commission_with_code` | ⚠️ | QR and manual codes, mDNS discovery, PASE → AddNOC → CASE → CommissioningComplete, first interview, `node_added`. Bluetooth is tried when mDNS finds nothing, on a Linux build with the `bluetooth` feature and an adapter present — see the gaps below. A node commissioned that way has no address of its own to record, so it is resolved from `_matter._tcp` once it has joined its network. |
| `commission_on_network` | ✅ | Explicit `ip_addr`, or `filter_type`/`filter` (none / short discriminator / long discriminator / vendor / device type). |
| `open_commissioning_window` | ✅ | Enhanced window: a fresh passcode, a SPAKE2+ verifier computed here (validated against the Matter test vector), and manual + QR codes in the response. |
| `discover` / `discover_commissionable_nodes` | ✅ | Browses `_matterc._udp` directly and reports the full TXT record: discriminator, vendor, product, device type and name, pairing hint and instruction, MRP intervals, TCP support, addresses. Filters are applied to the results. |
| `read_attribute` | ✅ | Single path, lists, and wildcards. Values decode tag-based with base64 octet strings, matching the reference. The 21 attributes the Matter IDL types `epoch_us` or `epoch_s` are reported as Unix time, as matter.js reports them. |
| `write_attribute` | ✅ | Returns `[{ Path: { EndpointId, ClusterId, AttributeId }, Status }]` — the capitalised shape the reference sends here (unlike ACL/binding writes). An epoch-typed attribute is written back in Matter epoch, so a client sends and receives Unix time throughout. |
| `device_command` | ✅ | `command_name` and named payload fields resolve through cluster metadata generated from rs-matter's own definitions (153 clusters, 411 commands, 58 payload structs). Fields inside a nested struct — and inside a list of structs — resolve by name too. Responses decode name-based via each command's declared response struct, so a shared response (`NOCResponse`) is still named correctly. |
| `get_matter_fabrics` | ✅ | Reads `OperationalCredentials.Fabrics`, decorated with vendor names. |
| `remove_matter_fabric` | ✅ | `RemoveFabric` invoke. |
| `set_acl_entry` | ✅ | Writes the fabric-scoped ACL; returns the snake_case `AttributeWriteResult`. |
| `set_node_binding` | ✅ | Writes the endpoint's binding list; a target must name a node or a group, not both. |
| `get_icd_state` | ✅ | From `IcdManagement`; a node without the cluster reports `supported: false`. `awake` and `next_expected_checkin` are derived from the device's own check-ins, against `ActiveModeDuration` and `IdleModeDuration`; both are `null` until it has checked in since this server started. |
| `register_icd` | ✅ | Rejects other-vendor administrators with error 100 and the vendor list unless `allow_multi_admin`. The check-in key is kept, with the other credentials, because it is the only thing that can read — or attribute — a check-in from that device. |
| `unregister_icd` | ✅ | `force` skips the peer round-trip. The stored check-in key is dropped either way. |
| `resync_icd` | ✅ | Unregisters and reconnects; answers `null`. |
| `check_node_update` | ✅ | Locally uploaded images first, then the CSA Distributed Compliance Ledger (cached for an hour). Test vendor ids use the test ledger only with `--enable-test-net-dcl`. A version with no published image is not reported as an update. |
| `update_node` | ⚠️ | Grants the node access to this server's OTA Provider cluster and invokes `AnnounceOTAProvider` on it; the device then queries, downloads over BDX, and applies on its own schedule — visible as `attribute_updated` on the requestor's `UpdateState`. Only an image uploaded here can be served: an update the ledger merely knows about is refused with error 11 and a reason. |
| `initiate_ota_upload` | ✅ | Single-use, client-bound, expiring ticket; the HTTP endpoint parses the OTA header and stores the image. |
| `get_thread_border_routers` | ✅ | Passive `_meshcop._udp` browse reporting extended address, extended PAN id, network name, host name, addresses and vendor/model. |
| `get_thread_diagnostics` | ❌ | Returns the documented "nothing cached" answers (`null` for one network, `[]` for all). MeshCoP/OTBR collection is not implemented. `ext_pan_id` is still validated. |
| `get_network_topology` | ✅ | Real graph derived from the nodes' Thread and Wi-Fi diagnostics: roles, neighbour links with per-direction LQI/RSSI, route-table fallback edges, unknown Thread neighbours, and synthetic Wi-Fi access points. `refresh` re-reads the diagnostics clusters. |
| `send_webrtc_provider_command` | ❌ | Validates its arguments, then reports error 7 with a reason. Invoking the provider command is the easy half; the answer and ICE candidates come back as invokes on a `WebRTCTransportRequestor` server this node does not host — see the gaps below. |
| `subscribe_attribute` | ❌ | Error 9, matching the reference implementation (its docs call it a stub, but its dispatcher rejects it). |

## Events

| Event | Status | Notes |
|---|---|---|
| `node_added` | ✅ | After commissioning's first interview, and on test-node import. |
| `node_updated` | ✅ | Interviews, reported and polled changes, and availability transitions. |
| `node_removed` | ✅ | Bare node id as the payload. |
| `attribute_updated` | ✅ | `[node_id, path, value]`. Produced by a node's own subscription reports, and by reads, writes, interviews and polling. |
| `endpoint_added` / `endpoint_removed` | ✅ | Derived from the endpoints an interview or poll reports. |
| `server_info_updated` | ✅ | After credential and fabric-label changes. |
| `server_shutdown` | ✅ | Published before the listener closes. |
| `node_event` | ✅ | Emitted from the event reports a node's subscription produces, with the endpoint, cluster, event id, number, priority and timestamp the device sent. A delta-encoded timestamp is resolved against the previous event in the same report. |
| `thread_diagnostics_updated` | ❌ | Gated opt-in is implemented; Border Routers are discovered but no collector produces diagnostics batches. |
| `network_topology_updated` | ✅ | Published when a node change moves the graph — a node added or removed, one coming or going, or a poll bringing back different Thread or Wi-Fi diagnostics. A rebuild that produces the same graph is not announced, and `collected_at` is excluded from that comparison so a rebuild alone is not a change. |
| `webrtc_callback` | ❌ | A device raises these by invoking `WebRTCTransportRequestor` on the controller; nothing here hosts that cluster. |

Event gating matches the reference: nothing is delivered before
`start_listening`, and the Thread, topology and WebRTC events additionally
require the connection to have issued the corresponding command — which latches
even when that command returns an error.

## Migration

`--import-matterjs` adopts a matterjs-server installation's fabric so that its
devices do not have to be re-commissioned. What crosses over, and what does
not:

| matter.js | here | |
|---|---|---|
| `Fabric.Config` — root certificate, controller NOC, ICAC, operational key, IPK, fabric and node ids | the rs-matter fabric | ✅ Installed as-is; rs-matter re-derives the node, fabric and compressed fabric ids from the certificates and cross-checks them against what the source announced. |
| the CA's root key, or its intermediate key and certificate | `controller-icac-key.bin` | ✅ Both PKI shapes work: rs-matter signs device NOCs with the root directly when there is no ICAC. |
| `nodes/commissionedNodes` and the per-node commissioning state | `nodes.json` | ✅ Node ids, commissioning dates, last known addresses, and the fabric index the device assigned this controller. |
| the `config` namespace | `config.json` | ✅ Fabric label, node-id counter, and the Wi-Fi and Thread credential lists. |
| the cached attribute values | — | ⚠️ Not copied. matter.js stores them decoded into its own object model; each node is re-read instead, on its first poll after startup, and reports unavailable with no attributes until then. |
| sessions and CASE resumption records | — | ❌ Dropped; a fresh CASE handshake replaces them. |
| subscription state | — | ❌ Dropped; this server polls (gap 1 below). |

The `wal`, `file` and `json` storage drivers are read; `sqlite` is not, and is
reported with the command that converts it. The source directory is never
written to, and the import is skipped once this server has a fabric, so the
flag is safe to leave in a compose file.

Two things this cannot verify without hardware, both stated plainly: that a
device commissioned by matterjs-server accepts the imported identity, and that
a device commissioned *after* the import accepts a NOC signed by the imported
CA. Everything that decides those outcomes is asserted in `tests/migrate.rs`
against certificates rs-matter generates, but the devices themselves are the
only real proof.

## Known gaps

How these get closed, in what order, and what each one takes to verify is in
[docs/ROADMAP.md](docs/ROADMAP.md).

1. **Subscriptions have never run against a device.** Every node is subscribed
   to — a wildcard subscription over attributes *and* events, established by
   `crate::monitor`, its reports consumed by `matter::reports` and published as
   `attribute_updated`, `node_updated` and `node_event` — so a change made at a
   device should now appear at once rather than within a poll interval. What
   has not happened is a device doing it: the whole path is asserted by unit
   tests and by construction, and the Shelly plug in the hardware table below
   was measured against the polling build.

   Polling remains, for the two cases where it is still the answer: a node that
   will not subscribe (no slots left, or a refused wildcard) keeps being
   polled, and a subscription that goes silent for twice its `max_interval` is
   dropped and re-established. `--poll-interval-secs` still sets that cadence.

   Changes this controller *caused* are still read back from the target
   endpoint as soon as the command returns (about 110 ms), which is quicker
   than waiting for the device's own report.
2. **Bluetooth commissioning is Linux-only, and has never run on hardware.**
   `commission_with_code` scans over Bluetooth when mDNS finds nothing. The
   device is reached over BTP, handed its Wi-Fi or Thread credentials over the
   PASE session between AddNOC and CASE, and then resolved over mDNS once it
   has joined its network. Which credentials are sent is decided by the
   device's `NetworkCommissioning` feature map rather than by what happens to
   be stored, and each refusal (`AuthFailure`, `NetworkNotFound`,
   `UnsupportedSecurity`, `IPV6Failed`) is reported as itself.

   Three limits:

   - **Linux only.** rs-matter's BTP Central backends are `target_os =
     "linux"`. macOS would need a CoreBluetooth backend that does not exist,
     and a plain CLI binary could not use one without an app bundle and
     entitlements. `server_info.bluetooth_enabled` reports what the host can
     actually do: feature compiled in, Linux, and BlueZ offering an adapter.
   - **The probe does not prove permission.** It asks BlueZ for an adapter,
     which is normally readable by anyone; starting discovery and connecting
     usually are not. A container running as uid 65532 needs the host's
     `bluetooth` group for those, so `bluetooth_enabled` can be `true` and
     commissioning still be refused. The compose file says how.
   - **No hardware run.** Every part of this is verified by compilation only.
     It has never commissioned a real device.
3. **mDNS queries go out over IPv4 only.** The one-shot browser binds an IPv4
   socket and asks the IPv4 group, so every discovery this server does —
   `discover`, `get_thread_border_routers`, and the operational resolve behind
   `get_node_ip_addresses` — depends on something answering over IPv4. A
   Wi-Fi or Ethernet device does. A Thread device has no IPv4 address at all
   and is found only because its border router advertises it on the
   infrastructure link; on an IPv6-only network nothing would be found.
   Closing this means sending the same query from an IPv6 socket to `ff02::fb`
   and collecting both, which is additive — the IPv4 query is unaffected by an
   IPv6 one failing to send.
4. **Thread diagnostics are not collected.** Border Routers are discovered and
   reported; the per-node diagnostics behind them are not. The shape is
   settled — `ThreadDiagnosticsBatch` and `ThreadDiagnosticsNode` in the
   reference's `model.ts`, with an `extPanIdHex`-keyed batch, a `source` of
   `meshcop` / `otbr-rest` / `none`, and a `partialReason` while a collection
   is still filling in. What is missing is the collection itself: a MeshCoP
   (CoAP/DTLS) client, which needs the `pskc` and `networkKey` from a stored
   Thread dataset, or the OpenThread REST API where a discovered Border Router
   exposes it.

   The reference also caches a batch for about an hour, collects over a
   ~20 second streaming window, and takes a `force` argument to bypass the
   cache. None of that exists here yet.
5. **An update the ledger knows about cannot be installed.** The whole
   provider side works — `matter::responder` hosts the OTA Software Update
   Provider cluster, `update_node` grants the device access and announces this
   server to it, and the image goes out over BDX — but only for an image that
   was uploaded here through `POST /ota-upload/<id>`. `check_node_update` also
   reports updates found in the CSA ledger, and those cannot be served: the
   ledger says an image exists and where, not what is in it, so serving one
   means fetching it from the vendor's CDN first and checking it against the
   digest the ledger publishes. Until that exists, `update_node` refuses a
   ledger-only version rather than announcing a provider with nothing to send.

   None of this has run against a device. The image store, the designator
   parsing and the access grant are unit-tested; the flow through a real
   requestor is not.

   Note that a Matter update is not the same thing as a vendor update: a device
   can be current in the ledger while the vendor's own app offers newer
   firmware over its own channel, which Matter cannot see.
6. **WebRTC signalling is not relayed.** rs-matter 0.3 has both signalling
   clusters (`dm::clusters::app::webrtc_prov`, `webrtc_req`) and a TCP
   transport for the SDP payloads too large for MRP. What is missing is this
   side: `send_webrtc_provider_command` invokes `SolicitOffer` / `ProvideOffer`
   on the camera, and the camera answers by invoking on a
   `WebRTCTransportRequestor` this node would host — which becomes the
   `webrtc_callback` event (`WebRtcCallbackData` in the reference's `model.ts`:
   a session id, node, endpoint and fabric index, plus an `event_type` of
   `offer` / `answer` / `ice_candidates` / `end` and its data).

   Both halves land together or neither does: sending an offer without hosting
   the requestor would leave a client with a session id and no answer, which is
   worse than the error it gets today.

   Media itself never touches this server; the reference relays signalling
   only, as would this.

## Hardware validation

Run against a Shelly Plug S Gen3 (vendor 5264, product 1) on a live network:

| Step | Result |
|---|---|
| `commission_with_code` with an 11-digit manual code | PASE → AddNOC → CASE → CommissioningComplete → interview in **5.4 s** |
| First interview | 179 attributes across endpoints 0 and 1; `matter_version` 1.3.0 derived from DataModelRevision |
| Attribute decoding | Tag-based structs as the reference emits them, e.g. `"0/29/0": [{"0": 22, "1": 1}]` |
| `read_attribute` single and wildcard | `1/6/0` and `1/6/*` both correct |
| `device_command` `toggle` / `on` / `off` | Plug switched; `attribute_updated` fired as `[1, "1/6/0", true]`, **110-119 ms** after the command across six consecutive runs |
| Restart recovery | Node, attributes and addresses restored; CASE re-established from the persisted fabric; commands still work |
| `ping_node` | `{"fe80::<device>": true}`, keyed by the address the node answered on |
| `open_commissioning_window` | The locally computed SPAKE2+ verifier was **accepted by the device**; manual and QR codes returned |
| `get_matter_fabrics` | Fabric descriptor decoded, vendor name resolved |
| Two contending clients | Home Assistant claimed the fabric label first; the second connection was correctly ignored, exactly as the reference specifies |

Home Assistant drives the same plug — entity discovery and control — against
the published container image.

`matter_version` comes from `BasicInformation::SpecificationVersion`, which
only devices from 1.3 onwards report. Older ones fall back to
`DataModelRevision`: revision 17 reads as "1.2.0", anything lower as
"<1.2.0", and a higher revision reports nothing rather than guess at a version
it cannot map to one release.

## Verifying

```bash
cargo test --manifest-path server/Cargo.toml
```

313 tests: protocol models and envelopes, TLV↔JSON round trips, the cluster and
wire-naming registry, the SPAKE2+ verifier against the Matter test vector, the
mDNS browser's message parsing, the update-ledger rules, storage and restart
recovery, every command handler, 7 matterjs-server import tests that build a
source directory from real certificates and adopt it, 18 end-to-end contract
tests over a real WebSocket (including the HTTP endpoints), and two that open a
Matter exchange against the responder over a real round-trip — one on each
transport.

Four more are `#[ignore]`d and need `--ignored` to run: three query the CSA
ledger over the network, and `fabric_creation_is_not_flaky` loops fabric
creation enough times to be meaningful (see
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)).

Against real hardware on a live network, the release binary discovers a
commissionable Shelly plug over `_matterc._udp` with its full TXT record, and
completes the OTA flow end to end (reserve → HTTP upload → header parse →
`check_node_update`).
