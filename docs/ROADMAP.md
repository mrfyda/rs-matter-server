# Roadmap

What is left. For what the server implements today and what hardware has
shown, see [PARITY.md](../PARITY.md); for how it is put together,
[ARCHITECTURE.md](ARCHITECTURE.md); for what landed and why, `git log`.

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

Verifying needs an **open** border router — a Pi with an nRF52840 dongle
running `ot-br-posix`, or Home Assistant's OTBR add-on. Apple and Google
border routers expose no REST API and hand out no credentials, so a mesh
behind one cannot be collected from at all.

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
images run to megabytes and the store holds them in RAM for the life of the
process.

### A dual-stack mDNS browser

The one-shot browser asks over IPv4 only, so everything it finds depends on an
IPv4 answer. On an IPv6-only network nothing is found at all. It also makes one
documented behaviour unreachable: `get_node_ip_addresses` promises the node's
addresses "ordered as Matter dials them, link-local IPv6 first", and against a
real device it returns the IPv4 address alone.

Send the same query from an IPv6 socket to `ff02::fb` and merge the answers.
Additive by construction — the IPv4 query keeps working if the IPv6 one cannot
be sent — but not as small as it sounds: sending to a link-local multicast
group needs an outgoing interface, so it means enumerating interfaces and
sending per interface, and the merged answers need de-duplicating where both
transports carry the same instance.

Note the responder is already dual-stack; this is the browser alone.

### Cross-cluster structs in the payload registry

`tools/build_registry.py` resolves a payload field's struct by looking for its
tag enum in the same generated file. Four are defined in the shared `globals`
module instead — `ICECandidateStruct`, `ICEServerStruct`, `ViewportStruct` and
`TestGlobalStruct` — so the seven request fields that use them fall back to
`other`, and their fields have to be addressed by numeric TLV tag rather than
by name.

The fix is to index tag enums across the whole generated tree rather than per
file. Small, no hardware, no dependency decision — and it blocks the camera
work: `IceServers` on `ProvideOffer` and `SolicitOffer`, and `IceCandidates`
on `ProvideICECandidates`, are exactly these types. Worth doing before anyone
tries a real WebRTC session.

## Deferred

**Recovering a node whose device rebooted.** A device that reboots forgets its
CASE session; rs-matter keeps its side and never removes it, so every later
operation — including the monitor's retries, which fire correctly — goes out
on a session the device will not answer. The node stays unreachable until this
server restarts. No per-peer eviction exists in rs-matter's public API, so the
fix belongs upstream: mark a session expired once its MRP has given up.
PARITY.md's gap 2 carries the trace.

Two smaller things beside it *are* ours, worth doing whenever that gap is next
opened. `ping_node` reports a dead node as reachable, because `ping` is handed
the cached session with no round trip — it proves a session object exists, not
that the device answers, and availability follows it. And `maintain_node`
calls `subscriptions.forget()` before awaiting `subscribe()`, so the entry is
gone before `forget_silent` could name it and `stopped reporting` can never be
logged.

**Bluetooth commissioning connects to nothing.** The scan matches the device
and the handshake is composed, but no connection is ever issued at the HCI
level while the backend's own discovery restarts every ten seconds. Upstream,
in `rs_matter::transport::network::btp::gatt::bluez`. PARITY.md's gap 3 has
the HCI capture.

**Bluetooth anywhere but Linux.** rs-matter's BTP Central backends are
`target_os = "linux"`, so macOS needs a CoreBluetooth backend upstream — and
even with one, a plain CLI binary could not use it without an app bundle and
entitlements.

**The migration's uncopied attribute cache.** A node adopted from
matterjs-server reports unavailable with no attributes until its first
subscription or poll fills it in. Copying matter.js's decoded cache would
shorten that window on first boot and nothing else.

**matter.js's `sqlite` storage driver.** `--import-matterjs` reads the `wal`,
`file` and `json` drivers. A source written by `sqlite` is detected and
reported with the matter.js command that converts it to one of those, which
is a shorter path than teaching this reader SQL for a one-time import.

**The node store is rewritten per report.** Every subscription report that
changes an attribute serializes all of `nodes.json` and fsyncs it — measured
at 4 ms for a 13 KB store on a Pi 5's SD card, and it grows with node count.
Polling bounded this to once per node per 30 s; subscriptions do not. Whether
it needs fixing depends on a fabric chatty enough to show it, which no rig
here has yet.
