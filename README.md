# lg-hue-sync (nstfn)

[![CI](https://github.com/nstfn/lg-hue-sync/actions/workflows/ci.yml/badge.svg)](https://github.com/nstfn/lg-hue-sync/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Ambient light for one TV: a rooted LG OLED G4 (webOS 11) drives a Philips Hue Play gradient
lightstrip (LCX002, 7 segments) behind it. There is no Hue Bridge: the lights go through
[Bifrost (nstfn)](https://github.com/nstfn/bifrost), a Hue bridge emulator on Zigbee2MQTT.

This is a fork of [adeze/lg-hue-sync](https://github.com/adeze/lg-hue-sync). It is tuned for this
TV and its owner's preferences, and it no longer follows upstream. Thanks to adeze for the daemon,
the capture code and the dashboard this work starts from.

## What it does

A Rust daemon on the TV captures the HDMI picture (320 x 180, 30 frames/s). It computes one
colour per strip segment from the part of the picture edge behind that segment, smooths it, and
streams it over Hue Entertainment (DTLS) to Bifrost. A dashboard at `http://<tv-ip>:8088/`
changes every setting live.

## How this fork diverges

Upstream base: `5ea246e`. This fork adds 42 commits of its own.

- **Bridge emulator.** An opt-in emulated-bridge mode pairs with Bifrost: the self-signed
  certificate is pinned at pairing, pairing accepts LAN addresses only, a real Philips
  certificate raises a warning, and the stream ends with a DTLS `close_notify`
  (`92ce81c`, `020294d`, `3222edf`).
- **Edge sampling.** Each gradient segment reads the picture edge behind it, like Ambilight:
  Gaussian weights along the edge, exponential decay inward, weights precomputed per frame size
  (`1c69f9f`). Edge width and depth are live settings (`4f065bd`).
- **Fast edge changes.** Upstream detects a scene cut from the picture centre only. Here, a
  large colour change on one edge also jumps at once for that segment, then the normal ramp
  lands it softly (`4f065bd`, setting `edge_cut_detection`).
- **Colour pipeline.** The hue survives the output trim, the noise gate, the step limit and the
  spatial blend; rise and fall smoothing survive Hue events (`03da27c`, `f7c9cad`).
- **TV power.** The lights follow TV standby and power-off: our stream stops first, then a
  second pass turns the lights off. The TV power state is polled once per round, off the frame
  loop (`0e215b9`, `0acc717`, `300c746`, `2d5892b`).
- **Capture recovery.** Capture restarts after an HDMI renegotiation instead of pausing
  (`f0c10c9`).
- **Hardened install on the G4.** Confined systemd unit, least-privilege Luna rights, optional
  watchdog, bounded HTTP API and config writes, no `DILE_VT` (`89cd056`, `68569ce`, `56526dc`,
  `199a17b`, `fac1cfa`, `579175c`). See [deploy/webos11/README.md](deploy/webos11/README.md).
- **CI.** Runs on every branch push and keeps the ARMv7 binary as an artifact (`960f1fd`,
  `0dc664d`).

Upstream commits after `5ea246e` that this fork does not take:

- `aae6ba8` (Hue streaming, dashboard controls, Luna subscriptions for the TV state). Its TV
  state source conflicts with our polling observer, which drives the lights-off at standby.
- `6af96e7` (opt-in NV12 screen streaming and its viewer) and `f6099d8` (mime update).

## Build

```bash
cargo fmt --all -- --check
cargo nextest run --locked
cargo clippy --bin lg-hue-sync -- -D warnings
bun install --cwd frontend --frozen-lockfile && make dashboard-check   # bun 1.4.4, as in CI
```

The ARMv7 binary for the TV comes from CI (artifact `webos-armv7-release`, with
`SHA256SUMS`), or locally from `make build` (webOS native SDK) or `make build-docker`.

Build `src/web/ui.html` on Linux, as CI does. A macOS build differs in Vite's module preload
helper and fails the CI step "Verify embedded dashboard matches sources".

## Deploy

Upload the binary to the TV as `/var/home/root/lg-hue-sync/lg-hue-sync.new`, then swap it:

```bash
ssh root@<tv-ip> "SHA=<sha256 of the binary> sh -s" < scripts/swap_binary.sh
```

The swap checks the SHA-256, keeps the running binary as `lg-hue-sync.previous`, and restores
it if the new one fails to start. `make deploy-bin TV_IP=<tv-ip>` does the same from a local
build and keeps the least-privilege Luna role.

The runtime configuration is `/var/home/root/lg-hue-sync/state/config.json` on the TV. It holds
pairing secrets: never commit it or copy it to another TV.

More: [docs/operations.md](docs/operations.md), [docs/architecture.md](docs/architecture.md),
[CHANGELOG.md](CHANGELOG.md).

## Limits

- The TV must be rooted. Rooting can void the warranty or break the TV.
- Capture is HDMI-only. It does not detect or bypass DRM.
- LG's capture APIs are private and may change with a firmware update.

## License

[MIT](LICENSE), as upstream.
