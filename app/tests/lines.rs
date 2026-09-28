//! 判定线是父对象、音符是子对象 —— 这条依赖关系的可执行断言。
//!
//! 覆盖三件事：
//! 1. 视图构建：音符挂在线上、按 zOrder 排序、每条线的音符不串线；
//! 2. **几何**：判定线旋转/平移时，它子音符的实例中心与实例角度**同步变换**；
//! 3. **无关性**：改一条线的表演，不动另一条线的音符实例。

use opm_app::doc::{Beat, BpmEntry, Document, Event, JudgeLine, Note as DocNote, NoteKind as DocKind};
use opm_app::render::{build_instances, NoteInstance};
use opm_app::state::{chart_from_doc, tracks_of, EditorState, TrackId};
use serde_json::json;

/// 造一份 2 条线的小谱面：L0 在原点，L1 平移 moveY 并**常量**旋转 `rotate_to`。
///
/// 常量（from == to）是刻意的：几何断言不该随时间变，否则测试得先算准时刻。
/// "事件随时间变化"由 `rotate_event_is_time_varying` 单独覆盖。
fn two_line_doc(rotate_to: f64) -> Document {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry {
        start: Beat::zero(),
        bpm: 180.0,
        foreign: Default::default(),
    }];
    doc.judge_lines.clear();
    for (name, my, rot) in [("L0", 0.0, 0.0), ("L1", 300.0, rotate_to)] {
        let mut l = JudgeLine::default();
        l.name = name.to_owned();
        l.z_order = if name == "L0" { 0 } else { 1 };
        let end = Beat::new(64, 1);
        for (track, from, to) in [
            ("moveX", 0.0, 0.0),
            ("moveY", my, my),
            ("rotate", rot, rot),
            ("alpha", 1.0, 1.0),
            ("speed", 10.0, 10.0),
        ] {
            l.layers[0].track_mut(track).unwrap().clear();
            l.layers[0].track_mut(track).unwrap().push(Event::new(
                Beat::zero(),
                end,
                json!(from),
                json!(to),
                "linear",
            ));
        }
        // 每条线两个音符：lane_x 分别为 -300 / +300。
        //
        // **拍位是按 RPE 的下落速度定的**（流速 10 = 1× = 1200 单位/秒，见
        // `perf::SPEED_UNITS_PER_SEC`）：BPM 180 ⇒ 一拍 1/3 秒。
        // · 0.5 拍 = 0.167 s ⇒ 离判定线 200 单位 —— 在 ±450 的窗口里，**会被画出来**；
        // · 3 拍 = 1.0 s ⇒ 离判定线 1200 单位 —— 远在窗口外，实例会被裁掉。
        // 于是"窗口里只有每线一个音符"这条前提在新公式下依然成立（下面几条断言都吃它）。
        for (k, lane) in [-300.0_f32, 300.0].iter().enumerate() {
            let start = if k == 0 { Beat::new(1, 2) } else { Beat::new(3, 1) };
            let mut n = DocNote::new(DocKind::Tap, start, *lane);
            n.end = None;
            l.notes.push(n);
        }
        doc.judge_lines.push(l);
    }
    doc
}

#[test]
fn notes_belong_to_their_line() {
    let doc = two_line_doc(0.0);
    let chart = chart_from_doc(&doc);
    assert_eq!(chart.lines.len(), 2, "应有两条判定线");
    // 按 zOrder 排序（小的先画）
    assert_eq!(chart.lines[0].name, "L0");
    assert_eq!(chart.lines[1].name, "L1");
    for l in &chart.lines {
        assert_eq!(l.notes.len(), 2, "每条线各 2 个音符（音符不串线）");
        assert_eq!(l.event_count(), 5, "每条线 5 条轨道各 1 条常量事件");
        // 子音符带 doc 下标，能回写命令
        let mut idx: Vec<usize> = l.notes.iter().map(|n| n.doc_index).collect();
        idx.sort();
        assert_eq!(idx, vec![0, 1]);
    }
}
#[test]
fn notes_follow_line_rotation_and_translation() {
    // L1 旋转到 90°：子音符的实例角度必须也是 90°，中心按 rotate→translate 变换
    let doc = two_line_doc(90.0);
    let chart = chart_from_doc(&doc);
    let mut st = EditorState::new(chart);
    st.lookahead = 2.0;
    st.selected_line = usize::MAX;

    let tmap = st.chart.tmap.clone();
    let l1 = st.chart.lines[1].clone();
    let perf = l1.perf(&tmap, 0.0);
    assert!((perf.rotate_deg - 90.0).abs() < 1e-6, "L1 应转到 90°，实际 {}", perf.rotate_deg);
    assert!((perf.y - 300.0).abs() < 1e-6);

    let mut inst = Vec::new();
    build_instances(&st, &mut inst);
    // 每线只有第 1 个音符落在可见窗口里（第 2 个远在窗口上方，见 `two_line_doc`）
    // ⇒ 2 条线本体 + 2 个可见子音符 = 4
    assert_eq!(inst.len(), 4, "实例数 = 2 条线本体 + 2 个可见子音符");

    // L1 的音符在可见窗口内（0.5 拍 = 0.167s ⇒ 离判定线 200 单位）
    let rotated: Vec<_> = inst
        .iter()
        .filter(|i| (i.angle().to_degrees() - 90.0).abs() < 1e-3)
        .collect();
    assert_eq!(rotated.len(), 2, "L1 本体 + 1 个可见子音符都应是 90°");
    // 每个旋转实例的中心必须等于 perf.apply(本地坐标) —— 父子变换一致。
    // 做法：把中心**逆变换**回线本地坐标，再看它是否落在合法位置上。
    for i in rotated {
        let c = i.center();
        let (dx, dy) = (c[0] - perf.x, c[1] - perf.y);
        let (s, co) = (-perf.rotate_deg).to_radians().sin_cos();
        let lx = dx * co - dy * s;
        let ly = dx * s + dy * co;
        if (i.half()[0] - st.line_half_w).abs() < 1e-3 {
            // 线本体：本地 y 必须是 0
            // （按 `st.line_half_w` 认，不写死 675 —— 默认线长是 3000，写死了就会把线当成音符）
            assert!(ly.abs() < 1e-3, "线本体的本地 y 应为 0，实际 {ly}");
        } else {
            // 子音符：本地 x 就是它的 lane_x（±300），本地 y 是下落距离（正、有限）
            assert!((lx.abs() - 300.0).abs() < 1e-3, "子音符的本地 x 应为 ±300，实际 {lx}");
            assert!(ly > 0.0 && ly <= 1000.0, "子音符的本地 y 应是合理的下落距离，实际 {ly}");
        }
    }
    // L0 不动：它的实例角度应为 0
    let flat = inst.iter().filter(|i| i.angle().abs() < 1e-6).count();
    assert_eq!(flat, 2, "L0 本体 + 1 个可见子音符保持 0°");
}
#[test]
fn one_line_change_does_not_move_another_lines_notes() {
    // 实例发射顺序是一条**合同**：按 zOrder 逐线、每条线先本体后子音符。
    // 有了它，测试才能精确断言"改动只影响了某条线的实例"（否则得靠几何反推）。
    let mut inst_a = Vec::new();
    let mut inst_b = Vec::new();

    let doc_a = two_line_doc(0.0);
    let mut st_a = EditorState::new(chart_from_doc(&doc_a));
    st_a.lookahead = 2.0;
    st_a.selected_line = usize::MAX;
    build_instances(&st_a, &mut inst_a);

    // 只把 L1 转到 120°，L0 一动不动
    let doc_b = two_line_doc(120.0);
    let mut st_b = EditorState::new(chart_from_doc(&doc_b));
    st_b.lookahead = 2.0;
    st_b.selected_line = usize::MAX;
    build_instances(&st_b, &mut inst_b);

    assert_eq!(inst_a.len(), 4);
    assert_eq!(inst_b.len(), 4);
    // 前两个实例属于 L0（本体 + 1 个可见子音符）：中心与角度都必须一模一样
    for k in 0..2 {
        assert_eq!(inst_a[k].center(), inst_b[k].center(), "L0 的第 {k} 个实例被 L1 的改动带偏了");
        assert_eq!(inst_a[k].angle(), inst_b[k].angle());
        assert!(inst_a[k].angle().abs() < 1e-6, "L0 未旋转，角度应为 0");
    }
    // 后两个属于 L1：中心必须随旋转改变
    assert!(
        (inst_a[2].angle() - inst_b[2].angle()).abs() > 1.0,
        "L1 本体的角度应随旋转改变"
    );
    assert_ne!(inst_a[3].center(), inst_b[3].center(), "L1 子音符的位置应随旋转改变");
}
#[test]
fn alpha_zero_line_is_not_emitted() {
    let mut doc = two_line_doc(0.0);
    // 把 L1 的 alpha 事件改成 0
    let l1 = &mut doc.judge_lines[1];
    let a = l1.layers[0].track_mut("alpha").unwrap();
    a[0].start_value = json!(0.0);
    a[0].end_value = json!(0.0);
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.lookahead = 2.0;
    st.selected_line = usize::MAX;
    let mut inst = Vec::new();
    build_instances(&st, &mut inst);
    // 只剩 L0 的 2 个实例（本体 + 1 个可见子音符）
    assert_eq!(inst.len(), 2, "alpha=0 的线与它的子音符都不该上报实例");
}
/// **下落速度必须与 RPE 一致**（用户要求"保持和RPE一致流速"）。
///
/// RPE 规范：1 单位流速 = 120 RPE y 单位/秒，判定线默认流速 10 ⇒ 1× = 1200 单位/秒，
/// 也就是 0.75 秒划过整个 900 高的窗口。这条直接量**画出来的实例**在哪 ——
/// 公式对不对不该只活在注释里。
#[test]
fn notes_fall_at_the_rpe_speed() {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry {
        start: Beat::zero(),
        bpm: 120.0, // 一拍 0.5 秒，手算方便
        foreign: Default::default(),
    }];
    doc.judge_lines.clear();
    let mut l = JudgeLine::default();
    // 流速恒为 10（RPE 新建判定线的默认值 = 1×）
    l.layers[0].track_mut("speed").unwrap().push(Event::new(
        Beat::zero(),
        Beat::new(64, 1),
        json!(10.0),
        json!(10.0),
        "linear",
    ));
    // 0.2 / 0.6 / 1.0 拍 ⇒ 0.1 / 0.3 / 0.5 秒后打击（120 BPM：一拍 0.5 秒）
    for b in [(1, 5), (3, 5), (1, 1)] {
        l.notes.push(DocNote::new(DocKind::Tap, Beat::new(b.0, b.1), 0.0));
    }
    doc.judge_lines.push(l);
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.selected_line = usize::MAX;
    let mut inst = Vec::new();
    build_instances(&st, &mut inst);
    // 只上报"还在窗口里"的：0.1 s → 120 单位 ✓；0.3 s → 360 ✓；0.5 s → 600 ⇒ 已出窗口
    assert_eq!(inst.len(), 1 + 2, "1 条线本体 + 2 个可见音符（第 3 个已飞出窗口）");
    let mut ys: Vec<f32> = inst[1..].iter().map(|i| i.center()[1]).collect();
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert!((ys[0] - 120.0).abs() < 1e-3, "0.1 秒后打击的音符应在 120 单位处，实际 {}", ys[0]);
    assert!((ys[1] - 360.0).abs() < 1e-3, "0.3 秒 ⇒ 360 单位，实际 {}", ys[1]);
    // 逐条对上：0.1 s × 10（流速）× 120 = 120 单位/秒
    for (k, dt) in [0.1_f64, 0.3].iter().enumerate() {
        let want = (dt * 10.0 * opm_app::perf::SPEED_UNITS_PER_SEC) as f32;
        assert!((ys[k] - want).abs() < 1e-3, "dt={dt}: {want} vs {}", ys[k]);
    }
    // 把流速改成 20 ⇒ 同样的时刻跑两倍远：0.1 s 应在 240，0.3 s 已出窗口
    let l = &mut doc.judge_lines[0];
    let sp = l.layers[0].track_mut("speed").unwrap();
    sp[0].start_value = json!(20.0);
    sp[0].end_value = json!(20.0);
    let mut st2 = EditorState::new(chart_from_doc(&doc));
    st2.selected_line = usize::MAX;
    let mut inst2 = Vec::new();
    build_instances(&st2, &mut inst2);
    assert_eq!(inst2.len(), 2, "流速翻倍 ⇒ 只剩最近那个还在窗口里");
    assert!((inst2[1].center()[1] - 240.0).abs() < 1e-3, "{}", inst2[1].center()[1]);
}

