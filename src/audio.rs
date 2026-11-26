//! Audio sources and speech segmentation shared by the platform capture backends.

#[cfg(not(windows))]
#[path = "audio/pulse.rs"]
mod platform;
#[cfg(windows)]
#[path = "audio/wasapi.rs"]
mod platform;

pub use platform::{Capture, default_sink, list_streams};

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::event::Event;

pub const RATE: usize = 16_000;
const FRAME: usize = RATE * 30 / 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    Them,
    You,
}

impl Speaker {
    pub fn label(self) -> &'static str {
        match self {
            Speaker::Them => "Them",
            Speaker::You => "You",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// Everything played on the default output device.
    System,
    /// A single playback stream, for example one browser tab.
    Stream(u32),
    Microphone,
}

/// Energy based voice activity detector with an adaptive noise floor. It only
/// tells whether someone is talking; all audio is transcribed regardless.
pub struct Vad {
    floor: f32,
    active: bool,
    onset: usize,
    silence: usize,
}

impl Vad {
    pub fn new() -> Self {
        Vad {
            floor: 0.001,
            active: false,
            onset: 0,
            silence: 0,
        }
    }

    /// Feeds the loudness of one frame; returns the new state when it changes.
    pub fn push(&mut self, level: f32) -> Option<bool> {
        let threshold = (self.floor * 3.0).clamp(0.004, 0.05);
        let loud = level > threshold;
        // The floor drops quickly and rises slowly, so speech barely moves it.
        let k = if level < self.floor {
            0.2
        } else if self.active {
            0.001
        } else {
            0.01
        };
        self.floor += (level - self.floor) * k;

        if !self.active {
            self.onset = if loud { self.onset + 1 } else { 0 };
            if self.onset < 3 {
                return None;
            }
            self.active = true;
            self.silence = 0;
            return Some(true);
        }

        self.silence = if loud { 0 } else { self.silence + 1 };
        if self.silence < 25 {
            return None;
        }
        self.flush()
    }

    /// Declares silence without waiting for quiet frames.
    pub fn flush(&mut self) -> Option<bool> {
        if !self.active {
            return None;
        }
        self.active = false;
        self.onset = 0;
        Some(false)
    }
}

/// Decodes s16le audio.
fn samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
        .collect()
}

pub fn rms(frame: &[f32]) -> f32 {
    (frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32).sqrt()
}

/// Consume the same 30 ms PCM frames on either platform. Dropping the sender
/// lets Deepgram finish the last words.
fn forward_frames(
    incoming: Receiver<Vec<u8>>,
    speaker: Speaker,
    generation: u64,
    events: &Sender<Event>,
    audio: Sender<Vec<u8>>,
) {
    let mut vad = Vad::new();
    let mut count = 0u32;
    let speaking = |active: bool| {
        let _ = events.send(Event::Speaking(generation, speaker, active));
    };
    loop {
        let change = match incoming.recv_timeout(Duration::from_millis(800)) {
            Ok(frame) => {
                let level = rms(&samples(&frame));
                let _ = audio.send(frame);
                count = count.wrapping_add(1);
                if count.is_multiple_of(4) {
                    let _ = events.send(Event::Level(generation, speaker, (level * 6.0).min(1.0)));
                }
                vad.push(level)
            }
            // A paused stream delivers nothing, which is also a pause.
            Err(RecvTimeoutError::Timeout) => vad.flush(),
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Some(active) = change {
            speaking(active);
        }
    }
    if let Some(active) = vad.flush() {
        speaking(active);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(vad: &mut Vad, level: f32, frames: usize) -> Vec<bool> {
        (0..frames).filter_map(|_| vad.push(level)).collect()
    }

    #[test]
    fn detects_speech_and_pauses() {
        let mut v = Vad::new();
        assert!(run(&mut v, 0.0, 50).is_empty());
        assert_eq!(run(&mut v, 0.1, 40), [true]);
        assert!(run(&mut v, 0.0, 20).is_empty());
        assert_eq!(run(&mut v, 0.0, 20), [false]);
    }

    #[test]
    fn decodes_samples() {
        assert_eq!(samples(&[0, 0, 0, 0x80]), [0.0, -1.0]);
    }
}
