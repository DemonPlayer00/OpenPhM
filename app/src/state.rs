//! 编辑器状态与**线优先**的视图模型。
//!
//! 依赖方向（这是本模块存在的理由）：
//! ```text
//!   Document
//!     └─ judge_lines[]            ← 父对象：位置/旋转/透明度/流速全部来自它的事件轨道
//!          ├─ layers[] → 事件轨道（moveX/moveY/rotate/alpha/speed）
//!          └─ notes[]            ← 子对象：只存"线本地坐标"，屏幕位置由父对象的表演决定
//! ```
//! 因此视图模型里 **`Chart` 是判定线的列表**，音符挂在各自线上；不存在"整谱一张扁平音符表"这种东西
//! （早先那版是扁平的，等于把父子关系丢掉，判定线一旋转音符就跟不上）。
//!
//! 这一层**不依赖任何 UI/渲染类型**，是 codec 与判定逻辑的落脚点。

use crate::doc::{Document, Event, NoteKind as DocKind};
use crate::perf::{sample_track, LinePerf, TimeMap};
use crate::doc::TRACKS;

/// 音符类型（与 `spec/note-types.json` 对齐，UI 层用字符串枚举而非裸整数）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NoteKind {
    Tap,
    Hold,
    Drag,
    Flick,
}

impl NoteKind {
    pub fn label(self) -> &'static str {
        match self {
            NoteKind::Tap => "Tap",
            NoteKind::Hold => "Hold",
            NoteKind::Drag => "Drag",
            NoteKind::Flick => "Flick",
        }
    }
    pub fn color(self) -> [f32; 4] {
        match self {
            NoteKind::Tap => [0.35, 0.65, 1.0, 1.0],
            NoteKind::Hold => [0.75, 0.90, 1.0, 1.0],
            NoteKind::Drag => [1.0, 0.92, 0.25, 1.0],
            NoteKind::Flick => [1.0, 0.45, 0.75, 1.0],
        }
    }
}

/// 音符（视图侧）：坐标是**线本地坐标**，屏幕位置由父线的表演变换得到
#[derive(Clone, Copy, Debug)]
pub struct Note {
    /// 在 `doc.judge_lines[line].notes` 里的下标 —— 回写命令要用它
    pub doc_index: usize,
    /// 判定时刻（秒）
    pub time: f64,
    /// Hold 结束时刻（秒）；非 Hold 等于 time
    pub end: f64,
    /// 线本地 X（RPE 单位，线宽 ±675）
    pub lane_x: f32,
    pub kind: NoteKind,
    /// 假音符：无判定、不计分、不计物量（本格式的核心特性之一，编辑器要能一眼看出来）
    pub is_fake: bool,
}

impl Note {
    /// 判定时刻换算成拍（叠加层纵轴用拍）
    pub fn time_beat(&self, tmap: &TimeMap) -> f64 {
        tmap.beat(self.time)
    }
    pub fn end_beat(&self, tmap: &TimeMap) -> f64 {
        tmap.beat(self.end)
    }
}

/// 五条事件轨道的枚举（顺序与 `doc::TRACKS` 一致）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrackId {
    MoveX = 0,
    MoveY = 1,
    Rotate = 2,
    Alpha = 3,
    Speed = 4,
}

impl TrackId {
    pub const ALL: [TrackId; 5] = [
        TrackId::MoveX,
        TrackId::MoveY,
        TrackId::Rotate,
        TrackId::Alpha,
        TrackId::Speed,
    ];
    pub fn key(self) -> &'static str {
        TRACKS[self as usize]
    }
    pub fn label(self) -> &'static str {
        match self {
            TrackId::MoveX => "移动 X",
            TrackId::MoveY => "移动 Y",
            TrackId::Rotate => "旋转",
            TrackId::Alpha => "透明度",
            TrackId::Speed => "流速",
        }
    }
    /// 列表里用的短单位（面板宽度有限，长单位会把行挤折）
    pub fn short_unit(self) -> &'static str {
        match self {
            TrackId::MoveX | TrackId::MoveY => "RPE",
            TrackId::Rotate => "度",
            TrackId::Alpha => "0–1",
            TrackId::Speed => "×10",
        }
    }
    pub fn unit(self) -> &'static str {
        match self {
            TrackId::MoveX | TrackId::MoveY => "RPE 单位",
            TrackId::Rotate => "度",
            TrackId::Alpha => "0–1",
            TrackId::Speed => "倍（10=基准）",
        }
    }
}

/// 一条事件轨道在视图侧的缓存：**拍域事件** + **秒域采样折线**（时间轴用）
#[derive(Clone, Default, Debug)]
pub struct TrackView {
    /// 事件本体（拍域，直接来自 doc；求值走 `perf::eval_events`，缓动才不会被近似掉）
    pub events: Vec<Event>,
    /// 采样折线 (秒, 值)，供时间轴画曲线
    pub curve: Vec<[f32; 2]>,
    /// 折线值域（画曲线时纵向归一）
    pub min: f32,
    pub max: f32,
}

impl TrackView {
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
    /// 事件在拍域的覆盖范围（时间轴画事件条）
    pub fn span_sec(&self, tmap: &TimeMap) -> (f64, f64) {
        match (self.events.first(), self.events.last()) {
            (Some(a), Some(b)) => (tmap.sec(a.start.to_f64()), tmap.sec(b.end.to_f64())),
            _ => (0.0, 0.0),
        }
    }
}

/// 一条判定线（父对象）及其子音符
#[derive(Clone, Debug)]
pub struct Line {
    /// `doc.judge_lines` 里的下标
    pub index: usize,
    pub name: String,
    pub z_order: i32,
    pub is_cover: bool,
    pub bpm_factor: f32,
    /// 子对象：按时间升序
    pub notes: Vec<Note>,
    /// 五条事件轨道
    pub tracks: [TrackView; 5],
}

impl Line {
    pub fn note_count(&self) -> usize {
        self.notes.len()
    }
    pub fn event_count(&self) -> usize {
        self.tracks.iter().map(|t| t.events.len()).sum()
    }
    pub fn track(&self, id: TrackId) -> &TrackView {
        &self.tracks[id as usize]
    }
    /// 在给定**秒**处求这条线的表演状态
    pub fn perf(&self, tmap: &TimeMap, sec: f64) -> LinePerf {
        let beat = tmap.beat(sec);
        let ev: [&[Event]; 5] = [
            &self.tracks[0].events,
            &self.tracks[1].events,
            &self.tracks[2].events,
            &self.tracks[3].events,
            &self.tracks[4].events,
        ];
        // 直接在这里按轨道求值：避免为"借用视图"再抄一份数组
        let mut p = LinePerf::default();
        if let Some(v) = crate::perf::eval_events(ev[0], beat) {
            p.x = v as f32;
        }
        if let Some(v) = crate::perf::eval_events(ev[1], beat) {
            p.y = v as f32;
        }
        if let Some(v) = crate::perf::eval_events(ev[2], beat) {
            p.rotate_deg = v as f32;
        }
        if let Some(v) = crate::perf::eval_events(ev[3], beat) {
            p.alpha = (v as f32).clamp(0.0, 1.0);
        }
        if let Some(v) = crate::perf::eval_events(ev[4], beat) {
            p.speed = v as f32;
        }
        p
    }
    /// 命中判定：屏幕坐标（RPE）是否落在这条线上（用于点选判定线）
    pub fn hit(&self, tmap: &TimeMap, sec: f64, point: [f32; 2], tol: f32, line_half_w: f32) -> bool {
        let p = self.perf(tmap, sec);
        // 逆变换回本地坐标（旋转可逆），再看 |y_local| 与 |x_local|
        let (s, c) = (-p.rotate_deg).to_radians().sin_cos();
        let dx = point[0] - p.x;
        let dy = point[1] - p.y;
        let lx = dx * c - dy * s;
        let ly = dx * s + dy * c;
        ly.abs() <= tol && lx.abs() <= line_half_w
    }
}

