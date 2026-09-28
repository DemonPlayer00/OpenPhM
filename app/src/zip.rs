//! 最小 ZIP 读写（**自研，不引依赖**）—— opm 的容器形态需要它。
//!
//! 为什么自己写：本机对 crates.io 不可达（`cargo add flate2` 拿不到包），而 opm 的容器格式是
//! 硬需求。ZIP 的**写入**只用 STORE（不压缩）就完全合规，几十行；**读取**要能打开别人做的包
//! （RPE 生态的 `.pez`、系统 `zip` 命令、Python `zipfile` 默认都是 DEFLATE），所以 inflate
//! 也得自己实现一份。
//!
//! 覆盖范围（够用且明确，不含糊）：单文件 ≤ 4 GiB 的 ZIP（本地头 + 中央目录 + EOCD），
//! 压缩方法 **0=STORE / 8=DEFLATE**（含 fixed/dynamic Huffman 与 stored block），
//! 不支持 ZIP64、加密、多卷 —— 遇到就**明确报错**，不猜。
//!
//! 目录/编码：文件名按 UTF-8 读；若通用位标记未置 UTF-8 位且字节不是合法 UTF-8，则退回 CP437
//! 的**保真**做法：原样保存字节（这里用 lossy 显示，但读写都以字节为准）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 一个条目（名字 + 原始内容）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub data: Vec<u8>,
}

/// 写一个 ZIP（全部 STORE，不压缩）。
///
/// 参数里给的时间戳统一用 1980-01-01（DOS 时间 0）：**刻意不写当前时间** ——
/// 同样的输入产出同样的字节，否则每次保存都是一个新的二进制（diff/校验/缓存全废）。
pub fn write_store(entries: &[Entry]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();
    for e in entries {
        let name = e.name.as_bytes();
        let crc = crc32(&e.data);
        let size = e.data.len() as u32;
        let offset = out.len() as u32;
        // 本地文件头
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0x0800u16.to_le_bytes()); // flags: UTF-8 名字
        out.extend_from_slice(&0u16.to_le_bytes()); // method = STORE
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0x0021u16.to_le_bytes()); // date = 1980-01-01
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name);
        out.extend_from_slice(&e.data);
        // 中央目录项
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0x0800u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0x0021u16.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra
        central.extend_from_slice(&0u16.to_le_bytes()); // comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }
    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    // EOCD
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

/// 是不是一个 ZIP（本地头/EOCD 魔法）。用来**按内容**判断 opm 容器 vs 裸 JSON。
pub fn looks_like_zip(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && (bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06"))
}

/// 读一个 ZIP，返回全部条目（顺序同中央目录）。
pub fn read(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let eocd = find_eocd(bytes).ok_or("不是 ZIP（没找到 EOCD）")?;
    let count = u16::from_le_bytes([bytes[eocd + 10], bytes[eocd + 11]]) as usize;
    let cd_size = u32::from_le_bytes([
        bytes[eocd + 12],
        bytes[eocd + 13],
        bytes[eocd + 14],
        bytes[eocd + 15],
    ]) as usize;
    let cd_off = u32::from_le_bytes([
        bytes[eocd + 16],
        bytes[eocd + 17],
        bytes[eocd + 18],
        bytes[eocd + 19],
    ]) as usize;
    if cd_off + cd_size > bytes.len() {
        return Err("中央目录越界（文件被截断？）".to_owned());
    }
    let mut out = Vec::with_capacity(count);
    let mut p = cd_off;
    for i in 0..count {
        if p + 46 > bytes.len() {
            return Err(format!("中央目录第 {i} 项越界"));
        }
        let sig = u32::from_le_bytes([bytes[p], bytes[p + 1], bytes[p + 2], bytes[p + 3]]);
        if sig != 0x0201_4b50 {
            return Err(format!("中央目录第 {i} 项签名不对（0x{sig:08x}）"));
        }
        let method = u16::from_le_bytes([bytes[p + 10], bytes[p + 11]]);
        let crc = u32::from_le_bytes([bytes[p + 16], bytes[p + 17], bytes[p + 18], bytes[p + 19]]);
        let csize = u32::from_le_bytes([bytes[p + 20], bytes[p + 21], bytes[p + 22], bytes[p + 23]])
            as usize;
        let usize_ =
            u32::from_le_bytes([bytes[p + 24], bytes[p + 25], bytes[p + 26], bytes[p + 27]]) as usize;
        let name_len = u16::from_le_bytes([bytes[p + 28], bytes[p + 29]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[p + 30], bytes[p + 31]]) as usize;
        let comment_len = u16::from_le_bytes([bytes[p + 32], bytes[p + 33]]) as usize;
        let lho = u32::from_le_bytes([bytes[p + 42], bytes[p + 43], bytes[p + 44], bytes[p + 45]])
            as usize;
        let name_at = p + 46;
        if name_at + name_len > bytes.len() {
            return Err(format!("中央目录第 {i} 项名字越界"));
        }
        let name = String::from_utf8_lossy(&bytes[name_at..name_at + name_len]).into_owned();
        p = name_at + name_len + extra_len + comment_len;

        // 本地头：名字/扩展区长度可能与中央目录不同，必须按本地头再算一次数据偏移
        if lho + 30 > bytes.len() {
            return Err(format!("条目 {name} 的本地头越界"));
        }
        let lsig = u32::from_le_bytes([bytes[lho], bytes[lho + 1], bytes[lho + 2], bytes[lho + 3]]);
        if lsig != 0x0403_4b50 {
            return Err(format!("条目 {name} 的本地头签名不对"));
        }
        let lname_len = u16::from_le_bytes([bytes[lho + 26], bytes[lho + 27]]) as usize;
        let lextra_len = u16::from_le_bytes([bytes[lho + 28], bytes[lho + 29]]) as usize;
        let data_at = lho + 30 + lname_len + lextra_len;
        if data_at + csize > bytes.len() {
            return Err(format!("条目 {name} 的数据越界"));
        }
        let raw = &bytes[data_at..data_at + csize];
        let data = match method {
            0 => raw.to_vec(),
            8 => inflate(raw, usize_)?,
            other => {
                return Err(format!(
                    "条目 {name} 用了压缩方法 {other}（只支持 0=STORE / 8=DEFLATE）"
                ))
            }
        };
        if data.len() != usize_ {
            return Err(format!(
                "条目 {name} 解压后长度不符（期望 {usize_}，得到 {}）",
                data.len()
            ));
        }
        if crc32(&data) != crc {
            return Err(format!("条目 {name} CRC 校验失败（文件损坏）"));
        }
        out.push(Entry { name, data });
    }
    Ok(out)
}

/// 按名字取（大小写敏感；容器里名字是我们自己写的）
pub fn get<'a>(entries: &'a [Entry], name: &str) -> Option<&'a [u8]> {
    entries.iter().find(|e| e.name == name).map(|e| e.data.as_slice())
}

