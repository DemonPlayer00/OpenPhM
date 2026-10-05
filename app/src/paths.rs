// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! **我们的文件放哪** —— 配置目录与本地数据目录，按平台取。
//!
//! 为什么要有这个模块：这套换算原先只有一处（`recents.rs` 里的 `$XDG_CONFIG_HOME` → `~/.config`），
//! 而**那两个环境变量在 Windows 上根本不存在** ⇒ 最近打开列表落到"当前目录"里的
//! `.opm-recents.json`（跟着 exe 到处跑；Wine 实测：启动页那一行显示的就是裸文件名）。
//! 控制通道的命名管道标记文件也要一个同类目录，于是"我们的文件放哪"收成一处 ——
//! 否则每个调用点各答一次，迟早答出两个不一样的答案。
//!
//! | 用途 | Windows | Linux/macOS |
//! |---|---|---|
//! | [`config_dir`]（最近打开列表） | `%APPDATA%\OpenPhM` | `$XDG_CONFIG_HOME/OpenPhM`，退化 `~/.config/OpenPhM` |
//! | [`local_data_dir`]（控制通道标记、缓存） | `%LOCALAPPDATA%\OpenPhM` | `$XDG_DATA_HOME/OpenPhM`，退化 `~/.local/share/OpenPhM` |
//!
//! 两个平台的规则都是**纯函数**（环境由调用方喂进来），所以两条分支在任何一个平台上都能测
//! —— 这正是"Linux 上写的代码在 Windows 上没跑过"要防的那件事。

use std::path::PathBuf;

/// 非空环境变量（空串当没设置：`XDG_CONFIG_HOME=""` 是"清空"而不是"根目录"）
fn env_nonempty(env: &dyn Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    env(key).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// Unix 的配置目录：`$XDG_CONFIG_HOME/OpenPhM` → `$HOME/.config/OpenPhM`
pub fn config_dir_unix(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(dir) = env_nonempty(env, "XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("OpenPhM"));
    }
    env_nonempty(env, "HOME").map(|h| PathBuf::from(h).join(".config").join("OpenPhM"))
}

/// Windows 的配置目录：`%APPDATA%\OpenPhM`（退化到 `%USERPROFILE%\AppData\Roaming\OpenPhM`）。
///
/// `APPDATA` 就是 Windows 的 "Roaming" 配置目录，与 Unix 的 `~/.config` 对位；
/// 老系统/精简环境下 `APPDATA` 可能没设，所以留一条 `USERPROFILE` 的退路。
pub fn config_dir_windows(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(dir) = env_nonempty(env, "APPDATA") {
        return Some(PathBuf::from(dir).join("OpenPhM"));
    }
    env_nonempty(env, "USERPROFILE")
        .map(|h| PathBuf::from(h).join("AppData").join("Roaming").join("OpenPhM"))
}

/// Unix 的本地数据目录：`$XDG_DATA_HOME/OpenPhM` → `$HOME/.local/share/OpenPhM`
pub fn local_data_dir_unix(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(dir) = env_nonempty(env, "XDG_DATA_HOME") {
        return Some(PathBuf::from(dir).join("OpenPhM"));
    }
    env_nonempty(env, "HOME").map(|h| PathBuf::from(h).join(".local").join("share").join("OpenPhM"))
}

/// Windows 的本地数据目录：`%LOCALAPPDATA%\OpenPhM`（退化到 `%USERPROFILE%\AppData\Local\OpenPhM`）。
///
/// 与配置目录的区别（Windows 自己分的）：Roaming 会跟着域账号漫游，**机器本地**的东西
/// （命名管道的标记文件、缓存）该放 Local —— 漫游到一个没有那个进程的机器上，标记文件只会误导。
pub fn local_data_dir_windows(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(dir) = env_nonempty(env, "LOCALAPPDATA") {
        return Some(PathBuf::from(dir).join("OpenPhM"));
    }
    env_nonempty(env, "USERPROFILE")
        .map(|h| PathBuf::from(h).join("AppData").join("Local").join("OpenPhM"))
}

/// 当前平台的配置目录（`None` = 环境里什么都问不到，调用方自己退到当前目录）
pub fn config_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var(k).ok();
    if cfg!(windows) {
        config_dir_windows(&env)
    } else {
        config_dir_unix(&env)
    }
}

