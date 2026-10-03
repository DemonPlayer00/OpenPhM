// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **边界测试**：EditCore 只管"会写进谱面文件的数据"，界面内部规则归界面自己。
//!
//! 用户定的规则原话：*editcore 只管最终会保存到谱面文件的数据，gui 不能私自更新可能未到 editcore 的数据，
//! 但 gui 内部的规则可以自行更新（例如网格吸附和谱面文件无关，谱面只有坐标）*。
//!
//! 这里的断言分三块：
//! 1. 文档核心**不认识**视图命令（两条通道互不重叠）；
//! 2. 音频有两条互不干扰的路径：`set_meta{audio}` 是文档、`{"op":"audio"}` 只是预览；
//! 3. 界面的视图状态（网格/吸附/播放头/线长…）改了之后，**文档一个字节都不变**。

use opm_app::control::{parse_view_cmd, ViewCmd};
use opm_app::core::EditCore;
use opm_app::state::{chart_from_doc, EditorState, GridCfg};
use serde_json::json;

#[test]
fn document_core_does_not_know_view_commands() {
    let mut core = EditCore::new();
    // 这些是视图命令（播放/定位/网格…），文档核心必须**不认识**它们
    for op in ["play", "pause", "toggle_play", "nudge", "ui_stats", "view"] {
        let r = core.exec(&json!({ "op": op }));
        assert_eq!(
            r["ok"],
            json!(false),
            "`{op}` 不该被 EditCore 接受（它属于视图状态，不是文档数据）"
        );
    }
    // 而它们都能被控制通道翻译成视图命令（或明确不需要排队）
    assert!(matches!(parse_view_cmd(&json!({"op":"play"})), Some(ViewCmd::Play)));
    assert!(matches!(
        parse_view_cmd(&json!({"op":"nudge","beats":2})),
        Some(ViewCmd::NudgeBeats(_))
    ));
    assert!(matches!(
        parse_view_cmd(&json!({"op":"audio","path":"a.wav"})),
        Some(ViewCmd::LoadAudio(_))
    ));
    // `view` 是查询：不排队，直接由控制线程回答
    assert!(parse_view_cmd(&json!({"op":"view"})).is_none());
}

/// **多选也能从控制通道指过去**：否则"框选/多选"的效果只能手点，
/// agent 没法截图复核（选区是视图状态，走的就是这条通道）。
#[test]
fn select_accepts_a_multi_selection() {
    match parse_view_cmd(&json!({"op":"select","notes":[0,2,5]})) {
        Some(ViewCmd::Select { notes, events, note, event, .. }) => {
            assert_eq!(notes, Some(vec![0, 2, 5]));
            assert!(events.is_none() && note.is_none() && event.is_none());
        }
        other => panic!("应解析成多选音符：{other:?}"),
    }
    match parse_view_cmd(&json!({"op":"select","events":[["alpha",0],["moveX",3],["nope",1]]})) {
        Some(ViewCmd::Select { events, notes, .. }) => {
            // 认不出的轨道名丢掉，不整条命令失败（视图命令不该因为一个笔误就没反应）
            assert_eq!(
                events,
                Some(vec![("alpha".to_owned(), 0), ("moveX".to_owned(), 3)])
            );
            assert!(notes.is_none());
        }
        other => panic!("应解析成多选事件：{other:?}"),
    }
    // 单选口径照旧
    match parse_view_cmd(&json!({"op":"select","note":3,"track":"alpha","event":1})) {
        Some(ViewCmd::Select { note, event, track, notes, .. }) => {
            assert_eq!((note, event), (Some(3), Some(1)));
            assert_eq!(track.as_deref(), Some("alpha"));
            assert!(notes.is_none());
        }
        other => panic!("应解析成单选：{other:?}"),
    }
}

#[test]
fn audio_has_a_document_path_and_a_view_path() {
    let mut core = EditCore::new();
    core.exec(&json!({"op": "add_line"}));

    // ① 文档路径：改 `meta.audio` —— 会广播、可撤销、会写进文件
    assert_eq!(core.doc().meta.audio, None);
    let r = core.exec(&json!({"op": "set_meta", "set": {"audio": "song.ogg"}}));
    assert_eq!(r["ok"], json!(true));
    assert_eq!(core.doc().meta.audio.as_deref(), Some("song.ogg"));
    let b = core.exec(&json!({"op": "broadcasts", "recent": 1}));
    assert_eq!(b["result"]["items"][0]["topics"][0], json!("Meta"), "改文档要广播");
    core.exec(&json!({"op": "undo"}));
    assert_eq!(core.doc().meta.audio, None, "文档字段可撤销");
    core.exec(&json!({"op": "redo"}));
    assert_eq!(core.doc().meta.audio.as_deref(), Some("song.ogg"));
    // 清空也给一条路（null = 不要音频）
    core.exec(&json!({"op": "set_meta", "set": {"audio": null}}));
    assert_eq!(core.doc().meta.audio, None);

    // ② 视图路径：`{"op":"audio"}` 只是"换预览用的音频"，文档核心根本不认识这个 op
    assert_eq!(core.exec(&json!({"op": "audio", "path": "other.wav"}))["ok"], json!(false));
    assert_eq!(
        core.doc().meta.audio,
        None,
        "换预览音频不该动文档里的 meta.audio"
    );
}

