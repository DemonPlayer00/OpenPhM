//! CJK 字体装载。
//!
//! 来自 S3 spike 的实测教训（见 `OpenPhM-框架选型.md` §7.1）：
//! 1. egui 默认字体**不含 CJK 字形**，不装载就是一片豆腐块；
//! 2. `NotoSansCJK-Regular.ttc` 的字面索引是 **0=JP、1=KR、2=SC、3=TC、4=HK**，
//!    而 `FontData::from_owned` **固定用索引 0** ⇒ 直接加载会让**中文用上日文字形**且不报错。
//!    因此这里优先选单字面 SC 字体；退化到集合时，用 `fc-scan` 查出 SC 的正确索引。

use std::borrow::Cow;
use std::sync::Arc;

/// 优先单字面 SC 字体（无索引歧义）
const SINGLE_FACE: &[&str] = &["Source Han Sans CN", "SourceHanSansCN", "SimHei", "WenQuanYi Micro Hei"];
/// 集合型回退（需要正确的字面索引）
const COLLECTION: &[&str] = &["Noto Sans CJK SC", "Noto Sans SC", "sans-serif:lang=zh-cn"];

pub struct LoadedFont {
    pub desc: String,
    pub index: u32,
}

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn fc_match(pattern: &str) -> Option<String> {
    let p = run("fc-match", &["-f", "%{file}", pattern])?;
    (!p.is_empty()).then_some(p)
}

/// 在集合文件里找出指定字族的索引
fn face_index(path: &str, want: &str) -> Option<u32> {
    let table = run("fc-scan", &["--format", "%{index}\t%{family}\n", path])?;
    table.lines().find_map(|l| {
        let (idx, fam) = l.split_once('\t')?;
        (fam.contains(want) && !fam.contains("Mono")).then(|| idx.trim().parse().ok())?
    })
}

/// 装载 CJK 字体并注册进 egui，返回实际使用的字体描述
pub fn install(ctx: &egui::Context) -> Option<LoadedFont> {
    let mut fonts = egui::FontDefinitions::default();

    let (path, index, desc) = {
        let mut chosen = None;
        for cand in SINGLE_FACE {
            if let Some(p) = fc_match(cand) {
                chosen = Some((p, 0, format!("{cand}（单字面）")));
                break;
            }
        }
        if chosen.is_none() {
            for cand in COLLECTION {
                if let Some(p) = fc_match(cand) {
                    let idx = face_index(&p, "SC").unwrap_or(0);
                    chosen = Some((p, idx, format!("{cand}（集合，字面索引 {idx}）")));
                    break;
                }
            }
        }
        chosen?
    };

    let bytes = std::fs::read(&path).ok()?;
    fonts.font_data.insert(
        "cjk".to_owned(),
        Arc::new(egui::FontData {
            font: Cow::Owned(bytes),
            index,
            tweak: Default::default(),
        }),
    );
    // 主字体放在最前，其余为回退
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().insert(0, "cjk".to_owned());
    }
    ctx.set_fonts(fonts);

    Some(LoadedFont {
        desc: format!("{desc} <- {path}"),
        index,
    })
}

/// 韩文等其它语种的回退（同一 .ttc 里的 KR 字面）。找不到就静默跳过。
pub fn install_kr_fallback(ctx: &egui::Context) {
    const KR_TTC: &str = "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc";
    if !std::path::Path::new(KR_TTC).exists() {
        return;
    }
    let Some(idx) = face_index(KR_TTC, "KR") else { return };
    let Ok(bytes) = std::fs::read(KR_TTC) else { return };

    let mut fonts = ctx.fonts(|f| f.definitions().clone());
    fonts.font_data.insert(
        "kr".to_owned(),
        Arc::new(egui::FontData {
            font: Cow::Owned(bytes),
            index: idx,
            tweak: Default::default(),
        }),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        if !list.contains(&"kr".to_owned()) {
            list.push("kr".to_owned());
        }
    }
    ctx.set_fonts(fonts);
}
