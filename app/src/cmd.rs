// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! 命令解析与校验（**不再持有会话** —— 编辑会话在 [`crate::core::EditCore`]）。
//!
//! 这里只保留三件与状态无关的事：
//! · [`parse_commands`] 把 JSONL / JSON 数组解析成命令序列；
//! · [`validate`] 按 `spec/opm-format.md` 第 8 节校验文档（与 `spec/check.py` 是两份独立实现）；
//! · [`parse_beat`] / [`response_line`] 这类纯函数。

use serde_json::{json, Value};

use crate::doc::{Beat, Document, NoteKind, TRACKS};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warn,
}

pub struct Issue {
    pub severity: Severity,
    pub pointer: String,
    pub message: String,
}

/// 全部合法缓动名 —— **数据源是 `spec/easing.json`**，这里不再手抄一份。
///
/// 以前这里是一个 29 个字符串的常量表。两份表在"加第 30 个缓动"那天必然分家，
/// 症状是"RPE 导入认它、`set_event` 不认它"（或反过来），而那只在真有人用那个缓动时才显形。
pub fn easings() -> Vec<&'static str> {
    crate::codec::easing_names()
}

/// 这个名字是不是已知缓动（**校验的唯一判据**）
pub fn is_easing(name: &str) -> bool {
    crate::codec::rpe_id_of_easing(name).is_some()
}

// ---------------------------------------------------------------- 缓动的两段选择

/// 缓动的**曲线**（用户要求：选择拆成"曲线 | in/out/io"两段，而不是在 29 个名字里翻）。
///
/// 29 个名字本来就是"曲线 × 变体"的笛卡尔积加一个线性，所以拆分是**无损**的
/// （`split_easing` / `easing_name` 有往返测试逐个钉住）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EaseCurve {
    Linear,
    Sine,
    Quad,
    Cubic,
    Quart,
    Quint,
    Expo,
    Circ,
    Back,
    Elastic,
    Bounce,
}

impl EaseCurve {
    /// 下拉里的顺序（线性在最前，其余按"常用 → 特殊"）
    pub const ALL: [EaseCurve; 11] = [
        EaseCurve::Linear,
        EaseCurve::Sine,
        EaseCurve::Quad,
        EaseCurve::Cubic,
        EaseCurve::Quart,
        EaseCurve::Quint,
        EaseCurve::Expo,
        EaseCurve::Circ,
        EaseCurve::Back,
        EaseCurve::Elastic,
        EaseCurve::Bounce,
    ];

    /// 中文名（下拉里显示的是它；名字里的英文段由 [`Self::key`] 给）
    pub fn label(self) -> &'static str {
        match self {
            EaseCurve::Linear => "线性",
            EaseCurve::Sine => "正弦 sine",
            EaseCurve::Quad => "二次 quad",
            EaseCurve::Cubic => "三次 cubic",
            EaseCurve::Quart => "四次 quart",
            EaseCurve::Quint => "五次 quint",
            EaseCurve::Expo => "指数 expo",
            EaseCurve::Circ => "圆形 circ",
            EaseCurve::Back => "回拉 back",
            EaseCurve::Elastic => "弹性 elastic",
            EaseCurve::Bounce => "弹跳 bounce",
        }
    }

    /// 名字里的那一段（`out` + `Sine` 的 `Sine`；线性整名就是 `linear`）
    fn key(self) -> &'static str {
        match self {
            EaseCurve::Linear => "linear",
            EaseCurve::Sine => "Sine",
            EaseCurve::Quad => "Quad",
            EaseCurve::Cubic => "Cubic",
            EaseCurve::Quart => "Quart",
            EaseCurve::Quint => "Quint",
            EaseCurve::Expo => "Expo",
            EaseCurve::Circ => "Circ",
            EaseCurve::Back => "Back",
            EaseCurve::Elastic => "Elastic",
            EaseCurve::Bounce => "Bounce",
        }
    }

    /// 这条曲线**真实存在**的变体（RPE 的 29 个名字里没有 `inOutQuint` / `inOutExpo`
    /// —— 下拉里因此也不该出现那两格，否则选出来的会是另一个名字）
    pub fn variants(self) -> &'static [EaseVariant] {
        match self {
            EaseCurve::Linear => &[],
            EaseCurve::Quint | EaseCurve::Expo => &[EaseVariant::In, EaseVariant::Out],
            _ => &EaseVariant::ALL,
        }
    }

    /// 这条曲线上**最接近** `variant` 的合法变体：不存在就退到 `out`（线性原样返回它的变体，
    /// 因为线性根本不看变体）。
    ///
    /// "切成 quint 之后变体还停在 io" 是界面里真会发生的一步，夹取规则只有这一份实现
    /// —— 界面与 [`easing_name`] 都用它。
    pub fn clamp_variant(self, variant: EaseVariant) -> EaseVariant {
        let avail = self.variants();
        if avail.is_empty() || avail.contains(&variant) {
            variant
        } else {
            EaseVariant::Out
        }
    }
}

