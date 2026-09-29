// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **底部时间轴**：几何、读数、绘制。
//!
//! 为什么单独一个模块（用户："重写时间轴代码"）：
//! 这块以前长在 `main.rs` 中央面板的闭包里（约 250 行），于是"总长怎么算、x 怎么映射、
//! 读数放不放得下"全都**测不了** —— 只能靠截图看，而截图看错一次就会改错代码（真发生过）。
//! 现在拆成三层，两层是纯逻辑：
//!
//! | 层 | 是什么 | 能不能单测 |
//! |---|---|---|
//! | [`TimelineGeom`] | 总长 → x 映射、黄线/浅色带的位置、拍线步长 | **能**（纯几何） |
//! | [`readouts`] | 按可用宽度逐级省略的读数 | **能**（把"量宽度"作为参数传进来） |
//! | [`draw`] | 把上面两者画出来 | 只有这一层只剩 egui 调用 |
//!
//! 时间轴的内容规范（谁在上、谁代表什么）见 [`draw`] 的注释与《框架选型》§7.56–7.58；
//! **子音符条 = 4 行（4 种音符）+ 行内合并**见 §7.77。

use crate::state::{EditorState, Line, NoteKind};

/// 面板配色（只有一处，改色不用满文件找）
const BG: [u8; 3] = [16, 16, 24];
const BAND: [u8; 4] = [210, 220, 245, 26];
const BEAT_MAJOR: [u8; 3] = [90, 95, 130];
const BEAT_MINOR: [u8; 3] = [55, 58, 80];
const START_LINE: [u8; 3] = [240, 210, 70];
const PLAYHEAD: [u8; 3] = [235, 238, 245];
const EVENT_BAR: [u8; 4] = [120, 200, 255, 90];
const CURVE: [u8; 3] = [120, 220, 160];
const TEXT_DIM: [u8; 3] = [120, 125, 160];
const TEXT_LEGEND: [u8; 3] = [150, 155, 185];
const TEXT_TRACK: [u8; 3] = [120, 220, 160];

// ---- 子音符条：4 行 = 4 种音符（用户："将音符显示改造为4行，分别对应4个音符"）----

/// 音符条的行数。**必须等于音符种类数** —— `kind_row` 的 `match` 不写通配臂，
/// 将来加第 5 种音符会**编译不过**，而不是悄悄挤进同一行。
pub const NOTE_ROWS: usize = 4;
/// 行号的规范顺序（自上而下）。行键文本与单测都由它生成 ⇒ 改顺序只有这一处。
pub const KIND_ROWS: [NoteKind; NOTE_ROWS] =
    [NoteKind::Tap, NoteKind::Hold, NoteKind::Drag, NoteKind::Flick];
/// 单颗音符至少占的像素宽（原口径 1.5px：不然一颗 tap 细到看不见）
const NOTE_MIN_PX: f32 = 1.5;
/// 相邻两段间隙不足这么多像素就并起来。**必须 ≥1px**：段数因此有上界（≈ 轴宽 / 1px × 4 行），
/// 而"间隙 0.4px 的两段"在屏幕上本来就分不开 —— 合并它不丢信息。
const NOTE_MERGE_GAP: f32 = 1.0;
/// 每行的高度上限（时间轴再高，音符条也不该变成主视觉）
const NOTE_ROW_H_MAX: f32 = 16.0;
/// 行间空隙
const NOTE_ROW_GAP: f32 = 1.0;
/// 底部事件条的高度（`rect.max.y - EVENT_BAR_H .. rect.max.y - EVENT_BAR_BOTTOM`）
const EVENT_BAR_H: f32 = 6.0;
const EVENT_BAR_BOTTOM: f32 = 1.0;
/// 音符条区与事件条之间留的空隙
const NOTE_BAND_PAD: f32 = 3.0;
/// 音符条区的上边界（占轴高比例）—— 曲线区是 0.08…0.70（见 [`draw`]），0.72 起不与它打架
const NOTE_BAND_TOP_FRAC: f32 = 0.72;

/// **音符种类 → 行号**（自上而下）。
pub fn kind_row(k: NoteKind) -> usize {
    match k {
        NoteKind::Tap => 0,
        NoteKind::Hold => 1,
        NoteKind::Drag => 2,
        NoteKind::Flick => 3,
    }
}

/// 读数第 2 行末尾那截**行键**（"哪一行是哪种音符"）。由 [`KIND_ROWS`] 生成 ⇒ 不会与 [`kind_row`] 漂移。
pub fn note_row_key() -> String {
    let names: Vec<&str> = KIND_ROWS.iter().map(|k| k.label()).collect();
    format!("音符行（自上而下）{}", names.join("/"))
}

/// 音符种类色（与演奏区同一套 [`NoteKind::color`]；转 `Color32` 的口径与改造前一致）
fn kind_color(k: NoteKind) -> egui::Color32 {
    let c = k.color();
    egui::Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8)
}

/// 合并后的一段：一个矩形（像素区间）。**段与段在 x 上互不重叠** —— 那正是"合并"的判据。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoteSpan {
    pub x0: f32,
    pub x1: f32,
}

/// 一行的两层：`base` 是**没选中**的（画种类色），`selected` 是**选中**的（画白）。
///
/// 分成两层而不是"选中状态不同就不合并"，是因为后者会留下一个恶心的边角：
/// 密流里选一颗时，前一段的右端早就越过了这颗的左端 —— 想要"互不重叠"就得把前一段**裁短**，
/// 而裁短会让"这一段到底有没有音符"变得可疑。两层则各自成立、各自不重叠，
/// **白的那层最后画**（压在种类色上）⇒ 选中的那颗一定看得见，且合并永远不用为它让路。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowSpans {
    pub base: Vec<NoteSpan>,
    pub selected: Vec<NoteSpan>,
}

