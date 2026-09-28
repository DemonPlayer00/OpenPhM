//! 无头渲染：把 opm 文档在指定播放头位置渲染成 PNG。
//!
//! 这是给 **agent 的"眼睛"**：无头 CLI 改完谱后可直接出图，再由 agent 读取图片核对结果
//! （配合 `--verify-align` 的对齐标记还可做几何自检）。

use std::path::Path;

use crate::doc::Document;
use crate::render::Playfield;
use crate::state::EditorState;

/// 把 opm 文档转成渲染用的视图（线优先：判定线是父对象，音符挂在线上）。
///
/// 实现在 `state.rs`（视图模型的归属地）；这里保留同名转发，使 `headless` 仍是 agent 的单一入口。
pub use crate::state::chart_from_doc;

/// 判定线在某个时刻的表演快照 —— **给 agent 的数值核对**（不是只能看图）。
///
/// 每条线给出：属性、子音符数、五条轨道的事件数、以及该时刻求值出的 moveX/moveY/rotate/alpha/speed。
/// agent 据此可以断言"我把 rotate 事件改成 90 度之后，线确实转了"，而不必依赖读图。
pub fn lines_report(doc: &Document, playhead_sec: f64) -> serde_json::Value {
    let tmap = crate::perf::TimeMap::from_doc(doc);
    let chart = chart_from_doc(doc);
    let lines: Vec<serde_json::Value> = chart
        .lines
        .iter()
        .map(|l| {
            let perf = l.perf(&tmap, playhead_sec);
            let tracks: Vec<serde_json::Value> = crate::state::TrackId::ALL
                .iter()
                .map(|id| {
                    let t = l.track(*id);
                    serde_json::json!({
                        "track": id.key(),
                        "events": t.events.len(),
                        "valueAt": crate::perf::eval_events(&t.events, tmap.beat(playhead_sec)),
                    })
                })
                .collect();
            serde_json::json!({
                "index": l.index,
                "name": l.name,
                "zOrder": l.z_order,
                "isCover": l.is_cover,
                "bpmFactor": l.bpm_factor,
                "notes": l.note_count(),
                "perf": {
                    "moveX": perf.x, "moveY": perf.y, "rotate": perf.rotate_deg,
                    "alpha": perf.alpha, "speed": perf.speed,
                },
                "tracks": tracks,
            })
        })
        .collect();
    serde_json::json!({
        "playheadSec": playhead_sec,
        "playheadBeat": tmap.beat(playhead_sec),
        "bpmSegments": tmap.seg_count(),
        "bpmAtPlayhead": tmap.bpm_at(playhead_sec),
        "duration": tmap.duration,
        "lines": lines,
    })
}

/// 出图选项（与 GUI 的视图设置同源，避免"agent 看到的"和"人看到的"不一致）
#[derive(Clone, Copy)]
pub struct RenderOpts {
    /// 演奏区垂直可视范围（秒）
    pub lookahead: f64,
    /// 判定线**全长**（编辑器设置，默认 1350 = 与窗口同宽）
    pub line_len: f32,
    /// 是否画窗口边界框（RPE ±675 × ±450）
    pub boundary: bool,
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            lookahead: 2.0,
            line_len: crate::state::RPE_LINE_HALF_W * 2.0,
            boundary: true,
        }
    }
}

/// 在给定播放头位置渲染一帧到 PNG。
pub fn render_png(
    doc: &Document,
    playhead_sec: f64,
    width: u32,
    height: u32,
    out: &Path,
    opts: RenderOpts,
) -> Result<(), String> {
    let chart = chart_from_doc(doc);
    let mut st = EditorState::new(chart);
    st.lookahead = opts.lookahead;
    st.line_half_w = (opts.line_len * 0.5).max(1.0);
    st.show_boundary = opts.boundary;
    st.boundary_dim = crate::render::dim_alpha_for(crate::render::DIM_ALPHA_DEFAULT, FORMAT);
    st.seek(playhead_sec);
    // 无头出图没有"选中"这回事：默认选中 0 号线会让它被画成高亮黄色，
    // 把 agent 的眼睛带偏（"为什么这条线是黄的？"）。置成越界值即无高亮。
    st.selected_line = usize::MAX;

    let mut instances = Vec::new();
    crate::render::build_instances(&st, &mut instances);
    // 收尾：窗口边界压暗 + 边框（与 GUI 同一份几何）
    crate::render::push_window_overlay(&st, &mut instances);

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        flags: wgpu::InstanceFlags::default(),
        memory_budget_thresholds: Default::default(),
        backend_options: Default::default(),
        display: None,
    });
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    let adapter = adapters
        .iter()
        .filter(|a| !matches!(a.get_info().device_type, wgpu::DeviceType::Cpu))
        .max_by_key(|a| match a.get_info().device_type {
            wgpu::DeviceType::DiscreteGpu => 3,
            wgpu::DeviceType::IntegratedGpu => 2,
            _ => 1,
        })
        .or_else(|| adapters.first())
        .ok_or("没有可用的 GPU 适配器")?
        .clone();

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("opm-headless"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        experimental_features: Default::default(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    }))
    .map_err(|e| format!("request_device 失败: {e}"))?;

    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut pf = Playfield::new(&device, FORMAT);
    let viewport_px = [width as f32, height as f32];

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("headless-target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target.create_view(&Default::default());

    pf.upload(&device, &queue, viewport_px, &instances);
    let count = instances.len() as u32;

    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("headless"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.04,
                        g: 0.04,
                        b: 0.06,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pf.draw(&mut pass, count, viewport_px);
    }
    queue.submit(Some(encoder.finish()));

    // 回读
    let bytes_per_row = (width * 4).div_ceil(256) * 256;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (bytes_per_row * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    let _ = device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    rx.recv()
        .map_err(|e| format!("回读回调丢失: {e}"))?
        .map_err(|e| format!("回读失败: {e}"))?;

    let data = slice
        .get_mapped_range()
        .map_err(|e| format!("get_mapped_range 失败: {e}"))?;
    // 去掉行填充
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for row in 0..height {
        let start = (row * bytes_per_row) as usize;
        pixels.extend_from_slice(&data[start..start + (width * 4) as usize]);
    }
    drop(data);
    readback.unmap();

    write_png(out, width, height, &pixels)?;
    println!(
        "  渲染完成：{}（{}×{}，实例 {}，播放头 {:.3}s）",
        out.display(),
        width,
        height,
        count,
        playhead_sec
    );
    Ok(())
}

fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("创建文件失败: {e}"))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| format!("PNG 头写入失败: {e}"))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| format!("PNG 数据写入失败: {e}"))?;
    Ok(())
}
