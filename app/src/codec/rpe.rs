// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! RPE（Re:PhiEdit / Phira）谱面编解码 —— **自研实现，不链接 `prpr`**（`prpr` 是 GPL-3.0，见提案 D1）。
//!
//! 字段语义来源：`Phigros-规则速查.md` §4（RPE 与 PEC 格式）与 `spec/opm-format.md` §9（与 RPE 的映射），
//! 这两份都是从 Phira Documents（CC-BY-4.0，署名）与 Lchzh Docs 核对过的。
//!
//! 导入侧必须处理（规范 §9 明列）：
//! - 四套音符类型枚举互不相同（RPE 的 2 是 Hold、3 是 Flick）；
//! - `META.RPEVersion` **不可信**（1.5.0~1.6.0 恒写 150，1.6.1 恒写 160）——只作记录；
//! - `alpha` 越界不截断（RPE 实测存在 >255；负数是"连音符一起隐藏"的废弃功能）；
//! - 层为空时字段存在性随版本变（早期 `null`、143 起无字段、全空则 `eventLayers` 不出现）；
//! - `bpmFactor` 是**除**不是乘；
//! - 事件轨道的补空隙（前值延拓）与裁重叠。
//!
//! 导出侧：目标版本档位可切换，且**必须产出保真度报告**。

use serde_json::{json, Map, Value};

use super::{
    beat_from_value, beat_to_triple, note_kind_from_rpe, note_kind_to_rpe, normalize_track,
    rpe_id_of_easing, easing_name_of_rpe, Fidelity,
};
use crate::doc::{Beat, BpmEntry, Document, Event, Foreign, JudgeLine, Layer, Meta, Note};

/// 导入结果
pub struct RpeImport {
    pub doc: Document,
    pub fidelity: Fidelity,
}

/// 导出目标档位（RPE 版本档位可切换 —— 规范 §9 要求）
#[derive(Clone, Copy, Debug)]
pub struct RpeTarget {
    /// 写进 `META.RPEVersion` 的值：150 / 160 是实际见过的两档
    pub version: i64,
    /// 事件时间是否写成三元组 `[b0,b1,b2]`（RPE 规范写法）。
    /// 关掉时写浮点拍 —— 只给"某些工具只吃浮点"的场合用，默认开。
    pub triple_time: bool,
    /// 音符时间是否也写成三元组。**默认开**，依据是实测：
    /// 从 PhiZone 抓的 3 份真实谱面（RPEVersion 113/141）里 **2591 个音符的 startTime/endTime
    /// 全部是整数数组**（`[33,0,1]`、`[7,1,4]`），BPMList 与事件同样；浮点拍是 RPE 1.5+ 的写法。
    /// 而且三元组是**精确**的：真实谱面里分母出现 3/6/20/24，走浮点回不去（37+1/3 → 37333333/1000000）。
    /// 关掉它只给"只吃浮点"的工具用。
    pub note_triple_time: bool,
}

impl Default for RpeTarget {
    fn default() -> Self {
        Self { version: 160, triple_time: true, note_triple_time: true }
    }
}

// ---------------------------------------------------------------- 小工具

fn get<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a Value> {
    o.get(k)
}
fn f64_of(v: Option<&Value>, d: f64) -> f64 {
    v.and_then(|x| x.as_f64()).unwrap_or(d)
}
fn i64_of(v: Option<&Value>, d: i64) -> i64 {
    v.and_then(|x| x.as_i64()).unwrap_or(d)
}
fn str_of(v: Option<&Value>, d: &str) -> String {
    v.and_then(|x| x.as_str()).unwrap_or(d).to_owned()
}
/// 宽容的布尔读取：RPE 各版本里同一字段出现过 `true`/`false`、`1`/`0`、`"true"`。
/// （测试里那条 `"bezier": 1` 就是真会碰到的写法 —— 只认 `as_bool()` 会把它读成 false 而不报错。）
fn truthy(v: Option<&Value>, d: bool) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(d),
        Some(Value::String(s)) => match s.as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => d,
        },
        _ => d,
    }
}

/// 拍转浮点（给只能用浮点的字段）
fn bf(b: Beat) -> f64 {
    b.to_f64()
}

/// 归一化负零：`-0.0` 与 `0.0` 在 IEEE 下相等，但 `dump`/diff/比较时会显出差别。
/// 真实谱面（Belle de Nuit，RPEVersion 141）里就写了 `-0.0`，往返一趟变成 `0.0` ——
/// 与其在比较里容忍，不如导入时统一，省得以后每次对拍都要解释"这不是数据丢失"。
fn norm0(x: f64) -> f64 {
    if x == 0.0 {
        0.0
    } else {
        x
    }
}

