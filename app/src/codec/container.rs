//! opm 的**容器形态**：`.opm` 是一个 ZIP，里面是谱面 + 音乐 + 曲绘等资源。
//!
//! 布局（沿用 `spec/opm-format.md` 第 172 行的约定：根目录 `opm.json` + 资源）：
//!
//! ```text
//! chart.opm
//! ├─ opm.json          谱面本身（就是裸 opm 的那些字段，原样）
//! ├─ song.ogg          音乐：文件名 = `meta.audio`
//! ├─ bg.png            曲绘/背景：文件名 = `meta.background`
//! └─ …                 其它资源**原样保留**（不认识的也带过去，不许悄悄丢）
//! ```
//!
//! 两条刻意的不变量：
//! 1. **裸 JSON 仍然支持**（`.opm.json`）：可 diff、可入版本库、agent/测试都用它 ——
//!    容器是**分发形态**，不是唯一形态（与 Krita 的 `.kra`(zip) / `.krita`(纯 XML) 同一个取舍）；
//! 2. **重写容器不丢资源**：读进来的资源留在内存里，保存时原样写回；缺的（磁盘上也找不到）
//!    只**报警告**，不静默产出缺音乐的包。

use std::path::{Path, PathBuf};

use crate::doc::Document;
use crate::zip;

use super::Fidelity;

/// 容器里谱面的固定名字（规范里就是它）
pub const CHART_NAME: &str = "opm.json";

/// 一个容器：谱面 + 资源
#[derive(Clone, Debug)]
pub struct Container {
    pub doc: Document,
    /// 资源：名字（容器内相对路径）→ 字节。**顺序保留**（写回时先谱面、再按原顺序）
    pub assets: Vec<zip::Entry>,
}

