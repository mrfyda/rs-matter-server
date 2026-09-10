# Hardware testing

Everything built after the responder is asserted by unit tests, by contract
tests, and by construction. No device has been on the other end of any of it.
This is the procedure for changing that: what to stand up, what to run, and
what counts as a pass.

[ROADMAP.md](ROADMAP.md) says *what* is worth proving and why. This says how.
Record results in [PARITY.md](../PARITY.md)'s hardware table — a run that is
not written down did not happen.

## The rig

Most of this needs no purchase. connectedhomeip's example apps run as ordinary
Linux processes on the same network and behave like real devices to a
controller; hardware is only needed where the thing under test is physical.

| Piece | What it is for |
|---|---|
| A Linux host on the same L2 network as the devices | The server, and where the example apps run. mDNS does not cross subnets. |
| [connectedhomeip](https://github.com/project-chip/connectedhomeip) built for Linux | `all-clusters-app`, `lock-app`, `thermostat-app`, `ota-requestor-app`, `camera-app` |
| The Shelly Plug S Gen3 | The only device this server has ever driven; the regression baseline |
| Home Assistant | The client the contract exists for |
| A second ecosystem controller — Apple Home or Google Home | The only honest way to test a change made by another ecosystem, and the multi-admin paths |
| An **open** border router: a Pi with an nRF52840 dongle running `ot-br-posix`, or HA's OTBR add-on | Thread diagnostics. Apple and Google border routers expose no REST API and hand out no credentials. |
| A battery Thread contact or motion sensor | The only thing that produces an ICD check-in |
| A Matter button or switch | The only thing that raises real `Switch` cluster events |
| An ESP32 or nRF52840 dev board | An OTA over a radio, with images you signed. A commercial device will not accept one. |

### Before the first run

The Matter port is the thing a migration hits first. This server binds and
advertises a real one — 5540 by default, `--matter-port` to move it — so if a
matterjs-server or python-matter-server still holds it on the same host, this
one refuses to start rather than advertise an address nothing answers on. Stop
the other server or move this one.

```bash
# Storage that is not the one being kept. A rig fabric is disposable; a fabric
# with the house's devices on it is not.
mkdir -p /tmp/rig && cargo run --manifest-path server/Cargo.toml -- \
    --storage-path /tmp/rig --log-level debug
```

`cargo run` is the right thing here: a debug build starts the same way a
release one does, and its logs say more. Both run the server on a 64 MiB
thread of its own, because the Matter stack's future does not fit in the
8 MiB one a process starts with.

### Driving it

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

Listening *before* the command is the shape nearly every check below wants:
events caused by a command arrive after it, and a client that was not
listening never sees them.

## What to prove

Each of these is a claim the code makes that no device has tested. Work down;
the early ones are prerequisites for the later ones.

### 1. A device's own reports replace polling

Rig: `all-clusters-app`, then the Shelly plug.

```bash
tools/ws.py --listen --for 120 read_attribute node_id=1 attribute_path=1/6/0
```

Press the plug's physical button. A pass is `attribute_updated` arriving
**immediately** — within a second — not within the 30 s poll interval. Check
the log for `Node N stopped reporting; will re-subscribe`: a subscription that
silently died and fell back to polling looks like a pass at 30 s resolution.

Also worth having: kill the device mid-subscription and confirm it is dropped
after twice its `max_interval` (10 minutes at the 300 s keepalive) and
re-established when it returns.

### 2. `node_event` carries real events

Rig: `all-clusters-app` — rebooting it raises `StartUp`. Hardware: a Matter
button or switch, which raises real `Switch` cluster events.

```bash
tools/ws.py --listen --for 300
```

A pass is `node_event` with the endpoint, cluster, event id, number, priority
and timestamp the device sent. Press the button several times in one burst:
the second and later events in a single report carry a **delta-encoded**
timestamp, and resolving those against the previous event is a code path
nothing else reaches.

### 3. A device can open a session *to* this node

Rig: `all-clusters-app`. This is the responder, and everything from here down
depends on it.

Watch for an accepted exchange in the log. If nothing arrives, check the port
note above before anything else: a device that resolved a port nobody listens
on fails exactly like a device that cannot reach the host.

### 4. An update is fetched and applied

Rig: `ota-requestor-app` with an image you signed yourself. Hardware: an ESP32
or nRF52840 dev board, to see it over a radio.

```bash
tools/ws.py initiate_ota_upload
curl --data-binary @firmware.ota http://127.0.0.1:5580/ota-upload/<id>
tools/ws.py check_node_update node_id=1
tools/ws.py --listen --for 600 update_node node_id=1 software_version=2
```

A pass is `attribute_updated` on the requestor's `UpdateState` walking through
querying → downloading → applying, and the device coming back on the new
version. Two things to watch that the unit tests cannot reach: an image large
enough for the BDX transfer to span many blocks, and the **restart** case —
the image store is in memory, so a server restarted between the upload and the
update has nothing to serve and refuses. That is current behaviour, not a bug
to be surprised by; note whether it bites in practice.

### 5. Nested payload fields resolve by name

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
`CredentialType` and `CredentialIndex` addressed by name rather than by
numeric TLV tag. Name matching ignores case and separators, so
`credentialType` and `credential_type` resolve too — try one, since clients
generated from different SDKs spell them differently.
Note that seven request fields still cannot: their structs are defined in the
shared `globals` module, which the registry generator does not index. Those
need numeric tags.

### 6. A BLE-commissioned node gets an address

Hardware: a Thread device, a border router, and the Linux BLE host. Nothing
else *must* use BLE.

Build with `--features bluetooth` on Linux, and check `server_info` reports
`bluetooth_enabled: true` before starting — the probe asks BlueZ for an
adapter, which is normally readable by anyone, while starting discovery and
connecting usually are not. A container running as uid 65532 needs the host's
`bluetooth` group.

If the host cannot reach the D-Bus system bus at all — Docker-in-Docker and
umbrelOS are the cases — there is no way round it. `ble_proxy_enabled` in
`server_info` is reported for contract parity and is always `false`; nothing
here proxies a radio from elsewhere. Run the server outside DinD for this
test.

```bash
tools/ws.py set_thread_dataset dataset=<hex>
tools/ws.py --listen --for 300 commission_with_code code=<11-digit>
```

A pass is the whole chain: mDNS finds nothing → BTP → PASE → credentials →
AddNOC → the device joins Thread → it is resolved over `_matter._tcp` → CASE →
`node_added` with an address. Each refusal along the way (`AuthFailure`,
`NetworkNotFound`, `UnsupportedSecurity`, `IPV6Failed`) should be reported as
itself, so a failure is as informative as a pass.

### 7. ICD check-ins fill in `awake`

Hardware: a genuinely sleepy device — a battery Thread contact or motion
sensor. Nothing else produces a check-in.

```bash
tools/ws.py register_icd node_id=1
tools/ws.py get_icd_state node_id=1     # awake and next_expected_checkin
```

Both fields are `null` until the device has checked in since this server
started, and check-ins are not persisted across a restart, so run this against
a server that has been up long enough. A pass is `awake` tracking the device's
actual idle and active windows against `ActiveModeDuration` and
`IdleModeDuration`.

### 8. WebRTC signalling round-trips

Rig: `camera-app`. Treat it as the target — Matter cameras barely exist.

```bash
tools/ws.py --listen --for 120 send_webrtc_provider_command node_id=1 \
    endpoint_id=1 command_name=SolicitOffer payload='{"streamUsage":1}'
```

**Do the registry fix first.** `IceServers` on both `ProvideOffer` and
`SolicitOffer`, and `IceCandidates` on `ProvideICECandidates`, are typed
`list:other` in the registry, because `ICEServerStruct` and
`ICECandidateStruct` live in the shared `globals` module that
`tools/build_registry.py` does not index. A bare `SolicitOffer` like the one
above works; anything carrying real ICE servers has to address those fields by
numeric TLV tag. Closing that gap is the smallest item on the roadmap, needs
no hardware, and turns this test from awkward into ordinary.

A pass is `webrtc_callback` carrying the session id and the `data` object for
its type. Two things are known to be untested rather than merely unverified:
an SDP is kilobytes and only reaches the device over the TCP transport, which
so far only a synthetic handshake has exercised; and the node id on a callback
is recovered by matching the accessor against known nodes, so a camera whose
node id is not in the store reports `null` there. Check which one you get.

### 9. Thread diagnostics collect

Hardware: an **open** border router.

Nothing collects yet — every network answers with an empty batch and
`no_credentials`. What this run is for is the input to building a collector:
point the border router's REST API at yourself and see what it actually
serves. The current OpenThread API is a JSON:API task collection — post a
`getNetworkDiagnosticTask`, poll it, read the diagnostics item it references —
not the single `GET /diagnostics` older documentation describes.

```bash
curl -s http://<border-router>:8081/api/diagnostics | head
tools/ws.py get_thread_border_routers
```

### 10. The migration is real

Two things nothing without hardware can settle: that a device commissioned by
matterjs-server accepts the imported identity, and that a device commissioned
*after* the import accepts a NOC signed by the imported CA.

```bash
cargo run --manifest-path server/Cargo.toml -- \
    --import-matterjs /path/to/matterjs/storage --import-matterjs-dry-run
```

Read the summary, then run it for real against a **copy** of that storage
directory. A pass is the old devices answering CASE without being
re-commissioned, and a new device commissioning onto the adopted fabric.

## What a real device will stress that a test cannot

These are not protocol gaps — every one of them is code that works. They are
places where the load a device generates is different in kind from the load a
test generates, and where the first honest measurement will come from the rig.
Watch for them throughout, not as a separate pass.

**A large interview against a client's own connection.** A first interview
publishes one `attribute_updated` per attribute, and the connection that asked
for it is not draining its event channel while its own command runs. The
channel holds 256. The Shelly's 179 attributes fit; a bridge, a multi-endpoint
`all-clusters-app`, or anything with more does not, and the server drops that
connection from the event stream and closes it so the client re-syncs. Watch
for `dropped from the event stream` in the log during an interview or a
commission, and count the attributes with `tools/ws.py --listen --count`.

**One connection, one command at a time.** Requests on a connection are
handled serially, so a `commission_with_code` — 5.4 s against the Shelly —
blocks every other command on that connection and all event delivery for its
duration. Home Assistant uses one connection. Watch what happens when a
command is issued while a commission is running.

**A write to `nodes.json` per report.** Every subscription report that changes
an attribute rewrites the whole node store and fsyncs it. Polling bounded that
to once per node per 30 s; subscriptions do not, so a device reporting often —
a power meter, an energy monitor — writes as often as it reports. The rig to
catch this on is the one most installs run: Home Assistant on a Raspberry Pi
with an SD card. Watch for `Could not persist polled attributes` in the log,
and watch the write rate with `iostat` while a chatty device is subscribed.

**Memory over days, not minutes.** Uploaded firmware images are held in memory
for the life of the process and are never evicted, at up to 64 MiB each. The
idle footprint is 11.4 MiB; leave the rig running for a week with real traffic
and see what it actually is.
