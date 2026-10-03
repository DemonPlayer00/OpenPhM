// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! 控制通道：让 `opm-ctl` **接进正在运行的 GUI 进程**，操作同一个编辑会话。
//!
//! 协议极简：Unix socket 上跑**行分隔 JSON**，一问一答。
//! 同一进程内 GUI 与控制线程共享一个 `Arc<Mutex<EditCore>>`；远端命令走的是**和 GUI 完全相同的路径**：
//! `exec` → EditCore 改动 → **update 广播** → GUI 按话题重建。这条路径上没有任何"文档同步"逻辑，
//! 也没有谁比谁特殊——区别只是广播里的 `origin` 标记是 `Remote`。
//!
//! 额外两件事让这套机制对外可验证：
//! · `{"op":"ui_stats"}` **由控制线程直接回答**（不进 EditCore）：把 GUI 的收广播次数、各面板重建次数、
//!   被跳过的重建次数暴露出来 —— 这是"无关控件不参与更新"的客观证据；
//! · `waker`：远端改完之后唤醒 egui 重绘一次，否则空闲心跳下（默认 1 fps）要等一秒才看到变化。
//!
//! **平台形态**：Linux/macOS 走 Unix socket（`std::os::unix::net`）；Windows 上这套还没有等价实现
//! —— 该换命名管道，**协议不变**（行分隔 JSON 与所有视图命令都通用）。所以那里 [`spawn_server`] /
//! [`attach`] 会**如实返回"未实现"**，不假装启用：GUI 的 `--control` 打一行提示，`opm-ctl attach`
//! 报同一条。除传输层之外的部分（视图命令队列、`ui_stats`、协议解析、文档命令）平台无关，
//! Windows 上也编译、也走同一套单测。

use std::collections::VecDeque;
// `BufRead`/`Write` 只被传输层（Unix socket 那两个函数）用到；Windows 上它们不存在 ⇒ 别引入空警告
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
#[cfg(unix)]
use serde_json::json;
use serde_json::Value;

#[cfg(unix)]
use crate::broadcast::Origin;
use crate::core::SharedCore;

/// 唤醒 GUI 重绘的钩子（GUI 传 `ctx.request_repaint()`；无 GUI 时传 [`no_waker`]）
pub type RepaintWaker = Arc<dyn Fn() + Send + Sync>;

pub fn no_waker() -> RepaintWaker {
    Arc::new(|| {})
}

/// GUI 侧的更新统计，由 GUI 每帧写入、控制通道按需读取。
///
/// 两套计数，量纲不同，别混：
/// · `skipped_*`：**逐广播**统计"这条广播的话题与该面板无关" ⇒ `broadcasts - skipped_x`
///   就是真正触及该面板的广播数（"无关控件不参与更新"的直接证据）；
/// · `builds_*`：**逐批**统计实际重建次数。同一帧到达的多条广播会合并成一次重建，
///   因此 `builds_x ≤ broadcasts - skipped_x`，差值就是合并收益（K 条广播 1 次重建）。
///
/// 粒度是**判定线**：`builds_notes`/`builds_tracks`/`builds_props` 计的是"重建了几条线的这一部分"，
/// 所以"改 3 号线的音符"只让 `builds_notes` +1，其余线与其他面板一帧都不参与。
/// `ui_stats.tags` 里的一项（**视图状态**，不是文档数据）
#[derive(Clone, Debug, Serialize)]
pub struct TagStat {
    pub index: usize,
    /// `"gui"`（界面按 R 起的）/ `"cli"`（控制通道加的）—— 决定它在中轴上的**列**
    pub source: &'static str,
    pub start: f64,
    pub end: f64,
    pub color: [u8; 3],
}

