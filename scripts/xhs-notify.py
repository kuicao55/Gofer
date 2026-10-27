#!/usr/bin/env python3
"""xhs-notify — 通过 agent-browser 把通知推送到小红书指定群聊。

用法:
    xhs-notify --group test --title "预售开启 🎬" --body "..."
    xhs-notify --group test --title "预售开启 🎬" --body "..." --url "https://..."
    xhs-notify --title "hi" --dry-run            # 只打印不发
    xhs-notify --group test --title "..." --keep-chat   # 发完不跳 about:blank（调试用）

默认行为：
- 自动 cold start headless daemon（带 cookies profile）
- 默认走最短路径——daemon 健康且已在 /chat 就 1 次 get url 直接发
- 不在消息页时自动 `open /chat`（SPA 深链比 click sidebar 可靠）
- 未登录 / session 过期时默认 headed 重开 + 截图二维码 → Discord 推送
  （30 分钟冷却；用 `--no-recover` 关闭；发送成功会自动清掉冷却）
- 发完自动 `open about:blank` 防止内存泄漏（`--keep-chat` 关闭）

环境变量:
    XHS_PROFILE   agent-browser profile 路径（默认: ~/.agent-browser/profiles/xiaohongshu）
    XHS_WEBHOOK   Discord webhook URL（session 过期扫码恢复用；不填则只在本机 headed 浏览器处理）

退出码:
    0  成功
    1  参数错误
    2  运行期找不到元素（侧栏 / 群聊 / 输入框）——已登录但群名/列表问题
    3  agent-browser 命令超时
    4  未登录 / session 过期 / 真·风控（已触发自动恢复流程，或冷却中静默退出）

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
前置条件（一次性设置）
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

1. 安装 agent-browser（详见 https://github.com/anthropics/agent-browser）

2. 创建 profile 并登录小红书（只需一次，扫码后 cookie 长期保留）:

     agent-browser --headed --profile ~/.agent-browser/profiles/xiaohongshu \
       open https://www.xiaohongshu.com/explore

   弹出浏览器窗口，扫码登录，然后直接关闭浏览器即可。

3. 安装 xhs-notify 到 PATH（让 ticket_tracker 能调）:

     chmod +x xhs-notify.py
     ln -s "$(pwd)/xhs-notify.py" /usr/local/bin/xhs-notify
     # 或: cp xhs-notify.py /usr/local/bin/xhs-notify

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
运行时前置条件（每次发通知前）
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

- agent-browser daemon 在跑（首次 agent-browser 命令会自动起）
- 浏览器当前停在**小红书任意页面**即可，脚本会自己导航
- 强烈建议停在「首页 /explore」或「消息页 /chat」，不要停在外部网站

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
session 过期处理
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

- 登录态保存在 profile 里，token 一般能撑数周到数月
- 过期征兆：脚本跑到 click 群聊后找不到元素 / 页面跳到登录二维码
- 过期后重新登录（head 模式，5 秒搞定）:

     agent-browser close
     agent-browser --headed --profile ~/.agent-browser/profiles/xiaohongshu \
       open https://www.xiaohongshu.com/explore
     # 扫码，关浏览器
"""
import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.request

# Cloudflare 会拦截默认的 Python-urllib UA（error 1010），
# 用真实浏览器 UA 通过校验。
USER_AGENT = (
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) "
    "AppleWebKit/537.36 (KHTML, like Gecko) "
    "Chrome/120.0.0.0 Safari/537.36"
)

DEFAULT_PROFILE = os.path.expanduser("~/.agent-browser/profiles/xiaohongshu")
DEFAULT_WEBHOOK = os.environ.get("XHS_WEBHOOK", "")

CHAT_URL = "https://www.xiaohongshu.com/chat"
LOGIN_URL = "https://www.xiaohongshu.com/explore"
BLANK_URL = "about:blank"

