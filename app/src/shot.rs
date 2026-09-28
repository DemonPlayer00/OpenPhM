//! 自截屏（`--shot`）的**决策逻辑**，抽成纯函数。
//!
//! 为什么值得单独一个模块：这段逻辑原来是内联在 `App::ui` 里的（带一个 `if shot.is_some()` 守卫），
//! 我把它抽成方法时**把守卫漏掉了**，于是没给 `--shot` 时也会在 `frames == shot_frame`（默认 30）发一次
//! 截图请求，下一帧拿到图就对 `None` 做 `unwrap()` —— **鼠标一动就疯狂重绘、30 帧几秒就到，于是"一移动就崩"**。
//! 用户报的 panic 就是这个。抽成纯函数 + 单测之后，"没给 --shot 时必须什么都不做"这条再也丢不了。

/// 这一帧该对截图做什么
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShotStep {
    /// 什么都不做
    Idle,
    /// 发一次截图请求（egui 会在下一帧把图回灌成事件）
    Request,
    /// 图到了：写到这个路径
    Save(String),
}

/// 决策：给定"是否配置了 --shot / 当前帧号 / 目标帧号 / 这一帧是否收到截图事件"
///
/// 三条不变量（都有单测）：
/// 1. **没配置 `--shot` 时永远 `Idle`** —— 哪怕帧号正好等于默认的目标帧（这就是那次 panic 的根因）；
/// 2. 配置了但还没到目标帧 → `Idle`；正好到 → `Request`；
/// 3. 收到图且配置了 → `Save`（**不用 unwrap**：配置在就取得到，配置不在就根本不该进这一支）。
pub fn shot_step(configured: Option<&str>, frames: u32, shot_frame: u32, got_image: bool) -> ShotStep {
    match (configured, got_image) {
        (Some(path), true) => ShotStep::Save(path.to_owned()),
        (Some(_), false) if frames == shot_frame => ShotStep::Request,
        _ => ShotStep::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **回归测试**：没给 `--shot` 时，哪怕帧号正好撞上默认目标帧，也必须什么都不做
    /// （这正是用户报的 "鼠标一动就 panic" 的根因）
    #[test]
    fn without_shot_flag_nothing_happens_even_on_the_target_frame() {
        for frames in [0, 1, 29, 30, 31, 100] {
            assert_eq!(
                shot_step(None, frames, 30, false),
                ShotStep::Idle,
                "没配置 --shot 时不该有任何动作（frames={frames}）"
            );
            // 就算莫名其妙收到了截图事件，也不该去 Save（那会 unwrap None）
            assert_eq!(shot_step(None, frames, 30, true), ShotStep::Idle);
        }
    }

    /// 配了 `--shot`：到目标帧请求、拿到图保存、其它帧不动
    #[test]
    fn with_shot_flag_it_requests_then_saves() {
        assert_eq!(shot_step(Some("/tmp/a.png"), 29, 30, false), ShotStep::Idle);
        assert_eq!(shot_step(Some("/tmp/a.png"), 30, 30, false), ShotStep::Request);
        assert_eq!(shot_step(Some("/tmp/a.png"), 31, 30, false), ShotStep::Idle, "只请求一次");
        assert_eq!(
            shot_step(Some("/tmp/a.png"), 31, 30, true),
            ShotStep::Save("/tmp/a.png".to_owned())
        );
        // 目标帧是 0 也要工作（`--shot-frame 0`）
        assert_eq!(shot_step(Some("/tmp/b.png"), 0, 0, false), ShotStep::Request);
    }
}
