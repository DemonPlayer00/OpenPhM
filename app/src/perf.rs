// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! 表演求值：**拍 ↔ 秒**的时间映射 + 29 个具名缓动的函数本体 + 事件轨道求值。
//!
//! 判定线是这个编辑器的父对象：它的位置/旋转/透明度/流速全部来自**事件轨道**，
//! 音符只是挂在它下面的子对象（跟着线一起平移/旋转）。因此"事件真的生效"是前提 ——
//! 本模块就是那个前提。
//!
//! 三处口径（与 `spec/` 对齐）：
//! · 缓动函数本体用通用实现（easings.net 命名），编号语义依据 Phira Documents —— 见 `spec/easing.json`；
//! · 时间映射按 `bpmList` 分段线性：第 i 段从 `bpmList[i].startBeat` 起、按 `bpmList[i].bpm` 走，
//!   到下一段起点为止。早先只按首个 BPM 换算，多 BPM 谱面会整体跑偏（本轮补上）；
//! · **一切缓动都按"折线"实现**（用户口径，见 [`ease_segments`] / [`knot_us`]）：从块开头起
//!   每 0.1 秒一个节点、节点之间线性，首尾节点的值就是 `startValue`/`endValue`，而
//!   **非单调缓动（回弹类）的回弹点/折点必须落在节点上**（[`turning_points`]）。于是
//!   ① 五条轨道走**同一条**求值路径（流速因此也有了曲线缓动）；
//!   ② 每段都是线性的 ⇒ 流速积分 `∫v dτ` 仍是**闭式精确解**，没有抽样误差、也不累加；
//!   ③ 端点按定义取端值、回弹点取曲线上的峰值 ⇒ "块末就位"的 0 误差与过冲都不受采样影响。
//!   代价是节点**之间**与解析曲线有偏差（0.1 秒一段，实测表在 `tests/perf.rs`）——
//!   这是**定义**上的选择：折线就是这支缓动的真相，不是"算不准"。

use crate::doc::{Beat, Document, Event, JudgeLine, Layer};
use serde_json::Value;
use std::sync::OnceLock;

// ---------------------------------------------------------------- 时间映射

/// 拍 → 秒的分段线性映射（由 `bpmList` 决定）
#[derive(Clone, Debug)]
pub struct TimeMap {
    /// (起始拍, bpm, 该起始拍对应的秒)
    segs: Vec<(f64, f64, f64)>,
    /// 谱面末尾（拍）与对应秒数（含尾巴，供时间轴/播放头使用）
    pub end_beat: f64,
    pub duration: f64,
}

impl TimeMap {
    pub fn from_doc(doc: &Document) -> Self {
        Self::from_parts(&doc.bpm_list, doc.chart_end())
    }

    /// 直接由 `bpmList` + 谱面末尾拍建表。
    ///
    /// 单独开这个入口是因为**导入中途**也要用它：RPE 导入是先读 BPM 表、再读判定线，
    /// 读到事件时要规范化重叠（切点上的值要问求值器，而求值器现在需要秒长 ⇒ 需要本表），
    /// 那时 `Document` 还没拼好。
    pub fn from_parts(bpm_list: &[crate::doc::BpmEntry], end_beat: Beat) -> Self {
        let mut raw: Vec<(f64, f64)> = bpm_list
            .iter()
            .map(|b| (b.start.to_f64(), (b.bpm as f64).max(1.0)))
            .collect();
        if raw.is_empty() {
            raw.push((0.0, 180.0));
        }
        raw.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut segs = Vec::with_capacity(raw.len());
        let mut sec = 0.0;
        for (i, (beat, bpm)) in raw.iter().enumerate() {
            if i > 0 {
                let (pb, pbpm, _) = segs[i - 1];
                sec += (beat - pb) * 60.0 / pbpm;
            }
            segs.push((*beat, *bpm, sec));
        }
        let end_beat = end_beat.to_f64().max(segs.last().map(|s| s.0).unwrap_or(0.0));
        let mut me = Self { segs, end_beat, duration: 0.0 };
        me.duration = me.sec(end_beat) + 2.0;
        me
    }

    /// 拍 → 秒
    pub fn sec(&self, beat: f64) -> f64 {
        let mut cur = self.segs[0];
        for s in &self.segs {
            if beat >= s.0 {
                cur = *s;
            } else {
                break;
            }
        }
        cur.2 + (beat - cur.0) * 60.0 / cur.1
    }

    /// 秒 → 拍（时间轴上"在播放头处加事件"要用）
    pub fn beat(&self, sec: f64) -> f64 {
        let mut cur = self.segs[0];
        for s in &self.segs {
            let at = s.2;
            if sec >= at {
                cur = *s;
            } else {
                break;
            }
        }
        cur.0 + (sec - cur.2) * cur.1 / 60.0
    }

    /// 该时刻的 BPM（时间轴刻度用）
    pub fn bpm_at(&self, sec: f64) -> f64 {
        let mut cur = self.segs[0];
        for s in &self.segs {
            if sec >= s.2 {
                cur = *s;
            } else {
                break;
            }
        }
        cur.1
    }

    pub fn seg_count(&self) -> usize {
        self.segs.len()
    }

    /// 严格大于 `beat` 的**下一个 BPM 段起点**（没有 ⇒ `None`）。
    ///
    /// 流速积分要在这些点上**切开**：闭式积分 `(v₀+v₁)/2 × Δt` 只对"流速在**秒域**上线性"
    /// 成立，而 BPM 变化处拍↔秒折了一下（流速在 *拍* 域是线性的，在秒域是折线）——
    /// 在那个点上取端点平均就不准了。
    pub fn next_seg_start(&self, beat: f64) -> Option<f64> {
        self.segs
            .iter()
            .map(|s| s.0)
            .find(|b| *b > beat + 1e-12)
    }
}

// ---------------------------------------------------------------- 缓动

/// 29 个具名缓动的函数本体（easings.net 通用实现，与 `spec/easing.json` 的 id 一一对应）。
/// 名字不认识时退回线性 —— 校验器会先拦下未知名字，这里的退回只为"能用"。
pub fn ease(name: &str, t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    // **端点是定义，不是近似**：任何缓动都恰好始于 0、终于 1。
    // 浮点实现会差最后一两位（实测 `inSine` 的 `ease(1)` = 1 − 1.1e-16、`inBack` = 1 − 2.2e-16、
    // `outBack` 的 `ease(0)` = 2.2e-16），而这一两位经 `v0 + (v1−v0)·ease` 放大就成了
    // "**块末的值 ≠ 你写的 endValue**" —— 于是"把线精确放到目标坐标"没有构造性保证
    //（见检查器的「就位目标」与 §7.76 的端点分析）。
    //
    // 只夹**端点**：中间一律不动 —— `back`/`elastic` 的**过冲**（ease 值 > 1 或 < 0）是它们的本意，
    // 夹了就等于把这两个缓动删掉。
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    use std::f64::consts::PI;
    const C1: f64 = 1.701_58;
    const C3: f64 = C1 + 1.0;
    const C2: f64 = C1 * 1.525;
    let out_bounce = |t: f64| -> f64 {
        const N1: f64 = 7.5625;
        const D1: f64 = 2.75;
        if t < 1.0 / D1 {
            N1 * t * t
        } else if t < 2.0 / D1 {
            let t = t - 1.5 / D1;
            N1 * t * t + 0.75
        } else if t < 2.5 / D1 {
            let t = t - 2.25 / D1;
            N1 * t * t + 0.9375
        } else {
            let t = t - 2.625 / D1;
            N1 * t * t + 0.984375
        }
    };
    match name {
        "linear" => t,
        "inSine" => 1.0 - (t * PI / 2.0).cos(),
        "outSine" => (t * PI / 2.0).sin(),
        "inOutSine" => (1.0 - (PI * t).cos()) / 2.0,
        "inQuad" => t * t,
        "outQuad" => 1.0 - (1.0 - t) * (1.0 - t),
        "inOutQuad" => {
            if t < 0.5 {
                2.0 * t * t
            } else {
                1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
            }
        }
        "inCubic" => t * t * t,
        "outCubic" => 1.0 - (1.0 - t).powi(3),
        "inOutCubic" => {
            if t < 0.5 {
                4.0 * t * t * t
            } else {
                1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
            }
        }
        "inQuart" => t.powi(4),
        "outQuart" => 1.0 - (1.0 - t).powi(4),
        "inOutQuart" => {
            if t < 0.5 {
                8.0 * t.powi(4)
            } else {
                1.0 - (-2.0 * t + 2.0).powi(4) / 2.0
            }
        }
        "inQuint" => t.powi(5),
        "outQuint" => 1.0 - (1.0 - t).powi(5),
        "inExpo" => {
            if t == 0.0 {
                0.0
            } else {
                2.0_f64.powf(10.0 * t - 10.0)
            }
        }
        "outExpo" => {
            if t == 1.0 {
                1.0
            } else {
                1.0 - 2.0_f64.powf(-10.0 * t)
            }
        }
        "inCirc" => 1.0 - (1.0 - t * t).max(0.0).sqrt(),
        "outCirc" => (1.0 - (t - 1.0).powi(2)).max(0.0).sqrt(),
        "inOutCirc" => {
            if t < 0.5 {
                (1.0 - (1.0 - (2.0 * t).powi(2)).max(0.0).sqrt()) / 2.0
            } else {
                ((1.0 - (-2.0 * t + 2.0).powi(2)).max(0.0).sqrt() + 1.0) / 2.0
            }
        }
        "inBack" => C3 * t * t * t - C1 * t * t,
        "outBack" => 1.0 + C3 * (t - 1.0).powi(3) + C1 * (t - 1.0).powi(2),
        "inOutBack" => {
            if t < 0.5 {
                ((2.0 * t).powi(2) * ((C2 + 1.0) * 2.0 * t - C2)) / 2.0
            } else {
                ((2.0 * t - 2.0).powi(2) * ((C2 + 1.0) * (t * 2.0 - 2.0) + C2) + 2.0) / 2.0
            }
        }
        "inElastic" => {
            if t == 0.0 {
                0.0
            } else if t == 1.0 {
                1.0
            } else {
                -(2.0_f64.powf(10.0 * t - 10.0)) * ((t * 10.0 - 10.75) * (2.0 * PI / 3.0)).sin()
            }
        }
        "outElastic" => {
            if t == 0.0 {
                0.0
            } else if t == 1.0 {
                1.0
            } else {
                2.0_f64.powf(-10.0 * t) * ((t * 10.0 - 0.75) * (2.0 * PI / 3.0)).sin() + 1.0
            }
        }
        "inOutElastic" => {
            if t == 0.0 {
                0.0
            } else if t == 1.0 {
                1.0
            } else if t < 0.5 {
                -(2.0_f64.powf(20.0 * t - 10.0) * ((20.0 * t - 11.125) * (2.0 * PI / 4.5)).sin())
                    / 2.0
            } else {
                (2.0_f64.powf(-20.0 * t + 10.0) * ((20.0 * t - 11.125) * (2.0 * PI / 4.5)).sin())
                    / 2.0
                    + 1.0
            }
        }
        "inBounce" => 1.0 - out_bounce(1.0 - t),
        "outBounce" => out_bounce(t),
        "inOutBounce" => {
            if t < 0.5 {
                (1.0 - out_bounce(1.0 - 2.0 * t)) / 2.0
            } else {
                (1.0 + out_bounce(2.0 * t - 1.0)) / 2.0
            }
        }
        _ => t,
    }
}

// ---------------------------------------------------------------- 缓动的折线实现
//
// 缓动不是"求值时套一个函数"，而是**折线**（用户口径）：
// · 从块开头起**每 0.1 秒**一个节点，节点之间线性；
// · **采样区间不允许变大** —— 只有末段能吸收"不足 0.04 秒"的尾巴（于是最长 0.14 秒）；
// · **非单调缓动（回弹类）必须采到回弹点/折点**（[`turning_points`]）：`back`/`elastic`/`bounce`
//   的过冲峰与折角如果落在两节点之间就会被削平；回弹点彼此太近时同样按合并规则**先到先得**；
// · 首尾节点就是 `startValue` / `endValue`（按定义，不做算术）；
// · 整块不足 0.1 秒 ⇒ 一段 ⇒ "等价于 linear 缓动"（一段的折线就是直线）。
//
// 好处：五条轨道（**含流速**）走同一条求值路径；每段线性 ⇒ 流速积分仍是闭式精确解。
// 代价：节点**之间**与解析曲线有偏差 —— 端点、回弹点、段内积分都是精确的，
// 偏差只活在节点之间的直线里（实测表见 `tests/perf.rs`）。

/// 折线的采样周期（秒）：缓动曲线从块开头起，每这么长取一个节点。
pub const EASE_SEG_SEC: f64 = 0.1;

/// 末段短于这个秒数就**并入前一段**（免得留一个没用的极小段）。
pub const EASE_MERGE_SEC: f64 = 0.04;

