//! **音符位置缓存的实测台**（默认 `#[ignore]`：它不是断言，是量尺）。
//!
//! 跑法：
//! ```sh
//! cargo test --release --test floor_bench -- --ignored --nocapture
//! ```
//!
//! 量的四件事（README「下落速度（流速）：与 RPE 一致」一节引用的就是它们）：
//! ① 加载时把**所有**音符位置算好要多久（`chart_from_doc`，含 `SpeedTable` 建表）；
//! ② 一帧的实例构建（`build_instances`，缓存干净 = 全部查表）；
//! ③ "查表"与"现算"两条路的单颗代价（异步没补完时走的正是现算）；
//! ④ 异步重算的吞吐（`pump_floors`：每帧预算按它定）。

use opm_app::doc::{Beat, BpmEntry, Document, Event, JudgeLine, Note as DocNote, NoteKind as DocKind};
use opm_app::render::build_instances;
use opm_app::state::{chart_from_doc, EditorState};
use serde_json::json;
use std::time::Instant;

/// 一份 N 音符 / M 流速事件的谱面（BPM 180 ⇒ 一拍 1/3 秒；一半音符是 2 拍长的 hold）
fn big_doc(notes: usize, speed_events: usize) -> Document {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 180.0, foreign: Default::default() }];
    doc.judge_lines.clear();
    let mut l = JudgeLine::default();
    let beat = |b: f64| Beat::new((b * 4.0).round() as i64, 4);
    let one = |v: f64| {
        Event::new(Beat::zero(), Beat::new(4096, 1), json!(v), json!(v), "linear")
    };
    for (track, v) in [("moveX", 0.0), ("moveY", 0.0), ("rotate", 0.0), ("alpha", 1.0)] {
        *l.layers[0].track_mut(track).unwrap() = vec![one(v)];
    }
    // 流速：连续铺满全谱，缓动轮着来（含非线性）
    let easings = ["linear", "inOutQuad", "outCubic", "inSine", "outBack", "inOutElastic"];
    let seg = 4096.0 / speed_events.max(1) as f64;
    let mut sp = Vec::with_capacity(speed_events);
    for i in 0..speed_events {
        let a = i as f64 * seg;
        let b = (i + 1) as f64 * seg;
        sp.push(Event::new(
            beat(a),
            beat(b),
            json!(10.0 + (i % 5) as f64 * 4.0),
            json!(10.0 + ((i + 2) % 5) as f64 * 4.0),
            easings[i % easings.len()],
        ));
    }
    *l.layers[0].track_mut("speed").unwrap() = sp;
    for k in 0..notes {
        let b = 1.0 + k as f64 * 0.3;
        let lane = -600.0 + (k as f32 * 53.0) % 1200.0;
        let mut n = if k % 2 == 0 {
            let mut n = DocNote::new(DocKind::Hold, beat(b), lane);
            n.end = Some(beat(b + 2.0));
            n
        } else {
            DocNote::new(DocKind::Tap, beat(b), lane)
        };
        n.speed = 1.0 + (k % 3) as f32 * 0.5;
        l.notes.push(n);
    }
    doc.judge_lines.push(l);
    doc
}

fn pct(v: &mut Vec<f64>, p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[(((v.len() - 1) as f64) * p).round() as usize]
}

#[test]
#[ignore]
fn measure() {
    for (notes, events) in [(2_000usize, 200usize), (20_000, 500), (100_000, 2_000)] {
        let doc = big_doc(notes, events);
        // ① 加载：视图 + **全部音符位置**
        let t0 = Instant::now();
        let chart = chart_from_doc(&doc);
        let load_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let mut st = EditorState::new(chart);
        st.selected_line = usize::MAX;
        st.show_boundary = false;
        let tmap = st.chart.tmap.clone();
        let last_sec = st.chart.lines[0].notes.last().map(|n| n.time).unwrap_or(0.0);

        // ② 一帧的实例构建：扫过整条谱面（缓存干净）
        let mut inst = Vec::new();
        let mut frames = Vec::new();
        let mut counts = Vec::new();
        for k in 0..=200 {
            st.playhead = last_sec * k as f64 / 200.0;
            let t = Instant::now();
            build_instances(&st, &mut inst);
            frames.push(t.elapsed().as_secs_f64() * 1000.0);
            counts.push(inst.len() as f64);
        }

        // ③ 单颗代价：查表 vs 现算（各取中间那 20 万次循环，摊掉调用开销）
        let line = &st.chart.lines[0];
        let h_now = line.h_at(0.0, &tmap);
        let n = line.notes.len().min(20_000);
        let t = Instant::now();
        let mut sink = 0.0f64;
        for (r, i) in (0..n).enumerate() {
            let off = line.floor_offset(i, h_now).unwrap_or(0.0);
            sink += off + r as f64 * 0.0;
        }
        let lookup_ns = t.elapsed().as_secs_f64() * 1e9 / n as f64;
        let t = Instant::now();
        for i in 0..n {
            let off = line.floor_offset_now(line.notes[i].time, h_now, &tmap);
            sink += off;
        }
        let direct_ns = t.elapsed().as_secs_f64() * 1e9 / n as f64;
        assert!(sink.is_finite());

        // ④ 异步重算：整条标脏，量每帧预算内能补多少
        st.chart.lines[0].mark_floors_stale_from(0);
        let stale = st.floor_pending();
        let t = Instant::now();
        let mut frames_used = 0;
        while st.floor_pending() > 0 {
            st.pump_floors(EditorState::FLOOR_NOTES_PER_FRAME);
            frames_used += 1;
        }
        let pump_ms = t.elapsed().as_secs_f64() * 1000.0;
        let per_frame_ms = pump_ms / frames_used.max(1) as f64;

        println!(
            "音符 {notes:>6} / 流速事件 {events:>4}：加载(含全部位置) {load_ms:7.2} ms ｜ \
             构建 p50 {:.3} / p99 {:.3} ms（实例 p50 {:.0}）｜ 查表 {lookup_ns:.1} ns vs 现算 {direct_ns:.1} ns ｜ \
             重算 {stale} 条 = {frames_used} 帧（{pump_ms:.2} ms，每帧 {per_frame_ms:.2} ms）",
            pct(&mut frames, 0.5),
            pct(&mut frames, 0.99),
            pct(&mut counts, 0.5),
        );
    }
}
