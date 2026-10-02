#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 DemonPlayer
"""opm 谱面校验器 v0.1 —— 仅标准库，无第三方依赖。

实现规范 spec/opm-format.md 第 8 节的规范性约束。

用法:
    python3 spec/check.py <file.opm.json> [...]
    python3 spec/check.py --compact <file.opm.json>     # 只打印未通过的文件名

退出码:
    0 = 无错误（可能含警告）
    1 = 存在错误
    2 = 用法错误 / 文件读取或 JSON 解析失败

**与 app 里那份校验器（`app/src/cmd.rs` 的 `validate`）是两份独立实现，两者应当在同一份文件上
给出相同结论** —— 这句话一直写在 `cmd.rs` 的注释里，但 2026-10-03 之前没有任何东西在守它，
于是真的分家了：本文件按**每条判定线自己的音符**算"谱面末尾"，而 Rust 用 `Document::chart_end`
（全文档一个数），"A 线铺到末尾、B 线只铺到自己音符"这种文件 Rust 报错、本文件放行。

`spec/examples/` 里的样例就是用来手工核对这一点的（`two-lines.opm.json` 正是上面那个反例；
`bad.opm.json` 故意一堆错、`minimal`/`mask` 必须全过）:

    python3 spec/check.py spec/examples/*.json
    for f in spec/examples/*.json; do opm-ctl --file "$f" validate; done

逐份比对**指针与措辞**，不是只看退出码 —— 两边都说"错"但指到不同地方，等于没对齐。
"""

from __future__ import annotations

import json
import sys
from fractions import Fraction
from pathlib import Path

HERE = Path(__file__).resolve().parent

KINDS = {"tap", "hold", "drag", "flick"}
SIDES = {"above", "below"}
DIFFICULTIES = {"EZ", "HD", "IN", "AT", "SP", "Legacy"}
ATTACH_UI = {"pause", "combonumber", "combo", "score", "bar", "name", "level"}
TRACKS = ("moveX", "moveY", "rotate", "alpha", "speed")
EXT_TRACKS = ("color", "scaleX", "scaleY", "text", "gif", "incline")
CTRL_TRACKS = ("pos", "size", "skew", "y", "alpha")
SPEED_TRACK = {"speed"}
RGB_TRACKS = {"color"}
TEXT_TRACKS = {"text"}

X_MAX, Y_MAX = 675.0, 450.0
EVENT_EPOCH = Fraction(0)
NOTE_SOFT_LIMIT = 32768

ROOT_KEYS = {"format", "formatVersion", "minClientCapability", "extensions",
             "meta", "bpmList", "judgeLines", "maskZones", "source", "foreign"}
MASK_TRACKS = ("x1", "y1", "x2", "y2", "x3", "y3", "active")
#: 遮蔽区需要的能力等级（§7 的第 4 档）：不认识它的读取方必须**明确拒绝**
CAP_MASK = 4
META_KEYS = {"name", "composer", "charter", "illustrator", "difficulty", "level",
             "constant", "offsetMs", "audio", "background", "id"}
LINE_KEYS = {"name", "group", "bpmFactor", "zOrder", "isCover", "attachUI", "isGif",
             "texture", "anchor", "father", "inheritRotation", "layers", "extended",
             "controls", "notes", "foreign"}
NOTE_KEYS = {"kind", "startBeat", "endBeat", "laneX", "side", "isFake", "alpha",
             "speed", "widthScale", "yOffset", "visibleTime", "judgeAreaScale",
             "tint", "hitEffectTint", "hitsound", "foreign"}
EVENT_KEYS = {"startBeat", "endBeat", "startValue", "endValue", "easing", "bezier",
              "bezierPoints", "easingRange", "linkGroup"}


def load_easings() -> set[str]:
    data = json.loads((HERE / "easing.json").read_text(encoding="utf-8"))
    return {e["name"] for e in data["easings"]}


EASINGS = load_easings()


