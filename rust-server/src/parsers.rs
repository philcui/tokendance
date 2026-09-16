//! 把各家 agent 的转录文件读成我们的 `Record`——一行一个解析器。
//
// 由 `main.rs` 拆分而来，逻辑未改动。

use crate::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Dotted lookup: `usage.prompt_tokens` walks nested objects.
pub(crate) fn dig<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() { return None; }
    let mut cur = v;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

pub(crate) fn dig_i64(v: &Value, path: &str) -> Option<i64> {
    dig(v, path).and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
}

/// Keys we accept without any configuration — the "self-describing" format. A
/// tool that logs `{"ts":…, "model":…, "input":…, "output":…}` lines needs no
/// mapping at all, which is the common case for someone's own program.
pub(crate) const SELF_IN: &[&str] = &["input", "input_tokens", "prompt_tokens", "promptTokens", "in", "inputTokens"];
pub(crate) const SELF_OUT: &[&str] = &["output", "output_tokens", "completion_tokens", "completionTokens", "out", "outputTokens"];
pub(crate) const SELF_CACHED: &[&str] = &["cached", "cache_read", "cached_tokens", "cachedTokens", "cacheRead", "cache_read_input_tokens"];
pub(crate) const SELF_CACHE_WRITE: &[&str] = &["cache_write", "cache_write_input_tokens", "cacheWrite", "cache_creation_input_tokens"];
pub(crate) const SELF_TOTAL: &[&str] = &["total_tokens", "totalTokens", "total"];
pub(crate) const SELF_TIME: &[&str] = &["timestamp", "ts", "time", "created_at", "createdAt"];
pub(crate) const SELF_MODEL: &[&str] = &["model", "modelId", "model_id", "model_name"];
pub(crate) const SELF_SESSION: &[&str] = &["session_id", "sessionId", "session", "conversation_id", "thread_id"];
pub(crate) const SELF_PROJECT: &[&str] = &["cwd", "project", "project_path", "directory", "workspace"];

/// Recursive usage sniffing: walk the object (bounded depth) looking for a dict
/// that carries a numeric input-ish key, and take output/cached/total from that
/// same dict.
///
/// Why recursive: our own formats nest the numbers two levels down —
/// Codex `payload.info.last_token_usage.*`, Claude `message.usage.*`,
/// WorkBuddy `providerData.usage.*`. A one-level sniff (which is what the first
/// version of this did) sees none of them, so a *blind* scan of a machine with
/// those tools finds nothing. Measured: 1 source with one level, 6 with recursion
/// (STATUS ㊱).
pub(crate) fn find_usage(node: &Value, depth: usize) -> Option<(i64, i64, i64)> {
    if depth > 5 { return None; }
    match node {
        Value::Object(o) => {
            if let Some(raw_in) = SELF_IN.iter().find_map(|k| o.get(*k).and_then(Value::as_i64)) {
                let out = SELF_OUT.iter().find_map(|k| o.get(*k).and_then(Value::as_i64)).unwrap_or(0);
                // OpenAI puts the cached count one level deeper than the totals
                // (`usage.prompt_tokens_details.cached_tokens`); not reading it
                // reports a 0% hit rate for every OpenAI-shaped log.
                let cached = SELF_CACHED.iter().find_map(|k| o.get(*k).and_then(Value::as_i64))
                    .or_else(|| ["prompt_tokens_details", "input_tokens_details",
                                 "cache_details", "details"].iter()
                        .filter_map(|c| o.get(*c))
                        .find_map(|inner| SELF_CACHED.iter()
                            .find_map(|k| inner.get(*k).and_then(Value::as_i64))))
                    .unwrap_or(0);
                let write = SELF_CACHE_WRITE.iter().find_map(|k| o.get(*k).and_then(Value::as_i64)).unwrap_or(0);
                let total = SELF_TOTAL.iter().find_map(|k| o.get(*k).and_then(Value::as_i64));
                // Same normalisation as the flat path: prefer the tool's own
                // total, otherwise decide from the relationship between input
                // and cached so that `cached <= input` always holds.
                let input = match total {
                    Some(t) if t >= out && t >= cached => (t - out).max(cached),
                    _ if raw_in >= cached => raw_in.saturating_add(write),
                    _ => raw_in.saturating_add(cached).saturating_add(write),
                };
                if input + out + cached > 0 { return Some((input, out, cached)); }
            }
            o.values().find_map(|v| find_usage(v, depth + 1))
        }
        Value::Array(a) => a.iter().take(8).find_map(|v| find_usage(v, depth + 1)),
        _ => None,
    }
}

/// First key in `names` that carries an integer, searched at the top level and
/// one level down (`usage.*`, `message.usage.*`) — the two shapes that cover
/// essentially every CLI that logs usage.
pub(crate) fn pick_str(v: &Value, names: &[&str]) -> Option<String> {
    for n in names {
        if let Some(s) = v.get(*n).and_then(Value::as_str) { return Some(s.to_string()); }
    }
    for outer in ["message", "meta"] {
        if let Some(inner) = v.get(outer) {
            for n in names {
                if let Some(s) = inner.get(*n).and_then(Value::as_str) { return Some(s.to_string()); }
            }
        }
    }
    None
}

