// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! 表演求值的验收：29 个缓动的端点与已知值、**缓动的折线实现**（0.1 秒采样）、
//! 多 BPM 时间映射、事件求值（含非线性缓动）、折线积分 = 解析积分。

use opm_app::doc::{Beat, BpmEntry, Document, Event, JudgeLine};
use opm_app::perf::{
    ease, ease_segments, eval_events, event_knots, event_segments, event_value, knot_us, perf_at,
    sample_track, track_value, turning_points, TimeMap, EASE_MERGE_SEC, EASE_SEG_SEC,
};
use serde_json::json;

/// 与 cmd::EASINGS 同源的 29 个名字（重列一遍：测试不该依赖被测实现里的表）
const NAMES: [&str; 29] = [
    "linear", "outSine", "inSine", "outQuad", "inQuad", "inOutSine", "inOutQuad", "outCubic",
    "inCubic", "outQuart", "inQuart", "inOutCubic", "inOutQuart", "outQuint", "inQuint", "outExpo",
    "inExpo", "outCirc", "inCirc", "outBack", "inBack", "inOutCirc", "inOutBack", "outElastic",
    "inElastic", "outBounce", "inBounce", "inOutBounce", "inOutElastic",
];

/// 给定 BPM 的时间映射（`Document::default()` 自带一条判定线，测试里只借它的 BPM 表）
fn tmap_of(bpm: f32) -> TimeMap {
    let mut doc = Document::default();
    doc.bpm_list =
        vec![BpmEntry { start: Beat::zero(), bpm, foreign: Default::default() }];
    TimeMap::from_doc(&doc)
}

/// 一块事件的**秒**时长（测试里的独立算法：两端各自算秒再相减）
fn event_span_of(e: &Event, tm: &TimeMap) -> f64 {
    tm.sec(e.end.to_f64()) - tm.sec(e.start.to_f64())
}

/// 端点**按位**精确（不是"误差小于 1e-9"）。
///
/// 这条从"容差"收紧成"按位"，是因为检查器的「就位目标」承诺的是"块末**0 误差**就位"：
/// 端点若只到 1e-16，`v0 + (v1−v0)·ease(1)` 放大之后就是"终点值 ≠ 你写的 endValue"
/// （实测 `inSine`/`inBack` 的 `ease(1)` 差 1~2 ulp、`outBack` 的 `ease(0)` 差 1 ulp）。
#[test]
fn every_easing_hits_both_endpoints_exactly() {
    for n in NAMES {
        let a = ease(n, 0.0);
        let b = ease(n, 1.0);
        assert_eq!(a, 0.0, "{n}(0) = {a}，应当是**恰好** 0");
        assert_eq!(b, 1.0, "{n}(1) = {b}，应当是**恰好** 1");
        // 越界输入也夹在端点上（t 已经被 clamp，这里确认夹完仍是精确端点）
        assert_eq!(ease(n, -3.0), 0.0);
        assert_eq!(ease(n, 7.0), 1.0);
    }
}

/// **过冲不能被"端点精确"顺手夹掉**：`back`/`elastic` 在中间就该越过 [0,1]。
///
/// 这条是防回归：把端点做精确最省事的写法是"夹住 ease 的返回值"，那会把这两个缓动删掉。
/// 折线实现同样不许夹：节点上的过冲值要留在折线上（见下面那条 `overshoot…`）。
#[test]
fn overshooting_easings_still_overshoot_in_the_middle() {
    // 扫一遍取极值，而不是写死某个 t（过冲峰在哪一点取决于具体实现参数）
    let scan = |n: &str| (0..=100).map(|i| ease(n, i as f64 / 100.0)).fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| (lo.min(v), hi.max(v)));
    for n in ["outBack", "outElastic", "inOutBack", "inOutElastic"] {
        let (_, hi) = scan(n);
        assert!(hi > 1.0, "{n} 应当在中间冲过 1（实测最大 {hi:.6}）");
    }
    for n in ["inBack", "inElastic", "inOutBack", "inOutElastic"] {
        let (lo, _) = scan(n);
        assert!(lo < 0.0, "{n} 应当在中间冲到 0 以下（实测最小 {lo:.6}）");
    }
}

/// **事件在自己端点上的值 == 文档里的端值，按位相等** —— "块末就位"的那条保证。
///
/// 取值特意用 `0.1 → 0.3`：即使 `ease(1)` 精确等于 1，`0.1 + (0.3−0.1) = 0.30000000000000004`
/// 也不等于 `0.3` —— 所以端点必须**直接取端值**，不能靠插值凑。
/// **折线实现不许把这条弄坏**：采样过后端点仍是定义，不是"最后一段外推出来的近似"。
#[test]
fn an_events_endpoint_values_are_bit_exact() {
    // 两块：一块短（< 0.1 秒 ⇒ 一段 ⇒ 等价 linear）、一块长（⇒ 折线多段）
    let tm_short = tmap_of(3000.0); // 4 拍 = 0.08 秒 < 0.1 秒
    let tm_long = tmap_of(120.0); // 4 拍 = 2 秒 ⇒ 20 段
    let ev = |easing: &str| Event::new(Beat::zero(), Beat::new(4, 1), json!(0.1), json!(0.3), easing);
    assert_eq!(event_segments(&ev("inOutElastic"), &tm_short), 1, "短块前提");
    assert!(event_segments(&ev("inOutElastic"), &tm_long) > 1, "长块前提");
    for easing in NAMES {
        for tm in [&tm_short, &tm_long] {
            let e = ev(easing);
            assert_eq!(
                event_value(&e, 0.0, tm).to_bits(),
                0.1f64.to_bits(),
                "{easing}：起点值应当**按位**等于 startValue"
            );
            assert_eq!(
                event_value(&e, 4.0, tm).to_bits(),
                0.3f64.to_bits(),
                "{easing}：终点值应当**按位**等于 endValue"
            );
            // 终点之后（空位保持）同样按位相等
            assert_eq!(
                event_value(&e, 4.0, tm).to_bits(),
                event_value(&e, 99.0, tm).to_bits()
            );
            // 折线的首尾节点也是端值本身
            let knots = event_knots(&e, tm);
            assert_eq!(knots.first().unwrap().1.to_bits(), 0.1f64.to_bits());
            assert_eq!(knots.last().unwrap().1.to_bits(), 0.3f64.to_bits());
        }
    }
    // 缺端点（只有一端）时：端点仍走常量那条路，不引入算术
    let only_start =
        Event::new(Beat::zero(), Beat::new(4, 1), json!(0.7), json!(null), "outBack");
    assert_eq!(event_value(&only_start, 4.0, &tm_long).to_bits(), 0.7f64.to_bits());
}

