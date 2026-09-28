//! 快捷键的**状态规则**（纯逻辑，与 egui/winit 无关，可单测）。
//!
//! 目前只有空格键的"自动播放"语义，但它有四种组合（停/播 × 单点/长按），
//! 靠"按下去就 toggle"那种一行写法会长出互相打架的特例 —— 所以抽成状态机：
//!
//! | 场景 | 按下 | 松开 |
//! |---|---|---|
//! | 停着 + **单点** | 开始播放 | 继续播（**进入**自动播放） |
//! | 播放中 + **单点** | 立刻暂停（**退出**） | 不做事 |
//! | 停着 + **长按** | 开始播放 | **暂停**（松手即退出，像"按住试听"） |
//! | 播放中 + **长按** | 立刻暂停 | 不做事 |
//!
//! 两个刻意的选择：
//! 1. **播放中按下就停**（不等松手）：单点退出要即时反馈，等松手会慢半拍；
//! 2. **长按退出只对"本次按下才开始播"生效**：否则松手会把用户自己之前的播放状态也停掉。

/// 按住多久算"长按"（秒）。0.28 s：比最快的连点（~0.15 s）长，又不至于让人等。
pub const SPACE_HOLD_SECS: f64 = 0.28;

/// 空格键要发起的动作
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayAction {
    Play,
    Pause,
}

/// 空格键状态机
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SpacePlayback {
    /// 本次按下的时刻（秒，来自 `InputState::time`）；None = 当前没按着
    pressed_at: Option<f64>,
    /// 本次按下**是不是它开始播放的**（长按松手时据此决定要不要停）
    started_by_press: bool,
}

impl SpacePlayback {
    /// 按下空格。`playing` = 按下去之前的播放状态。
    ///
    /// 返回要执行的动作（None = 什么都不做，例如重复按键事件）。
    pub fn on_press(&mut self, now: f64, playing: bool) -> Option<PlayAction> {
        if self.pressed_at.is_some() {
            return None; // 已经在按住状态（egui 的重复按下事件）—— 别把时刻刷掉，否则长按判不出来
        }
        self.pressed_at = Some(now);
        if playing {
            // 播放中按下：立刻退出（单点与长按都成立）
            self.started_by_press = false;
            Some(PlayAction::Pause)
        } else {
            self.started_by_press = true;
            Some(PlayAction::Play)
        }
    }

    /// 松开空格。`playing` = 松手**之前**的播放状态。
    pub fn on_release(&mut self, now: f64, playing: bool) -> Option<PlayAction> {
        let Some(t0) = self.pressed_at.take() else {
            return None; // 没记录到按下（例如焦点是刚进来的）—— 不做猜测
        };
        let held = now - t0;
        // 单点：什么都不做 —— 若这次按下开了播放，就等于"进入自动播放"并保持
        // 长按：只有本次按下开了播放才停（不能把用户原本的播放状态停掉）
        if held >= SPACE_HOLD_SECS && self.started_by_press && playing {
            self.started_by_press = false;
            Some(PlayAction::Pause)
        } else {
            self.started_by_press = false;
            None
        }
    }

    /// **一帧的入口**：把这一帧的输入喂进来，返回"这一帧要把播放设成什么"（None = 不动）。
    ///
    /// 主循环只调这一个方法，规则全在这里 —— 于是"打字时按下要复位""模态框期间不响应"
    /// 这类容易漏的分支都能单测到，而不是散在 UI 代码里靠人肉检查。
    pub fn step(
        &mut self,
        allowed: bool,
        down: bool,
        up: bool,
        now: f64,
        playing: bool,
    ) -> Option<bool> {
        if !allowed {
            // 打字/模态框期间：按下也要复位，否则松开时会拿一个陈旧的按下时刻去算"长按"
            self.cancel();
            return None;
        }
        let mut act = None;
        if down {
            act = self.on_press(now, playing).or(act);
        }
        if up {
            act = self.on_release(now, playing).or(act);
        }
        act.map(|a| a == PlayAction::Play)
    }