/// 数值写回 JSON：整数就写成整数。
/// RPE 的实际数据里 `"start": -100` 比 `-100.0` 常见，导出保持整数形状能让 diff 干净得多
/// （播放器两者都吃，但人读 diff 时会骂人）。
fn num_value(x: f64) -> Value {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

// ---------------------------------------------------------------- 导入

/// 从 RPE 文本导入
pub fn load_str(text: &str) -> Result<RpeImport, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("JSON 解析失败: {e}"))?;
    load_value(v)
}

/// 从 RPE JSON 导入（`judgeLineList` / `BPMList` / `META`）
pub fn load_value(v: Value) -> Result<RpeImport, String> {
    let root = v.as_object().ok_or("RPE 谱面根节点不是对象")?;
    if get(root, "judgeLineList").is_none() {
        return Err("不是 RPE 谱面：缺少 `judgeLineList`".to_owned());
    }
    let version = get(root, "META")
        .and_then(|m| m.as_object())
        .map(|m| i64_of(get(m, "RPEVersion"), 0))
        .unwrap_or(0);
    let mut fid = Fidelity::new("rpe", format!("RPEVersion={version}（此值不可信，仅作记录）"));

    // ---- BPM 表 ----
    let bpm_list = import_bpm_list(root, &mut fid)?;

    // ---- META ----
    let meta = import_meta(root, &mut fid);

    // ---- 判定线 ----
    let lines_v = get(root, "judgeLineList").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    // 谱面结束拍：所有音符/事件的最大值（末事件要延拓到这里）
    let mut chart_end = Beat::zero();
    let mut judge_lines = Vec::with_capacity(lines_v.len());
    for (li, lv) in lines_v.iter().enumerate() {
        let (line, end) = import_line(lv, li, &mut fid)?;
        chart_end = chart_end.max(end);
        judge_lines.push(line);
    }
    if chart_end <= Beat::zero() {
        chart_end = Beat::new(1, 1); // 空谱面也给 1 拍，免得轨道无条件延拓到 0
    }

    // ---- 事件轨道规范化（补空隙 / 裁重叠 / 首事件从 0 起 / 末事件延拓）----
    //
    // 规范化要问求值器"切点上的值"（裁重叠那一步），而求值要按**秒**长把缓动采样成折线
    // ⇒ 必须先有拍↔秒映射。BPM 表在上面已经读完、谱面末尾拍刚算出来。
    let tmap = crate::perf::TimeMap::from_parts(&bpm_list, chart_end);
    for (li, line) in judge_lines.iter_mut().enumerate() {
        for (gi, layer) in line.layers.iter_mut().enumerate() {
            for track in crate::doc::TRACKS {
                let list = match layer.track_mut(track) {
                    Some(l) => std::mem::take(l),
                    None => continue,
                };
                if list.is_empty() {
                    continue;
                }
                let ptr = format!("/judgeLineList[{li}].eventLayers[{gi}].{track}Events");
                let (norm, _) = normalize_track(list, &tmap, chart_end, &ptr, &mut fid);
                if let Some(slot) = layer.track_mut(track) {
                    *slot = norm;
                }
            }
        }
    }

    // ---- 根上的编辑器辅助字段：保留（不建模，但别丢）----
    let mut foreign = Foreign::new();
    for k in ["chartTime", "judgeLineGroup", "multiLineString", "multiScale", "timeTags", "xybind"] {
        if let Some(val) = get(root, k) {
            foreign.insert(k.to_owned(), val.clone());
        }
        // 早期版本会把层写成 null：这属于"编辑器辅助"之外的结构差异，单独报告
    }
    if get(root, "judgeLineList")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().any(|l| get(l.as_object().unwrap_or(&Map::new()), "eventLayers").is_none()))
        .unwrap_or(false)
    {
        fid.note("部分判定线没有 `eventLayers` 字段（143 起所有层为空时该字段不出现）—— 按空层处理");
    }
    if foreign.contains_key("chartTime") {
        fid.note("保留 `chartTime`（编辑时长，播放器不需要）");
    }
    if foreign.contains_key("judgeLineGroup") {
        fid.note("保留 `judgeLineGroup`（判定线组，模拟器不需要）");
    }

    let doc = Document {
        format: "opm".to_owned(),
        format_version: 1,
        min_client_capability: 1,
        extensions: Vec::new(),
        meta,
        bpm_list,
        judge_lines,
        foreign,
    };
    // 能力等级按内容推导：出现了 RPE 1.7 才有的字段（tint/judgeArea）就得抬到 2
    let mut doc = doc;
    doc.min_client_capability = capability_of(&doc);
    fid.finalize();
    Ok(RpeImport { doc, fidelity: fid })
}

/// 按内容推导 `minClientCapability`（`spec/opm-format.md` §7）
fn capability_of(doc: &Document) -> u8 {
    let mut cap = 1u8; // 有判定线事件/音符基础字段就是 rpe-base
    for line in &doc.judge_lines {
        for n in &line.notes {
            if n.judge_area_scale != 1.0
                || n.foreign.contains_key("visibleTime")
                || n.foreign.contains_key("tint")
                || n.foreign.contains_key("hitEffectTint")
            {
                cap = cap.max(2);
            }
        }
        if line.foreign.contains_key("extended") || line.foreign.contains_key("controls") {
            cap = cap.max(2);
        }
    }
    cap
}

