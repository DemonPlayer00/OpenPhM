//! 表演求值的验收：29 个缓动的端点与已知值、多 BPM 时间映射、事件求值（含非线性缓动）。

use opm_app::doc::{Beat, Document, Event, BpmEntry, JudgeLine};
use opm_app::perf::{ease, eval_events, perf_at, sample_track, TimeMap};
use serde_json::json;

/// 与 cmd::EASINGS 同源的 29 个名字（重列一遍：测试不该依赖被测实现里的表）
const NAMES: [&str; 29] = [
    "linear", "outSine", "inSine", "outQuad", "inQuad", "inOutSine", "inOutQuad", "outCubic",
    "inCubic", "outQuart", "inQuart", "inOutCubic", "inOutQuart", "outQuint", "inQuint", "outExpo",
    "inExpo", "outCirc", "inCirc", "outBack", "inBack", "inOutCirc", "inOutBack", "outElastic",
    "inElastic", "outBounce", "inBounce", "inOutBounce", "inOutElastic",
];

#[test]
fn every_easing_hits_both_endpoints() {
    for n in NAMES {
        let a = ease(n, 0.0);
        let b = ease(n, 1.0);
        assert!(a.abs() < 1e-9, "{n}(0) = {a}，应为 0");
        assert!((b - 1.0).abs() < 1e-9, "{n}(1) = {b}，应为 1");
    }
}

#[test]
fn easing_spot_values() {
    // 线性
    assert!((ease("linear", 0.37) - 0.37).abs() < 1e-12);
    // 幂次族：中点值是可手算的
    assert!((ease("inQuad", 0.5) - 0.25).abs() < 1e-12);
    assert!((ease("outQuad", 0.5) - 0.75).abs() < 1e-12);
    assert!((ease("inCubic", 0.5) - 0.125).abs() < 1e-12);
    assert!((ease("outCubic", 0.5) - 0.875).abs() < 1e-12);
    assert!((ease("inQuart", 0.5) - 0.0625).abs() < 1e-12);
    // 对称族在中点必须过 0.5
    for n in ["inOutSine", "inOutQuad", "inOutCubic", "inOutQuart", "inOutCirc", "inOutBack", "inOutBounce", "inOutElastic"] {
        let v = ease(n, 0.5);
        assert!((v - 0.5).abs() < 1e-9, "{n}(0.5) = {v}，对称缓动应过 0.5");
    }
    // in/out 互为镜像（对同一族成立）
    for (a, b) in [("inSine", "outSine"), ("inQuad", "outQuad"), ("inCubic", "outCubic"), ("inExpo", "outExpo"), ("inCirc", "outCirc")] {
        for k in 1..10 {
            let t = k as f64 / 10.0;
            let lhs = ease(a, t);
            let rhs = 1.0 - ease(b, 1.0 - t);
            assert!((lhs - rhs).abs() < 1e-9, "{a}/{b} 在 t={t} 不互为镜像: {lhs} vs {rhs}");
        }
    }
    // 回弹/弹性会越界（这是它们的定义特征，不是 bug）
    assert!(ease("outBack", 0.6) > 1.0, "outBack 应冲过 1");
    assert!(ease("inBack", 0.4) < 0.0, "inBack 应先退到 0 以下");
    // outElastic 的过冲出现在 t≈0.45（t=0.35 时还在上升段内）—— 手算：2^-4.5·sin(3.75·2π/3)+1≈1.044
    assert!(ease("outElastic", 0.45) > 1.0, "outElastic 应在 t≈0.45 冲过 1");
    assert!((ease("outElastic", 0.45) - 1.044).abs() < 2e-3, "outElastic(0.45)={}", ease("outElastic", 0.45));
    // outBounce 的分段落点：t=1/d1 处恰好接上第一段
    assert!((ease("outBounce", 1.0 / 2.75) - 1.0).abs() < 1e-9);
    // 未知名字退回线性（校验器会先拦，这里只保证不算出 NaN）
    assert_eq!(ease("noSuchEasing", 0.5), 0.5);
}

