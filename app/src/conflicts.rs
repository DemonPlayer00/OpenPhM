//! **冲突浏览器**：列出 `EditCore` 报出来的事件重叠，点一下跳到那里。
//!
//! 面板只做两件事：把 [`Overlap`] 画成可点的行、把点击变成 [`ConflictJump`]。
//! **跳转本身（选中线/轨道/事件 + 移动播放头）在调用点施加** —— 那是视图状态，不是文档数据，
//! 而且"点了之后怎么跳"与"怎么画"混在一起时，两边都不好改。
//!
//! 重叠数据来自 `EditCore::overlaps()`（GUI 与 CLI 同一份），这里的 `label()`/`pointer()` 也来自
//! 同一处 —— 所以界面上那几行与 `opm-ctl --file x overlaps` 打印的内容是**同一批字**。

use egui::Ui;
use opm_app::cmd::Overlap;

/// 用户点中的条目：跳到哪条线、哪条轨道、哪条事件、哪个拍
#[derive(Clone, Debug, PartialEq)]
pub struct ConflictJump {
    /// 文档里的判定线下标（不是视图序）
    pub line_doc: usize,
    /// 轨道键（`moveX`/`alpha`…）
    pub track: String,
    /// 该轨道内的事件下标
    pub event: usize,
    /// 重叠起点的拍
    pub beat: f64,
}

impl ConflictJump {
    /// 从一条重叠记录推出跳转目标（**纯映射**，有单测）
    pub fn of(o: &Overlap) -> Self {
        Self {
            line_doc: o.line,
            track: o.track.clone(),
            event: o.next,
            beat: o.start.to_f64(),
        }
    }
}

/// 画冲突浏览器。返回 `(要点跳的条目, 用户是否按了关闭)`。
pub fn conflicts_ui(ui: &mut Ui, conflicts: &[Overlap]) -> (Option<ConflictJump>, bool) {
    let mut jump: Option<ConflictJump> = None;
    let mut close = false;
    egui::Panel::bottom("conflict_browser").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.colored_label(
                egui::Color32::from_rgb(255, 110, 110),
                format!("冲突浏览器（{} 处事件重叠）", conflicts.len()),
            );
            ui.label("点击条目跳转（定位播放头 + 选中该事件）");
            if ui.small_button("关闭").clicked() {
                close = true;
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("conflict_scroll")
            .max_height(110.0)
            .show(ui, |ui| {
                for o in conflicts {
                    if ui
                        .selectable_label(false, format!("⚠ {}", o.label()))
                        .on_hover_text(o.pointer())
                        .clicked()
                    {
                        jump = Some(ConflictJump::of(o));
                    }
                }
            });
    });
    (jump, close)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opm_app::core::EditCore;
    use serde_json::json;

    // 一帧里画出来的所有文本：实现搬到 `testkit`（三个面板的测试曾各抄一份）
    use crate::testkit::drawn_texts as texts;

    /// 造两处重叠（同一条线的 alpha 轨道上放三条互相覆盖的事件）
    fn two_overlaps() -> EditCore {
        let mut c = EditCore::new();
        for (start, end) in [(0, 8), (4, 12), (6, 20)] {
            let r = c.exec(&json!({"op":"add_event","line":0,"layer":0,"track":"alpha",
                                   "startBeat":[start,1],"endBeat":[end,1],
                                   "startValue":1.0,"endValue":0.0}));
            assert_eq!(r["ok"], json!(true), "{r}");
        }
        assert!(!c.overlaps().is_empty(), "样例要有重叠");
        c
    }

    /// 面板真的列出了每一条重叠（标题带条数、条目文案来自 `Overlap::label`），
    /// 而且**没点就没跳转**（不然一打开就自己跳走）
    #[test]
    fn browser_lists_every_overlap_and_jumps_only_on_click() {
        let c = two_overlaps();
        let ov = c.overlaps().to_vec();
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(900.0, 300.0),
            )),
            ..Default::default()
        };
        // **跑两帧**：第一帧只是建立布局（滚动区还不知道自己多大，内容会被裁掉），
        // 第二帧才是用户看到的样子 —— 量 UI 的老规矩（见 `recents` 的稳定性测试）
        let mut res = (None, false);
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            res = conflicts_ui(ui, &ov);
        });
        out.textures_delta.clear();
        let mut out = ctx.run_ui(raw, |ui| {
            res = conflicts_ui(ui, &ov);
        });
        out.textures_delta.clear();
        let joined = texts(&out).join("\n");
        assert!(
            joined.contains(&format!("冲突浏览器（{} 处事件重叠）", ov.len())),
            "{joined}"
        );
        for o in &ov {
            assert!(joined.contains(&o.label()), "缺条目：{}", o.label());
        }
        assert_eq!(res.0, None, "没有点击不该产生跳转");
        assert!(!res.1, "没有点击关闭按钮");
    }

    /// 跳转目标映射：线/轨道/事件/拍都来自那条重叠记录
    #[test]
    fn jump_target_maps_the_overlap_fields() {
        let c = two_overlaps();
        let o = &c.overlaps()[0];
        let j = ConflictJump::of(o);
        assert_eq!(j.line_doc, o.line);
        assert_eq!(j.track, o.track);
        assert_eq!(j.event, o.next);
        assert_eq!(j.beat, o.start.to_f64());
        // 跳转目标必须是**文档下标**（视图序在列表排过序之后会不一样）
        assert_eq!(j.line_doc, 0);
    }

    /// 空列表不画面板（调用点已经用 `!conflicts.is_empty()` 挡了，这里钉住"画了也是空的"这条不变量）
    #[test]
    fn empty_list_draws_nothing_clickable() {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(900.0, 300.0),
            )),
            ..Default::default()
        };
        let mut res = (None, false);
        let mut out = ctx.run_ui(raw, |ui| {
            res = conflicts_ui(ui, &[]);
        });
        out.textures_delta.clear();
        assert_eq!(res, (None, false));
        assert!(texts(&out).join("\n").contains("0 处事件重叠"));
    }
}
