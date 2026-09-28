//! 文档生命周期：新建 / 打开 / 保存目标的语义（这些都是"会丢数据"的路径，必须有可执行断言）。
//!
//! 用户报过"保存新文件时无法指定路径"：新建出来的文档**没有保存目标**，
//! 而"保存"必须先问目标（Krita 的语义）。这条链路的每一段都在这里钉住。

use opm_app::broadcast::TopicFilter;
use opm_app::core::{EditCore, SaveFormat};
use serde_json::json;

fn tmpdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("opm-lifecycle-test-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `new` 必须产出一份**合法且脏**的空谱面，并清掉保存目标
#[test]
fn new_document_is_valid_dirty_and_targetless() {
    let mut core = EditCore::new();
    let sub = core.subscribe(TopicFilter::all());
    let resp = core.exec(&json!({
        "op": "new",
        "meta": {"name": "试作", "charter": "我", "composer": "某人", "audio": "song.ogg"},
        "bpm": 174.5
    }));
    assert_eq!(resp["ok"], json!(true), "{resp}");
    assert_eq!(resp["result"]["name"], json!("试作"));
    assert_eq!(resp["result"]["bpm"], json!(174.5));
    assert_eq!(resp["result"]["lines"], json!(1));
    assert!(resp["result"]["path"].is_null(), "新建后不该有保存目标");

    let doc = core.doc();
    assert_eq!(doc.meta.name, "试作");
    assert_eq!(doc.meta.charter, "我");
    assert_eq!(doc.meta.composer, "某人");
    assert_eq!(doc.meta.audio.as_deref(), Some("song.ogg"));
    // bpmList 的初始不变量：从拍 0 起、唯一一条、正数（规范错误规则 2）
    assert_eq!(doc.bpm_list.len(), 1);
    assert_eq!(doc.bpm_list[0].start.to_f64(), 0.0);
    assert!((doc.bpm_list[0].bpm - 174.5).abs() < 1e-6);
    // 至少一条判定线（否则编辑区没有可编辑的对象）
    assert_eq!(doc.judge_lines.len(), 1);
    // 新建 = 脏（还没写进任何文件），且没有路径
    assert!(core.path().is_none());
    assert!(core.is_dirty(), "新建之后必须是脏的，否则界面不会提示保存");
    // 撤销栈清空：不能把"上一份谱面"的逆操作带到新文档上
    assert!(core.undo().unwrap().is_none());
    // 广播要覆盖全量话题（GUI 靠它整表重建）
    let b = core.broadcasts().back().expect("new 要投递广播");
    assert!(b.topics.iter().any(|t| matches!(t.kind, opm_app::broadcast::TopicKind::LineList)));
    assert!(sub.rx.try_recv().is_ok());
    // 空曲名兜底
    let resp = core.exec(&json!({"op": "new", "meta": {"name": ""}, "bpm": 0}));
    assert_eq!(resp["result"]["name"], json!("untitled"));
    assert_eq!(core.doc().bpm_list[0].bpm, 174.0, "非法 BPM 要兜底成正数");
}

/// 没有保存目标时 `save` 必须**明确拒绝**（GUI 据此弹保存窗口，而不是静默写错地方）
#[test]
fn save_without_target_is_refused_until_a_target_is_given() {
    let mut core = EditCore::new();
    core.exec(&json!({"op": "new", "meta": {"name": "无目标"}, "bpm": 200.0}));
    let err = core.save(None).unwrap_err();
    assert!(err.contains("未指定保存路径"), "{err}");
    assert!(core.is_dirty(), "保存失败不能把脏标记清掉");

    // 目录不存在 → 报**能看懂**的错，而不是内核那句 "No such file or directory"
    let dir = tmpdir();
    let missing = dir.join("没有这个目录").join("x.opm.json");
    let err = core.save_as(&missing, SaveFormat::Opm).unwrap_err();
    assert!(err.contains("目标目录不存在"), "{err}");

    // 指定目标后写成功；**文件还不存在也能指定**（这正是用户报的那个问题）
    let fresh = dir.join("新谱面.opm.json");
    let (written, _fid) = core.save_as(&fresh, SaveFormat::Opm).unwrap();
    assert!(written.exists(), "{}", written.display());
    assert!(!core.is_dirty(), "保存成功后不该再是脏的");
    assert_eq!(core.path(), Some(written.as_path()));
    // 再打开一次，目标与格式都跟着文件走
    let again = EditCore::load(&written).unwrap();
    assert_eq!(again.doc().meta.name, "无目标");
    assert!(!again.is_dirty());
    assert_eq!(again.source_format().as_str(), "opm");
    std::fs::remove_dir_all(dir).ok();
}
