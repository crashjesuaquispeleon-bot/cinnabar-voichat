# Cinnabar Voice Chat — P2P Alpha

Status (2026-11-04): **P2P implementation is present; CI and real-device qualification are still in progress. Not approved for public release.**

## Goal

Provide a no-VPS, two-player voice path for Windows and Android using direct WebRTC media. Keep the existing SFU transport available as a separate mode and do not remove or weaken its admission checks.

## Implemented

### API broker and signaling

- Reuses the authenticated Cinnabar API WebSocket and the FriendHost-scoped room broker.
- The broker generates room/session UUIDs and derives the offerer role from stored membership; clients cannot choose a room, target XUID, peer identity or role.
- Signaling is accepted only for the sender's current stored room/session, a live host lease and exactly two live members.
- The FriendHost owner is the offerer; only the guest may send an answer. Offers, answers and bounded ICE candidate messages are relayed only to the other room member.
- SDP/ICE payloads are bounded to 8 KiB. Existing authenticated WebSocket frame and per-socket flood limits also apply.
- Broker-only P2P room creation and closure do not require SFU credentials. SFU admission issuance remains separately gated.
- Vitest coverage includes two-member offer/answer/candidate exchange, outsider rejection, wrong-role rejection, stale-session rejection, oversized payload rejection and P2P room lifecycle without SFU credentials.

### Native Go transport

- Adds a Pion WebRTC peer with broker-derived offerer/answerer role and a binary DataChannel.
- Uses the existing 20 ms mono PCM16 frame format, source IDs, monotonic sequence checks, bounded receive queue, transmit gate and backpressure handling.
- The Go bridge negotiates using the authenticated API signaling adapter, forwards local IPC PCM frames and sends remote frames back through IPC.
- The bridge rechecks membership/session role every 15 seconds and closes when the peer or local room is lost.
- Public Google STUN is configured as best-effort discovery when no custom ICE configuration is supplied. TURN is not included.
- Go tests cover two in-process Pion peers exchanging audio both ways and a two-bridge IPC integration test. The IPC disconnect test was corrected after CI exposed a stale write to the already-closed remote endpoint; the correction is awaiting a successful rerun.

### Application integration and packaging

- P2P is the default native transport when Cinnabar API is available. SFU remains selectable explicitly with `CINNABAR_VOICE_TRANSPORT=sfu`.
- FriendHost maintains the authenticated guest roster and renews the broker room lease using full snapshots.
- Windows x64 setup EXE and Android arm64 APK workflow jobs are defined for this branch.
- Android declares optional microphone hardware and requests the runtime microphone permission without blocking gameplay when declined.
- Local microphone capture, PTT, mute and remote playback are connected through the native IPC bridge.

## Trust and lifecycle limits

- The broker remains the authority for room IDs, session IDs, participant XUIDs, signaling roles and membership changes.
- The current FriendHost roster trust limitation remains: the API verifies authenticated live accounts but does not independently attest arbitrary third-party Bedrock servers. The supported path is Cinnabar FriendHost.
- The native bridge rechecks membership every 15 seconds. Revocation is therefore not instantaneous for an already established direct media path; peer/room closure and lease expiry are also enforced.
- Public STUN alone cannot guarantee connectivity through restrictive or symmetric NATs. A TURN service is required for broader coverage. Direct-connect failure must be reported honestly; the client must not fall back to an unauthenticated endpoint.

## Latest validation snapshot

- API CI passed on commit `391b6cbfed996cb5d9b3fdcbfe37a64055ced567`, including `pnpm typecheck` and `pnpm test`.
- Client formatting passed on the current code line after the Bevy `SystemParam` derive was qualified; latest observed run: [CI run 41917825868](https://github.com/crashjesuaquispeleon-bot/cinnabar-voichat/actions/runs/41917825868).
- Latest client CI is still running its preflight/browser checks; Go and Rust tests have not yet completed on the newest commit.
- Latest Windows and Android packaging run is still compiling; it has not uploaded verified artifacts: [Alpha build run 41917817335](https://github.com/crashjesuaquispeleon-bot/cinnabar-voichat/actions/runs/41917817335).
- There is no verified EXE/APK download and no physical Windows-to-Android microphone/speaker test yet.
- API and client PRs remain Draft. No Worker deployment, production secret configuration or PR merge is included in this work.

## Remaining qualification

1. Complete client Go tests, Rust tests, clippy/architecture checks and browser checks on the latest commit.
2. Build the Windows installer, smoke-test its layout and upload the artifact.
3. Build the Android arm64 APK, verify its signature and upload the artifact.
4. Install the same commit on two physical devices with separate Cinnabar accounts and test Windows-to-Android and Android-to-Windows audio.
5. Test microphone permission denial/retry, PTT release, quick mute, disconnect, reconnect, host closure, member removal, and NATs that require TURN.
6. After the API PR is merged and deployed to a test environment with explicit authorization, verify real broker signaling before considering a public Alpha.

No production deployment or release approval is implied by a passing CI build.