/// One line of a user-added source. `fields` is empty for `jsonl-usage`; for
/// `custom` every path comes from the config.
pub(crate) fn custom_line(id: u8, d: &Value, fp: &Path, mtime: i64, src: &CustomSource) -> Option<Record> {
    let (i, o, c) = if src.format == "custom" {
        let f = &src.fields;
        let g = |k: &str| f.get(k).and_then(|p| dig_i64(d, p)).unwrap_or(0);
        (g("input"), g("output"), g("cached"))
    } else {
        // Self-describing logs disagree about whether `input` already contains
        // the cache reads: pi and Goose write them as parallel fields
        // (their own total is input + output + cacheRead + cacheWrite), while
        // OpenAI-shaped logs make `cached_tokens` a *subset* of `input_tokens`.
        // Getting this wrong is what produced the 667.5% hit rate in ㉛, so the
        // rule below prefers the tool's own arithmetic and never inflates:
        //
        //   1. if it reports a total, trust it: input = total − output;
        //   2. else if input >= cached, treat input as inclusive (OpenAI style);
        //   3. else treat them as additive (pi / Goose style).
        //
        // All three keep `cached <= input`, which is what makes the displayed
        // hit rate a percentage.
        // Recursive sniff — see `find_usage`. Our own formats hide the numbers
        // two levels down (`payload.info.last_token_usage`, `message.usage`,
        // `providerData.usage`), so a flat sniff cannot read a log from a tool we
        // have never seen.
        find_usage(d, 0)?
    };
    if i + o + c == 0 { return None; }
    let ts = if src.format == "custom" {
        src.fields.get("time").and_then(|p| dig(d, p).and_then(|v| iso_to_ms(v).or_else(|| v.as_i64())))
    } else {
        pick_str(d, SELF_TIME).and_then(|s| iso_to_ms(&Value::String(s.clone())))
    }.unwrap_or(mtime);
    let model = if src.format == "custom" {
        src.fields.get("model").and_then(|p| dig(d, p).and_then(Value::as_str)).unwrap_or(&src.name).to_string()
    } else {
        pick_str(d, SELF_MODEL).unwrap_or_else(|| src.name.clone())
    };
    let session = if src.format == "custom" {
        src.fields.get("session").and_then(|p| dig(d, p).and_then(Value::as_str)).map(short_id)
    } else {
        pick_str(d, SELF_SESSION).map(|s| short_id(&s))
    }.unwrap_or_else(|| {
        // No session in the line: the file itself is the session. `short_id` is
        // for uuids and turns `llm_request.1.jsonl` into the unreadable
        // `llm_request.`; a user-facing log deserves its actual name.
        let stem = fp.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        stem.chars().take(24).collect()
    });
    let project = if src.format == "custom" {
        src.fields.get("project").and_then(|p| dig(d, p).and_then(Value::as_str)).map(home_short)
    } else {
        pick_str(d, SELF_PROJECT).map(|s| home_short(&s))
    }.unwrap_or_else(|| src.name.clone());
    Some(Record { a: id, m: model, t: ts, i, o, c, s: session, p: project })
}

// ---------- Antigravity (one SQLite database per conversation) ----------
//
// Where it keeps things: `~/.gemini/antigravity/conversations/<uuid>.db`, a
// SQLite file per conversation with `gen_metadata` rows holding the model call
// metadata as protobuf blobs. Its session index next door
// (`conversation_summaries.db`) carries titles and timestamps only — no usage.
//
// What we know about the blob: inside top-level field 1 there is a repeated
// message whose fields 1..11 include three small integers that move with the
// conversation (`{1: 1318}` alone on the first call, `{1: 1318, 2: 18583,
// 3: 284}` later, and `{1: 1318, 2: 2439, 3: 328, 5: 16267}` after that), plus
// request ids (`bot-<uuid>`, `sessionID`) — i.e. it looks like a telemetry/usage
// record. The binary's own protobuf descriptors say `output_tokens` is field 3
// and `prompt_tokens` is field 5, which is *consistent* with 3 = output and
// 5 = cached prompt (2 = non-cached prompt).
//
// Consistent is not confirmed, so nothing here is counted: the numbers are
// reported as `candidate` and the source shows as "detected, usage unconfirmed".
// One controlled message in Antigravity (paste a file of known size) settles the
// mapping by showing which field moves — see STATUS ㉜.

pub(crate) fn pb_varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v = 0u64; let mut shift = 0;
    loop {
        let c = *b.get(*i)?;
        *i += 1;
        v |= ((c & 0x7F) as u64) << shift;
        if c & 0x80 == 0 { return Some(v); }
        shift += 7;
        if shift > 63 { return None; }
    }
}

/// Walk one level and return the raw bytes of the first occurrence of `want`.
pub(crate) fn pb_field(b: &[u8], want: u32) -> Option<Vec<u8>> {
    let mut i = 0usize;
    while i < b.len() {
        let key = pb_varint(b, &mut i)?;
        let (field, wt) = ((key >> 3) as u32, (key & 7) as u32);
        match wt {
            0 => { pb_varint(b, &mut i)?; }
            2 => {
                let len = pb_varint(b, &mut i)? as usize;
                let end = i.checked_add(len)?.min(b.len());
                let chunk = b[i..end].to_vec();
                if field == want { return Some(chunk); }
                i = end;
            }
            5 => i += 4,
            1 => i += 8,
            _ => return None,
        }
    }
    None
}