fn import_bpm_list(root: &Map<String, Value>, fid: &mut Fidelity) -> Result<Vec<BpmEntry>, String> {
    let arr = get(root, "BPMList")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<BpmEntry> = Vec::new();
    if arr.is_empty() {
        fid.warn("`BPMList` 缺失或为空 —— 按 120 BPM 从拍 0 起补一条（否则时间无法换算）");
        out.push(BpmEntry { start: Beat::zero(), bpm: 120.0, foreign: Foreign::new() });
        return Ok(out);
    }
    for (i, e) in arr.iter().enumerate() {
        let o = e.as_object().ok_or_else(|| format!("BPMList[{i}] 不是对象"))?;
        let bpm = f64_of(get(o, "bpm"), 120.0) as f32;
        let start = match get(o, "startTime") {
            Some(v) => beat_from_value(v).map_err(|e| format!("BPMList[{i}].startTime: {e}"))?,
            None => Beat::zero(),
        };
        if bpm <= 0.0 {
            fid.warn(format!("BPMList[{i}].bpm = {bpm} ≤ 0（opm 要求正数）—— 按 120 处理"));
        }
        let mut foreign = Foreign::new();
        for (k, v) in o {
            if k != "bpm" && k != "startTime" {
                foreign.insert(k.clone(), v.clone());
            }
        }
        out.push(BpmEntry { start, bpm: if bpm > 0.0 { bpm } else { 120.0 }, foreign });
    }
    out.sort_by(|a, b| a.start.cmp(&b.start));
    if out[0].start > Beat::zero() {
        // opm 错误规则 2：首元素 startBeat 必须为 0
        let first = out[0].clone();
        fid.warn(format!(
            "BPMList 首项从拍 {} 起（opm 要求拍 0）—— 已在拍 0 补一条同 BPM 条目",
            first.start.to_f64()
        ));
        out.insert(0, BpmEntry { start: Beat::zero(), ..first });
    }
    // 严格递增：重复起点只留最后一条
    let mut dedup: Vec<BpmEntry> = Vec::with_capacity(out.len());
    for e in out {
        if let Some(last) = dedup.last_mut() {
            if last.start == e.start {
                fid.warn(format!("BPMList 在拍 {} 有重复条目 —— 保留后一条", e.start.to_f64()));
                *last = e;
                continue;
            }
        }
        dedup.push(e);
    }
    Ok(dedup)
}

fn import_meta(root: &Map<String, Value>, fid: &mut Fidelity) -> Meta {
    let m = get(root, "META").and_then(|x| x.as_object());
    let mut foreign = Foreign::new();
    if let Some(m) = m {
        for (k, v) in m {
            if !matches!(
                k.as_str(),
                "offset" | "name" | "song" | "background" | "composer" | "charter" | "illustration" | "level"
            ) {
                foreign.insert(k.clone(), v.clone());
            }
        }
    } else {
        fid.warn("缺少 `META` —— 元信息按空值处理");
    }
    let offset_ms = i64_of(m.and_then(|m| get(m, "offset")), 0);
    if m.is_some() {
        fid.note(format!(
            "`META.offset` = {offset_ms} ms → `meta.offsetMs`（RPE 与 opm 同为毫秒，符号语义一致）"
        ));
    }
    let audio = m.and_then(|m| get(m, "song")).and_then(|v| v.as_str()).map(str::to_owned);
    let background = m.and_then(|m| get(m, "background")).and_then(|v| v.as_str()).map(str::to_owned);
    Meta {
        name: str_of(m.and_then(|m| get(m, "name")), "untitled"),
        composer: str_of(m.and_then(|m| get(m, "composer")), ""),
        charter: str_of(m.and_then(|m| get(m, "charter")), ""),
        illustrator: str_of(m.and_then(|m| get(m, "illustration")), ""),
        difficulty: "IN".to_owned(),
        level: str_of(m.and_then(|m| get(m, "level")), ""),
        constant: None,
        offset_ms,
        audio,
        background,
        foreign,
    }
}

