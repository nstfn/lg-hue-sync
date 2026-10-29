# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Turn the Hue lights off when the TV goes to standby or the system shuts down. The bridge restores the lights' pre-stream state when our Entertainment stream stops, which usually left them on. The daemon now stops our stream first and waits until the stop completes, then sends `{"on":{"on":false}}` through CLIP v2 to each light the Entertainment configuration lists, over the verified HTTPS client, in at most 2 s. At a SIGTERM the order is: the 1 s system-state check, the stream stop (at most 4 s; a later stop skips the lights-off and logs it), then the lights-off, inside a 10 s stop timeout. After a standby lights-off only, a second pass 5 s later catches a late bridge restore: while the TV is still off and no application streams to the area, it re-reads the light states and turns off the lights still on, in at most 2 s, on a worker thread outside the main loop. It acts only when this application was the active streamer, never while another application streams and never with Hue sync off. A SIGTERM counts as a shutdown only when a 1 s bounded `systemctl is-system-running` reports `stopping`, or, without an answer, when the TV was last seen in standby; a service restart or upgrade with the TV on leaves the lights unchanged. The new `config.json` field `tv_off_lights` selects `"off"` (default) or `"restore"` (the previous behaviour).

### Fixed
- Start with one Hue bridge sync and one DTLS session. The startup sync now seeds the event worker's baseline and no longer leaves the layout marked for refresh, so the bridge-sync retry does not fire at once and the first event snapshot of an unchanged area reports no topology change. A layout that changed after the startup sync still triggers a sync.
- Keep the configured rise and fall smoothing after Hue events. Every Hue Entertainment event (stream start, reconnect, the reconnect that follows a settings change) reset both to a hard-coded 0.35 carried by `StreamState`, so custom values and the presets' temporal response only held until the next event. Remove the invented `smoothing_factor` and `brightness` fields, which the bridge never supplies, and the setters that only served them.
- Recover capture after HDMI renegotiation instead of pausing until a manual Start. Unavailable frames (libvtcapture code 13 "null buffer" while a source boots or a video starts) and undersized frame planes now restart capture with a backoff of 750 ms doubling to a 16 s cap, indefinitely; 30 s of healthy frames reset the backoff. A new capture session allows 10 s of unavailable frames before its first frame. Other invalid frames still pause, and the pause now clears on a TV wake or a switch to HDMI, never after a manual Stop. Restart attempts and the recovery are logged.
- Retry a failed Hue bridge sync instead of leaving Hue dark until the next bridge event. The retry waits 2 s, doubling to a 32 s cap, only while Hue output is wanted; a manual Start retries at once. Each attempt and the recovery are logged.
- Keep the Hue stream running through dashboard settings changes, presets and saves. A renewed event subscription now compares its snapshot with the last one of the same bridge, so only another area or changed channels or locations trigger a bridge sync. A topology change that arrives while the subscription renews is kept for the next one. A sync that finds the saved area unchanged no longer rewrites `config.json`.
- Report why a configured Hue bridge IP was unreachable. The TCP connect error stays the root cause when rediscovery also fails, cached cloud discovery failures keep their full cause chain, and bridge sync failures log the whole chain. Paired-bridge rediscovery no longer suggests entering the IP manually.
- Keep the hue through the Hue output trim. The trim scaled and clipped each channel on its own, so orange (255, 128, 0) at 1.3 became (255, 166, 0) and every highlight above 196 merged into white. One factor now scales all three channels, limited so the brightest channel stays at or below 255. The Nanoleaf trim and the dashboard preview use the same trim; the preview now includes the Hue trim.
- Keep saturated dark blues. The noise gate tested luma, so with a 0.02 gate a blue up to 70/255 became black. The gate and its ramp now test the largest channel.
- Keep the hue while the step limit applies. The limit capped red, green and blue separately, so black to orange showed yellow for about 0.25 s. It now scales the whole change, so the largest channel moves by the step limit and the others keep their ratio.
- Fade the spatial blend in with each zone's own level, from zero at black to full strength at twice the noise gate (at least 0.02). A zone leaving black jumped at once to half its neighbours' colour, which bypassed smoothing and the step limit.
- Weight the spatial blend by distance. The zone itself now counts with weight 1, so one distant neighbour gets a small share instead of the full blend strength.
- Stop dark frames from cropping the sampled area. A fully dark frame measured symmetric bars and cut the area to the middle third. A frame with a dark centre now leaves the area unchanged, and a new letterbox is adopted only after 3 consecutive matching detections (about 1.5 s at 30 fps).
- Let a scene cut move 4 times the normal step, so a full black-to-white cut takes about 6 frames instead of 22. It stays under the flash-safety limit.
- Keep fractional colours through the Hue output trim and convert them straight to 16 bits. Rounding to 8 bits first made dark fades step unevenly.
- Weight sampled pixels by chroma (max - min) instead of (max - min) / max, so near-black noise such as (3, 0, 1) weighs like grey instead of like pure red.
- Keep light updates flowing while the TV is queried. The TV power, foreground source and source frame-rate queries each took up to 1.5 s inside the frame loop, so every 2 s light updates could stall for up to 4.5 s. One background task now runs them every 2 s and publishes each round; the running and paused loops only read the newest round, without waiting. Power-change handling, the standby lights-off trigger, the source policy and the retry of a faulted capture after a TV wake or an HDMI switch are unchanged.
- Log the real cause of a pause (TV standby, capture fault, unsupported source, manual stop) instead of `web=true` for every stop.
- Show the server's message in the dashboard for a 409, 413, 421, 429, 503 or 504 answer to Save, a style recall or a style reset, instead of only the status code.
- Subscribe to Hue events again when the bridge address, bridge ID, certificate pin, application key or Entertainment area changes. The worker kept the configuration it got at the first request.
- Stop a Hue Entertainment session at the bridge address that the reconnect verified, not at the saved one.
- Keep the daemon running when the Nanoleaf controller is unreachable at startup. It logs the error and runs Hue alone; the next resume or Nanoleaf sync toggle retries Nanoleaf.
- Make a `config.json` save durable: the config directory is synced after the rename. At startup, remove `config.new-*` temporary files left by a killed writer.
- Log an idle Hue event stream read timeout (EAGAIN, "os error 11", or TimedOut) at info level as a reconnect, not as "Hue events unavailable". The reconnect is unchanged.

