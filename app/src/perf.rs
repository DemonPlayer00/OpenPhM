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

/// 在给定**拍**处对一条事件轨道求值（用事件自己的缓动，不是线性插值）。
///
/// 轨道不变量（无空隙无重叠）由 `set_track_constant`/`normalize` 保证；
/// 这里对空隙/越界的处理是"取最近端点的值"，让编辑器不会因为半成品数据算不出东西。
pub fn eval_events(events: &[Event], beat: f64) -> Option<f64> {
    if events.is_empty() {
        return None;
    }
    for e in events {
        let (a, b) = (e.start.to_f64(), e.end.to_f64());
        if beat >= a && beat <= b {
            return Some(event_value(e, beat));
        }
    }
    // 不在任何事件里 —— **保持**最近一条事件的值，而不是"跳到最后一个事件"。
    //
    // 这里修的是一个会让判定线自己动的 bug：事件之间留了空隙时（比如 [0,10] 与 [20,30]），
    // 拍 15 之前会走到下面 `beat >= events[0].start` 的分支，返回**最后一条事件的终值** ——
    // 于是线在空隙里直接跳到终值，正是用户说的"某时间没有事件却移动了判定线"。
    // 正确语义是"保持"：空隙里维持**前一条事件的终值**（首个事件之前则取它的起始值）。
    held_value(events, beat).or_else(|| as_f64(&events[0].start_value)).or(Some(0.0))
}

