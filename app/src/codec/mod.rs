// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! 格式编解码层（codec）：编辑器只认 opm 数据模型，格式差异全部收敛在这里。
//!
//! 三条规范要求（`spec/opm-format.md` §8–§9）在这里落地：
//! 1. **枚举表的单一数据源**是 `spec/note-types.json` 与 `spec/easing.json` —— 编译期 `include_str!`
//!    读进来（不是代码里再写一份 switch），因为"四套音符枚举互不相同、读错不报错"是这个生态最贵的坑；
//! 2. **导入侧负责规范化**：补空隙（前值延拓）、裁重叠、首事件从拍 0 起、末事件延拓到谱面结束
//!    —— opm 要求轨道无空隙无重叠，这是 codec 的职责，不是校验器放行的理由；
//! 3. **必须产出保真度报告**：转换了什么、丢了什么，都要写出来（导出侧尤其）。

pub mod container;
pub mod package;
pub mod rpe;

/// 资源名 → **包内文件名**（同时按 `/` 与 `\` 切分）。
///
/// 为什么要两种分隔符：谱面是**跨平台交换**的 —— Windows 上作者写的 `music\song.mp3` 到了 Linux
/// （或反过来）也得认出文件名，否则打包时会把整串当文件名：既读不到文件，又会把宿主机的目录结构
/// 带进包里。`Path::file_name()` 只认本机的分隔符，所以这里不能直接用它。
pub fn asset_base_name(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return String::new();
    }
    name.rsplit(['/', '\\']).next().unwrap_or(name).to_owned()
}

#[cfg(test)]
mod base_name_tests {
    use super::asset_base_name;

