//! 进程内共享状态与它的两个运行时助手（锁、阻塞任务卸载）。
//
// 由 `main.rs` 拆分而来，逻辑未改动。

use crate::*;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

pub(crate) struct AppState {
    pub(crate) records: Mutex<Vec<Record>>,
    /// Bumped on every mutation of `records`; the key the `/api/data` cache is
    /// validated against.
    pub(crate) records_gen: AtomicU64,
    /// `(generation, serialised "records" JSON array)`. See `api_data`.
    pub(crate) data_cache: Mutex<(u64, axum::body::Bytes)>,
    pub(crate) files_state: Mutex<HashMap<String, FileState>>,
    pub(crate) meta: Mutex<HashMap<String, CodexMeta>>,
    // gemini/qwen/pi/kimi: per-file seen message ids (dedup re-appended lines)
    pub(crate) seen: Mutex<HashMap<String, std::collections::HashSet<String>>>,
    // opencode: session_id -> [tokens_input, tokens_output, tokens_cache_read]
    pub(crate) oc_state: Mutex<HashMap<String, Vec<i64>>>,
    // Claude Code project directory -> readable project label (longest common
    // prefix of that project's cwds). See `resolve_project`.
    pub(crate) proj_roots: Mutex<HashMap<String, String>>,
    /// 手动"重新扫描"的运行状态：阶段、逐行日志、最终报告。
    /// 扫描本身在后台跑（约 10 秒，含一次三层发现），页面每 0.7 秒取一次进度。
    pub(crate) scan: Arc<Mutex<ScanRun>>,
    pub(crate) latest_write_ago_ms: AtomicU64,
    pub(crate) tx: broadcast::Sender<Vec<Record>>,
    pub(crate) dashboard: Vec<u8>,
    pub(crate) settings_html: Vec<u8>,
    pub(crate) about_html: Vec<u8>,
    pub(crate) sources_html: Vec<u8>,
    /// Vendored third-party assets served under `/vendor/…`. Kept in memory
    /// like the pages above: they are small, read once, and the dashboard needs
    /// one of them on every load.
    pub(crate) assets: HashMap<String, Vec<u8>>,
    // shared UI preferences (lang / theme) — single source of truth for web + app
    pub(crate) prefs: Mutex<Value>,
    pub(crate) store: Arc<Store>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run a slow, purely synchronous job somewhere other than a runtime worker.
///
/// Blocking a worker is not a local slowdown here: tokio owns the accept loop on
/// one of those threads, so a handler that blocks inside a syscall takes the
/// *whole* server down with it. Measured, not theorised — a single
/// `/api/registry/update` pointing at an unreachable host (10.255.255.1) froze
/// every other endpoint, `/api/registry` included, and it stayed frozen after
/// the caller gave up: `/api/registry` answered 200 before the call and timed
/// out for the next 90 s and beyond, until the process was restarted. The same
/// applies to the filesystem scans, which is why they are offloaded too.
///
/// `None` means the job panicked; callers degrade to an empty result rather than
/// propagating a panic into the response path.
pub(crate) async fn offload<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

// ---------- routes ----------

#[derive(Clone)]
pub(crate) struct Shared(pub(crate) Arc<AppState>);
