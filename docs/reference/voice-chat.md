# Cinnabar Voice Chat

## Product contract

The first release is a Cinnabar-client-only voice feature.

- **Room mode:** global voice. Every Cinnabar client admitted to the same voice room can hear every other unmuted participant; there is no distance attenuation or proximity filter.
- **Room scope (confirmed by the user):** players can communicate only when they are in the same active Minecraft multiplayer session/partida. The voice room is bound to that exact joined session; players in different servers or different partidas cannot hear each other. "Global" means everyone in that one session can hear everyone else, regardless of in-game distance.
- **Client compatibility:** Cinnabar only. Vanilla Bedrock and Java clients are out of scope.
- **Microphone modes:** open microphone and Push to Talk (PTT), selectable in Sound settings. Voice Chat is disabled by default; PTT is the default activation mode and the semantic PTT action defaults to V (remappable).
- **User controls:** Sound settings expose enable/disable, activation mode, persisted microphone mute, input sensitivity (0–200%), microphone permission status/retry, and a transient selector for the system default or an enumerated input device. The separate in-game **Toggle Voice Mute** action is remappable (default **M**) and does not overwrite the saved mute preference. Per-player receive volume and remote participant indicators require the not-yet-built network voice stage.

## Current implementation status (2026-11-04)

Stage 1 (local microphone capture) is implemented as a prototype: CPAL input is owned by a persistent worker, uses bounded 48 kHz mono / 20 ms queues, supports Open Microphone and PTT, keeps local mute separate, and stops/clears on capture lifecycle changes. Device selection uses worker-owned handles and temporary catalog tokens. Physical-device qualification and platform-specific OS permission UX remain incomplete.

Stage 2 now has a substantial code-level media path, but it is not enabled in the released Cinnabar core.

Implemented:
- Go core/voice verifies broker-signed Ed25519 admission tokens and maintains a room registry with one-use token IDs, expiry/renewal, participant caps, revocation, and room-to-session binding.
- The Pion WebRTC SFU performs signaling and bounded, unordered DataChannel fan-out only between peers whose signed admissions resolve to the same room and authoritative session claim. It enforces the server-side transmit gate and rejects malformed, duplicate, stale, or over-rate frames. The standalone SFU admin API now also supports immediate broker-only revocation of one participant's current admission jti, in addition to closing an entire room/session.
- core/cmd/voice-sfu is an independently deployable TLS service. It requires the broker public key and explicit operator-managed ICE/TURN configuration; it does not hold or generate the signing private key.
- The native Rust app has a separate local IPC worker. It converts captured 48 kHz frames to the current 16 kHz PCM16 wire format, sends only after the Go bridge acknowledges the transmit gate, receives remote frames, and attaches them to the existing client audio mixer.
- The bridge closes and requests fresh admission when the local WorldClock session generation or WorldStream actor session ID changes. Those values are local teardown discriminators only; they are never sent as proof of identity or partida membership.
- Go tests cover signed admission and SFU room isolation. A new local-IPC/two-peer integration test exercises both audio directions, acknowledged PTT gating, and admission cleanup; its CI result is still pending.

Not yet available in a live installation: the trusted Cinnabar broker/token issuer and a general authoritative partida-membership adapter are not present in this repository. The regular bedrock-core startup does not yet inject a production VoiceBridgeConfig, and no WSS SFU/TURN endpoint or broker public key is deployed/configured. Therefore the local app bridge safely remains inactive and two players still cannot talk in an ordinary running Cinnabar build.

The current media profile is uncompressed PCM over WebRTC DataChannels for initial two-client validation. Opus/RTP, a jitter buffer, speaking indicators, per-player receive volume, participant UI, physical-device tests, and Windows/macOS/Linux qualification remain outstanding. The microphone permission indicator remains best-effort OS error classification, not a universal permission API.

## Repository facts confirmed during initial inspection

- `crates/client-audio` owns client sound playback, audio settings and the Bevy adapter. Its current playback path uses `rodio` and decoded PCM; this is output functionality, not microphone capture.
- `crates/bridge` owns bounded framing for the local Rust/Go process stream. That stream carries Bedrock session frames and control handoff. It should not become the real-time voice media path: voice has different latency, loss, congestion and lifecycle requirements.
- `core/` is the Go integration process. The module already declares Pion WebRTC and the repository already has a browser relay contract in `crates/bridge` / `web/transport`. Their presence does **not** establish that a native voice room, SFU, signaling API or authorization path exists.
- No existing microphone-capture implementation or end-to-end voice-chat feature was identified in the inspected client audio modules. `client-audio` currently depends on `rodio` for output; there is no capture dependency in its manifest.

## Capture-backend findings (repository inspection)

