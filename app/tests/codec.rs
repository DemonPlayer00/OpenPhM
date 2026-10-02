// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! codec 的可执行断言：枚举表以 `spec/*.json` 为单一数据源、RPE 导入必须产出**合法** opm、
//! 以及"导入→导出→再导入"不许漂。
//!
//! 为什么这些必须是测试而不是注释：RPE 与本项目的**音符枚举不同**（RPE 的 2 是 Hold、3 是 Flick），
//! 读错不报错、只是安静地判错 —— 这个生态最贵的坑就靠这里钉住。

use opm_app::codec::{self, rpe};
use opm_app::cmd::{validate, Severity};
use opm_app::doc::{Document, NoteKind};
use serde_json::{json, Value};

/// 一份**故意难缠**的 RPE 谱面：多 BPM、全 4 种音符、越界 alpha、负 alpha 事件、
/// 空隙/重叠事件、首事件不在拍 0、末事件不覆盖谱面结束、非 hold 带 endTime、
/// 以及一堆 opm v1 没建模的字段（father / extended / *Control / tint / visibleTime / easingLeft）。
fn messy_rpe() -> Value {
    // fixture 是**文件**（`tests/data/messy.rpe.json`）：测试与 CLI 演示用同一份，
    // 免得"测试里那份"和"命令行演示那份"各写一遍、然后慢慢分叉。
    serde_json::from_str(include_str!("data/messy.rpe.json")).expect("fixture 必须是合法 JSON")
}

fn import(v: Value) -> (Document, codec::Fidelity) {
    let r = rpe::load_value(v).expect("messy_rpe 应该能导入");
    (r.doc, r.fidelity)
}

/// 枚举表必须与 `spec/note-types.json` 一致 —— 不允许代码里再抄一份
#[test]
fn note_type_tables_come_from_the_spec_file() {
    let spec: Value =
        serde_json::from_str(include_str!("../../spec/note-types.json")).expect("spec JSON");
    for (name, want) in spec["formats"]["rpe"].as_object().unwrap() {
        if !want.is_i64() {
            continue; // otherValues 之类的说明字段
        }
        let want = want.as_i64().unwrap();
        let kind = codec::note_kind_from_rpe(want);
        assert_eq!(kind.as_str(), name, "RPE {want} 应映射到 {name}");
        assert_eq!(codec::note_kind_to_rpe(kind), want, "反向映射不一致");
    }
    // **本生态最贵的坑**：RPE 与官谱的 2/3 是反的
    assert_eq!(codec::note_kind_from_rpe(2), NoteKind::Hold);
    assert_eq!(codec::note_kind_from_rpe(3), NoteKind::Flick);
    assert_eq!(codec::note_kind_from_official(2), Some(NoteKind::Drag));
    assert_eq!(codec::note_kind_from_official(3), Some(NoteKind::Hold));
    assert_eq!(codec::note_kind_from_rpe(42), NoteKind::Tap, "未知按 Tap");
}

/// 缓动表同样来自 `spec/easing.json`
#[test]
fn easing_table_matches_spec() {
    let spec: Value =
        serde_json::from_str(include_str!("../../spec/easing.json")).expect("spec JSON");
    let rows = spec["easings"].as_array().unwrap();
    assert_eq!(codec::easing_count(), rows.len(), "缓动数量应与 spec 一致");
    for r in rows {
        let id = r["id"].as_i64().unwrap();
        let name = r["name"].as_str().unwrap();
        assert_eq!(codec::easing_name_of_rpe(id), Some(name), "id {id}");
        assert_eq!(codec::rpe_id_of_easing(name), Some(id), "name {name}");
    }
    assert_eq!(codec::easing_name_of_rpe(4), Some("outQuad"));
    assert_eq!(codec::easing_name_of_rpe(29), Some("inOutElastic"));
    assert_eq!(codec::easing_name_of_rpe(0), None, "0 不在表里");
}

/// 浮点拍 → 精确有理拍；三元组 → 精确有理拍
#[test]
fn beat_conversions_are_exact() {
    let b = codec::beat_from_f64(4.25);
    assert_eq!((b.n, b.d), (17, 4), "4.25 应还原成 17/4");
    let b = codec::beat_from_value(&json!([3, 1, 2])).unwrap();
    assert_eq!((b.n, b.d), (7, 2), "[3,1,2] 是 7/2 拍");
    let b = codec::beat_from_value(&json!([2, 0, 1])).unwrap();
    assert_eq!((b.n, b.d), (2, 1));
    // 负拍（RPE 允许事件在 0 之前）
    let b = codec::beat_from_value(&json!([-1, 1, 2])).unwrap();
    assert_eq!(b.to_f64(), -0.5);
    // 三元组回写
    assert_eq!(codec::beat_to_triple(opm_app::doc::Beat::new(7, 2)), json!([3, 1, 2]));
    assert_eq!(codec::beat_to_triple(opm_app::doc::Beat::new(-1, 2)), json!([-1, 1, 2]));
    // 坏输入不 panic
    assert_eq!(codec::beat_from_f64(f64::NAN).to_f64(), 0.0);
    assert!(codec::beat_from_value(&json!("x")).is_err());
    assert!(codec::beat_from_value(&json!([1, 2, 0])).is_err(), "分母 0");
}

/// **导入的结果必须是合法 opm**（规范 §9：补空隙/裁重叠是 codec 的职责）
#[test]
fn imported_rpe_is_valid_opm() {
    let (doc, _fid) = import(messy_rpe());
    let issues = validate(&doc);
    let errors: Vec<_> = issues.iter().filter(|i| i.severity == Severity::Error).collect();
    let msgs: Vec<&str> = errors.iter().map(|i| i.message.as_str()).collect();
    assert!(errors.is_empty(), "导入结果不该有 ERROR：{msgs:?}");
    assert_eq!(doc.format, "opm");
}

