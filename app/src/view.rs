// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **视图模型**：给界面用的派生快照（"当前该显示什么"），全部是**纯函数**。
//!
//! 为什么放在库里而不是 `main.rs` 里：
//! 1. 它们是**纯派生**（输入是 `EditorState` / `Document` / `Chart`，输出是一堆字符串与数字），
//!    与 egui 无关 —— 放这儿就能**单测**："选中第 1 条线的第 2 个音符之后，右侧那一栏该显示什么"
//!    可以断言，而不必开窗口截图看；
//! 2. `main.rs` 只该留"怎么画、点了之后发哪条命令"。左侧判定线列表的一行、右侧属性检查器的
//!    快照都属于"显示什么"，不属于"怎么画"。
//!
//! 纪律：这里**只读**，不改文档、不发命令。可编辑字段（[`NoteEdit`] / [`EventEdit`]）只是把
//! 文档里的原始值**抄出来**给界面当"输入框的初值"，用户改完仍然要发命令回 `EditCore`。

use crate::doc::{Document, NoteKind};
use crate::perf;
use crate::state::{self, EditorState, TrackId};

/// 左侧「判定线」列表的一行（视图缓存：只在 structure/属性变时重建）
#[derive(Clone, Debug, PartialEq)]
pub struct LineRow {
    /// 视图序（列表里第几行）
    pub view: usize,
    /// doc 下标
    pub doc: usize,
    pub name: String,
    pub z_order: i32,
    pub is_cover: bool,
    pub notes: usize,
    pub events: usize,
}

/// 判定线列表的行快照。视图序与 doc 下标**分开**：列表按 zOrder 排过，
/// 于是"第 3 行"通常不是"3 号线"——发命令必须用后者。
pub fn line_rows_of(chart: &state::Chart) -> Vec<LineRow> {
    chart
        .lines
        .iter()
        .enumerate()
        .map(|(view, l)| LineRow {
            view,
            doc: l.index,
            name: l.name.clone(),
            z_order: l.z_order,
            is_cover: l.is_cover,
            notes: l.note_count(),
            events: l.event_count(),
        })
        .collect()
}

/// 检查器快照（线优先：先线，再轨道，再事件，最后音符）
#[derive(Clone, Debug)]
pub struct Inspector {
    pub line_index: usize,
    pub name: String,
    pub z_order: i32,
    pub is_cover: bool,
    pub bpm_factor: f32,
    /// 可编辑字段：都是**文档数据**，改动一律发命令（见编辑器的 apply 逻辑）
    pub note_edit: Option<NoteEdit>,
    pub event_edit: Option<EventEdit>,
    /// 「就位目标（块末）」那一组的**草稿**（x/y/角度/透明度）。
    ///
    /// 为什么草稿放在快照里、而不是像别的字段那样"当帧算完就发命令"：那一组是**两步**交互
    /// （改几个数 → 按「一次写入」），输入的值必须活过一帧；而 `DragValue(update_while_editing=false)`
    /// 只在回车那一下把值写进 `&mut`，下一帧若还从文档重取就丢了。快照在广播时重建 ⇒
    /// 文档真变了（包括我们自己写成功之后）草稿自动跟到新值上 —— 这正是要的语义。
    pub target: TargetEdit,
    pub notes: usize,
    pub events: usize,
    pub perf: perf::LinePerf,
    pub track: TrackId,
    pub track_events: usize,
    pub track_value: Option<f64>,
    pub event: Option<EventView>,
    pub note: Option<NoteView>,
}

/// 「就位目标」草稿：块末那一刻线该在哪儿（x/y 是 RPE 单位、angle 是度、alpha 0–1）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TargetEdit {
    pub x: f64,
    pub y: f64,
    pub angle: f64,
    pub alpha: f64,
}

impl Default for TargetEdit {
    fn default() -> Self {
        // 与"空轨道"的默认口径一致（move/rotate 0、alpha 1），只在没选中事件时用得到
        Self { x: 0.0, y: 0.0, angle: 0.0, alpha: 1.0 }
    }
}

/// 选中事件的**块末**拍（「就位目标」的锚点）。没选中事件时 `None`。
fn event_edit_end_beat(event: &Option<EventView>) -> Option<f64> {
    event.as_ref().map(|e| e.end_beat)
}

/// 当前播放时刻的事件快照（只读展示）
#[derive(Clone, Debug)]
pub struct EventView {
    pub start_beat: f64,
    pub end_beat: f64,
    pub start_sec: f64,
    pub end_sec: f64,
    pub start_value: String,
    pub end_value: String,
    pub easing: String,
}

/// 当前选中音符的快照（只读展示）
#[derive(Clone, Debug)]
pub struct NoteView {
    pub doc_index: usize,
    pub time: f64,
    pub end: f64,
    pub lane_x: f32,
    pub kind: &'static str,
    /// 线变换之后的屏幕位置（RPE 坐标）
    pub screen: [f32; 2],
}

