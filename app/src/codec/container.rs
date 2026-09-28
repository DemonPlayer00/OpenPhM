//! opm 的**容器形态**：`.opm` 是一个 ZIP，里面是谱面 + 音乐 + 曲绘等资源。
//!
//! 布局（沿用 `spec/opm-format.md` 第 172 行的约定：根目录 `opm.json` + 资源）：
//!
//! ```text
//! chart.opm
//! ├─ opm.json          谱面本身（就是裸 opm 的那些字段，原样）
//! ├─ song.ogg          音乐：文件名 = `meta.audio`
//! ├─ bg.png            曲绘/背景：文件名 = `meta.background`
//! └─ …                 其它资源**原样保留**（不认识的也带过去，不许悄悄丢）
//! ```
//!
//! 两条刻意的不变量：
//! 1. **裸 JSON 仍然支持**（`.opm.json`）：可 diff、可入版本库、agent/测试都用它 ——
//!    容器是**分发形态**，不是唯一形态（与 Krita 的 `.kra`(zip) / `.krita`(纯 XML) 同一个取舍）；
//! 2. **重写容器不丢资源**：读进来的资源留在内存里，保存时原样写回；缺的（磁盘上也找不到）
//!    只**报警告**，不静默产出缺音乐的包。

use std::path::{Path, PathBuf};

use crate::doc::Document;
use crate::zip;

use super::Fidelity;

/// 容器里谱面的固定名字（规范里就是它）
pub const CHART_NAME: &str = "opm.json";

/// 一个容器：谱面 + 资源
#[derive(Clone, Debug)]
pub struct Container {
    pub doc: Document,
    /// 资源：名字（容器内相对路径）→ 字节。**顺序保留**（写回时先谱面、再按原顺序）
    pub assets: Vec<zip::Entry>,
}

/// 从字节读容器
pub fn read(bytes: &[u8]) -> Result<(Container, Fidelity), String> {
    if !zip::looks_like_zip(bytes) {
        return Err("不是 opm 容器（缺少 ZIP 头）".to_owned());
    }
    let mut fid = Fidelity::new("opm", "容器（zip）".to_owned());
    let entries = unpack(bytes, &mut fid)?;
    let c = read_entries(entries, &mut fid)?;
    fid.finalize();
    Ok((c, fid))
}

/// **解包成条目**：内置解压优先（纯内存、快、覆盖 STORE/DEFLATE），搞不定的（ZIP64、加密、
/// 特殊方法）交给系统 7z —— 它久经考验，能把话说明白。
///
/// 抽出来的理由：opm 容器与 RPE 谱面包（`.pez`）**共用这一条解包链**，而"这是哪一种包"
/// 只能等解开、看过条目名才知道（见 `EditCore::stage_file`）。
pub fn unpack(bytes: &[u8], fid: &mut Fidelity) -> Result<Vec<zip::Entry>, String> {
    match zip::read(bytes) {
        Ok(e) => Ok(e),
        Err(builtin_err) => match zip::unpack_with(zip::Backend::SevenZip, bytes) {
            Ok(e) => {
                fid.note("内置解压搞不定这个包（ZIP64/特殊方法？），改用系统 7z 解开");
                Ok(e)
            }
            Err(z7_err) => Err(format!("解压失败：内置（{builtin_err}）；7z（{z7_err}）")),
        },
    }
}