/// 逐字段核对：枚举、时间、坐标、alpha、BPM、事件规范化
#[test]
fn rpe_import_maps_fields_correctly() {
    let (doc, fid) = import(messy_rpe());

    // META：offset 毫秒、song → audio、illustration → illustrator
    assert_eq!(doc.meta.name, "codec test");
    assert_eq!(doc.meta.offset_ms, -35);
    assert_eq!(doc.meta.audio.as_deref(), Some("song.ogg"));
    assert_eq!(doc.meta.background.as_deref(), Some("bg.png"));
    assert_eq!(doc.meta.illustrator, "I");
    assert_eq!(doc.meta.level, "IN 15");
    assert!(doc.meta.foreign.contains_key("RPEVersion"), "RPEVersion 只作记录，要留在扩展袋里");

    // BPM：第二段是三元组 [8,0,1] = 拍 8
    assert_eq!(doc.bpm_list.len(), 2);
    assert_eq!(doc.bpm_list[0].bpm, 180.0);
    assert_eq!(doc.bpm_list[1].start.to_f64(), 8.0);
    assert_eq!(doc.bpm_list[1].bpm, 200.0);

    let line = &doc.judge_lines[0];
    assert_eq!(line.name, "L0");
    assert_eq!(line.bpm_factor, 2.0, "bpmfactor 是**除**，但要原样透传");
    assert_eq!(line.z_order, 3);
    assert!(line.is_cover);
    assert_eq!(line.layers.len(), 3, "三层（含一个 null 层）都要在");
    assert!(line.foreign.contains_key("father"));
    assert!(line.foreign.contains_key("extended"));
    assert!(line.foreign.contains_key("posControl"));
    assert_eq!(line.foreign["Texture"], json!("line.png"));

    // 音符：类型（RPE 2 = Hold）、精确时间、above、alpha 不截断
    let kinds: Vec<&str> = line.notes.iter().map(|n| n.kind.as_str()).collect();
    assert_eq!(kinds, vec!["tap", "hold", "flick", "drag", "tap", "tap", "tap"]);
    let hold = &line.notes[1];
    assert_eq!(hold.start.to_f64(), 8.5);
    assert_eq!(hold.end.unwrap().to_f64(), 12.25, "hold 的 endTime 要精确成 49/4");
    assert_eq!(hold.lane_x, 240.0);
    assert_eq!(hold.speed, 1.5);
    assert_eq!(hold.width_scale, 2.0, "RPE 的 size 是宽度倍率");
    assert_eq!(hold.y_offset, 5.0);
    assert_eq!(line.notes[2].alpha, 300, "alpha > 255 **不得截断**");
    assert_eq!(line.notes[3].side, "below", "above=0 是背面");
    assert!(line.notes[3].is_fake);
    assert_eq!(line.notes[3].lane_x, 700.0, "越界坐标原样保留（validate 只警告）");
    assert_eq!(line.notes[3].judge_area_scale, 1.5);
    assert!(line.notes[3].foreign.contains_key("tint"), "color 旧名要归一到 tint");
    assert!(line.notes[4].end.is_none(), "非 hold 不许带时长");
    assert_eq!(line.notes[5].kind.as_str(), "tap", "未知 type 按 tap");

    // 事件：alpha **原样 0~255**（v2 起与 RPE 同量纲，不再 ÷255）、贝塞尔、缓动名、foreign 保留
    let a0 = &line.layers[0].alpha[0];
    assert!((a0.start_value.as_f64().unwrap() - 255.0).abs() < 1e-9);
    assert!((a0.end_value.as_f64().unwrap() - 128.0).abs() < 1e-9, "RPE 的 128 要原样落进来");
    let sp = &line.layers[0].speed[0];
    assert_eq!(sp.easing, "linear");
    assert!(sp.bezier && sp.bezier_points.is_some());
    assert!(sp.foreign.contains_key("easingLeft") && sp.foreign.contains_key("linkgroup"));
    let mv = &line.layers[2].move_x[0];
    assert_eq!(mv.easing, "inOutQuad", "easingType 7 = inOutQuad");

    // 报告要说出关键降级
    let w = fid.warnings.join("\n");
    assert!(w.contains("alpha 为负"), "负 alpha 必须报告：{w}");
    assert!(w.contains("零长度"), "丢掉零长度事件必须报告：{w}");
    assert!(w.contains("重叠"), "裁重叠必须报告：{w}");
    assert!(w.contains("easingLeft"), "缓动区间裁剪要报告：{w}");
    let notes = fid.conversions.join("\n");
    assert!(notes.contains("前值延拓"), "补空隙要写进转换明细");
}

/// 轨道规范化：首事件补到拍 0、空隙延拓、重叠裁剪、末事件延拓到谱面结束
#[test]
fn tracks_are_normalized_on_import() {
    let (doc, _fid) = import(messy_rpe());
    let line = &doc.judge_lines[0];
    let chart_end = doc.chart_end();

    // 层 0 的 moveX：原本从拍 2 开始 → 导入后从 0 开始，且最后一条延拓到谱面结束
    let mx = &line.layers[0].move_x;
    assert_eq!(mx[0].start.to_f64(), 0.0, "首事件必须从拍 0 起");
    assert_eq!(mx[0].end.to_f64(), 2.0);
    assert_eq!(mx[0].start_value, mx[0].end_value, "补的是常量事件");
    assert_eq!(mx.last().unwrap().end, chart_end, "末事件要延拓到谱面结束");

    // 每一层每条轨道都无空隙无重叠（这就是规范化定义）
    for (li, layer) in line.layers.iter().enumerate() {
        for track in opm_app::doc::TRACKS {
            let list = layer.track(track).unwrap();
            for w in list.windows(2) {
                assert_eq!(w[0].end, w[1].start, "层{li} {track} 有空隙/重叠");
            }
            for e in list {
                assert!(e.end > e.start, "层{li} {track} 有非正长度事件");
            }
        }
    }
    // 层 2：零长度事件（[6,6]）被丢掉，留下一条**斜坡**（0..4，inOutQuad）——
    // 它**不能被拉长**（拉长会把斜率改掉 = "重载之后谱面变慢"），所以延拓是**另加一条常量段**
    let mx2 = &line.layers[2].move_x;
    assert_eq!(mx2.len(), 2, "原斜坡 + 一条常量延拓段");
    assert_eq!(mx2[0].start.to_f64(), 0.0);
    assert_eq!(mx2[0].end.to_f64(), 4.0, "原斜坡的跨度不动");
    assert_eq!(mx2[0].easing, "inOutQuad", "留下的那条仍是原事件（缓动没被换掉）");
    assert_eq!(mx2[1].start, mx2[0].end, "延拓段紧随其后（无空隙）");
    assert_eq!(mx2[1].end, chart_end, "延拓段覆盖到谱面结束");
    assert_eq!(mx2[1].start_value, mx2[1].end_value, "延拓段是常量");
    assert_eq!(mx2[1].start_value, mx2[0].end_value, "值取原事件的终值（解析延拓）");
}

