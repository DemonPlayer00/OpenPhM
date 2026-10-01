// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! opm 文档模型（对齐 `spec/opm-format.md` v0.1）。
//!
//! 三条来自规范的硬约束在这里落地：
//! 1. **拍用精确有理数**（`{n, d}`），不用浮点 —— 否则"导入→导出"会数值漂移；
//! 2. **枚举用字符串**（`"tap"` / `"outQuad"`），整数只存在于 codec 边界；
//! 3. **未知字段必须保留** —— 每个对象带 `foreign` 袋，序列化时原样写回。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// 精确有理拍。构造时自动约分，`d` 恒为正。
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Beat {
    pub n: i64,
    pub d: i64,
}

impl Beat {
    pub fn new(n: i64, d: i64) -> Self {
        let (n, d) = if d == 0 { (n, 1) } else { (n, d) };
        let g = gcd(n.unsigned_abs() as i64, d.unsigned_abs() as i64).max(1);
        let (mut n, mut d) = (n / g, d / g);
        if d < 0 {
            n = -n;
            d = -d;
        }
        Beat { n, d }
    }
    pub fn zero() -> Self {
        Beat { n: 0, d: 1 }
    }
    pub fn to_f64(self) -> f64 {
        self.n as f64 / self.d as f64
    }
    /// 以另一拍为原点的偏移
    pub fn sub(self, other: Beat) -> f64 {
        self.to_f64() - other.to_f64()
    }
}

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