#[derive(Clone, Default, Serialize)]
pub struct UiStats {
    pub frames: u64,
    pub working_frames: u64,
    pub idle_frames: u64,
    /// 从 EditCore 收到的广播条数
    pub broadcasts: u64,
    /// 判定线整表重建次数（线集合/时间映射变化才发生）
    pub builds_structure: u64,
    /// 判定线属性（按线）
    pub builds_props: u64,
    /// 判定线**子音符**缓存（按线）
    pub builds_notes: u64,
    /// 判定线**事件轨道**缓存（按线）
    pub builds_tracks: u64,
    /// **遮蔽区**通道缓存（按区）
    pub builds_zones: u64,
    pub builds_inspector: u64,
    pub builds_meta: u64,
    pub skipped_structure: u64,
    pub skipped_props: u64,
    pub skipped_notes: u64,
    pub skipped_tracks: u64,
    pub skipped_inspector: u64,
    /// 已把命令交给 EditCore、**尚未收到广播**（"等广播"状态）
    pub pending: bool,
    pub seen_revision: u64,
    pub last_broadcast: String,
    pub last_topics: Vec<String>,
    /// **本进程内**的广播延迟（GUI 控制台发命令 → 广播被应用）毫秒
    pub latency_p50_ms: f64,
    pub latency_last_ms: f64,
    /// **唤醒延迟**：控制线程请求重绘 → 那一帧真的跑到"应用广播"。
    /// 这一段与 EditCore 无关，量的是窗口系统/合成器的出帧调度（窗口被遮挡时会明显变长）。
    pub wakes: u64,
    pub wake_p50_ms: f64,
    pub wake_last_ms: f64,
    /// 底栏右端那一格**此刻显示的文本**（永远是帧率，如 `"60.0 fps"`；`""` = 还没凑满第一个窗口）。
    ///
    /// 为什么要暴露它：那一格只在出帧时才会变 —— 空闲停下之后它会**冻在最后一个读数上**，
    /// 想从外面核实"屏幕上是多少"靠截图得挑对帧，靠这个字段一行就能读
    /// （`ui_stats` 由控制线程直接回答，不惊动 GUI）。
    pub fps_text: String,
    /// **为什么没进 IDLE**（空串 = 没活，就该是 `IDLE`）。判据见 `main::busy_reason_of`：
    /// 播放中 / 正在按住拖拽 / 首帧布局 / 欠着按帧号的活 / bench 活跃阶段 / 音符位置还在补 /
    /// 还有脏位 / 命令已交出等广播。
    ///
    /// 为什么要暴露它：用户报"手动测试里无论如何都停不下来"时，"忙/不忙"这一个布尔
    /// 说不清责任在哪一条 —— 这个字符串能直接说出来。
    pub busy: String,
    /// 播放状态与播放头（视图命令的效果靠这些观察）
    pub playing: bool,
    pub playhead_sec: f64,
    pub playhead_beat: f64,
    /// 音频：路径、设备、游标位置、cpal 自校准的输出延迟、欠载、用户校准偏移
    pub audio: String,
    pub audio_device: String,
    pub audio_pos_ms: f64,
    pub audio_rate_ppm: f64,
    /// 音频时钟与墙钟的**累计偏差**（ms）与窗口（s）—— 偏差有界即可，别只看 ppm
    pub audio_dev_ms: f64,
    pub audio_window_s: f64,
    /// 事件重叠处数（加载时全量检测、之后每次改动增量检测）
    pub conflicts: u64,
    /// 编辑区叠加层：是否可见 / 是否按住了 H / 可见拍数
    pub overlay_visible: bool,
    pub overlay_h_held: bool,
    /// 音符区**窗口 X 偏移**（RPE 单位）：显示区间 = [偏移−675, 偏移+675]
    pub window_offset_x: f32,
    pub overlay_beats: f64,
    pub audio_latency_ms: f64,
    pub audio_underruns: u64,
    pub audio_offset_ms: f64,
    /// 最近一次唤醒请求时刻（不序列化，仅在进程内传递）
    #[serde(skip)]
    pub wake_at: Option<Instant>,
    /// **中轴标签**（视图状态，不进文档）：agent 写完 `{"op":"tag",…}` 靠它核实。
    ///
    /// 放进来是有意的 —— "不存储"意味着它没有任何文件痕迹，**不从这里读就没有第二条核实路径**。
    pub tags: Vec<TagStat>,
    /// 当前选中的标签下标（`null` = 没选）
    pub selected_tag: Option<usize>,
}

