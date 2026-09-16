//! HTTP 层：所有路由、处理器、页面，以及手动「重新扫描」的编排。
//
// 由 `main.rs` 拆分而来，逻辑未改动。

use crate::*;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::Json;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::UNIX_EPOCH;
use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;

pub(crate) async fn api_data(State(sh): State<Shared>) -> impl IntoResponse {
    let headers = [(header::CONTENT_TYPE, "application/json; charset=utf-8"),
                   (header::CACHE_CONTROL, "no-store")];

    // The record array is by far the most expensive thing this process builds:
    // at the current 25 k records the JSON is ~3.3 MB, and at a year of history
    // it is tens of megabytes — serialised from scratch on every request, from
    // both the dashboard *and* the menubar app on a 60 s timer. So the array is
    // serialised once per record set and kept here, and a hit is a pointer copy
    // (`Bytes` is refcounted) rather than a multi-megabyte re-serialise.
    let gen = sh.0.records_gen.load(Ordering::Relaxed);
    let cached = {
        let c = lock(&sh.0.data_cache);
        if c.0 == gen && !c.1.is_empty() { Some(c.1.clone()) } else { None }
    };

    let recs_json = match cached {
        Some(b) => b,
        None => {
            let snapshot = lock(&sh.0.records).clone();
            let body = serde_json::to_vec(&snapshot).unwrap_or_else(|_| b"[]".to_vec());
            let bytes = axum::body::Bytes::from(body);
            *lock(&sh.0.data_cache) = (gen, bytes.clone());
            bytes
        }
    };

    // `generated_at` stays per-request, so an idle snapshot does not look stale:
    // only the array is cached. Assembling the envelope by hand is safe because
    // the timestamp is a fixed ASCII date — there is nothing to escape.
    let genstamp = Utc::now().with_timezone(&cst()).format("%Y-%m-%dT%H:%M:%S").to_string();
    // The agent list rides along: `AGENT_NAMES` is the single definition of the
    // sources (id order included), and the app and the dashboard both read it
    // from here rather than keeping a copy that can drift out of step with the
    // parsers.
    // Built-in names, then the user's own sources — the front-end labels each
    // record by `records.a`, so the list has to cover ids ≥ 10 too.
    let mut names: Vec<String> = AGENT_NAMES.iter().map(|s| s.to_string()).collect();
    names.extend(load_custom().sources.iter().map(|c| c.name.clone()));
    let agents_json = serde_json::to_vec(&names).unwrap_or_else(|_| b"[]".to_vec());
    (headers, data_envelope(&genstamp, &agents_json, &recs_json))
}

/// The `/api/data` envelope, assembled around an already-serialised record array
/// (see the cache above). Hand-rolled because that array is a `Bytes`, and pinned
/// by `data_envelope_is_valid_json` because hand-rolled JSON fails silently —
/// a stray quote after the agents array turns the whole payload into a parse
/// error for every client at once.
pub(crate) fn data_envelope(genstamp: &str, agents_json: &[u8], recs_json: &[u8]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::with_capacity(recs_json.len() + agents_json.len() + 120);
    body.extend_from_slice(br#"{"generated_at":""#);
    body.extend_from_slice(genstamp.as_bytes());
    body.extend_from_slice(br#"","agents":"#);
    body.extend_from_slice(agents_json);
    body.extend_from_slice(br#","records":"#);
    body.extend_from_slice(recs_json);
    body.extend_from_slice(b"}");
    body
}

/// "YYYY-MM-DD" → the first millisecond of that day in the server's zone.
pub(crate) fn day_start_ms(s: &str) -> Option<i64> {
    let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    d.and_hms_opt(0, 0, 0)?
        .and_local_timezone(cst())
        .single()
        .map(|t| t.timestamp_millis())
}

/// Query for the paginated call log.
///
/// The filters mirror the dashboard's `filtered()` exactly — a paged table that
/// disagreed with the charts above it would be worse than having no table. Two
/// of its quirks are reproduced on purpose:
///   * an *empty* model list means "every model" (the page only applies the
///     model filter when its set is non-empty), while an empty *agent* list
///     means "nothing" (that set is pre-populated with every agent);
///   * the date range is compared at day granularity in the server's zone, which
///     is the same zone the server's own "today" figure is computed in.
#[derive(serde::Deserialize)]
pub(crate) struct RecordsQuery {
    pub(crate) offset: Option<usize>,
    pub(crate) limit: Option<usize>,
    pub(crate) sort: Option<String>,
    pub(crate) dir: Option<String>,
    pub(crate) from: Option<String>,
    pub(crate) to: Option<String>,
    pub(crate) agents: Option<String>,
    pub(crate) models: Option<String>,
    pub(crate) proj: Option<String>,
}

/// The column a page is ordered by. An unrecognised name falls back to the
/// timestamp rather than erroring: the sort key comes from a query string, and a
/// typo should degrade to the default view, not 500 the table.
pub(crate) fn records_primary(r: &Record, sort: &str) -> i64 {
    match sort {
        "i" => r.i,
        "o" => r.o,
        "c" => r.c,
        _ => r.t,
    }
}

/// Ordering for one page of the call log.
///
/// The tiebreak runs all the way down to the whole row on purpose. A primary key
/// alone is not enough: thousands of calls share a token count (and a timestamp),
/// and paging over a key with ties reshuffles the rows between requests, so the
/// user sees some rows twice and never sees others. Only a *total* order makes
/// `OFFSET n LIMIT m` stable, which is why every field ends up in the chain —
/// together they are the same tuple the `records` table is unique on.
///
/// Note the direction applies to the chosen column only; ties always resolve
/// ascending, so a stable order does not depend on the sort direction.
pub(crate) fn records_cmp(x: &Record, y: &Record, sort: &str, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let head = records_primary(x, sort).cmp(&records_primary(y, sort));
    let head = if desc { head.reverse() } else { head };
    head.then_with(|| x.t.cmp(&y.t))
        .then_with(|| x.a.cmp(&y.a))
        .then_with(|| x.s.cmp(&y.s))
        .then_with(|| x.p.cmp(&y.p))
        .then_with(|| x.m.cmp(&y.m))
        .then_with(|| x.i.cmp(&y.i))
        .then_with(|| x.o.cmp(&y.o))
        .then_with(|| x.c.cmp(&y.c))
        .then(Ordering::Equal)
}

/// One page of raw call records.
///
/// Added because the dashboard used to render the call log from the full
/// `/api/data` payload: 25 k records is a 3.3 MB download, and every extra month
/// of history grows it. A page is a bounded slice, so the table's cost no longer
/// scales with how long the app has been running.
/// A day bound that may be absent, empty, or malformed.
///
/// An unrecognised date is rejected rather than dropped. Silently ignoring it
/// would *widen* the query, so a typo'd bound would show the reader more rows
/// than they asked for while the pager claimed the filter was applied — the
/// worst kind of wrong answer, because nothing looks broken. An empty string is
/// accepted as "no bound": the dashboard omits absent bounds, but a hand-built
/// request sending `from=` clearly means the same thing.
pub(crate) fn parse_day_bound(raw: Option<&str>, name: &str) -> Result<Option<i64>, String> {
    match raw {
        None => Ok(None),
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => day_start_ms(s.trim())
            .map(Some)
            .ok_or_else(|| format!("\"{name}\" must be YYYY-MM-DD, got {s:?}")),
    }
}

/// Error bodies are JSON too, so a client that always parses the response never
/// has to special-case a plain-text failure.
pub(crate) fn err_body(msg: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"error": msg})).unwrap_or_else(|_| b"{}".to_vec())
}

