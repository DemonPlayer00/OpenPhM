//! 音频模块的验收：WAV 解码、声道/采样率对齐、以及**帧/样本单位**（那个 2× bug 的回归测试）。
//!
//! 这里不打开真实输出设备（CI/无声环境也要能跑）：只测纯函数与解码。

use opm_app::audio::{decode, fill_frames, FillStatus};
use std::io::Write;
use std::path::PathBuf;

/// 写进临时目录的测试文件 —— **用完要自己清掉**：原先只写不删，跑一次 `cargo test` 就在
/// `/tmp` 落 2 个 wav，累积到实测 162 个（跨几十次运行）。`Drop` 里删，断言 panic 也不会漏。
struct TmpWav(PathBuf);

impl TmpWav {
    fn new(name: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!("opm-audio-test-{}-{name}", std::process::id()));
        Self(p)
    }
    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TmpWav {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 手写一个最小 WAV（绕开"用被测代码造测试数据"的循环）
fn write_wav(path: &PathBuf, channels: u16, rate: u32, bits: u16, frames: &[Vec<f32>]) {
    let mut data: Vec<u8> = Vec::new();
    for f in frames {
        for v in f {
            match bits {
                16 => data.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes()),
                32 => data.extend_from_slice(&v.to_le_bytes()),
                _ => panic!("测试没实现这种位深"),
            }
        }
    }
    let fmt_tag: u16 = if bits == 32 { 3 } else { 1 };
    let byte_rate = rate * channels as u32 * (bits as u32 / 8);
    let block_align = channels * (bits / 8);
    let mut h: Vec<u8> = Vec::new();
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
    h.extend_from_slice(b"WAVE");
    h.extend_from_slice(b"fmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&fmt_tag.to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&byte_rate.to_le_bytes());
    h.extend_from_slice(&block_align.to_le_bytes());
    h.extend_from_slice(&bits.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&(data.len() as u32).to_le_bytes());
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(&h).unwrap();
    f.write_all(&data).unwrap();
}

#[test]
fn decodes_wav_int_and_float() {
    let frames: Vec<Vec<f32>> = (0..100).map(|i| vec![(i as f32 / 100.0) * 2.0 - 1.0]).collect();
    let p16 = TmpWav::new("pcm16.wav");
    write_wav(p16.path(), 1, 48000, 16, &frames);
    let w = decode(p16.path()).unwrap();
    assert_eq!((w.channels, w.rate), (1, 48000));
    assert_eq!(w.samples.len(), 100);
    for (a, b) in w.samples.iter().zip(&frames) {
        assert!((a - b[0]).abs() < 1e-3, "{a} vs {}", b[0]);
    }

    let stereo: Vec<Vec<f32>> = frames.iter().map(|f| vec![f[0], -f[0]]).collect();
    let pf = TmpWav::new("f32.wav");
    write_wav(pf.path(), 2, 44100, 32, &stereo);
    let wf = decode(pf.path()).unwrap();
    assert_eq!((wf.channels, wf.rate), (2, 44100));
    assert_eq!(wf.samples.len(), 200, "立体声 ⇒ 样本数 = 帧数 × 声道数");
    assert_eq!(wf.samples[0], -wf.samples[1], "交错顺序不能错（左右声道各自保留）");
}

/// 回归测试：**帧 ≠ 样本**。
///
/// 立体声下若把"样本下标"当帧数，游标会走得正好快一倍（`游标/采样率` 于是 2×），
/// 表现是播放头跑得比音频快一倍 —— 这个 bug 真的发生过，所以钉一条断言。
#[test]
fn fill_frames_counts_frames_not_samples() {
    let frames = 480; // 10 ms @48k
    let ch = 2;
    let samples: Vec<f32> = (0..frames * ch).map(|i| i as f32).collect();

    let mut out = vec![0.0f32; frames * ch];
    let (next, status) = fill_frames(&samples, ch, 0, true, &mut out);
    assert_eq!(next, frames, "480 帧请求 ⇒ 游标前进 480（不是 960）");
    assert_eq!(status, None);
    // 交错顺序必须原样搬过去（L,R,L,R…）
    for (i, v) in out.iter().enumerate() {
        assert_eq!(*v, i as f32, "第 {i} 个样本应原样搬过去");
    }
    // 再填一次：游标从末尾之后开始 ⇒ 判定为播完
    let (next2, st2) = fill_frames(&samples, ch, next, true, &mut out);
    assert_eq!(next2, frames);
    assert_eq!(st2, Some(FillStatus::Ended));
    assert!(out.iter().all(|v| *v == 0.0), "播完之后应输出静音");
}

#[test]
fn fill_frames_paused_does_not_advance() {
    let samples: Vec<f32> = vec![1.0; 480 * 2];
    let mut out = vec![0.0f32; 480 * 2];
    let (next, status) = fill_frames(&samples, 2, 100, false, &mut out);
    assert_eq!((next, status), (100, None), "暂停时游标不动、也不报结束");
    assert!(out.iter().all(|v| *v == 0.0), "暂停时应输出静音");
}

#[test]
fn fill_frames_detects_underrun_on_jump_past_end() {
    let samples: Vec<f32> = vec![1.0; 48 * 2];
    let mut out = vec![0.0f32; 48 * 2];
    // 游标被 seek 到超过数据长度的位置：这是真的没数据（欠载），不是正常播完
    let (_, status) = fill_frames(&samples, 2, 999, true, &mut out);
    assert_eq!(status, Some(FillStatus::Underrun));
}

/// 空格的判定规则：**现在有库函数了**，别再在测试里抄一份规则。
///
/// 这条测试以前只能在本地写个 `|kb, sp| sp && !kb` 复刻一遍（因为规则在 `main.rs` 里拿不到）——
/// 一旦实现改了，复刻的副本不会跟着改，测试反而变成"保证旧行为"的锁。
/// 现在规则在 `keymap`：这里直接调真身，键序语义（单点/长按）另由 `keymap` 自己的单测覆盖。
#[test]
fn shortcuts_yield_to_typing_and_use_the_real_rule() {
    use opm_app::keymap::shortcut_allowed;
    assert!(shortcut_allowed(false, false), "正常界面：快捷键可用");
    assert!(!shortcut_allowed(true, false), "控制台输入中 ⇒ 不可用（否则敲 JSON 会跳播）");
    assert!(!shortcut_allowed(false, true), "模态框在上 ⇒ 不可用");
}