/// 导入一条判定线；返回 (线, 该线的最大拍)
fn import_line(v: &Value, li: usize, fid: &mut Fidelity) -> Result<(JudgeLine, Beat), String> {
    let o = v.as_object().ok_or_else(|| format!("judgeLineList[{li}] 不是对象"))?;
    let known = [
        "Name", "bpmfactor", "zOrder", "isCover", "eventLayers", "notes", "numOfNotes",
    ];
    let mut foreign = Foreign::new();
    for (k, val) in o {
        if !known.contains(&k.as_str()) {
            foreign.insert(k.clone(), val.clone());
        }
    }
    if i64_of(foreign.get("father"), -1) != -1 {
        fid.warn_grouped("判定线嵌套 `father`（父子关系）", &format!("/judgeLineList[{li}]"));
    }
    if foreign.contains_key("extended") {
        fid.warn_grouped("`extended` 故事板特殊事件层", &format!("/judgeLineList[{li}]"));
    }
    if o.keys().any(|k| k.ends_with("Control")) {
        fid.warn_grouped("`*Control` 控制曲线（按距线距离的关键帧）", &format!("/judgeLineList[{li}]"));
    }

    // ---- 事件层 ----
    let layers_v = get(o, "eventLayers").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let mut layers: Vec<Layer> = Vec::new();
    for (gi, lv) in layers_v.iter().enumerate() {
        // 早期版本把空层写成 null
        let Some(lo) = lv.as_object() else {
            layers.push(Layer::default());
            continue;
        };
        let mut layer = Layer::default();
        for (track, key) in [
            ("moveX", "moveXEvents"),
            ("moveY", "moveYEvents"),
            ("rotate", "rotateEvents"),
            ("alpha", "alphaEvents"),
            ("speed", "speedEvents"),
        ] {
            let arr = get(lo, key).and_then(|x| x.as_array()).cloned().unwrap_or_default();
            let mut list = Vec::with_capacity(arr.len());
            for (ei, ev) in arr.iter().enumerate() {
                let Some(eo) = ev.as_object() else {
                    fid.warn(format!("judgeLineList[{li}].eventLayers[{gi}].{key}[{ei}] 不是对象 —— 丢弃"));
                    continue;
                };
                match import_event(eo, track, li, gi, key, ei, fid) {
                    Ok(e) => list.push(e),
                    Err(why) => fid.warn(format!(
                        "judgeLineList[{li}].eventLayers[{gi}].{key}[{ei}] 无法解析（{why}）—— 丢弃"
                    )),
                }
            }
            if let Some(slot) = layer.track_mut(track) {
                *slot = list;
            }
        }
        // 层里的未知键（例如未来的第六轨道）保留
        let mut lf = Foreign::new();
        for (k, val) in lo {
            if !matches!(
                k.as_str(),
                "moveXEvents" | "moveYEvents" | "rotateEvents" | "alphaEvents" | "speedEvents"
            ) {
                lf.insert(k.clone(), val.clone());
            }
        }
        layer.foreign = lf;
        layers.push(layer);
    }
    if layers.is_empty() {
        layers.push(Layer::default());
    }
    if layers.len() > 5 {
        fid.warn(format!(
            "judgeLineList[{li}] 有 {} 个事件层（RPE 最多 5）—— 全部保留，但超出部分语义未定义",
            layers.len()
        ));
    }

    // ---- 音符 ----
    let notes_v = get(o, "notes").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    let mut notes: Vec<Note> = Vec::with_capacity(notes_v.len());
    let mut end = Beat::zero();
    for (ni, nv) in notes_v.iter().enumerate() {
        let Some(no) = nv.as_object() else {
            fid.warn(format!("judgeLineList[{li}].notes[{ni}] 不是对象 —— 丢弃"));
            continue;
        };
        let note = match import_note(no, li, ni, fid) {
            Ok(n) => n,
            Err(why) => {
                fid.warn(format!("judgeLineList[{li}].notes[{ni}] 无法解析（{why}）—— 丢弃"));
                continue;
            }
        };
        end = end.max(note.end_beat());
        notes.push(note);
    }
    // 线上最后一个音符也要算进谱面结束（事件轨道的延拓目标）
    for layer in &layers {
        for track in crate::doc::TRACKS {
            if let Some(list) = layer.track(track) {
                if let Some(last) = list.last() {
                    end = end.max(last.end);
                }
            }
        }
    }

    let line = JudgeLine {
        name: str_of(get(o, "Name"), "Untitled"),
        bpm_factor: {
            let f = f64_of(get(o, "bpmfactor"), 1.0) as f32;
            if f == 0.0 {
                fid.warn(format!("judgeLineList[{li}].bpmfactor = 0（opm 禁止）—— 记为 1"));
                1.0
            } else {
                f
            }
        },
        z_order: i64_of(get(o, "zOrder"), 0) as i32,
        is_cover: i64_of(get(o, "isCover"), 1) == 1,
        layers,
        notes,
        foreign,
    };
    Ok((line, end))
}

