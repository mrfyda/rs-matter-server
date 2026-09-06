# Architecture

How the server is put together, and why. For what it implements against the
matterjs-server contract, see [PARITY.md](../PARITY.md).

```
WebSocket / HTTP client
        |
   ws::connection      per-connection lifecycle, event gating, OTA upload
        |
   protocol            envelopes, wire models, events, attribute paths
        |
   api::*              one module per area of the protocol
        |         \
   matter::actor   storage      the only path to Matter; persistent state
        |
   rs-matter           transport, mDNS, commissioner, TLV
```

## Modules

- `server/src/protocol/` — the wire contract: envelopes, models, events, paths.
  No Matter or transport types appear here, which is what lets the command
  handlers be tested without a radio.
- `server/src/api/` — command handlers, one module per area of the protocol,
  plus the registry and the per-connection call context.
- `server/src/matter/` — the controller actor, commissioning, the Interaction
  Model client, the TLV↔JSON codec, the cluster registry, the mDNS browser, the
  SPAKE2+ verifier, the update-ledger client.
- `server/src/storage/` — nodes, credentials, fabric label, node-id counter.
- `server/src/monitor.rs` — attribute polling; the single place that changes if
  rs-matter gains a client-side subscription receiver.
- `server/src/ws/` — listener, connection lifecycle, HTTP endpoints.

## Design decisions

**One actor owns Matter.** `Matter` is `!Send` — it holds a
`dyn DeviceAttestation` and its transport state — so it stays on one executor
thread. Connections reach it only through a channel, which also gives the
Interaction Model the serialization it wants without a lock.

**A small op set.** The actor exposes read, write, invoke, and the lifecycle
operations that need the commissioner. Fabric lists, ACLs, bindings, ICD
registration and decommissioning are cluster operations composed from those in
`api`, so the actor never grows a case per protocol command.

**Cluster metadata is generated, not hand-written.**
`server/src/matter/clusters.json` is lifted from rs-matter's own generated
cluster definitions by `tools/build_registry.py`, so command ids and payload
field tags cannot drift from the stack that puts the bytes on the wire.

**Wire naming is ported, not approximated.** `server/src/matter/wire_naming.rs`
is a port of the reference's acronym rules and field-name overrides, because
clients index command payloads by exactly those names.

**Storage is separate from the wire model.** A node's cached addresses and the
fabric index the device assigned this controller are persisted but never
serialized into a `MatterNodeData`.

**Runtime.** async-std, with async-io for timers and sockets. tokio appears
only in the test harness, for ergonomic async tests.

## A known upstream flake

Fabric creation retries. rs-matter's certificate generators fail with
`InvalidData` for a small fraction of randomly generated keys — measured at 16
failures in 3000 creations, split between the root and intermediate
certificates, with every input except the key material held constant. It
reproduces on x86_64 and has never been seen on aarch64, which points at a DER
encoding case that depends on the signature's random values.

Retrying is sound because nothing is persisted until the final step, so a
failed attempt leaves no fabric behind and the next one uses fresh key
material. Without it roughly one first boot in two hundred would fail. The
`fabric_creation_is_not_flaky` test loops `init_matter` and asserts that every
creation succeeds, which guards the retry. It is `#[ignore]`d because it needs
thousands of iterations to be meaningful, and the `flake-hunt` CI job runs it
on demand.

## Container

The runtime image is `gcr.io/distroless/cc-debian12` — glibc and libgcc, no
package manager and no shell — running as uid 65532.

No `ca-certificates` package is needed, even though the firmware update check
speaks HTTPS: the client is rustls with `webpki-roots`, so the Mozilla root
store is compiled into the binary and nothing reads certificate files at
runtime. That is what removes the main argument for a distro base.

`scratch` would be smaller still, but needs a musl target so the binary links
statically, and `ring` (via rustls) has C and assembly that must compile
against the musl toolchain. Distroless keeps glibc and avoids that risk while
still dropping the package manager and the shell. The health check is written
in exec form precisely so it works without a shell.

Image size does not affect resident memory, so none of this changes the
footprint figures; it is disk and attack surface.

## Extending: Bluetooth commissioning

Commissioning a factory-fresh wireless device normally happens over Bluetooth:
the device has no network yet, so the controller reaches it over BLE, hands it
Wi-Fi or Thread credentials, and only then talks to it over IP. This server
cannot do that. Two separate pieces are missing, and only one of them is this
project's.

**1. Wire up the BLE transport (Linux only).** rs-matter already implements
it, so this is integration rather than protocol work:

- `rs_matter::transport::network::btp` implements BTP over GATT and supports
  the Central role via `Btp::set_initiator(true)`.
- Two backends provide the OS half, both `target_os = "linux"`: `gatt::bluer`
  (the `bluer` crate, feature `bluer`) and `gatt::bluez` (direct D-Bus, feature
  `zbus`). Each exposes `scan` for discovery and
  `run_central(adapter, addr, &btp)` for connect-and-pump.
- The shape is: `scan` for a commissionable advertisement matching the pairing
  code's discriminator, then `select` `run_central` against `matter.run` and
  commission with `Address::Btp(addr)` instead of `Address::Udp(..)`.
- macOS has no backend. rs-matter has no CoreBluetooth implementation, and a
  plain CLI binary on macOS cannot use CoreBluetooth without an app bundle and
  entitlements — so this is testable on Linux hardware, not on a development
  Mac.
- The container would need the host's D-Bus socket and the Bluetooth adapter,
  so the compose file grows a mount and the image probably stops being able to
  run unprivileged.

**2. Provision the network.** rs-matter's commissioner runs ArmFailSafe →
CSRRequest → AddTrustedRootCertificate → AddNOC → CASE → CommissioningComplete
and never touches the NetworkCommissioning cluster. A device reached over BLE
has no network, so between AddNOC and CASE it has to be sent
`AddOrUpdateWiFiNetwork` (or `AddOrUpdateThreadNetwork`) followed by
`ConnectNetwork`, over the existing PASE session, before it can be reached over
IP to finish. The credentials are already stored and the cluster registry
already knows those commands with their payload field names; what is missing is
the invoke sequence and the plumbing to reuse the PASE exchange —
`Exchange::initiate_pase` reuses an existing PASE session by peer address,
which is the likely way in.

Until both land, `server_info.bluetooth_enabled` stays `false` and
`commission_with_code` finds only devices already on the IP network.

Testing it needs a device advertising over BLE, which means factory resetting
one: it leaves the fabric and has to be re-added.
