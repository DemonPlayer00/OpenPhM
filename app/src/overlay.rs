//! 编辑区叠加层：**拍为纵轴**，左半音符 / 右半事件，叠在演奏区上。
//!
//! 为什么单独一层而不是又一个面板：演奏区是"游戏里会变成什么样"的唯一视图，
//! 编辑时要同时看"表现"与"数据"。叠加层半透明地盖在预览上，按 `H`（或自动播放中）藏起来，
//! 就能在"编"与"看"之间切换，不必来回挪面板。
//!
//! ```text
//!  ┌─────────────────────────── play_rect ───────────────────────────┐
//!  │ 拍号标尺（每 4 拍一个刻度、每拍一条细线）                        │
//!  │ 网格细分 = `EditorState::effective_beat_div()`，画线与吸附同源   │
//!  ├───────────────────────────────┬─────────────────────────────────┤
//!  │ 音符轨道区（当前判定线）      │ 事件区（当前判定线 5 条轨道）   │
//!  │ x = laneX（±675 铺满半宽）    │ x = 轨道列（moveX…speed）       │
//!  │ y = 拍（越上越晚）            │ y = 拍；事件 = 起止拍之间的块   │
//!  └───────────────────────────────┴─────────────────────────────────┘
//! ```
//!
//! 这一层只读 `EditorState`、只产出**动作**（选中），不直接改任何数据 —— 与左侧判定线树同一条规矩。

use opm_app::state::{EditorState, TrackId, RPE_WINDOW_HALF_W, RPE_WINDOW_W};
// 事件边界规则只有一份实现，住在库内 `state`（好让集成测试能调真身）：这里只再导出，GUI 侧的名字不变。
pub use opm_app::state::{prefer_edge, EventEdge};

/// 叠加层产出的动作（由调用方施加，见 `main.rs`）
#[derive(Debug)]
pub enum OverlayAction {
    /// 选中当前判定线的第 i 个音符（时间序）
    SelectNote(usize),
    /// 选中当前轨道（某条轨道列被点击）
    SelectTrack(TrackId),
    /// 选中当前轨道的第 i 条事件
    SelectEvent(usize),
    /// 把播放头定位到某一拍
    SeekBeat(f64),
    /// 把播放头**相对**挪动若干拍（滚轮用；相对量避免"取整再取整"的累积误差）
    ScrollBeats(f64),
    /// Ctrl+滚轮：按倍率缩放时间轴（可见拍数 × 倍率，调用方负责夹到允许范围）
    ZoomBeats(f64),
    /// 开始拖动音符（调用方据此开一个事务，让整段拖拽只占一个撤销步）
    NoteDragStart,
    /// 拖动中：目标位置**已经吸附过**（吸附在面板里做，因为它只依赖网格设置）
    NoteDrag { index: usize, doc_index: usize, lane_x: f32, beat: f64 },
    /// 拖动结束：调用方提交事务
    NoteDragEnd,
    /// 双击空白处放置音符（相同吸附规则）
    PlaceNote { lane_x: f32, beat: f64 },
    /// 事件块头/尾拖拽：起点（调用方开事务）
    EventResizeStart,
    /// 事件块头/尾拖拽中：把 start 或 end 挪到某个拍（已按拍网格吸附）
    EventResize { index: usize, edge: EventEdge, beat: f64 },
    /// 事件块头/尾拖拽结束（调用方提交事务）
    EventResizeEnd,
    /// **快速放置**（Q/W/E/R）：把指针处的音符放下去（位置已吸附；kind = tap/flick/drag/hold）
    QuickPlace { kind: opm_app::doc::NoteKind, lane_x: f32, beat: f64 },
    /// 待放置的 hold：终点跟随鼠标（**只在指针真的移动时**发 —— 滚动/缩放不改长度）
    PendingHoldFollow { beat: f64 },
    /// 待放置的 hold：拖时间控制杆改起点或终点
    PendingHoldResize { edge: EventEdge, beat: f64 },
    /// 待放置的 hold：放下（R / 回车）
    PendingHoldCommit,
    /// 待放置的 hold：取消（Esc）
    PendingHoldCancel,
    /// 面板想跟用户说一句话（例如"指针不在音符区，快速放置用不了"）——
    /// 动作只描述意图，显示在状态栏/控制台由调用方决定
    Notice(String),
}

/// 一条拍网格线
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridLine {
    /// 该线的拍值（**就是吸附目标**）
    pub beat: f64,
    /// 整拍线（比细分线粗）
    pub is_beat: bool,
    /// 小节线（4 拍一条，最粗）
    pub is_bar: bool,
}

/// 纵轴刻度（轴带里的数字标注）。
///
/// **标注密度与网格密度是两件事**：网格多密由设置（每拍 N 条）决定，标注多密由"放不放得下字"决定。
/// 用户要求：*时间轴数字标注应对应产生不同精度* —— 缩放到 4 拍可见时应当能读出 4.25 拍，
/// 缩到 256 拍可见时每条都标就会糊成一条黑带，于是只标 16 拍。
#[derive(Clone, Debug, PartialEq)]
pub struct AxisTick {
    pub beat: f64,
    /// 已按步长定好精度（步长 ≥1 拍 → 整数；1/4 拍 → 两位小数）
    pub text: String,
    /// 粗刻度（步长 ≥ 1 拍，带指向两侧的短刻度线）
    pub major: bool,
}

/// 标注步长：从**最细**（1/16 拍，四位小数就够读数）往上挑第一个"放得下"的整齐步长。
///
/// 阶梯是 4·2^k（…, 1/4, 1/2, 1, 2, 4, 8, …）：永远落在"整拍/半拍/小节"这类人能口算的位置上，
/// 不会出现 1/37 这种没法读的步长。
pub const AXIS_LABEL_MIN_PX: f32 = 34.0;
pub const AXIS_LABEL_FINEST: f64 = 1.0 / 16.0;

pub fn axis_label_step(beats_visible: f64, pane_h: f32) -> f64 {
    let px_per_beat = pane_h.max(1.0) as f64 / beats_visible.max(1e-6);
    let mut step = AXIS_LABEL_FINEST;
    while step <= 1024.0 {
        if step * px_per_beat >= AXIS_LABEL_MIN_PX as f64 {
            return step;
        }
        step *= 2.0;
    }
    1024.0
}

/// 标注该用几位小数（步长定的，不是拍值定的）：1/2 → 1 位、1/4 → 2 位、1/16 → 4 位。
pub fn axis_label_decimals(step: f64) -> usize {
    if step >= 1.0 {
        return 0;
    }
    let mut d = 0usize;
    let mut s = step;
    while s < 1.0 && d < 4 {
        s *= 2.0;
        d += 1;
    }
    d
}

/// 可见窗口内的纵轴刻度（拍号/小数拍），**随缩放自动换精度**。
pub fn axis_ticks(anchor: f64, beats_visible: f64, pane_h: f32) -> Vec<AxisTick> {
    let step = axis_label_step(beats_visible, pane_h);
    let dec = axis_label_decimals(step);
    let first = (anchor / step).floor() as i64;
    let last = ((anchor + beats_visible) / step).ceil() as i64;
    let mut out = Vec::with_capacity((last - first + 1).max(0) as usize);
    let mut k = first;
    while k <= last {
        let beat = k as f64 * step;
        out.push(AxisTick {
            beat,
            text: format!("{:.*}", dec, beat),
            major: step >= 1.0,
        });
        k += 1;
    }
    out
}

/// 可见窗口内的拍网格线。
///
/// 抽成纯函数的原因就是这里踩过坑：档位判定（整拍/小节）曾经用**自增之后**的循环下标去算，
/// 于是带拍号的粗线落在 0.75、1.75、2.75… 上，而吸附发生在整拍上 ——
/// 表现就是"音符/事件吸附后不落在粗线上"（用户报的"没有严格吸附到横轴上"）。
/// 现在"线在哪"与"吸附到哪"是同一份定义：`beat = k / div`，`is_beat = k % div == 0`。
pub fn beat_grid_lines(anchor: f64, beats_visible: f64, div: u32) -> Vec<GridLine> {
    let div = div.max(1) as i64;
    let step = 1.0 / div as f64;
    let first = (anchor / step).floor() as i64;
    let last = ((anchor + beats_visible) / step).ceil() as i64;
    let mut out = Vec::with_capacity((last - first + 1).max(0) as usize);
    let mut k = first;
    while k <= last {
        let is_beat = k % div == 0;
        let is_bar = k % (div * 4) == 0;
        out.push(GridLine {
            beat: k as f64 * step,
            is_beat,
            is_bar,
        });
        k += 1;
    }
    out
}

/// 拍 → 屏幕 y（编辑区纵轴映射）。抽成公开纯函数：绘制与测试共用同一份公式，
/// 否则测试里手算的坐标与实现漂移，等于没测。
pub fn beat_y(body: egui::Rect, anchor: f64, beats: f64, beat: f64) -> f32 {
    body.max.y - ((beat - anchor) / beats) as f32 * body.height()
}

/// laneX → 音符区屏幕 x。`offset` 是窗口 X 偏移：显示区间为 `[offset−675, offset+675]`。
///
/// 抽成纯函数（绘制、命中、测试共用同一份映射）—— 平移之后映射写错一处就会"看到的和吸到的不是同一个地方"。
pub fn lane_to_pane_x(pane: egui::Rect, lane: f32, offset: f32) -> f32 {
    let w = RPE_WINDOW_W;
    let t = (lane - offset + RPE_WINDOW_HALF_W) / w;
    pane.min.x + t.clamp(-2.0, 3.0) * pane.width()
}

/// 音符区屏幕 x → laneX（`lane_to_pane_x` 的逆；不钳制，越界的值交给调用方吸附/判断）
pub fn pane_x_to_lane(pane: egui::Rect, x: f32, offset: f32) -> f32 {
    offset - RPE_WINDOW_HALF_W + (x - pane.min.x) / pane.width().max(1e-6) * RPE_WINDOW_W
}

/// 头/尾把手的判定带宽（像素）
pub const EDGE_BAND: f32 = 6.0;

/// 这个键**本次**是不是被按下的 —— **忽略自动重复**。
///
/// egui 的 `key_pressed` 把 `repeat: true` 也算作"按下"（`num_presses` 只看 `pressed`），
/// 于是"按住 R"会变成：放下 → 又开始放 → 又放下…… 快速放置与"放下 hold"都必须只看**真实按键**。
fn key_pressed_once(ui: &egui::Ui, key: egui::Key) -> bool {
    ui.input(|i| {
        i.events.iter().any(|e| {
            matches!(
                e,
                egui::Event::Key { key: k, pressed: true, repeat: false, .. } if *k == key
            )
        })
    })
}

/// **时间控制杆**：一块"以拍为纵轴"的东西（事件块 / 待放置的 hold）的头尾把手。
///
/// 抽出来的理由：把手有**三样必须一致**的东西 —— 命中带、画出来的样子、拖拽时"指针 → 拍"的换算。
/// 早先它们散在事件块的绘制循环里（只有事件块能用）；待放置的 hold 要用同一套，
/// 于是先收成一处：**判定与绘制共用同一个 `TimeHandle`**，不会再出现"看着能抓、抓不到"。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeHandle {
    /// 左右范围（事件块是那一列的宽度；hold 是音符的横向范围）
    pub x0: f32,
    pub x1: f32,
    /// 起点/终点的屏幕 y（**起点在下**，与编辑区纵轴一致）
    pub start_y: f32,
    pub end_y: f32,
}

impl TimeHandle {
    pub fn from_span(x0: f32, x1: f32, start_y: f32, end_y: f32) -> Self {
        Self { x0, x1, start_y, end_y }
    }

    /// 指针命中哪一部分：x 要先落在范围内，y 用 `hit_event_part`（与事件块**同一份**规则）
    pub fn hit(&self, pointer: egui::Pos2) -> EventPart {
        if pointer.x < self.x0 || pointer.x > self.x1 {
            return EventPart::None;
        }
        hit_event_part(pointer.y, self.start_y, self.end_y, EDGE_BAND)
    }

    /// 画把手：**头**（下方）= 实线 + 短竖标记（方便和尾区分）；**尾**（上方）= 实线
    pub fn paint(&self, painter: &egui::Painter, edge: EventEdge, color: egui::Color32) {
        match edge {
            EventEdge::Start => {
                painter.line_segment(
                    [
                        egui::pos2(self.x0, self.start_y),
                        egui::pos2(self.x1, self.start_y),
                    ],
                    egui::Stroke::new(2.5, color),
                );
                painter.line_segment(
                    [
                        egui::pos2((self.x0 + self.x1) * 0.5, self.start_y),
                        egui::pos2((self.x0 + self.x1) * 0.5, self.start_y - 5.0),
                    ],
                    egui::Stroke::new(2.5, color),
                );
            }
            EventEdge::End => {
                painter.line_segment(
                    [egui::pos2(self.x0, self.end_y), egui::pos2(self.x1, self.end_y)],
                    egui::Stroke::new(2.5, color),
                );
            }
        }
    }
}

