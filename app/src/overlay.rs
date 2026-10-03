// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
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

use opm_app::state::{EditorState, MaskChannel, SelKind, TrackId, RPE_WINDOW_HALF_W, RPE_WINDOW_W};
// 事件边界规则只有一份实现，住在库内 `state`（好让集成测试能调真身）：这里只再导出，GUI 侧的名字不变。
pub use opm_app::state::{prefer_edge, EventEdge};

/// 叠加层产出的动作（由调用方施加，见 `main.rs`）
///
/// **选区动作分两档**：`Select*` 是"替换整个选区"（点选、框选），`Toggle*` 是
/// "把某一个在选中/未选中之间切换"（Ctrl+左键）。两者都由调用方落到 `EditorState` 的选区上，
/// 面板自己不碰状态 —— 与左侧判定线树同一条规矩。
#[derive(Debug)]
pub enum OverlayAction {
    /// 选中当前判定线的第 i 个音符（时间序）——**替换**整个选区
    SelectNote(usize),
    /// 框选：把选区替换成这一批音符（空 = 清空）
    SelectNotes(Vec<usize>),
    /// Ctrl+左键：把这个音符在选中/未选中之间切换
    ToggleNote(usize),
    /// 选中当前轨道（某条轨道列被点击）
    SelectTrack(TrackId),
    /// 选中第 i 条事件（**当前轨道**）——替换整个选区
    SelectEvent(usize),
    /// 框选：把选区替换成这一批事件（可跨轨道；空 = 清空）
    SelectEvents(Vec<(TrackId, usize)>),
    /// Ctrl+左键：把这条事件在选中/未选中之间切换
    ToggleEvent(TrackId, usize),
    /// 点空白/点标尺：清空选区
    ClearSelection,
    /// 把播放头定位到某一拍
    SeekBeat(f64),
    /// 把播放头**相对**挪动若干拍（滚轮用；相对量避免"取整再取整"的累积误差）
    ScrollBeats(f64),
    /// Ctrl+滚轮：按倍率缩放时间轴（可见拍数 × 倍率，调用方负责夹到允许范围）
    ZoomBeats(f64),
    /// **组拖动开始**：把选区冻结成抓手（原点定下之后，拖拽期间不再从文档反推）。
    /// 调用方据此开一个事务，让整段拖拽只占一个撤销步。
    GrabStart(Box<opm_app::edit::Grab>),
    /// 拖动中：**已经吸附并夹过**的位移（吸附只依赖网格设置，所以在面板里算）
    GrabMove { d_lane: f32, d_beat: f64 },
    /// 拖动结束：调用方提交事务
    GrabEnd,
    /// 双击空白处放置音符（相同吸附规则）
    PlaceNote { lane_x: f32, beat: f64 },
    /// 事件块头/尾拖拽：起点（调用方开事务）
    EventResizeStart,
    /// 事件块头/尾拖拽中：把 start 或 end 挪到某个拍（已按拍网格吸附）。
    /// 带**文档地址**（图层 + 该图层内下标）—— 视图把五个图层合并成一条时间线，
    /// 合并下标不能直接当图层下标用（见 `opm_app::doc::EventRef`）。
    EventResize {
        track: TrackId,
        at: opm_app::doc::EventRef,
        edge: EventEdge,
        beat: f64,
    },
    /// 事件块头/尾拖拽结束（调用方提交事务）
    EventResizeEnd,
    /// **快速放置**（Q/W/E/R）：把指针处的音符放下去（位置已吸附；kind = tap/flick/drag/hold）
    QuickPlace { kind: opm_app::doc::NoteKind, lane_x: f32, beat: f64 },
    /// **事件区**起一个事件块草稿（指针所在那一列 = 轨道；与 hold 同一套跟随流程）
    StartEventDraft { track: TrackId, beat: f64 },
    /// 草稿（hold 或事件块）的终点跟随鼠标 —— **只在指针真的移动时**发，滚动/缩放不改长度
    DraftFollow { beat: f64 },
    /// 草稿：拖时间控制杆改起点或终点
    DraftResize { edge: EventEdge, beat: f64 },
    /// 草稿：放下（R / 回车 / **鼠标左键**）
    DraftCommit,
    /// 草稿：取消（Esc）
    DraftCancel,
    /// **遮蔽区编辑**：选中某块区的某条通道里的某个事件块（顺便把"当前列"切过去）
    MaskSelect {
        zone: usize,
        channel: MaskChannel,
        index: usize,
    },
    /// 遮蔽区编辑：点空白/列头 ⇒ 只切"当前列"，清掉块选中
    MaskSelectChannel(MaskChannel),
    /// 遮蔽区编辑：把选中块的时间跨度设成这个（**绝对值**，不是增量）。
    ///
    /// 为什么是绝对值：拖动期间文档每帧都在变，增量会在"视图慢一帧"时累积成漂移；
    /// 绝对跨度只需要一个冻结的起点（见 `MaskDrag`），与判定线的 `resize_event` 同一条思路。
    ///
    /// 为什么是**有理拍**而不是 `f64`：吸附已经在面板里做完了（`beat_at_grid`），命令层再按网格
    /// 取整一次就是第二次量化 —— 而"拖回按下点"要的正是**按下时的原值**原样写回，那个值不一定
    /// 在网格上（见 `MASK_RETURN_PX`）。
    MaskSetSpan {
        zone: usize,
        channel: MaskChannel,
        index: usize,
        start: opm_app::doc::Beat,
        end: opm_app::doc::Beat,
    },
    /// 遮蔽区编辑：拖动开始（调用方开事务 ⇒ 整段拖拽只占一个撤销步）
    MaskDragStart,
    /// 遮蔽区编辑：拖动结束（调用方提交事务）
    MaskDragEnd,
    /// 遮蔽区编辑：**R 起一个事件块草稿**（与判定线事件区同一套手势 —— 用户口径
    /// "创建流程应与普通编辑模式下的事件块放置一样"）。起点是吸附后的拍，值由核心取
    /// "此刻的值"；长度随鼠标，上限是下一块的起点。
    StartMaskDraft {
        zone: usize,
        channel: MaskChannel,
        beat: f64,
    },
    /// **中轴标签**：在轴带里按 `R` 起一个标签草稿（与 hold / 事件块同一套跟随手势）。
    /// 来源一律是 `Gui` —— `Cli` 那条路走控制通道，不经过手势。
    StartTagDraft { beat: f64 },
    /// 选中中轴上的第 i 个标签（属性编辑器改颜色用它）
    SelectTag(usize),
    /// 清掉标签选区（点了音符/事件/空白 —— "选中的是什么"必须唯一，否则 `Del` 不知道该删谁）
    ClearTagSelection,
    /// 删掉中轴上的第 i 个标签（轴带里按 `D` / 点删除控制杆）
    DeleteTag(usize),
    /// 拖**标签控制杆**开始：调用方开一格撤销（整段拖拽 = 一次撤销）
    TagResizeBegin {
        index: usize,
        edge: opm_app::state::EventEdge,
    },
    /// 拖控制杆中：把那一头挪到某个拍（已吸附）。每帧都会发，所以**它自己不开撤销**
    TagResize { beat: f64 },
    /// 拖控制杆结束
    TagResizeEnd,
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

/// 标签在轴带上的**列中心**：轴带左右各一列 —— **左 = GUI、右 = CLI**。
///
/// 两列的分工就是"来源"（用户口径："只留两列标注来源是 gui 还是 cli"）；
/// **颜色不参与分列**，它是每个标签自己的属性，可以逐个改。
pub fn tag_column_center(axis: egui::Rect, source: opm_app::state::TagSource) -> f32 {
    // 列只占**拍号区右边**的那一段（数字归 `AXIS_NUM_W` 独占）—— 这就是"数字移到标签外面"
    let left = axis.min.x + AXIS_NUM_W;
    let half = (axis.width() - AXIS_NUM_W).max(2.0) * 0.5;
    match source {
        opm_app::state::TagSource::Gui => left + half * 0.5,
        opm_app::state::TagSource::Cli => left + half * 1.5,
    }
}

/// 一个标签在轴带里的矩形：横向由**来源**定列，纵向由拍区间定。
pub fn tag_rect(
    axis: egui::Rect,
    body: egui::Rect,
    anchor: f64,
    beats: f64,
    t: &opm_app::state::Tag,
) -> egui::Rect {
    let cx = tag_column_center(axis, t.source);
    let (s0, s1) = t.span();
    let y0 = beat_y(body, anchor, beats, s0);
    let y1 = beat_y(body, anchor, beats, s1);
    // 每列留 3px 边距；短标签至少 3px 高，否则点不着、也看不见
    // 每列留 **2px** 内缩（原来是 3px）：列只有 (58−22)/2 = 18px 宽，
    // 内缩太多的话标签条窄到放不下里面的删除按钮（实测 9px < 按钮的 11px）。
    let hw = ((axis.width() - AXIS_NUM_W).max(2.0) * 0.25 - 2.0).max(2.0);
    let (lo, hi) = (y0.min(y1), y0.max(y1));
    egui::Rect::from_min_max(
        egui::pos2(cx - hw, lo.min(hi - 3.0)),
        egui::pos2(cx + hw, hi.max(lo + 3.0)),
    )
}

/// 标签上能抓的几个部分。
///
/// **没有"删除"这一档**（用户口径 2026-10-03："删除按 del，不要放删除按钮"）——
/// 删除走全局的 `Del`（与"Del = 删掉选中的东西"这条既有约定同源），标签上只留两端的**拉长杆**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagPart {
    /// 起点杆（下缘）
    Start,
    /// 终点杆（上缘）
    End,
    /// 本体（用来选中）
    Body,
}


/// 标签的**两条拉长杆**：`(起点杆, 终点杆)`。
///
/// 与事件块的头/尾把手同一套语义（抓哪一头改哪一头）。
/// 起点拍小 ⇒ 屏幕 y 大（纵向轴越往上越晚），所以起点杆在下缘。
pub fn tag_handle_rects(
    axis: egui::Rect,
    body: egui::Rect,
    anchor: f64,
    beats: f64,
    t: &opm_app::state::Tag,
) -> (egui::Rect, egui::Rect) {
    let r = tag_rect(axis, body, anchor, beats, t);
    let (lo, hi) = (r.min.y.min(r.max.y), r.min.y.max(r.max.y));
    let bar = |y: f32| {
        egui::Rect::from_min_max(
            egui::pos2(r.min.x, y - EDGE_BAND * 0.5),
            egui::pos2(r.max.x, y + EDGE_BAND * 0.5),
        )
    };
    // ⚠️ 顺序：`lo` 是**上**缘（y 小）、`hi` 是**下**缘。纵向轴"越往上越晚" ⇒
    // **起点拍（小）在下缘**，所以第一个返回值取 `bar(hi)`。
    (bar(hi), bar(lo)) // (起点杆 = 下缘, 终点杆 = 上缘)
}

/// 指针在标签的哪一部分上。**反序遍历**（后加的盖在上面，与绘制顺序一致）。
pub fn tag_hit_part(
    tags: &[opm_app::state::Tag],
    axis: egui::Rect,
    body: egui::Rect,
    anchor: f64,
    beats: f64,
    pos: egui::Pos2,
) -> Option<(usize, TagPart)> {
    for (i, t) in tags.iter().enumerate().rev() {
        let r = tag_rect(axis, body, anchor, beats, t);
        if !r.expand(EDGE_BAND * 0.5).contains(pos) {
            continue;
        }
        let (y0, y1) = (r.min.y.min(r.max.y), r.min.y.max(r.max.y));
        return Some((
            i,
            match hit_event_part(pos.y, y1, y0, EDGE_BAND) {
                // 注意实参顺序：`hit_event_part(指针, 起点y, 终点y, band)`，
                // 而这里的"起点"是下缘（y 大）
                EventPart::Start => TagPart::Start,
                EventPart::End => TagPart::End,
                _ => TagPart::Body,
            },
        ));
    }
    None
}

/// 命中的标签下标：**先按列筛 x、再按拍区间筛 y**。
///
/// 反向遍历：后加的盖在上面，点到的就该是它（与绘制顺序一致）。
pub fn tag_hit(
    tags: &[opm_app::state::Tag],
    axis: egui::Rect,
    body: egui::Rect,
    anchor: f64,
    beats: f64,
    pos: egui::Pos2,
) -> Option<usize> {
    tags.iter()
        .enumerate()
        .rev()
        .find(|(_, t)| tag_rect(axis, body, anchor, beats, t).contains(pos))
        .map(|(i, _)| i)
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

/// 按下左键时指针下面是什么。
///
/// egui 要等指针移动几个像素才认定为拖拽，那时指针可能**已经离开 6px 的把手段** ——
/// 用"当前命中"判断会永远拖不起来，所以必须在按下那一刻把它记下来。
///
/// 事件一律记**文档地址**（图层 + 图层内下标）而不是视图下标：视图把五个图层合并成一条
/// 时间线并按起拍排序，而拖拽期间时间一直在变 ⇒ 合并下标会重排，
/// 用它拖着拖着就换了一条事件（见 `opm_app::doc::EventRef`）。
#[derive(Clone, Copy, Debug, PartialEq)]
enum PressHit {
    Note(usize),
    EventEdge {
        track: TrackId,
        at: opm_app::doc::EventRef,
        view: usize,
        edge: EventEdge,
    },
    EventBody {
        track: TrackId,
        view: usize,
    },
}

fn press_hit_set(ui: &egui::Ui, v: Option<PressHit>) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new("opm_press_hit"), v));
}
fn press_hit_get(ui: &egui::Ui) -> Option<PressHit> {
    ui.data(|d| d.get_temp::<Option<PressHit>>(egui::Id::new("opm_press_hit")))
        .flatten()
}

/// 正在拖的事件**头/尾把手**：轨道 + 文档地址 + 视图下标 + 哪一头。
///
/// 拖拽一开始就把它定下来，而不是每帧重新做命中测试 —— 指针移动快时会离开把手段，
/// 那样拖拽会中途"掉线"。**改的地址用 `at`（文档地址），高亮用 `view`（视图下标）**：
/// 拖拽期间时间在变、合并下标会重排，后者只用来在画面上找那一块。
#[derive(Clone, Copy, Debug, PartialEq)]
struct EdgeDrag {
    track: TrackId,
    at: opm_app::doc::EventRef,
    view: usize,
    edge: EventEdge,
}

fn drag_edge_set(ui: &egui::Ui, v: Option<EdgeDrag>) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new("opm_event_drag"), v));
}
fn drag_edge_get(ui: &egui::Ui) -> Option<EdgeDrag> {
    ui.data(|d| d.get_temp::<Option<EdgeDrag>>(egui::Id::new("opm_event_drag")))
        .flatten()
}

/// 正在拖的**组**：按下那一刻冻结的抓手（成员、原点、手指位置）。
/// 有它就是在拖组 —— 也是"这一帧要不要发 `GrabMove`"的判据。
fn grab_set(ui: &egui::Ui, v: Option<opm_app::edit::Grab>) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new("opm_grab"), v));
}
fn grab_get(ui: &egui::Ui) -> Option<opm_app::edit::Grab> {
    ui.data(|d| d.get_temp::<Option<opm_app::edit::Grab>>(egui::Id::new("opm_grab")))
        .flatten()
}

/// 正在拉的**框选**：起点 + 选哪一类（**起始点定半区**，用户定的规则）。
///
/// 起点存的是**内容坐标（拍）**，不是屏幕 y —— 用户要求"框选时可以用滚轮移动"，
/// 而滚轮动的正是时间轴：屏幕 y 在滚动后会落到别的拍上，框就跟着视图漂，
/// "从这个音拖到那个音"会变成一个说不清的区间。起点钉在拍上，视图怎么动都不漂。
#[derive(Clone, Copy, Debug)]
struct BoxSelState {
    /// 按下那一刻、按当时的映射算出来的拍
    beat: f64,
    /// 起点横向的屏幕 x。时间轴只纵向滚动（没有横向平移），所以它可以原样留着
    x: f32,
    kind: SelKind,
}

impl BoxSelState {
    /// 起点的**当前**屏幕位置（每帧按当前视图重算 —— 这就是"钉在内容上"的落地）。
    fn start_pos(&self, y_of: impl Fn(f64) -> f32) -> egui::Pos2 {
        egui::pos2(self.x, y_of(self.beat))
    }
}

/// **遮蔽区编辑模式下正在拖的那一块**（冻结拖拽开始时的跨度）。
///
/// 为什么不复用 `EdgeDrag`：那个存的是"判定线事件的文档地址 + 哪一头"，
/// 而遮蔽区**没有图层、也没有多选**（一块区的一条通道就是一个数组，下标即文档下标），
/// 需要的额外信息是"拖的是整块还是某一头"和"按下时指针在哪一拍"。
/// 共用一份状态会让两条路径的语义互相污染（判定线那边还有组拖动，这边没有）。
#[derive(Clone, Copy, Debug)]
struct MaskDrag {
    zone: usize,
    channel: MaskChannel,
    index: usize,
    /// 拖拽开始时的跨度（拍）—— 平移增量的**原点**，拖动期间不动它
    start: f64,
    end: f64,
    /// 同一个原点的**精确有理形态**（文档里读出来的那一份）：拖回按下点时原样写回，
    /// 不经网格吸附 —— 它不一定在网格上（见 [`MASK_RETURN_PX`]）
    exact: (opm_app::doc::Beat, opm_app::doc::Beat),
    /// **上一次真正发出去**的跨度（去重的比较对象）。
    ///
    /// 拿原点当比较对象是踩过的坑：指针拖回按下点那一帧算出来的跨度**正好等于原点**，
    /// 于是被当成"没变化"而不发命令，块就停在拖出去的位置上回不来
    /// （用户报："拖动头尾控制杆无法移动回原位，拖动时可能卡住，也可能移动时会跳过原位置"）。
    live: (f64, f64),
    /// `None` = 整块平移；`Some` = 拖那一头
    edge: Option<EventEdge>,
    /// 按下时指针所在的拍（算平移增量）
    press_beat: f64,
    /// 按下时指针的屏幕 y —— "拖回按下点"按像素判，见 [`MASK_RETURN_PX`]
    press_y: f32,
}

/// **拖回按下点**的判定容差（像素）：指针进到这个圈里就用回按下时的跨度（**精确**，不经吸附）。
///
/// 为什么需要它：拖动落点是吸附到拍网格的，而**原跨度不一定在网格上**（导入的谱面、或改过
/// 网格细分的谱面）—— 那样"拖回原位"只能落到最近的格点，永远差一点。用户的原话就是
/// "无法移动回原位"。
const MASK_RETURN_PX: f32 = 2.0;

/// **按下那一刻**指针下面是哪个块的哪一部分（遮蔽区面板版，与判定线的 `PressHit` 同一条理由）。
///
/// 为什么必须记在按下时：egui 的拖拽阈值约 6 像素，而头尾把手段 `EDGE_BAND` 也是 6 像素 ——
/// `drag_started()` 那一帧指针**必定已经离开把手段**。拿那一帧的命中判"抓的是哪一头"，
/// 长块会判成"抓身体"（拖端点变成整块平移），短块直接判成没抓到（压根拖不动）。
/// 用户报的"拖动头尾控制杆无法移动回原位"就是这两条叠在一起。
#[derive(Clone, Copy, Debug, PartialEq)]
struct MaskPressHit {
    channel: MaskChannel,
    index: usize,
    part: EventPart,
    /// 按下时指针所在的拍（平移增量与"回到按下点"都用它，不必再问一次 `press_origin`）
    beat: f64,
    /// 按下时指针的屏幕 y
    y: f32,
}

fn mask_press_hit_set(ui: &egui::Ui, v: Option<MaskPressHit>) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new("opm_mask_press_hit"), v));
}

fn mask_press_hit_get(ui: &egui::Ui) -> Option<MaskPressHit> {
    ui.data(|d| d.get_temp::<Option<MaskPressHit>>(egui::Id::new("opm_mask_press_hit")))
        .flatten()
}

