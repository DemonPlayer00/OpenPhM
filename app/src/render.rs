//! 演奏区渲染：自研 wgpu 实例化管线，经 `egui_wgpu::CallbackTrait` 挂进 egui 的同一个 render pass。
//!
//! 与 S1b spike 的关键差别：**用回调矩形的 viewport 把演奏区映射到自己的坐标空间**，
//! 而不是"铺满全屏再被裁剪"。这样演奏区在面板布局变化、分数缩放（fractional DPI）下都能正确对齐。
//!
//! 坐标约定（与 `spec/opm-format.md` 第 3 节一致）：
//!   · RPE 坐标系：x ∈ [-675, 675]，y ∈ [-450, 450]，原点在演奏区中心
//!   · 等比缩放（letterbox）：scale_px = min(viewport_w / 1350, viewport_h / 900)
//!   · 时间轴向上：y = RPE 的 floor position 差 × 音符 speed（流速 10 = 1× = 1200 单位/秒）

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::perf;
use crate::state::{Chart, EditorState, Note, NoteKind, RPE_WINDOW_HALF_H, RPE_WINDOW_HALF_W};

pub const RPE_W: f32 = 1350.0;
pub const RPE_H: f32 = 900.0;

/// 每个 RPE 单位对应多少物理像素（与着色器里的 `scale_px` 必须一致）
pub fn rpe_scale(viewport_px: [f32; 2]) -> f32 {
    (viewport_px[0] / RPE_W).min(viewport_px[1] / RPE_H).max(0.0001)
}

/// 演奏区四角 + 中心的标记点（RPE 单位），用于**对齐自检**：
/// 同一坐标既由自研管线画出，也由 egui 画笔标出，两者重合即映射正确。
pub const MARKERS: [(f32, f32, [f32; 4]); 5] = [
    (-RPE_W * 0.5, RPE_H * 0.5, [1.0, 0.0, 1.0, 1.0]),
    (RPE_W * 0.5, RPE_H * 0.5, [1.0, 0.0, 1.0, 1.0]),
    (-RPE_W * 0.5, -RPE_H * 0.5, [1.0, 0.0, 1.0, 1.0]),
    (RPE_W * 0.5, -RPE_H * 0.5, [1.0, 0.0, 1.0, 1.0]),
    (0.0, 0.0, [0.0, 1.0, 0.0, 1.0]),
];

pub fn push_alignment_markers(out: &mut Vec<NoteInstance>) {
    for (x, y, color) in MARKERS {
        out.push(NoteInstance::new([x, y], [9.0, 9.0], color, 0.0));
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    viewport_px: [f32; 2],
    scale_px: f32,
    _pad: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct NoteInstance {
    /// 中心（RPE 单位）
    center: [f32; 2],
    /// 半宽半高（RPE 单位）
    half: [f32; 2],
    color: [f32; 4],
    /// 绕中心的旋转角（弧度）——判定线旋转时，线与它下面的音符要一起转
    angle: f32,
}

impl NoteInstance {
    /// 造一个实例（`angle` 为弧度）
    pub fn new(center: [f32; 2], half: [f32; 2], color: [f32; 4], angle: f32) -> Self {
        Self { center, half, color, angle }
    }
    // 只读访问器：测试要断言"子音符跟着判定线变换"（几何断言比读图更可靠）
    pub fn center(&self) -> [f32; 2] {
        self.center
    }
    pub fn half(&self) -> [f32; 2] {
        self.half
    }
    pub fn color(&self) -> [f32; 4] {
        self.color
    }
    pub fn angle(&self) -> f32 {
        self.angle
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    corner: [f32; 2],
}

const QUAD: [Vertex; 4] = [
    Vertex { corner: [-1.0, -1.0] },
    Vertex { corner: [1.0, -1.0] },
    Vertex { corner: [-1.0, 1.0] },
    Vertex { corner: [1.0, 1.0] },
];

pub struct Playfield {
    pipeline: wgpu::RenderPipeline,
    vertex_buf: wgpu::Buffer,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instance_buf: wgpu::Buffer,
    capacity: u32,
    /// 全屏物理尺寸，用于画完自己的 viewport 后恢复
    screen_px: [f32; 2],
}

/// 每帧传进回调的载荷
pub struct PlayfieldFrame {
    pub instances: Vec<NoteInstance>,
    /// 演奏区矩形（物理像素）
    pub viewport_px: [f32; 2],
    /// 绘制耗时（GPU 提交前的 CPU 录制时间）
    pub paint_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Playfield {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("playfield-quad"),
            contents: bytemuck::cast_slice(&QUAD),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("playfield-uniform"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("playfield-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("playfield-bg"),
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("playfield-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("playfield-pl"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });

        let vertex_layouts = [
            Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Vertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![0 => Float32x2],
            }),
            Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<NoteInstance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &wgpu::vertex_attr_array![
                    1 => Float32x2, // center
                    2 => Float32x2, // half
                    3 => Float32x4, // color
                    4 => Float32,   // angle（弧度）
                ],
            }),
        ];

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("playfield-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &vertex_layouts,
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        const INITIAL: u32 = 4096;
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("playfield-instances"),
            size: (INITIAL as u64) * (std::mem::size_of::<NoteInstance>() as u64),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            vertex_buf,
            uniform_buf,
            bind_group,
            instance_buf,
            capacity: INITIAL,
            screen_px: [1.0, 1.0],
        }
    }
}

impl Playfield {
    /// 上传 uniform 与实例数据（不足时扩容）。返回可绘制实例数。
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport_px: [f32; 2],
        instances: &[NoteInstance],
    ) -> u32 {
        let scale = rpe_scale(viewport_px);
        let uniforms = Uniforms {
            viewport_px,
            scale_px: scale,
            _pad: 0.0,
        };
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        let need = instances.len() as u32;
        if need > self.capacity {
            let new_cap = need.next_power_of_two();
            self.instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("playfield-instances"),
                size: (new_cap as u64) * (std::mem::size_of::<NoteInstance>() as u64),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.capacity = new_cap;
        }
        if need > 0 {
            queue.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(instances));
        }
        need.min(self.capacity)
    }

    /// 在当前 render pass 里画。**不设置 viewport** —— 由调用方决定坐标空间。
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>, count: u32, _viewport_px: [f32; 2]) {
        if count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.vertex_buf.slice(..));
        pass.set_vertex_buffer(1, self.instance_buf.slice(..));
        pass.draw(0..4, 0..count);
    }
}