- The locked dependency graph already contains **CPAL 0.15.3** through `rodio 0.20.1`; the lockfile does not show a second CPAL version. Existing native output code imports `rodio::cpal` directly in `crates/client-audio/src/named_audio/backend/native.rs` and uses its `HostTrait` / `DeviceTrait` APIs.
- This makes CPAL the lowest-friction capture candidate to prototype: it avoids introducing a second audio host stack or an immediate new direct dependency. The existing use proves the re-export is available in this crate, but it does **not** yet prove input-stream behavior, device switching, permission UX, or all-target builds.
- Existing output ownership is deliberately kept on a dedicated `audio-output` thread in `named_audio/backend/native.rs`; voice capture must have similarly explicit stream ownership and teardown, but should not reuse the game-output mixer as a microphone transport. The existing `AudioDevice::start_capture` is a pull-driven capture of *game playback* for recording/mixing, not microphone input.
- `packaging/macos/Info.plist.in` includes `NSMicrophoneUsageDescription`. Native capture now opens the default CPAL input stream when voice is enabled in an active session; the first open outcome acts as a best-effort permission probe. Native permission settings UI, denial recovery and signing/sandbox review remain outstanding.
- The crate declares `platform-specific = ["macos", "windows"]`, while its native output implementation is compiled for every non-wasm target. Confirm the actual release target matrix before deciding whether voice input is desktop-only or should include other native targets.

The native CPAL capture path runs inside the persistent microphone owner worker. The worker retains CPAL Device handles in a generation-scoped catalog and creates, plays, stops and drops the exact selected Stream on that same thread. It requires an exact 48 kHz `f32` or `i16` configuration; the callback downmixes directly into fixed PCM frames without temporary vectors. `BoundedPcmQueue` uses the existing `crossbeam-queue` dependency's bounded `ArrayQueue`: 48 kHz mono, 20 ms / 960-sample frames, 15 slots by default (~300 ms), with non-blocking drop-newest overflow counters. The Sound settings panel persists Voice Chat enablement, local mute, activation mode and input sensitivity. It also renders a transient microphone list with explicit system-default/device selection and a best-effort permission status plus retry. Voice defaults disabled, Push to Talk is the activation default, and mute blocks transmission without disabling Voice Chat. The controller opens capture only when voice is enabled, permission permits probing and a multiplayer session is active; the transmit and monitor queues remain closed unless Open Microphone or a held PTT action permits transmission. The app's session-audio lifecycle adapter binds `session_active` to the presence of a `WorldStream` and resets capture when `(WorldClock::session_generation(), actor_session_id())` changes or the stream disappears. These values are local lifecycle discriminators only, never backend room authorization. Stop the producer before clearing the queue during lifecycle teardown so no callback can repopulate stale-session audio.

## Current staged media flow

CPAL microphone -> local permission / enable / PTT gates -> bounded 48 kHz PCM queue -> native Rust voice worker (48 kHz to 16 kHz PCM16) -> dedicated Rust/Go voice IPC -> Go VoiceBridge (requires a trusted VoiceAdmissionSource) -> broker-signed short-lived room admission -> WSS signaling and Pion WebRTC SFU -> same-room / same-partida DataChannel fan-out with server-generated ephemeral speaker IDs -> Go IPC downlink -> independent per-speaker receive queues, sequence validation and concurrent mixing in VoiceRemotePlayback -> client audio mixer.

The Rust IPC client and the Go SFU/bridge layers now exist, but the Go core starts the voice endpoint only when an operator supplies a trusted VoiceBridgeConfig. The shipping bedrock-core command does not yet supply one. Do not wire a placeholder or local signing key to make the socket appear: the broker must independently authenticate the account and prove the active partida first.

### Existing local session signals (confirmed in source)

- `client-audio::SequencedAudioEvent` carries `origin_stream_session_id`, sourced from `WorldStream::authority().actor_session_id()`. This identifies the originating local world stream; it is not a globally meaningful room ID or an authenticated account identity.
- `SessionObservation::session_generation()` is already paired with the current stream identity in the session-audio adapter. `SessionAudio` also tracks dimension changes and clears retained outcomes when its observed `(generation, dimension)` changes.
- When the world stream disappears, `drain_sequenced_audio_into_session` clears unread retained messages and ends the bound audio session. This is useful lifecycle precedent, but it currently governs game sound outcomes, not voice capture or remote media.
- `client-runtime::NetworkHandle` owns the local core endpoint only while a joined session is live and exposes shutdown/retirement paths. It is a possible place to observe lifecycle transitions, not a voice-media transport.
- These identifiers are process-local lifecycle signals. A backend must receive a separately authenticated binding from a trusted Cinnabar service; it must never accept `actor_session_id`, `session_generation`, or a client-selected room name as proof of membership.

The earlier initial-inspection recommendation was to evaluate an operator-owned SFU before selecting a transport. As of 2026-11-04, the repository has a Pion WebRTC SFU implementation and a standalone TLS process at `core/cmd/voice-sfu`; this is deployable code, not an operated endpoint. A full-mesh topology remains out of scope. The Pion dependency alone was not proof of a service boundary; the new SFU code still requires a trusted broker issuer, deployed WSS/ICE configuration, and production integration.

## Architectural constraints

