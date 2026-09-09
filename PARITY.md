# matterjs-server parity

The contract this server implements is the WebSocket API of
[`matter-js/matterjs-server`](https://github.com/matter-js/matterjs-server)
at **schema version 13** (minimum supported 11) — the same API Home Assistant's
Matter integration speaks. Shapes below come from the reference's own
`docs/websockets_api.md` and `packages/ws-client/src/models/model.ts` rather
than being inferred from traffic.

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
| `get_node_ip_addresses` | ⚠️ | Returns the address commissioning recorded. rs-matter resolves operational addresses internally and exposes no lookup, so `prefer_cache: false` performs a reachability check rather than a fresh mDNS resolve. |
| `remove_node` | ✅ | Sends `RemoveFabric` to the device, then forgets it locally. A node that cannot be reached is still removed locally (and logged), so an unplugged device is not undeletable. |
| `interview_node` | ✅ | Wildcard read of every endpoint; publishes the attribute, endpoint and node events it produced. |
| `ping_node` | ✅ | CASE probe with `attempts`; keyed by the node's known addresses. Updates availability. |
| `import_test_node` | ✅ | All three Home Assistant dump shapes; ids allocated from `0xFFFF_FFFE_0000_0000`. |
| `get_vendor_names` | ⚠️ | Serves the reference's static vendor table (1245 entries, decimal-keyed). No DCL lookup, so a very new vendor may be missing. |
| `set_wifi_credentials` | ✅ | Named lists, write-only secrets, and the "omit the password only for an unchanged SSID" rule. |
| `set_thread_dataset` | ✅ | Hex validation; the dataset is decoded for the credential summary. |
| `remove_wifi_credentials` / `remove_thread_dataset` | ✅ | Clearing `default` zeroes it but keeps it listed. |
| `get_all_credentials` | ✅ | `{ wifi: [{id, ssid}], thread: [{id, networkName, extPanId}] }`, `default` always present. |
| `set_default_fabric_label` | ✅ | Per-connection ownership, `--default-fabric-label` pinning, null/blank resets to `HomeAssistant`, answers `null`. The label is pushed to commissioned nodes so other ecosystems display it. |
| `get_fabric_label` | ✅ | `{ fabric_label }`. |
| `commission_with_code` | ⚠️ | QR and manual codes, mDNS discovery, PASE → AddNOC → CASE → CommissioningComplete, first interview, `node_added`. Bluetooth is tried when mDNS finds nothing, on a Linux build with the `bluetooth` feature and an adapter present; a Bluetooth-commissioned node records no IP address — see the gaps below. |
| `commission_on_network` | ✅ | Explicit `ip_addr`, or `filter_type`/`filter` (none / short discriminator / long discriminator / vendor / device type). |
| `open_commissioning_window` | ✅ | Enhanced window: a fresh passcode, a SPAKE2+ verifier computed here (validated against the Matter test vector), and manual + QR codes in the response. |
| `discover` / `discover_commissionable_nodes` | ✅ | Browses `_matterc._udp` directly and reports the full TXT record: discriminator, vendor, product, device type and name, pairing hint and instruction, MRP intervals, TCP support, addresses. Filters are applied to the results. |
| `read_attribute` | ✅ | Single path, lists, and wildcards. Values decode tag-based with base64 octet strings, matching the reference. |
| `write_attribute` | ✅ | Returns `[{ Path: { EndpointId, ClusterId, AttributeId }, Status }]` — the capitalised shape the reference sends here (unlike ACL/binding writes). |
| `device_command` | ✅ | `command_name` and named payload fields resolve through cluster metadata generated from rs-matter's own definitions (153 clusters, 411 commands). Responses decode name-based via each command's declared response struct, so a shared response (`NOCResponse`) is still named correctly. |
| `get_matter_fabrics` | ✅ | Reads `OperationalCredentials.Fabrics`, decorated with vendor names. |
| `remove_matter_fabric` | ✅ | `RemoveFabric` invoke. |
| `set_acl_entry` | ✅ | Writes the fabric-scoped ACL; returns the snake_case `AttributeWriteResult`. |
| `set_node_binding` | ✅ | Writes the endpoint's binding list; a target must name a node or a group, not both. |
| `get_icd_state` | ✅ | From `IcdManagement`; a node without the cluster reports `supported: false`. `awake` and `next_expected_checkin` are `null` — check-in traffic is not tracked. |
| `register_icd` | ✅ | Rejects other-vendor administrators with error 100 and the vendor list unless `allow_multi_admin`. |
| `unregister_icd` | ✅ | `force` skips the peer round-trip. |
| `resync_icd` | ✅ | Unregisters and reconnects; answers `null`. |
| `check_node_update` | ✅ | Locally uploaded images first, then the CSA Distributed Compliance Ledger (cached for an hour). Test vendor ids use the test ledger only with `--enable-test-net-dcl`. A version with no published image is not reported as an update. |
| `update_node` | ❌ | Reports error 11 with a reason. Delivering an image needs an OTA Provider cluster server and a BDX transfer, which are not implemented. |
| `initiate_ota_upload` | ✅ | Single-use, client-bound, expiring ticket; the HTTP endpoint parses the OTA header and stores the image. |
| `get_thread_border_routers` | ✅ | Passive `_meshcop._udp` browse reporting extended address, extended PAN id, network name, host name, addresses and vendor/model. |
| `get_thread_diagnostics` | ❌ | Returns the documented "nothing cached" answers (`null` for one network, `[]` for all). MeshCoP/OTBR collection is not implemented. `ext_pan_id` is still validated. |
| `get_network_topology` | ✅ | Real graph derived from the nodes' Thread and Wi-Fi diagnostics: roles, neighbour links with per-direction LQI/RSSI, route-table fallback edges, unknown Thread neighbours, and synthetic Wi-Fi access points. `refresh` re-reads the diagnostics clusters. |
| `send_webrtc_provider_command` | ❌ | Validates its arguments, then reports error 7 with a reason. rs-matter has no WebRTC transport. |
| `subscribe_attribute` | ❌ | Error 9, matching the reference implementation (its docs call it a stub, but its dispatcher rejects it). |

## Events

| Event | Status | Notes |
|---|---|---|
| `node_added` | ✅ | After commissioning's first interview, and on test-node import. |
| `node_updated` | ✅ | Interviews, polled changes, and availability transitions. |
| `node_removed` | ✅ | Bare node id as the payload. |
| `attribute_updated` | ✅ | `[node_id, path, value]`. Produced by reads, writes, interviews and polling. |
| `endpoint_added` / `endpoint_removed` | ✅ | Derived from the endpoints an interview or poll reports. |
| `server_info_updated` | ✅ | After credential and fabric-label changes. |
| `server_shutdown` | ✅ | Published before the listener closes. |
| `node_event` | ⚠️ | The shape and the `diagnostics` history are implemented, but nothing emits one: receiving Matter events needs the subscription path below. |
| `thread_diagnostics_updated` | ❌ | Gated opt-in is implemented; Border Routers are discovered but no collector produces diagnostics batches. |
| `network_topology_updated` | ⚠️ | Gated opt-in is implemented; the graph is built on request rather than pushed. |
| `webrtc_callback` | ❌ | No WebRTC transport. |

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

1. **Device-initiated subscriptions.** rs-matter 0.3 can *establish* a
   subscription but gives a controller no way to consume the ongoing reports,
   which arrive as device-initiated exchanges. Clients see the same
   `attribute_updated` / `node_updated` events, produced two ways:

   - **Changes this controller caused** are read back from the target endpoint
     as soon as the command returns, so a client's view updates in about
     110 ms (measured against a Shelly plug).
   - **Changes made at the device** — a physical button press, or another
     ecosystem — are found by polling (`--poll-interval-secs`, default 30 s),
     so they can take up to that long to appear.

   `crate::monitor` and `api::interaction::refresh_endpoint` are the two places
   that change when a client-side subscription receiver exists.
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
3. **A Bluetooth-commissioned node records no IP address.**
   `get_node_ip_addresses` returns `[]` for it. rs-matter resolves the
   operational address inside `complete_via_case_operational` and does not
   expose it, and a Bluetooth MAC is not an operational address — storing one
   would be wrong permanently, since nothing re-resolves that field after
   commissioning. Closing this means browsing `_matter._tcp` for the node's
   operational instance, which `matter::mdns_browser` is already equipped to
   do.
4. **Thread diagnostics.** Border Routers are discovered, but collecting
   per-node diagnostics from one needs a MeshCoP (CoAP/DTLS) or OTBR REST
   client.
5. **OTA distribution.** Update *discovery* is complete — the ledger is queried
   and local uploads are stored — but serving an image to a device needs an OTA
   Provider cluster server and a BDX transfer, which are not implemented. If the
   ledger offers an update, a client will show it and installing it will fail
   with the documented update error.

   Note that a Matter update is not the same thing as a vendor update: a device
   can be current in the ledger while the vendor's own app offers newer
   firmware over its own channel, which Matter cannot see.
6. **WebRTC.** Not supported by rs-matter.
7. **Epoch-typed attributes.** matter.js converts `epoch-s`/`epoch-us`
   attributes to Unix time using the cluster schema. Values here are reported as
   the device sent them (Matter epoch).
8. **Nested command payload fields.** Top-level payload fields resolve by name;
   fields inside a nested struct must be addressed by their numeric TLV tag.

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

254 tests: protocol models and envelopes, TLV↔JSON round trips, the cluster and
wire-naming registry, the SPAKE2+ verifier against the Matter test vector, the
mDNS browser's message parsing, the update-ledger rules, storage and restart
recovery, every command handler, 7 matterjs-server import tests that build a
source directory from real certificates and adopt it, and 18 end-to-end
contract tests over a real WebSocket (including the HTTP endpoints).

Two of those are `#[ignore]`d and need `--ignored` to run: one queries the CSA
ledger over the network, and `fabric_creation_is_not_flaky` loops fabric
creation enough times to be meaningful (see
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)).

Against real hardware on a live network, the release binary discovers a
commissionable Shelly plug over `_matterc._udp` with its full TXT record, and
completes the OTA flow end to end (reserve → HTTP upload → header parse →
`check_node_update`).