    /// 两种分隔符、重复分隔符、尾部分隔符、空串
    #[test]
    fn asset_base_name_handles_both_separators() {
        assert_eq!(asset_base_name("song.ogg"), "song.ogg");
        assert_eq!(asset_base_name("/tmp/x/song.ogg"), "song.ogg");
        assert_eq!(asset_base_name(r"C:\music\song.ogg"), "song.ogg");
        assert_eq!(asset_base_name(r"mixed/dir\song.ogg"), "song.ogg");
        assert_eq!(asset_base_name("  bg.png  "), "bg.png");
        assert_eq!(asset_base_name(""), "");
        // 尾部分隔符 ⇒ 那是个目录、不是文件：给空串（调用方据此当"没资源"处理）
        assert_eq!(asset_base_name("dir/"), "");
        assert_eq!(asset_base_name(r"dir\"), "");
    }
}

use std::sync::OnceLock;

use serde_json::Value;

use crate::doc::{Beat, Document, NoteKind};

// ---------------------------------------------------------------- 保真度报告

/// 一次导入/导出的保真度报告。
///
/// `conversions` 是**做了什么**（信息），`warnings` 是**哪里降级/丢东西了**（需要人看）。
/// 只要 `warnings` 非空就不算无损 —— 别把"能打开"当成"没丢东西"。
#[derive(Clone, Debug, Default)]
pub struct Fidelity {
    pub source: String,
    pub version: String,
    pub conversions: Vec<String>,
    pub warnings: Vec<String>,
    /// **同类问题的合并计数**：真实谱面里每条线都有 `extended`/`*Control`，
    /// 逐条报警告会得到 34 行几乎一样的文字（Prismatic 实测），把真正该看的那几条淹掉。
    /// 这里按类别累计，`finalize()` 时合并成一行（带首次出现的指针）。
    groups: Vec<(String, String, usize, String)>,
}

impl Fidelity {
    pub fn new(source: &str, version: String) -> Self {
        Self {
            source: source.to_owned(),
            version,
            conversions: Vec::new(),
            warnings: Vec::new(),
            groups: Vec::new(),
        }
    }
    pub fn note(&mut self, s: impl Into<String>) {
        self.conversions.push(s.into());
    }
    pub fn warn(&mut self, s: impl Into<String>) {
        self.warnings.push(s.into());
    }
    /// 同类警告合并计数（`key` 是类别名，`ptr` 是首次出现的 JSON 指针）
    pub fn warn_grouped(&mut self, key: &str, ptr: &str) {
        self.warn_grouped_note(key, ptr, "opm v1 未建模，已原样保留");
    }
    /// 同上，但**自己写说明**：有些合并警告说的不是"未建模"，而是"已建模、按另一种口径求值"
    /// （例：流速事件的缓动按 linear 求值，缓动名原样保留）。
    pub fn warn_grouped_note(&mut self, key: &str, ptr: &str, note: &str) {
        match self.groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.2 += 1,
            None => self.groups.push((key.to_owned(), ptr.to_owned(), 1, note.to_owned())),
        }
    }
    /// 把合并计数落成警告行（导入/导出结束时各调一次）
    pub fn finalize(&mut self) {
        let groups = std::mem::take(&mut self.groups);
        for (key, ptr, n, note) in groups {
            self.warnings
                .push(format!("{key}：共 {n} 处（首次于 {ptr}）—— {note}"));
        }
    }
    pub fn is_lossless(&self) -> bool {
        self.warnings.is_empty()
    }
    pub fn merge(&mut self, other: &Fidelity) {
        self.conversions.extend(other.conversions.iter().cloned());
        self.warnings.extend(other.warnings.iter().cloned());
        self.groups.extend(other.groups.iter().cloned());
    }
    /// 人类可读报告（CLI 直接打印；GUI 控制台也用它）
    pub fn report(&self) -> String {
        let mut s = format!("{} {}：{} 项转换", self.source, self.version, self.conversions.len());
        if self.warnings.is_empty() {
            s.push_str("，**无降级**");
        } else {
            s.push_str(&format!("，{} 项降级：", self.warnings.len()));
            for w in &self.warnings {
                s.push_str("\n  ⚠ ");
                s.push_str(w);
            }
        }
        if !self.conversions.is_empty() {
            s.push_str("\n  转换明细：");
            for c in &self.conversions {
                s.push_str("\n  · ");
                s.push_str(c);
            }
        }
        s
    }
}

// ---------------------------------------------------------------- 枚举表（单一数据源）

/// `spec/easing.json` 的 (id, name) 表 —— RPE `easingType` 编号与具名缓动一一对应。
fn easing_table() -> &'static Vec<(i64, String)> {
    static T: OnceLock<Vec<(i64, String)>> = OnceLock::new();
    T.get_or_init(|| {
        let v: Value = serde_json::from_str(include_str!("../../../spec/easing.json"))
            .expect("spec/easing.json 必须是合法 JSON");
        v.get("easings")
            .and_then(|e| e.as_array())
            .expect("spec/easing.json 缺少 easings 数组")
            .iter()
            .filter_map(|e| {
                let id = e.get("id")?.as_i64()?;
                let name = e.get("name")?.as_str()?.to_owned();
                Some((id, name))
            })
            .collect()
    })
}

/// RPE `easingType` → opm 缓动名（不在表里返回 None，调用方按线性处理并报告）
pub fn easing_name_of_rpe(id: i64) -> Option<&'static str> {
    easing_table().iter().find(|(i, _)| *i == id).map(|(_, n)| n.as_str())
}

/// opm 缓动名 → RPE `easingType`
pub fn rpe_id_of_easing(name: &str) -> Option<i64> {
    easing_table().iter().find(|(_, n)| n == name).map(|(i, _)| *i)
}

/// 缓动总数（测试与文档引用；与 `spec/easing.json` 同步）
pub fn easing_count() -> usize {
    easing_table().len()
}

/// 全部缓动名，按 `spec/easing.json` 里的 `id` 升序。
///
/// **命令层的"合法缓动"就该用它**：那边曾经手抄一份 29 个名字的常量表 ——
/// 两份表在"加第 30 个缓动"那天必然分家，而症状是"RPE 导入认它、`set_event` 不认它"
/// （或者反过来），并且只在真的有人用那个缓动时才看得出来。
pub fn easing_names() -> Vec<&'static str> {
    easing_table().iter().map(|(_, n)| n.as_str()).collect()
}

