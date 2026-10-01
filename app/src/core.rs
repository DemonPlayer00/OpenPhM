// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! 编辑核心：**CLI 与 GUI 共享的同一个编辑会话**，也是唯一的可写方。
//!
//! 数据流（单向）：
//! ```text
//!   GUI / opm-ctl ──命令──▶ EditCore ──改动完成──▶ update 广播（带细粒度话题）
//!                                                  │
//!                                                  ├─▶ 话题命中的订阅者才收到
//!                                                  └─▶ 未命中者一帧都不重建
//! ```
//! · GUI 持有 `Arc<Mutex<EditCore>>`，**自己不碰文档**：只发命令、等广播，收到广播后按话题重建对应面板；
//! · CLI 可以在进程内直接建 `EditCore`（无头批处理），也可以通过控制通道（见 `control.rs`）
//!   **接进正在运行的 GUI 进程**，操作同一份文档。
//!
//! 「同一进程内共享一个编辑会话」是这套架构的核心：agent 改的那一刻，GUI 立刻能看到（反之亦然），
//! 不存在"两份文档互相同步"的问题。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::codec::{self, Fidelity, Format};
use crate::broadcast::{topics_of, Broadcast, Origin, Subscribers, Subscription, Topic, TopicFilter, TopicKind};
use crate::cmd::{is_easing, parse_beat};
use crate::doc::{Beat, Document, Event, JudgeLine, Note, NoteKind, TRACKS};
use crate::journal::{read_track, Change, Journal, LineProps};

/// 最近广播的保留条数（调试面板 / `{"op":"broadcasts"}` 用）
const BROADCAST_RING: usize = 512;

/// **摊缓存时"谁在认领这个目录"**（见 [`EditCore::stage_file_as`]）。
///
/// 解压缓存按**容器内容**分目录（同一个包落在同一个目录），但 `session.json` 记的是**会话**状态
/// （谁的进程、有没有未保存的快照）—— 于是"一次性读一下"的工具不能去写别人的会话元数据，
/// 就像不能去动别人桌上的草稿。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheClaim {
    /// **一个会话**：在缓存目录里立 `session.json`（崩溃后它就是"上次没退干净"的证据），
    /// 换谱面/退出时负责清理。GUI 与"要在缓存里留工作副本"的调用方用这个。
    Session,
    /// **一次性读取**（`opm-ctl` 的转换/批处理）：别人已经认领的目录**一个字节都不碰**
    /// （尤其是那份可能带着未保存改动的 `opm.json` 快照）。
    ReadOnly,
}

/// 从解压缓存"继续编辑"之后**实际拿到了什么**（调用方据此报告；不是一个含糊的 bool）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resumed {
    /// 谱面名
    pub name: String,
    /// 恢复出来的保存目标（`None` = 这份谱面当初还没落过盘）
    pub source: Option<PathBuf>,
    /// 带回来的资源个数
    pub assets: usize,
    /// 继续之后**是不是脏的**（缓存里有未保存改动 ⇒ 是）
    pub unsaved: bool,
}

/// **"载入文件到临时文件夹"的结果**（见 [`EditCore::stage_file`]）：与"会话状态"无关的纯数据。
///
/// 它只回答"这份输入是什么、摊到哪儿了"，**不碰任何会话字段** —— 于是"打开文件"与
/// "从崩溃缓存继续"共用同一个装载入口 [`EditCore::load_staged`]，而两者的差别（保存目标、
/// 脏位）在这一层就已经定下来，装载那一步不需要知道输入是 zip 还是目录。
#[derive(Clone, Debug)]
pub struct Staged {
    /// **我们摊出来的**临时目录（`None` = 这份输入本来就是可编辑的真实文件/文件夹，没什么可摊的）。
    /// 有值 ⇒ 会话退出时要负责删掉（一个进程至多留一份）。
    pub dir: Option<PathBuf>,
    pub doc: Document,
    /// 资源（名字 → 字节）；裸 JSON / 文件夹形态时是磁盘上那些文件的这份拷贝
    pub assets: Vec<crate::zip::Entry>,
    /// 来源格式（保存默认写回同一种）
    pub format: Format,
    pub fid: Fidelity,
    /// 保存目标（**继续编辑时写回哪儿**）：容器/裸文件 = 输入路径；崩溃缓存 = 元数据里记的原文件
    pub target: Option<PathBuf>,
    /// 输入是**用户的文件夹**时给出那个目录（`None` = 输入是文件，或那是我们自己摊的缓存目录）。
    ///
    /// 光看保存目标分不出形态：文件夹形态下目标是目录里的 `opm.json`/`chart.json`，
    /// 按扩展名判会被当成"一个 `.json` 单文件"。
    pub folder: Option<PathBuf>,
    /// 缓存里有**未保存的改动**（只有"从崩溃缓存继续"会为真）
    pub unsaved: bool,
}

pub struct EditCore {
    /// **私有**：文档只能由 EditCore 写。
    ///
    /// 这条边界是刻意的（用户定的规则）：EditCore 只管"最终会写进谱面文件的数据"，
    /// 而这类数据的**写**只能发生在核心内部 —— 外部（GUI/CLI/控制通道）只能
    /// `doc()` 读，改动一律走 `exec`。字段私有把这条纪律交给编译器，而不是靠注释。
    /// 与之相对，网格吸附、播放头、选区、叠加层这些**界面内部规则**归 GUI 自己管，它们不进文件。
    doc: Document,
    /// **私有**：与 `doc` 同理 —— 逆操作的**写**（`record`/`undo`/`redo`/`abort`）只能发生在核心内部，
    /// 外部只读（见 [`EditCore::journal`]）。
    journal: Journal,
    /// **私有**：只读访问走 [`EditCore::path`]。
    path: Option<PathBuf>,
    /// 每次成功改动 +1。GUI **不再轮询它**，只作为广播里的序号与对外一致性检查。
    revision: u64,
    log: Vec<String>,
    /// 订阅登记表：改动完成后按话题过滤投递（外部只能问数量，见 [`EditCore::subscriber_count`]）
    subscribers: Subscribers,
    /// 最近广播（环形）
    broadcasts: VecDeque<Broadcast>,
    /// 上一次广播实际投递给了几个订阅者（测试用：证明"无关控件没收到"）
    last_delivered: usize,
    /// 本次执行的来源标记（控制通道线程在调用前 [`EditCore::set_origin`] 成 `Remote`）
    origin: Origin,
    /// 把每条广播打到 stderr（排障用；`--verbose-updates` 经 [`EditCore::set_verbose`] 打开）
    verbose: bool,
    /// **容器资源**（名字 → 字节；裸 JSON 时为空）：跟着文档走，保存容器时原样写回。
    /// 放在 EditCore 而不是 GUI：它属于"会写进文件的东西"，写入权同样只该有一处。
    assets: Vec<crate::zip::Entry>,
    /// **载入时的来源格式**：保存默认写回同一种格式（RPE 进、RPE 出），
    /// 否则"打开一个 RPE 谱面然后 Ctrl+S"会把它悄悄变成 opm —— 那是数据事故，不是便利。
    source_format: codec::Format,
    /// 来源是 RPE 时记下它的 `RPEVersion` 档位，导出时沿用（150/160 两档实际存在）
    rpe_target: codec::rpe::RpeTarget,
    /// 最近一次载入/保存的保真度报告（CLI 打印、GUI 控制台显示）。
    /// `None` 表示还没发生过 IO。
    last_fidelity: Option<codec::Fidelity>,
    /// 上次保存时的 revision —— 用来显示"未保存"标记。
    /// **保守**：撤回保存点之后仍算脏（只有再保存才清）——比"看起来干净其实没存"安全。
    saved_revision: u64,
    /// 当前文档里的**事件重叠**（缓存）。
    ///
    /// 算法本身是纯函数（`cmd::overlaps` / `cmd::overlaps_of_line`），但**什么时候重算**属于
    /// 文档语义，于是放在核心：每次改动之后由核心自己按"动过的那条线"增量刷新。
    /// 这样 GUI 与 CLI 都只是**读** [`EditCore::overlaps`]（或问 `{"op":"overlaps"}`），
    /// 而不是各自维护一份、各自决定何时重查 —— 两份实现迟早不一致（这正是它搬家的原因）。
    overlaps: Vec<crate::cmd::Overlap>,
    /// 容器资源**摊到磁盘**后的目录（`None` = 不是从容器载入的，或容器里没有资源）。
    ///
    /// 为什么必须摊出来：容器是**自包含**的，`meta.audio` 里写的是**文件名**（不是宿主机路径）——
    /// 只有把包里的资源落成真实文件，"按路径装载音频"这条既有链路才找得到它。
    /// 这条链以前只写了 `container::extract_assets`，**没人调用** ⇒ 从 `.opm` 打开的谱面
    /// 永远"音乐没装上"（用户报的"音乐应有时长"就是这个：包里有 41MB 的 flac，却解析到谱面旁边去找）。
    asset_dir: Option<PathBuf>,
    /// 上次保存用的（形态, 目标路径）：同路径再存要回到**同一个形态**。
    /// 文件夹形态下 `path` 是目录里的谱面文件，光看扩展名会把"文件夹"误判成"单文件"。
    last_save: Option<(SaveShape, PathBuf)>,
}

/// 保存形态：**两个格式 × 打包开关**（用户："4 种导出方式（opm|rpe|打包开关）"）。
///
/// |                | 打包（一个文件） | 不打包（一个文件夹） |
/// |---|---| ---|
/// | **opm** | `.opm`（zip：`opm.json` + 音乐/曲绘） | 目录：`opm.json` + 音乐/曲绘 |
/// | **RPE** | `.pez`（zip：`info.yml` + `chart.json` + 音乐/曲绘） | 目录：同左三件 |
///
/// **单文件 JSON 不再是保存形态**（用户："而不是 json"）：`.opm.json` 与 RPE 的单文件 `.json`
/// 仍然**读得进来**，而且**已经存在的**目标文件依旧按原形态写回（不偷偷改别人文件的格式），
/// 但"新建一个目标"时不再往单文件 JSON 上解析 —— 见 [`SaveFormat::resolve`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveFormat {
    /// 跟着目标走（没有目标时按"opm 包"这个正式形态）
    Auto,
    /// opm 包：一个 `.opm`（zip）
    OpmPacked,
    /// opm 无压缩文件夹：`opm.json` + 音乐/曲绘
    OpmFolder,
    /// RPE 谱面包：一个 `.pez`（zip：`info.yml` + `chart.json` + 音乐/曲绘）
    RpePacked,
    /// RPE 无压缩文件夹：`info.yml` + `chart.json` + 音乐/曲绘
    RpeFolder,
}

/// 解析之后的"实际写什么"（`Auto` 已经定下来）。
///
/// 比 [`SaveFormat`] 多出两个 `*Single`：它们**只用于写回已经存在的单文件 JSON**，
/// 界面上不提供（用户要求保存限定在四种形态里），但老文件不能因为这条规定就写不回去。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveShape {
    /// `.opm`（zip）
    OpmZip,
    /// 目录：`opm.json` + 资源
    OpmFolder,
    /// 裸 `.opm.json`（仅写回既有文件）
    OpmSingle,
    /// `.pez`（zip：`info.yml` + `chart.json` + 资源）
    RpeZip,
    /// 目录：`info.yml` + `chart.json` + 资源
    RpeFolder,
    /// 单文件 RPE `.json`（仅写回既有文件）
    RpeSingle,
}

impl SaveShape {
    /// **谱面文件本体**用哪个格式存 —— 它同时决定 `source_format`，因为"格式跟着文件走"：
    /// 文件夹形态里躺着的是裸 `opm.json`，所以它对应 `Format::Opm`（重新打开这个文件也是这样报的）。
    pub fn chart_file_format(self) -> Format {
        match self {
            SaveShape::OpmZip => Format::OpmZip,
            SaveShape::OpmFolder | SaveShape::OpmSingle => Format::Opm,
            SaveShape::RpeZip | SaveShape::RpeFolder | SaveShape::RpeSingle => Format::Rpe,
        }
    }
    /// 是不是"一个文件夹"形态（目标路径是目录，不是文件）
    pub fn is_folder(self) -> bool {
        matches!(self, SaveShape::OpmFolder | SaveShape::RpeFolder)
    }
    /// 谱面本体在包内/目录里叫什么（单文件形态没有这一层，返回 `None`）
    pub fn chart_entry(self) -> Option<&'static str> {
        match self {
            SaveShape::OpmZip | SaveShape::OpmFolder => Some(codec::container::CHART_NAME),
            SaveShape::RpeZip | SaveShape::RpeFolder => Some(codec::package::CHART_NAME),
            SaveShape::OpmSingle | SaveShape::RpeSingle => None,
        }
    }
    /// 打包形态的默认扩展名（文件夹形态没有扩展名）
    pub fn extension(self) -> Option<&'static str> {
        match self {
            SaveShape::OpmZip => Some(".opm"),
            SaveShape::RpeZip => Some(codec::package::PACKAGE_EXTENSION),
            _ => None,
        }
    }
}

impl From<Format> for SaveFormat {
    /// "跟随来源格式"：裸进裸出（旧的单文件形态 → 现在对应它的**文件夹**形态）
    fn from(f: Format) -> Self {
        match f {
            Format::Opm => SaveFormat::OpmFolder, // 裸 opm 就是"谱面 + 同目录资源"，即无压缩形态
            Format::OpmZip => SaveFormat::OpmPacked,
            Format::Rpe => SaveFormat::RpeFolder,
        }
    }
}

impl SaveFormat {
    pub fn parse(s: Option<&str>) -> Option<Self> {
        let s = s?.trim().to_ascii_lowercase();
        Some(match s.as_str() {
            "auto" => SaveFormat::Auto,
            "opm" | "opmz" | "container" | "opm-pack" | "opm-packed" => SaveFormat::OpmPacked,
            "opm-dir" | "opm-folder" | "opm-unpacked" => SaveFormat::OpmFolder,
            "rpe" | "pez" | "rpe-pack" | "rpe-packed" => SaveFormat::RpePacked,
            "rpe-dir" | "rpe-folder" | "rpe-unpacked" => SaveFormat::RpeFolder,
            _ => return None,
        })
    }
    /// 控制通道回话用的短名（与 `parse` 一一对应）
    pub fn as_str(self) -> &'static str {
        match self {
            SaveFormat::Auto => "auto",
            SaveFormat::OpmPacked => "opm",
            SaveFormat::OpmFolder => "opm-dir",
            SaveFormat::RpePacked => "rpe",
            SaveFormat::RpeFolder => "rpe-dir",
        }
    }
    /// 界面上那一行说明（四种形态 + 它们的产物）
    pub fn describe(self) -> &'static str {
        match self {
            SaveFormat::Auto => "自动（跟着目标：`.opm` → 包，目录 → 文件夹；新建默认 opm 包）",
            SaveFormat::OpmPacked => "opm 包：一个 `.opm`（谱面 + 音乐 + 曲绘）",
            SaveFormat::OpmFolder => "opm 文件夹：目录里放 `opm.json` + 音乐 + 曲绘（可 diff）",
            SaveFormat::RpePacked => "RPE 包：一个 `.pez`（`info.yml` + `chart.json` + 音乐 + 曲绘）",
            SaveFormat::RpeFolder => "RPE 文件夹：目录里放 `info.yml` + `chart.json` + 音乐 + 曲绘",
        }
    }
    /// 打包开关这一轴（`None` = 自动）
    pub fn packed(self) -> Option<bool> {
        match self {
            SaveFormat::Auto => None,
            SaveFormat::OpmPacked | SaveFormat::RpePacked => Some(true),
            SaveFormat::OpmFolder | SaveFormat::RpeFolder => Some(false),
        }
    }
    /// 格式这一轴（opm / RPE）——界面上两个控件就是这两轴
    pub fn chart_format(self) -> Option<Format> {
        match self {
            SaveFormat::Auto => None,
            SaveFormat::OpmPacked | SaveFormat::OpmFolder => Some(Format::OpmZip),
            SaveFormat::RpePacked | SaveFormat::RpeFolder => Some(Format::Rpe),
        }
    }
    /// 用两轴的取值拼回来（界面控件用）
    pub fn from_axes(chart: Format, packed: bool) -> Self {
        match (matches!(chart, Format::Rpe), packed) {
            (false, true) => SaveFormat::OpmPacked,
            (false, false) => SaveFormat::OpmFolder,
            (true, true) => SaveFormat::RpePacked,
            (true, false) => SaveFormat::RpeFolder,
        }
    }
    /// **还没有保存目标**时建议用什么名字（文件夹形态：建议一个目录名，不带扩展名）
    pub fn suggested_extension(self, loaded: Format) -> Option<&'static str> {
        match self {
            SaveFormat::OpmPacked => Some(".opm"),
            SaveFormat::OpmFolder => None,
            SaveFormat::RpePacked => Some(codec::package::PACKAGE_EXTENSION),
            SaveFormat::RpeFolder => None,
            // 自动：新建的目标用**正式形态**（opm 包）—— 不再给单文件 JSON 当默认
            SaveFormat::Auto => Some(match loaded {
                Format::Rpe => codec::package::PACKAGE_EXTENSION,
                _ => ".opm",
            }),
        }
    }
    /// 决定实际写什么。**这里是"保存形态只有四种"这条规则的落点。**
    ///
    /// `Auto` 的顺序：先看目标名（`.opm`/`.pez`），再看它是不是既有的单文件（写回原形态），
    /// 都不像就按来源格式的**文件夹**形态；而"**新建**一个 `.json`/`.opm.json`"会被明确拒绝 ——
    /// 单文件 JSON 不再是保存形态，但报错要说清楚该换成什么。
    pub fn resolve(self, path: &Path, loaded: Format) -> Result<SaveShape, String> {
        let explicit = match self {
            SaveFormat::OpmPacked => Some(SaveShape::OpmZip),
            SaveFormat::OpmFolder => Some(SaveShape::OpmFolder),
            SaveFormat::RpePacked => Some(SaveShape::RpeZip),
            SaveFormat::RpeFolder => Some(SaveShape::RpeFolder),
            SaveFormat::Auto => None,
        };
        if let Some(shape) = explicit {
            return Ok(shape);
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
        let exists = path.is_file();
        if name.ends_with(".opm") || name.ends_with(".opmz") {
            return Ok(SaveShape::OpmZip);
        }
        if name.ends_with(codec::package::PACKAGE_EXTENSION) || name.ends_with(".pez") {
            return Ok(SaveShape::RpeZip);
        }
        let single = if name.ends_with(".opm.json") {
            Some(SaveShape::OpmSingle)
        } else if name.ends_with(".json") {
            Some(SaveShape::RpeSingle)
        } else {
            None
        };
        if let Some(shape) = single {
            if exists {
                // 写回一个已经存在的单文件：**保持它原来的形态**（偷偷改格式比不改更糟）
                return Ok(shape);
            }
            return Err(format!(
                "单文件 JSON 不再是保存形态（只能是 opm 包 `.opm` / opm 文件夹 / RPE 包 `{}` / RPE 文件夹）：\
                 目标 `{}` 看起来是新建的单文件。在「另存为格式」里选一种形态，或把目标写成一个目录。",
                codec::package::PACKAGE_EXTENSION,
                path.display()
            ));
        }
        // 没有扩展名（或就是个目录路径）⇒ 按来源格式的**无压缩文件夹**形态
        Ok(match loaded {
            Format::Rpe => SaveShape::RpeFolder,
            _ => SaveShape::OpmFolder,
        })
    }
}

