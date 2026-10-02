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
use crate::doc::{Beat, Document, Event, JudgeLine, MaskZone, Note, NoteKind, CAP_MASK, MASK_TRACKS, TRACKS};
use crate::journal::{read_track, read_zone_track, Change, Journal, LineProps, ZoneProps};

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
#[derive(Debug)]
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
    /// 这份缓存目录的**独占锁**（`<缓存目录>/lock.pid`）。
    ///
    /// 它必须活到会话结束：`Drop` 即"我不再写这份缓存了"。放在 `Staged` 里而不是就地拿着，
    /// 是因为"摊缓存"与"装载进核心"是两步（`stage_file` 是静态函数，装载要 `&mut self`）——
    /// 锁的对象得跟着数据一起旅行，否则函数一返回锁就掉了（第二个进程立刻能抢到，等于没锁）。
    pub lock: Option<crate::session::ChartLock>,
}

/// [`EditCore::stage_into_cache`] 的产物：缓存目录 + 它的锁（锁要活到会话结束）
struct StagedCache {
    dir: PathBuf,
    lock: Option<crate::session::ChartLock>,
}

/// **能不能由我们接手这份缓存**（`open_file` 与 `stage_into_cache` 共用同一判据）。
///
/// 三条规则，都是从"会不会丢东西 / 会不会两个人写"出发的：
/// · `Free` ⇒ 可以；
/// · **崩溃遗留**（锁没人持、那个 pid ping 不通）⇒ **只有还留着未保存改动时**才拦下来问用户；
///   没有可丢的东西就直接重摊（拦下来问"要不要继续"而那里其实什么都没有，只会让人白点一下）；
/// · `Busy` ⇒ 别人正开着 ⇒ 拦；**自己开着**（同一个进程里的另一个 `EditCore`）⇒ 拦，
///   因为那也是"有人在写这份缓存"，覆盖它就是覆盖那个会话的工作副本。
///
/// 返回 `Ok(())` = 接手；`Err((dir, holder, crashed))` = 交给调用方去问用户/报错。
fn claim_blocker(
    dir: &Path,
    state: &crate::session::CacheState,
) -> Result<(), (PathBuf, codec::container::Session, bool)> {
    match state {
        crate::session::CacheState::Free => Ok(()),
        crate::session::CacheState::Busy { holder, .. } => {
            Err((dir.to_path_buf(), holder.clone(), false))
        }
        crate::session::CacheState::Crashed(h) => {
            let lost = codec::container::read_session(dir).is_some_and(|s| s.has_unsaved());
            if lost {
                Err((dir.to_path_buf(), h.clone(), true))
            } else {
                Ok(())
            }
        }
    }
}

/// **打开一份谱面的结果**（用户口径 2026-10-02：崩溃检查放在"打开谱面文件"时，不是启动时）。
///
/// 三种情形都要让调用方分得清：能开、有人在用、上次没退干净 ——
/// 第三种**没有动缓存一个字节**（那份快照就是用户没保存的编辑）。
#[derive(Debug)]
pub enum OpenOutcome {
    Ready(Box<Staged>),
    /// 另一份进程正开着它（pid 锁被持有，或那个 pid 还应答）
    Busy {
        dir: PathBuf,
        holder: codec::container::Session,
    },
    /// 有 pid 锁、但锁没人持且那个 pid ping 不通 ⇒ **崩溃遗留**
    Crashed {
        dir: PathBuf,
        holder: codec::container::Session,
    },
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
    /// **这份谱面缓存目录的独占锁**（见 [`crate::session::ChartLock`]）。
    ///
    /// 它活到"换谱面/退出"为止：中途掉了就等于放弃了"只有我在写这份缓存"这条承诺
    /// （第二个进程会以为自己可以写，两边互相覆盖快照）。`--doc` 之外的入口都经
    /// [`EditCore::open_file`] ⇒ [`EditCore::load_staged`] 装上它。
    chart_lock: Option<crate::session::ChartLock>,
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
            chart_lock: None,
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

