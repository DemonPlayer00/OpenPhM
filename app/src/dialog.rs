//! **模态对话框的统一外观**。
//!
//! 同一件事不该有三种长相：「新建谱面」原先在启动页里是**单独一屏**（整屏换掉），
//! 缺少 7z 又是另画一套（居中大标题 + 按钮），编辑页里还各写一份自己的弹窗 ——
//! 于是"提示是哪个灰、标题多高、Esc 关不关得掉"三处都不一样。
//!
//! 现在"长什么样、怎么关"只在这里定义，四处弹窗（文件 / 未保存守卫 / 新建谱面 / 缺少 7z）
//! 都从这里取：同一套颜色、同一族宽度、同一个标题与提示层级、同一种关法。
//!
//! 用 `egui::Modal`（自带遮罩、吞掉下层输入）而不是自绘浮层：模态语义由框架保证。
//!
//! **Esc 的归属**是这里最容易出错的一条：只有最上面那层模态能吃掉 Esc
//! （`ModalResponse::should_close` 内部用 `consume_key`），吃掉之后，下层那些
//! `key_pressed(Escape)`（启动页列表的"跳过"、编辑页的快捷键）就再也看不见它 ——
//! 这正是"弹窗开着时底下的界面不该响应 Esc"的实现方式，有单测钉住。

use egui::{Color32, Context, RichText, Ui};

/// 次要说明文字（小字、灰）——**唯一的灰**。
/// 原先 main.rs 与 recents.rs 各写一遍相近但不同的 RGB，改一处忘一处，两边的"灰"就不是同一个灰。
pub const HINT: Color32 = Color32::from_rgb(150, 160, 195);
/// 路径/等宽文本（淡蓝）
pub const PATH: Color32 = Color32::from_rgb(190, 205, 240);
/// 需要注意但不致命（橙）
pub const WARN: Color32 = Color32::from_rgb(255, 190, 110);
/// 成功
pub const OK: Color32 = Color32::from_rgb(150, 220, 150);
/// 失败
pub const ERR: Color32 = Color32::from_rgb(255, 120, 120);

/// 宽度只留三档：窄（确认/警告）、表单、宽（文件面板）。别在调用点上随手写数字 ——
/// 随手写出来的 480/560/660 就是"每个窗口一个样式"的来源。
pub const W_NARROW: f32 = 480.0;
pub const W_FORM: f32 = 560.0;
pub const W_WIDE: f32 = 660.0;

/// 弹窗跑完的结果：内容闭包返回了什么，以及"用户是不是把它关掉了"。
pub struct Outcome<T> {
    /// 内容闭包的返回值
    pub inner: T,
    /// Esc（最上层时）或点遮罩 ⇒ true
    pub dismissed: bool,
}

/// 画一个**可关闭**的模态：Esc 或点遮罩 ⇒ `dismissed = true`。
pub fn modal<T>(
    ctx: &Context,
    id: &str,
    width: f32,
    content: impl FnOnce(&mut Ui) -> T,
) -> Outcome<T> {
    show(ctx, id, width, true, content)
}

/// 画一个**关不掉**的模态（启动门槛：没装 7z 就没法交付 `.opm`，出口只能是明写的按钮）。
///
/// 它**仍然吃掉 Esc** —— 否则底下那层就会收到一个本该属于弹窗的 Esc（启动页的"跳过"会当场
/// 把用户送进编辑页，门槛就漏了）。`dismissed` 恒为 false。
pub fn sticky_modal<T>(
    ctx: &Context,
    id: &str,
    width: f32,
    content: impl FnOnce(&mut Ui) -> T,
) -> Outcome<T> {
    show(ctx, id, width, false, content)
}

fn show<T>(
    ctx: &Context,
    id: &str,
    width: f32,
    closable: bool,
    content: impl FnOnce(&mut Ui) -> T,
) -> Outcome<T> {
    let r = egui::Modal::new(egui::Id::new(id)).show(ctx, |ui| {
        // 宽度由**调用点**给（三档之一），内容不许反过来撑它 —— 上一次"列表越画越长"的反馈回路
        // 就是从"宽度由内容算"开始的
        ui.set_width(width);
        content(ui)
    });
    // 关不掉的弹窗也要调它：**消费掉 Esc** 才是"关不掉"的完整含义
    let close_requested = r.should_close();
    Outcome {
        inner: r.inner,
        dismissed: closable && close_requested,
    }
}

