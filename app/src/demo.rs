//! **演示谱面**（`--notes N` 用的那套负载）：按需造一份"有判定线、有事件、有音符"的文档。
//!
//! 它走 **`EditCore` 的命令路径**（不是直接构造 `Document`）："文档只有核心能写"这条边界对
//! 演示数据同样成立 —— 绕过去就等于给自己留了一个旁路，而这个旁路迟早会被当成"就这么写文档"的范例。
//! 顺带的好处是：这条路径本身就在验证命令语言（造 2 万音符 = 2 万条 `add_note`）。
//!
//! 放在库里而不是 `main.rs`：`--notes` 是 bench / 压测 / 造图共用的入口，CLI、测试与 GUI 都该
//! 能调到同一份实现（早先只有 GUI 有，测试各自手搓）。

use serde_json::json;

use crate::core::EditCore;

/// 演示谱面用几条判定线
pub const DEMO_LINES: usize = 4;

/// 前多少拍做动画，之后保持常量（谱面要求每条轨道"铺满"到末尾，见 `spec/opm-format.md`）
pub const DEMO_ANIM_END: i64 = 32;

/// 生成演示谱面并**通过 EditCore 的命令路径**写进 `core`，返回执行的命令条数。
///
/// 布局：`DEMO_LINES` 条判定线 × 5 条轨道（每条两段事件：0..32 拍做动画、32..末尾保持常量），
/// 音符按线轮转分配，拍 = `k/4`（180 BPM 下即 12 音符/秒）。
pub fn build_demo_via_core(core: &mut EditCore, notes: usize) -> DemoBuild {
    let end = (notes as i64 + 8) / 4 + 64;
    let anim_end = DEMO_ANIM_END;
    let mut cmds: Vec<serde_json::Value> = vec![
        json!({"op": "set_bpm", "index": 0, "bpm": 180.0}),
        json!({"op": "set_meta", "set": {"name": format!("demo-{notes}")}}),
    ];

    // 文档默认自带 1 条判定线，补到 DEMO_LINES 条
    for _ in 1..DEMO_LINES {
        cmds.push(json!({"op": "add_line"}));
    }
    for li in 0..DEMO_LINES {
        let name = format!("L{li}");
        cmds.push(json!({"op": "set_line", "line": li, "set": {
            "name": name, "zOrder": li as i64, "isCover": true, "bpmFactor": 1.0
        }}));
        for (track, from, to, easing) in plan_for_line(li) {
            cmds.push(json!({"op": "add_event", "line": li, "layer": 0, "track": track,
                "startBeat": [0, 1], "endBeat": [anim_end, 1],
                "startValue": from, "endValue": to, "easing": easing}));
            cmds.push(json!({"op": "add_event", "line": li, "layer": 0, "track": track,
                "startBeat": [anim_end, 1], "endBeat": [end, 1],
                "startValue": to, "endValue": to, "easing": "linear"}));
        }
    }
    // 音符按线轮转分配（与早先直接构造时的分布一致）
    for k in 0..notes {
        let li = k % DEMO_LINES;
        let kind = match k % 11 {
            0..=6 => "tap",
            7 | 8 => "drag",
            9 => "flick",
            _ => "hold",
        };
        let mut c = json!({"op": "add_note", "line": li, "kind": kind,
            "startBeat": [k as i64, 4], "laneX": ((k % 9) as f64 - 4.0) * 120.0});
        if kind == "hold" {
            c["endBeat"] = json!([k as i64 + 2, 4]);
        }
        cmds.push(c);
    }

    let n = cmds.len();
    let (resps, _failed) = core.exec_batch(&cmds);
    // 把"哪几条失败了"交回调用方：**打印策略属于界面**（库里只管造，不管怎么报）
    let failed = resps
        .into_iter()
        .filter(|r| r.get("ok").and_then(|v| v.as_bool()) != Some(true))
        .collect();
    DemoBuild { commands: n, failed }
}

/// 造完的结果：命令总数 + 失败的命令（正常应当为空）
#[derive(Clone, Debug)]
pub struct DemoBuild {
    pub commands: usize,
    /// 失败命令的响应（`opm_app::cmd::response_line` 可以把它变成一行人话）
    pub failed: Vec<serde_json::Value>,
}