/// 往一行的段表里放一颗音符：与上一段重叠（或间隙 < `gap`）就**并进去**，否则另起一段。
///
/// 前提：同一行的输入按 `x0` **非降序**（[`line_spans`] 满足它，见那里的说明）——
/// 这是"单趟贪心合并"成立的条件，`debug_assert` 盯着它（测试里若违反会当场炸）。
fn push_span(out: &mut Vec<NoteSpan>, x0: f32, x1: f32, min_px: f32, gap: f32) {
    if !(x0.is_finite() && x1.is_finite()) {
        return; // 坏数据不该让整帧画不出来（原来一个 NaN 矩形也会被 egui 丢掉，这里顺手挡掉）
    }
    let (a, b) = (x0, x1.max(x0 + min_px));
    match out.last_mut() {
        Some(last) if a <= last.x1 + gap => {
            debug_assert!(a + 1e-3 >= last.x0, "同一行必须按 x 非降序：{a} < {}", last.x0);
            last.x1 = last.x1.max(b);
        }
        _ => out.push(NoteSpan { x0: a, x1: b }),
    }
}

/// 合并一段序列（**输入顺序任意**：内部先按 `x0` 排序）。给单测与"顺序没保证"的调用方用；
/// 帧里走的是 [`line_spans`] 那条**不排序、不建中间数组**的路。
pub fn merge_spans(items: &[(f32, f32)], min_px: f32, gap: f32) -> Vec<NoteSpan> {
    let mut v: Vec<(f32, f32)> =
        items.iter().copied().filter(|(a, b)| a.is_finite() && b.is_finite()).collect();
    v.sort_by(|p, q| p.0.partial_cmp(&q.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = Vec::new();
    for (a, b) in v {
        push_span(&mut out, a, b, min_px, gap);
    }
    out
}

/// **把一条线的音符压成 4 行、每行若干互不重叠的段** —— `draw` 与单测共用这一份实现
/// （免得"测的"和"画的"不是一套）。
///
/// 为什么可以**不排序**：`Line::notes` 按时间升序（`state::notes_of` 排的），
/// `TimelineGeom::x_of` 对时间单调 ⇒ 每一行拿到的子序列也按 `x0` 非降序。
/// 于是单趟贪心合并即是正解，代价 O(音符数)、额外内存只有"段"本身（个位数）。
///
/// `sel` 是**视图下标**的升序集合（`Selection::notes_set()`）。这里用归并式游标而不是
/// 每颗音符查一次集合：查一次是 O(log n) 且要跳 BTree 节点，5 万颗就是 5 万次随机访存。
pub fn line_spans(
    line: &Line,
    geom: &TimelineGeom,
    sel: &std::collections::BTreeSet<usize>,
) -> [RowSpans; NOTE_ROWS] {
    let mut rows: [RowSpans; NOTE_ROWS] = std::array::from_fn(|_| RowSpans::default());
    let mut cursor = sel.iter().copied().peekable();
    for (i, n) in line.notes.iter().enumerate() {
        // 选区里可能有**不属于这条线的**（或已失效的）下标：游标只前进、不回退
        while cursor.peek().is_some_and(|&j| j < i) {
            cursor.next();
        }
        let is_sel = cursor.peek() == Some(&i);
        if is_sel {
            cursor.next();
        }
        let row = &mut rows[kind_row(n.kind)];
        let dst = if is_sel { &mut row.selected } else { &mut row.base };
        push_span(dst, geom.x_of(n.time), geom.x_of(n.end.max(n.time)), NOTE_MIN_PX, NOTE_MERGE_GAP);
    }
    rows
}

/// 音符条的**行布局**（4 行自上而下 = [`KIND_ROWS`]）。纯几何 ⇒ 可单测
/// （"矮轴里 4 行不越界、不重叠"曾经是只能靠截图看的那类东西）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoteRowLayout {
    pub row_h: f32,
    pub gap: f32,
    pub top: f32,
}

impl NoteRowLayout {
    pub fn new(rect: egui::Rect) -> Self {
        let band_top = rect.min.y + rect.height() * NOTE_BAND_TOP_FRAC;
        let band_bot = rect.max.y - EVENT_BAR_H - NOTE_BAND_PAD;
        let gap = NOTE_ROW_GAP;
        let avail = (band_bot - band_top).max(1.0);
        // 贴着事件条往上排：轴一般高得够（`timeline_height` 的下限是 64px）⇒ 4 行正好铺满这一段；
        // 轴高到离谱时行高封顶，不再把音符条撑成主视觉。
        let row_h = ((avail - gap * (NOTE_ROWS as f32 - 1.0)) / NOTE_ROWS as f32)
            .clamp(1.0, NOTE_ROW_H_MAX);
        let total = row_h * NOTE_ROWS as f32 + gap * (NOTE_ROWS as f32 - 1.0);
        // 轴矮到 4 行都塞不下时（时间轴隐藏 ⇒ 高度 0）：先保证**不跑到轴外去**，
        // 行被 `painter_at` 裁掉是小事，越界画到别的面板上是大事。
        let top = (band_bot - total).clamp(rect.min.y, band_bot.max(rect.min.y));
        Self { row_h, gap, top }
    }

    /// 第 `row` 行的顶边
    pub fn y_of(&self, row: usize) -> f32 {
        self.top + row as f32 * (self.row_h + self.gap)
    }

    /// 第 `row` 行里 `x0..x1` 那一段的矩形
    pub fn rect_of(&self, row: usize, x0: f32, x1: f32) -> egui::Rect {
        egui::Rect::from_min_max(
            egui::pos2(x0, self.y_of(row)),
            egui::pos2(x1, self.y_of(row) + self.row_h),
        )
    }
}

fn rgb(c: [u8; 3]) -> egui::Color32 {
    egui::Color32::from_rgb(c[0], c[1], c[2])
}
fn rgba(c: [u8; 4]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])
}

