//! **起始界面**（最近打开的谱面 + 新建谱面表单 + 缺 7z 的门槛提示）。
//!
//! 这一屏只有**一个整屏布局**（列表）；"新建谱面"与"缺少 7z"都是盖在它上面的**模态**
//! （见 [`new_chart_modal`] / [`missing_7z_modal`]，外观统一走 [`crate::dialog`]）——
//! 整屏换内容会让用户觉得"进了另一个程序"，而弹窗只是"这一屏上的一个问题"。
//!
//! 最近打开列表存在 `$XDG_CONFIG_HOME/OpenPhM/recents.json`（退化到 `~/.config/...`，再退化到当前目录），
//! 与谱面文件本身分开：它是**这台机器的使用痕迹**，不是谱面数据 —— 所以不进 opm 格式、不进仓库。
//!
//! 三条规矩：
//! 1. **去重且最近在前**：同一路径只留一条（再打开就提到最前），最多 [`MAX_ENTRIES`] 条；
//! 2. **载入时清理**：文件已经不在的就丢掉（起始界面不该列一堆点不开的条目），并报告丢了几条；
//! 3. **坏了就当空的**：配置读不动/解析失败不报错阻塞启动 —— 这是便利功能，不是数据。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::dialog;

/// 最多记多少条（再多列表也读不完）
pub const MAX_ENTRIES: usize = 20;

/// 一条"最近打开"
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecentEntry {
    /// 绝对路径（相对路径在不同工作目录下会指向不同文件，存进来没意义）
    pub path: PathBuf,
    /// 谱面的曲名（`meta.name`），列表里显示它
    #[serde(default)]
    pub title: String,
    /// 载入时判定的格式（`opm` / `rpe`）
    #[serde(default)]
    pub format: String,
    /// 打开时刻（Unix 秒），用来排序与显示"多久以前"
    #[serde(default)]
    pub opened_at: u64,
}

impl RecentEntry {
    /// 文件还在吗（列表里给不在的条目画灰）
    pub fn exists(&self) -> bool {
        self.path.is_file()
    }
    /// 显示用的名字：优先曲名，其次文件名
    pub fn display_name(&self) -> String {
        if !self.title.trim().is_empty() {
            return self.title.clone();
        }
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("(未命名)")
            .to_owned()
    }
    /// "多久以前"（秒 → 人话）。`now` 由调用方给，方便测试。
    pub fn age_text(&self, now: u64) -> String {
        if self.opened_at == 0 || now < self.opened_at {
            return String::new();
        }
        let d = now - self.opened_at;
        match d {
            0..=59 => "刚刚".to_owned(),
            60..=3599 => format!("{} 分钟前", d / 60),
            3600..=86399 => format!("{} 小时前", d / 3600),
            _ => format!("{} 天前", d / 86400),
        }
    }
}

/// 最近打开列表
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Recents {
    #[serde(default = "version_1")]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<RecentEntry>,
}

fn version_1() -> u32 {
    1
}

/// 现在（Unix 秒）。
///
/// **转发到 `codec::container::now_secs`**：那一个已经存在，而本文件里也有地方直接用它
/// （崩溃恢复那一段）—— 两份"现在"不会算出两个时间，但会让"谁的时钟"这个问题有两个答案。
pub use crate::codec::container::now_secs;

/// 配置文件路径：`$XDG_CONFIG_HOME/OpenPhM/recents.json` →
/// `~/.config/OpenPhM/recents.json` → `./.opm-recents.json`
///
/// **缓存**：这行路径每次都要查两个环境变量并拼一次 `PathBuf`，而它会被启动页每帧取用
/// （"记录存放在 …"那一行）。会话里没人会中途改 `XDG_CONFIG_HOME`，缓存是安全的。
pub fn default_path() -> &'static Path {
    static CACHE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    CACHE.get_or_init(compute_default_path).as_path()
}

fn compute_default_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir).join("OpenPhM").join("recents.json");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home)
                .join(".config")
                .join("OpenPhM")
                .join("recents.json");
        }
    }
    PathBuf::from(".opm-recents.json")
}

/// 配置路径的**显示形式**（同样缓存）：启动页右栏每帧要画它，
/// 而 `display().to_string()` 每帧都要分配一次字符串。
pub fn default_path_display() -> &'static str {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| default_path().display().to_string())
        .as_str()
}

impl Recents {
    /// 从默认位置读（读不到就是空列表，**不报错**：便利功能不该拦住启动）
    pub fn load_default() -> Self {
        Self::load_from(&default_path())
    }

    /// 从指定位置读。坏文件/不存在 ⇒ 空列表。
    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str::<Recents>(&text).unwrap_or_default()
    }

    /// 写回默认位置（顺带建目录）
    pub fn save_default(&self) -> Result<(), String> {
        self.save_to(&default_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败 {dir:?}: {e}"))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| format!("序列化失败: {e}"))?;
        std::fs::write(path, format!("{text}\n")).map_err(|e| format!("写入失败 {path:?}: {e}"))
    }

    /// 记一条（去重、提到最前、截断）。返回是否新增（false = 只是更新了已有条目）。
    pub fn add(&mut self, path: &Path, title: &str, format: &str, now: u64) -> bool {
        // 相对路径先绝对化：否则换个工作目录打开，同一条目会被当成两个文件
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|d| d.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        let existed = self.entries.iter().any(|e| e.path == abs);
        self.entries.retain(|e| e.path != abs);
        self.entries.insert(
            0,
            RecentEntry {
                path: abs,
                title: title.to_owned(),
                format: format.to_owned(),
                opened_at: now,
            },
        );
        self.entries.truncate(MAX_ENTRIES);
        !existed
    }

    /// 丢掉文件已经不在的条目，返回丢掉几条
    pub fn prune(&mut self) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| e.exists());
        before - self.entries.len()
    }

    /// 从列表里移除一条（界面上"从列表移除"，不是删文件）
    pub fn forget(&mut self, path: &Path) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.path != path);
        self.entries.len() != before
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 列表里的一行 —— **已经解析好的字符串**。
///
/// 为什么要这一层：列表是每帧画的，而每行原先要在帧里做一次 `is_file()`（文件还在不在）
/// 与一次 `format!`（"[格式] 多久以前"）。这些只跟"列表内容 + 当前时刻"有关、与帧无关，
/// 所以抽成一次计算（[`list_rows`]），UI 只负责画 —— 帧里的工作只剩"把字符串交给 egui"。
#[derive(Clone, Debug, PartialEq)]
pub struct ListRow {
    /// 对应 `Recents::entries` 里的哪一条（动作要回传路径，所以这里也存一份）
    pub path: PathBuf,
    /// 曲名（没有曲名就用文件名）
    pub title: String,
    /// 副标题："[格式] 3 分钟前"；文件不在时是"文件不存在"（画成警告色）
    pub meta: String,
    /// 文件还在不在（不在的行画灰、点了只给一条提示）
    pub exists: bool,
    /// 悬停提示（完整路径）
    pub tooltip: String,
}

