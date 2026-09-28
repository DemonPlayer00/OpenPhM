// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! OpenPhM 制谱器 —— UI 骨架（Linux 优先，Windows 兼容测试待后）
//!
//! 布局：顶部菜单 / 底部状态栏 / 左侧音符列表（虚拟化）/ 右侧属性检查器 /
//!       中央演奏区（自研 wgpu 实例化，viewport 映射）+ 时间轴
//!
//! **数据流（单向，不可逆）**：
//! ```text
//!   用户操作 / 控制台 / 远端 attach
//!        └─▶ EditCore::exec（唯一可写方）
//!                └─▶ update 广播（细粒度话题）
//!                        └─▶ 本文件按话题置脏 → 只重建脏掉的那一块
//! ```
//! 这里**没有任何一处直接改文档**：面板只是 EditCore 的订阅者。所有的"界面更新"都发生在
//! 收到广播之后；`ui_stats`（`opm-ctl --attach auto --cmd '{"op":"ui_stats"}'`）把
//! "收到几条广播 / 各面板重建几次 / 哪些重建被跳过"暴露给外部，供验收。
//!
//! 用法：
//!   opm-app [--notes 20000] [--bench 600] [--stress] [--scale 1.25]
//!           [--fps-cap 60] [--lookahead 2.0] [--control auto] [--doc x.opm.json]

use opm_app::{
    audio, broadcast, cli, cmd, control, core, filedialog, fonts, headless, keymap, recents,
    render, state, view, zip,
};
// 命令行参数与工作区预设（纯解析 + 单测在 `opm_app::cli`）
use opm_app::cli::{Args, Workspace};
// 视图模型（纯派生快照：左侧列表行 / 右侧检查器）——定义在库里，见 `opm_app::view`
use opm_app::view::{Inspector, LineRow};
// 脏位映射（话题 → 重建哪块）定义在库里：`opm_app::dirty`（纯函数 + 单测）
use opm_app::dirty::{self, Dirty};

mod conflicts;
mod inspector;
mod statusbar;
mod overlay;
mod tree;
// 测试公用件：与库里那份是**同一份文件**（这几个面板模块本来就既进库也进本程序，
// 见 `lib.rs` 的模块表；两边的 `cfg(test)` 各自成立，助手只有一份源码）
#[cfg(test)]
mod testkit;
use overlay::{OverlayAction, OverlayCfg};
use tree::{line_tree_ui, TreeAction};

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use render::{build_instances, build_instances_all, NoteInstance, Playfield, PlayfieldFrame};
use state::EditorState;

/// 状态栏那份**文档标识**（`[格式] 文件名`、有没有未保存改动、有没有保存目标）。
///
/// 抽成纯读函数是因为它有两个调用点（`App::new` 的初值、`refresh_file_badge` 的事件刷新），
/// 而"怎么拼这行字"只该有一份 —— 上一版把逻辑写在方法里，`--doc FILE` 那条启动路径
/// 没有经过任何刷新点，于是已载入的文件被显示成"尚未保存"。
/// 目录里有没有 `.json`（ICD 声明文件）——只看后缀，不解析内容
fn dir_has_json(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "json"))
        })
        .unwrap_or(false)
}

/// 是不是跑在 Wine 上（**只有 Windows 目标可能为真**）。
///
/// 判据是 `C:\windows\system32\wineboot.exe`：每个 Wine 前缀都有它，真 Windows 上不存在这个文件。
/// 为什么要知道：Wine 的 dxgi 缺 `IDXGIFactoryMedia`，DX12 实例创建时那次**可选**探测会在 stderr
/// 留一行 `create_factory_media failed: 0x80004002` —— 先说一句，省得每次都被当成故障去排查（§7.47）。
fn under_wine() -> bool {
    cfg!(windows) && std::path::Path::new(r"C:\windows\system32\wineboot.exe").exists()
}

/// wgpu 的 `DeviceType` → `opm_app::gpu::GpuKind`（库里的策略是纯逻辑，不依赖 wgpu 类型）
fn gpu_kind_of(t: eframe::wgpu::DeviceType) -> opm_app::gpu::GpuKind {
    use eframe::wgpu::DeviceType as T;
    use opm_app::gpu::GpuKind as K;
    match t {
        T::IntegratedGpu => K::Integrated,
        T::DiscreteGpu => K::Discrete,
        T::Cpu => K::Cpu,
        T::VirtualGpu => K::Virtual,
        T::Other => K::Other,
    }
}

fn file_badge_of(core: &core::SharedCore) -> (String, bool, bool) {
    let c = core.lock().unwrap();
    let name = c
        .path()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "（未命名）".to_owned());
    (
        format!("[{}] {name}", c.source_format().as_str()),
        c.is_dirty(),
        c.path().is_some(),
    )
}

/// 启动页（谱面列表）的**标题**（尺寸与编辑页共用，见下）。
///
/// **两个页面共用一个窗口尺寸**（用户要求："将启动页的窗口大小调整为编辑器大小。
/// 来回切换时不更改窗口大小"）。切换只换**标题**，不再发 `InnerSize` ——
/// 改尺寸会重建 wgpu surface 并让整窗跳一下，而那只是"换了一屏内容"，不是换了个程序。
///
/// 启动页上的弹窗（新建谱面 / 缺少 7z）本来就是**模态**：底下那屏照画，尺寸更不该动。
const LAUNCH_TITLE: &str = "OpenPhM — 选择谱面";

/// 启动页列表快照的最长寿命（秒）。
///
/// 列表里唯一会随时间变的只有"[格式] 多久以前"这一行文字，所以按秒表重算即可 ——
/// 一天按帧重算，只为了把"刚刚"改成"1 分钟前"是不划算的。
const LIST_ROWS_MAX_AGE: u64 = 30;

/// 统计量最短发布间隔：这些是**诊断量**，10 Hz 足够人（和 agent）看，
/// 没必要每帧排一次序、抢几次锁（见 `App::publish_stats`）。
const STATS_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// 编辑期把文档快照写回解压缓存的**最短间隔**（见 `App::maybe_snapshot`）。
///
/// 2 秒是个取舍：被强杀时最多只丢两秒的操作，而写的是**谱面 JSON**（几十 KB 级，资源不动），
/// 落在 `/tmp` 这种内存盘上几乎不花时间。**没做成"每次改动都写"**：那样每个按键都要序列化
/// 整份文档，而它防的是"崩溃"，不是"断电"。
const SNAPSHOT_MIN_INTERVAL: Duration = Duration::from_secs(2);

/// 启动阶段：先在**自己的窗口**里解决"选哪份谱面"，选完才进编辑页。
///
/// 关于"为什么不真的是两个并存窗口"：eframe 里只有根视口跑 pass（子视口都在根的 pass 里画），
/// 关掉根 = 退出、隐藏根 = 完全不跑 pass。所以"启动窗口关掉、编辑窗口留下"这套在 eframe 里做不到；
/// 启动页与编辑页因此是**同一个窗口的两个页面**：标题不同，**尺寸相同**（切换不发 `InnerSize`）。
/// 启动页内部的那些问题（新建谱面、缺 7z）则是**模态**，见 `dialog`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchPhase {
    /// 起始界面（谱面列表；缺 7z 时上面盖一层关不掉的门槛模态）
    StartScreen,
    /// 编辑页
    Editor,
}

/// 文件对话框里按下按钮 → 真正要执行的动作（在 UI 之外执行，因为系统对话框是阻塞的）
#[derive(Clone, Debug, PartialEq, Eq)]
enum FileAction {
    OpenDialog,
    SaveAsDialog,
    /// 写进**当前指定的保存目标**（空则弹保存窗口）
    SaveToTarget,
    /// 把曲名写进 `meta.name`
    ApplyName,
    /// 新建谱面（有未保存改动时先守卫）
    NewDoc,
    Reveal,
    Close,
}

/// 未保存守卫里用户选了哪个（Krita 的三个选项）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GuardChoice {
    Save,
    Discard,
    Cancel,
}

/// 未保存守卫要放行的动作（用户选「保存」或「不保存」之后继续做这件事）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GuardAction {
    NewDoc,
    OpenDialog,
    /// 退出程序（关窗）—— **唯一没有撤销机会**的那个动作
    Quit,
}

/// 「上次没有正常退出」这次要问的是哪一份遗留缓存。
///
/// `others` = 更旧的遗留份数（只报个数：本次只处理最新那份，别的留给用户下次决定）。
#[derive(Clone, Debug)]
struct ResumeOffer {
    item: opm_app::session::Leftover,
    others: usize,
}

/// 启动耗时探针：把"进程启动 → 首帧画完"之间每一步的**累计**与**本步**耗时打出来。
///
/// 为什么要它：启动慢的原因靠猜十有八九猜错（字体解析？7z 探测？Vulkan 初始化？窗口映射？），
/// 而这条链路跨了 `main` 与首帧两处，只有打点才能分辨。默认关（`--trace-startup` / `OPM_TRACE_STARTUP=1`）。
struct Trace {
    t0: std::time::Instant,
    last: std::time::Instant,
    on: bool,
}

impl Trace {
    fn new(on: bool) -> Self {
        let now = std::time::Instant::now();
        Self { t0: now, last: now, on }
    }
    /// 打一个点（`what` 写"这一步干了什么"）
    fn mark(&mut self, what: &str) {
        if !self.on {
            return;
        }
        let now = std::time::Instant::now();
        println!(
            "  启动耗时          : +{:7.1} ms（本步 {:7.1} ms）  {}",
            (now - self.t0).as_secs_f64() * 1000.0,
            (now - self.last).as_secs_f64() * 1000.0,
            what
        );
        self.last = now;
    }
}

/// `OPM_KEY_AUTO` 的一小片语法：`[ctrl+][shift+]键名` → `(修饰键, egui::Key)`。
///
/// 只认够用的那几个（这是自动化钩子，不是键盘映射表）：`ctrl` / `shift` / `alt` 前缀，
/// 键名走 `egui::Key` 的名字（`Delete`、`Z`、`A`…大小写都收）。
fn parse_key_spec(spec: &str) -> (egui::Modifiers, Option<egui::Key>) {
    let mut mods = egui::Modifiers::NONE;
    let mut name = spec.trim();
    loop {
        let low = name.to_ascii_lowercase();
        if let Some(rest) = low.strip_prefix("ctrl+") {
            mods.ctrl = true;
            mods.command = true;
            name = &name[name.len() - rest.len()..];
        } else if let Some(rest) = low.strip_prefix("shift+") {
            mods.shift = true;
            name = &name[name.len() - rest.len()..];
        } else if let Some(rest) = low.strip_prefix("alt+") {
            mods.alt = true;
            name = &name[name.len() - rest.len()..];
        } else {
            break;
        }
    }
    let key = egui::Key::ALL
        .iter()
        .find(|k| format!("{k:?}").eq_ignore_ascii_case(name.trim()))
        .copied();
    (mods, key)
}