/// Walk one level and return the value of the first *varint* field `want`.
/// (`pb_field` returns length-delimited payloads; usage counters are varints.)
pub(crate) fn pb_varint_field(b: &[u8], want: u32) -> Option<u64> {
    let mut i = 0usize;
    while i < b.len() {
        let key = pb_varint(b, &mut i)?;
        let (field, wt) = ((key >> 3) as u32, (key & 7) as u32);
        match wt {
            0 => {
                let v = pb_varint(b, &mut i)?;
                if field == want { return Some(v); }
            }
            2 => {
                let len = pb_varint(b, &mut i)? as usize;
                i = i.checked_add(len)?.min(b.len());
            }
            5 => i += 4,
            1 => i += 8,
            _ => return None,
        }
    }
    None
}

/// Candidate (prompt, cached, output) for one generation, per the field-number
/// hypothesis above. `None` when the blob does not carry that message.
pub(crate) fn antigravity_usage(blob: &[u8]) -> Option<(i64, i64, i64)> {
    let one = pb_field(blob, 1)?;
    let four = pb_field(&one, 4)?;
    let get = |n: u32| pb_varint_field(&four, n).unwrap_or(0) as i64;
    let (prompt, cached, output) = (get(2), get(5), get(3));
    if prompt + cached + output == 0 { return None; }
    Some((prompt, cached, output))
}

/// Detect Antigravity conversations and report the candidate numbers without
/// counting them. Cheap by construction: at most 40 databases per call.
pub(crate) fn antigravity_peek() -> Value {
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = PathBuf::from(home).join(".gemini/antigravity/conversations");
    let (mut files, mut bytes, mut gens) = (0u32, 0u64, 0u32);
    let (mut prompt, mut cached, mut output) = (0i64, 0i64, 0i64);
    let (mut newest_ms, mut newest_id) = (0i64, String::new());
    let mut opened = 0;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("db") { continue; }
            files += 1;
            let meta = std::fs::metadata(&path).ok();
            bytes += meta.as_ref().map(|m| m.len()).unwrap_or(0);
            if let Some(t) = meta.and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
            {
                if t > newest_ms {
                    newest_ms = t;
                    newest_id = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                }
            }
            if opened >= 40 { continue; }
            opened += 1;
            let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let Ok(conn) = rusqlite::Connection::open_with_flags(&path, flags) else { continue };
            let Ok(mut stmt) = conn.prepare("SELECT data FROM gen_metadata") else { continue };
            let Ok(rows) = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0)) else { continue };
            for blob in rows.flatten() {
                if let Some((p, c, o)) = antigravity_usage(&blob) {
                    gens += 1; prompt += p; cached += c; output += o;
                }
            }
        }
    }
    json!({
        "files": files, "bytes": bytes, "generations": gens,
        "candidate": {"prompt": prompt, "cached": cached, "output": output},
        "newest": {"id": newest_id, "t": newest_ms},
    })
}

// ---------- per-agent line parsers ----------

pub(crate) fn get_i64(v: &Value, keys: &[&str]) -> i64 {
    for k in keys {
        if let Some(n) = v.get(*k).and_then(Value::as_i64) {
            return n;
        }
        if let Some(n) = v.get(*k).and_then(Value::as_f64) {
            return n as i64;
        }
    }
    0
}

pub(crate) fn wb_line(d: &Value) -> Option<(i64, i64, i64, String, i64)> {
    // returns (inp, outp, cached, model, ms)
    let pd = d.get("providerData").filter(|v| !v.is_null());
    let msg = d.get("message").filter(|v| !v.is_null());
    let u = pd.and_then(|p| p.get("usage")).or_else(|| msg.and_then(|m| m.get("usage")));
    let u = u.filter(|v| v.is_object())?;
    let inp = get_i64(u, &["inputTokens", "input_tokens"]);
    let outp = get_i64(u, &["outputTokens", "output_tokens"]);
    let mut cached = 0i64;
    match u.get("inputTokensDetails") {
        Some(Value::Array(arr)) => {
            for x in arr {
                cached += get_i64(x, &["cached_tokens"]);
            }
        }
        Some(Value::Object(_)) => cached += get_i64(u, &["cached_tokens"]),
        _ => {}
    }
    cached += get_i64(u, &["cache_read_input_tokens"]);
    let model = pd.and_then(|p| p.get("model")).and_then(Value::as_str)
        .or_else(|| msg.and_then(|m| m.get("model")).and_then(Value::as_str))
        .unwrap_or("unknown").to_string();
    let ms = iso_to_ms(d.get("timestamp")?)?;
    Some((inp, outp, cached, model, ms))
}

/// Short, collision-free session key derived from a session identifier.
///
/// Agent transcripts are named after UUIDs, and taking a *prefix* of one is
/// unsafe: UUIDv7's leading 48 bits are a millisecond timestamp, so every session
/// started in the same millisecond shares its first 8 hex characters. Measured on
/// this machine with `first8`: Codex collapsed 42 sessions into 33 keys (one key
/// covered 8 files), Claude Code 9 into 8. The trailing bits are random, so the
/// last 12 hex characters are unique for every session observed (55 WorkBuddy /
/// 42 Codex / 9 Claude Code, zero collisions).
pub(crate) fn short_id(raw: &str) -> String {
    let stem = raw.trim_end_matches(".jsonl");
    let dehex: String = stem.chars().filter(|c| *c != '-').collect();
    let uuid_like = stem.contains('-') && dehex.len() >= 12
        && dehex.chars().all(|c| c.is_ascii_hexdigit());
    if uuid_like {
        return dehex[dehex.len() - 12..].to_string();
    }
    // not a uuid (odd or legacy naming) — keep a readable prefix instead
    stem.chars().take(12).collect()
}