/// 音符自带的 `speed` 乘在**离判定线的距离**上（RPE 规范：不改到达时刻，只改落多远）
#[test]
fn a_note_speed_multiplies_its_distance() {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry {
        start: Beat::zero(),
        bpm: 120.0,
        foreign: Default::default(),
    }];
    doc.judge_lines.clear();
    let mut l = JudgeLine::default();
    l.layers[0].track_mut("speed").unwrap().push(Event::new(
        Beat::zero(),
        Beat::new(64, 1),
        json!(10.0),
        json!(10.0),
        "linear",
    ));
    let mut n = DocNote::new(DocKind::Tap, Beat::new(1, 2), 0.0); // 0.5 拍 = 0.25 s
    n.speed = 3.0;
    l.notes.push(n);
    doc.judge_lines.push(l);
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.selected_line = usize::MAX;
    let mut inst = Vec::new();
    build_instances(&st, &mut inst);
    // 0.25 s × 1200 单位/秒 × 3 = 900 —— 已经出窗口了（这正是"乘距离"的直接后果）
    assert_eq!(inst.len(), 1, "speed=3 的音符 0.25 秒就跑出窗口了");
}

/// 一份"一条线 + 若干音符"的谱面，流速恒 10（RPE 默认 = 1× = 1200 单位/秒），BPM 120（一拍 0.5 秒）。
fn one_line_doc(notes: &[(DocKind, f64, Option<f64>, bool)]) -> Document {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry {
        start: Beat::zero(),
        bpm: 120.0,
        foreign: Default::default(),
    }];
    doc.judge_lines.clear();
    let mut l = JudgeLine::default();
    l.layers[0].track_mut("speed").unwrap().push(Event::new(
        Beat::zero(),
        Beat::new(4096, 1),
        json!(10.0),
        json!(10.0),
        "linear",
    ));
    for (kind, start, end, fake) in notes {
        let mut n = DocNote::new(*kind, Beat::new((start * 4.0) as i64, 4), 0.0);
        n.end = end.map(|e| Beat::new((e * 4.0) as i64, 4));
        n.is_fake = *fake;
        l.notes.push(n);
    }
    doc.judge_lines.push(l);
    doc
}

/// 白色闪光的实例数（击中效果的探针：闪光就是纯白，音符/判定线都不是）
fn flash_count(inst: &[NoteInstance]) -> usize {
    inst.iter()
        .filter(|i| i.color()[0] > 0.99 && i.color()[1] > 0.99 && i.color()[2] > 0.99)
        .count()
}

/// Tap 音符本体的实例数（按 `NoteKind::Tap` 的颜色认 —— 击中效果的环是"提亮过的"颜色，认不出来）
fn tap_quad_count(inst: &[NoteInstance]) -> usize {
    inst.iter()
        .filter(|i| {
            let c = i.color();
            (c[0] - 0.35).abs() < 0.02 && (c[1] - 0.65).abs() < 0.02 && (c[2] - 1.0).abs() < 0.02
        })
        .count()
}

/// **音符到达判定线后出现击中效果并消失**（用户要求）：
/// 到线前正常下落 → 到线那一下闪一次（且音符停在线上收缩）→ 收缩完音符没了、闪光还在 → 效果也结束。
#[test]
fn a_note_hits_the_judge_line_then_vanishes() {
    use opm_app::render::{HIT_FADE_SEC, HIT_FX_SEC};
    let doc = one_line_doc(&[(DocKind::Tap, 2.0, None, false)]); // 2 拍 = 1.0 秒
    let t_hit = 1.0_f64;
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.selected_line = usize::MAX;

    let mut count_at = |t: f64| {
        st.playhead = t;
        let mut inst = Vec::new();
        build_instances(&st, &mut inst);
        inst
    };

    // 到线之前：只有"线本体 + 音符"，没有闪光，而且**音符是它本来的大小与亮度**
    // （这里钉的是一个真 bug：消失进度没夹到 0 ⇒ 未来的音符被放大到 2.8 倍、alpha ×5.5）
    let before = count_at(t_hit - 0.1);
    assert_eq!(before.len(), 2, "到线前 = 线本体 + 1 个音符");
    assert_eq!(flash_count(&before), 0, "还没到线，不该有击中效果");
    let fresh = before.iter().find(|i| i.half()[0] < 100.0).expect("音符本体");
    assert!(
        (fresh.half()[0] - 46.0).abs() < 1e-3 && (fresh.half()[1] - 13.0).abs() < 1e-3,
        "还没到线的音符应保持原尺寸，实际 {:?}",
        fresh.half()
    );
    assert!(
        (fresh.color()[3] - 1.0).abs() < 1e-3,
        "还没到线的音符 alpha 应为 1，实际 {}",
        fresh.color()[3]
    );

    // 到线那一下：音符还在（停在线上收缩）+ 闪光出现（1 白闪 + 4 条扩散边）
    let at = count_at(t_hit);
    assert_eq!(flash_count(&at), 1, "到线当帧应闪一下");
    assert!(at.len() >= 1 + 5, "线本体 + 音符 + 5 个效果实例，实际 {}", at.len());
    let note_quad = at
        .iter()
        .find(|i| i.color()[0] < 0.99 && i.half()[0] > 3.0)
        .expect("音符本体");
    assert!(
        (note_quad.center()[1] - 0.0).abs() < 1e-3,
        "到线之后音符应**停在判定线上**，而不是继续往下掉：{}",
        note_quad.center()[1]
    );

    // 收缩完：音符本体没了，闪光还在
    let after_fade = count_at(t_hit + HIT_FADE_SEC + 0.01);
    assert_eq!(flash_count(&after_fade), 1, "闪光还在场上");
    assert_eq!(tap_quad_count(&after_fade), 0, "音符本体应该在收缩结束之后消失");

    // 效果结束：只剩判定线本体
    let after_fx = count_at(t_hit + HIT_FX_SEC + 0.01);
    assert_eq!(after_fx.len(), 1, "效果结束、音符也没了 ⇒ 只剩线本体");
    assert_eq!(flash_count(&after_fx), 0);
}

/// **hold 击中后立即播一次，之后每 3 拍再播一次**（用户要求）
#[test]
fn a_hold_pulses_a_hit_effect_every_three_beats() {
    use opm_app::render::{HIT_FX_SEC, HOLD_PULSE_BEATS};
    // 0 拍起、12 拍止（BPM 120 ⇒ 一拍 0.5 秒 ⇒ 0..6 秒）
    let doc = one_line_doc(&[(DocKind::Hold, 0.0, Some(12.0), false)]);
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.selected_line = usize::MAX;
    let at = |st: &mut EditorState, t: f64| {
        st.playhead = t;
        let mut inst = Vec::new();
        build_instances(st, &mut inst);
        flash_count(&inst)
    };
    // 击中当帧**立即**闪一次
    assert_eq!(at(&mut st, 0.0), 1, "hold 击中后应立即播一次击中效果");
    // 半个脉冲之后不闪（效果早已结束）
    assert_eq!(at(&mut st, 0.5 * HOLD_PULSE_BEATS - HIT_FX_SEC - 0.02), 0);
    // 第 3、6、9 拍各闪一次（都在 hold 之内）
    for beats in [3.0, 6.0, 9.0] {
        let t = beats * 0.5;
        assert_eq!(at(&mut st, t), 1, "{beats} 拍处应再闪一次");
        assert_eq!(at(&mut st, t + HIT_FX_SEC + 0.01), 0, "{beats} 拍的效果应结束");
    }
    // 尾巴之后不再闪
    assert_eq!(at(&mut st, 6.5), 0, "hold 结束后不该再有脉冲");
    assert_eq!(at(&mut st, 12.0), 0, "12 拍（超出一个脉冲）也不该有");
}