// ---------------------------------------------------------------- 坐标系常量
//
// RPE 口径（Phira Documents · 普通事件 / 音符，CC-BY-4.0）：
// **坐标系锚点位于屏幕中心，X 轴范围 -675 ~ 675，Y 轴范围 -450 ~ 450**，
// 音符的 positionX 就是"相对判定线中心点的 X"。即窗口是 1350×900 的 3:2 矩形，原点在中心。
//
// 判定线**长度**在 RPE 的格式里没有字段（判定线对象只有 name/bpmFactor/zOrder/isCover/
// layers/notes），它是编辑器与播放器的呈现约定。所以 opm 也把它放在**编辑器设置**里，
// 不进格式 —— 与"变速 hold 的语义交播放器"同一个判断。

/// 窗口边界半宽/半高（RPE 坐标系）
pub const RPE_WINDOW_HALF_W: f32 = 675.0;
pub const RPE_WINDOW_HALF_H: f32 = 450.0;
/// 窗口尺寸（= 2×半宽/半高）
pub const RPE_WINDOW_W: f32 = RPE_WINDOW_HALF_W * 2.0;
pub const RPE_WINDOW_H: f32 = RPE_WINDOW_HALF_H * 2.0;
/// 判定线长度的默认值：与窗口同宽（±675）。可在 GUI/CLI 调整（`--line-len`）。
pub const RPE_LINE_HALF_W: f32 = RPE_WINDOW_HALF_W;

/// 谱面视图：**判定线的列表**（按 zOrder 排序，绘制顺序）
#[derive(Clone, Debug)]
pub struct Chart {
    pub name: String,
    /// 谱面时长（秒，含 2 秒尾巴）。**它只描述谱面自己**（事件与音符的末端 + 尾巴），
    /// 时间轴的总长请用 [`EditorState::timeline_duration`]。
    pub duration: f64,
    /// 拍 ↔ 秒映射（多 BPM 分段线性）
    pub tmap: TimeMap,
    /// 一拍多少秒（当前 BPM，时间轴刻度用）
    pub beat_interval: f64,
    pub bpm: f64,
    /// 判定线（已按 zOrder 稳定排序）
    pub lines: Vec<Line>,
}

impl Chart {
    pub fn line(&self, i: usize) -> Option<&Line> {
        self.lines.get(i)
    }
    pub fn total_notes(&self) -> usize {
        self.lines.iter().map(Line::note_count).sum()
    }
    /// 该线的绘制序号（zOrder 相同时按文档顺序）
    pub fn zorder(&self, i: usize) -> i32 {
        self.lines.get(i).map(|l| l.z_order).unwrap_or(0)
    }
}

/// 由 opm 文档构建**一条判定线**的视图（只含属性 + 音符，不含轨道）—— 结构重建用
pub fn line_shell(doc: &Document, index: usize, tmap: &TimeMap) -> Option<Line> {
    let src = doc.judge_lines.get(index)?;
    Some(Line {
        index,
        name: src.name.clone(),
        z_order: src.z_order,
        is_cover: src.is_cover,
        bpm_factor: src.bpm_factor,
        notes: notes_of(doc, index, tmap),
        tracks: Default::default(),
    })
}

