#!/usr/bin/env python3
"""Gofer 猫眼选座演示（TixDash booker 派生）。

来源: TixDash/src/tixdash/booker.py @ 250b9fa (MIT)。
目标：登录态复用 → 进入影院 → 选择一个电影/场次 → 增强选座算法选座 →
停在选座页并保持浏览器打开。

硬约束：本脚本没有任何提交订单的代码路径。它不会点击确认按钮，
也不会把订单提交给猫眼；只用于演示和本地检查。
"""

import argparse
import os
import re
import shutil
import sys
import time
import traceback
from collections import defaultdict
from datetime import date, timedelta
from pathlib import Path

# stdout 逐行 flush：被自动化（pipe）驱动时 Python 默认块缓冲，终态 marker
# 要等进程退出才 flush，外部拿不到「选好座」的即时通知。
try:
    sys.stdout.reconfigure(line_buffering=True)
except Exception:
    pass

from playwright.sync_api import sync_playwright, Page, BrowserContext, Locator

# ----------------- 常量 -----------------

USER_DATA_DIR = Path.home() / ".maoyan-booker-profile"
DEFAULT_SCREENSHOT_DIR = (
    Path(os.environ.get("XDG_STATE_HOME", str(Path.home() / ".local/state")))
    / "gofer" / "demo-screenshots"
)
SCREENSHOT_DIR = DEFAULT_SCREENSHOT_DIR
MAOYAN_BASE = "https://www.maoyan.com"
MAOYAN_FILMS = f"{MAOYAN_BASE}/films"

# 登录后右上头像区会出现"我的"或用户名；未登录显示"登录"。
# 多个 fallback 避免页面改版
LOGGED_IN_SELECTORS = [
    "text=我的",                # 已登录 nav 显示"我的"
    ".user-info",              # 头像容器 class
    "[class*='userInfo']",
    "[class*='avatar']",
]
NOT_LOGGED_IN_SELECTORS = [
    "text=登录",
    "[class*='loginBtn']",
    "[class*='login-btn']",
]


# ----------------- 选座算法 -----------------

# 默认行偏好（保留为 --row-preference 的 fallback 值；正常流程 select_seats
# 会从 DOM 自动算，见 compute_row_preference()）。这个常量的顺序是按 11 排
# 影厅实测：G > H > F > J > K > I > E > D > C > B > A，含义是"中间+1 = best，
# 前后两排次优，靠后好于靠前"。
DEFAULT_ROW_PREFERENCE = "G,H,F,J,K,I,E,D,C,B,A"


class Seat:
    """一个座位的描述 + Playwright Locator 句柄。"""
    __slots__ = ("row", "col", "locator", "_score")

    def __init__(self, row: str, col: int, locator: Locator):
        self.row = row          # 行字母 "A", "B", ...
        self.col = col          # 列号 1, 2, ...
        self.locator = locator  # Playwright locator（可点击）
        self._score = 0.0       # 评分（由评分阶段填入）

    def __repr__(self):
        return f"Seat({self.row}{self.col}, score={self._score:.3f})"


def row_sort_key(row: str):
    """Natural order for row IDs (letters A..K or numbers 1..N)."""
    text = str(row).strip()
    if text.isdigit():
        return (0, int(text))
    return (1, text)


def detect_row_labels(page: Page) -> list[str]:
    """从选座 DOM 检测所有行标签，按自然顺序排序（前→后）。

    用 .seat 而不是 .seat.selectable — sold 行也要算上，否则会漏后排或前排。
    不按字典序 sort：数字行号 1..10 会被排成 1,10,2,...，导致行偏好/中间排
    计算错误。row_sort_key 对数字行按数值、字母行按字母排序。
    """
    rows = page.evaluate(r"""() => {
        const seats = document.querySelectorAll('.seat');
        const seen = new Set();
        const out = [];
        seats.forEach(s => {
            const r = s.getAttribute('data-row-id');
            if (r && !seen.has(r)) {
                seen.add(r);
                out.push(r);
            }
        });
        return out;
    }""")
    return sorted(rows or [], key=row_sort_key)


def compute_row_preference(row_labels: list[str]) -> list[str]:
    """根据排数动态算行偏好（不再硬编码 IMAX 的 G,H,F,J,K,...）。

    规则（用户 2026-08-24 定义）：
    - Tier 1: 正中间排的后一排 = 最优 (best = middle + 1)
    - Tier 2: 正中间排本身 + best 的后一排 (即 middle 和 best+1) = 次优
    - 后续: 一层一层往外推；同层内靠后 > 靠前 ("back > front")

    这样:
    - 11 排 (A-K): G, H, F, I, E, J, D, K, C, B, A
    - 12 排 (A-L): G, H, F, I, E, J, D, K, C, L, B, A
    - 13 排 (A-M): H, I, G, J, F, K, E, L, D, M, C, B, A
    -  9 排 (A-I): F, G, E, H, D, I, C, B, A

    Args:
        row_labels: 从 DOM 检测到的所有 row-id（如 ['A','B',...,'K'] 或
                    ['1','2',...,'10']），按自然顺序（通常是前→后）

    Returns:
        按偏好从优到劣排列的 row_labels（同输入顺序，但重新排列）
    """
    n = len(row_labels)
    if n == 0:
        return []
    if n == 1:
        return list(row_labels)
    if n == 2:
        # 2 排: best = row[1]，剩下 row[0]
        return [row_labels[1], row_labels[0]]

    # 取下中位数 (lower median) — 这样奇偶排数的 "best" 都对齐到中间+1
    # N=11: middle=5 (F), best=6 (G)
    # N=12: middle=5 (F), best=6 (G)  ← 一致！
    middle = (n - 1) // 2
    best = middle + 1

    layers: list[list[int]] = [[best]]
    used: set[int] = {best}

    # Tier 2: middle, best+1
    layer = []
    if middle >= 0 and middle not in used:
        layer.append(middle)
    if best + 1 < n and (best + 1) not in used:
        layer.append(best + 1)
    if layer:
        layers.append(layer)
        used.update(layer)

    # Tier 3+: middle-(n-2), best+(n-1) 逐层外推
    layer_idx = 2  # tier 3 = 第 2 层 (0-indexed 之后)
    while True:
        back = middle - (layer_idx - 1)
        front = best + layer_idx
        layer = []
        if 0 <= back < n and back not in used:
            layer.append(back)
        if 0 <= front < n and front not in used:
            layer.append(front)
        if not layer:
            break
        layers.append(layer)
        used.update(layer)
        layer_idx += 1

    # 同层 back > front (高 index 优先)
    ranked: list[str] = []
    for layer in layers:
        for idx in sorted(layer, reverse=True):
            ranked.append(row_labels[idx])

    # 防御: 任何漏的（不应该有）
    remaining = [i for i in range(n) if i not in used]
    for idx in sorted(remaining, reverse=True):
        ranked.append(row_labels[idx])

    return ranked


# 档位定义（用户 2026-08-31 定义）：通知正文里每档都有具体含义,不光显示数字
# - 1: 正中间后一排正中命中 (best = middle+1)            N=11→G正中,  N=13→H正中
# - 2: 正中间排 或 正中间后两排正中命中                  N=11→H/F正中, N=13→I/G正中
# - 3: 区域次佳（前 3 排 × 中心列 ±3 内最优连坐）        — G/H/F 行都没正中时的兜底
# - 4: 严格正中之外,idx > middle+1（靠后）              N=11→I/J/K,   N=13→I/J/K/L/M
# - 5: 严格正中之外,idx < middle（靠前）                N=11→E/D/C/B/A, N=13→F/E/D/C/B/A
LEVEL_LABEL = {
    1: "1档 (best排正中 - 最优)",
    2: "2档 (近中间排正中)",
    3: "3档 (前3排×中心±3 区域兜底)",
    4: "4档 (靠后排 - 严格正中之外)",
    5: "5档 (靠前排 - 严格正中之外)",
}


def _classify_pick_level(row: str, row_labels: list[str]) -> int:
    """根据 row 在字母序 row_labels 中的位置返回 1-5。

    row_labels: detect_row_labels() 的输出，字母序（如 ['A','B',...,'K']）。
    未知行/空列表兜底返 5（最差档位）。
    """
    if not row_labels or row not in row_labels:
        return 5
    idx = row_labels.index(row)
    n = len(row_labels)
    middle = (n - 1) // 2
    if idx == middle + 1:           # best = 正中间后一排
        return 1
    if idx in (middle, middle + 2):  # 正中间排 / best 的后一排
        return 2
    if idx > middle + 1:
        return 4                     # 靠后
    return 5                         # 靠前


def seat_score(seat: Seat, center_col: float, pref_rank: dict, n_rows: int) -> float:
    """综合评分 0~1：行偏好排名 × 列距离中心。

    用乘法而非加法的理由：
    - 加法下，最佳行 + 最差列 ≈ 次佳行 + 最佳列（互相掩盖）
    - 乘法下，两边都得高才得高分；任一边差就显著拉低
    - 这正好满足用户"非常边缘的G肯定比不上正中的I"的判断
    """
    # 行分数：偏好排名越靠前越高分。rank 0 → 1.0，rank n-1 → 1/n
    rank = pref_rank.get(seat.row, n_rows)  # 未知行排最后
    row_score = 1.0 - (rank / max(n_rows, 1))

    # 列分数：距离中心列越近越高分。中心 → 1.0，边缘 → 0.0
    max_dist = max(center_col - 1, 1)
    col_score = max(0.0, 1.0 - abs(seat.col - center_col) / max_dist)

    return row_score * col_score


def detect_center_col(page: Page, fallback: float | None = None) -> float:
    """自动检测中心列：优先用可售座位 col 范围的中点，回退到 .seats-block 的 data-cols。

    Args:
      fallback: 用户通过 --center-col 显式指定时优先用这个
    """
    if fallback is not None:
        return float(fallback)
    # 方案 1：可售座位 col 范围
    cols_attr = page.evaluate("""() => {
        const seats = document.querySelectorAll('.seat.selectable');
        const cols = [];
        seats.forEach(s => {
            const c = parseInt(s.getAttribute('data-column-id') || '0', 10);
            if (c > 0) cols.push(c);
        });
        if (cols.length > 0) {
            cols.sort((a, b) => a - b);
            return {min: cols[0], max: cols[cols.length - 1]};
        }
        return null;
    }""")
    if cols_attr:
        return (cols_attr["min"] + cols_attr["max"]) / 2
    # 方案 2：.seats-block data-cols
    data_cols = page.evaluate("""() => {
        const el = document.querySelector('.seats-block');
        return el ? parseInt(el.getAttribute('data-cols') || '0', 10) : 0;
    }""")
    if data_cols > 0:
        return (data_cols + 1) / 2
    return 18.0  # 兜底默认


def collect_seats(page: Page) -> list[Seat]:
    """抓页面上所有可售座位为 Seat 对象列表。"""
    raw = page.locator(".seat.selectable").all()
    seats = []
    for loc in raw:
        row = loc.get_attribute("data-row-id") or ""
        col_str = loc.get_attribute("data-column-id") or "0"
        col = int(col_str)
        if row and col > 0:
            seats.append(Seat(row=row, col=col, locator=loc))
    return seats


def _normalize_date(date_str: str | None) -> str | None:
    """把"今天"/"明天"/"后天"/"2026-08-25" 都转成 'YYYY-MM-DD'，None 原样返。

    猫眼页面里日期块显示的是 "8月24日 周日"，所以函数额外把 'YYYY-MM-DD'
    截成 'MM-DD' 用于 in-row 文本匹配。
    """
    if not date_str:
        return None
    s = date_str.strip()
    today = date.today()
    if s in ("今天", "今日"):
        d = today
    elif s in ("明天", "明日"):
        d = today + timedelta(days=1)
    elif s in ("后天", "后日"):
        d = today + timedelta(days=2)
    elif s in ("大后天",):
        d = today + timedelta(days=3)
    elif re.match(r"\d{4}-\d{2}-\d{2}", s):
        return s  # 已是标准格式
    else:
        # "X月X日" 或 "X月X号" — 本年内匹配
        m = re.match(r"(\d{1,2})月(\d{1,2})[日号]", s)
        if m:
            mm, dd = int(m.group(1)), int(m.group(2))
            d = date(today.year, mm, dd)
            # 如果日期在过去，滚到明年
            if d < today:
                d = date(today.year + 1, mm, dd)
            return d.strftime("%Y-%m-%d")
        return date_str  # 不识别就原样返回
    return d.strftime("%Y-%m-%d")


# ----------------- 工具函数 -----------------

def shot(page: Page, name: str):
    """截图到 screenshots/，并原子更新 latest.png 供 Gofer 面板展示。"""
    SCREENSHOT_DIR.mkdir(parents=True, exist_ok=True)
    path = SCREENSHOT_DIR / f"{name}.png"
    try:
        page.screenshot(path=str(path), full_page=True)
        latest_tmp = SCREENSHOT_DIR / "latest.png.tmp"
        try:
            shutil.copyfile(path, latest_tmp)
            os.replace(latest_tmp, SCREENSHOT_DIR / "latest.png")
        except Exception:
            pass
        print(f"[shot] {path}")
    except Exception as e:
        print(f"[shot] 截图失败（page 可能已关闭）: {e!r}")