pub type UiStatsHandle = Arc<Mutex<UiStats>>;

pub fn ui_stats() -> UiStatsHandle {
    Arc::new(Mutex::new(UiStats::default()))
}

/// **视图命令**：只改"看/听的状态"（播放头、播放、音频），不碰文档。
///
/// 为什么不塞进 EditCore：EditCore 的职责是文档与撤销栈（见 `core.rs` 的单向数据流），
/// 播放头/播放状态是**视图状态**，不属于文档、也不该进撤销栈。所以这类命令走独立队列：
/// 控制线程收下 → GUI 下一帧取走执行 → 结果通过 `ui_stats` 观察。
#[derive(Clone, Debug)]
pub enum ViewCmd {
    Play,
    Pause,
    TogglePlay,
    SeekSec(f64),
    SeekBeat(f64),
    /// 相对挪动播放头（拍）。编辑区滚轮与 `{"op":"nudge","beats":N}` 是同一个动作。
    NudgeBeats(f64),
    /// 设置网格（视图状态）：拍方向每拍几条、坐标方向窗口几等分
    SetGrid {
        beat_div: Option<u32>,
        lane_div: Option<u32>,
    },
    /// 音符区**窗口 X 偏移**（视图状态）：把音符区显示的 laneX 区间平移，查看/编辑窗口外的音符。
    /// 与顶栏 `窗口 X 偏移` 拖动框同一条路径（夹到 ±675）。
    SetWindowOffsetX(f32),
    /// 缩放时间轴（视图状态）：`factor` 是倍率（Ctrl+滚轮走的就是这条），`beats` 是绝对可见拍数。
    /// 两者都给时先按 `beats`，再乘 `factor`；结果一律夹到 ZOOM_MIN/MAX。
    Zoom {
        factor: Option<f64>,
        beats: Option<f64>,
    },
    /// 选中某个对象（视图状态）：让 GUI 指向某条线 / 某个音符 / 某条事件。
    /// agent 用它把界面"指到"要检查的地方（选中不属于文档，所以走视图通道）。
    ///
    /// `notes` / `events` 是**多选**口径（`{"notes":[0,2]}`、`{"events":[["alpha",0]]}`）：
    /// 给了就以它为准整批替换 —— 没有这条路，"框选/多选"的效果就只能靠手点，
    /// agent 截图复核不了。
    Select {
        line: Option<usize>,
        track: Option<String>,
        note: Option<usize>,
        event: Option<usize>,
        notes: Option<Vec<usize>>,
        events: Option<Vec<(String, usize)>>,
        /// **遮蔽区**：`{"zone":0}` 选中它，`{"zone":0,"maskEdit":true}` 顺便进编辑模式；
        /// `"channel":"x1"` 切当前列，`"zoneEvent":2` 选中该列第 3 个事件块。
        ///
        /// agent 要用它把界面指到遮蔽区上（截图复核 / 检查器读数）——
        /// 与音符/事件同一条理由：选中是**视图**状态，不走命令通道。
        zone: Option<usize>,
        mask_edit: Option<bool>,
        channel: Option<String>,
        zone_event: Option<usize>,
    },
    /// **中轴标签**（视图状态，不进文档 —— 用户口径"不存储"）。
    ///
    /// 一条命令管四种动作，因为它们共享同一批参数、而且是同一件事的几个面：
    /// `add`（加一个，来源默认 `cli`）、`del`（按下标删）、`clear`（全清）、
    /// `select`（选中某个，属性编辑器据此显示颜色）。
    Tag(TagCmd),
    LoadAudio(String),
    SetOffsetMs(f64),
}

