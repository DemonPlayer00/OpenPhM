// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **RPE 谱面包**（`.pez` / 无压缩文件夹）：`info.yml` + 核心谱面 + 音乐 + 曲绘。
//!
//! 形态依据（不是我们自己发明的）：Phira 的[谱面标准]规定"谱面包是一个压缩包，解压后**直接**包含
//! `info.yml` 与它所指定的其他文件"；[谱面信息]规定 `info.yml` 是 YAML 的 `ChartInfo`，
//! 其中 `chart` 指向谱面文件（**RPE 生成的通常是 `chart.json`**）、`music`/`illustration` 指向
//! 音乐与曲绘的文件名。
//!
//! 为什么照这套而不是"RPE 自己的导出"：RPE（Re:PhiEdit）自身的导出没有公开规范，而上面这套是
//! 生态里**唯一有文档、且能被 Phira 直接导入**的"携带谱面+音乐+曲绘"的 zip 形态 —— 用户要的
//! "参照其导出谱面功能"落到可实现、可验证的地方就是这个。**局限**：`info.yml` 里只写我们真的知道的
//! 字段（不知道的交给导入方的默认值），也不写 `format`（标准明说该字段由客户端识别后写入）。
//!
//! [谱面标准]: https://teamflos.github.io/phira-docs/chart-standard/index.html
//! [谱面信息]: https://teamflos.github.io/phira-docs/chart-standard/chartinfo.html

use std::path::{Path, PathBuf};

use crate::codec::rpe::{self, RpeTarget};
use crate::codec::Fidelity;
use crate::doc::Document;
use crate::zip::Entry;

/// 谱面条目名：RPE 生态里就叫 `chart.json`（Phira 文档：`chart` 默认值）
pub const CHART_NAME: &str = "chart.json";
/// 谱面信息条目名（Phira 标准）
pub const INFO_NAME: &str = "info.yml";
/// 谱面包的默认扩展名：Phira 生态里就是 `.pez`（Phira 自己"忽略后缀名"，但约定俗成是这个）
pub const PACKAGE_EXTENSION: &str = ".pez";