/// 生成列表快照（**纯函数**：输入是列表与"现在"，输出是每一行显示什么）。
///
/// `now` 由调用方给：快照的失效判据是"列表内容变了"或"时刻走远了"，
/// 不是"又过了一帧"。
pub fn list_rows(recents: &Recents, now: u64) -> Vec<ListRow> {
    recents
        .entries
        .iter()
        .map(|e| {
            let exists = e.exists();
            ListRow {
                path: e.path.clone(),
                title: e.display_name(),
                meta: if exists {
                    format!("[{}] {}", e.format, e.age_text(now))
                } else {
                    "文件不存在".to_owned()
                },
                exists,
                tooltip: e.path.display().to_string(),
            }
        })
        .collect()
}

/// 起始界面里能做出的选择（**由调用方施加**：库只管画与"用户点了什么"）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartAction {
    OpenRecent(PathBuf),
    OpenDialog,
    /// 切到"新建谱面"弹窗（**填表也在启动页上**，只是盖了一层模态，不换整屏）
    ShowNewForm,
    /// 关掉"新建"弹窗，回到列表
    BackToList,
    /// 表单填好了：用 [`NewChartForm`] 里的值建谱面（值由调用方持有，库只改它）
    Create,
    /// 用系统对话框挑音乐文件（结果写回表单的 `audio`）
    PickAudio,
    /// 用系统对话框挑曲绘/背景图（结果写回表单的 `illustration`）
    PickIllustration,
    /// 什么都不选，直接进编辑器
    Skip,
    /// 从列表里移除一条（不删文件）
    Forget(PathBuf),
    ClearAll,
    /// 界面上要显示一条提示（例如"文件不在了"）
    Notice(String),
}

/// 表单里"要挑文件"的那个字段（也是它在 `meta` 里的名字来源）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetField {
    /// `meta.audio` —— 音乐
    Audio,
    /// `meta.background` —— 曲绘/背景
    Illustration,
}

impl AssetField {
    /// 文档 `meta` 里的字段名（`new` 命令用它）
    pub fn meta_key(self) -> &'static str {
        match self {
            AssetField::Audio => "audio",
            AssetField::Illustration => "background",
        }
    }
}

/// 「挑一个资源文件」这件事的**全部**参数：挑完写回哪个字段、界面怎么称呼它、系统框列什么。
///
/// 为什么放进库里：用户报过"**新建谱面时音乐的系统框只列 json 文件**" —— 根因是那处
/// `filedialog::pick` 复用了谱面过滤器，而"谁该用哪个过滤器"当时写在 `main.rs` 里，**测不到**
/// （bin crate 的单测够不着那里的接线）。现在映射与过滤器都在这里，单测直接钉住。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssetPick {
    pub field: AssetField,
    /// 界面上的称呼（"音乐" / "曲绘"）
    pub what: &'static str,
    /// 输入框里的占位说明
    pub hint: &'static str,
    /// "浏览…"按钮的悬停说明
    pub hover: &'static str,
    pub filter: crate::filedialog::Filter,
}

/// 表单里两个要挑文件的字段（**顺序即界面顺序**；以后加资源类型只改这里）
pub const ASSET_PICKS: [AssetPick; 2] = [
    AssetPick {
        field: AssetField::Audio,
        what: "音乐",
        // 占位说明要**短到不被截断**（输入框宽度有限）：例子 + 字段名就够，细节在悬停里
        hint: "song.ogg → meta.audio",
        hover: "用系统文件对话框选音频（ogg/mp3/wav/flac/m4a/aac/opus）；保存 .opm 时一起装进容器",
        filter: crate::filedialog::AUDIO_FILTER,
    },
    AssetPick {
        field: AssetField::Illustration,
        what: "曲绘",
        hint: "bg.png → meta.background",
        hover: "用系统文件对话框选曲绘（png/jpg/jpeg/webp/bmp）；保存 .opm 时一起装进容器",
        filter: crate::filedialog::IMAGE_FILTER,
    },
];

impl AssetPick {
    /// 界面上的行标签（"音乐路径"）
    pub fn label(&self) -> String {
        format!("{}路径", self.what)
    }
    /// 这一行对应的动作（按钮按下时交给调用方）
    pub fn action(&self) -> StartAction {
        match self.field {
            AssetField::Audio => StartAction::PickAudio,
            AssetField::Illustration => StartAction::PickIllustration,
        }
    }
}

impl StartAction {
    /// 这个动作要挑资源吗？挑的话，参数全在这里（`None` = 不弹文件对话框）
    pub fn asset(&self) -> Option<AssetPick> {
        match self {
            StartAction::PickAudio => Some(ASSET_PICKS[0]),
            StartAction::PickIllustration => Some(ASSET_PICKS[1]),
            _ => None,
        }
    }
}

/// "新建谱面"表单的值。**放在库里**：它的校验规则（曲名必填、BPM 必须为正）要有单测，
/// 而且"新建"这件事发生在启动页，表单状态自然跟着那一屏走。
#[derive(Clone, Debug, PartialEq)]
pub struct NewChartForm {
    pub name: String,
    pub charter: String,
    pub composer: String,
    pub audio: String,
    /// 曲绘/背景图路径 —— 写进 `meta.background`，保存成 `.opm` 时和音乐一起装进容器
    pub illustration: String,
    pub bpm: f32,
}

impl Default for NewChartForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            charter: String::new(),
            composer: String::new(),
            audio: String::new(),
            illustration: String::new(),
            // 174 是 Phigros 常见档位，作为默认值比 120 更贴近实际；填错也就改一个数
            bpm: 174.0,
        }
    }
}