pub(crate) async fn api_records(
    State(sh): State<Shared>,
    Query(q): Query<RecordsQuery>,
) -> impl IntoResponse {
    let json_headers = [(header::CONTENT_TYPE, "application/json; charset=utf-8"),
                        (header::CACHE_CONTROL, "no-store")];

    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0);
    let sort = q.sort.as_deref().unwrap_or("t").to_string();
    let desc = q.dir.as_deref() != Some("asc");

    let lo = match parse_day_bound(q.from.as_deref(), "from") {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, json_headers, err_body(&e)),
    };
    // `to` is inclusive: the bound covers the whole named day.
    let hi = match parse_day_bound(q.to.as_deref(), "to") {
        Ok(Some(v)) => Some(v + 86_400_000),
        Ok(None) => None,
        Err(e) => return (StatusCode::BAD_REQUEST, json_headers, err_body(&e)),
    };

    let agents: Option<HashSet<u8>> = q.agents.as_deref().map(|s| {
        s.split(',').filter(|t| !t.is_empty())
            .filter_map(|t| t.trim().parse::<u8>().ok())
            .collect()
    });
    let models: HashSet<String> = q.models.as_deref().unwrap_or("")
        .split(',').filter(|t| !t.is_empty()).map(str::to_string).collect();
    let proj = q.proj.clone().unwrap_or_default();

    let snapshot = lock(&sh.0.records);
    let mut idx: Vec<usize> = (0..snapshot.len())
        .filter(|&i| {
            let r = &snapshot[i];
            if let Some(ref a) = agents {
                if !a.contains(&r.a) { return false; }
            }
            if !models.is_empty() && !models.contains(&r.m) { return false; }
            if !proj.is_empty() && r.p != proj { return false; }
            if let Some(lo) = lo {
                if r.t < lo { return false; }
            }
            if let Some(hi) = hi {
                if r.t >= hi { return false; }
            }
            true
        })
        .collect();
    let total = idx.len();

    idx.sort_by(|&x, &y| records_cmp(&snapshot[x], &snapshot[y], &sort, desc));

    let page: Vec<&Record> = idx.iter().skip(offset).take(limit).map(|&i| &snapshot[i]).collect();
    let body = json!({
        "total": total,
        "offset": offset,
        "limit": limit,
        "sort": sort,
        "dir": if desc { "desc" } else { "asc" },
        "rows": page,
    });
    (StatusCode::OK, json_headers,
     serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec()))
}

pub(crate) async fn api_live(State(sh): State<Shared>) -> impl IntoResponse {
    let mut v = live_stats(&sh.0);
    // settings ride along so the native app picks changes up within a second —
    // web and the menubar app share one source of truth
    v["settings"] = lock(&sh.0.prefs).clone();
    let body = serde_json::to_vec(&v).unwrap();
    ([(header::CONTENT_TYPE, "application/json; charset=utf-8"),
      (header::CACHE_CONTROL, "no-store")], body)
}

pub(crate) async fn api_settings_get(State(sh): State<Shared>) -> impl IntoResponse {
    let prefs = lock(&sh.0.prefs).clone();
    let body = serde_json::to_vec(&prefs).unwrap();
    ([(header::CONTENT_TYPE, "application/json; charset=utf-8"),
      (header::CACHE_CONTROL, "no-store")], body)
}

/// Whitelisted preference update — merged into the stored object.
pub(crate) async fn api_settings_post(
    State(sh): State<Shared>,
    Json(incoming): Json<Value>,
) -> impl IntoResponse {
    let mut prefs = lock(&sh.0.prefs);
    if let (Some(dst), Some(src)) = (prefs.as_object_mut(), incoming.as_object()) {
        for key in ["lang", "theme"] {
            if let Some(v) = src.get(key).and_then(Value::as_str) {
                dst.insert(key.to_string(), Value::String(v.to_string()));
            }
        }
    }
    let snapshot = prefs.clone();
    drop(prefs);
    sh.0.store.save_kv("prefs", &snapshot.to_string());
    let body = serde_json::to_vec(&snapshot).unwrap();
    ([(header::CONTENT_TYPE, "application/json; charset=utf-8"),
      (header::CACHE_CONTROL, "no-store")], body)
}

pub(crate) async fn settings_page(State(sh): State<Shared>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], sh.0.settings_html.clone())
}

pub(crate) async fn about_page(State(sh): State<Shared>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], sh.0.about_html.clone())
}

pub(crate) async fn sources_page(State(sh): State<Shared>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], sh.0.sources_html.clone())
}

