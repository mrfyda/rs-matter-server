# rs-matter-server

A Matter controller server in Rust, speaking the same WebSocket API as
[matterjs-server](https://github.com/matter-js/matterjs-server) — the API Home
Assistant's Matter integration talks to. Drop it in where you would run the
Node or Python Matter server.

Built on [rs-matter](https://github.com/project-chip/rs-matter). The point is
the resource footprint: **11.4 MiB resident against the reference's 199.6 MiB**
under the same workload, which matters on a small always-on host running Home
Assistant alongside everything else.

**Status:** running under Home Assistant against real devices. Commissioning,
control, and the full command surface are implemented. OTA image delivery is
not, and reports a specific error rather than failing quietly. Bluetooth
commissioning is implemented but has only ever been compiled, never run
against a device. See [PARITY.md](PARITY.md) for the command-by-command
matrix, the measured hardware results, and every known gap.

## Quick start

### Docker

```bash
docker compose up -d
docker compose logs -f
```

That pulls `ghcr.io/mrfyda/rs-matter-server:latest`, the newest release, in
the host's architecture — `linux/amd64` and `linux/arm64` are both published
under every tag. See [Versions and releases](#versions-and-releases) to pin a
series or a digest instead. To run your own build, `docker build -t
rs-matter-server .` and point `image:` at that tag.

State lives in a named volume, so there is nothing to create or chown first —
Docker gives the volume the image's ownership. A bind mount would need
`chown -R 65532:65532` on the host directory, since the container runs
unprivileged.

The runtime image is `gcr.io/distroless/cc-debian12` — glibc and libgcc, no
package manager and no shell. That means no `docker exec ... sh`; use
`docker logs`, and `docker inspect` for the health status.

Then point Home Assistant's Matter integration at `ws://<host-address>:5580/ws`.

Host networking is required and the compose file sets it: Matter reaches
devices over IPv6 link-local addresses and finds them with mDNS multicast,
neither of which survives Docker's bridge NAT.

Building needs a builder with roughly 4 GB of RAM and a few GB of free disk —
rs-matter's generated cluster code is large enough that a single `rustc` wants
more than 2 GB on its own. `--build-arg CARGO_JOBS=1` reduces peak memory at
the cost of build time. A host with that much memory builds its own
architecture without trouble; building for the other one under QEMU is slow
enough that pulling the published image is usually the better trade.

Bluetooth commissioning raises that bar sharply: rs-matter with the `zbus`
feature needs well over 8 GB in a single `rustc`, which no `CARGO_JOBS` value
avoids. It is in the default build because the published image is built by CI
on a 16 GB runner. To build the image on a smaller machine, leave it out with
`--build-arg CARGO_FEATURES=`; the result behaves exactly as a host with no
adapter does.

The image is built and released by CI from every commit that lands on
`main` and passes its tests — see [Versions and releases](#versions-and-releases).

### Running it directly

```bash
cargo build --manifest-path server/Cargo.toml --release
LISTEN_ADDRESS=0.0.0.0:5580 STORAGE_PATH=/data \
  ./server/target/release/rs-matter-server
```

## Footprint

Both servers idle with zero commissioned nodes, then after 1000 read-only
commands (`server_info`, `get_nodes`, `diagnostics`, `get_all_credentials`,
`get_vendor_names`) issued over five listening connections. Measured on an
Apple Silicon Mac, not on the target hardware — the ratios are the point, not
the absolute figures.

| | matterjs-server | rs-matter-server | |
|---|---|---|---|
| RSS, idle | 199.6 MiB | **11.4 MiB** | 17.5x smaller |
| RSS, after the workload | 200.6 MiB | **13.8 MiB** | 14.5x smaller |
| Throughput | 1505 req/s | **3365 req/s** | 2.2x faster |

Read fairly: some of the reference's footprint buys features this server does
not have. At startup it seeds a DCL certificate store, fetches a vendor list,
and stands up a WebRTC camera-controller endpoint; this server ships a static
vendor table and has no WebRTC. The throughput figure is not a Matter
measurement either — those commands are served from cache, so it compares
protocol and serialization overhead rather than radio work.

## Configuration

Every option is a flag or an environment variable.

| Flag | Environment | Default | What it does |
|---|---|---|---|
| `--listen` | `LISTEN_ADDRESS` | `0.0.0.0:5580` | WebSocket and HTTP listen address |
| `--storage-path` | `STORAGE_PATH` | `/data` | Fabric, nodes, credentials, config |
| `--import-matterjs` | `IMPORT_MATTERJS` | — | Adopt a matterjs-server installation on first start |
| `--import-matterjs-namespace` | `IMPORT_MATTERJS_NAMESPACE` | — | Which storage namespace to import, for a multi-fabric source |
| `--import-matterjs-dry-run` | — | — | Report what would be imported, then exit |
| `--log-level` | `LOG_LEVEL` | `info` | `error`, `warning`, `info`, `debug` |
| `--poll-interval-secs` | `POLL_INTERVAL_SECS` | `30` | How often a reachable node is re-read |
| `--default-fabric-label` | `DEFAULT_FABRIC_LABEL` | — | Pin the fabric label, ignoring clients |
| `--disable-ota` | `DISABLE_OTA` | off | Turn off update checks and the upload endpoint |
| `--disable-thread-diagnostics` | `DISABLE_THREAD_DIAGNOSTICS` | off | Turn off Thread discovery |
| `--disable-bluetooth` | `DISABLE_BLUETOOTH` | off | Turn off Bluetooth commissioning where an adapter exists |
| `--enable-test-net-dcl` | `ENABLE_TEST_NET_DCL` | off | Also query the CSA test ledger |
| `--health-check` | — | — | Probe a running server and exit; used by the container health check |
| `--version` | — | — | Print the version and exit |

## Endpoints

- `ws://host:5580/ws` — the protocol. On connect the server sends a
  `server_info` frame unprompted; after `start_listening` it streams events.
- `POST /ota-upload/<id>` — upload a local `.ota` firmware image, after
  reserving an id with `initiate_ota_upload`.
- `GET /health` — `{"status":"ok","schema_version":13,"nodes":N}`. Not part of
  the matterjs-server contract; added for container orchestration.

The protocol itself is documented by the reference server; this implementation
is verified against it command by command in [PARITY.md](PARITY.md).

## Migrating from matterjs-server

Point the server at the storage directory the other server was using, and it
adopts that fabric instead of creating one:

```bash
LISTEN_ADDRESS=0.0.0.0:5580 STORAGE_PATH=/data \
  IMPORT_MATTERJS=/root/.matter_server \
  ./server/target/release/rs-matter-server
```

**Nothing has to be re-commissioned.** A Matter device recognises its fabric by
the root public key and grants administrative access to one controller node id,
so this server presents the identity the devices already trust: the same root
certificate, the same controller certificate and operational key, and the same
IPK. Node ids, the fabric label, the node-id counter, and the Wi-Fi and Thread
credentials come across too, so Home Assistant keeps its devices and entities.

What it does *not* copy is the cached attribute values. matter.js stores those
decoded into its own object model; the devices are the authority, so each node
is read afresh instead. Until a node answers that first read it is reported
unavailable and carries no attributes — normally a few seconds, and up to one
poll interval. Sleepy devices take longer, exactly as they do after any restart.

To see what it would take across without committing to anything:

```bash
./server/target/release/rs-matter-server \
  --import-matterjs /root/.matter_server --import-matterjs-dry-run
```

```
Namespace:              server
Fabric id:              0x1122334455667788
Controller node id:     0x000000000001b669 (112233)
Fabric label:           Living Room
Device NOCs signed by:  the root CA
Nodes:                  2
                        1 — 2 address(es), fabric index 3 on the device
                        2 — 0 address(es)
Wi-Fi credentials:      default (home-network), guest (guest-net)
```

Some details worth knowing before you run it:

- **The source directory is only read.** Stop matterjs-server first, then start
  this one. If you want to go back, point matterjs-server at that same
  directory: it is byte for byte as it was.
- **The import happens once.** After this server has a fabric of its own the
  flag is ignored with a log line, so it is safe to leave in the compose file
  or unit — there is no risk of a later restart resetting anything.
- **Every file-based storage driver is read** — matter.js's current `wal`
  format, the older one-file-per-key `file` format, and `json`. A directory written
  by the `sqlite` driver has to be converted first: start matterjs-server once
  with `MATTER_STORAGE_DRIVER=wal`, let it exit, then import.
- **A bad path costs nothing.** The source is read before this server's storage
  is touched, so a mistyped path fails with an error and leaves you able to
  retry rather than with a fabric of our own to delete.
- **Back up the source directory first anyway.** It holds the fabric's signing
  material, and it is the only copy of the identity your devices trust.

With Docker, mount the old data read-only and name it:

```yaml
services:
  rs-matter-server:
    volumes:
      - matter-data:/data
      - /path/to/matter_server:/import:ro
    environment:
      IMPORT_MATTERJS: /import
```

The container runs as uid 65532, so the mounted directory has to be readable by
it — `:ro` plus world-readable, or match the ownership.

If matterjs-server was itself migrated from the Python Matter Server, its
certificates run through an intermediate CA. That works, and the intermediate
key comes across so new devices can still be commissioned.

## What survives a restart

Everything the protocol promises: the Matter fabric and its certificates, the
key that signs certificates for newly commissioned devices, the node list with
interview results, Wi-Fi and Thread credentials, the fabric label, and the
node-id counter. State is written
through a temporary file and renamed, so an interrupted write cannot truncate
what was already there.

`STORAGE_PATH` holds fabric signing material. Treat it like a private key —
back it up, and do not commit it.

## Versions and releases

Every commit that lands on `main` and passes CI is released: the version is
derived, the image is published, and a GitHub Release is cut listing the
commits since the last one. Nothing is tagged by hand, and a commit whose
tests fail is never released.

Versions are semver, and the project is pre-1.0: a minor bump may break
compatibility, a patch is not meant to.

| Image tag        | Points at                                    |
| ---------------- | -------------------------------------------- |
| `0.1.4`          | that release, permanently                    |
| `0.1`            | the newest patch of that series              |
| `latest`         | the newest release                           |
| `main`           | the same, for anyone already pulling it      |
| `sha-1a2b3c4`    | the build from that commit                   |

Every tag covers `linux/amd64` and `linux/arm64`, so `docker pull` gets the
host's architecture without being told which. Each is compiled on a runner of
that architecture and the two are joined under one tag — emulated builds are
slow enough to be impractical for a crate this size.

No bare `0` tag is published. While the major is 0 a minor bump may break the
API, so a tag moving across minors would promise a compatibility that does not
exist. From 1.0 on, `1` is published and follows that major.

Every release is a patch bump. To release a minor or major instead, bump
`version` in `server/Cargo.toml` and commit it — the next release takes that
version, and the automatic patches resume from there. The lockfile records
that version too, so refresh it in the same commit:

```bash
cargo metadata --manifest-path server/Cargo.toml --format-version 1 > /dev/null
```

That manifest version is the floor of the series rather than the released
version: the patch comes from the tag history at build time, and CI passes the
result into the build, so `rs-matter-server --version` and the `sdk_version`
Home Assistant displays both report exactly what the image is tagged with. A
build that is not a release — anything built locally — reports the manifest
version.

The `schema_version` the protocol reports (13) is matterjs-server's and has
nothing to do with this: it says which client protocol is spoken, not which
release is running.

The mechanics are [tools/next_version.py](tools/next_version.py), whose rules
are covered by `--self-test` in CI, and
[.github/workflows/release.yml](.github/workflows/release.yml).

## Development

```bash
cargo test --manifest-path server/Cargo.toml               # 254 tests
cargo test --manifest-path server/Cargo.toml -- --ignored  # + the 2 ignored
tools/linux-check.sh                                       # Linux-only paths
tools/linux-check.sh --features bluetooth                  # needs >8 GB
```

`tools/linux-check.sh` runs `cargo check` in an arm64 Linux container, because
a macOS build compiles nothing behind `cfg(not(target_os = "macos"))` — the
built-in mDNS responder, interface selection, socket binding, and the whole
Bluetooth transport. It needs Docker, and for the `bluetooth` feature a VM
larger than Docker Desktop's default; CI is the fallback for that one.

The layout and the reasoning behind it are in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). Briefly: `protocol/` is the wire
contract with no Matter types in it, `api/` is one module per area of the
protocol, `matter/` owns everything rs-matter touches behind a single-threaded
actor, `storage/` is the persistent state, and `ws/` is the listener and
connection lifecycle.

`server/src/matter/clusters.json` is generated — regenerate it with
`tools/build_registry.py` after upgrading rs-matter.

## Licence

Apache-2.0, matching rs-matter and matterjs-server.