/// 取该线的音符（时间升序，带 doc 下标）
pub fn notes_of(doc: &Document, index: usize, tmap: &TimeMap) -> Vec<Note> {
    let Some(src) = doc.judge_lines.get(index) else {
        return Vec::new();
    };
    let mut out: Vec<Note> = src
        .notes
        .iter()
        .enumerate()
        .map(|(i, n)| Note {
            doc_index: i,
            time: tmap.sec(n.start.to_f64()),
            end: tmap.sec(n.end_beat().to_f64()),
            lane_x: n.lane_x,
            is_fake: n.is_fake,
            kind: match n.kind {
                DocKind::Tap => NoteKind::Tap,
                DocKind::Hold => NoteKind::Hold,
                DocKind::Drag => NoteKind::Drag,
                DocKind::Flick => NoteKind::Flick,
            },
        })
        .collect();
    out.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// 取该线的五条事件轨道（拍域事件 + 秒域采样折线）
pub fn tracks_of(doc: &Document, index: usize, tmap: &TimeMap) -> [TrackView; 5] {
    let Some(src) = doc.judge_lines.get(index) else {
        return Default::default();
    };
    let mut out: [TrackView; 5] = Default::default();
    for (k, id) in TrackId::ALL.iter().enumerate() {
        let events = crate::perf::track_events(src, id.key());
        if events.is_empty() {
            continue;
        }
        let curve = sample_track(&events, tmap, 4);
        let (mut min, mut max) = (f32::INFINITY, f32::NEG_INFINITY);
        for p in &curve {
            min = min.min(p[1]);
            max = max.max(p[1]);
        }
        out[k] = TrackView {
            events,
            curve,
            min: if min.is_finite() { min } else { 0.0 },
            max: if max.is_finite() { max } else { 0.0 },
        };
    }
    out
}

/// 把文档整体转成视图（引导期 / BPM 或判定线集合变化时用）
pub fn chart_from_doc(doc: &Document) -> Chart {
    let tmap = TimeMap::from_doc(doc);
    let mut lines: Vec<Line> = (0..doc.judge_lines.len())
        .filter_map(|i| {
            let mut l = line_shell(doc, i, &tmap)?;
            l.tracks = tracks_of(doc, i, &tmap);
            Some(l)
        })
        .collect();
    // zOrder 小的先画（在后），大的后画（在前）；同 zOrder 保持文档顺序
    lines.sort_by(|a, b| a.z_order.cmp(&b.z_order).then(a.index.cmp(&b.index)));
    let bpm = tmap.bpm_at(0.0);
    Chart {
        name: doc.meta.name.clone(),
        duration: tmap.duration,
        beat_interval: 60.0 / bpm.max(1.0),
        bpm,
        tmap,
        lines,
    }
}

/// 网格设置（**视图设置**，不进文档、不进撤销栈）。
///
/// 网格定义了"音符可以放在哪儿"：两个方向各有一个数 ——
/// · **拍方向（纵向轴）**：每拍 `beat_div` 条 → 步长 `1/beat_div` 拍；
/// · **坐标方向（横向轴）**：可见窗口 `lane_div` 等分 → 步长 `1350/lane_div` RPE 单位。
///
/// 拖拽与双击放置**总是**吸附到格点（两轴网格的交叉点）—— 界面上不存在"格点之间的音符"，
/// 自由坐标仍然可以走命令（agent 想放哪儿就放哪儿）。吸附还有个硬理由：它保证写回文档的拍
/// 是**有理数 k/beat_div**，而不是浮点抖出来的 0.3333333。
#[derive(Clone, Copy, Debug)]
pub struct GridCfg {
    /// 拍方向：每拍几条网格线（1 = 只画整拍）
    pub beat_div: u32,
    /// 坐标方向：**整个可见窗口平均切成几列**（列宽 = 1350/lane_div RPE 单位），奇偶都行。
    ///
    /// 格点是**边界对齐**的：`-675 + k·(1350/N)`，两端恒为窗口边界 ⇒ 任何 N 都放得下。
    /// 代价是**奇数 N 时 laneX = 0 不是格点**：中轴线照画（视觉参考），但吸附不落在它上面。
    /// 偶数 N（默认 16）时中轴本身就是一条格线，两者重合。
    pub lane_div: u32,
}

impl Default for GridCfg {
    fn default() -> Self {
        Self {
            beat_div: 4,
            lane_div: 16,
        }
    }
}

/// 时间轴该占多高（**纯函数**，含"窗口很矮"的边界）。
///
/// 这条是从一次真 panic 里抽出来的：原来是
/// `(h * frac).clamp(64.0, h * 0.7)`，当窗口矮到 `h*0.7 < 64` 时 `clamp` 的 min > max、
/// 直接 panic（`min > max, or either was NaN. min = 64.0, max = 0.0`）。
/// 启动页→编辑页的转场把窗口从 620 高换成 900 高，中间那一帧就踩到了它；
/// 用户把窗口拖矮同样会踩。**clamp 的上界依赖另一个量时，必须先把它夹进合法区间。**
pub fn timeline_height(full_h: f32, frac: f32, show: bool) -> f32 {
    if !show || !full_h.is_finite() || full_h <= 0.0 {
        return 0.0;
    }
    let max_h = (full_h * 0.7).max(0.0);
    let want = full_h * frac.clamp(0.0, 1.0);
    let lo = 64.0_f32.min(max_h);
    want.clamp(lo, max_h.max(lo))
}

/// 编辑区里一条细线至少要有的像素间距（再密就糊成一片）
pub const GRID_MIN_LINE_PX: f64 = 1.2;
/// 编辑区高度的名义值（用于"这个细分画得出来吗"的判断；实际布局在 400~800px 之间）
pub const GRID_NOMINAL_PANE_H: f64 = 600.0;

impl GridCfg {
    /// 当前缩放下**真正画得出来**的拍细分（≤ 设定的 `beat_div`，且必定是它的约数）。
    ///
    /// 这一个数同时决定"画多少线"和"吸附到哪" —— 两者必须一致，否则就是用户两次报的那个毛病：
    /// ①改了数量看不到变化（细线被抽稀掉了）、②吸附落在没有线的位置上。
    /// 设定值太密时向下取它的**最大约数**（保持"整拍/半拍"这类整齐关系，不会变成 1/37 这种怪步长）。
    pub fn effective_beat_div(&self, beats_visible: f64) -> u32 {
        let want = self.beat_div.max(1);
        let budget = (GRID_NOMINAL_PANE_H / GRID_MIN_LINE_PX).max(4.0);
        let mut cand = want;
        while cand > 1 {
            if beats_visible.max(1.0) * cand as f64 <= budget {
                return cand;
            }
            // 向下找 want 的最大约数
            cand = (1..cand).rev().find(|d| want % d == 0).unwrap_or(1);
        }
        1
    }

    /// 拍方向步长（拍）
    pub fn v_step_beats(&self) -> f64 {
        1.0 / self.beat_div.max(1) as f64
    }
    /// 坐标方向步长（RPE 单位）
    pub fn h_step_rpe(&self) -> f32 {
        RPE_WINDOW_W / self.lane_div.max(1) as f32
    }
    /// 拍 → 最近的格点
    pub fn snap_beat(&self, beat: f64) -> f64 {
        (beat * self.beat_div.max(1) as f64).round() / self.beat_div.max(1) as f64
    }
    /// 第 k 条坐标格线的 laneX（k = 0..=lane_div）。
    ///
    /// **边界对齐**：格点 = `-675 + k·(1350/N)`，所以 k=0 与 k=N 正好落在窗口边界 ±675 上，
    /// 与奇偶无关。旧公式是"以 0 为中心"的 `k·(1350/N)`（k 取正负各 N/2 条）——
    /// 那套在**奇数**等分下最外的格点会跑到窗口外（5 等分时 676 最近的是 810），
    /// 于是被迫"只允许偶数等分"。用户否掉了这个限制：写几等分就整窗平均切几份。
    pub fn lane_of_index(&self, k: i32) -> f32 {
        -RPE_WINDOW_HALF_W + k as f32 * self.h_step_rpe()
    }

    /// laneX → 最近的格点（**边界对齐**的整窗等分），并夹到**官方窗口** ±675。
    ///
    /// 注意：奇数等分时 `laneX = 0` **不是**格点（吸附不会落在中轴线上）——
    /// 中轴线仍然照画（`overlay` 里单独画一条），它是视觉参考，不参与吸附。
    pub fn snap_lane(&self, lane: f32) -> f32 {
        self.snap_lane_windowed(lane, 0.0)
    }

    /// laneX → 最近格点，夹取范围是**当前显示窗口** `[off−675, off+675]`。
    ///
    /// 格点本身是**无限延伸**的 `-675 + k·step`（锚在官方坐标系上，平移后仍然对齐），
    /// 夹取只在**格点下标**上做 —— 所以结果必定仍是格点，且落在看得见的范围里。
    /// `off ≠ 0` 时格点会延伸出官方窗口 ⇒ 可以编辑 `|laneX| > 675` 的音符
    /// （那是格式允许的：`validate` 只报**警告**"超出 RPE 坐标系 ±675"，不是错误）。
    pub fn snap_lane_windowed(&self, lane: f32, offset: f32) -> f32 {
        let step = self.h_step_rpe().max(1e-6);
        let (k_lo, k_hi) = self.lane_index_range(offset);
        let k = ((lane + RPE_WINDOW_HALF_W) / step).round() as i32;
        let k = k.clamp(k_lo.min(k_hi), k_hi.max(k_lo));
        -RPE_WINDOW_HALF_W + k as f32 * step
    }

    /// 显示窗口（偏移 `offset`）覆盖到的格点下标范围（含两端）。
    ///
    /// `offset = 0` 时正好是 `0..=lane_div`（端点即官方窗口边界）；偏移后向两侧延伸。
    pub fn lane_index_range(&self, offset: f32) -> (i32, i32) {
        let step = self.h_step_rpe().max(1e-6);
        // 格点 -675 + k·step 落在 [offset-675, offset+675] 内的 k 区间
        let k_lo = ((offset - RPE_WINDOW_HALF_W + RPE_WINDOW_HALF_W) / step).ceil();
        let k_hi = ((offset + RPE_WINDOW_HALF_W + RPE_WINDOW_HALF_W) / step).floor();
        (k_lo as i32, (k_hi as i32).max(k_lo as i32))
    }

    /// 中轴（laneX = 0）是不是格点：**偶数**等分时是（N/2 那条线），奇数等分时不是。
    pub fn center_is_lattice(&self) -> bool {
        self.lane_div.max(1) % 2 == 0
    }

    /// 把任意等分数规整到合法范围（1..=128）。
    ///
    /// **不再强制偶数**：整窗等分下奇数同样放得下（端点始终是格线），
    /// 强制偶数只会让"3 等分""5 等分"这类需求变成做不到的事。
    pub fn normalize_lane_div(d: u32) -> u32 {
        d.clamp(1, 128)
    }
    /// 拍 → 有理数 `[n, d]`（写回文档用；吸附之后本就是这个网格上的分数）
    pub fn beat_json(&self, beat: f64) -> [i64; 2] {
        let d = self.beat_div.max(1) as i64;
        [((beat * d as f64).round() as i64), d]
    }
    /// 一条格线上有没有东西的判定容差（浮点比较用）
    pub fn on_lattice(lane: f32, beat: f64) -> bool {
        (lane.is_finite() && beat.is_finite())
            && ((lane * 1000.0).round() / 1000.0 - lane).abs() < 1e-3
    }
}

/// 正在"跟随鼠标"的 hold（按下 R 之后、放置之前）。
///
/// **它不是文档数据**：起点/终点都只是草稿，直到用户按 R/回车才发 `add_note`。
/// 放在视图状态里，是因为"Esc 取消""鼠标移动改长度"都属于看与操作 —— 不该进撤销栈。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PendingHold {
    pub lane_x: f32,
    pub start_beat: f64,
    pub end_beat: f64,
}