# 恢复流程冷却：session 过期时，监控每出一次票都弹 headed 窗口 + 刷 Discord
# 会非常烦人。首次触发后 30 分钟内只静默退出。发送成功会立即清除该标记。
RECOVER_STAMP = os.path.expanduser("~/.agent-browser/.xhs-notify-recover-stamp")
RECOVER_COOLDOWN = 30 * 60


def ab(*args, timeout=30):
    """Run agent-browser command, return CompletedProcess."""
    return subprocess.run(
        ["agent-browser", *args],
        capture_output=True, text=True, timeout=timeout)


def snapshot():
    r = ab("snapshot", "-i")
    if r.returncode != 0:
        raise RuntimeError(f"snapshot 失败: {r.stderr.strip()}")
    return r.stdout


# 解析一行 snapshot，形如：
#   - listitem "消息" [level=1, ref=e13] clickable [cursor:pointer]
#   - generic  "test09:46" [ref=e9] clickable [cursor:pointer]
#   - generic  [ref=e4] editable [contenteditable]
# name 字段允许含转义引号 \"…\"（来自群名修改提示等场景）
_LINE_RE = re.compile(
    r'-\s+(\w+)(?:\s+"((?:[^"\\]|\\.)*)")?\s+\[(?:level=(\d+),?\s*)?ref=(e\d+)\](.*)$')


def find_ref_by_text(snap, text, role=None, exact_name_only=False):
    """在 snapshot 中找包含 `text` 的可点击元素 ref。

    exact_name_only=True 时：精确匹配 name 字段去掉时间戳后的群名，
    避免 "test" 误中 "test-tracker"。
    exact_name_only=False 时：按整行做子串匹配（大小写不敏感），
    能容忍 name 里的转义引号，适合 sidebar 这种结构宽松的场景。
    """
    text_lower = text.lower()
    # 时间戳边界：数字（HH:MM / X月X日 等）/ 刚 / 昨 / 今 / 前天 / 周 / 星期
    # 这些都是时间相关词汇的开头，可作为群名结束边界
    _TIME_BOUNDARY = r'(?=\d|刚刚|昨天|今天|前天|周|星期|$)'
    for line in snap.splitlines():
        ref_match = re.search(r'ref=(e\d+)', line)  # 也匹配 [level=N, ref=eN] 形式
        if not ref_match:
            continue
        if "clickable" not in line and "editable" not in line:
            continue
        if role:
            if not re.match(rf'-\s+{re.escape(role)}\b', line):
                continue

        if exact_name_only:
            m = _LINE_RE.match(line)
            if not m:
                continue
            name = m.group(2) or ""
            # 群名 = name 在时间戳之前的前缀
            gm = re.match(rf'(.*?){_TIME_BOUNDARY}', name)
            group_name = (gm.group(1) if gm else name).strip()
            if group_name.lower() != text_lower:
                continue
        else:
            line_no_ref = re.sub(r'\[ref=e\d+\]', '', line).lower()
            if text_lower not in line_no_ref:
                continue

        return ref_match.group(1)
    return None


def find_editable_ref(snap):
    """找 contenteditable 输入框（聊天输入）。"""
    for line in snap.splitlines():
        if "editable" in line and "[ref=" in line:
            m = re.search(r'ref=(e\d+)', line)
            if m:
                return m.group(1)
    return None


def _same_path(a, b):
    """比较两个路径是否指向同一位置（容忍 symlink / 尾斜杠 / 不存在）。"""
    if not a or not b:
        return False
    try:
        return os.path.realpath(a).rstrip("/") == os.path.realpath(b).rstrip("/")
    except OSError:
        return a.rstrip("/") == b.rstrip("/")