/// YAML 标量：**一律加双引号并转义**。
///
/// 曲名里出现 `:`、`#`、`-`、前导空格这些东西是常态，而"裸标量"在 YAML 里对它们的解释各不相同 ——
/// 全部引起来最省心（`"`/`\` 与 <0x20 的控制字符按 YAML 的双引号转义规则处理）。
fn yaml_scalar(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 谱面包里 `info.yml` 的字段（只列我们**真的知道**的；其余交给导入方的默认值）
#[derive(Clone, Debug, PartialEq)]
pub struct ChartInfo {
    pub name: String,
    /// 数值难度（Phira 的 `difficulty`，f32）
    pub difficulty: f32,
    /// 显示用难度（Phira 的 `level`，例如 `IN Lv.15`）
    pub level: String,
    pub charter: String,
    pub composer: String,
    pub illustrator: String,
    /// 谱面文件名（包内条目名）
    pub chart: String,
    /// 音乐文件名；空 = 包里没有音乐
    pub music: String,
    /// 曲绘文件名；空 = 包里没有曲绘
    pub illustration: String,
    /// 音乐偏移**秒**（注意：RPE 的 `META.offset` 是毫秒）
    pub offset_sec: f64,
}

impl ChartInfo {
    /// 从文档 + 包内实际文件名拼出谱面信息
    pub fn from_doc(doc: &Document, music: String, illustration: String) -> Self {
        let m = &doc.meta;
        // Phigros 侧的两块信息：`difficulty` 是档位名（EZ/HD/IN/AT）、`level` 是数字、`constant` 是定数。
        // Phira 只要"数值难度 + 显示等级"，所以：数值优先取定数，其次取 level，最后兜底 10.0。
        let diff = m
            .constant
            .filter(|c| c.is_finite() && *c > 0.0)
            .or_else(|| m.level.trim().parse::<f32>().ok())
            .unwrap_or(10.0);
        let level = match (m.difficulty.trim(), m.level.trim()) {
            ("", "") => String::new(),
            (d, "") => d.to_owned(),
            ("", l) => l.to_owned(),
            (d, l) => format!("{d} Lv.{l}"),
        };
        Self {
            name: m.name.clone(),
            difficulty: diff,
            level,
            charter: m.charter.clone(),
            composer: m.composer.clone(),
            illustrator: m.illustrator.clone(),
            chart: CHART_NAME.to_owned(),
            music,
            illustration,
            offset_sec: m.offset_ms as f64 / 1000.0,
        }
    }

    /// 渲染成 YAML。空的可选字段**不写**（写空串会让导入方去找一个叫 "" 的文件）
    pub fn to_yaml(&self) -> String {
        let mut s = String::new();
        s.push_str("# OpenPhM 导出的 RPE 谱面包（Phira 谱面标准：info.yml + chart.json + 音乐 + 曲绘）\n");
        s.push_str(&format!("name: {}\n", yaml_scalar(&self.name)));
        s.push_str(&format!("difficulty: {}\n", self.difficulty));
        if !self.level.is_empty() {
            s.push_str(&format!("level: {}\n", yaml_scalar(&self.level)));
        }
        s.push_str(&format!("charter: {}\n", yaml_scalar(&self.charter)));
        s.push_str(&format!("composer: {}\n", yaml_scalar(&self.composer)));
        s.push_str(&format!("illustrator: {}\n", yaml_scalar(&self.illustrator)));
        s.push_str(&format!("chart: {}\n", yaml_scalar(&self.chart)));
        if !self.music.is_empty() {
            s.push_str(&format!("music: {}\n", yaml_scalar(&self.music)));
        }
        if !self.illustration.is_empty() {
            s.push_str(&format!("illustration: {}\n", yaml_scalar(&self.illustration)));
        }
        s.push_str(&format!("offset: {}\n", self.offset_sec));
        s.push_str("previewStart: 0.0\n");
        s.push_str("tags: []\n");
        s.push_str("intro: \"\"\n");
        s.push_str("holdPartialCover: false\n");
        s
    }
}

/// 从 `have` 或 `base_dir` 里找一份资源（包内条目名用**文件名**）
fn take_asset(
    what: &str,
    name: &str,
    have: &[Entry],
    base_dir: Option<&Path>,
    fid: &mut Fidelity,
) -> Option<Entry> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let base = Path::new(name).file_name().and_then(|n| n.to_str()).unwrap_or(name).to_owned();
    // ① 已经在手里（例如刚从别的容器/包里载入）
    if let Some(e) = have
        .iter()
        .find(|e| e.name == name || e.name == base)
        .cloned()
    {
        fid.note(format!("{what} `{}` 在包内（{} KiB）", e.name, e.data.len() / 1024));
        return Some(Entry { name: base, data: e.data });
    }
    // ② 从磁盘读（绝对路径直接用；相对路径相对"目标目录"或当前目录）
    let Some(base_dir) = base_dir else {
        fid.warn(format!("{what} `{name}` 不在手里，也不知道从哪个目录找 —— 包里将缺少它"));
        return None;
    };
    let p: PathBuf = if Path::new(name).is_absolute() { PathBuf::from(name) } else { base_dir.join(name) };
    match std::fs::read(&p) {
        Ok(data) => {
            fid.note(format!("{what} `{name}`（{} KiB）已装入谱面包，包内名 `{base}`", data.len() / 1024));
            Some(Entry { name: base, data })
        }
        Err(e) => {
            fid.warn(format!("{what} `{name}` 读不到（{}）：{e} —— 包里将缺少它", p.display()));
            None
        }
    }
}

