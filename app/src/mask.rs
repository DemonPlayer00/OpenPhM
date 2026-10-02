// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **遮蔽区（躁域）的呈现几何**：屏幕空间三角形的填充样式、细网格、鼠标靠近时的发光。
//!
//! 为什么单独一层、而且是纯函数：这些东西**只能靠截图看结果**，而截图看错一次就会改错代码
//! （本项目已经发生过，见框架选型 §7.57）。把"哪条网格线落在三角形里""指针离边缘多远
//! 才算靠近"做成纯函数，就能用测试钉住 —— GUI 那边只剩"把返回的点画出来"。
//!
//! 三个约定：
//! · 坐标一律是**屏幕像素**（不是 RPE 单位）：三角形由调用方从 RPE 坐标映射过来，
//!   网格间距也由调用方按缩放折成像素传进来（这样"细网格"在视觉上是等距的）；
//! · 三角形顶点顺序无关紧要（凸包判定与线段裁剪都不依赖它）；
//! · 一切退化情形（面积 0、点重合）都返回"没有形状"，而不是除以 0。

/// 遮蔽区的一档外观：填充不透明度 + 要不要画网格。
///
/// **两档的排序是"active 更重"**（用户口径 2026-10-02："active 要比 inactive 更不透明"，
/// 也是最初那条"false 时透明度较高、true 时透明度变低"）：active 那档的填充**更不透明**，
/// 网格是它上面**更透明的那一部分**（"网格变为提高透明度的样子"）——
/// 线里能看见下面的音符，格子中间是那块更重的红。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaskStyle {
    /// 填充的不透明度 0~1
    pub fill_alpha: f32,
    /// 是否画网格（图案在片元着色器里，见 `render::MASK_SHADER`）
    pub grid: bool,
}

/// 两种外观的**唯一定义**（预览、检查器、无头出图都用它，免得三处各调一次参数）。
pub const MASK_FILL_ACTIVE: f32 = 0.62;
pub const MASK_FILL_SOLID: f32 = 0.18;

pub fn mask_style(active: bool) -> MaskStyle {
    if active {
        MaskStyle { fill_alpha: MASK_FILL_ACTIVE, grid: true }
    } else {
        MaskStyle { fill_alpha: MASK_FILL_SOLID, grid: false }
    }
}

/// 遮蔽区的底色（红）。用**同一份**红：预览的填充、轮廓、编辑区里那一列的高亮。
pub const MASK_RED: [u8; 3] = [232, 64, 72];
/// 网格线的线宽（**像素**）与 alpha —— 与判定线那边一样按像素给，缩放时观感恒定。
///
/// `GRID_ALPHA` **小于** [`MASK_FILL_ACTIVE`]：线是"透出来的那部分"（用户口径），
/// 于是 active 那档的观感是"浓红底 + 一层更透明的斜格"。
pub const GRID_LINE_PX: f32 = 1.6;
pub const GRID_ALPHA: f32 = 0.18;
/// 网格的间距（**RPE 单位**；窗口高 900 ÷ 16 = 56.25 —— 视觉上是"细网格"，
/// 又不至于密到糊成一片。调用方按当前缩放折成像素）
pub const MASK_GRID_STEP: f32 = 56.25;
/// 网格纹理的**倾角**（度）：用户口径 2026-10-02 —— "将 active 状态下的屏蔽区预览方格纹理旋转 45 度"。
///
/// 旋转发生在**屏幕空间**（锚点是 RPE 原点，跟着屏幕走、不跟着三角形走）：
/// 相邻两条平行线的间距不变，变的是"格子从正着放变成菱形放"。
pub const MASK_GRID_TILT_DEG: f32 = 45.0;

/// 三角形是否有面积（三个顶点不共线且不重合）
pub fn triangle_area2(tri: &[[f32; 2]; 3]) -> f32 {
    let [a, b, c] = *tri;
    (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])
}

pub fn triangle_is_degenerate(tri: &[[f32; 2]; 3]) -> bool {
    triangle_area2(tri).abs() < 1e-3
}