def browser_status():
    """探测 Chrome 主进程状态，返回 (running, headless, profile_dir)。

    **必须零副作用**——这是本函数存在的全部理由。

    不能用 `agent-browser get url` 探活：那条命令在 daemon 不存在时会
    **自动拉起一个使用临时 profile 的 daemon**（user-data-dir 落在
    /var/folders/.../T/agent-browser-chrome-<uuid>），然后正常返回
    about:blank + returncode 0。调用方会因此误判「daemon 健康」而跳过
    带 --profile 的正确启动，最终因为没有 cookies 被小红书当成未登录。
    这个 bug 实际发生过。

    改为直接读 Chrome 主进程命令行，纯 pgrep，不触发任何启动。
    """
    try:
        r = subprocess.run(
            ["pgrep", "-lf", "Google Chrome for Testing"],
            capture_output=True, text=True, timeout=5)
    except subprocess.TimeoutExpired:
        return (False, False, None)

    # 子进程（renderer / gpu / utility）命令行都含 --type=，过滤掉只留主进程
    mains = [ln for ln in r.stdout.splitlines()
             if "--type=" not in ln and "Google Chrome for Testing" in ln]
    if not mains:
        return (False, False, None)

    # 可能同时存在多个实例（别的 profile / 临时 profile），优先认我们自己的
    line = next(
        (ln for ln in mains if f"--user-data-dir={DEFAULT_PROFILE}" in ln),
        mains[0])
    m = re.search(r"--user-data-dir=(\S+)", line)
    return (True, "--headless" in line, m.group(1) if m else None)


def reset_browser(headed=False, url=CHAT_URL):
    """关掉 daemon 并以指定模式重开。

    必须显式传 --profile：否则新 daemon 用 fresh profile 没 cookies，
    小红书会当成未登录拦截（错误页写 "IP at risk" 其实是未登录）。
    必须显式传 --headed：避免继承 daemon 之前锁定的模式。
    """
    subprocess.run(["agent-browser", "close"],
                   capture_output=True, text=True, timeout=15)
    time.sleep(0.5)
    mode = ["--headed"] if headed else ["--headed", "false"]
    r = subprocess.run(
        ["agent-browser", *mode, "--profile", DEFAULT_PROFILE, "open", url],
        capture_output=True, text=True, timeout=45)
    if r.returncode != 0:
        raise RuntimeError(f"无法启动浏览器: {(r.stderr or r.stdout).strip()}")
    time.sleep(2.0)


def ensure_browser_running():
    """保证 daemon 在跑、是 headless、且用的是带 cookies 的正确 profile。

    健康时 0 次 agent-browser 往返（只跑一次 pgrep，~30ms）——比先前
    「get url 探活 + pgrep」的写法更快，且补上了 profile 这个关键判据。
    """
    running, headless, profile = browser_status()
    if running and headless and _same_path(profile, DEFAULT_PROFILE):
        return

    if not running:
        why = "daemon 不在"
    elif not headless:
        why = "daemon 是 headed 模式"
    elif not profile:
        why = "无法解析 Chrome 的 --user-data-dir"
    else:
        why = f"profile 不符（当前 {profile}）"
    print(f"[xhs-notify] 重置浏览器：{why}", file=sys.stderr)
    reset_browser()


def _is_chat_url(url):
    """URL 是否在消息页。同时覆盖 /chat 和 /chat/<group_id>。

    用严格边界而非 `"/chat" in url`：后者会把 /chatgpt 之类路径也算进来，
    而且早期版本因此在 /chat/<id> 上误判「已到位」，导致 sidebar 没渲染时
    click_group 找不到群。
    """
    return bool(re.search(r"xiaohongshu\.com/chat(?:/|$|\?|#)", url or ""))


def ensure_chat_page():
    """确保当前在消息页。已在则 1 次往返返回，不做多余诊断。

    诊断留给失败路径（见 main 的 lazy diagnosis）——正常情况下不该为了
    健壮性给最短路径加往返。
    """
    url = ab("get", "url").stdout.strip()
    if _is_chat_url(url):
        return

    # SPA 深链比 click 侧栏 listitem 更可靠：sidebar 的「消息」item 名字会随
    # 未读 badge 变化（「消息」/「1消息」），且 click 在某些页面状态下不触发跳转。
    ab("open", CHAT_URL, timeout=45)
    for _ in range(3):
        time.sleep(0.5)
        if _is_chat_url(ab("get", "url").stdout):
            return
    # URL 没翻也继续：click_group 有自己的 retry，失败后 main 会做诊断，不在此误报