/// **纯几何**：这一段把"时间"换算成"像素"，并把三条标记（浅色带 / 黄线 / 白线）的位置定下来。
///
/// 它只吃 [`EditorState`] 与叠加层的 `lead_beats`，不碰 egui 的绘制 ——
/// 于是"总长多长、带子从哪到哪、点一下该 seek 到哪"都能在单测里断言。
pub struct TimelineGeom {
    pub rect: egui::Rect,
    /// 总长（秒）= `EditorState::timeline_duration()`（max(音乐, 内容) + 10 拍）
    pub duration: f64,
    /// 编辑区窗口的底部/顶部（秒）——**已夹到轴内**
    pub span_lo: f64,
    pub span_hi: f64,
    /// 编辑区窗口的原始底部（可能 < 0：播放头在 0 附近时叠加层会显示一点前导）
    pub span_raw_lo: f64,
    pub playhead: f64,
    /// 自适应抽稀之后的拍线步长（秒）与它对应的拍数倍数
    pub step_sec: f64,
    pub mult: u32,
    /// 公式的两个输入（读数里显示出来 —— "时间轴为什么这么短"通常一眼就看出来了：
    /// 音乐那项是 `None` 就意味着**没装上音乐**）
    pub music_sec: Option<f64>,
    pub content_sec: f64,
    pub content_beat: f64,
    /// 子音符条的 4 行落在哪（纯几何，见 [`NoteRowLayout`]）
    pub rows: NoteRowLayout,
}

impl TimelineGeom {
    /// 从状态算出这一段几何。`lead_beats` = 叠加层的 `lead_beats`（与编辑区同一套定义）。
    pub fn new(rect: egui::Rect, st: &EditorState, lead_beats: f64) -> Self {
        let duration = st.timeline_duration().max(0.001);
        let (raw_lo, raw_hi) = st.edit_area_span(lead_beats);
        let width = rect.width().max(1.0);
        // 拍线自适应抽稀：整谱可见时拍线密度会远超像素密度（20 万音符的谱面曾画出 5 万条线，
        // 把帧时间拖到 20 ms）。保持"相邻拍线至少 6px"。
        let base = st.chart.beat_interval;
        let px_per_sec = width / duration as f32;
        let mut mult = 1u32;
        while ((base * mult as f64) * px_per_sec as f64) < 6.0 && mult < 4096 {
            mult *= 2;
        }
        Self {
            rect,
            duration,
            music_sec: st.music_len(),
            content_sec: st.chart.tmap.sec(st.content_end_beat),
            content_beat: st.content_end_beat,
            span_lo: raw_lo.max(0.0),
            span_hi: raw_hi.clamp(0.0, duration),
            span_raw_lo: raw_lo,
            playhead: st.playhead,
            step_sec: base * mult as f64,
            mult,
            rows: NoteRowLayout::new(rect),
        }
    }

    /// 时刻 → 横坐标
    pub fn x_of(&self, t: f64) -> f32 {
        self.rect.min.x + (t / self.duration) as f32 * self.rect.width()
    }

    /// 横坐标 → 时刻（点击/拖动 seek 用）
    pub fn t_of(&self, x: f32) -> f64 {
        let frac = ((x - self.rect.min.x) / self.rect.width().max(1.0)).clamp(0.0, 1.0);
        frac as f64 * self.duration
    }

    fn content_end_bits(&self) -> u64 {
        self.content_sec.to_bits()
    }

    /// 拍线：`(横坐标, 是不是整拍大线)`。密度由 `step_sec` 定，**条数有上限**（防病态输入）。
    pub fn beat_lines(&self) -> Vec<(f32, bool)> {
        let mut out = Vec::new();
        if !(self.step_sec.is_finite() && self.step_sec > 0.0) {
            return out;
        }
        let mut t = 0.0;
        let mut beat = 0u32;
        while t <= self.duration && out.len() < 20_000 {
            out.push((self.x_of(t), beat % 4 == 0));
            t += self.step_sec;
            beat = beat.saturating_add(self.mult);
        }
        out
    }
}

/// 右上角那行"线 #N · 轨道 X（N 条事件）"的原料（拿不到就不画）
pub struct TrackInfo<'a> {
    pub line: u32,
    pub track: &'a str,
    pub events: usize,
}

/// 一条读数：文本 + 行号（0/1）+ 是左对齐还是右对齐 + 颜色
#[derive(Clone, Debug, PartialEq)]
pub struct Readout {
    pub row: u8,
    pub right: bool,
    pub text: String,
    pub color: [u8; 3],
}

