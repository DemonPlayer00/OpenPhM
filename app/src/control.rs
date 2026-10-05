// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! 控制通道：让 `opm-ctl` **接进正在运行的 GUI 进程**，操作同一个编辑会话。
//!
//! 协议极简：传输层上跑**行分隔 JSON**，一问一答。
//! 同一进程内 GUI 与控制线程共享一个 `Arc<Mutex<EditCore>>`；远端命令走的是**和 GUI 完全相同的路径**：
//! `exec` → EditCore 改动 → **update 广播** → GUI 按话题重建。这条路径上没有任何"文档同步"逻辑，
//! 也没有谁比谁特殊——区别只是广播里的 `origin` 标记是 `Remote`。
//!
//! 额外两件事让这套机制对外可验证：
//! · `{"op":"ui_stats"}` **由控制线程直接回答**（不进 EditCore）：把 GUI 的收广播次数、各面板重建次数、
//!   被跳过的重建次数暴露出来 —— 这是"无关控件不参与更新"的客观证据；
//! · `waker`：远端改完之后唤醒 egui 重绘一次，否则空闲心跳下（默认 1 fps）要等一秒才看到变化。
//!
//! **平台形态**：传输层两条路，**协议与上层逐字不变**（行分隔 JSON、所有视图命令、`ui_stats` 通用）：
//! · Unix ⇒ `std::os::unix::net`，路径 `$XDG_RUNTIME_DIR/opm-<pid>.sock`；
//! · Windows ⇒ **命名管道** `\\.\pipe\opm-<pid>`（`CreateNamedPipeW` 那一套）。发现方式是把
//!   `opm-<pid>.pipe` 标记文件写进 `%LOCALAPPDATA%\OpenPhM\control\`，`--attach auto` 扫这个目录、
//!   挑最新的一个，再用 `WaitNamedPipeW` **验活**。
//!
//! 为什么 Windows 不枚举 `\\.\pipe\`：那是 `FindFirstFile` 在一个**没写进文档的伪路径**上；
//! 标记文件是明面上的东西、可测，而且顺手解决了"上次没退干净留下的死管道"（验活不过就不算）。

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

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
        /// 省略 ⇒ 起点 + 最短长度（`TAG_MIN_BEATS`）
        end: Option<f64>,
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
/// 认得出名字、但 [`parse_view_cmd`] 没收下的视图命令该怎么说。
///
/// 报错要指到点子上：`{"op":"tag","action":"add","end":8}` 缺的是 `start`，
/// 不是"未知命令 tag"。
///
/// 曾经它挂着 `#[cfg(unix)]`（那时 Windows 上没有控制通道，不门控会报 dead_code）——
/// 现在两条传输层共用同一段 `handle_conn`，于是它也**不再分平台**。
fn view_cmd_hint(op: &str) -> Option<&'static str> {
    match op {
        "tag" => Some(
            "action 取 add / del / clear / select；**add 必须给 start（拍）**，end 可省             （缺省 = 起点 + 最短长度），color 可省（缺省 = 该来源的默认色）",
        ),
        "select" | "grid" | "zoom" | "window" | "nudge" | "seek" | "audio" | "audio_offset" => {
            Some("参数不合格（这个 op 认，但这次的字段不对）")
        }
        _ => None,
    }
}

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
                // `start` **必需**：默认成 0 的话，"少写一个字段"的后果是标签静默跑到第 0 拍
                // （与 `add_note` 的 `laneX` 同一个毛病）。缺它就**不是一条视图命令** ——
                // 落到 EditCore 那边报错，比悄悄放一个标签在第 0 拍强。
                "add" => Some(ViewCmd::Tag(TagCmd::Add {
                    start: v.get("start").and_then(|x| x.as_f64())?,
                    end: v.get("end").and_then(|x| x.as_f64()),
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

/// 自动控制通道路径（本进程）。
pub fn auto_path() -> PathBuf {
    path_for_pid(std::process::id())
}

/// **某个 pid** 的控制通道路径。
///
/// 为什么要有 pid 版：判"某个谱面缓存的主人还在不在"时要问**那个进程**（见 `session::inspect`），
/// 而它的名字里就带 pid —— 于是不需要把路径也写进锁文件，
/// 少一份"两边不一致"的可能（pid 与路径的换算是纯函数，只有这一处）。
///
/// 形态按平台：
/// · Unix ⇒ `$XDG_RUNTIME_DIR/opm-<pid>.sock`（退回到临时目录）；
/// · Windows ⇒ `\\.\pipe\opm-<pid>`（命名管道的名字本身就是路径，可以直接交给 `CreateFileW`）。
pub fn path_for_pid(pid: u32) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\opm-{pid}"))
    }
    #[cfg(not(windows))]
    {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
        PathBuf::from(dir).join(format!("opm-{pid}.sock"))
    }
}

// ---------------------------------------------------------------- Windows 命名管道

/// Windows 命名管道的**标记目录**：`%LOCALAPPDATA%\OpenPhM\control\`。
///
/// 里面一个进程一个文件（`opm-<pid>.pipe`），内容是人看的说明 —— 发现靠**文件名**，
/// 不靠内容（内容会被外部改动，文件名不会）。
#[cfg(windows)]
pub fn marker_dir() -> Option<PathBuf> {
    crate::paths::local_data_dir().map(|d| d.join("control"))
}

/// 某个 pid 的标记文件路径（`%LOCALAPPDATA%\OpenPhM\control\opm-<pid>.pipe`）
#[cfg(windows)]
pub fn marker_path(pid: u32) -> Option<PathBuf> {
    marker_dir().map(|d| d.join(format!("opm-{pid}.pipe")))
}

/// 从标记文件名反解 pid（`opm-123.pipe` → `123`）；不是这个形状就 `None`。
///
/// 纯函数：`--attach auto` 的候选筛选全在它上面，且**两条平台共用同一套文件名规则**
/// （Unix 那边是 `opm-<pid>.sock`，解析是同一份逻辑，只是后缀不同）。
pub fn pid_of_marker(name: &str) -> Option<u32> {
    let stem = name
        .strip_prefix("opm-")
        .and_then(|s| s.strip_suffix(".pipe").or_else(|| s.strip_suffix(".sock")))?;
    stem.parse().ok()
}

/// 命名管道那一套（`CreateNamedPipeW`/`ConnectNamedPipe`/`WaitNamedPipeW`）。
///
/// 单独一个模块：unsafe 集中在一处，上面的连接处理（`handle_conn`）对"这是管道还是 socket"
/// 一无所知 —— 它只认 `BufRead` + `Write`。这也是"协议与上层逐字不变"在代码上的样子。
#[cfg(windows)]
mod pipe {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    /// 宽字符串（以 NUL 结尾）—— 所有 Win32 W 接口都要这个形态
    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// 建**一个**管道实例并阻塞等一个客户端连上来，返回服务端这一头。
    ///
    /// 一个实例只服务一条连接（`incoming()` 那样）：处理完就丢掉、循环再建一个新的，
    /// 于是"同时来两个客户端"只是多开一个实例，与 Unix 侧行为一致。
    pub fn accept_one(name: &Path) -> std::io::Result<File> {
        let name = wide(&name.to_string_lossy());
        // 缓冲区给足：一条命令 + 一段 `ui_stats` 都远小于它，省得管道写阻塞在半截 JSON 上
        const BUF: u32 = 1 << 16;
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                BUF,
                BUF,
                0,
                std::ptr::null(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            let _ = h;
            return Err(std::io::Error::last_os_error());
        }
        // 客户端可能在 `CreateNamedPipeW` 与这一句之间就连上了：那时 `ConnectNamedPipe`
        // 返回 0 且 last-error 是 `ERROR_PIPE_CONNECTED` —— **那也是连上了**，不是失败。
        let ok = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_PIPE_CONNECTED {
                unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
                return Err(std::io::Error::from_raw_os_error(err as i32));
            }
        }
        Ok(unsafe { File::from_raw_handle(h as _) })
    }

    /// 这个管道名上**现在**有服务端在等连接吗（`--attach auto` 的验活）。
    ///
    /// `WaitNamedPipeW(name, 0)`：0 毫秒 = 只看一眼有没有空闲实例，不等。
    /// 死进程留下的名字会立刻回 `ERROR_FILE_NOT_FOUND` ⇒ 不算候选。
    pub fn is_live(name: &Path) -> bool {
        let name = wide(&name.to_string_lossy());
        unsafe { WaitNamedPipeW(name.as_ptr(), 0) != 0 }
    }

    /// 带截止时间读一行（**只有客户端用得上**：`ping` 跑在启动路径上，不能被一个只连不答的对端卡住）。
    ///
    /// Windows 的管道句柄没有 `set_read_timeout`（Unix 那边有），所以这里的超时是**自己数出来的**：
    /// 先 `PeekNamedPipe` 问"现在有多少字节可读"，没有就睡 2 ms 再看，直到超过截止时间。
    /// 有数据才 `read` —— 于是那个 `read` 不会阻塞（这是这一段的关键：直接 `read` 会一直挂着）。
    pub fn read_line_timeout(mut file: &File, timeout: std::time::Duration) -> std::io::Result<String> {
        use std::io::Read;
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;
        let handle = std::os::windows::io::AsRawHandle::as_raw_handle(file) as _;
        let deadline = std::time::Instant::now() + timeout;
        let mut out = String::new();
        let mut byte = [0u8; 1];
        loop {
            let mut avail: u32 = 0;
            let ok = unsafe {
                PeekNamedPipe(handle, std::ptr::null_mut(), 0, std::ptr::null_mut(), &mut avail, std::ptr::null_mut())
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if avail == 0 {
                if std::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "等控制通道应答超时",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
            // 至少一个字节可读 ⟹ 这次 read 不会阻塞
            if file.read(&mut byte)? == 0 {
                return Ok(out); // 对端关了
            }
            if byte[0] == b'\n' {
                return Ok(out);
            }
            out.push(byte[0] as char);
            // 一行 JSON 不可能有这么大：防的是"对端一直灌数据"把内存吃光
            if out.len() > 1 << 20 {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "应答行过长"));
            }
        }
    }
}

