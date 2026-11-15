//! Settings saved between runs, shared by the window and the command-line mode.
//!
//! eframe keeps them in `app.ron` in the app's data folder (on Linux
//! `~/.local/share/clipcram/`), as a map from key to a RON-encoded value. Each part has its
//! own key, so a part an older or newer version can't read resets on its own.

use crate::edit::Edit;
use crate::encode::{EncodeSettings, SizePreset};
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub const APP_ID: &str = "clipcram";
pub const KEY_SETTINGS: &str = "settings";
pub const KEY_ENCODE: &str = "encode_settings";
pub const KEY_QUEUE: &str = "queue";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemePref {
    #[default]
    System,
    Dark,
    Light,
}

/// A named set of encoding choices, size limit included.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    pub settings: EncodeSettings,
    pub size_preset: SizePreset,
    pub custom_mib: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Persisted {
    pub default_output_dir: String,
    pub size_preset: SizePreset,
    pub custom_mib: f64,
    pub volume: f32,
    pub muted: bool,
    pub last_add_dir: Option<String>,

    /// Copy instead of re-encoding when a clip already fits and plays on Discord.
    pub copy_if_fits: bool,
    /// Score each result against the source after encoding.
    pub measure_quality: bool,
    /// Put each finished file on the clipboard, ready to paste into Discord.
    pub copy_to_clipboard: bool,
    /// Desktop notification when the queue finishes.
    pub notify_when_done: bool,
    /// Bring back the queue (with trims and edits) after closing the app.
    pub restore_queue: bool,
    pub presets: Vec<Preset>,

    /// Folder to watch for new clips (an OBS or ShadowPlay replay folder).
    pub watch_dir: String,
    pub watch_enabled: bool,
    /// Encode new clips from the watched folder as soon as they appear.
    pub watch_auto_encode: bool,

    /// Discord webhook that finished clips are posted to.
    pub webhook_url: String,
    pub webhook_enabled: bool,

    pub check_updates: bool,
    pub theme: ThemePref,
}

impl Default for Persisted {
    fn default() -> Self {
        Persisted {
            default_output_dir: util::default_videos_dir(),
            size_preset: SizePreset::Discord10,
            custom_mib: 9.5,
            volume: 1.0,
            muted: false,
            last_add_dir: None,
            copy_if_fits: true,
            measure_quality: true,
            copy_to_clipboard: false,
            notify_when_done: true,
            restore_queue: true,
            presets: Vec::new(),
            watch_dir: String::new(),
            watch_enabled: false,
            watch_auto_encode: false,
            webhook_url: String::new(),
            webhook_enabled: false,
            check_updates: true,
            theme: ThemePref::System,
        }
    }
}

impl Persisted {
    pub fn target_mib(&self) -> f64 {
        self.size_preset.target_mib(self.custom_mib)
    }

    pub fn target_bytes(&self) -> u64 {
        (self.target_mib() * 1024.0 * 1024.0) as u64
    }
}

/// One queued video as saved between runs. Media info is read again on startup.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedItem {
    pub path: PathBuf,
    pub start: f64,
    pub end: f64,
    pub settings: EncodeSettings,
    pub edit: Edit,
    pub output_dir: String,
    pub output_name: String,
    pub custom_name: bool,
    /// Finished output, so a finished video still shows as done.
    pub last_output: Option<PathBuf>,
}

impl Default for SavedItem {
    fn default() -> Self {
        SavedItem {
            path: PathBuf::new(),
            start: 0.0,
            end: 0.0,
            settings: EncodeSettings::default(),
            edit: Edit::default(),
            output_dir: String::new(),
            output_name: String::new(),
            custom_name: false,
            last_output: None,
        }
    }
}

/// Reads the window's saved settings from disk, for the command-line mode.
pub fn load_from_disk() -> (Persisted, EncodeSettings) {
    let map: HashMap<String, String> = eframe::storage_dir(APP_ID)
        .and_then(|dir| std::fs::read_to_string(dir.join("app.ron")).ok())
        .and_then(|text| ron::from_str(&text).ok())
        .unwrap_or_default();
    let persisted = map.get(KEY_SETTINGS).and_then(|v| ron::from_str(v).ok()).unwrap_or_default();
    let encode = map.get(KEY_ENCODE).and_then(|v| ron::from_str(v).ok()).unwrap_or_default();
    (persisted, encode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_settings_files_still_load() {
        // What version 1.0 saved: none of the new fields.
        let old = r#"(default_output_dir:"/videos",size_preset:Nitro50,custom_mib:9.5,volume:0.5,muted:true,last_add_dir:None)"#;
        let p: Persisted = ron::from_str(old).unwrap();
        assert_eq!(p.default_output_dir, "/videos");
        assert_eq!(p.size_preset, SizePreset::Nitro50);
        assert!(p.copy_if_fits && p.measure_quality && p.restore_queue);
        assert!(p.presets.is_empty());
    }

    #[test]
    fn queue_round_trip() {
        let item = SavedItem { path: "/a/b.mkv".into(), start: 1.0, end: 9.0, ..SavedItem::default() };
        let text = ron::to_string(&vec![item.clone()]).unwrap();
        let back: Vec<SavedItem> = ron::from_str(&text).unwrap();
        assert_eq!(back[0].path, item.path);
        assert_eq!(back[0].end, 9.0);
    }
}