fn import_event(
    eo: &Map<String, Value>,
    track: &str,
    li: usize,
    gi: usize,
    key: &str,
    ei: usize,
    fid: &mut Fidelity,
) -> Result<Event, String> {
    let start = beat_from_value(get(eo, "startTime").ok_or("缺少 startTime")?)?;
    let end = beat_from_value(get(eo, "endTime").ok_or("缺少 endTime")?)?;
    let (mut a, mut b) = (
        norm0(f64_of(get(eo, "start"), 0.0)),
        norm0(f64_of(get(eo, "end"), 0.0)),
    );
    // alpha 轨道：RPE 0~255（负数是"隐藏判定线与其上所有音符"的废弃功能）→ opm 0~1
    if track == "alpha" {
        if a < 0.0 || b < 0.0 {
            fid.warn(format!(
                "judgeLineList[{li}].eventLayers[{gi}].{key}[{ei}] 的 alpha 为负（{a}→{b}）—— \
                 RPE 用负 alpha 连音符一起隐藏，opm 无此表达；已降级为 0"
            ));
        }
        a = (a / 255.0).clamp(0.0, 1.0);
        b = (b / 255.0).clamp(0.0, 1.0);
    }
    let easing_id = i64_of(get(eo, "easingType"), 1);
    let easing = match easing_name_of_rpe(easing_id) {
        Some(n) => n.to_owned(),
        None => {
            fid.warn(format!(
                "judgeLineList[{li}].eventLayers[{gi}].{key}[{ei}] 的 easingType={easing_id} 不在 \
                 spec/easing.json 里 —— 按 linear 处理"
            ));
            "linear".to_owned()
        }
    };
    let bezier = truthy(get(eo, "bezier"), false);
    let bezier_points = match (bezier, get(eo, "bezierPoints").and_then(|x| x.as_array())) {
        (true, Some(a)) if a.len() == 4 => Some([
            f64_of(a.first(), 0.0) as f32,
            f64_of(a.get(1), 0.0) as f32,
            f64_of(a.get(2), 1.0) as f32,
            f64_of(a.get(3), 1.0) as f32,
        ]),
        (true, _) => {
            fid.warn(format!(
                "judgeLineList[{li}].eventLayers[{gi}].{key}[{ei}] 标了 bezier 但 bezierPoints 不是 4 个数 —— 按普通缓动处理"
            ));
            None
        }
        (false, _) => None,
    };
    if bezier && track == "speed" {
        fid.warn(format!(
            "judgeLineList[{li}].eventLayers[{gi}].speedEvents[{ei}] 标了贝塞尔 —— RPE 的流速事件不支持贝塞尔，已按普通缓动处理"
        ));
    }
    // 流速轨的缓动**现在是真的生效的**（缓动按折线实现，见 `perf::event_knots`），
    // 语义取 RPE **1.7.0** 的说法："缓动作用在流速值上、再积分成位置"。
    //
    // 但 1.6.2~1.6.x 的谱面里这个字段的语义不同（phira-docs：作用在 floorPosition 上；
    // prpr 干脆忽略它）—— 这件事必须写进保真度报告，否则"导入后位置和原游戏不一样"会查无实据。
    if track == "speed" && easing != "linear" {
        fid.warn_grouped_note(
            "流速事件的缓动（按 1.7.0 语义求值）",
            &format!("/judgeLineList[{li}].eventLayers[{gi}].speedEvents[{ei}]"),
            "opm 按 RPE 1.7.0 的语义求值：缓动作用在**流速值**上、再积分成位置；来源若是 1.6.x，\
             它把缓动作用在 floorPosition 上（prpr 则忽略缓动），预览可能与原游戏不一致。\
             缓动名原样保留、导出照旧写回",
        );
    }
    // 其它字段（easingLeft/easingRight/linkgroup/自定义）原样保留
    let known = [
        "startTime", "endTime", "start", "end", "easingType", "bezier", "bezierPoints",
    ];
    let mut foreign = Foreign::new();
    for (k, v) in eo {
        if !known.contains(&k.as_str()) {
            foreign.insert(k.clone(), v.clone());
        }
    }
    for k in ["easingLeft", "easingRight"] {
        if let Some(v) = foreign.get(k) {
            let d = v.as_f64().unwrap_or(if k == "easingRight" { 1.0 } else { 0.0 });
            let default = if k == "easingRight" { 1.0 } else { 0.0 };
            if (d - default).abs() > 1e-9 {
                fid.warn(format!(
                    "judgeLineList[{li}].eventLayers[{gi}].{key}[{ei}].{k} = {d}（缓动区间裁剪）—— \
                     opm v1 未建模，已原样保留；本机预览按完整缓动曲线走"
                ));
            }
        }
    }
    Ok(Event {
        start,
        end,
        start_value: json!(a),
        end_value: json!(b),
        easing,
        bezier: bezier_points.is_some(),
        bezier_points,
        foreign,
    })
}