impl NewChartForm {
    /// 校验：**曲名必填**（没名字的谱面在最近列表里只能显示文件名），BPM 必须是正数
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("曲名不能为空".to_owned());
        }
        if !self.bpm.is_finite() || self.bpm <= 0.0 {
            return Err(format!("基础 BPM 必须是正数（现在是 {}）", self.bpm));
        }
        Ok(())
    }
    pub fn name_or_untitled(&self) -> String {
        let n = self.name.trim();
        if n.is_empty() {
            "untitled".to_owned()
        } else {
            n.to_owned()
        }
    }
    /// 提交给 `{"op":"new"}` 的参数（BPM 已在 `validate` 里保证为正）
    pub fn to_new_command(&self) -> serde_json::Value {
        let mut meta = serde_json::Map::new();
        meta.insert("name".to_owned(), serde_json::json!(self.name_or_untitled()));
        meta.insert("charter".to_owned(), serde_json::json!(self.charter.trim()));
        meta.insert("composer".to_owned(), serde_json::json!(self.composer.trim()));
        // 资源字段名与写回字段**同源**（`AssetField::meta_key`），不再各处写字符串
        meta.insert("audio".to_owned(), serde_json::json!(self.audio.trim()));
        meta.insert(
            AssetField::Illustration.meta_key().to_owned(),
            serde_json::json!(self.illustration.trim()),
        );
        serde_json::json!({ "op": "new", "meta": meta, "bpm": self.bpm as f64 })
    }

    /// 表单里某个资源字段的值（给界面用：标签/占位/按钮都从 [`ASSET_PICKS`] 来）
    pub fn asset(&self, field: AssetField) -> &str {
        match field {
            AssetField::Audio => &self.audio,
            AssetField::Illustration => &self.illustration,
        }
    }
    /// 同上，可变版（输入框直接改它）
    pub fn asset_mut(&mut self, field: AssetField) -> &mut String {
        match field {
            AssetField::Audio => &mut self.audio,
            AssetField::Illustration => &mut self.illustration,
        }
    }
    /// 系统对话框挑完之后写回（`asset_mut` 的语义化包装）
    pub fn set_asset(&mut self, field: AssetField, path: &str) {
        *self.asset_mut(field) = path.to_owned();
    }
}

/// 画**起始界面**：谱面列表（左＝最近打开，右＝打开/新建）。返回用户做出的选择（`None` = 还没选）。
///
/// 三条纪律，前两条来自真实 bug：
/// 1. **尺寸只由 `screen` 决定**（见 [`start_screen_columns`]）——绝不能用 `ui.available_width()`：
///    内容宽度按栏宽排布、内容又决定尺寸，于是每帧在上一帧结果上再放大，
///    表现就是"鼠标一滑，谱面列表的横向一直变长"；
/// 2. `modal_open = true` 时**不看 Esc**：这一屏上面的模态（新建谱面 / 缺 7z）已经把 Esc
///    消费掉了，这里再判一次就会"关弹窗的同时跳进编辑器"；
/// 3. 行内容**从外面传进来**（[`ListRow`]）：这一层不许 `stat` 文件、不许算"多久以前" ——
///    那两件事与帧无关，算一次就够（否则鼠标一动就是每帧 20 次 `is_file`）。
///
/// 它只画列表那一层；模态由 [`new_chart_modal`] / [`missing_7z_modal`] 各自画在上面。
pub fn start_screen_ui(
    ui: &mut egui::Ui,
    screen: egui::Rect,
    rows: &[ListRow],
    message: Option<&(bool, String)>,
    native_dialog: &str,
    modal_open: bool,
) -> Option<StartAction> {
    let mut action: Option<StartAction> = None;
    let (left_w, right_w, body_h) = start_screen_columns(screen, 8.0);
    let gap = 14.0;
    {
    ui.add_space(10.0);
    ui.vertical_centered(|ui| {
        ui.heading("OpenPhM");
        dialog::hint(ui, "选择一份谱面继续，或新建一份");
    });
    ui.add_space(14.0);
    ui.horizontal_top(|ui| {
        // ---- 左：最近打开的谱面 ----
        ui.allocate_ui_with_layout(
            egui::vec2(left_w, body_h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(left_w); // 固定宽度：别让内容反过来撑它
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_min_size(egui::vec2(left_w - 16.0, body_h - 16.0));
                    ui.set_max_width(left_w - 16.0);
                    ui.horizontal(|ui| {
                        ui.strong(format!("最近打开的谱面（{}）", rows.len()));
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if !rows.is_empty() && ui.small_button("清空列表").clicked() {
                                    action = Some(StartAction::ClearAll);
                                }
                            },
                        );
                    });
                    ui.separator();
                    if rows.is_empty() {
                        ui.add_space(8.0);
                        dialog::hint(ui, "（还没有记录 —— 打开或新建一份谱面就会出现在这里）");
                    }
                    let row_w = left_w - 40.0;
                    let mut forget: Option<PathBuf> = None;
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for row in rows {
                                ui.horizontal(|ui| {
                                    let mut text = egui::RichText::new(&row.title).strong();
                                    if !row.exists {
                                        // 文件不在了：整行压灰（点它只会给一条提示，不会打开）
                                        text = text.color(egui::Color32::from_gray(150));
                                    }
                                    let name_w = (row_w - 120.0).max(120.0);
                                    let resp = ui.add(
                                        egui::Button::new(text).min_size(egui::vec2(name_w, 22.0)),
                                    );
                                    if resp.clicked() {
                                        if row.exists {
                                            action =
                                                Some(StartAction::OpenRecent(row.path.clone()));
                                        } else {
                                            action = Some(StartAction::Notice(format!(
                                                "文件不在了：{}",
                                                row.path.display()
                                            )));
                                        }
                                    }
                                    // 副标题：格式 + 多久以前；文件不在了就换成警告色的一句
                                    ui.label(
                                        egui::RichText::new(&row.meta).small().color(if row.exists {
                                            dialog::HINT
                                        } else {
                                            dialog::WARN
                                        }),
                                    );
                                    if ui
                                        .small_button("✕")
                                        .on_hover_text("从列表移除（不删文件）")
                                        .clicked()
                                    {
                                        forget = Some(row.path.clone());
                                    }
                                })
                                .response
                                .on_hover_text(&row.tooltip);
                            }
                        });
                    if let Some(p) = forget {
                        action = Some(StartAction::Forget(p));
                    }
                });
            },
        );
        ui.add_space(gap);
        // ---- 右：打开 / 新建 ----
        ui.allocate_ui_with_layout(
            egui::vec2(right_w, body_h),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_width(right_w);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_min_size(egui::vec2(right_w - 16.0, body_h - 16.0));
                    ui.set_max_width(right_w - 16.0);
                    ui.strong("开始");
                    ui.separator();
                    ui.add_space(6.0);
                    let bw = right_w - 32.0;
                    if ui
                        .add(egui::Button::new("📂 打开谱面…").min_size(egui::vec2(bw, 30.0)))
                        .on_hover_text("系统文件对话框（opm 容器 / 裸 opm / RPE 都行）")
                        .clicked()
                    {
                        action = Some(StartAction::OpenDialog);
                    }
                    ui.add_space(6.0);
                    if ui
                        .add(egui::Button::new("🆕 新建谱面…").min_size(egui::vec2(bw, 30.0)))
                        .on_hover_text("曲名 / 谱面作者 / 音乐作者 / 音乐路径 / 基础 BPM")
                        .clicked()
                    {
                        action = Some(StartAction::ShowNewForm);
                    }
                    ui.add_space(12.0);
                    ui.separator();
                    dialog::hint(ui, "记录存放在");
                    ui.label(
                        egui::RichText::new(default_path_display())
                            .small()
                            .monospace()
                            .color(dialog::HINT),
                    );
                    dialog::hint(ui, format!("系统文件对话框：{native_dialog}"));
                    ui.add_space(8.0);
                    if ui
                        .button("跳过，直接进编辑器")
                        .on_hover_text("用一个空谱面开始（Esc 同效）")
                        .clicked()
                    {
                        action = Some(StartAction::Skip);
                    }
                    if let Some((ok, msg)) = message {
                        ui.add_space(6.0);
                        dialog::message(ui, *ok, msg);
                    }
                });
            },
        );
    });
    }
    // Esc = 跳过（用空谱面直接进编辑器）。**模态开着时不算** —— 那个 Esc 已经归弹窗了。
    if !modal_open && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        return Some(StartAction::Skip);
    }
    action
}