/// 形态的短名（回话与日志共用；与 `SaveFormat::as_str` 同一套词）
pub fn shape_name(s: SaveShape) -> &'static str {
    match s {
        SaveShape::OpmZip => "opm",
        SaveShape::OpmFolder => "opm-dir",
        SaveShape::OpmSingle => "opm-bare",
        SaveShape::RpeZip => "rpe",
        SaveShape::RpeFolder => "rpe-dir",
        SaveShape::RpeSingle => "rpe-json",
    }
}

/// 保真度报告 → JSON（CLI/控制通道输出）
pub fn fidelity_json(f: &Fidelity) -> Value {
    json!({
        "source": f.source,
        "version": f.version,
        "lossless": f.is_lossless(),
        "conversions": f.conversions,
        "warnings": f.warnings,
    })
}

impl Default for EditCore {
    fn default() -> Self {
        Self::new()
    }
}

impl EditCore {
    /// 一份**空的占位文档**：它是干净的（没有任何"会丢的东西"）。
    ///
    /// 为什么与 `{"op":"new"}` 不同：那条命令是**用户明确要建一份谱面**，建完就置脏；
    /// 而这里只是"还没有文档"的占位（启动页背后那一份）。若把占位也算脏，程序刚起来点一下关窗
    /// 就会问"要先保存吗" —— 那是在问一份用户从没碰过的空谱面。界面靠"没有保存目标"显示「尚未保存」，
    /// 不依赖这个标记（见 `statusbar::file_mark`）。
    pub fn new() -> Self {
        Self {
            doc: Document::default(),
            journal: Journal::default(),
            path: None,
            revision: 0,
            log: Vec::new(),
            subscribers: Subscribers::default(),
            broadcasts: VecDeque::new(),
            last_delivered: 0,
            origin: Origin::Local,
            verbose: false,
            saved_revision: 0,
            source_format: codec::Format::Opm,
            rpe_target: codec::rpe::RpeTarget::default(),
            last_fidelity: None,
            assets: Vec::new(),
            // 空文档没有事件 ⇒ 没有重叠（不必扫一遍）
            overlaps: Vec::new(),
            last_save: None,
            asset_dir: None,
        }
    }

    /// ① **载入文件到临时文件夹**（用户要求的一步：与"正式加载编辑"分开的两个调用）。
    ///
    /// **各种格式与打包情况都在这里处理**，出口只有一种东西：一份 [`Staged`]（文档 + 资源 +
    /// 保真度 + 格式 + 保存目标）。认得的输入形态：
    ///
    /// | 输入 | 判据（**按内容，不看扩展名**） | 摊到临时目录？ |
    /// |---|---|---|
    /// | opm 容器 `.opm` | ZIP 里是 `opm.json` | 是：`<临时目录>/opm/<key>/` |
    /// | RPE 谱面包 `.pez` | ZIP 里有 `info.yml` | 是：同上 |
    /// | opm 无压缩文件夹 | 目录里有 `opm.json` | 否（资源本来就是真实文件） |
    /// | RPE 无压缩文件夹 | 目录里有 `info.yml` | 否 |
    /// | 裸 opm / RPE JSON | 既不是 ZIP 也不是目录 | 否 |
    /// | 上一轮留下的解压缓存 | 目录里多一份 `session.json` | 是（它本来就在临时目录里） |
    ///
    /// 为什么要把这一步单独抽出来：**"怎么把输入摊开"与"装进会话"是两件事**。
    /// 早先它们缠在 `load_reporting`/`load_into` 里各写一遍，于是"启动那条路"漏了摊资源
    /// （用户报的"包里有音乐却装不上"），而 `.pez` 干脆读不回来（`chart.json` 被当 opm 解析）。
    /// 现在摊开只有这一份实现，装进会话只有 [`EditCore::load_staged`] 一份实现。
    pub fn stage_file(path: &Path) -> Result<Staged, String> {
        Self::stage_file_as(path, CacheClaim::Session)
    }