1. Keep capture, encoding, network I/O and decoding off the Bevy frame-critical path. Use bounded queues and explicit backpressure/drop policy; never let a slow receiver stall game packet processing or rendering.
2. Keep real-time voice media separate from Bedrock packet framing. Reuse existing identity and session lifecycle facts where possible, but introduce a dedicated media transport contract.
3. Keep device APIs and capture ownership in a low-level audio/platform owner. Keep Bevy/UI integration in the appropriate client adapter; do not put Bevy dependencies into a domain crate.
4. Do not start capture until the user has granted OS permission and explicitly enabled voice. Open-mic mode may transmit only while enabled; PTT transmits only while the configured key is held. Local mute must stop transmission immediately.
5. On disconnect, reconnect, account change or world-session change, stop capture/transmission and retire room membership, queued frames and remote playback belonging to the old session.
6. Bound frame size, queue depth, participant count, decoder work and jitter-buffer memory. Validate all signaling and media metadata; do not accept arbitrary room IDs or participant identities from an untrusted client.
7. Do not record, persist or upload raw microphone audio. Diagnostics must not include voice payloads, credentials or private room tokens.
8. Preserve Cinnabar's license and third-party notices. Any new codec or capture library requires license, platform and packaging review before adoption.

## Microphone discovery boundary

`audio::voice_capture` defines a platform-independent `MicrophoneBackend` interface and a `MicrophoneDevice` descriptor. The descriptor exposes a display name, a default-device flag and an opaque backend-local ID that is valid only for the enumeration snapshot that produced it. The interface is deliberately limited to discovery: it does not open a device, capture PCM, or own a worker. This keeps CPAL types out of the audio policy and allows deterministic mock implementations in unit tests.

A native `CpalMicrophoneBackend` now implements discovery through the already-available `rodio::cpal` re-export. It enumerates CPAL input devices and returns snapshot-scoped index IDs; it does not retain device handles, open a stream, or capture samples. The default-device flag is matched by name only when that name occurs exactly once in the snapshot; duplicate names are left unmarked rather than guessed. This remains presentation metadata, not stable device identity. The caller must invoke enumeration on a worker/owner thread, never from Bevy's frame schedule. OS permission prompting/denial handling, stream opening and asynchronous request/result wiring remain unimplemented. A device ID must not be persisted as a stable hardware identity or sent to a server.

## Capture review notes (provisional)

A review posted to PR #1 under the heading "Grok review — platform / capture / PCM queue" agrees with the CPAL reuse direction and recommends a dedicated stream owner, mockable capture boundary, fixed-format PCM and a local loopback acceptance test. Treat these as review proposals pending implementation and CI, not as tested behavior.

Version-specific platform correction: the repository locks CPAL **0.15.3**. Its upstream v0.15.3 README lists Linux support through **ALSA or optional JACK**, Windows through WASAPI by default, and macOS through CoreAudio. Do not describe PipeWire or PulseAudio as native CPAL 0.15.3 backends; newer CPAL documentation lists those optional backends, but that is not evidence they exist in the locked version. Linux first-release behavior must be qualified against the actual ALSA device routing and build environment.

The macOS purpose string is a real prerequisite: Apple's `NSMicrophoneUsageDescription` property is required for apps that access the microphone. Confirm signing/sandbox entitlements in the packaging path as well before shipping capture. Do not add the key alone and claim permission handling is complete.

For the callback queue, preallocate fixed-capacity storage and fixed-size frame slots before starting the stream. Avoid a per-frame `Box<[f32]>` allocation. Prefer a bounded non-blocking producer operation with an explicit overflow counter; dropping the newest frame is the simplest safe initial policy unless the selected queue explicitly supports producer-side overwrite without blocking or races. A 10–20 ms frame and 48 kHz mono are review suggestions, not yet product constants.

References:
- CPAL 0.15.3 platform support: https://github.com/RustAudio/cpal/blob/v0.15.3/README.md
- Apple microphone purpose-string requirement: https://developer.apple.com/documentation/BundleResources/Information-Property-List/NSMicrophoneUsageDescription

## Work plan and acceptance gates

### Stage 0 — Architecture and service discovery (current)

- [x] Confirm product choices: global voice within the same active Minecraft session, Cinnabar clients only, open mic + PTT.
- [x] Inspect current client playback, bridge framing and browser relay ownership at a high level.
- [x] Confirm local world-stream/session lifecycle signals and distinguish them from backend-authenticated room identity.
- [ ] Identify the authoritative Cinnabar account/session identity and the service that can authorize a room for the same joined Minecraft server.
- [ ] Determine whether a maintained voice SFU/service already exists in the wider Cinnabar deployment; if not, document the required service and operational owner.
- [ ] Compare capture/codec/transport options against supported desktop platforms, latency, licensing and packaging.
- [ ] Define a versioned, authenticated signaling contract and bounded media-frame contract before implementing network transport.

### Stage 1 — Local audio vertical slice