#[test]
fn view_state_never_leaks_into_the_document() {
    // 界面内部规则（网格、吸附、播放头、线长、叠加层…）改了又改，文档必须**逐字节不变**
    let mut core = EditCore::new();
    core.exec(&json!({"op": "add_line"}));
    core.exec(&json!({"op": "add_note", "line": 0, "kind": "tap", "laneX": 0.0, "startBeat": [1, 1]}));
    let before = serde_json::to_string(&core.doc().to_json()).unwrap();
    let rev_before = core.revision();
    // 先记下"文档改动本身造成的脏状态"：前面两条命令确实改了文档且没保存 ⇒ 本来就是脏的
    let dirty_before = core.is_dirty();

    let mut st = EditorState::new(chart_from_doc(core.doc()));
    st.grid = GridCfg { beat_div: 16, lane_div: 8 };
    st.lookahead = 8.0;
    st.line_half_w = 1234.0;
    st.show_boundary = false;
    st.boundary_dim = 0.99;
    st.overlay_enabled = false;
    st.overlay_beats = 7.0;
    st.selected_line = 0;
    st.selected_track = opm_app::state::TrackId::Speed;
    st.select_note(0);
    st.seek(3.0);
    st.set_playing(true);

    let after = serde_json::to_string(&core.doc().to_json()).unwrap();
    assert_eq!(before, after, "视图状态不得写进文档");
    assert_eq!(core.revision(), rev_before, "视图状态不得让核心 revision 变化");
    assert_eq!(
        core.is_dirty(),
        dirty_before,
        "只动视图状态不该改变是否未保存（前面的文档改动该脏就脏）"
    );

    // 反过来：吸附规则本身可验证（它是界面自己的事，但规则要正确）
    let g = GridCfg { beat_div: 4, lane_div: 16 };
    assert!((g.snap_beat(0.30) - 0.25).abs() < 1e-12);
    assert_eq!(g.beat_json(g.snap_beat(0.25)), [1, 4], "吸附后写回的是有理数 k/4");
}

/// **事件重叠检测**（用户要求：每次更改后查这次改动、加载谱面时全量查）。
///
/// 只报**真的多覆盖了一段**的情况：头尾相接（前一个止拍 == 后一个起拍）不算重叠 ——
/// 那正是格式要求的"无空隙不重叠"。
#[test]
fn overlap_detection_reports_real_overlaps_only() {
    use opm_app::cmd;
    use opm_app::doc::{Beat, BpmEntry, Document, Event, JudgeLine};
    use serde_json::json;

    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry {
        start: Beat::zero(),
        bpm: 180.0,
        foreign: Default::default(),
    }];
    doc.judge_lines.clear();
    let mut l = JudgeLine::default();
    let ev = |a: i64, b: i64| {
        Event::new(
            Beat::new(a, 1),
            Beat::new(b, 1),
            json!(0.0),
            json!(0.0),
            "linear",
        )
    };
    // 首尾相接：不算重叠
    l.layers[0].track_mut("alpha").unwrap().extend([ev(0, 16), ev(16, 32)]);
    // 真重叠：8..24 与 16..40 在 [16,24) 上重叠
    l.layers[0].track_mut("moveX").unwrap().extend([ev(8, 24), ev(16, 40)]);
    doc.judge_lines.push(l);

    let all = cmd::overlaps(&doc);
    assert_eq!(all.len(), 1, "只应报出 moveX 上那处重叠：{all:?}");
    let o = &all[0];
    assert_eq!(o.track, "moveX");
    assert_eq!((o.prev, o.next), (0, 1));
    assert_eq!(o.start, Beat::new(16, 1));
    assert_eq!(o.end, Beat::new(24, 1), "重叠区 = [16,24)");
    assert!(o.label().contains("重叠"), "{}", o.label());
    assert_eq!(o.pointer(), "/judgeLines[0].layers[0].moveX[1]");

    // 增量接口：按线查 == 全量查
    assert_eq!(cmd::overlaps_of_line(&doc, 0), all);
    assert!(cmd::overlaps_of_line(&doc, 9).is_empty(), "越界线号不该 panic");
}

/// 头尾相接时抓谁：**优先选中事件，都没选中则选尾巴**。
#[test]
fn boundary_prefers_selected_then_tail() {
    // 规则本体是 `opm_app::state::prefer_edge`（GUI 的 `overlay` 只再导出它）：这里直接调真身。
    // 以前这里手抄了一份副本（因为规则在 bin 内、集成测试拿不到）—— 副本不会跟着实现改，
    // 测试反而变成"保证旧行为"的锁，所以副本删掉（与 `tests/audio.rs` 调 `keymap` 真身同一个口径）。
    use opm_app::state::{prefer_edge, EventEdge};
    let both = [(0usize, EventEdge::End), (1, EventEdge::Start)]; // 事件 0 的尾 + 事件 1 的头，在同一条 y 上
    assert_eq!(prefer_edge(&both, None), Some((0, EventEdge::End)), "都没选中 ⇒ 选尾巴");
    assert_eq!(
        prefer_edge(&both, Some(1)),
        Some((1, EventEdge::Start)),
        "选中了事件 1 ⇒ 选它"
    );
    assert_eq!(
        prefer_edge(&both, Some(0)),
        Some((0, EventEdge::End)),
        "选中了事件 0 ⇒ 选它"
    );
    assert_eq!(
        prefer_edge(&[(2, EventEdge::Start)], Some(9)),
        Some((2, EventEdge::Start)),
        "选中项不在候选里 ⇒ 退回规则"
    );
    assert_eq!(prefer_edge(&[], None), None);
}
