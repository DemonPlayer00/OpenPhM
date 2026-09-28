//! 系统文件对话框 —— "打开/另存为"要走**桌面自己的文件管理器对话框**，不是自绘一个。
//!
//! 为什么是 `kdialog`/`zenity` 而不是 `rfd` 或 `org.freedesktop.portal.FileChooser`：
//! - `rfd` 要往工程里塞 GTK/Qt 绑定，只为一个对话框不值当；
//! - 门户（xdg-desktop-portal）是"最规范"的那条路，但它要求 **D-Bus 客户端 + 等 Response 信号**
//!   （Request/Response 是异步的，`gdbus call` 只会拿到 handle），本机没有 zbus 之类可用依赖；
//! - `kdialog`（KDE）/ `zenity`（GTK）弹出的**就是**这两个桌面自己的文件对话框 ——
//!   在 Plasma 会话里 `kdialog` 出来的就是 Dolphin 风格的保存框，含覆盖确认、最近位置、书签，
//!   这正是"遵循系统规范"的实质。
//!
//! 本机实测（2026-09-27，Plasma 6 / Wayland）：`kdialog` ✓ `zenity` ✓ `dolphin` ✓
//! `xdg-desktop-portal-kde` 在跑（门户留作后续：要接它得先有 D-Bus 客户端）。
//!
//! **对话框是阻塞的**：弹出期间本进程不出帧（系统模态框的常规行为，与其它编辑器一致）。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 文件过滤器：**一个标签 + 一组通配**（系统框里那一行就是"谱面 (*.json)"）。
///
/// 为什么要有这个类型：以前只有一个 `CHART_FILTER: &str`，而**通配是写死的 `*.json`**
/// （`format!("{filter} | *.json")`）—— 于是"选音乐"的系统框也只列 json 文件（用户报的 bug）。
/// 现在标签与通配绑在一起：选音乐给音频通配、选曲绘给图片通配，各归各位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Filter {
    pub label: &'static str,
    /// 空格分隔的多个通配（kdialog 与 zenity 都认这种写法）
    pub patterns: &'static str,
}

impl Filter {
    /// 系统框里那一行：`谱面 (*.json)`
    pub fn spec(&self) -> String {
        format!("{} ({})", self.label, self.patterns)
    }
    /// 主通配（第一个）—— 需要"建议扩展名"的地方用它
    pub fn first_pattern(&self) -> &'static str {
        self.patterns.split_whitespace().next().unwrap_or("*")
    }
}

/// 谱面过滤器：**四种落盘的形态都要列出来**。
///
/// 通配曾经只有 `*.json`（那时只想着 opm 原生与 RPE 原生），于是"打开谱面"的系统框里
/// **根本看不见 `.opm` 与 `.pez`** —— 而这两个正是本编辑器自己存出来的打包形态。
/// 用户报的正是这一条（启动页与编辑页两个「打开谱面」都撞到了）。
///
/// `*.json` 排在最后：`*.opm.json` / `*.rpe.json` 也命中它，而打包形态是"一眼能认出来的谱面"，
/// 排前面更顺手（顺序只影响系统框里的排列，不影响能不能选中）。
pub const CHART_FILTER: Filter = Filter {
    label: "谱面",
    patterns: "*.opm *.pez *.json",
};
/// 音频：与 `audio.rs` 那边解码器（symphonia）真正支持的容器对齐
pub const AUDIO_FILTER: Filter = Filter {
    label: "音频",
    patterns: "*.ogg *.mp3 *.wav *.flac *.m4a *.aac *.opus *.mp4",
};
/// opm 包（保存对话框用：目标是 `.opm` 一个文件）
pub const OPM_FILTER: Filter = Filter { label: "opm 包", patterns: "*.opm" };
/// RPE 谱面包（保存对话框用：目标是 `.pez` 一个文件）
pub const PEZ_FILTER: Filter = Filter { label: "RPE 谱面包", patterns: "*.pez" };
/// 曲绘/背景：常见位图（Phigros 侧实际就是 png/jpg）
pub const IMAGE_FILTER: Filter = Filter {
    label: "曲绘",
    patterns: "*.png *.jpg *.jpeg *.webp *.bmp",
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Open,
    Save,
    /// 只选目录（"保存窗口"里的"选择文件夹"）
    Directory,
}