/// `spec/note-types.json` 里某个格式的 名字→整数 表
fn note_type_table(format: &str) -> &'static Vec<(String, i64)> {
    static T: OnceLock<std::collections::HashMap<String, Vec<(String, i64)>>> = OnceLock::new();
    let all = T.get_or_init(|| {
        let v: Value = serde_json::from_str(include_str!("../../../spec/note-types.json"))
            .expect("spec/note-types.json 必须是合法 JSON");
        let mut out = std::collections::HashMap::new();
        if let Some(fmts) = v.get("formats").and_then(|f| f.as_object()) {
            for (name, table) in fmts {
                let mut rows: Vec<(String, i64)> = Vec::new();
                if let Some(o) = table.as_object() {
                    for (kind, num) in o {
                        if let Some(n) = num.as_i64() {
                            rows.push((kind.clone(), n));
                        }
                    }
                }
                rows.sort_by_key(|(_, n)| *n);
                out.insert(name.clone(), rows);
            }
        }
        out
    });
    all.get(format).unwrap_or_else(|| panic!("spec/note-types.json 缺少格式 {format}"))
}

/// RPE 音符整数 → opm `NoteKind`。**缺失/未知按 tap**（RPE 表记 Default = Tap）。
pub fn note_kind_from_rpe(v: i64) -> NoteKind {
    note_type_table("rpe")
        .iter()
        .find(|(_, n)| *n == v)
        .and_then(|(k, _)| NoteKind::parse(k))
        .unwrap_or(NoteKind::Tap)
}

/// opm `NoteKind` → RPE 音符整数（**不是**官谱的那套，别混）
pub fn note_kind_to_rpe(k: NoteKind) -> i64 {
    note_type_table("rpe")
        .iter()
        .find(|(name, _)| name == k.as_str())
        .map(|(_, n)| *n)
        .unwrap_or(1)
}

/// 对照用：官谱整数（测试矩阵要四种格式都比一遍）
pub fn note_kind_from_official(v: i64) -> Option<NoteKind> {
    note_type_table("phigros-official")
        .iter()
        .find(|(_, n)| *n == v)
        .and_then(|(k, _)| NoteKind::parse(k))
}

// ---------------------------------------------------------------- 条目的"哪一种谱面"

/// 一串条目属于**哪种谱面**：opm 容器，还是 RPE 谱面包（Phira 的 `.pez`）。
///
/// 判据只有一条：**看有没有 `info.yml`**（RPE 谱面包的清单文件，opm 容器没有）。
///
/// 为什么值得单独一个类型：这条判断以前写在两处（从**文件**开、从**目录**开），
/// 而两处必须永远一致 —— 分错的后果不是"读得差一点"，是**整份文档按错格式解析**；
/// "谱面本体叫什么名字"（`opm.json` / `chart.json`）也跟着它走，那个三元表达式同样各写了一遍。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// opm 容器 / 无压缩文件夹
    Opm,
    /// RPE 谱面包
    RpePackage,
}

impl EntryKind {
    /// 从条目名判断（**唯一判据**）
    pub fn of(entries: &[crate::zip::Entry]) -> EntryKind {
        if entries.iter().any(|e| e.name == package::INFO_NAME) {
            EntryKind::RpePackage
        } else {
            EntryKind::Opm
        }
    }
    /// 谱面本体在这类包里的名字（摊成目录、恢复保存目标时要用）
    pub fn chart_name(self) -> &'static str {
        match self {
            EntryKind::Opm => container::CHART_NAME,
            EntryKind::RpePackage => package::CHART_NAME,
        }
    }
    /// 按这一类读出 `(文档, 资源)`
    pub fn read(
        self,
        entries: Vec<crate::zip::Entry>,
        fid: &mut Fidelity,
    ) -> Result<(Document, Vec<crate::zip::Entry>), String> {
        match self {
            EntryKind::Opm => {
                let c = container::read_entries(entries, fid)?;
                Ok((c.doc, c.assets))
            }
            EntryKind::RpePackage => package::read_entries(entries, fid),
        }
    }
}

// ---------------------------------------------------------------- 拍 ↔ 数值

/// 浮点拍 → 精确有理拍。
///
/// 分母上限 1_000_000 再约分：RPE 里的 `4.25` 精确还原成 `17/4`，而 `3.333333` 这类
/// 本来就带浮点噪声的值会得到一个**接近但精确**的有理数（再由 `Beat::new` 约分）。
/// 为什么不直接存浮点：opm 的拍是有理数，导入→导出来回一趟不能漂。
pub fn beat_from_f64(x: f64) -> Beat {
    if !x.is_finite() {
        return Beat::zero();
    }
    const DEN: f64 = 1_000_000.0;
    let n = (x * DEN).round();
    Beat::new(n as i64, DEN as i64)
}

