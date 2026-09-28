//! 音频播放：**播放头跟着音频时钟走**，而不是跟着墙钟。
//!
//! 为什么必须这样：谱面与音乐一旦各走各的时钟，误差会一直累积（S2 spike 实测音频时钟相对墙钟
//! 漂移 7.4 ppm ≈ 每小时 27 ms，看似小，但"看着谱面打拍子"要的是**当前听到的那一帧**在哪）。
//! 所以这里的设计是：
//!
//! ```text
//!   cpal 输出回调（实时线程）  ──写──▶  Arc<AtomicU64> 已送入设备的帧数（唯一的真相）
//!                                          │
//!   GUI 线程 ──读───────────────▶ position_sec() = 帧数/采样率 − 输出延迟 + 用户校准
//! ```
//!
//! 输出延迟由 cpal 的 `OutputCallbackInfo::timestamp()` 自校准
//! （`playback.duration_since(callback)` 就是"这批样本多久之后才会响"），
//! 不写死经验值 —— 换设备/换 buffer 大小都不用改代码。剩余的那点偏差用 `offset_ms` 手动校准。
//!
//! 解码交给 **symphonia**（纯 Rust、活跃维护）：wav / flac / mp3 / ogg-vorbis / m4a-aac / alac / adpcm 等
//! 主流格式都能读。早先手写的"只读 WAV"版本已被它取代 —— 那份代码的唯一优势是零依赖，
//! 而一旦要主流格式，自己写解码器就不是省事而是冒险（自研 OGG/MP3 解码器不是这个项目该做的事）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// 一次播放会话的共享状态（音频回调线程写、GUI 线程读）
struct Shared {
    /// 已送入设备缓冲的**帧数**（不是样本数！交错样本下标 = 帧号 × 声道数）。
    /// 单位混用踩过：早先把"样本下标"当帧数，`游标/采样率` 于是正好差一个声道数倍（立体声 = 2×），
    /// 表现是播放头跑得比音频快一倍（声音本身速度却是对的）。
    cursor: AtomicU64,
    playing: AtomicBool,
    /// 音频是否已播完（cursor 到达末尾）
    ended: AtomicBool,
    /// 输出延迟（纳秒），首次回调时由 cpal 时间戳自校准
    latency_ns: AtomicU64,
    /// 回调欠载次数（本该出声却拿不到数据）
    underruns: AtomicU64,
    /// 回调次数（诊断用）
    callbacks: AtomicU64,
}

pub struct Audio {
    shared: Arc<Shared>,
    /// 交错样本（设备声道数、设备采样率）—— 只读，回调直接索引
    samples: Arc<Vec<f32>>,
    /// 保持流存活（drop 即停止）
    _stream: cpal::Stream,
    device_rate: u32,
    device_channels: u16,
    /// 源文件信息（只在界面里展示）
    pub src_rate: u32,
    pub src_channels: u16,
    /// 实际解码器（symphonia 报出来的编码名）
    pub codec: String,
    /// 用户校准偏移（毫秒）：正值表示"听到的比游标算出来的更早"。
    /// 用原子：控制通道线程会随时改它（`audio_offset` 视图命令），而回调/GUI 在别处读。
    offset_ns: AtomicU64,
    pub path: String,
    /// 显示用的文件名（**载入时算一次**）：状态栏每帧要画"♪ 歌名"，
    /// 而在帧里对路径 `file_name()` 再分配一个字符串是白做的功 —— 载入之后它不会变。
    name: String,
}

