// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **拖动变速事件、5000 音符要重算**时的每帧代价实测台（默认 `#[ignore]`：它不是断言，是量尺）。
//!
//! 跑法：
//! ```sh
//! cargo test --release --test drag_bench -- --ignored --nocapture
//! ```
//!
//! 场景（用户问的那一个）：一条线上 **5000 颗音符**，用户在**滑一个流速事件的值**
//! （每帧改一点 —— 等于指针每动一下就发一条命令）。于是每帧都要：
//! ① 走一遍命令（`set_event`，进撤销栈）；② 重建这条线的事件视图（`tracks_of`，
//! 含时间轴折线与 `min_speed_magnitude`）；③ `set_tracks` —— 流速真的变了 ⇒ **重建检查点表**
//! 并把改动点之后的音符标脏（`mark_from_sec`）；④ `pump_floors(4096)` 异步补一段；
//! ⑤ 渲染（没算准的音符在渲染侧**现算**）。
//!
//! 量的是这五步各自的 p50/p99、拖动期间有多少音符一直"没算准"、实例里有多少颗走了现算，
//! 以及**松手之后几帧补齐**。判定标准就是帧预算：60 Hz 16.7 ms / 144 Hz 6.9 ms / 240 Hz 4.2 ms。

use opm_app::core::EditCore;
use opm_app::doc::{Beat, BpmEntry, Document, Event, JudgeLine, Note, NoteKind};
use opm_app::render::build_instances;
use opm_app::state::{chart_from_doc, tracks_of, EditorState};
use std::time::Instant;

/// 与 `floor_bench` 同一套缓动混合（含两个"回弹类"：折线实现下它们节点最多）
const EASINGS: [&str; 6] = ["linear", "inOutQuad", "outCubic", "inSine", "outBack", "inOutElastic"];

/// 一条线：`notes` 颗音符铺满 `beats` 拍，`events` 条流速事件铺满全谱（缓动轮换）。
///
/// `slow` = 流速压到 0.5 附近：构建窗口会撑到 `MAX_BUILD_LOOKAHEAD`（30 秒），
/// 于是"没算准的那些"几乎全都在窗口里 ⇒ 渲染侧**每颗都现算**（这是最坏的一档）。
fn scene_full(notes: usize, events: usize, beats: i64, holds_every: usize, slow: bool) -> Document {
    let mut doc = Document::default();
    doc.bpm_list =
        vec![BpmEntry { start: Beat::zero(), bpm: 180.0, foreign: Default::default() }];
    doc.judge_lines.clear();
    let mut l = JudgeLine::default();
    l.name = "drag".into();
    // 音符：均匀铺满；每 holds_every 颗里一颗 hold（尾巴走另一条 H 查询路径）
    for i in 0..notes {
        let beat = Beat::new((i as i64) * beats / notes.max(1) as i64, 1);
        let hold = holds_every > 0 && i % holds_every == 0;
        let kind = if hold { NoteKind::Hold } else { NoteKind::Tap };
        let mut n = Note::new(kind, beat, -300.0 + (i % 7) as f32 * 100.0);
        if hold {
            n.end = Some(Beat::new(beat.n + 2, 1));
        }
        l.notes.push(n);
    }
    // 流速事件：铺满全谱，值在 8~16 之间来回，缓动轮换
    // 事件边界用**四分之一拍**的有理拍：事件密到"每 0.3 秒一条"时整数拍会退化成零长
    let mut sp = Vec::with_capacity(events);
    for i in 0..events {
        let a = (i as i64) * beats * 4 / events.max(1) as i64;
        let b = ((i as i64) + 1) * beats * 4 / events.max(1) as i64;
        let (v0, v1) = if slow {
            (0.4 + (i % 3) as f64 * 0.1, 0.4 + ((i + 1) % 3) as f64 * 0.1)
        } else {
            (8.0 + (i % 3) as f64 * 4.0, 8.0 + ((i + 1) % 3) as f64 * 4.0)
        };
        sp.push(Event::new(
            Beat::new(a, 4),
            Beat::new(b, 4),
            serde_json::json!(v0),
            serde_json::json!(v1),
            EASINGS[i % EASINGS.len()],
        ));
    }
    *l.layers[0].track_mut("speed").unwrap() = sp;
    doc.judge_lines.push(l);
    doc
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[(((v.len() - 1) as f64) * p).round() as usize]
}

