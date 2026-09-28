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

/// **数值字段的提交时机：只在回车 / 失焦**（用户要求）。
///
/// 单独抽成一个函数，是因为它是一条**规则**而不是"顺手补的一个 flag"：
/// `DragValue` 默认 `update_while_editing(true)` —— 每敲一个键就把值写回（并让调用方发命令），
/// 而检查器是**每次广播都重建**的 ⇒ 你敲 "12"，第一个字符就被快照里的旧值冲掉，
/// 字段变成 "1"、再敲变 "11" …… 数值输入根本没法用。
///
/// `update_while_editing(false)` 正是要的语义（egui 文档："值只在回车或取消选择时更新"，
/// Esc 取消）。**鼠标拖动仍然实时** —— 拖就是在调值，那是另一件事。
///
/// 一个数值字段这一帧的结果
pub struct ValueField {
    /// 底层响应（要矩形/焦点时用；**别用它的 `changed()` 当"该发命令了"**）。
    /// 生产代码只用 `changed`，`resp` 是给测试拿控件矩形用的。
    #[cfg_attr(not(test), allow(dead_code))]
    pub resp: egui::Response,
    /// **值真的变了没有** —— 该不该发命令只看它
    pub changed: bool,
}

/// 数值字段：**提交时机只有回车 / 失焦**（用户要求），返回"值真的变了没有"。
///
/// 每个字段都必须经过这里：以后谁再加一个字段，也不会悄悄回到"每敲一键提交一次"。
///
/// 判据为什么不是 `Response::changed()`（实测踩到的）：编辑态下 `DragValue` 返回的是**内部
/// TextEdit** 的响应，敲第一个字符那一帧它就是真，而值纹丝不动（要等回车/失焦才写回）。
/// 拿它当判据会在打字期间发一串**旧值**命令、把撤销栈灌满。用"值变了没有"则两种情形都对：
/// 打字期间不变（不发）、回车那一下变了（发一次）、鼠标拖动每帧都变（照旧实时）。
pub fn value_field<N: egui::emath::Numeric>(
    ui: &mut Ui,
    v: &mut N,
    prefix: &str,
    speed: f64,
    range: Option<std::ops::RangeInclusive<N>>,
) -> ValueField {
    let before = *v;
    let mut w = egui::DragValue::new(v)
        .update_while_editing(false)
        .prefix(prefix)
        .speed(speed);
    if let Some(r) = range {
        w = w.range(r);
    }
    let resp = ui.add(w);
    ValueField { resp, changed: *v != before }
}