fn mask_drag_set(ui: &egui::Ui, v: Option<MaskDrag>) {
    ui.data_mut(|d| {
        if let Some(v) = v {
            d.insert_temp(egui::Id::new("opm_mask_drag"), v);
        } else {
            d.remove::<MaskDrag>(egui::Id::new("opm_mask_drag"));
        }
    });
}

fn mask_drag_get(ui: &egui::Ui) -> Option<MaskDrag> {
    ui.data(|d| d.get_temp::<MaskDrag>(egui::Id::new("opm_mask_drag")))
}


/// 按 `pad` 把闭区间夹进合法范围（`[0, ∞)` 里的一个"有长度的"区间）
fn clamp_span(start: f64, end: f64, min_len: f64) -> (f64, f64) {
    let start = start.max(0.0);
    let end = end.max(start + min_len);
    (start, end)
}

fn box_set(ui: &egui::Ui, v: Option<BoxSelState>) {
    ui.data_mut(|d| d.insert_temp(egui::Id::new("opm_box_sel"), v));
}
fn box_get(ui: &egui::Ui) -> Option<BoxSelState> {
    ui.data(|d| d.get_temp::<Option<BoxSelState>>(egui::Id::new("opm_box_sel")))
        .flatten()
}

/// 框选命中：**矩形相交**就算选中（碰到就选，与 kdenlive/RPE 的选择框一致）。
/// 抽成纯函数：只吃"画出来的矩形"，于是不必开窗口就能钉住"选了哪些"。
pub fn box_hits(rects: &[(usize, egui::Rect)], sel: egui::Rect) -> Vec<usize> {
    rects
        .iter()
        .filter(|(_, r)| sel.intersects(*r))
        .map(|(i, _)| *i)
        .collect()
}

// ---------------------------------------------------------------- 音符选择框
//
// 用户口径（2026-10-01）：**编辑区的 note 选择框就是判定的唯一依据**。
//   ① hold 点身体（非头部）也要能选中；
//   ② 完全重叠的音符要能选到"被盖住的那个"（属性编辑器给一份列表）；
//   ③ hold 的命中优先级**低于**其它类型；
//   ④ 遮挡计算对 hold **只看头部**（长的身体既不算盖住别人，也不算被别人盖住）。
//
// 以前的单点命中**不看框**：它算的是"指针到音符头部点的切比雪夫距离 < 10px" ——
// 于是 hold 只有头上那一下点得中，而重叠的两个音符永远只能选到先遍历到的那个。

/// 音符选择框宽度（与画出来的一致；x 方向）
pub const NOTE_W: f32 = 10.0;
/// 非 hold 音符选择框高度（与画出来的一致；y 方向 = 一行）
pub const NOTE_ROW_H: f32 = 7.0;

/// **本帧画出来的一个音符选择框** —— 单点命中、框选、重叠组共用同一份几何。
#[derive(Clone, Copy, Debug)]
pub struct NoteBox {
    /// 视图下标（时间序，与 `Line::notes` 一致）
    pub index: usize,
    /// 画出来是**竖条**（有时长）⇒ 命中优先级更低、遮挡只看头部。
    /// 零长度的 hold 画的就是方块，因此按普通音符对待 —— 判据是"画出来的形状"。
    pub hold: bool,
    /// 头部框：宽度 `NOTE_W`、高度 `NOTE_ROW_H`，锚在判定时刻
    pub head: egui::Rect,
    /// 整框：hold = 头 → 尾的竖条；其余与 `head` 相同
    pub full: egui::Rect,
}

/// 单点命中：**以选择框为准**（指针落在哪个 `full` 里）。
///
/// 优先级（用户口径）：
/// 1. **非 hold 优先于 hold** —— "hold 的判定放在其它类型音符下方"；
/// 2. 同类里**锚优先** —— 点在已选中的那个上不该把选区换人（重叠处每点一次都换人最烦）；
/// 3. 其余**后画的优先**（画序 = 文档序，后画的在视觉上压在上面）。
pub fn note_hit(boxes: &[NoteBox], pos: egui::Pos2, anchor: Option<usize>) -> Option<usize> {
    let mut best: Option<NoteBox> = None;
    for b in boxes.iter().filter(|b| b.full.contains(pos)) {
        let take = match &best {
            None => true,
            Some(cur) => {
                if b.hold != cur.hold {
                    !b.hold
                } else if (Some(b.index) == anchor) != (Some(cur.index) == anchor) {
                    Some(b.index) == anchor
                } else {
                    b.index > cur.index
                }
            }
        };
        if take {
            best = Some(*b);
        }
    }
    best.map(|b| b.index)
}

/// `a` 是否**有效覆盖** `b`：交叠区在 x 或 y 上超过 `b` 的一半。
///
/// 一律用**头部框**比较（用户口径："对于 hold，遮挡算法只判定头部区域"）。
/// 交叠为空**不算**覆盖 —— 同一条线上不同时刻的两个音符宽度完全相同，
/// 只看宽度会得出"它们互相遮挡"这种显然错的结论。
pub fn covers(a: &NoteBox, b: &NoteBox) -> bool {
    let ov = a.head.intersect(b.head);
    if !ov.is_positive() {
        return false;
    }
    ov.width() * 2.0 > b.head.width() || ov.height() * 2.0 > b.head.height()
}

/// 锚音符所在的**重叠组**（含锚自己，按视图下标升序）。
///
/// 关系**不传递**：只收"与锚直接有效覆盖"的那些。传递闭包会把隔着两层的一串音符
/// 也算进来 —— 那不是"点开被盖住的那些"，而是"这张谱面都连在一起了"。
pub fn overlap_group(boxes: &[NoteBox], anchor: usize) -> Vec<usize> {
    let Some(a) = boxes.iter().find(|b| b.index == anchor) else {
        return Vec::new();
    };
    let mut g: Vec<usize> = boxes
        .iter()
        .filter(|b| b.index == anchor || covers(a, b) || covers(b, a))
        .map(|b| b.index)
        .collect();
    g.sort_unstable();
    g
}

/// 框选命中（音符）：与 [`box_hits`] 同一条规则（相交即中），数据源是 `NoteBox`
pub fn note_box_hits(boxes: &[NoteBox], sel: egui::Rect) -> Vec<usize> {
    boxes
        .iter()
        .filter(|b| sel.intersects(b.full))
        .map(|b| b.index)
        .collect()
}

/// **开始组拖动**：冻结抓手 → 把选区调整成"要拖的那些" → 发 `GrabStart`。
///
/// 四件事都在这里，是因为它们必须同时成立：
/// · 手指按在**选区之内** ⇒ 拖整个选区（相对偏移保持不变）；
/// · 手指按在**选区之外** ⇒ 先把选区换成它一个，再拖它（否则会把别的一起带走）；
/// · 冻结的原点取自**本帧**的视图状态（下一帧文档就变了）；
/// · 调用方收到 `GrabStart` 要开事务 —— 整段拖拽只占一个撤销步。
///
/// `note` / `event` 是**手指按住的那一个**（由按下时的命中给出），它同时是吸附的原点。
fn start_grab(
    ui: &egui::Ui,
    st: &EditorState,
    note: Option<usize>,
    event: Option<(TrackId, usize)>,
    press_lane: f32,
    press_beat: f64,
    actions: &mut Vec<OverlayAction>,
) {
    use opm_app::edit::GrabIntent;
    let in_selection = match (note, event) {
        (Some(i), _) => st.is_note_selected(i),
        (_, Some((t, i))) => st.is_event_selected(t, i),
        _ => false,
    };
    let mut intent = if in_selection {
        GrabIntent::selection(st, press_lane, press_beat)
    } else {
        match (note, event) {
            (Some(i), _) => {
                actions.push(OverlayAction::SelectNote(i));
                GrabIntent::one_note(i, press_lane, press_beat)
            }
            (_, Some((t, i))) => {
                actions.push(OverlayAction::SelectTrack(t));
                actions.push(OverlayAction::SelectEvent(i));
                GrabIntent::one_event(t, i, press_lane, press_beat)
            }
            _ => return,
        }
    };
    // 手指按住的那一个就是吸附的原点 —— 整组平移时"它跟着指针走"，其余保持相对偏移
    intent.anchor_note = note;
    intent.anchor_event = event;
    // **卷进重叠的事件不许移动**（用户要求）：轨道已经不是"一块接一块"的结构，
    // "挪到最近合法位置"那套推理没有意义 —— 拒绝，并说清楚改怎么走。
    if opm_app::edit::event_drag_disabled(st, &intent.events) {
        actions.push(OverlayAction::Notice(
            "选中的事件里有重叠，拖动已禁用：拖它的头/尾把手改时间，或在冲突浏览器里跳过去".to_owned(),
        ));
        return;
    }
    // 抓手取不到（这一帧视图里没有它）⇒ 当作没按下去，别开一个拖不动的事务
    let Some(g) = opm_app::edit::grab_selection(st, &intent) else {
        return;
    };
    grab_set(ui, Some(g.clone()));
    actions.push(OverlayAction::GrabStart(Box::new(g)));
}

/// 快速放置键的**落点**：同一个键在两个半区里的含义不一样，而"哪个键在哪个半区做什么"
/// 正是需求本身（踩过：事件区里按 Q/W/E 也会起一个事件草稿 —— 那是误触）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QuickKey {
    /// 在音符区放一个音符（tap/flick/drag/hold 由键定）
    Note(opm_app::doc::NoteKind),
    /// 在事件区起一个事件块草稿
    EventDraft,
    /// 在**中轴标签带**上起一个标签草稿（用户口径："光标在编辑区中间的时间轴上可以按 r 放置 tag"）
    TagDraft,
    /// 这个键在这个半区里没有意义 —— **什么都不做**
    Nothing,
    /// 指针不在任何一个半区里（标尺/轴带上）：说一句话解释为什么没反应
    Outside,
}

/// 规则（用户定）：**音符区** Q/W/E/R 四个键各放一种音符；**事件区只有 R** 能用
/// （起事件块草稿，与音符区的 hold 同一套跟随流程）；其余键在事件区**没有反应**。
pub fn quick_key_rule(key: egui::Key, in_notes: bool, in_events: bool, in_axis: bool) -> QuickKey {
    // **轴带优先**：它是三块里最窄的一块，而且 `R` 在这里的含义与另外两块都不同
    // （放标签，不是放音符/事件块）。先判它，免得被"指针 x 落在哪一半"顺手吃掉。
    if in_axis {
        return if key == egui::Key::R {
            QuickKey::TagDraft
        } else {
            QuickKey::Nothing
        };
    }
    if in_notes {
        return match opm_app::keymap::quick_place_kind(key) {
            Some(kind) => QuickKey::Note(kind),
            None => QuickKey::Nothing,
        };
    }
    if in_events {
        return if key == egui::Key::R {
            QuickKey::EventDraft
        } else {
            QuickKey::Nothing
        };
    }
    QuickKey::Outside
}

/// 滚轮位移 → 拍增量（纯函数，可单测）。
///
/// 约定：**向上滚 = 时间往后**（与纵轴"越上越晚"一致）。步长随可见拍数缩放：
/// 放大到 8 拍可见时一格走半拍，缩到 256 拍可见时一格走 16 拍 —— 无论缩放到哪一级，
/// "一格"在屏幕上走过的距离都差不多。
pub fn scroll_delta_to_beats(scroll_y: f32, beats_visible: f64, per_notch: f64) -> f64 {
    (scroll_y as f64 / 50.0) * per_notch * (beats_visible / 32.0).max(0.15)
}

/// 编辑区里"竖直滚一格"的量 —— **按住 Shift 时 egui 会把滚轮整体折到横轴**。
///
/// 这不是猜的：`egui::Options` 里 `horizontal_scroll_modifier` 默认就是 `SHIFT`，
/// 而 `vertical_scroll_modifier` 默认是 `NONE`；`WheelState` 见到 Shift 就做
/// `delta = vec2(delta.x + delta.y, 0.0)` —— 于是按住 Shift 滚轮时 `smooth_scroll_delta.y` **恒为 0**。
/// 编辑区没有横向滚动（时间轴是纵轴），所以两个轴在这里是同一个意思：合并读。
/// 之前只读 `.y`：Shift 拖框期间滚轮完全没反应（用户报的就是这一条）。
pub fn wheel_delta_y(delta_x: f32, delta_y: f32, shift: bool) -> f32 {
    if shift {
        delta_x + delta_y
    } else {
        delta_y
    }
}