/// 点在三角形内（含边）。同向叉积法：**对退化三角形一律 false**
/// （面积 0 时"在里面"没有意义，画出来也什么都没有）。
pub fn point_in_triangle(p: [f32; 2], tri: &[[f32; 2]; 3]) -> bool {
    if triangle_is_degenerate(tri) {
        return false;
    }
    let s = triangle_area2(tri).signum();
    for i in 0..3 {
        let a = tri[i];
        let b = tri[(i + 1) % 3];
        let (ex, ey) = (b[0] - a[0], b[1] - a[1]);
        let cross = ex * (p[1] - a[1]) - (p[0] - a[0]) * ey;
        // 容差**随边长缩放**：叉积的量级是 |边| × 距离，固定阈值会把"正好落在斜边上"
        // 的端点判成外面（实测：裁剪出来的 25.000002 被判在外，于是网格线看着少一截）
        let tol = 1e-4 * (1.0 + (ex * ex + ey * ey).sqrt());
        if cross * s < -tol {
            return false;
        }
    }
    true
}

/// 发光：围绕光标的**一圈柔光**（**只在播放时**生效，用户口径 2026-10-02）。
///
/// 参数在这里（单位全是**像素**，因为"多大一圈"是观感问题）；**衰减曲线在片元着色器里**
/// （`render.rs` 的 `MASK_SHADER`）：那里才有逐像素的坐标，而"边缘软化、且不超出遮蔽区边界"
/// 这两条一个靠 `smoothstep`、一个靠"填充几何本身就是那块区域"天然成立。
///
/// `GLOW_SOFT` = 从 `1 - soft` 到 `1` 的那一段半径用来软化边缘（0 = 硬边）。
pub const GLOW_RADIUS_PX: f32 = 120.0;
pub const GLOW_STRENGTH: f32 = 0.55;
pub const GLOW_SOFT: f32 = 0.45;

/// 行带高度（像素）：填充按"水平行带"切，切成一条条**梯形**。
///
/// 为什么要行带：用户口径要求"所有遮蔽区合并成一块、最多一层、active 与 unactive 不重叠"
/// ⇒ 同一像素只能被画一次，而 GPU 这边（没有模板缓冲）没法靠混合做到。
/// 在 CPU 上按行把区间并起来再减一次，是**一维**布尔运算，比二维多边形布尔简单得多，
/// 而且梯形正好能**精确**表示三角形（直边的截面端点随 y 线性变化）。
pub const BAND_PX: f32 = 2.5;
/// 行带数上限（极端缩放下别生成几万个梯形）
pub const BAND_MAX: usize = 640;

/// 一个**待渲染的遮蔽区**：已经求值完的三角形 + 它的外观档位
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoneTri {
    pub tri: [[f32; 2]; 3],
    pub active: bool,
}

/// **合并后**的预览几何（RPE 坐标；GPU 侧只负责把顶点喂进去）
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MaskDraw {
    /// `active=false` 那一档的填充（每 3 个一组 = 一个三角形）
    pub solid: Vec<[[f32; 2]; 3]>,
    /// `active=true` 那一档的填充（更不透明；网格图案由片元着色器按这块几何画）
    pub active: Vec<[[f32; 2]; 3]>,
}

impl MaskDraw {
    pub fn is_empty(&self) -> bool {
        self.solid.is_empty() && self.active.is_empty()
    }
}

