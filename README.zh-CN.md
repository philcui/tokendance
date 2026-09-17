# TokenDance

*[English](README.md) · 中文*

**macOS 菜单栏上的 AI 编码 agent 用量仪表** —— Codex、Claude Code，以及你装过的其它任何工具。
它盯着硬盘上已有的转录文件，所以那个数字是活的：今日总量、当前燃烧速率、缓存命中率，以及
此刻是哪个 agent 在烧。

解析和存储全部在本机完成。没有账号、不用登录、不用配 key。

[![许可证: MIT](https://img.shields.io/badge/%E8%AE%B8%E5%8F%AF%E8%AF%81-MIT-blue.svg)](LICENSE)
![平台: macOS 13+](https://img.shields.io/badge/%E5%B9%B3%E5%8F%B0-macOS_13%2B-lightgrey.svg)
![只走本地](https://img.shields.io/badge/%E7%BD%91%E7%BB%9C-%E5%8F%AA%E8%B5%B0%E6%9C%AC%E5%9C%B0-success.svg)
![技术栈: Rust + Swift](https://img.shields.io/badge/%E6%8A%80%E6%9C%AF%E6%A0%88-Rust_%2B_Swift-orange.svg)

![TokenDance 的菜单栏挂件：今日总量在翻滚，燃烧条在动](docs/hud.gif)

## 它能做什么

- **菜单栏上的悬挂窗**：今日总量（数字逐位滚）、实时燃烧速率、缓存命中率，以及按 agent 的排名。
  可以拖到任意位置，可以收成只剩一个数字，也可以贴到屏幕边缘收成一枚小环。
- **网页仪表盘**（`127.0.0.1:8737`）：按天 / 数据源 / 项目 / 模型的趋势、GitHub 那样的活动日历、
  时长与闲置分析、可分页的调用记录，以及每个数据源单独一页。
- **数据源是"发现"来的，不是配置出来的。** 先看名单里已知的路径，再按名字搜，最后按内容扫标准
  位置。只要读得出用量就算数——**包括这个 app 发布之后才出现的工具**。

## 为什么还要再做一个

这类工具已经有好几个不错的。这里只讲三个"非做不可"的理由：

1. **靠发现，不是靠清单。** 别的工具支持一组固定的 agent，靠发版来扩。这个是读一份随包内置、
   又能从网上更新的名单，然后按名字搜，再按内容扫。一个谁都没听说过的工具，你第一次用它就会
   自己冒出来——内置的 Goose 就是这么被找到的。
2. **是挂件，不是终端命令。** 它回答的是"我现在这一刻在烧多少"，在菜单栏上，不占终端、不需要
   你去跑什么。
3. **结构上就只走本地。** 本地服务只绑 `127.0.0.1`，数据写进 `~/.tokendance/` 的 SQLite。
   没有一个你必须信任的云端；对外请求的完整清单见下面「网络行为」——四个接口，没有别的。

## 安装

**最省事**：把这一行粘进「终端」回车（不经过浏览器，所以完全不会弹拦截框）：

```bash
curl -fsSL https://fanshitou.cn/tokendance/install | sh
```

它会向服务端问当前版本、下载、**校验 sha256**、解压、装进 `/Applications`、去掉隔离标记、然后
启动。想先看它做什么：`curl -fsSL https://fanshitou.cn/tokendance/install`。

**或者**从发布页下载 `.dmg`，把 `TokenDance.app` 拖进 Applications——但这条路会撞上下面那节
说的拦截。（发布页还有个 `.zip`，那个是 app 内自动更新用的。）

### 如果 macOS 说"无法打开"

这个包是 **ad-hoc 签名，没有公证**：它带着签名，但不是 Apple 能追回到某个开发者的那种。所以
第一次启动会被系统拦下——而且**任何经浏览器下载的包都会被拦**，因为浏览器会给文件打上
`com.apple.quarantine` 标记。真正触发检查的是那个标记，不是签名：同一份二进制，去掉标记就直接
能跑。

两种解法，都在真实下载上验证过：

```bash
# ① 已经拖进 Applications 了 —— 去掉那个"来自网络"的标记
xattr -dr com.apple.quarantine /Applications/TokenDance.app

# ② 或者改用命令行下载，它从来不会打这个标记
curl -fLO https://fanshitou.cn/tokendance/download/TokenDance-<版本>.dmg
```

macOS 14 及以前还可以右键 → 打开；macOS 15 起 Apple 去掉了这条捷径，等价操作是：被拦一次之后，
去 系统设置 → 隐私与安全性 → 点「仍要打开」。

顺带说清楚：**Homebrew 不解决这个问题**。cask 安装会给它解出来的东西打上同样的标记
（`com.apple.quarantine: …;Homebrew Cask;…`），所以 `brew install --cask` 一样会停在那张对话框上。
能去掉它的只有 Developer ID 签名 + 公证，而这正是这个项目没有的东西。

## 截图

仪表盘——实时燃烧监控、筛选条，以及一整年的活动日历：

![TokenDance 仪表盘：实时燃烧监控、筛选与活动日历](docs/dashboard.png)

图表、按模型与按项目的拆分，以及每个数字来自硬盘上哪个文件：

![图表、按模型与按项目的拆分](docs/dashboard-charts.png)

每个数据源都有自己的页面：检测到了什么、它在哪、产出了多少：

![数据源详情页](docs/sources.png)

## 支持的数据源

所有解析都在你自己的机器上完成，每个解析器都带着针对合成样本的单元测试。下面这些是拿真实本地
数据验证过的：

- **Codex**（CLI 与桌面版）· `~/.codex/sessions/**/rollout-*.jsonl`
- **Claude Code** · `~/.claude/projects/**/*.jsonl`
- **OpenCode** · `~/.local/share/opencode/opencode.db`（SQLite）
- **Antigravity** · `~/.gemini/antigravity/conversations/*.db`
- **WorkBuddy** · `~/.workbuddy/projects/**/*.jsonl`
- **Qwen Code** · `~/.qwen/tmp/*/chats/session-*.jsonl`

按各家自己的格式接入、有数据就自动生效：**pi**、**Kimi Code**、**iFlow**、**Qoder**，以及
**Goose**（它靠名单 + 内容扫描被发现，不是靠名字）。

不支持，因为数据不可用：Cursor（token 计数稀疏）、Trae（SQLCipher 加密）、Windsurf、
CodeBuddy CLI、通义灵码 / 文心快码（会话在服务端）。

## 从源码构建

需要：macOS 13+、Xcode 命令行工具（`swiftc`）、Rust（`cargo`）。

```bash
./scripts/build_app.sh          # → build/TokenDance.app（编译 Swift + Rust 服务端，并把两者打进去）
./scripts/package.sh            # → build/TokenDance-<版本>-mac.zip（发版用的那个包）
cd rust-server && cargo test    # 44 个解析/存储/API 测试
```

app 把本地服务装在它自己的 bundle 里，所以打出来的 `.app` 是自足的：目标机器上不需要 cargo、
不需要 node、不需要装任何运行时。

`package.sh` 不只是"打个 zip"：压缩包的名字和内部布局**是更新协议的一部分**。app 内更新器会
下载 `TokenDance-<版本>-mac.zip`、解包、并要求 `TokenDance.app` 就在压缩包**根目录**；所以脚本
用 `ditto` 打包，再解一遍来证明布局没错，同时校验签名和主程序校验和，最后打印发布页需要的
`sha256`。加 `--dmg` 会额外产出一个给人拖拽安装的磁盘映像；更新器只用 zip。

## 网络行为

这部分是大家最想核对的，所以直接摊开讲。默认设置下，客户端只跟 `https://fanshitou.cn/tokendance`
说话，没有别处：

没有 CDN、没有字体外链、没有统计脚本：仪表盘唯一需要的第三方库（chart.js 4.4.3，MIT）就放在
仓库的 `vendor/` 里，由本地服务以 `/vendor/chart.umd.min.js` 提供。`scripts/privacy_check.sh` 和
`tools/dash/verify.mjs` 都会在"页面开始引用外部脚本"时直接判失败。

| 什么时候 | 请求 | 带什么 |
|---|---|---|
| 每 6 小时一次（默认开着，菜单里一项可关） | `POST /api/ping` | 一个随机安装 id（`u-` + 8 位十六进制）、app 版本、系统名称与版本、CPU 架构、界面语言 / 主题、运行时长（秒） |
| 启动时、以及每 6 小时 | `GET /api/version?ver=<当前版本>` | 只有版本号，服务端据此回答"有没有新版本" |
| 只有你加入了排行榜 | `POST /api/lb/submit`、`GET /api/lb/me?uid=…&days=…` | 你的显示名与**每日总量**——绝不含逐条调用数据 |
| 很少，刷新数据源名单时 | `GET /registry.json` | 什么也不带 |

**从不发送**：token 用量（加入排行榜后也只有每日总量）、文件名、项目路径、会话内容、提问与回复。
服务端不记录 ping 的 IP ——那份日志只有时间、方法、路径和状态码。

你可以把客户端指向你自己实现的那四个接口：

```bash
# 运行时改（用户级，立即生效）
defaults write com.tokendance.app tb_tel_url "https://example.com/my-endpoint"

# 或者构建时改（变成 app 的默认值，源码一行都不用动）
SERVICE_BASE="https://example.com/my-endpoint" ./scripts/build_app.sh
```

`SERVICE_BASE` 是客户端里**唯一**的远端地址出处——匿名上报、更新检查、排行榜都挂在它下面；
名单地址（`<base>/registry.json`）由 app 拉起本地服务时注入。

本地服务可以用 `TOKENDANCE_REGISTRY_URL` 换成别的名单。

## 隐私

解析、存储、聚合全部在这台机器上完成。本地服务只绑 `127.0.0.1:8737`，数据写进
`~/.tokendance/tokendance.db`。它只读 agent 的转录文件，别的什么都不碰：系统里的隐私库
（通讯录、信息、邮件、Safari、健康、日历、提醒、iCloud 云盘）一个都不会进——递归扫描、按名字
搜索、手动填路径这三条路，全都在"碰硬盘之前"就拒绝掉。

## 这个仓库里没有什么

官方的埋点 / 更新 / 排行榜服务，以及它的部署配置。那一半是闭源的。客户端除了上面那四个 HTTP
调用之外，不依赖它任何东西——也就是说：你可以读客户端、可以看它发了什么请求，也可以把地址换成
自己的。

## 目录结构

| 路径 | 是什么 |
|---|---|
| `AppMain.swift` + `HUDViews/Leaderboard/Telemetry/Updater/RingRenderer.swift` | 菜单栏 app |
| `dashboard.html` `settings.html` `sources.html` `about.html` | app 提供的四个页面 |
| `rust-server/` | 本地服务：解析器、数据源发现、SQLite、HTTP 接口 |
| `Assets/` | 应用图标（源头是 `icon.svg`，`scripts/make_icon.py` 生成各尺寸） |

## 许可

MIT。