/// 按住期间**被吃掉的那一段不再画**：hold 的身子从判定线起算，而不是从头起算
#[test]
fn a_held_hold_body_starts_at_the_judge_line() {
    let doc = one_line_doc(&[(DocKind::Hold, 0.0, Some(8.0), false)]); // 0..4 秒
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.selected_line = usize::MAX;
    // 1 秒时：头（0 秒）已经过去 1 秒 ⇒ 身子应从判定线（本地 y=0）起算
    st.playhead = 1.0;
    let mut inst = Vec::new();
    build_instances(&st, &mut inst);
    // 身子那块的半高 = 剩余长度的一半，中心在剩余长度的一半处 ⇒ 下端正好落在 y=0
    let body = inst
        .iter()
        .find(|i| i.half()[1] > 20.0)
        .expect("hold 的身子");
    let bottom = body.center()[1] - body.half()[1];
    assert!(bottom.abs() < 1.0, "身子下端应贴着判定线，实际 {bottom}");
}

/// **长 hold 在头被击中之后仍然可见**（`visible_range_of` 以前只按时间回退 0.15s ⇒
/// 身子还在窗口里、却整条不再上报实例 —— 自动播放时表现为"长条一到线就消失"）
#[test]
fn a_long_hold_stays_visible_after_its_head_is_hit() {
    // 0 拍起、40 拍止（20 秒）：播放头在 10 秒，头早已过去、身子还有 10 秒
    let doc = one_line_doc(&[(DocKind::Hold, 0.0, Some(40.0), false)]);
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.selected_line = usize::MAX;
    st.playhead = 10.0;
    let mut inst = Vec::new();
    build_instances(&st, &mut inst);
    assert!(
        inst.iter().any(|i| i.half()[1] > 20.0),
        "被按住的那一段身子必须还在画：{:?}",
        inst.iter().map(|i| i.half()).collect::<Vec<_>>()
    );
}

/// **假音符没有击中效果**（它没有判定，游戏里也不会闪）
#[test]
fn a_fake_note_does_not_flash() {
    let doc = one_line_doc(&[(DocKind::Tap, 2.0, None, true)]);
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.selected_line = usize::MAX;
    st.playhead = 1.0; // 正好是打击时刻
    let mut inst = Vec::new();
    build_instances(&st, &mut inst);
    assert_eq!(flash_count(&inst), 0, "假音符不该有击中效果");
}

