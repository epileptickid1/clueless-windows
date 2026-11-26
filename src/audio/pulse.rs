//! PipeWire/PulseAudio capture on Linux.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Sender, sync_channel};
use std::thread;

use anyhow::{Context, Result};
use serde_json::Value;

use super::{FRAME, Source, Speaker, forward_frames};
use crate::event::Event;

impl Source {
    fn args(&self) -> Vec<String> {
        // Without these, PipeWire silently re-links a capture to the microphone
        // when the stream or device being recorded goes away.
        let pinned = [
            "--property=node.dont-reconnect=true",
            "--property=node.dont-fallback=true",
            "--property=node.dont-move=true",
        ];
        let target = match self {
            Source::System => "--device=@DEFAULT_MONITOR@".to_string(),
            Source::Stream(i) => format!("--monitor-stream={i}"),
            Source::Microphone => return vec!["--device=@DEFAULT_SOURCE@".into()],
        };
        std::iter::once(target).chain(pinned.map(String::from)).collect()
    }
}

/// Name of the default output device.
pub fn default_sink() -> Option<String> {
    let out = Command::new("pactl").arg("get-default-sink").output().ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// Parses `pactl --format=json list sink-inputs`.
fn parse_streams(json: &str) -> Vec<(u32, String)> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|i| {
            let index = u32::try_from(i["index"].as_u64()?).ok()?;
            let p = &i["properties"];
            let app = p["application.name"]
                .as_str()
                .or(p["node.name"].as_str())
                .unwrap_or("Unknown app");
            let media = p["media.name"].as_str().unwrap_or("");
            let mut label = if media.is_empty() {
                app.to_string()
            } else {
                format!("{app}: {media}")
            };
            if label.chars().count() > 48 {
                label = label.chars().take(47).collect::<String>() + "…";
            }
            Some((index, format!("{label} (#{index})")))
        })
        .collect()
}

/// Applications that are currently playing sound.
pub fn list_streams() -> Vec<(u32, String)> {
    Command::new("pactl")
        .args(["--format=json", "list", "sink-inputs"])
        .env("LC_ALL", "C.UTF-8")
        .output()
        .ok()
        .map(|o| parse_streams(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// A running capture; dropping it stops recording.
pub struct Capture {
    child: Child,
}

impl Capture {
    pub fn start(
        source: &Source,
        speaker: Speaker,
        generation: u64,
        events: Sender<Event>,
        audio: Sender<Vec<u8>>,
    ) -> Result<Self> {
        let mut child = Command::new("parec")
            .args([
                "--raw",
                "--format=s16le",
                "--rate=16000",
                "--channels=1",
                "--latency-msec=30",
            ])
            .arg("--client-name=clueless")
            .args(source.args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot start `parec` (install libpulse)")?;
        let mut out = child.stdout.take().context("no stdout")?;
        let mut err = child.stderr.take().context("no stderr")?;

        let (frames, incoming) = sync_channel::<Vec<u8>>(256);
        thread::spawn(move || {
            let mut bytes = [0u8; FRAME * 2];
            while out.read_exact(&mut bytes).is_ok() {
                if frames.send(bytes.to_vec()).is_err() {
                    break;
                }
            }
        });
        thread::spawn(move || {
            forward_frames(incoming, speaker, generation, &events, audio);
            let mut why = String::new();
            let _ = err.read_to_string(&mut why);
            let _ = events.send(Event::CaptureEnded(generation, speaker, why.trim().to_string()));
        });

        Ok(Self { child })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::parse_streams;

    #[test]
    fn reads_stream_list() {
        let j = r#"[{"index":7,"properties":{"application.name":"Firefox","media.name":"Meet"}}]"#;
        assert_eq!(parse_streams(j), vec![(7, "Firefox: Meet (#7)".to_string())]);
        assert!(parse_streams("garbage").is_empty());
    }
}
