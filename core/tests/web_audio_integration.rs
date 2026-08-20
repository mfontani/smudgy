#![cfg(feature = "web-audio")]

//! End-to-end Web Audio smoke through a real Smudgy trusted session isolate.
//!
//! The silent system sink keeps this deterministic and hardware-independent;
//! the same extension uses CPAL when the app enables `web-audio-cpal` and a
//! script constructs the default `AudioContext`.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use smudgy_core::session::runtime::RuntimeAction;
use smudgy_core::session::{BufferUpdate, SessionEvent, SessionId, SessionParams, spawn};

const WEB_AUDIO_TS: &str = r#"
import { echo } from "smudgy:core";

const required = [
  AudioContext,
  OfflineAudioContext,
  AudioBuffer,
  AudioBufferSourceNode,
  GainNode,
  OscillatorNode,
];

if (!required.every((value) => typeof value === "function")) {
  throw new Error("Web Audio constructors were not installed");
}

const context = new AudioContext({ sampleRate: 48_000, sinkId: "none" });
const gain = context.createGain();
const oscillator = context.createOscillator();

gain.gain.value = 0.05;
oscillator.frequency.value = 440;
oscillator.connect(gain);
gain.connect(context.destination);

oscillator.onended = async () => {
  await context.close();
  echo("WEB_AUDIO_OK");
};

oscillator.start();
oscillator.stop(context.currentTime + 0.02);
"#;

#[tokio::test]
async fn trusted_smudgy_script_can_render_and_close_web_audio() {
    let home = tempfile::tempdir().expect("create temp home");
    let home_path = home.path().to_path_buf();
    std::mem::forget(home);
    smudgy_core::set_smudgy_home(&home_path);
    let home_path = smudgy_core::get_smudgy_home().expect("smudgy home");

    let server = "WebAudioSmoke";
    let modules_dir = home_path.join(server).join("modules");
    std::fs::create_dir_all(&modules_dir).expect("create module directory");
    std::fs::create_dir_all(home_path.join(server).join("logs")).expect("create log directory");
    std::fs::write(modules_dir.join("web-audio.ts"), WEB_AUDIO_TS).expect("write Web Audio script");

    let params = Arc::new(SessionParams {
        session_id: SessionId::from(7_401),
        server_name: Arc::new(server.to_string()),
        profile_name: Arc::new("Test".to_string()),
        profile_subtext: Arc::new(String::new()),
        mapper: None,
        package_client: None,
        extra_script_extensions: Arc::new(Vec::new),
        on_engine_rebuild: None,
    });

    let mut events = Box::pin(spawn(params));
    let mut runtime_tx = None;
    let mut transcript = Vec::new();

    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = events
                .next()
                .await
                .expect("session ended before Web Audio smoke completed");
            match event.event {
                SessionEvent::RuntimeReady(tx) => runtime_tx = Some(tx),
                SessionEvent::UpdateBuffer(updates) => {
                    for update in updates.iter() {
                        if let BufferUpdate::Append(line) = update {
                            transcript.push(line.text.clone());
                            if line.text == "WEB_AUDIO_OK" {
                                return;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for Web Audio\n{}", transcript.join("\n")));

    runtime_tx
        .expect("RuntimeReady should precede completion")
        .send(RuntimeAction::Shutdown)
        .ok();
}