#[test]
fn track_and_curve_are_cached_per_line() {
    let doc = two_line_doc(45.0);
    let chart = chart_from_doc(&doc);
    for l in &chart.lines {
        let rot = l.track(TrackId::Rotate);
        assert_eq!(rot.events.len(), 1);
        // 采样折线：1 个事件 ×(4+1) 点
        assert_eq!(rot.curve.len(), 5);
        // L1 是 45° 常量、L0 是 0° 常量 —— 逐线各查各的（别拿 L0 去比 45）
        let want = if l.name == "L1" { 45.0 } else { 0.0 };
        assert!((rot.min - want).abs() < 1e-6, "{} 的 rotate 下界应为 {want}", l.name);
        assert!((rot.max - want).abs() < 1e-6, "{} 的 rotate 上界应为 {want}", l.name);
        assert_eq!(l.track(TrackId::MoveX).events.len(), 1);
    }
}
#[test]
fn lines_report_exposes_event_values_for_agents() {
    let doc = two_line_doc(90.0);
    let r = opm_app::headless::lines_report(&doc, 0.0);
    assert_eq!(r["lines"].as_array().unwrap().len(), 2);
    let l1 = &r["lines"][1];
    assert_eq!(l1["name"], json!("L1"));
    assert_eq!(l1["notes"], json!(2));
    assert!((l1["perf"]["rotate"].as_f64().unwrap() - 90.0).abs() < 1e-6);
    assert!((l1["perf"]["moveY"].as_f64().unwrap() - 300.0).abs() < 1e-6);
    // 轨道明细：agent 据此断言"事件确实存在且在此刻取到这个值"
    let tracks = l1["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 5);
    assert_eq!(tracks[2]["track"], json!("rotate"));
    assert_eq!(tracks[2]["events"], json!(1));
    assert!((tracks[2]["valueAt"].as_f64().unwrap() - 90.0).abs() < 1e-6);
}
#[test]
fn rotate_event_is_time_varying() {
    let mut doc = two_line_doc(0.0);
    // 把 L1 的 rotate 换成 0→90 的动画（64 拍线性）
    let rot = doc.judge_lines[1].layers[0].track_mut("rotate").unwrap();
    rot[0].end_value = json!(90.0);
    let chart = chart_from_doc(&doc);
    let tmap = chart.tmap.clone();
    let l1 = chart.lines.iter().find(|l| l.name == "L1").unwrap().clone();

    let t0 = l1.perf(&tmap, 0.0).rotate_deg;
    let t_mid = l1.perf(&tmap, tmap.sec(32.0)).rotate_deg;
    let t_end = l1.perf(&tmap, tmap.sec(64.0)).rotate_deg;
    assert!((t0 - 0.0).abs() < 1e-6, "起点应为 0，实际 {t0}");
    assert!((t_mid - 45.0).abs() < 1e-3, "线性中点应为 45，实际 {t_mid}");
    assert!((t_end - 90.0).abs() < 1e-6, "终点应为 90，实际 {t_end}");

    // 换缓动后中点必须变（证明求值用的是事件自己的缓动）
    let mut doc2 = two_line_doc(0.0);
    let rot2 = doc2.judge_lines[1].layers[0].track_mut("rotate").unwrap();
    rot2[0].end_value = json!(90.0);
    rot2[0].easing = "outCubic".into();
    let c2 = chart_from_doc(&doc2);
    let tm2 = c2.tmap.clone();
    let l2 = c2.lines.iter().find(|l| l.name == "L1").unwrap().clone();
    let mid2 = l2.perf(&tm2, tm2.sec(32.0)).rotate_deg;
    assert!((mid2 - 78.75).abs() < 1e-3, "outCubic 中点应为 78.75，实际 {mid2}");
}
#[test]
fn window_boundary_marks_rpe_extent() {
    use opm_app::render::push_window_overlay;
    use opm_app::state::{RPE_WINDOW_HALF_H, RPE_WINDOW_HALF_W};

    let doc = two_line_doc(0.0);
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.lookahead = 2.0;
    st.selected_line = usize::MAX;
    st.show_boundary = true;
    st.boundary_dim = 0.42;

    // 先单独构建"内容"，才能断言覆盖层的绘制顺序（内容 → 压暗 → 边框）
    let mut content = Vec::new();
    build_instances(&st, &mut content);
    let n_content = content.len();
    let mut on = content.clone();
    push_window_overlay(&st, &mut on);

    // 4 条边必须正好落在 RPE 的边界上（±675 / ±450），厚度是细边
    let edge_at = |cx: f32, cy: f32| {
        on.iter().any(|i| {
            (i.center()[0] - cx).abs() < 1e-3
                && (i.center()[1] - cy).abs() < 1e-3
                && (i.half()[1] <= 3.0 || i.half()[0] <= 3.0)
        })
    };
    assert!(edge_at(0.0, RPE_WINDOW_HALF_H), "缺上边");
    assert!(edge_at(0.0, -RPE_WINDOW_HALF_H), "缺下边");
    assert!(edge_at(RPE_WINDOW_HALF_W, 0.0), "缺右边");
    assert!(edge_at(-RPE_WINDOW_HALF_W, 0.0), "缺左边");

    // 边界外压暗：4 个大块，且必须**晚于内容**（否则盖不住窗口外的东西）
    let is_dim = |i: &opm_app::render::NoteInstance| {
        i.color()[3] > 0.3 && i.color()[0] < 0.01 && i.half()[0] > 1000.0
    };
    let dim_idx: Vec<usize> = (n_content..on.len())
        .filter(|k| is_dim(&on[*k]))
        .collect();
    assert_eq!(dim_idx.len(), 4, "压暗带应为 4 块");
    assert!(
        dim_idx.iter().all(|k| *k >= n_content),
        "压暗带必须画在所有内容之后（下标应 >= 内容长度 {n_content}）"
    );
    // 边框必须在压暗之后：最后 12 个实例是 4 边 + 8 角标，且不被压暗盖住
    let dim_max = *dim_idx.iter().max().unwrap();
    assert_eq!(on.len() - dim_max - 1, 12, "压暗之后应恰好剩 4 边 + 8 角标");

    // 关掉开关：这些实例一个都不该出现
    st.show_boundary = false;
    let mut off = Vec::new();
    build_instances(&st, &mut off);
    push_window_overlay(&st, &mut off);
    assert_eq!(on.len() - off.len(), 16, "关掉边界后应少 4 边 + 8 角标 + 4 压暗 = 16 个实例");
}
#[test]
fn line_length_is_an_editable_setting() {
    // 判定线长度在 RPE 格式里没有字段，是编辑器设置 —— 改它只影响线本体，不动子音符
    let doc = two_line_doc(0.0);
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.lookahead = 2.0;
    st.selected_line = usize::MAX;
    st.show_boundary = false;
    st.line_half_w = 500.0;

    let mut inst = Vec::new();
    build_instances(&st, &mut inst);
    let body = inst
        .iter()
        .find(|i| (i.half()[0] - 500.0).abs() < 1e-3)
        .expect("线本体应按设置的长度绘制");
    assert!((body.half()[0] * 2.0 - 1000.0).abs() < 1e-3, "线长 = 2×半长");
    // 音符实例不受线长影响（音符位置只由 lane_x 与时间决定）
    let n_before = inst.iter().filter(|i| i.half()[1] < 20.0).count();
    st.line_half_w = 200.0;
    let mut inst2 = Vec::new();
    build_instances(&st, &mut inst2);
    let n_after = inst2.iter().filter(|i| i.half()[1] < 20.0).count();
    assert_eq!(n_before, n_after, "改线长不该增减音符实例");
}
/// 编辑区叠加层的可见性规则：**自动播放中或按住 H 时隐藏**。
///
/// 规则很窄，但正是容易在重构里被改坏的那种（"播放时也显示吧"会让预览看不干净）。
#[test]
fn overlay_hides_while_playing_or_holding_h() {
    // 与 src/overlay.rs::overlay_visible 同一套规则
    let f = |enabled: bool, playing: bool, h: bool| enabled && !playing && !h;
    assert!(f(true, false, false), "暂停且没按 H ⇒ 显示");
    assert!(!f(true, true, false), "自动播放中 ⇒ 隐藏");
    assert!(!f(true, false, true), "按住 H ⇒ 隐藏");
    assert!(!f(true, true, true), "播放中按 H ⇒ 仍隐藏");
    assert!(!f(false, false, false), "用户在工具栏关掉 ⇒ 不显示");
}
/// 编辑区滚轮：位移 → 拍增量的换算（含"向上滚 = 时间往后"与随缩放的步长）。
///
/// 滚轮事件没法在本会话注入 Wayland 窗口（xdotool 需要 DISPLAY），所以把换算抽成纯函数单测；
/// **施加路径**用等价的 `{"op":"nudge","beats":N}` 端到端验证（二者是同一个动作）。
#[test]
fn overlay_scroll_maps_to_beats() {
    // 与 src/overlay.rs::scroll_delta_to_beats 同一套规则
    let f = |dy: f32, beats: f64, per_notch: f64| {
        (dy as f64 / 50.0) * per_notch * (beats / 32.0).max(0.15)
    };
    // 一格（+50）= 默认 2 拍；向上滚为正 ⇒ 时间往后
    assert!((f(50.0, 32.0, 2.0) - 2.0).abs() < 1e-9);
    assert!(f(-50.0, 32.0, 2.0) < 0.0, "向下滚应回到更早");
    assert_eq!(f(0.0, 32.0, 2.0), 0.0, "没有滚动就不该动播放头");
    // 步长随可见拍数缩放：放大（8 拍可见）时一格只走 0.5 拍，缩小（256 拍）时走 16 拍
    assert!((f(50.0, 8.0, 2.0) - 0.5).abs() < 1e-9);
    assert!((f(50.0, 256.0, 2.0) - 16.0).abs() < 1e-9);
    // 极端缩放下也不至于一格跳出谱面（下限 0.15，不做归零以免"滚不动"）
    assert!(f(50.0, 0.0, 2.0) > 0.0);
}
/// **横向（laneX）步长只由 `lane_div` 决定，与节拍细分无关。**
///
/// 这条断言是回答"横轴间还有四等分吸附吗"的：早先的公式曾是 `1350/(beat_div×4)`
/// （把横向步长绑在节拍细分上、还带一个 ×4），换成 `1350/lane_div` 之后必须钉住 ——
/// 否则哪天有人改回去，"横向跟着节拍走"这种怪现象会悄悄回来。
#[test]
fn lane_step_is_independent_of_beat_subdivision() {
    use opm_app::state::GridCfg;
    for beat_div in [1u32, 2, 3, 4, 8, 16] {
        let g = GridCfg { beat_div, lane_div: 16 };
        assert!(
            (g.h_step_rpe() - 1350.0 / 16.0).abs() < 1e-6,
            "横向步长不该随节拍细分变化（beat_div={beat_div} 时得到 {}）",
            g.h_step_rpe()
        );
        // 横向吸附落点也必须与节拍细分无关
        assert!((g.snap_lane(100.0) - 84.375).abs() < 1e-4);
    }
    // 反过来：纵向步长只跟 beat_div 走
    for lane_div in [4u32, 5, 7, 16, 32] {
        let g = GridCfg { beat_div: 4, lane_div };
        assert!((g.v_step_beats() - 0.25).abs() < 1e-12);
        assert!((g.snap_beat(0.3) - 0.25).abs() < 1e-12);
    }
    // **奇偶都能用**（用户要求"写几等分就整窗平均切几份"）：
    // 格点是**边界对齐**的 `-675 + k·(1350/N)`，端点恒为格线，所以奇数不会再"放不进去"。
    for n in [1u32, 2, 3, 4, 5, 7, 16, 17, 128] {
        let g = GridCfg { beat_div: 4, lane_div: n };
        let step = 1350.0 / n as f32;
        assert!((g.h_step_rpe() - step).abs() < 1e-3, "N={n} 步长应为 1350/N");
        // N+1 条格线，第一条与最后一条正好是窗口边界，全部落在窗口内
        assert!((g.lane_of_index(0) + 675.0).abs() < 1e-3, "N={n} 起点应是 -675");
        assert!((g.lane_of_index(n as i32) - 675.0).abs() < 1e-3, "N={n} 终点应是 +675");
        for k in 0..=n as i32 {
            let lane = g.lane_of_index(k);
            assert!(lane.abs() <= 675.0 + 1e-3, "N={n} 第 {k} 条越界：{lane}");
            // 每条格线都是吸附定点（"吸附就是格点"）
            assert!(
                (g.snap_lane(lane) - lane).abs() < 1e-3,
                "N={n} 第 {k} 条（{lane}）不是吸附定点"
            );
        }
        // 窗口边界永远是格点（这条正是旧算法在奇数下做不到的）
        assert!((g.snap_lane(675.0) - 675.0).abs() < 1e-3);
        assert!((g.snap_lane(-675.0) + 675.0).abs() < 1e-3);
        // 吸附结果永不越界（laneX 超出 ±675 是格式不允许的）
        for probe in [-900.0_f32, -676.0, 676.0, 900.0, 0.0] {
            let v = g.snap_lane(probe);
            assert!(v.abs() <= 675.0 + 1e-3, "{probe} 吸附后越界：{v}");
            // 吸附结果必定是某条格线
            let dup = (v + 675.0) / step;
            assert!((dup - dup.round()).abs() < 1e-3, "{probe} 吸附到 {v}，不在格点上");
        }
        // 中轴是不是格点 = 等分数的奇偶（**奇数时中轴不是格点**，但中轴线照画 —— 见 overlay）
        assert_eq!(g.center_is_lattice(), n % 2 == 0, "N={n} 的中轴格点性判错了");
    }
    // 规整只做范围夹取，**不改奇偶**（下限是 1：只有两条边线，即"不切"）
    assert_eq!(GridCfg::normalize_lane_div(1), 1);
    assert_eq!(GridCfg::normalize_lane_div(3), 3, "奇数不该被改成偶数");
    assert_eq!(GridCfg::normalize_lane_div(200), 128);
    assert_eq!(GridCfg::normalize_lane_div(0), 1);
}

/// 网格：两个方向各一个数 —— 拍方向"每拍 N 条"、坐标方向"窗口 N 等分"。
///
/// 关键性质：**拖拽/放置只能落在两轴网格的交叉点**（界面里不存在"格点之间的音符"），
/// 而吸附还保证写回文档的拍是**有理数 k/N**，不是浮点抖出来的 0.3333333。
#[test]
fn grid_is_a_lattice_and_snapping_lands_on_it() {
    use opm_app::state::GridCfg;
    let g = GridCfg { beat_div: 4, lane_div: 16 };

    // 拍方向：每拍 4 条 ⇒ 步长 1/4 拍
    assert!((g.v_step_beats() - 0.25).abs() < 1e-12);
    assert!((g.snap_beat(0.30) - 0.25).abs() < 1e-12);
    assert!((g.snap_beat(0.40) - 0.50).abs() < 1e-12);

    // 坐标方向：窗口 16 等分 ⇒ 步长 1350/16 = 84.375 RPE
    let step = g.h_step_rpe();
    assert!((step - 1350.0 / 16.0).abs() < 1e-6, "step={step}");
    assert!((g.snap_lane(50.0) - step).abs() < 1e-4);
    assert!((g.snap_lane(-300.0) + 337.5).abs() < 1e-4);

    // **落在格点上**：格点 = -675 + k·step（**边界对齐**，不是步长的整数倍 ——
    // 旧算法以 0 为中心才是"整数倍"，那套在奇数等分下放不下，已改）
    for lane in [-640.0_f32, -123.4, 0.0, 7.9, 511.0] {
        let s = g.snap_lane(lane);
        let k = (s + 675.0) / step;
        assert!((k - k.round()).abs() < 1e-3, "{lane} 吸附后 {s} 不在格点上（k={k}）");
        assert!(s.abs() <= 675.0 + 1e-3, "格点不该超出窗口");
    }
    // 奇数等分（5）：格点贴满窗口两端，中轴不是格点但吸附照样落在格点上
    let g5 = GridCfg { beat_div: 4, lane_div: 5 };
    let s5 = 1350.0 / 5.0;
    assert!((g5.lane_of_index(0) + 675.0).abs() < 1e-3);
    assert!((g5.lane_of_index(5) - 675.0).abs() < 1e-3);
    assert!(!g5.center_is_lattice());
    let snapped0 = g5.snap_lane(0.0);
    assert!((snapped0 + 675.0) % s5 < 1e-3 || (s5 - (snapped0 + 675.0) % s5) < 1e-3, "0 吸附后 {snapped0} 不在格点");
    assert!((snapped0 - 135.0).abs() < 1e-3, "5 等分下 0 最近（半格处取上）应吸到 135，得到 {snapped0}");
    for beat in [0.0_f64, 0.19, 1.6, 7.33, 32.0] {
        let b = g.snap_beat(beat);
        let k = b * 4.0;
        assert!((k - k.round()).abs() < 1e-9, "{beat} 吸附后 {b} 不在格点上");
        assert!(b >= 0.0);
    }

    // 写回文档用有理数：1/3 拍网格下 1/3 → [1,3]；1/4 网格下 0.25 → [1,4]
    let g3 = GridCfg { beat_div: 3, lane_div: 4 };
    assert_eq!(g3.beat_json(g3.snap_beat(1.0 / 3.0)), [1, 3]);
    assert_eq!(g.beat_json(g.snap_beat(0.25)), [1, 4]);
    // 换一个细分，格点跟着换（"可指定"）
    let g8 = GridCfg { beat_div: 8, lane_div: 32 };
    assert!((g8.v_step_beats() - 0.125).abs() < 1e-12);
    assert!((g8.h_step_rpe() - 1350.0 / 32.0).abs() < 1e-6);
}


/// **窗口 X 偏移下的吸附**：格点锚在官方坐标系上、向显示窗口两侧延伸，
/// 所以平移之后既能编辑窗口外的坐标，又不会"吸附结果不在格点上"。
#[test]
fn window_offset_extends_the_lattice_beyond_the_window() {
    use opm_app::state::GridCfg;
    let g = GridCfg { beat_div: 4, lane_div: 16 };
    let step = g.h_step_rpe();

    // 偏移 +675：显示 0…1350 ⇒ 右半边的格点在官方窗口之外
    let (k0, k1) = g.lane_index_range(675.0);
    assert_eq!(k0, 8, "偏移 +675 时可见格点应从 k=8 起（laneX=0）");
    assert_eq!(k1, 24, "…到 k=24（laneX=1350）");
    for k in k0..=k1 {
        let lane = g.lane_of_index(k);
        let snapped = g.snap_lane_windowed(lane, 675.0);
        assert!((snapped - lane).abs() < 1e-3, "偏移下 {lane} 应吸回自身，得到 {snapped}");
        assert!(lane >= -1e-3 && lane <= 1350.0 + 1e-3);
    }
    // 越界坐标确实能吸到（不被拉回 ±675）
    let beyond = g.snap_lane_windowed(1000.0, 675.0);
    assert!(beyond > 675.0, "1000 应吸到窗口外的格点，得到 {beyond}");
    assert!(((beyond + 675.0) / step - ((beyond + 675.0) / step).round()).abs() < 1e-3);
    // 偏移 0（默认）时行为不变：吸附永远落在官方窗口内
    for probe in [-900.0_f32, -676.0, 676.0, 900.0] {
        let v = g.snap_lane_windowed(probe, 0.0);
        assert!(v.abs() <= 675.0 + 1e-3, "偏移 0 时 {probe} 不该越界：{v}");
    }
    // 显示窗口比一格还窄的极端情况也不 panic（夹取区间反了就取左端）
    let wide = GridCfg { beat_div: 4, lane_div: 128 };
    let v = wide.snap_lane_windowed(0.0, 675.0);
    assert!(v.is_finite());
}

/// 一份"一条线 + 给定流速 + 给定音符"的谱面（BPM 120 ⇒ 一拍 0.5 秒）
fn one_line_doc_speed(speed: f64, notes: &[(DocKind, f64, Option<f64>, f32)]) -> Document {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry {
        start: Beat::zero(),
        bpm: 120.0,
        foreign: Default::default(),
    }];
    doc.judge_lines.clear();
    let mut l = JudgeLine::default();
    l.layers[0].track_mut("speed").unwrap().push(Event::new(
        Beat::zero(),
        Beat::new(4096, 1),
        json!(speed),
        json!(speed),
        "linear",
    ));
    for (kind, start, end, lane) in notes {
        let mut n = DocNote::new(*kind, Beat::new((start * 4.0) as i64, 4), *lane);
        n.end = end.map(|e| Beat::new((e * 4.0) as i64, 4));
        l.notes.push(n);
    }
    doc.judge_lines.push(l);
    doc
}