/// 「上次没有正常退出」的模态：缓存里还躺着一份 GUI 留下的谱面 ⇒ 问用户要不要接着编辑。
///
/// **判据在库里**（`session::gui_leftovers`）：解压缓存正常退出时会被删掉，留下的那份只能是
/// 被强杀/崩溃留下的；出处写在目录里的 `session.json`，于是 `opm-ctl` 的缓存不会来打扰用户。
/// 这里只负责画与收集选择 —— 三个出口都要有：继续（默认）、丢弃、稍后再说（**什么都不删**）。
///
/// 返回 `None` = **用户还没决定**（弹窗继续开着）。这一点必须是显式的：
/// 早先"没点按钮"被当成"稍后再说"，于是弹窗只活了一帧就自己消失了（截图里什么都看不到）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeChoice {
    /// 接着编辑这份谱面（连缓存里的未保存改动一起）
    Continue,
    /// 丢弃这份缓存（磁盘上的谱面文件不动）
    Discard,
    /// 先不动它：这次不问第二次，下次启动再问
    Later,
}

pub fn resume_cache_modal(
    ctx: &egui::Context,
    item: &crate::session::Leftover,
    others: usize,
) -> Option<ResumeChoice> {
    let mut choice: Option<ResumeChoice> = None;
    let out = dialog::modal(ctx, "opm_resume_cache", dialog::W_FORM, |ui| {
        dialog::title(ui, "上次没有正常退出");
        ui.label(format!(
            "上次运行时（{}）有一份谱面还摊在缓存里：{}。",
            crate::session::age_text(item.age_secs),
            item.headline()
        ));
        ui.add_space(6.0);
        dialog::hint(
            ui,
            "程序正常退出时会清理自己摊出来的缓存，这里还留着一份 ⇒ 上一个进程多半被强杀或崩溃了。",
        );
        ui.add_space(8.0);
        for line in item.details() {
            ui.label(line);
        }
        if others > 0 {
            ui.add_space(4.0);
            dialog::hint(ui, format!("另有 {others} 份更旧的遗留缓存，本次不动它们。"));
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui
                .button("▶ 继续此谱面")
                .on_hover_text("按缓存里的内容接着编辑（保存目标指回原来的文件）")
                .clicked()
            {
                choice = Some(ResumeChoice::Continue);
            }
            if ui
                .button("🗑 丢弃缓存")
                .on_hover_text("删掉这份解压缓存；磁盘上的谱面文件不动")
                .clicked()
            {
                choice = Some(ResumeChoice::Discard);
            }
            if ui
                .button("稍后再说")
                .on_hover_text("什么都不做：留着它，下次启动再问")
                .clicked()
            {
                choice = Some(ResumeChoice::Later);
            }
        });
    });
    // Esc / 点遮罩 = 稍后再说：**最保守的那一个** —— 不删东西，也不擅自换掉用户手里的文档
    if out.dismissed {
        return Some(ResumeChoice::Later);
    }
    choice
}

/// **单会话**门槛：已经有一个 OpenPhM 在跑（独占锁被占着）。
///
/// 为什么必须拦：解压缓存是**进程独占**的（退出即清、切换即删），两个会话同时跑会互相删对方
/// 正在用的那份。这里给一个关不掉的模态（Esc 与遮罩都吃不掉它），出口只有"关闭"。
///
/// 为什么不做成"直接把已有窗口提到前台"：那需要平台相关的窗口管理（X11/Wayland 各一套），
/// 而这句话本身已经足够让人明白该去看哪个窗口 —— 附上 pid 与启动时间，找不到时能自己查。
pub fn session_busy_modal(ctx: &egui::Context, who: Option<&crate::codec::container::Session>) -> bool {
    let mut quit = false;
    let _ = dialog::sticky_modal(ctx, "opm_session_busy", dialog::W_FORM, |ui| {
        dialog::title(ui, "已经有一个 OpenPhM 在运行");
        ui.label(
            "同一时刻只允许一个 OpenPhM 会话：解压缓存是进程独占的，两个会话会互相删掉对方正在用的那份。",
        );
        ui.add_space(6.0);
        match who {
            Some(w) => {
                let age = crate::session::age_text(
                    crate::codec::container::now_secs().saturating_sub(w.started),
                );
                dialog::warn(ui, &format!("正在运行的那个：进程 {}（{} 启动）", w.pid, age));
            }
            // 锁文件读不出来（比如刚被清掉）：**照样拦**，只是说不出是谁 —— 判定靠锁，不靠这个文件
            None => dialog::warn(ui, "正在运行的那个：读不到锁文件里的身份信息"),
        }
        ui.add_space(8.0);
        dialog::hint(ui, "这一份没有碰任何缓存与谱面文件，直接关掉它是安全的。");
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui
                .button("关闭")
                .on_hover_text("关掉这一份，回到已经在运行的那个窗口")
                .clicked()
            {
                quit = true;
            }
        });
    });
    quit
}