class Report:
    def __init__(self, path: Path) -> None:
        self.path = path
        self.errors: list[tuple[str, str]] = []
        self.warnings: list[tuple[str, str]] = []

    def err(self, ptr: str, msg: str) -> None:
        self.errors.append((ptr, msg))

    def warn(self, ptr: str, msg: str) -> None:
        self.warnings.append((ptr, msg))

    def ok(self) -> bool:
        return not self.errors

    def dump(self) -> None:
        for ptr, msg in self.errors:
            print(f"  ERROR  {ptr}  {msg}")
        for ptr, msg in self.warnings:
            print(f"  WARN   {ptr}  {msg}")
        status = "FAIL" if self.errors else ("PASS(warn)" if self.warnings else "PASS")
        print(f"[{status}] {self.path.name}: {len(self.errors)} error(s), "
              f"{len(self.warnings)} warning(s)")


def is_num(v) -> bool:
    return isinstance(v, (int, float)) and not isinstance(v, bool)


def beat_of(v, ptr: str, rep: Report):
    """把 {n, d} 解成 Fraction；非法则记错并返回 None。"""
    if not isinstance(v, dict):
        rep.err(ptr, "拍必须是 {\"n\": int, \"d\": int} 对象")
        return None
    n, d = v.get("n"), v.get("d")
    if not isinstance(n, int) or isinstance(n, bool) or not isinstance(d, int) \
            or isinstance(d, bool) or d < 1:
        rep.err(ptr, "拍非法：需要整数 n 与正整数 d")
        return None
    extra = set(v) - {"n", "d"}
    if extra:
        rep.warn(ptr, f"拍对象含多余键 {sorted(extra)}")
    return Fraction(n, d)


def unknown_keys(node: dict, allowed: set[str], ptr: str, rep: Report) -> None:
    extra = {k for k in node if k not in allowed
             and not k.startswith("x-opm:") and k != "foreign"}
    if extra:
        rep.warn(ptr, f"未知字段 {sorted(extra)}（应为 foreign 或 x-opm: 前缀）")


def check_color(v, ptr: str, rep: Report) -> None:
    if not (isinstance(v, list) and len(v) == 3 and all(
            isinstance(c, int) and not isinstance(c, bool) and 0 <= c <= 255 for c in v)):
        rep.err(ptr, "颜色必须是 0~255 的 [R, G, B] 整数三元组")


def check_event_track(track: list, name: str, ptr: str, rep: Report,
                      chart_end: Fraction, version: int = 2) -> None:
    prev_end = None
    for i, ev in enumerate(track):
        p = f"{ptr}[{i}]"
        if not isinstance(ev, dict):
            rep.err(p, "事件必须是对象")
            continue
        unknown_keys(ev, EVENT_KEYS, p, rep)
        sb = beat_of(ev.get("startBeat"), f"{p}.startBeat", rep)
        eb = beat_of(ev.get("endBeat"), f"{p}.endBeat", rep)
        if sb is None or eb is None:
            continue
        if eb <= sb:
            rep.err(p, f"endBeat({eb}) 必须大于 startBeat({sb})")
        if i == 0 and sb > EVENT_EPOCH:
            rep.err(f"{p}.startBeat",
                    f"轨道首事件必须从拍 0 或更早开始（当前 {sb}）；"
                    "补空隙是 codec 导入时的职责")
        if prev_end is not None and sb != prev_end:
            kind = "空隙" if sb > prev_end else "重叠"
            rep.err(p, f"轨道不连续（{kind}）：上一事件止于 {prev_end}，本事件起于 {sb}")
        prev_end = eb

        easing = ev.get("easing", "linear")
        if easing not in EASINGS:
            rep.err(f"{p}.easing", f"未知缓动 {easing!r}（见 spec/easing.json）")
        if ev.get("bezier") is True:
            bp = ev.get("bezierPoints")
            if not (isinstance(bp, list) and len(bp) == 4 and all(is_num(x) for x in bp)):
                rep.err(f"{p}.bezierPoints", "bezier 为 true 时需 4 个数字控制点")
        if name in SPEED_TRACK and ev.get("bezier") is True:
            rep.err(f"{p}.bezier", "速度事件不支持贝塞尔缓动")
        if easing == "inOutElastic" and name in SPEED_TRACK:
            rep.warn(f"{p}.easing", "29 号缓动在速度事件上语义未定")

        for key in ("startValue", "endValue"):
            v = ev.get(key)
            if v is None:
                rep.err(f"{p}.{key}", "事件缺少数值")
            elif name in RGB_TRACKS:
                check_color(v, f"{p}.{key}", rep)
            elif name in TEXT_TRACKS:
                if not isinstance(v, str):
                    rep.err(f"{p}.{key}", "文字事件的数值必须是字符串")
            elif not is_num(v):
                rep.err(f"{p}.{key}", "事件的数值必须是数字")

        # alpha 的量纲随 formatVersion 变（v2 起 0~255，与 RPE 和音符的 alpha 同量纲）。
        # 只是**警告**：规范第 8 节的错误清单里没有"数值越界"，量纲错通常表现为"线看不见了"，
        # 而不是读不动文件。
        if name == "alpha" and version >= 2:
            for key in ("startValue", "endValue"):
                v = ev.get(key)
                if is_num(v) and not 0 <= v <= 255:
                    rep.warn(f"{p}.{key}",
                             f"v{version} 的线 alpha 应按 0~255 写（当前 {v}）"
                             "—— 旧版本这里是 0~1，写 1.0 会让线变得几乎全透明")

    if track and prev_end is not None and prev_end < chart_end:
        rep.err(ptr, f"轨道末事件止于 {prev_end}，早于谱面末尾 {chart_end}"
                     "（官谱语义下会导致谱面停顿）")