// ---------------------------------------------------------------- 折线实现本身

/// **采样规则**（用户口径）：每 0.1 秒一段、区间不许变大、末尾不足 0.04 秒并入前一段、
/// 整块不足 0.1 秒降成一段。
#[test]
fn the_segment_rule_is_ten_hertz_with_a_merged_tail() {
    // 时长（秒）→ 段数
    let table = [
        (0.0, 1),
        (0.02, 1),   // 整块不足 0.1 秒 ⇒ 一段（= 等价 linear 缓动）
        (0.1, 1),    // 恰好一个采样区间 ⇒ 一段
        (0.13, 1),   // 尾巴 0.03 < 0.04 ⇒ 并进前一段（整块只剩一段）
        (0.14, 2),   // 尾巴 0.04 ⇒ 不并（0.1 + 0.04）
        (0.23, 2),   // 0.1 + 0.13（尾巴 0.03 并进前一段）
        (0.25, 3),   // 0.1 + 0.1 + 0.05
        (0.5, 5),    // 5 × 0.1，没有尾巴
        (1.0, 10),
        (4.0 / 3.0, 13), // 12 × 0.1 + 0.1333（尾巴 0.0333 并进前一段）
        (2.0, 20),
    ];
    for (span, want) in table {
        assert_eq!(ease_segments(span), want, "时长 {span} 秒应切成 {want} 段");
    }
    // 常数与阈值本身也是口径的一部分
    assert_eq!(EASE_SEG_SEC, 0.1);
    assert_eq!(EASE_MERGE_SEC, 0.04);
    // 病态输入不许 panic（非有限/负数）
    assert_eq!(ease_segments(-1.0), 1);
    assert_eq!(ease_segments(f64::NAN), 1);
    // 非有限的时长（病态数据）退回一段；**有限但极长**的块由上限兜住
    assert_eq!(ease_segments(f64::INFINITY), 1);
    assert_eq!(ease_segments(1e9), opm_app::perf::EASE_SEG_MAX);
}

/// **采样区间不许变大**：任何一块，除了最后一段（吸收尾巴）之外，段长必须恰好 0.1 秒；
/// 最后一段最长 0.1 + 0.04 秒。
#[test]
fn no_sampling_interval_ever_grows_except_the_merged_tail() {
    let tm = tmap_of(120.0); // 一拍 = 0.5 秒 ⇒ 拍数 × 0.5 = 秒数
    for beats in 1..=40 {
        let e = Event::new(
            Beat::zero(),
            Beat::new(beats, 2),
            json!(0.0),
            json!(100.0),
            "outCubic",
        );
        let (a, b) = (e.start.to_f64(), e.end.to_f64());
        let d = tm.sec(b) - tm.sec(a);
        let n = event_segments(&e, &tm);
        if n <= 1 {
            continue; // 整块 ≤ 0.1 秒：一段，本来就"等价 linear"
        }
        let knots = event_knots(&e, &tm);
        assert_eq!(knots.len(), n + 1);
        let spans: Vec<f64> = knots
            .windows(2)
            .map(|w| tm.sec(w[1].0) - tm.sec(w[0].0))
            .collect();
        for (i, s) in spans.iter().enumerate() {
            if i + 1 < spans.len() {
                assert!(
                    (s - EASE_SEG_SEC).abs() < 1e-9,
                    "拍 {beats}/2（{d} 秒，{n} 段）：第 {i} 段是 {s} 秒，除末段外都该是 0.1 秒"
                );
            } else {
                assert!(
                    *s <= EASE_SEG_SEC + EASE_MERGE_SEC + 1e-9,
                    "末段 {s} 秒超过了 0.1 + 0.04"
                );
            }
        }
    }
}

/// **节点上的值 = 解析曲线上的值**（采样点不近似），节点**之间**才是直线（中点 = 两端平均）。
#[test]
fn knots_sit_on_the_curve_and_segments_are_straight() {
    let tm = tmap_of(120.0);
    let e = Event::new(Beat::zero(), Beat::new(4, 1), json!(0.0), json!(100.0), "outElastic");
    let n = event_segments(&e, &tm);
    assert_eq!(n, 20, "4 拍 @120 = 2 秒 ⇒ 20 段");
    let knots = event_knots(&e, &tm);
    // 节点 k 的值 = 100 × ease(u_k)，u_k = 节点时刻的事件进度
    for (k, (beat, v)) in knots.iter().enumerate() {
        let u = (*beat / 4.0).clamp(0.0, 1.0);
        let want = 100.0 * ease("outElastic", u);
        assert!(
            (v - want).abs() < 1e-9,
            "第 {k} 个节点（拍 {beat}）应当是解析曲线的值 {want}，实际 {v}"
        );
    }
    // 段内线性：取每段中点，值应是两端节点的平均（折线没有二次项）
    for w in knots.windows(2) {
        let mid = 0.5 * (w[0].0 + w[1].0);
        let got = event_value(&e, mid, &tm);
        let want = 0.5 * (w[0].1 + w[1].1);
        assert!(
            (got - want).abs() < 1e-9,
            "段中点应当是两端节点的平均：{got} vs {want}"
        );
    }
}