fn main() -> eframe::Result<()> {
    let mut trace = Trace::new(
        std::env::var("OPM_TRACE_STARTUP").is_ok_and(|v| !v.trim().is_empty() && v.trim() != "0"),
    );
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    trace.mark("日志系统就绪");
    // 解析是纯函数（`cli::parse`）：打印警告、打用法、退出都由这里做
    let parsed = cli::parse(&std::env::args().skip(1).collect::<Vec<_>>());
    for w in &parsed.warnings {
        eprintln!("  ⚠️ {w}");
    }
    if parsed.help {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        println!("{}", cli::USAGE);
        return Ok(());
    }
    let args = parsed.args;
    if args.trace_startup {
        trace.on = true;
    }
    trace.mark("参数解析");
    // --fonts：只做字体自检，不开窗口。给 agent/CI（尤其是 Windows —— 那边没有 fontconfig）
    // 一条**可执行**的证据："中文会不会变豆腐块"。缺字形时退出码 3。
    // 放在启动横幅**之前**：这样输出就是干净的两行，不用让人从横幅里挑。
    if args.fonts {
        let cov = fonts::check();
        println!("CJK 字体          : {}", cov.summary());
        println!("字体许可          : {}", fonts::LICENSE_NOTE);
        if !cov.missing.is_empty() {
            std::process::exit(3);
        }
        return Ok(());
    }
    println!("== OpenPhM UI 骨架 ==");
    println!(
        "  音符={} 长度={:.1}s 前瞻={}s 压力模式={} 限帧={:?} 强制缩放={:?} bench={}",
        args.notes,
        args.notes as f64 / 12.0,
        args.lookahead,
        args.stress,
        args.fps_cap,
        args.scale,
        args.bench
    );
    println!("  空闲重绘={} fps（0=纯事件驱动）  bench 空闲测量={}s", args.idle_fps, args.idle_seconds);

    // --audio-probe：只解码，不开窗口。给 agent 一条"这个文件能不能用、是什么格式"的路。
    if let Some(p) = &args.audio_probe {
        match audio::probe(std::path::Path::new(p)) {
            Ok(info) => {
                println!("{}", serde_json::to_string_pretty(&info).unwrap_or_default());
                return Ok(());
            }
            Err(e) => {
                eprintln!("解码失败: {e}");
                std::process::exit(2);
            }
        }
    }

    trace.mark("启动横幅（stdout）");
    // ---- **单会话**（用户要求：同一时刻最多一个）----
    //
    // 抢的是解压缓存根目录上的**独占锁**（`<临时目录>/opm/.session.lock`）。为什么必须独占：
    // 缓存是"一个进程至多留一份、退出即清、切换即删"的东西，两个会话同时跑会互相删对方正在用的
    // 那份。锁用 `File::try_lock`（Unix `flock` / Windows `LockFileEx`）：**锁随句柄存在**，
    // 进程被强杀时由内核释放 —— 不需要 pid 存活检测，也不会留下"假的活锁"。
    //
    // 抢不到**不等于**放弃：本实例会开一个窗口，用关不掉的模态说清"已经有一个在跑"，
    // 而**什么都不碰**（不载入文档、不占控制 socket、退出也不清理任何东西）。
    let cache_root = opm_app::codec::container::cache_root();
    let mut session_lock = None;
    let mut busy_who: Option<opm_app::codec::container::Session> = None;
    match opm_app::session::acquire(&cache_root) {
        Ok(lock) => {
            println!("  会话锁            : {}（同一时刻只允许一个会话）", lock.path().display());
            session_lock = Some(lock);
        }
        Err(opm_app::session::Refused::Busy(who)) => {
            println!("  会话锁            : 已被占用 —— 本实例什么都不碰（见窗口里的提示）");
            busy_who = Some(who.unwrap_or_default());
        }
        Err(opm_app::session::Refused::Io(e)) => {
            // 保证不了独占就别动缓存（那比"多开一个"更危险）
            eprintln!("会话锁获取失败: {e}");
            eprintln!("按「同一时刻只允许一个会话」的约定，本实例不启动。");
            std::process::exit(4);
        }
    }
    // 上一轮**没退干净**的遗留缓存：只有 GUI 留下的那份才算（`opm-ctl` 的缓存按设计不清理）。
    // 判定在库 `session` 里（有单测），这里只把结果交给界面。
    let mut leftovers = Vec::new();
    if session_lock.is_some() {
        let unknown = opm_app::session::unidentified(&cache_root);
        if !unknown.is_empty() {
            println!("  遗留缓存          : {} 份出处不明（命令行或旧版本留下的），本次不动它们", unknown.len());
        }
        leftovers = opm_app::session::gui_leftovers(&cache_root);
        if let Some(l) = leftovers.first() {
            println!(
                "  遗留缓存          : {} 份上次没退干净的（最新：{}）—— 启动时问用户要不要继续",
                leftovers.len(),
                l.headline()
            );
        }
    }
    // 编辑文档：--doc 载入真实 opm 文件，否则按 --notes 生成演示谱面
    let doc_arg = args.doc.clone().filter(|_| session_lock.is_some() && busy_who.is_none());
    let core0 = match &doc_arg {
        Some(path) => match core::EditCore::load(std::path::Path::new(path)) {
            Ok(s) => {
                println!("  已载入文档        : {path}");
                s
            }
            Err(e) => {
                eprintln!("载入 {path} 失败: {e}");
                std::process::exit(2);
            }
        },
        None => core::EditCore::new(),
    };
    trace.mark("文档核心就绪（--doc 时含读盘+解析）");
    // ---- 起始界面（谱面列表）----
    // 打开程序先显示它；`--doc` 直接进编辑器（CLI/agent/截图都走那条），无头用途（bench/stress）也不显示。
    let mut recents = recents::Recents::load_default();
    let dropped = recents.prune();
    if dropped > 0 {
        println!("  最近打开          : 清理 {dropped} 条已不存在的记录");
    }
    trace.mark("最近打开列表（读盘 + 清理不存在的条目）");
    // ---- 启动检查：7z 能不能**调用**（opm 容器靠它打包/解包）----
    // 检查的是"能不能真的跑起来"，不是"文件在不在"：装了一半、权限不对、架构不符都会露出来。
    // 无头用途（bench/stress）不弹窗，只打一行日志。
    let seven_zip_missing = match zip::seven_zip_program() {
        Some(p) => {
            println!("  7z                : {}（opm 容器用系统 7z 打包）", p.display());
            None
        }
        None => {
            let msg = format!("没找到可调用的 7z（7z/7za/7zr）。{}", zip::install_hint());
            println!("  7z                : 不可用 —— {msg}");
            if args.bench_only() || args.stress {
                None
            } else {
                Some(msg)
            }
        }
    };
    trace.mark("7z 探测（起一次 `7z i` 真跑一遍）");
    let mut core0 = core0;
    if doc_arg.is_none() && args.notes > 0 && busy_who.is_none() {
        // 演示谱面也**走命令**（实现在库里 `opm_app::demo`）：文档只有 EditCore 能写，
        // 客户端（GUI/CLI/测试）一律发命令。这条路径此前是"直接改 doc"的最后一块飞地，
        // 现在没了 —— 由私有字段在编译期兜住。
        let t0 = std::time::Instant::now();
        let built = opm_app::demo::build_demo_via_core(&mut core0, args.notes);
        for r in &built.failed {
            eprintln!("  演示谱面命令失败: {}", cmd::response_line(r));
        }
        println!(
            "  演示谱面          : {} 音符 / {} 条命令（走 EditCore 命令路径，{:.2}s）{}",
            args.notes,
            built.commands,
            t0.elapsed().as_secs_f64(),
            if built.failed.is_empty() {
                String::new()
            } else {
                format!("，{} 条失败", built.failed.len())
            }
        );
    }
    let mut state = EditorState::new(headless::chart_from_doc(core0.doc()));
    // 自动化钩子（截图/自检用）：`OPM_EDIT_AUTO=hold:<lane>,<start>,<end>` 或
    // `OPM_EDIT_AUTO=event:<track>,<start>,<end>` —— 直接把"正在跟随鼠标的草稿"摆出来：
    // 没人能往窗口里注入按键，这是唯一能把它拍下来的办法。
    // 与 `OPM_LAUNCH_AUTO` 同类：**只在启动时读一次**，不影响交互路径。
    if let Ok(spec) = std::env::var("OPM_EDIT_AUTO") {
        let spec = spec.trim().to_owned();
        let nums = |rest: &str| -> Vec<f64> {
            rest.split(',').filter_map(|t| t.trim().parse().ok()).collect()
        };
        if let Some(rest) = spec.strip_prefix("hold:") {
            let n = nums(rest);
            if n.len() == 3 {
                state.begin_pending_hold(n[0] as f32, n[1]);
                if let Some(h) = state.pending_hold.as_mut() {
                    h.end_beat = n[2];
                }
                println!(
                    "  待放置 hold       : lane {:.0}，{:.2} → {:.2} 拍（OPM_EDIT_AUTO）",
                    n[0], n[1], n[2]
                );
            }
        } else if let Some(rest) = spec.strip_prefix("event:") {
            let mut it = rest.splitn(2, ',');
            let track = it.next().unwrap_or("").trim().to_owned();
            let n = nums(it.next().unwrap_or(""));
            let id = state::TrackId::from_key(&track);
            match (id, n.len()) {
                (Some(id), 2) => {
                    state.begin_pending_event(id, n[0]);
                    if let Some(e) = state.pending_event.as_mut() {
                        e.end_beat = n[1];
                    }
                    println!(
                        "  待放置事件        : {}，{:.2} → {:.2} 拍（OPM_EDIT_AUTO）",
                        id.key(),
                        n[0],
                        n[1]
                    );
                }
                (None, _) => eprintln!("  ⚠️ OPM_EDIT_AUTO：未知轨道 {track:?}（可选 moveX/moveY/rotate/alpha/speed）"),
                (_, _) => eprintln!("  ⚠️ OPM_EDIT_AUTO=event: 需要 `<track>,<start>,<end>`"),
            }
        } else {
            eprintln!("  ⚠️ OPM_EDIT_AUTO：只认 hold:<lane>,<start>,<end> 或 event:<track>,<start>,<end>");
        }
    }
    state.show_boundary = args.boundary;
    state.line_half_w = (args.line_len * 0.5).max(1.0);
    // 叠加层的开关与可见拍数是**视图状态**（不进文档），放在 EditorState 里统一管理
    state.overlay_enabled = args.overlay;
    // 与 Ctrl+滚轮/`{"op":"zoom"}` 同一个夹取范围：三处口径必须一致，否则 ui_stats 报的可见拍数
    // 会和画出来的窗口对不上（绘制里还有一层 `.max(4.0)` 兜底，那是防御不是口径）
    state.overlay_beats = args
        .overlay_beats
        .clamp(state::EditorState::ZOOM_MIN_BEATS, state::EditorState::ZOOM_MAX_BEATS);
    if let Some(d) = args.beat_div {
        state.grid.beat_div = d.clamp(1, 64);
    }
    if let Some(d) = args.lane_div {
        state.grid.lane_div = state::GridCfg::normalize_lane_div(d);
    }
    state.set_window_offset_x(args.window_offset);
    // GUI 只订阅，不写：拿到广播才更新自己那一小块
    let sub = core0.subscribe(broadcast::TopicFilter::all());
    let meta_name = core0.doc().meta.name.clone();
    let doc_lines = core0.doc().judge_lines.len();
    let doc_notes = core0.doc().note_count();

    trace.mark("演示谱面 / 首帧前状态");
    // 共享编辑会话：GUI 线程 + 控制通道线程操作同一份文档
    let shared = core::shared(core0);
    let stats = control::ui_stats();
    let view = control::view_queue();

    // ---- 音频：--audio 优先，否则用谱面 meta.audio（相对谱面目录）----
    let audio = match resolve_audio(&args, &shared) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("  音频未载入        : {e}");
            None
        }
    };
    if let Some(a) = &audio {
        a.set_offset_ms(args.audio_offset_ms);
        println!(
            "  音频              : {}（{}，{:.1}s）",
            a.path,
            a.device(),
            a.duration()
        );
        // 输出延迟由**首个回调**用 cpal 时间戳自校准，此刻还没回调，打印 0.0 会误导
        println!("  音频输出延迟      : 待首帧回调自校准（ui_stats.audio_latency_ms 可读）");
    }
    // 远端改动后唤醒一帧，否则空闲心跳（默认 1 fps）下要等一秒才看到变化。
    // egui 的 Context 要等首帧才拿得到，所以先给控制线程一个"插槽"，由 App 首次出帧时填上。
    let ctx_slot: Arc<std::sync::Mutex<Option<egui::Context>>> =
        Arc::new(std::sync::Mutex::new(None));
    let waker: control::RepaintWaker = {
        let slot = ctx_slot.clone();
        let st = stats.clone();
        Arc::new(move || {
            // 记下"我请求重绘了"的时刻，GUI 那帧应用到广播时算出唤醒延迟
            if let Ok(mut g) = st.lock() {
                g.wake_at = Some(Instant::now());
                g.wakes += 1;
            }
            if let Ok(g) = slot.lock() {
                if let Some(c) = g.as_ref() {
                    c.request_repaint();
                }
            }
        })
    };
    if args.verbose_updates {
        shared.lock().unwrap().set_verbose(true);
    }
    // **时间轴总长按乐曲时长**：音乐一装上就把时长交给视图状态（见 `EditorState::timeline_duration`）。
    // 放在这里（音频解析之后、`App::new` 之前）是因为顺序就是依赖：没有音频就没有"乐曲时长"这个概念。
    state.set_music_len(audio.as_ref().map(|a| a.duration()));
    if audio.is_some() {
        println!(
            "  时间轴总长        : {:.1}s（按乐曲时长；谱面自身 {:.1}s）",
            state.timeline_duration(),
            state.chart.duration
        );
    }
    let mut ctrl_path: Option<std::path::PathBuf> = None;
    // 已经有会话在跑时不接控制通道：那个 socket 是**已有实例**的（`opm-ctl --attach` 打的就是它），
    // 这里抢过来会让正在跑的那个失去远程入口
    if let Some(spec) = args.control.as_ref().filter(|_| session_lock.is_some()) {
        let path = if spec == "auto" {
            control::auto_path()
        } else {
            std::path::PathBuf::from(spec)
        };
        match control::spawn_server(shared.clone(), stats.clone(), view.clone(), waker, &path) {
            Ok(p) => {
                println!("  {}", control::describe(&p, "已启用"));
                ctrl_path = Some(p);
            }
            Err(e) => eprintln!("  控制通道启动失败: {e}"),
        }
    }

    trace.mark("音频（--audio/谱面 meta.audio：解码 + 开输出设备）");
    // 窗口**一开始就是编辑器尺寸**（两页共用），只有标题按启动阶段给：启动页是"选择谱面"，
    // 进了编辑页再换成"曲名（文件）"（见 `enter_editor`）。
    let on_launcher = doc_arg.is_none() && !args.bench_only() && !args.stress;
    let launch_phase = if busy_who.is_some() {
        // 被"已经有一个会话"挡住：走的是一屏独立提示（`App::busy_page`），阶段无所谓，但别进编辑页
        LaunchPhase::StartScreen
    } else if on_launcher {
        LaunchPhase::StartScreen
    } else {
        LaunchPhase::Editor
    };
    let (title, size) = match launch_phase {
        // 缺 7z 时标题直接说明门槛是什么（尺寸仍是启动页那一套：底下那屏照画，只是盖了模态）
        LaunchPhase::StartScreen => (
            if busy_who.is_some() {
                "OpenPhM — 已经有一个会话在运行"
            } else if seven_zip_missing.is_some() {
                "OpenPhM — 缺少 7-Zip"
            } else {
                LAUNCH_TITLE
            },
            egui::vec2(args.width, args.height),
        ),
        LaunchPhase::Editor => ("OpenPhM", egui::vec2(args.width, args.height)),
    };
    let mut vp = egui::ViewportBuilder::default()
        .with_inner_size(size)
        .with_title(title);
    if let Some((x, y)) = args.pos {
        vp = vp.with_position([x, y]);
    }
    trace.mark("控制通道 / GPU 策略 / 窗口参数");
    let trace_on = trace.on;
    let trace_t0 = trace.t0;
    let mut options = eframe::NativeOptions {
        viewport: vp,
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    // ---- **平台契约**：只有 Linux 插手显卡选择，别的平台保持默认选择器 ----
    //
    // 一处判定（`gpu::manages_gpu`），下面三处都用它：ICD 白名单 / 后端集合 / 适配器选择器。
    // Windows 上"程序用哪块卡"完全交给系统那一套；唯一越过这条线的是用户自己设 `OPM_GPU`。
    // `OPM_GPU_PLATFORM=windows` 是诊断钩子：在 Linux 上把下面整段当成 Windows 跑一遍。
    let gpu_linux = opm_app::gpu::manages_gpu(cfg!(target_os = "linux"), &|k| std::env::var(k).ok());
    if !gpu_linux {
        println!("  显卡平台          : 非 Linux：不改 ICD、不改后端集合、不装适配器选择器（用平台默认的显卡选择器）");
    }
    // ---- Wine：把"看着像故障"的那行日志先说清楚 ----
    //
    // Wine 的 `dxgi.dll` **没实现 `IDXGIFactoryMedia`**（GUID 41e7d1f2-a591-4f7b-a2e5-fa9c843e1c12），
    // 而 wgpu-hal 建 DX12 实例时会**试探性**取一次这个接口、失败时先 `log::error!` 再被 `.ok()` 丢掉 ——
    // 于是 stderr 上会出现一行 `create_factory_media failed: 0x80004002`（E_NOINTERFACE）。
    // **无害**：那个接口只用于"合成表面"（`SurfaceTarget::SurfaceHandle`）那条呈现路径，我们走的是 HWND；
    // 真 Windows 8+ 上它存在，这行根本不会出现。见《框架选型》§7.47。
    if under_wine() {
        println!(
            "  图形环境          : Wine（其 dxgi 未实现 IDXGIFactoryMedia）—— 若下面出现 \
             `create_factory_media failed: 0x80004002`，那是 wgpu 的一次**可选**探测，已被丢弃、不影响渲染"
        );
    }
    // ---- **不打扰独显**：默认把 NVIDIA 的 Vulkan ICD 从枚举里摘掉 ----
    //
    // 实测（`/sys/.../power/runtime_status`）：光启动一次程序，运行时断电的 NVIDIA 独显就会
    // 从 `suspended` 变 `active` —— 因为 Vulkan loader 会把目录里**所有** ICD 都加载起来，
    // 而"唤醒一块独显"本身要几百毫秒。用户根本没打算用它。
    // 规则见 `gpu::icd_plan`：只在"Linux + 用户没指定 ICD + 没显式要独显 + 本机还有别的 ICD"时才摘。
    {
        let env = |k: &str| std::env::var(k).ok();
        // 非 Linux：连目录都不扫（那边根本不查 Vulkan ICD）
        let icd_files: Vec<String> = if gpu_linux {
            opm_app::gpu::icd_search_paths(&env)
                .iter()
                .filter(|d| d.is_dir())
                .flat_map(|d| {
                    std::fs::read_dir(d)
                        .map(|rd| {
                            rd.flatten()
                                .map(|e| e.path())
                                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .collect()
        } else {
            Vec::new()
        };
        let plan = opm_app::gpu::icd_plan(gpu_linux, &env, &icd_files);
        match &plan.keep {
            Some(keep) => {
                println!(
                    "  图形 ICD          : {}（保留 {} 个：{}）",
                    plan.reason,
                    keep.len(),
                    keep.iter()
                        .filter_map(|p| std::path::Path::new(p).file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("、")
                );
                // 只认 `VK_DRIVER_FILES`（loader 的新名字）：这一步必须在建 wgpu 实例**之前**做
                std::env::set_var("VK_DRIVER_FILES", keep.join(":"));
            }
            None => println!("  图形 ICD          : {}（不干预）", plan.reason),
        }
    }

    // ---- 后端集合：**能确定 Vulkan 可用就只开 Vulkan**（省掉 GL 的初始化，实测 ~150 ms）----
    //
    // 探测本身**不触发初始化**（只看 ICD json 与 loader 库在不在），探测不到就保留全后端 ——
    // GL 回退在本会话里真的救过场（Vulkan 里没有可用卡那次）。开关：`OPM_BACKEND=vulkan|all`；
    // 用户设了 `WGPU_BACKEND` 时一切照旧（不抢 wgpu 自己的开关）。
    {
        let env = |k: &str| std::env::var(k).ok();
        // 非 Linux：不探测（`libvulkan.so.1` 那几条路径在别的平台没有意义，走不到 VulkanOnly）
        let (icd, loader) = if gpu_linux {
            // 显式指定的 ICD 路径要么都在、要么不信（见 `gpu::vulkan_icd_usable`）
            let explicit: Vec<bool> = ["VK_DRIVER_FILES", "VK_ICD_FILENAMES"]
                .iter()
                .filter_map(|k| std::env::var(k).ok())
                .flat_map(|v| {
                    v.split(':')
                        .filter(|p| !p.trim().is_empty())
                        .map(|p| std::path::Path::new(p.trim()).is_file())
                        .collect::<Vec<_>>()
                })
                .collect();
            let default_json = opm_app::gpu::icd_search_paths(&env)
                .iter()
                .any(|p| p.is_dir() && dir_has_json(p));
            let loader = opm_app::gpu::loader_candidates()
                .iter()
                .any(|p| std::path::Path::new(p).is_file());
            (opm_app::gpu::vulkan_icd_usable(&explicit, default_json), loader)
        } else {
            (false, false)
        };
        let (plan, why) = opm_app::gpu::backend_plan(gpu_linux, &env, icd, loader);
        println!("  图形后端          : {}（{why}）", plan.label());
        if plan == opm_app::gpu::BackendPlan::VulkanOnly {
            if let eframe::egui_wgpu::WgpuSetup::CreateNew(cfg_new) =
                &mut options.wgpu_options.wgpu_setup
            {
                cfg_new.instance_descriptor.backends = eframe::wgpu::Backends::VULKAN;
            }
        }
    }

    // ---- 显卡策略：**默认走核显**，独显只在显式指定时才用（见 `opm_app::gpu`）----
    //
    // 为什么要自己选：wgpu 的默认电源偏好是 `HighPerformance` ⇒ 什么都不设时会去开独显，
    // 而编谱这种 2D 活儿核显足够，独显白烧功耗与发热（用户明确要求）。
    // 选择器拿到的是**全部候选适配器**，所以能按策略挑，并把决定打出来（谁都能核对）。
    //
    // 边界（实测）：**"一个后端都用不了"轮不到这个选择器** —— wgpu 先要建 surface，
    // 没有可用后端时 eframe 直接报 `FailedToCreateSurfaceForAnyBackend` 并以可读错误退出；
    // 这里的选择器只在"有适配器可挑"时运行（候选全部不能出图到 surface 时会返回下面那句 Err）。
    let (gpu_policy, gpu_why) = opm_app::gpu::policy_from_env(gpu_linux, &|k| std::env::var(k).ok());
    println!("  显卡策略          : {}", opm_app::gpu::describe(gpu_policy, gpu_why));
    if gpu_policy != opm_app::gpu::GpuPolicy::Default {
        if let eframe::egui_wgpu::WgpuSetup::CreateNew(cfg_new) = &mut options.wgpu_options.wgpu_setup
        {
            let sel_t0 = trace_t0;
            cfg_new.native_adapter_selector = Some(std::sync::Arc::new(
                move |adapters: &[eframe::wgpu::Adapter],
                      surface: Option<&eframe::wgpu::Surface<'_>>| {
                    if trace_on {
                        println!(
                            "  启动耗时          : +{:7.1} ms  适配器枚举完成（wgpu 实例 + Vulkan loader 之后）",
                            (std::time::Instant::now() - sel_t0).as_secs_f64() * 1000.0
                        );
                    }
                    // 只考虑"能出图到这个 surface"的适配器（选一个不能呈现的等于自找黑屏）
                    let usable: Vec<usize> = adapters
                        .iter()
                        .enumerate()
                        .filter(|(_, a)| surface.map(|s| a.is_surface_supported(s)).unwrap_or(true))
                        .map(|(i, _)| i)
                        .collect();
                    for (i, a) in adapters.iter().enumerate() {
                        let info = a.get_info();
                        println!(
                            "  适配器候选        : [{}] {} [{:?}/{:?}]{}",
                            i,
                            info.name,
                            info.backend,
                            info.device_type,
                            if usable.contains(&i) { "" } else { "（不能出图到这个 surface）" }
                        );
                    }
                    let kinds: Vec<opm_app::gpu::GpuKind> =
                        usable.iter().map(|i| gpu_kind_of(adapters[*i].get_info().device_type)).collect();
                    let pick = opm_app::gpu::pick_index(gpu_policy, &kinds)
                        .ok_or_else(|| "没有可用的图形适配器".to_owned())?;
                    let chosen = usable[pick];
                    let info = adapters[chosen].get_info();
                    println!(
                        "  显卡选用          : {} [{:?}/{:?}]（{}）",
                        info.name,
                        info.backend,
                        info.device_type,
                        gpu_policy.label()
                    );
                    if trace_on {
                        println!(
                            "  启动耗时          : +{:7.1} ms  适配器选择完成（设备/表面还没建）",
                            (std::time::Instant::now() - sel_t0).as_secs_f64() * 1000.0
                        );
                    }
                    Ok(adapters[chosen].clone())
                },
            ));
        }
    }

    trace.mark("准备完毕，交棒给 eframe::run_native（窗口创建 + wgpu 初始化 + 首帧）");
    let a = args.clone();
    let cleanup_core = shared.clone();
    let r = eframe::run_native(
        "opm-app",
        options,
        Box::new(move |_cc| {
            let t_app = std::time::Instant::now();
            let app = App::new(
                state, shared, stats, sub, ctx_slot, audio, view, meta_name, doc_lines, doc_notes, a,
                recents, seven_zip_missing, launch_phase, busy_who, leftovers,
            );
            if trace_on {
                println!(
                    "  启动耗时          : +{:7.1} ms（本步 {:7.1} ms）  App::new（创建回调里）",
                    (t_app - trace_t0).as_secs_f64() * 1000.0,
                    t_app.elapsed().as_secs_f64() * 1000.0
                );
            }
            let mut app = app;
            app.startup_t0 = Some(trace_t0);
            app.trace_startup = trace_on;
            Ok(Box::new(app))
        }),
    );

    // ---- **退出时清理**（用户要求）----
    //
    // 把这次会话从容器里解压出来的那份删掉：`/tmp/opm/<key>` 在多数机器上是 **tmpfs（内存）**，
    // 用完就该还回去。（上一次异常退出留下的，由 `prune_cache` 按上限兜底。）
    //
    // 与"未保存的数据"无关：那是**文档**的事，已由未保存守卫按既定三选一处理
    // （保存 / 不保存 / 返回）—— 走到这里说明用户已经选过"不保存"或本来就没有改动。
    if let Some(dir) = cleanup_core.lock().ok().and_then(|c| c.asset_dir().map(std::path::Path::to_path_buf)) {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => println!("  解压缓存已清理    : {}", dir.display()),
            Err(e) => eprintln!("  解压缓存清理失败  : {}（{e}）", dir.display()),
        }
    }
    // 正常退出时收走自己的 socket（异常退出留下的由下次 bind 前清理 + 死进程过滤兜底）
    if let Some(p) = ctrl_path {
        let _ = std::fs::remove_file(p);
    }
    r
}

struct App {
    args: Args,
    state: EditorState,
    inited: bool,
    adapter: String,

    /// 每帧构建的实例（复用同一 Vec，避免每帧分配）
    instances: Vec<NoteInstance>,
    paint_us: Arc<AtomicU64>,

    // 诊断
    frames: u32,
    last_frame: Option<Instant>,
    deltas: Vec<f64>,
    ui_ms: Vec<f64>,
    build_ms: Vec<f64>,
    inst_counts: Vec<usize>,
    reported: bool,
    ws: Workspace,
    /// bench：活跃阶段结束后的空闲阶段起点与帧计数
    idle_start: Option<Instant>,
    idle_frames: u32,
    /// 首帧布局稳定前视为"工作中"，避免启动瞬间就被判定为空闲
    pending_layout_anim: bool,
    /// 共享编辑会话（GUI 与控制通道线程共同持有）
    core: core::SharedCore,
    /// **订阅句柄**：EditCore 改动后通过它投递 update 广播（只有 GUI 关心的话题会到达）
    sub: Option<broadcast::Subscription>,
    /// 累积的脏位（一帧内可能收到多条广播，合并后只重建一次）
    dirty: Dirty,
    /// 供控制通道读取的界面统计（证明"无关控件没被惊动"）
    stats: control::UiStatsHandle,
    /// 首次出帧后填上，给控制线程一个唤醒 egui 的把手
    ctx_slot: Arc<std::sync::Mutex<Option<egui::Context>>>,
    /// 音频输出（None = 没有音频，播放头退回墙钟）
    audio: Option<audio::Audio>,
    /// 控制通道投来的**视图命令**（播放/暂停/定位/换音频）
    view: control::ViewQueue,
    /// 播放头与**墙钟**的累计偏差（毫秒）与已累计的窗口长度（秒）。
    /// 刻意**不**只报 ppm：短窗口下一帧的采样抖动（4~16 ms）就能算出几千 ppm 的假数字
    /// （S2 spike 的文档里就警告过"混算成单一 ppm 会误导"）。偏差是**有界量**，窗口才是可解释的。
    audio_drift: Option<(Instant, f64)>,
    /// 跳过 1 秒稳定期后的锚点：速率必须用**斜率**，不能拿"起步瞬间"当基准 ——
    /// 起步时游标还没稳定推进，那里的一点固定偏差会被除以窗口算成几千 ppm 的假数字。
    audio_settle: Option<(Instant, f64)>,
    audio_dev_ms: f64,
    audio_window_s: f64,
    audio_rate_ppm: f64,
    /// `--autoplay`：首帧布局稳定后自动开始
    autoplay_pending: bool,
    /// 编辑区叠加层设置与按键状态
    overlay: OverlayCfg,
    /// 顶栏拖动框的临时值：真正生效要过 `EditorState::set_window_offset_x`（夹取 + 进 ui_stats）
    window_offset_x_ui: f32,
    /// 文件对话框（保存/另存为/打开）是否展开
    file_dialog_open: bool,
    /// **保存目标**（合并后的一个路径：文件夹 + 谱面名字 + 扩展名）。
    /// 空串 = 还没指定目标 ⇒ 保存时会弹保存窗口（Krita 的做法）。
    /// 保存目标的**只读显示文本**（不再是可编辑输入框：用户要求"保存就按已有目标存，
    /// 另存为/没有目标才弹选择窗"）。真值只在 `EditCore` 里，这里只是它的显示副本。
    target_text: String,
    /// 曲名（文档字段 `meta.name`）；新建下一份时的默认文件名也用它
    edit_name: String,
    /// **新建谱面表单**（启动页上的模态；值由这里持有，库只改它）
    new_form: opm_app::recents::NewChartForm,
    /// 启动页上的「新建谱面」模态是否打开（模态不换屏：底下的列表照画，只是被压暗且吞掉输入）
    new_form_open: bool,
    /// 待办的"回启动页并打开新建模态"（换窗口标题要用 `Context`，只能在帧里做）。
    /// 编辑页的「文件 → 新建…」走这条：谱面的建立只发生在启动页（那里才有填写表信息的地方）。
    pending_launch_new: bool,
    /// 未保存守卫：有未保存改动时要执行的动作，等用户选「保存｜不保存｜返回」
    guard_for: Option<GuardAction>,
    /// 空格键状态机：**单点＝进入/退出自动播放，长按＝松手退出**（规则见 `keymap`，有单测）
    space_play: keymap::SpacePlayback,
    /// 启动时的 7z 检查结果：`None` = 可用；`Some(探测到的说明)` = 不可用（单独的提示窗口）
    seven_zip_missing: Option<String>,
    /// 启动阶段（见 [`LaunchPhase`]）
    phase: LaunchPhase,
    /// 启动耗时探针：进程启动的时刻（`--trace-startup` 时才用）
    startup_t0: Option<Instant>,
    /// 启动耗时探针是否开着（`--trace-startup` / `OPM_TRACE_STARTUP=1`）
    trace_startup: bool,
    /// `OPM_LAUNCH_AUTO` 的值（**启动时读一次**）：启动页用它替人做选择（截图/CI）。
    /// 放在字段里而不是每帧 `env::var` —— 那是纯粹的启动期钩子，帧里不该有 env 查询。
    launch_auto: Option<String>,
    /// `OPM_RESUME_AUTO=continue|discard|later`：遗留缓存对话框替人做选择（截图/CI 用）。
    /// 与 `launch_auto` 同类 —— **只在启动时读一次**。
    resume_auto: Option<String>,
    /// 「上次没有正常退出」这份待问的遗留缓存（`None` = 没有 / 已经问过）
    resume: Option<ResumeOffer>,
    /// 被"已经有一个会话在运行"挡住时那个持有者的身份（`None` = 没被挡）
    busy_who: Option<opm_app::codec::container::Session>,
    /// 上次把文档快照写回解压缓存的时刻（节流用；见 `App::maybe_snapshot`）
    snapshot_at: Instant,
    /// 上一次快照失败的原因（**只在变化时报一次**：失败会每两秒重试，不能每两秒刷一行日志）
    snapshot_err: Option<String>,
    /// `OPM_CLOSE_AUTO=<帧号>`：在那一帧**模拟点右上角的叉**（发一个 Close 请求）——
    /// 验证"未保存就退出"的守卫用（本会话没法往 Wayland 窗口注入点击）。
    close_auto: Option<u64>,
    /// 守卫放行后的退出待办（帧里执行，那里有 `Context`）
    pending_quit: bool,
    /// 我们自己已经发过 `Close` ⇒ 下一帧的 `close_requested()` 不再过守卫
    quit_allowed: bool,
    /// **当前装着的是哪一份音频**（`audio::spec` 的标识）。用它判断"要不要重新装载"：
    /// 每改一个字都会来一条 `meta` 广播，不能每次都真的开设备、解一遍音频。
    loaded_audio_spec: Option<String>,

    recents: recents::Recents,
    /// 启动页列表的**行快照**（显示什么在这里算好；帧里只画字符串）。
    ///
    /// 为什么不让 UI 每帧自己算：那要在帧里对每条记录 `is_file()`（最多 20 次 stat）并拼
    /// "[格式] 多久以前"。这两件事只跟"列表内容 / 当前时刻"有关，与帧无关 ——
    /// 见 [`opm_app::recents::list_rows`]。
    list_rows: Vec<recents::ListRow>,
    /// 上面那份快照是什么时刻算的（Unix 秒）：只为"多久以前"那一行文字，超过
    /// [`LIST_ROWS_MAX_AGE`] 秒才重算 —— 不是每帧重算。
    list_rows_at: u64,
    /// 上一次把统计写进共享槽的时刻（`publish_stats` 的节流）
    stats_at: Instant,
    /// 上一次发布时 `applied_broadcasts` 的值：涨了就说明有真实更新，立刻再发一次
    stats_published_broadcasts: u64,
    /// 文档标识（状态栏）：`[格式] 文件名`。**算一次存起来**，帧里只画这个字符串 ——
    /// 它原先要在帧里锁核心、`display().to_string()`、`rsplit('/')`，而这三样只在
    /// 打开/保存/换格式时才变。
    file_badge: String,
    /// 有没有未保存改动（与 `file_badge` 同一时刻刷新）
    file_dirty: bool,
    /// 有没有保存目标（没有 ⇒ 第一次保存会弹保存窗口，状态栏据此说人话）
    file_has_target: bool,
    /// 上一次文件操作的结果提示（文件对话框/新建模态里显示）
    file_message: Option<(bool, String)>,

    save_format: core::SaveFormat,
    h_held: bool,
    overlay_visible: bool,
    /// `OPM_KEY_AUTO` 的待办：`(第几帧, 按键表)`，`;` 分隔成多组。
    /// 到点注入一次就取走 —— 按键是"一次"，不是每帧（`30:Delete;60:ctrl+z` 这类序列靠它）。
    key_auto: Vec<(u32, String)>,
    /// 是否正处在一次拖拽事务中（结束时 commit）
    drag_active: bool,
    /// 正在拖的**组**（按下那一刻冻结的抓手）。
    ///
    /// 拖拽期间文档一直在变（每帧都发命令），所以"原来在哪"不能从文档反推 ——
    /// 抓手就是那份冻住的原点；结束（`GrabEnd`）就丢掉。
    grab: Option<opm_app::edit::Grab>,
    /// **事件重叠**列表：加载谱面时全量检测，之后每次改动只重查动过的那条线
    conflicts: Vec<cmd::Overlap>,
    /// 冲突浏览器面板是否展开（有冲突时自动展开）
    show_conflicts: bool,

    // ---- 各面板的派生缓存（只在对应脏位为真时重建）----
    /// 顶部摘要文本（Meta 脏）
    meta_name: String,
    doc_lines: usize,
    doc_notes: usize,
    /// 左侧「判定线」列表的行文本缓存（structure 脏时重建：线名/属性随时可变）
    line_rows: Vec<LineRow>,
    /// 检查器展示的选中对象快照（Inspector 脏时重建）
    insp: Option<Inspector>,

    // ---- 观测：广播与重建计数（**逐线**的代价在这里可见）----
    applied_broadcasts: u64,
    builds_structure: u64,
    builds_props: u64,
    builds_notes: u64,
    builds_tracks: u64,
    builds_meta: u64,
    builds_inspector: u64,
    skipped_structure: u64,
    skipped_props: u64,
    skipped_notes: u64,
    skipped_tracks: u64,
    skipped_inspector: u64,
    last_broadcast: String,
    last_topics: Vec<String>,
    last_structure_ms: f64,
    /// 发出命令的时刻：用于量"发出 → 广播被应用"的延迟
    pending_dispatch: Option<Instant>,
    latencies: Vec<f64>,
    /// 唤醒 → 应用 的延迟样本（不含命令本身的开销）
    wake_latencies: Vec<f64>,
    working_frames_total: u64,
    idle_frames_total: u64,

    show_console: bool,
    console_input: String,
    console_log: Vec<(bool, String)>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        state: EditorState,
        core: core::SharedCore,
        stats: control::UiStatsHandle,
        sub: broadcast::Subscription,
        ctx_slot: Arc<std::sync::Mutex<Option<egui::Context>>>,
        audio: Option<audio::Audio>,
        view: control::ViewQueue,
        meta_name: String,
        doc_lines: usize,
        doc_notes: usize,
        args: Args,
        recents: recents::Recents,
        seven_zip_missing: Option<String>,
        launch_phase: LaunchPhase,
        // 「已经有一个会话在运行」时那个持有者的身份（`None` = 没被挡住）
        busy_who: Option<opm_app::codec::container::Session>,
        // 上次没退干净的遗留缓存（新→旧）；启动页上问用户要不要继续
        leftovers: Vec<opm_app::session::Leftover>,
    ) -> Self {
        let args_ws = args.ws.unwrap_or(Workspace::Compose);
        // 顶栏拖动框的初值来自状态（`--window-offset` 已在这一步之前作用于 state）
        let state_window_offset = state.window_offset_x;
        // 启动时那份音频是按什么规格装的（避免第一条 meta 广播就白重载一次）。
        // 必须在结构体字面量**之前**算：`args`/`core` 会被移进结构体。
        let loaded_audio_spec = {
            let meta = core.lock().ok().and_then(|c| c.doc().meta.audio.clone());
            audio::spec(args.audio.as_deref(), meta.as_deref())
        };
        // `args` 随后被移动进结构体，先把"启动就摊开哪个对话框"取出来
        let dialog_at_start = args.dialog.clone();
        // 初始检查器快照：与广播后的刷新走同一个入口（读一次核心）
        let insp = core
            .lock()
            .ok()
            .and_then(|c| view::inspector_of(&state, c.doc()));
        // 状态栏那份文档标识：**构造时算一次**（`--doc FILE` 直接进编辑页这条路径不经过
        // `sync_file_fields`，漏了就会把已载入的文件显示成"尚未保存"）
        let (file_badge, file_dirty, file_has_target) = file_badge_of(&core);
        // **加载时全量检测事件重叠**（之后每次改动只查动过的那条线）
        let conflicts: Vec<cmd::Overlap> = core
            .lock()
            .map(|c| c.overlaps().to_vec())
            .unwrap_or_default();
        // 有冲突就自动把浏览器展开（用户不用先去找入口）
        let show_conflicts = !conflicts.is_empty();
        let autoplay = args.autoplay;
        let ov_enabled = args.overlay;
        let overlay = OverlayCfg {
            body_alpha: args.overlay_alpha,
            ..Default::default()
        };
        let line_rows = view::line_rows_of(&state.chart);
        // 启动页列表的行快照：在构造时算一次（之后由 `App::refresh_list_rows` 在事件上重算）
        let list_rows_at = recents::now_secs();
        let list_rows = recents::list_rows(&recents, list_rows_at);
        // 初始快照：不建的话启动后检查器会一直空着，直到第一次广播才出现内容
        Self {
            args,
            state,
            inited: false,
            adapter: String::new(),
            instances: Vec::new(),
            paint_us: Arc::new(AtomicU64::new(0)),
            frames: 0,
            last_frame: None,
            deltas: Vec::new(),
            ui_ms: Vec::new(),
            build_ms: Vec::new(),
            inst_counts: Vec::new(),
            reported: false,
            ws: args_ws,
            idle_start: None,
            idle_frames: 0,
            pending_layout_anim: true,
            core,
            sub: Some(sub),
            dirty: Dirty::default(),
            stats,
            ctx_slot,
            audio,
            view,
            audio_drift: None,
            audio_settle: None,
            audio_dev_ms: 0.0,
            audio_window_s: 0.0,
            audio_rate_ppm: 0.0,
            autoplay_pending: autoplay,
            overlay,
            // 必须**从状态取初值**：写死 0.0 的话，`--window-offset 400` 在第一帧就会被
            // 顶栏的同步逻辑当成"用户把拖动框拉回 0"而抹掉（这个坑当场实测到了）
            window_offset_x_ui: state_window_offset,
            file_dialog_open: dialog_at_start.as_deref() == Some("file"),
            target_text: String::new(),
            edit_name: String::new(),
            // 守卫要配合"脏文档"才有意义：启动时直接摆出来仅供截图检查
            space_play: keymap::SpacePlayback::default(),
            seven_zip_missing: seven_zip_missing.clone(),
            // `--dialog new` 时把阶段拨回启动页：那个模态只属于启动页（见下面的 `new_form_open`）
            phase: if dialog_at_start.as_deref() == Some("new") {
                LaunchPhase::StartScreen
            } else {
                launch_phase
            },
            recents,
            list_rows,
            list_rows_at,
            stats_at: Instant::now(),
            stats_published_broadcasts: 0,
            file_badge,
            file_dirty,
            file_has_target,
            startup_t0: None,
            trace_startup: false,
            // 启动期钩子：**只在这里读一次**（帧里不该有 env 查询）
            launch_auto: std::env::var("OPM_LAUNCH_AUTO").ok(),
            snapshot_at: Instant::now(),
            snapshot_err: None,
            // 启动期钩子：遗留缓存对话框怎么选（`continue|discard|later`）—— 与 `OPM_LAUNCH_AUTO`
            // 同类，只在启动时读一次，给 agent 一条"把这一步走完"的路（没人能替它点鼠标）
            resume_auto: std::env::var("OPM_RESUME_AUTO").ok(),
            resume: {
                // 新→旧：问最新那份；更旧的那些只报个数（本次不动它们）
                let mut it = leftovers.into_iter();
                let first = it.next();
                let others = it.count();
                first.map(|item| ResumeOffer { item, others })
            },
            busy_who,
            close_auto: std::env::var("OPM_CLOSE_AUTO")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok()),
            pending_quit: false,
            quit_allowed: false,
            // 启动时那份音频是按什么规格装的（记下来，避免第一条 meta 广播就白重载一次）
                loaded_audio_spec,
            guard_for: match dialog_at_start.as_deref() {
                Some("guard") => Some(GuardAction::NewDoc),
                _ => None,
            },
            new_form: opm_app::recents::NewChartForm::default(),
            pending_launch_new: false,
            // `--dialog new`：直接把"新建谱面"那个模态摆出来（截图/自检用）。
            // 它只在启动页上有得画（模态属于那一屏），所以顺手把阶段也拨过去 —— 否则带 `--doc`
            // 时会"给了参数却什么都没弹"。
            new_form_open: dialog_at_start.as_deref() == Some("new"),
            file_message: None,
            save_format: core::SaveFormat::Auto,
            h_held: false,
            overlay_visible: ov_enabled,
            key_auto: std::env::var("OPM_KEY_AUTO")
                .ok()
                .map(|s| {
                    // `[帧号:]按键[,按键]`，多组用 `;` 分隔；不写帧号就是第 1 帧
                    s.split(';')
                        .filter(|g| !g.trim().is_empty())
                        .map(|g| match g.split_once(':') {
                            Some((n, rest)) if n.trim().parse::<u32>().is_ok() => {
                                (n.trim().parse().unwrap(), rest.to_owned())
                            }
                            _ => (1, g.to_owned()),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            drag_active: false,
            grab: None,
            conflicts,
            show_conflicts,
            meta_name,
            doc_lines,
            doc_notes,
            line_rows,
            insp,
            applied_broadcasts: 0,
            builds_structure: 0,
            builds_props: 0,
            builds_notes: 0,
            builds_tracks: 0,
            builds_meta: 0,
            builds_inspector: 0,
            skipped_structure: 0,
            skipped_props: 0,
            skipped_notes: 0,
            skipped_tracks: 0,
            skipped_inspector: 0,
            last_broadcast: String::new(),
            last_topics: Vec::new(),
            last_structure_ms: 0.0,
            pending_dispatch: None,
            latencies: Vec::new(),
            wake_latencies: Vec::new(),
            working_frames_total: 0,
            idle_frames_total: 0,
            show_console: false,
            console_input: String::new(),
            console_log: Vec::new(),
        }
    }

    // ------------------------------------------------------------------ 广播

    /// 累计"音频时钟 vs 墙钟"的相对偏差（ppm）。
    /// 这不是误差校正 —— 播放头直接取音频位置，本来就不会漂；这里只是**测量**，
    /// 用来验证"音频确实是唯一时钟"（S2 spike 量到的同项是 7.4 ppm）。
    fn track_audio_clock(&mut self, pos: f64) {
        let now = Instant::now();
        match self.audio_drift {
            None => {
                self.audio_drift = Some((now, pos));
                self.audio_settle = None;
                self.audio_dev_ms = 0.0;
                self.audio_window_s = 0.0;
                self.audio_rate_ppm = 0.0;
            }
            Some((t0, _p0)) => {
                let wall = now.duration_since(t0).as_secs_f64();
                // 跳过前 1 秒：那一段包含"游标刚起步"的固定偏差，不是速率差
                if wall >= 1.0 && self.audio_settle.is_none() {
                    self.audio_settle = Some((now, pos));
                }
                if let Some((ts, ps)) = self.audio_settle {
                    let w = now.duration_since(ts).as_secs_f64();
                    let a = pos - ps;
                    self.audio_window_s = w;
                    self.audio_dev_ms = (a - w) * 1000.0;
                    self.audio_rate_ppm = if w >= 3.0 { (a - w) / w * 1e6 } else { 0.0 };
                }
            }
        }
    }

    /// 把命令交给 EditCore。**这里不更新任何视图缓存** —— 视图只认广播。
    /// 这就是"向 editcore 发送更新后等待 update 广播"的字面实现。
    fn dispatch(&mut self, cmds: &[serde_json::Value]) {
        let started = Instant::now();
        let (resps, _failed) = {
            let mut c = self.core.lock().unwrap();
            c.exec_batch(cmds)
        };
        for resp in &resps {
            let ok = resp.get("ok").and_then(|v| v.as_bool()) == Some(true);
            self.console_log.push((ok, cmd::response_line(resp)));
        }
        // 命令已受理，但界面还停在旧状态：等广播
        self.pending_dispatch = Some(started);
    }

    /// 施加编辑区叠加层产出的动作（面板本身不碰数据）
    fn apply_overlay_actions(&mut self, acts: Vec<OverlayAction>) {
        let mut sel_changed = false;
        let mut seek_to: Option<f64> = None;
        let mut cmds: Vec<serde_json::Value> = Vec::new();
        for a in acts {
            match a {
                OverlayAction::SelectNote(i) => {
                    self.state.select_note(i);
                    sel_changed = true;
                }
                // 框选：整批**替换**选区（空集 = 清空）—— 与点选走同一份状态
                OverlayAction::SelectNotes(v) => {
                    self.state.select_notes(v);
                    sel_changed = true;
                }
                // Ctrl+左键：切换单个（跨半区时选区会整体换成那一类，见 `Selection::toggle_*`）
                OverlayAction::ToggleNote(i) => {
                    self.state.toggle_note_selection(i);
                    sel_changed = true;
                }
                OverlayAction::SelectTrack(id) => {
                    if self.state.selected_track != id {
                        self.state.selected_track = id;
                        self.state.clear_event_selection();
                        sel_changed = true;
                    }
                }
                OverlayAction::SelectEvent(i) => {
                    self.state.select_event(self.state.selected_track, i);
                    sel_changed = true;
                }
                OverlayAction::SelectEvents(v) => {
                    // 框选可能横跨几条轨道：先把当前轨道挪到**其中最多的一条**上，
                    // 检查器才有东西可显示（锚会落到它里面，见 `set_events`）
                    if let Some((t, _)) = v.first() {
                        self.state.selected_track = *t;
                    }
                    self.state.select_events(v);
                    sel_changed = true;
                }
                OverlayAction::ToggleEvent(t, i) => {
                    self.state.toggle_event_selection(t, i);
                    sel_changed = true;
                }
                // 点空白：清空选区（选区是视图状态，不动文档、不进撤销栈）
                OverlayAction::ClearSelection => {
                    self.state.clear_selection();
                    sel_changed = true;
                }
                OverlayAction::SeekBeat(b) => {
                    seek_to = Some(self.state.chart.tmap.sec(b.max(0.0)));
                }
                OverlayAction::ScrollBeats(d) => {
                    // 相对挪动：按当前拍加增量再换算回秒（避免"秒→拍→秒"来回取整）
                    let beat = self.state.chart.tmap.beat(self.state.playhead) + d;
                    seek_to = Some(self.state.chart.tmap.sec(beat.max(0.0)));
                }
                OverlayAction::ZoomBeats(f) => {
                    // Ctrl+滚轮：纯视图缩放，**不进文档**（谱面里没有"可见拍数"这个字段）
                    self.state.zoom_by(f);
                }
                // ---- 组拖动：整段 = **一个撤销步**（逐条改动仍然广播，面板实时更新）----
                //
                // 抓手（`edit::Grab`）是按下那一刻冻结的原点；这一层只把它变成命令 ——
                // "抓谁、吸附到哪、夹在哪"全在库里（`edit.rs`，有单测）。
                OverlayAction::GrabStart(g) => {
                    self.drag_active = true;
                    self.grab = Some(*g);
                    let label = opm_app::edit::move_label(self.state.selection_kind().unwrap_or(opm_app::state::SelKind::Notes));
                    cmds.push(opm_app::edit::begin_command(label));
                }
                OverlayAction::GrabMove { d_lane, d_beat } => {
                    if let Some(g) = self.grab.clone() {
                        // 音符按住时长、事件保持时长这类规则不该只活在"拖一下看看"里
                        cmds.extend(opm_app::edit::move_grab_commands(&self.state, &g, d_lane, d_beat));
                    }
                }
                OverlayAction::GrabEnd => {
                    self.grab = None;
                    if self.drag_active {
                        self.drag_active = false;
                        cmds.push(opm_app::edit::commit_command());
                    }
                }
                OverlayAction::EventResizeStart => {
                    self.drag_active = true;
                    cmds.push(opm_app::edit::begin_command(opm_app::edit::DRAG_EVENT_LABEL));
                }
                OverlayAction::EventResize { track, at, edge, beat } => {
                    cmds.push(opm_app::edit::event_resize_command(
                        &self.state, track, at, edge, beat,
                    ));
                }
                // Del 不在这里：它是**全局键**（见 `key_down` 那一段的 `delete_selection`），
                // 不该依赖"指针正悬在编辑区上"
                OverlayAction::EventResizeEnd => {
                    if self.drag_active {
                        self.drag_active = false;
                        cmds.push(opm_app::edit::commit_command());
                    }
                }
                OverlayAction::PlaceNote { lane_x, beat } => {
                    cmds.push(opm_app::edit::place_note_command(
                        &self.state,
                        opm_app::doc::NoteKind::Tap,
                        lane_x,
                        beat,
                        None,
                    ));
                }
                // ---- 快速放置（Q/W/E/R）：位置由面板吸附好，这里只发命令 ----
                OverlayAction::QuickPlace { kind, lane_x, beat } => {
                    if kind == opm_app::doc::NoteKind::Hold {
                        // hold：**进入跟随状态**（起点定下来，长度随鼠标；Esc 取消 / R 或回车放下）
                        self.state.begin_pending_hold(lane_x, beat);
                        self.file_message = Some((
                            true,
                            "正在放置 hold：移动鼠标定长度，R/回车放下，Esc 取消".to_owned(),
                        ));
                    } else {
                        cmds.push(opm_app::edit::place_note_command(
                            &self.state, kind, lane_x, beat, None,
                        ));
                    }
                }
                // ---- 事件区起草稿：与 hold 同一套跟随流程，只是落在某条轨道上 ----
                OverlayAction::StartEventDraft { track, beat } => {
                    self.state.begin_pending_event(track, beat);
                    self.file_message = Some((
                        true,
                        format!(
                            "正在放置事件（{}）：移动鼠标定长度，R/回车/左键放下，Esc 取消",
                            track.key()
                        ),
                    ));
                }
                // ---- 草稿（hold 或事件块）：跟随 / 拖控制杆 / 放下 / 取消 ----
                OverlayAction::DraftFollow { beat } => {
                    if self.state.pending_event.is_some() {
                        self.state.follow_pending_event(beat);
                    } else {
                        self.state.follow_pending_hold(beat);
                    }
                }
                OverlayAction::DraftResize { edge, beat } => {
                    if self.state.pending_event.is_some() {
                        self.state.resize_pending_event(edge, beat);
                    } else {
                        self.state.resize_pending_hold(edge, beat);
                    }
                }
                OverlayAction::DraftCommit => {
                    if let Some(e) = self.state.take_pending_event() {
                        let (start, end) = e.span();
                        let value =
                            opm_app::edit::new_event_value(&self.state, e.track, start);
                        cmds.push(opm_app::edit::place_event_command(
                            &self.state,
                            e.track,
                            e.layer,
                            start,
                            end,
                            value,
                        ));
                        self.file_message = Some((
                            true,
                            format!(
                                "放下事件（{}）：{start:.3} → {end:.3} 拍 = {value:.3}",
                                e.track.key()
                            ),
                        ));
                    } else if let Some(h) = self.state.take_pending_hold() {
                        let (start, end) = h.span();
                        cmds.push(opm_app::edit::place_note_command(
                            &self.state,
                            opm_app::doc::NoteKind::Hold,
                            h.lane_x,
                            start,
                            Some(end),
                        ));
                        self.file_message =
                            Some((true, format!("放下 hold：{start:.3} → {end:.3} 拍")));
                    }
                }
                OverlayAction::DraftCancel => {
                    self.state.cancel_pending();
                    self.file_message = Some((true, "已取消放置".to_owned()));
                }
                OverlayAction::Notice(t) => {
                    self.file_message = Some((false, t));
                }
            }
        }
        if let Some(t) = seek_to {
            self.seek_to(t);
        }
        if sel_changed {
            self.insp = self.build_inspector();
        }
        if !cmds.is_empty() {
            // 与其它面板同一条写路径：发命令 → 等广播
            self.dispatch(&cmds);
        }
    }

    /// 保存：走**和 CLI 完全相同**的命令（`{"op":"save"}`），不另开一条写文件的路径
    /// 从当前状态刷新对话框字段：**保存目标**（来自 `EditCore::path`）+ 曲名（`meta.name`）
    ///
    /// 保存目标是**一个**路径，而且只存在于 `EditCore` 里；这里同步的是它的**只读显示副本**
    /// （界面不再提供第二个输入口径 —— 想改目标只有「另存为…」或"第一次保存时自动弹窗"两条）。
    ///
    /// 顺带刷新状态栏那份文档标识：**这条路径上的每个入口（打开/另存为/新建/切格式）
    /// 都经过这里**，所以"什么时候该重算"只有一个答案。
    fn sync_file_fields(&mut self) {
        let (path, meta_name) = {
            let c = self.core.lock().unwrap();
            (c.path().map(std::path::Path::to_path_buf), c.doc().meta.name.clone())
        };
        self.target_text = path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "（还没有保存目标）".to_owned());
        self.edit_name = meta_name;
        self.refresh_file_badge();
    }

    /// **载入一份文档之后的收尾**：文件字段 → 最近打开 → 音乐。
    ///
    /// `open_doc` 与 `continue_cached` 共用这一条：两份手抄的收尾迟早分家，
    /// 而少一次 `reload_audio` 就等于"新谱面配着旧音乐"（听不出来，直到播放对不上）。
    /// `from` 只用于日志里"音乐是跟着谁换的"。
    fn after_document_loaded(&mut self, from: &str) {
        self.sync_file_fields(); // 打开之后：文件夹/名字/格式提示/状态栏标识都跟着新文件走
        self.remember_recent();
        // **音乐跟着谱面走**：走与新建/元数据改动同一条入口（内部会同步时间轴总长）
        self.reload_audio(from);
    }

    /// 重算状态栏那份文档标识（格式 / 文件名 / 保存状态）。**只在事件上调用**：
    /// 换文件、存盘、文档被改动（`apply_dirty`）。
    fn refresh_file_badge(&mut self) {
        let (badge, dirty, has_target) = file_badge_of(&self.core);
        self.file_badge = badge;
        self.file_dirty = dirty;
        self.file_has_target = has_target;
    }

    /// 当前**真的**保存目标（来自核心，不信任界面草稿）
    fn save_target(&self) -> Option<std::path::PathBuf> {
        let c = self.core.lock().unwrap();
        c.path().map(std::path::Path::to_path_buf)
    }

    /// 曲名 → 文件名主干（安全化规则在 `filedialog::sanitize_stem`，那边有单测）
    fn file_stem(&self) -> String {
        filedialog::sanitize_stem(&self.edit_name)
    }

    /// 当前形态在这个目标路径上会写成什么（`Auto` 也在这里定下来）。
    ///
    /// **不自己写一份匹配**：判据与真正写盘时用的 [`core::SaveFormat::resolve`] 是同一个 ——
    /// 否则界面提示与实际产物一定会在某天分叉。
    fn save_shape_of(&self, path: &std::path::Path) -> Result<core::SaveShape, String> {
        let loaded = self
            .core
            .lock()
            .map(|c| c.source_format())
            .unwrap_or(opm_app::codec::Format::Opm);
        self.save_format.resolve(path, loaded)
    }

    /// 补扩展名：**打包形态**才补（`.opm` / `.pez`）；文件夹形态的目标是目录，什么都不补
    fn with_extension(&self, path: std::path::PathBuf) -> std::path::PathBuf {
        let Some(ext) = self.save_shape_of(&path).ok().and_then(|s| s.extension()) else {
            return path;
        };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with(ext) {
            path
        } else {
            let mut s = path.as_os_str().to_os_string();
            s.push(ext);
            std::path::PathBuf::from(s)
        }
    }

    /// 还没有保存目标时的**建议名字**，以及它是"目录"还是"文件"：
    /// 打包形态给 `曲名.opm` / `曲名.pez`，文件夹形态给 `曲名`（一个目录）
    fn suggested_save_name(&self) -> (String, bool) {
        let loaded = self
            .core
            .lock()
            .map(|c| c.source_format())
            .unwrap_or(opm_app::codec::Format::Opm);
        let stem = self.file_stem();
        match self.save_format.suggested_extension(loaded) {
            Some(ext) => (format!("{stem}{ext}"), false),
            None => (stem, true),
        }
    }

    /// **未保存守卫**：有未保存改动时先问「保存｜不保存｜返回」，用户选完才继续。
    ///
    /// 三个选项而不是两个（Krita 的做法）：少了「返回」就没法反悔 —— 用户点开"新建"只是想看看，
    /// 结果被迫在"保存"和"丢弃"之间选一个。`dismissed`（Esc/点遮罩）等价于「返回」。
    ///
    /// 画在 `ui()` 的**最前面**也能盖住整页：`dialog::modal` 走 `egui::Modal`（Foreground 层 +
    /// 遮罩 + 自己吞输入），层级与调用顺序无关 —— 于是同一个守卫在启动页与编辑页都有效。
    fn unsaved_guard(&mut self, ctx: &egui::Context) {
        // ---- 未保存守卫：有未保存改动时先问「保存｜不保存｜返回」----
        //
        // 这是 Krita 的三个选项，不是常见的两个：少了「返回」就没法反悔 ——
        // 用户点开"新建"只是想看看，结果被迫在"保存"和"丢弃"之间选一个。
        if let Some(goal) = self.guard_for {
            let mut decide: Option<GuardChoice> = None;
            let name = {
                let c = self.core.lock().unwrap();
                c.doc().meta.name.clone()
            };
            let out = opm_app::dialog::modal(
                &ctx,
                "opm_unsaved_guard",
                opm_app::dialog::W_NARROW,
                |ui| {
                    opm_app::dialog::title(ui, "有未保存的改动");
                    ui.label(format!(
                        "谱面「{name}」自上次保存后有改动。{}之前要先保存吗？",
                        match goal {
                            GuardAction::NewDoc => "新建",
                            GuardAction::OpenDialog => "打开别的谱面",
                            GuardAction::Quit => "退出",
                        }
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("💾 保存").clicked() {
                            decide = Some(GuardChoice::Save);
                        }
                        if ui
                            .button("🗑 不保存")
                            .on_hover_text("丢弃这些改动")
                            .clicked()
                        {
                            decide = Some(GuardChoice::Discard);
                        }
                        if ui
                            .button("↩ 返回")
                            .on_hover_text("什么都不做，回到编辑器")
                            .clicked()
                        {
                            decide = Some(GuardChoice::Cancel);
                        }
                    });
                },
            );
            if out.dismissed {
                decide = Some(GuardChoice::Cancel);
            }
            match decide {
                Some(GuardChoice::Save) => {
                    // 保存（可能先弹保存窗口拿到目标）→ 保存成功后继续原动作
                    self.guard_for = None;
                    self.ensure_target_then_save();
                    if !self.core.lock().unwrap().is_dirty() {
                        self.start_guarded(goal);
                    }
                }
                Some(GuardChoice::Discard) => {
                    self.guard_for = None;
                    self.start_guarded(goal);
                }
                Some(GuardChoice::Cancel) => self.guard_for = None,
                None => {}
            }
        }
    }

    /// 「保存」的语义（用户要求）：**有目标就写回它；没有目标才弹文件选择窗**。
    ///
    /// 以前这里还有一个"可编辑的目标输入框"，于是同一个概念有两种输入方式（框里手打 vs 系统框里选），
    /// 谁优先、什么时候生效都得解释一遍。现在只有一条：**目标的真值是 `EditCore` 里的 path**，
    /// 想改它只有一条路 —— 「另存为…」（或第一次保存时的自动弹窗）。
    fn ensure_target_then_save(&mut self) {
        // 目标只有一个真值（`EditCore` 的 path）。没有目标时由 `save_doc` 弹系统文件选择窗 ——
        // **那条逻辑只写一次**，这里只补一句上下文，免得"点保存怎么突然弹窗"没人解释。
        if self.save_target().is_none() {
            self.file_message = Some((true, "还没有保存目标：选一个位置写下来".to_owned()));
            self.file_dialog_open = true;
        }
        self.save_doc();
    }

    /// 执行一次编辑动作（撤销/重做）。
    ///
    /// **走命令路径**（`{"op":"undo"}`），不直接调 `EditCore::undo()`：那样才有统一的结构化回话、
    /// 才会进核心日志，而且与控制通道、CLI 走的是同一条 —— 界面不发明第二套编辑入口。
    /// 界面只做三件事：按键 → 发命令 → 把结果说清楚（没事可撤时**明说**，不静默吞掉这一次按键）。
    fn apply_edit_action(&mut self, action: keymap::EditAction) {
        let resp = {
            let mut c = self.core.lock().unwrap();
            c.exec(&serde_json::json!({"op": action.op()}))
        };
        let ok = resp.get("ok").and_then(|v| v.as_bool()) == Some(true);
        if !ok {
            let e = resp.get("error").and_then(|v| v.as_str()).unwrap_or("?");
            self.file_message = Some((false, format!("{}失败：{e}", action.verb())));
            return;
        }
        let r = resp.get("result").cloned().unwrap_or(serde_json::Value::Null);
        match r.get(action.result_key()).and_then(|v| v.as_str()) {
            Some(label) => self.console_log.push((true, format!("{}：{label}", action.verb()))),
            // 栈空了：说一句，别让人以为按键没生效（"再按一次也没反应"最容易让人怀疑程序坏了）
            None => self.file_message = Some((true, format!("没有可{}的了", action.verb()))),
        }
    }

    /// **Del：删掉整个选区**（音符或事件，一次删干净 = 一个撤销步）。
    ///
    /// 两件事不在这一层做：
    /// · **命令怎么拼**（尤其是"同一张表里按下标降序发"这条正确性规则）在 `edit.rs`；
    /// · **哪些下标还算数**由 `EditCore` 判（越界会明确报错，这里不预筛）。
    fn delete_selection(&mut self) {
        let notes = self.state.selection().notes().count();
        let events = self.state.selection().events().count();
        if notes + events == 0 {
            self.file_message = Some((
                false,
                "没有选中的音符或事件（先框选，或 Ctrl+左键多选）".to_owned(),
            ));
            return;
        }
        let cmds = opm_app::edit::delete_selection_commands(&self.state);
        if cmds.is_empty() {
            return;
        }
        // 删掉的东西已经不存在了：选区当场清空，别留下一堆指不到东西的下标
        self.state.clear_selection();
        self.insp = self.build_inspector();
        self.dispatch(&cmds);
        let what = match (notes, events) {
            (0, e) => format!("{e} 条事件"),
            (n, 0) => format!("{n} 个音符"),
            (n, e) => format!("{n} 个音符 + {e} 条事件"),
        };
        self.file_message = Some((true, format!("已删除 {what}（Ctrl+Z 可撤销）")));
    }

    /// **按当前文档/命令行装载音乐，并同步"乐曲时长"**（时间轴总长要用它）。
    ///
    /// 调用点：打开文件、**新建谱面**、以及 `meta.audio` 真的变了的时候。
    ///
    /// 为什么必须收成一个方法：早先"装载音频"与"更新乐曲时长"是两处分开写的代码，
    /// 新建谱面那条路**只做了前者** ⇒ 用户看到"明明有音乐，时间轴却说无音乐、只有 10 拍留白那么长"。
    /// （同类教训在时间轴总长上也出现过一次：`set_content_end_beat` 与视图模型分开写就会漏。）
    fn reload_audio(&mut self, from: &str) {
        let spec = {
            let c = self.core.lock().unwrap();
            audio::spec(self.args.audio.as_deref(), c.doc().meta.audio.as_deref())
        };
        // 先只解析**路径**（`--audio` 优先、`meta.audio` 相对谱面目录），装载分两半做
        let (meta_audio, chart_path) = {
            let c = self.core.lock().unwrap();
            (
                c.doc().meta.audio.clone(),
                c.path().map(std::path::Path::to_path_buf),
            )
        };
        let asset_dir = asset_dir_of(&self.core);
        let resolved = audio::resolve_source(
            self.args.audio.as_deref(),
            meta_audio.as_deref(),
            chart_path.as_deref(),
            asset_dir.as_deref(),
        );
        let resolved = match resolved {
            Ok(v) => v,
            Err(e) => {
                self.state.set_music_len(None);
                self.audio = None;
                self.loaded_audio_spec = spec;
                self.console_log.push((false, format!("音乐没能载入（{from}）：{e}")));
                return;
            }
        };
        let Some((p, src)) = resolved else {
            self.state.set_music_len(None);
            self.audio = None;
            self.loaded_audio_spec = spec;
            self.console_log
                .push((true, format!("{from}：这份谱面没有音乐 —— 时间轴按内容 + 10 拍留白")));
            return;
        };
        match audio::decode(&p) {
            Ok(decoded) => {
                let sec = decoded.duration_sec();
                match audio::Audio::from_decoded(decoded) {
                    Ok(a) => {
                        self.state.set_music_len(Some(sec));
                        self.console_log.push((
                            true,
                            format!(
                                "音乐（{from}，来源 {}）→ {}（{sec:.1}s；时间轴总长按它）",
                                src.label(),
                                a.path
                            ),
                        ));
                        self.audio = Some(a);
                    }
                    Err(e) => {
                        // 有音乐、有长度，但放不出声：时间轴仍然按它算，如实说清
                        self.state.set_music_len(Some(sec));
                        self.console_log.push((
                            false,
                            format!("音乐（{from}）已解码（{sec:.1}s，时间轴按它算）但**放不出声**：{e}"),
                        ));
                        self.audio = None;
                    }
                }
            }
            Err(e) => {
                self.state.set_music_len(None);
                self.audio = None;
                self.console_log.push((
                    false,
                    format!("音乐解码失败（{from}）：{e} —— 时间轴只有内容 + 10 拍留白"),
                ));
            }
        }
        self.loaded_audio_spec = spec;
    }

    /// 把当前文件记进"最近打开"（起始界面左半边的内容）
    fn remember_recent(&mut self) {
        let (path, title, fmt) = {
            let c = self.core.lock().unwrap();
            (
                c.path().map(std::path::Path::to_path_buf),
                c.doc().meta.name.clone(),
                c.source_format().as_str(),
            )
        };
        if let Some(p) = path {
            self.recents.add(&p, &title, fmt, recents::now_secs());
            if let Err(e) = self.recents.save_default() {
                // 便利功能写不进去不该打断工作，但要留痕
                self.console_log.push((false, format!("最近打开列表未能保存：{e}")));
            }
            // 列表内容变了 ⇒ 重算启动页那份行快照（事件驱动，不是每帧）
            self.refresh_list_rows();
        }
    }

    /// 请求做一件"会丢掉当前文档"的事：有未保存改动就先弹守卫，否则直接做
    fn request_guarded(&mut self, goal: GuardAction) {
        let dirty = self.core.lock().map(|c| c.is_dirty()).unwrap_or(false);
        if dirty {
            self.guard_for = Some(goal);
        } else {
            self.start_guarded(goal);
        }
    }

    /// 守卫放行之后真正执行
    fn start_guarded(&mut self, goal: GuardAction) {
        match goal {
            GuardAction::NewDoc => {
                // **谱面的建立只发生在启动页**：切回启动页并打开「新建谱面」模态（底下是谱面列表，
                // 不是编辑页）。换窗口标题/尺寸需要 `Context`，这里拿不到 —— 记个待办，帧里处理
                // （见 `pump_launch_new`）。
                self.pending_launch_new = true;
            }
            GuardAction::OpenDialog => self.open_via_system(),
            // 关闭需要 `Context`（这一层拿不到）：立个旗子，由帧里那个 `pending_quit` 处理
            GuardAction::Quit => self.pending_quit = true,
        }
    }

    /// **我们自己**决定退出（截屏、bench、7z 门槛，以及守卫放行之后）。
    ///
    /// 先立 `quit_allowed` 再发 `Close`：`ViewportCommand::Close` 同样会让下一帧的
    /// `close_requested()` 为真 —— 不加这个旗子，程序化退出会被自己的"未保存守卫"拦下来等人点按钮
    /// （`--shot-exit` 与 bench 会当场卡住）。
    fn quit_now(&mut self, ctx: &egui::Context) {
        self.quit_allowed = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// 缺 7z 的门槛模态：**启动页与编辑页共用这一份**（文案、下载页 URL、`fetch`/`quit` 的处置）。
    ///
    /// 为什么抽出来：同一件事曾在两处各画一遍 —— 连"打开浏览器失败：…"那句都是手抄的，
    /// 改一处忘一处就会出现"启动页说装好要重启、编辑页没说"这类不一致。
    /// 返回"用户点了退出"；启动页还要额外看"窗口是否被关"（它有自己的事件循环）。
    fn missing_7z_gate(&mut self, ctx: &egui::Context) -> bool {
        let Some(msg) = self.seven_zip_missing.clone() else {
            return false;
        };
        let out = opm_app::recents::missing_7z_modal(ctx, &msg, cfg!(windows));
        if out.fetch {
            let next = match filedialog::open_url(zip::SEVEN_ZIP_URL) {
                Ok(()) => format!("{msg}\n（已用浏览器打开下载页；装好之后重启 OpenPhM）"),
                Err(e) => format!("{msg}\n（打开浏览器失败：{e}；地址：{}）", zip::SEVEN_ZIP_URL),
            };
            self.seven_zip_missing = Some(next);
        }
        out.quit
    }

    /// 按启动页表单里的值创建空谱面（曲名/谱面作者/音乐作者/音乐路径/基础 BPM）。
    /// 校验在 `NewChartForm::validate`（曲名必填、BPM 为正）。**返回是否真的建成了** ——
    /// 调用方靠它决定"进编辑页"还是"留在模态里看原因"：失败却把人送进编辑页，
    /// 等于把一条错误提示藏到一个已经没有上下文的界面后面。
    fn create_new_doc(&mut self) -> bool {
        if let Err(e) = self.new_form.validate() {
            // 原因**已经在模态里**（表单下方那行实时校验）—— 不再往按钮旁边贴第二份同样的字，
            // 也不进控制台日志：这是用户正在改的输入，不是系统故障。
            debug_assert!(!e.is_empty());
            self.new_form_open = true; // 模态不关：人在填表，别把他填的东西连同弹窗一起收走
            self.phase = LaunchPhase::StartScreen;
            return false;
        }
        let cmd = self.new_form.to_new_command();
        let resp = {
            let mut c = self.core.lock().unwrap();
            c.exec(&cmd)
        };
        let ok = resp.get("ok").and_then(|v| v.as_bool()) == Some(true);
        if ok {
            // 成功之后这一屏就该是"列表 → 编辑页"了：关掉模态
            self.new_form_open = false;
            let r = resp.get("result").cloned().unwrap_or(serde_json::Value::Null);
            self.console_log.push((
                true,
                format!(
                    "新建谱面「{}」（{:.2} BPM，{} 条判定线）—— 还没有保存目标",
                    r.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                    r.get("bpm").and_then(|v| v.as_f64()).unwrap_or(0.0),
                    r.get("lines").and_then(|v| v.as_u64()).unwrap_or(0),
                ),
            ));
            // 界面跟着新文档走：保存目标清空、曲名=新曲名、音频换掉
            self.sync_file_fields();
            // 音乐路径是**文档字段**（`meta.audio`，刚由 `new` 命令写进文档）：
            // 走统一入口装载 —— **它同时把"乐曲时长"交给视图状态**（时间轴总长要用）。
            // 这里以前只写了 `self.audio`，漏了 `set_music_len` ⇒ "有音乐但时间轴说无音乐"。
            self.reload_audio("新建谱面");
            self.state.playhead = 0.0;
        } else {
            // 命令被拒：模态留在原地，原因显示在模态里（也进控制台日志）
            let why = resp
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("?")
                .to_owned();
            self.new_form_open = true;
            self.phase = LaunchPhase::StartScreen;
            self.file_message = Some((false, format!("新建失败：{why}")));
            self.console_log.push((false, format!("新建失败：{why}")));
        }
        ok
    }

    /// 把对话框里的**谱面名字写进文档**（`meta.name`）。
    /// 只在真的变了时发命令 —— 否则每敲一个字都进一次撤销栈。
    fn apply_chart_name(&mut self) {
        let name = self.file_stem();
        let cur = {
            let c = self.core.lock().unwrap();
            c.doc().meta.name.clone()
        };
        if cur != name {
            let resp = {
                let mut c = self.core.lock().unwrap();
                c.exec(&serde_json::json!({"op": "set_meta", "set": {"name": name}}))
            };
            let ok = resp.get("ok").and_then(|v| v.as_bool()) == Some(true);
            self.console_log.push((
                ok,
                if ok {
                    format!("谱面名字 → {name}")
                } else {
                    format!(
                        "改谱面名字失败：{}",
                        resp.get("error").and_then(|e| e.as_str()).unwrap_or("?")
                    )
                },
            ));
        }
    }

    /// 走**系统文件对话框**打开谱面（Ctrl+O 与对话框里的「打开…」都是它）
    fn open_via_system(&mut self) {
        let cur = {
            let c = self.core.lock().unwrap();
            c.path().map(std::path::Path::to_path_buf)
        };
        let start = filedialog::start_for_open(cur.as_deref());
        match filedialog::pick(filedialog::Which::Open, start.as_deref(), filedialog::CHART_FILTER) {
            Ok(Some(p)) => self.open_doc(&p.display().to_string()),
            Ok(None) => self.file_message = Some((true, "已取消".to_owned())),
            Err(e) => {
                self.file_message = Some((false, e));
                self.sync_file_fields();
                self.file_dialog_open = true;
            }
        }
    }

    /// 走**系统文件对话框**另存为（Ctrl+Shift+S 与「另存为…」都是它）
    fn save_as_via_system(&mut self) {
        if self.edit_name.trim().is_empty() {
            self.sync_file_fields();
        }
        // 起始位置：**存在的目录 + 曲名 + 目标扩展名**。
        // 这里就是"保存新文件时无法指定路径"的修复点：新文件还不存在，但起始位置的父目录必须存在，
        // 否则 KDE 会把整串当目录并报"目录不存在"（见 filedialog::nearest_existing_dir）。
        let cur = {
            let c = self.core.lock().unwrap();
            c.path().map(std::path::Path::to_path_buf)
        };
        let dir = cur.as_deref().and_then(|p| p.parent()).map(|d| d.to_path_buf());
        let (name, is_dir) = self.suggested_save_name();
        // **两种形态、两种系统框**：打包形态要选一个**文件**（给建议文件名 + 对应过滤器）；
        // 文件夹形态要选一个**目录**（`pick_folder`）。起始位置都必须是**已存在**的目录 ——
        // 传一个不存在的路径给 kdialog，KDE 会当目录处理并报"目录不存在"（踩过的坑）。
        let (picked, err) = if is_dir {
            let start = dir.as_deref().map(filedialog::nearest_existing_dir);
            match filedialog::pick_folder(start.as_deref()) {
                Ok(v) => (v, None),
                Err(e) => (None, Some(e)),
            }
        } else {
            let start = Some(filedialog::start_for_new_save(dir.as_deref(), &name, ""));
            let filter = match self.save_shape_of(std::path::Path::new(&name)) {
                Ok(core::SaveShape::RpeZip) => filedialog::PEZ_FILTER,
                _ => filedialog::OPM_FILTER,
            };
            match filedialog::pick(filedialog::Which::Save, start.as_deref(), filter) {
                Ok(v) => (v, None),
                Err(e) => (None, Some(e)),
            }
        };
        match (picked, err) {
            (Some(p), _) => {
                // 系统框里可能没写扩展名：按当前形态补上（文件夹形态不补，见 `with_extension`）
                let p = self.with_extension(p);
                self.save_doc_as(&p.display().to_string());
                self.sync_file_fields();
            }
            (None, None) => self.file_message = Some((true, "已取消".to_owned())),
            (None, Some(e)) => {
                self.file_message = Some((false, e));
                self.sync_file_fields();
                self.file_dialog_open = true;
            }
        }
    }

    /// 在系统文件管理器里显示当前文件（KDE：dolphin --select）
    fn reveal_current(&mut self) {
        let cur = {
            let c = self.core.lock().unwrap();
            c.path().map(std::path::Path::to_path_buf)
        };
        match cur {
            Some(p) => match filedialog::reveal(&p) {
                Ok(()) => {
                    self.file_message =
                        Some((true, format!("已在文件管理器中显示 {}", p.display())));
                }
                Err(e) => self.file_message = Some((false, e)),
            },
            None => {
                self.file_message = Some((false, "还没有文件路径（先保存一次）".to_owned()));
            }
        }
    }

    /// 打开一个谱面文件：**按内容判格式**（opm / RPE 都行），走核心的 `load` 命令 ——
    /// 于是 CLI（`--attach` + `{"op":"load"}`）、GUI 按钮、控制台是**同一条路径**。
    fn open_doc(&mut self, path: &str) {
        let resp = {
            let mut c = self.core.lock().unwrap();
            c.exec(&serde_json::json!({"op": "load", "path": path}))
        };
        let ok = resp.get("ok").and_then(|v| v.as_bool()) == Some(true);
        if ok {
            let r = resp.get("result").cloned().unwrap_or(serde_json::Value::Null);
            let lines = r.get("lines").and_then(|v| v.as_u64()).unwrap_or(0);
            let notes = r.get("notes").and_then(|v| v.as_u64()).unwrap_or(0);
            let fmt = r.get("format").and_then(|v| v.as_str()).unwrap_or("?");
            self.console_log.push((true, format!("已打开 {path}（{fmt}，{lines} 线 / {notes} 音符）")));
            // 保真度报告：有降级就说出来，别让"能打开"冒充"没丢东西"
            if let Some(f) = r.get("fidelity") {
                let lossless = f.get("lossless").and_then(|v| v.as_bool()).unwrap_or(true);
                let n = f.get("warnings").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                if lossless {
                    self.console_log.push((true, "导入无降级".to_owned()));
                } else {
                    for w in f.get("warnings").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                        self.console_log
                            .push((false, format!("导入降级：{}", w.as_str().unwrap_or("?"))));
                    }
                    self.console_log.push((false, format!("共 {n} 项降级（其余字段已按原义转换）")));
                }
            }
            self.after_document_loaded("打开谱面");
        } else {
            self.console_log.push((
                false,
                format!(
                    "打开失败：{}",
                    resp.get("error").and_then(|e| e.as_str()).unwrap_or("?")
                ),
            ));
        }
    }

    /// 另存为：格式由 `save_format` 决定（`Auto` 按扩展名 —— `.opm.json` 是 opm，`.json` 按 RPE 生态习惯）
    fn save_doc_as(&mut self, path: &str) {
        // 形态名由库里给（`SaveFormat::as_str`）：界面上那一行说明与真正写盘用的是同一套词
        let fmt = self.save_format.as_str();
        let resp = {
            let mut c = self.core.lock().unwrap();
            c.exec(&serde_json::json!({"op": "save", "path": path, "format": fmt}))
        };
        let ok = resp.get("ok").and_then(|v| v.as_bool()) == Some(true);
        if ok {
            let r = resp.get("result").cloned().unwrap_or(serde_json::Value::Null);
            let p = r.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let f = r.get("format").and_then(|v| v.as_str()).unwrap_or("?");
            self.console_log.push((true, format!("已保存 {p}（{f}）")));
            self.remember_recent();
            if let Some(fid) = r.get("fidelity") {
                let lossless = fid.get("lossless").and_then(|v| v.as_bool()).unwrap_or(true);
                if !lossless {
                    for w in fid.get("warnings").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                        self.console_log
                            .push((false, format!("导出降级：{}", w.as_str().unwrap_or("?"))));
                    }
                }
            }
            self.sync_file_fields();
        } else {
            self.console_log.push((
                false,
                format!(
                    "另存为失败：{}",
                    resp.get("error").and_then(|e| e.as_str()).unwrap_or("?")
                ),
            ));
        }
    }

    fn save_doc(&mut self) {
        // 还没有路径（新建的谱面）：按系统规范直接弹"另存为"，而不是报一句"未指定保存路径"
        let has_path = {
            let c = self.core.lock().unwrap();
            c.path().is_some()
        };
        if !has_path {
            self.save_as_via_system();
            return;
        }
        let resp = {
            let mut c = self.core.lock().unwrap();
            c.exec(&serde_json::json!({"op": "save"}))
        };
        let ok = resp.get("ok").and_then(|v| v.as_bool()) == Some(true);
        let line = if ok {
            format!(
                "已保存 {}",
                resp.get("result")
                    .and_then(|r| r.get("path"))
                    .and_then(|p| p.as_str())
                    .unwrap_or("?")
            )
        } else {
            format!(
                "保存失败：{}（用 --doc 指定路径，或先另存）",
                resp.get("error").and_then(|e| e.as_str()).unwrap_or("?")
            )
        };
        self.console_log.push((ok, line));
        if ok {
            // 存盘不产生广播（文档没变），但"脏"翻了 ⇒ 状态栏那份标识要重算
            self.refresh_file_badge();
        }
    }

    /// 校验：同样走命令路径（结果是普通文档读取，不产生撤销步）
    fn validate_doc(&mut self) {
        let resp = {
            let mut c = self.core.lock().unwrap();
            c.exec(&serde_json::json!({"op": "validate"}))
        };
        let r = resp.get("result").cloned().unwrap_or(serde_json::Value::Null);
        let errors = r.get("errors").and_then(|v| v.as_u64()).unwrap_or(0);
        let warns = r.get("warnings").and_then(|v| v.as_u64()).unwrap_or(0);
        self.console_log.push((
            errors == 0,
            format!("校验：{errors} error(s), {warns} warning(s)"),
        ));
    }

    // ------------------------------------------------------------ 播放控制
    //
    // 空格键与 CLI 视图命令**走同一条路径**：只有一个入口，避免两条实现各修一半。

    /// 设置播放状态：音频流与播放头一起对齐（有音频时以音频游标为准）
    fn set_playing(&mut self, on: bool) {
        if on == self.state.playing {
            return;
        }
        if let Some(a) = &self.audio {
            if on {
                a.play_from(self.state.playhead);
            } else {
                a.pause();
            }
        }
        self.state.set_playing(on);
        self.audio_drift = None; // 重新计时，别把暂停期间算进漂移
    }

    fn toggle_play(&mut self) {
        let on = !self.state.playing;
        self.set_playing(on);
    }

    fn seek_to(&mut self, t: f64) {
        self.state.seek(t);
        if let Some(a) = &self.audio {
            if self.state.playing {
                a.play_from(self.state.playhead);
            } else {
                // 暂停中也把游标挪过去，下一次 play 从新位置开始
                a.play_from(self.state.playhead);
                a.pause();
            }
        }
        self.audio_drift = None;
        // 检查器里那一段"**此刻**表演（事件求值）"是**快照**：不在这里刷，拖动时间轴时它会
        // 停在旧的一刻上（而那一段的标题正是"此刻"）。seek 是离散动作（拖时间轴时每帧一次，
        // 与拖事件同量级），刷一次快照 = 锁一次文档 + 读选中项，代价可以接受。
        self.insp = self.build_inspector();
    }

    /// 取走控制通道投来的视图命令并执行
    fn pump_view_cmds(&mut self) {
        let cmds: Vec<control::ViewCmd> = {
            let mut q = match self.view.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            q.drain(..).collect()
        };
        for c in cmds {
            match c {
                control::ViewCmd::Play => self.set_playing(true),
                control::ViewCmd::Pause => self.set_playing(false),
                control::ViewCmd::TogglePlay => self.toggle_play(),
                control::ViewCmd::SeekSec(t) => self.seek_to(t),
                control::ViewCmd::SeekBeat(b) => {
                    let t = self.state.chart.tmap.sec(b);
                    self.seek_to(t);
                }
                control::ViewCmd::SetWindowOffsetX(x) => {
                    // 纯视图状态（文档里没有这个字段）：与顶栏拖动框同一条路径
                    self.state.set_window_offset_x(x);
                    self.window_offset_x_ui = self.state.window_offset_x;
                    let (lo, hi) = self.state.window_lane_range();
                    println!(
                        "  窗口 X 偏移已设    : {:+.1} RPE（音符区显示 laneX {lo:.1}…{hi:.1}）",
                        self.state.window_offset_x
                    );
                }
                control::ViewCmd::Zoom { factor, beats } => {
                    // 缩放是**纯视图状态**（谱面里没有"可见拍数"这个字段），与 Ctrl+滚轮同一条路径
                    if let Some(b) = beats {
                        self.state.overlay_beats = b.clamp(
                            state::EditorState::ZOOM_MIN_BEATS,
                            state::EditorState::ZOOM_MAX_BEATS,
                        );
                    }
                    if let Some(f) = factor {
                        self.state.zoom_by(f);
                    }
                    let step = crate::overlay::axis_label_step(
                        self.state.overlay_beats,
                        state::GRID_NOMINAL_PANE_H as f32,
                    );
                    let dec = crate::overlay::axis_label_decimals(step);
                    println!(
                        "  缩放已设          : {} 拍可见（纵轴标注每 {:.*} 拍一条）",
                        self.state.overlay_beats, dec, step
                    );
                }
                control::ViewCmd::SetGrid { beat_div, lane_div } => {
                    if let Some(d) = beat_div {
                        self.state.grid.beat_div = d.clamp(1, 64);
                    }
                    if let Some(d) = lane_div {
                        self.state.grid.lane_div = state::GridCfg::normalize_lane_div(d);
                    }
                    println!(
                        "  网格已设          : 每拍 {} 条 / 窗口 {} 等分（横向步长 {:.3} RPE{}）",
                        self.state.grid.beat_div,
                        self.state.grid.lane_div,
                        self.state.grid.h_step_rpe(),
                        if self.state.grid.center_is_lattice() {
                            "，偶数等分：中轴是格点"
                        } else {
                            "，奇数等分：中轴不是格点、但照画"
                        }
                    );
                }
                control::ViewCmd::Select { line, track, note, event, notes, events } => {
                    // 选中是视图状态：直接改 EditorState，不碰文档
                    if let Some(li) = line {
                        self.state.select_line_doc(li);
                    }
                    if let Some(t) = track {
                        if let Some(id) = state::TrackId::from_key(&t) {
                            self.state.selected_track = id;
                        }
                    }
                    // **多选口径优先**（整批替换）；否则退回单选；都没给就清空。
                    // 选区同时只有一类 ⇒ 多选的两个列表只会有一个非空（两个都给以 `notes` 为准）。
                    if let Some(ns) = notes {
                        self.state.select_notes(ns);
                    } else if let Some(es) = events {
                        let picked: Vec<opm_app::state::EventSel> = es
                            .iter()
                            .filter_map(|(t, i)| {
                                opm_app::state::TrackId::from_key(t).map(|id| (id, *i))
                            })
                            .collect();
                        if let Some((t, _)) = picked.first() {
                            self.state.selected_track = *t;
                        }
                        self.state.select_events(picked);
                    } else {
                        match (note, event) {
                            (Some(n), _) => self.state.select_note(n),
                            (None, Some(e)) => {
                                self.state.select_event(self.state.selected_track, e)
                            }
                            (None, None) => self.state.clear_selection(),
                        }
                    }
                    self.clamp_selection();
                    self.insp = self.build_inspector();
                }
                control::ViewCmd::NudgeBeats(d) => {
                    // 与编辑区里滚动滚轮**同一个动作**（滚轮只是它的一个触发器）
                    let beat = self.state.chart.tmap.beat(self.state.playhead) + d;
                    self.seek_to(self.state.chart.tmap.sec(beat.max(0.0)));
                }
                control::ViewCmd::SetOffsetMs(ms) => {
                    if let Some(a) = &self.audio {
                        a.set_offset_ms(ms);
                    }
                    self.args.audio_offset_ms = ms;
                }
                control::ViewCmd::LoadAudio(p) => match audio::Audio::load(std::path::Path::new(&p)) {
                    Ok(a) => {
                        a.set_offset_ms(self.args.audio_offset_ms);
                        println!("  音频已替换        : {}（{}）", a.path, a.device());
                        // 时间轴总长跟着新音乐走（换了歌，长度当然也换）
                        self.state.set_music_len(Some(a.duration()));
                        self.audio = Some(a);
                    }
                    Err(e) => eprintln!("  替换音频失败      : {e}"),
                },
            }
        }
    }

    /// 抽取待处理的广播，合并成一个脏位集合（一批改动只重建一次）
    fn pump_broadcasts(&mut self) {
        // 先把广播全部取出（释放对 self 的可变借用），再统一重建
        let mut batch: Vec<(u64, String, Vec<String>, Dirty)> = Vec::new();
        if let Some(sub) = &self.sub {
            while let Ok(b) = sub.rx.try_recv() {
                let d = dirty::dirty_of_topics(&b.topics);
                let topics: Vec<String> = b.topics.iter().map(broadcast::Topic::label).collect();
                batch.push((b.revision, b.summary(), topics, d));
            }
        }
        if batch.is_empty() {
            return;
        }
        let n = batch.len() as u64;
        let mut merged = Dirty::default();
        let (mut ns, mut np, mut nn, mut nt, mut ni) = (0u64, 0u64, 0u64, 0u64, 0u64);
        for (_, summary, topics, d) in batch {
            // 逐**广播**判定"这个话题与面板无关" —— 与批无关，因此不会因同帧合并而失真
            if !d.structure {
                ns += 1;
            }
            if d.props.is_empty() {
                np += 1;
            }
            if d.notes.is_empty() {
                nn += 1;
            }
            if d.tracks.is_empty() {
                nt += 1;
            }
            if !d.inspector {
                ni += 1;
            }
            merged.merge(&d);
            self.last_broadcast = summary;
            self.last_topics = topics;
        }
        self.applied_broadcasts += n;
        // 实际重建按**批**计（一帧内多条广播合并成一次重建，K 条广播 1 次重建即合并收益）
        self.skipped_structure += ns;
        self.skipped_props += np;
        self.skipped_notes += nn;
        self.skipped_tracks += nt;
        self.skipped_inspector += ni;

        // 唤醒延迟：从"控制线程请求重绘"到"这一帧应用广播"（与 EditCore 无关的那一段）
        if let Ok(mut st) = self.stats.lock() {
            if let Some(t0) = st.wake_at.take() {
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                self.wake_latencies.push(ms);
                if self.wake_latencies.len() > 512 {
                    self.wake_latencies.remove(0);
                }
                st.wake_last_ms = ms;
                let mut v = self.wake_latencies.clone();
                // NaN 会让 partial_cmp 返回 None：`unwrap()` 就成了"某个数坏掉 = 整个程序崩"。
                // 统计值坏掉最多是统计不准，不该拖垮编辑器 —— 当作相等排序即可。
                v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                st.wake_p50_ms = v[(v.len() - 1) / 2];
            }
        }

        // 延迟：从"发出命令"到"广播被应用"
        if let Some(t0) = self.pending_dispatch.take() {
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            self.latencies.push(ms);
            if self.latencies.len() > 512 {
                self.latencies.remove(0);
            }
        }

        self.apply_dirty(merged);
    }

    /// 只重建脏掉的东西，而且**按线**重建：改 3 号线的音符不动 0/1/2 号线的缓存。
    fn apply_dirty(&mut self, d: Dirty) {
        // **内容末端（拍）随任何内容变化刷新** —— 时间轴总长要用它。
        // 这条是用户报的 bug 的修复点：总长以前读 `state.chart.tmap.end_beat`，而那份视图模型只在
        // `structure` 变化时重建（加音符走"逐线局部重建"）⇒ 音符放到 68.9s 了，时间轴还停在 20.0s。
        // 只算末端（`chart_end()` 是 note/事件的 max），不重建视图模型，所以可以每条广播都做。
        // （不含 `d.meta`：改曲名/曲师不影响内容长度，别为它白扫一遍所有音符）
        if d.structure || !d.props.is_empty() || !d.notes.is_empty() || !d.tracks.is_empty() {
            let c = self.core.lock().unwrap();
            let end = c.doc().chart_end().to_f64();
            drop(c);
            self.state.set_content_end_beat(end);
        }
        // 文档变了 ⇒ 状态栏的"有未保存改动"标记可能翻转（改一笔就脏、撤销回去就干净）。
        // 这是**事件**，不是每帧：只在收到广播时重算。
        self.refresh_file_badge();
        // 文档级聚合（音符总数/判定线数）随任何结构变化而变 —— 这是踩过的缓存不一致：
        // 早先只把它挂在 Meta 上，于是"加了 3 个音符、工具栏仍显示旧计数"。
        if d.meta || d.structure || !d.notes.is_empty() || !d.props.is_empty() {
            let c = self.core.lock().unwrap();
            self.doc_lines = c.doc().judge_lines.len();
            self.doc_notes = c.doc().note_count();
            drop(c);
        }
        if d.meta {
            let c = self.core.lock().unwrap();
            self.meta_name = c.doc().meta.name.clone();
            drop(c);
            self.builds_meta += 1;
            // `meta.audio` **真的变了**才重新装载（每敲一个字都会来一条 meta 广播，
            // 不能每次都开设备、解一遍音频 —— 比字符串就够了）
            let spec = {
                let c = self.core.lock().unwrap();
                audio::spec(self.args.audio.as_deref(), c.doc().meta.audio.as_deref())
            };
            if spec != self.loaded_audio_spec {
                self.reload_audio("meta.audio 改动");
            }
        }

        if d.structure {
            // 线集合/时间映射变了：整表重建（这是唯一"全量"的一档）
            let t = Instant::now();
            let c = self.core.lock().unwrap();
            let doc = c.doc().clone();
            drop(c);
            self.state.set_chart(state::chart_from_doc(&doc)); // 连内容末端一起同步
            self.last_structure_ms = t.elapsed().as_secs_f64() * 1000.0;
            self.builds_structure += 1;
            self.line_rows = view::line_rows_of(&self.state.chart);
            self.builds_notes += self.state.chart.lines.len() as u64;
            self.clamp_selection();
        } else {
            // 逐线局部重建：只锁一次文档，按需取该线的部分
            if !d.props.is_empty() || !d.notes.is_empty() || !d.tracks.is_empty() {
                let c = self.core.lock().unwrap();
                let tmap = self.state.chart.tmap.clone();
                for i in &d.props {
                    if let Some(l) = state::line_shell(c.doc(), *i, &tmap) {
                        if let Some(slot) = self.state.chart.lines.iter_mut().find(|x| x.index == *i) {
                            slot.name = l.name;
                            slot.z_order = l.z_order;
                            slot.is_cover = l.is_cover;
                            slot.bpm_factor = l.bpm_factor;
                        }
                        self.builds_props += 1;
                    }
                }
                for i in &d.notes {
                    let notes = state::notes_of(c.doc(), *i, &tmap);
                    if let Some(slot) = self.state.chart.lines.iter_mut().find(|x| x.index == *i) {
                        // 走 `set_notes` 而不是直接赋值：音符位置缓存（加载时算好的那份）
                        // 必须跟着重算 —— 下标全变了，"只补一段"补不对
                        slot.set_notes(notes, &tmap);
                        self.builds_notes += 1;
                    }
                }
                for i in &d.tracks {
                    let tracks = state::tracks_of(c.doc(), *i, &tmap);
                    if let Some(slot) = self.state.chart.lines.iter_mut().find(|x| x.index == *i) {
                        // 走 `set_tracks`：流速真的变了就把**它之后**的音符位置标成待重算
                        // （异步补齐，见下面的 `pump_floors`），并刷新构建窗口用的 min_speed_abs
                        slot.set_tracks(tracks, &tmap);
                        self.builds_tracks += 1;
                    }
                }
                drop(c);
                // zOrder 可能变了：保持绘制顺序（小的先画）
                self.state
                    .chart
                    .lines
                    .sort_by(|a, b| a.z_order.cmp(&b.z_order).then(a.index.cmp(&b.index)));
                self.line_rows = view::line_rows_of(&self.state.chart);
                self.clamp_selection();
            }
        }

        if d.inspector {
            self.insp = self.build_inspector();
            self.builds_inspector += 1;
        }

        // ---- 事件重叠：**由 EditCore 拥有并维护**（它自己知道这次动过哪条线），
        // GUI 只是把那份缓存抄出来显示。这里不再有第二份"何时重查"的逻辑。
        let was_empty = self.conflicts.is_empty();
        {
            let c = self.core.lock().unwrap();
            self.conflicts = c.overlaps().to_vec();
        }
        // 冲突**一出现就自动展开**浏览器（用户还得先找到入口才能点，那太绕）；
        // 关掉之后不会再自己弹回来（只在"从无到有"时展开）
        if was_empty && !self.conflicts.is_empty() {
            self.show_conflicts = true;
        }

        // d.render 不需要重建任何缓存：演奏区实例每帧现构，置位即"该重绘"
        if d.render && self.args.verbose_updates {
            // 走 stderr：stdout 重定向到文件时是块缓冲，进程被 SIGTERM 收走时留痕会整段丢失
            eprintln!("[update] 演奏区需重绘（线属性/事件/音符变化，无整表重建）");
        }
    }

    /// 选中项越界时收敛（线被删掉、音符/事件被删掉、换了轨道）。
    ///
    /// 多选之后这件事必须**整批**做：集合里任何一个下标过期都要剔除（否则 Del 会删错东西）。
    /// 只清锚是不够的 —— 锚没了会自动落到集合里还在的第一个，见 `Selection::retain`。
    fn clamp_selection(&mut self) {
        let n = self.state.chart.lines.len();
        if n == 0 {
            self.state.selected_line = 0;
        } else if self.state.selected_line >= n {
            self.state.selected_line = n - 1;
        }
        // 先把"当前这条线上还有哪些下标存在"取出来（借用分开：state 既要读又要改）
        let track = self.state.selected_track;
        let (line_notes, track_events) = match self.state.selected() {
            Some(l) => (l.notes.len(), l.track(track).events.len()),
            None => (0, 0),
        };
        self.state.retain_selection(
            |i| i < line_notes,
            // 事件只保留**当前轨道**上的：换轨道之后旧下标指向的是别的轨道
            move |(t, i)| t == track && i < track_events,
        );
    }

    /// 检查器展示的选中对象：**当前判定线 + 当前轨道 + 当前事件 + 当前音符**（线优先）
    fn build_inspector(&self) -> Option<Inspector> {
        // 读一次核心（读不是写；检查器只在选中/广播变化时刷新，不是每帧）
        let doc = self.core.lock().ok()?;
        view::inspector_of(&self.state, doc.doc())
    }
}

/// 检查器快照：**当前判定线 + 当前轨道 + 当前事件 + 当前音符**（线优先）。
/// 自由函数：初始快照与广播后的重建走同一条路径，避免两份实现漂移。
impl App {
    /// 请求下一帧（帧率策略）。**启动页与编辑页共用**：早退分支漏掉它就会"只出几帧然后卡住"。
    fn pace(&self, ctx: &egui::Context) {
        // 启动页没有动画：事件驱动（egui 收到输入会自动出帧）+ 一个低频心跳，
        // 与编辑页空闲时的策略一致。**早退分支必须调它**，否则 egui 出几帧就彻底停下。
        let fps = if self.args.idle_fps > 0.0 { self.args.idle_fps } else { 1.0 };
        ctx.request_repaint_after(Duration::from_secs_f64(1.0 / fps));
    }

    /// 自截屏（`--shot`）：**启动页与编辑页都要能截**，所以从 UI 主体里抽出来单独调用。
    ///
    /// 决策本身在 `shot::shot_step`（纯函数 + 单测）。这里只负责读输入、执行动作 ——
    /// 上一版把决策内联在这里、抽方法时**漏掉了 `--shot` 的守卫**，于是没给 `--shot` 时
    /// 也会在默认的第 30 帧发一次截图请求，拿到图就对 `None` 做 `unwrap()`：
    /// **鼠标一动就疯狂重绘、30 帧几秒就到 ⇒ "一移动就崩"**（用户报的那个 panic）。
    fn handle_shot(&mut self, ctx: &egui::Context) {
        let got = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let step = opm_app::shot::shot_step(
            self.args.shot.as_deref(),
            self.frames as u32,
            self.args.shot_frame,
            got.is_some(),
        );
        match (step, got) {
            (opm_app::shot::ShotStep::Save(path), Some(img)) => {
                match opm_app::shot::write_png_image(&img, std::path::Path::new(&path)) {
                    Ok(()) => println!("  已自截屏          : {path}（{}×{}）", img.width(), img.height()),
                    Err(e) => eprintln!("  自截屏失败: {e}"),
                }
                let _ = std::io::stdout().flush();
                if self.args.shot_exit {
                    self.quit_now(&ctx);
                } else {
                    self.args.shot = None;
                }
            }
            (opm_app::shot::ShotStep::Request, _) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            }
            _ => {}
        }
    }

    /// 启动页列表快照：**在事件上重算**（列表变了 / 时刻走远了），不是每帧。
    ///
    /// 快照里包含"文件还在不在"（一次 `stat`）与"[格式] 多久以前"（一次 `format!`）——
    /// 两者都只跟列表内容与当前时刻有关，与帧无关。见 [`LIST_ROWS_MAX_AGE`]。
    fn refresh_list_rows(&mut self) {
        self.list_rows_at = recents::now_secs();
        self.list_rows = recents::list_rows(&self.recents, self.list_rows_at);
    }

    /// 只在"超龄"时重算（`now - built_at >= LIST_ROWS_MAX_AGE`）。判据是**秒**，不是帧号。
    fn refresh_list_rows_if_stale(&mut self) {
        if recents::now_secs().saturating_sub(self.list_rows_at) >= LIST_ROWS_MAX_AGE {
            self.refresh_list_rows();
        }
    }

    /// 处理"回启动页并摆出「新建谱面」模态"的待办（换标题要 `Context`，只能在帧里做）。
    /// **启动分支与编辑页都要调** —— 启动分支早退，漏调就永远切不过去。
    fn pump_launch_new(&mut self, ctx: &egui::Context) {
        if !self.pending_launch_new {
            return;
        }
        self.pending_launch_new = false;
        self.phase = LaunchPhase::StartScreen;
        self.new_form_open = true;
        // 回启动页 = 重新看到那份列表：顺手把快照刷新（列表内容可能已经变了）
        self.refresh_list_rows();
        // **只换标题**：尺寸两页共用，来回切都不动窗口（用户要求）
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(LAUNCH_TITLE.to_owned()));
    }

    /// 进编辑页：**同一个窗口**换标题（尺寸与启动页一致，刻意不动 —— 见 `LAUNCH_TITLE` 的说明）
    fn enter_editor(&mut self, ctx: &egui::Context) {
        self.phase = LaunchPhase::Editor;
        let (name, file) = {
            let c = self.core.lock().unwrap();
            (
                c.doc().meta.name.clone(),
                c.path()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(str::to_owned),
            )
        };
        let title = match file {
            Some(f) => format!("OpenPhM — {name}（{f}）"),
            None => format!("OpenPhM — {name}（未保存）"),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        println!("  进入编辑页            : 是（窗口尺寸不变）");
    }

    /// 把统计写进共享槽（控制通道读它、调试面板显示它）。
    ///
    /// **不是每帧都写**：这里面 p50 要对最多 512 个样本排序，另有几次锁与字符串克隆，
    /// 而它们是**诊断量**。判据两条：收到了新广播（有真实更新就立刻反映，别让
    /// "等广播被应用"的轮询多等一拍）或距上次已过 [`STATS_MIN_INTERVAL`]。
    fn publish_stats(&mut self) {
        let now = Instant::now();
        let fresh_broadcast = self.applied_broadcasts != self.stats_published_broadcasts;
        if !fresh_broadcast && now.duration_since(self.stats_at) < STATS_MIN_INTERVAL {
            return;
        }
        self.stats_at = now;
        self.stats_published_broadcasts = self.applied_broadcasts;
        let p50 = {
            let mut v = self.latencies.clone();
            if v.is_empty() {
                f64::NAN
            } else {
                // NaN 会让 partial_cmp 返回 None：`unwrap()` 就成了"某个数坏掉 = 整个程序崩"。
                // 统计值坏掉最多是统计不准，不该拖垮编辑器 —— 当作相等排序即可。
                v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                v[(v.len() - 1) / 2]
            }
        };
        let mut s = self.stats.lock().unwrap();
        s.frames = self.frames as u64;
        s.broadcasts = self.applied_broadcasts;
        s.builds_structure = self.builds_structure;
        s.builds_props = self.builds_props;
        s.builds_notes = self.builds_notes;
        s.builds_tracks = self.builds_tracks;
        s.builds_meta = self.builds_meta;
        s.builds_inspector = self.builds_inspector;
        s.skipped_structure = self.skipped_structure;
        s.skipped_props = self.skipped_props;
        s.skipped_notes = self.skipped_notes;
        s.skipped_tracks = self.skipped_tracks;
        s.skipped_inspector = self.skipped_inspector;
        s.pending = self.pending_dispatch.is_some();
        s.seen_revision = self
            .core
            .lock()
            .map(|c| c.revision())
            .unwrap_or_default();
        s.last_broadcast = self.last_broadcast.clone();
        s.last_topics = self.last_topics.clone();
        s.latency_p50_ms = p50;
        s.latency_last_ms = self.latencies.last().copied().unwrap_or(f64::NAN);
        // 播放与音频：视图命令的效果、以及"音频是不是唯一时钟"的证据
        s.playing = self.state.playing;
        s.playhead_sec = self.state.playhead;
        s.playhead_beat = self.state.chart.tmap.beat(self.state.playhead);
        s.conflicts = self.conflicts.len() as u64;
        s.overlay_visible = self.overlay_visible;
        s.overlay_h_held = self.h_held;
        s.overlay_beats = self.state.overlay_beats;
        s.window_offset_x = self.state.window_offset_x;
        s.audio_rate_ppm = self.audio_rate_ppm;
        s.audio_dev_ms = self.audio_dev_ms;
        s.audio_window_s = self.audio_window_s;
        match &self.audio {
            Some(a) => {
                s.audio = a.path.clone();
                s.audio_device = a.device();
                s.audio_pos_ms = a.position_sec() * 1000.0;
                s.audio_latency_ms = a.latency_ms();
                s.audio_underruns = a.underruns();
                s.audio_offset_ms = a.offset_ms();
            }
            None => {
                s.audio = "（无）".into();
                s.audio_device = String::new();
                s.audio_pos_ms = f64::NAN;
                s.audio_latency_ms = f64::NAN;
                s.audio_underruns = 0;
                s.audio_offset_ms = self.args.audio_offset_ms;
            }
        }
    }

    fn report(&mut self) {
        self.reported = true;
        let _ = std::io::stdout().flush();
        fn pct(v: &mut [f64], q: f64) -> f64 {
            if v.is_empty() {
                return f64::NAN;
            }
            // NaN 会让 partial_cmp 返回 None：`unwrap()` 就成了"某个数坏掉 = 整个程序崩"。
                // 统计值坏掉最多是统计不准，不该拖垮编辑器 —— 当作相等排序即可。
                v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            v[((v.len() as f64 - 1.0) * q).round() as usize]
        }
        let n = self.deltas.len();
        let mut d = self.deltas.clone();
        let mut u = self.ui_ms.clone();
        let mut b = self.build_ms.clone();
        let inst_max = self.inst_counts.iter().copied().max().unwrap_or(0);
        let inst_avg = if self.inst_counts.is_empty() {
            0.0
        } else {
            self.inst_counts.iter().sum::<usize>() as f64 / self.inst_counts.len() as f64
        };
        let d50 = pct(&mut d, 0.5);
        println!("\n== bench 结果（{n} 帧）==");
        println!("  适配器            : {}", self.adapter);
        println!("  整帧 p50 / p99    : {d50:.3} / {:.3} ms  ({:.1} fps)", pct(&mut d, 0.99), 1000.0 / d50);
        println!("  UI 构建 p50 / p99 : {:.3} / {:.3} ms", pct(&mut u, 0.5), pct(&mut u, 0.99));
        println!("  实例构建 p50/p99  : {:.3} / {:.3} ms", pct(&mut b, 0.5), pct(&mut b, 0.99));
        println!("  实例数 平均/峰值  : {inst_avg:.0} / {inst_max}");
        println!(
            "  回调 paint        : {:.1} µs（CPU 录制）",
            self.paint_us.load(Ordering::Relaxed) as f64
        );
        if let Some(t0) = self.idle_start {
            let secs = t0.elapsed().as_secs_f64();
            println!(
                "  空闲阶段          : {:.2}s 内 {} 帧 ⇒ **{:.2} fps**（目标 {} fps）",
                secs,
                self.idle_frames,
                self.idle_frames as f64 / secs.max(1e-6),
                self.args.idle_fps
            );
        }
    }
    /// **启动页一帧**：列表 + 两个模态 + 把用户的选择施加下去。
    ///
    /// 它自带"本帧到此为止"的收尾（截屏 / 统计 / 帧计数 / 心跳）—— 启动页与编辑页是同一个窗口的
    /// 两个页面，启动页这一帧画完就没有别的活了。收尾放在这里而不是调用点，是因为**早退分支
    /// 最容易被漏掉某一步**（这个项目已经踩过两次：漏字体装载 ⇒ 中文豆腐块；漏 `pace` ⇒ 只出几帧就停）。
    /// **从解压缓存继续**（启动页那个「上次没有正常退出」对话框选「继续」走这里）。
    ///
    /// 与 [`App::open_doc`] 是同一套收尾（同步文件字段 / 记最近打开 / 重新装载音乐），
    /// 区别只在来源：文档来自缓存目录，**保存目标从会话元数据恢复**（继续之后 Ctrl+S 写回
    /// 原来那个文件，而不是写进 `/tmp`）。
    fn continue_cached(&mut self, dir: &std::path::Path) -> bool {
        let r = {
            let mut c = self.core.lock().unwrap();
            c.load_session_into(dir)
        };
        match r {
            Ok(info) => {
                self.console_log.push((
                    true,
                    format!(
                        "已从缓存继续「{}」（{} 个资源{}）",
                        info.name,
                        info.assets,
                        if info.unsaved { "，含未保存的改动" } else { "" }
                    ),
                ));
                if let Some(p) = &info.source {
                    self.console_log
                        .push((true, format!("保存目标：{}", p.display())));
                }
                // 缓存里那份可能比磁盘上的文件新：先按"未保存"呈现
                //（`sync_file_fields` 内部的 `refresh_file_badge` 已按当前核心重算脏标识）
                self.after_document_loaded("从缓存继续");
                true
            }
            Err(e) => {
                self.console_log.push((false, format!("从缓存继续失败：{e}")));
                false
            }
        }
    }

    /// **编辑期把文档快照写回解压缓存**（节流：最多每 [`SNAPSHOT_MIN_INTERVAL`] 一次）。
    ///
    /// 为什么要有：进程被强杀时磁盘上的谱面文件停在上一次保存，编辑期的改动本来一个字节都不剩。
    /// 缓存目录是这次会话的工作副本，顺手把文档写进去 ⇒ 下次启动那句「继续此谱面」才有意义
    /// （否则它只能重新打开一份旧文件，用户看到的还是"全丢了"）。
    ///
    /// 只在**脏**的时候写：干净就意味着文档与文件一致，不必动缓存。
    fn maybe_snapshot(&mut self, now: Instant) {
        if now.duration_since(self.snapshot_at) < SNAPSHOT_MIN_INTERVAL {
            return;
        }
        let (dirty, has_dir) = {
            let c = self.core.lock().unwrap();
            (c.is_dirty(), c.asset_dir().is_some())
        };
        if !dirty || !has_dir {
            return;
        }
        self.snapshot_at = now;
        let r = {
            let mut c = self.core.lock().unwrap();
            c.snapshot_session()
        };
        match r {
            Ok(()) => self.snapshot_err = None,
            // 失败**只在原因变化时报一次**：这是每两秒重试的兜底，不能每两秒刷一行日志
            Err(e) => {
                if self.snapshot_err.as_deref() != Some(e.as_str()) {
                    self.console_log.push((false, format!("缓存快照失败：{e}")));
                    self.snapshot_err = Some(e);
                }
            }
        }
    }

    /// **被"已经有一个会话在运行"挡住的那一屏**。
    ///
    /// 这一份实例**什么都不碰**：没抢到锁 ⇒ 不载入文档、不占控制 socket、退出也不清理任何缓存
    /// （清理会删掉正在跑的那个会话的工作副本）。给用户的出口只有"关闭"。
    fn busy_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, egui::Color32::from_rgb(20, 22, 30));
        // 模态底下仍照画一屏（模态会把它压暗）：与启动页同一族配色
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.heading("OpenPhM");
            opm_app::dialog::hint(ui, "这一份没有启动：同一时刻只允许一个会话");
        });
        if opm_app::recents::session_busy_modal(ctx, self.busy_who.as_ref()) {
            self.quit_now(ctx);
        }
        self.finish_launch_frame(ctx);
    }

    /// 启动页/阻断页每帧的收尾：自截屏 → 统计 → 帧计数 → 心跳。
    ///
    /// 抽出来是因为它有三个出口（正常走完、遗留缓存对话框选了"继续"、阻断页），
    /// 少调一个就会出现"截屏永远超时 / 统计不动 / 页面卡住不再重绘"这类难查的毛病。
    fn finish_launch_frame(&mut self, ctx: &egui::Context) {
        self.handle_shot(ctx);
        self.publish_stats();
        self.frames += 1;
        self.pace(ctx);
    }

    fn launch_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.pump_launch_new(ctx);
        // 这一帧问过遗留缓存吗（问了就不再让别的启动期钩子在同一帧替用户做第二个决定）
        let mut asked_this_frame = false;
        // ---- 「上次没有正常退出」：**先问这个** ----
        //
        // 它画在列表之上（`egui::Modal` 自带遮罩并吞掉下层输入），而且**先于**列表处理 Esc ——
        // 于是这个 Esc 归弹窗（`dialog::modal` 内部 `consume_key`），列表的"跳过直接进编辑器"
        // 看不见它（这条归属规则在 `dialog` 模块的单测里钉着）。
        if let Some(offer) = self.resume.clone() {
            asked_this_frame = true;
            let choice = match self.resume_auto.as_deref().map(str::trim) {
                // 自动化钩子：没人能替 agent 点这个按钮（与 `OPM_LAUNCH_AUTO` 同类）
                Some("continue") => Some(opm_app::recents::ResumeChoice::Continue),
                Some("discard") => Some(opm_app::recents::ResumeChoice::Discard),
                Some("later") => Some(opm_app::recents::ResumeChoice::Later),
                Some(other) => {
                    eprintln!("  ⚠️ OPM_RESUME_AUTO：只认 continue/discard/later，收到 {other:?}");
                    opm_app::recents::resume_cache_modal(ctx, &offer.item, offer.others)
                }
                None => opm_app::recents::resume_cache_modal(ctx, &offer.item, offer.others),
            };
            // `None` = 用户还没点：**弹窗继续开着**（帧照画，遮罩会把底下的启动页压暗）
            if let Some(choice) = choice {
                self.resume = None;
                match choice {
                    opm_app::recents::ResumeChoice::Continue => {
                        if self.continue_cached(&offer.item.dir) {
                            self.enter_editor(ctx);
                        } else {
                            // 继续失败（缓存被外力删了之类）：留在启动页，原因已经进了控制台日志
                            self.file_message = Some((
                                false,
                                format!("继续「{}」失败——缓存可能已经被清理掉了", offer.item.name()),
                            ));
                        }
                        // 这一帧到此为止（与列表动作那条路一样要收尾：截屏/统计/帧计数/心跳）
                        self.finish_launch_frame(ctx);
                        return;
                    }
                    opm_app::recents::ResumeChoice::Discard => {
                        let (n, freed) = opm_app::session::discard(std::slice::from_ref(&offer.item));
                        self.console_log.push((
                            true,
                            format!(
                                "已丢弃遗留缓存 {n} 份 / {}（谱面文件没动）",
                                opm_app::session::size_text(freed)
                            ),
                        ));
                        self.file_message =
                            Some((true, format!("已丢弃「{}」的遗留缓存", offer.item.name())));
                    }
                    opm_app::recents::ResumeChoice::Later => {
                        self.console_log.push((
                            true,
                            format!("遗留缓存留在 {}（下次启动再问）", offer.item.dir.display()),
                        ));
                    }
                }
            }
        }
        let screen = ui.max_rect();
        let native = filedialog::availability();
        let msg = self.file_message.clone();
        // 行快照只在"列表变了"或"时刻走远了（30 秒）"时重算 —— 不是每帧
        self.refresh_list_rows_if_stale();
        // 缺 7z = 一道**关不掉的门槛**（`.opm` 容器靠它打包/解包，没它交付不出正式格式）
        let gated = self.seven_zip_missing.is_some();
        let mut action = opm_app::recents::start_screen_ui(
            ui,
            screen,
            &self.list_rows,
            msg.as_ref(),
            native,
            gated || self.new_form_open || asked_this_frame,
        );
        // 「新建谱面」：盖在列表上的模态（`opm_new_chart`）
        if self.new_form_open {
            if let Some(a) =
                opm_app::recents::new_chart_modal(ctx, &mut self.new_form, msg.as_ref())
            {
                action = Some(a);
            }
        }
        if gated {
            // 门槛期间列表的动作一律作废（遮罩底下本来就点不到，这里是第二道保险）
            action = None;
            if self.missing_7z_gate(&ctx) || ctx.input(|i| i.viewport().close_requested()) {
                self.quit_now(&ctx);
            }
        }
        // 自动化钩子（截图/CI 用）：`OPM_LAUNCH_AUTO=skip|new|create:<曲名>|open:<path>|recent:<n>`
        // —— 没人点鼠标时也能把"选完切编辑页"这一步走完。**只在这里生效**，不影响交互路径。
        // 门槛期间不生效：否则 `OPM_LAUNCH_AUTO=skip` 就成了绕过 7z 检查的后门。
        if action.is_none() && !gated && !asked_this_frame && self.frames >= 2 {
            // 环境变量在启动时读一次（`App::launch_auto`）：每帧查一次就要分配一个 String，
            // 而这是纯粹的启动期钩子，帧里不该出现 env 查询。
            if let Some(auto) = self.launch_auto.as_deref() {
                action = match auto.trim() {
                    "skip" => Some(opm_app::recents::StartAction::Skip),
                    "new" => Some(opm_app::recents::StartAction::ShowNewForm),
                    other if other.starts_with("open:") => Some(
                        opm_app::recents::StartAction::OpenRecent(std::path::PathBuf::from(
                            &other[5..],
                        )),
                    ),
                    // `create:<曲名>`：填好表单直接创建（把"填表→建谱→编辑页"整条走完）
                    other if other.starts_with("create:") => {
                        self.new_form.name = other[7..].to_owned();
                        Some(opm_app::recents::StartAction::Create)
                    }
                    other if other.starts_with("recent:") => other[7..]
                        .parse::<usize>()
                        .ok()
                        .and_then(|i| self.recents.entries.get(i).map(|e| e.path.clone()))
                        .map(opm_app::recents::StartAction::OpenRecent),
                    _ => None,
                };
            }
        }
        match action {
            Some(opm_app::recents::StartAction::OpenRecent(p)) => {
                self.open_doc(&p.display().to_string());
                if self.core.lock().map(|c| c.path().is_some()).unwrap_or(false) {
                    self.enter_editor(ctx);
                }
            }
            Some(opm_app::recents::StartAction::OpenDialog) => {
                self.open_via_system();
                if self.core.lock().map(|c| c.path().is_some()).unwrap_or(false) {
                    self.enter_editor(ctx);
                }
            }
            // 「新建谱面…」：**不换屏**，只是把模态打开（窗口标题/尺寸都不动）
            Some(opm_app::recents::StartAction::ShowNewForm) => {
                self.new_form_open = true;
                self.file_message = None;
            }
            // 模态关掉（返回列表 / Esc / 点遮罩）
            Some(opm_app::recents::StartAction::BackToList) => {
                self.new_form_open = false;
                self.file_message = None;
            }
            // 选资源（音乐 / 曲绘）：**过滤器与写回字段由库里 `StartAction::asset()` 给** ——
            // 用户报过"选音乐的系统框只列 json"，根因就是这一处的过滤器硬编码在 bin 里、测不到。
            Some(
                action @ (opm_app::recents::StartAction::PickAudio
                | opm_app::recents::StartAction::PickIllustration),
            ) => {
                // 新增资源类型时这里会**编译不过**（or 模式没覆盖到），不会被悄悄漏掉
                let spec = action.asset().expect("这两个动作一定有 asset()");
                let start = filedialog::start_for_open(None);
                match filedialog::pick(filedialog::Which::Open, start.as_deref(), spec.filter) {
                    Ok(Some(p)) => {
                        self.new_form.set_asset(spec.field, &p.display().to_string());
                        self.file_message = Some((true, format!("{} → {}", spec.what, p.display())));
                    }
                    Ok(None) => {}
                    Err(e) => self.file_message = Some((false, e)),
                }
            }
            // 校验通过**且命令真的成功**才进编辑页：失败就留在模态里看原因
            Some(opm_app::recents::StartAction::Create) => {
                if self.create_new_doc() {
                    self.enter_editor(ctx);
                }
            }
            Some(opm_app::recents::StartAction::Skip) => self.enter_editor(ctx),
            Some(opm_app::recents::StartAction::Forget(p)) => {
                self.recents.forget(&p);
                let _ = self.recents.save_default();
                self.refresh_list_rows();
            }
            Some(opm_app::recents::StartAction::ClearAll) => {
                self.recents.entries.clear();
                let _ = self.recents.save_default();
                self.refresh_list_rows();
            }
            Some(opm_app::recents::StartAction::Notice(t)) => {
                self.file_message = Some((false, t));
            }
            None => {}
        }
        self.finish_launch_frame(ctx);
        if self.frames == 1 {
            if let Some(t0) = self.startup_t0.filter(|_| self.trace_startup) {
                println!(
                    "  启动耗时          : +{:7.1} ms  **首帧（启动页）构建完成** —— 之后交给合成器显示",
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
        } else if self.frames == 2 {
            // 第二帧 = 稳态：首帧里那几百毫秒如果在这里消失，就说明它是**一次性**开销（字体图集等）
            if let Some(t0) = self.startup_t0.filter(|_| self.trace_startup) {
                let ui_ms = self.ui_ms.last().copied().unwrap_or(f64::NAN);
                println!(
                    "  启动耗时          : +{:7.1} ms  第二帧（稳态，UI {ui_ms:.1} ms）",
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
    }
}

impl eframe::App for App {
    /// 清屏色：**必须不透明**。
    ///
    /// eframe 的默认值带 alpha=180（`from_rgba_unmultiplied(12,12,12,180)`），于是每帧的清屏
    /// 只是"盖一层半透明"——上一帧的内容会透过来。启动页→编辑页切换时表现为
    /// **启动页的像素糊在编辑页底下**（我截图时一眼看到的那个重影就是这个）。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        match self.phase {
            LaunchPhase::Editor => egui::Color32::from_rgb(12, 13, 16).to_normalized_gamma_f32(),
            // 启动页用同一族的深色，但slightly偏冷，与编辑页一眼可分
            _ => egui::Color32::from_rgb(20, 22, 30).to_normalized_gamma_f32(),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let t_ui = Instant::now();
        let ctx = ui.ctx().clone();

        if !self.inited {
            // 首帧的两段初始化（wgpu 回调资源 + 字体）也计入启动耗时
            let mark = |me: &Self, what: &str, t: Instant| {
                if let (true, Some(t0)) = (me.trace_startup, me.startup_t0) {
                    println!(
                        "  启动耗时          : +{:7.1} ms（本步 {:7.1} ms）  {}",
                        (t - t0).as_secs_f64() * 1000.0,
                        t.elapsed().as_secs_f64() * 1000.0,
                        what
                    );
                }
            };
            let t_step = Instant::now();
            if let Some(rs) = frame.wgpu_render_state() {
                let info = rs.adapter.get_info();
                self.adapter = format!("{} [{:?}/{:?}]", info.name, info.backend, info.device_type);
                println!("  适配器            : {}", self.adapter);
                println!("  目标格式          : {:?}", rs.target_format);
                // 边界压暗要按目标色彩空间换算（sRGB 目标的线性混合会让同样的 alpha 观感变弱）
                self.state.boundary_dim =
                    render::dim_alpha_for(render::DIM_ALPHA_DEFAULT, rs.target_format);
                println!("  边界压暗 alpha    : {:.3}（观感目标 {:.2}）", self.state.boundary_dim, render::DIM_ALPHA_DEFAULT);
                let pf = Playfield::new(&rs.device, rs.target_format);
                rs.renderer.write().callback_resources.insert(pf);
            }
            mark(self, "wgpu 渲染回调资源（Playfield）", t_step);
            let t_fonts = Instant::now();
            let f = fonts::install(&ctx);
            println!("  CJK 字体          : {}", f.desc);
            fonts::install_kr_fallback(&ctx);
            if let Some(s) = self.args.scale {
                ctx.set_pixels_per_point(s);
            }
            // 把 ctx 交给控制线程：远端改完之后能立刻唤醒一帧
            if let Ok(mut g) = self.ctx_slot.lock() {
                *g = Some(ctx.clone());
            }
            mark(self, "CJK 字体装载（读中文/韩文字体文件）", t_fonts);
            self.inited = true;
            if let Some(t0) = self.startup_t0.filter(|_| self.trace_startup) {
                println!(
                    "  启动耗时          : +{:7.1} ms  首帧初始化结束（此后是首帧 UI 构建）",
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            }
        }

        // ---- 退出检查：**关窗是唯一没有撤销机会的操作** ----
        //
        // 走 eframe 的官方否决式（`eframe::epi` 里就写着这条用法）：看到关闭请求先 `CancelClose`，
        // 然后决定是放行、还是把"未保存守卫"摊开让用户选（保存 / 不保存 / 返回）。
        // `quit_allowed` 只给**我们自己**发的 Close 用（截屏、bench、7z 门槛都走 `quit_now`）——
        // 不加这个旗子，程序化退出会被自己的守卫拦下来等人点按钮。
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_allowed {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            let dirty = self.core.lock().map(|c| c.is_dirty()).unwrap_or(false);
            if dirty {
                self.guard_for = Some(GuardAction::Quit);
            } else {
                self.pending_quit = true; // 干净就走，不停留
            }
        }
        if self.pending_quit {
            self.pending_quit = false;
            self.quit_now(&ctx);
        }
        // 自动化钩子：把"点右上角的叉"变成可复现的一步（`OPM_CLOSE_AUTO=<帧号>`）。
        // **故意不立 `quit_allowed`** —— 要的就是"用户点了叉"这件事本身，守卫该拦就得拦。
        if self.close_auto.is_some_and(|n| u64::from(self.frames) >= n) {
            self.close_auto = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // 守卫画在最前面也能盖住整页（`egui::Modal` 的层级与调用顺序无关），
        // 于是启动页与编辑页共用同一个守卫 —— 关窗不再有"哪一页才有效"的区别。
        self.unsaved_guard(&ctx);

        // ---- 自动化钩子：`OPM_KEY_AUTO=[帧号:]按键[,按键…]` ----
        //
        // 例：`OPM_KEY_AUTO=40:Delete`、`OPM_KEY_AUTO=ctrl+z`。
        //
        // 为什么需要它：**没人能往 Wayland 窗口注入按键**（xdotool 要 `DISPLAY`），
        // 而"按键 → 动作"这一段（Del 删选区、Ctrl+Z 撤销）恰恰是最容易接线接错的地方。
        // 这里把按键塞进 egui 本帧的输入里，于是真的走一遍**和用户按下去完全相同**的路径
        // （门槛也一样：在文本框里打字时不吃）。帧号前缀是为了等前面的视图命令到位
        // —— 第 1 帧选区还是空的，那时按 Del 只会说一句"没有选中的东西"。
        // 与 `OPM_EDIT_AUTO` 同一条纪律：只为拍不出来的中间态/一步操作存在，不改变默认行为。
        while self
            .key_auto
            .first()
            .is_some_and(|(at, _)| self.frames >= *at)
        {
            let (_, spec) = self.key_auto.remove(0);
            let mut injected = 0usize;
            for name in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let (mods, key) = parse_key_spec(name);
                if let Some(key) = key {
                    ctx.input_mut(|i| {
                        // 修饰键要**直接写字段**：`InputState::begin_pass` 是从 RawInput 的
                        // 事件里算出 `modifiers` 的，往 `i.events` 里塞 `ModifiersChanged`
                        // 已经太晚（那一遍循环早就过去了）—— 实测：塞事件只让 `key_pressed` 为真，
                        // `modifiers.command` 仍是 false，于是 `ctrl+z` 一声不响地什么也没做。
                        // 写字段与真实事件的效果完全相同（`begin_pass` 也是这么赋的）。
                        i.modifiers = mods;
                        i.events.push(egui::Event::Key {
                            key,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers: mods,
                        });
                    });
                    injected += 1;
                } else {
                    eprintln!("  ⚠️ OPM_KEY_AUTO：认不出 {name:?}（例：Delete / ctrl+z）");
                }
            }
            if injected > 0 {
                println!("  OPM_KEY_AUTO     : 帧 {} 注入 {injected} 次按键（{spec}）", self.frames);
            }
        }

        // ---- 「已经有一个会话在运行」：这一份实例什么都不碰，只说明情况 ----
        // 放在最前面（除退出处理之外）：它不该进编辑页、不该碰缓存、也不该跑任何启动期动作。
        if self.busy_who.is_some() {
            self.busy_page(ui, &ctx);
            return;
        }

        // ---- 启动页：**一屏**（谱面列表）+ 盖在它上面的模态 ----
        //
        // 「新建谱面」与「缺少 7z」都不是另一屏，而是**模态**（同一个 `dialog` 模块画出来的，
        // 与编辑页的文件对话框/未保存守卫同一套外观）：底下那一屏照画，只是被遮罩压暗并吞掉输入。
        // 整屏换内容会让人以为"进了另一个程序"，弹窗才是"这一屏上的一个问题"。
        //
        // 这一屏自己带"本帧到此为止"的收尾（截屏/统计/帧计数/心跳），所以整块抽成方法：
        // `ui()` 的主干只留编辑页那一条路。
        if self.phase == LaunchPhase::StartScreen {
            self.launch_page(ui, &ctx);
            return;
        }
        self.pump_launch_new(&ctx);

        // **进编辑页先把整窗铺一层不透明底色**：启动页→编辑页会换内容（尺寸不再变），
        // 而尺寸变化时 wgpu 的 surface 会重建、上一帧的像素可能还留在那里；只有面板覆盖的区域
        // 才会被重画，中央区就露馅（我截图时看到的"启动页糊在编辑页底下"就是这个）。
        // 一行底色把这件事一次性解决，也不依赖 eframe 的清屏时机。
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, egui::Color32::from_rgb(12, 13, 16));
        // 编辑页也要截屏（启动分支各自调用过 `handle_shot`；这里补上编辑页那一次）
        self.handle_shot(&ctx);


        // ---- 自截屏：请求 → 下一帧收图 → 写 PNG ----
        // egui 把截图结果作为 `Event::Screenshot` 回灌到 raw events 里（不是 frame.screenshot()）。
        // 走这条路，截图就与窗口位置、合成器、遮挡完全无关。
        // ---- 空格键：**单点 = 进入/退出自动播放；长按 = 按住播放、松手退出** ----
        //
        // 规则本身在 `keymap::SpacePlayback`（纯状态机，四种组合都有单测）；这里只负责
        // 把"按下/松开/当前时刻/当前播放状态"喂进去并执行返回的动作。
        // 注意 `wants_keyboard_input()`：控制台输入框获得焦点时，空格是"打字"，
        // 不能被当成播放快捷键（否则在命令行里敲空格就跳播）。
        let typing = ctx.egui_wants_keyboard_input();
        // 文件/新建/未保存守卫/缺 7z 对话框打开时也不吃快捷键：模态框在上，空格不该把谱面播起来
        let modal_open = self.file_dialog_open
            || self.guard_for.is_some()
            || self.new_form_open
            || self.seven_zip_missing.is_some();
        let (space_down, space_up, now) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_released(egui::Key::Space),
                i.time,
            )
        });
        // 长按阈值只有一处（`keymap::SPACE_HOLD_SECS`）——不加旋钮：多一个旋钮就多一种"说不清"的手感
        let allowed = keymap::shortcut_allowed(typing, modal_open);
        if let Some(play) = self.space_play.step(allowed, space_down, space_up, now, self.state.playing)
        {
            self.set_playing(play);
        }
        // 失焦复位：回来时不该以为空格一直按着
        if !ctx.input(|i| i.focused) {
            self.space_play.cancel();
        }
        // Ctrl+S 保存（与顶栏按钮同一条路径）；Ctrl+Shift+S 另存为；Ctrl+O 打开
        let (cmd_s, shift_s, cmd_o) = ctx.input(|i| {
            (
                i.modifiers.command && i.key_pressed(egui::Key::S),
                i.modifiers.shift,
                i.modifiers.command && i.key_pressed(egui::Key::O),
            )
        });
        if !typing && cmd_s {
            if shift_s {
                self.save_as_via_system(); // Ctrl+Shift+S = 系统"另存为"
            } else {
                self.save_doc();
            }
        }
        if !typing && cmd_o {
            self.open_via_system(); // Ctrl+O = 系统"打开"
        }
        // 撤销/重做：`Ctrl+Z` / `Ctrl+Shift+Z`（键位表在 `keymap::edit_shortcut`，有单测）。
        // 与 Ctrl+S 同一条纪律：**在文本框里打字时不吃**（那时的 Ctrl+Z 归文本框自己）。
        if keymap::shortcut_allowed(typing, modal_open) {
            // "按键 + 修饰键 → 动作"整条链在库里（`keymap::edit_action_from_input`），
            // 这里只剩"取到就执行" —— bin 里没有可测的逻辑，也不该有
            let hit = ctx.input(keymap::edit_action_from_input);
            if let Some(action) = hit {
                self.apply_edit_action(action);
            }
        }
        // **Del：删掉选中的音符/事件**。
        //
        // 为什么放在全局这一层而不是编辑区里：它是"作用在选区上"的命令，
        // 不该要求"指针正好悬在编辑区上"。门槛与 Ctrl+Z 一致（打字/模态期间不吃）。
        if keymap::shortcut_allowed(typing, modal_open)
            && ctx.input(keymap::delete_selection_pressed)
        {
            self.delete_selection();
        }
        // 按住 H：临时藏掉编辑区（放开即恢复）。同样不能在控制台打字时误触发。
        self.h_held = !typing && !modal_open && ctx.input(|i| i.key_down(egui::Key::H));
        // 可见性规则抽成纯函数（有单测）：**自动播放中或按住 H 时隐藏**
        self.overlay_visible = overlay::overlay_visible(self.state.overlay_enabled, self.state.playing, self.h_held);

        // ---- 控制通道的视图命令（play/pause/seek/audio）----
        self.pump_view_cmds();

        // ---- 唯一的更新入口：抽广播 → 置脏 → 只重建脏掉的那块 ----
        // GUI 不轮询 `revision`、不直接读文档判断"要不要更新"：文档什么时候变了，由 EditCore 说。
        self.pump_broadcasts();
        // ---- 音符位置重算：**异步补一小段**（在帧里做，不开线程）----
        //
        // 流速事件一改，它之后的音符位置就过期了（`Line::set_tracks` 把那段标脏）。
        // 这里每帧补 `FLOOR_NOTES_PER_FRAME` 条，顺序是"从当前时间轴 → 结尾"再
        // "从头 → 当前时间轴"（屏幕上马上要用的先算）；没补好的那些音符在渲染侧**现算**，
        // 所以补得快慢都不影响画面 —— 它只决定每帧要花多少代价，进度显示在底栏。
        // 放在 `build_instances` **之前**：这一帧补好的位置这一帧就用上。
        self.state.pump_floors(EditorState::FLOOR_NOTES_PER_FRAME);
        // 编辑期把文档快照写回解压缓存（节流；被强杀时"继续此谱面"才有东西可继续）
        self.maybe_snapshot(Instant::now());

        // 播放头：有音频时由音频游标驱动（见 state::advance），否则墙钟
        self.state.advance(self.audio.as_ref());
        // 音频侧状态先取样（避免同时借用 &self.audio 与 &mut self）
        let audio_state = self.audio.as_ref().map(|a| {
            (
                a.is_playing(),
                a.ended(),
                a.position_sec(),
                a.underruns(),
                a.latency_ms(),
                a.offset_ms(),
                a.device(),
                a.path.clone(),
                a.callbacks(),
                a.duration(),
            )
        });
        if let Some((a_playing, a_ended, a_pos, _ur, _lat, _off, _dev, _path, _cb, _dur)) = &audio_state
        {
            if self.state.playing && *a_ended {
                self.state.set_playing(false);
                // stderr：stdout 重定向到文件是块缓冲，这种"一条就完"的日志会被吞掉
                eprintln!("  音频播放结束      : {a_pos:.3}s");
            }
            if self.state.playing && *a_playing {
                self.track_audio_clock(*a_pos);
            }
        }
        if self.autoplay_pending && !self.pending_layout_anim {
            self.autoplay_pending = false;
            self.set_playing(true);
            println!("  --autoplay        : 已开始播放");
        }
        let t_build = Instant::now();
        if self.args.stress {
            build_instances_all(&self.state, &mut self.instances);
        } else {
            build_instances(&self.state, &mut self.instances);
        }
        // 收尾：窗口边界压暗 + 边框（画在内容之上；与无头出图共用同一份几何）
        render::push_window_overlay(&self.state, &mut self.instances);
        if self.args.verify_align {
            render::push_alignment_markers(&mut self.instances);
        }
        let build_ms = t_build.elapsed().as_secs_f64() * 1000.0;
        let inst_count = self.instances.len();

        // ---- 顶部菜单 ----
        // ---- 顶栏：只放真设置，不放调试开关（调试走 CLI：--stress / --verify-align…）----
        egui::Panel::top("topbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("OpenPhM");
                ui.separator();

                // 传输
                if ui
                    .button(if self.state.playing { "⏸ 暂停" } else { "▶ 播放" })
                    .on_hover_text("空格")
                    .clicked()
                {
                    self.toggle_play();
                }
                if ui.button("⏮").on_hover_text("回到开头").clicked() {
                    self.seek_to(0.0);
                }
                ui.separator();
                // 顶栏**不放保存按钮**（用户要求）：保存只从「文件…」对话框或 Ctrl+S 走 ——
                // 顶栏留出位置给真正需要常驻的东西；是否脏由状态栏的 `•` 与对话框里的提示负责。

                // 谱面摘要（Meta 脏时刷新的缓存，不直接读文档）
                ui.label(self.meta_name.as_str());
                ui.label(format!("♪ {}", self.doc_notes));
                ui.separator();
                // ---- 文件：走**系统文件对话框**（kdialog/zenity），不再自绘路径输入当主入口 ----
                // 保存/另存为两个按钮放在一个"文件"对话框里（点 💾 保存时若还没有路径也会打开它）
                if ui
                    .button("📂 文件…")
                    .on_hover_text("打开 / 保存 / 另存为（系统文件对话框）")
                    .clicked()
                {
                    self.sync_file_fields();
                    self.file_dialog_open = true;
                }
            });
            // ---- 第二行：视图与网格（工具多了就得换行，否则挤成一条看不清）----
            ui.horizontal_wrapped(|ui| {
                // ---- 基础设置：网格 —— 两个方向各一个数，取代原来的两个吸附勾选框 ----
                // 拍方向 = 每拍几条线（决定纵向节拍步长 1/N）；
                // 坐标方向 = 可见窗口几等分（决定横向坐标步长 1350/N）。
                // 音符拖拽/放置**总是**落在两轴网格的交叉点上（见 README「数据边界」与「编辑区」）。
                ui.label("网格");
                ui.add(
                    egui::DragValue::new(&mut self.state.grid.beat_div)
                        .range(1..=64)
                        .speed(0.2)
                        .prefix("拍 每拍 ")
                        .suffix(" 条"),
                )
                .on_hover_text("纵向（拍）网格：每拍几条线 ⇒ 步长 1/N 拍");
                ui.add(
                    egui::DragValue::new(&mut self.state.grid.lane_div)
                        .range(1..=128)
                        .speed(0.2)
                        .prefix("坐标 窗口 ")
                        .suffix(" 等分"),
                )
                .on_hover_text(
                    "横向（坐标）网格：**整个可见窗口平均切成 N 列** ⇒ 步长 1350/N RPE 单位，奇偶都可以。\n                     端点是窗口边界（±675）恒为格线；奇数等分时中轴 laneX=0 不是格点，但中轴线照画。",
                );
                // 规整到合法范围（1..=128）：**不再强制偶数** —— 整窗等分下奇数也放得下
                self.state.grid.lane_div = state::GridCfg::normalize_lane_div(self.state.grid.lane_div);
                // 设定值被缩放顶掉时**明说**：否则用户改数字看不到线变，只会以为坏了
                let eff = self.state.effective_beat_div();
                if eff != self.state.grid.beat_div {
                    ui.label(
                        egui::RichText::new(format!("→ 实际 1/{eff}"))
                            .color(egui::Color32::from_rgb(255, 190, 110)),
                    )
                    .on_hover_text(
                        "当前可见拍数下 1/N 已经密到画不出来（线距 < 1.2px），\n                         画线与吸附一并降到最大可画细分。缩小可见拍数（放大）就能启用更细的网格。",
                    );
                }
                ui.separator();
                // 时间轴缩放（与 Ctrl+滚轮同一条状态；这里给一个可键盘输入的入口）
                ui.add(
                    egui::DragValue::new(&mut self.state.overlay_beats)
                        .range(4.0..=256.0)
                        .speed(1.0)
                        .prefix("可见 ")
                        .suffix(" 拍"),
                )
                .on_hover_text("时间轴缩放：可见拍数越少 = 放得越大（Ctrl+滚轮 同一条状态）");
                ui.separator();
                // ---- 窗口 X 偏移：把音符区显示的 laneX 区间整体平移 ----
                // 官方窗口是 ±675；偏移 ≠ 0 时格线延伸到窗口外 ⇒ 可以查看/编辑越界坐标的音符
                // （那不是错误：`validate` 只报警告"超出 RPE 坐标系 ±675"）。
                ui.label("窗口 X 偏移")
                    .on_hover_text(
                        "把音符区显示的 laneX 区间整体平移：[偏移−675, 偏移+675]。\n                         官方窗口是 ±675；偏移后可以查看、拖拽、放置**窗口外**的音符。\n                         只改编辑区怎么显示与吸附，演奏区预览永远显示真实窗口。\n                         用法：工具条上的 ▶ 播放/⏮ 键位不受影响；音符区滚轮仍是改时间。",
                    );
                ui.add(
                    egui::DragValue::new(&mut self.window_offset_x_ui)
                        .range(-state::EditorState::WINDOW_OFFSET_MAX..=state::EditorState::WINDOW_OFFSET_MAX)
                        .speed(1.0)
                        .suffix(" RPE"),
                );
                // 拖动框只写这个临时值，真正生效走 set_window_offset_x（夹取 + 广播到 ui_stats）
                if (self.window_offset_x_ui - self.state.window_offset_x).abs() > 1e-4 {
                    self.state.set_window_offset_x(self.window_offset_x_ui);
                }
                if ui
                    .button("⟲")
                    .on_hover_text("窗口偏移归零")
                    .clicked()
                {
                    self.state.set_window_offset_x(0.0);
                    self.window_offset_x_ui = 0.0;
                }
                let (lo, hi) = self.state.window_lane_range();
                if self.state.window_offset_x.abs() > 1e-4 {
                    ui.label(
                        egui::RichText::new(format!("显示 {lo:.0}…{hi:.0}"))
                            .color(egui::Color32::from_rgb(255, 190, 110)),
                    )
                    .on_hover_text("当前音符区显示的 laneX 区间（橙色竖线 = 官方窗口边界 ±675）");
                }
                ui.separator();
                // ---- 基础设置（收在"设置"里）----
                ui.menu_button("设置 ⚙", |ui| {
                    ui.checkbox(&mut self.state.overlay_enabled, "编辑区叠加层");
                    ui.add(
                        egui::DragValue::new(&mut self.overlay.body_alpha)
                            .range(0.30..=1.0)
                            .speed(0.01)
                            .prefix("编辑区暗度 ")
                            .fixed_decimals(2),
                    );
                    ui.separator();
                    ui.checkbox(&mut self.state.show_boundary, "窗口边界框");
                    ui.add(
                        egui::DragValue::new(&mut self.state.line_half_w)
                            .range(50.0..=2000.0)
                            .speed(5.0)
                            .prefix("判定线半长 ")
                            .suffix(" RPE"),
                    );
                    ui.separator();
                    ui.checkbox(&mut self.show_console, "命令控制台");
                    if ui.button("校验谱面").clicked() {
                        self.validate_doc();
                    }
                    ui.separator();
                    ui.label("工作区");
                    for w in Workspace::ALL {
                        ui.selectable_value(&mut self.ws, w, w.label());
                    }
                });
            });
        });

        // ---- 底部状态栏：常用状态；性能与更新广播的计数只在"调试"工作区显示 ----
        //
        // 画法与文案在 `statusbar` 模块（可无头测）：这里只把**算好的值**放进快照，
        // 并把面板产出的动作（切换冲突浏览器）施加下去。面板自己不锁核心、不解析路径。
        {
            let eff = self.state.effective_beat_div();
            let btxt = if eff == self.state.grid.beat_div {
                format!("每拍 {} 条", self.state.grid.beat_div)
            } else {
                format!("每拍 {} 条(实际 1/{eff})", self.state.grid.beat_div)
            };
            // 奇数等分时补一句：中轴不再是格点（省得用户以为"吸不到 0"是 bug）
            let odd = if self.state.grid.center_is_lattice() {
                String::new()
            } else {
                "（奇数等分：中轴不是格点）".to_owned()
            };
            let grid_text = format!(
                "网格 {} / 窗口 {} 等分（{:.1} RPE）{}",
                btxt,
                self.state.grid.lane_div,
                self.state.grid.h_step_rpe(),
                odd
            );
            let window_offset = (self.state.window_offset_x.abs() > 1e-4).then(|| {
                let (lo, hi) = self.state.window_lane_range();
                (self.state.window_offset_x, lo, hi)
            });
            let diagnostics = (self.ws == Workspace::Debug).then(|| {
                format!(
                    "实例 {inst_count} 构建 {build_ms:.3} ms 帧 {} ｜ 广播 {} 重建 整表{}/属性{}/音符{}/轨道{} 跳过 整表{}/音符{}/轨道{} ｜ {}",
                    self.frames,
                    self.applied_broadcasts,
                    self.builds_structure,
                    self.builds_props,
                    self.builds_notes,
                    self.builds_tracks,
                    self.skipped_structure,
                    self.skipped_notes,
                    self.skipped_tracks,
                    self.adapter,
                )
            });
            let mut view = statusbar::StatusView {
                playhead: self.state.playhead,
                beat: self.state.chart.tmap.beat(self.state.playhead),
                playing: self.state.playing,
                audio: self.audio.as_ref(),
                audio_offset_ms: &mut self.args.audio_offset_ms,
                grid_text,
                file_badge: self.file_badge.as_str(),
                file_mark: statusbar::file_mark(self.file_dirty, self.file_has_target),
                file_hover: statusbar::file_hover(self.file_has_target, &self.target_text),
                window_offset,
                overlay_hidden: (!self.overlay_visible)
                    .then(|| statusbar::overlay_hidden_text(self.h_held)),
                conflicts: self.conflicts.len(),
                show_conflicts: self.show_conflicts,
                // 编辑器里常驻的只有状态栏 ⇒ 消息也显示在这里（文件对话框里那份照旧）
                notice: self.file_message.clone(),
                // 音符位置重算的进度（流速事件改了之后要异步补的那批活）
                floor_rebuild: self.state.floor_rebuild(),
                diagnostics,
            };
            let act = egui::Panel::bottom("status").show(ui, |ui| {
                statusbar::status_bar_ui(ui, &mut view)
            });
            let act = act.inner;
            if act.toggle_conflicts {
                self.show_conflicts = !self.show_conflicts;
            }
        }
        // 校准偏移是**视图设置**：改完同步给音频流（文档里没有这个字段）
        if let Some(a) = &self.audio {
            a.set_offset_ms(self.args.audio_offset_ms);
        }

        // ---- 冲突浏览器：列出事件重叠的区域，点一下跳到那里 ----
        //
        // 数据来自 `EditCore::overlaps()`（GUI 与 CLI 同一份），画法在 `conflicts` 模块。
        // 这里只施加"跳转"这个**视图动作**：选中线/轨道/事件 + 移动播放头（不碰文档）。
        if self.show_conflicts && !self.conflicts.is_empty() {
            let (jump, close) = conflicts::conflicts_ui(ui, &self.conflicts);
            if close {
                self.show_conflicts = false;
            }
            if let Some(j) = jump {
                self.state.select_line_doc(j.line_doc);
                if let Some(id) = state::TrackId::from_key(&j.track) {
                    self.state.selected_track = id;
                }
                self.state.select_event(self.state.selected_track, j.event);
                let t = self.state.chart.tmap.sec(j.beat);
                self.seek_to(t);
                self.insp = self.build_inspector();
            }
        }

        // ---- 命令控制台（与 opm-ctl 共用命令语言；agent 走 CLI，人走这里） ----
        let mut do_exec = false;
        let mut do_validate = false;
        let mut do_save = false;
        // 对话框第一次出现时把"文件夹 / 谱面名字"填好：`--file-dialog` 直接启动的场景也要有值，
        // 否则用户（和我的截图）看到的是两个空格子加一句"untitled"。
        if self.file_dialog_open && self.edit_name.is_empty() {
            self.sync_file_fields();
        }

        // ---- 起始界面：谱面列表（左＝最近打开，右＝打开/新建）----
        //
        // 打开程序先看这一屏：最近打开的谱面在左，动作在右。它**不是模态框**，而是整屏布局 ——
        // 第一屏没有"下面的编辑器"可遮，用 CentralPanel 直接铺开更简单也更像系统里的启动页。
        // 遮住编辑器不等于禁用：下面那一帧仍然照常构建（见本函数末尾的早退）。
        // ---- 文件对话框：保存 / 另存为 / 打开 / 新建（用系统文件对话框）----
        //
        // 为什么是"对话框"而不是顶栏一排按钮：保存/另存为/打开是**互斥的一次性决定**，
        // 而且要看当前保存目标与格式；挤在工具栏上既占地方又容易点错。用 `egui::Modal`
        // （有背景遮罩 + 吞掉输入）而不是自绘浮层 —— 模态语义由框架保证，别自己糊。
        if self.file_dialog_open {
            let ctx = ui.ctx().clone();
            let mut pending: Option<FileAction> = None;
            let (path, fmt, dirty) = {
                let c = self.core.lock().unwrap();
                (
                    c.path().map(|p| p.display().to_string()),
                    c.source_format().as_str(),
                    c.is_dirty(),
                )
            };
            let native = filedialog::availability();
            let out = opm_app::dialog::modal(
                &ctx,
                "opm_file_dialog",
                opm_app::dialog::W_WIDE,
                |ui| {
                opm_app::dialog::title(ui, "文件");
                ui.horizontal(|ui| {
                    // **保存目标 = 文件夹 + 谱面名字，合成一个东西**：这里只显示它
                    ui.label("保存目标");
                    match &path {
                        Some(p) => opm_app::dialog::path(ui, p),
                        None => opm_app::dialog::warn(
                            ui,
                            "（未指定 —— 保存时会弹保存窗口）",
                        ),
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(format!("格式 [{fmt}]{}", if dirty { "  • 有未保存改动" } else { "" }));
                    ui.label(
                        egui::RichText::new("曲名")
                            .color(egui::Color32::from_rgb(150, 160, 195)),
                    );
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.edit_name)
                            .desired_width(200.0)
                            .hint_text("曲名（meta.name）"),
                    );
                    resp.clone().on_hover_text(
                        "文档里的 `meta.name`（顶栏显示的就是它）；另存为时当默认文件名。\n                         回车或「应用曲名」时生效 —— 逐字生效会往撤销栈里灌一堆步骤。",
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        pending = Some(FileAction::ApplyName);
                    }
                    if ui.button("应用曲名").clicked() {
                        pending = Some(FileAction::ApplyName);
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .button("🆕 新建…")
                        .on_hover_text("新建空谱面（未保存会先问你要不要保存）")
                        .clicked()
                    {
                        pending = Some(FileAction::NewDoc);
                    }
                    if ui
                        .button("📂 打开…")
                        .on_hover_text("系统文件对话框（Ctrl+O）")
                        .clicked()
                    {
                        pending = Some(FileAction::OpenDialog);
                    }
                    if ui
                        .button("💾 保存")
                        .on_hover_text("写回保存目标；没有目标就先弹保存窗口（Ctrl+S）")
                        .clicked()
                    {
                        pending = Some(FileAction::SaveToTarget);
                    }
                    if ui
                        .button("⤓ 另存为…")
                        .on_hover_text("系统文件对话框（Ctrl+Shift+S）")
                        .clicked()
                    {
                        pending = Some(FileAction::SaveAsDialog);
                    }
                    if ui
                        .button("🗂 在文件管理器中显示")
                        .on_hover_text("调用系统文件管理器定位当前文件")
                        .clicked()
                    {
                        pending = Some(FileAction::Reveal);
                    }
                    // 撤销/重做**只有快捷键**（用户点名的就是这两个组合）：把键位写在这儿，
                    // 否则没人会知道它存在 —— 快捷键不该只活在源码里
                    opm_app::dialog::hint(ui, "编辑：Ctrl+Z 撤销 / Ctrl+Shift+Z 重做");
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    // **两个轴**：格式（opm / RPE）× 打包开关 —— 一共就是那四种形态
                    ui.label("另存为");
                    let cur_chart = self.save_format.chart_format();
                    let packed = self.save_format.packed().unwrap_or(true);
                    if ui
                        .selectable_label(self.save_format == core::SaveFormat::Auto, "自动")
                        .on_hover_text("跟着目标走；新建的谱面默认存成 opm 包")
                        .clicked()
                    {
                        self.save_format = core::SaveFormat::Auto;
                    }
                    for (label, f) in [
                        ("opm", opm_app::codec::Format::OpmZip),
                        ("RPE", opm_app::codec::Format::Rpe),
                    ] {
                        if ui
                            .selectable_label(cur_chart == Some(f), label)
                            .clicked()
                        {
                            self.save_format = core::SaveFormat::from_axes(f, packed);
                        }
                    }
                    let mut p = packed;
                    if ui
                        .checkbox(&mut p, "打包成一个文件")
                        .on_hover_text("勾上 = 一个 `.opm`/`.pez`；不勾 = 一个无压缩文件夹（可 diff、可进版本库）")
                        .changed()
                    {
                        self.save_format = core::SaveFormat::from_axes(
                            cur_chart.unwrap_or(opm_app::codec::Format::OpmZip),
                            p,
                        );
                    }
                    opm_app::dialog::hint(ui, self.save_format.describe());
                });
                ui.separator();
                opm_app::dialog::hint(ui, format!("系统文件对话框：{native}"));
                // **目标不可编辑**（用户要求）：保存写回已有目标，没目标时「保存」自动弹文件选择窗，
                // 「另存为…」则总是弹。目标只在标题下面那行**显示**（`dialog::path`），不给第二个输入口径。
                opm_app::dialog::hint(
                    ui,
                    format!(
                        "在系统框里选的目标若没写扩展名会按格式补上（opm 包 → {}；RPE 包 → {}）",
                        opm_app::codec::Format::OpmZip.extension(),
                        opm_app::codec::Format::Rpe.extension(),
                    ),
                );
                if let Some((ok, msg)) = &self.file_message {
                    ui.add_space(4.0);
                    opm_app::dialog::message(ui, *ok, msg);
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("关闭").clicked() {
                        pending = Some(FileAction::Close);
                    }
                    opm_app::dialog::hint(ui, "Esc / 点空白处也可关闭");
                });
            },
            );
            // Esc / 点遮罩 = 关闭（`dialog::modal` 已经消费掉 Esc，这里只管取结果）
            if out.dismissed {
                pending = Some(FileAction::Close);
            }
            // **动作在 UI 之外执行**：系统对话框是阻塞的，不能在画帧的过程中把它弹出来
            match pending {
                Some(FileAction::NewDoc) => {
                    self.file_dialog_open = false;
                    self.request_guarded(GuardAction::NewDoc);
                }
                Some(FileAction::OpenDialog) => {
                    self.file_dialog_open = false;
                    self.request_guarded(GuardAction::OpenDialog);
                }
                Some(FileAction::SaveAsDialog) => {
                    self.file_dialog_open = false;
                    self.save_as_via_system();
                }
                Some(FileAction::SaveToTarget) => {
                    self.ensure_target_then_save();
                }
                Some(FileAction::ApplyName) => {
                    self.apply_chart_name();
                    self.file_message = Some((true, format!("曲名 → {}", self.edit_name)));
                }
                Some(FileAction::Reveal) => self.reveal_current(),
                Some(FileAction::Close) => self.file_dialog_open = false,
                _ => {}
            }
        }

        if self.show_console || self.ws == Workspace::Debug {
            egui::Panel::bottom("console").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("命令控制台");
                    ui.label("（JSON，每行一条；与 opm-ctl 同一套命令）");
                    if ui.button("执行").clicked() {
                        do_exec = true;
                    }
                    if ui.button("校验").clicked() {
                        do_validate = true;
                    }
                    if ui.button("保存").clicked() {
                        do_save = true;
                    }
                    if ui.button("摘要").clicked() {
                        let v = self.core.lock().unwrap().doc().summary();
                        self.console_log.push((true, v.to_string()));
                    }
                });
                ui.add(
                    egui::TextEdit::multiline(&mut self.console_input)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text(r#"{"op":"add_note","line":0,"kind":"hold","startBeat":[1,1],"endBeat":[3,1],"laneX":-120}"#),
                );
                egui::ScrollArea::vertical()
                    .max_height(140.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for (ok, line) in &self.console_log {
                            let color = if *ok {
                                egui::Color32::from_rgb(150, 210, 150)
                            } else {
                                egui::Color32::from_rgb(240, 140, 140)
                            };
                            ui.colored_label(color, egui::RichText::new(line).monospace());
                        }
                    });
            });
        }
        if do_exec {
            let text = std::mem::take(&mut self.console_input);
            match cmd::parse_commands(&text) {
                Ok(cmds) => {
                    // 逐条执行（每条自成撤销步）—— 只把命令交给 EditCore，
                    // 界面不动；下一步更新由广播驱动
                    self.dispatch(&cmds);
                }
                Err(e) => self.console_log.push((false, format!("解析失败: {e}"))),
            }
        }
        if do_validate {
            let doc = self.core.lock().unwrap().doc().clone();
            let issues = cmd::validate(&doc);
            let errors = issues
                .iter()
                .filter(|i| i.severity == cmd::Severity::Error)
                .count();
            self.console_log.push((
                errors == 0,
                format!("校验：{} error(s), {} warning(s)", errors, issues.len() - errors),
            ));
            for i in &issues {
                let tag = match i.severity {
                    cmd::Severity::Error => "ERROR",
                    cmd::Severity::Warn => " WARN",
                };
                self.console_log
                    .push((i.severity == cmd::Severity::Warn, format!("  {tag} {} {}", i.pointer, i.message)));
            }
        }
        if do_save {
            let r = self.core.lock().unwrap().save(None);
            match r {
                Ok(p) => self.console_log.push((true, format!("已保存 {}", p.display()))),
                Err(e) => self.console_log.push((false, format!("保存失败: {e}（用 --doc 指定文件）"))),
            }
        }
        if self.console_log.len() > 300 {
            let drop = self.console_log.len() - 300;
            self.console_log.drain(0..drop);
        }

        // ---- 左侧：判定线 → 事件轨道 → 事件 → 子音符（层级即数据模型的依赖方向） ----
        //
        // 面板**只产出动作**（`TreeAction`），由调用方统一施加：这样面板函数只借用 `&EditorState`，
        // 既绕开了借用冲突，也让"界面不直接改数据"这条规则在类型层面成立 ——
        // 想改选中/想发命令，都得走返回值。
        if self.ws.show_list() {
            let actions = {
                let w = self.ws.list_width();
                let st = &self.state;
                let rows = &self.line_rows;
                let (dl, dn) = (self.doc_lines, self.doc_notes);
                let mut acts: Vec<TreeAction> = Vec::new();
                egui::Panel::left("lines_tree").default_size(w).show(ui, |ui| {
                    line_tree_ui(ui, st, rows, dl, dn, &mut acts);
                });
                acts
            };
            let mut sel_changed = false;
            let mut seek_to = None;
            let mut cmds: Vec<serde_json::Value> = Vec::new();
            for a in actions {
                match a {
                    TreeAction::SelectLine(v) => {
                        self.state.selected_line = v;
                        self.state.clear_selection();
                        sel_changed = true;
                    }
                    TreeAction::SelectTrack(id) => {
                        self.state.selected_track = id;
                        self.state.clear_event_selection();
                        sel_changed = true;
                    }
                    TreeAction::SelectEvent(i) => {
                        let track = self.state.selected_track;
                        self.state.select_event(track, i);
                        sel_changed = true;
                    }
                    TreeAction::SelectNote(i) => {
                        self.state.select_note(i);
                        if let Some(n) = self.state.selected().and_then(|l| l.notes.get(i)) {
                            seek_to = Some(n.time);
                        }
                        sel_changed = true;
                    }
                    TreeAction::Cmd(c) => cmds.push(c),
                }
            }
            if let Some(t) = seek_to {
                self.state.seek(t);
            }
            if sel_changed {
                self.insp = self.build_inspector();
            }
            if !cmds.is_empty() {
                // 走统一写路径：发命令 → 等广播（本帧界面不动）
                self.dispatch(&cmds);
            }
        }

        // ---- 右侧属性检查器（线优先：线 → 轨道 → 事件 → 音符） ----
        let mut pending_edits: Vec<serde_json::Value> = Vec::new();
        if self.ws.show_inspector() {
        egui::Panel::right("inspector").show(ui, |ui| {
            ui.label("属性编辑器");
            ui.separator();
            // 面板**只产出命令**（见 `inspector::inspector_ui` 的注释：施加命令的那几行曾经
            // 写在"调试工作区"分支里 ⇒ 其它工作区改什么都不生效）。拖动类控件用事务包住
            //（松手才 commit），所以"拖一次 = 一个撤销步"。
            pending_edits.extend(inspector::inspector_ui(ui, &self.state, self.insp.as_ref()));
            if self.ws == Workspace::Debug {
            ui.separator();
            ui.label("诊断（调试工作区）");
            ui.monospace(format!(
                "窗口   {}×{}（±{:.0}/±{:.0}）",
                state::RPE_WINDOW_W as i32,
                state::RPE_WINDOW_H as i32,
                state::RPE_WINDOW_HALF_W,
                state::RPE_WINDOW_HALF_H
            ));
            ui.monospace(format!("线长   {:.0}（半长 {:.0}）", self.state.line_half_w * 2.0, self.state.line_half_w));
            ui.monospace(format!("边界框 {}", if self.state.show_boundary { "开" } else { "关" }));
            ui.monospace(format!("缩放   {:.2}×", ctx.pixels_per_point()));
            ui.monospace(format!("限帧   {:?}", self.args.fps_cap));
            ui.monospace(format!("空闲   {} fps", self.args.idle_fps));
            ui.monospace(format!("工作区 {}", self.ws.label()));
            ui.separator();
            ui.label("更新广播");
            ui.monospace(format!("已应用 {} 条", self.applied_broadcasts));
            ui.monospace(format!("最近 {}", self.last_broadcast));
            ui.monospace(format!("话题 {}", self.last_topics.join(", ")));
            ui.monospace(format!("整表重建 {:.3} ms", self.last_structure_ms));
            // 检查器那一格是**必须跟着事件话题走的**（它显示选中事件的值）——
            // 把它印出来，"改完面板没刷"这类 bug 就能在诊断里一眼看出来。
            // 拆成两行是为了**截图里读得全**：面板窄，一长行会被折掉后半截
            // （那几个数字正是"哪一块被重建了"的唯一证据）。
            ui.monospace(format!(
                "重建 整表{} 属性{} 音符{}",
                self.builds_structure, self.builds_props, self.builds_notes
            ));
            ui.monospace(format!(
                "重建 轨道{} 检查{}",
                self.builds_tracks, self.builds_inspector
            ));
            ui.monospace(format!(
                "跳过 整表{} 属性{} 音符{}",
                self.skipped_structure, self.skipped_props, self.skipped_notes
            ));
            ui.monospace(format!(
                "跳过 轨道{} 检查{}",
                self.skipped_tracks, self.skipped_inspector
            ));
            // 音符位置缓存：待重算条数（0 = 全部算准）。这行是"异步补完了没有"的读数 ——
            // 底栏那行字只在真的在补时出现，**补完就没了**，所以核对时要看这里。
            match self.state.floor_rebuild() {
                Some((done, total)) => {
                    ui.monospace(format!("位置缓存 重算中 {done}/{total}"));
                }
                None => {
                    ui.monospace(format!("位置缓存 已算准（{} 条）", self.state.floor_cached()));
                }
            }
            ui.separator();
            ui.label("播放与音频");
            ui.monospace(format!(
                "状态   {}",
                if self.state.playing { "播放中" } else { "暂停" }
            ));
            ui.monospace(format!("播放头 {:.3} s（{:.2} 拍）", self.state.playhead, self.state.chart.tmap.beat(self.state.playhead)));
            if let Some(a) = &self.audio {
                ui.monospace(format!("音频   {}", a.device()));
                ui.monospace(format!("游标   {:.1} ms", a.position_sec() * 1000.0));
                ui.monospace(format!("延迟   {:.2} ms（cpal 自校准）", a.latency_ms()));
                ui.monospace(format!("校准   {:+.2} ms", a.offset_ms()));
                ui.monospace(format!("欠载   {}", a.underruns()));
            } else {
                ui.monospace("音频   （无）");
            }
            if self.audio_window_s > 0.0 {
                ui.monospace(format!(
                    "对墙钟   {:+.2} ms / {:.1}s{}",
                    self.audio_dev_ms,
                    self.audio_window_s,
                    if self.audio_rate_ppm != 0.0 {
                        format!("（{:.0} ppm）", self.audio_rate_ppm)
                    } else {
                        "（窗口 <3s，不给 ppm）".into()
                    }
                ));
            }
            } // Debug 工作区结束
        });
        }
        // 属性编辑器的命令**在这里无条件施加**（与其它面板同一条写路径：发命令 → 等广播）。
        // 原先这一段写在 `if self.ws == Workspace::Debug { … }` 里面 —— 于是默认（制谱）工作区里
        // 属性编辑器改了什么都不生效：命令算出来又被丢掉。这条分支现在没有了。
        if !pending_edits.is_empty() {
            self.dispatch(&pending_edits);
            self.insp = self.build_inspector();
        }

        // ---- 中央：演奏区 + 时间轴 ----
        egui::CentralPanel::default().show(ui, |ui| {
            let full = ui.available_rect_before_wrap();
            // 上界依赖窗口高度 ⇒ 必须先夹进合法区间，否则矮窗口下 `clamp(min>max)` 直接 panic
            let timeline_h =
                state::timeline_height(full.height(), self.ws.timeline_frac(), self.ws.show_timeline());
            let play_rect = egui::Rect::from_min_max(
                full.min,
                egui::pos2(full.max.x, full.max.y - timeline_h - 6.0),
            );
            let tl_rect = egui::Rect::from_min_max(
                egui::pos2(full.min.x, full.max.y - timeline_h),
                full.max,
            );

            // 演奏区底 + 边框（egui 侧）
            ui.painter().rect_filled(play_rect, 2.0, egui::Color32::from_rgb(10, 10, 16));
            ui.painter().rect_stroke(
                play_rect,
                2.0,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(70, 70, 110)),
                egui::StrokeKind::Inside,
            );

            // 自研实例化渲染：viewport 映射到 play_rect
            let ppp = ctx.pixels_per_point();
            let viewport_px = [play_rect.width() * ppp, play_rect.height() * ppp];

            // ---- 窗口边界（RPE：锚点在屏幕中心，X ±675 / Y ±450 ⇒ 1350×900）----
            // 边界线、角标、边界外压暗三者都由渲染层按同一坐标系画（GUI 与无头出图共用一份几何，
            // 避免两套实现漂移）；这里只算它的屏幕矩形，用于文字标注与判定线端点标记。
            let scale_pts = render::rpe_scale(viewport_px) / ppp;
            let rpe_center = play_rect.center();
            let rpe_of = |x: f32, y: f32| rpe_center + egui::vec2(x * scale_pts, -y * scale_pts);
            let win_rect = egui::Rect::from_min_max(
                rpe_of(-state::RPE_WINDOW_HALF_W, state::RPE_WINDOW_HALF_H),
                rpe_of(state::RPE_WINDOW_HALF_W, -state::RPE_WINDOW_HALF_H),
            );
            ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                play_rect,
                PlayfieldFrame {
                    instances: std::mem::take(&mut self.instances),
                    viewport_px,
                    paint_ms: self.paint_us.clone(),
                },
            ));

            // 边界与判定线的标注（文字与端点标记走 egui，几何线走 wgpu，两者用同一个映射公式）
            // 编辑区开着时不再画窗口文字：两套 chrome 叠在同一角会互相糊掉（边框本身还在）
            if self.state.show_boundary && !self.overlay_visible {
                let p = ui.painter();
                p.text(
                    win_rect.left_top() + egui::vec2(6.0, 3.0),
                    egui::Align2::LEFT_TOP,
                    format!(
                        "RPE 窗口 {}×{}（X ±{:.0} / Y ±{:.0}）  预览缩放 {:.3}×",
                        state::RPE_WINDOW_W as i32,
                        state::RPE_WINDOW_H as i32,
                        state::RPE_WINDOW_HALF_W,
                        state::RPE_WINDOW_HALF_H,
                        scale_pts
                    ),
                    egui::FontId::monospace(10.0),
                    egui::Color32::from_rgb(150, 165, 205),
                );
            }
            // 判定线端点：线长是编辑器设置，端点位置 = 线变换后的 (±line_half, 0)
            //
            // 默认线长是 **3000**（比窗口的 1350 宽得多）⇒ 两个端点本来会落在画面之外、
            // 连"线 #N 长 L"这行字一起看不见。所以：端点圆圈只画**看得见的**那些，
            // 而长度读数贴在**看得见的那一端**上，并写明"伸出窗口"。
            if let Some(line) = self.state.selected() {
                let perf = line.perf(&self.state.chart.tmap, self.state.playhead);
                let p = ui.painter();
                let col = egui::Color32::from_rgb(250, 220, 120);
                let outside = self.state.line_half_w > state::RPE_WINDOW_HALF_W + 0.5;
                for sx in [-1.0_f32, 1.0] {
                    let q = perf.apply([sx * self.state.line_half_w, 0.0]);
                    let pos = rpe_of(q[0], q[1]);
                    if !play_rect.expand(8.0).contains(pos) {
                        continue; // 端点在线长超过窗口时本来就在画外
                    }
                    p.circle_stroke(pos, 5.0, egui::Stroke::new(1.2, col));
                    p.line_segment(
                        [pos - egui::vec2(9.0, 0.0), pos + egui::vec2(9.0, 0.0)],
                        egui::Stroke::new(1.0, col),
                    );
                }
                // 读数贴在**窗口内**的那一端（长线时就是窗口边缘），且右对齐 ——
                // 否则"线 #0 长 3000（伸出窗口）"这行字会从右边缘伸出去、同样被裁掉。
                let at = self
                    .state
                    .line_half_w
                    .min(state::RPE_WINDOW_HALF_W * 0.98);
                let q = perf.apply([at, 0.0]);
                p.text(
                    rpe_of(q[0], q[1]) + egui::vec2(-10.0, 0.0),
                    egui::Align2::RIGHT_CENTER,
                    format!(
                        "线 #{} 长 {:.0}{}",
                        line.index,
                        self.state.line_half_w * 2.0,
                        if outside { "（伸出窗口）" } else { "" }
                    ),
                    egui::FontId::monospace(10.0),
                    col,
                );
            }

            // ---- 编辑区叠加层（拍为纵轴；自动播放中或按住 H 时隐藏）----
            if self.overlay_visible {
                let mut acts: Vec<OverlayAction> = Vec::new();
                // 键门控只有一处：打字/模态期间不给面板用快捷键（与空格/H 同一口径）
                overlay::draw(
                    ui,
                    &self.state,
                    play_rect,
                    &self.overlay,
                    !typing && !modal_open,
                    &mut acts,
                );
                self.apply_overlay_actions(acts);
            }

            // 对齐自检：用与着色器相同的映射公式，把同一批 RPE 坐标画成十字。
            // 若自研管线的方块与这些十字重合，说明 viewport 映射在任意缩放/布局下都正确。
            if self.args.verify_align {
                let scale_pts = render::rpe_scale(viewport_px) / ppp;
                let c = play_rect.center();
                let p = ui.painter();
                for (x, y, _) in render::MARKERS {
                    let pos = c + egui::vec2(x * scale_pts, -y * scale_pts);
                    let col = egui::Color32::from_rgb(0, 220, 255);
                    p.line_segment([pos - egui::vec2(14.0, 0.0), pos + egui::vec2(14.0, 0.0)], egui::Stroke::new(1.0, col));
                    p.line_segment([pos - egui::vec2(0.0, 14.0), pos + egui::vec2(0.0, 14.0)], egui::Stroke::new(1.0, col));
                }
            }

            // 判定线位置提示（egui 侧文字，验证与自研渲染的对齐）
            ui.painter().text(
                play_rect.center_top() + egui::vec2(0.0, 14.0),
                egui::Align2::CENTER_TOP,
                "判定线 y=0（自研渲染）",
                egui::FontId::monospace(11.0),
                egui::Color32::from_rgb(140, 150, 190),
            );

            // ---- 时间轴 ----
            //
            // 实现整体在库里（`opm_app::timeline`）：几何与读数都是**纯逻辑**（可单测），
            // 这里只剩"把这一帧的动作执行掉"。以前这 250 行长在这个闭包里，测不了，
            // 只能靠截图看 —— 而截图看错一次就会改错代码（真发生过，见 §7.57）。
            let out = opm_app::timeline::draw(ui, &self.state, tl_rect, self.overlay.lead_beats);
            if let Some(t) = out.seek {
                self.state.seek(t);
                // 同一件事的另一条入口（`seek_to` 之外）：检查器里那段"**此刻**表演"是快照，
                // 拖时间轴时不刷它就会停在旧的一刻上 —— 而那一段的标题正是"此刻"
                self.insp = self.build_inspector();
            }
        });

        // ---- 缺少 7z（**最后画**：画序决定谁在上面，它必须在所有面板之上）----
        //
        // 与启动页上的那一份是**同一个实现**（[`App::missing_7z_gate`]）：
        // 同一件事在两处画成两种样子，就是这个文件里最容易长出来的不一致。
        // 它只在"带着 `--doc` 直接进编辑页、而系统里没有 7z"这条路上出现（无 `--doc` 时
        // 启动页就会把它拦住），所以这里不重复解释门槛的理由。
        if self.seven_zip_missing.is_some() {
            let ctx = ui.ctx().clone();
            if self.missing_7z_gate(&ctx) {
                self.quit_now(&ctx);
            }
        }

        // ---- 帧计时与限帧 ----
        self.publish_stats();
        self.ui_ms.push(t_ui.elapsed().as_secs_f64() * 1000.0);
        self.build_ms.push(build_ms);
        self.inst_counts.push(inst_count);
        let now = Instant::now();
        if let Some(prev) = self.last_frame {
            self.deltas.push((now - prev).as_secs_f64() * 1000.0);
        }
        self.last_frame = Some(now);
        self.frames += 1;

        if self.args.bench > 0 && self.frames >= self.args.bench {
            // 进入空闲阶段：停止主动重绘，只保留 idle_fps 心跳，测量真实空闲帧率
            if self.args.idle_seconds > 0.0 {
                if self.idle_start.is_none() {
                    self.idle_start = Some(Instant::now());
                    self.state.playing = false;
                    println!("\n>> 转入空闲阶段 {:.1}s（不主动重绘，仅 {} fps 心跳）", self.args.idle_seconds, self.args.idle_fps);
                    let _ = std::io::stdout().flush();
                } else {
                    self.idle_frames += 1;
                let _ = std::io::stdout().flush();
                }
                if self.idle_frames <= 6 {
                    let causes = ctx.repaint_causes();
                    if causes.is_empty() {
                        println!("[idle] 帧 {} 无重绘请求（事件/定时驱动）", self.idle_frames);
                    } else {
                        for c in &causes {
                            println!("[idle] 帧 {} 重绘原因: {c}", self.idle_frames);
                        }
                    }
                }
                if self.idle_start.unwrap().elapsed().as_secs_f64() >= self.args.idle_seconds {
                    if !self.reported {
                        self.report();
                    }
                    self.quit_now(&ctx);
                }
                // 空闲阶段：不连续重绘。
                // idle_fps > 0 → 心跳；idle_fps == 0 → 只在空闲时段结束时唤醒一帧（否则事件驱动下永远轮不到这里收尾）
                let elapsed = self.idle_start.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
                let remain = (self.args.idle_seconds - elapsed).max(0.02);
                let period = if self.args.idle_fps > 0.0 {
                    (1.0 / self.args.idle_fps).min(remain)
                } else {
                    remain
                };
                ctx.request_repaint_after(Duration::from_secs_f64(period));
                self.frames += 1;
                self.last_frame = Some(Instant::now());
                self.idle_frames_total += 1;
                if let Ok(mut st) = self.stats.lock() {
                    st.idle_frames = self.idle_frames_total;
                }
                return;
            }
            if !self.reported {
                self.report();
            }
            self.quit_now(&ctx);
            return;
        }

        // ---- 节奏控制：画面无更新时降到 1 帧，工作时按屏幕帧率 ----
        // egui 本身是事件驱动的：不主动 request_repaint 就不会出帧（输入仍会触发重绘）。
        // 「工作」= 播放中 / 正在拖拽或滚轮交互 / 有动画；此时连续请求重绘，由 vsync 封顶到屏幕刷新率。
        // 注意：`egui_wants_pointer_input()` 的语义是「egui 想接收指针事件」（鼠标悬停即为真），
        // **不能**当作「用户正在操作」的活动信号 —— 那样会让空闲态在鼠标停靠时被判为工作态。
        // 只认「正在按住/拖拽」。
        let interacting = ctx.egui_is_using_pointer();
        // 「等广播」也是工作态：命令已交给 EditCore，界面要保持出帧才能及时应用更新
        // 「还在补音符位置」也是工作态：不主动出帧的话它会在别人看的时候停在那儿不动
        let awaiting =
            self.pending_dispatch.is_some() || self.dirty.any() || self.state.floor_pending() > 0;
        // bench 的活跃阶段本身就是「持续出帧」的测量场景，需强制视为工作态，
        // 否则事件驱动下帧数永远到不了目标值（实测踩过：进程直接被 timeout 杀掉）。
        let bench_active = self.args.bench > 0 && self.frames < self.args.bench;
        let working =
            self.state.playing || interacting || awaiting || self.pending_layout_anim || bench_active;

        if working {
            self.working_frames_total += 1;
            ctx.request_repaint();
        } else {
            self.idle_frames_total += 1;
            if self.args.idle_fps > 0.0 {
                // 空闲心跳：仅用于让状态栏/诊断保持可见（默认 1 fps），不是连续渲染
                ctx.request_repaint_after(Duration::from_secs_f64(1.0 / self.args.idle_fps));
            } // idle_fps == 0 ⇒ 纯事件驱动，不主动重绘
        }

        if let Ok(mut st) = self.stats.lock() {
            st.working_frames = self.working_frames_total;
            st.idle_frames = self.idle_frames_total;
        }
        if self.frames == 2 {
            // 首帧之后用于判断"布局是否还在动画"
            self.pending_layout_anim = false;
        }
    }
}

