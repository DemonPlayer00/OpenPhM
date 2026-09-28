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
//! 非 Linux 平台**不动**（Windows 的"高性能/省电"由系统设置决定，程序不该抢）。
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

/// 环境变量 → 策略（**纯函数**：`env` 传进来，于是可测）。
///
/// 顺序即优先级：程序自己的开关 > prime-run 的标记 > wgpu 的环境变量 > 默认。
pub fn policy_from_env(
    is_linux: bool,
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
    if is_linux {
        let offload = get("__NV_PRIME_RENDER_OFFLOAD").as_deref() == Some("1");
        let optimus = get("__VK_LAYER_NV_optimus").as_deref() == Some("nvidia_only");
        let glx = get("__GLX_VENDOR_LIBRARY_NAME").as_deref() == Some("nvidia");
        let dri = matches!(get("DRI_PRIME").as_deref(), Some(v) if v != "0");
        if offload || optimus || glx || dri {
            return (GpuPolicy::DiscreteFirst, "prime-run / DRI_PRIME：显式要独显");
        }
    }
    // ③ wgpu 自己的偏好变量：尊重它（`high` 才是"要独显"）
    match get("WGPU_POWER_PREF").as_deref() {
        Some("high") => return (GpuPolicy::DiscreteFirst, "WGPU_POWER_PREF=high"),
        Some("low") => return (GpuPolicy::IntegratedFirst, "WGPU_POWER_PREF=low"),
        Some("none") => return (GpuPolicy::Default, "WGPU_POWER_PREF=none"),
        _ => {}
    }
    // ④ 默认
    if is_linux {
        (GpuPolicy::IntegratedFirst, "默认：核显优先，独显要显式指定")
    } else {
        (GpuPolicy::Default, "非 Linux：交给平台默认")
    }
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
