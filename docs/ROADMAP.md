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

## The one thing five gaps had in common

**This server accepted no incoming exchange.** `ws::run` drove `matter.run(...)`
and nothing else, so a message a device *started* — a subscription report, an
ICD check-in, an OTA `QueryImage`, a WebRTC answer — arrived at a stack with no
responder behind it and was dropped.

That single absence was what sat behind gaps 1, 5 and 6, the ICD fields in
`get_icd_state`, and the `node_event` event. The Matter-side pieces those gaps
need are all present in rs-matter 0.3: `Exchange::accept` and the IM encodings
for a receiver, `dm::clusters::ota_prov` and `bdx` for updates,
`dm::clusters::app::webrtc_prov` / `webrtc_req` and a TCP transport for camera
signalling. So the plan was one keystone phase and then the things it unlocks,
with the independent work pulled forward so it was not held behind the
keystone.

Phase 1 below is that keystone, and it has landed: exchanges are accepted and
routed. What each of the phases after it adds is the handling behind one of
those routes.

## Phase 0 — independent wins

Nothing here touches the Matter transport, and each item flips a PARITY.md row
on its own. One is left:

**A dual-stack mDNS browser** (gap 3). The one-shot browser asks over IPv4
only, so everything it finds depends on an IPv4 answer — fine for Wi-Fi and
Ethernet, and true of a Thread device only because its border router
re-advertises it. Send the same query from an IPv6 socket to `ff02::fb` as
well and merge the answers. Additive by construction: the IPv4 query keeps
working if the IPv6 one cannot be sent, which is what makes it safe to do
without a Thread network to test on.

## Phase 1 — the responder loop — done

`matter::responder` runs beside the transport on the Matter thread and routes
each accepted exchange by protocol and opcode. rs-matter's own `Responder`
turned out to be the whole loop — accept, hand to a handler, log, repeat, with
a fixed number of handlers running concurrently as one future — so this was a
routing table and a wiring change rather than an accept loop written by hand.

What each arm does today, and what replaces it:

- IM `ReportData` → `InvalidSubscription`, since nothing here subscribes yet.
  Phase 2 replaces it with the receiver.
- Secure Channel `CheckIn` → dropped, which is all an unreliable sessionless
  message asks for. Phase 2 records it.
- Any other IM message → `Busy` via `im::busy`. Phase 3 replaces it with a
  hosted data model.
- Any other Secure Channel message → `Busy` via `sc::busy`. That includes
  `CASESigma1`, so a device trying to establish a session *to* this node is
  told to try later rather than met with silence. Phase 3 replaces it with
  rs-matter's real `SecureChannel` handler.
- Any other protocol → dropped.

## Phase 2 — subscriptions, events, ICD

**Attributes: done.** Every node is subscribed to with a wildcard, and its
reports are published as the events clients already know. `monitor.rs` demoted
from the primary path to the thing that establishes subscriptions, falls back
to polling for a node that will not take one, and re-establishes one that has
gone silent. This needed the data model of phase 1's second commit: rs-matter
routes a report to a `ReportDataHandler` *with the peer that sent it*, and
nothing else in its public API says who opened an accepted exchange.

**Events: done.** The subscription asks for events as well as attributes, and
the report handler turns each one into the `node_event` that had a shape, a
`diagnostics` history, and nothing emitting it.

**ICD next.** Check-ins want an `sc::AsyncScHandler`, which exists
precisely so a controller can react to the Secure Channel messages the
accessory role drops. `awake` and `next_expected_checkin` in `get_icd_state`
are what it fills in.

## Phase 3 — OTA distribution

Needs **a minimal hosted data model** on top of the responder: a root endpoint
with Descriptor and ACL, because an incoming invoke is access-controlled and a
node that has not been granted anything can invoke nothing. Standing that up is
the real cost of this phase and the next, and the reason both come last.

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

Phase 5 and what is left of phase 0 run in parallel with anything.
`2 → 3 → 4` is a hard chain on phase 1, which is done. Most of the
user-visible value is in phase 2 — 30 seconds down to immediate for a change
made at the device — and most of the cost is in 3 and 4.

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
| 1 — responder | done: a test opens a real exchange over UDP | none |
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