def is_logged_in(page: Page) -> bool:
    """登录态判断：检查 Maoyan 的登录 cookie 是否存在。

    UI 元素判断太脆（avatar 可能是 div+background-image 而非 img，
    avatar 可能延迟渲染，登录浮窗可能临时隐藏登录链接）。
    用 cookie 判断最可靠：Maoyan 登录后 _m_U / _m_H cookie 必有值。
    """
    cookies = page.context.cookies("https://www.maoyan.com")
    auth_cookie_names = {"_m_U", "_m_H", "token", "uid", "_lx_s"}
    for c in cookies:
        if c["name"] in auth_cookie_names and c.get("value"):
            return True
    return False


def wait_for_login(page: Page, timeout_sec: int = 300, log=print):
    """提示用户在 headed 浏览器中扫码，自动轮询登录态直到成功。

    不依赖 stdin（uv run / pipe 场景下 stdin 不可用）。
    扫码成功 → 猫眼页面刷新 → cookie 写入 → 我们检测到"登录"字样消失/头像出现。
    log: 日志回调（TUI 场景传入 UI 上的日志函数，避免 print 污染界面）。
    """
    log("")
    log("=" * 60)
    log("  检测到未登录。请在弹出的浏览器窗口中扫码登录。")
    log("  脚本会自动检测登录态，登录成功后自动继续。")
    log(f"  超时 {timeout_sec}s 后仍未登录会报错退出。")
    log("=" * 60)
    try:
        shot(page, "00_login")
    except Exception:
        pass
    deadline = time.time() + timeout_sec
    last_status_print = 0.0
    while time.time() < deadline:
        # 每 5 秒打印一次状态，避免用户以为卡住
        now = time.time()
        if now - last_status_print > 5:
            remaining = int(deadline - now)
            log(f"[wait-login] 等待中…剩 {remaining}s（请在浏览器里扫码）")
            last_status_print = now
        if is_logged_in(page):
            log("[ok] 检测到登录态，继续…")
            return
        time.sleep(1.5)
    raise RuntimeError(f"{timeout_sec}s 内未检测到登录，请重跑脚本（cookie 已持久化，下次不会再问）")


# ----------------- 主流程 -----------------

def navigate_to_cinema(page: Page, cinema_id: str):
    """打开影院页。等电影列表加载。"""
    url = f"{MAOYAN_BASE}/cinema/{cinema_id}"
    print(f"[nav] cinema: {url}")
    page.goto(url, wait_until="domcontentloaded")
    page.wait_for_load_state("networkidle", timeout=15000)
    # 关掉首页/影院页一进就弹的"扫码下载APP"浮窗（modal），否则后面 click_movie
    # 会误匹配 modal 里隐藏的'直接选座购票'链接（not visible → TimeoutError）
    close_app_promo_modal(page)
    shot(page, "01_cinema")


def close_xseats_notice_modal(page: Page, max_attempts: int = 3):
    """关掉 xseats 选座页的"提示性"弹窗。

    已知两类（视场次距离今天的天数触发）：
    A. "您选的是周四 8月27日的场次" → 按钮"我知道了"
    B. "请在 X 分钟内完成支付" → 倒计时提示，可能没有按钮

    策略（按优先级尝试）：
    1. JS 全文搜索 — 找包含"我知道了"文本的元素（跨 tag/role），
       可能是 <button> / <a> / <span> / <div role="button"> / <p>
    2. 找 modal 内的 X / × 关闭按钮
    3. 兜底调用 close_app_promo_modal

    因为这个 modal 不一定每次都出现，每次进 xseats 都调一次是安全的 —
    没 modal 时早返回（O(1) 选择器检查）。
    """
    for attempt in range(max_attempts):
        # 1) JS 直接搜"我知道了"文本 — 不依赖 tag/role/class
        clicked = False
        try:
            clicked = page.evaluate(r"""() => {
                const wantTexts = ['我知道了', '知道了', '我知'];
                const all = Array.from(document.querySelectorAll('*'));
                for (const el of all) {
                    // 只看叶子或近似叶子节点（少子节点）— 避免匹配到外层 <div>
                    if (el.children.length > 3) continue;
                    const txt = (el.innerText || el.textContent || '').trim();
                    if (!txt) continue;
                    // 文本必须精确（去掉空白后）匹配 wantTexts 之一
                    const compact = txt.replace(/\s+/g, '');
                    if (!wantTexts.includes(compact)) continue;
                    // 可见
                    const rect = el.getBoundingClientRect();
                    if (rect.width === 0 || rect.height === 0) continue;
                    // 不能是已 disabled 的元素
                    if (el.disabled || el.getAttribute('aria-disabled') === 'true') continue;
                    // 点击 — 使用事件派发，绕过 pointer intercept 警告
                    el.scrollIntoView({block: 'center'});
                    el.click();
                    return {
                        tag: el.tagName.toLowerCase(),
                        cls: String(el.className || '').slice(0, 60),
                        txt: compact,
                    };
                }
                return null;
            }""")
        except Exception as e:
            print(f"[notice-modal] JS 搜索失败: {e!r}")

        if clicked:
            print(f"[notice-modal] 已点击 <{clicked['tag']} class='{clicked['cls']}'> "
                  f"text='{clicked['txt']}' (attempt {attempt + 1})")
            time.sleep(0.4)
            continue  # 可能还有第二个 modal

        # 2) 找 modal 内的关闭按钮（X / ×）
        try:
            x_btn = page.locator(
                "div[class*='modal']:visible >> "
                "[class*='close' i]:visible, "
                "[aria-label='close']:visible, "
                "button:has-text('×'), button:has-text('✕')"
            ).first
            if x_btn.count() > 0 and x_btn.is_visible(timeout=500):
                x_btn.click(timeout=2000)
                print(f"[notice-modal] 已点击 X (attempt {attempt + 1})")
                time.sleep(0.4)
                continue
        except Exception:
            pass

        # 3) 没有可点的提示 modal — 退出循环
        break

    # 兜底
    close_app_promo_modal(page)


def close_app_promo_modal(page: Page):
    """关掉"扫码下载APP购票更优惠"浮窗（如果还在）。

    猫眼很多页加载后会立刻弹这个 modal，里面已经放了第一个场次的'直接选座购票'链接。
    这个链接放在 DOM 里（hidden），被 :has-text / href 选择器匹配后 Playwright 等不到 visible
    就 30s 超时。所以要在任何 click 之前关掉。

    关闭策略（按优先级尝试）：
    1. modal 内的 close 按钮 (X / 关闭 / 跳过)
    2. 点击 modal 外部（backdrop）
    3. ESC 键
    """
    modal = page.locator(
        "[class*='modal']:visible, [class*='Modal']:visible, "
        "[class*='popup']:visible, [class*='Popup']:visible, "
        "[role='dialog']:visible, [class*='app-promo']:visible, "
        "[class*='downloadDialog']:visible"
    ).first
    if modal.count() == 0:
        return

    # 1. 找 modal 内的关闭按钮
    close_candidates = [
        "button:has-text('关闭')",
        "button:has-text('跳过')",
        "button:has-text('取消')",
        "a:has-text('关闭')",
        "a:has-text('跳过')",
        "[class*='close']:visible",
        "[class*='Close']:visible",
        "[aria-label='close']:visible",
        "[aria-label='Close']:visible",
        # X 文字符号（极少数猫眼版本用纯字符）
        "button:has-text('×')",
        "button:has-text('✕')",
    ]
    for sel in close_candidates:
        loc = modal.locator(sel).first
        if loc.count() > 0:
            try:
                loc.click(timeout=2000)
                print(f"[modal] 已关闭: {sel}")
                time.sleep(0.3)
                return
            except Exception:
                continue

    # 2. 点 backdrop（modal 外面的遮罩）
    backdrop = page.locator(
        "[class*='mask']:visible, [class*='Mask']:visible, "
        "[class*='overlay']:visible, [class*='Overlay']:visible"
    ).first
    if backdrop.count() > 0:
        try:
            backdrop.click(position={"x": 5, "y": 5}, timeout=2000)
            print("[modal] 已点击 backdrop 关闭")
            time.sleep(0.3)
            return
        except Exception:
            pass

    # 3. ESC
    try:
        page.keyboard.press("Escape")
        print("[modal] 已按 ESC")
        time.sleep(0.3)
    except Exception:
        pass


def click_movie(page: Page, movie_id: str):
    """点开指定电影。等场次列表加载。"""
    # 经验：电影卡片有 data 属性；找不到时尝试文本匹配。
    print(f"[nav] click movie {movie_id}")
    # 占位选择器 — 按实际 DOM 调整
    candidates = [
        f"[data-movie-id='{movie_id}']",
        f"[data-id='{movie_id}']",
        f"a[href*='/film/{movie_id}']",
        # 猫眼影院页电影卡片的常见入口
        f"a[href*='/films/{movie_id}']",
        f"a[href*='movieId={movie_id}']",
    ]

    def _is_in_modal_or_hidden(el) -> bool:
        """判断元素是不是藏在 modal 里 / 被遮罩盖着。返回 True 跳过。"""
        try:
            visible = el.is_visible(timeout=300)
            in_modal = el.evaluate(
                "el => !!el.closest("
                "  \"[class*='modal' i],[class*='Modal'],[class*='popup' i],[class*='Popup'],[role='dialog']\""
                ")"
            )
            return (not visible) or in_modal
        except Exception:
            return True  # 检查失败也算"不可点"

    clicked = False
    for sel in candidates:
        loc = page.locator(sel)
        n = loc.count()
        for i in range(n):
            el = loc.nth(i)
            if _is_in_modal_or_hidden(el):
                continue
            try:
                el.click(timeout=5000)
                clicked = True
                print(f"[nav] 电影卡片 selector 命中: {sel} (idx={i})")
                break
            except Exception:
                continue
        if clicked:
            break
    if not clicked:
        # 兜底：扫一下页面上所有 a[href*=film]，把电影 ID 列表打出来
        try:
            candidates_a = page.evaluate("""() => {
                const out = [];
                document.querySelectorAll('a[href*="/film"]').forEach(a => {
                    const m = a.getAttribute('href').match(/film[s]?\\/(\\d+)/);
                    if (m) out.push({id: m[1], text: (a.innerText||'').trim().slice(0,40)});
                });
                return out.slice(0, 10);
            }""")
            avail = ", ".join(f"{x['id']}({x['text']})" for x in candidates_a) or "(无 /film/ 链接)"
        except Exception as ex:
            avail = f"(扫失败: {ex})"
        raise RuntimeError(
            f"找不到电影 {movie_id} 的入口。"
            f"页面上扫到的 film 链接: {avail}"
            f" 截图 {SCREENSHOT_DIR}/01_cinema.png")
    page.wait_for_load_state("networkidle", timeout=15000)
    shot(page, "02_movie")


def _click_date_tab(page: Page, ymd: str):
    """点日期 tab — 让 cinema 页面的"高速票"表格切到指定日期。

    ymd: 'YYYY-MM-DD'

    Maoyan 实际格式（从 DOM 抓到的）：
        <span class="date-item active" data-index="0">今天 8月24</span>
        <span class="date-item" data-index="1">明天 8月25</span>
        <span class="date-item" data-index="2">后天 8月26</span>
        <span class="date-item" data-index="3">周四 8月27</span>
        <span class="date-item" data-index="4">周五 8月28</span>

    关键：data-index 就是今天起算的天数（0=今天, 1=明天, ...）。
    直接按 data-index 拿最可靠。

    策略按优先级：
    1. JS: [data-index="N"] where N = delta_days from today
    2. 文本匹配（含"X月X"和"X月X日"两种格式）
    3. 全部失败时打印 [data-index] 所有 tab 供诊断
    """
    y, m, d = ymd.split("-")
    y_int, m_int, d_int = int(y), int(m), int(d)
    today = date.today()
    delta_days = (date(y_int, m_int, d_int) - today).days

    close_app_promo_modal(page)

    # 1) 最靠谱：data-index
    if delta_days >= 0:
        sel_idx = f'span.date-item[data-index="{delta_days}"]'
        loc = page.locator(sel_idx).first
        if loc.count() > 0:
            try:
                loc.scroll_into_view_if_needed(timeout=1500)
                loc.click(timeout=2000)
                print(f"[date-tab] data-index={delta_days} 命中 ({sel_idx})")
                time.sleep(0.6)
                return
            except Exception as e:
                print(f"[date-tab] data-index={delta_days} click 失败: {e!r}")

    # 2) 文本匹配：Maoyan 文本可能是 "今天 8月24" 或 "周四 8月27" — 注意没"日"字
    text_alt = f"{m_int}月{d_int}"  # "8月27"（无"日"）
    text_alt_full = f"{m_int}月{d_int}日"  # "8月27日"（兜底）
    for text in [text_alt, text_alt_full]:
        for sel in [
            f"span.date-item:has-text('{text}')",
            f"text={text}",
            f"[class*='date' i]:has-text('{text}')",
            f"li:has-text('{text}')",
            f"a:has-text('{text}')",
            f"span:has-text('{text}')",
            f"div:has-text('{text}')",
        ]:
            loc = page.locator(sel).first
            if loc.count() > 0 and loc.is_visible(timeout=300):
                try:
                    loc.scroll_into_view_if_needed(timeout=1500)
                    loc.click(timeout=2000)
                    print(f"[date-tab] 文本命中: '{text}' via {sel}")
                    time.sleep(0.6)
                    return
                except Exception:
                    continue

    # 3) 全部失败 — 打印所有 [data-index] tab 帮助诊断
    print(f"[date-tab] 没找到 {ymd} 对应的 tab，可用的 date-item:")
    try:
        tabs = page.evaluate(r"""() => {
            return Array.from(document.querySelectorAll('[data-index]')).map(el => ({
                idx: el.getAttribute('data-index'),
                text: (el.innerText || el.textContent || '').trim().slice(0, 30),
                tag: el.tagName.toLowerCase(),
                cls: String(el.className || '').slice(0, 60),
            }));
        }""")
        seen = set()
        for t in tabs:
            key = (t['idx'], t['text'])
            if key in seen:
                continue
            seen.add(key)
            print(f"  data-index={t['idx']} <{t['tag']} class='{t['cls']}'>{t['text']!r}")
    except Exception as e:
        print(f"  (列 tab 失败: {e!r})")
    print("[date-tab] 兜底：继续用当前 tab（多半是今天）的场次")


