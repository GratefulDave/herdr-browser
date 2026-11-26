//! Paths, configuration and owner-to-browser state.

use crate::R;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

pub const PLUGIN_ID: &str = "herdr-browser";

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is not set"))
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

static PROFILE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

fn valid_profile_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// List actual named Chrome profiles without creating state or following symlinks.
pub fn profiles() -> R<Vec<String>> {
    let dir = base_dir().join("profiles");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display()).into()),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| valid_profile_name(name)) else { continue };
        let profile = entry.path().join("profile");
        match fs::symlink_metadata(&profile) {
            Ok(metadata) if metadata.file_type().is_dir() => names.push(name.to_owned()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", profile.display()).into()),
        }
    }
    names.sort_unstable();
    Ok(names)
}

pub fn select_profile(name: Option<String>) -> R<()> {
    if let Some(name) = &name {
        if !valid_profile_name(name) {
            return Err("invalid profile name: use 1–64 lowercase letters, digits, '-' or '_'".into());
        }
    }
    PROFILE.set(name).map_err(|_| "profile already selected")?;
    Ok(())
}

pub fn selected_profile() -> Option<&'static str> {
    PROFILE.get().and_then(|name| name.as_deref())
}

pub fn base_dir() -> PathBuf {
    env_dir("XDG_STATE_HOME").unwrap_or_else(|| home().join(".local/state")).join(PLUGIN_ID)
}

pub fn state_dir() -> PathBuf {
    match selected_profile() {
        Some(name) => base_dir().join("profiles").join(name),
        None => base_dir(),
    }
}

pub fn config_dir() -> PathBuf {
    env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config")).join(PLUGIN_ID)
}

pub fn profile_dir() -> PathBuf {
    state_dir().join("profile")
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Optional dedicated Chrome executable, useful for fixture installations.
    pub chrome_path: Option<PathBuf>,
    pub max_chars: Option<usize>,
    /// Live view: terminal cell height / width, if the detected one is off.
    pub cell_aspect: Option<f64>,
    /// Live view: "kitty" or "blocks"; detected when unset.
    pub view_renderer: Option<String>,
    /// Live view in herdr desktop: the screen's scale (2 on a HiDPI screen), when it isn't
    /// detected (Linux).
    pub scale: Option<f64>,
}

pub const DEFAULT_MAX_CHARS: usize = 20_000;

impl Config {
    pub fn load() -> R<Config> {
        let path = config_dir().join("config.toml");
        match fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display()).into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display()).into()),
        }
    }
    pub fn max_chars(&self) -> usize {
        self.max_chars.unwrap_or(DEFAULT_MAX_CHARS)
    }
}


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ViewRecord {
    #[serde(default)]
    pub tab: String,
    #[serde(default)]
    pub space: String,
}

/// A target group is keyed by its root target ID. Closing the root migrates
/// the key and owner mappings to a surviving member under the group lock.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabGroup {
    pub selected: String,
    pub members: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub openers: BTreeMap<String, String>,
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    #[serde(default)]
    pub tabs: BTreeMap<String, String>,
    #[serde(default)]
    pub tab_groups: BTreeMap<String, TabGroup>,
    #[serde(default)]
    pub views: BTreeMap<String, ViewRecord>,
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("state.json")
}

fn lock(dir: &Path) -> R<File> {
    fs::create_dir_all(dir)?;
    let f = File::options().create(true).truncate(false).write(true).open(dir.join("state.lock"))?;
    f.lock()?;
    Ok(f)
}

/// Acquire a process-wide exclusive lock for a caller-selected safe name.
pub fn lock_named(name: &str) -> R<File> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("invalid lock name".into());
    }
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let f = File::options().create(true).truncate(false).write(true).open(dir.join(format!("{name}.lock")))?;
    f.lock()?;
    Ok(f)
}

/// Serialize installation with browser startup across every profile.
pub fn lock_install() -> R<File> {
    let dir = base_dir();
    fs::create_dir_all(&dir)?;
    let file = File::options().create(true).truncate(false).write(true).open(dir.join("install.lock"))?;
    file.lock()?;
    Ok(file)
}

/// Refuse immediately while a manual login owns this profile.
pub fn login_guard() -> R<File> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let file = File::options().create(true).truncate(false).write(true).open(dir.join("login.lock"))?;
    file.try_lock().map_err(|_| "profile is in manual login; close the login window first")?;
    Ok(file)
}

fn read(dir: &Path) -> R<State> {
    match fs::read_to_string(state_path(dir)) {
        Ok(s) => Ok(serde_json::from_str(&s)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e.into()),
    }
}

/// Read-modify-write under an exclusive lock; the write is atomic (tmp + rename).
pub fn update_in<T>(dir: &Path, f: impl FnOnce(&mut State) -> T) -> R<T> {
    let _guard = lock(dir)?;
    let mut s = read(dir)?;
    let out = f(&mut s);
    let tmp = dir.join("state.json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&s)?)?;
    fs::rename(tmp, state_path(dir))?;
    Ok(out)
}

pub fn load_in(dir: &Path) -> R<State> {
    let _guard = lock(dir)?;
    read(dir)
}

pub fn update<T>(f: impl FnOnce(&mut State) -> T) -> R<T> {
    update_in(&state_dir(), f)
}

pub fn load() -> R<State> {
    load_in(&state_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("hb-state-{}", std::process::id()));
        update_in(&dir, |s| {
            s.tabs.insert("w1:p1".into(), "TARGET1".into());
        })
        .unwrap();
        let s = load_in(&dir).unwrap();
        assert_eq!(s.tabs.get("w1:p1").map(String::as_str), Some("TARGET1"));
        fs::remove_dir_all(dir).unwrap();
    }
}
