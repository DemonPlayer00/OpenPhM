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
use crate::cmd::{parse_beat, validate, EASINGS};
use crate::doc::{Beat, Document, Event, JudgeLine, Note, NoteKind, TRACKS};
use crate::journal::{read_track, Change, Journal, LineProps};

/// 最近广播的保留条数（调试面板 / `{"op":"broadcasts"}` 用）
const BROADCAST_RING: usize = 512;

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
}

/// 保存目标格式（`Auto` 按扩展名：`*.opm.json` → opm，其余 `*.json` → RPE 生态习惯）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveFormat {
    Auto,
    /// **opm 容器**（`.opm`）：ZIP，含谱面与资源 —— 这是"opm 文件"的正式形态
    Opm,
    /// 裸 opm（`.opm.json`）：可 diff、可入版本库；开发/agent/测试用
    OpmBare,
    Rpe,
}

impl From<Format> for SaveFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Opm => SaveFormat::OpmBare, // 裸进裸出
            Format::OpmZip => SaveFormat::Opm,   // 容器进容器出
            Format::Rpe => SaveFormat::Rpe,
        }
    }
}

impl SaveFormat {
    pub fn parse(s: Option<&str>) -> Option<Self> {
        match s? {
            "auto" => Some(SaveFormat::Auto),
            "opm" | "opmz" | "container" => Some(SaveFormat::Opm),
            "opm-bare" | "opm.json" => Some(SaveFormat::OpmBare),
            "rpe" => Some(SaveFormat::Rpe),
            _ => None,
        }
    }
    /// 决定实际写哪种格式：显式指定就用它，`Auto` 看扩展名，都判不出来时**沿用来源格式**
    pub fn resolve(self, path: &Path, loaded: Format) -> Format {
        match self {
            SaveFormat::Opm => Format::OpmZip,      // "opm" = 容器（正式形态）
            SaveFormat::OpmBare => Format::Opm,     // 裸工程文件
            SaveFormat::Rpe => Format::Rpe,
            SaveFormat::Auto => {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.ends_with(".opm.json") {
                    Format::Opm // 显式要裸工程文件
                } else if name.ends_with(".opm") || name.ends_with(".opmz") {
                    Format::OpmZip // `.opm` = 容器（正式形态）
                } else if name.ends_with(".json") {
                    Format::Rpe
                } else {
                    loaded
                }
            }
        }
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
        }
    }

    /// 载入谱面：**按内容判断格式**（opm 原生或 RPE），返回保真度报告。
    ///
    /// 扩展名不参与判断 —— 两种格式都是 `.json`，`.opm.json` 只是本项目的命名习惯。
    pub fn load_reporting(path: &Path) -> Result<(Self, codec::Fidelity), String> {
        // **按字节分流**：ZIP 魔数 ⇒ opm 容器（谱面 + 资源），否则当 JSON（裸 opm / RPE）。
        // 这里以前直接 `read_to_string` —— 容器一上来就会以"不是 UTF-8"失败。
        let (doc, assets, fid) = codec::load_file_with_assets(path)?;
        let fmt = codec::Format::parse(&fid.source).unwrap_or(codec::Format::Opm);
        let rpe_target = if fmt == codec::Format::Rpe {
            // 沿用来源文件的版本档位（`META.RPEVersion` 不可信，但作为"写回哪一档"的依据可用）
            let v = fid.version.clone();
            let n = v
                .split('=')
                .nth(1)
                .and_then(|x| x.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|x| x.parse::<i64>().ok())
                .unwrap_or(160);
            codec::rpe::RpeTarget { version: if n > 0 { n } else { 160 }, ..Default::default() }
        } else {
            codec::rpe::RpeTarget::default()
        };
        let core = Self {
            doc,
            journal: Journal::default(),
            path: Some(path.to_path_buf()),
            revision: 0,
            log: vec![format!("load({}) 格式={}", path.display(), fmt.as_str())],
            subscribers: Subscribers::default(),
            broadcasts: VecDeque::new(),
            last_delivered: 0,
            origin: Origin::Local,
            verbose: false,
            saved_revision: 0,
            source_format: fmt,
            rpe_target,
            last_fidelity: Some(fid.clone()),
            assets, // 裸 JSON 时为空；容器时是包里的资源
            overlaps: Vec::new(), // 下面立刻全量扫一遍（载入是"整表变化"）
        };
        let mut core = core;
        core.refresh_overlaps_all();
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
        let target = path
            .map(|p| p.to_path_buf())
            .or_else(|| self.path.clone())
            .ok_or("未指定保存路径")?;
        // 显式给了新路径（Save As）时按扩展名重新判断格式；同路径保存则沿用载入格式
        let fmt = if path.is_some() { SaveFormat::Auto } else { SaveFormat::from(self.source_format) };
        let (target, fid) = self.save_as(&target, fmt)?;
        self.last_fidelity = Some(fid);
        Ok(target)
    }

    /// 另存为：`fmt` 决定写哪种格式（`Auto` 按扩展名）
    pub fn save_as(
        &mut self,
        path: &Path,
        fmt: SaveFormat,
    ) -> Result<(PathBuf, codec::Fidelity), String> {
        let fmt = fmt.resolve(path, self.source_format);
        // 目标目录不存在就**明说**：内核不会替你建目录，`写入失败: No such file or directory`
        // 这种原始错误对用户毫无指向性（"我明明选了文件夹"）。
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            if !dir.is_dir() {
                return Err(format!(
                    "目标目录不存在：{}（先建好目录，或用「选择文件夹…」挑一个已有的）",
                    dir.display()
                ));
            }
        }
        let (target, fid) = match fmt {
            codec::Format::Opm => {
                // 裸 opm（`.opm.json`）：可 diff、可入版本库
                let text = serde_json::to_string_pretty(&self.doc.to_json())
                    .map_err(|e| format!("序列化失败: {e}"))?;
                std::fs::write(path, format!("{text}\n")).map_err(|e| format!("写入失败: {e}"))?;
                let mut fid = codec::Fidelity::new("opm", format!("v{}", self.doc.format_version));
                fid.note("原生格式，无转换");
                (path.to_path_buf(), fid)
            }
            codec::Format::OpmZip => {
                // **opm 容器**：谱面 + 资源。已有的资源（例如刚从这个容器载入的）原样带回去；
                // 文档新引用的外部文件（`meta.audio`/`meta.background`）从它所在目录读进来。
                let base = path.parent().map(|p| p.to_path_buf());
                let mut fid = codec::Fidelity::new("opm", "容器（zip）".to_owned());
                // 资源**先按当前字段**收集（字段里可能还是 `/tmp/x/song.ogg` 这样的外部路径，
                // 那时才读得到盘）；随后才把字段规范成包内相对名。
                let assets = codec::container::collect_assets(
                    &self.doc,
                    &self.assets,
                    base.as_deref(),
                    &mut fid,
                );
                // 资源在包里用**文件名**（不是外部绝对路径）：写进 `opm.json` 的文档必须同步改写，
                // 否则"读回来"会去找 `/tmp/.../song.ogg` 这种容器里根本没有的条目。
                let renames = planned_asset_renames(&self.doc);
                let mut shifted = self.doc.clone();
                for r in &renames {
                    r.apply(&mut shifted);
                    fid.note(format!(
                        "{} `{}` → 包内相对名 `{}`（文档字段同步改写，可撤销）",
                        r.label, r.from, r.to
                    ));
                }
                // **副本**送去写盘：文件真的写出来了，才把这次改名落到内存文档。
                // 早先它直接改内存文档而 `write_file` 在之后 —— 写盘失败就会留下
                // "文档被改、文件没写、没有任何信号"的状态；而且那次改动没 +revision、没广播，
                // 界面缓存看不见它（保存链路上唯一一处绕过命令路径的改写）。
                let doc_for_write = if renames.is_empty() { &self.doc } else { &shifted };
                let backend = codec::container::write_file(doc_for_write, &assets, path)?;
                fid.note(format!(
                    "容器：{} 个资源 + 谱面 `opm.json`（打包后端：{}）",
                    assets.len(),
                    backend.name()
                ));
                fid.finalize();
                self.assets = assets; // 存完把资源留在内存里，下次保存不必再读盘
                // 写成功了，才把资源名落到内存文档 —— 走**命令路径**，与用户自己改 `meta` 同一条路：
                // 记 journal（可撤销）、revision +1、按 set_meta 的话题广播。
                if !renames.is_empty() {
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
                (path.to_path_buf(), fid)
            }
            codec::Format::Rpe => {
                let fid = codec::rpe::save_file(&self.doc, path, self.rpe_target)?;
                (path.to_path_buf(), fid)
            }
        };
        self.path = Some(target.clone());
        self.source_format = fmt;
        self.saved_revision = self.revision; // 存过就不算脏
        self.last_fidelity = Some(fid.clone());
        Ok((target, fid))
    }

    /// 把另一个文件**装进当前核心**（GUI「打开」/控制通道 `{"op":"load"}` 走这里）。
    ///
    /// 整体替换是数据结构层面的"全量变更"：撤销栈清空、revision +1、按**全量话题**广播，
    /// 于是 GUI 只会走一次"整表重建"（`structure` 脏位），不需要各面板自己去猜。
    pub fn load_into(&mut self, path: &Path) -> Result<Fidelity, String> {
        let (doc, assets, fid) = codec::load_file_with_assets(path)?;
        let origin = self.origin;
        self.doc = doc;
        self.assets = assets;
        self.journal = Journal::default(); // 换了谱面，旧的逆操作全部作废
        self.path = Some(path.to_path_buf());
        self.source_format = codec::Format::parse(&fid.source).unwrap_or(codec::Format::Opm);
        if self.source_format == codec::Format::Rpe {
            let n = fid
                .version
                .split('=')
                .nth(1)
                .and_then(|x| x.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|x| x.parse::<i64>().ok())
                .unwrap_or(160);
            self.rpe_target = codec::rpe::RpeTarget { version: if n > 0 { n } else { 160 }, ..Default::default() };
        }
        self.revision += 1;
        self.saved_revision = self.revision;
        self.last_fidelity = Some(fid.clone());
        self.refresh_overlaps_all(); // 整表换了，全量
        self.log.push(format!(
            "load_into({}) 格式={} 判定线={} 音符={}",
            path.display(),
            self.source_format.as_str(),
            self.doc.judge_lines.len(),
            self.doc.judge_lines.iter().map(|l| l.notes.len()).sum::<usize>()
        ));
        // 全量话题：线集合、时间映射、所有属性/轨道/音符都可能是新的
        let topics = Self::all_topics();
        self.emit(origin, format!("载入 {}", path.display()), topics, Vec::new());
        Ok(fid)
    }

    /// 用一份现成文档替换（转换工具/测试用）。不广播：这种核心通常还没有订阅者；
    /// GUI 里换谱面请走 [`EditCore::load_into`]（那条会按全量话题广播）。
    pub fn replace_doc(&mut self, doc: Document) {
        self.doc = doc;
        self.journal = Journal::default();
        self.revision += 1;
        self.saved_revision = self.revision;
        self.refresh_overlaps_all(); // 整份文档被换掉，重叠缓存必须跟着换
    }

    /// 只读访问文档。要改？走 `exec`（命令），别想要 `&mut`。
    pub fn doc(&self) -> &Document {
        &self.doc
    }

    /// 有未保存改动吗（保守口径，见 `saved_revision` 注释）
    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    /// 只读访问更改日志。要改？走 `exec`（命令）—— 外面拿不到 `&mut Journal`。
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// 当前保存目标；`None` = 还没落过盘。
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
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
        let topics = self.journal.undo_top().map(topics_of);
        let origin = self.origin;
        let r = self.journal.undo(&mut self.doc)?;
        if let Some(ref label) = r {
            self.revision += 1;
            // 一次撤销可能跨多条线（事务）⇒ 全量重算，别猜
            self.refresh_overlaps_all();
            let changes = self.journal.recent(8);
            self.emit(origin, format!("undo: {label}"), topics.unwrap_or_else(Self::all_topics), changes);
        }
        Ok(r)
    }

    pub fn redo(&mut self) -> Result<Option<String>, String> {
        let topics = self.journal.redo_top().map(topics_of);
        let origin = self.origin;
        let r = self.journal.redo(&mut self.doc)?;
        if let Some(ref label) = r {
            self.revision += 1;
            self.refresh_overlaps_all();
            let changes = self.journal.recent(8);
            self.emit(origin, format!("redo: {label}"), topics.unwrap_or_else(Self::all_topics), changes);
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
                    background: None,
                    foreign: Default::default(),
                };
                self.doc = Document::fresh(meta, if bpm > 0.0 { bpm } else { 174.0 });
                self.journal = Journal::default();
                self.path = None;
                self.source_format = codec::Format::Opm;
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
            "validate" => return Ok(validate_json(&self.doc)),
            "ping" => return Ok(json!({"pong": true, "revision": self.revision})),
            "save" => {
                let path = c.get("path").and_then(|v| v.as_str()).map(PathBuf::from);
                let fmt = SaveFormat::parse(c.get("format").and_then(|v| v.as_str()));
                match (path, fmt) {
                    (Some(p), Some(f)) => {
                        let (p, fid) = self.save_as(&p, f)?;
                        return Ok(json!({
                            "path": p.display().to_string(),
                            "format": f.resolve(&p, self.source_format).as_str(),
                            "fidelity": fidelity_json(&fid),
                        }));
                    }
                    (Some(p), None) => {
                        // 显式路径但没指定格式：按扩展名
                        let (p, fid) = self.save_as(&p, SaveFormat::Auto)?;
                        return Ok(json!({
                            "path": p.display().to_string(),
                            "format": self.source_format.as_str(),
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
                    .unwrap_or(crate::state::RPE_LINE_HALF_W as f64 * 2.0) as f32;
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
                if let Err(e) = self.journal.abort(&mut self.doc) {
                    self.log.push(format!("abort 回滚异常: {e}"));
                }
                if had {
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
                if auto {
                    // 命令失败：回滚它已经做了一半的改动（`normalize` 这类会边算边写），
                    // 保证"失败的命令不改变文档"这句话是真的
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
                if !EASINGS.contains(&easing.as_str()) {
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
                                if !EASINGS.contains(&s) {
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
                let mid = match (&ev.start_value, &ev.end_value) {
                    (Value::Number(a), Value::Number(b)) => {
                        let a = a.as_f64().unwrap_or(0.0);
                        let b = b.as_f64().unwrap_or(0.0);
                        let span = (ev.end.to_f64() - ev.start.to_f64()).max(1e-9);
                        let t = (at.to_f64() - ev.start.to_f64()) / span;
                        json!(a + (b - a) * t)
                    }
                    _ => ev.start_value.clone(),
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
                let end = add_beat(self.doc.chart_end(), Beat::new(1024, 1)).ok_or("拍数溢出")?;
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
                            let mut after = before.clone();
                            after.sort_by(|a, b| a.start.cmp(&b.start));
                            for i in 1..after.len() {
                                let prev_end = after[i - 1].end;
                                if after[i].start != prev_end {
                                    after[i].start = prev_end;
                                    fixes += 1;
                                }
                                if after[i].end <= after[i].start {
                                    after[i].end = add_beat(after[i].start, Beat::new(1, 1)).ok_or("拍数溢出")?;
                                    fixes += 1;
                                }
                            }
                            if after[0].start > Beat::zero() {
                                after[0].start = Beat::zero();
                                fixes += 1;
                            }
                            let last = after.len() - 1;
                            if after[last].end < end {
                                after[last].end = end;
                                fixes += 1;
                            }
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
                }
                Ok(json!({"fixes": fixes, "chartEnd": self.doc.chart_end().to_f64()}))
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

fn validate_json(doc: &Document) -> Value {
    let issues = validate(doc);
    let errors = issues
        .iter()
        .filter(|i| i.severity == crate::cmd::Severity::Error)
        .count();
    json!({
        "errors": errors,
        "warnings": issues.len() - errors,
        "issues": issues.iter().map(|i| json!({
            "severity": match i.severity { crate::cmd::Severity::Error => "ERROR", crate::cmd::Severity::Warn => "WARN" },
            "pointer": i.pointer,
            "message": i.message,
        })).collect::<Vec<_>>(),
    })
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
}