/// **草稿区间**的规则（hold 与事件块草稿共用一份实现）：
/// 鼠标移动改**终点**（保底一个格点，反向拖不会把草稿拖没）、拖控制杆改一端且不许交叉。
///
/// 抽成自由函数而不是各写一遍：这两条规则是"手感"的全部，两份实现迟早会分叉
/// （一份允许反向、一份不允许，用户就会觉得"有时能拖没有时不能"）。
fn follow_span(start: &mut f64, end: &mut f64, beat: f64, min_len: f64) {
    *end = beat.max(*start + min_len.max(1e-6));
}

fn resize_span(start: &mut f64, end: &mut f64, edge: EventEdge, beat: f64, min_len: f64) {
    let min = min_len.max(1e-6);
    match edge {
        EventEdge::Start => *start = beat.min(*end - min),
        EventEdge::End => *end = beat.max(*start + min),
    }
}

/// 提交时的 `(起点, 终点)`（哪一头在前面都给出正序）
fn span_of(start: f64, end: f64) -> (f64, f64) {
    if start <= end {
        (start, end)
    } else {
        (end, start)
    }
}

impl PendingHold {
    /// 默认长度：一个格点（按下 R 的那一帧就至少这么长，免得"刚按下就零长"）
    pub fn new(lane_x: f32, start_beat: f64, min_len: f64) -> Self {
        let min = min_len.max(1e-6);
        Self {
            lane_x,
            start_beat,
            end_beat: start_beat + min,
        }
    }

    /// 鼠标移动：改**终点**。反向拖不会把长条拖没（保底 `start + min_len`）。
    pub fn follow(&mut self, beat: f64, min_len: f64) {
        follow_span(&mut self.start_beat, &mut self.end_beat, beat, min_len);
    }

    /// 拖控制杆：改起点或终点（另一个端点不动，且不许交叉）
    pub fn resize(&mut self, edge: EventEdge, beat: f64, min_len: f64) {
        resize_span(&mut self.start_beat, &mut self.end_beat, edge, beat, min_len);
    }

    /// 提交时的 `(起点, 终点)`
    pub fn span(&self) -> (f64, f64) {
        span_of(self.start_beat, self.end_beat)
    }

    /// 长度（拍）
    pub fn len(&self) -> f64 {
        (self.end_beat - self.start_beat).abs()
    }

    pub fn is_empty(&self) -> bool {
        self.len() <= 1e-9
    }
}

/// 正在跟随鼠标的**事件块草稿**（在事件区按键之后、放下之前）。
///
/// 与 [`PendingHold`] 同一套流程（起点定在指针处、长度随鼠标、Esc 取消、R/回车/左键放下），
/// 区别只是它落在**某条轨道**上 —— 轨道决定"改的是移动还是透明度"。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PendingEvent {
    pub track: TrackId,
    pub layer: usize,
    pub start_beat: f64,
    pub end_beat: f64,
}

impl PendingEvent {
    pub fn new(track: TrackId, layer: usize, start_beat: f64, min_len: f64) -> Self {
        let min = min_len.max(1e-6);
        Self {
            track,
            layer,
            start_beat,
            end_beat: start_beat + min,
        }
    }

    pub fn follow(&mut self, beat: f64, min_len: f64) {
        follow_span(&mut self.start_beat, &mut self.end_beat, beat, min_len);
    }

    pub fn resize(&mut self, edge: EventEdge, beat: f64, min_len: f64) {
        resize_span(&mut self.start_beat, &mut self.end_beat, edge, beat, min_len);
    }

    pub fn span(&self) -> (f64, f64) {
        span_of(self.start_beat, self.end_beat)
    }

    pub fn len(&self) -> f64 {
        (self.end_beat - self.start_beat).abs()
    }

    pub fn is_empty(&self) -> bool {
        self.len() <= 1e-9
    }
}

