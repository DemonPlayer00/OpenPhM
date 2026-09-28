//! **单会话**（同一时刻只允许一个 OpenPhM）与「上次没退干净」的判定。
//!
//! 两件事是同一个问题的两面：解压缓存目录是**进程独占**的（一个进程至多留一份，退出即清），
//! 所以 ①两个会话同时跑会互相删对方的缓存（`prune_cache` 与退出清理都会动手），
//! ②一个**没人清理**的缓存目录 = 上一个进程被强杀或崩溃留下的。
//!
//! 判据全部来自**文件系统**，不写 pid 文件里那种"谁死谁活"的猜测：
//!
//! - **独占**用 [`std::fs::File::try_lock`]（Unix `flock` / Windows `LockFileEx`）：
//!   锁随**句柄**存在，进程被杀时由内核释放 —— 于是"锁没人拿"与"进程已经不在"是同一件事，
//!   不需要 pid 存活检测，也不会有"上次崩溃留下一个假的 pid 文件"这种自欺。
//! - **出处**用每个缓存目录里的 `session.json`（见 [`crate::codec::container::Session`]）：
//!   只有进程名是 `opm-app` 的才算"GUI 上次没退干净"；`opm-ctl` 的缓存按设计不清理。
//!
//! 于是启动顺序是：**先抢锁**（抢不到 = 已经有会话在跑，本实例什么都不碰就退出）→
//! 再扫缓存根目录（还躺着的 GUI 目录 = 崩溃遗留，问用户要不要继续）。

use std::path::{Path, PathBuf};

use crate::codec::container::{self, CacheDir, Session};

/// 锁文件的名字（放在缓存根目录`<临时目录>/opm`里；**不是目录**，所以扫描缓存时不会被当成一份谱面）
pub const LOCK_NAME: &str = ".session.lock";

/// 拿到的独占会话锁。**持有它**就是"我是唯一的会话"；随 `Drop`（进程退出/被杀）由内核释放。
pub struct Lock {
    /// 句柄必须活着 —— 锁跟着它走（`File` 一关，锁就没了）
    _file: std::fs::File,
    path: PathBuf,
}

impl Lock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 只打路径：`File` 的 Debug 会带上句柄与内部状态，对日志没意义
impl std::fmt::Debug for Lock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lock").field("path", &self.path).finish_non_exhaustive()
    }
}

/// 抢锁失败的原因
#[derive(Clone, Debug)]
pub enum Refused {
    /// 已经有会话在跑。里面是它的身份（从锁文件读出来的；读不到就是 `None`）
    Busy(Option<Session>),
    /// 连锁文件都开不了（临时目录不可写之类）—— 这是**真错误**，不是"有人在用"。
    /// 按"只允许一个会话"的契约，这时候也**不该继续**：保证不了独占，就别动缓存。
    Io(String),
}

/// 锁文件路径
pub fn lock_path(root: &Path) -> PathBuf {
    root.join(LOCK_NAME)
}

/// **抢会话锁**。成功 ⇒ 本进程是唯一的会话；失败 ⇒ 别碰缓存（见 [`Refused`]）。
///
/// 成功后把自己的身份写进锁文件：第二个实例要靠它说清"谁在占着"（只给人看，不参与判定）。
pub fn acquire(root: &Path) -> Result<Lock, Refused> {
    std::fs::create_dir_all(root)
        .map_err(|e| Refused::Io(format!("建缓存根目录失败 {}: {e}", root.display())))?;
    let path = lock_path(root);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| Refused::Io(format!("打开会话锁失败 {}: {e}", path.display())))?;
    match file.try_lock() {
        Ok(()) => {
            let me = Session {
                pid: std::process::id(),
                exe: container::exe_name(),
                started: container::now_secs(),
                ..Default::default()
            };
            let _ = write_holder(&file, &me); // 写失败不影响独占性（锁已经在手上）
            Ok(Lock { _file: file, path })
        }
        Err(std::fs::TryLockError::WouldBlock) => Err(Refused::Busy(holder(root))),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(Refused::Io(format!("对 {} 加锁失败: {e}", path.display())))
        }
    }
}

