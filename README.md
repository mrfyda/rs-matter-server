# rs-matter-server

A Matter controller server in Rust, speaking the same WebSocket API as
[matterjs-server](https://github.com/matter-js/matterjs-server) — the API Home
Assistant's Matter integration talks to. Drop it in where you would run the
Node or Python Matter server.

Built on [rs-matter](https://github.com/project-chip/rs-matter). The point is
the resource footprint: **11.4 MiB resident against the reference's 199.6 MiB**
under the same workload, which matters on a small always-on host running Home
Assistant alongside everything else.

**Status:** running under Home Assistant against real devices.
Commissioning, control, and the full command surface are implemented. Two
things are not — Bluetooth commissioning and OTA image delivery — and both
report a specific error rather than failing quietly. See [PARITY.md](PARITY.md)
for the command-by-command matrix, the measured hardware results, and every
known gap.

## Quick start

### Docker

```bash
docker compose up -d
docker compose logs -f
```

That pulls `ghcr.io/mrfyda/rs-matter-server:latest`, published by CI. To run
your own build instead, `docker build --platform linux/arm64 -t
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
the cost of build time. An arm64 host with that much memory builds it
natively without trouble, so cross-building is optional.

The image is built and published by CI on every push to `main`.

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
| `--log-level` | `LOG_LEVEL` | `info` | `error`, `warning`, `info`, `debug` |
| `--poll-interval-secs` | `POLL_INTERVAL_SECS` | `30` | How often a reachable node is re-read |
| `--default-fabric-label` | `DEFAULT_FABRIC_LABEL` | — | Pin the fabric label, ignoring clients |
| `--disable-ota` | `DISABLE_OTA` | off | Turn off update checks and the upload endpoint |
| `--disable-thread-diagnostics` | `DISABLE_THREAD_DIAGNOSTICS` | off | Turn off Thread discovery |
| `--enable-test-net-dcl` | `ENABLE_TEST_NET_DCL` | off | Also query the CSA test ledger |
| `--health-check` | — | — | Probe a running server and exit; used by the container health check |

## Endpoints

- `ws://host:5580/ws` — the protocol. On connect the server sends a
  `server_info` frame unprompted; after `start_listening` it streams events.
- `POST /ota-upload/<id>` — upload a local `.ota` firmware image, after
  reserving an id with `initiate_ota_upload`.
- `GET /health` — `{"status":"ok","schema_version":13,"nodes":N}`. Not part of
  the matterjs-server contract; added for container orchestration.

The protocol itself is documented by the reference server; this implementation
is verified against it command by command in [PARITY.md](PARITY.md).

## What survives a restart

Everything the protocol promises: the Matter fabric and its certificates, the
ICAC signing key, the node list with interview results, Wi-Fi and Thread
credentials, the fabric label, and the node-id counter. State is written
through a temporary file and renamed, so an interrupted write cannot truncate
what was already there.

`STORAGE_PATH` holds fabric signing material. Treat it like a private key —
back it up, and do not commit it.

## Development

```bash
cargo test --manifest-path server/Cargo.toml            # 212 tests
cargo test --manifest-path server/Cargo.toml -- --ignored  # + the 2 ignored
```

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