/// 该加载哪个音频文件（**纯函数**：只做路径决策，不碰音频设备、不读文件）。
///
/// 规则（`--audio` 优先，其次谱面字段）：
/// - `--audio off|none|""` ⇒ **明确不要音频**（`Ok(None)`）；
/// - `--audio FILE` ⇒ 就用它（相对路径按当前工作目录，交给系统解释）；
/// - 不给 `--audio` ⇒ 用谱面 `meta.audio`：**相对路径按谱面所在目录解析**
///   （谱面里写的是相对自身的名字：容器把音频装进包里就是 `song.ogg` 这种裸文件名）。
///
/// 返回值第二项是"来源"，只用于错误提示 —— 用户要能分清是 `--audio` 写错了，还是谱面里那个字段有问题。
pub fn resolve_source(
    spec: Option<&str>,
    meta_audio: Option<&str>,
    chart_path: Option<&Path>,
) -> Result<Option<(PathBuf, Source)>, String> {
    match spec {
        Some("off") | Some("none") | Some("") => return Ok(None),
        Some(p) => {
            if p.trim().is_empty() {
                return Ok(None);
            }
            return Ok(Some((PathBuf::from(p), Source::Cli)));
        }
        None => {}
    }
    let Some(rel) = meta_audio.filter(|r| !r.trim().is_empty()) else {
        return Ok(None);
    };
    let p = PathBuf::from(rel);
    if p.is_absolute() {
        return Ok(Some((p, Source::Meta)));
    }
    // 相对路径：按谱面所在目录补全；谱面还没保存过（没有路径）时保持原样，交给系统按 CWD 解释
    let full = match chart_path.and_then(|b| b.parent()) {
        Some(dir) => dir.join(&p),
        None => p,
    };
    Ok(Some((full, Source::Meta)))
}

/// 音频来源（只为错误提示服务）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `--audio FILE`
    Cli,
    /// 谱面字段 `meta.audio`
    Meta,
}

impl Source {
    /// 出错时给用户看的那句话（"谁指定的"）
    pub fn label(self) -> &'static str {
        match self {
            Source::Cli => "--audio",
            Source::Meta => "谱面 meta.audio",
        }
    }
}

impl Audio {
    /// 载入 WAV 并打开输出流。`device` 为 None 时用默认输出设备。
    pub fn load(path: &Path) -> Result<Self, String> {
        let wav = decode(path)?;
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "没有可用的默认输出设备".to_string())?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("读取默认输出配置失败: {e}"))?;
        let device_rate = supported.sample_rate();
        let device_channels = supported.channels();
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        // 声道/采样率对齐（线性重采样：编辑器预览够用，且不做隐藏的质量承诺）
        let samples = Arc::new(convert(
            &wav.samples,
            wav.channels,
            wav.rate,
            device_channels,
            device_rate,
        ));

        let shared = Arc::new(Shared {
            cursor: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            latency_ns: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            callbacks: AtomicU64::new(0),
        });

        let stream = build_stream(
            &device,
            &config,
            sample_format,
            device_channels,
            samples.clone(),
            shared.clone(),
        )?;

        let me = Self {
            shared,
            samples,
            _stream: stream,
            device_rate,
            device_channels,
            src_rate: wav.rate,
            src_channels: wav.channels,
            codec: wav.codec.clone(),
            offset_ns: AtomicU64::new(0),
            path: path.display().to_string(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
        };
        me._stream.play().map_err(|e| format!("启动输出流失败: {e}"))?;
        Ok(me)
    }

    /// 显示用的文件名（状态栏"♪ 歌名"）—— 载入时算好，帧里不再解析路径
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 当前**应当听到**的位置（秒）。音频未载入时返回 None。
    pub fn position_sec(&self) -> f64 {
        let frames = self.shared.cursor.load(Ordering::Relaxed) as f64;
        let latency = self.shared.latency_ns.load(Ordering::Relaxed) as f64 / 1e9;
        let offset = self.offset_ns.load(Ordering::Relaxed) as f64 / 1e9;
        let pos = frames / self.device_rate as f64 - latency - offset;
        pos.max(0.0)
    }

    /// 从 `from_sec` 开始播放（会立刻重设游标，不需要等下一帧）
    pub fn play_from(&self, from_sec: f64) {
        let frames = (from_sec.max(0.0) * self.device_rate as f64).round() as u64;
        let total = self.total_frames();
        self.shared.cursor.store(frames.min(total), Ordering::Relaxed);
        self.shared.ended.store(false, Ordering::Relaxed);
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&self) {
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    pub fn ended(&self) -> bool {
        self.shared.ended.load(Ordering::Relaxed)
    }

    /// 已载入音频的时长（秒）
    pub fn duration(&self) -> f64 {
        self.total_frames() as f64 / self.device_rate as f64
    }

    /// 设置用户校准偏移（毫秒）
    pub fn set_offset_ms(&self, ms: f64) {
        // 负值要能表示（听到的比游标更晚），因此用 i64 存到 AtomicU64 里
        self.offset_ns
            .store((ms * 1e6) as i64 as u64, Ordering::Relaxed);
    }

    pub fn offset_ms(&self) -> f64 {
        self.offset_ns.load(Ordering::Relaxed) as i64 as f64 / 1e6
    }

    pub fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    pub fn callbacks(&self) -> u64 {
        self.shared.callbacks.load(Ordering::Relaxed)
    }

    /// cpal 自校准出来的输出延迟（毫秒）
    pub fn latency_ms(&self) -> f64 {
        self.shared.latency_ns.load(Ordering::Relaxed) as f64 / 1e6
    }

    pub fn device(&self) -> String {
        format!(
            "{} Hz / {} ch（源 {} Hz / {} ch / {}）",
            self.device_rate, self.device_channels, self.src_rate, self.src_channels, self.codec
        )
    }

    fn total_frames(&self) -> u64 {
        (self.samples.len() / self.device_channels.max(1) as usize) as u64
    }
}

