//! **右侧属性检查器**（线 → 轨道 → 事件 → 音符）。从 `main.rs` 搬出来，因为它有一个
//! 必须一眼看清的契约：**面板只产出命令，不碰文档**（文档数据只能由 `EditCore` 写）。
//!
//! 契约的由来（一次真 bug）：命令原先攒在局部的 `ec` 里，而**施加**它的那几行写在
//! "调试工作区"的分支里 —— 于是除了调试工作区，属性编辑器改了什么都不生效（命令被算出来又丢掉）。
//! 现在函数返回命令列表、**调用点无条件施加**；"什么时候才 dispatch"这种分支不存在了。

use egui::Ui;
use opm_app::cmd;
use opm_app::state::EditorState;
use opm_app::view::Inspector;

/// 画属性编辑器，返回**要发的命令**（空 = 这一帧没改动）。
///
/// 只读 `st`（网格吸附/选中项）与快照 `insp`；命令怎么拼在 `opm_app::edit`（有单测）。
pub fn inspector_ui(
    ui: &mut Ui,
    st: &EditorState,
    insp: Option<&Inspector>,
) -> Vec<serde_json::Value> {
    let mut ec: Vec<serde_json::Value> = Vec::new();
    match &insp {
        Some(v) => {
            // ---- 判定线（当前线）：可编辑 ----
            let line_doc = v.line_index;
            ui.horizontal(|ui| {
                ui.label("线名");
                let mut name = v.name.clone();
                if ui.add(egui::TextEdit::singleline(&mut name).desired_width(110.0)).changed()
                    && !name.is_empty()
                {
                    ec.push(opm_app::edit::set_line_command(line_doc, serde_json::json!({"name": name})));
                }
            });
            let mut z = v.z_order;
            if ui
                .add(egui::DragValue::new(&mut z).prefix("zOrder ").speed(0.2))
                .changed()
            {
                ec.push(opm_app::edit::set_line_command(line_doc, serde_json::json!({"zOrder": z})));
            }
            let mut cover = v.is_cover;
            if ui.checkbox(&mut cover, "isCover（遮挡音符）").changed() {
                ec.push(opm_app::edit::set_line_command(line_doc, serde_json::json!({"isCover": cover})));
            }
            let mut bf = v.bpm_factor;
            if ui
                .add(egui::DragValue::new(&mut bf).prefix("线速 ").speed(0.01).range(0.01..=8.0))
                .changed()
            {
                ec.push(opm_app::edit::set_line_command(line_doc, serde_json::json!({"bpmFactor": bf})));
            }
            ui.monospace(format!("子音符 {}   事件 {} 条", v.notes, v.events));

            // ---- 事件（当前轨道选中项）：可编辑 ----
            if let Some(e) = &v.event_edit {
                ui.separator();
                ui.label(format!("事件 · {}", v.track.key()));
                let track_key = v.track.key();
                let idx = st.selected_event.unwrap_or(0);
                let mut changed = false;
                let mut sb = e.start_beat;
                let mut eb = e.end_beat;
                let mut sv = e.start_value;
                let mut ev = e.end_value;
                let mut easing = e.easing.clone();
                changed |= ui
                    .add(egui::DragValue::new(&mut sb).prefix("起 ").speed(0.05).range(0.0..=1e6))
                    .changed();
                changed |= ui
                    .add(egui::DragValue::new(&mut eb).prefix("止 ").speed(0.05).range(0.0..=1e6))
                    .changed();
                changed |= ui
                    .add(egui::DragValue::new(&mut sv).prefix("值起 ").speed(0.5))
                    .changed();
                changed |= ui
                    .add(egui::DragValue::new(&mut ev).prefix("值止 ").speed(0.5))
                    .changed();
                egui::ComboBox::from_id_salt("ev_easing")
                    .selected_text(easing.clone())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for name in cmd::EASINGS {
                            if ui.selectable_value(&mut easing, name.to_owned(), name).clicked() {
                                changed = true;
                            }
                        }
                    });
                if changed {
                    // 头尾改时间走 resize_event（**只改这一个事件**；它早先会同步邻块，
                    // 用户明确否掉了那个语义，见 core.rs 的 `resize_event` 注释）；
                    // 值/缓动走 set_event
                    if (sb - e.start_beat).abs() > 1e-9 {
                        ec.push(opm_app::edit::event_resize_command(
                            &st, idx, opm_app::state::EventEdge::Start, sb));
                    }
                    if (eb - e.end_beat).abs() > 1e-9 {
                        ec.push(opm_app::edit::event_resize_command(
                            &st, idx, opm_app::state::EventEdge::End, eb));
                    }
                    let mut set = serde_json::Map::new();
                    if (sv - e.start_value).abs() > 1e-9 {
                        set.insert("startValue".into(), serde_json::json!(sv));
                    }
                    if (ev - e.end_value).abs() > 1e-9 {
                        set.insert("endValue".into(), serde_json::json!(ev));
                    }
                    if easing != e.easing {
                        set.insert("easing".into(), serde_json::json!(easing));
                    }
                    if !set.is_empty() {
                        ec.push(opm_app::edit::set_event_command(
                            line_doc, track_key, idx, serde_json::Value::Object(set)));
                    }
                }
            }

            // ---- 音符（当前线选中项）：可编辑 ----
            if let Some(n) = &v.note_edit {
                ui.separator();
                ui.label(format!("音符 doc#{}", v.note.as_ref().map(|x| x.doc_index).unwrap_or(0)));
                let idx = v.note.as_ref().map(|x| x.doc_index).unwrap_or(0);
                let mut set = serde_json::Map::new();
                let mut kind = n.kind.clone();
                egui::ComboBox::from_id_salt("note_kind")
                    .selected_text(kind.clone())
                    .width(80.0)
                    .show_ui(ui, |ui| {
                        for k in ["tap", "hold", "drag", "flick"] {
                            ui.selectable_value(&mut kind, k.to_owned(), k);
                        }
                    });
                if kind != n.kind {
                    set.insert("kind".into(), serde_json::json!(kind));
                }
                let mut sb = n.start_beat;
                if ui
                    .add(egui::DragValue::new(&mut sb).prefix("拍 ").speed(0.05).range(0.0..=1e6))
                    .changed()
                {
                    set.insert("startBeat".into(), serde_json::json!(st.beat_json(sb)));
                }
                if let Some(eb0) = n.end_beat {
                    let mut eb = eb0;
                    if ui
                        .add(egui::DragValue::new(&mut eb).prefix("止 ").speed(0.05).range(0.0..=1e6))
                        .changed()
                    {
                        set.insert("endBeat".into(), serde_json::json!(st.beat_json(eb)));
                    }
                }
                let mut lane = n.lane_x;
                if ui
                    .add(egui::DragValue::new(&mut lane).prefix("laneX ").speed(1.0).range(-675.0..=675.0))
                    .changed()
                {
                    set.insert("laneX".into(), serde_json::json!(st.snap_lane(lane)));
                }
                let mut alpha = n.alpha as i64;
                if ui
                    .add(egui::DragValue::new(&mut alpha).prefix("alpha ").speed(1.0).range(0..=255))
                    .changed()
                {
                    set.insert("alpha".into(), serde_json::json!(alpha));
                }
                let mut fake = n.is_fake;
                if ui.checkbox(&mut fake, "假音符").changed() {
                    set.insert("isFake".into(), serde_json::json!(fake));
                }
                let mut sp = n.speed;
                if ui
                    .add(egui::DragValue::new(&mut sp).prefix("speed ").speed(0.01).range(0.01..=20.0))
                    .changed()
                {
                    set.insert("speed".into(), serde_json::json!(sp));
                }
                let mut ws = n.width_scale;
                if ui
                    .add(egui::DragValue::new(&mut ws).prefix("宽度 ").speed(0.01).range(0.01..=10.0))
                    .changed()
                {
                    set.insert("widthScale".into(), serde_json::json!(ws));
                }
                let mut yo = n.y_offset;
                if ui
                    .add(egui::DragValue::new(&mut yo).prefix("yOffset ").speed(0.5))
                    .changed()
                {
                    set.insert("yOffset".into(), serde_json::json!(yo));
                }
                if !set.is_empty() {
                    ec.push(opm_app::edit::set_note_command(
                        line_doc, idx, serde_json::Value::Object(set)));
                }
            }
            ui.separator();
            ui.label("此刻表演（事件求值）");
            ui.monospace(format!("moveX  {:>8.2}", v.perf.x));
            ui.monospace(format!("moveY  {:>8.2}", v.perf.y));
            ui.monospace(format!("rotate {:>8.2}°", v.perf.rotate_deg));
            ui.monospace(format!("alpha  {:>8.3}", v.perf.alpha));
            ui.monospace(format!("speed  {:>8.2}", v.perf.speed));
            ui.separator();
            ui.label(format!(
                "轨道 {} · {} 条",
                v.track.key(),
                v.track_events
            ));
            ui.monospace(format!(
                "值     {}",
                v.track_value.map(|x| format!("{x:.3}")).unwrap_or_else(|| "—".into())
            ));
            match &v.event {
                Some(e) => {
                    ui.monospace(format!("起     {:.3} 拍 / {:.3} s", e.start_beat, e.start_sec));
                    ui.monospace(format!("止     {:.3} 拍 / {:.3} s", e.end_beat, e.end_sec));
                    ui.monospace(format!("{} → {}", e.start_value, e.end_value));
                    ui.monospace(format!("缓动   {}", e.easing));
                }
                None => {
                    ui.label("（在事件列表里点一条）");
                }
            }
            if let Some(n) = &v.note {
                ui.separator();
                ui.label("选中音符（子对象）");
                ui.monospace(format!("doc#{:<6} {}", n.doc_index, n.kind));
                ui.monospace(format!("时刻   {:.4} s", n.time));
                ui.monospace(format!("结束   {:.4} s", n.end));
                ui.monospace(format!("lane_x {:.2}（线本地）", n.lane_x));
                ui.monospace(format!("屏幕   ({:.1}, {:.1})", n.screen[0], n.screen[1]));
            }
        }
        None => {
            ui.label("（没有判定线）");
        }
    }
    ec
}

