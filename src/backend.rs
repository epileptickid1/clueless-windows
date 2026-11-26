//! The model providers share the UI's turn and event interfaces.

use std::sync::mpsc::Sender;

use anyhow::Result;
use serde_json::Value;

use crate::antigravity::Antigravity;
use crate::claude::Claude;
use crate::codex::Codex;
use crate::config::{Provider, Settings};
use crate::event::Event;

pub enum Backend {
    Claude(Claude),
    Antigravity(Antigravity),
    Codex(Codex),
}

impl Backend {
    pub fn spawn(settings: &Settings, resume: Option<&str>, generation: u64, events: Sender<Event>) -> Result<Self> {
        let model = settings.model_id();
        match settings.provider {
            Provider::Claude => Claude::spawn(model, resume, generation, events).map(Self::Claude),
            Provider::Antigravity => Antigravity::spawn(model, resume, generation, events).map(Self::Antigravity),
            Provider::Codex => Codex::spawn(model, resume, settings.codex_fast, generation, events).map(Self::Codex),
        }
    }

    pub fn send(&self, content: Vec<Value>) -> Result<()> {
        match self {
            Self::Claude(client) => client.send(content),
            Self::Antigravity(client) => client.send(content),
            Self::Codex(client) => client.send(content),
        }
    }
}