/// 编辑器视图状态（与文档无关的部分）
pub struct EditorState {
    pub chart: Chart,
    pub playhead: f64,
    pub playing: bool,
    pub speed: f64,
    /// 选中的判定线（视图序，非 doc 下标）——**选中是视图状态，不属于文档**
    pub selected_line: usize,
    /// 选中的事件轨道
    pub selected_track: TrackId,
    /// 选中的事件下标（该轨道内）
    pub selected_event: Option<usize>,
    /// 选中的音符（该线内的时间序下标）
    pub selected_note: Option<usize>,
    /// 按 R 之后正在跟随鼠标的 hold（`None` = 没有待放置的长条）。
    /// **视图状态**：Esc 取消、鼠标改长度都在这一层，不进文档。
    pub pending_hold: Option<PendingHold>,
    /// 在事件区按键之后正在跟随鼠标的事件块草稿（与 hold **互斥**：同时只放一个东西）
    pub pending_event: Option<PendingEvent>,
    /// 演奏区垂直可视范围（秒）：以播放头为基准往后看 `lookahead` 秒
    pub lookahead: f64,
    /// 是否绘制**窗口边界框**（RPE 的 ±675 × ±450，即 1350×900）
    pub show_boundary: bool,
    /// 判定线半长（编辑器设置，默认 = 窗口半宽 675 ⇒ 线长 1350）
    pub line_half_w: f32,
    /// 网格与吸附（视图设置）
    pub grid: GridCfg,
    /// 编辑区叠加层的**视图设置**（开关与可见拍数；黑度在渲染侧）
    pub overlay_enabled: bool,
    pub overlay_beats: f64,
    /// 音符区的**窗口 X 偏移**（视图状态，文档里没有这个字段）：
    /// 把音符区显示的 laneX 区间平移（`[off−675, off+675]`），用来查看/编辑**窗口外**的音符。
    /// 只影响编辑区怎么显示与吸附，不影响演奏区（预览永远显示真实窗口）。
    pub window_offset_x: f32,
    /// 边界外压暗的 alpha（**目标色彩空间下的名义值**）。
    /// 若渲染目标是 sRGB，混合发生在线性空间，同样的名义 alpha 观感会弱得多 ——
    /// 调用方用 [`crate::render::dim_alpha_for`] 按目标格式换算，保证 GUI 与无头出图观感一致。
    pub boundary_dim: f32,
    /// 播放头由单调时钟推进（后续换音频时钟）
    started: Option<std::time::Instant>,
    start_playhead: f64,
    /// **乐曲时长（秒）**：时间轴的**总长**按它来（`None` = 没有音乐/还没解码出来）。
    ///
    /// 为什么要单独存一个：`chart.duration` 是**谱面自身**的跨度（末尾还留 2 秒尾巴），
    /// 一份刚建的谱面几乎是 0 ⇒ 时间轴只有 2 秒（用户报的"默认总长度只有2秒"）。
    /// 音乐一装上，时间轴就该和歌一样长：你才滚得到副歌去写谱。
    music_len: Option<f64>,
}

impl EditorState {
    pub fn new(chart: Chart) -> Self {
        Self {
            chart,
            playhead: 0.0,
            playing: false,
            speed: 1.0,
            selected_line: 0,
            selected_track: TrackId::Alpha,
            selected_event: None,
            selected_note: None,
            pending_hold: None,
            pending_event: None,
            lookahead: 2.0,
            show_boundary: true,
            line_half_w: RPE_LINE_HALF_W,
            grid: GridCfg::default(),
            overlay_enabled: true,
            // 时间轴默认缩放：**放大档**（用户要求"拉长 4 倍"：原来 32 拍可见 → 8 拍可见，
            // 每拍占的屏幕高度 ×4）。8 拍可见下一拍 ≈ 75px，1/4 拍细分线 ≈ 19px、1/16 拍 ≈ 4.7px 都看得清，
            // "最细的线画明显"这件事只有在放得足够大时才真的有意义。
            overlay_beats: Self::DEFAULT_OVERLAY_BEATS,
            window_offset_x: 0.0,
            boundary_dim: crate::render::DIM_ALPHA_DEFAULT,
            started: None,
            start_playhead: 0.0,
            music_len: None,
        }
    }

    /// 设定**乐曲时长**（秒）。`None` = 没有音乐（或还没解码出来）；非正数/非有限值一律当"没有"。
    ///
    /// 调用点：音频解析完成时、运行中替换音频时（GUI 在拿到 `Audio` 之后调一次）。
    pub fn set_music_len(&mut self, sec: Option<f64>) {
        self.music_len = sec.filter(|s| s.is_finite() && *s > 0.0);
    }

    /// **时间轴总长（拍）**：`max(乐曲时长, 最后一个 note/事件) + 10 拍`（用户给定的公式）。
    ///
    /// 为什么要 +10 拍：谱面末尾那一点总得留出来，否则最后一个音符就贴在时间轴右边缘上，
    /// 既看不清也点不到。**这 10 拍只是显示留白，绝不写进文件** ——
    /// 文档里根本没有"总长"这个字段（`chart_end` 是**由内容算出来的**只读视图，见 `doc::chart_end`），
    /// 所以这条在结构上就不可能被写进去（`tests/lifecycle.rs` 有一条守着它）。
    ///
    /// "不要变"：它只由**文档内容**与**乐曲时长**决定，与播放头、滚动、缩放、窗口大小都无关 ——
    /// 拖时间轴或缩放时总长不会跳。
    pub fn timeline_end_beat(&self) -> f64 {
        let tmap = &self.chart.tmap;
        let content = tmap.end_beat; // note 与事件末端（`TimeMap::from_doc` 里算的）
        let music = self.music_len.map(|sec| tmap.beat(sec)).unwrap_or(0.0);
        content.max(music) + Self::TIMELINE_TAIL_BEATS
    }

    /// 时间轴总长（秒）。时间轴绘制、播放头上限、可见范围都以它为准。
    pub fn timeline_duration(&self) -> f64 {
        self.chart.tmap.sec(self.timeline_end_beat())
    }

    /// **编辑区（叠加层）当前显示的时间跨度**（秒，`[底部, 顶部]`）。
    ///
    /// 与 `overlay::draw` 里的 `anchor` 是**同一套定义**（`lead_beats` 由调用方给，那是叠加层的配置）：
    /// 底部 = 播放头所在拍退回 `lead_beats`，顶部 = 底部 + 可见拍数。
    /// 时间轴上的**黄线取底部**（"起点"），**浅色窗口取这一整段**（"编辑区从底层到顶层"）。
    pub fn edit_area_span(&self, lead_beats: f64) -> (f64, f64) {
        let tmap = &self.chart.tmap;
        let bottom = tmap.beat(self.playhead) - lead_beats;
        let top = bottom + self.overlay_beats.max(4.0);
        (tmap.sec(bottom), tmap.sec(top))
    }

    /// 当前判定线的 **doc 下标**
    /// 拍网格步长（拍）：吸附与"最短 hold 长度"共用同一个粒度
    pub fn beat_step(&self) -> f64 {
        1.0 / self.effective_beat_div().max(1) as f64
    }

    /// 开始放置一个 hold（按下 R）：起点取指针处，终点先给一个格点。
    /// **同时只允许一个草稿** —— 起 hold 就把事件草稿清掉。
    pub fn begin_pending_hold(&mut self, lane_x: f32, start_beat: f64) {
        self.pending_event = None;
        self.pending_hold = Some(PendingHold::new(lane_x, start_beat, self.beat_step()));
    }

    /// 开始放置一个事件块（事件区按键）：落在 `track` 上，起点取指针处
    pub fn begin_pending_event(&mut self, track: TrackId, start_beat: f64) {
        self.pending_hold = None;
        self.pending_event = Some(PendingEvent::new(track, 0, start_beat, self.beat_step()));
    }

