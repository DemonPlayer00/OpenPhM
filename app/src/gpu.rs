//! **显卡选择策略（Linux 混显笔记本）**。
//!
//! 背景：这台机器是 AMD 核显 + NVIDIA RTX 5070 的混显本。wgpu 的默认电源偏好是
//! `HighPerformance`，于是**不指定任何东西**时程序会去开独显 —— 编谱这种活（2D、几十个实例）
//! 用核显足够，而独显的功耗与发热高得多，还会让笔记本风扇一直转。
//!
//! 所以这里的规矩是：**默认走核显；只有"显式指定"才用独显**。什么算显式：
//!
//! | 方式 | 含义 |
//! |---|---|
//! | `prime-run opm-app …` | 系统自带脚本，设 `__NV_PRIME_RENDER_OFFLOAD=1` / `__VK_LAYER_NV_optimus=NVIDIA_only` / `__GLX_VENDOR_LIBRARY_NAME=nvidia` —— **这就是"我要独显"** |
//! | `DRI_PRIME=1` | Mesa 那一套的等价写法（`DRI_PRIME=0` 表示"不要"，不算指定） |
//! | `OPM_GPU=discrete` | 本程序自己的开关（也可 `integrated` 明确要核显） |
//! | `WGPU_POWER_PREF=high` | wgpu 自己的偏好环境变量，尊重它 |
//!
//! 非 Linux 平台**不动**（Windows 的"高性能/省电"由系统设置决定，程序不该抢）—— 这条现在是
//! 结构化的平台契约，见下。
//!
//! ## 平台契约：**只有 Linux 会插手，别的平台保持默认选择器**
//!
//! 非 Linux（首要是 Windows）**一个字都不改**，全部交给平台自己的默认选择器：
//!
//! | 我们会不会动 | Linux | Windows / 其它 |
//! |---|---|---|
//! | 适配器选择器（`native_adapter_selector`） | 默认装（核显优先） | **不装** —— 除非用户自己设了 `OPM_GPU` |
//! | Vulkan ICD 白名单（`icd_plan`） | 默认摘掉 NVIDIA | **不碰**（连目录都不扫） |
//! | 后端集合（`backend_plan`） | 探到 Vulkan 就只开 Vulkan | **不碰**（egui-wgpu 默认 `PRIMARY \| GL`） |
//! | 电源偏好（`WGPU_POWER_PREF`） | 我们翻译成策略 | **不翻译** —— egui-wgpu 自己就 `from_env()` 读它 |
//! | prime-run / `DRI_PRIME` 标记 | 认 | **不认**（这些变量在别的平台没有意义） |
//!
//! 于是 Windows 上"程序用哪块卡"完全由系统那套（Windows 图形设置 / 驱动面板 / 混合输出）决定 ——
//! 那正是用户要的"保持默认显卡选择器"。**唯一能越过这条线的是 `OPM_GPU`**：它明确写着
//! "本程序自己的开关"，是用户点名要的，不是我们替他做的决定。
//!
//! 契约靠 [`manages_gpu`] 一处判定；`OPM_GPU_PLATFORM` 是**诊断钩子**，能把 Linux 构建当成
//! Windows 跑一遍 —— 于是"Windows 不干预"不是只写在文档里的声明，而是在本机真被执行到
//! （README 的「平台契约」与《框架选型》§7.44.3 有实测输出）。
//!
//! 纯逻辑都在这里（策略判定 + 打分），**不依赖 wgpu 类型**，所以能单测；
//! 把 `wgpu::Adapter` 映射成 [`GpuKind`] 的那几行在 `main.rs`（那里本来就有 wgpu）。

/// 适配器的大类（wgpu 的 `DeviceType` 去掉平台细节，便于单测）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuKind {
    /// 核显 / 集成显卡
    Integrated,
    /// 独显
    Discrete,
    /// 软件渲染（llvmpipe 之类）：**最次**，只该在没有硬件适配器时用
    Cpu,
    /// 虚拟 GPU（云桌面/直通）
    Virtual,
    Other,
}

/// 选卡策略
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuPolicy {
    /// 核显优先（**默认**：不给系统添功耗）
    IntegratedFirst,
    /// 独显优先（用户显式要求：prime-run 或 `OPM_GPU=discrete`）
    DiscreteFirst,
    /// 不干预（非 Linux，或 wgpu 自己的环境变量说了"none"）
    Default,
}

impl GpuPolicy {
    pub fn label(self) -> &'static str {
        match self {
            GpuPolicy::IntegratedFirst => "核显优先（默认：不占用独显）",
            GpuPolicy::DiscreteFirst => "独显优先（已显式指定）",
            GpuPolicy::Default => "交给平台默认",
        }
    }
}

/// 打分：**越大越优先**。同分时取先枚举到的（保持稳定）。
pub fn rank(policy: GpuPolicy, kind: GpuKind) -> u8 {
    match policy {
        // 核显优先，但"只有独显"时仍然用独显（否则等于不给用硬件）；软件渲染排最后
        GpuPolicy::IntegratedFirst => match kind {
            GpuKind::Integrated => 4,
            GpuKind::Discrete => 3,
            GpuKind::Virtual => 2,
            GpuKind::Other => 1,
            GpuKind::Cpu => 0,
        },
        GpuPolicy::DiscreteFirst => match kind {
            GpuKind::Discrete => 4,
            GpuKind::Integrated => 3,
            GpuKind::Virtual => 2,
            GpuKind::Other => 1,
            GpuKind::Cpu => 0,
        },
        GpuPolicy::Default => 0,
    }
}