/// RPE 的时间值 → 拍。接受两种写法：
/// - 数字（笔记与 `BPMList` 常见）：按拍浮点；
/// - 三元组 `[b0, b1, b2]`（事件的规范写法）：`b0 + b1 / b2`，**精确**。
pub fn beat_from_value(v: &Value) -> Result<Beat, String> {
    match v {
        Value::Number(n) => Ok(beat_from_f64(n.as_f64().unwrap_or(0.0))),
        Value::Array(a) if a.len() == 3 => {
            let b0 = a[0].as_i64().ok_or("时间三元组 [0] 不是整数")?;
            let b1 = a[1].as_i64().ok_or("时间三元组 [1] 不是整数")?;
            let b2 = a[2].as_i64().ok_or("时间三元组 [2] 不是整数")?;
            if b2 == 0 {
                return Err("时间三元组分母为 0".to_owned());
            }
            // b0 + b1/b2 = (b0*b2 + b1) / b2
            Ok(Beat::new(b0 * b2 + b1, b2))
        }
        Value::Array(a) if a.len() == 2 => {
            // 宽容：有些工具写 [分子, 分母]
            let n = a[0].as_i64().ok_or("时间二元组 [0] 不是整数")?;
            let d = a[1].as_i64().ok_or("时间二元组 [1] 不是整数")?;
            if d == 0 {
                return Err("时间二元组分母为 0".to_owned());
            }
            Ok(Beat::new(n, d))
        }
        other => Err(format!("时间字段类型不认识：{other}")),
    }
}

/// 有理拍 → RPE 的三元组 `[整拍, 余数, 分母]`（**精确**，不经过浮点）
pub fn beat_to_triple(b: Beat) -> Value {
    let (n, d) = (b.n, b.d);
    let whole = n.div_euclid(d);
    let rem = n.rem_euclid(d);
    serde_json::json!([whole, rem, d])
}

// ---------------------------------------------------------------- 规范化

/// 一条**常量**事件 `[from, to)`，值取 `v`（规范化里所有"补一段"都用它）。
///
/// 单独一个构造函数是为了让"补的是常量事件"这件事在所有调用点长得一样 ——
/// 测试与保真度报告都依赖"常量"这个性质（拉长它才是无损的）。
pub fn const_hold(from: crate::doc::Beat, to: crate::doc::Beat, v: Value) -> crate::doc::Event {
    crate::doc::Event::new(from, to, v.clone(), v, "linear")
}

/// 轨道规范化的统计（给保真度报告用）
#[derive(Clone, Copy, Debug, Default)]
pub struct NormalizeStats {
    pub sorted: bool,
    pub dropped: usize,
    pub gaps: usize,
    pub overlaps: usize,
    pub prepended: bool,
    pub extended_to_end: bool,
}

