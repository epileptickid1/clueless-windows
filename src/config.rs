//! Paths and persisted user settings.

use std::path::PathBuf;

use serde_json::{Value, json};

pub const MODELS: &[&str] = &["sonnet", "haiku", "opus"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Provider {
    #[default]
    Claude,
    Antigravity,
    Codex,
}

impl Provider {
    pub const ALL: [Self; 3] = [Self::Claude, Self::Antigravity, Self::Codex];

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Antigravity => "Antigravity",
            Self::Codex => "Codex",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Antigravity => "antigravity",
            Self::Codex => "codex",
        }
    }

    pub fn index(self) -> u32 {
        Self::ALL.iter().position(|&provider| provider == self).unwrap() as u32
    }
}

/// (label, Deepgram language code)
pub const LANGUAGES: &[(&str, &str)] = &[
    ("Multilingual", "multi"),
    ("English", "en"),
    ("Українська", "uk"),
    ("Русский", "ru"),
    ("Deutsch", "de"),
    ("Polski", "pl"),
    ("Español", "es"),
    ("Français", "fr"),
];

#[cfg(not(windows))]
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(not(windows))]
fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

#[cfg(windows)]
fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("AppData/Local")))
        .unwrap_or_else(std::env::temp_dir)
        .join("Clueless")
}

pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    return local_app_data();
    #[cfg(not(windows))]
    xdg("XDG_DATA_HOME", ".local/share").join("clueless")
}

fn config_dir() -> PathBuf {
    #[cfg(windows)]
    return local_app_data();
    #[cfg(not(windows))]
    xdg("XDG_CONFIG_HOME", ".config").join("clueless")
}

pub fn deepgram_key_path() -> PathBuf {
    config_dir().join("deepgram.key")
}

/// `DEEPGRAM_API_KEY`, otherwise the contents of the key file.
pub fn deepgram_key() -> Option<String> {
    let usable = |k: String| Some(k.trim().to_string()).filter(|k| !k.is_empty());
    std::env::var("DEEPGRAM_API_KEY")
        .ok()
        .and_then(usable)
        .or_else(|| std::fs::read_to_string(deepgram_key_path()).ok().and_then(usable))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Background opacity of the window, 0.2 ..= 1.0.
    pub opacity: f64,
    pub auto_answer: bool,
    pub microphone: bool,
    pub keep_above: bool,
    pub model: u32,
    pub provider: Provider,
    pub antigravity_model: String,
    pub codex_model: String,
    pub codex_fast: bool,
    pub language: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            opacity: 0.72,
            auto_answer: true,
            microphone: false,
            keep_above: true,
            model: 0,
            provider: Provider::Claude,
            antigravity_model: String::new(),
            codex_model: String::new(),
            codex_fast: false,
            language: 0,
        }
    }
}

impl Settings {
    pub fn model_id(&self) -> &str {
        match self.provider {
            Provider::Claude => MODELS.get(self.model as usize).copied().unwrap_or(MODELS[0]),
            Provider::Antigravity => &self.antigravity_model,
            Provider::Codex => &self.codex_model,
        }
    }

    pub fn select_model(&mut self, id: String) {
        match self.provider {
            Provider::Claude => {
                if let Some(index) = MODELS.iter().position(|&model| model == id.as_str()) {
                    self.model = index as u32;
                }
            }
            Provider::Antigravity => self.antigravity_model = id,
            Provider::Codex => self.codex_model = id,
        }
    }

    pub fn from_json(v: &Value) -> Self {
        let d = Settings::default();
        let flag = |k: &str, d: bool| v[k].as_bool().unwrap_or(d);
        let index = |k: &str, len: usize| v[k].as_u64().filter(|&i| (i as usize) < len).unwrap_or(0) as u32;
        Settings {
            opacity: v["opacity"].as_f64().unwrap_or(d.opacity).clamp(0.2, 1.0),
            auto_answer: flag("auto_answer", d.auto_answer),
            microphone: flag("microphone", d.microphone),
            keep_above: flag("keep_above", d.keep_above),
            model: index("model", MODELS.len()),
            provider: Provider::ALL
                .into_iter()
                .find(|provider| v["provider"].as_str() == Some(provider.id()))
                .unwrap_or_default(),
            antigravity_model: v["antigravity_model"].as_str().unwrap_or_default().to_string(),
            codex_model: v["codex_model"].as_str().unwrap_or_default().to_string(),
            codex_fast: flag("codex_fast", d.codex_fast),
            language: index("language", LANGUAGES.len()),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "opacity": self.opacity,
            "auto_answer": self.auto_answer,
            "microphone": self.microphone,
            "keep_above": self.keep_above,
            "model": self.model,
            "provider": self.provider.id(),
            "antigravity_model": self.antigravity_model,
            "codex_model": self.codex_model,
            "codex_fast": self.codex_fast,
            "language": self.language,
        })
    }

    pub fn load() -> Self {
        std::fs::read_to_string(config_dir().join("settings.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .map(|v| Settings::from_json(&v))
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = config_dir().join("settings.json");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, self.to_json().to_string()) {
            eprintln!("clueless: cannot save settings to {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_legacy_claude_selection_and_persists_codex_fast() {
        let mut settings = Settings::from_json(&json!({"model": 2}));
        assert_eq!(settings.provider, Provider::Claude);
        assert_eq!(settings.model_id(), "opus");
        assert!(!settings.codex_fast);

        settings.provider = Provider::Codex;
        settings.select_model("codex-model".into());
        settings.codex_fast = true;
        let restored = Settings::from_json(&settings.to_json());
        assert_eq!(restored, settings);
        assert_eq!(restored.model_id(), "codex-model");
        assert_eq!(restored.model, 2);
    }
}