def beat_quiet(v):
    """`beat_of` 的**不出声**版本：预扫"谱面末尾"用它。

    预扫必须静默 —— 不静默的话，主扫那一遍会在同一个位置再报一次，
    同一条错误于是在报告里出现两次（错误数看起来翻倍，实际只有一条）。
    """
    if not isinstance(v, dict):
        return None
    n, d = v.get("n"), v.get("d")
    if not isinstance(n, int) or isinstance(n, bool) or not isinstance(d, int) \
            or isinstance(d, bool) or d < 1:
        return None
    return Fraction(n, d)


def chart_end_of(lines: list, zones: list | None = None) -> Fraction:
    """**全文档的谱面末尾**：音符、**基础轨**事件、以及**遮蔽区通道**事件里最晚的 `endBeat`。

    刻意与 `app/src/doc.rs` 的 `Document::chart_end` 逐条对应（那是唯一的实现）：
    · 音符取 `endBeat`，非 hold 没有该键 ⇒ 取 `startBeat`（= `Note::end_beat`）；
    · 基础轨五条（`moveX`/`moveY`/`rotate`/`alpha`/`speed`）；
    · **遮蔽区七条通道也算内容** —— 一块区域的顶点表演常常跟在最后一个音符之后
      （`tests/codec.rs::mask_events_extend_the_chart_end` 就是守着这条的）。
      注意别和第 8 节那条搞混：遮蔽区通道**自己**不受"末事件 ≥ 谱面末尾"约束（§4.6 允许它
      早于谱末结束），但它的末事件**会抬高**这个末尾，从而抬高**判定线轨道**要够到的地方。
    · `extended` / `controls` **不计入**（Rust 那边也不扫这两个词典）。

    早先这里是**逐线**算的（而且只按该线自己的音符），于是"多线谱面里 A 线的轨道铺到末尾、
    B 线的轨道只铺到自己的音符"这种谱面：Rust 的 `validate` 报错、本校验器放行 ——
    同一份文件两个答案。2026-10-03 收口成全局。
    """
    end = EVENT_EPOCH
    for line in lines:
        if not isinstance(line, dict):
            continue
        notes = line.get("notes")
        if isinstance(notes, list):
            for note in notes:
                if not isinstance(note, dict):
                    continue
                b = beat_quiet(note.get("endBeat", note.get("startBeat")))
                if b is not None and b > end:
                    end = b
        layers = line.get("layers")
        if isinstance(layers, list):
            for layer in layers:
                if not isinstance(layer, dict):
                    continue
                for name in TRACKS:
                    track = layer.get(name)
                    if not isinstance(track, list):
                        continue
                    for ev in track:
                        if isinstance(ev, dict):
                            b = beat_quiet(ev.get("endBeat"))
                            if b is not None and b > end:
                                end = b
    for zone in zones or []:
        if not isinstance(zone, dict):
            continue
        for name in MASK_TRACKS:
            track = zone.get(name)
            if not isinstance(track, list):
                continue
            for ev in track:
                if isinstance(ev, dict):
                    b = beat_quiet(ev.get("endBeat"))
                    if b is not None and b > end:
                        end = b
    return end