/// 从**已经解开的条目**读 opm 容器（`read` 与"载入文件"共用；这里不再碰压缩）
pub fn read_entries(entries: Vec<zip::Entry>, fid: &mut Fidelity) -> Result<Container, String> {
    // 谱面：优先 `opm.json`，其次唯一的 `*.opm.json`（容忍别家工具换个名字）
    let chart_at = entries
        .iter()
        .position(|e| e.name == CHART_NAME)
        .or_else(|| {
            let mut it = entries.iter().enumerate().filter(|(_, e)| {
                e.name.ends_with(".opm.json") || e.name.ends_with(".json")
            });
            match (it.next(), it.next()) {
                (Some((i, _)), None) => Some(i),
                _ => None,
            }
        })
        .ok_or_else(|| {
            format!(
                "容器里没有谱面（找不到 `{CHART_NAME}`；实际条目：{}）",
                entries
                    .iter()
                    .map(|e| e.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let (chart_name, chart_bytes) = {
        let e = &entries[chart_at];
        (e.name.clone(), e.data.clone())
    };
    if chart_name != CHART_NAME {
        fid.warn(format!(
            "谱面文件名是 `{chart_name}` 而不是 `{CHART_NAME}` —— 已按它读取，写回时会改成 `{CHART_NAME}`"
        ));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&chart_bytes).map_err(|e| format!("`{chart_name}` 不是合法 JSON: {e}"))?;
    let doc = Document::from_json(v)?;

    let assets: Vec<zip::Entry> = entries
        .into_iter()
        .enumerate()
        .filter(|(i, _)| *i != chart_at)
        .map(|(_, e)| e)
        .collect();
    let mut bytes_total = 0usize;
    for a in &assets {
        bytes_total += a.data.len();
    }
    fid.note(format!(
        "容器：{} 个资源（共 {} KiB）",
        assets.len(),
        (bytes_total + 1023) / 1024
    ));
    // 文档引用的资源在不在包里 —— 不在就明说（别让用户以为音乐跟着走了）
    for (what, want) in [("音乐", doc.meta.audio.as_deref()), ("曲绘/背景", doc.meta.background.as_deref())] {
        if let Some(name) = want.filter(|n| !n.trim().is_empty()) {
            match assets.iter().find(|a| a.name == *name) {
                Some(a) => fid.note(format!("{what} `{name}` 在容器内（{} KiB）", a.data.len() / 1024)),
                None => fid.warn(format!(
                    "{what} `{name}` **不在容器内**（`meta` 里写着它，但包里没有对应条目）"
                )),
            }
        }
    }
    fid.finalize();
    Ok(Container { doc, assets })
}

/// 写容器：**优先系统 7z**（谱面文本 Deflate、媒体 Copy），没有 7z 时用内置实现（全 STORE）。
/// 返回 `(字节, 用的后端)` —— 后端写进保真度报告，别让"谁打的包"变成谜。
pub fn write(doc: &Document, assets: &[zip::Entry]) -> Result<(Vec<u8>, zip::Backend), String> {
    let chart = serde_json::to_vec_pretty(&doc.to_json()).map_err(|e| format!("序列化失败: {e}"))?;
    let mut files: Vec<zip::Entry> = Vec::with_capacity(assets.len() + 1);
    files.push(zip::Entry { name: CHART_NAME.to_owned(), data: chart });
    for a in assets {
        if a.name == CHART_NAME {
            continue; // 别让资源把谱面顶掉
        }
        files.push(a.clone());
    }
    zip::pack_preferred(&files)
}

/// 容器/谱面包的**条目清单**：谱面 + 资源（顺序固定：谱面在前）。
///
/// 打包（zip）与不打包（目录）两种形态共用这一份 —— 于是"打成包"和"摊成文件夹"
/// **内容逐字节相同**，不会出现"包里有、文件夹里没有"这种两套逻辑的偏差。
pub fn entries_for_dir(doc: &Document, assets: &[zip::Entry]) -> Vec<zip::Entry> {
    let chart = serde_json::to_vec_pretty(&doc.to_json())
        .unwrap_or_else(|_| b"{}".to_vec());
    let mut files: Vec<zip::Entry> = Vec::with_capacity(assets.len() + 1);
    files.push(zip::Entry { name: CHART_NAME.to_owned(), data: chart });
    for a in assets {
        if a.name == CHART_NAME {
            continue; // 别让资源把谱面顶掉
        }
        files.push(a.clone());
    }
    files
}

/// 把一组条目**写进一个目录**（无压缩形态：`opm.json` + 资源，或 RPE 谱面包的三件套）。
///
/// 目录不存在就建。条目名必须是**纯文件名**（带目录的一律拒绝）—— 包内名字来自文档字段，
/// 而文档字段是可以被手改成 `../x` 的；放行等于让"保存"写到目标目录之外。
pub fn write_entries_to_dir(
    entries: &[zip::Entry],
    dir: &Path,
    fid: &mut Fidelity,
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("建目录 {} 失败: {e}", dir.display()))?;
    let mut bytes = 0usize;
    for e in entries {
        if e.name.is_empty() || e.name.contains('/') || e.name.contains('\\') {
            return Err(format!(
                "条目名 {:?} 不是合法文件名（不能带目录）—— 检查 meta.audio / meta.background",
                e.name
            ));
        }
        let p = dir.join(&e.name);
        std::fs::write(&p, &e.data).map_err(|e| format!("写入 {} 失败: {e}", p.display()))?;
        bytes += e.data.len();
    }
    fid.note(format!(
        "无压缩文件夹：{} 个文件（共 {} KiB）→ {}",
        entries.len(),
        (bytes + 1023) / 1024,
        dir.display()
    ));
    Ok(())
}

/// 把资源逐个落到 `dir`，并追加进 `out`（"包内名 → 落盘路径"）。
///
/// **只取文件名**：容器内可能是 `assets/song.ogg` 这种带目录的名字，而 `../` 之类不许写到
/// 缓存目录之外（名字只用于定位，不参与"写到哪"）。
///
/// 这段规则曾经有两份：`extract_container_into` 与一个叫 `extract_assets` 的旧函数
/// （后者还声称"内部走前者"，实际是抄了一份，而且**已经没有调用点**）。
/// "越界写"这种规则漏掉一份的后果不是报错，是文件被写到目录外面 —— 只留一处。
fn write_assets(
    dir: &Path,
    assets: &[zip::Entry],
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), String> {
    for a in assets {
        let file = Path::new(&a.name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("asset.bin");
        let p = dir.join(file);
        std::fs::write(&p, &a.data).map_err(|e| format!("写缓存失败 {}: {e}", p.display()))?;
        out.push((a.name.clone(), p));
    }
    Ok(())
}

/// 写容器到文件；返回用的后端
pub fn write_file(
    doc: &Document,
    assets: &[zip::Entry],
    path: &Path,
) -> Result<zip::Backend, String> {
    let (bytes, backend) = write(doc, assets)?;
    std::fs::write(path, bytes).map_err(|e| format!("写入失败: {e}"))?;
    Ok(backend)
}

/// 从文件读容器
pub fn read_file(path: &Path) -> Result<(Container, Fidelity), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读取失败: {e}"))?;
    read(&bytes)
}

/// **解压出来的谱面（含资源）**放哪儿：`<临时目录>/opm/<key>/`（用户指定）。
///
/// - Linux ⇒ `/tmp/opm/<key>/`（设了 `TMPDIR` 就跟着它走）；Windows ⇒ `%TEMP%\opm\<key>\`；
/// - `<key>` = 容器内容的 hash（[`cache_key`]）：同一个包反复打开落在**同一个**目录；
/// - 为什么是临时目录而不是 `~/.cache`：容器是**自包含**的，摊出来的这份是"这次编辑要按路径访问的
///   东西"（音频必须落成真实文件才装载得了），用完即弃、重启即清 —— 不该长期留在用户家目录里。
///
/// **注意 `/tmp` 在多数机器上是 tmpfs**（本机 16 GB 内存盘）：摊出来的东西是**内存**，
/// 所以有总量上限与修剪（见 [`CACHE_CAP_BYTES`] 与 [`prune_cache`]）。
pub fn cache_root() -> PathBuf {
    std::env::temp_dir().join("opm")
}

/// 某个容器内容对应的解压目录（**纯函数**，便于测试；`root` 由调用方给）
pub fn extract_dir_in(root: &Path, key: &str) -> PathBuf {
    root.join(key)
}

/// [`cache_root`] 下的解压目录
pub fn extract_dir(key: &str) -> PathBuf {
    extract_dir_in(&cache_root(), key)
}

/// 解压缓存的总量上限（字节）。**512 MB**：本机 `/tmp` 是内存盘，40 MB 级的谱面能放十几份，
/// 又不会让几个大谱面把内存吃掉一大块。超了按 mtime 从旧到新删（正在用的那份刚写过 ⇒ 不会被删）。
pub const CACHE_CAP_BYTES: u64 = 512 * 1024 * 1024;

/// 一个目录（只下一层）的总字节数
pub(crate) fn dir_bytes(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// 按上限修剪解压缓存：保留**最近写过**的那些，删到总量 ≤ `cap`。返回（删了几个、删了多少字节）。
///
/// 为什么要它：`/tmp` 常是 tmpfs ⇒ 摊出来的是内存。没有上限的话，翻十几个带音频的容器就会
/// 常驻几百 MB 内存直到重启 —— 而这份缓存本来就是"用完即弃"的东西。
pub fn prune_cache(root: &Path, cap: u64) -> (usize, u64) {
    let Ok(rd) = std::fs::read_dir(root) else { return (0, 0) };
    let mut entries: Vec<(std::time::SystemTime, PathBuf, u64)> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let m = e.metadata().ok()?;
            if !m.is_dir() {
                return None;
            }
            let when = m.modified().ok()?;
            Some((when, p.clone(), dir_bytes(&p)))
        })
        .collect();
    // 新的在前：留着；剩下的一直删到总量达标
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    let mut kept: u64 = 0;
    let mut removed = 0usize;
    let mut freed = 0u64;
    for (_, path, bytes) in entries {
        if kept + bytes <= cap {
            kept += bytes;
            continue;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
            freed += bytes;
        }
    }
    (removed, freed)
}

/// 把**容器内容**摊到 `dir`：谱面本体写成 `opm.json`（与包内同名、同样格式），资源逐个落盘。
/// 返回"包内名 → 落盘路径"。
///
/// 为什么要连谱面本体一起摊：用户要的是"**解压的谱面**放到缓存"，而且这样缓存目录里就是一份
/// 完整可读的工程（`opm.json` + 音频 + 曲绘），出问题时可以直接去看。
pub fn extract_container_into(
    dir: &Path,
    doc: &Document,
    assets: &[zip::Entry],
) -> Result<Vec<(String, PathBuf)>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("建缓存目录失败 {}: {e}", dir.display()))?;
    let mut out = Vec::with_capacity(assets.len() + 1);
    let chart = serde_json::to_vec_pretty(&doc.to_json()).map_err(|e| format!("序列化失败: {e}"))?;
    let chart_path = dir.join(CHART_NAME);
    std::fs::write(&chart_path, chart).map_err(|e| format!("写 {} 失败: {e}", chart_path.display()))?;
    out.push((CHART_NAME.to_owned(), chart_path));
    write_assets(dir, assets, &mut out)?;
    Ok(out)
}