- [x] Add a device-independent `VoiceInputPolicy` in `client-audio` and register it as a client resource. It gates capture on OS permission, explicit enablement and an active multiplayer session; it gates transmission separately for open-mic/PTT and local mute.
- [x] Add deterministic tests for permission gating, open-mic, PTT press/release, mute, disconnect, permission revocation, disable and mode changes.
- [x] Implement native CPAL input-device enumeration behind `MicrophoneBackend`; it is a synchronous discovery adapter and must be called off the Bevy frame schedule.
- [x] Implement a dedicated owner-thread CPAL opener with a synchronous ready/error handshake and exact 48 kHz `f32` / `i16` negotiation.
- [x] Rate-limit transient microphone open failures and unexpected runtime stream stops with bounded exponential backoff; clear stale PCM before reopening.
- [x] Wire capture policy to the active `WorldStream` lifecycle; stop and clear on disconnect, transfer or stream-generation change.
- [ ] Add OS-specific privacy-settings links and platform-qualified permission guidance. The Sound panel displays best-effort permission status and offers a denied-state retry after the player changes system privacy settings. The in-game quick-mute action is implemented and remappable. Sound settings persist enablement, local mute, activation mode and input sensitivity.
- [x] Define mockable `VoiceCaptureOpener` / `VoiceCaptureStream` traits, fixed-frame accumulation, allocation-free conversion helpers, mock lifecycle tests, and the first native CPAL stream owner.
- [x] Connect the remappable semantic PTT input and saved enablement/activation-mode settings to the policy.
- [x] Add a bounded, thread-safe PCM frame queue with fixed internal format, explicit overflow accounting and a live policy-gated CPAL capture producer.
- [x] Add a bounded mock capture-to-playback loopback test covering the existing mixer, independent transmit queue and disconnect cancellation; real-device shutdown still needs platform qualification.
- [x] Add saved enablement, local mute, activation-mode and input-sensitivity controls to the existing Sound settings panel without changing game sound sliders.
- [x] Add a transient microphone-device selector to the Sound panel. It refreshes native devices through the CPAL owner worker, supports explicit system-default or exact-device selection, rejects stale menu revisions, and clears old rows on worker/device failure. Opaque device IDs remain local and are never persisted.

### Stage 2 — Authenticated global rooms (partially implemented)

- [x] Implement SFU-side Ed25519 token verification and a room registry with one-use tokens, room/session binding, expiry, renewal, revocation, and participant caps.
- [x] Implement Pion signaling and same-room media fan-out with server-side transmit gating and bounded PCM frames.
- [x] Add a standalone TLS SFU executable with pinned broker public key and explicit ICE/TURN configuration.
- [x] Connect the native Rust capture/playback path to the Go voice IPC bridge; close and re-admit when the local game-session lifecycle changes.
- [x] Add Go two-peer SFU tests and a local IPC two-peer integration test (CI validation pending).
- [ ] Implement/deploy the trusted broker issuer that derives XUID from the authenticated Cinnabar account and the partida UUID from an authoritative server-side adapter.
- [ ] Inject the real VoiceAdmissionSource and deployed WSS/ICE/TURN settings into the production bedrock-core configuration. Until then, the IPC voice endpoint remains disabled by default.
- [x] Give every downlink stream a random server-generated source ID; validate sequence numbers per source and mix up to the SFU's 64-participant limit concurrently, with bounded per-speaker queues and a two-frame startup prebuffer.
- [ ] Add an adaptive jitter buffer, migrate the media plane to Opus/RTP, add participant speaking state, per-player receive volume and multi-client UI.
- [ ] Verify real two-client and multi-client sessions across separate installations, session transfers, packet loss, reconnect, and shutdown.

### Stage 3 — Product integration and qualification

- [ ] Add settings and in-game voice controls using the existing UI ownership rules.
- [ ] Qualify Windows, macOS and Linux capture/playback and device switching.
- [ ] Measure CPU, memory, bandwidth, end-to-end latency and queue drops at representative room sizes.
- [ ] Document server deployment, configuration, privacy behavior and troubleshooting.

## Microphone permission status

The Sound panel reflects `VoiceInputPolicy` permission state as **Not checked**, **Granted** or **Denied**. The first permitted capture-open attempt is the probe: a successful stream marks Granted, an explicit access-denied outcome marks Denied, and missing devices/unsupported formats remain Unknown or retain the previous state. On desktop this is best-effort error classification, not a portable OS permission API; the native OS prompt may appear when the player enables Voice Chat in an active multiplayer session. After the player changes system privacy settings, the Denied state offers **Retry**; it resets the permission to Unknown so the normal policy may attempt another open, but only while voice remains enabled in a live multiplayer session. The retry does not bypass OS access controls. OS-specific privacy-settings deep links and platform-qualified prompt guidance remain pending.

## Microphone device selection

The Sound panel's Microphone Input section has an explicit Refresh action, a System default action and one row for each device in the latest snapshot. The UI holds only presentation labels and opaque generation-scoped tokens; selections are routed to the native owner worker, which resolves the exact token on the same thread that owns CPAL handles. The menu includes its own snapshot revision so a delayed click from an older rendered list is rejected before it can become a worker request. A worker refresh invalidates prior tokens, and an unplug, enumeration failure or selection error clears the displayed rows so the player must refresh before choosing again. Device enumeration and switching are available only while the capture worker is active (Voice Chat enabled in a live multiplayer session); the system-default choice is disabled unless the refreshed catalog identifies exactly one default. The dedicated operating-system permission prompt, denial recovery and permission-status UX are still pending.

## Voice input sensitivity

The Sound settings panel includes a persisted **Voice Input Sensitivity** slider from 0% to 200%, defaulting to 100%. The app applies the saved value to both the transmit and local-monitor PCM queues. Gain is applied as each fixed frame enters the bounded queue, using an atomic scalar so the CPAL callback does not lock or allocate. Values above unity are clipped to the normalized PCM range; non-finite internal gain values recover to unity. This control changes capture level only: it does not grant microphone permission, bypass mute/PTT, open a microphone, or authorize network transmission.