    /// 同 [`EditCore::stage_file`]，但显式说明"这次摊缓存算不算认领一个会话"（见 [`CacheClaim`]）。
    pub fn stage_file_as(path: &Path, claim: CacheClaim) -> Result<Staged, String> {
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("读取失败 {}: {e}", path.display()))?;
        if meta.is_dir() {
            return Self::stage_folder(path);
        }
        let bytes = std::fs::read(path).map_err(|e| format!("读取失败: {e}"))?;
        if !crate::zip::looks_like_zip(&bytes) {
            // 裸 JSON：按**内容**判 opm / RPE（`codec::load_bytes` 就是那条判据）
            let (doc, fid) = codec::load_bytes(&bytes)?;
            return Ok(Staged {
                dir: None,
                doc,
                assets: Vec::new(),
                format: codec::Format::parse(&fid.source).unwrap_or(codec::Format::Opm),
                fid,
                target: Some(path.to_path_buf()),
                folder: None, // 裸 JSON 是文件，不是文件夹
                unsaved: false,
            });
        }
        let mut fid = codec::Fidelity::new("opm", "容器（zip）".to_owned());
        let entries = codec::container::unpack(&bytes, &mut fid)?;
        // **"这是哪一种包"只能等解开、看过条目名才知道**（RPE 谱面包里有 `info.yml`）——
        // 判据在 `codec::EntryKind::of` 一处（见那里的注释：分错了就是整份文档按错格式解析）
        let kind = codec::EntryKind::of(&entries);
        let (doc, assets) = kind.read(entries, &mut fid)?;
        // 来源格式跟着**内容**走（与旧行为一致）：容器 ⇒ `opm`，RPE 谱面包 ⇒ `rpe`。
        // 它决定"保存时默认写回哪种格式"（RPE 进就 RPE 出），也是会话元数据里记的那一项。
        let format = codec::Format::parse(&fid.source).unwrap_or(codec::Format::Opm);
        // 容器/包：**摊到 `<临时目录>/opm/<内容 hash>`**（同一个包反复打开落在同一个目录）。
        // `meta.audio` 里写的是**包内文件名**，只有摊成真实文件，"按路径装载音频"才找得到它。
        let key = codec::container::cache_key(&bytes);
        let dir = Self::stage_into_cache(&doc, &assets, &key, Some(path), format, claim, &mut fid)?;
        Ok(Staged {
            dir: Some(dir),
            doc,
            assets,
            format,
            fid,
            target: Some(path.to_path_buf()),
            folder: None, // 包（zip）不是文件夹
            unsaved: false,
        })
    }

    /// 目录形态：opm 无压缩文件夹 / RPE 无压缩文件夹 / **上一轮留下的解压缓存**（三步同一条路）。
    fn stage_folder(dir: &Path) -> Result<Staged, String> {
        let mut entries: Vec<crate::zip::Entry> = Vec::new();
        let mut subdirs: Vec<String> = Vec::new();
        let rd = std::fs::read_dir(dir).map_err(|e| format!("读目录失败 {}: {e}", dir.display()))?;
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = e.metadata() else { continue };
            let Some(name) = p.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                continue;
            };
            if m.is_dir() {
                subdirs.push(name); // 无压缩形态是**平的**（见 `write_entries_to_dir`）
                continue;
            }
            // 解压缓存里的会话元数据 / 半截临时文件都不是资源
            if name == codec::container::SESSION_NAME || name.ends_with(".tmp") {
                continue;
            }
            let data = std::fs::read(&p).map_err(|e| format!("读 {name} 失败: {e}"))?;
            entries.push(crate::zip::Entry { name, data });
        }
        let session = codec::container::read_session(dir);
        let mut fid = codec::Fidelity::new("opm", "无压缩文件夹".to_owned());
        // 与"从文件开"走同一个判据（`codec::EntryKind`）
        let kind = codec::EntryKind::of(&entries);
        let (doc, assets) = kind.read(entries, &mut fid)?;
        // 来源格式：**会话元数据优先**（它记的是"原来那个文件是什么形态"—— 摊开之后目录里只剩
        // 一份 `opm.json`，光看内容分不出它原来是 `.opm` 容器还是无压缩文件夹，而"继续编辑之后
        // Ctrl+S 写回哪种形态"要的正是前者）；没有元数据时才按内容判。
        let format = session
            .as_ref()
            .and_then(|s| codec::Format::parse(&s.format))
            .or_else(|| codec::Format::parse(&fid.source))
            .unwrap_or(codec::Format::Opm);
        if !subdirs.is_empty() {
            fid.warn(format!(
                "目录里的子目录（{}）没被载入：无压缩形态是平的，资源要放在这一层",
                subdirs.join("、")
            ));
        }
        // 这一层目录**是我们摊出来的**吗？
        // · 上一轮崩溃留下的缓存：它就在临时目录里，退出时归我们清理（`target`/`unsaved` 从元数据恢复）
        // · 用户自己的工程文件夹：不是我们的东西，**一个字节都不许删**
        let ours = session.is_some() && dir.starts_with(codec::container::cache_root());
        let target = session
            .as_ref()
            .and_then(|s| s.source.clone())
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| (!ours).then(|| dir.join(kind.chart_name())));
        let unsaved = session.as_ref().is_some_and(codec::container::Session::has_unsaved);
        fid.note(format!(
            "无压缩形态：{} 个文件（资源 {} 个）",
            assets.len() + 1,
            assets.len()
        ));
        fid.finalize();
        Ok(Staged {
            dir: ours.then(|| dir.to_path_buf()),
            doc,
            assets,
            format,
            fid,
            target,
            // 用户的工程文件夹（不是我们摊的缓存）⇒ 记下来：**第一次**保存就得回到"文件夹"形态
            folder: (!ours).then(|| dir.to_path_buf()),
            unsaved,
        })
    }

    /// 把容器内容摊到 `<临时目录>/opm/<key>/`：**谱面本体 + 资源 + 会话元数据**，然后按上限修剪。
    ///
    /// （这一步以前叫 `materialize_assets`，长在"载入会话"里；现在只属于"载入文件到临时文件夹"。）
    fn stage_into_cache(
        doc: &Document,
        assets: &[crate::zip::Entry],
        key: &str,
        source: Option<&Path>,
        format: codec::Format,
        claim: CacheClaim,
        fid: &mut codec::Fidelity,
    ) -> Result<PathBuf, String> {
        let dir = codec::container::extract_dir(key);
        // **别人已经认领的目录，一次性读取者一个字节都不碰**。
        //
        // 为什么要有这条：缓存目录按**容器内容**命名，而 `session.json` 与那份 `opm.json`
        // 快照是**会话**状态。`opm-ctl`（转换/批处理）读同一个包时若照常摊一遍，就会把 GUI
        // 崩溃留下的元数据与未保存快照一起覆盖掉 —— "上次没退干净"那条提示连同用户一小时
        // 的改动就这么没了（实测：一次 `opm-ctl --file X dump` 就能抹掉）。
        let existing = codec::container::read_session(&dir);
        let claimed_by_other = existing.as_ref().is_some_and(|s| s.pid != std::process::id());
        if claimed_by_other && claim == CacheClaim::ReadOnly {
            fid.note(format!(
                "缓存目录 {} 已被另一个会话占用（pid {}）：本次只读不动它（不覆盖它的快照与会话元数据）",
                dir.display(),
                existing.map(|s| s.pid).unwrap_or(0)
            ));
        } else {
            codec::container::extract_container_into(&dir, doc, assets)?;
            // 记下"这份缓存是谁、什么时候、为哪个谱面摊出来的"（见 `codec::container::Session`）：
            // 正常退出会删掉它，于是**下次启动时还躺着的 GUI 目录 = 上个进程被强杀或崩溃**，
            // 靠这份元数据说清"是哪份谱面、有没有未保存改动"
            let _ = codec::container::write_session(
                &dir,
                &codec::container::Session {
                    pid: std::process::id(),
                    exe: codec::container::exe_name(),
                    source: source.map(|p| p.display().to_string()),
                    format: format.as_str().to_owned(),
                    name: doc.meta.name.clone(),
                    started: codec::container::now_secs(),
                    snapshot: 0,
                    dirty: false,
                },
            );
        }
        // 摊完就按上限修剪（`/tmp` 常是 tmpfs）：本目录刚写过 ⇒ mtime 最新，不会被删
        let (n, freed) = codec::container::prune_cache(
            &codec::container::cache_root(),
            codec::container::CACHE_CAP_BYTES,
        );
        fid.note(format!(
            "容器解压到 {}（{} 个文件）{}",
            dir.display(),
            assets.len() + 1,
            if n > 0 {
                format!("；清理解压缓存 {n} 个旧目录 / {} MB", freed / 1024 / 1024)
            } else {
                String::new()
            }
        ));
        fid.finalize();
        Ok(dir)
    }

    /// ② **正式加载编辑**：把 [`Staged`] 装进这个会话（**唯一**一个入口）。
    ///
    /// 整体替换是数据结构层面的"全量变更"：撤销栈清空、revision +1、按**全量话题**广播，
    /// 于是 GUI 只会走一次"整表重建"（`structure` 脏位），不需要各面板自己去猜。
    ///
    /// **换谱面时上一份解压目录立刻删掉** ⇒ 一个进程至多留一份（缓存是进程独占的）。
    pub fn load_staged(&mut self, staged: Staged) -> Result<codec::Fidelity, String> {
        let Staged { dir, doc, assets, fid, format, target, folder, unsaved } = staged;
        let origin = self.origin;
        self.doc = doc;
        self.assets = assets;
        self.journal = Journal::default(); // 换了谱面，旧的逆操作全部作废
        self.last_save = None; // 换了文档，上次的保存形态不再适用
        if let Some(old) = self.asset_dir.take() {
            if Some(&old) != dir.as_ref() {
                let _ = std::fs::remove_dir_all(&old);
            }
        }
        self.asset_dir = dir;
        self.path = target;
        self.source_format = format;
        if let Some(folder) = folder {
            // 从**用户的文件夹**载入 ⇒ 形态当场定下来，**第一次**保存就写回同一个文件夹。
            // 不这么做的后果实测过（2026-10-01）：`opm-ctl --file <opm 文件夹> --cmd … --save`
            // 走 `Auto`、只看扩展名，把目录里的 `opm.json` 当成"一个 `.json` 单文件" ⇒
            // 用 RPE 写回，**那份工程下次连打开都打不开**（`format 必须是 "opm"`）。
            // 同时这也修掉"第一次 Ctrl+S 不刷新音乐/曲绘"（扩展名判不出文件夹形态）。
            let shape = if format == codec::Format::Rpe {
                SaveShape::RpeFolder
            } else {
                SaveShape::OpmFolder
            };
            self.last_save = Some((shape, folder));
        }
        if self.source_format == codec::Format::Rpe {
            // 沿用来源文件的版本档位（`META.RPEVersion` 不可信，但作为"写回哪一档"的依据可用）
            let n = fid
                .version
                .split('=')
                .nth(1)
                .and_then(|x| x.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|x| x.parse::<i64>().ok())
                .unwrap_or(160);
            self.rpe_target =
                codec::rpe::RpeTarget { version: if n > 0 { n } else { 160 }, ..Default::default() };
        }
        self.revision += 1;
        // 缓存里有未保存改动（只有"从会话目录继续"会这样）⇒ 载入之后就是脏的
        self.saved_revision =
            if unsaved { self.revision.wrapping_sub(1) } else { self.revision };
        self.last_fidelity = Some(fid.clone());
        self.refresh_overlaps_all(); // 整表换了，全量
        self.log.push(format!(
            "load 格式={} 保存目标={} 判定线={} 音符={}",
            self.source_format.as_str(),
            self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "无".to_owned()),
            self.doc.judge_lines.len(),
            self.doc.judge_lines.iter().map(|l| l.notes.len()).sum::<usize>()
        ));
        // 全量话题：线集合、时间映射、所有属性/轨道/音符都可能是新的
        let topics = Self::all_topics();
        self.emit(
            origin,
            format!("载入 {}", self.doc.meta.name),
            topics,
            Vec::new(),
        );
        Ok(fid)
    }

    /// 载入谱面：**按内容判断格式**（opm 原生 / RPE / 两种打包形态 / 无压缩文件夹），返回保真度报告。
    pub fn load_reporting(path: &Path) -> Result<(Self, codec::Fidelity), String> {
        // 两步走：① 摊到临时文件夹（各种格式与打包情况）② 装进会话（同一个入口）
        let staged = Self::stage_file(path)?;
        let mut core = Self::new();
        let fid = core.load_staged(staged)?;
        Ok((core, fid))
    }

    /// 载入（不看报告）
    pub fn load(path: &Path) -> Result<Self, String> {
        Self::load_reporting(path).map(|(c, _)| c)
    }

    // ------------------------------------------------------------ 订阅 / 广播

    /// 订阅更新广播。过滤器决定"这个订阅者关心什么话题" —— 不命中的广播根本不会发给他。
    pub fn subscribe(&mut self, filter: TopicFilter) -> Subscription {
        let s = self.subscribers.subscribe(filter);
        self.log
            .push(format!("subscribe(filter={:?}) 共 {} 个订阅者", s.filter, self.subscribers.len()));
        s
    }

    /// 当前文档里的事件重叠（**核心拥有的缓存**：GUI 与 CLI 都读它，不各算一遍）。
    ///
    /// 每次成功改动之后由核心按"动过的那条线"增量刷新（结构变化则全量重算），
    /// 顺序稳定（线 → 层 → 轨道 → 起点），于是 GUI 列表与 CLI 输出都可逐字比对。
    pub fn overlaps(&self) -> &[crate::cmd::Overlap] {
        &self.overlaps
    }

    /// 重叠检测：全量重算
    fn refresh_overlaps_all(&mut self) {
        self.overlaps = crate::cmd::overlaps(&self.doc);
        self.sort_overlaps();
    }

    /// 重叠检测：**只重查这次改动动过的线**（说不清动了哪儿就全量）。
    fn refresh_overlaps(&mut self, changes: &[Change]) {
        let structural = changes.iter().any(|c| {
            matches!(
                c,
                Change::InsertLine { .. } | Change::RemoveLine { .. } | Change::SetBpm { .. }
            )
        });
        let mut lines: Vec<usize> = Vec::new();
        if !structural {
            for c in changes {
                if let Some(l) = c.line() {
                    if !lines.contains(&l) {
                        lines.push(l);
                    }
                }
            }
        }
        if structural || lines.is_empty() {
            // BPM/线集合变化会让"拍"的含义整体变；一条线都指不出来时也别猜
            self.refresh_overlaps_all();
            return;
        }
        for l in lines {
            self.overlaps.retain(|o| o.line != l);
            self.overlaps.extend(crate::cmd::overlaps_of_line(&self.doc, l));
        }
        self.sort_overlaps();
    }

    /// 顺序稳定：增量刷新是"删掉该线的旧结果再追加"，不排序的话 GUI 里那几行的次序会跳。
    fn sort_overlaps(&mut self) {
        self.overlaps.sort_by(|a, b| {
            (a.line, a.layer, a.track.as_str(), a.next, a.start.n, a.start.d).cmp(&(
                b.line,
                b.layer,
                b.track.as_str(),
                b.next,
                b.start.n,
                b.start.d,
            ))
        });
    }

    /// 构造并投递一条广播。
    ///
    /// **空话题 = "什么都可能变了"**：在这里补齐成全量话题。这不只是措辞问题 ——
    /// `Subscribers::emit` 是"话题命中才投递"，而一条**记录不出任何改动**的命令
    /// （`broadcast_recorded` 收到空 `recorded`）正好会给出空话题：那次广播就会
    /// **发给 0 个订阅者**，revision 涨了、界面却不知道（静默丢更新）。
    fn emit(&mut self, origin: Origin, label: String, topics: Vec<Topic>, changes: Vec<String>) {
        let topics = if topics.is_empty() {
            Self::all_topics()
        } else {
            topics
        };
        let b = Broadcast {
            revision: self.revision,
            origin,
            label,
            topics,
            changes,
        };
        self.last_delivered = self.subscribers.emit(&b);
        let line = format!(
            "update {} → 投递 {}/{} 订阅者",
            b.summary(),
            self.last_delivered,
            self.subscribers.len()
        );
        if self.verbose {
            eprintln!("[core] {line}");
        }
        self.log.push(line);
        if self.broadcasts.len() == BROADCAST_RING {
            self.broadcasts.pop_front();
        }
        self.broadcasts.push_back(b);
    }

    /// 全量话题（数据被整体替换/回退时使用）
    fn all_topics() -> Vec<Topic> {
        use TopicKind::*;
        [Meta, Bpm, LineList, LineProps, Notes, Note, Track]
            .iter()
            .map(|k| Topic::new(*k, None))
            .collect()
    }

    /// 把"刚记录的改动"变成一条广播：话题由**这些改动**推出（不再用"全量话题"兜底 ——
    /// 那会让话题过滤器失效：只订阅 3 号线的控件会被 0 号线的改动叫醒，而话题还写着"什么都可能变了"）。
    fn broadcast_recorded(&mut self, origin: Origin, op: &str, recorded: &[Change]) {
        let mut topics: Vec<Topic> = Vec::new();
        for ch in recorded {
            for t in topics_of(ch) {
                if !topics.contains(&t) {
                    topics.push(t);
                }
            }
        }
        let label = recorded
            .last()
            .map(Change::label)
            .unwrap_or_else(|| op.to_owned());
        let changes = self.journal.recent(8);
        self.emit(origin, label, topics, changes);
    }

    /// 保存（沿用**载入时的格式**）：RPE 进就 RPE 出，opm 进就 opm 出。
    pub fn save(&mut self, path: Option<&Path>) -> Result<PathBuf, String> {
        // 同路径保存（Ctrl+S）：**沿用上次用的形态**。文件夹形态下 `self.path` 是目录里的
        // `opm.json`，只看扩展名会被判成"单文件"，于是音乐与曲绘不会被刷新 —— 那是错的。
        if path.is_none() {
            if let (Some((shape, target)), true) = (self.last_save.clone(), self.path.is_some()) {
                let (target, fid) = self.save_shape(&target, shape)?;
                self.last_fidelity = Some(fid);
                return Ok(target);
            }
        }
        // 没存过（或显式给了新路径）：交给 `Auto` 按目标名判形态
        let target = path
            .map(|p| p.to_path_buf())
            .or_else(|| self.path.clone())
            .ok_or("未指定保存路径")?;
        let (target, fid) = self.save_as(&target, SaveFormat::Auto)?;
        self.last_fidelity = Some(fid);
        Ok(target)
    }

    /// 另存为：`fmt` 决定写**哪一种形态**（四种之一；`Auto` 见 [`SaveFormat::resolve`]）。
    ///
    /// 四种形态（用户："opm|rpe|打包开关"）：
    ///
    /// | 形态 | 产物 |
    /// |---|---|
    /// | opm 包 | 一个 `.opm`（zip：`opm.json` + 音乐/曲绘） |
    /// | opm 文件夹 | 目录里 `opm.json` + 音乐/曲绘 |
    /// | RPE 包 | 一个 `.pez`（zip：`info.yml` + `chart.json` + 音乐/曲绘） |
    /// | RPE 文件夹 | 目录里 `info.yml` + `chart.json` + 音乐/曲绘 |
    ///
    /// 前两种与后两种共用同一份"资源收集 + 名字规范化"逻辑（`collect_assets` +
    /// `planned_asset_renames`），所以**打包与不打包的内容逐字节相同**（有测试钉住）。
    /// 返回的路径：文件夹形态给的是**目录里的那个谱面文件**（`opm.json` / `chart.json`）——
    /// 于是"保存后这个文档的路径"仍然是一个能直接再打开的文件。
    pub fn save_as(
        &mut self,
        path: &Path,
        fmt: SaveFormat,
    ) -> Result<(PathBuf, codec::Fidelity), String> {
        let shape = fmt.resolve(path, self.source_format)?;
        self.save_shape(path, shape)
    }

    /// 按**已解析的形态**写盘（`save_as` 与"同路径再存一次"都走这里）。
    fn save_shape(
        &mut self,
        path: &Path,
        shape: SaveShape,
    ) -> Result<(PathBuf, codec::Fidelity), String> {
        let dir_target: Option<PathBuf> = if shape.is_folder() { Some(path.to_path_buf()) } else { None };
        let file_target: PathBuf = match shape.chart_entry() {
            Some(entry) if shape.is_folder() => path.join(entry),
            Some(_) => path.to_path_buf(),
            None => path.to_path_buf(),
        };
        // 压缩形态：目标**目录**必须已经存在（内核不会替你建）。
        // 文件夹形态：目录由我们建（"另存为一个文件夹"当然可以创建它）。
        if !shape.is_folder() {
            if let Some(dir) = file_target.parent().filter(|d| !d.as_os_str().is_empty()) {
                if !dir.is_dir() {
                    return Err(format!(
                        "目标目录不存在：{}（先建好目录，或用「选择文件夹…」挑一个已有的）",
                        dir.display()
                    ));
                }
            }
        } else if path.exists() && !path.is_dir() {
            return Err(format!(
                "{} 已经是一个文件；文件夹形态需要一个目录路径（或改用打包形态）",
                path.display()
            ));
        }
        // 资源从哪儿找：优先"文档现在所在的那本谱面"的目录，其次新目标的目录。
        // （`meta.audio`/`meta.background` 里是绝对路径时用不着它，相对路径才需要。）
        let base_dir = self
            .path
            .as_ref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .or_else(|| file_target.parent().map(Path::to_path_buf));

        let (target, fid) = match shape {
            SaveShape::OpmSingle => {
                // 裸 opm（`.opm.json`）：**只**用于写回已存在的单文件（不再是可选的保存形态）
                let text = serde_json::to_string_pretty(&self.doc.to_json())
                    .map_err(|e| format!("序列化失败: {e}"))?;
                std::fs::write(&file_target, format!("{text}\n"))
                    .map_err(|e| format!("写入失败: {e}"))?;
                let mut fid = codec::Fidelity::new("opm", format!("v{}", self.doc.format_version));
                fid.note("原生格式，无转换（写回既有的单文件 `.opm.json`）");
                (file_target.clone(), fid)
            }
            SaveShape::OpmZip | SaveShape::OpmFolder => {
                // opm 的两种形态：谱面 + 资源。已有的资源（例如刚从容器载入的）原样带回去；
                // 文档新引用的外部文件（`meta.audio`/`meta.background`）从它所在目录读进来。
                let mut fid = codec::Fidelity::new(
                    "opm",
                    if shape.is_folder() { "无压缩文件夹".to_owned() } else { "容器（zip）".to_owned() },
                );
                let (assets, doc_for_write, renames) =
                    self.package_assets_and_doc(base_dir.as_deref(), &mut fid);
                if shape.is_folder() {
                    let dir = dir_target.as_deref().unwrap_or(path);
                    let entries = codec::container::entries_for_dir(&doc_for_write, &assets);
                    codec::container::write_entries_to_dir(&entries, dir, &mut fid)?;
                } else {
                    let backend = codec::container::write_file(&doc_for_write, &assets, path)?;
                    fid.note(format!(
                        "容器：{} 个资源 + 谱面 `opm.json`（打包后端：{}）",
                        assets.len(),
                        backend.name()
                    ));
                }
                self.finish_package(fid, assets, renames, &file_target)
            }
            SaveShape::RpeSingle => {
                let fid = codec::rpe::save_file(&self.doc, &file_target, self.rpe_target)?;
                (file_target.clone(), fid)
            }
            SaveShape::RpeZip | SaveShape::RpeFolder => {
                // RPE 的两种形态：`info.yml` + `chart.json` + 音乐/曲绘（Phira 谱面标准）。
                // 名字规范化与 opm 走同一条规矩（包里一律用文件名，字段同步改写）。
                let mut fid = codec::Fidelity::new(
                    "rpe",
                    if shape.is_folder() { "谱面包（无压缩文件夹）".to_owned() } else { "谱面包（zip）".to_owned() },
                );
                let (assets, doc_for_write, renames) =
                    self.package_assets_and_doc(base_dir.as_deref(), &mut fid);
                let entries = codec::package::build_entries(
                    &doc_for_write,
                    self.rpe_target,
                    base_dir.as_deref(),
                    &assets,
                    &mut fid,
                )?;
                if shape.is_folder() {
                    let dir = dir_target.as_deref().unwrap_or(path);
                    codec::container::write_entries_to_dir(&entries, dir, &mut fid)?;
                } else {
                    let (bytes, backend) = crate::zip::pack_preferred(&entries)?;
                    std::fs::write(path, bytes).map_err(|e| format!("写入失败: {e}"))?;
                    fid.note(format!("谱面包已打包（打包后端：{}）", backend.name()));
                }
                self.finish_package(fid, assets, renames, &file_target)
            }
        };
        self.path = Some(target.clone());
        self.source_format = shape.chart_file_format();
        // 记住"这次用的是哪种形态、目标路径是哪一个"：文件夹形态下 `self.path` 是目录里的谱面文件，
        // 下次 Ctrl+S 必须回到**同一个形态**（否则只会刷新谱面，把旁边的音乐/曲绘落下）
        self.last_save = Some((shape, path.to_path_buf()));
        self.saved_revision = self.revision; // 存过就不算脏
        self.last_fidelity = Some(fid.clone());
        // 解压缓存里那份快照**跟着保存走**：否则下次启动时"继续此谱面"会把**保存前**的旧内容
        // 当成最新（用户刚存过盘，缓存却说他还有未保存改动 —— 那就成了骗人）。
        let _ = self.snapshot_session();
        Ok((target, fid))
    }

    /// **打包保存的第一段**（两种打包格式共用）：收集资源 + 规范化资源名。
    ///
    /// 顺序不能换：资源要**按当前字段**收集（字段里可能还是 `/tmp/x/song.ogg` 这样的外部路径，
    /// 那时才读得到盘），收完才把字段改成包内相对名 —— 反过来就找不到文件了。
    /// 抽出来的理由：opm 与 RPE 两条路曾各写一遍，而"改名清单"这种东西一旦漏了一条，
    /// 表现是"包里有两个名字一样的资源"（不是报错）。
    fn package_assets_and_doc(
        &self,
        base_dir: Option<&Path>,
        fid: &mut codec::Fidelity,
    ) -> (Vec<crate::zip::Entry>, Document, Vec<AssetRename>) {
        let assets = codec::container::collect_assets(&self.doc, &self.assets, base_dir, fid);
        let (doc_for_write, renames) = self.normalized_for_write(&assets, fid);
        (assets, doc_for_write, renames)
    }

    /// **打包保存的最后一段**（两种打包格式共用）：文件真的写出去了，才做这些。
    ///
    /// 收尾顺序也是规矩：资源留在内存（下次保存不必再读盘）、改名**走命令路径**落到文档
    /// （见 [`EditCore::land_renames`]）、最后交付保真度报告。
    fn finish_package(
        &mut self,
        mut fid: codec::Fidelity,
        assets: Vec<crate::zip::Entry>,
        renames: Vec<AssetRename>,
        file_target: &Path,
    ) -> (PathBuf, codec::Fidelity) {
        fid.finalize();
        self.assets = assets;
        self.land_renames(renames);
        (file_target.to_path_buf(), fid)
    }

    /// 资源名规范化：把 `meta.audio`/`meta.background` 里的外部路径改成**包内文件名**，
    /// 返回（送去写盘的文档副本, 需要落到内存文档的改名清单）。
    ///
    /// 为什么用**副本**：文件真的写出来了，才把改名落到内存文档 —— 早先它直接改内存文档而写盘在后，
    /// 写盘失败就会留下"文档被改、文件没写、没有任何信号"的状态；而且那次改动没 +revision、没广播，
    /// 界面缓存看不见它（保存链路上唯一一处绕过命令路径的改写）。
    fn normalized_for_write(
        &self,
        _assets: &[crate::zip::Entry],
        fid: &mut codec::Fidelity,
    ) -> (Document, Vec<AssetRename>) {
        let renames = planned_asset_renames(&self.doc);
        let mut shifted = self.doc.clone();
        for r in &renames {
            r.apply(&mut shifted);
            fid.note(format!(
                "{} `{}` → 包内相对名 `{}`（文档字段同步改写，可撤销）",
                r.label, r.from, r.to
            ));
        }
        (shifted, renames)
    }

    /// 写盘成功后，把资源改名**走命令路径**落到内存文档：记 journal（可撤销）、revision +1、
    /// 按 `set_meta` 的话题广播 —— 与用户自己改 `meta` 是同一条路。
    fn land_renames(&mut self, renames: Vec<AssetRename>) {
        if renames.is_empty() {
            return;
        }
        let mut set = serde_json::Map::new();
        for r in &renames {
            set.insert(r.field.to_owned(), json!(r.to));
        }
        let cmd = json!({"op": "set_meta", "set": Value::Object(set)});
        let resp = self.exec(&cmd);
        if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            self.log.push(format!(
                "资源名规范化未能落到文档（文件已写好）：{}",
                resp.get("error").and_then(|e| e.as_str()).unwrap_or("?")
            ));
        }
    }

    /// 把另一个文件**装进当前核心**（GUI「打开」/控制通道 `{"op":"load"}` 走这里）。
    ///
    /// 就是"两步走"的合体：[`EditCore::stage_file`] → [`EditCore::load_staged`]。
    /// 想在中途做点别的（问用户、只摊开不装载、把摊开的目录留着）就分开调那两个。
    pub fn load_into(&mut self, path: &Path) -> Result<Fidelity, String> {
        let staged = Self::stage_file(path)?;
        self.load_staged(staged)
    }

    /// 用一份现成文档替换（转换工具/测试用）。不广播：这种核心通常还没有订阅者；
    /// GUI 里换谱面请走 [`EditCore::load_into`]（那条会按全量话题广播）。
    ///
    /// **置脏**：换进来的文档没有任何文件对得上它（`new` 是同一条口径）——
    /// 早先这里写的是"saved"，等于对一份从没落过盘的文档说"已保存"。
    pub fn replace_doc(&mut self, doc: Document) {
        self.doc = doc;
        self.journal = Journal::default();
        self.last_save = None;
        self.revision += 1;
        self.saved_revision = self.revision.wrapping_sub(1); // 脏
        self.refresh_overlaps_all(); // 整份文档被换掉，重叠缓存必须跟着换
    }

    /// 只读访问文档。要改？走 `exec`（命令），别想要 `&mut`。
    pub fn doc(&self) -> &Document {
        &self.doc
    }

    /// **"需要保存"的唯一判据**（`revision != saved_revision`）—— 由核心维护，外面不许自己算。
    ///
    /// 契约（`tests/lifecycle.rs::core_owns_the_needs_save_state` 钉住）：
    /// - 任何**成功**的改动命令都 +1 revision（`dispatch` 里那一处，见"改动命令"段），于是天然置脏；
    /// - 查询命令（`summary`/`dump`/`validate`/`overlaps`/`journal`/`broadcasts`/`ping`…）不动它；
    /// - 失败的命令回滚、**不**推进版本号，所以"打错一条参数"不会被喊成未保存；
    /// - `undo`/`redo`/`abort` 也算文档变更（**保守口径**：撤回保存点之后仍算脏 ——
    ///   宁可多提示一次保存，也不要"看着干净其实和文件不一样"）；
    /// - `save`/`save_as` 成功后把 `saved_revision` 对齐到当前 revision；`load_into` 载入即干净
    ///   （盘上就是它）；`{"op":"new"}` 与 `replace_doc` 换来的是**没有文件对应**的文档 ⇒ 脏。
    ///
    /// 保守口径（见 `saved_revision` 注释）
    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    /// 只读访问更改日志。要改？走 `exec`（命令）—— 外面拿不到 `&mut Journal`。
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// 写/刷新缓存目录里的会话元数据。`snapshot` = 最近一次文档快照的时刻（0 = 从没写过）。
    ///
    /// 失败只记日志：这是**元数据**，写不进去不该影响编辑（顶多下次启动少一次提示）。
    fn write_session(&self, snapshot: u64, dirty: bool) -> Result<(), String> {
        let Some(dir) = self.asset_dir.clone() else {
            return Err("没有解压缓存目录".to_owned());
        };
        let s = self.session_meta(&dir, snapshot, dirty);
        codec::container::write_session(&dir, &s)
    }

    /// 会话元数据（`session.json` 的内容）：出处、出生时间、快照时刻与脏位。
    ///
    /// 抽出来是因为它有**两个用户**：同步写（[`Self::snapshot_session`]，保存时走的一条）
    /// 与后台写手（[`Self::snapshot_job`] → [`crate::autosave::write_snapshot`]）。
    /// 两份手抄的元数据必然漂移，而漂移的表现是"继续此谱面"提示错的东西。
    pub fn session_meta(&self, dir: &Path, snapshot: u64, dirty: bool) -> codec::container::Session {
        let started = codec::container::read_session(dir).map(|s| s.started).unwrap_or(0);
        codec::container::Session {
            pid: std::process::id(),
            exe: codec::container::exe_name(),
            source: self.path.as_ref().map(|p| p.display().to_string()),
            format: self.source_format.as_str().to_owned(),
            name: self.doc.meta.name.clone(),
            // 从缓存"继续"过来的：沿用原来那份的出生时间（它确实是那时候摊出来的）
            started: if started > 0 { started } else { codec::container::now_secs() },
            snapshot,
            dirty,
        }
    }

    /// **打一份可以交给别的线程去写的快照**：锁内只做"克隆文档 + 定下元数据"。
    ///
    /// 为什么不在锁内序列化：那是 223 ms（50 000 音符，见 [`crate::autosave`] 的表），
    /// 而克隆只要 2~4.5 ms —— 差三五十倍。GUI 帧里付这几毫秒没人看得出来，付那 223 ms
    /// 就是"每 2 秒卡一下"（而且播放头是墙钟驱动的，画面还会跟着跳 0.28 秒）。
    ///
    /// 元数据在这里就**定死**：`snapshot` 用此刻、`dirty` 用此刻的脏位 —— 序列化时文档可能
    /// 已经被继续编辑，那份改动属于下一拍。
    pub fn snapshot_job(&self) -> Result<crate::autosave::SnapshotJob, String> {
        let Some(dir) = self.asset_dir.clone() else {
            return Err("没有解压缓存目录（不是从容器载入的）".to_owned());
        };
        Ok(crate::autosave::SnapshotJob {
            session: self.session_meta(&dir, codec::container::now_secs(), self.is_dirty()),
            dir,
            doc: self.doc.clone(),
        })
    }

    /// 把当前**文档快照**写回解压缓存（`<缓存目录>/opm.json` + 会话元数据里的时间与脏位）。
    ///
    /// 为什么要它：进程被强杀时，磁盘上的谱面文件是**上一次保存**的版本，编辑了一小时的东西
    /// 按理说一个字节都不剩。缓存目录本来就是"这次会话的工作副本"，顺手把文档写进去，
    /// 「继续此谱面」就真的能接着编辑而不是"从头打开"。
    ///
    /// **与"退出时清理、未保存的数据按计划丢弃"不冲突**：那条说的是正常退出（守卫里用户选了
    /// 不保存 ⇒ 缓存随目录一起删掉）；这里是**没走到退出**的那条路上的兜底。
    ///
    /// 落法是"先写临时文件再改名"：强杀可能正好发生在写的中途，半截 JSON 比旧快照更糟
    /// （下次启动会拿着半截文件当文档）。同一目录内的改名在 Unix/Windows 上都是原子替换。
    ///
    /// **这条是同步的（调用线程付 223 ms/5 万音符）**，只有两处该用它：保存之后的对齐
    /// （用户刚存过盘，界面本来就停在那一下）与 CLI/无头路径。GUI 每 2 秒的那一拍走
    /// [`crate::autosave`] 的后台写手，别在这里同步写 —— 那正是"播放时每 2 秒卡一下"的成因。
    pub fn snapshot_session(&self) -> Result<(), String> {
        let job = self.snapshot_job()?;
        crate::autosave::write_snapshot(&job)
    }

    /// **从解压缓存继续**（启动时那个"上次没有正常退出"的对话框选「继续」走这里）。
    ///
    /// 就是"两步走"在**目录**这种输入上的用法：[`EditCore::stage_file`] 认得"摊开的目录"，
    /// 会话元数据（`session.json`）告诉它**保存目标是原来那个文件**、以及**缓存里有没有未保存改动**。
    /// 于是"继续"与"打开文件"走的是**同一个装载入口**，不存在第二套装载逻辑。
    pub fn load_session_into(&mut self, dir: &Path) -> Result<Resumed, String> {
        let session = codec::container::read_session(dir)
            .ok_or_else(|| format!("{} 里没有会话元数据（不是一份解压缓存）", dir.display()))?;
        let staged = Self::stage_file(dir)?;
        let unsaved = staged.unsaved;
        let source = staged.target.clone();
        let fid = self.load_staged(staged)?;
        let _ = fid;
        // 接着编辑的是**我们**：把会话元数据改成自己的进程（快照时间与脏位沿用缓存里的记录）
        let _ = self.write_session(session.snapshot, session.dirty);
        self.log.push(format!(
            "从缓存继续：{}（资源={}，保存目标={}）",
            dir.display(),
            self.assets.len(),
            self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "无".to_owned())
        ));
        Ok(Resumed {
            name: self.doc.meta.name.clone(),
            source,
            assets: self.assets.len(),
            unsaved,
        })
    }

    /// 当前保存目标；`None` = 还没落过盘。
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// **容器资源摊到磁盘后的目录**（见 `asset_dir` 字段的说明）。音频装载要在它里面找
    /// `meta.audio` 写的那个文件名。
    pub fn asset_dir(&self) -> Option<&Path> {
        self.asset_dir.as_deref()
    }

    /// 文档有没有引用外部资源（音乐 / 曲绘）。
    ///
    /// 用途只有一个但很关键：**新建谱面第一次保存时建议哪种扩展名** —— 引用了资源就建议
    /// 容器 `.opm`（它们得装进包里），否则维持裸 `.opm.json`（见 [`SaveFormat::suggested_extension`]）。
    pub fn references_assets(&self) -> bool {
        [self.doc.meta.audio.as_deref(), self.doc.meta.background.as_deref()]
            .into_iter()
            .flatten()
            .any(|s| !s.trim().is_empty())
    }

    /// 当前 revision（广播序号；测试用来断言"视图状态没碰过文档"）。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// 订阅者数量。**只报数不发表**：外部无从增删订阅，加订阅只有 [`EditCore::subscribe`]。
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// 最近广播（环形，最新在尾部）。只读：广播由核心自己投递。
    pub fn broadcasts(&self) -> &VecDeque<Broadcast> {
        &self.broadcasts
    }

    /// 上一次广播实际投递给了几个订阅者。
    pub fn last_delivered(&self) -> usize {
        self.last_delivered
    }

    /// 载入时的来源格式（保存默认写回同一种）。
    pub fn source_format(&self) -> Format {
        self.source_format
    }

    /// 来源 RPE 的 `RPEVersion` 档位（导出沿用）。
    pub fn rpe_target(&self) -> &codec::rpe::RpeTarget {
        &self.rpe_target
    }

    /// 设置 RPE 导出档位（`convert --rpe-version`）。与 `set_verbose`/`set_origin` 同类：
    /// **调用方的导出偏好**，不是文档数据，所以不进谱面文件、也不置脏。
    pub fn set_rpe_target(&mut self, target: codec::rpe::RpeTarget) {
        self.rpe_target = target;
    }

    /// 最近一次载入/保存的保真度报告（`None` = 还没发生过 IO）。
    pub fn last_fidelity(&self) -> Option<&Fidelity> {
        self.last_fidelity.as_ref()
    }

    /// 打开/关闭"每条广播打到 stderr"（`--verbose-updates`）。这是**界面/排障开关**，
    /// 不进谱面文件，所以给显式方法而不是一个可写字段。
    pub fn set_verbose(&mut self, on: bool) {
        self.verbose = on;
    }

    /// 设置本次执行的**来源标记**：控制通道线程在远程命令前设 `Remote`、执行完复原 `Local`，
    /// 广播据此让界面区分"谁改的"。同样是调用方状态，不是文档数据。
    pub fn set_origin(&mut self, origin: Origin) {
        self.origin = origin;
    }

    pub fn undo(&mut self) -> Result<Option<String>, String> {
        self.time_travel(false)
    }

    pub fn redo(&mut self) -> Result<Option<String>, String> {
        self.time_travel(true)
    }

    /// 撤销 / 重做：**同一段收尾**，只有"往哪边走"不同。
    ///
    /// 为什么合成一个：撤销与重做的后处理必须对称 —— 少一次 `refresh_overlaps_all` 或
    /// 漏 `revision += 1`，表现是"撤销之后状态栏不脏了/重叠数还是旧的"，而这类不一致
    /// 只在撤销与重做各写一遍时才会出现（而人只会去测他刚才改的那一边）。
    fn time_travel(&mut self, forward: bool) -> Result<Option<String>, String> {
        let (topics, verb) = if forward {
            (self.journal.redo_top().map(topics_of), "redo")
        } else {
            (self.journal.undo_top().map(topics_of), "undo")
        };
        let origin = self.origin;
        let r = if forward {
            self.journal.redo(&mut self.doc)?
        } else {
            self.journal.undo(&mut self.doc)?
        };
        if let Some(ref label) = r {
            self.revision += 1;
            // 一次撤销可能跨多条线（事务）⇒ 全量重算，别猜
            self.refresh_overlaps_all();
            let changes = self.journal.recent(8);
            self.emit(origin, format!("{verb}: {label}"), topics.unwrap_or_else(Self::all_topics), changes);
        }
        Ok(r)
    }

    /// 执行一条命令；返回结构化响应（与 `opm-ctl` 输出同构）
    pub fn exec(&mut self, cmd: &Value) -> Value {
        let op = cmd.get("op").and_then(|v| v.as_str()).unwrap_or("").to_owned();
        match self.dispatch(&op, cmd) {
            Ok(result) => {
                self.log.push(format!("{op} ok"));
                json!({"ok": true, "op": op, "result": result, "revision": self.revision})
            }
            Err(e) => {
                self.log.push(format!("{op} FAILED: {e}"));
                json!({"ok": false, "op": op, "error": e, "revision": self.revision})
            }
        }
    }

    /// 逐条执行，**每条命令自成一个撤销步**（默认粒度）。
    ///
    /// 早先这里把整批包成一个事务，后果是批内的 `undo` 在空栈上执行、静默无效 ——
    /// 而无头模式下日志只存在于进程内，跨调用又撤不了，等于 `undo` 在 CLI 里不可用。
    /// 需要原子性的调用方请显式用 `begin`/`commit`，或走 [`Self::exec_batch_atomic`]。
    pub fn exec_batch(&mut self, cmds: &[Value]) -> (Vec<Value>, usize) {
        let mut out = Vec::with_capacity(cmds.len());
        let mut failed = 0;
        for c in cmds {
            let resp = self.exec(c);
            if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                failed += 1;
            }
            out.push(resp);
        }
        (out, failed)
    }

    /// 把整批作为**一个**事务执行（一次撤销可整体回退、一次广播）
    pub fn exec_batch_atomic(&mut self, cmds: &[Value], label: &str) -> (Vec<Value>, usize) {
        self.journal.begin(label);
        let mut out = Vec::with_capacity(cmds.len());
        let mut failed = 0;
        for c in cmds {
            let resp = self.exec(c);
            if resp.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                failed += 1;
            }
            out.push(resp);
        }
        // 事务里的每条命令在发生时就已经广播过了（见 dispatch），commit 只结束事务
        self.journal.commit();
        let _ = label;
        (out, failed)
    }

    fn dispatch(&mut self, op: &str, c: &Value) -> Result<Value, String> {
        // ---- 只读 / 日志命令：不进撤销栈 ----
        match op {
            "summary" => return Ok(self.doc.summary()),
            "new" => {
                // 新建空谱面：**不设保存目标**（path 清空）—— 于是"保存"必然先问目标（Krita 的做法）
                let m = c.get("meta").and_then(|v| v.as_object());
                let get = |k: &str| m.and_then(|m| m.get(k)).and_then(|v| v.as_str()).unwrap_or("").to_owned();
                let bpm = c
                    .get("bpm")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(174.0) as f32;
                let meta = crate::doc::Meta {
                    name: {
                        let n = get("name");
                        if n.is_empty() { "untitled".to_owned() } else { n }
                    },
                    composer: get("composer"),
                    charter: get("charter"),
                    illustrator: get("illustrator"),
                    difficulty: {
                        let d = get("difficulty");
                        if d.is_empty() { "IN".to_owned() } else { d }
                    },
                    level: get("level"),
                    constant: None,
                    offset_ms: c.get("offsetMs").and_then(|v| v.as_i64()).unwrap_or(0),
                    audio: {
                        let a = get("audio");
                        if a.is_empty() { None } else { Some(a) }
                    },
                    // 曲绘/背景：与 `audio` 同口径 —— 表单里填了什么就写什么。
                    // 存 `.opm` 时由 `container::collect_assets` 读进来装进包里（§7.48）
                    background: {
                        let b = get("background");
                        if b.is_empty() { None } else { Some(b) }
                    },
                    foreign: Default::default(),
                };
                self.doc = Document::fresh(meta, if bpm > 0.0 { bpm } else { 174.0 });
                self.journal = Journal::default();
                self.path = None;
                self.source_format = codec::Format::Opm;
                // **上一份谱面的容器资源与解压缓存必须一起清掉**：留着的话，新建的谱面第一次
                // 存成 `.opm` 会把**上一个包的音乐/曲绘**装进去（`collect_assets` 从 `self.assets`
                // 里带过去的），而且解压缓存里那份 `opm.json` 还会被当成"这份新谱面的工作副本"。
                self.assets = Vec::new();
                if let Some(old) = self.asset_dir.take() {
                    let _ = std::fs::remove_dir_all(&old);
                }
                self.revision += 1;
                // 新建之后**是脏的**：还没写进任何文件，界面要提示保存
                self.saved_revision = self.revision.wrapping_sub(1);
                let topics = Self::all_topics();
                self.emit(self.origin, format!("新建 {}", self.doc.meta.name), topics, Vec::new());
                return Ok(json!({
                    "name": self.doc.meta.name,
                    "bpm": self.doc.bpm_list[0].bpm,
                    "lines": self.doc.judge_lines.len(),
                    "path": Value::Null,
                }));
            }
            "load" => {
                let path = c
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or("load 需要 path")?;
                let fid = self.load_into(Path::new(path))?;
                return Ok(json!({
                    "path": path,
                    "format": self.source_format.as_str(),
                    "lines": self.doc.judge_lines.len(),
                    "notes": self.doc.judge_lines.iter().map(|l| l.notes.len()).sum::<usize>(),
                    "fidelity": fidelity_json(&fid),
                }));
            }
            "dump" => return Ok(self.doc.to_json()),
            // 校验结果 JSON **只有一份实现**（`cmd::validate_json`）：`opm-ctl --file x validate`
            // 与 GUI 的这条命令出去的是同一份文本，不给"命令行说有错、界面说没有"留缝
            "validate" => return Ok(crate::cmd::validate_json(&self.doc)),
            "ping" => return Ok(json!({"pong": true, "revision": self.revision})),
            "save" => {
                let path = c.get("path").and_then(|v| v.as_str()).map(PathBuf::from);
                let fmt = SaveFormat::parse(c.get("format").and_then(|v| v.as_str()));
                match (path, fmt) {
                    (Some(p), Some(f)) => {
                        let (p, fid) = self.save_as(&p, f)?;
                        return Ok(json!({
                            "path": p.display().to_string(),
                            "format": f.as_str(),
                            "shape": self.last_save.as_ref().map(|(s, _)| shape_name(*s)),
                            "fidelity": fidelity_json(&fid),
                        }));
                    }
                    (Some(p), None) => {
                        // 显式路径但没指定形态：按扩展名/目标名判（`Auto`）
                        let (p, fid) = self.save_as(&p, SaveFormat::Auto)?;
                        return Ok(json!({
                            "path": p.display().to_string(),
                            "format": "auto",
                            "shape": self.last_save.as_ref().map(|(s, _)| shape_name(*s)),
                            "fidelity": fidelity_json(&fid),
                        }));
                    }
                    _ => {
                        let p = self.save(None)?;
                        return Ok(json!({
                            "path": p.display().to_string(),
                            "format": self.source_format.as_str(),
                            "fidelity": self.last_fidelity.as_ref().map(fidelity_json),
                        }));
                    }
                }
            }
            "render" => {
                let at = c.get("at").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let lookahead = c.get("lookahead").and_then(|v| v.as_f64()).unwrap_or(2.0);
                let width = c.get("width").and_then(|v| v.as_u64()).unwrap_or(1280) as u32;
                let height = c.get("height").and_then(|v| v.as_u64()).unwrap_or(720) as u32;
                let out = c
                    .get("out")
                    .and_then(|v| v.as_str())
                    .ok_or("render 需要 out（PNG 路径）")?;
                let line_len = c
                    .get("lineLen")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(crate::state::RPE_LINE_LEN_DEFAULT as f64) as f32;
                let boundary = c.get("boundary").and_then(|v| v.as_bool()).unwrap_or(true);
                crate::headless::render_png(
                    &self.doc,
                    at,
                    width,
                    height,
                    std::path::Path::new(out),
                    crate::headless::RenderOpts {
                        lookahead,
                        line_len,
                        boundary,
                    },
                )?;
                return Ok(json!({"out": out, "at": at}));
            }
            "undo" => {
                let label = self.undo()?;
                return Ok(json!({
                    "undone": label, "undoDepth": self.journal.undo_depth(),
                    "redoDepth": self.journal.redo_depth(),
                }));
            }
            "redo" => {
                let label = self.redo()?;
                return Ok(json!({
                    "redone": label, "undoDepth": self.journal.undo_depth(),
                    "redoDepth": self.journal.redo_depth(),
                }));
            }
            "begin" => {
                let label = c.get("label").and_then(|v| v.as_str()).unwrap_or("tx");
                self.journal.begin(label);
                return Ok(json!({"transaction": label}));
            }
            "commit" => {
                // 事务里的每条改动在发生时就已经广播过了，这里只结束事务（避免重复广播）
                let committed = self.journal.commit();
                let label = committed.as_ref().map(Change::label);
                return Ok(json!({
                    "committed": label,
                    "undoDepth": self.journal.undo_depth(),
                }));
            }
            "abort" => {
                let had = self.journal.in_transaction();
                // 回滚前记下"事务里到底改了几条"：真要撤销了东西才算一次文档变更（+1 revision），
                // 空事务 abort 不该平白多一个版本号
                let pending = self.journal.pending_len();
                if let Err(e) = self.journal.abort(&mut self.doc) {
                    self.log.push(format!("abort 回滚异常: {e}"));
                }
                if had {
                    if pending > 0 {
                        // 文档内容被回滚**也是一次变更**：不 +1 的话这条广播会带着和上一条相同的
                        // revision，按 revision 增量拉取的订阅者（`{"op":"broadcasts","since":N}`）会漏掉它
                        self.revision += 1;
                    }
                    // 事务里的改动被丢弃：让订阅者按全量话题重取一次，避免界面停在幻影状态
                    let origin = self.origin;
                    // 回滚可能跨多条线 ⇒ 重叠检测全量重算（否则会留下"已经不存在了"的冲突）
                    self.refresh_overlaps_all();
                    self.emit(origin, "abort(revert)".into(), Self::all_topics(), Vec::new());
                }
                return Ok(json!({"aborted": true}));
            }
            "broadcasts" => {
                let since = c.get("since").and_then(|v| v.as_u64()).unwrap_or(0);
                let n = c.get("recent").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let items: Vec<Value> = self
                    .broadcasts
                    .iter()
                    .filter(|b| b.revision > since)
                    .rev()
                    .take(n)
                    .map(|b| {
                        json!({
                            "revision": b.revision,
                            "origin": format!("{:?}", b.origin),
                            "label": b.label,
                            "topics": b.topics.iter().map(Topic::label).collect::<Vec<_>>(),
                            "changes": b.changes,
                        })
                    })
                    .collect();
                return Ok(json!({
                    "count": self.broadcasts.len(),
                    "subscribers": self.subscribers.len(),
                    "lastDelivered": self.last_delivered,
                    "revision": self.revision,
                    "items": items,
                }));
            }
            // 事件重叠：**查询**（不算改动、不进撤销栈）。GUI 直接读 `overlaps()`，
            // CLI/控制通道问这一条 —— 两条路走的是同一份缓存、同一份算法。
            "overlaps" => {
                let items: Vec<Value> = self
                    .overlaps
                    .iter()
                    .map(|o| {
                        json!({
                            "line": o.line,
                            "layer": o.layer,
                            "track": o.track,
                            "prev": o.prev,
                            "next": o.next,
                            "startBeat": [o.start.n, o.start.d],
                            "endBeat": [o.end.n, o.end.d],
                            "pointer": o.pointer(),
                            "label": o.label(),
                        })
                    })
                    .collect();
                return Ok(json!({"count": self.overlaps.len(), "items": items}));
            }
            "journal" => {
                let n = c.get("recent").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
                return Ok(json!({
                    "undoDepth": self.journal.undo_depth(),
                    "redoDepth": self.journal.redo_depth(),
                    "bytes": self.journal.bytes(),
                    "capBytes": self.journal.cap_bytes(),
                    "applied": self.journal.applied,
                    "undone": self.journal.undone,
                    "recent": self.journal.recent(n),
                }));
            }
            "patch" => return Ok(self.journal.patch()),
            _ => {}
        }

        // ---- 改动命令：单条自成事务（除非外层已 begin） ----
        let auto = !self.journal.in_transaction();
        if auto {
            self.journal.begin(op);
        }
        let origin = self.origin;
        let pending_before = self.journal.pending_len();
        match self.mutate(op, c) {
            Ok(v) => {
                let recorded = self.journal.pending_since(pending_before);
                let committed = if auto { self.journal.commit() } else { None };
                self.revision += 1;
                // **每次成功改动都广播**（事务内也广播）。
                // 早先"事务内压着不发、commit 时合并成一条"是为了让广播粒度等于撤销粒度，
                // 但那让"拖拽"这类连续编辑在整段手势里界面不更新 —— 订阅者要的是"文档变了"，
                // 撤销怎么分组是另一件事。abort 仍发一条全量话题让订阅者重取。
                let _ = committed;
                // 文档变了 ⇒ 重叠检测跟着走（只重查动过的那条线；见 `refresh_overlaps`）
                self.refresh_overlaps(&recorded);
                self.broadcast_recorded(origin, op, &recorded);
                Ok(v)
            }
            Err(e) => {
                // 命令失败：**只回滚它自己写进去的那几条**（`normalize`/`move_notes` 这类可能边算边写）。
                // 以前这里是 `if auto { journal.abort(...) }` —— 显式事务里一条命令失败会把半成品
                // 留在文档上：既没广播、也没置脏，"失败的命令不改变文档"就成了假话。
                // 回滚是净效果为零的操作，所以**不推进 revision、也不广播**（订阅者从没看见过半成品）。
                match self.journal.rollback_since(&mut self.doc, pending_before) {
                    Ok(0) => {}
                    Ok(k) => self.log.push(format!("{op} 失败，已回滚它自己写下的 {k} 条改动")),
                    Err(e2) => self.log.push(format!("失败回滚异常: {e2}")),
                }
                if auto {
                    // 自动事务（单条命令自成一步）失败后要把它关掉，别让下一条命令 join 进来
                    if let Err(e2) = self.journal.abort(&mut self.doc) {
                        self.log.push(format!("失败回滚异常: {e2}"));
                    }
                }
                Err(e)
            }
        }
    }

    // ---------------------------------------------------------------- 具体命令

    fn mutate(&mut self, op: &str, c: &Value) -> Result<Value, String> {
        match op {
            "add_note" => {
                let line_idx = line_arg(c)?;
                let kind = NoteKind::parse(c.get("kind").and_then(|v| v.as_str()).unwrap_or(""))
                    .ok_or_else(|| format!("未知音符类型 {:?}（可选 tap/hold/drag/flick）", c.get("kind")))?;
                let start = beat_arg(c, "startBeat")?;
                let mut note = Note::new(kind, start, num(c, "laneX").unwrap_or(0.0) as f32);
                if kind == NoteKind::Hold {
                    let end = beat_arg(c, "endBeat").map_err(|_| "hold 必须提供 endBeat".to_string())?;
                    if end <= start {
                        return Err("hold 的 endBeat 必须大于 startBeat".into());
                    }
                    note.end = Some(end);
                }
                apply_note_set(&mut note, c.get("set"))?;
                let line = self.line_mut(line_idx)?;
                let index = line.notes.len();
                line.notes.push(note.clone());
                self.journal.record(Change::InsertNote {
                    line: line_idx,
                    index,
                    note: Box::new(note),
                });
                Ok(json!({"line": line_idx, "index": index, "notes": self.line(line_idx)?.notes.len()}))
            }
            "set_note" => {
                let line_idx = line_arg(c)?;
                let index = usize_arg(c, "index")?;
                let before = self
                    .line(line_idx)?
                    .notes
                    .get(index)
                    .cloned()
                    .ok_or_else(|| format!("音符索引 {index} 越界"))?;
                let mut after = before.clone();
                apply_note_set(&mut after, c.get("set"))?;
                self.line_mut(line_idx)?.notes[index] = after.clone();
                self.journal.record(Change::SetNote {
                    line: line_idx,
                    index,
                    before: Box::new(before),
                    after: Box::new(after),
                });
                Ok(json!({"line": line_idx, "index": index}))
            }
            "del_note" => {
                let line_idx = line_arg(c)?;
                let index = usize_arg(c, "index")?;
                let line = self.line_mut(line_idx)?;
                if index >= line.notes.len() {
                    return Err(format!("音符索引 {index} 越界（共 {}）", line.notes.len()));
                }
                let removed = line.notes.remove(index);
                self.journal.record(Change::RemoveNote {
                    line: line_idx,
                    index,
                    note: Box::new(removed.clone()),
                });
                Ok(json!({"line": line_idx, "removed": {"kind": removed.kind.as_str(), "startBeat": removed.start}}))
            }
            "move_notes" => {
                let line_idx = line_arg(c)?;
                let delta = beat_arg(c, "delta")?;
                let before = self.line(line_idx)?.notes.clone();
                let mut after = before.clone();
                for n in after.iter_mut() {
                    n.start = add_beat(n.start, delta).ok_or("拍数溢出")?;
                    if let Some(e) = n.end {
                        n.end = Some(add_beat(e, delta).ok_or("拍数溢出")?);
                    }
                }
                self.line_mut(line_idx)?.notes = after.clone();
                self.journal.record(Change::NotesOfLine {
                    line: line_idx,
                    before,
                    after,
                });
                Ok(json!({"line": line_idx, "movedNotes": self.line(line_idx)?.notes.len()}))
            }
            "add_line" => {
                let mut l = JudgeLine::default();
                if let Some(name) = c.get("name").and_then(|v| v.as_str()) {
                    l.name = name.to_owned();
                }
                if let Some(b) = num(c, "bpmFactor") {
                    l.bpm_factor = b as f32;
                }
                let index = self.doc.judge_lines.len();
                self.doc.judge_lines.push(l.clone());
                self.journal.record(Change::InsertLine {
                    index,
                    line: Box::new(l),
                });
                Ok(json!({"index": index}))
            }
            "del_line" => {
                let index = line_arg(c)?;
                if index >= self.doc.judge_lines.len() {
                    return Err(format!("判定线索引 {index} 越界"));
                }
                let removed = self.doc.judge_lines.remove(index);
                self.journal.record(Change::RemoveLine {
                    index,
                    line: Box::new(removed),
                });
                Ok(json!({"lines": self.doc.judge_lines.len()}))
            }
            "set_line" => {
                let index = line_arg(c)?;
                let before = LineProps::of(self.line(index)?);
                let mut after = before.clone();
                if let Some(set) = c.get("set").and_then(|v| v.as_object()) {
                    for (k, v) in set {
                        match k.as_str() {
                            "name" => after.name = v.as_str().unwrap_or("Untitled").to_owned(),
                            "bpmFactor" => after.bpm_factor = v.as_f64().unwrap_or(1.0) as f32,
                            "zOrder" => after.z_order = v.as_i64().unwrap_or(0) as i32,
                            "isCover" => after.is_cover = v.as_bool().unwrap_or(true),
                            other => return Err(format!("set_line 不支持的字段 {other}")),
                        }
                    }
                }
                after.apply_to(self.line_mut(index)?);
                self.journal.record(Change::SetLine {
                    index,
                    before: Box::new(before),
                    after: Box::new(after),
                });
                Ok(json!({"line": index}))
            }
            "add_event" => {
                let (line_idx, layer_idx, track) = layer_arg(c)?;
                let start = beat_arg(c, "startBeat")?;
                let end = beat_arg(c, "endBeat")?;
                if end <= start {
                    return Err("endBeat 必须大于 startBeat".into());
                }
                let easing = c
                    .get("easing")
                    .and_then(|v| v.as_str())
                    .unwrap_or("linear")
                    .to_owned();
                if !is_easing(&easing) {
                    return Err(format!("未知缓动 {easing:?}"));
                }
                let ev = Event::new(
                    start,
                    end,
                    c.get("startValue").cloned().unwrap_or(json!(0.0)),
                    c.get("endValue").cloned().unwrap_or(json!(0.0)),
                    &easing,
                );
                let list = self.track_mut(line_idx, layer_idx, &track)?;
                let index = list.len();
                list.push(ev.clone());
                let list = self.track_mut(line_idx, layer_idx, &track)?;
                list.sort_by(|a, b| a.start.cmp(&b.start));
                let index = list.iter().position(|e| e.start == ev.start && e.end == ev.end).unwrap_or(index);
                self.journal.record(Change::InsertEvent {
                    line: line_idx,
                    layer: layer_idx,
                    track: track.clone(),
                    index,
                    event: Box::new(ev),
                });
                Ok(json!({"line": line_idx, "index": index}))
            }
            "set_event" => {
                let (line_idx, layer_idx, track) = layer_arg(c)?;
                let index = usize_arg(c, "index")?;
                let before = self
                    .track(line_idx, layer_idx, &track)?
                    .get(index)
                    .cloned()
                    .ok_or_else(|| format!("事件索引 {index} 越界"))?;
                let mut after = before.clone();
                if let Some(o) = c.get("set").and_then(|v| v.as_object()) {
                    for (k, v) in o {
                        match k.as_str() {
                            "startBeat" => after.start = parse_beat(v)?,
                            "endBeat" => after.end = parse_beat(v)?,
                            "startValue" => after.start_value = v.clone(),
                            "endValue" => after.end_value = v.clone(),
                            "easing" => {
                                let s = v.as_str().unwrap_or("linear");
                                if !is_easing(&s) {
                                    return Err(format!("未知缓动 {s:?}"));
                                }
                                after.easing = s.to_owned();
                            }
                            other => return Err(format!("set_event 不支持的字段 {other}")),
                        }
                    }
                }
                if after.end <= after.start {
                    return Err("endBeat 必须大于 startBeat".into());
                }
                self.track_mut(line_idx, layer_idx, &track)?[index] = after.clone();
                self.journal.record(Change::SetEvent {
                    line: line_idx,
                    layer: layer_idx,
                    track,
                    index,
                    before: Box::new(before),
                    after: Box::new(after),
                });
                Ok(json!({"line": line_idx, "index": index}))
            }
            "del_event" => {
                let (line_idx, layer_idx, track) = layer_arg(c)?;
                let index = usize_arg(c, "index")?;
                let list = self.track_mut(line_idx, layer_idx, &track)?;
                if index >= list.len() {
                    return Err(format!("事件索引 {index} 越界（共 {}）", list.len()));
                }
                let removed = list.remove(index);
                self.journal.record(Change::RemoveEvent {
                    line: line_idx,
                    layer: layer_idx,
                    track: track.clone(),
                    index,
                    event: Box::new(removed),
                });
                Ok(json!({"line": line_idx, "events": self.track(line_idx, layer_idx, &track)?.len()}))
            }
            // 拖事件块的头/尾：**只改这一个事件**的 start 或 end。
            //
            // 早先这里会**同步相邻事件**（把邻块的边界一起挪）来维持"无空隙无重叠"，结果是一次拖拽
            // 同时改了两个事件 —— 用户明确说这是错的（"拖动控制柄仍然会同时控制两个事件"）。
            // 现在的语义：控制柄只控制它所属的那一个事件；由此产生的**空隙/重叠**由检测机制报出来
            // （空隙里判定线的行为由求值器保证"保持"而不是跳变，见 `perf::eval_events`）。
            "resize_event" => {
                let (line_idx, layer_idx, track) = layer_arg(c)?;
                let index = usize_arg(c, "index")?;
                let edge = c
                    .get("edge")
                    .and_then(|v| v.as_str())
                    .ok_or("resize_event 需要 edge（start|end）")?
                    .to_owned();
                let to = beat_arg(c, "toBeat")?;
                let before = self.track(line_idx, layer_idx, &track)?;
                if index >= before.len() {
                    return Err(format!("事件索引 {index} 越界（共 {}）", before.len()));
                }
                let mut after = before.clone();
                match edge.as_str() {
                    "start" => {
                        if to >= after[index].end {
                            return Err(format!(
                                "start({}) 必须早于该事件的 end({})",
                                to.to_f64(),
                                after[index].end.to_f64()
                            ));
                        }
                        after[index].start = to;
                    }
                    "end" => {
                        if to <= after[index].start {
                            return Err(format!(
                                "end({}) 必须晚于该事件的 start({})",
                                to.to_f64(),
                                after[index].start.to_f64()
                            ));
                        }
                        after[index].end = to;
                    }
                    other => return Err(format!("edge 只能是 start|end，收到 {other:?}")),
                }
                *self.track_mut(line_idx, layer_idx, &track)? = after.clone();
                self.journal.record(Change::ReplaceTrack {
                    line: line_idx,
                    layer: layer_idx,
                    track: track.clone(),
                    before,
                    after: after.clone(),
                });
                Ok(json!({
                    "line": line_idx, "index": index, "edge": edge,
                    "startBeat": after[index].start,
                    "endBeat": after[index].end,
                }))
            }
            "split_event" => {
                let (line_idx, layer_idx, track) = layer_arg(c)?;
                let index = usize_arg(c, "index")?;
                let at = beat_arg(c, "atBeat")?;
                let before = self.track(line_idx, layer_idx, &track)?;
                let ev = before
                    .get(index)
                    .cloned()
                    .ok_or_else(|| format!("事件索引 {index} 越界"))?;
                if at <= ev.start || at >= ev.end {
                    return Err(format!(
                        "atBeat({}) 必须落在事件的 ({}, {}) 开区间内",
                        at.to_f64(),
                        ev.start.to_f64(),
                        ev.end.to_f64()
                    ));
                }
                // 切点上的值**问求值器**（`perf::track_value`）：它带缓动（折线实现），
                // 五条轨道同一条口径。
                //
                // 这里曾写死"线性插值 + `Value::Number` 守卫"：于是一条非线性缓动的事件被切一刀，
                // 切点上的数与**预览显示的**不是同一个数 —— 切完当场多出一个跳变，
                // 而"切一刀不改变表演"正是这个操作的全部意义。
                let mid = if matches!(
                    (&ev.start_value, &ev.end_value),
                    (Value::Number(_), Value::Number(_))
                ) {
                    let tmap = crate::perf::TimeMap::from_doc(&self.doc);
                    json!(crate::perf::track_value(std::slice::from_ref(&ev), at.to_f64(), &tmap)
                        .unwrap_or_default())
                } else {
                    // 非数值端点：原样搬运（求值器把它们算成 0，那是预览的口径，不是这里的口径）
                    ev.start_value.clone()
                };
                let mut after = before.clone();
                let mut a = ev.clone();
                a.end = at;
                a.end_value = mid.clone();
                let mut b = ev;
                b.start = at;
                b.start_value = mid;
                after[index] = a;
                after.insert(index + 1, b);
                let list = self.track_mut(line_idx, layer_idx, &track)?;
                *list = after.clone();
                self.journal.record(Change::ReplaceTrack {
                    line: line_idx,
                    layer: layer_idx,
                    track: track.clone(),
                    before,
                    after,
                });
                Ok(json!({"line": line_idx, "index": index, "events": self.track(line_idx, layer_idx, &track)?.len()}))
            }
            // ---- 「就位目标」：一次给出判定线该在哪儿，四轨一起写（用户要求）----
            //
            // 用户口径：「事件块结束点上，给一个单次事件目标设置（x/y 坐标，透明度，角度），
            // 给一次事件块末尾的值，以保证最终 0 误差就位」。
            //
            // 与 `set_event` 的区别：那条改**一条轨道的一个端值**，这条要的是"**线此刻在哪儿**"
            // —— 一次给四个数（x / y / angle / alpha），工具负责把该拍上的值写成目标，
            // 并且**保证按位相等**（求值器端点是按定义取端值，见 `perf::endpoint_value`）。
            //
            // 四轨各按"该拍处是什么情形"选动作（顺序即优先级）：
            // · 有事件**正好结束**在该拍 ⇒ 写它的终值（若下一条也正好在该拍**开始**，一并写它的起值，
            //   否则"这一瞬"求值到的会是下一条的起值 —— 那就不是 0 误差了）；
            // · 该拍落在某事件**内部** ⇒ 先按 `split_event` 切一刀（切点值 = 求值器的值，不跳变），
            //   再写左右两半的相邻端值；
            // · 该拍处于**空位**（前一块已结束）⇒ 写**前一块的终值** —— 空位的值就是它（不是全局默认）；
            // · 该拍在**首事件之前** ⇒ 写首事件的**起始值**（`track_value` 对"块前"的口径就是它）；
            // · 轨道**一条事件都没有** ⇒ 不动它（无从下手，报告里说明；铺满可用 `set_track_constant`）。
            //
            // 值已经按位等于目标的轨道一律**跳过** —— 于是"只改 X"不会顺手切别人的块。
            // 整条命令是**一个撤销步**（事务）。
            //
            // 流速轨**不在其中**：它不是"坐标"（音符位置是它的积分，不是一次就位的目标）。
            "set_target" => {
                let line_idx = line_arg(c)?;
                let layer_idx = c.get("layer").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let at = beat_arg(c, "atBeat")?;
                let target = c
                    .get("target")
                    .and_then(|v| v.as_object())
                    .ok_or("缺少 target 对象（x / y / angle / alpha，至少给一个）")?;
                // 键名是**坐标口径**（x/y/角度/透明度），不是内部轨道名 —— 这是给人/脚本用的接口
                let mut wanted: Vec<(&'static str, f64)> = Vec::new();
                for (k, track) in [("x", "moveX"), ("y", "moveY"), ("angle", "rotate"), ("alpha", "alpha")] {
                    if let Some(v) = target.get(k) {
                        let v = v.as_f64().ok_or_else(|| format!("target.{k} 需为数字"))?;
                        if !v.is_finite() {
                            return Err(format!("target.{k} 需为有限数"));
                        }
                        wanted.push((track, v));
                    }
                }
                if wanted.is_empty() {
                    return Err("target 至少要给一个键：x / y / angle / alpha".into());
                }

                // ---- 先算计划（只读），再统一施加 ----
                let mut cmds: Vec<Value> = Vec::new();
                let mut plan: Vec<Value> = Vec::new();
                for (track, v) in wanted {
                    let list = self.track(line_idx, layer_idx, track)?;
                    let neutral = crate::state::TrackId::from_key(track)
                        .map(crate::edit::track_neutral_value)
                        .unwrap_or(0.0);
                    // "此刻的值" = 求值器的值（缓动折线 + 端点按定义）—— 判"要不要写"用的就是它
                    let tmap = crate::perf::TimeMap::from_doc(&self.doc);
                    let current =
                        crate::perf::track_value(&list, at.to_f64(), &tmap).unwrap_or(neutral);
                    if current.to_bits() == v.to_bits() {
                        plan.push(json!({"track": track, "action": "skip", "why": "已经是这个值"}));
                        continue;
                    }
                    if list.is_empty() {
                        plan.push(json!({"track": track, "action": "skip",
                                         "why": "这条轨道还没有事件（先放一块，或用 set_track_constant 铺满全谱）"}));
                        continue;
                    }
                    let at_json = json!([at.n, at.d]);
                    let set = |index: usize, key: &str| -> Value {
                        let mut s = serde_json::Map::new();
                        s.insert(key.to_owned(), json!(v));
                        json!({"op": "set_event", "line": line_idx, "layer": layer_idx,
                               "track": track, "index": index, "set": Value::Object(s)})
                    };
                    // **要改的是"此刻生效的那一块"** —— 判据必须是 `perf::active_event`：
                    // 它正是求值器用的那一条（起点不晚于该拍的最后一条）。早先按"哪一个块结束在这一拍"
                    // 去找，在**重叠**时就会改到一个根本不被求值的事件上：实测（一条 [0,8] 的斜坡 +
                    // 一条后加的 [0,500] 常量）就位到 250.3 之后，那一刻求值到的仍是 7.0 ——
                    // 命令报成功、画面没动。这是"同一个问题两套判据"的又一个实例。
                    let action;
                    match crate::perf::active_event(&list, at.to_f64()) {
                        // 该拍在首事件之前：这条轨道的值取**首事件的起始值**（`track_value` 的口径）
                        None => {
                            cmds.push(set(0, "startValue"));
                            action = "块前（值取首块起始值 ⇒ 写首块的起值）";
                        }
                        Some(i) => {
                            let e = &list[i];
                            if e.start == at {
                                cmds.push(set(i, "startValue"));
                                action = "块首（这一刻正好是它的起点）";
                            } else if e.end == at {
                                cmds.push(set(i, "endValue"));
                                action = "块末（本来就在边界上）";
                            } else if at < e.end {
                                cmds.push(json!({"op": "split_event", "line": line_idx, "layer": layer_idx,
                                                 "track": track, "index": i, "atBeat": at_json}));
                                cmds.push(set(i, "endValue"));
                                cmds.push(set(i + 1, "startValue"));
                                action = "块内（先在此切一刀，再写两侧端值）";
                            } else {
                                cmds.push(set(i, "endValue"));
                                action = "空位（空位的值来自这一块 ⇒ 写它的终值）";
                            }
                        }
                    }
                    plan.push(json!({"track": track, "action": action, "value": v}));
                }
                let tracks_written = plan.iter().filter(|p| p["action"] != "skip").count();
                if cmds.is_empty() {
                    return Ok(json!({"line": line_idx, "atBeat": [at.n, at.d], "wrote": 0, "cmds": 0,
                                     "plan": plan, "note": "四轨都已经在目标上（或无从下手），没有改动"}));
                }

                // ---- 施加：一个事务 = 一个撤销步 ----
                //
                // 已经在事务里（例如 `--atomic` 批次）时**不自己开/关**：`journal.begin` 不嵌套，
                // 这里 commit 会把外层事务提前收掉。
                let nested = self.journal.in_transaction();
                if !nested {
                    self.journal.begin("set_target");
                }
                let mut failed: Vec<Value> = Vec::new();
                for cmd in &cmds {
                    let r = self.exec(cmd);
                    if r.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                        failed.push(json!({"cmd": cmd, "error": r.get("error")}));
                    }
                }
                if !nested {
                    self.journal.commit();
                }
                Ok(json!({
                    "line": line_idx,
                    "atBeat": [at.n, at.d],
                    // `wrote` 数**轨道**（调用方关心的"几条轨就位了"）；
                    // `cmds` 是实际落地的子命令数（块内那种情形一条轨道要 3 条：切一刀 + 写两侧）
                    "wrote": tracks_written.saturating_sub(failed.len().min(tracks_written)),
                    "cmds": cmds.len() - failed.len(),
                    "plan": plan,
                    "failed": failed,
                }))
            }
            "set_track_constant" => {
                let (line_idx, layer_idx, track) = layer_arg(c)?;
                let value = c.get("value").cloned().unwrap_or(json!(0.0));
                let end = add_beat(self.doc.chart_end(), Beat::new(1024, 1)).ok_or("拍数溢出")?;
                let before = self.track(line_idx, layer_idx, &track)?;
                let after = vec![Event::new(Beat::zero(), end, value.clone(), value.clone(), "linear")];
                *self.track_mut(line_idx, layer_idx, &track)? = after.clone();
                self.journal.record(Change::ReplaceTrack {
                    line: line_idx,
                    layer: layer_idx,
                    track,
                    before,
                    after,
                });
                Ok(json!({"line": line_idx, "value": value, "span": [0.0, end.to_f64()]}))
            }
            "normalize" => {
                // **规范化的唯一实现在 codec**（导入侧用的就是它）：排序 → 丢零长度 →
                // 相邻处把**前一条**的 `end` 挪到下一个 `start`（重叠时后一条的起点**不动**）→
                // 首事件补到拍 0 → 末事件延拓到谱面结束；延长只对常量事件直接改 `end`，
                // 斜坡一律"原样 + 追加常量段"（拉长会改斜率）。
                //
                // 早先这里另有一份实现，规则**与导入侧相反**（把后一条事件挪到前一条的终点）——
                // 于是同一个 bug 有第三种表现：刚放好的事件被 `normalize` 挪走。
                let end = add_beat(self.doc.chart_end(), Beat::new(1024, 1)).ok_or("拍数溢出")?;
                let tmap = crate::perf::TimeMap::from_parts(&self.doc.bpm_list, self.doc.chart_end());
                let mut fixes = 0usize;
                let mut touched: Vec<(usize, usize, String, Vec<Event>, Vec<Event>)> = Vec::new();
                for (li, line) in self.doc.judge_lines.iter().enumerate() {
                    for (yi, layer) in line.layers.iter().enumerate() {
                        for track in TRACKS {
                            let Some(list) = layer.track(track) else { continue };
                            if list.is_empty() {
                                continue;
                            }
                            let before = list.clone();
                            let mut fid = codec::Fidelity::new("normalize", String::new());
                            let (after, st) = codec::normalize_track(
                                before.clone(),
                                &tmap,
                                end,
                                &format!("/judgeLines[{li}].layers[{yi}].{track}"),
                                &mut fid,
                            );
                            fixes += st.dropped
                                + st.gaps
                                + st.overlaps
                                + usize::from(st.sorted)
                                + usize::from(st.prepended)
                                + usize::from(st.extended_to_end);
                            if after != before {
                                touched.push((li, yi, track.to_owned(), before, after));
                            }
                        }
                    }
                }
                for (li, yi, track, before, after) in touched {
                    *self.track_mut(li, yi, &track)? = after.clone();
                    self.journal.record(Change::ReplaceTrack {
                        line: li,
                        layer: yi,
                        track,
                        before,
                        after,
                    });
                }                Ok(json!({"fixes": fixes, "chartEnd": self.doc.chart_end().to_f64()}))
            }
            "set_bpm" => {
                let index = usize_arg(c, "index")?;
                let bpm = num(c, "bpm").ok_or("缺少 bpm")? as f32;
                if bpm <= 0.0 {
                    return Err("bpm 必须大于 0".into());
                }
                let before = self
                    .doc
                    .bpm_list
                    .get(index)
                    .cloned()
                    .ok_or_else(|| format!("BPM 索引 {index} 越界"))?;
                let mut after = before.clone();
                after.bpm = bpm;
                self.doc.bpm_list[index] = after.clone();
                self.journal.record(Change::SetBpm {
                    index,
                    before: Box::new(before),
                    after: Box::new(after),
                });
                Ok(json!({"index": index, "bpm": bpm}))
            }
            "set_meta" => {
                let set = c.get("set").and_then(|v| v.as_object()).ok_or("缺少 set 对象")?;
                let before = self.doc.meta.clone();
                let mut after = before.clone();
                for (k, v) in set {
                    match k.as_str() {
                        "name" => after.name = v.as_str().unwrap_or("").to_owned(),
                        "composer" => after.composer = v.as_str().unwrap_or("").to_owned(),
                        "charter" => after.charter = v.as_str().unwrap_or("").to_owned(),
                        "illustrator" => after.illustrator = v.as_str().unwrap_or("").to_owned(),
                        "difficulty" => after.difficulty = v.as_str().unwrap_or("IN").to_owned(),
                        "level" => after.level = v.as_str().unwrap_or("").to_owned(),
                        "offsetMs" => after.offset_ms = v.as_i64().unwrap_or(0),
                        // 音频**是文档字段**（会写进谱面文件），所以改它必须走这里（EditCore）——
                        // 而"只换预览用的音频"是视图命令 `{"op":"audio"}`，两者刻意分开。
                        "audio" => {
                            after.audio = if v.is_null() {
                                None
                            } else {
                                Some(v.as_str().unwrap_or("").to_owned())
                            }
                        }
                        // 背景/曲绘同样是**文档字段**（容器会把文件装进去），所以也走这里
                        "background" => {
                            after.background = if v.is_null() {
                                None
                            } else {
                                Some(v.as_str().unwrap_or("").to_owned())
                            }
                        }
                        other => return Err(format!("set_meta 不支持的字段 {other}")),
                    }
                }
                self.doc.meta = after.clone();
                self.journal.record(Change::SetMeta {
                    before: Box::new(before),
                    after: Box::new(after),
                });
                Ok(json!({"name": self.doc.meta.name}))
            }
            other => Err(format!("未知命令 {other:?}")),
        }
    }

    // ---------------------------------------------------------------- 取用助手

    fn line(&self, i: usize) -> Result<&JudgeLine, String> {
        self.doc
            .judge_lines
            .get(i)
            .ok_or_else(|| format!("判定线索引 {i} 越界（共 {}）", self.doc.judge_lines.len()))
    }
    fn line_mut(&mut self, i: usize) -> Result<&mut JudgeLine, String> {
        let n = self.doc.judge_lines.len();
        self.doc
            .judge_lines
            .get_mut(i)
            .ok_or_else(|| format!("判定线索引 {i} 越界（共 {n}）"))
    }
    /// 返回 owned Vec —— 早先图省事用 `Box::leak` 返回引用，那是内存泄漏，已改掉
    fn track(&self, line: usize, layer: usize, track: &str) -> Result<Vec<Event>, String> {
        read_track(&self.doc, line, layer, track)
            .ok_or_else(|| format!("无法读取轨道 line={line} layer={layer} track={track}"))
    }
    fn track_mut(&mut self, line: usize, layer: usize, track: &str) -> Result<&mut Vec<Event>, String> {
        let l = self.line_mut(line)?;
        let lay = l
            .layers
            .get_mut(layer)
            .ok_or_else(|| format!("层索引 {layer} 越界"))?;
        lay.track_mut(track)
            .ok_or_else(|| format!("未知轨道 {track}（可选 {TRACKS:?}）"))
    }
}