/// 缓动曲线上的位置：**in / out / io**（用户用的就是这三个写法；RPE 里写作 `inOut`）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EaseVariant {
    In,
    Out,
    InOut,
}

impl EaseVariant {
    pub const ALL: [EaseVariant; 3] = [EaseVariant::In, EaseVariant::Out, EaseVariant::InOut];

    /// 选择器里显示的字（用户口径：`in` / `out` / `io`）
    pub fn label(self) -> &'static str {
        match self {
            EaseVariant::In => "in",
            EaseVariant::Out => "out",
            EaseVariant::InOut => "io",
        }
    }

    /// 名字里的前缀（RPE 口径：`in` / `out` / `inOut`）
    fn key(self) -> &'static str {
        match self {
            EaseVariant::In => "in",
            EaseVariant::Out => "out",
            EaseVariant::InOut => "inOut",
        }
    }
}

/// 把 29 个名字拆成 `(曲线, 变体)`；线性给 `None`（它没有变体）。
/// 认不出来 ⇒ `None`（调用方按"未知缓动"处理，不要瞎猜一个）。
pub fn split_easing(name: &str) -> Option<(EaseCurve, Option<EaseVariant>)> {
    if name == EaseCurve::Linear.key() {
        return Some((EaseCurve::Linear, None)); // 线性是唯一没有变体的
    }
    for curve in EaseCurve::ALL {
        for v in EaseVariant::ALL {
            let full = format!("{}{}", v.key(), curve.key());
            if name == full {
                return Some((curve, Some(v)));
            }
        }
    }
    None
}

/// `(曲线, 变体)` → 29 个名字之一。
///
/// **变体不适用于这条曲线时退到 `out`**（线性 ⇒ `linear`；`quint`/`expo` + `io` ⇒ `outQuint`/
/// `outExpo`）—— 界面上"切曲线时顺手换了变体"不该发出一格不存在的名字，
/// 而退回 `out` 比退回 `linear` 更接近用户的本意。界面自己也会夹变体（见 [`EaseCurve::variants`]），
/// 所以正常情况下这条兜底根本走不到。
pub fn easing_name(curve: EaseCurve, variant: EaseVariant) -> &'static str {
    if curve == EaseCurve::Linear {
        return "linear";
    }
    let want = curve.clamp_variant(variant);
    // 拼出来的一定在 spec 的表里（往返测试逐个钉住），所以能返回 'static
    for name in easings() {
        if split_easing(name) == Some((curve, Some(want))) {
            return name;
        }
    }
    "linear"
}

