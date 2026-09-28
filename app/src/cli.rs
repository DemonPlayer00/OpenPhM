//! **命令行参数**：解析是纯函数（不吃 `argv`、不打印、不退出），于是能单测。
//!
//! 为什么值得单独一层：这串参数是 **agent 与 CI 唯一的入口**（`--doc` / `--shot` / `--control` /
//! `--bench` / `--dialog` …），而它原先长在 `main.rs` 里、`parse_args()` 直接读 `std::env::args()`
//! 并在 `--help` 时 `exit(0)` —— 于是**一条测试都写不了**，改坏了只能靠"跑一下看看"。
//! 现在：`parse(argv)` 返回参数 + 警告 + 是否要打用法，`main` 只负责把这三样打印出来。

use crate::state;

/// 工作区预设：不同任务下该看什么、该让什么占主位
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Workspace {
    /// 制谱：列表 + 演奏区 + 时间轴 + 检查器（默认）
    Compose,
    /// 时间轴：时间轴占主位，隐藏列表，用于对齐节拍与长条
    Timeline,
    /// 表演：只留演奏区，用于检查判定线表演与观感
    Perform,
    /// 调试：全部面板 + 诊断，用于排障与性能观测
    Debug,
}

impl Default for Workspace {
    fn default() -> Self {
        Workspace::Compose
    }
}

impl Workspace {
    pub const ALL: [Workspace; 4] = [
        Workspace::Compose,
        Workspace::Timeline,
        Workspace::Perform,
        Workspace::Debug,
    ];
    pub fn from_str(s: &str) -> Option<Workspace> {
        match s {
            "compose" => Some(Workspace::Compose),
            "timeline" => Some(Workspace::Timeline),
            "perform" => Some(Workspace::Perform),
            "debug" => Some(Workspace::Debug),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Workspace::Compose => "制谱",
            Workspace::Timeline => "时间轴",
            Workspace::Perform => "表演",
            Workspace::Debug => "调试",
        }
    }
    pub fn show_list(self) -> bool {
        matches!(self, Workspace::Compose | Workspace::Debug)
    }
    pub fn show_inspector(self) -> bool {
        !matches!(self, Workspace::Perform)
    }
    pub fn show_timeline(self) -> bool {
        !matches!(self, Workspace::Perform)
    }
    /// 时间轴面板占中央区高度的比例（`--ws` 各预设目前同高）。
    ///
    /// **这个数是量出来的，不是拍的**：把重构前的二进制与候选值各截一帧（1600×900）逐像素比对，
    /// 取到**完全一致**的那一个（0.22；0.25/0.28 各差约 670 行，0.22 差 0 行），
    /// 再在 1600×1100 上复核 —— 比例是线性量，两个高度都对才算数。
    /// 起因：这次重构把一个没写进任何文档的常量从 `main.rs` 搬走，**差点凭手感编一个** ——
    /// 这类"界面比例"常量文档里查不到、也没有测试钉住，只能靠渲染结果反推。
    pub fn timeline_frac(self) -> f32 {
        0.22
    }
    pub fn list_width(self) -> f32 {
        match self {
            Workspace::Debug => 360.0,
            // 300px ≈ 41 个等宽字符：判定线行要在一行内放完（线名+子音符数+事件数+此刻表演值）
            _ => 300.0,
        }
    }
}

