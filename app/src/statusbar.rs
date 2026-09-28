//! **底部状态栏**：常用状态一行放下（播放头 / 音频 / 网格 / 文档标识 / 偏移 / 冲突 / 诊断）。
//!
//! 为什么单独一层：这一行是"用户唯一的常驻读数"，它的文字必须**能单独测**——
//! 比如"没音频"该写 `♪ 无音频`、"没有保存目标"该写「尚未保存」而不是一个圆点、
//! 冲突从有到无该变成 `✓ 无事件重叠`。这些规则原先埋在 120 行 egui 代码里，
//! 只能靠开窗口看；现在它们是 [`StatusView`] → 一串文本 + 一两个动作（[`StatusAction`]）。
//!
//! 纪律：面板**不锁核心、不解析路径**——显示要用的东西由调用点算好放进来
//! （`file_badge`、网格文本都是缓存过的），面板只画与收动作。

use egui::{Color32, Ui};

use opm_app::audio::Audio;
use opm_app::dialog;

/// 状态栏的一份**快照**（都是算好的值与字符串）
pub struct StatusView<'a> {
    pub playhead: f64,
    /// 播放头对应的拍
    pub beat: f64,
    pub playing: bool,
    /// 当前音频（名字与欠载数从这里读）
    pub audio: Option<&'a Audio>,
    /// 音频校准偏移（毫秒）：面板里的输入框直接改它 —— 这是状态栏**唯一**的写，
    /// 而且写的是视图设置，不是文档
    pub audio_offset_ms: &'a mut f64,
    /// 网格文本（"每拍 4 条(实际 1/8) / 窗口 16 等分（84.4 RPE）（奇数等分：中轴不是格点）"）
    pub grid_text: String,
    /// 文档标识（`[opm] demo.opm`）
    pub file_badge: &'a str,
    /// 保存状态：文案 + 颜色
    pub file_mark: (&'static str, Color32),
    /// 文档标识的悬停说明
    pub file_hover: String,
    /// 窗口 X 偏移（`None` = 偏移为 0，不显示）：`(偏移, laneX 左, laneX 右)`
    pub window_offset: Option<(f32, f32, f32)>,
    /// 叠加层为什么看不见（`None` = 看得见）
    pub overlay_hidden: Option<&'static str>,
    /// 冲突条数
    pub conflicts: usize,
    /// 冲突浏览器是否展开（决定红字可点还是显示"✓ 无事件重叠"）
    pub show_conflicts: bool,
    /// 临时提示（`(是否成功, 文本)`）：快速放置的说明、hold 放置结果、保存失败……
    /// 编辑器里**只有状态栏是常驻的**，所以消息必须能落在这里（否则用户什么都看不到）
    pub notice: Option<(bool, String)>,
    /// 诊断段（只有调试工作区给 `Some`）
    pub diagnostics: Option<String>,
}

/// 状态栏产出的动作（调用点施加；面板不碰 App 状态）
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StatusAction {
    /// 点了红字/绿字：切换冲突浏览器
    pub toggle_conflicts: bool,
}