/// 校验实现，对应 `spec/opm-format.md` 第 8 节。
/// **与 `spec/check.py` 是两份独立实现**，两者应在同一份文件上给出相同结论。
pub fn validate(doc: &Document) -> Vec<Issue> {
    let mut out: Vec<Issue> = Vec::new();
    macro_rules! err {
        ($ptr:expr, $msg:expr $(,)?) => {
            out.push(Issue {
                severity: Severity::Error,
                pointer: $ptr.to_owned(),
                message: $msg,
            })
        };
    }
    macro_rules! warn {
        ($ptr:expr, $msg:expr $(,)?) => {
            out.push(Issue {
                severity: Severity::Warn,
                pointer: $ptr.to_owned(),
                message: $msg,
            })
        };
    }

    if doc.format != "opm" {
        err!("/format", format!("format 必须是 \"opm\"（当前 {}）", doc.format));
    }
    if doc.extensions.iter().any(|e| !e.starts_with("x-opm:")) {
        err!("/extensions", "扩展名必须使用 x-opm: 前缀".into());
    }
    if !doc.extensions.is_empty() && doc.min_client_capability < 3 {
        err!(
            "/minClientCapability",
            format!(
                "声明了 {} 个扩展但 minClientCapability={}（应为 3）",
                doc.extensions.len(),
                doc.min_client_capability
            )
        );
    }

    if doc.bpm_list.is_empty() {
        err!("/bpmList", "bpmList 不能为空".into());
    } else {
        for (i, b) in doc.bpm_list.iter().enumerate() {
            if b.bpm <= 0.0 {
                err!(&format!("/bpmList[{i}].bpm"), format!("bpm 必须大于 0（当前 {}）", b.bpm));
            }
            if i == 0 && b.start != Beat::zero() {
                err!(
                    &format!("/bpmList[{i}].startBeat"),
                    format!("首个 BPM 必须从拍 0 开始（当前 {}）", b.start.to_f64())
                );
            }
            if i > 0 && b.start <= doc.bpm_list[i - 1].start {
                err!(&format!("/bpmList[{i}].startBeat"), "BPM 的 startBeat 必须严格递增".into());
            }
        }
    }

    let chart_end = doc.chart_end();
    if doc.judge_lines.is_empty() {
        err!("/judgeLines", "judgeLines 不能为空".into());
    }
    if doc.judge_lines.len() > 100 {
        warn!(
            "/judgeLines",
            format!("判定线 {} 条 > 100：官谱格式下会导致谱面停顿", doc.judge_lines.len())
        );
    }

    for (li, line) in doc.judge_lines.iter().enumerate() {
        let lp = format!("/judgeLines[{li}]");
        if line.bpm_factor == 0.0 {
            err!(&format!("{lp}.bpmFactor"), "bpmFactor 不得为 0".into());
        }
        if line.layers.is_empty() {
            err!(&format!("{lp}.layers"), "layers 必须非空".into());
        }
        if line.layers.len() > 5 {
            err!(&format!("{lp}.layers"), "层数超过上限 5".into());
        }

        for (yi, layer) in line.layers.iter().enumerate() {
            for track in TRACKS {
                let Some(events) = layer.track(track) else { continue };
                let tp = format!("{lp}.layers[{yi}].{track}");
                if events.is_empty() {
                    continue;
                }
                let mut prev_end: Option<Beat> = None;
                for (ei, e) in events.iter().enumerate() {
                    let ep = format!("{tp}[{ei}]");
                    if e.end <= e.start {
                        err!(
                            &format!("{ep}.endBeat"),
                            format!("endBeat({}) 必须大于 startBeat({})", e.end.to_f64(), e.start.to_f64())
                        );
                    }
                    if ei == 0 && e.start > Beat::zero() {
                        err!(
                            &format!("{ep}.startBeat"),
                            format!("轨道首事件必须从拍 0 或更早开始（当前 {}）", e.start.to_f64())
                        );
                    }
                    if let Some(pe) = prev_end {
                        if e.start != pe {
                            let kind = if e.start > pe { "空隙" } else { "重叠" };
                            err!(
                                &ep,
                                format!(
                                    "轨道不连续（{kind}）：上一事件止于 {}，本事件起于 {}",
                                    pe.to_f64(),
                                    e.start.to_f64()
                                )
                            );
                        }
                    }
                    if !is_easing(&e.easing) {
                        err!(&format!("{ep}.easing"), format!("未知缓动 {:?}", e.easing));
                    }
                    prev_end = Some(e.end);
                }
                if let Some(pe) = prev_end {
                    if pe < chart_end {
                        err!(
                            &tp,
                            format!("轨道末事件止于 {}，早于谱面末尾 {}", pe.to_f64(), chart_end.to_f64())
                        );
                    }
                }
            }
        }

        for (ni, n) in line.notes.iter().enumerate() {
            let np = format!("{lp}.notes[{ni}]");
            match n.kind {
                NoteKind::Hold => match n.end {
                    None => err!(&np, "hold 必须有 endBeat".into()),
                    Some(e) if e <= n.start => err!(
                        &format!("{np}.endBeat"),
                        format!("hold 的 endBeat({}) 必须大于 startBeat({})", e.to_f64(), n.start.to_f64())
                    ),
                    _ => {}
                },
                _ => {
                    if n.end.is_some() {
                        err!(&np, format!("非 hold 音符不得携带 endBeat（kind={:?}）", n.kind));
                    }
                }
            }
            // alpha 是 u16：上界由类型本身保证，"超过 65535"这种比较无意义（编译器会提示 unused_comparisons）
            if n.alpha > 255 {
                warn!(
                    &format!("{np}.alpha"),
                    format!("alpha={} 超出规范 0~255（RPE 实际存在此类值，读入不得截断）", n.alpha)
                );
            }
            if n.lane_x.abs() > 675.0 {
                warn!(&format!("{np}.laneX"), format!("laneX={} 超出 RPE 坐标系 ±675", n.lane_x));
            }
            if n.judge_area_scale <= 0.0 {
                err!(&format!("{np}.judgeAreaScale"), "判定区宽度倍率必须是正数".into());
            }
        }
    }

    // ---- 遮蔽区（躁域）----
    //
    // **与判定线轨道的关键差别**（规范 §4.6）：遮蔽区的通道**允许空隙**、**允许首事件晚于拍 0**
    // —— "什么时候出现"就是靠"第一条坐标事件从哪一拍开始"表达的，所以那两条判定线规则
    // （首事件从 0 起、轨道连续）**不适用**。剩下的不变量只有三条：按 start 升序、
    // 不许重叠、`endBeat > startBeat`（升序是求值的前提：`perf::active_event` 走二分）。
    for (zi, z) in doc.mask_zones.iter().enumerate() {
        let zp = format!("/maskZones[{zi}]");
        for track in crate::doc::MASK_TRACKS {
            let Some(events) = z.track(track) else { continue };
            if events.is_empty() {
                continue;
            }
            let tp = format!("{zp}.{track}");
            let mut prev: Option<(&Beat, &Beat)> = None;
            for (ei, e) in events.iter().enumerate() {
                let ep = format!("{tp}[{ei}]");
                if e.end <= e.start {
                    err!(
                        &format!("{ep}.endBeat"),
                        format!("endBeat({}) 必须大于 startBeat({})", e.end.to_f64(), e.start.to_f64())
                    );
                }
                if !is_easing(&e.easing) {
                    err!(&format!("{ep}.easing"), format!("未知缓动 {:?}", e.easing));
                }
                // **一个 active 事件块只能是一种状态**（用户口径 2026-10-02）：头尾值必须落在同一档，
                // 否则这块区会在中途换外观（"false 渐变到 true" 就是这种）。要换外观就放**两块**。
                if track == "active" {
                    match (
                        crate::doc::doc_active_state(&e.start_value),
                        crate::doc::doc_active_state(&e.end_value),
                    ) {
                        (Some(a), Some(b)) if a != b => err!(
                            &ep,
                            format!(
                                "active 事件块只能是一种状态：起值 {:?} 与终值 {:?} 分别是 {a} 与 {b}\n                 —— 想中途换外观就放两块（各是一种状态），别用渐变",
                                e.start_value, e.end_value
                            )
                        ),
                        (None, _) | (_, None) => {}  // 类型不对由别处报（这里不重复）
                        _ => {}
                    }
                }
                if let Some((ps, pe)) = prev {
                    if e.start < *ps {
                        err!(
                            &ep,
                            format!(
                                "遮蔽区通道必须按 startBeat 升序（上一事件起于 {}，本事件起于 {}）",
                                ps.to_f64(),
                                e.start.to_f64()
                            )
                        );
                    } else if e.start < *pe {
                        err!(
                            &ep,
                            format!(
                                "遮蔽区通道不允许重叠：上一事件止于 {}，本事件起于 {}",
                                pe.to_f64(),
                                e.start.to_f64()
                            )
                        );
                    }
                }
                prev = Some((&e.start, &e.end));
            }
        }
    }
    if !doc.mask_zones.is_empty() && doc.min_client_capability < crate::doc::CAP_MASK {
        err!(
            "/minClientCapability",
            format!(
                "有 {} 块遮蔽区但 minClientCapability={}（应为 {}）—— 不认识遮蔽区的读取方必须拒绝载入，\n                 否则会渲染出一份「该挡的地方没挡」的谱面",
                doc.mask_zones.len(),
                doc.min_client_capability,
                crate::doc::CAP_MASK
            )
        );
    }
    out
}

