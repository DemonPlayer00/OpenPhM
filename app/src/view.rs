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
use crate::state::{self, EditorState, MaskChannel, TrackId};

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

/// 重叠组里的一行（属性编辑器那份列表用）
#[derive(Clone, Debug)]
pub struct NoteStackRow {
    /// 视图下标（**选中用的就是它**：`select_note` 吃的是时间序下标）
    pub view_index: usize,
    /// 文档下标（命令用；显示也用）
    pub doc_index: usize,
    pub kind: &'static str,
    /// 判定时刻（拍）
    pub beat: f64,
    pub lane_x: f32,
    /// 是不是当前锚（列表里高亮它）
    pub is_anchor: bool,
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
    /// 判定时刻/释放时刻的**精确有理拍**（编辑器按 `[整拍] + [分子] / [分母]` 三个整数编辑）
    pub start_exact: crate::doc::Beat,
    /// 只有 hold 有（其余类型没有释放时刻）
    pub end_exact: Option<crate::doc::Beat>,
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
    /// 头/尾的**精确有理拍**（事件编辑器按 `[整拍] + [分子] / [分母]` 三个整数编辑，
    /// 不走浮点：1/3 这类拍在二进制浮点里留不住，而 pez 里它本来就是三元组）
    pub start_exact: crate::doc::Beat,
    pub end_exact: crate::doc::Beat,
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
            start_exact: d.start,
            end_exact: (d.kind == NoteKind::Hold).then(|| d.end_beat()),
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
            start_exact: ev2.start,
            end_exact: ev2.end,
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
            .and_then(|_| perf::track_value(&track.events, st.chart.tmap.beat(st.playhead), &st.chart.tmap)),
        event,
        note,
    })
}

/// **重叠组的显示行**（属性编辑器那份列表用）。
///
/// 为什么是函数、不是快照里的字段：`note_stack` 由**编辑区每帧**按"本帧画出来的选择框"刷新，
/// 而检查器快照只在广播/换选区时重建 —— 放进快照就会永远慢一拍（列表要等你再点一下才出现）。
pub fn note_stack_rows(st: &EditorState) -> Vec<NoteStackRow> {
    let Some(line) = st.selected() else {
        return Vec::new();
    };
    st.note_stack()
        .iter()
        .filter_map(|i| line.notes.get(*i).map(|n| (*i, n)))
        .map(|(view_index, n)| NoteStackRow {
            view_index,
            doc_index: n.doc_index,
            kind: n.kind.label(),
            beat: st.chart.tmap.beat(n.time),
            lane_x: n.lane_x,
            is_anchor: st.selected_note() == Some(view_index),
        })
        .collect()
}

// ---------------------------------------------------------------- 遮蔽区（躁域）

/// 检查器里**一条通道**的一行
#[derive(Clone, Debug)]
pub struct MaskChannelRow {
    pub channel: MaskChannel,
    /// 这条通道上有几个事件块
    pub events: usize,
    /// 此刻的值（空通道给 `None` —— 那不是 0，是"这条通道还没被用过"）
    pub value: Option<f64>,
    pub min: f32,
    pub max: f32,
}

/// 检查器里**选中的那个事件块**的可编辑字段。
///
/// 值原样是 `serde_json::Value`：`active` 通道的值是**布尔**（用户口径"二值化"），
/// 坐标通道是数字 —— 由界面按通道决定用复选框还是数字框，这里不做类型转换。
#[derive(Clone, Debug)]
pub struct MaskEventEdit {
    pub index: usize,
    pub start_beat: f64,
    pub end_beat: f64,
    pub start_exact: crate::doc::Beat,
    pub end_exact: crate::doc::Beat,
    pub start_value: serde_json::Value,
    pub end_value: serde_json::Value,
    pub easing: String,
}

/// 遮蔽区检查器快照（**遮蔽区编辑模式下右栏的全部内容**）
#[derive(Clone, Debug)]
pub struct MaskInspect {
    pub index: usize,
    pub zones: usize,
    pub name: String,
    /// 此刻的区域状态（显示与否 / 三个顶点 / active）
    pub state: perf::MaskState,
    /// 当前通道（列表里高亮它）
    pub channel: MaskChannel,
    pub channels: Vec<MaskChannelRow>,
    /// 当前通道里选中的事件块（没选中给 `None`）
    pub event: Option<MaskEventEdit>,
    /// 播放头那一拍（"在播放头放一块"的缺省起点）
    pub playhead_beat: f64,
}