## Local capture and permission probe implemented

- `VoiceInputPolicy` remains closed by default: permission unknown, voice disabled, no active multiplayer session and no activation mode selected. PTT additionally requires an observed physical release after mute, permission loss, disabling voice, a session boundary or an activation-mode change.
- Native capture uses CPAL 0.15.3 through the existing `rodio::cpal` re-export. A persistent owner thread creates and retains the host, enumerated device handles and active stream; initial capture chooses the system default, while later selection opens the exact opaque token from the latest enumeration snapshot. The current slice accepts exact 48 kHz F32/I16 input only.
- The first open attempt is the Stage-1 permission probe. A successful stream open marks permission Granted; explicit permission/access-denied-like errors mark Denied; missing devices and unsupported formats remain retryable. Transient open failures use an exponential retry delay beginning at 500 ms and capped at 8 seconds, rather than retrying on every Bevy frame. A stream that stops asynchronously after a successful open (for example, after device removal or a host audio error) now enters the same bounded backoff instead of causing a reopen attempt every frame; a deterministic controller test covers that transition. A successful open or a deliberate session stop resets the delay. The controller closes and clears both PCM queues before a reopen attempt so frames from a failed/stopped stream cannot leak into a later capture. Error-string classification is best-effort, not a portable OS permission API or native permission dialog.
- `VoiceCaptureController` opens only while voice is enabled and a multiplayer session is active. Unknown permission is probed once the controller attempts to open; Denied does not spin-reopen. After a persistent worker loses its device, the controller deliberately does not reopen a different default device; it remains available for an explicit refresh and selection.
- The capture callback writes fixed 20 ms mono frames into a bounded, preallocated queue. It does not allocate, log, encode or block.
- The queue has a policy gate and generation tag. It accepts samples only while `transmission_permitted()` is true; PTT keeps the device open but does not retain microphone PCM until the held action is active. Mute, release, mode change, disconnect and session replacement close the gate, advance its epoch and retire stale in-flight frames.
- macOS packaging includes `NSMicrophoneUsageDescription`. The Sound panel presents best-effort permission status, a denied-state retry and the native device list; OS-specific privacy-settings deep links and platform-qualified prompt behavior remain pending. The controller exposes nonblocking refresh, exact-device selection and worker-event polling, now routed through the menu. The mock loopback path is covered; real-device playback and shutdown still need platform qualification. Network transport remains a Stage-2 task.

The permission probe may open the microphone device while voice is enabled in a live session even if PTT is not held. The queue gate prevents samples from being retained or forwarded during that interval.

## Input and lifecycle adapter findings

- Physical desktop key translation is owned by `crates/client-shell/src/physical_input.rs`; the semantic input domain is `crates/input`; the Bevy-facing collection and routing adapter is `app/src/semantic_controls.rs` (with physical sampling in `app/src/semantic_controls/physical.rs`). The audio crate should not poll Bevy keyboard state or own persisted keybinding UI.
- `semantic_input::ActionSnapshot` exposes per-action pressed/held/released phases and authority-release information. `Action::PushToTalk` is now a held semantic action, exposed as the remappable `key.pushToTalk` control with default V. The app adapter forwards the finalized held phase to `VoiceInputPolicy`; `client-audio` does not poll keyboard state.
- The Bevy app configures the session transition systems `drive_session`, `follow_server_transfer` and `recover_session_failure` before its session-audio drain. Voice lifecycle integration belongs at this app adapter boundary so it observes the same join/leave/transfer outcomes, while capture/device ownership remains in `client-audio`. The local `session_generation` is useful to invalidate local capture queues, but is not backend authorization.
- `client-audio`'s architecture policy keeps the audio owner below app/UI orchestration. Keep the CPAL stream abstraction and bounded PCM handoff there; keep physical input and session-observation wiring in the app edge. This preserves the repository's crate direction and avoids adding Bevy input dependencies to a low-level audio backend.

## Confirmed session-isolation behavior

- A voice room belongs to one exact active multiplayer session, not merely to a server address or a global Cinnabar account pool.
- All admitted Cinnabar players in that same partida can talk regardless of distance.
- A player joining another server/session must leave the previous room before joining the new one.
- Disconnecting, changing account, changing world/server session or losing authorization must stop capture/transmission and discard old-session audio.
- The backend must derive/validate the room membership from trusted session state; clients must not choose arbitrary room IDs.

## Open decisions

- Which Cinnabar-owned service/API is authoritative for account identity, current server-session identity and voice-room authorization?
- Is there an existing deployable Cinnabar SFU, or must one be designed and operated separately?
- Which desktop platforms are in the first release? The initial qualification proposal is Windows, macOS and Linux.
- Which codec and media transport meet the selected service's constraints? Opus/WebRTC is a candidate, not a decision.
- Which platform-stable device identity should be used for an optional saved microphone preference? Never persist a generation-scoped catalog token.
- What is the room participant cap and what should happen when the cap is reached?

Until the service and identity questions are answered, the media transport and room implementation remain intentionally incomplete. This document is an architecture proposal, not evidence that voice chat is already functional.


## Push-to-talk semantic input