/// 一处**事件重叠**：中间那段被两个事件同时覆盖
#[derive(Clone, Debug, PartialEq)]
pub struct Overlap {
    pub line: usize,
    pub layer: usize,
    pub track: String,
    /// 前一个事件的下标（止于重叠区起点）
    pub prev: usize,
    /// 后一个事件的下标（起于重叠区之前）
    pub next: usize,
    /// 重叠区间的拍（[start, end)，end 是 prev 的末端）
    pub start: Beat,
    pub end: Beat,
}

impl Overlap {
    pub fn pointer(&self) -> String {
        format!(
            "/judgeLines[{}].layers[{}].{}[{}]",
            self.line, self.layer, self.track, self.next
        )
    }
    pub fn label(&self) -> String {
        format!(
            "线 #{} · {} · 事件 {}→{} 重叠 [{:.3}, {:.3}) 拍",
            self.line,
            self.track,
            self.prev,
            self.next,
            self.start.to_f64(),
            self.end.to_f64()
        )
    }
}

/// 找某一层某条轨道上的重叠（按 start 排序后，后一个的起拍早于前一个的止拍即重叠）。
///
/// 与 `validate` 里的"轨道不连续"检查同源，但这里**只报重叠**、且能按线跑 ——
/// 前者是给人看的全量校验，这里是给界面用的增量检测（每次改动只查动过的那条线）。
pub fn overlaps_in_track(
    line: usize,
    layer: usize,
    track: &str,
    events: &[crate::doc::Event],
) -> Vec<Overlap> {
    let mut idx: Vec<usize> = (0..events.len()).collect();
    idx.sort_by(|&a, &b| {
        events[a]
            .start
            .cmp(&events[b].start)
            .then(events[a].end.cmp(&events[b].end))
    });
    let mut out = Vec::new();
    for w in idx.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (pa, pb) = (&events[a], &events[b]);
        if pb.start < pa.end {
            // 只在**真的多覆盖了一段**时报（止拍相同、只是端点相接不算重叠）
            let end = pa.end.min(pb.end);
            if pb.start < end {
                out.push(Overlap {
                    line,
                    layer,
                    track: track.to_owned(),
                    prev: a,
                    next: b,
                    start: pb.start,
                    end,
                });
            }
        }
    }
    out
}