/// 非 Unix ∩ 非 Windows：没有传输层（这套东西目前只支持这/两种平台）
#[cfg(not(any(unix, windows)))]
pub fn marker_dir() -> Option<PathBuf> {
    None
}

/// **ping 一个正在跑的进程**（连它的控制通道，问一条 `{"op":"ping"}`）。
///
/// 两件事一起核：① 那个名字上确实有个 OpenPhM 在应答；② 它自报的 pid 就是我们要找的那个
/// （Unix 的 socket 文件不会随进程消失，只连上不核对 pid 会把"死进程留下的文件"当成活的）。
///
/// 返回它的应答（含 `pid`/`revision`/`cacheDir`）。**任何一步失败都算"没应答"**：
/// 连接被拒、超时、回的不是 JSON、`pong` 不是 true、pid 对不上 —— 调用方按"不通"处理。
#[cfg(unix)]
pub fn ping(path: &Path, timeout: std::time::Duration) -> Result<Value, String> {
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
    ping_reply(path, &line, &hello).ok_or_else(|| format!("{} 没有回应 ping（收到 {line:?}）", path.display()))
}

/// Windows：同一件事，走命名管道。
///
/// 与 Unix 那支的差别只有一处、但是**必须**的一处：管道没有 `set_read_timeout`，
/// 而 `ping` 会在**启动路径**上被调用（`session::inspect` 判"上次没退干净"）——
/// 一个只连不答的对端不能把启动卡住。所以这里的读是"先 `PeekNamedPipe` 看有没有数据、
/// 没有就按截止时间等"（见 [`pipe::read_line_timeout`]）。
#[cfg(windows)]
pub fn ping(path: &Path, timeout: std::time::Duration) -> Result<Value, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("连接 {} 失败: {e}", path.display()))?;
    let mut writer = file.try_clone().map_err(|e| format!("复制连接失败: {e}"))?;
    // 连上就先收一条 `hello`（服务端行为），再问 ping —— 顺序由服务端决定，这里照它来
    let hello = pipe::read_line_timeout(&file, timeout).unwrap_or_default();
    writer
        .write_all(b"{\"op\":\"ping\"}\n")
        .map_err(|e| format!("写 ping 失败: {e}"))?;
    writer.flush().map_err(|e| format!("刷新 ping 失败: {e}"))?;
    let line = pipe::read_line_timeout(&file, timeout)
        .map_err(|e| format!("读 ping 应答失败: {e}"))?;
    ping_reply(path, &line, &hello).ok_or_else(|| format!("{} 没有回应 ping（收到 {line:?}）", path.display()))
}