/// **过冲留在折线上**：`back`/`elastic` 的越界值必须能从求值器里读出来（不能被采样抹平）。
#[test]
fn overshoot_survives_the_sampling() {
    let tm = tmap_of(120.0);
    let e = Event::new(Beat::zero(), Beat::new(8, 1), json!(0.0), json!(100.0), "outBack");
    let top = (0..=400)
        .map(|k| event_value(&e, k as f64 * 0.01, &tm))
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(top > 100.0, "outBack 的过冲应当留在折线上，实测最大值 {top}");
    // `inBack` 起步往负方向退
    let back = Event::new(Beat::zero(), Beat::new(8, 1), json!(0.0), json!(100.0), "inBack");
    let lo = (0..=400)
        .map(|k| event_value(&back, k as f64 * 0.01, &tm))
        .fold(f64::INFINITY, f64::min);
    assert!(lo < 0.0, "inBack 应当冲到 0 以下，实测最小值 {lo}");
}

/// **整块不足 0.1 秒 ⇒ 等价于 linear 缓动**（用户口径）：此时缓动名不起任何作用。
#[test]
fn a_block_shorter_than_a_tenth_of_a_second_behaves_as_linear() {
    let tm = tmap_of(600.0); // 4 拍 = 0.4 秒…用更快的 BPM 让块真的短
    let tm_fast = tmap_of(3000.0); // 4 拍 = 0.08 秒 < 0.1 秒
    let _ = tm;
    for easing in NAMES {
        let e = Event::new(Beat::zero(), Beat::new(4, 1), json!(0.0), json!(100.0), easing);
        assert_eq!(event_segments(&e, &tm_fast), 1, "{easing}：短块应当是一段");
        for k in 0..=20 {
            let beat = k as f64 * 0.2;
            let got = event_value(&e, beat, &tm_fast);
            let want = 100.0 * (beat / 4.0).clamp(0.0, 1.0);
            assert!(
                (got - want).abs() < 1e-9,
                "{easing} 在拍 {beat}：短块应当按线性走，实际 {got}，期望 {want}"
            );
        }
    }
}

/// **已有的线性谱面逐位不变**：`linear` 走的是同一条 `interp`，采样不许插一脚。
#[test]
fn linear_events_stay_bit_identical() {
    let tm = tmap_of(120.0);
    for (v0, v1) in [(0.0, 1.0), (7.0, 7.0), (-240.5, 675.0), (0.1, 0.3)] {
        let e = Event::new(Beat::zero(), Beat::new(8, 1), json!(v0), json!(v1), "linear");
        for k in 0..=100 {
            let beat = k as f64 * 0.08;
            let t = (beat / 8.0).clamp(0.0, 1.0);
            // 逐位相同：求值器必须走 `v0 + (v1−v0)·t` 这一条式子
            assert_eq!(
                event_value(&e, beat, &tm).to_bits(),
                (v0 + (v1 - v0) * t).to_bits(),
                "{v0}→{v1} 在拍 {beat} 上不该被采样动过"
            );
        }
    }
}

/// **段数跟着 BPM 走**（用户口径是"秒"，不是"拍"）：同一块在快 BPM 下段数少。
#[test]
fn the_segment_count_follows_the_tempo() {
    let e = Event::new(Beat::zero(), Beat::new(8, 1), json!(0.0), json!(100.0), "outCubic");
    // 8 拍：@120 = 4 秒 ⇒ 40 段；@240 = 2 秒 ⇒ 20 段；@60 = 8 秒 ⇒ 80 段
    assert_eq!(event_segments(&e, &tmap_of(120.0)), 40);
    assert_eq!(event_segments(&e, &tmap_of(240.0)), 20);
    assert_eq!(event_segments(&e, &tmap_of(60.0)), 80);
}

