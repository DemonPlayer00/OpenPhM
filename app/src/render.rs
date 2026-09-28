//! 演奏区渲染：自研 wgpu 实例化管线，经 `egui_wgpu::CallbackTrait` 挂进 egui 的同一个 render pass。
//!
//! 与 S1b spike 的关键差别：**用回调矩形的 viewport 把演奏区映射到自己的坐标空间**，
//! 而不是"铺满全屏再被裁剪"。这样演奏区在面板布局变化、分数缩放（fractional DPI）下都能正确对齐。
//!
//! 坐标约定（与 `spec/opm-format.md` 第 3 节一致）：
//!   · RPE 坐标系：x ∈ [-675, 675]，y ∈ [-450, 450]，原点在演奏区中心
//!   · 等比缩放（letterbox）：scale_px = min(viewport_w / 1350, viewport_h / 900)
//!   · 时间轴向上：y = (note_time - playhead) / lookahead * 450

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::state::{Chart, EditorState, NoteKind};

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
        let lookahead = state.lookahead.max(1e-3);
        // 流速只是**预览约定**：10 为基准，大于 10 时同样时间内落得更远。
        // 游戏内判定语义由播放器决定（与"变速 hold 交播放器"同一个判断）。
        let speed_scale = (perf.speed / 10.0).clamp(0.05, 20.0);
        for idx in state.visible_range_of(li) {
            let note = &line.notes[idx];
            let dt = note.time - state.playhead;
            let y_local = (dt / lookahead) as f32 * (RPE_H * 0.5) * speed_scale;

            let mut color = note.kind.color();
            color[3] *= perf.alpha;
            let hold_selected = selected && Some(idx) == state.selected_note;
            if hold_selected {
                color = [1.0, 1.0, 1.0, perf.alpha];
            }

            if note.kind == NoteKind::Hold {
                let dy = ((note.end - note.time) / lookahead) as f32 * (RPE_H * 0.5) * speed_scale;
                let mid_local = [note.lane_x, y_local + dy * 0.5];
                let mut hc = [color[0], color[1], color[2], 0.55 * perf.alpha];
                if hold_selected {
                    hc = [1.0, 1.0, 1.0, 0.7];
                }
                out.push(NoteInstance::new(
                    perf.apply(mid_local),
                    [HOLD_W * 0.5, (dy.abs() * 0.5).max(1.0)],
                    hc,
                    angle,
                ));
            }

            let (w, h) = match note.kind {
                NoteKind::Hold => (HOLD_W, NOTE_H),
                _ => (NOTE_W, NOTE_H),
            };
            out.push(NoteInstance::new(
                perf.apply([note.lane_x, y_local]),
                [w * 0.5, h * 0.5],
                color,
                angle,
            ));
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
