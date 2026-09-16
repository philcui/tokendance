# Contributing

This repository is the **client**: a Swift menu-bar app, four web pages, and a local Rust service
that reads agent transcripts into SQLite. The official telemetry / update / leaderboard service is
a separate private service and its code is not here — see the README for what that means for the
network surface.

## Build and test

```bash
./scripts/build_app.sh              # → build/TokenDance.app (Swift app + Rust server, bundled)
cd rust-server && cargo test        # the parser / store / API test suite
./scripts/privacy_check.sh          # before pushing: no personal traces, and it really builds
```

Requirements: macOS 13+, Xcode command line tools (`swiftc`), Rust (`cargo`).

## Where things live

| Path | What |
|---|---|
| `AppMain.swift` | the app delegate: menu, HUD lifecycle, spawning and supervising the local server |
| `HUDViews.swift`, `RingRenderer.swift`, `Leaderboard.swift`, `Telemetry.swift`, `Updater.swift` | the HUD, the edge-docked ring, the leaderboard client, the ping, the in-app updater |
| `dashboard.html` `settings.html` `sources.html` `about.html` | the four pages the local server serves |
| `rust-server/src/parsers.rs` | one function per agent: transcript line → `Record` |
| `rust-server/src/ingest.rs` | which files are watched, and how they are tailed incrementally |
| `rust-server/src/discover.rs` | where a source may live, the registry, the three-tier search |
| `rust-server/src/store.rs` | SQLite schema, and every migration that has ever run |
| `rust-server/src/api.rs` | the HTTP surface |
| `vendor/` | third-party assets served from this app, so no page ever loads a CDN |

## Adding an agent

Start by not writing any code: run 设置 → 重新扫描数据源. Known paths are checked first, then the
tool is searched for by name, then the standard locations are scanned for content that parses. If
the tool writes readable usage anywhere under `$HOME`, it is frequently picked up with no code
change at all.

When that is not enough:

1. `model.rs` — add the name to `AGENT_NAMES`. **The index is the `a` column of every stored row**,
   so append rather than insert, and add a migration in `store.rs` if you must renumber. A retired
   agent's slot is removed and everything after it shifts down; `parser_repair_v6` is the worked
   example, including the "delete, don't rewrite" reasoning.
2. `parsers.rs` — add a `parse_line` arm. Unknown ids return `None`, which means a mis-dispatch
   *under*-counts rather than mislabels. Do not fall back to another parser.
3. `ingest.rs` — add the file globs for that agent.
4. `rust-server/src/registry.json` — add an entry so discovery can find it. `paths` when you know
   where it lives, `name` alone when you don't; the tiers use whichever you gave.
5. The pages (`sources.html`, `dashboard.html`, `settings.html`) — a chip / icon, and the i18n keys
   in every language the file carries.
6. Tests — see below.

## House rules

**Fixtures are synthetic.** No real transcripts, no real paths, no real project names, no real
usage figures, in tests or in comments. There is a parser test that exists because a real working
directory was committed once. If you need a realistic shape, invent one — `/Users/alice/…`,
`example-repo`, and a made-up session folder are all in the test files already.

**Numbers get checked against the real thing.** A parser that passes its unit test against a
sample you also wrote proves very little. The bugs worth remembering here were found by comparing
against the tool itself: pi's `input` not including cache reads (hit rate above 100%), one
directory producing two project names, a handler that stopped the whole server because it did
blocking I/O on an async worker. Say in the pull request how you checked, and what you saw.

**Comments explain the measurement, not the code.** "This is the sort order" is visible from the
line below; "sorted by tail time, because the file is 200 MB and re-reading it per request cost
2 s" is not. Several files here carry the numbers that justify a decision and the counter-example
that would have been the bug — that is the house style, and it is deliberate.

**Search before you add.** Four copies of the same list was a real bug here, as was a second
hand-written position calculation. If something is already derived somewhere, derive it there.

**Do not put a CDN, a font, or an analytics snippet in a page.** `vendor/` exists for that, and
`scripts/privacy_check.sh` fails on an outside `<script>`/`<link>` for good reason.

## Pull requests

Run the three commands above. CI runs the same ones (plus a build) on a macOS runner. Keep a pull
request to one idea; if you found a second problem on the way, say so in the description and open
it separately.

For a new data source, the *path* and a redacted sample of one record are the two things that make
review possible in one round trip — the issue template asks for exactly those.
