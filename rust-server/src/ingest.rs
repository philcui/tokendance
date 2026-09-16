//! 增量采集：盯住文件偏移量，只解析新增的部分；含 opencode 的基线快照。
//
// 由 `main.rs` 拆分而来，逻辑未改动。

use crate::*;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

/// Locations that are known to hold agent data but that we deliberately do not
/// parse (yet). They are reported by `/api/sources` so their presence is never
/// silently ignored — `reason` is a key the UI turns into a sentence.
///
/// Audited on 2026-09-15 (STATUS ㉘): Cursor's chat store has 3792 bubbles but
/// only 8 with non-zero usage; the Trae family's `database.db` is not plain
/// SQLite (encrypted); CodeBuddy's session store holds a single row; the Qoder
/// desktop store reports `tokenCountsAvailable=false`; and the Codex directory
/// beside Application Support is a Chromium profile, not usage.
pub(crate) const KNOWN_UNPARSED: &[(&str, &str, &str)] = &[
    ("Cursor", "Library/Application Support/Cursor/User/globalStorage/state.vscdb", "sparse"),
    ("Trae CN", "Library/Application Support/Trae CN/ModularData/ai-agent/database.db", "encrypted"),
    ("Trae", "Library/Application Support/Trae/ModularData/ai-agent/database.db", "encrypted"),
    ("TRAE SOLO CN", "Library/Application Support/TRAE SOLO CN/ModularData/ai-agent/database.db", "encrypted"),
    ("CodeBuddy CN", "Library/Application Support/CodeBuddy CN/codebuddy-sessions.vscdb", "empty_store"),
    ("Qoder (桌面版)", "Library/Application Support/com.qodercn.app.stable/main.sqlite", "no_token_counts"),
    ("Windsurf", "Library/Application Support/Windsurf/User/globalStorage/state.vscdb", "sparse"),
    ("Codex (桌面版数据目录)", "Library/Application Support/Codex", "browser_profile"),
];

/// The glob patterns we look for, per agent — the single owner of "where the
/// data lives". `enumerate_files()` globs them; `/api/sources` reports them, so
/// the UI can say *where* it looked instead of silently showing zero.
///
/// Patterns are absolute (HOME / env overrides are already substituted) and the
/// literal prefix before the first `*` is that source's base directory.
pub(crate) fn source_patterns() -> Vec<(u8, Vec<String>)> {
    let home = std::env::var("HOME").unwrap_or_default();
    let home = PathBuf::from(home);
    let h = home.display();
    // Agents whose data dir can be relocated (official env support) — a user
    // who sets these would otherwise silently show 0 records:
    //   Codex: CODEX_HOME          (default ~/.codex)
    //   Claude Code: CLAUDE_CONFIG_DIR (default ~/.claude)
    //   OpenCode: XDG_DATA_HOME    (default ~/.local/share)
    let codex_base = std::env::var("CODEX_HOME").unwrap_or_else(|_| format!("{h}/.codex"));
    let claude_base = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_else(|_| format!("{h}/.claude"));
    // OpenCode honors XDG_DATA_HOME — resolved inside tick_opencode
    vec![
        (0, vec![format!("{h}/.workbuddy/projects/*/*.jsonl")]),
        (1, vec![format!("{codex_base}/sessions/*/*/*/rollout-*.jsonl")]),
        (2, {
            let mut v = vec![format!("{claude_base}/projects/*/*.jsonl")];
            v.push(format!("{claude_base}/projects/*/*/*.jsonl"));
            v.push(format!("{claude_base}/projects/*/*/*/*.jsonl"));
            v
        }),
        // Qwen Code (fork of gemini-cli): ~/.qwen/tmp/<hash>/chats/session-*.jsonl
        (3, vec![format!("{h}/.qwen/tmp/*/chats/session-*.jsonl")]),
        (4, vec![]), // OpenCode lives in SQLite, handled by tick_opencode
        // pi (badlogic/pi-mono): ~/.pi/agent/sessions/<enc-cwd>/<session>.jsonl
        (5, vec![format!("{h}/.pi/agent/sessions/*/*.jsonl")]),
        // Kimi CLI (legacy kimi-cli): ~/.kimi/sessions/<md5-wd>/<session>/context.jsonl
        // Kimi Code CLI: ~/.kimi-code/sessions/<wdkey>/<session>/agents/main/wire.jsonl
        (6, {
            let mut v = vec![format!("{h}/.kimi/sessions/*/*/context.jsonl")];
            v.push(format!("{h}/.kimi-code/sessions/*/*/agents/*/wire.jsonl"));
            v
        }),
        // iFlow CLI (gemini-cli fork): ~/.iflow/projects/<proj>/session-*.jsonl
        (7, vec![format!("{h}/.iflow/projects/*/*.jsonl")]),
        // Qoder CLI: ~/.qoder/projects/<proj>/<session>.jsonl (+ logs layout)
        (8, {
            let mut v = vec![format!("{h}/.qoder/projects/*/*.jsonl")];
            v.push(format!("{h}/.qoder/logs/sessions/**/*.jsonl"));
            v
        }),
        // Antigravity (Google's successor to Gemini CLI): one SQLite database
        // per conversation under ~/.gemini/antigravity/. These are databases,
        // not transcripts, so they are read by `tick_antigravity` rather than
        // line-parsed — the pattern is here so the UI can *detect* them.
        (9, vec![format!("{h}/.gemini/antigravity/conversations/*.db")]),
    ]
}