/// **导入 → 导出 → 再导入**：建模过的部分必须逐字段相同（这是"保存/加载"的底线）
#[test]
fn rpe_roundtrip_preserves_modeled_data() {
    let (doc1, _) = import(messy_rpe());
    let (text, fid) = rpe::save_str(&doc1, rpe::RpeTarget::default());
    assert!(!fid.conversions.is_empty(), "导出也要有报告");
    let root: Value = serde_json::from_str(&text).expect("导出的必须是合法 JSON");
    assert_eq!(root["META"]["RPEVersion"], json!(160));
    assert_eq!(root["META"]["offset"], json!(-35));
    assert_eq!(root["BPMList"][1]["startTime"], json!([8, 0, 1]), "8 拍写成三元组");
    // 事件时间写成三元组、音符写浮点（可查到的 RPE 约定）。
    // 注意下标：导入时给"首事件晚于拍 0"补了一条常量事件，所以原来的第一条现在排在第 2 位 ——
    // 按**值**去找（start == 200）而不是按固定下标，免得规范化的细节一变测试就假红。
    let mx = root["judgeLineList"][0]["eventLayers"][0]["moveXEvents"].as_array().unwrap();
    let e200 = mx
        .iter()
        .find(|e| e["start"].as_f64() == Some(200.0))
        .expect("导出里应能找到 start=200 的那条");
    assert_eq!(e200["startTime"], json!([10, 0, 1]), "10 拍应写成三元组");
    // **斜坡的跨度不动**：延拓是把"末事件之后"补成一条常量段，而不是把它拉长
    // （拉长 = 改斜率 = 重新加载之后谱面动得更慢）
    assert_eq!(e200["endTime"], json!([12, 0, 1]), "斜坡事件保持它自己的跨度");
    let tail = mx
        .iter()
        .find(|e| e["startTime"] == json!([12, 0, 1]))
        .expect("规范化为给它补了一条常量延拓段");
    assert_eq!(
        tail["endTime"],
        codec::beat_to_triple(doc1.chart_end()),
        "延拓段覆盖到谱面结束"
    );
    assert_eq!(tail["start"].as_f64(), Some(300.0), "延拓段的值 = 原斜坡的终值");
    assert!(mx[0]["startTime"].is_array(), "补位事件也是三元组");
    // 音符时间默认也写三元组（实测：真实 RPE 谱面里 2591 个音符时间全是整数数组），
    // 这样 37+1/3 这种分母为 3 的时间才是**精确**的 —— 走浮点会退化成 37333333/1000000
    assert_eq!(root["judgeLineList"][0]["notes"][1]["startTime"], json!([8, 1, 2]));

    let (doc2, _) = import(root.clone());
    assert_eq!(
        modeled(doc1),
        modeled(doc2),
        "往返一趟后建模数据必须完全一致"
    );
    // numOfNotes 的 RPE 语义：含 FakeNote、不含 Hold
    let n = root["judgeLineList"][0]["numOfNotes"].as_i64().unwrap();
    assert_eq!(n, 7 - 1, "7 个音符里有 1 个 hold → 6");
}

/// **分母不是 2 的幂**的时间必须精确往返。
///
/// 这条是真实谱面逼出来的：我最初把音符时间导出成浮点（"RPE 音符写浮点"是文档里的印象），
/// 合成 fixture 全用 1/4、1/2 这类二进制友好的时间，跑得通；换成 PhiZone 抓的真实谱面立刻不等 ——
/// 因为真实谱面里分母出现 3/6/20/24，37+1/3 走浮点会退化成 37333333/1000000。
/// 现在默认导出三元组（精确），这条测试把它钉住。
#[test]
fn non_power_of_two_times_roundtrip_exactly() {
    let (doc1, _) = import(messy_rpe());
    let one_third = doc1.judge_lines[0]
        .notes
        .iter()
        .find(|n| (n.start.to_f64() - (37.0 + 1.0 / 3.0)).abs() < 1e-9)
        .expect("fixture 里应有 37+1/3 拍的音符");
    assert_eq!((one_third.start.n, one_third.start.d), (112, 3), "37+1/3 应是 112/3");
    let rot = &doc1.judge_lines[0].layers[0].rotate[0];
    assert_eq!((rot.end.n, rot.end.d), (65, 6), "10+5/6 应是 65/6");

    let (text, _) = rpe::save_str(&doc1, rpe::RpeTarget::default());
    let root: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(root["judgeLineList"][0]["notes"][6]["startTime"], json!([37, 1, 3]));
    assert_eq!(root["judgeLineList"][0]["eventLayers"][0]["rotateEvents"][0]["endTime"], json!([10, 5, 6]));
    let (doc2, _) = import(root);
    assert_eq!(modeled(doc1), modeled(doc2), "非 2 的幂时间往返后必须逐字段一致");
}

