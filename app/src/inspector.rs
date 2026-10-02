// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
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

/// **拍的三元组编辑器**：`【整拍数】 + 【分子】 / 【分母】`（用户口径，与 RPE/pez 的存储形状一致）。
///
/// 为什么不是一个浮点框：opm 的拍是**精确有理数**，而 `1/3` 用 f64 存下来是
/// `0.3333333333333333` —— 写回 pez 要么变成 `[0,333333,1000000]`（浮点噪声进文件），
/// 要么被网格吸附到别的位置（用户编 1/3，落到 1/4）。三个整数控件没有这一步：
/// 编出来的就是 `Beat::new(整拍 × 分母 + 分子, 分母)`，导出时按 `[整拍, 分子, 分母]` 原样落盘。
///
/// 显示的是**规范形**：`Beat` 已经约分，整数部分取 `div_euclid`、分子取 `rem_euclid`
/// （所以 `-1/2` 显示成 `-1 + 1/2`，与 `codec::beat_to_triple` 写出去的三元组一致）。
///
/// 提交时机与 [`value_field`] 同一套（**回车/失焦**才写回，拖动实时）；判据是
/// "三元组真的变了没有"，不用 `Response::changed()`（理由见 `value_field` 的注释）。
pub fn beat_triple_field(
    ui: &mut Ui,
    key: &str,
    label: &str,
    beat: opm_app::doc::Beat,
) -> Option<opm_app::doc::Beat> {
    let (whole, num, den) = (beat.n.div_euclid(beat.d), beat.n.rem_euclid(beat.d), beat.d);
    let (mut w, mut n, mut d) = (whole, num, den);
    ui.push_id(("opm_insp_beat", key), |ui| {
        ui.horizontal(|ui| {
            ui.label(label);
            ui.add(
                egui::DragValue::new(&mut w)
                    .update_while_editing(false)
                    .speed(1.0)
                    .range(-1_000_000..=1_000_000),
            );
            ui.label("+");
            ui.add(
                egui::DragValue::new(&mut n)
                    .update_while_editing(false)
                    .speed(1.0)
                    .range(0..=1_000_000),
            );
            ui.label("/");
            ui.add(
                egui::DragValue::new(&mut d)
                    .update_while_editing(false)
                    .speed(1.0)
                    .range(1..=1_000_000),
            );
        })
        .response
        .on_hover_text(format!(
            "拍 = 整拍数 + 分子/分母（现在是 {whole} + {num}/{den} = {:.4} 拍）\n\
             三个整数分别编辑 —— 1/3 这类拍因此是精确的，导出到 RPE 就是 [整拍, 分子, 分母]",
            beat.to_f64()
        ));
    });
    if (w, n, d) != (whole, num, den) {
        Some(opm_app::doc::Beat::new(w * d + n, d))
    } else {
        None
    }
}

/// 属性编辑器这一帧的产物。
///
/// 两条通道分开，是因为**文档**与**选区**是两回事：
/// · `commands`：要发的文档命令（面板只产出命令，施加由调用方做）；
/// · `select_note`：**视图**动作 —— 在重叠组列表里点了某个音符，把选区换成它。
///   选中不属于文档，所以它不走命令通道（与编辑区里点一下音符是同一条路）。
#[derive(Default)]
pub struct InspectorOut {
    pub commands: Vec<serde_json::Value>,
    pub select_note: Option<usize>,
    /// **视图**动作：在通道列表里点了某一条 ⇒ 把编辑区的"当前列"切过去
    /// （与 `select_note` 同一条理由：选中不是文档数据，不走命令通道）
    pub select_channel: Option<opm_app::state::MaskChannel>,
    /// `commands` 里**有没有遮蔽区面板发的命令**。
    ///
    /// 为什么要这一位：遮蔽区一块都还没有时（草稿态）那些命令要先建区再应用，
    /// 而那段包装只有调用方做得到（见 `main.rs` 的 `mask_commands_many`）——
    /// 让面板自己塞 `add_zone` 会把"顺序与事务"这件调用方的事漏进面板。
    pub mask_edits: bool,
}