/// 缺少 7z 的**黏性模态**：盖在起始界面上，是个门槛，不是提示条。
///
/// 为什么是硬门槛而不是"打一行日志继续跑"：`.opm` 容器（音乐/曲绘装在里面的那个形态）
/// 就是靠 7z 打包/解包的，没有它等于交付不出正式格式的文件。
/// 出口只有明写的按钮 —— Esc 与点遮罩都关不掉（`sticky_modal`），但它们仍然**被消费掉**，
/// 于是底下的列表不会收到一个本该属于门槛的 Esc（否则"跳过直接进编辑器"会当场把门槛漏掉）。
///
/// `can_fetch` = Windows 上多一个"获取 7z"按钮（非 Windows 把装法写在正文里，别让用户去搜）。
pub struct Missing7zOutcome {
    pub quit: bool,
    pub fetch: bool,
}

pub fn missing_7z_modal(ctx: &egui::Context, message: &str, can_fetch: bool) -> Missing7zOutcome {
    let mut out = Missing7zOutcome { quit: false, fetch: false };
    let _ = dialog::sticky_modal(ctx, "opm_missing_7z", dialog::W_FORM, |ui| {
        dialog::title(ui, "缺少 7-Zip（7z）");
        ui.label(
            "opm 的正式形态 `.opm` 是一个 ZIP 容器（谱面 + 音乐 + 曲绘装在一个文件里），\
             打包与解包由 7-Zip 完成。当前系统里找不到可调用的 7z。",
        );
        ui.add_space(6.0);
        dialog::warn(ui, message);
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui
                .button("退出")
                .on_hover_text("关掉 OpenPhM，装好 7z 再启动")
                .clicked()
            {
                out.quit = true;
            }
            if can_fetch
                && ui
                    .button("获取 7z…")
                    .on_hover_text("用浏览器打开 7-zip.org 下载页")
                    .clicked()
            {
                out.fetch = true;
            }
        });
        // 非 Windows 不给按钮，但把装法写清楚（"文本 + 退出按钮"就够了）
        if !can_fetch {
            ui.add_space(8.0);
            dialog::hint(ui, "装好 7z 之后重启 OpenPhM。");
        }
        ui.add_space(4.0);
        dialog::hint(
            ui,
            "提示：CLI（opm-ctl）不受此限制 —— 没装 7z 时它会用内置实现打包（不压缩，体积更大）。",
        );
    });
    out
}

/// 「新建谱面」的**模态**：盖在谱面列表上，不换整屏（和编辑页的弹窗同一个模块画出来的）。
///
/// 返回用户做出的选择。Esc / 点遮罩 = 返回列表 —— **不是**"跳过直接进编辑器"：
/// 用户在填表，别把他踢进编辑器（这条有单测）。
pub fn new_chart_modal(
    ctx: &egui::Context,
    form: &mut NewChartForm,
    message: Option<&(bool, String)>,
) -> Option<StartAction> {
    let mut action: Option<StartAction> = None;
    let out = dialog::modal(ctx, "opm_new_chart", dialog::W_FORM, |ui| {
        dialog::title(ui, "新建谱面");
        dialog::hint(
            ui,
            "这些会写进谱面文件（曲名 / 谱面作者 / 音乐作者 / 音乐与曲绘路径 / 基础 BPM）；\
             音乐与曲绘会一起装进 `.opm` 容器",
        );
        ui.add_space(8.0);
        // 输入框宽度是**弹窗宽度的函数**（纯量），不看屏幕、也不看上一层还剩多少地方
        let w = dialog::W_FORM - 230.0;
        egui::Grid::new("opm_new_chart_grid")
            .num_columns(2)
            .spacing([14.0, 10.0])
            .show(ui, |ui| {
                ui.label("曲名");
                ui.add(
                    egui::TextEdit::singleline(&mut form.name)
                        .desired_width(w)
                        .hint_text("例如：Belle de Nuit"),
                );
                ui.end_row();
                ui.label("谱面作者");
                ui.add(
                    egui::TextEdit::singleline(&mut form.charter)
                        .desired_width(w)
                        .hint_text("charter"),
                );
                ui.end_row();
                ui.label("音乐作者");
                ui.add(
                    egui::TextEdit::singleline(&mut form.composer)
                        .desired_width(w)
                        .hint_text("composer"),
                );
                ui.end_row();
                // 资源行（音乐 / 曲绘）**由 `ASSET_PICKS` 驱动**：标签、占位、悬停、按钮动作、
                // 以及按下后弹哪种过滤器，全都来自那一处定义 —— 界面与"挑文件要带什么过滤器"不会各说各话
                for spec in ASSET_PICKS {
                    ui.label(spec.label());
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(form.asset_mut(spec.field))
                                .desired_width(w - 90.0)
                                .hint_text(spec.hint),
                        );
                        if ui.button("浏览…").on_hover_text(spec.hover).clicked() {
                            action = Some(spec.action());
                        }
                    });
                    ui.end_row();
                }
                ui.label("基础 BPM");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::DragValue::new(&mut form.bpm)
                            .range(1.0..=1000.0)
                            .speed(0.5)
                            .fixed_decimals(2),
                    );
                    dialog::hint(ui, "写入 bpmList 的首条（从拍 0 起）");
                });
                ui.end_row();
            });
        ui.add_space(6.0);
        match form.validate() {
            Ok(()) => dialog::hint(
                ui,
                "新建的谱面还没有保存目标：第一次保存会弹保存窗口让你指定路径。",
            ),
            // 校验没过就把原因留在**弹窗里**（不是关掉弹窗再在外面报错）
            Err(e) => dialog::warn(ui, &e),
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui
                .button("✔ 创建")
                .on_hover_text("建好之后进编辑页")
                .clicked()
            {
                action = Some(StartAction::Create);
            }
            if ui
                .button("↩ 返回列表")
                .on_hover_text("Esc 同效")
                .clicked()
            {
                action = Some(StartAction::BackToList);
            }
            if let Some((ok, msg)) = message {
                dialog::message(ui, *ok, msg);
            }
        });
    });
    // Esc / 点遮罩 = 返回列表（`dialog::modal` 已经把 Esc 消费掉了，这里只管取结果）
    if out.dismissed {
        return Some(StartAction::BackToList);
    }
    action
}

