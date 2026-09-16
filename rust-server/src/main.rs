// TokenDance server — Rust + SQLite edition (macOS only)
//
// Sources of truth: each agent's local transcript files. SQLite (~/.tokendance/
// tokendance.db) is the durable accumulation layer: records, per-file offsets,
// codex meta and the opencode baseline all survive restarts, so startup is an
// incremental read instead of a full rescan.
//
// Modes:
//   tokendance-server            serve on 127.0.0.1:8737
//   tokendance-server --port N   serve on custom port
//   tokendance-server --db PATH  custom SQLite path
//   tokendance-server --scan     full scan, print JSON to stdout, exit (no DB)
//   tokendance-server --discover list every source the scan can find, exit
//
// Agent ids are the index into `AGENT_NAMES` (model.rs); the parsers are in
// parsers.rs, one function per agent.
//
// Layout (this file is the wiring; everything else is a module):
//   model.rs     data types and shared constants
//   state.rs     the process-wide AppState plus the lock/offload helpers
//   custom.rs    user-added sources: config file, path validation, storage
//   parsers.rs   transcript line -> Record, one function per agent
//   store.rs     SQLite schema, migrations, and every read/write of it
//   ingest.rs    incremental tailing of every watched file
//   discover.rs  where a source may live, the registry, the three-tier search
//   live.rs      today / burn-rate aggregation
//   api.rs       the HTTP surface and the manual rescan orchestration

use axum::routing::{get, post};
use axum::Router;
use chrono::Utc;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;
use tokio::sync::broadcast;

mod model;
mod state;
mod custom;
mod parsers;
mod store;
mod ingest;
mod live;
mod discover;
mod api;

pub(crate) use model::*;
pub(crate) use state::*;
pub(crate) use custom::*;
pub(crate) use parsers::*;
pub(crate) use store::*;
pub(crate) use ingest::*;
pub(crate) use live::*;
pub(crate) use discover::*;
pub(crate) use api::*;