### Security
- Remove the legacy DILE_VT capture backend. It mapped physical addresses returned by the driver through `/dev/mem`, read-write, with no range check, and its structure layout dated from webOS 6. libvtcapture is now the only native backend: a failed or busy init keeps no capture and retries on the existing capture backoff.
- Space native capture sessions at least 2 s apart, whatever sequence of Start, Stop, Toggle, Restart or recovery requests them. A refused session start retries on the capture backoff.
- Bound the HTTP API: JSON bodies are limited to 16 KiB (128 KiB for `POST /api/preview/render`, sized from its largest valid frame), at most 16 requests run at once (503 beyond), and each request times out after 10 s (504), 60 s for pairing and Hue area routes. Pairing and Hue area operations run one at a time; another one gets 409.
- Refuse DNS rebinding and cross-site requests. A Host other than an IP literal, `localhost` or `lg-hue-sync.local` (each with an optional port) gets 421. A POST, PUT, PATCH or DELETE whose Origin header does not match the Host gets 403; requests without Origin, such as Home Assistant, curl and Shortcuts, are unchanged.
- Limit flash writes of `config.json`. A save whose serialised bytes equal the file on disk is skipped. `POST /api/save-config` and `POST /api/bonjour` save at most once every 10 s; a request inside that window gets 429 and live settings stay applied.
- Redact the Hue application key, the Hue client key and the Nanoleaf auth token from `Debug` output of the configuration.
- Accept only a private (RFC 1918) or link-local IPv4 address for Nanoleaf pairing from the dashboard. Accept a Nanoleaf auth token only when it holds ASCII letters and digits, and build Nanoleaf addresses through `SocketAddr`, so an IPv6 address is always bracketed.
- Request no webOS permissions for the launcher app (`requiredPermissions: []`). It only opens the local dashboard.

