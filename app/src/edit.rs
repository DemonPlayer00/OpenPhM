// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **编辑意图 → 命令**：把界面上的手势（拖动音符、拖事件头尾、双击放音符…）翻成
//! `EditCore` 的命令 JSON。
//!
//! 为什么单独一层：这是"改谱面的那一下"最终落到哪条命令、带哪些字段的地方 ——
//! 字段名写错、hold 忘了带上 `endBeat`、边拖边丢帧，都是**看不见的功能性 bug**。
//! 放在这里就能拿测试钉住（喂一个 `EditorState`，断言吐出来的 JSON），
//! 而不是"开窗口拖一下看看对不对"。
//!
//! 三条纪律：
//! 1. **纯函数**：只读 `EditorState`，不发命令、不改状态（施加由 GUI/CLI 做）；
//! 2. 拍一律走 `EditorState::beat_json`（吸附后的**有理数** `[分子, 分母]`），别自己除；
//! 3. 整段拖拽用 `begin`/`commit` 包成**一个撤销步**（逐条改动仍然广播，面板实时更新）。

use serde_json::{json, Value};

use crate::doc::{EventRef, NoteKind};
use crate::perf;
use crate::state::{EditorState, EventSel, SelKind, TrackId};

/// 拖动中的音符 → `set_note`（**按冻结的原点算绝对位置**）。
///
/// 组拖动与单拖共用同一条：`start_beat` 是"按下那一刻的位置 + 位移"，
/// `hold_beats` 是按下那一刻的时长（拍）——**都不从当前文档反推**：
/// 拖拽期间文档每帧都在变，每帧反推一次会让偏移逐帧累积（越拖越偏）。
pub fn note_move_command(
    st: &EditorState,
    doc_index: usize,
    lane_x: f32,
    start_beat: f64,
    hold_beats: Option<f64>,
) -> Value {
    let mut set = json!({ "laneX": lane_x, "startBeat": st.beat_json(start_beat) });
    if let Some(d) = hold_beats {
        // 终点的拍 = 起点 + 原时长（吸附结果决定起点）⇒ 时长在拖动中不变
        set["endBeat"] = json!(st.beat_json(start_beat + d.max(1e-3)));
    }
    json!({ "op": "set_note", "line": st.selected_doc_line(), "index": doc_index, "set": set })
}

/// 拖事件块的头/尾 → `resize_event`。
///
/// 语义是"**只改这一个事件**"（早先会同步邻块，用户明确否掉了）；由此产生的空隙/重叠由
/// 重叠检测报出来（那份检测现在归 `EditCore`）。
///
/// `at` 是**文档地址**（图层 + 该图层里的下标）：视图把五个图层合并成一条时间线，
/// 合并下标直接当图层下标用会改到另一条事件（详见 `doc::EventRef`）。
pub fn event_resize_command(
    st: &EditorState,
    track: TrackId,
    at: EventRef,
    edge: crate::state::EventEdge,
    beat: f64,
) -> Value {
    let edge = match edge {
        crate::state::EventEdge::Start => "start",
        crate::state::EventEdge::End => "end",
    };
    json!({
        "op": "resize_event",
        "line": st.selected_doc_line(),
        "layer": at.layer,
        "track": track.key(),
        "index": at.index,
        "edge": edge,
        "toBeat": st.beat_json(beat),
    })
}

/// 精确有理拍 → **命令语言**里的拍形状 `[分子, 分母]`（既约，不过浮点）。
///
/// 命令语言只有这一种拍形状（`cmd::parse_beat`）；**文件**里的形状是 RPE 的三元组
/// `[整拍, 分子, 分母]`，由 `codec::beat_to_triple` 在导出时写出来。两者别混：
/// 界面控件按三元组编辑，命令层收发既约分数。
pub fn beat_arg(b: crate::doc::Beat) -> Value {
    json!([b.n, b.d])
}

/// 事件头/尾 → `resize_event`，**精确有理拍**（事件编辑器的三元组控件走这条）。
///
/// 与上面那条的区别就是**不吸附、不经过浮点**：控件编出来的是 `【整拍】+【分子】/【分母】`，
/// 这里直接算成既约分数 `[分子, 分母]` 交给命令层（命令语言只有这一种拍形状，见
/// `cmd::parse_beat`）。走浮点那条在 1/3、1/6 这类拍上会先被舍入到最近的 f64、
/// 再按网格取整 —— 用户明明按 1/3 编的，落到的却是别的位置。
pub fn event_resize_command_exact(
    st: &EditorState,
    track: TrackId,
    at: EventRef,
    edge: crate::state::EventEdge,
    beat: crate::doc::Beat,
) -> Value {
    let edge = match edge {
        crate::state::EventEdge::Start => "start",
        crate::state::EventEdge::End => "end",
    };
    json!({
        "op": "resize_event",
        "line": st.selected_doc_line(),
        "layer": at.layer,
        "track": track.key(),
        "index": at.index,
        "edge": edge,
        "toBeat": beat_arg(beat),
    })
}

/// 组拖动中的一条事件 → `set_event`：整块挪到 `[start, end]`（时长不变）。
pub fn event_move_command(
    st: &EditorState,
    track: TrackId,
    at: EventRef,
    start: f64,
    end: f64,
) -> Value {
    json!({
        "op": "set_event",
        "line": st.selected_doc_line(),
        "layer": at.layer,
        "track": track.key(),
        "index": at.index,
        "set": {
            "startBeat": st.beat_json(start),
            "endBeat": st.beat_json(end.max(start + 1e-3)),
        },
    })
}

// ---------------------------------------------------------------- 组拖动

/// 一个被拖的音符的**冻结原点**
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoteGrab {
    /// 视图下标（认锚要用它；也是"这一条还在不在选区里"的凭据）
    pub view_index: usize,
    pub doc_index: usize,
    pub lane_x: f32,
    pub start_beat: f64,
    /// hold 的时长（拍）；非 hold 为 `None`（跟着走的就是"起点 + 时长"）
    pub hold_beats: Option<f64>,
}

/// 一条被拖的事件的**冻结原点**
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EventGrab {
    pub track: TrackId,
    pub at: EventRef,
    /// 视图下标 —— 拖拽期间用它把"未选中的邻居"认出来（钉子/障碍就是那些）
    pub view_index: usize,
    pub start_beat: f64,
    pub end_beat: f64,
}

/// **抓手的冻结原点**：按下左键那一刻，把选区里每一条的位置记下来。
///
/// 为什么必须冻结：拖拽期间每帧都发命令、文档每帧都在变。若每帧从当前文档重算
/// "它原来在哪"，位移就会累积 —— 表现为"拖一下就跑得比鼠标快"。
#[derive(Clone, Debug, PartialEq)]
pub struct Grab {
    pub kind: SelKind,
    /// 手指按住的是哪一个（吸附只按它算，其余的保持相对偏移）
    pub anchor_lane: f32,
    pub anchor_beat: f64,
    /// 按下时指针的位置（laneX / 拍）——位移 = 当前指针 − 它
    pub press_lane: f32,
    pub press_beat: f64,
    pub notes: Vec<NoteGrab>,
    pub events: Vec<EventGrab>,
}

impl Grab {
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty() && self.events.is_empty()
    }
    /// 选区里最靠前的起点（负拍保护要用它）
    pub fn min_start_beat(&self) -> f64 {
        self.notes
            .iter()
            .map(|n| n.start_beat)
            .chain(self.events.iter().map(|e| e.start_beat))
            .fold(f64::INFINITY, f64::min)
    }
}

/// 组拖动开始时的**意图**：拖谁、手指按在哪。
///
/// 为什么不让 `grab_selection` 自己去读选区：面板这一帧看到的状态是**上一帧**的，
/// 而"手指按在一个没选中的东西上"这件事必须先把它变成选区再拖（否则会把别的一起带走）——
/// 所以"拖谁"由调用方明确给出来，不在库里猜。
#[derive(Clone, Debug, PartialEq)]
pub struct GrabIntent {
    /// 要拖的音符（视图下标）；与 `events` 互斥（选区同时只有一类）
    pub notes: Vec<usize>,
    pub events: Vec<EventSel>,
    /// 手指按住的那一个：吸附只按它算，其余的保持相对偏移
    pub anchor_note: Option<usize>,
    pub anchor_event: Option<EventSel>,
    /// 按下时指针的位置（laneX / 拍）
    pub press_lane: f32,
    pub press_beat: f64,
}

