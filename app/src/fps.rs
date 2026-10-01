// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **帧率指示**（底栏那一格读数）。用户口径：「在底栏添加帧率指示，不要影响帧刷新策略。
//! 刷新最低时间间隔为 0.5 秒」。
//!
//! 三条纪律，缺一条它就会变成"另一个性能问题"：
//!
//! 1. **不改帧刷新策略**：它只在**已经出帧**的时候记账 —— 整个模块不碰 `egui::Context`、
//!    不 `request_repaint`（连 `Instant` 都是调用方给的）。于是空闲时它显示的就是心跳的真实帧率
//!    （默认 1 fps），而不是"被指示器抬起来"的帧率；`--idle-fps 0`（纯事件驱动）下它甚至不会更新 ——
//!    没有帧就没有新读数，屏幕上留着上一个值，直到下一次真的出帧。
//! 2. **显示值最多每 [`MIN_INTERVAL`] 更新一次**：否则屏幕上的数字每帧都在抖，而且
//!    **每帧重建字符串 = 每帧重新排版**（白做的工作）。值没变时连文本都不重建（缓存着）。
//! 3. 取的是**这一段窗口的平均**（帧数 / 窗口时长），不是 `1 / 最后一帧` —— vsync 下单帧倒数会在
//!    16.7 / 33.3 之间来回跳，而"帧率"要回答的是"这一段跑了多快"。
//!
//! 与 `--bench` 的分工：`--bench` 给整段的 p50/p99（"总体多快"），这一格给**此刻**多快，
//! 而且它是用户在真机上唯一不借助命令行/环境变量就能看到的那个数。

use std::time::{Duration, Instant};

/// 显示值的**最小刷新间隔**（用户口径："刷新最低时间间隔为 0.5 秒"）。
///
/// 注意它约束的是**显示值**，不是重绘频率 —— 指示器自己不产生任何一帧。
pub const MIN_INTERVAL: Duration = Duration::from_millis(500);

/// 底栏那格的悬停说明（文案只有一处）
pub const HOVER: &str = "最近这一段（不少于 0.5 秒）**实际出帧**的平均帧率。\n\
     它只统计已经发生的帧、从不主动要求重绘：空闲时那是心跳的真实帧率（默认 1 fps），\n\
     纯事件驱动（--idle-fps 0）下要等下一次真的出帧才会变。";

/// 帧率表：喂帧间隔，吐出"最多每 0.5 秒更新一次"的显示值。
#[derive(Debug, Default)]
pub struct FpsMeter {
    /// 本窗口累计的帧数与时长（窗口 = 上一次发布之后的那些帧）
    frames: u32,
    span: f64,
    /// 开窗时刻 / 上一次发布的时刻
    last: Option<Instant>,
    /// 当前显示值（fps）
    shown: Option<f64>,
    /// 当前显示文本（只在发布时重建；空串 = 还没有值）
    text: String,
}