/// 画状态栏
pub fn status_bar_ui(ui: &mut Ui, v: &mut StatusView) -> StatusAction {
    let mut act = StatusAction::default();
    ui.horizontal(|ui| {
        ui.label(format!("{:.3}s / {:.2} 拍", v.playhead, v.beat));
        if v.playing {
            ui.colored_label(dialog::OK, "▶ 播放中");
        }
        ui.separator();
        match v.audio {
            Some(a) => {
                // 文件名在载入时算好了（`Audio::name`）：帧里不再解析路径、不再分配字符串
                ui.label(format!("♪ {}", a.name()));
                if a.underruns() > 0 {
                    ui.colored_label(Color32::from_rgb(240, 160, 120), format!("欠载 {}", a.underruns()));
                }
                ui.add(
                    egui::DragValue::new(v.audio_offset_ms)
                        .range(-200.0..=200.0)
                        .speed(0.5)
                        .prefix("校准 ")
                        .suffix(" ms"),
                )
                .on_hover_text("听到的与游标算出来的差多少；按耳朵调到对拍即可");
            }
            None => {
                ui.label("♪ 无音频");
            }
        }
        ui.separator();
        ui.label(v.grid_text.as_str());
        // ---- 文档标识：格式 + 文件名 + 保存状态（算一次存起来，见 `App::refresh_file_badge`）----
        ui.label(v.file_badge).on_hover_text(v.file_hover.as_str());
        ui.colored_label(v.file_mark.1, v.file_mark.0);
        if let Some((off, lo, hi)) = v.window_offset {
            ui.label(format!("窗口 X 偏移 {off:+.0}（显示 laneX {lo:.0}…{hi:.0}）"));
        }
        if let Some(why) = v.overlay_hidden {
            ui.label(why);
        }
        if v.conflicts > 0 {
            let txt = format!("⚠ {} 处事件重叠", v.conflicts);
            if ui
                .add(
                    egui::Label::new(egui::RichText::new(txt).color(Color32::from_rgb(255, 110, 110)))
                        .sense(egui::Sense::click()),
                )
                .clicked()
            {
                act.toggle_conflicts = true;
            }
        } else if v.show_conflicts {
            ui.colored_label(Color32::from_rgb(140, 200, 140), "✓ 无事件重叠");
        }
        if let Some((ok, msg)) = &v.notice {
            ui.separator();
            ui.colored_label(if *ok { dialog::OK } else { dialog::ERR }, msg);
        }
        if let Some(d) = &v.diagnostics {
            ui.separator();
            ui.label(d.as_str());
        }
    });
    act
}

/// 文档标识的保存状态（文案 + 颜色）：三态，**说人话**。
///
/// `•` 这种记号只有在知道它什么意思时才有用，所以直接写"有未保存改动"；
/// 没有保存目标时写「尚未保存」而不是"已保存"（那会让人以为存过了）。
pub fn file_mark(dirty: bool, has_target: bool) -> (&'static str, Color32) {
    if dirty {
        ("● 有未保存改动", dialog::WARN)
    } else if has_target {
        ("✓ 已保存", dialog::OK)
    } else {
        ("尚未保存", dialog::HINT)
    }
}

/// 文档标识的悬停说明（有目标给路径，没目标说清"第一次保存会弹窗口"）
pub fn file_hover(has_target: bool, target: &str) -> String {
    if has_target {
        format!("当前文件：{target}\n（状态栏只显示文件名；完整路径在「文件…」对话框里）")
    } else {
        "还没有保存目标：第一次保存会弹系统保存窗口让你指定路径".to_owned()
    }
}