/// 解析该用哪个音频文件：`--audio FILE` 优先，其次谱面 `meta.audio`（相对谱面目录），
/// `--audio off` 表示明确不要音频。
/// 该放哪段音频：**路径决策在库里**（`audio::resolve_source`，纯函数 + 单测），
/// 这里只负责"真的去读文件并开设备"。
/// 当前文档的**容器资源目录**（摊到磁盘的那份）；由 `EditCore::asset_dir()` 给。
fn asset_dir_of(core: &core::SharedCore) -> Option<std::path::PathBuf> {
    core.lock().ok().and_then(|c| c.asset_dir().map(std::path::Path::to_path_buf))
}

fn resolve_audio(args: &Args, core: &core::SharedCore) -> Result<Option<audio::Audio>, String> {
    let (meta_audio, chart_path, asset_dir) = {
        let c = core.lock().unwrap();
        (
            c.doc().meta.audio.clone(),
            c.path().map(std::path::Path::to_path_buf),
            c.asset_dir().map(std::path::Path::to_path_buf),
        )
    };
    let Some((path, src)) = audio::resolve_source(
        args.audio.as_deref(),
        meta_audio.as_deref(),
        chart_path.as_deref(),
        asset_dir.as_deref(),
    )?
    else {
        return Ok(None);
    };
    audio::Audio::load(&path).map(Some).map_err(|e| format!("{e}（来源：{}）", src.label()))
}

// 空格键的规则已整体搬进 `keymap::SpacePlayback`（单点 = 进入/退出自动播放，长按 = 松手退出）。
// 这里不再留"按下就 toggle"的旧实现 —— 同一件事有两份实现时，早晚有人用错那一份。



