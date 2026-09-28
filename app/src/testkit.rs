// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **测试公用件**（只在 `cfg(test)` 下编译，见 `lib.rs`）。
//!
//! 为什么要有这个模块：有几段测试助手本来在三个文件里各抄一份 —— "一帧里画出来的所有文本"、
//! "从一对对 `(键, 值)` 造出查环境变量的闭包"、"造一个临时目录"。抄的时候它们一模一样，
//! **改起来就不一样了**：于是同一条断言在三个面板的测试里有三种行为，而用户看到的是三种界面。
//!
//! 判据：一段助手只要在**两个以上**模块里逐字出现，就该搬到这里；只在一个模块里用一次的
//! （哪怕很像别的模块的）留在原处 —— 那叫"各模块自己的夹具"，不叫重复。

use std::path::PathBuf;

/// 一帧里画出来的所有文本（递归走 `Shape`，`Shape::Vec` 里的也算）。
///
/// 用途：断言"这一栏真的画出了那行字"。比截图可靠 —— 截图要人看，文本可以 `assert!`。
pub fn drawn_texts(out: &egui::FullOutput) -> Vec<String> {
    fn walk(shape: &egui::epaint::Shape, acc: &mut Vec<String>) {
        match shape {
            egui::epaint::Shape::Text(t) => acc.push(t.galley.text().to_owned()),
            egui::epaint::Shape::Vec(v) => {
                for s in v {
                    walk(s, acc);
                }
            }
            _ => {}
        }
    }
    let mut acc = Vec::new();
    for cs in &out.shapes {
        walk(&cs.shape, &mut acc);
    }
    acc
}

/// 造一个"查环境变量"的闭包：字体选择与显卡选择都是纯函数（把环境当输入），
/// 单测靠它喂环境，不必真的去改进程的环境变量（那会让测试互相干扰）。
pub fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
    let map: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k: &str| map.iter().find(|(mk, _)| mk == k).map(|(_, v)| v.clone())
}

/// 一个本次测试专用的临时目录：`<临时目录>/opm-<tag>-<pid>-<序号>`，**先清掉再建**。
///
/// 带进程号与序号 ⇒ 同一个测试二进制里并行跑的两个用例不会互相删目录
/// （`cargo test` 默认多线程，这是踩过的：两个用例共用一个路径，一个先 `remove_dir_all`
/// 就把另一个的文件删了，症状是随机失败）。
pub fn tmp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("opm-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("建临时目录");
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录：两次调用**不是一个路径**（并行用例互不干扰），且建好就是空目录
    #[test]
    fn tmp_dirs_are_unique_and_empty() {
        let a = tmp_dir("testkit");
        let b = tmp_dir("testkit");
        assert_ne!(a, b, "同一个 tag 也要给出不同的目录");
        assert!(a.is_dir() && b.is_dir());
        assert_eq!(std::fs::read_dir(&a).unwrap().count(), 0);
        // 预清：先塞个文件再要同一个 tag，不会拿到上一次的残留（这里只能验证"新目录是空的"）
        std::fs::write(a.join("x"), b"x").unwrap();
        let c = tmp_dir("testkit");
        assert_eq!(std::fs::read_dir(&c).unwrap().count(), 0, "新目录不许有残留");
        for d in [a, b, c] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    /// 环境闭包：命中就给值，没命中给 `None`（不是空串）
    #[test]
    fn env_of_looks_up_exact_keys() {
        let env = env_of(&[("OPM_FONT", "system"), ("OPM_X", "")]);
        assert_eq!(env("OPM_FONT"), Some("system".to_owned()));
        assert_eq!(env("OPM_X"), Some(String::new()));
        assert_eq!(env("OPM_NOPE"), None);
        assert_eq!(env("opm_font"), None, "键要区分大小写（环境变量就是这样）");
    }
}
