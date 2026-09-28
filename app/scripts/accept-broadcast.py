#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 DemonPlayer
"""OpenPhM 更新广播验收：话题分级 + 端到端延迟。

直接说 UDS 上的行分隔 JSON（第二份客户端实现，顺带验证协议），
每条命令发出后轮询 ui_stats 直到 GUI 的 seen_revision 递增 —— 这就是
"命令 → 广播被应用"的端到端延迟，包含远端唤醒、出帧、重建三段的全部开销。
"""
import glob
import json
import socket
import sys
import time

SOCK = sorted(glob.glob("/run/user/1000/opm-*.sock"))[-1]


class Ctl:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        self.f = self.s.makefile("rwb")
        self.hello = json.loads(self.f.readline())

    def send(self, obj):
        self.f.write((json.dumps(obj) + "\n").encode())
        self.f.flush()
        return json.loads(self.f.readline())


def fmt(s):
    r = s["result"]
    return (
        "广播={broadcasts:>2}  重建 整表={builds_structure} 属性={builds_props} "
        "音符={builds_notes} 轨道={builds_tracks} 检查={builds_inspector}  "
        "跳过 整表={skipped_structure} 音符={skipped_notes} 轨道={skipped_tracks}".format(**r)
    )


def main():
    c = Ctl(SOCK)
    h = c.hello["result"]
    print(f"attach → pid={h['pid']} 文档={h['name']} 音符={h['notes']} 行={h['lines']} "
          f"revision={h['revision']} **订阅者={h['subscribers']}**")

    def stats():
        return c.send({"op": "ui_stats"})

    base = stats()
    print("\n基线               ", fmt(base))

    cases = [
        ("只改元信息 Meta", {"op": "set_meta", "set": {"name": "acceptance-run"}},
         "整表/音符/轨道/属性 都不该重建"),
        ("只改 0 号线的 alpha 轨道", {"op": "add_event", "line": 0, "layer": 0, "track": "alpha",
                                "startBeat": [0, 1], "endBeat": [8, 1],
                                "startValue": 0, "endValue": 255, "easing": "linear"},
         "只重建 0 号线的轨道缓存，别的一律不动"),
        ("只改 3 号线的流速轨道", {"op": "set_track_constant", "line": 3, "layer": 0,
                            "track": "speed", "value": 12.0},
         "同上是**逐线**的：只 3 号线的轨道缓存重建"),
        ("加音符到 2 号线 Notes/Note", {"op": "add_note", "line": 2, "kind": "tap",
                           "startBeat": [200, 1], "laneX": 0},
         "只重建 2 号线的音符缓存 + 检查器"),
        ("远端 undo", {"op": "undo"}, "撤掉上一步加音符 → 同上"),
        ("改 1 号线属性 LineProps", {"op": "set_line", "line": 1, "set": {"zOrder": 5}},
         "只重建 1 号线的属性缓存"),
    ]

    prev = base["result"]
    for name, cmd, expect in cases:
        t0 = time.perf_counter()
        rev_before = prev["seen_revision"]
        r = c.send(cmd)
        assert r.get("ok"), f"{name} 失败: {r}"
        # 轮询直到 GUI 报告应用了这条广播
        latency = None
        while time.perf_counter() - t0 < 2.0:
            st = stats()["result"]
            if st["seen_revision"] > rev_before:
                latency = (time.perf_counter() - t0) * 1000.0
                break
            time.sleep(0.002)
        st = stats()["result"]
        d = {k: st[k] - prev[k] for k in
             ("builds_structure", "builds_props", "builds_notes", "builds_tracks",
              "builds_meta", "builds_inspector", "skipped_structure", "skipped_props",
              "skipped_notes", "skipped_tracks", "skipped_inspector", "broadcasts")}
        print(f"\n{name}   —— {expect}")
        print(f"  话题            {','.join(st['last_topics'])}")
        print(f"  本条广播        {st['last_broadcast']}")
        print(f"  Δ重建           整表{d['builds_structure']:+d} 属性{d['builds_props']:+d} "
              f"音符{d['builds_notes']:+d} 轨道{d['builds_tracks']:+d} "
              f"元{d['builds_meta']:+d} 检查{d['builds_inspector']:+d}")
        print(f"  Δ跳过           整表{d['skipped_structure']:+d} 属性{d['skipped_props']:+d} "
              f"音符{d['skipped_notes']:+d} 轨道{d['skipped_tracks']:+d} "
              f"检查{d['skipped_inspector']:+d}")
        print(f"  端到端延迟      {latency:.2f} ms（命令 → GUI 已应用广播）" if latency is not None
              else "  端到端延迟      超时（GUI 未应用广播！）")
        prev = st

    print("\n最终               ", fmt(stats()))
    r = stats()["result"]
    print("\n口径：'跳过'逐广播统计（该话题与本面板无关）；'重建'逐批统计（同帧多条广播合并成一次），"
          "且**逐线**计（重建了几条线的这一部分）。")
    print(f"      触及该面板的广播数 = 广播 {r['broadcasts']} - 跳过 = "
          f"整表 {r['broadcasts']-r['skipped_structure']} / 音符 {r['broadcasts']-r['skipped_notes']} / "
          f"轨道 {r['broadcasts']-r['skipped_tracks']}；实际重建 音符{r['builds_notes']} 轨道{r['builds_tracks']}。")


if __name__ == "__main__":
    sys.exit(main())