def click_group(group):
    """点开指定群聊。精确匹配群名前缀，找不到时重试几次。"""
    ref = None
    for attempt in range(3):
        snap = snapshot()
        ref = find_ref_by_text(snap, group, exact_name_only=True)
        if ref:
            break
        time.sleep(0.7)
    if not ref:
        raise RuntimeError(
            f"找不到群聊 {group!r}（可能列表太长被折叠、刚创建还没刷新、"
            f"或名字打错。可以手动刷新消息页或滚到列表可见区再试）")
    # 优先走 eval click（~0.2s）：直接定位 sidebar 群聊 item，绕开 CDP
    # 等元素稳定的 5s 开销。失败时降级到 click @ref（含 scroll into view）。
    clicked = ab(
        "eval",
        f"(() => {{"
        f"  const scroll = document.querySelector('.xhs-im-conv-list__scroll');"
        f"  if (!scroll) return 'no scroll';"
        f"  const target = Array.from(scroll.children)"
        f"    .find(c => c.innerText && c.innerText.includes({group!r}));"
        f"  if (!target) return 'not in DOM';"
        f" target.click();"
        f"  return 'ok';"
        f"}})()").stdout.strip().strip('"')
    if clicked != "ok":
        ab("click", f"@{ref}")
    time.sleep(0.5)


def type_and_send(text):
    """在输入框输入 text 并回车发送。"""
    snap = snapshot()
    ref = find_editable_ref(snap)
    if not ref:
        raise RuntimeError("找不到聊天输入框（contenteditable）")
    # eval focus contenteditable（~0.2s vs click @ref ~5s）。
    # 用 .focus() 而非 .click()：小红书 React 用合成事件系统，native click
    # 不会触发 React focus；直接调 native focus API 让 React 把输入框设为 active。
    ab(
        "eval",
        "(() => { const el = document.querySelector('[contenteditable]');"
        "if (!el) return 'no editor'; el.focus(); return 'ok'; })()")
    time.sleep(0.1)
    ab("keyboard", "type", text)
    ab("press", "Enter")


# ━━━━━━━━━━━━ 页面诊断 / 登录恢复（默认开启）━━━━━━━━━━━━

# classify_page 的返回值
PAGE_BLANK = "BLANK"            # 空白页，导航没完成——不是风控
PAGE_OFFSITE = "OFFSITE"        # 停在站外——不是风控
PAGE_LOGGED_IN = "LOGGED_IN"    # 已登录（sidebar 在）
PAGE_LOGIN_REQUIRED = "LOGIN"   # 真·未登录 / session 过期
PAGE_BLOCKED = "BLOCKED"        # 真·风控拦截页
PAGE_UNKNOWN = "UNKNOWN"        # 说不准，按「重新导航」处理，别当风控


def classify_page():
    """判断当前页面状态，返回 (state, url, 人话说明)。

    设计要点——**不要动不动判风控**。实践中绝大多数「打不开」其实是
    空白页或导航没落地，误判成风控会触发无谓的 close+open 重启，
    反而更容易出问题。所以只有 BLOCKED / LOGIN 才算真需要扫码，
    BLANK / OFFSITE / UNKNOWN 一律只重新导航。

    判定顺序有意如此：sidebar 检查排在登录关键词之前——只要 sidebar
    「消息」在，就是已登录，不管页面别处有没有「登录」字样（小红书的
    已登录页面同样会出现这些词，先前的实现因此误报过）。
    """
    url = (ab("get", "url").stdout or "").strip()

    if not url or url.startswith(("about:", "chrome://", "data:", "devtools://")):
        return PAGE_BLANK, url, f"空白页（{url or '无 URL'}），导航尚未完成"
    if "/website-login/error" in url or "error_code=300012" in url:
        return PAGE_BLOCKED, url, "小红书风控拦截页"
    if "xiaohongshu.com" not in url:
        return PAGE_OFFSITE, url, f"停在站外：{url}"

    snap = snapshot()
    has_sidebar = any(
        re.search(r'-\s+listitem\s+"消息"', ln) for ln in snap.splitlines())
    if has_sidebar:
        return PAGE_LOGGED_IN, url, "已登录"

    login_keywords = ("扫码登录", "手机号登录", "获取验证码", "+86")
    if any(kw in snap for kw in login_keywords):
        return PAGE_LOGIN_REQUIRED, url, "未登录 / session 已过期"

    return PAGE_UNKNOWN, url, "页面既无侧栏也无登录框，可能还在加载"