/// **折线有多接近解析曲线**：0.1 秒采样下的最大偏差（相对值域）。
///
/// 这是"0 偏差"那句话的**边界说明**：端点按位精确、节点上就是曲线上的值、
/// 段内积分精确；只有节点**之间**与解析曲线有偏差。这里把它量出来并钉一个上界 ——
/// 数字随缓动不同，值域都是 100，所以读出来就是"100 单位里差了多少"。
#[test]
fn the_polyline_deviation_from_the_analytic_curve_is_measured() {
    let tm = tmap_of(120.0); // 2 秒的块 ⇒ 20 段（0.1 秒一段）
    for easing in NAMES {
        let e = Event::new(Beat::zero(), Beat::new(4, 1), json!(0.0), json!(100.0), easing);
        let n = event_segments(&e, &tm);
        let mut worst = 0.0f64;
        let mut worst_u = 0.0;
        for k in 0..=2000 {
            let u = k as f64 / 2000.0;
            let beat = 4.0 * u;
            // 解析曲线（未经采样）
            let want = 100.0 * ease(easing, u);
            let got = event_value(&e, beat, &tm);
            let d = (got - want).abs();
            if d > worst {
                worst = d;
                worst_u = u;
            }
        }
        // **实测值**（2 秒的块 = 20 个 0.1 秒区间、值域 100）：上界留约 5% 余量。
        // 回弹类（back/elastic/bounce）的值是**采到回弹点之后**的数：bounce 族从
        // 7.7 / 7.7 / 8.6 掉到 0.87 / 0.88 / 1.76 —— 峰被削平的误差整块消失了。
        // 剩下的是节点**之间**的曲率误差，`circ` 族（弧线、端点处斜率竖直）最大。
        let bound = match easing {
            "linear" => 0.0,
            "outQuad" | "inQuad" => 0.07,
            "outSine" | "inSine" => 0.09,
            "inOutQuad" => 0.14,
            "inOutSine" => 0.17,
            "outCubic" | "inCubic" => 0.20,
            "outQuart" | "inQuart" | "inOutCubic" => 0.38,
            "outBack" | "inBack" => 0.41,
            "outQuint" | "inQuint" => 0.61,
            "inOutQuart" => 0.72,
            "outBounce" => 0.92,
            "inBounce" => 0.93,
            "inOutBack" => 1.01,
            "outExpo" | "inExpo" => 1.34,
            "inOutBounce" => 1.85,
            "inOutElastic" => 4.68,
            "outElastic" | "inElastic" => 5.66,
            "inOutCirc" => 5.95,
            "outCirc" | "inCirc" => 8.36,
            other => panic!("第 30 个缓动 {other} 还没量过偏差：量一次再把数写进来"),
        };
        assert!(
            worst <= bound,
            "{easing}（{n} 段）偏差 {worst:.4}（在 u={worst_u:.3}）超过上界 {bound}"
        );
        println!(
            "{easing:<14} 段数 {n:>3}  最大偏差 {worst:.6} @u={worst_u:.4}（值域 100，上界 {bound}）"
        );
    }

    // **偏差随块变长而迅速变小**：区间固定 0.1 秒，块越长、曲线在区间里越平（∝ 1/时长²）。
    // 用**单调**缓动量这个规律（它的节点结构不随时长变）；回弹类的节点结构会随时长变
    // （秒距变大 ⇒ 保留的回弹点变多、被顶掉的网格节点也变多），所以只要求它确实在变小。
    let dev = |name: &str, beats: i64, tm: &TimeMap| -> f64 {
        let e = Event::new(Beat::zero(), Beat::new(beats, 1), json!(0.0), json!(100.0), name);
        let mut worst = 0.0f64;
        for k in 0..=2000 {
            let u = k as f64 / 2000.0;
            worst = worst.max((event_value(&e, beats as f64 * u, tm) - 100.0 * ease(name, u)).abs());
        }
        worst
    };
    let (d2, d4, d8) = (dev("inOutCubic", 4, &tm), dev("inOutCubic", 8, &tm), dev("inOutCubic", 16, &tm));
    assert!(d2 > d4 && d4 > d8, "块越长偏差越小：{d2} / {d4} / {d8}");
    assert!(
        (d2 / d4 - 4.0).abs() < 0.2 && (d4 / d8 - 4.0).abs() < 0.2,
        "单调缓动的偏差应按 1/时长² 缩小（实测比值 {} / {}）",
        d2 / d4,
        d4 / d8
    );
    let (b2, b8) = (dev("inOutBounce", 4, &tm), dev("inOutBounce", 16, &tm));
    println!(
        "inOutCubic 偏差：2s {d2:.4} → 4s {d4:.4} → 8s {d8:.4}；inOutBounce：2s {b2:.4} → 8s {b8:.4}"
    );
    assert!(b2 / b8 > 4.0, "回弹类的偏差也要随块变长而变小（实测 {b2:.4} → {b8:.4}）");
}

// ---------------------------------------------------------------- 回弹点（非单调缓动）

/// 哪些缓动是**非单调**的（回弹类）：`back` / `elastic` / `bounce` 三族。
fn is_rebound(name: &str) -> bool {
    matches!(
        name,
        "inBack"
            | "outBack"
            | "inOutBack"
            | "inElastic"
            | "outElastic"
            | "inOutElastic"
            | "inBounce"
            | "outBounce"
            | "inOutBounce"
    )
}

/// **特判只给非单调函数**：单调的 20 个缓动必须没有回弹点（不能因为"多采几个点没坏处"
/// 就给所有缓动加节点 —— 那会让时间轴与流速分段凭空变复杂）。
#[test]
fn turning_points_exist_for_non_monotonic_easings_only() {
    for name in NAMES {
        let tp = turning_points(name);
        if !is_rebound(name) {
            assert!(tp.is_empty(), "{name} 是单调缓动，不该有回弹点：{tp:?}");
            continue;
        }
        assert!(!tp.is_empty(), "{name} 是回弹缓动，必须采到回弹点");
        for (i, &c) in tp.iter().enumerate() {
            assert!(c > 0.0 && c < 1.0, "{name} 的回弹点 {c} 跑到 (0,1) 外面了");
            if i > 0 {
                assert!(c > tp[i - 1], "{name} 的回弹点必须升序且互不相同：{tp:?}");
            }
            // 真的是极值：两侧差分异号（单调函数在任一点都过不了这一关）
            let h = 1e-5;
            let (l, m, r) = (ease(name, c - h), ease(name, c), ease(name, c + h));
            let is_extreme = (m >= l && m >= r) || (m <= l && m <= r);
            assert!(is_extreme, "{name} 的 {c} 不是极值：{l} / {m} / {r}");
        }
    }
}