/// 音符的可编辑字段（属性编辑器的输入）
#[derive(Clone, Debug)]
pub struct NoteEdit {
    pub kind: String,
    pub start_beat: f64,
    pub end_beat: Option<f64>,
    pub lane_x: f32,
    pub alpha: u16,
    pub is_fake: bool,
    pub speed: f32,
    pub width_scale: f32,
    pub y_offset: f32,
}

/// 事件的可编辑字段
#[derive(Clone, Debug)]
pub struct EventEdit {
    pub start_beat: f64,
    pub end_beat: f64,
    pub start_value: f64,
    pub end_value: f64,
    pub easing: String,
}

/// 当前选中对象 → 检查器快照。没有选中判定线时返回 `None`（右侧那一栏就空着）。
pub fn inspector_of(st: &EditorState, doc: &Document) -> Option<Inspector> {
    let line = st.selected()?;
    let perf = line.perf(&st.chart.tmap, st.playhead);
    let track = line.track(st.selected_track);
    let event = st
        .selected_event()
        .and_then(|i| track.events.get(i))
        .map(|e| EventView {
            start_beat: e.start.to_f64(),
            end_beat: e.end.to_f64(),
            start_sec: st.chart.tmap.sec(e.start.to_f64()),
            end_sec: st.chart.tmap.sec(e.end.to_f64()),
            start_value: e.start_value.to_string(),
            end_value: e.end_value.to_string(),
            easing: e.easing.clone(),
        });
    let note = st
        .selected_note()
        .and_then(|i| line.notes.get(i))
        .map(|n| NoteView {
            doc_index: n.doc_index,
            time: n.time,
            end: n.end,
            lane_x: n.lane_x,
            kind: n.kind.label(),
            // 屏幕位置（线变换之后）——用来证明"音符跟着线走"
            screen: perf.apply([n.lane_x, 0.0]),
        });
    // 可编辑字段取自**文档原始数据**（alpha/isFake/speed/widthScale/yOffset 只存在文档里）
    let note_edit = st
        .selected_note()
        .and_then(|i| line.notes.get(i))
        .and_then(|n| {
            doc.judge_lines
                .get(line.index)
                .and_then(|l| l.notes.get(n.doc_index))
        })
        .map(|d| NoteEdit {
            kind: d.kind.as_str().to_owned(),
            start_beat: d.start.to_f64(),
            end_beat: if d.kind == NoteKind::Hold {
                Some(d.end_beat().to_f64())
            } else {
                None
            },
            lane_x: d.lane_x,
            alpha: d.alpha,
            is_fake: d.is_fake,
            speed: d.speed,
            width_scale: d.width_scale,
            y_offset: d.y_offset,
        });
    // 「就位目标」的草稿初值 = **选中事件块末**那一刻的线状态（不是播放头那一刻：
    // 那一组说的就是"块末就位"，初值给播放头会让人以为改的是别处）
    let target = event_edit_end_beat(&event)
        .map(|b| line.perf(&st.chart.tmap, st.chart.tmap.sec(b)))
        .map(|p| TargetEdit { x: p.x as f64, y: p.y as f64, angle: p.rotate_deg as f64, alpha: p.alpha as f64 })
        .unwrap_or_default();
    let event_edit = event.as_ref().and_then(|ev| {
        // 事件值可能是非数值（颜色/字符串轨道）：不可编辑数值时就退化为只读
        let idx = st.selected_event().unwrap_or(0);
        let sv = track.events.get(idx).and_then(|e| e.start_value.as_f64());
        let ev2 = track.events.get(idx)?;
        sv.map(|sv| EventEdit {
            start_beat: ev.start_beat,
            end_beat: ev.end_beat,
            start_value: sv,
            end_value: ev2.end_value.as_f64().unwrap_or(0.0),
            easing: ev.easing.clone(),
        })
    });
    Some(Inspector {
        line_index: line.index,
        name: line.name.clone(),
        z_order: line.z_order,
        is_cover: line.is_cover,
        bpm_factor: line.bpm_factor,
        notes: line.notes.len(),
        events: line.event_count(),
        perf,
        track: st.selected_track,
        track_events: track.events.len(),
        note_edit,
        event_edit,
        target,
        track_value: track
            .events
            .first()
            .and_then(|_| perf::track_value(st.selected_track.key(), &track.events, st.chart.tmap.beat(st.playhead))),
        event,
        note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::EditCore;
    use serde_json::json;

    /// 造一个"两条线 + 两个音符 + 一条移动事件"的小文档。
    /// **走命令路径**（`EditCore::exec`）：测试用的文档与真实编辑出来的一模一样。
    fn sample() -> EditCore {
        let mut c = EditCore::new();
        c.exec(&json!({"op": "add_note", "line": 0, "kind": "tap",
                       "startBeat": [4, 1], "laneX": 100.0}));
        c.exec(&json!({"op": "add_note", "line": 0, "kind": "hold",
                       "startBeat": [8, 1], "endBeat": [12, 1], "laneX": -200.0,
                       "set": {"alpha": 128, "isFake": true, "speed": 2.5}}));
        c.exec(&json!({"op": "add_event", "line": 0, "layer": 0, "track": "moveX",
                       "startBeat": [0, 1], "endBeat": [4, 1], "startValue": 0.0, "endValue": 400.0}));
        c
    }

    fn state_of(c: &EditCore) -> EditorState {
        EditorState::new(state::chart_from_doc(c.doc()))
    }

    /// 行快照：视图序与 doc 下标是两件事，计数要对得上
    #[test]
    fn line_rows_map_view_order_to_doc_index() {
        let c = sample();
        let chart = state::chart_from_doc(c.doc());
        let rows = line_rows_of(&chart);
        assert_eq!(rows.len(), c.doc().judge_lines.len(), "一条线一行");
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.view, i, "view 是列表里的行号");
        }
        // 样例文档默认带一条判定线；音符与事件都挂在它上面
        let l0 = rows.iter().find(|r| r.doc == 0).expect("0 号线的行");
        assert_eq!(l0.notes, 2, "两个音符");
        assert_eq!(l0.events, 1, "一条 moveX 事件");
        assert!(!l0.name.is_empty(), "线名要抄出来（列表要显示它）");
    }

    /// 没选中判定线 ⇒ 检查器为空（右侧那一栏就该空着，不该显示上一条线的内容）
    #[test]
    fn inspector_is_empty_without_a_selection() {
        let c = sample();
        let mut st = state_of(&c);
        st.chart.lines.clear(); // 让 selected() 落空
        assert!(inspector_of(&st, c.doc()).is_none());
    }

    /// 选中音符 ⇒ 可编辑字段取自**文档**（alpha/isFake/speed 这些只存在文档里）
    #[test]
    fn inspector_carries_document_fields_for_the_selected_note() {
        let c = sample();
        let mut st = state_of(&c);
        st.selected_line = 0;
        st.select_note(1); // 第二个音符 = hold（那个带 set 的）
        let insp = inspector_of(&st, c.doc()).expect("有选中的线");
        assert_eq!(insp.line_index, 0);
        assert_eq!(insp.notes, 2);
        let ne = insp.note_edit.expect("选中了音符就该有可编辑字段");
        assert_eq!(ne.kind, "hold");
        assert_eq!(ne.end_beat, Some(12.0), "hold 才有结束拍");
        assert_eq!(ne.alpha, 128);
        assert!(ne.is_fake, "isFake 只存在文档里，必须从文档抄");
        assert_eq!(ne.speed, 2.5);
        assert_eq!(ne.lane_x, -200.0);
        // 只读快照与可编辑字段指向**同一个**音符（同一 laneX、同一时间轴换算）
        let nv = insp.note.expect("note 视图");
        assert_eq!(nv.lane_x, ne.lane_x);
        assert!((nv.time - st.chart.tmap.sec(8.0)).abs() < 1e-9);
        assert!(nv.end > nv.time, "hold 的结束时间要晚于开始");
    }

    /// 非数值事件（颜色/字符串轨道）⇒ 退化为只读：有 `event` 快照、没有 `event_edit`
    #[test]
    fn non_numeric_events_are_read_only() {
        let mut c = sample();
        // alpha 轨道的值是数值；这里造一条**字符串值**的事件来验证退化路径
        c.exec(&json!({"op": "add_event", "line": 0, "layer": 0, "track": "alpha",
                       "startBeat": [0, 1], "endBeat": [2, 1], "startValue": 1.0, "endValue": 0.0}));
        let mut st = state_of(&c);
        st.selected_line = 0;
        st.selected_track = TrackId::Alpha;
        st.select_event(TrackId::Alpha, 0);
        let insp = inspector_of(&st, c.doc()).expect("有选中的线");
        assert_eq!(insp.track, TrackId::Alpha);
        assert_eq!(insp.track_events, 1);
        let ev = insp.event.expect("有选中事件就该有快照");
        assert!(ev.end_beat >= ev.start_beat, "{}..{}", ev.start_beat, ev.end_beat);
        assert!(insp.event_edit.is_some(), "数值事件可以编辑");
        assert!(insp.track_value.is_some(), "轨道有事件 ⇒ 该给出当前时刻的求值结果");

        // 空轨道：没有事件 ⇒ 没有快照、也没有可编辑字段（而不是给一堆 0）
        st.selected_track = TrackId::Rotate;
        st.select_event(TrackId::Alpha, 0);
        let insp = inspector_of(&st, c.doc()).expect("有选中的线");
        assert!(insp.event.is_none());
        assert!(insp.event_edit.is_none());
        assert!(insp.track_value.is_none());
    }

    /// 选中轨道没有事件时，`event_edit` 必须为 `None`（否则属性编辑器会显示一条不存在的事件）
    #[test]
    fn selected_event_out_of_range_is_ignored() {
        let c = sample();
        let mut st = state_of(&c);
        st.selected_line = 0;
        st.selected_track = TrackId::MoveX;
        st.select_event(TrackId::Alpha, 99); // 越界
        let insp = inspector_of(&st, c.doc()).expect("有选中的线");
        assert!(insp.event.is_none() && insp.event_edit.is_none());
        assert_eq!(insp.track_events, 1, "轨道本身还是有 1 条事件");
    }
}