/// **判定线默认长度 = 3000**（用户要求；渲染按半长画，长度是编辑器设置而不是格式字段）
#[test]
fn the_default_judge_line_is_3000_long() {
    let st = EditorState::new(chart_from_doc(&Document::default()));
    assert_eq!(
        st.line_half_w * 2.0,
        opm_app::state::RPE_LINE_LEN_DEFAULT,
        "默认线长应等于这个常量"
    );
    assert_eq!(opm_app::state::RPE_LINE_LEN_DEFAULT, 3000.0);
    // 三个入口用的是**同一个常量**（默认值不许各写各的）
    assert_eq!(opm_app::cli::Args::default().line_len, 3000.0, "CLI 默认");
    // 无头出图默认也走它
    assert_eq!(
        opm_app::headless::RenderOpts::default().line_len,
        3000.0,
        "无头默认"
    );
}

/// **每一颗音符都必须在它该看得见的时候被画出来、且完整落在窗口里**（不被裁掉）。
///
/// 这条盯的是一个真 bug：实例构建窗口曾经是**固定 2 秒前瞻**，而音符进入窗口的时刻由流速决定
/// —— 流速 1（0.1×）时音符是 4.25 秒之前进画面，于是 2 秒之外那些**本该看得见**的音符
/// 整颗没有实例（实测：流速 1、播放头在 0 时，3 秒处那颗在窗口内的音符没有被画）。
#[test]
fn every_note_becomes_visible_inside_the_window_before_its_hit() {
    for speed in [10.0_f64, 3.0, 1.0, 0.5] {
        // 五颗音符，横向铺满"完整落在窗口内"的范围（±675 减去一个音符的半宽）
        let lanes = [-600.0_f32, -300.0, 0.0, 300.0, 600.0];
        let notes: Vec<(DocKind, f64, Option<f64>, f32)> = lanes
            .iter()
            .enumerate()
            .map(|(k, lane)| (DocKind::Tap, 2.0 + 4.0 * k as f64, None, *lane))
            .collect();
        let doc = one_line_doc_speed(speed, &notes);
        let mut st = EditorState::new(chart_from_doc(&doc));
        st.selected_line = usize::MAX;
        // 音符从窗口边缘走到判定线要多久（+ 一点余量）
        let span = 450.0 / (opm_app::perf::SPEED_UNITS_PER_SEC * speed) + 0.2;

        for (k, lane) in lanes.iter().enumerate() {
            let t_hit = (2.0 + 4.0 * k as f64) * 0.5;
            let mut well_inside = 0usize;
            let mut far_outside = 0usize;
            let mut quad_outside = 0usize;
            for step in 0..=80 {
                st.playhead = t_hit - span * 1.2 + span * 1.2 * step as f64 / 80.0;
                let mut inst = Vec::new();
                build_instances(&st, &mut inst);
                // 只认**音符本体**：判定线本体（半长 1500，故意伸出窗口）与击中效果的方框
                // 颜色/尺寸都不同，按 Tap 的颜色 + 中心横向位置挑出来
                let is_this_note = |q: &NoteInstance| {
                    let c = q.color();
                    (q.center()[0] - lane).abs() < 1.0
                        && (c[0] - 0.35).abs() < 0.02
                        && (c[1] - 0.65).abs() < 0.02
                        && (c[2] - 1.0).abs() < 0.02
                };
                for q in inst.iter().filter(|q| is_this_note(q)) {
                    // 中心**稳稳地在窗口里**（离边缘还有 30 单位）⇒ 这一帧它是真的看得见
                    if q.center()[1].abs() <= 450.0 - 30.0 {
                        well_inside += 1;
                    }
                    // 构建窗口留了 `NOTE_SPAN_MARGIN` 的余量：贴边滑入的那几帧允许中心越界，
                    // 但**不许**超出余量（那才是"建了实例又看不见"的浪费/漏画）
                    // 判据是"屏幕包围盒与窗口相交"，方块旋转后外接半径最多 ~48，
                    // 所以允许中心超出窗口 `NOTE_SPAN_MARGIN + 60`（再多就是在白建实例了）
                    if q.center()[1].abs()
                        > 450.0 + opm_app::state::EditorState::NOTE_SPAN_MARGIN + 60.0 + 1.0
                    {
                        far_outside += 1;
                    }
                    // 横向也要完整落在窗口里（这几条 laneX 都在"放得下"的范围内）
                    if q.center()[0].abs() + q.half()[0] > 675.0 + 1e-3 {
                        quad_outside += 1;
                    }
                }
            }
            assert!(
                well_inside > 0,
                "流速 {speed}：laneX={lane} 的音符从没完整地出现在窗口里"
            );
            assert_eq!(far_outside, 0, "流速 {speed}：laneX={lane} 的音符被建到了余量之外");
            assert_eq!(quad_outside, 0, "流速 {speed}：laneX={lane} 的音符横向被裁了");
        }
    }
}

/// **负流速**：音符从判定线**下方**飞上来 —— 但"判定线之下不显示"（用户口径），
/// 所以到线之前一颗都不画；到线那一刻**击中效果照旧**（否则负流速段完全没有反馈），
/// 音符本体随后停在判定线上收缩消失。
#[test]
fn a_negative_flow_speed_draws_nothing_below_the_line_but_still_flashes() {
    use opm_app::render::{HIT_FADE_SEC, HIT_FX_SEC};
    let doc = one_line_doc_speed(-10.0, &[(DocKind::Tap, 2.0, None, 0.0)]); // 2 拍 = 1 秒
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.selected_line = usize::MAX;

    let at = |st: &mut EditorState, t: f64| {
        st.playhead = t;
        let mut inst = Vec::new();
        build_instances(st, &mut inst);
        inst
    };
    // 到线之前：真值偏移是 −1200×(1−t)（在判定线下面）⇒ 一颗音符都不画
    for t in [0.0, 0.5, 0.7, 0.9, 0.99] {
        let inst = at(&mut st, t);
        assert_eq!(
            inst.len(),
            1,
            "t={t}：负流速下到线之前不该画音符（只剩判定线本体），实际 {} 个实例",
            inst.len()
        );
    }
    // 到线那一刻：击中效果要播（音符本体停在线上，随后淡出）
    let hit = at(&mut st, 1.0);
    assert!(flash_count(&hit) > 0, "负流速下击中效果仍要播");
    // 效果与淡出都结束之后：又只剩判定线
    let after = at(&mut st, 1.0 + HIT_FADE_SEC + HIT_FX_SEC + 0.01);
    assert_eq!(after.len(), 1, "结束之后只剩线本体，实际 {}", after.len());
}