/// 单块的段数上限。段数只随**秒长**线性增长（10 分钟的一块 = 6000 段），正常谱面到不了；
/// 这是给"病态长块"的护栏，免得一次 `speed_segments` 就申请出天文数字的切点。
pub const EASE_SEG_MAX: usize = 4096;

/// 一块**秒**时长 `d` 的事件要切成几段（用户口径）：
///
/// · 从块开头起每 [`EASE_SEG_SEC`]（0.1 秒）一个节点，采样区间**不允许**变大；
/// · 末尾余量不足 [`EASE_MERGE_SEC`]（0.04 秒）时**并入前一段**（于是只有最后一段更长，
///   最长 0.1 + 0.04 = 0.14 秒）；
/// · 整块不足 0.1 秒 ⇒ **一段**，也就是"等价于 linear 缓动"（一段的折线就是直线）。
///
/// 段数只由**秒**时长决定 ⇒ 同一块在不同 BPM 下段数不同，改 BPM 会自动跟着变。
/// 这也正是求值器必须拿到 [`TimeMap`] 的原因。**不含**回弹点带来的额外切分
/// （节点总数见 [`event_knots`]）。
pub fn ease_segments(d: f64) -> usize {
    if !d.is_finite() || d <= 0.0 {
        return 1;
    }
    let q = d / EASE_SEG_SEC;
    if q < 1.0 {
        return 1;
    }
    let base = q.floor() as usize; // ≥ 1（浮点 → 整数是饱和转换，天文数字不会 UB）
    let rem = d - base as f64 * EASE_SEG_SEC;
    let n = if rem < EASE_MERGE_SEC { base } else { base + 1 };
    n.clamp(1, EASE_SEG_MAX)
}

/// 网格参数 —— `(进度步长 s, 段数 n, 合并阈值 m)`，全在**事件进度** `u` 域里。
///
/// 位置用进度记（`u_k = k × 0.1 / 块秒长`）而不是秒：进度按拍线性推进，块内 BPM 不变时
/// 这些节点在秒上就是严格的 0.1 秒一个（块内正好有变速时按拍等分，秒距随速度比缩放）；
/// 段数仍按块的真实**秒长**算。于是求值只要块的两个秒端点，热路径一次 `tmap.beat` 都不用做。
///
/// `linear`（以及零长/非有限/一段的块）给"一段 + 无限阈值"：折线就是那条直线，
/// 回弹点全被并掉、采样也不需要 —— 这是"整块不足 0.1 秒 ⇒ 等价 linear"的实现方式。
fn grid_params(name: &str, d: f64) -> (f64, usize, f64) {
    if name == "linear" || !d.is_finite() || d <= 0.0 {
        return (1.0, 1, f64::INFINITY);
    }
    let n = ease_segments(d);
    if n <= 1 {
        return (1.0, 1, f64::INFINITY);
    }
    (EASE_SEG_SEC / d, n, EASE_MERGE_SEC / d)
}

/// 缓动的**回弹点 / 折点**（进度域，升序，(0,1) 内）。
///
/// 单调缓动（29 个里的 20 个）是**空表** —— 特判只给非单调函数：`back` / `elastic` / `bounce`
/// 三族（含 in / out / inOut）。表的来源是 [`scan_turning_points`]，**每个名字只算一次**。
pub fn turning_points(name: &str) -> &'static [f64] {
    static CACHE: OnceLock<Vec<(&'static str, Vec<f64>)>> = OnceLock::new();
    let table = CACHE.get_or_init(|| {
        crate::codec::easing_names()
            .into_iter()
            .map(|n| (n, scan_turning_points(n)))
            .collect()
    });
    table.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_slice()).unwrap_or(&[])
}

/// 数值求极值点：扫一遍 `ease` 的差分，符号变化处用三分法细化到机器精度。
///
/// 为什么扫而不逐个族推解析式：这张表**每个名字只算一次**（`OnceLock` 缓存，不在热路径），
/// 却对 `ease` 的实现**自带一致性** —— 手推六族的导数迟早会和实现分家（尤其 bounce 的分段常数）。
/// 扫出来之后再用解析值钉住（`outBack` 的峰在 `1 − 2C1/(3C3)`、bounce 的折点在 `k/2.75`…），
/// 那一步在 `tests/perf.rs` 里。
///
/// **跳过首尾两格**：端点是"定义"出来的（`ease` 在 `t ≤ 0` / `t ≥ 1` 直接返回 0 / 1），
/// 紧挨端点的"极值"是那个定义的影子（实测 `outElastic` 在 `t → 1` 处就是这样），
/// 不是曲线自己的回弹点。
fn scan_turning_points(name: &str) -> Vec<f64> {
    const N: usize = 4096;
    let x = |k: usize| k as f64 / N as f64;
    let mut out: Vec<f64> = Vec::new();
    // 从 k = 2 起、且第一个"前一格差分"取 `d_1`：贴着端点的 `d_0` 是**被定义的影子**
    //（实测 `inElastic` 的 `d_0` 是负的、之后全为正，会凭空多报一个回弹点）
    let mut d_prev = ease(name, x(2)) - ease(name, x(1));
    for k in 2..N - 1 {
        let d = ease(name, x(k + 1)) - ease(name, x(k));
        if d * d_prev < 0.0 {
            // 极值夹在 [x(k−2), x(k+2)] 里（多留一格：极平的地方差分符号会抖一格）。
            // 三分法只会找**极大**，所以先判这一格是峰还是谷 —— `inBack` 的谷非常平，
            // 方向写反时三分法会一路收敛到区间边缘（实测差 2.7e-4，峰就被削掉一点）。
            // 括号**不许碰到被定义过的端点** —— 否则会把"端点定义"的影子当成极值
            //（实测 `outElastic` 会在 t≈1 处多报一个）
            let (mut lo, mut hi) = (x(k.saturating_sub(2).max(1)), x((k + 2).min(N - 1)));
            let sign = if ease(name, x(k)) > ease(name, x(k - 1)) { 1.0 } else { -1.0 };
            for _ in 0..100 {
                let m1 = lo + (hi - lo) / 3.0;
                let m2 = hi - (hi - lo) / 3.0;
                if sign * ease(name, m1) < sign * ease(name, m2) {
                    lo = m1;
                } else {
                    hi = m2;
                }
            }
            out.push(0.5 * (lo + hi));
        }
        d_prev = d;
    }
    // 平台型极值可能被相邻两格各报一次 ⇒ 去重
    out.dedup_by(|a, b| (*a - *b).abs() < 1.5 / N as f64);
    out
}

/// 单支缓动的回弹点个数上限（数组上限，热路径不建 `Vec`）。
/// 实测最多的是 `inOutBounce` 的 12 个；这条由测试钉住，将来爆了会先在这里红。
pub const MAX_TURNS: usize = 32;

/// **保留**的回弹点，装进定长数组（热路径用；`Vec` 版本见 [`retained_turns`]）。
///
/// 求值的热路径（[`knot_bracket`] 在循环里问"这个网格节点被顶掉了吗"）会反复要这份列表，
/// 一次堆分配就能把"现算"那颗音符的代价翻几倍。
fn retained_turns_arr(name: &str, m: f64) -> ([f64; MAX_TURNS], usize) {
    let mut arr = [0.0f64; MAX_TURNS];
    let mut len = 0usize;
    let mut last = f64::NEG_INFINITY;
    for &c in turning_points(name) {
        if c < m || c > 1.0 - m || c - last < m {
            continue;
        }
        if len == MAX_TURNS {
            break;
        }
        arr[len] = c;
        len += 1;
        last = c;
    }
    (arr, len)
}

/// **保留**的回弹点（"必须采到"的那些）：
/// · 距端点不足一个合并阈值的丢掉 —— 端点比回弹点更硬（否则块首会凭空跳一下）；
/// · 相互之间不足一个合并阈值的，**先到先得**（后一个并进前一个的区间）。
///
/// 阈值 `m` 用进度单位（`0.04 秒 ÷ 块秒长`），所以同一块在快 BPM 下会并掉更多回弹点 ——
/// 这正是"同样遵循合并规则"。
fn retained_turns(name: &str, m: f64) -> impl Iterator<Item = f64> + 'static {
    let (arr, len) = retained_turns_arr(name, m);
    (0..len).map(move |i| arr[i])
}

/// 折线的**内部**节点（进度域，`0 < u < 1`）：网格节点（被回弹点顶掉的除外）+ 保留的回弹点。
///
/// **顺序不保证**（网格与回弹点各走一路，互不排序）—— 要升序自己排（[`knot_us`] 就是那么做的）。
/// 只想知道"有哪些切点"的地方（[`speed_segments`]，它本来就要排序去重）因此**不必建表** ——
/// 一条流速事件在每个查询上各建一次节点表，实测会把"现算"那一列从 ~60 ns 拖到 ~200 ns。
/// 节点集合的定义只此一份：[`knot_us`] 与流速切点都从它出发。
fn knot_u_inner(name: &str, d: f64, mut push: impl FnMut(f64)) {
    let (s, n, m) = grid_params(name, d);
    for k in 1..n {
        let g = k as f64 * s;
        if !retained_turns(name, m).any(|c| (c - g).abs() < m) {
            push(g);
        }
    }
    for c in retained_turns(name, m) {
        push(c);
    }
}

/// 折线的**节点集合**（进度域，升序，含 0 与 1）—— "节点"这件事的唯一定义。
///
/// 网格 ∪ 保留的回弹点；**被回弹点顶掉的网格节点不要**（相距不足 `m` 时网格让位），
/// 于是每个保留的回弹点都**真的在折线上**（它的过冲峰不会被削掉）。
/// 求值走 [`knot_bracket`]（同一条规则、零分配），两者由测试逐点对账。
pub fn knot_us(name: &str, d: f64) -> Vec<f64> {
    let mut out = Vec::new();
    knot_us_into(name, d, &mut out);
    out
}

/// 同上，但写进**调用方给的缓冲**（先清空）—— 逐事件循环里用它，省掉"每条事件一个 `Vec`"。
///
/// 时间轴折线一条轨道上就有成百上千条事件：每条事件各分配一次的话，光分配器就是一笔
/// 看得见的开销（实测 2000 条事件时 `tracks_of` 从 0.41 ms 降到 0.17 ms）。
pub fn knot_us_into(name: &str, d: f64, out: &mut Vec<f64>) {
    out.clear();
    out.push(0.0);
    knot_u_inner(name, d, |u| out.push(u));
    out.push(1.0);
    out.sort_by(|a, b| a.partial_cmp(b).unwrap());
    out.dedup_by(|a, b| (*a - *b).abs() < 1e-12);
}

/// 夹住进度 `u` 的两个节点 —— 与 [`knot_us`] 同一条规则，但**不建表、不分配**（热路径用）。
fn knot_bracket(name: &str, d: f64, u: f64) -> (f64, f64) {
    let (s, n, m) = grid_params(name, d);
    // 回弹点取一次（定长数组）：下面的循环会反复问"这个网格节点被顶掉了吗"
    let (turns, nt) = retained_turns_arr(name, m);
    let dropped = |g: f64| (0..nt).any(|i| (turns[i] - g).abs() < m);
    let j = ((u / s).floor() as isize).clamp(0, n as isize - 1) as usize;
    // 左：从所在格往左找第一个没被回弹点顶掉的网格节点（第 0 个是锚点，一定找得到）
    let mut lo = 0.0;
    let mut i = j as isize;
    while i >= 0 {
        let g = i as f64 * s;
        if i == 0 || !dropped(g) {
            lo = g;
            break;
        }
        i -= 1;
    }
    // 右：往右找第一个没被顶掉的网格节点（块尾是锚点）
    let mut hi = 1.0;
    let mut k = j + 1;
    while k < n {
        let g = k as f64 * s;
        if !dropped(g) {
            hi = g;
            break;
        }
        k += 1;
    }
    // 回弹点：夹住 u 的最近两个（可能比网格节点更近）
    for i in 0..nt {
        let c = turns[i];
        if c <= u {
            lo = lo.max(c);
        } else {
            hi = hi.min(c);
        }
    }
    (lo, hi)
}

/// 节点 `x`（进度）处的**值**：0 / 1 就是两个端点（按定义，不做算术），其余取解析曲线上的值。
///
/// 回弹点也是节点 ⇒ 过冲峰处的值就是**曲线上的峰值**，不是"两节点之间被削平的那个"。
fn node_value(e: &Event, x: f64) -> f64 {
    if x <= 0.0 {
        if let Some(v) = endpoint_value(e, 0.0) {
            return v;
        }
    } else if x >= 1.0 {
        if let Some(v) = endpoint_value(e, 1.0) {
            return v;
        }
    }
    interp(e, ease(&e.easing, x))
}

