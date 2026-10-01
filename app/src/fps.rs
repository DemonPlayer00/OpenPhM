// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **帧率指示**（底栏那一格读数）。用户口径：「在底栏添加帧率指示，不要影响帧刷新策略。
//! 刷新最低时间间隔为 0.5 秒」；「修改空闲策略，从每秒一帧改为直接停下」；
//! 最后一步是今天收敛到的样子：「移除 IDLE 显示，直接全部显示 fps」。
//!
//! 四条纪律，缺一条它就会变成"另一个性能问题"：
//!
//! 1. **不改帧刷新策略**：它只在**已经出帧**的时候记账 —— 整个模块不碰 `egui::Context`、
//!    不 `request_repaint`（连 `Instant` 都是调用方给的）。指示器一旦参与请求重绘，
//!    "空闲直接停下"就变成"指示器要的帧率"。
//! 2. **显示值最多每 [`MIN_INTERVAL`] 更新一次**：否则屏幕上的数字每帧都在抖，而且
//!    **每帧重建字符串 = 每帧重新排版**（白做的工作）。值没变时连文本都不重建（缓存着）。
//! 3. 取的是**这一段窗口的平均**（帧数 / 窗口时长），不是 `1 / 最后一帧` —— vsync 下单帧倒数会在
//!    16.7 / 33.3 之间来回跳，而"帧率"要回答的是"这一段跑了多快"。
//! 4. **永远显示帧率数字**（用户口径 2026-10-01：先要 IDLE，看过之后改成"移除 IDLE，全部显示 fps"）。
//!    空闲时编辑器**一帧都不出**，于是那个数字会**停在那儿不再变** —— 这是"屏幕冻结"的必然结果，
//!    不是它坏掉了。`set_idle` 只负责"睡过去的那段时长不算进帧率"，**不换文本**：
//!    读数照旧是"最近这一段（不少于 0.5 秒）实际出帧的平均"，只是不再有人刷新它。
//!    **它从不产生帧**（模块拿不到 `egui::Context`）—— 用户另一条口径："fps 控件更新不能影响总体帧"。
//!
//! 与 `--bench` 的分工：`--bench` 给整段的 p50/p99（"总体多快"），这一格给**此刻**多快，
//! 而且它是用户在真机上唯一不借助命令行/环境变量就能看到的那个数。
//!
//! **为什么桌面不会认为窗口无响应**：ping/pong 走的是**事件循环**，不是重绘。
//! X11 侧 `_NET_WM_PING` 由 winit 自己应答（`winit/src/platform_impl/linux/x11/event_processor.rs`），
//! Wayland 侧 `xdg_wm_base.ping` 由 SCTK 自动回 `pong`（`smithay-client-toolkit/src/shell/xdg/mod.rs`）——
//! 两者都在 socket 事件到达时被派发，而我们"停下"只是不再 `request_repaint`，事件循环照旧在跑。

use std::time::{Duration, Instant};

/// 显示值的**最小刷新间隔**（用户口径："刷新最低时间间隔为 0.5 秒"）。
///
/// 注意它约束的是**显示值**，不是重绘频率 —— 指示器自己不产生任何一帧。
pub const MIN_INTERVAL: Duration = Duration::from_millis(500);

/// 底栏那格的悬停说明（文案只有一处）
pub const HOVER: &str = "最近这一段（不少于 0.5 秒）**实际出帧**的平均帧率。\n\
     它只统计已经发生的帧、从不主动要求重绘。\n\
     空闲（没有播放、没有拖动、没有待应用的改动）时编辑器**完全停下**：一帧都不出，\n\
     所以这个数字会**停在那儿不再变** —— 你看到的每一帧都是输入、广播或快照截止时刻把它叫醒的。";

/// 帧率表：喂每帧的间隔与"这一帧算不算在干活"，吐出底栏那一格的文本。
#[derive(Debug, Default)]
pub struct FpsMeter {
    /// 本窗口累计的帧数与时长（窗口 = 上一次发布之后的那些帧）
    frames: u32,
    span: f64,
    /// 开窗时刻 / 上一次发布的时刻
    last: Option<Instant>,
    /// 当前显示值（fps；空闲时是 `None` —— 没有帧率可言）
    shown: Option<f64>,
    /// 当前显示文本（只在发布/切换状态时重建；空串 = 还没有值）
    text: String,
    /// 现在算不算"睡下"（空闲、而且没有人再要帧）—— 只影响记账，不影响显示
    idle: bool,
    /// 刚从睡眠里醒来：下一帧立刻给个新读数（别让屏幕上挂着睡前的旧值太久）
    wake: bool,
}