/// 非 Unix ∩ 非 Windows：没有传输层 ⟹ ping 不可用。
///
/// 调用方必须把"不可用"与"不通"分开：判崩溃的第一判据是**锁没人持**
/// （Unix `flock` / Windows `LockFileEx`，两边都有），ping 只是补一道核对。
#[cfg(not(any(unix, windows)))]
pub fn ping(path: &Path, _timeout: std::time::Duration) -> Result<Value, String> {
    Err(format!(
        "本平台没有控制通道，无法 ping {}（判据退回到「锁没人持」）",
        path.display()
    ))
}

/// 从收到的行里找出 `ping` 的应答（两条平台的解析**同一份**）。
///
/// 应答前可能还夹着别的行（统计/广播）：所以 hello 与那一行一起看，往后找第一条能解析、
/// 且 `op == "ping"` 的对象。
fn ping_reply(_path: &Path, line: &str, hello: &str) -> Option<Value> {
    for cand in std::iter::once(line).chain(hello.lines()) {
        if let Ok(v) = serde_json::from_str::<Value>(cand.trim()) {
            if v.get("op").and_then(|o| o.as_str()) == Some("ping") {
                return Some(v);
            }
        }
    }
    None
}

/// 发现最新的控制通道（给 `--attach auto` 用）。
///
/// · Unix：扫 `$XDG_RUNTIME_DIR` 下的 `opm-*.sock`，按 mtime 取最新；
///   **先按 `/proc/<pid>` 过滤**（socket 文件不会随进程消失，候选里混进死进程时
///   `--attach auto` 会连到一个没人监听的文件上）；
/// · Windows：见 [`find_live_pipe`]。
#[cfg(unix)]
pub fn find_socket() -> Option<PathBuf> {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(pid) = pid_of_marker(&name) {
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

/// Windows：扫标记目录挑最新的**活着**的管道。
///
/// 判活不是猜进程在不在，而是 `WaitNamedPipeW` **真的试一下** —— 死进程留下的标记会被跳过
/// （顺手删掉：`spawn_server` 那边也会清理，这里再清一次是为了"没人重启过 GUI 也能自愈"）。
#[cfg(windows)]
pub fn find_socket() -> Option<PathBuf> {
    find_live_pipe(&marker_dir()?, pipe::is_live)
}

/// [`find_socket`] 的实现主体（**纯逻辑，可测**）：目录 + 判活函数 → 最新那个活着的管道。
///
/// `is_live` 由调用方给：真身是 `WaitNamedPipeW`，测试里喂一个假的 —— 于是
/// "取最新的**活着**的那一个"这条规则不开 Wine 也能钉住（本机是 Linux，起不了命名管道）。
#[cfg(windows)]
fn find_live_pipe(
    dir: &Path,
    is_live: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(pid) = pid_of_marker(&name) else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(t) = meta.modified() else { continue };
        candidates.push((t, path_for_pid(pid)));
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0)); // 新→旧
    candidates.into_iter().map(|(_, p)| p).find(|p| is_live(p))
}

