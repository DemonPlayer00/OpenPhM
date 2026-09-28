//! CJK 字体装载：**自带一份**，不依赖系统字体。
//!
//! 为什么要自带（2026-09-28，用户："自带cjk字体以防止文字变为方块"）：
//! - **Windows 上没有 fontconfig**：以前这里靠 `fc-match`/`fc-scan` 找字体，那段在 Windows 上
//!   直接失效 ⇒ 中文会整片变成豆腐块。现场复现：Wine 的默认前缀里 `C:\windows\Fonts` **是空的**，
//!   连拉丁字体都没有。
//! - 顺带把 Linux 侧那两个子进程也去掉了：字体字节就在可执行文件里，**不读磁盘、不 fork** ——
//!   启动更快（省掉 `fc-match` + `fc-scan` + 8 MB 读盘），而且**两个平台渲染完全一致**。
//!
//! 代价：可执行文件 +8.4 MB（思源黑体 CN Regular，OFL-1.1，见 `assets/fonts/`，随字体带许可原文）。
//! 想换字体：`OPM_FONT=<字体文件>[:字面索引]`（解析是纯函数、有单测；指了却读不到 ⇒ 回退内嵌）。
//!
//! S3 spike 的教训（§7.1）仍然成立：`NotoSansCJK-Regular.ttc` 的字面索引是 **0=JP / 1=KR / 2=SC**，
//! 而 `FontData::from_owned` **固定用索引 0** ⇒ 加载集合会让**中文用上日文字形**且不报错。
//! 内嵌的是**单字面 SC** 的 OTF（索引 0 就是 SC），从根上避开这个坑。

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 内嵌字体：思源黑体 CN Regular（OFL-1.1，单字面 SC，索引 0）
const EMBEDDED: &[u8] = include_bytes!("../assets/fonts/SourceHanSansCN-Regular.otf");
const EMBEDDED_DESC: &str = "内嵌 思源黑体 CN Regular（OFL-1.1）";
/// 许可说明（`--fonts` 与文档共用一句话：OFL 要求随字体附带许可原文）
pub const LICENSE_NOTE: &str =
    "思源黑体 CN Regular © Adobe，SIL Open Font License 1.1（原文见 app/assets/fonts/SourceHanSansCN-LICENSE.txt）";

/// 韩文回退用的系统字体（仅 Linux；找不到就静默跳过，见 [`install_kr_fallback`]）
const KR_TTC: &str = "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc";

pub struct LoadedFont {
    pub desc: String,
    pub index: u32,
}

/// 用哪份字体
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// 内嵌的那份（默认：没有路径、没有索引、没有子进程）
    Embedded,
    /// `OPM_FONT` 指定的外部字体
    File { path: PathBuf, index: u32 },
}

/// 解析 `OPM_FONT=路径[:索引]`（**纯函数**）。
///
/// `:` 在 Windows 路径里到处都是（`C:\Windows\Fonts\msyh.ttc`），所以只有**结尾的 `:<数字>`**
/// 才算字面索引 —— 这也正是这个函数值得单测的原因。
pub fn parse_override(spec: &str) -> (PathBuf, u32) {
    let spec = spec.trim();
    if let Some((head, tail)) = spec.rsplit_once(':') {
        if !head.is_empty() && !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            return (PathBuf::from(head), tail.parse().unwrap_or(0));
        }
    }
    (PathBuf::from(spec), 0)
}

/// 决定用哪份字体（**纯函数**：环境由调用方给，于是可测）
pub fn choice_from_env(env: &dyn Fn(&str) -> Option<String>) -> Choice {
    match env("OPM_FONT").map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()) {
        Some(spec) => {
            let (path, index) = parse_override(&spec);
            Choice::File { path, index }
        }
        None => Choice::Embedded,
    }
}

/// 读取选定的字体字节。`OPM_FONT` 指了却读不到 ⇒ **回退内嵌**（不让程序因为一个坏路径变豆腐块）。
fn resolve(choice: &Choice) -> (Cow<'static, [u8]>, u32, String) {
    match choice {
        Choice::Embedded => (Cow::Borrowed(EMBEDDED), 0, EMBEDDED_DESC.to_owned()),
        Choice::File { path, index } => match std::fs::read(path) {
            Ok(bytes) => (
                Cow::Owned(bytes),
                *index,
                format!("OPM_FONT={}（字面索引 {index}）", path.display()),
            ),
            Err(e) => (
                Cow::Borrowed(EMBEDDED),
                0,
                format!("{EMBEDDED_DESC} ← OPM_FONT={} 读不到（{e}）", path.display()),
            ),
        },
    }
}

/// 装载 CJK 字体并注册进 egui。
///
/// **不会失败**：最差也是内嵌那份 —— 所以调用点不再需要"没找到字体，中文将是豆腐块"那条告警
/// （那个状态已经不存在了）。
pub fn install(ctx: &egui::Context) -> LoadedFont {
    let (bytes, index, desc) = resolve(&choice_from_env(&|k| std::env::var(k).ok()));
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".to_owned(),
        Arc::new(egui::FontData { font: bytes, index, tweak: Default::default() }),
    );
    // 主字体放在最前，其余为回退
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().insert(0, "cjk".to_owned());
    }
    ctx.set_fonts(fonts);
    LoadedFont { desc, index }
}