impl GrabIntent {
    /// 拖**整个选区**（手指按在选区里的某一条上）
    pub fn selection(st: &EditorState, press_lane: f32, press_beat: f64) -> Self {
        Self {
            notes: st.selection().notes().collect(),
            events: st.selection().events().collect(),
            anchor_note: st.selected_note(),
            anchor_event: st.selected_event_ref(),
            press_lane,
            press_beat,
        }
    }
    /// 只拖**一个音符**（手指按在选区之外的东西上：先把选区换成它）
    pub fn one_note(i: usize, press_lane: f32, press_beat: f64) -> Self {
        Self {
            notes: vec![i],
            events: Vec::new(),
            anchor_note: Some(i),
            anchor_event: None,
            press_lane,
            press_beat,
        }
    }
    /// 只拖**一条事件**
    pub fn one_event(track: TrackId, i: usize, press_lane: f32, press_beat: f64) -> Self {
        Self {
            notes: Vec::new(),
            events: vec![(track, i)],
            anchor_note: None,
            anchor_event: Some((track, i)),
            press_lane,
            press_beat,
        }
    }
}

/// 按下左键：把**意图里的那些**冻结成抓手。
///
/// 为什么必须冻结：拖拽期间每帧都发命令、文档每帧都在变。若每帧从当前文档重算
/// "它原来在哪"，位移就会累积 —— 表现为"拖一下就跑得比鼠标还快"。
pub fn grab_selection(st: &EditorState, intent: &GrabIntent) -> Option<Grab> {
    let line = st.selected()?;
    let mut notes = Vec::new();
    for i in &intent.notes {
        let Some(n) = line.notes.get(*i) else { continue };
        let start_beat = st.chart.tmap.beat(n.time);
        let hold_beats = (n.end - n.time).abs();
        notes.push(NoteGrab {
            view_index: *i,
            doc_index: n.doc_index,
            lane_x: n.lane_x,
            start_beat,
            hold_beats: (hold_beats > 1e-6)
                .then(|| st.chart.tmap.beat(n.end) - st.chart.tmap.beat(n.time)),
        });
    }
    let mut events = Vec::new();
    for (track, i) in &intent.events {
        let tv = line.track(*track);
        let (Some(e), Some(at)) = (tv.events.get(*i), tv.origin(*i)) else {
            continue;
        };
        events.push(EventGrab {
            track: *track,
            at,
            view_index: *i,
            start_beat: e.start.to_f64(),
            end_beat: e.end.to_f64(),
        });
    }
    let kind = match (notes.is_empty(), events.is_empty()) {
        (false, true) => SelKind::Notes,
        (true, false) => SelKind::Events,
        _ => return None, // 空抓手，或"两类都有"（选区同时只有一类，不该发生）
    };
    let anchor_note = intent
        .anchor_note
        .and_then(|a| notes.iter().find(|n| n.view_index == a));
    let anchor_event = intent
        .anchor_event
        .and_then(|a| events.iter().find(|e| (e.track, e.view_index) == a));
    let (anchor_lane, anchor_beat) = match kind {
        SelKind::Notes => anchor_note
            .or(notes.first())
            .map(|n| (n.lane_x, n.start_beat))
            .unwrap_or((intent.press_lane, intent.press_beat)),
        // 事件没有横向自由度：横向那一半只是占位（`grab_delta` 不会用它）
        SelKind::Events => anchor_event
            .or(events.first())
            .map(|e| (intent.press_lane, e.start_beat))
            .unwrap_or((intent.press_lane, intent.press_beat)),
    };
    Some(Grab {
        kind,
        anchor_lane,
        anchor_beat,
        press_lane: intent.press_lane,
        press_beat: intent.press_beat,
        notes,
        events,
    })
}

