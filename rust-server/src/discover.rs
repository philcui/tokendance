//! 数据源发现：路径候选、名单注册表、三层发现（已知路径 → 按名字 → 按内容）。
//
// 由 `main.rs` 拆分而来，逻辑未改动。
//
// 顺序就是发现顺序：`discovery_locations` 出候选根目录，`scan_location` 逐目录
// 找特征文件，`discover_tiered` 把三层结果合并并去重。

use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ---------- source discovery: candidates, registry, three tiers ----------

pub(crate) fn name_variants(name: &str) -> Vec<String> {
    let t = name.trim();
    let lower = t.to_lowercase();
    let mut v = vec![t.to_string(), lower.clone(), t.to_uppercase()];
    let squashed: String = lower.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let kebab: String = lower.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let kebab = kebab.trim_matches('-').to_string();
    let cli_variants = [format!("{lower}-cli"), format!("{squashed}-cli")];
    for extra in [squashed.clone(), kebab, cli_variants[0].clone(), cli_variants[1].clone()] {
        if !extra.is_empty() && !v.contains(&extra) { v.push(extra); }
    }
    v.dedup();
    v
}

pub(crate) fn plist_string(path: &Path, key: &str) -> Option<String> {
    let out = std::process::Command::new("/usr/libexec/PlistBuddy")
        .arg("-c").arg(format!("Print :{key}"))
        .arg(path)
        .output().ok()?;
    if !out.status.success() { return None; }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

pub(crate) fn probe_roots(name: &str) -> (Vec<String>, Vec<String>) {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut variants = name_variants(name);
    let mut derived: Vec<String> = Vec::new();
    let lower = name.to_lowercase();
    if let Ok(rd) = std::fs::read_dir("/Applications") {
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            if !fname.to_lowercase().contains(&lower) { continue; }
            let plist = e.path().join("Contents/Info.plist");
            for key in ["CFBundleIdentifier", "CFBundleName", "CFBundleDisplayName"] {
                if let Some(v) = plist_string(&plist, key) {
                    derived.push(v.clone());
                    if let Some(tail) = v.rsplit('.').next() { derived.push(tail.to_string()); }
                }
            }
        }
    }
    for cand in [format!("{home}/.local/bin/{lower}"), format!("/usr/local/bin/{lower}"),
                 format!("/opt/homebrew/bin/{lower}")] {
        if Path::new(&cand).exists() {
            if let Some(parent) = Path::new(&cand).parent().and_then(|p| p.file_name()) {
                let p = parent.to_string_lossy().to_string();
                // `~/.local/bin/<name>` must not turn "bin" into a product name:
                // that produced junk roots like `~/.bin` and, worse, ranked them
                // alongside real ones. The binary's own name is already a
                // variant, and its data locations are covered by the templates.
                const GENERIC_BIN: &[&str] = &["bin", "sbin", "binaries", "local", "Cellar",
                                                "node_modules", ".local", "libexec"];
                if !GENERIC_BIN.contains(&p.as_str()) { derived.push(p); }
            }
        }
    }
    for v in derived.clone() { if !variants.contains(&v) { variants.push(v); } }
    variants.dedup();
    derived.dedup();
    (candidate_roots(&home, &variants), derived)
}

/// Template the usual per-tool locations and keep the ones that exist.
///
/// The privacy check has to happen **before** `exists()`: a bare `stat` on
/// `~/Library/Application Support/AddressBook` is enough to involve the system —
/// it either blocks or raises a Contacts request, and either way the answer is
/// not ours to ask for. Found the hard way: typing "AddressBook" into the search
/// box hung the probe for over two minutes while the rest of the server stayed
/// perfectly healthy, because the check that skips private stores lived inside
/// the scanner, one call too late.
pub(crate) fn candidate_roots(home: &str, variants: &[String]) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    for v in variants {
        for tmpl in ["{home}/.{v}", "{home}/Library/Application Support/{v}", "{home}/.config/{v}",
                     "{home}/.local/share/{v}", "{home}/.local/state/{v}", "{home}/Library/Logs/{v}"] {
            let p = tmpl.replace("{home}", home).replace("{v}", v);
            if private_path(&p) { continue; }
            if Path::new(&p).exists() && !roots.contains(&p) { roots.push(p); }
        }
    }
    roots
}

/// `$HOME/...` → `~/...`, for anything we store or show.
pub(crate) fn short_home(p: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    p.strip_prefix(&home).map(|r| format!("~{r}")).unwrap_or_else(|| p.to_string())
}