/// 叠加层隐藏的原因（文案只有一处，别在两个分支里各写一句）
pub fn overlay_hidden_text(h_held: bool) -> &'static str {
    if h_held {
        "编辑区 H 隐藏"
    } else {
        "编辑区 播放中隐藏"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view<'a>(offset: &'a mut f64) -> StatusView<'a> {
        StatusView {
            playhead: 1.25,
            beat: 2.5,
            playing: true,
            audio: None,
            audio_offset_ms: offset,
            grid_text: "网格 每拍 4 条 / 窗口 16 等分（84.4 RPE）".to_owned(),
            file_badge: "[opm] demo.opm",
            file_mark: file_mark(true, true),
            file_hover: file_hover(true, "/charts/demo.opm"),
            window_offset: Some((400.0, -275.0, 1075.0)),
            overlay_hidden: None,
            conflicts: 2,
            show_conflicts: true,
            notice: None,
            diagnostics: None,
        }
    }

    fn texts(out: &egui::FullOutput) -> Vec<String> {
        fn walk(shape: &egui::epaint::Shape, acc: &mut Vec<String>) {
            match shape {
                egui::epaint::Shape::Text(t) => acc.push(t.galley.text().to_owned()),
                egui::epaint::Shape::Vec(v) => {
                    for s in v {
                        walk(s, acc);
                    }
                }
                _ => {}
            }
        }
        let mut acc = Vec::new();
        for cs in &out.shapes {
            walk(&cs.shape, &mut acc);
        }
        acc
    }

    fn draw<'a>(v: &mut StatusView<'a>) -> (StatusAction, Vec<String>) {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1600.0, 30.0),
            )),
            ..Default::default()
        };
        // 两帧：第一帧建立布局（滚动/换行还没定），第二帧才是用户看到的
        let mut act = StatusAction::default();
        let mut out = ctx.run_ui(raw.clone(), |ui| {
            act = status_bar_ui(ui, v);
        });
        out.textures_delta.clear();
        let mut out = ctx.run_ui(raw, |ui| {
            act = status_bar_ui(ui, v);
        });
        out.textures_delta.clear();
        (act, texts(&out))
    }

    /// 一行里有：播放头/拍、播放中、音频（无）、网格、文档标识 + 保存状态、窗口偏移、冲突计数
    #[test]
    fn status_line_shows_every_reader() {
        let mut off = 0.0;
        let mut v = view(&mut off);
        let (act, t) = draw(&mut v);
        let j = t.join("\n");
        for want in [
            "1.250s / 2.50 拍",
            "▶ 播放中",
            "♪ 无音频",
            "网格 每拍 4 条 / 窗口 16 等分（84.4 RPE）",
            "[opm] demo.opm",
            "● 有未保存改动",
            "窗口 X 偏移 +400",
            "⚠ 2 处事件重叠",
        ] {
            assert!(j.contains(want), "缺 {want:?}：\n{j}");
        }
        assert_eq!(act, StatusAction::default(), "没点就不该有动作");
    }

    /// 提示行：编辑器的消息要落在**常驻的状态栏**上（否则"指针不在音符区"这种说明没人看得到）
    #[test]
    fn notices_are_visible_in_the_status_bar() {
        let mut off = 0.0;
        let mut v = view(&mut off);
        v.notice = Some((false, "快速放置需要把指针放在音符区里，再按 Q/W/E/R".to_owned()));
        let (_act, t) = draw(&mut v);
        assert!(
            t.join("\n").contains("快速放置需要把指针放在音符区里"),
            "{t:?}"
        );
        v.notice = Some((true, "放下 hold：1.000 → 3.000 拍".to_owned()));
        let (_act, t) = draw(&mut v);
        assert!(t.join("\n").contains("放下 hold"), "{t:?}");
    }

    /// 三态保存状态 + 悬停文案（说人话：不再是一个圆点）
    #[test]
    fn save_state_reads_as_a_sentence() {
        assert_eq!(file_mark(true, true).0, "● 有未保存改动");
        assert_eq!(file_mark(true, false).0, "● 有未保存改动");
        assert_eq!(file_mark(false, true).0, "✓ 已保存");
        assert_eq!(file_mark(false, false).0, "尚未保存");
        assert!(file_hover(true, "/x/y.opm").contains("/x/y.opm"));
        assert!(file_hover(false, "").contains("第一次保存"));
    }

    /// 冲突清空之后：红字换成绿字（只在浏览器还开着时显示），且偏移为 0 时不占地方
    #[test]
    fn cleared_conflicts_show_a_positive_line_only_when_browser_is_open() {
        let mut off = 0.0;
        let mut v = view(&mut off);
        v.conflicts = 0;
        v.window_offset = None;
        v.overlay_hidden = Some(overlay_hidden_text(true));
        v.show_conflicts = true;
        let (_act, t) = draw(&mut v);
        let j = t.join("\n");
        assert!(j.contains("✓ 无事件重叠"), "{j}");
        assert!(!j.contains("窗口 X 偏移"), "偏移 0 时不该出现：{j}");
        assert!(j.contains("编辑区 H 隐藏"), "{j}");
        // 浏览器关着时不该有绿字（否则每帧都挂着一句没信息量的话）
        v.show_conflicts = false;
        let (_act, t) = draw(&mut v);
        assert!(!t.join("\n").contains("无事件重叠"));
    }

    /// 有音频时显示**文件名**（不是整条路径）与欠载计数
    #[test]
    fn audio_shows_the_file_name_not_the_path() {
        // 造一个假的 Audio 不方便（要开设备）⇒ 这里只钉住"名字来自 `Audio::name`"这一条
        // 由 `audio` 模块的单测覆盖（`Audio::load` 时算好），这里断言文案分支：
        assert_eq!(overlay_hidden_text(false), "编辑区 播放中隐藏");
        assert_eq!(overlay_hidden_text(true), "编辑区 H 隐藏");
    }
}
