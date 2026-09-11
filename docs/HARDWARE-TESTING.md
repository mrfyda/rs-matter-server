# Hardware testing

What is still unproven against a device, and how to prove it. For what
hardware has already shown, see [PARITY.md](../PARITY.md).

## The rig

Most of this needs no purchase: connectedhomeip's example apps run as ordinary
Linux processes on the same network and behave like real devices. There are no
prebuilt arm64 images for them, so on a Pi that means building the tree from
source — hours, once.

| Piece | What it is for |
|---|---|
| A Linux host on the same L2 network as the devices | mDNS does not cross subnets |
| [connectedhomeip](https://github.com/project-chip/connectedhomeip) built for Linux | `all-clusters-app`, `lock-app`, `thermostat-app`, `ota-requestor-app`, `camera-app` |
| Home Assistant | The client the contract exists for |
| A second ecosystem controller — Apple Home or Google Home | A change made by another ecosystem, and the multi-admin paths |
| An **open** border router: a Pi with an nRF52840 dongle running `ot-br-posix`, or HA's OTBR add-on | Thread diagnostics. Apple and Google border routers expose no REST API |
| A battery Thread contact or motion sensor | The only thing that produces an ICD check-in |
| A Matter button or switch | The only thing that raises real `Switch` cluster events |
| An ESP32 or nRF52840 dev board | OTA over a radio, with images you signed |

**The Matter port is what a migration hits first.** This server binds and
advertises a real one (5540, `--matter-port` to move it). If another Matter
server still holds it, this one refuses to start rather than advertise an
address nothing answers on. That refusal is correct and looks like a failure.

## Driving it

[`tools/ws.py`](../tools/ws.py) speaks the protocol with no dependencies —
copy it next to the device and run it against stock Python. It prints the
greeting, the request, the reply, and every event that follows, each stamped
with milliseconds since the connection opened.

```bash
tools/ws.py server_info
tools/ws.py --listen                                  # stream until Ctrl-C
tools/ws.py --listen interview_node node_id=1         # listen, then act
tools/ws.py --listen --count interview_node node_id=1 # tally instead of print
```

Listen *before* the command: events caused by a command arrive after it, and a
client that was not listening never sees them.

`key=value` parses the value as JSON when it can, so a numeric-looking string
arrives as a number and the server rejects it. Pass those through
`--arg-json '{"code":"13870711089"}'`.

Raise the server's log level with `set_loglevel console_loglevel=debug`. The
argument is `console_loglevel`; a name it does not know is ignored and still
reports success. Most of what the monitor does is at `debug`, so at the
default level a node in trouble produces silence.

## What to prove

### `node_event` carries real events

Rig: `all-clusters-app` — rebooting it raises `StartUp`. Hardware: a Matter
button or switch.

```bash
tools/ws.py --listen --for 300
```

A pass is `node_event` with the endpoint, cluster, event id, number, priority
and timestamp the device sent. Press the button several times in one burst:
the second and later events in a single report carry a **delta-encoded**
timestamp, and resolving those against the previous event is a code path
nothing else reaches.

### An update is fetched and applied

Rig: `ota-requestor-app` with an image you signed. Hardware: an ESP32 or
nRF52840 dev board, to see it over a radio.

```bash
tools/ws.py initiate_ota_upload
curl --data-binary @firmware.ota http://127.0.0.1:5580/ota-upload/<id>
tools/ws.py check_node_update node_id=1
tools/ws.py --listen --for 600 update_node node_id=1 software_version=2
```

A pass is `attribute_updated` on the requestor's `UpdateState` walking through
querying → downloading → applying. Two things the unit tests cannot reach: an
image large enough for the BDX transfer to span many blocks, and the restart
case — the image store is in memory, so a server restarted between the upload
and the update has nothing to serve.

### Nested payload fields resolve by name

Rig: `lock-app` or `thermostat-app`.

```bash
tools/ws.py device_command node_id=1 endpoint_id=1 cluster_id=257 \
    command_name=SetCredential \
    payload='{"OperationType":0,
              "Credential":{"CredentialType":1,"CredentialIndex":1},
              "CredentialData":"MTIzNDU2",
              "UserIndex":1,"UserStatus":1,"UserType":0}'
```

`Credential` is the nested struct: a pass is the device acting on it, with its
members addressed by name rather than numeric TLV tag. Name matching ignores
case and separators, so `credentialType` resolves too.

### The interview does not overflow its own client

A first interview publishes one event per attribute into a 256-slot channel,
on a connection that is not draining it while its own command runs. A
two-endpoint plug produced 184. Point `all-clusters-app` at it — anything past
256 should drop the commissioning client from the event stream and close it.

```bash
tools/ws.py --listen --count commission_with_code code=<code>
```

The tally is the measurement. `dropped from the event stream` in the log is
the symptom.

### ICD check-ins fill in `awake`

Hardware: a battery Thread contact or motion sensor. Nothing else produces a
check-in.

```bash
tools/ws.py register_icd node_id=1
tools/ws.py get_icd_state node_id=1
```

Both fields are `null` until the device has checked in since this server
started, and check-ins are not persisted across a restart. A pass is `awake`
tracking the device's real idle and active windows.

### WebRTC signalling round-trips

Rig: `camera-app`. Matter cameras barely exist.

**Do the registry fix first** — see the roadmap. `IceServers` and
`IceCandidates` are typed `list:other`, so anything carrying real ICE servers
has to be addressed by numeric TLV tag until that lands.

```bash
tools/ws.py --listen --for 120 send_webrtc_provider_command node_id=1 \
    endpoint_id=1 command_name=SolicitOffer payload='{"streamUsage":1}'
```

A pass is `webrtc_callback` carrying the session id and the `data` object for
its type. Two things are untested rather than unverified: an SDP is kilobytes
and only reaches the device over the TCP transport, which so far only a
synthetic handshake has exercised; and the node id on a callback is recovered
by matching the accessor, so a camera not in the store reports `null` there.

### Thread diagnostics collect

Hardware: an **open** border router.

Nothing collects yet, so this run is reconnaissance for building the
collector: point the border router's REST API at yourself and see what it
actually serves.

```bash
curl -s http://<border-router>:8081/api/diagnostics | head
tools/ws.py get_thread_border_routers
```

### The migration is real

Two things nothing without hardware can settle: that a device commissioned by
matterjs-server accepts the imported identity, and that a device commissioned
*after* the import accepts a NOC signed by the imported CA.

```bash
cargo run --manifest-path server/Cargo.toml -- \
    --import-matterjs /path/to/matterjs/storage --import-matterjs-dry-run
```

Read the summary, then run it against a **copy**. A pass is the old devices
answering CASE without being re-commissioned, and a new device commissioning
onto the adopted fabric.

Note this needs a matter.js source. Home Assistant's own Matter add-on is
python-matter-server, whose storage this importer does not read.

### Home Assistant drives it end to end

The client the contract exists for. Point its Matter integration at
`ws://<host>:5580/ws`, commission a device, and check entity discovery and
control. Watch HA's log rather than only the server's: a contract mismatch
surfaces there as a parse error, not as anything this server would notice.
