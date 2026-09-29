// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **时间轴音符条的实测台**（默认 `#[ignore]`：它不是断言，是量尺）。
//!
//! 跑法：
//! ```sh
//! cargo test --release --test timeline_bench -- --ignored --nocapture
//! ```
//!
//! 为什么有它：时间轴的子音符条曾经是**一颗音符一个 `rect_filled`** —— 5 万音符的谱面整谱可见时
//! 5 万个形状/帧（逐帧流水里 `delta` p50 24.6 ms ≈ 40 fps；把时间轴藏掉 `--ws perform` 是 8.96 ms
//! ≈ 112 fps）。用户点的改法是：**行内有重合的合并成 1 个矩形**，并把音符条改成 **4 行 = 4 种音符**。
//!
//! 这一台在**同一个进程、同一份文档、同一个 egui `Context`、同一帧尺寸**下把两条路各画一遍，
//! 于是"快了多少"不靠两次不同时间点跑的窗口基准去比：
//!
//! - **旧**：`rect_filled` per note（把改造前的那几行原样复刻，`y`/最小宽度/配色都照旧）；
//! - **新**：`timeline::draw` **整条时间轴**（拍线、曲线、事件条、读数**全都算进去**）——
//!   这一行因此是偏保守的：它多画了旧那行没画的东西，形状数却少好几个数量级。
//!
//! 三档密度都量，是因为**合并不是万灵药**：稀疏谱面上它什么都不做（本来就一颗一个矩形），
//! 只有"几颗挤在一个像素里"时它才有意义 —— 而 Phigros 谱面的后半段正是那种密度。

use opm_app::doc::{Beat, BpmEntry, Document, Note as DocNote, NoteKind as DocKind};
use opm_app::state::{chart_from_doc, EditorState};
use opm_app::timeline::{self, TimelineGeom};
use std::time::Instant;

/// 与 `bench/stress-50k.opm` 同一形状：BPM 180、每秒 150 颗、四种音符轮流、12 条轨道
fn stress_doc(n: i64) -> Document {
    let kinds = [DocKind::Tap, DocKind::Hold, DocKind::Drag, DocKind::Flick];
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 180.0, foreign: Default::default() }];
    for i in 0..n {
        let k = kinds[(i % 4) as usize];
        // 1/50 拍一颗：180bpm 下 = 每秒 150 颗
        let mut note = DocNote::new(k, Beat::new(i * 5, 250), (i % 12) as f32 * 100.0 - 550.0);
        if k == DocKind::Hold {
            note.end = Some(Beat::new(i * 5 + 25, 250));
        }
        doc.judge_lines[0].notes.push(note);
    }
    doc
}

/// 改造前的音符条：**一颗音符一个矩形**（原样复刻，包括 1.5px 最小宽与那条 0.74 的行位）
fn old_note_strip(ui: &mut egui::Ui, st: &EditorState, geom: &TimelineGeom, rect: egui::Rect) {
    let p = ui.painter_at(rect);
    let Some(line) = st.selected() else { return };
    for (i, n) in line.notes.iter().enumerate() {
        let y = rect.min.y + rect.height() * 0.74;
        let (x0, x1) = (geom.x_of(n.time), geom.x_of(n.end.max(n.time)));
        let col = if st.is_note_selected(i) {
            egui::Color32::WHITE
        } else {
            let c = n.kind.color();
            egui::Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8)
        };
        p.rect_filled(
            egui::Rect::from_min_max(egui::pos2(x0, y), egui::pos2(x1.max(x0 + 1.5), y + 6.0)),
            0.0,
            col,
        );
    }
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// 一档密度的一行读数：形状数 / 构建 ms / tessellate ms（各取 5 次的中位）
struct Row {
    notes: i64,
    old_shapes: usize,
    new_shapes: usize,
    old_ms: f64,
    old_tess: f64,
    new_ms: f64,
    new_tess: f64,
    /// 新实现里**音符条**贡献的矩形数（= 4 行各自的段数）
    strip: usize,
}

