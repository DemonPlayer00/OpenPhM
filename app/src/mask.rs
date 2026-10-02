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

/// 遮蔽区的一档外观（用户口径：`active=false` = 纯色、不透明度更高；
/// `active=true` = 不透明度更低 + 细网格线条）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaskStyle {
    /// 填充的不透明度 0~1
    pub fill_alpha: f32,
    /// 是否画细网格
    pub grid: bool,
}

/// 两种外观的**唯一定义**（预览、编辑区、无头出图都用它，免得三处各调一次参数）。
///
/// 依据用户口径：false 是"屏蔽区透明度较高且纯色"，true 是"透明度变低且绘制细网格线条"。
/// 数值本身是观感取舍：纯色那档要挡住底下的音符（0.42），网格那档要能看清底下（0.18）。
pub const MASK_FILL_SOLID: f32 = 0.42;
pub const MASK_FILL_GRID: f32 = 0.18;

pub fn mask_style(active: bool) -> MaskStyle {
    if active {
        MaskStyle { fill_alpha: MASK_FILL_GRID, grid: true }
    } else {
        MaskStyle { fill_alpha: MASK_FILL_SOLID, grid: false }
    }
}

/// 遮蔽区的底色（红）。用**同一份**红：预览的填充、轮廓、编辑区里那一列的高亮。
pub const MASK_RED: [u8; 3] = [232, 64, 72];
/// 细网格线的间距（**RPE 单位**；窗口高 900 ÷ 16 = 56.25 —— 视觉上是"细网格"，
/// 又不至于密到糊成一片。调用方按当前缩放折成像素）
pub const MASK_GRID_STEP: f32 = 56.25;

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

/// 点到**线段**的最短距离
fn dist_point_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let v = [b[0] - a[0], b[1] - a[1]];
    let w = [p[0] - a[0], p[1] - a[1]];
    let vv = v[0] * v[0] + v[1] * v[1];
    if vv <= 1e-9 {
        return (w[0] * w[0] + w[1] * w[1]).sqrt();
    }
    let t = ((w[0] * v[0] + w[1] * v[1]) / vv).clamp(0.0, 1.0);
    let q = [a[0] + v[0] * t, a[1] + v[1] * t];
    ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2)).sqrt()
}

/// 点到三角形的最短距离（**在里面是 0**）—— "靠近鼠标的遮蔽区发光"的判据。
pub fn point_triangle_distance(p: [f32; 2], tri: &[[f32; 2]; 3]) -> f32 {
    if point_in_triangle(p, tri) {
        return 0.0;
    }
    (0..3)
        .map(|i| dist_point_segment(p, tri[i], tri[(i + 1) % 3]))
        .fold(f32::INFINITY, f32::min)
}

/// 指针离边缘多远之内算"靠近"（像素）—— 曲线之外的唯一常数
pub const MASK_GLOW_PX: f32 = 44.0;

/// **发光强度** 0~1：指针在区域内 ⇒ 1；在 `MASK_GLOW_PX` 之外 ⇒ 0；中间线性。
///
/// 平方衰减（更"软"）而不是线性：线性在边界上是一道看得见的折角。
pub fn mask_glow(dist_px: f32) -> f32 {
    if dist_px.is_nan() {
        return 0.0; // 说不清多远 ⇒ 不发光（宁可少亮一次，也别让 NaN 传进颜色里）
    }
    if dist_px <= 0.0 {
        return 1.0;
    }
    if dist_px >= MASK_GLOW_PX {
        return 0.0;
    }
    let t = 1.0 - dist_px / MASK_GLOW_PX;
    t * t
}