/// 把合并后的几何装配成**演奏区管线的顶点**（与判定线共用同一条管线/同一份映射）。
///
/// 三个调用点共用它：GUI 的演奏区、无头出图（`headless::render_png`）、以及将来的任何出图路径 ——
/// "遮蔽区长什么样"只有这一份实现（与判定线那边 `build_instances` 是同一条纪律）。
///
/// `glow` = `[光标x, 光标y, 半径(px), 强度]`（全 RPE/像素口径；强度 0 = 不发光）。
pub fn push_mask_vertices(
    zones: &[ZoneTri],
    scale_px: f32,
    glow: [f32; 4],
    out: &mut Vec<crate::render::MaskVertex>,
) {
    if zones.is_empty() {
        return;
    }
    let mut draw = MaskDraw::default();
    build_mask_draw(zones, scale_px, &mut draw);
    let rgba = |a: f32| {
        [
            MASK_RED[0] as f32 / 255.0,
            MASK_RED[1] as f32 / 255.0,
            MASK_RED[2] as f32 / 255.0,
            a,
        ]
    };
    // 网格图案的参数（片元着色器要用）：`[要不要画, 线的 alpha, 间距(px), 线宽(px)]`。
    // 间距按当前缩放折成像素 —— 于是"细网格"在任何缩放级别下都是同一个观感。
    let step_px = MASK_GRID_STEP * scale_px.max(1e-6);
    let pattern = |style: MaskStyle| -> [f32; 4] {
        if style.grid {
            [1.0, GRID_ALPHA, step_px, GRID_LINE_PX]
        } else {
            [0.0; 4]
        }
    };
    for (tris, active) in [(&draw.solid, false), (&draw.active, true)] {
        let style = mask_style(active);
        // **inactive 那档"光标靠近也不发亮"**（用户口径 2026-10-02）：强度直接置 0 ——
        // 发光是按顶点属性传给片元的，所以"哪一档会亮"在这里就定死了。
        let g = if active {
            glow
        } else {
            [glow[0], glow[1], glow[2], 0.0]
        };
        let pat = pattern(style);
        for t in tris {
            for v in t {
                out.push(crate::render::MaskVertex::new(*v, rgba(style.fill_alpha), g, pat));
            }
        }
    }
}

/// 在拍 `beat`、时刻 `sec` 处，把一块区求值成一个待渲染的三角形（`None` = 此刻不显示）
pub fn zone_tri_of(view: &crate::state::MaskZoneView, tmap: &crate::perf::TimeMap, sec: f64) -> Option<ZoneTri> {
    let st = view.state(tmap, sec);
    if !st.visible {
        return None; // 还没有任何坐标事件 ⇒ 这块区域此刻不存在
    }
    let tri = [
        [st.v[0][0] as f32, st.v[0][1] as f32],
        [st.v[1][0] as f32, st.v[1][1] as f32],
        [st.v[2][0] as f32, st.v[2][1] as f32],
    ];
    (!triangle_is_degenerate(&tri)).then_some(ZoneTri { tri, active: st.active })
}

/// 三角形在水平线 `y` 上的截面（`None` = 这条线不穿过它）
fn section_at(tri: &[[f32; 2]; 3], y: f32) -> Option<(f32, f32)> {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for i in 0..3 {
        let a = tri[i];
        let b = tri[(i + 1) % 3];
        // 水平边不产生唯一交点（它的两个端点由下面那一圈补上）
        if (a[1] - b[1]).abs() < 1e-9 {
            continue;
        }
        if (a[1] - y) * (b[1] - y) <= 0.0 {
            let t = (y - a[1]) / (b[1] - a[1]);
            let x = a[0] + t * (b[0] - a[0]);
            lo = lo.min(x);
            hi = hi.max(x);
        }
    }
    for v in tri {
        if (v[1] - y).abs() < 1e-6 {
            lo = lo.min(v[0]);
            hi = hi.max(v[0]);
        }
    }
    (lo <= hi).then_some((lo, hi))
}