/// 单条事件在拍 `beat` 处的值（按它自己的缓动）。
///
/// 从 [`eval_events`] 里抽出来的：流速积分要在**段内**反复求值，
/// 值怎么算只该有一份实现 —— 两份实现迟早会在某个缓动上分家。
pub fn event_value(e: &Event, beat: f64) -> f64 {
    let (a, b) = (e.start.to_f64(), e.end.to_f64());
    let span = (b - a).max(1e-9);
    let t = ((beat - a) / span).clamp(0.0, 1.0);
    let (v0, v1) = (as_f64(&e.start_value), as_f64(&e.end_value));
    match (v0, v1) {
        (Some(v0), Some(v1)) => v0 + (v1 - v0) * ease(&e.easing, t),
        (Some(v0), None) => v0,
        (None, Some(v1)) => v1,
        (None, None) => 0.0,
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
    let (a, b) = (e.start.to_f64(), e.end.to_f64());
    let span = (b - a).max(1e-9);
    let t = ((beat - a) / span).clamp(0.0, 1.0);
    let (v0, v1) = (as_f64(&e.start_value), as_f64(&e.end_value));
    match (v0, v1) {
        (Some(v0), Some(v1)) => v0 + (v1 - v0) * t,
        (Some(v0), None) => v0,
        (None, Some(v1)) => v1,
        (None, None) => 0.0,
    }
}

/// 一条轨道在拍 `beat` 处的值 —— **"哪条轨道用哪种求值"的唯一判断处**。
///
/// 流速轨（`"speed"`）走 [`speed_value`]（只线性），其余四条走 [`eval_events`]（事件自己的缓动）。
/// 单独一个入口是为了不漏：树面板/检查器/时间轴/`lines` 各显示一个"此刻的值"，
/// 谁要是直接调 `eval_events`，同一时刻就会显示两个不同的流速。
pub fn track_value(track: &str, events: &[Event], beat: f64) -> Option<f64> {
    if track == "speed" {
        let e = events.iter().find(|e| beat >= e.start.to_f64() && beat <= e.end.to_f64())?;
        Some(speed_value(e, beat))
    } else {
        eval_events(events, beat)
    }
}

/// 空隙里"保持"的值：上一条已结束事件的终值（首条之前没有 ⇒ `None`）
fn held_value(events: &[Event], beat: f64) -> Option<f64> {
    let mut held: Option<f64> = None;
    for e in events {
        if e.end.to_f64() <= beat {
            held = as_f64(&e.end_value).or(held);
        }
    }
    held
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
    let idx = events.partition_point(|e| e.end.to_f64() <= b_from);
    let (_, _, acc) = integrate_until(events, tmap, idx, b_from, 0.0, tmap.beat(to_sec));
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

/// 位置 `at_beat`、下一条还没走完的事件是 `events[idx]` 时，**这一段**怎么求值。
///
/// 判断只此一份：整条走一遍（[`walk_speed`]）与"从检查点往前外推"（[`SpeedTable`]）都从它拿值，
/// 段怎么切才不会在两处慢慢分家。
fn seg_at(events: &[Event], idx: usize, at_beat: f64) -> SpeedSeg {
    match events.get(idx) {
        // 事件覆盖着当前位置：值走它的缓动，段尾就是它的终点
        Some(e) if e.start.to_f64() <= at_beat => SpeedSeg::Eased(idx),
        // 空隙（或在首条事件之前）：保持"上一条的终值"（首条之前取它的起始值）
        ev => SpeedSeg::Hold(
            held_value(&events[..idx], at_beat)
                .or_else(|| ev.and_then(|e| as_f64(&e.start_value)))
                .unwrap_or(SPEED_DEFAULT),
        ),
    }
}

/// 流速积分的**唯一走法**：从 `(idx, at_beat, acc)` 一路积到 `b_to`。
///
/// `acc` 的单位是"流速单位 × 秒"（换算成 RPE y 单位只在 [`speed_travel`] 与 [`SpeedTable::build`]
/// 两处乘 120）。每走到一段就回调一次 `on_seg(段起点拍, 段起点处的 acc, 这一段怎么求值)` ——
/// [`SpeedTable`] 靠它记检查点，其余调用方传一个空闭包。
///
/// 切点：**事件边界**（段的划分）与 **BPM 段起点**（闭式积分只对"秒域线性"成立）。
/// 段内积分见 [`integrate_seg`] —— 流速只按线性 ⇒ 那个式子是精确值，没有采样误差。
fn walk_speed(
    events: &[Event],
    tmap: &TimeMap,
    mut idx: usize,
    mut at_beat: f64,
    mut acc: f64,
    b_to: f64,
    mut on_seg: impl FnMut(f64, f64, SpeedSeg),
) -> (usize, f64, f64) {
    while at_beat < b_to {
        let ev = events.get(idx);
        let seg = seg_at(events, idx, at_beat);
        on_seg(at_beat, acc, seg);
        // 这一段到哪里为止：事件段到事件终点，空隙段到下一条事件的**起点**（没有下一条就到底）。
        // `consumed` 区分两种"走到 seg_end"：事件段走完才换下一条事件；
        // 空隙段走完时 `events[idx]` **还没被用过**（它正好从 `seg_end` 开始）⇒ idx 不许前进。
        //
        // 踩过的坑：早先两种情况一起 `idx += 1`，于是**空隙之后的那条事件被整条跳过** ——
        // 积分在空隙之后永远保持空隙前的值（谱面里只要有一个空隙，后面全错）。
        // 是 `tests/lines.rs` 里那条"独立基准对账"（直接积分 vs 前缀积分）把它抓出来的。
        let (seg_end, consumed) = match (ev, &seg) {
            (Some(e), SpeedSeg::Eased(_)) => (e.end.to_f64(), true),
            (Some(e), _) => (e.start.to_f64(), false),
            (None, _) => (b_to, false),
        };
        // 这一段到哪里为止：段尾 / `b_to` / **下一个 BPM 段的起点**，取最近的那个。
        // BPM 变化处必须切开（闭式积分只对"秒域线性"成立，见 `TimeMap::next_seg_start`）。
        // `max(at_beat)`：病态输入（后一条事件整条包在前一条里）下 `seg_end` 会落在身后，
        // 夹一下保证这一步**永远向前**（走法单调 ⇒ 不会原地打转，也不会重复积分）
        let bpm_cut = tmap.next_seg_start(at_beat).unwrap_or(f64::INFINITY);
        let prev = at_beat;
        let stop = seg_end.min(b_to).min(bpm_cut).max(at_beat);
        acc += integrate_seg(tmap, events, &seg, at_beat, stop);
        at_beat = stop;
        if stop >= b_to - 1e-12 {
            break; // 到目标了
        }
        if stop >= seg_end - 1e-12 {
            if consumed {
                idx += 1; // 事件段走完 ⇒ 换下一条
                continue;
            }
            if ev.is_none() {
                break; // 末尾之后：`held` 一直保持，只有 `b_to` 能叫停
            }
            // 空隙走完（`stop == ev.start`）：**idx 不动**，继续下一轮 ——
            // 那时 `seg_at` 看到 `start <= at_beat`，这条事件就成了当前段
            // （这里必须 `continue`：`stop` 与 `at_beat` 此刻正好相等，落到下面那道
            //  "没前进就收手"的守卫上会把走法**停在这里**，于是表里缺了这一段之后的切点）
            continue;
        }
        if stop <= prev {
            break; // **相对上一轮**没前进（零长段 / 病态输入）⇒ 收手，别打转
        }
    }
    (idx, at_beat, acc)
}

/// 同 [`walk_speed`]，但不记检查点（返回新的 `(idx, at_beat, acc)`）
fn integrate_until(
    events: &[Event],
    tmap: &TimeMap,
    idx: usize,
    at_beat: f64,
    acc: f64,
    b_to: f64,
) -> (usize, f64, f64) {
    walk_speed(events, tmap, idx, at_beat, acc, b_to, |_, _, _| {})
}

/// 在**一段之内**从 `b_from` 积到 `b_to`，遇到 BPM 段起点就切开。
///
/// 闭式积分只对"流速在**秒域**上线性"成立，而 BPM 变化处拍↔秒折了一下 ——
/// 查表路径（[`SpeedTable::h_at_hinted`]）的"段内余下那一小截"也必须按 BPM 切开，
/// 否则表与"从 0 整条走一遍"就会在 BPM 变化点上分家（实测：差 24 单位）。
fn integrate_span(tmap: &TimeMap, events: &[Event], seg: &SpeedSeg, b_from: f64, b_to: f64) -> f64 {
    let mut acc = 0.0;
    let mut from = b_from;
    while from < b_to {
        let stop = tmap.next_seg_start(from).map_or(b_to, |c| c.min(b_to));
        acc += integrate_seg(tmap, events, seg, from, stop);
        from = stop;
    }
    acc
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
        let at0 = tmap.beat(0.0);
        let mut cuts: Vec<(f64, f64, SpeedSeg)> = Vec::new();
        walk_speed(events, tmap, 0, at0, 0.0, b_end, |beat, acc, seg| {
            cuts.push((beat, acc * SPEED_UNITS_PER_SEC, seg));
        });
        if cuts.is_empty() {
            // 谱面长度为 0 的退化情形（走不进循环）：补第一段，照样能外推 —— 别 panic
            cuts.push((at0, 0.0, seg_at(events, 0, at0)));
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
            // 退回直接积分。两条路径的值相同（共用走法与段内积分），只是慢。
            return (speed_travel(events, tmap, 0.0, sec), 0);
        };
        (h + integrate_span(tmap, events, &seg, beat, b) * SPEED_UNITS_PER_SEC, i)
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

/// 求某条判定线在**秒** t 处的表演状态。
///
/// `tracks` 是预先按 `[moveX, moveY, rotate, alpha, speed]` 取好的事件表 ——
/// 让调用方可以缓存（每帧对每条线求值 5 次二分，代价可忽略）。
pub fn perf_at(tracks: &[Vec<Event>; 5], tmap: &TimeMap, sec: f64) -> LinePerf {
    let beat = tmap.beat(sec);
    let mut p = LinePerf::default();
    if let Some(v) = eval_events(&tracks[0], beat) {
        p.x = v as f32;
    }
    if let Some(v) = eval_events(&tracks[1], beat) {
        p.y = v as f32;
    }
    if let Some(v) = eval_events(&tracks[2], beat) {
        p.rotate_deg = v as f32;
    }
    if let Some(v) = eval_events(&tracks[3], beat) {
        p.alpha = (v as f32).clamp(0.0, 1.0);
    }
    if let Some(v) = eval_events(&tracks[4], beat) {
        p.speed = v as f32;
    }
    p
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
    let n = per_event.max(1);
    for e in events {
        let (a, b) = (e.start.to_f64(), e.end.to_f64());
        for k in 0..=n {
            let t = k as f64 / n as f64;
            let beat = a + (b - a) * t;
            // `linear_only` = 流速轨：曲线要与求值**同一条口径**（只用线性），
            // 否则时间轴画的是缓动、音符位置却是线性积分 —— 两个面板互相打脸
            let v = if linear_only {
                speed_value(e, beat)
            } else {
                eval_events(std::slice::from_ref(e), beat).unwrap_or(0.0)
            };
            out.push([tmap.sec(beat) as f32, v as f32]);
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