/// Where the scanner looks, and how much of each place it may look at.
///
/// **Per-location budget, not one global cap.** The first version shared a single
/// file counter, and `~/Library/Application Support` — tens of thousands of
/// shallow cache files — spent it before breadth-first search ever reached
/// `~/.codex/sessions/2026/09/15/rollout-*.jsonl` five levels down. Two runs
/// measured exactly that: 4,000 and 40,000 files examined, `.codex` never seen,
/// and a wrong conclusion ("discovery finds one source"). Each location now gets
/// its own allowance, and every dot-directory in the home folder counts as its
/// own location — a statement about layout, not a list of products.
pub(crate) fn discovery_locations(blind: bool) -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut locs: Vec<String> = [".local/state", ".local/share", ".config", "Library/Logs",
                                 "Library/Application Support", ".cache"]
        .iter().map(|r| format!("{home}/{r}")).collect();
    if blind {
        // Without this, a CLI tool that keeps state in `~/.<tool>` is only
        // reachable through the home tree, which the budget above cannot afford.
        if let Ok(rd) = std::fs::read_dir(&home) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with('.') && !SKIP_DIRS.iter().any(|s| *s == name)
                    && e.path().is_dir() {
                    locs.push(e.path().to_string_lossy().to_string());
                }
            }
        }
    }
    locs.sort(); locs.dedup();
    locs
}

/// Directories never worth walking: our own data, the agents we already read,
/// and the packaging noise that fills Application Support.
pub(crate) const SKIP_DIRS: &[&str] = &[".tokendance", ".tokendance-lab", "node_modules", ".git",
    "vendor", "dist", "build", "__pycache__", "Caches", "Cache", "GPUCache",
    "Code Cache", "DawnCache", "Crashpad", "blob_storage", "Service Worker",
    "Backup", "Backups", "partitions", "CacheStorage", ".npm", ".yarn", ".gradle",
    ".cargo", ".rustup", ".docker", ".venv", "site-packages", ".vscode", ".git-credential"];

/// Directories the scan must never enter, for two reasons that happen to have
/// the same fix.
///
/// 1. **They hang.** `~/Library/Application Support/AddressBook` does not return
///    an error for a process without Contacts permission — macOS blocks the
///    `opendir()` indefinitely, waiting for a consent decision that a background
///    process can never show. Measured: `--discover` ran in 9 s twice, then a
///    later run sat at 0 % CPU for four minutes with the main thread parked in
///    `open$NOCANCEL` inside `scan_location`'s `read_dir`. Which run hits it
///    depends only on the order the directory entries come back in.
/// 2. **They are not ours.** Mail, Messages, Safari, Contacts, Health, TCC and
///    friends are other people's private data, not transcripts of an LLM
///    conversation. Reading them is exactly the kind of thing this app must not
///    do, so they are excluded by name *and* by the `com.apple.` prefix.
pub(crate) const PRIVATE_DIRS: &[&str] = &["addressbook", "callhistorydb", "callhistorytransactions",
    "knowledge", "facetime", "mail", "messages", "safari", "photos", "mobilesync",
    "clouddocs", "fileprovider", "homekit", "shortcuts", "reminders", "calendar",
    "cookies", "webkit", "passes", "health", "coreduet", "biome", "duetexpertcenter",
    "syncedpreferences", ".trash"];

/// Noise, not privacy: enormous and of no interest, but a user may legitimately
/// point a custom source inside one of them, so an explicit root is honoured.
pub(crate) const NOISE_DIRS: &[&str] = &["google", "microsoft"];

/// One place, so both scanners agree on what is off limits.
pub(crate) fn skip_dir(name: &str) -> bool {
    if SKIP_DIRS.iter().any(|s| *s == name) { return true; }
    let lower = name.to_lowercase();
    lower.starts_with("com.apple.")
        || PRIVATE_DIRS.iter().any(|s| lower == *s)
        || NOISE_DIRS.iter().any(|s| lower == *s)
}

/// Any *component* of a path is off limits — the check for a scan root.
///
/// `skip_dir` only filters children, so a root handed to `scan_location` — a
/// custom source path, or a registry entry someone added — could still open
/// `~/Library/Application Support/AddressBook` directly. Deliberately narrower
/// than `skip_dir`: it ignores the noise list and the caches list, so pointing a
/// source at `SomeApp/dist/logs` still works.
pub(crate) fn private_path(path: &str) -> bool {
    path.split('/').any(|c| {
        let lower = c.to_lowercase();
        lower.starts_with("com.apple.") || PRIVATE_DIRS.iter().any(|s| lower == *s)
    })
}