/// 回弹点的**个数**（表的形状）与**解析值**（表的内容）都要对得上。
///
/// 表本身是数值扫描出来的（对 `ease` 的实现自带一致性），所以这里拿手推的解析式钉住它：
/// 扫描跑偏、或者将来有人给 `ease` 换了实现却忘了这张表，这条会红。
#[test]
fn the_detected_turning_points_match_the_analytic_ones() {
    let near = |a: f64, b: f64| (a - b).abs() < 1e-6;
    let has = |name: &str, x: f64| turning_points(name).iter().any(|&c| near(c, x));
    const C1: f64 = 1.701_58;
    const C3: f64 = C1 + 1.0;
    const C2: f64 = C1 * 1.525;
    // back：f' = (t−1)(3C3(t−1) + 2C1) ⇒ 峰在 1 − 2C1/(3C3)，镜像的是 in
    assert!(has("outBack", 1.0 - 2.0 * C1 / (3.0 * C3)));
    assert!(has("inBack", 2.0 * C1 / (3.0 * C3)));
    // inOutBack：h(s) = (C2+1)s³ − C2 s² 的极值 s* 压到两半里
    let s = 2.0 * C2 / (3.0 * (C2 + 1.0));
    assert!(has("inOutBack", s / 2.0) && has("inOutBack", 1.0 - s / 2.0));
    // bounce：三段抛物线的**接缝** k/2.75（折点）与各段**顶点**（谷）
    for k in [1.0, 2.0, 2.5, 1.5, 2.25, 2.625] {
        assert!(has("outBounce", k / 2.75), "outBounce 缺 {k}/2.75 这个点");
    }
    for k in [1.0, 2.0, 2.5, 1.5, 2.25, 2.625] {
        assert!(has("inBounce", 1.0 - k / 2.75), "inBounce 缺 1 − {k}/2.75 这个点");
    }
    // elastic：f' = 0 ⇔ tan θ = ±(20π/3)/(10 ln2) + kπ
    let c = (20.0 * std::f64::consts::PI / 3.0) / (10.0 * std::f64::consts::LN_2);
    for k in 0..6 {
        let theta = c.atan() + k as f64 * std::f64::consts::PI;
        let t = (theta / (2.0 * std::f64::consts::PI / 3.0) + 0.75) / 10.0;
        assert!(has("outElastic", t), "outElastic 缺第 {k} 个回弹点（解析值 {t}）");
        assert!(has("inElastic", 1.0 - t), "inElastic 缺第 {k} 个回弹点");
    }
    // 个数（含 inOut 的两半）
    assert_eq!(turning_points("outBack").len(), 1);
    assert_eq!(turning_points("inBack").len(), 1);
    assert_eq!(turning_points("inOutBack").len(), 2);
    assert_eq!(turning_points("outElastic").len(), 6);
    assert_eq!(turning_points("inElastic").len(), 6);
    assert_eq!(turning_points("inOutElastic").len(), 8);
    assert_eq!(turning_points("outBounce").len(), 6);
    assert_eq!(turning_points("inBounce").len(), 6);
    assert_eq!(turning_points("inOutBounce").len(), 12);
    // 热路径把回弹点装进**定长数组**（`perf::MAX_TURNS`）⇒ 表长必须留在上限里。
    // 将来某个缓动的回弹点爆了，这条会先红（而不是在求值器里被悄悄截断）。
    for name in NAMES {
        assert!(
            turning_points(name).len() <= opm_app::perf::MAX_TURNS,
            "{name} 的回弹点 {} 个，超过 MAX_TURNS={}",
            turning_points(name).len(),
            opm_app::perf::MAX_TURNS
        );
    }
}

/// **过冲峰必须落在折线上**：0.1 秒采样会把回弹类削平（`outBounce` 实测从 7.7 掉到 0.87
/// 个单位就是这个原因）—— 采到回弹点之后，折线的极值必须**等于**曲线的极值。
#[test]
fn the_rebound_extremes_land_exactly_on_the_polyline() {
    let tm = tmap_of(120.0); // 4 拍 = 2 秒
    for name in NAMES.iter().filter(|n| is_rebound(n)) {
        let e = Event::new(Beat::zero(), Beat::new(4, 1), json!(0.0), json!(100.0), name);
        let knots = event_knots(&e, &tm);
        let hi = knots.iter().map(|k| k.1).fold(f64::NEG_INFINITY, f64::max);
        let lo = knots.iter().map(|k| k.1).fold(f64::INFINITY, f64::min);
        // 曲线自己的极值（细扫，独立于节点表）
        let mut truth_hi = f64::NEG_INFINITY;
        let mut truth_lo = f64::INFINITY;
        for i in 0..=50_000 {
            let v = 100.0 * ease(name, i as f64 / 50_000.0);
            truth_hi = truth_hi.max(v);
            truth_lo = truth_lo.min(v);
        }
        assert!(
            (hi - truth_hi).abs() < 1e-6,
            "{name} 的过冲峰被削掉了：折线最高 {hi}，曲线最高 {truth_hi}"
        );
        assert!(
            (lo - truth_lo).abs() < 1e-6,
            "{name} 的下冲被削掉了：折线最低 {lo}，曲线最低 {truth_lo}"
        );
    }
}