/// 缓存 key：容器内容的 hash（同样的包落到同一个目录，省得反复解压）
pub fn cache_key(bytes: &[u8]) -> String {
    format!("{:08x}", zip::crc32(bytes))
}

// ---------------------------------------------------------------- 会话元数据 / 遗留缓存
//
// 每个解压目录里放一份 `session.json`，说明"这份缓存是谁、什么时候、为哪个谱面摊出来的"。
//
// **它存在的理由是"留下的那份 = 没退干净"**：正常退出时本程序会删掉自己的解压目录
// （见 `main.rs` 末尾的清理），所以下次启动时还躺在 `<临时目录>/opm` 里的目录，
// 只能是**上一个进程被强杀或崩溃**留下的。但"目录里有什么"（`opm.json` + 音频）分不清是谁留的：
// `opm-ctl` 转换完**按设计不清理**，它的目录也不该在 GUI 启动时被当成"上次崩了"来问。

/// 会话元数据的文件名（放在解压目录里，与 `opm.json` 同级）
pub const SESSION_NAME: &str = "session.json";

/// 一份解压缓存的出处与状态（见本段开头的说明）
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Session {
    /// 摊出这份缓存的进程号
    #[serde(default)]
    pub pid: u32,
    /// 进程名（`opm-app` / `opm-app.exe` / `opm-ctl` / 测试二进制）—— 只有 GUI 留下的才算"上次没退干净"
    #[serde(default)]
    pub exe: String,
    /// 原始谱面文件（保存目标）。新建的谱面还没有目标 ⇒ `None`
    #[serde(default)]
    pub source: Option<String>,
    /// 来源格式 token（[`super::Format::as_str`]）；继续编辑时按它恢复"存回哪种格式"
    #[serde(default)]
    pub format: String,
    /// 谱面名（对话框要显示它，不值得为一行字去解析整份文档）
    #[serde(default)]
    pub name: String,
    /// 摊出缓存的时刻（unix 秒）
    #[serde(default)]
    pub started: u64,
    /// 最近一次把**文档快照**写回缓存的时刻（0 = 从没写过 ⇒ 缓存里就是载入容器时的那份原样）
    #[serde(default)]
    pub snapshot: u64,
    /// 最近一次快照时，文档是否**有未保存改动**（决定"继续"会不会把改动带回来）
    #[serde(default)]
    pub dirty: bool,
}