// ---------- project naming ----------

pub(crate) fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// `/Users/alice/Desktop/proj` → `~/Desktop/proj`. A path that is not under the
/// home directory is returned unchanged.
pub(crate) fn home_short(p: &str) -> String {
    let h = home_dir();
    if h.is_empty() {
        return p.to_string();
    }
    if p == h {
        return "~".to_string();
    }
    match p.strip_prefix(&h) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => p.to_string(),
    }
}

/// A `cwd` is a path, except when it is spelled as a URL.
///
/// Codex writes a plain absolute path in its session meta, but the same field
/// shows up as `file:///Users/…` in other records the app produces (1367 of them
/// in one rollout on this machine). Left alone, `home_short` cannot recognise
/// the home prefix, so the project would be labelled `file:///Users/…` — a
/// second name for a directory that already has one.
pub(crate) fn strip_file_url(p: &str) -> String {
    p.strip_prefix("file://").unwrap_or(p).to_string()
}

/// Longest common *component-wise* prefix of two paths.
///
/// `/a/b/c` + `/a/b/cc` → `/a/b` (a plain string prefix test would keep `/a/b/c`
/// against `/a/b/cc` and invent a directory that does not exist).
/// `/a/b` + `/a/b/c` → `/a/b`.
pub(crate) fn path_lcp(a: &str, b: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut ai = a.split('/');
    let mut bi = b.split('/');
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) if x == y => out.push(x),
            _ => break,
        }
    }
    let joined = out.join("/");
    if out.is_empty() { String::new() } else { joined }
}

/// The Claude Code project directory name: the path component directly below
/// `projects/`.
///
/// Claude Code names a project directory after the cwd with **every
/// non-alphanumeric character replaced by `-`**, which is lossy — a Chinese
/// directory name becomes a run of dashes that no longer identifies anything.
/// Worse, its transcripts live at four different depths
/// (`projects/<enc>/*.jsonl`, `.../<session>/*.jsonl`,
/// `.../<session>/subagents/*.jsonl`), so the *immediate* parent directory is
/// not the project: for a subagent transcript it is literally `subagents`.
/// Searching from the right finds the real `projects` directory even if the
/// home path happens to contain a component called `projects`.
pub(crate) fn claude_enc_dir(fp: &Path) -> Option<String> {
    let comps: Vec<String> = fp
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    let idx = comps.iter().rposition(|c| c == "projects")?;
    comps.get(idx + 1).cloned()
}

/// Fold one Claude Code record's `cwd` into the project root for its project
/// directory.
///
/// A single session records several cwds: the directory it was launched from
/// plus every subdirectory the agent walked into. Measured on this machine, one
/// project holds `~/Desktop/proj/中文名` (343 lines),
/// `~/Desktop/proj/中文名/frontend` (455) and `.../backend` (433) — so using
/// the per-line cwd as the project name splits *one session* across three
/// "projects".
///
/// The root is therefore the longest common prefix of every cwd seen for that
/// project directory, accumulated in `roots` and persisted by the caller.
/// Persisting matters: an incremental tick only ever reads the *tail* of a
/// transcript, so without the remembered root it would start again from whatever
/// subdirectory happens to be in those lines and re-fragment the project.
///
/// Returns the label to use, or `None` when nothing is known yet (the caller
/// then falls back to the raw directory name).
pub(crate) fn resolve_project(
    roots: &mut HashMap<String, String>,
    changed: &mut Vec<(String, String)>,
    enc: &str,
    cwd: &str,
) -> Option<String> {
    if cwd.is_empty() {
        return roots.get(enc).cloned();
    }
    let cand = home_short(cwd);
    match roots.get(enc).cloned() {
        None => {
            roots.insert(enc.to_string(), cand.clone());
            changed.push((enc.to_string(), cand.clone()));
            Some(cand)
        }
        Some(cur) => {
            let lcp = path_lcp(&cur, &cand);
            // An empty prefix means the cwds share nothing below the root (e.g.
            // only "/" matched) — keep the known-good root rather than relabel
            // the project with an empty string.
            if !lcp.is_empty() && lcp != cur {
                roots.insert(enc.to_string(), lcp.clone());
                changed.push((enc.to_string(), lcp.clone()));
                Some(lcp)
            } else {
                Some(cur)
            }
        }
    }
}

