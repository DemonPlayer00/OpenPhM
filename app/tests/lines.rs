//! 判定线是父对象、音符是子对象 —— 这条依赖关系的可执行断言。
//!
//! 覆盖三件事：
//! 1. 视图构建：音符挂在线上、按 zOrder 排序、每条线的音符不串线；
//! 2. **几何**：判定线旋转/平移时，它子音符的实例中心与实例角度**同步变换**；
//! 3. **无关性**：改一条线的表演，不动另一条线的音符实例。

use opm_app::doc::{Beat, BpmEntry, Document, Event, JudgeLine, Note as DocNote, NoteKind as DocKind};
use opm_app::render::{build_instances, NoteInstance};
use opm_app::state::{chart_from_doc, EditorState, TrackId};
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
        if (i.half()[0] - 675.0).abs() < 1e-3 {
            // 线本体：本地 y 必须是 0
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