/// **最近合法位置**：请求的平移量 `want` 若会让选中块与未选中的邻居重叠，
/// 就退到"离请求最近的那个合法位置"。
///
/// 规则由用户定（仿 kdenlive 时间轴拖动）：**不硬夹在邻居边界上** ——
/// 轨道上只要还有一块够大的空隙，把指针拖得足够远就**跨过障碍落到那块空隙里**；
/// 轨道整条铺满时唯一的合法位置就是原地（Δ=0），这也是"夹住"的字面结果。
///
/// 相接（块尾 == 邻居头）算合法：不产生重叠就不算侵犯。
///
/// `selected` / `blocked` 都是 `(起拍, 止拍)`；`floor` 是最小允许位移（照惯例是
/// `-min_start`，即不许挪到负拍）。
pub fn nearest_free_delta(
    selected: &[(f64, f64)],
    blocked: &[(f64, f64)],
    want: f64,
    floor: f64,
) -> f64 {
    if selected.is_empty() {
        return want.max(floor);
    }
    // ① "会重叠"的位移区间：选中块 s 与障碍 u 重叠 ⇔ s.end+Δ > u.start 且 s.start+Δ < u.end
    let mut bad: Vec<(f64, f64)> = vec![(f64::NEG_INFINITY, floor)];
    for &(s, e) in selected {
        for &(bs, be) in blocked {
            bad.push((bs - e, be - s));
        }
    }
    // ② 合并**只并真正重叠的**（`a < last.1`，不是 `<=`）：
    //    禁区都是**开区间**（相接不算重叠），所以 "(…,4)" 与 "(4,…)" 之间的那一个点 4
    //    是合法的 —— 把它并掉就等于"贴着邻居也不许停"，用户要的"贴住"就没了。
    bad.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged: Vec<(f64, f64)> = Vec::with_capacity(bad.len());
    for (a, b) in bad {
        match merged.last_mut() {
            Some(last) if a < last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    // ③ 合法区间 = 补集；每个区间里取离 want 最近的点（就是夹进区间）
    let mut best: Option<(f64, f64)> = None; // (候选值, 与 want 的距离)
    let mut cursor = floor;
    let consider = |lo: f64, hi: f64, best: &mut Option<(f64, f64)>| {
        if hi < lo {
            return;
        }
        let v = want.clamp(lo, hi);
        let d = (v - want).abs();
        if best.map(|(_, bd)| d < bd).unwrap_or(true) {
            *best = Some((v, d));
        }
    };
    for &(a, b) in &merged {
        // `>=`：禁区是开区间，(cursor, a) 与 [cursor, a] 都合法 ——
        // 退化成一个点（cursor == a）时正是"贴住邻居"那一个位置，不能漏
        if a >= cursor {
            consider(cursor, a, &mut best);
        }
        cursor = cursor.max(b);
    }
    consider(cursor, f64::INFINITY, &mut best);
    best.map(|(v, _)| v).unwrap_or(want)
}

/// 组拖动的一步：给定抓手与"请求的位移"，算出**吸附 + 夹子之后**真正该用的位移。
///
/// 三件事，顺序不能换：
/// 1. 抓手（锚）先按网格吸附 —— 于是写回文档的拍仍是格点上的有理数；
/// 2. 不许挪到负拍（整组一起夹，保持相对间隔）；
/// 3. 事件再过 [`nearest_free_delta`]（音符之间没有"占位"这回事，不必过）。
pub fn grab_delta(st: &EditorState, grab: &Grab, cur_lane: f32, cur_beat: f64) -> (f32, f64) {
    let want_lane = grab.anchor_lane + (cur_lane - grab.press_lane);
    let want_beat = grab.anchor_beat + (cur_beat - grab.press_beat);
    let mut d_lane = st.snap_lane(want_lane) - grab.anchor_lane;
    let mut d_beat = st.snap_beat(want_beat) - grab.anchor_beat;
    // 负拍：整组一起夹（逐条夹会把相对间隔压扁）
    let floor = -grab.min_start_beat();
    d_beat = d_beat.max(floor);
    if grab.kind == SelKind::Events {
        // 障碍 = 同一条轨道上**没被选中**的那些事件
        let mut blocked: Vec<(f64, f64)> = Vec::new();
        if let Some(l) = st.selected() {
            let mut tracks: Vec<TrackId> = grab.events.iter().map(|g| g.track).collect();
            tracks.sort_by_key(|t| *t as usize);
            tracks.dedup();
            for t in tracks {
                for (i, e) in l.track(t).events.iter().enumerate() {
                    if grab.events.iter().any(|g| g.track == t && g.view_index == i) {
                        continue;
                    }
                    blocked.push((e.start.to_f64(), e.end.to_f64()));
                }
            }
        }
        let selected: Vec<(f64, f64)> = grab
            .events
            .iter()
            .map(|e| (e.start_beat, e.end_beat))
            .collect();
        d_beat = nearest_free_delta(&selected, &blocked, d_beat, floor);
    }
    if grab.kind == SelKind::Notes {
        // 音符横向没有"占位"冲突，但整组也要留在**看得见的窗口**里：
        // 逐条夹会把相对间隔压扁，所以按整组的极值夹**位移**。
        let lo = grab.notes.iter().map(|n| n.lane_x).fold(f32::INFINITY, f32::min);
        let hi = grab.notes.iter().map(|n| n.lane_x).fold(f32::NEG_INFINITY, f32::max);
        let (win_lo, win_hi) = st.window_lane_range();
        // 选区比窗口还宽时（窗口外编辑、或偏移改变）没有可移动的余地 —— 区间会反过来，
        // 直接 clamp 会 panic（本项目在 clamp 上踩过），所以先判区间是否成立。
        if lo.is_finite() && hi.is_finite() && win_lo - lo <= win_hi - hi {
            d_lane = d_lane.clamp(win_lo - lo, win_hi - hi);
        }
    }
    (d_lane, d_beat)
}

/// 组拖动的一步：按冻结原点 + 位移算出**命令序列**（每条一个 `set_note` / `set_event`）。
pub fn move_grab_commands(st: &EditorState, grab: &Grab, d_lane: f32, d_beat: f64) -> Vec<Value> {
    let mut out = Vec::with_capacity(grab.notes.len() + grab.events.len());
    for n in &grab.notes {
        out.push(note_move_command(
            st,
            n.doc_index,
            n.lane_x + d_lane,
            n.start_beat + d_beat,
            n.hold_beats,
        ));
    }
    for e in &grab.events {
        out.push(event_move_command(
            st,
            e.track,
            e.at,
            e.start_beat + d_beat,
            e.end_beat + d_beat,
        ));
    }
    out
}

/// 这些事件里有没有**卷入重叠**的？
///
/// 判据是**两两比较**，不是"排序后看相邻的一对"：[0,10) / [1,2) / [3,4) 按起点排下来，
/// 相邻的是 (0,1) 与 (1,2) —— 第三块和第二块不重叠、和**第一块**才重叠，只查相邻会漏。
/// **相接不算重叠**（一块的尾巴正好是另一块的头，是规范允许的"紧接"）。
pub fn event_items_overlap(st: &EditorState, items: &[EventSel]) -> bool {
    let Some(line) = st.selected() else {
        return false;
    };
    for (track, i) in items {
        let tv = line.track(*track);
        let Some(a) = tv.events.get(*i) else { continue };
        let (a0, a1) = (a.start.to_f64(), a.end.to_f64());
        for (j, b) in tv.events.iter().enumerate() {
            if j == *i {
                continue;
            }
            let (b0, b1) = (b.start.to_f64(), b.end.to_f64());
            if a0 < b1 && b0 < a1 {
                return true;
            }
        }
    }
    false
}

/// 当前**选区**里的事件有没有卷入重叠（拖动被禁用的判据）
pub fn selection_has_event_overlap(st: &EditorState) -> bool {
    let items: Vec<EventSel> = st.selection().events().collect();
    event_items_overlap(st, &items)
}

/// **选中了卷入重叠的事件 ⇒ 禁止移动**（用户要求）。
///
/// 为什么直接拒绝而不是"尽力挪到最近合法位置"：重叠意味着这条轨道已经不是"一块接一块"的
/// 结构，那套推理（在"会重叠的位移"的补集里取最近点）整个失去意义 —— 想挪开 A 与 B 的重叠，
/// 正确做法是拖它们的**头/尾把手**改时间（那条路仍然可用），或者到冲突浏览器里点过去处理。
/// 猜一个位置只会把"已经坏了"的状态变得更难解释。
pub fn event_drag_disabled(st: &EditorState, items: &[EventSel]) -> bool {
    !items.is_empty() && event_items_overlap(st, items)
}

// ---------------------------------------------------------------- 删除

/// Del：把当前选区删干净 —— **一个撤销步**（begin + 删除 + commit）。
///
/// 顺序是正确性的一部分：`del_note` / `del_event` 都是 `remove(index)`，
/// 删掉第 3 条之后原来的第 4 条就变成第 3 条 ⇒ **同一张表里必须按下标降序发**。
/// 音符一张表（该线的 `notes`）；事件按 `(图层, 轨道)` 分表，各表内部降序。
///
/// 空选区返回空（调用方据此不发命令）。
pub fn delete_selection_commands(st: &EditorState) -> Vec<Value> {
    let Some(line) = st.selected() else {
        return Vec::new();
    };
    let mut doc_notes: Vec<usize> = st
        .selection()
        .notes()
        .filter_map(|i| line.notes.get(i).map(|n| n.doc_index))
        .collect();
    let mut events: Vec<(TrackId, EventRef)> = st
        .selection()
        .events()
        .filter_map(|(t, i)| line.track(t).origin(i).map(|at| (t, at)))
        .collect();
    if doc_notes.is_empty() && events.is_empty() {
        return Vec::new();
    }
    let line_doc = st.selected_doc_line();
    let mut out = vec![begin_command(DELETE_LABEL)];
    doc_notes.sort_unstable_by(|a, b| b.cmp(a));
    doc_notes.dedup();
    for index in doc_notes {
        out.push(json!({ "op": "del_note", "line": line_doc, "index": index }));
    }
    events.sort_by(|a, b| {
        (a.0 as usize, a.1.layer, a.1.index).cmp(&(b.0 as usize, b.1.layer, b.1.index))
    });
    events.dedup();
    events.reverse(); // 同一张表内降序（跨表顺序无所谓，分组只为可读）
    for (track, at) in events {
        out.push(json!({
            "op": "del_event", "line": line_doc,
            "layer": at.layer, "track": track.key(), "index": at.index,
        }));
    }
    out.push(commit_command());
    out
}

/// 放置一个音符 → `add_note`（双击空白处、或按 Q/W/E/R 快速放置都走这里）。
///
/// `end_beat` 只有 hold 才该给：**给了就写进命令**（长条的时长由此确定），
/// tap/flick/drag 给了会被丢弃 —— 免得"随手传了一个 end 就把 tap 变成隐式长条"。
pub fn place_note_command(
    st: &EditorState,
    kind: NoteKind,
    lane_x: f32,
    beat: f64,
    end_beat: Option<f64>,
) -> Value {
    let mut cmd = json!({
        "op": "add_note",
        "line": st.selected_doc_line(),
        "kind": kind.as_str(),
        "startBeat": st.beat_json(beat),
        "laneX": lane_x,
    });
    if kind == NoteKind::Hold {
        // hold 必须有终点；没给就按"一拍"兜底（命令层不猜，但也不发一条注定被拒的命令）
        let end = end_beat.unwrap_or(beat + 1.0);
        cmd["endBeat"] = json!(st.beat_json(end.max(beat + 1e-3)));
    }
    cmd
}

/// 拍 → 文档里的**有理数**：毫拍（`[(beat*1000).round(), 1000]`）。
///
/// 与 [`EditorState::beat_json`]（按当前网格等分）不同：切分点由播放头决定，不属于任何格线，
/// 所以用毫拍表示 —— **别把浮点拍直接写进文档**（那会在导出/往返里变成 `0.3333333`）。
pub fn milli_beat(beat: f64) -> [i64; 2] {
    [(beat * 1000.0).round() as i64, 1000]
}

// ---------------------------------------------------------------- 遮蔽区（躁域）

/// 新建一块遮蔽区（默认在中央摆一个正三角形 —— 那三条常量事件由核心写）。
///
/// `start` 是**播放头那一拍**（界面按当前网格吸附过）：区域从哪一刻开始存在，就是这条命令决定的
/// （用户口径："当前时间没有任何坐标事件就不显示"—— 首事件的起点就是它出现的那一刻）。
pub fn add_zone_command(start: crate::doc::Beat) -> Value {
    json!({ "op": "add_zone", "startBeat": beat_arg(start) })
}

/// 遮蔽区属性（目前只有 `name`）
pub fn set_zone_command(zone: usize, set: Value) -> Value {
    json!({ "op": "set_zone", "zone": zone, "set": set })
}

/// 删除一块遮蔽区
pub fn del_zone_command(zone: usize) -> Value {
    json!({ "op": "del_zone", "index": zone })
}

/// 在遮蔽区的某条通道上放一个事件块。
///
/// **不带值**：缺省值 = 该通道此刻的值（由核心算）—— 界面上"放下一刻不跳变"这句话
/// 只有核心算得准（它握着文档与时间映射），界面自己算会变成第二份求值实现。
pub fn add_mask_event_command(
    zone: usize,
    track: crate::state::MaskChannel,
    start: crate::doc::Beat,
    end: crate::doc::Beat,
) -> Value {
    json!({
        "op": "add_zone_event",
        "zone": zone,
        "track": track.key(),
        "startBeat": beat_arg(start),
        "endBeat": beat_arg(end),
    })
}

// ---------------------------------------------------------------- 遮蔽区事件：跨度规则
//
// **这几条是"能不能放、最多放到哪"的唯一一份判据**（格式不变量：同一通道内不许重叠、
// 必须按起点升序，见 `spec/opm-format.md` §4.6）。四个调用点共用它们：
// 编辑区的手势草稿、属性编辑器那颗按钮、核心的 `add_zone_event` 缺省终点、
// 以及 `set_zone_event` / `resize_zone_event` 的重叠闸门。
//
// 为什么必须共用：判定线那边的重叠留给冲突浏览器，遮蔽区**没有**那个东西 —— 编辑器自己
// 造出来的重叠会让「校验谱面」当场报错（自己的产物过不了自己的校验器）。以前这四条规则
// 分散在 `overlay::mask_block_span`（1 拍）、属性编辑器（4 拍，还不夹取）、核心（4 拍）
// 三处，于是"同一个动作换个入口就得到不同的块"。

/// `start` 处能不能起一块：**已经有一块正好从这一点开始** ⇒ `false`。
///
/// 两条起点相同的事件在任何求值口径下都是重叠，而"插一块"（起点落在别人**里面**）
/// 是另一回事：那会把前一块裁到新起点，是合法的。
pub fn mask_can_start(track: &[crate::doc::Event], start: crate::doc::Beat) -> bool {
    !track.iter().any(|e| e.start == start)
}

/// 终点上限 = **下一块的起点**（`None` = 后面没有块，不设上限）。
///
/// 只看"起点严格晚于 `start`"的那些块：起点早于 `start` 的块会被新块裁掉（见
/// `codec::trim_before_insert`），不是障碍。
pub fn mask_end_limit(track: &[crate::doc::Event], start: crate::doc::Beat) -> Option<crate::doc::Beat> {
    track
        .iter()
        .map(|e| e.start)
        .filter(|s| *s > start)
        .min()
}

/// 缺省终点：`start + MASK_EVENT_BEATS 拍`，**不越过下一块**；放不下（起点上已有块、
/// 或空档里塞不进任何长度）⇒ `None`。
///
/// 核心的 `add_zone_event` 不带 `endBeat` 时走它 —— 缺省值天然合法，显式给的值越界则报错
/// （见 `core.rs` 那条注释）。
pub fn mask_default_end(
    track: &[crate::doc::Event],
    start: crate::doc::Beat,
) -> Option<crate::doc::Beat> {
    if !mask_can_start(track, start) {
        return None;
    }
    let full = start.checked_add(crate::doc::Beat::new(crate::doc::MASK_EVENT_BEATS, 1))?;
    let end = match mask_end_limit(track, start) {
        Some(limit) if limit < full => limit,
        _ => full,
    };
    (end > start).then_some(end)
}

/// `cur` 换到 `index` 这个位置之后，与**别的**块重叠的那个下标（没有 ⇒ `None`）。
///
/// `set_zone_event` / `resize_zone_event` 的闸门 —— 与 `move_zone_event` 里那段是同一句话
/// （半个区间相交即重叠），只是那两个命令只动一个端点。
pub fn mask_overlap(
    cur: &crate::doc::Event,
    index: usize,
    track: &[crate::doc::Event],
) -> Option<usize> {
    track.iter().enumerate().find_map(|(i, o)| {
        (i != index && cur.start < o.end && o.start < cur.end).then_some(i)
    })
}

/// **草稿态**下"先建区、再应用编辑"的命令序列（用户口径 2026-10-02）。
///
/// "遮蔽区数量为 0 时也能进入遮蔽区编辑，此时有默认的绘制三角形事件，当用户执行任意编辑后
/// 创建遮蔽区并应用编辑（可撤销）"——于是草稿态的第一条编辑要带上 `add_zone`，
/// 而且**与那条编辑同属一个事务**（一次 Ctrl+Z 全回去）。
///
/// 为什么放在库里：`begin`/`commit` 的配对与 `add_zone` 的**顺序**是这条口径的全部内容，
/// 而它有两个调用点（编辑区的双击放块、属性编辑器那一列按钮）—— 各写一遍就会有一处漏掉 commit。
pub fn zone_draft_edits(draft: bool, start: crate::doc::Beat, edits: Vec<Value>) -> Vec<Value> {
    if edits.is_empty() || !draft {
        return edits;
    }
    let mut out = Vec::with_capacity(edits.len() + 3);
    out.push(begin_command("新建遮蔽区并应用编辑"));
    out.push(add_zone_command(start));
    out.extend(edits);
    out.push(commit_command());
    out
}

/// **草稿态下开始一次拖动**：只需要 `begin` + `add_zone`（`add_zone` 只能出现一次 ——
/// 同一帧里"拖动开始"与"改跨度"会各发一条命令，两条都带建区就会凭空多出一块）。
pub fn zone_draft_begin(draft: bool, start: crate::doc::Beat, label: &str) -> Vec<Value> {
    if draft {
        vec![begin_command("新建遮蔽区并应用编辑"), add_zone_command(start)]
    } else {
        vec![begin_command(label)]
    }
}

/// 改遮蔽区事件块（起止拍用精确有理数；值可以是数字或布尔 —— `active` 通道就是布尔）
pub fn set_mask_event_command(
    zone: usize,
    track: crate::state::MaskChannel,
    index: usize,
    set: Value,
) -> Value {
    json!({
        "op": "set_zone_event",
        "zone": zone,
        "track": track.key(),
        "index": index,
        "set": set,
    })
}

/// 删遮蔽区事件块
pub fn del_mask_event_command(
    zone: usize,
    track: crate::state::MaskChannel,
    index: usize,
) -> Value {
    json!({
        "op": "del_zone_event",
        "zone": zone,
        "track": track.key(),
        "index": index,
    })
}

/// 判定线属性：`set_line`。字段名是**文档字段名**（`zOrder`/`isCover`/`bpmFactor`/`name`），
/// 手写在这些调用点上迟早会写歪一个大小写。
pub fn set_line_command(line: usize, set: Value) -> Value {
    json!({ "op": "set_line", "line": line, "set": set })
}

/// 事件：切分（`atBeat` 用毫拍分数）
pub fn split_event_command(line: usize, track: &str, index: usize, at_beat: f64) -> Value {
    json!({
        "op": "split_event", "line": line, "layer": 0, "track": track,
        "index": index, "atBeat": milli_beat(at_beat),
    })
}

/// **就位目标** → `set_target`：一次给出"线此刻该在哪儿"（x/y/角度/透明度），四轨一起写。
///
/// 用户口径：「事件块结束点上，给一个单次事件目标设置……以保证最终 0 误差就位」。
/// 命令层负责"怎么写"（边界/块内切分/空位三种情形），这里只管把意图拼成 JSON。
/// `target` 为空 ⇒ 返回 `None`（检查器据此把按钮置灰，而不是发一条会被拒的命令）。
pub fn set_target_command(
    line: usize,
    at: crate::doc::Beat,
    target: &[(&'static str, f64)],
) -> Option<serde_json::Value> {
    if target.is_empty() {
        return None;
    }
    let mut m = serde_json::Map::new();
    for (k, v) in target {
        m.insert((*k).to_owned(), json!(*v));
    }
    Some(json!({
        "op": "set_target",
        "line": line,
        "atBeat": [at.n, at.d],
        "target": serde_json::Value::Object(m),
    }))
}

/// 新建一个事件块 → `add_event`（事件区按键放置走这里）。
///
/// 值取**平段**（`startValue == endValue`）：新事件不该带来跳变；要渐变就放好之后在属性编辑器里改。
pub fn place_event_command(
    st: &EditorState,
    track: TrackId,
    layer: usize,
    start: f64,
    end: f64,
    value: f64,
) -> Value {
    json!({
        "op": "add_event",
        "line": st.selected_doc_line(),
        "layer": layer,
        "track": track.key(),
        "startBeat": st.beat_json(start),
        "endBeat": st.beat_json(end.max(start + 1e-3)),
        "startValue": value,
        "endValue": value,
        "easing": "linear",
    })
}

/// 该轨道"什么都不发生"的值：移动/旋转 0、透明度 1（全不透明）、流速 10（倍速基准）。
///
/// 依据是 `state::TrackId` 里写的量纲（`Alpha` 0–1、`Speed` ×10 = 基准）——
/// **只在轨道上一条事件都没有时**才用得到；有事件时取"此刻的值"（见 [`new_event_value`]）。
pub fn track_neutral_value(track: TrackId) -> f64 {
    match track {
        TrackId::Alpha => 1.0,
        TrackId::Speed => 10.0,
        TrackId::MoveX | TrackId::MoveY | TrackId::Rotate => 0.0,
    }
}

/// 新事件块的初值：**优先取该轨道此刻的值**（放一条平段事件 ⇒ 画面不变），
/// 轨道为空时才用 [`track_neutral_value`]。
pub fn new_event_value(st: &EditorState, track: TrackId, beat: f64) -> f64 {
    st.selected()
        .map(|l| l.track(track))
        .and_then(|t| perf::track_value(&t.events, beat, &st.chart.tmap))
        .unwrap_or_else(|| track_neutral_value(track))
}

/// 事件：删除（**按文档地址**：图层 + 该图层里的下标）
pub fn del_event_command(line: usize, track: &str, at: EventRef) -> Value {
    json!({ "op": "del_event", "line": line, "layer": at.layer, "track": track, "index": at.index })
}

/// 事件：改值/缓动（`set` 只放**真的变了**的字段；空 `set` 由调用方跳过，别发空命令）
pub fn set_event_command(line: usize, track: &str, at: EventRef, set: Value) -> Value {
    json!({ "op": "set_event", "line": line, "layer": at.layer, "track": track, "index": at.index, "set": set })
}

/// 音符：改字段（`set` 里放文档字段名；拍用 `beat_json`/`milli_beat` 的有理数）
pub fn set_note_command(line: usize, doc_index: usize, set: Value) -> Value {
    json!({ "op": "set_note", "line": line, "index": doc_index, "set": set })
}

/// 开一个事务（整段拖拽 = 一个撤销步）。标签会出现在撤销提示里，所以写人话。
pub fn begin_command(label: &str) -> Value {
    json!({ "op": "begin", "label": label })
}

/// 提交事务
pub fn commit_command() -> Value {
    json!({ "op": "commit" })
}

/// 事务标签（写两遍容易写歪；它们会原样出现在撤销提示里，所以写人话）
pub const DRAG_NOTE_LABEL: &str = "拖动音符";
pub const DRAG_EVENT_LABEL: &str = "调整事件时间";
/// 多选整体平移
pub const MOVE_NOTES_LABEL: &str = "移动选中音符";
pub const MOVE_EVENTS_LABEL: &str = "移动选中事件";
/// Del 一次删掉整个选区（**一个撤销步**）
pub const DELETE_LABEL: &str = "删除选中";

/// 选区是音符还是事件 → 该用哪个事务标签
pub fn move_label(kind: SelKind) -> &'static str {
    match kind {
        SelKind::Notes => MOVE_NOTES_LABEL,
        SelKind::Events => MOVE_EVENTS_LABEL,
    }
}

/// 当前选中的轨道（属性编辑器与事件条都靠它）——只是把 `EditorState` 的字段读出来，
/// 放在这里是为了让"选中了什么"与"发什么命令"挨着，读代码时不用来回跳。
pub fn selected_track(st: &EditorState) -> TrackId {
    st.selected_track
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::EditCore;
    use crate::state;
    use serde_json::json;

    fn exec_ok(c: &mut EditCore, cmd: Value) -> Value {
        let r = c.exec(&cmd);
        assert_eq!(r["ok"], json!(true), "命令失败：{cmd} → {r}");
        r
    }

    /// 一个 tap + 一个 hold（长 4 拍）的谱面，外加一条 moveX 事件
    fn sample() -> (EditCore, EditorState) {
        let mut c = EditCore::new();
        exec_ok(&mut c, json!({"op":"add_note","line":0,"kind":"tap","startBeat":[1,1],"laneX":100.0}));
        exec_ok(&mut c, json!({"op":"add_note","line":0,"kind":"hold",
                               "startBeat":[4,1],"endBeat":[8,1],"laneX":-100.0}));
        exec_ok(&mut c, json!({"op":"add_event","line":0,"layer":0,"track":"moveX",
                               "startBeat":[0,1],"endBeat":[4,1],"startValue":0.0,"endValue":100.0}));
        let st = EditorState::new(state::chart_from_doc(c.doc()));
        (c, st)
    }

    /// 拖动 **tap**：只改 laneX 与起点，不带 endBeat（它本来就不是长条）
    /// 「就位目标」命令：四个键都给/只给一个/一个都不给（后者 ⇒ `None`，按钮据此置灰）
    #[test]
    fn set_target_command_shapes_the_json() {
        let at = crate::doc::Beat::new(4, 1);
        assert!(set_target_command(0, at, &[]).is_none(), "一个键都不给 ⇒ 不发命令");
        let one = set_target_command(2, at, &[("x", 250.0)]).unwrap();
        assert_eq!(one["op"], json!("set_target"));
        assert_eq!(one["line"], json!(2));
        assert_eq!(one["atBeat"], json!([4, 1]), "拍必须写成有理数，不能是浮点");
        assert_eq!(one["target"], json!({"x": 250.0}));
        let all = set_target_command(0, at, &[("x", 1.0), ("y", -2.0), ("angle", 45.0), ("alpha", 0.5)]).unwrap();
        assert_eq!(all["target"], json!({"x": 1.0, "y": -2.0, "angle": 45.0, "alpha": 0.5}));
    }

    #[test]
    fn dragging_a_tap_sets_position_only() {
        let (_c, mut st) = sample();
        st.selected_line = 0;
        let cmd = note_move_command(&st, 0, 250.0, 2.0, None);
        assert_eq!(cmd["op"], json!("set_note"));
        assert_eq!(cmd["line"], json!(0));
        assert_eq!(cmd["index"], json!(0), "命令用的是**文档里**的下标");
        assert_eq!(cmd["set"]["laneX"], json!(250.0));
        assert_eq!(cmd["set"]["startBeat"], json!(st.beat_json(2.0)));
        assert!(cmd["set"].get("endBeat").is_none(), "tap 不该被塞一个 endBeat");
    }

    /// 拖动 **hold**：终点跟着走、**时长不变**（时长由调用方在按下那一刻冻结）
    #[test]
    fn dragging_a_hold_keeps_its_duration() {
        let (_c, mut st) = sample();
        st.selected_line = 0;
        let cmd = note_move_command(&st, 1, -100.0, 10.0, Some(4.0));
        let set = &cmd["set"];
        let start = st.beat_json(10.0);
        let end = st.beat_json(14.0); // 起点 10 拍 + 原时长 4 拍
        assert_eq!(set["startBeat"], json!(start));
        assert_eq!(set["endBeat"], json!(end), "终点必须保持原来的 4 拍时长");
        // 拍是**有理数**（吸附结果），不是浮点
        assert!(start[1] > 0 && end[1] > 0);
    }

    /// 拖事件头/尾：edge 映射成 "start"/"end"，图层/下标来自**文档地址**
    #[test]
    fn resizing_an_event_maps_edge_track_and_layer() {
        use crate::doc::EventRef;
        use crate::state::EventEdge;
        let (_c, mut st) = sample();
        st.selected_line = 0;
        let cmd = event_resize_command(&st, TrackId::MoveX, EventRef::new(0, 0), EventEdge::End, 6.0);
        assert_eq!(cmd["op"], json!("resize_event"));
        assert_eq!(cmd["track"], json!("moveX"));
        assert_eq!(cmd["edge"], json!("end"));
        assert_eq!(cmd["layer"], json!(0));
        assert_eq!(cmd["toBeat"], json!(st.beat_json(6.0)));

        // 轨道与图层都跟着**文档地址**走（不是写死的 0/当前轨道）
        let cmd = event_resize_command(&st, TrackId::Alpha, EventRef::new(2, 3), EventEdge::Start, 1.0);
        assert_eq!(cmd["track"], json!("alpha"));
        assert_eq!(cmd["layer"], json!(2), "多层文档里图层必须是真图层");
        assert_eq!(cmd["edge"], json!("start"));
        assert_eq!(cmd["index"], json!(3));
    }

    /// 双击放置：落在**已吸附**的 laneX/拍 上，默认 tap
    #[test]
    fn placing_a_note_uses_the_snapped_position() {
        let (_c, mut st) = sample();
        st.selected_line = 0;
        let cmd = place_note_command(&st, NoteKind::Tap, -337.5, 3.25, None);
        assert_eq!(cmd["op"], json!("add_note"));
        assert_eq!(cmd["kind"], json!("tap"));
        assert_eq!(cmd["laneX"], json!(-337.5));
        assert_eq!(cmd["startBeat"], json!(st.beat_json(3.25)));
        assert_eq!(cmd["line"], json!(0));
    }

    /// 快速放置：四种 kind 都要写对；**只有 hold 带 endBeat**
    #[test]
    fn place_note_carries_the_kind_and_only_hold_gets_an_end() {
        let (_c, mut st) = sample();
        st.selected_line = 0;
        for (kind, name) in [
            (NoteKind::Tap, "tap"),
            (NoteKind::Flick, "flick"),
            (NoteKind::Drag, "drag"),
        ] {
            let cmd = place_note_command(&st, kind, 120.0, 2.0, Some(9.0));
            assert_eq!(cmd["kind"], json!(name));
            assert!(
                cmd.get("endBeat").is_none(),
                "{name} 不该有 endBeat（给了也要丢掉）：{cmd}"
            );
        }
        // hold：终点按拍网格写进命令
        let cmd = place_note_command(&st, NoteKind::Hold, -60.0, 4.0, Some(7.0));
        assert_eq!(cmd["kind"], json!("hold"));
        assert_eq!(cmd["startBeat"], json!(st.beat_json(4.0)));
        assert_eq!(cmd["endBeat"], json!(st.beat_json(7.0)));
        // 没给终点（不该发生，但别发出注定被拒的命令）：兜底成一拍
        let cmd = place_note_command(&st, NoteKind::Hold, 0.0, 4.0, None);
        assert_eq!(cmd["endBeat"], json!(st.beat_json(5.0)));

        // 端到端：命令真的能在文档里放出一个 hold
        let (mut c, st2) = sample();
        let cmd = place_note_command(&st2, NoteKind::Hold, 0.0, 20.0, Some(22.0));
        let r = exec_ok(&mut c, cmd);
        assert_eq!(r["ok"], json!(true));
        let l = &c.doc().judge_lines[0];
        let last = l.notes.last().expect("刚放的音符");
        assert_eq!(last.kind, NoteKind::Hold);
        assert_eq!(last.end_beat().to_f64(), 22.0);
    }

    /// 事件块放置：轨道/拍/平段值都要对；初值优先"此刻的值"，空轨道用中性值
    #[test]
    fn place_event_carries_track_span_and_value() {
        let (mut c, mut st) = sample();
        st.selected_line = 0;
        st.selected_track = TrackId::MoveX;
        // 样例里 moveX 在 0..4 拍从 0 渐变到 100 ⇒ 2 拍处此刻的值是 50
        let v = new_event_value(&st, TrackId::MoveX, 2.0);
        assert!((v - 50.0).abs() < 1e-6, "应取此刻的值，实际 {v}");

        let cmd = place_event_command(&st, TrackId::MoveX, 0, 8.0, 12.0, v);
        assert_eq!(cmd["op"], json!("add_event"));
        assert_eq!(cmd["track"], json!("moveX"));
        assert_eq!(cmd["layer"], json!(0));
        assert_eq!(cmd["startBeat"], json!(st.beat_json(8.0)));
        assert_eq!(cmd["endBeat"], json!(st.beat_json(12.0)));
        assert_eq!(cmd["startValue"], json!(v));
        assert_eq!(cmd["endValue"], json!(v), "新事件是**平段**，不该自带跳变");
        assert_eq!(cmd["easing"], json!("linear"));

        // 端到端：命令真的放进文档，且这条轨道现在有 2 条事件
        let r = exec_ok(&mut c, cmd);
        assert_eq!(r["ok"], json!(true));
        let l = &c.doc().judge_lines[0];
        let evs = l.layers[0].track("moveX").unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs.last().unwrap().start.to_f64(), 8.0);

        // 空轨道 ⇒ 中性值（透明度 1、流速 10、移动 0）
        assert_eq!(new_event_value(&st, TrackId::Alpha, 0.0), 1.0);
        assert_eq!(new_event_value(&st, TrackId::Speed, 0.0), 10.0);
        assert_eq!(new_event_value(&st, TrackId::Rotate, 0.0), 0.0);
        assert_eq!(track_neutral_value(TrackId::MoveY), 0.0);
        // 空轨道上真的能放出一条事件（不会因为"没有值"而失败）
        let cmd = place_event_command(&st, TrackId::Alpha, 0, 0.0, 2.0, 1.0);
        exec_ok(&mut c, cmd);
        assert_eq!(c.doc().judge_lines[0].layers[0].track("alpha").unwrap().len(), 1);
    }

    /// **一组音符整体平移**：相对偏移不变（这是"统一拖动位置"的全部意义），hold 保持自己的时长
    #[test]
    fn moving_a_group_keeps_relative_offsets_and_hold_lengths() {
        let (mut c, mut st) = sample();
        st.selected_line = 0;
        // 视图序：[0] = 1 拍的 tap，[1] = 4 拍的 hold（长 4 拍）
        st.select_notes([0, 1]);
        let grab = grab_selection(&st, &GrabIntent::selection(&st, 0.0, 0.0)).expect("抓手");
        assert_eq!(grab.notes.len(), 2);
        assert_eq!(grab.kind, SelKind::Notes);
        let cmds = move_grab_commands(&st, &grab, 200.0, 2.0);
        assert_eq!(cmds.len(), 2);
        // tap：起点 1 → 3，横向 100 + Δ200 = 300（参数是**位移**，不是绝对位置）
        assert_eq!(cmds[0]["set"]["startBeat"], json!(st.beat_json(3.0)));
        assert_eq!(cmds[0]["set"]["laneX"], json!(300.0));
        // hold：起点 4 → 6、终点 8 → 10（时长仍是 4 拍）
        assert_eq!(cmds[1]["set"]["startBeat"], json!(st.beat_json(6.0)));
        assert_eq!(cmds[1]["set"]["endBeat"], json!(st.beat_json(10.0)));

        // 端到端：整组真的动了，且相对间隔（3 拍）没变
        for cmd in cmds {
            exec_ok(&mut c, cmd);
        }
        let notes = &c.doc().judge_lines[0].notes;
        assert_eq!(notes[0].start.to_f64(), 3.0);
        assert_eq!(notes[1].start.to_f64(), 6.0);
        assert_eq!(notes[1].end_beat().to_f64(), 10.0);
    }

    /// 组拖动的位移：**锚吸附**（写回文档的拍仍是格点），负拍整组一起夹（不压扁相对间隔）
    #[test]
    fn grab_delta_snaps_the_anchor_and_clamps_the_whole_group() {
        let (_c, mut st) = sample();
        st.selected_line = 0;
        st.select_notes([0, 1]);
        // 按下时指针在 (0, 1)，抓手原点 = 选区锚（最小的那个 = tap，起点 1 拍 / lane 0）
        let grab = grab_selection(&st, &GrabIntent::selection(&st, 0.0, 1.0)).expect("抓手");
        // 往左下拖：锚**跟着指针走**（按下时指针在 lane 0 / 拍 1，锚也在那儿）
        // ⇒ 目标 = 锚原点 + (指针现在 − 按下时指针)，再吸附
        let (d_lane, d_beat) = grab_delta(&st, &grab, -37.0, 0.4);
        let want_lane = st.snap_lane(grab.anchor_lane + (-37.0 - grab.press_lane)) - grab.anchor_lane;
        let want_beat = st.snap_beat(grab.anchor_beat + (0.4 - grab.press_beat)) - grab.anchor_beat;
        assert!((d_lane - want_lane).abs() < 1e-6, "{d_lane} vs {want_lane}");
        assert!((d_beat - want_beat).abs() < 1e-9, "{d_beat} vs {want_beat}");
        // 锚落回**格点**（写回文档的拍/坐标必须是格点上的有理数）
        let step = st.grid.h_step_rpe();
        let k = (grab.anchor_lane + d_lane + 675.0) / step;
        assert!((k - k.round()).abs() < 1e-3, "横向应落在格点上，k={k}");
        // 拖到很大的负拍：整组夹在"最小起点 = 0"，而不是各自夹（那样相对间隔会被压扁）
        let (_, d_beat) = grab_delta(&st, &grab, 0.0, -100.0);
        assert!((d_beat + 1.0).abs() < 1e-9, "应整组退到起点 0，实际 Δ={d_beat}");
    }

    /// **事件的"最近合法位置"**（用户选定：仿 kdenlive）——
    /// 铺满的轨道上拖不动；指针拖得够远、越过了障碍，就**跳到障碍另一侧的空档里**
    #[test]
    fn event_group_move_jumps_across_an_obstacle_when_there_is_room() {
        // 选中 [0,4)，障碍 [4,8)：Δ ∈ (0,8) 会重叠 ⇒ 合法的是 Δ=0（贴住）与 Δ≥8（整个跨过去）
        let selected = [(0.0, 4.0)];
        let blocked = [(4.0, 8.0)];
        assert_eq!(nearest_free_delta(&selected, &blocked, 0.0, 0.0), 0.0, "原地合法");
        assert_eq!(nearest_free_delta(&selected, &blocked, 0.5, 0.0), 0.0, "贴着邻居停住");
        assert_eq!(nearest_free_delta(&selected, &blocked, 3.0, 0.0), 0.0, "近的一侧是原地");
        assert_eq!(nearest_free_delta(&selected, &blocked, 6.0, 0.0), 8.0, "远的一侧更近 ⇒ 跨过去");
        assert_eq!(nearest_free_delta(&selected, &blocked, 20.0, 0.0), 20.0, "另一侧空着随便走");
        // 底下不许到负拍
        assert_eq!(nearest_free_delta(&selected, &blocked, -5.0, -2.0), -2.0);
        // 有空档：障碍在 [6,8)，选中块 [0,2) ⇒ Δ ∈ (4,8) 不行，[0,4] 随便走
        let selected = [(0.0, 2.0)];
        let blocked = [(6.0, 8.0)];
        assert_eq!(nearest_free_delta(&selected, &blocked, 3.0, 0.0), 3.0);
        assert_eq!(nearest_free_delta(&selected, &blocked, 5.0, 0.0), 4.0, "最多贴到邻居边界");
        assert_eq!(nearest_free_delta(&selected, &blocked, 7.9, 0.0), 8.0, "越过去");
        // 没有障碍 / 空选区：原样（只受地板限制）
        assert_eq!(nearest_free_delta(&selected, &[], 5.0, 0.0), 5.0);
        assert_eq!(nearest_free_delta(&[], &blocked, 5.0, 0.0), 5.0);
    }

    /// 事件组平移**端到端**：整块挪走、时长不变；铺满时挪不动（不制造重叠）
    #[test]
    fn moving_an_event_group_does_not_create_overlaps() {
        let (mut c, _st) = sample();
        // 再加一条 [4,8) 的 moveX 事件 ⇒ 轨道铺满 [0,8)
        exec_ok(&mut c, json!({"op":"add_event","line":0,"layer":0,"track":"moveX",
                               "startBeat":[4,1],"endBeat":[8,1],"startValue":100.0,"endValue":0.0}));
        let mut st = EditorState::new(state::chart_from_doc(c.doc()));
        st.selected_line = 0;
        st.select_event(TrackId::MoveX, 0);
        let grab = grab_selection(&st, &GrabIntent::selection(&st, 0.0, 0.0)).expect("抓手");
        assert_eq!(grab.kind, SelKind::Events);
        // 请求 +1 拍：会盖住邻居 ⇒ 最近合法位置是原地
        let (_, d) = grab_delta(&st, &grab, 0.0, 1.0);
        assert_eq!(d, 0.0, "铺满的轨道上不该挪出重叠来");
        // 请求 +12 拍：整个跨过邻居之后（没有别的障碍）⇒ 允许
        let (_, d) = grab_delta(&st, &grab, 0.0, 12.0);
        assert_eq!(d, 12.0);
        let cmds = move_grab_commands(&st, &grab, 0.0, d);
        for cmd in cmds {
            exec_ok(&mut c, cmd);
        }
        let evs = c.doc().judge_lines[0].layers[0].track("moveX").unwrap();
        let moved = evs.iter().find(|e| e.start.to_f64() == 12.0).expect("整块挪到 12");
        assert_eq!(moved.end.to_f64(), 16.0, "时长不变");
        // 没有制造出重叠
        assert!(c.overlaps().is_empty(), "{:?}", c.overlaps());
    }

    /// **Del：整批删除 = 一个撤销步**，且同一张表里**按下标降序**发
    /// （删掉第 3 条之后原来的第 4 条会变成第 3 条 —— 顺序错了就删错东西）
    #[test]
    fn deleting_a_selection_is_one_undo_step_in_descending_order() {
        let mut c = EditCore::new();
        // 故意按"乱序"插入：文档下标 0/1/2 对应拍 4/2/6 ⇒ 视图序 = [1, 0, 2]
        for b in [4, 2, 6] {
            exec_ok(&mut c, json!({"op":"add_note","line":0,"kind":"tap",
                                   "startBeat":[b,1],"laneX":0.0}));
        }
        let mut st = EditorState::new(state::chart_from_doc(c.doc()));
        st.selected_line = 0;
        st.select_notes([0, 2]); // 视图 0 = 拍 2 = 文档下标 1；视图 2 = 拍 6 = 文档下标 2
        let cmds = delete_selection_commands(&st);
        assert_eq!(cmds[0]["op"], json!("begin"), "整批删除是一个撤销步");
        assert_eq!(cmds[0]["label"], json!(DELETE_LABEL));
        assert_eq!(cmds.last().unwrap()["op"], json!("commit"));
        let dels: Vec<i64> = cmds
            .iter()
            .filter(|c| c["op"] == json!("del_note"))
            .map(|c| c["index"].as_i64().unwrap())
            .collect();
        assert_eq!(dels, vec![2, 1], "同一张表里必须降序：{cmds:?}");
        for cmd in &cmds {
            exec_ok(&mut c, cmd.clone());
        }
        let notes = &c.doc().judge_lines[0].notes;
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].start.to_f64(), 4.0, "留下的应是拍 4 那个");
        // 一次撤销把两条都带回来
        exec_ok(&mut c, json!({"op":"undo"}));
        assert_eq!(c.doc().judge_lines[0].notes.len(), 3);
    }

    /// **卷入重叠的事件禁止移动**（用户要求）：判据是两两比较（只查相邻会漏第三种情形），
    /// 相接不算重叠，没选中东西时不禁用。
    #[test]
    fn events_involved_in_an_overlap_cannot_be_dragged() {
        let mut c = EditCore::new();
        let mut doc = c.doc().clone();
        let tr = doc.judge_lines[0].layers[0].track_mut("alpha").unwrap();
        tr.clear();
        // 相接的一对 + 一块压在第二块上的
        for (a, b) in [(0.0, 4.0), (4.0, 8.0), (6.0, 10.0)] {
            tr.push(crate::doc::Event::new(
                crate::doc::Beat::new((a * 4.0) as i64, 4),
                crate::doc::Beat::new((b * 4.0) as i64, 4),
                json!(1.0),
                json!(1.0),
                "linear",
            ));
        }
        c.replace_doc(doc);
        let mut st = EditorState::new(state::chart_from_doc(c.doc()));
        st.selected_line = 0;
        // 视图序按起拍：[0,4) / [4,8) / [6,10)。
        // 只选第一块 ⇒ 它和谁都不重叠 ⇒ 可以拖
        st.select_event(TrackId::Alpha, 0);
        assert!(!selection_has_event_overlap(&st), "相接不算重叠");
        assert!(!event_drag_disabled(&st, &st.selection().events().collect::<Vec<_>>()));
        // 只选第二块 ⇒ 它和第二块不重叠、和**第三块**才重叠（只查相邻会漏掉这一对）
        st.select_event(TrackId::Alpha, 1);
        assert!(
            selection_has_event_overlap(&st),
            "第二块与第三块重叠：两两比较才查得到"
        );
        // 选中"重叠的一方"就禁用；没选中东西时不拦
        st.clear_selection();
        assert!(!selection_has_event_overlap(&st));
        assert!(!event_drag_disabled(&st, &[]));
    }

    /// Del 删事件：按**文档地址**（图层 + 图层内下标）走，多层文档里不会删错
    #[test]
    fn deleting_events_uses_the_document_address() {
        let mut c = EditCore::new();
        // 两层各一条（`add_event` 只能落在已存在的层上 ⇒ 直接造文档）：
        // 合并视图序按起拍排 ⇒ 视图 0 = 图层 0 的那条、视图 1 = 图层 1 的那条
        let mut doc = c.doc().clone();
        doc.judge_lines[0].layers.push(crate::doc::Layer::default());
        doc.judge_lines[0].layers[0].track_mut("alpha").unwrap().push(crate::doc::Event::new(
            crate::doc::Beat::zero(),
            crate::doc::Beat::new(4, 1),
            json!(1.0),
            json!(1.0),
            "linear",
        ));
        doc.judge_lines[0].layers[1].track_mut("alpha").unwrap().push(crate::doc::Event::new(
            crate::doc::Beat::new(2, 1),
            crate::doc::Beat::new(6, 1),
            json!(0.5),
            json!(0.5),
            "linear",
        ));
        c.replace_doc(doc);
        let mut st = EditorState::new(state::chart_from_doc(c.doc()));
        st.selected_line = 0;
        // 视图序 = [(layer0,idx0) 起拍 0, (layer1,idx0) 起拍 2]
        st.select_event(TrackId::Alpha, 1);
        let cmds = delete_selection_commands(&st);
        let del = cmds.iter().find(|c| c["op"] == json!("del_event")).expect("应有删除");
        assert_eq!(del["layer"], json!(1), "合并下标 1 落在**图层 1**，不是图层 0");
        assert_eq!(del["index"], json!(0), "图层 1 里的第 0 条");
        for cmd in &cmds {
            exec_ok(&mut c, cmd.clone());
        }
        assert_eq!(c.doc().judge_lines[0].layers[0].track("alpha").unwrap().len(), 1);
        assert_eq!(c.doc().judge_lines[0].layers[1].track("alpha").unwrap().len(), 0);
    }

    /// 事务：一段拖拽用 begin/commit 包住 ⇒ 撤销一步（标签是人话，会出现在撤销提示里）
    #[test]
    fn a_drag_is_wrapped_in_one_transaction() {
        let b = begin_command(DRAG_NOTE_LABEL);
        assert_eq!(b["op"], json!("begin"));
        assert_eq!(b["label"], json!("拖动音符"));
        assert_eq!(commit_command()["op"], json!("commit"));
        assert_eq!(begin_command(DRAG_EVENT_LABEL)["label"], json!("调整事件时间"));
        assert_ne!(DRAG_NOTE_LABEL, DRAG_EVENT_LABEL);
    }

    /// 端到端：把拖动命令真的喂给核心 ⇒ 文档里的音符确实动了（并且 hold 时长没变）
    #[test]
    fn drag_commands_actually_move_the_document() {
        let (mut c, mut st) = sample();
        st.selected_line = 0;
        let before = c.doc().judge_lines[0].notes[1].clone();
        let dur_before = before.end_beat().to_f64() - before.start.to_f64();
        exec_ok(&mut c, begin_command(DRAG_NOTE_LABEL));
        exec_ok(&mut c, note_move_command(&st, 1, -300.0, 10.0, Some(4.0)));
        exec_ok(&mut c, commit_command());
        let after = &c.doc().judge_lines[0].notes[1];
        assert_eq!(after.lane_x, -300.0);
        assert_eq!(after.start.to_f64(), 10.0);
        let dur_after = after.end_beat().to_f64() - after.start.to_f64();
        assert!((dur_after - dur_before).abs() < 1e-9, "{dur_before} → {dur_after}");
        // 一次撤销就把整段拖拽退回去
        exec_ok(&mut c, json!({"op":"undo"}));
        let back = &c.doc().judge_lines[0].notes[1];
        assert_eq!(back.lane_x, before.lane_x);
        assert_eq!(back.start.to_f64(), before.start.to_f64());
    }
}