/// 起始界面两栏的尺寸：**只由屏幕矩形决定**（纯函数）。
///
/// 这一条是从一个真 bug 里长出来的：栏宽原先用 `ui.available_width()` 算，而模态框的 `Area`
/// 会记住上一帧的尺寸、内容又按栏宽排布 ⇒ 每帧在上一帧结果上再放大，鼠标一动就持续重绘，
/// 表现就是"谱面列表横向一直变长"。纯函数 + 只吃屏幕尺寸 ⇒ **同样的窗口大小必然同样的结果**，
/// 反馈回路从结构上就不成立（测试 `columns_are_stable_across_calls` 钉住这条）。
pub fn start_screen_columns(screen: egui::Rect, chrome: f32) -> (f32, f32, f32) {
    // 模态框的内外边距：窗口边距 18×2 + 两栏之间的 gap
    const GAP: f32 = 14.0; // 两栏之间的间隙（与 `start_screen_ui` 里保持一致）
    let usable = (screen.width() - 80.0 - 36.0 - GAP - chrome).max(520.0);
    let left_w = (usable * 0.62).max(280.0);
    let right_w = (usable - left_w).max(220.0);
    let body_h = (screen.height() - 96.0 - 40.0).max(200.0);
    (left_w, right_w, body_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **每个测试一个自己的目录**：同一进程里的测试是并行跑的，共用目录会互相删
    /// （这正是一次瞬时失败的真凶：一个测试 `remove_dir_all` 掉了另一个正在写的目录）。
    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("opm-recents-test-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 去重 + 最近在前 + 截断
    #[test]
    fn add_dedupes_and_moves_to_front() {
        let mut r = Recents::default();
        assert!(r.add(Path::new("/tmp/a.json"), "A", "opm", 100));
        assert!(r.add(Path::new("/tmp/b.json"), "B", "rpe", 200));
        assert!(!r.add(Path::new("/tmp/a.json"), "A2", "opm", 300), "已有条目不算新增");
        assert_eq!(r.entries.len(), 2, "同一条路径只留一条");
        assert_eq!(r.entries[0].path, PathBuf::from("/tmp/a.json"), "最近打开的在前");
        assert_eq!(r.entries[0].title, "A2", "重复打开要刷新曲名与时刻");
        assert_eq!(r.entries[0].opened_at, 300);
        // 截断
        for i in 0..(MAX_ENTRIES + 5) {
            r.add(Path::new(&format!("/tmp/c{i}.json")), "C", "opm", 400 + i as u64);
        }
        assert_eq!(r.entries.len(), MAX_ENTRIES);
    }

    /// 相对路径绝对化：否则换工作目录后同一条目会变成两条
    #[test]
    fn relative_paths_are_absolutized() {
        let mut r = Recents::default();
        r.add(Path::new("chart.json"), "x", "opm", 1);
        assert!(r.entries[0].path.is_absolute(), "{:?}", r.entries[0].path);
        let abs = r.entries[0].path.clone();
        r.add(&abs, "x", "opm", 2);
        assert_eq!(r.entries.len(), 1, "绝对/相对指的是同一个文件，应去重");
    }

    /// 存取往返 + 坏文件不阻塞
    #[test]
    fn roundtrip_and_broken_file() {
        let dir = tmp_dir("roundtrip");
        let p = dir.join("recents.json");
        let mut r = Recents::default();
        r.add(Path::new("/tmp/a.json"), "A", "rpe", 42);
        r.save_to(&p).unwrap();
        let back = Recents::load_from(&p);
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].title, "A");
        assert_eq!(back.entries[0].format, "rpe");

        // 坏 JSON / 不存在的文件 ⇒ 空列表（便利功能不该拦住启动）
        std::fs::write(&p, "{ 这不是 json").unwrap();
        assert!(Recents::load_from(&p).is_empty());
        assert!(Recents::load_from(&dir.join("不存在.json")).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 清理：文件没了的条目要丢掉；`forget` 只从列表移除、不删文件
    #[test]
    fn prune_drops_missing_files_and_forget_does_not_delete() {
        let dir = tmp_dir("prune");
        let real = dir.join("real.opm.json");
        std::fs::write(&real, "{}").unwrap();
        let mut r = Recents::default();
        r.add(&real, "真文件", "opm", 1);
        r.add(Path::new("/tmp/根本不存在-x.json"), "幽灵", "opm", 2);
        assert_eq!(r.prune(), 1);
        assert_eq!(r.entries.len(), 1);
        assert!(real.exists(), "prune 只从列表里丢，不碰文件");
        assert!(r.forget(&real));
        assert!(r.entries.is_empty());
        assert!(real.exists(), "forget 也不删文件");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 显示名与"多久以前"
    #[test]
    fn display_name_and_age() {
        let e = RecentEntry {
            path: PathBuf::from("/tmp/x/song.opm.json"),
            title: "Belle de Nuit".to_owned(),
            format: "rpe".to_owned(),
            opened_at: 1000,
        };
        assert_eq!(e.display_name(), "Belle de Nuit");
        let e2 = RecentEntry { title: String::new(), ..e.clone() };
        assert_eq!(e2.display_name(), "song.opm.json", "没曲名就退回文件名");
        assert_eq!(e.age_text(1000), "刚刚");
        assert_eq!(e.age_text(1000 + 120), "2 分钟前");
        assert_eq!(e.age_text(1000 + 7200), "2 小时前");
        assert_eq!(e.age_text(1000 + 86400 * 3), "3 天前");
        assert_eq!(e.age_text(0), "", "时刻不可信时不显示");
    }

    /// 列表快照：**一次性算好每行显示什么**（帧里不再 stat 文件、不再拼格式）
    #[test]
    fn list_rows_snapshot_resolves_names_meta_and_missing_files() {
        let dir = tmp_dir("list-rows");
        let real = dir.join("real.opm.json");
        std::fs::write(&real, "{}").unwrap();
        let mut r = Recents::default();
        r.add(&real, "真文件", "opm", 1000);
        // 没曲名的条目要退回文件名；不存在的条目要标出来
        r.entries.push(RecentEntry {
            path: dir.join("幽灵.rpe.json"),
            title: String::new(),
            format: "rpe".to_owned(),
            opened_at: 1000,
        });
        let rows = list_rows(&r, 1000 + 120);
        assert_eq!(rows.len(), 2, "一条记录一行");
        assert_eq!(rows[0].title, "真文件");
        assert!(rows[0].exists);
        assert_eq!(rows[0].meta, "[opm] 2 分钟前");
        assert!(rows[0].tooltip.ends_with("real.opm.json"), "{}", rows[0].tooltip);
        assert_eq!(rows[1].title, "幽灵.rpe.json", "没曲名就退回文件名");
        assert!(!rows[1].exists);
        assert_eq!(rows[1].meta, "文件不存在");
        // 时刻不同 ⇒ 只有"多久以前"变化（这正是不必每帧重算的原因：它只跟 now 有关）
        let later = list_rows(&r, 1000 + 86400);
        assert_eq!(later[0].meta, "[opm] 1 天前");
        assert_eq!(later[0].title, rows[0].title);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **尺存只跟屏幕有关**：同样的屏幕尺寸必然同样的结果 —— 反馈回路（越画越长）从结构上不成立
    #[test]
    fn columns_are_stable_across_calls() {
        let r = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1600.0, 900.0));
        let a = start_screen_columns(r, 8.0);
        for _ in 0..50 {
            let b = start_screen_columns(r, 8.0);
            assert_eq!(a, b, "同样的屏幕必须给出同样的栏宽（否则就是自我放大的回路）");
        }
        // 两栏 + 间隙不能超出可用宽度（不然每帧把内容撑大一点）
        let (l, rr, h) = a;
        assert!(l + rr + 14.0 <= 1600.0 - 80.0, "{a:?}");
        assert!(h > 0.0 && h < 900.0, "{a:?}");
        // 大屏小屏都要给出可用值
        for w in [640.0f32, 1280.0, 2560.0] {
            let (l, rr, _) =
                start_screen_columns(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, 720.0)), 8.0);
            assert!(l >= 280.0 && rr >= 220.0, "w={w} ⇒ {l}/{rr}");
        }
    }

    /// **起始界面必须帧间稳定**（真实代码的无头回归测试）。
    ///
    /// 用户报的 bug：鼠标在开始界面滑动时，谱面列表的横向长度**持续变长**。这是布局反馈回路
    /// （内容宽度由 `available_width()` 算出，而内容又决定 Area 的尺寸，每帧在上一帧结果上再放大）。
    /// 复现它最靠得住的办法不是推理，而是**把真实那段 UI 连跑若干帧、量它每帧画出来的宽度**：
    /// 这里用 `FullOutput.shapes` 里最大的那个填充矩形（就是模态框的底板）当量尺。
    #[test]
    fn start_screen_drawn_width_is_stable_across_frames() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1600.0, 900.0));
        let ctx = egui::Context::default();
        // 有记录 + 名字故意很长（真实曲名会比 "Belle de Nuit" 长），逼出任何"被内容撑大"的路径
        let mut r = Recents::default();
        for i in 0..3 {
            r.entries.push(RecentEntry {
                path: PathBuf::from(format!("/tmp/不存在的谱面-{i}.opm")),
                title: format!("一个相当长的曲名 for growth probing #{i} —— 中文与 English 混排"),
                format: "opm 容器".to_owned(),
                opened_at: 1000,
            });
        }
        // 行快照**在循环外算一次**：这正是真实调用方的用法（帧里只画字符串）
        let rows = list_rows(&r, 1000);
        let mut widths: Vec<f32> = Vec::new();
        let mut heights: Vec<f32> = Vec::new();
        for _ in 0..12 {
            let raw = egui::RawInput { screen_rect: Some(rect), ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| {
                let screen = ui.max_rect();
                let _ = start_screen_ui(ui, screen, &rows, None, "kdialog（测试）", false);
            });
            // 量尺：本帧画出来的最大的**填充矩形**（两栏的底板）
            let mut best = (0.0f32, 0.0f32);
            for cs in &out.shapes {
                if let egui::epaint::Shape::Rect(rs) = &cs.shape {
                    let (w, h) = (rs.rect.width(), rs.rect.height());
                    if w * h > best.0 * best.1 {
                        best = (w, h);
                    }
                }
            }
            widths.push(best.0);
            heights.push(best.1);
            out.textures_delta.clear();
        }
        // 它是**窗口内容**（两栏布局，不是浮层），第一帧就有尺寸 ⇒ 从第 0 帧起就必须逐帧相同
        let (w0, h0) = (widths[0], heights[0]);
        assert!(w0 > 100.0 && h0 > 100.0, "要有实际尺寸：{widths:?} {heights:?}");
        for (i, w) in widths.iter().enumerate() {
            assert!(
                (w - w0).abs() < 0.5,
                "第 {i} 帧宽度 {w} 与首帧 {w0} 不同 ⇒ 又出现自我放大的回路：{widths:?}"
            );
        }
        for h in &heights {
            assert!((h - h0).abs() < 0.5, "高度也必须稳定：{heights:?}");
        }
        // 而且不能超出屏幕（超出去就说明被内容撑破了）
        assert!(widths.iter().fold(0.0f32, |a, b| a.max(*b)) <= rect.width(), "{widths:?}");
        assert!(heights.iter().fold(0.0f32, |a, b| a.max(*b)) <= rect.height(), "{heights:?}");
    }

    /// 列表那一层画出来的东西对**用户操作**有响应（Esc = 跳过进编辑器），
    /// 但**模态开着时不许有反应** —— 那个 Esc 归弹窗（这里是缺 7z 的门槛场景）。
    #[test]
    fn start_screen_returns_actions_for_clicks() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1600.0, 900.0));
        let ctx = egui::Context::default();
        let r = Recents::default();
        let rows = list_rows(&r, 1000);
        let mut actions: Vec<StartAction> = Vec::new();
        let pass = |events: Vec<egui::Event>, modal_open: bool, actions: &mut Vec<StartAction>| {
            let raw = egui::RawInput { screen_rect: Some(rect), events, ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| {
                let screen = ui.max_rect();
                if let Some(a) =
                    start_screen_ui(ui, screen, &rows, None, "kdialog（测试）", modal_open)
                {
                    actions.push(a);
                }
            });
            out.textures_delta.clear();
        };
        let esc = || {
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }]
        };
        // Esc 一定给"跳过"（不必去猜按钮坐标）
        pass(esc(), false, &mut actions);
        assert!(
            actions.contains(&StartAction::Skip),
            "Esc 应给出跳过动作，实际 {actions:?}"
        );
        // 模态开着：同一个 Esc 一份也不该落到列表上
        actions.clear();
        pass(esc(), true, &mut actions);
        assert!(
            actions.is_empty(),
            "模态开着时列表不该响应 Esc（否则门槛会被跳过），实际 {actions:?}"
        );
    }

    /// 表单校验与提交参数（**新建谱面的一切都从这里出去**，所以规则要有单测）
    #[test]
    fn new_chart_form_validates_and_builds_the_command() {
        let mut f = NewChartForm::default();
        assert_eq!(f.bpm, 174.0, "默认 BPM 是个常见档位");
        // 曲名必填
        assert!(f.validate().is_err());
        f.name = "   ".to_owned();
        assert!(f.validate().is_err(), "全是空格也算空");
        f.name = "Belle de Nuit".to_owned();
        assert!(f.validate().is_ok());
        // BPM 必须为正、且要有限
        for bad in [0.0f32, -1.0, f32::NAN, f32::INFINITY] {
            f.bpm = bad;
            assert!(f.validate().is_err(), "bad bpm {bad} 应被拒");
        }
        f.bpm = 180.5;
        assert!(f.validate().is_ok());
        // 提交参数：曲名去空白、作者去空白、空曲名兜底 untitled
        f.charter = "  me  ".to_owned();
        f.composer = " someone ".to_owned();
        f.audio = " song.ogg ".to_owned();
        f.illustration = " bg.png ".to_owned();
        let cmd = f.to_new_command();
        assert_eq!(cmd["op"], "new");
        assert_eq!(cmd["meta"]["name"], "Belle de Nuit");
        assert_eq!(cmd["meta"]["charter"], "me");
        assert_eq!(cmd["meta"]["composer"], "someone");
        assert_eq!(cmd["meta"]["audio"], "song.ogg");
        // 曲绘走 `meta.background`（文档模型里的字段名），而且要去空白 —— 空着就是"没有曲绘"
        assert_eq!(cmd["meta"]["background"], "bg.png");
        assert_eq!(
            NewChartForm { illustration: "  ".to_owned(), ..f.clone() }.to_new_command()["meta"]["background"],
            ""
        );
        assert_eq!(cmd["bpm"], 180.5);
        let empty = NewChartForm { name: String::new(), ..f.clone() };
        assert_eq!(empty.name_or_untitled(), "untitled");
        assert_eq!(empty.to_new_command()["meta"]["name"], "untitled");
    }

    /// **每个资源字段用自己那套过滤器** —— 用户报的"新建时音乐的系统框只列 json"就是这条断了。
    ///
    /// 映射（动作 → 字段/过滤器/界面文案）全在库里，所以这条能在 bin 之外被钉住；
    /// 界面那两行也由同一份 [`ASSET_PICKS`] 驱动，不会各说各话。
    #[test]
    fn asset_picks_use_their_own_filters() {
        use crate::filedialog::{AUDIO_FILTER, IMAGE_FILTER};
        let audio = StartAction::PickAudio.asset().expect("选音乐要给出参数");
        assert_eq!(audio.field, AssetField::Audio);
        assert_eq!(audio.filter, AUDIO_FILTER);
        assert_eq!(audio.what, "音乐");
        assert_eq!(audio.label(), "音乐路径");
        assert!(!audio.filter.patterns.contains("json"), "音乐过滤器不该列 json");

        let art = StartAction::PickIllustration.asset().expect("选曲绘要给出参数");
        assert_eq!(art.field, AssetField::Illustration);
        assert_eq!(art.filter, IMAGE_FILTER);
        assert_eq!(art.what, "曲绘");
        assert_eq!(art.label(), "曲绘路径");
        assert!(art.filter.patterns.contains("png") && art.filter.patterns.contains("jpg"));

        // 两个字段互不相同，且文档字段名就是 `new` 命令认的那两个
        assert_ne!(audio.field, art.field);
        assert_eq!(AssetField::Audio.meta_key(), "audio");
        assert_eq!(AssetField::Illustration.meta_key(), "background");

        // 双向一致：`ASSET_PICKS[i].action().asset() == ASSET_PICKS[i]`（界面按 spec.action() 发动作）
        for spec in ASSET_PICKS {
            assert_eq!(spec.action().asset(), Some(spec), "{spec:?}");
        }
        // 不是资源动作的就没有 asset()
        for other in [StartAction::Create, StartAction::Skip, StartAction::OpenDialog] {
            assert!(other.asset().is_none(), "{other:?}");
        }

        // 写回字段 → `new` 命令的 meta：两边字段名同源，路径要去空白
        let mut f = NewChartForm::default();
        f.set_asset(AssetField::Audio, " /m/song.ogg ");
        f.set_asset(AssetField::Illustration, " /m/bg.png ");
        assert_eq!(f.asset(AssetField::Audio), " /m/song.ogg ");
        let cmd = f.to_new_command();
        assert_eq!(cmd["meta"]["audio"], "/m/song.ogg");
        assert_eq!(cmd["meta"]["background"], "/m/bg.png");
    }

    /// 新建谱面是**盖在列表上的模态**（不换整屏）：Esc 是"返回列表"，
    /// 而且那个 Esc **不会同时**把列表的"跳过进编辑器"也触发掉 —— 一份输入只能有一个主人。
    #[test]
    fn new_chart_modal_esc_goes_back_to_the_list_and_never_skips() {
        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(980.0, 620.0));
        let ctx = egui::Context::default();
        let r = Recents::default();
        let rows = list_rows(&r, 1000);
        let mut form = NewChartForm::default();
        let mut actions: Vec<StartAction> = Vec::new();
        let mut pass = |events: Vec<egui::Event>, actions: &mut Vec<StartAction>| {
            let raw = egui::RawInput { screen_rect: Some(rect), events, ..Default::default() };
            let mut out = ctx.run_ui(raw, |ui| {
                let screen = ui.max_rect();
                // 真实的画法：列表那一层 + 盖在它上面的模态，同一帧里
                if let Some(a) = start_screen_ui(ui, screen, &rows, None, "kdialog（测试）", true) {
                    actions.push(a);
                }
                if let Some(a) = new_chart_modal(ui.ctx(), &mut form, None) {
                    actions.push(a);
                }
            });
            out.textures_delta.clear();
        };
        pass(vec![], &mut actions); // 第一帧只是建立布局
        pass(
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }],
            &mut actions,
        );
        assert!(
            actions.contains(&StartAction::BackToList),
            "Esc 应回到列表，实际 {actions:?}"
        );
        assert!(
            !actions.contains(&StartAction::Skip),
            "模态的 Esc 不该把人踢进编辑器：{actions:?}"
        );
        assert_eq!(
            actions.len(),
            1,
            "一份 Esc 只该产生一个动作（弹窗的），实际 {actions:?}"
        );
    }

    /// 默认路径落在配置目录里（不写进工作目录）
    #[test]
    fn default_path_is_under_config_dir() {
        let p = default_path();
        let s = p.display().to_string();
        assert!(s.ends_with("OpenPhM/recents.json") || s.ends_with(".opm-recents.json"), "{s}");
    }
}