#[test]
fn time_map_handles_multiple_bpms() {
    let mut doc = Document::default();
    // 180 BPM 起，第 4 拍变速到 240 BPM
    doc.bpm_list = vec![
        BpmEntry { start: Beat::zero(), bpm: 180.0, foreign: Default::default() },
        BpmEntry { start: Beat::new(4, 1), bpm: 240.0, foreign: Default::default() },
    ];
    let tm = TimeMap::from_doc(&doc);
    assert_eq!(tm.seg_count(), 2);
    // 0→4 拍 @180 = 4/3 秒
    assert!((tm.sec(4.0) - 4.0 / 3.0).abs() < 1e-9, "sec(4)={}", tm.sec(4.0));
    // 4→8 拍 @240 = 1 秒 ⇒ 8 拍 = 7/3 秒
    assert!((tm.sec(8.0) - 7.0 / 3.0).abs() < 1e-9, "sec(8)={}", tm.sec(8.0));
    // 反向映射必须自洽
    for beat in [0.0, 1.0, 3.9, 4.0, 5.5, 12.0] {
        let back = tm.beat(tm.sec(beat));
        assert!((back - beat).abs() < 1e-9, "beat {beat} 往返变成 {back}");
    }
    // 单 BPM 时就是一条直线
    let mut one = Document::default();
    one.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 120.0, foreign: Default::default() }];
    let t1 = TimeMap::from_doc(&one);
    assert_eq!(t1.seg_count(), 1);
    assert!((t1.sec(2.0) - 1.0).abs() < 1e-9);
}

#[test]
fn events_evaluate_with_their_own_easing() {
    let ev = vec![Event::new(Beat::zero(), Beat::new(4, 1), json!(0.0), json!(100.0), "inQuad")];
    assert_eq!(eval_events(&ev, 0.0), Some(0.0));
    assert_eq!(eval_events(&ev, 4.0), Some(100.0));
    // 中点：inQuad(0.5)=0.25 ⇒ 25
    let mid = eval_events(&ev, 2.0).unwrap();
    assert!((mid - 25.0).abs() < 1e-9, "中点 {mid}，期望 25（inQuad 而非线性插值）");
    // 换了缓动，中点就该不同 —— 证明缓动真的参与求值
    let lin = vec![Event::new(Beat::zero(), Beat::new(4, 1), json!(0.0), json!(100.0), "linear")];
    assert!((eval_events(&lin, 2.0).unwrap() - 50.0).abs() < 1e-9);
    // 事件之外取最近端（半成品数据也能算，不 panic）
    let gap = vec![Event::new(Beat::new(4, 1), Beat::new(8, 1), json!(7.0), json!(9.0), "linear")];
    assert_eq!(eval_events(&gap, 0.0), Some(7.0));
    assert_eq!(eval_events(&gap, 99.0), Some(9.0));
    assert_eq!(eval_events(&[], 0.0), None);
    // **空隙里必须保持前一条事件的终值**（"某时间没有事件则不移动判定线"）。
    // 这里曾经是错的：不在任何事件里时会返回"最后一条事件的终值"，于是线在空隙里直接跳过去。
    let gap = vec![
        Event::new(Beat::zero(), Beat::new(10, 1), json!(0.0), json!(5.0), "linear"),
        Event::new(Beat::new(20, 1), Beat::new(30, 1), json!(90.0), json!(99.0), "linear"),
    ];
    assert_eq!(eval_events(&gap, 0.0), Some(0.0));
    assert_eq!(eval_events(&gap, 15.0), Some(5.0), "空隙里应保持前一条的终值 5，而不是跳到 99");
    assert_eq!(eval_events(&gap, 19.9), Some(5.0));
    assert_eq!(eval_events(&gap, 20.0), Some(90.0), "进入下一条事件才变");
    assert_eq!(eval_events(&gap, 99.0), Some(99.0), "末条事件之后保持其终值");

    // 非数值（字符串/颜色轨道）不 panic，取 0
    let s = vec![Event::new(Beat::zero(), Beat::new(1, 1), json!("a"), json!("b"), "linear")];
    assert_eq!(eval_events(&s, 0.5), Some(0.0));
}

