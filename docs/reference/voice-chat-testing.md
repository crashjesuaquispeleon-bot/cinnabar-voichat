# Voice Chat preview — manual test guide

> **Preview only.** This branch tests local microphone capture and local monitoring. It does not connect two players: voice encoding, signaling, authenticated room admission, a media router/SFU and remote playback are not implemented.

## Get the source

Use the feature branch:

```powershell
git clone --depth 1 --branch feature/voice-chat-foundation https://github.com/crashjesuaquispeleon-bot/cinnabar-voichat.git
cd cinnabar-voichat
```

This is source code, not a ready-to-run Windows installer.

## Build on Windows

Install Rust toolchain 1.99.0, Go, GNU Make, Git for Windows (including Bash), and Visual Studio C++ Build Tools with the Windows SDK. Ensure `cargo`, `rustup`, `go`, `make`, `bash` and PowerShell are available from the terminal.

From the repository root, run:

```powershell
make play
```

The target prepares assets, builds the Go Bedrock core, Rust client and shader worker, and fetches the Windows DXC helper. First launch may download vanilla resources after you accept the Minecraft EULA.

This branch has not been qualified on physical Windows 10 hardware. A successful build does not prove that a particular microphone, driver or OS privacy configuration works.

## Test the local microphone path

Use headphones to avoid acoustic feedback.

1. Launch Cinnabar, sign in, and join an active multiplayer world/session.
2. Open **Settings → Sound** and enable **Voice Chat**. It is disabled by default.
3. Press **Refresh** under Microphone Input. Select **System default input** or one exact device from the current list. The selection token is temporary and is not saved as a hardware preference.
4. Check microphone permission status. If it says **Denied**, allow desktop microphone access in Windows privacy settings, then press **Retry** in Cinnabar.
5. Choose **Push to Talk** or **Open Microphone**. Push to Talk is the default; its action defaults to **V** and can be remapped under Controls.
6. Leave **Input Sensitivity** at 100% initially. Adjust it between 0% and 200% to suit the input level.
7. Speak while holding the PTT key (or while Open Microphone is enabled). When the local monitor/output path is available, you should hear your microphone locally. Release PTT or mute to close the PCM gate.
8. Test **Toggle Voice Mute** (default **M**). It changes effective mute without rewriting the saved Sound setting.
9. Refresh and select another microphone. Disconnect from the world and confirm capture stops; reconnect before testing again.

## What this preview proves

- It can exercise CPAL capture, current-device selection, sensitivity, PTT/open-mic gating, local mute, bounded PCM queues, local monitoring and session teardown.
- It does not send microphone audio to another player or a voice server. A second Cinnabar client will not create a voice conversation.
- The permission label is best-effort classification of the capture-open result, not a universal Windows permission API.
- Capture currently accepts an exact 48 kHz `f32` or `i16` input format. A device/driver that cannot provide one may be rejected.
- Refresh invalidates previous device rows; select again after refreshing or after a device disappears.
- Microphone audio is not saved as a recording by this preview, and this branch has no remote voice transport.

## Report a test result

Include the Cinnabar commit SHA, Windows version, microphone model/driver, selected input name, activation mode, permission label and exact reproduction steps. Do not share account credentials or authentication tokens.

A successful local test is Stage 1 evidence only. Remote voice remains blocked until the authenticated room-service and media-routing contracts documented in [voice-chat.md](voice-chat.md) exist.
