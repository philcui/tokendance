//! 核心数据类型与共享常量：一条记录、一次扫描的状态、以及时区/时间换算。
//
// 由 `main.rs` 拆分而来，逻辑未改动。

use chrono::{DateTime, FixedOffset};
use serde::Serialize;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const CST_HOURS: i32 = 8;
// in-memory hot window; SQLite keeps the full history
pub(crate) const KEEP_DAYS: i64 = 365;
// mirrors the menubar app's agent list (index = agent id)
pub(crate) const AGENT_NAMES: [&str; 10] = [
    // Gemini CLI was retired upstream (its users are pointed at Antigravity),
    // so its slot is gone: every later index shifts down one and
    // `parser_repair_v6` rewrites the stored rows to match. Antigravity takes
    // the new slot at the end, which keeps that migration a plain -1.
    "WorkBuddy", "Codex", "Claude Code", "Qwen",
    "OpenCode", "pi", "Kimi", "iFlow", "Qoder", "Antigravity",
];

#[derive(Clone, Serialize)]
pub(crate) struct Record {
    pub(crate) a: u8,       // agent id
    pub(crate) m: String,   // model
    pub(crate) t: i64,      // epoch ms
    pub(crate) i: i64,      // input incl. cache
    pub(crate) o: i64,      // output
    pub(crate) c: i64,      // cached-read
    pub(crate) s: String,   // session
    pub(crate) p: String,   // project ("" if none)
}

/// Ceiling for one record's token fields, applied on the way in.
///
/// Not a rounding of anything we display: it is the difference between "this
/// machine's worst day" and "a number no agent can produce". Measured on this
/// machine (29,659 records): the largest single record is **3,126,357** tokens
/// (one call, or one delta of a session total) and the largest *day* is
/// **1,176,446,631**. A single record therefore has its own hard scale, and
/// 10^11 is 85× the heaviest whole day ever recorded here — 32,000× above any
/// real record.
///
/// Why a ceiling at all, if i64 holds 9.2·10^18: the number that reaches us is
/// not always produced by an agent. A hand-edited transcript, a new format where
/// a field means bytes or nanoseconds, or a float that inflates all arrive as
/// "a very large integer", and a single one of them used to make every *sum*
/// afterwards wrap (measured: 5·10^18 twice → `today.t` became
/// −8,446,744,073,709,551,616, and the app crashed on it). 10^11 × 10 million
/// records is still 10^18 < 9.2·10^18, so with this clamp saturating addition is
/// belt-and-braces rather than the last line of defence.
pub(crate) const MAX_RECORD_TOKENS: i64 = 100_000_000_000;

pub(crate) static CLAMP_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Clamp one record's token fields. Returns the record unchanged in every real
/// case; the warning is printed once per process so a corrupt file cannot turn
/// the log into a wall of text.
pub(crate) fn sane(mut r: Record) -> Record {
    let cl = |v: i64| v.clamp(0, MAX_RECORD_TOKENS);
    if r.i != cl(r.i) || r.o != cl(r.o) || r.c != cl(r.c) {
        if !CLAMP_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!("token counts outside 0..{MAX_RECORD_TOKENS} clamped (model {}); \
                       this looks like a corrupt or non-token field, not real usage",
                      r.m);
        }
    }
    r.i = cl(r.i);
    r.o = cl(r.o);
    r.c = cl(r.c);
    r
}

/// 一次"重新扫描"的过程与结果。`log` 只保留最近 300 行。
#[derive(Clone, Default)]
pub(crate) struct ScanRun {
    pub(crate) running: bool,
    pub(crate) started_at: i64,
    pub(crate) finished_at: i64,
    pub(crate) phase: String,
    pub(crate) log: Vec<String>,
    pub(crate) report: Value,
    pub(crate) files_reset: u64,
    pub(crate) records_before: i64,
    pub(crate) records_after: i64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FileState {
    pub(crate) agent: u8,
    pub(crate) off: u64,
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CodexMeta {
    pub(crate) cwd: Option<String>,
    /// Authoritative model name, as recorded on `turn_context` lines.
    pub(crate) model: Option<String>,
    /// Weak fallback: the very oldest builds (0.146.x) only ever wrote
    /// `model_provider` ("deepseek"), never a model name. `serde(default)` keeps
    /// codex_meta blobs written by earlier versions loadable.
    #[serde(default)]
    pub(crate) provider: Option<String>,
}

pub(crate) fn cst() -> FixedOffset {
    FixedOffset::east_opt(CST_HOURS * 3600).unwrap()
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

pub(crate) fn iso_to_ms(ts: &Value) -> Option<i64> {
    match ts {
        Value::Number(n) => {
            let v = n.as_f64()?;
            Some(if v > 1e11 { v as i64 } else { (v * 1000.0) as i64 })
        }
        Value::String(s) => {
            let norm = s.replace('Z', "+00:00");
            DateTime::parse_from_str(&norm, "%Y-%m-%dT%H:%M:%S%.f%z")
                .or_else(|_| DateTime::parse_from_str(&norm, "%Y-%m-%dT%H:%M:%S%z"))
                .ok()
                .map(|d| d.timestamp_millis())
        }
        _ => None,
    }
}