/// 当前平台的本地数据目录（见 [`local_data_dir_windows`] 里"为什么不用 Roaming"）
pub fn local_data_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var(k).ok();
    if cfg!(windows) {
        local_data_dir_windows(&env)
    } else {
        local_data_dir_unix(&env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: Vec<(String, String)> =
            pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |k: &str| map.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone())
    }

    /// Linux：XDG 优先，退化到 `~/.config`（与 `recents.rs` 原来那条规则逐字一致）
    #[test]
    fn unix_config_prefers_xdg_then_home() {
        let e = env_of(&[("XDG_CONFIG_HOME", "/xdg/cfg"), ("HOME", "/home/u")]);
        assert_eq!(config_dir_unix(&e), Some(PathBuf::from("/xdg/cfg/OpenPhM")));
        let e = env_of(&[("HOME", "/home/u")]);
        assert_eq!(config_dir_unix(&e), Some(PathBuf::from("/home/u/.config/OpenPhM")));
        // 空串当没设置（`XDG_CONFIG_HOME=""` 是"清空"，不是"放在根目录"）
        let e = env_of(&[("XDG_CONFIG_HOME", "   "), ("HOME", "/home/u")]);
        assert_eq!(config_dir_unix(&e), Some(PathBuf::from("/home/u/.config/OpenPhM")));
        // 什么都问不到 ⇒ None（调用方退到当前目录，而不是造一个 "/OpenPhM"）
        assert_eq!(config_dir_unix(&env_of(&[])), None);
    }

    /// Windows：`%APPDATA%\OpenPhM`；没有 APPDATA 才退到 USERPROFILE。
    ///
    /// 这一条**在 Linux 上也要跑**（所以规则是纯函数）：Windows 分支以前是"写完就没跑过"的那种。
    /// 注意断言怎么写：规则说的是 Windows 路径，但 `PathBuf::join` 在本机按**本机**分隔符拼，
    /// 所以期望值也用 `join` 拼（比对字面量字符串会把 `/` 与 `\` 的差异当成 bug）。
    #[test]
    fn windows_config_uses_appdata() {
        let roaming = r"C:\Users\dp\AppData\Roaming";
        let e = env_of(&[("APPDATA", roaming)]);
        assert_eq!(
            config_dir_windows(&e),
            Some(PathBuf::from(roaming).join("OpenPhM"))
        );
        // 老环境里没有 APPDATA ⇒ 退到 USERPROFILE 下的标准位置
        let e = env_of(&[("USERPROFILE", r"C:\Users\dp")]);
        assert_eq!(
            config_dir_windows(&e),
            Some(PathBuf::from(r"C:\Users\dp").join("AppData").join("Roaming").join("OpenPhM"))
        );
        // 空串同样当没设置
        let e = env_of(&[("APPDATA", " "), ("USERPROFILE", r"C:\Users\dp")]);
        assert_eq!(
            config_dir_windows(&e),
            Some(PathBuf::from(r"C:\Users\dp").join("AppData").join("Roaming").join("OpenPhM"))
        );
        assert_eq!(config_dir_windows(&env_of(&[])), None);
        // **不认识 XDG/HOME**：那是 Unix 的规矩，在 Windows 上照着读只会读到别的东西
        let e = env_of(&[("HOME", r"C:\users\dp"), ("XDG_CONFIG_HOME", "C:\\xdg")]);
        assert_eq!(config_dir_windows(&e), None);
    }

    /// 本地数据目录：Windows 用 **Local**（不是 Roaming）—— 命名管道标记 / 缓存是机器本地的东西
    #[test]
    fn windows_local_data_is_the_local_appdata_not_roaming() {
        let local = r"C:\Users\dp\AppData\Local";
        let e = env_of(&[("APPDATA", r"C:\Roaming"), ("LOCALAPPDATA", local)]);
        assert_eq!(
            local_data_dir_windows(&e),
            Some(PathBuf::from(local).join("OpenPhM")),
            "漫游目录不该用来放管道标记"
        );
        let e = env_of(&[("USERPROFILE", r"C:\Users\dp")]);
        assert_eq!(
            local_data_dir_windows(&e),
            Some(PathBuf::from(r"C:\Users\dp").join("AppData").join("Local").join("OpenPhM"))
        );
        assert_eq!(local_data_dir_windows(&env_of(&[])), None);
    }

    /// Unix 的本地数据目录（同样是纯函数，两条分支都有测）
    #[test]
    fn unix_local_data_prefers_xdg_data_home() {
        let e = env_of(&[("XDG_DATA_HOME", "/xdg/data"), ("HOME", "/home/u")]);
        assert_eq!(local_data_dir_unix(&e), Some(PathBuf::from("/xdg/data/OpenPhM")));
        let e = env_of(&[("HOME", "/home/u")]);
        assert_eq!(
            local_data_dir_unix(&e),
            Some(PathBuf::from("/home/u/.local/share/OpenPhM"))
        );
    }

    /// 当前平台那条路要真的能答出一个目录（本机环境变量当然齐全）——
    /// 这条是"别把 `cfg!` 的分支写反"的保险：Linux 上必须走 Unix 规则。
    #[test]
    fn the_current_platform_picks_the_right_rule() {
        let cfg = config_dir().expect("本机环境变量齐全");
        let text = cfg.to_string_lossy().to_owned();
        if cfg!(windows) {
            assert!(!text.contains(".config"), "Windows 不该走 XDG 那条：{text}");
        } else {
            assert!(
                text.ends_with("/OpenPhM") && (text.contains(".config") || text.contains("xdg")),
                "{text}"
            );
        }
    }
}