/// Vendored chart.js, served from this process instead of a CDN (see
/// `vendor/README.md`). The dashboard used to pull it from cdn.jsdelivr.net:
/// that leaked the user's IP on every dashboard open and gave a third party's
/// script a seat inside a page that can read the whole local token store — and
/// it broke the charts offline or wherever the CDN is unreachable.
///
/// A missing asset answers 404 with a sentence rather than 200 with an empty
/// body, so a bad build shows up as an error instead of as silent blank charts.
pub(crate) async fn vendor_chart_js(State(sh): State<Shared>) -> axum::response::Response {
    let bytes = sh.0.assets.get("vendor/chart.umd.min.js").cloned().unwrap_or_default();
    if bytes.is_empty() {
        return (StatusCode::NOT_FOUND, "chart.umd.min.js missing from this build").into_response();
    }
    ([(header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
      (header::CACHE_CONTROL, "public, max-age=86400")], bytes).into_response()
}

pub(crate) async fn api_stream(State(sh): State<Shared>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let mut rx = sh.0.tx.subscribe();
    let (tx_out, rx_out) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(16);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if tx_out.send(Ok(Event::default().comment("keepalive"))).await.is_err() { break; }
                }
                msg = rx.recv() => {
                    match msg {
                        Ok(new) => {
                            let payload = serde_json::to_string(&json!({"new": new})).unwrap_or_default();
                            if tx_out.send(Ok(Event::default().data(payload))).await.is_err() { break; }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    });
    Sse::new(ReceiverStream::new(rx_out))
}

pub(crate) async fn index(State(sh): State<Shared>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], sh.0.dashboard.clone())
}

/// Per-agent source detection: how many transcript files and bytes are being
/// watched. Agents installed later are picked up automatically (enumerate_files
/// re-runs every tick) — this endpoint makes that visible to the user.
pub(crate) async fn api_sources(State(sh): State<Shared>) -> impl IntoResponse {
    let fs = lock(&sh.0.files_state);
    let mut files = [0u32; 10];
    let mut bytes = [0u64; 10];
    // Custom sources have ids ≥ 10 and do not fit the built-in arrays.
    let mut custom_files: std::collections::HashMap<u8, u32> = std::collections::HashMap::new();
    let mut custom_bytes: std::collections::HashMap<u8, u64> = std::collections::HashMap::new();
    for st in fs.values() {
        if (st.agent as usize) < 10 {
            files[st.agent as usize] += 1;
            bytes[st.agent as usize] += st.off;
        } else {
            *custom_files.entry(st.agent).or_insert(0) += 1;
            *custom_bytes.entry(st.agent).or_insert(0) += st.off;
        }
    }
    // How many records each source actually contributed. This is the one signal
    // the UI uses to decide whether a source is worth showing: a format we know
    // but that has never produced a number is not a data source this machine
    // has, and the pages now say so by leaving it out rather than by printing a
    // row that says "no data". Counted here so all three pages agree, and so
    // nobody has to pull 28k records to find out.
    let mut records_by_agent: std::collections::HashMap<u8, u64> = std::collections::HashMap::new();
    for r in lock(&sh.0.records).iter() {
        *records_by_agent.entry(r.a).or_insert(0) += 1;
    }
    // The rule this endpoint exists to keep: *如果它存在，就不能被忽略*.
    // Every source reports the paths it looked at and whether each base exists,
    // so "0 files" can be told apart from "not installed" — and the locations we
    // have audited but cannot parse yet are reported too (`probes`), instead of
    // being invisible.
    let home = std::env::var("HOME").unwrap_or_default();
    let short = |p: &str| p.strip_prefix(&home).map(|r| format!("~{r}")).unwrap_or_else(|| p.to_string());
    let patterns = source_patterns();
    // Antigravity keeps one SQLite database per conversation instead of
    // transcripts, so its counts come from that directory rather than from the
    // line reader's bookkeeping. `parseable: false` is deliberate — the numbers
    // are reported as candidates, not counted (see `antigravity_peek`).
    let ag = antigravity_peek();
    let mut sources: Vec<Value> = AGENT_NAMES
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let pats: Vec<&String> = patterns.iter().find(|(a, _)| *a as usize == i)
                .map(|(_, v)| v.iter().collect()).unwrap_or_default();
            // the literal prefix of a pattern is its base directory
            let bases: Vec<Value> = pats.iter().map(|p| {
                let base = p.split('*').next().unwrap_or(p).trim_end_matches('/');
                let base = base.rsplit_once('/').map(|(d, _)| d).unwrap_or(base);
                json!({"path": short(base), "exists": std::path::Path::new(base).exists()})
            }).collect();
            let mut v = json!({
                "agent": i, "name": name, "files": files[i], "bytes": bytes[i],
                "parseable": true,
                "records": records_by_agent.get(&(i as u8)).copied().unwrap_or(0),
                "patterns": pats.iter().map(|p| short(p)).collect::<Vec<_>>(),
                "bases": bases,
            });
            if i == AGENT_NAMES.len() - 1 {
                v["files"] = ag["files"].clone();
                v["bytes"] = ag["bytes"].clone();
                v["parseable"] = json!(false);
                v["generations"] = ag["generations"].clone();
                v["candidate"] = ag["candidate"].clone();
                v["newest"] = ag["newest"].clone();
                if let Some(o) = v.as_object_mut() {
                    o.insert("bases".into(), json!([{
                        "path": short(&format!("{home}/.gemini/antigravity/conversations")),
                        "exists": std::path::Path::new(&format!("{home}/.gemini/antigravity/conversations")).exists(),
                    }]));
                }
            }
            // OpenCode keeps everything in one SQLite database rather than
            // transcripts, so `source_patterns()` has nothing to offer and the
            // page printed "no data" next to a source that was plainly working.
            // Report the file it actually opens (same resolution as
            // `tick_opencode`: XDG_DATA_HOME, else ~/.local/share).
            if name == &"OpenCode" {
                let dir = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
                let db = format!("{dir}/opencode/opencode.db");
                if let Some(o) = v.as_object_mut() {
                    o.insert("patterns".into(), json!([short(&db)]));
                    o.insert("bases".into(), json!([{
                        "path": short(&format!("{dir}/opencode")),
                        "exists": std::path::Path::new(&format!("{dir}/opencode")).exists(),
                    }]));
                }
            }
            v
        })
        .collect();
    let probes: Vec<Value> = KNOWN_UNPARSED.iter().map(|(name, rel, reason)| {
        let path = format!("{home}/{rel}");
        let p = std::path::Path::new(&path);
        let bytes = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        json!({"name": name, "path": short(&path), "exists": p.exists(),
               "bytes": bytes, "reason": reason})
    }).collect();
    // User-added sources, appended after the built-in ten so ids line up with
    // `records.a` (10, 11, …). Their offsets come from the same file bookkeeping.
    let custom_cfg = load_custom();
    for (i, c) in custom_cfg.sources.iter().enumerate() {
        let id = 10 + i as u8;
        let base = expand_tilde(&c.path);
        let base = base.split('*').next().unwrap_or(&base).trim_end_matches('/');
        let base = base.rsplit_once('/').map(|(d, _)| d).unwrap_or(base).to_string();
        sources.push(json!({
            "agent": id, "name": c.name, "files": custom_files.get(&id).copied().unwrap_or(0),
            "bytes": custom_bytes.get(&id).copied().unwrap_or(0),
            "parseable": c.enabled && !c.format.is_empty(),
            "records": records_by_agent.get(&id).copied().unwrap_or(0),
            "custom": true, "format": c.format,
            // How it got here is a fact the UI wants to print, and only this
            // endpoint knows it (`discovered` is the auto-discovery marker).
            "discovered": custom_cfg.discovered.contains(&c.id),
            "patterns": [short(&c.path)],
            "bases": [{"path": short(&base), "exists": Path::new(&base).exists()}],
        }));
    }
    let body = serde_json::to_vec(&json!({"sources": sources, "probes": probes})).unwrap();
    ([(header::CONTENT_TYPE, "application/json; charset=utf-8"),
      (header::CACHE_CONTROL, "no-store")], body)
}

