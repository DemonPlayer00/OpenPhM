// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! 表演求值：**拍 ↔ 秒**的时间映射 + 29 个具名缓动的函数本体 + 事件轨道求值。
//!
//! 判定线是这个编辑器的父对象：它的位置/旋转/透明度/流速全部来自**事件轨道**，
//! 音符只是挂在它下面的子对象（跟着线一起平移/旋转）。因此"事件真的生效"是前提 ——
//! 本模块就是那个前提。
//!
//! 两处口径（与 `spec/` 对齐）：
//! · 缓动函数本体用通用实现（easings.net 命名），编号语义依据 Phira Documents —— 见 `spec/easing.json`；
//! · 时间映射按 `bpmList` 分段线性：第 i 段从 `bpmList[i].startBeat` 起、按 `bpmList[i].bpm` 走，
//!   到下一段起点为止。早先只按首个 BPM 换算，多 BPM 谱面会整体跑偏（本轮补上）。

use crate::doc::{Document, Event, JudgeLine, Layer};
use serde_json::Value;

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
        let mut raw: Vec<(f64, f64)> = doc
            .bpm_list
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
        let end_beat = doc.chart_end().to_f64().max(segs.last().map(|s| s.0).unwrap_or(0.0));
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
pub fn eval_events(events: &[Event], beat: f64) -> Option<f64> {
    if events.is_empty() {
        return None;
    }
    match active_event(events, beat) {
        Some(i) => Some(event_value(&events[i], beat)),
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
/// 与 [`event_t`] 一起构成"取值"的**唯一一份**实现：[`event_value`] 与 [`speed_value`]
/// 的区别只有 `t` 要不要过缓动，这一层的端点/缺值规则没有第二条路。
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
/// 要的正是**按位相等** —— 于是端点直接取端值，中间照旧走插值（含 `back`/`elastic` 的过冲）。
///
/// 缺端点（`None`）时不介入：那种情形 [`interp`] 返回的是常量，本来就没有算术误差。
fn endpoint_value(e: &Event, t: f64) -> Option<f64> {
    match (as_f64(&e.start_value), as_f64(&e.end_value)) {
        (Some(v0), Some(_)) if t <= 0.0 => Some(v0),
        (Some(_), Some(v1)) if t >= 1.0 => Some(v1),
        _ => None,
    }
}

/// 单条事件在拍 `beat` 处的值（按它自己的缓动）。
///
/// 从 [`eval_events`] 里抽出来的：流速积分要在**段内**反复求值，
/// 值怎么算只该有一份实现 —— 两份实现迟早会在某个缓动上分家。
pub fn event_value(e: &Event, beat: f64) -> f64 {
    let t = event_t(e, beat);
    match endpoint_value(e, t) {
        Some(v) => v,
        None => interp(e, ease(&e.easing, t)),
    }
}

/// **流速事件的值：只按线性取**（`easing` 字段被忽略 —— 见模块头"只实现 linear"）。
///
/// 为什么可以这么简化：流速事件在两个端点之间按**线性**变化时，`∫v dτ` 有闭式
/// `(v₀+v₁)/2 × Δt`（精确、无抽样），整条链路（检查点表、异步重算、现算兜底）都因此变简单；
/// 而"缓动过的流速"在编辑器里既难看出差别（音符位置是它的**积分**，缓动会被积掉大半），
/// 又让每次求值都要抽 8 个点。
///
/// 导入的谱面里若真有非线性缓动的流速事件，**文档原样保留**（`easing` 不改写），
/// 只是预览/求值按线性算 —— 导入报告里会写明，检查器里也标出来。
pub fn speed_value(e: &Event, beat: f64) -> f64 {
    // 线性 = 不过缓动：`t` 本身就是缓动后的 `t`。
    // 端点同样直接取端值：流速是**积分**，段端点差 1 ulp 会进检查点表（那里正是要逐位可对账）
    let t = event_t(e, beat);
    match endpoint_value(e, t) {
        Some(v) => v,
        None => interp(e, t),
    }
}

/// 一条轨道在拍 `beat` 处的值 —— **"哪条轨道用哪种求值"的唯一判断处**。
///
/// 流速轨（`"speed"`）走 [`speed_value`]（只线性），其余四条走 [`eval_events`]（事件自己的缓动）。
/// 单独一个入口是为了不漏：树面板/检查器/时间轴/`lines` 各显示一个"此刻的值"，
/// 谁要是直接调 `eval_events`，同一时刻就会显示两个不同的流速。
///
/// **返回值里的"空位"口径**（用户要求：事件块前后有空位时保持相邻那块的值）：
/// · 事件块**之后**：保持末事件的**终值**（`active_event` 取到末条，取值夹在它自己的终点上）；
/// · 事件块**之前**：取首事件的**起始值**（`unwrap_or(0)` 那一步 —— 早先这里返回 `None`，
///   调用方于是回落到**全局默认值**（流速 10 / 透明度 1 / 移动 0），
///   和这条轨道真正的值不是一个东西）；
/// · **空轨道**才返回 `None` —— 那是"全局默认值"唯一该出现的地方（流速 10、透明度 1、移动 0）。
pub fn track_value(track: &str, events: &[Event], beat: f64) -> Option<f64> {
    if events.is_empty() {
        return None;
    }
    let i = active_event(events, beat).unwrap_or(0);
    Some(if track == "speed" {
        speed_value(&events[i], beat)
    } else {
        event_value(&events[i], beat)
    })
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
/// 实现：**闭式** `∫v dτ = (v(a) + v(b))/2 × Δt`（流速事件只按线性，见 [`speed_value`]）——
/// 精确、无抽样。切点有两类：**事件边界**（段的划分）与 **BPM 段起点**
/// （拍↔秒在那里折了一下，端点平均就不准了）。
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
/// **解析求，不抽样**：流速事件是线性的，所以 `|v|` 在一段上的最小值只可能在**端点**；
/// 端点异号则中间必然穿过 0 ⇒ 下界就是 **0**（"穿过窗口要多久"发散，调用方夹到上限）。
///
/// **过零必须算进去**：流速过零时音符会在判定线附近**长时间逗留**（偏移 ≈ 0，一直在窗口里）——
/// 踩过的坑：早先按 `|v| ≥ 0.05` 过滤，于是斜坡过零的那种谱面下界取成 1.25 ⇒ 窗口只有 3.4 秒
/// ⇒ 3.45 秒外那颗**就贴在判定线上**的音符整颗没有实例。
/// 返回 `None` = 没有流速事件（调用方按 `SPEED_DEFAULT` 处理）。
pub fn min_speed_magnitude(events: &[Event]) -> Option<f64> {
    if events.is_empty() {
        return None;
    }
    let mut best = f64::INFINITY;
    for e in events {
        let a = e.start.to_f64();
        let b = e.end.to_f64();
        let (v0, v1) = (speed_value(e, a), speed_value(e, b));
        let m = if v0 * v1 < 0.0 { 0.0 } else { v0.abs().min(v1.abs()) };
        best = best.min(m);
    }
    best.is_finite().then_some(best)
}

/// 一段流速的求值方式：走在某条事件的缓动上（**记下标**：检查点表要把"哪一段"存下来，
/// 跨帧、跨查询用），或"保持"某个定值（空隙里 / 首尾之外）。
#[derive(Clone, Copy, Debug)]
enum SpeedSeg {
    Eased(usize),
    Hold(f64),
}

impl SpeedSeg {
    fn at(self, events: &[Event], beat: f64) -> f64 {
        match self {
            SpeedSeg::Hold(v) => v,
            SpeedSeg::Eased(i) => events.get(i).map(|e| speed_value(e, beat)).unwrap_or(0.0),
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

/// 流速轨的**分段表示**（唯一表示）：切点 = **事件起点 ∪ 事件终点 ∪ BPM 段起点**，
/// 逐段给出那一段的流速来源。返回 `(段起点拍, 该段怎么求值)`，**按拍升序**，
/// 最后一段一直延伸到查询的终点。
///
/// 为什么这么切：段内"哪条事件生效"不变（[`active_event`] 只会在事件起点处换人），
/// 值函数在秒域是线性的（事件线性 + 段内 BPM 不变）⇒ [`integrate_seg`] 的闭式是**精确值**。
/// 事件终点也是切点：过了终点之后取值夹在终值上（= "前值延拓"，与 `normalize` 一致）。
///
/// 有了它，[`speed_travel`]（现积）与 [`SpeedTable`]（检查点）走的是**同一份分段**，
/// 原先那套"带着 idx 一段段往前挪"的走法（以及它在空隙/重叠上的三个 bug）整块删掉。
fn speed_segments(events: &[Event], tmap: &TimeMap, b_to: f64) -> Vec<(f64, SpeedSeg)> {
    let b0 = tmap.beat(0.0);
    let mut cuts: Vec<f64> = vec![b0, b_to];
    for e in events {
        cuts.push(e.start.to_f64());
        cuts.push(e.end.to_f64());
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
/// 流速事件只按线性（见 [`speed_value`]）⇒ 这个式子是**精确值**，不是近似：
/// 不需要抽点、没有采样误差。代价是调用方必须保证"这一段里流速在**秒域**上线性" ——
/// BPM 变化处由 [`walk_speed`] 切开（`TimeMap::next_seg_start`）。
///
/// **返回 `∫v dτ`（流速单位 × 秒），不乘 120** —— 换算成 RPE y 单位只在
/// [`speed_travel`] 与 [`SpeedTable::h_at_hinted`] 那两处发生，免得两条路径各乘一次或漏乘。
fn integrate_seg(tmap: &TimeMap, events: &[Event], seg: &SpeedSeg, b_from: f64, b_to: f64) -> f64 {
    if !(b_to > b_from) {
        return 0.0;
    }
    let dt = tmap.sec(b_to) - tmap.sec(b_from);
    match seg {
        SpeedSeg::Hold(v) => v * dt,
        SpeedSeg::Eased(_) => 0.5 * (seg.at(events, b_from) + seg.at(events, b_to)) * dt,
    }
}

/// 流速积分的**分段检查点**：每个段起点上的 `H`（`H = 120 ∫ v dτ`，从谱面 0 秒起）。
///
/// 存在的理由：`H(t)` 是**前缀**积分 ——
/// ① 每帧从 0 重积是 O(流速事件数)；
/// ② 更要紧的是"**随机取一段音符来算**"（流速事件改了之后要重算它之后的音符、以及还没重算完
///    那几颗的兜底现算）：若从 0 起到每个时刻各积一遍，就是 O(音符 × 流速事件)。
/// 有了检查点，`H(t)` = 查一次表（二分）+ 在段内积一小段，代价与 t 在哪儿、问的是哪一段无关。
///
/// **切法与 [`walk_speed`] 共用同一份实现、段内积分共用 [`integrate_seg`]**，所以
/// "查表得到的 `H`"与"从 0 整条走一遍"是同一个数 —— 于是"预算好的位置"与"现算的位置"
/// 可以互为基准对账（`tests/lines.rs` 里那条对账就是这么钉的）。
///
/// 被它换掉的那个**单调累加器**只能往前问：查询过去的时刻会静默返回当前累计值（0），
/// hold 尾巴"被钉死在头 + 全长"那个 bug 就是从这儿来的。检查点表没有这个毛病 ——
/// 过去、现在、将来都能问。
#[derive(Clone, Debug, Default)]
pub struct SpeedTable {
    /// 段起点：`(拍, 该点的 H（RPE y 单位）, 这一段怎么求值)`
    cuts: Vec<(f64, f64, SpeedSeg)>,
}

impl SpeedTable {
    /// 建表：切到 `b_end`（拍）为止。**O(流速事件数)**
    ///
    /// 最后一段之后不再有点：查表时用最后一段的求值方式外推（末尾之后是"保持"，
    /// 而事件段之后 `event_value` 自己就夹在终点值上 —— 与走一遍的语义一致）。
    pub fn build(events: &[Event], tmap: &TimeMap, b_end: f64) -> Self {
        let segs = speed_segments(events, tmap, b_end);
        let mut cuts: Vec<(f64, f64, SpeedSeg)> = Vec::with_capacity(segs.len());
        let mut acc = 0.0; // `∫v dτ`（流速单位 × 秒）
        for (k, (beat, seg)) in segs.iter().enumerate() {
            cuts.push((*beat, acc * SPEED_UNITS_PER_SEC, *seg));
            if let Some(next) = segs.get(k + 1).map(|(b, _)| *b) {
                acc += integrate_seg(tmap, events, seg, *beat, next);
            }
        }
        Self { cuts }
    }

    /// `H(sec)`（RPE y 单位）。**任何时刻都能问**（过去 / 现在 / 将来一视同仁）。
    pub fn h_at(&self, events: &[Event], tmap: &TimeMap, sec: f64) -> f64 {
        self.h_at_hinted(events, tmap, sec, 0).0
    }

    /// 同上，但允许带一个"上次落在哪一段"的提示（升序查询时摊还 O(1)）。
    /// 返回 `(H, 新的提示)`；提示只当**起点**用：不命中就二分，所以倒退查询不会算错。
    pub fn h_at_hinted(
        &self,
        events: &[Event],
        tmap: &TimeMap,
        sec: f64,
        hint: usize,
    ) -> (f64, usize) {
        let b = tmap.beat(sec);
        let i = self.seg_index(b, hint);
        let Some(&(beat, h, seg)) = self.cuts.get(i) else {
            // 表还没建（这条线是从 `line_shell` 造出来的、事件轨道还是空的）：
            // 退回直接积分。两条路径的值相同（共用分段与段内积分），只是慢。
            return (speed_travel(events, tmap, 0.0, sec), 0);
        };
        // `[cut.beat, b]` 落在**一段之内**（BPM 段起点也是切点）⇒ 一次闭式就够
        (h + integrate_seg(tmap, events, &seg, beat, b) * SPEED_UNITS_PER_SEC, i)
    }

    /// `beat` 落在第几段：`hint` 命中就 O(1)（升序查询的顺序），否则二分
    fn seg_index(&self, beat: f64, hint: usize) -> usize {
        let hit = self
            .cuts
            .get(hint)
            .is_some_and(|c| c.0 <= beat && self.cuts.get(hint + 1).is_none_or(|n| n.0 > beat));
        if hit {
            return hint;
        }
        self.cuts.partition_point(|c| c.0 <= beat).saturating_sub(1)
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
/// ——而"流速只按 linear 求值"才是定的规矩）。两份实现里"看起来一样"的那四条轨道，
/// 只是还没轮到它们分家而已。每一条都经 [`track_value`]（"哪条轨道用哪种求值"的唯一判断处）。
pub fn perf_of(tracks: &[&[Event]; 5], beat: f64) -> LinePerf {
    let mut p = LinePerf::default();
    let mut v = [0.0f64; 5];
    let mut has = [false; 5];
    for (i, ev) in tracks.iter().enumerate() {
        if let Some(x) = track_value(crate::doc::TRACKS[i], ev, beat) {
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
    perf_of(&ev, tmap.beat(sec))
}

/// 把一条轨道采样成折线（供时间轴画曲线）：每个事件取 `per_event+1` 个点。
///
/// 线性缓动其实只需两端点，但 29 个缓动里有非线性/回弹（elastic/bounce），
/// 少采样会让曲线形状骗人 —— 显示用 N 点采样，求值仍走 [`eval_events`] 的精确公式。
/// `linear_only` 给流速轨用（它只按线性求值，见 [`speed_value`]）。
pub fn sample_track(
    events: &[Event],
    tmap: &TimeMap,
    per_event: usize,
    linear_only: bool,
) -> Vec<[f32; 2]> {
    let mut out: Vec<[f32; 2]> = Vec::new();
    if events.is_empty() {
        return out;
    }
    let n = per_event.max(1);
    // `linear_only` = 流速轨：曲线要与求值**同一条口径**（只用线性），
    // 否则时间轴画的是缓动、音符位置却是线性积分 —— 两个面板互相打脸
    let value_of = |e: &Event, beat: f64| -> f64 {
        if linear_only {
            speed_value(e, beat)
        } else {
            event_value(e, beat)
        }
    };
    let mut push = |beat: f64, v: f64| out.push([tmap.sec(beat) as f32, v as f32]);

    // ① 首条事件**之前**的空位：保持首条的起始值（与 `track_value`/`active_event` 同一条规则）
    let b0 = tmap.beat(0.0);
    let first = &events[0];
    if first.start.to_f64() > b0 + 1e-9 {
        let v = value_of(first, first.start.to_f64());
        push(b0, v);
        push(first.start.to_f64(), v);
    }
    for (i, e) in events.iter().enumerate() {
        let (a, b) = (e.start.to_f64(), e.end.to_f64());
        for k in 0..=n {
            let t = k as f64 / n as f64;
            push(a + (b - a) * t, value_of(e, a + (b - a) * t));
        }
        // ② 两条事件之间的空位：**保持这一条的终值**（不是插值过去 —— 那与求值器不一致）
        if let Some(next) = events.get(i + 1) {
            let ns = next.start.to_f64();
            if ns > b + 1e-9 {
                let v = value_of(e, b);
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
        let v = value_of(last, le);
        push(le, v);
        push(tmap.end_beat, v);
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
        assert_eq!(eval_events(&events, 2.0), Some(10.0));
        assert_eq!(eval_events(&events, 6.0), Some(30.0));
        assert_eq!(eval_events(&events, 20.0), Some(30.0));
        assert_eq!(active_event(&events, 6.0), Some(1));
        // 积分同样：H 与"规范化之后的轨道"必须逐点相同
        let normalized = crate::codec::normalize_track(
            events.clone(),
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
                Beat::new(32, 1),
                "/x",
                &mut crate::codec::Fidelity::new("test", "v1".into()),
            )
            .0;
            for k in 0..=400 {
                let beat = k as f64 * 0.1;
                let (a, b) = (eval_events(&events, beat), eval_events(&normalized, beat));
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
        assert_eq!(track_value("speed", &events, 2.0), Some(7.0), "块**之前**取块的起始值");
        assert_eq!(track_value("alpha", &events, 2.0), Some(7.0), "四条轨道同一口径");
        assert_eq!(track_value("speed", &events, 12.0), Some(7.0), "块里");
        assert_eq!(track_value("speed", &events, 100.0), Some(7.0), "块**之后**保持终值");
        // 斜坡：块前取起始值、块后取终值（不是 0、也不是别的默认值）
        let ramp = vec![ev(8.0, 16.0, 2.0, 9.0)];
        assert_eq!(track_value("moveX", &ramp, 2.0), Some(2.0));
        assert_eq!(track_value("moveX", &ramp, 100.0), Some(9.0));
        // **空轨道** ⇒ None（调用方用自己的默认值：流速 10 / 透明度 1 / 移动 0）
        assert_eq!(track_value("speed", &[], 2.0), None);
        assert_eq!(eval_events(&[], 2.0), None);
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
        let pts = sample_track(&one, &tmap, 4, false);
        let first = pts.first().expect("非空");
        assert!(first[0].abs() < 1e-6, "曲线要从谱面开头（拍 0 = 0 秒）起");
        assert!((first[1] - 5.0).abs() < 1e-6, "开头是首事件的起始值，实际 {}", first[1]);
        let last = pts.last().expect("非空");
        assert!((last[0] as f64 - tmap.sec(tmap.end_beat)).abs() < 1e-3, "画到谱面末尾");
        assert!((last[1] - 5.0).abs() < 1e-6, "末尾保持末事件的终值，实际 {}", last[1]);
        // 事件之间的空位：保持前一条的终值（两点同值 ⇒ 水平段，不是插值）
        let gap = vec![ev(0.0, 4.0, 5.0, 5.0), ev(8.0, 12.0, 9.0, 9.0)];
        let pts = sample_track(&gap, &tmap, 4, false);
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

    /// **流速事件只按线性取**：`easing` 字段被忽略，且闭式积分是**精确值**（无抽样误差）。
    ///
    /// 记一条 `outElastic` 的流速事件，`H` 必须与记成 `linear` 的**一模一样**。
    #[test]
    fn speed_events_ignore_their_easing() {
        let tmap = tmap120();
        let eased = vec![ev_ease(0.0, 8.0, 0.0, 20.0, "outElastic")];
        let linear = vec![ev(0.0, 8.0, 0.0, 20.0)];
        let t_eased = SpeedTable::build(&eased, &tmap, 20.0);
        let t_linear = SpeedTable::build(&linear, &tmap, 20.0);
        for k in 0..=100 {
            let sec = k as f64 * 0.1;
            let (a, b) = (
                t_eased.h_at(&eased, &tmap, sec),
                t_linear.h_at(&linear, &tmap, sec),
            );
            assert_eq!(a, b, "t={sec} 缓动过的流速事件必须与线性完全相同（{a} ≠ {b}）");
        }
        // 闭式积分对线性是精确的：0→20 用 4 秒 ⇒ H(4s) = 平均 10 × 4 × 120
        assert!((t_eased.h_at(&eased, &tmap, 4.0) - 10.0 * 4.0 * 120.0).abs() < 1e-9);
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
            let a = table.h_at(&events, &tmap, sec);
            let b = speed_travel(&events, &tmap, 0.0, sec);
            assert!((a - b).abs() < 1e-6, "t={sec}：查表 {a} ≠ 直积 {b}");
        }
    }

    /// `|v|` 的下界是**解析**求的：线性段的最小值在端点；端点异号 ⇒ 中间过零 ⇒ 0
    #[test]
    fn min_speed_magnitude_is_analytic() {
        let no = |v: f64| vec![ev(0.0, 8.0, v, v)];
        assert_eq!(min_speed_magnitude(&[]), None);
        assert_eq!(min_speed_magnitude(&no(10.0)), Some(10.0));
        assert_eq!(min_speed_magnitude(&no(-4.0)), Some(4.0), "负流速取绝对值");
        // 斜坡 2 → 8：最小在起点
        assert_eq!(min_speed_magnitude(&vec![ev(0.0, 8.0, 2.0, 8.0)]), Some(2.0));
        // 斜坡 −3 → +5：中间穿过 0 ⇒ 下界是 0（"穿过窗口要多久"发散 ⇒ 调用方夹到上限）
        assert_eq!(min_speed_magnitude(&vec![ev(0.0, 8.0, -3.0, 5.0)]), Some(0.0));
        // 多事件取最小
        let two = vec![ev(0.0, 4.0, 20.0, 20.0), ev(4.0, 8.0, 5.0, 0.5)];
        assert_eq!(min_speed_magnitude(&two), Some(0.5));
    }

    /// **`track_value`：流速轨只线性，其余四条照旧认缓动**（"哪条轨道用哪种求值"的唯一判断处）
    #[test]
    fn track_value_is_linear_for_speed_only() {
        let tmap = tmap120();
        let eased = vec![ev_ease(0.0, 8.0, 0.0, 20.0, "outBounce")];
        let beat = tmap.beat(2.0); // 0→20 走 4 秒，第 2 秒是拍 4
        let sp = track_value("speed", &eased, beat).unwrap();
        let mx = track_value("moveX", &eased, beat).unwrap();
        assert!((sp - 10.0).abs() < 1e-9, "流速按线性 ⇒ 中点 10，实际 {sp}");
        assert!(mx > 10.0, "outBounce 中点应明显高于线性中点，实际 {mx}");
        // 认不出的轨道名 = 非流速 ⇒ 认缓动
        assert_eq!(track_value("alpha", &eased, beat), Some(mx));
    }

    /// **检查点表 = 逐段直接积分**：几百个时刻（事件中间、空隙里、末尾之后）逐一对账。
    ///
    /// 这条是"预算好的位置"与"现算的位置"能互为基准的前提 —— 两条路径共用同一份段划分与段内积分。
    /// 四种形状都过一遍：缓动斜坡（缓动被忽略 ⇒ 线性）、负流速、带空隙、没有事件（默认流速 10）。
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
                let got = table.h_at(&events, &tmap, sec);
                // 闭式积分（不是抽样近似）⇒ 两条路应当**完全一致**，容差只留浮点噪声
                assert!(
                    (got - want).abs() < 1e-9,
                    "事件 {} 条：t={sec} 查表 {got} ≠ 直积 {want}",
                    events.len()
                );
            }
        }
    }

    /// **过去也能问**（这是被换掉那个累加器的病根：它只会往前走，问过去一律返回当前累计值 0，
    /// hold 的尾巴因此被钉死在"头 + 全长"上，身子不随按住而缩短）。
    #[test]
    fn the_table_answers_about_the_past_too() {
        let tmap = tmap120();
        let events = vec![ev(0.0, 8.0, 10.0, 10.0)];
        let table = SpeedTable::build(&events, &tmap, 20.0);
        let early = table.h_at(&events, &tmap, 2.0);
        let late = table.h_at(&events, &tmap, 5.0);
        assert!((early - 2.0 * 1200.0).abs() < 1e-6, "2 秒处应是 2400，实际 {early}");
        assert!(early < late, "5 秒处应更远：{late} ≤ {early}");
        // 问的顺序不影响答案（累加器时代这里会返回"当前累计值"）
        assert_eq!(table.h_at(&events, &tmap, 2.0), early);
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
            let (a, h) = table.h_at_hinted(&events, &tmap, sec, hint);
            hint = h;
            seen = seen.max(h);
            assert_eq!(a, table.h_at(&events, &tmap, sec), "t={sec} 带提示的值必须一样");
        }
        assert!(seen > 0, "这条用例本身要走过多个段（否则提示根本没被用到）");
        // 倒着走：提示失效 ⇒ 二分回去，值照样对
        for k in (0..=200).rev() {
            let sec = k as f64 * 0.1;
            let (a, h) = table.h_at_hinted(&events, &tmap, sec, hint);
            hint = h;
            assert_eq!(a, table.h_at(&events, &tmap, sec), "倒序 t={sec} 值必须一样");
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
        assert_eq!(table.h_at(&events, &tmap, 0.0), 0.0);
    }
}