def check_note(note: dict, ptr: str, rep: Report) -> None:
    if not isinstance(note, dict):
        rep.err(ptr, "音符必须是对象")
        return
    unknown_keys(note, NOTE_KEYS, ptr, rep)
    kind = note.get("kind")
    if kind not in KINDS:
        rep.err(f"{ptr}.kind", f"未知音符类型 {kind!r}（{sorted(KINDS)}）")

    sb = beat_of(note.get("startBeat"), f"{ptr}.startBeat", rep)
    has_end = "endBeat" in note
    eb = beat_of(note.get("endBeat"), f"{ptr}.endBeat", rep) if has_end else None
    if kind == "hold":
        if eb is None:
            rep.err(ptr, "hold 必须有 endBeat")
        elif sb is not None and eb <= sb:
            rep.err(f"{ptr}.endBeat", f"hold 的 endBeat({eb}) 必须大于 startBeat({sb})")
    elif has_end:
        rep.err(ptr, f"非 hold 音符不得携带 endBeat（kind={kind!r}）")

    if "laneX" not in note:
        rep.err(f"{ptr}.laneX", "缺少 laneX")
    elif not is_num(note["laneX"]):
        rep.err(f"{ptr}.laneX", "laneX 必须是数字")
    elif abs(note["laneX"]) > X_MAX:
        rep.warn(f"{ptr}.laneX", f"laneX={note['laneX']} 超出 RPE 坐标系 ±{X_MAX}")

    side = note.get("side", "above")
    if side not in SIDES:
        rep.err(f"{ptr}.side", f"未知正反面 {side!r}")

    alpha = note.get("alpha", 255)
    if not isinstance(alpha, int) or isinstance(alpha, bool):
        rep.err(f"{ptr}.alpha", "alpha 必须是整数")
    elif alpha > 65535:
        rep.err(f"{ptr}.alpha", "alpha 超出可容纳范围")
    elif not 0 <= alpha <= 255:
        rep.warn(f"{ptr}.alpha", f"alpha={alpha} 超出规范 0~255（RPE 实际存在此类值，"
                                 "读入不得截断，但需提示）")

    if kind == "hold" and note.get("speed") == 0:
        rep.warn(ptr, "hold 的 speed 为 0：长度为 0，不会被渲染")
    if note.get("hitsound") is not None:
        rep.warn(f"{ptr}.hitsound", "自定义打击音：目标播放器可能忽略")
    tint = note.get("tint")
    if tint is not None:
        check_color(tint, f"{ptr}.tint", rep)
    het = note.get("hitEffectTint")
    if het is not None:
        check_color(het, f"{ptr}.hitEffectTint", rep)
    jas = note.get("judgeAreaScale")
    if jas is not None:
        if not is_num(jas) or jas <= 0:
            rep.err(f"{ptr}.judgeAreaScale", "判定区宽度倍率必须是正数")