impl Which {
    pub fn verb(self) -> &'static str {
        match self {
            Which::Open => "打开",
            Which::Save => "另存为",
            Which::Directory => "选择文件夹",
        }
    }
}

/// 按优先级探测可用的对话框程序（KDE 优先：本会话就是 Plasma）
pub fn detect() -> Option<&'static str> {
    for prog in ["kdialog", "zenity"] {
        if which_exists(prog) {
            return Some(prog);
        }
    }
    None
}

fn which_exists(prog: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':').any(|dir| {
        let p = Path::new(dir).join(prog);
        p.is_file()
    })
}

/// 可用程序的人类可读说明（给 GUI 提示用）。
///
/// **缓存**：这行文字会被 UI 每帧取用（启动页右栏、文件对话框各一处），而算它要对 PATH 里
/// 每个目录做两次 `stat`。会话中途不会有人装上 kdialog，所以缓存是安全的；
/// `detect()` 本身仍是"即问即答"的纯探测（测试用它，不看缓存）。
pub fn availability() -> &'static str {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| match detect() {
            Some("kdialog") => "kdialog（KDE 系统文件对话框）".to_owned(),
            Some("zenity") => "zenity（GTK 系统文件对话框）".to_owned(),
            Some(other) => other.to_owned(),
            None => "无（将使用内置路径输入框）".to_owned(),
        })
        .as_str()
}

/// 构造命令行（**纯函数**：参数怎么拼在这里一眼可查，也有单测钉住）
///
/// `start` 给起始目录或"目录 + 建议文件名"（保存时用后者，系统框会预填名字并做覆盖确认）。
/// `filter` 决定**中间那一行列出什么** —— 谱面/音频/曲绘各有各的通配（见 [`Filter`]）。
pub fn args(
    prog: &str,
    which: Which,
    start: Option<&Path>,
    filter: Filter,
) -> Vec<String> {
    let title = format!("OpenPhM · {}", which.verb());
    match prog {
        "kdialog" => {
            let mut a = vec!["--title".to_owned(), title];
            a.push(match which {
                Which::Open => "--getopenfilename".to_owned(),
                Which::Save => "--getsavefilename".to_owned(),
                Which::Directory => "--getexistingdirectory".to_owned(),
            });
            a.push(
                start
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| ".".to_owned()),
            );
            if which != Which::Directory {
                // kdialog 的写法：`标签 (*.a *.b)`（**不是** zenity 的 `标签 | *.a`）
                a.push(filter.spec());
                a.push("所有文件 (*)".to_owned());
            }
            a
        }
        // zenity 的默认程序名，也接受显式路径（测试注入用）
        _ => {
            let mut a = vec!["--file-selection".to_owned(), format!("--title={title}")];
            if which == Which::Save {
                a.push("--save".to_owned());
                a.push("--confirm-overwrite".to_owned());
            }
            if which == Which::Directory {
                a.push("--directory".to_owned());
            }
            if let Some(s) = start {
                a.push(format!("--filename={}", s.display()));
            }
            if which != Which::Directory {
                // zenity 的写法：`--file-filter=标签 | *.a *.b`
                a.push(format!("--file-filter={} | {}", filter.label, filter.patterns));
                a.push("--file-filter=所有文件 | *".to_owned());
            }
            a
        }
    }
}

/// 弹系统对话框选一个文件。`Ok(None)` = 用户取消。
///
/// 取消与失败的区分：**标准输出为空就算取消**（两个程序取消时都不输出、退出码不为 0），
/// 只有"程序起不来"才算错误 —— 这样用户按 Esc 不会被报成故障。
pub fn pick(which: Which, start: Option<&Path>, filter: Filter) -> Result<Option<PathBuf>, String> {
    match detect() {
        Some(prog) => pick_with(prog, which, start, filter),
        None => Err("系统里没有 kdialog/zenity —— 请用内置路径输入框".to_owned()),
    }
}

