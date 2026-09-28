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

use crate::doc::NoteKind;
use crate::perf;
use crate::state::{EditorState, TrackId};

/// 拖动中的音符 → `set_note`。
///
/// `index` 是**视图内**的第几个音符（时间序），`doc_index` 才是文档里的下标 ——
/// 命令要的是后者。Hold 跟着一起挪：**保持时长不变**（起点的吸附结果决定终点）。
pub fn note_drag_command(
    st: &EditorState,
    index: usize,
    doc_index: usize,
    lane_x: f32,
    beat: f64,
) -> Value {
    let line_doc = st.selected_doc_line();
    let mut set = json!({ "laneX": lane_x, "startBeat": st.beat_json(beat) });
    if let Some(n) = st.selected().and_then(|l| l.notes.get(index)) {
        let dur = n.end - n.time;
        if dur > 1e-6 {
            // 终点的拍 = 起点拍 + 原时长（按当前时间映射换算），于是时长在拖动中不变
            let end_beat =
                beat + st.chart.tmap.beat(n.end) - st.chart.tmap.beat(n.time);
            set["endBeat"] = json!(st.beat_json(end_beat));
        }
    }
    json!({ "op": "set_note", "line": line_doc, "index": doc_index, "set": set })
}

/// 拖事件块的头/尾 → `resize_event`。
///
/// 语义是"**只改这一个事件**"（早先会同步邻块，用户明确否掉了）；由此产生的空隙/重叠由
/// 重叠检测报出来（那份检测现在归 `EditCore`）。
pub fn event_resize_command(
    st: &EditorState,
    index: usize,
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
        "layer": 0,
        "track": st.selected_track.key(),
        "index": index,
        "edge": edge,
        "toBeat": st.beat_json(beat),
    })
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
        .and_then(|t| perf::eval_events(&t.events, beat))
        .unwrap_or_else(|| track_neutral_value(track))
}

/// 事件：删除
pub fn del_event_command(line: usize, track: &str, index: usize) -> Value {
    json!({ "op": "del_event", "line": line, "layer": 0, "track": track, "index": index })
}

/// 事件：改值/缓动（`set` 只放**真的变了**的字段；空 `set` 由调用方跳过，别发空命令）
pub fn set_event_command(line: usize, track: &str, index: usize, set: Value) -> Value {
    json!({ "op": "set_event", "line": line, "layer": 0, "track": track, "index": index, "set": set })
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

/// 拖动音符时的事务标签 / 拖事件头尾时的事务标签（两处各写一遍容易写歪）
pub const DRAG_NOTE_LABEL: &str = "拖动音符";
pub const DRAG_EVENT_LABEL: &str = "调整事件时间";

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
    #[test]
    fn dragging_a_tap_sets_position_only() {
        let (_c, mut st) = sample();
        st.selected_line = 0;
        let cmd = note_drag_command(&st, 0, 0, 250.0, 2.0);
        assert_eq!(cmd["op"], json!("set_note"));
        assert_eq!(cmd["line"], json!(0));
        assert_eq!(cmd["index"], json!(0), "命令用的是**文档里**的下标");
        assert_eq!(cmd["set"]["laneX"], json!(250.0));
        assert_eq!(cmd["set"]["startBeat"], json!(st.beat_json(2.0)));
        assert!(cmd["set"].get("endBeat").is_none(), "tap 不该被塞一个 endBeat");
    }

    /// 拖动 **hold**：终点跟着走、**时长不变**（这是用户会立刻看出来的行为）
    #[test]
    fn dragging_a_hold_keeps_its_duration() {
        let (_c, mut st) = sample();
        st.selected_line = 0;
        // 文档里第二个音符是 hold：视图下标 1
        let cmd = note_drag_command(&st, 1, 1, -100.0, 10.0);
        let set = &cmd["set"];
        let start = st.beat_json(10.0);
        let end = st.beat_json(14.0); // 起点 10 拍 + 原时长 4 拍
        assert_eq!(set["startBeat"], json!(start));
        assert_eq!(set["endBeat"], json!(end), "终点必须保持原来的 4 拍时长");
        // 拍是**有理数**（吸附结果），不是浮点
        assert!(start[1] > 0 && end[1] > 0);
    }

    /// 拖事件头/尾：edge 映射成 "start"/"end"，轨道用**当前选中**的轨道
    #[test]
    fn resizing_an_event_maps_edge_and_track() {
        use crate::state::EventEdge;
        let (_c, mut st) = sample();
        st.selected_line = 0;
        st.selected_track = TrackId::MoveX;
        let cmd = event_resize_command(&st, 0, EventEdge::End, 6.0);
        assert_eq!(cmd["op"], json!("resize_event"));
        assert_eq!(cmd["track"], json!("moveX"));
        assert_eq!(cmd["edge"], json!("end"));
        assert_eq!(cmd["layer"], json!(0));
        assert_eq!(cmd["toBeat"], json!(st.beat_json(6.0)));

        st.selected_track = TrackId::Alpha;
        let cmd = event_resize_command(&st, 2, EventEdge::Start, 1.0);
        assert_eq!(cmd["track"], json!("alpha"), "轨道跟着选中项走");
        assert_eq!(cmd["edge"], json!("start"));
        assert_eq!(cmd["index"], json!(2));
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
        exec_ok(&mut c, note_drag_command(&st, 1, 1, -300.0, 10.0));
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
