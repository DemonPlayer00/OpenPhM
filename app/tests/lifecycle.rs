//! 文档生命周期：新建 / 打开 / 保存目标的语义（这些都是"会丢数据"的路径，必须有可执行断言）。
//!
//! 用户报过"保存新文件时无法指定路径"：新建出来的文档**没有保存目标**，
//! 而"保存"必须先问目标（Krita 的语义）。这条链路的每一段都在这里钉住。

use opm_app::broadcast::TopicFilter;
use opm_app::core::{EditCore, SaveFormat};
use serde_json::json;

/// 每个用例一个**独立**目录。
///
/// 曾经这里是"按进程号命名"的一个共享目录，而每个用例结尾都会 `remove_dir_all` 它 ——
/// cargo 默认**并行跑同一文件里的用例**，于是 A 的清理会把 B 正在用的目录删掉
/// （表现是 `写入失败: No such file or directory`，而且只在跑整个测试文件时出现）。
fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("opm-lifecycle-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d); // 上次跑剩下的（同进程重跑同名 tag 也不会串）
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
    let dir = tmpdir("save-target");
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

/// **音乐与曲绘要跟着谱面进 `.opm` 容器**（用户要求）。
///
/// 端到端走一遍真实链路：表单填的路径 → `{"op":"new"}` → `save_as(.opm)` → 重新读容器。
/// 断言两侧：① 文档字段被规范成**包内文件名**；② 容器里**真的**有两个条目、内容一致。
#[test]
fn new_chart_packs_audio_and_illustration_into_the_container() {
    let dir = tmpdir("packing");
    let audio = dir.join("song.ogg");
    let art = dir.join("曲绘 封面.png"); // 名字里带空格与中文，顺便验证文件名处理
    std::fs::write(&audio, b"OggS-fake-audio-payload").unwrap();
    std::fs::write(&art, b"\x89PNG-fake-illustration-payload").unwrap();

    // 命令**由表单自己产出**（不是手写 JSON）：这样"表单字段 → meta 字段 → 容器条目"
    // 是一条完整的链，中间改名/写错字段名都会在这里断掉。
    let mut form = opm_app::recents::NewChartForm::default();
    form.name = "带资源的谱".to_owned();
    form.charter = "我".to_owned();
    form.audio = audio.display().to_string();
    form.illustration = art.display().to_string();
    form.bpm = 174.0;
    form.validate().expect("表单应当通过校验");

    let mut core = EditCore::new();
    let resp = core.exec(&form.to_new_command());
    assert_eq!(resp["ok"], json!(true), "{resp}");
    assert_eq!(core.doc().meta.audio.as_deref(), Some(audio.display().to_string().as_str()));
    assert_eq!(
        core.doc().meta.background.as_deref(),
        Some(art.display().to_string().as_str()),
        "曲绘路径要真的进 meta.background（`new` 以前把它硬编码成 None）"
    );

    let target = dir.join("带资源的谱.opm");
    let (written, fid) = core.save_as(&target, SaveFormat::Opm).unwrap();
    assert!(written.exists(), "{}", written.display());
    // 保存后字段里应当是**包内名**（不是宿主机绝对路径），否则读回来会在容器里找不到条目
    assert_eq!(core.doc().meta.audio.as_deref(), Some("song.ogg"));
    assert_eq!(core.doc().meta.background.as_deref(), Some("曲绘 封面.png"));
    // 保真度报告要说明它们进包了（用户"我的音乐到底跟没跟着走"就靠这条）
    let notes = format!("{fid:?}");
    assert!(notes.contains("音乐"), "{notes}");
    assert!(notes.contains("曲绘"), "{notes}");

    // 重新读容器：两份资源都在，且**字节一致**
    let (container, _fid2) = opm_app::codec::container::read_file(&written).unwrap();
    for (what, name, want) in [
        ("音乐", "song.ogg", &b"OggS-fake-audio-payload"[..]),
        ("曲绘", "曲绘 封面.png", &b"\x89PNG-fake-illustration-payload"[..]),
    ] {
        let e = container
            .assets
            .iter()
            .find(|a| a.name == name)
            .unwrap_or_else(|| panic!("容器里没有{what} `{name}`：{:?}", container.assets.iter().map(|a| &a.name).collect::<Vec<_>>()));
        assert_eq!(e.data, want, "{what} 内容不一致");
    }
    // 文档字段与包内条目对得上（读回来就能直接用）
    assert_eq!(container.doc.meta.audio.as_deref(), Some("song.ogg"));
    assert_eq!(container.doc.meta.background.as_deref(), Some("曲绘 封面.png"));

    std::fs::remove_dir_all(dir).ok();
}

/// 新建谱面**第一次保存**建议哪种扩展名：引用了音乐/曲绘就给容器 `.opm`（它们要进包），
/// 没引用资源就维持裸 `.opm.json`（可 diff、可入版本库）；用户显式选的格式一律优先。
#[test]
fn suggested_extension_prefers_the_container_when_assets_are_referenced() {
    use opm_app::codec::Format;
    assert_eq!(SaveFormat::Auto.suggested_extension(Format::Opm, true), ".opm");
    assert_eq!(SaveFormat::Auto.suggested_extension(Format::Opm, false), ".opm.json");
    // 从容器载入的文档（没有目标时）也仍旧给容器
    assert_eq!(SaveFormat::Auto.suggested_extension(Format::OpmZip, false), ".opm");
    // 用户显式选的格式优先于"有资源就容器"这条建议
    assert_eq!(SaveFormat::OpmBare.suggested_extension(Format::OpmZip, true), ".opm.json");
    assert_eq!(SaveFormat::Opm.suggested_extension(Format::Opm, false), ".opm");
    assert_eq!(SaveFormat::Rpe.suggested_extension(Format::Opm, true), ".json");
}

/// `references_assets` 是上面那条建议的判据：音乐/曲绘任一**非空白**即为真
#[test]
fn references_assets_tracks_music_and_illustration() {
    let mut core = EditCore::new();
    assert!(!core.references_assets(), "空文档不该说引用了资源");
    core.exec(&json!({"op": "new", "meta": {"name": "x", "audio": "a.ogg"}, "bpm": 174.0}));
    assert!(core.references_assets(), "有音乐即为真");
    core.exec(&json!({"op": "new", "meta": {"name": "x", "background": "   "}, "bpm": 174.0}));
    assert!(!core.references_assets(), "只有空白路径不算引用");
    core.exec(&json!({"op": "new", "meta": {"name": "x", "background": "bg.png"}, "bpm": 174.0}));
    assert!(core.references_assets(), "有曲绘即为真");
}