/// 只保留**建模过**的数据（丢掉 foreign），用来比较往返一致性
fn modeled(doc: Document) -> Value {
    let mut v = doc.to_json();
    fn strip(v: &mut Value) {
        match v {
            Value::Object(o) => {
                o.remove("foreign");
                for (k, val) in o.iter_mut() {
                    // foreign 是 flatten 进去的：比较前把已知键之外的全删掉
                    if k != "foreign" {
                        strip(val);
                    }
                }
                // flatten 的 foreign 键会与已知键同级出现，无法在这里区分 —— 由下面按需删
                for k in [
                    "RPEVersion", "id", "Texture", "Group", "anchor", "father", "rotateWithFather",
                    "isGif", "attachUI", "extended", "posControl", "sizeControl", "skewControl",
                    "yControl", "alphaControl", "easingLeft", "easingRight", "linkgroup",
                    "visibleTime", "hitsound", "tint", "tintHitEffects", "color", "extraTrack",
                    "chartTime", "judgeLineGroup", "multiLineString", "multiScale", "timeTags",
                    "xybind",
                ] {
                    o.remove(k);
                }
            }
            Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut v);
    v
}

/// opm 原生：存 → 读 必须一模一样（含 foreign）
#[test]
fn opm_native_roundtrip_is_exact() {
    let (doc1, _) = import(messy_rpe());
    let text = serde_json::to_string_pretty(&doc1.to_json()).unwrap();
    let back = Document::from_json(serde_json::from_str(&text).unwrap()).unwrap();
    assert_eq!(doc1.to_json(), back.to_json());
}

/// 格式检测不看扩展名
#[test]
fn format_detection_by_content() {
    assert_eq!(codec::detect(&doc_of_opm()), Some(codec::Format::Opm));
    assert_eq!(codec::detect(&messy_rpe()), Some(codec::Format::Rpe));
    // 只有 BPMList 的也算 RPE
    assert_eq!(codec::detect(&json!({"BPMList": []})), Some(codec::Format::Rpe));
    assert_eq!(codec::detect(&json!({"hello": 1})), None);
    let (doc, fid) = codec::to_document(messy_rpe()).unwrap();
    assert_eq!(fid.source, "rpe");
    assert_eq!(doc.format, "opm");
    assert!(codec::to_document(json!({"a": 1})).is_err());
}

fn doc_of_opm() -> Value {
    Document::default().to_json()
}

/// 空谱面 / 最小 RPE：不能 panic，且要产出**合法** opm
#[test]
fn minimal_and_empty_rpe_are_handled() {
    // 只有 BPMList 和一条空线
    let v = json!({
        "BPMList": [],
        "META": { "RPEVersion": 150, "offset": 0 },
        "judgeLineList": [{ "Name": "L", "notes": [], "eventLayers": [] }]
    });
    let (doc, fid) = import(v);
    assert_eq!(doc.bpm_list.len(), 1, "BPMList 空要补一条默认");
    assert_eq!(doc.bpm_list[0].bpm, 120.0);
    assert!(fid.warnings.iter().any(|w| w.contains("BPMList")));
    assert!(validate(&doc).iter().all(|i| i.severity != Severity::Error));

    // 没有 META 的
    let v = json!({ "BPMList": [{ "bpm": 120, "startTime": 0 }], "judgeLineList": [] });
    let (doc, _) = import(v);
    assert_eq!(doc.judge_lines.len(), 0);
    assert_eq!(doc.meta.name, "untitled");
}

/// hold 的 endTime ≤ startTime 要补成长度，不能产出非法文档
#[test]
fn zero_length_hold_is_repaired() {
    let v = json!({
        "BPMList": [{ "bpm": 120, "startTime": 0 }],
        "judgeLineList": [{
            "Name": "L",
            "eventLayers": [],
            "notes": [{ "type": 2, "startTime": 4.0, "endTime": 4.0, "positionX": 0, "alpha": 255 }]
        }]
    });
    let (doc, fid) = import(v);
    let n = &doc.judge_lines[0].notes[0];
    assert!(n.end.unwrap() > n.start, "零长度 hold 要补成有长度");
    assert!(fid.warnings.iter().any(|w| w.contains("hold")), "要报告");
    assert!(validate(&doc).iter().all(|i| i.severity != Severity::Error));
}

/// 目标版本档位可切换（规范 §9）
#[test]
fn export_target_version_is_switchable() {
    let (doc, _) = import(messy_rpe());
    for ver in [150, 160] {
        let (text, _) = rpe::save_str(&doc, rpe::RpeTarget { version: ver, ..Default::default() });
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["META"]["RPEVersion"], json!(ver));
    }
    // 音符时间形状可以切：关掉就写浮点（给只吃浮点的工具）
    let (text, _) = rpe::save_str(
        &doc,
        rpe::RpeTarget { note_triple_time: false, ..Default::default() },
    );
    let v: Value = serde_json::from_str(&text).unwrap();
    assert!(v["judgeLineList"][0]["notes"][1]["startTime"].is_number());
}