/// **文本字段的提交时机：与数值同一套（回车 / 失焦）**。
///
/// 编辑期间那份文本放在 egui 的临时内存里，**不用每帧重建的快照去覆盖它** ——
/// 那正是"输入被自动填充（敲一个字就被旧值冲掉）"的来源。没在编辑时永远显示文档里的值，
/// 于是撤销/外部改动照常立刻反映出来。
///
/// 返回 `Some(新值)` 只在"刚刚提交且确实变了"的那一帧（调用方据此发命令）。
pub fn text_field(ui: &mut Ui, key: &str, current: &str, width: f32) -> Option<String> {
    let id = egui::Id::new(("opm_insp_text", key));
    let editing = ui.memory(|m| m.has_focus(id));
    let mut buf = if editing {
        ui.data(|d| d.get_temp::<String>(id)).unwrap_or_else(|| current.to_owned())
    } else {
        current.to_owned()
    };
    let resp = ui.add(
        egui::TextEdit::singleline(&mut buf)
            .id(id)
            .desired_width(width),
    );
    let value = buf.trim().to_owned();
    if resp.lost_focus() {
        // 回车、Tab、点到别处都会走到这里（Esc 取消编辑时 egui 已经回滚了文本）
        ui.data_mut(|d| d.remove::<String>(id));
        return (!value.is_empty() && value != current).then_some(value);
    }
    if editing {
        ui.data_mut(|d| d.insert_temp(id, buf));
    }
    None
}

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
                if let Some(name) = text_field(ui, "line-name", &v.name, 110.0) {
                    ec.push(opm_app::edit::set_line_command(
                        line_doc,
                        serde_json::json!({"name": name}),
                    ));
                }
            });
            let mut z = v.z_order;
            if value_field(ui, &mut z, "zOrder ", 0.2, None).changed
            {
                ec.push(opm_app::edit::set_line_command(line_doc, serde_json::json!({"zOrder": z})));
            }
            let mut cover = v.is_cover;
            if ui.checkbox(&mut cover, "isCover（遮挡音符）").changed() {
                ec.push(opm_app::edit::set_line_command(line_doc, serde_json::json!({"isCover": cover})));
            }
            let mut bf = v.bpm_factor;
            if value_field(ui, &mut bf, "线速 ", 0.01, Some(0.01..=8.0)).changed
            {
                ec.push(opm_app::edit::set_line_command(line_doc, serde_json::json!({"bpmFactor": bf})));
            }
            ui.monospace(format!("子音符 {}   事件 {} 条", v.notes, v.events));

            // ---- 事件（当前轨道选中项）：可编辑 ----
            if let Some(e) = &v.event_edit {
                ui.separator();
                ui.label(format!("事件 · {}", v.track.key()));
                let track_key = v.track.key();
                let idx = st.selected_event().unwrap_or(0);
                // 命令要的是**文档地址**（图层 + 该图层里的下标），不是合并视图下标：
                // 合并序号直接当图层下标用会改到另一条事件（见 `doc::EventRef`）。
                // 只有"视图下标已经越界"（等待下一帧的状态）才会退到这个兜底值 ——
                // 那时命令本来也会被核心以"越界"拒掉，不会改错东西。
                let at = st
                    .selected()
                    .map(|l| l.track(v.track))
                    .and_then(|tv| tv.origin(idx))
                    .unwrap_or(opm_app::doc::EventRef::new(0, idx));
                let mut changed = false;
                let mut sb = e.start_beat;
                let mut eb = e.end_beat;
                let mut sv = e.start_value;
                let mut ev = e.end_value;
                let mut easing = e.easing.clone();
                changed |= value_field(ui, &mut sb, "起 ", 0.05, Some(0.0..=1e6)).changed;
                changed |= value_field(ui, &mut eb, "止 ", 0.05, Some(0.0..=1e6)).changed;
                changed |= value_field(ui, &mut sv, "值起 ", 0.5, None).changed;
                changed |= value_field(ui, &mut ev, "值止 ", 0.5, None).changed;
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
                            &st, v.track, at, opm_app::state::EventEdge::Start, sb));
                    }
                    if (eb - e.end_beat).abs() > 1e-9 {
                        ec.push(opm_app::edit::event_resize_command(
                            &st, v.track, at, opm_app::state::EventEdge::End, eb));
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
                            line_doc, track_key, at, serde_json::Value::Object(set)));
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
                if value_field(ui, &mut sb, "拍 ", 0.05, Some(0.0..=1e6)).changed
                {
                    set.insert("startBeat".into(), serde_json::json!(st.beat_json(sb)));
                }
                if let Some(eb0) = n.end_beat {
                    let mut eb = eb0;
                    if value_field(ui, &mut eb, "止 ", 0.05, Some(0.0..=1e6)).changed
                    {
                        set.insert("endBeat".into(), serde_json::json!(st.beat_json(eb)));
                    }
                }
                let mut lane = n.lane_x;
                if value_field(ui, &mut lane, "laneX ", 1.0, Some(-675.0..=675.0)).changed
                {
                    set.insert("laneX".into(), serde_json::json!(st.snap_lane(lane)));
                }
                let mut alpha = n.alpha as i64;
                if value_field(ui, &mut alpha, "alpha ", 1.0, Some(0..=255)).changed
                {
                    set.insert("alpha".into(), serde_json::json!(alpha));
                }
                let mut fake = n.is_fake;
                if ui.checkbox(&mut fake, "假音符").changed() {
                    set.insert("isFake".into(), serde_json::json!(fake));
                }
                let mut sp = n.speed;
                if value_field(ui, &mut sp, "speed ", 0.01, Some(0.01..=20.0)).changed
                {
                    set.insert("speed".into(), serde_json::json!(sp));
                }
                let mut ws = n.width_scale;
                if value_field(ui, &mut ws, "宽度 ", 0.01, Some(0.01..=10.0)).changed
                {
                    set.insert("widthScale".into(), serde_json::json!(ws));
                }
                let mut yo = n.y_offset;
                if value_field(ui, &mut yo, "yOffset ", 0.5, None).changed
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
        st.select_note(0);
        st.selected_track = state::TrackId::MoveX;
        st.select_event(state::TrackId::Alpha, 0);
        let insp = view::inspector_of(&st, c.doc()).expect("有选中的线");
        (c, st, insp)
    }

    /// 面板真的把该画的东西画出来了（这条是"面板还在、没被改哑"的回归网）
    /// **数值字段只在回车/失焦时提交**（用户要求）。
    ///
    /// 这条必须真的驱动一遍控件：`DragValue` 默认"每敲一键就写回"，而检查器每次广播都重建
    /// ⇒ 打字期间被快照里的旧值冲掉，输入根本没法用。测试点进字段、敲三个字符、
    /// 断言期间**一次都没有提交**，回车才变成 123。
    ///
    /// 顺带钉住第二条（踩出来的）：提交判据**不能**用 `Response::changed()`
    /// —— 编辑态返回的是内部 TextEdit 的响应，敲第一个字符那一帧它为真、而值纹丝不动。
    /// 用"值变了没有"才对，所以这里同时断言两件事。
    #[test]
    fn value_fields_commit_only_on_enter() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(400.0, 120.0));
        let mut v: f64 = 1.0;
        let mut rect: Option<egui::Rect> = None;
        // (值, 真身报的"变了没有", Response 报的 changed)
        let mut commits: Vec<(f64, bool, bool)> = Vec::new();
        let run = |events: Vec<egui::Event>,
                   v: &mut f64,
                       rect: &mut Option<egui::Rect>,
                       log: &mut Vec<(f64, bool, bool)>| {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| {
                let r = value_field(ui, v, "拍 ", 0.05, Some(0.0..=1e6));
                *rect = Some(r.resp.rect);
                log.push((*v, r.changed, r.resp.changed()));
            });
            out.textures_delta.clear();
        };
        let click = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        // 帧 1：铺出来，拿到字段矩形
        run(vec![], &mut v, &mut rect, &mut commits);
        let c = rect.expect("字段矩形").center();
        // 点进去（进入编辑态）
        run(vec![egui::Event::PointerMoved(c), click(c, true)], &mut v, &mut rect, &mut commits);
        run(vec![click(c, false)], &mut v, &mut rect, &mut commits);
        // 敲三个字符：**一次都不该提交**
        for ch in ["1", "2", "3"] {
            run(vec![egui::Event::Text(ch.to_owned())], &mut v, &mut rect, &mut commits);
            assert_eq!(v, 1.0, "打字期间不该提交（敲到 {ch} 就变了）");
            assert!(!commits.last().unwrap().1, "打字期间不该报「变了」");
        }
        // 回车：提交
        run(
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }],
            &mut v,
            &mut rect,
            &mut commits,
        );
        assert_eq!(v, 123.0, "回车才提交（三个字符拼成 123）");
        assert!(commits.last().unwrap().1, "回车那一下要报「变了」");
        assert_eq!(
            commits.iter().filter(|(_, real, _)| *real).count(),
            1,
            "整段只有回车那一次提交"
        );
        // 而 `Response::changed()` 在打字期间就是真 —— 这就是"不能拿它当判据"的原因
        assert!(
            commits.iter().any(|(_, real, resp)| !*real && *resp),
            "应有「值没变但 Response 说变了」的帧（否则这条测试就白写了）"
        );
    }


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