/// Derive a session key from a Codex rollout file name.
///
/// `rollout-2026-09-05T14-00-19-01a07027-4d56-7493-bc60-2edc86a9b7aa.jsonl`
///   → the trailing uuid → `bc60-2edc86a9b7aa`'s last 12 hex chars
///
/// Handing the whole file name to `short_id` would not work: the date and clock
/// digits are hex characters too, so they would dominate the tail.
pub(crate) fn codex_session_id(base: &str) -> String {
    let s = base.strip_prefix("rollout-").unwrap_or(base);
    let s = s.trim_end_matches(".jsonl");
    // Continuing a thread starts a *new* rollout file named after the file it
    // continues, plus the new id: `rollout-<ts>-<uuid>_<uuid>.jsonl` (seen from
    // the Codex desktop app). The conversation is the original uuid, so that is
    // what the session keys on — everything after the first `_` is discarded.
    // Not doing this fell through to `short_id`'s non-uuid branch, which handed
    // back a *prefix* of the original uuid ("01a07027-4d5") and so filed one
    // conversation under two sessions.
    let s = s.split('_').next().unwrap_or(s);
    // s == "<date>T<HH>-<MM>-<SS>-<uuid>" — the uuid starts after the 4th dash
    match s.split_once('T') {
        Some((_date, rest)) => {
            let uuid = rest.splitn(4, '-').nth(3).unwrap_or(rest);
            short_id(uuid)
        }
        None => short_id(s),
    }
}

pub(crate) fn codex_line(d: &Value, meta_key: &str, meta: &mut HashMap<String, CodexMeta>) -> Option<Record> {
    let m = meta.entry(meta_key.to_string()).or_default();
    let pl = d.get("payload").filter(|v| !v.is_null());
    let top_type = d.get("type").and_then(Value::as_str);
    let pl_type = pl.and_then(|p| p.get("type")).and_then(Value::as_str);
    if top_type == Some("session_meta") || pl_type == Some("session_meta") {
        if m.cwd.is_none() {
            let cwd = pl.and_then(|p| p.get("cwd")).and_then(Value::as_str)
                .or_else(|| d.get("cwd").and_then(Value::as_str));
            m.cwd = cwd.map(|s| s.to_string());
        }
        if m.provider.is_none() {
            m.provider = pl.and_then(|p| p.get("model_provider"))
                .and_then(Value::as_str).map(|s| s.to_string());
        }
        if m.model.is_none() {
            m.model = pl.and_then(|p| p.get("model"))
                .and_then(Value::as_str).map(|s| s.to_string());
        }
    }
    // `turn_context` is the authoritative carrier of the model name — but the
    // marker lives on the TOP-LEVEL object, and its payload has no `type` field
    // at all. Verified across all 42 rollout files: top=turn_context appears 879
    // times while payload.type=turn_context appears 0 times, so the previous
    // `pl_type == Some("turn_context")` test could never fire. That single wrong
    // field is why all 20,330 Codex records were stored as model "unknown".
    if top_type == Some("turn_context") {
        if let Some(v) = pl.and_then(|p| p.get("model")).and_then(Value::as_str) {
            // overwrite rather than fill-once: a session can switch models mid-way
            m.model = Some(v.to_string());
        }
    }
    // Alternate carriers of the same value, seen in older builds.
    if m.model.is_none() {
        if top_type == Some("world_state") {
            m.model = pl.and_then(|p| p.get("state")).and_then(|s| s.get("model"))
                .and_then(Value::as_str).map(|s| s.to_string());
        }
        if m.model.is_none() && pl_type == Some("thread_settings_applied") {
            m.model = pl.and_then(|p| p.get("thread_settings")).and_then(|s| s.get("model"))
                .and_then(Value::as_str).map(|s| s.to_string());
        }
    }
    if pl_type != Some("token_count") {
        return None;
    }
    let info = pl.and_then(|p| p.get("info")).filter(|v| !v.is_null())?;
    let lu = info.get("last_token_usage").filter(|v| v.is_object())?;
    let ms = iso_to_ms(d.get("timestamp").unwrap_or(&Value::Null));
    // One directory, one project name — for every agent.
    //
    // This used to keep only the *last two path components*
    // (`/Users/alice/WorkBuddy/2024-01-15-09-30-00/example-repo` →
    // `2024-01-15-09-30-00/example-repo`) while pi and Claude Code stored the
    // home-shortened full path for the very same directory. So a project worked
    // on by two agents appeared twice in the dashboard's project filter, and a
    // pair like `~/a/example-repo` / `~/b/example-repo` could also collide into
    // one label. The rule is now the same everywhere: `home_short(cwd)`, with
    // the source name as the fallback when a transcript has no cwd at all.
    let proj = match m.cwd.as_deref() {
        Some(cwd) => home_short(&strip_file_url(cwd)),
        None => "codex".to_string(),
    };
    let base = meta_key.split('|').nth(1).unwrap_or("");
    let skey = codex_session_id(base);
    Some(Record {
        a: 1,
        m: m.model.clone().or_else(|| m.provider.clone()).unwrap_or_else(|| "codex".into()),
        t: ms?,
        i: get_i64(lu, &["input_tokens"]),
        o: get_i64(lu, &["output_tokens"]),
        c: get_i64(lu, &["cached_input_tokens"]),
        s: skey,
        p: proj,
    })
}