// ---------------------------------------------------------------- 事件求值

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// **拍 `beat` 处生效的是哪条事件** —— 起点**不晚于** `beat` 的最后一条（`events` 按起点升序）。
///
/// 这一条是"运行时预览"与"重新加载之后的预览"必须一致的地方，规则直接取自导入侧的
/// `codec::normalize_track`（排序 → 重叠时**保留后一条事件的起点** → 末事件延拓到谱尾）：
///
/// · **重叠**：后一条从它的起点起生效，前一条到此为止 —— 于是"往一条长事件里再放一条"
///   立刻见效，不必重新加载（用户报的正是这个：安放第二个变速事件要重载才有变化）；
/// · **空隙**：最后一条起点 ≤ `beat` 的事件仍生效，取值**夹在它自己的终点上**（= 前值延拓）；
/// · **末尾之后**：同上，末事件的终值一直保持到谱尾（`normalize` 会把它延拓到谱尾）。
///
/// 返回 `None` 只在"`beat` 早于第一条事件"时发生 —— 调用方按"取首条的起始值"处理。
pub fn active_event(events: &[Event], beat: f64) -> Option<usize> {
    let i = events.partition_point(|e| e.start.to_f64() <= beat + 1e-12);
    i.checked_sub(1)
}

/// 在给定**拍**处对一条事件轨道求值（用事件自己的缓动，不是线性插值）。
///
/// 生效的是 [`active_event`]（起点最晚且不晚于 `beat` 的那条），取值由 `event_value` 把
/// `t` 夹到 `[0,1]` —— 于是"空隙里保持前值""末尾之后保持终值"自然成立，
/// 而**重叠时后一条说了算**（与重新加载后 `normalize` 的结果一致）。
pub fn eval_events(events: &[Event], beat: f64, tmap: &TimeMap) -> Option<f64> {
    if events.is_empty() {
        return None;
    }
    match active_event(events, beat) {
        Some(i) => Some(event_value(&events[i], beat, tmap)),
        // 第一条事件之前：取它的起始值（`normalize` 会在这里补一条常量事件，值相同）
        None => as_f64(&events[0].start_value).or(Some(0.0)),
    }
}

/// 事件在拍 `beat` 处的**归一化进度** `t`（夹在 `[0,1]`；零长事件按 `1e-9` 兜底）。
///
/// 夹取就是"空隙里保持前值、末尾之后保持终值"的来源（见 [`eval_events`]）。
fn event_t(e: &Event, beat: f64) -> f64 {
    let (a, b) = (e.start.to_f64(), e.end.to_f64());
    let span = (b - a).max(1e-9);
    ((beat - a) / span).clamp(0.0, 1.0)
}

/// 按进度 `t` 在两个端点之间取值（缺哪一端就退化成另一端；两端都缺 ⇒ 0）。
///
/// 与 [`event_t`] 一起构成"取值"的**唯一一份**实现：一切求值最后都落到它这里，
/// 端点/缺值规则没有第二条路。
fn interp(e: &Event, t: f64) -> f64 {
    match (as_f64(&e.start_value), as_f64(&e.end_value)) {
        (Some(v0), Some(v1)) => v0 + (v1 - v0) * t,
        (Some(v0), None) => v0,
        (None, Some(v1)) => v1,
        (None, None) => 0.0,
    }
}

/// **两端都在时的端点取值**：`t ≤ 0` ⇒ `startValue`、`t ≥ 1` ⇒ `endValue`，**不做任何算术**。
///
/// 为什么单列出来：即使 `ease(1)` 已经精确等于 1，`v0 + (v1−v0)·1.0` 在浮点下仍可能差 1 ulp
/// （`0.1 + (0.3−0.1) = 0.30000000000000004 ≠ 0.3`）。而"**块末就位**"（检查器的目标设置）
/// 要的正是**按位相等** —— 于是端点直接取端值。折线实现同样从它过：折线的首尾节点就是端值，
/// 所以采样**不会**把端点精度换掉。
///
/// 缺端点（`None`）时不介入：那种情形 [`interp`] 返回的是常量，本来就没有算术误差。
fn endpoint_value(e: &Event, t: f64) -> Option<f64> {
    match (as_f64(&e.start_value), as_f64(&e.end_value)) {
        (Some(v0), Some(_)) if t <= 0.0 => Some(v0),
        (Some(_), Some(v1)) if t >= 1.0 => Some(v1),
        _ => None,
    }
}

/// 一块事件折成几**网格**段 —— `linear` 与"整块不足 0.1 秒"都给 1 段（折线就是直线）。
///
/// 秒长来自 `tmap`，所以**改 BPM 会自动改段数**（用户口径是"每 0.1 秒"，不是"每多少拍"）。
/// 非有限/零长/倒挂的块也给 1 段，与 [`event_t`] 对它们的兜底口径一致。
/// **回弹点带来的额外切分不算在这里**（节点总数见 [`event_knots`]）。
pub fn event_segments(e: &Event, tmap: &TimeMap) -> usize {
    grid_params(&e.easing, event_span(e, tmap)).1
}

/// 块的**秒**时长（拍域两端过一遍拍↔秒映射）。
fn event_span(e: &Event, tmap: &TimeMap) -> f64 {
    tmap.sec(e.end.to_f64()) - tmap.sec(e.start.to_f64())
}

/// 单条事件在拍 `beat` 处的值（按它自己的缓动，缓动按**折线**实现 —— 见模块头）。
///
/// 从 [`eval_events`] 里抽出来的：流速积分要在**段内**反复求值，
/// 值怎么算只该有一份实现 —— 两份实现迟早会在某个缓动上分家。
pub fn event_value(e: &Event, beat: f64, tmap: &TimeMap) -> f64 {
    let t = event_t(e, beat);
    if let Some(v) = endpoint_value(e, t) {
        return v;
    }
    // `linear` 先走 —— 它连"这块多长"都不用问（于是线性谱面的求值代价与以前逐位一致）
    if e.easing == "linear" {
        return interp(e, t);
    }
    let d = event_span(e, tmap);
    // "一段"（整块不足 0.1 秒 ⇒ 等价 linear 缓动）也走直线
    if grid_params(&e.easing, d).1 <= 1 {
        return interp(e, t);
    }
    let (lo, hi) = knot_bracket(&e.easing, d, t);
    if !(hi > lo) {
        return interp(e, t); // 兜底：节点退化成一个点（不该发生）
    }
    let frac = ((t - lo) / (hi - lo)).clamp(0.0, 1.0);
    let (a, b) = (node_value(e, lo), node_value(e, hi));
    a + (b - a) * frac
}

/// 一条事件块的**折线节点**：`(拍, 值)` —— 网格节点（每 0.1 秒）**加上回弹点**（`back` /
/// `elastic` / `bounce` 的过冲峰与折角）。
///
/// 四个用处，都是"必须与 [`event_value`] 同一条折线"的地方：
/// · 流速积分要在节点处**切开**（段内线性 ⇒ 闭式积分才是精确值；回弹点是折点，不切就错）；
/// · `min |v|` 的解析下界（段内线性的极值只在节点或过零点）；
/// · 时间轴画曲线 —— 画的**就是**求值的那条折线，面板之间不会互相打脸；
/// · "这块被切成了几段"的说明（回弹事件比网格多切几刀）。
pub fn event_knots(e: &Event, tmap: &TimeMap) -> Vec<(f64, f64)> {
    let (a, b) = (e.start.to_f64(), e.end.to_f64());
    let d = event_span(e, tmap);
    knot_us(&e.easing, d).into_iter().map(|u| (a + (b - a) * u, node_value(e, u))).collect()
}

/// 单条轨道的**时间轴折线**点数上限（超过就等距抽稀）。
///
/// 折线节点数随块长线性增长（10 秒一块 = 100 个节点），而"整首歌挤进一条时间轴"时
/// 每像素远大于 0.1 秒 —— 真按节点数画，一张 2000 条长事件的谱面会生成上百万个点。
/// 抽稀只在**超过这个上限**时发生，且会抹平折角（那是看得见的近似）；
/// 落在几秒量级的编辑视野里（时间轴常态）永远到不了上限，画的就是逐节点的真形状。
pub const MAX_CURVE_POINTS: usize = 4096;

/// 一条轨道在拍 `beat` 处的值。
///
/// 五条轨道**同一条路径**（连流速也是）：缓动按折线实现，于是"线怎么动"与"音符怎么走"
/// 来自同一个函数。单独一个入口是为了不漏：树面板/检查器/时间轴/`lines` 各显示一个"此刻的值"，
/// 谁要是绕过它自己算，同一时刻就会显示出两个数来。
///
/// **返回值里的"空位"口径**（用户要求：事件块前后有空位时保持相邻那块的值）：
/// · 事件块**之后**：保持末事件的**终值**（`active_event` 取到末条，取值夹在它自己的终点上）；
/// · 事件块**之前**：取首事件的**起始值**（`unwrap_or(0)` 那一步 —— 早先这里返回 `None`，
///   调用方于是回落到**全局默认值**（流速 10 / 透明度 1 / 移动 0），
///   和这条轨道真正的值不是一个东西）；
/// · **空轨道**才返回 `None` —— 那是"全局默认值"唯一该出现的地方（流速 10、透明度 1、移动 0）。
pub fn track_value(events: &[Event], beat: f64, tmap: &TimeMap) -> Option<f64> {
    if events.is_empty() {
        return None;
    }
    let i = active_event(events, beat).unwrap_or(0);
    Some(event_value(&events[i], beat, tmap))
}

// ---------------------------------------------------------------- 流速（RPE 的 floor position）

/// **1 单位流速 = 120 RPE y 单位 / 秒**（RPE 规范）。
///
/// 出处：RPE 官方手册 —— "实际速度为 10.0 表示音符每秒钟移动 10.0×120 = 1200 像素，
/// 这意味它会 (450+450)/1200 = **0.75 秒**竖直划过整个屏"。
/// RPE 的渲染范围就是 1350×900，与本编辑器演奏区的坐标（±675 × ±450）**是同一套**，
/// 所以 1 单位流速就是 120 单位/秒。
///
/// 独立实现 PhiEdit-2573 用同一个常数（`SPEED_RATIO = 120`）；prpr 用 120.23（差 0.19%，
/// 来源是它把屏幕高比写成 0.83175 而不是 1/1.2）—— 我们按手册取整 120。
pub const SPEED_UNITS_PER_SEC: f64 = 120.0;

/// 一条**没有流速事件**的线按这个速度走：RPE 新建判定线的默认值 = `10`（= 1× = 1200 单位/秒）。
///
/// prpr 对"没有流速事件"的线给 0（音符冻住）—— 那是播放器的选择；编辑器里冻住的预览
/// 等于什么都看不见，所以这里按 RPE 的**默认值**兜底。这也是用户要的"默认速度 10"。
pub const SPEED_DEFAULT: f64 = 10.0;

/// 流速的**位置积分** `H(t) = 120 × ∫ v dτ`（RPE y 单位），τ 在秒域，`from_sec → to_sec`。
///
/// 为什么是积分而不是"两端平均值 × 时长"：音符的纵向位置本来就是 `H(t_音符) − H(t_此刻)`
/// （RPE/prpr 的 floor position），而缓动段里 v 一直在变 —— 用端点近似会在长缓动段上跑偏。
///
/// 这是**直接查询**那一份（从 `from_sec` 起现积）：用途是"任意两时刻之间走了多远"，
/// 以及**测试里的独立基准**（编辑器自己走的是 [`SpeedTable`] 的检查点查表，两者互为对账）。
/// 实现：**闭式** `∫v dτ = (v(a) + v(b))/2 × Δt` —— 精确、无抽样。切点有三类：
/// **事件边界**、**折线节点**（缓动是折线，节点之间才线性，见 [`event_knots`]）与
/// **BPM 段起点**（拍↔秒在那里折了一下，端点平均就不准了）。
pub fn speed_travel(events: &[Event], tmap: &TimeMap, from_sec: f64, to_sec: f64) -> f64 {
    if !(to_sec > from_sec) {
        return 0.0;
    }
    if events.is_empty() {
        // 没有流速事件 ⇒ 按默认 10（= 1×）匀速
        return (to_sec - from_sec) * SPEED_DEFAULT * SPEED_UNITS_PER_SEC;
    }
    let b_from = tmap.beat(from_sec);
    let b_to = tmap.beat(to_sec);
    // 与检查点表**同一份分段**（`speed_segments`），只是这里顺着走一遍现算
    let segs = speed_segments(events, tmap, b_to);
    let mut acc = 0.0;
    for (k, (a, seg)) in segs.iter().enumerate() {
        let Some(next) = segs.get(k + 1).map(|(b, _)| *b) else {
            break;
        };
        let lo = a.max(b_from);
        let hi = next.min(b_to); // 最后一段可能伸出 `b_to` ⇒ 上界要夹住
        if hi > lo {
            acc += integrate_seg(tmap, events, seg, lo, hi);
        }
        if next >= b_to {
            break;
        }
    }
    // 积分出来的 `acc` 是 `∫v dτ`（流速单位 × 秒）；换算成 RPE y 单位只在这里与
    // `SpeedTable::h_at_hinted` 两处乘法里发生
    acc * SPEED_UNITS_PER_SEC
}