fn measure(notes: i64, frames: usize) -> Row {
    let st = EditorState::new(chart_from_doc(&stress_doc(notes)));
    let ctx = egui::Context::default();
    // 与默认窗口（1600×900、时间轴占 0.22 高）里的那一条同尺寸
    let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1600.0, 198.0));
    let raw = || egui::RawInput { screen_rect: Some(rect), ..Default::default() };
    let old_frame = |ui: &mut egui::Ui| old_note_strip(ui, &st, &TimelineGeom::new(rect, &st, 2.0), rect);
    let new_frame = |ui: &mut egui::Ui| {
        timeline::draw(ui, &st, rect, 2.0);
    };

    // 暖机：第一帧要装字体图集，后面几帧 egui 才稳定
    for _ in 0..2 {
        let _ = ctx.run_ui(raw(), old_frame);
        let _ = ctx.run_ui(raw(), new_frame);
    }

    let (mut old_shapes, mut new_shapes) = (Vec::new(), Vec::new());
    let (mut old_ms, mut new_ms) = (Vec::new(), Vec::new());
    let (mut old_tess, mut new_tess) = (Vec::new(), Vec::new());
    for _ in 0..frames {
        let t = Instant::now();
        let mut f = ctx.run_ui(raw(), old_frame);
        old_ms.push(t.elapsed().as_secs_f64() * 1e3);
        old_shapes.push(f.shapes.len());
        let ppp = f.pixels_per_point;
        let t = Instant::now();
        let _ = ctx.tessellate(std::mem::take(&mut f.shapes), ppp);
        old_tess.push(t.elapsed().as_secs_f64() * 1e3);

        let t = Instant::now();
        let mut f = ctx.run_ui(raw(), new_frame);
        new_ms.push(t.elapsed().as_secs_f64() * 1e3);
        new_shapes.push(f.shapes.len());
        let ppp = f.pixels_per_point;
        let t = Instant::now();
        let _ = ctx.tessellate(std::mem::take(&mut f.shapes), ppp);
        new_tess.push(t.elapsed().as_secs_f64() * 1e3);
    }

    let strip = {
        let geom = TimelineGeom::new(rect, &st, 2.0);
        let spans = timeline::line_spans(
            st.selected().expect("总有一条判定线"),
            &geom,
            st.selection().notes_set(),
        );
        spans.iter().map(|r| r.base.len() + r.selected.len()).sum()
    };
    let m = |v: &mut Vec<f64>| median(v);
    Row {
        notes,
        old_shapes: median(&mut old_shapes.iter().map(|&n| n as f64).collect::<Vec<_>>()) as usize,
        new_shapes: median(&mut new_shapes.iter().map(|&n| n as f64).collect::<Vec<_>>()) as usize,
        old_ms: m(&mut old_ms),
        old_tess: m(&mut old_tess),
        new_ms: m(&mut new_ms),
        new_tess: m(&mut new_tess),
        strip,
    }
}

#[test]
#[ignore = "实测台：手动跑，见文件头"]
fn note_strip_cost_before_and_after() {
    let rows: Vec<Row> = [200i64, 5_000, 50_000].iter().map(|&n| measure(n, 5)).collect();
    println!("\n== 时间轴：改造前（一颗音符一个矩形）vs 改造后（4 行 + 行内合并）==");
    println!("   1600×198 的时间轴、BPM 180、5 次取中位、egui 侧（不含 GPU）");
    println!(
        "  {:>7} | {:>8} {:>8} | {:>8} {:>8} | {:>8} {:>8} | {:>6}",
        "音符数", "旧形状", "新形状", "旧构建", "新构建", "旧三角", "新三角", "音符矩形"
    );
    for r in &rows {
        println!(
            "  {:>7} | {:>8} {:>8} | {:>6.2}ms {:>6.2}ms | {:>6.2}ms {:>6.2}ms | {:>6}",
            r.notes,
            r.old_shapes,
            r.new_shapes,
            r.old_ms,
            r.new_ms,
            r.old_tess,
            r.new_tess,
            r.strip
        );
    }
    println!("  （新那一列画的是**整条时间轴**：拍线、曲线、事件条、两行读数都在里面；旧那列只有音符条）");
}