/// 组装 RPE 谱面包的全部条目：`info.yml` + `chart.json` + 音乐 + 曲绘。
///
/// `doc` 里的 `meta.audio`/`meta.background` 应当是**已经规范化过的名字**（调用方用
/// `planned_asset_renames` 先改写成包内相对名，和 opm 容器同一条规矩）；这里只负责把它们
/// 读进来、并让 `info.yml` 与 `chart.json` 指向同一批文件名。
pub fn build_entries(
    doc: &Document,
    target: RpeTarget,
    base_dir: Option<&Path>,
    have: &[Entry],
    fid: &mut Fidelity,
) -> Result<Vec<Entry>, String> {
    let (chart_text, mut rpe_fid) = rpe::save_str(doc, target);
    let chart = chart_text.into_bytes();
    // 音乐/曲绘的文件名以 **`chart.json` 里 META 的写法**为准（而不是再读一遍文档）——
    // 这样"info.yml 指向的文件名"与"谱面里引用的文件名"是同一个来源，不会各写各的
    let v: serde_json::Value =
        serde_json::from_slice(&chart).map_err(|e| format!("RPE 谱面序列化后无法解析: {e}"))?;
    let meta = v.get("META").cloned().unwrap_or(serde_json::Value::Null);
    let field = |k: &str| meta.get(k).and_then(|x| x.as_str()).unwrap_or("").to_owned();
    let music = field("song");
    let illustration = field("background");

    let mut entries = Vec::with_capacity(4);
    let info = ChartInfo::from_doc(doc, file_name_of(&music), file_name_of(&illustration));
    for (what, name) in [("音乐", &music), ("曲绘", &illustration)] {
        if let Some(e) = take_asset(what, name, have, base_dir, &mut rpe_fid) {
            entries.push(e);
        }
    }
    entries.push(Entry { name: INFO_NAME.to_owned(), data: info.to_yaml().into_bytes() });
    entries.push(Entry { name: CHART_NAME.to_owned(), data: chart });
    rpe_fid.note(format!("谱面包：`{INFO_NAME}` + `{CHART_NAME}` + {} 个资源", entries.len() - 2));
    *fid = rpe_fid;
    Ok(entries)
}

/// 文件名部分（`a/b/song.ogg` → `song.ogg`）：**两种分隔符都认**（跨平台交换的谱面里出现过 `\`）
fn file_name_of(name: &str) -> String {
    crate::codec::asset_base_name(name)
}

// ---------------------------------------------------------------- 读：`.pez` / 无压缩文件夹

/// `info.yml` 里我们**真的用到**的字段（只解析顶层标量；不引 YAML 库）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InfoFields {
    /// 谱面文件名（Phira 的 `chart`，默认 `chart.json`）
    pub chart: String,
    /// 音乐文件名
    pub music: String,
    /// 曲绘文件名
    pub illustration: String,
}

/// 顶层标量的极简解析：`key: value`（值可带引号）、`#` 注释、空行。
///
/// **只认顶层**：缩进行（嵌套映射/数组的元素）一律跳过 —— 我们需要的 `chart`/`music`/
/// `illustration` 都是 Phira 规定的顶层标量，多余的结构不该在这里被"猜"。
/// 引号包裹的值按 YAML 双引号规则反转义（[`yaml_scalar`] 的逆）。
pub fn parse_info_scalars(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        if raw.starts_with(' ') || raw.starts_with('\t') || raw.trim_start().starts_with('#') {
            continue;
        }
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let key = k.trim();
        if key.is_empty() || key.starts_with('-') {
            continue;
        }
        out.push((key.to_owned(), unquote_scalar(v)));
    }
    out
}

