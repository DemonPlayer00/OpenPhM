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

pub const EASINGS: [&str; 29] = [
    "linear", "outSine", "inSine", "outQuad", "inQuad", "inOutSine", "inOutQuad", "outCubic",
    "inCubic", "outQuart", "inQuart", "inOutCubic", "inOutQuart", "outQuint", "inQuint", "outExpo",
    "inExpo", "outCirc", "inCirc", "outBack", "inBack", "inOutCirc", "inOutBack", "outElastic",
    "inElastic", "outBounce", "inBounce", "inOutBounce", "inOutElastic",
];

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
                    if !EASINGS.contains(&e.easing.as_str()) {
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
