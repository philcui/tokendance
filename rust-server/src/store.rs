//! SQLite 落盘层：表结构、迁移、以及读写整张 records 表的唯一入口。
//
// 由 `main.rs` 拆分而来，逻辑未改动。

use crate::*;
use chrono::Utc;
use rusqlite::params;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

// ---------- SQLite store ----------

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS records(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  a INTEGER NOT NULL, m TEXT NOT NULL, t INTEGER NOT NULL,
  i INTEGER NOT NULL, o INTEGER NOT NULL, c INTEGER NOT NULL,
  s TEXT NOT NULL, p TEXT NOT NULL,
  UNIQUE(a,m,t,i,o,c,s,p)
);
CREATE INDEX IF NOT EXISTS idx_records_t ON records(t);
CREATE TABLE IF NOT EXISTS file_state(path TEXT PRIMARY KEY, agent INTEGER NOT NULL, off INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT NOT NULL);
";

/// One-time data repairs, each guarded by a marker row in `kv` so it runs at most
/// once per database. They are safe to leave in the binary forever.
///
/// Rows are dropped rather than UPDATE-ed because every defect lived in the
/// *parser*, so the only trustworthy repair is to forget the derived rows and let
/// the incremental scanner rebuild them from the source transcripts. `file_state`
/// must be cleared alongside, otherwise the offsets would resume at EOF and
/// nothing would be re-read. `codex_meta` / `oc_state` are dropped too: they carry
/// per-session state (cwd, model, cumulative-token baseline) that the rebuild
/// re-derives, and a stale baseline would either hide history or double count it.
///
/// `parser_repair_v1` covers four measured defects:
///   1. Codex model was written as "unknown" for all 20,330 rows — the marker is
///      read from `payload.type` but lives on the top-level object. 81% of the
///      store had no model.
///   2. Session keys were a prefix of the file name / uuid. Every rollout file
///      starts with "rollout-<date>T", so 42 Codex sessions became 2 keys; and
///      UUIDv7 timestamps made the first 8 hex chars collide, so 42 sessions
///      became 33. Claude Code and WorkBuddy keys changed with the same rule.
///   3. OpenCode cache reads were never read, so its hit rate was always 0%.
///   4. Codex "context snapshot" events (no tokens at all) counted as calls.
pub(crate) const MIGRATIONS: &[(&str, &str)] = &[
    ("parser_repair_v1",
     "DELETE FROM records;
      DELETE FROM file_state;
      DELETE FROM kv WHERE k IN ('codex_meta','oc_state');"),
    // v2: Claude Code project names. The old parser took the transcript's
    // *parent directory* as the project, but Claude Code encodes the cwd by
    // replacing every non-alphanumeric character with '-', which is lossy:
    // Chinese directory names became bare runs of dashes
    // (`Users-alice-Desktop-proj---`), and subagent transcripts — which
    // live one level deeper — were labelled `subagents`. The project is now the
    // longest common prefix of the `cwd` values inside the transcript, so only
    // Claude Code's rows are stale; scoping the delete to agent 2 keeps the
    // repair cheap, since a blanket rescan would re-parse every Codex rollout
    // (one of them is 253 MB) to recompute values that never changed.
    ("parser_repair_v2",
     "DELETE FROM records WHERE a=2;
      DELETE FROM file_state WHERE agent=2;
      DELETE FROM kv WHERE k='proj_roots';"),
    // v3: Codex session keys for *continued* threads. The desktop app continues
    // a thread in a new rollout file named `<original-uuid>_<new-uuid>.jsonl`,
    // and the derivation fell through to a prefix of the original uuid — so one
    // conversation was stored as two sessions (and re-started from 0 offsets
    // under the second key as it kept running). Codex rows are dropped so the
    // scanner re-derives them; scoped to agent 1, and all 45 Codex files are
    // ~390 MB / ~2 s of re-parse, measured with `--scan`.
    ("parser_repair_v3",
     "DELETE FROM records WHERE a=1;
      DELETE FROM file_state WHERE agent=1;"),
    // v4: pi's session and project were both derived from the *file name* and
    // the *directory name*, with a hard `take(8)` / `take(16)` cut. Real pi
    // files are `<ISO timestamp>_<uuid>.jsonl` inside a directory named after
    // the cwd with every '/' turned into '-' (`--Users-alice-proj--`), so every
    // row stored a session id of `2026-09-` and a project of `--Users-alice-…`.
    // Only synthetic fixtures (`s01.jsonl`) ever exercised those two lines
    // before — which is exactly why this survived 36 passing tests. Scoped to
    // agent 6; pi transcripts are tiny (a few KB each).
    ("parser_repair_v4",
     "DELETE FROM records WHERE a=6;
      DELETE FROM file_state WHERE agent=6;"),
    // v5: pi's input was stored as the *non-cached* prompt only. pi's usage
    // object lists `input` and `cacheRead`/`cacheWrite` as parallel fields (its
    // own total is `input + output + cacheRead + cacheWrite`), so the stored
    // input was short by the cache reads — and the cache-hit rate, which is
    // c/i, could exceed 100% (measured: 667.5% on the HUD). Same scope as v4.
    ("parser_repair_v5",
     "DELETE FROM records WHERE a=6;
      DELETE FROM file_state WHERE agent=6;"),
    // v6: Gemini CLI retired upstream. Its slot (3) is removed from
    // `AGENT_NAMES`, so every agent id above it shifts down by one — records and
    // per-file offsets both. Scoped to the id reshuffle only: no agent's *data*
    // changes meaning, so nothing is reparsed. (Gemini had never matched a real
    // transcript on this machine; any row it ever stored is deleted here.)
    ("parser_repair_v6",
     "DELETE FROM records WHERE a=3;
      DELETE FROM file_state WHERE agent=3;
      UPDATE records SET a=a-1 WHERE a>3;
      UPDATE file_state SET agent=agent-1 WHERE agent>3;"),
    // v7: OpenCode stored `tokens_input` as the whole input, but that column is
    // the *non-cached* prompt (its own message payloads prove it: total =
    // input + output + cache read + cache write), so every OpenCode row was
    // short by its cache reads — and `c <= i` only held by luck. Rows and the
    // delta baseline are dropped together: with no baseline and no rows left,
    // the scan re-emits each session's cumulative total once, now with the
    // correct arithmetic. Scoped to agent 4.
    ("parser_repair_v7",
     "DELETE FROM records WHERE a=4;
      DELETE FROM file_state WHERE agent=4;
      DELETE FROM kv WHERE k='oc_state';"),
    // v8: read OpenCode's database from a snapshot copy (db + -wal + -shm)
    // instead of the live file. On this machine the main file is 4 KB while
    // 1.1 MB — including the schema — lives in the WAL, so a reader that opens
    // the file alone sees an empty database. Re-derive so the rows come from the
    // snapshot path.
    ("parser_repair_v8",
     "DELETE FROM records WHERE a=4;
      DELETE FROM kv WHERE k='oc_state';"),
    // v9: one directory, one project name. Codex stored the *last two path
    // components* (`2024-01-15-09-30-00/example-repo`) while pi and Claude Code
    // stored the home-shortened full path for the same directory, so a project
    // two agents had worked on showed up twice in the dashboard's project
    // filter — exactly what the user reported. WorkBuddy stored the session
    // folder's tail (`2024-01-15-09-30-00`) and OpenCode stored the literal
    // string "opencode"; both now use the cwd from the transcript.
    //
    // The labels cannot be repaired in place: `2024-01-15-09-30-00/example-repo`
    // does not say which parent it had, so there is nothing to expand it to.
    // The rows are dropped and re-derived from the transcripts instead, which is
    // the same approach v2/v3/v4 used for label changes. Cost nothing worth
    // mentioning: Codex re-reads ~390 MB in ~2 s (v3 measured that), WorkBuddy
    // ~93 MB, OpenCode is a handful of rows plus its state baseline.
    ("parser_repair_v9",
     "DELETE FROM records WHERE a IN (0, 1, 4);
      DELETE FROM file_state WHERE agent IN (0, 1);
      DELETE FROM kv WHERE k IN ('proj_roots','oc_state');"),
    // v10: Kimi's session id was the *file name*, and that name is a constant
    // (`context.jsonl` / `wire.jsonl`) — every session on a machine collapsed into
    // one bucket called "context"/"wire". The project was the session directory
    // instead of the `<wd-key>` above it. Both now come from the path
    // (`sessions/<wd-key>/<session>/…`), which means old rows would sit next to
    // the corrected ones and be counted twice, so they are dropped and re-derived
    // (same approach as v4/v5 did for pi). Kimi transcripts are small; a machine
    // with none re-reads nothing at all.
    ("parser_repair_v10",
     "DELETE FROM records WHERE a=7;
      DELETE FROM file_state WHERE agent=7;"),
];

/// Lock a mutex, recovering from poisoning instead of panicking.
///
/// `Mutex::lock` returns `Err` for the rest of the process' life once any
/// thread has panicked while holding that lock, and each later `.unwrap()`
/// re-panics — so one bad line anywhere would permanently brick a daemon that is
/// meant to run for months, and both the dashboard and the HUD would simply
/// stop working until it was restarted by hand. Taking the guard back out of
/// the poison error is safe here: every critical section in this file is a
/// whole-hog assignment, a `push`/`extend`, or a single SQL statement, none of
/// which can leave the guarded data half-updated.
pub(crate) struct Store {
    pub(crate) conn: Mutex<rusqlite::Connection>,
}

/// Applies every not-yet-recorded entry of MIGRATIONS, then stamps it. A failure
/// is logged but never fatal: a half-repaired database still works, it just keeps
/// the old (wrong) numbers until the next start retries.
pub(crate) fn run_migrations(conn: &rusqlite::Connection) {
    for (name, sql) in MIGRATIONS {
        let done: Option<String> = conn
            .query_row("SELECT v FROM kv WHERE k=?1", params![format!("migr:{name}")], |r| r.get(0))
            .ok();
        if done.is_some() {
            continue;
        }
        match conn.execute_batch(sql) {
            Ok(()) => {
                let _ = conn.execute(
                    "INSERT INTO kv(k, v) VALUES(?1, ?2)
                     ON CONFLICT(k) DO UPDATE SET v=?2",
                    params![format!("migr:{name}"), Utc::now().to_rfc3339()],
                );
                eprintln!("migration applied: {name}");
            }
            Err(e) => eprintln!("migration FAILED ({name}): {e}"),
        }
    }
}

impl Store {
    pub(crate) fn open(path: &Path) -> rusqlite::Result<Store> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        run_migrations(&conn);
        Ok(Store { conn: Mutex::new(conn) })
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> rusqlite::Result<Store> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    // returns true when the row is new (not a duplicate)
    pub(crate) fn insert(&self, r: &Record) -> bool {
        // the last gate before the disk: whatever a future call site forgets,
        // an absurd value cannot be stored (see MAX_RECORD_TOKENS)
        let r = sane(r.clone());
        let conn = lock(&self.conn);
        matches!(conn.execute(
            "INSERT OR IGNORE INTO records(a,m,t,i,o,c,s,p) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![r.a, r.m, r.t, r.i, r.o, r.c, r.s, r.p],
        ), Ok(n) if n > 0)
    }

    /// Drop every row that came from a user-added source (agent ids ≥ 10) plus
    /// their offsets. User sources are small, so re-deriving them is cheap — and
    /// necessary: ids are assigned by position in `sources.json`, so adding or
    /// removing one shifts the rest.
    pub(crate) fn delete_custom_records(&self) -> rusqlite::Result<()> {
        let conn = lock(&self.conn);
        conn.execute("DELETE FROM records WHERE a >= 10", [])?;
        conn.execute("DELETE FROM file_state WHERE agent >= 10", [])?;
        Ok(())
    }

    pub(crate) fn load_records(&self) -> Vec<Record> {
        let conn = lock(&self.conn);
        let mut stmt = match conn.prepare("SELECT a,m,t,i,o,c,s,p FROM records ORDER BY t") {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let rows = stmt.query_map([], |r| {
            Ok(Record {
                a: r.get::<_, i64>(0)? as u8,
                m: r.get(1)?,
                t: r.get(2)?,
                i: r.get(3)?,
                o: r.get(4)?,
                c: r.get(5)?,
                s: r.get(6)?,
                p: r.get(7)?,
            })
        });
        match rows {
            // rows written before the clamp existed are tamed here, so an old
            // bad row cannot reach the accumulators either
            Ok(it) => it.filter_map(Result::ok).map(sane).collect(),
            Err(_) => vec![],
        }
    }

    pub(crate) fn load_file_state(&self) -> HashMap<String, FileState> {
        let conn = lock(&self.conn);
        let mut stmt = match conn.prepare("SELECT path, agent, off FROM file_state") {
            Ok(s) => s,
            Err(_) => return HashMap::new(),
        };
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u8, r.get::<_, i64>(2)? as u64))
        });
        match rows {
            Ok(it) => it.filter_map(Result::ok)
                .map(|(p, a, o)| (p, FileState { agent: a, off: o })).collect(),
            Err(_) => HashMap::new(),
        }
    }

    pub(crate) fn save_file_states(&self, dirty: &[(String, u8, u64)]) {
        let conn = lock(&self.conn);
        for (path, agent, off) in dirty {
            let _ = conn.execute(
                "INSERT INTO file_state(path, agent, off) VALUES(?1,?2,?3)
                 ON CONFLICT(path) DO UPDATE SET agent=?2, off=?3",
                params![path, agent, *off as i64],
            );
        }
    }

    pub(crate) fn load_kv(&self, k: &str) -> Option<String> {
        let conn = lock(&self.conn);
        conn.query_row("SELECT v FROM kv WHERE k=?1", params![k], |r| r.get(0)).ok()
    }

    pub(crate) fn save_kv(&self, k: &str, v: &str) {
        let conn = lock(&self.conn);
        let _ = conn.execute(
            "INSERT INTO kv(k, v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=?2",
            params![k, v],
        );
    }
}