## [1.0.0] - 2026-10-29

### Added
- Add demand-driven HDMI frame capture and optional live thumbnails to Advanced tuning, using the existing capture pipeline with bounded 128×72 previews at up to one frame per second.
- Preview static colour adjustments using the daemon's shared colour processing, with optional Hue device regions and Nanoleaf perimeter overlays. Explain unsupported spatial, temporal and physical-light effects in a preview information popup.
- Save custom tuning per ambient style, retain the selected style, and restore factory defaults with Reset. Preserve legacy settings and keep pairing, device placement and output trims separate from preset overrides.
- Add cached, read-only input/output diagnostics for HDMI, display output and internal-player information where the TV exposes it. Distinguish source properties from display output; unavailable fields remain unavailable.

### Changed
- Organize the dashboard into Tuning, Devices and Diagnostics tabs, with compact screen-sync and follow-TV-power controls.
- Combine the adjusted colour preview and light layout, place ambient styles above it, and use horizontally scrolling vertical response faders with larger focus targets.
- Place Save/Reset in Advanced tuning, remove preview pinning, and show contextual fader explanations without repetitive live-preview status announcements.

### Validation
- All 118 Rust tests, dashboard preview/preset checks, formatting, warnings-denied Clippy and embedded JavaScript syntax checks passed for 1.0.0. Launcher packaging and native-build safety tests passed. Native ARMv7 builds passed ELF/ABI, loader and glibc <= 2.28 checks.
- The feature build passed on-TV binary hash, service/dashboard health and HDMI frame capture checks, with paired configuration preserved and a verified rollback binary. Built-in Netflix and HDMI observations distinguished internal-player properties from display output.
- Published assets are built independently by Docker CI. The release binary is not the previously deployed feature binary; physical colour accuracy, touchscreen interaction and CPU impact were not measured. MAT/Atmos decoding and audio capture are not provided. Launcher-only IPKs and manifests remain unpublished.

## [0.9.1] - 2026-10-28

### Fixed
- Show one Hue preview box per physical device, combining the sampling bounds of its streaming channels. Keep different devices and legacy channels separate; use a representative average sampled colour for multichannel previews only.
- Replace segment-range labels with the device name and retain channel details in the tooltip. Add large orientation glyphs when Hue supplies enough service layout points; the monitor end marks front toward the TV wall, not LED facing.
- Preserve optional Hue layout points for preview orientation without changing sampling regions, channel IDs, segment membership, or output processing. Missing or invalid geometry does not invent orientation.

### Validation
- All 105 Rust tests, preview grouping/orientation checks, formatting, warnings-denied Clippy, dashboard syntax and release metadata checks passed. Native ARMv7 builds passed ELF/ABI, loader and glibc <= 2.28 checks.
- Local browser checks confirmed eight device boxes, one Flux strip label and accessible orientation glyphs using captured layout data. TV deployment is verified separately from the independently built Docker CI release artifact.
- Physical LED facing, colour accuracy and alternative sampling strategies were not tested or changed. Launcher-only IPKs and manifests remain unpublished.

## [0.9.0] - 2026-10-28

### Added
- Generate and serve `/openapi.json` with `utoipa` and `utoipa-axum`, covering all 16 API routes, request/response types, validation bounds and error responses. Cache the description at server startup.
- Add opt-in Bonjour HTTP advertising with dashboard and OpenAPI links. Discovery defaults to off for new and existing configurations; disabling it shuts down its mDNS thread and sockets.
- Add a dashboard Bonjour toggle and `POST /api/bonjour` to persist and apply discovery changes without restarting capture or lighting. Report the saved preference through `/api/status`; failed saves leave discovery unchanged.