/// **按可用宽度逐级省略读数**（用户："读数不要被裁/撞字"）。
///
/// 优先级：① 总长/拍线（读数本身）＞ ② 颜色图例（解释那三条线）＞ ③ 右上角"线 #N · 轨道 X"
/// （**第一个让位**：左侧「事件轨道」那行写着同样的信息）。判据是 `measure` 量出来的**像素宽**，
/// 不是按字符数猜 —— 等宽字体里中文与数字宽度不同，猜必然错。
///
/// **不变量**：返回的每一条都 `measure(text) + 边距 ≤ 宽度`（有单测）。宁可少一行，也不撞字。
pub fn readouts(
    geom: &TimelineGeom,
    track: Option<&TrackInfo<'_>>,
    measure: &dyn Fn(&str) -> f32,
) -> Vec<Readout> {
    const MARGIN: f32 = 12.0;
    let fits = |s: &str| measure(s) + MARGIN <= geom.rect.width();
    let mut out = Vec::new();

    // 第 1 行左：总长 + 拍线（挤不下就只留总长）
    let music = match geom.music_sec {
        Some(m) => format!("{m:.1}s"),
        None => "无音乐".to_owned(),
    };
    // 三档：把公式的两个输入也写出来 → 只看"总长"是不够的（"为什么这么短"要看输入）
    let head_verbose = format!(
        "总长 {:.1}s（音乐 {music} ｜ 内容 {:.1}s / {:.0} 拍）｜ 拍线 1/{}（{:.3}s）",
        geom.duration, geom.content_sec, geom.content_beat, geom.mult, geom.step_sec
    );
    let head_full = format!(
        "总长 {:.1}s（音乐 {music} ｜ 内容 {:.1}s）｜ 拍线 1/{}（{:.3}s）",
        geom.duration, geom.content_sec, geom.mult, geom.step_sec
    );
    let head_short = format!("总长 {:.1}s", geom.duration);
    // **放不下就不画**：宁可空着，也不画半句被面板边缘裁掉的读数
    let head = if fits(&head_verbose) {
        Some(head_verbose)
    } else if fits(&head_full) {
        Some(head_full)
    } else if fits(&head_short) {
        Some(head_short)
    } else {
        None
    };
    let head_w = head.as_deref().map(measure).unwrap_or(0.0);
    if let Some(head) = &head {
        out.push(Readout { row: 0, right: false, text: head.clone(), color: TEXT_DIM });
    }

    // 第 1 行右：轨道信息（放得下才画 —— 18px 让两边不贴在一起）
    if let Some(t) = track {
        let s = format!("线 #{} · 轨道 {}（{} 条事件）", t.line, t.track, t.events);
        if head_w + measure(&s) + 18.0 <= geom.rect.width() {
            out.push(Readout { row: 0, right: true, text: s, color: TEXT_TRACK });
        }
    }

    // 第 2 行：颜色图例（逐档挑放得下的最长那个；都放不下就不画）
    // 最长的一档多带一句"哪一行是哪种音符"（4 行靠颜色认，而 Tap/Hold 都是蓝的 —— 不给行键会看糊）
    let legends = [
        format!(
            "黄线 = 编辑区起点 ｜ 浅色 = 编辑区窗口（{:.1}→{:.1}s）｜ 白线 = 播放头 ｜ {}",
            geom.span_lo,
            geom.span_hi,
            note_row_key()
        ),
        format!(
            "黄线 = 编辑区起点 ｜ 浅色 = 编辑区窗口（{:.1}→{:.1}s）｜ 白线 = 播放头",
            geom.span_lo, geom.span_hi
        ),
        format!(
            "黄线=起点 ｜ 浅色=编辑区窗口 {:.1}→{:.1}s ｜ 白线=播放头",
            geom.span_lo, geom.span_hi
        ),
        format!("黄线=起点 ｜ 浅色=窗口 {:.0}→{:.0}s", geom.span_lo, geom.span_hi),
        "黄线=起点 ｜ 浅色=窗口".to_owned(),
    ];
    if let Some(legend) = legends.iter().find(|s| fits(s)) {
        out.push(Readout { row: 1, right: false, text: legend.clone(), color: TEXT_LEGEND });
    }
    out
}

/// 时间轴这一帧产出的动作（调用方执行 —— 模块本身不改状态）
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TimelineOut {
    /// 用户点了/拖了时间轴：请求 seek 到这里（秒）
    pub seek: Option<f64>,
}