// ---------------------------------------------------------------- 解码（symphonia）

/// 解码结果：交错 f32 样本 + 源规格
#[derive(Debug)]
pub struct Decoded {
    pub samples: Vec<f32>,
    pub channels: u16,
    pub rate: u32,
    /// 实际用的解码器名（诊断/界面展示：能看出"这个文件是谁解出来的"）
    pub codec: String,
}

/// 把 symphonia 的编码 id 变成人看得懂的名字。
/// symphonia 的 `Display` 给的是十六进制 id（`0x1006`），界面上没法看。
fn codec_name(id: symphonia::core::codecs::audio::AudioCodecId) -> String {
    use symphonia::core::codecs::audio::well_known as wk;
    let name = if id == wk::CODEC_ID_MP3 {
        "MP3"
    } else if id == wk::CODEC_ID_VORBIS {
        "OGG Vorbis"
    } else if id == wk::CODEC_ID_FLAC {
        "FLAC"
    } else if id == wk::CODEC_ID_AAC {
        "AAC (m4a)"
    } else if id == wk::CODEC_ID_ALAC {
        "ALAC"
    } else if id == wk::CODEC_ID_PCM_S16LE
        || id == wk::CODEC_ID_PCM_S24LE
        || id == wk::CODEC_ID_PCM_S32LE
        || id == wk::CODEC_ID_PCM_F32LE
    {
        "PCM (wav)"
    } else {
        return format!("{id}");
    };
    name.to_owned()
}

/// 只解码并报告规格（`--audio-probe` / agent 用）。不解码输出流，因此无需音频设备。
pub fn probe(path: &Path) -> Result<serde_json::Value, String> {
    let d = decode(path)?;
    let frames = d.samples.len() / d.channels.max(1) as usize;
    Ok(serde_json::json!({
        "path": path.display().to_string(),
        "codec": d.codec,
        "sampleRate": d.rate,
        "channels": d.channels,
        "frames": frames,
        "durationSec": frames as f64 / d.rate.max(1) as f64,
    }))
}

/// 用 symphonia 解码任意受支持格式（wav/flac/mp3/ogg-vorbis/m4a-aac/alac/adpcm…）。
///
/// 失败时给出**明确**的原因（识别不出容器 / 没有音频轨 / 不支持的编码），而不是静默无声。
pub fn decode(path: &Path) -> Result<Decoded, String> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::errors::Error as SymErr;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let file = std::fs::File::open(path).map_err(|e| format!("打开 {} 失败: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| {
            format!(
                "识别不出音频格式（{e}）。支持 wav / flac / mp3 / ogg-vorbis / m4a-aac / alac 等主流格式"
            )
        })?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| "文件里没有音频轨".to_string())?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| "缺少音频编码参数".to_string())?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| format!("不支持的编码（{e}）"))?;

    let mut samples: Vec<f32> = Vec::new();
    let mut channels = 0u16;
    let mut rate = 0u32;
    let codec = codec_name(params.codec);
    let mut buf: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break, // 流结束
            Err(SymErr::ResetRequired) => break, // 链式 ogg 等：当作结束（v0.5 起只有这一种用途）
            Err(SymErr::IoError(_)) => break,
            Err(e) => return Err(format!("读取包失败: {e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                let spec = audio_buf.spec();
                if channels == 0 {
                    channels = spec.channels().count() as u16;
                    rate = spec.rate();
                }
                buf.resize(audio_buf.samples_interleaved(), 0.0);
                audio_buf.copy_to_slice_interleaved(&mut buf);
                samples.extend_from_slice(&buf);
            }
            // 单个包坏了就跳过（坏帧不该让整首歌载不进来）
            Err(SymErr::DecodeError(_)) | Err(SymErr::IoError(_)) => continue,
            Err(e) => return Err(format!("解码失败: {e}")),
        }
    }
    if channels == 0 || samples.is_empty() {
        return Err("解码后没有任何音频样本".into());
    }
    Ok(Decoded {
        samples,
        channels,
        rate,
        codec,
    })
}