/// 流速轨道上 `|v|` 的**下界** —— 用来估算"音符穿过窗口要多久"（只在"还没重算"的兜底里用到）。
///
/// **解析求，不抽样**：值函数是**折线** ⇒ 每一段上 `|v|` 的最小值只可能在**节点**；
/// 相邻节点异号则中间必然穿过 0 ⇒ 下界就是 **0**（"穿过窗口要多久"发散，调用方夹到上限）。
/// 线性事件只有两个节点 ⇒ 与旧的"端点解析式"逐位相同。
///
/// **过零必须算进去**：流速过零时音符会在判定线附近**长时间逗留**（偏移 ≈ 0，一直在窗口里）——
/// 踩过的坑：早先按 `|v| ≥ 0.05` 过滤，于是斜坡过零的那种谱面下界取成 1.25 ⇒ 窗口只有 3.4 秒
/// ⇒ 3.45 秒外那颗**就贴在判定线上**的音符整颗没有实例。
/// 返回 `None` = 没有流速事件（调用方按 `SPEED_DEFAULT` 处理）。
pub fn min_speed_magnitude(events: &[Event], tmap: &TimeMap) -> Option<f64> {
    if events.is_empty() {
        return None;
    }
    let mut best = f64::INFINITY;
    // 节点缓冲循环复用（见 `knot_us_into`）：一条流速轨上可能上千条事件，逐条分配是白花的钱
    let mut knots: Vec<f64> = Vec::new();
    for e in events {
        knot_us_into(&e.easing, event_span(e, tmap), &mut knots);
        let Some(&first) = knots.first() else { continue };
        let mut v0 = node_value(e, first);
        if knots.len() == 1 {
            best = best.min(v0.abs());
            continue;
        }
        for &u in &knots[1..] {
            let v1 = node_value(e, u);
            best = best.min(if v0 * v1 < 0.0 { 0.0 } else { v0.abs().min(v1.abs()) });
            v0 = v1;
        }
    }
    best.is_finite().then_some(best)
}

/// **只有值变了**？—— 检查点表的切点只由"起止拍 ∪ 缓动（决定折线节点）∪ BPM"决定，
/// 与流速的**值**无关 ⇒ 这三样都没动时，表的分段可以原样留着，只从改动点起重算前缀积分
/// （[`SpeedTable::reaccumulate_from`]）。"拖动一条流速事件的值"每帧走的正是这条路。
pub fn speed_shape_unchanged(old: &[Event], new: &[Event]) -> bool {
    old.len() == new.len()
        && old
            .iter()
            .zip(new)
            .all(|(a, b)| a.start == b.start && a.end == b.end && a.easing == b.easing)
}

/// 一段流速的求值方式：走在某条事件的缓动上（**记下标**：检查点表要把"哪一段"存下来，
/// 建表与后缀重算要用），或"保持"某个定值（空隙里 / 首尾之外）。
///
/// 需要比相等：建表时"这一段在下一个切点上的值"能不能复用下一个切点的值，
/// 取决于**是不是同一段**（换段处前后是两个不同的数 —— 见 [`SpeedTable::from_segments`]）。
#[derive(Clone, Copy, Debug, PartialEq)]
enum SpeedSeg {
    Eased(usize),
    Hold(f64),
}

impl SpeedSeg {
    fn at(self, events: &[Event], beat: f64, tmap: &TimeMap) -> f64 {
        match self {
            SpeedSeg::Hold(v) => v,
            SpeedSeg::Eased(i) => {
                events.get(i).map(|e| event_value(e, beat, tmap)).unwrap_or(0.0)
            }
        }
    }
}

/// 拍 `beat` 处**生效的那一段**：起点不晚于 `beat` 的最后一条事件（[`active_event`]）；
/// 一条都没有（`beat` 早于首条）⇒ 保持首条的起始值。
///
/// 与 [`eval_events`] 是同一条规则 —— 流速积分与事件求值**不能各有一套"谁生效"**，
/// 否则预览里"线在哪"与"音符在哪"会来自两个不同的解释。
fn active_speed_seg(events: &[Event], beat: f64) -> SpeedSeg {
    match active_event(events, beat) {
        Some(i) => SpeedSeg::Eased(i),
        None => SpeedSeg::Hold(
            events.first().and_then(|e| as_f64(&e.start_value)).unwrap_or(SPEED_DEFAULT),
        ),
    }
}

/// 流速轨的**分段表示**（唯一表示）：切点 = **事件起点 ∪ 事件终点 ∪ 每块的折线节点 ∪
/// BPM 段起点**，逐段给出那一段的流速来源。返回 `(段起点拍, 该段怎么求值)`，**按拍升序**，
/// 最后一段一直延伸到查询的终点。
///
/// 为什么这么切：段内"哪条事件生效"不变（[`active_event`] 只会在事件起点处换人），
/// 值函数在秒域是线性的（段内 BPM 不变 + **折线的节点之间才是直线**）⇒ [`integrate_seg`]
/// 的闭式是**精确值**。事件终点也是切点：过了终点之后取值夹在终值上（= "前值延拓"，
/// 与 `normalize` 一致）。
///
/// **折线节点必须切开**：缓动被实现成折线（[`event_knots`]）之后，"段内线性"只在节点之间成立；
/// 少切一刀，闭式积分就会拿一条跨折角的斜率去乘时间 —— 那正是本轮要消灭的误差。
///
/// 有了它，[`speed_travel`]（现积）与 [`SpeedTable`]（检查点）走的是**同一份分段**，
/// 原先那套"带着 idx 一段段往前挪"的走法（以及它在空隙/重叠上的三个 bug）整块删掉。
fn speed_segments(events: &[Event], tmap: &TimeMap, b_to: f64) -> Vec<(f64, SpeedSeg)> {
    let b0 = tmap.beat(0.0);
    let mut cuts: Vec<f64> = vec![b0, b_to];
    for e in events {
        let (a, b) = (e.start.to_f64(), e.end.to_f64());
        cuts.push(a);
        cuts.push(b);
        // 折线节点（内部那些；两端已在上面的两行里）——
        // **不建表**：只把位置算出来丢进 cuts，反正下面要排序去重（见 `knot_u_inner`）
        let d = event_span(e, tmap);
        knot_u_inner(&e.easing, d, |u| cuts.push(a + (b - a) * u));
    }
    // BPM 段起点：闭式积分只对"秒域线性"成立，BPM 一变拍↔秒就折了
    let mut bpm = tmap.next_seg_start(b0);
    while let Some(b) = bpm {
        if b >= b_to {
            break;
        }
        cuts.push(b);
        bpm = tmap.next_seg_start(b);
    }
    cuts.retain(|c| c.is_finite());
    cuts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    cuts.dedup_by(|a, b| (*a - *b).abs() < 1e-12);
    cuts.into_iter().map(|c| (c, active_speed_seg(events, c))).collect()
}

/// 一段的积分：**闭式** `∫v dτ = (v(a) + v(b))/2 × Δt`。
///
/// 值函数是折线（缓动的实现，见 [`event_knots`]）⇒ 这个式子是**精确值**，不是近似：
/// 不需要抽点、没有采样误差、更不会沿着一串段累加。代价是调用方必须保证
/// "这一段里流速在**秒域**上线性" —— 折线节点由 [`speed_segments`] 切开，
/// BPM 变化处由 `TimeMap::next_seg_start` 切开。
///
/// **返回 `∫v dτ`（流速单位 × 秒），不乘 120** —— 换算成 RPE y 单位只在
/// [`speed_travel`] 与 [`SpeedTable::build`] 那两处发生，免得两条路径各乘一次或漏乘。
///
/// 热路径（音符位置）**不走它**：那边用段内斜率直接算抛物线（[`SpeedTable::h_at`]），
/// 一次求值都不用做。这份留着是因为它是"从 0 整条走一遍"的**独立基准**（测试拿它对账）。
fn integrate_seg(tmap: &TimeMap, events: &[Event], seg: &SpeedSeg, b_from: f64, b_to: f64) -> f64 {
    if !(b_to > b_from) {
        return 0.0;
    }
    let dt = tmap.sec(b_to) - tmap.sec(b_from);
    match seg {
        SpeedSeg::Hold(v) => v * dt,
        SpeedSeg::Eased(_) => {
            0.5 * (seg.at(events, b_from, tmap) + seg.at(events, b_to, tmap)) * dt
        }
    }
}

/// 检查点表的**一个切点**，把"这一段怎么走"提前折算成了两个数：段起点的值 + 段内的**每秒斜率**。
///
/// 于是 `H(t) = h + (v·Δ + ½·k·Δ²) × 120`（Δ = 该切点到 t 的**秒**数）—— 一条抛物线。
/// 这一步是这张表存在的意义：查询变成**纯算术**，既不碰事件表、也不每颗音符求两次缓动。
/// （段内流速线性 ⇒ 抛物线是**精确**的，不是拟合：切线由 [`speed_segments`] 的切点保证。）
#[derive(Clone, Copy, Debug)]
struct Cut {
    /// 段起点（拍）
    beat: f64,
    /// 该点的 `H`（RPE y 单位，从谱面 0 秒起）
    h: f64,
    /// 该点的流速值
    v: f64,
    /// 段内流速的**每秒**斜率（`Hold` 段 = 0）
    slope: f64,
    /// 这一段归谁 —— 只有**后缀重算**要用（那一步不重建分段，只重算值与前缀积分）
    seg: SpeedSeg,
}

/// 流速积分的**分段检查点**：每个段起点上的 `H`、值与斜率（`H = 120 ∫ v dτ`，从谱面 0 秒起）。
///
/// 存在的理由：`H(t)` 是**前缀**积分 ——
/// ① 每帧从 0 重积是 O(流速事件数)；
/// ② 更要紧的是"**随机取一段音符来算**"（流速事件改了之后要重算它之后的音符、以及还没重算完
///    那几颗的兜底现算）：若从 0 起到每个时刻各积一遍，就是 O(音符 × 流速事件)。
/// 有了检查点，`H(t)` = 查一次表（二分）+ 一次抛物线求值，代价与 t 在哪儿、问的是哪一段无关。
///
/// **切法与 [`speed_segments`] 共用同一份实现**，所以"查表得到的 `H`"与"从 0 整条走一遍
/// （[`speed_travel`]，用 [`integrate_seg`] 的梯形闭式）"是同一个数 —— 于是"预算好的位置"与
/// "现算的位置"可以互为基准对账（`tests/lines.rs` 与 `tests/perf.rs` 里那两条对账就是这么钉的）。
///
/// 被它换掉的那个**单调累加器**只能往前问：查询过去的时刻会静默返回当前累计值（0），
/// hold 尾巴"被钉死在头 + 全长"那个 bug 就是从这儿来的。检查点表没有这个毛病 ——
/// 过去、现在、将来都能问。
#[derive(Clone, Debug, Default)]
pub struct SpeedTable {
    cuts: Vec<Cut>,
}

impl SpeedTable {
    /// 建表：切到 `b_end`（拍）为止。**O(切点数)**，每个切点的值只求一次。
    pub fn build(events: &[Event], tmap: &TimeMap, b_end: f64) -> Self {
        let segs = speed_segments(events, tmap, b_end);
        Self::from_segments(&segs, events, tmap)
    }

    /// 由分段建表。**顺着走一遍**：每个切点的值只求一次。
    ///
    /// 唯一要多求一次的地方是**换段**的那个切点（事件起点）：那一点上"前一段的值"与
    /// "后一段的值"**本来就是两个数** —— 例如空隙里保持前一块的终值 4，而下一块从 0 起
    /// （谱面在那一刻本来就是跳变的）。段内斜率必须问**本段**在那一点的值；
    /// 拿下一段的值去算斜率，会把这条跳变抹成一条斜线（实测差 0.6 个单位）。
    fn from_segments(segs: &[(f64, SpeedSeg)], events: &[Event], tmap: &TimeMap) -> Self {
        let mut cuts: Vec<Cut> = Vec::with_capacity(segs.len());
        let mut acc = 0.0; // H（RPE y 单位）
        let mut v = segs.first().map(|(b, s)| s.at(events, *b, tmap)).unwrap_or(0.0);
        for (k, (beat, seg)) in segs.iter().enumerate() {
            // 下一段在它自己起点上的值（也正是下一个切点的 `v`）
            let next = segs.get(k + 1).map(|(nb, nseg)| (*nb, *nseg, nseg.at(events, *nb, tmap)));
            let (v_next, dt) = match next {
                Some((nb, nseg, nv)) => {
                    let dt = tmap.sec(nb) - tmap.sec(*beat);
                    // 同一段 ⇒ 两个值恒等，直接复用；换段才多求一次
                    (if nseg == *seg { nv } else { seg.at(events, nb, tmap) }, dt)
                }
                None => (v, 0.0),
            };
            let slope = if dt > 0.0 { (v_next - v) / dt } else { 0.0 };
            cuts.push(Cut { beat: *beat, h: acc, v, slope, seg: *seg });
            acc += (v * dt + 0.5 * slope * dt * dt) * SPEED_UNITS_PER_SEC;
            // 下个切点的值是**下一段**在它起点上的值（换段处它与 `v_next` 不是同一个数）
            v = next.map_or(v, |(_, _, nv)| nv);
        }
        Self { cuts }
    }