// ---------------------------------------------------------------- 自检（`--fonts`）

/// 探针：**必须都有字形**的一串字。三类混在一起 ——
/// ① 界面自己的词汇（缺了就是界面上的豆腐块）；② 常见曲名/汉字与标点；③ 拉丁、数字、假名。
///
/// 为什么值得当测试：字体"能加载"与"能显示"是两件事，而缺字形在界面上就是一片方框 ——
/// 用 egui 自己的 `has_glyphs` 逐字问一遍，比肉眼看截图可靠，而且无头、在 Windows 上也能跑。
pub const PROBE: &str = "\
新建 打开 保存 另存为 关闭 退出 撤销 重做 播放 暂停 停止 时间 缩放 网格 音符 判定线 事件 轨道 \
不透明度 旋转 移动 流速 缓动 重叠 冲突 校验 提示 警告 错误 缺少 获取 文件 编辑 视图 帮助 设置 \
起始界面 最近打开 控制通道 未保存 已保存 有无 改动 拍 秒 毫秒 偏移 音量 时长 宽度 高度 版本 \
光 影 梦 夜 星 空 花 火 雪 雨 风 云 海 山 心 恋 爱 永 远 未 来 时 间 记 忆 旅 人 世 界 命 运 终 焉 \
之 歌 曲 舞 白 黑 红 蓝 绿 紫 金 银 色 樱 蝶 翼 刃 剑 雷 电 冰 霜 月 日 天 地 生 死 约 定 誓 言 \
中 文 字 体 汉 语 编 谱 音 乐 游 戏 图 形 界 面 按 钮 输 入 输 出 显 示 屏 幕 键 盘 鼠 标 路 径 目 录 名 称 \
，。、；：？！（）「」『』《》〈〉…—·～＋－×÷＝％＆＠＃ \
ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789 \
!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~ \
あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほ アイウエオカキクケコサシスセソタチツテト";

/// 自检结果：装载了哪份字体、探针里有没有缺字形的
pub struct Coverage {
    pub desc: String,
    pub index: u32,
    /// 探针字数（去重后）
    pub probes: usize,
    /// 缺字形的字（空 = 界面不会出豆腐块）
    pub missing: Vec<char>,
}