impl Session {
    /// 这份缓存是 **GUI 会话**留下的吗？
    ///
    /// `opm-ctl`（以及测试二进制）的缓存**按设计就是不清理的** —— 它们不该在 GUI 启动时
    /// 触发"上次没有正常退出"。判据只看进程名，不看时间/大小（那些推不出出处）。
    pub fn is_gui(&self) -> bool {
        self.exe.starts_with("opm-app")
    }

    /// 缓存里有没有**磁盘上没有**的改动
    pub fn has_unsaved(&self) -> bool {
        self.dirty
    }

    /// 快照写于多久之前（秒）；**从没写过**（`snapshot == 0`）⇒ `None`
    pub fn snapshot_age_secs(&self, now: u64) -> Option<u64> {
        if self.snapshot == 0 {
            None
        } else {
            Some(now.saturating_sub(self.snapshot))
        }
    }
}

/// 当前时刻（unix 秒）。取不到系统时间（时钟早于 1970）时给 0 —— 只影响显示，不影响判定。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 当前进程名（不带目录）：写进缓存，用来区分"谁留下的"
pub fn exe_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "?".to_owned())
}

/// 写一份会话元数据到 `dir`
pub fn write_session(dir: &Path, s: &Session) -> Result<(), String> {
    let p = dir.join(SESSION_NAME);
    let text = serde_json::to_vec_pretty(s).map_err(|e| format!("序列化会话元数据失败: {e}"))?;
    std::fs::write(&p, text).map_err(|e| format!("写 {} 失败: {e}", p.display()))
}