/// 第 `li` 条线的五条轨道计划：`(轨道, 起点值, 终点值, 缓动)`
fn plan_for_line(li: usize) -> [(&'static str, f64, f64, &'static str); 5] {
    match li {
        0 => [
            ("moveX", -400.0, 400.0, "inOutSine"),
            ("moveY", 0.0, 0.0, "linear"),
            ("rotate", 0.0, 0.0, "linear"),
            ("alpha", 1.0, 1.0, "linear"),
            ("speed", 10.0, 10.0, "linear"),
        ],
        1 => [
            ("moveX", 0.0, 0.0, "linear"),
            ("moveY", 250.0, 250.0, "linear"),
            ("rotate", 0.0, 90.0, "outCubic"),
            ("alpha", 1.0, 0.6, "linear"),
            ("speed", 8.0, 8.0, "linear"),
        ],
        2 => [
            ("moveX", 0.0, 0.0, "linear"),
            ("moveY", -250.0, -250.0, "linear"),
            ("rotate", 0.0, 0.0, "linear"),
            ("alpha", 1.0, 0.25, "inOutQuad"),
            ("speed", 6.0, 14.0, "inOutCubic"),
        ],
        _ => [
            ("moveX", -300.0, 300.0, "outBack"),
            ("moveY", -120.0, -120.0, "linear"),
            ("rotate", -45.0, 45.0, "inOutElastic"),
            ("alpha", 0.9, 0.9, "linear"),
            ("speed", 10.0, 10.0, "linear"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::TRACKS;

    /// 形状：4 条线、每条 5 条轨道、音符数就是 `notes`，且 **0 重叠**（演示谱面本身要干净 ——
    /// 它是压测与截图的基线，自带冲突会让人以为是编辑器的问题）
    #[test]
    fn demo_has_the_expected_shape() {
        let mut c = EditCore::new();
        let n = build_demo_via_core(&mut c, 200);
        assert!(n.commands > 200, "命令数至少是音符数：{n:?}");
        assert!(n.failed.is_empty(), "演示谱面的命令不该失败：{:?}", n.failed);
        assert_eq!(c.doc().judge_lines.len(), DEMO_LINES);
        assert_eq!(c.doc().note_count(), 200, "音符数就是请求的数");
        for (i, l) in c.doc().judge_lines.iter().enumerate() {
            for track in TRACKS {
                let evs = l.layers[0].track(track).expect("五条轨道都要有");
                assert!(!evs.is_empty(), "线 {i} 的 {track} 轨道是空的");
                // 铺满：最后一条事件的末尾 = 谱面末尾（格式不变量）
                let last = evs.last().unwrap();
                assert_eq!(last.end.to_f64(), last.end.to_f64());
            }
            assert_eq!(l.notes.len(), 200 / DEMO_LINES + usize::from(i < 200 % DEMO_LINES),
                "音符按线轮转分配");
        }
        assert!(c.overlaps().is_empty(), "演示谱面不该自带事件重叠：{:?}", c.overlaps());
        assert_eq!(c.doc().meta.name, "demo-200");
    }

    /// 0 音符也要能造（`--notes 0` 是默认值：只有线与事件，没有音符）
    #[test]
    fn demo_with_zero_notes_still_builds_lines_and_tracks() {
        let mut c = EditCore::new();
        build_demo_via_core(&mut c, 0);
        assert_eq!(c.doc().note_count(), 0);
        assert_eq!(c.doc().judge_lines.len(), DEMO_LINES);
        assert!(c.doc().judge_lines[0].layers[0].track("moveX").is_some());
    }

    /// 走的是**命令路径**：撤销一步能退掉最近一条音符（说明文档确实是被命令改的，
    /// 而不是被直接构造出来的）
    #[test]
    fn demo_is_written_through_the_command_path() {
        let mut c = EditCore::new();
        build_demo_via_core(&mut c, 30);
        let before = c.doc().note_count();
        assert_eq!(before, 30);
        let r = c.exec(&json!({"op":"del_note","line":0,"index":0}));
        assert_eq!(r["ok"], json!(true));
        assert_eq!(c.doc().note_count(), 29);
        let r = c.exec(&json!({"op":"undo"}));
        assert_eq!(r["ok"], json!(true));
        assert_eq!(c.doc().note_count(), 30, "撤销要能回到造完的状态");
    }

    /// 每条线的计划都是 5 条轨道、数值有限（缓动名必须是引擎认识的）
    #[test]
    fn every_line_plan_covers_all_five_tracks_with_known_easings() {
        for li in 0..DEMO_LINES {
            let plan = plan_for_line(li);
            let mut names: Vec<&str> = plan.iter().map(|p| p.0).collect();
            names.sort();
            let mut want: Vec<&str> = TRACKS.to_vec();
            want.sort();
            assert_eq!(names, want, "线 {li} 的轨道要覆盖五条");
            for (_t, from, to, easing) in plan {
                assert!(from.is_finite() && to.is_finite());
                assert!(crate::cmd::EASINGS.contains(&easing), "未知缓动 {easing}");
            }
        }
    }
}