### Changed
- Group Bonjour discovery and Restart Pipeline in a collapsed Maintenance panel at the bottom of the dashboard. Move Save to Disk to an accessible header glyph beside the theme control.
- Register API routes and OpenAPI metadata together while preserving existing URLs and request formats. Document plain-text JSON extraction errors alongside application JSON errors.

### Validation
- All 104 Rust tests, formatting, warnings-denied Clippy, dashboard syntax and release metadata checks passed. Native ARMv7 builds passed ELF32, little-endian ARM EABI5 soft-float, loader and glibc <= 2.28 checks.
- Before the version bump, feature builds passed on-TV binary hash, service, dashboard and OpenAPI checks. Bonjour enable/disable persisted correctly, started/stopped its mDNS thread without changing the daemon PID, and restored the original configuration byte-for-byte; discovery was left off.
- Published daemon assets are built independently by Docker CI. Third-party client discovery, multicast visibility, resource impact and physical lighting output were not validated for this release. Launcher-only IPKs and manifests remain unpublished.

## [0.8.0] - 2026-10-22

### Added
- Track Hue ownership and partial configuration changes through a bounded SSE subscription; refresh the selected layout before first output and after topology changes.
- Discover bridges through local mDNS with a rate-limited cloud fallback; recover changed addresses only for the saved Bridge ID.
- Repeat cached Hue packets at 50 Hz independently of capture cadence. Adaptive deadband still suppresses new colour-update submissions; no additional sampling is performed for repeats.

### Changed
- Pair exclusively over verified HTTPS using both official Hue root CAs and the Bridge ID. Migrate existing leaf pins only after an exact match, then accept signed certificate renewal for that identity.
- Use the separate application ID from `/auth/v1` for DTLS authentication and stream ownership. Preserve pairing credentials when area selection is still required.
- Delegate the deprecated Python pairing helper to the host Rust CLI; remove plaintext pairing and direct Hyperion mutation.
- Update architecture, operations, agent and skill guidance with recovery rules and current Hue documentation findings.

### Fixed
- Refuse automatic takeover and avoid deliberately stopping another application's Hue stream. Suspend Hue independently of Nanoleaf on ownership loss.
- Stop cached Hue output on revoked authorization, deleted areas or failed topology refresh; keep stale mappings blocked across manual Start, Restart and output re-enable.
- Use configured Hue channel IDs in the test pattern instead of inventing fixed channels.

### Validation
- All 97 Rust tests, formatting, warnings-denied Clippy, dashboard syntax and release metadata checks passed. Launcher packaging and native-build safety tests passed; launcher packages remain unpublished.
- The native v0.8.0 artifact passed on-TV SHA-256, service, verified HTTPS pairing, legacy-pin identity migration, DTLS and SSE checks with zero automatic service restarts. HDMI capture returned to 60 FPS; Home released capture and paused outputs; returning to HDMI resumed them. Manual Stop survived input changes.
- Disabling Hue left Nanoleaf and capture running; disabling Nanoleaf left Hue running. Both outputs were restored. Pairing replaced only Hue credentials/identity and preserved Nanoleaf configuration.
- Competing-owner runtime testing was intentionally omitted at the maintainer's request; ownership guards have host-test coverage only. Colour accuracy, CPU impact and packet-loss performance were not measured.
- The maintainer confirmed both Hue and Nanoleaf visibly syncing after restoration; this was a functional visual check, not a colour-accuracy or latency measurement.
- The cached Docker fallback also compiled. Published daemon assets are built independently by Docker CI and are not the TV-tested native artifact.

## [0.7.0] - 2026-10-22

### Added
- Automatically pause capture on known built-in apps/Home and retry when HDMI returns, while preserving manual Stop. Show an explicit unsupported-input state instead of repeated driver initialization attempts.
- Add macOS-native ARMv7 cross-builds using the webOS Brew compiler and a Buster/glibc 2.28 sysroot bootstrapped once from the cached builder. Validate ELF class, endianness, ARM EABI5 soft-float ABI, loader and glibc requirements before atomic artifact replacement.
- Add Rust Ares launcher packaging and explicit-device install, launch and app-list commands, preserving the Homebrew manifest and selecting the exact current package instead of a stale filename.
- Expose requested sync state, actual lifecycle state, readiness and capture failures separately; release capture resources before reporting stopped and show lifecycle transitions in the dashboard.
- Add packaging and native-build safety tests, Codex build/package actions, and an isolated cargo-zigbuild experiment with verified ARM/glibc output.