/// 控制通道的标签动作
#[derive(Clone, Debug)]
pub enum TagCmd {
    Add {
        start: f64,
        end: f64,
        color: Option<[u8; 3]>,
    },
    Del {
        index: usize,
    },
    Clear,
    Select {
        index: Option<usize>,
    },
}

pub type ViewQueue = Arc<Mutex<VecDeque<ViewCmd>>>;

pub fn view_queue() -> ViewQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

/// 把一条 JSON 命令翻译成视图命令；不是视图命令则返回 None
pub fn parse_view_cmd(v: &Value) -> Option<ViewCmd> {
    let op = v.get("op").and_then(|o| o.as_str())?;
    match op {
        "play" => Some(ViewCmd::Play),
        "pause" => Some(ViewCmd::Pause),
        "toggle_play" => Some(ViewCmd::TogglePlay),
        "seek" => {
            if let Some(b) = v.get("beat").and_then(|x| x.as_f64()) {
                Some(ViewCmd::SeekBeat(b))
            } else {
                Some(ViewCmd::SeekSec(v.get("to").and_then(|x| x.as_f64()).unwrap_or(0.0)))
            }
        }
        "grid" => Some(ViewCmd::SetGrid {
            beat_div: v.get("beatDiv").and_then(|x| x.as_u64()).map(|x| x as u32),
            lane_div: v.get("laneDiv").and_then(|x| x.as_u64()).map(|x| x as u32),
        }),
        "zoom" => Some(ViewCmd::Zoom {
            factor: v.get("factor").and_then(|x| x.as_f64()),
            beats: v.get("beats").and_then(|x| x.as_f64()),
        }),
        "window" => Some(ViewCmd::SetWindowOffsetX(
            v.get("offsetX").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32,
        )),
        "select" => Some(ViewCmd::Select {
            line: v.get("line").and_then(|x| x.as_u64()).map(|x| x as usize),
            track: v.get("track").and_then(|x| x.as_str()).map(|x| x.to_owned()),
            note: v.get("note").and_then(|x| x.as_u64()).map(|x| x as usize),
            event: v.get("event").and_then(|x| x.as_u64()).map(|x| x as usize),
            notes: v.get("notes").and_then(|x| x.as_array()).map(|a| {
                a.iter().filter_map(|n| n.as_u64()).map(|n| n as usize).collect()
            }),
            // `[["alpha", 0], ["alpha", 1]]` —— 轨道名用文档里的键（与事件区列名一致）。
            // **认不出的轨道名丢掉**：少选一条总好过整条视图命令没反应（agent 手写时最容易写歪这里）。
            events: v.get("events").and_then(|x| x.as_array()).map(|a| {
                a.iter()
                    .filter_map(|pair| {
                        let p = pair.as_array()?;
                        let track = p.first()?.as_str()?.to_owned();
                        let index = p.get(1)?.as_u64()? as usize;
                        Some((track, index))
                    })
                    .filter(|(t, _)| {
                        crate::state::TrackId::ALL.iter().any(|id| id.key() == t)
                    })
                    .collect()
            }),
            zone: v.get("zone").and_then(|x| x.as_u64()).map(|x| x as usize),
            mask_edit: v.get("maskEdit").and_then(|x| x.as_bool()),
            channel: v.get("channel").and_then(|x| x.as_str()).map(|x| x.to_owned()),
            zone_event: v.get("zoneEvent").and_then(|x| x.as_u64()).map(|x| x as usize),
        }),
        "tag" => {
            let idx = v.get("index").and_then(|x| x.as_u64()).map(|x| x as usize);
            match v.get("action").and_then(|x| x.as_str()).unwrap_or("add") {
                "add" => Some(ViewCmd::Tag(TagCmd::Add {
                    start: v.get("start").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    end: v.get("end").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    // 颜色缺省 = 该来源的默认色（CLI 黄）；写歪了就当没写
                    color: v.get("color").and_then(|x| x.as_array()).and_then(|a| {
                        if a.len() < 3 {
                            return None;
                        }
                        let mut c = [0u8; 3];
                        for (i, x) in a.iter().take(3).enumerate() {
                            c[i] = x.as_u64()?.min(255) as u8;
                        }
                        Some(c)
                    }),
                })),
                "del" => idx.map(|index| ViewCmd::Tag(TagCmd::Del { index })),
                "clear" => Some(ViewCmd::Tag(TagCmd::Clear)),
                "select" => Some(ViewCmd::Tag(TagCmd::Select { index: idx })),
                _ => None,
            }
        }
        "nudge" => Some(ViewCmd::NudgeBeats(
            v.get("beats").and_then(|x| x.as_f64()).unwrap_or(0.0),
        )),
        "audio" => Some(ViewCmd::LoadAudio(
            v.get("path").and_then(|x| x.as_str()).unwrap_or("").to_owned(),
        )),
        "audio_offset" => Some(ViewCmd::SetOffsetMs(
            v.get("ms").and_then(|x| x.as_f64()).unwrap_or(0.0),
        )),
        // `view` 是**查询**：不排队，直接由控制线程回 ui_stats（和 {"op":"ui_stats"} 等价）
        // 生产路径走不到这一支：`handle_conn` 已按 `op_name == "view"` 拦下它；这里只由公开契约测试 `tests/boundary.rs:39` 覆盖
        "view" => None,
        _ => None,
    }
}

