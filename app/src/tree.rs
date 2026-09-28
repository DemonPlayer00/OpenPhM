// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **判定线树**（左侧面板）：判定线 → 事件轨道 → 事件 → 子音符。
//!
//! 它只读 `&EditorState`，把用户点的东西写成 [`TreeAction`]（**面板自己不施加动作**）：
//! "怎么画"与"点了之后干什么、发哪条命令"分开，后者在 `main.rs` 里统一走命令路径。
//! 从 `main.rs` 搬出来是因为它近 200 行、且与其它面板没有耦合 —— 主文件只剩下
//! "谁调用它、收到的动作怎么施加"。

use opm_app::state::{self, EditorState};
use opm_app::view::LineRow;

/// 左侧面板产出的**动作**（面板自己不施加，见调用处注释）
pub enum TreeAction {
    SelectLine(usize),
    SelectTrack(state::TrackId),
    SelectEvent(usize),
    SelectNote(usize),
    Cmd(serde_json::Value),
}

/// 判定线树：判定线 → 事件轨道 → 事件 → 子音符。只读 `&EditorState`，动作写进 `acts`。
pub fn line_tree_ui(
    ui: &mut egui::Ui,
    st: &EditorState,
    rows: &[LineRow],
    doc_lines: usize,
    doc_notes: usize,
    acts: &mut Vec<TreeAction>,
) {
    let tmap = &st.chart.tmap;
    let playhead = st.playhead;

    // ① 判定线（父对象）：每行直接显示该线**此刻的表演值** —— 事件是否生效先看数字
    egui::CollapsingHeader::new(format!(
        "判定线（{} 条；doc {} 行 {} 音符）",
        rows.len(),
        doc_lines,
        doc_notes
    ))
    .default_open(true)
    .show(ui, |ui| {
        for row in rows {
            let perf = st.chart.lines.get(row.view).map(|l| l.perf(tmap, playhead));
            let mark = if row.view == st.selected_line { "▶" } else { " " };
            // 单行必须放得下：面板宽 300px ≈ 41 个等宽字符。超宽会折行，
            // 而折行会让"这一行的值"看起来属于下一行（实测被自己误读过一次）。
            let name: String = row.name.chars().take(6).collect();
            let extra = match perf {
                Some(p) => format!(
                    "x{:.0} y{:.0} r{:.0} a{:.1}",
                    p.x, p.y, p.rotate_deg, p.alpha
                ),
                None => String::new(),
            };
            let text = format!(
                "{mark}#{} {:<6} z{}{} ♪{} ⚡{} {extra}",
                row.doc,
                name,
                row.z_order,
                if row.is_cover { "C" } else { "-" },
                row.notes,
                row.events,
            );
            if ui.monospace(text).clicked() {
                acts.push(TreeAction::SelectLine(row.view));
            }
        }
    });

    let Some(line) = st.selected() else {
        ui.label("（没有判定线：用 add_line 建一条）");
        return;
    };

    ui.separator();

    // ② 事件轨道（五条）：条数 + 播放头处的求值 + 值域
    egui::CollapsingHeader::new(format!("事件轨道（线 #{} {}）", line.index, line.name))
        .default_open(true)
        .show(ui, |ui| {
            let beat = tmap.beat(playhead);
            for id in state::TrackId::ALL {
                let t = line.track(id);
                // 五条轨道同一个口径（缓动按折线实现，含回弹点）—— 都在 `perf::track_value` 里
                let cur = opm_app::perf::track_value(&t.events, beat, tmap);
                let mark = if id == st.selected_track { "▶" } else { " " };
                let text = format!(
                    "{mark}{:<6}{:>3}条 t={:<7} [{:.0},{:.0}] {}",
                    id.key(),
                    t.events.len(),
                    cur.map(|v| format!("{v:.1}")).unwrap_or_else(|| "—".into()),
                    t.min,
                    t.max,
                    id.short_unit()
                );
                if ui.monospace(text).clicked() {
                    acts.push(TreeAction::SelectTrack(id));
                }
            }
        });

    ui.separator();

    // ③ 事件（当前轨道）：虚拟化列表 + 两个编辑动作（都只是"发命令"）
    let track = line.track(st.selected_track);
    let track_key = st.selected_track.key().to_owned();
    let line_doc = line.index;
    let playhead_beat = tmap.beat(playhead);
    egui::CollapsingHeader::new(format!("事件（{} 共 {} 条）", track_key, track.events.len()))
        .default_open(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.small_button("在播放头切分").clicked() {
                    if let Some((i, _)) = track.events.iter().enumerate().find(|(_, e)| {
                        playhead_beat > e.start.to_f64() && playhead_beat < e.end.to_f64()
                    }) {
                        // 拍用分数表示（毫拍 → 约分），别把浮点拍直接写进文档
                        // 切分点用**毫拍**（播放头不在格线上）——这条规则在 `edit::milli_beat`
                        acts.push(TreeAction::Cmd(opm_app::edit::split_event_command(
                            line_doc, &track_key, i, playhead_beat,
                        )));
                    }
                }
                if ui.small_button("删除选中").clicked() {
                    // 文档地址由**合并视图下标**换算回来（多图层文档里两者不是一回事）
                    if let Some(at) = st.selected_event().and_then(|i| track.origin(i)) {
                        acts.push(TreeAction::Cmd(opm_app::edit::del_event_command(
                            line_doc, &track_key, at,
                        )));
                    }
                }
            });
            let clicked = row_list(
                ui,
                "events_scroll",
                150.0,
                track.events.len(),
                st.selected_event(),
                |i| {
                    let e = &track.events[i];
                    format!(
                        "{i:>4} {:>6.2}s {:>6.1}→{:<6.1} {}",
                        tmap.sec(e.start.to_f64()),
                        e.start_value,
                        e.end_value,
                        e.easing
                    )
                },
            );
            if let Some(i) = clicked {
                acts.push(TreeAction::SelectEvent(i));
            }
        });

    ui.separator();

    // ④ 子音符（当前判定线的音符）
    let notes_len = line.notes.len();
    let line_speed = line.perf(tmap, playhead).speed;
    egui::CollapsingHeader::new(format!("子音符（{} 个；线速 {:.1}）", notes_len, line_speed))
        .default_open(true)
        .show(ui, |ui| {
            let clicked = row_list(
                ui,
                "notes_scroll",
                140.0,
                notes_len,
                st.selected_note(),
                |i| {
                    let n = &line.notes[i];
                    format!("{i:>6}  {:>7.3}s  {:>6.1}  {}", n.time, n.lane_x, n.kind.label())
                },
            );
            if let Some(i) = clicked {
                acts.push(TreeAction::SelectNote(i));
            }
        });
}

/// 一列**虚拟滚动**的行（事件表与子音符表共用）：返回被点中的下标。
///
/// 两处逐字相同，只有行文本、高度、滚动 id 不同。抽出来的真正理由是 `show_rows`：
/// 复制第二份时最容易漏掉它（改成 `for i in 0..total`），而漏了之后的症状是
/// "谱面一大就卡"——在几十个音符的样例上看不出来。行首的 "▶" 也是在这里统一的。
fn row_list(
    ui: &mut egui::Ui,
    id_salt: &str,
    max_height: f32,
    total: usize,
    selected: Option<usize>,
    row_text: impl Fn(usize) -> String,
) -> Option<usize> {
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    let mut clicked = None;
    egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .max_height(max_height)
        .auto_shrink([false, false])
        .show_rows(ui, row_h, total, |ui, range| {
            for i in range {
                let mark = if Some(i) == selected { "▶" } else { " " };
                if ui.monospace(format!("{mark}{}", row_text(i))).clicked() {
                    clicked = Some(i);
                }
            }
        });
    clicked
}

