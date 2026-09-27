//! Saved connections and preferences.
//!
//! Connection metadata lives in `<config>/DuckPlus/connections.json` (folders
//! in `folders.json`); tokens
//! never touch disk — they go to the OS credential store (macOS Keychain,
//! Windows Credential Manager, Secret Service on Linux).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::quack::TlsMode;

const KEYCHAIN_SERVICE: &str = "app.duckplus.quack";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ProfileKind {
    #[default]
    Quack,
    /// A DuckDB database file opened in-process.
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: ProfileKind,
    /// Quack endpoint, or the database file path for [`ProfileKind::Local`].
    pub endpoint: String,
    #[serde(default)]
    pub tls: TlsMode,
    /// Open local files read-only (e.g. while another process holds the lock).
    #[serde(default)]
    pub read_only: bool,
    /// Index into [`crate::theme::TAG_COLORS`].
    #[serde(default)]
    pub color: usize,
    #[serde(default)]
    pub last_used: Option<u64>,
    /// [`Folder::id`] this connection is filed under.
    #[serde(default)]
    pub folder: Option<String>,
    /// Database the workspace focuses on (`USE`) when it opens.
    #[serde(default)]
    pub database: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
}

impl Profile {
    pub fn new(name: String, endpoint: String, tls: TlsMode, color: usize) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            kind: ProfileKind::Quack,
            endpoint,
            tls,
            read_only: false,
            color,
            last_used: None,
            folder: None,
            database: None,
        }
    }

    pub fn local(path: &std::path::Path, read_only: bool) -> Self {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Self {
            kind: ProfileKind::Local,
            read_only,
            ..Self::new(name, path.display().to_string(), TlsMode::Auto, 0)
        }
    }

    pub fn is_local(&self) -> bool {
        self.kind == ProfileKind::Local
    }

    /// Where this connection points, for display: `host:port` or a `~/` path.
    pub fn location(&self) -> String {
        match self.kind {
            ProfileKind::Quack => crate::quack::normalize_endpoint(&self.endpoint)
                .trim_start_matches("quack:")
                .to_string(),
            ProfileKind::Local => tildify(&self.endpoint),
        }
    }

    fn entry(&self) -> Result<keyring::Entry> {
        Ok(keyring::Entry::new(KEYCHAIN_SERVICE, &self.id)?)
    }

    pub fn token(&self) -> Result<String> {
        self.entry()?
            .get_password()
            .context("token not found in keychain")
    }

    pub fn set_token(&self, token: &str) -> Result<()> {
        self.entry()?
            .set_password(token)
            .context("failed to save token to keychain")
    }

    pub fn delete_token(&self) {
        if let Ok(e) = self.entry() {
            let _ = e.delete_credential();
        }
    }
}

/// `/Users/me/data/x.duckdb` → `~/data/x.duckdb`.
pub fn tildify(path: &str) -> String {
    match dirs::home_dir().and_then(|h| {
        std::path::Path::new(path)
            .strip_prefix(&h)
            .ok()
            .map(|p| p.display().to_string())
    }) {
        Some(rest) => format!("~/{rest}"),
        None => path.to_string(),
    }
}

/// `~/x.duckdb` → absolute path.
pub fn expand_tilde(path: &str) -> std::path::PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => std::path::PathBuf::from(path),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    /// Rows fetched per query before truncating.
    pub row_limit: usize,
    /// Rows shown when opening a table from the sidebar.
    pub preview_limit: usize,
    pub editor_font_size: f32,
    /// Schema tree and results grid.
    pub ui_font_size: f32,
    /// Ask before running DROP / DELETE / TRUNCATE.
    pub confirm_destructive: bool,
    pub zebra_rows: bool,
    /// Query log pane expanded under the results.
    pub show_query_log: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            row_limit: 10_000,
            preview_limit: 500,
            editor_font_size: 13.0,
            ui_font_size: 13.0,
            confirm_destructive: true,
            zebra_rows: true,
            show_query_log: false,
        }
    }
}

pub const EDITOR_FONT_SIZES: (f32, f32) = (9., 28.);
pub const UI_FONT_SIZES: (f32, f32) = (10., 20.);

impl Settings {
    /// Step both font sizes (⌘+ / ⌘−), or reset them with `None` (⌘0).
    pub fn zoom(&mut self, delta: Option<f32>) {
        let defaults = Self::default();
        match delta {
            Some(d) => {
                self.editor_font_size =
                    (self.editor_font_size + d).clamp(EDITOR_FONT_SIZES.0, EDITOR_FONT_SIZES.1);
                self.ui_font_size = (self.ui_font_size + d).clamp(UI_FONT_SIZES.0, UI_FONT_SIZES.1);
            }
            None => {
                self.editor_font_size = defaults.editor_font_size;
                self.ui_font_size = defaults.ui_font_size;
            }
        }
    }
}

