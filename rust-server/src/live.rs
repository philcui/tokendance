//! 实时聚合：今日用量、近 60 秒燃烧速率、命中率。
//
// 由 `main.rs` 拆分而来，逻辑未改动。

use crate::*;
use chrono::TimeZone;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

// ---------- /api/live ----------

/// Start of the current CST day, in epoch ms.
pub(crate) fn cst_day_start_ms(now: i64) -> i64 {
    let now_cst = cst().timestamp_opt(now / 1000, 0).unwrap();
    let d = now_cst.date_naive();
    cst().from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .unwrap()
        .timestamp_millis()
}

/// Burn / activity / today-totals for a given moment.
///
/// `recs` **must be sorted ascending by `t`**. That invariant is what lets the
/// three windows be located by binary search instead of a full scan: the 60 s
/// and 10 s burn windows and the "today" window are all contiguous *suffixes* of
/// a sorted history, so their starts are two `partition_point` calls and the
/// loops only touch the records that actually belong to them.
///
/// This matters because `/api/live` is polled once per second by the menubar app
/// forever. The original scanned every record on every call, so the cost of that
/// endpoint grew with total history — O(history) per second, for a number that
/// only ever depends on the last minute and today. At 25 k records that is
/// already ~25 k iterations/s; at a year, ~200 k/s.
///
/// Split out from `live_stats` (which only supplies `now` and the atomics) so
/// the windowing can be tested against a brute-force reference.
/// How far ahead of the server's own clock a record's stamp may be before it
/// stops counting.
///
/// A record is stamped by the writer and read by us a moment later, so its
/// timestamp can legitimately trail `now` by a fraction of a second but cannot
/// lead it. The allowance covers clock granularity and the gap between the write
/// and the read.
pub(crate) const CLOCK_SKEW_MS: i64 = 5_000;