def _in_recover_cooldown():
    """是否处于恢复冷却期内（避免 session 过期时反复弹窗刷屏）。"""
    try:
        return (time.time() - os.path.getmtime(RECOVER_STAMP)) < RECOVER_COOLDOWN
    except OSError:
        return False


def _mark_recovered():
    try:
        with open(RECOVER_STAMP, "w") as f:
            f.write(str(time.time()))
    except OSError:
        pass


def clear_recover_cooldown():
    """发送成功即调用——能发出去就说明登录正常，冷却没有存在意义了。

    这样用户扫码登录后无需手工清理，下次真出问题能立即再次告警。
    """
    try:
        os.remove(RECOVER_STAMP)
    except OSError:
        pass



def notify_discord_webhook(webhook_url, content):
    """POST 到 Discord webhook。返回是否 2xx。失败不抛异常。"""
    if not webhook_url:
        return False
    try:
        payload = json.dumps({"content": content}).encode("utf-8")
        req = urllib.request.Request(
            webhook_url, data=payload,
            headers={"Content-Type": "application/json",
                     "User-Agent": USER_AGENT},
            method="POST")
        with urllib.request.urlopen(req, timeout=10) as resp:
            return 200 <= resp.status < 300
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")[:200]
        print(f"[xhs-notify] Discord HTTP {e.code}: {body}", file=sys.stderr)
        return False
    except Exception as e:
        print(f"[xhs-notify] Discord 通知异常: {type(e).__name__}: {e}",
              file=sys.stderr)
        return False


def capture_qr_png():
    """截取当前登录页（应是二维码页），返回 PNG 字节。"""
    r = subprocess.run(
        ["agent-browser", "screenshot", "--screenshot-format", "png"],
        capture_output=True, text=True, timeout=30)
    if r.returncode != 0:
        raise RuntimeError(f"screenshot 失败: {r.stderr.strip()}")
    # 输出形如：✓ Screenshot saved to /tmp/.../screenshot-1234.png
    path = None
    for line in r.stdout.splitlines():
        m = re.search(r"saved to (\S+)", line)
        if m:
            path = m.group(1)
            break
    if not path or not os.path.exists(path):
        raise RuntimeError(f"找不到截图路径: stdout={r.stdout!r}")
    with open(path, "rb") as f:
        return f.read()


def notify_discord_with_qr(webhook_url, png_bytes, caption):
    """上传 PNG 到 Discord webhook（multipart/form-data），附带文字说明。"""
    if not webhook_url:
        return False
    try:
        boundary = "----xhsNotifyBoundary7MA4YWxkTrZu0gW"
        head = (
            f"--{boundary}\r\n"
            f"Content-Disposition: form-data; name=\"payload_json\"\r\n"
            f"Content-Type: application/json\r\n\r\n"
            f"{json.dumps({'content': caption})}\r\n"
            f"--{boundary}\r\n"
            f"Content-Disposition: form-data; name=\"files[0]\"; "
            f"filename=\"qr.png\"\r\n"
            f"Content-Type: image/png\r\n\r\n"
        ).encode("utf-8")
        tail = f"\r\n--{boundary}--\r\n".encode("utf-8")
        body = head + png_bytes + tail

        req = urllib.request.Request(
            webhook_url, data=body,
            headers={"Content-Type":
                     f"multipart/form-data; boundary={boundary}",
                     "User-Agent": USER_AGENT},
            method="POST")
        with urllib.request.urlopen(req, timeout=20) as resp:
            return 200 <= resp.status < 300
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")[:200]
        print(f"[xhs-notify] Discord 图片 HTTP {e.code}: {body}",
              file=sys.stderr)
        return False
    except Exception as e:
        print(f"[xhs-notify] Discord 图片上传异常: {type(e).__name__}: {e}",
              file=sys.stderr)
        return False