// ---------------------------------------------------------------- 容器保存时的资源名规范化

/// 一条"资源字段要改成包内相对名"的改名计划（保存 `.opm` 容器时用）。
///
/// 它是**计划**、不是动作：调用方先按它改写一份副本去写盘，写成功之后才把改名落到内存文档
/// （走命令路径）。这样"保存"要么完整发生，要么什么都没发生。
struct AssetRename {
    /// 文档字段名（`set_meta` 的参数名）
    field: &'static str,
    /// 给人看的字段名（写进保真度报告）
    label: &'static str,
    from: String,
    to: String,
}

impl AssetRename {
    fn apply(&self, doc: &mut Document) {
        match self.field {
            "audio" => doc.meta.audio = Some(self.to.clone()),
            "background" => doc.meta.background = Some(self.to.clone()),
            other => debug_assert!(false, "未知资源字段 {other}"),
        }
    }
}

/// 算一遍"保存容器时哪些资源字段要改成包内相对名"（**纯函数**，不改任何东西）。
///
/// 规则：取文件名。`meta.audio = "/tmp/x/song.ogg"` ⇒ `song.ogg`；已经是裸文件名就什么都不做。
fn planned_asset_renames(doc: &Document) -> Vec<AssetRename> {
    let mut out = Vec::new();
    for (field, label, cur) in [
        ("audio", "音乐", doc.meta.audio.as_deref()),
        ("background", "曲绘/背景", doc.meta.background.as_deref()),
    ] {
        let Some(cur) = cur else { continue };
        let Some(base) = Path::new(cur).file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if base != cur {
            out.push(AssetRename {
                field,
                label,
                from: cur.to_owned(),
                to: base.to_owned(),
            });
        }
    }
    out
}