    /// **打开一份谱面**：先看它的缓存目录归谁，再决定摊不摊（见 [`OpenOutcome`]）。
    ///
    /// 与 [`EditCore::stage_file`] 的差别只有一条：**不把"有人在用/上次没退干净"压成一句错误**，
    /// 而是原样交给调用方（GUI 要据此问用户"继续上次编辑还是丢弃重开"）。
    /// 不落缓存的输入（裸 JSON、用户的工程文件夹）一律 `Ready` —— 它们没有缓存目录可争。
    pub fn open_file(path: &Path) -> Result<OpenOutcome, String> {
        // 目录形态：只有"就在缓存根目录里"的那些才可能被别人占着（用户自己的工程文件夹不是共享物）
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("读取失败 {}: {e}", path.display()))?;
        if meta.is_dir() {
            if path.starts_with(codec::container::cache_root()) {
                if let Err((dir, holder, crashed)) =
                    claim_blocker(path, &crate::session::inspect(path))
                {
                    return Ok(if crashed {
                        OpenOutcome::Crashed { dir, holder }
                    } else {
                        OpenOutcome::Busy { dir, holder }
                    });
                }
            }
            return Ok(OpenOutcome::Ready(Box::new(Self::stage_folder(path)?)));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("读取失败: {e}"))?;
        if !crate::zip::looks_like_zip(&bytes) {
            return Ok(OpenOutcome::Ready(Box::new(Self::stage_bytes(path, bytes, CacheClaim::Session)?)));
        }
        // 容器/包：**先按内容算出缓存目录**，看一眼它归谁 —— 这一眼必须在摊之前，
        // 因为摊会把别人（或上次崩溃）留下的快照覆盖掉
        let dir = codec::container::extract_dir(&codec::container::cache_key(&bytes));
        match claim_blocker(&dir, &crate::session::inspect(&dir)) {
            Err((dir, holder, crashed)) => Ok(if crashed {
                OpenOutcome::Crashed { dir, holder }
            } else {
                OpenOutcome::Busy { dir, holder }
            }),
            Ok(()) => Ok(OpenOutcome::Ready(Box::new(Self::stage_bytes(
                path,
                bytes,
                CacheClaim::Session,
            )?))),
        }
    }

    /// 同 [`EditCore::stage_file`]，但显式说明"这次摊缓存算不算认领一个会话"（见 [`CacheClaim`]）。
    pub fn stage_file_as(path: &Path, claim: CacheClaim) -> Result<Staged, String> {
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("读取失败 {}: {e}", path.display()))?;
        if meta.is_dir() {
            return Self::stage_folder(path);
        }
        let bytes = std::fs::read(path).map_err(|e| format!("读取失败: {e}"))?;
        Self::stage_bytes(path, bytes, claim)
    }

    /// 从一个**已经读进内存**的输入摊出会话（文件与容器共用的那一段：
    /// `open_file` 与 `stage_file_as` 都走它，免得两条路各写一份"裸 JSON 怎么办"）。
    fn stage_bytes(path: &Path, bytes: Vec<u8>, claim: CacheClaim) -> Result<Staged, String> {
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
                lock: None, // 裸 JSON 不摊缓存 ⇒ 没有缓存目录可锁
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
        let staged = Self::stage_into_cache(&doc, &assets, &key, Some(path), format, claim, &mut fid)?;
        Ok(Staged {
            dir: Some(staged.dir),
            doc,
            assets,
            format,
            fid,
            target: Some(path.to_path_buf()),
            folder: None, // 包（zip）不是文件夹
            unsaved: false,
            lock: staged.lock,
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
            // 解压缓存里的会话元数据 / pid 锁 / 半截临时文件都不是资源（判据只有一处）
            if codec::container::is_internal_entry(&name) {
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
            // 打开的若是**缓存目录本身**（继续上次编辑那条路）：它已经归我们了（有锁就是我们的），
            // 这里不重新抢锁 —— 抢锁发生在 `stage_into_cache`
            lock: None,
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
    ) -> Result<StagedCache, String> {
        let dir = codec::container::extract_dir(key);
        // **先看这份缓存归谁，再决定动不动它**（用户口径 2026-10-02：锁落到每份谱面自己的目录上）。
        //
        // 判据在 `session::inspect`：`Busy` = 另一份进程正拿它当工作副本（锁被持有或它还应答）；
        // `Crashed` = 有 pid 锁但主人已经不在了（**崩溃遗留**，`open_file` 会在此之前拦下来问用户；
        // 走到这里说明调用方没先问过，那就按"不动缓存"处理 —— 覆盖它等于把人家没保存的编辑删掉）。
        let state = crate::session::inspect(&dir);
        if claim == CacheClaim::Session {
            if let Err((dir, holder, crashed)) = claim_blocker(&dir, &state) {
                return Err(if crashed {
                    format!(
                        "缓存目录 {} 里有上次没退干净的编辑（pid {} 已经不在了）—— \
                         先在界面上选「继续上次编辑 / 丢弃并重新打开」再打开",
                        dir.display(),
                        holder.pid
                    )
                } else if holder.pid == std::process::id() {
                    format!(
                        "缓存目录 {} 已经由本进程的另一个会话占着（pid {}）—— 先关掉那一份再打开",
                        dir.display(),
                        holder.pid
                    )
                } else {
                    format!(
                        "这份谱面已经在另一个 OpenPhM 里打开着（pid {}，{}）—— 先关掉那一个，或改开别份谱面",
                        holder.pid, holder.exe
                    )
                });
            }
        }
        // **别人已经认领的目录，一次性读取者一个字节都不碰**。
        //
        // 为什么要有这条：缓存目录按**容器内容**命名，而 `session.json` 与那份 `opm.json`
        // 快照是**会话**状态。`opm-ctl`（转换/批处理）读同一个包时若照常摊一遍，就会把 GUI
        // 崩溃留下的元数据与未保存快照一起覆盖掉 —— "上次没退干净"那条提示连同用户一小时
        // 的改动就这么没了（实测：一次 `opm-ctl --file X dump` 就能抹掉）。
        let existing = codec::container::read_session(&dir);
        let claimed_by_other = !state.is_free()
            || existing.as_ref().is_some_and(|s| s.pid != std::process::id());
        if claimed_by_other && claim == CacheClaim::ReadOnly {
            fid.note(format!(
                "缓存目录 {} 已被另一个会话占用（{}）：本次只读不动它（不覆盖它的快照与会话元数据）",
                dir.display(),
                state
                    .holder()
                    .map(|h| format!("pid {}", h.pid))
                    .unwrap_or_else(|| "有来源不明的元数据".to_owned())
            ));
            return Ok(StagedCache { dir, lock: None });
        }
        // **认领一份会话**：抢到锁才写（抢不到上面已经返回了）。只读的一次性用途**不抢锁** ——
        // 抢了就成"认领"了，而它承诺过不碰别人的会话状态。
        let lock = if claim == CacheClaim::Session {
            Some(crate::session::acquire(&dir, source.and_then(|p| p.to_str())).map_err(|e| {
                match e {
                    crate::session::Refused::Busy(h) => format!("这份谱面已经被 pid {} 占用", h.pid),
                    crate::session::Refused::Io(e) => e,
                }
            })?)
        } else {
            None
        };
        {
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
        Ok(StagedCache { dir, lock })
    }

    /// ② **正式加载编辑**：把 [`Staged`] 装进这个会话（**唯一**一个入口）。
    ///
    /// 整体替换是数据结构层面的"全量变更"：撤销栈清空、revision +1、按**全量话题**广播，
    /// 于是 GUI 只会走一次"整表重建"（`structure` 脏位），不需要各面板自己去猜。
    ///
    /// **换谱面时上一份解压目录立刻删掉** ⇒ 一个进程至多留一份（缓存是进程独占的）。
    pub fn load_staged(&mut self, staged: Staged) -> Result<codec::Fidelity, String> {
        let Staged { dir, doc, assets, fid, format, target, folder, unsaved, lock } = staged;
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
        // 换谱面就把**上一份的锁放掉**（它的目录紧接着会被删），拿上这一份的
        // （`lock: None` 只出现在"裸 JSON/用户文件夹"与"直接打开缓存目录继续编辑"两条路上：
        //   前者没有缓存目录，后者那份缓存本来就归我们）
        self.chart_lock = lock;
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
        // **只动了遮蔽区 ⇒ 判定线的重叠结果一个字都不用改**（遮蔽区不属于任何线）。
        // 少了这一条，拖一下顶点就会全量重查一遍所有线的事件重叠 —— 那是 O(全谱面)。
        if !changes.is_empty() && changes.iter().all(Change::is_mask_only) {
            return;
        }
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
        [Meta, Bpm, LineList, LineProps, Notes, Note, Track, MaskZoneList, MaskZone]
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
            // `ping` 顺带**自报身份**：判"某个谱面缓存的主人还在不在"要靠它核对
            // （见 `session::inspect`）——只连得上不够，还得确认那个进程认领的正是这份缓存。
            "ping" => {
                return Ok(json!({
                    "pong": true,
                    "revision": self.revision,
                    "pid": std::process::id(),
                    "exe": codec::container::exe_name(),
                    "chart": self.path.as_ref().map(|p| p.display().to_string()),
                    "cacheDir": self.asset_dir.as_ref().map(|p| p.display().to_string()),
                }))
            }
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
                    n.start = n.start.checked_add(delta).ok_or("拍数溢出")?;
                    if let Some(e) = n.end {
                        n.end = Some(e.checked_add(delta).ok_or("拍数溢出")?);
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
            // ================================================================ 遮蔽区（躁域）
            //
            // 七条通道：`x1..y3`（三角形顶点，RPE 屏幕坐标）+ `active`（外观开关）。
            // 与判定线事件的三点关键差别（**规范 §4.6**，别按判定线的直觉改这里）：
            // · 通道**允许空隙**、**允许首事件晚于拍 0** —— "什么时候出现"就是靠这个表达的；
            // · 没有图层：一个通道就是一份事件表，下标即文档下标；
            // · 事件块的值可以是非数值（`active` 写 `true`/`false`），求值器按 0/1 处理。
            "add_zone" => {
                let start = c.get("startBeat").map(parse_beat).transpose()?.unwrap_or_else(Beat::zero);
                // 终点：`MaskZone::default_span`（**起点 + 1 拍**，用户口径 2026-10-02）——
                // 不铺到谱面末尾：块的跨度就是这块区域存在的时段（见 `perf::mask_state_at`），
                // "整首都在"不该是默认。这条规则与界面的草稿三角共用一份（见 `default_span`）。
                let (_, default_end) = MaskZone::default_span(start);
                let end = c
                    .get("endBeat")
                    .map(parse_beat)
                    .transpose()?
                    .unwrap_or(default_end);
                if end <= start {
                    return Err(format!(
                        "endBeat({}) 必须大于 startBeat({})",
                        end.to_f64(),
                        start.to_f64()
                    ));
                }
                let mut zone = if c.get("empty").and_then(|v| v.as_bool()).unwrap_or(false) {
                    MaskZone::default()
                } else {
                    // 用户口径：新建即在中央摆一个**正三角形**（六条常量事件，各一条）
                    MaskZone::with_default_triangle(start, end)
                };
                if let Some(name) = c.get("name").and_then(|v| v.as_str()) {
                    zone.name = name.to_owned();
                }
                if let Some(set) = c.get("set").and_then(|v| v.as_object()) {
                    for (k, v) in set {
                        let list = zone
                            .track_mut(k)
                            .ok_or_else(|| format!("未知遮蔽区通道 {k}（可选 {MASK_TRACKS:?}）"))?;
                        if !v.is_number() && !v.is_boolean() {
                            return Err(format!("遮蔽区通道 {k} 的值需为数字或布尔"));
                        }
                        list.clear();
                        *list = vec![Event::new(start, end, v.clone(), v.clone(), "linear")];
                    }
                }
                let index = self.doc.mask_zones.len();
                self.doc.mask_zones.push(zone.clone());
                // 能力等级：出现遮蔽区 ⇒ 至少 4（`spec/opm-format.md` §7）
                self.doc.min_client_capability = self.doc.min_client_capability.max(CAP_MASK);
                self.journal.record(Change::InsertZone {
                    index,
                    zone: Box::new(zone),
                });
                Ok(json!({
                    "index": index,
                    "zones": self.doc.mask_zones.len(),
                    "startBeat": start,
                    "endBeat": end,
                    "capability": self.doc.min_client_capability,
                }))
            }
            "del_zone" => {
                let index = zone_index_arg(c)?;
                if index >= self.doc.mask_zones.len() {
                    return Err(format!("遮蔽区下标 {index} 越界"));
                }
                let removed = self.doc.mask_zones.remove(index);
                self.journal.record(Change::RemoveZone {
                    index,
                    zone: Box::new(removed),
                });
                // 能力等级按**内容**重算（最后一块遮蔽区没了就掉回来）—— 判据只有
                // `doc::capability_of` 一份，别在这里手写第二份
                self.doc.min_client_capability = crate::doc::capability_of(&self.doc)
                    .max(if self.doc.mask_zones.is_empty() { 1 } else { CAP_MASK });
                Ok(json!({"zones": self.doc.mask_zones.len()}))
            }
            "set_zone" => {
                let index = zone_arg(c)?;
                let before = ZoneProps::of(self.zone(index)?);
                let mut after = before.clone();
                // 整区切 `active` 要改**一条通道**（不是属性），于是记的是 `ZoneTrack` ——
                // 与 `set_zone` 的属性改动合成**一个撤销步**（见下面 `wrap`）。
                let mut active_change: Option<(Vec<Event>, Vec<Event>)> = None;
                if let Some(set) = c.get("set").and_then(|v| v.as_object()) {
                    for (k, v) in set {
                        match k.as_str() {
                            "name" => after.name = v.as_str().unwrap_or("遮蔽区").to_owned(),
                            // **整区切 active**（用户口径 2026-10-02："每个遮蔽区的 active 只能是
                            // 一种状态"）：已有块的时间跨度不动、值**全部改写**成这一档；
                            // 一块都没有（= false 那档）时，按**坐标事件的包络**写一块
                            // （"这块区存在多久，它就是这个状态"）。
                            "active" => {
                                let on = v
                                    .as_bool()
                                    .or_else(|| v.as_f64().map(|x| x >= 0.5))
                                    .ok_or("set_zone 的 active 需要布尔（true/false）")?;
                                let mut list = self.zone_track(index, "active")?;
                                if list.is_empty() {
                                    let (s, e) = self.zone(index)?.coord_span().ok_or(
                                        "这块区还没有任何坐标事件 —— active 现在没有意义\
                                         （先把三角形写出来）",
                                    )?;
                                    list = vec![Event::new(s, e, json!(on), json!(on), "linear")];
                                } else {
                                    for e in list.iter_mut() {
                                        e.start_value = json!(on);
                                        e.end_value = json!(on);
                                    }
                                }
                                active_change = Some((self.zone_track(index, "active")?, list));
                            }
                            other => return Err(format!("set_zone 不支持的字段 {other}")),
                        }
                    }
                }
                // 一个命令 = 一个撤销步：同时改了属性与通道时才需要显式开事务
                //（调用方自己开着事务时不动它 —— `commit` 会把**它**的事务提前结掉）
                let wrap = active_change.is_some()
                    && after != before
                    && !self.journal.in_transaction();
                if wrap {
                    self.journal.begin("设置遮蔽区");
                }
                after.apply_to(self.zone_mut(index)?);
                self.journal.record(Change::SetZone {
                    index,
                    before: Box::new(before),
                    after: Box::new(after),
                });
                if let Some((track_before, track_after)) = active_change {
                    *self.zone_track_mut(index, "active")? = track_after.clone();
                    self.journal.record(Change::ZoneTrack {
                        zone: index,
                        track: "active".to_owned(),
                        before: track_before,
                        after: track_after,
                    });
                }
                if wrap {
                    self.journal.commit();
                }
                Ok(json!({"zone": index}))
            }
            "add_zone_event" => {
                let zone_idx = zone_arg(c)?;
                let track = zone_track_arg(c)?;
                let start = beat_arg(c, "startBeat")?;
                let before = self.zone_track(zone_idx, &track)?;
                // ---- 跨度规则：**只有 `edit::mask_*` 那一份判据** ----
                //
                // 界面的手势草稿与属性编辑器那颗按钮调的是同一条（用户口径 2026-10-02：
                // "创建流程应与普通编辑模式下的事件块放置一样"）。放在核心是因为
                // CLI/agent 与界面必须拿到同一个答案：遮蔽区通道的不变量是"不许重叠"
                //（规范 §4.6），而判定线那边没有的不变量这里是**拒绝**而不是留给冲突浏览器。
                if !crate::edit::mask_can_start(&before, start) {
                    return Err(format!(
                        "起点 {} 拍上已经有一块了（通道内不许重叠）—— 换个起点，或先删掉那一块",
                        start.to_f64()
                    ));
                }
                let end = match c.get("endBeat") {
                    // 显式给的终点：越界要**报错**，不悄悄夹 —— 那是"缺省值"才做的事
                    Some(v) => {
                        let end = parse_beat(v)?;
                        if let Some(limit) = crate::edit::mask_end_limit(&before, start) {
                            if end > limit {
                                return Err(format!(
                                    "终点 {} 拍会越过下一块（它起于 {} 拍）—— 通道内不许重叠",
                                    end.to_f64(),
                                    limit.to_f64()
                                ));
                            }
                        }
                        end
                    }
                    // 缺省终点 = 起点 + `MASK_EVENT_BEATS` 拍，缩到空档为止（缩不出来就报错）
                    None => crate::edit::mask_default_end(&before, start)
                        .ok_or("这里放不下新的事件块（起点上已有块，或与下一块之间没有空档）")?,
                };
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
                // 缺省值 = **该通道此刻的值**（不让区域/外观在放下的那一刻跳变）。
                // 这一条放在核心而不是 GUI：CLI/agent 与界面必须拿到同一个缺省值。
                let neutral = || self.zone_neutral_value(zone_idx, &track, start);
                let from = c.get("startValue").cloned().unwrap_or_else(neutral);
                let to = c.get("endValue").cloned().unwrap_or_else(|| from.clone());
                let ev = Event::new(start, end, from, to, &easing);
                check_active_block(&track, &ev)?;
                let mut after = before.clone();
                // **先裁后插**：把被这一块压在下面的那些裁到它的起点（保持切点上的值）。
                // 用户最常见的动作是"给一条铺满全谱的常量事件里插一个关键帧" ——
                // 不裁的话文件里就留下一处重叠，而遮蔽区通道的不变量是"无重叠"
                //（`validate` 会报错：自己的产物过不了自己的校验器）。
                {
                    let tmap = crate::perf::TimeMap::from_parts(&self.doc.bpm_list, self.doc.chart_end());
                    let n = codec::trim_before_insert(&mut after, start, &tmap);
                    if n > 0 {
                        after.retain(|e| e.end > e.start); // 裁成零长度的丢掉（opm 不允许）
                    }
                }
                after.push(ev.clone());
                after.sort_by(|a, b| a.start.cmp(&b.start));
                let index = after
                    .iter()
                    .position(|e| e.start == ev.start && e.end == ev.end)
                    .unwrap_or(after.len().saturating_sub(1));
                *self.zone_track_mut(zone_idx, &track)? = after.clone();
                self.journal.record(Change::ZoneTrack {
                    zone: zone_idx,
                    track: track.clone(),
                    before,
                    after,
                });
                Ok(json!({"zone": zone_idx, "track": track, "index": index}))
            }
            "set_zone_event" => {
                let zone_idx = zone_arg(c)?;
                let track = zone_track_arg(c)?;
                let index = usize_arg(c, "index")?;
                let before = self.zone_track(zone_idx, &track)?;
                let mut after = before.clone();
                let cur = after
                    .get(index)
                    .ok_or_else(|| format!("事件索引 {index} 越界（共 {}）", after.len()))?;
                let mut ev = cur.clone();
                if let Some(o) = c.get("set").and_then(|v| v.as_object()) {
                    for (k, v) in o {
                        match k.as_str() {
                            "startBeat" => ev.start = parse_beat(v)?,
                            "endBeat" => ev.end = parse_beat(v)?,
                            "startValue" => ev.start_value = v.clone(),
                            "endValue" => ev.end_value = v.clone(),
                            "easing" => {
                                let s = v.as_str().unwrap_or("linear");
                                if !is_easing(s) {
                                    return Err(format!("未知缓动 {s:?}"));
                                }
                                ev.easing = s.to_owned();
                            }
                            other => return Err(format!("set_zone_event 不支持的字段 {other}")),
                        }
                    }
                }
                if ev.end <= ev.start {
                    return Err("endBeat 必须大于 startBeat".into());
                }
                if ev.start < Beat::zero() {
                    return Err("拍不能为负".into());
                }
                check_active_block(&track, &ev)?;
                // **不许压到邻块**：拖端点/属性编辑器改头尾都走这条命令，而通道的不变量是
                // "不许重叠"（判定线那边由冲突浏览器兜着，遮蔽区没有那个东西）。
                // 判据与 `move_zone_event` 同一句（半个区间相交），只是这里只重排一个事件的跨度。
                if let Some(i) = crate::edit::mask_overlap(&ev, index, &after) {
                    let o = &after[i];
                    return Err(format!(
                        "改完会与第 {i} 个事件重叠（{}..{}）—— 通道内不允许重叠",
                        o.start.to_f64(),
                        o.end.to_f64()
                    ));
                }
                after[index] = ev;
                ensure_sorted(&after, &format!("遮蔽区 {zone_idx} 的 {track}"))?;
                *self.zone_track_mut(zone_idx, &track)? = after.clone();
                self.journal.record(Change::ZoneTrack {
                    zone: zone_idx,
                    track,
                    before,
                    after,
                });
                Ok(json!({"zone": zone_idx, "index": index}))
            }
            "del_zone_event" => {
                let zone_idx = zone_arg(c)?;
                let track = zone_track_arg(c)?;
                let index = usize_arg(c, "index")?;
                let before = self.zone_track(zone_idx, &track)?;
                if index >= before.len() {
                    return Err(format!("事件索引 {index} 越界（共 {}）", before.len()));
                }
                let mut after = before.clone();
                let removed = after.remove(index);
                let left = after.len();
                *self.zone_track_mut(zone_idx, &track)? = after.clone();
                self.journal.record(Change::ZoneTrack {
                    zone: zone_idx,
                    track,
                    before,
                    after,
                });
                Ok(json!({
                    "zone": zone_idx,
                    "events": left,
                    "removed": {"startBeat": removed.start, "endBeat": removed.end},
                }))
            }
            "resize_zone_event" => {
                let zone_idx = zone_arg(c)?;
                let track = zone_track_arg(c)?;
                let index = usize_arg(c, "index")?;
                let edge = c
                    .get("edge")
                    .and_then(|v| v.as_str())
                    .ok_or("resize_zone_event 需要 edge（start|end）")?
                    .to_owned();
                let to = beat_arg(c, "toBeat")?;
                if to < Beat::zero() {
                    return Err("拍不能为负".into());
                }
                let before = self.zone_track(zone_idx, &track)?;
                let mut after = before.clone();
                let cur = after
                    .get(index)
                    .cloned()
                    .ok_or_else(|| format!("事件索引 {index} 越界（共 {}）", after.len()))?;
                match edge.as_str() {
                    "start" => {
                        if to >= cur.end {
                            return Err(format!("start({}) 必须早于该事件的 end({})", to.to_f64(), cur.end.to_f64()));
                        }
                        after[index].start = to;
                    }
                    "end" => {
                        if to <= cur.start {
                            return Err(format!("end({}) 必须晚于该事件的 start({})", to.to_f64(), cur.start.to_f64()));
                        }
                        after[index].end = to;
                    }
                    other => return Err(format!("edge 只能是 start|end，收到 {other:?}")),
                }
                // 与 `set_zone_event` 同一道闸（见那里的注释）：只动一个端点也不许压到邻块
                if let Some(i) = crate::edit::mask_overlap(&after[index], index, &after) {
                    let o = &after[i];
                    return Err(format!(
                        "改完会与第 {i} 个事件重叠（{}..{}）—— 通道内不允许重叠",
                        o.start.to_f64(),
                        o.end.to_f64()
                    ));
                }
                // 与判定线同一条纪律：**只动这一个事件的这一个端点**（空隙/重叠留给检测），
                // 但顺序不能乱 —— `perf::active_event` 的二分依赖"按 start 升序"。
                ensure_sorted(&after, &format!("遮蔽区 {zone_idx} 的 {track}"))?;
                *self.zone_track_mut(zone_idx, &track)? = after.clone();
                self.journal.record(Change::ZoneTrack {
                    zone: zone_idx,
                    track,
                    before,
                    after: after.clone(),
                });
                Ok(json!({"zone": zone_idx, "index": index, "edge": edge}))
            }
            "move_zone_event" => {
                let zone_idx = zone_arg(c)?;
                let track = zone_track_arg(c)?;
                let index = usize_arg(c, "index")?;
                let delta = beat_arg(c, "delta")?;
                let before = self.zone_track(zone_idx, &track)?;
                let mut after = before.clone();
                let cur = after
                    .get(index)
                    .cloned()
                    .ok_or_else(|| format!("事件索引 {index} 越界（共 {}）", after.len()))?;
                let start = cur.start.checked_add(delta).ok_or("拍数溢出")?;
                let end = cur.end.checked_add(delta).ok_or("拍数溢出")?;
                if start < Beat::zero() {
                    return Err("事件起点不能为负".into());
                }
                // **不许越过邻块**（用户口径同判定线：有空档才跨得过去）——
                // 顺带保证"按 start 升序"这条不变量不被破坏
                for (i, other) in after.iter().enumerate() {
                    if i == index {
                        continue;
                    }
                    if start < other.end && other.start < end {
                        return Err(format!(
                            "移动后与第 {i} 个事件重叠（{}..{}）—— 通道内不允许重叠",
                            other.start.to_f64(),
                            other.end.to_f64()
                        ));
                    }
                }
                after[index].start = start;
                after[index].end = end;
                ensure_sorted(&after, &format!("遮蔽区 {zone_idx} 的 {track}"))?;
                *self.zone_track_mut(zone_idx, &track)? = after.clone();
                self.journal.record(Change::ZoneTrack {
                    zone: zone_idx,
                    track,
                    before,
                    after,
                });
                Ok(json!({"zone": zone_idx, "index": index, "startBeat": start, "endBeat": end}))
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
                let end = self
                    .doc
                    .chart_end()
                    .checked_add(Beat::new(1024, 1))
                    .ok_or("拍数溢出")?;
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
                let end = self
                    .doc
                    .chart_end()
                    .checked_add(Beat::new(1024, 1))
                    .ok_or("拍数溢出")?;
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

    // ---------------------------------------------------------------- 遮蔽区取用

    fn zone(&self, i: usize) -> Result<&MaskZone, String> {
        self.doc
            .mask_zones
            .get(i)
            .ok_or_else(|| format!("遮蔽区下标 {i} 越界（共 {}）", self.doc.mask_zones.len()))
    }
    fn zone_mut(&mut self, i: usize) -> Result<&mut MaskZone, String> {
        let n = self.doc.mask_zones.len();
        self.doc
            .mask_zones
            .get_mut(i)
            .ok_or_else(|| format!("遮蔽区下标 {i} 越界（共 {n}）"))
    }
    fn zone_track(&self, zone: usize, track: &str) -> Result<Vec<Event>, String> {
        read_zone_track(&self.doc, zone, track)
            .ok_or_else(|| format!("无法读取遮蔽区轨道 zone={zone} track={track}"))
    }
    fn zone_track_mut(&mut self, zone: usize, track: &str) -> Result<&mut Vec<Event>, String> {
        self.zone_mut(zone)?
            .track_mut(track)
            .ok_or_else(|| format!("未知遮蔽区通道 {track}（可选 {MASK_TRACKS:?}）"))
    }

    /// 某条通道在拍 `beat` 处的当前值（新建事件块时的缺省值）。
    ///
    /// 走的是**唯一那份求值**（[`crate::perf::mask_state_at`]）—— 缺省值要是自己算的，
    /// "放下一刻不跳变"这句话就不成立了。`active` 给布尔（写进文件的是 `true`/`false`）。
    fn zone_neutral_value(&self, zone: usize, track: &str, beat: Beat) -> Value {
        let Some(z) = self.doc.mask_zones.get(zone) else {
            return json!(0.0);
        };
        let lists: Vec<Vec<Event>> = MASK_TRACKS
            .iter()
            .map(|t| z.track(t).cloned().unwrap_or_default())
            .collect();
        let r: [&[Event]; 7] = std::array::from_fn(|i| lists[i].as_slice());
        let tmap = crate::perf::TimeMap::from_parts(&self.doc.bpm_list, self.doc.chart_end());
        let st = crate::perf::mask_state_at(&r, beat.to_f64(), &tmap);
        if track == "active" {
            return json!(st.active);
        }
        match track {
            "x1" => json!(st.v[0][0]),
            "y1" => json!(st.v[0][1]),
            "x2" => json!(st.v[1][0]),
            "y2" => json!(st.v[1][1]),
            "x3" => json!(st.v[2][0]),
            "y3" => json!(st.v[2][1]),
            _ => json!(0.0),
        }
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

/// 遮蔽区下标（缺省 0 号 —— 与 `line` 缺省 0 号同一个口径）
fn zone_arg(c: &Value) -> Result<usize, String> {
    Ok(c.get("zone").and_then(|v| v.as_u64()).unwrap_or(0) as usize)
}

/// `del_zone` 的区下标：`zone` 与 `index` 都收。
///
/// 为什么允许两个名字：删一块区时"删第几个"在别的命令里叫 `index`（`del_note`/`del_event`），
/// 而"哪一块区"在本组命令里叫 `zone`。让 agent 猜哪个是哪个，只会换来一次报错和一次重试。
/// **只有这一条命令**这么做 —— 别的命令里的 `index` 是"通道内第几个事件"，混用会取错区。
fn zone_index_arg(c: &Value) -> Result<usize, String> {
    if c.get("zone").is_some() {
        zone_arg(c)
    } else {
        usize_arg(c, "index")
    }
}

/// 遮蔽区通道名（**按 `doc::MASK_TRACKS` 校验**：可选值只有一处出处，报错里也列它）
fn zone_track_arg(c: &Value) -> Result<String, String> {
    let track = c
        .get("track")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("缺少 track（可选 {MASK_TRACKS:?}）"))?
        .to_owned();
    if !MASK_TRACKS.contains(&track.as_str()) {
        return Err(format!("未知遮蔽区通道 {track}（可选 {MASK_TRACKS:?}）"));
    }
    Ok(track)
}

/// `active` 通道的一个事件块**只能是一种状态**（用户口径 2026-10-02：
/// "一个事件块一种状态，不能在头和尾有不同状态"）。
///
/// 头尾落在两档 ⇒ 这块区会在中途换外观（`false` 渐变到 `true` 就是这种）；要换就放**两块**。
/// 判据与两个校验器共用 [`crate::doc::doc_active_state`] 那一份（阈值同求值侧 `≥ 0.5`）。
/// 放在写侧拦一道，是为了让"编辑器造出来的东西"永远过得了自己的校验器。
fn check_active_block(track: &str, ev: &Event) -> Result<(), String> {
    if track != "active" {
        return Ok(());
    }
    let (a, b) = (
        crate::doc::doc_active_state(&ev.start_value),
        crate::doc::doc_active_state(&ev.end_value),
    );
    match (a, b) {
        (None, _) | (_, None) => Err(format!(
            "active 的值必须是布尔（true/false）或数字，收到 {:?} / {:?}",
            ev.start_value, ev.end_value
        )),
        (Some(a), Some(b)) if a != b => Err(format!(
            "active 事件块只能是一种状态：起值 {:?} 与终值 {:?} 分别是 {a} 与 {b} —— \
             想中途换外观就放两块（各是一种状态），别用渐变",
            ev.start_value, ev.end_value
        )),
        _ => Ok(()),
    }
}

/// 事件必须按 `startBeat` 升序 —— `perf::active_event` 的二分依赖它。
///
/// 判定线那边靠"插入时排序"维持；遮蔽区的拖动/改端点会破坏它，所以在这里显式挡一道
/// （宁可报错，也别让求值器在一个乱序的通道上做二分 —— 那会取到随机哪一条）。
fn ensure_sorted(list: &[Event], ptr: &str) -> Result<(), String> {
    if list.windows(2).all(|w| w[0].start <= w[1].start) {
        Ok(())
    } else {
        Err(format!("{ptr}：事件必须按 startBeat 升序（这条通道的求值依赖它）"))
    }
}

fn num(c: &Value, name: &str) -> Option<f64> {
    c.get(name).and_then(|v| v.as_f64())
}

fn beat_arg(c: &Value, name: &str) -> Result<Beat, String> {
    parse_beat(c.get(name).ok_or_else(|| format!("缺少 {name}"))?)
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

#[cfg(test)]
mod mask_zone_tests {
    use super::*;
    use crate::doc::MASK_DEFAULT_TRIANGLE;

    fn exec_ok(c: &mut EditCore, cmd: Value) -> Value {
        let r = c.exec(&cmd);
        assert_eq!(r["ok"], json!(true), "命令失败：{cmd} → {r}");
        r
    }
    fn exec_err(c: &mut EditCore, cmd: Value) -> String {
        let r = c.exec(&cmd);
        assert_eq!(r["ok"], json!(false), "命令本该失败：{cmd} → {r}");
        r["error"].as_str().unwrap_or_default().to_owned()
    }

    /// 某条通道上**起点为 `at`** 的那条事件的终点（放块那几条断言读它 —— 回执只给下标）
    fn end_of(c: &EditCore, track: &str, at: f64) -> f64 {
        c.doc().mask_zones[0]
            .track(track)
            .and_then(|l| {
                l.iter()
                    .find(|e| (e.start.to_f64() - at).abs() < 1e-9)
                    .map(|e| e.end.to_f64())
            })
            .unwrap_or(f64::NAN)
    }

    /// 新建遮蔽区：**中央正三角形**（六条常量事件）+ 能力等级抬到 4
    #[test]
    fn adding_a_zone_writes_the_center_triangle_and_raises_the_capability() {
        let mut c = EditCore::new();
        assert_eq!(c.doc().min_client_capability, 1);
        let r = exec_ok(&mut c, json!({"op": "add_zone", "startBeat": [0, 1]}));
        assert_eq!(r["result"]["index"], json!(0));
        assert_eq!(c.doc().mask_zones.len(), 1);
        assert_eq!(c.doc().min_client_capability, CAP_MASK, "有遮蔽区就得声明能力 4");
        let z = &c.doc().mask_zones[0];
        for t in MASK_TRACKS {
            let n = z.track(t).map(|l| l.len()).unwrap_or(0);
            if t == "active" {
                assert_eq!(n, 0, "active 不写事件（没有它时是 false）");
            } else {
                assert_eq!(n, 1, "{t} 应有一条常量事件");
            }
        }
        // 值 = 中央正三角形，且起点 → 终点是常量（不是斜坡）
        let x1 = &z.x1[0];
        assert_eq!(x1.start_value, json!(MASK_DEFAULT_TRIANGLE[0][0]));
        assert_eq!(x1.start_value, x1.end_value, "常量事件（新建时不该有斜坡）");
        // 终点：**起点 + 1 拍**（用户口径："调整初始屏蔽区事件区间为 0~1 拍"）
        assert_eq!(z.x1[0].end.to_f64(), 1.0);
    }

    /// `empty: true` 建一个**没有任何事件**的区：预览里它不存在（三条坐标轨道都没事件）
    #[test]
    fn an_empty_zone_has_no_events_and_is_not_visible() {
        let mut c = EditCore::new();
        exec_ok(&mut c, json!({"op": "add_zone", "empty": true}));
        let z = &c.doc().mask_zones[0];
        assert_eq!(z.event_count(), 0);
        let chart = crate::state::chart_from_doc(c.doc());
        let st = chart.zones[0].state(&chart.tmap, 0.0);
        assert!(!st.visible, "没有任何坐标事件 ⇒ 不显示");
    }

    /// **先裁后插**：给一条铺满全谱的常量事件里插关键帧之后，文件里不留重叠
    /// （遮蔽区通道的不变量是"无重叠"；编辑器自己的产物必须过得了自己的校验器）
    #[test]
    fn inserting_an_event_trims_the_one_it_covers() {
        let mut c = EditCore::new();
        exec_ok(&mut c, json!({"op": "add_zone", "empty": true}));
        // 一条**铺得很长**的常量事件（真实用法：新建的区往往是"整首歌都待在原地"）
        exec_ok(
            &mut c,
            json!({"op": "add_zone_event", "track": "x1", "startBeat": [0, 1], "endBeat": [64, 1],
                   "startValue": 300.0, "endValue": 300.0}),
        );
        assert_eq!(c.doc().mask_zones[0].x1.len(), 1);
        // 在拍 8 插一块（值给 0 —— 默认会给"此刻的值"，这里显式给）
        exec_ok(
            &mut c,
            json!({"op": "add_zone_event", "track": "x1", "startBeat": [8, 1], "endBeat": [16, 1],
                   "startValue": 0.0, "endValue": -200.0}),
        );
        let x1 = c.doc().mask_zones[0].x1.clone();
        assert_eq!(x1.len(), 2, "两条：被裁短的常量段 + 新块");
        assert_eq!(x1[0].end.to_f64(), 8.0, "前一条被裁到新块起点");
        assert_eq!(x1[0].end_value, json!(300.0), "常量段的值不变");
        // 校验器必须放行（这正是加这道裁剪的理由）
        let issues = crate::cmd::validate(c.doc());
        let mask_errors: Vec<String> = issues
            .iter()
            .filter(|i| i.pointer.starts_with("/maskZones"))
            .map(|i| format!("{} {}", i.pointer, i.message))
            .collect();
        assert!(mask_errors.is_empty(), "{mask_errors:?}");
    }

    /// 通道名按 `MASK_TRACKS` 校验（写错要报错，不能静默变成 no-op）
    #[test]
    fn unknown_channels_and_fields_are_rejected() {
        let mut c = EditCore::new();
        exec_ok(&mut c, json!({"op": "add_zone", "empty": true}));
        let e = exec_err(&mut c, json!({"op": "add_zone_event", "track": "z1", "startBeat": [0, 1]}));
        assert!(e.contains("未知遮蔽区通道"), "{e}");
        let e = exec_err(&mut c, json!({"op": "set_zone", "set": {"zOrder": 3}}));
        assert!(e.contains("不支持的字段"), "{e}");
        let e = exec_err(&mut c, json!({"op": "del_zone", "index": 7}));
        assert!(e.contains("越界"), "{e}");
    }

    /// 新建事件块的缺省值 = **该通道此刻的值**（放下那一刻不跳变）
    #[test]
    fn a_new_event_defaults_to_the_channels_current_value() {
        let mut c = EditCore::new();
        exec_ok(&mut c, json!({"op": "add_zone", "set": {"x1": 300.0}}));
        // 在拍 8 再放一条 x1：起值应当是那一刻的值（常量 300）
        let r = exec_ok(
            &mut c,
            json!({"op": "add_zone_event", "track": "x1", "startBeat": [8, 1], "endBeat": [12, 1]}),
        );
        assert_eq!(r["result"]["index"], json!(1));
        let x1 = c.doc().mask_zones[0].x1.clone();
        assert_eq!(x1[1].start_value, json!(300.0), "缺省值 = 当前值");
        assert_eq!(x1[1].end_value, json!(300.0));
        // active 通道的缺省值是**布尔**（此刻没有 active 事件 ⇒ false）
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "active", "startBeat": [0, 1], "endBeat": [4, 1]}));
        let a = c.doc().mask_zones[0].active.clone();
        assert_eq!(a[0].start_value, json!(false));
    }

    /// **一块区的 active 只能是一种状态**（用户口径 2026-10-02："一个事件块一种状态，
    /// 不能在头和尾有不同状态"）：写侧（`add_zone_event` / `set_zone_event`）直接拒渐变；
    /// `set_zone` 的 `active` 是"整区切档"——已有块全部改写、没有块时按坐标包络写一块。
    #[test]
    fn one_active_block_claims_exactly_one_state() {
        let mut c = EditCore::new();
        exec_ok(&mut c, json!({"op": "add_zone", "set": {"x1": 0.0}}));
        // 渐变（false → true）在建的时候就拒
        let e = exec_err(
            &mut c,
            json!({"op": "add_zone_event", "track": "active", "startBeat": [0, 1], "endBeat": [4, 1],
                   "startValue": false, "endValue": true}),
        );
        assert!(e.contains("只能是一种状态"), "{e}");
        // 数字也按 ≥0.5 二值化：0.2 → 0.8 同样拒（换的是同一档的意思）
        let e = exec_err(
            &mut c,
            json!({"op": "add_zone_event", "track": "active", "startBeat": [0, 1], "endBeat": [4, 1],
                   "startValue": 0.2, "endValue": 0.8}),
        );
        assert!(e.contains("只能是一种状态"), "{e}");
        // 一种状态就放行（写 1 与写 true 等价）
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "active", "startBeat": [0, 1], "endBeat": [4, 1], "startValue": 1, "endValue": 1}));
        // 改端点也不能把它变成渐变：只改一头会被拒
        let e = exec_err(&mut c, json!({"op": "set_zone_event", "track": "active", "index": 0, "set": {"startValue": false}}));
        assert!(e.contains("只能是一种状态"), "{e}");
        // 两头一起改可以（这就是界面那颗复选框发出的形状）
        exec_ok(&mut c, json!({"op": "set_zone_event", "track": "active", "index": 0, "set": {"startValue": false, "endValue": false}}));

        // ---- 整区切档：已有块全部改写，跨度不动；一个撤销步 ----
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "active", "startBeat": [8, 1], "endBeat": [12, 1], "startValue": false, "endValue": false}));
        exec_ok(&mut c, json!({"op": "set_zone", "set": {"active": true}}));
        let a = c.doc().mask_zones[0].active.clone();
        assert_eq!(a.len(), 2, "块数不变（改的是档，不是块）");
        assert_eq!(a[0].start_value, json!(true));
        assert_eq!(a[1].end_value, json!(true));
        assert_eq!(a[1].start.to_f64(), 8.0, "跨度不动");
        exec_ok(&mut c, json!({"op": "undo"}));
        let a = c.doc().mask_zones[0].active.clone();
        assert_eq!(a[0].start_value, json!(false), "一次 Ctrl+Z 全回去");
        assert_eq!(a[1].end_value, json!(false));
        // 自己的校验器必须放行
        let bad: Vec<String> = crate::cmd::validate(c.doc())
            .iter()
            .filter(|i| i.pointer.starts_with("/maskZones"))
            .map(|i| format!("{} {}", i.pointer, i.message))
            .collect();
        assert!(bad.is_empty(), "{bad:?}");
    }

    /// 本来没有 active 块（= false 那档）的区：`set_zone {active:true}` 按**坐标事件的包络**写一块；
    /// 一条坐标事件都没有的区（`empty:true`）则明确拒绝 —— active 在那时没有意义。
    #[test]
    fn turning_a_zone_active_writes_one_block_across_its_coord_span() {
        let mut c = EditCore::new();
        // 坐标事件横跨 [2, 10)：active 那一段就应当是 [2, 10)
        exec_ok(&mut c, json!({"op": "add_zone", "empty": true}));
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "x1", "startBeat": [2, 1], "endBeat": [4, 1], "startValue": 0.0}));
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "y3", "startBeat": [6, 1], "endBeat": [10, 1], "startValue": -100.0}));
        exec_ok(&mut c, json!({"op": "set_zone", "set": {"active": true}}));
        let a = c.doc().mask_zones[0].active.clone();
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].start.to_f64(), a[0].end.to_f64()), (2.0, 10.0), "包络");
        assert_eq!(a[0].start_value, json!(true));
        // 再切回 false：还是那一块，值改了
        exec_ok(&mut c, json!({"op": "set_zone", "set": {"active": false}}));
        let a = c.doc().mask_zones[0].active.clone();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].end_value, json!(false));
        // 一条坐标事件都没有 ⇒ 拒（不猜一个跨度出来）
        exec_ok(&mut c, json!({"op": "add_zone", "empty": true}));
        let e = exec_err(&mut c, json!({"op": "set_zone", "zone": 1, "set": {"active": true}}));
        assert!(e.contains("还没有任何坐标事件"), "{e}");
        // 非布尔/数字 ⇒ 拒（别把字符串塞进去）
        let e = exec_err(&mut c, json!({"op": "set_zone", "set": {"active": "yes"}}));
        assert!(e.contains("需要布尔"), "{e}");
    }

    /// **放一块的跨度规则**（用户口径 2026-10-02："创建流程应与普通编辑模式下的事件块放置一样"）：
    /// 缺省终点 = 起点 + `MASK_EVENT_BEATS` 拍、缩到空档为止；起点重合被拒；显式终点越过下一块被拒。
    ///
    /// 这三条与界面（手势草稿 / 属性编辑器那颗按钮）共用 `edit::mask_*` 那一份判据 ——
    /// 以前属性编辑器写死 4 拍又不夹取，于是"在播放头放一块"能造出一条与下一块重叠的事件。
    #[test]
    fn placing_a_block_never_makes_an_overlap() {
        let mut c = EditCore::new();
        exec_ok(&mut c, json!({"op": "add_zone", "set": {"x1": 100.0}}));
        // 种子块是 [0, 1)：缺省终点 = 1 拍，正好接上，不重叠
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "x1", "startBeat": [1, 1], "endBeat": [5, 1], "startValue": 100.0}));
        // 缺省终点（不带 endBeat）= 起点 + 1 拍
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "x1", "startBeat": [6, 1]}));
        assert_eq!(end_of(&c, "x1", 6.0), 7.0);
        // 空档只剩半拍：缺省终点**缩到空档**（10 拍处先摆一块）
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "x1", "startBeat": [10, 1], "endBeat": [12, 1]}));
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "x1", "startBeat": [19, 2]}));
        assert_eq!(end_of(&c, "x1", 9.5), 10.0, "缺省终点缩到下一块起点");
        // 起点重合 ⇒ 拒（两条起点相同的事件 = 重叠）
        let e = exec_err(&mut c, json!({"op": "add_zone_event", "track": "x1", "startBeat": [6, 1]}));
        assert!(e.contains("已经有一块"), "{e}");
        // 终点**正好**停在下一块起点上是允许的（相接不是重叠）：[5, 6) 紧贴 6 拍那块
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "x1", "startBeat": [5, 1], "endBeat": [6, 1]}));
        // 显式终点越过下一块 ⇒ 拒（"悄悄夹"是缺省值才做的事）
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "x1", "startBeat": [16, 1], "endBeat": [18, 1]}));
        let e = exec_err(&mut c, json!({"op": "add_zone_event", "track": "x1", "startBeat": [13, 1], "endBeat": [17, 1]}));
        assert!(e.contains("越过下一块"), "{e}");
        // 落点在某一块**里面**（不是起点）是允许的：把前一块裁到新起点，切点值不变
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "x1", "startBeat": [3, 1], "endBeat": [4, 1]}));
        // 一整轮下来自己的校验器必须放行（这才是这几道闸存在的理由）
        let issues = crate::cmd::validate(c.doc());
        let bad: Vec<String> = issues
            .iter()
            .filter(|i| i.pointer.starts_with("/maskZones"))
            .map(|i| format!("{} {}", i.pointer, i.message))
            .collect();
        assert!(bad.is_empty(), "{bad:?}");
    }

    /// `resize` / `move` 只动那一个事件块，且**不许越过邻块**（顺序是求值的前提）
    #[test]
    fn resizing_and_moving_respect_the_channel_order() {
        let mut c = EditCore::new();
        exec_ok(&mut c, json!({"op": "add_zone", "empty": true}));
        for (a, b) in [(0, 4), (8, 12)] {
            exec_ok(
                &mut c,
                json!({"op": "add_zone_event", "track": "x1", "startBeat": [a, 1], "endBeat": [b, 1],
                       "startValue": a as f64, "endValue": b as f64}),
            );
        }
        // 第三条：用来验证"顺序被打乱"这道闸
        exec_ok(
            &mut c,
            json!({"op": "add_zone_event", "track": "x1", "startBeat": [20, 1], "endBeat": [24, 1],
                   "startValue": 20.0, "endValue": 24.0}),
        );
        // 把第一条的终点从 4 拖到 6：允许（还没碰到 8）
        exec_ok(&mut c, json!({"op": "resize_zone_event", "track": "x1", "index": 0, "edge": "end", "toBeat": [6, 1]}));
        assert_eq!(c.doc().mask_zones[0].x1[0].end.to_f64(), 6.0);
        // 负拍：拍不能为负（"这条通道的第一条事件可以晚于拍 0"不等于可以早于 0）
        let e = exec_err(&mut c, json!({"op": "resize_zone_event", "track": "x1", "index": 1, "edge": "start", "toBeat": [-1, 1]}));
        assert!(e.contains("拍不能为负"), "{e}");
        // 把第三条的起点改到 8 **之前** ⇒ 通道不再按 start 升序 ⇒ 报错（求值的二分依赖这个不变量）。
        // 端点取 [6,7)：与 [0,6) 和 [8,12) 都只是**相接**（不算重叠），所以撞的是"升序"那道闸，
        // 而不是重叠闸 —— 两道闸各管各的。
        let e = exec_err(&mut c, json!({"op": "set_zone_event", "track": "x1", "index": 2, "set": {"startBeat": [6, 1], "endBeat": [7, 1]}}));
        assert!(e.contains("升序"), "{e}");
        // 换成真的压到别人身上 ⇒ 重叠闸响（`set_zone_event` 以前没有这道闸：
        // 拖一下端点就能造出"自己的校验器不认"的谱面）
        let e = exec_err(&mut c, json!({"op": "set_zone_event", "track": "x1", "index": 2, "set": {"startBeat": [5, 1], "endBeat": [7, 1]}}));
        assert!(e.contains("重叠"), "{e}");
        // `resize_zone_event` 同一条闸：把第一条的终点拖过第二条的起点
        let e = exec_err(&mut c, json!({"op": "resize_zone_event", "track": "x1", "index": 0, "edge": "end", "toBeat": [9, 1]}));
        assert!(e.contains("重叠"), "{e}");
        assert_eq!(c.doc().mask_zones[0].x1[0].end.to_f64(), 6.0, "被拒的命令一个字节都不该改");
        // 平移整块：与邻块重叠 ⇒ 报错
        let e = exec_err(&mut c, json!({"op": "move_zone_event", "track": "x1", "index": 1, "delta": [-6, 1]}));
        assert!(e.contains("重叠"), "{e}");
        assert_eq!(c.doc().mask_zones[0].x1[1].start.to_f64(), 8.0, "失败的命令一个字节都不该改");
        // 往空档里挪是允许的
        exec_ok(&mut c, json!({"op": "move_zone_event", "track": "x1", "index": 1, "delta": [1, 1]}));
        assert_eq!(c.doc().mask_zones[0].x1[1].start.to_f64(), 9.0);
    }

    /// 撤销/重做：遮蔽区的增删与通道改动都是**一步**（含 `active` 的布尔值原样回去）
    #[test]
    fn undo_and_redo_round_trip_the_zones() {
        let mut c = EditCore::new();
        exec_ok(&mut c, json!({"op": "add_zone"}));
        let snapshot = c.doc().to_json();
        exec_ok(&mut c, json!({"op": "add_zone_event", "track": "active", "startBeat": [0, 1], "endBeat": [4, 1], "startValue": true, "endValue": true}));
        exec_ok(&mut c, json!({"op": "del_zone", "index": 0}));
        assert!(c.doc().mask_zones.is_empty());
        exec_ok(&mut c, json!({"op": "undo"}));
        assert_eq!(c.doc().mask_zones.len(), 1, "撤销删区");
        exec_ok(&mut c, json!({"op": "undo"}));
        assert_eq!(c.doc().to_json()["maskZones"], snapshot["maskZones"], "撤回到新建那一刻");
        // 能力等级随后退回去（最后一块区没了 ⇒ 不再需要 4）
        exec_ok(&mut c, json!({"op": "del_zone", "index": 0}));
        assert_eq!(c.doc().min_client_capability, 1);
    }

    /// **`maskZones` 空数组不写进文件**：没有遮蔽区的谱面与加这个字段之前逐字节相同
    #[test]
    fn a_chart_without_zones_has_no_maskzones_key() {
        let c = EditCore::new();
        let v = c.doc().to_json();
        assert!(v.get("maskZones").is_none(), "空数组不该写进文件：{v}");
        // 有一块区时才出现，且 opm → opm 原样读回
        let mut c2 = EditCore::new();
        exec_ok(&mut c2, json!({"op": "add_zone"}));
        let v = c2.doc().to_json();
        assert!(v.get("maskZones").is_some());
        let back = Document::from_json(v).expect("opm 读回");
        assert_eq!(back.mask_zones.len(), 1);
        assert_eq!(back.mask_zones[0].x2, c2.doc().mask_zones[0].x2);
    }
}
