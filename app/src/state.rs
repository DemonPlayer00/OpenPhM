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
    /// 音符自身的**流速倍率**（文档字段 `speed`，默认 1.0）。
    ///
    /// RPE 规范：它乘在"音符离判定线的距离"上 —— 不改到达时刻（打击时刻由 `time` 定），
    /// 只改这一路上落多远，所以演奏区要按它缩放纵向位置。
    pub speed: f32,
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
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
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
    /// 字符串键 → 轨道（**"键"与枚举的换算只此一份**）。
    ///
    /// 控制通道的 `{"op":"select","track":…}`、`OPM_EDIT_AUTO=event:<track>,…`、
    /// 冲突浏览器的跳转都要反查；各自手写 `.find(|id| id.key() == s)` 时，
    /// 少写一处就是"某一条轨道选不上"（而且只在那一条路径上）。
    pub fn from_key(key: &str) -> Option<TrackId> {
        TrackId::ALL.iter().copied().find(|t| t.key() == key)
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
    /// 与 `events` **一一对应**的文档出处（多层文档里靠它才删得对/改得对）
    pub origins: Vec<crate::doc::EventRef>,
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
    /// 第 i 个事件在文档里的出处（越界给 `None`：视图下标可能比文档旧一帧）
    pub fn origin(&self, i: usize) -> Option<crate::doc::EventRef> {
        self.origins.get(i).copied()
    }
    /// 事件在拍域的覆盖范围（时间轴画事件条）
    pub fn span_sec(&self, tmap: &TimeMap) -> (f64, f64) {
        match (self.events.first(), self.events.last()) {
            (Some(a), Some(b)) => (tmap.sec(a.start.to_f64()), tmap.sec(b.end.to_f64())),
            _ => (0.0, 0.0),
        }
    }
}

// ---------------------------------------------------------------- 音符位置（加载时算好）

/// 一条线上**位置还没算准**的音符下标集合。
///
/// 为什么是一串区间而不是一个：异步重算的优先级是"播放头 → 结尾"再"开头 → 播放头"，
/// 预算用完时中间就会留下空洞 —— 待算集合因此天然是 1~3 块。区间**升序**且互不相邻，
/// 于是"从 k 到末尾都要重算"这类操作只是截断/合并。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StaleSet {
    ranges: Vec<std::ops::Range<usize>>,
}

impl StaleSet {
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
    /// 还有几颗要算
    pub fn count(&self) -> usize {
        self.ranges.iter().map(|r| r.end - r.start).sum()
    }
    pub fn contains(&self, i: usize) -> bool {
        self.ranges.iter().any(|r| r.contains(&i))
    }
    pub fn clear(&mut self) {
        self.ranges.clear();
    }
    /// 待算的那些下标（升序；诊断与测试用）
    pub fn indices(&self) -> Vec<usize> {
        self.ranges.iter().flat_map(|r| r.clone()).collect()
    }
    /// 把 `[from, len)` 整段标成待重算（与已有区间合并 —— 流速再改一次时走的就是这里）
    pub fn mark_from(&mut self, from: usize, len: usize) {
        let from = from.min(len);
        if from >= len {
            return;
        }
        // 完全落在新区间里的旧区间丢掉；起点在 `from` 之前的那个保留（下面把它扩到末尾）
        self.ranges.retain(|r| r.start < from);
        match self.ranges.last_mut() {
            Some(last) if last.end >= from => last.end = len,
            _ => self.ranges.push(from..len),
        }
    }
    /// 把**单独一颗**标成待重算（与相邻区间合并）。
    ///
    /// 用途只有一个但很要紧：流速改动之前起头、改动之后才收尾的**长 hold** ——
    /// 它的头没变、尾巴变了，落在"后缀"之外。
    pub fn mark_one(&mut self, i: usize) {
        let pos = self.ranges.partition_point(|r| r.end <= i);
        // `pos` 之后的第一个区间可能正好紧邻（`r.start == i + 1`）⇒ 合并
        if let Some(r) = self.ranges.get_mut(pos) {
            if r.start <= i + 1 {
                r.start = r.start.min(i);
                return;
            }
        }
        // 前一个区间可能与它相邻（`r.end == i`）
        if let Some(r) = pos.checked_sub(1).and_then(|k| self.ranges.get_mut(k)) {
            if r.end == i {
                r.end = i + 1;
                // 合并之后可能与后一个接上
                if let Some(next) = self.ranges.get(pos) {
                    if next.start == i + 1 {
                        let end = next.end;
                        self.ranges[pos - 1].end = end;
                        self.ranges.remove(pos);
                    }
                }
                return;
            }
        }
        self.ranges.insert(pos, i..i + 1);
    }

    /// 从 `region` 里**摘出最靠左的至多 `budget` 条**（从待算集合里移除并返回它们的下标区间）
    pub fn take(&mut self, region: std::ops::Range<usize>, budget: usize) -> std::ops::Range<usize> {
        if budget == 0 || region.start >= region.end {
            return 0..0;
        }
        for k in 0..self.ranges.len() {
            let r = self.ranges[k].clone();
            let lo = r.start.max(region.start);
            let hi = r.end.min(region.end);
            if lo >= hi {
                continue;
            }
            let n = (hi - lo).min(budget);
            let mut rest: Vec<std::ops::Range<usize>> = Vec::new();
            if r.start < lo {
                rest.push(r.start..lo);
            }
            if lo + n < r.end {
                rest.push(lo + n..r.end);
            }
            self.ranges.remove(k);
            for (j, x) in rest.into_iter().enumerate() {
                self.ranges.insert(k + j, x);
            }
            return lo..lo + n;
        }
        0..0
    }
}

/// 一条线的**音符纵向位置**（加载时算好；流速事件改了就把它之后的重算）。
///
/// 位置的口径与渲染完全同源：
/// ```text
///   离判定线的距离 = (H(t_音符) − H(t_此刻)) × 音符自身 speed
///   H(t) = 120 ∫₀ᵗ v dτ            ← 从**谱面 0 秒**起积的绝对量（RPE y 单位）
/// ```
/// `H(t_音符)` 是音符自己的常量：与播放头、与判定线被搬到哪里都无关 ⇒ 在**加载时**一次算好。
/// 渲染每帧只要这条线的一个标量 `H(t_此刻)`（[`crate::perf::SpeedTable`] 查一次表）。
///
/// **流速事件一改**，它之后（时间上）的音符位置就全变了：那时把这段后缀标成待重算
/// （[`Self::mark_from`]，由 [`Line::set_tracks`] 做），GUI 每帧补一小段
/// （[`EditorState::pump_floors`]）。没补好之前渲染侧**现算**那一颗
/// （[`Line::floor_offset_now`]）—— 同一个公式、同一份实现，所以补得快慢都不影响画面，
/// 异步只影响"每帧要花多少代价"。
///
/// 检查点表与 `head`/`tail` **一起建**（表里的段下标指向的就是这条线现在那份
/// `tracks[4].events`），所以不存在"表与事件对不上"的中间态。
#[derive(Clone, Debug, Default)]
pub struct FlowCache {
    /// 流速积分的检查点（`H` 怎么查）
    table: crate::perf::SpeedTable,
    /// `H(note.time)`，与 `Line::notes` 一一对应
    head: Vec<f64>,
    /// `H(note.end)`（hold 的尾巴；非 hold 与 `head` 相同）
    tail: Vec<f64>,
    /// 还没算准的那些
    stale: StaleSet,
}

impl FlowCache {
    /// 音符位置离判定线多远（`H(t_音符) − H(此刻)`，RPE y 单位，带符号）；
    /// `None` = 还没算准 ⇒ 调用方现算（[`Line::floor_offset_now`]）
    pub fn offset(&self, i: usize, h_now: f64) -> Option<f64> {
        if self.stale.contains(i) {
            return None;
        }
        self.head.get(i).map(|h| h - h_now)
    }
    /// 同上，尾巴（hold 用）
    pub fn tail_offset(&self, i: usize, h_now: f64) -> Option<f64> {
        if self.stale.contains(i) {
            return None;
        }
        self.tail.get(i).map(|h| h - h_now)
    }
    pub fn is_stale(&self, i: usize) -> bool {
        i >= self.head.len() || self.stale.contains(i)
    }
    pub fn stale_count(&self) -> usize {
        self.stale.count()
    }
    pub fn len(&self) -> usize {
        self.head.len()
    }
    pub fn is_empty(&self) -> bool {
        self.head.is_empty()
    }
    /// 把 `from` 之后（含）的音符全标成待重算
    pub fn mark_from(&mut self, from: usize) {
        let n = self.head.len();
        self.stale.mark_from(from, n);
    }