/// 命令行参数（**DTO**：字段全公开，因为 GUI 各处都要读它们）
#[derive(Clone, Debug)]
pub struct Args {
    pub notes: usize,
    pub bench: u32,
    pub stress: bool,
    pub scale: Option<f32>,
    pub fps_cap: Option<f64>,
    pub lookahead: f64,
    pub width: f32,
    pub height: f32,
    pub verify_align: bool,
    /// 空闲时的重绘频率（0 = 完全不主动重绘，纯事件驱动）
    pub idle_fps: f64,
    /// bench 模式：活跃阶段之后转入空闲阶段并测量秒数（0 = 跳过）
    pub idle_seconds: f64,
    pub ws: Option<Workspace>,
    pub doc: Option<String>,
    /// 控制通道：`--control`（自动路径）或 `--control <path>`；不启用则不传该参数
    pub control: Option<String>,
    /// 每次广播与重建都打一行（排查"到底谁被更新了"用）
    pub verbose_updates: bool,
    /// `--trace-startup`：把"进程启动 → 首帧"之间每一步的耗时打出来（启动慢在哪，猜不如量）
    pub trace_startup: bool,
    /// 窗口位置 `--pos X,Y` —— 只是给合成器的提示，Wayland 下会被无视（实测）
    pub pos: Option<(f32, f32)>,
    /// `--shot PATH`：**让应用自己截图**（egui viewport 截图，与合成器无关）。
    /// 这比外部抓屏可靠得多：位置/缩放/遮挡都不影响，agent 也能据此"看界面"。
    pub shot: Option<String>,
    /// 第几帧截图（等布局与首帧动画稳定）
    pub shot_frame: u32,
    /// 截完就退出
    pub shot_exit: bool,
    /// 是否绘制窗口边界框（RPE ±675 × ±450）
    pub boundary: bool,
    /// 判定线全长（编辑器设置，默认 1350 = 与窗口同宽；RPE 格式里没有这个字段）
    pub line_len: f32,
    /// 音频：`--audio FILE` 指定文件，`--audio off` 明确不要音频；
    /// 不给则用谱面 `meta.audio`（相对谱面目录解析）
    pub audio: Option<String>,
    /// 启动即播放（`--autoplay`）
    pub autoplay: bool,
    /// 用户校准偏移（毫秒）：正值表示"听到的比游标算出来的更早"
    pub audio_offset_ms: f64,
    /// 编辑区叠加层：默认开（自动播放中或按住 H 时自动隐藏）
    pub overlay: bool,
    /// 编辑区纵向可见拍数
    pub overlay_beats: f64,
    /// 编辑区底板黑度 0~1
    pub overlay_alpha: f32,
    /// `--audio-probe FILE`：只解码并打印信息然后退出（agent/排障用，不开窗口）
    pub audio_probe: Option<String>,
    /// `--fonts`：只做字体自检（装进无头 egui、逐字问有没有字形）然后退出，**不开窗口**。
    /// 这是"中文会不会变豆腐块"的可执行证据 —— 在 Windows/Wine 上也能跑。
    pub fonts: bool,
    /// 音符区窗口 X 偏移（视图设置）：`--window-offset X`（RPE 单位）
    pub window_offset: f32,
    /// 启动时把某个对话框摊开（截图/agent 验证用）：`--dialog file|new|guard`
    pub dialog: Option<String>,
    /// 网格（视图设置）：`--beat-div`=每拍几条、`--lane-div`=窗口几等分
    pub beat_div: Option<u32>,
    pub lane_div: Option<u32>,
}

impl Args {
    /// 无头用途（不显示起始界面）：bench/stress 这类跑完就退的
    pub fn bench_only(&self) -> bool {
        self.bench > 0 || self.verify_align
    }
}

impl Default for Args {
    fn default() -> Self {
        Self {
            // **默认不生成任何谱面**：编辑页不该内建一份演示谱面（用户要求清掉它）。
            // 需要压力测试/截图时显式给 `--notes N`。
            notes: 0,
            bench: 0,
            stress: false,
            scale: None,
            fps_cap: None,
            lookahead: 2.0,
            width: 1600.0,
            height: 900.0,
            verify_align: false,
            idle_fps: 1.0,
            idle_seconds: 0.0,
            ws: None,
            doc: None,
            control: None,
            verbose_updates: false,
            trace_startup: false,
            pos: None,
            shot: None,
            shot_frame: 30,
            shot_exit: false,
            boundary: true,
            line_len: state::RPE_LINE_HALF_W * 2.0,
            audio: None,
            autoplay: false,
            audio_offset_ms: 0.0,
            overlay: true,
            // 默认缩放与 EditorState 保持同一个来源（曾经两处各写 32.0，改一处就会不一致）
            overlay_beats: state::EditorState::DEFAULT_OVERLAY_BEATS,
            overlay_alpha: 0.82,
            audio_probe: None,
            fonts: false,
            beat_div: None,
            lane_div: None,
            window_offset: 0.0,
            dialog: None,
        }
    }
}

