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

/// 启动页（谱面列表）窗口的标题与尺寸。
///
/// **一屏一个尺寸**：启动页上的弹窗（新建谱面 / 缺少 7z）是**模态**，不再换整屏、也不再改窗口尺寸
/// —— 换个尺寸会重建 wgpu surface，视觉上就是"切了个程序"，而弹窗只是"这一屏上的一个问题"。
const LAUNCH_TITLE: &str = "OpenPhM — 选择谱面";
const LAUNCH_SIZE: egui::Vec2 = egui::Vec2::new(980.0, 620.0);

/// 启动页列表快照的最长寿命（秒）。
///
/// 列表里唯一会随时间变的只有"[格式] 多久以前"这一行文字，所以按秒表重算即可 ——
/// 一天按帧重算，只为了把"刚刚"改成"1 分钟前"是不划算的。
const LIST_ROWS_MAX_AGE: u64 = 30;

/// 统计量最短发布间隔：这些是**诊断量**，10 Hz 足够人（和 agent）看，
/// 没必要每帧排一次序、抢几次锁（见 `App::publish_stats`）。
const STATS_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// 启动阶段：先在**自己的窗口**里解决"选哪份谱面"，选完才进编辑页。
///
/// 关于"为什么不真的是两个并存窗口"：eframe 里只有根视口跑 pass（子视口都在根的 pass 里画），
/// 关掉根 = 退出、隐藏根 = 完全不跑 pass。所以"启动窗口关掉、编辑窗口留下"这套在 eframe 里做不到；
/// 启动页与编辑页因此是**同一个窗口的两个页面**，各自有自己的标题与尺寸。
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
    /// 采用手输的目标路径
    UseTypedTarget,
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
    // 编辑文档：--doc 载入真实 opm 文件，否则按 --notes 生成演示谱面
    let doc_arg = args.doc.clone();
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
    if doc_arg.is_none() && args.notes > 0 {
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
            let id = state::TrackId::ALL.iter().find(|t| t.key() == track).copied();
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
    let mut ctrl_path: Option<std::path::PathBuf> = None;
    if let Some(spec) = &args.control {
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
    // 窗口的**初始标题与尺寸按启动阶段来**：启动页是小窗（"选择谱面"），进编辑页后再换成编辑尺寸。
    // 启动页上的弹窗不改这两个值（模态不换屏，见 `LAUNCH_SIZE` 的注释）。
    let on_launcher = doc_arg.is_none() && !args.bench_only() && !args.stress;
    let launch_phase = if on_launcher {
        LaunchPhase::StartScreen
    } else {
        LaunchPhase::Editor
    };
    let (title, size) = match launch_phase {
        // 缺 7z 时标题直接说明门槛是什么（尺寸仍是启动页那一套：底下那屏照画，只是盖了模态）
        LaunchPhase::StartScreen => (
            if seven_zip_missing.is_some() {
                "OpenPhM — 缺少 7-Zip"
            } else {
                LAUNCH_TITLE
            },
            LAUNCH_SIZE,
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
    // ---- 后端集合：**能确定 Vulkan 可用就只开 Vulkan**（省掉 GL 的初始化，实测 ~150 ms）----
    //
    // 探测本身**不触发初始化**（只看 ICD json 与 loader 库在不在），探测不到就保留全后端 ——
    // GL 回退在本会话里真的救过场（Vulkan 里没有可用卡那次）。开关：`OPM_BACKEND=vulkan|all`；
    // 用户设了 `WGPU_BACKEND` 时一切照旧（不抢 wgpu 自己的开关）。
    {
        let env = |k: &str| std::env::var(k).ok();
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
        let icd = opm_app::gpu::vulkan_icd_usable(&explicit, default_json);
        let loader = opm_app::gpu::loader_candidates().iter().any(|p| std::path::Path::new(p).is_file());
        let (plan, why) = opm_app::gpu::backend_plan(cfg!(target_os = "linux"), &env, icd, loader);
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
    let (gpu_policy, gpu_why) =
        opm_app::gpu::policy_from_env(cfg!(target_os = "linux"), &|k| std::env::var(k).ok());
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
    let r = eframe::run_native(
        "opm-app",
        options,
        Box::new(move |_cc| {
            let t_app = std::time::Instant::now();
            let app = App::new(
                state, shared, stats, sub, ctx_slot, audio, view, meta_name, doc_lines, doc_notes, a,
                recents, seven_zip_missing, launch_phase,
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
    target_draft: String,
    /// 曲名（文档字段 `meta.name`）；新建下一份时的默认文件名也用它
    edit_name: String,
    /// **新建谱面表单**（启动页上的模态；值由这里持有，库只改它）
    new_form: opm_app::recents::NewChartForm,
    /// 启动页上的「新建谱面」模态是否打开（模态不换屏：底下的列表照画，只是被压暗且吞掉输入）
    new_form_open: bool,
    /// 待办的"回启动页并打开新建模态"（换窗口标题/尺寸要用 `Context`，只能在帧里做）。
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
    /// 是否正处在一次拖拽事务中（结束时 commit）
    drag_active: bool,
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
    ) -> Self {
        let args_ws = args.ws.unwrap_or(Workspace::Compose);
        // 顶栏拖动框的初值来自状态（`--window-offset` 已在这一步之前作用于 state）
        let state_window_offset = state.window_offset_x;
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
            target_draft: String::new(),
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
            drag_active: false,
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
                    self.state.selected_note = Some(i);
                    sel_changed = true;
                }
                OverlayAction::SelectTrack(id) => {
                    if self.state.selected_track != id {
                        self.state.selected_track = id;
                        self.state.selected_event = None;
                        sel_changed = true;
                    }
                }
                OverlayAction::SelectEvent(i) => {
                    self.state.selected_event = Some(i);
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
                OverlayAction::NoteDragStart => {
                    // 整段拖拽 = **一个撤销步**：用事务包住，逐条改动仍然广播（面板实时更新）
                    self.drag_active = true;
                    cmds.push(opm_app::edit::begin_command(opm_app::edit::DRAG_NOTE_LABEL));
                }
                OverlayAction::NoteDrag { index, doc_index, lane_x, beat } => {
                    // 命令怎么拼在库里（`opm_app::edit`，有单测）：hold 要保持时长这类规则
                    // 不该只活在"拖一下看看"里
                    cmds.push(opm_app::edit::note_drag_command(
                        &self.state, index, doc_index, lane_x, beat,
                    ));
                }
                OverlayAction::NoteDragEnd => {
                    if self.drag_active {
                        self.drag_active = false;
                        cmds.push(opm_app::edit::commit_command());
                    }
                }
                OverlayAction::EventResizeStart => {
                    self.drag_active = true;
                    cmds.push(opm_app::edit::begin_command(opm_app::edit::DRAG_EVENT_LABEL));
                }
                OverlayAction::EventResize { index, edge, beat } => {
                    cmds.push(opm_app::edit::event_resize_command(
                        &self.state, index, edge, beat,
                    ));
                }
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
    /// 保存目标是**一个**路径 —— 文件夹与谱面名字合起来构成它，而不是两个各自生效的设置。
    /// 没有目标时留空：界面显示"（未指定）"，保存时就会弹保存窗口（Krita 的语义）。
    ///
    /// 顺带刷新状态栏那份文档标识：**这条路径上的每个入口（打开/另存为/新建/切格式）
    /// 都经过这里**，所以"什么时候该重算"只有一个答案。
    fn sync_file_fields(&mut self) {
        let (path, meta_name) = {
            let c = self.core.lock().unwrap();
            (c.path().map(std::path::Path::to_path_buf), c.doc().meta.name.clone())
        };
        self.target_draft = path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        self.edit_name = meta_name;
        self.refresh_file_badge();
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

    /// 这次保存（按当前草稿目标与所选格式）会写出什么扩展名。
    ///
    /// **不自己写一份匹配**：判据与真正写盘时用的 [`core::SaveFormat::resolve`] 完全相同
    /// （`Auto` 先看目标扩展名，判不出来才沿用来源格式），扩展名再取自
    /// [`opm_app::codec::Format::extension`]。原先这里另有一份 `match`，而对话框的提示文字
    /// 又是第三份说法 —— 结果提示说"opm → `.opm.json`"、实际写出 `.opm`。
    fn save_ext(&self) -> &'static str {
        let loaded = self
            .core
            .lock()
            .map(|c| c.source_format())
            .unwrap_or(opm_app::codec::Format::Opm);
        let draft = self
            .target_path()
            .unwrap_or_else(|| std::path::PathBuf::from("未命名"));
        self.save_format.resolve(&draft, loaded).extension()
    }

    /// 草稿目标（界面上那行"保存目标"）→ 路径。空串表示"还没指定"。
    fn target_path(&self) -> Option<std::path::PathBuf> {
        let t = self.target_draft.trim();
        if t.is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(t))
        }
    }

    /// 保存前检查目标：没有就**先弹保存窗口**（这是用户报的"保存新文件无法指定路径"的正解）
    fn ensure_target_then_save(&mut self) {
        if self.save_target().is_none() && self.target_path().is_none() {
            self.file_message = Some((
                false,
                "还没有保存目标 —— 请先指定（另存为… 或直接在下面输入目标路径）".to_owned(),
            ));
            self.file_dialog_open = true;
            self.save_as_via_system();
            return;
        }
        if self.save_target().is_none() {
            // 界面草稿里有目标：先采用它再保存
            if let Some(p) = self.target_path() {
                let p = filedialog::ensure_extension(&p, self.save_ext());
                self.target_draft = p.display().to_string();
                self.save_doc_as(&p.display().to_string());
                return;
            }
        }
        self.save_doc();
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
        }
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
            // 音乐路径是**文档字段**（`meta.audio`，已由 `new` 命令写进文档），
            // 这里顺带把它换成本次预览的音频（失败只提示，不影响谱面）
            if !self.new_form.audio.trim().is_empty() {
                let p = self.new_form.audio.trim().to_owned();
                match audio::Audio::load(std::path::Path::new(&p)) {
                    Ok(a) => {
                        self.audio = Some(a);
                        self.state.playhead = 0.0;
                    }
                    Err(e) => self
                        .console_log
                        .push((false, format!("音频未能载入（谱面字段已写入）：{e}"))),
                }
            }
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
        match filedialog::pick(filedialog::Which::Open, start.as_deref()) {
            Ok(Some(p)) => self.open_doc(&p.display().to_string()),
            Ok(None) => self.file_message = Some((true, "已取消".to_owned())),
            Err(e) => {
                self.file_message = Some((false, format!("{e}（可在下面直接输入路径）")));
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
        let start = Some(filedialog::start_for_new_save(
            dir.as_deref(),
            &self.file_stem(),
            self.save_ext(),
        ));
        match filedialog::pick(filedialog::Which::Save, start.as_deref()) {
            Ok(Some(p)) => {
                self.save_doc_as(&p.display().to_string());
                self.sync_file_fields();
            }
            Ok(None) => self.file_message = Some((true, "已取消".to_owned())),
            Err(e) => {
                self.file_message = Some((false, format!("{e}（可在下面直接输入路径）")));
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
            self.sync_file_fields(); // 打开之后：文件夹/名字/格式提示都跟着新文件走
            self.remember_recent();
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
        let fmt = match self.save_format {
            core::SaveFormat::Auto => "auto",
            core::SaveFormat::Opm => "opm",           // 容器（正式形态）
            core::SaveFormat::OpmBare => "opm-bare",  // 裸工程文件
            core::SaveFormat::Rpe => "rpe",
        };
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
                control::ViewCmd::Select { line, track, note, event } => {
                    // 选中是视图状态：直接改 EditorState，不碰文档
                    if let Some(li) = line {
                        if let Some(view) = self.state.chart.lines.iter().position(|l| l.index == li) {
                            self.state.selected_line = view;
                        }
                    }
                    if let Some(t) = track {
                        if let Some(id) = state::TrackId::ALL.iter().find(|id| id.key() == t) {
                            self.state.selected_track = *id;
                        }
                    }
                    self.state.selected_note = note;
                    self.state.selected_event = event;
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
        }

        if d.structure {
            // 线集合/时间映射变了：整表重建（这是唯一"全量"的一档）
            let t = Instant::now();
            let c = self.core.lock().unwrap();
            let doc = c.doc().clone();
            drop(c);
            self.state.chart = state::chart_from_doc(&doc);
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
                        slot.notes = notes;
                        self.builds_notes += 1;
                    }
                }
                for i in &d.tracks {
                    let tracks = state::tracks_of(c.doc(), *i, &tmap);
                    if let Some(slot) = self.state.chart.lines.iter_mut().find(|x| x.index == *i) {
                        slot.tracks = tracks;
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

    /// 选中项越界时收敛（线被删掉、音符被删掉）
    fn clamp_selection(&mut self) {
        let n = self.state.chart.lines.len();
        if n == 0 {
            self.state.selected_line = 0;
        } else if self.state.selected_line >= n {
            self.state.selected_line = n - 1;
        }
        let line_notes = self
            .state
            .selected()
            .map(|l| l.notes.len())
            .unwrap_or(0);
        if let Some(i) = self.state.selected_note {
            if i >= line_notes {
                self.state.selected_note = None;
            }
        }
        let track_events = self
            .state
            .selected()
            .map(|l| l.track(self.state.selected_track).events.len())
            .unwrap_or(0);
        if let Some(i) = self.state.selected_event {
            if i >= track_events {
                self.state.selected_event = None;
            }
        }
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
                match write_png(&img, std::path::Path::new(&path)) {
                    Ok(()) => println!("  已自截屏          : {path}（{}×{}）", img.width(), img.height()),
                    Err(e) => eprintln!("  自截屏失败: {e}"),
                }
                let _ = std::io::stdout().flush();
                if self.args.shot_exit {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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

    /// 处理"回启动页并摆出「新建谱面」模态"的待办（换标题与尺寸要 `Context`，只能在帧里做）。
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
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(LAUNCH_TITLE.to_owned()));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(LAUNCH_SIZE));
    }

    /// 进编辑页：**同一个窗口**换标题与尺寸（见文件头关于 eframe 视口约束的说明）
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
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
            self.args.width,
            self.args.height,
        )));
        println!("  进入编辑页            : 是");
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
    fn launch_page(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.pump_launch_new(ctx);
        let screen = ui.max_rect();
        let native = filedialog::availability();
        let msg = self.file_message.clone();
        // 行快照只在"列表变了"或"时刻走远了（30 秒）"时重算 —— 不是每帧
        self.refresh_list_rows_if_stale();
        // 缺 7z = 一道**关不掉的门槛**（`.opm` 容器靠它打包/解包，没它交付不出正式格式）
        let gate = self.seven_zip_missing.clone();
        let gated = gate.is_some();
        let mut action = opm_app::recents::start_screen_ui(
            ui,
            screen,
            &self.list_rows,
            msg.as_ref(),
            native,
            gated || self.new_form_open,
        );
        // 「新建谱面」：盖在列表上的模态（`opm_new_chart`）
        if self.new_form_open {
            if let Some(a) =
                opm_app::recents::new_chart_modal(ctx, &mut self.new_form, msg.as_ref())
            {
                action = Some(a);
            }
        }
        if let Some(text) = &gate {
            // 门槛期间列表的动作一律作废（遮罩底下本来就点不到，这里是第二道保险）
            action = None;
            let out = opm_app::recents::missing_7z_modal(ctx, text, cfg!(windows));
            if out.fetch {
                let next = match filedialog::open_url(zip::SEVEN_ZIP_URL) {
                    Ok(()) => format!("{text}\n（已用浏览器打开下载页；装好之后重启 OpenPhM）"),
                    Err(e) => format!(
                        "{text}\n（打开浏览器失败：{e}；地址：{}）",
                        zip::SEVEN_ZIP_URL
                    ),
                };
                self.seven_zip_missing = Some(next);
            }
            if out.quit || ctx.input(|i| i.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        // 自动化钩子（截图/CI 用）：`OPM_LAUNCH_AUTO=skip|new|create:<曲名>|open:<path>|recent:<n>`
        // —— 没人点鼠标时也能把"选完切编辑页"这一步走完。**只在这里生效**，不影响交互路径。
        // 门槛期间不生效：否则 `OPM_LAUNCH_AUTO=skip` 就成了绕过 7z 检查的后门。
        if action.is_none() && !gated && self.frames >= 2 {
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
            Some(opm_app::recents::StartAction::PickAudio) => {
                let start = filedialog::start_for_open(None);
                match filedialog::pick(filedialog::Which::Open, start.as_deref()) {
                    Ok(Some(p)) => {
                        self.new_form.audio = p.display().to_string();
                        self.file_message = Some((true, format!("音乐 → {}", p.display())));
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
        self.handle_shot(ctx);
        self.publish_stats();
        self.frames += 1;
        self.pace(ctx);
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
            match fonts::install(&ctx) {
                Some(f) => {
                    println!("  CJK 字体          : {}（字面索引 {}）", f.desc, f.index);
                    fonts::install_kr_fallback(&ctx);
                }
                None => eprintln!("  ⚠️ 未找到 CJK 字体，中文将显示为豆腐块"),
            }
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

        // **进编辑页先把整窗铺一层不透明底色**：启动页→编辑页会同时换内容与窗口尺寸，
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
        // 按住 H：临时藏掉编辑区（放开即恢复）。同样不能在控制台打字时误触发。
        self.h_held = !typing && !modal_open && ctx.input(|i| i.key_down(egui::Key::H));
        // 可见性规则抽成纯函数（有单测）：**自动播放中或按住 H 时隐藏**
        self.overlay_visible = overlay::overlay_visible(self.state.overlay_enabled, self.state.playing, self.h_held);

        // ---- 控制通道的视图命令（play/pause/seek/audio）----
        self.pump_view_cmds();

        // ---- 唯一的更新入口：抽广播 → 置脏 → 只重建脏掉的那块 ----
        // GUI 不轮询 `revision`、不直接读文档判断"要不要更新"：文档什么时候变了，由 EditCore 说。
        self.pump_broadcasts();

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
                file_hover: statusbar::file_hover(self.file_has_target, &self.target_draft),
                window_offset,
                overlay_hidden: (!self.overlay_visible)
                    .then(|| statusbar::overlay_hidden_text(self.h_held)),
                conflicts: self.conflicts.len(),
                show_conflicts: self.show_conflicts,
                // 编辑器里常驻的只有状态栏 ⇒ 消息也显示在这里（文件对话框里那份照旧）
                notice: self.file_message.clone(),
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
                if let Some(view) = self.state.chart.lines.iter().position(|l| l.index == j.line_doc)
                {
                    self.state.selected_line = view;
                }
                if let Some(id) = state::TrackId::ALL.iter().find(|id| id.key() == j.track) {
                    self.state.selected_track = *id;
                }
                self.state.selected_note = None;
                self.state.selected_event = Some(j.event);
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
        // ---- 未保存守卫：有未保存改动时先问「保存｜不保存｜返回」----
        //
        // 这是 Krita 的三个选项，不是常见的两个：少了「返回」就没法反悔 ——
        // 用户点开"新建"只是想看看，结果被迫在"保存"和"丢弃"之间选一个。
        if let Some(goal) = self.guard_for {
            let ctx = ui.ctx().clone();
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
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("另存为格式");
                    ui.selectable_value(&mut self.save_format, core::SaveFormat::Auto, "自动");
                    ui.selectable_value(&mut self.save_format, core::SaveFormat::Opm, "opm 包");
                    ui.selectable_value(&mut self.save_format, core::SaveFormat::OpmBare, "裸 opm");
                    ui.selectable_value(&mut self.save_format, core::SaveFormat::Rpe, "RPE");
                    opm_app::dialog::hint(
                        ui,
                        "opm 包 = `.opm`（ZIP：谱面 + 音乐 + 曲绘）；裸 opm = `.opm.json`（可 diff）；RPE = `.json`",
                    );
                });
                ui.separator();
                opm_app::dialog::hint(ui, format!("系统文件对话框：{native}"));
                // 直接指定保存目标（没有系统对话框、或想精确写路径时用）：
                // 文件夹与文件名在这里是**一串**，不再拆成两个各自生效的设置
                ui.horizontal(|ui| {
                    ui.label("目标");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.target_draft)
                            .desired_width(400.0)
                            .hint_text("/path/to/charts/曲名.opm.json"),
                    )
                    .on_hover_text("保存目标：文件夹 + 谱面名字 + 扩展名，一串就行");
                    if ui.button("采用此目标").clicked() {
                        pending = Some(FileAction::UseTypedTarget);
                    }
                });
                ui.add_space(2.0);
                opm_app::dialog::hint(
                    ui,
                    format!(
                        "提示：目标没有扩展名时会按格式补上 —— opm 包 → {}；裸 opm → {}；RPE → {}",
                        opm_app::codec::Format::OpmZip.extension(),
                        opm_app::codec::Format::Opm.extension(),
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
                Some(FileAction::UseTypedTarget) => {
                    if let Some(p) = self.target_path() {
                        let p = filedialog::ensure_extension(&p, self.save_ext());
                        self.target_draft = p.display().to_string();
                        self.file_message =
                            Some((true, format!("保存目标 → {}", p.display())));
                    } else {
                        self.file_message = Some((false, "目标不能为空".to_owned()));
                    }
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
                        self.state.selected_event = None;
                        self.state.selected_note = None;
                        sel_changed = true;
                    }
                    TreeAction::SelectTrack(id) => {
                        self.state.selected_track = id;
                        self.state.selected_event = None;
                        sel_changed = true;
                    }
                    TreeAction::SelectEvent(i) => {
                        self.state.selected_event = Some(i);
                        sel_changed = true;
                    }
                    TreeAction::SelectNote(i) => {
                        self.state.selected_note = Some(i);
                        self.state.selected_event = None;
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
            ui.separator();
            ui.label("更新广播");
            ui.monospace(format!("已应用 {} 条", self.applied_broadcasts));
            ui.monospace(format!("最近 {}", self.last_broadcast));
            ui.monospace(format!("话题 {}", self.last_topics.join(", ")));
            ui.monospace(format!("整表重建 {:.3} ms", self.last_structure_ms));
            ui.monospace(format!(
                "重建 整表{}/属性{}/音符{}/轨道{}",
                self.builds_structure, self.builds_props, self.builds_notes, self.builds_tracks
            ));
            ui.monospace(format!(
                "跳过 整表{}/属性{}/音符{}/轨道{}",
                self.skipped_structure, self.skipped_props, self.skipped_notes, self.skipped_tracks
            ));
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
            if let Some(line) = self.state.selected() {
                let perf = line.perf(&self.state.chart.tmap, self.state.playhead);
                let p = ui.painter();
                let col = egui::Color32::from_rgb(250, 220, 120);
                for sx in [-1.0_f32, 1.0] {
                    let q = perf.apply([sx * self.state.line_half_w, 0.0]);
                    let pos = rpe_of(q[0], q[1]);
                    p.circle_stroke(pos, 5.0, egui::Stroke::new(1.2, col));
                    p.line_segment([pos - egui::vec2(9.0, 0.0), pos + egui::vec2(9.0, 0.0)], egui::Stroke::new(1.0, col));
                }
                let q = perf.apply([self.state.line_half_w, 0.0]);
                p.text(
                    rpe_of(q[0], q[1]) + egui::vec2(10.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    format!("线 #{} 长 {:.0}", line.index, self.state.line_half_w * 2.0),
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
            let resp = ui.allocate_rect(tl_rect, egui::Sense::click_and_drag());
            let p = ui.painter_at(tl_rect);
            p.rect_filled(tl_rect, 2.0, egui::Color32::from_rgb(16, 16, 24));
            let dur = self.state.chart.duration.max(0.001);
            let x_of = |t: f64| tl_rect.min.x + (t / dur) as f32 * tl_rect.width();

            // 拍线（自适应抽稀）：整谱可见时拍线密度会远超像素密度 ——
            // 20 万音符的谱面曾按原步长画出 5 万条线，把帧时间拖到 20 ms（且镶嵌开销不计入 ui_ms）。
            let px_per_sec = tl_rect.width() / dur as f32;
            let base = self.state.chart.beat_interval;
            let mut mult = 1u32;
            while ((base * mult as f64) * px_per_sec as f64) < 6.0 && mult < 4096 {
                mult *= 2;
            }
            let step = base * mult as f64;
            let mut t = 0.0;
            let mut beat = 0u32;
            while t <= dur {
                let x = x_of(t);
                let major = beat % 4 == 0;
                let h = if major { tl_rect.height() * 0.45 } else { tl_rect.height() * 0.22 };
                p.line_segment(
                    [egui::pos2(x, tl_rect.max.y - h), egui::pos2(x, tl_rect.max.y)],
                    egui::Stroke::new(
                        if major { 1.0 } else { 0.5 },
                        if major {
                            egui::Color32::from_rgb(90, 95, 130)
                        } else {
                            egui::Color32::from_rgb(55, 58, 80)
                        },
                    ),
                );
                t += step;
                beat += mult;
            }
            p.text(
                tl_rect.min + egui::vec2(6.0, 4.0),
                egui::Align2::LEFT_TOP,
                format!("拍线 1/{mult} 步长（{step:.3}s），共 {} 条", (dur / step) as u64),
                egui::FontId::monospace(10.0),
                egui::Color32::from_rgb(120, 125, 160),
            );

            // ---- 事件与子音符（当前判定线）----
            // 判定线的事件轨道是"表演"的本体，时间轴上必须看得见：
            // · 事件跨度画成横条（底带）；
            // · 当前轨道画成折线（用缓存好的采样点，逐帧只做绘制）；
            // · 子音符画成小方块（时长按 Hold 拉长）。
            if let Some(line) = self.state.selected() {
                let track = line.track(self.state.selected_track);
                // 事件条（底带）
                for e in &track.events {
                    let (t0, t1) = (
                        self.state.chart.tmap.sec(e.start.to_f64()),
                        self.state.chart.tmap.sec(e.end.to_f64()),
                    );
                    let r = egui::Rect::from_min_max(
                        egui::pos2(x_of(t0), tl_rect.max.y - 6.0),
                        egui::pos2(x_of(t1), tl_rect.max.y - 1.0),
                    );
                    p.rect_filled(r, 0.0, egui::Color32::from_rgba_unmultiplied(120, 200, 255, 90));
                }
                // 折线：值域归一化到时间轴上 2/3 高度
                if track.curve.len() >= 2 {
                    let (mn, mx) = (track.min, track.max);
                    let span = (mx - mn).max(1e-6);
                    let top = tl_rect.min.y + tl_rect.height() * 0.08;
                    let h = tl_rect.height() * 0.62;
                    let pts: Vec<egui::Pos2> = track
                        .curve
                        .iter()
                        .map(|q| {
                            let y = top + h * (1.0 - (q[1] - mn) / span);
                            egui::pos2(x_of(q[0] as f64), y)
                        })
                        .collect();
                    p.add(egui::Shape::line(
                        pts,
                        egui::Stroke::new(1.5, egui::Color32::from_rgb(120, 220, 160)),
                    ));
                }
                // 子音符
                let sel_note = self.state.selected_note;
                for (i, n) in line.notes.iter().enumerate() {
                    let y = tl_rect.min.y + tl_rect.height() * 0.74;
                    let x0 = x_of(n.time);
                    let x1 = x_of(n.end.max(n.time));
                    let col = if Some(i) == sel_note {
                        egui::Color32::from_rgb(255, 255, 255)
                    } else {
                        let c = n.kind.color();
                        egui::Color32::from_rgb(
                            (c[0] * 255.0) as u8,
                            (c[1] * 255.0) as u8,
                            (c[2] * 255.0) as u8,
                        )
                    };
                    p.rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(x0, y),
                            egui::pos2(x1.max(x0 + 1.5), y + 6.0),
                        ),
                        0.0,
                        col,
                    );
                }
                p.text(
                    tl_rect.min + egui::vec2(tl_rect.width() - 6.0, 4.0),
                    egui::Align2::RIGHT_TOP,
                    format!(
                        "线 #{} · 轨道 {}（{} 条事件）",
                        line.index,
                        self.state.selected_track.key(),
                        track.events.len()
                    ),
                    egui::FontId::monospace(10.0),
                    egui::Color32::from_rgb(120, 220, 160),
                );
            }

            // 可见窗口
            let vis = self.state.visible_range();
            p.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x_of(vis.0), tl_rect.min.y),
                    egui::pos2(x_of(vis.1), tl_rect.max.y),
                ),
                0.0,
                egui::Color32::from_rgba_unmultiplied(90, 130, 220, 28),
            );
            // 播放头
            let px = x_of(self.state.playhead);
            p.line_segment(
                [egui::pos2(px, tl_rect.min.y), egui::pos2(px, tl_rect.max.y)],
                egui::Stroke::new(1.5, egui::Color32::from_rgb(240, 240, 120)),
            );

            if resp.clicked() || resp.dragged() {
                if let Some(pos) = resp.interact_pointer_pos() {
                    let frac = ((pos.x - tl_rect.min.x) / tl_rect.width()).clamp(0.0, 1.0);
                    self.state.seek(frac as f64 * dur);
                }
            }
        });

        // ---- 缺少 7z（**最后画**：画序决定谁在上面，它必须在所有面板之上）----
        //
        // 与启动页上的那一份是**同一个实现**（`recents::missing_7z_modal` + `dialog` 模块）：
        // 同一件事在两处画成两种样子，就是这个文件里最容易长出来的不一致。
        // 它只在"带着 `--doc` 直接进编辑页、而系统里没有 7z"这条路上出现（无 `--doc` 时
        // 启动页就会把它拦住），所以这里不重复解释门槛的理由。
        if let Some(msg) = self.seven_zip_missing.clone() {
            let ctx = ui.ctx().clone();
            let out = opm_app::recents::missing_7z_modal(&ctx, &msg, cfg!(windows));
            if out.fetch {
                match filedialog::open_url(zip::SEVEN_ZIP_URL) {
                    Ok(()) => {
                        self.seven_zip_missing = Some(format!(
                            "{msg}\n（已用浏览器打开下载页；装好之后重启 OpenPhM）"
                        ));
                    }
                    Err(e) => {
                        self.seven_zip_missing =
                            Some(format!("{msg}\n（打开浏览器失败：{e}；地址：{}）", zip::SEVEN_ZIP_URL));
                    }
                }
            }
            if out.quit {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
        let awaiting = self.pending_dispatch.is_some() || self.dirty.any();
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

/// 把 egui 的 `ColorImage` 写成 PNG（复用已有的 `png` 依赖，不额外引 crate）
fn write_png(img: &egui::ColorImage, path: &std::path::Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width() as u32, img.height() as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    // ColorImage 是 premultiplied RGBA
    let mut buf: Vec<u8> = Vec::with_capacity(img.pixels.len() * 4);
    for p in &img.pixels {
        buf.extend_from_slice(&[p.r(), p.g(), p.b(), p.a()]);
    }
    writer.write_image_data(&buf).map_err(|e| e.to_string())
}

/// 解析该用哪个音频文件：`--audio FILE` 优先，其次谱面 `meta.audio`（相对谱面目录），
/// `--audio off` 表示明确不要音频。
/// 该放哪段音频：**路径决策在库里**（`audio::resolve_source`，纯函数 + 单测），
/// 这里只负责"真的去读文件并开设备"。
fn resolve_audio(args: &Args, core: &core::SharedCore) -> Result<Option<audio::Audio>, String> {
    let (meta_audio, chart_path) = {
        let c = core.lock().unwrap();
        (
            c.doc().meta.audio.clone(),
            c.path().map(std::path::Path::to_path_buf),
        )
    };
    let Some((path, src)) = audio::resolve_source(
        args.audio.as_deref(),
        meta_audio.as_deref(),
        chart_path.as_deref(),
    )?
    else {
        return Ok(None);
    };
    audio::Audio::load(&path).map(Some).map_err(|e| format!("{e}（来源：{}）", src.label()))
}

// 空格键的规则已整体搬进 `keymap::SpacePlayback`（单点 = 进入/退出自动播放，长按 = 松手退出）。
// 这里不再留"按下就 toggle"的旧实现 —— 同一件事有两份实现时，早晚有人用错那一份。



