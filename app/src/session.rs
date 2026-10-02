// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **每份谱面一把锁**（`<缓存目录>/lock.pid`）与「上次没退干净」的判定。
//!
//! 历史（2026-10-02 改）：原先是**全局单会话**（`<临时目录>/opm/.session.lock`），
//! 同一时刻只允许一个 OpenPhM。用户口径改成："支持不同进程读取不同谱面" ——
//! 于是锁落到**每份谱面自己的缓存目录**上：读 A 的进程与读 B 的进程互不相干，
//! 而**读同一份谱面**的两个进程仍然互斥（缓存目录是共享工作副本，两边都写就互相覆盖快照）。
//!
//! 崩溃判定（用户口径）：**只在打开谱面时**检查（不是启动时扫缓存根目录），
//! 判据是"**谱面文件夹存在** ∧ **pid 锁的持有者 ping 不通**"：
//!
//! - 先看 `lock.pid` 在不在 —— 不在就没人认领过这份缓存（`Free`）；
//! - 再用 [`std::fs::File::try_lock`]（Unix `flock` / Windows `LockFileEx`）问一句
//!   "锁还在手上吗"：**锁随句柄存在**，进程被杀时由内核释放，所以"锁没人持"与
//!   "那个进程已经不在"在绝大多数情况下是同一件事；
//! - 最后**ping** 一下那个 pid 的控制通道（[`crate::control::ping`]）：锁没人持但它还应答，
//!   说明它（或它的另一份缓存句柄）还在 ⇒ 也算占着。反过来，锁没人持、ping 也不通
//!   ⇒ 认定**崩溃遗留**，问用户要不要继续上次的编辑。
//!
//! **不做"pid 存活检测"**：pid 会被复用，`/proc/<pid>` 存在说明不了什么；
//! 锁 + ping 这两条都是"对方自己给的证据"。

use std::path::{Path, PathBuf};

use crate::codec::container::{self, CacheDir, Session};

/// 锁文件的名字（放在**该谱面的缓存目录里**，与 `opm.json` 同级）
pub const LOCK_NAME: &str = "lock.pid";

/// ping 的等待上限：判据要快（打开文件时同步做），而本机 socket 的往返在微秒级
pub const PING_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);

/// 一份缓存目录的锁实体（同一个进程里**再认领一次**时共享它）
struct LockEntry {
    /// 句柄必须活着 —— 锁跟着它走（`File` 一关，锁就没了）。
    /// 它**只被"活着"这件事用到**（读都不读），所以名字带下划线：别让读者以为它还有别的用途。
    _file: std::fs::File,
    dir: PathBuf,
}

/// **进程内**已认领的缓存目录（`dir → Weak<LockEntry>`）。
///
/// 为什么需要它：同一个进程里可以有多个 [`crate::core::EditCore`]（测试、CLI 批处理都这样），
/// 而 `flock` 是**按打开文件描述**算的 —— 已经拿着的锁再用一个新 fd 去 `try_lock` 会 `WouldBlock`，
/// 于是"自己占着自己的目录"会被判成"别人占着"。这里让第二次认领**共享同一个实体**
/// （`try_clone` 出的 fd 与原 fd 属于同一个打开文件描述，锁是同一把）。
///
/// `Weak` 而不是 `Arc`：最后一个 `ChartLock` 一掉，条目就没了（
/// 否则锁会一直挂在进程里，退出前再也不释放）。
fn registry() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Weak<LockEntry>>> {
    static R: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Weak<LockEntry>>>,
    > = std::sync::OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 拿到的**谱面缓存独占锁**。**持有它**就是"这份缓存归我写"；随 `Drop`（进程退出/被杀）由内核释放。
pub struct ChartLock {
    /// 共享的实体：同一个进程里的第二次认领拿的是同一份（见 [`registry`]）
    _entry: std::sync::Arc<LockEntry>,
    path: PathBuf,
    /// 是不是**这一次**调用真的去抢了内核锁（复用时为 false，只为日志/诊断）
    freshly_acquired: bool,
}

impl ChartLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// 这份锁看管的缓存目录（打开谱面时告诉用户"哪一份"）
    pub fn dir(&self) -> &Path {
        &self._entry.dir
    }
    /// 这一次是**新抢到**的，还是"本进程已经拿着、复用"的（诊断用）
    pub fn is_fresh(&self) -> bool {
        self.freshly_acquired
    }
    /// 这份缓存是不是**本进程**在写（`inspect` 用它区分"别人占着"与"我自己占着"）
    pub fn is_mine(dir: &Path) -> bool {
        registry()
            .lock()
            .map(|m| m.get(dir).is_some_and(|w| w.strong_count() > 0))
            .unwrap_or(false)
    }
}

