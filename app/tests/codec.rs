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

    // 事件：alpha ÷255、贝塞尔、缓动名、foreign 保留
    let a0 = &line.layers[0].alpha[0];
    assert!((a0.start_value.as_f64().unwrap() - 1.0).abs() < 1e-9);
    assert!((a0.end_value.as_f64().unwrap() - 128.0 / 255.0).abs() < 1e-9);
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
    assert_eq!(root["BPMList"][1]["startTime"], json!(8.0));
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


/// **RPE 导出的 `chartTime` 取"全部事件的最大终点"**，而不是"每条轨道起点最晚那条的终点"。
///
/// 后者曾是这个文件里的第二份实现（`rpe::chart_end`，只取 `list.last()`）：一条轨道上
/// "起点最晚的事件"不一定是"结束最晚的事件"（长事件后面又放了一条短事件就会这样），
/// 于是导出的 `chartTime` 比真实谱面短 —— 播放器按它截断，末尾的表演就没了。
#[test]
fn exported_chart_time_is_the_max_end_not_the_last_by_start() {
    let mut doc = Document::default();
    // 一条**长**事件在前，一条**短**的在后（起点更晚、终点更早）
    let mut l = opm_app::doc::JudgeLine::default();
    l.layers[0].move_x.push(opm_app::doc::Event::new(
        opm_app::doc::Beat::zero(),
        opm_app::doc::Beat::new(16, 1),
        json!(0.0),
        json!(100.0),
        "linear",
    ));
    l.layers[0].move_x.push(opm_app::doc::Event::new(
        opm_app::doc::Beat::new(4, 1),
        opm_app::doc::Beat::new(6, 1),
        json!(50.0),
        json!(50.0),
        "linear",
    ));
    doc.judge_lines.push(l);
    assert_eq!(doc.chart_end(), opm_app::doc::Beat::new(16, 1), "文档口径就是最大终点");

    let (text, _fid) = rpe::save_str(&doc, rpe::RpeTarget::default());
    let root: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        root["chartTime"],
        json!(16.0_f64),
        "chartTime 要跟 `Document::chart_end` 一致（实际 {text}）"
    );
}