/// 某条判定线上所有轨道的重叠（增量检测的入口）
pub fn overlaps_of_line(doc: &Document, line: usize) -> Vec<Overlap> {
    let Some(l) = doc.judge_lines.get(line) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (yi, layer) in l.layers.iter().enumerate() {
        for track in crate::doc::TRACKS {
            if let Some(events) = layer.track(track) {
                out.extend(overlaps_in_track(line, yi, track, events));
            }
        }
    }
    out
}

/// 全量重叠检测（加载谱面、或结构变化时用）
pub fn overlaps(doc: &Document) -> Vec<Overlap> {
    let mut out = Vec::new();
    for i in 0..doc.judge_lines.len() {
        out.extend(overlaps_of_line(doc, i));
    }
    out
}

pub fn validate_json(doc: &Document) -> Value {
    let issues = validate(doc);
    let errors = issues.iter().filter(|i| i.severity == Severity::Error).count();
    json!({
        "errors": errors,
        "warnings": issues.len() - errors,
        "issues": issues.iter().map(|i| json!({
            "severity": match i.severity { Severity::Error => "ERROR", Severity::Warn => "WARN" },
            "pointer": i.pointer,
            "message": i.message,
        })).collect::<Vec<_>>(),
    })
}

/// 接受 `[n, d]` 或 `{"n":…, "d":…}`
pub fn parse_beat(v: &Value) -> Result<Beat, String> {
    if let Some(a) = v.as_array() {
        if a.len() != 2 {
            return Err("拍数组必须是 [分子, 分母]".into());
        }
        let n = a[0].as_i64().ok_or("拍的分子必须是整数")?;
        let d = a[1].as_i64().ok_or("拍的分母必须是整数")?;
        if d == 0 {
            return Err("拍的分母不得为 0".into());
        }
        return Ok(Beat::new(n, d));
    }
    if let Some(o) = v.as_object() {
        let n = o.get("n").and_then(|x| x.as_i64()).ok_or("拍缺少整数 n")?;
        let d = o.get("d").and_then(|x| x.as_i64()).ok_or("拍缺少整数 d")?;
        if d == 0 {
            return Err("拍的分母不得为 0".into());
        }
        return Ok(Beat::new(n, d));
    }
    Err("拍必须是 [n, d] 或 {\"n\":…,\"d\":…}".into())
}