pub(crate) fn claude_line(d: &Value) -> Option<(i64, i64, i64, String, Option<i64>, String)> {
    // (inp, outp, cached, model, ms, session)
    let msg = d.get("message").filter(|v| !v.is_null())?;
    let u = msg.get("usage").filter(|v| v.is_object())?;
    let base_in = get_i64(u, &["input_tokens"]);
    let cached = get_i64(u, &["cache_read_input_tokens"]);
    let cache_w = get_i64(u, &["cache_creation_input_tokens"]);
    let outp = get_i64(u, &["output_tokens"]);
    if base_in + cached + cache_w + outp == 0 {
        return None;
    }
    let ms = match d.get("timestamp") {
        Some(Value::String(_)) => iso_to_ms(d.get("timestamp").unwrap()),
        _ => None,
    };
    let model = msg.get("model").and_then(Value::as_str).unwrap_or("unknown").to_string();
    let sess = d.get("sessionId").and_then(Value::as_str).unwrap_or("cc");
    let sess = short_id(sess);
    Some((base_in + cached + cache_w, outp, cached, model, ms, sess))
}

// Gemini CLI / Qwen Code session line: {"id","type":"gemini","timestamp","model","tokens":{...}}
// cached is included in input; $set metadata patches and user lines are skipped.
pub(crate) fn gemini_qwen_line(agent: u8, d: &Value, fp: &Path) -> Option<Record> {
    if d.get("$set").is_some() || d.get("$rewindTo").is_some() {
        return None;
    }
    if d.get("type").and_then(Value::as_str) != Some("gemini") {
        return None;
    }
    let tk = d.get("tokens").filter(|v| v.is_object())?;
    let input = get_i64(tk, &["input"]);
    let output = get_i64(tk, &["output"]);
    let cached = get_i64(tk, &["cached"]);
    if input + output == 0 {
        return None;
    }
    let model = d.get("model").and_then(Value::as_str)
        .unwrap_or("qwen").to_string();   // Qwen Code is the only fork left here
    let ms = iso_to_ms(d.get("timestamp").unwrap_or(&Value::Null))?;
    let chats = fp.parent()?;
    let hash_dir = chats.parent()?.file_name()?.to_string_lossy().to_string();
    let p: String = hash_dir.chars().take(16).collect();
    let fname = fp.file_name()?.to_string_lossy().to_string();
    let sess: String = fname.strip_prefix("session-").unwrap_or(&fname)
        .trim_end_matches(".jsonl").chars().take(8).collect();
    Some(Record { a: agent, m: model, t: ms, i: input, o: output, c: cached, s: sess, p })
}

// pi (badlogic/pi-mono): ~/.pi/agent/sessions/<enc-cwd>/<session>.jsonl
// v3 JSONL tree; assistant entries carry usage {input, output, cacheRead,
// cacheWrite, totalTokens}. Two observed nestings are accepted:
//   A) {"type":"assistant", "id":..., "model":..., "usage":{...}}
//   B) {"type":"message", "message":{"role":"assistant","model":...,"usage":{...}}}
// Tree branches re-emit sibling lines — dedup by message id upstream.
pub(crate) fn pi_line(d: &Value, fp: &Path, mtime: i64, meta: &mut HashMap<String, CodexMeta>, key: &str) -> Option<Record> {
    // pi writes the real `cwd` on the session's first line. The directory above
    // the transcript is pi's escaped form of that path (`--Users-alice-proj--`),
    // which is lossy — the same trap as Claude Code. Remember the cwd per file so
    // a later tail-only read (new lines appended to the same session) still
    // labels the project correctly.
    if let Some(cwd) = d.get("cwd").and_then(Value::as_str) {
        let m = meta.entry(key.to_string()).or_default();
        if m.cwd.is_none() { m.cwd = Some(cwd.to_string()); }
    }
    let msg = d.get("message").filter(|v| v.is_object());
    let role = d.get("role").and_then(Value::as_str)
        .or_else(|| msg.and_then(|m| m.get("role")).and_then(Value::as_str));
    let typ = d.get("type").and_then(Value::as_str);
    let is_assistant = role == Some("assistant") || typ == Some("assistant");
    if !is_assistant {
        return None;
    }
    let u = d.get("usage").filter(|v| v.is_object())
        .or_else(|| msg.and_then(|m| m.get("usage")).filter(|v| v.is_object()))?;
    // pi reports input and cacheRead as *parallel* fields, unlike Codex where
    // `input_tokens` already contains `cached_input_tokens`. Its own arithmetic
    // settles it: `totalTokens = input + output + cacheRead + cacheWrite` (the
    // real transcripts carry `totalTokens: 5135` next to `1153 + 142 + 3840`).
    // Taking `input` alone therefore stored *non-cached* input as the whole
    // input, and the cache-hit rate (c/i) came out above 100% — 667.5% on the
    // HUD for this machine. Input now means the same thing it means for every
    // other source: everything the model was fed, cache reads and writes
    // included. `c` stays cache *reads* only, so c <= i holds again.
    let input = get_i64(u, &["input"])
        + get_i64(u, &["cacheRead", "cache_read"])
        + get_i64(u, &["cacheWrite", "cache_write"]);
    let output = get_i64(u, &["output"]);
    let cached = get_i64(u, &["cacheRead", "cache_read"]);
    if input + output == 0 {
        return None;
    }
    let model = d.get("model").and_then(Value::as_str)
        .or_else(|| msg.and_then(|m| m.get("model")).and_then(Value::as_str))
        .unwrap_or("pi").to_string();
    let ms = iso_to_ms(d.get("timestamp").or_else(|| d.get("ts")).unwrap_or(&Value::Null))
        .unwrap_or(mtime);
    // Real files: `<ISO timestamp>_<uuid>.jsonl` → the session is the uuid, not
    // the first eight characters of the timestamp. `short_id` strips the dashes
    // and keeps the last 12 hex characters, the same shape Codex sessions use.
    let fname = fp.file_name()?.to_string_lossy().to_string();
    let stem = fname.trim_end_matches(".jsonl");
    let sess = short_id(stem.rsplit('_').next().unwrap_or(stem));
    // Project: the remembered cwd when we have seen it, otherwise the directory
    // name as-is (no truncation — a cut-off path is worse than a long one).
    let p = meta.get(key).and_then(|m| m.cwd.as_deref()).map(home_short)
        .or_else(|| fp.parent()?.file_name().map(|s| s.to_string_lossy().to_string()))?;
    Some(Record { a: 6, m: model, t: ms, i: input, o: output, c: cached, s: sess, p })
}