/// 画时间轴并返回本帧动作。
///
/// 内容规范（这几条都是用户点名的，改动前先看 §7.56–7.58）：
/// - 底色 → **浅色带**（编辑区从底层到顶层的跨度）→ 拍线 → 当前判定线的事件条/折线/**子音符条
///   （4 行 = 4 种音符，行内重合已合并）** → **黄线**（编辑区**底部** = "起点"）→ **细白线**（播放头）→ 两行读数。
/// - 浅色带画在拍线**之前**：它是底衬，不能把数据压灰。
/// - 黄线与播放头是**两种东西**（播放头在黄线上方 `lead_beats` 拍），必须一眼分得清。
pub fn draw(
    ui: &mut egui::Ui,
    st: &EditorState,
    rect: egui::Rect,
    lead_beats: f64,
) -> TimelineOut {
    let geom = TimelineGeom::new(rect, st, lead_beats);
    trace_if_changed(&geom);
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 2.0, rgb(BG));

    // 浅色带：编辑区可见跨度（夹到轴内 —— 播放头在 0 附近时底部是负的）
    p.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(geom.x_of(geom.span_lo), rect.min.y),
            egui::pos2(geom.x_of(geom.span_hi), rect.max.y),
        ),
        0.0,
        rgba(BAND),
    );

    // 拍线
    for (x, major) in geom.beat_lines() {
        let h = if major { rect.height() * 0.45 } else { rect.height() * 0.22 };
        p.line_segment(
            [egui::pos2(x, rect.max.y - h), egui::pos2(x, rect.max.y)],
            egui::Stroke::new(if major { 1.0 } else { 0.5 }, rgb(if major { BEAT_MAJOR } else { BEAT_MINOR })),
        );
    }

    // 当前判定线的事件条 / 折线 / 子音符
    let mut track_info: Option<(u32, String, usize)> = None;
    if let Some(line) = st.selected() {
        let track = line.track(st.selected_track);
        track_info = Some((line.index as u32, st.selected_track.key().to_owned(), track.events.len()));
        for e in &track.events {
            let (t0, t1) = (
                st.chart.tmap.sec(e.start.to_f64()),
                st.chart.tmap.sec(e.end.to_f64()),
            );
            p.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(geom.x_of(t0), rect.max.y - EVENT_BAR_H),
                    egui::pos2(geom.x_of(t1), rect.max.y - EVENT_BAR_BOTTOM),
                ),
                0.0,
                rgba(EVENT_BAR),
            );
        }
        if track.curve.len() >= 2 {
            let (mn, mx) = (track.min, track.max);
            let span = (mx - mn).max(1e-6);
            let top = rect.min.y + rect.height() * 0.08;
            let h = rect.height() * 0.62;
            let mut pts: Vec<egui::Pos2> = track
                .curve
                .iter()
                .map(|q| {
                    let y = top + h * (1.0 - (q[1] - mn) / span);
                    egui::pos2(geom.x_of(q[0] as f64), y)
                })
                .collect();
            // 采样只到"谱面末尾"（`sample_track` 把事件块前后的空位也按"保持相邻那块的值"补上了）；
            // 时间轴可能比谱面长（有音乐时按音乐长度）⇒ 两端补齐到画面边缘，**值不变** ——
            // 否则事件块之后那段看上去像"回到了默认值"（用户报的正是这个）。
            if let (Some(first), Some(last)) = (pts.first().copied(), pts.last().copied()) {
                if first.x > rect.left() + 0.5 {
                    pts.insert(0, egui::pos2(rect.left(), first.y));
                }
                if last.x < rect.right() - 0.5 {
                    pts.push(egui::pos2(rect.right(), last.y));
                }
            }
            p.add(egui::Shape::line(pts, egui::Stroke::new(1.5, rgb(CURVE))));
        }
        // 子音符条：**4 行 = 4 种音符**（Tap/Hold/Drag/Flick），行内时间重合的合并成一段。
        // 用户："在时间轴上将有重合的音符合并为1个矩形，将音符显示改造为4行，分别对应4个音符"。
        //
        // 为什么必须合并：整谱可见时 5 万音符里有 30 颗挤在同一个像素上 —— 一颗一个矩形既是
        // 5 万个形状/帧（实测到过 ~15 ms/帧），又**不含任何额外信息**（糊成一片）。
        // 分 4 行反而多给了信息："这一段是谁在堆"。选中的那颗走单独一层、最后画（见 `line_spans`）。
        let spans = line_spans(line, &geom, st.selection().notes_set());
        for (r, row) in spans.iter().enumerate() {
            let col = kind_color(KIND_ROWS[r]);
            for s in &row.base {
                p.rect_filled(geom.rows.rect_of(r, s.x0, s.x1), 0.0, col);
            }
        }
        // 选中的那颗：白，**压在种类色之上**（两层各自合并，所以它永远不会被合并吃掉）
        for (r, row) in spans.iter().enumerate() {
            for s in &row.selected {
                p.rect_filled(geom.rows.rect_of(r, s.x0, s.x1), 0.0, egui::Color32::WHITE);
            }
        }
    }

    // **黄线 = 起点**（编辑区底部），随后是播放头（细白线）—— 两者刻意不同
    let sx = geom.x_of(geom.span_lo);
    p.line_segment(
        [egui::pos2(sx, rect.min.y), egui::pos2(sx, rect.max.y)],
        egui::Stroke::new(2.0, rgb(START_LINE)),
    );
    let px = geom.x_of(geom.playhead);
    p.line_segment(
        [egui::pos2(px, rect.min.y), egui::pos2(px, rect.max.y)],
        egui::Stroke::new(1.0, rgb(PLAYHEAD)),
    );

    // 读数：量宽度要 `&mut FontsView`（`layout_*` 会往字形图集里塞东西）⇒ `fonts_mut`
    let font = egui::FontId::monospace(10.0);
    let ctx = ui.ctx().clone();
    let measure = |s: &str| {
        ctx.fonts_mut(|f| f.layout_no_wrap(s.to_owned(), font.clone(), egui::Color32::WHITE).size().x)
    };
    let info = track_info
        .as_ref()
        .map(|(line, track, events)| TrackInfo { line: *line, track, events: *events });
    for r in readouts(&geom, info.as_ref(), &measure) {
        let pos = if r.right {
            rect.min + egui::vec2(rect.width() - 6.0, 4.0 + r.row as f32 * 13.0)
        } else {
            rect.min + egui::vec2(6.0, 4.0 + r.row as f32 * 13.0)
        };
        p.text(
            pos,
            if r.right { egui::Align2::RIGHT_TOP } else { egui::Align2::LEFT_TOP },
            r.text,
            font.clone(),
            rgb(r.color),
        );
    }

    // 点击/拖动 = seek（换算成时刻交给调用方；本模块不改状态）
    let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());
    let mut out = TimelineOut::default();
    if resp.clicked() || resp.dragged() {
        if let Some(pos) = resp.interact_pointer_pos() {
            out.seek = Some(geom.t_of(pos.x));
        }
    }
    out
}

