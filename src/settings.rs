//! User settings persisted to RON at the OS-conventional config location.
//!
//! Settings are loaded on app start and saved when the user closes the
//! settings modal. A missing or invalid config file falls back to defaults
//! — there is no migration story yet, and unrecognized fields are tolerated
//! via `#[serde(default)]` on each top-level field.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    /// Preferred theme. On startup the app applies this; the F10 theme picker
    /// can override transiently without changing this until settings are saved.
    #[serde(default)]
    pub theme: ThemePref,

    /// Render the emoji file-type glyph in front of each row name.
    #[serde(default = "default_true")]
    pub show_glyphs: bool,

    /// Alternating row-background tint for horizontal scanning.
    #[serde(default = "default_true")]
    pub row_striping: bool,

    /// On SFTP back-navigation, kick off a background refresh that replaces
    /// cached entries with fresh server-side data. When false, cached entries
    /// stay until the user manually refreshes (F2).
    #[serde(default = "default_true")]
    pub auto_refresh: bool,

    /// List names that start with a dot. `..` stays either way.
    #[serde(default = "default_true")]
    pub show_hidden: bool,

    /// Fill in every folder size on the active panel.
    #[serde(default)]
    pub auto_sizes: bool,

    /// Saved SFTP host bookmarks. Surfaced in the quick-jump dropdown so a
    /// labeled bookmark expands to (host, initial_path).
    #[serde(default)]
    pub bookmarks: Vec<Bookmark>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub enum ThemePref {
    #[default]
    Dark,
    Light,
    External(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bookmark {
    pub label: String,
    pub host: String,
    pub path: String,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemePref::Dark,
            show_glyphs: true,
            row_striping: true,
            auto_refresh: true,
            show_hidden: true,
            auto_sizes: false,
            bookmarks: Vec::new(),
        }
    }
}

/// Locate the OS-conventional config directory for fileman.
/// Linux: `$XDG_CONFIG_HOME/fileman` or `$HOME/.config/fileman`.
/// macOS: `$HOME/Library/Application Support/fileman`.
/// Windows: `%APPDATA%/fileman`.
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return Some(PathBuf::from(xdg).join("fileman"));
        }
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            return Some(PathBuf::from(home).join(".config").join("fileman"));
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            return Some(
                PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
                    .join("fileman"),
            );
        }
        None
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA")
            && !appdata.is_empty()
        {
            return Some(PathBuf::from(appdata).join("fileman"));
        }
        None
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

pub fn settings_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("settings.ron"))
}

/// Load settings from disk. Returns defaults if the file is missing, empty,
/// or fails to parse.
pub fn load() -> Settings {
    let Some(path) = settings_path() else {
        return Settings::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Settings::default();
    };
    match ron::from_str::<Settings>(&text) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("settings: parse error in {}: {e}", path.display());
            // Preserve the unparseable file rather than letting the next save
            // silently overwrite it — it may hold recoverable bookmarks.
            let backup = path.with_extension("ron.corrupt");
            match std::fs::rename(&path, &backup) {
                Ok(()) => eprintln!("settings: moved corrupt file to {}", backup.display()),
                Err(re) => eprintln!("settings: could not preserve corrupt file: {re}"),
            }
            Settings::default()
        }
    }
}

/// Serialize settings to disk. Creates the config directory if needed. The
/// write is atomic (temp file + fsync + rename) so a crash mid-write can't
/// truncate the settings file, which `load` would then treat as corrupt.
pub fn save(settings: &Settings) -> anyhow::Result<()> {
    use std::io::Write as _;
    let dir = config_dir().ok_or_else(|| anyhow::anyhow!("no config dir available"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("settings.ron");
    let pretty = ron::ser::PrettyConfig::default();
    let text = ron::ser::to_string_pretty(settings, pretty)?;
    let tmp = dir.join("settings.ron.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_fields() {
        let s = Settings {
            show_glyphs: false,
            auto_refresh: false,
            theme: ThemePref::External("solarized".into()),
            bookmarks: vec![Bookmark {
                label: "home".into(),
                host: "example.com".into(),
                path: "/srv".into(),
            }],
            ..Settings::default()
        };
        let text = ron::ser::to_string_pretty(&s, ron::ser::PrettyConfig::default()).unwrap();
        let back: Settings = ron::from_str(&text).unwrap();
        assert!(!back.show_glyphs);
        assert!(!back.auto_refresh);
        assert_eq!(back.bookmarks.len(), 1);
        assert_eq!(back.bookmarks[0].host, "example.com");
        assert!(matches!(back.theme, ThemePref::External(ref n) if n == "solarized"));
    }

    #[test]
    fn garbage_does_not_parse() {
        assert!(ron::from_str::<Settings>("this is not ron {{{").is_err());
        assert!(ron::from_str::<Settings>("").is_err());
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        // A minimal file (as an older/newer version might write) must still load,
        // with absent fields taking their #[serde(default)] values.
        let s: Settings = ron::from_str("(theme: Dark)").unwrap();
        assert!(s.show_glyphs);
        assert!(s.row_striping);
        assert!(s.bookmarks.is_empty());
    }
}