def _dump_available_showtimes(page: Page) -> list[dict]:
    """扫描页面上所有场次行 — 兜底用（诊断级）。

    同时执行：
    1. 按 '购票' 按钮回溯查行 — 找得到场次时直接返回
    2. 把整个 <body> innerHTML dump 到 .cache/page.html 便于用户/我检查 DOM
    3. 抓所有 'xseats' 链接，作为旁路

    之前 dump 返回 [] 的原因有三个常见：(a) click 后表格还没渲染完；
    (b) 按钮在屏幕外 / opacity:0；(c) modal 把按钮挡了。此版本把它们都点出来。
    """
    # 让任何异步 render 跑完再 dump
    try:
        page.wait_for_load_state("networkidle", timeout=5000)
    except Exception:
        pass
    time.sleep(0.8)

    # 1) dump 整页 HTML，便于本地检查
    try:
        Path("/Users/kuicao/Applications/maoyan-booker/.cache").mkdir(
            parents=True, exist_ok=True
        )
        html_path = "/Users/kuicao/Applications/maoyan-booker/.cache/page.html"
        Path(html_path).write_text(page.content(), encoding="utf-8")
        print(f"[dump] 页面 HTML 已存到 {html_path}")
    except Exception as e:
        print(f"[dump] 存 HTML 失败: {e}")

    # 2) 主 dump：找购票按钮 + 回溯行
    try:
        result = page.evaluate(r"""() => {
            const out = [];
            const seen = new Set();
            const buyKeywords = ['选座购票', '购票', '立即预订', '选座', '立即购买', '订票'];

            // 抓所有可能是按钮的元素
            const cands = document.querySelectorAll('a, button, [class*="btn" i]');
            const btns = [];
            cands.forEach(el => {
                const t = (el.innerText || el.textContent || '').trim();
                if (!t) return;
                if (buyKeywords.some(k => t.includes(k))) {
                    btns.push({el, text: t.slice(0, 20)});
                }
            });

            for (const {el: btn, text} of btns) {
                // 跳过 modal 里的按钮
                if (btn.closest('[class*="modal" i],[class*="popup" i],[role="dialog"]')) {
                    out.push({_skip: 'modal', text, tag: btn.tagName.toLowerCase()});
                    continue;
                }
                // 跳过不可见的按钮
                const rect = btn.getBoundingClientRect();
                if (rect.width === 0 || rect.height === 0) {
                    out.push({_skip: 'invisible', text, tag: btn.tagName.toLowerCase()});
                    continue;
                }
                // 回溯到行
                const row = btn.closest(
                    'tr, li, ul > li, [class*="row" i], [class*="item" i], [class*="cell" i]'
                ) || btn.parentElement;
                const rowText = (row.innerText || '').replace(/\s+/g, ' ').trim().slice(0, 200);
                const t = rowText.match(/(\d{1,2}:\d{2})/);
                const p = rowText.match(/[¥￥]\s*(\d+(?:\.\d+)?)/);
                const a = btn.closest('a') || (btn.tagName === 'A' ? btn : null);
                const href = a ? (a.href || '') : '';
                if (seen.has(rowText + href)) continue;
                seen.add(rowText + href);
                out.push({
                    text: rowText,
                    time: t ? t[1] : '',
                    price: p ? p[1] : '',
                    href,
                    btnText: text,
                });
            }

            // 旁路：抓 xseats 链接，看哪条路直连选座页
            const xseats = [];
            document.querySelectorAll('a[href*="/xseats/"]').forEach(a => {
                xseats.push({href: a.href, text: (a.innerText||'').trim().slice(0,20)});
            });
            return {rows: out, xseats: xseats.slice(0, 10)};
        }""")
    except Exception as e:
        return [{"_error": str(e)}]

    # 把 xseats 旁路结果也返回出去 — 给 click_showtime 备用
    if isinstance(result, dict):
        return result.get("rows", []) + [
            {"_xseats": True, **x} for x in result.get("xseats", [])
        ]
    return result if isinstance(result, list) else []


def _find_xseats_url_for_showtime(page: Page, target_time: str,
                                   target_date_norm: str | None,
                                   movie_id: str | None) -> str | None:
    """直接找场次 row 对应的 xseats URL — 绕过整个 modal dance。

    Maoyan cinema 页每个 <tr class="..."> 的最后一个 <td> 里有：
      <div class="modal-mask">
        ...
        <a href="/xseats/{id}?movieId=...&cinemaId=..." class="modal-buy-btn">直接选座购票</a>
      </div>
      <span class="buy-btn normal" data-act="show-click">选座购票</span>

    既然 row 内部藏着 xseats URL，我们直接读出来，比触发 modal + 点"直接选座购票"稳得多。

    ⚠️ 注意：cinema 页可能同时有多个电影+多个日期的 rows，其中只有 1 个电影+1 个日期
    的 table 是可见的（display:block），其它的 display:none。这里不能用 visible 来过滤 —
    否则只看见活跃电影。全部 row 都查，按 movieId + URL 日期精确挑。

    0 匹配时也要打诊断（之前太安静了，看不出为啥）。

    Args:
      target_time: '22:55'
      target_date_norm: 'YYYY-MM-DD' 或 None
      movie_id: 过滤用

    Returns:
      完整 xseats URL 或 None
    """
    # ⭐ 时间规范化：'9:45' -> '09:45'，'13:00' -> '13:00'
    # Maoyan DOM 里 begin-time 永远是 HH:MM 两位，但用户可能传 '9:45'
    def _norm_time(t: str) -> str:
        t = (t or "").strip()
        m = re.match(r"^(\d{1,2}):(\d{1,2})$", t)
        if not m:
            return t
        return f"{int(m.group(1)):02d}:{int(m.group(2)):02d}"

    target_time_norm = _norm_time(target_time)
    print(f"[find-xseats] 开始: target={target_time!r} "
          f"normalized={target_time_norm!r} "
          f"movieId={movie_id or '*'} "
          f"date={target_date_norm or '*'}")

    try:
        result = page.evaluate(r"""({targetTime, targetDate, movieId}) => {
            // 内部把两边都规整成 HH:MM 再比 — 处理 '9:45' vs '09:45'
            function normTime(s) {
                const m = String(s || '').trim().match(/^(\d{1,2}):(\d{2})$/);
                if (!m) return '';
                return (parseInt(m[1], 10).toString().padStart(2, '0') + ':' + m[2]);
            }
            const targetNorm = normTime(targetTime);
            const timeRe = new RegExp('^\\s*' + targetNorm + '\\s*$');
            const rows = Array.from(document.querySelectorAll('tr'));
            const out = [];
            const all_with_time = []; // 不带过滤的 — 0 匹配时做诊断
            for (const row of rows) {
                const beginTimeEl = row.querySelector('span.begin-time');
                if (!beginTimeEl) continue;
                const rowTime = (beginTimeEl.innerText || beginTimeEl.textContent || '').trim();
                if (!rowTime.match(/\d{1,2}:\d{2}/)) continue;

                const a = row.querySelector('div.modal-mask a[href*="/xseats/"]');
                if (!a) continue;
                let href = a.getAttribute('href') || '';
                if (href.startsWith('/')) href = location.origin + href;
                const mId = (href.match(/movieId=(\d+)/) || [])[1] || '';
                const urlYmd = (href.match(/xseats\/(\d{8})/) || [])[1] || '';

                const rect = row.getBoundingClientRect();
                const visible = rect.width > 0 && rect.height > 0;

                all_with_time.push({
                    time: rowTime, movieId: mId, urlYmd, visible,
                    href: href.slice(0, 90),
                });

                if (!timeRe.test(rowTime)) continue;
                if (movieId && mId !== movieId) continue;

                out.push({
                    time: rowTime, href, visible,
                    urlYmd, dateText: '',
                });
            }
            return {matched: out, all_with_time, targetNorm};
        }""", {"targetTime": target_time, "targetDate": target_date_norm or "",
               "movieId": movie_id or ""})
    except Exception as e:
        print(f"[find-xseats] JS 错误: {e}")
        return None

    # JS 现在返回 dict {matched, all_with_time}
    if isinstance(result, dict):
        matched = result.get("matched") or []
        all_with_time = result.get("all_with_time") or []
    else:
        matched = result or []
        all_with_time = []

    # ⭐ 关键修复：0 匹配时也要打印诊断 — 之前太安静
    print(f"[find-xseats] target={target_time!r} movieId={movie_id or '*'} "
          f"date={target_date_norm or '*'}: matched={len(matched)} "
          f"total_rows_with_time={len(all_with_time)}")

    if matched:
        for r in matched:
            print(f"  ✓ time={r.get('time')} urlYmd={r.get('urlYmd')} "
                  f"visible={r.get('visible')} "
                  f"href={r.get('href', '')[:90]}")
    elif all_with_time:
        # 显示全部 begin-time 行的诊断
        from collections import Counter
        time_counter = Counter(r['time'] for r in all_with_time)
        movie_counter = Counter(r['movieId'] for r in all_with_time
                                if r['time'] == target_time or True)
        print(f"[find-xseats] 0 匹配。看页面上有什么：")
        print(f"  全部时间分布: {dict(time_counter)}")
        print(f"  {target_time} 行的 movieId 分布:")
        for r in all_with_time:
            if r['time'] == target_time:
                print(f"    movieId={r['movieId']} urlYmd={r['urlYmd']} "
                      f"visible={r['visible']}")

    if not matched:
        return None

    # 匹配后按 urlYmd 过滤目标日期；不要窜到错日期
    if target_date_norm:
        target_ymd = target_date_norm.replace("-", "")
        dated = [r for r in matched if r.get("urlYmd") == target_ymd]
        if dated:
            return dated[0]["href"]
        print(f"[find-xseats] 没有 urlYmd={target_ymd} 的 row，候选: "
              f"{sorted(set(r['urlYmd'] for r in matched))}")
        return None

    return matched[0]["href"]


def _find_nearest_xseats_url(page: Page, target_time: str,
                             target_date_norm: str | None,
                             movie_id: str | None,
                             tolerance_min: int = 60
                             ) -> tuple[str | None, dict | None]:
    """目标时间无场次时，找 ±tolerance_min 分钟内最近的场次。

    Returns:
      (url, info) — url 非空就跳；info['candidates'] 是全部候选（用于诊断）
      都 None 表示 JS 出错或无候选
    """
    try:
        candidates = page.evaluate(r"""({targetTime, targetDate, movieId, tolMin}) => {
            function timeToMin(s) {
                const m = String(s || '').trim().match(/^(\d{1,2}):(\d{2})$/);
                if (!m) return null;
                return parseInt(m[1], 10) * 60 + parseInt(m[2], 10);
            }
            const targetMin = timeToMin(targetTime);
            const targetYmd = (targetDate || '').replace(/-/g, '');
            const rows = Array.from(document.querySelectorAll('tr'));
            const out = [];
            for (const row of rows) {
                const beginTimeEl = row.querySelector('span.begin-time');
                if (!beginTimeEl) continue;
                const rowTime = (beginTimeEl.innerText || beginTimeEl.textContent || '').trim();
                const rowMin = timeToMin(rowTime);
                if (rowMin === null) continue;

                const a = row.querySelector('div.modal-mask a[href*="/xseats/"]');
                if (!a) continue;
                let href = a.getAttribute('href') || '';
                if (href.startsWith('/')) href = location.origin + href;
                const mId = (href.match(/movieId=(\d+)/) || [])[1] || '';
                const urlYmd = (href.match(/xseats\/(\d{8})/) || [])[1] || '';

                // 过滤：同电影 + 同日期（targetDate 给定的情况下）
                if (movieId && mId !== movieId) continue;
                if (targetYmd && urlYmd !== targetYmd) continue;

                const dist = Math.abs(rowMin - targetMin);
                out.push({
                    time: rowTime,
                    distance: dist,
                    href,
                    movieId: mId,
                    urlYmd,
                });
            }
            // 距离小优先；相同距离下选稍晚的（更接近"下一场"）
            out.sort((a, b) => (a.distance - b.distance) || (-1));
            return {targetMin, candidates: out, toleranceMin: tolMin};
        }""", {"targetTime": target_time,
               "targetDate": target_date_norm or "",
               "movieId": movie_id or "",
               "tolMin": tolerance_min})
    except Exception as e:
        print(f"[find-nearest] JS 错误: {e!r}")
        return None, None

    target_min = candidates.get("targetMin") if isinstance(candidates, dict) else None
    cands = candidates.get("candidates", []) if isinstance(candidates, dict) else []
    tol = candidates.get("toleranceMin", tolerance_min) if isinstance(candidates, dict) else tolerance_min

    print(f"[find-nearest] target={target_time!r} ({target_min}min) "
          f"movieId={movie_id or '*'} date={target_date_norm or '*'} "
          f"tolerance=±{tol}min: 候选 {len(cands)} 个")

    if not cands:
        return None, {"candidates": [], "tolerance_min": tol, "reason": "no_rows"}

    best = cands[0]
    if best["distance"] > tol:
        # 超出容差 — 列全部可用场次给用户看
        available = sorted({c['time'] for c in cands})
        print(f"[find-nearest] 最近 {best['time']} (距 {best['distance']}min) "
              f"也超出 ±{tol}min 容差")
        print(f"[find-nearest] {target_date_norm or '该日期'} "
              f"{movie_id or '任意电影'} 的可用场次: {available}")
        return None, {"candidates": cands, "tolerance_min": tol,
                      "available_times": available, "reason": "out_of_tolerance"}

    # 在容差内 — 打印决策
    print(f"[find-nearest] 目标 {target_time} 无场次，"
          f"就近选 {best['time']} (距离 {best['distance']}min, "
          f"urlYmd={best['urlYmd']}, movieId={best['movieId']})")
    return best["href"], best