fn config_dir() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("DuckPlus");
    let _ = fs::create_dir_all(&dir);
    dir
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(name: &str) -> T {
    fs::read(config_dir().join(name))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_json<T: Serialize>(name: &str, value: &T) -> Result<()> {
    let path = config_dir().join(name);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

#[derive(Default)]
pub struct Store {
    pub profiles: Vec<Profile>,
    pub folders: Vec<Folder>,
    pub settings: Settings,
}

impl Store {
    pub fn load() -> Self {
        Self {
            profiles: load_json("connections.json"),
            folders: load_json("folders.json"),
            settings: load_json("settings.json"),
        }
    }

    pub fn save_profiles(&self) {
        if let Err(e) = save_json("connections.json", &self.profiles) {
            eprintln!("duckplus: {e:#}");
        }
    }

    pub fn save_settings(&self) {
        if let Err(e) = save_json("settings.json", &self.settings) {
            eprintln!("duckplus: {e:#}");
        }
    }

    pub fn upsert(&mut self, profile: Profile) {
        match self.profiles.iter_mut().find(|p| p.id == profile.id) {
            Some(p) => *p = profile,
            None => self.profiles.push(profile),
        }
        self.save_profiles();
    }

    pub fn remove(&mut self, id: &str) {
        if let Some(p) = self.profiles.iter().find(|p| p.id == id) {
            p.delete_token();
        }
        self.profiles.retain(|p| p.id != id);
        self.save_profiles();
    }

    pub fn touch(&mut self, id: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        if let Some(p) = self.profiles.iter_mut().find(|p| p.id == id) {
            p.last_used = Some(now);
        }
        self.save_profiles();
    }

    fn save_folders(&self) {
        if let Err(e) = save_json("folders.json", &self.folders) {
            eprintln!("duckplus: {e:#}");
        }
    }

    /// Alphabetical, case-insensitive.
    pub fn sorted_folders(&self) -> Vec<Folder> {
        let mut v = self.folders.clone();
        v.sort_by_key(|f| f.name.to_lowercase());
        v
    }

    pub fn add_folder(&mut self, name: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.folders.push(Folder {
            id: id.clone(),
            name: name.to_string(),
            collapsed: false,
        });
        self.save_folders();
        id
    }

    pub fn rename_folder(&mut self, id: &str, name: &str) {
        if let Some(f) = self.folders.iter_mut().find(|f| f.id == id) {
            f.name = name.to_string();
        }
        self.save_folders();
    }

    pub fn toggle_folder(&mut self, id: &str) {
        if let Some(f) = self.folders.iter_mut().find(|f| f.id == id) {
            f.collapsed = !f.collapsed;
        }
        self.save_folders();
    }

    /// Delete a folder; its connections move back to the top level.
    pub fn delete_folder(&mut self, id: &str) {
        self.folders.retain(|f| f.id != id);
        for p in &mut self.profiles {
            if p.folder.as_deref() == Some(id) {
                p.folder = None;
            }
        }
        self.save_folders();
        self.save_profiles();
    }

    pub fn set_database(&mut self, profile_id: &str, database: Option<String>) {
        if let Some(p) = self.profiles.iter_mut().find(|p| p.id == profile_id) {
            p.database = database;
        }
        self.save_profiles();
    }

    pub fn move_to_folder(&mut self, profile_id: &str, folder: Option<String>) {
        if let Some(p) = self.profiles.iter_mut().find(|p| p.id == profile_id) {
            p.folder = folder;
        }
        self.save_profiles();
    }

    /// Most recently used first.
    pub fn sorted_profiles(&self) -> Vec<Profile> {
        let mut v = self.profiles.clone();
        v.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.name.cmp(&b.name)));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::{Profile, Settings};

    #[test]
    fn zoom_steps_clamps_and_resets() {
        let mut s = Settings::default();
        s.zoom(Some(1.));
        assert_eq!((s.editor_font_size, s.ui_font_size), (14., 14.));
        for _ in 0..30 {
            s.zoom(Some(1.));
        }
        assert_eq!((s.editor_font_size, s.ui_font_size), (28., 20.));
        s.zoom(None);
        assert_eq!((s.editor_font_size, s.ui_font_size), (13., 13.));
    }
    use crate::quack::TlsMode;

    /// Touches the real OS keychain: cargo test -- --ignored keychain
    #[test]
    #[ignore]
    fn keychain_roundtrip() {
        let p = Profile::new("test".into(), "localhost".into(), TlsMode::Auto, 0);
        p.set_token("s3cret").unwrap();
        assert_eq!(p.token().unwrap(), "s3cret");
        p.delete_token();
        assert!(p.token().is_err());
    }
}