impl FpsMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// **帧末调用**（空闲策略的边界都在这里）：`idle` = 这一帧之后我们不再要帧。
    ///
    /// - **从"忙"切到"空闲"**：这一帧还算数 ⇒ 记账并**立刻发布**（不等 0.5 秒节流）。
    ///   屏幕接下来会冻住，所以停在上面的必须是一个真实帧率（窗口可能不满 0.5 秒 —— 照现有窗口算）。
    ///   没有这一步，界面上会留一个**空格**（还没凑满第一个窗口就睡下了，见 2026-10-01 的实测）。
    /// - **一直空闲**：什么都不做（空闲帧的 delta 是"睡过去的时长"，不是帧间隔）。
    /// - **从"空闲"切到"忙"**：窗口作废，下一帧立刻给个新数（不让睡前的旧值久留）。
    ///
    /// 返回值 = 是否发布了新读数（给单测数刷新次数）。**从不请求帧**（模块碰不到 `egui::Context`）。
    pub fn note_frame(&mut self, now: Instant, delta_ms: Option<f64>, idle: bool) -> bool {
        if idle {
            let entering = !self.idle;
            self.idle = true;
            if !entering {
                return false; // 一直空闲：不记账、不发布
            }
            self.accumulate(delta_ms);
            let published = self.publish(now);
            self.reset_window();
            return published;
        }
        if std::mem::take(&mut self.idle) {
            // 刚醒：这一帧的 delta 是睡过去的时长，丢掉；窗口作废，下一帧立刻给数
            self.reset_window();
            self.wake = true;
            return false;
        }
        self.accumulate(delta_ms);
        let due = match self.last {
            // 开窗：起步时先攒够一个窗口（单个帧间隔不足以代表帧率，启动帧还带着建表的代价）；
            // 但**刚睡醒**时不等 —— 屏幕上挂着的是睡前那个数，先给个新的比"再等半秒"重要
            None => {
                self.last = Some(now);
                self.frames > 0 && self.wake
            }
            Some(_) if self.wake => true,
            Some(t) => now.saturating_duration_since(t) >= MIN_INTERVAL,
        };
        if due { self.publish(now) } else { false }
    }

    /// 把一帧的间隔记进窗口（坏值丢掉：`NaN`/非正/无穷既不污染读数也不触发发布）
    fn accumulate(&mut self, delta_ms: Option<f64>) {
        if let Some(ms) = delta_ms {
            let secs = ms / 1000.0;
            if secs.is_finite() && secs > 0.0 {
                self.frames += 1;
                self.span += secs;
            }
        }
    }

    /// 发布当前窗口（帧数 / 时长）；窗口空就什么都不做
    fn publish(&mut self, now: Instant) -> bool {
        if self.frames == 0 || !(self.span > 0.0) {
            return false;
        }
        let fps = self.frames as f64 / self.span;
        self.shown = Some(fps);
        self.text = format!("{fps:.1} fps");
        self.frames = 0;
        self.span = 0.0;
        self.last = Some(now);
        self.wake = false;
        true
    }

    /// 窗口作废（睡下/醒来时都要）：睡过去的时长不是帧间隔
    fn reset_window(&mut self) {
        self.frames = 0;
        self.span = 0.0;
        self.last = None;
    }

    /// 当前显示值（空闲时是 `None`；还没凑满第一个窗口时也是 `None`）
    pub fn shown(&self) -> Option<f64> {
        self.shown
    }

    /// 现在是不是"睡下了"（空闲且不再要帧）—— 只影响记账，不影响显示
    pub fn is_idle(&self) -> bool {
        self.idle
    }

    /// 底栏那一格的文本（值不变时返回缓存的那一份 —— 帧里不重建字符串）
    pub fn text(&self) -> Option<&str> {
        (!self.text.is_empty()).then_some(self.text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个"从 base 起 ms 毫秒处"的时刻
    fn at(base: Instant, ms: f64) -> Instant {
        base + Duration::from_secs_f64(ms / 1000.0)
    }

    /// **0.5 秒才刷新一次**：5 秒里 16.7 ms 一帧（300 帧）最多发布 10 次，且两次间隔 ≥ 0.5s
    #[test]
    fn the_displayed_value_updates_at_most_every_half_second() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        let mut published_at: Vec<f64> = Vec::new();
        for i in 0..300 {
            let ms = 16.6667 * i as f64;
            if m.note_frame(at(base, ms), Some(16.6667), false) {
                published_at.push(ms);
            }
        }
        assert!(
            published_at.len() <= 10,
            "5 秒里发布了 {} 次：{published_at:?}",
            published_at.len()
        );
        assert!(published_at.len() >= 9, "发布得太少：{published_at:?}");
        for w in published_at.windows(2) {
            assert!(w[1] - w[0] >= 500.0 - 16.7, "两次刷新间隔不足 0.5s：{published_at:?}");
        }
        let fps = m.shown().expect("应当有显示值");
        assert!((fps - 60.0).abs() < 1.0, "16.7ms 一帧应当是 60 fps 上下，实际 {fps}");
    }

    /// 取的是**窗口平均**，不是"最后一帧的倒数"：窗口里 10 帧 10 ms + 10 帧 40 ms ⇒ 40 fps
    /// （最后一帧是 40 ms，它的倒数是 25 fps —— 那个数是错的）
    #[test]
    fn the_value_is_the_window_average_not_the_last_frame() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        m.note_frame(base, None, false); // 开窗
        let mut t = 0.0;
        for _ in 0..10 {
            t += 10.0;
            assert!(!m.note_frame(at(base, t), Some(10.0), false), "还没到 0.5s");
        }
        let mut published = false;
        for _ in 0..10 {
            t += 40.0;
            published |= m.note_frame(at(base, t), Some(40.0), false);
        }
        assert!(published, "凑满 0.5 秒就该发布");
        let fps = m.shown().unwrap();
        assert!((fps - 40.0).abs() < 0.5, "窗口平均是 20 帧 / 0.5s = 40 fps，实际 {fps}");
    }

    /// **永远显示帧率数字**（用户口径："移除 IDLE 显示，直接全部显示 fps"）：
    /// ① 睡下的那一帧**必须留下一个真实读数**（屏幕要冻住了，不能留空格）；
    /// ② 之后空闲帧不记账、不发布，数字就那么停着；③ 醒来第一帧给新数（不等 0.5 秒）。
    #[test]
    fn the_cell_always_ends_up_showing_a_frame_rate() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        // 只跑 6 帧（不到 0.5 秒）就决定睡下：这正是"启动后马上空闲"的情形
        let mut t = 0.0;
        for _ in 0..6 {
            t += 16.6667;
            assert!(!m.note_frame(at(base, t), Some(16.6667), false), "6 帧还不到 0.5 秒");
        }
        assert_eq!(m.text(), None, "还没发布过就是空的");
        // 决定睡下：这一帧**强制发布**，于是屏幕上留下的是一个真实帧率
        assert!(m.note_frame(at(base, t), Some(16.6667), true), "睡下那一帧必须发布一个读数");
        let shown = m.text().unwrap().to_owned();
        assert!(shown.ends_with("fps"), "留下的必须是一个帧率：{shown}");
        let fps: f64 = shown.trim_end_matches(" fps").parse().unwrap();
        assert!((fps - 60.0).abs() < 3.0, "6 帧 16.7ms ⇒ 约 60 fps，实际 {shown}");
        // 之后一直空闲：不记账、不发布、文本不变
        for i in 0..5 {
            assert!(!m.note_frame(at(base, t + 1000.0 * i as f64), Some(1000.0), true), "空闲帧不该发布");
            assert_eq!(m.text(), Some(shown.as_str()));
        }
        assert!(m.is_idle());
        // 醒来：这一帧丢掉（delta 是睡过去的时长），下一帧立刻给新值
        assert!(!m.note_frame(at(base, t + 60_000.0), Some(60_000.0), false));
        assert_eq!(m.text(), Some(shown.as_str()), "醒来这一帧还挂着睡前那个数");
        assert!(m.note_frame(at(base, t + 60_016.0), Some(16.6667), false), "下一帧立刻给新数");
        let fresh: f64 = m.text().unwrap().trim_end_matches(" fps").parse().unwrap();
        assert!((fresh - 60.0).abs() < 2.0, "{:?}", m.text());
    }

    /// 空闲但**有心跳**（`--idle-fps N`，诊断用）时照旧报帧率：那时确实还在自己出帧
    #[test]
    fn an_idle_heartbeat_still_reports_its_own_rate() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        assert!(!m.note_frame(base, None, false), "第一帧只开窗");
        let mut published = 0;
        for i in 1..=5 {
            if m.note_frame(at(base, 1000.0 * i as f64), Some(1000.0), false) {
                published += 1;
            }
        }
        assert!(published >= 4, "每秒一帧应当每帧都发布一次新值：{published}");
        assert!((m.shown().unwrap() - 1.0).abs() < 1e-9, "{:?}", m.shown());
        assert_eq!(m.text(), Some("1.0 fps"));
    }

    /// 还没凑满一个窗口时**一个字都不显示**（不许拿单帧样本冒充帧率）
    #[test]
    fn nothing_is_displayed_before_a_full_window() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        m.note_frame(base, None, false);
        assert_eq!(m.text(), None);
        for i in 1..10 {
            assert!(!m.note_frame(at(base, 16.0 * i as f64), Some(16.0), false));
        }
        assert_eq!(m.text(), None, "0.16 秒还不到一个窗口");
        assert_eq!(m.shown(), None);
    }

    /// **醒来要立刻给数**，而且睡过去的时长不能算进帧率
    /// （否则睡 60 秒醒来会算出 0.02 fps 这种荒谬值）
    #[test]
    fn waking_up_ignores_the_nap_and_publishes_a_fresh_value() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        let mut t = 0.0;
        while m.shown().is_none() {
            t += 16.6667;
            m.note_frame(at(base, t), Some(16.6667), false);
        }
        m.note_frame(at(base, t + 16.6667), Some(16.6667), true); // 睡下
        t += 60_000.0; // 睡了一分钟（屏幕上一直是睡前那个数字）
        // 醒来（这一帧的 delta 是睡过去的时长，必须丢掉）
        assert!(!m.note_frame(at(base, t), Some(60_000.0), false));
        // 醒来这一帧**还没数出新值** ⇒ 屏幕上暂时还是睡前那个数（不是 0.02 fps 那种荒谬值）
        let stale = m.shown().unwrap();
        assert!((stale - 60.0).abs() < 2.0, "睡前的读数留着，实际 {stale}");
        // 下一帧：立刻发布（不等 0.5 秒），值来自**新鲜**的帧间隔
        assert!(m.note_frame(at(base, t + 16.6667), Some(16.6667), false), "醒来后该立刻给个数");
        let fps = m.shown().unwrap();
        assert!((fps - 60.0).abs() < 2.0, "60 fps 上下，实际 {fps}");
        assert_eq!(m.text(), Some("60.0 fps"));
    }

    /// 坏值（NaN / 负 / 0 / 无穷）既不污染读数也不触发发布
    #[test]
    fn bad_deltas_are_ignored() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        m.note_frame(base, None, false);
        for bad in [f64::NAN, -5.0, 0.0, f64::INFINITY] {
            assert!(!m.note_frame(at(base, 1000.0), Some(bad), false), "{bad} 不该发布");
        }
        assert_eq!(m.shown(), None, "全是坏值，没有可显示的数");
        assert_eq!(m.text(), None);
        // 掺一个真值就该正常发布
        assert!(m.note_frame(at(base, 2000.0), Some(100.0), false));
        assert!((m.shown().unwrap() - 10.0).abs() < 1e-9, "{:?}", m.shown());
    }

    /// 文本是**缓存**的：值没变就不重建字符串（帧里不做无谓的格式化/排版）
    #[test]
    fn the_text_is_cached_between_refreshes() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        m.note_frame(base, None, false);
        let mut t = 0.0;
        while !m.note_frame(at(base, t), Some(20.0), false) {
            t += 20.0;
        }
        let first = m.text().unwrap().to_owned();
        assert_eq!(first, "50.0 fps");
        let mut published = 0;
        for _ in 0..10 {
            t += 20.0;
            if m.note_frame(at(base, t), Some(20.0), false) {
                published += 1;
            }
        }
        assert_eq!(published, 0, "0.2 秒内不该再发布");
        assert_eq!(m.text(), Some(first.as_str()));
    }

    /// **记账本身的代价**（微基准，手动跑）：
    /// `cargo test --release --lib fps:: -- --ignored --nocapture`
    ///
    /// 为什么要有它：用户的要求是"不要影响帧刷新策略"，而"不影响"要能落到数上 ——
    /// 不是"我觉得几次浮点很快"。这里量的是**每一帧**都要走的那条路（`note_frame`），
    /// 不包括 0.5 秒才走一次的格式化。
    #[test]
    #[ignore = "微基准：手动跑"]
    fn note_frame_cost_per_frame() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        let n: u64 = 2_000_000;
        let t = Instant::now();
        for i in 0..n {
            // 16.67 ms 一帧（60 fps）的模拟时间戳
            let at = base + Duration::from_nanos(i * 16_666_667 / 1000);
            m.note_frame(at, Some(16.6667), false);
        }
        let ns = t.elapsed().as_secs_f64() * 1e9 / n as f64;
        println!(
            "\n[fps] note_frame（每帧都要走的那条路）: **{ns:.1} ns/帧**（{n} 次，含 0.5 秒一次的格式化与发布）"
        );
        println!("[fps] 作为对照：一个 60 fps 的帧预算 = 16.7 ms ⇒ 占了 {:.9}%", ns / 16_666_667.0 * 100.0);
    }
}