### Changed
- Make `make build` use the native macOS toolchain after successful TV service/capture validation. Keep `make build-docker` as the explicit fallback and Linux CI path; separate native and Docker toolchain update commands.
- Remove unused hard-float Cross/Cargo settings; preserve `armv7-unknown-linux-gnueabi` and the glibc 2.28 ceiling.
- Improve dashboard spacing, contrast and status presentation while keeping Glasshouse and LG Hue Sync separately owned.
- Package the launcher before full deployment mutates the TV. Keep paired configuration, checksum verification and a recoverable previous daemon binary.

### Fixed
- Keep Luna query stdin open until the asynchronous response arrives; bound reply size and process lifetime, share transport across source/power/video queries, and limit source frame-rate polling to once every two seconds.
- Preserve Faulted/error state when an explicit Stop cancels a capture Restart, including commands received during the restart delay.
- Run periodic Hue Bridge status polling in a separate worker, reject superseded results, and wait for stop cleanup before checking for app-driven resume.
- Move Hue reconnect and session cleanup to a serial background worker; discard superseded results and keep Nanoleaf and control processing running while Hue reconnects.
- Reject malformed Hue client keys without UTF-8 slicing panics, and share a five-second DTLS handshake deadline across retries and socket operations.
- Bound Nanoleaf pairing requests, response bodies, polling delays and panel-layout retrieval by the remaining pairing deadline.

### Validation
- Native v0.7.0 passed a live HDMI → app/Home → HDMI transition: hardware capture released and both rates reached zero on the unsupported source, then 60 FPS hardware capture resumed automatically and the error cleared. Binary SHA-256 matched and paired configuration stayed unchanged; all 89 Rust tests and packaging/build safety tests passed.
- A preliminary native v0.6.1 build passed TV binary checksum, unchanged configuration, dashboard and service checks, with zero restarts, 320x180/60 FPS hardware capture, and Hue/Nanoleaf connected. Subsequent built-in app tests motivated the explicit input policy. Physical lighting quality was not measured.
- The Zig candidate passed local ELF/ABI/glibc inspection only; it was not deployed. Published daemon assets are built by the Docker CI fallback and are not the TV-tested native artifact. Launcher-only IPKs and Homebrew manifests remain unpublished.

## [0.6.1] - 2026-10-08

### Added
- Add optional geometry-based spatial gradient blending for Hue and Nanoleaf, controlled by the dashboard and persisted as `spatial_blend`; default off and preserve black regions.

### Changed
- Give ambient styles explicit spatial blending: Neutral and Fast Response 0%, High Chroma 15%, Neon Contrast 8%, Dark-Scene Detail 10%, Low Stimulation 30%. Selecting a style now resets spatial blending; manual tuning remains available.
- Replace the dashboard theme dropdown with an accessible glyph button cycling system, dark and light; retain the status subtitle and remove its duplicate update-rate counter.
- Collapse Devices and Advanced tuning by default on the dashboard, matching Calibration while keeping status, sync controls and presets visible.
- Replace handwritten RGB/HSV conversions, encoded-RGB mixing and sRGB decoding with Palette 0.7.7; retain device-specific gamut handling and response controls.
- Replace the NV12 pixel loop with yuv 0.8.19 using explicit BT.601 limited-range conversion and existing buffer checks. Sample differences stay within one RGB count; normalize below-black luma to preserve legacy behaviour across scalar and SIMD paths.
- Retain fractional spatial averages, colour transforms, temporal state, and colour-step limits until output rounding, preventing small fades from stalling at 8-bit boundaries.