// ---------------------------------------------------------------- 参数解析助手

fn line_arg(c: &Value) -> Result<usize, String> {
    Ok(c.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as usize)
}

fn layer_arg(c: &Value) -> Result<(usize, usize, String), String> {
    let layer = c.get("layer").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let track = c
        .get("track")
        .and_then(|v| v.as_str())
        .ok_or("缺少 track（moveX/moveY/rotate/alpha/speed）")?
        .to_owned();
    if !TRACKS.contains(&track.as_str()) {
        return Err(format!("未知轨道 {track}（可选 {TRACKS:?}）"));
    }
    Ok((line_arg(c)?, layer, track))
}

fn usize_arg(c: &Value, name: &str) -> Result<usize, String> {
    c.get(name)
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .ok_or_else(|| format!("缺少或非法的 {name}（需非负整数）"))
}

fn num(c: &Value, name: &str) -> Option<f64> {
    c.get(name).and_then(|v| v.as_f64())
}

fn beat_arg(c: &Value, name: &str) -> Result<Beat, String> {
    parse_beat(c.get(name).ok_or_else(|| format!("缺少 {name}"))?)
}

fn add_beat(a: Beat, b: Beat) -> Option<Beat> {
    Some(Beat::new(
        a.n.checked_mul(b.d)?.checked_add(b.n.checked_mul(a.d)?)?,
        a.d.checked_mul(b.d)?,
    ))
}