/// 从**文件**读会话元数据（锁文件与缓存目录共用这一种格式）
pub fn read_session_file(path: &Path) -> Option<Session> {
    let text = std::fs::read(path).ok()?;
    serde_json::from_slice(&text).ok()
}

/// 从解压目录读会话元数据；没有（旧版本留下的、或命令行留下的）⇒ `None`
pub fn read_session(dir: &Path) -> Option<Session> {
    read_session_file(&dir.join(SESSION_NAME))
}

/// 一个解压目录 + 它的会话元数据（启动时"要不要问用户"的输入）
#[derive(Clone, Debug)]
pub struct CacheDir {
    pub dir: PathBuf,
    pub session: Option<Session>,
    pub bytes: u64,
    /// 目录 mtime 距今多少秒（"多久以前动的"）
    pub age_secs: u64,
}

/// 扫出 `<root>` 下的**解压目录**（新→旧）。只认目录，于是锁文件之类的东西不会被卷进来。
pub fn cache_dirs(root: &Path) -> Vec<CacheDir> {
    let now = now_secs();
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<(std::time::SystemTime, CacheDir)> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let m = e.metadata().ok()?;
            if !m.is_dir() {
                return None;
            }
            let when = m.modified().ok()?;
            let age = when
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| now.saturating_sub(d.as_secs()))
                .unwrap_or(0);
            Some((
                when,
                CacheDir { dir: p.clone(), session: read_session(&p), bytes: dir_bytes(&p), age_secs: age },
            ))
        })
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0)); // 新的在前
    out.into_iter().map(|(_, c)| c).collect()
}

/// 删掉这些目录，返回（删了几个、释放多少字节）。**只删目录** —— 缓存根目录本身与锁文件都不动。
pub fn discard_dirs(dirs: &[PathBuf]) -> (usize, u64) {
    let mut n = 0usize;
    let mut freed = 0u64;
    for d in dirs {
        let bytes = dir_bytes(d);
        if std::fs::remove_dir_all(d).is_ok() {
            n += 1;
            freed += bytes;
        }
    }
    (n, freed)
}