The semantic input domain now includes `Action::PushToTalk` as a persistent held action (not a one-shot). It is exposed in the remappable controls registry as `key.pushToTalk`; its default keyboard assignment is V, and the binding is supplied through `ControlSettings` rather than polled by `client-audio`. Users can change or unbind it through the ordinary controls binding path.

The app session-audio adapter reads only the finalized `SemanticInputSnapshot` phase for `Action::PushToTalk` and forwards its `held` value to `VoiceInputPolicy::set_push_to_talk_held()`. It runs after `ClientFrameSet::SemanticFinalize` and before `sync_voice_capture_with_policy`, so the controller observes the same frame's policy. Missing or cleared snapshots are neutral/released. The audio crate never reads Bevy keyboard state.

Router and launcher tests cover PTT press/hold/release and remapping. Enablement, saved mute, activation mode and input sensitivity appear in Sound settings. The in-game quick-mute action is a separate remappable one-shot (default M) and does not rewrite the saved preference. A quick toggle resets to the saved Sound-panel preference on a game-session change; PTT must be physically released before it can transmit again after a privacy boundary. The device selector and best-effort permission/retry UI are implemented; OS-specific settings deep links and physical loopback qualification remain follow-up work. The local IPC transport and SFU code are now implemented; production admission remains blocked on the trusted broker issuer and partida adapter.

## PTT-aware PCM privacy gate

The capture device can remain open to avoid repeatedly opening the OS microphone for every PTT press, but the PCM queue is closed unless the selected activation mode currently permits transmission. The queue advances a generation whenever this gate changes. Each callback frame is tagged with the generation it observed, and the consumer rejects frames from an older generation. This prevents a callback already in flight during PTT release, mute or session shutdown from being replayed after the gate reopens.

This is a local capture/queue privacy boundary only. The future network sender must independently enforce the same current-session and `transmission_permitted()` checks before encoding or sending any frame.


## Local loopback mixer attachment

The microphone callback now fans each accepted 20 ms PCM frame into two independently bounded queues:
- the transmit queue, reserved for the future network encoder/sender;
- the monitor queue, consumed only by local playback.

This separation is required because queue reads are destructive. A local loopback consumer must never drain the only copy of a frame before the network sender can read it. Both queues share the same transmission-policy gate and generation invalidation, so PTT release, mute, permission loss, and session teardown close and clear both destinations together.

VoiceLoopbackSource is a pull-driven rodio source attached through the existing AudioDevice::play_source path. The attachment system waits for microphone capture and the existing named-audio device to become available and adds at most one source. The source is cancelled when capture ends. It does not open another output device. If the app has no named-audio carrier, the device is disabled, or output initialization fails, attachment remains a no-op and the client continues without local monitoring.

The loopback source is still a local monitor only. It does not implement the voice network, remote playback, room membership, or server authorization. The monitor must remain independently muteable without changing capture/transmission policy.


## Service-boundary audit (2026-11-03)

The Stage-2 gate was rechecked against the current Go integration code before selecting a transport:

- `core/webrelay` is an existing operator-configured HTTP/3 + WebTransport service with an HTTPS/WebSocket fallback. Its exposed channels are the reliable `control` and `play` channels defined by the relay contract. The play channel carries the Minecraft session stream; it is not a low-latency voice media channel and must not be overloaded with microphone packets.
- The relay's browser session keeps its Microsoft account/authcache state and a `proxy.SessionHandler` for the Bedrock join. This is useful evidence of an existing authenticated game-session lifecycle, but it is not yet a documented native-client voice authorization API or a server-issued, opaque voice-room capability.
- `core/cinnabarapi` is a separate authenticated API connection for Cinnabar account-facing features. Its existing lookup and presence contract does not, by itself, authorize a participant for a particular live Minecraft partida.
- `core/go.mod` pins Pion WebRTC, but a dependency declaration is not an operational SFU. No native voice-room admission, signaling, room lifecycle, or media-routing contract was established by this audit.

**Stage-2 implementation remains blocked on service ownership, not on the local microphone prototype.** Before adding network code, the service operator must identify (1) the trusted account identity available to native Cinnabar clients, (2) the authoritative server-side partida/session identifier and its lifecycle, (3) the API that can mint and revoke a short-lived room admission token bound to both identities, and (4) the deployed media-router/SFU endpoint and operational owner. The client must never send an arbitrary room ID and treat it as authorization, nor use local `actor_session_id` / `session_generation` values as backend proof.

Until those contracts are supplied, keep Stage 2 unchecked. Do not add a client-only room implementation, trust a user-supplied room name, send audio through the Bedrock relay, or introduce an unowned SFU deployment. Safe parallel work remains Stage-1 hardening: loopback/mute/disconnect/exit acceptance tests, device-selection and permission UX, and Windows/macOS/Linux qualification.


### Identity and room-admission audit (2026-11-03)