fn apply_note_set(n: &mut Note, set: Option<&Value>) -> Result<(), String> {
    let Some(Value::Object(o)) = set else { return Ok(()) };
    for (k, v) in o {
        match k.as_str() {
            "kind" => {
                n.kind = NoteKind::parse(v.as_str().unwrap_or(""))
                    .ok_or_else(|| format!("未知音符类型 {v:?}"))?
            }
            "startBeat" | "start" => n.start = parse_beat(v)?,
            "endBeat" | "end" => {
                n.end = if v.is_null() { None } else { Some(parse_beat(v)?) };
            }
            "laneX" | "lane_x" => n.lane_x = v.as_f64().ok_or("laneX 需为数字")? as f32,
            "side" => n.side = v.as_str().unwrap_or("above").to_owned(),
            "isFake" | "is_fake" => n.is_fake = v.as_bool().unwrap_or(false),
            "alpha" => {
                // `as u16` 会静默截断：alpha:70000 曾变成 4464 且不报错 —— 解析侧就得拦住
                let a = v.as_u64().ok_or("alpha 需为非负整数")?;
                n.alpha = u16::try_from(a).map_err(|_| format!("alpha 超出 u16 范围: {a}"))?;
            }
            "speed" => n.speed = v.as_f64().ok_or("speed 需为数字")? as f32,
            "widthScale" | "width_scale" => {
                n.width_scale = v.as_f64().ok_or("widthScale 需为数字")? as f32
            }
            "yOffset" | "y_offset" => n.y_offset = v.as_f64().ok_or("yOffset 需为数字")? as f32,
            "judgeAreaScale" | "judge_area_scale" => {
                n.judge_area_scale = v.as_f64().ok_or("judgeAreaScale 需为数字")? as f32
            }
            other => return Err(format!("不支持的音符字段 {other}")),
        }
    }
    if n.kind == NoteKind::Hold && n.end.is_none() {
        return Err("hold 必须提供 endBeat".into());
    }
    if n.kind != NoteKind::Hold && n.end.is_some() {
        return Err("非 hold 音符不得携带 endBeat".into());
    }
    Ok(())
}