def _parse_time_range(s: str) -> tuple[int, int]:
    """解析 '15:00-19:00' → (start_min, end_min) 当日分钟数。

    hour 归一化为两位（DOM 里 begin-time 永远是 HH:MM，用户可能传 H:MM，
    见 README 踩坑记录 #1）。跨天范围（end <= start）不支持。
    """
    if not s:
        raise ValueError("--show-time-range 不能为空")
    m = re.match(r"^\s*(\d{1,2}):(\d{2})\s*-\s*(\d{1,2}):(\d{2})\s*$", s)
    if not m:
        raise ValueError(
            f"--show-time-range 格式应为 'HH:MM-HH:MM'（如 '15:00-19:00'），"
            f"收到: {s!r}")
    sh, sm, eh, em = (int(g) for g in m.groups())
    if sh > 23 or sm > 59 or eh > 23 or em > 59:
        raise ValueError(f"--show-time-range 时间非法: {s!r}")
    start, end = sh * 60 + sm, eh * 60 + em
    if end <= start:
        raise ValueError(
            f"--show-time-range 结束时间必须晚于开始（不支持跨天）: {s!r}")
    return start, end


def _find_sessions_in_range(page: Page, start_min: int, end_min: int,
                            target_date_norm: str | None,
                            movie_id: str | None) -> tuple[list[dict], list[dict]]:
    """找目标日期内、时间落在 [start_min, end_min] 区间里的全部场次。

    扫描模式仿 _find_nearest_xseats_url：遍历 <tr> 读 span.begin-time +
    div.modal-mask a[href*=/xseats/]，按 movieId + urlYmd 过滤后，再按
    行分钟是否落在范围内过滤（两者统一按 HH:MM 归一化）。

    Returns:
      (sessions, all_rows)
      sessions: [{time, minutes, url, price, movieId, urlYmd}] 按时间升序
      all_rows: 过滤前扫描到的所有行（诊断用）
    """
    try:
        result = page.evaluate(r"""({startMin, endMin, targetDate, movieId}) => {
            function toMin(s) {
                const m = String(s || '').trim().match(/^(\d{1,2}):(\d{2})$/);
                if (!m) return null;
                return parseInt(m[1], 10) * 60 + parseInt(m[2], 10);
            }
            const targetYmd = (targetDate || '').replace(/-/g, '');
            const rows = Array.from(document.querySelectorAll('tr'));
            const all = [];
            const matched = [];
            for (const row of rows) {
                const beginEl = row.querySelector('span.begin-time');
                if (!beginEl) continue;
                const rowTime = (beginEl.innerText || beginEl.textContent || '').trim();
                const minutes = toMin(rowTime);
                if (minutes === null) continue;

                const a = row.querySelector('div.modal-mask a[href*="/xseats/"]');
                if (!a) continue;
                let href = a.getAttribute('href') || '';
                if (href.startsWith('/')) href = location.origin + href;
                const mId = (href.match(/movieId=(\d+)/) || [])[1] || '';
                const urlYmd = (href.match(/xseats\/(\d{8})/) || [])[1] || '';
                const priceM = (row.innerText || '').match(/[¥￥]\s*(\d+(?:\.\d+)?)/);
                // 厅类型：<span class="hall">IMAX 激光厅</span>（lang 是语言，别混）
                const hallEl = row.querySelector('span.hall');
                const hall = hallEl
                    ? (hallEl.innerText || hallEl.textContent || '').trim()
                    : '';

                const rec = {
                    time: rowTime, minutes, url: href,
                    price: priceM ? priceM[1] : '',
                    movieId: mId, urlYmd,
                    hall,
                    imax: /imax/i.test(hall),
                };
                all.push(rec);
                if (movieId && mId !== movieId) continue;
                if (targetYmd && urlYmd !== targetYmd) continue;
                if (minutes < startMin || minutes > endMin) continue;
                matched.push(rec);
            }
            matched.sort((a, b) => a.minutes - b.minutes);
            return {matched, all};
        }""", {"startMin": start_min, "endMin": end_min,
               "targetDate": target_date_norm or "",
               "movieId": movie_id or ""})
    except Exception as e:
        print(f"[range] 范围枚举 JS 错误: {e!r}")
        return [], []

    if isinstance(result, dict):
        matched = result.get("matched") or []
        all_rows = result.get("all") or []
    else:
        matched, all_rows = result or [], []
    print(f"[range] {target_date_norm or '(当前日期)'} "
          f"{movie_id or '全部电影'} 区间内命中 {len(matched)} 场 "
          f"(扫描到 {len(all_rows)} 行)")
    return matched, all_rows


def _best_batch_in_zone(seats: list[Seat], num_seats: int,
                        force_consecutive: bool,
                        row_preference: list[str],
                        center_col: float,
                        trail: list[tuple[int, str, str]] | None = None) -> list[Seat]:
    """最佳位置区域内凑 num_seats 张（连坐/独立）的最优批次。

    区域 = 偏好前 3 排（最佳排+前后次佳排） × 中心列 ±3。
    该场次能否在区域内凑出可行批次 → 返回非空 list（区内最优，列号物理连续）。

    trail: 可选,命中或无解时追加 (3, "区域次佳", info)。
    """
    zone_rows = set(row_preference[:3])
    lo, hi = int(center_col) - 3, int(center_col) + 3
    zseats = [s for s in seats if s.row in zone_rows and lo <= s.col <= hi]
    if force_consecutive:
        picks = _select_consecutive(zseats, num_seats, row_preference)
    else:
        picks = _select_individual(zseats, num_seats)
    if trail is not None:
        if picks:
            cols = [f"{s.row}{s.col}" for s in picks]
            info = f"命中 {','.join(cols)}"
        else:
            info = "无解"
        trail.append((3, "区域次佳", info))
    return picks


def _row_midpoint(row_seats: list[Seat]) -> float:
    """算某一行的"中线位置"（连续 0.5，比如 14.5 意味着 14 和 15 之间是视觉中线）。

    不同影院/不同影厅/不同排的"中间"是不一样的（用户 2026-08-28 实测）：
    - F 排（cols 1..28）：中间是 14 和 15 → 中线 14.5
    - G 排（cols 1..32）：中间是 16 和 17 → 中线 16.5
    - H 排（cols 1..36）：中间是 18 和 19 → 中线 18.5
    老的 `detect_center_col` 是用**全局** cols 算的，G 排被错算成 14, 15，
    就是因为全局中点（15.5）和 G 排自己的中点（16.5）不一致。

    算法（2026-08-28）：
    用**该行**可售座位的几何中点 (min_col + max_col) / 2.0。
    不考虑 DOM 里的过道 gap — 因为实测猫眼"中间两个座位"是物理相邻的，
    中线只是视觉线条（DOM 上这两个座位之间没有空缺列）。
    """
    if not row_seats:
        return 0.0
    sorted_cols = sorted(s.col for s in row_seats)
    return (sorted_cols[0] + sorted_cols[-1]) / 2.0


def _find_middle_block_in_row(row_seats: list[Seat], n_seats: int) -> list[Seat]:
    """找某一行"以该行中线为锚点"的严格中间连续 n_seats 个座位。

    用户（2026-08-28）要求语义："中间两个座位"必须是**正中间那一对**，而不是
    "任何近中间的连续块"。否则 G 排没有正中块时会被错选成 (G17, G18) 之类。

    Args:
      row_seats: 该行的所有可售座位（同 row 的 Seat 列表）
      n_seats: 要几张（1-6）

    Returns:
      正好是该行正中位置的那 n_seats 个连续 Seat（按 col 升序），
      列号物理连续；不是正中位置 或 中间座位有缺位 → 返 []

    算法（严格正中）：
    1. 算该行中线 mid = _row_midpoint()
    2. 严格正中窗口起点 = int(mid - (n - 1) / 2)（向下取整让窗口对称在中线）
       - n=2, mid=14.5 → start=14 → 期望 [14, 15]（正中）
       - n=2, mid=16.5 → start=16 → 期望 [16, 17]（正中）
       - n=4, mid=14.5 → start=13 → 期望 [13, 14, 15, 16]（正中）
    3. 校验 [start..start+n-1] 这些列都存在且连续可售；否则返回 []
    """
    if len(row_seats) < n_seats:
        return []
    sorted_seats = sorted(row_seats, key=lambda s: s.col)
    mid = _row_midpoint(sorted_seats)
    # 严格正中窗口的起点（向下取整）
    ideal_start = int(mid - (n_seats - 1) / 2)
    # 从 sorted_seats 中找出 col ∈ [ideal_start, ideal_start + n_seats - 1] 的连续 n 个
    expected_cols = set(range(ideal_start, ideal_start + n_seats))
    picked = [s for s in sorted_seats if s.col in expected_cols]
    if len(picked) != n_seats:
        return []
    picked.sort(key=lambda s: s.col)
    # 列号必须物理连续（防御 DOM 中间有缺位）
    if any(picked[j + 1].col != picked[j].col + 1 for j in range(n_seats - 1)):
        return []
    return picked


def _select_by_row_middle(seats: list[Seat], n_seats: int,
                          row_preference: list[str],
                          row_labels: list[str] | None = None,
                          trail: list[tuple[int, str, str]] | None = None) -> list[Seat]:
    """按行偏好顺序，逐行尝试其"中间块"。

    优先级（用户 2026-08-28 定义）：
    1. 最佳排（row_preference[0]）的中间块
    2. 第 2 名偏好排的中间块
    3. 第 3 名偏好排的中间块
    4. 返回 []（外层继续走"最佳区域"→多场次比较兜底）

    返回第一个找到的非空块；都找不到返 []。
    行偏好前 3 名 = tier 1+2（见 compute_row_preference），即中间+1、最中间、最中间+1

    trail: 可选,每行尝试后追加 (level, label, info)。label="X正中";info="命中 X14-15"或"无正中块"。

    row_labels: 可选,字母序的原始行列表(如 ['A','B',...,'K'])。仅在 trail 非空时
    需要(用于 _classify_pick_level 算档位);未传时从 seats 里现取,精确度够用。
    """
    by_row: dict[str, list[Seat]] = defaultdict(list)
    for s in seats:
        by_row[s.row].append(s)
    # row_labels 未传时,兜底从 seats 里现取字母序 — 用于档位分类(不是必须精确,
    # 因为档位判定只在 trail 收集时跑,而跑 trail 的 select_best_seats 一定会传)
    if trail is not None and row_labels is None:
        row_labels = sorted({s.row for s in seats})

    for row in row_preference[:3]:
        row_seats = by_row.get(row, [])
        block = _find_middle_block_in_row(row_seats, n_seats)
        if trail is not None:
            level = _classify_pick_level(row, row_labels or [])
            label = f"{row}正中"
            if block:
                info = f"命中 {row}{block[0].col}-{row}{block[-1].col}"
                trail.append((level, label, info))
                return block
            trail.append((level, label, "无正中块"))
        elif block:
            return block
    return []