fn import_note(
    no: &Map<String, Value>,
    li: usize,
    ni: usize,
    fid: &mut Fidelity,
) -> Result<Note, String> {
    let ty = i64_of(get(no, "type"), 1);
    let kind = note_kind_from_rpe(ty);
    if !(1..=4).contains(&ty) {
        fid.warn(format!(
            "judgeLineList[{li}].notes[{ni}].type = {ty} 不在 1~4 —— RPE 表记缺失按 Tap，已按 tap 处理"
        ));
    }
    let start = beat_from_value(get(no, "startTime").ok_or("缺少 startTime")?)?;
    let raw_end = match get(no, "endTime") {
        Some(v) => beat_from_value(v)?,
        None => start,
    };
    let mut end = None;
    if kind == crate::doc::NoteKind::Hold {
        // opm 错误规则 8：hold 必须有 endBeat > startBeat
        // 加一个 1/64 拍：Beat 没有 Add，按有理数加
        let tiny = Beat::new(start.n * 64 + start.d, start.d * 64);
        let e = if raw_end > start { raw_end } else { tiny };
        if raw_end <= start {
            fid.warn(format!(
                "judgeLineList[{li}].notes[{ni}] 是 hold 但 endTime ≤ startTime —— 补成 1/64 拍（否则 opm 不合法）"
            ));
        }
        end = Some(e);
    } else if raw_end > start {
        fid.warn(format!(
            "judgeLineList[{li}].notes[{ni}] 类型 {} 却带 endTime > startTime —— opm 不允许非 hold 带时长，已忽略 endTime",
            kind.as_str()
        ));
    }
    let alpha_raw = i64_of(get(no, "alpha"), 255);
    if alpha_raw > 255 {
        fid.note(format!(
            "judgeLineList[{li}].notes[{ni}].alpha = {alpha_raw} > 255 —— 按规范**不截断**，原样保留"
        ));
    }
    let alpha = alpha_raw.clamp(0, u16::MAX as i64) as u16;
    let above = i64_of(get(no, "above"), 1);
    if above != 1 {
        fid.note(format!(
            "judgeLineList[{li}].notes[{ni}].above = {above}（背面下落）→ side = \"below\""
        ));
    }
    // RPE 的 `tint` 与旧名 `color` 双名共存 —— 都保留，取到哪个算哪个
    let mut foreign = Foreign::new();
    for (k, v) in no {
        if !matches!(
            k.as_str(),
            "type" | "startTime" | "endTime" | "positionX" | "above" | "alpha" | "isFake"
                | "speed" | "size" | "judgeArea" | "yOffset"
        ) {
            foreign.insert(k.clone(), v.clone());
        }
    }
    if foreign.contains_key("color") && !foreign.contains_key("tint") {
        foreign.insert("tint".to_owned(), foreign["color"].clone());
    }
    Ok(Note {
        kind,
        start,
        end,
        lane_x: norm0(f64_of(get(no, "positionX"), 0.0)) as f32,
        side: if above == 1 { "above".to_owned() } else { "below".to_owned() },
        is_fake: i64_of(get(no, "isFake"), 0) == 1,
        alpha,
        speed: norm0(f64_of(get(no, "speed"), 1.0)) as f32,
        width_scale: norm0(f64_of(get(no, "size"), 1.0)) as f32,
        y_offset: norm0(f64_of(get(no, "yOffset"), 0.0)) as f32,
        judge_area_scale: norm0(f64_of(get(no, "judgeArea"), 1.0)) as f32,
        foreign,
    })
}

// ---------------------------------------------------------------- 导出

/// opm 文档 → RPE JSON 文本（pretty）+ 保真度报告
pub fn save_str(doc: &Document, target: RpeTarget) -> (String, Fidelity) {
    let (v, fid) = to_value(doc, target);
    let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_owned());
    (format!("{text}\n"), fid)
}

/// 写文件
pub fn save_file(
    doc: &Document,
    path: &std::path::Path,
    target: RpeTarget,
) -> Result<Fidelity, String> {
    let (text, fid) = save_str(doc, target);
    std::fs::write(path, text).map_err(|e| format!("写入失败: {e}"))?;
    Ok(fid)
}