/// 共享句柄：GUI 线程与控制通道线程各自持一份 Arc
pub type SharedCore = Arc<Mutex<EditCore>>;

pub fn shared(core: EditCore) -> SharedCore {
    Arc::new(Mutex::new(core))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broadcast::TopicFilter;

    /// 执行一条命令并**断言它成功**。
    ///
    /// 为什么要有这个助手：测试里直接 `c.exec(...)` 会把"参数名写错"变成静默的 no-op ——
    /// 于是测试在断言别的东西时"通过"，实际什么都没发生（写这几条重叠测试时就先踩了一次：
    /// `resize_event` 要的是 `edge`+`toBeat`，我写了 `startBeat`，命令报错被丢掉，
    /// 断言才在下一行炸出来，白查一轮）。
    fn exec_ok(c: &mut EditCore, cmd: serde_json::Value) -> serde_json::Value {
        let r = c.exec(&cmd);
        assert_eq!(r["ok"], serde_json::json!(true), "命令失败：{cmd} → {r}");
        r
    }

    /// `split_event` 在切点上取的值 = **预览在那一刻显示的值**（带缓动、且流速按线性）。
    ///
    /// 这条钉住的是一个"两份实现分家"的坑：切割点上的值曾经由这里**另写一遍线性插值**算出来，
    /// 与 `perf::event_value` 无关 —— 于是给一条 `inOutCubic` 的移动事件切一刀，
    /// 切点上的数与预览不是同一个数，切完当场多出一个跳变。
    #[test]
    fn splitting_an_event_uses_the_evaluator_not_a_second_interpolation() {
        let mut c = EditCore::new();
        exec_ok(
            &mut c,
            serde_json::json!({"op": "add_event", "line": 0, "layer": 0, "track": "moveX",
                               "startBeat": [0, 1], "endBeat": [4, 1],
                               "startValue": 0.0, "endValue": 100.0, "easing": "inOutCubic"}),
        );
        let ev = c.doc().judge_lines[0].layers[0].move_x[0].clone();
        let at = crate::doc::Beat::new(1, 1); // 1/4 处：缓动在这里明显不等于线性
        // 预览在 1 拍处的值（**唯一的求值口径**）
        let tmap = crate::perf::TimeMap::from_doc(c.doc());
        let want = crate::perf::event_value(&ev, at.to_f64(), &tmap);
        let linear = 25.0; // 线性插值会给的数（= 100 × 1/4）
        assert!((want - linear).abs() > 1.0, "样例本身的缓动要看得出来：{want} vs {linear}");

        exec_ok(
            &mut c,
            serde_json::json!({"op": "split_event", "line": 0, "layer": 0, "track": "moveX",
                               "index": 0, "atBeat": [1, 1]}),
        );
        let list = &c.doc().judge_lines[0].layers[0].move_x;
        assert_eq!(list.len(), 2, "切一刀要变成两条");
        let cut = list[0].end_value.as_f64().unwrap();
        assert!((cut - want).abs() < 1e-9, "切点上的值应当是 {want}，实际 {cut}");
        assert!(
            (list[1].start_value.as_f64().unwrap() - want).abs() < 1e-9,
            "后半段的起点要接着它（否则表演在切点跳变）"
        );
    }

    /// 流速事件被切开时，切点上的值 = **求值器给的值**（缓动照旧生效 —— 流速不再"只看线性"）
    #[test]
    fn splitting_a_speed_event_uses_the_evaluated_value() {
        let mut c = EditCore::new();
        exec_ok(
            &mut c,
            serde_json::json!({"op": "add_event", "line": 0, "layer": 0, "track": "speed",
                               "startBeat": [0, 1], "endBeat": [4, 1],
                               "startValue": 0.0, "endValue": 100.0, "easing": "inOutCubic"}),
        );
        let ev = c.doc().judge_lines[0].layers[0].speed[0].clone();
        let at = crate::doc::Beat::new(1, 1);
        let tmap = crate::perf::TimeMap::from_doc(c.doc());
        let want = crate::perf::event_value(&ev, at.to_f64(), &tmap);
        // 这块按折线求值 ⇒ 明显不等于线性插值的 25，正是"流速也认缓动"的证据
        assert!((want - 25.0).abs() > 1e-6, "样例的缓动要看得出来：{want} vs 25");
        exec_ok(
            &mut c,
            serde_json::json!({"op": "split_event", "line": 0, "layer": 0, "track": "speed",
                               "index": 0, "atBeat": [1, 1]}),
        );
        let cut = c.doc().judge_lines[0].layers[0].speed[0].end_value.as_f64().unwrap();
        assert!((cut - want).abs() < 1e-9, "切点应当是求值器的值 {want}，实际 {cut}");
    }

    /// `normalize` 命令与**导入侧同一条规则**：重叠时**后一条的起点不动**，裁的是前一条；
    /// 斜坡不被拉长（延拓是"另加一条常量段"）。
    ///
    /// 这条是人报的 bug 的第三种表现：早先命令的实现把**后一条挪到前一条的终点** ——
    /// 于是"我把新事件放在第 4 拍"会被 `normalize` 悄悄挪走，而运行时预览又按另一个规则算。
    #[test]
    fn normalize_command_keeps_the_later_events_start() {
        let mut c = EditCore::new();
        // 一条长事件 [0,64] 常量 10 + 放进去的第二条 [4,8] 常量 30（重叠）
        exec_ok(&mut c, serde_json::json!({"op": "set_track_constant", "line": 0, "track": "speed", "value": 10.0}));
        exec_ok(
            &mut c,
            serde_json::json!({"op": "add_event", "line": 0, "layer": 0, "track": "speed",
                               "startBeat": [4, 1], "endBeat": [8, 1],
                               "startValue": 30.0, "endValue": 30.0, "easing": "linear"}),
        );
        exec_ok(&mut c, serde_json::json!({"op": "normalize"}));
        let line = c.doc().judge_lines[0].clone();
        let sp = &line.layers[0].speed;
        // 规范化之后：无空隙无重叠，且**后一条仍在第 4 拍起**
        assert!(
            sp.windows(2).all(|w| w[0].end == w[1].start),
            "规范化之后必须无空隙无重叠：{:?}",
            sp.iter().map(|e| (e.start.to_f64(), e.end.to_f64())).collect::<Vec<_>>()
        );
        let second = sp.iter().find(|e| e.start_value == serde_json::json!(30.0)).expect("第二条还在");
        assert_eq!(second.start.to_f64(), 4.0, "后一条事件的起点**不许被挪动**");
        // 第一条被裁到第 4 拍（而不是把第二条推走）
        let first = &sp[0];
        assert_eq!(first.start.to_f64(), 0.0);
        assert_eq!(first.end.to_f64(), 4.0, "裁的是前一条");
        // 末事件延拓到谱尾之后（常量事件直接拉长是**无损**的；斜坡才会另加一段）
        assert!(sp.last().unwrap().end.to_f64() > 64.0, "末事件要覆盖到谱面结束之后");
        // 语义（值函数）不变：4 拍之前 10、之后 30
        let tmap = crate::perf::TimeMap::from_doc(c.doc());
        assert_eq!(crate::perf::eval_events(sp, 2.0, &tmap), Some(10.0));
        assert_eq!(crate::perf::eval_events(sp, 5.0, &tmap), Some(30.0));
        assert_eq!(crate::perf::eval_events(sp, 30.0, &tmap), Some(30.0), "末尾之后保持末事件的终值");
    }

    /// **空话题的广播 = "什么都可能变了"**，必须送到每个订阅者手里。
    ///
    /// 反例（修复前）：`Subscribers::emit` 是"话题命中才投递"，空话题谁也命不中 ——
    /// 那条广播发给 0 个订阅者，revision 涨了、界面却收不到通知（静默丢更新）。
    /// 一条"记录不出任何改动"的命令就会走到这里。
    #[test]
    fn broadcast_with_no_topics_reaches_every_subscriber() {
        let mut c = EditCore::new();
        let sub = c.subscribe(TopicFilter::all());
        c.emit(Origin::Local, "空话题".into(), Vec::new(), Vec::new());
        let got = sub.rx.try_recv().expect("空话题的广播也要投递");
        assert!(
            !got.topics.is_empty(),
            "投出去的广播必须带着全量话题：{:?}",
            got.topics
        );
    }

    /// 事件重叠由**核心**维护：改一笔就更新，撤销也跟得上（GUI 与 CLI 读的是同一份）
    #[test]
    fn overlaps_are_owned_and_updated_by_the_core() {
        let mut c = EditCore::new();
        assert!(c.overlaps().is_empty());
        // 两条**互相重叠**的 moveX 事件：[0,4) 与 [2,6) ⇒ 重叠 [2,4)
        exec_ok(&mut c, json!({"op":"add_event","line":0,"layer":0,"track":"moveX",
                       "startBeat":[0,1],"endBeat":[4,1],"startValue":0.0,"endValue":100.0}));
        assert!(c.overlaps().is_empty(), "只有一条事件时谈不上重叠");
        exec_ok(&mut c, json!({"op":"add_event","line":0,"layer":0,"track":"moveX",
                       "startBeat":[2,1],"endBeat":[6,1],"startValue":0.0,"endValue":100.0}));
        let ov = c.overlaps().to_vec();
        assert_eq!(ov.len(), 1, "两条重叠的事件要被抓到：{ov:?}");
        assert_eq!((ov[0].line, ov[0].layer), (0, 0));
        assert_eq!(ov[0].track, "moveX");
        assert_eq!(ov[0].pointer(), "/judgeLines[0].layers[0].moveX[1]");
        assert!(ov[0].label().contains("重叠"));

        // 把它挪开 ⇒ 冲突消失
        exec_ok(&mut c, json!({"op":"resize_event","line":0,"layer":0,"track":"moveX","index":1,
                       "edge":"start","toBeat":[4,1]}));
        assert!(c.overlaps().is_empty(), "挪开之后不该再有重叠");

        // 撤销 ⇒ 冲突回来（撤销走的路径和改动不同，这里钉住它也会刷新）
        exec_ok(&mut c, json!({"op":"undo"}));
        assert_eq!(c.overlaps().len(), 1, "撤销要把重叠也带回来");
    }

    /// **只重查动过的那条线**：改 0 号线的重叠不该动到 1 号线的检测结果
    #[test]
    fn overlaps_refresh_is_scoped_to_the_touched_line() {
        let mut c = EditCore::new();
        exec_ok(&mut c, json!({"op":"add_line","name":"L1"}));
        // 两条线各造一处重叠
        for line in [0usize, 1] {
            exec_ok(&mut c, json!({"op":"add_event","line":line,"layer":0,"track":"alpha",
                           "startBeat":[0,1],"endBeat":[4,1],"startValue":1.0,"endValue":1.0}));
            exec_ok(&mut c, json!({"op":"add_event","line":line,"layer":0,"track":"alpha",
                           "startBeat":[2,1],"endBeat":[6,1],"startValue":0.0,"endValue":0.0}));
        }
        assert_eq!(c.overlaps().len(), 2, "{:?}", c.overlaps());
        let line1_before: Vec<_> = c.overlaps().iter().filter(|o| o.line == 1).cloned().collect();
        assert_eq!(line1_before.len(), 1);

        // 只动 0 号线：把它的第二条事件挪开
        exec_ok(&mut c, json!({"op":"resize_event","line":0,"layer":0,"track":"alpha","index":1,
                       "edge":"start","toBeat":[4,1]}));
        let after: Vec<_> = c.overlaps().to_vec();
        assert_eq!(after.len(), 1, "0 号线的冲突解决了，1 号线的还在：{after:?}");
        assert_eq!(after[0].line, 1);
        assert_eq!(
            after.iter().filter(|o| o.line == 1).cloned().collect::<Vec<_>>(),
            line1_before,
            "没动过的那条线，结果必须逐字不变"
        );
    }

    /// CLI 那条路：`{"op":"overlaps"}` 与 `overlaps()` 是同一份（GUI 与 CLI 不分家）
    #[test]
    fn overlaps_query_command_matches_the_cache() {
        let mut c = EditCore::new();
        exec_ok(&mut c, json!({"op":"add_event","line":0,"layer":0,"track":"rotate",
                       "startBeat":[0,1],"endBeat":[8,1],"startValue":0.0,"endValue":90.0}));
        exec_ok(&mut c, json!({"op":"add_event","line":0,"layer":0,"track":"rotate",
                       "startBeat":[4,1],"endBeat":[12,1],"startValue":0.0,"endValue":180.0}));
        let resp = exec_ok(&mut c, json!({"op":"overlaps"}));
        assert_eq!(resp["ok"], json!(true));
        assert_eq!(resp["result"]["count"], json!(c.overlaps().len()));
        assert_eq!(resp["result"]["count"], json!(1));
        let item = &resp["result"]["items"][0];
        assert_eq!(item["track"], json!("rotate"));
        assert_eq!(item["pointer"], json!(c.overlaps()[0].pointer()));
        assert_eq!(item["startBeat"], json!([4, 1]), "重叠从第二条事件的起点开始");
        // 查询**不改文档**：revision 不动（否则界面会收到一条没有内容的广播）
        let rev_before = c.revision();
        exec_ok(&mut c, json!({"op":"overlaps"}));
        assert_eq!(c.revision(), rev_before, "查询不该改文档");
    }

    /// **失败的命令既不推进 revision、也不广播** —— GUI 的"等广播"旗子就是靠这条判断该不该立。
    ///
    /// 为什么值得钉住：`App::dispatch` 用"revision 有没有推进"来决定要不要进入"等广播"态，
    /// 而那面旗算**工作态**（`busy_reason_of`）。早先它无条件立旗 ⇒ 只要有一条失败的面板命令
    /// （检查器提交非法值、拖动被拒、删除越界……），旗子就永远摘不掉 ⇒ 界面永远满帧重绘、
    /// **再也进不了 IDLE**（用户 2026-10-01 报的"手动测试里无论如何都不会进入 IDLE"）。
    #[test]
    fn a_failed_command_neither_bumps_revision_nor_broadcasts() {
        let mut c = EditCore::new();
        let sub = c.subscribe(TopicFilter::all());
        let rev_before = c.revision();
        // 越界删除：一定失败
        let resp = c.exec(&json!({"op":"del_note","line":0,"index":9999999}));
        assert_eq!(resp["ok"], json!(false), "越界删除应当失败：{resp}");
        assert_eq!(c.revision(), rev_before, "失败的命令不该推进 revision");
        assert!(sub.rx.try_recv().is_err(), "失败的命令不该广播");
        // 对照：成功的命令**必须**推进 revision 并广播（否则"等广播"就永远等不到）
        let resp = c.exec(&json!({"op":"add_note","line":0,"kind":"tap","startBeat":[4,1],"laneX":0.0}));
        assert_eq!(resp["ok"], json!(true), "{resp}");
        assert!(c.revision() > rev_before, "成功的命令要推进 revision");
        let b = sub.rx.try_recv().expect("成功的命令要广播");
        assert!(b.revision > rev_before, "{b:?}");
    }

    /// 反过来：**普通广播仍然要过过滤器**（全量话题只用在"不知道变了什么"的时候）
    #[test]
    fn ordinary_broadcasts_still_respect_topic_filters() {
        let mut c = EditCore::new();
        let meta_only = c.subscribe(TopicFilter::of(&[TopicKind::Meta]));
        c.emit(
            Origin::Local,
            "只改了 BPM".into(),
            vec![Topic::new(TopicKind::Bpm, None)],
            Vec::new(),
        );
        assert!(
            meta_only.rx.try_recv().is_err(),
            "只订阅 Meta 的订阅者不该被 BPM 改动叫醒"
        );
    }

    // ---------------------------------------------------------------- 「就位目标」set_target

    /// 四轨各放一条事件（用会过冲/非线性的缓动，端点误差才显形）
    fn four_tracks_ending_at(end_beat: i64) -> EditCore {
        let mut c = EditCore::new();
        for (track, easing) in [
            ("moveX", "inOutCubic"),
            ("moveY", "outBack"),
            ("rotate", "outElastic"),
            ("alpha", "inBack"),
        ] {
            exec_ok(
                &mut c,
                serde_json::json!({"op": "add_event", "line": 0, "layer": 0, "track": track,
                                   "startBeat": [0, 1], "endBeat": [end_beat, 1],
                                   "startValue": 0.0, "endValue": 100.0, "easing": easing}),
            );
        }
        c
    }

    fn value_at(c: &EditCore, track: &str, beat: i64) -> f64 {
        let list = match track {
            "moveX" => &c.doc().judge_lines[0].layers[0].move_x,
            "moveY" => &c.doc().judge_lines[0].layers[0].move_y,
            "rotate" => &c.doc().judge_lines[0].layers[0].rotate,
            "alpha" => &c.doc().judge_lines[0].layers[0].alpha,
            "speed" => &c.doc().judge_lines[0].layers[0].speed,
            other => panic!("未知轨道 {other}"),
        };
        opm_app_perf_track_value(c, list, beat as f64)
    }

    /// 求值要按**秒**长把缓动采样成折线 ⇒ 测试里的求值也得带上文档的拍↔秒映射。
    fn opm_app_perf_track_value(c: &EditCore, list: &[Event], beat: f64) -> f64 {
        let tmap = crate::perf::TimeMap::from_doc(c.doc());
        crate::perf::track_value(list, beat, &tmap).unwrap_or(0.0)
    }

    /// **块末就位：按位相等**（这条是用户那句"以保证最终 0 误差就位"的可执行定义）。
    ///
    /// 四个目标值都挑成"浮点插值会掉最后一位"的（0.3 / -1234.567 / 45.5 / 0.7），
    /// 终点又都在边界上（本来就有块末），所以只该改端值、不该多出事件。
    #[test]
    fn a_target_at_the_block_end_lands_bit_exactly() {
        let mut c = four_tracks_ending_at(4);
        let before = c.doc().judge_lines[0].clone();
        let r = exec_ok(
            &mut c,
            serde_json::json!({"op": "set_target", "line": 0, "atBeat": [4, 1],
                               "target": {"x": 0.3, "y": -1234.567, "angle": 45.5, "alpha": 0.7}}),
        );
        assert_eq!(r["result"]["wrote"], serde_json::json!(4), "四条轨道各写了一次：{r}");
        assert_eq!(r["result"]["cmds"], serde_json::json!(4), "边界上不需要切分：{r}");
        let wants: [(&str, f64); 4] = [("moveX", 0.3), ("moveY", -1234.567), ("rotate", 45.5), ("alpha", 0.7)];
        for (track, want) in wants {
            let got = value_at(&c, track, 4);
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "{track} 在块末应当是**按位**相等的 {want}，实际 {got}"
            );
        }
        // 都在边界上 ⇒ 一条事件都没多（切分只该发生在"块内"那种情形）
        let after = c.doc().judge_lines[0].clone();
        assert_eq!(after.layers[0].move_x.len(), before.layers[0].move_x.len());
        assert_eq!(after.layers[0].alpha.len(), before.layers[0].alpha.len());
        // 块末之后（空位）保持的也是这个值，且同样按位
        assert_eq!(value_at(&c, "moveX", 9).to_bits(), 0.3f64.to_bits());
        // 写块末的**只该是终值**：起点值、缓动家族都不动 —— 块内的插值当然会跟着新的终值变
        //（斜坡的中间点由两端决定，改了终点就改了斜坡；这正是"给块末一个值"的含义）
        for (track, ev_before, ev_after) in [
            ("moveX", &before.layers[0].move_x[0], &after.layers[0].move_x[0]),
            ("alpha", &before.layers[0].alpha[0], &after.layers[0].alpha[0]),
        ] {
            assert_eq!(ev_after.start_value.to_string(), ev_before.start_value.to_string(), "{track} 起值不该动");
            assert_eq!(ev_after.easing, ev_before.easing, "{track} 缓动不该动");
            assert_eq!(ev_after.start, ev_before.start, "{track} 起点拍不该动");
            assert_eq!(ev_after.end, ev_before.end, "{track} 终点拍不该动");
            assert_eq!(
                value_at(&c, track, 0).to_bits(),
                ev_before.start_value.as_f64().unwrap().to_bits(),
                "{track} 块首仍应当按位等于它的起值"
            );
        }
    }

    /// 目标落在**块内**：先就地切一刀（切点值 = 求值器的值），再写两侧端值 —— 结果同样按位
    #[test]
    fn a_target_inside_a_block_splits_then_lands_bit_exactly() {
        let mut c = EditCore::new();
        exec_ok(
            &mut c,
            serde_json::json!({"op": "add_event", "line": 0, "layer": 0, "track": "moveX",
                               "startBeat": [0, 1], "endBeat": [8, 1],
                               "startValue": 0.0, "endValue": 100.0, "easing": "outBounce"}),
        );
        exec_ok(
            &mut c,
            serde_json::json!({"op": "set_target", "line": 0, "atBeat": [4, 1], "target": {"x": 250.0}}),
        );
        let mx = c.doc().judge_lines[0].layers[0].move_x.clone();
        assert_eq!(mx.len(), 2, "块内就位要先切一刀");
        assert_eq!(mx[0].end, Beat::new(4, 1));
        assert_eq!(mx[1].start, Beat::new(4, 1));
        assert_eq!(mx[0].end_value.as_f64().unwrap().to_bits(), 250.0f64.to_bits());
        assert_eq!(mx[1].start_value.as_f64().unwrap().to_bits(), 250.0f64.to_bits());
        assert_eq!(value_at(&c, "moveX", 4).to_bits(), 250.0f64.to_bits());
    }

    /// 目标落在**空位**：空位的值来自前一块 ⇒ 写前一块的终值（不是"什么都不做"）
    #[test]
    fn a_target_in_a_gap_writes_the_previous_blocks_end_value() {
        let mut c = EditCore::new();
        exec_ok(
            &mut c,
            serde_json::json!({"op": "add_event", "line": 0, "layer": 0, "track": "moveX",
                               "startBeat": [0, 1], "endBeat": [4, 1],
                               "startValue": 0.0, "endValue": 100.0, "easing": "linear"}),
        );
        exec_ok(
            &mut c,
            serde_json::json!({"op": "set_target", "line": 0, "atBeat": [6, 1], "target": {"x": -50.0}}),
        );
        let mx = &c.doc().judge_lines[0].layers[0].move_x;
        assert_eq!(mx.len(), 1, "空位不该凭空多一块");
        assert_eq!(mx[0].end_value.as_f64().unwrap().to_bits(), (-50.0f64).to_bits());
        assert_eq!(value_at(&c, "moveX", 6).to_bits(), (-50.0f64).to_bits());
    }

    /// 目标落在**首块之前**：那条轨道的值取首块起始值 ⇒ 写首块的起值
    #[test]
    fn a_target_before_the_first_block_writes_the_first_blocks_start_value() {
        let mut c = EditCore::new();
        exec_ok(
            &mut c,
            serde_json::json!({"op": "add_event", "line": 0, "layer": 0, "track": "moveX",
                               "startBeat": [4, 1], "endBeat": [8, 1],
                               "startValue": 0.0, "endValue": 100.0, "easing": "linear"}),
        );
        exec_ok(
            &mut c,
            serde_json::json!({"op": "set_target", "line": 0, "atBeat": [1, 1], "target": {"x": 33.0}}),
        );
        let mx = &c.doc().judge_lines[0].layers[0].move_x;
        assert_eq!(mx.len(), 1);
        assert_eq!(mx[0].start_value.as_f64().unwrap().to_bits(), 33.0f64.to_bits());
        assert_eq!(value_at(&c, "moveX", 1).to_bits(), 33.0f64.to_bits());
    }

    /// **重叠时改的必须是"此刻生效的那一块"**（用户口径的 0 误差就位在重叠下也得成立）。
    ///
    /// 实测过的坑：一条 [0,8] 的斜坡 + 一条**后加的** [0,500] 常量 —— 就位命令写的是前者，
    /// 而求值器生效的是后者（起点不晚于该拍的最后一条）⇒ 命令报成功、那一刻的值纹丝不动。
    #[test]
    fn a_target_writes_the_event_that_is_actually_in_effect() {
        let mut c = EditCore::new();
        for (end, v) in [(8, 100.0), (500, 7.0)] {
            exec_ok(
                &mut c,
                serde_json::json!({"op": "add_event", "line": 0, "layer": 0, "track": "moveX",
                                   "startBeat": [0, 1], "endBeat": [end, 1],
                                   "startValue": 0.0, "endValue": v, "easing": "linear"}),
            );
        }
        // 此刻生效的是后加的那条常量（7.0），它的**起点**就在这一刻之前 ⇒ 走"块内切分"那条路
        let r = exec_ok(
            &mut c,
            serde_json::json!({"op": "set_target", "line": 0, "atBeat": [8, 1], "target": {"x": 250.3}}),
        );
        assert_eq!(
            value_at(&c, "moveX", 8).to_bits(),
            250.3f64.to_bits(),
            "就位之后，那一刻**求值到**的必须是目标（不是被写的那条事件自己的端值）：{r}"
        );
        // 斜坡那条一个字都不该被动（它此刻不生效）
        let mx = &c.doc().judge_lines[0].layers[0].move_x;
        assert!(
            mx.iter().any(|e| e.end == Beat::new(8, 1) && e.end_value.as_f64() == Some(100.0)),
            "不被求值的那条事件不该被改：{mx:?}"
        );
    }

    /// **已经在目标上的轨道一律不动** —— 否则"只改 X"会顺手把别人的块切开
    #[test]
    fn tracks_already_at_the_target_are_left_alone() {
        let mut c = EditCore::new();
        for track in ["moveX", "moveY"] {
            exec_ok(
                &mut c,
                serde_json::json!({"op": "add_event", "line": 0, "layer": 0, "track": track,
                                   "startBeat": [0, 1], "endBeat": [8, 1],
                                   "startValue": 0.0, "endValue": 100.0, "easing": "linear"}),
            );
        }
        // 第 4 拍 moveX 恰好是 50（线性中点）⇒ 把它当目标就等于"不改它"
        let r = exec_ok(
            &mut c,
            serde_json::json!({"op": "set_target", "line": 0, "atBeat": [4, 1],
                               "target": {"x": 50.0, "y": 20.0}}),
        );
        assert_eq!(r["result"]["wrote"], serde_json::json!(1), "只有 moveY 真的要写：{r}");
        assert_eq!(r["result"]["cmds"], serde_json::json!(3), "moveY 要切一刀 + 写两侧：{r}");
        let line = c.doc().judge_lines[0].clone();
        assert_eq!(line.layers[0].move_x.len(), 1, "moveX 已在目标上 ⇒ 不该被切开");
        assert_eq!(line.layers[0].move_y.len(), 2, "moveY 要切一刀才能在第 4 拍就位");
        assert_eq!(value_at(&c, "moveX", 4).to_bits(), 50.0f64.to_bits());
        assert_eq!(value_at(&c, "moveY", 4).to_bits(), 20.0f64.to_bits());
        // 报告里写清了每条轨道做了什么
        let plan = r["result"]["plan"].to_string();
        assert!(plan.contains("skip"), "{plan}");
        assert!(plan.contains("块内"), "{plan}");
    }

    /// 空轨道：**报告出来**，不静默吞掉；其余轨道照常就位
    #[test]
    fn an_empty_track_is_reported_not_silently_swallowed() {
        let mut c = four_tracks_ending_at(4);
        // 把 moveY 清空
        exec_ok(&mut c, serde_json::json!({"op": "del_event", "line": 0, "track": "moveY", "index": 0}));
        let r = exec_ok(
            &mut c,
            serde_json::json!({"op": "set_target", "line": 0, "atBeat": [4, 1],
                               "target": {"x": 1.0, "y": 2.0, "alpha": 0.25}}),
        );
        assert_eq!(r["result"]["wrote"], serde_json::json!(2), "moveX 与 alpha 就位：{r}");
        let plan = r["result"]["plan"].to_string();
        assert!(plan.contains("还没有事件"), "{plan}");
        assert_eq!(value_at(&c, "moveX", 4).to_bits(), 1.0f64.to_bits());
        assert_eq!(value_at(&c, "alpha", 4).to_bits(), 0.25f64.to_bits());
    }

    /// **一次撤销**：四轨一起写（含切分）只算一个撤销步
    #[test]
    fn a_target_is_one_undo_step() {
        let mut c = four_tracks_ending_at(8);
        let before = c.doc().clone();
        // 撤销后的基准**从操作前的文档算**，不手写数：四条轨道的缓动各不相同
        //（inOutCubic / outBack / outElastic / inBack —— `inBack` 在中点是**负**的，
        // 那正是过冲/欠冲的本意，任何"顺手夹一下"的写法都会把它毁掉）
        let want: Vec<f64> = ["moveX", "moveY", "rotate", "alpha"].iter().map(|t| value_at(&c, t, 4)).collect();
        exec_ok(
            &mut c,
            serde_json::json!({"op": "set_target", "line": 0, "atBeat": [4, 1],
                               "target": {"x": 0.3, "y": -1.5, "angle": 12.5, "alpha": 0.5}}),
        );
        assert_eq!(c.doc().judge_lines[0].layers[0].move_x.len(), 2, "切过一刀");
        exec_ok(&mut c, serde_json::json!({"op": "undo"}));
        let back = c.doc().judge_lines[0].clone();
        assert_eq!(back.layers[0].move_x.len(), before.judge_lines[0].layers[0].move_x.len());
        for (t, w) in ["moveX", "moveY", "rotate", "alpha"].iter().zip(want) {
            assert_eq!(value_at(&c, t, 4).to_bits(), w.to_bits(), "{t} 应当撤回到操作前的值");
        }
    }

    /// 事务里调用 `set_target` **不会替外层提前 commit**（`journal.begin` 不嵌套）
    #[test]
    fn a_target_inside_an_outer_transaction_does_not_commit_early() {
        let mut c = four_tracks_ending_at(4);
        exec_ok(&mut c, serde_json::json!({"op": "begin", "label": "外层"}));
        exec_ok(
            &mut c,
            serde_json::json!({"op": "set_target", "line": 0, "atBeat": [4, 1], "target": {"x": 7.0}}),
        );
        exec_ok(&mut c, serde_json::json!({"op": "add_note", "line": 0, "kind": "tap", "startBeat": [1, 1]}));
        exec_ok(&mut c, serde_json::json!({"op": "commit"}));
        assert_eq!(value_at(&c, "moveX", 4).to_bits(), 7.0f64.to_bits());
        assert_eq!(c.doc().judge_lines[0].notes.len(), 1);
        // 一次撤销要把**两件事**一起撤掉，说明它们同属外层那一个事务
        exec_ok(&mut c, serde_json::json!({"op": "undo"}));
        assert_eq!(value_at(&c, "moveX", 4).to_bits(), 100.0f64.to_bits(), "块末回到原值");
        assert_eq!(c.doc().judge_lines[0].notes.len(), 0, "音符也一起撤掉");
    }

    /// 参数校验：target 不能空、不能非数字
    #[test]
    fn set_target_validates_its_arguments() {
        let mut c = four_tracks_ending_at(4);
        let r = c.exec(&serde_json::json!({"op": "set_target", "line": 0, "atBeat": [4, 1], "target": {}}));
        assert_eq!(r["ok"], serde_json::json!(false));
        assert!(r["error"].as_str().unwrap().contains("至少"), "{r}");
        let r = c.exec(&serde_json::json!({"op": "set_target", "line": 0, "atBeat": [4, 1],
                                           "target": {"x": "一百"}}));
        assert_eq!(r["ok"], serde_json::json!(false));
        assert!(r["error"].as_str().unwrap().contains("x"), "{r}");
        let r = c.exec(&serde_json::json!({"op": "set_target", "line": 0, "target": {"x": 1.0}}));
        assert_eq!(r["ok"], serde_json::json!(false), "缺 atBeat 应当报错");
    }
}
