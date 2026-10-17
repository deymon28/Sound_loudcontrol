//! User configuration: ducking rules and global settings, stored as TOML.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Selects audio sessions by their owning application.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppMatcher {
    /// Process executable name, e.g. `Discord.exe` (case-insensitive, `.exe` optional).
    Process(String),
    /// The Windows "System Sounds" session.
    SystemSounds,
    /// Every session that is not a trigger of the same rule. Only meaningful as a target.
    AllOthers,
}

impl AppMatcher {
    pub fn label(&self) -> String {
        match self {
            AppMatcher::Process(name) => name.clone(),
            AppMatcher::SystemSounds => "System sounds".to_owned(),
            AppMatcher::AllOthers => "All other apps".to_owned(),
        }
    }
}

/// Normalizes an executable name for matching: lowercase, without a trailing `.exe`.
pub fn normalize_exe(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    lower.strip_suffix(".exe").map(str::to_owned).unwrap_or(lower)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DuckMode {
    /// Lower targets to a fixed level whenever a trigger is active.
    Fixed,
    /// Ducking depth follows trigger loudness: louder trigger, deeper duck.
    Proportional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FadeCurve {
    /// Fades move at a constant rate in decibels, which sounds even to the ear.
    Decibel,
    /// Fades move at a constant rate in linear volume.
    Linear,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    /// Runtime identity used to keep per-rule state across edits; not persisted.
    #[serde(skip)]
    pub uid: u64,
    pub name: String,
    pub enabled: bool,
    /// Sound from any of these sessions activates the rule.
    pub triggers: Vec<AppMatcher>,
    /// Sessions whose volume is lowered while the rule is active.
    pub targets: Vec<AppMatcher>,
    /// Trigger peak level (dBFS) above which the trigger counts as active.
    pub threshold_db: f32,
    /// Sound must stay above the threshold this long before ducking starts; filters clicks.
    pub min_active_ms: f32,
    pub mode: DuckMode,
    /// Target volume while fully ducked, as a fraction of the user's volume (0.0 = mute).
    pub duck_level: f32,
    /// Proportional mode: trigger level range above the threshold that maps to full ducking.
    pub range_db: f32,
    pub curve: FadeCurve,
    /// Fade-down time.
    pub attack_ms: f32,
    /// Time the duck is held after the trigger goes quiet; bridges pauses between words.
    pub hold_ms: f32,
    /// Fade-up time.
    pub release_ms: f32,
}

impl Default for Rule {
    fn default() -> Self {
        Self {
            uid: next_uid(),
            name: "New rule".to_owned(),
            enabled: true,
            triggers: Vec::new(),
            targets: Vec::new(),
            threshold_db: -45.0,
            min_active_ms: 40.0,
            mode: DuckMode::Fixed,
            duck_level: 0.25,
            range_db: 24.0,
            curve: FadeCurve::Decibel,
            attack_ms: 120.0,
            hold_ms: 900.0,
            release_ms: 1200.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Ducking is globally suspended; targets fade back to their normal volume.
    pub paused: bool,
    /// Meter polling interval while any trigger application is running.
    pub tick_ms: u32,
    /// Global hotkey toggling pause, e.g. `Ctrl+Alt+M`. Empty disables it.
    pub pause_hotkey: String,
    pub start_minimized: bool,
    pub close_to_tray: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            paused: false,
            tick_ms: 10,
            pause_hotkey: "Ctrl+Alt+M".to_owned(),
            start_minimized: false,
            close_to_tray: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub settings: Settings,
    pub rules: Vec<Rule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            rules: vec![Rule {
                name: "Discord over music".to_owned(),
                triggers: vec![AppMatcher::Process("Discord.exe".to_owned())],
                targets: vec![AppMatcher::Process("YouTube Music.exe".to_owned())],
                ..Rule::default()
            }],
        }
    }
}

impl Config {
    /// Clamps values that came from a hand-edited file into safe ranges.
    pub fn sanitize(&mut self) {
        let s = &mut self.settings;
        s.tick_ms = s.tick_ms.clamp(5, 100);
        for r in &mut self.rules {
            r.threshold_db = finite_or(r.threshold_db, -45.0).clamp(-80.0, 0.0);
            r.min_active_ms = finite_or(r.min_active_ms, 40.0).clamp(0.0, 2000.0);
            r.duck_level = finite_or(r.duck_level, 0.25).clamp(0.0, 1.0);
            r.range_db = finite_or(r.range_db, 24.0).clamp(3.0, 60.0);
            r.attack_ms = finite_or(r.attack_ms, 120.0).clamp(0.0, 5000.0);
            r.hold_ms = finite_or(r.hold_ms, 900.0).clamp(0.0, 10000.0);
            r.release_ms = finite_or(r.release_ms, 1200.0).clamp(0.0, 10000.0);
            // A trigger cannot be "everything else".
            r.triggers.retain(|m| *m != AppMatcher::AllOthers);
        }
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let mut cfg: Config = toml::from_str(&text).map_err(|e| e.to_string())?;
        cfg.sanitize();
        for r in &mut cfg.rules {
            r.uid = next_uid();
        }
        Ok(cfg)
    }

    /// Writes atomically: a temp file is renamed over the target so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        write_atomic(path, text.as_bytes())
    }
}

/// Process-wide unique rule id.
pub fn next_uid() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn finite_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() { v } else { fallback }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// `%APPDATA%\LoudControl`, or the directory given by `LOUDCONTROL_HOME`.
pub fn app_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LOUDCONTROL_HOME") {
        return PathBuf::from(dir);
    }
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("LoudControl")
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_through_toml() {
        let mut cfg = Config::default();
        cfg.rules[0].targets.push(AppMatcher::AllOthers);
        cfg.rules[0].triggers.push(AppMatcher::SystemSounds);
        let text = toml::to_string_pretty(&cfg).unwrap();
        let mut back: Config = toml::from_str(&text).unwrap();
        back.rules[0].uid = cfg.rules[0].uid;
        assert_eq!(cfg, back);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let cfg: Config = toml::from_str("[[rules]]\nname = \"x\"\n").unwrap();
        assert_eq!(cfg.rules[0].hold_ms, Rule::default().hold_ms);
        assert_eq!(cfg.settings, Settings::default());
    }

    #[test]
    fn sanitize_clamps_and_drops_invalid_trigger() {
        let mut cfg = Config::default();
        cfg.rules[0].duck_level = 7.0;
        cfg.rules[0].attack_ms = f32::NAN;
        cfg.rules[0].triggers.push(AppMatcher::AllOthers);
        cfg.sanitize();
        assert_eq!(cfg.rules[0].duck_level, 1.0);
        assert_eq!(cfg.rules[0].attack_ms, 120.0);
        assert!(!cfg.rules[0].triggers.contains(&AppMatcher::AllOthers));
    }

    #[test]
    fn exe_names_normalize() {
        assert_eq!(normalize_exe(" Discord.EXE "), "discord");
        assert_eq!(normalize_exe("YouTube Music"), "youtube music");
    }
}