/// 解析结果：参数本身 + 给用户看的警告 + "他要看用法"
#[derive(Debug)]
pub struct Parsed {
    pub args: Args,
    /// 认不出来的参数 / 认不出来的取值（调用方负责打印到 stderr）
    pub warnings: Vec<String>,
    /// `--help`/`-h`：调用方打印用法后退出
    pub help: bool,
}

/// 解析命令行（**纯函数**：同样的 argv 必然同样的结果，不读环境、不打印、不退出）
pub fn parse(argv: &[String]) -> Parsed {
    let mut a = Args::default();
    let mut warnings: Vec<String> = Vec::new();
    let mut help = false;
    let mut i = 0;
    fn num<T: std::str::FromStr>(s: &str) -> Option<T> {
        s.parse().ok()
    }
    while i < argv.len() {
        let take = |i: &mut usize| -> Option<String> {
            *i += 1;
            argv.get(*i).cloned()
        };
        match argv[i].as_str() {
            "--notes" => a.notes = take(&mut i).and_then(|v| num(&v)).unwrap_or(a.notes),
            "--bench" => a.bench = take(&mut i).and_then(|v| num(&v)).unwrap_or(0),
            "--scale" => a.scale = take(&mut i).and_then(|v| num(&v)),
            "--fps-cap" => a.fps_cap = take(&mut i).and_then(|v| num(&v)),
            "--lookahead" => a.lookahead = take(&mut i).and_then(|v| num(&v)).unwrap_or(a.lookahead),
            "--width" => a.width = take(&mut i).and_then(|v| num(&v)).unwrap_or(a.width),
            "--height" => a.height = take(&mut i).and_then(|v| num(&v)).unwrap_or(a.height),
            "--stress" => a.stress = true,
            "--verify-align" => a.verify_align = true,
            "--idle-fps" => a.idle_fps = take(&mut i).and_then(|v| num(&v)).unwrap_or(a.idle_fps),
            "--idle-seconds" => a.idle_seconds = take(&mut i).and_then(|v| num(&v)).unwrap_or(0.0),
            "--ws" => {
                let v = take(&mut i);
                a.ws = v.as_deref().and_then(Workspace::from_str);
                if let (Some(v), None) = (v.as_deref(), a.ws) {
                    // 静默忽略会让人以为"我明明设了" —— 说清楚可选值
                    warnings.push(format!(
                        "未知工作区 {v:?}（可选 compose/timeline/perform/debug）"
                    ));
                }
            }
            "--doc" => a.doc = take(&mut i),
            "--control" => {
                // 允许 `--control`（自动路径）或 `--control <path>`
                let next = argv.get(i + 1).filter(|v| !v.starts_with('-')).cloned();
                match next {
                    Some(p) => {
                        a.control = Some(p);
                        i += 1;
                    }
                    None => a.control = Some("auto".into()),
                }
            }
            "--help" | "-h" => help = true,
            "--verbose-updates" => a.verbose_updates = true,
            "--trace-startup" => a.trace_startup = true,
            "--file-dialog" => a.dialog = Some("file".to_owned()), // 旧写法，等价于 --dialog file
            "--dialog" => a.dialog = take(&mut i),
            "--window-offset" => {
                a.window_offset = take(&mut i).and_then(|v| num(&v)).unwrap_or(0.0)
            }
            "--shot" => {
                if let Some(v) = take(&mut i) {
                    a.shot = Some(v);
                }
            }
            "--shot-frame" => {
                if let Some(v) = take(&mut i) {
                    a.shot_frame = v.parse().unwrap_or(30);
                }
            }
            "--shot-exit" => a.shot_exit = true,
            "--boundary" => {
                if let Some(v) = take(&mut i) {
                    a.boundary = !matches!(v.as_str(), "off" | "0" | "no" | "false");
                }
            }
            "--line-len" => {
                if let Some(v) = take(&mut i) {
                    a.line_len = v.parse().unwrap_or(a.line_len);
                }
            }
            "--audio" => {
                if let Some(v) = take(&mut i) {
                    a.audio = Some(v);
                }
            }
            "--autoplay" => a.autoplay = true,
            "--overlay" => {
                if let Some(v) = take(&mut i) {
                    a.overlay = !matches!(v.as_str(), "off" | "0" | "no" | "false");
                }
            }
            "--beat-div" => {
                if let Some(v) = take(&mut i) {
                    a.beat_div = v.parse().ok();
                }
            }
            "--lane-div" => {
                if let Some(v) = take(&mut i) {
                    a.lane_div = v.parse().ok();
                }
            }
            "--audio-probe" => {
                if let Some(v) = take(&mut i) {
                    a.audio_probe = Some(v);
                }
            }
            "--fonts" => a.fonts = true,
            "--overlay-beats" => {
                if let Some(v) = take(&mut i) {
                    a.overlay_beats = v.parse().unwrap_or(a.overlay_beats);
                }
            }
            "--overlay-alpha" => {
                if let Some(v) = take(&mut i) {
                    a.overlay_alpha = v.parse().unwrap_or(a.overlay_alpha);
                }
            }
            "--audio-offset-ms" => {
                if let Some(v) = take(&mut i) {
                    a.audio_offset_ms = v.parse().unwrap_or(0.0);
                }
            }
            "--pos" => {
                if let Some(v) = take(&mut i) {
                    let mut it = v.split(',');
                    let x = it.next().and_then(|s| s.trim().parse().ok());
                    let y = it.next().and_then(|s| s.trim().parse().ok());
                    if let (Some(x), Some(y)) = (x, y) {
                        a.pos = Some((x, y));
                    } else {
                        warnings.push(format!("--pos 需要 `X,Y` 两个数（收到 {v:?}）"));
                    }
                }
            }
            other => warnings.push(format!("未知参数 {other}（--help 查看用法）")),
        }
        i += 1;
    }
    Parsed {
        args: a,
        warnings,
        help,
    }
}