/// The label a group of files is reported under. A dot-directory at home *is*
/// the product name (`~/.codex` → Codex); inside a generic location we take the
/// first meaningful component (`~/.local/state/goose/...` → Goose).
pub(crate) fn group_label(root: &str, path: &str) -> String {
    pub(crate) const GENERIC_ROOT: &[&str] = &["state", "share", "config", "cache", "logs",
                                    "Application Support", "Library", ".local", ".cache", ".config"];
    let root_name = Path::new(root).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    if root_name.starts_with('.') && !GENERIC_ROOT.contains(&root_name.as_str()) {
        let bare = root_name.trim_start_matches('.');
        let mut c = bare.chars();
        return match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => root_name,
        };
    }
    name_from_path(path, root).0
}

/// Scan one location: breadth-first to `budget` files, and for every
/// line-delimited JSON file, sniff usage recursively. Returns one row per group
/// found: (label, files, usage lines, bytes, sample, example path).
pub(crate) fn scan_location(root: &str, parse_budget: usize) -> Vec<(String, u64, u64, u64, Value, String)> {
    use std::collections::{HashMap, VecDeque};
    // Filesystem TCC is not a suggestion: never open a privacy store, not even
    // when a path was typed in by hand or came from the registry. Asked for one,
    // we report nothing rather than asking the system for permission.
    if private_path(root) { return Vec::new(); }
    let mut agg: HashMap<String, (u64, u64, u64, Value, String)> = HashMap::new();
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::from([(PathBuf::from(root), 0usize)]);
    // The budget counts **directories and parsed files**, not files merely seen.
    // Counting every file seen is what starved the deep trees twice: a shallow
    // location with tens of thousands of caches spent the whole allowance before
    // breadth-first search reached `Application Support/VClaw/.openclaw/agents/
    // main/sessions/` (five levels) or `~/.codex/sessions/2026/09/15/`.
    // Directories are cheap (`read_dir` + `stat`); reading and parsing is not.
    pub(crate) const DIR_BUDGET: usize = 20_000;
    let mut dirs_seen = 0usize;
    let mut parsed = 0usize;
    let cutoff = now_ms() - 120 * 86_400_000;
    while let Some((dir, depth)) = queue.pop_front() {
        if depth > 6 || dirs_seen >= DIR_BUDGET || parsed >= parse_budget { break; }
        dirs_seen += 1;
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if skip_dir(&name) { continue; }
            let p = e.path();
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                if depth < 6 { queue.push_back((p, depth + 1)); }
                continue;
            }
            let size = md.len();
            if size == 0 || size > 60 * 1024 * 1024 { continue; }
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            if !matches!(ext.as_str(), "jsonl" | "json" | "ndjson" | "log" | "txt") { continue; }
            let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64).unwrap_or(0);
            if mtime < cutoff { continue; }
            parsed += 1;
            let head = {
                use std::io::Read;
                let mut buf = vec![0u8; 64 * 1024];
                match std::fs::File::open(&p).and_then(|mut f| f.read(&mut buf)) {
                    Ok(n) => { buf.truncate(n); buf }
                    Err(_) => continue,
                }
            };
            if head.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') { continue; }
            let src = CustomSource { id: "probe".into(), name: "probe".into(), enabled: true,
                path: p.to_string_lossy().to_string(), format: "jsonl-usage".into(), fields: HashMap::new() };
            let mut lines = 0u64; let mut sample = Value::Null;
            for raw in head.split(|b| *b == b'\n') {
                if raw.is_empty() { continue; }
                let Ok(d) = serde_json::from_slice::<Value>(raw) else { continue };
                if let Some(r) = custom_line(11, &d, &p, mtime, &src) {
                    lines += 1;
                    if sample.is_null() {
                        sample = json!({"input": r.i, "output": r.o, "cached": r.c});
                    }
                }
            }
            if lines == 0 { continue; }
            let g = group_label(root, &p.to_string_lossy());
            let e = agg.entry(g).or_insert((0, 0, 0, Value::Null, String::new()));
            e.0 += 1; e.1 += lines; e.2 += size;
            if e.3.is_null() { e.3 = sample; }
            if e.4.is_empty() { e.4 = p.to_string_lossy().to_string(); }
        }
    }
    let mut rows: Vec<_> = agg.into_iter().map(|(g, (f, l, b, s, x))| (g, f, l, b, s, x)).collect();
    rows.sort_by(|a, b| b.2.cmp(&a.2));
    rows
}

