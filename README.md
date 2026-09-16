# TokenDance

> 中文一句话：macOS 菜单栏上的 AI 编码 agent 用量仪表——把 Codex、Claude Code 等工具烧掉的
> token 变成一个随时能看一眼的数字。**用量数据全部在本机解析和保存**。
> 官方上报/更新/排行榜服务是另一个独立的私有服务，客户端只用 HTTP 跟它说话（见「网络行为」）。

A menu-bar HUD and web dashboard for the token your AI coding agents burn on macOS.
Everything is parsed and stored locally.

This repository is the whole client: a Swift menu-bar app, the four web pages it serves, and a
local Rust service that reads the agent transcripts already on disk and writes them to SQLite.
The official telemetry / update / leaderboard service is a **separate, private service**; the
client only ever reaches it over HTTP, and every call it makes is listed under
*Network behaviour* below — including how to point it somewhere else.

## What it does

- A floating HUD in the menu bar: today's total, live burn rate, cache hit rate, per-agent ranking.
  It can be dragged, collapsed to the number alone, or docked to the screen edge as a small ring.
- A web dashboard on `127.0.0.1:8737`: trends by day / source / project / model, a GitHub-style
  activity calendar, a paged call log, duration & idle analysis.
- Sources are discovered, not configured: known paths first, then a search by name, then a content
  scan. Agents installed later show up on their own.

Supported out of the box: WorkBuddy, Codex (CLI + desktop), Claude Code, Qwen Code, OpenCode,
pi, Kimi Code, iFlow, Qoder, Antigravity — plus anything the registry or the content scan finds
(Goose, for example, was found that way).

## Install

Download the zip from the release page, unzip, drag `TokenDance.app` to Applications.

The build is **ad-hoc signed, not notarised**, so the first launch is blocked by Gatekeeper:
right-click the app → *Open* → *Open* again, or run
`xattr -dr com.apple.quarantine /Applications/TokenDance.app`.
You only need to do this once.

## Build from source

Requirements: macOS 13+, Xcode command line tools (`swiftc`), Rust (`cargo`).

```bash
./scripts/build_app.sh          # → build/TokenDance.app  (compiles Swift + the Rust server, bundles both)
cd rust-server && cargo test    # 44 parser/store/API tests
```

The app carries the local server inside its bundle, so a built `.app` is self-contained:
no cargo, no node, no runtime to install on the target machine.

## Network behaviour

This is the part people want to check, so here it is explicitly. With the default settings the
client talks to `https://fanshitou.cn/tokendance` and nowhere else:

| When | Request | Carries |
|---|---|---|
| every 6 hours (on by default, one menu item turns it off) | `POST /api/ping` | a random install id (`u-` + 8 hex), app version, OS name and version, CPU architecture, UI language / theme, uptime in seconds |
| on launch and every 6 hours | `GET /api/version?ver=<current>` | the app version, so the service can answer "is there a newer release" |
| only if you opt in to the leaderboard | `POST /api/lb/submit`, `GET /api/lb/me?uid=…&days=…` | your display name and **daily totals** — never per-call data |
| rarely, when the source registry is refreshed | `GET /registry.json` | nothing |

**Never sent**: token counts (unless you join the leaderboard, and then only daily totals), file
names, project paths, session content, prompts or responses. The service does not log the IP
address of a ping — that log keeps only time, method, path and status.

You can point the client at your own implementation of those four endpoints:

```bash
# 运行时改（用户级，立即生效）
defaults write com.tokendance.app tb_tel_url "https://example.com/my-endpoint"

# 或者构建时改（打包进 app 的默认值，源码不用动）
SERVICE_BASE="https://example.com/my-endpoint" ./scripts/build_app.sh
```

`SERVICE_BASE` 是客户端里**唯一**的远端地址出处——匿名上报、更新检查、排行榜都挂在它下面，
名单地址（`<base>/registry.json`）由 app 拉起本地服务时注入。

The local service can be given a different registry with `TOKENDANCE_REGISTRY_URL`.

## Privacy

Parsing, storage and aggregation all happen on this machine. The local service binds
`127.0.0.1:8737` only and writes SQLite to `~/.tokendance/tokendance.db`. It reads agent
transcripts and nothing else: system privacy stores (contacts, messages, mail, Safari, Health,
Calendar, Reminders, iCloud Drive) are never entered — the recursive scan, the search-by-name
and the manual path box all refuse before touching the disk.

## What is *not* in this repository

The official telemetry / update / leaderboard service and the deployment configuration for it.
That half is closed. Nothing in the client depends on it beyond the four HTTP calls above, which
means: read the client, watch the requests, or replace the endpoint with your own.

## Layout

| Path | What |
|---|---|
| `AppMain.swift` + `HUDViews/Leaderboard/Telemetry/Updater/RingRenderer.swift` | the menu-bar app |
| `dashboard.html` `settings.html` `sources.html` `about.html` | the pages the app serves |
| `rust-server/` | the local service: parsers, discovery, SQLite, HTTP API |
| `Assets/` | the app icon (source of truth: `icon.svg`, `scripts/make_icon.py`) |

## License

MIT.