    /// **流速在 `sec`（秒）处变了**：头或尾落在那之后的音符，位置都要重算。
    ///
    /// 判据为什么是"头或尾"而不是"头"：一条长 hold 的头可能在改动**之前**、尾巴却在**之后**
    /// —— 尾巴离判定线多远是 `H(t_尾) − H(此刻)`，同样吃这次改动。
    /// 时间升序的列表里这**不是**一段后缀（改动点之前那些长 hold 会零散地落进来），
    /// 所以后缀部分一次标完，前面那些长 hold 逐个标（数量通常是个位数）。
    /// 标记是 O(音符) 的线性扫；**真正要省的那笔钱是重算**，那部分照旧异步。
    pub fn mark_from_sec(&mut self, sec: f64, notes: &[Note]) {
        let from = notes.partition_point(|n| n.time < sec);
        self.stale.mark_from(from, notes.len());
        for (i, n) in notes.iter().take(from).enumerate() {
            if n.end >= sec {
                self.stale.mark_one(i);
            }
        }
    }
    /// 建/换流速检查点表（流速事件变了 ⇒ 表必须跟着换；**不动**已有的音符位置）
    fn rebuild_table(&mut self, events: &[Event], tmap: &TimeMap) {
        self.table = crate::perf::SpeedTable::build(events, tmap, tmap.end_beat);
    }
    /// 一颗音符的 **`(头 H, 尾 H, 新的提示下标)`** —— 算这个**只有这一处**。
    ///
    /// 头与尾各要一次 `h_at_hinted`（hold 的尾巴是另一个时刻，而且可能落在改动点之后，
    /// 见 [`FlowCache::mark_from_sec`]）；提示下标一路带下去，于是整条线重建是 O(音符)
    /// 而不是每颗各做一次二分。整表重建与异步补算必须给出一模一样的数 —— 所以共用它。
    fn note_floors(
        table: &crate::perf::SpeedTable,
        events: &[Event],
        tmap: &TimeMap,
        n: &Note,
        hint: usize,
    ) -> (f64, f64, usize) {
        let (h, hi) = table.h_at_hinted(events, tmap, n.time, hint);
        if n.kind == NoteKind::Hold {
            let (t, hi) = table.h_at_hinted(events, tmap, n.end, hi);
            (h, t, hi)
        } else {
            (h, h, hi)
        }
    }
    /// 整条线一次算好（**加载时**、以及音符表变了之后走这里）
    fn rebuild(&mut self, notes: &[Note], events: &[Event], tmap: &TimeMap) {
        self.head.clear();
        self.tail.clear();
        self.head.reserve(notes.len());
        self.tail.reserve(notes.len());
        let mut hint = 0usize;
        for n in notes {
            let (h, t, hi) = Self::note_floors(&self.table, events, tmap, n, hint);
            hint = hi;
            self.head.push(h);
            self.tail.push(t);
        }
        self.stale.clear();
    }
    /// 建表 + 全部算好（**唯一**"从零建立缓存"的入口）
    fn activate(&mut self, notes: &[Note], events: &[Event], tmap: &TimeMap) {
        self.rebuild_table(events, tmap);
        self.rebuild(notes, events, tmap);
    }
    /// 摘出本帧要算的那一段（`region` = 播放头之后 / 之前那一半）
    fn take_stale(&mut self, region: std::ops::Range<usize>, budget: usize) -> std::ops::Range<usize> {
        self.stale.take(region, budget)
    }
    /// 把 `span` 这一段算出来（升序），返回实际算了几条
    fn build_span(
        &mut self,
        notes: &[Note],
        events: &[Event],
        tmap: &TimeMap,
        span: std::ops::Range<usize>,
    ) -> usize {
        if span.start >= span.end || span.end > notes.len() || span.end > self.head.len() {
            return 0;
        }
        let mut hint = 0usize;
        for i in span.clone() {
            let (h, t, hi) = Self::note_floors(&self.table, events, tmap, &notes[i], hint);
            hint = hi;
            self.head[i] = h;
            self.tail[i] = t;
        }
        span.end - span.start
    }
}