- The native proxy obtains `IdentityData()` from the upstream Minecraft login and passes the canonical display name, XUID and UUID into the local session handoff. This is a trusted integration point for identity correlation, but it is not by itself a room-admission token or proof that a voice service has authorized the current partida.
- Xbox MPSD integration in `core/proxy/friend_session.go` tracks friend-hosted `MinecraftLobby` membership and per-member nonces. `core/proxy/friend_host.go` checks the joining XUID and nonce before admitting a player to that FriendHost world. This proof is scoped to Cinnabar FriendHost sessions; it is not a universal room identity for arbitrary third-party Bedrock servers.
- `core/webrelay` is a browser gameplay relay with control/play channels, not a native voice SFU. The repository does not currently provide a general authenticated voice-room broker, room signaling contract or operated media router.
- Therefore Stage 2 still requires an operator-owned broker/API that binds trusted Cinnabar account identity to authoritative server/partida membership and issues short-lived, revocable room capabilities. A client-supplied XUID, room name, local `actor_session_id` or `session_generation` must never be accepted as proof.
## Microphone discovery boundary audit (2026-11-03)

The current native discovery and capture adapters are intentionally not a selectable-device implementation:

- `CpalMicrophoneBackend` enumerates input devices and returns display names plus opaque `cpal-snapshot-{index}` identifiers. Those identifiers are valid only for that enumeration snapshot; they are not stable hardware IDs and must not be persisted or sent to a service.
- `CpalVoiceCaptureOpener` independently asks CPAL for `default_input_device()`. It does not accept a selected descriptor or reopen a device by the discovery identifier.
- The discovery adapter drops the CPAL device handles after enumeration. Re-enumerating later and treating the same numeric index as the same device would be unsafe because device order and availability can change.
- Default-device detection still compares human-readable names because the discovery adapter does not retain a comparable native handle. The current `unique_default_name` helper marks a default only when its name occurs exactly once; duplicate names therefore produce no default label rather than incorrectly marking multiple devices. This is conservative presentation metadata, not a reliable device identity.

**Do not wire the snapshot identifier directly into persisted audio settings or claim that the settings select a microphone.** A real selection implementation needs one backend-owned selection contract that binds a fresh enumeration snapshot to the corresponding CPAL device handle, handles disappearance/re-enumeration, and opens the selected handle on the capture owner thread. Persisted preferences should use a backend-supported stable identity where available and fall back explicitly to the OS default when the device is missing. Device handles and native identifiers must remain local and must never become network identity or room authorization.

Until that contract and platform behavior are tested, keep the UI selection marked as pending and continue to use the OS default input device.


## Selectable microphone contract (2026-11-03)

The next device-selection implementation must use a catalog/command boundary owned by the native audio backend, rather than extending the current descriptor-only trait with a naked CPAL index.

### Required lifecycle

1. A backend worker creates the CPAL host, enumerates inputs, and retains the resulting device handles in that worker's private catalog. The UI receives only display descriptors and opaque, process-local selection tokens.
2. Every catalog refresh advances a catalog generation. A token is valid only for the generation that produced it; a refresh, host reset, or device-removal event invalidates older tokens.
3. Selecting a device sends the opaque token to the same backend worker. The worker resolves it against its retained catalog and opens that exact handle. It must reject an unknown or stale token rather than reinterpret it as an index or silently open another microphone.
4. The capture stream remains owned and stopped on the capture worker, as today. The command protocol must not move a CPAL handle across threads unless the pinned CPAL/backend contract explicitly guarantees that operation; keeping enumeration, handle lookup, stream construction, and destruction on one owner thread is the safe default design.
5. If the selected device disappears, stop capture, close both PCM gates, clear transmit and monitor queues, invalidate the catalog, and surface a device-unavailable state. Do not silently switch to another physical microphone. A user-visible “Use system default” choice may deliberately select the current OS default.

### Identity and persistence

- Selection tokens are opaque, process-local catalog capabilities, not `cpal-snapshot-{index}`, hardware identifiers, player IDs, or server room IDs. They are valid only within the catalog instance and generation that issued them.
- Never persist a catalog token. Persist a preferred device only if the platform backend exposes a documented stable identity with suitable semantics. Otherwise persist “system default” and ask the user to choose again after restart.
- Display names are labels only. Duplicate names must remain separately selectable through distinct opaque tokens; name equality must never resolve a selection.
- Enumeration and selection are local-only. No native device ID, display name, or hardware fingerprint is sent to the voice service.

### Required tests before exposing the selector

- Selecting each of two mock devices opens the exact matching retained handle, including when both have the same display name.
- A token from an earlier catalog generation is rejected after refresh.
- Removing a selected device stops capture and clears both PCM queues; it does not fall back silently.
- An explicit system-default selection resolves the default at selection time and handles a changed default deterministically.
- Repeated selection, refresh, disconnect, and shutdown do not leak worker threads or leave a stale stream running.
- Native qualification verifies device changes and unplug/replug behavior on Windows, macOS, and Linux. Mock tests alone do not establish OS behavior.

This was the pre-integration contract. The persistent CPAL worker described in the later owner-worker integration section now retains the catalog handles and implements exact selection. The enablement/activation-mode settings and dynamic device-list UI are now user-facing. Stable device-preference persistence and cross-platform device tests remain pending.

## Explicit microphone preference resolver (2026-11-03)