/// opm 文档 → RPE JSON 值
pub fn to_value(doc: &Document, target: RpeTarget) -> (Value, Fidelity) {
    let mut fid = Fidelity::new("rpe", format!("目标 RPEVersion={}", target.version));

    // ---- META ----
    let mut meta = Map::new();
    meta.insert("RPEVersion".to_owned(), json!(target.version));
    meta.insert("offset".to_owned(), json!(doc.meta.offset_ms));
    meta.insert("name".to_owned(), json!(doc.meta.name));
    meta.insert("id".to_owned(), json!(""));
    meta.insert("song".to_owned(), json!(doc.meta.audio.clone().unwrap_or_default()));
    meta.insert("background".to_owned(), json!(doc.meta.background.clone().unwrap_or_default()));
    meta.insert("composer".to_owned(), json!(doc.meta.composer));
    meta.insert("charter".to_owned(), json!(doc.meta.charter));
    meta.insert("illustration".to_owned(), json!(doc.meta.illustrator));
    meta.insert("level".to_owned(), json!(doc.meta.level));
    for (k, v) in &doc.meta.foreign {
        if k == "RPEVersion" || k == "id" {
            continue; // 由本函数决定
        }
        meta.insert(k.clone(), v.clone());
    }
    if !doc.meta.foreign.is_empty() {
        fid.note(format!("`META` 保留 {} 个来源字段", doc.meta.foreign.len()));
    }

    // ---- BPM 表 ----
    let bpm: Vec<Value> = doc
        .bpm_list
        .iter()
        .map(|e| {
            let mut o = Map::new();
            o.insert("bpm".to_owned(), num_value(e.bpm as f64));
            o.insert("startTime".to_owned(), json!(bf(e.start)));
            for (k, v) in &e.foreign {
                o.insert(k.clone(), v.clone());
            }
            Value::Object(o)
        })
        .collect();

    // ---- 判定线 ----
    // 谱面时长**用文档自己的那一份**（`Document::chart_end` = 全部音符与事件的 max）：
    // 这里曾另有一份"取每条轨道最后一条事件"的计算，而"最后一条"按**起点**排序 ——
    // 一旦某条轨道上"起点最晚的那条"不是"结束最晚的那条"，导出的 `chartTime` 就比真实谱面短。
    let last_beat = doc.chart_end();
    let mut lines: Vec<Value> = Vec::with_capacity(doc.judge_lines.len());
    for (li, line) in doc.judge_lines.iter().enumerate() {
        let mut o = Map::new();
        o.insert("Group".to_owned(), json!(0));
        o.insert("Name".to_owned(), json!(line.name));
        o.insert("Texture".to_owned(), json!("line.png"));
        o.insert("zOrder".to_owned(), json!(line.z_order));
        o.insert("isCover".to_owned(), json!(if line.is_cover { 1 } else { 0 }));
        o.insert("father".to_owned(), json!(-1));
        o.insert("rotateWithFather".to_owned(), json!(false));
        o.insert("bpmfactor".to_owned(), num_value(line.bpm_factor as f64));

        let mut layers: Vec<Value> = Vec::new();
        for (gi, layer) in line.layers.iter().enumerate() {
            let mut lo = Map::new();
            for (track, key) in [
                ("moveX", "moveXEvents"),
                ("moveY", "moveYEvents"),
                ("rotate", "rotateEvents"),
                ("alpha", "alphaEvents"),
                ("speed", "speedEvents"),
            ] {
                let list = layer.track(track).cloned().unwrap_or_default();
                let arr: Vec<Value> = list
                    .iter()
                    .map(|e| export_event(e, track, target, li, gi, &mut fid))
                    .collect();
                lo.insert(key.to_owned(), Value::Array(arr));
            }
            for (k, v) in &layer.foreign {
                lo.insert(k.clone(), v.clone());
            }
            layers.push(Value::Object(lo));
        }
        o.insert("eventLayers".to_owned(), Value::Array(layers));

        // ---- 音符 ----
        let notes: Vec<Value> = line
            .notes
            .iter()
            .map(|n| export_note(n, target, &mut fid))
            .collect();
        // RPE 的 numOfNotes 语义：**含 FakeNote，不含 Hold**（Phigros-规则速查 §4.2）
        let num = line.notes.iter().filter(|n| n.kind != crate::doc::NoteKind::Hold).count();
        o.insert("numOfNotes".to_owned(), json!(num));
        o.insert("notes".to_owned(), Value::Array(notes));

        // 来源字段（Texture/father/Group/anchor/controls/extended…）原样写回
        for (k, v) in &line.foreign {
            if o.contains_key(k) {
                continue;
            }
            o.insert(k.clone(), v.clone());
        }
        lines.push(Value::Object(o));
    }
    if doc.judge_lines.iter().any(|l| l.foreign.contains_key("father")) {
        fid.warn(
            "判定线嵌套 `father` 只是原样写回：opm v1 未建模父子关系，导出后播放器可能重新按嵌套渲染"
                .to_owned(),
        );
    }

    let mut root = Map::new();
    root.insert("BPMList".to_owned(), Value::Array(bpm));
    root.insert("META".to_owned(), Value::Object(meta));
    root.insert("chartTime".to_owned(), json!(bf(last_beat)));
    root.insert("judgeLineGroup".to_owned(), json!([""]));
    root.insert("judgeLineList".to_owned(), Value::Array(lines));
    root.insert("multiLineString".to_owned(), json!(""));
    root.insert("multiScale".to_owned(), json!([1.0, 1.0]));
    root.insert("timeTags".to_owned(), json!([]));
    root.insert("xybind".to_owned(), json!([]));
    for (k, v) in &doc.foreign {
        if root.contains_key(k) {
            continue;
        }
        root.insert(k.clone(), v.clone());
    }
    // 未建模的字段一律报告（不许"悄悄丢"）
    let mut unmodeled: Vec<String> = Vec::new();
    for (k, _) in &doc.foreign {
        unmodeled.push(format!("根字段 `{k}`"));
    }
    if !unmodeled.is_empty() {
        fid.warn(format!("以下来源字段按原名写回（opm v1 未建模）：{}", unmodeled.join("、")));
    }
    if doc.extensions.iter().any(|e| !e.starts_with("x-opm:")) {
        fid.warn("`extensions` 里存在非 `x-opm:` 前缀项 —— RPE 不认识，已忽略".to_owned());
    }
    fid.note(format!(
        "导出 {} 条判定线、{} 个音符、{} 条 BPM 条目",
        doc.judge_lines.len(),
        doc.judge_lines.iter().map(|l| l.notes.len()).sum::<usize>(),
        doc.bpm_list.len()
    ));
    fid.finalize();
    (Value::Object(root), fid)
}