    /// 鼠标移动 ⇒ 哪个草稿在跟随就改哪个的长度
    pub fn follow_pending_event(&mut self, beat: f64) {
        let step = self.beat_step();
        if let Some(e) = self.pending_event.as_mut() {
            e.follow(beat, step);
        }
    }

    /// 拖时间控制杆（事件草稿）
    pub fn resize_pending_event(&mut self, edge: EventEdge, beat: f64) {
        let step = self.beat_step();
        if let Some(e) = self.pending_event.as_mut() {
            e.resize(edge, beat, step);
        }
    }

    /// 取走事件草稿（提交用）
    pub fn take_pending_event(&mut self) -> Option<PendingEvent> {
        self.pending_event.take()
    }

    /// 现在有没有草稿（hold 或事件块）——面板用它决定"是不是在放置模式"
    pub fn drafting(&self) -> bool {
        self.pending_hold.is_some() || self.pending_event.is_some()
    }

    /// 取消任何草稿（Esc / 左键放下时都会走到这里）
    pub fn cancel_pending(&mut self) {
        self.pending_hold = None;
        self.pending_event = None;
    }

    /// 鼠标移动：hold 的长度跟着走（保底一个格点）
    pub fn follow_pending_hold(&mut self, beat: f64) {
        let step = self.beat_step();
        if let Some(h) = self.pending_hold.as_mut() {
            h.follow(beat, step);
        }
    }

    /// 拖时间控制杆：改待放置 hold 的起点或终点
    pub fn resize_pending_hold(&mut self, edge: EventEdge, beat: f64) {
        let step = self.beat_step();
        if let Some(h) = self.pending_hold.as_mut() {
            h.resize(edge, beat, step);
        }
    }

    /// 取走待放置的 hold（提交用：取走之后就没有"正在放"的状态了）
    pub fn take_pending_hold(&mut self) -> Option<PendingHold> {
        self.pending_hold.take()
    }

    /// 取消（Esc）：什么都没发生 —— 这就是"草稿不进撤销栈"的好处
    pub fn cancel_pending_hold(&mut self) {
        self.pending_hold = None;
    }

    pub fn selected_doc_line(&self) -> usize {
        self.chart
            .lines
            .get(self.selected_line)
            .map(|l| l.index)
            .unwrap_or(0)
    }

    /// 演奏区当前可见的时间区间（播放头 → 播放头 + 前瞻，**在谱面末尾截断**）。
    ///
    /// 它是纯视图计算（`ui_stats` 与渲染都用它），放在状态里是因为"可见范围"本来就是视图状态的一部分。
    pub fn visible_range(&self) -> (f64, f64) {
        (
            self.playhead,
            (self.playhead + self.lookahead).min(self.timeline_duration()),
        )
    }

    pub fn selected(&self) -> Option<&Line> {
        self.chart.lines.get(self.selected_line)
    }

    /// 时间轴总长在内容末尾之后**多留的拍数**（显示留白；不写进文件）。
    /// 单独一个常量是因为"为什么是 10"只该解释一次，而且测试要按它断言。
    pub const TIMELINE_TAIL_BEATS: f64 = 10.0;

    /// 时间轴缩放的允许范围（拍）：太小看不见结构，太大标注密到没法读
    pub const ZOOM_MIN_BEATS: f64 = 4.0;
    pub const ZOOM_MAX_BEATS: f64 = 256.0;

    /// 音符区"窗口 X 偏移"的允许范围（RPE 单位）：向右/左各推开一个窗口宽
    pub const WINDOW_OFFSET_MAX: f32 = 675.0;

    /// 默认可见拍数（视图状态，不进文档）。改这一个数就改默认缩放；
    /// 历史：曾是 32 拍可见，用户要求"拉长 4 倍" ⇒ 8 拍可见。
    pub const DEFAULT_OVERLAY_BEATS: f64 = 8.0;

    /// Ctrl+滚轮缩放：把可见拍数乘以倍率并夹到允许范围
    pub fn zoom_by(&mut self, factor: f64) {
        let f = if factor.is_finite() && factor > 0.0 { factor } else { 1.0 };
        self.overlay_beats =
            (self.overlay_beats * f).clamp(Self::ZOOM_MIN_BEATS, Self::ZOOM_MAX_BEATS);
    }

    /// 当前缩放下实际生效的拍细分（= 画出来的细分 = 吸附步长）
    pub fn effective_beat_div(&self) -> u32 {
        self.grid.effective_beat_div(self.overlay_beats)
    }

    /// 纵向吸附：**按画得出来的网格**吸（保证"吸附就是格点"）
    pub fn snap_beat(&self, beat: f64) -> f64 {
        let d = self.effective_beat_div().max(1) as f64;
        (beat * d).round() / d
    }

    /// 拍 → 有理数 `[n, d]`（d 用实际生效的细分，写回文档的就是格点上的分数）
    pub fn beat_json(&self, beat: f64) -> [i64; 2] {
        let d = self.effective_beat_div().max(1) as i64;
        [((beat * d as f64).round() as i64), d]
    }

    /// 音符区当前显示的 laneX 区间（左端, 右端）
    pub fn window_lane_range(&self) -> (f32, f32) {
        let o = self.window_offset_x;
        (o - RPE_WINDOW_HALF_W, o + RPE_WINDOW_HALF_W)
    }

    /// 设置窗口 X 偏移（夹到 ±WINDOW_OFFSET_MAX；坏输入归零）
    pub fn set_window_offset_x(&mut self, off: f32) {
        self.window_offset_x = if off.is_finite() {
            off.clamp(-Self::WINDOW_OFFSET_MAX, Self::WINDOW_OFFSET_MAX)
        } else {
            0.0
        };
    }

    /// 横向吸附（横向没有抽稀，直接转发给网格设置）。
    /// **带上窗口偏移**：平移之后吸附照样落在格点上，所以窗口外的坐标也能编辑。
    pub fn snap_lane(&self, lane: f32) -> f32 {
        self.grid.snap_lane_windowed(lane, self.window_offset_x)
    }

    pub fn toggle_play(&mut self) {
        self.set_playing(!self.playing);
    }

    /// 显式设置播放状态（音视频同步时由 App 调用：它还要同步音频流的起停）
    pub fn set_playing(&mut self, on: bool) {
        if on == self.playing {
            return;
        }
        self.playing = on;
        if on {
            self.started = Some(std::time::Instant::now());
            self.start_playhead = self.playhead;
        } else {
            self.started = None;
        }
    }

    pub fn seek(&mut self, t: f64) {
        self.playhead = t.clamp(0.0, self.timeline_duration());
        if self.playing {
            self.started = Some(std::time::Instant::now());
            self.start_playhead = self.playhead;
        }
    }

