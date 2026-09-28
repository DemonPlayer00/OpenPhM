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
    let missing = dir.join("没有这个目录").join("x.opm");
    let err = core.save_as(&missing, SaveFormat::OpmPacked).unwrap_err();
    assert!(err.contains("目标目录不存在"), "{err}");

    // 指定目标后写成功；**文件还不存在也能指定**（这正是用户报的那个问题）
    let fresh = dir.join("新谱面.opm");
    let (written, _fid) = core.save_as(&fresh, SaveFormat::OpmPacked).unwrap();
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
    let (written, fid) = core.save_as(&target, SaveFormat::OpmPacked).unwrap();
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

/// 新建谱面**第一次保存**建议哪种名字：打包形态给扩展名，文件夹形态给目录名；
/// **不再给单文件 JSON 当默认**（用户："保存时限定为 opm 或 rpe 包（或对应无压缩文件夹），而不是 json"）
#[test]
fn suggested_extension_offers_packages_or_folders_never_json() {
    use opm_app::codec::Format;
    // 打包形态：各自的包扩展名
    assert_eq!(SaveFormat::OpmPacked.suggested_extension(Format::Opm), Some(".opm"));
    assert_eq!(SaveFormat::RpePacked.suggested_extension(Format::Opm), Some(".pez"));
    // 文件夹形态：没有扩展名（建议的是一个目录名）
    assert_eq!(SaveFormat::OpmFolder.suggested_extension(Format::Opm), None);
    assert_eq!(SaveFormat::RpeFolder.suggested_extension(Format::Rpe), None);
    // 自动：新建的谱面默认给正式形态（opm 包）；从 RPE 来的给 `.pez`
    assert_eq!(SaveFormat::Auto.suggested_extension(Format::Opm), Some(".opm"));
    assert_eq!(SaveFormat::Auto.suggested_extension(Format::OpmZip), Some(".opm"));
    assert_eq!(SaveFormat::Auto.suggested_extension(Format::Rpe), Some(".pez"));
    // 任何一条都不该给出 json
    for f in [SaveFormat::Auto, SaveFormat::OpmPacked, SaveFormat::OpmFolder, SaveFormat::RpePacked, SaveFormat::RpeFolder] {
        for loaded in [Format::Opm, Format::OpmZip, Format::Rpe] {
            if let Some(ext) = f.suggested_extension(loaded) {
                assert!(!ext.contains("json"), "{f:?} + {loaded:?} → {ext}");
            }
        }
    }
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

/// **四种形态**（用户："4 种导出方式（opm|rpe|打包开关）"）各写一遍，并核对内容一致。
///
/// 要点：① opm 的包与文件夹内容**同源**；② RPE 的包与文件夹都带 `info.yml` + `chart.json`；
/// ③ 打包出来的是 zip（PK 头），文件夹形态真的落在目录里；④ 保存后 `path` 指向能直接再打开的那个文件。
#[test]
fn four_save_shapes_produce_the_expected_artifacts() {
    use opm_app::codec::{package, Format};
    use opm_app::core::SaveFormat;
    let dir = tmpdir("four-shapes");
    let audio = dir.join("song.ogg");
    let art = dir.join("bg.png");
    std::fs::write(&audio, b"OggS-payload").unwrap();
    std::fs::write(&art, b"\x89PNG-payload").unwrap();

    let mut form = opm_app::recents::NewChartForm::default();
    form.name = "四形态".to_owned();
    form.charter = "我".to_owned();
    form.bpm = 174.0;

    // ---- ① opm 包（zip）与 ② opm 文件夹：内容同源 ----
    let mut zip_core = EditCore::new();
    zip_core.exec(&form.to_new_command());
    zip_core.exec(&json!({"op": "set_meta", "set": {
        "audio": audio.display().to_string(), "background": art.display().to_string()}}));
    let packed = dir.join("四形态.opm");
    let (p1, _f) = zip_core.save_as(&packed, SaveFormat::OpmPacked).unwrap();
    assert_eq!(p1, packed);
    let bytes = std::fs::read(&packed).unwrap();
    assert_eq!(&bytes[..2], b"PK", "opm 包应当是个 zip");

    let mut dir_core = EditCore::new();
    dir_core.exec(&form.to_new_command());
    dir_core.exec(&json!({"op": "set_meta", "set": {
        "audio": audio.display().to_string(), "background": art.display().to_string()}}));
    let folder = dir.join("四形态.opm.d");
    let (p2, _f) = dir_core.save_as(&folder, SaveFormat::OpmFolder).unwrap();
    assert_eq!(p2, folder.join("opm.json"), "文件夹形态的路径指向目录里的谱面文件");
    assert!(folder.is_dir());
    for name in ["opm.json", "song.ogg", "bg.png"] {
        assert!(folder.join(name).is_file(), "文件夹里缺 {name}");
    }
    // 打包与不打包的**谱面内容**逐字节一致（两种形态用同一份条目清单）
    let zip_entries = opm_app::zip::read(&bytes).unwrap();
    let in_zip = zip_entries.iter().find(|e| e.name == "opm.json").expect("包内有 opm.json");
    assert_eq!(
        std::fs::read(folder.join("opm.json")).unwrap(),
        in_zip.data,
        "包里的 opm.json 与文件夹里的 opm.json 应当逐字节相同"
    );

    // ---- ③ RPE 包（.pez）与 ④ RPE 文件夹 ----
    let mut r1 = EditCore::new();
    r1.exec(&form.to_new_command());
    r1.exec(&json!({"op": "set_meta", "set": {
        "audio": audio.display().to_string(), "background": art.display().to_string()}}));
    let pez = dir.join("四形态.pez");
    let (p3, fid3) = r1.save_as(&pez, SaveFormat::RpePacked).unwrap();
    assert_eq!(p3, pez);
    let zb = std::fs::read(&pez).unwrap();
    assert_eq!(&zb[..2], b"PK", "RPE 谱面包应当是个 zip");
    let entries = opm_app::zip::read(&zb).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    for want in [package::INFO_NAME, package::CHART_NAME, "song.ogg", "bg.png"] {
        assert!(names.contains(&want), "谱面包里缺 {want}（实际 {names:?}）");
    }
    assert!(format!("{fid3:?}").contains("info.yml"), "保真度报告要提到 info.yml");
    // `info.yml` 指向同一批文件（不会"包里有、信息文件里写别的名字"）
    let info = String::from_utf8(
        entries.iter().find(|e| e.name == package::INFO_NAME).unwrap().data.clone(),
    )
    .unwrap();
    assert!(info.contains("chart: \"chart.json\""), "{info}");
    assert!(info.contains("music: \"song.ogg\""), "{info}");
    assert!(info.contains("illustration: \"bg.png\""), "{info}");
    assert!(info.contains("name: \"四形态\""), "{info}");

    let mut r2 = EditCore::new();
    r2.exec(&form.to_new_command());
    r2.exec(&json!({"op": "set_meta", "set": {
        "audio": audio.display().to_string(), "background": art.display().to_string()}}));
    let rdir = dir.join("四形态.rpe.d");
    let (p4, _f) = r2.save_as(&rdir, SaveFormat::RpeFolder).unwrap();
    assert_eq!(p4, rdir.join("chart.json"));
    for name in [package::INFO_NAME, package::CHART_NAME, "song.ogg", "bg.png"] {
        assert!(rdir.join(name).is_file(), "RPE 文件夹里缺 {name}");
    }
    // 文件夹里的 info.yml 与包里的**逐字节相同**
    assert_eq!(
        std::fs::read(rdir.join(package::INFO_NAME)).unwrap(),
        entries.iter().find(|e| e.name == package::INFO_NAME).unwrap().data
    );

    // ---- 保存后都能"再打开"（文件夹形态的 path 就是目录里那个谱面文件）----
    assert_eq!(dir_core.source_format(), Format::Opm);
    assert_eq!(r2.source_format(), Format::Rpe);
    let reopened = EditCore::load(&p4).unwrap();
    assert_eq!(reopened.doc().meta.name, "四形态");
    assert_eq!(reopened.doc().meta.audio.as_deref(), Some("song.ogg"));

    std::fs::remove_dir_all(dir).ok();
}

/// `Auto`：**新建单文件 JSON 被拒绝**（保存形态只有那四种），但**写回已存在的单文件**照旧 ——
/// 老谱面不能因为这条规定就存不回去。
#[test]
fn auto_refuses_new_json_targets_but_still_writes_back_existing_ones() {
    use opm_app::codec::Format;
    use opm_app::core::{SaveFormat, SaveShape};
    let dir = tmpdir("auto-json");
    let fresh = dir.join("新谱面.json");
    let err = SaveFormat::Auto.resolve(&fresh, Format::Opm).unwrap_err();
    assert!(err.contains("单文件 JSON 不再是保存形态"), "{err}");
    assert!(err.contains(".opm") && err.contains(".pez"), "报错要说清楚该换成什么：{err}");

    // 已经存在的单文件：按原形态写回
    let legacy = dir.join("老谱面.opm.json");
    std::fs::write(&legacy, "{}").unwrap();
    assert_eq!(SaveFormat::Auto.resolve(&legacy, Format::Opm).unwrap(), SaveShape::OpmSingle);
    let mut core = EditCore::new();
    core.exec(&json!({"op": "new", "meta": {"name": "老谱面"}, "bpm": 174.0}));
    let (p, _f) = core.save_as(&legacy, SaveFormat::Auto).unwrap();
    assert_eq!(p, legacy);
    assert!(!std::fs::read(&legacy).unwrap().starts_with(b"PK"), "写回的是裸 json，不是 zip");
    let reopened = EditCore::load(&legacy).unwrap();
    assert_eq!(reopened.doc().meta.name, "老谱面");

    // 扩展名决定形态；没有扩展名（或目录路径）⇒ 按来源格式的**文件夹**形态
    assert_eq!(SaveFormat::Auto.resolve(&dir.join("a.opm"), Format::Opm).unwrap(), SaveShape::OpmZip);
    assert_eq!(SaveFormat::Auto.resolve(&dir.join("a.pez"), Format::Opm).unwrap(), SaveShape::RpeZip);
    assert_eq!(SaveFormat::Auto.resolve(&dir.join("某目录"), Format::Opm).unwrap(), SaveShape::OpmFolder);
    assert_eq!(SaveFormat::Auto.resolve(&dir.join("某目录"), Format::Rpe).unwrap(), SaveShape::RpeFolder);
    std::fs::remove_dir_all(dir).ok();
}

/// **范式检查**：脏状态（"需要保存"）只由 `EditCore` 维护，接口也只在它身上。
///
/// 规则：**任何让谱面更新的命令都置脏**；查询命令不置；失败的命令不置（回滚）；
/// 保存清掉它。这条测试把它钉住 —— 以前 `replace_doc`（换进一份从没落过盘的文档）被标成"已保存"，
/// 就是这条不成立的一个实例。
#[test]
fn core_owns_the_needs_save_state() {
    let dir = tmpdir("dirty-owner");
    let mut core = EditCore::new();
    // `EditCore::new()` 是**占位空文档**：干净（没有会丢的东西）—— 与 `{"op":"new"}` 不同，
    // 后者是"用户明确要建一份谱面"，建完就脏。若占位也算脏，程序刚起来点关窗就会问"要先保存吗"。
    assert!(!core.is_dirty(), "占位空文档不算脏（见 EditCore::new 的说明）");

    core.exec(&json!({"op": "new", "meta": {"name": "范式"}, "bpm": 174.0}));
    assert!(core.is_dirty(), "新建之后必须脏（界面上要提示保存）");
    let target = dir.join("范式.opm");
    core.save_as(&target, SaveFormat::OpmPacked).unwrap();
    assert!(!core.is_dirty(), "刚存过 ⇒ 干净");

    // 查询命令**不许**动脏标记（列表要能穷举，这里挑几类：计数/导出/校验/缓存查询/日志）
    for q in [
        json!({"op": "ping"}),
        json!({"op": "summary"}),
        json!({"op": "dump"}),
        json!({"op": "validate"}),
        json!({"op": "overlaps"}),
        json!({"op": "journal"}),
        json!({"op": "broadcasts"}),
    ] {
        let r = core.exec(&q);
        assert_eq!(r["ok"], json!(true), "{q} 应当成功：{r}");
        assert!(!core.is_dirty(), "查询命令 {} 不该把文档弄脏", q["op"]);
    }

    // 改动命令 ⇒ 脏（每一条都验：置脏之后再存回去，保证下一条测的是"从干净出发"）
    for m in [
        json!({"op": "set_meta", "set": {"charter": "我"}}),
        json!({"op": "add_note", "line": 0, "kind": "tap", "startBeat": [1, 1], "laneX": 0.0}),
        json!({"op": "add_event", "line": 0, "track": "moveX", "startBeat": [0, 1], "endBeat": [2, 1],
               "start": -100.0, "end": 100.0, "easing": "linear"}),
        json!({"op": "set_bpm", "index": 0, "bpm": 180.0}),
    ] {
        let r = core.exec(&m);
        assert_eq!(r["ok"], json!(true), "{m} 应当成功：{r}");
        assert!(core.is_dirty(), "改动命令 {} 必须置脏", m["op"]);
        core.save(None).unwrap();
        assert!(!core.is_dirty(), "存完应当干净（{}）", m["op"]);
    }

    // 失败的命令：回滚干净，**不许**置脏（否则"手滑打错一条"会让界面喊未保存）
    let before = core.revision();
    let bad = core.exec(&json!({"op": "add_note", "line": 99, "kind": "tap", "startBeat": [1, 1]}));
    assert_eq!(bad["ok"], json!(false), "{bad}");
    assert_eq!(core.revision(), before, "失败的命令不该推进版本号");
    assert!(!core.is_dirty(), "失败的命令不该置脏");

    // 撤销/重做**也算改动**（保守口径：撤过保存点之后仍算脏 —— 宁可多提示一次保存）
    core.exec(&json!({"op": "set_meta", "set": {"composer": "某人"}}));
    core.save(None).unwrap();
    assert!(!core.is_dirty());
    core.exec(&json!({"op": "undo"}));
    assert!(core.is_dirty(), "撤销也是一次文档变更");

    // `replace_doc`（换进一份现成文档）：没有任何文件对得上它 ⇒ 脏
    core.save(None).unwrap();
    assert!(!core.is_dirty());
    core.replace_doc(opm_app::doc::Document::default());
    assert!(core.is_dirty(), "换进一份从没落过盘的文档，必须是脏的");

    std::fs::remove_dir_all(dir).ok();
}

/// 事务回滚（`abort`）：**真撤销了东西才算一次文档变更** —— 否则拉取方按 revision 增量取广播会漏。
#[test]
fn abort_counts_as_a_change_only_when_it_rolls_something_back() {
    let mut core = EditCore::new();
    core.exec(&json!({"op": "new", "meta": {"name": "事务"}, "bpm": 174.0}));

    // 空事务：版本号不动
    core.exec(&json!({"op": "begin", "label": "空"}));
    let r0 = core.revision();
    core.exec(&json!({"op": "abort"}));
    assert_eq!(core.revision(), r0, "空事务 abort 不该多一个版本号");

    // 有改动的事务：改动本身已 +1，回滚再 +1（内容确实变了两次）
    core.exec(&json!({"op": "begin", "label": "改一条"}));
    let r1 = core.revision();
    core.exec(&json!({"op": "set_meta", "set": {"charter": "甲"}}));
    assert_eq!(core.revision(), r1 + 1, "事务内的改动也要 +1");
    core.exec(&json!({"op": "abort"}));
    assert_eq!(core.revision(), r1 + 2, "回滚撤销了东西 ⇒ 也是一次变更");
    assert!(core.is_dirty(), "回滚之后仍是脏（保守口径）");
    // 回滚真的把内容退回去了
    assert_eq!(core.doc().meta.charter, "");
}