/// 收资源：文档引用的 `meta.audio` / `meta.background` 若不在已有资源里，
/// 就从 `base_dir` 读进来（把"引用外部文件"变成"装进包里"）。
///
/// 读不到只**记警告**（用户可能把音乐放在别处了），不阻断保存。
pub fn collect_assets(
    doc: &Document,
    have: &[zip::Entry],
    base_dir: Option<&Path>,
    fid: &mut Fidelity,
) -> Vec<zip::Entry> {
    let mut out: Vec<zip::Entry> = have.to_vec();
    for (what, want) in [
        ("音乐", doc.meta.audio.as_deref()),
        ("曲绘/背景", doc.meta.background.as_deref()),
    ] {
        let Some(name) = want.map(str::trim).filter(|n| !n.is_empty()) else {
            continue;
        };
        if out.iter().any(|a| a.name == name) {
            continue; // 已经在包里（例如刚从容器载入）
        }
        let Some(base) = base_dir else {
            fid.warn(format!("{what} `{name}` 不在资源里，且不知道从哪个目录找 —— 容器将缺少它"));
            continue;
        };
        let p = if Path::new(name).is_absolute() {
            PathBuf::from(name)
        } else {
            base.join(name)
        };
        // 包内条目名用**文件名**：容器是自包含的，不该把宿主机的绝对路径带进去
        // （文档字段的同步改写由调用方负责，见 `EditCore::normalize_asset_names`）
        let entry_name = Path::new(name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(name)
            .to_owned();
        match std::fs::read(&p) {
            Ok(data) => {
                fid.note(format!(
                    "{what} `{name}`（{} KiB）已装入容器，包内名 `{entry_name}`",
                    data.len() / 1024
                ));
                out.push(zip::Entry { name: entry_name, data });
            }
            Err(e) => fid.warn(format!(
                "{what} `{name}` 读不到（{}: {e}）—— 容器将缺少它",
                p.display()
            )),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Beat, BpmEntry, JudgeLine, Meta, Note, NoteKind};

    fn doc_with_assets() -> Document {
        let mut doc = Document::fresh(
            Meta {
                name: "容器测试".to_owned(),
                composer: "作曲".to_owned(),
                charter: "谱师".to_owned(),
                illustrator: String::new(),
                difficulty: "IN".to_owned(),
                level: "15".to_owned(),
                constant: None,
                offset_ms: 12,
                audio: Some("song.ogg".to_owned()),
                background: Some("bg.png".to_owned()),
                foreign: Default::default(),
            },
            174.0,
        );
        doc.judge_lines = vec![JudgeLine::default()];
        doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 174.0, foreign: Default::default() }];
        doc.judge_lines[0].notes.push(Note::new(NoteKind::Tap, Beat::new(1, 1), 0.0));
        doc
    }

    /// 容器往返：谱面与资源都要完好回来
    #[test]
    fn container_roundtrip_keeps_chart_and_assets() {
        let doc = doc_with_assets();
        let assets = vec![
            zip::Entry { name: "song.ogg".to_owned(), data: vec![1u8, 2, 3, 4] },
            zip::Entry { name: "bg.png".to_owned(), data: b"\x89PNG".to_vec() },
            // 不认识的条目也要带过去（不许悄悄丢）
            zip::Entry { name: "extra/readme.txt".to_owned(), data: b"hello".to_vec() },
        ];
        let (bytes, backend) = write(&doc, &assets).unwrap();
        assert!(zip::looks_like_zip(&bytes));
        let entries = zip::read(&bytes).unwrap();
        assert!(
            entries.iter().any(|e| e.name == CHART_NAME),
            "谱面要在包里（条目顺序由打包器决定，只保证按名字能查到）：{:?}",
            entries.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        assert!(backend == zip::Backend::SevenZip || backend == zip::Backend::Builtin);

        let (c, fid) = read(&bytes).unwrap();
        assert_eq!(c.doc.meta.name, "容器测试");
        assert_eq!(c.doc.meta.audio.as_deref(), Some("song.ogg"));
        assert_eq!(c.doc.judge_lines[0].notes.len(), 1);
        assert_eq!(c.assets.len(), 3, "资源一条不少");
        assert_eq!(zip::get(&c.assets, "song.ogg").unwrap(), &[1u8, 2, 3, 4]);
        assert!(fid.is_lossless(), "{:?}", fid.warnings);
        // 文档里写的资源都在包里 ⇒ 报告应说清楚
        assert!(fid.conversions.iter().any(|c| c.contains("音乐 `song.ogg` 在容器内")));
    }

    /// 文档引用的资源不在包里 → **报警告**（不静默产出缺音乐的包）
    #[test]
    fn missing_asset_is_reported() {
        let doc = doc_with_assets();
        let (bytes, _backend) = write(&doc, &[]).unwrap();
        let (_c, fid) = read(&bytes).unwrap();
        assert!(!fid.is_lossless());
        assert!(fid.warnings.iter().any(|w| w.contains("音乐 `song.ogg` **不在容器内**")), "{:?}", fid.warnings);
        assert!(fid.warnings.iter().any(|w| w.contains("曲绘/背景")));
    }

    /// 没有谱面的包要**明确拒绝**并列出实际条目
    #[test]
    fn container_without_chart_is_rejected() {
        let bytes = zip::pack(&[("song.ogg".to_owned(), vec![0u8; 4])]);
        let err = read(&bytes).unwrap_err();
        assert!(err.contains("没有谱面"), "{err}");
        assert!(err.contains("song.ogg"), "错误信息要列出实际条目：{err}");
        // 裸 JSON 不是容器
        assert!(read(b"{\"format\":\"opm\"}").is_err());
    }

    /// 收集资源：文档引用外部文件时把它们装进包；找不到只警告
    #[test]
    fn collect_assets_pulls_files_from_disk() {
        let dir = std::env::temp_dir().join(format!("opm-container-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("song.ogg"), b"music-bytes").unwrap();
        let doc = doc_with_assets();
        let mut fid = Fidelity::new("opm", "容器".to_owned());
        let assets = collect_assets(&doc, &[], Some(&dir), &mut fid);
        assert_eq!(zip::get(&assets, "song.ogg").unwrap(), b"music-bytes");
        // 文档里写的是**绝对路径**，包内条目名只取文件名
        assert!(
            !assets.iter().any(|a| a.name.contains('/')),
            "包内不该出现带目录的条目名：{:?}",
            assets.iter().map(|a| &a.name).collect::<Vec<_>>()
        );
        assert!(fid.conversions.iter().any(|c| c.contains("已装入容器")));
        // 曲绘不存在 → 警告，但不 panic
        assert!(fid.warnings.iter().any(|w| w.contains("曲绘/背景")), "{:?}", fid.warnings);

        // 已经在包里的不再重复读盘
        let mut fid2 = Fidelity::new("opm", "容器".to_owned());
        let have = vec![zip::Entry { name: "song.ogg".to_owned(), data: b"in-package".to_vec() }];
        let again = collect_assets(&doc, &have, Some(&dir), &mut fid2);
        assert_eq!(zip::get(&again, "song.ogg").unwrap(), b"in-package", "包里的优先");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 资源摊到缓存：只取文件名（容器内的 `../` 不能写到缓存目录之外）
    ///
    /// 走的是真正摊缓存的那个入口（[`extract_container_into`]）—— 以前这里调的是一个
    /// 只有本测试在用的 `extract_assets`，于是"越界写"这条规则在两条路上各有一份，
    /// 而测试只盯着没人走的那一份。
    #[test]
    fn extraction_is_sandboxed_to_the_cache_dir() {
        let assets = vec![
            zip::Entry { name: "../../evil.txt".to_owned(), data: b"nope".to_vec() },
            zip::Entry { name: "assets/sub/song.ogg".to_owned(), data: b"ok".to_vec() },
        ];
        let dir = extract_dir("test-sandbox");
        let out = extract_container_into(&dir, &Document::default(), &assets).unwrap();
        for (_, p) in &out {
            assert!(p.starts_with(&dir), "落盘路径越界：{}", p.display());
        }
        assert!(dir.join("evil.txt").exists());
        assert!(dir.join("song.ogg").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    // 临时目录助手：实现搬到 `testkit`（`session` 那边也有一份一模一样的）
    use crate::testkit::tmp_dir as tmp;

    /// 解压目录 = `<临时目录>/opm/<key>`（用户指定：Linux `/tmp/opm`、Windows `%TEMP%\opm`）
    #[test]
    fn extract_dir_lives_under_temp_opm() {
        let base = std::env::temp_dir().join("opm");
        assert_eq!(cache_root(), base, "根目录必须是 <临时目录>/opm");
        assert_eq!(extract_dir("abc123"), base.join("abc123"));
        assert_eq!(extract_dir_in(Path::new("/x/y"), "k"), PathBuf::from("/x/y/k"));
        // 本机（Linux）就是 /tmp/opm；Windows 上 `temp_dir()` 自己会给 %TEMP%
        if cfg!(target_os = "linux") && std::env::var_os("TMPDIR").is_none() {
            assert_eq!(cache_root(), PathBuf::from("/tmp/opm"));
        }
    }

    /// 摊出来的是**一整套**：`opm.json`（谱面本体）+ 资源，且条目名只取文件名（不许越界写）
    #[test]
    fn extract_writes_the_chart_and_assets() {
        let root = tmp("extract");
        let dir = extract_dir_in(&root, "k1");
        let doc = Document::default();
        let assets = vec![
            zip::Entry { name: "song.ogg".to_owned(), data: b"OggS-x".to_vec() },
            zip::Entry { name: "../evil.bin".to_owned(), data: b"nope".to_vec() },
        ];
        let files = extract_container_into(&dir, &doc, &assets).unwrap();
        assert_eq!(files.len(), 3, "谱面 + 两个资源");
        assert!(dir.join("opm.json").is_file(), "谱面本体也要摊出来");
        assert!(dir.join("song.ogg").is_file());
        assert!(dir.join("evil.bin").is_file(), "只取文件名 ⇒ 不会写到目录之外");
        assert!(!root.join("evil.bin").exists(), "不许越界");
        std::fs::remove_dir_all(root).ok();
    }

    /// 缓存上限：超了删**最旧**的，且**不碰**最新那份（正在用的那份刚写过）
    #[test]
    fn prune_keeps_the_newest_and_respects_the_cap() {
        let root = tmp("prune");
        // 三个 1 KB 的目录，mtime 依次拉开
        for (i, age) in [(0, 3u64), (1, 2), (2, 1)] {
            let d = root.join(format!("k{i}"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("a.bin"), vec![0u8; 1024]).unwrap();
            let when = std::time::SystemTime::now() - std::time::Duration::from_secs(age * 3600);
            filetime_like(&d.join("a.bin"), when);
        }
        // 上限 2.5 KB ⇒ 只能留两份：最旧的 k0 该被删
        let (n, freed) = prune_cache(&root, 2560);
        assert_eq!(n, 1, "只该删一个");
        assert!(freed >= 1024);
        assert!(!root.join("k0").exists(), "删的是最旧的");
        assert!(root.join("k1").exists() && root.join("k2").exists());
        // 上限很大 ⇒ 什么都不删
        assert_eq!(prune_cache(&root, 10 * 1024 * 1024).0, 0);
        // 目录不存在 ⇒ 不 panic
        assert_eq!(prune_cache(&root.join("nope"), 1).0, 0);
        std::fs::remove_dir_all(root).ok();
    }

    /// 会话元数据：写进去能原样读回来；`read_session` 读不到就 `None`（旧目录/命令行目录）
    #[test]
    fn session_round_trips_and_survives_missing_files() {
        let root = tmp("session");
        let dir = extract_dir_in(&root, "k1");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read_session(&dir).is_none(), "没写过 ⇒ None（不是 panic）");
        let s = Session {
            pid: 4321,
            exe: "opm-app".to_owned(),
            source: Some("/charts/a.opm".to_owned()),
            format: "opm 容器".to_owned(),
            name: "朝色".to_owned(),
            started: 1000,
            snapshot: 2000,
            dirty: true,
        };
        write_session(&dir, &s).unwrap();
        assert_eq!(read_session(&dir).unwrap(), s);
        assert!(s.is_gui() && s.has_unsaved());
        assert_eq!(s.snapshot_age_secs(2600), Some(600));
        assert_eq!(Session { snapshot: 0, ..s.clone() }.snapshot_age_secs(2600), None);
        // 命令行留下的：出处不同 ⇒ 不该被当成"上次崩了"
        assert!(!Session { exe: "opm-ctl".to_owned(), ..s.clone() }.is_gui());
        assert!(!Session { exe: "lifecycle-1a2b".to_owned(), ..s }.is_gui());
        std::fs::remove_dir_all(root).ok();
    }

    /// 扫缓存：只认目录（锁文件之类不算）、新的在前、带得出会话元数据
    #[test]
    fn cache_dirs_scan_dirs_newest_first() {
        let root = tmp("scan");
        for (key, exe) in [("old", None), ("new", Some("opm-app"))] {
            let d = extract_dir_in(&root, key);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(CHART_NAME), b"{}").unwrap();
            if let Some(exe) = exe {
                write_session(&d, &Session { pid: 1, exe: exe.to_owned(), ..Default::default() }).unwrap();
            }
        }
        // 目录 mtime = "多久以前动过"。**不保证每个平台都设得动**（Windows 上目录不能这样当文件打开）
        // ⇒ 设不动就只查集合，不硬编一个平台相关的期望
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        let aged = std::fs::File::open(root.join("old")).and_then(|f| f.set_modified(when)).is_ok();
        std::fs::write(root.join("not-a-dir.txt"), b"x").unwrap();
        let got = cache_dirs(&root);
        assert_eq!(got.len(), 2, "只认目录：根目录里的普通文件不算缓存");
        assert!(got[0].dir.ends_with("new"), "新→旧：{}", got[0].dir.display());
        let old = got.iter().find(|c| c.dir.ends_with("old")).expect("旧目录还在");
        assert!(old.session.is_none(), "没有 session.json 的目录：出处未知");
        assert!(got[0].session.as_ref().unwrap().is_gui());
        assert!(got[0].bytes >= 2, "字节数要算得出来");
        if aged {
            assert!(old.age_secs >= 7000, "按 mtime 报年龄：{}", old.age_secs);
        }
        // 丢弃：删指定目录，根目录与别的文件都不动
        let (n, _) = discard_dirs(&[old.dir.clone()]);
        assert_eq!(n, 1);
        assert!(!old.dir.exists() && root.join("not-a-dir.txt").exists());
        assert_eq!(discard_dirs(&[root.join("nope")]).0, 0, "删不存在的不算数、也不 panic");
        std::fs::remove_dir_all(root).ok();
    }

    /// 把 mtime 往回拨（用 `filetime` 那种系统调用；这里直接用标准库的 set_modified，无需新依赖）
    fn filetime_like(p: &Path, when: std::time::SystemTime) {
        let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        let _ = f.set_modified(when);
    }
}
