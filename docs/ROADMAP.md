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

## Deferred

**Recovering a node whose device rebooted.** A device that reboots forgets its
CASE session; rs-matter keeps its side and never removes it, so every later
operation — including the monitor's retries, which fire correctly — goes out
on a session the device will not answer. The node stays unreachable until this
server restarts. No per-peer eviction exists in rs-matter's public API, so the
fix belongs upstream: mark a session expired once its MRP has given up.
PARITY.md's gap 2 carries the trace.

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