/// `GET /api/registry` — what the registry knows right now, and where it came from.
pub(crate) async fn api_registry_get() -> impl IntoResponse {
    let reg = load_registry();
    let local = registry_path().exists();
    let list: Vec<Value> = reg.sources.iter().map(|e| json!({
        "name": e.name, "paths": e.paths, "note": e.note,
        "exists": e.paths.iter().any(|p| std::path::Path::new(&expand_registry_path(p)).exists()),
    })).collect();
    // Default host for the hosted list: the telemetry server, which already
    // serves the download page and the release files. Overridable by env for a
    // self-hosted list, and remembered once a fetch has succeeded.
    let default_url = default_registry_url();
    let hits = list.iter().filter(|e| e["exists"].as_bool().unwrap_or(false)).count();
    json_ok(json!({"version": reg.version, "updated": reg.updated,
                   "local_overlay": local, "config_path": "~/.tokendance/registry.json",
                   "source_url": if reg.source_url.is_empty() { default_url.clone() } else { reg.source_url.clone() },
                   "fetched_from": reg.source_url, "default_url": default_url,
                   "count": list.len(), "hits": hits, "sources": list}))
}

/// `POST /api/registry/update` — fetch a registry JSON and merge it over the
/// seed. This is the "update" half of the mechanism: the shipped list covers
/// what we know at release time, and this lets a hosted list add names later
/// without a new build. Only `{version, updated, sources:[{name, paths, note}]}`
/// is accepted; a bad payload changes nothing.
pub(crate) async fn api_registry_update(Json(v): Json<Value>) -> impl IntoResponse {
    // 不带 url 时用"上次成功拉取过的地址"，再退回到默认地址：设置页那张卡删掉之后，
    // 用户手上只剩一个「更新名单」按钮，没有输入框可以填地址。
    let given = v.get("url").and_then(Value::as_str).unwrap_or("");
    let url = if given.starts_with("http") {
        given.to_string()
    } else {
        let remembered = load_registry().source_url;
        if remembered.starts_with("http") { remembered } else { default_registry_url() }
    };
    if !url.starts_with("http") {
        return json_ok(json!({"ok": false, "error": "give an http(s) url"}));
    }
    // Two guards, both learned the hard way. (1) The fetch runs on the blocking
    // pool: spawning `curl` from a runtime worker once froze the entire server,
    // because the accept loop shares those threads. (2) The caller gets an
    // answer within 20 s even if the child never returns — a spawned process
    // that hangs cannot be cancelled from here, and without this the page just
    // spins forever.
    let target = url.to_string();
    let fetch = offload(move || {
        std::process::Command::new("/usr/bin/curl")
            .args(["-sSL", "--connect-timeout", "8", "--max-time", "15", &target])
            .output()
    });
    let Ok(Some(fetched)) = tokio::time::timeout(std::time::Duration::from_secs(20), fetch).await
    else { return json_ok(json!({"ok": false, "error": "fetch timed out"})) };
    let Ok(out) = fetched else { return json_ok(json!({"ok": false, "error": "curl failed to start"})) };
    if !out.status.success() {
        return json_ok(json!({"ok": false, "error": format!("fetch failed: {}", out.status)}));
    }
    let Ok(text) = String::from_utf8(out.stdout) else {
        return json_ok(json!({"ok": false, "error": "response was not utf-8"}));
    };
    let Ok(incoming) = serde_json::from_str::<Registry>(&text) else {
        return json_ok(json!({"ok": false, "error": "not a registry json ({version, sources:[{name,paths}]})"}));
    };
    if incoming.sources.is_empty() {
        return json_ok(json!({"ok": false, "error": "registry had no sources"}));
    }
    // merge over the existing local overlay
    let mut local: Registry = std::fs::read_to_string(registry_path()).ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Registry { version: 1, updated: String::new(), source_url: String::new(), sources: Vec::new() });
    let mut added = 0;
    for e in incoming.sources {
        if e.name.trim().is_empty() { continue; }
        match local.sources.iter_mut().find(|x| x.name.eq_ignore_ascii_case(&e.name)) {
            Some(slot) => *slot = e,
            None => { local.sources.push(e); added += 1; }
        }
    }
    // Keep only the *delta*. The first version of this wrote every fetched entry
    // into the overlay, which quietly froze the shipped list: a later app release
    // that corrected a path would be shadowed by the stale copy the overlay still
    // carried. Entries identical to the seed are therefore dropped again — the
    // overlay is for what we add or override, not for a second copy of the list.
    let seed = seed_registry();
    local.sources.retain(|e| match seed.sources.iter().find(|s| s.name.eq_ignore_ascii_case(&e.name)) {
        None => true,
        Some(s) => s.paths != e.paths || s.note != e.note,
    });
    local.version = incoming.version.max(1);
    local.updated = incoming.updated;
    local.source_url = url.clone();
    let p = registry_path();
    if let Some(dir) = p.parent() { let _ = std::fs::create_dir_all(dir); }
    if std::fs::write(&p, serde_json::to_string_pretty(&local).unwrap_or_default()).is_err() {
        return json_ok(json!({"ok": false, "error": "cannot write ~/.tokendance/registry.json"}));
    }
    json_ok(json!({"ok": true, "added": added, "total": local.sources.len(), "updated": local.updated}))
}