/// **求值的热路径（不建表的夹逼）与节点表（`knot_us`）逐点一致** —— 两条路必须是同一条折线。
///
/// 热路径不分配、只用网格下标 + 回弹点扫一遍；节点表是"先建表再插值"的直白写法。
/// 只要有一处规则不对（顶掉哪个网格节点、回弹点怎么稀疏化），这里就会红。
#[test]
fn the_fast_bracket_matches_the_knot_list_everywhere() {
    for name in NAMES {
        for beats in [1_i64, 2, 3, 4, 6, 8, 13, 17] {
            let tm = tmap_of(120.0);
            let e = Event::new(Beat::zero(), Beat::new(beats, 1), json!(0.37), json!(-12.5), name);
            let us = knot_us(name, event_span_of(&e, &tm));
            let vals: Vec<f64> = event_knots(&e, &tm).iter().map(|k| k.1).collect();
            assert_eq!(us.len(), vals.len(), "{name}：节点表两条路长度不同");
            assert_eq!(us.first(), Some(&0.0));
            assert_eq!(us.last(), Some(&1.0));
            for i in 0..=2000 {
                let u = i as f64 / 2000.0;
                // 参照：在节点表上插值（节点表是"定义"，热路径必须与它一致）
                let j = us.partition_point(|x| *x <= u).saturating_sub(1).min(us.len() - 2);
                let (x0, x1) = (us[j], us[j + 1]);
                let want = vals[j] + (vals[j + 1] - vals[j]) * ((u - x0) / (x1 - x0));
                let got = event_value(&e, beats as f64 * u, &tm);
                assert!(
                    (got - want).abs() < 1e-9,
                    "{name}（{beats} 拍）在 u={u}：热路径 {got} ≠ 节点表 {want}"
                );
            }
        }
    }
}

/// **采样区间与合并规则对回弹点同样成立**：
///
/// · 任何相邻节点在秒上都**不小于 0.04 秒**（这是硬规则）；
/// · **一般**是 0.1 秒，只有两处会变长：① 末段吸收尾巴（≤ 0.1 + 0.04）；
///   ② 回弹点把两侧的网格节点各顶掉一个时（弹性族的回弹点在进度上相隔 0.15，
///   中间那两个网格节点分别离两端 0.015 / 0.035 秒 ⇒ 都被并掉）—— 这种变长只允许
///   发生在**回弹点旁边**，而且有上界。
///
/// ② 是"必须采到回弹点"与"区间不许变大"两条规则**冲突时**的取舍：合并规则是用户
/// 明写的例外（"小于 0.04 秒才让前一个区间合并它"），所以这里让区间变长、回弹点保留。
#[test]
fn knot_intervals_follow_the_merge_rule() {
    let mut worst_plain = 0.0_f64; // 与回弹点无关的最长区间（应当 ≤ 0.14）
    let mut worst_turn = 0.0_f64; // 挨着回弹点的最长区间
    for name in NAMES {
        // 两个速度：120 BPM 让块长是 0.25 秒的整数倍（一般不会触发末段合并），
        // 180 BPM 让余数散开（能撞上"末段不足 0.04 秒被并掉"那条路径）
        for (bpm, half_beats) in [(120.0_f32, 1_i64), (180.0, 1)] {
            for half_beats in half_beats..=40 {
            let tm = tmap_of(bpm);
            let e = Event::new(
                Beat::zero(),
                Beat::new(half_beats, 2),
                json!(0.0),
                json!(100.0),
                name,
            );
            let d = event_span_of(&e, &tm);
            let us = knot_us(name, d);
            if name == "linear" {
                assert_eq!(us, vec![0.0, 1.0], "linear 根本不采样：只要两个端点");
                continue;
            }
            if ease_segments(d) <= 1 {
                assert_eq!(us, vec![0.0, 1.0], "{name}：一段的块只有两个端点");
                continue;
            }
            let is_turn = |x: f64| turning_points(name).iter().any(|c| (c - x).abs() < 1e-9);
            for w in us.windows(2) {
                let secs = (w[1] - w[0]) * d;
                assert!(
                    secs >= EASE_MERGE_SEC - 1e-9,
                    "{name}（{d} 秒）：节点间隔 {secs} 秒短于合并阈值"
                );
                if is_turn(w[0]) || is_turn(w[1]) {
                    worst_turn = worst_turn.max(secs);
                    assert!(
                        secs <= 0.18 + 1e-9,
                        "{name}（{d} 秒）：挨着回弹点的间隔 {secs} 秒太长"
                    );
                } else {
                    worst_plain = worst_plain.max(secs);
                    assert!(
                        secs <= EASE_SEG_SEC + EASE_MERGE_SEC + 1e-9,
                        "{name}（{d} 秒）：与回弹点无关的间隔 {secs} 秒超过了 0.1 + 0.04"
                    );
                }
            }
            }
        }
    }
    println!("最长区间：与回弹点无关 {worst_plain:.6} 秒；挨着回弹点 {worst_turn:.6} 秒");
    assert!(worst_plain > 0.13, "用例本身要覆盖到末段合并那种 0.1~0.14 的区间");
}

/// **回弹点太近就按合并规则并掉**（"先到先得"）：块越短，回弹点在秒上越挤。
#[test]
fn too_close_rebound_points_get_merged() {
    let name = "outElastic";
    let raw = turning_points(name).len();
    let kept = |d: f64| -> usize {
        let us = knot_us(name, d);
        us.iter().filter(|u| turning_points(name).iter().any(|c| (c - *u).abs() < 1e-9)).count()
    };
    // 2 秒的块：回弹点秒距 0.15 × 2 = 0.3 秒 ⇒ 一个都不用并
    assert_eq!(kept(2.0), raw, "2 秒的块不该并掉回弹点");
    // 0.2 秒的块：秒距 0.03 秒 < 0.04 ⇒ 并掉一些（还有的落到端点附近被端点吸收）
    let short = kept(0.2);
    assert!(short < raw, "0.2 秒的块应当并掉一些回弹点，实际 {short}/{raw}");
    // 并完之后，间隔照样在 [0.04, 0.14] 秒里（这条由上面的通用断言覆盖，这里再点一次）
    let us = knot_us(name, 0.2);
    for w in us.windows(2) {
        let secs = (w[1] - w[0]) * 0.2;
        assert!(secs >= EASE_MERGE_SEC - 1e-9 && secs <= 0.14 + 1e-9, "间隔 {secs} 秒");
    }
}