    /// 推进播放头。
    ///
    /// **有音频时以音频时钟为准**（`Audio::position_sec()` = 已送入设备的帧数 − 输出延迟），
    /// 墙钟只在无音频时兜底。这样谱面与声音不会各走各的时钟而累积漂移
    /// （S2 spike 实测两者的相对漂移 7.4 ppm，一个小时就是 27 ms —— 打拍子能听出来）。
    pub fn advance(&mut self, audio: Option<&crate::audio::Audio>) {
        if let (Some(a), true) = (audio, self.playing) {
            if a.is_playing() {
                // 音乐比谱面长时，播放头要跟着音乐走到曲末（否则歌还在放、游标却停在谱面末尾）
                self.playhead = a.position_sec().clamp(0.0, self.timeline_duration());
                return;
            }
        }
        if let Some(t0) = self.started {
            let dt = t0.elapsed().as_secs_f64() * self.speed;
            self.playhead = (self.start_playhead + dt).min(self.timeline_duration());
            if self.playhead >= self.timeline_duration() {
                self.playing = false;
                self.started = None;
            }
        }
    }

    /// 某条线在可见窗口内的音符下标区间（半开）
    pub fn visible_range_of(&self, line: usize) -> std::ops::Range<usize> {
        let Some(l) = self.chart.lines.get(line) else {
            return 0..0;
        };
        let lo = l.notes.partition_point(|n| n.time < self.playhead - 0.15);
        let hi = l.notes.partition_point(|n| n.time < self.playhead + self.lookahead);
        lo..hi
    }
}

// ------------------------------------------------- 事件边界的抓取规则

/// 事件块的哪一头
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventEdge {
    Start,
    End,
}