/// 从候选里挑一个（**纯函数**）。`None` = 没得挑（空列表，或策略是"不干预"）。
///
/// `GpuPolicy::Default` 明确返回 `None`：它表示"交给平台"，调用点根本不会装选择器
/// （见 `main.rs`）—— 与其给一个"随便挑第一个"的结果（那会让 `Cpu` 这种最差候选被选中），
/// 不如在这里说"我不发表意见"。
pub fn pick_index(policy: GpuPolicy, kinds: &[GpuKind]) -> Option<usize> {
    if policy == GpuPolicy::Default {
        return None;
    }
    kinds
        .iter()
        .enumerate()
        .max_by_key(|(i, k)| (rank(policy, **k), std::cmp::Reverse(*i)))
        .map(|(i, _)| i)
}

/// **平台契约的唯一判定点**：本程序要不要按 Linux 那套插手显卡选择。
///
/// `linux_build` 是编译期事实（调用点传 `cfg!(target_os = "linux")`）。
/// `OPM_GPU_PLATFORM=windows|linux` 是**诊断钩子**（生产环境没人会设它）：在 Linux 上把它设成
/// `windows`，整条链路就走"非 Linux"分支 —— 于是"Windows 保持默认选择器"能被真的执行、
/// 被启动日志与 sysfs 验证，而不是靠阅读代码相信。
///
/// 认不出的值（含空串）**退回编译期平台**：钩子不该能凭一个错别字把程序带进另一条路。
pub fn manages_gpu(linux_build: bool, env: &dyn Fn(&str) -> Option<String>) -> bool {
    let forced = env("OPM_GPU_PLATFORM")
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty());
    match forced.as_deref() {
        Some("windows") | Some("win") | Some("other") | Some("macos") => false,
        Some("linux") => true,
        _ => linux_build,
    }
}

/// 环境变量 → 策略（**纯函数**：`env` 传进来，于是可测）。
///
/// `linux` = [`manages_gpu`] 的结论（**不是**"编译目标是 Linux"）：为 `false` 时除 `OPM_GPU`
/// 外一切都不拦 —— 包括 `WGPU_POWER_PREF`，那个变量 egui-wgpu 自己会读
/// （`PowerPreference::from_env()`），用不着我们翻译一遍（翻译反而多装一个选择器）。
///
/// 顺序即优先级：程序自己的开关 > prime-run 的标记 > wgpu 的环境变量 > 默认。
pub fn policy_from_env(
    linux: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> (GpuPolicy, &'static str) {
    let get = |k: &str| env(k).map(|v| v.trim().to_ascii_lowercase()).filter(|v| !v.is_empty());
    // ① 程序自己的开关
    match get("OPM_GPU").as_deref() {
        Some("discrete") | Some("high") | Some("nvidia") | Some("dGPU") => {
            return (GpuPolicy::DiscreteFirst, "OPM_GPU=discrete")
        }
        Some("integrated") | Some("low") | Some("igpu") => {
            return (GpuPolicy::IntegratedFirst, "OPM_GPU=integrated")
        }
        _ => {}
    }
    // ② prime-run / Mesa 的显式指定（**在 Linux 上才认**：别的平台这些变量没有意义）
    if linux {
        let offload = get("__NV_PRIME_RENDER_OFFLOAD").as_deref() == Some("1");
        let optimus = get("__VK_LAYER_NV_optimus").as_deref() == Some("nvidia_only");
        let glx = get("__GLX_VENDOR_LIBRARY_NAME").as_deref() == Some("nvidia");
        let dri = matches!(get("DRI_PRIME").as_deref(), Some(v) if v != "0");
        if offload || optimus || glx || dri {
            return (GpuPolicy::DiscreteFirst, "prime-run / DRI_PRIME：显式要独显");
        }
    }
    // ③ wgpu 自己的偏好变量：**只在 Linux 上翻译**。别的平台上 egui-wgpu 会自己
    //    `PowerPreference::from_env()` 读同一个变量（low/high/none 与我们的策略一一对应），
    //    我们插一手只会多装一个选择器 —— 那就不是"保持默认"了。
    if linux {
        match get("WGPU_POWER_PREF").as_deref() {
            Some("high") => return (GpuPolicy::DiscreteFirst, "WGPU_POWER_PREF=high"),
            Some("low") => return (GpuPolicy::IntegratedFirst, "WGPU_POWER_PREF=low"),
            Some("none") => return (GpuPolicy::Default, "WGPU_POWER_PREF=none"),
            _ => {}
        }
    }
    // ④ 默认
    if linux {
        (GpuPolicy::IntegratedFirst, "默认：核显优先，独显要显式指定")
    } else {
        (GpuPolicy::Default, "非 Linux：交给平台默认（不动选择器）")
    }
}

/// 启用哪些后端（`wgpu::Backends` 的"我们要哪些"）。
///
/// 为什么要它：wgpu 默认把所有后端都初始化一遍 —— 在 Linux 上这意味着**连 GL（Mesa EGL/GLX）
/// 也一起拉起来**，实测多花约 150 ms（枚举 772 → 622 ms）。而 GL 那条路我们只在
/// "Vulkan 里没有可用卡"时才用得到（本会话验证过：只挂 intel ICD 那次就是靠 GL 才出得了图）。
///
/// 所以：**能确定 Vulkan 可用时只开 Vulkan**（省掉 GL 初始化），否则保持全后端（留着回退）。
/// "确定可用"用的是**不会触发初始化的**探测：ICD json 存在 + Vulkan loader 库存在。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendPlan {
    /// 只开 Vulkan
    VulkanOnly,
    /// 全后端（保持 GL 回退）
    All,
}

