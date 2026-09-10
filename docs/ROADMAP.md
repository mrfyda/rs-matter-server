# Roadmap

How the gaps in [PARITY.md](../PARITY.md) get closed, in what order, and what
each one takes to verify. For how the server is put together, see
[ARCHITECTURE.md](ARCHITECTURE.md).

One gap is not on this list: **Bluetooth on anything but Linux**. rs-matter's
BTP Central backends are `target_os = "linux"`, so macOS needs a CoreBluetooth
backend upstream — and even with one, a plain CLI binary could not use it
without an app bundle and entitlements. Nothing here can fix that. Everything
else in PARITY.md's gap list is work in this repository against rs-matter 0.3
as it already ships.

## The one thing five gaps have in common

**This server accepts no incoming exchange.** `ws::run` drives
`matter.run(...)` and nothing else, so a message a device *starts* — a
subscription report, an ICD check-in, an OTA `QueryImage`, a WebRTC answer —
arrives at a stack with no responder behind it and is dropped.

That single absence is what is actually behind gaps 1, 5 and 6, the ICD fields
in `get_icd_state`, and the `node_event` event. The Matter-side pieces those
gaps need are all present in rs-matter 0.3: `Exchange::accept` and the IM
encodings for a receiver, `dm::clusters::ota_prov` and `bdx` for updates,
`dm::clusters::app::webrtc_prov` / `webrtc_req` and a TCP transport for camera
signalling. So the plan is one keystone phase and then the things it unlocks,
with the independent work pulled forward so it is not held behind the keystone.

## Phase 0 — independent wins

Nothing here touches the transport, and each item flips a PARITY.md row on its
own.

**A dual-stack mDNS browser** (gap 3). The one-shot browser asks over IPv4
only, so everything it finds depends on an IPv4 answer — fine for Wi-Fi and
Ethernet, and true of a Thread device only because its border router
re-advertises it. Send the same query from an IPv6 socket to `ff02::fb` as
well and merge the answers. Additive by construction: the IPv4 query keeps
working if the IPv6 one cannot be sent, which is what makes it safe to do
without a Thread network to test on.

**`network_topology_updated` as a push.** Publish after a poll or a `refresh`
recomputes the graph, rather than only building it on request.

## Phase 1 — the responder loop

Add an accept arm beside `run_transport` in `ws/mod.rs`: `Exchange::accept` in
a loop, each accepted exchange driven inside a `FuturesUnordered` — the pattern
`matter::actor` already uses for concurrent work. It has to live on the
executor thread that owns `Matter`, which is `!Send`, and it must never block
the actor's own exchanges. Concurrent exchanges are fine at the transport
layer; the serialization constraint is this server's design, not Matter's.

Dispatch on protocol id and opcode:

- IM `ReportData` → the subscription receiver (phase 2)
- Secure Channel `CheckIn` → ICD (phase 2)
- IM `InvokeRequest` → the hosted clusters (phases 3 and 4); until those exist,
  answer through `im::busy`
- anything else, or a peer that is not a commissioned node → a status response
  and drop. An unsolicited exchange is not evidence of anything.

This phase changes nothing a client can see. Its test is a contract test that
opens an exchange against the server and gets a well-formed status back.

## Phase 2 — subscriptions, events, ICD

All of this is handled at the exchange level. **No data model is needed**,
which is what makes it much cheaper than phases 3 and 4.

Establish a subscription per node after its first interview, with a registry
keyed by `(fabric, node, subscription id)` and a resubscribe when a node misses
its committed `max_int`. Route the reports into the `attribute_updated`,
`endpoint_added` and `endpoint_removed` paths that already exist — the event
shapes do not change, only what feeds them. Event reports arriving on the same
stream finally emit `node_event`, whose shape and `diagnostics` history are
already implemented and unused.

`monitor.rs` demotes from the primary path to a per-node fallback: a node whose
subscribe fails keeps being polled, and the poll doubles as the liveness check
for a subscription that has gone quiet. Keep `refresh_endpoint`'s read-back as
the fast path for changes this controller caused — 110 ms is quicker than a
device's own report.

Check-ins (`sc.rs`'s handler hook, `sc/checkin.rs`'s codec) fill in `awake` and
`next_expected_checkin` in `get_icd_state`.

