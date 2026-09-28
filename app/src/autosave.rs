// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **解压缓存快照的后台写手**（把"整份文档 → JSON → 落盘"从 GUI 帧里搬走）。
//!
//! ## 为什么需要它
//!
//! `App::maybe_snapshot` 每 2 秒把整份文档写回解压缓存，用来兜"被强杀时一个字节都不剩"。
//! 但它曾经是**在 GUI 帧里**做的，而这件事的代价与谱面规模成正比 —— 实测
//! （`cargo test --release --test snapshot_bench -- --ignored --nocapture`，50 000 音符）：
//!
//! | 段 | 代价（五次取样里的区间） |
//! |---|---|
//! | `Document::clone` | 2.1 ~ 4.5 ms |
//! | `to_json`（建 `serde_json::Value` 树） | 75 ~ 106 ms |
//! | `to_vec_pretty`（15.9 MB 输出） | 57 ~ 111 ms |
//! | `write` + `rename` | 4.3 ~ 5.6 ms |
//! | **合计** | **136 ~ 223 ms** |
//!
//! 于是**每 2 秒卡一下 ~250 ms**（逐帧流水 `OPM_FRAME_LOG` 量到 30 次/60 秒，`ui_ms` 250~275 ms
//! —— 比上面那张表的分项合计还大，因为它在帧里还要抢锁、还要写会话元数据；
//! 而且播放头是墙钟驱动的 ⇒ 画面还会**跳掉 0.28 秒**）。
//! 不只是播放：拖动、滚动、任何编辑都一样会撞上它 —— 它跟"你在干什么"无关，只跟"到点了吗"有关。
//!
//! ## 现在的分工
//!
//! · **GUI 帧**：只做 [`EditCore::snapshot_job`]（在锁内**克隆**文档并定下会话元数据，2~4.5 ms），
//!   然后把这份任务投给这里的线程。上一份还没写完就**跳过这一拍**（`try_send` 返回 `false`）——
//!   快照是"隔两秒的最新副本"，不是必须每拍都写。
//! · **本线程**：序列化 + 落盘（仍走"先写 `.tmp` 再 `rename`"的原子替换，见 `write_snapshot`）。
//!
//! 顺序上只有一处需要小心：**保存前**要先 [`Autosave::flush`] —— 否则一份"保存之前"的旧任务
//! 可能在保存之后落盘，把缓存里的文档退回旧内容（见 `App::save_doc` 的注释）。
//! 退出前同样要 flush（`Drop` 会做）：缓存目录是 `main` 在 `run_native` 返回后删的，
//! 后台还在写就会把刚删掉的目录又建出来 —— 下次启动会多问一次"上次没有正常退出"。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::codec;
use crate::codec::container::Session;
use crate::doc::Document;

/// 一份**已经定下来**的快照任务：文档副本 + 目标目录 + 会话元数据。
///
/// "已经定下来"是要紧的：`session` 里的 `snapshot` 时刻与 `dirty` 位取自**克隆那一刻**，
/// 序列化时文档可能已经被继续编辑了 —— 那份改动属于下一拍，不该写进这一份的元数据。
#[derive(Debug)]
pub struct SnapshotJob {
    pub dir: PathBuf,
    pub doc: Document,
    pub session: Session,
}

