# Roadmap

What is left. For what the server implements today and what hardware has
shown, see [PARITY.md](../PARITY.md); for how it is put together,
[ARCHITECTURE.md](ARCHITECTURE.md); for what landed and why, `git log`.

## Left to build

### MeshCoP as a second source for Thread diagnostics

Diagnostics are collected over a border router's OpenThread REST API: a
network's border routers are probed on port 8081, the mesh is queried one
node at a time through the task API (or in one read from an older build), and
the batch streams — `in_progress` as nodes arrive, then complete. That covers
an open border router: a Pi running `ot-br-posix`, or Home Assistant's OTBR
add-on.

It does not cover a mesh whose only border routers are Apple's or Google's,
which expose no REST API. The reference reaches those over **MeshCoP** — CoAP
over DTLS, authenticated with the `pskc` and `networkKey` of a Thread dataset
`set_thread_dataset` already stores — and prefers it even where REST exists,
because the border router's task API queries the mesh one node at a time.
Nothing in this dependency tree speaks CoAP or DTLS. Rust has no small
pure-Rust DTLS crate: the choices are a binding to OpenSSL or mbedTLS, which
complicates the arm64 cross-build, or `webrtc-dtls`, which is pure Rust but
large and built for a different use. Choosing is the work; the collection
state in `api::thread_diagnostics` already takes any source that hands over
nodes one at a time, and the reference's `@matter/thread-br-client` is a
complete worked example of the protocol.

Verifying REST against a real border router is in docs/HARDWARE-TESTING.md.

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