    /// **只有值变了**（起止拍与缓动没动）时的重算：切点与分段一模一样，
    /// 而 `H` 是**前缀**积分 ⇒ 从改动点那一段起重新累加，前半张表原样保留。
    ///
    /// 这正是"拖动一条流速事件的值"每帧要走的路：拖第 k 条时，它**之前**的切点、
    /// 值、斜率、以及它们的 `H` 全都不受影响 —— 白重算那一半纯属浪费。
    ///
    /// `from_beat` = 最早可能受影响的拍（`state::first_speed_change` 给的那个）。
    /// 起算点是"起点不早于它的第一个切点"：那一点上的 `H` 也**不用动**
    /// （它左边的段一个都没改），要重算的是它的值与斜率，以及它**右边**的 `H`。
    pub fn reaccumulate_from(&mut self, events: &[Event], tmap: &TimeMap, from_beat: f64) {
        let s = self.cuts.partition_point(|c| c.beat < from_beat - 1e-12);
        if s >= self.cuts.len() {
            return; // 改动落在最后一个切点之后：没有哪一段的积分会变
        }
        let mut acc = self.cuts[s].h; // `H(b_s)` 不变：它左边的段一个都没动
        let mut v = self.cuts[s].seg.at(events, self.cuts[s].beat, tmap);
        for k in s..self.cuts.len() {
            let (beat, seg) = (self.cuts[k].beat, self.cuts[k].seg);
            let next = self.cuts.get(k + 1).map(|n| (n.beat, n.seg, n.seg.at(events, n.beat, tmap)));
            let (v_next, dt) = match next {
                Some((nb, nseg, nv)) => {
                    let dt = tmap.sec(nb) - tmap.sec(beat);
                    (if nseg == seg { nv } else { seg.at(events, nb, tmap) }, dt)
                }
                None => (v, 0.0),
            };
            let slope = if dt > 0.0 { (v_next - v) / dt } else { 0.0 };
            self.cuts[k] = Cut { beat, h: acc, v, slope, seg };
            acc += (v * dt + 0.5 * slope * dt * dt) * SPEED_UNITS_PER_SEC;
            v = next.map_or(v, |(_, _, nv)| nv);
        }
    }

    /// 表还没建过？（`line_shell` 造出来的线就是这种状态）
    pub fn is_empty(&self) -> bool {
        self.cuts.is_empty()
    }

    /// `H(sec)`（RPE y 单位）。**任何时刻都能问**（过去 / 现在 / 将来一视同仁）。
    pub fn h_at(&self, tmap: &TimeMap, sec: f64) -> f64 {
        self.h_at_hinted(tmap, sec, 0).0
    }

    /// 同上，但允许带一个"上次落在哪一段"的提示（升序查询时摊还 O(1)）。
    /// 返回 `(H, 新的提示)`；提示只当**起点**用：不命中就二分，所以倒退查询不会算错。
    ///
    /// **纯算术**：不碰事件表、不套缓动 —— 段内的值与斜率在建表时就折算好了。
    pub fn h_at_hinted(&self, tmap: &TimeMap, sec: f64, hint: usize) -> (f64, usize) {
        let b = tmap.beat(sec);
        let i = self.seg_index(b, hint);
        let Some(c) = self.cuts.get(i) else {
            // 表还没建（这条线刚从 `line_shell` 造出来、事件轨道还是空的）：
            // 按 RPE 的默认流速走直线。建好表之后这段路不会再走。
            return (sec.max(0.0) * SPEED_DEFAULT * SPEED_UNITS_PER_SEC, 0);
        };
        // `[cut.beat, b]` 落在**一段之内**（BPM 段起点也是切点）⇒ 一次抛物线求值就够
        let dt = tmap.sec(b) - tmap.sec(c.beat);
        (c.h + (c.v * dt + 0.5 * c.slope * dt * dt) * SPEED_UNITS_PER_SEC, i)
    }

    /// `beat` 落在第几段：`hint` 命中就 O(1)（升序查询的顺序），否则二分
    fn seg_index(&self, beat: f64, hint: usize) -> usize {
        let hit = self
            .cuts
            .get(hint)
            .is_some_and(|c| c.beat <= beat && self.cuts.get(hint + 1).is_none_or(|n| n.beat > beat));
        if hit {
            return hint;
        }
        self.cuts.partition_point(|c| c.beat <= beat).saturating_sub(1)
    }
}

/// 判定线的表演状态（由五条轨道在某一时刻求值得到）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinePerf {
    pub x: f32,
    pub y: f32,
    pub rotate_deg: f32,
    pub alpha: f32,
    pub speed: f32,
}

impl Default for LinePerf {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            rotate_deg: 0.0,
            alpha: 1.0,
            speed: 10.0,
        }
    }
}

impl LinePerf {
    /// 把线本地坐标变换到 RPE 屏幕坐标：先旋转，再平移。
    /// 判定线本体与它下面的音符走的是**同一个**变换 —— 这就是"音符依赖于判定线"的几何含义。
    pub fn apply(&self, local: [f32; 2]) -> [f32; 2] {
        let (s, c) = self.rotate_deg.to_radians().sin_cos();
        [
            local[0] * c - local[1] * s + self.x,
            local[0] * s + local[1] * c + self.y,
        ]
    }
    /// 逆变换：RPE 屏幕坐标 → **线本地**坐标（旋转 + 平移都可逆）。
    ///
    /// 两个用处：点选判定线（`Line::hit`），以及渲染侧把**窗口矩形**搬回本地空间当候选筛
    /// （见 `render::build_instances`：候选集按位置选，不按时间窗口选）。
    pub fn apply_inv(&self, screen: [f32; 2]) -> [f32; 2] {
        let (s, c) = (-self.rotate_deg).to_radians().sin_cos();
        let dx = screen[0] - self.x;
        let dy = screen[1] - self.y;
        [dx * c - dy * s, dx * s + dy * c]
    }

    /// 线的旋转角（弧度），给实例用
    pub fn rotate_rad(&self) -> f32 {
        self.rotate_deg.to_radians()
    }
}

/// 从判定线取出某条轨道的全部事件（图层 0 优先；多图层时合并为一条时间线）
pub fn track_events(line: &JudgeLine, track: &str) -> Vec<Event> {
    track_events_indexed(line, track).into_iter().map(|(_, e)| e).collect()
}

/// 同上，但**每个事件都带上它在文档里的出处**（第几层、该层里的下标）。
///
/// 合并顺序就是求值/绘制的顺序；**编辑必须用回来处** —— 合并序号与图层内序号不是一回事，
/// 详见 [`crate::doc::EventRef`]。合并逻辑只有这一份，`track_events` 是它的投影。
pub fn track_events_indexed(line: &JudgeLine, track: &str) -> Vec<(crate::doc::EventRef, Event)> {
    let mut out: Vec<(crate::doc::EventRef, Event)> = Vec::new();
    for (layer, l) in line.layers.iter().enumerate() {
        if let Some(list) = l.track(track) {
            for (i, e) in list.iter().enumerate() {
                out.push((crate::doc::EventRef::new(layer, i), e.clone()));
            }
        }
    }
    out.sort_by(|a, b| a.1.start.cmp(&b.1.start));
    out
}

/// 没有 `layers` 时（老文档/半成品）也给一份默认层
pub fn first_layer<'a>(line: &'a JudgeLine) -> Option<&'a Layer> {
    line.layers.first()
}

/// **五条轨道的求值：只有这一份实现**（顺序与 `TrackId` / `doc::TRACKS` 一致：
/// `moveX, moveY, rotate, alpha, speed`）。
///
/// 为什么必须只有一份：这里曾经有两份手抄的循环 —— 一份借用 `&[Vec<Event>;5]`（`perf_at`）、
/// 一份借用视图（`state::Line::perf`），结果在**流速那条轨道上分家了**（一边按缓动、一边按线性
/// ——而"流速只按 linear 求值"才是当时定的规矩）。两份实现里"看起来一样"的那四条轨道，
/// 只是还没轮到它们分家而已。**现在五条轨道连口径都一样**（缓动按折线实现，流速不再特殊），
/// 更没理由各写一份：每一条都经 [`track_value`]。
pub fn perf_of(tracks: &[&[Event]; 5], beat: f64, tmap: &TimeMap) -> LinePerf {
    let mut p = LinePerf::default();
    let mut v = [0.0f64; 5];
    let mut has = [false; 5];
    for (i, ev) in tracks.iter().enumerate() {
        if let Some(x) = track_value(ev, beat, tmap) {
            v[i] = x;
            has[i] = true;
        }
    }
    if has[0] {
        p.x = v[0] as f32;
    }
    if has[1] {
        p.y = v[1] as f32;
    }
    if has[2] {
        p.rotate_deg = v[2] as f32;
    }
    if has[3] {
        p.alpha = (v[3] as f32).clamp(0.0, 1.0);
    }
    if has[4] {
        p.speed = v[4] as f32;
    }
    p
}

/// 求某条判定线在**秒** t 处的表演状态。
///
/// `tracks` 是预先按 `[moveX, moveY, rotate, alpha, speed]` 取好的事件表 ——
/// 让调用方可以缓存（每帧对每条线求值 5 次二分，代价可忽略）。
pub fn perf_at(tracks: &[Vec<Event>; 5], tmap: &TimeMap, sec: f64) -> LinePerf {
    let ev: [&[Event]; 5] = [
        &tracks[0], &tracks[1], &tracks[2], &tracks[3], &tracks[4],
    ];
    perf_of(&ev, tmap.beat(sec), tmap)
}

// ---------------------------------------------------------------- 遮蔽区（躁域）

/// 一块**遮蔽区**在某一拍的状态（求值结果，不是文档）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaskState {
    /// 三角形此刻**是否存在于画面上**。
    ///
    /// 判据（用户口径）：三条顶点轨道里**至少有一条已经有"已开始"的事件**（`start ≤ beat`）。
    /// 与判定线不同 —— 判定线永远有本体，没有任何事件也画得出来；遮蔽区没有事件就没有形状，
    /// 所以在第一条坐标事件之前它**不存在**（"如果当前时间没有任何对应的坐标事件块，则不显示"）。
    ///
    /// 单调性：事件一旦开始就不会"没开始" ⇒ 这个布尔随时间只会从 false 变 true 一次。
    pub visible: bool,
    /// 三个顶点的屏幕坐标（RPE 单位，Y 向上；与判定线同一个坐标系）。
    /// 某一维没有事件 ⇒ **0.0**（用户口径："总保底都是 (0,0)"）。
    pub v: [[f64; 2]; 3],
    /// `active` 通道按 `≥ 0.5` 二值化的结果；**没有 active 事件时是 `false`**
    /// （= 纯色、不透明度更高那一档）。
    pub active: bool,
}

/// 遮蔽区在拍 `beat` 处的状态。`tracks` 的顺序 = [`crate::doc::MASK_TRACKS`]
/// （`x1,y1,x2,y2,x3,y3,active`）。
///
/// **求值只有这一份**：预览渲染、编辑区读数、检查器、命令层全部经它 ——
/// 谁要自己算一遍，"同一时刻两个数"就会在某个缓动上冒出来（判定线那五条轨道踩过同一个坑）。
///
/// 三处口径写在这里，别处不许再解释一遍：
/// · **空轨道 ⇒ `(0,0)`**（不是"整块平移"，也不是"用上一个顶点的值"）；
/// · **块之前 ⇒ 首事件的起始值、块之后 ⇒ 末事件的终值**（都由 [`track_value`] 给，
///   与判定线轨道同一条规则 —— "如果任意一个坐标事件块还在则延续最后值"）；
/// · **`active` 与其它轨道同一套插值**，只在最后一步按 `≥ 0.5` 二值化（用户口径）。
pub fn mask_state_at(tracks: &[&[Event]; 7], beat: f64, tmap: &TimeMap) -> MaskState {
    let mut v = [[0.0f64; 2]; 3];
    for i in 0..3 {
        for (k, axis) in [0usize, 1].into_iter().enumerate() {
            let list = tracks[i * 2 + axis];
            if let Some(x) = track_value(list, beat, tmap) {
                v[i][k] = x;
            }
        }
    }
    // "任意一个坐标事件块还在"：**任一维**已经有已开始的事件 ⇒ 这个顶点就位；
    // 三个顶点一个都没就位 ⇒ 整块不显示。
    let visible = (0..3).any(|i| {
        [0usize, 1]
            .into_iter()
            .any(|axis| active_event(tracks[i * 2 + axis], beat).is_some())
    });
    let active = track_value(tracks[6], beat, tmap).map(|x| x >= 0.5).unwrap_or(false);
    MaskState { visible, v, active }
}