/// 把一份快照写到 `<dir>/opm.json`（+ 刷新 `session.json`）。**纯 IO，可在任何线程上跑。**
///
/// 与 `EditCore::snapshot_session` 是同一条路（同一份 `to_json` + `to_vec_pretty` + 原子改名），
/// 两条路的产物**逐字节相同**这件事有回归测试盯着（`tests/snapshot_bench.rs`）——
/// 一个"同步写"和一个"后台写"各写一份实现，就是下次"缓存里的文档怎么和刚才不一样"的来源。
pub fn write_snapshot(job: &SnapshotJob) -> Result<(), String> {
    let chart =
        serde_json::to_vec_pretty(&job.doc.to_json()).map_err(|e| format!("序列化失败: {e}"))?;
    // 先写临时文件再改名：强杀可能正好发生在写的中途，半截 JSON 比旧快照更糟
    let tmp = job.dir.join(format!("{}.tmp", codec::container::CHART_NAME));
    std::fs::write(&tmp, &chart).map_err(|e| format!("写 {} 失败: {e}", tmp.display()))?;
    std::fs::rename(&tmp, job.dir.join(codec::container::CHART_NAME))
        .map_err(|e| format!("替换 {} 失败: {e}", job.dir.display()))?;
    codec::container::write_session(&job.dir, &job.session)
}

enum Msg {
    Write(SnapshotJob),
    Stop,
}

/// 后台快照线程的把手。
pub struct Autosave {
    tx: Option<Sender<Msg>>,
    handle: Option<JoinHandle<()>>,
    /// 手上那份写完了没有（`false` = 空闲）。GUI 每拍只看这一个原子量，不抢锁。
    busy: Arc<AtomicBool>,
    /// 写完的份数（诊断用：与逐帧流水里的 `snapshots` 对照，能看出"投出去几份、落地几份"）
    written: Arc<AtomicU64>,
    /// 最近一次失败的原因（GUI 每帧 `try_recv` 式的回捞，只报一次变化的理由）
    err: Arc<Mutex<Option<String>>>,
}

impl Autosave {
    /// 起一条后台线程。线程名固定，`ps -L` / 调试器里认得出。
    pub fn spawn() -> Self {
        let (tx, rx): (Sender<Msg>, Receiver<Msg>) = mpsc::channel();
        let busy = Arc::new(AtomicBool::new(false));
        let written = Arc::new(AtomicU64::new(0));
        let err = Arc::new(Mutex::new(None));
        let (b, w, e) = (busy.clone(), written.clone(), err.clone());
        let handle = std::thread::Builder::new()
            .name("opm-snapshot".to_owned())
            .spawn(move || worker(rx, b, w, e))
            .ok();
        Self { tx: Some(tx), handle, busy, written, err }
    }

    /// 手上那份写完没有。
    pub fn idle(&self) -> bool {
        !self.busy.load(Ordering::Acquire)
    }

    /// 已经落地的快照份数。
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// 投一份任务。**上一份还没写完 ⇒ 返回 `false`**（这一拍跳过，调用方别把节流时刻推进）。
    pub fn try_send(&mut self, job: SnapshotJob) -> bool {
        let Some(tx) = self.tx.as_ref() else { return false };
        if self.busy.swap(true, Ordering::AcqRel) {
            return false; // 还在写上一份：这一拍不要了
        }
        if tx.send(Msg::Write(job)).is_err() {
            self.busy.store(false, Ordering::Release); // 线程没了 ⇒ 别把自己卡在"永远忙"
            return false;
        }
        true
    }