/// 指针落在事件块的哪一部分
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventPart {
    Start,
    End,
    Body,
    None,
}

/// 渐变两端的色相偏移（**度**）。用户要求：**渐变不要用透明度，用色相偏移**。
pub const GRAD_HUE_SHIFT_DEG: f32 = 40.0;

/// 在 **sRGB** 空间里偏色相。
///
/// 为什么不用 `egui::ecolor::Hsva`：它在**线性**空间里算色相，于是"偏 0.11 圈"在屏幕上只有约 28°
/// （gamma 是逐通道幂函数，会改变非灰颜色的色相）。自己按 sRGB 算，常量的字面值就等于肉眼看到的度数 ——
/// 上一条注释里把 0.11 写成"≈40°"就是这么错的，实测才发现。
fn shift_hue_srgb(c: egui::Color32, deg: f32, alpha: u8) -> egui::Color32 {
    let (r, g, b) = (
        c.r() as f32 / 255.0,
        c.g() as f32 / 255.0,
        c.b() as f32 / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let mut h = if d == 0.0 {
        0.0
    } else if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    h = (h * 60.0 + deg).rem_euclid(360.0);
    let sat = if max == 0.0 { 0.0 } else { d / max };
    // HSV → RGB（同一个 sRGB 空间）
    let ch = sat * max;
    let x = ch * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = max - ch;
    let (r2, g2, b2) = match (h / 60.0) as u32 {
        0 => (ch, x, 0.0),
        1 => (x, ch, 0.0),
        2 => (0.0, ch, x),
        3 => (0.0, x, ch),
        4 => (x, 0.0, ch),
        _ => (ch, 0.0, x),
    };
    egui::Color32::from_rgba_unmultiplied(
        ((r2 + m) * 255.0).round() as u8,
        ((g2 + m) * 255.0).round() as u8,
        ((b2 + m) * 255.0).round() as u8,
        alpha,
    )
}

/// sRGB 空间里的色相（度）——测试用，也用于文档里的"实测"口径
#[cfg_attr(not(test), allow(dead_code))]
pub fn hue_deg(c: egui::Color32) -> f32 {
    let (r, g, b) = (
        c.r() as f32 / 255.0,
        c.g() as f32 / 255.0,
        c.b() as f32 / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d == 0.0 {
        return 0.0;
    }
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0).rem_euclid(360.0)
}

/// 事件块的渐变两端色：`(起点色, 终点色)`。
///
/// **只偏色相**：饱和度、明度、alpha 都保持一致 —— 这样"头尾"的区分不靠明暗，
/// 叠在深色预览上也不会因为半透明而糊成一块。选中时整体更亮一档（两端一起变）。
pub fn gradient_colors(
    base: egui::Color32,
    selected: bool,
) -> (egui::Color32, egui::Color32) {
    // 选中态只在**不透明度**上更实一点，两端仍然一致（渐变本身不碰 alpha）
    let a = if selected { 250 } else { 170 };
    (
        shift_hue_srgb(base, 0.0, a),
        shift_hue_srgb(base, GRAD_HUE_SHIFT_DEG, a),
    )
}

/// 该给哪个块、哪一头画把手高亮。
///
/// 规则：**拖拽中优先**（亮你抓住的那一头），否则亮本帧命中。
/// 抽出来的意义在于：高亮与拖拽必须用**同一个** `(事件, 头/尾)` —— 早先高亮是在绘制循环里
/// 按块各自判定的，头尾相接时两个块各有一条边落在同一条 y 上 ⇒ **两个把手一起亮**，
/// 看起来像"一次选中了两个事件"。现在只有一处决策。
pub fn handle_to_highlight(
    dragging: Option<(usize, EventEdge)>,
    hover: Option<(usize, EventEdge)>,
) -> Option<(usize, EventEdge)> {
    dragging.or(hover)
}

// `prefer_edge`（事件边界优先规则）与 `EventEdge` 已上移到 `opm_app::state`，本模块只再导出（见文件头）。

/// 纯函数：给定指针 y、事件**起点/终点**在屏幕上的 y（与块体范围），判断命中哪一部分。
///
/// 注意这里刻意**不接收 Rect**：本编辑器的纵轴是拍、且越往上越晚，所以事件的**起点在下方**。
/// 早先用 `rect.min.y` 当"头"（上边）—— 那其实是**尾**，于是"抓到头却去改尾"（测试里表现为
/// `EventResize(End, …)`）。改成直接按语义传两个 y，方向就不靠猜了。
pub fn hit_event_part(pointer_y: f32, start_y: f32, end_y: f32, band: f32) -> EventPart {
    let (lo, hi) = (start_y.min(end_y), start_y.max(end_y));
    if pointer_y < lo - band || pointer_y > hi + band {
        return EventPart::None;
    }
    let to_start = (pointer_y - start_y).abs();
    let to_end = (pointer_y - end_y).abs();
    // 块比两倍带宽还矮时两头会抢同一段：取**更近**的那一头（否则小事件根本抓不到边）
    if (hi - lo) <= band * 2.0 {
        return if to_start <= to_end {
            EventPart::Start
        } else {
            EventPart::End
        };
    }
    if to_start <= band {
        return EventPart::Start;
    }
    if to_end <= band {
        return EventPart::End;
    }
    if pointer_y >= lo && pointer_y <= hi {
        return EventPart::Body;
    }
    EventPart::None
}

/// 把"按下时指针下面是什么"记下来：egui 要等指针移动几个像素才认定为拖拽，
/// 那时指针可能**已经离开 6px 的把手段** —— 用"当前命中"判断会永远拖不起来。
/// 编码成 (kind, index, edge)：0=音符 1=事件头 2=事件尾 3=其它
fn press_hit_set(ui: &egui::Ui, v: Option<(u8, usize, u8)>) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new("opm_press_hit"), v));
}
fn press_hit_get(ui: &egui::Ui) -> Option<(u8, usize, u8)> {
    ui.data(|d| d.get_temp::<Option<(u8, usize, u8)>>(egui::Id::new("opm_press_hit")))
        .flatten()
}

/// 正在拖的音符（egui 临时内存）。
///
/// 拖拽一开始就把它记下来，而不是每帧重新做命中测试 —— 指针移动快时会离开音符的命中范围，
/// 那样拖拽会中途"掉线"。
fn drag_note_set(ui: &egui::Ui, v: Option<usize>) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new("opm_note_drag"), v));
}
fn drag_note_get(ui: &egui::Ui) -> Option<usize> {
    ui.data(|d| d.get_temp::<Option<usize>>(egui::Id::new("opm_note_drag")))
        .flatten()
}

/// 事件块正在拖哪一头（egui 临时内存：纯视图瞬态，不进 EditorState）
fn drag_edge_set(ui: &egui::Ui, v: Option<(usize, EventEdge)>) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new("opm_event_drag"), v));
}
fn drag_edge_get(ui: &egui::Ui) -> Option<(usize, EventEdge)> {
    ui.data(|d| d.get_temp::<Option<(usize, EventEdge)>>(egui::Id::new("opm_event_drag")))
        .flatten()
}

/// 滚轮位移 → 拍增量（纯函数，可单测）。
///
/// 约定：**向上滚 = 时间往后**（与纵轴"越上越晚"一致）。步长随可见拍数缩放：
/// 放大到 8 拍可见时一格走半拍，缩到 256 拍可见时一格走 16 拍 —— 无论缩放到哪一级，
/// "一格"在屏幕上走过的距离都差不多。
pub fn scroll_delta_to_beats(scroll_y: f32, beats_visible: f64, per_notch: f64) -> f64 {
    (scroll_y as f64 / 50.0) * per_notch * (beats_visible / 32.0).max(0.15)
}

/// egui 的 `zoom_delta` → **可见拍数倍率**（纯函数，可单测）。
///
/// 约定：**向上滚 = 放大**（可见拍数变少、看得更细）。不按 Ctrl 时滚轮移动时间轴，
/// 按住 Ctrl 时滚轮缩放 —— 但**缩放事件不是从 `smooth_scroll_delta` 拿的**：
/// egui 0.36 在 `InputState::begin_pass` 里发现 `wheel.modifiers` 命中 `options.zoom_modifier`
/// 就不把滚动写进 `smooth_scroll_delta`，而是折算成 `zoom_factor_delta`
/// （`exp(scroll_zoom_speed · Δ)`）。上一版按 ctrl 去读 `smooth_scroll_delta`，
/// 于是按住 Ctrl 时读到的是 0 —— 无头测试里实测出来的，不是推理出来的。
/// `zoom_delta > 1` = 放大 ⇒ 可见拍数要乘上它的倒数。
pub fn zoom_delta_to_beats_factor(zoom_delta: f32) -> f64 {
    if !zoom_delta.is_finite() || zoom_delta <= 0.0 {
        return 1.0;
    }
    1.0 / zoom_delta as f64
}

/// 叠加层的可见性规则。抽成纯函数：**自动播放中或按住 H 时隐藏**这条规则要有单测钉住。
pub fn overlay_visible(enabled: bool, playing: bool, h_held: bool) -> bool {
    enabled && !playing && !h_held
}

/// 编辑区可调参数（视图状态，和播放头一样不属于文档）
#[derive(Clone, Copy, Debug)]
pub struct OverlayCfg {
    /// 窗口底部相对播放头的偏移（拍）：默认往前留 2 拍，方便看"刚过去"的音符
    pub lead_beats: f64,
    /// 底板黑度 0~1：越大越黑（预览越看不见）。数据可读性 vs 预览可见性的取舍旋钮。
    pub body_alpha: f32,
    /// 滚轮一格（约 50 单位）走多少拍；实际步长按可见拍数缩放（放大时走得更细）
    pub scroll_beats_per_notch: f64,
}

impl Default for OverlayCfg {
    fn default() -> Self {
        Self {
            lead_beats: 2.0,
            body_alpha: 0.82,
            scroll_beats_per_notch: 2.0,
        }
    }
}

const RULER_H: f32 = 18.0;
/// 中间的**纵轴轴带**宽度：拍号写在这里，把"音符区"和"事件区"隔开。
/// 放在中间而不是贴左边缘：它是两个区共用的纵轴，贴边会让它看起来只属于左半。
const AXIS_W: f32 = 34.0;

/// 事件值的紧凑写法（块内空间小；长小数会被截成噪声）
fn fmt_val(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            if (f - f.round()).abs() < 1e-6 {
                format!("{:.0}", f)
            } else {
                format!("{:.2}", f)
            }
        }
        other => other.to_string(),
    }
}
const TRACK_COLORS: [[u8; 3]; 5] = [
    [120, 190, 255], // moveX
    [150, 220, 170], // moveY
    [255, 200, 120], // rotate
    [220, 160, 240], // alpha
    [255, 150, 150], // speed
];

