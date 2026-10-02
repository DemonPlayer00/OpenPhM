// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **SHA-256**（标准库实现，只为了给缓存目录算一个"看起来随机、实际上唯一"的键）。
//!
//! 为什么要自己写而不是加个 crate：这里要的只是"同一份输入 ⇒ 同一个 32 字节摘要，
//! 不同输入几乎不可能撞"，而仓库的依赖表一直是**能不加就不加**（内置 ZIP 也是自研的）。
//! 选 SHA-256 而不是更短的哈希，是因为它有**公开测试向量**可核对 —— 自研 FNV 之类的
//! 只能自己跟自己比，而"哈希函数写错了"这件事的后果是**两份不同的谱面落进同一个缓存目录**
//! （互相覆盖对方的未保存快照），属于不能靠"应该没问题"过关的那一类。
//!
//! 用途只有一个：[`crate::codec::container::cache_key`]。**不用于任何安全场景**
//! （不参与凭据、校验或签名）。

/// SHA-256 的 64 个轮常量（FIPS 180-4 §4.2.2，立方根的小数部分前 32 位）
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// 初始哈希值（FIPS 180-4 §5.3.3，前 8 个素数平方根的小数部分前 32 位）
const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// 一份**流式**的 SHA-256 状态（`cache_key` 只需要一次性摘要，留着它是为了不把
/// "分块喂进去"这件事在调用点上写成"先拼一个几百 MB 的 Vec"）。
#[derive(Clone)]
pub struct Sha256 {
    h: [u32; 8],
    /// 还没凑满 64 字节的尾巴
    buf: [u8; 64],
    buf_len: usize,
    /// 已经吃进去多少字节（填充要按它写长度）
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self { h: H0, buf: [0; 64], buf_len: 0, total: 0 }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        // 先把上次的尾巴补齐
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        // 整块直接压（不复制）
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        // 余下的留到下次（或 `finish`）
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    /// 收尾（追加填充与长度）并给出摘要。**可以只用一次**（之后状态就作废了）。
    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        // 填充：一个 0x80，然后补 0 到 56 mod 64，最后 8 字节大端长度
        // `tail[0]` 对应**绝对位置 buf_len**（即缓冲区后面那一个字节）。
        // 长度字段的**尾内下标**是 `len_at`：短情形落在 56（一个块装得下），
        // 长情形落在 120（要再开一个块）—— 这里曾经把"绝对位置"当成了尾内下标，
        // 于是 `buf_len != 0` 的输入全错（空串恰好对，所以只跑空串是发现不了的）。
        let mut tail = [0u8; 128];
        tail[0] = 0x80;
        let len_at = if self.buf_len < 56 { 56 - self.buf_len } else { 120 - self.buf_len };
        tail[len_at..len_at + 8].copy_from_slice(&bits.to_be_bytes());
        let buf_len = self.buf_len;
        let mut first = self.buf;
        first[buf_len..].copy_from_slice(&tail[..64 - buf_len]);
        self.compress(&first);
        if len_at + 8 > 64 - buf_len {
            let mut second = [0u8; 64];
            second.copy_from_slice(&tail[64 - buf_len..128 - buf_len]);
            self.compress(&second);
        }
        let mut out = [0u8; 32];
        for (i, w) in self.h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in self.h.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(v);
        }
    }
}

/// 一次性摘要
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finish()
}

/// 小写十六进制（缓存目录名用）
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(s: &str) -> String {
        hex(&sha256(s.as_bytes()))
    }

    /// **FIPS 180-4 的官方测试向量**（这就是选 SHA-256 而不是自研短哈希的全部理由：
    /// 写错了能当场发现，而不是等到"两份谱面撞进同一个缓存目录"）
    #[test]
    fn official_test_vectors() {
        assert_eq!(
            digest(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 56 字节：填充**跨过**一个块边界（padding 分支的第一条）
        assert_eq!(
            digest("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 112 字节：两段式填充那条分支
        assert_eq!(
            digest(
                "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmno\
                 ijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"
            ),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }

    /// 分块喂进去与一次喂进去**结果相同**（`update` 的尾巴逻辑与 `finish` 的填充是两段代码）
    #[test]
    fn streaming_matches_one_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let want = hex(&sha256(&data));
        for chunk in [1usize, 7, 63, 64, 65, 128, 999] {
            let mut h = Sha256::new();
            for part in data.chunks(chunk) {
                h.update(part);
            }
            assert_eq!(hex(&h.finish()), want, "分块 {chunk}");
        }
    }

    /// 长的输入（多块 + 长度字段真的被写进去）与短输入不会撞
    #[test]
    fn length_is_part_of_the_hash() {
        let a = vec![b'a'; 55];
        let b = vec![b'a'; 56];
        let c = vec![b'a'; 64];
        let d = vec![b'a'; 65];
        let mut seen = std::collections::HashSet::new();
        for v in [&a, &b, &c, &d] {
            assert!(seen.insert(hex(&sha256(v))), "不同长度不该同摘要");
        }
        // 前导零字节也算内容（别把 `Vec<u8>` 当字符串比）
        assert_ne!(hex(&sha256(&[0, 1])), hex(&sha256(&[1])));
    }

    /// 缓存键：32 个十六进制字符（128 位）—— 比原来的 crc32（32 位）宽得多，
    /// "两份不同的谱面落进同一个目录"这件事从"理论上会撞"变成"不必考虑"
    #[test]
    fn cache_key_is_wide_and_stable() {
        let k1 = crate::codec::container::cache_key(b"chart A");
        let k2 = crate::codec::container::cache_key(b"chart A");
        let k3 = crate::codec::container::cache_key(b"chart B");
        assert_eq!(k1, k2, "同一份内容 ⇒ 同一个目录（用户口径：读取相同谱面就会重合）");
        assert_ne!(k1, k3);
        assert_eq!(k1.len(), 32, "{k1}");
        assert!(k1.chars().all(|c| c.is_ascii_hexdigit()), "{k1}");
        assert_eq!(k1, hex(&sha256(b"chart A"))[..32], "取前 128 位");
    }
}