/// **"音符只要在可见区域就要显示"**：判据必须是**屏幕上的位置**，不是"离判定线多远"。
///
/// 这条盯的是一个真 bug：判定线被事件移开（`moveY = -300`）时，偏移 660 的音符在屏幕上
/// 的 y 是 +360（明明在窗口里），却被旧的"线本地偏移 > 510 就跳过"整颗丢掉；
/// 线旋转 90° 时，同样的音符落在屏幕 x = -660（±675 之内）也被丢掉。
#[test]
fn notes_inside_the_window_are_drawn_however_far_they_are_from_the_line() {
    // (moveY, rotate)：音符 0.55 秒后打击（偏移 660）
    for (my, rot, want_screen) in [
        (0.0_f64, 0.0_f64, None),          // 屏幕上是 660 —— 真的在窗口外，不画才对
        (-300.0, 0.0, Some([0.0, 360.0])), // 线被移到下面 ⇒ 音符出现在屏幕上方 360（可见）
        (0.0, 90.0, Some([-660.0, 0.0])),  // 线转了 90° ⇒ 音符出现在屏幕左方 660（可见）
    ] {
        let mut doc = Document::default();
        doc.bpm_list = vec![BpmEntry {
            start: Beat::zero(),
            bpm: 120.0,
            foreign: Default::default(),
        }];
        doc.judge_lines.clear();
        let mut l = JudgeLine::default();
        let end = Beat::new(4096, 1);
        for (track, v) in [("speed", 10.0), ("moveY", my), ("rotate", rot)] {
            l.layers[0].track_mut(track).unwrap().push(Event::new(
                Beat::zero(),
                end,
                json!(v),
                json!(v),
                "linear",
            ));
        }
        l.notes
            .push(DocNote::new(DocKind::Tap, Beat::new(11, 10), 0.0)); // 0.55 s ⇒ 偏移 660
        doc.judge_lines.push(l);
        let mut st = EditorState::new(chart_from_doc(&doc));
        st.selected_line = usize::MAX;
        st.playhead = 0.0;
        let mut inst = Vec::new();
        build_instances(&st, &mut inst);
        let note = inst.iter().find(|q| {
            let c = q.color();
            (c[0] - 0.35).abs() < 0.02 && (c[1] - 0.65).abs() < 0.02 && (c[2] - 1.0).abs() < 0.02
        });
        match want_screen {
            Some(p) => {
                let q = note.unwrap_or_else(|| {
                    panic!("moveY={my} rotate={rot}：窗口里的音符被丢掉了（屏幕位置 {p:?}）")
                });
                assert!(
                    (q.center()[0] - p[0]).abs() < 1.0 && (q.center()[1] - p[1]).abs() < 1.0,
                    "屏幕位置应约等于 {p:?}，实际 {:?}",
                    q.center()
                );
            }
            None => assert!(note.is_none(), "屏幕上是 660（窗口外）⇒ 不该建实例"),
        }
    }
}

/// **hold 的尾巴位置必须按当前时刻推算**（用户发现："没有给 hold 尾部推算位置"）。
///
/// 曾经算成"头的偏移 + 整段时长"，而头的偏移当时来自一个**单调累加器**（它只往前走，
/// 查询过去的时刻一律返回当前累计值 0）⇒ 被按住时尾巴被钉死在"头 + 全长"上：
/// 身子不随按住而缩短，尾巴过去之后也永远不消失。
#[test]
fn a_held_hold_tail_follows_the_current_time() {
    // 0..4 拍（BPM 120 ⇒ 0..2 秒），流速 10 ⇒ 1200 单位/秒 ⇒ 全长 2400
    let doc = one_line_doc_speed(10.0, &[(DocKind::Hold, 0.0, Some(4.0), 0.0)]);
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.selected_line = usize::MAX;

    // 身子的**上端**（= 尾巴的偏移；判定线在原点时中心 + 半高就是它）
    let body_top = |st: &mut EditorState, t: f64| -> Option<f32> {
        st.playhead = t;
        let mut inst = Vec::new();
        build_instances(st, &mut inst);
        inst.iter()
            .find(|q| {
                let c = q.color();
                (c[0] - 0.75).abs() < 0.02
                    && (c[1] - 0.90).abs() < 0.02
                    && (c[2] - 1.0).abs() < 0.02
                    && c[3] < 0.9
            })
            .map(|q| q.center()[1] + q.half()[1])
    };

    // 尾巴离判定线多远 = H(t_尾) − H(t_此刻) = 1200 × (2 − t)
    for (t, want) in [(0.0, 2400.0), (0.5, 1800.0), (1.0, 1200.0), (1.5, 600.0), (1.9, 120.0)] {
        let got = body_top(&mut st, t).unwrap_or_else(|| panic!("t={t}：身子不该缺席"));
        assert!(
            (got - want).abs() < 1.0,
            "t={t}：尾巴应在 {want}，实际 {got}（这正是「没给尾巴推算位置」的样子）"
        );
    }
    // **第二次打击动画那一帧**（3 拍 = 1.5 秒）：身子与效果必须同时在
    // （用户报的就是这一帧"hold 会消失"）
    st.playhead = 1.5;
    let mut inst = Vec::new();
    build_instances(&st, &mut inst);
    assert!(flash_count(&inst) > 0, "第二次打击动画要播");
    assert!(body_top(&mut st, 1.5).is_some(), "动画播放时 hold 的身子不该消失");
    // 尾巴过去之后：身子必须消失（旧代码在这里会一直留着一条全长身子）
    assert_eq!(body_top(&mut st, 2.05), None, "尾巴过去之后身子必须消失");
    assert_eq!(body_top(&mut st, 4.0), None, "更晚也一样");
}

/// 广撒网：**各种流速 / 音符 speed / 时长**下——
/// 正流速时按住期间身子必须一直在、尾巴之后必须消失；
/// **负流速时线下一律不画**（身子整段都在判定线之下）⇒ 过了头部的淡出窗口之后只剩判定线本体。
#[test]
fn a_held_hold_body_follows_the_speed_sign() {
    for speed in [10.0_f64, 1.0, -10.0] {
        for note_speed in [1.0_f32, 2.0] {
            for hold_beats in [1.0_f64, 4.0, 12.0] {
                let mut doc =
                    one_line_doc_speed(speed, &[(DocKind::Hold, 0.0, Some(hold_beats), 0.0)]);
                doc.judge_lines[0].notes[0].speed = note_speed;
                let mut st = EditorState::new(chart_from_doc(&doc));
                st.selected_line = usize::MAX;
                let hold_sec = hold_beats * 0.5; // BPM 120
                // "音符本体（头或身子）在不在"：按**节奏色**认 —— 击中效果的环是提亮过的颜色
                // （`rgb × 0.35 + 0.65`），闪光又是纯白，都不会被误认。
                let note_quad = |st: &mut EditorState, t: f64| -> bool {
                    st.playhead = t;
                    let mut inst = Vec::new();
                    build_instances(st, &mut inst);
                    inst.iter().any(|q| {
                        let c = q.color();
                        (c[0] - 0.75).abs() < 0.02
                            && (c[1] - 0.90).abs() < 0.02
                            && (c[2] - 1.0).abs() < 0.02
                    })
                };
                // 从头部淡出之后扫到尾巴前 0.15 秒（`dy > 1` 的收尾不算）
                let mut t = 0.1;
                while t < hold_sec - 0.15 {
                    let got = note_quad(&mut st, t);
                    if speed > 0.0 {
                        assert!(
                            got,
                            "流速 {speed}、长 {hold_beats} 拍：t={t} 按住期间身子该在"
                        );
                    } else {
                        assert!(
                            !got,
                            "流速 {speed}、长 {hold_beats} 拍：t={t} 身子在判定线之下 ⇒ 不该画"
                        );
                    }
                    t += 0.05;
                }
                // 尾巴之后：两种符号都不该再有身子
                assert!(
                    !note_quad(&mut st, hold_sec + 0.1),
                    "流速 {speed}、长 {hold_beats} 拍：尾巴过去了身子还在"
                );
            }
        }
    }
}