/// **打开文件必须让 GUI 全量重建**：`load_into` 要按全量话题广播。
///
/// GUI 只订阅、不轮询：它靠话题里的 `LineList`（整表重建）等去刷新自己那一小块。
/// 少发一个话题的后果是"打开了一个文件，编辑区还显示旧谱面"——静默且难查，所以钉住。
#[test]
fn load_into_broadcasts_all_topics() {
    use opm_app::broadcast::TopicFilter;
    use opm_app::core::EditCore;

    // 先写一个 opm 文件出来当输入
    let dir = std::env::temp_dir().join(format!("opm-codec-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("load-into.opm.json");
    let (doc, _) = import(messy_rpe());
    std::fs::write(&path, serde_json::to_string(&doc.to_json()).unwrap()).unwrap();

    let mut core = EditCore::new();
    let sub = core.subscribe(TopicFilter::all());
    core.load_into(&path).expect("load_into 应成功");
    let b = core
        .broadcasts()
        .back()
        .expect("load_into 必须投递一条广播");
    // 广播抬头用**谱面名**（不是路径）：同一条装载路也服务"从崩溃缓存继续"，那时"路径"是
    // `/tmp/opm/<hash>` 这种对用户没意义的东西
    assert_eq!(b.label, format!("载入 {}", doc.meta.name));
    use opm_app::broadcast::TopicKind;
    for want in [
        TopicKind::Meta,
        TopicKind::Bpm,
        TopicKind::LineList,
        TopicKind::LineProps,
        TopicKind::Notes,
        TopicKind::Note,
        TopicKind::Track,
    ] {
        assert!(
            b.topics.iter().any(|t| t.kind == want),
            "广播缺少话题 {want:?}（实际 {:?}）",
            b.topics.iter().map(|t| t.kind).collect::<Vec<_>>()
        );
    }
    assert!(sub.rx.try_recv().is_ok(), "订阅者应收到这条广播");
    // 打开之后：路径/格式/撤销栈都换新
    assert_eq!(core.path(), Some(path.as_path()));
    assert_eq!(core.source_format().as_str(), "opm");
    assert!(!core.is_dirty(), "刚打开的文件不该是脏的");
    // 再打开一次 RPE，格式要跟着换（保存写回的格式由它决定）
    let rpe_path = dir.join("load-into.rpe.json");
    std::fs::write(&rpe_path, include_str!("data/messy.rpe.json")).unwrap();
    core.load_into(&rpe_path).expect("RPE 也该能 load_into");
    assert_eq!(core.source_format().as_str(), "rpe");
    assert_eq!(core.rpe_target().version, 160, "RPEVersion=160 应被沿用");
    assert!(core.last_fidelity().map(|f| !f.is_lossless()).unwrap_or(false));
    std::fs::remove_dir_all(&dir).ok();
}


/// **编辑器辅助根字段按来源原样写回**（`chartTime` / `multiScale` / `xybind` / …）。
///
/// 这里曾经是"导出时先无条件写默认值，再跑 foreign 循环"，而那个循环见键已存在就跳过 ——
/// 于是六个字段永远是默认值：`chartTime` 88237.46（编辑器时长）变 327.0（拍数）、
/// `multiScale` 0.334（标量）变 `[1,1]`、`xybind` false 变 `[]`。
/// 而导入侧写着"保留（不建模，但别丢）"、保真度报告写着"按原名写回" —— 代码与声明脱节。
/// 真实数据（12 份 RPE 140/160/170 谱面）：`chartTime` 11/12 有、`judgeLineGroup` 全是
/// `["Default"]`、`multiScale` 全是标量、`xybind` 是布尔。
#[test]
fn editor_helper_root_fields_are_written_back_verbatim() {
    let mut src = messy_rpe();
    let src = src.as_object_mut().unwrap();
    src.insert("chartTime".into(), json!(88237.46398370003_f64));
    src.insert("judgeLineGroup".into(), json!(["Default"]));
    src.insert("multiLineString".into(), json!("4 5 6 7 8 9"));
    src.insert("multiScale".into(), json!(0.334_f64));
    src.insert("timeTags".into(), json!([{"name": "intro", "time": [2, 0, 1]}]));
    src.insert("xybind".into(), json!(false));

    let src = Value::Object(src.clone());
    let (doc, _) = import(src.clone());
    let (text, _fid) = rpe::save_str(&doc, rpe::RpeTarget::default());
    let root: Value = serde_json::from_str(&text).unwrap();
    for k in [
        "chartTime",
        "judgeLineGroup",
        "multiLineString",
        "multiScale",
        "timeTags",
        "xybind",
    ] {
        assert_eq!(root[k], src[k], "`{k}` 必须原样写回（实际 {text}）");
    }
}

/// **音符/事件里"来源有、opm 没建模"的字段同样不许被默认值顶掉**。
///
/// 与根字段是同一个 bug 的另一半：导出先写默认值，再跑 foreign 循环（那个循环"见键已存在
/// 就跳过"）⇒ `visibleTime` 一律变 `999999.0`、`easingLeft/Right` 变 `0.0`/`1.0`、
/// `linkgroup` 变 `0`，而保真度报告还写着"`easingLeft/Right` 原样写回"。
/// 真实数据里这些值**确实不是**默认值：`visibleTime` 0.1、`easingLeft` 0.337095、
/// `easingRight` 0.5773、`linkgroup` 1（12 份谱面统计）。
#[test]
fn note_and_event_foreign_fields_are_not_shadowed_by_defaults() {
    let (doc, _) = import(messy_rpe());
    let (text, _) = rpe::save_str(&doc, rpe::RpeTarget::default());
    let root: Value = serde_json::from_str(&text).unwrap();
    let notes = root["judgeLineList"][0]["notes"].as_array().unwrap();
    let hit = notes
        .iter()
        .find(|n| n.get("visibleTime").and_then(|v| v.as_f64()) == Some(3.0))
        .unwrap_or_else(|| panic!("fixture 里那个 visibleTime=3.0 的音符没了：{text}"));
    assert_eq!(hit["hitsound"], json!("hit.wav"), "来源字段要与它一起留下");
    let layers = root["judgeLineList"][0]["eventLayers"][0].as_object().unwrap();
    let all: Vec<&Value> = layers
        .iter()
        .filter(|(k, _)| k.ends_with("Events"))
        .flat_map(|(_, v)| v.as_array().unwrap().iter())
        .collect();
    let ev = all
        .iter()
        .find(|e| e.get("easingLeft").and_then(|v| v.as_f64()) == Some(0.25))
        .unwrap_or_else(|| panic!("`easingLeft=0.25` 的那条事件没了：{text}"));
    assert_eq!(ev["easingRight"], json!(0.75), "{ev}");
    assert_eq!(ev["linkgroup"], json!(2), "{ev}");
    // 而**来源没有**的时候，默认值照旧要补上（不然 RPE 读到的是缺字段）
    let empty = opm_app::doc::Note::new(opm_app::doc::NoteKind::Tap, opm_app::doc::Beat::zero(), 0.0);
    let mut d2 = Document::default();
    d2.judge_lines[0].notes.push(empty);
    let (text2, _) = rpe::save_str(&d2, rpe::RpeTarget::default());
    let root2: Value = serde_json::from_str(&text2).unwrap();
    assert_eq!(root2["judgeLineList"][0]["notes"][0]["visibleTime"], json!(999999.0));
}

/// 新建（或来源里没有）的文档：只补**真实谱面里 12/12 都在**的字段，其余不编。
///
/// `chartTime`（11/12）、`timeTags`（3/12）、`xybind`（10/12）在真实谱面里本来就会缺
/// ⇒ RPE 容得下缺失，编一个出来反而是往文件里塞假数据（`chartTime` 尤其明显：
/// 它是 RPE 编辑器的时长，141 起才写，量级是秒的千倍 —— 既不是内容末端也不是音频长度）。
#[test]
fn fresh_document_gets_real_shaped_defaults_and_invents_nothing_else() {
    let doc = Document::default();
    let (text, _fid) = rpe::save_str(&doc, rpe::RpeTarget::default());
    let root: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(root["judgeLineGroup"], json!(["Default"]), "真实谱面里都是 [\"Default\"]");
    assert_eq!(root["multiScale"], json!(1.0_f64), "真实谱面里是标量，不是 [1,1]");
    assert_eq!(root["multiLineString"], json!(""));
    for k in ["chartTime", "timeTags", "xybind"] {
        assert!(root.get(k).is_none(), "`{k}` 编不出来就不写：{text}");
    }
}

/// **判定线的 `father` / `rotateWithFather` 原样写回**（同样从"默认值顶掉来源值"修过来的）。
///
/// 真实数据：12 份谱面 505 条线里 63 条 `father != -1`（父子嵌套）、360 条 `rotateWithFather`
/// 为 true。导出写 `-1`/`false` 再"跳过已有键"，等于把嵌套结构悄悄拍平 ——
/// 而保真度报告里还写着"`father` 只是原样写回"。这条钉住它与声明一致。
#[test]
fn line_father_and_rotate_with_father_are_written_back() {
    let mut src = messy_rpe();
    let lines = src["judgeLineList"].as_array_mut().unwrap();
    lines[0]["father"] = json!(24);
    lines[0]["rotateWithFather"] = json!(true);
    // 再加一条**平**的线：来源里 father = -1 也要原样写回（不能靠"默认值恰好也是 -1"蒙对）
    let mut flat = lines[0].clone();
    flat["father"] = json!(-1);
    flat["rotateWithFather"] = json!(false);
    lines.push(flat);
    let (doc, _) = import(src);
    let (text, _fid) = rpe::save_str(&doc, rpe::RpeTarget::default());
    let root: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(root["judgeLineList"][0]["father"], json!(24), "{text}");
    assert_eq!(root["judgeLineList"][0]["rotateWithFather"], json!(true));
    assert_eq!(root["judgeLineList"][1]["father"], json!(-1));
    assert_eq!(root["judgeLineList"][1]["rotateWithFather"], json!(false));
}

/// **BPM 换点与音符/事件同一形状**：`startTime` 是三元组 `[整拍, 分子, 分母]`。
///
/// 这里曾经无条件写浮点：`[78,1,2]` 出去变 `78.5`，`1/3` 拍变 `0.3333333333333333` ——
/// BPM 换点落不到谱面作者写的位置上。真实数据：12 份谱面 19 个 BPM 条目**全是三元组**。
#[test]
fn bpm_start_time_is_a_triple_like_everything_else() {
    let mut doc = Document::default();
    doc.bpm_list = vec![
        opm_app::doc::BpmEntry {
            start: opm_app::doc::Beat::zero(),
            bpm: 120.0,
            foreign: Default::default(),
        },
        opm_app::doc::BpmEntry {
            start: opm_app::doc::Beat::new(157, 2),
            bpm: 180.0,
            foreign: Default::default(),
        },
        opm_app::doc::BpmEntry {
            start: opm_app::doc::Beat::new(1, 3),
            bpm: 200.0,
            foreign: Default::default(),
        },
    ];
    let (text, _fid) = rpe::save_str(&doc, rpe::RpeTarget::default());
    let root: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(root["BPMList"][0]["startTime"], json!([0, 0, 1]));
    assert_eq!(root["BPMList"][1]["startTime"], json!([78, 1, 2]), "78.5 拍要写成 78 + 1/2");
    assert_eq!(root["BPMList"][2]["startTime"], json!([0, 1, 3]), "1/3 拍要精确落在 1/3：{text}");
    // 只有显式要求"只吃浮点"的工具才走浮点那条路
    let (text2, _) = rpe::save_str(
        &doc,
        rpe::RpeTarget { triple_time: false, note_triple_time: false, ..Default::default() },
    );
    let root2: Value = serde_json::from_str(&text2).unwrap();
    assert_eq!(root2["BPMList"][1]["startTime"], json!(78.5));
}

// ---------------------------------------------------------------- 遮蔽区（躁域）

/// 带一块遮蔽区的 opm 文档（走**命令路径**造，与编辑器产出的一模一样）
fn doc_with_zone() -> Document {
    use opm_app::core::EditCore;
    let mut c = EditCore::new();
    for cmd in [
        json!({"op": "add_note", "line": 0, "kind": "tap", "startBeat": [4, 1], "laneX": 100.0}),
        json!({"op": "add_zone", "startBeat": [0, 1]}),
        json!({"op": "add_zone_event", "zone": 0, "track": "x1", "startBeat": [8, 1],
               "endBeat": [16, 1], "startValue": 0.0, "endValue": -400.0, "easing": "inOutCubic"}),
        // active：**一块一种状态**（用户口径 2026-10-02）——要换外观就放两块，别用渐变。
        // 这里刻意摆成"先 false 后 true"的两段，顺带钉住"外观可以分段切换"这条。
        json!({"op": "add_zone_event", "zone": 0, "track": "active", "startBeat": [4, 1],
               "endBeat": [8, 1], "startValue": false, "endValue": false}),
        json!({"op": "add_zone_event", "zone": 0, "track": "active", "startBeat": [8, 1],
               "endBeat": [12, 1], "startValue": true, "endValue": true}),
    ] {
        let r = c.exec(&cmd);
        assert_eq!(r["ok"], json!(true), "{cmd} → {r}");
    }
    c.doc().clone()
}

/// opm → opm：遮蔽区逐字段一致，且**读得回来**（含 `active` 的布尔值）
#[test]
fn mask_zones_roundtrip_through_opm() {
    let doc = doc_with_zone();
    let text = serde_json::to_string_pretty(&doc.to_json()).unwrap();
    let back = Document::from_json(serde_json::from_str(&text).unwrap()).unwrap();
    assert_eq!(doc.to_json(), back.to_json(), "opm 原生往返必须一模一样");
    assert_eq!(back.mask_zones.len(), 1);
    assert_eq!(back.mask_zones[0].active[0].start_value, json!(false));
    assert_eq!(back.mask_zones[0].active[1].end_value, json!(true), "第二段是 true（分段切换）");
    assert_eq!(back.mask_zones[0].x1[1].easing, "inOutCubic");
    assert_eq!(back.min_client_capability, opm_app::doc::CAP_MASK, "能力等级必须是 4");
}

/// **没有遮蔽区的谱面：文件里不该凭空多出 `maskZones`**（既有文件的字节不变）
#[test]
fn a_chart_without_zones_has_no_maskzones_field() {
    let v = Document::default().to_json();
    assert!(v.get("maskZones").is_none(), "{v}");
    // 但带 `maskZones: []` 的文件也读得进来（空数组等价于没有）
    let mut with_empty = v.clone();
    with_empty["maskZones"] = json!([]);
    let d = Document::from_json(with_empty).unwrap();
    assert!(d.mask_zones.is_empty());
    assert!(d.to_json().get("maskZones").is_none(), "读进来再写出去，空数组也不落盘");
}

/// **RPE 无法表达遮蔽区**：导出必须**明确报出丢弃**，而且导出的 JSON 里不得出现它
#[test]
fn exporting_to_rpe_drops_mask_zones_with_a_warning() {
    let doc = doc_with_zone();
    let (v, fid) = rpe::to_value(&doc, rpe::RpeTarget::default());
    assert!(v.get("maskZones").is_none(), "RPE 里不该有遮蔽区字段");
    assert!(!fid.is_lossless(), "丢了东西就不算无损");
    let hit = fid
        .warnings
        .iter()
        .find(|w| w.contains("遮蔽区"))
        .unwrap_or_else(|| panic!("警告里必须点名遮蔽区：{:?}", fid.warnings));
    assert!(hit.contains("已丢弃"), "{hit}");
    assert!(hit.contains("1 个区"), "要报出丢了几个区：{hit}");
    // 逐条报告也是保真度报告的一部分（不是只写进日志）
    assert!(fid.report().contains("遮蔽区"), "{}", fid.report());
}

/// 遮蔽区会影响 `chart_end`（区域的表演常常比最后一个音符更长）—— 时间轴总长靠它
#[test]
fn mask_events_extend_the_chart_end() {
    let mut doc = doc_with_zone();
    let before = doc.chart_end();
    let end = opm_app::doc::Beat::new(512, 1);
    doc.mask_zones[0].y1 = vec![opm_app::doc::Event::new(
        opm_app::doc::Beat::new(500, 1),
        end,
        json!(0.0),
        json!(0.0),
        "linear",
    )];
    assert!(doc.chart_end() >= end, "{} < {}", doc.chart_end().to_f64(), end.to_f64());
    assert!(doc.chart_end() > before);
}

/// 校验器：遮蔽区通道**允许空隙、允许首事件晚于拍 0**（这正是"什么时候出现"的表达），
/// 但**不许重叠、不许倒挂、不许乱序**
#[test]
fn mask_channel_invariants_differ_from_judge_line_tracks() {
    let doc = doc_with_zone();
    let issues = validate(&doc);
    let mask: Vec<String> = issues
        .iter()
        .filter(|i| i.pointer.contains("maskZones"))
        .map(|i| format!("{} {}", i.pointer, i.message))
        .collect();
    assert!(mask.is_empty(), "{mask:?}");

    let mut broken = doc.clone();
    // x1 是 [0,1]（默认三角那条种子块）+ [8,16]（后插的那块）⇒ 把后者起点压到 0，与前者重叠
    assert_eq!(broken.mask_zones[0].x1.len(), 2);
    broken.mask_zones[0].x1[1].start = opm_app::doc::Beat::new(0, 1);
    let issues = validate(&broken);
    assert!(
        issues.iter().any(|i| i.pointer.contains("maskZones") && i.severity == Severity::Error),
        "{}",
        issues.iter().map(|i| format!("{} {}", i.pointer, i.message)).collect::<Vec<_>>().join("; ")
    );

    // 首事件晚于拍 0、中间有空隙：**合法**（与判定线轨道相反）
    let mut sparse = doc.clone();
    sparse.mask_zones[0].x1 = vec![opm_app::doc::Event::new(
        opm_app::doc::Beat::new(64, 1),
        opm_app::doc::Beat::new(72, 1),
        json!(0.0),
        json!(0.0),
        "linear",
    )];
    let issues = validate(&sparse);
    assert!(
        !issues.iter().any(|i| i.pointer.contains("maskZones")),
        "遮蔽区的稀疏轨道是合法的：{}",
        issues.iter().map(|i| format!("{} {}", i.pointer, i.message)).collect::<Vec<_>>().join("; ")
    );
}

/// **一个 active 事件块只能是一种状态**（用户口径 2026-10-02）：手写的 JSON 里出现渐变时，
/// 校验器必须报出来 —— 写侧（`add_zone_event` / `set_zone_event`）拦得住编辑器，
/// 拦不住别人手改文件，所以这条不变量在两侧各有一道。
#[test]
fn a_ramped_active_block_is_a_validation_error() {
    use opm_app::doc::{Beat, Event};
    let mut doc = doc_with_zone();
    assert!(validate(&doc).iter().all(|i| !i.pointer.contains("maskZones")));
    // 把第一段改成 false → true（头尾两档）
    doc.mask_zones[0].active[0].end_value = json!(true);
    let issues = validate(&doc);
    let hit = issues
        .iter()
        .find(|i| i.pointer.contains("maskZones") && i.pointer.ends_with("active[0]"))
        .unwrap_or_else(|| {
            panic!(
                "渐变 active 块必须报错：{:?}",
                issues.iter().map(|i| format!("{} {}", i.pointer, i.message)).collect::<Vec<_>>()
            )
        });
    assert!(matches!(hit.severity, Severity::Error), "{:?}", hit.severity as u8);
    assert!(hit.message.contains("只能是一种状态"), "{}", hit.message);
    // 数字写法同理（0.2 / 0.8 也是两档）
    doc.mask_zones[0].active[0].start_value = json!(0.2);
    doc.mask_zones[0].active[0].end_value = json!(0.8);
    assert!(
        validate(&doc).iter().any(|i| i.pointer.ends_with("active[0]") && i.severity == Severity::Error),
        "0.2 → 0.8 同样跨档"
    );
    // 两档但**分成两块** = 合法的"外观分段切换"
    doc.mask_zones[0].active = vec![
        Event::new(Beat::new(4, 1), Beat::new(8, 1), json!(false), json!(false), "linear"),
        Event::new(Beat::new(8, 1), Beat::new(12, 1), json!(true), json!(true), "linear"),
    ];
    assert!(
        !validate(&doc).iter().any(|i| i.pointer.contains("maskZones")),
        "分段切换是合法的"
    );
}

/// 能力等级：有遮蔽区却声明 3 ⇒ **错误**（读取方必须明确拒绝，而不是默默不画那块区域）
#[test]
fn declaring_a_capability_below_the_mask_level_is_an_error() {
    let mut doc = doc_with_zone();
    doc.min_client_capability = 3;
    let issues = validate(&doc);
    assert!(
        issues
            .iter()
            .any(|i| i.pointer == "/minClientCapability" && i.severity == Severity::Error),
        "{}",
        issues.iter().map(|i| format!("{} {}", i.pointer, i.message)).collect::<Vec<_>>().join("; ")
    );
}

/// 只为"谱面末尾"这条判据造一份最小多线文档：每条线一条常量轨 + 一个音符。
fn two_lines_json(a_track_end: i64, a_note: i64, b_track_end: i64, b_note: i64) -> Value {
    let beat = |n: i64| json!({"n": n, "d": 1});
    let line = |name: &str, track_end: i64, note: i64| {
        json!({
            "name": name,
            "bpmFactor": 1.0,
            "layers": [{"moveX": [
                {"startBeat": beat(0), "endBeat": beat(track_end),
                 "startValue": 0.0, "endValue": 0.0, "easing": "linear"}
            ]}],
            "notes": [{"kind": "tap", "startBeat": beat(note), "laneX": 0.0}]
        })
    };
    json!({
        "format": "opm", "formatVersion": 1, "minClientCapability": 1, "extensions": [],
        "meta": {"name": "末尾判据", "composer": "", "charter": "", "illustrator": "",
                 "difficulty": "IN", "level": "IN 1", "offsetMs": 0,
                 "audio": null, "background": null},
        "bpmList": [{"startBeat": beat(0), "bpm": 120.0}],
        "judgeLines": [line("A", a_track_end, a_note), line("B", b_track_end, b_note)]
    })
}

/// 「轨道末事件早于谱面末尾」这类问题的指针。
fn short_track_errors(doc: &Document) -> Vec<String> {
    validate(doc)
        .into_iter()
        .filter(|i| i.severity == Severity::Error && i.message.contains("早于谱面末尾"))
        .map(|i| format!("{} {}", i.pointer, i.message))
        .collect()
}

/// **"谱面末尾"是全文档一个数**（音符 ∪ 五条基础轨 ∪ 遮蔽区七条通道），不是每条线各算一份。
///
/// 这条判据曾经在两个实现里分家：`cmd::validate` 用 `Document::chart_end`（全局），
/// 而 `spec/check.py` 按**每条线自己的音符**算末尾 —— 于是"多线谱面里 A 线的轨道铺到末尾、
/// B 线的轨道只铺到自己的音符"这种文件：Rust 报错、`check.py` 放行，同一份文件两个答案。
/// 2026-10-03 把 `check.py` 收口到同一份定义，这个用例守住它。
#[test]
fn the_chart_end_is_one_number_for_the_whole_document() {
    // A 线铺到 32、B 线只铺到自己的音符 8 ⇒ 末尾是全局的 32，B 线不合格
    let doc = Document::from_json(two_lines_json(32, 32, 8, 8)).unwrap();
    assert_eq!(doc.chart_end().to_f64(), 32.0);
    let errs = short_track_errors(&doc);
    assert_eq!(errs.len(), 1, "只有 B 线该被点出来：{errs:?}");
    assert!(errs[0].starts_with("/judgeLines[1].layers[0].moveX"), "{errs:?}");
    assert!(errs[0].contains('8') && errs[0].contains("32"), "{errs:?}");

    // 两条线都铺到末尾就没问题（对照组：证明上面的报错来自"长度"，不是别的）
    let doc = Document::from_json(two_lines_json(32, 32, 32, 32)).unwrap();
    assert!(short_track_errors(&doc).is_empty());
}

/// **遮蔽区把谱面末尾抬高**，判定线轨道因此得够到那里。
///
/// 别与 §4.6 那条搞混：遮蔽区通道**自己**允许早于谱末结束，但它的末事件**会抬高**末尾。
/// 写这条测试的直接原因：本轮改 `check.py` 时我先按"遮蔽区不计入"写了一遍，
/// 被 `mask_events_extend_the_chart_end` 撞回来 —— 直觉在这里是错的，所以要有一条正面的用例。
#[test]
fn a_mask_zone_dragged_late_raises_the_end_for_every_line() {
    let mut v = two_lines_json(16, 16, 16, 16);
    assert!(short_track_errors(&Document::from_json(v.clone()).unwrap()).is_empty());

    v["minClientCapability"] = json!(4);
    v["maskZones"] = json!([{
        "x1": [{"startBeat": {"n": 8, "d": 1}, "endBeat": {"n": 40, "d": 1},
                "startValue": 0.0, "endValue": 100.0, "easing": "linear"}]
    }]);
    let doc = Document::from_json(v).unwrap();
    assert_eq!(doc.chart_end().to_f64(), 40.0, "遮蔽区事件要参与谱面末尾");
    assert_eq!(
        short_track_errors(&doc).len(),
        2,
        "两条线的轨道都止于 16，都该被点出来"
    );
}

/// **v1 → v2 迁移：判定线 alpha 轨道 0~1 → 0~255**（用户口径 2026-10-03："透明度数值和 RPE
/// 保持一致，使用 0~255 计算法"）。
///
/// 为什么值得单独守：这条迁移是**打开旧文件时自动发生的**，错了不会报错 —— 只会让所有谱面的
/// 判定线一起变全透明（`1.0` 若被当成 0~255 里的 1，就是不透明度 1/255）。所以三个性质都要钉：
/// ① 值 ×255；② **无损**（v1 的值本来就是 `k/255`）；③ **幂等**（再读一次不许重复乘）。
#[test]
fn a_v1_document_gets_its_alpha_migrated_to_0_255() {
    let v1 = json!({
        "format": "opm", "formatVersion": 1, "minClientCapability": 1, "extensions": [],
        "meta": {"name": "旧谱", "composer": "", "charter": "", "illustrator": "",
                 "difficulty": "IN", "level": "IN 1", "offsetMs": 0,
                 "audio": null, "background": null},
        "bpmList": [{"startBeat": {"n": 0, "d": 1}, "bpm": 120.0}],
        "judgeLines": [{
            "name": "L0", "bpmFactor": 1.0,
            "layers": [{"alpha": [
                // 两个典型来源：手写的 1.0（不透明）与 RPE 导出的 128/255
                {"startBeat": {"n": 0, "d": 1}, "endBeat": {"n": 4, "d": 1},
                 "startValue": 1.0, "endValue": 0.5, "easing": "linear"},
                {"startBeat": {"n": 4, "d": 1}, "endBeat": {"n": 8, "d": 1},
                 "startValue": 128.0 / 255.0, "endValue": 0.0, "easing": "linear"}
            ]}],
            "notes": []
        }]
    });

    let doc = Document::from_json(v1.clone()).expect("v1 应当能读进来");
    assert_eq!(doc.format_version, opm_app::doc::FORMAT_VERSION, "读进来就升到当前版本");
    let a = &doc.judge_lines[0].layers[0].alpha;
    assert!((a[0].start_value.as_f64().unwrap() - 255.0).abs() < 1e-9, "1.0 → 255");
    assert!((a[0].end_value.as_f64().unwrap() - 127.5).abs() < 1e-9, "0.5 → 127.5");
    assert!(
        (a[1].start_value.as_f64().unwrap() - 128.0).abs() < 1e-9,
        "**无损**：RPE 的 128 除以 255 再乘回来必须还是 128，实际 {}",
        a[1].start_value
    );
    assert!((a[1].end_value.as_f64().unwrap()).abs() < 1e-9);

    // **幂等**：把迁移结果再读一遍，值不许再乘一次 255
    let twice = Document::from_json(doc.to_json()).expect("迁移后的文档要能读回");
    assert_eq!(twice.format_version, opm_app::doc::FORMAT_VERSION);
    let b = &twice.judge_lines[0].layers[0].alpha;
    assert!((b[0].start_value.as_f64().unwrap() - 255.0).abs() < 1e-9, "不许重复 ×255");
    assert!((b[1].start_value.as_f64().unwrap() - 128.0).abs() < 1e-9);
    assert_eq!(doc.to_json(), twice.to_json(), "迁移过一次之后应当是不动点");
}

/// 负 alpha（RPE 那条"连音符一起隐藏"的废弃分支）在 v1 里没有表达，迁移时**夹到 0** ——
/// 这正是 v1 时代 `÷255` 那一步的既有口径，迁移不该让它变成负数。
#[test]
fn a_negative_v1_alpha_migrates_to_zero_not_below() {
    let v1 = json!({
        "format": "opm", "formatVersion": 1, "minClientCapability": 1, "extensions": [],
        "meta": {"name": "负 alpha", "composer": "", "charter": "", "illustrator": "",
                 "difficulty": "IN", "level": "IN 1", "offsetMs": 0,
                 "audio": null, "background": null},
        "bpmList": [{"startBeat": {"n": 0, "d": 1}, "bpm": 120.0}],
        "judgeLines": [{
            "name": "L0", "bpmFactor": 1.0,
            "layers": [{"alpha": [
                {"startBeat": {"n": 0, "d": 1}, "endBeat": {"n": 8, "d": 1},
                 "startValue": -0.5, "endValue": 1.0, "easing": "linear"}
            ]}],
            "notes": []
        }]
    });
    let doc = Document::from_json(v1).expect("应当能读进来");
    let e = &doc.judge_lines[0].layers[0].alpha[0];
    assert_eq!(e.start_value.as_f64().unwrap(), 0.0, "负值夹到 0");
    assert!((e.end_value.as_f64().unwrap() - 255.0).abs() < 1e-9);
}