impl FpsMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一帧。`delta_ms` = 与上一帧的间隔（毫秒；第一帧没有 ⇒ `None`）。
    ///
    /// 返回 `true` 表示这一帧**发布了新显示值**（单测用它数"刷新了几次"）。
    /// 坏值（`NaN` / 非正 / 无穷）被丢掉：它们既不该污染读数，也不该触发发布。
    pub fn note_frame(&mut self, now: Instant, delta_ms: Option<f64>) -> bool {
        if let Some(ms) = delta_ms {
            let secs = ms / 1000.0;
            if secs.is_finite() && secs > 0.0 {
                self.frames += 1;
                self.span += secs;
            }
        }
        match self.last {
            // 第一个能统计的帧只**开窗**：单个帧间隔（启动帧还可能带着建表/首帧布局的代价）
            // 不足以代表"帧率"，等凑满一个 MIN_INTERVAL 再说
            None => {
                self.last = Some(now);
                return false;
            }
            Some(t) if now.saturating_duration_since(t) < MIN_INTERVAL => return false,
            Some(_) => {}
        }
        if self.frames == 0 || !(self.span > 0.0) {
            return false;
        }
        let fps = self.frames as f64 / self.span;
        self.shown = Some(fps);
        self.text = format!("{fps:.1} fps");
        self.frames = 0;
        self.span = 0.0;
        self.last = Some(now);
        true
    }

    /// 当前显示值（还没凑满第一个窗口时是 `None`）
    pub fn shown(&self) -> Option<f64> {
        self.shown
    }

    /// 底栏那一格的文本（值不变时返回缓存的那一份 —— 帧里不重建字符串）
    pub fn text(&self) -> Option<&str> {
        (!self.text.is_empty()).then_some(self.text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **0.5 秒才刷新一次**：5 秒里 16.7 ms 一帧（300 帧）最多发布 10 次，且两次间隔 ≥ 0.5s
    #[test]
    fn the_displayed_value_updates_at_most_every_half_second() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        let mut published_at: Vec<f64> = Vec::new();
        for i in 0..300 {
            let ms = 16.6667 * i as f64;
            if m.note_frame(base + Duration::from_secs_f64(ms / 1000.0), Some(16.6667)) {
                published_at.push(ms);
            }
        }
        assert!(published_at.len() <= 10, "5 秒里发布了 {} 次：{published_at:?}", published_at.len());
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
        m.note_frame(base, None); // 开窗
        let mut t = 0.0;
        for _ in 0..10 {
            t += 10.0;
            assert!(!m.note_frame(base + Duration::from_secs_f64(t / 1000.0), Some(10.0)), "还没到 0.5s");
        }
        let mut published = false;
        for _ in 0..10 {
            t += 40.0;
            published |= m.note_frame(base + Duration::from_secs_f64(t / 1000.0), Some(40.0));
        }
        assert!(published, "凑满 0.5 秒就该发布");
        let fps = m.shown().unwrap();
        assert!((fps - 40.0).abs() < 0.5, "窗口平均是 20 帧 / 0.5s = 40 fps，实际 {fps}");
    }

    /// 空闲心跳（1 fps）要**照实**显示 1.0 —— 指示器不许把它抬起来
    #[test]
    fn an_idle_heartbeat_shows_the_heartbeat_rate() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        assert!(!m.note_frame(base, None), "第一帧只开窗");
        let mut published = 0;
        for i in 1..=5 {
            if m.note_frame(base + Duration::from_millis(1000 * i), Some(1000.0)) {
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
        m.note_frame(base, None);
        assert_eq!(m.text(), None);
        for i in 1..10 {
            assert!(!m.note_frame(base + Duration::from_millis(16 * i), Some(16.0)));
        }
        assert_eq!(m.text(), None, "0.16 秒还不到一个窗口");
        assert_eq!(m.shown(), None);
    }

    /// 坏值（NaN / 负 / 0 / 无穷）既不污染读数也不触发发布
    #[test]
    fn bad_deltas_are_ignored() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        m.note_frame(base, None);
        for bad in [f64::NAN, -5.0, 0.0, f64::INFINITY] {
            assert!(!m.note_frame(base + Duration::from_secs(1), Some(bad)), "{bad} 不该发布");
        }
        assert_eq!(m.shown(), None, "全是坏值，没有可显示的数");
        assert_eq!(m.text(), None);
        // 掺一个真值就该正常发布
        assert!(m.note_frame(base + Duration::from_secs(2), Some(100.0)));
        assert!((m.shown().unwrap() - 10.0).abs() < 1e-9, "{:?}", m.shown());
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
            m.note_frame(at, Some(16.6667));
        }
        let ns = t.elapsed().as_secs_f64() * 1e9 / n as f64;
        println!("\n[fps] note_frame（每帧都要走的那条路）: **{ns:.1} ns/帧**（{n} 次，含 0.5 秒一次的格式化与发布）");
        println!("[fps] 作为对照：一个 60 fps 的帧预算 = 16.7 ms ⇒ 占了 {:.9}%", ns / 16_666_667.0 * 100.0);
    }

    /// 文本是**缓存**的：值没变就不重建字符串（帧里不做无谓的格式化/排版）
    #[test]
    fn the_text_is_cached_between_refreshes() {
        let base = Instant::now();
        let mut m = FpsMeter::new();
        m.note_frame(base, None);
        let mut t = 0.0;
        while !m.note_frame(base + Duration::from_secs_f64(t / 1000.0), Some(20.0)) {
            t += 20.0;
        }
        let first = m.text().unwrap().to_owned();
        assert_eq!(first, "50.0 fps");
        // 下一个窗口之前：内容不变（每帧都重建会得到同样的串，但那是白做的工作 —— 这里用
        // 返回值的语义钉住"只有发布才重建"）
        let mut published = 0;
        for _ in 0..10 {
            t += 20.0;
            if m.note_frame(base + Duration::from_secs_f64(t / 1000.0), Some(20.0)) {
                published += 1;
            }
        }
        assert_eq!(published, 0, "0.2 秒内不该再发布");
        assert_eq!(m.text(), Some(first.as_str()));
    }
}