/// The scan the product runs. `blind` ignores everything we already know, which
/// is how the "no built-in list at all" question gets answered by the binary
/// itself rather than by a throwaway script.
pub(crate) fn discover_scan(blind: bool) -> Vec<Value> {
    let locs = discovery_locations(blind);
    let budget = (40_000 / locs.len().max(1)).max(3_000);
    let covered = if blind { Vec::new() } else { covered_prefixes() };
    let mut out: Vec<Value> = Vec::new();
    let mut by_example: std::collections::HashSet<String> = std::collections::HashSet::new();
    for loc in &locs {
        for (group, files, lines, bytes, sample, example) in scan_location(loc, budget) {
            // Locations overlap by construction (`~/.local` contains
            // `~/.local/state`), and the same file must not be reported — or
            // counted — twice.
            if !by_example.insert(example.clone()) { continue; }
            out.push(json!({
                "group": group, "files": files, "lines": lines, "bytes": bytes,
                "sample": sample, "example": short_home(&example),
                "location": short_home(loc),
                "covered": covered.iter().any(|p| example.starts_with(p)),
            }));
        }
    }
    out.sort_by(|a, b| b["lines"].as_u64().cmp(&a["lines"].as_u64()));
    out
}

/// Content-first discovery: **no name required**.
///
/// Take every group the scan finds (see `discover_scan`) that is not already
/// covered by a built-in or an existing custom source, that the user has not
/// removed, and that parses. `covered_prefixes()` is what stops a Codex rollout —
/// a JSONL file full of usage — from being added a second time under a new name.
pub(crate) fn discover_by_content() -> Vec<String> {
    discover_tiered(true).into_iter()
        .filter(|r| r["added"].as_bool().unwrap_or(false))
        .map(|r| format!("[T{}] {} ← {} ({} 行)", r["tier"],
                r["name"].as_str().unwrap_or(""), r["path"].as_str().unwrap_or(""),
                r["lines"].as_u64().unwrap_or(0)))
        .collect()
}

/// Absolute prefixes that are already covered: the base of every built-in
/// pattern, plus everything the user's own sources point at.
pub(crate) fn covered_prefixes() -> Vec<String> {
    covered_with_names().into_iter().map(|(_, p)| p).collect()
}

/// The same list, but carrying which source owns each prefix — so refusing an
/// add can say *why* instead of just "no".
pub(crate) fn covered_with_names() -> Vec<(String, String)> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out: Vec<(String, String)> = Vec::new();
    for (agent, pats) in source_patterns() {
        let name = AGENT_NAMES.get(agent as usize).copied().unwrap_or("").to_string();
        for p in pats {
            let literal = p.split('*').next().unwrap_or(&p).trim_end_matches('/').to_string();
            if literal.is_empty() { continue; }
            // both the literal directory and the source's home-level base
            out.push((name.clone(), literal.clone()));
            let rel = literal.strip_prefix(&home).unwrap_or(&literal).trim_start_matches('/');
            let mut comps = rel.split('/');
            if let (Some(a), Some(b)) = (comps.next(), comps.next()) {
                if a.starts_with('.') || b == "Application Support" {
                    out.push((name.clone(), format!("{home}/{a}")));
                }
            }
        }
    }
    for c in load_custom().sources {
        let abs = expand_tilde(&c.path);
        let literal = abs.split('*').next().unwrap_or(&abs).trim_end_matches('/').to_string();
        if !literal.is_empty() { out.push((c.name.clone(), literal)); }
    }
    out
}

/// Is this path already being read by something we count? Returns the owner.
pub(crate) fn cover_owner(path: &str) -> Option<String> {
    let abs = expand_tilde(path);
    covered_with_names().into_iter()
        .find(|(_, prefix)| abs.starts_with(prefix))
        .map(|(name, _)| name)
}