impl egui_wgpu::CallbackTrait for PlayfieldFrame {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(pf) = resources.get_mut::<Playfield>() else {
            return Vec::new();
        };
        pf.screen_px = [
            screen_descriptor.size_in_pixels[0] as f32,
            screen_descriptor.size_in_pixels[1] as f32,
        ];

        pf.upload(device, queue, self.viewport_px, &self.instances);
        Vec::new()
    }

    fn paint(
        &self,
        info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(pf) = resources.get::<Playfield>() else { return };
        let t0 = std::time::Instant::now();

        // 关键：把 viewport 设成回调矩形 —— 演奏区由此获得自己的坐标空间
        let vp = info.viewport_in_pixels();
        if vp.width_px <= 0 || vp.height_px <= 0 {
            return;
        }
        pass.set_viewport(
            vp.left_px as f32,
            vp.top_px as f32,
            vp.width_px as f32,
            vp.height_px as f32,
            0.0,
            1.0,
        );
        pass.set_pipeline(&pf.pipeline);
        pass.set_bind_group(0, &pf.bind_group, &[]);
        pass.set_vertex_buffer(0, pf.vertex_buf.slice(..));
        pass.set_vertex_buffer(1, pf.instance_buf.slice(..));
        let n = (self.instances.len() as u32).min(pf.capacity);
        if n > 0 {
            pass.draw(0..4, 0..n);
        }

        // 恢复全屏 viewport，避免影响同一个 pass 里后续的 egui 绘制
        pass.set_viewport(0.0, 0.0, pf.screen_px[0], pf.screen_px[1], 0.0, 1.0);

        self.paint_ms.store(
            t0.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

/// 窗口边界框：**RPE 的 ±675 × ±450**（1350×900，原点在中心）。
///
/// 预览区是等比 letterbox 的，所以窗口边界不一定顶到预览区边缘 —— 必须画出来，
/// 否则"哪里是游戏画面"只能靠猜。4 条边 + 8 个角标（角标粗一点，方便一眼定位边界）。
/// 边界外压暗的默认**观感**目标（名义 alpha，非线性目标下即为观感值）
pub const DIM_ALPHA_DEFAULT: f32 = 0.42;

/// 按渲染目标的色彩空间换算压暗 alpha。
///
/// 若 `target_format` 是 sRGB，硬件会把目标解码到线性空间混合、再编码回去，
/// 于是**同样的名义 alpha 观感会弱很多**：实测名义 0.42 在暗背景上只得到 0.78 的观感
/// （56 → 42）。这里把名义值按 sRGB 传输函数反算成等效 alpha，让 GUI（目标 `Rgba8Unorm`，
/// 混合发生在存储空间）与无头出图（`Rgba8UnormSrgb`）**尽量**一致。
///
/// 注意这是**近似**：sRGB 不是纯幂函数（有 +0.055 的线性段），因此不存在一个对所有亮度都
/// 精确相等的 alpha —— 暗背景上换算后观感约 0.48、中灰约 0.53（目标 0.58）。跨色彩空间
/// 的精确一致做不到，能保证的是"亮的东西在窗口外一定明显更暗"。
pub fn dim_alpha_for(perceived: f32, target_format: wgpu::TextureFormat) -> f32 {
    if target_format.is_srgb() {
        1.0 - (1.0 - perceived).powf(2.4)
    } else {
        perceived
    }
}

/// 边界**之外**的压暗带。画在内容**之上**：屏幕外的东西变暗但仍看得见 ——
/// "音符跑到窗口外了"是个该被发现的错误，不该被藏起来，也不该与窗口内的东西一样亮。
///
/// 用实例（而不是 egui 画笔）是为了让 GUI 与无头出图共用同一份几何：两套实现必然漂移，
/// 而"边界在哪"必须两边一致。带宽取远大于任何视口（16:9 letterbox 下视口 RPE 宽度 < 2400）。
pub fn push_window_dim(out: &mut Vec<NoteInstance>, alpha: f32) {
    use crate::state::{RPE_WINDOW_HALF_H as HALF_H, RPE_WINDOW_HALF_W as HALF_W};
    let c_dim = [0.0, 0.0, 0.0, alpha];
    const BIG: f32 = 4000.0;
    const BAND: f32 = 3000.0;
    // 上下两条带：横向铺满（BIG），纵向从 ±HALF_H 往外延伸
    for sy in [-1.0_f32, 1.0] {
        out.push(NoteInstance::new(
            [0.0, sy * (HALF_H + BAND * 0.5)],
            [BIG, BAND * 0.5],
            c_dim,
            0.0,
        ));
    }
    // 左右两条带：纵向只覆盖窗口高度（避免与上下带在角上重复叠加出更深的块）
    for sx in [-1.0_f32, 1.0] {
        out.push(NoteInstance::new(
            [sx * (HALF_W + BAND * 0.5), 0.0],
            [BAND * 0.5, HALF_H],
            c_dim,
            0.0,
        ));
    }
}

/// 窗口边框：4 条边 + 8 个角标（RPE 的 ±675 × ±450）。画在最上层，保证边界清晰可读。
pub fn push_window_frame(out: &mut Vec<NoteInstance>, alpha: f32) {
    use crate::state::{RPE_WINDOW_HALF_H as HALF_H, RPE_WINDOW_HALF_W as HALF_W};
    let edge = 2.0_f32; // 边线半厚（RPE 单位）
    let tick = 6.0_f32; // 角标半厚
    let tick_len = 70.0_f32; // 角标长度
    let c_edge = [0.42, 0.48, 0.60, 0.75 * alpha];
    let c_tick = [0.62, 0.72, 0.95, 0.95 * alpha];
    // 上下两条**水平**边：y = ±450（半高），x 方向铺满窗宽
    for y in [-HALF_H, HALF_H] {
        out.push(NoteInstance::new([0.0, y], [HALF_W, edge], c_edge, 0.0));
    }
    // 左右两条**竖直**边：x = ±675（半宽），y 方向只覆盖窗高
    for x in [-HALF_W, HALF_W] {
        out.push(NoteInstance::new([x, 0.0], [edge, HALF_H], c_edge, 0.0));
    }
    // 8 个角标：每个角一条横、一条竖（"边界正好在这里"要一眼可见）
    for sx in [-1.0_f32, 1.0] {
        for sy in [-1.0_f32, 1.0] {
            out.push(NoteInstance::new(
                [sx * (HALF_W - tick_len * 0.5), sy * HALF_H],
                [tick_len * 0.5, tick],
                c_tick,
                0.0,
            ));
            out.push(NoteInstance::new(
                [sx * HALF_W, sy * (HALF_H - tick_len * 0.5)],
                [tick, tick_len * 0.5],
                c_tick,
                0.0,
            ));
        }
    }
}

// ---------------------------------------------------------------- 击中效果

/// 音符到达判定线之后的**收缩消失**时长（秒）。
///
/// 游戏里是"到线即消失 + 闪光"；给一小段收缩只是让"消失"看起来发生在一个瞬间里，
/// 而不是凭空少了一个方块。
pub const HIT_FADE_SEC: f64 = 0.06;

/// **击中效果**的持续时间（秒）：白闪 + 一圈向外扩散的方框。
pub const HIT_FX_SEC: f64 = 0.22;

/// hold 在按住期间每隔几拍再播一次击中效果（用户要求：每 3 拍 1 次）
pub const HOLD_PULSE_BEATS: f64 = 3.0;

/// 这颗音符**此刻**有没有击中效果在场？有就给出它的进度 `0..1`（`0` = 刚击中）。
///
/// 该在哪些时刻闪：
/// · `k = 0` —— 音符的**打击时刻**。hold 也一样：**击中后立即播一次**（用户明确要求，
///   不能等 3 拍才闪第一下）；
/// · `k ≥ 1` —— hold 按住期间每 [`HOLD_PULSE_BEATS`] 拍一次，直到它的尾巴为止。
///
/// 假音符没有判定 ⇒ 没有击中效果（它在游戏里也不该有）。
pub fn hit_fx_progress(note: &Note, tmap: &perf::TimeMap, playhead: f64) -> Option<f32> {
    if note.is_fake {
        return None;
    }
    let pulse = tmap.sec(tmap.beat(note.time) + HOLD_PULSE_BEATS) - note.time;
    // 每帧最多一个效果在场（脉冲间隔 ≥ 一拍 ≫ HIT_FX_SEC），所以只需看两个候选：
    // 当前落在哪一格、以及上一格（浮点边界上不会漏掉刚触发的那一次）。
    let k_now = if pulse > 1e-9 {
        ((playhead - note.time) / pulse).floor() as i64
    } else {
        0
    };
    for k in [k_now, k_now - 1] {
        if k < 0 {
            continue;
        }
        let at = if k == 0 {
            note.time
        } else {
            if note.kind != NoteKind::Hold {
                continue; // 只有 hold 有后续脉冲
            }
            tmap.sec(tmap.beat(note.time) + HOLD_PULSE_BEATS * k as f64)
        };
        // 尾巴之后不再闪（hold 结束就是结束）
        if k > 0 && at >= note.end {
            continue;
        }
        let age = playhead - at;
        if (0.0..HIT_FX_SEC).contains(&age) {
            return Some((age / HIT_FX_SEC) as f32);
        }
    }
    None
}

/// 画一次击中效果：`t` 是进度 `0..1`。
///
/// 判定线上那一点（音符的 `laneX`、线本地 y = 0）闪一下 —— 白闪 + 一圈向外扩散的方框。
/// 渲染层只有实例化的方块可用，所以"环"是**四条细边拼的空心方框**（跟着判定线一起转）。
/// 颜色取音符类型的颜色（环）与白色（闪），与游戏里"打击点亮一下"的观感一致。
fn push_hit_fx(
    out: &mut Vec<NoteInstance>,
    perf: &perf::LinePerf,
    lane_x: f32,
    angle: f32,
    rgb: [f32; 3],
    alpha: f32,
    t: f32,
) {
    let t = t.clamp(0.0, 1.0);
    // ① 白闪：一开始最大最亮，迅速收小淡出（"啪"那一下）
    let flash = 1.0 - t;
    let fw = NOTE_W * 0.5 * (1.0 + 1.8 * t);
    out.push(NoteInstance::new(
        perf.apply([lane_x, 0.0]),
        [fw, fw * (NOTE_H / NOTE_W)],
        [1.0, 1.0, 1.0, 0.85 * flash * alpha],
        angle,
    ));
    // ② 扩散的方框：向外扩到约 3 倍，同时淡出
    let half = NOTE_W * 0.5 * (1.1 + 2.2 * t);
    let w = 1.7_f32;
    let ring = [
        rgb[0] * 0.35 + 0.65,
        rgb[1] * 0.35 + 0.65,
        rgb[2] * 0.35 + 0.65,
        0.9 * (1.0 - t) * alpha,
    ];
    for (dx, dy, hx, hy) in [
        (0.0, half, half, w),
        (0.0, -half, half, w),
        (half, 0.0, w, half),
        (-half, 0.0, w, half),
    ] {
        out.push(NoteInstance::new(
            perf.apply([lane_x + dx, dy]),
            [hx, hy],
            ring,
            angle,
        ));
    }
}

/// 把 RPE 窗口矩形（±675 × ±450）**逆变换**回这条线的本地空间，返回它的 AABB `(lo, hi)`。
///
/// 用途：`build_instances` 的候选筛 —— "音符在屏幕上的包围盒是否与窗口相交"这件事，
/// 在本地空间里等价于"音符的本地包围盒是否与这个框相交"（旋转不改变距离，
/// 而实例的旋转角就是判定线的旋转角 ⇒ 本地空间里音符是轴对齐的）。候选筛只求**保守**
/// （宁可多留几颗让它走到后面那道屏幕判据），所以取 AABB 而不是精确的旋转矩形。
fn local_window_box(perf: &perf::LinePerf) -> ([f32; 2], [f32; 2]) {
    let mut lo = [f32::INFINITY; 2];
    let mut hi = [f32::NEG_INFINITY; 2];
    // 旋转矩形的 AABB 由四个角决定（再带上四边中点：多算两次，防的是我自己写错角标）
    for (sx, sy) in [
        (-1.0_f32, -1.0_f32),
        (1.0, -1.0),
        (-1.0, 1.0),
        (1.0, 1.0),
        (0.0, -1.0),
        (0.0, 1.0),
        (-1.0, 0.0),
        (1.0, 0.0),
    ] {
        let p = perf.apply_inv([sx * RPE_WINDOW_HALF_W, sy * RPE_WINDOW_HALF_H]);
        lo[0] = lo[0].min(p[0]);
        lo[1] = lo[1].min(p[1]);
        hi[0] = hi[0].max(p[0]);
        hi[1] = hi[1].max(p[1]);
    }
    (lo, hi)
}

/// 构建演奏区实例（CPU 侧）。
///
/// 顺序即绘制顺序：`Chart.lines` 已按 zOrder 排好（小的先画、大的盖在上面）。
/// **每条线的变换对它自己和它下面的音符是同一个** —— 这就是"音符依赖于判定线"：
/// 线一旋转/平移，子音符跟着走，不需要给音符单独记位置。
pub fn build_instances(state: &EditorState, out: &mut Vec<NoteInstance>) {
    out.clear();
    let chart: &Chart = &state.chart;
    let tmap = &chart.tmap;

    for (li, line) in chart.lines.iter().enumerate() {
        let perf = line.perf(tmap, state.playhead);
        if perf.alpha <= 0.004 {
            continue; // 完全透明的线（含子音符）不必上报实例
        }
        let angle = perf.rotate_rad();

        // ---- 判定线本体：一条贯穿全宽的细条（±675），跟着旋转 ----
        let selected = li == state.selected_line;
        let mut lc = if selected {
            [0.95, 0.85, 0.35, 0.95]
        } else {
            [0.55, 0.60, 0.75, 0.55]
        };
        lc[3] *= perf.alpha;
        out.push(NoteInstance::new(
            perf.apply([0.0, 0.0]),
            [state.line_half_w, 3.0],
            lc,
            angle,
        ));

        // ---- 子音符 ----
        //
        // 纵向位置按 **RPE 的 floor position** 算：`(H(t_音符) − H(t_此刻)) × 音符自身 speed`，
        // 其中 `H = 120 × ∫ v dτ`（v = 流速事件的值，单位流速 = 120 RPE y 单位/秒，见
        // `perf::SPEED_UNITS_PER_SEC`）。于是**流速 10（RPE 默认值 = 1×）的音符 0.75 秒划过
        // 整个 900 高的窗口** —— 与 RPE 一致。
        //
        // **每个音符自己的那一半（`H(t_音符)`）是加载时算好的**（`state::FlowCache`）：
        // 它与播放头无关、与判定线被搬到哪里也无关，所以没有理由每帧重算一遍。
        // 这里每帧只需要这条线的一个标量 —— `H(此刻)`（查流速检查点表，O(log 事件数)）。
        //
        // 流速事件改了之后，"它之后"的音符位置会过期；那些还没被异步补上的音符在这里**现算**
        // （`floor_offset_now`）。两条路径共用同一份积分实现 ⇒ 现算的值与预算好的值一致，
        // 异步补得快慢**不影响画面**，只影响每帧的代价。
        //
        // ---- 候选集：**按位置选，不按时间窗口选** ----
        //
        // 时间窗口（`510/(120·|v|)`）在**流速换过符号**的谱面上是错的：正负相消时，
        // 一颗远在几秒之外的音符可能仍然贴在窗口里。实测（流速 10 → −10 → −20，
        // 播放头 0.2 s）：3.5 秒那颗的偏移是 **360**（稳稳在 ±450 里），
        // 却被"2.2 秒窗口"挡在外面 ⇒ 整颗没有实例 —— 表现在界面上就是
        // "第二个负流速事件和不存在一样，音符的位置和速度都不受它控制"（用户 2026-09-28 报的）。
        //
        // 现在候选集用**算出来的位置**判：把窗口矩形**逆变换**回这条线的本地空间，取它的 AABB，
        // 再放宽一个音符的半个外接框 + 余量；落在里面的音符才继续做屏幕包围盒与绘制。
        // 于是"增速/减速/换向/长 hold"都不影响正确性 —— 候选集与判定线被搬到哪里也无关。
        // 代价：每帧扫这条线的全部音符（缓存命中时 ~4 ns/颗；10 万音符 ≈ 0.5 ms，
        // `tests/floor_bench.rs` 量得出来），而不是只扫"窗口里那几颗"。
        let h_now = line.h_at(state.playhead, tmap);
        let (box_lo, box_hi) = local_window_box(&perf);
        // 时间窗口只剩一个用途：**还没重算**（脏）的音符拿它兜底 —— 那些音符的位置现在是现算的，
        // 一颗一颗现算太贵（~160 ns/颗），所以只算窗口里的；窗口外的那几颗下一帧补上
        // （`pump_floors` 每帧补 4096 条，且优先补播放头之后的）。
        let window = state.visible_range_of(li);
        for idx in 0..line.notes.len() {
            let note = &line.notes[idx];
            // 音符自身的 speed（文档字段，默认 1.0）乘在**离判定线的距离**上：
            // RPE/prpr 就是这么用的（它不改到达时刻，只改"落多远"）。**带符号** ——
            // 负的音符 speed 把方向翻过来（音符从下方上来），与负流速是同一套几何。
            let spd = note.speed as f64;
            // `speed = 0` 的东西不渲染（RPE：Hold 的 `speed = 0` ⇒ 长度 0 ⇒ 不渲染；
            // 这里对四种音符一视同仁，免得它永远贴在判定线上）
            if spd.abs() < 1e-3 {
                continue;
            }
            // 横向先筛一道：只用 `lane_x`（不碰缓存）。取**最宽**的音符半宽当界，
            // 于是这一道是保守的，命中的才去看纵向。
            if note.lane_x + MAX_NOTE_HALF_W < box_lo[0] - EditorState::NOTE_SPAN_MARGIN
                || note.lane_x - MAX_NOTE_HALF_W > box_hi[0] + EditorState::NOTE_SPAN_MARGIN
            {
                continue;
            }
            // 这颗音符此刻离判定线多远（判据与画法都只用这一个数）
            let lead_h = match line.floor_offset(idx, h_now) {
                Some(v) => v,
                None if window.contains(&idx) => line.floor_offset_now(note.time, h_now, tmap),
                None => continue,
            };
            let lead = (lead_h * spd) as f32;
            let age = state.playhead - note.time;
            // ---- 画不画：**位置 < 0 ⇒ 在判定线之下 ⇒ 不显示**（用户口径）----
            //
            // 只有这一条规则，**没有"哪种流速"的分支**：流速为负（音符从判定线下面飞上来）、
            // 以及"负→正"的过零段（过零点之前也在下面），都只是"位置此刻是负的"的不同来路。
            // 到线那一刻（`age ≥ 0`）就不再算"之下"：音符停在判定线上收缩消失，
            // **击中效果照旧会播** —— 否则负流速段完全没有反馈。
            //
            // 这条与"音符只要在可见区域就要显示"不冲突：后者管的是**别拿"离判定线多远"
            // 当可见性判据**（判定线被移开/旋转时会漏画），本条管的是**在线下面那一半不画**。
            let y_local = if age > 0.0 { 0.0 } else { lead };
            if y_local < 0.0 {
                continue;
            }
            // 到达之后 0→1 的消失进度；`>= 1` 就彻底没了（只剩击中效果在场）。
            // **必须夹到 0**：`age < 0` 是"还没到"（绝大多数音符），不夹就成了负进度 ⇒
            // 音符被放大到 2.8 倍、alpha 乘到 5.5（实测：一个 510×144 的亮蓝块挂在窗口顶上）。
            let gone = if age > 0.0 {
                (age / HIT_FADE_SEC).min(1.0) as f32
            } else {
                0.0
            };
            // ---- hold 的**尾巴**：`H(t_尾) − H(t_此刻)`（与头部同源）----
            //
            // 尾巴曾经算成"头的偏移 + 整段时长"，而"头的偏移"当时来自一个**只会往前走**的
            // 单调累加器 —— 查询过去的时刻一律返回当前累计值（0）⇒ 被按住时尾巴被钉死在
            // "头 + 全长"上：身子不随按住而缩短，尾巴过去之后也永远不消失。
            // 现在头尾都是查表（过去/现在/将来都能问），这个病根不存在了。
            let tail_h = if note.kind == NoteKind::Hold {
                line.floor_tail_offset(idx, h_now)
                    .unwrap_or_else(|| line.floor_offset_now(note.end, h_now, tmap))
            } else {
                lead_h
            };
            let tail_y = (tail_h * spd) as f32;
            // ---- 候选筛（本地空间，保守）：头尾那一段连它的半宽半高都在本地窗口框之外 ⇒
            //      一定看不见，不必做变换与屏幕包围盒 ----
            //
            // 本地空间里音符是**轴对齐**的（实例的旋转角就是判定线的旋转角）：x 是 `lane_x ± nw/2`，
            // y 是 `[min(头,尾) − nh/2, max(头,尾) + nh/2]`；判定线被移开/旋转时窗口框也跟着搬
            // （`local_window_box`），所以"判定线被搬到别处、音符其实在屏幕里"那种情形照样命中。
            let (nw, nh) = match note.kind {
                NoteKind::Hold => (HOLD_W, NOTE_H),
                _ => (NOTE_W, NOTE_H),
            };
            let slack = EditorState::NOTE_SPAN_MARGIN;
            let (ny_lo, ny_hi) = (y_local.min(tail_y) - nh * 0.5, y_local.max(tail_y) + nh * 0.5);
            if note.lane_x + nw * 0.5 < box_lo[0] - slack
                || note.lane_x - nw * 0.5 > box_hi[0] + slack
                || ny_hi < box_lo[1] - slack
                || ny_lo > box_hi[1] + slack
            {
                continue;
            }
            // ---- 可见性判据：**按屏幕上的位置**，不是"离判定线多远" ----
            //
            // 这里修的是一个真 bug：判据曾经只看**线本地**的偏移（`|offset| > 510 就跳过`），
            // 而判定线是会被事件移开/旋转的 —— 线被移到 y=-300 时，偏移 660 的音符在屏幕上
            // 的 y 是 **+360**（明明在窗口里），却被当成"离判定线太远"整颗丢掉；
            // 线旋转 90° 时，偏移 660 的音符落在屏幕上 x=-660（±675 之内）同样被丢掉。
            // 现在判据是"这颗音符（连它自己的半宽半高）在屏幕上的包围盒是否与窗口相交"。
            let head_pt = perf.apply([note.lane_x, y_local]);
            let tail_pt = perf.apply([note.lane_x, tail_y]);
            // 旋转过的方块在屏幕上的外接半径（保守：宁可多建几个实例，也不能漏画看得见的）
            let radius = ((nw * 0.5) * (nw * 0.5) + (nh * 0.5) * (nh * 0.5)).sqrt()
                + EditorState::NOTE_SPAN_MARGIN;
            let (lo_x, hi_x) = (head_pt[0].min(tail_pt[0]), head_pt[0].max(tail_pt[0]));
            let (lo_y, hi_y) = (head_pt[1].min(tail_pt[1]), head_pt[1].max(tail_pt[1]));
            if hi_x < -RPE_WINDOW_HALF_W - radius
                || lo_x > RPE_WINDOW_HALF_W + radius
                || hi_y < -RPE_WINDOW_HALF_H - radius
                || lo_y > RPE_WINDOW_HALF_H + radius
            {
                continue;
            }

            // ---- 击中效果 ----
            //
            // 用户要求：音符**到达判定线后出现击中效果并消失**；hold **击中后立即播一次**，
            // 之后按住期间每 3 拍再播一次。渲染是每帧重算的（没有"事件"这个对象），所以
            // "播一次"落实成"在某个时刻之后的一小段窗口里画它" —— 见 `hit_fx_progress`。
            if let Some(t) = hit_fx_progress(note, &state.chart.tmap, state.playhead) {
                let c = note.kind.color();
                push_hit_fx(out, &perf, note.lane_x, angle, [c[0], c[1], c[2]], perf.alpha, t);
            }

            let mut color = note.kind.color();
            color[3] *= perf.alpha;
            let hold_selected = selected && state.is_note_selected(idx);
            if hold_selected {
                color = [1.0, 1.0, 1.0, perf.alpha];
            }

            // ---- hold 的身子：被"按住"吃掉的那一段不再画（从判定线起算到尾巴）----
            //
            // **寿命**：hold 到 `note.end` 就结束 —— 之后一段身子都不画。这条必须显式写出来，
            // 不能靠符号判断（下面的"整段在线下"只挡住了正流速那一半）：**流速为负时
            // "已经过去的尾巴"在判定线上面**（`H` 随时间是减小的，`now > end` ⇒
            // `H(end) − H(now) > 0`），光看符号会把它当成"还在上升的长条"画出来。
            // 是那条"尾巴之后两种符号都不该再有身子"的用例把它抓出来的。
            if note.kind == NoteKind::Hold && state.playhead < note.end {
                // 到线之前身子是 [头, 尾]；到线之后头那一段已经被吃掉 ⇒ 身子从**判定线**起算。
                // 段是**带符号**的：负流速时尾巴在判定线下面，段就画在线的下面。
                let body_a = y_local;
                let dy = tail_y - body_a;
                // 身子**整段都在判定线之下**（含贴线的那一端）⇒ 不画，与上面同一条口径
                if dy.abs() > 1.0 && body_a.max(tail_y) > 0.0 {
                    let mid_local = [note.lane_x, body_a + dy * 0.5];
                    let mut hc = [color[0], color[1], color[2], 0.55 * perf.alpha];
                    if hold_selected {
                        hc = [1.0, 1.0, 1.0, 0.7];
                    }
                    out.push(NoteInstance::new(
                        perf.apply(mid_local),
                        [HOLD_W * 0.5, dy.abs() * 0.5],
                        hc,
                        angle,
                    ));
                }
            }

            // ---- 音符本体：到线之后收缩淡出，`HIT_FADE_SEC` 之后不再画 ----
            if gone < 1.0 {
                let k = (1.0 - gone).max(0.0);
                let mut c = color;
                c[3] *= k;
                out.push(NoteInstance::new(
                    head_pt,
                    [nw * 0.5 * k.max(0.15), nh * 0.5 * k.max(0.15)],
                    c,
                    angle,
                ));
            }
        }
    }
}

/// 收尾：边界压暗 + 边框。必须在所有内容之后调用，否则压暗盖不住内容、边框会被内容压住。
pub fn push_window_overlay(state: &EditorState, out: &mut Vec<NoteInstance>) {
    if state.show_boundary {
        push_window_dim(out, state.boundary_dim);
        push_window_frame(out, 1.0);
    }
}

/// 压力模式：无视时间裁剪，把所有线的所有音符都塞进实例列表（逼近渲染上限）
pub fn build_instances_all(state: &EditorState, out: &mut Vec<NoteInstance>) {
    out.clear();
    let chart = &state.chart;
    let total: usize = chart.lines.iter().map(|l| l.notes.len()).sum();
    let spacing = (RPE_H * 0.5) / (total.max(1) as f32);
    let mut k = 0usize;
    for line in &chart.lines {
        let perf = line.perf(&chart.tmap, state.playhead);
        let angle = perf.rotate_rad();
        for note in &line.notes {
            let y = -RPE_H * 0.5 + k as f32 * spacing;
            k += 1;
            out.push(NoteInstance::new(
                perf.apply([note.lane_x, y]),
                [NOTE_W * 0.5, (spacing * 0.4).max(0.5)],
                note.kind.color(),
                angle,
            ));
        }
    }
}

const NOTE_W: f32 = 92.0;
const NOTE_H: f32 = 26.0;
const HOLD_W: f32 = 78.0;

/// 四种音符里最宽的半宽（`NOTE_W / 2`）。候选筛的第一道（只看 `lane_x`）用它当界 ——
/// 保守（宁可多留几颗），但**不碰缓存**，于是横向就在窗口外的音符一颗都不用管。
const MAX_NOTE_HALF_W: f32 = NOTE_W * 0.5;

const SHADER: &str = r#"
struct Uniforms {
    viewport_px: vec2<f32>,
    scale_px: f32,
    _pad: f32,
};
@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsIn {
    @location(0) corner: vec2<f32>,
    @location(1) center: vec2<f32>,
    @location(2) half: vec2<f32>,
    @location(3) color: vec4<f32>,
    @location(4) angle: f32,
};
struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(in: VsIn) -> VsOut {
    // 局部角点 → 按 angle 旋转 → 平移到中心（判定线的旋转/位移对线与音符是同一个变换）
    let off = in.corner * in.half;
    let c = cos(in.angle);
    let s = sin(in.angle);
    let rot = vec2<f32>(off.x * c - off.y * s, off.x * s + off.y * c);
    // RPE 单位 → 演奏区像素 → 该 viewport 内的 NDC
    let rpe = in.center + rot;
    let px = rpe * u.scale_px;
    let ndc = px / (u.viewport_px * 0.5);
    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;