def check_judge_line(line: dict, idx: int, rep: Report, count: int,
                     chart_end: Fraction, version: int = 2) -> None:
    """一条判定线。`chart_end` 是**全文档**的谱面末尾（`chart_end_of` 算一次，所有线共用）。

    它以前在这里现算，而且只按**本线自己的**音符算 —— 见 `chart_end_of` 的说明。
    """
    ptr = f"/judgeLines[{idx}]"
    if not isinstance(line, dict):
        rep.err(ptr, "判定线必须是对象")
        return
    unknown_keys(line, LINE_KEYS, ptr, rep)

    if line.get("bpmFactor", 1.0) == 0:
        rep.err(f"{ptr}.bpmFactor", "bpmFactor 不得为 0（线 BPM = 谱面 BPM / bpmFactor）")
    elif not is_num(line.get("bpmFactor", 1.0)):
        rep.err(f"{ptr}.bpmFactor", "bpmFactor 必须是数字")

    z = line.get("zOrder", 0)
    if not isinstance(z, int) or isinstance(z, bool):
        rep.err(f"{ptr}.zOrder", "zOrder 必须是整数")
    elif abs(z) > 100:
        rep.warn(f"{ptr}.zOrder", f"zOrder={z} 超出建议范围 ±100")

    ui = line.get("attachUI")
    if ui is not None and ui not in ATTACH_UI:
        rep.err(f"{ptr}.attachUI", f"未知 UI 绑定 {ui!r}")

    anchor = line.get("anchor", [0.5, 0.5])
    if not (isinstance(anchor, list) and len(anchor) == 2
            and all(is_num(a) and 0.0 <= a <= 1.0 for a in anchor)):
        rep.err(f"{ptr}.anchor", "anchor 必须是两个 0~1 的数")

    father = line.get("father", -1)
    if not isinstance(father, int) or isinstance(father, bool):
        rep.err(f"{ptr}.father", "father 必须是整数索引")
    elif father != -1 and not 0 <= father < count:
        rep.err(f"{ptr}.father", f"father={father} 越界（共 {count} 条判定线）")
    elif father == idx:
        rep.err(f"{ptr}.father", "判定线不能以自己为父线")

    layers = line.get("layers")
    if not isinstance(layers, list) or not layers:
        rep.err(f"{ptr}.layers", "layers 必须是非空数组")
        layers = []
    elif len(layers) > 5:
        rep.err(f"{ptr}.layers", f"层数 {len(layers)} 超过 RPE 上限 5")

    notes = line.get("notes")
    if not isinstance(notes, list):
        rep.err(f"{ptr}.notes", "notes 必须是数组")
        notes = []
    for i, note in enumerate(notes):
        check_note(note, f"{ptr}.notes[{i}]", rep)

    # `chart_end` 是**全文档**一份（由 `check_document` 传入，见 `chart_end_of`）：
    # 早先它在这里现算，而且只按本线自己的音符算 —— 那是同一份文件两个答案的根源。
    has_hold = any(isinstance(n, dict) and n.get("kind") == "hold" for n in notes)
    for li, layer in enumerate(layers):
        lp = f"{ptr}.layers[{li}]"
        if not isinstance(layer, dict):
            rep.err(lp, "层必须是对象")
            continue
        unknown_keys(layer, set(TRACKS) | set(EXT_TRACKS), lp, rep)
        for name in TRACKS:
            track = layer.get(name)
            if track is None:
                continue
            if not isinstance(track, list):
                rep.err(f"{lp}.{name}", "轨道必须是数组")
                continue
            check_event_track(track, name, f"{lp}.{name}", rep, chart_end, version)

    ext = line.get("extended") or {}
    if not isinstance(ext, dict):
        rep.err(f"{ptr}.extended", "extended 必须是对象")
    else:
        unknown_keys(ext, set(EXT_TRACKS), f"{ptr}.extended", rep)
        for name in EXT_TRACKS:
            track = ext.get(name)
            if isinstance(track, list):
                check_event_track(track, name, f"{ptr}.extended.{name}", rep, chart_end, version)
            elif track is not None:
                rep.err(f"{ptr}.extended.{name}", "轨道必须是数组")

    ctrl = line.get("controls") or {}
    if not isinstance(ctrl, dict):
        rep.err(f"{ptr}.controls", "controls 必须是对象")
    else:
        unknown_keys(ctrl, set(CTRL_TRACKS), f"{ptr}.controls", rep)
        for name in CTRL_TRACKS:
            track = ctrl.get(name)
            if track is None:
                continue
            if has_hold:
                rep.warn(f"{ptr}.controls.{name}",
                         "控制曲线对 hold 无效（RPE 原义），本线含 hold 音符")
            if not isinstance(track, list):
                rep.err(f"{ptr}.controls.{name}", "控制曲线必须是数组")
                continue
            prev = None
            for ki, kf in enumerate(track):
                kp = f"{ptr}.controls.{name}[{ki}]"
                if not isinstance(kf, dict):
                    rep.err(kp, "关键帧必须是对象")
                    continue
                extra = set(kf) - {"atDistance", "value", "easing"}
                if extra:
                    rep.warn(kp, f"未知字段 {sorted(extra)}")
                if not is_num(kf.get("atDistance")):
                    rep.err(f"{kp}.atDistance", "缺少数值 atDistance（距判定线的纵向距离）")
                elif prev is not None and kf["atDistance"] < prev:
                    rep.err(f"{kp}.atDistance", "关键帧必须按 atDistance 升序")
                if is_num(kf.get("atDistance")):
                    prev = kf["atDistance"]
                if not is_num(kf.get("value")):
                    rep.err(f"{kp}.value", "缺少数值 value")
                if kf.get("easing", "linear") not in EASINGS:
                    rep.err(f"{kp}.easing", f"未知缓动 {kf.get('easing')!r}")