/// 指定程序版本的 [`pick`]（测试注入假程序用，不必真的弹窗）
pub fn pick_with(
    prog: &str,
    which: Which,
    start: Option<&Path>,
    filter: Filter,
) -> Result<Option<PathBuf>, String> {
    let argv = args(prog, which, start, filter);
    let out = Command::new(prog)
        .args(&argv)
        .output()
        .map_err(|e| format!("无法启动 {prog}: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let path = text.trim();
    if path.is_empty() {
        return Ok(None); // 用户取消
    }
    Ok(Some(PathBuf::from(path)))
}

/// 选一个目录（"保存窗口"里的"选择文件夹"）。`Ok(None)` = 取消。
pub fn pick_folder(start: Option<&Path>) -> Result<Option<PathBuf>, String> {
    match detect() {
        Some(prog) => pick_with(prog, Which::Directory, start, CHART_FILTER),
        None => Err("系统里没有 kdialog/zenity —— 请直接输入文件夹".to_owned()),
    }
}

/// 打开文件时用哪个起始位置：优先"当前文件所在目录"
pub fn start_for_open(current: Option<&Path>) -> Option<PathBuf> {
    current
        .and_then(|p| p.parent())
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.to_path_buf())
        .or_else(|| std::env::current_dir().ok())
}

/// 另存为时的起始位置：**存在的目录 + 建议文件名**（系统框会预填、并做覆盖确认）
pub fn start_for_save(current: Option<&Path>, suggested_name: &str) -> Option<PathBuf> {
    let dir = match current.and_then(|p| p.parent()).filter(|d| !d.as_os_str().is_empty()) {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().ok()?,
    };
    Some(nearest_existing_dir(&dir).join(suggested_name))
}

/// 另存为的建议文件名：沿用原名，没有就用 `untitled.opm.json`
pub fn suggested_name(current: Option<&Path>) -> String {
    current
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "untitled.opm.json".to_owned())
}

/// 谱面名字 → 安全的文件名主干。
///
/// 名字是给人看的（`meta.name` 常带空格、冒号、书名号，甚至 `/`），不能直接当文件名用：
/// 路径分隔符与 Windows 保留字符换成 `-`，控制字符丢掉，首尾空白与点去掉，空名字兜底 `untitled`。
pub fn sanitize_stem(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || c == '/' || c == '\\' || "?:*\"<>|".contains(c) {
                '-'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_owned();
    if cleaned.is_empty() {
        "untitled".to_owned()
    } else {
        cleaned
    }
}

/// "文件夹 + 名字 + 扩展名" → 目标路径。相对文件夹按工作目录补全（预览里要给绝对路径，
/// 否则"到底存哪去了"只能靠猜）。
pub fn derive_path(folder: &str, stem: &str, ext: &str) -> PathBuf {
    let raw = if folder.trim().is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(folder.trim())
    };
    let dir = if raw.is_absolute() {
        raw
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(raw)
    };
    dir.join(format!("{}{}", sanitize_stem(stem), ext))
}

/// 从 `dir` 起向上找**最近的、真的存在的**目录（都不存在就退到工作目录）。
///
/// 这是"保存新文件时无法指定路径"的根因修复：`kdialog --getsavefilename <路径>` 的第一个参数
/// 是 **startDir** —— 传进去的路径若不存在，KDE 会把它当目录并报"目录不存在"，
/// 于是"新文件还没有对应文件"时反而指定不了路径。给系统框的起始位置必须落在**存在的目录**上。
pub fn nearest_existing_dir(dir: &Path) -> PathBuf {
    let mut cur = if dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        dir.to_path_buf()
    };
    if !cur.is_absolute() {
        cur = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(cur);
    }
    loop {
        if cur.is_dir() {
            return cur;
        }
        match cur.parent() {
            Some(p) if p != cur => cur = p.to_path_buf(),
            _ => return std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

/// 系统"另存为"的起始**文件路径**：父目录保证存在，文件名用曲名。
///
/// 返回的是"目录/名字"整串（KDE 会用它预填文件名），但**父目录一定是存在的**，
/// 所以"文件还不存在"不会让对话框拒绝这个起始位置。
pub fn start_for_new_save(dir: Option<&Path>, stem: &str, ext: &str) -> PathBuf {
    let base = dir
        .filter(|d| !d.as_os_str().is_empty())
        .map(nearest_existing_dir)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    base.join(format!("{}{}", sanitize_stem(stem), ext))
}

/// 用户从系统框里挑的路径可能没有扩展名 —— 按目标格式补上（Krita 也是这么做的）。
/// 判据只看末尾：已经带 `.json`（含 `.opm.json`）就原样返回。
pub fn ensure_extension(path: &Path, ext: &str) -> PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(ext) || name.ends_with(".json") {
        path.to_path_buf()
    } else {
        let mut s = path.as_os_str().to_os_string();
        s.push(ext);
        PathBuf::from(s)
    }
}

/// URL 白名单校验（**纯函数**）：只收 http(s)。
///
/// `file://`、裸路径、`javascript:` 一律拒 —— "让浏览器去开本地文件"是白送出去的一个口子，
/// 而这个函数的唯一用途是打开 7-zip.org。
///
/// 校验与"真的去开"分开，是为了让测试**不会弹出浏览器**：自动化里不该有这种副作用
/// （曾经这条测试真的把 example.com 与 7-zip 官网打开了）。
pub fn check_url(url: &str) -> Result<(), String> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Ok(())
    } else {
        Err(format!("只接受 http(s) URL：{url}"))
    }
}