def recover_from_block(webhook_url, reason="需要登录"):
    """登录恢复流程：headed 重开 → 等二维码渲染 → 截图 → Discord 上传（带图）。

    用户可在 headed 窗口扫码，或直接在 Discord 通知里扫码（手机上）。
    登录完成后关闭 headed 窗口，下次 xhs-notify 会自己把 daemon 重置回
    headless（ensure_browser_running 检测到 headed 就会重置）。

    带 30 分钟冷却：session 过期期间监控可能高频出票，不做限流会反复弹窗。
    """
    print(f"[xhs-notify] {reason}，开始恢复流程…", file=sys.stderr)
    _mark_recovered()
    try:
        reset_browser(headed=True, url=LOGIN_URL)
    except RuntimeError as e:
        print(f"[xhs-notify] headed 重开失败: {e}", file=sys.stderr)

    caption = (
        f"🔴 **xhs-notify 无法发送：{reason}**\n"
        "请用小红书 App 扫描下方二维码登录（headed 浏览器也已弹出可备用）。\n"
        "登录完成后关闭 headed 浏览器即可，下次通知会自动恢复无头模式。\n"
        f"（{RECOVER_COOLDOWN // 60} 分钟内不再重复提醒）"
    )
    try:
        png = capture_qr_png()
        ok = notify_discord_with_qr(webhook_url, png, caption)
        if ok:
            print("[xhs-notify] Discord 通知（含二维码）已发送 ✓", file=sys.stderr)
        else:
            raise RuntimeError("图片上传失败")
    except Exception as e:
        print(f"[xhs-notify] 截图/上传失败: {type(e).__name__}，降级为纯文本",
              file=sys.stderr)
        ok = notify_discord_webhook(webhook_url, caption)
        print(f"[xhs-notify] Discord 文本通知: {'已发送' if ok else '失败'}",
              file=sys.stderr)

    print("[xhs-notify] headed 浏览器已弹出 + 二维码已推 Discord，等待扫码…",
          file=sys.stderr)


def park_browser():
    """发送完成后把页面导离 /chat。

    小红书聊天页的 WebSocket + service worker 在 headless 下不受后台标签页
    降频保护，长期停留会持续泄漏——实测停 6 天渲染进程涨到 1.6GB 并占满一个
    CPU 核心。导航到 about:blank 会让该渲染进程退出，内存立即释放。

    daemon 本身保持存活（cookies 在 profile 目录，不受影响），下次发送
    只需重新加载 /chat，约 2-3s。失败不抛异常：消息已经发出去了。
    """
    try:
        ab("open", BLANK_URL, timeout=15)
    except Exception as e:
        print(f"[xhs-notify] 收尾导航失败（不影响已发送的消息）: "
              f"{type(e).__name__}", file=sys.stderr)


def _handle_login_required(args, why):
    """真·需要登录时的处理。不返回——总是以退出码 4 结束。"""
    if not args.recover:
        print(f"[xhs-notify] {why}（--no-recover，跳过恢复）", file=sys.stderr)
        sys.exit(4)
    if _in_recover_cooldown():
        print(f"[xhs-notify] {why}；{RECOVER_COOLDOWN // 60} 分钟内已提醒过，"
              f"静默退出不再弹窗", file=sys.stderr)
        sys.exit(4)
    recover_from_block(args.webhook, reason=why)
    sys.exit(4)