impl BackendPlan {
    pub fn label(self) -> &'static str {
        match self {
            BackendPlan::VulkanOnly => "仅 Vulkan（检测到 ICD + loader，省掉 GL 初始化）",
            BackendPlan::All => "全部后端（保留 GL 回退）",
        }
    }
}

/// 后端计划（**纯函数**）：
/// - `OPM_BACKEND=vulkan|all` 说了算（`gl` 交给 `WGPU_BACKEND`，见下）；
/// - 用户设了 `WGPU_BACKEND` ⇒ **不动**（那是 wgpu 自己的开关，别抢）；
/// - Linux 且 ICD json 与 loader 都在 ⇒ 只开 Vulkan；
/// - 其余（非 Linux、探测不到 Vulkan）⇒ 全后端。
///
/// `linux` = [`manages_gpu`] 的结论。非 Linux 时调用点**连探测都不做**（`libvulkan.so.1`
/// 这种路径在 Windows 上本来就不存在），这里的结论也就是"不动后端集合"。
pub fn backend_plan(
    linux: bool,
    env: &dyn Fn(&str) -> Option<String>,
    vulkan_icd_present: bool,
    vulkan_loader_present: bool,
) -> (BackendPlan, &'static str) {
    let get = |k: &str| {
        env(k)
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty())
    };
    match get("OPM_BACKEND").as_deref() {
        Some("vulkan") | Some("vk") => return (BackendPlan::VulkanOnly, "OPM_BACKEND=vulkan"),
        Some("all") | Some("auto") => return (BackendPlan::All, "OPM_BACKEND=all"),
        _ => {}
    }
    if get("WGPU_BACKEND").is_some() {
        return (BackendPlan::All, "尊重 WGPU_BACKEND（用户自己指定了后端）");
    }
    if !linux {
        // 非 Linux：后端集合保持 egui-wgpu 的默认（`PRIMARY | GL`），我们不碰
        return (BackendPlan::All, "非 Linux：不动后端集合");
    }
    if vulkan_icd_present && vulkan_loader_present {
        return (BackendPlan::VulkanOnly, "检测到 Vulkan ICD 与 loader");
    }
    (BackendPlan::All, "没探测到可用的 Vulkan：保留全后端")
}

/// Vulkan ICD 声明文件可能所在的位置（按 Vulkan loader 的查找规则：环境变量优先，其次 XDG/默认目录）。
///
/// **Linux 专用**：非 Linux 平台不查 ICD（见 [`manages_gpu`]），这里给的也是 Linux 的路径。
pub fn icd_search_paths(env: &dyn Fn(&str) -> Option<String>) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut out: Vec<PathBuf> = Vec::new();
    // ① 显式指定（`VK_DRIVER_FILES` 是 `VK_ICD_FILENAMES` 的新名字）
    for k in ["VK_DRIVER_FILES", "VK_ICD_FILENAMES"] {
        if let Some(v) = env(k) {
            for p in v.split(':').filter(|p| !p.trim().is_empty()) {
                out.push(PathBuf::from(p.trim()));
            }
        }
    }
    // ② XDG 数据目录与默认目录下的 `vulkan/icd.d`
    let dirs = env("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    for d in dirs.split(':').filter(|d| !d.trim().is_empty()) {
        out.push(PathBuf::from(d.trim()).join("vulkan").join("icd.d"));
    }
    // ③ 兜底（有些发行版/驱动把 json 放这儿）
    out.push(PathBuf::from("/etc/vulkan/icd.d"));
    out
}

/// "别打扰独显"的 ICD 计划。
///
/// 背景（实测）：wgpu 枚举适配器时，Vulkan loader 会把**所有** ICD 都加载起来 ——
/// 包括 `nvidia_icd.json`。而加载 NVIDIA 的 Vulkan ICD 会把**已经在运行时断电（`runtime_status=suspended`）
/// 的独显唤醒**（实测：启动一次程序，`0000:01:00.0` 从 suspended 变 active），
/// 而"唤醒一块独显"本身就要几百毫秒 —— 这正是启动变慢的一大块，而且用户根本不打算用它。
///
/// 所以：**默认把 NVIDIA 的 ICD 从枚举里摘掉**，只留别的（核显/其它厂商）。
/// 只在下面这种情况下才摘（任何一个不满足都不动）：
/// 1. Linux；
/// 2. 用户**没有**显式指定 ICD（`VK_DRIVER_FILES`/`VK_ICD_FILENAMES`）；
/// 3. 用户**没有**显式要独显（`OPM_GPU=discrete` / prime-run / `DRI_PRIME≠0`）；
/// 4. 本机**还有别的 ICD 可用**（否则"只有 NVIDIA"的机器会被摘成没有显卡可用）。
///
/// 局限（写清楚，别让后来人以为它能分辨一切）：只能按**文件名**判断厂商，
/// 分不出"同厂商的核显与独显"；AMD 平台上的 `radeon_icd` 可能是独显，那种机器仍会被枚举到。
///
/// `linux` = [`manages_gpu`] 的结论：非 Linux 一律不干预（Windows 的显卡就归 Windows 管）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IcdPlan {
    /// `Some(paths)` = 只加载这些 ICD；`None` = 不干预
    pub keep: Option<Vec<String>>,
    pub reason: &'static str,
}