/// **回弹过的流速也照样积得准**：折线的每一段都是线性 ⇒ 闭式积分是精确值。
///
/// 基准是**与实现无关**的极细梯形求积（1e-5 秒一步、只问 `event_value`）。
#[test]
fn a_curved_speed_ramp_integrates_exactly() {
    let tm = tmap_of(120.0);
    for name in ["outBounce", "inOutElastic", "outBack", "inOutCubic"] {
        // 2 秒的流速块：0 → 20（再回 0），缓动按折线
        let events = vec![Event::new(Beat::zero(), Beat::new(4, 1), json!(0.0), json!(20.0), name)];
        let table = opm_app::perf::SpeedTable::build(&events, &tm, 8.0);
        for sec in [0.05_f64, 0.3, 0.87, 1.5, 1.99, 2.4] {
            // 极细梯形求积（不经过任何"分段"逻辑）
            let steps = (sec / 1e-5).ceil() as usize;
            let dt = sec / steps as f64;
            let mut acc = 0.0;
            for k in 0..steps {
                let (a, b) = (k as f64 * dt, (k + 1) as f64 * dt);
                acc += 0.5
                    * (event_value(&events[0], tm.beat(a), &tm)
                        + event_value(&events[0], tm.beat(b), &tm))
                    * dt;
            }
            let want = acc * 120.0;
            let got = table.h_at(&tm, sec);
            assert!(
                (got - want).abs() < 1e-6,
                "{name} 在 {sec} 秒：查表 {got} ≠ 极细求积 {want}"
            );
        }
    }
}

// ---------------------------------------------------------------- 时间映射与事件求值

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
    let tm = tmap_of(120.0);
    // 4 拍 @120 = 2 秒 ⇒ 20 段：中点在节点上（k=10），所以下面的数值是**曲线上的值**
    let ev = vec![Event::new(Beat::zero(), Beat::new(4, 1), json!(0.0), json!(100.0), "inQuad")];
    assert_eq!(eval_events(&ev, 0.0, &tm), Some(0.0));
    assert_eq!(eval_events(&ev, 4.0, &tm), Some(100.0));
    // 中点：inQuad(0.5)=0.25 ⇒ 25
    let mid = eval_events(&ev, 2.0, &tm).unwrap();
    assert!((mid - 25.0).abs() < 1e-9, "中点 {mid}，期望 25（inQuad 而非线性插值）");
    // 换了缓动，中点就该不同 —— 证明缓动真的参与求值
    let lin = vec![Event::new(Beat::zero(), Beat::new(4, 1), json!(0.0), json!(100.0), "linear")];
    assert!((eval_events(&lin, 2.0, &tm).unwrap() - 50.0).abs() < 1e-9);
    // 事件之外取最近端（半成品数据也能算，不 panic）
    let gap = vec![Event::new(Beat::new(4, 1), Beat::new(8, 1), json!(7.0), json!(9.0), "linear")];
    assert_eq!(eval_events(&gap, 0.0, &tm), Some(7.0));
    assert_eq!(eval_events(&gap, 99.0, &tm), Some(9.0));
    assert_eq!(eval_events(&[], 0.0, &tm), None);
    // **空隙里必须保持前一条事件的终值**（"某时间没有事件则不移动判定线"）。
    // 这里曾经是错的：不在任何事件里时会返回"最后一条事件的终值"，于是线在空隙里直接跳过去。
    let gap = vec![
        Event::new(Beat::zero(), Beat::new(10, 1), json!(0.0), json!(5.0), "linear"),
        Event::new(Beat::new(20, 1), Beat::new(30, 1), json!(90.0), json!(99.0), "linear"),
    ];
    assert_eq!(eval_events(&gap, 0.0, &tm), Some(0.0));
    assert_eq!(eval_events(&gap, 15.0, &tm), Some(5.0), "空隙里应保持前一条的终值 5，而不是跳到 99");
    assert_eq!(eval_events(&gap, 19.9, &tm), Some(5.0));
    assert_eq!(eval_events(&gap, 20.0, &tm), Some(90.0), "进入下一条事件才变");
    assert_eq!(eval_events(&gap, 99.0, &tm), Some(99.0), "末条事件之后保持其终值");

    // 非数值（字符串/颜色轨道）不 panic，取 0
    let s = vec![Event::new(Beat::zero(), Beat::new(1, 1), json!("a"), json!("b"), "linear")];
    assert_eq!(eval_events(&s, 0.5, &tm), Some(0.0));
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