/// `POST /api/custom/validate` — the step that makes a user-supplied path safe
/// to accept: does the directory exist, how many files match, which of our
/// known shapes does a sample line look like, and what numbers come out of it.
pub(crate) fn json_ok(v: Value) -> axum::response::Response {
    ([(header::CONTENT_TYPE, "application/json; charset=utf-8"),
      (header::CACHE_CONTROL, "no-store")],
     serde_json::to_vec(&v).unwrap_or_default()).into_response()
}

pub(crate) fn validate_source(v: &Value) -> Value {
    let path = v.get("path").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if let Err(e) = validate_path(&path) {
        return json!({"ok": false, "error": e});
    }
    // Refuse paths that are already counted. `chatgpt` resolves to
    // `/Applications/ChatGPT.app` whose bundle id is `com.openai.codex`, i.e. the
    // Codex desktop app — so a name that looks like a new product can point
    // straight at data we already read, and adding it would double every number.
    if let Some(owner) = cover_owner(&path) {
        return json!({"ok": false, "covered": true, "owner": owner,
                      "error": format!("这条路径已经在「{owner}」的统计范围内，再加会重复计数")});
    }
    let pat = expand_tilde(&path);
    let mut files: Vec<PathBuf> = glob::glob(&pat).into_iter().flatten().filter_map(Result::ok).collect();
    files.sort_by_key(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok());
    files.reverse();
    let bytes: u64 = files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
    let newest = files.first()
        .and_then(|f| std::fs::metadata(f).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64).unwrap_or(0);

    // Which shape is it? Try the sample against each parser we ship.
    let mut detected = String::new();
    let mut sample = Value::Null;
    let mut parsed_lines = 0usize;
    let mut keys: Vec<String> = Vec::new();
    let probe = CustomSource {
        id: "probe".into(), name: "probe".into(), enabled: true,
        path: path.clone(),
        format: v.get("format").and_then(Value::as_str).unwrap_or("").to_string(),
        fields: v.get("fields").and_then(Value::as_object)
            .map(|o| o.iter().filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string()))).collect())
            .unwrap_or_default(),
    };
    if let Some(f) = files.first() {
        let text = std::fs::read(f).unwrap_or_default();
        let mut meta = HashMap::new();
        for line in text.split(|b| *b == b'\n').take(400) {
            if line.is_empty() { continue; }
            let Ok(d) = serde_json::from_slice::<Value>(line) else { continue };
            if keys.is_empty() {
                if let Some(o) = d.as_object() {
                    keys = o.keys().take(14).cloned().collect();
                }
            }
            let rec = match probe.format.as_str() {
                "codex-rollout" => codex_line(&d, "probe", &mut meta),
                "claude-transcript" => parse_line(2, &d, f, &mut meta, 0, &CustomFile::default()),
                _ => custom_line(11, &d, f, 0, &probe),
            };
            if let Some(r) = rec {
                parsed_lines += 1;
                if sample.is_null() {
                    sample = json!({"model": r.m, "input": r.i, "output": r.o, "cached": r.c,
                                    "time_ms": r.t, "session": r.s, "project": r.p});
                    if detected.is_empty() {
                        detected = if probe.format.is_empty() { "jsonl-usage".into() } else { probe.format.clone() };
                    }
                }
            }
        }
    }
    json!({
        "ok": true,
        "files": files.len(),
        "bytes": bytes,
        "newest_ms": newest,
        "newest_file": files.first().map(|f| f.to_string_lossy().to_string()),
        "detected_format": if detected.is_empty() { Value::Null } else { Value::String(detected) },
        "parsed_lines": parsed_lines,
        "sample_keys": keys,
        "sample": sample,
        "sample_files": files.iter().take(5).map(|f| f.to_string_lossy().to_string()).collect::<Vec<_>>(),
    })
}

pub(crate) async fn api_custom_validate(Json(v): Json<Value>) -> impl IntoResponse {
    json_ok(validate_source(&v))
}

/// `GET /api/custom/probe?name=X` — the codified search. The user supplies a
/// product name and nothing else.
pub(crate) async fn api_custom_probe(axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>) -> impl IntoResponse {
    let name = q.get("name").cloned().unwrap_or_default();
    if name.trim().len() < 2 {
        return json_ok(json!({"ok": false, "error": "give me at least two characters of the product name"}));
    }
    let mut probe = offload(move || probe_agent(&name)).await.unwrap_or(Value::Null);
    probe["ok"] = json!(probe["best"].is_object());
    json_ok(probe)
}

/// `POST /api/custom/discover` — run the automatic search now (the same thing
/// that happens at start-up and every six hours).
pub(crate) async fn api_custom_discover(State(sh): State<Shared>) -> impl IntoResponse {
    // The full three-tier report goes back to the page, because "T1 knew it, T3
    // found it by content" is the part that tells the user the mechanism worked.
    // `added` stays the news-only list, so the page never claims it just added
    // something that was counted all along.
    // A whole scan takes ~9 s here, so 120 s is a generous ceiling, not a
    // timeout in the normal path. It exists because a scan can in principle be
    // blocked by something outside our control (a hung network mount, a new
    // protected store): the page must get an answer it can show either way.
    let scan = offload(|| discover_tiered(true));
    let scanned = tokio::time::timeout(std::time::Duration::from_secs(120), scan).await;
    let (rows, timed_out) = match scanned {
        Ok(Some(rows)) => (rows, false),
        _ => (Vec::<Value>::new(), true),
    };
    let added: Vec<String> = rows.iter().filter(|r| r["added"].as_bool().unwrap_or(false))
        .map(|r| format!("[T{}] {} ← {} ({} 行)", r["tier"],
                         r["name"].as_str().unwrap_or(""), r["path"].as_str().unwrap_or(""),
                         r["lines"].as_u64().unwrap_or(0)))
        .collect();
    if !added.is_empty() {
        reset_custom_rows(&sh.0);
        let mut fs = lock(&sh.0.files_state);
        fs.retain(|_, st| st.agent < 10);
    }
    json_ok(json!({"ok": true, "added": added, "report": rows, "timed_out": timed_out}))
}