#[cfg(test)]
mod tests {
    use super::*;
    use opm_app::core::EditCore;
    use opm_app::state;
    use opm_app::view;
    use serde_json::json;

    /// 一帧里画出来的所有文本（用于断言"这一栏真的画了什么"）
    fn drawn_texts(out: &egui::FullOutput) -> Vec<String> {
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

    /// 选中一条线 + 一个事件 + 一个音符的检查器快照
    fn sample() -> (EditCore, EditorState, Inspector) {
        let mut c = EditCore::new();
        for cmd in [
            json!({"op":"add_note","line":0,"kind":"tap","startBeat":[1,1],"laneX":100.0}),
            json!({"op":"add_event","line":0,"layer":0,"track":"moveX",
                   "startBeat":[0,1],"endBeat":[4,1],"startValue":0.0,"endValue":100.0}),
        ] {
            let r = c.exec(&cmd);
            assert_eq!(r["ok"], json!(true), "{cmd} → {r}");
        }
        let mut st = EditorState::new(state::chart_from_doc(c.doc()));
        st.selected_line = 0;
        st.selected_note = Some(0);
        st.selected_track = state::TrackId::MoveX;
        st.selected_event = Some(0);
        let insp = view::inspector_of(&st, c.doc()).expect("有选中的线");
        (c, st, insp)
    }

    /// 面板真的把该画的东西画出来了（这条是"面板还在、没被改哑"的回归网）
    #[test]
    fn inspector_draws_the_line_track_event_and_note() {
        let (_c, st, insp) = sample();
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(320.0, 900.0),
            )),
            ..Default::default()
        };
        let mut cmds = Vec::new();
        let mut out = ctx.run_ui(raw, |ui| {
            cmds = inspector_ui(ui, &st, Some(&insp));
        });
        out.textures_delta.clear();
        let texts = drawn_texts(&out);
        let joined = texts.join("\n");
        // 注意："属性编辑器" 这个标题留在调用点（`main.rs` 的面板包装里），不在本函数里
        for want in [
            "线名",
            "isCover（遮挡音符）",
            "事件 · moveX",
            "音符 doc#0",
            "此刻表演（事件求值）",
        ] {
            assert!(joined.contains(want), "没画出来 {want:?}；实际画了：\n{joined}");
        }
        // tap 不该出现 hold 才有的结束拍输入框（`note_edit.end_beat` 为 None ⇒ 不画）
        assert!(insp.note_edit.as_ref().unwrap().end_beat.is_none());
        assert!(
            !joined.contains("结束拍"),
            "tap 不该画 hold 的结束拍字段：\n{joined}"
        );
        // **没碰任何控件 ⇒ 一条命令都不发**（面板每帧乱发命令会让撤销栈爆炸）
        assert!(cmds.is_empty(), "没有交互却产出了命令：{cmds:?}");
    }

    /// 没有选中判定线时画的是"（没有判定线）"，而不是空白（用户要知道为什么右边是空的）
    #[test]
    fn inspector_says_so_when_nothing_is_selected() {
        let ctx = egui::Context::default();
        let st = {
            let c = EditCore::new();
            EditorState::new(state::chart_from_doc(c.doc()))
        };
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(320.0, 900.0),
            )),
            ..Default::default()
        };
        let mut cmds = Vec::new();
        let mut out = ctx.run_ui(raw, |ui| {
            cmds = inspector_ui(ui, &st, None);
        });
        out.textures_delta.clear();
        let joined = drawn_texts(&out).join("\n");
        assert!(joined.contains("（没有判定线）"), "{joined}");
        assert!(cmds.is_empty());
    }
}