/// 用**系统默认浏览器**打开一个 URL（"获取 7z"按钮用它）。
///
/// 三平台各一句：Windows `cmd /C start`（`start` 是 cmd 内建命令，必须经 cmd；空标题参数
/// 是为了让带引号的 URL 不被当成窗口标题）、macOS `open`、Linux `xdg-open`。
pub fn open_url(url: &str) -> Result<(), String> {
    check_url(url)?;
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    // Windows 上别闪控制台窗口
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("打不开浏览器：{e}"))
}

/// 在**系统文件管理器**里定位这个文件（这就是"调用系统文件管理器"的另一半）。
///
/// KDE 用 `dolphin --select`（会选中该文件而不是只打开目录）；退化到 `xdg-open` 打开所在目录。
pub fn reveal(path: &Path) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let spawned = if which_exists("dolphin") {
        Command::new("dolphin").arg("--select").arg(path).spawn()
    } else {
        Command::new("xdg-open").arg(dir).spawn()
    };
    spawned.map(|_| ()).map_err(|e| format!("无法打开文件管理器: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 命令行拼装：KDE 与 GTK 两套语法都要对（写错就是"点了没反应"）
    #[test]
    fn command_lines_are_built_per_program() {
        let dir = Path::new("/tmp/charts");
        let a = args("kdialog", Which::Save, Some(&dir.join("x.opm.json")), CHART_FILTER);
        assert_eq!(a[0], "--title");
        assert_eq!(a[1], "OpenPhM · 另存为");
        assert_eq!(a[2], "--getsavefilename");
        assert_eq!(a[3], "/tmp/charts/x.opm.json", "保存时要把建议文件名一起给出去");
        assert!(a.iter().any(|s| s.contains("*.json")));

        let a = args("kdialog", Which::Open, None, CHART_FILTER);
        assert_eq!(a[2], "--getopenfilename");
        assert_eq!(a[3], ".", "没给起始位置就退到当前目录，而不是空字符串");

        let a = args("zenity", Which::Save, Some(&dir.join("y.json")), CHART_FILTER);
        assert_eq!(a[0], "--file-selection");
        assert!(a.contains(&"--save".to_owned()));
        assert!(a.contains(&"--confirm-overwrite".to_owned()), "保存要有覆盖确认");
        assert!(a.iter().any(|s| s == "--filename=/tmp/charts/y.json"));

        let a = args("zenity", Which::Open, None, CHART_FILTER);
        assert!(!a.contains(&"--save".to_owned()), "打开不该带 --save");

        // **每个过滤器各带自己的通配** —— 这条是用户报的 bug（"选音乐的系统框只列 json"）的回归测试
        let a = args("kdialog", Which::Open, None, AUDIO_FILTER);
        assert!(
            a.iter().any(|s| s == "音频 (*.ogg *.mp3 *.wav *.flac *.m4a *.aac *.opus *.mp4)"),
            "{a:?}"
        );
        assert!(!a.iter().any(|s| s.contains("*.json")), "选音乐不该出现 json 通配：{a:?}");
        let a = args("zenity", Which::Open, None, AUDIO_FILTER);
        assert!(
            a.iter().any(|s| s
                == "--file-filter=音频 | *.ogg *.mp3 *.wav *.flac *.m4a *.aac *.opus *.mp4"),
            "{a:?}"
        );
        assert!(a.contains(&"--file-filter=所有文件 | *".to_owned()), "所有文件要留着兜底：{a:?}");
        // 曲绘走图片通配
        let a = args("kdialog", Which::Open, None, IMAGE_FILTER);
        assert!(a.iter().any(|s| s == "曲绘 (*.png *.jpg *.jpeg *.webp *.bmp)"), "{a:?}");
        assert!(!a.iter().any(|s| s.contains("*.json")), "{a:?}");
        let a = args("zenity", Which::Open, None, IMAGE_FILTER);
        assert!(
            a.iter().any(|s| s == "--file-filter=曲绘 | *.png *.jpg *.jpeg *.webp *.bmp"),
            "{a:?}"
        );
        // 三个过滤器的形状约定（`spec`/`first_pattern` 是拼命令行与建议扩展名的公共入口）
        for f in [CHART_FILTER, AUDIO_FILTER, IMAGE_FILTER] {
            assert_ne!(f.patterns, "*");
            assert!(f.spec().starts_with(f.label), "{f:?}");
            assert!(f.first_pattern().starts_with("*."), "{f:?}");
        }

        // 只选目录：KDE 用 --getexistingdirectory，GTK 用 --directory，且都不该带文件过滤器
        let a = args("kdialog", Which::Directory, Some(dir), CHART_FILTER);
        assert_eq!(a[2], "--getexistingdirectory");
        assert!(!a.iter().any(|s| s.contains("*.json")), "选目录不需要文件过滤器：{a:?}");
        let a = args("zenity", Which::Directory, Some(dir), CHART_FILTER);
        assert!(a.contains(&"--directory".to_owned()));
        assert!(!a.iter().any(|s| s.starts_with("--file-filter")), "{a:?}");
    }

    /// **打开谱面要列全四种落盘形态**（用户报的 bug：系统框里只有 json ⇒ 看不见 `.opm`/`.pez`）。
    ///
    /// 这条测试盯的是"两个页面的打开按钮用的是同一个过滤器"这个事实：
    /// 启动页的「打开谱面…」与编辑页的「打开…」都走 `open_via_system`，所以只要这一个常量对，
    /// 两个入口就都对。此处断言命令行里真的列出了那四个通配。
    #[test]
    fn the_chart_filter_lists_every_shape_we_can_save() {
        assert_eq!(CHART_FILTER.patterns, "*.opm *.pez *.json");
        for (prog, want) in [
            ("kdialog", "谱面 (*.opm *.pez *.json)"),
            ("zenity", "--file-filter=谱面 | *.opm *.pez *.json"),
        ] {
            let a = args(prog, Which::Open, None, CHART_FILTER);
            assert!(a.iter().any(|s| s == want), "{prog} 里应列出全部形态：{a:?}");
            for ext in ["*.opm", "*.pez", "*.json"] {
                assert!(a.iter().any(|s| s.contains(ext)), "{prog} 少了 {ext}：{a:?}");
            }
        }
        // kdialog 的"所有文件"是 `标签 (*)`，**不是** zenity 的 `标签 | *`（写错就是一条选不动的行）
        let a = args("kdialog", Which::Open, None, CHART_FILTER);
        assert!(a.contains(&"所有文件 (*)".to_owned()), "{a:?}");
        assert!(!a.iter().any(|s| s == "所有文件 | *"), "kdialog 不吃竖线写法：{a:?}");
        let a = args("zenity", Which::Open, None, CHART_FILTER);
        assert!(a.contains(&"--file-filter=所有文件 | *".to_owned()), "{a:?}");
    }

    /// 起始位置与建议名
    #[test]
    fn start_paths_follow_the_current_file() {
        // 用一个**真实存在**的目录：起始位置现在保证落在存在的目录上（见上一个测试），
        // 拿不存在的假路径会让它按设计向上退，那样测的就不是"跟随当前文件"了
        let tmp = std::env::temp_dir();
        let cur = Some(tmp.join("song.opm.json"));
        let cur = cur.as_deref();
        assert_eq!(start_for_open(cur).unwrap(), tmp);
        assert_eq!(start_for_save(cur, "song.opm.json").unwrap(), tmp.join("song.opm.json"));
        assert_eq!(suggested_name(cur), "song.opm.json");
        assert_eq!(suggested_name(None), "untitled.opm.json");
        // 没有当前文件时退到工作目录，不能返回 None 让系统框随机开在别处
        assert!(start_for_open(None).is_some());
    }

    /// 管道：程序输出路径 → 返回那个路径；空输出 → 取消；起不来 → 报错
    #[test]
    fn pick_pipes_stdout_and_treats_empty_as_cancel() {
        let dir = std::env::temp_dir().join(format!("opm-filedialog-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stub = |name: &str, body: &str| -> PathBuf {
            let p = dir.join(name);
            let mut f = std::fs::File::create(&p).unwrap();
            writeln!(f, "#!/bin/sh").unwrap();
            writeln!(f, "{body}").unwrap();
            drop(f);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            p
        };

        let ok = stub("ok.sh", "echo /tmp/charts/picked.json");
        let got = pick_with(ok.to_str().unwrap(), Which::Save, None, CHART_FILTER).unwrap();
        assert_eq!(got, Some(PathBuf::from("/tmp/charts/picked.json")));

        // 取消：不输出（真实 kdialog/zenity 取消时就是这样，退出码可能是 1）
        let cancel = stub("cancel.sh", "exit 1");
        assert_eq!(pick_with(cancel.to_str().unwrap(), Which::Open, None, CHART_FILTER).unwrap(), None);

        // 输出里带空白也要能解析（有些程序会多打一个换行）
        let spaced = stub("spaced.sh", "printf '  /tmp/a b.json  \\n'");
        assert_eq!(
            pick_with_retrying_text_busy(spaced.to_str().unwrap()),
            Some(PathBuf::from("/tmp/a b.json"))
        );

        // 程序不存在 → 明确报错，而不是当成"用户取消"
        let err = pick_with("/nonexistent/kdialog", Which::Open, None, CHART_FILTER).unwrap_err();
        assert!(err.contains("无法启动"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `pick_with` 的"脚本刚写完就 exec"版本：`ETXTBSY`（Text file busy）时重试。
    ///
    /// 这是**测试环境**的竞态，不是被测代码的性质：`cargo test` 把整个 crate 的用例并行跑在
    /// 一个进程里，而并行的别的用例正在 `Command::spawn`（7z / `sh`）—— fork 出来的子进程在 exec
    /// 之前持有 fd 表的副本，于是"刚写完的脚本"在这一瞬间可能被判为"正在被写入"而拒绝执行。
    /// 实测：147 个用例并行跑时大约每二十次复现一次。脚本内容与解析逻辑都没变，
    /// 所以这里只对**这一个错误码**重试到 1 秒（别的错误照旧当场失败）。
    #[cfg(test)]
    fn pick_with_retrying_text_busy(program: &str) -> Option<PathBuf> {
        let mut last = None;
        for _ in 0..20 {
            match pick_with(program, Which::Open, None, CHART_FILTER) {
                Ok(v) => return v,
                Err(e) if e.contains("Text file busy") => {
                    last = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => panic!("不该失败：{e}"),
            }
        }
        panic!("一直 Text file busy：{last:?}");
    }

    /// 谱面名字 → 文件名主干：分隔符与保留字符必须换掉（拿 `meta.name` 当文件名的安全边界）
    #[test]
    fn chart_names_become_safe_file_stems() {
        assert_eq!(sanitize_stem("glacia"), "glacia");
        assert_eq!(sanitize_stem("  Belle de Nuit  "), "Belle de Nuit", "首尾空白去掉");
        assert_eq!(sanitize_stem("a/b"), "a-b", "斜杠不能让名字变成另一个目录");
        // 路径穿越：分隔符必须都没了，且结果不能是隐藏文件/上级目录
        // （`..\..\etc\passwd` → 分隔符变 `-`，再去掉首部的点 ⇒ 具体字符不那么重要，
        //  重要的是"它就是一个普通文件名"，所以这里断言**性质**而不是字面）
        let evil = sanitize_stem("..\\..\\etc\\passwd");
        assert!(!evil.contains('/') && !evil.contains('\\'), "{evil}");
        assert!(!evil.starts_with('.'), "{evil} 不该是隐藏文件");
        assert!(!evil.is_empty());
        assert_eq!(sanitize_stem("C:con*?"), "C-con--");
        // 全角标点**保留**：它不违反任何文件系统规则（Windows 只禁 ASCII 的那几个），
        // 中文曲名带 `：` 是常态，顺手换掉反而是在改用户的名字
        assert_eq!(sanitize_stem("名字：测试"), "名字：测试");
        assert_eq!(sanitize_stem("   "), "untitled");
        assert_eq!(sanitize_stem("..."), "untitled", "全是点会变成隐藏/上级路径");
        assert_eq!(sanitize_stem(""), "untitled");
        assert!(!sanitize_stem("a\nb").contains('\n'), "控制字符要去掉");
    }

    /// 派生路径：绝对/相对文件夹都要给出**绝对**目标
    #[test]
    fn derived_path_is_absolute_and_uses_the_extension() {
        let p = derive_path("/tmp/charts", "My Song", ".opm.json");
        assert_eq!(p, PathBuf::from("/tmp/charts/My Song.opm.json"));
        let p = derive_path("", "x", ".json");
        assert!(p.is_absolute(), "空文件夹要落到工作目录，而不是相对路径");
        assert!(p.ends_with("x.json"));
        let p = derive_path("sub/dir", "y", ".json");
        assert!(p.is_absolute() && p.ends_with("sub/dir/y.json"));
    }

    /// 起始目录必须**真的存在**（这是"新文件指定不了路径"的修复）
    #[test]
    fn dialog_starts_in_an_existing_directory() {
        let tmp = std::env::temp_dir();
        assert_eq!(nearest_existing_dir(&tmp), tmp);
        // 不存在的深层目录 → 退到最近的已存在祖先（这里是 /tmp 或它的某个已存在子目录）
        let deep = tmp.join("opm-not-here-1234/deeper");
        let got = nearest_existing_dir(&deep);
        assert!(got.is_dir(), "{}", got.display());
        assert!(tmp.starts_with(&got) || got.starts_with(&tmp), "{}", got.display());
        // 相对路径按工作目录补全后也必须存在
        assert!(nearest_existing_dir(Path::new(".")).is_dir());

        // 新文件还不存在，但起始位置的父目录必须存在
        let start = start_for_new_save(Some(&deep), "My Song", ".opm.json");
        assert!(start.parent().unwrap().is_dir(), "{}", start.display());
        assert!(start.ends_with("My Song.opm.json"));
        let start = start_for_new_save(None, "x", ".json");
        assert!(start.parent().unwrap().is_dir());
    }

    /// 系统框回来时可能没有扩展名 → 按目标格式补上
    #[test]
    fn missing_extension_is_added() {
        let p = Path::new("/tmp/charts/song");
        assert_eq!(ensure_extension(p, ".opm.json"), PathBuf::from("/tmp/charts/song.opm.json"));
        let p = Path::new("/tmp/charts/song.json");
        assert_eq!(ensure_extension(p, ".opm.json"), PathBuf::from("/tmp/charts/song.json"));
        let p = Path::new("/tmp/charts/song.opm.json");
        assert_eq!(ensure_extension(p, ".json"), PathBuf::from("/tmp/charts/song.opm.json"));
    }

    /// 打开 URL 前的校验：只收 http(s)，别的直接拒（不给"拿它打开本地文件"留口子）。
    ///
    /// **只测纯校验，不真的去开** —— 上一版这条测试对 `https://www.7-zip.org/…` 与
    /// `http://example.com/` 调了真身，于是每跑一次测试就弹出两个浏览器标签页。
    #[test]
    fn check_url_only_accepts_http() {
        for bad in [
            "file:///etc/passwd",
            "/etc/passwd",
            "javascript:alert(1)",
            "",
            "ftp://example.com/x",
            "HTTP://EXAMPLE.COM/",
        ] {
            assert!(check_url(bad).is_err(), "应被拒：{bad:?}");
        }
        assert!(check_url("https://www.7-zip.org/download.html").is_ok());
        assert!(check_url("http://example.com/").is_ok());
    }

    /// 探测结果要么是空（没有可用程序），要么是我们支持的两个之一
    #[test]
    fn detect_returns_only_supported_programs() {
        match detect() {
            None => {}
            Some(p) => assert!(p == "kdialog" || p == "zenity", "意外程序 {p}"),
        }
        // `availability()` 是缓存过的说明文字：两次调用必须是**同一个**字符串（同一份缓存），
        // 而且不能是空的 —— 它是会被 UI 每帧取用的那一行
        let a = availability();
        assert!(!a.is_empty());
        assert!(std::ptr::eq(a, availability()), "availability() 应命中同一份缓存");
        assert!(a.contains("kdialog") || a.contains("zenity") || a.contains("无（"), "{a}");
    }
}
