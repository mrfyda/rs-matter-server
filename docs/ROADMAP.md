# Roadmap

What is left, what it costs, and what it takes to prove. For what the server
implements today, see [PARITY.md](../PARITY.md); for how it is put together,
[ARCHITECTURE.md](ARCHITECTURE.md).

The phased plan this file used to describe is done: the responder, device
subscriptions, node events, ICD check-ins, OTA distribution, WebRTC signalling
and the Thread diagnostics contract all landed. What follows is what that left
behind.

## First: none of it has met a device

Everything built after the responder is asserted by unit tests, by contract
tests, and by construction. No device has been on the other end of any of it.
That is the single most valuable thing left to do, and it is worth doing before
building more: it is the only thing that can say whether the shape of what is
already there is right.

The procedure — the rig, the commands, and what counts as a pass for each
claim below — is [HARDWARE-TESTING.md](HARDWARE-TESTING.md).

Most of it needs no purchase. The
[connectedhomeip](https://github.com/project-chip/connectedhomeip) example apps
run as ordinary Linux processes on the same network and behave like real
devices. Hardware is only needed where the thing being tested is physical.

| What to prove | Software rig | Hardware it actually needs |
|---|---|---|
| A device's own reports replace polling | `all-clusters-app` | the Shelly plug: press its button and expect an immediate `attribute_updated`, not one within 30 s |
| `node_event` carries real events | `all-clusters-app`; a reboot raises `StartUp` | a Matter button or switch, which raises real `Switch` cluster events |
| A device can open a session *to* this node | `all-clusters-app` | none — but see the port note below |
| An update is fetched and applied | `ota-requestor-app`, with images you sign yourself | an ESP32 or nRF52840 dev board to see it over a radio. A commercial device will not accept an image you signed |
| Nested payload fields resolve by name | `lock-app` / `thermostat-app` | none |
| A BLE-commissioned node gets an address | — | a Thread device (nothing else *must* use BLE), a border router, and the Linux BLE host |
| ICD check-ins fill in `awake` | — | a genuinely sleepy device: a battery Thread contact or motion sensor. Nothing else produces a check-in |
| WebRTC signalling round-trips | `camera-app` | a Matter camera, of which there are very few. Treat the example app as the target |
| Thread diagnostics collect | — | an **open** border router (see below) |

Two pieces of the existing setup keep earning their place: Home Assistant as
the client that drives the whole contract, and a second ecosystem controller on
the same fabric — Apple Home or Google Home — which is the only honest way to
test "a change made by another ecosystem" and the multi-admin paths around
fabric labels and ICD registration.

**One deployment change to watch.** This server now binds and advertises a real
Matter port (5540 by default, `--matter-port` to move it) instead of an
ephemeral one it never listened on. If a matterjs-server or
python-matter-server still holds that port on the same host, this one refuses
to start rather than advertise an address nothing answers on. That is
deliberate, and it is the first thing a migration will hit.

### And four things only a device generates enough of

None of these is a protocol gap — every one is code that works, and none is
waiting on a decision. They are places where the load a real device puts on
the server differs in kind from the load a test puts on it, which means the
first honest measurement of each will come from the rig rather than from
reasoning. They belong here rather than in the list below because what to do
about them depends on what the rig says, and possibly nothing.

- **A large interview closes the connection that asked for it.** A first
  interview publishes one `attribute_updated` per attribute, and the
  connection that issued it is not draining its 256-slot event channel while
  its own command runs. The Shelly's 179 fit. A bridge, a multi-endpoint
  `all-clusters-app`, or anything larger does not, and the server drops that
  connection from the event stream and closes it so the client re-syncs. What
  the rig settles is whether that is a real client experience or an
  arithmetic worry: a bridge is the device to point at it.
- **One connection handles one command at a time.** A `commission_with_code` —
  5.4 s against the Shelly — blocks every other command on that connection,
  and its event delivery, for the duration. Home Assistant uses one
  connection, and the reference is asynchronous here, so this may be a parity
  difference as well as a latency one. It is also what makes the previous
  item reachable.
- **Every changed report rewrites the whole node store.** A subscription
  report that changes an attribute serializes all of `nodes.json` and fsyncs
  it. Polling bounded that to once per node per 30 s; subscriptions do not, so
  a device that reports often writes as often as it reports. The rig that
  matters is the one most installs run — Home Assistant on a Pi with an SD
  card — with a chatty device such as a power meter subscribed. `Could not
  persist polled attributes` in the log is the symptom, and it has been seen
  once already.
- **Memory over days.** Uploaded firmware images are held in memory for the
  life of the process and never evicted, at up to 64 MiB each; the ICD
  check-in map and the Thread diagnostics cache are in memory too. Idle
  footprint is 11.4 MiB. A week of real traffic is the measurement.

## Left to build

### A source for Thread diagnostics

The contract is implemented — the batch model, the per-network cache and its
hour of validity, `force`, both forms of `get_thread_diagnostics`, and the
`thread_diagnostics_updated` event. Every network answers with an empty batch
and `no_credentials`, which is what the reference answers for a network it has
no way into. What is missing is a way in.

Two exist, and the reference prefers the first:

- **MeshCoP**, over CoAP and DTLS, authenticated with the `pskc` and
  `networkKey` of a Thread dataset `set_thread_dataset` already stores. Nothing
  in this dependency tree speaks either protocol, so this starts with choosing
  what should — the largest single decision left on this list.
- **OpenThread REST**, where a discovered border router exposes it on port
  8081. No new dependency: `ureq` is already here. But the current API is a
  JSON:API task collection — post a `getNetworkDiagnosticTask`, poll it, read
  the diagnostics item it references — not the single `GET /diagnostics` older
  documentation describes, so check what the border router in front of you
  actually serves before writing against either.

Either way, one behaviour is not built and matters as much as the source:
collection **streams**. The reference gathers over about 20 seconds and
publishes partial batches as nodes answer, refining `partialReason` until the
window closes. The event and the reason it carries are in place; nothing yet
fills them in over time.

Verifying needs an open border router — a Pi with an nRF52840 dongle running
`ot-br-posix`, or Home Assistant's OTBR add-on. Apple and Google border routers
expose no REST API and hand out no credentials, so a mesh behind one cannot be
collected from at all.

### Serving an update the ledger knows about

`update_node` serves images uploaded through `POST /ota-upload/<id>`, and
refuses a version only the CSA ledger knows about. The ledger publishes a URL
and a digest, not the image, so serving one means fetching it from the vendor's
CDN, checking it against that digest, and storing it as though it had been
uploaded — at which point everything already built takes over.

rs-matter's `ota_prov::dcl` module is a worked example of exactly this, behind
its `ota-dcl` feature, over a pluggable HTTPS client. Two things deserve care:
the digest check, because an unverified image must never reach a device
whatever the device does with its own signature check; and memory, because
images run to megabytes and the store holds them in RAM.

### A dual-stack mDNS browser

The one-shot browser asks over IPv4 only, so everything it finds depends on an
IPv4 answer — fine for Wi-Fi and Ethernet, and true of a Thread device only
because its border router re-advertises it. On an IPv6-only network nothing is
found at all.

Send the same query from an IPv6 socket to `ff02::fb` and merge the answers.
Additive by construction — the IPv4 query keeps working if the IPv6 one cannot
be sent — but not as small as it sounds: sending to a link-local multicast
group needs an outgoing interface, so it means enumerating interfaces and
sending per interface, and the merged answers need de-duplicating where both
transports carry the same instance.

### Cross-cluster structs in the payload registry

`tools/build_registry.py` resolves a payload field's struct by looking for its
tag enum in the same generated file. Four are defined in the shared `globals`
module instead — `ICECandidateStruct`, `ICEServerStruct`, `ViewportStruct` and
`TestGlobalStruct` — so the seven request fields that use them fall back to
`other`, and their fields have to be addressed by numeric TLV tag rather than
by name.

The fix is to index tag enums across the whole generated tree rather than per
file. Small, and the same class of gap the nested-struct work closed.

## Deferred

**The migration's uncopied attribute cache.** A node adopted from
matterjs-server reports unavailable with no attributes until its first
subscription or poll fills it in. Copying matter.js's decoded cache would
shorten that window on first boot and nothing else.

**matter.js's `sqlite` storage driver.** `--import-matterjs` reads the `wal`,
`file` and `json` drivers. A source written by `sqlite` is detected and
reported with the matter.js command that converts it to one of those, which
is a shorter path than teaching this reader SQL for a one-time import.

**Bluetooth anywhere but Linux.** rs-matter's BTP Central backends are
`target_os = "linux"`, so macOS needs a CoreBluetooth backend upstream — and
even with one, a plain CLI binary could not use it without an app bundle and
entitlements. Nothing in this repository can fix that.

## Done

Recorded so the list above reads as what is left, rather than as everything
there ever was. PARITY.md is the contract; `git log` is the reasoning.

- **The responder.** Exchanges a device opens are accepted and answered, by
  rs-matter's own Interaction Model, Secure Channel and BDX handlers over a
  controller-shaped data model. Five gaps sat behind that one absence.
- **Subscriptions, events, ICD.** Nodes are subscribed to rather than polled,
  their events become `node_event`, and check-ins fill in `get_icd_state`.
  Polling remains the fallback for a node that will not subscribe.
- **OTA distribution.** The provider cluster is hosted, images go out over BDX,
  and `update_node` grants access and announces this server.
- **WebRTC signalling.** Both directions, over a TCP transport chained beside
  UDP because an SDP does not fit in MRP.
- **Thread diagnostics.** The shape, the cache and the answers, pending a
  source.
- **Smaller things.** Nested payload fields by name, epoch attributes as Unix
  time, operational addresses over mDNS, vendor names from the ledger, the
  network topology as a push, and a Matter port that is actually listened on.
- **A debug build that starts.** The server runs on a 64 MiB thread of its
  own: the Matter stack's future is laid out across the stack of whatever
  executes it, and unoptimized it overflowed the 8 MiB main thread and aborted
  before the listener came up. Release fitted, by a margin nobody had
  measured. Both builds now start the same way, which is what makes `cargo
  run` usable next to a device.