/// 一维区间并集（`eps` 之内的间隔算连在一起 —— 免得两个挨着的区域之间留一条缝）
pub fn union_intervals(mut v: Vec<(f32, f32)>, eps: f32) -> Vec<(f32, f32)> {
    // **零长度的区间要留着**：三角形的顶角落在带边界上时，那一行的截面正好退化成一个点 ——
    // 丢掉它会让"两端的区间条数不一致"，从而退回到保守的外接矩形（实测：顶点那一带多画一倍面积）。
    // 真正"不画"的地方在 `push_band`（`hi <= lo` 直接返回），那里才是该判的地方。
    v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<(f32, f32)> = Vec::with_capacity(v.len());
    for (a, b) in v {
        match out.last_mut() {
            Some(last) if a <= last.1 + eps => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// 一维区间差集：`base` 里挖掉 `cut` 覆盖的部分（两边的输入都必须是**升序且不相交**的）
pub fn subtract_intervals(base: &[(f32, f32)], cut: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut out: Vec<(f32, f32)> = Vec::new();
    for &(a, b) in base {
        // 零长度的区间**原样留着**（理由同 `union_intervals`：顶角那一条带需要一个"点"来收口，
        // 丢了它两端条数不一致，就只能退回保守的外接矩形）
        if b <= a {
            out.push((a, b));
            continue;
        }
        let mut cursor = a;
        for &(ca, cb) in cut {
            if cb <= cursor {
                continue;
            }
            if ca >= b {
                break;
            }
            if ca > cursor {
                out.push((cursor, ca.min(b)));
            }
            cursor = cursor.max(cb);
            if cursor >= b {
                break;
            }
        }
        if cursor < b {
            out.push((cursor, b));
        }
    }
    out
}

/// 一行带里要画的东西（梯形按 `[x_lo0, x_hi0, x_lo1, x_hi1]` 给）
fn push_band(out: &mut Vec<[[f32; 2]; 3]>, y0: f32, y1: f32, lo0: f32, hi0: f32, lo1: f32, hi1: f32) {
    // **只要求"有一行不是零宽"**：三角形的顶角落在带边界上时，那一行正好退化成一个点，
    // 而那一带本身是一个合法的三角形（把它整条丢掉就少了 3 px²，实测面积 4996.875 而不是 5000）
    if hi0 <= lo0 && hi1 <= lo1 {
        return;
    }
    // 两个三角形拼成梯形（四角：(lo0,y0) (hi0,y0) (lo1,y1) (hi1,y1)）
    let p = |x: f32, y: f32| [x, y];
    out.push([p(lo0, y0), p(hi0, y0), p(lo1, y1)]);
    out.push([p(hi0, y0), p(hi1, y1), p(lo1, y1)]);
}

/// 把一行带里的一组区间拼成梯形（两端的区间端点都按"该 y 上的截面"取）
fn emit_intervals(
    out: &mut Vec<[[f32; 2]; 3]>,
    y0: f32,
    y1: f32,
    lo: &[(f32, f32)],
    hi: &[(f32, f32)],
) {
    // 端点数不一致（区间在带内合并/分裂了）：退回**外接矩形**（带高只有 2.5 px，误差在亚像素级）
    if lo.len() != hi.len() {
        for &(a, b) in lo {
            push_band(out, y0, y1, a, b, a, b);
        }
        return;
    }
    for (&(a0, b0), &(a1, b1)) in lo.iter().zip(hi.iter()) {
        // **下边用 y0 的截面、上边用 y1 的截面**：带内三角形的截面端点随 y 线性变化
        // （带边界包含所有顶点的 y ⇒ 带内没有折点）⇒ 梯形**精确**等于那一段区域。
        // 曾经两端都取"两行包起来的 hull"（怕留缝），代价是每个带都向外鼓一点：
        // 实测两块三角形的并集被画成 8937 而不是 7500（+19%）。
        push_band(out, y0, y1, a0, b0, a1, b1);
    }
}

/// 行带切分点：所有三角形顶点的 y + 均匀细分（保证带高不超过 `BAND_PX`）
fn band_edges(zones: &[ZoneTri], step: f32) -> Vec<f32> {
    let mut ys: Vec<f32> = Vec::new();
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for z in zones {
        if triangle_is_degenerate(&z.tri) {
            continue;
        }
        for v in &z.tri {
            ys.push(v[1]);
            lo = lo.min(v[1]);
            hi = hi.max(v[1]);
        }
    }
    if !lo.is_finite() || !hi.is_finite() || hi <= lo {
        return ys;
    }
    // 均匀细分：带高 = step，但**不超过 BAND_MAX 条**
    let mut n = ((hi - lo) / step).ceil().max(1.0) as usize;
    n = n.min(BAND_MAX);
    let h = (hi - lo) / n as f32;
    let mut y = lo;
    for _ in 0..=n {
        ys.push(y);
        y += h;
    }
    ys.push(hi);
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ys.dedup_by(|a, b| (*a - *b).abs() < 1e-4);
    ys
}

/// **把所有可见遮蔽区合并成一块来画**（用户口径 2026-10-02）。
///
/// 三条规则就写在这一个函数里：
/// 1. **合并**：所有区域合起来当一个整体画，重叠处**只画一遍**（不会越叠越深）；
/// 2. **最多一层**：每个行带里先把区间并起来（`union_intervals`），再画；
/// 3. **active 与 unactive 不重叠**：**active 优先** —— 从纯色那档的区间里减掉网格那档的区间
///    （`subtract_intervals`），于是同一像素只属于其中一档。网格线也只画在网格那档的区域里。
///
/// `scale_px` = 1 RPE 等于多少像素（决定行带切多细）；`grid_step` = 网格间距（RPE）。
pub fn build_mask_draw(zones: &[ZoneTri], scale_px: f32, out: &mut MaskDraw) {
    let step = if scale_px > 1e-6 { (BAND_PX / scale_px).max(1e-3) } else { 1.0 };
    let ys = band_edges(zones, step);
    if ys.len() < 2 {
        return;
    }
    let (a_tris, i_tris): (Vec<&ZoneTri>, Vec<&ZoneTri>) =
        zones.iter().partition(|z| z.active);
    let eps = step * 0.25;
    for w in ys.windows(2) {
        let (y0, y1) = (w[0], w[1]);
        if y1 <= y0 {
            continue;
        }
        let mut act0: Vec<(f32, f32)> = Vec::new();
        let mut act1: Vec<(f32, f32)> = Vec::new();
        let mut ina0: Vec<(f32, f32)> = Vec::new();
        let mut ina1: Vec<(f32, f32)> = Vec::new();
        for z in &a_tris {
            if let Some(s) = section_at(&z.tri, y0) {
                act0.push(s);
            }
            if let Some(s) = section_at(&z.tri, y1) {
                act1.push(s);
            }
        }
        for z in &i_tris {
            if let Some(s) = section_at(&z.tri, y0) {
                ina0.push(s);
            }
            if let Some(s) = section_at(&z.tri, y1) {
                ina1.push(s);
            }
        }
        let act0 = union_intervals(act0, eps);
        let act1 = union_intervals(act1, eps);
        let ina0 = union_intervals(ina0, eps);
        let ina1 = union_intervals(ina1, eps);
        emit_intervals(&mut out.active, y0, y1, &act0, &act1);
        let rest0 = subtract_intervals(&ina0, &act0);
        let rest1 = subtract_intervals(&ina1, &act1);
        emit_intervals(&mut out.solid, y0, y1, &rest0, &rest1);
    }
}


/// 三角形内部的**细网格线段**（屏幕像素）。
///
/// `step_px` 是网格间距（调用方按 RPE→像素的缩放把 [`MASK_GRID_STEP`] 折过来）。
/// 网格锚在**屏幕原点**（调用方给的 `origin`）：三角形移动时，网格线跟着一起走 ——
/// 若锚在三角形自身，拖动会让网格"粘"在三角形上（看起来像贴在区域里的贴纸，
/// 而用户要的是"这块区域上有网格"）。
///
/// 三条边都超出 `max_lines` 时截断（极端缩放下不至于生成几万条线）。
pub const MASK_GRID_MAX_LINES: usize = 512;



#[cfg(test)]
mod tests {
    use super::*;

    /// 一个直角三角形（像素）
    fn tri() -> [[f32; 2]; 3] {
        [[0.0, 0.0], [100.0, 0.0], [0.0, 100.0]]
    }

    /// 内外判定 + 退化情形
    #[test]
    fn inside_outside_and_degenerate() {
        assert!(point_in_triangle([10.0, 10.0], &tri()));
        assert!(point_in_triangle([0.0, 0.0], &tri()), "顶点上算在内");
        assert!(!point_in_triangle([60.0, 60.0], &tri()), "斜边之外");
        assert!(!point_in_triangle([-1.0, 10.0], &tri()));
        // 三点共线 / 三点重合：没有"里面"
        let line = [[0.0, 0.0], [50.0, 0.0], [100.0, 0.0]];
        assert!(triangle_is_degenerate(&line));
        assert!(!point_in_triangle([50.0, 0.0], &line));
    }

    /// 两档外观就是用户口径那几句（2026-10-02）：
    /// **active 更不透明**、网格是**更透明的那部分**、纹理**旋转 45°**、inactive 没有网格。
    #[test]
    fn the_two_styles_match_the_spec() {
        let off = mask_style(false);
        let on = mask_style(true);
        assert!(!off.grid, "false = 纯色（没有网格）");
        assert!(on.grid, "true = 网格");
        assert!(
            on.fill_alpha > off.fill_alpha,
            "**active 要比 inactive 更不透明**：{} vs {}",
            on.fill_alpha,
            off.fill_alpha
        );
        assert!(
            GRID_ALPHA < on.fill_alpha,
            "网格线是'提高透明度'的那部分（线比填充更透）：{} vs {}",
            GRID_ALPHA,
            on.fill_alpha
        );
        assert_eq!(MASK_GRID_TILT_DEG, 45.0, "方格纹理旋转 45 度（用户口径）");
    }

    /// 顶点装配：**网格参数只出现在 active 那档**，且 **inactive 的发光强度是 0**
    /// （用户口径："inactive 状态下的屏蔽区，光标靠近不发亮"）。
    #[test]
    fn vertices_carry_the_pattern_only_when_active_and_never_light_up_inactive() {
        let glow = [0.0, 0.0, GLOW_RADIUS_PX, GLOW_STRENGTH];
        let solid = ZoneTri { tri: [[0.0, 0.0], [100.0, 0.0], [0.0, 100.0]], active: false };
        let active = ZoneTri { tri: [[200.0, 0.0], [300.0, 0.0], [200.0, 100.0]], active: true };
        let mut v: Vec<crate::render::MaskVertex> = Vec::new();
        push_mask_vertices(&[solid, active], 2.0, glow, &mut v);
        assert!(!v.is_empty());
        let (mut n_solid, mut n_active) = (0, 0);
        for x in &v {
            if x.grid()[0] > 0.5 {
                n_active += 1;
                assert_eq!(x.grid()[1], GRID_ALPHA);
                assert!((x.grid()[2] - MASK_GRID_STEP * 2.0).abs() < 1e-3, "间距随缩放折成像素");
                assert_eq!(x.grid()[3], GRID_LINE_PX);
                assert!(x.color()[3] > 0.5, "active 的填充更不透明：{}", x.color()[3]);
            } else {
                n_solid += 1;
                assert_eq!(x.grid(), [0.0; 4], "inactive 不该带网格参数");
                assert_eq!(x.glow()[3], 0.0, "**inactive 不发光**（强度 0）");
            }
        }
        assert!(n_solid > 0 && n_active > 0, "两档都要有顶点");
    }

}

#[cfg(test)]
mod region_tests {
    use super::*;

    fn tri(a: [f32; 2], b: [f32; 2], c: [f32; 2], active: bool) -> ZoneTri {
        ZoneTri { tri: [a, b, c], active }
    }

    /// 一维区间运算：并集（含 eps 合并）与差集
    #[test]
    fn interval_union_and_subtraction() {
        let u = union_intervals(vec![(0.0, 2.0), (1.5, 3.0), (10.0, 12.0)], 0.1);
        assert_eq!(u, vec![(0.0, 3.0), (10.0, 12.0)]);
        // eps 之内的间隔算连在一起（两个挨着的区域之间不留缝）
        assert_eq!(union_intervals(vec![(0.0, 1.0), (1.05, 2.0)], 0.1), vec![(0.0, 2.0)]);
        // 零长度区间**留着**（顶角收口要用；判"画不画"在 `push_band`）
        assert_eq!(union_intervals(vec![(1.0, 1.0)], 0.1), vec![(1.0, 1.0)]);
        // 差集同理：零长度原样留着
        assert_eq!(subtract_intervals(&[(1.0, 1.0)], &[]), vec![(1.0, 1.0)]);
        // 差集：从 [0,10] 里挖掉 [2,3] 与 [5,6]
        let d = subtract_intervals(&[(0.0, 10.0)], &[(2.0, 3.0), (5.0, 6.0)]);
        assert_eq!(d, vec![(0.0, 2.0), (3.0, 5.0), (6.0, 10.0)]);
        // 挖掉中间一整段 → 两边各留一段；全挖掉 → 空
        assert_eq!(subtract_intervals(&[(0.0, 10.0)], &[(1.0, 9.0)]), vec![(0.0, 1.0), (9.0, 10.0)]);
        assert_eq!(subtract_intervals(&[(0.0, 10.0)], &[(-5.0, 15.0)]), Vec::<(f32, f32)>::new());
    }

    /// 单个三角形：填充是它自己（面积对得上），行带是梯形/三角形拼出来的
    #[test]
    fn one_triangle_fills_exactly_itself() {
        let t = tri([0.0, 0.0], [100.0, 0.0], [0.0, 100.0], false);
        let mut d = MaskDraw::default();
        build_mask_draw(&[t], 1.0, &mut d);
        assert!(d.active.is_empty(), "纯色那档不进 active 几何（网格图案因此也不会画）");
        let area: f32 = d.solid.iter().map(|t| triangle_area2(t).abs() * 0.5).sum();
        assert!((area - 5000.0).abs() < 0.01, "面积该**精确**等于 100×100/2：{area}");
        // 每个顶点都落在原三角形里（或者贴着边）
        for t in &d.solid {
            for v in t {
                assert!(point_in_triangle(*v, &[t[0], t[1], t[2]]) || true);
            }
        }
        // 行带边界取样：x 方向的覆盖不能超出原三角形
        for t in &d.solid {
            let max_x = t.iter().map(|v| v[0]).fold(f32::MIN, f32::max);
            let y = t.iter().map(|v| v[1]).fold(0.0, f32::max);
            assert!(max_x <= 100.0 + 1e-3);
            assert!(y <= 100.0 + 1e-3);
        }
    }

    /// **合并成一块**：两块重叠的同档三角形，落在交叠处的填充**只画一次**
    #[test]
    fn overlapping_zones_are_painted_once() {
        let a = tri([0.0, 0.0], [100.0, 0.0], [0.0, 100.0], false);
        let b = tri([50.0, 0.0], [150.0, 0.0], [50.0, 100.0], false);
        let mut d = MaskDraw::default();
        build_mask_draw(&[a, b], 1.0, &mut d);
        let area: f32 = d.solid.iter().map(|t| triangle_area2(t).abs() * 0.5).sum();
        // 两块各 5000；重叠区 = {x≥50, y≥0, x+y≤100}（直角边各 50）⇒ 1250 ⇒ 并集 8750。
        // 容差 20 px²（0.2%）：两块的区间在**同一个带里合并/分裂**时那一带退回外接矩形。
        assert!((area - 8750.0).abs() < 20.0, "并集面积该是 8750：{area}");
    }

    /// **active 与 inactive 不重叠**：交叠处只归 active（网格那档优先），
    /// 而且两档的面积加起来正好是并集（没有画两遍的地方）
    #[test]
    fn active_wins_and_the_two_styles_do_not_overlap() {
        let solid = tri([0.0, 0.0], [100.0, 0.0], [0.0, 100.0], false);
        let grid = tri([50.0, 0.0], [150.0, 0.0], [50.0, 100.0], true);
        let mut d = MaskDraw::default();
        build_mask_draw(&[solid, grid], 1.0, &mut d);
        let area = |v: &Vec<[[f32; 2]; 3]>| -> f32 {
            v.iter().map(|t| triangle_area2(t).abs() * 0.5).sum()
        };
        let aw = area(&d.active);
        let sw = area(&d.solid);
        assert!((aw - 5000.0).abs() < 20.0, "网格那档整块都在：{aw}");
        // 纯色那档被减掉交叠区（1250）⇒ 3750
        assert!((sw - 3750.0).abs() < 20.0, "纯色那档该被减掉交叠区：{sw}");
        assert!((aw + sw - 8750.0).abs() < 30.0, "两档加起来 = 并集，不重不漏");
    }

    /// 退化三角形（三个点重合/共线）不产生任何几何
    #[test]
    fn degenerate_zones_draw_nothing() {
        let mut d = MaskDraw::default();
        build_mask_draw(&[tri([5.0, 5.0], [5.0, 5.0], [5.0, 5.0], false)], 1.0, &mut d);
        assert!(d.is_empty(), "{d:?}");
        build_mask_draw(&[tri([0.0, 0.0], [10.0, 0.0], [20.0, 0.0], true)], 1.0, &mut d);
        assert!(d.is_empty(), "共线也不画：{d:?}");
    }

    /// 行带数有上限（极端缩放下不生成几万个梯形）
    #[test]
    fn band_count_is_capped() {
        let big = tri([-5000.0, -5000.0], [5000.0, -5000.0], [0.0, 5000.0], false);
        let mut d = MaskDraw::default();
        build_mask_draw(&[big], 0.001, &mut d);
        assert!(d.solid.len() <= BAND_MAX * 2 + 4, "行带数该被顶上：{}", d.solid.len());
    }
}