/// 自动 socket 路径：`$XDG_RUNTIME_DIR/opm-<pid>.sock`（退回到临时目录）
pub fn auto_path() -> PathBuf {
    path_for_pid(std::process::id())
}

/// **某个 pid** 的控制通道路径。
///
/// 为什么要有 pid 版：判"某个谱面缓存的主人还在不在"时要问**那个进程**（见 `session::inspect`），
/// 而它的 socket 名字里就带 pid —— 于是不需要把 socket 路径也写进锁文件，
/// 少一份"两边不一致"的可能（pid 与路径的换算是纯函数，只有这一处）。
pub fn path_for_pid(pid: u32) -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    PathBuf::from(dir).join(format!("opm-{pid}.sock"))
}

/// **ping 一个正在跑的进程**（连它的控制通道，问一条 `{"op":"ping"}`）。
///
/// 两件事一起核：① 那个 socket 上确实有个 OpenPhM 在应答；② 它自报的 pid 就是我们要找的那个
/// （socket 文件不会随进程消失，只连上不核对 pid 会把"死进程留下的文件"当成活的）。
///
/// 返回它的应答（含 `pid`/`revision`/`cacheDir`）。**任何一步失败都算"没应答"**：
/// 连接被拒、超时、回的不是 JSON、`pong` 不是 true、pid 对不上 —— 调用方按"不通"处理。
#[cfg(unix)]
pub fn ping(path: &Path, timeout: std::time::Duration) -> Result<Value, String> {
    use std::io::{BufRead, BufReader, Write};
    let stream = std::os::unix::net::UnixStream::connect(path)
        .map_err(|e| format!("连接 {} 失败: {e}", path.display()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("设置读超时失败: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("设置写超时失败: {e}"))?;
    let mut writer = stream
        .try_clone()
        .map_err(|e| format!("复制连接失败: {e}"))?;
    let mut reader = BufReader::new(stream);
    // 连上就先收一条 `hello`（服务端行为），再问 ping —— 顺序由服务端决定，这里照它来
    let mut hello = String::new();
    let _ = reader.read_line(&mut hello);
    writer
        .write_all(b"{\"op\":\"ping\"}\n")
        .map_err(|e| format!("写 ping 失败: {e}"))?;
    writer.flush().map_err(|e| format!("刷新 ping 失败: {e}"))?;
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("读 ping 应答失败: {e}"))?;
    // 应答前可能还夹着别的行（统计/广播）：往后找第一条能解析的对象
    for cand in std::iter::once(line.as_str()).chain(hello.lines()) {
        if let Ok(v) = serde_json::from_str::<Value>(cand.trim()) {
            if v.get("op").and_then(|o| o.as_str()) == Some("ping") {
                return Ok(v);
            }
        }
    }
    Err(format!("{} 没有回应 ping（收到 {line:?}）", path.display()))
}

/// 非 Unix：控制通道本身还不存在（见 `spawn_server`）⇒ ping **不可用**。
///
/// 调用方必须把"不可用"与"不通"分开：判崩溃的第一判据是**锁没人持**（Unix `flock` /
/// Windows `LockFileEx`，两边都有），ping 只是补一道核对。
#[cfg(not(unix))]
pub fn ping(path: &Path, _timeout: std::time::Duration) -> Result<Value, String> {
    Err(format!(
        "本平台没有控制通道，无法 ping {}（判据退回到「锁没人持」）",
        path.display()
    ))
}

/// 发现最新的 opm socket（给 `--attach auto` 用）
pub fn find_socket() -> Option<PathBuf> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("opm-") && name.ends_with(".sock") {
            // 文件名里就带 pid —— socket 文件不会随进程消失，候选里混进死进程时
            // `--attach auto` 会连到一个没人监听的文件上。先按 /proc 过滤掉。
            let pid: i32 = name
                .trim_start_matches("opm-")
                .trim_end_matches(".sock")
                .parse()
                .unwrap_or(-1);
            if !Path::new(&format!("/proc/{pid}")).exists() {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(t) = meta.modified() {
                    if best.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
                        best = Some((t, entry.path()));
                    }
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

/// 在后台线程起一个监听器；返回实际绑定的路径。
#[cfg(unix)]
pub fn spawn_server(
    core: SharedCore,
    stats: UiStatsHandle,
    view: ViewQueue,
    waker: RepaintWaker,
    path: &Path,
) -> Result<PathBuf, String> {
    // 清掉陈旧的 socket 文件（上次异常退出会留下）
    let _ = std::fs::remove_file(path);
    let listener = std::os::unix::net::UnixListener::bind(path)
        .map_err(|e| format!("绑定 {} 失败: {e}", path.display()))?;
    let bound = path.to_path_buf();

    std::thread::Builder::new()
        .name("opm-control".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => {
                        let core = core.clone();
                        let stats = stats.clone();
                        let view = view.clone();
                        let waker = waker.clone();
                        std::thread::Builder::new()
                            .name("opm-control-conn".into())
                            .spawn(move || handle_conn(core, stats, view, waker, stream))
                            .ok();
                    }
                    Err(e) => eprintln!("[control] 接受连接失败: {e}"),
                }
            }
        })
        .map_err(|e| format!("启动控制线程失败: {e}"))?;

    Ok(bound)
}

/// 非 Unix（Windows）：控制通道还没有等价传输层 ⇒ **如实说不支持**。
///
/// 为什么不做成"静默成功"：GUI 的 `--control` 会据此打一行提示，使用者一眼知道
/// "这次没起控制通道"，而不是对着一个连不上的路径猜。要做的是把 Unix socket 换成命名管道
/// （协议与所有视图命令都不用改），那是另一件事。
#[cfg(not(unix))]
pub fn spawn_server(
    _core: SharedCore,
    _stats: UiStatsHandle,
    _view: ViewQueue,
    _waker: RepaintWaker,
    _path: &Path,
) -> Result<PathBuf, String> {
    Err("Windows 上还没有控制通道：Unix socket 换成命名管道这件事还没做（协议不变，见 control.rs 头部）".into())
}

#[cfg(unix)]
fn handle_conn(
    core: SharedCore,
    stats: UiStatsHandle,
    view: ViewQueue,
    waker: RepaintWaker,
    stream: std::os::unix::net::UnixStream,
) {
    let reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;

    // 连接即给一条 hello，便于客户端确认接对了进程
    let hello = {
        let c = core.lock().unwrap();
        json!({
            "ok": true, "op": "hello", "result": {
                "pid": std::process::id(),
                "name": c.doc().meta.name,
                "notes": c.doc().note_count(),
                "lines": c.doc().judge_lines.len(),
                "revision": c.revision(),
                "subscribers": c.subscriber_count(),
                "path": c.path().map(|p| p.display().to_string()),
            }
        })
    };
    if writeln!(writer, "{hello}").is_err() {
        return;
    }
    let _ = writer.flush();

    for line in reader.lines() {
        let Ok(line) = line else { break };
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str::<Value>(&line) {
            Ok(v) => v,
            Err(e) => {
                let resp = json!({"ok": false, "op": "parse", "error": format!("JSON 解析失败: {e}")});
                if writeln!(writer, "{resp}").is_err() {
                    break;
                }
                let _ = writer.flush();
                continue;
            }
        };

        let op_name = v.get("op").and_then(|o| o.as_str()).unwrap_or("");
        // GUI 侧统计：由控制线程直接回答，不进 EditCore（EditCore 不该知道界面长什么样）
        let resp = if op_name == "ui_stats" || op_name == "view" {
            let s = stats.lock().unwrap();
            let name = if op_name == "view" { "view" } else { "ui_stats" };
            json!({"ok": true, "op": name, "result": serde_json::to_value(&*s).unwrap_or(Value::Null)})
        } else if let Some(cmd) = parse_view_cmd(&v) {
            // 视图命令：排队给 GUI，下一帧执行；**响应只承诺"已受理"**，
            // 效果请读 ui_stats（和文档改动"等广播"是同一个诚实口径）
            let queued = {
                let mut q = view.lock().unwrap();
                q.push_back(cmd.clone());
                q.len()
            };
            waker(); // 别让空闲心跳（1 fps）拖慢响应
            json!({
                "ok": true, "op": op_name,
                "result": {
                    "view": true, "queued": queued,
                    "cmd": format!("{cmd:?}"),
                    "note": "视图状态在下一帧生效；读 ui_stats 观察 playing/playhead_sec/audio_pos_ms",
                }
            })
        } else {
            let mut c = core.lock().unwrap();
            let before = c.revision();
            c.set_origin(Origin::Remote); // 广播里标记来源，便于 UI 区分"谁改的"
            let r = c.exec(&v);
            c.set_origin(Origin::Local);
            let changed = c.revision() != before;
            drop(c);
            if changed {
                waker(); // 空闲心跳下也能立刻看到远端改动
            }
            r
        };

        if writeln!(writer, "{resp}").is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

/// 客户端：把一批命令发到已运行的进程，逐条打印响应。返回 (失败条数, 校验错误数)
#[cfg(unix)]
pub fn attach(
    path: &Path,
    cmds: &[Value],
    json_out: bool,
    quiet: bool,
) -> Result<(usize, usize), String> {
    use std::os::unix::net::UnixStream;
    let stream = UnixStream::connect(path)
        .map_err(|e| format!("连接 {} 失败: {e}（GUI 是否带 --control 启动？）", path.display()))?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);

    // 读 hello
    let mut banner = String::new();
    reader
        .read_line(&mut banner)
        .map_err(|e| format!("读取 greeting 失败: {e}"))?;
    if !quiet {
        if let Ok(v) = serde_json::from_str::<Value>(&banner) {
            let r = v.get("result").cloned().unwrap_or(Value::Null);
            eprintln!(
                "[attach] pid={} 文档={} 音符={} revision={}",
                r.get("pid").and_then(|x| x.as_i64()).unwrap_or(-1),
                r.get("name").and_then(|x| x.as_str()).unwrap_or("?"),
                r.get("notes").and_then(|x| x.as_u64()).unwrap_or(0),
                r.get("revision").and_then(|x| x.as_u64()).unwrap_or(0),
            );
        }
    }

    let mut failed = 0usize;
    let mut errors = 0usize;
    for cmd in cmds {
        writeln!(writer, "{cmd}").map_err(|e| format!("发送失败: {e}"))?;
        writer.flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Err("对端关闭了连接".into());
        }
        let resp: Value = serde_json::from_str(line.trim()).map_err(|e| format!("响应解析失败: {e}"))?;
        let ok = resp.get("ok").and_then(|v| v.as_bool()) == Some(true);
        if !ok {
            failed += 1;
        }
        // validate 的返回里带 errors 计数
        if resp.get("op").and_then(|v| v.as_str()) == Some("validate") {
            errors += resp
                .get("result")
                .and_then(|r| r.get("errors"))
                .and_then(|e| e.as_u64())
                .unwrap_or(0) as usize;
        }
        if !quiet {
            if json_out {
                println!("{resp}");
            } else {
                println!("{}", crate::cmd::response_line(&resp));
            }
        }
    }
    Ok((failed, errors))
}

/// 非 Unix（Windows）：见 [`spawn_server`] —— 没有传输层就连不上，如实报错
#[cfg(not(unix))]
pub fn attach(
    _path: &Path,
    _cmds: &[Value],
    _json_out: bool,
    _quiet: bool,
) -> Result<(usize, usize), String> {
    Err("Windows 上还没有控制通道：Unix socket 换成命名管道这件事还没做（协议不变，见 control.rs 头部）".into())
}

/// 供 GUI 侧调用：把控制通道状态拼成一行摘要
pub fn describe(path: &Path, name: &str) -> String {
    format!("控制通道 {name} → {}（opm-ctl --attach {}）", path.display(), path.display())
}

#[cfg(test)]
mod tag_cmd_tests {
    use super::*;
    use serde_json::json;

    /// 控制通道的标签命令：四个动作都要认，参数写歪了要**落回安全值**而不是整条命令没反应。
    #[test]
    fn the_tag_view_command_parses_all_four_actions() {
        match parse_view_cmd(&json!({"op": "tag", "action": "add", "start": 4.0, "end": 8.0})) {
            Some(ViewCmd::Tag(TagCmd::Add { start, end, color })) => {
                assert_eq!((start, end), (4.0, 8.0));
                assert_eq!(color, None, "不写颜色 ⇒ 用该来源的默认色（由调用方补）");
            }
            other => panic!("add 没解析出来：{other:?}"),
        }
        match parse_view_cmd(&json!({"op": "tag", "action": "add", "start": 1.0,
                                     "color": [10, 20, 300]})) {
            Some(ViewCmd::Tag(TagCmd::Add { color, .. })) => {
                assert_eq!(color, Some([10, 20, 255]), "颜色要夹到 0~255")
            }
            other => panic!("带颜色的 add 没解析出来：{other:?}"),
        }
        // 颜色数组长度不够 ⇒ 当没写（而不是补 0 变成黑）
        match parse_view_cmd(&json!({"op": "tag", "action": "add", "color": [1]})) {
            Some(ViewCmd::Tag(TagCmd::Add { color, .. })) => assert_eq!(color, None),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_view_cmd(&json!({"op": "tag", "action": "del", "index": 2})),
            Some(ViewCmd::Tag(TagCmd::Del { index: 2 }))
        ));
        assert!(matches!(
            parse_view_cmd(&json!({"op": "tag", "action": "clear"})),
            Some(ViewCmd::Tag(TagCmd::Clear))
        ));
        assert!(matches!(
            parse_view_cmd(&json!({"op": "tag", "action": "select", "index": 1})),
            Some(ViewCmd::Tag(TagCmd::Select { index: Some(1) }))
        ));
        assert!(matches!(
            parse_view_cmd(&json!({"op": "tag", "action": "select"})),
            Some(ViewCmd::Tag(TagCmd::Select { index: None }))
        ));
        // del 不给下标 ⇒ 不是命令（`None` 会让它落到 EditCore，由那边报"未知 op"）
        assert!(parse_view_cmd(&json!({"op": "tag", "action": "del"})).is_none());
        // 未知动作同理
        assert!(parse_view_cmd(&json!({"op": "tag", "action": "??"})).is_none());
    }
}