#[cfg(test)]
mod mask_command_tests {
    use super::*;
    use crate::doc::Beat;
    use crate::state::MaskChannel;

    /// 遮蔽区那几条命令的**拼法**（界面只调它们，所以 JSON 键名只在这里出现一次）。
    ///
    /// 钉住的是键名与**拍的形状**：拍一律既约分数 `[n, d]`（命令语言唯一的拍形状，
    /// `cmd::parse_beat` 吃它），绝不放浮点 —— 界面上拖出来的 `1/3` 拍必须是 `[1,3]`。
    #[test]
    fn mask_commands_use_fractional_beats_and_document_keys() {
        let start = Beat::new(1, 3);
        let end = Beat::new(7, 6);
        let add = add_zone_command(start);
        assert_eq!(add["op"], serde_json::json!("add_zone"));
        assert_eq!(add["startBeat"], serde_json::json!([1, 3]));

        let ev = add_mask_event_command(2, MaskChannel::Y3, start, end);
        assert_eq!(ev["op"], serde_json::json!("add_zone_event"));
        assert_eq!(ev["zone"], serde_json::json!(2));
        assert_eq!(ev["track"], serde_json::json!("y3"), "轨道名用文档键");
        assert_eq!(ev["startBeat"], serde_json::json!([1, 3]));
        assert_eq!(ev["endBeat"], serde_json::json!([7, 6]));
        // **值不在这条命令里**：缺省值由核心取"该通道此刻的值"（放下一刻不跳变）
        assert!(ev.get("startValue").is_none());

        let set = set_mask_event_command(0, MaskChannel::Active, 3, serde_json::json!({"startValue": true}));
        assert_eq!(set["op"], serde_json::json!("set_zone_event"));
        assert_eq!(set["track"], serde_json::json!("active"));
        assert_eq!(set["index"], serde_json::json!(3));
        assert_eq!(set["set"]["startValue"], serde_json::json!(true), "active 的值是布尔");

        assert_eq!(del_mask_event_command(1, MaskChannel::X2, 0)["op"], serde_json::json!("del_zone_event"));
        assert_eq!(del_zone_command(4)["index"], serde_json::json!(4));
        assert_eq!(set_zone_command(0, serde_json::json!({"name": "x"}))["op"], serde_json::json!("set_zone"));
    }