/// `POST /api/custom/probe` — find *and* add in one step, which is what the user
/// asked for: "type the name, it searches, and if the result looks right it gets
/// added". Nothing is added unless a candidate actually parsed.
pub(crate) async fn api_custom_add_by_name(State(sh): State<Shared>, Json(v): Json<Value>) -> impl IntoResponse {
    let name = v.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if name.len() < 2 {
        return json_ok(json!({"ok": false, "error": "give me the product name"}));
    }
    let probe = offload({
        let name = name.clone();
        move || probe_agent(&name)
    }).await.unwrap_or(Value::Null);
    if probe["builtin"].as_bool().unwrap_or(false) {
        return json_ok(json!({"ok": false, "builtin": true,
            "error": "已经内置支持这个数据源了，不用添加" }));
    }
    let Some(best) = probe["best"].as_object() else {
        return json_ok(json!({"ok": false, "probe": probe,
            "error": "没找到能读出用量的文件（下面列出我找过的位置）"}));
    };
    let path = best.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    // Same guard as the manual path: the name may resolve to data we already
    // count (typing "chatgpt" lands on ~/.codex, because that app's bundle id is
    // com.openai.codex). Adding it would double-count, so refuse and say who
    // already owns it.
    if let Some(owner) = cover_owner(&path) {
        return json_ok(json!({"ok": false, "covered": true, "owner": owner,
            "found": short_home(&path),
            "error": format!("找到的其实是「{owner}」的数据（{owner} 已在统计中），不用再加")}));
    }
    // Logs rotate: `llm_request.1.jsonl` becomes `.2`, `session-2026-09-15.jsonl`
    // becomes tomorrow's. Registering the single file we found would stop
    // counting at the next rotation, so a trailing number turns into a glob over
    // its siblings. (Nothing else about the path is guessed.)
    let home = std::env::var("HOME").unwrap_or_default();
    let stored_path = {
        let short = path.strip_prefix(&home).map(|r| format!("~{r}")).unwrap_or_else(|| path.clone());
        let p = Path::new(&path);
        let (dir, stem, ext) = (
            p.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default(),
            p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            p.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        );
        let trimmed = stem.trim_end_matches(|c: char| c.is_ascii_digit())
            .trim_end_matches(['.', '_', '-']).to_string();
        if trimmed != stem && !trimmed.is_empty() {
            let short_dir = dir.strip_prefix(&home).map(|r| format!("~{r}")).unwrap_or(dir.clone());
            format!("{short_dir}/{trimmed}*{}{ext}", if ext.is_empty() { "" } else { "." })
        } else { short }
    };
    let mut cfg = load_custom();
    cfg.version = 1;
    let id: String = name.to_lowercase().chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let id = id.trim_matches('-').to_string();
    let entry = CustomSource { id: id.clone(), name: name.clone(), enabled: true,
                              path: stored_path.clone(), format: "jsonl-usage".into(), fields: HashMap::new() };
    match cfg.sources.iter_mut().find(|c| c.id == id) { Some(slot) => *slot = entry, None => cfg.sources.push(entry) }
    if let Err(e) = save_custom(&cfg) {
        return json_ok(json!({"ok": false, "error": format!("cannot write config: {e}")}));
    }
    reset_custom_rows(&sh.0);
    { let mut fs = lock(&sh.0.files_state); fs.retain(|_, st| st.agent < 10); }
    json_ok(json!({"ok": true, "id": id, "added": {"name": name, "path": stored_path,
                   "found": short_home(&path),
                   "parsed_lines": best.get("parsed_lines"), "sample": best.get("sample"),
                   "bytes": best.get("bytes")},
                   "checked": probe["checked"], "derived_names": probe["derived_names"]}))
}

/// Sources live in `~/.tokendance/sources.json`. Saving also drops every custom
/// record so the scan re-derives them: agent ids are assigned by position in the
/// file, so adding or removing an entry shifts the ones after it — the same
/// renumbering trap `parser_repair_v6` handled for the built-in list.
pub(crate) fn reset_custom_rows(sh: &AppState) {
    let _ = sh.store.delete_custom_records();
    // The in-memory copy has to go too, otherwise a removed source keeps showing
    // its rows until the next restart — the database and `/api/data` disagree.
    {
        let mut recs = lock(&sh.records);
        recs.retain(|r| r.a < 10);
    }
    // bump the body cache generation so /api/data is rebuilt on the next read
    sh.records_gen.fetch_add(1, Ordering::Relaxed);
}

pub(crate) async fn api_custom_get(State(sh): State<Shared>) -> impl IntoResponse {
    let cfg = load_custom();
    let fs = lock(&sh.0.files_state);
    let mut files = std::collections::HashMap::new();
    let mut bytes = std::collections::HashMap::new();
    for st in fs.values() {
        if st.agent >= 10 {
            *files.entry(st.agent).or_insert(0u32) += 1;
            *bytes.entry(st.agent).or_insert(0u64) += st.off;
        }
    }
    let list: Vec<Value> = cfg.sources.iter().enumerate().map(|(i, c)| {
        let id = 10 + i as u8;
        json!({"id": c.id, "name": c.name, "enabled": c.enabled, "path": c.path,
               "format": c.format, "fields": c.fields, "agent": id,
               // `discovered` = found by the automatic search rather than typed in
               "discovered": cfg.discovered.contains(&c.id),
               "files": files.get(&id).copied().unwrap_or(0),
               "bytes": bytes.get(&id).copied().unwrap_or(0)})
    }).collect();
    json_ok(json!({"sources": list, "config_path": "~/.tokendance/sources.json"}))
}

