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
Nothing in this dependency tree speaks CoAP or DTLS, and the DTLS is not the
ordinary kind: MeshCoP authenticates with `TLS_ECJPAKE_WITH_AES_128_CCM_8`,
a password-authenticated key exchange over the PSKc. No general-purpose DTLS
crate offers that cipher suite — `webrtc-dtls` does not, and neither does
OpenSSL; mbedTLS does, behind a C binding that complicates the arm64
cross-build. The reference took the other route and wrote its own: in
`@matter/thread-br-client`, about 3,000 lines of TypeScript for the DTLS
handshake, EC-JPAKE, the record layer and CoAP, on top of P-256, AES-CCM and
SHA-256 primitives this tree already has through rs-matter's crypto. Porting
that is the realistic path, and it is pure code; the collection state in
`api::thread_diagnostics` already takes any source that hands over nodes one
at a time. What it cannot be is verified without a border router — though
for MeshCoP any border router will do, Apple's and Google's included, since
the credentials come from the dataset rather than from the router.

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

**matter.js's `sqlite` storage driver.** `--import-matterjs` reads the `wal`,
`file` and `json` drivers. A source written by `sqlite` is detected and
reported with the matter.js command that converts it to one of those, which
is a shorter path than teaching this reader SQL for a one-time import.