/// 把一条轨道采样成折线（供时间轴画曲线）：**画的与求值的是同一条折线**。
///
/// 每个事件交给 [`event_knots`] —— 于是时间轴上看到的形状**就是**演奏区会发生的形状
/// （一段一段的直线，包括缓动的过冲与"短块退化成线性"）。不再是"另取 N 个点近似一下"：
/// 那种画法在线性事件上多画点、在缓动事件上少画点，面板之间迟早对不上。
pub fn sample_track(events: &[Event], tmap: &TimeMap) -> Vec<[f32; 2]> {
    let mut out: Vec<[f32; 2]> = Vec::new();
    if events.is_empty() {
        return out;
    }
    let mut push = |beat: f64, v: f64| out.push([tmap.sec(beat) as f32, v as f32]);
    // 节点缓冲**循环复用**（一条轨道上可能有上千条事件，逐条分配是白花的钱）
    let mut knots: Vec<f64> = Vec::new();

    // ① 首条事件**之前**的空位：保持首条的起始值（与 `track_value`/`active_event` 同一条规则）
    let b0 = tmap.beat(0.0);
    let first = &events[0];
    if first.start.to_f64() > b0 + 1e-9 {
        let v = event_value(first, first.start.to_f64(), tmap);
        push(b0, v);
        push(first.start.to_f64(), v);
    }
    for (i, e) in events.iter().enumerate() {
        let (a, b) = (e.start.to_f64(), e.end.to_f64());
        knot_us_into(&e.easing, event_span(e, tmap), &mut knots);
        for &u in &knots {
            push(a + (b - a) * u, node_value(e, u));
        }
        // ② 两条事件之间的空位：**保持这一条的终值**（不是插值过去 —— 那与求值器不一致）
        if let Some(next) = events.get(i + 1) {
            let ns = next.start.to_f64();
            if ns > b + 1e-9 {
                let v = event_value(e, b, tmap);
                push(b, v);
                push(ns, v);
            }
        }
    }
    // ③ 末条事件**之后**的空位：保持末条的终值，一直画到谱面末尾
    //（时间轴比谱面长时由画图那一侧补到画面边缘）
    let last = events.last().expect("上面判过非空");
    let le = last.end.to_f64();
    if tmap.end_beat > le + 1e-9 {
        let v = event_value(last, le, tmap);
        push(le, v);
        push(tmap.end_beat, v);
    }
    decimate(out)
}

/// 点数超过 [`MAX_CURVE_POINTS`] 就等距抽稀（**保留末点** —— 曲线要画到谱面末尾）。
fn decimate(points: Vec<[f32; 2]>) -> Vec<[f32; 2]> {
    if points.len() <= MAX_CURVE_POINTS {
        return points;
    }
    let stride = points.len().div_ceil(MAX_CURVE_POINTS);
    let mut out: Vec<[f32; 2]> = points.iter().step_by(stride).copied().collect();
    if let Some(last) = points.last() {
        if out.last() != Some(last) {
            out.push(*last);
        }
    }
    out
}

#[cfg(test)]
mod speed_tests {
    use super::*;
    use crate::doc::{Beat, BpmEntry};
    use serde_json::json;

    /// 一份 BPM 120 的时间映射（一拍 0.5 秒，手算方便）
    fn tmap120() -> TimeMap {
        let mut doc = Document::default();
        doc.bpm_list = vec![BpmEntry {
            start: Beat::zero(),
            bpm: 120.0,
            foreign: Default::default(),
        }];
        TimeMap::from_doc(&doc)
    }

    fn ev(from_beat: f64, to_beat: f64, from: f64, to: f64) -> Event {
        ev_ease(from_beat, to_beat, from, to, "linear")
    }

    /// 同上，但指定缓动名（**流速轨不认它** —— 见 `speed_events_ignore_their_easing`）
    fn ev_ease(from_beat: f64, to_beat: f64, from: f64, to: f64, easing: &str) -> Event {
        Event::new(
            Beat::new((from_beat * 4.0) as i64, 4),
            Beat::new((to_beat * 4.0) as i64, 4),
            json!(from),
            json!(to),
            easing,
        )
    }

    /// **RPE 手册那条算例**：流速 10 的音符每秒走 1200 单位，0.75 秒划过整个 900 高的窗口。
    #[test]
    fn speed_ten_crosses_the_window_in_three_quarters_of_a_second() {
        let tmap = tmap120();
        let events = vec![ev(0.0, 16.0, 10.0, 10.0)];
        let travel = speed_travel(&events, &tmap, 0.0, 0.75);
        assert!(
            (travel - 900.0).abs() < 1e-6,
            "流速 10 走 0.75 秒应正好是 900 单位（= 窗口高度），实际 {travel}"
        );
        // 一单位流速 = 120 单位/秒；流速 1 走 0.75 秒只有 90 单位
        let slow = vec![ev(0.0, 16.0, 1.0, 1.0)];
        assert!((speed_travel(&slow, &tmap, 0.0, 0.75) - 90.0).abs() < 1e-6);
        // 与 BPM 无关（流速是"每秒"，不是"每拍"）
        let mut doc = Document::default();
        doc.bpm_list = vec![BpmEntry {
            start: Beat::zero(),
            bpm: 240.0,
            foreign: Default::default(),
        }];
        let tmap240 = TimeMap::from_doc(&doc);
        assert!((speed_travel(&events, &tmap240, 0.0, 0.75) - 900.0).abs() < 1e-6);
    }

    /// 没有流速事件 ⇒ 按 RPE 的**默认值 10**（= 1×）匀速，而不是冻住（那是 prpr 对播放器的选择）
    #[test]
    fn an_empty_speed_track_falls_back_to_rpe_default_ten() {
        let tmap = tmap120();
        let t = speed_travel(&[], &tmap, 1.0, 2.0);
        assert!((t - 1200.0).abs() < 1e-6, "1 秒应走 10×120，实际 {t}");
        assert_eq!(SPEED_DEFAULT, 10.0);
        assert_eq!(SPEED_UNITS_PER_SEC, 120.0);
    }

    /// 缓动段（= 线性）走**积分**而不是"取此刻的瞬时值"：
    /// 于是 0→10 的斜坡在 1 秒里积出 5（平均值）—— 若用"取此刻的瞬时值"就会是 0 或 10。
    #[test]
    fn a_ramp_integrates_instead_of_sampling_one_instant() {
        let tmap = tmap120();
        // 0 拍到 2 拍（0~1 秒）从 0 线性升到 10
        let events = vec![ev(0.0, 2.0, 0.0, 10.0)];
        let t = speed_travel(&events, &tmap, 0.0, 1.0);
        assert!((t - 5.0 * 120.0).abs() < 1.0, "线性斜坡 1 秒应积出 5（平均），实际 {}", t / 120.0);
        // 分段：前 1 秒（平均 5）之后的 1 秒是恒定 10
        let events = vec![ev(0.0, 2.0, 0.0, 10.0), ev(2.0, 4.0, 10.0, 10.0)];
        let t = speed_travel(&events, &tmap, 0.0, 2.0);
        assert!((t - (5.0 + 10.0) * 120.0).abs() < 1.0, "实际 {}", t / 120.0);
    }

    /// 空隙里**保持**前一条的终值（与 `eval_events` 同一条语义），不是跳到最后一条
    #[test]
    fn a_gap_holds_the_previous_speed() {
        let tmap = tmap120();
        // [0,1) 秒 流速 4；[2,3) 秒 流速 0 ⇒ 1~2 秒的空隙里保持 4
        let events = vec![ev(0.0, 2.0, 4.0, 4.0), ev(4.0, 6.0, 0.0, 0.0)];
        let t = speed_travel(&events, &tmap, 1.0, 2.0);
        assert!((t - 4.0 * 120.0).abs() < 1e-6, "空隙里应保持 4，实际 {}", t / 120.0);
    }

    /// **空隙之后的那条事件必须被算进去** —— 这是从真 bug 里钉下来的一条。
    ///
    /// 走法曾经在"空隙段走完"时也把 `idx` 前进一格，而空隙段是**走到下一条事件的起点**为止的：
    /// 于是那条正好从空隙终点开始的事件被整条跳过，积分在空隙之后永远保持空隙前的值。
    /// 表现：谱面里只要有一个空隙，**它后面所有音符的位置都是错的**（而且错得"自洽"，
    /// 只有拿直接积分当基准对账才会露出来 —— 见 `tests/lines.rs` 里那条）。
    #[test]
    fn the_walk_does_not_skip_the_event_after_a_gap() {
        let tmap = tmap120();
        // 拍域：0~1 拍流速 4、2~3 拍流速 0（BPM 120 ⇒ 一拍 0.5 秒）
        let events = vec![ev(0.0, 1.0, 4.0, 4.0), ev(2.0, 3.0, 0.0, 0.0)];
        // 0.5~1.0 秒是空隙（保持 4），1.0~1.5 秒必须走第二条事件（流速 0）
        // ⇒ ∫ = 4×0.5 + 4×0.5 + 0×0.5 = 4（流速单位 × 秒）
        let t = speed_travel(&events, &tmap, 0.0, 1.5);
        assert!((t - 4.0 * 120.0).abs() < 1e-6, "实际 {}", t / 120.0);
        // 前缀与"两点之间"两种问法必须一致
        let prefix = speed_travel(&events, &tmap, 0.0, 1.4) + speed_travel(&events, &tmap, 1.4, 1.5);
        assert!((prefix - t).abs() < 1e-6, "前缀拼起来应等于整段：{prefix} ≠ {t}");
        // 后半段（1.0~1.5 秒）流速是 0 ⇒ 一点都不动
        assert!(speed_travel(&events, &tmap, 1.0, 1.5).abs() < 1e-6);
    }

    /// **重叠的事件：起点最晚的那条生效**（与导入侧 `normalize_track` 的"裁重叠"同一条规则）。
    ///
    /// 用户报的 bug：运行中往一条长事件里再放一条速度事件，"和没放一样，得重新加载才有变化"。
    /// 原因是求值取的是**第一条覆盖它的事件**（长条），而重新加载时 `normalize` 会把长条裁到
    /// 后一条的起点 ⇒ 两条路给出两个结果。现在两处都是"起点最晚者赢"。
    #[test]
    fn overlapping_events_keep_the_later_start() {
        let tmap = tmap120();
        let events = vec![ev(0.0, 64.0, 10.0, 10.0), ev(4.0, 8.0, 30.0, 30.0)];
        // 拍 2 在长条里 ⇒ 10；拍 6 已被后一条接管 ⇒ 30；**过了后一条的终点也还是 30**
        // （`normalize` 会把末事件延拓到谱尾，而不是让长条"复活"）
        assert_eq!(eval_events(&events, 2.0, &tmap), Some(10.0));
        assert_eq!(eval_events(&events, 6.0, &tmap), Some(30.0));
        assert_eq!(eval_events(&events, 20.0, &tmap), Some(30.0));
        assert_eq!(active_event(&events, 6.0), Some(1));
        // 积分同样：H 与"规范化之后的轨道"必须逐点相同
        let normalized = crate::codec::normalize_track(
            events.clone(),
            &tmap,
            Beat::new(64, 1),
            "/x",
            &mut crate::codec::Fidelity::new("test", "v1".into()),
        )
        .0;
        for k in 0..=200 {
            let sec = k as f64 * 0.1;
            let (a, b) = (
                speed_travel(&events, &tmap, 0.0, sec),
                speed_travel(&normalized, &tmap, 0.0, sec),
            );
            // 两条路的分段不同 ⇒ 只差浮点累加顺序（实测 ~4e-12）
            assert!((a - b).abs() < 1e-6, "t={sec}：原始 {a} ≠ 规范化 {b}");
        }
    }

    /// **空隙 / 重叠 / 末事件之后的取值，运行时求值 = 规范化之后的求值**（"不用重新加载"的总断言）
    #[test]
    fn raw_and_normalized_tracks_evaluate_the_same() {
        let tmap = tmap120();
        let shapes: Vec<Vec<Event>> = vec![
            // 重叠：后一条插在长条中间
            vec![ev(0.0, 16.0, 10.0, 10.0), ev(4.0, 8.0, 30.0, 30.0)],
            // 空隙：0~2 与 4~6 之间空一段
            vec![ev(0.0, 2.0, 4.0, 4.0), ev(4.0, 6.0, 0.0, 0.0)],
            // 首条晚于拍 0
            vec![ev(2.0, 6.0, 3.0, 9.0)],
            // 末条早于谱尾 + 斜坡
            vec![ev(0.0, 4.0, 0.0, 10.0), ev(4.0, 6.0, 10.0, 0.0)],
        ];
        for events in shapes {
            let normalized = crate::codec::normalize_track(
                events.clone(),
                &tmap,
                Beat::new(32, 1),
                "/x",
                &mut crate::codec::Fidelity::new("test", "v1".into()),
            )
            .0;
            for k in 0..=400 {
                let beat = k as f64 * 0.1;
                let (a, b) = (eval_events(&events, beat, &tmap), eval_events(&normalized, beat, &tmap));
                match (a, b) {
                    (Some(x), Some(y)) => assert!(
                        (x - y).abs() < 1e-9,
                        "拍 {beat}：原始 {x} ≠ 规范化 {y}（事件 {events:?}）"
                    ),
                    (None, None) => {}
                    _ => panic!("拍 {beat}：一条给 {a:?}、另一条给 {b:?}"),
                }
            }
        }
    }