/// Burn / activity / today-totals for a given moment.
///
/// `recs` **must be sorted ascending by `t`**. That invariant is what lets the
/// three windows be located by binary search instead of a full scan: the 60 s
/// and 10 s burn windows and the "today" window are all contiguous *suffixes* of
/// a sorted history, so their starts are two `partition_point` calls and the
/// loops only touch the records that actually belong to them.
///
/// This matters because `/api/live` is polled once per second by the menubar app
/// forever. The original scanned every record on every call, so the cost of that
/// endpoint grew with total history — O(history) per second, for a number that
/// only ever depends on the last minute and today. At 25 k records that is
/// already ~25 k iterations/s; at a year, ~200 k/s.
///
/// Split out from `live_stats` (which only supplies `now` and the atomics) so
/// the windowing can be tested against a brute-force reference.
pub(crate) fn live_stats_from(recs: &[Record], now: i64, write_ago_ms: u64) -> Value {
    let day_start = cst_day_start_ms(now);
    // Every window is a closed interval *ending now*, not a half-open ray into
    // the future:
    //
    //     age ∈ [-CLOCK_SKEW_MS, WIDTH)
    //
    // Three things are load-bearing here.
    //
    // The upper bound is not decoration. With only a lower bound, a record whose
    // timestamp lies ahead of the clock sits inside every window forever, so a
    // single bad stamp counts in "the last 60 seconds" and in "today"
    // indefinitely and keeps the menubar ring spinning with no work happening.
    // Measured with a record dated 30 days ahead: `today.n` 10 → 12 and
    // `burn60` 20 000 000 → 24 000 000.
    //
    // The window is exactly 60 whole seconds, so `burn60` is the sum of the 60
    // chart buckets and nothing else. Widening it by a millisecond (including
    // age == 60 000) leaves one record that the burn total counts and the chart
    // cannot show, because there is no 61st bucket — the dashboard used to do
    // exactly that, adding the record to `b60` while writing `buckets[-1]` into
    // nowhere.
    //
    // The allowance reaches backwards past zero so a stamp a little ahead of the
    // clock counts as "just happened" rather than being dropped.
    let horizon = now + CLOCK_SKEW_MS;
    let win_end = recs.partition_point(|r| r.t <= horizon);
    let win60 = recs.partition_point(|r| r.t <= now - 60_000).min(win_end);
    let win10 = recs.partition_point(|r| r.t <= now - 10_000).min(win_end);
    let day_at = recs.partition_point(|r| r.t < day_start).min(win_end);

    let mut b60 = 0i64;
    let mut b10 = 0i64;
    let mut buckets = [0i64; 60];
    let mut active = std::collections::HashSet::new();
    for r in &recs[win60..win_end] {
        // 燃烧 = 输入 + 输出，和"今日 TOKEN 总量"、挂件上那个大数字同一口径。
        //
        // 它曾经是 `输出 +（输入 − 缓存读）`，理由是缓存读在各家都打折
        // （OpenAI 约 5 折、Anthropic 缓存读约 1 折、DeepSeek 命中约 1 折），
        // 于是这个数近似"按全价计费的那部分"。问题是：**那是"钱"的口径，
        // 不是 token 的口径**——业界（各家控制台、第三方用量工具）展示的是
        // 输入/缓存/输出分类 + 费用，没有把"输出 + 非缓存输入"当成头号指标的
        // 做法；而我们没有按模型定价的能力，所以这个近似只会让"消耗了多少
        // token"这个最直白的问题答非所问（本机缓存命中 99%，差距近 100 倍）。
        // Saturating, not `+`: see MAX_RECORD_TOKENS. A release build wraps
        // silently here (measured before the clamp: today.t −8,446,744,073,709,551,616),
        // and a wrapped total is indistinguishable from a real one on screen.
        let burn = r.i.saturating_add(r.o);
        b60 = b60.saturating_add(burn);
        active.insert(r.a);
        // Index 0 is the oldest second of the minute and 59 the current one,
        // matching the dashboard's own bucketing. The age is clamped rather
        // than trusted: a stamp a little ahead of `now` belongs in the current
        // bucket, and letting it index past the end would be a panic, not a
        // wrong number.
        let age = (now - r.t).clamp(0, 59_999);
        buckets[59 - (age / 1000) as usize] =
            buckets[59 - (age / 1000) as usize].saturating_add(burn);
    }
    for r in &recs[win10..win_end] {
        b10 = b10.saturating_add(r.i.saturating_add(r.o));
    }
    let (mut t_n, mut t_i, mut t_o, mut t_c) = (0i64, 0i64, 0i64, 0i64);
    // Per-agent "today", so the menubar HUD's agent rows can follow the same 1 s
    // cadence as the rest of this payload. They used to be derived from the
    // client's own 60 s aggregate, which meant the rows could be missing for up
    // to a minute after the first call of a new day (the day rolls over, every
    // count goes to zero, and the next recompute is up to 60 s away).
    let mut t_agent: Vec<[i64; 4]> = Vec::new();
    for r in &recs[day_at..win_end] {
        t_n = t_n.saturating_add(1);
        t_i = t_i.saturating_add(r.i);
        t_o = t_o.saturating_add(r.o);
        t_c = t_c.saturating_add(r.c);
        let idx = r.a as usize;
        if t_agent.len() <= idx {
            t_agent.resize(idx + 1, [0; 4]);
        }
        t_agent[idx][0] = t_agent[idx][0].saturating_add(1);
        t_agent[idx][1] = t_agent[idx][1].saturating_add(r.i);
        t_agent[idx][2] = t_agent[idx][2].saturating_add(r.o);
        t_agent[idx][3] = t_agent[idx][3].saturating_add(r.c);
    }
    // Newest record that is not ahead of the clock. Zero (→ `null` below) when
    // every record is in the future, which is the honest answer: we have seen no
    // activity yet.
    let last_t = if win_end == 0 { 0 } else { recs[win_end - 1].t };

    json!({
        "burn60": b60,
        "burn10": b10,
        "buckets": &buckets[..],
        // Ids are record ids, so the array is sparse when a source has no
        // activity today — the client reads it by index and skips zeros.
        "today_agents": t_agent.iter().map(|v| json!(v)).collect::<Vec<Value>>(),
        "active": active.len(),
        // Clamped at zero: a record stamped just ahead of `now` has "just
        // happened", not "happened in minus 3 seconds", and the menubar app
        // decides whether the ring turns by testing this against a threshold.
        "last_ago_s": if last_t > 0 {
            Some((((now - last_t) as f64 / 1000.0).max(0.0) * 10.0).round() / 10.0)
        } else { None },
        "write_ago": (write_ago_ms as f64 / 1000.0 * 10.0).round() / 10.0,
        // `t` is the headline figure both UIs show: input + output, which is
        // what "total tokens" means everywhere else (an OpenAI-style usage
        // page, a provider dashboard). It is *not* the billable figure — with
        // prompt caching, most of `i` is a cache read — so `i`, `o` and `c`
        // stay separate for the burn line and the hit rate. The server owns the
        // definition so the HUD and the dashboard cannot drift apart again.
        // `t` is the headline figure both UIs show, so it saturates too: with
        // both halves at i64::MAX the plain `+` would wrap to −2.
        "today": {"n": t_n, "i": t_i, "o": t_o, "c": t_c, "t": t_i.saturating_add(t_o),
                  "rate": if t_i > 0 { (t_c as f64 / t_i as f64 * 1000.0).round() / 10.0 } else { 0.0 }},
    })
}

pub(crate) fn live_stats(state: &AppState) -> Value {
    let now = now_ms();
    let recs = lock(&state.records);
    live_stats_from(&recs, now, state.latest_write_ago_ms.load(Ordering::Relaxed))
}
