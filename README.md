# Gofer

本地动作服务：只要知道地址和 token，任何应用都可以让 Gofer 执行：

- **微信（本机）**：发到当前打开的微信会话
- **小红书（本机）**：发到指定群聊
- **自动锁票**：通过 `maoyan-booker` 真锁猫眼座位

Gofer 是一个独立的 Rust 服务（axum + SQLite + Askama），自带 HTTP API、CLI 和 Web 面板。
它不依赖 ticket-tracker 或任何监控服务；监控服务只把它当作一个普通的投递目标。

![Gofer dashboard](docs/dashboard.png)

---

## 目录

- [架构](#架构)
- [新 Mac 安装](#新-mac-安装)
- [配置 token 与启动](#配置-token-与启动)
- [常用命令](#常用命令)
- [HTTP API](#http-api)
- [能力说明与安全边界](#能力说明与安全边界)
- [Web 面板](#web-面板)
- [macOS 后台运行 / Mac mini 部署](#macos-后台运行--mac-mini-部署)
- [环境变量](#环境变量)
- [数据与备份](#数据与备份)
- [升级](#升级)
- [故障排查](#故障排查)
- [项目结构](#项目结构)

---

## 架构

```text
外部应用 / 监控服务
        │  POST /v1/notify  /  POST /v1/lock  (token)
        ▼
┌──────────────────────────────┐
│ Gofer                        │
│  ├─ HTTP API + Web 面板      │
│  ├─ SQLite 请求历史/统计     │
│  ├─ notify worker（串行）    │
│  │    ├─ 微信脚本            │
│  │    └─ 小红书脚本          │
│  └─ lock worker（独立串行）  │
│       └─ maoyan-booker       │
└──────────────────────────────┘
```

设计约束：

- 请求异步：API 先返回 `202 + request_id`，结果写入 SQLite 并在面板展示。
- 微信和小红书共享 GUI 焦点，因此通知是单 worker 串行执行。
- 锁票使用独立 worker，不会阻塞通知。
- 不重试未完成请求；调用方可用 `request_id` 轮询，或通过 CLI `--wait` 等待。
- 锁票始终真锁；`--dry-run` 只影响微信/小红书通知。

---

## 新 Mac 安装

以下以全新 macOS（Apple Silicon 或 Intel）为例。命令默认使用 zsh。

### 0. 基础工具

安装 Xcode Command Line Tools：

```bash
xcode-select --install
```

安装 Homebrew（已安装可跳过）：

```bash
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

安装常用依赖：

```bash
brew install git node python uv
```

> `python3` 必须可执行，Gofer 的脚本通过 `python3 script.py ...` 调用。
> `uv` 用于运行 maoyan-booker。

### 1. 安装 Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version
cargo --version
```

要求 Rust 1.75+。Gofer 当前使用 edition 2021。

### 2. 安装 agent-browser（小红书需要）

```bash
npm install -g agent-browser
agent-browser --version
```

如果 `agent-browser` 不在 PATH：

```bash
npm prefix -g
# 把 <prefix>/bin 加进 ~/.zshrc 的 PATH
```

### 3. 安装并登录 maoyan-booker（锁票需要）

```bash
mkdir -p ~/Applications
cd ~/Applications
git clone https://github.com/kuicao55/maoyan-booker.git
cd maoyan-booker

uv sync
uv run playwright install chromium
```

首次登录猫眼（会打开 headed 浏览器，扫码登录）：

```bash
uv run python booker.py --cinema-id 37534
```

登录态保存在 `~/.maoyan-booker-profile/`，**不要删除**。
Gofer 默认使用：

```text
~/Applications/maoyan-booker/booker.py
~/.maoyan-booker-profile/
```

如果 booker 放在别处：

```bash
export MAOYAN_BOOKER_DIR=/path/to/maoyan-booker
```

建议先用 booker 自己的 README 做一次 dry-run 选座验证。

### 4. 准备微信

- 安装并登录微信 macOS 版。
- 微信专用账号建议只有一个目标群，并置顶。
- 发送前**把微信停在目标群聊页面**；`wechat-notify-fast.py` 不会按群名切换会话，只对当前打开的会话操作。
- 首次使用需在「系统设置 → 隐私与安全性 → 辅助功能 / 自动化」中允许运行 Gofer 的终端或进程控制微信。
- 微信进程名通常是 `WeChat`；Gofer 的 `/health` 会用 `pgrep -x WeChat` 检查。

### 5. 登录小红书 profile

```bash
agent-browser close 2>/dev/null || true
agent-browser --headed --profile ~/.agent-browser/profiles/xiaohongshu \
  open https://www.xiaohongshu.com/explore
```

在弹出的浏览器里扫码登录，然后关闭浏览器。登录态保存在：

```text
~/.agent-browser/profiles/xiaohongshu/
```

session 过期后重复上述步骤。
`xhs-notify.py` 默认使用这个 profile；群名必须和聊天列表显示的标题一致。

### 6. 克隆并编译 Gofer

```bash
mkdir -p ~/Applications
cd ~/Applications
git clone https://github.com/kuicao55/Gofer.git
cd Gofer

cargo build --release --locked
./target/release/gofer --version
```

可选：把 CLI 安装到 PATH：

```bash
cargo install --path . --locked
gofer --version
```

> 首次编译需要几分钟（tokio / axum / sqlx / reqwest 等）。
> `scripts/` 已带 vendored 的 `wechat-notify-fast.py` 和 `xhs-notify.py`；
> 如果从上游更新脚本，重新复制到 `scripts/`，或用 `GOFER_SCRIPTS_DIR` 指向别的目录。

### 7. 启动并检查

```bash
cd ~/Applications/Gofer
./target/release/gofer serve \
  --bind 127.0.0.1:8790 \
  --db ~/.local/state/gofer/gofer.db
```

打开 <http://127.0.0.1:8790>。

另一个终端检查：

```bash
curl -s http://127.0.0.1:8790/health | python3 -m json.tool
./target/release/gofer health
```

你应该看到：

```json
{
  "ready": {
    "wechat": true,
    "xhs": true,
    "lock": true
  }
}
```

如果微信没打开，`wechat` 会是 `false`，但服务本身仍然正常运行。

---

## 配置 token 与启动

### 生成 token

本机联调可以不设置 token；如果要给其他机器/应用调用，**必须设置 token**：

```bash
openssl rand -hex 32
```

把输出保存到安全位置，例如 macOS Keychain 或密码管理器。

### 前台启动（带 token）

```bash
cd ~/Applications/Gofer
GOFER_TOKEN='你的随机token' \
  ./target/release/gofer serve \
  --bind 127.0.0.1:8790 \
  --db ~/.local/state/gofer/gofer.db
```

### 后台启动（推荐日常使用）

```bash
cd ~/Applications/Gofer
GOFER_TOKEN='你的随机token' ./start-gofer.sh start
./start-gofer.sh status
./start-gofer.sh logs
./start-gofer.sh stop
```

`start-gofer.sh` 使用：

- 二进制：`target/release/gofer`（可用 `GOFER_BIN` 覆盖）
- 监听：`GOFER_BIND`，默认 `127.0.0.1:8790`
- 数据库：`GOFER_DB`，默认 `~/.local/state/gofer/gofer.db`
- PID：`~/.local/state/gofer/gofer.pid`
- 日志：`~/.local/state/gofer/gofer.log`

重启：

```bash
GOFER_TOKEN='你的随机token' ./start-gofer.sh restart
```

> 如果启用了 token，所有 `/v1/notify`、`/v1/lock`、`/v1/requests*`、`/v1/stats`
> 请求都必须带 token；`/health` 和面板仍然公开。

### 真实验证 vs dry-run

```bash
# 只模拟微信/小红书通知，不操作 GUI；锁票仍然真锁
./target/release/gofer serve --dry-run
```

建议第一次安装时用 `--dry-run` 验证 Gofer 本身，再单独验证真实脚本。

---

## 常用命令

```bash
# 微信：发到当前打开的微信会话
gofer notify --channel wechat --title "放票了" -m "19:30 IMAX 有座"

# 小红书：发到指定群
gofer notify --channel xhs --group "群名" \
  --title "放票了" -m "19:30 IMAX 有座"

# 等待执行结果
gofer notify --channel wechat -m "hello" --wait

# 锁票
gofer lock \
  --cinema-id 37534 \
  --movie-id 1545360 \
  --show-date 2026-11-12 \
  --time-range "19:00-22:00" \
  --num-seats 2 \
  --imax-only

# 锁票等待结果（最长 5 分钟）
gofer lock ... --wait

# 查询
gofer requests --limit 20
gofer requests --json --limit 50
gofer status
gofer health
gofer health --json
```

远端调用：

```bash
export GOFER_URL=http://127.0.0.1:8790
export GOFER_TOKEN='你的随机token'

gofer notify --channel wechat -m "hi"
```

> `gofer lock` 是**真锁**。确认 `GOFER_URL` 指向的是测试还是生产实例。
> 联调 booker 请用 `MAOYAN_BOOKER_DIR` 指向 mock 项目，而不是依赖 `--dry-run`。

---

## HTTP API

所有 API 返回 JSON。异步请求返回 `202`：

```json
{"request_id":"g_ab12cd34","status":"queued","capability":"wechat"}
```

### 鉴权

设置了 token 时，任一方式即可：

```http
X-Gofer-Token: <token>
X-Agent-Token: <token>
Authorization: Bearer <token>
```

### 微信通知

```bash
curl -X POST http://127.0.0.1:8790/v1/notify \
  -H 'Content-Type: application/json' \
  -H 'X-Gofer-Token: change-me' \
  -d '{
    "channel": "wechat",
    "title": "放票了",
    "message": "19:30 IMAX 有座",
    "source": "my-app"
  }'
```

### 小红书通知

```bash
curl -X POST http://127.0.0.1:8790/v1/notify \
  -H 'Content-Type: application/json' \
  -H 'X-Gofer-Token: change-me' \
  -d '{
    "channel": "xhs",
    "group": "群名",
    "title": "放票了",
    "message": "19:30 IMAX 有座",
    "source": "my-app"
  }'
```

`group` 也可以放在 `config.group` 里。

### 锁票

```bash
curl -X POST http://127.0.0.1:8790/v1/lock \
  -H 'Content-Type: application/json' \
  -H 'X-Gofer-Token: change-me' \
  -d '{
    "movie_id": 1545360,
    "cinema_id": "37534",
    "show_date": "2026-11-12",
    "time_range": "19:00-22:00",
    "num_seats": 2,
    "imax_only": true,
    "seats": [],
    "headless": true,
    "source": "my-app"
  }'
```

可选字段：

- `watch_id` / `movie_name` / `cinema_name` / `event_id`：只用于展示和回调上下文。
- `seats`：手动指定座位，例如 `["5排6座"]`；为空时智能选座。
- `headless`：默认 true；调试可传 false。
- `callback_url`：锁票完成后 best-effort POST 结果。

### 查询请求

```bash
curl -H 'X-Gofer-Token: change-me' \
  http://127.0.0.1:8790/v1/requests/<request_id>
```

状态：

- `queued`：已接收，等待 worker
- `running`：执行中
- `ok`：成功
- `failed`：失败，`error` 有原因

### 其他端点

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | 服务健康、能力静态检查、队列长度 |
| GET | `/v1/stats` | 24h 总数/成功/失败、按能力统计 |
| GET | `/v1/requests?limit=50` | 最近请求 |
| GET | `/v1/requests/:id` | 单个请求 |
| POST | `/v1/notify` | 微信/小红书 |
| POST | `/v1/lock` | 锁票 |

兼容旧路径 `/notify`、`/lock`，方便替换旧 agent。

---

## 能力说明与安全边界

### 微信（本机）

- **没有群名参数**：固定发到当前打开的微信会话。
- 发送前请把微信停在目标群。
- 依赖 macOS 辅助功能/自动化权限。
- `/health` 只能检查 `WeChat` 进程是否存在，不能确认当前会话、登录状态或脚本权限。
- `--dry-run` 时只打印，不操作微信。

### 小红书（本机）

- 每个请求必须提供 `group`（群标题），必须和聊天列表完全一致。
- 依赖 `agent-browser` 和 `~/.agent-browser/profiles/xiaohongshu` 登录态。
- `/health` 只检查脚本、`agent-browser` 和 profile 目录存在；不能确认 cookie 未过期。
- session 过期时按 [登录小红书 profile](#5-登录小红书-profile) 重新扫码。

### 自动锁票

- 依赖 `uv`、`maoyan-booker`、`~/.maoyan-booker-profile`。
- 始终 `--confirm` 真锁；没有 Gofer 级别的 dry-run。
- 锁成功后需要在 15 分钟内到猫眼 App 付款。
- `/health` 只做依赖检查，不会真的锁票。
- 结果码：`Ok`、`ShowGone`、`Error`、`Timeout`、`Unknown`。
- `callback_url` 是 best-effort，不保证送达；以面板/SQLite 状态为准。

### Gofer 不会做什么

- 不会自动重试失败请求。
- 不会持久化未执行队列；进程重启时 queued/running 请求不会自动恢复。
- 不会修改微信/小红书/猫眼页面以外的系统状态。
- 面板没有登录鉴权，只应监听本机或放在受信任的内网/SSH 隧道后面。

---

## Web 面板

访问 <http://127.0.0.1:8790>。

- **Overview**：三项能力、预检状态、24h 请求量/成功率、队列、最近请求。
- **Requests**：完整请求历史（最多 200 条），含 request_id、耗时、来源、结果/错误。
- 顶部开关可暂停/恢复 Gofer；暂停后 `/v1/notify` 和 `/v1/lock` 返回 `503`，
  已在执行的任务会完成，状态会持久化到 SQLite。
- 每 5 秒自动刷新。
- 面板是只读的；操作通过 CLI / HTTP 发起。

如果页面样式异常，检查 URL 中的 `app.css?v=...` 是否随版本更新，或强制刷新。

---

## 面板更新

访问 `http://127.0.0.1:8790/update`：

1. 检查 GitHub Releases 最新版本；
2. 显示 release notes；
3. 下载当前 Mac 架构的 `gofer-<arch>-apple-darwin`；
4. 校验 `SHA256SUMS`；
5. 替换二进制并重启。

Gofer 仓库现在是公开仓库，通常不需要 token。如果遇到 GitHub 限流，可以配置只读 token：

```bash
export GOFER_UPDATE_TOKEN='github_pat_...'   # contents: read，可选
```

launchd 模板里设置 `GOFER_SUPERVISED=launchd`，这样更新后由 KeepAlive 拉起新进程。
用 `start-gofer.sh` 启动时不需要 supervised，Gofer 会用重启助手接管 PID/日志。

发版：

```bash
git tag v0.3.0
git push origin v0.3.0
```

Release 资产：

```text
gofer-aarch64-apple-darwin
gofer-x86_64-apple-darwin
SHA256SUMS
```

二进制会按顺序查找通知脚本：`GOFER_SCRIPTS_DIR`、可执行文件附近的 `scripts/`、
`~/Applications/Gofer/scripts`。只复制二进制到别处时，请设置：

```bash
export GOFER_SCRIPTS_DIR=/path/to/Gofer/scripts
```

`start-gofer.sh` 会自动设置 `GOFER_SCRIPTS_DIR=$ROOT/scripts`。

## macOS 后台运行 / Mac mini 部署

### 方案 A：`start-gofer.sh`（最简单）

```bash
cd ~/Applications/Gofer
GOFER_TOKEN='...' ./start-gofer.sh start --build
```

适合本机日常使用。登录后手动启动，或写进登录项。

### 方案 B：launchd（推荐 Mac mini 常驻）

模板：`deploy/launchd/com.kuicao.gofer.plist`

先改成你的实际路径、用户名和 token：

```bash
cp deploy/launchd/com.kuicao.gofer.plist ~/Library/LaunchAgents/
# 编辑 ~/Library/LaunchAgents/com.kuicao.gofer.plist
```

安装 / 启动：

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.kuicao.gofer.plist
launchctl kickstart -k gui/$(id -u)/com.kuicao.gofer
```

查看状态 / 日志：

```bash
launchctl print gui/$(id -u)/com.kuicao.gofer | head -40
tail -f ~/Library/Logs/gofer.log
```

卸载：

```bash
launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/com.kuicao.gofer.plist
```

> 旧教程里的 `launchctl load/unload` 仍可用，但新版 macOS 建议用 `bootstrap/bootout`。

### Mac mini 专项 checklist

Mac mini 更适合长期运行，但 GUI 自动化有几个硬性条件：

1. **用户登录会话**
   - WeChat / agent-browser 需要已登录的 macOS 用户会话。
   - 重启后如果没有自动登录，launchd user agent 不会在登录前运行。
   - 建议使用专用用户 + 自动登录，或重启后手动登录一次。
   - 如果开启 FileVault，自动登录会被禁用；需要自己权衡安全与无人值守。

2. **禁止睡眠**
   ```bash
   sudo pmset -a sleep 0 disksleep 0 displaysleep 10
   pmset -g
   ```
   - 显示器可以睡眠；系统不要睡眠。
   - 也可以外层用 `caffeinate -i -s`，但更推荐 `pmset`。
   - 不要依赖锁屏后的 headed GUI 流程；小红书登录恢复需要可见会话。

3. **GUI 权限**
   - 系统设置 → 隐私与安全性 → 辅助功能：允许运行 Gofer 的终端 / launchd 进程 / `osascript`。
   - 自动化：允许 Terminal / Gofer 控制微信。
   - 修改权限后通常需要重新登录或重启进程。

4. **微信保持登录**
   - 微信专用账号；目标群置顶。
   - 不要把微信退出登录，否则 `/health` 仍显示进程存在，但发送会失败。

5. **小红书 session**
   - profile 在 `~/.agent-browser/profiles/xiaohongshu`。
   - session 过期后需要重新 headed 扫码；无人值守机器上应定期检查。

6. **锁票 session**
   - `~/.maoyan-booker-profile` 不要删除。
   - 定期验证 booker 登录态。

7. **网络与安全**
   - 默认只绑定 `127.0.0.1`，只允许本机调用。
   - 如果监控服务在另一台机器：
     - 推荐 SSH 隧道 / Tailscale，仍然让 Gofer 绑 `127.0.0.1`；
     - 或绑定内网地址并**务必启用 token + 防火墙**，只允许监控服务器 IP。
   - 不要把 8790 直接暴露到公网；面板没有登录鉴权。

### 远程监控服务连接示例

在监控机器上建 SSH 隧道：

```bash
ssh -N -L 8790:127.0.0.1:8790 user@mac-mini
```

监控服务的 Settings / watch 渠道里填：

```text
Gofer URL:   http://127.0.0.1:8790
Gofer token: 与 Gofer 的 GOFER_TOKEN 一致
```

监控服务不会探活 Gofer；Gofer 离线只会在监控事件里记录一次投递失败。

---

## 环境变量

| 变量 | 作用 | 默认 |
|---|---|---|
| `GOFER_BIND` | 监听地址 | `127.0.0.1:8790` |
| `GOFER_DB` | SQLite 路径 | `~/.local/state/gofer/gofer.db` |
| `GOFER_TOKEN` | API token；空=不校验 | 空 |
| `GOFER_DRY_RUN` | 仅通知 dry-run | `false` |
| `GOFER_LOCK_HEADLESS` | booker 默认 headless | `true` |
| `GOFER_RETENTION_DAYS` | 请求历史保留天数 | `30` |
| `GOFER_SCRIPTS_DIR` | 通知脚本目录 | `<repo>/scripts` |
| `MAOYAN_BOOKER_DIR` | booker 项目目录 | `~/Applications/maoyan-booker` |
| `GOFER_UPDATE_TOKEN` | 可选；GitHub 只读 token，避免限流 | 空 |
| `GOFER_UPDATE_API_BASE` | 可选；GitHub API/镜像地址 | `https://api.github.com` |
| `GOFER_SUPERVISED` | `launchd` 时更新后由 launchd 重启 | 空 |
| `XHS_WEBHOOK` | 小红书 session 过期时发二维码的 Discord webhook（可选） | 空 |
| `GOFER_URL` | CLI 默认服务地址 | `http://127.0.0.1:8790` |

---

## 数据与备份

默认数据库：

```text
~/.local/state/gofer/gofer.db
```

表：

- `requests`：每次请求的时间、能力、状态、耗时、来源、摘要、错误、payload。

备份（SQLite 在线备份）：

```bash
mkdir -p ~/Backups/gofer
sqlite3 ~/.local/state/gofer/gofer.db \
  ".backup '$HOME/Backups/gofer/gofer-$(date +%Y%m%d-%H%M%S).db'"
```

恢复：

```bash
./start-gofer.sh stop
cp ~/Backups/gofer/gofer-YYYYMMDD-HHMMSS.db ~/.local/state/gofer/gofer.db
./start-gofer.sh start
```

历史默认保留 30 天；可用 `--retention-days` / `GOFER_RETENTION_DAYS` 调整。

---

## 升级

```bash
cd ~/Applications/Gofer
git pull --ff-only
cargo build --release --locked
./start-gofer.sh restart
```

- SQLite migration 在启动时自动执行。
- 升级前建议先备份数据库。
- 如果使用 launchd，`restart` 仍通过 `launchctl kickstart -k`：

```bash
launchctl kickstart -k gui/$(id -u)/com.kuicao.gofer
```

升级通知脚本时：

```bash
# 从上游复制
cp /path/to/wechat-notify-fast.py scripts/
cp /path/to/xhs-notify.py scripts/
chmod +x scripts/*.py
./start-gofer.sh restart
```

---

## 故障排查

### `gofer health` 显示 `wechat: false`

- 微信 App 没打开 → 打开并登录。
- 进程名不是 `WeChat` → `pgrep -fl -i wechat`；如果不同，需要调整 `capabilities.rs` 中的检查。
- 打开微信后立即重试；Gofer 每次 `/health` 都会重新检查。

### 微信显示就绪但发送失败

- 微信没有停在目标群聊页面。
- 没有辅助功能/自动化权限。
- 微信专用账号被切换或群名/置顶变化。
- 发送失败会在请求 `error` 和 Gofer 日志中显示脚本 stderr。

### 小红书 403 / 找不到群 / 登录过期

- 群名必须完全一致（含 emoji/空格）。
- 重新登录 profile：
  ```bash
  agent-browser close
  agent-browser --headed --profile ~/.agent-browser/profiles/xiaohongshu \
    open https://www.xiaohongshu.com/explore
  ```
- 检查 `agent-browser --version` 和 PATH。
- session 过期后脚本可能自动 headed 恢复；无人值守时要注意。

### 锁票立即失败

```bash
gofer health --json
```

检查：

- `uv`、`maoyan-booker`、`booker_profile` 三项是否 ok。
- `MAOYAN_BOOKER_DIR` 是否指向含 `booker.py` 的目录。
- `~/.maoyan-booker-profile` 是否被删除。
- booker 是否还登录着；必要时重新运行一次 headed 登录。
- 查看 Gofer 请求的 `result_json` / `error`，里面包含 booker stdout/stderr 尾部。

### 端口被占用

```bash
lsof -nP -iTCP:8790 -sTCP:LISTEN
```

如果旧 `tt-agent` 还在：

```bash
launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/com.kuicao.ticket-agent.plist 2>/dev/null || true
pkill -f 'tt-agent'
```

### 401

- Gofer 设置了 `GOFER_TOKEN`，但请求没带或带错。
- CLI 设置 `GOFER_TOKEN`；HTTP 带 `X-Gofer-Token` 或 `Authorization: Bearer`。
- 监控服务的 Gofer token 要和 Gofer 的 `GOFER_TOKEN` 完全一致。

### 429

- 通知队列 64 / 锁票队列 16 已满。
- 等 worker 消化后重试；不要无界并发发通知。

### 更新后提示“通知脚本未找到”

Release 二进制不再使用编译机路径。确认：

```bash
echo "$GOFER_SCRIPTS_DIR"
ls "$GOFER_SCRIPTS_DIR/wechat-notify-fast.py" "$GOFER_SCRIPTS_DIR/xhs-notify.py"
```

如果为空或路径不存在：

```bash
export GOFER_SCRIPTS_DIR=/Users/你的用户名/Applications/Gofer/scripts
./start-gofer.sh restart
```

或直接从 Gofer 仓库目录用 `start-gofer.sh` 启动。

### 请求一直 `queued`

- worker 没有启动（服务异常/旧进程）。
- 查看日志：`./start-gofer.sh logs` 或 `~/Library/Logs/gofer.log`。

### 面板样式不更新

- 强制刷新 `Cmd+Shift+R`。
- 确认 `app.css?v=` 的版本已变化。
- 确认服务重启后加载的是新二进制。

---

## 项目结构

```text
Gofer/
├── src/
│   ├── main.rs          # CLI + serve
│   ├── server.rs        # HTTP、队列、worker、callback
│   ├── web.rs           # 面板视图
│   ├── db.rs            # SQLite 请求历史/统计
│   ├── capabilities.rs  # 静态能力检查
│   ├── notify.rs        # 微信/小红书脚本调用
│   └── lock.rs          # maoyan-booker 调用与结果分类
├── templates/           # Askama 页面
├── web/                 # CSS / htmx / favicon
├── migrations/          # SQLite migration
├── scripts/             # vendored 通知脚本
├── tests/               # API smoke tests
├── deploy/launchd/      # Mac mini launchd 模板
├── start-gofer.sh
└── docs/dashboard.png
```

开发检查：

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

---

## 安全提醒

- **锁票是真锁**：任何拿到 token 的应用都可以触发真实锁票。token 泄露等于把锁票能力交出去。
- 建议只绑定 `127.0.0.1`；远程使用 SSH 隧道 / Tailscale。
- 面板无登录，禁止直接暴露公网。
- 微信/小红书脚本能操作 GUI，只给可信应用 token。
- `~/.agent-browser/profiles/xiaohongshu` 和 `~/.maoyan-booker-profile` 含登录态，按敏感数据保护。
- 不要把 token 写进公开仓库；launchd plist 不要提交真实 token。
