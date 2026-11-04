//! Positional, category-mixed sound playback driven by packets, local motion and ambience.

pub mod ambient;
mod bank;
pub mod echo;
pub mod engine;
pub mod inventory;
pub mod listener;
pub mod local;
#[cfg(not(target_arch = "wasm32"))]
pub mod media;
mod music;
pub mod predicted;
mod route;
mod server;
pub mod settings;
pub mod systems;
mod voice;
pub mod voice_capture;
pub mod voice_controller;
pub mod voice_input;
pub mod voice_loopback;
pub mod voice_pcm;
pub mod voice_permission;
pub mod voice_remote;
pub mod voice_stream;
mod water;
pub mod weather;

pub use bank::{SoundBank, SoundDecodeDriver, SoundDecodeRequest, sound_bank_path};

pub use engine::AudioEngine;
pub use predicted::LocalBlockCue;
#[cfg(any(test, feature = "test-support"))]
pub use server::{SERVER_SOUNDS_TEST_LOCK, current_generation as server_sounds_generation};
pub use server::{ServerSoundPack, publish_server_sounds};
#[allow(unused_imports)]
pub use settings::AudioSettings;
#[allow(unused_imports)]
pub use systems::UiSoundCue;
pub use voice::OUTPUT_RATE;

pub use echo::{EchoLedger, EchoOrigin, EchoSubject};
pub use systems::BLOCK_ECHO_SECONDS;