/// **时间轴画的就是求值的那条折线**：采样点 = 折线节点，一个不多一个不少。
#[test]
fn the_drawn_curve_is_the_evaluated_polyline() {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 180.0, foreign: Default::default() }];
    let tm = TimeMap::from_doc(&doc);
    let ev = vec![
        Event::new(Beat::zero(), Beat::new(2, 1), json!(0.0), json!(10.0), "linear"),
        Event::new(Beat::new(2, 1), Beat::new(4, 1), json!(10.0), json!(0.0), "outBounce"),
    ];
    let pts = sample_track(&ev, &tm);
    // 时间单调不减
    for w in pts.windows(2) {
        assert!(w[1][0] >= w[0][0], "采样时间应单调: {:?}", w);
    }
    // 每个采样点都必须在求值曲线上（不许多画一个"近似点"）
    for q in &pts {
        let beat = tm.beat(q[0] as f64);
        let v = if beat <= 2.0 {
            event_value(&ev[0], beat, &tm)
        } else {
            event_value(&ev[1], beat, &tm)
        };
        assert!(
            (q[1] as f64 - v).abs() < 1e-3,
            "采样点 {q:?}（拍 {beat}）不在求值曲线上（求值给 {v}）"
        );
    }
    // 折线节点总数：linear 事件 2 个点 + outBounce 事件的节点
    //（2 拍 @180 = 2/3 秒 ⇒ 7 个网格段；再插进 5 个回弹点、顶掉 3 个网格节点）
    let n = event_segments(&ev[1], &tm);
    assert_eq!(n, 7, "2 拍 @180 = 2/3 秒 ⇒ 7 个网格段");
    let knots = event_knots(&ev[1], &tm);
    assert_eq!(pts.len(), 2 + knots.len());
    assert!(knots.len() > n + 1, "回弹事件的节点比网格多（{} > {}）", knots.len(), n + 1);
    // outBounce 中点已明显低于线性中点（10 → 0 的中点是 5）
    let mid = event_value(&ev[1], 3.0, &tm);
    assert!(mid < 3.0, "outBounce 在中点应明显低于 5.0，实际 {mid}");
}

// ---------------------------------------------------------------- 五条轨道同一条路

/// **两条求值入口必须给出同一个表演状态**：`perf_at`（借用 `[Vec<Event>; 5]` 表）
/// 与视图侧的 `Line::perf`（借用 `TrackView`）。
///
/// 它们曾是两份手抄的五轨道循环，并在**流速**那条轨道上分家了：`perf_at` 走 `eval_events`
/// （认缓动），视图走 `track_value`（流速只按线性）。于是同一份谱面，`opm-ctl`/无头渲染
/// 看到的流速与界面/检查器看到的不是一个数 —— 现在两者都经 `perf::perf_of`，
/// 而流速也**认缓动了**（折线实现），这条分歧从根上不存在了。
#[test]
fn the_two_evaluation_entries_agree_even_with_an_eased_speed_event() {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 180.0, foreign: Default::default() }];
    doc.judge_lines.clear();
    let mut l = JudgeLine::default();
    l.layers[0].speed.push(Event::new(
        Beat::zero(),
        Beat::new(4, 1),
        json!(10.0),
        json!(2.0),
        "outBounce", // 现在是**真的生效**的缓动
    ));
    doc.judge_lines.push(l);

    let tm = TimeMap::from_doc(&doc);
    let st = opm_app::state::EditorState::new(opm_app::state::chart_from_doc(&doc));
    let tracks = opm_app::state::tracks_of(&doc, 0, &tm);
    // `perf_at` 要的是 `[Vec<Event>; 5]`（预先取好的事件表）；视图那份由 `tracks_of` 给
    let flat: [Vec<Event>; 5] = std::array::from_fn(|i| tracks[i].events.clone());
    for sec in [0.0, 0.15, 0.4, 0.9, 1.3, 5.0] {
        let a = perf_at(&flat, &tm, sec);
        let b = st.chart.lines[0].perf(&tm, sec);
        assert!(
            (a.speed - b.speed).abs() < 1e-6,
            "{sec}s：两条入口的流速不一致（{a:?} vs {b:?}）"
        );
        assert!((a.x - b.x).abs() < 1e-6 && (a.alpha - b.alpha).abs() < 1e-6);
    }
    // 4 拍 @180 = 4/3 秒 ⇒ 13 段（末段吸收 0.0333 秒）；中点不在节点上，所以取"曲线下"的值
    let ev = &tracks[4].events[0];
    assert_eq!(event_segments(ev, &tm), 13);
    // 端点按定义：起点 10、终点 2
    assert_eq!(track_value(&tracks[4].events, 0.0, &tm).unwrap(), 10.0);
    assert_eq!(track_value(&tracks[4].events, 4.0, &tm).unwrap(), 2.0);
    // 中点（第 2 秒 = 拍 4 的一半…用拍 2）**不等于**线性中值 —— 缓动真的在起作用
    let mid = track_value(&tracks[4].events, 2.0, &tm).unwrap();
    assert!((mid - 6.0).abs() > 0.05, "流速中点应当偏离线性中值 6.0，实际 {mid}");
}

/// 本文件顶部那份 `NAMES` 与 `spec/easing.json` 必须一致。
///
/// 重列一遍是**刻意**的（测试不该依赖被测实现里的表），但"刻意重列"和"悄悄过期"只差一步：
/// 加第 30 个缓动时这里不会红，直到有人发现"新缓动没被测过"。所以拿 spec 钉住它 ——
/// 仍然不读实现，读的是数据源。
#[test]
fn the_relisted_easing_names_match_the_spec_file() {
    let spec: serde_json::Value =
        serde_json::from_str(include_str!("../../spec/easing.json")).expect("spec JSON");
    let mut want: Vec<&str> = spec["easings"]
        .as_array()
        .expect("spec 里有 easings 数组")
        .iter()
        .map(|e| e["name"].as_str().expect("每个缓动都有 name"))
        .collect();
    want.sort_unstable();
    let mut got: Vec<&str> = NAMES.to_vec();
    got.sort_unstable();
    assert_eq!(got, want, "重列的 29 个名字要与 spec 一致");
}