// Kimi CLI: legacy ~/.kimi/sessions/<md5>/<sid>/context.jsonl records internal
// {"type":"_usage", ...} token lines; Kimi Code ~/.kimi-code/.../wire.jsonl
// carries usage objects on model messages. Both accepted, keys are tolerant
// across naming variants. If the on-disk format drifts this yields zero
// records (safe no-op), never wrong numbers.
/// Kimi 的会话与项目标识。
///
/// 两种落盘布局（见 `ingest.rs` 的路径表）：
///   `~/.kimi/sessions/<wd-key>/<session>/context.jsonl`（legacy kimi-cli）
///   `~/.kimi-code/sessions/<wd-key>/<session>/agents/main/wire.jsonl`
///
/// 共同点是 `<wd-key>/<session>` 紧跟 `sessions/`。以前这里拿**文件名**当会话 id，
/// 而文件名是常量（`context.jsonl` / `wire.jsonl`），于是所有 Kimi 会话都被压成
/// 同一个 `context`/`wire`——pi 出过一模一样的 bug（`parser_repair_v4`），
/// 当时只修了 pi。项目名也一并从"会话目录"改成 `<wd-key>`。
pub(crate) fn kimi_ids(fp: &Path) -> Option<(String, String)> {
    // `ancestors()` 是从自己往根的，所以命中 `sessions` 之后要取**相对它**的前两段，
    // 而不是继续往上走。
    for p in fp.ancestors() {
        if p.file_name().map(|n| n == "sessions").unwrap_or(false) {
            let mut rest = fp.strip_prefix(p).ok()?.components();
            let wd = rest.next()?.as_os_str().to_string_lossy().to_string();
            let sess = rest.next()?.as_os_str().to_string_lossy().to_string();
            return Some((wd, short_id(&sess)));
        }
    }
    // 没有 `sessions/` 这一层（改名/搬家）：退回"上一级目录是会话"，至少不会
    // 把不同会话算成一个。
    let sess = fp.parent()?.file_name()?.to_string_lossy().to_string();
    let wd = fp.parent()?.parent()?.file_name()?.to_string_lossy().to_string();
    Some((wd, short_id(&sess)))
}

pub(crate) fn kimi_line(d: &Value, fp: &Path, mtime: i64) -> Option<Record> {
    let is_usage_rec = d.get("type").and_then(Value::as_str) == Some("_usage");
    let msg = d.get("message").filter(|v| v.is_object());
    let u = if is_usage_rec {
        d
    } else {
        d.get("usage").filter(|v| v.is_object())
            .or_else(|| msg.and_then(|m| m.get("usage")).filter(|v| v.is_object()))?
    };
    let input = get_i64(u, &["input_tokens", "input", "prompt_tokens", "promptTokens"]);
    let output = get_i64(u, &["output_tokens", "output", "completion_tokens", "completionTokens"]);
    let cached = get_i64(u, &["cache_read_input_tokens", "cache_read", "cacheRead", "cached_tokens", "cached"]);
    if input + output == 0 {
        return None;
    }
    let model = d.get("model").and_then(Value::as_str)
        .or_else(|| msg.and_then(|m| m.get("model")).and_then(Value::as_str))
        .unwrap_or("kimi").to_string();
    let ms = iso_to_ms(d.get("timestamp").or_else(|| d.get("ts")).unwrap_or(&Value::Null))
        .unwrap_or(mtime);
    let (p, sess) = kimi_ids(fp)?;
    Some(Record { a: 7, m: model, t: ms, i: input, o: output, c: cached, s: sess, p })
}

// iFlow CLI (gemini-cli fork): ~/.iflow/projects/<proj>/session-*.jsonl with a
// chat-recording shape close to gemini/qwen. Accept type:"gemini"-style lines
// and any line carrying a {input,output} tokens object.
pub(crate) fn iflow_line(d: &Value, fp: &Path, mtime: i64) -> Option<Record> {
    if d.get("$set").is_some() || d.get("$rewindTo").is_some() {
        return None;
    }
    let tk = d.get("tokens").filter(|v| v.is_object())?;
    let input = get_i64(tk, &["input"]);
    let output = get_i64(tk, &["output"]);
    let cached = get_i64(tk, &["cached"]);
    if input + output == 0 {
        return None;
    }
    let model = d.get("model").and_then(Value::as_str).unwrap_or("iflow").to_string();
    let ms = iso_to_ms(d.get("timestamp").unwrap_or(&Value::Null)).unwrap_or(mtime);
    let dir = fp.parent()?.file_name()?.to_string_lossy().to_string();
    let p: String = dir.chars().take(16).collect();
    let fname = fp.file_name()?.to_string_lossy().to_string();
    let sess: String = fname.strip_prefix("session-").unwrap_or(&fname)
        .trim_end_matches(".jsonl").chars().take(8).collect();
    Some(Record { a: 8, m: model, t: ms, i: input, o: output, c: cached, s: sess, p })
}