/// `OPM_TL_TRACE=1`：**公式的输入与结果一变就打一行**（不是每帧打，避免刷屏）。
///
/// 为什么要它：用户报"时间轴状态无改变"时，唯一能分辨的情况是"他那边公式的输入是什么" ——
/// 比如音乐压根没装上（`无音乐`），那时间轴当然短，而这不是总长公式的问题。
/// 与 `--trace-startup` 同一类：把"猜"换成"看一行数"。
fn trace_if_changed(geom: &TimelineGeom) {
    use std::sync::{Mutex, OnceLock};
    static LAST: OnceLock<Mutex<Option<(u64, u64, u64)>>> = OnceLock::new();
    if std::env::var_os("OPM_TL_TRACE").is_none() {
        return;
    }
    let key = (
        geom.duration.to_bits(),
        geom.music_sec.map(f64::to_bits).unwrap_or(u64::MAX),
        geom.content_end_bits(),
    );
    let cell = LAST.get_or_init(|| Mutex::new(None));
    let mut last = cell.lock().unwrap();
    if *last == Some(key) {
        return;
    }
    *last = Some(key);
    eprintln!(
        "[tl] 总长 {:.3}s = max(音乐 {}, 内容 {:.3}s / {:.1} 拍) + {} 拍",
        geom.duration,
        match geom.music_sec {
            Some(m) => format!("{m:.3}s"),
            None => "无音乐".to_owned(),
        },
        geom.content_sec,
        geom.content_beat,
        crate::state::EditorState::TIMELINE_TAIL_BEATS,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Beat, BpmEntry, Document, Note, NoteKind};
    /// 视图侧的音符种类（与 `doc::NoteKind` 是两个类型 —— 名字在这一层撞车，用别名分开）
    use crate::state::NoteKind as VKind;
    use crate::state::chart_from_doc;

    fn rect(w: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(10.0, 100.0), egui::vec2(w, 60.0))
    }

    fn doc_120bpm(notes_at: &[i64]) -> Document {
        let mut doc = Document::default();
        doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 120.0, foreign: Default::default() }];
        for b in notes_at {
            doc.judge_lines[0]
                .notes
                .push(Note::new(NoteKind::Tap, Beat::new(*b, 1), 0.0));
        }
        doc
    }

    /// 几何：总长铺满整条轴；反向换算自洽；带子夹在轴内
    #[test]
    fn geometry_maps_time_to_pixels_both_ways() {
        let st = EditorState::new(chart_from_doc(&doc_120bpm(&[40]))); // 40 拍 = 20s，总长 25s
        let g = TimelineGeom::new(rect(500.0), &st, 2.0);
        assert!((g.duration - 25.0).abs() < 1e-9, "{}", g.duration);
        assert_eq!(g.x_of(0.0), 10.0, "左端");
        assert!((g.x_of(g.duration) - 510.0).abs() < 1e-3, "右端铺满");
        assert!((g.t_of(g.x_of(7.0)) - 7.0).abs() < 1e-3, "往返自洽");
        assert_eq!(g.t_of(-100.0), 0.0, "轴外夹到 0");
        assert!((g.t_of(9999.0) - g.duration).abs() < 1e-9, "轴外夹到末端");
        // 播放头 0 时编辑区底部为负（叠加层的前导），但带子夹到 0
        assert!(g.span_raw_lo < 0.0);
        assert_eq!(g.span_lo, 0.0);
        assert!(g.span_hi > 0.0 && g.span_hi <= g.duration);
    }

    /// 拍线：步长随宽度自适应放大，且**条数有上限**（不会画出一堆看不见的线）
    #[test]
    fn beat_lines_thin_out_on_narrow_or_long_charts() {
        let st = EditorState::new(chart_from_doc(&doc_120bpm(&[4000]))); // 很长
        let wide = TimelineGeom::new(rect(2000.0), &st, 2.0);
        let narrow = TimelineGeom::new(rect(200.0), &st, 2.0);
        assert!(
            narrow.mult > wide.mult,
            "窄轴上要抽得更狠：{} vs {}",
            narrow.mult,
            wide.mult
        );
        for g in [&wide, &narrow] {
            let lines = g.beat_lines();
            assert!(!lines.is_empty());
            assert!(lines.len() < 20_000, "条数上限");
            assert!(lines.iter().all(|(x, _)| x.is_finite()));
        }
    }

    /// **读数的不变量**：每一条都放得下（用户要求"不要被裁/撞字"），且宽度不够时逐级省略
    #[test]
    fn readouts_never_exceed_the_available_width() {
        // 假的量宽：每个字符 8px（够近似，且**可复现**）
        let measure = |s: &str| s.chars().count() as f32 * 8.0;
        let st = EditorState::new(chart_from_doc(&doc_120bpm(&[40])));
        let info = TrackInfo { line: 0, track: "alpha", events: 0 };

        let wide = TimelineGeom::new(rect(1100.0), &st, 2.0);
        let rs = readouts(&wide, Some(&info), &measure);
        assert!(rs.iter().any(|r| r.right), "宽轴上右上角那条要画");
        assert!(rs.iter().any(|r| r.row == 1 && r.text.contains("白线")), "宽轴上图例是完整版");
        // 宽轴上把公式的两个输入也写出来（"为什么这么短"要看输入，不只看结果）
        let head = rs.iter().find(|r| r.row == 0 && !r.right).unwrap();
        assert!(head.text.contains("音乐") && head.text.contains("内容"), "{}", head.text);
        for r in &rs {
            assert!(measure(&r.text) <= wide.rect.width(), "放不下：{:?}", r.text);
        }

        // 窄到装不下"总长 + 右上角那条"时：先丢右上角（左侧「事件轨道」那行有同样信息）
        let mid = TimelineGeom::new(rect(200.0), &st, 2.0);
        let rs = readouts(&mid, Some(&info), &measure);
        assert!(rs.iter().all(|r| !r.right), "窄轴上先丢右上角那条：{rs:?}");
        for r in &rs {
            assert!(measure(&r.text) <= mid.rect.width(), "{:?}", r.text);
        }

        let tiny = TimelineGeom::new(rect(90.0), &st, 2.0);
        let rs = readouts(&tiny, Some(&info), &measure);
        assert!(rs.iter().any(|r| r.row == 0), "再挤也要留'总长'");
        for r in &rs {
            assert!(measure(&r.text) <= tiny.rect.width(), "{:?}", r.text);
        }
        // 挤到连"总长"都放不下时：一条都不画（宁可空着，也不裁半句）
        let none = readouts(&TimelineGeom::new(rect(10.0), &st, 2.0), Some(&info), &measure);
        assert!(none.is_empty(), "{none:?}");
    }

    /// 没有音乐时读数要**明说**（"时间轴为什么短"最常见的原因就是音乐没装上）
    #[test]
    fn readout_says_so_when_there_is_no_music() {
        let st = EditorState::new(chart_from_doc(&doc_120bpm(&[40])));
        let measure = |s: &str| s.chars().count() as f32 * 8.0;
        let g = TimelineGeom::new(rect(600.0), &st, 2.0);
        assert_eq!(g.music_sec, None);
        let rs = readouts(&g, None, &measure);
        let head = &rs[0].text;
        assert!(head.contains("无音乐"), "{head}");
        assert!(head.contains("总长"), "{head}");
    }

    /// **点击时间轴要能 seek**（重写之后这条最容易被碰坏：`allocate_rect` 的位置变了）
    #[test]
    fn clicking_the_timeline_requests_a_seek() {
        let mut st = EditorState::new(chart_from_doc(&doc_120bpm(&[40]))); // 总长 25s
        st.overlay_beats = 8.0;
        let ctx = egui::Context::default();
        let r = rect(500.0);
        let raw_click = |x: f32| egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(egui::pos2(x, r.center().y)),
                egui::Event::PointerButton {
                    pos: egui::pos2(x, r.center().y),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
                egui::Event::PointerButton {
                    pos: egui::pos2(x, r.center().y),
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                },
            ],
            ..Default::default()
        };
        // 第一帧只做命中注册（egui 的老规矩：frame 0 指针、frame 1 按键），第二帧才算点击
        let mut out = None;
        let mut frame = ctx.run_ui(raw_click(r.center().x), |ui| {
            draw(ui, &st, r, 2.0);
        });
        frame.textures_delta.clear();
        let mut frame2 = ctx.run_ui(raw_click(r.center().x), |ui| {
            out = draw(ui, &st, r, 2.0).seek;
        });
        frame2.textures_delta.clear();
        let t = out.expect("点在时间轴上应当请求 seek");
        assert!((t - 12.5).abs() < 0.6, "正中间 ≈ 总长一半（25s/2 = 12.5s），实际 {t}");
    }

    /// 黄线/浅色带取的是**编辑区**跨度（与叠加层同一套 anchor），不是播放头那一瞬间
    #[test]
    fn band_and_start_line_come_from_the_edit_area_span() {
        let mut st = EditorState::new(chart_from_doc(&doc_120bpm(&[40])));
        st.overlay_beats = 8.0;
        st.seek(10.0); // = 20 拍
        let g = TimelineGeom::new(rect(500.0), &st, 2.0);
        assert!((g.span_lo - 9.0).abs() < 1e-9, "底部 = 20 拍 − 2 拍 = 9s");
        assert!((g.span_hi - 13.0).abs() < 1e-9, "顶部 = +8 拍 = 13s");
        assert!((g.playhead - 10.0).abs() < 1e-9);
        assert!(g.x_of(g.span_lo) < g.x_of(g.playhead), "黄线在播放头左边");
    }

    // ---- 子音符条：4 行 + 行内合并（用户 2026-09-29 的口径）----

    /// 造一份"给定种类/拍位/时长"的谱面（`doc_120bpm` 只会造 tap）
    fn doc_kinds(notes: &[(NoteKind, i64, i64, i64)]) -> Document {
        let mut doc = Document::default();
        doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 120.0, foreign: Default::default() }];
        for (k, num, den, len) in notes {
            let mut n = Note::new(*k, Beat::new(*num, *den), 0.0);
            if *len > 0 {
                n.end = Some(Beat::new(*num + *len, *den));
            }
            doc.judge_lines[0].notes.push(n);
        }
        doc
    }

    fn sel_of(idx: &[usize]) -> std::collections::BTreeSet<usize> {
        idx.iter().copied().collect()
    }

    /// 4 行 = 4 种音符：**每种各占一行**，且行号与 [`KIND_ROWS`] 一一对应（两处不能漂移）
    #[test]
    fn the_four_note_kinds_get_four_distinct_rows() {
        let rows: Vec<usize> = KIND_ROWS.iter().map(|k| kind_row(*k)).collect();
        assert_eq!(rows, (0..NOTE_ROWS).collect::<Vec<_>>(), "{KIND_ROWS:?} 的顺序就是行号顺序");
        for i in 0..NOTE_ROWS {
            for j in (i + 1)..NOTE_ROWS {
                assert_ne!(KIND_ROWS[i], KIND_ROWS[j], "两种音符挤在同一行：{KIND_ROWS:?}");
            }
        }
        // 行键文本得把 4 行都说出来（否则界面上 4 行颜色只能靠猜）
        let key = note_row_key();
        for k in KIND_ROWS {
            assert!(key.contains(k.label()), "{key} 少了 {}", k.label());
        }
    }

    /// **有重合的音符合并为 1 个矩形**（用户点名的规则）：分开的还是各是各的
    #[test]
    fn overlapping_notes_merge_into_one_rect() {
        let s = merge_spans(&[(0.0, 10.0), (5.0, 15.0), (20.0, 22.0)], NOTE_MIN_PX, NOTE_MERGE_GAP);
        assert_eq!(s.len(), 2, "{s:?}");
        assert_eq!((s[0].x0, s[0].x1), (0.0, 15.0));
        assert_eq!((s[1].x0, s[1].x1), (20.0, 22.0));
    }

    /// 段之间**永不重叠**、按 x 升序，且合并不丢区间（乱序输入也成立 ⇒ 内部先排序）
    #[test]
    fn merged_spans_never_overlap_and_keep_x_order() {
        let items = [(30.0, 40.0), (0.0, 12.0), (5.0, 6.0), (41.0, 41.0), (12.5, 13.0), (13.4, 60.0)];
        let s = merge_spans(&items, NOTE_MIN_PX, 0.0);
        for w in s.windows(2) {
            assert!(w[1].x0 > w[0].x1, "两段叠在一起了：{s:?}");
        }
        assert_eq!(s.first().unwrap().x0, 0.0, "起点被丢了：{s:?}");
        assert_eq!(s.last().unwrap().x1, 60.0, "终点被丢了：{s:?}");
    }

    /// 一颗单独的 tap 也要看得见（最小 1.5px —— 改造前就是这个口径，别在合并时弄丢）
    #[test]
    fn a_lone_note_keeps_its_minimum_width() {
        let s = merge_spans(&[(100.0, 100.0)], NOTE_MIN_PX, NOTE_MERGE_GAP);
        assert_eq!(s.len(), 1);
        assert!((s[0].x1 - s[0].x0 - NOTE_MIN_PX).abs() < 1e-6, "{s:?}");
    }

    /// **选中的那颗不会被合并吃掉**：它在单独一层里，密流合并出的那一段盖不住它
    #[test]
    fn selected_notes_form_their_own_layer_over_the_merged_run() {
        // 2000 颗 tap 挤在 500 拍里（120bpm ⇒ 0.125s 一颗）：整谱可见时平均 0.25px 一颗，
        // 于是"没选中的那些"必然合并成 1 段，而第 1000 颗选中的要单独成段、落在它的范围里。
        let doc = doc_kinds(&(0..2000).map(|b| (NoteKind::Tap, b, 4, 0)).collect::<Vec<_>>());
        let st = EditorState::new(chart_from_doc(&doc));
        let geom = TimelineGeom::new(rect(500.0), &st, 2.0);
        assert!(geom.duration > 250.0, "总长 {}（2000 颗 tap = 500 拍）", geom.duration);
        let spans = line_spans(st.selected().expect("总有一条判定线"), &geom, &sel_of(&[1000]));

        let tap = &spans[kind_row(VKind::Tap)];
        assert_eq!(tap.base.len(), 1, "密流应当合并成 1 段：{:?}", tap.base);
        assert_eq!(tap.selected.len(), 1, "选中的那颗要单独成段：{:?}", tap.selected);
        let (run, hit) = (tap.base[0], tap.selected[0]);
        assert!(hit.x0 >= run.x0 && hit.x1 <= run.x1, "选中段落在合并段之外：{run:?} vs {hit:?}");
        assert!(hit.x1 - hit.x0 < run.x1 - run.x0, "选中的只是密流里的一颗");
        // 其余 3 行没有 tap ⇒ 一条都不该画
        for r in 1..NOTE_ROWS {
            assert!(spans[r].base.is_empty() && spans[r].selected.is_empty(), "第 {r} 行凭空有东西");
        }
    }

    /// 4 行各管各的：hold 的长条**不会**把同时间的 tap/drag/flick 盖住（分行的直接好处）
    #[test]
    fn kinds_do_not_bleed_into_each_others_rows() {
        let doc = doc_kinds(&[
            (NoteKind::Tap, 0, 1, 0),
            (NoteKind::Hold, 0, 1, 8), // 0~8 拍的长条
            (NoteKind::Drag, 2, 1, 0),
            (NoteKind::Flick, 4, 1, 0),
        ]);
        let st = EditorState::new(chart_from_doc(&doc));
        let geom = TimelineGeom::new(rect(500.0), &st, 2.0);
        let spans = line_spans(st.selected().unwrap(), &geom, &std::collections::BTreeSet::new());
        for (r, row) in spans.iter().enumerate() {
            assert_eq!(row.base.len(), 1, "第 {r} 行应当各有一段：{spans:?}");
        }
        let hold = &spans[kind_row(VKind::Hold)].base[0];
        let tap = &spans[kind_row(VKind::Tap)].base[0];
        assert!(hold.x1 - hold.x0 > tap.x1 - tap.x0, "hold 是长条，tap 是一颗");
    }

    /// 矮/高时间轴里 4 行都不越界、不重叠（纯几何 —— 这类东西以前只能靠截图看）
    #[test]
    fn note_row_layout_stays_inside_the_axis() {
        for h in [64.0f32, 100.0, 189.0, 271.0, 400.0, 900.0] {
            let r = egui::Rect::from_min_size(egui::pos2(10.0, 100.0), egui::vec2(800.0, h));
            let lay = NoteRowLayout::new(r);
            assert!(lay.row_h >= 1.0 && lay.row_h <= NOTE_ROW_H_MAX, "h={h} 行高 {}", lay.row_h);
            for i in 0..NOTE_ROWS {
                let y = lay.y_of(i);
                assert!(y.is_finite() && y >= r.min.y - 0.01, "h={h} 第 {i} 行跑到轴上方：{y}");
                assert!(y + lay.row_h <= r.max.y + 0.01, "h={h} 第 {i} 行跑到轴下方：{y}");
                if i > 0 {
                    assert!(lay.y_of(i) > lay.y_of(i - 1) + lay.row_h - 0.01, "h={h} 第 {i} 行与上一行重叠");
                }
            }
            // 行不骑在底部事件条上（事件条的 6px 是"轨道事件"的地盘）
            let last = lay.y_of(NOTE_ROWS - 1) + lay.row_h;
            assert!(last <= r.max.y - EVENT_BAR_H + 0.01, "h={h} 音符条压住事件条：{last}");
        }
        // 时间轴隐藏（高度 0）时也不能算出一堆 NaN / 越界（`draw` 那时仍会被调用，只是全被裁掉）
        for h in [0.0f32, 1.0, 8.0] {
            let lay = NoteRowLayout::new(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, h)));
            assert!(lay.row_h.is_finite() && lay.top.is_finite());
            for i in 0..NOTE_ROWS {
                assert!(lay.y_of(i).is_finite(), "h={h} 第 {i} 行不是有限数");
                assert!(lay.y_of(i) >= 0.0, "h={h} 第 {i} 行跑到轴外：{}", lay.y_of(i));
            }
        }
    }

    /// **头条回归**：5 万音符整谱可见时只画个位数的矩形（改造前是"一颗音符一个矩形"= 50 000 个/帧）
    #[test]
    fn fifty_thousand_notes_collapse_to_a_handful_of_rects() {
        // 与 `bench/stress-50k.opm` 同一形状：BPM 180、每秒 150 颗、四种音符轮流
        let kinds = [NoteKind::Tap, NoteKind::Hold, NoteKind::Drag, NoteKind::Flick];
        let notes: Vec<(NoteKind, i64, i64, i64)> = (0..50_000i64)
            .map(|i| {
                let k = kinds[(i % 4) as usize];
                (k, i * 5, 250, if k == NoteKind::Hold { 25 } else { 0 })
            })
            .collect();
        let st = EditorState::new(chart_from_doc(&doc_kinds(&notes)));
        assert_eq!(st.chart.lines[0].notes.len(), 50_000);
        let geom = TimelineGeom::new(rect(1500.0), &st, 2.0);
        let spans = line_spans(st.selected().unwrap(), &geom, &std::collections::BTreeSet::new());

        let drawn: usize =
            spans.iter().map(|r| r.base.len() + r.selected.len()).sum();
        for (r, row) in spans.iter().enumerate() {
            assert!(!row.base.is_empty(), "第 {r} 行（{}）没画出来", KIND_ROWS[r].label());
        }
        assert!(
            drawn <= 4 * NOTE_ROWS,
            "5 万颗音符应当压成几十个矩形以内，实际 {drawn} 个（一颗一个就是 50 000 个）"
        );
    }
}
