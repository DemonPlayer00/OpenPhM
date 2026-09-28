// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **解压缓存快照的实测台**（默认 `#[ignore]`：它不是断言，是量尺）。
//!
//! 跑法：
//! ```sh
//! cargo test --release --test snapshot_bench -- --ignored --nocapture
//! ```
//!
//! 为什么有它：`App::maybe_snapshot` 每 2 秒（`main.rs` 的 `SNAPSHOT_MIN_INTERVAL`）把**整份文档**
//! 序列化并写回解压缓存，而这活曾经是在 **GUI 帧里**干的 —— 50 000 音符的谱面上，逐帧流水
//! （`OPM_FRAME_LOG`）量到的是**每 2 秒一次、每次 ~250 ms 的停顿**（`ui_ms` 250~275 ms，
//! 而且播放头是墙钟驱动的 ⇒ 画面还会跳掉 0.28 秒）。这活现在是 [`opm_app::autosave`] 在后台做的，
//! 这一台留着当"为什么值得搬走"的账，也留着当以后改序列化方式时的对照。
//!
//! 这一台把那次停顿**拆成四段**，好决定往哪儿修：
//! ① `Document::clone`（把文档交给别的线程要付的钱）；
//! ② `Document::to_json`（建立 `serde_json::Value` 树，含 `merge_foreign`）；
//! ③ `to_vec_pretty`（8 MB 级输出）；
//! ④ `fs::write` + `rename`（落盘）。
//!

use opm_app::doc::{Beat, BpmEntry, Document, Event, JudgeLine, Note as DocNote, NoteKind as DocKind};
use serde_json::json;
use std::time::Instant;

/// 与压力谱同形的一份文档：`notes` 颗、四种音符轮换、12 条轨道位、`speed_events` 个流速事件
fn big_doc(notes: usize, speed_events: usize) -> Document {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 180.0, foreign: Default::default() }];
    doc.judge_lines.clear();
    let mut l = JudgeLine::default();
    let beat = |b: f64| Beat::new((b * 4.0).round() as i64, 4);
    let one = |v: f64| Event::new(Beat::zero(), Beat::new(4096, 1), json!(v), json!(v), "linear");
    for (track, v) in [("moveX", 0.0), ("moveY", 0.0), ("rotate", 0.0), ("alpha", 1.0)] {
        *l.layers[0].track_mut(track).unwrap() = vec![one(v)];
    }
    let seg = 4096.0 / speed_events.max(1) as f64;
    let mut sp = Vec::with_capacity(speed_events);
    for i in 0..speed_events {
        let (a, b) = (i as f64 * seg, (i + 1) as f64 * seg);
        sp.push(Event::new(beat(a), beat(b), json!(16.0), json!(-12.0), "inOutQuad"));
    }
    *l.layers[0].track_mut("speed").unwrap() = sp;
    for k in 0..notes {
        let b = 1.0 + k as f64 * 0.02;
        let lane = -600.0 + (k as f32 * 53.0) % 1200.0;
        let mut n = match k % 4 {
            0 => {
                let mut h = DocNote::new(DocKind::Hold, beat(b), lane);
                h.end = Some(beat(b + 1.0));
                h
            }
            1 => DocNote::new(DocKind::Drag, beat(b), lane),
            2 => DocNote::new(DocKind::Flick, beat(b), lane),
            _ => DocNote::new(DocKind::Tap, beat(b), lane),
        };
        n.speed = 1.0 + (k % 3) as f32 * 0.5;
        l.notes.push(n);
    }
    doc.judge_lines.push(l);
    doc
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

#[test]
#[ignore]
fn snapshot_session_cost_breakdown() {
    for notes in [5_000usize, 50_000] {
        let doc = big_doc(notes, 10);
        // ① 文档克隆（跨线程递交文档副本要付的钱）
        let t = Instant::now();
        let copy = doc.clone();
        let clone_ms = ms(t);
        std::hint::black_box(&copy);

        // ② Value 树（`to_json` 自己也是 `to_value` + `merge_foreign`）
        let t = Instant::now();
        let v = doc.to_json();
        let to_json_ms = ms(t);

        // ③ 漂亮打印（快照用的正是 `to_vec_pretty`）
        let t = Instant::now();
        let bytes = serde_json::to_vec_pretty(&v).unwrap();
        let pretty_ms = ms(t);
        let t = Instant::now();
        let compact = serde_json::to_vec(&v).unwrap();
        let compact_ms = ms(t);

        // ④ 落盘（写到 /tmp 的同尺寸临时文件，再改名 —— 与 `snapshot_session` 同一条路）
        let path = std::env::temp_dir().join(format!("opm-snapshot-bench-{notes}.json"));
        let tmp = path.with_extension("tmp");
        let t = Instant::now();
        std::fs::write(&tmp, &bytes).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        let write_ms = ms(t);
        let _ = std::fs::remove_file(&path);

        println!(
            "\n== {notes} 音符：快照一段一段 =="
        );
        println!("  ① Document::clone        : {clone_ms:8.2} ms");
        println!("  ② to_json（Value 树）    : {to_json_ms:8.2} ms");
        println!("  ③ to_vec_pretty          : {pretty_ms:8.2} ms   （{} KB）", bytes.len() / 1024);
        println!("     to_vec（紧凑，作对照）: {compact_ms:8.2} ms   （{} KB）", compact.len() / 1024);
        println!("  ④ write + rename         : {write_ms:8.2} ms");
        println!(
            "  ⇒ 现在这条链（②+③+④）  : {:8.2} ms   ← 每 2 秒在 GUI 帧里付一次",
            to_json_ms + pretty_ms + write_ms
        );
        println!(
            "  ⇒ 若只把②③④搬到别的线程: {:8.2} ms（GUI 只付①）",
            clone_ms
        );
    }
}