/// Attribute a found file to a product name, using its own path: the first
/// "meaningful" directory below the standard root, else the file's own folder.
/// `~/.local/state/goose/logs/llm_request.1.jsonl` → ("Goose", "goose").
pub(crate) fn name_from_path(path: &str, root: &str) -> (String, String) {
    pub(crate) const GENERIC: &[&str] = &["logs", "log", "cache", "caches", "data", "sessions", "session",
                               "state", "storage", "tmp", "temp", "user", "users", "db", "sqlite",
                               "history", "telemetry", "metrics", "traces", "usage", "records"];
    let rel = path.strip_prefix(root).unwrap_or(path).trim_start_matches('/');
    let mut parts = rel.split('/').filter(|s| !s.is_empty());
    let mut chosen = parts.next().unwrap_or("").to_string();
    if GENERIC.contains(&chosen.to_lowercase().as_str()) {
        chosen = parts.next().unwrap_or(&chosen).to_string();
    }
    if chosen.contains('.') && !chosen.starts_with('.') {
        // looks like a bundle id (com.example.app) — its last component reads better
        chosen = chosen.rsplit('.').next().unwrap_or(&chosen).to_string();
    }
    let mut name: String = chosen.chars().take(24).collect();
    if let Some(first) = name.chars().next() {
        if first.is_ascii_lowercase() {
            name = first.to_ascii_uppercase().to_string() + &name[first.len_utf8()..];
        }
    }
    let id: String = name.to_lowercase().chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    (name, id.trim_matches('-').to_string())
}

/// A found file becomes a pattern that survives log rotation: the trailing
/// number of `llm_request.1.jsonl` turns into `*`, and `$HOME` becomes `~`.
/// Nothing else about the path is guessed.
pub(crate) fn register_path(path: &str) -> String {
    let p = Path::new(path);
    let (dir, stem, ext) = (
        p.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default(),
        p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        p.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
    );
    let trimmed = stem.trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end_matches(['.', '_', '-']).to_string();
    if trimmed != stem && !trimmed.is_empty() {
        let short_dir = short_home(&dir);
        format!("{short_dir}/{trimmed}*{}{ext}", if ext.is_empty() { "" } else { "." })
    } else {
        // Not a rotated log: a *session* store. Those add a new file per
        // conversation, so registering the single file we happened to find would
        // stop counting at the next one. If the parent directory is one of the
        // names such stores use, take the whole directory instead.
        let dir_name = Path::new(&dir).file_name().map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default().to_lowercase();
        const SESSION_DIRS: &[&str] = &["sessions", "session", "transcripts", "conversations",
                                        "chats", "history", "logs", "runs"];
        if !ext.is_empty() && SESSION_DIRS.contains(&dir_name.as_str()) {
            format!("{}/*.{ext}", short_home(&dir))
        } else {
            short_home(path)
        }
    }
}

/// The whole search: a product name in, candidate files (ranked by how many
/// lines actually parsed) out. `checked` lists every root we looked in, so "not
/// found" is auditable instead of a shrug.
pub(crate) fn probe_agent(name: &str) -> Value {
    let (roots, derived) = probe_roots(name);
    // Use the *same* scanner as content discovery, just with the roots narrowed
    // to the ones this name implies. The first version had its own shallow walk
    // (depth ≤ 3, 800 files) and it showed: typing "VClaw" reached the right
    // directory and still parsed nothing, because its sessions sit four levels
    // down, and typing "codex" found only `archived_sessions/` while missing the
    // live `sessions/2026/09/15/`. Two search paths with different depth limits
    // is how the same machine gives two different answers.
    let mut files: Vec<Value> = Vec::new();
    for r in &roots {
        for (group, nfiles, lines, bytes, sample, example) in scan_location(r, 4_000) {
            files.push(json!({"path": example, "group": group, "files": nfiles,
                              "parsed_lines": lines, "bytes": bytes, "sample": sample}));
        }
    }
    files.sort_by(|a, b| b["parsed_lines"].as_u64().cmp(&a["parsed_lines"].as_u64()));
    let best = files.first().cloned();
    // "claude" has to match "Claude Code" — comparing the whole string made the
    // most obvious thing a user types look like a new source.
    let typed = name.trim();
    let builtin = AGENT_NAMES.iter().any(|n| {
        let first = n.split_whitespace().next().unwrap_or(n);
        n.eq_ignore_ascii_case(typed) || first.eq_ignore_ascii_case(typed)
    });
    json!({
        "name": name.trim(),
        "builtin": builtin,
        "derived_names": derived,
        "checked": roots,
        "candidates": files.iter().take(5).cloned().collect::<Vec<_>>(),
        "best": best,
    })
}

