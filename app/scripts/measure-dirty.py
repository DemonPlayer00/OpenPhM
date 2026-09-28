#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 DemonPlayer
"""OpenPhM 脏位落点验收：**每条话题到底重建了哪几个面板**。

为什么要有它：README 里那张"命令 → 话题 → Δ整表/属性/音符/轨道/检查"的表是**量出来的**，
不是设计出来的。这张表抓出过一个真 bug（2026-09-28）：
`Track`（改事件）**不刷检查器**，而检查器显示并可编辑**选中事件**的起止拍/值/缓动 ——
于是命令改了文档、左边的事件列表也换了新值，检查器却停在改之前那份快照上
（用户报的「第二个负变速事件似乎不生效 / 下拉选择框总选上一次选中的内容」就是它）。

跑法（GUI 必须带 `--control`；`--notes 400` 造 4 条线 / 400 音符的演示谱面，
`--bench` 保证有帧、`--shot-frame` 让进程活到量完）：
```sh
opm-app --notes 400 --control auto --bench 20000 --shot /tmp/x.png --shot-frame 15000 --shot-exit &
python3 scripts/measure-dirty.py /run/user/1000/opm-<pid>.sock
```

读数口径：每条命令**发出前**读一次 `ui_stats`，等 `seen_revision` 涨了再读一次，差值就是落点。
"""
import json
import socket
import sys
import time

if len(sys.argv) < 2:
    raise SystemExit("用法: measure-dirty.py /run/user/1000/opm-<pid>.sock")

FIELDS = [
    ("builds_structure", "整表"),
    ("builds_props", "属性"),
    ("builds_notes", "音符"),
    ("builds_tracks", "轨道"),
    ("builds_inspector", "检查"),
]

# 覆盖全部五种话题（外加一条 undo：它证明"撤销也走同一条广播路"）
CMDS = [
    ("set_meta", {"op": "set_meta", "set": {"name": "measure"}}),
    (
        "add_event alpha@0",
        {
            "op": "add_event",
            "line": 0,
            "layer": 0,
            "track": "alpha",
            "startBeat": [0, 1],
            "endBeat": [4, 1],
            "startValue": 1,
            "endValue": 0.5,
            "easing": "linear",
        },
    ),
    ("set_track_constant speed@3", {"op": "set_track_constant", "line": 3, "track": "speed", "value": 10}),
    ("add_note @2", {"op": "add_note", "line": 2, "kind": "tap", "startBeat": [9, 2], "laneX": 0.0}),
    ("undo", {"op": "undo"}),
    ("set_line zOrder@1", {"op": "set_line", "line": 1, "set": {"zOrder": 3}}),
]


def connect(path):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    f = s.makefile("rwb")
    json.loads(f.readline())  # hello
    return f


def send(f, obj):
    f.write((json.dumps(obj) + "\n").encode())
    f.flush()
    return json.loads(f.readline())


def stats(f):
    return send(f, {"op": "ui_stats"})["result"]


def wait_rev(f, rev, timeout=5.0):
    t0 = time.time()
    while time.time() - t0 < timeout:
        st = stats(f)
        if st["seen_revision"] > rev:
            return st
        time.sleep(0.002)
    raise SystemExit("广播没到（seen_revision 没涨）")


def main():
    f = connect(sys.argv[1])
    print(f"{'命令':30} {'话题':26} " + " ".join(f"{n:>4}" for _, n in FIELDS))
    for label, cmd in CMDS:
        before = stats(f)
        resp = send(f, cmd)
        if not resp.get("ok"):
            print(f"!! {label} 失败：{resp}")
            continue
        after = wait_rev(f, before["seen_revision"])
        delta = [after[k] - before[k] for k, _ in FIELDS]
        topics = "+".join(after["last_topics"])
        print(f"{label:30} {topics:26} " + " ".join(f"{d:>4}" for d in delta))


if __name__ == "__main__":
    main()