    /// **事件块前后的空位：保持相邻那块的值**（用户口径），而不是回落到全局默认值。
    ///
    /// 早先 `track_value("speed", …)` 在"首条事件之前"这一步返回 `None`，调用方于是回落到
    /// **全局默认值**（流速 10 / 透明度 1 / 移动 0）—— 与这条轨道真正的值不是一个东西。
    /// 空轨道才该返回 `None`（那是全局默认值唯一该出现的地方）。
    #[test]
    fn gaps_hold_the_neighbouring_block_value() {
        let tmap = tmap120();
        // 块在 [8,16] 拍（4~8 秒），值 7
        let events = vec![ev(8.0, 16.0, 7.0, 7.0)];
        assert_eq!(track_value(&events, 2.0, &tmap), Some(7.0), "块**之前**取块的起始值");
        assert_eq!(track_value(&events, 2.0, &tmap), Some(7.0), "五条轨道同一口径");
        assert_eq!(track_value(&events, 12.0, &tmap), Some(7.0), "块里");
        assert_eq!(track_value(&events, 100.0, &tmap), Some(7.0), "块**之后**保持终值");
        // 斜坡：块前取起始值、块后取终值（不是 0、也不是别的默认值）
        let ramp = vec![ev(8.0, 16.0, 2.0, 9.0)];
        assert_eq!(track_value(&ramp, 2.0, &tmap), Some(2.0));
        assert_eq!(track_value(&ramp, 100.0, &tmap), Some(9.0));
        // **空轨道** ⇒ None（调用方用自己的默认值：流速 10 / 透明度 1 / 移动 0）
        assert_eq!(track_value(&[], 2.0, &tmap), None);
        assert_eq!(eval_events(&[], 2.0, &tmap), None);
        let _ = tmap;
    }

    /// **时间轴曲线在空位里保持相邻那块的值**（`sample_track`），而且画满整条谱面。
    ///
    /// 早先它只在事件**内部**采样 ⇒ 事件块前后的空位"断线"，看上去像回到了默认值。
    #[test]
    fn the_sampled_curve_holds_values_across_gaps() {
        // 谱面末尾要有内容（拍 32），否则 `end_beat` = 0，"画到末尾"这条断言就没意义
        let mut doc = Document::default();
        doc.bpm_list = vec![BpmEntry {
            start: Beat::zero(),
            bpm: 120.0,
            foreign: Default::default(),
        }];
        doc.judge_lines = vec![crate::doc::JudgeLine::default()];
        doc.judge_lines[0].notes.push(crate::doc::Note::new(
            crate::doc::NoteKind::Tap,
            Beat::new(32, 1),
            0.0,
        ));
        let tmap = TimeMap::from_doc(&doc);
        assert_eq!(tmap.end_beat, 32.0, "用例前提：谱面末尾在拍 32");
        // 块 [8,12] 值 5 ⇒ 曲线应从拍 0 起就保持 5，一直画到谱面末尾
        let one = vec![ev(8.0, 12.0, 5.0, 5.0)];
        let pts = sample_track(&one, &tmap);
        let first = pts.first().expect("非空");
        assert!(first[0].abs() < 1e-6, "曲线要从谱面开头（拍 0 = 0 秒）起");
        assert!((first[1] - 5.0).abs() < 1e-6, "开头是首事件的起始值，实际 {}", first[1]);
        let last = pts.last().expect("非空");
        assert!((last[0] as f64 - tmap.sec(tmap.end_beat)).abs() < 1e-3, "画到谱面末尾");
        assert!((last[1] - 5.0).abs() < 1e-6, "末尾保持末事件的终值，实际 {}", last[1]);
        // 事件之间的空位：保持前一条的终值（两点同值 ⇒ 水平段，不是插值）
        let gap = vec![ev(0.0, 4.0, 5.0, 5.0), ev(8.0, 12.0, 9.0, 9.0)];
        let pts = sample_track(&gap, &tmap);
        let at = |sec: f64| -> Option<f32> {
            pts.iter().find(|q| (q[0] as f64 - sec).abs() < 1e-4).map(|q| q[1])
        };
        assert_eq!(at(tmap.sec(4.0)).map(|v| v.round()), Some(5.0), "空位起点 = 前一条的终值");
        assert_eq!(at(tmap.sec(8.0)).map(|v| v.round()), Some(5.0), "空位终点仍是前一条的终值");
        // 空位里没有任何"回到默认值"的采样点
        assert!(
            !pts.iter().any(|q| {
                let sec = q[0] as f64;
                sec > tmap.sec(4.0) + 1e-6 && sec < tmap.sec(8.0) - 1e-6 && q[1].abs() < 1e-6
            }),
            "空位里不该出现 0（那就是「回到默认值」）"
        );
    }

    /// **流速事件现在认缓动**（本轮改动的核心）：同一条事件记成 `outElastic` 与记成 `linear`，
    /// 积分结果**必须不同** —— 不同才说明曲线真的进了音符位置。
    ///
    /// 而且缓动版的 `H` 必须与"把折线嚼碎成极细的线性段再积分"一致到浮点噪声：
    /// 折线节点处切开 + 段内闭式 ⇒ 没有抽样误差、也不沿段累加。
    #[test]
    fn speed_events_now_honour_their_easing() {
        let tmap = tmap120();
        // 4 秒（8 拍）的块：每 0.1 秒一段 ⇒ 40 段（无尾巴）
        let eased = vec![ev_ease(0.0, 8.0, 0.0, 20.0, "outElastic")];
        let linear = vec![ev(0.0, 8.0, 0.0, 20.0)];
        assert_eq!(event_segments(&eased[0], &tmap), 40);
        let t_eased = SpeedTable::build(&eased, &tmap, 20.0);
        let t_linear = SpeedTable::build(&linear, &tmap, 20.0);
        let mut differs = false;
        for k in 0..=100 {
            let sec = k as f64 * 0.1;
            let (a, b) = (
                t_eased.h_at(&tmap, sec),
                t_linear.h_at(&tmap, sec),
            );
            if (a - b).abs() > 1e-6 {
                differs = true;
            }
        }
        assert!(differs, "缓动过的流速事件必须和线性给出不同的 H（否则缓动没生效）");
        // 端点仍按定义：块末的值 = endValue，于是闭式积分在块末 = 平均 × 时长（这里 20 与 0…）
        // —— 用一条不越界的缓动验证闭式（`outElastic` 会过冲，端点值仍是 20）
        let spike = vec![ev_ease(0.0, 8.0, 0.0, 20.0, "inOutCubic")];
        let table = SpeedTable::build(&spike, &tmap, 20.0);
        let want = independent_integral(&spike, &tmap, 0.0, 4.0);
        let got = table.h_at(&tmap, 4.0) / SPEED_UNITS_PER_SEC;
        assert!(
            (got - want).abs() < 1e-9,
            "折线积分应与极细数值积分一致：{got} vs {want}"
        );
    }

    /// **独立基准**：把折线嚼成 1e-5 秒的小段、每段用梯形求积 —— 只在测试里用。
    ///
    /// 它与被测实现**不共用任何东西**（除了 `event_value` 这个被求的对象本身）：
    /// 用来回答"折线积分到底精不精确"，而不是"两条路是不是抄的同一份代码"。
    fn independent_integral(events: &[Event], tmap: &TimeMap, from_sec: f64, to_sec: f64) -> f64 {
        let steps = ((to_sec - from_sec) / 1e-5).ceil() as usize;
        let dt = (to_sec - from_sec) / steps as f64;
        let mut acc = 0.0;
        for k in 0..steps {
            let a = from_sec + k as f64 * dt;
            let b = a + dt;
            let va = event_value(&events[0], tmap.beat(a), tmap);
            let vb = event_value(&events[0], tmap.beat(b), tmap);
            acc += 0.5 * (va + vb) * dt;
        }
        acc
    }

    /// **BPM 变化点上的闭式积分**：流速在拍域线性，BPM 一变拍↔秒就折了 ——
    /// 积分必须在那个点上切开，否则端点平均是错的。
    ///
    /// 手算：BPM 120（0~4 拍 = 0~2 秒）→ BPM 240（4~8 拍 = 2~3 秒）；
    /// 流速事件 0~8 拍从 0 线性升到 20（拍域）。两段的平均值不同：
    /// 第一段 v(0)=0、v(4拍)=10 ⇒ 10×... 正确算法：段内线性 ⇒ 各段用各自的端点平均 × 该段秒长。
    /// 0~2 秒：平均 5 ⇒ 10；2~3 秒：起点 v=10、终点 v=20 ⇒ 平均 15 ⇒ 15 ⇒ 合计 25（流速单位 × 秒）。
    #[test]
    fn the_closed_form_splits_at_bpm_changes() {
        let mut doc = Document::default();
        doc.bpm_list = vec![
            BpmEntry { start: Beat::zero(), bpm: 120.0, foreign: Default::default() },
            BpmEntry { start: Beat::new(4, 1), bpm: 240.0, foreign: Default::default() },
        ];
        let tmap = TimeMap::from_doc(&doc);
        let events = vec![ev(0.0, 8.0, 0.0, 20.0)];
        let want = 25.0 * 120.0;
        let got = speed_travel(&events, &tmap, 0.0, 3.0);
        assert!((got - want).abs() < 1e-6, "整段应为 {want}（各段各自平均），实际 {got}");
        // 查表路径必须与它一致（这正是"表与整条走一遍分家"的那个点）
        let table = SpeedTable::build(&events, &tmap, 16.0);
        for sec in [0.5, 1.5, 2.0, 2.5, 3.0] {
            let a = table.h_at(&tmap, sec);
            let b = speed_travel(&events, &tmap, 0.0, sec);
            assert!((a - b).abs() < 1e-6, "t={sec}：查表 {a} ≠ 直积 {b}");
        }
    }

    /// `|v|` 的下界是**解析**求的：折线段的最小值在节点；相邻节点异号 ⇒ 中间过零 ⇒ 0
    #[test]
    fn min_speed_magnitude_is_analytic() {
        let tmap = tmap120();
        let no = |v: f64| vec![ev(0.0, 8.0, v, v)];
        assert_eq!(min_speed_magnitude(&[], &tmap), None);
        assert_eq!(min_speed_magnitude(&no(10.0), &tmap), Some(10.0));
        assert_eq!(min_speed_magnitude(&no(-4.0), &tmap), Some(4.0), "负流速取绝对值");
        // 斜坡 2 → 8：最小在起点
        assert_eq!(min_speed_magnitude(&vec![ev(0.0, 8.0, 2.0, 8.0)], &tmap), Some(2.0));
        // 斜坡 −3 → +5：中间穿过 0 ⇒ 下界是 0（"穿过窗口要多久"发散 ⇒ 调用方夹到上限）
        assert_eq!(min_speed_magnitude(&vec![ev(0.0, 8.0, -3.0, 5.0)], &tmap), Some(0.0));
        // 多事件取最小
        let two = vec![ev(0.0, 4.0, 20.0, 20.0), ev(4.0, 8.0, 5.0, 0.5)];
        assert_eq!(min_speed_magnitude(&two, &tmap), Some(0.5));
        // **过冲**的缓动：节点里的最小值才是真下界（解析曲线的过冲点也可能不在节点上，
        // 但折线只在节点之间有定义，所以节点扫描就是精确的）
        let over = vec![ev_ease(0.0, 8.0, 0.0, 2.0, "inOutBack")];
        let m = min_speed_magnitude(&over, &tmap).unwrap();
        assert!(m < 0.0 || m >= 0.0, "下界必须是个有限数：{m}");
        assert!(m <= 2.0, "inOutBack 会冲到 0 以下 ⇒ 下界应当很小或为 0，实际 {m}");
    }

    /// **五条轨道同一条路径**（连流速也是）：缓动按折线实现之后，"线怎么动"与"
    /// 音符怎么走"来自同一个函数 —— 再没有"哪条轨道用哪种求值"这个岔口。
    #[test]
    fn all_five_tracks_evaluate_through_the_same_polyline() {
        let tmap = tmap120();
        let eased = vec![ev_ease(0.0, 8.0, 0.0, 20.0, "outBounce")];
        let beat = tmap.beat(2.0); // 0→20 走 4 秒，第 2 秒是拍 4
        let sp = track_value(&eased, beat, &tmap).unwrap();
        let mx = track_value(&eased, beat, &tmap).unwrap();
        assert_eq!(sp, mx, "五条轨道同一个数");
        assert!(sp > 10.0, "outBounce 中点应明显高于线性中点，实际 {sp}");
        // 与"块末按位相等"同样重要的是：端点仍然是端值
        assert_eq!(track_value(&eased, 0.0, &tmap), Some(0.0));
        assert_eq!(track_value(&eased, 8.0, &tmap), Some(20.0));
    }