/// 从字节读容器
pub fn read(bytes: &[u8]) -> Result<(Container, Fidelity), String> {
    if !zip::looks_like_zip(bytes) {
        return Err("不是 opm 容器（缺少 ZIP 头）".to_owned());
    }
    // 先走内置解压（纯内存、快、覆盖 STORE/DEFLATE）；遇到内置搞不定的（ZIP64、加密、
    // 特殊方法）再交给系统 7z —— 它久经考验，能把话说明白。
    let (entries, via_7z) = match zip::read(bytes) {
        Ok(e) => (e, false),
        Err(builtin_err) => match zip::unpack_with(zip::Backend::SevenZip, bytes) {
            Ok(e) => (e, true),
            Err(z7_err) => {
                return Err(format!("解压失败：内置（{builtin_err}）；7z（{z7_err}）"))
            }
        },
    };
    let mut fid = Fidelity::new("opm", "容器（zip）".to_owned());
    if via_7z {
        fid.note("内置解压搞不定这个包（ZIP64/特殊方法？），改用系统 7z 解开");
    }

    // 谱面：优先 `opm.json`，其次唯一的 `*.opm.json`（容忍别家工具换个名字）
    let chart_at = entries
        .iter()
        .position(|e| e.name == CHART_NAME)
        .or_else(|| {
            let mut it = entries.iter().enumerate().filter(|(_, e)| {
                e.name.ends_with(".opm.json") || e.name.ends_with(".json")
            });
            match (it.next(), it.next()) {
                (Some((i, _)), None) => Some(i),
                _ => None,
            }
        })
        .ok_or_else(|| {
            format!(
                "容器里没有谱面（找不到 `{CHART_NAME}`；实际条目：{}）",
                entries
                    .iter()
                    .map(|e| e.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let (chart_name, chart_bytes) = {
        let e = &entries[chart_at];
        (e.name.clone(), e.data.clone())
    };
    if chart_name != CHART_NAME {
        fid.warn(format!(
            "谱面文件名是 `{chart_name}` 而不是 `{CHART_NAME}` —— 已按它读取，写回时会改成 `{CHART_NAME}`"
        ));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&chart_bytes).map_err(|e| format!("`{chart_name}` 不是合法 JSON: {e}"))?;
    let doc = Document::from_json(v)?;

    let assets: Vec<zip::Entry> = entries
        .into_iter()
        .enumerate()
        .filter(|(i, _)| *i != chart_at)
        .map(|(_, e)| e)
        .collect();
    let mut bytes_total = 0usize;
    for a in &assets {
        bytes_total += a.data.len();
    }
    fid.note(format!(
        "容器：{} 个资源（共 {} KiB）",
        assets.len(),
        (bytes_total + 1023) / 1024
    ));
    // 文档引用的资源在不在包里 —— 不在就明说（别让用户以为音乐跟着走了）
    for (what, want) in [("音乐", doc.meta.audio.as_deref()), ("曲绘/背景", doc.meta.background.as_deref())] {
        if let Some(name) = want.filter(|n| !n.trim().is_empty()) {
            match assets.iter().find(|a| a.name == *name) {
                Some(a) => fid.note(format!("{what} `{name}` 在容器内（{} KiB）", a.data.len() / 1024)),
                None => fid.warn(format!(
                    "{what} `{name}` **不在容器内**（`meta` 里写着它，但包里没有对应条目）"
                )),
            }
        }
    }
    fid.finalize();
    Ok((Container { doc, assets }, fid))
}

/// 写容器：**优先系统 7z**（谱面文本 Deflate、媒体 Copy），没有 7z 时用内置实现（全 STORE）。
/// 返回 `(字节, 用的后端)` —— 后端写进保真度报告，别让"谁打的包"变成谜。
pub fn write(doc: &Document, assets: &[zip::Entry]) -> Result<(Vec<u8>, zip::Backend), String> {
    let chart = serde_json::to_vec_pretty(&doc.to_json()).map_err(|e| format!("序列化失败: {e}"))?;
    let mut files: Vec<zip::Entry> = Vec::with_capacity(assets.len() + 1);
    files.push(zip::Entry { name: CHART_NAME.to_owned(), data: chart });
    for a in assets {
        if a.name == CHART_NAME {
            continue; // 别让资源把谱面顶掉
        }
        files.push(a.clone());
    }
    zip::pack_preferred(&files)
}

/// 写容器到文件；返回用的后端
pub fn write_file(
    doc: &Document,
    assets: &[zip::Entry],
    path: &Path,
) -> Result<zip::Backend, String> {
    let (bytes, backend) = write(doc, assets)?;
    std::fs::write(path, bytes).map_err(|e| format!("写入失败: {e}"))?;
    Ok(backend)
}

/// 从文件读容器
pub fn read_file(path: &Path) -> Result<(Container, Fidelity), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读取失败: {e}"))?;
    read(&bytes)
}

/// 资源缓存目录：`$XDG_CACHE_HOME/OpenPhM/assets/<key>`（把容器内资源摊到磁盘，
/// 播放器/图片加载都按**路径**工作，不必为它们各自改一套内存接口）。
pub fn asset_cache_dir(key: &str) -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("OpenPhM").join("assets").join(key)
}

/// 把容器资源摊到缓存目录，返回"名字 → 落盘路径"
pub fn extract_assets(assets: &[zip::Entry], key: &str) -> Result<Vec<(String, PathBuf)>, String> {
    let dir = asset_cache_dir(key);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建缓存目录失败 {}: {e}", dir.display()))?;
    let mut out = Vec::with_capacity(assets.len());
    for a in assets {
        // 容器内可能是 `assets/song.ogg` 这种带目录的名字：**只取文件名**落到缓存里，
        // 避免越界写（`../` 之类）—— 名字只用于定位，不参与"写到哪"
        let file = Path::new(&a.name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("asset.bin");
        let p = dir.join(file);
        std::fs::write(&p, &a.data).map_err(|e| format!("写缓存失败 {}: {e}", p.display()))?;
        out.push((a.name.clone(), p));
    }
    Ok(out)
}

/// 缓存 key：容器内容的 hash（同样的包落到同一个目录，省得反复解压）
pub fn cache_key(bytes: &[u8]) -> String {
    format!("{:08x}", zip::crc32(bytes))
}

/// 收资源：文档引用的 `meta.audio` / `meta.background` 若不在已有资源里，
/// 就从 `base_dir` 读进来（把"引用外部文件"变成"装进包里"）。
///
/// 读不到只**记警告**（用户可能把音乐放在别处了），不阻断保存。
pub fn collect_assets(
    doc: &Document,
    have: &[zip::Entry],
    base_dir: Option<&Path>,
    fid: &mut Fidelity,
) -> Vec<zip::Entry> {
    let mut out: Vec<zip::Entry> = have.to_vec();
    for (what, want) in [
        ("音乐", doc.meta.audio.as_deref()),
        ("曲绘/背景", doc.meta.background.as_deref()),
    ] {
        let Some(name) = want.map(str::trim).filter(|n| !n.is_empty()) else {
            continue;
        };
        if out.iter().any(|a| a.name == name) {
            continue; // 已经在包里（例如刚从容器载入）
        }
        let Some(base) = base_dir else {
            fid.warn(format!("{what} `{name}` 不在资源里，且不知道从哪个目录找 —— 容器将缺少它"));
            continue;
        };
        let p = if Path::new(name).is_absolute() {
            PathBuf::from(name)
        } else {
            base.join(name)
        };
        // 包内条目名用**文件名**：容器是自包含的，不该把宿主机的绝对路径带进去
        // （文档字段的同步改写由调用方负责，见 `EditCore::normalize_asset_names`）
        let entry_name = Path::new(name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(name)
            .to_owned();
        match std::fs::read(&p) {
            Ok(data) => {
                fid.note(format!(
                    "{what} `{name}`（{} KiB）已装入容器，包内名 `{entry_name}`",
                    data.len() / 1024
                ));
                out.push(zip::Entry { name: entry_name, data });
            }
            Err(e) => fid.warn(format!(
                "{what} `{name}` 读不到（{}: {e}）—— 容器将缺少它",
                p.display()
            )),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Beat, BpmEntry, JudgeLine, Meta, Note, NoteKind};

    fn doc_with_assets() -> Document {
        let mut doc = Document::fresh(
            Meta {
                name: "容器测试".to_owned(),
                composer: "作曲".to_owned(),
                charter: "谱师".to_owned(),
                illustrator: String::new(),
                difficulty: "IN".to_owned(),
                level: "15".to_owned(),
                constant: None,
                offset_ms: 12,
                audio: Some("song.ogg".to_owned()),
                background: Some("bg.png".to_owned()),
                foreign: Default::default(),
            },
            174.0,
        );
        doc.judge_lines = vec![JudgeLine::default()];
        doc.bpm_list = vec![BpmEntry { start: Beat::zero(), bpm: 174.0, foreign: Default::default() }];
        doc.judge_lines[0].notes.push(Note::new(NoteKind::Tap, Beat::new(1, 1), 0.0));
        doc
    }

    /// 容器往返：谱面与资源都要完好回来
    #[test]
    fn container_roundtrip_keeps_chart_and_assets() {
        let doc = doc_with_assets();
        let assets = vec![
            zip::Entry { name: "song.ogg".to_owned(), data: vec![1u8, 2, 3, 4] },
            zip::Entry { name: "bg.png".to_owned(), data: b"\x89PNG".to_vec() },
            // 不认识的条目也要带过去（不许悄悄丢）
            zip::Entry { name: "extra/readme.txt".to_owned(), data: b"hello".to_vec() },
        ];
        let (bytes, backend) = write(&doc, &assets).unwrap();
        assert!(zip::looks_like_zip(&bytes));
        let entries = zip::read(&bytes).unwrap();
        assert!(
            entries.iter().any(|e| e.name == CHART_NAME),
            "谱面要在包里（条目顺序由打包器决定，只保证按名字能查到）：{:?}",
            entries.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        assert!(backend == zip::Backend::SevenZip || backend == zip::Backend::Builtin);

        let (c, fid) = read(&bytes).unwrap();
        assert_eq!(c.doc.meta.name, "容器测试");
        assert_eq!(c.doc.meta.audio.as_deref(), Some("song.ogg"));
        assert_eq!(c.doc.judge_lines[0].notes.len(), 1);
        assert_eq!(c.assets.len(), 3, "资源一条不少");
        assert_eq!(zip::get(&c.assets, "song.ogg").unwrap(), &[1u8, 2, 3, 4]);
        assert!(fid.is_lossless(), "{:?}", fid.warnings);
        // 文档里写的资源都在包里 ⇒ 报告应说清楚
        assert!(fid.conversions.iter().any(|c| c.contains("音乐 `song.ogg` 在容器内")));
    }

    /// 文档引用的资源不在包里 → **报警告**（不静默产出缺音乐的包）
    #[test]
    fn missing_asset_is_reported() {
        let doc = doc_with_assets();
        let (bytes, _backend) = write(&doc, &[]).unwrap();
        let (_c, fid) = read(&bytes).unwrap();
        assert!(!fid.is_lossless());
        assert!(fid.warnings.iter().any(|w| w.contains("音乐 `song.ogg` **不在容器内**")), "{:?}", fid.warnings);
        assert!(fid.warnings.iter().any(|w| w.contains("曲绘/背景")));
    }

    /// 没有谱面的包要**明确拒绝**并列出实际条目
    #[test]
    fn container_without_chart_is_rejected() {
        let bytes = zip::pack(&[("song.ogg".to_owned(), vec![0u8; 4])]);
        let err = read(&bytes).unwrap_err();
        assert!(err.contains("没有谱面"), "{err}");
        assert!(err.contains("song.ogg"), "错误信息要列出实际条目：{err}");
        // 裸 JSON 不是容器
        assert!(read(b"{\"format\":\"opm\"}").is_err());
    }

    /// 收集资源：文档引用外部文件时把它们装进包；找不到只警告
    #[test]
    fn collect_assets_pulls_files_from_disk() {
        let dir = std::env::temp_dir().join(format!("opm-container-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("song.ogg"), b"music-bytes").unwrap();
        let doc = doc_with_assets();
        let mut fid = Fidelity::new("opm", "容器".to_owned());
        let assets = collect_assets(&doc, &[], Some(&dir), &mut fid);
        assert_eq!(zip::get(&assets, "song.ogg").unwrap(), b"music-bytes");
        // 文档里写的是**绝对路径**，包内条目名只取文件名
        assert!(
            !assets.iter().any(|a| a.name.contains('/')),
            "包内不该出现带目录的条目名：{:?}",
            assets.iter().map(|a| &a.name).collect::<Vec<_>>()
        );
        assert!(fid.conversions.iter().any(|c| c.contains("已装入容器")));
        // 曲绘不存在 → 警告，但不 panic
        assert!(fid.warnings.iter().any(|w| w.contains("曲绘/背景")), "{:?}", fid.warnings);

        // 已经在包里的不再重复读盘
        let mut fid2 = Fidelity::new("opm", "容器".to_owned());
        let have = vec![zip::Entry { name: "song.ogg".to_owned(), data: b"in-package".to_vec() }];
        let again = collect_assets(&doc, &have, Some(&dir), &mut fid2);
        assert_eq!(zip::get(&again, "song.ogg").unwrap(), b"in-package", "包里的优先");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 资源摊到缓存：只取文件名（容器内的 `../` 不能写到缓存目录之外）
    #[test]
    fn extraction_is_sandboxed_to_the_cache_dir() {
        let assets = vec![
            zip::Entry { name: "../../evil.txt".to_owned(), data: b"nope".to_vec() },
            zip::Entry { name: "assets/sub/song.ogg".to_owned(), data: b"ok".to_vec() },
        ];
        let out = extract_assets(&assets, "test-sandbox").unwrap();
        let dir = asset_cache_dir("test-sandbox");
        for (_, p) in &out {
            assert!(p.starts_with(&dir), "落盘路径越界：{}", p.display());
        }
        assert!(dir.join("evil.txt").exists());
        assert!(dir.join("song.ogg").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