#[test]
fn line_perf_applies_rotate_then_translate() {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 180.0, foreign: Default::default() }];
    // Document::default() 自带一条判定线（"新谱面至少有线"）—— 测试里要先清掉，
    // 否则下面 push 进去的是第 2 条，读 [0] 会拿到空轨道（这个坑先踩过一次）
    doc.judge_lines.clear();
    let mut line = JudgeLine::default();
    line.name = "L0".into();
    let end = Beat::new(64, 1);
    for (track, from, to) in [
        ("moveX", 0.0, 100.0),
        ("moveY", 0.0, 50.0),
        ("rotate", 0.0, 90.0),
        ("alpha", 1.0, 0.0),
        ("speed", 10.0, 20.0),
    ] {
        line.layers[0].track_mut(track).unwrap().push(Event::new(
            Beat::zero(),
            end,
            json!(from),
            json!(to),
            "linear",
        ));
    }
    doc.judge_lines.push(line);

    let tm = TimeMap::from_doc(&doc);
    let line = &doc.judge_lines[0];
    let tracks = [
        opm_app::perf::track_events(line, "moveX"),
        opm_app::perf::track_events(line, "moveY"),
        opm_app::perf::track_events(line, "rotate"),
        opm_app::perf::track_events(line, "alpha"),
        opm_app::perf::track_events(line, "speed"),
    ];
    // t=0：全在起点
    let p0 = perf_at(&tracks, &tm, 0.0);
    assert_eq!((p0.x, p0.y, p0.rotate_deg, p0.alpha, p0.speed), (0.0, 0.0, 0.0, 1.0, 10.0));
    // 中途：线性 ⇒ 各自按比例
    let half_sec = tm.sec(32.0);
    let p1 = perf_at(&tracks, &tm, half_sec);
    assert!((p1.x - 50.0).abs() < 1e-3 && (p1.y - 25.0).abs() < 1e-3, "p1={p1:?}");
    assert!((p1.rotate_deg - 45.0).abs() < 1e-3 && (p1.alpha - 0.5).abs() < 1e-3);
    // 旋转 90° 后：本地 (100,0) → 屏幕 (0,100) 再叠加平移
    let mut perf = p1;
    perf.rotate_deg = 90.0;
    perf.x = 0.0;
    perf.y = 0.0;
    let got = perf.apply([100.0, 0.0]);
    assert!((got[0] - 0.0).abs() < 1e-3 && (got[1] - 100.0).abs() < 1e-3, "got={got:?}");
}

#[test]
fn track_sampling_spans_event_boundaries() {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 180.0, foreign: Default::default() }];
    let tm = TimeMap::from_doc(&doc);
    let ev = vec![
        Event::new(Beat::zero(), Beat::new(2, 1), json!(0.0), json!(10.0), "linear"),
        Event::new(Beat::new(2, 1), Beat::new(4, 1), json!(10.0), json!(0.0), "outBounce"),
    ];
    let pts = sample_track(&ev, &tm, 4);
    // 2 个事件 ×(4+1) 点
    assert_eq!(pts.len(), 10);
    // 时间单调不减，端点值正确
    for w in pts.windows(2) {
        assert!(w[1][0] >= w[0][0], "采样时间应单调: {:?}", w);
    }
    assert!((pts[0][1] - 0.0).abs() < 1e-6);
    assert!((pts[5][1] - 10.0).abs() < 1e-6, "第二段起点应为 10");
    // 第二段 10→0 用 outBounce：它是"先快后慢"，中点已掉到线性中点(5.0)以下 ——
    // 手算 outBounce(0.5)=0.7655 ⇒ 10-10·0.7655≈2.34。采样必须反映非线性（若用线性插值会得 5.0）
    let mid = pts[7][1];
    assert!((mid - 2.343_75).abs() < 1e-3, "outBounce 中点应为 2.34，实际 {mid}");
}