fn export_event(
    e: &Event,
    track: &str,
    target: RpeTarget,
    li: usize,
    gi: usize,
    fid: &mut Fidelity,
) -> Value {
    let mut o = Map::new();
    let (st, et) = if target.triple_time {
        (beat_to_triple(e.start), beat_to_triple(e.end))
    } else {
        (json!(bf(e.start)), json!(bf(e.end)))
    };
    o.insert("startTime".to_owned(), st);
    o.insert("endTime".to_owned(), et);
    // alpha 轨道：opm 0~1 → RPE 0~255（四舍五入；离线值与 RPE 一致）
    let scale = if track == "alpha" { 255.0 } else { 1.0 };
    let a = e.start_value.as_f64().unwrap_or(0.0) * scale;
    let b = e.end_value.as_f64().unwrap_or(0.0) * scale;
    let a = if track == "alpha" { a.round() } else { a };
    let b = if track == "alpha" { b.round() } else { b };
    o.insert("start".to_owned(), num_value(a));
    o.insert("end".to_owned(), num_value(b));
    let easing_id = rpe_id_of_easing(&e.easing).unwrap_or(1);
    o.insert("easingType".to_owned(), json!(easing_id));
    o.insert("bezier".to_owned(), json!(if e.bezier { 1 } else { 0 }));
    o.insert(
        "bezierPoints".to_owned(),
        match e.bezier_points {
            Some(p) => json!([p[0], p[1], p[2], p[3]]),
            None => json!([0.0, 0.0, 1.0, 1.0]),
        },
    );
    o.insert("easingLeft".to_owned(), json!(0.0));
    o.insert("easingRight".to_owned(), json!(1.0));
    o.insert("linkgroup".to_owned(), json!(0));
    for (k, v) in &e.foreign {
        if o.contains_key(k) {
            continue;
        }
        o.insert(k.clone(), v.clone());
    }
    if e.foreign.contains_key("easingLeft") || e.foreign.contains_key("easingRight") {
        fid.note(format!(
            "/judgeLineList[{li}].eventLayers[{gi}].{track}Events：`easingLeft/Right` 原样写回（opm v1 未建模）"
        ));
    }
    Value::Object(o)
}

fn export_note(n: &Note, target: RpeTarget, fid: &mut Fidelity) -> Value {
    let mut o = Map::new();
    o.insert("type".to_owned(), json!(note_kind_to_rpe(n.kind)));
    let end = n.end_beat();
    if target.note_triple_time {
        o.insert("startTime".to_owned(), beat_to_triple(n.start));
        o.insert("endTime".to_owned(), beat_to_triple(end));
    } else {
        o.insert("startTime".to_owned(), json!(bf(n.start)));
        o.insert("endTime".to_owned(), json!(bf(end)));
    }
    o.insert("positionX".to_owned(), num_value(n.lane_x as f64));
    o.insert("above".to_owned(), json!(if n.side == "below" { 0 } else { 1 }));
    o.insert("alpha".to_owned(), json!(n.alpha));
    o.insert("isFake".to_owned(), json!(if n.is_fake { 1 } else { 0 }));
    o.insert("speed".to_owned(), num_value(n.speed as f64));
    o.insert("size".to_owned(), num_value(n.width_scale as f64));
    o.insert("visibleTime".to_owned(), json!(999999.0));
    o.insert("yOffset".to_owned(), num_value(n.y_offset as f64));
    if n.judge_area_scale != 1.0 {
        o.insert("judgeArea".to_owned(), json!(n.judge_area_scale));
    }
    for (k, v) in &n.foreign {
        if o.contains_key(k) || k == "color" {
            continue; // color 是 tint 的旧名，统一写 tint
        }
        o.insert(k.clone(), v.clone());
    }
    if n.foreign.contains_key("tint") || n.foreign.contains_key("color") {
        let t = n.foreign.get("tint").or_else(|| n.foreign.get("color")).cloned();
        o.insert("tint".to_owned(), t.unwrap_or(json!([255, 255, 255])));
    }
    if n.alpha > 255 {
        fid.note(format!(
            "音符 alpha = {} > 255 —— RPE 能存但多数播放器按 255 处理，已原样写出",
            n.alpha
        ));
    }
    Value::Object(o)
}