/// **判定线之上且在窗口里 ⇒ 必须被画；判定线之下 ⇒ 一律不画**（含"负→正"过零段）。
///
/// 这条是上面两条规则的**联合**验收，用**独立积分**（`perf::speed_travel`，不经过渲染侧的
/// 单调累加器）当基准，对四种流速形状逐帧对账：
/// · 线下采样（期望偏移 < −1）被画出来 ⇒ 失败；
/// · 线上采样（期望偏移 > +1）没被画出来 ⇒ 失败。
///
/// 它一次抓出过两个真 bug：① 过零段里"线下却照画"（判定线之下不显示这条规则被漏掉过）；
/// ② 过零段里"线上却没画"——构建窗口的下界按 `|v| ≥ 0.05` 过滤掉了过零点，
/// 于是下界取成 1.25 ⇒ 窗口只有 3.4 秒，而**贴着判定线逗留**的音符在 3.45 秒外 ⇒ 整颗没实例。
#[test]
fn visible_notes_are_always_drawn_and_below_line_ones_never_are() {
    for (a, b, from, to) in [
        (2.0_f64, 10.0_f64, -10.0_f64, 10.0_f64),
        (2.0, 4.0, -10.0, 10.0),
        (4.0, 6.0, -20.0, 20.0),
        (2.0, 20.0, -3.0, 3.0),
    ] {
        let mut doc = Document::default();
        doc.bpm_list = vec![BpmEntry {
            start: Beat::zero(),
            bpm: 120.0,
            foreign: Default::default(),
        }];
        doc.judge_lines.clear();
        let mut l = JudgeLine::default();
        // 拍 = 秒 × 2（BPM 120），用 1/4 拍的分母写精确
        let beat = |sec: f64| Beat::new((sec * 8.0).round() as i64, 4);
        l.layers[0].track_mut("speed").unwrap().push(Event::new(
            Beat::zero(),
            beat(a),
            json!(from),
            json!(from),
            "linear",
        ));
        l.layers[0].track_mut("speed").unwrap().push(Event::new(
            beat(a),
            beat(b),
            json!(from),
            json!(to),
            "linear",
        ));
        // 每颗音符一个**独一份**、且**落在窗口里**的 laneX（否则会认错音符 / 被横向裁掉）
        let count = ((b + 1.0 - a) / 0.25).floor() as usize + 1;
        let step = if count > 1 { 1100.0 / (count - 1) as f32 } else { 0.0 };
        let mut s = a;
        let mut i = 0;
        while s <= b + 1.0 {
            l.notes.push(DocNote::new(DocKind::Tap, beat(s), -550.0 + i as f32 * step));
            s += 0.25;
            i += 1;
        }
        doc.judge_lines.push(l);
        let mut st = EditorState::new(chart_from_doc(&doc));
        st.selected_line = usize::MAX;
        let tmap = st.chart.tmap.clone();
        let events = st.chart.lines[0].tracks[4].events.clone();
        let notes: Vec<(f64, f32)> = st.chart.lines[0]
            .notes
            .iter()
            .map(|n| (n.time, n.lane_x))
            .collect();

        let (mut below, mut below_drawn, mut above, mut above_missing) =
            (0usize, 0usize, 0usize, 0usize);
        for (t_hit, lane) in &notes {
            let mut t = (a - 1.0).max(0.0);
            while t < *t_hit {
                let want = opm_app::perf::speed_travel(&events, &tmap, t, *t_hit) as f32;
                if want.abs() <= 380.0 {
                    st.playhead = t;
                    let mut inst = Vec::new();
                    build_instances(&st, &mut inst);
                    let drawn = inst.iter().any(|q| {
                        let c = q.color();
                        (q.center()[0] - lane).abs() < 1.0
                            && (c[0] - 0.35).abs() < 0.02
                            && (c[1] - 0.65).abs() < 0.02
                            && (c[2] - 1.0).abs() < 0.02
                    });
                    if want < -1.0 {
                        below += 1;
                        if drawn {
                            below_drawn += 1;
                        }
                    } else if want > 1.0 {
                        above += 1;
                        if !drawn {
                            above_missing += 1;
                            if above_missing <= 3 {
                                eprintln!("  线上却没画：t_hit={t_hit:.2} t={t:.2} 偏移 {want:.0} lane {lane:.0}");
                            }
                        }
                    }
                }
                t += 0.05;
            }
        }
        assert_eq!(below_drawn, 0, "事件 [{a},{b}] {from}→{to}：判定线之下的音符被画了 {below_drawn} 次");
        assert_eq!(
            above_missing, 0,
            "事件 [{a},{b}] {from}→{to}：窗口里的音符漏画了 {above_missing} 次"
        );
        assert!(below > 0 && above > 0, "用例本身要覆盖到线的两侧");
    }
}

// ------------------------------------------------ 音符位置：加载时算好 + 异步重算

/// 一份"一条线 + 多段流速 + 若干音符（含 hold）"的谱面（BPM 120 ⇒ 一拍 0.5 秒）。
///
/// 刻意混进四件难事：**缓动段**（段内梯形法与线性不同）、**空隙**（保持前一条的终值）、
/// **负流速段**（音符在判定线之下）、**长 hold**（尾巴要单独推算位置）。
fn floor_doc() -> Document {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry {
        start: Beat::zero(),
        bpm: 120.0,
        foreign: Default::default(),
    }];
    doc.judge_lines.clear();
    let mut l = JudgeLine::default();
    // 秒 → 拍（BPM 120 ⇒ 一拍 0.5 秒；写 `sec × 8 / 4` 保证落在精确的 1/4 拍上）
    let beat = |sec: f64| Beat::new((sec * 8.0).round() as i64, 4);
    let mut ev = |a: f64, b: f64, from: f64, to: f64, easing: &str| {
        l.layers[0].track_mut("speed").unwrap().push(Event::new(
            beat(a),
            beat(b),
            json!(from),
            json!(to),
            easing,
        ));
    };
    ev(0.0, 1.0, 10.0, 20.0, "linear"); // 起步加速
    ev(4.0, 6.0, 20.0, -10.0, "inOutQuad"); // 1~4 秒是空隙（保持 20），之后掉到负流速
    ev(6.0, 12.0, -10.0, 10.0, "outCubic"); // 负→正
    // 音符：铺满整条时间轴，含两条 hold（一条长的、一条在负流速段里且音符 speed = 2）
    for (k, sec) in [0.5_f64, 1.0, 2.5, 3.5, 5.5, 7.0, 9.0].iter().enumerate() {
        l.notes.push(DocNote::new(DocKind::Tap, beat(*sec), -600.0 + 200.0 * k as f32));
    }
    let mut h1 = DocNote::new(DocKind::Hold, beat(1.5), -500.0);
    h1.end = Some(beat(8.0));
    l.notes.push(h1);
    let mut h2 = DocNote::new(DocKind::Hold, beat(5.0), 0.0);
    h2.end = Some(beat(9.0));
    h2.speed = 2.0;
    l.notes.push(h2);
    doc.judge_lines.push(l);
    doc
}

/// 两个 `EditorState` 在给定播放头下**逐实例比对**（位置 / 半宽 / 颜色 / 角度）
fn assert_same_instances(a: &mut EditorState, b: &mut EditorState, at: f64, why: &str) {
    a.playhead = at;
    b.playhead = at;
    let mut ia = Vec::new();
    let mut ib = Vec::new();
    build_instances(a, &mut ia);
    build_instances(b, &mut ib);
    assert_eq!(ia.len(), ib.len(), "{why}：t={at} 实例条数不同");
    for (k, (x, y)) in ia.iter().zip(&ib).enumerate() {
        assert_eq!(x.center(), y.center(), "{why}：t={at} 第 {k} 个实例位置不同");
        assert_eq!(x.half(), y.half(), "{why}：t={at} 第 {k} 个实例大小不同");
        assert_eq!(x.color(), y.color(), "{why}：t={at} 第 {k} 个实例颜色不同");
        assert_eq!(x.angle(), y.angle(), "{why}：t={at} 第 {k} 个实例角度不同");
    }
}

/// **"预算好的位置"与"现算的位置"给出的是同一帧**（逐位相同）。
///
/// 这是这整套缓存最要紧的性质：异步重算补没补完、补到哪儿、中途又改没改，**画面都一样** ——
/// 于是"异步"退化成纯粹的代价问题，不必为它再写一份渲染语义。
/// 做法：同一条线，缓存干净时建一次实例；把缓存整条标脏（等价于"一颗都还没重算"）再建一次，
/// 两次的实例逐字段比对（含 hold 尾巴与击中效果 —— 它们都吃位置）。
#[test]
fn cached_and_on_the_fly_positions_agree() {
    let doc = floor_doc();
    for t in [0.0, 0.4, 1.2, 1.9, 2.8, 3.3, 4.5, 5.2, 6.4, 7.8, 9.5] {
        let mut a = EditorState::new(chart_from_doc(&doc));
        a.selected_line = usize::MAX;
        let mut b = EditorState::new(chart_from_doc(&doc));
        b.selected_line = usize::MAX;
        b.chart.lines[0].mark_floors_stale_from(0); // 整条标脏 ⇒ 每个音符都走"现算"
        assert_eq!(
            b.floor_pending(),
            b.chart.lines[0].notes.len(),
            "整条该是脏的（否则这条用例没测到现算路径）"
        );
        assert_same_instances(&mut a, &mut b, t, "缓存 vs 现算");
    }
}

/// **流速事件改了之后：改完的下一帧就已经是对的**，异步补完还是对的。
///
/// 走的是 GUI 那条路（`tracks_of` → `Line::set_tracks`，即"按线局部重建"）：
/// ① 改完立刻与"从头加载这份改过的谱面"逐实例相同；
/// ② 待重算的那些确实被标脏了（否则这条测试等于没测异步）；
/// ③ 一帧一小段补完之后，仍然与从头加载完全相同。
#[test]
fn a_speed_edit_is_correct_before_and_after_the_async_rebuild() {
    let doc = floor_doc();
    // 改第 2 条流速事件（4~6 秒那段）的起始值：20 → 3
    let mut edited = doc.clone();
    {
        let t = edited.judge_lines[0].layers[0].track_mut("speed").unwrap();
        t[1].start_value = json!(3.0);
    }
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.selected_line = usize::MAX;
    let tmap = st.chart.tmap.clone();
    let tracks = tracks_of(&edited, 0, &tmap);
    st.chart.lines[0].set_tracks(tracks, &tmap);

    let mut fresh = EditorState::new(chart_from_doc(&edited));
    fresh.selected_line = usize::MAX;
    let dirty = st.floor_pending();
    assert!(dirty > 0, "这次改动该弄脏一部分音符（否则下面测的不是异步）");
    assert!(dirty < st.chart.lines[0].notes.len(), "前缀积分：不该整条都脏");

    let spots = [0.0, 1.0, 3.0, 4.2, 5.0, 6.5, 8.5, 10.0];
    for t in spots {
        assert_same_instances(&mut st, &mut fresh, t, "改完但还没重算");
    }
    // 异步补齐：一帧只补 3 条（远小于 `FLOOR_NOTES_PER_FRAME`，好确认它真的一步步来）
    let mut frames = 0;
    while st.floor_pending() > 0 {
        assert!(st.pump_floors(3) <= 3, "一帧不许超过预算");
        frames += 1;
        assert!(frames <= dirty, "补不完：{dirty} 条脏、{frames} 帧");
    }
    assert_eq!(st.floor_rebuild(), None, "补完之后不该还挂着进度");
    for t in spots {
        assert_same_instances(&mut st, &mut fresh, t, "重算之后");
    }
}