/// 非 Unix ∩ 非 Windows：没有可发现的东西
#[cfg(not(any(unix, windows)))]
pub fn find_socket() -> Option<PathBuf> {
    None
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
                            .spawn(move || {
                                let Ok(reader) = stream.try_clone() else { return };
                                handle_conn(core, stats, view, waker, BufReader::new(reader), stream);
                            })
                            .ok();
                    }
                    Err(e) => eprintln!("[control] 接受连接失败: {e}"),
                }
            }
        })
        .map_err(|e| format!("启动控制线程失败: {e}"))?;

    Ok(bound)
}

/// Windows：控制通道的**服务端**（命名管道）。
///
/// 比 Unix 那支多两件事：
/// · **写标记文件**（`%LOCALAPPDATA%\OpenPhM\control\opm-<pid>.pipe`）—— `--attach auto` 靠它发现；
/// · 顺手**清理死标记**：目录里那些"管道已经没人应答"的标记（上次没退干净留下的）先删掉，
///   否则那个目录只会越积越多。
/// 有标记 + `WaitNamedPipeW` 验活这套组合，比 Unix 那边"文件名里带 pid 再问 `/proc`"还准 ——
/// 它是**真的连一下试试**，而不是猜进程在不在。
#[cfg(windows)]
pub fn spawn_server(
    core: SharedCore,
    stats: UiStatsHandle,
    view: ViewQueue,
    waker: RepaintWaker,
    path: &Path,
) -> Result<PathBuf, String> {
    let bound = path.to_path_buf();
    let me = std::process::id();
    if let Some(dir) = marker_dir() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("建标记目录失败: {e}"))?;
        prune_dead_markers(&dir, me);
        if let Some(marker) = marker_path(me) {
            let _ = std::fs::write(&marker, format!("{}\n", bound.display()));
        }
    }

    let name = bound.clone();
    std::thread::Builder::new()
        .name("opm-control".into())
        .spawn(move || loop {
            // 一条连接一个实例：处理完就丢掉、再建一个（与 `listener.incoming()` 同形）
            let handle = match pipe::accept_one(&name) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("[control] 建管道/接受连接失败: {e}");
                    // 名字被占、权限问题这类硬错误重试没意义，但**不能退出**：
                    // 退出之后就再也接不上了（宁可刷日志也别静默死掉）
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
            };
            let core = core.clone();
            let stats = stats.clone();
            let view = view.clone();
            let waker = waker.clone();
            std::thread::Builder::new()
                .name("opm-control-conn".into())
                .spawn(move || {
                    let Ok(reader) = handle.try_clone() else { return };
                    handle_conn(core, stats, view, waker, BufReader::new(reader), handle);
                })
                .ok();
        })
        .map_err(|e| format!("启动控制线程失败: {e}"))?;

    Ok(bound)
}

