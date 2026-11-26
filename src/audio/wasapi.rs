//! Capture the Windows default render endpoint (loopback) or microphone.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, SyncSender, channel, sync_channel};
use std::thread;

use anyhow::{Context, Result, bail};

use super::{FRAME, RATE, Source, Speaker, forward_frames};
use crate::event::Event;
use crate::windows::{Wasapi, default_output};

pub fn default_sink() -> Option<String> {
    default_output()
}

/// This backend exposes the default render endpoint as the system audio source.
pub fn list_streams() -> Vec<(u32, String)> {
    Vec::new()
}

pub struct Capture {
    stop: Arc<AtomicBool>,
}

impl Capture {
    pub fn start(
        source: &Source,
        speaker: Speaker,
        generation: u64,
        events: Sender<Event>,
        audio: Sender<Vec<u8>>,
    ) -> Result<Self> {
        let mic = match source {
            Source::System => false,
            Source::Microphone => true,
            Source::Stream(_) => bail!("choose All system audio on Windows"),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (frames, incoming) = sync_channel(256);
        let (outcome, result) = channel();
        thread::Builder::new().name("clueless-wasapi".into()).spawn(move || {
            let why = capture(mic, &worker_stop, &frames)
                .err()
                .map_or_else(String::new, |error| format!("{error:#}"));
            let _ = outcome.send(why);
        })?;
        thread::spawn(move || {
            forward_frames(incoming, speaker, generation, &events, audio);
            let why = result.try_recv().unwrap_or_default();
            let _ = events.send(Event::CaptureEnded(generation, speaker, why));
        });
        Ok(Self { stop })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn capture(mic: bool, stop: &AtomicBool, frames: &SyncSender<Vec<u8>>) -> Result<()> {
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    let endpoint = if mic { "the default microphone" } else { "the default playback device" };
    let mut input = Wasapi::start(mic, RATE as u32).with_context(|| format!("cannot capture {endpoint}"))?;
    let mut pending = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        input.wait()?;
        while !stop.load(Ordering::Relaxed) && input.append_packet(&mut pending)? {
            let mut chunks = pending.chunks_exact(FRAME * 2);
            for frame in &mut chunks {
                if frames.send(frame.to_vec()).is_err() {
                    return Ok(());
                }
            }
            let consumed = pending.len() - chunks.remainder().len();
            pending.drain(..consumed);
        }
    }
    Ok(())
}