pub(crate) async fn api_custom_post(State(sh): State<Shared>, Json(v): Json<Value>) -> impl IntoResponse {
    let valid = validate_source(&v);
    if valid["ok"] != json!(true) {
        return json_ok(json!({"ok": false, "error": valid["error"]}));
    }
    let name = v.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let name = if name.is_empty() { "Custom".to_string() } else { name };
    let mut id = v.get("id").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if id.is_empty() {
        id = name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        id = id.trim_matches('-').to_string();
        if id.is_empty() { id = "custom".into(); }
    }
    let format = v.get("format").and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| valid["detected_format"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "jsonl-usage".into());
    let fields: HashMap<String, String> = v.get("fields").and_then(Value::as_object)
        .map(|o| o.iter().filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string()))).collect())
        .unwrap_or_default();

    let mut cfg = load_custom();
    cfg.version = 1;
    let entry = CustomSource { id: id.clone(), name, enabled: true, path: v.get("path").and_then(Value::as_str).unwrap_or("").into(), format, fields };
    match cfg.sources.iter_mut().find(|c| c.id == id) {
        Some(slot) => *slot = entry,
        None => cfg.sources.push(entry),
    }
    if let Err(e) = save_custom(&cfg) {
        return json_ok(json!({"ok": false, "error": format!("cannot write config: {e}")}));
    }
    reset_custom_rows(&sh.0);
    {
        let mut fs = lock(&sh.0.files_state);
        fs.retain(|_, st| st.agent < 10);
    }
    json_ok(json!({"ok": true, "id": id, "format": cfg.sources.last().map(|c| c.format.clone()).unwrap_or_default(),
                   "note": "saved; the scan picks it up within a second"}))
}

pub(crate) async fn api_custom_remove(State(sh): State<Shared>, Json(v): Json<Value>) -> impl IntoResponse {
    let id = v.get("id").and_then(Value::as_str).unwrap_or("");
    let mut cfg = load_custom();
    let before = cfg.sources.len();
    cfg.sources.retain(|c| c.id != id);
    // Remember the rejection: automatic discovery runs again on every start, and
    // "remove" has to mean removed, not "removed until the next scan".
    if !id.is_empty() && !cfg.removed.iter().any(|r| r == id) {
        cfg.removed.push(id.to_string());
    }
    cfg.discovered.retain(|d| d != id);
    if let Err(e) = save_custom(&cfg) {
        return json_ok(json!({"ok": false, "error": format!("cannot write config: {e}")}));
    }
    reset_custom_rows(&sh.0);
    {
        let mut fs = lock(&sh.0.files_state);
        fs.retain(|_, st| st.agent < 10);
    }
    json_ok(json!({"ok": true, "removed": before - cfg.sources.len()}))
}

/// `GET /api/custom/find` — the answer to "where would the file even be?".
///
/// A bounded, *content-sniffing* search: a handful of standard roots, depth ≤ 4,
/// files touched in the last 45 days, and a score from whether the first 8 KB
/// contains usage-ish keys. It never walks the whole disk, never enters the
/// TCC-protected folders (Desktop / Documents / Downloads) and never reads more
/// than the first few KB of a candidate.
pub(crate) async fn api_custom_find() -> impl IntoResponse {
    match offload(custom_find_scan).await {
        Some(r) => r,
        None => json_ok(json!({"ok": false, "error": "the scan failed"})),
    }
}

/// The scan itself: walks up to 40 k entries across the standard roots, so it is
/// exactly the kind of blocking work that must not run on a runtime worker.
pub(crate) fn custom_find_scan() -> axum::response::Response {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let roots = [
        "Library/Application Support", ".config", ".local/share", ".local/state",
        "Library/Logs", ".cache",
    ];
    let want_keys = ["\"input\"", "\"input_tokens\"", "\"prompt_tokens\"", "\"usage\"",
                     "\"tokens\"", "\"total_tokens\"", "\"completion_tokens\"", "\"cacheRead\""];
    // Directories that are never worth walking: the agents we already read, the
    // ones we know better than the user does, and the packaging noise that fills
    // `~/Library/Application Support` with tens of thousands of files.
    let skip = [".codex", ".claude", ".tokendance", ".workbuddy", ".gemini", ".pi",
                ".qwen", ".kimi", ".kimi-code", ".iflow", ".qoder", "Antigravity",
                "CodeBuddy", "Trae", "Cursor",
                // noise
                "node_modules", ".git", "vendor", "dist", "build", "__pycache__",
                "Caches", "Cache", "GPUCache", "Code Cache", "DawnCache",
                "DawnGraphiteCache", "DawnWebGPUCache", "Crashpad", "blob_storage",
                "Service Worker", "Backups", "partitions", "CacheStorage",
                "Local Storage", "Session Storage", "IndexedDB"];
    let cutoff = now_ms() - 45 * 86_400_000;
    let mut hits: Vec<(i32, i64, String, u64)> = Vec::new();
    let mut visited = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = roots.iter().map(|r| (home.join(r), 0)).collect();
    while let Some((dir, depth)) = stack.pop() {
        // 40k entries: enough to reach a hand-rolled tool's folder in
        // Application Support (which alone holds tens of thousands), while still
        // bounding the request to a second or two.
        if depth > 4 || visited > 40_000 || hits.len() > 60 { break; }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            visited += 1;
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            // Same deny list as the discovery scan: this walk opens directories
            // too, so it can be blocked by the very same protected store.
            if skip.iter().any(|s| name == *s) || skip_dir(&name) { continue; }
            // A `/Caches/` anywhere in the path is a cache too, whatever it is
            // called — that is where the first draft's only hit came from.
            if p.to_string_lossy().contains("/Caches/") { continue; }
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                if depth < 4 { stack.push((p, depth + 1)); }
                continue;
            }
            let size = md.len();
            if size == 0 || size > 40 * 1024 * 1024 { continue; }
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            if !matches!(ext.as_str(), "jsonl" | "json" | "log" | "ndjson" | "txt") { continue; }
            let mtime = md.modified().ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64).unwrap_or(0);
            if mtime < cutoff { continue; }
            let mut score = 0;
            if name.contains("session") || name.contains("usage") || name.contains("token") { score += 2; }
            let head = {
                use std::io::Read;
                let mut buf = vec![0u8; 8192];
                std::fs::File::open(&p).and_then(|mut f| f.read(&mut buf)).map(|n| { buf.truncate(n); buf }).unwrap_or_default()
            };
            let text = String::from_utf8_lossy(&head);
            let matched = want_keys.iter().filter(|k| text.contains(*k)).count();
            score += matched as i32 * 3;
            if score <= 2 { continue; }
            hits.push((score, mtime, p.to_string_lossy().to_string(), size));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let list: Vec<Value> = hits.iter().take(15).map(|(s, t, p, sz)| json!({
        "path": p.replacen(&*home.to_string_lossy(), "~", 1),
        "score": s, "size": sz, "mtime_ms": t,
    })).collect();
    json_ok(json!({"ok": true, "candidates": list, "visited": visited}))
}