fn find_eocd(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 22 {
        return None;
    }
    // EOCD 在最后 22 字节，或更靠前（有注释时）；从尾部往前找，最多 64 KiB 注释
    let start = bytes.len().saturating_sub(22 + 65535);
    (start..=bytes.len() - 22)
        .rev()
        .find(|&i| bytes[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
}

// ---------------------------------------------------------------- CRC32

fn crc_table() -> &'static [u32; 256] {
    static T: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    })
}

/// 标准 CRC-32（ZIP 用的那个）
pub fn crc32(data: &[u8]) -> u32 {
    let t = crc_table();
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------- inflate（DEFLATE 解压）

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
    acc: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, bit: 0, acc: 0 }
    }
    fn need(&mut self, n: u32) -> Result<(), String> {
        while self.bit < n {
            let b = *self.data.get(self.pos).ok_or("DEFLATE 数据提前结束")?;
            self.pos += 1;
            self.acc |= (b as u32) << self.bit;
            self.bit += 8;
        }
        Ok(())
    }
    fn bits(&mut self, n: u32) -> Result<u32, String> {
        if n == 0 {
            return Ok(0);
        }
        self.need(n)?;
        let v = self.acc & ((1u32 << n) - 1);
        self.acc >>= n;
        self.bit -= n;
        Ok(v)
    }
    /// 丢弃到字节边界（stored block 用）
    fn align(&mut self) {
        let drop = self.bit % 8;
        self.acc >>= drop;
        self.bit -= drop;
    }
}

