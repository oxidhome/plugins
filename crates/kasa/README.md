# TP-Link Kasa

Local control of TP-Link Kasa plugs, wall switches, dimmers, bulbs, power strips and
cameras. Devices are found with the Kasa UDP probe on port 9999 and adopted
automatically. Plugs, switches and bulbs speak the legacy XOR-obfuscated JSON protocol on
TCP 9999. Cameras are controlled over LINKIE2 (HTTPS 10443) and stream over HTTPS 19443.
Nothing goes through the TP-Link cloud. Cameras still need the Kasa account email and
password, because they use them to authenticate local requests.

This is a port of the [Scrypted Kasa plugin](https://github.com/sv-tools/scrypted-kasa-plugin).

> **Status: plugs, switches, dimmers, bulbs, strips and camera controls work; camera
> video waits on media.** The component targets the `streaming-plugin` world (`oxidhome`
> `d558f7d`):
>
> - **Works:** discovery by broadcast through the host's discovery socket (the
>   `255.255.255.255` probe and the subnet broadcast of each local network the grant
>   covers), with `discovery_targets` as a unicast fallback; TCP 9999 to smart-home
>   devices (polling, commands with H5 outcomes); and camera control and snapshots over
>   HTTPS (LINKIE2 on 10443, snapshots on 19443) through the SDK's HTTP client, with the host
>   trusting each camera's self-signed certificate on first use and pinning it. The host
>   checks every send, connect and request against the manifest's network rules.
>   Discovery replies wake the plugin early; every tick, early or on the 1 s fallback
>   cadence, also runs queued camera commands and due polls.
> - **Waits on Phase 9:** media. `setup-pipeline` is exported and opens the camera's live
>   stream over HTTPS into `host-media` pipes, but the host doesn't call the export and its
>   `host-media` answers `unavailable`.
>
> Everything below the host boundary is also tested natively against fakes: the protocol
> core, discovery, polling, commands, camera controls and stream re-packetization into
> RTP. [Platform gaps](#platform-gaps) lists what is still missing.

## Supported devices

| Family | Models (from the Scrypted plugin's testing and the protocol) |
|---|---|
| Plugs | HS100, HS103, HS105, HS107, HS110, KP100, KP115, KP125, EP25 |
| Wall switches | HS200, HS210, KS200 |
| Dimmers | HS220, KS230 (3-way) |
| Bulbs | LB1xx, KL1xx (dimmable, color, variable color temperature) |
| Power strips | HS300, KP303 (per-outlet on/off) |
| Cameras | KD110, KC200, KC401, KC410S (pan/tilt), KC420WS (spotlight), EC71 |

The plugin only supports devices that answer the legacy IOT protocol. Newer KLAP/SMART
firmware (Tapo, and some recent Kasa hardware revisions) is not supported.

Power strips were not modeled in the Scrypted plugin. Here each outlet uses the
documented `context.child_ids` addressing, with the child id exactly as the strip
reports it (as python-kasa does). This hasn't been tested on hardware.

## Device mapping

Every physical device becomes one main `OxidHome` device. Some also get child devices.
Kasa-specific features are namespaced extensions under `dev.oxidhome.kasa` (M2). Local
ids are stable across restarts, keyed by the Kasa `deviceId`.

| Kasa device | `OxidHome` device (local id) | Capabilities |
|---|---|---|
| Plug, wall switch | `<id>` | `switch` (+ `status-led@1`) |
| Plug with energy meter (`ENE`) | `<id>` | `switch`, `status-led@1`, `energy-meter@1` |
| | `<id>/power` | `sensor` (W): live power, for charts and automations |
| Dimmer | `<id>` | `switch`, `dimmer` (+ `status-led@1`) |
| Bulb | `<id>` | `switch`, `dimmer`, `color-light` (HSV and/or 2700–6500 K, per bulb) |
| Power strip | `<id>` | `status-led@1` |
| | `<id>/outlet-<childId>` | `switch`, named after the outlet's alias |
| Camera | `<id>` | `video-stream`, `audio-stream`, `camera-control@1`, `status-led@1`, `motion@1` (opt-in), `pan-tilt@1` (when the camera has it) |
| | `<id>/spotlight` | `switch` (when present) |
| | `<id>/siren` | `siren` (when present; the camera stops it after its configured duration). A sensitive capability: commanding it needs the owner token or a role grant naming the `alarm` category |
| | `<id>/privacy` | `switch`: on = camera off (video stops for every client) |

Every device carries the metadata `kasa_id`, `kasa_class`, `address` and `mac`.

### Standard capabilities

- **`switch`**: `set { state }`, `toggle`. State `state`.
- **`siren`**: `set { state }`, `toggle`. State `state`.
- **`dimmer`**: `set { level }` with `level` in (0, 1]. Kasa has no 0 % brightness, so
  use `switch` to turn the device off. State `level`.
- **`color-light`**: `set { hue, saturation, value? }` (all 0–1), or
  `set { color_temp_kelvin }`. Setting HSV drops the bulb out of white mode, as the Kasa
  app does. State `hue`, `saturation`, `value`, plus `color_temp_kelvin` on bulbs with
  a white channel (0 while in color mode).
- **`sensor`**: `value` in watts.

### Extensions (`extension(dev.oxidhome.kasa/<name>@1)`)

| Extension | State | Commands |
|---|---|---|
| `status-led` | `on` | `set { on }` |
| `energy-meter` | `voltage_v`, `current_a`, `power_w`, `total_wh`, `today_wh`, `month_wh`, `history` (JSON: `days_wh` for the last 30 days, `months_wh` for the last 12 months) | `refresh` |
| `motion` | `detected` (held 10 s after the camera last reports motion) | none |
| `pan-tilt` | `max_x`, `max_y`, `presets` (JSON list of `{index, name}`) | `move { direction: left\|right\|top\|bottom, speed? 1–10 \| magnitude? 0–1 }`, `goto-preset { index }` |
| `camera-control` | none | `reboot`, `snapshot` (returns `snapshot_blob`, `snapshot_blob_id`, `snapshot_bytes`) |

### Command outcomes (H5)

**Plugs, switches, dimmers, bulbs and strips** answer each command synchronously, in one
TCP exchange:

| What happened | Result |
|---|---|
| The device confirmed the change | `ok-with-state`, carrying the state that was set, plus a `state-changed` event so subscribers see it |
| The arguments were invalid | `err(invalid-argument)`; nothing is sent to the device |
| The connection couldn't be made | `err(unavailable)`; nothing reached the device |
| The device returned `err_code` ≠ 0 | `err(unavailable)`, or `err(invalid-argument)` for `-3` |
| The request was sent but no usable reply came back | `unknown`; the cached on/off is dropped, so a `toggle` waits for the next read |

**Camera commands** first answer `accepted`. On the next `tick` (within about a second
when idle; later while another call holds the instance) the plugin makes the call and reports the result with `complete-command`:

| What happened | Result |
|---|---|
| The call succeeded | `ok` or `ok-with-state` |
| The camera rejected the credentials (HTTP 401/403) | `err(permission-denied)` |
| The camera is off (`-40919`) | `err(unavailable)` |
| PTZ argument or end stop (`-41203`) | `err(invalid-argument)` |
| Unsupported (`-10008`) | `err(invalid-argument)` |
| The call timed out after the request was sent, or the camera answered HTTP 5xx | `unknown`; a later `toggle` waits for the next read |

A tick starts new network work only during its first 12 s, well inside the host's 30 s
watchdog. Queued commands and due polls that don't fit wait for the next tick, in order.

## Configuration

| Field | Type | Default | Meaning |
|---|---|---|---|
| `discovery` | bool | `true` | Run UDP discovery. |
| `discovery_broadcast` | bool | `true` | Send the probe to 255.255.255.255 and to the subnet broadcast of each of the host's networks that a granted IP or CIDR rule covers whole (`host-network::local-networks`; the send also needs a `udp` rule). |
| `discovery_targets` | string | empty | Fallback for devices broadcast doesn't reach: comma-separated addresses, or /24-or-narrower networks, to probe one by one, 3 ms apart. Some cameras (KC420WS) ignore broadcast but answer unicast, and devices on another VLAN never hear it. A single address works as a manual add. At most 1,024 hosts per run; past that, the rest are skipped and a warning is logged at start. |
| `discovery_interval_s` | int | 300 | Seconds between discovery runs; 0 runs it only at start; minimum 60. |
| `poll_interval_s` | int | 30 | State poll interval per device; minimum 5. |
| `camera_username` | string | empty | Kasa account email. |
| `camera_password` | secret | none | Kasa account password. Without it, cameras are registered, but their commands return `unavailable` and they are not polled. |
| `camera_motion` | bool | `false` | Poll each camera's motion state once a second. |

Adopted devices are remembered in plugin storage: an index (`devices`) and one small
record per device (`device/<id>`, names clipped to 64 characters). Only changed records
are written. They come back on restart before discovery runs, and follow DHCP address
changes when discovery sees them again. If storage can't be read at start, saving waits
until a read succeeds, so a partial discovery can't overwrite the stored list. Outlets a
strip stops reporting are removed from the host. When another device answers at a device's address (DHCP
reuse), nothing more is sent there until discovery finds the device again.

## Required capabilities

- **Network:** private ranges only (10/8, 172.16/12, 192.168/16) on the Kasa ports, plus
  the limited broadcast. The owner can narrow the ranges to the LAN's real subnet when
  granting.
  - UDP `255.255.255.255:9999`: the broadcast probe. A broadcast needs a rule naming that
    exact address and port; no range covers it.
  - UDP 9999 on the private ranges: the replies (devices answer from their own address),
    the subnet broadcast of each local network a rule covers whole, and
    `discovery_targets`. A rule narrower than the interface's network turns the subnet
    broadcast off for it.
  - TCP 9999: plugs, switches and bulbs.
  - HTTPS 10443: camera control.
  - HTTPS 19443: stream and snapshots.

  Two-way audio (HTTPS 18443) isn't requested: the talkback framing is implemented, but
  nothing can drive it yet (gap 11).
- **Devices:** the capability keys in the mapping table.
- **Storage:** 512 KB, for up to 256 devices at a few hundred bytes to ~2 KB each.
- **Blobs:** 16 MB, enough for one snapshot per camera, overwritten in place.

## Security model

- The Kasa account password is a `secret` config field. It is sent only to cameras on the
  LAN:
  - base64-encoded as the Basic-auth password on 19443;
  - as an md5 hex digest on 10443 (and on 18443 for talkback, which nothing drives
    yet).

  Both encodings are trivially reversible or brute-forceable, and that is the firmware's
  scheme, not this plugin's. The password never goes to the cloud.
- Camera certificates are self-signed. The host trusts each on first use and pins it per
  address and port; a different certificate there is refused (and audited) until the
  owner resets the pin with `oxidhome tls-pin reset`. Requests go to the camera's IP, so
  the certificate's name isn't checked; the pin is what ties it to the camera. Whoever
  answers first at that address when the pin is made is trusted, so a LAN attacker
  present at that moment, or at an address a camera moves to, can still capture those
  credentials. Use a dedicated Kasa account, or isolate cameras on their own VLAN.
- Discovery replies are unauthenticated UDP, and cameras are adopted automatically: any
  host that answers a probe as a camera is polled with the credential encodings. The
  plugin also follows a known device to a new address when discovery sees it there (DHCP
  changes), so a LAN attacker who answers with a camera's id moves the camera to its own
  address, where no pin exists yet. Turn `discovery` off once cameras are adopted to
  freeze the device list and their addresses.
- The IOT protocol has no authentication. Anyone on the LAN can switch these devices,
  and the plugin adds no protection.
- Every device reply is untrusted:
  - frames are capped at 256 KiB;
  - multipart headers at 32 KiB and parts at 8 MiB;
  - snapshots at 4 MiB and must start with a JPEG marker;
  - at most 256 adopted devices, 16 outlets per strip, 64-byte ids and 64 discovery
    replies per tick;
  - video parts with more than 256 NAL units are dropped;
  - malformed JSON is logged and ignored.

## Platform gaps

These are what the SDK, the current WIT (`oxidhome:plugin@0.1.0` at oxidhome `d558f7d`)
and the host lack for this plugin to run end to end. The plugin doesn't work around any
of them by changing the WIT. Numbers are kept stable so code comments can refer to them.

### Sockets and HTTPS (Phase 8)

0. **Closed (8.1, oxidhome/oxidhome#211).** The SDK has the `streaming-plugin` world:
   the component exports `setup-pipeline` through `streaming_plugin!`, and the world
   provides `wasi:sockets`, `wasi:http` and `host-media`.
1. **Closed (8.1, oxidhome/oxidhome#211).** The world creates sockets, gated on
   `[capabilities] network`.
2. **Closed (8.3, oxidhome/oxidhome#231).** Broadcast goes through the host's discovery
   socket (`oxidhome_sdk::streaming::discovery`), behind a `udp` rule naming
   `255.255.255.255:9999`. `discovery_targets` stays as a unicast fallback.
3. **Closed (8.3, oxidhome/oxidhome#231).** `host-network::local-networks` lists the
   host's networks the grant covers whole; the plugin sends each one's subnet broadcast.
   (It doesn't unicast-sweep them as the Scrypted plugin does; cameras that ignore
   broadcast go in `discovery_targets`.)
4. **Closed (oxidhome/oxidhome#223).** Camera HTTPS runs over `wasi:http`. The host
   trusts a camera's self-signed certificate on first use and pins it; for IP-literal
   URLs only the name check is skipped. Since oxidhome/oxidhome#242 the plugin uses the
   SDK's HTTP client, always through `send_streaming` so an error status is answered
   without reading its body. The SDK's total timeout bounds each exchange up to the
   response head, and the plugin bounds the rest of a LINKIE2 reply or snapshot by the
   same deadline (a trickling camera can't hold a `tick` out of the watchdog's reach);
   the live stream's body only has a 60 s idle timeout. `Error::may_have_reached_server`
   decides `err` versus `unknown` up to the head; a body that fails after it counts as
   sent.
5. **`credential-ref` sends the password as-is (Phase 9).** A host-run source on 19443
   would need the base64-encoded password, and talk on 18443 needs the md5 hex. The
   plugin computes both itself and sends them in its own requests, so the password does
   reach plugin memory.
6. **Closed (oxidhome/oxidhome#224, #242).** The host ticks the plugin early when a held
   socket (the discovery socket included) or HTTP response body is readable, so discovery
   replies and live-stream data don't wait for the 1 s fallback tick. Each tick, early or not, also runs queued camera commands and due
   polls. The 3 ms pacing of `discovery_targets` probes blocks inside a tick, for about
   190 ms per 64 probes.

### Media (Phase 9)

7. **Pipes aren't linked to pipelines.** `setup-pipeline` returns a `plugin-pipe` source
   with a `plugin-pipe-config`, and `host-media::open-pipe` returns a writer. Nothing
   ties the writer to that pipeline, so the plugin opens the pipes and the host has to
   guess.
8. **A pipeline has one source.** The camera muxes H.264 and G.711 µ-law on one
   connection, which allows one live stream per camera. The plugin splits them into two
   pipes, video and audio, but `media-pipeline` can carry only one, so audio has no way
   into the pipeline.
9. **No start/stop signal.** The camera serves a single stream connection, so it should
   be opened only while someone watches. `activation-policy::on-demand` exists, but the
   plugin is never told when consumers come and go. Today the stream opens on
   `setup-pipeline` and runs until it fails.
10. **`pipe-writer.write` has no timestamps.** The plugin works around it by writing
    pre-framed RTP from its own packetizer (FU-A, MTU 1400; PCMU 160-sample packets).
    It stamps arrival time, as the Scrypted plugin does. The stream is drained per tick,
    and stream data wakes the plugin early (gap 6, at most every 100 ms), so video frames
    that arrive within one such tick share a timestamp (audio advances by sample count).
11. **No outbound audio.** There's no way to receive talkback audio from a consumer (a
    WebRTC client). The 18443 framing is implemented and tested (`protocol::talk`), but
    nothing can feed it.

### Device model

12. **One sensor per device.** Live power gets a child `sensor` device. Voltage, current
    and energy totals live in the `energy-meter@1` extension.
13. **No device role or type.** The spotlight and privacy switch are plain `switch`
    devices; a UI can't tell them apart from an outlet. (The siren uses the `siren`
    capability.)
14. **No settings, readme or device-config UI.** The Scrypted plugin's adoption form,
    per-device settings and energy readme are not ported. Energy history is published as
    JSON in the extension state instead.

## Out of scope

- **HomeKit:** PrivacyMode binding, Link Status Indicator, accessory grouping. That's
  OxidHome's job, if it ever does it.
- **The Scrypted settings UI:** discover and adopt buttons, the per-device settings and
  Readme tabs, room pickers. Here devices are adopted automatically, with config fields
  instead.
- **The in-plugin RTSP server** (`src/camera/rtsp.ts`). Re-streaming is the host media
  pipeline's job; the plugin writes RTP into pipes.

## TODO

- Talkback glue once the WIT has an outbound audio path (gap 11).
- Snapshot fallback: return the last good JPEG for up to 10 minutes when the camera is
  slow, as the Scrypted plugin does.
- Cache SPS/PPS per camera for a fast stream start, once the pipeline can take it.
- Energy meters on individual strip outlets (HS300). Needs `context.child_ids` on the
  `emeter` calls.
- Integration tests against `oxidhome-test-host`, which loads `streaming-plugin`
  components.
- Review round 3 follow-ups:
  - bound the camera command queue by the host's pending-command limit (64, 5 min expiry)
    and check a job's age before sending it;
  - schedule motion reads fairly with ordinary polls, and settle expired motion holds
    outside the network budget;
  - escape `/` in device ids so a physical id can't collide with a generated child id;
  - promote a device to `Strip` when a later reply lists children;
  - keep a stored device whose re-registration the host refuses at restore in the index
    until it is explicitly retired;
  - track bulb brightness separately from power, so a confirmed `dimmer::set` before
    the first read is kept.

## Development

```sh
cargo test -p oxidhome-kasa-plugin                     # protocol + end-to-end over fakes
cargo clippy -p oxidhome-kasa-plugin --target wasm32-wasip2 --all-features --lib -- -D warnings
cargo build -p oxidhome-kasa-plugin --target wasm32-wasip2 --release
```

Layout:

| Path | Contents |
|---|---|
| `src/protocol/` | Pure protocol, no I/O, with known-answer tests: `cipher` (XOR autokey), `iot` (framing, requests, sysinfo, light, emeter, energy history), `discovery` (probe, reply walk, classification, sweep), `http` (auth encodings), `linkie` (LINKIE2 requests and responses), `camera` (LED, spotlight, siren, privacy, PTZ, motion, reboot, snapshot), `multipart` (stream parser, SPS/PPS), `rtp` (H.264 FU-A, PCMU, sender reports), `talk` (talkback framing). |
| `src/kasa.rs` | Plugin logic over `Host` + `Net`. |
| `src/mapping.rs` | Device and capability mapping. |
| `src/media.rs` | Stream session: multipart → RTP → pipes. |
| `src/wasm.rs` | Component exports through `oxidhome-sdk` (`streaming_plugin!`), and `Host`/`Net` over the SDK host calls, `host-media`, `std::net` (TCP), the discovery socket and the SDK's HTTP client. |
| `src/fake.rs`, `src/e2e.rs` | Simulated devices and end-to-end tests. |
| `tests/manifest.rs` | Validates `manifest.toml` with the host's `oxidhome-manifest`. |

## Credits

Protocol knowledge comes from:

- the Scrypted Kasa plugin and its `docs/PROTOCOL.md`;
- [python-kasa](https://github.com/python-kasa/python-kasa), for the IOT protocol and
  strip child addressing;
- the softScheck TP-Link Smart Home protocol write-up, for the XOR cipher.