/// 锁文件里那份身份（谁在占着）。读不到 ⇒ `None`。
pub fn holder(root: &Path) -> Option<Session> {
    container::read_session_file(&lock_path(root))
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

// ---------------------------------------------------------------- 遗留缓存

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

/// GUI 会话留下的缓存（**启动时该问用户的那些**），新→旧
pub fn gui_leftovers(root: &Path) -> Vec<Leftover> {
    container::cache_dirs(root)
        .into_iter()
        .map(Leftover::of)
        .filter(Leftover::is_gui)
        .collect()
}

/// 出处不明的缓存目录（旧版本、命令行、或摊到一半就被杀）：**只在日志里报一句**，不打断用户
pub fn unidentified(root: &Path) -> Vec<Leftover> {
    container::cache_dirs(root)
        .into_iter()
        .map(Leftover::of)
        .filter(|l| l.session.is_none())
        .collect()
}

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

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("opm-session-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 缓存目录（带一份会话元数据）；`exe` 决定它算谁的
    fn cache_dir(root: &Path, key: &str, exe: &str, dirty: bool) -> PathBuf {
        let d = container::extract_dir_in(root, key);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(container::CHART_NAME), b"{}").unwrap();
        container::write_session(
            &d,
            &Session {
                pid: 4242,
                exe: exe.to_owned(),
                source: Some("/charts/a.opm".to_owned()),
                format: "opm 容器".to_owned(),
                name: "朝色の紙飛行機".to_owned(),
                started: container::now_secs().saturating_sub(120),
                snapshot: container::now_secs().saturating_sub(30),
                dirty,
            },
        )
        .unwrap();
        d
    }

    /// **同一个时刻只能有一个会话**：第二个 `acquire` 被拒，并说得出是谁在占着
    #[test]
    fn the_second_session_is_refused() {
        let root = tmp("lock");
        let first = acquire(&root).expect("第一个会话该拿到锁");
        assert_eq!(first.path(), lock_path(&root));
        match acquire(&root) {
            Err(Refused::Busy(Some(who))) => {
                assert_eq!(who.pid, std::process::id(), "锁文件里写的是持有者自己的身份");
                assert!(!who.exe.is_empty(), "锁文件里该写下进程名（第二个实例要靠它说清是谁）");
            }
            other => panic!("第二个会话必须被拒：{other:?}"),
        }
        // 释放之后又能拿到（**锁随句柄**：被强杀的进程也一样会被内核放掉）
        //
        // 注意：`drop` 之后**不一定立刻**能拿到。`flock` 属于**打开文件描述**，而进程 fork 出来的
        // 子进程（同一进程里并行的别的用例正起着 7z / `sh`）在 exec 之前短暂持有同一份描述的副本
        // ⇒ 锁要等那个子进程 exec（fd 带 CLOEXEC，随即关掉）才真的放开。实测：147 个用例并行跑时
        // 大约每十几次复现一次 `Busy`。所以这里重试到 1 秒 —— 断言的仍是"会放开"，不是"立刻放开"。
        drop(first);
        let mut again = acquire(&root);
        for _ in 0..20 {
            if again.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            again = acquire(&root);
        }
        assert!(again.is_ok(), "释放之后该能重新拿到：{again:?}");
        std::fs::remove_dir_all(root).ok();
    }

    /// 遗留缓存：只挑 **GUI** 留下的那份；命令行/无出处的都不问用户
    #[test]
    fn only_gui_leftovers_are_offered() {
        let root = tmp("leftover");
        let gui = cache_dir(&root, "aaa", "opm-app", true);
        cache_dir(&root, "bbb", "opm-ctl", false);
        std::fs::create_dir_all(root.join("ccc")).unwrap(); // 无 session.json：出处不明
        std::fs::write(root.join(LOCK_NAME), b"{}").unwrap(); // 锁文件不是缓存

        let offered = gui_leftovers(&root);
        assert_eq!(offered.len(), 1, "只问 GUI 留下的");
        let l = &offered[0];
        assert_eq!(l.dir, gui);
        assert_eq!(l.name(), "朝色の紙飛行機");
        assert!(!l.source_exists(), "路径不存在：那是测试编的");
        assert!(l.headline().contains("朝色"), "抬头要点出是哪份谱面：{}", l.headline());
        let text = l.details().join("\n");
        assert!(text.contains("未保存的改动"), "有未保存改动必须说清楚：{text}");
        assert!(text.contains("已经不在了"), "原文件不在也要说清楚：{text}");
        assert_eq!(unidentified(&root).len(), 1, "无出处的只在日志里报一句");
        assert_eq!(discard(&offered).0, 1);
        assert!(!gui.exists() && root.join("bbb").exists(), "只删被丢弃的那份");
        assert!(root.join(LOCK_NAME).exists(), "锁文件不是缓存，别删");
        std::fs::remove_dir_all(root).ok();
    }

    /// 没有未保存改动时，正文不许说成"有改动"（说反了会让人不敢丢弃）
    #[test]
    fn details_do_not_claim_unsaved_changes() {
        let root = tmp("clean");
        cache_dir(&root, "aaa", "opm-app", false);
        let l = &gui_leftovers(&root)[0];
        let text = l.details().join("\n");
        assert!(!text.contains("未保存的改动"), "{text}");
        assert!(text.contains("一致"), "{text}");
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