/// 解析一批命令：JSONL（每行一条，`//` 开头忽略）或 JSON 数组
pub fn parse_commands(text: &str) -> Result<Vec<Value>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        let v: Value = serde_json::from_str(trimmed).map_err(|e| format!("JSON 解析失败: {e}"))?;
        return v
            .as_array()
            .cloned()
            .ok_or_else(|| "顶层数组里必须是命令对象".to_string());
    }
    trimmed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//"))
        .map(|l| serde_json::from_str::<Value>(l).map_err(|e| format!("第 {l:?} 行解析失败: {e}")))
        .collect()
}

/// 把响应压成一行（CLI 默认输出）
pub fn response_line(resp: &Value) -> String {
    let op = resp.get("op").and_then(|v| v.as_str()).unwrap_or("?");
    if resp.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        let r = resp.get("result").cloned().unwrap_or(Value::Null);
        format!("ok   {op} {}", compact(&r))
    } else {
        let e = resp.get("error").and_then(|v| v.as_str()).unwrap_or("未知错误");
        format!("FAIL {op}: {e}")
    }
}

fn compact(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 160 {
        let head: String = s.chars().take(160).collect();
        format!("{head}…")
    } else {
        s
    }
}

#[cfg(test)]
mod easing_split_tests {
    use super::*;

    /// **29 个名字全都能拆回 `(曲线, 变体)` 再拼回去**（拆分必须无损，否则界面里选一圈
    /// 就会把用户的缓动换成另一个名字）。
    #[test]
    fn every_easing_name_round_trips_through_the_two_part_selection() {
        for name in easings() {
            let (curve, variant) = split_easing(name).unwrap_or_else(|| panic!("拆不开 {name}"));
            match variant {
                Some(v) => assert_eq!(easing_name(curve, v), name, "{name} 往返不一致"),
                None => {
                    assert_eq!(curve, EaseCurve::Linear, "只有线性没有变体：{name}");
                    assert_eq!(easing_name(curve, EaseVariant::Out), "linear");
                }
            }
        }
    }