    /// **草稿态**：第一条编辑要带上 `add_zone`，且**整段是一个事务**（一次撤销全回去）
    #[test]
    fn a_draft_edit_creates_the_zone_inside_the_same_transaction() {
        let start = Beat::new(8, 1);
        let edit = add_mask_event_command(0, MaskChannel::X1, Beat::new(8, 1), Beat::new(12, 1));
        let seq = zone_draft_edits(true, start, vec![edit.clone()]);
        let ops: Vec<&str> = seq.iter().filter_map(|c| c["op"].as_str()).collect();
        assert_eq!(ops, vec!["begin", "add_zone", "add_zone_event", "commit"], "{seq:?}");
        assert_eq!(seq[1]["startBeat"], serde_json::json!([8, 1]), "起点用界的吸附结果");
        // 已经有区（或不在编辑模式）：原样返回那条编辑，不额外建区
        assert_eq!(zone_draft_edits(false, start, vec![edit.clone()]), vec![edit.clone()]);
        assert!(zone_draft_edits(true, start, Vec::new()).is_empty(), "没有编辑就什么都不做");
        // 拖动开始：**只建一次区**
        let begin = zone_draft_begin(true, start, "拖动遮蔽区事件");
        let ops: Vec<&str> = begin.iter().filter_map(|c| c["op"].as_str()).collect();
        assert_eq!(ops, vec!["begin", "add_zone"], "{begin:?}");
        let begin = zone_draft_begin(false, start, "拖动遮蔽区事件");
        assert_eq!(begin.len(), 1);
        assert_eq!(begin[0]["op"], serde_json::json!("begin"));
    }