// ---------- source registry + the three-tier discovery ----------
//
// The order of work, cheapest first, with one shared "claimed" set so the same
// data can never be counted twice:
//
//   1. registry entries **with paths** — a handful of `stat`/glob calls. This is
//      the cache: we already know where these live.
//   2. registry entries **with a name only** — searched by name (app bundle id →
//      binary → name variants → standard roots → parse).
//   3. **recursive content scan** over the standard locations — the fallback that
//      needs no name at all, and the only one that can find a tool nobody has
//      heard of yet.
//
// A path is claimed when it is (a) already covered by a built-in parser or an
// existing custom source, or (b) claimed by an earlier tier in this run. Tier 3
// therefore only ever sees what tiers 1 and 2 did not take.

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RegEntry {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) paths: Vec<String>,
    #[serde(default)]
    pub(crate) note: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Registry {
    #[serde(default = "one")]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) updated: String,
    /// Where this list was fetched from, so the page can show its origin and
    /// offer the same URL again next time instead of an empty box.
    #[serde(default)]
    pub(crate) source_url: String,
    #[serde(default)]
    pub(crate) sources: Vec<RegEntry>,
}

/// Shipped seed. `~/.tokendance/registry.json` (written by the update endpoint)
/// is merged on top of it, so a fetched list can add names without a release.
pub(crate) const SEED_REGISTRY: &str = include_str!("registry.json");

/// 名单的默认地址：环境变量（app 起本地服务时会把它设成 telURL + /registry.json），
/// 否则是生产服务器——**不是** 127.0.0.1，那在用户机器上不存在。
/// 名单地址：**没有内置默认值**，由拉起它的应用注入
/// （`AppMain.swift` 里 `ensureServerRunning()` 会设置 `TOKENDANCE_REGISTRY_URL`，
/// 值来自客户端唯一的那个 `ServiceConfig.base`）。
///
/// 为什么要这样：客户端里"远端地址"只应该有一个出处。以前这里还写死了一份
/// 官方地址，于是自建/改地址时会有两个地方要对齐（2026-09-16 用户要求"网络请求都配置化"）。
/// 单独跑这个本地服务（开发时）没有注入的话，名单相关功能会明确报"未配置地址"，
/// 而不是偷偷去连某个写死的域名。
pub(crate) fn default_registry_url() -> String {
    std::env::var("TOKENDANCE_REGISTRY_URL").ok()
        .filter(|s| s.starts_with("http"))
        .unwrap_or_default()
}

pub(crate) fn registry_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".tokendance/registry.json")
}

/// Seed + local overlay, deduplicated by name (the overlay wins).
pub(crate) fn seed_registry() -> Registry {
    serde_json::from_str(SEED_REGISTRY).unwrap_or(Registry {
        version: 1, updated: String::new(), source_url: String::new(), sources: Vec::new(),
    })
}

pub(crate) fn load_registry() -> Registry {
    let mut reg = seed_registry();
    if let Ok(text) = std::fs::read_to_string(registry_path()) {
        if let Ok(local) = serde_json::from_str::<Registry>(&text) {
            for e in local.sources {
                match reg.sources.iter_mut().find(|x| x.name.eq_ignore_ascii_case(&e.name)) {
                    Some(slot) => *slot = e,
                    None => reg.sources.push(e),
                }
            }
            if !local.updated.is_empty() { reg.updated = local.updated; }
            if !local.source_url.is_empty() { reg.source_url = local.source_url; }
        }
    }
    reg
}

/// Resolve `$VAR` and `~` in a registry path.
pub(crate) fn expand_registry_path(p: &str) -> String {
    let mut s = p.to_string();
    if let Some(rest) = s.strip_prefix('$') {
        if let Some((var, tail)) = rest.split_once('/') {
            if let Ok(v) = std::env::var(var) { s = format!("{v}/{tail}"); }
        } else if let Ok(v) = std::env::var(rest) { s = v; }
    }
    expand_tilde(&s)
}

