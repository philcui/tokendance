//! 用户自己加的采集源：配置文件、路径校验、增删改查的存储层。
//
// 由 `main.rs` 拆分而来，逻辑未改动。

use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ---------- user-added data sources ----------
//
// The point of this section: a user with a tool we have never heard of should be
// able to add it without us shipping code. Three questions have to be answered,
// and the answers are what the config file records:
//
//   1. where is the data?      `path`  (absolute, or `~`, with `*` / `**`)
//   2. what shape is it?       `format` (one we know) or `fields` (a mapping)
//   3. does it parse?          `/api/custom/validate` answers before saving
//
// Everything else — file counts, byte counts, offsets, dedupe, the UI row — is
// the same machinery the built-in sources use, because they enter the scan
// through the same door (`enumerate_files`).

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CustomSource {
    pub id: String,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub path: String,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub fields: HashMap<String, String>,
}

pub(crate) fn yes() -> bool { true }

#[derive(Clone, Serialize, Deserialize, Default)]
pub(crate) struct CustomFile {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub sources: Vec<CustomSource>,
    /// Ids the user removed by hand. Auto-discovery must not resurrect something
    /// they deleted, or "remove" would only last until the next scan.
    #[serde(default)]
    pub removed: Vec<String>,
    /// Set when a source was found by `discover_once` rather than typed in, so
    /// the UI can say so.
    #[serde(default)]
    pub discovered: Vec<String>,
}

pub(crate) fn one() -> u32 { 1 }

impl CustomSource {
    /// Built-in sources own ids 0..10; user sources start at 10 and keep their
    /// slot by position in the file, so removing one renumbers the ones after it
    /// — the same trap `parser_repair_v6` dealt with, handled here by rebuilding
    /// custom rows from scratch whenever the set changes (see `api_custom_post`).
    pub(crate) fn agent_id(&self) -> u8 { 10 + custom_index(&self.id) as u8 }
}

pub(crate) fn custom_config_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".tokendance/sources.json")
}

pub(crate) fn load_custom() -> CustomFile {
    std::fs::read_to_string(custom_config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub(crate) fn save_custom(cfg: &CustomFile) -> std::io::Result<()> {
    let p = custom_config_path();
    if let Some(dir) = p.parent() { let _ = std::fs::create_dir_all(dir); }
    std::fs::write(&p, serde_json::to_string_pretty(cfg).unwrap_or_default())
}

pub(crate) fn custom_index(id: &str) -> usize {
    load_custom().sources.iter().position(|c| c.id == id).unwrap_or(0)
}

/// `~` → `$HOME`, and nothing else: a user path we cannot resolve is a bug we
/// want to see, not a path we silently guess at.
pub(crate) fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        format!("{}/{rest}", std::env::var("HOME").unwrap_or_default())
    } else { p.to_string() }
}

/// Reject the shapes that would make the config ambiguous or dangerous.
pub(crate) fn validate_path(p: &str) -> Result<(), String> {
    if p.trim().is_empty() { return Err("path is empty".into()); }
    if p.contains("..") { return Err("path must not contain `..`".into()); }
    if !(p.starts_with('/') || p.starts_with("~/")) {
        return Err("path must be absolute or start with `~/`".into());
    }
    // Refuse before touching the disk: `exists()` on one of these is already an
    // access the system has to adjudicate, and the honest answer is "not ours".
    if private_path(&expand_tilde(p)) {
        return Err("that is a macOS privacy store (contacts, mail, messages, …) — TokenDance never reads it".into());
    }
    let glob_start = p.find(['*', '?', '[']).unwrap_or(p.len());
    let base = &p[..glob_start];
    let base = base.rsplit_once('/').map(|(d, _)| d).unwrap_or(base);
    if !Path::new(&expand_tilde(base)).exists() {
        return Err(format!("directory does not exist: {base}"));
    }
    Ok(())
}