def active_state(v):
    """`active` 的值 → 它认领的状态（`None` = 类型不对，由别处报）。

    阈值与求值器同一条线：`≥ 0.5` 即 true（写 `true`/`false` 与写 `1`/`0` 等价）。
    """
    if isinstance(v, bool):
        return v
    if is_num(v):
        return v >= 0.5
    return None


def check_mask_track(track: list, name: str, ptr: str, rep: Report) -> None:
    """遮蔽区的一条通道。

    **与判定线轨道刻意不同的三条**（规范 §4.6）：
    · 允许**空隙**（空档里保持前值）；
    · 允许**首事件晚于拍 0** —— "这块区域什么时候出现"就是靠它表达的；
    · 不要求末事件延拓到谱面末尾。
    剩下的不变量只有：按 startBeat 升序、不重叠、endBeat > startBeat、缓动合法。
    """
    prev = None  # (start, end)
    for i, ev in enumerate(track):
        p = f"{ptr}[{i}]"
        if not isinstance(ev, dict):
            rep.err(p, "事件必须是对象")
            continue
        unknown_keys(ev, EVENT_KEYS, p, rep)
        sb = beat_of(ev.get("startBeat"), f"{p}.startBeat", rep)
        eb = beat_of(ev.get("endBeat"), f"{p}.endBeat", rep)
        if sb is None or eb is None:
            continue
        if eb <= sb:
            rep.err(p, f"endBeat({eb}) 必须大于 startBeat({sb})")
        if prev is not None:
            ps, pe = prev
            if sb < ps:
                rep.err(p, f"通道必须按 startBeat 升序（上一事件起于 {ps}，本事件起于 {sb}）")
            elif sb < pe:
                rep.err(p, f"通道不允许重叠：上一事件止于 {pe}，本事件起于 {sb}")
        prev = (sb, eb)

        easing = ev.get("easing", "linear")
        if easing not in EASINGS:
            rep.err(f"{p}.easing", f"未知缓动 {easing!r}（见 spec/easing.json）")
        for key in ("startValue", "endValue"):
            v = ev.get(key)
            if v is None:
                rep.err(f"{p}.{key}", "事件缺少数值")
            elif name == "active":
                # 二值化：写 true/false 最贴口径，写 0/1 也认（求值器按 ≥0.5 二值化）
                if not isinstance(v, bool) and not is_num(v):
                    rep.err(f"{p}.{key}", "active 的值必须是布尔（true/false）或数字")
            elif not is_num(v):
                rep.err(f"{p}.{key}", "遮蔽区坐标通道的值必须是数字")
        if name == "active":
            # **一个 active 事件块只能是一种状态**（用户口径 2026-10-02）：
            # 头尾值必须落在同一档（≥0.5 = true），否则这块区会在中途换外观。
            # 想中途换外观就放**两块**，别用渐变。
            a = active_state(ev.get("startValue"))
            b = active_state(ev.get("endValue"))
            if a is not None and b is not None and a != b:
                rep.err(
                    p,
                    f"active 事件块只能是一种状态：起值 {ev.get('startValue')!r} 与"
                    f"终值 {ev.get('endValue')!r} 分别是 {a} 与 {b} —— 想中途换外观就放两块",
                )