#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut port: u16 = 8737;
    let mut scan_only = false;
    let mut discover = false;
    let mut blind = false;
    let mut as_json = false;
    let mut db_path = PathBuf::from(
        std::env::var("HOME").unwrap_or_default()).join(".tokendance/tokendance.db");
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--scan" => scan_only = true,
            // One-shot discovery, run by the binary itself: `--discover` reports
            // what the shipped scanner finds and changes nothing; `--blind`
            // adds nothing we already know, which answers "what would this find
            // with no built-in list at all"; `--json` for machines.
            "--discover" => discover = true,
            "--blind" => blind = true,
            "--json" => as_json = true,
            "--port" => {
                i += 1;
                port = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(8737);
            }
            "--db" => {
                i += 1;
                if let Some(p) = args.get(i) { db_path = PathBuf::from(p); }
            }
            _ => {}
        }
        i += 1;
    }

    if discover {
        let rows = if blind { discover_scan(true) } else { discover_tiered(false) };
        if as_json {
            println!("{}", serde_json::to_string_pretty(&json!({
                "blind": blind,
                "covered_prefixes": if blind { Vec::new() } else { covered_prefixes() },
                "groups": rows })).unwrap_or_default());
        } else {
            println!("discover{}: {} group(s) parsed usage",
                     if blind { " (blind — ignoring everything we already know)" } else { "" }, rows.len());
            for r in &rows {
                let who = r["claimed_by"].as_str();
                println!("  {:<24} files={:<4} lines={:<6} {:>7.1} MB  {:<9} {}{}",
                         if r["tier"].is_null() { r["group"].as_str().unwrap_or("").to_string() }
                         else { format!("T{} {}", r["tier"].as_u64().unwrap_or(0), r["name"].as_str().unwrap_or("")) },
                         r["files"].as_u64().unwrap_or(0),
                         r["lines"].as_u64().unwrap_or(0),
                         r["bytes"].as_u64().unwrap_or(0) as f64 / 1e6,
                         r["path"].as_str().unwrap_or(r["location"].as_str().unwrap_or("")),
                         if who.is_some() { "已由 " } else { "新增 → " },
                         who.unwrap_or(""));
            }
        }
        return;
    }
    if scan_only {
        // full read-only scan, zero-persisted-state, no DB writes
        let mut files_state = HashMap::new();
        let mut meta = HashMap::new();
        let mut seen = HashMap::new();
        eprintln!("scanning…");
        let t0 = std::time::Instant::now();
        let (mut recs, _, _) = tick(
            &mut files_state, &mut meta, &mut seen,
            &AtomicU64::new(999_000), &mut HashMap::new());
        // initial=false: with a throwaway empty baseline this emits each session's
        // cumulative totals once, which is what a read-only scan is supposed to show
        let (oc_recs, _) = tick_opencode(&mut HashMap::new(), false);
        recs.extend(oc_recs);
        recs.sort_by_key(|r| r.t);
        eprintln!("loaded {} records in {:.1} ms", recs.len(), t0.elapsed().as_millis());
        let gen = Utc::now().with_timezone(&cst()).format("%Y-%m-%dT%H:%M:%S").to_string();
        println!("{}", serde_json::to_string(&json!({"generated_at": gen, "records": recs})).unwrap());
        return;
    }

    let store = match Store::open(&db_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("FATAL: cannot open SQLite store at {}: {e}", db_path.display());
            std::process::exit(1);
        }
    };

    // fast startup: history from SQLite, per-file offsets persisted
    let t0 = std::time::Instant::now();
    let mut recs = store.load_records();
    let db_had_state = !recs.is_empty();
    let mut files_state = store.load_file_state();
    let mut meta: HashMap<String, CodexMeta> = store.load_kv("codex_meta")
        .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let mut seen: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    // Claude Code project directory → readable project label (the longest common
    // prefix of that project's cwds). Persisted so a tail-only incremental read
    // cannot restart the label from a subdirectory.
    let mut proj_roots: HashMap<String, String> = store.load_kv("proj_roots")
        .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let mut oc_state: HashMap<String, Vec<i64>> = store.load_kv("oc_state")
        .map(|s| oc_state_parse(&s)).unwrap_or_default();
    // "baseline only, do not emit" is needed exactly when we have no persisted
    // baseline AND rows already exist — re-reporting there would double count.
    // With an empty store (fresh install, or right after the opencode migration
    // dropped both the rows and the baseline) we *do* want the cumulative totals
    // emitted once, so the existing history shows up instead of vanishing.
    // Agent id 4 is OpenCode (it was 5 before Gemini was retired in ㉜ — the
    // stale `== 5` made this test look at pi instead). With the baseline gone
    // *and* no OpenCode rows left, the scan re-emits the cumulative totals as a
    // single record, which is exactly what `parser_repair_v7` relies on.
    let oc_initial = store.load_kv("oc_state").is_none() && recs.iter().any(|r| r.a == 4);
    eprintln!("scanning… (db loaded {} records)", recs.len());
    let (new, dirty, roots_changed) = tick(
        &mut files_state, &mut meta, &mut seen,
        &AtomicU64::new(999_000), &mut proj_roots);
    let (oc_new, oc_changed) = tick_opencode(&mut oc_state, oc_initial);
    store.save_file_states(&dirty);
    if !roots_changed.is_empty() {
        store.save_kv("proj_roots", &serde_json::to_string(&proj_roots).unwrap_or_default());
    }
    if oc_changed {
        store.save_kv("oc_state", &oc_state_json(&oc_state));
    }
    let mut fresh = Vec::new();
    for r in new.into_iter().chain(oc_new.into_iter()) {
        let r = sane(r);   // the in-memory copy is clamped too, not just the row
        if store.insert(&r) {
            fresh.push(r);
        }
    }
    if db_had_state && !fresh.is_empty() {
        eprintln!("incremental: +{} new records", fresh.len());
    }
    recs.extend(fresh.iter().cloned());
    recs.sort_by_key(|r| r.t);
    eprintln!("ready: {} records in {:.1} ms", recs.len(), t0.elapsed().as_millis());

    // Bundled pages resolve in a fixed order: a user override in
    // ~/.tokendance/ wins, then the working directory (dev runs), then the
    // app bundle. Same order for every page — one helper, not three copies.
    fn read_page(name: &str) -> Vec<u8> {
        let home = std::env::var("HOME").unwrap_or_default();
        std::fs::read(Path::new(&home).join(format!(".tokendance/{name}")))
            .or_else(|_| std::fs::read(name))
            .or_else(|_| {
                std::fs::read(format!("/Applications/TokenDance.app/Contents/Resources/{name}"))
            })
            .unwrap_or_else(|_| format!("<h1>TokenDance {name}</h1>").into_bytes())
    }
    let dashboard = read_page("dashboard.html");
    let settings_html = read_page("settings.html");
    let about_html = read_page("about.html");
    let sources_html = read_page("sources.html");

    // Third-party assets the pages need (`vendor/chart.umd.min.js`), read once
    // at startup like the pages above. Same three-step resolution, except that
    // a missing asset resolves to *nothing* — the route then answers 404
    // instead of handing a browser an HTML placeholder where JS was expected.
    fn read_asset(rel: &str) -> Vec<u8> {
        let home = std::env::var("HOME").unwrap_or_default();
        std::fs::read(Path::new(&home).join(format!(".tokendance/{rel}")))
            .or_else(|_| std::fs::read(rel))
            .or_else(|_| {
                std::fs::read(format!("/Applications/TokenDance.app/Contents/Resources/{rel}"))
            })
            .unwrap_or_default()
    }
    // An explicit list, never a path built from the request URL: the route takes
    // no name parameter, so there is nothing to walk out of the directory with.
    const ASSETS: [&str; 1] = ["chart.umd.min.js"];
    let assets: HashMap<String, Vec<u8>> = ASSETS
        .iter()
        .map(|n| (format!("vendor/{n}"), read_asset(&format!("vendor/{n}"))))
        .collect();
    if assets.values().any(|v| v.is_empty()) {
        eprintln!("WARNING: a vendored asset is missing from this build (see vendor/README.md)");
    }

    // prefs: stored once, consumed by both UIs
    let prefs: Value = store
        .load_kv("prefs")
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({"lang": "sys", "theme": "auto"}));

    let (tx, _) = broadcast::channel::<Vec<Record>>(64);
    let state = Arc::new(AppState {
        records: Mutex::new(recs),
        records_gen: AtomicU64::new(0),
        data_cache: Mutex::new((u64::MAX, axum::body::Bytes::new())),
        files_state: Mutex::new(files_state),
        meta: Mutex::new(meta),
        seen: Mutex::new(seen),
        oc_state: Mutex::new(oc_state),
        proj_roots: Mutex::new(proj_roots),
        scan: Arc::new(Mutex::new(ScanRun::default())),
        latest_write_ago_ms: AtomicU64::new(999_000),
        tx,
        dashboard,
        settings_html,
        about_html,
        sources_html,
        assets,
        prefs: Mutex::new(prefs),
        store: store.clone(),
    });
    let sh = Shared(state.clone());

    // Automatic discovery: no name required. Runs shortly after start-up (so the
    // first scan is not competing with it) and every six hours after that.
    {
        let st = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(12)).await;
            loop {
                // offloaded for the same reason as the request path: a scan
                // that blocks a worker starves the accept loop (see `offload`)
                let added = offload(discover_by_content).await.unwrap_or_default();
                if !added.is_empty() {
                    eprintln!("discovered {} source(s): {}", added.len(), added.join("; "));
                    reset_custom_rows(&st);
                    {
                        let mut fs = lock(&st.files_state);
                        fs.retain(|_, x| x.agent < 10);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
            }
        });
    }

    // background tick loop, 1s cadence
    {
        let st = state.clone();
        let store = store.clone();
        tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                // take maps out (no per-second clone), scan, then put updated maps back
                let (mut fs, mut mt, mut sn, mut oc, mut pr) = {
                    let mut a = lock(&st.files_state);
                    let mut b = lock(&st.meta);
                    let mut c = lock(&st.seen);
                    let mut d = lock(&st.oc_state);
                    let mut e = lock(&st.proj_roots);
                    (std::mem::take(&mut *a), std::mem::take(&mut *b),
                     std::mem::take(&mut *c), std::mem::take(&mut *d),
                     std::mem::take(&mut *e))
                };
                let (new, dirty, roots_changed) =
                    tick(&mut fs, &mut mt, &mut sn, &st.latest_write_ago_ms, &mut pr);
                let (oc_new, oc_changed) = tick_opencode(&mut oc, false);
                {
                    let mut a = lock(&st.files_state);
                    *a = fs;
                    let mut b = lock(&st.meta);
                    *b = mt;
                    let mut c = lock(&st.seen);
                    *c = sn;
                    let mut d = lock(&st.oc_state);
                    *d = oc;
                    let mut e = lock(&st.proj_roots);
                    *e = pr;
                }
                if !dirty.is_empty() {
                    store.save_file_states(&dirty);
                }
                if !roots_changed.is_empty() {
                    let snapshot = lock(&st.proj_roots).clone();
                    store.save_kv("proj_roots",
                                  &serde_json::to_string(&snapshot).unwrap_or_default());
                }
                if oc_changed {
                    let snapshot = lock(&st.oc_state).clone();
                    store.save_kv("oc_state", &oc_state_json(&snapshot));
                }
                let mut kept = Vec::new();
                if !new.is_empty() || !oc_new.is_empty() {
                    for r in new.into_iter().chain(oc_new.into_iter()) {
                        let r = sane(r);   // ditto: memory and disk see the same value
                        if store.insert(&r) {
                            kept.push(r);
                        }
                    }
                    // codex meta evolves with transcripts — persist for next start
                    let snapshot = lock(&st.meta).clone();
                    store.save_kv("codex_meta", &serde_json::to_string(&snapshot).unwrap_or_default());
                }
                if !kept.is_empty() {
                    {
                        let mut recs = lock(&st.records);
                        recs.extend(kept.iter().cloned());
                        recs.sort_by_key(|r| r.t);
                    }
                    // invalidate the /api/data body cache
                    st.records_gen.fetch_add(1, Ordering::Relaxed);
                    let _ = st.tx.send(kept);
                }
                n += 1;
                if n % 600 == 0 {
                    // hot-window pruning: SQLite keeps everything, memory sheds
                    let cutoff = now_ms() - KEEP_DAYS * 86_400_000;
                    {
                        let mut recs = lock(&st.records);
                        recs.retain(|r| r.t >= cutoff);
                    }
                    st.records_gen.fetch_add(1, Ordering::Relaxed);
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
    }

    let app = Router::new()
        .route("/api/data", get(api_data))
        .route("/api/records", get(api_records))
        .route("/api/live", get(api_live))
        .route("/api/stream", get(api_stream))
        .route("/api/sources", get(api_sources))
        .route("/api/custom", get(api_custom_get).post(api_custom_post))
        .route("/api/custom/validate", post(api_custom_validate))
        .route("/api/custom/probe", get(api_custom_probe).post(api_custom_add_by_name))
        .route("/api/custom/remove", post(api_custom_remove))
        .route("/api/custom/discover", post(api_custom_discover))
        .route("/api/custom/find", get(api_custom_find))
        .route("/api/registry", get(api_registry_get))
        .route("/api/registry/update", post(api_registry_update))
        .route("/api/rescan", post(api_rescan))
        .route("/api/scan/start", post(api_scan_start))
        .route("/api/scan/status", get(api_scan_status))
        .route("/api/settings", get(api_settings_get).post(api_settings_post))
        .route("/settings", get(settings_page))
        .route("/about", get(about_page))
        .route("/sources", get(sources_page))
        .route("/vendor/chart.umd.min.js", get(vendor_chart_js))
        .route("/", get(index))
        .fallback(not_found)
        .with_state(sh);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    eprintln!("serving on http://127.0.0.1:{} (db: {})", port, db_path.display());
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ---------- tests: synthetic fixtures for every agent parser ----------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use serde_json::json;

    fn rec(a: u8, m: &str, t: i64, i: i64, o: i64, c: i64, s: &str, p: &str) -> Record {
        Record { a, m: m.into(), t, i, o, c, s: s.into(), p: p.into() }
    }

    /// A corrupt transcript cannot turn every later total into a negative number.
    ///
    /// Two things have to hold, and they are different defences: the clamp stops
    /// absurd values from ever *entering* (a hand-edited file, or a field that
    /// means bytes), and saturating addition means that even a value that got in
    /// anyway cannot wrap. Measured before either existed: two records of
    /// 5·10^18 made `today.t` −8,446,744,073,709,551,616, and the app crashed
    /// reading it (STATUS.md 58).
    #[test]
    fn absurd_token_counts_are_clamped_and_sums_saturate() {
        // the gate: a record straight off a hostile line
        let hostile = sane(rec(1, "codex", 1000, i64::MAX, i64::MAX, -5, "s", "p"));
        assert_eq!(hostile.i, MAX_RECORD_TOKENS);
        assert_eq!(hostile.o, MAX_RECORD_TOKENS);
        assert_eq!(hostile.c, 0, "a negative count is not a count");

        // …and a real record is untouched, to the token
        let real = sane(rec(1, "codex", 1000, 3_126_357, 47_073, 3_103_488, "s", "p"));
        assert_eq!((real.i, real.o, real.c), (3_126_357, 47_073, 3_103_488));

        // the accumulator: values that are already clamped, summed
        let now = 1_700_000_000_000;
        let day = vec![
            rec(1, "codex", now - 3000, MAX_RECORD_TOKENS, 0, 0, "s", "p"),
            rec(1, "codex", now - 2000, MAX_RECORD_TOKENS, 0, 0, "s", "p"),
            rec(1, "codex", now - 1000, MAX_RECORD_TOKENS, 0, 0, "s", "p"),
        ];
        let v = live_stats_from(&day, now, 0);
        assert_eq!(v["today"]["t"], json!(3 * MAX_RECORD_TOKENS));
        assert_eq!(v["burn60"], json!(3 * MAX_RECORD_TOKENS));
        // per-agent totals are indexed by agent id (1 = Codex), not by position
        assert_eq!(v["today_agents"][1][1], json!(3 * MAX_RECORD_TOKENS));

        // and the belt: a value that bypassed the clamp saturates instead of
        // flipping sign — note `t` is `i + o`, so both halves matter
        let bypass = vec![
            rec(1, "codex", now - 2000, i64::MAX, i64::MAX, 0, "s", "p"),
            rec(1, "codex", now - 1000, i64::MAX, i64::MAX, 0, "s", "p"),
        ];
        let v2 = live_stats_from(&bypass, now, 0);
        assert_eq!(v2["today"]["t"], json!(i64::MAX), "saturated, never negative");
        assert_eq!(v2["today"]["i"], json!(i64::MAX));
        assert_eq!(v2["burn60"], json!(i64::MAX));
        // The 60 s buckets saturate on their own (these two records are a second
        // apart, so they land in two different buckets) — the invariant that
        // survives is "nothing wrapped". With plain `+` these went negative,
        // which is what a chart diving below zero looked like.
        let buckets: Vec<i64> = v2["buckets"].as_array().unwrap().iter()
            .map(|b| b.as_i64().unwrap()).collect();
        assert!(buckets.iter().all(|b| *b >= 0), "a bucket wrapped: {buckets:?}");
        assert_eq!(buckets.iter().filter(|b| **b > 0).count(), 2);
    }

    /// A row written before the clamp existed is tamed on the way back in.
    #[test]
    fn stored_absurd_rows_are_clamped_on_load() {
        let store = Store::in_memory().unwrap();
        // insert the row the way the build *before* the clamp would have: it went
        // straight to SQLite, so the loader has to be the one that tames it
        lock(&store.conn).execute(
            "INSERT INTO records(a,m,t,i,o,c,s,p) VALUES(1,'codex',1000,?1,5,0,'s','p')",
            params![i64::MAX],
        ).unwrap();
        let back = store.load_records();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].i, MAX_RECORD_TOKENS);
        assert_eq!(back[0].o, 5);
    }

    /// macOS answers a read of `~/Library/Application Support/AddressBook` with a
    /// **Contacts permission request**, not with an error — which is why the app
    /// used to ask for contacts it has no business touching (and why one run sat
    /// in `open$NOCANCEL` for four minutes; see PRIVATE_DIRS).
    ///
    /// This builds a stand-in tree in the temp directory, so the test never goes
    /// near the real store, and asserts both halves of the guard: a private
    /// directory is not descended into, and a private directory handed in as a
    /// root is refused outright.
    #[test]
    fn the_scan_never_opens_a_private_store() {
        let usage = "{\"timestamp\":\"2026-09-15T10:00:00Z\",\"model\":\"m\",\"session_id\":\"s\",\
                     \"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":250,\"cached_tokens\":100}}\n";
        let base = std::env::temp_dir().join(format!("tokendance-priv-{}-{}",
                                                     std::process::id(), now_ms()));
        let support = base.join("Library/Application Support");
        let mut made: Vec<PathBuf> = Vec::new();
        for name in ["AddressBook", "Mail", "com.apple.TCC", "Messages", "Safari"] {
            let d = support.join(name).join("sessions");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("usage.jsonl"), usage).unwrap();
            made.push(support.join(name));
        }
        let real = support.join("RealTool/sessions");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("usage.jsonl"), usage).unwrap();

        // The names themselves are the contract, so a rename in the list cannot
        // silently drop one of them.
        for name in ["AddressBook", "Mail", "Messages", "Safari", "CloudDocs",
                     "FileProvider", "MobileSync", "CallHistoryDB", "com.apple.TCC",
                     "com.apple.Safari", "Google", "Microsoft"] {
            assert!(skip_dir(name), "{name} must not be walked into");
        }
        assert!(!skip_dir("Goose") && !skip_dir("Application Support"));

        // Walking the parent finds the ordinary tool and nothing else.
        let found = scan_location(&base.to_string_lossy(), 4_000);
        let files: u64 = found.iter().map(|(_, f, _, _, _, _)| *f).sum();
        assert_eq!(files, 1,
                   "expected exactly the one ordinary tool's file, got {found:?}");
        let example = found[0].5.clone();
        assert!(example.contains("RealTool"), "wrong file reported: {example}");
        for name in ["AddressBook", "Mail", "com.apple.TCC", "Messages", "Safari"] {
            assert!(!example.contains(name), "a private store was read: {example}");
        }

        // Handed a private directory as the *root*, the scan declines politely.
        for name in ["AddressBook", "Mail", "com.apple.TCC"] {
            let p = support.join(name);
            assert!(scan_location(&p.to_string_lossy(), 4_000).is_empty(),
                    "{name} was opened even though it was given as a scan root");
            assert!(private_path(&p.to_string_lossy()));
        }
        // ...while a legitimate path whose *children* happen to be noisy still
        // works: `dist` is in the skip list but is not private.
        assert!(!private_path("/Users/x/Library/Application Support/SomeApp/dist/logs"));
        assert!(private_path("/Users/x/Library/Application Support/AddressBook"));

        // The name search must not even *look*: `probe_roots` builds candidate
        // paths from the typed name and stats the ones that exist, which is how
        // "AddressBook" used to reach the real store before the scanner ran.
        let fake_home = base.join("fakehome");
        std::fs::create_dir_all(fake_home.join("Library/Application Support/AddressBook")).unwrap();
        std::fs::create_dir_all(fake_home.join("Library/Application Support/RealTool")).unwrap();
        let h = fake_home.to_string_lossy().to_string();
        let private = candidate_roots(&h, &["AddressBook".to_string()]);
        assert!(private.is_empty(), "a private store became a probe root: {private:?}");
        let normal = candidate_roots(&h, &["RealTool".to_string()]);
        assert_eq!(normal.len(), 1, "an ordinary tool should still be found: {normal:?}");
        assert!(normal[0].ends_with("Application Support/RealTool"));
        // ...and the manual-path route says no before it stats anything.
        assert!(validate_path(&format!("{h}/Library/Application Support/AddressBook/*.jsonl"))
                    .is_err());

        for p in made { let _ = std::fs::remove_dir_all(p); }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// One directory must produce one project name, whichever agent wrote it.
    ///
    /// The user found this in the project filter: the same repository was listed
    /// twice — once as Codex's `2024-01-15-09-30-00/example-repo` (last two path
    /// components) and once as pi's `~/WorkBuddy/2024-01-15-09-30-00/example-repo`
    /// (home-shortened full path). It was not a data problem: two parsers had two
    /// rules for the same field.
    #[test]
    fn one_directory_gets_one_project_name_from_every_parser() {
        let home = home_dir();
        assert!(!home.is_empty(), "this test needs a home directory");
        let repo = format!("{home}/WorkBuddy/2024-01-15-09-30-00/example-repo");
        let want = "~/WorkBuddy/2024-01-15-09-30-00/example-repo";
        assert_eq!(home_short(&repo), want);

        // Codex: the session meta carries the cwd, in either spelling.
        let mut meta = HashMap::new();
        let key = "1|rollout-2026-09-15T10-00-00-01a0a412-270d-7471-b093-1e298e282ab1.jsonl";
        for spelling in [repo.clone(), format!("file://{repo}")] {
            let d = json!({
                "type": "event_msg",
                "payload": {"type": "session_meta", "cwd": spelling, "model": "gpt-x"}
            });
            codex_line(&d, key, &mut meta).is_none();   // meta line: no usage
            meta.entry(key.to_string()).or_insert_with(|| CodexMeta { cwd: None, model: None, provider: None });
            let usage = json!({
                "type": "event_msg", "timestamp": "2026-09-15T10:00:01Z",
                "payload": {"type": "token_count", "info": {"last_token_usage": {
                    "input_tokens": 10, "output_tokens": 5, "cached_input_tokens": 0}}}
            });
            let rec = codex_line(&usage, key, &mut meta).expect("usage line parses");
            assert_eq!(rec.p, want, "codex labelled {spelling} as {}", rec.p);
        }

        // pi: cwd comes from the first line of the transcript and is what the
        // usage lines are labelled with.
        let mut pmeta = HashMap::new();
        let pi_path = Path::new("/tmp/--Users-alice-WorkBuddy--/2026-09-15T10-00-00_x.jsonl");
        let pkey = "5|2026-09-15T10-00-00_x.jsonl";
        let pline = json!({"type": "session", "cwd": repo, "timestamp": "2026-09-15T10:00:00Z"});
        pi_line(&pline, pi_path, 0, &mut pmeta, pkey).is_none();   // session header: no usage
        let puse = json!({
            "type": "assistant", "timestamp": "2026-09-15T10:00:05Z", "model": "pi-model",
            "usage": {"input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0}
        });
        let prec = pi_line(&puse, pi_path, 0, &mut pmeta, pkey).expect("pi usage parses");
        assert_eq!(prec.p, want, "pi labelled the same directory differently");

        // WorkBuddy: the cwd wins over the folder tail, which is only a fallback.
        let wb_dir = Path::new("/tmp/Users-alice-WorkBuddy-2024-01-15-09-30-00/x.jsonl");
        let with_cwd = json!({
            "timestamp": "2026-09-15T10:00:00+08:00", "cwd": repo,
            "providerData": {"model": "m", "usage": {"inputTokens": 5, "outputTokens": 2}}
        });
        let custom = CustomFile { version: 1, sources: Vec::new(), removed: Vec::new(),
            discovered: Vec::new() };
        let r = parse_line(0, &with_cwd, wb_dir, &mut HashMap::new(), 0, &custom)
            .expect("workbuddy line parses");
        assert_eq!(r.p, want, "workbuddy should use the cwd it wrote");
        let no_cwd = json!({
            "timestamp": "2026-09-15T10:00:00+08:00",
            "providerData": {"model": "m", "usage": {"inputTokens": 5, "outputTokens": 2}}
        });
        let r = parse_line(0, &no_cwd, wb_dir, &mut HashMap::new(), 0, &custom)
            .expect("workbuddy line without cwd");
        assert_eq!(r.p, "2024-01-15-09-30-00", "the folder tail remains the fallback");

        // ...and the old Codex shape must be gone for good.
        assert_ne!(home_short(&repo), "2024-01-15-09-30-00/example-repo");
    }

    #[test]
    fn wb_line_parses_provider_data() {
        let d = json!({
            "timestamp": "2026-09-13T10:00:00+08:00",
            "providerData": {"model": "glm-5", "usage": {
                "inputTokens": 1000, "outputTokens": 200,
                "inputTokensDetails": [{"cached_tokens": 800}]}},
            "message": {"usage": {"inputTokens": 1000, "outputTokens": 200}}
        });
        let (i, o, c, m, _ms) = wb_line(&d).unwrap();
        assert_eq!((i, o, c, m.as_str()), (1000, 200, 800, "glm-5"));
    }

    #[test]
    fn codex_line_uses_last_token_usage() {
        let mut meta = HashMap::new();
        let d = json!({
            "type": "event_msg", "payload": {"type": "session_meta", "cwd": "/Users/x/proj/app"}
        });
        assert!(codex_line(&d, "1|f.jsonl", &mut meta).is_none());
        let d = json!({
            "timestamp": "2026-09-13T10:00:00+08:00",
            "payload": {"type": "token_count", "info": {
                "last_token_usage": {"input_tokens": 500, "output_tokens": 90, "cached_input_tokens": 450}}}
        });
        let r = codex_line(&d, "1|f.jsonl", &mut meta).unwrap();
        assert_eq!(r.i, 500); assert_eq!(r.o, 90); assert_eq!(r.c, 450);
        // The project is the home-shortened cwd — not its last two components.
        // `/Users/x/...` is not this machine's home, so it survives as written;
        // `one_directory_gets_one_project_name_from_every_parser` covers the
        // `~/...` spelling with the same directory through three parsers.
        assert_eq!(r.p, "/Users/x/proj/app");
    }

    // Regression: the model name is carried on the TOP-LEVEL `turn_context`
    // object, whose payload has no `type` field. Reading `payload.type` meant the
    // condition never fired and every Codex row was stored as "unknown"
    // (measured: 20,330 rows, 81% of the whole store).
    #[test]
    fn codex_model_comes_from_top_level_turn_context() {
        let mut meta = HashMap::new();
        let ctx = json!({
            "type": "turn_context",
            "payload": {"turn_id": "t1", "cwd": "/Users/x/proj/app", "model": "deepseek-v4-flash"}
        });
        assert!(codex_line(&ctx, "1|rollout-x.jsonl", &mut meta).is_none());
        let tc = json!({
            "type": "event_msg", "timestamp": "2026-09-13T10:00:00+08:00",
            "payload": {"type": "token_count", "info": {
                "last_token_usage": {"input_tokens": 10, "output_tokens": 1, "cached_input_tokens": 0}}}
        });
        let r = codex_line(&tc, "1|rollout-x.jsonl", &mut meta).unwrap();
        assert_eq!(r.m, "deepseek-v4-flash");

        // a session may switch models: later turn_context must win
        let ctx2 = json!({"type": "turn_context", "payload": {"model": "glm-5.3-flash"}});
        assert!(codex_line(&ctx2, "1|rollout-x.jsonl", &mut meta).is_none());
        let r = codex_line(&tc, "1|rollout-x.jsonl", &mut meta).unwrap();
        assert_eq!(r.m, "glm-5.3-flash");
    }

    #[test]
    fn codex_model_falls_back_to_provider_then_name() {
        // oldest builds (0.146.x) only record model_provider
        let mut meta = HashMap::new();
        let sm = json!({"type": "session_meta", "payload": {"cwd": "/a/b", "model_provider": "deepseek"}});
        assert!(codex_line(&sm, "1|rollout-y.jsonl", &mut meta).is_none());
        let tc = json!({
            "type": "event_msg", "timestamp": "2026-09-13T10:00:00+08:00",
            "payload": {"type": "token_count", "info": {
                "last_token_usage": {"input_tokens": 7, "output_tokens": 1}}}
        });
        assert_eq!(codex_line(&tc, "1|rollout-y.jsonl", &mut meta).unwrap().m, "deepseek");
        // ...and when there is nothing at all we still must not say "unknown"
        let mut meta2 = HashMap::new();
        assert_eq!(codex_line(&tc, "1|rollout-z.jsonl", &mut meta2).unwrap().m, "codex");
    }

    #[test]
    fn codex_model_from_alternate_carriers() {
        // world_state.payload.state.model
        let mut meta = HashMap::new();
        let ws = json!({"type": "world_state", "payload": {"state": {"model": "deepseek-v4-pro"}}});
        assert!(codex_line(&ws, "1|r.jsonl", &mut meta).is_none());
        let tc = json!({
            "type": "event_msg", "timestamp": "2026-09-13T10:00:00+08:00",
            "payload": {"type": "token_count", "info": {"last_token_usage": {"input_tokens": 3}}}
        });
        assert_eq!(codex_line(&tc, "1|r.jsonl", &mut meta).unwrap().m, "deepseek-v4-pro");
        // event_msg/thread_settings_applied
        let mut meta = HashMap::new();
        let ts = json!({"type": "event_msg",
                        "payload": {"type": "thread_settings_applied",
                                    "thread_settings": {"model": "glm-5.2"}}});
        assert!(codex_line(&ts, "1|r2.jsonl", &mut meta).is_none());
        assert_eq!(codex_line(&tc, "1|r2.jsonl", &mut meta).unwrap().m, "glm-5.2");
    }

    // Regression: the session key used to be a prefix of the file name, which is
    // "rollout-<date>T..." for every single session — 42 real sessions collapsed
    // into 2 keys. A uuid *prefix* is no better: UUIDv7 leads with a millisecond
    // timestamp, so same-millisecond sessions still collide (33 keys for 42).
    #[test]
    fn codex_session_id_uses_uuid_tail_not_prefix() {
        let a = codex_session_id("rollout-2026-09-05T14-00-19-01a07027-4d56-7493-bc60-2edc86a9b7aa.jsonl");
        let b = codex_session_id("rollout-2026-09-05T23-53-53-01a07246-bcb0-74f3-8715-35315640595c.jsonl");
        let c = codex_session_id("rollout-2026-08-02T15-27-24-019fc15e-d281-7af3-8acc-5e0e227bd082.jsonl");
        let d = codex_session_id("rollout-2026-08-02T15-27-24-019fc15e-d283-7221-b7e4-c629371784ca.jsonl");
        assert_eq!(a, "2edc86a9b7aa");
        assert_eq!(b, "35315640595c");
        assert_eq!(c, "5e0e227bd082");
        assert_eq!(d, "c629371784ca");
        // c and d share the leading "019fc15e" timestamp — the whole point
        assert_ne!(c, d);
        // unknown shapes must degrade, never panic
        assert_eq!(codex_session_id("weird.jsonl"), "weird");
        assert_eq!(codex_session_id(""), "");
    }

    // Continuing a thread from the desktop app starts a second rollout file:
    // `rollout-<ts>-<original-uuid>_<new-uuid>.jsonl`. It used to fall through
    // to the non-uuid branch and key on a prefix of the original uuid, so the
    // one conversation was stored (and shown) as two sessions.
    #[test]
    fn codex_session_id_unwraps_a_continued_thread() {
        let original = "rollout-2026-09-05T14-00-19-01a07027-4d56-7493-bc60-2edc86a9b7aa.jsonl";
        let continued = "rollout-2026-09-14T17-33-56-01a07027-4d56-7493-bc60-2edc86a9b7aa_01a09f44-1eba-7772-8677-a13f849ec730.jsonl";
        assert_eq!(codex_session_id(continued), "2edc86a9b7aa");
        // the whole point: the continuation lands on the same session
        assert_eq!(codex_session_id(continued), codex_session_id(original));
        // and not on the prefix fallback it used to produce
        assert_ne!(codex_session_id(continued), "01a07027-4d5");
    }

    // /api/data is assembled by hand around a cached byte array. A wrong quote
    // in that assembly is not a cosmetic slip: it invalidates the payload for
    // the app *and* the dashboard at the same time (this test exists because the
    // agents field was first added with a stray `"` after the array).
    #[test]
    fn data_envelope_is_valid_json() {
        let body = data_envelope("2026-09-14T21:35:12", br#"["WorkBuddy","Codex"]"#, br#"[{"a":0}]"#);
        let v: Value = serde_json::from_slice(&body).expect("envelope must parse");
        assert_eq!(v["generated_at"], "2026-09-14T21:35:12");
        assert_eq!(v["agents"][1], "Codex");
        assert_eq!(v["records"][0]["a"], 0);
        // and it survives an empty record set, which is the first-boot case
        let empty = data_envelope("2026-09-14T21:35:12", b"[]", b"[]");
        let v: Value = serde_json::from_slice(&empty).expect("empty envelope must parse");
        assert!(v["records"].as_array().unwrap().is_empty());
    }

    #[test]
    fn short_id_is_collision_free_across_uuid_versions() {
        // uuidv4 (random everywhere) and uuidv7 (timestamp leader) both work
        assert_eq!(short_id("06f01410-c6c6-406f-aea7-6d9e7c4d7ff0.jsonl"), "6d9e7c4d7ff0");
        assert_eq!(short_id("6f34fee9-cdfd-43ae-9d6d-20d5da3af95b"), "20d5da3af95b");
        assert_ne!(short_id("019fc15e-d281-7af3-8acc-5e0e227bd082"),
                   short_id("019fc15e-d283-7221-b7e4-c629371784ca"));
        // no uuid at all: readable prefix, no panic on non-hex chars
        assert_eq!(short_id("weird.jsonl"), "weird");
        assert_eq!(short_id(""), "");
    }

    #[test]
    fn zero_usage_records_are_noise() {
        assert!(is_noise(&rec(1, "deepseek", 1, 0, 0, 0, "s", "p")));
        // input-only (Claude compaction) and output-only are real
        assert!(!is_noise(&rec(2, "x", 1, 500, 0, 0, "s", "p")));
        assert!(!is_noise(&rec(2, "x", 1, 0, 5, 0, "s", "p")));
        assert!(!is_noise(&rec(2, "x", 1, 0, 0, 3, "s", "p")));
    }

    #[test]
    fn claude_line_sums_cache_creation_into_input() {
        let d = json!({
            "timestamp": "2026-09-13T10:00:00+08:00", "sessionId": "6f34fee9-cdfd-43ae-9d6d-20d5da3af95b",
            "message": {"model": "claude-x", "usage": {
                "input_tokens": 10, "cache_read_input_tokens": 900,
                "cache_creation_input_tokens": 50, "output_tokens": 20}}
        });
        let (i, o, c, m, _ms, s) = claude_line(&d).unwrap();
        assert_eq!((i, o, c), (960, 20, 900)); // input incl. cache_creation
        assert_eq!((m.as_str(), s.as_str()), ("claude-x", "20d5da3af95b"));
    }

    #[test]
    fn gemini_line_skips_set_patches() {
        let d = json!({"$set": {"foo": 1}});
        assert!(gemini_qwen_line(3, &d, Path::new("/x")).is_none());
        let d = json!({
            "id": "m1", "type": "gemini", "timestamp": "2026-09-13T10:00:00+08:00",
            "model": "gemini-3", "tokens": {"input": 300, "output": 40, "cached": 250}
        });
        let r = gemini_qwen_line(3, &d, Path::new("/home/u/.gemini/tmp/abc123def4567890/chats/session-1.jsonl")).unwrap();
        assert_eq!((r.a, r.i, r.o, r.c), (3, 300, 40, 250));
        assert_eq!(r.p, "abc123def4567890");
    }

    // ---------- Claude Code project naming ----------

    #[test]
    fn path_lcp_compares_components_not_characters() {
        // "/a/b/c" and "/a/b/cc" share the *string* prefix "/a/b/c", which is not
        // a directory either path lives in — the prefix must stop at "/a/b".
        assert_eq!(path_lcp("/a/b/c", "/a/b/cc"), "/a/b");
        assert_eq!(path_lcp("/a/b", "/a/b/c"), "/a/b");
        assert_eq!(path_lcp("/a/b", "/a/b"), "/a/b");
        assert_eq!(path_lcp("/a/b", "/x/y"), "");
    }

    #[test]
    fn claude_enc_dir_ignores_session_and_subagent_levels() {
        // Claude Code writes transcripts at four depths. Taking the immediate
        // parent directory labels the last two `subagents` / a session id.
        for fp in [
            "/h/.claude/projects/-srv-app/a.jsonl",
            "/h/.claude/projects/-srv-app/s1/a.jsonl",
            "/h/.claude/projects/-srv-app/s1/subagents/a.jsonl",
            "/h/.claude/projects/-srv-app/s1/subagents/deep/a.jsonl",
        ] {
            assert_eq!(claude_enc_dir(Path::new(fp)).unwrap(), "-srv-app", "for {fp}");
        }
        // The bug this replaced: the immediate parent of a subagent transcript.
        let p = Path::new("/h/.claude/projects/-srv-app/s1/subagents/a.jsonl");
        assert_eq!(p.parent().unwrap().file_name().unwrap().to_str().unwrap(), "subagents");
    }

    #[test]
    fn claude_project_comes_from_the_transcript_cwd() {
        let d = json!({
            "timestamp": "2026-09-13T10:00:00+08:00",
            "sessionId": "6f34fee9-cdfd-43ae-9d6d-20d5da3af95b",
            "cwd": "/srv/app/评测",
            "message": {"model": "claude-x", "usage": {"input_tokens": 5, "output_tokens": 1}}
        });
        let fp = Path::new("/h/.claude/projects/-srv-app---/6f34fee9-cdfd-43ae-9d6d-20d5da3af95b.jsonl");
        let r = parse_line(2, &d, fp, &mut HashMap::new(), 0, &CustomFile::default()).unwrap();
        // The raw cwd, not the encoded directory name: every non-alphanumeric
        // character of a Claude cwd is written as '-', so "-srv-app---" carries
        // no information about the real path.
        assert_eq!(r.p, "/srv/app/评测");
        assert_eq!(r.s, "20d5da3af95b");
    }

    #[test]
    fn resolve_project_folds_a_sessions_subdirectories_into_one_root() {
        let mut roots = HashMap::new();
        let mut changed = Vec::new();
        // Measured shape: one Claude project directory holds the launch cwd plus
        // every subdirectory the agent walked into, so a per-line cwd would put
        // one session into three different "projects".
        let want = home_short("/srv/app/评测");
        for cwd in [
            "/srv/app/评测",
            "/srv/app/评测/frontend",
            "/srv/app/评测/backend",
            "/srv/app/评测",
        ] {
            let got = resolve_project(&mut roots, &mut changed, "-srv-app---", cwd);
            assert_eq!(got.as_deref(), Some(want.as_str()), "for {cwd}");
        }
        assert_eq!(roots.get("-srv-app---").unwrap(), &want);
        assert!(changed.iter().any(|(k, v)| k == "-srv-app---" && v == &want),
                "the resolved root must be reported for persistence");
    }

    #[test]
    fn resolve_project_survives_a_tail_only_read() {
        // A later tick only sees newly appended lines, i.e. subdirectory cwds,
        // never the transcript's first line. Without the remembered root the
        // project would restart from a subdirectory and re-fragment.
        let mut roots = HashMap::new();
        let mut seen_changes = Vec::new();
        let want = home_short("/srv/app/评测");
        resolve_project(&mut roots, &mut seen_changes, "-srv-app---", "/srv/app/评测");

        let mut tail = Vec::new();
        let got = resolve_project(&mut roots, &mut tail, "-srv-app---", "/srv/app/评测/backend");
        assert_eq!(got.as_deref(), Some(want.as_str()));
        assert!(tail.is_empty(), "a tail-only read must not move the root");
    }

    #[test]
    fn resolve_project_never_collapses_to_an_empty_label() {
        let mut roots = HashMap::new();
        let mut changed = Vec::new();
        resolve_project(&mut roots, &mut changed, "-x", "/a/b");
        // Unrelated cwd: the only common prefix is empty, so the known-good root
        // is kept instead of relabelling the project with "".
        assert_eq!(resolve_project(&mut roots, &mut changed, "-x", "/c/d").as_deref(), Some("/a/b"));
        // A line with no cwd at all falls back to the remembered root too.
        assert_eq!(resolve_project(&mut roots, &mut changed, "-x", "").as_deref(), Some("/a/b"));
    }

    /// A synthetic corpus built to be adversarial for ordering: ties are the
    /// norm, not the exception. Every field except the last one in the tiebreak
    /// chain draws from a tiny value space, so the primary key alone is hopeless
    /// — but `c` carries the row number, which keeps the whole tuple unique (as
    /// the real `records` table's UNIQUE constraint does). Without that, two
    /// generated rows could be identical and the totality assertion below would
    /// fail on a correct comparator.
    fn tied_corpus() -> Vec<Record> {
        let mut v = Vec::new();
        for n in 0..96i64 {
            v.push(rec(
                (n % 2) as u8,
                if n % 3 == 0 { "m-a" } else { "m-b" },
                1_700_000_000_000 + (n % 8),
                [0i64, 10, 10][(n % 3) as usize],
                [0i64, 5, 5][(n % 3) as usize],
                n, // unique: the last link in the chain, so ties survive down to it
                &format!("s-{}", n % 3),
                if n % 5 == 0 { "~/p" } else { "~/q" },
            ));
        }
        v
    }

    fn row_key(r: &Record) -> (u8, i64, i64, i64, i64, String, String) {
        (r.a, r.t, r.i, r.o, r.c, r.s.clone(), r.p.clone())
    }

    /// The property that makes `OFFSET n LIMIT m` safe: no two *distinct* rows
    /// may compare equal, or the sort is only partial and the database is free
    /// to return them in any order it likes between two requests.
    #[test]
    fn records_order_is_total_not_merely_primary() {
        let rows = tied_corpus();
        for sort in ["t", "i", "o", "c"] {
            for desc in [false, true] {
                let mut sorted = rows.clone();
                sorted.sort_by(|x, y| records_cmp(x, y, sort, desc));
                for w in sorted.windows(2) {
                    assert_ne!(
                        records_cmp(&w[0], &w[1], sort, desc),
                        std::cmp::Ordering::Equal,
                        "sort={sort} desc={desc}: two distinct rows tied, \
                         which would make paging unstable"
                    );
                }
                // And the sort must be a permutation, not a rewrite.
                let mut before: Vec<_> = rows.iter().map(row_key).collect();
                let mut after: Vec<_> = sorted.iter().map(row_key).collect();
                before.sort();
                after.sort();
                assert_eq!(before, after, "sort={sort} desc={desc}: the sort mutated the corpus");
            }
        }
    }

    /// Paging the whole corpus must yield every row exactly once, for every sort
    /// column and both directions. This is the check that would have caught a
    /// partial order: with one, consecutive pages overlap and leave gaps.
    #[test]
    fn paging_yields_every_row_exactly_once() {
        let rows = tied_corpus();
        for sort in ["t", "i", "o", "c"] {
            for desc in [false, true] {
                let mut sorted = rows.clone();
                sorted.sort_by(|x, y| records_cmp(x, y, sort, desc));

                // Walk the corpus in uneven page sizes; a bug that only shows up
                // when a page boundary lands inside a run of ties would survive
                // a single fixed page size.
                let mut seen = Vec::new();
                let mut offset = 0usize;
                for (n, size) in [7usize, 13, 1, 31, 5, 39].iter().cycle().enumerate() {
                    if offset >= sorted.len() { break; }
                    let page = &sorted[offset..(offset + size).min(sorted.len())];
                    seen.extend(page.iter().map(row_key));
                    offset += size;
                    assert!(n < 1000, "paging failed to terminate");
                }
                assert_eq!(offset, sorted.len());
                let uniq: std::collections::HashSet<_> = seen.iter().collect();
                assert_eq!(uniq.len(), seen.len(), "sort={sort} desc={desc}: a row was served twice");
                assert_eq!(uniq.len(), rows.len(), "sort={sort} desc={desc}: a row was never served");
            }
        }
    }

    /// The direction must apply to the chosen column only. If a descending sort
    /// also flipped the tiebreak, the relative order of rows that tie on the
    /// primary key would depend on direction, and the "exactly once" guarantee
    /// above would hold only for one of the two orders.
    #[test]
    fn records_ties_resolve_the_same_way_in_both_directions() {
        let rows = tied_corpus();
        for sort in ["i", "o", "c"] {
            // Group each ordering by the primary value, keeping the order the
            // rows appear in. Comparing the two maps (rather than the two flat
            // lists) avoids having to undo the direction reversal.
            let groups = |desc: bool| {
                let mut sorted = rows.clone();
                sorted.sort_by(|x, y| records_cmp(x, y, sort, desc));
                let mut map: HashMap<i64, Vec<(u8, i64, i64, i64, String, String)>> = HashMap::new();
                for r in &sorted {
                    map.entry(records_primary(r, sort))
                        .or_default()
                        .push((r.a, r.t, r.i, r.o, r.s.clone(), r.p.clone()));
                }
                map
            };
            let asc = groups(false);
            let desc = groups(true);
            assert_eq!(asc.len(), desc.len(), "sort={sort}: different numbers of tie groups");
            for (value, a) in &asc {
                let b = desc.get(value).unwrap_or_else(|| panic!("sort={sort}: group {value} missing when descending"));
                assert_eq!(a, b, "sort={sort}: rows tying on {value} are ordered differently by direction");
            }
        }
    }

    /// Every link in the chain has to actually be consulted, or the order is a
    /// partial one. This asserts that directly, one field at a time: change a
    /// single field and the comparison must stop calling the rows equal. Drop a
    /// `then_with` from `records_cmp` and the corresponding case below fails —
    /// which the corpus-based tests above cannot promise, because a corpus with
    /// one globally-unique field stays total even when an earlier link is gone.
    #[test]
    fn records_order_uses_every_field_in_the_chain() {
        let base = rec(1, "m-x", 1_700_000_000_000, 100, 20, 7, "s-x", "~/proj");
        let variants = vec![
            ("t", rec(1, "m-x", 1_700_000_000_001, 100, 20, 7, "s-x", "~/proj")),
            ("a", rec(2, "m-x", 1_700_000_000_000, 100, 20, 7, "s-x", "~/proj")),
            ("s", rec(1, "m-x", 1_700_000_000_000, 100, 20, 7, "s-y", "~/proj")),
            ("p", rec(1, "m-x", 1_700_000_000_000, 100, 20, 7, "s-x", "~/other")),
            ("m", rec(1, "m-y", 1_700_000_000_000, 100, 20, 7, "s-x", "~/proj")),
            ("i", rec(1, "m-x", 1_700_000_000_000, 101, 20, 7, "s-x", "~/proj")),
            ("o", rec(1, "m-x", 1_700_000_000_000, 100, 21, 7, "s-x", "~/proj")),
            ("c", rec(1, "m-x", 1_700_000_000_000, 100, 20, 8, "s-x", "~/proj")),
        ];
        // The varying field must be a non-primary one for this to be meaningful
        // when the primary happens to be that field, so exercise every sort key.
        for sort in ["t", "i", "o", "c"] {
            for (field, other) in &variants {
                if *field == sort { continue; }
                assert_ne!(
                    records_cmp(&base, other, sort, false),
                    std::cmp::Ordering::Equal,
                    "sort={sort}: rows differing only in `{field}` compared equal — \
                     `{field}` is missing from the tiebreak chain"
                );
                assert_ne!(
                    records_cmp(&base, other, sort, true),
                    std::cmp::Ordering::Equal,
                    "sort={sort} desc: rows differing only in `{field}` compared equal"
                );
            }
        }
        // And a row must compare equal to itself, so the chain never fabricates
        // an order between identical rows.
        for sort in ["t", "i", "o", "c"] {
            for desc in [false, true] {
                assert_eq!(records_cmp(&base, &base, sort, desc), std::cmp::Ordering::Equal);
            }
        }
    }

    #[test]
    fn day_bounds_are_parsed_or_rejected_but_never_silently_dropped() {
        assert_eq!(parse_day_bound(None, "from"), Ok(None));
        assert_eq!(parse_day_bound(Some(""), "from"), Ok(None));
        assert_eq!(parse_day_bound(Some("   "), "from"), Ok(None));
        let d = day_start_ms("2026-08-01").unwrap();
        assert_eq!(parse_day_bound(Some("2026-08-01"), "from"), Ok(Some(d)));
        assert_eq!(parse_day_bound(Some(" 2026-08-01 "), "from"), Ok(Some(d)));
        // Dropping an unparseable bound would widen the query and show the
        // reader extra rows, so these must be errors.
        for bad in ["notadate", "2026-13-45", "2026-02-30", "2026/08/01", "20260801"] {
            let got = parse_day_bound(Some(bad), "from");
            assert!(got.is_err(), "{bad:?} must be rejected, got {got:?}");
            assert!(got.unwrap_err().contains("YYYY-MM-DD"));
        }
    }

    #[test]
    fn day_bound_end_of_day_is_inclusive() {
        // "to" is a whole-day bound, so the last millisecond of that day is in
        // range while the first of the next day is out.
        let start = day_start_ms("2026-09-14").unwrap();
        let end_exclusive = start + 86_400_000;
        assert!(end_exclusive - 1 >= start);
        assert!(!(end_exclusive >= start && end_exclusive < end_exclusive));
        assert_eq!(day_start_ms("2026-09-15").unwrap(), end_exclusive);
    }

    #[test]
    fn unknown_sort_column_degrades_to_timestamp() {        let r = rec(1, "m", 42, 9, 9, 9, "s", "p");
        assert_eq!(records_primary(&r, "t"), 42);
        assert_eq!(records_primary(&r, "i"), 9);
        assert_eq!(records_primary(&r, "nonsense"), 42);
    }

    #[test]
    fn pi_line_shape_a_and_b() {        let fa = Path::new("/home/u/.pi/agent/sessions/Users-u-proj/s01.jsonl");
        let mk = &mut HashMap::new();
        let da = json!({
            "type": "assistant", "id": "a1", "timestamp": "2026-09-13T10:00:00+08:00",
            "model": "kimi-k2", "usage": {"input": 120, "output": 30, "cacheRead": 90, "cacheWrite": 10}
        });
        let r = pi_line(&da, fa, 1111, mk, "6|s01.jsonl").unwrap();
        // input = input + cacheRead + cacheWrite (pi's own `totalTokens` rule):
        // 120 + 90 + 10. Storing 120 alone is the bug that produced a 667.5%
        // cache-hit rate on the HUD.
        assert_eq!((r.a, r.i, r.o, r.c, r.m.as_str()), (6, 220, 30, 90, "kimi-k2"));
        assert!(r.c <= r.i, "cache reads cannot exceed input");
        assert_eq!(r.s, "s01");
        let db = json!({
            "type": "message",
            "message": {"role": "assistant", "model": "glm-5",
                        "usage": {"input": 50, "output": 8, "cacheRead": 40, "cacheWrite": 0}}
        });
        let r = pi_line(&db, fa, 1111, mk, "6|s01.jsonl").unwrap();
        assert_eq!((r.i, r.o, r.c), (50 + 40, 8, 40));   // input + cacheRead
        assert!(pi_line(&json!({"type":"user","usage":{"input":1,"output":1}}), fa, 1111, mk, "6|s01.jsonl").is_none());
        // no timestamp -> falls back to file mtime
        let dn = json!({"type":"assistant","id":"a2","usage":{"input":5,"output":1}});
        assert_eq!(pi_line(&dn, fa, 7777, mk, "6|s01.jsonl").unwrap().t, 7777);
    }

    /// The regression this test exists for: every pi row used to be stored with
    /// a session id of `2026-09-` and a project of `--Users-alice-…`, because
    /// both were taken as *prefixes* of the file name and directory name. The
    /// fixture below is byte-for-byte the shape of the transcripts pi actually
    /// writes — the old synthetic fixture (`s01.jsonl` in `Users-u-proj/`) could
    /// not expose either bug, which is why 36 green tests missed it.
    #[test]
    fn pi_line_reads_the_real_layout() {
        let fp = Path::new("/h/.pi/agent/sessions/--Users-alice-proj--/2026-09-15T08-28-45-066Z_01a0a42e-ca8a-71fd-a635-72ce0c495cd9.jsonl");
        let key = "6|2026-09-15T08-28-45-066Z_01a0a42e-ca8a-71fd-a635-72ce0c495cd9.jsonl";
        let mut meta = HashMap::new();
        // first line: session header carrying the real cwd, no usage
        let head = json!({"type": "session", "version": 1, "id": "01a0a42e",
                          "timestamp": "2026-09-15T08:28:45.066Z", "cwd": "/srv/app/proj"});
        assert!(pi_line(&head, fp, 0, &mut meta, key).is_none());
        let d = json!({"type": "message", "timestamp": "2026-09-15T08:29:00.000Z",
                       "message": {"role": "assistant", "model": "deepseek-flash",
                                   "usage": {"input": 151, "output": 2, "cacheRead": 4864}}});
        let r = pi_line(&d, fp, 0, &mut meta, key).unwrap();
        assert_eq!(r.s, "72ce0c495cd9", "session must be the uuid, not a date prefix");
        assert_eq!(r.p, home_short("/srv/app/proj"), "project must come from the cwd");
        assert_eq!((r.i, r.o, r.c), (151 + 4864, 2, 4864), "input must include cache reads");
        assert!(r.c <= r.i);
        // a tail-only read (no header line) must keep the same project
        let r2 = pi_line(&d, fp, 0, &mut meta, key).unwrap();
        assert_eq!(r2.p, home_short("/srv/app/proj"));
    }

    #[test]
    fn kimi_line_legacy_and_wire() {
        let fp = Path::new("/home/u/.kimi/sessions/abc123/s9/context.jsonl");
        let d = json!({"type": "_usage", "ts": "2026-09-13T10:00:00+08:00",
                       "input_tokens": 700, "output_tokens": 120, "cached_tokens": 600});
        let r = kimi_line(&d, fp, 1111).unwrap();
        assert_eq!((r.a, r.i, r.o, r.c), (7, 700, 120, 600));
        // 会话取的是**目录**，不是文件名：文件名是常量，所有会话都会撞在一起。
        assert_eq!((r.s.as_str(), r.p.as_str()), ("s9", "abc123"));
        let d = json!({"kind": "assistant", "model": "kimi-k2.5",
                       "usage": {"input_tokens": 40, "output_tokens": 5}});
        assert!(kimi_line(&d, fp, 1111).is_some());
        assert!(kimi_line(&json!({"type":"user","text":"hi"}), fp, 1111).is_none());
        // Kimi Code CLI 的布局多两层（agents/main/wire.jsonl），会话同样是 sessions/ 后第二段
        let fp2 = Path::new("/home/u/.kimi-code/sessions/wd7/3f2a91b4/agents/main/wire.jsonl");
        let r2 = kimi_line(&d, fp2, 1111).unwrap();
        assert_eq!((r2.s.as_str(), r2.p.as_str()), ("3f2a91b4", "wd7"));
    }

    #[test]
    fn unknown_agent_id_parses_to_nothing() {
        // 4 = OpenCode（走 SQLite），13 = 没见过的 id：都不该被静默当成 Claude Code
        let d = json!({"message": {"usage": {"input_tokens": 10, "output_tokens": 5}}});
        let fp = Path::new("/tmp/x.jsonl");
        let mut meta = HashMap::new();
        assert!(parse_line(4, &d, fp, &mut meta, 0, &CustomFile::default()).is_none());
        assert!(parse_line(13, &d, fp, &mut meta, 0, &CustomFile::default()).is_none());
    }

    #[test]
    fn iflow_line_parses_tokens_object() {
        let fp = Path::new("/home/u/.iflow/projects/projA/session-e1.jsonl");
        let d = json!({
            "id": "x1", "type": "gemini", "timestamp": "2026-09-13T10:00:00+08:00",
            "model": "kimi-k2", "tokens": {"input": 220, "output": 33, "cached": 200}
        });
        let r = iflow_line(&d, fp, 1111).unwrap();
        assert_eq!((r.a, r.i, r.o, r.c, r.m.as_str()), (8, 220, 33, 200, "kimi-k2"));
        assert_eq!(r.s, "e1");
    }

    #[test]
    fn qoder_line_message_and_toplevel_usage() {
        let fp = Path::new("/home/u/.qoder/projects/projB/abc.jsonl");
        let d = json!({
            "timestamp": "2026-09-13T10:00:00+08:00", "sessionId": "sess-01",
            "message": {"model": "glm-5", "usage": {"input_tokens": 90, "output_tokens": 11}}
        });
        let r = qoder_line(&d, fp, 1111).unwrap();
        assert_eq!((r.a, r.i, r.o), (9, 90, 11));
        let d = json!({"ts": "2026-09-13T10:00:00+08:00",
                       "usage": {"prompt_tokens": 60, "completion_tokens": 7}});
        let r = qoder_line(&d, fp, 1111).unwrap();
        assert_eq!((r.i, r.o), (60, 7));
    }

    #[test]
    fn self_describing_logs_keep_cached_within_input() {
        // Three real-world shapes for the same question ("does `input` already
        // contain the cache reads?"), each must end up with cached <= input —
        // otherwise the hit rate on screen exceeds 100% (see ㉛).
        let src = CustomSource { id: "x".into(), name: "X".into(), enabled: true,
                                 path: String::new(), format: "jsonl-usage".into(), fields: HashMap::new() };
        let p = Path::new("/h/log.jsonl");
        // 1. a tool that reports its own total (Goose): total = in + out
        let goose = json!({"usage": {"input_tokens": 421, "output_tokens": 31,
                                     "total_tokens": 452, "cache_read_input_tokens": 0}});
        let r = custom_line(11, &goose, p, 0, &src).unwrap();
        assert_eq!((r.i, r.o, r.c), (421, 31, 0));
        let goose_cached = json!({"usage": {"input_tokens": 421, "output_tokens": 31,
                                            "total_tokens": 452, "cache_read_input_tokens": 400}});
        let r = custom_line(11, &goose_cached, p, 0, &src).unwrap();
        assert_eq!((r.i, r.o, r.c), (421, 31, 400), "total wins, and c <= i holds");
        // 2. OpenAI-shaped: cached is a subset of input
        let openai = json!({"usage": {"prompt_tokens": 1000, "completion_tokens": 50,
                                      "prompt_tokens_details": {"cached_tokens": 900}}});
        let r = custom_line(11, &openai, p, 0, &src).unwrap();
        assert_eq!((r.i, r.o, r.c), (1000, 50, 900), "nested details are read");
        let openai_flat = json!({"usage": {"input_tokens": 1000, "output_tokens": 50, "cached_tokens": 900}});
        let r = custom_line(11, &openai_flat, p, 0, &src).unwrap();
        assert_eq!((r.i, r.o, r.c), (1000, 50, 900));
        // 3. additive style without a total: input is only the non-cached part
        let pi_like = json!({"usage": {"input": 151, "output": 2, "cacheRead": 4864}});
        let r = custom_line(11, &pi_like, p, 0, &src).unwrap();
        assert_eq!((r.i, r.o, r.c), (5015, 2, 4864), "additive: 151 + 4864");
        assert!(r.c <= r.i);
    }

    #[test]
    fn cache_reads_never_exceed_input() {
        // `c <= i` is what makes the number on the HUD a *percentage*. Every
        // parser must therefore define `i` as the whole prompt (cache reads and
        // writes included) and `c` as the cache-read part of it. pi broke this
        // by taking only its `input` field, which excludes cacheRead — the HUD
        // then showed 667.5% (STATUS ㉛).
        //
        // The fixtures here pin the contract for every source; the five parsers
        // without real transcripts (gemini/qwen/kimi/iflow/qoder) still need the
        // same question asked of real files, because a fixture encodes *our*
        // assumption about the provider's arithmetic, not the provider's.
        let f = |name: &str, r: Option<Record>| {
            let r = r.unwrap_or_else(|| panic!("{name}: fixture did not parse"));
            assert!(r.c <= r.i, "{name}: c={} > i={} — hit rate would exceed 100%", r.c, r.i);
        };
        let p = Path::new("/h/.x/f.jsonl");
        f("codex", codex_line(&json!({"timestamp": "2026-09-15T10:00:00+08:00",
            "payload": {"type": "token_count", "info": {"last_token_usage":
                {"input_tokens": 500, "output_tokens": 90, "cached_input_tokens": 450}}}}),
            "1|f.jsonl", &mut HashMap::new()));
        f("claude", parse_line(2, &json!({"timestamp": "2026-09-15T10:00:00+08:00",
            "sessionId": "6f34fee9-cdfd-43ae-9d6d-20d5da3af95b", "cwd": "/srv/app",
            "message": {"model": "claude-x", "usage": {"input_tokens": 10,
                "cache_read_input_tokens": 900, "cache_creation_input_tokens": 50,
                "output_tokens": 20}}}), p, &mut HashMap::new(), 0, &CustomFile::default()));
        f("pi", pi_line(&json!({"type": "message", "timestamp": "2026-09-15T10:00:00.000Z",
            "message": {"role": "assistant", "model": "deepseek-flash",
                "usage": {"input": 1153, "output": 142, "cacheRead": 3840, "cacheWrite": 0}}}),
            p, 0, &mut HashMap::new(), "6|f.jsonl"));
        f("gemini", gemini_qwen_line(3, &json!({"id": "m1", "type": "gemini",
            "timestamp": "2026-09-15T10:00:00+08:00", "model": "gemini-3",
            "tokens": {"input": 300, "output": 40, "cached": 250}}), p));
        f("qwen", gemini_qwen_line(4, &json!({"id": "m1", "type": "gemini",
            "timestamp": "2026-09-15T10:00:00+08:00", "model": "qwen3",
            "tokens": {"input": 300, "output": 40, "cached": 250}}), p));
        f("kimi", kimi_line(&json!({"type": "_usage", "ts": "2026-09-15T10:00:00+08:00",
            "input_tokens": 700, "output_tokens": 120, "cached_tokens": 600}), p, 0));
        f("iflow", iflow_line(&json!({"id": "x1", "type": "gemini",
            "timestamp": "2026-09-15T10:00:00+08:00", "model": "kimi-k2",
            "tokens": {"input": 220, "output": 33, "cached": 200}}), p, 0));
        f("qoder", qoder_line(&json!({"timestamp": "2026-09-15T10:00:00+08:00",
            "sessionId": "s1", "message": {"model": "glm-5",
                "usage": {"input_tokens": 90, "output_tokens": 11, "cached_tokens": 80}}}), p, 0));
    }

    #[test]
    fn store_dedups_identical_records() {
        let store = Store::in_memory().unwrap();
        let r = rec(3, "gemini", 1000, 10, 2, 5, "s", "p");
        assert!(store.insert(&r));
        assert!(!store.insert(&r)); // duplicate ignored
        assert_eq!(store.load_records().len(), 1);
        let r2 = rec(3, "gemini", 1001, 10, 2, 5, "s", "p");
        assert!(store.insert(&r2));
        assert_eq!(store.load_records().len(), 2);
    }

    #[test]
    fn store_file_state_roundtrip() {
        let store = Store::in_memory().unwrap();
        store.save_file_states(&[("/a.jsonl".into(), 3, 42)]);
        let fs = store.load_file_state();
        assert_eq!(fs.get("/a.jsonl").unwrap().off, 42);
        store.save_file_states(&[("/a.jsonl".into(), 3, 99)]);
        assert_eq!(store.load_file_state().get("/a.jsonl").unwrap().off, 99);
    }

    #[test]
    fn oc_state_kv_roundtrip() {
        let store = Store::in_memory().unwrap();
        let mut m = HashMap::new();
        m.insert("s1".to_string(), vec![10i64, 20i64, 7i64]);
        store.save_kv("oc_state", &oc_state_json(&m));
        let back = oc_state_parse(&store.load_kv("oc_state").unwrap());
        assert_eq!(back.get("s1").map(|v| v.as_slice()), Some(&[10i64, 20, 7][..]));
        // a 2-element baseline written by an older build must still load
        let old = oc_state_parse("{\"s2\":[1,2]}");
        assert_eq!(old.get("s2").map(|v| v.as_slice()), Some(&[1i64, 2][..]));
    }

    #[test]
    fn migrations_are_recorded_and_run_once() {
        let store = Store::in_memory().unwrap();
        let conn = lock(&store.conn);
        conn.execute_batch(
            "INSERT INTO records(a,m,t,i,o,c,s,p) VALUES(1,'unknown',1,0,0,0,'s','p');
             INSERT INTO file_state(path,agent,off) VALUES('/x.jsonl',1,99);
             INSERT INTO kv(k,v) VALUES('codex_meta','{}');",
        ).unwrap();
        run_migrations(&conn);
        for (table, want) in [("records", 0i64), ("file_state", 0i64)] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, want, "{table} not cleared");
        }
        assert!(conn.query_row("SELECT v FROM kv WHERE k='migr:parser_repair_v1'", [], |r| r.get::<_, String>(0)).is_ok());
        assert!(conn.query_row("SELECT COUNT(*) FROM kv WHERE k='codex_meta'", [], |r| r.get::<_, i64>(0)).unwrap() == 0);
        // second pass is a no-op and must not error
        run_migrations(&conn);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM kv", [], |r| r.get::<_, i64>(0)).unwrap(),
            MIGRATIONS.len() as i64
        );
    }

    #[test]
    fn keep_window_prune_cutoff() {
        let cutoff = now_ms() - KEEP_DAYS * 86_400_000;
        let old = rec(0, "m", cutoff - 1, 1, 1, 0, "s", "p");
        let recent = rec(0, "m", now_ms(), 1, 1, 0, "s", "p");
        let mut v = vec![old, recent];
        v.retain(|r| r.t >= cutoff);
        assert_eq!(v.len(), 1);
    }

    /// Brute-force twin of `live_stats_from`, kept deliberately dumb: it is the
    /// oracle the binary-search version is checked against.
    /// The reference implementation, written from the *rule* rather than from
    /// `live_stats_from`: each figure is defined by an age interval and the
    /// buckets are filled bucket-major, i.e. by asking "which records fall in
    /// this second" instead of "which second does this record fall in".
    ///
    /// Both differences matter. When this was a record-major mirror of the
    /// production loop it shared the production loop's missing upper bound, so
    /// the two agreed on a wrong answer and the test passed — a defect frozen
    /// into the contract.
    fn live_stats_bruteforce(recs: &[Record], now: i64, write_ago_ms: u64) -> Value {
        let day_start = cst_day_start_ms(now);
        // The oracle must mirror the production definition exactly: 燃烧 is
        // 输入 + 输出 (see `live_stats_from`). When it was "output + non-cached
        // input" the two matched each other and stayed wrong together — which is
        // the reason this test asserts against an *independent* implementation
        // rather than against a copy of the same expression.
        let burn = |r: &Record| r.i + r.o;
        let age_of = |r: &Record| now - r.t;
        // A record counts if it is in the past (within the skew allowance) ...
        let not_ahead = |r: &Record| age_of(r) >= -CLOCK_SKEW_MS;

        let mut b60 = 0i64;
        let mut b10 = 0i64;
        let mut active = std::collections::HashSet::new();
        let (mut t_n, mut t_i, mut t_o, mut t_c) = (0i64, 0i64, 0i64, 0i64);
        let mut t_agent: Vec<[i64; 4]> = Vec::new();
        for r in recs {
            if !not_ahead(r) { continue; }
            let age = age_of(r);
            // [-skew, 60 000) and [-skew, 10 000): the allowance lets a stamp a
            // moment ahead of the clock count as "just happened".
            if (-CLOCK_SKEW_MS..60_000).contains(&age) {
                b60 += burn(r);
                active.insert(r.a);
            }
            if (-CLOCK_SKEW_MS..10_000).contains(&age) {
                b10 += burn(r);
            }
            if r.t >= day_start {
                t_n += 1;
                t_i += r.i;
                t_o += r.o;
                t_c += r.c;
                let idx = r.a as usize;
                if t_agent.len() <= idx { t_agent.resize(idx + 1, [0; 4]); }
                t_agent[idx][0] += 1;
                t_agent[idx][1] += r.i;
                t_agent[idx][2] += r.o;
                t_agent[idx][3] += r.c;
            }
        }

        // bucket-major: bucket b covers ages [(59-b)*1000, (59-b)*1000 + 999],
        // and a stamp up to CLOCK_SKEW_MS ahead of now lands in the newest one
        let mut buckets = [0i64; 60];
        for (b, slot) in buckets.iter_mut().enumerate() {
            let lo = (59 - b) as i64 * 1000;
            let hi = lo + 999;
            for r in recs {
                if !not_ahead(r) { continue; }
                let age = age_of(r);
                if age >= lo && age <= hi {
                    *slot += burn(r);
                } else if b == 59 && age < 0 {
                    // the allowance: "just happened" belongs in the newest bucket
                    *slot += burn(r);
                }
            }
        }

        let last_t = recs.iter().filter(|r| not_ahead(r)).map(|r| r.t).max().unwrap_or(0);
        json!({
            "burn60": b60,
            "burn10": b10,
            "buckets": &buckets[..],
            "today_agents": t_agent.iter().map(|v| json!(v)).collect::<Vec<Value>>(),
            "active": active.len(),
            "last_ago_s": if last_t > 0 {
                Some((((now - last_t) as f64 / 1000.0).max(0.0) * 10.0).round() / 10.0)
            } else { None },
            "write_ago": (write_ago_ms as f64 / 1000.0 * 10.0).round() / 10.0,
            // the brute-force oracle has to model the whole contract, `t`
            // included — the assertion below is a full-object comparison
            "today": {"n": t_n, "i": t_i, "o": t_o, "c": t_c, "t": t_i + t_o,
                      "rate": if t_i > 0 { (t_c as f64 / t_i as f64 * 1000.0).round() / 10.0 } else { 0.0 }},
        })
    }

    /// A stamp ahead of the clock must not be able to pollute the live figures.
    ///
    /// The generator that produced this test case tripped the bug by accident —
    /// it advances each synthetic session's clock by a random handful of seconds
    /// per call, so a long session starts in the past and ends in the future —
    /// and the live sample showed `last_ago_s = -872.5`, i.e. "the last call
    /// happened 14 minutes from now", with the burn figure inflated by the
    /// future records. A clock that is skewed, restored from a snapshot, or
    /// simply wrong produces the same shape.
    #[test]
    fn a_record_from_the_future_cannot_inflate_the_live_figures() {
        let now = 1_760_000_000_000i64;
        let day_start = cst_day_start_ms(now);
        // ten honest records within the last minute of today. Sorted ascending
        // because that is the invariant the real caller maintains — feeding the
        // function a descending slice makes `partition_point` answer nonsense,
        // and the first version of this test did exactly that.
        let mut recs: Vec<Record> = (0..10)
            .map(|k| rec(1, "m", now - k * 5_000, 1_000_000, 1_000_000, 0, "s", "p"))
            .collect();
        recs.sort_by_key(|r| r.t);
        let honest_burn: i64 = 10 * 2_000_000;

        let clean = live_stats_from(&recs, now, 0);
        assert_eq!(clean["today"]["n"], 10);
        assert_eq!(clean["burn60"], honest_burn);

        // 燃烧 is 输入 + 输出 — the cache *is* counted. It used to be
        // "output + non-cached input", which on a 99 %-hit machine answers a
        // different question than "how many tokens did I consume": this record
        // is 1010 under the current definition and was 110 under the old one.
        let cached = vec![rec(1, "m", now - 1_000, 1_000, 10, 900, "s", "p")];
        assert_eq!(live_stats_from(&cached, now, 0)["burn60"], 1_010);
        assert_eq!(clean["last_ago_s"], 0.0);   // the newest is exactly `now`

        // now add one record stamped an hour ahead and one 30 days ahead
        for ahead in [3_600_000i64, 30 * 86_400_000] {
            recs.push(rec(9, "m", now + ahead, 4_000_000, 4_000_000, 0, "s", "future"));
        }
        // keep the slice sorted the way the server keeps it
        recs.sort_by_key(|r| r.t);

        let dirty = live_stats_from(&recs, now, 0);
        assert_eq!(dirty["today"]["n"], 10,
                   "a record dated in the future was counted as part of today");
        assert_eq!(dirty["today"]["i"], clean["today"]["i"],
                   "a record dated in the future was counted into today's input");
        assert_eq!(dirty["burn60"], honest_burn,
                   "a record dated in the future was counted into the 60 s burn");
        assert_eq!(dirty["active"], clean["active"],
                   "a record dated in the future made a data source look active");
        assert_eq!(dirty["last_ago_s"], 0.0,
                   "a record dated in the future became 'the most recent call'");
        assert_eq!(dirty["buckets"], clean["buckets"],
                   "a record dated in the future landed in the burn chart");

        // The skew allowance is not a licence: a record a second or two ahead is
        // treated as "just now", and one beyond the allowance is ignored.
        let mut edge = vec![rec(1, "m", now + CLOCK_SKEW_MS, 5, 5, 0, "s", "p")];
        assert_eq!(live_stats_from(&edge, now, 0)["today"]["n"], 1);
        edge = vec![rec(1, "m", now + CLOCK_SKEW_MS + 1, 5, 5, 0, "s", "p")];
        assert_eq!(live_stats_from(&edge, now, 0)["today"]["n"], 0,
                   "past the allowance the record must be ignored, not counted");

        // A corpus made *entirely* of future stamps reports no activity at all,
        // which is the honest answer.
        let all_future = vec![rec(1, "m", now + 86_400_000, 5, 5, 0, "s", "p")];
        let v = live_stats_from(&all_future, now, 0);
        assert_eq!(v["today"]["n"], 0);
        assert_eq!(v["burn60"], 0);
        assert_eq!(v["last_ago_s"], Value::Null);
        assert!(v["buckets"].as_array().unwrap().iter().all(|b| b == 0));

        // And the boundary: a record exactly at the start of today counts, one
        // a millisecond earlier does not.
        let at_start = vec![rec(1, "m", day_start, 7, 7, 0, "s", "p")];
        assert_eq!(live_stats_from(&at_start, now, 0)["today"]["n"], 1);
        let before = vec![rec(1, "m", day_start - 1, 7, 7, 0, "s", "p")];
        assert_eq!(live_stats_from(&before, now, 0)["today"]["n"], 0);
    }

    /// The windowed version must agree with the brute-force one on every probe
    /// moment — including the exact boundaries, where `age <= 60_000` vs
    /// `t >= now - 60_000` is the kind of off-by-one that silently moves a
    /// record in or out of the burn rate.
    #[test]
    fn live_stats_windows_match_bruteforce() {
        let now = 1_760_000_000_000i64;   // arbitrary, well inside range
        let mut recs: Vec<Record> = Vec::new();
        // a 3-day spread so "today" and the 60 s window are genuinely different
        for k in 0..400i64 {
            let t = now - (k * 37_000) % (3 * 86_400_000) - 1;   // scatter incl. boundaries
            recs.push(rec((k % 10) as u8, "m", t, 100 + k, 10 + k, k % 50, "s", "p"));
        }
        // hit every boundary exactly, plus records past the 60 s window, past
        // today, exactly at the skew allowance, and far in the future
        for off in [0i64, 1, 9_999, 10_000, 10_001, 59_998, 59_999, 60_000, 60_001,
                    86_400_000, 30 * 86_400_000] {
            recs.push(rec(3, "m", now - off, 777, 77, 7, "s", "p"));
        }
        // ahead of the clock, inside and outside the allowance
        for ahead in [1i64, CLOCK_SKEW_MS, CLOCK_SKEW_MS + 1, 60_000, 30 * 86_400_000] {
            recs.push(rec(4, "m", now + ahead, 999, 99, 9, "s", "p"));
        }
        recs.sort_by_key(|r| r.t);

        // probe at several "now"s, each of which slides all three windows
        for probe in [now, now + 1, now + 4_321, now + 55_000, now + 120_000, now + 86_400_000] {
            let fast = live_stats_from(&recs, probe, 1_500);
            let slow = live_stats_bruteforce(&recs, probe, 1_500);
            assert_eq!(fast, slow, "mismatch at now={probe}");

            // The chart and the number above it must describe the same 60
            // seconds. They used to differ by one record, which is invisible on
            // screen and impossible to explain to a user who adds the bars up.
            let sum: i64 = fast["buckets"].as_array().unwrap()
                .iter().map(|v| v.as_i64().unwrap()).sum();
            assert_eq!(sum, fast["burn60"].as_i64().unwrap(),
                       "the buckets do not add up to burn60 at now={probe}");
        }

        // empty history must not panic (partition_point on an empty slice)
        let empty: Vec<Record> = Vec::new();
        let v = live_stats_from(&empty, now, 0);
        assert_eq!(v["burn60"], 0);
        assert_eq!(v["last_ago_s"], Value::Null);
    }
}