/// 一个 Huffman 表：`(长度, 符号)` 列表 + 快速查表
struct Huffman {
    /// counts[len] = 该长度的码字数
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Self {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        // 各长度的起始码字（规范里的 canonical 顺序）
        let mut offs = [0u16; 16];
        for i in 1..16 {
            offs[i] = offs[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Self { counts, symbols }
    }

    fn decode(&self, br: &mut BitReader) -> Result<u16, String> {
        let mut code = 0u32;
        let mut first = 0u32;
        let mut index = 0u32;
        for len in 1..16 {
            code |= br.bits(1)?;
            let count = self.counts[len] as u32;
            if code < first + count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("Huffman 码不合法".to_owned())
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// DEFLATE 解压（RFC 1951）。`hint` 是预期输出长度，仅用于预分配。
pub fn inflate(data: &[u8], hint: usize) -> Result<Vec<u8>, String> {
    let mut br = BitReader::new(data);
    let mut out: Vec<u8> = Vec::with_capacity(hint.max(64));
    loop {
        let last = br.bits(1)? == 1;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                br.align();
                // stored：LEN/NLEN 是字节对齐的
                let len = br.bits(16)? as usize;
                let nlen = br.bits(16)? as usize;
                if len ^ 0xFFFF != nlen {
                    return Err("stored block 的 LEN/NLEN 不互补".to_owned());
                }
                for _ in 0..len {
                    out.push(br.bits(8)? as u8);
                }
            }
            1 => {
                let (lit, dist) = fixed_tables();
                inflate_block(&mut br, &lit, &dist, &mut out)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut br)?;
                inflate_block(&mut br, &lit, &dist, &mut out)?;
            }
            _ => return Err("DEFLATE: btype=3 是保留值".to_owned()),
        }
        if last {
            return Ok(out);
        }
    }
}

fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit_lens = vec![0u8; 288];
    for (i, l) in lit_lens.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    (Huffman::new(&lit_lens), Huffman::new(&[5u8; 30]))
}

fn dynamic_tables(br: &mut BitReader) -> Result<(Huffman, Huffman), String> {
    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    let mut cl_lens = [0u8; 19];
    for &idx in ORDER.iter().take(hclen) {
        cl_lens[idx] = br.bits(3)? as u8;
    }
    let cl = Huffman::new(&cl_lens);
    let mut lengths: Vec<u8> = Vec::with_capacity(hlit + hdist);
    while lengths.len() < hlit + hdist {
        let sym = cl.decode(br)?;
        match sym {
            0..=15 => lengths.push(sym as u8),
            16 => {
                let prev = *lengths.last().ok_or("码长重复但没有前一个码长")?;
                let n = 3 + br.bits(2)? as usize;
                lengths.extend(std::iter::repeat(prev).take(n));
            }
            17 => {
                let n = 3 + br.bits(3)? as usize;
                lengths.extend(std::iter::repeat(0u8).take(n));
            }
            18 => {
                let n = 11 + br.bits(7)? as usize;
                lengths.extend(std::iter::repeat(0u8).take(n));
            }
            other => return Err(format!("码长表的符号 {other} 非法")),
        }
    }
    lengths.truncate(hlit + hdist);
    Ok((
        Huffman::new(&lengths[..hlit]),
        Huffman::new(&lengths[hlit..]),
    ))
}

fn inflate_block(
    br: &mut BitReader,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    loop {
        let sym = lit.decode(br)?;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()),
            257..=285 => {
                let i = (sym - 257) as usize;
                let len = LEN_BASE[i] as usize + br.bits(LEN_EXTRA[i] as u32)? as usize;
                let ds = dist.decode(br)? as usize;
                if ds >= DIST_BASE.len() {
                    return Err("距离符号越界".to_owned());
                }
                let d = DIST_BASE[ds] as usize + br.bits(DIST_EXTRA[ds] as u32)? as usize;
                if d == 0 || d > out.len() {
                    return Err("回溯距离超出已输出内容".to_owned());
                }
                let start = out.len() - d;
                for k in 0..len {
                    let b = out[start + k];
                    out.push(b);
                }
            }
            other => return Err(format!("字面量符号 {other} 非法")),
        }
    }
}

// ---------------------------------------------------------------- 后端：系统 7z / 内置实现

/// 打包/解压后端。**优先系统 `7z`**：它久经考验，deflate/zip64/文件名编码这些都它管；
/// 内置实现作为**退化路径**（没装 7z 的机器、以及"不依赖外部程序"的场合）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// 系统 7z（`7z`/`7za`/`7zr`）
    SevenZip,
    /// 内置实现（STORE 写 + STORE/DEFLATE 读）
    Builtin,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::SevenZip => "7z（系统）",
            Backend::Builtin => "内置 zip 实现",
        }
    }
}

/// 候选路径（**纯函数**：把"哪个平台、环境变量是什么"作为输入，于是 Windows 那份也能在 Linux 上单测）
///
/// Windows 上 7-Zip 装好后**不一定在 PATH 里**（安装器默认只装到 `Program Files`，GUI 用 7zFM），
/// 所以除了 PATH 还要探常见安装目录；Unix 上 `7z` 一般就在 PATH（`7zip` 包）。
pub fn seven_zip_candidates(
    is_windows: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let names: &[&str] = if is_windows {
        &["7z.exe", "7za.exe", "7zr.exe", "7z", "7za"]
    } else {
        &["7z", "7za", "7zr"]
    };
    // PATH 的分隔符按**目标平台**取（Windows 是 `;`）——不能用 `std::env::split_paths`：
    // 它按**宿主**平台的规则切，于是在 Linux 上模拟 Windows 会切错（这个测试当场抓到了）。
    let sep = if is_windows { ';' } else { ':' };
    if let Some(path) = env("PATH") {
        for dir in path.split(sep).filter(|d| !d.trim().is_empty()) {
            let dir = PathBuf::from(dir);
            for n in names {
                out.push(dir.join(n));
            }
        }
    }
    if is_windows {
        for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = env(var) {
                let base = PathBuf::from(base);
                let sub = if var == "LOCALAPPDATA" { "Programs/7-Zip" } else { "7-Zip" };
                out.push(base.join(sub).join("7z.exe"));
                out.push(base.join(sub).join("7za.exe"));
            }
        }
    } else {
        for p in ["/usr/bin/7z", "/usr/local/bin/7z", "/usr/bin/7za", "/opt/homebrew/bin/7z"] {
            out.push(PathBuf::from(p));
        }
    }
    out
}