/// 两条流速事件表**第一处不同**发生在哪一拍（`None` = 完全一样）。
///
/// 用途：流速事件一改，只有它**之后**（时间上）的音符位置会变 —— 这个拍号就是"之后"的边界。
/// 逐下标比对、取两者起点的较小值：那是最早可能被这次改动影响到的时刻
/// （`H` 是前缀积分，`t` 之前的值只由 `t` 之前的事件决定）。
pub fn first_speed_change(old: &[Event], new: &[Event]) -> Option<f64> {
    for i in 0..old.len().max(new.len()) {
        match (old.get(i), new.get(i)) {
            (Some(a), Some(b)) if a == b => continue,
            (Some(a), Some(b)) => return Some(a.start.to_f64().min(b.start.to_f64())),
            (Some(a), None) => return Some(a.start.to_f64()),
            (None, Some(b)) => return Some(b.start.to_f64()),
            (None, None) => break,
        }
    }
    None
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
    /// 这条线上**最长的音符时长**（秒，等于 `max(end − time)`；全是 tap 时是 0）。
    ///
    /// 只为"可见区间该从多早开始"服务：一条长 hold 的头可能远在窗口之前、身子还在窗口里，
    /// 而音符是按**时间**排序的 ⇒ 只按时间取连续区间会把它的身子漏掉（长条在头被击中后
    /// 立刻消失）。用"最长时长"当回退量是个**正确**的下界：任何 `end ≥ 窗口起点` 的音符
    /// 都必然满足 `time ≥ 窗口起点 − 最长时长`。
    pub max_note_sec: f64,
    /// 这条线流速的**最小非零量级**（`min |v|`，见 [`crate::perf::min_speed_magnitude`]；
    /// 没有流速事件时是 [`crate::perf::SPEED_DEFAULT`]）。
    ///
    /// 决定"往后看多久"：流速越慢，音符越早进入窗口 —— 固定的 2 秒前瞻在慢流速下会**漏画**
    /// 本该看得见的音符（实测：流速 1 时 3 秒外的音符在窗口里，却整颗没有实例）。
    /// 由 [`Line::set_tracks`] 随事件轨道一起刷新（**不是**只有整表重建才刷新）。
    pub min_speed_abs: f64,
    /// **音符纵向位置**（加载时算好；流速事件改了就把它之后的重算）—— 见 [`FlowCache`]
    pub floors: FlowCache,
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
        // 五条轨道的求值在 `perf::perf_of` 一处（那边解释过为什么不能各写一份）
        let ev: [&[Event]; 5] = [
            &self.tracks[0].events,
            &self.tracks[1].events,
            &self.tracks[2].events,
            &self.tracks[3].events,
            &self.tracks[4].events,
        ];
        crate::perf::perf_of(&ev, tmap.beat(sec))
    }
    /// 命中判定：屏幕坐标（RPE）是否落在这条线上（用于点选判定线）
    pub fn hit(&self, tmap: &TimeMap, sec: f64, point: [f32; 2], tol: f32, line_half_w: f32) -> bool {
        let p = self.perf(tmap, sec);
        // 逆变换回本地坐标（旋转可逆），再看 |y_local| 与 |x_local|
        let [lx, ly] = p.apply_inv(point);
        ly.abs() <= tol && lx.abs() <= line_half_w
    }

    // ---------------------------------------------------------------- 音符位置

    /// `H(sec)`（绝对位置，RPE y 单位）：查这条线的流速检查点表。
    /// **任何时刻都能问**（过去/现在/将来一视同仁，不是"只会往前走"的累加器）。
    pub fn h_at(&self, sec: f64, tmap: &TimeMap) -> f64 {
        self.floors.table.h_at(&self.tracks[4].events, tmap, sec)
    }

    /// 该音符此刻离判定线多远（**预算好的值**；`None` = 还没算准 ⇒ 调用方现算）
    pub fn floor_offset(&self, i: usize, h_now: f64) -> Option<f64> {
        self.floors.offset(i, h_now)
    }

    /// 同上，尾巴（hold 用；非 hold 与头相同）
    pub fn floor_tail_offset(&self, i: usize, h_now: f64) -> Option<f64> {
        self.floors.tail_offset(i, h_now)
    }

    /// **现算**同一件事（还没算准的音符的兜底）：与预算好的值是同一个公式、同一份实现，
    /// 所以两条路径可以互为基准对账（`tests/lines.rs::cached_and_on_the_fly_positions_agree`）。
    pub fn floor_offset_now(&self, sec: f64, h_now: f64, tmap: &TimeMap) -> f64 {
        self.h_at(sec, tmap) - h_now
    }

    /// 把 `from` 之后（含）的音符位置标成"待重算"
    pub fn mark_floors_stale_from(&mut self, from: usize) {
        self.floors.mark_from(from);
    }

    /// **加载路径**：建流速检查点表、把这条线**全部**音符的位置算好
    /// （"加载时算好所有音符实例的实际位置"就发生在这里）
    pub fn activate_floors(&mut self, tmap: &TimeMap) {
        self.floors.activate(&self.notes, &self.tracks[4].events, tmap);
    }

    /// 换上新的**音符表**（按线局部重建的唯一入口）：位置当场整条重算 ——
    /// 插入/删除会移动后面所有音符的下标，"只补一段"是补不对的。
    ///
    /// 顺带重算 `max_note_sec`：它是"可见区间该从多早开始"的回退量，早先只有整表重建才刷新
    /// ⇒ 编辑期**新加一条长 hold** 之后它仍是 0，那条长条的身子会在头被击中时整条消失
    /// （与用户报过的"长条一到线就消失"是同一个病，只是入口不同）。
    pub fn set_notes(&mut self, notes: Vec<Note>, tmap: &TimeMap) {
        self.notes = notes;
        self.max_note_sec = self
            .notes
            .iter()
            .map(|n| (n.end - n.time).max(0.0))
            .fold(0.0_f64, f64::max);
        self.floors.rebuild(&self.notes, &self.tracks[4].events, tmap);
    }

    /// 换上新的**事件轨道**（按线局部重建的唯一入口）。
    ///
    /// 三件事必须跟着走：
    /// ① 流速真的变了 ⇒ **把它之后的音符位置标成待重算**（异步补齐，见
    ///    [`EditorState::pump_floors`]）；没补好之前渲染侧现算，画面照样是准的；
    /// ② 换流速检查点表（`H` 的查询靠它）；
    /// ③ 重算 `min_speed_abs` —— 早先只有整表重建才刷新，于是"把流速改慢"之后构建窗口
    ///    没跟着变宽，本该看得见的音符整颗没有实例（与那个真 bug 同源）。
    pub fn set_tracks(&mut self, tracks: [TrackView; 5], tmap: &TimeMap) {
        let changed = first_speed_change(&self.tracks[4].events, &tracks[4].events);
        self.min_speed_abs =
            crate::perf::min_speed_magnitude(&tracks[4].events).unwrap_or(crate::perf::SPEED_DEFAULT);
        self.tracks = tracks;
        match changed {
            // 前缀积分：`beat` 之前的时刻只由它之前的事件决定 ⇒ 本线只有**它之后**的音符要重算
            // （"之后"按**头或尾**算：长 hold 的尾巴可能落在改动之后而头在之前，见
            // `FlowCache::mark_from_sec`）
            Some(beat) => {
                self.floors.rebuild_table(&self.tracks[4].events, tmap);
                if self.floors.len() == self.notes.len() {
                    self.floors.mark_from_sec(tmap.sec(beat), &self.notes);
                } else {
                    // 缓存还没建过（或与音符表长度对不上，比如这条线刚从 `line_shell` 造出来）
                    self.floors.rebuild(&self.notes, &self.tracks[4].events, tmap);
                }
            }
            // **流速没变**（改的是透明度/移动/旋转…）：位置照旧有效，连检查点表都不用重建 ——
            // 拖动透明度事件时每帧都会走到这里，白重建一次表就等于每帧白积一遍全谱的流速。
            // 只有"缓存还没建过 / 长度对不上"时才整条重算。
            None if self.floors.len() != self.notes.len() => {
                self.floors.activate(&self.notes, &self.tracks[4].events, tmap);
            }
            None => {}
        }
    }

    /// 本帧把这条线待重算的位置补一段：`after = true` 先补**播放头之后**那一半，
    /// `false` 再补**播放头之前**那一半（用户口径：从当前时间轴 → 结尾，再从头 → 当前时间轴）。
    /// 返回实际算了几条。
    pub fn pump_floors(&mut self, playhead: f64, after: bool, budget: usize, tmap: &TimeMap) -> usize {
        if budget == 0 || self.floors.stale_count() == 0 {
            return 0;
        }
        let n = self.notes.len();
        let p = self.notes.partition_point(|x| x.time < playhead);
        let region = if after { p..n } else { 0..p };
        let mut done = 0;
        while done < budget {
            let span = self.floors.take_stale(region.clone(), budget - done);
            if span.is_empty() {
                break;
            }
            let got = self.floors.build_span(&self.notes, &self.tracks[4].events, tmap, span);
            if got == 0 {
                break;
            }
            done += got;
        }
        done
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
/// 判定线**长度**的默认值（用户要求：3000）。
///
/// 它比窗口宽（窗口是 ±675 共 1350）—— 线两端会伸出游戏画面，这是刻意的：
/// 判定线常常被旋转/缩放，长一点更容易看清"它此刻在哪、朝哪边"。
/// 长度是**编辑器设置**而不是格式字段（`spec/opm-format.md` 里判定线没有这个字段），
/// GUI 的 `线半长` 与 CLI 的 `--line-len` 都能改。
pub const RPE_LINE_LEN_DEFAULT: f32 = 3000.0;

/// 判定线**半长**的默认值（= 长度的一半；渲染按半长画）。见 [`RPE_LINE_LEN_DEFAULT`]。
pub const RPE_LINE_HALF_W: f32 = RPE_LINE_LEN_DEFAULT * 0.5;

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
///
/// ⚠️ 轨道是空的 ⇒ **音符位置的缓存不在这里建**（`H` 要靠流速事件才算得出来）：
/// 轨道就位（`tracks_of` 之后）必须调 [`Line::activate_floors`]，
/// 否则这条线会退化成"没有流速事件"（默认 10）那条路径。
pub fn line_shell(doc: &Document, index: usize, tmap: &TimeMap) -> Option<Line> {
    let src = doc.judge_lines.get(index)?;
    let notes = notes_of(doc, index, tmap);
    let max_note_sec = notes
        .iter()
        .map(|n| (n.end - n.time).max(0.0))
        .fold(0.0_f64, f64::max);
    // 流速轨道上的最小量级（采样）；没有事件 ⇒ RPE 的默认 10
    let speed_events = crate::perf::track_events(src, "speed");
    let min_speed_abs =
        crate::perf::min_speed_magnitude(&speed_events).unwrap_or(crate::perf::SPEED_DEFAULT);
    Some(Line {
        index,
        name: src.name.clone(),
        z_order: src.z_order,
        is_cover: src.is_cover,
        bpm_factor: src.bpm_factor,
        notes,
        tracks: Default::default(),
        max_note_sec,
        min_speed_abs,
        floors: FlowCache::default(),
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
            speed: n.speed,
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
        let indexed = crate::perf::track_events_indexed(src, id.key());
        if indexed.is_empty() {
            continue;
        }
        let (origins, events): (Vec<_>, Vec<_>) = indexed.into_iter().unzip();
        // 流速轨只按线性求值 ⇒ 曲线也用线性采（否则面板显示的缓动与音符位置对不上）
        let curve = sample_track(&events, tmap, 4, *id == TrackId::Speed);
        let (mut min, mut max) = (f32::INFINITY, f32::NEG_INFINITY);
        for p in &curve {
            min = min.min(p[1]);
            max = max.max(p[1]);
        }
        out[k] = TrackView {
            events,
            origins,
            curve,
            min: if min.is_finite() { min } else { 0.0 },
            max: if max.is_finite() { max } else { 0.0 },
        };
    }
    out
}

/// 把文档整体转成视图（引导期 / BPM 或判定线集合变化时用）
///
/// **音符位置在这里一次算好**（`activate_floors`）：它是"加载时算好所有音符实例的实际位置"
/// 那条要求的落点 —— 之后每帧只查表，不再逐个音符积分。
pub fn chart_from_doc(doc: &Document) -> Chart {
    let tmap = TimeMap::from_doc(doc);
    let mut lines: Vec<Line> = (0..doc.judge_lines.len())
        .filter_map(|i| {
            let mut l = line_shell(doc, i, &tmap)?;
            l.tracks = tracks_of(doc, i, &tmap);
            l.activate_floors(&tmap);
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

/// 选区里装的是哪一类（框选时由**拖动起始点**落在哪个半区决定，用户定的规则）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelKind {
    Notes,
    Events,
}

/// **选中的事件**（视图坐标）：轨道 + 该轨道**合并视图**里的下标。
///
/// 合并下标只用来在视图里定位；要发命令时回文档地址（`doc::EventRef`）由
/// [`TrackView::origin`] 换算 —— 两者不是一回事，见 `doc::EventRef`。
pub type EventSel = (TrackId, usize);

/// **选区**（视图状态：不进文档、不进撤销栈、不影响保存）。
///
/// 两条纪律，都是用户定的：
/// 1. **同时只有一类**（音符 xor 事件）。框选按**起始点**落在哪个半区来定选哪一类；
///    Ctrl+左键点到另一半区时把旧的清掉。于是 Del 不需要猜"该删哪个"。
/// 2. `EditorState::selected_note()` / `selected_event()` 是这里的**锚**（最后碰过的那一个）：
///    检查器、判定线树、时间轴都只认锚 —— 多选因此没有把它们连锁改成 `Vec`。
///
/// 锚与集合的一致性由本类型独占维护：外面只能通过 [`EditorState`] 上的方法改选区，
/// 不存在"改了集合忘了改锚"这条路。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    notes: std::collections::BTreeSet<usize>,
    events: std::collections::BTreeSet<EventSel>,
    anchor_note: Option<usize>,
    anchor_event: Option<EventSel>,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty() && self.events.is_empty()
    }
    /// 选区大小（音符或事件，两者不会同时非空）
    pub fn len(&self) -> usize {
        self.notes.len().max(self.events.len())
    }
    pub fn has_note(&self, i: usize) -> bool {
        self.notes.contains(&i)
    }
    pub fn has_event(&self, track: TrackId, i: usize) -> bool {
        self.events.contains(&(track, i))
    }
    pub fn has_any_event(&self) -> bool {
        !self.events.is_empty()
    }
    /// 选中的音符下标（升序）
    pub fn notes(&self) -> impl Iterator<Item = usize> + '_ {
        self.notes.iter().copied()
    }
    /// 选中的事件（按轨道、再按下标）
    pub fn events(&self) -> impl Iterator<Item = EventSel> + '_ {
        self.events.iter().copied()
    }
    pub fn notes_set(&self) -> &std::collections::BTreeSet<usize> {
        &self.notes
    }
    /// 锚（最后碰过的那一个音符）
    pub fn anchor_note(&self) -> Option<usize> {
        self.anchor_note
    }
    /// 锚（最后碰过的那一条事件）
    pub fn anchor_event(&self) -> Option<EventSel> {
        self.anchor_event
    }

    /// 用一批音符**替换**整个选区（框选音符；空集 = 清空）。锚取最小的那个。
    ///
    /// **不碰事件那一类的锚**：选区同时只有一类，但"检查器显示哪一条事件"是另一件事 ——
    /// 点了音符就把事件编辑器收起来，是用户没要求的退化（老的界面两边同时显示）。
    pub fn set_notes(&mut self, notes: impl IntoIterator<Item = usize>) {
        self.events.clear();
        self.notes = notes.into_iter().collect();
        if self.anchor_note.is_none_or(|a| !self.notes.contains(&a)) {
            self.anchor_note = self.notes.iter().next().copied();
        }
    }
    /// 用一批事件**替换**整个选区（框选事件；空集 = 清空）。锚取排序最小的那个。
    pub fn set_events(&mut self, events: impl IntoIterator<Item = EventSel>) {
        self.notes.clear();
        self.events = events.into_iter().collect();
        if self.anchor_event.is_none_or(|a| !self.events.contains(&a)) {
            self.anchor_event = self.events.iter().next().copied();
        }
    }
    /// 单选一个音符（点选：清空后只剩它）
    pub fn select_note(&mut self, i: usize) {
        self.set_notes([i]);
    }
    /// 单选一条事件
    pub fn select_event(&mut self, track: TrackId, i: usize) {
        self.set_events([(track, i)]);
    }
    /// Ctrl+左键：在"选中 / 未选中"之间**切换**。
    /// 点到另一半区时把旧的清掉（选区同时只有一类）—— 与框选同一条规则。
    pub fn toggle_note(&mut self, i: usize) {
        self.events.clear();
        if !self.notes.remove(&i) {
            self.notes.insert(i);
            self.anchor_note = Some(i);
        } else if self.anchor_note == Some(i) {
            self.anchor_note = self.notes.iter().next().copied();
        }
    }
    pub fn toggle_event(&mut self, track: TrackId, i: usize) {
        self.notes.clear();
        let key = (track, i);
        if !self.events.remove(&key) {
            self.events.insert(key);
            self.anchor_event = Some(key);
        } else if self.anchor_event == Some(key) {
            self.anchor_event = self.events.iter().next().copied();
        }
    }
    /// 清空选区（点空白处、Esc、换线、加载新谱面都走这里）
    pub fn clear(&mut self) {
        self.notes.clear();
        self.events.clear();
        self.anchor_note = None;
        self.anchor_event = None;
    }
    /// 收敛到"文档里还在"的那些（删掉/换线之后视图下标会过期）。
    ///
    /// 判据是 [`Self::keep`] 的**存在性**（`keep(i)` = 下标 i 还有意义），不是"在不在选区里"——
    /// 锚可以不在选区里（它是"每一类最后碰过的那一个"，供检查器显示）。
    /// 锚真的没了才退到集合里的第一个：检查器永远显示一个**存在**的东西。
    pub fn retain(
        &mut self,
        mut keep_note: impl FnMut(usize) -> bool,
        mut keep_event: impl FnMut(EventSel) -> bool,
    ) {
        self.notes.retain(|i| keep_note(*i));
        self.events.retain(|k| keep_event(*k));
        if self.anchor_note.is_some_and(|a| !keep_note(a)) {
            self.anchor_note = self.notes.iter().next().copied();
        }
        if self.anchor_event.is_some_and(|a| !keep_event(a)) {
            self.anchor_event = self.events.iter().next().copied();
        }
    }
    /// 只清掉事件那一半（换轨道时用：轨道变了，旧下标就没意义了）
    pub fn clear_events(&mut self) {
        self.events.clear();
        self.anchor_event = None;
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
    /// **多选集合**（含锚）。读写一律走本结构的方法：集合与锚必须一起变。
    sel: Selection,
    /// 按 R 之后正在跟随鼠标的 hold（`None` = 没有待放置的长条）。
    /// **视图状态**：Esc 取消、鼠标改长度都在这一层，不进文档。
    pub pending_hold: Option<PendingHold>,
    /// 在事件区按键之后正在跟随鼠标的事件块草稿（与 hold **互斥**：同时只放一个东西）
    pub pending_event: Option<PendingEvent>,
    /// 演奏区**实例构建窗口**（秒）：以播放头为基准往后看 `lookahead` 秒的**音符**才会被送进
    /// 渲染管线。
    ///
    /// 注意它**不等于"看得见的时间"**：音符落在哪由 RPE 的 floor position 决定
    /// （`perf::speed_travel`）—— 流速 10（RPE 默认 = 1×）时音符 0.375 秒就走完半个窗口，
    /// 所以真正看得见的那一段比这个窗口短得多。这个值是**超集**：多出来的实例由渲染侧
    /// "整条都在窗口上方就跳过"那一道判据挡掉。
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
    /// **文档内容的末端（拍）**：`max(note 末端, 事件末端)`，由**文档**算出来（`doc.chart_end()`）。
    ///
    /// 为什么不直接用 `chart.tmap.end_beat`：那是**视图模型**里的副本，而视图模型只在 `structure`
    /// 话题变化时才整表重建（加/删音符走的是"逐线局部重建"那条路）⇒ 它会**过期**。
    /// 用户报的正是这个："放了音符的谱面上，时间轴还是短的" —— 音符在 68.9s，时间轴却停在 20.0s。
    pub content_end_beat: f64,
    /// **乐曲时长（秒）**：时间轴的**总长**按它来（`None` = 没有音乐/还没解码出来）。
    ///
    /// 为什么要单独存一个：`chart.duration` 是**谱面自身**的跨度（末尾还留 2 秒尾巴），
    /// 一份刚建的谱面几乎是 0 ⇒ 时间轴只有 2 秒（用户报的"默认总长度只有2秒"）。
    /// 音乐一装上，时间轴就该和歌一样长：你才滚得到副歌去写谱。
    music_len: Option<f64>,
    /// 这一批音符位置重算**已经算了多少条**（底栏进度用；`pump_floors` 累加、
    /// 待算清零时归零）
    floor_done: usize,
}

impl EditorState {
    pub fn new(chart: Chart) -> Self {
        // 先把长度取出来：`chart` 会被移动进结构体（借用顺序而已，不是逻辑问题）
        let content_end_beat = chart.tmap.end_beat;
        Self {
            chart,
            playhead: 0.0,
            playing: false,
            speed: 1.0,
            selected_line: 0,
            selected_track: TrackId::Alpha,
            sel: Selection::default(),
            pending_hold: None,
            pending_event: None,
            lookahead: 2.0,
            show_boundary: true,
            line_half_w: RPE_LINE_HALF_W, // = 3000 的一半
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
            content_end_beat,
            music_len: None,
            floor_done: 0,
        }
    }

    /// 整表重建之后同步内容末端（视图模型与它必须一起更新，见 [`Self::content_end_beat`]）。
    pub fn set_chart(&mut self, chart: Chart) {
        self.content_end_beat = chart.tmap.end_beat;
        self.chart = chart;
    }

    /// 内容变了（加/删/移动 note 或事件）之后由调用方刷新 —— 只更新**长度**，不重建整个视图模型。
    pub fn set_content_end_beat(&mut self, beat: f64) {
        if beat.is_finite() && beat >= 0.0 {
            self.content_end_beat = beat;
        }
    }

    /// 设定**乐曲时长**（秒）。`None` = 没有音乐（或还没解码出来）；非正数/非有限值一律当"没有"。
    ///
    /// 调用点：音频解析完成时、运行中替换音频时（GUI 在拿到 `Audio` 之后调一次）。
    pub fn set_music_len(&mut self, sec: Option<f64>) {
        self.music_len = sec.filter(|s| s.is_finite() && *s > 0.0);
    }

    /// 乐曲时长（秒）：`None` = 没有音乐/还没解码出来。读数会把它显示出来，
    /// 于是"时间轴为什么这么短"一眼能看出是不是**音乐没装上**（那正是最常见的原因）。
    pub fn music_len(&self) -> Option<f64> {
        self.music_len
    }

    // ---------------------------------------------------------------- 选区
    //
    // 读写**只走这几个方法**：集合与锚必须一起变，所以字段是私有的。
    // `selected_note` / `selected_event` 这两个名字仍然是"锚"的口径 —— 老调用点
    // （检查器、判定线树、时间轴、渲染）都只关心"当前是哪一条"，语义没变。

    /// 选区（只读）
    pub fn selection(&self) -> &Selection {
        &self.sel
    }

    /// 锚音符（检查器/树/时间轴高亮的那一个）
    pub fn selected_note(&self) -> Option<usize> {
        self.sel.anchor_note()
    }

    /// 锚事件（**当前轨道**里的下标）
    pub fn selected_event(&self) -> Option<usize> {
        self.sel.anchor_event().map(|(_, i)| i)
    }

    /// 锚事件的**完整坐标**（轨道 + 下标）——组拖动要按它认"手指按住的是谁"
    pub fn selected_event_ref(&self) -> Option<EventSel> {
        self.sel.anchor_event()
    }

    /// 某个音符是否在选区里
    pub fn is_note_selected(&self, i: usize) -> bool {
        self.sel.has_note(i)
    }
    /// 某条事件是否在选区里
    pub fn is_event_selected(&self, track: TrackId, i: usize) -> bool {
        self.sel.has_event(track, i)
    }
    /// 把选区替换成"只有这一个音符"（点选）
    pub fn select_note(&mut self, i: usize) {
        self.sel.select_note(i);
    }
    /// 把选区替换成"这一批音符"（框选）；空集即清空
    pub fn select_notes(&mut self, notes: impl IntoIterator<Item = usize>) {
        self.sel.set_notes(notes);
    }
    /// 把选区替换成"只有这一条事件"（点选）
    pub fn select_event(&mut self, track: TrackId, i: usize) {
        self.sel.select_event(track, i);
    }
    /// 把选区替换成"这一批事件"（框选）；空集即清空
    pub fn select_events(&mut self, events: impl IntoIterator<Item = EventSel>) {
        self.sel.set_events(events);
    }
    /// Ctrl+左键：切换单个音符/单条事件的选中状态
    pub fn toggle_note_selection(&mut self, i: usize) {
        self.sel.toggle_note(i);
    }
    pub fn toggle_event_selection(&mut self, track: TrackId, i: usize) {
        self.sel.toggle_event(track, i);
    }
    /// 清空选区
    pub fn clear_selection(&mut self) {
        self.sel.clear();
    }
    /// **按文档下标**选中判定线；返回是否找到。
    ///
    /// 文档下标 ≠ 视图下标（视图里可能有"线壳"，见 `EditorState::line_shell`）——
    /// 这个换算只该有一处：控制通道选线、冲突浏览器跳转、`OPM_EDIT_AUTO` 走的都是它。
    pub fn select_line_doc(&mut self, index: usize) -> bool {
        match self.chart.lines.iter().position(|l| l.index == index) {
            Some(view) => {
                self.selected_line = view;
                true
            }
            None => false,
        }
    }
    /// 换轨道：事件那一半的旧下标就没意义了
    pub fn clear_event_selection(&mut self) {
        self.sel.clear_events();
    }
    /// 视图下标过期时收敛（删了音符/事件、换了线）
    pub fn retain_selection(
        &mut self,
        keep_note: impl FnMut(usize) -> bool,
        keep_event: impl FnMut(EventSel) -> bool,
    ) {
        self.sel.retain(keep_note, keep_event);
    }
    /// 现在选中的是音符还是事件（框选起始点半区的口径也是它）
    pub fn selection_kind(&self) -> Option<SelKind> {
        if self.sel.has_any_event() {
            Some(SelKind::Events)
        } else if !self.sel.notes_set().is_empty() {
            Some(SelKind::Notes)
        } else {
            None
        }
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
        // **内容是缓存值**（`content_end_beat`，随每次内容变化刷新），不是 `tmap.end_beat` ——
        // 后者那份视图模型在"只加了个音符"时不会重建，会一直报旧的长度（用户报的 bug）
        let music = self.music_len.map(|sec| tmap.beat(sec)).unwrap_or(0.0);
        self.content_end_beat.max(music) + Self::TIMELINE_TAIL_BEATS
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

    /// 某条线在可见窗口内的音符下标区间（半开）。
    ///
    /// 上界按"还没到"取（`time < 播放头 + lookahead`）；下界**必须按最长音符回退**：
    /// 一条长 hold 的头在窗口之前、身子还在窗口里，而音符是按**时间**排序的 ——
    /// 只看时间会让长条在头被击中后立刻消失（自动播放时最明显）。
    /// 回退量 = `max_note_sec`（正确的下界，见 `Line::max_note_sec`）+ 一点余量
    /// （刚过去的音符还要画"到线收缩"与击中效果）。
    pub fn visible_range_of(&self, line: usize) -> std::ops::Range<usize> {
        let Some(l) = self.chart.lines.get(line) else {
            return 0..0;
        };
        let back = Self::PAST_NOTE_MARGIN_SEC + l.max_note_sec;
        let lo = l.notes.partition_point(|n| n.time < self.playhead - back);
        // 往前看多久由**流速**决定，不是固定 2 秒：音符从窗口边缘（±450 + 一个音符的余量）
        // 走到判定线要 `510 / (120·|v|)` 秒 —— 流速 10（默认）是 0.42 秒，流速 1 是 4.25 秒。
        // `lookahead` 是**下限**（至少往后算这么多秒，保证它仍然是个有意义的旋钮），
        // 上限是 `MAX_BUILD_LOOKAHEAD`（流速趋近 0 时别把整份谱面都塞进实例列表）。
        // 流速过零（或极慢）时 `speed_span` 会发散 —— 那正是"音符贴着判定线长时间逗留"的情形，
        // 夹到上限即可（`clamp` 的下界也先夹一次，免得 floor > MAX 时 panic）
        let v = l.min_speed_abs.max(1e-6);
        let speed_span = (RPE_WINDOW_HALF_H + Self::NOTE_SPAN_MARGIN) as f64
            / (crate::perf::SPEED_UNITS_PER_SEC * v);
        let floor = self.lookahead.min(Self::MAX_BUILD_LOOKAHEAD).max(0.05);
        let ahead = speed_span.clamp(floor, Self::MAX_BUILD_LOOKAHEAD);
        let hi = l.notes.partition_point(|n| n.time < self.playhead + ahead);
        lo..hi
    }

    /// 判断"音符还在窗口里"时给端点留的余量（RPE y 单位）：音符自身有半高，判定线还可能被
    /// 事件挪动，留一点免得贴边的音符被裁掉。
    pub const NOTE_SPAN_MARGIN: f32 = 60.0;

    // ---------------------------------------------------------------- 音符位置重算（异步）

    /// 每帧给音符位置重算多少条。**实测**（`cargo test --release --test floor_bench -- --ignored
    /// --nocapture`）：4096 条约 **0.2 ~ 0.8 ms**，是一帧预算的 1% 量级；10 万音符的谱面因此
    /// 大约 **25 帧（0.4 秒）**补完，而**画面从第一帧就是准的**（没补好的那几颗由渲染侧现算，
    /// 见 [`crate::render::build_instances`]；实测查表 **4 ns/颗** vs 现算 **160 ns/颗**）。
    pub const FLOOR_NOTES_PER_FRAME: usize = 4096;

    /// 还有多少颗音符的位置**没算准**（0 = 没有异步的活）
    pub fn floor_pending(&self) -> usize {
        self.chart.lines.iter().map(|l| l.floors.stale_count()).sum()
    }

    /// 已经算准的音符总数（诊断面板用：与 [`Self::floor_pending`] 一起看就是"缓存完不完整"）
    pub fn floor_cached(&self) -> usize {
        self.chart
            .lines
            .iter()
            .map(|l| l.notes.len().saturating_sub(l.floors.stale_count()))
            .sum()
    }

    /// 底栏提示用：`(这一批已算好, 这一批总数)`；`None` = 没有在跑的活。
    ///
    /// "本批"= 从"上一次全部算准"到"下一次全部算准"之间累计的条数（含中途又改流速新增的）：
    /// 正在拖动流速事件时它会一直涨，那正是它该有的样子（那批活确实一直在变大）。
    pub fn floor_rebuild(&self) -> Option<(usize, usize)> {
        let pending = self.floor_pending();
        if pending == 0 {
            return None;
        }
        Some((self.floor_done, self.floor_done + pending))
    }

    /// **异步重算一步**：本帧最多算 `budget` 条，返回实际算了几条。
    ///
    /// 在 GUI 的帧里做（**不开线程**：这份缓存只被渲染与它自己用，没必要为它引入并发）。
    /// 顺序是用户口径的两段：**从当前时间轴 → 结尾**，再**从头 → 当前时间轴** ——
    /// 屏幕上马上要用的先算，屏幕外（将来/过去）的后算。
    ///
    /// 期间如果又改了流速事件（`Line::set_tracks` 会把新的后缀标脏），
    /// 待算集合自然就并进来了：下帧从新的进度继续，不需要任何"取消/重来"的仪式。
    pub fn pump_floors(&mut self, budget: usize) -> usize {
        if budget == 0 {
            return 0;
        }
        let playhead = self.playhead;
        // 借用拆分：`tmap` 只读、`lines` 可变，两者是 `chart` 的不同字段
        let tmap = &self.chart.tmap;
        let lines = &mut self.chart.lines;
        let mut left = budget;
        for after in [true, false] {
            for l in lines.iter_mut() {
                if left == 0 {
                    break;
                }
                left -= l.pump_floors(playhead, after, left, tmap);
            }
            if left == 0 {
                break;
            }
        }
        let done = budget - left;
        self.floor_done += done;
        if self.floor_pending() == 0 {
            self.floor_done = 0;
        }
        done
    }

    /// 实例构建窗口的**上限**（秒）。流速趋近 0 时"穿过窗口要多久"会发散 ——
    /// 封顶之后极端谱面（流速 0.01）里 30 秒之外的音符不会建实例（它们也都贴在判定线附近）。
    pub const MAX_BUILD_LOOKAHEAD: f64 = 30.0;

    /// 播放头**之前**还要送进渲染管线的余量（秒）。
    ///
    /// 刚到达判定线的音符还要画"到线收缩 + 击中效果"，所以不能一到播放头就不见了。
    /// 取值要点：≥ 击中效果的时长（`render::HIT_FX_SEC`），否则效果会被截尾。
    pub const PAST_NOTE_MARGIN_SEC: f64 = 0.25;
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

    // ------------------------------------------------ 音符位置缓存（加载时算好 + 异步重算）

    /// 一份"一条线 + 若干音符 + 给定流速事件"的谱面（BPM 120 ⇒ 一拍 0.5 秒）
    fn speed_doc(events: Vec<Event>, notes: &[(&str, f64, Option<f64>)]) -> Document {
        use crate::doc::{Beat, BpmEntry, JudgeLine, Note as DocNote, NoteKind as DocKind};
        let mut doc = Document::default();
        doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 120.0, foreign: Default::default() }];
        doc.judge_lines.clear();
        let mut l = JudgeLine::default();
        *l.layers[0].track_mut("speed").unwrap() = events;
        for (kind, start, end) in notes {
            let k = match *kind {
                "hold" => DocKind::Hold,
                "drag" => DocKind::Drag,
                "flick" => DocKind::Flick,
                _ => DocKind::Tap,
            };
            let mut n = DocNote::new(k, Beat::new((start * 4.0).round() as i64, 4), 0.0);
            n.end = end.map(|e| Beat::new((e * 4.0).round() as i64, 4));
            l.notes.push(n);
        }
        doc.judge_lines.push(l);
        doc
    }

    fn ev(a: f64, b: f64, from: f64, to: f64) -> Event {
        Event::new(
            crate::doc::Beat::new((a * 4.0).round() as i64, 4),
            crate::doc::Beat::new((b * 4.0).round() as i64, 4),
            serde_json::json!(from),
            serde_json::json!(to),
            "linear",
        )
    }

    /// **加载时算好的位置 = 直接积分**（独立基准：`perf::speed_travel`，不经过检查点表）。
    ///
    /// 这条是"缓存"能不能被信的前提 —— 缓存错了，预览就会在没有流速改动时也是歪的。
    #[test]
    fn load_time_positions_match_direct_integration() {
        // 三段：0→10 升到 20、保持、再降回 -5（含负流速），音符铺在整条时间轴上
        let events = vec![ev(0.0, 4.0, 10.0, 20.0), ev(4.0, 8.0, 20.0, 20.0), ev(8.0, 12.0, 20.0, -5.0)];
        let notes: Vec<(&str, f64, Option<f64>)> = (0..12)
            .map(|k| ("tap", 0.5 * k as f64, None))
            .chain(std::iter::once(("hold", 1.0, Some(9.0))))
            .collect();
        let doc = speed_doc(events.clone(), &notes);
        let chart = chart_from_doc(&doc);
        let tmap = chart.tmap.clone();
        let line = &chart.lines[0];
        assert_eq!(line.floors.stale_count(), 0, "加载之后不该还有没算准的");
        assert_eq!(line.floors.len(), line.notes.len());
        for (i, n) in line.notes.iter().enumerate() {
            let h = line.h_at(n.time, &tmap);
            let want = crate::perf::speed_travel(&events, &tmap, 0.0, n.time);
            assert!((h - want).abs() < 1e-6, "第 {i} 颗：缓存 {h} ≠ 直积 {want}");
            // 尾巴也一样（hold 的尾巴过去也能问 —— 那个累加器版本在这里会返回 0）
            if n.kind == NoteKind::Hold {
                let t = line.h_at(n.end, &tmap);
                let want = crate::perf::speed_travel(&events, &tmap, 0.0, n.end);
                assert!((t - want).abs() < 1e-6, "尾巴：缓存 {t} ≠ 直积 {want}");
            }
        }
    }

    /// **事件块前后的空位：线的表演值取相邻那块的值**，不是全局默认值（用户口径）。
    ///
    /// 早先 `track_value("speed", …)` 在"首条事件之前"返回 `None`，`Line::perf` 于是留着
    /// `LinePerf::default()` 里的 **10**（流速）/ **1**（透明度）—— 一条从第 4 秒才开始的不透明
    /// 事件会让判定线在 0~4 秒**完全可见**（该是事件自己的起始值才对）。
    #[test]
    fn a_leading_gap_uses_the_first_blocks_value_not_a_global_default() {
        // 事件块 [8,16] 拍（4~8 秒）：alpha 0.25、流速 3
        let doc = speed_doc(
            vec![ev(8.0, 16.0, 3.0, 3.0)],
            &[("tap", 20.0, None)],
        );
        let chart = chart_from_doc(&doc);
        let tmap = chart.tmap.clone();
        let line = &chart.lines[0];
        // 块之前（1 秒）：取块的起始值
        let p = line.perf(&tmap, 1.0);
        assert!((p.speed - 3.0).abs() < 1e-6, "块前流速该是 3（块的起始值），实际 {}", p.speed);
        // 块之后（20 秒）：保持终值
        let p = line.perf(&tmap, 20.0);
        assert!((p.speed - 3.0).abs() < 1e-6, "块后流速该保持 3，实际 {}", p.speed);
        // 空轨道仍然是**全局默认**（那是默认值唯一该出现的地方：流速 10 / 透明度 1）
        let doc = speed_doc(vec![], &[("tap", 20.0, None)]);
        let chart = chart_from_doc(&doc);
        let tmap = chart.tmap.clone();
        let p = chart.lines[0].perf(&tmap, 1.0);
        assert!((p.speed - 10.0).abs() < 1e-6, "空流速轨道按 RPE 默认 10 走");
        assert!((p.alpha - 1.0).abs() < 1e-6, "空透明度轨道按 1（不透明）走");
    }

    /// **流速事件一改：只有它之后的音符被标脏**（前缀积分的直接推论），其余仍是"已算准"。
    /// 唯一的例外是**跨过改动点的长 hold** —— 它的头没变、尾巴变了（见下）。
    #[test]
    fn a_speed_edit_only_dirties_the_notes_after_it() {
        // 音符在 1..10 拍（一拍一个），另外加一条 2 拍起、9 拍止的长 hold（跨过第 5 拍）
        let mut notes: Vec<(&str, f64, Option<f64>)> =
            (0..10).map(|k| ("tap", 1.0 + k as f64, None)).collect();
        notes.push(("hold", 2.0, Some(9.0)));
        let doc = speed_doc(vec![ev(0.0, 16.0, 10.0, 10.0)], &notes);
        let chart = chart_from_doc(&doc);
        let mut line = chart.lines[0].clone();
        let tmap = chart.tmap.clone();
        // 在第 5 拍（2.5 秒）处插一条改流速的事件
        let mut events = line.tracks[4].events.clone();
        events.push(ev(5.0, 16.0, 10.0, 30.0));
        events.sort_by(|a, b| a.start.cmp(&b.start));
        let mut tracks = line.tracks.clone();
        tracks[4].events = events;
        line.set_tracks(tracks, &tmap);
        // 改动点：第 5 拍 = 2.5 秒。音符按时间升序，**头在 2.5 秒之前、尾在之后的**只有那条长 hold
        // （2~9 拍 = 1.0~4.5 秒 ⇒ 下标 2）；其后（下标 5..11）整段都要重算。
        let crossing: Vec<usize> = (0..line.notes.len())
            .filter(|i| line.notes[*i].time < 2.5 && line.notes[*i].end >= 2.5)
            .collect();
        assert_eq!(crossing, vec![2], "这条用例要有一条跨过改动点的长 hold");
        let stale: Vec<usize> = (0..line.notes.len()).filter(|i| line.floors.is_stale(*i)).collect();
        let want: Vec<usize> = crossing.iter().copied().chain(5..11).collect();
        assert_eq!(stale, want, "跨点的长 hold + 改动点之后的那些");
        // 其余（改动点之前的短音符）位置**一个字都没变**，而且仍然算准
        let h_now = line.h_at(0.0, &tmap);
        for i in 0..11 {
            if want.contains(&i) {
                continue;
            }
            let got = line.floor_offset(i, h_now).expect("改动点之前的音符仍然算准");
            let direct = line.floor_offset_now(line.notes[i].time, h_now, &tmap);
            assert!((got - direct).abs() < 1e-9, "第 {i} 颗：{got} ≠ {direct}");
        }
    }

    /// **异步重算：预算 + 优先级**（用户口径：先"当前时间轴 → 结尾"，再"开头 → 当前时间轴"）
    #[test]
    fn the_rebuild_goes_from_the_playhead_to_the_end_first() {
        let doc = speed_doc(
            vec![ev(0.0, 16.0, 10.0, 10.0)],
            &(0..20).map(|k| ("tap", 1.0 + k as f64, None)).collect::<Vec<_>>(),
        );
        let mut st = EditorState::new(chart_from_doc(&doc));
        // 整条标脏（等于"流速事件改了整条"）
        st.chart.lines[0].mark_floors_stale_from(0);
        let total = st.floor_pending();
        assert_eq!(total, 20);
        assert_eq!(st.floor_rebuild(), Some((0, 20)), "刚开始：0/20");
        // 播放头落在 3.5 秒（= 第 7 拍）：音符在 1..20 拍（0.5 … 10.0 秒）
        st.seek(3.5);
        let before: Vec<usize> = (0..20).filter(|i| st.chart.lines[0].floors.is_stale(*i)).collect();
        assert_eq!(before.len(), 20);
        // 一帧只算 3 条
        assert_eq!(st.pump_floors(3), 3);
        let left: Vec<usize> = (0..20).filter(|i| st.chart.lines[0].floors.is_stale(*i)).collect();
        // 先算的必须是**播放头之后**那三颗：时间 < 3.5s 的有 6 颗（下标 0..6）⇒ 算掉 6、7、8
        assert_eq!(left, (0..6).chain(9..20).collect::<Vec<_>>(), "实际剩下 {left:?}");
        assert_eq!(st.floor_rebuild(), Some((3, 20)), "进度：3/20");
        // 补完为止：每帧 ≤ 预算，总数正好是剩下的
        let mut frames = 0;
        while st.floor_pending() > 0 {
            assert!(st.pump_floors(3) <= 3, "一帧不许超过预算");
            frames += 1;
            assert!(frames < 20, "补不完说明预算没被用上");
        }
        assert_eq!(frames, 6, "还剩 17 条、每帧 3 条 ⇒ 6 帧");
        assert_eq!(st.floor_rebuild(), None, "补完之后底栏那行字要消失");
        assert_eq!(st.floor_pending(), 0);
    }

    /// 中途又改一次流速（拖动事件就是这个节奏）：**新的脏区间并进来**，从当前的进度继续
    #[test]
    fn a_second_speed_edit_merges_into_the_running_rebuild() {
        let doc = speed_doc(
            vec![ev(0.0, 16.0, 10.0, 10.0)],
            &(0..12).map(|k| ("tap", 1.0 + k as f64, None)).collect::<Vec<_>>(),
        );
        let mut st = EditorState::new(chart_from_doc(&doc));
        st.chart.lines[0].mark_floors_stale_from(0);
        st.seek(0.0);
        st.pump_floors(4); // 先补 4 条（播放头在 0 ⇒ 从开头往后）
        assert_eq!(st.floor_pending(), 8);
        // 第二笔：从第 4 颗起再改（下标 3）⇒ 并成一个待算集合，总数不变（本来就都待算）
        st.chart.lines[0].mark_floors_stale_from(3);
        assert_eq!(st.floor_pending(), 12 - 3.max(0) - 0, "实际 {}", st.floor_pending());
        assert_eq!(st.floor_rebuild(), Some((4, 4 + st.floor_pending())));
        while st.floor_pending() > 0 {
            st.pump_floors(64);
        }
        assert_eq!(st.floor_rebuild(), None);
        assert_eq!(st.floor_cached(), 12);
    }

    /// `set_notes` / `set_tracks` 顺手要修的两件事（都是"只有整表重建才刷新"留下的病）：
    /// ① 新加一条长 hold ⇒ `max_note_sec` 跟着涨（否则长条一到线就消失）；
    /// ② 把流速改慢 ⇒ `min_speed_abs` 跟着降（否则构建窗口没变宽、音符整颗没有实例）。
    #[test]
    fn note_and_track_rebuilds_refresh_the_derived_numbers() {
        let doc = speed_doc(vec![ev(0.0, 16.0, 10.0, 10.0)], &[("tap", 1.0, None)]);
        let chart = chart_from_doc(&doc);
        let tmap = chart.tmap.clone();
        let mut line = chart.lines[0].clone();
        assert_eq!(line.min_speed_abs, 10.0);
        assert_eq!(line.max_note_sec, 0.0);
        // ① 加一条 8 拍的长 hold（0.5 秒/拍 ⇒ 4 秒；起点与 0 号音符相同 ⇒ 列表仍按时间升序）
        //    再加一颗 3.0 秒（第 6 拍）的音符 —— 它只在窗口放宽之后才该进构建区间
        let mut notes = line.notes.clone();
        let mut hold = notes[0];
        hold.kind = NoteKind::Hold;
        hold.end = hold.time + 4.0;
        let mut far = notes[0];
        far.time = 3.0;
        far.end = 3.0;
        notes.push(hold);
        notes.push(far);
        line.set_notes(notes, &tmap);
        assert!((line.max_note_sec - 4.0).abs() < 1e-9, "实际 {}", line.max_note_sec);

        let mut st = EditorState::new(chart);
        st.chart.lines[0] = line;
        st.lookahead = 2.0;
        st.playhead = 0.0;
        // 流速 10（默认）时窗口是 `lookahead` 的 2 秒 ⇒ 3.0 秒那颗不在构建区间里
        assert_eq!(st.visible_range_of(0).end, 2, "流速 10 时只该有前两颗");
        // ② 流速从 10 改成 1 ⇒ `min_speed_abs` 跟着降，窗口放宽到 ~4.25 秒
        let mut tracks = st.chart.lines[0].tracks.clone();
        tracks[4].events = vec![ev(0.0, 16.0, 1.0, 1.0)];
        st.chart.lines[0].set_tracks(tracks, &tmap);
        assert_eq!(st.chart.lines[0].min_speed_abs, 1.0);
        assert_eq!(st.visible_range_of(0).end, 3, "流速 1 ⇒ 窗口 4.25 秒，3.0 秒那颗要在区间里");
    }

    /// `StaleSet`：注入、合并、按区间摘取（异步重算的账本，只有一个地方算得对才算数）
    #[test]
    fn stale_set_bookkeeping() {
        let mut s = StaleSet::default();
        assert!(s.is_empty() && s.count() == 0 && !s.contains(0));
        s.mark_from(5, 10);
        assert_eq!(s.indices(), (5..10).collect::<Vec<_>>());
        // 再往前标 ⇒ 合并成一整段
        s.mark_from(2, 10);
        assert_eq!(s.indices(), (2..10).collect::<Vec<_>>());
        // 往后标（新的流速改动弄脏尾巴）⇒ 仍是连续的一段
        s.mark_from(8, 10);
        assert_eq!(s.indices(), (2..10).collect::<Vec<_>>());
        // 摘取：只动 `region` 里的那些，最靠左优先
        let got = s.take(0..4, 1);
        assert_eq!(got, 2..3, "播放头之前那一半只摘得到 2");
        assert_eq!(s.indices(), vec![3, 4, 5, 6, 7, 8, 9]);
        let got = s.take(4..10, 3);
        assert_eq!(got, 4..7);
        assert_eq!(s.indices(), vec![3, 7, 8, 9]);
        // 越界的 region / 预算为 0：什么都不摘
        assert_eq!(s.take(0..0, 5), 0..0);
        assert_eq!(s.take(0..10, 0), 0..0);
        assert_eq!(s.count(), 4);
        // 单独标一颗（跨过改动点的长 hold 就是这么标的）：与相邻区间合并，不制造碎片
        s.mark_one(0);
        assert_eq!(s.indices(), vec![0, 3, 7, 8, 9]);
        s.mark_one(3);
        assert_eq!(s.indices(), vec![0, 3, 7, 8, 9], "已经在里面了 ⇒ 不变");
        s.mark_one(2);
        assert_eq!(s.indices(), vec![0, 2, 3, 7, 8, 9], "2 与 3 相邻 ⇒ 合成一段");
        s.mark_one(10);
        assert_eq!(s.indices(), vec![0, 2, 3, 7, 8, 9, 10]);
        s.mark_one(1);
        assert_eq!(s.indices(), (0..4).chain(7..11).collect::<Vec<_>>(), "1 把 0 与 2..3 接起来");
        assert_eq!(s.count(), 8);
        // 反复摘到空
        while !s.is_empty() {
            s.take(0..16, 2);
        }
        assert_eq!(s.count(), 0);
        // 长度 0 的线：标脏是空操作（别 panic）
        s.mark_from(0, 0);
        assert!(s.is_empty());
    }

    /// `first_speed_change`：找到第一处不同、并给出"最早可能被影响的拍"
    #[test]
    fn first_speed_change_finds_the_earliest_affected_beat() {
        let a = vec![ev(0.0, 4.0, 10.0, 10.0), ev(4.0, 8.0, 10.0, 20.0)];
        assert_eq!(first_speed_change(&a, &a), None, "一模一样 ⇒ 没有要重算的");
        // 改第二条 ⇒ 边界是 4 拍
        let b = vec![ev(0.0, 4.0, 10.0, 10.0), ev(4.0, 8.0, 10.0, 5.0)];
        assert_eq!(first_speed_change(&a, &b), Some(4.0));
        // 把第二条的起点往前提 ⇒ 最早受影响的仍是 4 拍（前面那条不动）
        let c = vec![ev(0.0, 4.0, 10.0, 10.0), ev(3.0, 8.0, 10.0, 5.0)];
        assert_eq!(first_speed_change(&a, &c), Some(3.0));
        // 删掉第一条 ⇒ 边界 0
        assert_eq!(first_speed_change(&a, &b[1..]), Some(0.0));
        // 空表 → 非空：从新事件起点开始
        assert_eq!(first_speed_change(&[], &a), Some(0.0));
    }


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

    /// **回归**：内容末端是**缓存值**，过期的那份视图模型不许再影响总长。
    ///
    /// 用户报的现象："在放了音符的谱面上，底部时间轴还是保持短的状态" —— 音符放到 68.9s，
    /// 时间轴仍停在 20.0s。根因是总长读的是 `chart.tmap.end_beat`，而那份视图模型只在 `structure`
    /// 话题变化时整表重建（加音符走"逐线局部重建"）⇒ 一直是旧值。现在读 `content_end_beat`。
    #[test]
    fn timeline_length_follows_new_notes_even_when_the_view_model_is_stale() {
        let mut doc = crate::doc::Document::default();
        doc.bpm_list = vec![crate::doc::BpmEntry {
            start: crate::doc::Beat::zero(),
            bpm: 120.0, // 1 拍 = 0.5s
            foreign: Default::default(),
        }];
        let line = &mut doc.judge_lines[0];
        line.notes.push(crate::doc::Note::new(
            crate::doc::NoteKind::Tap,
            crate::doc::Beat::new(4, 1),
            0.0,
        ));
        let mut st = EditorState::new(chart_from_doc(&doc));
        assert_eq!(st.timeline_end_beat(), 14.0, "4 拍 + 10 拍留白");

        // 文档里又加了两个音符（末端 200 拍）——但**视图模型故意不重建**（这正是加音符时的真实情况）
        st.set_content_end_beat(200.0);
        assert_eq!(st.timeline_end_beat(), 210.0, "总长要跟着新内容走");
        assert!(
            (st.timeline_duration() - 105.0).abs() < 1e-9,
            "210 拍 @120BPM = 105s（实际 {}）",
            st.timeline_duration()
        );
        // 视图模型里那份旧值仍然是 4 拍 —— 证明我们读的不是它
        assert_eq!(st.chart.tmap.end_beat, 4.0);
        // 播放头也能走到新末端（以前被短的总长夹住）
        st.seek(100.0);
        assert!((st.playhead - 100.0).abs() < 1e-9);

        // 整表重建（`set_chart`）时缓存跟着一起更新
        st.set_chart(chart_from_doc(&doc));
        assert_eq!(st.content_end_beat, 4.0, "重建后与视图模型一致（文档里确实只有那个音符）");
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

    /// **选区**：同时只有一类（框选/点选换类时旧的清掉），但**锚保留** ——
    /// 检查器要同时显示"上次点的音符"和"上次点的事件"（老界面就是这样，没理由退化）。
    #[test]
    fn selection_is_single_kind_but_anchors_persist() {
        let mut st = EditorState::new(chart_from_doc(&crate::doc::Document::default()));
        assert!(st.selection().is_empty() && st.selection_kind().is_none());

        st.select_note(3);
        assert_eq!(st.selection_kind(), Some(SelKind::Notes));
        assert_eq!(st.selected_note(), Some(3));
        assert_eq!(st.selection().len(), 1);

        // 加一个事件：音符那一半被清掉，锚留着（检查器两边都还能显示）
        st.select_event(TrackId::MoveX, 2);
        assert_eq!(st.selection_kind(), Some(SelKind::Events));
        assert_eq!(st.selected_event(), Some(2));
        assert!(!st.is_note_selected(3), "选区只有一类：事件替换了音符");
        assert_eq!(st.selected_note(), Some(3), "但锚还在");

        // Ctrl 切换：加进第二条事件 → 两条；再切一次 → 只剩一条
        st.toggle_event_selection(TrackId::MoveX, 5);
        assert_eq!(st.selection().len(), 2);
        assert_eq!(st.selected_event(), Some(5), "锚 = 最后碰过的那一个");
        st.toggle_event_selection(TrackId::MoveX, 5);
        assert_eq!(st.selection().len(), 1);
        assert_eq!(st.selected_event(), Some(2), "锚被切掉后落到集合里还在的那个");

        // 框选替换：整批换掉，锚取最小的
        st.select_events([(TrackId::Alpha, 1), (TrackId::Alpha, 7)]);
        assert_eq!(st.selection().len(), 2);
        assert_eq!(st.selected_event(), Some(1));

        // 换类：事件那一半清空，音符锚仍在
        st.select_note(4);
        assert!(st.selection().events().next().is_none());
        assert_eq!(st.selection().len(), 1);
        assert_eq!(st.selected_note(), Some(4));

        // 清空：锚也一起清（"点空白"就是要什么都没选）
        st.clear_selection();
        assert!(st.selection().is_empty());
        assert_eq!(st.selected_note(), None);
        assert_eq!(st.selected_event(), None);
    }

    /// 视图下标过期时的收敛：**整批**剔掉，不是只清锚（否则 Del 会删错东西）
    #[test]
    fn a_stale_selection_is_pruned_as_a_whole() {
        let mut st = EditorState::new(chart_from_doc(&crate::doc::Document::default()));
        // 音符那一半：文档只剩 3 个音符（视图下标 0..=2）
        st.select_notes([1, 2, 9]);
        st.retain_selection(|i| i < 3, |(t, i)| t == TrackId::Alpha && i < 2);
        assert_eq!(st.selection().notes().collect::<Vec<_>>(), vec![1, 2], "越界的 9 被剔掉");
        assert_eq!(st.selected_note(), Some(1), "锚不在集合里了就退到最小的那个");
        // 事件那一半：只剩 2 条事件（0..=1）
        st.select_events([(TrackId::Alpha, 1), (TrackId::Alpha, 7)]);
        st.retain_selection(|i| i < 3, |(t, i)| t == TrackId::Alpha && i < 2);
        assert_eq!(
            st.selection().events().collect::<Vec<_>>(),
            vec![(TrackId::Alpha, 1)],
            "越界的 7 被剔掉"
        );
        // 锚越界（它不在集合里也算）：整批剔完之后锚没了就落到集合里第一个
        st.select_event(TrackId::Alpha, 7);
        assert_eq!(st.selected_event(), Some(7));
        st.retain_selection(|i| i < 3, |(t, i)| t == TrackId::Alpha && i < 2);
        assert_eq!(st.selected_event(), None, "锚越界 ⇒ 集合空 ⇒ 锚也没了");
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