/// 画叠加层。返回本帧产生的动作。
#[allow(clippy::too_many_arguments)]
pub fn draw(
    ui: &mut egui::Ui,
    st: &EditorState,
    rect: egui::Rect,
    cfg: &OverlayCfg,
    // 现在能不能用快捷键（打字/模态期间为 false，由调用方算好 —— 门控只有一处）
    keys_enabled: bool,
    actions: &mut Vec<OverlayAction>,
) {
    let p = ui.painter_at(rect);
    let beat_now = st.chart.tmap.beat(st.playhead);
    // 窗口：底部为 anchor，向上到 anchor + beats_visible
    let anchor = beat_now - cfg.lead_beats;
    let beats = st.overlay_beats.max(4.0);
    let body = egui::Rect::from_min_max(
        egui::pos2(rect.min.x, rect.min.y + RULER_H),
        rect.max,
    );
    let y_of = |beat: f64| beat_y(body, anchor, beats, beat);
    let beat_of = |y: f32| anchor + ((body.max.y - y) / body.height()) as f64 * beats;

    // 底板：半透明，下面的预览仍看得见
    // 底板黑度可调：黑一点数据更清楚，透明一点预览更清楚（默认偏黑，编谱时以数据为主）
    p.rect_filled(
        rect,
        2.0,
        egui::Color32::from_rgba_unmultiplied(9, 10, 15, (cfg.body_alpha.clamp(0.0, 1.0) * 255.0) as u8),
    );

    let mid_x = rect.center().x;
    // 中间是**纵轴轴带**（拍号写在里面），两半各让出一半宽度 —— 这样两区被轴分开
    let axis = egui::Rect::from_min_max(
        egui::pos2(mid_x - AXIS_W * 0.5, body.min.y),
        egui::pos2(mid_x + AXIS_W * 0.5, body.max.y),
    );
    let note_pane = egui::Rect::from_min_max(body.min, egui::pos2(axis.min.x, body.max.y));
    let ev_pane = egui::Rect::from_min_max(egui::pos2(axis.max.x, body.min.y), body.max);
    // 两半各自压一点底色，轴带再暗一档（轴是"骨架"，不该抢内容）
    let pane_a = (cfg.body_alpha.clamp(0.0, 1.0) * 46.0) as u8;
    p.rect_filled(
        note_pane,
        0.0,
        egui::Color32::from_rgba_unmultiplied(20, 24, 34, pane_a),
    );
    p.rect_filled(
        ev_pane,
        0.0,
        egui::Color32::from_rgba_unmultiplied(26, 22, 32, pane_a),
    );
    p.rect_filled(
        axis,
        0.0,
        egui::Color32::from_rgba_unmultiplied(10, 11, 16, (cfg.body_alpha.clamp(0.0, 1.0) * 150.0) as u8),
    );
    // 轴带两侧各一条竖线：分栏线就是轴本身
    for x in [axis.min.x, axis.max.x] {
        p.line_segment(
            [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
            egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(150, 160, 200, 110)),
        );
    }

    // ---- 拍网格 + 拍号标尺（网格线数量 = 每拍细分 div）----
    // 三档线宽：每 4 拍最粗（带拍号）、每拍中等、1/div 细线。
    //
    // **画多少线 = 吸附到哪**：两边都用 `st.effective_beat_div()`（设定值在当前缩放下的可画细分）。
    // 曾经这里是"按像素密度偷偷少画细线"，而吸附仍用设定值 —— 于是用户看到两个毛病：
    // ①把每拍条数从 4 调到 8/16 画面毫无变化（细线被静默抽稀掉了）；
    // ②吸附落在没画出来的线上（"吸附不遵循规范"）。抽稀本身没错，错在两套规则不一致。
    // 现在只有一个数，抽稀只在这里发生一次，且设定值被顶掉时顶栏会写明"实际 1/N"。
    let div = st.effective_beat_div();
    for line in beat_grid_lines(anchor, beats, div) {
        let y = y_of(line.beat);
        if y < body.min.y - 1.0 || y > body.max.y + 1.0 {
            continue;
        }
        // 档位配色（用户要求：**一拍为基准**、线要画明显）
        // 基准 = 每拍线（最亮最粗的正线）；4 拍小节只比它略强一点，用来定位；
        // 细分线是"细节"，细而暗 —— 结构读起来必须是"一拍一格"，不是"四拍一格"。
        let (w, col) = if line.is_bar {
            (2.2, egui::Color32::from_rgba_unmultiplied(182, 192, 228, 215))
        } else if line.is_beat {
            (1.5, egui::Color32::from_rgba_unmultiplied(152, 162, 214, 170))
        } else {
            // **最细的一档也要看得见**（用户要求）：但必须仍明显弱于基准线，
            // 否则"一拍为基准"又糊成一整片。α 阶梯 110 < 170 < 215，线宽 0.9 < 1.5 < 2.2。
            (0.9, egui::Color32::from_rgba_unmultiplied(118, 128, 174, 110))
        };
        // 网格线只画在两半里：轴带留白，视觉上才是"被轴分开的两个区"
        for pane in [note_pane, ev_pane] {
            p.line_segment(
                [egui::pos2(pane.min.x, y), egui::pos2(pane.max.x, y)],
                egui::Stroke::new(w, col),
            );
        }
    }

    // ---- 轴带里每一拍补一个小刻度 ----
    // 标注只落在"放得下字"的位置（可能每 2/16 拍一个），但**节奏基元是一拍** ——
    // 所以轴带里按每一拍补短刻度，让一拍有一个看得见的落点（标注位置的刻度更长更亮，画在后面压住它）。
    if div >= 1 {
        for line in beat_grid_lines(anchor, beats, div).iter().filter(|l| l.is_beat) {
            let y = y_of(line.beat);
            if y < body.min.y - 1.0 || y > body.max.y + 1.0 {
                continue;
            }
            for x in [axis.min.x, axis.max.x] {
                p.line_segment(
                    [egui::pos2(x, y), egui::pos2(x + if x < mid_x { 3.5 } else { -3.5 }, y)],
                    egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(140, 150, 195, 130)),
                );
            }
        }
    }

    // ---- 纵轴数字标注（密度/精度随缩放变化，与网格密度无关）----
    for t in axis_ticks(anchor, beats, body.height()) {
        let y = y_of(t.beat);
        if y < body.min.y - 1.0 || y > body.max.y + 1.0 {
            continue;
        }
        let (fsize, col, tick) = if t.major {
            (9.5, egui::Color32::from_rgb(190, 200, 235), 7.0)
        } else {
            (8.5, egui::Color32::from_rgb(150, 160, 205), 4.5)
        };
        // 数字居中写在轴带里，两侧各一个小刻度线指向相邻半区
        p.text(
            egui::pos2(mid_x, y - 1.0),
            egui::Align2::CENTER_BOTTOM,
            &t.text,
            egui::FontId::monospace(fsize),
            col,
        );
        for x in [axis.min.x, axis.max.x] {
            p.line_segment(
                [egui::pos2(x, y), egui::pos2(x + if x < mid_x { tick } else { -tick }, y)],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(150, 160, 205)),
            );
        }
    }
    // 标题靠右放：左上角是 RPE 窗口标注的地盘，两边都放会叠字
    p.text(
        egui::pos2(rect.max.x - 4.0, rect.min.y + 1.0),
        egui::Align2::RIGHT_TOP,
        // 简短：右半的列名就在同一行，长标题会跟列名叠字（其余信息在工具栏/检查器里）
        format!(
            "编辑区 [{} 拍] · 基准 1 拍 · 网格 1/{} · Ctrl+滚轮 缩放 · H 隐藏",
            beats as i64,
            div
        ),
        egui::FontId::monospace(9.5),
        egui::Color32::from_rgb(165, 175, 215),
    );

    // 播放头线（拍位置）
    let y_now = y_of(beat_now);
    if y_now >= body.min.y && y_now <= body.max.y {
        p.line_segment(
            [egui::pos2(body.min.x, y_now), egui::pos2(body.max.x, y_now)],
            egui::Stroke::new(1.4, egui::Color32::from_rgb(245, 235, 120)),
        );
    }

    let Some(line) = st.selected() else {
        return;
    };

    // ---- 左半：音符轨道区 ----
    // 窗口 X 偏移（视图状态）：把显示的 laneX 区间平移，就能看/编辑窗口外的音符
    let off = st.window_offset_x;
    let x_of_lane = |lane: f32| lane_to_pane_x(note_pane, lane, off);
    let lane_of_x = |x: f32| pane_x_to_lane(note_pane, x, off);
    // 坐标方向的网格：**整个窗口平均切割**（1350/N，**边界对齐**，吸附到哪就画到哪）。
    //
    // 旧算法是"以 0 为中心"的 `k·(1350/N)`，奇数等分时最外那条格线会跑到 ±675 之外
    // （5 等分时 676 最近的是 810）—— 于是只能靠"禁止奇数等分"回避。用户要求改成整窗等分：
    // 端点恒为格线，任何 N 都放得下，不再有"奇数放不进去"这回事。
    // 格点锚在**官方坐标系**上（`-675 + k·step`），所以平移之后格线仍然对齐；
    // 覆盖范围取当前显示窗口（偏移 0 时正好是 0..=lane_div）。
    let (k0, k1) = st.grid.lane_index_range(off);
    let fine_lane = egui::Color32::from_rgba_unmultiplied(118, 128, 174, 110);
    for k in k0..=k1 {
        let x = x_of_lane(st.grid.lane_of_index(k));
        p.line_segment(
            [egui::pos2(x, note_pane.min.y), egui::pos2(x, note_pane.max.y)],
            egui::Stroke::new(0.9, fine_lane),
        );
    }
    // **中轴持续存在**（laneX = 0）：偶数等分时它就是第 N/2 条格线，这里是同一条；
    // 奇数等分时它不是格点（吸附不落在它上面），但仍然要画 —— 它是"窗口正中"的视觉参考。
    for (lane, w, col) in [
        (0.0_f32, 1.3_f32, egui::Color32::from_rgba_unmultiplied(136, 146, 192, 160)), // 中轴
        (-675.0, 1.7, egui::Color32::from_rgba_unmultiplied(255, 190, 110, 190)),      // 官方窗口边界
        (675.0, 1.7, egui::Color32::from_rgba_unmultiplied(255, 190, 110, 190)),
    ] {
        // 只画落在**显示窗口**内的（平移出去就不画）
        if lane < off - 675.0 - 0.5 || lane > off + 675.0 + 0.5 {
            continue;
        }
        let x = x_of_lane(lane);
        p.line_segment(
            [egui::pos2(x, note_pane.min.y), egui::pos2(x, note_pane.max.y)],
            egui::Stroke::new(w, col),
        );
    }
    let row_h = 7.0_f32;
    for (i, n) in line.notes.iter().enumerate() {
        let y0 = y_of(n.time_beat(&st.chart.tmap));
        let y1 = y_of(n.end_beat(&st.chart.tmap));
        if y0 < body.min.y - 20.0 && y1 < body.min.y - 20.0 {
            continue; // 完全在窗口下方
        }
        if y0 > body.max.y + 20.0 && y1 > body.max.y + 20.0 {
            continue; // 完全在窗口上方
        }
        let x = x_of_lane(n.lane_x);
        let c = n.kind.color();
        let col = egui::Color32::from_rgb(
            (c[0] * 255.0) as u8,
            (c[1] * 255.0) as u8,
            (c[2] * 255.0) as u8,
        );
        // Hold 用竖条表示时长，其余用方块
        let (r, fill) = if (n.end - n.time).abs() > 1e-6 {
            (
                egui::Rect::from_min_max(
                    egui::pos2(x - 5.0, y0.min(y1)),
                    egui::pos2(x + 5.0, y0.max(y1)),
                ),
                egui::Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 110),
            )
        } else {
            (
                egui::Rect::from_min_max(
                    egui::pos2(x - 5.0, y0 - row_h * 0.5),
                    egui::pos2(x + 5.0, y0 + row_h * 0.5),
                ),
                col,
            )
        };
        // 裁剪到音符区：平移之后，窗口外的音符会落到面板之外 —— 直接画就会糊到轴带/事件区上
        let r = r.intersect(note_pane);
        if !r.is_positive() {
            continue;
        }
        let selected = Some(i) == st.selected_note;
        p.rect_filled(r, 1.0, fill);
        if selected {
            p.rect_stroke(
                r.expand(1.0),
                1.0,
                egui::Stroke::new(1.2, egui::Color32::WHITE),
                egui::StrokeKind::Outside,
            );
        }
        // 假音符用空心标记（它不是"真"音符，别和真音符一样实心）
        if n.is_fake {
            p.rect_stroke(
                r.expand(0.5),
                1.0,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(240, 240, 240)),
                egui::StrokeKind::Inside,
            );
        }
    }

    // ---- 右半：事件区（5 条轨道各一列）----
    let lanes = TrackId::ALL.len();
    let col_w = ev_pane.width() / lanes as f32;
    // 本帧画过的事件块：(事件下标, 矩形, 起点 y, 终点 y) —— 供把手高亮复用同一决策
    let mut blocks: Vec<(usize, egui::Rect, f32, f32)> = Vec::new();
    for (k, id) in TrackId::ALL.iter().enumerate() {
        let x0 = ev_pane.min.x + k as f32 * col_w;
        let col = TRACK_COLORS[k];
        let is_sel = *id == st.selected_track;
        // 列背景 + 分隔线 + 列名
        p.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x0 + 1.0, ev_pane.min.y),
                egui::pos2(x0 + col_w - 1.0, ev_pane.max.y),
            ),
            0.0,
            egui::Color32::from_rgba_unmultiplied(col[0], col[1], col[2], if is_sel { 20 } else { 8 }),
        );
        p.text(
            egui::pos2(x0 + col_w * 0.5, rect.min.y + RULER_H - 1.0),
            egui::Align2::CENTER_BOTTOM,
            id.key(),
            egui::FontId::monospace(9.0),
            egui::Color32::from_rgb(col[0], col[1], col[2]),
        );
        let tr = line.track(*id);
        for (i, e) in tr.events.iter().enumerate() {
            let y0 = y_of(st.chart.tmap.beat(st.chart.tmap.sec(e.start.to_f64())));
            let y1 = y_of(st.chart.tmap.beat(st.chart.tmap.sec(e.end.to_f64())));
            if y0.max(y1) < body.min.y - 20.0 || y0.min(y1) > body.max.y + 20.0 {
                continue;
            }
            // 块只占列宽的 55%（居中）：恒定事件会跨满整个可见窗口，
            // 若铺满整列就看不出"这是一条事件"还是"这是列底色"
            let inset = col_w * 0.225;
            let r = egui::Rect::from_min_max(
                egui::pos2(x0 + inset, y0.min(y1)),
                egui::pos2(x0 + col_w - inset, y0.max(y1).max(y0.min(y1) + 3.0)),
            );
            let selected_ev = is_sel && Some(i) == st.selected_event;
            // **整块渐变：起点（下方）→ 终点（上方）走色相**（透明度两端相同）。
            // 早先用的是"暗端乘 0.55 + 降 alpha"，那在深色预览上会糊；色相偏移更清楚，
            // 也不牺牲可见度。用 Mesh 两个顶点色，比"画很多细条"干净，也不随块高变化。
            {
                let (bottom, top) =
                    gradient_colors(egui::Color32::from_rgb(col[0], col[1], col[2]), selected_ev);
                let mut mesh = egui::Mesh::default();
                let base = mesh.vertices.len() as u32;
                let uv = egui::epaint::WHITE_UV;
                mesh.vertices.push(egui::epaint::Vertex { pos: r.left_bottom(), uv, color: bottom });
                mesh.vertices.push(egui::epaint::Vertex { pos: r.right_bottom(), uv, color: bottom });
                mesh.vertices.push(egui::epaint::Vertex { pos: r.right_top(), uv, color: top });
                mesh.vertices.push(egui::epaint::Vertex { pos: r.left_top(), uv, color: top });
                mesh.indices
                    .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
                p.add(egui::Shape::mesh(mesh));
            }
            if selected_ev {
                p.rect_stroke(
                    r.expand(1.0),
                    1.0,
                    egui::Stroke::new(1.2, egui::Color32::WHITE),
                    egui::StrokeKind::Outside,
                );
            }
            // 块里写起止值：恒定事件会跨满整个可见窗口，没有数值就只能看出"有一条事件"。
            // 高度不够时不硬塞（宁可少写，也别糊成一团）。
            let h = r.height();
            let txt = egui::Color32::from_rgb(235, 240, 255);
            if h >= 16.0 {
                p.text(
                    egui::pos2(r.center().x, r.min.y + 1.0),
                    egui::Align2::CENTER_TOP,
                    fmt_val(&e.start_value),
                    egui::FontId::monospace(9.0),
                    txt,
                );
                p.text(
                    egui::pos2(r.center().x, r.max.y - 1.0),
                    egui::Align2::CENTER_BOTTOM,
                    fmt_val(&e.end_value),
                    egui::FontId::monospace(9.0),
                    txt,
                );
            }
            if h >= 34.0 && e.easing != "linear" {
                p.text(
                    egui::pos2(r.center().x, r.center().y),
                    egui::Align2::CENTER_CENTER,
                    &e.easing,
                    egui::FontId::monospace(8.5),
                    egui::Color32::from_rgb(200, 210, 235),
                );
            }
            // 把手高亮**不在这里画**：头尾相接时两个块各有一条边落在同一条 y 上，
            // 按块各自判定会让两个把手同时亮（看起来像"一次选中了两个"）。
            // 统一放到交互阶段、用**同一个决策**（prefer_edge 的结果）画，见下面的 `blocks`。
            blocks.push((i, r, y0, y1));
        }
    }

    // ---- 交互：命中 → 光标 → 拖拽/点选 ----
    //
    // 这里踩过一个坑：拖拽处理最初被写在 `else if resp.clicked()` 里，而拖拽时 `clicked()` 为假
    // ⇒ 整段代码根本执行不到（表现为"有提示但拖不动"）。现在按"先算命中、再分派事件"来组织，
    // 拖拽与点选不再互相嵌套。
    let resp = ui.interact(
        rect,
        egui::Id::new("opm_overlay"),
        egui::Sense::click_and_drag() | egui::Sense::hover(),
    );
    // 悬停位置用于**光标形状**（`interact_pointer_pos()` 只在按下时才有值，拿它做悬停提示是取不到的）
    let hover_pos = resp.hover_pos();
    let ptr = resp.interact_pointer_pos().or(hover_pos);
    let over_ruler = |pos: egui::Pos2| pos.y < rect.min.y + RULER_H;
    let in_axis = |pos: egui::Pos2| axis.min.x <= pos.x && pos.x <= axis.max.x;

    // 命中：先算清楚"指针下面是什么"，后面所有分支只看这个结果
    #[derive(Clone, Copy)]
    enum Hit {
        Ruler(f64),
        Axis,
        Note(usize),
        EventEdge(usize, EventEdge),
        EventBlock(usize),
        Pane,
    }
    let hit = ptr.map(|pos| {
        if over_ruler(pos) {
            return Hit::Ruler(beat_of(pos.y.max(rect.min.y + RULER_H)));
        }
        if in_axis(pos) {
            return Hit::Axis;
        }
        if pos.x < mid_x {
            // 音符区：取最近的音符（窗口内通常只有几十个）
            let mut best: Option<(usize, f32)> = None;
            for (i, n) in line.notes.iter().enumerate() {
                let y = y_of(n.time_beat(&st.chart.tmap));
                let d = (x_of_lane(n.lane_x) - pos.x).abs().max((y - pos.y).abs());
                if d < 10.0 && best.map(|(_, bd)| d < bd).unwrap_or(true) {
                    best = Some((i, d));
                }
            }
            return best.map(|(i, _)| Hit::Note(i)).unwrap_or(Hit::Pane);
        }
        // 事件区：命中列 → 命中块（优先头/尾把手）
        let k = (((pos.x - ev_pane.min.x) / col_w).floor() as usize).min(lanes - 1);
        let id = TrackId::ALL[k];
        let tr = line.track(id);
        // 收集**所有**候选，再按规则挑 —— 头尾相接时会有两个事件同时命中同一条 y
        let mut near: Vec<(usize, EventEdge, f32)> = Vec::new();
        let mut block_hit: Option<(usize, f32)> = None;
        for (i, e) in tr.events.iter().enumerate() {
            let y0 = y_of(st.chart.tmap.beat(st.chart.tmap.sec(e.start.to_f64())));
            let y1 = y_of(st.chart.tmap.beat(st.chart.tmap.sec(e.end.to_f64())));
            // y0 = 起点的屏幕 y（在下）、y1 = 终点的屏幕 y（在上）
            match hit_event_part(pos.y, y0, y1, EDGE_BAND) {
                EventPart::Start => near.push((i, EventEdge::Start, (pos.y - y0).abs())),
                EventPart::End => near.push((i, EventEdge::End, (pos.y - y1).abs())),
                EventPart::Body => {
                    if block_hit.map(|(_, bd)| 0.0 < bd).unwrap_or(true) {
                        block_hit = Some((i, 0.0));
                    }
                }
                EventPart::None => {}
            }
        }
        // 只保留最近的一批（同一条边界上会有一对），再按"选中优先 → 否则选尾巴"决定
        if let Some(best) = near.iter().map(|(_, _, d)| *d).fold(None, |m: Option<f32>, d| {
            Some(m.map(|x| x.min(d)).unwrap_or(d))
        }) {
            let cands: Vec<(usize, EventEdge)> = near
                .iter()
                .filter(|(_, _, d)| (*d - best).abs() < 0.75) // 同一 y 上的都算相接
                .map(|(i, e, _)| (*i, *e))
                .collect();
            if let Some((i, edge)) = prefer_edge(&cands, st.selected_event) {
                return Hit::EventEdge(i, edge);
            }
        }
        block_hit
            .map(|(i, _)| Hit::EventBlock(i))
            .unwrap_or(Hit::Pane)
    });

    // 记录"指针下面是什么"，供**拖拽开始**时使用（见 press_hit_set 的注释）。
    //
    // 三个条件都是踩出来的：
    // · 已经进入拖拽 ⇒ 不再更新（否则会把"按下时抓的是哪一头"覆盖掉）；
    // · `drag_started/dragged` 的那一帧也不更新 —— 那一帧指针已经移开把手段了，
    //   而拖拽分支就在本帧稍后运行，必须让它看到**按下时**的命中；
    // · egui 的第一帧没有交互状态（hover 还是 None），所以"只在未按下时记录"会漏掉按下那一帧。
    if !resp.drag_started()
        && !resp.dragged()
        && drag_edge_get(ui).is_none()
        && drag_note_get(ui).is_none()
    {
        let code = match &hit {
            Some(Hit::Note(i)) => Some((0u8, *i, 0u8)),
            Some(Hit::EventEdge(i, EventEdge::Start)) => Some((1u8, *i, 0u8)),
            Some(Hit::EventEdge(i, EventEdge::End)) => Some((2u8, *i, 0u8)),
            _ => None,
        };
        press_hit_set(ui, code);
    }

    // ---- 把手高亮：**只亮一个**（与拖拽同一个决策，见 handle_to_highlight）----
    let hover_edge = match &hit {
        Some(Hit::EventEdge(i, edge)) => Some((*i, *edge)),
        _ => None,
    };
    let highlight = handle_to_highlight(drag_edge_get(ui), hover_edge);
    if let Some((i, edge)) = highlight {
        if let Some((_, r, _, _)) = blocks.iter().find(|(bi, _, _, _)| *bi == i) {
            let p = ui.painter_at(rect);
            // 与待放置的 hold 用**同一个** `TimeHandle`（判定/绘制同源）
            TimeHandle::from_span(r.min.x, r.max.x, r.bottom(), r.top())
                .paint(&p, edge, egui::Color32::WHITE);
        }
    }

    // 光标形状：事件头/尾 = **双头箭头**（ResizeVertical，上下双向）；音符 = 抓手
    match &hit {
        Some(Hit::EventEdge(_, _)) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical),
        Some(Hit::Note(_)) => {
            let dragging = resp.dragged();
            ui.ctx().set_cursor_icon(if dragging {
                egui::CursorIcon::Grabbing
            } else {
                egui::CursorIcon::Grab
            });
        }
        Some(Hit::Ruler(_)) => ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand),
        _ => {}
    }

    // ---- 快速放置（Q/W/E/R）与 hold 跟随 ----
    //
    // 位置解析（指针 → 吸附后的 laneX/拍）只有这里懂，所以"按键 → 放什么"也放在这里：
    // 调用方只负责施加动作。
    //
    // **约束（用户要求）：hold 跟随状态下，编辑区的滚动与缩放必须照常。**
    // 具体做法：这一段既不读也不消费滚轮/缩放事件（它们在下面独立处理），
    // 跟随只认"指针**移动**"这一个信号 —— 于是滚动/缩放时长度不变、功能不受影响。
    let pointer_in_notes = |pos: egui::Pos2| pos.x < mid_x && !in_axis(pos) && !over_ruler(pos);
    if keys_enabled && st.pending_hold.is_none() {
        for key in [egui::Key::Q, egui::Key::W, egui::Key::E, egui::Key::R] {
            if !key_pressed_once(ui, key) {
                continue;
            }
            let Some(kind) = opm_app::keymap::quick_place_kind(key) else { continue };
            match ptr.filter(|q| pointer_in_notes(*q)) {
                Some(pos) => actions.push(OverlayAction::QuickPlace {
                    kind,
                    lane_x: st.snap_lane(lane_of_x(pos.x)),
                    beat: st.snap_beat(beat_of(pos.y)).max(0.0),
                }),
                // 指针不在音符区：**说清楚为什么没反应**（静默失败最让人困惑）
                None => actions.push(OverlayAction::Notice(
                    "快速放置需要把指针放在音符区里（左边那半），再按 Q/W/E/R".to_owned(),
                )),
            }
        }
    }
    if let Some(h) = st.pending_hold {
        // 放下 / 取消：只有这一处判键，免得和别处抢。
        // `keys_enabled` 同样管着它们 —— 在控制台打字时按回车不该把 hold 放下。
        let (commit, cancel) = if keys_enabled {
            (
                key_pressed_once(ui, egui::Key::R) || key_pressed_once(ui, egui::Key::Enter),
                key_pressed_once(ui, egui::Key::Escape),
            )
        } else {
            (false, false)
        };
        if commit {
            actions.push(OverlayAction::PendingHoldCommit);
        } else if cancel {
            actions.push(OverlayAction::PendingHoldCancel);
        }
        // 草稿 + **与事件块同一套控制杆**
        let x = x_of_lane(h.lane_x);
        let (y0, y1) = (y_of(h.start_beat), y_of(h.end_beat));
        let draft = egui::Rect::from_min_max(
            egui::pos2(x - 5.0, y0.min(y1)),
            egui::pos2(x + 5.0, y0.max(y1)),
        )
        .intersect(note_pane);
        if draft.is_positive() {
            p.rect_filled(draft, 1.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 90));
            p.rect_stroke(
                draft.expand(1.0),
                1.0,
                egui::Stroke::new(1.2, egui::Color32::from_rgb(255, 235, 160)),
                egui::StrokeKind::Outside,
            );
        }
        let handle = TimeHandle::from_span(x - 5.0, x + 5.0, y0, y1);
        let hot = ptr.map(|q| handle.hit(q));
        let edge_color = |hit: bool| {
            if hit {
                egui::Color32::WHITE
            } else {
                egui::Color32::from_rgb(255, 235, 160)
            }
        };
        handle.paint(&p, EventEdge::Start, edge_color(hot == Some(EventPart::Start)));
        handle.paint(&p, EventEdge::End, edge_color(hot == Some(EventPart::End)));
        if resp.drag_started() || resp.dragged() {
            match hot {
                Some(EventPart::Start) | Some(EventPart::End) => {
                    let edge = if hot == Some(EventPart::Start) {
                        EventEdge::Start
                    } else {
                        EventEdge::End
                    };
                    if let Some(q) = ptr {
                        actions.push(OverlayAction::PendingHoldResize {
                            edge,
                            beat: st.snap_beat(beat_of(q.y)).max(0.0),
                        });
                    }
                }
                // 拖在别处 = "长条跟着鼠标走"（长度随移动）
                _ => {
                    if let Some(q) = ptr {
                        actions.push(OverlayAction::PendingHoldFollow {
                            beat: st.snap_beat(beat_of(q.y)).max(0.0),
                        });
                    }
                }
            }
        } else if let Some(q) = ptr.filter(|q| pointer_in_notes(*q)) {
            // 只有**真的移动**才改长度：滚动/缩放时指针没动，长度与视口都保持原样
            let moved = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
            if moved {
                actions.push(OverlayAction::PendingHoldFollow {
                    beat: st.snap_beat(beat_of(q.y)).max(0.0),
                });
            }
        }
    }

    // 滚轮：在编辑区里滚动就是**改谱面当前时间**（向上滚 = 往后）。跟随播放头的窗口会把这一变化
    // 直接体现出来，所以"滚动"与"移动播放头"在这里是同一件事（只此一处，别重复写第二份）。
    if resp.hovered() {
        // `zoom_delta` 里含 Ctrl+滚轮与触控板捏合；按住 Ctrl 时 `smooth_scroll_delta` 恒为 0
        let (dy, zoom) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
        if (zoom - 1.0).abs() > 1e-4 {
            // Ctrl+滚轮 = **缩放时间轴**（标注精度随之变化，见 `axis_ticks`）
            actions.push(OverlayAction::ZoomBeats(zoom_delta_to_beats_factor(zoom)));
        } else if dy.abs() > 0.01 {
            actions.push(OverlayAction::ScrollBeats(scroll_delta_to_beats(
                dy,
                beats,
                cfg.scroll_beats_per_notch,
            )));
        }
    }

    // 拖拽与点选：互不嵌套
    // 拖拽：分三段处理，且**位置更新在"开始的那一帧"也要发**。
    //
    // 这一条是踩出来的：egui 把"刚判定为拖拽"的那一帧标记为 `drag_started`（`dragged` 同时为真），
    // 早先写成 `if drag_started {..} else if dragged {..}` ⇒ 那一帧被跳过，于是
    // "按下→小幅移动→松开"这类**快速拖拽一次位置更新都发不出去**（看上去就是拖不动）。
    if resp.drag_started() {
        // 用**按下时**的命中（不是当前命中）：拖拽阈值会让指针先移开那 6px 的把手段
        match press_hit_get(ui) {
            Some((1, i, _)) | Some((2, i, _)) => {
                let edge = if matches!(press_hit_get(ui), Some((1, ..))) {
                    EventEdge::Start
                } else {
                    EventEdge::End
                };
                let k = (((ptr.map(|p| p.x).unwrap_or(ev_pane.min.x) - ev_pane.min.x) / col_w)
                    .floor() as usize)
                    .min(lanes - 1);
                actions.push(OverlayAction::SelectTrack(TrackId::ALL[k]));
                actions.push(OverlayAction::SelectEvent(i));
                actions.push(OverlayAction::EventResizeStart);
                drag_edge_set(ui, Some((i, edge)));
            }
            Some((0, i, _)) => {
                actions.push(OverlayAction::SelectNote(i));
                actions.push(OverlayAction::NoteDragStart);
                drag_note_set(ui, Some(i));
            }
            _ => {}
        }
    }
    if resp.drag_started() || resp.dragged() {
        if let Some((i, edge)) = drag_edge_get(ui) {
            if let Some(pos) = ptr {
                // 头/尾按**拍网格**吸附（它调的就是时间）
                actions.push(OverlayAction::EventResize {
                    index: i,
                    edge,
                    beat: st.snap_beat(beat_of(pos.y)).max(0.0),
                });
            }
        } else if let Some(i) = drag_note_get(ui) {
            if let (Some(pos), Some(n)) = (ptr, line.notes.get(i)) {
                let lane = lane_of_x(pos.x); // 逆映射带上窗口偏移（否则平移后"拖到哪 = 吸到哪"会错位）
                // 音符**总是**落在两轴网格的交叉点（横向 laneX、纵向拍都吸附）
                actions.push(OverlayAction::NoteDrag {
                    index: i,
                    doc_index: n.doc_index,
                    lane_x: st.snap_lane(lane),
                    beat: st.snap_beat(beat_of(pos.y)).max(0.0),
                });
            }
        }
    } else if resp.drag_stopped() {
        press_hit_set(ui, None);
        if drag_edge_get(ui).is_some() {
            actions.push(OverlayAction::EventResizeEnd);
            drag_edge_set(ui, None);
        }
        if drag_note_get(ui).is_some() {
            actions.push(OverlayAction::NoteDragEnd);
            drag_note_set(ui, None);
        }
    } else if resp.double_clicked() {
        if let Some(Hit::Pane) = hit {
            if let Some(pos) = ptr {
                if pos.x < mid_x && !in_axis(pos) && !over_ruler(pos) {
                    let lane = lane_of_x(pos.x); // 逆映射带上窗口偏移（否则平移后"拖到哪 = 吸到哪"会错位）
                    actions.push(OverlayAction::PlaceNote {
                        lane_x: st.snap_lane(lane),
                        beat: st.snap_beat(beat_of(pos.y)).max(0.0),
                    });
                }
            }
        }
    } else if resp.clicked() {
        match hit {
            Some(Hit::Ruler(b)) => actions.push(OverlayAction::SeekBeat(b)),
            Some(Hit::Note(i)) => actions.push(OverlayAction::SelectNote(i)),
            Some(Hit::EventEdge(i, _)) | Some(Hit::EventBlock(i)) => {
                if let Some(pos) = ptr {
                    let k = (((pos.x - ev_pane.min.x) / col_w).floor() as usize).min(lanes - 1);
                    actions.push(OverlayAction::SelectTrack(TrackId::ALL[k]));
                }
                actions.push(OverlayAction::SelectEvent(i));
            }
            _ => {}
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use opm_app::doc::{Beat, BpmEntry, Document, Event, JudgeLine};
    use opm_app::state::{chart_from_doc, EditorState, TrackId};
    use serde_json::json;

    fn state_with_events() -> EditorState {
        let mut doc = Document::default();
        doc.bpm_list = vec![BpmEntry {
            start: Beat::zero(),
            bpm: 180.0,
            foreign: Default::default(),
        }];
        doc.judge_lines.clear();
        let mut l = JudgeLine::default();
        l.name = "L0".into();
        for (from, to) in [(0.0, 100.0), (100.0, 100.0)] {
            l.layers[0].track_mut("alpha").unwrap().push(Event::new(
                Beat::new(if from == 0.0 { 0 } else { 16 }, 1),
                Beat::new(if from == 0.0 { 16 } else { 32 }, 1),
                json!(from),
                json!(to),
                "linear",
            ));
        }
        doc.judge_lines.push(l);
        let mut st = EditorState::new(chart_from_doc(&doc));
        st.selected_track = TrackId::Alpha;
        st.selected_event = Some(0);
        st
    }

    /// 头/尾命中带（"光标对准头或尾"的规则）
    #[test]
    fn edge_hit_bands() {
        // 纵轴是拍且越上越晚 ⇒ 起点在下（y=200）、终点在上（y=100）
        let (start_y, end_y) = (200.0_f32, 100.0_f32);
        assert_eq!(hit_event_part(200.0, start_y, end_y, EDGE_BAND), EventPart::Start);
        assert_eq!(hit_event_part(195.0, start_y, end_y, EDGE_BAND), EventPart::Start);
        assert_eq!(hit_event_part(100.0, start_y, end_y, EDGE_BAND), EventPart::End);
        assert_eq!(hit_event_part(104.5, start_y, end_y, EDGE_BAND), EventPart::End);
        assert_eq!(hit_event_part(150.0, start_y, end_y, EDGE_BAND), EventPart::Body);
        assert_eq!(hit_event_part(220.0, start_y, end_y, EDGE_BAND), EventPart::None);
        assert_eq!(hit_event_part(70.0, start_y, end_y, EDGE_BAND), EventPart::None);
        assert_eq!(hit_event_part(193.0, start_y, end_y, EDGE_BAND), EventPart::Body);
        // 很矮的块：两头抢同一段 ⇒ 取更近的那头，保证小事件也抓得到
        // 起点在下（308）、终点在上（300）：301 靠近终点 ⇒ End；307 靠近起点 ⇒ Start
        assert_eq!(hit_event_part(301.0, 308.0, 300.0, EDGE_BAND), EventPart::End);
        assert_eq!(hit_event_part(307.0, 308.0, 300.0, EDGE_BAND), EventPart::Start);
    }

    /// 光标停在**头尾相接**的那条边界上时，只能命中一个事件块（不能两个一起亮/一起选）。
    ///
    /// 用无头 egui 真的把指针放上去：先记录基线，再在共享边界上按下，
    /// 断言这一帧只对**一个**事件产生 SelectEvent / EventResizeStart。
    #[test]
    fn hover_on_shared_boundary_hits_single_event() {
        let mut st = state_with_events(); // alpha 轨道：事件 0 = [0,16)、事件 1 = [16,32)（头尾相接）
        st.selected_event = None;
        st.selected_track = TrackId::Alpha;
        // **显式钉住缩放**，不要吃默认值：默认可见拍数是个视图偏好（8 拍，用户要求拉长 4 倍之后），
        // 它一变，"beat 16 在屏幕外"就会让这条测试莫名其妙地红 —— 之前就是这么红的。
        st.overlay_beats = 32.0;
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        let body = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.min.y + RULER_H), rect.max);
        let anchor = st.chart.tmap.beat(st.playhead) - cfg.lead_beats;
        let axis_max = rect.center().x + AXIS_W * 0.5;
        let ev_w = (rect.max.x - axis_max) / 5.0;
        let x = axis_max + ev_w * 3.5; // alpha 列
        let y_boundary = beat_y(body, anchor, st.overlay_beats, 16.0); // 共享边界 = 事件 0 的尾 = 事件 1 的头

        let mut all: Vec<String> = Vec::new();
        let pass = |events: Vec<egui::Event>, all: &mut Vec<String>| {
            let mut acts: Vec<OverlayAction> = Vec::new();
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                draw(ui, &st, rect, &cfg, false, &mut acts);
            });
            out.textures_delta.clear();
            all.extend(acts.iter().map(|a| format!("{a:?}")));
        };
        pass(vec![egui::Event::PointerMoved(egui::pos2(x, y_boundary))], &mut all);
        pass(
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y_boundary),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            }],
            &mut all,
        );
        // 移动一点点：egui 这时才认定是拖拽；命中用"按下时"的位置（共享边界）
        pass(
            vec![egui::Event::PointerMoved(egui::pos2(x, y_boundary + 8.0))],
            &mut all,
        );
        let sel: Vec<&String> = all.iter().filter(|a| a.starts_with("SelectEvent(")).collect();
        assert_eq!(sel.len(), 1, "共享边界上只能选中一个事件块；实际 {all:?}");
        assert_eq!(sel[0], "SelectEvent(0)", "未选中时应抓尾巴=事件 0 的 End");
        let starts = all.iter().filter(|a| *a == "EventResizeStart").count();
        assert_eq!(starts, 1, "只应开始一次拖拽；实际 {all:?}");
        assert!(
            all.iter().any(|a| a.contains("edge: End")),
            "未选中时应抓尾巴（End）；实际 {all:?}"
        );
        // 反例：把事件 1 设为选中，同样的位置应改为抓**事件 1 的头**
        let mut all2: Vec<String> = Vec::new();
        let mut st2 = state_with_events();
        st2.selected_event = Some(1);
        st2.overlay_beats = st.overlay_beats; // 缩放也要一致（坐标是用上面的 y_boundary 算的）
        // **换一个 Context**：egui 的临时内存（拖拽状态）按 Id 存在 Context 里，
        // 复用同一个 Context 会让第一段序列的拖拽状态延续到第二段（测试里踩过）
        let ctx2 = egui::Context::default();
        let pass2 = |events: Vec<egui::Event>, out: &mut Vec<String>| {
            let mut acts: Vec<OverlayAction> = Vec::new();
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            let mut o = ctx2.run_ui(raw, |ui| {
                draw(ui, &st2, rect, &cfg, false, &mut acts);
            });
            o.textures_delta.clear();
            out.extend(acts.iter().map(|a| format!("{a:?}")));
        };
        pass2(vec![egui::Event::PointerMoved(egui::pos2(x, y_boundary))], &mut all2);
        pass2(
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y_boundary),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            }],
            &mut all2,
        );
        pass2(
            vec![egui::Event::PointerMoved(egui::pos2(x, y_boundary + 8.0))],
            &mut all2,
        );
        let sel2: Vec<&String> = all2.iter().filter(|a| a.starts_with("SelectEvent(")).collect();
        assert_eq!(sel2.len(), 1, "仍然只能选中一个；实际 {all2:?}");
        assert_eq!(sel2[0], "SelectEvent(1)", "选中了事件 1 ⇒ 应抓它的头");
        assert!(
            all2.iter().any(|a| a.contains("edge: Start")),
            "选中事件 1 时应抓它的头（Start）；实际 {all2:?}"
        );
    }

    /// **网格线必须与吸附目标重合**（用户报的"吸附后不落在横轴上"就是这个坏了）。
    ///
    /// 曾经的 bug：档位判定用了自增后的下标 ⇒ 带拍号的粗线落在 0.75/1.75/… 而非整拍，
    /// 于是吸附到整拍的东西看起来"没吸附上"。
    #[test]
    fn beat_grid_lines_coincide_with_snap_targets() {
        use opm_app::state::GridCfg;
        let g = GridCfg { beat_div: 4, lane_div: 16 };
        let lines = beat_grid_lines(-2.0, 32.0, g.beat_div);

        // 1) 每条线都必须是**吸附的定点**：对线所在拍再吸附一次不应移动
        for l in &lines {
            assert!(
                (g.snap_beat(l.beat) - l.beat).abs() < 1e-12,
                "线 {} 不是吸附目标（吸附 → {}）",
                l.beat,
                g.snap_beat(l.beat)
            );
        }
        // 2) 小节线落在 4 拍整数倍上；轴带标注由**另一套**（像素密度）规则给出，
        //    见 axis_labels_adapt_precision_to_zoom —— 网格密度与标注精度故意分开
        for l in lines.iter().filter(|l| l.is_bar) {
            let b = l.beat;
            assert!((b / 4.0 - (b / 4.0).round()).abs() < 1e-9, "小节线 {b} 不是 4 的倍数");
        }
        // 3) 每个整拍都必须有一条线（且 is_beat=true，永不被抽稀跳过）
        for beat in [-2i64, -1, 0, 1, 7, 8, 29, 30] {
            let found = lines
                .iter()
                .find(|l| (l.beat - beat as f64).abs() < 1e-9)
                .unwrap_or_else(|| panic!("整拍 {beat} 没有网格线"));
            assert!(found.is_beat, "整拍 {beat} 应标记为整拍线");
        }
        // 4) 换细分（可指定）：1/3 拍网格下 1/3、2/3 也都是线，且仍是吸附定点
        let g3 = GridCfg { beat_div: 3, lane_div: 4 };
        let lines3 = beat_grid_lines(0.0, 4.0, 3);
        for want in [0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0] {
            let f = lines3.iter().find(|l| (l.beat - want).abs() < 1e-9);
            assert!(f.is_some(), "1/3 网格缺少 {want}");
            assert!((g3.snap_beat(want) - want).abs() < 1e-9);
        }
    }

    /// **改网格数量必须看得见**（用户报的 bug）：画出来的细分与吸附步长必须是同一个数。
    ///
    /// 旧实现里画线按像素密度偷偷抽稀、吸附却用设定值，于是 4→8→16 画面不变，
    /// 而吸附还会落在没画出来的线上。这条测试把"一个数说了算"钉死。
    #[test]
    fn drawn_grid_equals_snap_grid_and_responds_to_setting() {
        use opm_app::state::{EditorState, GridCfg};

        // 默认缩放 32 拍可见：4 与 8 都画得出来，且**画出来就比 4 密一倍**
        let mut st = EditorState::new(chart_from_doc(&opm_app::doc::Document::default()));
        st.overlay_beats = 32.0;
        st.grid = GridCfg { beat_div: 4, lane_div: 16 };
        let n4 = beat_grid_lines(0.0, st.overlay_beats, st.effective_beat_div()).len();
        st.grid.beat_div = 8;
        let n8 = beat_grid_lines(0.0, st.overlay_beats, st.effective_beat_div()).len();
        assert_eq!(st.effective_beat_div(), 8, "8 条在默认缩放下应当画得出来");
        assert!(n8 > n4, "4→8 后线数应变多：{n4} → {n8}");

        // 每条画出来的线都是吸附定点，每个吸附结果都是画出来的线（双向，不允许单向包含）
        let lines = beat_grid_lines(0.0, st.overlay_beats, st.effective_beat_div());
        for l in &lines {
            assert!((st.snap_beat(l.beat) - l.beat).abs() < 1e-12, "线 {} 不是吸附定点", l.beat);
        }
        for k in 0..=(st.overlay_beats as i64 * 7) {
            let raw = k as f64 / 7.0; // 故意用 1/7 这种非格点拍去吸
            let snapped = st.snap_beat(raw);
            assert!(
                lines.iter().any(|l| (l.beat - snapped).abs() < 1e-9),
                "{raw} 吸附到 {snapped}，但那里没有画出来的线"
            );
        }
        // 写回文档的有理数分母 = 实际生效细分（否则存盘位置与看得见的网格又会错开）
        let [_, den] = st.beat_json(1.0 / 8.0);
        assert_eq!(den, st.effective_beat_div() as i64);

        // 密到画不出来时：降级到**设定值的约数**（保持 1/4、1/8 这类整齐关系），且降级后仍是吸附步长
        st.overlay_beats = 256.0;
        st.grid.beat_div = 32;
        let eff = st.effective_beat_div();
        assert!(eff < 32 && 32 % eff == 0, "抽稀应取约数，得到 1/{eff}");
        assert_eq!(st.beat_json(0.5)[1], eff as i64, "存盘分母应跟随实际细分");
        // 放大 → 更细的网格重新可用（这是抽稀合理的证明：不是永久禁用）
        st.overlay_beats = 8.0;
        assert_eq!(st.effective_beat_div(), 32, "缩小可见拍数后 1/32 应重新画得出");
    }

    /// **窗口 X 偏移**：把音符区显示的 laneX 区间平移，从而编辑**官方窗口之外**的音符。
    ///
    /// 这条测试盯三件事：①正/逆映射互逆；②偏移 0 时和以前完全一样；③偏移拉满（+675）时，
    /// 音符区右半边的落点已经越出官方窗口 ±675，而且**仍然吸附在格点上**（格点锚在官方坐标系上、向两侧延伸）。
    #[test]
    fn window_offset_allows_editing_beyond_official_window() {
        use opm_app::state::{EditorState, GridCfg};
        let pane = egui::Rect::from_min_max(egui::pos2(300.0, 100.0), egui::pos2(833.0, 700.0));

        // ① 互逆（含偏移）
        for off in [0.0_f32, 400.0, -400.0, 675.0] {
            for lane in [-675.0_f32, -200.0, 0.0, 333.0, 675.0] {
                let x = lane_to_pane_x(pane, lane, off);
                let back = pane_x_to_lane(pane, x, off);
                assert!((back - lane).abs() < 1e-3, "off={off} lane={lane} 逆映射回到 {back}");
            }
        }
        // ② 偏移 0 时与旧公式一致：laneX=±675 正好是音符区两端，0 在正中
        assert!((lane_to_pane_x(pane, -675.0, 0.0) - pane.min.x).abs() < 1e-3);
        assert!((lane_to_pane_x(pane, 675.0, 0.0) - pane.max.x).abs() < 1e-3);
        assert!((lane_to_pane_x(pane, 0.0, 0.0) - pane.center().x).abs() < 1e-3);

        // ③ 偏移拉满 + 拖到音符区右侧 ⇒ 可以编辑越界坐标，且仍落在格点上
        let mut st = EditorState::new(chart_from_doc(&opm_app::doc::Document::default()));
        st.grid = GridCfg { beat_div: 4, lane_div: 16 };
        st.set_window_offset_x(675.0);
        let (lo, hi) = st.window_lane_range();
        assert!((lo + 0.0).abs() < 1e-3 && (hi - 1350.0).abs() < 1e-3, "显示区间应为 0…1350");
        let x = pane.min.x + pane.width() * 0.9; // 音符区靠右
        let lane = pane_x_to_lane(pane, x, st.window_offset_x);
        assert!(lane > 675.0, "90% 处应已越出官方窗口，实际 {lane}");
        let snapped = st.snap_lane(lane);
        assert!(snapped > 675.0, "偏移下吸附不该被拉回窗口内：{snapped}");
        assert!(snapped <= 1350.0 + 1e-3);
        // 吸附结果仍是格点（格点锚在官方坐标系、向两侧延伸）
        let step = st.grid.h_step_rpe();
        let k = (snapped + 675.0) / step;
        assert!((k - k.round()).abs() < 1e-3, "{snapped} 不在格点上（k={k}）");
        // 偏移归零后，同样的指针位置落回窗口内的格点
        st.set_window_offset_x(0.0);
        assert!(st.snap_lane(pane_x_to_lane(pane, x, 0.0)).abs() <= 675.0 + 1e-3);

        // 夹取与坏输入
        st.set_window_offset_x(9999.0);
        assert_eq!(st.window_offset_x, EditorState::WINDOW_OFFSET_MAX);
        st.set_window_offset_x(f32::NAN);
        assert_eq!(st.window_offset_x, 0.0);
    }

    /// **标注精度随缩放变化**（用户要求"时间轴数字标注应对应产生不同精度"）。
    #[test]
    fn axis_labels_adapt_precision_to_zoom() {
        let pane = 600.0f32;
        // 放大到 4 拍可见：每拍 150px ⇒ 能标到 1/4 拍，两位小数
        let fine = axis_label_step(4.0, pane);
        assert_eq!(fine, 0.25, "4 拍可见时步长应为 1/4 拍");
        assert_eq!(axis_label_decimals(fine), 2);
        let t = axis_ticks(0.0, 4.0, pane);
        let texts: Vec<&str> = t.iter().map(|x| x.text.as_str()).collect();
        assert!(texts.contains(&"0.25") && texts.contains(&"1.50"), "{texts:?}");
        assert_eq!(t.len(), 17, "0..=4 拍每 1/4 一条 = 17 条：{texts:?}");

        // 32 拍可见（不是默认值，只是这一档的样例）：每拍 18.75px ⇒ 只放得下每 2 拍一个整数
        let mid = axis_label_step(32.0, pane);
        assert_eq!(mid, 2.0);
        assert_eq!(axis_label_decimals(mid), 0);
        assert!(axis_ticks(0.0, 32.0, pane).iter().all(|x| !x.text.contains('.')));

        // 缩到 256 拍可见：每拍 2.3px ⇒ 每 16 拍才标一个，否则糊成黑带
        let coarse = axis_label_step(256.0, pane);
        assert_eq!(coarse, 16.0);
        assert!(coarse * (pane as f64 / 256.0) >= AXIS_LABEL_MIN_PX as f64 - 1e-9);

        // 步长只走 4·2^k 阶梯：永远落在"整拍/半拍/小节"这种能口算的位置
        for beats in [4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0] {
            let st = axis_label_step(beats, pane);
            let log2 = st.log2();
            assert!((log2 - log2.round()).abs() < 1e-9, "{beats} 拍可见 → 步长 {st} 不是 2 的幂");
        }
        // 缩得越小（看得越细）标注只能更密，不能反着来
        let mut prev = 0.0;
        for beats in [4.0, 16.0, 64.0, 256.0] {
            let st = axis_label_step(beats, pane);
            assert!(st >= prev, "缩放越粗标注步长不应变小：{beats} 拍 → {st}");
            prev = st;
        }
    }

    /// **Ctrl+滚轮缩放**这条交互要真的走一遍指针/滚轮事件：不按 Ctrl 是移时间轴，按住才是缩放。
    /// 本机没法往 Wayland 窗口注入滚轮，所以用无头 egui 合成 `MouseWheel`。
    #[test]
    fn ctrl_wheel_zooms_while_plain_wheel_seeks() {
        let st = state_with_events();
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        let center = rect.center();

        // 滚轮事件要**多帧**才会从 egui 的平滑器里放出来（见 egui WheelState::after_events），
        // 所以每轮喂几帧，别只喂一帧就说"没反应"。
        let run = |mods: egui::Modifiers, all: &mut Vec<String>| {
            all.push(format!("--- mods={mods:?}"));
            for _ in 0..8 {
                let events = vec![
                    egui::Event::PointerMoved(center),
                    egui::Event::ModifiersChanged(mods),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, 50.0),
                        phase: egui::TouchPhase::Move,
                        modifiers: mods,
                    },
                ];
                let mut acts: Vec<OverlayAction> = Vec::new();
                let raw = egui::RawInput {
                    screen_rect: Some(rect),
                    events,
                    ..Default::default()
                };
                let mut out = ctx.run_ui(raw, |ui| {
                    draw(ui, &st, rect, &cfg, false, &mut acts);
                });
                out.textures_delta.clear();
                all.extend(acts.iter().map(|a| format!("{a:?}")));
            }
        };

        let mut all: Vec<String> = Vec::new();
        run(egui::Modifiers::CTRL, &mut all);
        let zooms: Vec<&String> = all.iter().filter(|a| a.starts_with("ZoomBeats(")).collect();
        let seeks: Vec<&String> = all.iter().filter(|a| a.starts_with("ScrollBeats(")).collect();
        assert!(!zooms.is_empty(), "Ctrl+滚轮应产出缩放：{all:?}");
        assert!(seeks.is_empty(), "Ctrl+滚轮**不应**同时移动时间轴：{all:?}");
        // 向上滚 = 放大 = 可见拍数变少 ⇒ 倍率 < 1（egui 的 zoom_delta > 1）
        let f: f64 = zooms.last().unwrap().trim_start_matches("ZoomBeats(").trim_end_matches(')').parse().unwrap();
        assert!(f < 1.0 && f > 0.0, "向上滚应放大（倍率 <1），实际 {f}");

        let mut plain: Vec<String> = Vec::new();
        run(egui::Modifiers::NONE, &mut plain);
        assert!(
            !plain.iter().any(|a| a.starts_with("ZoomBeats(")),
            "不按 Ctrl 不该缩放：{plain:?}"
        );
        assert!(
            plain.iter().any(|a| a.starts_with("ScrollBeats(")),
            "不按 Ctrl 时滚轮应移动时间轴：{plain:?}"
        );

        // 换算本身的约定：zoom_delta>1（放大）⇒ 可见拍数倍率 <1；互为倒数；坏输入不放大也不缩小
        assert!((zoom_delta_to_beats_factor(1.25) - 0.8).abs() < 1e-12);
        // 容差按 f32 的精度给：0.8 在 f32 里本就不精确，别用 1e-12 假装没有这件事
        assert!((zoom_delta_to_beats_factor(1.25) * zoom_delta_to_beats_factor(0.8) - 1.0).abs() < 1e-6);
        assert_eq!(zoom_delta_to_beats_factor(0.0), 1.0);
        assert_eq!(zoom_delta_to_beats_factor(f32::NAN), 1.0);
        let mut s2 = state_with_events();
        for _ in 0..100 {
            s2.zoom_by(0.5); // 一直放大
        }
        assert_eq!(s2.overlay_beats, EditorState::ZOOM_MIN_BEATS);
        for _ in 0..100 {
            s2.zoom_by(2.0);
        }
        assert_eq!(s2.overlay_beats, EditorState::ZOOM_MAX_BEATS);
        s2.zoom_by(f64::NAN); // 坏输入不该把缩放搞成 NaN
        assert_eq!(s2.overlay_beats, EditorState::ZOOM_MAX_BEATS);
    }

    /// 时间控制杆：命中判定与绘制用同一份几何（x 不在范围内一律不命中）
    #[test]
    fn time_handle_hit_covers_both_ends_and_the_range() {
        // 起点在下（y=300）、终点在上（y=100），横向 195..205
        let h = TimeHandle::from_span(195.0, 205.0, 300.0, 100.0);
        assert_eq!(h.hit(egui::pos2(200.0, 300.0)), EventPart::Start, "起点（下方）");
        assert_eq!(h.hit(egui::pos2(200.0, 297.0)), EventPart::Start, "判定带内");
        assert_eq!(h.hit(egui::pos2(200.0, 100.0)), EventPart::End, "终点（上方）");
        assert_eq!(h.hit(egui::pos2(200.0, 200.0)), EventPart::Body, "中间是块体");
        assert_eq!(h.hit(egui::pos2(200.0, 400.0)), EventPart::None, "外面不命中");
        // 横向范围外：即使 y 正对着把手也不算命中（否则整行都会"抓得到"）
        assert_eq!(h.hit(egui::pos2(150.0, 300.0)), EventPart::None);
        assert_eq!(h.hit(egui::pos2(250.0, 100.0)), EventPart::None);
    }

    /// 快速放置：指针在音符区里按 Q/W/E/R ⇒ 在**吸附过的位置**放对应种类的音符；
    /// 指针不在音符区 ⇒ 明确告知（而不是静默什么都没发生）
    #[test]
    fn quick_place_keys_emit_place_actions_at_the_snapped_pointer() {
        use opm_app::doc::NoteKind;
        let st = state_with_events();
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();

        // 音符区在左半边；轴带在正中，标尺在顶部 ⇒ 取左下角附近，稳落在音符区里
        let inside = egui::pos2(180.0, 360.0);
        let outside = egui::pos2(700.0, 360.0); // 事件区
        let press = |pos: egui::Pos2, key: egui::Key| -> Vec<String> {
            let mut all: Vec<String> = Vec::new();
            // 第 0 帧：指针就位 + **补一个"松开"**。
            // 为什么要有这个松开：同一个 `Context` 里键盘是**有状态**的，上一轮没发过 release
            // ⇒ egui 认为键还按着，下一次 `pressed: true` 会被标成 `repeat: true`
            //（真实使用中总会先松开，所以这是测试自己造出来的假象）。
            for frame in 0..2 {
                let mut events = vec![egui::Event::PointerMoved(pos)];
                if frame == 0 {
                    events.push(egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: false,
                        repeat: false,
                        modifiers: Default::default(),
                    });
                } else {
                    events.push(egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: Default::default(),
                    });
                }
                let mut acts: Vec<OverlayAction> = Vec::new();
                let raw = egui::RawInput {
                    screen_rect: Some(rect),
                    events,
                    ..Default::default()
                };
                let mut out = ctx.run_ui(raw, |ui| {
                    draw(ui, &st, rect, &cfg, true, &mut acts);
                });
                out.textures_delta.clear();
                all.extend(acts.iter().map(|a| format!("{a:?}")));
            }
            all
        };

        // Q → tap（位置是**吸附后**的：lane 与拍都落在格点上）
        let q = press(inside, egui::Key::Q);
        let placed = q
            .iter()
            .find(|a| a.starts_with("QuickPlace"))
            .unwrap_or_else(|| panic!("按 Q 应该产出 QuickPlace：{q:?}"));
        assert!(placed.contains("Tap"), "{placed}");
        let lane: f32 = placed
            .split("lane_x: ")
            .nth(1)
            .and_then(|t| t.split([',', '}']).next())
            .and_then(|t| t.trim().parse().ok())
            .expect("lane_x");
        assert!(
            (st.snap_lane(lane) - lane).abs() < 1e-3,
            "放下位置必须是吸附过的：{lane}"
        );
        let beat: f64 = placed
            .split("beat: ")
            .nth(1)
            .and_then(|t| t.split(['}', ',']).next())
            .and_then(|t| t.trim().parse().ok())
            .expect("beat");
        assert!((st.snap_beat(beat) - beat).abs() < 1e-9, "拍也要吸附：{beat}");
        assert!(beat >= 0.0);

        // W/E/R 分别是 flick/drag/hold（按 Debug 名比对：`as_str()` 是小写，动作里是枚举名）
        for (key, want) in [
            (egui::Key::W, NoteKind::Flick),
            (egui::Key::E, NoteKind::Drag),
            (egui::Key::R, NoteKind::Hold),
        ] {
            let got = press(inside, key);
            let act = got.iter().find(|a| a.starts_with("QuickPlace")).expect("QuickPlace");
            assert!(
                act.contains(&format!("{want:?}")),
                "{key:?} 应放 {want:?}，实际 {act}"
            );
        }

        // 指针不在音符区：给一句话，而不是什么都不做
        let out = press(outside, egui::Key::Q);
        assert!(
            out.iter().any(|a| a.starts_with("Notice")),
            "指针不在音符区时应给出说明：{out:?}"
        );
        assert!(!out.iter().any(|a| a.starts_with("QuickPlace")));
    }

    /// 按住不放（自动重复）不该"放一个又放下一个"：重复事件一律不算新的按键
    #[test]
    fn key_auto_repeat_does_not_restart_or_commit() {
        let st = state_with_events();
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        let inside = egui::pos2(180.0, 360.0);
        let mut all: Vec<String> = Vec::new();
        // 第 0 帧只把指针放过去（egui 第一帧还没有悬停状态），第 1 帧才是"真的按下"
        for frame in 0..5 {
            let mut events = vec![egui::Event::PointerMoved(inside)];
            if frame >= 1 {
                events.push(egui::Event::Key {
                    key: egui::Key::R,
                    physical_key: None,
                    pressed: true,
                    // 第 1 帧是真按下，之后都是自动重复
                    repeat: frame > 1,
                    modifiers: Default::default(),
                });
            }
            let mut acts: Vec<OverlayAction> = Vec::new();
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                draw(ui, &st, rect, &cfg, true, &mut acts);
            });
            out.textures_delta.clear();
            all.extend(acts.iter().map(|a| format!("{a:?}")));
        }
        let places: Vec<&String> = all.iter().filter(|a| a.starts_with("QuickPlace")).collect();
        assert_eq!(places.len(), 1, "自动重复不该反复开始放置：{all:?}");
        assert!(
            !all.iter().any(|a| a.starts_with("PendingHoldCommit")),
            "自动重复不该顺手把 hold 放下：{all:?}"
        );
    }

    /// **hold 跟随状态下，滚动与缩放必须照常**（用户明确要求）：
    /// 滚轮照旧产出 ScrollBeats / Ctrl+滚轮照旧 ZoomBeats，而**长度不受滚动影响**
    /// （跟随只认"指针移动"，滚动时指针没动）。
    #[test]
    fn scrolling_and_zooming_still_work_while_a_hold_follows() {
        let mut st = state_with_events();
        st.begin_pending_hold(0.0, 4.0);
        let before = st.pending_hold.unwrap();
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        let center = egui::pos2(180.0, 360.0);

        let run = |mods: egui::Modifiers, all: &mut Vec<String>| {
            for _ in 0..8 {
                let events = vec![
                    // 指针**不动**（同一个位置）＋滚轮
                    egui::Event::PointerMoved(center),
                    egui::Event::ModifiersChanged(mods),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, 50.0),
                        phase: egui::TouchPhase::Move,
                        modifiers: mods,
                    },
                ];
                let mut acts: Vec<OverlayAction> = Vec::new();
                let raw = egui::RawInput {
                    screen_rect: Some(rect),
                    events,
                    ..Default::default()
                };
                let mut out = ctx.run_ui(raw, |ui| {
                    draw(ui, &st, rect, &cfg, true, &mut acts);
                });
                out.textures_delta.clear();
                all.extend(acts.iter().map(|a| format!("{a:?}")));
            }
        };

        let mut plain: Vec<String> = Vec::new();
        run(egui::Modifiers::NONE, &mut plain);
        assert!(
            plain.iter().any(|a| a.starts_with("ScrollBeats(")),
            "跟随状态下普通滚轮仍应移动时间轴：{plain:?}"
        );
        assert!(
            !plain.iter().any(|a| a.starts_with("PendingHoldFollow")),
            "滚动不是鼠标移动 ⇒ 不该改 hold 长度：{plain:?}"
        );

        let mut ctrl: Vec<String> = Vec::new();
        run(egui::Modifiers::CTRL, &mut ctrl);
        assert!(
            ctrl.iter().any(|a| a.starts_with("ZoomBeats(")),
            "跟随状态下 Ctrl+滚轮仍应缩放：{ctrl:?}"
        );
        assert!(!ctrl.iter().any(|a| a.starts_with("PendingHoldFollow")));
        // 长度一点没变（跟随只认指针移动）
        assert_eq!(st.pending_hold.unwrap(), before);
    }

    /// hold 跟随：指针移动改长度、控制杆改起止、R/回车放下、Esc 取消
    #[test]
    fn pending_hold_follows_the_pointer_and_commits_or_cancels() {
        let mut st = state_with_events();
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();

        let pass = |st: &EditorState,
                    events: Vec<egui::Event>,
                    keys: bool,
                    ctx: &egui::Context|
         -> Vec<String> {
            let mut acts: Vec<OverlayAction> = Vec::new();
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                draw(ui, st, rect, &cfg, keys, &mut acts);
            });
            out.textures_delta.clear();
            acts.iter().map(|a| format!("{a:?}")).collect()
        };

        let key = |k: egui::Key| {
            vec![egui::Event::Key {
                key: k,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }]
        };

        // 先给指针一个位置（第一帧只是记录）
        let a = egui::pos2(180.0, 380.0);
        let b = egui::pos2(180.0, 200.0); // 往上 ⇒ 拍更大 ⇒ 更长
        st.begin_pending_hold(0.0, 1.0);
        pass(&st, vec![egui::Event::PointerMoved(a)], true, &ctx);
        let acts = pass(
            &st,
            vec![egui::Event::PointerMoved(b), egui::Event::PointerMoved(b)],
            true,
            &ctx,
        );
        assert!(
            acts.iter().any(|x| x.starts_with("PendingHoldFollow")),
            "指针移动应产出跟随：{acts:?}"
        );
        assert!(!acts.iter().any(|x| x.starts_with("PendingHoldCommit")));

        // 放下：R 与回车都要行
        for k in [egui::Key::R, egui::Key::Enter] {
            let acts = pass(&st, key(k), true, &ctx);
            assert!(
                acts.iter().any(|x| x.starts_with("PendingHoldCommit")),
                "{k:?} 应该放下 hold：{acts:?}"
            );
        }
        // 取消：Esc
        let acts = pass(&st, key(egui::Key::Escape), true, &ctx);
        assert!(acts.iter().any(|x| x.starts_with("PendingHoldCancel")), "{acts:?}");

        // **打字/模态期间（keys=false）按键不算数**：不该把 hold 放下或取消
        for k in [egui::Key::R, egui::Key::Enter, egui::Key::Escape] {
            let acts = pass(&st, key(k), false, &ctx);
            assert!(
                !acts.iter().any(|x| x.contains("PendingHoldC")),
                "keys=false 时不该响应 {k:?}：{acts:?}"
            );
        }
    }

    /// 待放置 hold 的**几何**：画出来的高度必须等于"拍数 → 像素"的换算
    /// （草稿与真音符用同一套 `y_of`，高度对不上就说明画错了地方）
    #[test]
    fn pending_hold_draft_height_matches_the_beat_mapping() {
        let mut st = state_with_events();
        st.pending_hold = None;
        st.begin_pending_hold(0.0, 4.0);
        st.pending_hold.as_mut().unwrap().end_beat = 6.0; // 2 拍
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(rect),
            ..Default::default()
        };
        let mut acts: Vec<OverlayAction> = Vec::new();
        let mut out = ctx.run_ui(raw, |ui| {
            draw(ui, &st, rect, &cfg, false, &mut acts);
        });
        out.textures_delta.clear();
        // 找草稿矩形：**按宽度**认它（音符宽 10px；描边那圈是 12）—— 取最接近 10 的那个（填充体）
        let mut best: Option<(f32, f32)> = None;
        for cs in &out.shapes {
            if let egui::epaint::Shape::Rect(r) = &cs.shape {
                let (w, h) = (r.rect.width(), r.rect.height());
                if (9.0..=13.0).contains(&w) && h > 4.0 {
                    let d = (w - 10.0).abs();
                    if best.map(|(bd, _)| d < bd).unwrap_or(true) {
                        best = Some((d, h));
                    }
                }
            }
        }
        let (_d, h) = best.expect("应该画出草稿矩形");
        // 期望高度：可见拍数（默认 8）× 2 拍 / 8 拍 × 有效高度
        let body_h = rect.height() - RULER_H;
        let want = body_h * 2.0 / st.overlay_beats as f32;
        assert!(
            (h - want).abs() < 2.0,
            "草稿高度 {h} 与换算 {want} 不符（可见 {} 拍，体高 {body_h}）",
            st.overlay_beats
        );
    }

    /// 渐变必须是**色相偏移**：两端 alpha 相同，只有色相不同（用户明确要求，别退回透明度渐变）
    #[test]
    fn event_gradient_uses_hue_shift_not_alpha() {
        for base in [
            egui::Color32::from_rgb(120, 190, 255), // moveX 蓝
            egui::Color32::from_rgb(255, 200, 120), // rotate 橙
            egui::Color32::from_rgb(220, 160, 240), // alpha 紫
        ] {
            for selected in [false, true] {
                let (a, b) = gradient_colors(base, selected);
                assert_eq!(
                    a.a(),
                    b.a(),
                    "两端透明度必须相同（渐变不能用透明度做，base={base:?}）"
                );
                // 口径是 sRGB 空间的色相：偏移量应等于常量本身（8 位量化留 1° 容差）
                let dh = (hue_deg(b) - hue_deg(a)).rem_euclid(360.0);
                assert!(
                    (dh - GRAD_HUE_SHIFT_DEG).abs() < 1.5,
                    "色相应偏移 {}°，实际 {dh}°（base={base:?}）",
                    GRAD_HUE_SHIFT_DEG
                );
                // 明度（= max 通道）不应变：渐变不是靠变暗做的
                let lum = |c: egui::Color32| c.r().max(c.g()).max(c.b()) as i32;
                assert!(
                    (lum(a) - lum(b)).abs() <= 2,
                    "明度不该变：{} vs {}",
                    lum(a),
                    lum(b)
                );
                // 选中态只是整体更不透明，仍然两端一致
                if selected {
                    assert!(a.a() > 200, "选中应更实一些：{a:?}");
                }
            }
        }
    }

    /// 头尾相接时的**高亮**：只亮一个把手（与拖拽同一决策）。
    ///
    /// 这条断言针对的就是"拖动头尾相接的事件时两个把手同时亮"这个 bug：
    /// 候选有两个（A 的尾、B 的头），但决策必须只有一个。
    #[test]
    fn boundary_highlights_exactly_one_handle() {
        let cands = [(0usize, EventEdge::End), (1usize, EventEdge::Start)];
        // 都没选中 ⇒ 选尾巴（事件 0 的 End）；这正是高亮应当亮的唯一一个
        let chosen = prefer_edge(&cands, None).unwrap();
        assert_eq!(chosen, (0, EventEdge::End));
        let hl = handle_to_highlight(None, Some(chosen));
        assert_eq!(hl, Some((0, EventEdge::End)), "只应亮一个把手");
        // 拖拽中：亮抓住的那一头（即使指针已经移开把手段）
        let hl2 = handle_to_highlight(Some((1, EventEdge::Start)), Some(chosen));
        assert_eq!(hl2, Some((1, EventEdge::Start)), "拖拽中亮抓住的那一头");
        // 什么都没命中 ⇒ 不亮
        assert_eq!(handle_to_highlight(None, None), None);
    }

    /// 头尾相接时抓谁：优先选中，都没选中选尾巴（下方 = 头/Start，上方 = 尾/End）
    #[test]
    fn prefer_edge_rule() {
        let both = [(0usize, EventEdge::End), (1, EventEdge::Start)];
        assert_eq!(prefer_edge(&both, None), Some((0, EventEdge::End)), "都没选中 ⇒ 选尾巴");
        assert_eq!(prefer_edge(&both, Some(1)), Some((1, EventEdge::Start)));
        assert_eq!(prefer_edge(&both, Some(0)), Some((0, EventEdge::End)));
        assert_eq!(
            prefer_edge(&[(2, EventEdge::Start)], Some(9)),
            Some((2, EventEdge::Start)),
            "选中项不在候选里 ⇒ 退回默认规则"
        );
        assert_eq!(prefer_edge(&[], None), None);
    }

    /// **无头 egui 合成事件**：按在事件块的头上 → 移动 → 松开，必须产出拖拽动作序列。
    ///
    /// 这条测试针对的正是"有提示但拖不动"那个 bug：当时判定算得出来，可拖拽分支被嵌在
    /// `resp.clicked()` 里面，而拖拽时 `clicked()` 为假 ⇒ 永远进不去。纯函数单测抓不到这种
    /// **结构**错误，只有真的走一遍指针事件序列才行（本机没法注入 Wayland 指针，但 egui 本身
    /// 可以无头跑，把 RawInput 喂进去即可）。
    #[test]
    fn drag_on_event_head_emits_resize_actions() {
        let mut st = state_with_events();
        // 同上：测试自己定缩放（绘制与测试的坐标换算必须用**同一个**可见拍数，
        // 否则指针落在测试算出来的位置、面板却按另一个缩放画，测试会红得莫名其妙）
        st.overlay_beats = 32.0;
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        let body = egui::Rect::from_min_max(
            egui::pos2(rect.min.x, rect.min.y + RULER_H),
            rect.max,
        );
        let beats = st.overlay_beats;
        let anchor = st.chart.tmap.beat(st.playhead) - cfg.lead_beats;
        // 事件 0 的**头**（beat 0）在屏幕上的位置；x 落在 alpha 列（第 4 列，共 5 列）
        let axis_min = rect.center().x - AXIS_W * 0.5;
        let axis_max = rect.center().x + AXIS_W * 0.5;
        let ev_w = (rect.max.x - axis_max) / 5.0;
        let x = axis_max + ev_w * 3.5; // alpha 是第 4 列（索引 3）
        let y_head = beat_y(body, anchor, beats, 0.0);
        assert!(y_head > body.min.y && y_head <= body.max.y, "测试点应在窗口内: {y_head}");
        let _ = axis_min;

        let mut all: Vec<String> = Vec::new();
        // 每帧把 egui 要求的光标也收下来：光标图标没法截图（自截屏只有应用自己的帧缓冲，
        // 系统光标是合成器画的），但 egui 会把它写在 platform_output 里 —— 一样能断言。
        let mut cursors: Vec<egui::CursorIcon> = Vec::new();
        let mut pass = |events: Vec<egui::Event>, all: &mut Vec<String>| {
            let mut acts: Vec<OverlayAction> = Vec::new();
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            // 0.36 的无头入口：`run_ui` 直接给根 Ui（不需要建窗口）。
            // 字体图集的 deltas 必须清掉，否则 epaint 在析构时会 panic（无头跑没有渲染器来消费它们）。
            let mut out = ctx.run_ui(raw, |ui| {
                draw(ui, &st, rect, &cfg, false, &mut acts);
            });
            cursors.push(out.platform_output.cursor_icon);
            out.textures_delta.clear();
            for a in &acts {
                all.push(match a {
                    OverlayAction::EventResizeStart => "EventResizeStart".to_owned(),
                    OverlayAction::EventResize { edge, beat, .. } => {
                        format!("EventResize({edge:?},{beat:.3})")
                    }
                    OverlayAction::EventResizeEnd => "EventResizeEnd".to_owned(),
                    OverlayAction::NoteDragStart => "NoteDragStart".to_owned(),
                    OverlayAction::SelectEvent(i) => format!("SelectEvent({i})"),
                    OverlayAction::SelectTrack(t) => format!("SelectTrack({})", t.key()),
                    OverlayAction::SelectNote(i) => format!("SelectNote({i})"),
                    other => format!("{other:?}"),
                });
            }
        };

        // 帧 1：悬停到头（应给出光标提示，不产出拖拽动作）
        pass(
            vec![egui::Event::PointerMoved(egui::pos2(x, y_head))],
            &mut all,
        );
        // 帧 2：按下
        pass(
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y_head),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            }],
            &mut all,
        );
        // 帧 3：往"更早"的方向拖 20 像素（时间变小）
        pass(
            vec![egui::Event::PointerMoved(egui::pos2(x, y_head + 20.0))],
            &mut all,
        );
        // 帧 4：松开
        pass(
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y_head + 20.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            }],
            &mut all,
        );

        assert!(
            all.iter().any(|a| a == "EventResizeStart"),
            "按下事件块的头之后必须开始拖拽；实际动作：{all:?}"
        );
        assert!(
            all.iter().any(|a| a.starts_with("EventResize(Start")),
            "拖动必须产出 EventResize(Start, …)；实际：{all:?}"
        );
        assert!(
            all.iter().any(|a| a == "EventResizeEnd"),
            "松开必须结束拖拽（调用方据此 commit）；实际：{all:?}"
        );
        assert!(
            !all.iter().any(|a| a.starts_with("NoteDrag")),
            "事件区的拖拽不该被当成拖音符；实际：{all:?}"
        );
        // 悬停在头/尾上时必须请求**双头箭头**（ResizeVertical）
        assert!(
            cursors.contains(&egui::CursorIcon::ResizeVertical),
            "指针压到头/尾时应设为双头箭头；实际光标序列：{cursors:?}"
        );
    }
}