/// 能不能**真的调用**它（不是"文件在不在"）：
/// 起进程、跑到结束、能拿到输出就算可用。装了一半/权限不对/架构不符都会在这里露出来。
///
/// Windows 上给子进程加 `CREATE_NO_WINDOW`：否则每次探测都会闪一个控制台窗口。
pub fn can_invoke(prog: &Path) -> bool {
    let mut cmd = std::process::Command::new(prog);
    cmd.arg("i"); // `7z i` 打印编解码器信息并退出 0；比无参数更"确定不会弹交互"
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    // **ETXTBSY 要重试**（Linux 报 "Text file busy"）。
    //
    // 为什么：内核拒绝 exec "正被以写方式打开"的文件。多线程程序里，一个线程刚写完这个文件、
    // 另一个线程恰好在同一瞬间 `fork()`，**子进程会继承那个写 fd** ⇒ 紧接着的 exec 撞上 ETXTBSY。
    // 这不是"这个 7z 不可用"，只是撞了一下。实测：zip 那组测试不重试时约 1/12 概率假失败
    // （`detection_requires_an_invocable_program` 报"可执行的假 7z 应判为可用"）。
    for attempt in 0..CAN_INVOKE_ATTEMPTS {
        match cmd.status() {
            Ok(st) => return st.success(),
            Err(e) if is_text_file_busy(&e) && attempt + 1 < CAN_INVOKE_ATTEMPTS => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return false,
        }
    }
    false
}

/// 最多试几次（20 ms 一次 ⇒ 最多约 100 ms；启动探测不该为这个卡住）
const CAN_INVOKE_ATTEMPTS: usize = 5;

/// `ETXTBSY`：Linux 上"文件正被以写方式打开"（撞一下就好的那种错误）
fn is_text_file_busy(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(26)
}

#[cfg(test)]
mod can_invoke_retry_tests {
    use super::*;

    /// 只有 ETXTBSY 值得重试；权限不足之类重试一百次也没用（**别把真错误拖成启动变慢**）
    #[test]
    fn only_text_file_busy_is_retried() {
        assert!(is_text_file_busy(&std::io::Error::from_raw_os_error(26)));
        assert!(!is_text_file_busy(&std::io::Error::from_raw_os_error(13)), "EACCES 不该重试");
        assert!(!is_text_file_busy(&std::io::Error::from_raw_os_error(2)), "ENOENT 不该重试");
        assert!(!is_text_file_busy(&std::io::Error::other("x")));
        assert!(CAN_INVOKE_ATTEMPTS >= 2, "至少要有一次重试的机会");
    }
}

/// 探测可用的 7z（返回**路径**，因为 Windows 上可能是绝对路径）。
///
/// 环境变量 `OPM_7Z` 可以**显式指定** 7z 的位置（非标准安装、便携版、CI）：
/// 指定了就只认它 —— 免得"我明明指了却还是用了别的那个"。
pub fn seven_zip_program() -> Option<PathBuf> {
    let env = |k: &str| std::env::var(k).ok();
    seven_zip_program_from(cfg!(windows), &env)
}

/// 探测的可测版本：平台与环境变量作为输入（于是两条分支都能在 Linux 上单测）
pub fn seven_zip_program_from(
    is_windows: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    if let Some(explicit) = env("OPM_7Z") {
        let p = PathBuf::from(explicit.trim());
        return if p.is_file() && can_invoke(&p) { Some(p) } else { None };
    }
    for c in seven_zip_candidates(is_windows, env) {
        if c.is_file() && can_invoke(&c) {
            return Some(c);
        }
    }
    None
}

/// 7z 是否可用（启动时的检查用它）
pub fn seven_zip_available() -> bool {
    seven_zip_program().is_some()
}

/// 没装 7z 时给用户看的安装提示（Linux/macOS 的包名与 Windows 的下载页）
pub fn install_hint() -> &'static str {
    if cfg!(windows) {
        "Windows：到 https://www.7-zip.org/download.html 下载安装（装完 7z.exe 在 `C:\\Program Files\\7-Zip\\`）"
    } else if cfg!(target_os = "macos") {
        "macOS：`brew install sevenzip`（或 p7zip）"
    } else {
        "Linux：装 `7zip` 包（Arch: `sudo pacman -S 7zip`；Debian/Ubuntu: `sudo apt install 7zip`）"
    }
}