/// 用**半平面**裁剪一条线段到凸多边形（这里是三角形）。
///
/// 返回裁剪后的两个端点（`None` = 整条线段在外面）。用 Cyrus–Beck 的等价形式：
/// 逐条边把参数区间 `[t0, t1]` 收紧；退化多边形直接给 `None`。
///
/// 为什么需要它：三角形里的"细网格"是**直线**，而 egui 只能按矩形裁剪 ——
/// 按外接矩形裁会在三角形外面留下网格线（那看起来像溢出的 bug）。
pub fn clip_segment_to_triangle(
    p0: [f32; 2],
    p1: [f32; 2],
    tri: &[[f32; 2]; 3],
) -> Option<[[f32; 2]; 2]> {
    if triangle_is_degenerate(tri) {
        return None;
    }
    let s = triangle_area2(tri).signum();
    let d = [p1[0] - p0[0], p1[1] - p0[1]];
    let mut t0 = 0.0f32;
    let mut t1 = 1.0f32;
    for i in 0..3 {
        let a = tri[i];
        let b = tri[(i + 1) % 3];
        // 内侧 = cross(b-a, p-a) 与三角形定向同号
        let e = [b[0] - a[0], b[1] - a[1]];
        let num = s * ((e[0] * (p0[1] - a[1])) - (e[1] * (p0[0] - a[0])));
        let den = s * ((e[0] * d[1]) - (e[1] * d[0]));
        if den.abs() < 1e-9 {
            if num < -1e-6 {
                return None; // 平行且在边外
            }
            continue;
        }
        let t = -num / den;
        if den > 0.0 {
            // 进入
            if t > t0 {
                t0 = t;
            }
        } else if t < t1 {
            t1 = t;
        }
        if t0 > t1 {
            return None;
        }
    }
    let at = |t: f32| [p0[0] + d[0] * t, p0[1] + d[1] * t];
    Some([at(t0), at(t1)])
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

/// 裁剪之后还值得画吗（**退化成点的线段一律丢**）：网格线正好落在三角形的顶点上时，
/// 裁剪结果是"两个端点重合"——画它等于画一个点，还会把网格线的条数算多。
fn segment_is_drawable(s: &[[f32; 2]; 2]) -> bool {
    let dx = s[1][0] - s[0][0];
    let dy = s[1][1] - s[0][1];
    (dx * dx + dy * dy).sqrt() >= 0.5
}

pub fn mask_grid_lines(tri: &[[f32; 2]; 3], step_px: f32, origin: [f32; 2]) -> Vec<[[f32; 2]; 2]> {
    let mut out: Vec<[[f32; 2]; 2]> = Vec::new();
    if triangle_is_degenerate(tri) || !(step_px > 1.0) {
        return out;
    }
    let (mut minx, mut maxx, mut miny, mut maxy) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for v in tri {
        minx = minx.min(v[0]);
        maxx = maxx.max(v[0]);
        miny = miny.min(v[1]);
        maxy = maxy.max(v[1]);
    }
    // 竖线
    let k0 = ((minx - origin[0]) / step_px).floor() as i32;
    let k1 = ((maxx - origin[0]) / step_px).ceil() as i32;
    for k in k0..=k1 {
        if out.len() >= MASK_GRID_MAX_LINES {
            return out;
        }
        let x = origin[0] + k as f32 * step_px;
        if let Some(s) = clip_segment_to_triangle([x, miny - 1.0], [x, maxy + 1.0], tri) {
            if segment_is_drawable(&s) {
                out.push(s);
            }
        }
    }
    // 横线
    let k0 = ((miny - origin[1]) / step_px).floor() as i32;
    let k1 = ((maxy - origin[1]) / step_px).ceil() as i32;
    for k in k0..=k1 {
        if out.len() >= MASK_GRID_MAX_LINES {
            return out;
        }
        let y = origin[1] + k as f32 * step_px;
        if let Some(s) = clip_segment_to_triangle([minx - 1.0, y], [maxx + 1.0, y], tri) {
            if segment_is_drawable(&s) {
                out.push(s);
            }
        }
    }
    out
}

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
        assert!(clip_segment_to_triangle([0.0, 0.0], [10.0, 10.0], &line).is_none());
    }

    /// 距离：里面 0、边上是 0、外面按垂足算
    #[test]
    fn distance_is_zero_inside_and_euclidean_outside() {
        assert_eq!(point_triangle_distance([10.0, 10.0], &tri()), 0.0);
        assert!(point_triangle_distance([50.0, 0.0], &tri()) < 1e-3, "边上");
        // 点在 (200, 0)：最近的是 x 轴那一段的端点 (100,0) ⇒ 100
        assert!((point_triangle_distance([200.0, 0.0], &tri()) - 100.0).abs() < 1e-3);
        // 斜边外侧的垂足在斜边中点：|(100,100)-(50,50)| = 70.71
        let d = point_triangle_distance([100.0, 100.0], &tri());
        assert!((d - 70.7107).abs() < 0.01, "{d}");
    }

    /// 发光：里面最亮、越远越暗、超过阈值归零（且是**平方**衰减）
    #[test]
    fn glow_fades_with_distance() {
        assert_eq!(mask_glow(0.0), 1.0);
        assert_eq!(mask_glow(-5.0), 1.0, "在里面（调用方给 0 或负数）都算最亮");
        assert!((mask_glow(MASK_GLOW_PX * 0.5) - 0.25).abs() < 1e-6, "一半距离 = 1/4 亮度");
        assert_eq!(mask_glow(MASK_GLOW_PX), 0.0);
        assert_eq!(mask_glow(1000.0), 0.0);
        assert_eq!(mask_glow(f32::INFINITY), 0.0);
        assert_eq!(mask_glow(f32::NAN), 0.0, "说不清多远 ⇒ 不发光");
    }

    /// 两档外观就是用户口径那两句
    #[test]
    fn the_two_styles_match_the_spec() {
        let off = mask_style(false);
        let on = mask_style(true);
        assert!(!off.grid, "false = 纯色");
        assert!(on.grid, "true = 细网格");
        assert!(off.fill_alpha > on.fill_alpha, "false 更不透明（透明度更低）");
    }

    /// 线段裁剪：穿过、完全在内、完全在外、刚好切角
    #[test]
    fn clipping_a_segment_to_the_triangle() {
        // 完全在内
        let s = clip_segment_to_triangle([10.0, 10.0], [20.0, 10.0], &tri()).expect("在内");
        assert!((s[0][0] - 10.0).abs() < 1e-3 && (s[1][0] - 20.0).abs() < 1e-3);
        // 横穿：从 (-50, 10) 到 (500, 10) ⇒ 只保留 x ∈ [0, 90]（斜边 x + y = 100）
        let s = clip_segment_to_triangle([-50.0, 10.0], [500.0, 10.0], &tri()).expect("横穿");
        assert!((s[0][0] - 0.0).abs() < 1e-3, "{s:?}");
        assert!((s[1][0] - 90.0).abs() < 1e-3, "{s:?}");
        // 完全在外
        assert!(clip_segment_to_triangle([200.0, 200.0], [300.0, 300.0], &tri()).is_none());
        // 竖线贴着 x=0：只保留三角形内的那一段（y 从 0 到 10 —— y<0 在边外）
        let s = clip_segment_to_triangle([0.0, -10.0], [0.0, 10.0], &tri()).expect("贴边竖线");
        assert!(s[0][1].abs() < 1e-3 && (s[1][1] - 10.0).abs() < 1e-3, "{s:?}");
        // 与边平行且在外侧：没有交集
        assert!(clip_segment_to_triangle([110.0, -10.0], [110.0, 10.0], &tri()).is_none());
    }

    /// 网格线：全在三角形里（裁剪生效）、间距正确、锚在给定原点上
    #[test]
    fn grid_lines_are_clipped_and_anchored() {
        let lines = mask_grid_lines(&tri(), 25.0, [0.0, 0.0]);
        assert!(!lines.is_empty());
        for l in &lines {
            // 两端点都必须落在三角形内（裁剪过的）
            assert!(point_in_triangle(l[0], &tri()), "端点跑出去了：{l:?}");
            assert!(point_in_triangle(l[1], &tri()), "端点跑出去了：{l:?}");
        }
        // x = 0 / 25 / 50 / 75 与 y = 0 / 25 / 50 / 75 ⇒ 竖线 4 条 + 横线 4 条（x=100、y=100 退化成点）
        let vertical = lines.iter().filter(|l| (l[0][0] - l[1][0]).abs() < 1e-3).count();
        let horizontal = lines.iter().filter(|l| (l[0][1] - l[1][1]).abs() < 1e-3).count();
        assert_eq!((vertical, horizontal), (4, 4), "{lines:?}");
        // 锚点平移 ⇒ 网格线整体平移
        let shifted = mask_grid_lines(&tri(), 25.0, [5.0, 5.0]);
        assert!(shifted.iter().any(|l| (l[0][0] - l[1][0]).abs() < 1e-3 && (l[0][0] - 5.0).abs() < 1e-3));
        // 间距过小 / 退化三角形：不生成（否则会画出几万条线）
        assert!(mask_grid_lines(&tri(), 0.5, [0.0, 0.0]).is_empty());
        assert!(mask_grid_lines(&[[0.0, 0.0]; 3], 25.0, [0.0, 0.0]).is_empty());
        // 上限生效
        let many = mask_grid_lines(&[[0.0, 0.0], [4000.0, 0.0], [0.0, 4000.0]], 1.5, [0.0, 0.0]);
        assert!(many.len() <= MASK_GRID_MAX_LINES);
    }
}