    /// **等手上那份写完**（保存前、退出前用；上限就是一次快照的代价 ~223 ms/5 万音符）。
    ///
    /// 为什么不是"取消"：写入是原子改名，半途停下只会留下一个 `.tmp`；而调用方要的恰恰是
    /// "缓存里那份不许再被旧内容盖回去"，那就只有等它落完再动手。
    pub fn flush(&mut self) {
        if self.idle() {
            return; // 手上没活：连"通道还在不在"都不必问
        }
        let Some(tx) = self.tx.as_mut() else { return };
        // 用一条"停"消息当栅栏：前一条一定先被处理完（单线程 FIFO），它处理完才会读到 Stop。
        // 但 Stop 会终结线程，所以再起一条 —— 直接把 tx 换成新线程的发送端。
        let (ntx, nrx) = mpsc::channel();
        let _ = tx.send(Msg::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let (b, w, e) = (self.busy.clone(), self.written.clone(), self.err.clone());
        self.handle = std::thread::Builder::new()
            .name("opm-snapshot".to_owned())
            .spawn(move || worker(nrx, b, w, e))
            .ok();
        self.tx = Some(ntx);
    }

    /// 取走"最近一次失败原因"（取一次就清空：GUI 只在**理由变化**时提示）。
    pub fn take_error(&self) -> Option<String> {
        self.err.lock().ok().and_then(|mut g| g.take())
    }
}

impl Drop for Autosave {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Stop);
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn worker(rx: Receiver<Msg>, busy: Arc<AtomicBool>, written: Arc<AtomicU64>, err: Arc<Mutex<Option<String>>>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Stop => break,
            Msg::Write(job) => {
                // 兜一层 panic：写盘这条路上任何一处炸了都不该**悄悄**把这台机器停掉 ——
                // 线程一死，`busy` 就永远停在"忙"，GUI 那边于是永远跳过快照（而且一声不响）。
                // `AssertUnwindSafe` 在这里是安全的：job 是这条线程独占的，炸了就直接丢掉它。
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write_snapshot(&job)))
                    .unwrap_or_else(|_| Err("快照写入 panic（后台线程已捕获）".to_owned()));
                match r {
                    Ok(()) => {
                        written.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut g) = err.lock() {
                            *g = None; // 好了就别再提旧账
                        }
                    }
                    Err(e) => {
                        if let Ok(mut g) = err.lock() {
                            *g = Some(e);
                        }
                    }
                }
                busy.store(false, Ordering::Release);
            }
        }
    }
    busy.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Beat, BpmEntry, Note as DocNote, NoteKind};

    fn small_doc() -> Document {
        let mut d = Document::default();
        d.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 174.0, foreign: Default::default() }];
        d.judge_lines[0].notes.push(DocNote::new(NoteKind::Tap, Beat::new(4, 1), 0.0));
        d
    }

    fn session_meta(dirty: bool) -> Session {
        Session {
            pid: std::process::id(),
            exe: codec::container::exe_name(),
            format: "opm".to_owned(),
            name: "t".to_owned(),
            started: 1,
            snapshot: 42,
            dirty,
            ..Default::default()
        }
    }

    /// 写一份 → 盘上就是那份文档（`opm.json` 能被重新解析回同一份 JSON），元数据也落下了
    #[test]
    fn a_job_lands_as_chart_plus_session_metadata() {
        let dir = std::env::temp_dir().join(format!("opm-autosave-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let doc = small_doc();
        let job = SnapshotJob { dir: dir.clone(), doc: doc.clone(), session: session_meta(true) };
        write_snapshot(&job).unwrap();

        let text = std::fs::read_to_string(dir.join(codec::container::CHART_NAME)).unwrap();
        let back: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back, doc.to_json(), "盘上的文档要与内存里那份一致");
        let s = codec::container::read_session(&dir).expect("会话元数据要落下");
        assert_eq!((s.snapshot, s.dirty), (42, true));
        assert!(!dir.join(format!("{}.tmp", codec::container::CHART_NAME)).exists(), "临时文件不该留下");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `flush` 之后盘上一定有那份文件；`try_send` 在写的时候拒绝第二份
    #[test]
    fn flush_waits_and_a_busy_writer_refuses_a_second_job() {
        let dir = std::env::temp_dir().join(format!("opm-autosave-flush-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut a = Autosave::spawn();
        let mk = || SnapshotJob { dir: dir.clone(), doc: small_doc(), session: session_meta(true) };
        assert!(a.try_send(mk()), "第一份要收下");
        // 立刻再投：要么还在写（拒），要么已经写完（收）—— 两种都对，但**不能**是"收下却不写"
        let _ = a.try_send(mk());
        a.flush();
        assert!(a.idle(), "flush 之后必须空闲");
        assert!(dir.join(codec::container::CHART_NAME).exists(), "flush 之后盘上要有文档");
        assert!(a.written() >= 1);
        drop(a);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