/// 删掉标记目录里**已经没人应答**的标记（`keep` = 自己那个，永远不删）。
#[cfg(windows)]
fn prune_dead_markers(dir: &Path, keep: u32) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(pid) = pid_of_marker(&name) else { continue };
        if pid == keep {
            continue;
        }
        if !pipe::is_live(&path_for_pid(pid)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 非 Unix ∩ 非 Windows：没有传输层，如实说不支持（`--control` 会打一行提示）
#[cfg(not(any(unix, windows)))]
pub fn spawn_server(
    _core: SharedCore,
    _stats: UiStatsHandle,
    _view: ViewQueue,
    _waker: RepaintWaker,
    _path: &Path,
) -> Result<PathBuf, String> {
    Err("本平台还没有控制通道（只有 Unix socket 与 Windows 命名管道两条路）".into())
}

/// 一条连接的处理循环。**与传输层无关**：只认"能按行读、能写"。
///
/// 这就是"协议与上层逐字不变"在代码上的样子 —— Unix socket 与命名管道在这里没有任何分支。
fn handle_conn<R: BufRead, W: Write>(
    core: SharedCore,
    stats: UiStatsHandle,
    view: ViewQueue,
    waker: RepaintWaker,
    reader: R,
    mut writer: W,
) {
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
        } else if let Some(hint) = view_cmd_hint(op_name) {
            // **认得出名字、但参数不合格**的视图命令。
            //
            // 单独一支的理由：不加的话它会掉到 EditCore，报成"未知命令 tag" —— 而调用方
            // 明明是**少写了一个字段**。用户 2026-10-03 报的"推 tag 到谱面中坐标都为 0"
            // 就是这种情形的前身（那时更糟：不是报错，是静默放在第 0 拍）。
            json!({"ok": false, "op": op_name, "error": format!("{op_name}：{hint}")})
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
///
/// 传输层的差别只有"怎么连上"这一句，`attach_stream` 里是**两条平台逐字共用**的协议部分。
#[cfg(unix)]
pub fn attach(
    path: &Path,
    cmds: &[Value],
    json_out: bool,
    quiet: bool,
) -> Result<(usize, usize), String> {
    let stream = std::os::unix::net::UnixStream::connect(path)
        .map_err(|e| format!("连接 {} 失败: {e}（GUI 是否带 --control 启动？）", path.display()))?;
    let writer = stream.try_clone().map_err(|e| e.to_string())?;
    attach_stream(BufReader::new(stream), writer, cmds, json_out, quiet)
}

/// Windows：同一件事，走命名管道（写半边要复制一份句柄，与 Unix 那边同形）。
#[cfg(windows)]
pub fn attach(
    path: &Path,
    cmds: &[Value],
    json_out: bool,
    quiet: bool,
) -> Result<(usize, usize), String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("连接 {} 失败: {e}（GUI 是否带 --control 启动？）", path.display()))?;
    let writer = file.try_clone().map_err(|e| e.to_string())?;
    attach_stream(BufReader::new(file), writer, cmds, json_out, quiet)
}

/// 非 Unix ∩ 非 Windows：没有传输层
#[cfg(not(any(unix, windows)))]
pub fn attach(
    _path: &Path,
    _cmds: &[Value],
    _json_out: bool,
    _quiet: bool,
) -> Result<(usize, usize), String> {
    Err("本平台还没有控制通道（只有 Unix socket 与 Windows 命名管道两条路）".into())
}