## Phase 3 — OTA distribution

Needs phase 1 **plus a minimal hosted data model**: a root endpoint with
Descriptor and ACL, because an incoming invoke is access-controlled and a node
that has not been granted anything can invoke nothing. Standing that up is the
real cost of this phase and the next, and the reason both come last.

On top of it: `OtaProviderHandler` and `OtaBdxHandler` over the image store
`api::ota` already keeps, implementing `OtaImagesRegistry` and `OtaImages`
against it; an `AnnounceOTAProvider` invoke pointing the device at this node;
and the ACL entry that lets that node invoke back. `update_node` stops
returning error 11.

## Phase 4 — WebRTC signalling

On the same data model: host `WebRTCTransportRequestor`, relay the invokes it
receives out as `webrtc_callback` events, and let
`send_webrtc_provider_command` perform the invoke it already validates. Also
needs `TcpNetwork` chained alongside UDP — an SDP payload does not fit in MRP.

No media touches this server. The reference relays signalling only, and so
would this.

## Phase 5 — Thread diagnostics

Independent of everything above, and not Matter at all: an OTBR REST client (or
MeshCoP over CoAP/DTLS) against the border routers `get_thread_border_routers`
already discovers, feeding `get_thread_diagnostics` and the gated
`thread_diagnostics_updated` batches.

Sequence this one by whether there is an open border router to test against.
Without one it is unverifiable, which is what makes it a poor early pick
despite being self-contained.

## Deferred

The migration's uncopied attribute cache. One poll after startup already fills
it in, so the payoff is a shorter unavailable window on first boot and little
else.

## Sequencing

Phases 0 and 5 run in parallel with anything. `1 → 2 → 3 → 4` is a hard chain.
Most of the user-visible value is in phase 2 — 30 seconds down to immediate for
a change made at the device — and most of the cost is in 3 and 4.

## What it takes to verify

Nothing past phase 0 can be signed off on compilation alone; PARITY.md says
plainly where a claim rests on compilation only, and that list should not grow.

The good news is that most of it needs no purchase. The
[connectedhomeip](https://github.com/project-chip/connectedhomeip) example apps
run as ordinary Linux processes on the same network and behave like real
devices: `all-clusters-app` for subscriptions and events, `lock-app` and
`thermostat-app` for commands with nested payload structs, `ota-requestor-app`
for the whole of phase 3, `camera-app` for phase 4. Hardware is only needed
where the thing being tested is physical.

| Phase | Software rig | Hardware it actually needs |
|---|---|---|
| 0 — registry | `lock-app` / `thermostat-app` for a named nested payload; unit tests cover the rest | none |
| 0 — operational address | — | a device that must be commissioned over BLE, i.e. a Thread device, plus a border router and the Linux BLE host |
| 0 — DCL, topology push | — | none beyond the existing plug |
| 1 — responder | a contract test opening an exchange | none |
| 2 — subscriptions | `all-clusters-app` | the Shelly plug: press its button, expect an immediate `attribute_updated` rather than a poll |
| 2 — events | `all-clusters-app`; a reboot raises `StartUp` | a Matter button or switch raises real `Switch` cluster events |
| 2 — ICD | — | a genuinely sleepy device — a battery Thread contact or motion sensor — plus a border router. Nothing else produces a check-in |
| 3 — OTA | `ota-requestor-app`, whose images you sign yourself | an ESP32 or nRF52840 dev board if you want it over a radio. A commercial device will not accept an image you signed |
| 4 — WebRTC | `camera-app` | a Matter camera, of which there are very few. Treat the example app as the target |
| 5 — Thread diagnostics | — | an **open** border router: a Pi with an nRF52840 dongle running `ot-br-posix`, or HA's OTBR add-on, both of which expose the REST API on 8081. Apple and Google border routers expose no REST API and do not hand out the credential MeshCoP would need, so they cannot be collected from |

Two further pieces of the existing setup keep earning their place: Home
Assistant as the client that drives the whole contract, and a second ecosystem
controller on the same fabric — Apple Home or Google Home — which is the only
honest way to test "a change made by another ecosystem" and the multi-admin
paths around fabric labels and ICD registration.