/// 判定线**之下**的音符在"现算"路径上也一律不画（现算路径 = 缓存被标脏之后那条路）。
///
/// 基准是**直接积分**（`perf::speed_travel(此刻 → 打击时刻)`）：它 < 0 的那些帧，
/// 一个实例都不许有。这条用例顺带抓出过一个真 bug —— 走法在**空隙之后把下一条事件整条跳过**，
/// 于是前缀积分（渲染用的那条）与直接积分在空隙之后分家；见 `perf::walk_speed`。
#[test]
fn the_below_the_line_rule_holds_on_the_on_the_fly_path_too() {
    let doc = floor_doc();
    let mut st = EditorState::new(chart_from_doc(&doc));
    st.selected_line = usize::MAX;
    st.chart.lines[0].mark_floors_stale_from(0); // 全部走现算
    let tmap = st.chart.tmap.clone();
    let events = st.chart.lines[0].tracks[4].events.clone();
    let notes: Vec<(f64, f32)> = st.chart.lines[0]
        .notes
        .iter()
        .map(|n| (n.time, n.lane_x))
        .collect();
    for (t_hit, lane) in &notes {
        let mut t = 0.0;
        while t < *t_hit {
            let want = opm_app::perf::speed_travel(&events, &tmap, t, *t_hit);
            if want < -1.0 {
                st.playhead = t;
                let mut inst = Vec::new();
                build_instances(&st, &mut inst);
                let drawn = inst.iter().any(|q| {
                    let c = q.color();
                    (q.center()[0] - lane).abs() < 1.0
                        && (c[0] - 0.35).abs() < 0.02
                        && (c[1] - 0.65).abs() < 0.02
                        && (c[2] - 1.0).abs() < 0.02
                });
                assert!(!drawn, "t={t}（打击时刻 {t_hit}）：位置 {want:.0} 在判定线之下，却画了音符");
            }
            t += 0.1;
        }
    }
}

/// **空隙之后的事件必须被算进去**（渲染侧验收：位置必须是**手算**出来的那个数）。
///
/// 谱面：流速 0~1 秒 = 0.5、**1~2 秒是空隙**（保持 0.5）、2~5 秒 = 10、5~8 秒 = 20。
/// 播放头 0.875 秒（**在空隙之前**），两颗音符在 2.125 / 2.25 秒。手算（不依赖任何一行积分代码）：
///
/// ```text
///   [0.875, 1.00]  0.5  → 0.125 × 0.5 × 120 =  7.5
///   [1.00,  2.00]  0.5  → 1.000 × 0.5 × 120 = 60      （空隙里保持前一条的终值）
///   [2.00,  2.125] 10   → 0.125 × 10  × 120 = 150     ⇒ 第一颗 217.5
///   [2.00,  2.25]  10   → 0.250 × 10  × 120 = 300     ⇒ 第二颗 367.5
/// ```
///
/// 走法出错时（把空隙之后那条事件整条跳过）会一直拿 0.5 算 ⇒ 90 / 105 ——
/// 音符**还在画面上**，只是位置差了几百单位。这比"丢了"更难发现，
/// 而且**不能拿 `speed_travel(播放头 → 音符)` 当基准**（它和渲染侧的旧实现共用同一个走法，
/// 会一起错、于是对得上）—— 必须手算。
#[test]
fn a_note_after_a_gap_sits_where_the_direct_integral_says() {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry {
        start: Beat::zero(),
        bpm: 120.0,
        foreign: Default::default(),
    }];
    doc.judge_lines.clear();
    let mut l = JudgeLine::default();
    let beat = |b: f64| Beat::new((b * 4.0).round() as i64, 4);
    let mut push = |a: f64, b: f64, v: f64| {
        l.layers[0].track_mut("speed").unwrap().push(Event::new(
            beat(a),
            beat(b),
            json!(v),
            json!(v),
            "linear",
        ));
    };
    push(0.0, 2.0, 0.5); // 0~1 秒
    // 2~4 拍（1~2 秒）故意留空
    push(4.0, 10.0, 10.0); // 2~5 秒
    push(10.0, 16.0, 20.0); // 5~8 秒
    for (k, b) in [4.25_f64, 4.5].iter().enumerate() {
        l.notes.push(DocNote::new(DocKind::Tap, beat(*b), -300.0 + 600.0 * k as f32));
    }
    doc.judge_lines.push(l);

    let mut st = EditorState::new(chart_from_doc(&doc));
    st.selected_line = usize::MAX;
    st.playhead = 0.875;
    let mut inst = Vec::new();
    build_instances(&st, &mut inst);
    for (k, sec, want) in [(0usize, 2.125_f64, 217.5_f32), (1, 2.25, 367.5)] {
        let lane = -300.0 + 600.0 * k as f32;
        let got = inst.iter().find(|q| {
            let c = q.color();
            (q.center()[0] - lane).abs() < 1.0
                && (c[0] - 0.35).abs() < 0.02
                && (c[1] - 0.65).abs() < 0.02
                && (c[2] - 1.0).abs() < 0.02
        });
        let q = got.unwrap_or_else(|| panic!("{sec} 秒那颗（手算偏移 {want}）没被画出来"));
        assert!(
            (q.center()[1] - want).abs() < 0.5,
            "{sec} 秒那颗应在 {want}（手算），实际 {} —— 空隙之后的事件被跳过了？",
            q.center()[1]
        );
    }
}

/// **空隙之后的事件必须被算进去**（前缀积分 vs 直接积分的一条回归）。
///
/// 走法曾经在"空隙段走完"时也把 `idx` 前进一格，于是那条**正好从空隙终点开始**的事件
/// 被整条跳过：积分在空隙之后永远保持空隙前的值 —— 谱面里只要有一个空隙，它后面全错。
/// 这里把两种口径逐点对账（`H` 是前缀积分，两者的差必须等于"两点之间的直接积分"）。
#[test]
fn the_integral_keeps_going_after_a_gap() {
    let doc = floor_doc();
    let chart = chart_from_doc(&doc);
    let tmap = chart.tmap.clone();
    let line = &chart.lines[0];
    let events = line.tracks[4].events.clone();
    for sec in [0.5_f64, 1.5, 2.5, 3.9, 4.1, 5.0, 5.4, 6.2, 7.5, 9.0, 11.0] {
        let prefix = line.h_at(sec, &tmap);
        let direct = opm_app::perf::speed_travel(&events, &tmap, 0.0, sec);
        assert!(
            (prefix - direct).abs() < 1e-6,
            "t={sec}：前缀积分 {prefix} ≠ 从 0 直接积分 {direct}（空隙之后的事件被跳过了？）"
        );
    }
}

/// **连着两个负流速事件，各算各的**（用户报"第二个负变速事件似乎不生效"的那条口径的数值验收）。
///
/// 谱面（BPM 120 ⇒ 1 拍 = 0.5 秒）：流速 0~2 秒 = 10、2~4 秒 = **−10**、4~6 秒 = **−20**。
/// 手算 `H(t) = 120 ∫₀ᵗ v dτ`：
///
/// ```text
///   H(1.0) = 1200 ｜ H(2.0) = 2400 ｜ H(2.5) = 1800 ｜ H(4.0) = 0 ｜ H(4.5) = −1200 ｜ H(5.5) = −3600
/// ```
///
/// 第二个负事件（−20）**必须**在 H 上留下痕迹：4.0 → 4.5 秒那 0.5 秒走的是 −1200，不是 −600。
/// 同一段也能用"直接积分"独立问一遍（`perf::speed_travel(4.0 → 4.5)`），两条路必须一致。
#[test]
fn two_negative_speed_events_each_count() {
    let mut doc = Document::default();
    doc.bpm_list = vec![BpmEntry {
        start: Beat::zero(),
        bpm: 120.0,
        foreign: Default::default(),
    }];
    doc.judge_lines.clear();
    let mut l = JudgeLine::default();
    let beat = |b: f64| Beat::new((b * 4.0).round() as i64, 4);
    for (a, b, v) in [(0.0, 4.0, 10.0), (4.0, 8.0, -10.0), (8.0, 12.0, -20.0)] {
        l.layers[0].track_mut("speed").unwrap().push(Event::new(
            beat(a),
            beat(b),
            json!(v),
            json!(v),
            "linear",
        ));
    }
    // 音符：负流速段里放一颗（4.5 秒），用来断言"线下一律不画"在这条路上也成立
    l.notes.push(DocNote::new(DocKind::Tap, beat(9.0), 0.0));
    doc.judge_lines.push(l);

    let chart = chart_from_doc(&doc);
    let tmap = chart.tmap.clone();
    let line = &chart.lines[0];
    for (sec, want) in [
        (1.0, 1200.0),
        (2.0, 2400.0),
        (2.5, 1800.0),
        (4.0, 0.0),
        (4.5, -1200.0),
        (5.5, -3600.0),
    ] {
        let got = line.h_at(sec, &tmap);
        assert!((got - want).abs() < 1e-6, "H({sec}) 应为 {want}，实际 {got}");
    }
    // 独立基准：两点之间直接积分
    let events = line.tracks[4].events.clone();
    let seg = opm_app::perf::speed_travel(&events, &tmap, 4.0, 4.5);
    assert!((seg + 1200.0).abs() < 1e-6, "4.0→4.5 秒应为 −1200（走的是第二个负事件），实际 {seg}");

    // 渲染侧：播放头 4.0 秒时，4.5 秒那颗在判定线**之下**（偏移 −1200）⇒ 一颗都不画
    let mut st = EditorState::new(chart);
    st.selected_line = usize::MAX;
    st.playhead = 4.0;
    let mut inst = Vec::new();
    build_instances(&st, &mut inst);
    let drawn = inst.iter().any(|q| {
        let c = q.color();
        (c[0] - 0.35).abs() < 0.02 && (c[1] - 0.65).abs() < 0.02 && (c[2] - 1.0).abs() < 0.02
    });
    assert!(!drawn, "负流速段里线下的音符不该被画出来，实际画了");
}
