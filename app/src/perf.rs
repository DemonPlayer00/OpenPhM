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
            let span = (b - a).max(1e-9);
            let t = ((beat - a) / span).clamp(0.0, 1.0);
            let (v0, v1) = (as_f64(&e.start_value), as_f64(&e.end_value));
            return Some(match (v0, v1) {
                (Some(v0), Some(v1)) => v0 + (v1 - v0) * ease(&e.easing, t),
                (Some(v0), None) => v0,
                (None, Some(v1)) => v1,
                (None, None) => 0.0,
            });
        }
    }
    // 不在任何事件里 —— **保持**最近一条事件的值，而不是"跳到最后一个事件"。
    //
    // 这里修的是一个会让判定线自己动的 bug：事件之间留了空隙时（比如 [0,10] 与 [20,30]），
    // 拍 15 之前会走到下面 `beat >= events[0].start` 的分支，返回**最后一条事件的终值** ——
    // 于是线在空隙里直接跳到终值，正是用户说的"某时间没有事件却移动了判定线"。
    // 正确语义是"保持"：空隙里维持**前一条事件的终值**（首个事件之前则取它的起始值）。
    let mut held: Option<f64> = None;
    for e in events {
        if e.end.to_f64() <= beat {
            held = as_f64(&e.end_value).or(held);
        }
    }
    if let Some(v) = held {
        return Some(v);
    }
    as_f64(&events[0].start_value).or(Some(0.0))
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
    /// 线的旋转角（弧度），给实例用
    pub fn rotate_rad(&self) -> f32 {
        self.rotate_deg.to_radians()
    }
}

/// 从判定线取出某条轨道的全部事件（图层 0 优先；多图层时合并为一条时间线）
pub fn track_events(line: &JudgeLine, track: &str) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    for layer in &line.layers {
        if let Some(list) = layer.track(track) {
            out.extend(list.iter().cloned());
        }
    }
    out.sort_by(|a, b| a.start.cmp(&b.start));
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

/// 把一条轨道采样成折线（供时间轴画曲线）：每个事件按缓动取 `per_event+1` 个点。
///
/// 线性缓动其实只需两端点，但 29 个缓动里有非线性/回弹（elastic/bounce），
/// 少采样会让曲线形状骗人 —— 显示用 N 点采样，求值仍走 [`eval_events`] 的精确公式。
pub fn sample_track(events: &[Event], tmap: &TimeMap, per_event: usize) -> Vec<[f32; 2]> {
    let mut out: Vec<[f32; 2]> = Vec::new();
    let n = per_event.max(1);
    for e in events {
        let (a, b) = (e.start.to_f64(), e.end.to_f64());
        for k in 0..=n {
            let t = k as f64 / n as f64;
            let beat = a + (b - a) * t;
            let v = eval_events(std::slice::from_ref(e), beat).unwrap_or(0.0);
            out.push([tmap.sec(beat) as f32, v as f32]);
        }
    }
    out
}