    /// **每一格都拼得出名字**，且名字一定合法（界面里随便点都不会发出非法缓动）
    #[test]
    fn every_curve_and_variant_pair_makes_a_known_name() {
        for curve in EaseCurve::ALL {
            for v in EaseVariant::ALL {
                let name = easing_name(curve, v);
                assert!(is_easing(name), "{curve:?}+{v:?} 拼出了非法名字 {name}");
            }
        }
        // 线性的变体是空的；quint/expo 只有 in/out（RPE 里没有 inOutQuint / inOutExpo）
        assert!(EaseCurve::Linear.variants().is_empty());
        assert_eq!(easing_name(EaseCurve::Linear, EaseVariant::InOut), "linear");
        assert_eq!(
            EaseCurve::Quint.variants(),
            &[EaseVariant::In, EaseVariant::Out]
        );
        assert_eq!(EaseCurve::Expo.variants(), &[EaseVariant::In, EaseVariant::Out]);
        for c in EaseCurve::ALL {
            for v in c.variants() {
                assert!(
                    is_easing(easing_name(c, *v)),
                    "{c:?}+{v:?} 是下拉里会出现的一格，名字必须合法"
                );
            }
        }
        // 变体不适用时退到 `out`（不是 linear —— 退回线性等于把用户选的曲线也扔了）
        assert_eq!(easing_name(EaseCurve::Quint, EaseVariant::InOut), "outQuint");
        assert_eq!(easing_name(EaseCurve::Expo, EaseVariant::InOut), "outExpo");
        // 下拉里所有格子拼出来的名字**正好是那 29 个**（一个不多一个不少）
        let mut seen: Vec<&str> = Vec::new();
        for curve in EaseCurve::ALL {
            if curve == EaseCurve::Linear {
                seen.push(easing_name(curve, EaseVariant::Out)); // 线性那一格（没有变体可选）
                continue;
            }
            for v in curve.variants() {
                let n = easing_name(curve, *v);
                if !seen.contains(&n) {
                    seen.push(n);
                }
            }
        }
        seen.sort_unstable();
        let mut want: Vec<&str> = easings();
        want.sort_unstable();
        assert_eq!(seen, want, "下拉能选出的名字应正好等于 spec 里的那 29 个");
    }

    /// 认不出的名字：返回 `None`，不去猜一个（界面按"未知"显示原文，不动它）
    #[test]
    fn unknown_names_are_not_guessed() {
        for bad in ["", "linear2", "sine", "InOutSine", "easeInOutQuad", "outSine "] {
            assert!(split_easing(bad).is_none(), "{bad:?} 不该被认成某个缓动");
        }
        // 大小写敏感：RPE 的 `inOut` 就是大写 O
        assert!(split_easing("inoutsine").is_none());
        assert_eq!(
            split_easing("inOutSine"),
            Some((EaseCurve::Sine, Some(EaseVariant::InOut)))
        );
    }

    /// 换曲线之后变体要**夹到这条曲线有的那些**（`quint`/`expo` 没有 `io`）
    #[test]
    fn switching_curve_clamps_the_variant() {
        use EaseCurve as C;
        use EaseVariant as V;
        assert_eq!(C::Sine.clamp_variant(V::InOut), V::InOut, "正弦有 io");
        assert_eq!(C::Quint.clamp_variant(V::InOut), V::Out, "五次没有 io ⇒ 退到 out");
        assert_eq!(C::Expo.clamp_variant(V::InOut), V::Out);
        assert_eq!(C::Quint.clamp_variant(V::In), V::In, "有的变体原样保留");
        assert_eq!(C::Linear.clamp_variant(V::InOut), V::InOut, "线性不看变体");
        // 夹完之后一定拼得出合法名字
        for c in EaseCurve::ALL {
            for v in EaseVariant::ALL {
                let got = easing_name(c, c.clamp_variant(v));
                assert!(is_easing(got), "{c:?}+{v:?} → {got}");
            }
        }
    }

    /// 变体显示用 `in/out/io`（用户口径），名字里拼的是 RPE 的 `in/out/inOut`
    #[test]
    fn variant_labels_follow_the_user_wording() {
        assert_eq!(EaseVariant::In.label(), "in");
        assert_eq!(EaseVariant::Out.label(), "out");
        assert_eq!(EaseVariant::InOut.label(), "io");
        assert_eq!(easing_name(EaseCurve::Quad, EaseVariant::InOut), "inOutQuad");
    }
}