- Use cargo-nextest for local Rust tests and CI, with CI pinned to nextest 0.9.146.

## [0.6.0] - 2026-10-01

### Added
- Add a boot-persistent systemd timer that checks `daemon.log` every minute and clears it in place at 1 MiB without restarting sync or retaining archives.
- Add an opt-in raw NV12 centre-patch probe with read-only TV mode hints, plus selectable SDR browser patches at `/capture-patterns`; neither path writes frames or drives lights.
- Allow trusted-LAN automation, including Apple Shortcuts, to apply named dashboard presets through `POST /api/presets/{name}` without changing output trims or other controls.
- Document webOS Brew userland headers/API reference and the Buildroot source behind the native SDK.
- Record a capture colourimetry, temporal response, precision, and latency validation plan based on HyperHDR's published pipeline.

### Changed
- Leave the unverified 8-bit midtone lift off in newly generated configurations and label it accurately in the dashboard; preserve existing configuration values and the legacy JSON key.

### Fixed
- Classify capture errors with native return codes and preserved source chains; skip transient frame outages, cap automatic driver recovery at three increasing delays, and pause invalid frames until manual retry.
- Cancel capture recovery on shutdown, release queued dashboard requests before server shutdown, and terminate timed-out Luna child processes.
- Base Hue and Nanoleaf temporal smoothing and colour-step limits on elapsed time, preserving 30 FPS tuning across variable capture rates and bounding long-stall jumps.
- Validate webOS capture frame dimensions and plane lengths before conversion, keep NV12 conversion safe for callers, release partially mapped display buffers when setup fails, and remove an unverified cross-thread handle guarantee.
- Process dashboard commands while capture is paused, honor per-device output toggles on resume, and report Nanoleaf status after a successful local send.
- Reject invalid capture sizes and dashboard settings, serialize configuration read-modify-write operations across daemon, dashboard, and setup commands, and report save failures to the dashboard.

## [0.5.0] - 2026-09-29

### Changed
- Moved pairing and bridge setup commands out of the daemon entrypoint; setup now preserves an existing unreadable configuration instead of overwriting it.
- Keep Hue/Nanoleaf LAN HTTP traffic direct, reject redirects, and redact Nanoleaf token URLs from request errors; send watchdog heartbeats only when configured and at half the reported interval.
- Updated `ureq` to 3, `sd-notify` to 0.5, and `libloading` to 0.9, migrating their HTTP, notification, and library-loading call sites.
- Added a tracked Codex project environment with worktree setup and actions for dependencies, validation, webOS builds, TV transfer, and scoped Docker cache cleanup.
- Switched the Docker cross-toolchain to Rust stable and added an explicit refresh action that rebuilds its image and validates the ARM binary.

## [0.4.2] - 2026-09-24

### Changed
- Replaced per-build Debian/Rust provisioning with one cached, pinned cross-toolchain image shared by local, Codex, deployment, and CI builds.
- Added explicit compatible dependency check/update targets and ARMv7 target-build CI coverage.
- Added pull-request validation, release metadata checks, and tag-driven publication of verified ARMv7 daemon artifacts.
- Documented the external-contributor review and validation path while retaining direct-to-`main` maintainer work.

## [0.4.1] - 2026-09-23

### Changed
- Replaced independent dashboard request flags with an ordered Tokio command channel and explicit pipeline state.
- Unified Hue and Nanoleaf colour transforms in one processor while preserving device-specific geometry and protocols.
- Added bounded exponential backoff for Hue and Nanoleaf reconnection attempts.

## [0.4.0] - 2026-09-23

### Added
- Optional TV power following: pause Hue and Nanoleaf sync in standby and resume when the TV becomes active; can be disabled for manual control.
- Responsive dashboard with device pairing, Entertainment Area selection, independent outputs, themes, presets, calibration, and semantic WebMCP controls.
- Nanoleaf 4D corner, direction, and perimeter-offset alignment.
- Hue API v2 discovery that preserves grouped gradient-member identity.