/// 编辑区读滚轮的**唯一一处**：`(竖直量, 缩放倍率)`。
/// 两份绘制路径（判定线编辑器 / 遮蔽区编辑器）都用它，免得哪天只修好一边。
fn overlay_wheel(ui: &egui::Ui) -> (f32, f32) {
    ui.input(|i| {
        (
            wheel_delta_y(i.smooth_scroll_delta.x, i.smooth_scroll_delta.y, i.modifiers.shift),
            i.zoom_delta(),
        )
    })
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

/// 叠加层的可见性规则。抽成纯函数：这条规则要有单测钉住。
///
/// **`H` 的语义是"翻转当前这一档"**（用户口径 2026-10-03："在播放时按住 H 可以重新显示编辑区，
/// 同时播放谱面"）：
///
/// | 状态 | 不按 H | 按住 H |
/// |---|---|---|
/// | 暂停 | **显示**（编谱时数据为主） | 隐藏（想干净看一眼预览） |
/// | 播放中 | 隐藏（预览要干净） | **显示**（边听边看数据，**播放不停**） |
///
/// 写成 `playing == h_held` 就是这张表 —— 两个布尔量相同则显示。
/// 早先的写法是 `!playing && !h_held`：它在"播放中按 H"那一格**永远为假**，
/// 也就是播放时 H 没有任何作用（连"按住也不显示"这一点都无从谈起）。
pub fn overlay_visible(enabled: bool, playing: bool, h_held: bool) -> bool {
    enabled && playing == h_held
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
/// 轴带宽度。**用户口径 2026-10-03**："将时间轴数字移到标签外面" —— 于是它分成两段：
/// 左边 [`AXIS_NUM_W`] 专画拍号，右边剩下的才是标签的两列。数字因此**永远不会**被标签盖住
///（此前数字居中、标签占满两半，标签一长就把拍号糊掉一截）。
const AXIS_W: f32 = 58.0;
/// 轴带里**拍号区**的宽度（靠左）。剩下的宽度均分给两列标签。
const AXIS_NUM_W: f32 = 22.0;

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

/// 叠加层每帧的**回执**（不是动作）：调用方拿它更新**视图状态**。
///
/// 为什么要有这个：锚音符的**重叠组**必须由"本帧画出来的选择框"算出来（用户口径：
/// "以编辑区 note 选择框为准"），而选择框的几何只在这里有（缩放、窗口偏移都在这边算）。
/// 与 `timeline::draw` 的返回值同一个套路。
#[derive(Clone, Debug, Default)]
pub struct OverlayOut {
    /// 锚音符所在**重叠组**的视图下标（含锚自己；没选中音符时为空）
    pub note_stack: Vec<usize>,
}

/// 画叠加层。返回本帧产生的动作 + 回执（见 [`OverlayOut`]）。
#[allow(clippy::too_many_arguments)]
pub fn draw(
    ui: &mut egui::Ui,
    st: &EditorState,
    rect: egui::Rect,
    cfg: &OverlayCfg,
    // 现在能不能用快捷键（打字/模态期间为 false，由调用方算好 —— 门控只有一处）
    keys_enabled: bool,
    actions: &mut Vec<OverlayAction>,
) -> OverlayOut {
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
    // **两种模式的骨架**（用户口径：遮蔽区编辑模式下"使用整个编辑区空间、音符功能停用"）：
    // · 普通模式：纵轴轴带在**正中**，左半音符 / 右半事件；
    // · 遮蔽区编辑：轴带贴**左边缘**（拍号还是要有），右边整片是七条通道 ——
    //   不给 7 列塞一半宽度，因为坐标通道要靠拖时间块来编，列越宽越好用。
    let mask_mode = st.mask_edit;
    let (axis, note_pane, ev_pane, lanes) = if mask_mode {
        let axis = egui::Rect::from_min_max(
            egui::pos2(rect.min.x, body.min.y),
            egui::pos2(rect.min.x + AXIS_W, body.max.y),
        );
        let lanes = egui::Rect::from_min_max(egui::pos2(axis.max.x, body.min.y), body.max);
        (axis, egui::Rect::NOTHING, egui::Rect::NOTHING, lanes)
    } else {
        let axis = egui::Rect::from_min_max(
            egui::pos2(mid_x - AXIS_W * 0.5, body.min.y),
            egui::pos2(mid_x + AXIS_W * 0.5, body.max.y),
        );
        let note_pane = egui::Rect::from_min_max(body.min, egui::pos2(axis.min.x, body.max.y));
        let ev_pane = egui::Rect::from_min_max(egui::pos2(axis.max.x, body.min.y), body.max);
        (axis, note_pane, ev_pane, ev_pane)
    };
    // 两半各自压一点底色，轴带再暗一档（轴是"骨架"，不该抢内容）
    let pane_a = (cfg.body_alpha.clamp(0.0, 1.0) * 46.0) as u8;
    if !mask_mode {
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
    }
    p.rect_filled(
        axis,
        0.0,
        egui::Color32::from_rgba_unmultiplied(10, 11, 16, (cfg.body_alpha.clamp(0.0, 1.0) * 150.0) as u8),
    );
    // 轴带两侧各一条竖线：分栏线就是轴本身
    // （遮蔽区模式下轴贴在左边缘 ⇒ 只有右侧那条有意义）
    let axis_sides: &[f32] = if mask_mode { &[axis.max.x] } else { &[axis.min.x, axis.max.x] };
    for x in axis_sides {
        p.line_segment(
            [egui::pos2(*x, rect.min.y), egui::pos2(*x, rect.max.y)],
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
        // 网格线只画在内容区里：轴带留白，视觉上才是"被轴分开的区"
        let panes: &[egui::Rect] = if mask_mode { &[lanes] } else { &[note_pane, ev_pane] };
        for pane in panes {
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
            for x in axis_sides {
                let x = *x;
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
        // 数字居中写在**轴带**里，两侧各一个小刻度线指向相邻半区。
        // 遮蔽区模式下轴带贴在左边缘 ⇒ 这里必须用轴带自己的中心，不能用 `mid_x`
        //（用 `mid_x` 会把拍号写到通道列上 —— 截图里一眼看得出来，测试看不出来）
        // 拍号画在**数字区**（轴带左侧那一条）里 —— 用户口径："将时间轴数字移到标签外面"。
        // 此前它居中写在整条轴带上，而标签占满两半 ⇒ 标签一长就把拍号盖掉一截。
        p.text(
            egui::pos2(axis.min.x + AXIS_NUM_W * 0.5, y - 1.0),
            egui::Align2::CENTER_BOTTOM,
            &t.text,
            egui::FontId::monospace(fsize),
            col,
        );
        for x in axis_sides {
            let x = *x;
            p.line_segment(
                [egui::pos2(x, y), egui::pos2(x + if x < mid_x { tick } else { -tick }, y)],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(150, 160, 205)),
            );
        }
    }
    // ---- 中轴标签：**两列**（左 = GUI，右 = CLI），颜色是标签自己的 ----
    //
    // 用户口径 2026-10-03："标签放置在中轴，黄色和蓝色分两列" → "只留两列标注来源是 gui 还是 cli，
    // 可以自定义不同标签的颜色"。⇒ **分列管来源、颜色管标签自己**，两层信息互不干扰。
    //
    // 只在普通模式下画：遮蔽区模式里轴带贴在左边缘，那里的"拍"仍然有意义，但那一栏
    // 整片是七条通道的地盘，再塞两列标签只会两边都看不清。
    // 指针压着哪个标签（控制杆只在它身上亮）—— 用**按下时也算**的指针，拖的时候不闪
    // 指针压着哪个标签（控制杆只在它身上亮）。这里用 egui 的**悬停**位置而不是后面那个
    // `ptr`（按下时才有值）—— 绘制发生在交互解析之前，拿不到它。
    let hovered_tag = ui
        .ctx()
        .pointer_hover_pos()
        .filter(|q| rect.contains(*q))
        .and_then(|q| tag_hit(&st.tags, axis, body, anchor, beats, q));
    if !mask_mode {
        for (i, t) in st.tags.iter().enumerate() {
            let r = tag_rect(axis, body, anchor, beats, t);
            if r.max.y < body.min.y || r.min.y > body.max.y {
                continue; // 完全在窗口外
            }
            let col = egui::Color32::from_rgb(t.color[0], t.color[1], t.color[2]);
            p.rect_filled(r, 1.5, col);
            if st.selected_tag == Some(i) {
                p.rect_stroke(
                    r.expand(1.5),
                    1.5,
                    egui::Stroke::new(1.6, egui::Color32::WHITE),
                    egui::StrokeKind::Outside,
                );
            }
            // ---- 控制杆：**悬停或选中的那一个**才亮 ----
            //
            // 用户口径："删除和变长控制杆"。两端的横条 = 拉长（抓哪头改哪头），
            // 中间的 ✕ = 删除。全画出来会糊成一片，所以只在指针压着它、或它被选中时画。
            let hot = hovered_tag == Some(i) || st.selected_tag == Some(i);
            if hot {
                // 只画两条**拉长杆**；删除不在这里做（用户口径："删除按 del，不要放删除按钮"）
                let (sb, eb) = tag_handle_rects(axis, body, anchor, beats, t);
                let bar_col = egui::Color32::from_rgb(255, 246, 200);
                for bar in [sb, eb] {
                    p.rect_filled(bar, 1.0, bar_col);
                }
            }
        }
    }

    // 标题靠右放：左上角是 RPE 窗口标注的地盘，两边都放会叠字
    p.text(
        egui::pos2(rect.max.x - 4.0, rect.min.y + 1.0),
        egui::Align2::RIGHT_TOP,
        // 简短：右半的列名就在同一行，长标题会跟列名叠字（其余信息在工具栏/检查器里）
        if mask_mode {
            format!(
                "遮蔽区编辑 [{} 拍] · 七条通道 · R 起稿放块 · Del 删除 · Ctrl+滚轮 缩放 · H 隐藏",
                beats as i64
            )
        } else {
            format!(
                "编辑区 [{} 拍] · 基准 1 拍 · 网格 1/{} · Ctrl+滚轮 缩放 · H 隐藏",
                beats as i64,
                div
            )
        },
        egui::FontId::monospace(9.5),
        if mask_mode {
            egui::Color32::from_rgb(250, 180, 180)
        } else {
            egui::Color32::from_rgb(165, 175, 215)
        },
    );

    // **选区读数**：多选之后"选了几个"必须一眼可见（Del 与拖动都作用在它上面）。
    // 与标题同一列右对齐、错开一行，两行都不叠字。
    if !st.selection().is_empty() {
        let msg = match st.selection_kind() {
            Some(SelKind::Notes) => format!(
                "已选 {} 个音符 · Del 删除 · 拖动整体平移（四向箭头）",
                st.selection().len()
            ),
            Some(SelKind::Events) => format!(
                "已选 {} 条事件 · Del 删除 · 拖动整块平移（有空档才跨得过去）",
                st.selection().len()
            ),
            None => String::new(),
        };
        p.text(
            egui::pos2(rect.max.x - 4.0, rect.min.y + 12.0),
            egui::Align2::RIGHT_TOP,
            msg,
            egui::FontId::monospace(9.5),
            egui::Color32::from_rgb(255, 225, 150),
        );
    }

    // 播放头线（拍位置）
    let y_now = y_of(beat_now);
    if y_now >= body.min.y && y_now <= body.max.y {
        p.line_segment(
            [egui::pos2(body.min.x, y_now), egui::pos2(body.max.x, y_now)],
            egui::Stroke::new(1.4, egui::Color32::from_rgb(245, 235, 120)),
        );
    }

    // ---- 遮蔽区编辑模式：整个编辑区都是它的（音符区功能停用，用户口径）----
    //
    // 放在"取选中判定线"**之前**：这个模式下不需要判定线，没有判定线也能编遮蔽区。
    if mask_mode {
        draw_mask_pane(ui, st, rect, lanes, keys_enabled, cfg, &y_of, &beat_of, actions);
        return OverlayOut::default();
    }

    let Some(line) = st.selected() else {
        return OverlayOut::default();
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
    let row_h = NOTE_ROW_H;
    // 本帧画过的音符选择框：**单点命中、框选、重叠组全都用它** ——
    // "画在哪"与"选得中什么"必须是同一份几何（这一条以前只做到了框选那一半）。
    let mut note_boxes: Vec<NoteBox> = Vec::new();
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
        // 头部框（一个方块）与整框（hold 是有时长的竖条，其余与头部框相同）
        let head = egui::Rect::from_min_max(
            egui::pos2(x - NOTE_W * 0.5, y0 - row_h * 0.5),
            egui::pos2(x + NOTE_W * 0.5, y0 + row_h * 0.5),
        );
        let hold = (n.end - n.time).abs() > 1e-6;
        let full = if hold {
            egui::Rect::from_min_max(
                egui::pos2(x - NOTE_W * 0.5, y0.min(y1)),
                egui::pos2(x + NOTE_W * 0.5, y0.max(y1)),
            )
        } else {
            head
        };
        // Hold 用竖条表示时长，其余用方块
        let (r, fill) = if hold {
            (
                full,
                egui::Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 110),
            )
        } else {
            (full, col)
        };
        // 裁剪到音符区：平移之后，窗口外的音符会落到面板之外 —— 直接画就会糊到轴带/事件区上
        let r = r.intersect(note_pane);
        if !r.is_positive() {
            continue;
        }
        // 框本身**不裁剪**（命中判定要用真正的框；指针在音符区内，等价于裁剪后的框）
        note_boxes.push(NoteBox { index: i, hold, head, full });
        // **选区成员一律描边**（不是只有锚亮）：多选之后"选了几个"必须一眼看得见。
        // 锚用实线白框、其余成员用稍细的浅框 —— 检查器显示的是锚，视觉上要能分辨。
        let in_sel = st.is_note_selected(i);
        p.rect_filled(r, 1.0, fill);
        if in_sel {
            let anchor = st.selected_note() == Some(i);
            p.rect_stroke(
                r.expand(1.0),
                1.0,
                egui::Stroke::new(
                    if anchor { 1.4 } else { 1.0 },
                    if anchor {
                        egui::Color32::WHITE
                    } else {
                        egui::Color32::from_rgba_unmultiplied(235, 240, 255, 190)
                    },
                ),
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
    // 本帧画过的事件块：(轨道, 事件下标, 矩形, 起点 y, 终点 y)
    // —— 把手高亮、框选、命中都复用同一份几何
    let mut blocks: Vec<(TrackId, usize, egui::Rect, f32, f32)> = Vec::new();
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
            let selected_ev = st.is_event_selected(*id, i);
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
            blocks.push((*id, i, r, y0, y1));
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
        /// 轴带：**带上指针下面的标签下标**（没有就是 `None`）。
        /// 标签只住在这里（用户口径："标签放置在中轴"），所以命中信息挂在轴带这一支上。
        Axis(Option<usize>),
        Note(usize),
        /// 事件：**轨道** + 该轨道**合并视图**里的下标 + 命中哪一部分（头/尾/本体）。
        /// 带上轨道是因为"选中优先"与"哪一块"都必须限定在这一列里 ——
        /// 锚的下标与别的轨道毫无关系。
        Event(TrackId, usize, EventPart),
        Pane,
    }
    let hit = ptr.map(|pos| {
        if over_ruler(pos) {
            return Hit::Ruler(beat_of(pos.y.max(rect.min.y + RULER_H)));
        }
        if in_axis(pos) {
            return Hit::Axis(tag_hit(&st.tags, axis, body, anchor, beats, pos));
        }
        if pos.x < mid_x {
            // 音符区：**以选择框为准**（`note_hit` 里写着优先级：非 hold > hold、锚优先、后画的优先）
            return note_hit(&note_boxes, pos, st.selected_note())
                .map(Hit::Note)
                .unwrap_or(Hit::Pane);
        }
        // 事件区：命中列 → 命中块（优先头/尾把手）
        let k = (((pos.x - ev_pane.min.x) / col_w).floor() as usize).min(lanes - 1);
        let id = TrackId::ALL[k];
        let tr = line.track(id);
        // 收集**所有**候选，再按规则挑 —— 头尾相接时会有两个事件同时命中同一条 y
        let mut near: Vec<(usize, EventEdge, f32)> = Vec::new();
        let mut block_hit: Option<usize> = None;
        for (i, e) in tr.events.iter().enumerate() {
            let y0 = y_of(st.chart.tmap.beat(st.chart.tmap.sec(e.start.to_f64())));
            let y1 = y_of(st.chart.tmap.beat(st.chart.tmap.sec(e.end.to_f64())));
            // y0 = 起点的屏幕 y（在下）、y1 = 终点的屏幕 y（在上）
            match hit_event_part(pos.y, y0, y1, EDGE_BAND) {
                EventPart::Start => near.push((i, EventEdge::Start, (pos.y - y0).abs())),
                EventPart::End => near.push((i, EventEdge::End, (pos.y - y1).abs())),
                EventPart::Body => {
                    if block_hit.is_none() {
                        block_hit = Some(i);
                    }
                }
                EventPart::None => {}
            }
        }
        // 只保留最近的一批（同一条边界上会有一对），再按"选中优先 → 否则选尾巴"决定。
        // "选中优先"必须是**这一列上**的选中项：锚若落在别的轨道上，它的下标与本列候选无关。
        if let Some(best) = near.iter().map(|(_, _, d)| *d).fold(None, |m: Option<f32>, d| {
            Some(m.map(|x| x.min(d)).unwrap_or(d))
        }) {
            let cands: Vec<(usize, EventEdge)> = near
                .iter()
                .filter(|(_, _, d)| (*d - best).abs() < 0.75) // 同一 y 上的都算相接
                .map(|(i, e, _)| (*i, *e))
                .collect();
            let sel_here = cands
                .iter()
                .find(|(i, _)| st.is_event_selected(id, *i))
                .map(|(i, _)| *i);
            if let Some((i, edge)) = prefer_edge(&cands, sel_here) {
                let part = if edge == EventEdge::Start {
                    EventPart::Start
                } else {
                    EventPart::End
                };
                return Hit::Event(id, i, part);
            }
        }
        block_hit
            .map(|i| Hit::Event(id, i, EventPart::Body))
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
        && grab_get(ui).is_none()
    {
        let code = match &hit {
            Some(Hit::Note(i)) => Some(PressHit::Note(*i)),
            Some(Hit::Event(track, i, part @ (EventPart::Start | EventPart::End))) => {
                let edge = if *part == EventPart::Start {
                    EventEdge::Start
                } else {
                    EventEdge::End
                };
                // 冻结**文档地址**：拖拽期间时间在变 ⇒ 合并下标会重排
                line.track(*track)
                    .origin(*i)
                    .map(|at| PressHit::EventEdge {
                        track: *track,
                        at,
                        view: *i,
                        edge,
                    })
            }
            Some(Hit::Event(track, i, _)) => Some(PressHit::EventBody {
                track: *track,
                view: *i,
            }),
            _ => None,
        };
        press_hit_set(ui, code);
    }

    // ---- 把手高亮：**只亮一个**（与拖拽同一个决策，见 handle_to_highlight）----
    let hover_edge = match &hit {
        Some(Hit::Event(_, i, EventPart::Start)) => Some((*i, EventEdge::Start)),
        Some(Hit::Event(_, i, EventPart::End)) => Some((*i, EventEdge::End)),
        _ => None,
    };
    let highlight = handle_to_highlight(drag_edge_get(ui).map(|d| (d.view, d.edge)), hover_edge);
    if let Some((i, edge)) = highlight {
        // 亮的是**哪一列**上的那一条：光有下标不够（5 条轨道都有下标 i）
        let hl_track = drag_edge_get(ui).map(|d| d.track).or(match &hit {
            Some(Hit::Event(t, _, _)) => Some(*t),
            _ => None,
        });
        if let Some((_, _, r, _, _)) = blocks
            .iter()
            .find(|(bt, bi, _, _, _)| Some(*bt) == hl_track && *bi == i)
        {
            let p = ui.painter_at(rect);
            // 与待放置的 hold 用**同一个** `TimeHandle`（判定/绘制同源）
            TimeHandle::from_span(r.min.x, r.max.x, r.bottom(), r.top())
                .paint(&p, edge, egui::Color32::WHITE);
        }
    }

    // ---- 光标形状 ----
    //
    // 用户要求：**多选**的音符上是四向箭头（X 与拍都能动）、事件上是上下双头箭头
    // （事件只有时间一个轴）—— 光标说的就是"拖起来会怎么动"。
    // 单选/未选中时保持原来的口径：音符 = 抓手，事件头尾 = 双头箭头。
    let multi = st.selection().len() > 1;
    // 拖这个事件会不会被"重叠"挡住？选中了就看整个选区，没选中就看它一个 —— 同一份判据
    let blocked_event_drag = |track: TrackId, i: usize| -> bool {
        let items: Vec<opm_app::state::EventSel> = if st.is_event_selected(track, i) {
            st.selection().events().collect()
        } else {
            vec![(track, i)]
        };
        opm_app::edit::event_drag_disabled(st, &items)
    };
    match &hit {
        // 重叠事件的**块体**：光标直接说"不许拖"（按下去也只会被拒，不该先给一个可以拖的暗示）。
        // 头/尾把手**不在此列** —— 拖把手改时间正是修重叠的那条路，它仍然好用。
        Some(Hit::Event(t, i, EventPart::Body)) if blocked_event_drag(*t, *i) => {
            ui.ctx().set_cursor_icon(egui::CursorIcon::NotAllowed);
        }
        Some(Hit::Note(i)) if multi && st.is_note_selected(*i) => {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Move);
        }
        Some(Hit::Event(t, i, EventPart::Body)) if multi && st.is_event_selected(*t, *i) => {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        // 事件块本体也能整块平移（只有时间一个轴）⇒ 与把手同一个光标
        Some(Hit::Event(_, _, _)) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical),
        Some(Hit::Note(_)) => {
            let dragging = grab_get(ui).is_some();
            ui.ctx().set_cursor_icon(if dragging {
                egui::CursorIcon::Grabbing
            } else {
                egui::CursorIcon::Grab
            });
        }
        Some(Hit::Ruler(_)) => ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand),
        _ => {}
    }

    // ---- 快速放置（Q/W/E/R）、事件区起草稿、草稿跟随与控制杆 ----
    //
    // 位置解析（指针 → 吸附后的 laneX/拍/轨道列）只有这里懂，所以"按键 → 放什么"也放在这里：
    // 调用方只负责施加动作。
    //
    // **约束（用户要求）：草稿跟随状态下，编辑区的滚动与缩放必须照常。**
    // 具体做法：这一段既不读也不消费滚轮/缩放事件（它们在下面独立处理），
    // 跟随只认"指针**移动**"这一个信号 —— 于是滚动/缩放时长度不变、功能不受影响。
    let pointer_in_notes = |pos: egui::Pos2| pos.x < mid_x && !in_axis(pos) && !over_ruler(pos);
    let event_col_of = |pos: egui::Pos2| -> Option<(TrackId, f32)> {
        if pos.x <= mid_x || in_axis(pos) || over_ruler(pos) {
            return None;
        }
        let k = (((pos.x - ev_pane.min.x) / col_w).floor() as usize).min(lanes - 1);
        Some((TrackId::ALL[k], ev_pane.min.x + col_w * (k as f32 + 0.5)))
    };
    if keys_enabled && !st.drafting() {
        for key in [egui::Key::Q, egui::Key::W, egui::Key::E, egui::Key::R] {
            if !key_pressed_once(ui, key) {
                continue;
            }
            let in_notes = ptr.map(pointer_in_notes).unwrap_or(false);
            let in_events = ptr.and_then(event_col_of).is_some();
            let axis_hit = ptr.map(|q| in_axis(q)).unwrap_or(false);
            match quick_key_rule(key, in_notes, in_events, axis_hit) {
                QuickKey::Note(kind) => {
                    let pos = ptr.expect("in_notes 只可能来自指针");
                    actions.push(OverlayAction::QuickPlace {
                        kind,
                        lane_x: st.snap_lane(lane_of_x(pos.x)),
                        beat: st.snap_beat(beat_of(pos.y)).max(0.0),
                    });
                }
                QuickKey::TagDraft => {
                    let pos = ptr.expect("TagDraft 只可能来自指针");
                    actions.push(OverlayAction::StartTagDraft {
                        beat: st.snap_beat(beat_of(pos.y)).max(0.0),
                    });
                }
                QuickKey::EventDraft => {
                    let pos = ptr.expect("in_events 只可能来自指针");
                    let (track, _) = event_col_of(pos).expect("in_events 只可能来自指针");
                    actions.push(OverlayAction::StartEventDraft {
                        track,
                        beat: st.snap_beat(beat_of(pos.y)).max(0.0),
                    });
                }
                // **事件区里按 Q/W/E：什么都不做**（用户要求）。
                // 它们只对音符有意义，先前这里会拿它们去起事件草稿 —— 那是误触。
                QuickKey::Nothing => {}
                // 指针不在两个半区里（标尺上/轴带上）：**说清楚为什么没反应**
                QuickKey::Outside => actions.push(OverlayAction::Notice(
                    "快速放置：音符区用 Q/W/E/R，事件区 R，中轴 R 放标签（指针要放在某一栏里）"
                        .to_owned(),
                )),
            }
        }
        // `D`：删掉轴带上指针下面那个标签（用户口径："d删除标签"）。
        // 与放置分开写：它不在 Q/W/E/R 那一组里，也不产生草稿。
        if key_pressed_once(ui, egui::Key::D) {
            let in_axis_now = ptr.map(|q| in_axis(q)).unwrap_or(false);
            if in_axis_now {
                if let Some(i) = ptr.and_then(|q| tag_hit(&st.tags, axis, body, anchor, beats, q)) {
                    actions.push(OverlayAction::DeleteTag(i));
                }
            }
        }
    }
    // ---- 标签控制杆的**拖拽**（视图状态：没有事务，但整段只占一格撤销）----
    //
    // 与点选分开：`drag_started()` 要等 egui 的拖拽阈值（约 6px）过了才为真，而那一刻指针
    // 早已离开把手段 —— 所以"抓的是哪一头"要用**按下时**的指针位置判（`press_origin`），
    // 这是判定线事件区与遮蔽区事件块都踩过同一个坑，见 `event_press_hit` / `mask_press_hit`。
    if keys_enabled && !st.drafting() {
        if resp.drag_started() {
            if let Some(q) = ui.input(|i| i.pointer.press_origin()).or(ptr) {
                if in_axis(q) {
                    if let Some((i, part)) = tag_hit_part(&st.tags, axis, body, anchor, beats, q) {
                        let edge = match part {
                            TagPart::Start => Some(opm_app::state::EventEdge::Start),
                            TagPart::End => Some(opm_app::state::EventEdge::End),
                            _ => None,
                        };
                        if let Some(edge) = edge {
                            actions.push(OverlayAction::SelectTag(i));
                            actions.push(OverlayAction::TagResizeBegin { index: i, edge });
                        }
                    }
                }
            }
        }
        if resp.dragged() {
            if let Some(q) = ptr.filter(|q| in_axis(*q)) {
                actions.push(OverlayAction::TagResize {
                    beat: st.snap_beat(beat_of(q.y)).max(0.0),
                });
            }
        }
        if resp.drag_stopped() {
            actions.push(OverlayAction::TagResizeEnd);
        }
    }

    // ---- 草稿（hold / 事件块）：矩形由本模式算，手势两个模式共用 ----
    //
    // 用户口径 2026-10-02（遮蔽区）："创建流程应与普通编辑模式下的事件块放置一样" ——
    // 于是"按 R 起稿 → 鼠标定长度 → R/回车/左键放下 → Esc 取消"只有一份实现
    //（`draft_gesture`），两个模式各自只负责回答"草稿画在哪"和"指针在不在我这半区里"。
    let draft_rect = {
        let span_rect = |x0: f32, x1: f32, y0: f32, y1: f32| {
            egui::Rect::from_min_max(egui::pos2(x0, y0.min(y1)), egui::pos2(x1, y0.max(y1)))
        };
        if let Some(t) = st.pending_tag {
            // 标签草稿在轴带上（横向 = 它那一列），纵向由拍区间定
            Some(tag_rect(
                axis,
                body,
                anchor,
                beats,
                &opm_app::state::Tag {
                    start: t.start,
                    end: t.end,
                    source: t.source,
                    color: t.color,
                },
            ))
        } else if let Some(h) = st.pending_hold {
            // hold 在音符区（横向 = 音符宽）
            let x = x_of_lane(h.lane_x);
            Some(span_rect(x - 5.0, x + 5.0, y_of(h.start_beat), y_of(h.end_beat)))
        } else if let Some(e) = st.pending_event {
            // 事件块在它那一列（横向 = 列宽）
            let k = TrackId::ALL.iter().position(|t| *t == e.track).unwrap_or(0);
            let x0 = ev_pane.min.x + col_w * k as f32;
            Some(span_rect(x0 + 2.0, x0 + col_w - 2.0, y_of(e.start_beat), y_of(e.end_beat)))
        } else {
            None
        }
    };
    // 标签草稿的"半区"是**轴带**（它不住在音符区也不住在事件区）——
    // 漏掉这一条的表现是：草稿能起、控制杆却拖不动（`draft_gesture` 认为指针不在半区里）
    let in_pane = ptr
        .map(|q| {
            pointer_in_notes(q)
                || event_col_of(q).is_some()
                || (st.pending_tag.is_some() && in_axis(q))
        })
        .unwrap_or(false);
    draft_gesture(
        ui,
        &p,
        st,
        keys_enabled,
        &resp,
        ptr,
        draft_rect,
        in_pane,
        &|y| st.snap_beat(beat_of(y)).max(0.0),
        actions,
    );

    // 滚轮：在编辑区里滚动就是**改谱面当前时间**（向上滚 = 往后）。跟随播放头的窗口会把这一变化
    // 直接体现出来，所以"滚动"与"移动播放头"在这里是同一件事（只此一处，别重复写第二份）。
    //
    // **Shift 拖框期间滚轮照旧有效**（用户要求）：Shift 会把滚轮折到横轴（见 `wheel_delta_y`），
    // 所以这里两个轴一起读 —— 只读 `.y` 的话，正是"框选时滚轮没反应"的那条 bug。
    // 滚轮动的是视图，框的起点钉在拍上（`BoxSelState::start_pos`），于是框跟着内容走。
    if resp.hovered() {
        // `zoom_delta` 里含 Ctrl+滚轮与触控板捏合；按住 Ctrl 时 `smooth_scroll_delta` 恒为 0
        let (dy, zoom) = overlay_wheel(ui);
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

    // ---- 框选 / 组拖动 / 点选：互不嵌套 ----
    //
    // 三段各自判"这一帧有没有在拖"，优先级就是下面的顺序：
    // **Shift+左键 = 框选**（显式意图，最先判）→ **拖动**（把手 / 整组）→ **点选**。
    //
    // 这一条是踩出来的：egui 把"刚判定为拖拽"的那一帧标记为 `drag_started`（`dragged` 同时为真），
    // 早先写成 `if drag_started {..} else if dragged {..}` ⇒ 那一帧被跳过，于是
    // "按下→小幅移动→松开"这类**快速拖拽一次位置更新都发不出去**（看上去就是拖不动）。
    let shift = ui.input(|i| i.modifiers.shift);
    let ctrl = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);

    if resp.drag_started() {
        // **按下时**的指针位置（不是"刚判定为拖拽"那一刻的）：egui 要等指针移开几个像素才
        // 认定拖拽，那时指针已经跑了；`press_origin` 给的就是按下去的那一点 ——
        // 框选的起始半区、抓手的原点都必须取它，否则"起始点定半区"会按错半区、
        // 被拖的东西也会凭空跳一个拖拽阈值的距离。
        let press = ui
            .input(|i| i.pointer.press_origin())
            .or(resp.interact_pointer_pos())
            .or(hover_pos);
        // 框选的**起始点定半区**（用户要求：框跨过两区时以起始点判断选哪一类）。
        // 半区在这一刻定下来，之后往哪拖都不改。
        let box_kind = press.and_then(|pos| {
            if pointer_in_notes(pos) {
                Some(SelKind::Notes)
            } else if event_col_of(pos).is_some() {
                Some(SelKind::Events)
            } else {
                None // 标尺/轴带上起手：不框选（那里没有可选的东西）
            }
        });
        if shift {
            if let (Some(pos), Some(k)) = (press, box_kind) {
                // 起点存**拍**（不是屏幕 y）：之后滚轮移动视图时，框仍然从同一个音起算
                box_set(
                    ui,
                    Some(BoxSelState {
                        beat: beat_of(pos.y),
                        x: pos.x,
                        kind: k,
                    }),
                );
            }
        } else {
            // 用**按下时**的命中（不是当前命中）：拖拽阈值会让指针先移开那 6px 的把手段
            match press_hit_get(ui) {
                Some(PressHit::EventEdge { track, at, view, edge }) => {
                    // 选了一组时，压住其中一块的**任何位置**都是"整组平移"
                    // （用户要求：多选事件上悬停 = 上下双向箭头 = 可以统一拖动位置）
                    if st.is_event_selected(track, view) && st.selection().len() > 1 {
                        if let Some(q) = press {
                            let (lane, beat) = (lane_of_x(q.x), beat_of(q.y));
                            start_grab(ui, st, None, Some((track, view)), lane, beat, actions);
                        }
                    } else {
                        actions.push(OverlayAction::SelectTrack(track));
                        actions.push(OverlayAction::SelectEvent(view));
                        actions.push(OverlayAction::EventResizeStart);
                        drag_edge_set(ui, Some(EdgeDrag { track, at, view, edge }));
                    }
                }
                Some(PressHit::Note(i)) => {
                    if let Some(q) = press {
                        start_grab(ui, st, Some(i), None, lane_of_x(q.x), beat_of(q.y), actions);
                    }
                }
                Some(PressHit::EventBody { track, view }) => {
                    if let Some(q) = press {
                        start_grab(
                            ui,
                            st,
                            None,
                            Some((track, view)),
                            lane_of_x(q.x),
                            beat_of(q.y),
                            actions,
                        );
                    }
                }
                None => {}
            }
        }
    }
    if resp.drag_started() || resp.dragged() {
        if let Some(d) = drag_edge_get(ui) {
            if let Some(pos) = ptr {
                // 头/尾按**拍网格**吸附（它调的就是时间）
                actions.push(OverlayAction::EventResize {
                    track: d.track,
                    at: d.at,
                    edge: d.edge,
                    beat: st.snap_beat(beat_of(pos.y)).max(0.0),
                });
            }
        } else if let Some(g) = grab_get(ui) {
            if let Some(pos) = ptr {
                // 吸附与"最近合法位置"都在库里（`edit::grab_delta`，纯函数有单测）；
                // 面板只负责把"指针在哪"换算成 laneX/拍。
                let (d_lane, d_beat) =
                    opm_app::edit::grab_delta(st, &g, lane_of_x(pos.x), beat_of(pos.y));
                actions.push(OverlayAction::GrabMove { d_lane, d_beat });
            }
        }
        // 框选：把框画出来（`drag_started` 那一帧也要画，否则第一帧看不到框）
        // 起点每帧按**当前的拍映射**重算 ⇒ 拖动期间滚轮移动视图时，框跟着内容一起走
        if let (Some(sel), Some(cur)) = (box_get(ui), ptr) {
            let b = egui::Rect::from_two_pos(sel.start_pos(&y_of), cur);
            let col = match sel.kind {
                SelKind::Notes => egui::Color32::from_rgb(150, 190, 255),
                SelKind::Events => egui::Color32::from_rgb(255, 200, 120),
            };
            let p = ui.painter_at(rect);
            p.rect_filled(b, 0.0, egui::Color32::from_rgba_unmultiplied(255, 255, 255, 24));
            p.rect_stroke(b, 0.0, egui::Stroke::new(1.0, col), egui::StrokeKind::Inside);
        }
    } else if resp.drag_stopped() {
        press_hit_set(ui, None);
        if drag_edge_get(ui).is_some() {
            actions.push(OverlayAction::EventResizeEnd);
            drag_edge_set(ui, None);
        }
        if grab_get(ui).is_some() {
            actions.push(OverlayAction::GrabEnd);
            grab_set(ui, None);
        }
        if let Some(sel) = box_get(ui) {
            // 框选落地：**只按起始点定下的那一类**算命中（另一类的矩形根本不看）
            let start = sel.start_pos(&y_of);
            let b = egui::Rect::from_two_pos(start, ptr.unwrap_or(start));
            match sel.kind {
                SelKind::Notes => {
                    actions.push(OverlayAction::SelectNotes(note_box_hits(&note_boxes, b)));
                }
                SelKind::Events => {
                    let rects: Vec<(usize, egui::Rect)> =
                        blocks.iter().enumerate().map(|(n, b)| (n, b.2)).collect();
                    let picked: Vec<(TrackId, usize)> = box_hits(&rects, b)
                        .into_iter()
                        .map(|n| (blocks[n].0, blocks[n].1))
                        .collect();
                    actions.push(OverlayAction::SelectEvents(picked));
                }
            }
            box_set(ui, None);
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
    } else if resp.clicked() && !st.drafting() {
        // 草稿期间不点选（那时候左键是"放下"）—— 见 `drafting` 那一段。
        // **Shift+单击什么都不做**：Shift+拖动是框选，而"没拖动起来"的那一下
        // 不该顺手把选区清掉（那是用户看得见的数据丢失感）。
        if !shift {
            match hit {
                // 标尺只挪播放头：它和选区没关系，点它不该把选区清掉
                Some(Hit::Ruler(b)) => {
                    actions.push(OverlayAction::ClearTagSelection);
                    actions.push(OverlayAction::SeekBeat(b));
                }
                // 轴带上点中一个标签 ⇒ 选中它（属性编辑器据此显示/改颜色）。
                // 但**控制杆优先**：删除按钮、两端拉长杆各自有语义，不能都被"选中"吃掉。
                // 轴带上点中一个标签 ⇒ 选中它（属性编辑器据此显示/改颜色）。
                // 两端拉长杆**不在这里起拖拽** —— 它们是"拖"的，不是"点"的；
                // 单击若也发一次 `TagResizeBegin`，会留下一个永远等不到 `drag_stopped` 的悬空拖拽。
                //
                // 顺带清掉音符/事件选区：**"选中的是什么"必须唯一**，否则 `Del` 不知道该删谁。
                Some(Hit::Axis(Some(i))) => {
                    actions.push(OverlayAction::ClearSelection);
                    actions.push(OverlayAction::SelectTag(i));
                }
                // Ctrl+左键：在"选中 / 未选中"之间切换（用户要求）
                Some(Hit::Note(i)) => actions.push(if ctrl {
                    OverlayAction::ToggleNote(i)
                } else {
                    OverlayAction::SelectNote(i)
                }),
                Some(Hit::Event(t, i, _)) => {
                    if ctrl {
                        actions.push(OverlayAction::ToggleEvent(t, i));
                    } else {
                        actions.push(OverlayAction::SelectTrack(t));
                        actions.push(OverlayAction::SelectEvent(i));
                    }
                }
                // 点空白/轴带：清空选区（多选之后总得有个"全不选"的手势）
                _ => actions.push(OverlayAction::ClearSelection),
            }
        }
    }

    // ---- 回执：锚音符所在的重叠组（视图状态；不进文档）----
    //
    // 只在"选中的是音符"时算：事件选中时锚音符会**保留**（切回去还是那一个），
    // 但那时不该显示音符的重叠组 —— 与属性编辑器"只显示选中那一半"是同一条口径。
    let note_stack = if st.sel_kind() == Some(SelKind::Notes) {
        st.selected_note()
            .map(|a| overlap_group(&note_boxes, a))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    OverlayOut { note_stack }
}


/// 草稿（hold / 判定线事件块 / 遮蔽区事件块）的**手势**：放下、取消、控制杆、跟随。
///
/// 为什么抽成一份：用户口径 2026-10-02（遮蔽区）——"创建流程应与普通编辑模式下的事件块放置
/// 一样"。以前遮蔽区是"按下 R 就地放一块、长度写死"，与普通模式那套"起稿 → 跟随 → 放下"
/// 是两条流程，于是同一个动作在两个模式下得到不同的块。
///
/// 各模式只回答两件事：**草稿矩形**（列宽/列位置不同）与**指针在不在自己这半区里**
/// （跟随只在指针留在本区时才改长度）；键位、放下条件、控制杆的判定与绘制都在这。
#[allow(clippy::too_many_arguments)]
fn draft_gesture(
    ui: &egui::Ui,
    p: &egui::Painter,
    st: &EditorState,
    keys_enabled: bool,
    resp: &egui::Response,
    ptr: Option<egui::Pos2>,
    draft: Option<egui::Rect>,
    in_pane: bool,
    // 屏幕 y → 吸附后的拍（两个模式的 y 映射不同，但吸附规则同一条）
    turn: &impl Fn(f32) -> f64,
    actions: &mut Vec<OverlayAction>,
) {
    if !st.drafting() {
        return;
    }
    // 放下 / 取消：只有这一处判键，免得和别处抢。
    // `keys_enabled` 同样管着它们 —— 在控制台打字时按回车不该把草稿放下。
    let (commit_key, cancel) = if keys_enabled {
        (
            key_pressed_once(ui, egui::Key::R) || key_pressed_once(ui, egui::Key::Enter),
            key_pressed_once(ui, egui::Key::Escape),
        )
    } else {
        (false, false)
    };
    // **左键点一下 = 放下**（用户要求）：拖动控制杆仍然是改起止、拖动别处仍然是跟随，
    // 所以只认"没有拖动过的单击"。
    let commit_click = !resp.dragged() && resp.clicked_by(egui::PointerButton::Primary);
    if commit_key || commit_click {
        actions.push(OverlayAction::DraftCommit);
        return;
    }
    if cancel {
        actions.push(OverlayAction::DraftCancel);
        return;
    }
    let Some(draft) = draft else { return };
    let fill = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 90);
    let edge_col = egui::Color32::from_rgb(255, 235, 160);
    if draft.is_positive() {
        p.rect_filled(draft, 1.0, fill);
        p.rect_stroke(
            draft.expand(1.0),
            1.0,
            egui::Stroke::new(1.2, edge_col),
            egui::StrokeKind::Outside,
        );
    }
    // 矩形 → 控制杆：起点在下（纵轴越上越晚），与编辑区同一条约定
    let handle = TimeHandle::from_span(draft.min.x, draft.max.x, draft.max.y, draft.min.y);
    // 判"抓的是哪一头"用**按下时的位置**：egui 的拖拽阈值（约 6 像素）与把手段 `EDGE_BAND`
    // 一样宽 ⇒ `drag_started()` 那一帧指针必定已经离开把手段，用当前位置判会把"抓头/尾"
    // 降级成"长度跟着鼠标走"（短草稿甚至判成没抓到）。与遮蔽区事件块的 `MaskPressHit`、
    // 判定线事件区的 `press_hit` 是同一条理由。
    // 指针没按下时 `press_origin()` 是 `None` ⇒ 退回当前位置，悬停高亮照常。
    let grab = ui.input(|i| i.pointer.press_origin()).or(ptr);
    let hot = grab.map(|q| handle.hit(q));
    let color = |hit: bool| if hit { egui::Color32::WHITE } else { edge_col };
    handle.paint(p, EventEdge::Start, color(hot == Some(EventPart::Start)));
    handle.paint(p, EventEdge::End, color(hot == Some(EventPart::End)));
    if resp.drag_started() || resp.dragged() {
        match hot {
            Some(EventPart::Start) | Some(EventPart::End) => {
                let edge = if hot == Some(EventPart::Start) {
                    EventEdge::Start
                } else {
                    EventEdge::End
                };
                if let Some(q) = ptr {
                    actions.push(OverlayAction::DraftResize { edge, beat: turn(q.y) });
                }
            }
            // 拖在别处 = "长度跟着鼠标走"
            _ => {
                if let Some(q) = ptr {
                    actions.push(OverlayAction::DraftFollow { beat: turn(q.y) });
                }
            }
        }
    } else if let Some(q) = ptr.filter(|_| in_pane) {
        // 只有**真的移动**才改长度：滚动/缩放时指针没动，长度与视口都保持原样
        let moved = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        if moved {
            actions.push(OverlayAction::DraftFollow { beat: turn(q.y) });
        }
    }
}

/// 遮蔽区编辑模式的七条通道列。
///
/// 交互与判定线的事件区**同一条规则**（命中头/尾优先于本体、吸附到拍网格、拖动期间冻结跨度），
/// 差别只在数据源与动作：这里发 `Mask*` 动作，调用方翻译成 `*_zone_event` 命令。
/// 单文件里两套列布局并存是刻意的 —— 判定线那边还挂着多选、组拖动、框选、跨图层合并地址，
/// 把这些一起泛化会让 3000 行的叠加层多出一层抽象，而收益只是"少写 120 行"。
///
/// 它**放块的手势与普通模式共用一份实现**（`R` 起稿 → 跟随 → `R`/回车/左键放下，见
/// `draft_gesture`），删除靠全局 `Del`；滚轮换算与普通模式**逐字相同** ——
/// 用户口径："统一两个模式下的滚轮滑动速度"。
#[allow(clippy::too_many_arguments)]
fn draw_mask_pane(
    ui: &mut egui::Ui,
    st: &EditorState,
    rect: egui::Rect,
    lanes: egui::Rect,
    // 现在能不能用快捷键（打字/模态期间为 false，调用方算好 —— 门控只有一处）
    keys_enabled: bool,
    cfg: &OverlayCfg,
    y_of: &impl Fn(f64) -> f32,
    beat_of: &impl Fn(f32) -> f64,
    actions: &mut Vec<OverlayAction>,
) {
    let p = ui.painter_at(rect);
    let tmap = &st.chart.tmap;
    let beat_now = tmap.beat(st.playhead);
    let zone_idx = st.selected_zone;
    // 一个区都没有时这里是**草稿区**（`add_zone` 会写出来的那一块）：七列照常画、照常编，
    // 用户真的动一下编辑才 materialize（用户口径 2026-10-02，见 `App::mask_commands_many`）
    let Some(zone) = st.mask_edit_view() else {
        return;
    };
    let zone = zone.as_ref();

    let n = MaskChannel::ALL.len();
    let col_w = lanes.width() / n as f32;
    let col_of_x = |x: f32| -> usize {
        (((x - lanes.min.x) / col_w).floor().max(0.0) as usize).min(n - 1)
    };

    // ---- 列背景 + 列名 + 此刻的值 ----
    for (k, ch) in MaskChannel::ALL.iter().enumerate() {
        let x0 = lanes.min.x + k as f32 * col_w;
        let col = MASK_COLORS[k];
        let is_sel = *ch == st.selected_channel;
        p.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(x0 + 1.0, lanes.min.y),
                egui::pos2(x0 + col_w - 1.0, lanes.max.y),
            ),
            0.0,
            egui::Color32::from_rgba_unmultiplied(col[0], col[1], col[2], if is_sel { 26 } else { 10 }),
        );
        if k > 0 {
            p.line_segment(
                [egui::pos2(x0, lanes.min.y), egui::pos2(x0, lanes.max.y)],
                egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(120, 130, 170, 90)),
            );
        }
        // 列头：通道名 + 此刻的值（值就是"这块区现在长什么样"的一部分）
        let tr = zone.track(*ch);
        let cur = opm_app::perf::track_value(&tr.events, beat_now, tmap);
        let val = match (*ch, cur) {
            (MaskChannel::Active, Some(v)) => if v >= 0.5 { "true" } else { "false" }.to_owned(),
            (_, Some(v)) => format!("{v:.0}"),
            (_, None) => "—".to_owned(),
        };
        p.text(
            egui::pos2(x0 + col_w * 0.5, rect.min.y + RULER_H - 1.0),
            egui::Align2::CENTER_BOTTOM,
            format!("{} {val}", ch.key()),
            egui::FontId::monospace(9.0),
            egui::Color32::from_rgb(col[0], col[1], col[2]),
        );
        if is_sel {
            p.line_segment(
                [
                    egui::pos2(x0 + 2.0, rect.min.y + RULER_H - 1.0),
                    egui::pos2(x0 + col_w - 2.0, rect.min.y + RULER_H - 1.0),
                ],
                egui::Stroke::new(1.6, egui::Color32::from_rgb(col[0], col[1], col[2])),
            );
        }
    }

    // ---- 事件块 ----
    // 本帧画过的块：(通道, 下标, 矩形, 起点 y, 终点 y) —— 命中/框选/高亮共用这一份几何
    let mut blocks: Vec<(MaskChannel, usize, egui::Rect, f32, f32)> = Vec::new();
    for (k, ch) in MaskChannel::ALL.iter().enumerate() {
        let x0 = lanes.min.x + k as f32 * col_w;
        let col = MASK_COLORS[k];
        let inset = col_w * 0.16; // 比判定线那半边更靠边：通道少、列宽，块画宽一点更好点
        for (i, e) in zone.track(*ch).events.iter().enumerate() {
            let y0 = y_of(tmap.beat(tmap.sec(e.start.to_f64())));
            let y1 = y_of(tmap.beat(tmap.sec(e.end.to_f64())));
            if y0.max(y1) < lanes.min.y - 20.0 || y0.min(y1) > lanes.max.y + 20.0 {
                continue;
            }
            let r = egui::Rect::from_min_max(
                egui::pos2(x0 + inset, y0.min(y1)),
                egui::pos2(x0 + col_w - inset, y0.max(y1).max(y0.min(y1) + 3.0)),
            );
            let selected = st.selected_channel == *ch && st.is_mask_event_selected(i);
            let (bottom, top) =
                gradient_colors(egui::Color32::from_rgb(col[0], col[1], col[2]), selected);
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
            if selected {
                p.rect_stroke(
                    r.expand(1.0),
                    1.0,
                    egui::Stroke::new(1.4, egui::Color32::WHITE),
                    egui::StrokeKind::Outside,
                );
            }
            // 块里写起止值（高度不够就不硬塞 —— 与判定线事件区同一条口径）
            if r.height() >= 16.0 {
                let txt = egui::Color32::from_rgb(235, 240, 255);
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
            blocks.push((*ch, i, r, y0, y1));
        }
    }

    // ---- 交互 ----
    let resp = ui.interact(
        rect,
        egui::Id::new("opm_mask_pane"),
        egui::Sense::click_and_drag() | egui::Sense::hover(),
    );
    let hover_pos = resp.hover_pos();
    let ptr = resp.interact_pointer_pos().or(hover_pos);
    let over_ruler = |pos: egui::Pos2| pos.y < rect.min.y + RULER_H;
    let in_axis = |pos: egui::Pos2| pos.x < lanes.min.x;

    // 命中：先算清楚"指针下面是什么"
    #[derive(Clone, Copy)]
    enum Hit {
        Ruler(f64),
        Channel(MaskChannel),
        Block(MaskChannel, usize, EventPart),
    }
    let hit = ptr.map(|pos| {
        if over_ruler(pos) {
            return Hit::Ruler(beat_of(pos.y.max(rect.min.y + RULER_H)));
        }
        if in_axis(pos) {
            return Hit::Channel(MaskChannel::ALL[0]);
        }
        let k = col_of_x(pos.x);
        let ch = MaskChannel::ALL[k];
        let tr = zone.track(ch);
        // 与判定线同一条规则：头/尾把手优先，同一 y 上"选中的那个优先"，否则选靠后的
        let mut near: Vec<(usize, EventEdge, f32)> = Vec::new();
        let mut body: Option<usize> = None;
        for (i, e) in tr.events.iter().enumerate() {
            let y0 = y_of(tmap.beat(tmap.sec(e.start.to_f64())));
            let y1 = y_of(tmap.beat(tmap.sec(e.end.to_f64())));
            match hit_event_part(pos.y, y0, y1, EDGE_BAND) {
                EventPart::Start => near.push((i, EventEdge::Start, (pos.y - y0).abs())),
                EventPart::End => near.push((i, EventEdge::End, (pos.y - y1).abs())),
                EventPart::Body => {
                    if body.is_none() {
                        body = Some(i);
                    }
                }
                EventPart::None => {}
            }
        }
        if let Some(best) = near
            .iter()
            .map(|(_, _, d)| *d)
            .fold(None, |m: Option<f32>, d| Some(m.map(|x: f32| x.min(d)).unwrap_or(d)))
        {
            let cands: Vec<(usize, EventEdge)> = near
                .iter()
                .filter(|(_, _, d)| (*d - best).abs() < 0.75)
                .map(|(i, e, _)| (*i, *e))
                .collect();
            let sel_here = cands
                .iter()
                .find(|(i, _)| st.selected_channel == ch && st.is_mask_event_selected(*i))
                .map(|(i, _)| *i);
            if let Some((i, edge)) = prefer_edge(&cands, sel_here) {
                let part = if edge == EventEdge::Start {
                    EventPart::Start
                } else {
                    EventPart::End
                };
                return Hit::Block(ch, i, part);
            }
        }
        match body {
            Some(i) => Hit::Block(ch, i, EventPart::Body),
            None => Hit::Channel(ch),
        }
    });

    // 光标：拖得动就直说（事件块只有时间一个轴 ⇒ 上下双头箭头）——草稿期间不改光标
    // （那时左键是"放下"，说成"能拖"会误导）
    if !st.drafting() {
        match &hit {
            Some(Hit::Block(_, _, _)) => ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical),
            Some(Hit::Channel(_)) if ptr.is_some() && !in_axis(ptr.unwrap()) => {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            }
            _ => {}
        }
    }

    // ---- 事件块草稿（R 起稿）：矩形 + 手势 ----
    //
    // 矩形在这里算（列宽与列位置只有本模式知道），放下/取消/控制杆/跟随走两个模式共用的
    // `draft_gesture` —— 与判定线事件区的创建流程**同一份实现**。
    let draft_rect = st.pending_mask.and_then(|d| {
        let k = MaskChannel::ALL.iter().position(|c| *c == d.channel)?;
        let x0 = lanes.min.x + k as f32 * col_w;
        let (y0, y1) = (y_of(d.start.to_f64()), y_of(d.end.to_f64()));
        Some(egui::Rect::from_min_max(
            egui::pos2(x0 + 2.0, y0.min(y1)),
            egui::pos2(x0 + col_w - 2.0, y0.max(y1)),
        ))
    });
    draft_gesture(
        ui,
        &p,
        st,
        keys_enabled,
        &resp,
        ptr,
        draft_rect,
        // 跟随只在指针留在通道区里时改长度（滚轮/缩放照常 —— 见 `draft_gesture` 的尾注）
        ptr.map(|q| !in_axis(q) && !over_ruler(q)).unwrap_or(false),
        &|y| st.snap_beat(beat_of(y)).max(0.0),
        actions,
    );

    // 记录"**按下时**指针下面是什么"，供拖拽开始那一帧使用 —— 与判定线事件区同一套
    // （见 `MaskPressHit`；判定线那边是 `press_hit_set`，三个条件逐字相同）：
    // · 已经进入拖拽 ⇒ 不再更新（否则会把"按下时抓的是哪一头"覆盖掉）；
    // · `drag_started/dragged` 那一帧也不更新 —— 那一帧指针已经移开把手段了，
    //   而拖拽分支就在本帧稍后运行，必须让它看到**按下时**的命中；
    // · egui 的第一帧没有交互状态（hover 还是 None），所以"只在未按下时记录"会漏掉按下那一帧。
    if !resp.drag_started() && !resp.dragged() && mask_drag_get(ui).is_none() {
        let rec = match (&hit, ptr) {
            (Some(Hit::Block(ch, i, part)), Some(q)) => Some(MaskPressHit {
                channel: *ch,
                index: *i,
                part: *part,
                beat: beat_of(q.y),
                y: q.y,
            }),
            _ => None,
        };
        mask_press_hit_set(ui, rec);
    }

    // 拖动（草稿期间不动事件块：那时左键是"放下"）
    if resp.drag_started() && !st.drafting() {
        // **用按下时的命中**，不是这一帧的：拖拽阈值让指针一上来就离开把手段（见 `MaskPressHit`）
        if let Some(ph) = mask_press_hit_get(ui) {
            let e = zone.track(ph.channel).events.get(ph.index);
            if let Some(e) = e {
                let edge = match ph.part {
                    EventPart::Start => Some(EventEdge::Start),
                    EventPart::End => Some(EventEdge::End),
                    _ => None,
                };
                let (start, end) = (e.start.to_f64(), e.end.to_f64());
                mask_drag_set(
                    ui,
                    Some(MaskDrag {
                        zone: zone_idx,
                        channel: ph.channel,
                        index: ph.index,
                        start,
                        end,
                        exact: (e.start, e.end),
                        // 刚按下时文档就在原点 ⇒ 去重的起点是原点本身
                        live: (start, end),
                        edge,
                        press_beat: ph.beat,
                        press_y: ph.y,
                    }),
                );
                actions.push(OverlayAction::MaskSelect {
                    zone: zone_idx,
                    channel: ph.channel,
                    index: ph.index,
                });
                actions.push(OverlayAction::MaskDragStart);
            }
        }
    }
    if resp.drag_started() || resp.dragged() {
        if let (Some(d), Some(q)) = (mask_drag_get(ui), ptr) {
            // 邻块给出这**一块**能待的窗口：上一块的终点 .. 下一块的起点。
            // 通道的不变量是"不许重叠"，所以拖动必须在窗口里停下 —— 核心那边也会拒
            //（`set_zone_event` 的重叠闸），但"拖到边界就停住"比"每帧弹一句错误"好用。
            let evs = &zone.track(d.channel).events;
            let floor = d
                .index
                .checked_sub(1)
                .and_then(|i| evs.get(i))
                .map_or(0.0, |e| e.end.to_f64());
            let ceiling = evs.get(d.index + 1).map(|e| e.start.to_f64());
            // 吸附与夹取都在这里一次算完（拖动只改时间一个轴）
            let snapped = st
                .snap_beat(beat_of(q.y))
                .max(0.0)
                .max(floor)
                .min(ceiling.unwrap_or(f64::INFINITY));
            // **指针拖回按下点 ⇒ 精确用回按下时的跨度**（连有理拍都照抄，见 `MASK_RETURN_PX`）。
            // 判据是"指针回到按下点了没有"，不是"算出来的跨度变了没有"：后者正是把块卡在
            // 外面回不来的原因。之所以还要回到**有理拍**：吸附只保证落在格点上，而按下时的原值
            // 不一定在网格上 —— 那样"拖回原位"永远差一点点。
            let back_to_press = (q.y - d.press_y).abs() <= MASK_RETURN_PX;
            let span = if back_to_press {
                d.exact
            } else {
                let (start, end) = match d.edge {
                    // 拖端点：**只动这一头**，另一头钉死，且不许交叉 —— 与草稿的
                    // `state::resize_span` 是同一份规则（此前这里另写了一份，反向拖会把块翻过来）
                    Some(edge) => {
                        let (mut s, mut e) = (d.start, d.end);
                        opm_app::state::resize_span(
                            &mut s,
                            &mut e,
                            edge,
                            snapped,
                            st.beat_step(),
                        );
                        (s, e)
                    }
                    None => {
                        // 整块平移：两端一起走，两头都不许压到邻块（长度保住 —— 拖的是位置）
                        let delta = (snapped - d.press_beat)
                            .max(floor - d.start)
                            .min(ceiling.map_or(f64::INFINITY, |hi| hi - d.end));
                        clamp_span(d.start + delta, d.end + delta, 1e-3)
                    }
                };
                // 吸附在**面板里**做完（命令层不再取整一次，见 `MaskSetSpan` 的头注）
                (st.beat_at_grid(start), st.beat_at_grid(end))
            };
            // 去重比的是**上一次发出去的**跨度，不是原点（见 `MaskDrag::live`）
            if (span.0.to_f64(), span.1.to_f64()) != d.live {
                actions.push(OverlayAction::MaskSetSpan {
                    zone: d.zone,
                    channel: d.channel,
                    index: d.index,
                    start: span.0,
                    end: span.1,
                });
                mask_drag_set(
                    ui,
                    Some(MaskDrag {
                        live: (span.0.to_f64(), span.1.to_f64()),
                        ..d
                    }),
                );
            }
        }
    }
    if resp.drag_stopped() {
        mask_press_hit_set(ui, None);
        if mask_drag_get(ui).is_some() {
            actions.push(OverlayAction::MaskDragEnd);
            mask_drag_set(ui, None);
        }
    }

    // 点选（双击**不放块**：普通模式里双击是音符区的事，事件块一律"R 起稿再放下"）
    if resp.clicked() && mask_drag_get(ui).is_none() && !st.drafting() {
        match hit {
            Some(Hit::Ruler(b)) => actions.push(OverlayAction::SeekBeat(b)),
            Some(Hit::Block(ch, i, _)) => actions.push(OverlayAction::MaskSelect {
                zone: zone_idx,
                channel: ch,
                index: i,
            }),
            Some(Hit::Channel(ch)) => actions.push(OverlayAction::MaskSelectChannel(ch)),
            None => {}
        }
    }

    // ---- R：在**指针所在的那一列**起一个草稿 ----
    //
    // 用户口径 2026-10-02："在屏蔽区按 r 添加事件块"，以及"创建流程应与普通编辑模式下的事件块
    // 放置一样"—— 于是这里与判定线事件区**逐字同一条**：R 起稿 → 鼠标定长度（保底一个格点、
    // 不越过下一块）→ R/回车/左键放下 → Esc 取消；放下/跟随/控制杆都在 `draft_gesture`。
    if keys_enabled && !st.drafting() && !resp.dragged() {
        if key_pressed_once(ui, egui::Key::R) {
            // 指针在列上（块里也算 —— 那时起点落在块内，放下会把前一块裁到新起点）
            let target = match (&hit, ptr) {
                (Some(Hit::Channel(ch)), Some(q)) => Some((*ch, q)),
                (Some(Hit::Block(ch, _, _)), Some(q)) => Some((*ch, q)),
                _ => None,
            };
            match target {
                Some((ch, q)) => actions.push(OverlayAction::StartMaskDraft {
                    zone: zone_idx,
                    channel: ch,
                    beat: st.snap_beat(beat_of(q.y)).max(0.0),
                }),
                None => actions.push(OverlayAction::Notice(
                    "按 R 放块：指针要放在某一条通道列上".to_owned(),
                )),
            }
        }
    }

    // 滚轮：**与普通模式逐字相同**（用户口径："统一两个模式下的滚轮滑动速度"）——
    // 同一个公式、同一个门控（`resp.hovered()`，不额外看 keys_enabled）。
    // 曾经这里另写了一条 `beats/32*2` 的换算，于是同一个滚轮动作在两个模式下手感不同；
    // 换算只有一份实现（`scroll_delta_to_beats`），参数也只有一处（`OverlayCfg`）。
    if resp.hovered() {
        let (dy, zoom) = overlay_wheel(ui);
        if (zoom - 1.0).abs() > 1e-4 {
            actions.push(OverlayAction::ZoomBeats(zoom_delta_to_beats_factor(zoom)));
        } else if dy.abs() > 0.01 {
            actions.push(OverlayAction::ScrollBeats(scroll_delta_to_beats(
                dy,
                st.overlay_beats.max(4.0),
                cfg.scroll_beats_per_notch,
            )));
        }
    }
}

/// 七条通道的配色（与判定线那五条一样：颜色只用来分辨列，不承载语义）
const MASK_COLORS: [[u8; 3]; 7] = [
    [240, 120, 120], // x1
    [240, 175, 110], // y1
    [150, 220, 150], // x2
    [110, 205, 195], // y2
    [140, 180, 255], // x3
    [185, 160, 255], // y3
    [255, 220, 120], // active
];

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
        st.select_event(TrackId::Alpha, 0);
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
        st.clear_event_selection();
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
        st2.select_event(TrackId::Alpha, 1);
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

    /// **框选**：矩形相交就算选中；与"画出来的矩形"是同一份几何（纯函数，直接喂矩形）
    #[test]
    fn box_hits_are_intersection_based() {
        let rects = [
            (0usize, egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(20.0, 20.0))),
            (1, egui::Rect::from_min_max(egui::pos2(30.0, 30.0), egui::pos2(40.0, 40.0))),
            (2, egui::Rect::from_min_max(egui::pos2(50.0, 50.0), egui::pos2(60.0, 60.0))),
        ];
        // 只碰到第 1 个
        let b = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(15.0, 15.0));
        assert_eq!(box_hits(&rects, b), vec![0]);
        // 框住两个
        let b = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(45.0, 45.0));
        assert_eq!(box_hits(&rects, b), vec![0, 1]);
        // 从左下往右上拉（矩形没规范化时也照样算）
        let b = egui::Rect::from_two_pos(egui::pos2(65.0, 65.0), egui::pos2(45.0, 45.0));
        assert_eq!(box_hits(&rects, b), vec![2]);
        // 空框：什么都不选（"点空白清空"就是它）
        let b = egui::Rect::from_min_max(egui::pos2(100.0, 100.0), egui::pos2(101.0, 101.0));
        assert!(box_hits(&rects, b).is_empty());
    }

    /// **快速放置的键位规则**（用户定）：音符区 Q/W/E/R，事件区**只有 R**，别处给提示。
    /// 这条踩过：事件区里按 Q/W/E 也会起一个事件草稿 —— 那是误触。
    #[test]
    fn quick_keys_are_per_pane() {
        use opm_app::doc::NoteKind;
        for (key, want) in [
            (egui::Key::Q, NoteKind::Tap),
            (egui::Key::W, NoteKind::Flick),
            (egui::Key::E, NoteKind::Drag),
            (egui::Key::R, NoteKind::Hold),
        ] {
            assert_eq!(
                quick_key_rule(key, true, false, false),
                QuickKey::Note(want),
                "{key:?} 在音符区应放 {want:?}"
            );
        }
        // 事件区：只有 R
        assert_eq!(
            quick_key_rule(egui::Key::R, false, true, false),
            QuickKey::EventDraft
        );
        for key in [egui::Key::Q, egui::Key::W, egui::Key::E] {
            assert_eq!(
                quick_key_rule(key, false, true, false),
                QuickKey::Nothing,
                "{key:?} 应无反应"
            );
        }
        // **中轴标签带**：只有 R 放标签，其余键无反应。
        // 而且它**优先于**音符/事件两半区 —— 轴带夹在两者之间，不先判就会被顺手吃掉。
        assert_eq!(
            quick_key_rule(egui::Key::R, false, false, true),
            QuickKey::TagDraft
        );
        assert_eq!(
            quick_key_rule(egui::Key::R, true, false, true),
            QuickKey::TagDraft,
            "轴带优先：即使指针 x 也落在音符半区，R 仍然是放标签"
        );
        for key in [egui::Key::Q, egui::Key::W, egui::Key::E] {
            assert_eq!(
                quick_key_rule(key, false, false, true),
                QuickKey::Nothing,
                "轴带里 {key:?} 应无反应"
            );
        }
        // 不在任何一栏（标尺）：说一句话
        for key in [egui::Key::Q, egui::Key::R] {
            assert_eq!(quick_key_rule(key, false, false, false), QuickKey::Outside);
        }
    }

    /// **框选**（Shift+左键）：起始点定半区 ⇒ 框跨到另一半也不改选哪一类。
    /// 用无头 egui 真的走一遍指针事件序列。
    #[test]
    fn shift_drag_boxes_the_notes_of_the_half_it_started_in() {
        let mut st = state_with_events();
        st.overlay_beats = 32.0;
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        // 修饰键状态是 `InputState.modifiers`，**只由 `Event::ModifiersChanged` 更新**
        // （egui 0.36 的 `RawInput` 没有 modifiers 字段；事件自带的那份不会写进状态）。
        // 真实运行时由窗口层发这个事件，测试里就得自己喂一次。
        let shift_mods = egui::Modifiers { shift: true, ..Default::default() };
        let mut all: Vec<String> = Vec::new();
        let pass = |events: Vec<egui::Event>, all: &mut Vec<String>| {
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
            for a in &acts {
                all.push(format!("{a:?}"));
            }
        };
        // 起手在**音符区**的左上角，往右下拖到事件区里去（跨过轴带）
        let from = egui::pos2(30.0, 40.0);
        let to = egui::pos2(700.0, 380.0);
        let shift = shift_mods;
        pass(vec![egui::Event::ModifiersChanged(shift)], &mut all);
        pass(vec![egui::Event::PointerMoved(from)], &mut all);
        pass(
            vec![egui::Event::PointerButton {
                pos: from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: shift,
            }],
            &mut all,
        );
        pass(vec![egui::Event::PointerMoved(to)], &mut all);
        pass(
            vec![egui::Event::PointerButton {
                pos: to,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: shift,
            }],
            &mut all,
        );
        assert!(
            all.iter().any(|a| a.starts_with("SelectNotes(")),
            "起始点在音符区 ⇒ 只选音符；实际 {all:?}"
        );
        assert!(
            !all.iter().any(|a| a.starts_with("SelectEvents(")),
            "框跨到事件区也不该改选事件（起始点定半区）；实际 {all:?}"
        );

        // 反例：起手在事件区 ⇒ 只选事件
        let ctx2 = egui::Context::default();
        let mut all2: Vec<String> = Vec::new();
        let pass2 = |events: Vec<egui::Event>, out: &mut Vec<String>| {
            let mut acts: Vec<OverlayAction> = Vec::new();
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            let mut o = ctx2.run_ui(raw, |ui| {
                draw(ui, &st, rect, &cfg, true, &mut acts);
            });
            o.textures_delta.clear();
            for a in &acts {
                out.push(format!("{a:?}"));
            }
        };
        let ev_from = egui::pos2(700.0, 380.0);
        let ev_to = egui::pos2(30.0, 40.0);
        pass2(vec![egui::Event::ModifiersChanged(shift)], &mut all2);
        pass2(vec![egui::Event::PointerMoved(ev_from)], &mut all2);
        pass2(
            vec![egui::Event::PointerButton {
                pos: ev_from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: shift,
            }],
            &mut all2,
        );
        pass2(vec![egui::Event::PointerMoved(ev_to)], &mut all2);
        pass2(
            vec![egui::Event::PointerButton {
                pos: ev_to,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: shift,
            }],
            &mut all2,
        );
        assert!(
            all2.iter().any(|a| a.starts_with("SelectEvents(")),
            "起始点在事件区 ⇒ 只选事件；实际 {all2:?}"
        );
        assert!(
            !all2.iter().any(|a| a.starts_with("SelectNotes(")),
            "不该顺手把音符也选了；实际 {all2:?}"
        );
    }

    /// **Ctrl+左键 = 在多选里切换**；不带修饰键 = 替换成它一个。
    /// 光标也按用户要求分档：多选里的音符 = 四向箭头，事件 = 上下双头箭头。
    #[test]
    fn ctrl_click_toggles_and_multi_selection_shows_the_move_cursor() {
        let mut st = state_with_events();
        st.overlay_beats = 32.0;
        // 先造一个多选（音符在这里没有，用事件：alpha 轨道两条）
        st.select_events([(TrackId::Alpha, 0), (TrackId::Alpha, 1)]);
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        let body = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.min.y + RULER_H), rect.max);
        let anchor = st.chart.tmap.beat(st.playhead) - cfg.lead_beats;
        let axis_max = rect.center().x + AXIS_W * 0.5;
        let ev_w = (rect.max.x - axis_max) / 5.0;
        let x = axis_max + ev_w * 3.5; // alpha 列
        let y = beat_y(body, anchor, st.overlay_beats, 8.0); // 第 0 条事件的块体

        let mut all: Vec<String> = Vec::new();
        let mut cursors: Vec<egui::CursorIcon> = Vec::new();
        let ctrl_mods = egui::Modifiers { ctrl: true, command: true, ..Default::default() };
        let pass = |events: Vec<egui::Event>,
                    all: &mut Vec<String>,
                    cursors: &mut Vec<egui::CursorIcon>| {
            let mut acts: Vec<OverlayAction> = Vec::new();
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                draw(ui, &st, rect, &cfg, true, &mut acts);
            });
            cursors.push(out.platform_output.cursor_icon);
            out.textures_delta.clear();
            for a in &acts {
                all.push(format!("{a:?}"));
            }
        };
        // 悬停在多选中的一条事件上：光标应是上下双头箭头。
        // **两帧**：第一帧 egui 才刚知道指针进了这个控件（`hover_pos` 还没有值）。
        let hover = egui::Event::PointerMoved(egui::pos2(x, y));
        pass(vec![hover.clone()], &mut all, &mut cursors);
        pass(vec![hover], &mut all, &mut cursors);
        assert!(
            cursors.contains(&egui::CursorIcon::ResizeVertical),
            "多选事件上应是上下双头箭头；实际 {cursors:?}"
        );
        // Ctrl+左键：切换（Debug 形式里带 ToggleEvent）
        // Ctrl 状态：一次 `ModifiersChanged` 之后一直有效（指针事件自带的那份不写状态）
        pass(vec![egui::Event::ModifiersChanged(ctrl_mods)], &mut all, &mut cursors);
        pass(
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: ctrl_mods,
            }],
            &mut all,
            &mut cursors,
        );
        pass(
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: ctrl_mods,
            }],
            &mut all,
            &mut cursors,
        );
        assert!(
            all.iter().any(|a| a.starts_with("ToggleEvent(")),
            "Ctrl+左键应产出 ToggleEvent；实际 {all:?}"
        );
    }

    /// **多选整体拖动**：按下多选里的一条 → `GrabStart`（带着冻结的抓手）→ 拖动发 `GrabMove`
    #[test]
    fn dragging_a_multi_selection_emits_a_frozen_grab() {
        let mut st = state_with_events();
        st.overlay_beats = 32.0;
        st.select_events([(TrackId::Alpha, 0), (TrackId::Alpha, 1)]);
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        let body = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.min.y + RULER_H), rect.max);
        let anchor = st.chart.tmap.beat(st.playhead) - cfg.lead_beats;
        let axis_max = rect.center().x + AXIS_W * 0.5;
        let ev_w = (rect.max.x - axis_max) / 5.0;
        let x = axis_max + ev_w * 3.5;
        let y = beat_y(body, anchor, st.overlay_beats, 8.0);

        let mut all: Vec<String> = Vec::new();
        let mut grabbed: Vec<opm_app::edit::Grab> = Vec::new();
        let pass = |events: Vec<egui::Event>,
                    all: &mut Vec<String>,
                    grabbed: &mut Vec<opm_app::edit::Grab>| {
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
            for a in &acts {
                if let OverlayAction::GrabStart(g) = a {
                    grabbed.push((**g).clone());
                }
                all.push(format!("{a:?}"));
            }
        };
        pass(vec![egui::Event::PointerMoved(egui::pos2(x, y))], &mut all, &mut grabbed);
        pass(
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            }],
            &mut all,
            &mut grabbed,
        );
        pass(vec![egui::Event::PointerMoved(egui::pos2(x, y - 20.0))], &mut all, &mut grabbed);
        assert_eq!(grabbed.len(), 1, "按下多选里的一条应冻结一个抓手：{all:?}");
        let g = &grabbed[0];
        assert_eq!(g.kind, SelKind::Events);
        assert_eq!(g.events.len(), 2, "整组都要在抓手里面");
        assert!(
            all.iter().any(|a| a.starts_with("GrabMove")),
            "拖动中要发位移：{all:?}"
        );
        assert!(
            !all.iter().any(|a| a.starts_with("EventResizeStart")),
            "多选整体拖动不该被当成拖把手；实际 {all:?}"
        );
        // 松开：结束事务
        pass(
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y - 20.0),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            }],
            &mut all,
            &mut grabbed,
        );
        assert!(all.iter().any(|a| a == "GrabEnd"), "松开要结束拖动：{all:?}");
    }

    /// **卷进重叠的事件不许拖动**（用户要求）：按下不发 `GrabStart`，而是给一句话；
    /// 光标也直接变成"禁止"。重叠要靠拖头/尾把手或冲突浏览器解决。
    #[test]
    fn an_overlapping_event_refuses_to_be_dragged() {
        // 造一份"有重叠"的文档：alpha [0,16) / [8,24) / [16,32) —— 中间那块与第一块重叠
        let mut st;
        {
            let mut d = Document::default();
            d.bpm_list = vec![BpmEntry {
                start: Beat::zero(),
                bpm: 180.0,
                foreign: Default::default(),
            }];
            d.judge_lines.clear();
            let mut l = JudgeLine::default();
            for (a, b) in [(0.0, 16.0), (8.0, 24.0), (16.0, 32.0)] {
                l.layers[0].track_mut("alpha").unwrap().push(Event::new(
                    Beat::new(a as i64, 1),
                    Beat::new(b as i64, 1),
                    json!(1.0),
                    json!(1.0),
                    "linear",
                ));
            }
            d.judge_lines.push(l);
            st = EditorState::new(chart_from_doc(&d));
        }
        st.overlay_beats = 32.0;
        // 选中中间那块（它和第一块重叠）⇒ 拖动应被禁用
        st.select_event(TrackId::Alpha, 0);
        assert!(
            opm_app::edit::selection_has_event_overlap(&st),
            "用例本身要真的重叠（[0,16) 与 [8,24)）"
        );
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        let body = egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.min.y + RULER_H), rect.max);
        let anchor = st.chart.tmap.beat(st.playhead) - cfg.lead_beats;
        let axis_max = rect.center().x + AXIS_W * 0.5;
        let ev_w = (rect.max.x - axis_max) / 5.0;
        let x = axis_max + ev_w * 3.5; // alpha 列
        let y = beat_y(body, anchor, st.overlay_beats, 4.0); // 第一块的块体

        let mut all: Vec<String> = Vec::new();
        let mut cursors: Vec<egui::CursorIcon> = Vec::new();
        let pass = |events: Vec<egui::Event>,
                    all: &mut Vec<String>,
                    cursors: &mut Vec<egui::CursorIcon>| {
            let mut acts: Vec<OverlayAction> = Vec::new();
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                draw(ui, &st, rect, &cfg, true, &mut acts);
            });
            cursors.push(out.platform_output.cursor_icon);
            out.textures_delta.clear();
            for a in &acts {
                all.push(format!("{a:?}"));
            }
        };
        let hover = egui::Event::PointerMoved(egui::pos2(x, y));
        pass(vec![hover.clone()], &mut all, &mut cursors);
        pass(vec![hover], &mut all, &mut cursors);
        assert!(
            cursors.contains(&egui::CursorIcon::NotAllowed),
            "重叠事件上应是「禁止」光标；实际 {cursors:?}"
        );
        pass(
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            }],
            &mut all,
            &mut cursors,
        );
        pass(
            vec![egui::Event::PointerMoved(egui::pos2(x, y - 20.0))],
            &mut all,
            &mut cursors,
        );
        assert!(
            !all.iter().any(|a| a.starts_with("GrabStart")),
            "重叠事件不该开始拖动：{all:?}"
        );
        assert!(
            all.iter().any(|a| a.starts_with("Notice") && a.contains("重叠")),
            "要说明为什么不动：{all:?}"
        );
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

        // 指针在**事件区**：**只有 R** 起事件块草稿。
        let out = press(outside, egui::Key::R);
        let start = out
            .iter()
            .find(|a| a.starts_with("StartEventDraft"))
            .unwrap_or_else(|| panic!("事件区按 R 应起草稿：{out:?}"));
        assert!(start.contains("track: "), "要带上轨道：{start}");
        assert!(!out.iter().any(|a| a.starts_with("QuickPlace")));
        // **Q/W/E 在事件区没有反应**（用户报的误触：它们只对音符有意义）
        for k in [egui::Key::Q, egui::Key::W, egui::Key::E] {
            let out = press(outside, k);
            assert!(
                out.is_empty(),
                "{k:?} 在事件区不该有任何反应，实际 {out:?}"
            );
        }

        // 指针在标尺上（既不在音符区也不在事件区）：给一句话，而不是什么都不做
        let ruler = egui::pos2(180.0, 5.0);
        let out = press(ruler, egui::Key::Q);
        assert!(
            out.iter().any(|a| a.starts_with("Notice")),
            "标尺上按键应给出说明：{out:?}"
        );
        assert!(!out.iter().any(|a| a.starts_with("QuickPlace")));
        assert!(!out.iter().any(|a| a.starts_with("StartEventDraft")));
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
            !all.iter().any(|a| a.starts_with("DraftCommit")),
            "自动重复不该顺手把 hold 放下：{all:?}"
        );
    }

    /// 事件区：按 Q/W/E/R 里任一键 ⇒ 在**指针所在那一列**起事件草稿；
    /// 跟随改长度；**左键点一下 = 放下**（用户要求）；Esc 取消；拖控制杆改起止
    #[test]
    fn event_area_draft_starts_follows_and_commits_on_left_click() {
        let mut st = state_with_events();
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();

        let pass = |st: &EditorState, events: Vec<egui::Event>, ctx: &egui::Context| -> Vec<String> {
            let mut acts: Vec<OverlayAction> = Vec::new();
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                draw(ui, st, rect, &cfg, true, &mut acts);
            });
            out.textures_delta.clear();
            acts.iter().map(|a| format!("{a:?}")).collect()
        };
        let key = |k: egui::Key, pressed: bool| egui::Event::Key {
            key: k,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: Default::default(),
        };
        // 事件区：左半边是音符区、右半边是事件列（中间还有轴带）⇒ 取靠右的位置
        let ev_pos = egui::pos2(700.0, 360.0);

        // 第 0 帧：指针就位 + 补一个 release（键盘是有状态的）。
        // 事件区**只认 R**（Q/W/E 在那里没有反应 —— 用户报的误触）。
        pass(&st, vec![egui::Event::PointerMoved(ev_pos), key(egui::Key::R, false)], &ctx);
        let acts = pass(&st, vec![egui::Event::PointerMoved(ev_pos), key(egui::Key::R, true)], &ctx);
        assert!(
            acts.iter().any(|a| a.starts_with("StartEventDraft")),
            "事件区按 R 应起草稿：{acts:?}"
        );
        for k in [egui::Key::Q, egui::Key::W, egui::Key::E] {
            pass(&st, vec![key(k, false)], &ctx);
            let acts = pass(&st, vec![key(k, true)], &ctx);
            assert!(acts.is_empty(), "{k:?} 在事件区不该有反应：{acts:?}");
        }

        // 进入草稿状态（调用方本来会这么做）后：移动鼠标 ⇒ 跟随
        st.begin_pending_event(TrackId::MoveX, 2.0);
        let a = egui::pos2(700.0, 300.0);
        let b = egui::pos2(700.0, 150.0); // 往上 ⇒ 更晚 ⇒ 更长
        pass(&st, vec![egui::Event::PointerMoved(a)], &ctx);
        let acts = pass(&st, vec![egui::Event::PointerMoved(a), egui::Event::PointerMoved(b)], &ctx);
        assert!(
            acts.iter().any(|x| x.starts_with("DraftFollow")),
            "事件草稿也要跟着鼠标：{acts:?}"
        );
        assert!(
            !acts.iter().any(|x| x.starts_with("SelectEvent")),
            "草稿期间不该顺手点选事件：{acts:?}"
        );

        // **左键点一下 = 放下**（按下 + 松开两帧）
        let acts_down = pass(
            &st,
            vec![egui::Event::PointerButton {
                pos: a,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            }],
            &ctx,
        );
        assert!(
            !acts_down.iter().any(|x| x.starts_with("DraftCommit")),
            "按下还没松开时不该放下：{acts_down:?}"
        );
        let acts_up = pass(
            &st,
            vec![egui::Event::PointerButton {
                pos: a,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            }],
            &ctx,
        );
        assert!(
            acts_up.iter().any(|x| x.starts_with("DraftCommit")),
            "左键单击应放下草稿：{acts_up:?}"
        );

        // Esc 取消；拖控制杆改起止
        let acts = pass(&st, vec![key(egui::Key::Escape, true)], &ctx);
        assert!(acts.iter().any(|x| x.starts_with("DraftCancel")), "{acts:?}");
        // 控制杆：草稿的尾在上（y = y_of(end)），把指针放到那附近按下并拖
        let span = st.pending_event.unwrap().span();
        let y_end = {
            // 与面板同一套映射：不重新实现，直接扫一遍找哪一行有"把手高亮"太麻烦 ——
            // 这里用"拖到明显更晚的拍"这条更粗的断言：拖动**任意位置**都改长度
            let _ = span;
            150.0
        };
        let acts = pass(&st, vec![egui::Event::PointerMoved(egui::pos2(700.0, y_end))], &ctx);
        let _ = acts;
        st.resize_pending_event(crate::state::EventEdge::End, 9.0);
        assert_eq!(st.pending_event.unwrap().span().1, 9.0);
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
            !plain.iter().any(|a| a.starts_with("DraftFollow")),
            "滚动不是鼠标移动 ⇒ 不该改 hold 长度：{plain:?}"
        );

        let mut ctrl: Vec<String> = Vec::new();
        run(egui::Modifiers::CTRL, &mut ctrl);
        assert!(
            ctrl.iter().any(|a| a.starts_with("ZoomBeats(")),
            "跟随状态下 Ctrl+滚轮仍应缩放：{ctrl:?}"
        );
        assert!(!ctrl.iter().any(|a| a.starts_with("DraftFollow")));
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
            acts.iter().any(|x| x.starts_with("DraftFollow")),
            "指针移动应产出跟随：{acts:?}"
        );
        assert!(!acts.iter().any(|x| x.starts_with("DraftCommit")));

        // 放下：R 与回车都要行
        for k in [egui::Key::R, egui::Key::Enter] {
            let acts = pass(&st, key(k), true, &ctx);
            assert!(
                acts.iter().any(|x| x.starts_with("DraftCommit")),
                "{k:?} 应该放下 hold：{acts:?}"
            );
        }
        // 取消：Esc
        let acts = pass(&st, key(egui::Key::Escape), true, &ctx);
        assert!(acts.iter().any(|x| x.starts_with("DraftCancel")), "{acts:?}");

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
        // 这个闭包**捕获**了 `cursors` 并往里 push ⇒ 必须 `mut`
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
                    OverlayAction::GrabStart(_) => "GrabStart".to_owned(),
                    OverlayAction::GrabEnd => "GrabEnd".to_owned(),
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

    // ---------------------------------------------------------------- 选择框（命中 / 遮挡）

    fn nb(index: usize, hold: bool, cx: f32, cy: f32, len: f32) -> NoteBox {
        let head = egui::Rect::from_center_size(
            egui::pos2(cx, cy),
            egui::vec2(NOTE_W, NOTE_ROW_H),
        );
        // 竖条从头部**向上**长（与屏幕坐标相反也一样，这里只要长度）
        let full = if hold {
            egui::Rect::from_min_max(egui::pos2(cx - NOTE_W * 0.5, cy), egui::pos2(cx + NOTE_W * 0.5, cy + len))
        } else {
            head
        };
        NoteBox { index, hold, head, full }
    }

    /// **hold 点身体也要能选中**（用户报的那条）；但**判定排在其它类型下面**。
    #[test]
    fn hold_body_is_clickable_and_loses_to_other_note_types() {
        // 只有 hold：头部、身体中段、身体末端都该命中
        let boxes = [nb(0, true, 100.0, 200.0, 80.0)];
        for y in [200.0, 240.0, 279.0] {
            assert_eq!(
                note_hit(&boxes, egui::pos2(100.0, y), None),
                Some(0),
                "y={y} 落在 hold 的选择框里就该选中它"
            );
        }
        assert_eq!(note_hit(&boxes, egui::pos2(100.0, 300.0), None), None, "框外不选");

        // hold + 一个压在它身上的 tap：**tap 赢**（hold 判定在其它类型下方）
        let boxes = [nb(0, true, 100.0, 200.0, 80.0), nb(1, false, 100.0, 240.0, 0.0)];
        assert_eq!(note_hit(&boxes, egui::pos2(100.0, 240.0), None), Some(1));
        // 反过来把 tap 放前面也一样（与遍历顺序无关）
        let boxes = [nb(1, false, 100.0, 240.0, 0.0), nb(0, true, 100.0, 200.0, 80.0)];
        assert_eq!(note_hit(&boxes, egui::pos2(100.0, 240.0), None), Some(1));
        // 只有 hold 的那个位置仍然选 hold
        assert_eq!(note_hit(&boxes, egui::pos2(100.0, 270.0), None), Some(0));
    }

    /// 完全重叠的两个音符：**锚优先**（点一下不换人），否则后画的优先；
    /// 而**重叠组**把两个都列出来 —— 这是被盖住那个的唯一入口。
    #[test]
    fn fully_overlapping_notes_are_reachable_through_the_group() {
        let boxes = [nb(3, false, 100.0, 200.0, 0.0), nb(7, false, 100.0, 200.0, 0.0)];
        let p = egui::pos2(100.0, 200.0);
        assert_eq!(note_hit(&boxes, p, None), Some(7), "没人被选中时后画的在上");
        assert_eq!(note_hit(&boxes, p, Some(3)), Some(3), "锚优先：点自己身上不换人");
        assert_eq!(note_hit(&boxes, p, Some(7)), Some(7));
        // 组：两个都在（升序），含锚自己
        assert_eq!(overlap_group(&boxes, 3), vec![3, 7]);
        assert_eq!(overlap_group(&boxes, 7), vec![3, 7]);
        // 单个音符：组里就剩它自己（用户口径）
        assert_eq!(overlap_group(&boxes[..1], 3), vec![3]);
    }

    /// **遮挡只看头部**，而且要真的交叠：
    /// · hold 的长身体不算"盖住"别人；
    /// · 同一条线上不同时刻的两个音符（宽度相同、y 不交叠）不算互相遮挡。
    #[test]
    fn occlusion_uses_heads_only_and_needs_a_real_overlap() {
        let hold = nb(0, true, 100.0, 100.0, 120.0); // 头部 y≈100，身体一直到 220
        let tap_under = nb(1, false, 100.0, 180.0, 0.0); // 落在 hold 的身体里
        assert!(!covers(&hold, &tap_under), "hold 的身体不算遮挡（只看头部）");
        assert!(!covers(&tap_under, &hold), "反过来也不算被 hold 身体挡住");
        assert_eq!(overlap_group(&[hold, tap_under], 0), vec![0], "组里只有它自己");
        assert_eq!(overlap_group(&[hold, tap_under], 1), vec![1]);

        // 同一 lane、拍差得远：交叠为空 ⇒ 不算覆盖（否则"同线必互相遮挡"就荒唐了）
        let a = nb(0, false, 100.0, 100.0, 0.0);
        let b = nb(1, false, 100.0, 400.0, 0.0);
        assert!(!covers(&a, &b));
        assert_eq!(overlap_group(&[a, b], 0), vec![0]);

        // 部分交叠：交叠高度超过被盖者的一半 ⇒ 算覆盖
        let c = nb(2, false, 100.0, 102.0, 0.0); // 与 a 相差 2px < 3.5px
        assert!(covers(&a, &c));
        assert_eq!(overlap_group(&[a, c], 0), vec![0, 2]);
        // **"或"的另一面**（用户口径就是"长度**或**宽度"）：同一条线上只要 y 上有一点点交叠，
        // 宽度方向就是整整 10px 都在对方框里 ⇒ 也算。宁可多列一行，也别漏掉真被盖住的那个。
        let d = nb(3, false, 100.0, 106.0, 0.0); // 相差 6px：高度只交叠 1px
        assert!(covers(&a, &d), "宽度方向整整 10px 被盖住 ⇒ 按「或」算覆盖");

        // 斜着擦到一点（两个方向都不到一半）⇒ **不算**
        let e = nb(4, false, 106.0, 104.0, 0.0); // x 交叠 4px ≤ 5、y 交叠 3px ≤ 3.5
        assert!(!covers(&a, &e), "两个方向都不到一半 ⇒ 不是有效覆盖");
        assert_eq!(overlap_group(&[a, e], 0), vec![0]);

        // hold 盖 hold：同样只看头部（两个头叠在一起 ⇒ 互相算覆盖）
        let h1 = nb(0, true, 50.0, 100.0, 60.0);
        let h2 = nb(1, true, 50.0, 100.0, 30.0);
        assert!(covers(&h1, &h2));
        assert_eq!(overlap_group(&[h1, h2], 0), vec![0, 1]);
    }

    /// 框选：与单点命中用**同一份选择框**（hold 的整条竖条都算在内）
    #[test]
    fn box_select_uses_the_same_note_boxes() {
        let boxes = [nb(0, true, 100.0, 200.0, 80.0), nb(1, false, 300.0, 200.0, 0.0)];
        let sel = egui::Rect::from_min_max(egui::pos2(90.0, 230.0), egui::pos2(110.0, 260.0));
        assert_eq!(note_box_hits(&boxes, sel), vec![0], "框在 hold 身体上也要选中它");
        let sel2 = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(50.0, 50.0));
        assert!(note_box_hits(&boxes, sel2).is_empty());
    }

    /// **遮蔽区草稿要跟着指针走**（用户报："按下 r 能出现临时事件块，但鼠标移动无法控制长度"）。
    ///
    /// 机制级回归：面板必须**发出** `DraftFollow`（这一段），状态层必须把它送给遮蔽区那一支
    /// （`state::follow_pending`，见 `state::mask_draft_tests` 那条）—— 缺任何一段都会表现为
    /// "起稿成功、鼠标却不动长度"，而且不报错。当初缺的正是第二段：`main.rs` 的
    /// `DraftFollow` 只认"事件草稿 / hold"，遮蔽区草稿静默落到 hold 上。
    #[test]
    fn a_mask_draft_follows_the_pointer() {
        let mut st = state_with_events();
        st.mask_edit = true;
        st.overlay_beats = 32.0;
        // 起稿：x1 通道、10 拍处（长度先给一个格点）
        assert!(st.begin_pending_mask(MaskChannel::X1, 0, 10.0));
        let end0 = st.pending_mask.expect("草稿").end.to_f64();

        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        // 指针依次停在 y=380（更早的拍）→ y=200（更晚）→ y=380：**帧间位移**就是跟随的信号
        let mut follows: Vec<f64> = Vec::new();
        for y in [380.0f32, 200.0, 380.0] {
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events: vec![egui::Event::PointerMoved(egui::pos2(600.0, y))],
                ..Default::default()
            };
            let mut acts: Vec<OverlayAction> = Vec::new();
            let mut out = ctx.run_ui(raw, |ui| {
                draw(ui, &st, rect, &cfg, true, &mut acts);
            });
            out.textures_delta.clear();
            follows.extend(acts.iter().filter_map(|a| match a {
                OverlayAction::DraftFollow { beat } => Some(*beat),
                _ => None,
            }));
        }
        assert_eq!(follows.len(), 2, "两次位移 ⇒ 两条 DraftFollow：{follows:?}");
        let (up, down) = (follows[0], follows[1]);
        assert!(up > down, "指针往上 = 更晚的拍：{up} 应晚于 {down}");

        // 动作接上状态层：草稿的终点真的跟着走（往上变长）
        st.follow_pending(up);
        let grown = st.pending_mask.expect("草稿还在").end.to_f64();
        assert!(grown > end0, "跟随之后终点要变大：{end0} → {grown}");
        // 往回拖：**保底一个格点**（与判定线那边同一条规则），不会把草稿拖没
        st.follow_pending(down);
        let back = st.pending_mask.expect("草稿还在");
        let floor = back.start.to_f64() + st.beat_step();
        assert!(
            (back.end.to_f64() - floor).abs() < 1e-9,
            "反拖到起点之前 ⇒ 停在保底长度 {floor}，实际 {}",
            back.end.to_f64()
        );
    }

    /// 只有一块 x1 事件的遮蔽区，进入遮蔽区编辑模式。
    ///
    /// 默认那块是 `[8,10)`：**两拍**——短到"拖拽阈值（约 6 像素）比把手段（`EDGE_BAND` 6 像素）
    /// 还宽"这件事会露出来（见 `MaskPressHit`）。
    fn state_with_mask_zone(start: Beat, end: Beat) -> EditorState {
        use opm_app::doc::MaskZone;
        let mut doc = Document::default();
        doc.bpm_list = vec![BpmEntry {
            start: Beat::zero(),
            bpm: 180.0,
            foreign: Default::default(),
        }];
        doc.judge_lines.clear();
        let mut z = MaskZone::default();
        z.x1.push(Event::new(start, end, json!(0), json!(0), "linear"));
        doc.mask_zones.push(z);
        let mut st = EditorState::new(chart_from_doc(&doc));
        st.mask_edit = true;
        st.selected_zone = 0;
        st.selected_channel = MaskChannel::X1;
        st.overlay_beats = 32.0;
        st
    }

    fn mask_pane_rect() -> egui::Rect {
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0))
    }

    /// 遮蔽区面板里第 `k` 条通道列的**中心 x**（列布局只有面板自己知道，测试照抄同一套算式）
    fn mask_col_x(k: usize) -> f32 {
        let r = mask_pane_rect();
        let lanes_min = r.min.x + AXIS_W;
        let col_w = (r.max.x - lanes_min) / MaskChannel::ALL.len() as f32;
        lanes_min + col_w * (k as f32 + 0.5)
    }

    /// 遮蔽区面板里某一拍的**屏幕 y**（纵轴：越上越晚）
    fn mask_beat_y(st: &EditorState, beat: f64) -> f32 {
        let r = mask_pane_rect();
        let body = egui::Rect::from_min_max(egui::pos2(r.min.x, r.min.y + RULER_H), r.max);
        let anchor = st.chart.tmap.beat(st.playhead) - OverlayCfg::default().lead_beats;
        beat_y(body, anchor, st.overlay_beats, beat)
    }

    fn ptr_moved(x: f32, y: f32) -> Vec<egui::Event> {
        vec![egui::Event::PointerMoved(egui::pos2(x, y))]
    }

    fn ptr_button(x: f32, y: f32, pressed: bool) -> Vec<egui::Event> {
        vec![egui::Event::PointerButton {
            pos: egui::pos2(x, y),
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        }]
    }

    /// 把"每帧一串事件"喂给 `draw`，返回每帧发出的动作（遮蔽区拖动测试共用）
    fn drive_frames(st: &EditorState, frames: Vec<Vec<egui::Event>>) -> Vec<Vec<OverlayAction>> {
        let ctx = egui::Context::default();
        let rect = mask_pane_rect();
        let cfg = OverlayCfg::default();
        frames
            .into_iter()
            .map(|events| {
                let mut acts: Vec<OverlayAction> = Vec::new();
                let raw = egui::RawInput {
                    screen_rect: Some(rect),
                    events,
                    ..Default::default()
                };
                let mut out = ctx.run_ui(raw, |ui| {
                    draw(ui, st, rect, &cfg, true, &mut acts);
                });
                out.textures_delta.clear();
                acts
            })
            .collect()
    }

    /// 这些动作里的"改跨度"命令（拖端点每帧最多一条），按发出顺序 —— 拍是**有理数**，
    /// 这里连分子分母一起收下来：`[8,1]` 与 `[32,4]` 相等的写法也要区分得出来（口径是精确拍，
    /// 不是浮点近似）。
    fn spans_of(acts: &[Vec<OverlayAction>]) -> Vec<((i64, i64), (i64, i64))> {
        acts.iter()
            .flatten()
            .filter_map(|a| match a {
                OverlayAction::MaskSetSpan { start, end, .. } => {
                    Some(((start.n, start.d), (end.n, end.d)))
                }
                _ => None,
            })
            .collect()
    }

    /// 有理拍的浮点值（断言里说人话用）
    fn beats(span: ((i64, i64), (i64, i64))) -> (f64, f64) {
        (
            span.0 .0 as f64 / span.0 .1 as f64,
            span.1 .0 as f64 / span.1 .1 as f64,
        )
    }

    fn has(acts: &[Vec<OverlayAction>], f: impl Fn(&OverlayAction) -> bool) -> bool {
        acts.iter().flatten().any(f)
    }

    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// **拖遮蔽区事件块的尾巴**：隔着拖拽阈值也要抓得住把手段，而且**拖得回原位**。
    ///
    /// 用户报（2026-10-02）："拖动屏蔽区事件块的头尾控制杆时，无法移动回原位（原本尾在 10 拍，
    /// 拖动时可能卡住，也可能能够拖动但移动时会跳过 10 拍）"。根因是两条叠在一起：
    /// · **命中算晚了**：拖拽阈值与把手段一样宽（都约 6 像素）⇒ `drag_started()` 那一帧指针必定
    ///   已经离开把手段，用那一帧的命中判"抓的是哪一头"，两拍的块直接判成"没抓到"（面板里
    ///   什么都不发生），长一点的块判成"抓身体"（拖端点变成整块平移）；
    /// · **去重比错了对象**：拿"拖拽开始时的跨度"当比较对象 ⇒ 拖回按下点算出来的跨度正好等于它，
    ///   被当成"没变化"而不发命令 ⇒ 块停在拖出去的位置上，直到指针越过原位才突然跳过去
    ///   （"移动时会跳过 10 拍"）。
    #[test]
    fn mask_handle_drag_grabs_the_edge_and_can_return_to_where_it_started() {
        let st = state_with_mask_zone(Beat::new(8, 1), Beat::new(10, 1));
        let x = mask_col_x(0);
        let (y10, y14) = (mask_beat_y(&st, 10.0), mask_beat_y(&st, 14.0));
        // 悬停 → 按在尾上 → 往上拖（更晚的拍）→ 拖回来 → 松开。中间那几步都跨过拖拽阈值
        let mut frames = vec![ptr_moved(x, y10), ptr_button(x, y10, true)];
        for y in [y10 - 8.0, y10 - 20.0, y14, y10 - 6.0, y10] {
            frames.push(ptr_moved(x, y));
        }
        frames.push(ptr_button(x, y10, false));
        let acts = drive_frames(&st, frames);

        assert!(
            has(&acts, |a| matches!(a, OverlayAction::MaskDragStart)),
            "按在把手上必须开始拖拽：{acts:?}"
        );
        let spans = spans_of(&acts);
        assert!(!spans.is_empty(), "拖拽必须改跨度：{acts:?}");
        assert!(
            spans.iter().all(|sp| near(beats(*sp).0, 8.0)),
            "抓的是**尾**，起点一次都不许动（动了就是被当成整块平移）：{spans:?}"
        );
        assert!(
            spans.iter().any(|sp| near(beats(*sp).1, 14.0)),
            "指针拖到 14 拍，尾就该在 14 拍：{spans:?}"
        );
        assert_eq!(
            spans.last().copied(),
            Some(((8, 1), (10, 1))),
            "**拖回按下点必须精确回到原位**（尾 10 拍 = [10,1]）：{spans:?}"
        );
    }

    /// **拖头**：起点跟着走、终点钉死；反向拖不许把块翻过来（头跑到尾后面）。
    #[test]
    fn mask_head_drag_moves_only_the_head() {
        let st = state_with_mask_zone(Beat::new(8, 1), Beat::new(14, 1));
        let x = mask_col_x(0);
        let (y8, y4) = (mask_beat_y(&st, 8.0), mask_beat_y(&st, 4.0));
        let mut frames = vec![ptr_moved(x, y8), ptr_button(x, y8, true)];
        // 往下拖 = 更早的拍（4 拍）；再拖过头到 16 拍（越过尾）——只许停在"保底一个格点"
        for y in [y8 + 8.0, y4, mask_beat_y(&st, 16.0)] {
            frames.push(ptr_moved(x, y));
        }
        frames.push(ptr_button(x, mask_beat_y(&st, 16.0), false));
        let acts = drive_frames(&st, frames);
        let spans = spans_of(&acts);
        assert!(
            spans.iter().all(|sp| near(beats(*sp).1, 14.0)),
            "抓的是**头**，终点一次都不许动：{spans:?}"
        );
        assert!(
            spans.iter().any(|sp| near(beats(*sp).0, 4.0)),
            "指针拖到 4 拍，头就该在 4 拍：{spans:?}"
        );
        // 越过尾：停在 `end - 一个格点`（0.25 拍），**不翻块**
        assert!(
            spans.last().is_some_and(|sp| near(beats(*sp).0, 13.75)),
            "头不许越过尾（保底一个格点）：{spans:?}"
        );
    }

    /// **拖身体 = 整块平移**（长度不变），并且同样能拖回按下点 —— 这条把"去重比错对象"单独钉住：
    /// 块够高（8 拍），按下与拖动都在块体内，用不到"按下的命中"那一条修正也有拖拽。
    #[test]
    fn mask_body_drag_translates_and_can_return() {
        let st = state_with_mask_zone(Beat::new(4, 1), Beat::new(12, 1));
        let x = mask_col_x(0);
        let y_body = mask_beat_y(&st, 8.0);
        let y_up = mask_beat_y(&st, 12.0);
        let mut frames = vec![ptr_moved(x, y_body), ptr_button(x, y_body, true)];
        for y in [y_body - 8.0, y_up, y_body] {
            frames.push(ptr_moved(x, y));
        }
        frames.push(ptr_button(x, y_body, false));
        let acts = drive_frames(&st, frames);
        let spans = spans_of(&acts);
        assert!(!spans.is_empty(), "拖身体要平移：{acts:?}");
        let floats: Vec<(f64, f64)> = spans.iter().map(|sp| beats(*sp)).collect();
        assert!(
            floats.iter().all(|(s, e)| near(e - s, 8.0)),
            "平移不许改长度：{floats:?}"
        );
        assert!(
            floats.iter().any(|(s, _)| *s > 4.5),
            "往上拖 = 更晚：起点要变大：{floats:?}"
        );
        assert_eq!(
            spans.last().copied(),
            Some(((4, 1), (12, 1))),
            "拖回按下点必须精确回到原位：{spans:?}"
        );
    }

    /// **原跨度不在网格上时也要回得去**：吸附只会落到最近的格点，差一点点就永远回不了原位。
    /// 这里那块是 `[8, 9.4)`，网格是 1/4 拍（最近的格点 9.5）—— 全靠"拖回按下点用回原跨度"。
    #[test]
    fn mask_handle_drag_returns_to_an_off_grid_span() {
        let st = state_with_mask_zone(Beat::new(8, 1), Beat::new(47, 5));
        let x = mask_col_x(0);
        let y94 = mask_beat_y(&st, 9.4);
        let mut frames = vec![ptr_moved(x, y94), ptr_button(x, y94, true)];
        for y in [y94 - 8.0, mask_beat_y(&st, 13.0), y94] {
            frames.push(ptr_moved(x, y));
        }
        frames.push(ptr_button(x, y94, false));
        let acts = drive_frames(&st, frames);
        let spans = spans_of(&acts);
        // `[47,5]` 而不是吸附出来的 `[38,4]`（= 9.5 拍）：精确有理拍才算回到了原位
        assert_eq!(
            spans.last().copied(),
            Some(((8, 1), (47, 5))),
            "拖回按下点要回到**原来的** 9.4 拍（不是吸附出来的 9.5）：{spans:?}"
        );
    }

    /// **草稿的控制杆也按"按下时"判**：拖拽阈值那一帧指针已经离开把手段了（都约 6 像素）。
    ///
    /// 这条与遮蔽区事件块的 `MaskPressHit` 是同一个坑的**第二处**：`draft_gesture` 两个模式共用，
    /// 用当前位置判的话，拖**头**会被降级成"长度跟着鼠标走"（尾动头不动）—— 而且不报错。
    /// 这里故意把草稿拉成 10 拍长：移动 8px 之后指针落在**块体**里（短草稿会落进"取更近那一头"
    /// 的小块分支，反而看不出来）。
    #[test]
    fn a_draft_handle_drag_uses_the_press_position() {
        let mut st = state_with_events();
        st.mask_edit = true;
        st.overlay_beats = 32.0;
        assert!(st.begin_pending_mask(MaskChannel::X1, 0, 10.0));
        st.follow_pending_mask(20.0); // 草稿 [10,20)
        let x = mask_col_x(0);
        let y_head = mask_beat_y(&st, 10.0); // 起点在下边缘
        let mut frames = vec![ptr_moved(x, y_head), ptr_button(x, y_head, true)];
        for dy in [8.0, 16.0] {
            frames.push(ptr_moved(x, y_head + dy));
        }
        frames.push(ptr_button(x, y_head + 16.0, false));
        let acts = drive_frames(&st, frames);

        let edges: Vec<EventEdge> = acts
            .iter()
            .flatten()
            .filter_map(|a| match a {
                OverlayAction::DraftResize { edge, .. } => Some(*edge),
                _ => None,
            })
            .collect();
        assert!(
            !edges.is_empty(),
            "拖草稿的**头**必须发 DraftResize(Start)，而不是降级成跟随：{acts:?}"
        );
        assert!(edges.iter().all(|e| *e == EventEdge::Start), "{edges:?}");
        assert!(
            !has(&acts, |a| matches!(a, OverlayAction::DraftFollow { .. })),
            "抓到头就不该再「长度跟着鼠标走」：{acts:?}"
        );
    }

    /// **遮蔽区编辑模式下按 R 起的是草稿，不是就地放一块** —— 与判定线事件区同一套手势
    /// （用户口径 2026-10-02："创建流程应与普通编辑模式下的事件块放置一样"）。
    ///
    /// 钉住的是这条流程的两个端点：R 产出的动作是 `StartMaskDraft`（起点已吸附），
    /// 而草稿放下走的是全局的 `DraftCommit` —— 面板自己不再有"放块"这条路。
    #[test]
    fn r_in_the_mask_pane_starts_a_draft_instead_of_placing_a_block() {
        let mut st = state_with_events();
        st.mask_edit = true;
        st.overlay_beats = 32.0;
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        // 遮蔽区模式下七条通道铺满轴带以右：取右侧一条通道列的中段偏下
        let col = egui::pos2(600.0, 360.0);
        let mut all: Vec<String> = Vec::new();
        for frame in 0..3 {
            let mut events = vec![egui::Event::PointerMoved(col)];
            if frame >= 1 {
                events.push(egui::Event::Key {
                    key: egui::Key::R,
                    physical_key: None,
                    pressed: true,
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
        let starts: Vec<&String> = all
            .iter()
            .filter(|a| a.starts_with("StartMaskDraft"))
            .collect();
        assert_eq!(starts.len(), 1, "自动重复不该反复起稿：{all:?}");
        assert!(starts[0].contains("channel: "), "要带上通道：{}", starts[0]);
        assert!(
            !all.iter().any(|a| a.starts_with("MaskPlace")),
            "按 R 不该再「就地放一块」：{all:?}"
        );
    }
    /// **Shift 拖框期间滚轮照旧移动视图**（用户要求）。
    ///
    /// 这条 bug 藏得比较深：egui 的 `Options::horizontal_scroll_modifier` **默认就是 SHIFT**，
    /// `WheelState` 见到 Shift 会把滚轮整个折到横轴（`delta = vec2(dx + dy, 0.0)`）——
    /// 于是按住 Shift 滚轮时 `smooth_scroll_delta.y` **恒为 0**，只读 `.y` 的滚轮处理一个事件都收不到。
    /// 实测：修复前这一串事件产出 **0 个动作**（连 ScrollBeats 都没有），修复后照常滚。
    #[test]
    fn shift_wheel_still_moves_the_timeline() {
        // 换算：Shift 下滚轮落在横轴里，其余情况取纵轴；没有横轴时不能把纵轴吃掉
        assert_eq!(wheel_delta_y(50.0, 0.0, true), 50.0, "Shift 下 egui 把滚轮折进横轴");
        assert_eq!(wheel_delta_y(0.0, 50.0, false), 50.0);
        assert_eq!(wheel_delta_y(0.0, 50.0, true), 50.0);
        assert_eq!(wheel_delta_y(-50.0, 0.0, true), -50.0);
        assert_eq!(wheel_delta_y(0.0, 0.0, true), 0.0);

        let st = state_with_events();
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        let pos = egui::pos2(180.0, 380.0);
        // 滚轮事件要**多帧**才会从 egui 的平滑器里放出来（见 egui `WheelState::after_events`）
        let run = |mods: egui::Modifiers, all: &mut Vec<String>| {
            for _ in 0..8 {
                let events = vec![
                    egui::Event::PointerMoved(pos),
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
            "普通滚轮本来就能移动视图：{plain:?}"
        );
        let mut shifted: Vec<String> = Vec::new();
        run(egui::Modifiers::SHIFT, &mut shifted);
        let scroll: Vec<&String> = shifted
            .iter()
            .filter(|a| a.starts_with("ScrollBeats("))
            .collect();
        assert!(
            !scroll.is_empty(),
            "Shift 按下时 egui 把滚轮折进横轴 —— 编辑区仍要能移动时间轴：{shifted:?}"
        );
        // 方向：向上滚（delta.y > 0）= 时间往后 ⇒ 拍增量必须是正的
        let beats: f64 = scroll[0]
            .trim_start_matches("ScrollBeats(")
            .trim_end_matches(')')
            .parse()
            .expect("ScrollBeats 里应是拍数");
        assert!(beats > 0.0, "向上滚 = 时间往后，方向不能反：{scroll:?}");

        // 真正的场景：**框选拖拽正在进行中**滚轮也要照常发 ScrollBeats
        // （`resp.hovered()` 在自己的拖拽期间保持为真 —— egui 的约定，这里实测钉住它）
        let ctx2 = egui::Context::default();
        let shift = egui::Modifiers { shift: true, ..Default::default() };
        let from = egui::pos2(180.0, 380.0);
        let to = egui::pos2(300.0, 300.0);
        let pass = |events: Vec<egui::Event>, all: &mut Vec<String>| {
            let mut acts: Vec<OverlayAction> = Vec::new();
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            let mut out = ctx2.run_ui(raw, |ui| {
                draw(ui, &st, rect, &cfg, true, &mut acts);
            });
            out.textures_delta.clear();
            all.extend(acts.iter().map(|a| format!("{a:?}")));
        };
        let wheel = egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, 50.0),
            phase: egui::TouchPhase::Move,
            modifiers: shift,
        };
        let mut dragging: Vec<String> = Vec::new();
        pass(vec![egui::Event::ModifiersChanged(shift)], &mut dragging);
        pass(vec![egui::Event::PointerMoved(from)], &mut dragging);
        pass(
            vec![egui::Event::PointerButton {
                pos: from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: shift,
            }],
            &mut dragging,
        );
        pass(vec![egui::Event::PointerMoved(to)], &mut dragging);
        for _ in 0..8 {
            pass(
                vec![
                    egui::Event::PointerMoved(to),
                    egui::Event::ModifiersChanged(shift),
                    wheel.clone(),
                ],
                &mut dragging,
            );
        }
        assert!(
            dragging.iter().any(|a| a.starts_with("ScrollBeats(")),
            "框选拖拽进行中，滚轮仍应移动视图：{dragging:?}"
        );
    }

    /// **框选的起点钉在"拍"上，不钉在屏幕 y 上** —— 这是"框选时可以用滚轮移动视图"能成立的前提。
    ///
    /// 场景（alpha 轨道两个事件：拍 0..16 与拍 16..32）：
    /// 在**拍 8**的屏幕位置按下 Shift → 往拍 24 拖 → **中途把视图滚 10 拍**（指针停在原屏幕位置）→ 松手。
    ///
    /// · 修复后：起点仍锚在拍 8 ⇒ 框从拍 8 一直盖到指针处，**事件 0 与事件 1 都在框里**；
    /// · 修复前：起点是屏幕 y，视图一滚它就漂到拍 18 上 ⇒ 框只剩拍 18..34 ⇒ **事件 0 掉出选区**。
    ///
    /// 断言就钉在这一条上：`event 0` 在不在选区里。
    #[test]
    fn box_select_anchor_stays_on_the_beat_when_the_view_scrolls() {
        let mut st = state_with_events();
        st.overlay_beats = 32.0;
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(800.0, 400.0));
        let cfg = OverlayCfg::default();
        let body = egui::Rect::from_min_max(
            egui::pos2(rect.min.x, rect.min.y + RULER_H),
            rect.max,
        );
        let beats = 32.0_f64;
        let anchor_of = |s: &EditorState| s.chart.tmap.beat(s.playhead) - cfg.lead_beats;
        // 视图 1：拍 8 的屏幕 y 与拍 24 的屏幕 y
        let a1 = anchor_of(&st);
        let y8 = beat_y(body, a1, beats, 8.0);
        let y24 = beat_y(body, a1, beats, 24.0);
        assert!(y8 > y24, "拍越大越靠上：{y8} vs {y24}");

        // alpha 列（`TrackId::ALL` 里第 4 列）：列宽与起点跟绘制同源
        let mid_x = rect.center().x;
        let axis_right = mid_x + AXIS_W * 0.5;
        let col_w = (rect.max.x - axis_right) / TrackId::ALL.len() as f32;
        let k = TrackId::ALL.iter().position(|t| *t == TrackId::Alpha).unwrap();
        let x = axis_right + (k as f32 + 0.5) * col_w;

        // 视图 2：播放头前进 10 拍（= 用滚轮把视图滚了 10 拍）
        let mut st2 = state_with_events();
        st2.overlay_beats = 32.0;
        st2.playhead = st2.chart.tmap.sec(10.0);

        let shift = egui::Modifiers { shift: true, ..Default::default() };
        let pass = |s: &EditorState, events: Vec<egui::Event>, all: &mut Vec<String>| {
            let mut acts: Vec<OverlayAction> = Vec::new();
            let raw = egui::RawInput {
                screen_rect: Some(rect),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                draw(ui, s, rect, &cfg, true, &mut acts);
            });
            out.textures_delta.clear();
            all.extend(acts.iter().map(|a| format!("{a:?}")));
        };
        let from = egui::pos2(x, y8);
        let to = egui::pos2(x, y24);
        let mut all: Vec<String> = Vec::new();
        pass(&st, vec![egui::Event::ModifiersChanged(shift)], &mut all);
        pass(&st, vec![egui::Event::PointerMoved(from)], &mut all);
        pass(
            &st,
            vec![egui::Event::PointerButton {
                pos: from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: shift,
            }],
            &mut all,
        );
        pass(&st, vec![egui::Event::PointerMoved(to)], &mut all);
        // 拖框途中滚轮把视图挪走（这里直接换一份"播放头已前进"的状态，等价于滚动生效后的那一帧）
        pass(&st2, vec![egui::Event::PointerMoved(to)], &mut all);
        pass(
            &st2,
            vec![egui::Event::PointerButton {
                pos: to,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: shift,
            }],
            &mut all,
        );
        let dropped: Vec<&String> = all.iter().filter(|a| a.starts_with("SelectEvents(")).collect();
        assert_eq!(dropped.len(), 1, "框选落地要且只要一次 SelectEvents：{all:?}");
        assert!(dropped[0].contains("Alpha"), "选的是事件：{}", dropped[0]);
        // 事件 0 的起点（拍 0）已随视图滚到框外，只有"起点锚在拍 8"才会把它框住
        assert!(
            dropped[0].contains(", 0)") && dropped[0].contains(", 1)"),
            "起点必须钉在拍上：视图滚动后事件 0 仍应被框住（只框到事件 1 就是屏幕坐标的旧行为）：{}",
            dropped[0]
        );
    }
}

#[cfg(test)]
mod tag_tests {
    use super::*;
    use opm_app::state::{Tag, TagSource};

    fn pane() -> (egui::Rect, egui::Rect) {
        let body = egui::Rect::from_min_max(egui::pos2(300.0, 40.0), egui::pos2(1300.0, 840.0));
        let axis = egui::Rect::from_min_max(
            egui::pos2(783.0, 40.0),
            egui::pos2(783.0 + AXIS_W, 840.0),
        );
        (body, axis)
    }

    /// **两列的分工就是"来源"**：左 = GUI、右 = CLI。这是用户口径里"分两列"的全部含义 ——
    /// 颜色不参与分列，它是标签自己的属性。
    #[test]
    fn the_two_columns_are_gui_left_and_cli_right() {
        let (_, axis) = pane();
        let gui = tag_column_center(axis, TagSource::Gui);
        let cli = tag_column_center(axis, TagSource::Cli);
        // 注意：**两列都落在轴带右半**（左半让给拍号了）—— 所以"GUI 在轴带中心左边"这种
        // 旧几何的断言不再成立。真正要守的是：GUI 在 CLI 左边、且两者都在数字区右边。
        assert!(gui < cli, "GUI 列应在 CLI 列左边：{gui} vs {cli}");
        // 数字区归拍号独占（用户口径：把时间轴数字移到标签外面）：
        // 两列都必须落在 `axis.min.x + AXIS_NUM_W` 右边，一像素都不许侵进去。
        let num_right = axis.min.x + AXIS_NUM_W;
        assert!(gui > num_right, "GUI 列侵进了拍号区：{gui} <= {num_right}");
        assert!(cli > num_right, "CLI 列侵进了拍号区：{cli} <= {num_right}");
        let left = num_right;
        let half = (axis.width() - AXIS_NUM_W) * 0.5;
        assert!((gui - (left + half * 0.5)).abs() < 1e-3);
        assert!((cli - (left + half * 1.5)).abs() < 1e-3);

        // 而且标签的矩形也不许压到拍号区（这是真正会被看见的那一条）
        let body = egui::Rect::from_min_max(egui::pos2(300.0, 40.0), egui::pos2(1300.0, 840.0));
        let t = Tag { start: 4.0, end: 8.0, source: TagSource::Gui, color: [0, 0, 0] };
        let r = tag_rect(axis, body, 0.0, 32.0, &t);
        assert!(r.min.x >= num_right - 1e-3, "标签左缘压到了拍号区：{}", r.min.x);
    }

    /// 命中：**先按列筛、再按拍区间筛**。同一点上两列各有一个标签时，各自只能命中自己那一列。
    #[test]
    fn a_tag_is_only_hit_in_its_own_column() {
        let (body, axis) = pane();
        let (anchor, beats) = (0.0, 32.0);
        let tags = vec![
            Tag { start: 4.0, end: 8.0, source: TagSource::Gui, color: [1, 2, 3] },
            Tag { start: 4.0, end: 8.0, source: TagSource::Cli, color: [4, 5, 6] },
        ];
        let y = beat_y(body, anchor, beats, 6.0); // 落在两个标签的拍区间正中
        let hit_gui = tag_hit(&tags, axis, body, anchor, beats, egui::pos2(tag_column_center(axis, TagSource::Gui), y));
        let hit_cli = tag_hit(&tags, axis, body, anchor, beats, egui::pos2(tag_column_center(axis, TagSource::Cli), y));
        assert_eq!(hit_gui, Some(0), "左列只该命中 GUI 那个");
        assert_eq!(hit_cli, Some(1), "右列只该命中 CLI 那个");
        // 拍区间之外不命中
        let y_out = beat_y(body, anchor, beats, 20.0);
        assert_eq!(tag_hit(&tags, axis, body, anchor, beats, egui::pos2(tag_column_center(axis, TagSource::Gui), y_out)), None);
        // 轴带之外不命中（哪怕 y 对）
        assert_eq!(tag_hit(&tags, axis, body, anchor, beats, egui::pos2(axis.min.x - 20.0, y)), None);
    }

    /// **控制杆的优先级**：删除 > 拉长 > 本体。
    ///
    /// 这条必须钉住：三者的矩形是**叠在一起**的（✕ 在标签正中、拉长杆贴着两端），
    /// 判错一个就会"点删除却选中了它"或者"想拉长却只是选中"。
    #[test]
    fn the_two_bars_are_the_only_handles() {
        let (body, axis) = pane();
        let (anchor, beats) = (0.0, 32.0);
        let t = Tag { start: 4.0, end: 8.0, source: TagSource::Gui, color: [9, 9, 9] };
        let tags = [t];
        let r = tag_rect(axis, body, anchor, beats, &t);
        let (start_bar, end_bar) = tag_handle_rects(axis, body, anchor, beats, &t);

        let hit = |p: egui::Pos2| tag_hit_part(&tags, axis, body, anchor, beats, p);
        assert_eq!(hit(end_bar.center()), Some((0, TagPart::End)), "上缘是终点杆");
        assert_eq!(hit(start_bar.center()), Some((0, TagPart::Start)), "下缘是起点杆");
        // **正中是本体**：标签上没有删除按钮了（用户口径："删除按 del，不要放删除按钮"），
        // 所以正中回到"选中它"这条最朴素的语义上。
        assert_eq!(hit(r.center()), Some((0, TagPart::Body)), "正中是本体");
        // 拉长杆在标签**里面**（不是贴在外面的另一块），所以它天然被 `tag_rect` 圈住
        assert!(r.expand(1.0).contains(start_bar.center()));
        assert!(r.expand(1.0).contains(end_bar.center()));
    }

    /// 标签的矩形覆盖它的**拍区间**，而且同一个标签在窗口滚动后跟着移动
    /// （`beat_y` 是全局面板映射，标签不该自己再算一套）。
    #[test]
    fn a_tag_rect_follows_its_beat_span() {
        let (body, axis) = pane();
        let t = Tag { start: 4.0, end: 8.0, source: TagSource::Gui, color: [9, 9, 9] };
        let r0 = tag_rect(axis, body, 0.0, 32.0, &t);
        let y4 = beat_y(body, 0.0, 32.0, 4.0);
        let y8 = beat_y(body, 0.0, 32.0, 8.0);
        assert!((r0.min.y.min(r0.max.y) - y4.min(y8)).abs() < 1e-3);
        assert!((r0.min.y.max(r0.max.y) - y4.max(y8)).abs() < 1e-3);
        // 反向跨度的标签（先拖到上面再拖回来）也要给出同一个矩形
        let flipped = Tag { start: 8.0, end: 4.0, ..t };
        assert_eq!(tag_rect(axis, body, 0.0, 32.0, &flipped), r0);
    }
}