/// 算出 ICD 计划（**纯函数**：环境与"目录里有哪些 ICD"都由调用方给）
pub fn icd_plan(
    linux: bool,
    env: &dyn Fn(&str) -> Option<String>,
    icd_files: &[String],
) -> IcdPlan {
    let get = |k: &str| {
        env(k)
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty())
    };
    let no_change = |reason: &'static str| IcdPlan { keep: None, reason };
    if !linux {
        return no_change("非 Linux：不动 ICD");
    }
    if get("VK_DRIVER_FILES").is_some() || get("VK_ICD_FILENAMES").is_some() {
        return no_change("用户显式指定了 Vulkan ICD");
    }
    if policy_from_env(linux, env).0 == GpuPolicy::DiscreteFirst {
        return no_change("已显式要独显（prime-run / OPM_GPU=discrete）");
    }
    let is_nvidia = |p: &str| {
        std::path::Path::new(p)
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase().contains("nvidia"))
            .unwrap_or(false)
    };
    let keep: Vec<String> = icd_files.iter().filter(|p| !is_nvidia(p)).cloned().collect();
    if keep.is_empty() || keep.len() == icd_files.len() {
        // 只有 NVIDIA（单独显机器）⇒ 不能摘；没有 NVIDIA ⇒ 无所谓
        return no_change("没有可替换的非 NVIDIA ICD：不干预");
    }
    IcdPlan {
        keep: Some(keep),
        reason: "默认不唤醒独显：只加载非 NVIDIA 的 Vulkan ICD",
    }
}

/// 判定"Vulkan ICD 可用"（**纯函数**）：
/// - **显式指定了** ICD 文件（`VK_DRIVER_FILES`/`VK_ICD_FILENAMES`）时**必须全部存在** ——
///   指了却找不到说明这套 Vulkan 配置是坏的，此时不该信它（该退回全后端，留着 GL 回退）；
/// - 没显式指定时，看默认目录（`…/vulkan/icd.d`）里有没有 json。
///
/// 这一条是被自己的实验逼出来的：`VK_DRIVER_FILES=/nonexistent.json` 时 Vulkan 枚举是空的，
/// 而**修好之前**默认会"因为看到 ICD 就只开 Vulkan" ⇒ 直接起不来（全后端时它会退到 GL 出图）。
pub fn vulkan_icd_usable(explicit_exists: &[bool], default_dir_has_json: bool) -> bool {
    if explicit_exists.is_empty() {
        default_dir_has_json
    } else {
        explicit_exists.iter().all(|ok| *ok)
    }
}

/// Vulkan loader 库的常见位置（`ldconfig` 太贵，直接看这几个路径就够）
pub fn loader_candidates() -> [&'static str; 4] {
    [
        "/usr/lib/libvulkan.so.1",
        "/usr/lib64/libvulkan.so.1",
        "/usr/lib/x86_64-linux-gnu/libvulkan.so.1",
        "/usr/local/lib/libvulkan.so.1",
    ]
}