    // ---- 跨度规则（R 起稿 / 属性编辑器 / 核心命令共用的那一份判据）----

    fn ev(a: f64, b: f64) -> crate::doc::Event {
        crate::doc::Event::new(
            Beat::new((a * 4.0) as i64, 4),
            Beat::new((b * 4.0) as i64, 4),
            serde_json::json!(0.0),
            serde_json::json!(0.0),
            "linear",
        )
    }

    /// 起点判据：只有"已经有一块**正好从这一点开始**"才不许放（起点落在别人里面是合法的插入）
    #[test]
    fn a_start_is_refused_only_when_a_block_starts_exactly_there() {
        let list = vec![ev(8.0, 12.0)];
        assert!(mask_can_start(&list, Beat::new(6, 1)));
        assert!(mask_can_start(&list, Beat::new(9, 1)), "落在块里面：允许（会裁前一块）");
        assert!(!mask_can_start(&list, Beat::new(8, 1)), "起点重合 = 重叠");
        assert!(!mask_can_start(&list, Beat::new(16, 2)), "8/1 与 16/2 是同一拍");
        assert!(mask_can_start(&[], Beat::zero()));
    }

    /// 终点上限 = **下一块的起点**（起点早于我们的那些不算 —— 它们会被裁掉）
    #[test]
    fn the_end_limit_is_the_next_blocks_start() {
        let list = vec![ev(0.0, 4.0), ev(20.0, 24.0)];
        assert_eq!(mask_end_limit(&list, Beat::new(8, 1)), Some(Beat::new(20, 1)));
        // 落在某块里面：上限看**再往后**那一块
        assert_eq!(mask_end_limit(&list, Beat::new(1, 1)), Some(Beat::new(20, 1)));
        assert_eq!(mask_end_limit(&list, Beat::new(24, 1)), None, "后面没有块 = 不限");
    }