def _evaluate_session(page: Page, session: dict,
                      num_seats: int, force_consecutive: bool,
                      row_preference: list[str] | None,
                      center_col: float | None,
                      auto_row_preference: bool) -> dict:
    """加载一个场次的选座图，只读评估其最优座位批次质量。

    只读不点（locator 绑当前页，评估完就 goto 走人，不产生点击副作用）。
    返回 dict：
      成功: {time, url, price, batch: [(row,col),...], avg_score, top_score}
      失败: {time, url, price, reason}
    """
    time_str = session["time"]
    url = session["url"]
    print(f"\n[range-eval] 评估 {time_str} … ({url[:70]})")
    try:
        page.goto(url, wait_until="domcontentloaded")
        page.wait_for_load_state("networkidle", timeout=15000)
        wait_seat_map(page)  # 内含 close_xseats_notice_modal
        close_xseats_notice_modal(page)
    except Exception as e:
        return {"time": time_str, "url": url, "price": session.get("price", ""),
                "reason": f"选座页加载失败: {e!r}"}

    try:
        # 动态行偏好（复刻 select_seats 开头逻辑）
        if auto_row_preference and row_preference is None:
            labels = detect_row_labels(page)
            if labels:
                row_preference = compute_row_preference(labels)
        seats = collect_seats(page)
        # 选座图没加载出来判断：.seat 元素一个都没有
        if page.locator(".seat").count() == 0:
            return {"time": time_str, "url": url, "price": session.get("price", ""),
                    "reason": "选座图未加载（.seat 元素为 0）"}
        if not seats:
            return {"time": time_str, "url": url, "price": session.get("price", ""),
                    "reason": "无可用座位（全部售罄？）"}
        # 兜底：连动态行偏好都没算出来时，按实际存在的行当偏好，保证连坐可算
        if not row_preference:
            row_preference = sorted({s.row for s in seats})

        center = detect_center_col(page, center_col)
        pref_rank = {r.strip(): i for i, r in enumerate(row_preference)}
        n_rows = len(pref_rank)
        for s in seats:
            s._score = seat_score(s, center, pref_rank, n_rows)

        if force_consecutive:
            # 与 select_best_seats 同样的优先级：
            # 行偏好前 3 排的中间块 → 最佳区域 → 全局连坐搜索
            batch = _select_by_row_middle(seats, num_seats, row_preference)
            if not batch:
                batch = _best_batch_in_zone(seats, num_seats, force_consecutive,
                                            row_preference, center)
            if not batch:
                batch = _select_consecutive(seats, num_seats, row_preference)
        else:
            batch = _select_individual(seats, num_seats)
        if not batch:
            mode = "连续" if force_consecutive else "独立"
            return {"time": time_str, "url": url,
                    "price": session.get("price", ""),
                    "reason": f"{num_seats} 个{mode}可售座位不足"}

        batch.sort(key=lambda s: (s.row, s.col))
        avg = sum(s._score for s in batch) / len(batch)
        top = max(s._score for s in batch)
        picked = [(s.row, s.col) for s in batch]
        print(f"[range-eval] {time_str} → 批次 {picked}  均分 {avg:.3f}")

        # 最佳位置区域硬性条件：偏好前 3 排 × 中心列 ±3，凑 num_seats 张连坐。
        # 该场次能否在区域内凑出可行批次 → zone_batch 非空（区外最优也算进去）。
        lo, hi = int(center) - 3, int(center) + 3
        zone_batch = _best_batch_in_zone(seats, num_seats, force_consecutive,
                                         row_preference, center)
        zone_picked = [(s.row, s.col) for s in zone_batch]
        zone_avg = (sum(s._score for s in zone_batch) / len(zone_batch)
                    if zone_batch else None)
        if zone_picked:
            print(f"[range-eval] {time_str} 最佳区域 "
                  f"(行 {'/'.join(row_preference[:3])} 列{lo}~{hi}) "
                  f"内最优批次 {zone_picked}  均分 {zone_avg:.3f} ✓")
        return {"time": time_str, "url": url, "price": session.get("price", ""),
                "batch": picked, "avg_score": avg, "top_score": top,
                "zone_batch": zone_picked, "zone_avg": zone_avg}
    except Exception as e:
        return {"time": time_str, "url": url, "price": session.get("price", ""),
                "reason": f"评估异常: {e!r}"}


def _try_demo_session(page: Page, session: dict,
                      num_seats: int, force_consecutive: bool,
                      row_preference: list[str] | None,
                      center_col: float | None,
                      auto_row_preference: bool,
                      zone_only: bool = False) -> str:
    """进一个场次：评估可售性 → pick 座位 → 停在选座页（绝不提交订单）。

    zone_only=True: 只接受在"最佳位置区域"内的批次，并精确点选区域批次。

    Returns:
      "stopped"                  — 已选好座位，停在选座页。
      "failed_before_selection"  — 可售性/选座失败，没有提交任何东西。
    """
    ev = _evaluate_session(page, session, num_seats, force_consecutive,
                           row_preference, center_col, auto_row_preference)
    if "reason" in ev:
        print(f"[demo] {session['time']} 不可行: {ev['reason']}")
        return "failed_before_selection"
    if zone_only and not ev.get("zone_batch"):
        print(f"[demo] {session['time']} 无最佳区域批次，跳过")
        return "failed_before_selection"

    time_str = session["time"]
    # pick 座位（在 _evaluate_session 已加载的同一页面上现选现点，locator 有效）
    try:
        if zone_only:
            spec = [f"{r}排{c}座" for r, c in ev["zone_batch"]]
            select_seats(page, spec, "manual", num_seats=num_seats,
                         force_consecutive=force_consecutive,
                         row_preference=row_preference or None,
                         center_col=center_col,
                         auto_row_preference=auto_row_preference)
        else:
            select_seats(page, None, "manual", num_seats=num_seats,
                         force_consecutive=force_consecutive,
                         row_preference=row_preference or None,
                         center_col=center_col,
                         auto_row_preference=auto_row_preference)
    except Exception as e:
        print(f"[demo] {time_str} pick 失败: {e!r}")
        return "failed_before_selection"

    stop_at_seat_map(page, session=session)
    return "stopped"


def run_range_mode(page: Page, args):
    """时间范围模式：逐场评估，选出最佳场次后停在选座页（不提交订单）。

    - 逐场评估按时间升序（_find_sessions_in_range 已排好）。
    - 硬性条件：某场能在"最佳位置区域"（偏好前 3 排 × 中心列 ±3）内凑出
      num_seats 张连坐 → 记为 satisfied，不再检查后续场次。
    - 没有一场满足硬性条件 → 退回按最优座位质量排行兜底。
    - 选定后只选座并停留，绝不点击确认或提交订单。
    """
    date_norm = _normalize_date(args.show_date or "今天")
    row_pref = ([r.strip() for r in args.row_preference.split(",") if r.strip()]
                if args.row_preference else None)
    start_min, end_min = _parse_time_range(args.show_time_range)
    imax_only = bool(getattr(args, "imax_only", False))
    print(f"[range] date={date_norm} 电影={args.movie_id or '(全部)'} "
          f"票数={args.num_seats} 连坐={args.force_consecutive}"
          + (" | 仅 IMAX" if imax_only else ""))

    # 1. 切到目标日期 tab（复刻 click_showtime 的流程）
    if date_norm:
        _click_date_tab(page, date_norm)
        time.sleep(2.0)
        close_app_promo_modal(page)

    # 2. 枚举范围内场次
    sessions, _ = _find_sessions_in_range(
        page, start_min, end_min, date_norm, args.movie_id)
    if not sessions:
        available = _dump_available_showtimes(page)
        shot(page, "99_showtime_not_found")
        rows = [a for a in available if 'time' in a]
        avail_str = ("\n".join(
            f"  [{a.get('btnText', '?')}] time={a.get('time', '')} "
            f"price=¥{a.get('price', '')}" for a in rows)
            if rows else "  (没扫到带时间的行)")
        raise RuntimeError(
            f"时间范围 {args.show_time_range}（{date_norm}）内没有可选场次。\n"
            f"页面上扫到的场次:\n{avail_str}\n"
            f"HTML: /Users/kuicao/Applications/maoyan-booker/.cache/page.html")
    for s in sessions:
        print(f"    ✓ {s['time']}"
              + (f" {s.get('hall', '')}" if s.get('hall') else "")
              + (f" ¥{s['price']}" if s.get('price') else "")
              + f"  {s['url'][:80]}")

    # 2.5 仅 IMAX：非 IMAX 场次直接跳过，不用评估最佳座位
    if imax_only:
        skipped = sum(1 for s in sessions if not s.get("imax"))
        if skipped:
            print(f"[range] --imax-only：跳过 {skipped} 场非 IMAX "
                  f"（剩 {len(sessions) - skipped} 场 IMAX）")
        sessions = [s for s in sessions if s.get("imax")]
        if not sessions:
            raise RuntimeError(
                f"时间范围 {args.show_time_range}（{date_norm}）内没有 IMAX 场次"
                f"（--imax-only 只订 IMAX 厅）。")

    shot(page, "10_range_candidates")

    # 3. 逐场评估（时间升序），最佳区域硬性条件短路：
    #    "如果检查的第一场就落入最佳区域 → 不再检查下一场，直接锁票"
    print("[range] 逐场评估（时间升序）。最佳区域 = 偏好前3排 × 中心列±3，"
          "第一场能凑出连坐即停。")
    satisfied, evaluated = None, []
    for s in sessions:
        ev = _evaluate_session(page, s, args.num_seats, args.force_consecutive,
                               row_pref, args.center_col,
                               args.auto_row_preference)
        evaluated.append(ev)
        if "reason" in ev:
            print(f"[range] {ev['time']} 排除: {ev['reason']}")
            continue
        batch_str = "".join(f"{r}{c}" for r, c in ev["batch"])
        in_zone = bool(ev.get("zone_batch"))
        print(f"[range] {ev['time']} 最优批次 {batch_str} 均分 {ev['avg_score']:.3f}"
              + (" — 落入最佳区域 ✓" if in_zone else " — 不在最佳区域，继续下一场"))
        if in_zone:
            satisfied = ev
            break

    # 没有一场满足硬性条件 → 退回"按最优座位质量排行选最好"兜底
    if satisfied is None:
        ranked = [ev for ev in evaluated if "reason" not in ev]
        ranked.sort(key=lambda e: (-e["avg_score"], e["time"]))  # 平分时时间早先
        print("\n[range] 无场次落入最佳区域，退回按最优座位质量排行:")
        for i, ev in enumerate(ranked, 1):
            batch = "".join(f"{r}{c}" for r, c in ev["batch"])
            print(f"  #{i} {ev['time']}"
                  + (f" ¥{ev.get('price', '')}" if ev.get('price') else "")
                  + f"  批次 {batch}  均分 {ev['avg_score']:.3f}"
                  + ("  ← 选" if i == 1 else ""))
        if not ranked:
            raise RuntimeError(
                "范围内每场都评不出可售座位。\n" + "\n".join(
                    f"  {ex['time']}: {ex['reason']}" for ex in evaluated))
        target = ranked[0]
        fallback = True
    else:
        target = satisfied
        fallback = False
        print(f"[range] 第一场 {target['time']} 落入最佳区域 → "
              "不再检查后续场次")

    # 4. 只演示：选座并停在选座页，绝不提交订单
    print(f"[demo] 选中场次 {target['time']}，准备选座（不会提交订单）")
    session = {"time": target["time"], "url": target["url"]}
    res = _try_demo_session(
        page, session, args.num_seats, args.force_consecutive,
        row_pref, args.center_col, args.auto_row_preference,
        zone_only=not fallback)
    if res == "stopped":
        print(f"[demo] 完成: 场次={target['time']} 未提交订单")
    else:
        print(f"[demo] 未完成选座: 场次={target['time']} 未提交订单")