/// 一句话说明当前策略（启动时打一行，用户就知道程序用了哪块卡、为什么）
pub fn describe(policy: GpuPolicy, why: &str) -> String {
    format!("{}（{why}）", policy.label())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| {
            map.iter()
                .find(|(mk, _)| mk == k)
                .map(|(_, v)| v.clone())
        }
    }

    /// 默认（什么都不设）⇒ **核显优先**：这就是用户要的"没指定就别开独显"
    #[test]
    fn default_is_integrated_first_on_linux() {
        let (p, why) = policy_from_env(true, &env_of(&[]));
        assert_eq!(p, GpuPolicy::IntegratedFirst);
        assert!(why.contains("默认"), "{why}");
        assert!(describe(p, why).contains("核显优先"));
        // 非 Linux：不干预
        let (p, _) = policy_from_env(false, &env_of(&[]));
        assert_eq!(p, GpuPolicy::Default);
    }

    /// **Windows 保持默认选择器**（穷举）：非 Linux 上，除了用户自己点名的 `OPM_GPU`，
    /// 任何环境组合都不许让我们插手 —— 不改 ICD 白名单、不改后端集合、不装选择器。
    ///
    /// 环境池专挑"在 Linux 上会立刻改变行为"的那些变量：prime-run 的三个标记、显式 ICD、
    /// wgpu 自己的 `WGPU_POWER_PREF`/`WGPU_BACKEND`。2^7 = 128 种组合逐个过。
    #[test]
    fn non_linux_never_interferes_with_the_platform_selector() {
        let pool: [(&str, &str); 7] = [
            ("DRI_PRIME", "1"),
            ("__NV_PRIME_RENDER_OFFLOAD", "1"),
            ("__GLX_VENDOR_LIBRARY_NAME", "nvidia"),
            ("VK_DRIVER_FILES", "/usr/share/vulkan/icd.d/nvidia_icd.json"),
            ("VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/nvidia_icd.json"),
            ("WGPU_POWER_PREF", "high"),
            ("WGPU_BACKEND", "vulkan"),
        ];
        let icds = vec![
            "/usr/share/vulkan/icd.d/radeon_icd.json".to_owned(),
            "/usr/share/vulkan/icd.d/nvidia_icd.json".to_owned(),
        ];
        for mask in 0u32..(1 << pool.len()) {
            let pairs: Vec<(&str, &str)> = (0..pool.len())
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| pool[i])
                .collect();
            let env = env_of(&pairs);
            // ① 策略 = 交给平台（`Default` ⇒ 调用点根本不装选择器）
            let (p, why) = policy_from_env(false, &env);
            assert_eq!(p, GpuPolicy::Default, "{pairs:?} ⇒ {why}");
            assert_eq!(
                pick_index(p, &[GpuKind::Integrated, GpuKind::Discrete]),
                None,
                "{pairs:?}"
            );
            // ② ICD 白名单：一个字都不改（哪怕显式指了 NVIDIA 的 ICD）
            assert_eq!(icd_plan(false, &env, &icds).keep, None, "{pairs:?}");
            // ③ 后端集合：即便"探测到 Vulkan"也不改（非 Linux 上连探测都不做）
            assert_eq!(backend_plan(false, &env, true, true).0, BackendPlan::All, "{pairs:?}");
        }
        // ④ `WGPU_POWER_PREF` 在非 Linux 上**交还给 wgpu**：egui-wgpu 自己 `PowerPreference::from_env()`
        //    读它（low/high/none 与我们的策略一一对应）；我们翻译一遍只会多装一个选择器。
        for (v, on_linux) in [
            ("low", GpuPolicy::IntegratedFirst),
            ("high", GpuPolicy::DiscreteFirst),
            ("none", GpuPolicy::Default),
        ] {
            let env = env_of(&[("WGPU_POWER_PREF", v)]);
            assert_eq!(policy_from_env(true, &env).0, on_linux, "{v}");
            assert_eq!(policy_from_env(false, &env).0, GpuPolicy::Default, "{v}");
        }
    }

    /// 唯一的例外是 `OPM_GPU`：它写着"本程序自己的开关"，是用户点名要的，非 Linux 也照办
    #[test]
    fn opm_gpu_is_the_only_knob_that_works_on_non_linux() {
        let (p, why) = policy_from_env(false, &env_of(&[("OPM_GPU", "integrated")]));
        assert_eq!(p, GpuPolicy::IntegratedFirst);
        assert!(why.contains("OPM_GPU"), "{why}");
        assert_eq!(
            policy_from_env(false, &env_of(&[("OPM_GPU", "discrete")])).0,
            GpuPolicy::DiscreteFirst
        );
        // 但"选哪块卡"之外的事仍然是 Linux 的活儿：非 Linux 不动 ICD 白名单
        let icds = vec![
            "/usr/share/vulkan/icd.d/nvidia_icd.json".to_owned(),
            "/usr/share/vulkan/icd.d/radeon_icd.json".to_owned(),
        ];
        assert_eq!(
            icd_plan(false, &env_of(&[("OPM_GPU", "integrated")]), &icds).keep,
            None
        );
    }

    /// 平台钩子 `OPM_GPU_PLATFORM`：能把 Linux 构建当成 Windows 跑（"不干预"这条因此可执行、
    /// 可验证）；认不出的值退回编译期平台 —— 钩子不该被一个错别字带进另一条路。
    #[test]
    fn platform_hook_switches_the_contract() {
        assert!(manages_gpu(true, &env_of(&[])));
        assert!(!manages_gpu(false, &env_of(&[])));
        for v in ["windows", " Win ", "other", "macos"] {
            assert!(!manages_gpu(true, &env_of(&[("OPM_GPU_PLATFORM", v)])), "{v}");
        }
        assert!(manages_gpu(false, &env_of(&[("OPM_GPU_PLATFORM", "linux")])));
        for v in ["", "  ", "windwos", "win32"] {
            assert!(manages_gpu(true, &env_of(&[("OPM_GPU_PLATFORM", v)])), "{v:?}");
            assert!(!manages_gpu(false, &env_of(&[("OPM_GPU_PLATFORM", v)])), "{v:?}");
        }
    }

    /// prime-run 的三个标记 + Mesa 的 DRI_PRIME 都算"显式要独显"；`DRI_PRIME=0` 不算
    #[test]
    fn prime_run_markers_select_the_discrete_gpu() {
        for (k, v) in [
            ("__NV_PRIME_RENDER_OFFLOAD", "1"),
            ("__VK_LAYER_NV_optimus", "NVIDIA_only"),
            ("__GLX_VENDOR_LIBRARY_NAME", "nvidia"),
            ("DRI_PRIME", "1"),
            ("DRI_PRIME", "0000:01:00.0"),
        ] {
            let (p, why) = policy_from_env(true, &env_of(&[(k, v)]));
            assert_eq!(p, GpuPolicy::DiscreteFirst, "{k}={v} 应要独显");
            assert!(why.contains("prime-run"), "{why}");
        }
        // DRI_PRIME=0 = "不要用另一块卡"，不是"要独显"
        let (p, _) = policy_from_env(true, &env_of(&[("DRI_PRIME", "0")]));
        assert_eq!(p, GpuPolicy::IntegratedFirst);
        // 非 Linux 上这些变量不认（不该因为环境里恰好有同名变量就改策略）
        let (p, _) = policy_from_env(false, &env_of(&[("DRI_PRIME", "1")]));
        assert_eq!(p, GpuPolicy::Default);
    }

    /// 程序自己的开关优先于其它一切；`WGPU_POWER_PREF` 也尊重
    #[test]
    fn explicit_knobs_win_in_order() {
        // OPM_GPU 压过 prime-run 标记
        let (p, why) = policy_from_env(
            true,
            &env_of(&[("OPM_GPU", "integrated"), ("DRI_PRIME", "1")]),
        );
        assert_eq!(p, GpuPolicy::IntegratedFirst);
        assert!(why.contains("OPM_GPU=integrated"), "{why}");
        let (p, _) = policy_from_env(true, &env_of(&[("OPM_GPU", "discrete")]));
        assert_eq!(p, GpuPolicy::DiscreteFirst);
        // wgpu 自己的变量：high 要独显、low 要核显、none 不干预
        let (p, why) = policy_from_env(true, &env_of(&[("WGPU_POWER_PREF", "high")]));
        assert_eq!(p, GpuPolicy::DiscreteFirst);
        assert!(why.contains("WGPU_POWER_PREF"), "{why}");
        assert_eq!(
            policy_from_env(true, &env_of(&[("WGPU_POWER_PREF", "low")])).0,
            GpuPolicy::IntegratedFirst
        );
        assert_eq!(
            policy_from_env(true, &env_of(&[("WGPU_POWER_PREF", "none")])).0,
            GpuPolicy::Default
        );
        // 大小写与空白都容错
        assert_eq!(
            policy_from_env(true, &env_of(&[("OPM_GPU", " Discrete ")])).0,
            GpuPolicy::DiscreteFirst
        );
    }

    /// **穷举所有适配器组合**（2^5 个子集 × 3 种策略）：机制必须在任何硬件组合上都给出
    /// 一个可用结论 —— 单核显、单独显、非 NVIDIA 独显、核显+独显、只有虚拟卡、只有软件渲染……
    ///
    /// 注意这套逻辑**完全不看厂商**：它只吃 `GpuKind`（来自 wgpu 的 `DeviceType`），
    /// 所以"非 NVIDIA 独显"（AMD/Intel 独显）与 NVIDIA 独显走的是同一条路 —— 这也正是要保证的。
    #[test]
    fn every_adapter_combination_yields_a_sane_choice() {
        use GpuKind::*;
        let all = [Integrated, Discrete, Virtual, Cpu, Other];
        // 只穷举两种"真挑卡"的策略：`Default` = 交给平台（`pick_index` 对它返回 `None`，见下一条测试）
        let policies = [GpuPolicy::IntegratedFirst, GpuPolicy::DiscreteFirst];
        let mut checked = 0;
        for mask in 0u32..(1 << all.len()) {
            let kinds: Vec<GpuKind> = (0..all.len())
                .filter(|i| mask & (1 << i) != 0)
                .map(|i| all[i])
                .collect();
            for policy in policies {
                let pick = pick_index(policy, &kinds);
                match (kinds.is_empty(), pick) {
                    // 空列表：必须给出 None（调用点据此报"没有可用的图形适配器"，而不是 panic）
                    (true, None) => {}
                    (true, Some(_)) => panic!("空候选不该选出东西"),
                    // 非空：必须选出**存在的**那一个
                    (false, Some(i)) => {
                        assert!(i < kinds.len(), "下标越界：{kinds:?} → {i}");
                        let chosen = kinds[i];
                        // 不变量 1：有硬件适配器时**绝不**选软件渲染
                        if kinds.iter().any(|k| *k != Cpu) {
                            assert_ne!(chosen, Cpu, "有硬件却选了软件渲染：{kinds:?} → {chosen:?}");
                        }
                        // 不变量 2：核显优先时，存在核显就必须选核显
                        if policy == GpuPolicy::IntegratedFirst && kinds.contains(&Integrated) {
                            assert_eq!(chosen, Integrated, "{kinds:?}");
                        }
                        // 不变量 3：独显优先时，存在独显就必须选独显
                        if policy == GpuPolicy::DiscreteFirst && kinds.contains(&Discrete) {
                            assert_eq!(chosen, Discrete, "{kinds:?}");
                        }
                        // 不变量 4：**单独显**机器不受"核显优先"影响（照样用独显）
                        if kinds == vec![Discrete] {
                            assert_eq!(chosen, Discrete, "只有独显时必须用它（不能宁可不给用）");
                        }
                        // 不变量 5：只有软件渲染时也只能用它（比开不了窗口好）
                        if kinds == vec![Cpu] {
                            assert_eq!(chosen, Cpu);
                        }
                        // 不变量 6：确定性 —— 同一输入两次结果相同
                        assert_eq!(pick_index(policy, &kinds), Some(i));
                    }
                    (false, None) => panic!("非空候选必须选出东西：{kinds:?} / {policy:?}"),
                }
                checked += 1;
            }
        }
        assert_eq!(checked, 32 * 2, "应当覆盖全部 32 个子集 × 2 种挑卡策略");
    }

    /// 后端计划：能确定 Vulkan 可用就只开 Vulkan；用户自己的开关一概不抢
    #[test]
    fn backend_plan_prefers_vulkan_but_never_overrides_the_user() {
        let none = env_of(&[]);
        // Linux + ICD + loader ⇒ 只开 Vulkan（省掉 GL 初始化）
        let (p, why) = backend_plan(true, &none, true, true);
        assert_eq!(p, BackendPlan::VulkanOnly);
        assert!(why.contains("Vulkan"), "{why}");
        assert!(p.label().contains("仅 Vulkan"));
        // 缺 ICD 或缺 loader ⇒ 保留全后端（**回退比省 150ms 重要**）
        assert_eq!(backend_plan(true, &none, false, true).0, BackendPlan::All);
        assert_eq!(backend_plan(true, &none, true, false).0, BackendPlan::All);
        assert_eq!(backend_plan(true, &none, false, false).0, BackendPlan::All);
        // 非 Linux 不动
        assert_eq!(backend_plan(false, &none, true, true).0, BackendPlan::All);
        // 用户设了 WGPU_BACKEND ⇒ 尊重它，别改后端集合
        let (p, why) = backend_plan(true, &env_of(&[("WGPU_BACKEND", "gl")]), true, true);
        assert_eq!(p, BackendPlan::All);
        assert!(why.contains("WGPU_BACKEND"), "{why}");
        // 我们的开关压过一切
        assert_eq!(
            backend_plan(true, &env_of(&[("OPM_BACKEND", "vulkan"), ("WGPU_BACKEND", "gl")]), false, false).0,
            BackendPlan::VulkanOnly
        );
        assert_eq!(
            backend_plan(true, &env_of(&[("OPM_BACKEND", "all")]), true, true).0,
            BackendPlan::All
        );
    }

    /// "别打扰独显"：默认摘掉 NVIDIA 的 ICD；但**单独显机器**、**显式指定**、**非 Linux** 都不动
    #[test]
    fn nvidia_icd_is_dropped_unless_the_user_asked_for_it() {
        let mixed = vec![
            "/usr/share/vulkan/icd.d/radeon_icd.json".to_owned(),
            "/usr/share/vulkan/icd.d/intel_icd.json".to_owned(),
            "/usr/share/vulkan/icd.d/nvidia_icd.json".to_owned(),
        ];
        let none = env_of(&[]);
        // ① 默认：摘掉 nvidia，留别的
        let p = icd_plan(true, &none, &mixed);
        let keep = p.keep.expect("应当给出白名单");
        assert_eq!(keep.len(), 2);
        assert!(keep.iter().all(|k| !k.contains("nvidia")), "{keep:?}");
        assert!(p.reason.contains("独显"), "{}", p.reason);
        // ② 只有 NVIDIA（单独显机器）⇒ 不能摘，否则这台机器就没 Vulkan 了
        let only_nv = vec!["/usr/share/vulkan/icd.d/nvidia_icd.json".to_owned()];
        assert_eq!(icd_plan(true, &none, &only_nv).keep, None);
        // ③ 没有 NVIDIA ⇒ 无所谓（也不必改环境）
        let no_nv = vec!["/usr/share/vulkan/icd.d/radeon_icd.json".to_owned()];
        assert_eq!(icd_plan(true, &none, &no_nv).keep, None);
        // ④ 显式要独显 ⇒ 不动（prime-run / OPM_GPU=discrete / DRI_PRIME）
        for pairs in [
            vec![("OPM_GPU", "discrete")],
            vec![("DRI_PRIME", "1")],
            vec![("__NV_PRIME_RENDER_OFFLOAD", "1")],
        ] {
            let p = icd_plan(true, &env_of(&pairs), &mixed);
            assert_eq!(p.keep, None, "{pairs:?} 应不干预：{}", p.reason);
        }
        // ⑤ 显式指定 ICD ⇒ 不动
        assert_eq!(
            icd_plan(true, &env_of(&[("VK_DRIVER_FILES", "/a.json")]), &mixed).keep,
            None
        );
        // ⑥ 非 Linux ⇒ 不动
        assert_eq!(icd_plan(false, &none, &mixed).keep, None);
        // ⑦ 空目录 ⇒ 不动
        assert_eq!(icd_plan(true, &none, &[]).keep, None);
    }

    /// ICD 可用性：显式指定必须全部存在（指了却找不到 = 别信 Vulkan，退回全后端留 GL 回退）
    #[test]
    fn explicit_icd_paths_must_all_exist() {
        // 没显式指定 ⇒ 看默认目录
        assert!(vulkan_icd_usable(&[], true));
        assert!(!vulkan_icd_usable(&[], false));
        // 显式指定：一个不存在就不信
        assert!(vulkan_icd_usable(&[true], false));
        assert!(vulkan_icd_usable(&[true, true], false));
        assert!(!vulkan_icd_usable(&[true, false], true), "缺一个也不该信");
        assert!(!vulkan_icd_usable(&[false], true));
    }

    /// ICD 查找路径：环境变量优先、XDG 目录其次、再兜底；loader 只看几个常见路径
    #[test]
    fn icd_and_loader_probe_paths_are_sane() {
        let p = icd_search_paths(&env_of(&[("VK_DRIVER_FILES", "/a/x.json:/b/y.json")]));
        assert_eq!(p[0], std::path::PathBuf::from("/a/x.json"));
        assert_eq!(p[1], std::path::PathBuf::from("/b/y.json"));
        assert!(p.iter().any(|q| q.ends_with("vulkan/icd.d")));
        // 空的/带空格的段要跳过
        let p = icd_search_paths(&env_of(&[("VK_ICD_FILENAMES", "  : ")]));
        assert!(p.iter().all(|q| q.to_string_lossy().contains("vulkan") || q.to_string_lossy().starts_with("/etc")));
        // XDG_DATA_DIRS 生效
        let p = icd_search_paths(&env_of(&[("XDG_DATA_DIRS", "/opt/share")]));
        assert!(p.iter().any(|q| q == &std::path::PathBuf::from("/opt/share/vulkan/icd.d")));
        assert!(loader_candidates().iter().all(|p| p.ends_with("libvulkan.so.1")));
    }

    /// `Default` = "交给平台"：**不给出选择**（否则"随便挑第一个"会把软件渲染那种最差候选选中）。
    /// 调用点（`main.rs`）对 Default 根本不装选择器，这条测试钉住这层语义。
    #[test]
    fn default_policy_abstains_instead_of_picking_arbitrarily() {
        use GpuKind::*;
        for kinds in [
            vec![],
            vec![Cpu],
            vec![Cpu, Other],
            vec![Integrated, Discrete, Cpu],
        ] {
            assert_eq!(pick_index(GpuPolicy::Default, &kinds), None, "{kinds:?}");
        }
    }

    /// 策略**只看 `device_type`，不看厂商**：非 NVIDIA 独显与 NVIDIA 独显走同一条路。
    ///
    /// 这条是结构性保证（`GpuKind` 里根本没有厂商信息），用一个"名字很像 NVIDIA"的例子说明：
    /// 决定选谁的只有类别，厂商标识最多进日志。
    #[test]
    fn selection_is_vendor_blind() {
        use GpuKind::*;
        // 同一组类别，无论现实里是 AMD/Intel/NVIDIA 的独显，结论都一样
        let kinds = [Integrated, Discrete];
        assert_eq!(pick_index(GpuPolicy::IntegratedFirst, &kinds), Some(0));
        assert_eq!(pick_index(GpuPolicy::DiscreteFirst, &kinds), Some(1));
        // 只有独显（不管是谁家的）⇒ 都用它
        for policy in [GpuPolicy::IntegratedFirst, GpuPolicy::DiscreteFirst] {
            assert_eq!(pick_index(policy, &[Discrete]), Some(0), "{policy:?}");
        }
        // 混合：Intel 核显 + AMD 独显、AMD 核显 + NVIDIA 独显……在类型上无法区分 ⇒ 同一条路径
    }

    /// 挑卡：有核显就用核显；**只有独显时仍然用独显**（不是"拒绝用硬件"）；
    /// 软件渲染排最后；独显优先策略反过来
    #[test]
    fn picking_prefers_the_right_adapter() {
        use GpuKind::*;
        let kinds = [Discrete, Integrated, Cpu];
        assert_eq!(pick_index(GpuPolicy::IntegratedFirst, &kinds), Some(1));
        assert_eq!(pick_index(GpuPolicy::DiscreteFirst, &kinds), Some(0));
        // 只有独显 ⇒ 还是用它
        assert_eq!(pick_index(GpuPolicy::IntegratedFirst, &[Discrete]), Some(0));
        // 独显 + 软件 ⇒ 硬件赢
        assert_eq!(
            pick_index(GpuPolicy::IntegratedFirst, &[Cpu, Discrete]),
            Some(1)
        );
        // 只有软件 ⇒ 也只能用它（比开不了窗口好）
        assert_eq!(pick_index(GpuPolicy::IntegratedFirst, &[Cpu]), Some(0));
        // 空列表 ⇒ None
        assert_eq!(pick_index(GpuPolicy::IntegratedFirst, &[]), None);
        // 同分取先枚举到的（稳定）
        assert_eq!(
            pick_index(GpuPolicy::IntegratedFirst, &[Integrated, Integrated]),
            Some(0)
        );
        // 虚拟 GPU 排在硬件之后、软件之前
        assert_eq!(
            pick_index(GpuPolicy::IntegratedFirst, &[Virtual, Cpu]),
            Some(0)
        );
        assert!(rank(GpuPolicy::IntegratedFirst, Integrated) > rank(GpuPolicy::IntegratedFirst, Discrete));
        assert!(rank(GpuPolicy::DiscreteFirst, Discrete) > rank(GpuPolicy::DiscreteFirst, Integrated));
    }
}