def check_mask_zone(zone, index: int, rep: Report) -> None:
    ptr = f"/maskZones[{index}]"
    if not isinstance(zone, dict):
        rep.err(ptr, "遮蔽区必须是对象")
        return
    unknown_keys(zone, set(MASK_TRACKS) | {"name"}, ptr, rep)
    name = zone.get("name")
    if name is not None and not isinstance(name, str):
        rep.err(f"{ptr}.name", "name 必须是字符串")
    for track_name in MASK_TRACKS:
        track = zone.get(track_name)
        if track is None:
            continue  # 缺省即空数组
        if not isinstance(track, list):
            rep.err(f"{ptr}.{track_name}", "通道必须是数组")
            continue
        check_mask_track(track, track_name, f"{ptr}.{track_name}", rep)


def check_document(doc, rep: Report) -> None:
    if not isinstance(doc, dict):
        rep.err("/", "根必须是对象")
        return
    unknown_keys(doc, ROOT_KEYS, "/", rep)

    if doc.get("format") != "opm":
        rep.err("/format", f"format 必须是 \"opm\"（当前 {doc.get('format')!r}）")
    fv = doc.get("formatVersion")
    if not isinstance(fv, int) or isinstance(fv, bool) or fv < 1:
        rep.err("/formatVersion", "formatVersion 必须是正整数")
    elif fv == 1:
        rep.warn("/formatVersion",
                 "formatVersion=1：**判定线 alpha 轨道是 0~1 量纲**（v2 起改成 0~255，与 RPE "
                 "和音符的 alpha 同量纲）。载入时会自动 ×255 迁移，无损；重新保存即为 v2")
    elif fv != 2:
        rep.warn("/formatVersion", f"formatVersion={fv}：本校验器实现 v1（alpha 0~1）与 v2（alpha 0~255）")

    cap = doc.get("minClientCapability")
    if not isinstance(cap, int) or isinstance(cap, bool) or not 0 <= cap <= CAP_MASK:
        rep.err("/minClientCapability", f"minClientCapability 必须是 0~{CAP_MASK} 的整数")
        cap = None
    exts = doc.get("extensions")
    if not isinstance(exts, list) or not all(isinstance(e, str) for e in exts):
        rep.err("/extensions", "extensions 必须是字符串数组")
        exts = []
    for e in exts:
        if not e.startswith("x-opm:"):
            rep.err("/extensions", f"扩展名 {e!r} 必须使用 x-opm: 前缀")
    if exts and cap is not None and cap < 3:
        rep.err("/minClientCapability",
                f"声明了 {len(exts)} 个扩展但 minClientCapability={cap}（应为 3）")
    if not exts and cap == 3:
        rep.warn("/minClientCapability", "minClientCapability=3 但没有任何扩展声明")

    # ---- 遮蔽区（躁域）----
    zones = doc.get("maskZones", [])
    if not isinstance(zones, list):
        rep.err("/maskZones", "maskZones 必须是数组")
        zones = []
    for i, zone in enumerate(zones):
        check_mask_zone(zone, i, rep)
    if zones and cap is not None and cap < CAP_MASK:
        rep.err("/minClientCapability",
                f"有 {len(zones)} 块遮蔽区但 minClientCapability={cap}（应为 {CAP_MASK}）——"
                "不认识遮蔽区的读取方必须拒绝载入，"
                "否则会渲染出一份「该挡的地方没挡」的谱面")

    meta = doc.get("meta")
    if not isinstance(meta, dict):
        rep.err("/meta", "meta 必须是对象")
    else:
        unknown_keys(meta, META_KEYS, "/meta", rep)
        # `audio` / `background` 是**必需（可空）**（§2.1）：没有音乐就写 `null`，别把键省掉。
        # 早先这里不查它们，而序列化器又会把 `null` 抹掉 —— "必需"于是只写在纸上。
        for key in ("name", "composer", "charter", "difficulty", "level", "offsetMs",
                    "audio", "background"):
            if key not in meta:
                rep.err(f"/meta.{key}", "缺少必需字段")
        if meta.get("difficulty") not in DIFFICULTIES:
            rep.err("/meta.difficulty", f"未知难度 {meta.get('difficulty')!r}")
        if not isinstance(meta.get("offsetMs"), int) or isinstance(meta.get("offsetMs"), bool):
            rep.err("/meta.offsetMs", "offsetMs 必须是整数（毫秒）")
        const = meta.get("constant")
        if const is not None and not is_num(const):
            rep.err("/meta.constant", "constant 必须是数字或 null")

    bpm_list = doc.get("bpmList")
    if not isinstance(bpm_list, list) or not bpm_list:
        rep.err("/bpmList", "bpmList 必须是非空数组")
        bpm_list = []
    prev_beat = None
    for i, entry in enumerate(bpm_list):
        p = f"/bpmList[{i}]"
        if not isinstance(entry, dict):
            rep.err(p, "BPM 项必须是对象")
            continue
        extra = set(entry) - {"startBeat", "bpm"}
        if extra:
            rep.warn(p, f"未知字段 {sorted(extra)}")
        b = beat_of(entry.get("startBeat"), f"{p}.startBeat", rep)
        bpm = entry.get("bpm")
        if not is_num(bpm) or bpm <= 0:
            rep.err(f"{p}.bpm", f"bpm 必须大于 0（当前 {bpm!r}）")
        if b is not None:
            if i == 0 and b != EVENT_EPOCH:
                rep.err(f"{p}.startBeat", f"首个 BPM 必须从拍 0 开始（当前 {b}）")
            if prev_beat is not None and b <= prev_beat:
                rep.err(f"{p}.startBeat", f"BPM 的 startBeat 必须严格递增（{prev_beat} → {b}）")
            prev_beat = b

    lines = doc.get("judgeLines")
    if not isinstance(lines, list) or not lines:
        rep.err("/judgeLines", "judgeLines 必须是非空数组")
        return
    if len(lines) > 100:
        rep.warn("/judgeLines", f"判定线 {len(lines)} 条 > 100：官谱格式下会导致谱面停顿")

    # 谱面末尾**先算一次**（全文档一份），再逐线校验 —— 顺序不能反：
    # 每条线都要拿它去判"本线的轨道铺到末尾了吗"，而它取的是**所有线**的最大值。
    chart_end = chart_end_of(lines, zones)
    # 版本影响 alpha 的量纲：1 ⇒ 0~1，2 ⇒ 0~255。取不到就按当前版本（2）判 —— 缺
    # formatVersion 本身已经被上面记了一条错，不必在这里再放大。
    version = fv if isinstance(fv, int) and not isinstance(fv, bool) else 2
    for i, line in enumerate(lines):
        check_judge_line(line, i, rep, len(lines), chart_end, version)

    # 父线成环检测
    n = len(lines)
    for start in range(n):
        seen, cur, hops = set(), start, 0
        while isinstance(lines[cur], dict) and hops <= n:
            f = lines[cur].get("father", -1)
            if not isinstance(f, int) or f < 0 or f >= n:
                break
            if f in seen:
                rep.err(f"/judgeLines[{start}].father",
                        f"父线形成环（{start} → … → {f}）")
                break
            seen.add(f)
            cur, hops = f, hops + 1

    total_notes = sum(len(l.get("notes", [])) for l in lines
                      if isinstance(l, dict) and isinstance(l.get("notes"), list))
    if total_notes > NOTE_SOFT_LIMIT:
        rep.warn("/judgeLines", f"音符合计 {total_notes} 超过软上限 {NOTE_SOFT_LIMIT}"
                                "（官谱实测加载近似 O(n²)）")


def main(argv: list[str]) -> int:
    args = [a for a in argv[1:] if a != "--compact"]
    compact = "--compact" in argv[1:]
    if not args:
        print(__doc__.strip())
        return 2

    failed = 0
    for name in args:
        path = Path(name)
        try:
            doc = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            print(f"[FAIL] {path.name}: 无法读取或解析 —— {exc}")
            failed += 1
            continue
        rep = Report(path)
        check_document(doc, rep)
        if compact:
            if not rep.ok():
                failed += 1
                print(f"FAIL {path.name} ({len(rep.errors)} error(s))")
        else:
            rep.dump()
            if not rep.ok():
                failed += 1
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