/// The three tiers, in order. `apply` false = report only (the `--discover` CLI);
/// true = also write the new sources to `sources.json`.
pub(crate) fn discover_tiered(apply: bool) -> Vec<Value> {
    let reg = load_registry();
    let mut claimed: Vec<(String, String)> = covered_with_names();
    let mut seen_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::new();
    let mut cfg = load_custom();
    let mut dirty = false;

    let mut accept = |name: &str, group: &str, files: u64, lines: u64, bytes: u64,
                      sample: &Value, example: &str, tier: u8,
                      claimed: &mut Vec<(String, String)>, seen: &mut std::collections::HashSet<String>,
                      cfg: &mut CustomFile, dirty: &mut bool| {
        let abs = expand_tilde(example);
        // Compare paths case-insensitively: the default macOS filesystem is
        // case-insensitive, and tier 2 builds its roots out of the *name* the
        // registry holds, so probing "Goose" returns `~/.local/state/Goose/…`
        // for a directory actually spelled `goose`. Without folding case here the
        // same file came back a second time under tier 3 and looked like a
        // different source.
        let key = abs.to_lowercase();
        if seen.contains(&key) { return; }
        let id: String = name.to_lowercase().chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let id = id.trim_matches('-').to_string();
        // Report every hit, including the ones already counted — a discovery
        // report that hides its own hits is not auditable. `claimed_by` says who
        // already owns the path, so the reader can see the layering work.
        let owner = claimed.iter().find(|(_, p)| key.starts_with(&p.to_lowercase())).map(|(n, _)| n.clone());
        if owner.is_none() {
            let prefix = abs.split('*').next().unwrap_or(&abs).trim_end_matches('/').to_string();
            claimed.push((name.to_string(), prefix.to_lowercase()));
        }
        seen.insert(key);
        // `added` is what a caller must use to decide whether this is news: the
        // report lists every hit so the layering is auditable, but only the rows
        // with added = true were written to sources.json. Returning the whole
        // report as "discovered and added" made the Settings page claim it had
        // just added Codex, Claude Code and WorkBuddy — the three it already had.
        let mut was_added = false;
        if apply && owner.is_none() && !id.is_empty()
            && !cfg.sources.iter().any(|c| c.id == id) && !cfg.removed.contains(&id) {
            cfg.sources.push(CustomSource { id: id.clone(), name: name.to_string(), enabled: true,
                path: register_path(&abs), format: "jsonl-usage".into(), fields: HashMap::new() });
            if !cfg.discovered.contains(&id) { cfg.discovered.push(id); }
            *dirty = true;
            was_added = true;
        }
        out.push(json!({"tier": tier, "name": name, "group": group, "files": files,
                        "lines": lines, "bytes": bytes, "sample": sample,
                        "path": short_home(&abs), "claimed_by": owner, "added": was_added}));
    };

    // ---- tier 1: known paths (the cache) -------------------------------------
    for e in &reg.sources {
        for p in &e.paths {
            let root = expand_registry_path(p);
            if !Path::new(&root).exists() { continue; }
            for (group, files, lines, bytes, sample, example) in scan_location(&root, 4_000) {
                accept(&e.name, &group, files, lines, bytes, &sample, &example, 1,
                       &mut claimed, &mut seen_files, &mut cfg, &mut dirty);
            }
        }
    }
    // ---- tier 2: name only ---------------------------------------------------
    for e in &reg.sources {
        if e.paths.iter().any(|p| Path::new(&expand_registry_path(p)).exists()) { continue; }
        let (roots, _) = probe_roots(&e.name);
        if roots.is_empty() { continue; }
        let probe = probe_agent(&e.name);
        let Some(best) = probe["best"].as_object() else { continue };
        accept(&e.name, &e.name,
               best["files"].as_u64().unwrap_or(1), best["parsed_lines"].as_u64().unwrap_or(0),
               best["bytes"].as_u64().unwrap_or(0), &best["sample"],
               best["path"].as_str().unwrap_or(""), 2,
               &mut claimed, &mut seen_files, &mut cfg, &mut dirty);
    }
    // ---- tier 3: content scan (no names at all) ------------------------------
    for row in discover_scan(false) {
        let example = row["example"].as_str().unwrap_or("");
        let group = row["group"].as_str().unwrap_or("");
        if example.is_empty() || group.is_empty() { continue; }
        accept(group, group, row["files"].as_u64().unwrap_or(0), row["lines"].as_u64().unwrap_or(0),
               row["bytes"].as_u64().unwrap_or(0), &row["sample"],
               &expand_tilde(example), 3,
               &mut claimed, &mut seen_files, &mut cfg, &mut dirty);
    }
    if apply && dirty {
        cfg.version = 1;
        let _ = save_custom(&cfg);
    }
    out
}