/// Force a full re-read of every watched transcript file. Safe: the SQLite
/// UNIQUE(a,m,t,i,o,c,s,p) constraint dedups everything already recorded, so
/// only genuinely new lines land. Useful after restoring files, changing
/// CLAUDE_CONFIG_DIR/CODEX_HOME, or upgrading parsers.
pub(crate) async fn api_rescan(State(sh): State<Shared>) -> impl IntoResponse {
    let mut n = 0usize;
    {
        let mut fs = lock(&sh.0.files_state);
        for st in fs.values_mut() {
            st.off = 0;
            n += 1;
        }
    }
    // per-file message-id dedup must be cleared or re-read lines are skipped
    lock(&sh.0.seen).clear();
    // offsets are re-persisted by the tick loop on the next pass
    let body = serde_json::to_vec(&json!({"ok": true, "files_reset": n})).unwrap();
    ([(header::CONTENT_TYPE, "application/json; charset=utf-8"),
      (header::CACHE_CONTROL, "no-store")], body)
}


/// `POST /api/scan/start` — 重新扫描：重置读取位置 → 走一遍三层发现 → 报告结果。
///
/// 一次性把"重新扫描数据源"和"添加数据源"合并成这一件事：用户不需要输入名字或路径，
/// 需要知道的只是"现在有哪些工具在被统计"以及"这次扫描发现了什么"。
pub(crate) async fn api_scan_start(State(sh): State<Shared>) -> impl IntoResponse {
    {
        let mut s = lock(&sh.0.scan);
        if s.running {
            return json_ok(json!({"ok": false, "running": true}));
        }
        let before = lock(&sh.0.records).len() as i64;
        *s = ScanRun { running: true, started_at: now_ms(), phase: "准备中…".into(),
                       records_before: before, ..Default::default() };
    }
    let st = sh.clone();
    tokio::spawn(async move { run_scan(st).await; });
    json_ok(json!({"ok": true, "started": true}))
}

pub(crate) fn scan_log(sh: &Shared, line: impl Into<String>) {
    let mut s = lock(&sh.0.scan);
    s.log.push(line.into());
    if s.log.len() > 300 { s.log.drain(0..100); }
}

pub(crate) fn scan_phase(sh: &Shared, phase: &str) {
    let mut s = lock(&sh.0.scan);
    s.phase = phase.to_string();
    s.log.push(format!("… {phase}"));
}

/// 后台任务本体。分三步，每一步都写一行日志（页面就是把这十行左右滚动出来）：
///   ① 重置每个转录文件的读取位置（历史重新读一遍）
///   ② 三层发现（名单已知路径 → 按名字 → 按内容递归扫）
///   ③ 汇总：新增数据源、命中明细、当前记录数
pub(crate) async fn run_scan(sh: Shared) {
    // ① 重置偏移
    let files = {
        let mut fs = lock(&sh.0.files_state);
        for st in fs.values_mut() { st.off = 0; }
        fs.len() as u64
    };
    lock(&sh.0.seen).clear();          // 逐文件去重表也要清，否则重读的行会被跳过
    {
        let mut s = lock(&sh.0.scan);
        s.files_reset = files;
    }
    scan_log(&sh, format!("① 重置 {files} 个转录文件的读取位置，历史重新读入（后台按秒推进）"));

    // ② 三层发现
    scan_phase(&sh, "三层发现：① 名单已知路径 ② 按名字去找 ③ 按内容递归扫描");
    let rows = offload(|| discover_tiered(true)).await.unwrap_or_default();
    for r in &rows {
        let name = r["name"].as_str().unwrap_or("?");
        let tier = r["tier"].as_u64().unwrap_or(0);
        let files = r["files"].as_u64().unwrap_or(0);
        let lines = r["lines"].as_u64().unwrap_or(0);
        let added = r["added"].as_bool().unwrap_or(false);
        let owner = r["claimed_by"].as_str().unwrap_or("");
        let how = if !owner.is_empty() { format!("已由 {owner} 覆盖") }
                  else if added { "新收录".to_string() }
                  else { "已收录".to_string() };
        scan_log(&sh, format!("   · T{tier} {name}：{files} 个文件 / {lines} 行可解析（{how}）"));
    }
    let added: Vec<String> = rows.iter().filter(|r| r["added"].as_bool().unwrap_or(false))
        .map(|r| r["name"].as_str().unwrap_or("?").to_string()).collect();

    // ③ 汇总（记录数还在后台增长，status 里会继续更新）
    scan_phase(&sh, "重新读取转录并入库");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let after = lock(&sh.0.records).len() as i64;
    if added.is_empty() {
        scan_log(&sh, "✓ 完成：没有发现新的可解析数据源".to_string());
    } else {
        scan_log(&sh, format!("✓ 完成：新收录 {} 个数据源（{}）", added.len(), added.join("、")));
    }
    // 先把 before 读出来再记日志：在 scan_log 的参数里 lock(scan) 会和 scan_log 自己
    // 要拿的那把锁撞上（同一把锁、同一个语句内 → 死锁）。
    let before_n = lock(&sh.0.scan).records_before;
    let delta = after - before_n;
    scan_log(&sh, format!("   当前记录数 {after}（{}{} 条）",
        if delta >= 0 { "+" } else { "" }, delta));
    {
        let mut s = lock(&sh.0.scan);
        s.running = false;
        s.finished_at = now_ms();
        s.phase = "完成".into();
        s.records_after = after;
        s.report = json!({
            "hits": rows,
            "added": added,
            "files_reset": files,
            "records_before": s.records_before,
            "records_after": after,
        });
    }
}

/// `GET /api/scan/status` — 页面每 ~0.7 秒取一次：阶段、日志、报告、当前记录数。
pub(crate) async fn api_scan_status(State(sh): State<Shared>) -> impl IntoResponse {
    let s = lock(&sh.0.scan);
    let records = lock(&sh.0.records).len() as i64;
    let elapsed = if s.running {
        ((now_ms() - s.started_at) as f64 / 1000.0).round()
    } else if s.finished_at > 0 {
        ((s.finished_at - s.started_at) as f64 / 1000.0).round()
    } else { 0.0 };
    json_ok(json!({
        "running": s.running,
        "phase": s.phase,
        "log": s.log,
        "report": s.report,
        "files_reset": s.files_reset,
        "records": records,
        "elapsed_s": elapsed,
    }))
}

pub(crate) async fn not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "not found")
}