def click_showtime(page: Page, show_id: str | None, target_time: str | None,
                   target_date: str | None = None, movie_id: str | None = None):
    """点开场次。

    两种方式（按顺序尝试）：
    1. 直读 row 内的 xseats URL，page.goto 直接进 — 跳过 modal dance
    2. 走老路径：click row 的 buy-btn → modal → "直接选座购票"

    Args:
      target_time: '19:30'
      target_date: '2026-08-25' / '今天' / ...
      movie_id: 过滤 movieId=...
    """
    print(f"[nav] click showtime (show_id={show_id}, time={target_time}, "
          f"date={target_date}, movie_id={movie_id})")

    # 0) 把友好日期转成 YYYY-MM-DD
    target_date_norm = _normalize_date(target_date)
    # 关闭首次进 cinema 页可能残留的 modal
    close_app_promo_modal(page)

    target = target_time or show_id or ""

    # 0.5) 指定了日期就点 date tab — 否则 find_xseats 只看到今天的场次
    if target_date_norm:
        _click_date_tab(page, target_date_norm)
        # 关键：tab 切换后 cinema 页要重新渲染该日期的 showtime 表，旧表在新数据来前可能还在
        # 等到旧的目标时间行消失 / 新的出现（不依赖网络层）
        time.sleep(2.0)
        close_app_promo_modal(page)

    # 1) 直读 URL 跳转 — 强行优先于 click+modal
    if target:
        url = _find_xseats_url_for_showtime(page, target, target_date_norm, movie_id)
        if url:
            print(f"[nav] 直跳 xseats URL: {url}")
            page.goto(url, wait_until="domcontentloaded")
            page.wait_for_load_state("networkidle", timeout=15000)
            shot(page, "04_xseats")
            return

    # 1.5) 严格匹配失败 → 尝试 ±60min 内最近场次（用户确认的设计）
    if target and target_date_norm and movie_id:
        near_url, near_info = _find_nearest_xseats_url(
            page, target, target_date_norm, movie_id, tolerance_min=60
        )
        if near_url:
            print(f"[nav] ⚠️ 目标 {target} 没场次，"
                  f"就近跳 xseats URL: {near_url}")
            page.goto(near_url, wait_until="domcontentloaded")
            page.wait_for_load_state("networkidle", timeout=15000)
            shot(page, "04_xseats")
            return
        # near_info 存在说明有候选但都超容差 → 继续到下面报错路径，会列出可用场次

    # 2) 兜底：click 行 + 走 modal — 仅在 find_xseats 真的找不到时执行
    print("[nav] 走 click+modal 兜底路径（find_xseats 没找到）")
    booking_selectors = [
        f"tr:has-text('{target}') [data-act='show-click']",
        f"tr:has-text('{target}') span.buy-btn",
        f"tr:has-text('{target}') [class*='buy-btn']",
        f"tr:has-text('{target}') span:has-text('选座购票')",
        f"li:has-text('{target}') [data-act='show-click']",
        f"li:has-text('{target}') span.buy-btn",
        # 没有 'time' 的全局 fallback — 容易点错电影
        "[data-act='show-click']",
        "span.buy-btn",
        "[class*='buy-btn']",
        "a:has-text('选座购票')",
        "button:has-text('选座购票')",
    ]
    clicked = False
    for sel in booking_selectors:
        # 用 get_by_role / .first 改为遍历查 visible
        loc = page.locator(sel)
        n = loc.count()
        for i in range(n):
            try:
                el = loc.nth(i)
                if not el.is_visible(timeout=300):
                    continue
                # 不在 modal 里
                in_modal = el.evaluate(
                    "el => !!el.closest("
                    "'[class*=\"modal\" i],[class*=\"popup\" i],[role=\"dialog\"]')"
                )
                if in_modal:
                    continue
                el.click(timeout=3000)
                clicked = True
                print(f"[nav] 兜底点击: {sel} (idx={i})")
                break
            except Exception:
                continue
        if clicked:
            break
    if clicked:
        # modal 弹出后点"直接选座购票"
        page.wait_for_load_state("networkidle", timeout=10000)
        time.sleep(1)
        bypass_selectors = [
            "a:has-text('直接选座购票')",
            "button:has-text('直接选座购票')",
            "[class*='popup']:has-text('直接选座购票')",
            "text=直接选座购票",
        ]
        for sel in bypass_selectors:
            try:
                el = page.locator(sel).first
                if el.count() > 0 and el.is_visible(timeout=2000):
                    el.click()
                    print(f"[nav] 绕过 APP 浮窗: {sel}")
                    page.wait_for_load_state("networkidle", timeout=15000)
                    shot(page, "04_xseats")
                    return
            except Exception:
                continue

    if not clicked:
        # 兜底：把页面上所有场次打出来
        available = _dump_available_showtimes(page)
        shot(page, "99_showtime_not_found")
        # 区分 _xseats 旁路 / _skip / 正常行
        rows = [a for a in available if 'time' in a]
        xseats_links = [a for a in available if a.get('_xseats')]
        skipped = [a for a in available if a.get('_skip')]

        avail_str = ""
        # 优先用 _find_nearest 已经整理过的 available_times（更精炼）
        try:
            _ni = near_info  # 可能未定义（target_date_norm 或 movie_id 为空时跳过 nearest）
        except NameError:
            _ni = None
        if _ni and _ni.get('available_times'):
            avail_str += (
                f"[find-nearest] ±{_ni.get('tolerance_min', 60)}min 容差内无场次。"
                f"{target_date_norm or '该日期'} {movie_id or '该电影'} "
                f"实际可用场次:\n  "
                + ", ".join(_ni['available_times'])
                + "\n"
            )
        if rows:
            avail_str += "\n[正常行]\n" + "\n".join(
                f"  [{a.get('btnText','?')}] time={a.get('time','')} "
                f"price=¥{a.get('price','')} href={a.get('href','')[:80]}\n"
                f"    text: {a.get('text','')[:120]}"
                for a in rows
            )
        if xseats_links:
            avail_str += "\n\n[xseats 直链]\n" + "\n".join(
                f"  {a['href']} (text='{a.get('text','')}')"
                for a in xseats_links
            )
        if skipped:
            avail_str += "\n\n[被跳过的按钮]\n" + "\n".join(
                f"  [{a.get('_skip')}] {a.get('text','')} <{a.get('tag','')}>"
                for a in skipped[:10]
            )
        if not avail_str:
            avail_str = "  (没扫到任何东西 — 看 .cache/page.html + 截图)"

        raise RuntimeError(
            f"找不到场次 time={target_time!r} date={target_date!r} "
            f"movie_id={movie_id!r}。\n{avail_str}\n"
            f"截图: {SCREENSHOT_DIR}/99_showtime_not_found.png\n"
            f"HTML: /Users/kuicao/Applications/maoyan-booker/.cache/page.html"
        )

    # 2) 等"扫码下载APP"浮窗出现
    page.wait_for_load_state("networkidle", timeout=10000)
    time.sleep(1)  # 浮窗动画
    shot(page, "03_popup")

    # 3) 点浮窗里的"直接选座购票"链接绕过 APP 推荐
    bypass_selectors = [
        "a:has-text('直接选座购票')",
        "button:has-text('直接选座购票')",
        "[class*='popup']:has-text('直接选座购票')",
        "text=直接选座购票",
    ]
    bypassed = False
    for sel in bypass_selectors:
        loc = page.locator(sel).first
        if loc.count() > 0 and loc.is_visible(timeout=2000):
            loc.click()
            bypassed = True
            print(f"[nav] 绕过 APP 浮窗: {sel}")
            break
    if not bypassed:
        print("[!] 没找到'直接选座购票'链接，可能页面已直达 xseats 或弹窗不同")
    page.wait_for_load_state("networkidle", timeout=15000)
    shot(page, "04_xseats")


def wait_seat_map(page: Page):
    """等选座图加载。猫眼选座图通常是 canvas 或 SVG，监听特定 class 出现。"""
    print("[wait] 选座图加载…")
    # 占位选择器 — 实际 class 需要看 DOM
    seat_selectors = [
        ".seat-map",
        "[class*='seatMap']",
        "[class*='seat-map']",
        ".seats-wrapper",
        "canvas",  # 兜底：canvas 出现就当作加载完
    ]
    deadline = time.time() + 20
    while time.time() < deadline:
        for sel in seat_selectors:
            if page.locator(sel).first.is_visible(timeout=200):
                print(f"[wait] 选座图就绪: {sel}")
                # 选座图就位 → 关掉"您选的是 X 场次"提示 modal
                # （不每次都弹，但触发了不点掉会拦住 .seat.selectable click）
                close_xseats_notice_modal(page)
                return
        time.sleep(0.5)
    print("[!] 20s 内未检测到选座图，继续往下走（可能选择器失效）")
    shot(page, "03b_seat_map_missing")