/// 弹窗第一行：标题 + 一段留白
pub fn title(ui: &mut Ui, text: &str) {
    ui.heading(text);
    ui.add_space(4.0);
}

/// 次要说明（小字、灰）
pub fn hint(ui: &mut Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(HINT));
}

/// 需要注意但不致命的一行（橙）
pub fn warn(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).color(WARN));
}

/// 路径（等宽 + 淡蓝）
pub fn path(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).monospace().color(PATH));
}

/// 操作结果：成功绿 / 失败红
pub fn message(ui: &mut Ui, ok: bool, text: &str) {
    ui.colored_label(if ok { OK } else { ERR }, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc() -> Vec<egui::Event> {
        vec![egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }]
    }

    fn screen() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(900.0, 600.0))
    }

    /// 可关闭的模态：Esc ⇒ `dismissed`，**而且这个 Esc 之后谁也看不见了**。
    ///
    /// 后半条是这一层的核心契约：启动页底下的列表用 `key_pressed(Escape)` 判"跳过"，
    /// 编辑页用它判快捷键；如果模态不消费掉 Esc，弹窗开着按 Esc 会同时触发两件事。
    #[test]
    fn escape_dismisses_a_closable_modal_and_is_consumed() {
        let ctx = egui::Context::default();
        let mut dismissed: Vec<bool> = Vec::new();
        let mut seen_after: Vec<bool> = Vec::new();
        for _ in 0..2 {
            let raw = egui::RawInput {
                screen_rect: Some(screen()),
                events: esc(),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                let o = modal(ui.ctx(), "t_closable", W_NARROW, |ui| {
                    ui.label("内容");
                });
                dismissed.push(o.dismissed);
                // 模态之后（也就是"下一层"）再看这个 Esc
                seen_after.push(ui.input(|i| i.key_pressed(egui::Key::Escape)));
            });
            out.textures_delta.clear();
        }
        assert!(dismissed[1], "Esc 应关掉可关闭的模态：{dismissed:?}");
        assert!(
            !seen_after[1],
            "Esc 已被模态消费，后面的代码不该再看见它：{seen_after:?}"
        );
    }

    /// 关不掉的模态：`dismissed` 永远是 false，但 Esc 照样被吃掉（否则门槛会漏）
    #[test]
    fn sticky_modal_never_dismisses_but_still_eats_escape() {
        let ctx = egui::Context::default();
        let mut dismissed: Vec<bool> = Vec::new();
        let mut seen_after: Vec<bool> = Vec::new();
        for _ in 0..2 {
            let raw = egui::RawInput {
                screen_rect: Some(screen()),
                events: esc(),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                let o = sticky_modal(ui.ctx(), "t_sticky", W_NARROW, |ui| {
                    ui.label("门槛");
                });
                dismissed.push(o.dismissed);
                seen_after.push(ui.input(|i| i.key_pressed(egui::Key::Escape)));
            });
            out.textures_delta.clear();
        }
        assert!(
            dismissed.iter().all(|d| !*d),
            "黏性模态不该被 Esc 关掉：{dismissed:?}"
        );
        assert!(
            !seen_after[1],
            "即使关不掉，Esc 也必须被它消费掉：{seen_after:?}"
        );
    }

    /// 内容闭包的返回值原样带出来（调用点靠它拿用户的决定）
    #[test]
    fn inner_value_is_passed_through() {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(screen()),
            ..Default::default()
        };
        let mut got = 0u32;
        let mut out = ctx.run_ui(raw, |ui| {
            let o = modal(ui.ctx(), "t_inner", W_FORM, |ui| {
                if ui.button("按我").clicked() {
                    7
                } else {
                    3
                }
            });
            got = o.inner;
        });
        out.textures_delta.clear();
        assert_eq!(got, 3, "没点击时应拿到闭包的默认返回值");
    }
}