/// 去掉 YAML 标量的引号并反转义（裸标量原样返回，只裁掉两侧空白）
fn unquote_scalar(v: &str) -> String {
    let v = v.trim();
    let Some(inner) = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return v.to_owned();
    };
    let mut out = String::with_capacity(inner.len());
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('u') => {
                let hex: String = it.by_ref().take(4).collect();
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None => out.push_str(&format!("\\u{hex}")),
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 从**已经解开的条目**读 RPE 谱面包（`.pez` zip 与 RPE 无压缩文件夹共用）。
///
/// 为什么要它：`.pez` 之前**只写得出来、读不回去**（`chart.json` 被当成 opm 谱面解析 ⇒
/// `format 必须是 "opm"`）。谱面包是导出形态，导出形态必须能自己读回来 —— 否则"导出一份
/// 给别人"之后，自己都验不了它。
pub fn read_entries(
    entries: Vec<Entry>,
    fid: &mut Fidelity,
) -> Result<(Document, Vec<Entry>), String> {
    let info_at = entries
        .iter()
        .position(|e| e.name == INFO_NAME)
        .ok_or_else(|| format!("不是 RPE 谱面包（找不到 `{INFO_NAME}`）"))?;
    let info_text = String::from_utf8_lossy(&entries[info_at].data).into_owned();
    let fields = parse_info_scalars(&info_text);
    let get = |k: &str| {
        fields
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let info = InfoFields {
        chart: get("chart"),
        music: get("music"),
        illustration: get("illustration"),
    };
    // 谱面条目：`info.yml` 指的那个；没写就找 `chart.json`；再不行找唯一的 `*.json`
    let chart_at = entries
        .iter()
        .position(|e| !info.chart.trim().is_empty() && e.name == info.chart)
        .or_else(|| entries.iter().position(|e| e.name == CHART_NAME))
        .or_else(|| {
            let mut it = entries
                .iter()
                .enumerate()
                .filter(|(i, e)| *i != info_at && e.name.ends_with(".json"));
            match (it.next(), it.next()) {
                (Some((i, _)), None) => Some(i),
                _ => None,
            }
        })
        .ok_or_else(|| {
            format!(
                "谱面包里没有谱面（`{INFO_NAME}` 写着 `{}`；实际条目：{}）",
                info.chart,
                entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(", ")
            )
        })?;
    if !info.chart.trim().is_empty() && entries[chart_at].name != info.chart {
        fid.warn(format!(
            "`{INFO_NAME}` 指向 `{}`，包里没有它 —— 已按 `{}` 读取",
            info.chart, entries[chart_at].name
        ));
    }
    let (mut doc, rpe_fid) = crate::codec::to_document(
        serde_json::from_slice(&entries[chart_at].data)
            .map_err(|e| format!("`{}` 不是合法 JSON: {e}", entries[chart_at].name))?,
    )?;
    *fid = rpe_fid;
    fid.note(format!(
        "RPE 谱面包：`{INFO_NAME}` + `{}` + {} 个资源",
        entries[chart_at].name,
        entries.len().saturating_sub(2)
    ));
    // `info.yml` 里写着、而谱面本体里没写的资源名：补上（`meta.audio`/`meta.background`
    // 决定"按哪个文件名去找音乐"，缺了就会"包里有音乐却说不认识"）
    for (what, field, from_info, cur) in [
        ("音乐", "audio", info.music.clone(), doc.meta.audio.clone()),
        ("曲绘/背景", "background", info.illustration.clone(), doc.meta.background.clone()),
    ] {
        let from_info = file_name_of(&from_info);
        if cur.as_deref().map(str::trim).unwrap_or("").is_empty() && !from_info.is_empty() {
            fid.note(format!("{what}：按 `{INFO_NAME}` 的写法补成 `{from_info}`"));
            match field {
                "audio" => doc.meta.audio = Some(from_info),
                _ => doc.meta.background = Some(from_info),
            }
        }
    }
    let assets: Vec<Entry> = entries
        .into_iter()
        .enumerate()
        .filter(|(i, _)| *i != info_at && *i != chart_at)
        .map(|(_, e)| e)
        .collect();
    for (what, want) in [("音乐", doc.meta.audio.as_deref()), ("曲绘/背景", doc.meta.background.as_deref())] {
        if let Some(name) = want.map(str::trim).filter(|n| !n.is_empty()) {
            match assets.iter().find(|a| a.name == name) {
                Some(a) => fid.note(format!("{what} `{name}` 在包内（{} KiB）", a.data.len() / 1024)),
                None => fid.warn(format!("{what} `{name}` **不在包内**（包里没有这个条目）")),
            }
        }
    }
    fid.finalize();
    Ok((doc, assets))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 标量一律引号 + 转义：曲名里的 `:`/`#`/引号/换行都不该把 YAML 结构搞坏
    #[test]
    fn yaml_scalars_are_always_quoted_and_escaped() {
        assert_eq!(yaml_scalar("plain"), "\"plain\"");
        assert_eq!(yaml_scalar("a: b"), "\"a: b\"");
        assert_eq!(yaml_scalar("#not-a-comment"), "\"#not-a-comment\"");
        assert_eq!(yaml_scalar("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(yaml_scalar("c:\\path"), "\"c:\\\\path\"");
        assert_eq!(yaml_scalar("two\nlines"), "\"two\\nlines\"");
        assert_eq!(yaml_scalar("tab\there"), "\"tab\\there\"");
        assert_eq!(yaml_scalar("\u{1}"), "\"\\u0001\"");
        // 中文与 emoji 原样留着（YAML 是 UTF-8）
        assert_eq!(yaml_scalar("曲名🎵"), "\"曲名🎵\"");
    }

    /// `info.yml` 的字段映射：数值难度取定数 → 其次 level → 兜底 10.0；
    /// `level` 拼成 `IN Lv.15`；偏移从毫秒换成秒
    #[test]
    fn chart_info_maps_phigros_fields_onto_phira_fields() {
        let meta = crate::doc::Meta {
            name: String::new(),
            composer: String::new(),
            charter: String::new(),
            illustrator: String::new(),
            difficulty: "IN".to_owned(),
            level: String::new(),
            constant: None,
            offset_ms: 0,
            audio: None,
            background: None,
            foreign: Default::default(),
        };
        let mut doc = Document::fresh(meta, 174.0);
        doc.meta.name = "曲名: 带冒号".to_owned();
        doc.meta.charter = "我".to_owned();
        doc.meta.composer = "某人".to_owned();
        doc.meta.illustrator = "画师".to_owned();
        doc.meta.difficulty = "IN".to_owned();
        doc.meta.level = "15".to_owned();
        doc.meta.constant = Some(15.6);
        doc.meta.offset_ms = -250;
        let info = ChartInfo::from_doc(&doc, "song.ogg".to_owned(), "bg.png".to_owned());
        assert_eq!(info.difficulty, 15.6, "数值难度优先取定数");
        assert_eq!(info.level, "IN Lv.15");
        assert!((info.offset_sec + 0.25).abs() < 1e-9, "毫秒 → 秒：{}", info.offset_sec);
        let y = info.to_yaml();
        assert!(y.contains("name: \"曲名: 带冒号\""), "{y}");
        assert!(y.contains("chart: \"chart.json\""), "{y}");
        assert!(y.contains("music: \"song.ogg\""), "{y}");
        assert!(y.contains("illustration: \"bg.png\""), "{y}");
        assert!(y.contains("difficulty: 15.6"), "{y}");
        assert!(y.contains("level: \"IN Lv.15\""), "{y}");
        // 不知道的字段不写（`format` 由客户端识别后写入，标准明说不该手填）
        assert!(!y.contains("format:"), "{y}");

        // 没有定数时退回 level 的数字；两块都空时留空（不写 level 行）
        doc.meta.constant = None;
        assert_eq!(ChartInfo::from_doc(&doc, String::new(), String::new()).difficulty, 15.0);
        doc.meta.difficulty = String::new();
        doc.meta.level = String::new();
        let info = ChartInfo::from_doc(&doc, String::new(), String::new());
        assert_eq!(info.difficulty, 10.0);
        assert!(info.level.is_empty());
        let y = info.to_yaml();
        assert!(!y.contains("level:"), "{y}");
        // 没有音乐/曲绘时不写那两行（写空串会让导入方去找一个叫 "" 的文件）
        assert!(!y.contains("music:"), "{y}");
        assert!(!y.contains("illustration:"), "{y}");
    }

    /// 文件名提取：外部绝对路径也要落到**包内文件名**（容器是自包含的）
    #[test]
    fn asset_names_collapse_to_base_names() {
        assert_eq!(file_name_of("/tmp/x/song.ogg"), "song.ogg");
        assert_eq!(file_name_of("bg.png"), "bg.png");
        assert_eq!(file_name_of("  "), "");
        assert_eq!(file_name_of(r"C:\m\a.mp3"), "a.mp3");
    }

    /// `info.yml` 的极简解析：顶层标量 + 引号反转义；缩进/注释/数组行都不是字段
    #[test]
    fn info_scalars_are_parsed_from_the_top_level_only() {
        let text = "# 注释\n\
                    name: \"曲名: 带冒号\"\n\
                    chart: \"chart.json\"\n\
                    music: song.ogg\n\
                    nested:\n  chart: \"不该被当成字段\"\n\
                    tags: []\n\
                    - 数组项: x\n\
                    escaped: \"a\\\"b\\\\c\\n\"\n";
        let f = parse_info_scalars(text);
        let get = |k: &str| f.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert_eq!(get("name").as_deref(), Some("曲名: 带冒号"));
        assert_eq!(get("chart").as_deref(), Some("chart.json"));
        assert_eq!(get("music").as_deref(), Some("song.ogg"), "裸标量原样保留");
        assert_eq!(get("tags").as_deref(), Some("[]"));
        assert_eq!(get("escaped").as_deref(), Some("a\"b\\c\n"));
        assert_eq!(get("chart"), Some("chart.json".to_owned()), "缩进行里的 chart 不算");
    }

    /// **写得出去就要读得回来**：`build_entries` 组出来的谱面包，`read_entries` 能原样认出来
    /// （`.pez` 曾经只导得出去、读不回来 —— `chart.json` 被当 opm 谱面解析）
    #[test]
    fn a_built_package_reads_back() {
        let doc = Document::fresh(
            crate::doc::Meta {
                name: "包内曲名".to_owned(),
                composer: "某人".to_owned(),
                charter: "我".to_owned(),
                illustrator: String::new(),
                difficulty: "IN".to_owned(),
                level: String::new(),
                constant: None,
                offset_ms: 0,
                audio: Some("song.ogg".to_owned()),
                background: Some("bg.png".to_owned()),
                foreign: Default::default(),
            },
            174.0,
        );
        let have = vec![
            Entry { name: "song.ogg".to_owned(), data: b"OggS-x".to_vec() },
            Entry { name: "bg.png".to_owned(), data: b"\x89PNG-x".to_vec() },
        ];
        let mut wfid = Fidelity::new("rpe", "谱面包（zip）".to_owned());
        let entries = build_entries(&doc, RpeTarget::default(), None, &have, &mut wfid).unwrap();
        let mut rfid = Fidelity::new("rpe", "谱面包（zip）".to_owned());
        let (back, assets) = read_entries(entries, &mut rfid).unwrap();
        assert_eq!(back.meta.name, "包内曲名");
        assert_eq!(back.meta.charter, "我");
        assert_eq!(back.meta.audio.as_deref(), Some("song.ogg"), "音乐名从谱面本体带回来");
        assert_eq!(back.meta.background.as_deref(), Some("bg.png"));
        let names: Vec<&str> = assets.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["song.ogg", "bg.png"], "谱面与 info.yml 都不算资源");
        assert!(rfid.report().contains("谱面包"), "{}", rfid.report());
    }
}