def select_seats(page: Page, seats_spec: list[str] | None, auto: str,
                 num_seats: int = 2, force_consecutive: bool = True,
                 row_preference: list[str] | None = None,
                 center_col: float | None = None,
                 auto_row_preference: bool = True):
    """选座位。

    seats_spec 非空：每项形如 "A排6座" 或 "A6"，按用户指定选。
    seats_spec 为空：按 auto 策略自动挑。

    auto_row_preference=True 时（默认），从 DOM 检测排数 + 自动算行偏好，
    忽略传入的 row_preference。传 False 时用 row_preference 字面值。

    DOM 知识（实测）：
    - 可售:  <span class="seat selectable" data-row-id="A" data-column-id="6" data-st="N">
    - 已售:  <span class="seat sold"       data-row-id="A" data-column-id="6" data-st="LK">
    - 已选:  <span class="seat selected"   ...>  (点击后由 JS 加上)
    """
    # 自动行偏好：检测后排数 + 动态计算（推荐，适配任意影厅）
    row_labels: list[str] = []
    if auto_row_preference and row_preference is None:
        row_labels = detect_row_labels(page)
        if row_labels:
            row_preference = compute_row_preference(row_labels)
            print(f"[seat] 自动检测 {len(row_labels)} 排 ({row_labels})，"
                  f"偏好: {row_preference}")
    if seats_spec:
        picked = []
        first_row = None
        for spec in seats_spec:
            # 解析 "A排6座" / "A6" / "a排6座" / "6排4座"（数字行号也常见）
            import re
            m = re.match(r"([^\s排座]+)\s*排?\s*(\d+)\s*座?", spec)
            if not m:
                raise ValueError(
                    f"座位格式应为 'X排Y座'（X 是行 ID，如 A / 6；Y 是数字），"
                    f"收到: {spec!r}")
            row, col = m.group(1).upper(), m.group(2)
            if first_row is None:
                first_row = row
            sel = f".seat.selectable[data-row-id='{row}'][data-column-id='{col}']"
            loc = page.locator(sel).first
            if loc.count() == 0:
                # 调试：找最近的可售座位
                nearby = page.locator(
                    f".seat.selectable[data-row-id='{row}']").all()
                cols = [s.get_attribute("data-column-id") for s in nearby[:5]]
                raise RuntimeError(
                    f"座位 {spec} 找不到或不可售。"
                    f"行 {row} 的可售座位列号样例: {cols}")
            loc.click()
            picked.append(f"{row}{col}")
            time.sleep(0.2)
        # 这个分支被两条路径调用:
        # 1. CLI --seat 指定 → 真正的"手动选"
        # 2. 多场次演示的 _try_demo_session 把 zone_batch 当 spec 传入 → 算法选好的"选中"
        # 区分:CLI 调用时 auto_seat="manual" 且 spec 是用户 CLI 拼的字符串;演示 zone 调用时
        # auto_seat 也是 "manual"。这里无法精确区分，但两种情况 stdout 都带
        # level(从 row 算)，通知/面板里就显示档位而不是"手选(无档位)"。
        level = _classify_pick_level(first_row or "", row_labels or [])
        print(f"[seat] 选中: {picked} | level={level}")
        shot(page, "04_seats_picked")
    elif auto in ("middle", "first", "best"):
        # 旧版快速策略 — 向后兼容
        n = auto_n_for(auto)
        all_seats = page.locator(".seat.selectable").all()
        # 提取行/列信息
        seats = []
        for s in all_seats:
            row = s.get_attribute("data-row-id") or ""
            col = int(s.get_attribute("data-column-id") or "0")
            seats.append((row, col, s))
        if not seats:
            raise RuntimeError("找不到任何可售座位（'seat.selectable' 选择器失效？）")

        if auto == "middle":
            # 按行号排序，挑中间行
            unique_rows = sorted({r for r, _, _ in seats},
                                 key=row_sort_key)
            mid_row = unique_rows[len(unique_rows) // 2]
            row_seats = sorted(
                [(c, s) for r, c, s in seats if r == mid_row],
                key=lambda x: x[0])
            # 从中间偏后开始取 n 个连续座位
            start = max(0, len(row_seats) // 2 - n // 2)
            chosen = row_seats[start:start + n]
            if not chosen:
                chosen = row_seats[:n]  # 行座位不够就取头
            picked_specs = [f"{mid_row}{c}" for c, _ in chosen]
            for _, s in chosen:
                s.click()
                time.sleep(0.2)
        else:
            # first / best: 按行+列排序取前 n 个
            seats.sort(key=lambda x: (row_sort_key(x[0]), x[1]))
            chosen = seats[:n]
            picked_specs = [f"{r}{c}" for r, c, _ in chosen]
            for _, _, s in chosen:
                s.click()
                time.sleep(0.2)
        print(f"[seat] 自动选中 ({auto}): {picked_specs}")
        shot(page, "04_seats_picked")
    elif auto in ("middle", "first", "best"):
        # 旧版快速策略 — 向后兼容（保留是因为 CLI 默认 --auto-seat middle）
        n = auto_n_for(auto)
        all_seats = page.locator(".seat.selectable").all()
        seats = []
        for s in all_seats:
            row = s.get_attribute("data-row-id") or ""
            col = int(s.get_attribute("data-column-id") or "0")
            seats.append((row, col, s))
        if not seats:
            raise RuntimeError("找不到任何可售座位（'.seat.selectable' 选择器失效？）")
        if auto == "middle":
            unique_rows = sorted({r for r, _, _ in seats}, key=row_sort_key)
            mid_row = unique_rows[len(unique_rows) // 2]
            row_seats = sorted(
                [(c, s) for r, c, s in seats if r == mid_row],
                key=lambda x: x[0])
            start = max(0, len(row_seats) // 2 - n // 2)
            chosen = row_seats[start:start + n] or row_seats[:n]
        else:
            seats.sort(key=lambda x: (row_sort_key(x[0]), x[1]))
            chosen = seats[:n]
        for _, _, s in chosen:
            s.click()
            time.sleep(0.2)
        print(f"[seat] 旧版策略 ({auto}) 选中: "
              f"{[f'{r}{c}' for r, c, _ in chosen]}")
        shot(page, "04_seats_picked")
    else:
        # 新版智能选座算法（推荐）
        picks = select_best_seats(page, num_seats, force_consecutive,
                                  row_preference, center_col)
        for s in picks:
            # 防御：click 之前再扫一次（modal 可能在这时才弹）
            close_xseats_notice_modal(page)
            try:
                s.locator.click(timeout=5000)
            except Exception as click_err:
                # modal 可能在 click 中弹出 → 关掉再试一次
                err_str = str(click_err).lower()
                if "intercept" in err_str or "modal" in err_str or "obscures" in err_str:
                    print(f"[seat] click 被 modal 拦截，关闭后重试 {s.row}{s.col}")
                    close_xseats_notice_modal(page)
                    s.locator.click(timeout=5000)
                else:
                    raise
            time.sleep(0.2)
        shot(page, "04_seats_picked")


def auto_n_for(strategy: str) -> int:
    return {"first": 2, "middle": 2, "best": 4}.get(strategy, 2)


def select_best_seats(page: Page, n_seats: int, force_consecutive: bool,
                      row_preference: list[str], center_col: float | None) -> list[Seat]:
    """主选座函数。

    Args:
      n_seats: 票数（1-6）
      force_consecutive: True=N 个必须相邻；False=各自挑最好（不保证相邻）
      row_preference: 行偏好列表，如 ["G","H","F",...]
      center_col: 中心列；None 时自动检测

    Returns: 选中的 Seat 列表（按 row, col 排序）

    连坐模式的选座优先级（用户 2026-08-28 定义）：
    1. 最佳排（row_preference[0]）的"中间块"（以该行中线为锚点的连续 n_seats 个）
    2. 第 2 名偏好排的中间块
    3. 第 3 名偏好排的中间块
    4. 最佳区域（前 3 排 × 全局中心列 ±3）内 _select_consecutive 的最优批次
    5. 全局 _select_consecutive 兜底（按行偏好顺序滑窗找最佳连坐）

    非连坐模式：直接按评分取 top N（不在乎是否相邻，各自挑最好）。

    通知格式（用户 2026-08-31 要求）：
    最终 stdout 输出形如 `[seat] 选中: ['G14','G15'] | level=2 | 过程: G正中✗ → H正中✗ → F正中✓`
    level 取 trail 最后一条（即最终命中的档位）。非连坐模式 level 字段为 'multi'。
    """
    pref_rank = {r.strip(): i for i, r in enumerate(row_preference)}
    n_rows = len(pref_rank)

    # 1. 抓可售座位
    seats = collect_seats(page)
    if not seats:
        raise RuntimeError("找不到任何可售座位（'.seat.selectable' 选择器失效？）")
    if n_seats > 6:
        raise ValueError(f"单笔最多 6 张票，收到 --num-seats {n_seats}")
    if n_seats < 1:
        raise ValueError(f"--num-seats 必须 ≥ 1")

    # 2. 中心列（仅用于评分；中间块逻辑按每行自己的中线走，不需要这个 center_col）
    center = detect_center_col(page, center_col)

    # 3. 评分（非连坐模式 / 兜底评分用）
    for s in seats:
        s._score = seat_score(s, center, pref_rank, n_rows)

    # 4. 原始行列表（字母序，用于 _classify_pick_level 计算档位）
    row_labels = detect_row_labels(page)

    print(f"[seat] 可售 {len(seats)} 个 | center_col={center} "
          f"{'(用户指定)' if center_col is not None else '(自动检测)'} | "
          f"n={n_seats} | 强制连坐={force_consecutive}")
    print(f"[seat] 行偏好: {row_preference}")
    top5 = sorted(seats, key=lambda s: s._score, reverse=True)[:5]
    print(f"[seat] Top-5 预览: {[(f'{s.row}{s.col}', round(s._score, 3)) for s in top5]}")

    # 5. 选座 + 记录每一档尝试（用户 2026-08-31 要求的过程可见性）
    trail: list[tuple[int, str, str]] = []
    if force_consecutive:
        # 5.1 新策略：按行偏好顺序，逐行找"以该行中线为锚点"的连续 n_seats
        picks = _select_by_row_middle(seats, n_seats, row_preference, row_labels, trail)
        if picks:
            mid_rows = sorted({s.row for s in picks})
            mids = []
            for r in mid_rows:
                row_block = [s for s in picks if s.row == r]
                if row_block:
                    mids.append(
                        f"{r}{row_block[0].col}-{r}{row_block[-1].col}"
                    )
            print(f"[seat] 中间块命中: {mids}（行 {mid_rows} 的中线两侧）")
        else:
            # 5.2 兜底：最佳区域内 _select_consecutive
            print(f"[seat] 偏好前 3 排均无完整中间块，退回最佳区域策略")
            picks = _best_batch_in_zone(seats, n_seats, force_consecutive,
                                        row_preference, center, trail)
            if not picks:
                # 5.3 兜底兜底：全局 _select_consecutive（按行偏好滑窗）
                print(f"[seat] 最佳区域也无解，退回全局连坐搜索")
                picks = _select_consecutive(seats, n_seats, row_preference,
                                             row_labels, trail)
    else:
        # 非连坐：评分取 top N，无需中间块逻辑
        picks = _select_individual(seats, n_seats, row_labels, trail)

    if not picks:
        mode = "N 个连续" if force_consecutive else f"{n_seats} 个独立"
        raise RuntimeError(
            f"找不到{mode}可售座位。"
            f"试试减小 --num-seats，或去掉 --force-consecutive")

    picks.sort(key=lambda s: (s.row, s.col))
    avg_score = sum(s._score for s in picks) / len(picks)

    # 6. 新格式输出:level + 过程（给 monitor.rs 的正则用，格式固定）
    if force_consecutive and trail:
        trail_str = " → ".join(
            f"{label}{'✓' if '命中' in info else '✗'}"
            for _, label, info in trail
        )
        final_level = trail[-1][0] if trail else 0
        print(f"[seat] 选中: {[f'{s.row}{s.col}' for s in picks]} "
              f"(平均分 {avg_score:.3f}) | level={final_level} | 过程: {trail_str}")
    elif not force_consecutive and trail:
        # 非连坐：每个独立座位记一行；level 字段写 multi 表示混合档
        trail_str = " → ".join(
            f"{label}{'✓' if '命中' in info else '✗'}"
            for _, label, info in trail
        )
        print(f"[seat] 选中: {[f'{s.row}{s.col}' for s in picks]} "
              f"(平均分 {avg_score:.3f}) | level=multi | 过程: {trail_str}")
    else:
        print(f"[seat] 选中: {[f'{s.row}{s.col}' for s in picks]} "
              f"(平均分 {avg_score:.3f})")
    return picks


def _select_consecutive(seats: list[Seat], n_seats: int,
                        row_preference: list[str],
                        row_labels: list[str] | None = None,
                        trail: list[tuple[int, str, str]] | None = None) -> list[Seat]:
    """连坐模式：按行偏好顺序遍历每行，滑动窗口找 N 个物理相邻的最佳组合。

    输入的 seats 只有可售座位，滑动窗口直接滑会跳过中间已售的列——所以窗口内
    必须校验列号严格连续（window[j+1].col == window[j].col + 1），否则两个座位
    中间隔着已售座位，实际并不挨着（实测坑：锁到两个人不在一起）。

    算法：O(rows × cols) 滑动窗口；窗口得分 = 窗口中心座位的评分。
    选最佳窗口（按行偏好顺序，行越靠前越优先，平分时按窗口中心位置更靠前的窗口胜）。

    trail: 可选,winning row 命中时追加 (level, row, info)。winning row 此时必
    不在 row_preference[:3] 内,所以 level 必为 4 或 5（靠后/靠前）。
    """
    by_row = defaultdict(list)
    for s in seats:
        by_row[s.row].append(s)
    for r in by_row:
        by_row[r].sort(key=lambda x: x.col)

    best_window = None
    best_score = -1.0
    best_row_idx = len(row_preference) + 1  # 用于平分时偏好的行胜出

    for row_idx, row_letter in enumerate(row_preference):
        row_letter = row_letter.strip()
        row_seats = by_row.get(row_letter, [])
        if len(row_seats) < n_seats:
            continue
        # 滑动窗口（校验列号物理连续：中间不能有已售跳号）
        for i in range(len(row_seats) - n_seats + 1):
            window = row_seats[i:i + n_seats]
            if any(window[j + 1].col != window[j].col + 1
                   for j in range(n_seats - 1)):
                continue
            center = window[n_seats // 2] if n_seats > 1 else window[0]
            score = center._score
            # 平分时：行偏好靠前胜；再次平分：窗口更靠前（i 更小）胜
            if (score > best_score or
                    (score == best_score and row_idx < best_row_idx)):
                best_score = score
                best_row_idx = row_idx
                best_window = window
    picks = best_window or []
    if trail is not None and picks:
        row = picks[0].row
        level = _classify_pick_level(row, row_labels or row_preference)
        info = f"命中 {row}{picks[0].col}-{row}{picks[-1].col}"
        trail.append((level, row, info))
    return picks


def _select_individual(seats: list[Seat], n_seats: int,
                        row_labels: list[str] | None = None,
                        trail: list[tuple[int, str, str]] | None = None) -> list[Seat]:
    """非连坐模式：按评分排序，取 top N（可能不挨着）。

    trail: 可选,top N 每个独立座位按 level 记录。trail[-1] 取最低档位（最差那张）。
    多档混合时,通知里 level 字段会用 'multi' 表示。
    """
    picks = sorted(seats, key=lambda s: s._score, reverse=True)[:n_seats]
    if trail is not None and picks:
        # 非连坐:每个独立座位记一条,最终 level 取最差档（数字最大）
        rows = sorted({s.row for s in picks})
        for r in rows:
            row_seats = [s for s in picks if s.row == r]
            level = _classify_pick_level(r, row_labels or rows)
            info = f"命中 {','.join(f'{s.row}{s.col}' for s in row_seats)}"
            trail.append((level, f"{r}独立", info))
    return picks


def dump_seat_dom(page: Page):
    """debug mode：打印座位图区域的 DOM 结构，找可售/已售/已选 selector。

    打印：
    - xseats 页面整体结构（主要容器 class/id）
    - 行标识 (A/B/C...) 的元素
    - 几类座位的 HTML 片段（可售/已售/已选各一个）
    - "确认选座"按钮 selector
    """
    print()
    print("=" * 60)
    print("  DEBUG: 打印选座页 DOM 结构")
    print("=" * 60)

    # 1. 顶层容器
    containers = page.evaluate("""() => {
        const out = [];
        const els = document.body.querySelectorAll('*');
        // 找几个典型容器
        ['seat-map', 'seats', 'hall', 'cinema-hall', 'xseats', 'screen'].forEach(kw => {
            document.querySelectorAll(`[class*="${kw}"], [id*="${kw}"]`).forEach(el => {
                out.push({
                    tag: el.tagName.toLowerCase(),
                    cls: String(el.className || '').slice(0, 100),
                    id: el.id || '',
                    childCount: el.children.length,
                });
            });
        });
        return out.slice(0, 30);
    }""")
    print("\n[1. 主要容器（class/id 含 seat/seats/hall/xseats/screen）]")
    for c in containers:
        print(f"  <{c['tag']}> class='{c['cls']}' id='{c['id']}' ({c['childCount']} children)")

    # 2. 行标识元素
    rows = page.evaluate("""() => {
        const out = [];
        document.querySelectorAll('*').forEach(el => {
            const t = (el.innerText || '').trim();
            if (/^[A-K]$/.test(t) && el.children.length === 0) {
                out.push({
                    tag: el.tagName.toLowerCase(),
                    cls: String(el.className || '').slice(0, 80),
                    text: t,
                });
            }
        });
        return out.slice(0, 20);
    }""")
    print("\n[2. 行标识元素（单字母 A-K）]")
    for r in rows:
        print(f"  <{r['tag']}> class='{r['cls']}' text='{r['text']}'")

    # 3. 座位元素 — 抓几个有代表性的（按可视状态分类）
    seats = page.evaluate("""() => {
        const out = {available: [], sold: [], selected: [], unknown: []};
        // 常见的座位元素 class 模式
        const seatSelectors = [
            '[class*="seat"]', 'li[class*="seat"]', 'div[class*="seat"]',
            'a[class*="seat"]', 'svg[class*="seat"]', '[data-seat-id]',
        ];
        const seen = new Set();
        for (const sel of seatSelectors) {
            document.querySelectorAll(sel).forEach(el => {
                const key = el.outerHTML.slice(0, 200);
                if (seen.has(key)) return;
                seen.add(key);
                const cls = String(el.className || '').toLowerCase();
                const ds = el.dataset || {};
                const status = ds.status || ds.state || '';
                let cat = 'unknown';
                if (cls.match(/available|free|selectable|可选|isok|can/)) cat = 'available';
                else if (cls.match(/sold|unavailable|disabled|已售|nosale/)) cat = 'sold';
                else if (cls.match(/selected|active|checked|已选|chosen/)) cat = 'selected';
                else if (status) cat = status;
                if (out[cat].length < 3) {
                    out[cat].push({
                        tag: el.tagName.toLowerCase(),
                        cls: cls.slice(0, 100),
                        id: el.id || '',
                        outerHtml: el.outerHTML.slice(0, 250),
                    });
                }
            });
        }
        return out;
    }""")
    for cat in ['available', 'sold', 'selected', 'unknown']:
        items = seats.get(cat, [])
        print(f"\n[3.{cat}] {len(items)} 个示例")
        for s in items:
            print(f"  <{s['tag']}> class='{s['cls']}' id='{s['id']}'")
            print(f"    {s['outerHtml'][:200]}")

    # 4. 全页面截图
    page.screenshot(path=str(SCREENSHOT_DIR / "debug_seats.png"), full_page=True)
    print(f"\n[4. 全页面截图] {SCREENSHOT_DIR}/debug_seats.png")
    print("\n把上面这些信息贴给我，我就能写准确的 selector 了。")


def stop_at_seat_map(page: Page, session: dict | None = None):
    """Demo stop point: seats are selected; never submit an order.

    Also installs a small DOM guard that neutralises any element whose visible
    text is "确认选座", so a human cannot accidentally click through while the
    browser is left open.
    """
    guard = """() => {
        const nodes = document.querySelectorAll('button, a, div, span');
        nodes.forEach((el) => {
            const text = (el.innerText || '').trim();
            if (text === '确认选座' || text.startsWith('确认选座')) {
                el.style.pointerEvents = 'none';
                el.style.opacity = '0.45';
                el.addEventListener('click', (e) => {
                    e.preventDefault();
                    e.stopPropagation();
                }, true);
            }
        });
        return true;
    }"""
    try:
        page.evaluate(guard)
    except Exception as e:
        print(f"[demo] 防误点保护安装失败: {e!r}")

    when = session.get("time", "") if session else ""
    print()
    print("=" * 60)
    print("  [demo] 已选好座位，停在选座页。")
    print("  [demo] 本脚本不会点击确认，也不会提交订单。")
    if when:
        print(f"  [demo] 场次: {when}")
    print(f"  截图见: {SCREENSHOT_DIR}/04_seats_picked.png")
    print("=" * 60)


# ----------------- CLI -----------------

def main():
    if any(arg == "--" + "confirm" for arg in sys.argv[1:]):
        print("[demo] 此脚本只演示选座，禁止提交订单参数", file=sys.stderr)
        sys.exit(2)
    p = argparse.ArgumentParser(
        description="Gofer 猫眼选座演示（只展示，不提交订单）",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
示例:
  # 演示：扫描该影院今天的全部电影/场次，选最优座位后停在选座页
  python gofer-seat-demo.py --cinema-id 37534 \\
      --show-time-range "00:00-23:59" --num-seats 2 --auto-seat manual

  # 指定电影
  python gofer-seat-demo.py --cinema-id 37534 --movie-id 1545360 \\
      --show-time-range "00:00-23:59" --num-seats 2

本脚本没有任何提交订单的参数或代码路径。
        """,
    )
    p.add_argument("--headless", action="store_true", help="无头模式（首次登录必须 headed）")
    p.add_argument("--cinema-id", required=True, help="影院 ID（来自 URL /films/cinema/{id}）")
    p.add_argument("--movie-id", help="电影 ID")
    p.add_argument("--show-id", help="场次 ID（如果知道）")
    p.add_argument("--show-time", help="场次时间字符串，如 '19:30'，用于匹配按钮（与 --show-time-range 互斥）")
    p.add_argument("--show-time-range",
                   help="场次时间范围 'HH:MM-HH:MM'，如 '15:00-19:00'（当日）。"
                        "范围内每场加载选座图评估最优座位，选出最好的那场后停在选座页。"
                        "与 --show-time / --show-id 互斥")
    p.add_argument("--imax-only", action="store_true",
                   help="（配 --show-time-range）只订 IMAX 厅：范围内非 IMAX 场次"
                        "直接跳过不用评估最佳座位；没有 IMAX 场次则报错。"
                        "仅范围模式有效")
    p.add_argument("--show-date", default=None,
                   help="场次日期 'YYYY-MM-DD' 或 '今天'/'明天'/'后天'。"
                        "限定 --show-time 只在该日期内匹配（避免同日同时段跨天误匹配）")
    p.add_argument("--xseats-url",
                   help="直接跳到选座页 URL（跳过场次列表点击）。"
                        "形如 https://www.maoyan.com/xseats/{show_id}?movieId=...&cinemaId=...")
    p.add_argument("--debug-dom", action="store_true",
                   help="debug 模式：进入选座页后打印座位元素 DOM 详情")
    p.add_argument("--seat", action="append", help="座位 'X排Y座'（X 是字母 A-K，Y 是数字 1-36），可多次指定")
    p.add_argument("--auto-seat", choices=["first", "middle", "best", "manual"], default="manual",
                   help="自动选座策略：first/middle/best=旧版快速；manual=用新版智能算法（推荐）")
    p.add_argument("--num-seats", type=int, default=2,
                   help="票数（1-6），新版算法下生效")
    p.add_argument("--force-consecutive", action="store_true", default=True,
                   help="新版算法强制 N 张票相邻（默认开）。去 --no-force-consecutive 关掉")
    p.add_argument("--no-force-consecutive", dest="force_consecutive",
                   action="store_false",
                   help="新版算法下允许 N 张票不挨着（各自挑最好）")
    p.add_argument("--row-preference",
                   default=None,
                   help=f"行偏好字符串，从优到劣用逗号分隔。"
                        f"默认 None（自动按影厅排数计算）。"
                        f"硬编码示例: {DEFAULT_ROW_PREFERENCE}")
    p.add_argument("--auto-row-preference", dest="auto_row_preference",
                   action="store_true", default=True,
                   help="按 DOM 实际排数动态算行偏好（默认开）。"
                        "传 --no-auto-row-preference 关掉，用 --row-preference 字面值")
    p.add_argument("--no-auto-row-preference", dest="auto_row_preference",
                   action="store_false",
                   help="用 --row-preference 字面值，不自动算")
    p.add_argument("--center-col", type=float, default=None,
                   help="中心列号（默认从 DOM 自动检测。不同影院/影厅中心不同）")
    p.add_argument("--user-data-dir", default=str(USER_DATA_DIR),
                   help="Playwright persistent context 路径（cookie 持久化）")
    p.add_argument("--screenshot-dir", default=str(DEFAULT_SCREENSHOT_DIR),
                   help="截图目录（latest.png 会原子更新，供 Gofer 面板展示）")
    args = p.parse_args()

    if args.show_time_range and args.show_time:
        p.error("--show-time-range 与 --show-time 互斥，只能二选一")
    if args.show_time_range and args.show_id:
        p.error("--show-time-range 与 --show-id 互斥，只能二选一")
    if args.show_time_range and args.xseats_url:
        p.error("--show-time-range 与 --xseats-url 互斥，只能二选一")
    if args.imax_only and not args.show_time_range:
        p.error("--imax-only 只对时间范围模式（--show-time-range）生效")

    global SCREENSHOT_DIR
    SCREENSHOT_DIR = Path(args.screenshot_dir)
    SCREENSHOT_DIR.mkdir(parents=True, exist_ok=True)
    print(f"[cfg] user_data_dir = {args.user_data_dir}")
    print(f"[cfg] headless      = {args.headless}")
    print(f"[cfg] screenshots   = {SCREENSHOT_DIR}")
    print("[demo] 只演示选座，不会提交订单")

    with sync_playwright() as p:
        # 关键：launch_persistent_context 让 cookie / session 持久化
        context: BrowserContext = p.chromium.launch_persistent_context(
            user_data_dir=args.user_data_dir,
            headless=args.headless,
            viewport={"width": 1280, "height": 800},
            locale="zh-CN",
        )
        # persistent context 默认有一个 page
        page = context.pages[0] if context.pages else context.new_page()

        try:
            # 1. 登录态检查 — 等到 networkidle 再判断，避免 JS 还没渲染就检查
            page.goto(MAOYAN_BASE, wait_until="domcontentloaded")
            page.wait_for_load_state("networkidle", timeout=10000)
            if not is_logged_in(page):
                if args.headless:
                    print("[!] 未登录且无头模式无法扫码。请去掉 --headless 再跑一次。")
                    sys.exit(2)
                wait_for_login(page)

            # 1.5 直接 xseats 路径：跳过场次列表点击
            if args.xseats_url:
                print(f"[nav] 直接进入选座页: {args.xseats_url}")
                page.goto(args.xseats_url, wait_until="domcontentloaded")
                page.wait_for_load_state("networkidle", timeout=15000)
                shot(page, "02_xseats")
            else:
                # 2. 导航到目标影院
                navigate_to_cinema(page, args.cinema_id)

                # 3. 仅探索模式：没指定 movie/show/range 就优雅退出
                if not (args.movie_id or args.show_id or args.show_time
                        or args.show_time_range):
                    print()
                    print("=" * 60)
                    print("  探索模式：未指定 --movie-id / --show-time / --show-time-range，停在影院页。")
                    print(f"  看截图: {SCREENSHOT_DIR}/01_cinema.png")
                    print("  下一步：用浏览器手动点开想测试的场次，记下：")
                    print("    - 选座页 URL（用于 --xseats-url 直接跳转）")
                    print("    - 或者场次时间（用于 --show-time 通过列表点击）")
                    print("=" * 60)
                    return

                # 4. 选电影（best-effort — cinema 页可能已经显示该电影的场次，直接 showtime 即可）
                if args.movie_id:
                    try:
                        click_movie(page, args.movie_id)
                    except RuntimeError as e:
                        print(f"[!] click_movie 跳过: {e}")
                        print("[!] cinema 页可能已直接展示该电影的场次列表，"
                              "继续往下走到 click_showtime")

                # 5. 选场次：
                #    范围模式走独立流程（切日期 tab → 枚举 → 评估排行 → 选出
                #    最好的场次并停在选座页），单场模式走原 click_showtime 路径
                if args.show_time_range:
                    run_range_mode(page, args)
                    return
                click_showtime(page, args.show_id, args.show_time,
                               target_date=args.show_date,
                               movie_id=args.movie_id)

            # 6. 等选座图
            wait_seat_map(page)

            # 7. debug mode: 打印座位 DOM 详情（不进选座）
            if args.debug_dom:
                dump_seat_dom(page)
                return

            # 8. 选座位
            row_pref = (
                [r.strip() for r in args.row_preference.split(",") if r.strip()]
                if args.row_preference else None
            )
            select_seats(page, args.seat, args.auto_seat,
                         num_seats=args.num_seats,
                         force_consecutive=args.force_consecutive,
                         row_preference=row_pref,
                         center_col=args.center_col,
                         auto_row_preference=args.auto_row_preference)

            # 9. Demo stop point: never submit an order.
            stop_at_seat_map(page, session={
                "time": args.show_time or "",
                "url": args.xseats_url or "",
            })
            print("[demo] 完成: 未提交订单")
        except Exception as e:
            # 之前 finally 吞异常，用户看到 '流程结束' 不知道为啥。
            # 现在打出来 + 截图当前状态，最后再走 finally 让用户关浏览器。
            print()
            print("=" * 60)
            print(f"  [ERROR] 流程异常: {type(e).__name__}")
            print(f"  {e}")
            print("=" * 60)
            traceback.print_exc()
            try:
                shot(page, "99_error")
            except Exception:
                pass
            print(f"\n  当前状态截图: {SCREENSHOT_DIR}/99_error.png")
            print("  截图里能看到 DOM 结构 / selector 是否对得上。")
        finally:
            # 保持浏览器打开，方便用户继续在浏览器里手动操作（看截图/试选座）
            # 外层 sync_playwright() 退出时会关浏览器，所以这里阻塞住等用户决定
            print()
            print("=" * 60)
            print(f"  流程结束。浏览器保持打开。")
            print(f"  截图在: {SCREENSHOT_DIR}/")
            print(f"  在浏览器里手动操作完后回到这里按 回车 关闭浏览器。")
            print(f"  或按 Ctrl+C 强制退出。")
            print("=" * 60)
            try:
                input(">>> 按回车关闭浏览器: ")
            except EOFError:
                # stdin 不可用（pipe / Claude Code / ticket-tracker 锁票驱动等）：
                # 选好的座位结果已经由 stdout marker（`[seat] 手动选中:` / `[seat] 选中:`）
                # 实时 flush 出去了，外部立刻出通知，无需再留浏览器，直接收尾。
                pass
            context.close()


# ----------------- TixDash 集成入口（医生 / 登录向导复用） -----------------

def check_login_state(user_data_dir=None, timeout_sec=15):
    """无头快速检查猫眼登录态。

    返回 (logged_in: bool, note: str)。给 doctors 自检和 UI 状态用。
    登录态判断走 cookie（is_logged_in），与实际页面刷新的下一次请求无关。
    """
    try:
        with sync_playwright() as p:
            udd = user_data_dir or str(USER_DATA_DIR)
            try:
                context = p.chromium.launch_persistent_context(
                    user_data_dir=udd, headless=True, locale="zh-CN")
            except Exception as e:
                return False, "浏览器启动失败: {}".format(e)
            page = context.pages[0] if context.pages else context.new_page()
            try:
                page.goto(MAOYAN_BASE, wait_until="domcontentloaded")
                page.wait_for_load_state("networkidle", timeout=timeout_sec * 1000)
                ok = is_logged_in(page)
                return ok, ("已登录 ✓" if ok else "未登录（需要扫码一次）")
            except Exception as e:
                return False, "打开猫眼页面失败: {}".format(e)
            finally:
                context.close()
    except Exception as e:
        return False, "Playwright 不可用: {}".format(e)


def run_login_wizard(timeout_sec=300, log=print):
    """引导用户扫码登录猫眼（有头浏览器，首次用）。

    打开 maoyan 首页 → 未登录则提示扫码 → 自动轮询检测 → 成功后返回 "ok"。
    返回 "ok" 或 "err:<原因>"。登录态持久化在 ~/.maoyan-booker-profile/。
    log: 日志回调（TUI 场景传 UI 日志函数，避免 print 污染界面）。
    """
    try:
        with sync_playwright() as p:
            context = p.chromium.launch_persistent_context(
                user_data_dir=str(USER_DATA_DIR),
                headless=False,
                viewport={"width": 1280, "height": 800},
                locale="zh-CN",
            )
            page = context.pages[0] if context.pages else context.new_page()
            try:
                log("")
                log("=" * 60)
                log("  🖥 正在打开浏览器，请在弹出的窗口中登录猫眼（扫码或账号）")
                log("=" * 60)
                page.goto(MAOYAN_BASE, wait_until="domcontentloaded")
                page.wait_for_load_state("networkidle", timeout=10000)
                if is_logged_in(page):
                    log("[ok] 已登录")
                    return "ok"
                wait_for_login(page, timeout_sec, log=log)
                log("[ok] 登录完成，登录态已保存到 " + str(USER_DATA_DIR))
                return "ok"
            except RuntimeError as e:
                return "err:" + str(e)
            except Exception as e:
                return "err:" + (traceback.format_exc()[-500:] if str(e) == "" else str(e))
            finally:
                context.close()
    except Exception as e:
        return "err:" + str(e)


if __name__ == "__main__":
    main()