pub(crate) fn enumerate_files() -> Vec<(u8, Vec<PathBuf>)> {
    let mut all: Vec<(u8, Vec<PathBuf>)> = source_patterns()
        .into_iter()
        .map(|(agent, pats)| {
            let files = pats
                .iter()
                .flat_map(|p| glob::glob(p).into_iter().flatten().filter_map(Result::ok))
                .collect();
            (agent, files)
        })
        .collect();
    // User-added sources ride the same pipeline: their patterns are globbed here
    // and their lines are parsed by `custom_line()` (agent ids ≥ 10).
    for c in load_custom().sources.iter().filter(|c| c.enabled) {
        let pats = expand_tilde(&c.path);
        let files: Vec<PathBuf> = glob::glob(&pats).into_iter().flatten().filter_map(Result::ok).collect();
        all.push((c.agent_id(), files));
    }
    all
}

// ---------- incremental tick ----------

// Third element: Claude Code project roots that changed this tick
// (project dir → label), so the caller can persist them.
type TickOut = (Vec<Record>, Vec<(String, u8, u64)>, Vec<(String, String)>);

pub(crate) fn tick(
    files_state: &mut HashMap<String, FileState>,
    meta: &mut HashMap<String, CodexMeta>,
    seen: &mut HashMap<String, std::collections::HashSet<String>>,
    write_ago: &AtomicU64,
    proj_roots: &mut HashMap<String, String>,
) -> TickOut {
    let mut new = Vec::new();
    // Read once per tick, not once per line: `parse_line` needs the user's own
    // source definitions, and a single scan can touch hundreds of thousands of
    // lines.
    let custom_cfg = load_custom();
    let mut dirty: Vec<(String, u8, u64)> = Vec::new();
    let mut roots_changed: Vec<(String, String)> = Vec::new();
    let mut max_mtime_ms = 0i64;
    for (agent, paths) in enumerate_files() {
        for fp in paths {
            let Ok(md) = std::fs::metadata(&fp) else { continue };
            let size = md.len();
            let mtime_ms = md.modified().ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64).unwrap_or(0);
            if mtime_ms > max_mtime_ms {
                max_mtime_ms = mtime_ms;
            }
            let key = fp.to_string_lossy().to_string();
            let st = files_state.entry(key.clone()).or_insert(FileState { agent, off: 0 });
            let mut off = st.off;
            if size == off {
                continue;
            }
            if size < off {
                off = 0; // truncated / rewritten
                seen.remove(&key);
            }
            let mut newoff = off;
            {
                let Ok(f) = std::fs::File::open(&fp) else { continue };
                // Streamed line by line instead of read_to_end: a single rollout
                // file on this machine is 253 MB, and slurping it plus the split
                // slices would spike memory into the hundreds of MB for no gain.
                let mut br = BufReader::with_capacity(1 << 20, f);
                if br.seek(SeekFrom::Start(off)).is_err() {
                    continue;
                }
                let mut line: Vec<u8> = Vec::with_capacity(4096);
                loop {
                    line.clear();
                    match br.read_until(b'\n', &mut line) {
                        Ok(0) => break, // EOF
                        Ok(n) => {
                            // A trailing line without '\n' is still being written —
                            // leave `off` before it so we re-read it next tick.
                            if line.last() != Some(&b'\n') {
                                break;
                            }
                            newoff += n as u64;
                            if line.last() == Some(&b'\n') {
                                line.pop();
                            }
                            if line.last() == Some(&b'\r') {
                                line.pop();
                            }
                            if line.is_empty() {
                                continue;
                            }
                            let bline: &[u8] = &line;
                            // cheap pre-filter before JSON parse
                            let keep = match agent {
                                1 => find(bline, b"\"token_count\"")
                                    || find(bline, b"\"session_meta\"")
                                    || find(bline, b"\"turn_context\"")
                                    || find(bline, b"\"world_state\"")
                                    || find(bline, b"\"thread_settings_applied\""),
                                3 | 7 => find(bline, b"\"tokens\""),   // Qwen, iFlow
                                // pi carries the real cwd only on the session
                                // header line, which has no usage — filtering on
                                // "usage" alone meant that line never reached the
                                // parser, so the project stayed the escaped
                                // directory name (`--Users-alice-proj--`).
                                5 => find(bline, b"\"usage\"") || find(bline, b"\"cwd\""),   // pi
                                // User-added sources are small and their keys are
                                // unknown by definition (`input`/`output` need not
                                // appear next to the word "usage"), so they get no
                                // pre-filter — the JSON parse is the filter.
                                n if n >= 10 => true,
                                _ => find(bline, b"\"usage\""),
                            };
                            if !keep {
                                continue;
                            }
                            let Ok(d) = serde_json::from_slice::<Value>(bline) else { continue };
                            if let Some(mut rec) = parse_line(agent, &d, &fp, meta, mtime_ms, &custom_cfg) {
                                // Claude Code: the parser hands back the raw cwd,
                                // which is folded into a per-project root here so
                                // one session can never span several "projects".
                                if agent == 2 {
                                    let enc = claude_enc_dir(&fp)
                                        .unwrap_or_else(|| rec.p.clone());
                                    rec.p = resolve_project(
                                        proj_roots, &mut roots_changed, &enc, &rec.p)
                                        .unwrap_or(enc);
                                }
                                if rec.t > 0 && !is_noise(&rec) {
                                    // dedup re-appended / tree-branch messages by (file, message id)
                                    if matches!(agent, 3 | 4 | 6 | 7) {
                                        let mid = d.get("id").and_then(Value::as_str).unwrap_or("");
                                        if !mid.is_empty() {
                                            let ids = seen.entry(key.clone()).or_default();
                                            if !ids.insert(mid.to_string()) {
                                                continue;
                                            }
                                        }
                                    }
                                    new.push(rec);
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            if st.off != newoff {
                st.off = newoff;
                dirty.push((key, st.agent, newoff));
            }
        }
    }
    if max_mtime_ms > 0 {
        let ago_ms = (now_ms() - max_mtime_ms).max(0) as u64;
        write_ago.store(ago_ms, Ordering::Relaxed);
    }
    (new, dirty, roots_changed)
}

pub(crate) fn find(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// A record that moved no tokens at all is not a call.
///
/// Codex periodically emits a "context window snapshot" (`token_count` with
/// `last_token_usage` = all zeros and `total_tokens` holding the context length).
/// Those carried no usage, yet each one bumped the visible call count — 44 of
/// them sat in the store before this filter existed. Measured: no other agent
/// produces an all-zero record, and records with only *one* of the three
/// counters at zero are legitimate (Claude Code emits input-only calls), so the
/// test is deliberately "all three".
pub(crate) fn is_noise(r: &Record) -> bool {
    r.i == 0 && r.o == 0 && r.c == 0
}

// OpenCode: ~/.local/share/opencode/opencode.db (SQLite). The session table stores
// CUMULATIVE tokens per session — we diff against oc_state to emit incremental events.
// On first contact ever (no persisted baseline) we only establish it: historical
// totals must NOT be re-reported after a server restart.
//
// oc_state value is [input, output, cached]; a 2-element array written by an older
// build still parses (missing cached reads as 0).
pub(crate) fn tick_opencode(oc_state: &mut HashMap<String, Vec<i64>>, initial: bool) -> (Vec<Record>, bool) {
    let mut new = Vec::new();
    let mut changed = false;
    let home = std::env::var("HOME").unwrap_or_default();
    let data_dir = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
    let db = PathBuf::from(data_dir).join("opencode/opencode.db");
    if !db.exists() {
        return (new, changed);
    }
    // Copy the database **and its -wal/-shm sidecars** before opening it.
    //
    // Measured on this machine: opencode.db is 4 KB while 1.1 MB of the data —
    // including the schema — sits in opencode.db-wal, so anything that opens the
    // main file alone sees an empty database. Copying also means we read a
    // consistent snapshot instead of a file another process is mid-write on, and
    // never contend for its lock. (Cline's importer does exactly this — copying
    // "-wal" and "-shm" next to the database into a temp dir — which is where I
    // noticed the pattern.)
    let tmp = copy_sqlite_with_sidecars(&db);
    let read_path = tmp.as_ref().map(|d| d.join("opencode.db")).unwrap_or_else(|| db.clone());
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(conn) = rusqlite::Connection::open_with_flags(&read_path, flags) else { return (new, changed) };
    // defensive column check — schema may vary between versions
    let cols: std::collections::HashSet<String> = (|| -> rusqlite::Result<_> {
        let mut stmt = conn.prepare("PRAGMA table_info(session)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        Ok(rows.filter_map(Result::ok).collect())
    })().unwrap_or_default();
    for need in ["id", "tokens_input", "tokens_output"] {
        if !cols.contains(need) {
            return (new, changed);
        }
    }
    let has_model = cols.contains("model");
    let has_updated = cols.contains("time_updated");
    // Cache reads and writes live in their own columns and used to be dropped
    // entirely, so every OpenCode record reported a 0% hit rate.
    let cache_sel = if cols.contains("tokens_cache_read") { "tokens_cache_read" } else { "0" };
    let cwrite_sel = if cols.contains("tokens_cache_write") { "tokens_cache_write" } else { "0" };
    let model_sel = if has_model { "model" } else { "'opencode'" };
    let ts_sel = if has_updated { "time_updated" } else { "time_created" };
    // The session row also carries the working directory it ran in, so OpenCode
    // gets the same project name as everyone else instead of the literal string
    // "opencode" for every session it ever had.
    let dir_sel = if cols.contains("directory") { "directory" } else { "''" };
    let sql = format!(
        "SELECT id, {model_sel}, {ts_sel}, tokens_input, tokens_output, {cache_sel}, {cwrite_sel}, {dir_sel} \
         FROM session WHERE tokens_input > 0 OR tokens_output > 0 OR {cache_sel} > 0"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else { return (new, changed) };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1).unwrap_or(None).unwrap_or_default(),
            r.get::<_, i64>(2).unwrap_or(0),
            r.get::<_, i64>(3).unwrap_or(0),
            r.get::<_, i64>(4).unwrap_or(0),
            r.get::<_, i64>(5).unwrap_or(0),
            r.get::<_, i64>(6).unwrap_or(0),
            r.get::<_, Option<String>>(7).unwrap_or(None).unwrap_or_default(),
        ))
    });
    let Ok(rows) = rows else { return (new, changed) };
    for row in rows {
        let Ok((sid, model_raw, ts, tin, tout, tcache, twrite, directory)) = row else { continue };
        // `tokens_input` is the *non-cached* prompt: OpenCode's own message
        // payloads carry `{"total":11662,"input":9817,"output":53,"cache":{"read":
        // 1792,"write":0}}` — i.e. total = input + output + cache read + cache
        // write, so input excludes the cache. Taking `tokens_input` alone
        // understated input by the cache amount (here 15%), and the earlier
        // "fold it in only when cache exceeds input" heuristic left every
        // normally-cached session short. Input now means the same thing as for
        // every other source: everything the model was fed.
        let i_now = tin.saturating_add(tcache).saturating_add(twrite);
        let last = oc_state.get(&sid).map(|v| v.as_slice()).unwrap_or(&[]);
        let l = |n: usize| last.get(n).copied().unwrap_or(0);
        let d_in = i_now - l(0);
        let d_out = tout - l(1);
        let d_c = tcache - l(2);
        if d_in <= 0 && d_out <= 0 && d_c <= 0 {
            continue;
        }
        oc_state.insert(sid.clone(), vec![i_now, tout, tcache]);
        changed = true;
        if initial {
            continue; // baseline only — do not re-emit historical totals
        }
        // model column may be a JSON string '{"modelID":...}' or plain text
        let model = match serde_json::from_str::<Value>(&model_raw) {
            Ok(Value::Object(o)) => ["modelID", "id", "model"].iter()
                .find_map(|k| o.get(*k).and_then(Value::as_str))
                .unwrap_or("opencode").to_string(),
            Ok(Value::String(s)) => s,
            _ => if model_raw.is_empty() { "opencode".to_string() } else { model_raw },
        };
        let ms = if ts > 1_000_000_000_000 { ts } else if ts > 1_000_000_000 { ts * 1000 } else { now_ms() };
        new.push(Record {
            a: 4,   // OpenCode's new index after Gemini was retired
            m: model,
            t: ms,
            i: d_in.max(0),
            o: d_out.max(0),
            c: d_c.max(0),
            s: short_id(&sid),
            p: if directory.is_empty() { "opencode".to_string() }
               else { home_short(&strip_file_url(&directory)) },
        });
    }
    // Unlink the snapshot now — the tick runs every second, so a copy left
    // behind each time would be a slow leak. Unlinking while the connection
    // still holds the file open is fine on Unix: SQLite keeps reading the inode
    // it opened, and the space returns when the handles close.
    if let Some(d) = &tmp {
        let _ = std::fs::remove_dir_all(d);
    }
    (new, changed)
}

/// Copy `db`, `db-wal` and `db-shm` into a fresh temp directory and return it.
/// `None` when even the main file cannot be copied — the caller then falls back
/// to reading the original in place.
pub(crate) fn copy_sqlite_with_sidecars(db: &Path) -> Option<PathBuf> {
    let stem = db.file_name()?.to_string_lossy().to_string();
    let dir = std::env::temp_dir().join(format!("tokendance-sqlite-{}-{}",
                                                std::process::id(), now_ms()));
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::copy(db, dir.join(&stem)).ok()?;
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", db.display()));
        if side.exists() {
            let _ = std::fs::copy(&side, dir.join(format!("{stem}{suffix}")));
        }
    }
    Some(dir)
}

pub(crate) fn oc_state_json(oc: &HashMap<String, Vec<i64>>) -> String {
    serde_json::to_string(oc).unwrap_or_else(|_| "{}".into())
}

pub(crate) fn oc_state_parse(s: &str) -> HashMap<String, Vec<i64>> {
    serde_json::from_str(s).unwrap_or_default()
}