/// 把"某格式语义下的事件轨道"规范化成 opm 轨道 —— **全工程唯一的规范化规则**。
///
/// 语义依据（RPE 原义，也是**运行时求值**的口径，见 `perf::active_event`）：
/// · **空隙**：保持前一条事件的终值（解析延拓）；
/// · **重叠**：后一条事件从**它的起点**起覆盖前一条（前一条到此为止，起点不被挪动）；
/// · **末尾之后**：一直保持末事件的终值。
///
/// 于是规范化就是：排序 → 丢零长度 → 相邻处把**前一条的 `end`** 挪到下一个 `start` →
/// 首个事件从拍 0 起（前面补一条常量事件）→ 末事件延拓到谱面结束。
///
/// **保值**是硬要求：一条**斜坡**事件（起值 ≠ 终值）被拉长会把斜率改掉 ——
/// 那等于"重新加载之后谱面动得更慢"。所以延长只对**常量**事件直接改 `end`，
/// 斜坡一律"原样 + 追加一条常量事件"（值取它的终值）。空隙与末尾延拓都走这条规则。
///
/// `ptr` 是给报告用的 JSON 指针前缀（例如 `/judgeLineList[0].eventLayers[1].moveXEvents`）。
pub fn normalize_track(
    mut events: Vec<crate::doc::Event>,
    tmap: &crate::perf::TimeMap,
    chart_end: Beat,
    ptr: &str,
    fid: &mut Fidelity,
) -> (Vec<crate::doc::Event>, NormalizeStats) {
    let mut st = NormalizeStats::default();
    let before = events.len();
    let already_sorted = events.windows(2).all(|w| w[0].start <= w[1].start);
    events.sort_by(|a, b| a.start.cmp(&b.start));
    st.sorted = !already_sorted;

    // 丢零长度/倒挂事件（opm 错误规则 6：endBeat ≤ startBeat）
    events.retain(|e| e.end > e.start);
    st.dropped = before - events.len();

    let mut out: Vec<crate::doc::Event> = Vec::with_capacity(events.len() + 1);
    for e in events {
        let ns = e.start.to_f64();
        // 上一条要止于哪里 —— 常量事件直接改 `end`（无损），斜坡要保住它**到该点为止的值**
        if let Some(prev) = out.last().cloned() {
            let pe = prev.end.to_f64();
            if (ns - pe).abs() > 1e-9 {
                if ns > pe {
                    // 空隙：**前值延拓**到后一条的起点。拉长常量事件无损；
                    // 斜坡拉长会把斜率改掉（= "重载之后谱面变慢"），所以补一条常量段
                    st.gaps += 1;
                    if prev.start_value == prev.end_value {
                        out.last_mut().expect("刚读过 last").end = e.start;
                    } else {
                        out.push(const_hold(prev.end, e.start, prev.end_value.clone()));
                    }
                } else {
                    // 重叠：前一条止于后一条的起点，**并把终值改成它在该点的值**
                    // （同一条斜坡在更短的跨度上重新插值会把斜率改掉）
                    st.overlaps += 1;
                    let cut = serde_json::json!(crate::perf::event_value(&prev, ns, tmap));
                    let trimmed = out.last_mut().expect("刚读过 last");
                    trimmed.end = e.start;
                    trimmed.end_value = cut;
                    if trimmed.end <= trimmed.start {
                        out.pop(); // 裁成零长度 ⇒ 丢掉（opm 不允许零长度事件）
                    }
                }
            }
        }
        out.push(e);
    }
    // 首事件必须从拍 0（或更早）起：前面补一条常量事件，避免"谱面开头没有事件"
    if let Some(first) = out.first().cloned() {
        if first.start > Beat::zero() {
            let hold = const_hold(Beat::zero(), first.start, first.start_value.clone());
            out.insert(0, hold);
            st.prepended = true;
        }
    }
    // 末事件延拓到谱面结束（否则官谱语义下会"谱面停顿"，校验器报错）—— 同样要**保值**
    if out.last().is_some_and(|last| last.end < chart_end) {
        let last = out.last().cloned().expect("刚判过非空");
        if last.start_value == last.end_value {
            out.last_mut().expect("刚判过非空").end = chart_end;
        } else {
            out.push(const_hold(last.end, chart_end, last.end_value.clone()));
        }
        st.extended_to_end = true;
    }

    if st.dropped > 0 {
        fid.warn(format!("{ptr}：丢弃 {} 条零长度/倒挂事件（endBeat ≤ startBeat）", st.dropped));
    }
    if st.gaps > 0 {
        fid.note(format!("{ptr}：{} 处空隙按「前值延拓」补齐（RPE 原义）", st.gaps));
    }
    if st.overlaps > 0 {
        fid.warn(format!("{ptr}：{} 处重叠已裁剪（保留后一条事件的起点）", st.overlaps));
    }
    if st.prepended {
        fid.note(format!("{ptr}：首事件晚于拍 0，已补一条常量事件"));
    }
    if st.extended_to_end {
        fid.note(format!("{ptr}：末事件延拓到谱面结束（拍 {}）", chart_end.to_f64()));
    }
    (out, st)
}