    /// 当前是否按着（界面提示用）
    pub fn is_held(&self) -> bool {
        self.pressed_at.is_some()
    }

    /// 焦点丢失（`WindowFocused(false)`）时复位：否则回来时会以为一直按着
    pub fn cancel(&mut self) {
        self.pressed_at = None;
        self.started_by_press = false;
    }
}

/// 键盘输入该不该被当成快捷键：输入框（控制台）获得焦点时不算 ——
/// 命令 JSON 里空格极常见，否则在控制台里敲空格就会跳播。
pub fn shortcut_allowed(wants_keyboard_input: bool, modal_open: bool) -> bool {
    !wants_keyboard_input && !modal_open
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 四种组合逐一钉住（这张表就是需求本身）
    #[test]
    fn tap_enters_and_exits_hold_plays_until_release() {
        // 停着 + 单点 → 开始播，松手继续播（进入自动播放）
        let mut s = SpacePlayback::default();
        assert_eq!(s.on_press(10.0, false), Some(PlayAction::Play));
        assert_eq!(s.on_release(10.1, true), None, "单点松手不该停");
        assert!(!s.is_held());

        // 播放中 + 单点 → 按下即暂停（退出自动播放）
        let mut s = SpacePlayback::default();
        assert_eq!(s.on_press(20.0, true), Some(PlayAction::Pause));
        assert_eq!(s.on_release(20.1, false), None);

        // 停着 + 长按 → 按下开始播，松手暂停
        let mut s = SpacePlayback::default();
        assert_eq!(s.on_press(30.0, false), Some(PlayAction::Play));
        assert!(s.is_held());
        assert_eq!(s.on_release(30.5, true), Some(PlayAction::Pause), "长按松手要退出");

        // 播放中 + 长按 → 按下立即暂停，松手不做第二次动作
        let mut s = SpacePlayback::default();
        assert_eq!(s.on_press(40.0, true), Some(PlayAction::Pause));
        assert_eq!(s.on_release(40.9, false), None);
    }

    /// 边界：刚好等于阈值算长按；阈值前后各差一点
    #[test]
    fn hold_threshold_is_inclusive() {
        let mut s = SpacePlayback::default();
        s.on_press(0.0, false);
        assert_eq!(s.on_release(SPACE_HOLD_SECS - 0.001, true), None, "差一点算单点");
        let mut s = SpacePlayback::default();
        s.on_press(0.0, false);
        assert_eq!(s.on_release(SPACE_HOLD_SECS, true), Some(PlayAction::Pause), "正好等于算长按");
    }

    /// 长按松手**不能**停掉"用户自己之前就在播"的播放：这次按下没开始播放，就不该由松手来结束
    #[test]
    fn release_never_stops_playback_it_did_not_start() {
        let mut s = SpacePlayback::default();
        // 用户在播放中按住空格：按下已暂停，松手不再有动作
        assert_eq!(s.on_press(1.0, true), Some(PlayAction::Pause));
        assert_eq!(s.on_release(5.0, false), None);
        // 状态复位后，即便"松手时仍在播"（例如外部命令又开了播放），也不该由一次长按松手来停
        let mut s = SpacePlayback::default();
        s.on_press(1.0, true); // 按下时在播 ⇒ started_by_press = false
        assert_eq!(s.on_release(9.0, true), None, "不是它开的播放，松手不该停");
    }

    /// 重复按下事件不能刷新按下时刻（否则一直按住时松手那一刻会被误判成"单点"）
    #[test]
    fn repeated_press_events_do_not_reset_the_timer() {
        let mut s = SpacePlayback::default();
        assert_eq!(s.on_press(0.0, false), Some(PlayAction::Play));
        for t in [0.1, 0.2, 0.3, 0.4] {
            assert_eq!(s.on_press(t, true), None, "重复按下不该产生动作");
        }
        assert_eq!(s.on_release(0.5, true), Some(PlayAction::Pause), "仍应判为长按");
    }

    /// 没记录到按下就松手（例如窗口刚获得焦点）：不做任何猜测
    #[test]
    fn release_without_press_does_nothing() {
        let mut s = SpacePlayback::default();
        assert_eq!(s.on_release(1.0, true), None);
        assert_eq!(s.on_release(1.0, false), None);
    }

    /// 失焦要复位
    #[test]
    fn cancel_resets_state() {
        let mut s = SpacePlayback::default();
        s.on_press(0.0, false);
        assert!(s.is_held());
        s.cancel();
        assert!(!s.is_held());
        assert_eq!(s.on_release(1.0, true), None, "复位后松手不该冒出动作");
    }

    /// 一帧入口：打字期间按下 → 松开时不能误判成长按
    #[test]
    fn step_ignores_and_resets_while_typing() {
        let mut s = SpacePlayback::default();
        // 在控制台里打字时按住空格：不响应，也不留状态
        assert_eq!(s.step(false, true, false, 0.0, false), None);
        assert!(!s.is_held(), "被挡住时不该记下按下");
        // 之后真的松手：不该因为"按下时刻很早"而冒出暂停
        assert_eq!(s.step(true, false, true, 5.0, true), None);

        // 正常允许时：单点进入播放（返回 Some(true) = 设成播放）
        let mut s = SpacePlayback::default();
        assert_eq!(s.step(true, true, false, 0.0, false), Some(true));
        assert_eq!(s.step(true, false, true, 0.1, true), None, "单点松手不改播放状态");

        // 长按松手 → Some(false) = 设成暂停
        let mut s = SpacePlayback::default();
        assert_eq!(s.step(true, true, false, 0.0, false), Some(true));
        assert_eq!(s.step(true, false, true, 0.4, true), Some(false));
    }

    /// 输入框/模态框在前时快捷键要让路
    #[test]
    fn shortcuts_yield_to_typing_and_modals() {
        assert!(shortcut_allowed(false, false));
        assert!(!shortcut_allowed(true, false), "控制台打字时空格不是播放");
        assert!(!shortcut_allowed(false, true), "模态框在上时不该响应");
        assert!(!shortcut_allowed(true, true));
    }
}