/// 无头自检：造一个 egui `Context`、装上字体、逐字问"有没有字形"。
///
/// 不需要窗口与 GPU，所以 `opm-app --fonts` 在任何平台（含 Wine、CI）都能跑。
pub fn check() -> Coverage {
    let ctx = egui::Context::default();
    let loaded = install(&ctx);
    // egui 得先跑一帧才有字体表（在那之前 `fonts_mut` 会 panic："No fonts available until
    // first call to Context::run()"）。跑一帧就够了，图集不用留。
    let mut out = ctx.run_ui(egui::RawInput::default(), |_ui| {});
    out.textures_delta.clear();
    let probes: Vec<char> = {
        let mut v: Vec<char> = PROBE.chars().filter(|c| !c.is_whitespace()).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let missing = ctx.fonts_mut(|f| {
        probes
            .iter()
            .copied()
            .filter(|c| !f.has_glyphs(&egui::FontId::proportional(16.0), &c.to_string()))
            .collect::<Vec<char>>()
    });
    Coverage { desc: loaded.desc, index: loaded.index, probes: probes.len(), missing }
}

impl Coverage {
    /// 一行摘要（`opm-app --fonts` 与启动日志共用同一种说法）
    pub fn summary(&self) -> String {
        let base = format!("{}（字面索引 {}，探针 {} 字）", self.desc, self.index, self.probes);
        if self.missing.is_empty() {
            format!("{base} —— CJK 覆盖完整，不会出现豆腐块")
        } else {
            format!(
                "{base} —— ⚠️ 缺 {} 个字形：{}",
                self.missing.len(),
                self.missing.iter().collect::<String>()
            )
        }
    }
}

/// 韩文等其它语种的回退（同一 `.ttc` 里的 KR 字面）。**仅 Linux**：字体不在就静默跳过。
///
/// 内嵌的思源黑体 CN 覆盖中日标点与假名，但**不含韩文**；Linux 上能借到系统那份 Noto CJK。
pub fn install_kr_fallback(ctx: &egui::Context) {
    if !Path::new(KR_TTC).exists() {
        return;
    }
    let Some(idx) = face_index(KR_TTC, "KR") else { return };
    let Ok(bytes) = std::fs::read(KR_TTC) else { return };

    let mut fonts = ctx.fonts(|f| f.definitions().clone());
    fonts.font_data.insert(
        "kr".to_owned(),
        Arc::new(egui::FontData { font: Cow::Owned(bytes), index: idx, tweak: Default::default() }),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(family).or_default();
        if !list.contains(&"kr".to_owned()) {
            list.push("kr".to_owned());
        }
    }
    ctx.set_fonts(fonts);
}

/// 在集合文件里找出指定字族的索引（`fc-scan`；只在 Linux 的韩文回退里用到）
fn face_index(path: &str, want: &str) -> Option<u32> {
    let out = std::process::Command::new("fc-scan")
        .args(["--format", "%{index}\t%{family}\n", path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let table = String::from_utf8_lossy(&out.stdout).to_string();
    table.lines().find_map(|l| {
        let (idx, fam) = l.split_once('\t')?;
        (fam.contains(want) && !fam.contains("Mono")).then(|| idx.trim().parse().ok())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // "把环境当输入"的闭包：实现搬到 `testkit`（显卡策略那边也用它）
    use crate::testkit::env_of;

    /// 默认 = 内嵌（不读任何系统字体）
    #[test]
    fn default_is_the_embedded_font() {
        assert_eq!(choice_from_env(&env_of(&[])), Choice::Embedded);
        assert_eq!(choice_from_env(&env_of(&[("OPM_FONT", "   ")])), Choice::Embedded);
        // 内嵌字节确实是字体（OTTO = CFF/OpenType）且体量对得上
        assert_eq!(&EMBEDDED[..4], b"OTTO", "内嵌的应当是 OTF/CFF 字体");
        assert!(EMBEDDED.len() > 1_000_000, "内嵌字体才 {} 字节，可疑", EMBEDDED.len());
    }

    /// `OPM_FONT` 的解析：结尾 `:数字` 才是字面索引 —— **Windows 路径里的冒号不能切错**
    #[test]
    fn override_specs_parse_paths_and_indices() {
        assert_eq!(parse_override("/a/b.otf"), (PathBuf::from("/a/b.otf"), 0));
        assert_eq!(parse_override("/a/b.ttc:2"), (PathBuf::from("/a/b.ttc"), 2));
        assert_eq!(parse_override("  /a/b.ttc:12 "), (PathBuf::from("/a/b.ttc"), 12));
        // Windows 路径：盘符的冒号不是索引分隔符
        assert_eq!(
            parse_override(r"C:\Windows\Fonts\msyh.ttc"),
            (PathBuf::from(r"C:\Windows\Fonts\msyh.ttc"), 0)
        );
        assert_eq!(
            parse_override(r"C:\Windows\Fonts\msyh.ttc:1"),
            (PathBuf::from(r"C:\Windows\Fonts\msyh.ttc"), 1)
        );
        // 结尾不是数字 ⇒ 整串都是路径
        assert_eq!(parse_override("/a/b:c"), (PathBuf::from("/a/b:c"), 0));
        assert_eq!(
            choice_from_env(&env_of(&[("OPM_FONT", "/x/y.otf:3")])),
            Choice::File { path: PathBuf::from("/x/y.otf"), index: 3 }
        );
    }

    /// 指了却读不到 ⇒ 回退内嵌（而不是"没字体了"）
    #[test]
    fn unreadable_override_falls_back_to_the_embedded_font() {
        let choice = Choice::File { path: PathBuf::from("/nonexistent/nope.otf"), index: 5 };
        let (bytes, index, desc) = resolve(&choice);
        assert_eq!(&bytes[..4], b"OTTO");
        assert_eq!(index, 0, "回退时索引也要回到 0（内嵌的是单字面）");
        assert!(desc.contains("读不到"), "{desc}");
        // 能读到的就用它
        let dir = std::env::temp_dir().join(format!("opm-font-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.otf");
        std::fs::write(&p, EMBEDDED).unwrap();
        let (bytes, index, desc) = resolve(&Choice::File { path: p, index: 2 });
        assert_eq!(index, 2);
        assert_eq!(bytes.len(), EMBEDDED.len());
        assert!(desc.starts_with("OPM_FONT="), "{desc}");
    }

    /// **这是"不会出豆腐块"的真测试**：把内嵌字体装进一个无头 egui，逐字问有没有字形。
    ///
    /// 不需要窗口、不需要 GPU、不需要系统字体 —— 所以它在 Linux 与 Windows 上是同一条断言。
    #[test]
    fn embedded_font_covers_the_probe_text() {
        let c = check();
        assert!(c.probes > 300, "探针只有 {} 字，太少", c.probes);
        assert!(c.desc.contains("内嵌"), "{}", c.desc);
        assert!(c.missing.is_empty(), "缺字形：{:?}", c.missing);
        assert!(c.summary().contains("不会出现豆腐块"), "{}", c.summary());
    }

    /// 探针本身要"值得问"：中文、拉丁、标点、假名都得在里面（否则上面那条测试是空的）
    #[test]
    fn probe_covers_the_kinds_of_text_the_ui_shows() {
        for c in ['中', '文', '判', '定', '线', 'A', 'z', '0', '，', '。', '「', 'あ', 'ア'] {
            assert!(PROBE.contains(c), "探针里没有 {c:?}");
        }
        assert!(PROBE.contains("另存为") && PROBE.contains("不透明度"));
    }
}