    /// 缺省终点：起点 + 1 拍，**不越过下一块**；起点重合或空档塞不下 ⇒ `None`
    #[test]
    fn the_default_end_stops_at_the_next_block() {
        assert_eq!(mask_default_end(&[], Beat::new(8, 1)), Some(Beat::new(9, 1)));
        // 下一块在 8.5：缩到它起点
        let list = vec![ev(8.5, 12.0)];
        assert_eq!(mask_default_end(&list, Beat::new(8, 1)), Some(Beat::new(17, 2)));
        assert_eq!(mask_default_end(&list, Beat::new(17, 2)), None, "起点重合 ⇒ 放不下");
        // 空档比一个格点还窄：给一条细块（能拖，不硬塞一个重叠）
        let list = vec![ev(8.4, 12.0)]; // 下一块起于 33/4 拍
        assert_eq!(mask_default_end(&list, Beat::new(8, 1)), Some(Beat::new(33, 4)));
        // 起点在**已结束**的块之后：不受它影响
        let list = vec![ev(0.0, 4.0), ev(20.0, 24.0)];
        assert_eq!(mask_default_end(&list, Beat::new(8, 1)), Some(Beat::new(9, 1)));
    }

    /// 重叠判据：半个区间相交即重叠（相接不算）—— `set_zone_event` / `resize_zone_event` 走它
    #[test]
    fn overlap_is_half_open_interval_intersection() {
        let list = vec![ev(0.0, 4.0), ev(8.0, 12.0)];
        // 把第 0 块的终点拉到 10 ⇒ 压住了 [8,12) 那一块
        let mut cur = list[0].clone();
        cur.end = Beat::new(10, 1);
        assert_eq!(mask_overlap(&cur, 0, &list), Some(1));
        // 正好停在 8：相接，不算重叠
        cur.end = Beat::new(8, 1);
        assert_eq!(mask_overlap(&cur, 0, &list), None);
        // 自己不算自己
        assert_eq!(mask_overlap(&list[0], 0, &list), None);
    }
}