The platform-independent catalog now exposes `MicrophoneSelection` with two explicit choices: `SystemDefault` and `Device(opaque_token)`. `MicrophoneCatalog::resolve_selection` resolves a selected token only in the current catalog generation and returns a borrowed handle, so the caller can keep native handle ownership local to the catalog's backend worker. It rejects missing/stale tokens and refuses a missing or ambiguous default instead of guessing. Tests cover duplicate display names, stale tokens after refresh, explicit system-default resolution against the current snapshot, and missing/ambiguous default states.

The catalog and resolver are used by the production CPAL owner worker: enumeration retains Device handles, selection resolves an opaque token from the current generation, and the exact handle is opened on the same owner thread. The controller exposes refresh/select/event-polling methods, and the Sound panel renders the dynamic device list. Stable device-preference persistence, native unplug/replug qualification, OS-specific privacy-settings links and cross-platform runtime validation remain pending; catalog tokens must never be persisted.

## Microphone catalog invalidation primitive (2026-11-03)

`MicrophoneCatalog::invalidate()` now advances the catalog generation and drops every retained descriptor/handle entry. A token issued before invalidation therefore resolves as `DeviceUnavailable`, and an explicit `SystemDefault` request returns `NoDefaultDevice` until a fresh snapshot is published. A deterministic unit test verifies both outcomes and confirms the exposed descriptor list is empty after invalidation.

The persistent worker invokes invalidation on the owner thread when its active stream reports failure, then emits `DeviceUnavailable`; the controller keeps the worker available for an explicit refresh and re-selection. CPAL hot-plug notifications are not yet subscribed, so unplug detection currently relies on the stream's asynchronous error/health signal. The selector is now exposed in Sound settings; physical unplug/replug behavior remains to be qualified.

## CPAL owner-worker command contract (2026-11-03)

`audio::voice_capture` now defines the Send-safe command/event vocabulary for the upcoming owner worker: `Refresh`, `Select(MicrophoneSelection)` and `Shutdown`, with `Devices`, `Selected`, `SelectionRejected`, `DeviceUnavailable` and `Stopped` events. Commands carry only owned descriptors/tokens; native CPAL host/device/stream handles must remain inside the worker thread. A selection must stop the previous stream and resolve only against the current catalog generation. A stale token is rejected, never replaced by the system default. Shutdown stops capture and invalidates the catalog.

The production CPAL adapter is wired to `spawn_microphone_worker`: the worker factory constructs the host on its owner thread, retains enumerated CPAL Device handles in the generation-scoped catalog, and creates, plays, stops and drops the exact selected Stream on that same thread. `CpalVoiceCaptureOpener` starts with an explicit `SystemDefault` selection; while the stream worker remains alive, `VoiceCaptureController` can request refresh, select an exact current token, and poll nonblocking worker events. A selected-device loss closes and clears both PCM queues, invalidates the catalog, and does not fall back to another microphone. Deterministic tests cover exact switching, stale-token rejection, disconnect invalidation, controller re-selection, and orderly shutdown. Sound settings expose persisted enablement, local mute, activation mode and input sensitivity, plus the dynamic microphone list and best-effort permission/retry status. Hot-plug event subscriptions, stable device-preference persistence, and physical-device qualification remain outstanding.


## CPAL owner-worker integration (2026-11-03)

The native CPAL path now uses the persistent owner worker described above; the earlier paragraphs in this document that say the CPAL adapter is not wired to the worker are historical notes and are superseded by this section.

- The CPAL host is constructed inside the worker factory. Input enumeration retains the real CPAL Device handles in the worker-owned MicrophoneCatalog; only opaque generation-scoped descriptors cross to the controller.
- The worker opens the exact retained Device associated with the selected token. The default opener deliberately issues an explicit SystemDefault selection after the initial enumeration. CPAL Host, Device and Stream objects never cross the worker boundary.
- The worker owns stream creation, playback, stop and destruction. Its real-time callback only downmixes supported 48 kHz F32/I16 samples into the preallocated transmit and monitor queues. An asynchronous CPAL error flips an atomic health flag; the worker notices it, stops and drops the stream on its owner thread, clears both queues, invalidates the catalog and emits DeviceUnavailable.
- The live VoiceCaptureStream exposes nonblocking refresh, exact-device selection and event polling. VoiceCaptureController exposes matching methods for the app/settings edge. If the selected device disappears, the controller keeps the owner worker alive, closes and clears PCM queues, and does not auto-open the OS default. The user can refresh the snapshot and select a fresh token.
- A stale token is rejected before stopping the current stream. An invalid selection therefore does not interrupt an otherwise working microphone. A valid selection stops the old stream and starts the new retained handle on the owner thread.
- Deterministic worker tests now cover switching between two same-session devices, reporting unplug/disconnect, invalidating the disconnected device token, rejecting stale tokens, and stopping the active stream during shutdown. They protect the ownership protocol but do not replace tests with physical microphones.

Still pending: retain an optional device preference using a platform-supported stable identity (never persist a catalog token), subscribe to platform hot-plug events where supported, and qualify actual device switching/unplug/replug on Windows, macOS and Linux. Voice enablement, local mute, activation mode and input sensitivity are persisted in the Sound panel and applied to VoiceInputPolicy. The panel exposes the device list, permission status and retry action. The default capture probe starts with an explicit OS-default selection; a later manual selection remains local to that worker lifetime. Real hardware behavior and cross-platform CI must pass before marking capture production-ready.

