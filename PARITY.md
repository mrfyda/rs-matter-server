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
| `write_attribute` | ✅ | Returns `[{ Path: { EndpointId, ClusterId, AttributeId }, Status }]` — the capitalised shape the reference sends here (unlike ACL/binding writes), verified against a device. An epoch-typed attribute is written back in Matter epoch, so a client sends and receives Unix time throughout. Both the plain and the timed (`timed_request_timeout_ms`) forms round-trip. |
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
| `get_thread_diagnostics` | ⚠️ | Both forms answer in the reference's shape: `ext_pan_id` returns that network's `ThreadDiagnosticsBatch` (or `null` when no discovered Border Router claims it), and the bare form returns the cache for every known network at once and refreshes behind it. `force` bypasses the cache, and only a *complete* batch is ever a cache hit. Nothing collects yet, so every batch is `source: "none"` with `partialReason: "no_credentials"` — see the gaps below. |
| `get_network_topology` | ✅ | Real graph derived from the nodes' Thread and Wi-Fi diagnostics: roles, neighbour links with per-direction LQI/RSSI, route-table fallback edges, unknown Thread neighbours, and synthetic Wi-Fi access points. `refresh` re-reads the diagnostics clusters. |
| `send_webrtc_provider_command` | ⚠️ | `ProvideOffer` and `SolicitOffer` are invoked on the camera's provider cluster, with payload fields resolved by name through the same metadata `device_command` uses. The camera's reply arrives on the `WebRTCTransportRequestor` cluster this node hosts and reaches the client as `webrtc_callback`. No camera has ever been on the other end of it. |
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
| `thread_diagnostics_updated` | ⚠️ | Published for each network as its batch is produced, gated as the reference gates it. Every batch is currently the empty one described above. |
| `network_topology_updated` | ✅ | Published when a node change moves the graph — a node added or removed, one coming or going, or a poll bringing back different Thread or Wi-Fi diagnostics. A rebuild that produces the same graph is not announced, and `collected_at` is excluded from that comparison so a rebuild alone is not a change. |
| `webrtc_callback` | ⚠️ | Raised for each of the four commands a camera invokes on the hosted `WebRTCTransportRequestor`: `offer`, `answer`, `ice_candidates` and `end`, carrying the session id, node, endpoint and fabric index, and the `data` object the reference's model defines for that type. Unverified against a camera. |

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

What closing each one takes, and what it takes to prove closed, is in
[docs/ROADMAP.md](docs/ROADMAP.md).

1. **Subscriptions have run against a device; node events have not.** Every
   node is subscribed to — a wildcard subscription over attributes *and*
   events, established by `crate::monitor`, its reports consumed by
   `matter::reports` and published as `attribute_updated`, `node_updated` and
   `node_event`. The attribute half is now proven on hardware: the Shelly plug
   accepts the wildcard subscription, and a change made *at the device* — its
   physical button — arrives as `attribute_updated` on an exchange the device
   itself opens, with no poll involved (the monitor skips a node with a live
   subscription entirely).

   The event half is not. The plug raises no `Switch` cluster events, so
   nothing has ever exercised `node_event` against a device, and the
   delta-encoded timestamp path in particular is still asserted only by unit
   tests.

   Polling remains, for the two cases where it is still the answer: a node that
   will not subscribe (no slots left, or a refused wildcard) keeps being
   polled, and a subscription that goes silent for twice its `max_interval` is
   dropped and re-established. `--poll-interval-secs` still sets that cadence.

   Changes this controller *caused* are still read back from the target
   endpoint as soon as the command returns (about 110 ms), which is quicker
   than waiting for the device's own report.

   What that first hardware run cost, recorded because it is the argument for
   doing this sooner: a device's report carries only what changed, and it was
   being applied as though it were a poll's wildcard read. One button press
   reduced the node from 179 attributes to the 2 in the report and announced
   endpoint 0 as removed. The priming report *is* a full read — which is why
   establishing a subscription always looked right, and why 324 tests missed
   it. Absent paths now mean different things depending on where the values
   came from; see `storage::nodes::Coverage`.