// Qoder CLI: ~/.qoder/projects/<proj>/<session>.jsonl — Claude-Code-like
// transcript (message.usage) with a tolerant top-level-usage fallback.
pub(crate) fn qoder_line(d: &Value, fp: &Path, mtime: i64) -> Option<Record> {
    let msg = d.get("message").filter(|v| v.is_object());
    let u = msg.and_then(|m| m.get("usage")).filter(|v| v.is_object())
        .or_else(|| d.get("usage").filter(|v| v.is_object()))?;
    let base_in = get_i64(u, &["input_tokens", "input", "prompt_tokens"]);
    let cached = get_i64(u, &["cache_read_input_tokens", "cache_read", "cacheRead", "cached_tokens", "cached"]);
    let cache_w = get_i64(u, &["cache_creation_input_tokens", "cache_write", "cacheWrite"]);
    let outp = get_i64(u, &["output_tokens", "output", "completion_tokens"]);
    if base_in + cached + cache_w + outp == 0 {
        return None;
    }
    let model = msg.and_then(|m| m.get("model")).and_then(Value::as_str)
        .or_else(|| d.get("model").and_then(Value::as_str))
        .unwrap_or("qoder").to_string();
    let ms = iso_to_ms(d.get("timestamp").or_else(|| d.get("ts")).unwrap_or(&Value::Null))
        .unwrap_or(mtime);
    let sess = d.get("sessionId").or_else(|| d.get("session_id"))
        .and_then(Value::as_str).unwrap_or("qd");
    let dir = fp.parent()?.file_name()?.to_string_lossy().to_string();
    let p: String = dir.chars().take(16).collect();
    Some(Record { a: 9, m: model, t: ms, i: base_in + cached + cache_w, o: outp, c: cached,
                  s: sess.chars().take(8).collect(), p })
}

pub(crate) fn parse_line(agent: u8, d: &Value, fp: &Path, meta: &mut HashMap<String, CodexMeta>,
              mtime: i64, custom: &CustomFile) -> Option<Record> {
    match agent {
        0 => {
            let (i, o, c, m, t) = wb_line(d)?;
            let dir = fp.parent()?.file_name()?.to_string_lossy().to_string();
            // WorkBuddy names project dirs after the path with '/' → '-'
            // (e.g. "Users-alice-WorkBuddy-2026-01-01"); strip the home prefix
            // generically instead of hardcoding a username.
            let mut p = dir.as_str();
            if let Some(rest) = p.strip_prefix("Users-") {
                if let Some(idx) = rest.find("-WorkBuddy-") {
                    p = &rest[idx + "-WorkBuddy-".len()..];
                }
            }
            // The transcript itself carries the real cwd, so use it — the
            // directory tail is only a fallback for lines that have none. Same
            // rule as every other parser, so one directory is one project name.
            let p = match d.get("cwd").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                Some(cwd) => home_short(&strip_file_url(cwd)),
                None => p.to_string(),
            };
            let fname = fp.file_name()?.to_string_lossy().to_string();
            Some(Record { a: 0, m, t, i, o, c, s: short_id(&fname), p })
        }
        1 => {
            let base = fp.file_name()?.to_string_lossy().to_string();
            let key = format!("1|{}", base);
            codex_line(d, &key, meta)
        }
        3 => gemini_qwen_line(agent, d, fp),   // Qwen Code (was Gemini + Qwen)
        5 => {
            let base = fp.file_name()?.to_string_lossy().to_string();
            let key = format!("5|{}", base);
            pi_line(d, fp, mtime, meta, &key)
        }
        6 => kimi_line(d, fp, mtime),
        7 => iflow_line(d, fp, mtime),
        8 => qoder_line(d, fp, mtime),
        n if n >= 10 => {   // user-added sources: same Record shape, their own parser
            let src = custom.sources.get((n - 10) as usize)?;
            if !src.enabled { return None; }
            custom_line(n, d, fp, mtime, src)
        }
        // 显式写 2，不再用 `_` 兜底。以前 Claude Code 是**默认分支**：以后加数据源时
        // 漏写一个 arm，那些文件会静默按 Claude Code 解析，数字能出、但全是错的，
        // 而且看起来一切正常。未知 id 现在返回 None（宁可不记，也不记错的）。
        2 => {
            let (i, o, c, m, t, s) = claude_line(d)?;
            // Hold the raw `cwd`, not the encoded directory name. Claude Code
            // writes every non-alphanumeric character of the cwd as `-`, so the
            // directory name is unreadable (`Users-alice-Desktop-proj---`)
            // and cannot be decoded. The transcript carries the real path, and
            // every usage-bearing line observed has one (780/780). The scanner
            // folds it into a per-project root afterwards.
            let p = d.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
            Some(Record { a: 2, m, t: t?, i, o, c, s, p })
        }
        _ => None,
    }
}