/// 只打路径：`File` 的 Debug 会带上句柄与内部状态，对日志没意义
impl std::fmt::Debug for ChartLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChartLock")
            .field("dir", &self._entry.dir)
            .finish_non_exhaustive()
    }
}

/// 抢锁失败的原因
#[derive(Clone, Debug)]
pub enum Refused {
    /// 这份谱面已经有会话在跑。里面是它的身份（从锁文件读出来的）
    Busy(Session),
    /// 连锁文件都开不了（目录不可写之类）—— 这是**真错误**，不是"有人在用"。
    /// 保证不了独占就别动缓存：把别人的工作副本搅了比"打不开"严重得多。
    Io(String),
}

/// 锁文件路径
pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join(LOCK_NAME)
}

/// **抢这份谱面缓存的锁**。成功 ⇒ 本进程可以写它；失败 ⇒ 别碰（见 [`Refused`]）。
///
/// 成功后把自己的身份写进锁文件（pid/进程名/来源/时间）：① 第二个实例要靠它说清"谁在占着"；
/// ② 崩溃之后它就是"上次是谁、在为哪个谱面摊的这份缓存"——继续编辑对话框读的正是它。
pub fn acquire(dir: &Path, source: Option<&str>) -> Result<ChartLock, Refused> {
    std::fs::create_dir_all(dir)
        .map_err(|e| Refused::Io(format!("建缓存目录失败 {}: {e}", dir.display())))?;
    let path = lock_path(dir);
    // 本进程已经认领过这一份 ⇒ 复用（`flock` 认的是打开文件描述，再开一个 fd 去抢会被拒）
    if let Some(entry) = registry()
        .lock()
        .ok()
        .and_then(|m| m.get(dir).and_then(std::sync::Weak::upgrade))
    {
        return Ok(ChartLock { _entry: entry, path, freshly_acquired: false });
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| Refused::Io(format!("打开谱面锁失败 {}: {e}", path.display())))?;
    match file.try_lock() {
        Ok(()) => {
            let me = Session {
                pid: std::process::id(),
                exe: container::exe_name(),
                source: source.map(str::to_owned),
                started: container::now_secs(),
                ..Default::default()
            };
            let _ = write_holder(&file, &me); // 写失败不影响独占性（锁已经在手上）
            let entry = std::sync::Arc::new(LockEntry { _file: file, dir: dir.to_path_buf() });
            if let Ok(mut m) = registry().lock() {
                m.insert(dir.to_path_buf(), std::sync::Arc::downgrade(&entry));
            }
            Ok(ChartLock { _entry: entry, path, freshly_acquired: true })
        }
        Err(std::fs::TryLockError::WouldBlock) => Err(Refused::Busy(holder(dir).unwrap_or_default())),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(Refused::Io(format!("对 {} 加锁失败: {e}", path.display())))
        }
    }
}

/// 锁文件里那份身份（谁在占着）。读不到 ⇒ `None`。
pub fn holder(dir: &Path) -> Option<Session> {
    container::read_session_file(&lock_path(dir))
}

/// 把身份写进**已经打开的**锁文件（截断重写；锁句柄不换，锁不会掉）
fn write_holder(file: &std::fs::File, s: &Session) -> Result<(), String> {
    use std::io::{Seek, SeekFrom, Write};
    let text = serde_json::to_vec_pretty(s).map_err(|e| format!("序列化失败: {e}"))?;
    let mut f = file;
    f.set_len(0).map_err(|e| format!("截断锁文件失败: {e}"))?;
    f.seek(SeekFrom::Start(0)).map_err(|e| format!("定位失败: {e}"))?;
    f.write_all(&text).map_err(|e| format!("写锁文件失败: {e}"))?;
    f.flush().map_err(|e| format!("刷新锁文件失败: {e}"))
}

