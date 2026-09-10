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
        |
   matter::responder   the other direction: the exchanges a device opens
        |
   a Matter device
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
- `server/src/matter/responder.rs` — the accept side: the exchanges a *device*
  opens, routed by protocol and opcode. It runs on the Matter thread beside the
  transport, because rs-matter's responder is a single future running several
  handlers concurrently and `Matter` is `!Send`.
- `server/src/storage/` — nodes, credentials, fabric label, node-id counter.
- `server/src/migrate/` — reading a matterjs-server storage directory into that
  state: `value` decodes matter.js's tagged JSON, `store` normalises its three
  storage drivers into one context/key map, `model` extracts the fabric,
  nodes and settings, and `mod` writes them. Read-only towards the source.
- `server/src/monitor.rs` — attribute polling; the single place that changes if
  rs-matter gains a client-side subscription receiver.
- `server/src/ws/` — listener, connection lifecycle, HTTP endpoints.

## Design decisions

**An import is a first start, not a mode.** `--import-matterjs` only supplies
the fabric that a first start would otherwise create; every later start takes
the ordinary path and the flag is ignored with a log line. That keeps the
migration out of the running server entirely — there is no importing state, no
half-migrated server, and a flag left in a compose file cannot reset anything.
The source is read before this server's storage is touched, so a source that
cannot be read leaves nothing behind to clean up before retrying.

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

## Bluetooth commissioning

Commissioning a factory-fresh wireless device happens over Bluetooth: the
device has no network yet, so the controller reaches it over BLE, hands it
Wi-Fi or Thread credentials, and only then talks to it over IP. Linux only —
rs-matter's BTP Central backends are `target_os = "linux"`, and macOS has no
CoreBluetooth backend — and behind the `bluetooth` cargo feature.

**The transport is chained, not switched.** `Btp` is created at startup and
joined to the Matter transport next to UDP with `ChainedNetwork`, keyed on
`Address::is_btp`. A device reached over Bluetooth is then just another
`Address` to everything above the transport, so the commissioning flow does not
branch on which one it is. Multicast stays on the UDP socket alone: `Btp` has
no `NetworkMulticast` impl, and mDNS has no business on a GATT link. This is
the one place a controller differs from a device — a device switches from BLE
to IP when commissioning ends, whereas a controller has to keep serving every
existing node over IP while commissioning a new one over BLE.

`Btp` is 4616 bytes and holds a single session, so it costs nothing worth
measuring and allows one Bluetooth commissioning at a time. rs-matter offers
`max-btp-sessions-{1,2,4,8}` if that ever needs to change.

**The pump and the flow are raced.** `bluez::run_central` holds the GATT
connection to one device and moves bytes between the adapter and `Btp`; the
commissioning flow's exchanges travel over `Btp`. Neither makes progress
without the other, so they run as two futures with the first to finish
deciding: the pump returning means the link dropped before commissioning was
done. (The backends live at `btp::bluez` and `btp::bluer` — `btp::gatt` itself
is private, and `btp` re-exports its public children.)

**Network provisioning sits between AddNOC and CASE**, where the Matter spec
puts it: the fail-safe is still armed and PASE is still the only way to talk.
`Exchange::initiate_pase` reuses the session the commissioner established,
keyed by peer address, so `AddOrUpdateWiFiNetwork` or
`AddOrUpdateThreadNetwork` followed by `ConnectNetwork` are further exchanges
on it rather than a second SPAKE2+ handshake. Which one is sent is decided by
reading the device's `NetworkCommissioning` feature map, because a controller
can hold both kinds of credentials and only the device knows which it can use.

**Phase 2 resolves over mDNS.** `Commissioner::complete_via_case_operational`
rather than `complete_via_case`: by then the device has left the GATT link for
its own network, and the address it will answer on is not known until it
announces itself.

**Finding the bus is the fiddly part in a container.** Two things bite. The
image sets `DBUS_SYSTEM_BUS_ADDRESS` because zbus otherwise falls back to the
spec's `/var/run/dbus/system_bus_socket`, and the distroless runtime has no
`/var/run` — it ships `run` and `var` but not Debian's symlink between them.
And under Docker-in-Docker, the bind source is resolved by the inner daemon
inside the DinD container rather than on the machine, so the real host's
`/run/dbus` is out of reach; Docker then creates an empty root-owned directory
at the destination, and connecting to *that* fails with `EACCES` rather than
`ENOENT`, because the kernel checks write permission before it checks the
target is a socket. Nothing in this project's compose fixes that — the bus has
to be passed into the DinD container itself, which on umbrelOS means editing
the Portainer app's own compose and redoing it after every update. A host
where the bus cannot be reached is the case BLE proxy mode exists for.

**What is not proven.** All of it compiles and none of it has commissioned a
real device. Testing needs a factory-reset device — which means removing one
from the fabric — and a host whose D-Bus policy lets the server's uid drive the
adapter, not merely read it. The container mounts the host's system bus and
runs as uid 65532, which is normally enough to see an adapter and not enough to
start discovery.