/// 用法说明（`--help` 打印它；**只有一处**，别在 README 与代码里各写一份）
pub const USAGE: &str = "\
OpenPhM —— Phigros 谱面编辑器（GUI）
用法: opm-app [选项]

文件与启动
  --doc FILE            直接打开谱面（CLI/agent/截图走这条，跳过起始界面）
  --dialog file|new|guard   启动就把对应对话框摊开（截图/人工检查用）
  --file-dialog         = --dialog file（旧写法）
  --audio FILE|off      指定音频，或明确不要音频（默认用谱面 meta.audio）
  --autoplay            启动即播放

窗口与视图
  --width N --height N  编辑页窗口尺寸（默认 1600×900）
  --scale F             像素缩放
  --pos X,Y             窗口位置提示（Wayland 下会被忽略）
  --ws compose|timeline|perform|debug   工作区预设
  --window-offset X     音符区窗口 X 偏移（RPE 单位）
  --boundary on|off     窗口边界框
  --line-len N          判定线全长（RPE 单位）
  --overlay on|off      编辑区叠加层
  --overlay-beats N     叠加层纵向可见拍数
  --overlay-alpha F     叠加层底板黑度 0~1
  --beat-div N --lane-div N   网格：每拍 N 条 / 窗口 N 等分

测量与自动化
  --shot PATH           应用自截屏到 PATH（egui viewport 截图，与合成器无关）
  --shot-frame N        第几帧截（默认 30）
  --shot-exit           截完退出
  --control [PATH]      开控制通道（不写路径 = 自动路径）
  --idle-fps F          空闲重绘频率（0 = 纯事件驱动）
  --idle-seconds S      bench：活跃阶段后转空闲并测量 S 秒
  --bench N             跑 N 帧基准后打印报告
  --stress              极限负载（全量实例）
  --verify-align        对齐自检
  --notes N             造 N 个音符的演示谱面（默认 0：不内建任何谱面）
  --audio-probe FILE    只解码并打印音频信息后退出（不开窗口）
  --fonts               只做 CJK 字体自检（内嵌字体 + 逐字问字形）后退出（不开窗口）
  --verbose-updates     每次广播/重建打一行日志
  --trace-startup       打印启动耗时分解（进程启动 → 首帧；也认 OPM_TRACE_STARTUP=1）
  -h, --help            显示本说明";

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Parsed {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse(&v)
    }

    /// 默认值里有一条**用户可见的约定**：不内建任何演示谱面
    #[test]
    fn defaults_have_no_builtin_chart() {
        let p = parse_str(&[]);
        assert_eq!(p.args.notes, 0, "默认不造演示谱面（用户要求清掉内建的）");
        assert!(p.args.overlay && p.args.boundary, "叠加层与边界框默认开");
        assert_eq!(p.args.shot_frame, 30);
        assert!(!p.args.shot_exit && !p.args.stress);
        assert!(!p.args.bench_only());
        assert!(p.warnings.is_empty());
        assert!(!p.help);
        // 默认缩放与 EditorState 同一个来源（曾经两处各写 32.0）
        assert_eq!(p.args.overlay_beats, state::EditorState::DEFAULT_OVERLAY_BEATS);
    }

    /// agent 那条路：doc + 自截屏 + 帧号 + 截完退出
    #[test]
    fn shot_pipeline_flags_are_parsed() {
        let p = parse_str(&[
            "--doc",
            "/tmp/x.opm.json",
            "--shot",
            "/tmp/a.png",
            "--shot-frame",
            "12",
            "--shot-exit",
        ]);
        assert_eq!(p.args.doc.as_deref(), Some("/tmp/x.opm.json"));
        assert_eq!(p.args.shot.as_deref(), Some("/tmp/a.png"));
        assert_eq!(p.args.shot_frame, 12);
        assert!(p.args.shot_exit);
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    /// `--control` 两种写法：光杆 = 自动路径；带值 = 那个路径，且**不会**被当成多余的位置参数
    #[test]
    fn control_accepts_bare_and_valued_forms() {
        assert_eq!(parse_str(&["--control"]).args.control.as_deref(), Some("auto"));
        let p = parse_str(&["--control", "/tmp/opm.sock", "--stress"]);
        assert_eq!(p.args.control.as_deref(), Some("/tmp/opm.sock"));
        assert!(p.args.stress, "后面的参数还要接着解析");
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        // 后面跟着的是另一个选项 ⇒ 光杆形式
        let p = parse_str(&["--control", "--stress"]);
        assert_eq!(p.args.control.as_deref(), Some("auto"));
        assert!(p.args.stress);
    }

    /// 启动耗时探针：默认关，`--trace-startup` 打开
    #[test]
    fn trace_startup_is_opt_in() {
        assert!(!parse_str(&[]).args.trace_startup);
        assert!(parse_str(&["--trace-startup"]).args.trace_startup);
        assert!(USAGE.contains("--trace-startup"));
    }

    /// 字体自检：默认关，`--fonts` 打开（"中文会不会变豆腐块"的可执行证据，见 `opm_app::fonts`）
    #[test]
    fn fonts_self_check_is_opt_in() {
        assert!(!parse_str(&[]).args.fonts);
        assert!(parse_str(&["--fonts"]).args.fonts);
        assert!(USAGE.contains("--fonts"));
    }

    /// 数值参数写坏了不许 panic、也不许把默认值写成 0（"没听懂就用默认"）
    #[test]
    fn malformed_numbers_keep_defaults_instead_of_panicking() {
        let p = parse_str(&["--notes", "abc", "--width", "x", "--scale", "?", "--shot-frame", "y"]);
        assert_eq!(p.args.notes, 0);
        assert_eq!(p.args.width, 1600.0);
        assert_eq!(p.args.scale, None);
        assert_eq!(p.args.shot_frame, 30, "帧号写坏退回默认 30，而不是 0（0 会永远等不到）");
    }

    /// `--pos` 要两个数；只给一个数时明确报出来（而不是静默忽略）
    #[test]
    fn pos_requires_two_numbers_and_says_so() {
        assert_eq!(parse_str(&["--pos", "100,200"]).args.pos, Some((100.0, 200.0)));
        assert_eq!(parse_str(&["--pos", " 10 , 20 "]).args.pos, Some((10.0, 20.0)));
        let p = parse_str(&["--pos", "100"]);
        assert_eq!(p.args.pos, None);
        assert_eq!(p.warnings.len(), 1);
        assert!(p.warnings[0].contains("X,Y"), "{:?}", p.warnings);
    }

    /// 开关型参数认 `off/0/no/false`（人写的与脚本写的都要能用）
    #[test]
    fn on_off_flags_accept_the_usual_spellings() {
        for off in ["off", "0", "no", "false"] {
            let p = parse_str(&["--boundary", off, "--overlay", off]);
            assert!(!p.args.boundary && !p.args.overlay, "{off}");
        }
        let p = parse_str(&["--boundary", "on"]);
        assert!(p.args.boundary);
    }

    /// 工作区：认识的用上，不认识的**要出声**（静默忽略会让人以为"我明明设了"）
    #[test]
    fn workspace_accepts_known_names_and_warns_on_unknown() {
        assert_eq!(parse_str(&["--ws", "timeline"]).args.ws, Some(Workspace::Timeline));
        let p = parse_str(&["--ws", "nonsense"]);
        assert_eq!(p.args.ws, None);
        assert!(p.warnings[0].contains("compose/timeline/perform/debug"), "{:?}", p.warnings);
    }

    /// 认不出来的参数：报一声，但**不吞掉**后面的参数
    #[test]
    fn unknown_flags_warn_without_eating_the_next_argument() {
        let p = parse_str(&["--nope", "--stress"]);
        assert_eq!(p.warnings.len(), 1);
        assert!(p.warnings[0].contains("--nope"), "{:?}", p.warnings);
        assert!(p.args.stress, "未知参数不该把后面的 --stress 一起吃掉");
    }

    /// `--help`/`-h` 只置一个标志（打印与退出由调用方做，解析层不许 exit）
    #[test]
    fn help_is_reported_not_executed() {
        for f in ["--help", "-h"] {
            let p = parse_str(&[f]);
            assert!(p.help, "{f}");
        }
        assert!(!parse_str(&[]).help);
        assert!(USAGE.contains("--shot") && USAGE.contains("--control"));
    }

    /// 工作区的可见性规则（面板布局靠它决定，改错就是"某个面板突然不见了"）
    #[test]
    fn workspace_presets_control_which_panels_show() {
        assert!(Workspace::Compose.show_list() && Workspace::Compose.show_inspector());
        assert!(!Workspace::Timeline.show_list(), "时间轴工作区隐藏列表");
        assert!(Workspace::Timeline.show_timeline());
        assert!(!Workspace::Perform.show_inspector() && !Workspace::Perform.show_timeline());
        assert!(Workspace::Debug.show_list());
        assert_eq!(Workspace::ALL.len(), 4);
        assert_eq!(Workspace::Debug.list_width(), 360.0);
        assert_eq!(Workspace::from_str("compose"), Some(Workspace::Compose));
        assert_eq!(Workspace::default(), Workspace::Compose);
    }
}