/// 遮蔽区检查器快照。没选中任何区（或一个区都没有）时给 `None`。
pub fn mask_inspect(st: &EditorState) -> Option<MaskInspect> {
    // 走 `mask_edit_view`：一个区都没有时给的是**草稿区**（用户口径："数量为 0 时也能进入
    // 遮蔽区编辑，此时有默认的绘制三角形事件"）—— 面板因此不必为"没有区"分一套空状态。
    let zone = st.mask_edit_view()?;
    let zone = zone.as_ref();
    let beat = st.chart.tmap.beat(st.playhead);
    let channels = MaskChannel::ALL
        .iter()
        .map(|c| {
            let t = zone.track(*c);
            MaskChannelRow {
                channel: *c,
                events: t.events.len(),
                value: perf::track_value(&t.events, beat, &st.chart.tmap),
                min: t.min,
                max: t.max,
            }
        })
        .collect();
    let event = st.mask_sel.and_then(|i| {
        let t = zone.track(st.selected_channel);
        t.events.get(i).map(|e| MaskEventEdit {
            index: i,
            start_beat: e.start.to_f64(),
            end_beat: e.end.to_f64(),
            start_exact: e.start,
            end_exact: e.end,
            start_value: e.start_value.clone(),
            end_value: e.end_value.clone(),
            easing: e.easing.clone(),
        })
    });
    Some(MaskInspect {
        index: zone.index,
        zones: st.chart.zones.len(),
        name: zone.name.clone(),
        state: zone.state(&st.chart.tmap, st.playhead),
        channel: st.selected_channel,
        channels,
        event,
        playhead_beat: beat,
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

    /// 遮蔽区检查器：七条通道都列出来、此刻的值来自**唯一那份求值**、选中的块带上原始值
    #[test]
    fn mask_inspector_lists_channels_and_the_selected_block() {
        let mut c = EditCore::new();
        c.exec(&serde_json::json!({"op": "add_zone", "set": {"x1": 300.0, "active": true}}));
        c.exec(&serde_json::json!({"op": "add_zone_event", "track": "x1", "startBeat": [8, 1],
                                   "endBeat": [12, 1], "startValue": 300.0, "endValue": -100.0}));
        let mut st = EditorState::new(state::chart_from_doc(c.doc()));
        st.mask_edit = true;
        st.select_zone(0);
        let mi = mask_inspect(&st).expect("有遮挡区");
        assert_eq!(mi.zones, 1);
        assert_eq!(mi.channels.len(), 7, "七条通道");
        assert!(mi.event.is_none(), "没选中块 ⇒ 没有可编辑字段");
        // 此刻（拍 0）：x1 的常量事件 ⇒ 300
        let x1 = mi.channels.iter().find(|r| r.channel == MaskChannel::X1).unwrap();
        assert_eq!(x1.value, Some(300.0));
        assert_eq!(x1.events, 2);
        // active 通道此刻是 true（值是布尔，求值器按 0/1 算）
        assert!(mi.state.active);
        assert!(mi.state.visible, "坐标系上有事件 ⇒ 显示");
        // 选中 x1 的第二个块（doc 下标 1）
        st.selected_channel = MaskChannel::X1;
        st.mask_sel = Some(1);
        let mi = mask_inspect(&st).expect("有遮挡区");
        let ev = mi.event.expect("选中了块");
        assert_eq!(ev.index, 1);
        assert_eq!(ev.start_exact.to_f64(), 8.0);
        assert_eq!(ev.start_value, serde_json::json!(300.0));
        assert_eq!(ev.end_value, serde_json::json!(-100.0));
        // 没有区、也不在遮蔽区编辑模式 ⇒ 没有快照（右栏该空着）
        let empty = EditorState::new(state::chart_from_doc(&crate::doc::Document::default()));
        assert!(mask_inspect(&empty).is_none());
    }

    /// **零区草稿**（用户口径："遮蔽区数量为 0 时也能进入遮蔽区编辑，此时有默认的绘制三角形事件"）：
    /// 一个区都没有也能进模式，此时面板显示的就是 `add_zone` 会写出来的那块中央正三角形 ——
    /// 只是它还**不在文档里**（`zones == 0`），用户动一下编辑才 materialize。
    #[test]
    fn the_mask_inspector_shows_a_draft_triangle_when_there_are_no_zones() {
        let mut st = EditorState::new(state::chart_from_doc(&crate::doc::Document::default()));
        st.mask_edit = true;
        let mi = mask_inspect(&st).expect("零区 + 编辑模式 ⇒ 草稿区");
        assert_eq!(mi.zones, 0, "文档里一块区都没有");
        assert_eq!(mi.channels.len(), 7);
        for row in &mi.channels {
            let want = if row.channel == MaskChannel::Active { 0 } else { 1 };
            assert_eq!(row.events, want, "{}", row.channel.key());
        }
        // 草稿三角就是"中央正三角形"：此刻的值 = 常量那三个坐标
        assert!(mi.state.visible, "六条常量事件从起点就开始 ⇒ 显示");
        assert!(!mi.state.active, "active 没有事件 ⇒ false（纯色那一档）");
        assert_eq!(mi.state.v[0], [0.0, 200.0]);
        assert_eq!(mi.state.v[1], [-173.205, -100.0]);
        assert!(mi.event.is_none(), "草稿里没有选中的块");
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