/// [`attach`] 的协议部分：**与传输层无关**（只认"能按行读、能写"）。
fn attach_stream<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    cmds: &[Value],
    json_out: bool,
    quiet: bool,
) -> Result<(usize, usize), String> {
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
                assert_eq!((start, end), (4.0, Some(8.0)));
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
        match parse_view_cmd(&json!({"op": "tag", "action": "add", "start": 4.0, "color": [1]})) {
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
        // **`start` 必需**：不给就**不是一条视图命令**（落到 EditCore 报错），
        // 而不是静默地在第 0 拍放一个标签 —— 用户报的"坐标都为 0"就是这个毛病。
        assert!(parse_view_cmd(&json!({"op": "tag", "action": "add", "end": 8.0})).is_none());
        // 只给 start ⇒ end 交给调用方按最短长度补
        match parse_view_cmd(&json!({"op": "tag", "action": "add", "start": 4.0})) {
            Some(ViewCmd::Tag(TagCmd::Add { start, end, .. })) => {
                assert_eq!((start, end), (4.0, None))
            }
            other => panic!("{other:?}"),
        }
        // del 不给下标 ⇒ 不是命令（`None` 会让它落到 EditCore，由那边报"未知 op"）
        assert!(parse_view_cmd(&json!({"op": "tag", "action": "del"})).is_none());
        // 未知动作同理
        assert!(parse_view_cmd(&json!({"op": "tag", "action": "??"})).is_none());
    }
}

/// 传输层的"发现"那一半：**两条平台共用同一套文件名规则**，所以解析要一起测。
#[cfg(test)]
mod transport_tests {
    use super::*;

    /// 标记文件名 ⇄ pid。两个后缀都认（`.pipe` 是 Windows 的标记、`.sock` 是 Unix 的 socket），
    /// 别的形状一律 `None` —— `--attach auto` 拿它筛候选，认错一个就会去连一个不存在的东西。
    #[test]
    fn marker_names_map_to_pids_on_both_platforms() {
        assert_eq!(pid_of_marker("opm-1234.pipe"), Some(1234));
        assert_eq!(pid_of_marker("opm-1234.sock"), Some(1234));
        assert_eq!(pid_of_marker("opm-1.sock"), Some(1));
        // 不是我们的东西：别的程序的管道/socket、目录项、备份文件
        for bad in [
            "opm-.pipe",
            "opm-abc.pipe",
            "opm-12.pipe.bak",
            "other-12.pipe",
            "opm-12",
            ".opm-12.pipe",
            "opm-99999999999999999999.pipe", // 溢出 u32
        ] {
            assert_eq!(pid_of_marker(bad), None, "{bad} 不该被认成控制通道");
        }
    }

    /// `--attach auto` 的挑选规则：**新的优先，且只挑活着的**（死的跳过，不是报错）。
    ///
    /// 判活函数由调用方注入（Windows 真身是 `WaitNamedPipeW`），所以这条规则**不开 Windows 也能测**。
    #[cfg(windows)]
    #[test]
    fn auto_attach_picks_the_newest_live_pipe() {
        let dir = std::env::temp_dir().join(format!("opm-markers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 三个标记，mtime 依次拉开（旧 → 新：11 / 22 / 33）
        for pid in [11u32, 22, 33] {
            let p = dir.join(format!("opm-{pid}.pipe"));
            std::fs::write(&p, "x").unwrap();
            let age = (33 - pid) as u64; // 33 最新
            let when = std::time::SystemTime::now() - std::time::Duration::from_secs(age * 60);
            let _ = std::fs::File::options().write(true).open(&p).unwrap().set_modified(when);
        }
        // 只有最旧的活着 ⇒ 挑它（跳过两个更新的死的）。
        // 注意返回的是**管道名**（`\.\pipe\opm-<pid>`），不是标记文件的路径 —— 标记只用来发现。
        let only_old = find_live_pipe(&dir, |p| p.ends_with("opm-11"));
        assert!(only_old.as_ref().is_some_and(|p| p.ends_with("opm-11")), "{only_old:?}");
        // 都活着 ⇒ 挑最新的
        let newest = find_live_pipe(&dir, |_| true);
        assert!(newest.as_ref().is_some_and(|p| p.ends_with("opm-33")), "{newest:?}");
        // 都死了 ⇒ None（不是 panic、也不是随便挑一个）
        assert_eq!(find_live_pipe(&dir, |_| false), None);
        // 目录不存在 ⇒ None
        assert_eq!(find_live_pipe(&dir.join("nope"), |_| true), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