### Changed
- Single-maintainer changes go directly to `main`; pull requests are opt-in.
- Deployment requires an explicit TV address and preserves existing credentials by default.
- Public runbooks now distinguish host checks, target builds, transfer, verification, rollback, uninstall, and release evidence.

### Fixed
- Hue recovery reactivates its v2 Entertainment configuration before reconnecting DTLS.
- Nanoleaf alignment remaps panel IDs without moving screen sampling coordinates.
- Device status and refresh controls are grouped with their corresponding setup controls.

## [0.3.0] - 2026-09-18

### Added
- **Nanoleaf 4D (V1) Lightstrip Integration**: High-speed binary UDP streaming on port 60222 using the `extControl` v2 protocol ported from Hyperion's battle-tested networking implementation.
- **Perimeter Edge Sampler**: Automatically generates 30+ normalized sampling zones along the TV perimeter (Left, Top, Right, Bottom) with 16:9 aspect ratio distribution, saturation-weighted dominant color extraction, and OLED near-black noise gating.
- **Unified Lockstep Dual-System Sync**: Drives both the TV perimeter Nanoleaf 4D strip and Philips Hue room surround lighting simultaneously from a single captured video frame with shared scene-cut detection.
- **`sync-hue` Dynamic Re-Sync Command**: Re-queries the Hue Bridge for updated 3D light coordinates and entertainment area selection without requiring physical button press re-pairing.
- **`pair-nanoleaf` Setup Command**: Pairs with a Nanoleaf controller on LAN via HTTP POST `/api/v1/new` and queries physical panel/segment layout.
- **`test-nanoleaf` Verification Command**: Streams a rotating rainbow test pattern to verify Nanoleaf UDP connectivity and perimeter segment sequencing.
- **3D Depth & Height Room Projection**: Maps Philips Hue 3D coordinates `[X, Y, Z]` into screen sampling zones; front-stage lights maintain tight directional focus, while rear surrounds expand into diffuse ambient reflections.

## [0.2.0] - 2026-09-18

### Added
- **Dynamic Letterbox / Aspect Ratio Auto-Detection**: Real-time row-variance analysis detecting top/bottom black bars on 2.39:1 / 2.0:1 anamorphic content and re-anchoring zone coordinates to the active film frame.
- **Saturation-Weighted Dominant Color Sampling**: Non-linear chroma saturation weighting ($1.0 + \gamma \cdot S^2$) preventing bright accent colors from being diluted by neutral background noise.
- **Scene-Cut Instant Snap & Adaptive EMA**: Global color difference tracking snapping instantly ($\alpha = 1.0$) on hard camera cuts while maintaining smooth ($\alpha = 0.35$) temporal tracking during camera pans.
- **OLED Near-Black Noise Gate**: Suppresses video compression dither and near-black artifacts ($< 2\%$ luminance) to true zero to eliminate distracting bias light flicker.
- **Adaptive Deadband Throttling**: Reduces UDP transmission to a 2 Hz keep-alive heartbeat when the video is paused or static, slashing Bridge load and WiFi airtime by 90%.
- **systemd Watchdog & Status Integration**: Integrated `sd-notify` with `WatchdogSec=10` and live status reporting (`systemctl status lg-hue-sync`).
- **Dynamic libvtcapture Loading**: Uses `libloading` for dynamic runtime resolution of `/usr/lib/libvtcapture.so`.

## [0.1.0] - 2026-09-17

### Added
- Initial release of headless Rust sync daemon for LG C1 webOS Smart TVs.
- Zero-copy display frame capture via `libvtcapture.so` hardware scaler.
- Legacy 8-bit midtone lift (historically described as HDR10/Dolby Vision tone mapping; source transfer metadata was not available).
- Philips Hue Gamut C boundary clamping and CIE 1931 xy mode.
- Direct DTLS 1.2 PSK UDP streaming on port 2100.
- Automatic pushlink pairing (`pair`), test pattern (`test-pattern`), and test capture (`test-capture`) CLI commands.