/// 7z 官网（Windows 上"获取 7z"按钮打开它）
pub const SEVEN_ZIP_URL: &str = "https://www.7-zip.org/download.html";

/// 首选后端：装了 7z 就用它
pub fn detect_backend() -> Backend {
    if seven_zip_available() {
        Backend::SevenZip
    } else {
        Backend::Builtin
    }
}

/// 打包（首选后端）
pub fn pack_preferred(files: &[Entry]) -> Result<(Vec<u8>, Backend), String> {
    let b = detect_backend();
    Ok((pack_with(b, files)?, b))
}

/// 解包（首选后端）
pub fn unpack_preferred(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    unpack_with(detect_backend(), bytes)
}

/// 用指定后端打包。名字里带 `/` 的会被放成子目录。
///
/// 7z 那条路会**分两遍**：文本类（`.json` 等）用 Deflate，媒体（音乐/图片，本来就是压缩格式）
/// 用 Copy 直接存 —— 对已压缩的数据再 deflate 是白烧 CPU 换不到体积。
pub fn pack_with(backend: Backend, files: &[Entry]) -> Result<Vec<u8>, String> {
    match backend {
        Backend::Builtin => Ok(write_store(files)),
        Backend::SevenZip => seven_zip_pack(files),
    }
}

/// 用指定后端解包
pub fn unpack_with(backend: Backend, bytes: &[u8]) -> Result<Vec<Entry>, String> {
    match backend {
        Backend::Builtin => read(bytes),
        Backend::SevenZip => seven_zip_unpack(bytes).or_else(|e| {
            // 7z 不在/失败时退回内置：两个都失败才把 7z 的错报出去（它更能说明问题）
            read(bytes).map_err(|builtin| format!("7z: {e}；内置: {builtin}"))
        }),
    }
}

fn is_texty(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".json", ".txt", ".md", ".yaml", ".yml", ".xml", ".csv", ".opm"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// 临时工作目录（每次调用一个，退出前删）
fn temp_work_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("opm-{tag}-{}-{n}", std::process::id()))
}

fn write_work_file(dir: &Path, name: &str, data: &[u8]) -> Result<(), String> {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败 {}: {e}", parent.display()))?;
    }
    std::fs::write(&p, data).map_err(|e| format!("写临时文件失败 {}: {e}", p.display()))
}