impl PartialEq for Beat {
    /// **精确相等**（交叉相乘，走 `i128`）—— 不经过 f64。
    ///
    /// 以前是 `self.to_f64() == other.to_f64()`。那是"看起来相等"： numerator 超过 2^53 的两个
    /// 不同整数在 f64 里会撞成一个数（`9007199254740993/1` 与 `9007199254740992/1`），
    /// 于是判定时刻的相等/排序、去重、重叠检测都会把它们当成同一个时刻。
    /// 交叉相乘还顺带容忍**没约分**的输入（`2/6 == 1/3`，`Beat` 经 `serde` 反序列化时可能带进来）。
    ///
    /// 实测真实谱面离撞车还差 ~11 个数量级（同一条线上相邻判定时刻最小间距 7.54e-4 拍，
    /// 该量级的 f64 分辨率 1.2e-14）—— 仍然改成精确的：这条链上"几乎不会错"没有意义。
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for Beat {}
impl PartialOrd for Beat {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Beat {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // i128：`n`/`d` 各自到 i64 上限时乘积也不会溢出
        (self.n as i128 * other.d as i128).cmp(&(other.n as i128 * self.d as i128))
    }
}

/// 扩展字段袋：按来源格式名保存无法识别的原始字段
pub type Foreign = BTreeMap<String, Value>;

fn foreign_from(obj: &Map<String, Value>, known: &[&str]) -> Foreign {
    obj.iter()
        .filter(|(k, _)| !known.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn merge_foreign(obj: &mut Map<String, Value>, foreign: &Foreign) {
    for (k, v) in foreign {
        obj.entry(k.clone()).or_insert_with(|| v.clone());
    }
}

// ---------------------------------------------------------------- 事件

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    #[serde(rename = "startBeat")]
    pub start: Beat,
    #[serde(rename = "endBeat")]
    pub end: Beat,
    #[serde(rename = "startValue")]
    pub start_value: Value,
    #[serde(rename = "endValue")]
    pub end_value: Value,
    #[serde(default = "default_easing")]
    pub easing: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bezier: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bezier_points: Option<[f32; 4]>,
    #[serde(flatten)]
    pub foreign: Foreign,
}

fn default_easing() -> String {
    "linear".to_owned()
}

impl Event {
    pub fn new(start: Beat, end: Beat, from: Value, to: Value, easing: &str) -> Self {
        Self {
            start,
            end,
            start_value: from,
            end_value: to,
            easing: easing.to_owned(),
            bezier: false,
            bezier_points: None,
            foreign: Foreign::new(),
        }
    }
}

/// 一条事件在**文档里的出处**：第几个图层 + 该图层事件表里的下标。
///
/// 为什么必须有它：视图把一条线的**五个图层**合并成一条时间线（求值要的就是这个），
/// 可合并之后的序号与任何单独图层里的序号都不是一回事 —— 拿合并序号去发 `del_event`
/// 或 `set_event`，在多层文档上删掉/改动的会是**另一条事件**。
/// 编辑（删、移）一律按这个地址走；只读的求值仍然用合并后的顺序。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventRef {
    pub layer: usize,
    pub index: usize,
}

impl EventRef {
    pub fn new(layer: usize, index: usize) -> Self {
        Self { layer, index }
    }
}

// ---------------------------------------------------------------- 判定线

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Layer {
    #[serde(rename = "moveX", default)]
    pub move_x: Vec<Event>,
    #[serde(rename = "moveY", default)]
    pub move_y: Vec<Event>,
    #[serde(default)]
    pub rotate: Vec<Event>,
    #[serde(default)]
    pub alpha: Vec<Event>,
    #[serde(default)]
    pub speed: Vec<Event>,
    #[serde(flatten)]
    pub foreign: Foreign,
}

pub const TRACKS: [&str; 5] = ["moveX", "moveY", "rotate", "alpha", "speed"];

impl Layer {
    pub fn track(&self, name: &str) -> Option<&Vec<Event>> {
        match name {
            "moveX" => Some(&self.move_x),
            "moveY" => Some(&self.move_y),
            "rotate" => Some(&self.rotate),
            "alpha" => Some(&self.alpha),
            "speed" => Some(&self.speed),
            _ => None,
        }
    }
    pub fn track_mut(&mut self, name: &str) -> Option<&mut Vec<Event>> {
        match name {
            "moveX" => Some(&mut self.move_x),
            "moveY" => Some(&mut self.move_y),
            "rotate" => Some(&mut self.rotate),
            "alpha" => Some(&mut self.alpha),
            "speed" => Some(&mut self.speed),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteKind {
    Tap,
    Hold,
    Drag,
    Flick,
}

impl NoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NoteKind::Tap => "tap",
            NoteKind::Hold => "hold",
            NoteKind::Drag => "drag",
            NoteKind::Flick => "flick",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tap" => Some(NoteKind::Tap),
            "hold" => Some(NoteKind::Hold),
            "drag" => Some(NoteKind::Drag),
            "flick" => Some(NoteKind::Flick),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Note {
    pub kind: NoteKind,
    #[serde(rename = "startBeat")]
    pub start: Beat,
    #[serde(rename = "endBeat", default, skip_serializing_if = "Option::is_none")]
    pub end: Option<Beat>,
    #[serde(rename = "laneX")]
    pub lane_x: f32,
    #[serde(default = "default_side")]
    pub side: String,
    #[serde(rename = "isFake", default, skip_serializing_if = "std::ops::Not::not")]
    pub is_fake: bool,
    #[serde(default = "default_alpha")]
    pub alpha: u16,
    #[serde(default = "default_speed")]
    pub speed: f32,
    #[serde(rename = "widthScale", default = "default_speed")]
    pub width_scale: f32,
    #[serde(rename = "yOffset", default)]
    pub y_offset: f32,
    #[serde(rename = "judgeAreaScale", default = "default_speed")]
    pub judge_area_scale: f32,
    #[serde(flatten)]
    pub foreign: Foreign,
}

fn default_side() -> String {
    "above".to_owned()
}
fn default_alpha() -> u16 {
    255
}
fn default_speed() -> f32 {
    1.0
}

impl Note {
    pub fn new(kind: NoteKind, start: Beat, lane_x: f32) -> Self {
        Self {
            kind,
            start,
            end: None,
            lane_x,
            side: default_side(),
            is_fake: false,
            alpha: 255,
            speed: 1.0,
            width_scale: 1.0,
            y_offset: 0.0,
            judge_area_scale: 1.0,
            foreign: Foreign::new(),
        }
    }
    pub fn end_beat(&self) -> Beat {
        self.end.unwrap_or(self.start)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JudgeLine {
    #[serde(default = "default_line_name")]
    pub name: String,
    #[serde(rename = "bpmFactor", default = "default_speed")]
    pub bpm_factor: f32,
    #[serde(rename = "zOrder", default)]
    pub z_order: i32,
    #[serde(rename = "isCover", default = "default_true")]
    pub is_cover: bool,
    #[serde(default)]
    pub layers: Vec<Layer>,
    #[serde(default)]
    pub notes: Vec<Note>,
    #[serde(flatten)]
    pub foreign: Foreign,
}

fn default_line_name() -> String {
    "Untitled".to_owned()
}
fn default_true() -> bool {
    true
}

impl Default for JudgeLine {
    fn default() -> Self {
        Self {
            name: default_line_name(),
            bpm_factor: 1.0,
            z_order: 0,
            is_cover: true,
            layers: vec![Layer::default()],
            notes: Vec::new(),
            foreign: Foreign::new(),
        }
    }
}

impl JudgeLine {
    pub fn notes_sorted(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.notes.len()).collect();
        idx.sort_by(|&a, &b| self.notes[a].start.cmp(&self.notes[b].start));
        idx
    }
}

// ---------------------------------------------------------------- 元信息与根

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub name: String,
    #[serde(default)]
    pub composer: String,
    #[serde(default)]
    pub charter: String,
    #[serde(default)]
    pub illustrator: String,
    #[serde(default = "default_difficulty")]
    pub difficulty: String,
    #[serde(default)]
    pub level: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constant: Option<f32>,
    #[serde(rename = "offsetMs", default)]
    pub offset_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(flatten)]
    pub foreign: Foreign,
}

fn default_difficulty() -> String {
    "IN".to_owned()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BpmEntry {
    #[serde(rename = "startBeat")]
    pub start: Beat,
    pub bpm: f32,
    #[serde(flatten)]
    pub foreign: Foreign,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Document {
    pub format: String,
    #[serde(rename = "formatVersion")]
    pub format_version: u32,
    #[serde(rename = "minClientCapability")]
    pub min_client_capability: u8,
    #[serde(default)]
    pub extensions: Vec<String>,
    pub meta: Meta,
    #[serde(rename = "bpmList")]
    pub bpm_list: Vec<BpmEntry>,
    #[serde(rename = "judgeLines")]
    pub judge_lines: Vec<JudgeLine>,
    #[serde(flatten)]
    pub foreign: Foreign,
}

impl Document {
    /// 一份**新建**的空谱面：给定曲名/作者/音频/基础 BPM，一条默认判定线，一条 BPM 条目。
    ///
    /// 为什么在库里而不是 GUI 里拼：CLI（`opm-ctl new`）、GUI"新建"、测试要的是同一份初始形态
    /// （bpmList 必须从拍 0 起、至少一条判定线），这种"初始不变量"只能有一处实现。
    pub fn fresh(meta: Meta, bpm: f32) -> Self {
        let mut doc = Document::default();
        doc.meta = meta;
        doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm, foreign: Foreign::new() }];
        doc.judge_lines = vec![JudgeLine::default()];
        doc
    }
}

impl Default for Document {
    fn default() -> Self {
        Self {
            format: "opm".to_owned(),
            format_version: 1,
            min_client_capability: 1,
            extensions: Vec::new(),
            meta: Meta {
                name: "untitled".to_owned(),
                composer: String::new(),
                charter: String::new(),
                illustrator: String::new(),
                difficulty: default_difficulty(),
                level: String::new(),
                constant: None,
                offset_ms: 0,
                audio: None,
                background: None,
                foreign: Foreign::new(),
            },
            bpm_list: vec![BpmEntry {
                start: Beat::zero(),
                bpm: 180.0,
                foreign: Foreign::new(),
            }],
            judge_lines: vec![JudgeLine::default()],
            foreign: Foreign::new(),
        }
    }
}

impl Document {
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        // 回填 foreign 袋（serde 的 flatten 只负责读，写回需显式合并避免覆盖已知字段）
        if let Value::Object(root) = &mut v {
            merge_foreign(root, &self.foreign);
        }
        v
    }

    pub fn from_json(v: Value) -> Result<Self, String> {
        let obj = v.as_object().ok_or("根必须是对象")?;
        if obj.get("format").and_then(|f| f.as_str()) != Some("opm") {
            return Err(format!(
                "format 必须是 \"opm\"（当前 {:?}）",
                obj.get("format")
            ));
        }
        let mut doc: Document =
            serde_json::from_value(v.clone()).map_err(|e| format!("解析失败: {e}"))?;
        // 手工收 foreign（serde flatten 对嵌套对象的行为不完全可靠）
        let known = [
            "format",
            "formatVersion",
            "minClientCapability",
            "extensions",
            "meta",
            "bpmList",
            "judgeLines",
        ];
        doc.foreign = foreign_from(obj, &known);
        Ok(doc)
    }

    /// 谱面末尾（所有音符结束拍的最大值）
    /// 谱面末尾：音符与事件取最大。
    ///
    /// 事件必须计入 —— 判定线的表演（移动/旋转/透明度）常常比最后一个音符更长，
    /// 只按音符算会让"只有事件、还没放音符"的谱面时长为 0（本轮事件成为一等对象后补上）。
    pub fn chart_end(&self) -> Beat {
        let mut end = Beat::zero();
        for line in &self.judge_lines {
            for n in &line.notes {
                let e = n.end_beat();
                if e > end {
                    end = e;
                }
            }
            for layer in &line.layers {
                for track in TRACKS {
                    if let Some(list) = layer.track(track) {
                        for ev in list {
                            if ev.end > end {
                                end = ev.end;
                            }
                        }
                    }
                }
            }
        }
        end
    }

    pub fn note_count(&self) -> usize {
        self.judge_lines.iter().map(|l| l.notes.len()).sum()
    }

    /// 汇总（供 agent 快速读状态，不必 dump 全文）
    pub fn summary(&self) -> Value {
        serde_json::json!({
            "name": self.meta.name,
            "judgeLines": self.judge_lines.len(),
            "notes": self.note_count(),
            "chartEnd": self.chart_end(),
            "bpmList": self.bpm_list.len(),
            "capability": self.min_client_capability,
            "extensions": self.extensions,
            "eventCounts": self.judge_lines.iter().enumerate().map(|(i, l)| {
                let layer = l.layers.first();
                serde_json::json!({
                    "line": i,
                    "moveX": layer.map(|x| x.move_x.len()).unwrap_or(0),
                    "moveY": layer.map(|x| x.move_y.len()).unwrap_or(0),
                    "rotate": layer.map(|x| x.rotate.len()).unwrap_or(0),
                    "alpha": layer.map(|x| x.alpha.len()).unwrap_or(0),
                    "speed": layer.map(|x| x.speed.len()).unwrap_or(0),
                    "notes": l.notes.len(),
                })
            }).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod beat_tests {
    use super::Beat;

    /// **拍的相等/排序是精确的**（交叉相乘），不经过 f64。
    ///
    /// 用户口径（2026-10-01）：判定时刻的比较要逐位精确。以前 `PartialEq`/`Ord` 都走
    /// `to_f64()`，于是两个不同的大分子会在 f64 里撞成一个数 —— 去重、排序、重叠检测
    /// 都会把它们当成同一个时刻。
    #[test]
    fn beat_equality_and_order_are_exact() {
        // 没约分的输入也认（`serde` 反序列化可能带进来 `{"n":2,"d":6}`）
        assert_eq!(Beat { n: 2, d: 6 }, Beat { n: 1, d: 3 });
        assert_eq!(Beat { n: -2, d: -6 }, Beat { n: 1, d: 3 });
        // 超过 2^53 的相邻整数：f64 下相等（都是 9007199254740992.0），精确比较必须不等
        let a = Beat { n: 9_007_199_254_740_993, d: 1 };
        let b = Beat { n: 9_007_199_254_740_992, d: 1 };
        assert_eq!(a.to_f64(), b.to_f64(), "这条的前提：f64 下它们确实撞成一个数");
        assert_ne!(a, b);
        assert!(a > b);
        // 排序：负数、分数、同值不同写法
        assert!(Beat::new(-1, 3) < Beat::zero());
        assert!(Beat::new(1, 3) < Beat::new(1, 2));
        assert_eq!(
            Beat::new(1, 3).cmp(&Beat::new(2, 6)),
            std::cmp::Ordering::Equal,
            "2/6 与 1/3 是同一个拍"
        );
        // 大分母也不能撞：1/(10^9) 与 1/(10^9+1) 在 f64 里是两个数，但精确比较更要分得开
        assert!(Beat::new(1, 1_000_000_000) > Beat::new(1, 1_000_000_001));
        // 交叉相乘不溢出：两边都取 i64 上限
        assert!(Beat { n: i64::MAX, d: 1 } > Beat { n: i64::MAX - 1, d: 1 });
    }
}