/// 那把锁现在**被持有**吗（修剪缓存时也要问：**不许删别人正拿着的目录**）（内核事实：`try_lock` 拿不到就是有人拿着）。
///
/// 注意"拿得到"这一支会**立刻放掉**（文件句柄在这里析构）—— 这个函数只问一句，不占锁。
pub(crate) fn lock_is_held(dir: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(lock_path(dir)) else {
        return false; // 文件没了/开不了：没人持
    };
    match file.try_lock() {
        Ok(()) => false, // 拿到了 ⇒ 没人持（句柄随后析构，锁随之释放）
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

/// 那个持有者还**应答**吗（连它的控制通道问一句 ping，核对 pid）
fn holder_answers(h: &Session) -> bool {
    if h.pid == 0 || h.pid == std::process::id() {
        return false; // 没有 pid 可问；自己问自己没有意义（锁没持有时这种情况说明状态不对）
    }
    ping_at(&crate::control::path_for_pid(h.pid), h.pid)
}

/// 在**给定路径**上 ping 指定 pid（`holder_answers` 的可测版本：socket 路径由调用方给，
/// 于是测试不必去改 `XDG_RUNTIME_DIR` 那种进程级全局状态）
fn ping_at(path: &Path, pid: u32) -> bool {
    match crate::control::ping(path, PING_TIMEOUT) {
        Ok(v) => v
            .get("result")
            .and_then(|r| r.get("pid"))
            .and_then(|p| p.as_u64())
            .is_some_and(|got| got == pid as u64),
        Err(_) => false,
    }
}

/// **打开一份谱面之前**：这份缓存目录的处境
#[derive(Clone, Debug, PartialEq)]
pub enum CacheState {
    /// 目录不存在 / 没有 `lock.pid` / 旧版本留下但没有未保存改动 ⇒ 没有别人，也没有可恢复的东西
    Free,
    /// 有活着的会话占着（锁被持有，或那个 pid 还应答）
    Busy {
        holder: Session,
        /// 占着它的是**本进程自己**（同一个进程里可以有多个 `EditCore`：测试与批处理都这样）
        mine: bool,
    },
    /// 有 pid 锁、但**锁没人持且那个 pid ping 不通** ⇒ 崩溃遗留（用户口径的判据）
    Crashed(Session),
}

impl CacheState {
    pub fn is_free(&self) -> bool {
        matches!(self, CacheState::Free)
    }
    pub fn holder(&self) -> Option<&Session> {
        match self {
            CacheState::Free => None,
            CacheState::Busy { holder, .. } => Some(holder),
            CacheState::Crashed(h) => Some(h),
        }
    }
}

/// 判一份缓存目录的处境（**纯查询**，不写任何东西；见 [`CacheState`] 与模块头）。
///
/// 旧版本留下的目录（有 `session.json`、没有 `lock.pid`）也认：
/// GUI 会话 + **未保存改动**时算 `Crashed`（否则那些改动会在下次打开时被静默覆盖）；
/// 其余一律 `Free`。
pub fn inspect(dir: &Path) -> CacheState {
    match holder(dir) {
        Some(h) => {
            // "占着"的两条证据：内核锁还在手上（`flock`），或者那个 pid 还应答 ping。
            // `mine` 只说明"占用者是自己" —— 同一个进程里可以有多个 `EditCore`，调用方据此
            // 区分"接管自己的"与"别人正开着"（见 `core::claim_blocker`）。
            let mine = ChartLock::is_mine(dir) && h.pid == std::process::id();
            if lock_is_held(dir) || holder_answers(&h) {
                CacheState::Busy { holder: h, mine }
            } else {
                CacheState::Crashed(h)
            }
        }
        None => {
            // 没有 pid 锁的旧缓存：只在"GUI 留下 + 有未保存改动"时当成崩溃遗留
            match container::read_session(dir) {
                Some(s) if s.is_gui() && s.has_unsaved() => CacheState::Crashed(s),
                _ => CacheState::Free,
            }
        }
    }
}

// ---------------------------------------------------------------- 遗留缓存

/// **某一份缓存目录**的处境快照（打开谱面时"要不要问用户"用的就是它）。
///
/// 与 [`gui_leftovers`]（扫根目录，用于"启动时把所有遗留列出来"那条旧路）不同：
/// 它只回答**这一份**，而且不需要目录里有 `session.json` —— 判据是 pid 锁。
pub fn leftover_of(dir: &Path) -> Leftover {
    match container::cache_dir_of(dir) {
        Some(c) => Leftover::of(c),
        None => Leftover {
            dir: dir.to_path_buf(),
            session: holder(dir).or_else(|| container::read_session(dir)),
            bytes: container::dir_bytes(dir),
            age_secs: 0,
        },
    }
}

/// 缓存根目录里**还躺着**的一份解压缓存
#[derive(Clone, Debug)]
pub struct Leftover {
    pub dir: PathBuf,
    /// 出处（没有 = 旧版本或非本程序流程留下的，判定不了就别问用户）
    pub session: Option<Session>,
    pub bytes: u64,
    /// 目录 mtime 距今多少秒
    pub age_secs: u64,
}

impl Leftover {
    fn of(c: CacheDir) -> Self {
        Self { dir: c.dir, session: c.session, bytes: c.bytes, age_secs: c.age_secs }
    }

    /// 是 GUI 会话留下的吗（`opm-ctl` 留下的按设计就不清理，不该拿来问"上次是不是崩了"）
    pub fn is_gui(&self) -> bool {
        self.session.as_ref().is_some_and(Session::is_gui)
    }

    /// 谱面名（元数据里没有就退回目录名——总得有个能叫的名字）
    pub fn name(&self) -> String {
        let n = self.session.as_ref().map(|s| s.name.trim().to_owned()).unwrap_or_default();
        if !n.is_empty() {
            return n;
        }
        self.dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "（未命名）".to_owned())
    }

    /// 原始谱面文件（保存目标）
    pub fn source_path(&self) -> Option<PathBuf> {
        self.session
            .as_ref()
            .and_then(|s| s.source.clone())
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
    }

    /// 原始文件还在不在（不在了要说清楚：继续之后保存会**重新创建**它）
    pub fn source_exists(&self) -> bool {
        self.source_path().is_some_and(|p| p.is_file())
    }

    /// 缓存里有没有磁盘上没有的改动
    pub fn has_unsaved(&self) -> bool {
        self.session.as_ref().is_some_and(Session::has_unsaved)
    }

    /// 一行抬头：`「谱面名」· 12.3 MB · 2 小时前`
    pub fn headline(&self) -> String {
        format!("「{}」· {} · {}", self.name(), size_text(self.bytes), age_text(self.age_secs))
    }

    /// 对话框正文（一条一行）。**每一句都是可核对的事实**：路径、大小、时间、有没有未保存改动。
    pub fn details(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self.source_path() {
            Some(p) => out.push(format!(
                "原始文件：{}（{}）",
                p.display(),
                if self.source_exists() { "还在" } else { "已经不在了：继续之后保存会重新创建它" }
            )),
            None => out.push("这份谱面还没有保存目标（新建后没落过盘）".to_owned()),
        }
        out.push(format!("缓存目录：{}", self.dir.display()));
        let now = container::now_secs();
        let snap = self.session.as_ref().and_then(|s| s.snapshot_age_secs(now));
        if self.has_unsaved() {
            // 纯文本：egui 的 label 不渲染 Markdown，写 `**粗体**` 只会把星号画出来
            out.push(format!(
                "缓存里有未保存的改动（快照于{}）—— 继续编辑会把它们带回来",
                age_text(snap.unwrap_or(0))
            ));
        } else if let Some(age) = snap {
            out.push(format!("缓存与磁盘上的文件一致（上次快照于{}）", age_text(age)));
        } else {
            out.push("缓存里就是打开容器时摊出来的那份内容，没有未保存的改动".to_owned());
        }
        out
    }
}

// 注：这里曾有 `gui_leftovers` / `unidentified`（扫缓存根目录，列出所有那次没退干净的会话）——
// 用户口径改成"崩溃检查放在打开谱面那一刻"之后，那条路没有了：现在只查**正在打开的那一份**
// （[`inspect`] + [`leftover_of`]）。留着扫全根目录的 API 只会让人再写一次启动期检查。

/// 丢掉这些遗留缓存（**只删目录**，锁文件与根目录不动）
pub fn discard(items: &[Leftover]) -> (usize, u64) {
    let dirs: Vec<PathBuf> = items.iter().map(|l| l.dir.clone()).collect();
    container::discard_dirs(&dirs)
}

/// 人话的"多久以前"（对话框里用；越久越粗）
pub fn age_text(secs: u64) -> String {
    match secs {
        0..=9 => "刚刚".to_owned(),
        10..=59 => format!("{secs} 秒前"),
        60..=3599 => format!("{} 分钟前", secs / 60),
        3600..=86399 => format!("{} 小时前", secs / 3600),
        _ => format!("{} 天前", secs / 86400),
    }
}

/// 人话的字节数
pub fn size_text(bytes: u64) -> String {
    const K: u64 = 1024;
    const M: u64 = 1024 * K;
    const G: u64 = 1024 * M;
    match bytes {
        0..=1023 => format!("{bytes} B"),
        _ if bytes < M => format!("{:.1} KiB", bytes as f64 / K as f64),
        _ if bytes < G => format!("{:.1} MB", bytes as f64 / M as f64),
        _ => format!("{:.1} GB", bytes as f64 / G as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 临时目录助手：实现搬到 `testkit`（`codec::container` 那边也有一份一模一样的）
    use crate::testkit::tmp_dir as tmp;
    use container::extract_dir_in;

    /// **同一份谱面、同一进程**：再认领一次是**复用**（不是第二次抢锁 —— `flock` 认打开文件描述，
    /// 自己再抢一次会被拒，于是"自己占着自己的目录"会被误判成"别人占着"）。
    #[test]
    fn re_claiming_the_same_chart_in_one_process_is_reuse() {
        let root = tmp("lock");
        let dir = extract_dir_in(&root, "chart-a");
        let first = acquire(&dir, Some("/charts/a.opm")).expect("第一次");
        assert!(first.is_fresh(), "第一次是真抢到");
        let second = acquire(&dir, Some("/charts/a.opm")).expect("同进程再认领要复用");
        assert!(!second.is_fresh(), "第二次是复用");
        assert!(ChartLock::is_mine(&dir), "这份缓存是本进程在写");
        assert_eq!(first.path(), lock_path(&dir));
        assert_eq!(first.dir(), dir);
        drop(second);
        assert!(ChartLock::is_mine(&dir), "还留着一个句柄 ⇒ 仍然算我的");
        drop(first);
        assert!(!ChartLock::is_mine(&dir), "全放掉之后就不是我的了");
        std::fs::remove_dir_all(root).ok();
    }

    /// **同一个进程之外**：锁被持有、而锁文件里写的是别的 pid ⇒ `inspect` 判 Busy
    /// （`EditCore::stage_into_cache` 就是靠它拒绝"两个人写同一份缓存"的）。
    #[test]
    fn a_chart_held_by_another_pid_reads_as_busy() {
        let root = tmp("lock-other");
        let dir = extract_dir_in(&root, "chart-a");
        let lock = acquire(&dir, Some("/charts/a.opm")).expect("拿到锁");
        // 把身份改成"别的进程"（锁本身还在我们手上 —— 模拟"另一个进程正拿着它"）
        std::fs::write(
            lock_path(&dir),
            serde_json::to_vec(&Session {
                pid: std::process::id().wrapping_add(1),
                exe: "opm-app".to_owned(),
                source: Some("/charts/a.opm".to_owned()),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        match inspect(&dir) {
            CacheState::Busy { holder, mine } => {
                assert_eq!(holder.pid, std::process::id().wrapping_add(1));
                assert!(!mine, "不是自己占的");
            }
            other => panic!("别人拿着锁 ⇒ Busy，得到 {other:?}"),
        }
        drop(lock);
        std::fs::remove_dir_all(root).ok();
    }

    /// **不同谱面互不影响**（用户口径 2026-10-02："支持不同进程读取不同谱面"）——
    /// 每份谱面一个缓存目录、一把锁，两个会话可以同时开着各自那份
    #[test]
    fn different_charts_do_not_block_each_other() {
        let root = tmp("lock-many");
        let a = extract_dir_in(&root, "chart-a");
        let b = extract_dir_in(&root, "chart-b");
        let la = acquire(&a, Some("/charts/a.opm")).expect("A 的锁");
        let lb = acquire(&b, Some("/charts/b.opm")).expect("B 的锁");
        assert_ne!(la.path(), lb.path());
        assert!(matches!(inspect(&a), CacheState::Busy { .. }), "A 被自己占着");
        assert!(matches!(inspect(&b), CacheState::Busy { .. }), "B 被自己占着");
        drop(la);
        std::fs::remove_dir_all(root).ok();
    }

    /// `inspect` 的三种判决：没有锁文件 = Free；锁被持有 = Busy；
    /// **有 pid 锁、锁没人持、那个 pid 也 ping 不通 = Crashed**（用户口径的判据）
    #[test]
    fn inspect_verdicts_follow_the_lock_and_the_ping() {
        let root = tmp("inspect");
        let free = extract_dir_in(&root, "free");
        std::fs::create_dir_all(&free).unwrap();
        assert_eq!(inspect(&free), CacheState::Free, "没有锁文件 ⇒ Free");

        // 别人拿着锁 ⇒ Busy
        let held = extract_dir_in(&root, "held");
        let lock = acquire(&held, Some("/charts/held.opm")).unwrap();
        match inspect(&held) {
            CacheState::Busy { mine, .. } => assert!(mine, "把自己占着的认成自己"),
            other => panic!("锁被持有 ⇒ Busy，得到 {other:?}"),
        }
        drop(lock);

        // 锁没人持、pid 又不可能存在 ⇒ Crashed（这就是"崩溃遗留"）
        let dead = extract_dir_in(&root, "dead");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::write(
            lock_path(&dead),
            serde_json::to_vec(&Session {
                pid: 4_000_000_000,
                exe: "opm-app".to_owned(),
                started: container::now_secs().saturating_sub(600),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        match inspect(&dead) {
            CacheState::Crashed(h) => assert_eq!(h.pid, 4_000_000_000),
            other => panic!("锁没人持 + ping 不通 ⇒ 崩溃遗留，得到 {other:?}"),
        }

        // 旧版本留下的目录（有 session.json、没有 lock.pid）：GUI + 未保存改动才算崩溃遗留
        let legacy = extract_dir_in(&root, "legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        let s = Session {
            pid: 4242,
            exe: "opm-app".to_owned(),
            name: "旧缓存".to_owned(),
            dirty: true,
            ..Default::default()
        };
        container::write_session(&legacy, &s).unwrap();
        assert!(matches!(inspect(&legacy), CacheState::Crashed(_)), "旧缓存 + 脏 ⇒ 也算遗留");
        container::write_session(&legacy, &Session { dirty: false, ..s }).unwrap();
        assert_eq!(inspect(&legacy), CacheState::Free, "旧缓存 + 干净 ⇒ 不值得问");
        std::fs::remove_dir_all(root).ok();
    }

    /// **锁没人持但那个 pid 还应答 ⇒ 仍然算在用**（ping 这一道的意义：
    /// 锁可能因为"目录被重建/换过 inode"而看起来没人持，而进程其实还开着那份谱面）。
    ///
    /// 这条同时把 `control::ping` 端到端跑了一遍（起一个假的控制通道，连上去问 ping）。
    #[cfg(unix)]
    #[test]
    fn a_holder_that_still_answers_ping_counts_as_alive() {
        use std::io::{BufRead, BufReader, Write};
        let root = tmp("ping");
        let sock = root.join("fake.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).expect("绑一个假控制通道");
        let me = std::process::id();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = writeln!(stream, "{{\"ok\":true,\"op\":\"hello\",\"result\":{{\"pid\":{me}}}}}");
                let _ = stream.flush();
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                if reader.read_line(&mut line).is_ok() {
                    let _ = writeln!(
                        stream,
                        "{{\"ok\":true,\"op\":\"ping\",\"result\":{{\"pong\":true,\"pid\":{me}}}}}"
                    );
                    let _ = stream.flush();
                }
            }
        });
        assert!(ping_at(&sock, me), "假控制通道应答了 ping ⇒ 算活着");
        assert!(!ping_at(&sock, me + 1), "应答里的 pid 对不上 ⇒ 不算");
        // 没有监听者的路径：连不上 ⇒ 不通（判"崩溃"的那一半）
        assert!(!ping_at(&root.join("nobody.sock"), me));
        let _ = server.join();
        // `inspect` 在"锁没人持 + ping 不通"时给 Crashed —— 与上面那条测试合起来就是完整判据
        let dir = extract_dir_in(&root, "d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            lock_path(&dir),
            serde_json::to_vec(&Session { pid: 4_000_000_000, ..Default::default() }).unwrap(),
        )
        .unwrap();
        assert!(matches!(inspect(&dir), CacheState::Crashed(_)));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn age_and_size_are_human() {
        assert_eq!(age_text(0), "刚刚");
        assert_eq!(age_text(59), "59 秒前");
        assert_eq!(age_text(60), "1 分钟前");
        assert_eq!(age_text(7200), "2 小时前");
        assert_eq!(age_text(3 * 86400), "3 天前");
        assert_eq!(size_text(0), "0 B");
        assert_eq!(size_text(1536), "1.5 KiB");
        assert_eq!(size_text(12 * 1024 * 1024), "12.0 MB");
        assert_eq!(size_text(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