// ---------------------------------------------------------------- 格式检测

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// 裸 opm（`.opm.json`）：可 diff、可入版本库、agent 与测试都用它
    Opm,
    /// **opm 容器**（`.opm`）：ZIP，里面是谱面 + 音乐 + 曲绘等资源（分发形态）
    OpmZip,
    Rpe,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "opm" => Some(Format::Opm),
            "opmz" | "opm-zip" | "container" => Some(Format::OpmZip),
            // `as_str()` 写出来的是给人看的中文名 —— 至少要能读回来（缓存里的会话元数据就存它）
            "opm 容器" => Some(Format::OpmZip),
            "rpe" => Some(Format::Rpe),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Opm => "opm",
            Format::OpmZip => "opm 容器",
            Format::Rpe => "rpe",
        }
    }

    /// 这种格式的**扩展名**（单一出处）。
    ///
    /// 界面上三处需要它：对话框里的提示文字、`另存为` 的预填文件名、系统框回来后的补全。
    /// 原先 GUI 自己写了一份匹配，而且提示文字还跟它对不上（提示说"opm → `.opm.json`"，
    /// 实际写出的是 `.opm`）—— "写哪种格式"与"扩展名"必须是同一份判据。
    pub fn extension(self) -> &'static str {
        match self {
            Format::Opm => ".opm.json", // 裸工程文件：可 diff、可入版本库
            Format::OpmZip => ".opm",    // 容器：一个文件带走谱面 + 音乐 + 曲绘
            Format::Rpe => ".json",
        }
    }
}

/// 按 JSON 结构判断格式（**不靠扩展名**：两种格式都是 `.json`，扩展名靠不住）。
///
/// opm：有 `format == "opm"`；RPE：有 `judgeLineList` 或 `BPMList` 且不是 opm。
pub fn detect(v: &Value) -> Option<Format> {
    if v.get("format").and_then(|f| f.as_str()) == Some("opm") {
        return Some(Format::Opm);
    }
    if v.get("judgeLineList").is_some() || v.get("BPMList").is_some() {
        return Some(Format::Rpe);
    }
    None
}

/// 统一入口：把任意受支持格式的 JSON 变成 opm 文档
pub fn to_document(v: Value) -> Result<(Document, Fidelity), String> {
    match detect(&v) {
        Some(Format::Opm) => {
            let doc = Document::from_json(v)?;
            let mut fid = Fidelity::new("opm", format!("v{}", doc.format_version));
            fid.note("原生格式，无转换");
            Ok((doc, fid))
        }
        Some(Format::Rpe) => {
            let r = rpe::load_value(v)?;
            Ok((r.doc, r.fidelity))
        }
        // 容器走的是**字节**那条路（`load_bytes`），不该到这里 —— 真到了说明调用方跳过了分流
        Some(Format::OpmZip) => Err(
            "这是 opm 容器（ZIP），要用 `codec::load_bytes`/`load_file` 读；`to_document` 只吃 JSON"
                .to_owned(),
        ),
        None => Err(
            "无法识别的谱面格式：既没有 opm 的 `format: \"opm\"`，也没有 RPE 的 `judgeLineList`/`BPMList`"
                .to_owned(),
        ),
    }
}

/// 从文件读入（按内容判断格式：ZIP 魔数 ⇒ 容器；否则当 JSON 解析）
///
/// **编辑器/转换工具请走 `EditCore::stage_file`** —— 那条会把资源一起带进临时目录、
/// 并认得 `.pez` 与无压缩文件夹；这里的 `load_file` 只回答"这份 JSON 是什么文档"，
/// 适合"只要文档本体"的一次性用途。
pub fn load_file(path: &std::path::Path) -> Result<(Document, Fidelity), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读取失败: {e}"))?;
    load_bytes(&bytes)
}

/// 从字节读入。**容器（zip）与裸 JSON 都在这里分流** —— 上层（GUI/CLI）不必知道文件是哪种。
pub fn load_bytes(bytes: &[u8]) -> Result<(Document, Fidelity), String> {
    if crate::zip::looks_like_zip(bytes) {
        let (c, fid) = container::read(bytes)?;
        return Ok((c.doc, fid));
    }
    let text = std::str::from_utf8(bytes).map_err(|e| format!("既不是 ZIP 也不是 UTF-8 JSON: {e}"))?;
    let v: Value = serde_json::from_str(text).map_err(|e| format!("JSON 解析失败: {e}"))?;
    to_document(v)
}