2. **A device that reboots is unreachable until this server restarts.**
   Found on hardware: power-cycling the Shelly plug left it unreachable for
   the ten minutes it was tried, and only restarting the server fixed it.

   The cause is a CASE session that outlives the peer that forgot it. A
   rebooted device has no memory of the session; rs-matter keeps its side in
   the session table, and nothing takes it out. MRP noticing the silence —
   `Too many retransmissions. Giving up` — clears only that exchange's
   retransmission and ACK state (`transport/mrp.rs`), leaving the session
   itself untouched. `Exchange::initiate` reuses an existing session whenever
   there is one and establishes a fresh CASE only when there is not, so every
   later operation is sent on a session the device will not answer. Sessions
   are removed on explicit close, on fabric removal, when marked expired, or
   as the LRU victim when the table is *full* — none of which happens to one
   dead session on a small fabric. A restart empties the table in memory,
   which is why it recovers.

   Nothing in this repository can fix it. rs-matter exposes no per-peer
   session eviction, `Matter::transport` is a private field, and
   `TransportMgr::reset` clears only the RX and TX buffers. It belongs
   upstream: a session whose MRP has given up should be marked expired, or
   eviction should be reachable.

   **The monitor is not at fault**, though an earlier version of this entry
   said it was. It notices the silent subscription and retries on schedule —
   subscribe, then poll, each ending in `RxTimeout`, then a 300 s backoff once
   the node is marked unavailable. The retries cannot succeed because they
   reuse the same dead session. What made it look wedged is that every one of
   those failure paths logs at `debug`, so at the default level a node in this
   state produces silence.

   Two smaller findings from the same run, both fixable here. `ping_node`
   answered `true` in 3.2 ms throughout, and availability followed it, because
   `ping` opens an exchange through `open()` and gets the cached session back
   without a round trip: the probe proves a session object exists, not that
   the device answers, which is exactly backwards in the case it exists to
   catch. And `maintain_node` calls `subscriptions.forget()` before awaiting
   `subscribe()`, so the entry is gone before `forget_silent` could name it
   and `stopped reporting` can never be logged.

3. **Bluetooth commissioning is Linux-only, and has never run on hardware.**
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
4. **mDNS queries go out over IPv4 only.** The one-shot browser binds an IPv4
   socket and asks the IPv4 group, so every discovery this server does —
   `discover`, `get_thread_border_routers`, and the operational resolve behind
   `get_node_ip_addresses` — depends on something answering over IPv4. A
   Wi-Fi or Ethernet device does. A Thread device has no IPv4 address at all
   and is found only because its border router advertises it on the
   infrastructure link; on an IPv6-only network nothing would be found.
   Closing this means sending the same query from an IPv6 socket to `ff02::fb`
   and collecting both, which is additive — the IPv4 query is unaffected by an
   IPv6 one failing to send.
5. **Nothing collects Thread diagnostics yet.** The shape, the cache and the
   answers are implemented — a network is named, its batch is cached for an
   hour once complete, `force` bypasses that, and each batch is announced as
   an event. What is missing is a source, so every batch says `no_credentials`
   and carries no nodes.

   Two sources exist, and the reference prefers the first. **MeshCoP** over
   CoAP/DTLS, authenticated with the `pskc` and `networkKey` of a stored Thread
   dataset: no CoAP or DTLS client is in this dependency tree, so this is a
   dependency decision as much as a coding one. **OpenThread REST**, where a
   Border Router exposes it on port 8081: ordinary HTTP against a client this
   server already has, but the current API is a JSON:API task collection —
   post an action, poll it, read the result — rather than the single GET older
   documentation describes.

   The reference also collects over a ~20 second streaming window, publishing
   partial batches as nodes answer. The event and the `partialReason` that
   carries are in place; nothing yet streams into them.
6. **An update the ledger knows about cannot be installed.** The whole
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
7. **WebRTC signalling has never met a camera.** Both halves are implemented —
   the offer goes out to the camera's provider cluster, and the
   `WebRTCTransportRequestor` this node hosts turns what comes back into
   `webrtc_callback` — and neither has run against a device. Matter cameras are
   rare; connectedhomeip's `camera-app` is what this should be verified
   against.

   Two things are known to be untested rather than merely unverified. The
   payloads large enough to need the TCP transport (an SDP is kilobytes, MRP
   carries about one) are exercised only by a handshake in the test suite, not
   by a real offer. And the node id on a callback is resolved by asking the
   accessor to match each known node, because rs-matter exposes no peer
   identity on an invoke; a camera whose node id is not in the store would
   report `null` there.

   Media itself never touches this server; the reference relays signalling
   only, as does this.

## Hardware validation

The procedure for extending this table — the rig each claim needs, the
commands, and what counts as a pass — is
[docs/HARDWARE-TESTING.md](docs/HARDWARE-TESTING.md).

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

A second run on 2026-09-10, against the current build after a factory reset,
covering what the first could not:

| Step | Result |
|---|---|
| `discover` against a factory-reset plug | Full TXT record: `long_discriminator` 1612 — matching the manual pairing code's own encoding — vendor 5264, product 1, port 5540, `commissioning_mode` 1. Only the IPv4 address is reported, which is the IPv4-only browser (gap 4) visible in practice |
| `commission_with_code`, current build | PASE → CASE → CommissioningComplete → **subscribe** → interview in **6.93 s**, on a debug build |
| A device accepts a wildcard subscription | Accepted, keepalive 300 s. Never previously proven |
| A change made *at the device* | Physical button press → `attribute_updated [2, "1/6/0", true]`, delivered on an exchange the device opened. The monitor skips subscribed nodes, so no poll was involved |
| The store survives a report | **Failed first**, then fixed: see gap 1. After the fix, one press produces exactly `attribute_updated` and `node_updated`, no `endpoint_removed`, and the node still holds all 179 attributes across both endpoints |
| `ping_node` on an unreachable node | `{"fe80::4af6:eeff:feb6:8e4c": false}`, keyed by the known address, after a 5.0 s CASE timeout |
| `remove_node` on an unreachable node | Removed locally after 10.0 s of trying, logged as `could not be decommissioned cleanly (...); removing it locally anyway`, and `node_removed` published with the bare node id |
| `device_command` `on` / `off` | 129 ms and 144 ms; exactly one `attribute_updated` each. The read-back updates the store first, so the device's own report of the same change is correctly recognised as no change rather than published twice |
| `write_attribute` | **Failed first**, then fixed: every write this server makes omitted the mandatory `TimedRequest` field and the plug rejected the action. After the fix, `Status: 0`, the value read back off the device, and the timed form works too |
| `get_node_ip_addresses` | `["192.168.1.228"]` — resolved over mDNS in 1.5 s, but **IPv4 only**. The documented "link-local IPv6 first" ordering cannot happen while the browser is IPv4-only (gap 4) |
| `get_matter_fabrics` | One fabric decoded off the device: index 1, label `Home`, vendor 65521 resolved to `[Test vendor #1]` |
| `get_icd_state` on a device without the cluster | `supported: false`, everything else null, as specified |
| `check_node_update` against the real CSA ledger | `null` in 178 ms cold, 3 ms cached. Independently confirmed correct: the ledger publishes exactly one software version for vid 5264 / pid 1, `16908353`, which is what the device runs |
| `get_network_topology` | Real graph: node 2 as a Wi-Fi station at RSSI −47, edged to a synthetic AP whose BSSID `C4:9A:31:01:51:F1` is the router's LAN MAC plus one |
| `set_acl_entry` | `status: 0` writing the fabric's own admin entry back unchanged, in the snake_case shape — the asymmetry with `write_attribute`'s capitalised one is real. The ACL read back identical afterwards, and access was retained |
| `set_node_binding` on an endpoint with no Binding cluster | `status: 195` (`UNSUPPORTED_CLUSTER`) — a per-path status in a real `WriteResponse`, which is the device answering rather than refusing the action |
| `open_commissioning_window` | The locally computed SPAKE2+ verifier was accepted, in 102 ms. `discover` then showed the node at `commissioning_mode: 2` with a *fresh* discriminator 1696, distinct from the factory 1612 |
| `device_command` with a timed invoke | `RevokeCommissioning` (cluster 60) closed that window in 279 ms; `discover` returned to `commissioning_mode: 0` and discriminator 1612 |
| The fabric label reaches the device | `set_default_fabric_label` to `RigLabel` in 106 ms, then read back off the device's own `OperationalCredentials.Fabrics`. Restored to `Home` afterwards |
| `ping_node` on a reachable node | `{"192.168.1.228": true}` in 3.4 ms, against 5.0 s for the unreachable case above |
| `interview_node` on an already-interviewed node | 265 ms and **3** events, not 179: only the attributes that actually changed are republished |
| `read_attribute` shapes | Single path, a list of three paths, and the `1/6/*` wildcard all correct, including live values such as RSSI |

One measurement from that run belongs with the gaps rather than the passes.
Commissioning published **184 events** — 179 `attribute_updated`, 2
`endpoint_added`, 2 `node_updated`, 1 `node_added` — into the 256-slot event
channel, on a connection that was also listening, which is what Home Assistant
does. That is 72% of the buffer for a two-endpoint plug, so the interview
overflow described in the roadmap is not a bridge-only concern.

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

324 tests: protocol models and envelopes, TLV↔JSON round trips, the cluster and
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