fn seven_zip_pack(files: &[Entry]) -> Result<Vec<u8>, String> {
    let prog = seven_zip_program().ok_or_else(|| format!("系统里没有可用的 7z。{}", install_hint()))?;
    let dir = temp_work_dir("zip-pack");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建临时目录失败: {e}"))?;
    let zip_path = dir.join("out.zip");
    let result = (|| -> Result<Vec<u8>, String> {
        for f in files {
            write_work_file(&dir, &f.name, &f.data)?;
        }
        // 共用的确定性开关：**不写时间戳** ⇒ 同样输入产出同样字节（否则每次保存都是新二进制）
        let common = ["-mtm=off", "-mta=off", "-mtc=off", "-bso0", "-bsp0"];
        for (pass, method, subset) in [
            (1, "Deflate", true),
            (2, "Copy", false),
        ] {
            let names: Vec<&str> = files
                .iter()
                .filter(|f| is_texty(&f.name) == subset)
                .map(|f| f.name.as_str())
                .collect();
            if names.is_empty() {
                continue;
            }
            let mut cmd = std::process::Command::new(&prog);
            cmd.current_dir(&dir);
            cmd.args(["a", "-tzip", "-mx=9"]);
            cmd.arg(format!("-mm={method}"));
            cmd.args(common);
            cmd.arg(&zip_path);
            cmd.args(&names);
            if pass == 2 && zip_path.exists() {
                // 第二遍是"追加"：7z 默认就是更新已有包
            }
            let out = cmd.output().map_err(|e| format!("无法启动 {}: {e}", prog.display()))?;
            if !out.status.success() {
                return Err(format!(
                    "7z 第 {pass} 遍失败：{}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
        if !zip_path.exists() {
            return Err("7z 没有产出文件".to_owned());
        }
        std::fs::read(&zip_path).map_err(|e| format!("读回打包结果失败: {e}"))
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn seven_zip_unpack(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let prog = seven_zip_program().ok_or_else(|| format!("系统里没有可用的 7z。{}", install_hint()))?;
    let dir = temp_work_dir("zip-unpack");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建临时目录失败: {e}"))?;
    let result = (|| -> Result<Vec<Entry>, String> {
        let zip_path = dir.join("in.zip");
        std::fs::write(&zip_path, bytes).map_err(|e| format!("写临时 zip 失败: {e}"))?;
        let outdir = dir.join("out");
        let out = std::process::Command::new(&prog)
            .args(["x", "-y", "-bso0", "-bsp0"])
            .arg(format!("-o{}", outdir.display()))
            .arg(&zip_path)
            .output()
            .map_err(|e| format!("无法启动 {}: {e}", prog.display()))?;
        if !out.status.success() {
            return Err(format!(
                "7z 解压失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let mut out_entries = Vec::new();
        walk_files(&outdir, &outdir, &mut out_entries)?;
        out_entries.sort_by(|a, b| a.name.cmp(&b.name)); // 确定性
        Ok(out_entries)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// 递归收集目录下的文件（名字用 `/` 分隔的相对路径；目录本身不产生条目）
fn walk_files(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("读目录失败 {}: {e}", dir.display()))?;
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_files(root, &p, out)?;
        } else if p.is_file() {
            let rel = p
                .strip_prefix(root)
                .map_err(|_| "相对路径算不出来".to_owned())?
                .to_string_lossy()
                .replace('\\', "/");
            let data = std::fs::read(&p).map_err(|e| format!("读 {} 失败: {e}", p.display()))?;
            out.push(Entry { name: rel, data });
        }
    }
    Ok(())
}

/// 便捷：把若干 `(名字, 字节)` 打包（顺序即条目顺序）
pub fn pack(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let entries: Vec<Entry> = files
        .iter()
        .map(|(n, d)| Entry { name: n.clone(), data: d.clone() })
        .collect();
    write_store(&entries)
}

/// 便捷：解包成 map（名字 → 内容），便于按名查资源
pub fn unpack_map(bytes: &[u8]) -> Result<HashMap<String, Vec<u8>>, String> {
    Ok(read(bytes)?
        .into_iter()
        .map(|e| (e.name, e.data))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows 的候选路径（**在 Linux 上也能测**：把"哪个平台 + 环境变量"当输入）
    #[test]
    fn windows_candidates_cover_path_and_install_dirs() {
        let env = |k: &str| -> Option<String> {
            match k {
                "PATH" => Some(r"C:\Windows;C:\Tools".to_owned()),
                "ProgramFiles" => Some(r"C:\Program Files".to_owned()),
                "ProgramFiles(x86)" => Some(r"C:\Program Files (x86)".to_owned()),
                "ProgramW6432" => Some(r"C:\Program Files".to_owned()),
                "LOCALAPPDATA" => Some(r"C:\Users\u\AppData\Local".to_owned()),
                _ => None,
            }
        };
        let c = seven_zip_candidates(true, &env);
        // 在 Linux 上模拟 Windows 时 PathBuf 仍用 `/` 拼，所以断言看**组件**
        let text: Vec<String> = c
            .iter()
            .map(|p| p.display().to_string().replace('\\', "/"))
            .collect();
        assert!(text.iter().any(|p| p.ends_with("Windows/7z.exe")), "{text:?}");
        assert!(text.iter().any(|p| p.ends_with("Tools/7za.exe")), "{text:?}");
        assert!(
            text.iter().any(|p| p.contains("Program Files/7-Zip/7z.exe")),
            "要探常见安装目录（PATH 里没有也能找到）：{text:?}"
        );
        assert!(
            text.iter().any(|p| p.contains("AppData/Local/Programs/7-Zip/7z.exe")),
            "{text:?}"
        );
        // Unix 侧用 Unix 的 PATH（`:` 分隔），候选里不该出现反斜杠
        let env_u = |k: &str| -> Option<String> {
            match k {
                "PATH" => Some("/usr/bin:/bin".to_owned()),
                _ => None, // Unix 上不该去看 ProgramFiles 之类
            }
        };
        let cu = seven_zip_candidates(false, &env_u);
        let textu: Vec<String> = cu.iter().map(|p| p.display().to_string()).collect();
        assert!(textu.iter().all(|p| !p.contains('\\')), "{textu:?}");
        assert!(textu.iter().any(|p| p.ends_with("/usr/bin/7z")), "{textu:?}");
        assert!(textu.iter().any(|p| p.ends_with("/bin/7za")), "{textu:?}");
        assert!(cu.iter().any(|p| p.ends_with("usr/bin/7z") || p.ends_with("bin/7z")));
    }

    /// 显式指定（`OPM_7Z`）优先，且"指了但调不动"就是不可用（不偷偷换别的）
    #[test]
    fn explicit_env_override_wins_and_is_not_silently_replaced() {
        let dir = std::env::temp_dir().join(format!("opm-7z-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 只在下面 unix 那一支里用得到（Windows 上那段整块不编译）—— 显式带上 cfg，
        // 免得 Windows 那份构建多一条 "unused variable" 警告
        #[cfg(unix)]
        let fake = dir.join("my7z");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&fake, b"#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

            let good = fake.display().to_string();
            let env = move |k: &str| -> Option<String> {
                match k {
                    "OPM_7Z" => Some(good.clone()),
                    "PATH" => Some("/usr/bin:/bin".to_owned()),
                    _ => None,
                }
            };
            assert_eq!(seven_zip_program_from(false, &env), Some(fake.clone()));

            let bad = dir.join("not-executable");
            std::fs::write(&bad, b"x").unwrap();
            std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644)).unwrap();
            let bad_s = bad.display().to_string();
            let env_bad = move |k: &str| -> Option<String> {
                match k {
                    "OPM_7Z" => Some(bad_s.clone()),
                    "PATH" => Some("/usr/bin:/bin".to_owned()),
                    _ => None,
                }
            };
            assert_eq!(
                seven_zip_program_from(false, &env_bad),
                None,
                "显式指定的程序调不动就该判为不可用，不能悄悄退回候选列表"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **"能调用"而不是"文件存在"**：PATH 里放个不可执行的同名文件必须被拒
    #[test]
    fn detection_requires_an_invocable_program() {
        let dir = std::env::temp_dir().join(format!("opm-7z-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // ① 不可执行的普通文件 ⇒ 不算可用
        let fake = dir.join("7z");
        std::fs::write(&fake, b"not a program").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert!(!can_invoke(&fake), "不可执行的同名文件不该被判为可用");
        // ② 可执行的假 7z（打印一行就退出）⇒ 可用
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&fake, b"#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(can_invoke(&fake), "可执行的假 7z 应判为可用");
        }
        // ③ 目录当程序 ⇒ 不可用（别把目录当成程序）
        assert!(!can_invoke(&dir));
        // ④ 本机真装没装？装了就必须探到
        if let Some(p) = seven_zip_program() {
            assert!(p.is_file(), "{}", p.display());
            assert!(can_invoke(&p));
        }
        assert!(!install_hint().is_empty());
        assert!(SEVEN_ZIP_URL.starts_with("https://"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **两个后端互验**：7z 打的包内置读得回来，内置打的包 7z 解得开。
    /// 互相印证比"自己读自己写的"强得多 —— 单靠自己往返，一致地写错也一样通过。
    #[test]
    fn seven_zip_and_builtin_agree() {
        if seven_zip_program().is_none() {
            eprintln!("跳过：本机没有 7z");
            return;
        }
        // 谱面用**能压得动**的大 JSON（验证两遍策略里的 Deflate），媒体用不可压的随机字节
        // （验证媒体走 Copy —— 对已压缩数据再 deflate 是白烧 CPU）
        let big: String = (0..4000)
            .map(|i| format!("{{\"i\":{i},\"x\":{},\"name\":\"note-{i}\"}}\n", i as f64 * 1.25))
            .collect();
        let media: Vec<u8> = (0..20000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let files: Vec<Entry> = vec![
            Entry { name: "opm.json".to_owned(), data: big.clone().into_bytes() },
            Entry { name: "extra/readme.txt".to_owned(), data: b"hello 7z".to_vec() },
            Entry { name: "song.ogg".to_owned(), data: media.clone() },
        ];
        // ① 7z 打包 → 内置读
        let (z7, backend) = pack_preferred(&files).unwrap();
        assert_eq!(backend, Backend::SevenZip);
        let by_builtin = read(&z7).expect("内置要能读 7z 打的包");
        for f in &files {
            assert_eq!(get(&by_builtin, &f.name).unwrap(), &f.data, "{} 内容不一致", f.name);
        }
        // ② 内置打包 → 7z 解
        let zb = pack_with(Backend::Builtin, &files).unwrap();
        let by_7z = seven_zip_unpack(&zb).expect("7z 要能解开内置打的包");
        for f in &files {
            assert!(
                by_7z.iter().any(|e| e.name == f.name && e.data == f.data),
                "7z 解出来的 {} 不一致",
                f.name
            );
        }
        // ③ 谱面是文本 ⇒ 7z 的包应显著更小（这条同时验证"两遍"确实生效）
        assert!(
            z7.len() + 4000 < zb.len(),
            "7z 压缩后应显著更小：{} vs {}",
            z7.len(),
            zb.len()
        );
        // ④ 两遍策略要真的按文件区分：谱面 Deflate、媒体 Store
        let prog = seven_zip_program().unwrap();
        let dir = temp_work_dir("zip-method-test");
        std::fs::create_dir_all(&dir).unwrap();
        let zp = dir.join("m.zip");
        std::fs::write(&zp, &z7).unwrap();
        let listing = std::process::Command::new(prog.as_path())
            .args(["l", "-slt"])
            .arg(&zp)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&listing.stdout).into_owned();
        let _ = std::fs::remove_dir_all(&dir);
        let method_of = |name: &str| -> String {
            let mut lines = text.lines();
            let mut cur = String::new();
            let mut method = String::new();
            for l in lines.by_ref() {
                if let Some(p) = l.strip_prefix("Path = ") {
                    cur = p.to_owned();
                }
                if let Some(m) = l.strip_prefix("Method = ") {
                    if cur.ends_with(name) {
                        method = m.to_owned();
                        break;
                    }
                }
            }
            method
        };
        assert_eq!(method_of("opm.json"), "Deflate", "谱面应该被压缩");
        assert_eq!(method_of("song.ogg"), "Store", "媒体应该直存（Copy）");
        // ⑤ 确定性：同样的输入、同样的字节
        let again = pack_with(Backend::SevenZip, &files).unwrap();
        assert_eq!(z7, again, "7z 打包必须字节确定（-mtm/-mta/-mtc=off）");
    }

    #[test]
    fn crc32_matches_reference_values() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926, "CRC-32/ISO-HDLC 标准向量");
        assert_eq!(crc32(b"hello"), 0x3610_a686);
    }

    /// 自己写的 STORE 包自己读得回来
    #[test]
    fn store_roundtrip() {
        let files = vec![
            ("opm.json".to_owned(), b"{\"format\":\"opm\"}".to_vec()),
            ("song.ogg".to_owned(), vec![0u8, 1, 2, 255, 254]),
            ("bg.png".to_owned(), b"\x89PNG\r\n\x1a\n".to_vec()),
        ];
        let z = pack(&files);
        assert!(looks_like_zip(&z));
        let back = read(&z).expect("自研读回");
        assert_eq!(back.len(), 3);
        assert_eq!(back[0].name, "opm.json");
        assert_eq!(back[1].data, vec![0u8, 1, 2, 255, 254]);
        assert_eq!(get(&back, "bg.png").unwrap(), b"\x89PNG\r\n\x1a\n");
        // 头部固定（时间戳写死 1980）⇒ 同样输入产出同样字节
        assert_eq!(z, pack(&files), "同样的输入必须产出同样的字节");
    }

    /// **解压别人做的包**：交给 Python 的 zipfile 造 STORE 与 DEFLATE 两种，我们解出来必须一致
    #[test]
    fn inflate_reads_python_made_zips() {
        let dir = std::env::temp_dir().join(format!("opm-zip-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let payload = "谱面测试 payload — 中文与重复片段重复片段重复片段 abcabcabc".repeat(40);
        let py = format!(
            r#"
import zipfile, sys
p = sys.argv[1]; data = {payload:?}.encode()
for name, comp in (("store.zip", zipfile.ZIP_STORED), ("deflate.zip", zipfile.ZIP_DEFLATED)):
    with zipfile.ZipFile(p + "/" + name, "w", comp) as z:
        z.writestr("opm.json", "{{\"format\":\"opm\",\"n\":1}}")
        z.writestr("assets/song.bin", data)
print("ok")
"#,
            payload = payload
        );
        let script = dir.join("make.py");
        std::fs::write(&script, py).unwrap();
        let out = std::process::Command::new("python3")
            .arg(&script)
            .arg(&dir)
            .output()
            .expect("需要 python3 造测试 zip");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

        for name in ["store.zip", "deflate.zip"] {
            let bytes = std::fs::read(dir.join(name)).unwrap();
            let entries = read(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(entries.len(), 2, "{name}");
            let song = get(&entries, "assets/song.bin").unwrap();
            assert_eq!(song, payload.as_bytes(), "{name} 解压内容不一致");
            assert_eq!(get(&entries, "opm.json").unwrap(), b"{\"format\":\"opm\",\"n\":1}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 坏输入要**明确报错**，不能猜
    #[test]
    fn broken_inputs_are_rejected() {
        assert!(read(b"not a zip at all").is_err());
        assert!(!looks_like_zip(b"{\"format\":\"opm\"}"));
        let mut z = pack(&[("a.txt".to_owned(), b"hello world".to_vec())]);
        let n = z.len();
        z.truncate(n - 8); // 截断
        assert!(read(&z).is_err(), "截断的包必须报错");
        // CRC 被改坏
        let mut z = pack(&[("a.txt".to_owned(), b"hello world".to_vec())]);
        // 数据从第 30+5 字节开始（本地头 30 + 名字 5）
        z[35] ^= 0xFF;
        let err = read(&z).unwrap_err();
        assert!(err.contains("CRC"), "{err}");
    }

    /// inflate 的 stored block 分支（Python 的 ZIP_STORED 不走 DEFLATE，
    /// 这里手工构造一个只含 stored block 的 DEFLATE 流）
    #[test]
    fn inflate_handles_stored_blocks() {
        // BFINAL=1, BTYPE=00, 然后是对齐后的 LEN/NLEN 与数据
        let mut raw = vec![0b0000_0001u8];
        let data = b"hello stored block";
        raw.extend_from_slice(&(data.len() as u16).to_le_bytes());
        raw.extend_from_slice(&(!(data.len() as u16)).to_le_bytes());
        raw.extend_from_slice(data);
        assert_eq!(inflate(&raw, 0).unwrap(), data);
    }

}