/// 两个事件头尾相接时，该抓哪一个？
///
/// 用户定的规则：**优先选中的那个**；两个都没选中时**选尾巴**（即前一个事件的末端，
/// 也就是"止于这一点"的那个）。这条规则很短，但正是"点下去抓到谁"的全部依据，所以单测钉住。
///
/// `cands` 是候选 (事件下标, 是哪一头)，`dist` 相同（都在同一 y 上）时才会走到这里。
///
/// **只此一份实现**：GUI 的 `overlay` 模块 `pub use` 它（拖拽与把手高亮共用同一个值），
/// 集成测试也直接调它 —— 规则住在库里而不是 bin 内，正是为了 `tests/boundary.rs` 能钉住真身。
pub fn prefer_edge(
    cands: &[(usize, EventEdge)],
    selected: Option<usize>,
) -> Option<(usize, EventEdge)> {
    if cands.is_empty() {
        return None;
    }
    if let Some(sel) = selected {
        if let Some(c) = cands.iter().find(|(i, _)| *i == sel) {
            return Some(*c);
        }
    }
    // 都没选中：选尾巴（End）—— 前一个事件止于此
    cands
        .iter()
        .find(|(_, e)| *e == EventEdge::End)
        .or_else(|| cands.first())
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 待放置的 hold：默认一个格点、反向拖有保底、控制杆拖不交叉、Esc 取消不动文档
    #[test]
    fn pending_hold_rules() {
        let mut st = EditorState::new(chart_from_doc(&crate::doc::Document::default()));
        let step = st.beat_step();
        assert!(step > 0.0);
        st.begin_pending_hold(100.0, 4.0);
        let h = st.pending_hold.unwrap();
        assert_eq!(h.lane_x, 100.0);
        assert_eq!(h.start_beat, 4.0);
        assert!((h.len() - step).abs() < 1e-9, "刚按下时有一个格点的长度：{}", h.len());

        // 鼠标往上（更大拍）⇒ 变长
        st.follow_pending_hold(8.0);
        assert_eq!(st.pending_hold.unwrap().span(), (4.0, 8.0));
        // 鼠标拖到起点以下 ⇒ 不许反向（保底一个格点）
        st.follow_pending_hold(1.0);
        let h = st.pending_hold.unwrap();
        assert!((h.len() - step).abs() < 1e-9, "反向拖不该把长条拖没：{h:?}");
        assert_eq!(h.start_beat, 4.0, "跟随鼠标改的是**终点**");

        // 控制杆：拖尾到 10 拍
        st.resize_pending_hold(EventEdge::End, 10.0);
        assert_eq!(st.pending_hold.unwrap().span(), (4.0, 10.0));
        // 拖头到 6 拍 ⇒ 起点动、终点不动
        st.resize_pending_hold(EventEdge::Start, 6.0);
        assert_eq!(st.pending_hold.unwrap().span(), (6.0, 10.0));
        // 头拖过尾 ⇒ 夹住（不许交叉）
        st.resize_pending_hold(EventEdge::Start, 99.0);
        let h = st.pending_hold.unwrap();
        assert!(h.start_beat < h.end_beat, "{h:?}");

        // 提交：取走之后状态清空
        let h = st.take_pending_hold().expect("有待放置的 hold");
        assert!(h.len() > 0.0);
        assert!(st.pending_hold.is_none());

        // 取消：清空
        st.begin_pending_hold(0.0, 2.0);
        st.cancel_pending_hold();
        assert!(st.pending_hold.is_none());
    }

    /// 事件块草稿：与 hold 同一套规则（跟随改终点、控制杆不许交叉），且**两者互斥**
    #[test]
    fn pending_event_rules_and_mutual_exclusion() {
        let mut st = EditorState::new(chart_from_doc(&crate::doc::Document::default()));
        let step = st.beat_step();
        st.begin_pending_hold(100.0, 1.0);
        assert!(st.drafting() && st.pending_hold.is_some());
        // 起事件草稿 ⇒ hold 草稿被清掉（同时只放一个东西）
        st.begin_pending_event(TrackId::Alpha, 4.0);
        assert!(st.pending_hold.is_none(), "两个草稿不能同时存在");
        let e = st.pending_event.unwrap();
        assert_eq!((e.track, e.layer), (TrackId::Alpha, 0));
        assert!((e.len() - step).abs() < 1e-9);

        st.follow_pending_event(8.0);
        assert_eq!(st.pending_event.unwrap().span(), (4.0, 8.0));
        // 反向拖：保底一个格点
        st.follow_pending_event(-100.0);
        let e = st.pending_event.unwrap();
        assert_eq!(e.start_beat, 4.0);
        assert!((e.len() - step).abs() < 1e-9, "{e:?}");
        // 控制杆
        st.resize_pending_event(EventEdge::End, 12.0);
        assert_eq!(st.pending_event.unwrap().span(), (4.0, 12.0));
        st.resize_pending_event(EventEdge::Start, 9.0);
        assert_eq!(st.pending_event.unwrap().span(), (9.0, 12.0));
        st.resize_pending_event(EventEdge::Start, 99.0);
        let e = st.pending_event.unwrap();
        assert!(e.start_beat < e.end_beat, "{e:?}");
        // 取走 / 取消
        assert!(st.take_pending_event().is_some());
        assert!(!st.drafting());
        st.begin_pending_event(TrackId::MoveX, 0.0);
        st.cancel_pending();
        assert!(!st.drafting());
    }

    /// 可见区间：从播放头起、前瞻那么长，**在时间轴末端截断**（曲末不该显示到时间轴之外）。
    /// 上限是 [`EditorState::timeline_duration`]（= max(音乐, 内容) + 10 拍），不是谱面自身的跨度。
    #[test]
    fn visible_range_is_clamped_to_the_timeline_end() {
        let mut st = EditorState::new(chart_from_doc(&crate::doc::Document::default()));
        st.set_music_len(Some(10.0)); // 默认 180BPM ⇒ 总长 = 10s + 10 拍 = 13.333s
        let total = st.timeline_duration();
        assert!((total - 13.3333333).abs() < 1e-4, "{total}");
        st.playhead = 1.0;
        st.lookahead = 2.0;
        assert_eq!(st.visible_range(), (1.0, 3.0));
        st.playhead = total - 0.5;
        assert_eq!(
            st.visible_range(),
            (total - 0.5, total),
            "末尾要夹住，别越过时间轴长度"
        );
        st.playhead = total;
        assert_eq!(st.visible_range(), (total, total), "已经到末尾时区间退化为一个点");
    }

    /// **时间轴总长 = max(乐曲时长, 最后一个 note/事件) + 10 拍**（用户给定的公式）。
    ///
    /// 还有两条同样重要的性质：**与视图状态无关**（拖/缩/滚动都不改它），
    /// 以及 **+10 只活在视图里**（文档的 `chart_end` 仍由内容算，见测试后半段）。
    #[test]
    fn timeline_length_is_content_and_music_plus_ten_beats() {
        let mut doc = crate::doc::Document::default();
        doc.bpm_list = vec![crate::doc::BpmEntry {
            start: crate::doc::Beat::zero(),
            bpm: 120.0, // 一拍 0.5 秒，好算
            foreign: Default::default(),
        }];
        let mut st = EditorState::new(chart_from_doc(&doc));
        // 空谱面（没有任何 note/事件）：总长就是那 10 拍留白 = 5 秒
        assert_eq!(st.timeline_end_beat(), 10.0);
        assert!((st.timeline_duration() - 5.0).abs() < 1e-9, "{}", st.timeline_duration());

        // 视角状态与它无关：拖、缩、换选中都不该动它
        let before = st.timeline_duration();
        st.playhead = 3.0;
        st.zoom_by(2.0);
        st.selected_track = TrackId::MoveX;
        assert_eq!(st.timeline_duration(), before, "视图状态不该改总长");

        // 有音乐：max(音乐, 内容) + 10 拍
        st.set_music_len(Some(60.0));
        assert!((st.timeline_duration() - 65.0).abs() < 1e-9, "60s 音乐 + 10 拍");
        // 3 秒音乐 = 6 拍 ⇒ max(0, 6) + 10 = 16 拍 = 8 秒（留白是**加在**内容/音乐之后的，不是下限）
        st.set_music_len(Some(3.0));
        assert!((st.timeline_duration() - 8.0).abs() < 1e-9, "{}", st.timeline_duration());

        // 内容比音乐长：按内容算
        let mut doc2 = crate::doc::Document::default();
        doc2.bpm_list = doc.bpm_list.clone();
        let line = &mut doc2.judge_lines[0];
        line.notes.push(crate::doc::Note::new(
            crate::doc::NoteKind::Tap,
            crate::doc::Beat::new(80, 1),
            0.0,
        ));
        let mut st2 = EditorState::new(chart_from_doc(&doc2));
        assert_eq!(st2.timeline_end_beat(), 90.0, "80 拍处的音符 + 10 拍");
        st2.set_music_len(Some(10.0));
        assert_eq!(st2.timeline_duration(), st2.chart.tmap.sec(90.0), "音乐更短 ⇒ 不影响");

        // **+10 不写进文件**：文档的 chart_end 仍由内容算（没有"总长"这个字段可写）
        assert_eq!(doc2.chart_end().to_f64(), 80.0, "文档末端是内容末端，不是 +10 之后的");
        let json = doc2.to_json();
        for k in ["chartEnd", "duration", "length", "timeline"] {
            assert!(json.get(k).is_none(), "文档里不该有 {k}：{json}");
        }
    }

    /// 编辑区窗口（黄线取底部、浅色带取整段）：底部 = 播放头退回 `lead_beats`，顶部 = +可见拍数
    #[test]
    fn edit_area_span_matches_the_overlay_window() {
        let mut doc = crate::doc::Document::default();
        doc.bpm_list = vec![crate::doc::BpmEntry {
            start: crate::doc::Beat::zero(),
            bpm: 120.0, // 1 拍 = 0.5 秒
            foreign: Default::default(),
        }];
        let mut st = EditorState::new(chart_from_doc(&doc));
        st.overlay_beats = 8.0;
        st.playhead = 10.0; // = 20 拍
        let (lo, hi) = st.edit_area_span(2.0);
        assert!((lo - 9.0).abs() < 1e-9, "底部 = 20 拍 - 2 拍 = 18 拍 = 9s（实际 {lo}）");
        assert!((hi - 13.0).abs() < 1e-9, "顶部 = 18 + 8 拍 = 13s（实际 {hi}）");
        // 缩放只改顶部（底部跟着播放头走）
        st.overlay_beats = 4.0;
        let (lo2, hi2) = st.edit_area_span(2.0);
        assert_eq!(lo2, lo, "底部不受缩放影响");
        assert!(hi2 < hi, "放大之后窗口更短：{hi2} < {hi}");
        // 播放头一动，两段一起动
        st.playhead = 0.0;
        let (lo3, _) = st.edit_area_span(2.0);
        assert!(lo3 < lo2);
    }

    /// 无音频时靠墙钟推进：到**总长**为止（有音乐就走到曲末，而不是谱面末尾）
    #[test]
    fn playback_advances_to_the_timeline_end_not_the_chart_end() {
        let mut st = EditorState::new(chart_from_doc(&crate::doc::Document::default()));
        st.set_music_len(Some(10.0));
        let total = st.timeline_duration();
        assert!(total > 10.0, "总长 = 音乐 + 10 拍留白：{total}");
        st.set_playing(true);
        st.start_playhead = 0.0;
        st.started = Some(std::time::Instant::now() - std::time::Duration::from_secs(60));
        st.advance(None);
        assert_eq!(st.playhead, total, "推到时间轴末端（= 音乐 + 留白）");
        assert!(!st.playing, "到末端就停");
    }

    #[test]
    fn timeline_height_survives_tiny_windows() {
        for h in [0.0f32, 1.0, 20.0, 60.0, 91.0, 200.0, 900.0] {
            for frac in [0.0f32, 0.25, 0.5, 1.0, 2.0, -1.0] {
                let t = timeline_height(h, frac, true);
                assert!(t.is_finite() && t >= 0.0 && t <= h.max(0.0), "h={h} frac={frac} → {t}");
            }
        }
        assert_eq!(timeline_height(900.0, 0.3, false), 0.0, "不显示时间轴就是 0");
        assert_eq!(timeline_height(f32::NAN, 0.3, true), 0.0, "坏输入不能 panic");
        let t = timeline_height(900.0, 0.3, true);
        assert!((t - 270.0).abs() < 1.0, "{t}");
    }
}