fn report(label: &str, v: &mut Vec<f64>) -> f64 {
    let p99 = pct(v, 0.99);
    println!("{label:<22} p50 {:6.3} ms  p99 {:6.3} ms  max {:6.3} ms", pct(v, 0.5), p99, pct(v, 1.0));
    p99
}

#[test]
#[ignore]
fn measure() {
    for (notes, events, beats, holds_every, slow, drag_first) in [
        (5_000usize, 12usize, 540i64, 5usize, false, false),
        // 用户那个场景的最坏版：5000 颗**全被标脏**（拖第一条流速事件）
        (5_000, 12, 540, 5, false, true),
        (5_000, 12, 540, 0, true, true),
        (5_000, 48, 540, 5, false, false),
        (5_000, 12, 540, 0, false, false),
        (10_000, 48, 1080, 5, false, false),
        (500, 12, 540, 5, false, false),
        // 慢流速 ⇒ 构建窗口撑到 30 秒，密簇全在窗口里（渲染侧现算的那一档）
        (5_000, 12, 90, 5, true, false),
        // 最坏一档：慢流速 + **拖第一条**（把 10 000 颗全标脏，pump 4096 一帧补不完）
        (10_000, 12, 90, 0, true, true),
        // 事件密度爬到"每 0.3 秒一条"：看 `set_tracks`（建表+折线）从哪里开始顶不住 240 Hz
        (5_000, 200, 540, 5, false, false),
        (5_000, 600, 540, 5, false, false),
        (5_000, 2000, 540, 5, false, false),
        // 音符数再上一个数量级：看每帧代价是不是**与音符数无关**
        (100_000, 12, 540, 5, false, false),
        (100_000, 200, 540, 5, false, false),
    ] {
        let doc = scene_full(notes, events, beats, holds_every, slow);
        let dragged = if drag_first { 0 } else { events / 2 }; // 拖哪一条流速事件
        let mut core = EditCore::new();
        core.replace_doc(doc.clone());

        let t = Instant::now();
        let mut st = EditorState::new(chart_from_doc(&doc));
        let load_ms = t.elapsed().as_secs_f64() * 1000.0;
        st.selected_line = 0;
        st.show_boundary = false;
        let tmap = st.chart.tmap.clone();
        // 播放头放在被拖事件附近（屏幕上马上要用的那些先算）
        let drag_beat = (dragged as f64 + 0.5) * (beats as f64 / events as f64);
        st.playhead = tmap.sec(drag_beat);
        let line_idx = st.chart.lines[0].index;

        let mut cmd_ms = Vec::new();
        let mut end_ms = Vec::new();
        let mut view_ms = Vec::new();
        let mut set_ms = Vec::new();
        let mut pump_ms = Vec::new();
        let mut inst_ms = Vec::new();
        let mut stale_all = Vec::new();
        let mut stale_visible = Vec::new();
        let mut inst_n = 0usize;
        let frames = 240; // 240 Hz 指针拖 1 秒
        let mut inst = Vec::new();
        for k in 0..frames {
            // ① 命令：滑值（正弦来回，像指针来回抖）
            let v = 10.0 + (k as f64 * 0.05).sin() * 2.0;
            let t = Instant::now();
            let r = core.exec(&serde_json::json!({
                "op":"set_event","line":0,"layer":0,"track":"speed","index":dragged,
                "set":{"endValue": v}
            }));
            assert_eq!(r["ok"], serde_json::json!(true), "{r}");
            cmd_ms.push(t.elapsed().as_secs_f64() * 1000.0);

            // ①′ GUI 每条广播都做的两件小事：内容末端（时间轴总长）与音符总数
            let t = Instant::now();
            let (end_beat, count) = (core.doc().chart_end().to_f64(), core.doc().note_count());
            end_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            std::hint::black_box((end_beat, count));

            // ② 事件视图（GUI `apply_dirty` 的那一步）
            let t = Instant::now();
            let tracks = tracks_of(core.doc(), line_idx, &tmap);
            view_ms.push(t.elapsed().as_secs_f64() * 1000.0);

            // ③ set_tracks：流速变了 ⇒ 重建检查点表 + 把改动点之后的音符标脏
            let t = Instant::now();
            let slot = st.chart.lines.iter_mut().find(|l| l.index == line_idx).expect("线还在");
            slot.set_tracks(tracks, &tmap);
            set_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            stale_all.push(st.floor_pending() as f64);

            // ④ 异步补一段（放在渲染之前：这一帧补好的这一帧就用上）
            let t = Instant::now();
            st.pump_floors(EditorState::FLOOR_NOTES_PER_FRAME);
            pump_ms.push(t.elapsed().as_secs_f64() * 1000.0);

            // ⑤ 渲染：没算准的那些在渲染侧现算
            st.playhead += 1.0 / 240.0;
            let t = Instant::now();
            build_instances(&st, &mut inst);
            inst_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            inst_n = inst.len();
            // 实例里有多少颗是"现算"的：`is_stale` 就是那条判据（渲染侧的兜底条件 + 在窗口里）
            let line = &st.chart.lines[0];
            stale_visible.push(
                line.notes
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| line.floors.is_stale(*i))
                    .count() as f64,
            );
        }

        // 松手：补齐要几帧、多久
        let mut frames_to_settle = 0usize;
        let t = Instant::now();
        while st.floor_pending() > 0 && frames_to_settle < 100_000 {
            st.pump_floors(EditorState::FLOOR_NOTES_PER_FRAME);
            frames_to_settle += 1;
        }
        let settle_ms = t.elapsed().as_secs_f64() * 1000.0;

        println!(
            "\n=== 音符 {notes} / 流速事件 {events} / {beats} 拍（{} 秒 @180）/ hold 每 {holds_every} 颗一颗 / 慢流速 {slow} / 拖第 {dragged} 条",
            beats / 3
        );
        println!("加载（含全部音符位置）{load_ms:.2} ms；拖动 {frames} 帧（240 Hz 指针）+ 每帧 pump 4096");
        report("① 命令 set_event", &mut cmd_ms);
        report("①′ chart_end+note_count", &mut end_ms);
        report("② tracks_of(整条线)", &mut view_ms);
        report("③ set_tracks", &mut set_ms);
        report("④ pump_floors(4096)", &mut pump_ms);
        report("⑤ build_instances", &mut inst_ms);
        let mut total: Vec<f64> = set_ms
            .iter()
            .zip(&view_ms)
            .zip(&pump_ms)
            .zip(&inst_ms)
            .map(|(((a, b), c), d)| a + b + c + d)
            .collect();
        let p99_total = pct(&mut total, 0.99);
        println!(
            "拖动期每帧合计（②+③+④+⑤）：p50 {:6.3} ms  p99 {:6.3} ms  ⇒ 占 240 Hz 预算 {:.1}%",
            pct(&mut total.clone(), 0.5),
            p99_total,
            p99_total / 4.167 * 100.0
        );
        println!(
            "拖动期：没算准的音符 min {:.0} / p50 {:.0} / max {:.0} 颗；实例 {inst_n} 个；\
             松手后 {frames_to_settle} 帧（{settle_ms:.1} ms）补齐",
            pct(&mut stale_all.clone(), 0.0),
            pct(&mut stale_all.clone(), 0.5),
            pct(&mut stale_all, 1.0)
        );
        println!(
            "拖动期：pump 之后**仍没算准**的音符 p50 {:.0} / max {:.0} 颗（它们在渲染侧现算，只限落在构建窗口里的）",
            pct(&mut stale_visible.clone(), 0.5),
            pct(&mut stale_visible, 1.0)
        );
        // 对照：**关掉 pump**（永远不补，全部走现算）—— 这就是"现算那一档"的上界
        let mut inst_nopump = Vec::new();
        for k in 0..frames {
            let v = 10.0 + (k as f64 * 0.05).sin() * 2.0;
            core.exec(&serde_json::json!({
                "op":"set_event","line":0,"layer":0,"track":"speed","index":dragged,
                "set":{"endValue": v}
            }));
            let tracks = tracks_of(core.doc(), line_idx, &tmap);
            let slot = st.chart.lines.iter_mut().find(|l| l.index == line_idx).expect("线还在");
            slot.set_tracks(tracks, &tmap);
            st.playhead += 1.0 / 240.0;
            let t = Instant::now();
            build_instances(&st, &mut inst);
            inst_nopump.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let stale_nopump = st.floor_pending();
        println!(
            "对照：**不 pump**、{stale_nopump} 颗全走现算时 build_instances p50 {:6.3} ms（实例 {inst_n} 个）",
            pct(&mut inst_nopump, 0.5)
        );
    }
}