/// 画属性编辑器，返回这一帧的产物（见 [`InspectorOut`]）。
///
/// 只读 `st`（网格吸附/选中项）与快照 `insp`；命令怎么拼在 `opm_app::edit`（有单测）。
pub fn inspector_ui(
    ui: &mut Ui,
    st: &EditorState,
    insp: Option<&mut Inspector>,
) -> InspectorOut {
    let mut out = InspectorOut::default();
    let ec = &mut out.commands;
    // ---- 遮蔽区编辑模式：右栏整片让给它 ----
    //
    // 放在 `match insp` **之前**：这个模式下面板要的是"当前遮蔽区"，而 `Inspector` 快照是
    // 按**判定线**取的（没有判定线的谱面上它会是 `None`）—— 遮蔽区与判定线本来就没有关系。
    if st.mask_edit {
        mask_panel(ui, st, &mut out);
        return out;
    }
    match insp {
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

            // ---- 属性编辑器**分两半**：选中哪一类就只显示哪一半（用户口径）----
            //
            // 以前两边同时显示：锚是**跨类型保留**的（点了音符，事件锚还在），于是
            // "事件 · moveX" 和 "音符 doc#N" 一起占着右栏、改错栏是常事。
            // 现在只看**选区类型**（`SelKind`）—— 选区本身就"同时只有一类"（音符 xor 事件），
            // 所以这两个分支天然互斥；锚仍然保留，切回去时还是那一条。
            let sel = st.sel_kind();
            let show_event = sel == Some(opm_app::state::SelKind::Events);
            let show_note = sel == Some(opm_app::state::SelKind::Notes);

            // ---- 事件（当前轨道选中项）：可编辑 ----
            if let Some(e) = v.event_edit.as_ref().filter(|_| show_event) {
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
                let mut sv = e.start_value;
                let mut ev = e.end_value;
                let mut easing = e.easing.clone();
                // ---- 头/尾：**【整拍数】 + 【分子】 / 【分母】**（三个整数，不经过浮点）----
                // 走 `event_resize_command_exact`：不发吸附过的浮点拍，直接发三元组。
                for (key, label, edge, was) in [
                    ("ev_start", "起 ", opm_app::state::EventEdge::Start, e.start_exact),
                    ("ev_end", "止 ", opm_app::state::EventEdge::End, e.end_exact),
                ] {
                    if let Some(b) = beat_triple_field(ui, key, label, was) {
                        if b.n != was.n || b.d != was.d {
                            ec.push(opm_app::edit::event_resize_command_exact(
                                st, v.track, at, edge, b,
                            ));
                        }
                    }
                }
                changed |= value_field(ui, &mut sv, "值起 ", 0.5, None).changed;
                changed |= value_field(ui, &mut ev, "值止 ", 0.5, None).changed;
                // ---- 缓动选择：**五条轨道都一样**（流速也认缓动了 —— 缓动按"折线"实现，
                // 0.1 秒一段；回弹类还会把回弹点/折点插进节点，见 `perf::event_knots`）----
                if let Some((curve, variant)) = cmd::split_easing(&easing) {
                    let mut new_curve = curve;
                    egui::ComboBox::from_id_salt("ev_easing_curve")
                        .selected_text(curve.label())
                        .width(104.0)
                        .show_ui(ui, |ui| {
                            for c in cmd::EaseCurve::ALL {
                                ui.selectable_value(&mut new_curve, c, c.label());
                            }
                        });
                    // 变体：线性没有（置灰而不是藏起来 —— 布局不跳，"为什么没得选"也看得出来）
                    let mut new_variant = variant.unwrap_or(cmd::EaseVariant::Out);
                    let avail = new_curve.variants();
                    // 换了曲线之后旧变体可能不存在（`quint`/`expo` 没有 io）⇒ 先夹回去
                    new_variant = new_curve.clamp_variant(new_variant);
                    ui.add_enabled_ui(!avail.is_empty(), |ui| {
                        egui::ComboBox::from_id_salt("ev_easing_variant")
                            .selected_text(new_variant.label())
                            .width(52.0)
                            .show_ui(ui, |ui| {
                                for v in avail {
                                    ui.selectable_value(&mut new_variant, *v, v.label());
                                }
                            });
                    });
                    let want = cmd::easing_name(new_curve, new_variant);
                    if want != easing {
                        easing = want.to_owned();
                        changed = true;
                    }
                } else {
                    // 认不出的缓动：**原样显示、不动它**（别拿一个猜的名字把用户的文件改了）
                    ui.monospace(format!("缓动 {easing}（无法识别，已原样保留）"));
                }
                if changed {
                    // 头尾改时间走 resize_event（**只改这一个事件**；它早先会同步邻块，
                    // 用户明确否掉了那个语义，见 core.rs 的 `resize_event` 注释）——
                    // 但**在控件那里就发出去了**（三元组控件用精确有理拍，不经过这里的浮点比较）；
                    // 值/缓动走 set_event
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

                // ---- 就位目标（块末）：一次给全线的坐标，保证 0 误差 ----
                //
                // 用户口径：「事件块结束点上，给一个单次事件目标设置（x/y 坐标，透明度，角度），
                // 给一次事件块末尾的值，以保证最终 0 误差就位」。
                //
                // 与上面那四行"值起/值止"的区别：那四行改的是**这条轨道这一个端值**；
                // 这一组要的是"**块末那一刻，线该在哪儿**"—— 四个数一次给，四条轨道一起写，
                // 而且求值器在端点是**按定义取端值**（`perf::endpoint_value`），所以块末的值
                // 与这里写下的数**按位相等**（不是"误差小于 1e-9"）。
                //
                // 只把**真的变了**的那些键放进命令：没动的轨道连碰都不碰（核心也会跳过，
                // 但命令里少一个键就少一次"要不要切它一刀"的犹豫）。
                let anchor = opm_app::codec::beat_from_f64(e.end_beat);
                let sec = st.chart.tmap.sec(e.end_beat);
                // 取**选中那条线**（视图侧），不是 `lines[doc_index]` —— 视图下标与文档下标不是一回事。
                // 它是"文档此刻的值"，用来判断草稿里哪几个数真的变了
                let at_end = st
                    .selected()
                    .map(|l| l.perf(&st.chart.tmap, sec))
                    .unwrap_or_default();
                ui.separator();
                ui.label(format!("就位目标（块末 {:.3} 拍 / {:.3}s）", e.end_beat, sec))
                    .on_hover_text(
                        "在**这块的末尾**把线放到给定坐标：x/y（RPE 单位）、角度（度）、透明度（0–1）。\n\
                         四条轨道一起写，整条命令是**一个撤销步**。\n\
                         · 块末本来就在边界上 ⇒ 只改端值，块内曲线跟着新端值走；\n\
                         · 块末落在块内 ⇒ 先在此切一刀（切点值 = 当前值，不跳变）再写两侧；\n\
                         · 块末在空位 ⇒ 写前一块的终值（空位的值就是它）；\n\
                         · 已经是这个值的轨道**不动**。\n\
                         流速轨不在其中（它不是坐标，音符位置是它的积分）。",
                    );
                // 初值取快照里的**草稿**（不是每帧从文档重取）：用户改完要按按钮，
                // 值必须活过一帧 —— 见 `view::Inspector::target` 的注释
                let mut tx = v.target.x;
                let mut ty = v.target.y;
                let mut ta = v.target.angle;
                let mut tp = v.target.alpha;
                value_field(ui, &mut tx, "目标 x ", 1.0, None);
                value_field(ui, &mut ty, "目标 y ", 1.0, None);
                value_field(ui, &mut ta, "目标角度 ", 1.0, None);
                // 透明度与 RPE/音符同量纲：**0~255**（v2 口径）
                value_field(ui, &mut tp, "目标透明度 ", 1.0, Some(0.0..=255.0));
                // "变了没有"按**位**比：同一个数重敲一遍不该发命令（与 set_target 的跳过判据同源）
                let diff = |now: f64, was: f64| now.to_bits() != was.to_bits();
                let mut target: Vec<(&'static str, f64)> = Vec::new();
                if diff(tx, at_end.x as f64) {
                    target.push(("x", tx));
                }
                if diff(ty, at_end.y as f64) {
                    target.push(("y", ty));
                }
                if diff(ta, at_end.rotate_deg as f64) {
                    target.push(("angle", ta));
                }
                if diff(tp, at_end.alpha as f64) {
                    target.push(("alpha", tp));
                }
                let cmd = opm_app::edit::set_target_command(line_doc, anchor, &target);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(cmd.is_some(), egui::Button::new("一次写入（四轨就位）"))
                        .on_disabled_hover_text("四个数都还是当前值：没有要写的")
                        .clicked()
                    {
                        if let Some(c) = cmd {
                            ec.push(c);
                        }
                    }
                });
                // 草稿写回快照（下一帧的初值），这样"改几个数 → 按按钮"才成立
                v.target = opm_app::view::TargetEdit { x: tx, y: ty, angle: ta, alpha: tp };
            }

            // ---- 音符（当前线选中项）：可编辑 ----
            if let Some(n) = v.note_edit.as_ref().filter(|_| show_note) {
                ui.separator();
                ui.label(format!("音符 doc#{}", v.note.as_ref().map(|x| x.doc_index).unwrap_or(0)));
                let idx = v.note.as_ref().map(|x| x.doc_index).unwrap_or(0);
                // ---- **重叠组**：完全重叠的音符在这里点开（用户口径）----
                //
                // 判据完全来自**编辑区的选择框**（`overlay::overlap_group`：交叠区在长或宽上
                // 超过被盖者的一半；hold 只按头部算、判定排在其它类型下面）。这里只负责列出来 ——
                // 编辑区里点一下只能选到"最上面"那个，被盖住的没有别的入口。
                // 没有任何音符被盖住时，列表里就剩它自己一项。
                // 行取自**活的** `st`（编辑区每帧刷新 `note_stack`）；快照只在广播/换选区时重建，
                // 放进快照会永远慢一拍。
                let stack = opm_app::view::note_stack_rows(st);
                if !stack.is_empty() {
                    ui.label(format!("重叠组（{}）", stack.len())).on_hover_text(
                        "与这个音符**选择框有效覆盖**的音符：交叠区在长或宽上超过被盖住那个的一半。\n\
                         · hold 只按**头部**算（长身体既不算盖住别人、也不算被盖住），且判定排在其它类型下面；\n\
                         · hold 盖 hold 同样按这条判据；\n\
                         · 点一行就把选区换成它（完全重叠时这是被盖住那个的唯一入口）。\n\
                         这条机制只关乎**编辑区怎么选**，与谱面本身无关（不进文档）。",
                    );
                    for row in &stack {
                        // 右栏很窄（约 190px）：行里只放"能分辨"的三件事，细节挂 hover。
                        // 当前锚那一行由 `selectable_label` 自己高亮，不再占文字宽度写"← 当前"。
                        let text = format!("#{} {} {:.3} 拍", row.doc_index, row.kind, row.beat);
                        let r = ui
                            .selectable_label(row.is_anchor, egui::RichText::new(text).monospace())
                            .on_hover_text(format!(
                                "doc#{} · {} · 判定 {:.3} 拍（{:.3} s）· laneX {:.1}{}",
                                row.doc_index,
                                row.kind,
                                row.beat,
                                st.chart.tmap.sec(row.beat),
                                row.lane_x,
                                if row.is_anchor { " · 就是当前这个" } else { "" }
                            ));
                        if r.clicked() && !row.is_anchor {
                            out.select_note = Some(row.view_index);
                        }
                    }
                }
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
                // ---- 判定时刻（`拍`）与释放时刻（hold 的 `止`）：同样是**三元组控件** ----
                // 用户口径（2026-10-01）：音符的判定时间也用三元组。以前是浮点框 + `beat_json`
                // 按当前网格取整 —— 编 1/3 得先把网格改成"每拍 3 条"，而且落盘走的是量化后的值。
                // 这里直接给精确有理拍（`[分子, 分母]`，既约），鼠标拖拽那条路仍然按网格吸附。
                if let Some(b) = beat_triple_field(ui, "note_start", "拍 ", n.start_exact) {
                    if b.n != n.start_exact.n || b.d != n.start_exact.d {
                        set.insert("startBeat".into(), opm_app::edit::beat_arg(b));
                    }
                }
                if let Some(was) = n.end_exact {
                    if let Some(b) = beat_triple_field(ui, "note_end", "止 ", was) {
                        if b.n != was.n || b.d != was.d {
                            set.insert("endBeat".into(), opm_app::edit::beat_arg(b));
                        }
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
                // **负值也允许**：音符自身的 speed 带符号，负值把方向翻过来
                // （与负流速是同一套几何：音符从判定线下面上来，到线之前不显示）
                if value_field(ui, &mut sp, "speed ", 0.01, Some(-20.0..=20.0)).changed
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
            // 什么都没选（或选区处于"等下一帧"的中间态）：说明这里为什么是空的，
            // 而不是留一片让人猜的空白。线属性在上面 —— 它永远属于当前这条线。
            if sel.is_none() {
                ui.separator();
                ui.label("（选中音符或事件，这里显示对应的编辑器）").on_hover_text(
                    "属性编辑器分两半：**音符** / **事件**。选区同时只有一类（框选按起点在哪半区定），\n\
                     所以点到哪一类就编辑哪一类；另一半不会占着位置。",
                );
            }
            ui.separator();
            ui.label("此刻表演（事件求值）");
            ui.monospace(format!("moveX  {:>8.2}", v.perf.x));
            ui.monospace(format!("moveY  {:>8.2}", v.perf.y));
            ui.monospace(format!("rotate {:>8.2}°", v.perf.rotate_deg));
            ui.monospace(format!("alpha  {:>8.1}", v.perf.alpha));
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
    out
}

/// **遮蔽区面板**（遮蔽区编辑模式下右栏的全部内容）。
///
/// 它只读 `st`（视图状态）—— 名字/通道/事件块/此刻的状态都在视图模型里，
/// 不需要文档快照（`Inspector` 是按判定线取的，遮蔽区与判定线无关）。
fn mask_panel(ui: &mut Ui, st: &EditorState, out: &mut InspectorOut) {
    let ec = &mut out.commands;
    let beat = st.chart.tmap.beat(st.playhead);
    let snapped = st.snap_beat(beat).max(0.0);

    ui.horizontal(|ui| {
        ui.label("遮蔽区");
        ui.monospace(format!(
            "#{} / {}",
            st.selected_zone,
            st.chart.zones.len()
        ));
    });
    // 「新建」的起点：**0 区时是拍 0**（那块草稿三角就是它），已有区时是播放头 ——
    // 口径只有一处（`EditorState::mask_new_zone_start`），树面板那颗按钮调的是同一个。
    let new_start = st.mask_new_zone_start();
    ui.horizontal(|ui| {
        if ui
            .small_button("新建")
            .on_hover_text(format!(
                "摆一个中央正三角形（六条常量事件、长 {} 拍；active 不写事件）。\n\
                 起点：一块都没有时 = 拍 0（与编辑区里那块草稿三角同一块），否则 = 播放头（{:.3} 拍）",
                opm_app::doc::MASK_EVENT_BEATS,
                new_start.to_f64()
            ))
            .clicked()
        {
            ec.push(opm_app::edit::add_zone_command(new_start));
        }
        if ui
            .add_enabled(
                !st.chart.zones.is_empty(),
                egui::Button::new("删除本区").small(),
            )
            .on_hover_text("删掉当前这块遮蔽区（Ctrl+Z 可撤销）")
            .clicked()
        {
            ec.push(opm_app::edit::del_zone_command(st.selected_zone));
        }
    });

    let Some(mi) = opm_app::view::mask_inspect(st) else {
        ui.separator();
        ui.label("（还没有遮蔽区 —— 点上面的「新建」）").on_hover_text(
            "遮蔽区（游戏里的「躁域」）：一块三角形区域，游戏里点进这块区域无法与音符交互。\n\
             它的形状完全由事件块决定 —— **三条坐标通道一个事件都没有时它不存在**（不显示）。",
        );
        return;
    };
    // 从**这一行往后**产生的命令才是"假定已经有区"的编辑（草稿态下要先建区再应用，
    // 见 `InspectorOut::mask_edits`）。
    //
    // 为什么要落在「新建/删除」**之后**：`add_zone` 本身就是一条完整命令 —— 再被草稿包装
    // 套一层，一次点击就会建出**两块**区（草稿那块 + 新建那块）。删除在草稿态是禁用的，
    // 所以只有新建这一颗需要挡。
    let mask_edits_mark = ec.len();

    // ---- 名字 ----
    ui.horizontal(|ui| {
        ui.label("名字");
        if let Some(name) = text_field(ui, "zone-name", &mi.name, 130.0) {
            ec.push(opm_app::edit::set_zone_command(
                mi.index,
                serde_json::json!({"name": name}),
            ));
        }
    });

    // ---- 此刻 ----
    // "此刻"= 播放头那一拍：不显示时**明说**（这正是它与判定线不同的地方）
    let st_now = mi.state;
    ui.separator();
    if st_now.visible {
        ui.monospace(format!(
            "此刻 三角形 ({:.0},{:.0}) ({:.0},{:.0}) ({:.0},{:.0})",
            st_now.v[0][0], st_now.v[0][1], st_now.v[1][0], st_now.v[1][1], st_now.v[2][0], st_now.v[2][1]
        ));
    } else {
        ui.colored_label(
            egui::Color32::from_rgb(255, 190, 110),
            "此刻**不显示**（三条坐标通道都还没有已开始的事件）",
        );
    }
    ui.monospace(format!(
        "外观 {}",
        if st_now.active { "true（细网格 + 更透明）" } else { "false（纯色）" }
    ));

    // ---- 整区 active：**一块区一种状态**（用户口径 2026-10-02）----
    //
    // "一个事件块一种状态，不能在头和尾有不同状态" —— 于是这里只给一个开关：
    // 一按把这块区**所有** active 块改写成那一档（各块的时间跨度不动）；一块都没有时
    // 按坐标事件的包络写一块（`core::set_zone` 的 `active` 字段）。想中途换外观就放两块，
    // 那是编辑区里的事；这里是"整块区的档位"。
    let zone_active = mi.channels.iter().find_map(|r| {
        (r.channel == opm_app::state::MaskChannel::Active).then_some(r)
    });
    let active_blocks = zone_active.map(|r| r.events).unwrap_or(0);
    // 判据取自视图模型（**逐块**看头尾，见 `MaskInspect::active_mixed`）——
    // 不是"整条通道的 min/max"：几块之间取不同的档是合法的（外观分段切换）
    let active_mixed = mi.active_mixed;
    let has_coords = mi
        .channels
        .iter()
        .any(|r| r.channel != opm_app::state::MaskChannel::Active && r.events > 0);
    ui.horizontal(|ui| {
        let mut on = st_now.active;
        let resp = ui.add_enabled(
            has_coords,
            egui::Checkbox::new(&mut on, "整区 active（true = 细网格）"),
        );
        let hint = if has_coords {
            format!(
                "这块区**只有一种状态**：一按就把它的 {active_blocks} 个 active 块全改成这一档。\n\
                 想中途换外观：在编辑区的 active 列里放**两块**（各是一种状态）。"
            )
        } else {
            "这块区还没有任何坐标事件 —— active 现在没有意义（先把三角形写出来）".to_owned()
        };
        if resp.on_hover_text(hint).changed() {
            ec.push(opm_app::edit::set_zone_command(
                mi.index,
                serde_json::json!({ "active": on }),
            ));
        }
    });
    if active_mixed {
        ui.colored_label(
            egui::Color32::from_rgb(255, 190, 110),
            "⚠ 某一块 active 的头尾不同档（校验会报错）—— 选中它、用下面那个复选框改一下即可",
        );
    }

    // ---- 在播放头放一块 ----
    //
    // 跨度规则与编辑区里的手势草稿**同一条**（`EditorState::mask_span_at`）：长度一个格点、
    // 不越过下一块；这一点上已经有一块时按钮禁用并说明原因 —— 以前这里写死 4 拍、也不夹取，
    // 于是"在播放头放一块"能造出一条与下一块重叠的事件（自己的产物过不了自己的校验器）。
    ui.separator();
    let ch = mi.channel;
    let place = st.mask_span_at(ch, snapped);
    let reason = format!(
        "给当前列 {} 放一个事件块：长度一个格点（当前 {:.3} 拍）、**值取此刻的值**（放下不跳变）\n\
         · 不越过下一块（通道内不许重叠）\n\
         · 与编辑区里的 R 起稿是**同一条跨度规则**，只是这里没有鼠标可拖",
        ch.key(),
        st.beat_step()
    );
    ui.horizontal(|ui| {
        if ui
            .add_enabled(place.is_some(), egui::Button::new("在播放头放一块").small())
            .on_hover_text(if place.is_some() {
                reason
            } else {
                format!("{reason}\n\n**这里放不下**：播放头那一拍上已经有一块了 —— 挪一下播放头，或先删掉那一块")
            })
            .clicked()
        {
            if let Some((start, end)) = place {
                ec.push(opm_app::edit::add_mask_event_command(
                    mi.index, ch, start, end,
                ));
            }
        }
    });

    // ---- 七条通道 ----
    ui.separator();
    ui.label("通道（点一行切换编辑区的当前列）");
    for row in &mi.channels {
        let mark = if row.channel == ch { "▶" } else { " " };
        let val = match (row.channel, row.value) {
            (opm_app::state::MaskChannel::Active, Some(v)) => {
                if v >= 0.5 { "true".to_owned() } else { "false".to_owned() }
            }
            (_, Some(v)) => format!("{v:>8.1}"),
            (_, None) => "       —".to_owned(),
        };
        let text = format!(
            "{mark}{:<6}{:>3}块 {val}  [{:.0},{:.0}] {}",
            row.channel.key(),
            row.events,
            row.min,
            row.max,
            row.channel.short_unit()
        );
        if ui.monospace(text).clicked() {
            out.select_channel = Some(row.channel);
        }
    }
    ui.monospace(format!("（播放头 {beat:.3} 拍）"));

    // ---- 选中的事件块：可编辑 ----
    let Some(ev) = mi.event.as_ref() else {
        ui.separator();
        ui.label("（在编辑区点一个事件块，这里编辑它）");
        return;
    };
    ui.separator();
    ui.label(format!("事件块 · {} #{}", ch.key(), ev.index));
    // 头/尾：**【整拍数】+【分子】/【分母】**（与判定线的事件编辑器同一套控件、同一个理由：
    // 1/3 这类拍走浮点留不住）
    for (key, label, edge, was) in [
        ("ze_start", "起 ", opm_app::state::EventEdge::Start, ev.start_exact),
        ("ze_end", "止 ", opm_app::state::EventEdge::End, ev.end_exact),
    ] {
        if let Some(b) = beat_triple_field(ui, key, label, was) {
            if b.n != was.n || b.d != was.d {
                let field = if edge == opm_app::state::EventEdge::Start { "startBeat" } else { "endBeat" };
                ec.push(opm_app::edit::set_mask_event_command(
                    mi.index,
                    ch,
                    ev.index,
                    serde_json::json!({ field: opm_app::edit::beat_arg(b) }),
                ));
            }
        }
    }
    // 值：`active` 是**布尔**（用户口径"二值化"），坐标通道是数字。
    // 判据取自值本身的类型，不是通道名 —— 文件里写 `true`/`false` 与写 `1`/`0` 都成立。
    let as_bool = |v: &serde_json::Value| -> Option<bool> {
        match v {
            serde_json::Value::Bool(b) => Some(*b),
            serde_json::Value::Number(n) => Some(n.as_f64().unwrap_or(0.0) >= 0.5),
            _ => None,
        }
    };
    match (as_bool(&ev.start_value), as_bool(&ev.end_value)) {
        (Some(sb), Some(eb)) if ch == opm_app::state::MaskChannel::Active => {
            // **一个 active 块一种状态**（用户口径）：所以只有一个复选框，
            // 一改就同时写 `startValue` 与 `endValue` —— 两个独立复选框正是"头 true、尾 false"
            // 这种块的来源（核心现在也会拒，但界面不该给出这条歧路）。
            let mut state = sb;
            let resp = ui
                .checkbox(&mut state, "true（细网格 + 更透明）")
                .on_hover_text(if sb != eb {
                    format!(
                        "这块现在的头尾不一致（起 {sb} / 止 {eb}）—— 一改就都写成同一档；\
                         不改它的话，校验会一直报这一块"
                    )
                } else {
                    "这一块的状态（头尾一起改）—— 想中途换外观就再放一块".to_owned()
                });
            if resp.changed() {
                ec.push(opm_app::edit::set_mask_event_command(
                    mi.index,
                    ch,
                    ev.index,
                    serde_json::json!({"startValue": state, "endValue": state}),
                ));
            }
        }
        _ => {
            let (mut sv, mut evv) = (
                ev.start_value.as_f64().unwrap_or(0.0),
                ev.end_value.as_f64().unwrap_or(0.0),
            );
            let mut changed = false;
            changed |= value_field(ui, &mut sv, "值起 ", 1.0, None).changed;
            changed |= value_field(ui, &mut evv, "值止 ", 1.0, None).changed;
            if changed {
                ec.push(opm_app::edit::set_mask_event_command(
                    mi.index,
                    ch,
                    ev.index,
                    serde_json::json!({"startValue": sv, "endValue": evv}),
                ));
            }
        }
    }
    // 缓动：与判定线事件同一套控件（遮蔽区的坐标也是"从这一块到下一块"插值的）
    if let Some((curve, variant)) = cmd::split_easing(&ev.easing) {
        let mut new_curve = curve;
        egui::ComboBox::from_id_salt("ze_easing_curve")
            .selected_text(curve.label())
            .width(104.0)
            .show_ui(ui, |ui| {
                for c in cmd::EaseCurve::ALL {
                    ui.selectable_value(&mut new_curve, c, c.label());
                }
            });
        let mut new_variant = new_curve.clamp_variant(variant.unwrap_or(cmd::EaseVariant::Out));
        let avail = new_curve.variants();
        ui.add_enabled_ui(!avail.is_empty(), |ui| {
            egui::ComboBox::from_id_salt("ze_easing_variant")
                .selected_text(new_variant.label())
                .width(52.0)
                .show_ui(ui, |ui| {
                    for v in avail {
                        ui.selectable_value(&mut new_variant, *v, v.label());
                    }
                });
        });
        let want = cmd::easing_name(new_curve, new_variant);
        if want != ev.easing {
            ec.push(opm_app::edit::set_mask_event_command(
                mi.index,
                ch,
                ev.index,
                serde_json::json!({"easing": want}),
            ));
        }
    } else {
        ui.monospace(format!("缓动 {}（无法识别，已原样保留）", ev.easing));
    }
    if ui
        .button("删除这一块")
        .on_hover_text("Del 同效（Ctrl+Z 可撤销）")
        .clicked()
    {
        ec.push(opm_app::edit::del_mask_event_command(mi.index, ch, ev.index));
    }
    out.mask_edits = ec.len() > mask_edits_mark;
}

#[cfg(test)]
mod tests {
    use super::*;
    use opm_app::core::EditCore;
    use opm_app::state;
    use opm_app::view;
    use serde_json::json;

    // 一帧里画出来的所有文本（用于断言"这一栏真的画了什么"）：实现搬到 `testkit`
    use crate::testkit::drawn_texts;

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
    fn inspector_draws_the_line_props_and_only_the_selected_kinds_editor() {
        let (_c, st, mut insp) = sample();
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(320.0, 900.0),
            )),
            ..Default::default()
        };
        let draw = |st: &state::EditorState, insp: &mut Inspector, cmds: &mut Vec<serde_json::Value>| {
            let mut out = ctx.run_ui(raw.clone(), |ui| {
                *cmds = inspector_ui(ui, st, Some(insp)).commands;
            });
            out.textures_delta.clear();
            drawn_texts(&out).join("\n")
        };
        let mut cmds = Vec::new();
        let joined = draw(&st, &mut insp, &mut cmds);
        // 注意："属性编辑器" 这个标题留在调用点（`main.rs` 的面板包装里），不在本函数里
        for want in ["线名", "isCover（遮挡音符）", "此刻表演（事件求值）"] {
            assert!(joined.contains(want), "没画出来 {want:?}；实际画了：\n{joined}");
        }
        // `sample()` 最后点的是**事件** ⇒ 只有事件编辑器那一半
        assert!(joined.contains("事件 · moveX"), "选中的是事件，事件编辑器该在：\n{joined}");
        assert!(
            !joined.contains("音符 doc#"),
            "选中的是事件 ⇒ 音符编辑器**不该**同时占着右栏（用户口径：拆成两半，选中哪种显示哪种）：\n{joined}"
        );
        assert!(!joined.contains("laneX"), "音符那一半的字段也不该露出来：\n{joined}");
        // 锚仍然保留（切回音符时还是那一个），只是不显示 —— 这正是"留锚 ≠ 显示"的意思
        assert!(insp.note_edit.is_some(), "锚还在");
        assert!(cmds.is_empty(), "没有交互却产出了命令：{cmds:?}");

        // 反过来：只选音符 ⇒ 音符编辑器在、事件编辑器不在
        let (c2, mut st2) = {
            let mut c = EditCore::new();
            let r = c.exec(&json!({"op":"add_note","line":0,"kind":"tap","startBeat":[1,4],"laneX":100.0}));
            assert_eq!(r["ok"], json!(true), "{r}");
            let mut st = EditorState::new(state::chart_from_doc(c.doc()));
            st.selected_line = 0;
            st.select_note(0);
            (c, st)
        };
        let mut insp2 = view::inspector_of(&st2, c2.doc()).expect("有选中的线");
        let joined2 = draw(&st2, &mut insp2, &mut cmds);
        assert!(joined2.contains("音符 doc#0"), "选中的是音符，音符编辑器该在：\n{joined2}");
        assert!(!joined2.contains("事件 · "), "音符选中时不该出现事件编辑器：\n{joined2}");
        assert!(!joined2.contains("就位目标"), "就位目标是事件那一半的：\n{joined2}");
        // 选区清空 ⇒ 两半都不画，但要说明为什么是空的（不留让人猜的空白）
        st2.clear_selection();
        insp2 = view::inspector_of(&st2, c2.doc()).expect("有选中的线");
        let joined3 = draw(&st2, &mut insp2, &mut cmds);
        assert!(joined3.contains("选中音符或事件"), "空选区要说一句：\n{joined3}");
        assert!(!joined3.contains("音符 doc#") && !joined3.contains("事件 · "), "{joined3}");
        // tap 不该出现 hold 才有的结束拍输入框（`note_edit.end_exact` 为 None ⇒ 不画）
        assert!(insp2.note_edit.is_none() || insp2.note_edit.as_ref().unwrap().end_exact.is_none());
    }

    /// 一帧里某个文本的中心点（按文字找控件矩形 —— 按钮/字段的矩形在 `inspector_ui` 内部，
    /// 测试拿不到，但画出来的字形位置就是它的位置）
    fn text_center(out: &egui::FullOutput, needle: &str) -> Option<egui::Pos2> {
        fn walk(shape: &egui::epaint::Shape, needle: &str, acc: &mut Option<egui::Pos2>) {
            match shape {
                egui::epaint::Shape::Text(t) => {
                    if acc.is_none() && t.galley.text().contains(needle) {
                        *acc = Some(t.pos + t.galley.size() * 0.5);
                    }
                }
                egui::epaint::Shape::Vec(v) => {
                    for s in v {
                        walk(s, needle, acc);
                    }
                }
                _ => {}
            }
        }
        let mut acc = None;
        for cs in &out.shapes {
            walk(&cs.shape, needle, &mut acc);
        }
        acc
    }

    /// **「就位目标」按钮真的产出 `set_target`**（端到端：点字段 → 改一个数 → 点按钮）。
    ///
    /// 这条盯的是"面板只产出命令"这条契约在新控件上没被漏掉：字段改动**本身不发命令**
    /// （四个数都还是当前值时按钮是灰的），只有按下去才发**一条** `set_target`，
    /// 且带着块末的**精确有理拍**与只变了的那个键。
    #[test]
    fn the_target_button_emits_one_set_target_command() {
        let mut c = EditCore::new();
        let r = c.exec(&json!({"op": "add_event", "line": 0, "layer": 0, "track": "moveX",
                               "startBeat": [0, 1], "endBeat": [4, 1],
                               "startValue": 0.0, "endValue": 100.0, "easing": "inOutCubic"}));
        assert_eq!(r["ok"], json!(true), "{r}");
        let mut st = EditorState::new(state::chart_from_doc(c.doc()));
        st.selected_line = 0;
        st.selected_track = state::TrackId::MoveX;
        st.select_event(state::TrackId::MoveX, 0);
        let mut insp = view::inspector_of(&st, c.doc()).expect("有选中的线");

        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(360.0, 900.0),
            )),
            ..Default::default()
        };
        let mut cmds: Vec<serde_json::Value> = Vec::new();
        let mut frame = |events: Vec<egui::Event>, cmds: &mut Vec<serde_json::Value>| {
            let mut raw = raw.clone();
            raw.events = events;
            let mut out = ctx.run_ui(raw, |ui| {
                cmds.extend(inspector_ui(ui, &st, Some(&mut insp)).commands);
            });
            out.textures_delta.clear();
            out
        };
        let click = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        // 两帧建立布局（第一帧滚动区还不知道多大）
        frame(vec![], &mut cmds);
        let out = frame(vec![], &mut cmds);
        assert!(cmds.is_empty(), "没交互不该发命令：{cmds:?}");

        // ① 只改「目标 x」字段：点进去 → 输入 250 → 回车
        let fx = text_center(&out, "目标 x").expect("目标 x 字段画出来了");
        frame(vec![egui::Event::PointerMoved(fx), click(fx, true)], &mut cmds);
        frame(vec![click(fx, false)], &mut cmds);
        for ch in ["2", "5", "0"] {
            frame(vec![egui::Event::Text(ch.to_owned())], &mut cmds);
        }
        frame(
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }],
            &mut cmds,
        );
        assert!(
            cmds.is_empty(),
            "改字段本身不该发命令（要按「一次写入」才写）：{cmds:?}"
        );

        // ② 按「一次写入」：该发**一条** set_target，锚在块末 4 拍，只带 x
        let out = frame(vec![], &mut cmds);
        let btn = text_center(&out, "一次写入").expect("按钮画出来了");
        frame(vec![egui::Event::PointerMoved(btn), click(btn, true)], &mut cmds);
        frame(vec![click(btn, false)], &mut cmds);
        let target: Vec<&serde_json::Value> =
            cmds.iter().filter(|c| c["op"] == json!("set_target")).collect();
        assert_eq!(target.len(), 1, "应当只发一条 set_target：{cmds:?}");
        assert_eq!(target[0]["line"], json!(0));
        assert_eq!(target[0]["atBeat"], json!([4, 1]), "锚点是块末的**精确有理拍**");
        assert_eq!(target[0]["target"]["x"], json!(250.0), "只带变了的那个键");
        assert!(target[0]["target"].get("y").is_none(), "{:?}", target[0]);
        assert!(target[0]["target"].get("alpha").is_none(), "{:?}", target[0]);
    }

    /// 没有选中判定线时画的是"（没有判定线）"，而不是空白（用户要知道为什么右边是空的）
    #[test]
    fn inspector_says_so_when_nothing_is_selected() {        let ctx = egui::Context::default();
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
            cmds = inspector_ui(ui, &st, None).commands;
        });
        out.textures_delta.clear();
        let joined = drawn_texts(&out).join("\n");
        assert!(joined.contains("（没有判定线）"), "{joined}");
        assert!(cmds.is_empty());
    }

    /// **事件时间用三元组编辑**：控件是 `【整拍数】 + 【分子】 / 【分母】`，命令里发的是精确三元组。
    ///
    /// 这条钉的是"从界面到文件"整条链：控件编出来的拍 → `resize_event` 的 `toBeat` → 导出为 pez
    /// 时的 `startTime`。以前这条链上有两处浮点：检查器的 `起`/`止` 是 f64 框、命令走
    /// `beat_json`（按当前网格取整）—— 用户编 `1/3`，落到的可能是 `1/4`，或者写文件时变成
    /// `333333/1000000`。
    #[test]
    fn event_time_is_edited_as_a_whole_plus_fraction_triple() {
        let mut c = EditCore::new();
        let r = c.exec(&json!({"op":"add_event","line":0,"layer":0,"track":"moveX",
                               "startBeat":[0,1],"endBeat":[4,1],
                               "startValue":0.0,"endValue":100.0}));
        assert_eq!(r["ok"], json!(true), "{r}");
        let mut st = EditorState::new(state::chart_from_doc(c.doc()));
        st.selected_line = 0;
        st.selected_track = state::TrackId::MoveX;
        st.select_event(state::TrackId::MoveX, 0);
        let mut insp = view::inspector_of(&st, c.doc()).expect("有选中的线");
        let ev = insp.event_edit.as_ref().expect("有选中的事件");
        assert_eq!((ev.start_exact.n, ev.start_exact.d), (0, 1), "起点的精确拍进快照");
        assert_eq!((ev.end_exact.n, ev.end_exact.d), (4, 1));

        // ① 三个控件真的画出来了（整拍 / 分子 / 分母 + 两个分隔符），且没交互就不发命令
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
            cmds = inspector_ui(ui, &st, Some(&mut insp)).commands;
        });
        out.textures_delta.clear();
        let texts = drawn_texts(&out);
        let joined = texts.join("\n");
        for want in ["起", "止", "+", "/"] {
            assert!(joined.contains(want), "三元组控件没画全，缺 {want:?}：\n{joined}");
        }
        assert!(cmds.is_empty(), "没交互却发了命令：{cmds:?}");

        // ② 精确命令 → 施加 → 导出 pez：`startTime` 就是 `[0, 1, 3]`
        let cmd = opm_app::edit::event_resize_command_exact(
            &st,
            state::TrackId::MoveX,
            opm_app::doc::EventRef::new(0, 0),
            state::EventEdge::Start,
            opm_app::doc::Beat::new(1, 3),
        );
        assert_eq!(cmd["toBeat"], json!([1, 3]), "命令里是**既约**有理拍，不经过浮点：{cmd}");
        let r = c.exec(&cmd);
        assert_eq!(r["ok"], json!(true), "{r}");
        let (text, _fid) = opm_app::codec::rpe::save_str(c.doc(), Default::default());
        let root: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mx = root["judgeLineList"][0]["eventLayers"][0]["moveXEvents"]
            .as_array()
            .unwrap();
        assert!(
            mx.iter().any(|e| e["startTime"] == json!([0, 1, 3])),
            "导出的 pez 里应有 startTime = [0,1,3]：{text}"
        );
    }

    /// **音符的判定时刻同样用三元组编辑**（用户口径，2026-10-01）：
    /// `拍`（判定时刻）与 hold 的 `止`（释放时刻）都是 `【整拍】+【分子】/【分母】`，
    /// 命令里发**既约**有理拍 ⇒ 导出到 pez 就是 `[整拍, 分子, 分母]`。
    ///
    /// 以前是浮点框 + `st.beat_json()` 按网格取整：编 `1/3` 得先把网格改成"每拍 3 条"，
    /// 而且落盘的是量化后的值。（鼠标拖拽那条路仍然按网格吸附 —— 那是手势，不是键入。）
    #[test]
    fn note_judge_time_is_edited_as_a_whole_plus_fraction_triple() {
        let mut c = EditCore::new();
        let r = c.exec(&json!({"op":"add_note","line":0,"kind":"hold",
                               "startBeat":[1,3],"endBeat":[5,3],"laneX":100.0}));
        assert_eq!(r["ok"], json!(true), "{r}");
        let mut st = EditorState::new(state::chart_from_doc(c.doc()));
        st.selected_line = 0;
        st.select_note(0);
        let mut insp = view::inspector_of(&st, c.doc()).expect("有选中的线");
        let n = insp.note_edit.as_ref().expect("有选中的音符");
        assert_eq!((n.start_exact.n, n.start_exact.d), (1, 3), "判定时刻的精确拍进快照");
        assert_eq!(n.end_exact.map(|b| (b.n, b.d)), Some((5, 3)), "hold 的释放时刻也在");

        // ① 面板画的是三元组控件（`拍`/`止` + 两个分隔符），且没交互不发命令
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
            cmds = inspector_ui(ui, &st, Some(&mut insp)).commands;
        });
        out.textures_delta.clear();
        let joined = drawn_texts(&out).join("\n");
        for want in ["拍", "止", "+", "/"] {
            assert!(joined.contains(want), "音符的三元组控件没画全，缺 {want:?}：\n{joined}");
        }
        assert!(cmds.is_empty(), "没交互却发了命令：{cmds:?}");

        // ② 控件产出的那份 `set` 载荷 → 施加 → 导出 pez：`startTime` 是 `[0, 2, 5]`
        let payload = json!({"startBeat": opm_app::edit::beat_arg(opm_app::doc::Beat::new(2, 5))});
        assert_eq!(payload["startBeat"], json!([2, 5]), "命令里是既约有理拍：{payload}");
        let r = c.exec(&json!({"op":"set_note","line":0,"index":0,"set":payload}));
        assert_eq!(r["ok"], json!(true), "{r}");
        let (text, _fid) = opm_app::codec::rpe::save_str(c.doc(), Default::default());
        let root: serde_json::Value = serde_json::from_str(&text).unwrap();
        let note = &root["judgeLineList"][0]["notes"][0];
        assert_eq!(note["startTime"], json!([0, 2, 5]), "判定时刻要精确落在 2/5 拍：{text}");
        assert_eq!(note["endTime"], json!([1, 2, 3]), "释放时刻仍是 5/3 拍");
    }

    /// **重叠组列表**：列出被盖住的音符，点一行就把选区换成它。
    ///
    /// 选中是**视图**动作（不属于文档）⇒ 走 `InspectorOut::select_note`，不进命令列表。
    /// 分组本身在编辑区算（`overlay::overlap_group`，那边有选择框几何，另有一组单测）；
    /// 这条钉的是"列表画出来了吗、点得动吗"。
    #[test]
    fn the_overlap_group_lists_covered_notes_and_a_click_switches_the_anchor() {
        let mut c = EditCore::new();
        // 两个完全重叠的 tap（同一拍、同一 lane）——正是"点一下只能选到最上面那个"的场景
        for _ in 0..2 {
            let r = c.exec(&json!({"op":"add_note","line":0,"kind":"tap",
                                   "startBeat":[2,1],"laneX":100.0}));
            assert_eq!(r["ok"], json!(true), "{r}");
        }
        let mut st = EditorState::new(state::chart_from_doc(c.doc()));
        st.selected_line = 0;
        st.select_note(0);
        // 编辑区每帧会把这一份喂进来（这里直接给，几何算法不在这条测试里）
        st.set_note_stack(vec![0, 1]);
        let mut insp = view::inspector_of(&st, c.doc()).expect("有选中的线");
        let rows = view::note_stack_rows(&st);
        assert_eq!(rows.len(), 2, "两个都在组里");
        assert!(rows[0].is_anchor, "第一行是锚");
        assert!(!rows[1].is_anchor);

        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(360.0, 900.0),
            )),
            ..Default::default()
        };
        let mut sel: Option<usize> = None;
        let mut frame = |events: Vec<egui::Event>, sel: &mut Option<usize>| {
            let mut raw = raw.clone();
            raw.events = events;
            let mut o = ctx.run_ui(raw, |ui| {
                let out = inspector_ui(ui, &st, Some(&mut insp));
                if out.select_note.is_some() {
                    *sel = out.select_note;
                }
            });
            o.textures_delta.clear();
            o
        };
        let click = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        frame(vec![], &mut sel);
        let out = frame(vec![], &mut sel);
        let joined = drawn_texts(&out).join("\n");
        assert!(joined.contains("重叠组（2）"), "列表头没画出来：\n{joined}");
        assert!(joined.contains("#0 Tap") && joined.contains("#1 Tap"), "两行都要列出来：\n{joined}");
        assert!(sel.is_none(), "没点不该换选区");

        // 点**非锚那一行**（doc#1，视图下标 1 —— 锚是 #0）⇒ 换选区到它
        let row = text_center(&out, "#1 Tap").expect("非锚那一行画出来了");
        frame(vec![egui::Event::PointerMoved(row), click(row, true)], &mut sel);
        frame(vec![click(row, false)], &mut sel);
        assert_eq!(sel, Some(1), "点那一行要把选区换成它（视图下标 1）");
    }
}