def send_with_fallback(args, text):
    """发送消息，失败后诊断并按阶梯升级。

    每一级都是「先诊断，再决定」，绝不因为一次失败就断定被风控——实践中
    绝大多数失败其实是页面停在空白页或导航没落地，这种情况重启浏览器
    毫无帮助，只会徒增风险。

      1. 直接尝试（最短路径）
      2. 失败 → classify_page()
           LOGIN / BLOCKED → 恢复流程（带冷却），退出 4
           LOGGED_IN       → 页面本身正常，是群名或列表渲染问题，退出 2
           其余            → 只是没导航到位：重新 open /chat 再试
      3. 再失败 → 重置浏览器（换 daemon）做最后一次尝试
      4. 仍失败 → 退出 2
    """
    attempt = 0
    while True:
        attempt += 1
        try:
            ensure_chat_page()
            click_group(args.group)
            type_and_send(text)
            return
        except RuntimeError as e:
            state, url, why = classify_page()
            print(f"[xhs-notify] 第 {attempt} 次尝试失败：{e}", file=sys.stderr)
            print(f"[xhs-notify] 页面诊断：{state} — {why}", file=sys.stderr)

            if state in (PAGE_LOGIN_REQUIRED, PAGE_BLOCKED):
                _handle_login_required(args, why)   # 不返回

            if state == PAGE_LOGGED_IN:
                # 已登录且页面正常 → 重启浏览器也救不了，是群名/列表的问题
                raise

            # BLANK / OFFSITE / UNKNOWN —— 没到位而已，不是风控
            if attempt == 1:
                print("[xhs-notify] 重新导航到消息页后重试…", file=sys.stderr)
                ab("open", CHAT_URL, timeout=45)
                time.sleep(1.5)
                continue
            if attempt == 2:
                print("[xhs-notify] 仍未到位，重置浏览器做最后一次尝试…",
                      file=sys.stderr)
                reset_browser()
                continue
            raise


def main():
    p = argparse.ArgumentParser(
        description="把通知推送到小红书群聊（via agent-browser）",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__.split("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")[0])
    p.add_argument("--group", required=True,
                   help="群聊名称（必填，无默认值）")
    p.add_argument("--title", required=True, help="通知标题")
    p.add_argument("--body", default="", help="通知正文（可选）")
    p.add_argument("--url", default="", help="链接（可选）")
    p.add_argument("--dry-run", action="store_true",
                   help="只打印计划，不实际发送")
    p.add_argument("--keep-chat", action="store_true",
                   help="发完保持在消息页（下次发送快 2-3s，但页面会持续"
                        "泄漏内存；默认发完跳 about:blank）")
    # 恢复流程现在默认开启：调用方（ticket_tracker）不传任何 flag，
    # 而 session 过期时必须能通知到人，否则就是静默失败。
    p.add_argument("--recover-on-block", dest="recover", action="store_true",
                   default=True, help=argparse.SUPPRESS)  # 兼容旧调用
    p.add_argument("--no-recover", dest="recover", action="store_false",
                   help="未登录时不弹 headed 窗口、不推 Discord（默认会）")
    p.add_argument("--webhook",
                   default=os.environ.get("XHS_WEBHOOK", DEFAULT_WEBHOOK),
                   help="Discord webhook URL（也可用 $XHS_WEBHOOK）")
    args = p.parse_args()

    parts = [args.title]
    if args.body:
        parts.append(args.body)
    if args.url:
        parts.append(args.url)
    text = "\n".join(parts)

    print(f"[xhs-notify] group={args.group!r}")
    print(f"[xhs-notify] 文本:\n{text}\n")

    if args.dry_run:
        print("[xhs-notify] dry-run，跳过实际发送")
        return

    try:
        ensure_browser_running()
        send_with_fallback(args, text)
    except subprocess.TimeoutExpired as e:
        print(f"[xhs-notify] agent-browser 命令超时: {e}", file=sys.stderr)
        sys.exit(3)
    except RuntimeError as e:
        print(f"[xhs-notify] {e}", file=sys.stderr)
        sys.exit(2)

    # 能发出去就说明登录正常，清掉冷却标记，下次真出问题能立即告警
    clear_recover_cooldown()
    if not args.keep_chat:
        park_browser()
    print("[xhs-notify] 发送成功 ✓")


if __name__ == "__main__":
    main()