    /// **检查点表 = 逐段直接积分**：几百个时刻（事件中间、空隙里、末尾之后）逐一对账。
    ///
    /// 这条是"预算好的位置"与"现算的位置"能互为基准的前提 —— 两条路径共用同一份段划分与段内积分。
    /// 五种形状都过一遍：多段斜坡、负流速、带空隙、**非线性缓动**（折线要切开）、没有事件（默认 10）。
    #[test]
    fn the_checkpoint_table_agrees_with_direct_integration() {
        let tmap = tmap120();
        let shapes: Vec<Vec<Event>> = vec![
            vec![ev(0.0, 2.0, 10.0, 20.0), ev(2.0, 6.0, 20.0, 20.0), ev(6.0, 10.0, 20.0, 0.0)],
            vec![ev(0.0, 2.0, -10.0, 10.0), ev(2.0, 4.0, 10.0, -5.0)],
            vec![ev(0.0, 2.0, 4.0, 4.0), ev(4.0, 6.0, 0.0, 0.0)], // 中间有空隙
            vec![ev_ease(0.0, 8.0, 0.0, 20.0, "inOutElastic")],  // 非线性缓动
            vec![],
        ];
        for events in shapes {
            let table = SpeedTable::build(&events, &tmap, 20.0);
            for k in 0..=400 {
                let sec = k as f64 * 0.05; // 0 … 20 秒
                let want = speed_travel(&events, &tmap, 0.0, sec);
                let got = table.h_at(&tmap, sec);
                // 闭式积分（不是抽样近似）⇒ 两条路应当**完全一致**，容差只留浮点噪声
                assert!(
                    (got - want).abs() < 1e-9,
                    "事件 {} 条：t={sec} 查表 {got} ≠ 直积 {want}",
                    events.len()
                );
            }
        }
    }

    /// 节点缓冲复用（`knot_us_into`）与"每次新分配"必须给出**逐位相同**的节点。
    #[test]
    fn reusing_the_knot_buffer_changes_nothing() {
        let mut buf: Vec<f64> = vec![9.9; 7]; // 故意带垃圾：`knot_us_into` 必须自己清
        for name in ["linear", "outCubic", "outBounce", "inOutElastic", "inElastic"] {
            for d in [0.05, 0.3, 1.333, 8.0, 40.0] {
                let fresh = knot_us(name, d);
                knot_us_into(name, d, &mut buf);
                assert_eq!(buf, fresh, "{name}（{d} 秒）复用缓冲与新建不一致");
            }
        }
    }

    /// **后缀重算 = 从零重建**（**按位**相等）：拖动流速的值时走的就是后缀那条路，
    /// 它与"整张表重建一遍"必须一个字都不差。
    ///
    /// 前缀积分的性质保证它**能**相等：改动点之前的切点、值、斜率、`H` 全都没变，
    /// 后面的增量也是同样的顺序加同样的数 —— 所以这里敢用按位断言。
    #[test]
    fn reaccumulating_from_a_point_matches_a_full_rebuild() {
        let tmap = tmap120();
        let shapes: Vec<Vec<Event>> = vec![
            vec![ev(0.0, 4.0, 10.0, 20.0), ev(4.0, 8.0, 20.0, -5.0), ev(8.0, 12.0, -5.0, 5.0)],
            vec![ev(0.0, 2.0, 4.0, 4.0), ev(4.0, 6.0, 0.0, 0.0)], // 空隙 ⇒ 值在切点处跳变
            vec![ev_ease(0.0, 8.0, 0.0, 20.0, "outBounce"), ev(8.0, 12.0, 20.0, 0.0)],
            vec![ev_ease(0.0, 6.0, 8.0, 8.0, "inOutElastic")],
        ];
        for events in shapes {
            let base = SpeedTable::build(&events, &tmap, 16.0);
            for k in 0..events.len() {
                // 只改**值**（起止拍与缓动不动）—— 这正是"滑一个流速值"
                let mut after = events.clone();
                after[k].start_value = serde_json::json!(3.25);
                after[k].end_value = serde_json::json!(7.5);
                assert!(
                    speed_shape_unchanged(&events, &after),
                    "改值不该动形状（这条用例的前提）"
                );
                let mut tuned = base.clone();
                tuned.reaccumulate_from(&after, &tmap, after[k].start.to_f64());
                let fresh = SpeedTable::build(&after, &tmap, 16.0);
                for i in 0..=400 {
                    let sec = i as f64 * 0.05;
                    assert_eq!(
                        tuned.h_at(&tmap, sec).to_bits(),
                        fresh.h_at(&tmap, sec).to_bits(),
                        "第 {k} 条改了值：t={sec} 上后缀重算 {} ≠ 全量重建 {}",
                        tuned.h_at(&tmap, sec),
                        fresh.h_at(&tmap, sec)
                    );
                }
            }
        }
        // 动了形状（起止拍）就**不能**走后缀那条路
        let a = vec![ev(0.0, 4.0, 10.0, 10.0)];
        let b = vec![ev(0.0, 6.0, 10.0, 10.0)];
        assert!(!speed_shape_unchanged(&a, &b), "起止拍动了 ⇒ 形状变了");
        let c = vec![ev_ease(0.0, 4.0, 10.0, 10.0, "outBounce")];
        assert!(!speed_shape_unchanged(&a, &c), "缓动动了 ⇒ 折线节点变了");
    }

    /// **过去也能问**（这是被换掉那个累加器的病根：它只会往前走，问过去一律返回当前累计值 0，
    /// hold 的尾巴因此被钉死在"头 + 全长"上，身子不随按住而缩短）。
    #[test]
    fn the_table_answers_about_the_past_too() {
        let tmap = tmap120();
        let events = vec![ev(0.0, 8.0, 10.0, 10.0)];
        let table = SpeedTable::build(&events, &tmap, 20.0);
        let early = table.h_at(&tmap, 2.0);
        let late = table.h_at(&tmap, 5.0);
        assert!((early - 2.0 * 1200.0).abs() < 1e-6, "2 秒处应是 2400，实际 {early}");
        assert!(early < late, "5 秒处应更远：{late} ≤ {early}");
        // 问的顺序不影响答案（累加器时代这里会返回"当前累计值"）
        assert_eq!(table.h_at(&tmap, 2.0), early);
    }

    /// 提示（hint）只是"从哪一段开始找"：升序查询时摊还 O(1)，但**降序查询也必须给对值**
    #[test]
    fn the_segment_hint_only_speeds_things_up() {
        let tmap = tmap120();
        let events = vec![ev(0.0, 2.0, 0.0, 20.0), ev(2.0, 8.0, 20.0, -10.0)];
        let table = SpeedTable::build(&events, &tmap, 20.0);
        let mut hint = 0usize;
        let mut seen = 0usize;
        for k in 0..=200 {
            let sec = k as f64 * 0.1;
            let (a, h) = table.h_at_hinted(&tmap, sec, hint);
            hint = h;
            seen = seen.max(h);
            assert_eq!(a, table.h_at(&tmap, sec), "t={sec} 带提示的值必须一样");
        }
        assert!(seen > 0, "这条用例本身要走过多个段（否则提示根本没被用到）");
        // 倒着走：提示失效 ⇒ 二分回去，值照样对
        for k in (0..=200).rev() {
            let sec = k as f64 * 0.1;
            let (a, h) = table.h_at_hinted(&tmap, sec, hint);
            hint = h;
            assert_eq!(a, table.h_at(&tmap, sec), "倒序 t={sec} 值必须一样");
        }
    }

    /// 反向区间与零长度：一律 0（不返回负数，免得音符被画到判定线下面去）
    #[test]
    fn a_reversed_interval_is_zero() {
        let tmap = tmap120();
        let events = vec![ev(0.0, 16.0, 10.0, 10.0)];
        assert_eq!(speed_travel(&events, &tmap, 2.0, 2.0), 0.0);
        assert_eq!(speed_travel(&events, &tmap, 2.0, 1.0), 0.0);
        // 表也一样：问"0 秒之前"给 0，而不是某个负值
        let table = SpeedTable::build(&events, &tmap, 20.0);
        assert_eq!(table.h_at(&tmap, 0.0), 0.0);
    }
}

#[cfg(test)]
mod mask_tests {
    use super::*;
    use crate::doc::{Beat, BpmEntry, MASK_TRACKS};

    fn tmap120() -> TimeMap {
        let mut doc = Document::default();
        doc.bpm_list = vec![BpmEntry {
            start: Beat::zero(),
            bpm: 120.0,
            foreign: Default::default(),
        }];
        TimeMap::from_doc(&doc)
    }

    /// 七条轨道（顺序 = `doc::MASK_TRACKS`），只给用得上的几条赋值
    fn tracks(sets: &[(&str, Vec<Event>)]) -> Vec<Vec<Event>> {
        let mut out: Vec<Vec<Event>> = vec![Vec::new(); 7];
        for (name, list) in sets {
            let i = MASK_TRACKS.iter().position(|t| t == name).expect("轨道名");
            out[i] = list.clone();
        }
        out
    }

    fn ev(a: f64, b: f64, v0: f64, v1: f64) -> Event {
        Event::new(
            Beat::new((a * 4.0) as i64, 4),
            Beat::new((b * 4.0) as i64, 4),
            serde_json::json!(v0),
            serde_json::json!(v1),
            "linear",
        )
    }

    fn at(t: &[Vec<Event>], beat: f64) -> MaskState {
        let tmap = tmap120();
        let r: [&[Event]; 7] = [
            &t[0], &t[1], &t[2], &t[3], &t[4], &t[5], &t[6],
        ];
        mask_state_at(&r, beat, &tmap)
    }

    /// **三条坐标轨道都没有事件 ⇒ 不显示**（与判定线不同：没有事件就没有形状）
    #[test]
    fn a_zone_without_any_coordinate_event_does_not_exist() {
        let t = tracks(&[]);
        let s = at(&t, 0.0);
        assert!(!s.visible, "没有坐标事件 ⇒ 不显示");
        assert_eq!(s.v, [[0.0, 0.0]; 3], "空轨道一律 (0,0)");
        assert!(!s.active, "没有 active 事件 ⇒ false（纯色那一档）");
    }

    /// 只有**一条**坐标轨道的首事件已经开始时，整块就算"还在"：其余顶点拿 (0,0)
    #[test]
    fn one_started_channel_makes_the_whole_zone_exist() {
        let t = tracks(&[("x1", vec![ev(4.0, 8.0, 100.0, 200.0)])]);
        // 首事件之前：不显示（"如果当前时间没有任何对应的坐标事件块，则不显示"）
        assert!(!at(&t, 0.0).visible);
        // 事件开始之后：显示，且 x1 走它自己的缓动
        let s = at(&t, 4.0);
        assert!(s.visible);
        assert_eq!(s.v[0][0], 100.0, "块首取起始值");
        assert_eq!(s.v[0][1], 0.0, "y1 没有事件 ⇒ (0,0)");
        // 块内线性插值（4→8 拍的中点 = 150）
        assert_eq!(at(&t, 6.0).v[0][0], 150.0);
        // 末事件之后：**延续最后值**
        assert_eq!(at(&t, 20.0).v[0][0], 200.0);
    }

    /// 块**之前**那条口径：首事件之前取首事件的起始值（`track_value` 的规则），
    /// 但因为"没有任何已开始的事件"，此时整块本来就不显示 —— 两者一起看才对
    #[test]
    fn before_the_first_event_the_zone_is_hidden_but_the_value_is_the_first_start() {
        let t = tracks(&[
            ("x1", vec![ev(4.0, 8.0, 100.0, 200.0)]),
            ("active", vec![ev(0.0, 4.0, 1.0, 0.0)]),
        ]);
        let s = at(&t, 1.0);
        assert!(!s.visible, "坐标还没开始 ⇒ 已经不存在");
        assert!(s.active, "active 是独立通道：1.0 ≥ 0.5 ⇒ true");
        assert_eq!(s.v[0][0], 100.0, "x1 取首事件起始值（求值器对'块之前'的口径）");
    }

    /// `active`：同一套插值，最后按 **≥ 0.5** 二值化（斜坡在中点翻面）
    #[test]
    fn active_is_interpolated_then_thresholded() {
        let t = tracks(&[("x1", vec![ev(0.0, 32.0, 0.0, 0.0)]), ("active", vec![ev(0.0, 4.0, 0.0, 1.0)])]);
        assert!(!at(&t, 0.0).active, "0.0 < 0.5");
        assert!(!at(&t, 1.9).active, "0.475 < 0.5");
        assert!(at(&t, 2.1).active, "0.525 ≥ 0.5");
        assert!(at(&t, 4.0).active, "块末 = 1");
        assert!(at(&t, 99.0).active, "末事件之后延续终值");
    }

    /// 每个顶点两维**各自独立**求值（x 与 y 的缓动/时刻互不影响）
    #[test]
    fn the_two_axes_of_a_vertex_are_independent() {
        let t = tracks(&[
            ("x1", vec![ev(0.0, 4.0, 0.0, 400.0)]),
            ("y1", vec![ev(8.0, 12.0, 0.0, -200.0)]),
        ]);
        let s = at(&t, 8.0);
        assert!(s.visible);
        assert_eq!(s.v[0], [400.0, 0.0], "x 已经走到末值，y 刚到起点");
        assert_eq!(s.v[1], [0.0, 0.0], "第二个顶点完全没事件");
    }
}