/// **快速放置**：Q/W/E/R → tap/flick/drag/hold（键盘从左到右 = 种类列表的顺序）。
///
/// 只做"按键 → 种类"这一件事：**位置**由编辑区决定（只有它知道指针在哪、怎么吸附），
/// "现在能不能用键"由调用方算好（打字/模态期间不算）—— 三件事分开，各自可测。
pub fn quick_place_kind(key: egui::Key) -> Option<crate::doc::NoteKind> {
    use crate::doc::NoteKind;
    match key {
        egui::Key::Q => Some(NoteKind::Tap),
        egui::Key::W => Some(NoteKind::Flick),
        egui::Key::E => Some(NoteKind::Drag),
        egui::Key::R => Some(NoteKind::Hold),
        _ => None,
    }
}

#[cfg(test)]
mod quick_place_tests {
    use super::*;

    /// 四个键各对应一种音符；别的键不给东西（免得 H/空格这类功能键被抢走）
    #[test]
    fn quick_place_keys_map_to_note_kinds() {
        use crate::doc::NoteKind;
        assert_eq!(quick_place_kind(egui::Key::Q), Some(NoteKind::Tap));
        assert_eq!(quick_place_kind(egui::Key::W), Some(NoteKind::Flick));
        assert_eq!(quick_place_kind(egui::Key::E), Some(NoteKind::Drag));
        assert_eq!(quick_place_kind(egui::Key::R), Some(NoteKind::Hold));
        for k in [egui::Key::H, egui::Key::Space, egui::Key::A, egui::Key::Enter] {
            assert_eq!(quick_place_kind(k), None, "{k:?} 不该是快速放置键");
        }
        // 四种种类都要被覆盖到（漏一个就是"某个键没反应"）
        let kinds: Vec<NoteKind> = [egui::Key::Q, egui::Key::W, egui::Key::E, egui::Key::R]
            .iter()
            .filter_map(|k| quick_place_kind(*k))
            .collect();
        assert_eq!(kinds, vec![NoteKind::Tap, NoteKind::Flick, NoteKind::Drag, NoteKind::Hold]);
    }
}