// ---------------------------------------------------------------- 声道/采样率对齐

/// 把源样本转成设备要求的声道数与采样率（线性插值）。
fn convert(src: &[f32], src_ch: u16, src_rate: u32, dst_ch: u16, dst_rate: u32) -> Vec<f32> {
    let (sc, dc) = (src_ch as usize, dst_ch as usize);
    let frames = src.len() / sc;
    let out_frames = ((frames as f64) * dst_rate as f64 / src_rate as f64).ceil() as usize;
    let mut out = vec![0.0f32; out_frames * dc];
    let ratio = src_rate as f64 / dst_rate as f64;
    for i in 0..out_frames {
        let pos = i as f64 * ratio;
        let i0 = pos.floor() as usize;
        let frac = (pos - i0 as f64) as f32;
        let i1 = (i0 + 1).min(frames.saturating_sub(1));
        for c in 0..dc {
            // 声道映射：多于目标则丢弃多余声道；少于目标则复制最后一条
            let s0 = src.get(i0 * sc + c.min(sc - 1)).copied().unwrap_or(0.0);
            let s1 = src.get(i1 * sc + c.min(sc - 1)).copied().unwrap_or(0.0);
            out[i * dc + c] = s0 + (s1 - s0) * frac;
        }
    }
    out
}

// ---------------------------------------------------------------- 混音（纯函数，可单测）

/// 填充结果：正常填满返回 None；到位则是"播完了"或"数据不够"
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FillStatus {
    /// 播放位置越过了数据末尾
    Ended,
    /// 数据在中间断了（真实丢帧）
    Underrun,
}

/// 把 `samples` 里从第 `cur` **帧**开始的音频拷进 `out`（交错，每帧 `ch` 个样本）。
/// 返回 (新的帧游标, 状态)。**单位是帧，不是样本** —— 这个函数的存在就是为了让
/// "把样本下标当帧数"那类 bug 在单测里立刻现形（立体声下会正好差 2×）。
pub fn fill_frames(
    samples: &[f32],
    ch: usize,
    cur: usize,
    playing: bool,
    out: &mut [f32],
) -> (usize, Option<FillStatus>) {
    let ch = ch.max(1);
    let total_frames = samples.len() / ch;
    let mut frame = cur;
    let mut status = None;
    for buf in out.chunks_mut(ch) {
        if playing && frame < total_frames {
            let base = frame * ch;
            for (o, s) in buf.iter_mut().zip(&samples[base..base + ch]) {
                *o = *s;
            }
            frame += 1;
        } else {
            for o in buf.iter_mut() {
                *o = 0.0;
            }
            if playing && status.is_none() {
                // 游标已经越过末尾（中途跳跃）算欠载；正好在末尾停下算正常结束
                status = Some(if frame > total_frames {
                    FillStatus::Underrun
                } else {
                    FillStatus::Ended
                });
            }
        }
    }
    (frame, status)
}

// ---------------------------------------------------------------- cpal 输出流

fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    channels: u16,
    samples: Arc<Vec<f32>>,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, String> {
    let err_fn = |e| eprintln!("[audio] 输出流错误: {e}");
    let ch = channels.max(1) as usize;
    // 游标以**原子值为准**（seek 会从别的线程直接改它），回调内不另存一份副本 ——
    // 否则一帧里"回调缓存的旧值"会把 seek 覆盖回去。
    let fill = move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
        let ts = info.timestamp();
        // 首次回调自校准输出延迟：playback 比 callback 晚多少，就是这批样本多久后才会响
        if shared.latency_ns.load(Ordering::Relaxed) == 0 {
            // cpal 0.18：`duration_since` 直接返回 Duration（不是 Option），参数按值传
            let d = ts.playback.duration_since(ts.callback);
            shared
                .latency_ns
                .store(d.as_nanos() as u64, Ordering::Relaxed);
        }
        shared.callbacks.fetch_add(1, Ordering::Relaxed);

        let playing = shared.playing.load(Ordering::Relaxed);
        // 游标单位是**帧**，以原子值为准（seek 会从别的线程直接改它）
        let cur = shared.cursor.load(Ordering::Relaxed) as usize;
        let (next, status) = fill_frames(&samples, ch, cur, playing, data);
        if let Some(st) = status {
            match st {
                FillStatus::Ended => {
                    shared.playing.store(false, Ordering::Relaxed);
                    shared.ended.store(true, Ordering::Relaxed);
                }
                FillStatus::Underrun => {
                    shared.underruns.fetch_add(1, Ordering::Relaxed);
                    shared.playing.store(false, Ordering::Relaxed);
                    shared.ended.store(true, Ordering::Relaxed);
                }
            }
        }
        shared.cursor.store(next as u64, Ordering::Relaxed);
    };

    // 只走 f32 一条路径：其它采样格式在 cpal 侧做类型转换太啰嗦，
    // 而默认输出配置在 PipeWire/PulseAudio 上通常就是 f32。
    if format != cpal::SampleFormat::F32 {
        // 退路：让 cpal 用它的默认格式建流（下面 match 用泛型），此处直接报清楚
        return Err(format!(
            "默认输出格式是 {format:?}，本实现只处理 f32；请把系统输出设为 f32 或指定其它设备"
        ));
    }
    device
        .build_output_stream(
            config.clone(),
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| fill(data, info),
            err_fn,
            None,
        )
        .map_err(|e| format!("建立输出流失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 路径决策：`--audio` 优先、`off` 明确不要、谱面字段相对**谱面所在目录**解析
    #[test]
    fn source_resolution_rules() {
        let chart = Path::new("/charts/song/track.opm.json");
        // ① `--audio` 明确关掉
        for off in ["off", "none", ""] {
            assert_eq!(
                resolve_source(Some(off), Some("song.ogg"), Some(chart)).unwrap(),
                None,
                "--audio {off:?} 表示不要音频"
            );
        }
        // ② `--audio` 显式指定：原样用它（**不**拼谱面目录），来源标成 CLI
        let (p, src) = resolve_source(Some("/music/a.wav"), Some("song.ogg"), Some(chart))
            .unwrap()
            .unwrap();
        assert_eq!(p, PathBuf::from("/music/a.wav"));
        assert_eq!(src, Source::Cli);
        assert_eq!(src.label(), "--audio");
        // ③ 没给 `--audio` ⇒ 用谱面字段；**相对路径按谱面目录**（容器里就是 `song.ogg` 这种裸名）
        let (p, src) = resolve_source(None, Some("song.ogg"), Some(chart)).unwrap().unwrap();
        assert_eq!(p, PathBuf::from("/charts/song/song.ogg"), "相对谱面自身，不是相对 CWD");
        assert_eq!(src, Source::Meta);
        assert_eq!(src.label(), "谱面 meta.audio");
        // ④ 谱面字段是绝对路径：原样
        let (p, _) = resolve_source(None, Some("/abs/b.ogg"), Some(chart)).unwrap().unwrap();
        assert_eq!(p, PathBuf::from("/abs/b.ogg"));
        // ⑤ 谱面还没保存过（没有路径）：保持原样，交给系统按 CWD 解释
        let (p, _) = resolve_source(None, Some("c.ogg"), None).unwrap().unwrap();
        assert_eq!(p, PathBuf::from("c.ogg"));
        // ⑥ 谁都没给 / 字段是空白 ⇒ 没有音频（不是错误）
        assert_eq!(resolve_source(None, None, Some(chart)).unwrap(), None);
        assert_eq!(resolve_source(None, Some("   "), Some(chart)).unwrap(), None);
    }
}
