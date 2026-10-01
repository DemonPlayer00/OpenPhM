// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! OpenPhM —— 制谱器核心库。
//!
//! 模块职责：
//! · [`doc`]    opm 文档模型（有理拍、字符串枚举、foreign 字段袋）
//! · [`core`]   **编辑核心**：CLI 与 GUI 共享的同一个编辑会话（含命令分发）
//! · [`autosave`] **快照的后台写手**：把"整份文档 → JSON → 落盘"（50 000 音符 223 ms）
//!   从 GUI 帧里搬走 —— 它曾经是"播放时每 2 秒卡一下 250 ms"的成因
//! · [`broadcast`] 更新广播与细粒度话题（改完就广播；无关控件不参与更新）
//! · [`perf`]    表演求值：拍↔秒时间映射、29 个缓动、事件轨道求值
//! · [`journal`] 更改日志（记录更改模式的撤销/重做）
//! · [`control`] 控制通道：让 CLI 接进正在运行的 GUI 进程
//! · [`cli`]    命令行参数与工作区预设（纯解析，可单测）
//! · [`cmd`]    命令解析与校验（无状态）
//! · [`state`]  编辑器视图状态（播放头、可见区间等）
//! · [`render`] 演奏区渲染（wgpu 实例化，viewport 映射）
//! · [`gpu`]    显卡选择策略（**只插手 Linux**：默认核显、prime-run/显式开关才用独显；其它平台保持默认选择器）
//! · [`headless`] 无头渲染出图（给 agent 用：改完谱能自己"看"结果）
//! · [`fonts`]  CJK 字体装载
//! · [`codec`]  格式编解码（opm 原生 + RPE 导入导出；枚举表以 `spec/*.json` 为单一数据源）
//! · [`filedialog`] 系统文件对话框（kdialog/zenity）与"在文件管理器中显示"
//! · [`keymap`]  快捷键的状态规则（空格＝单点切播 / 长按试听，纯逻辑可单测）
//! · [`dirty`]  广播话题 → 面板脏位（唯一映射表；纯函数，有单测）
//! · [`fps`]    **帧率指示**（底栏那一格）：只统计已经发生的帧、从不主动要求重绘；
//!   显示值最多每 0.5 秒更新一次（用户口径）
//! · [`demo`]   演示谱面（`--notes N`：走命令路径造的负载谱面）
//! · [`edit`]   编辑意图 → 命令（手势翻译成命令 JSON；纯函数，可单测）
//! · [`dialog`] 模态对话框的统一外观（颜色/宽度/Esc 归属；新建谱面与编辑页的弹窗共用一套）
//! · [`recents`] 起始界面（最近打开的谱面 + 新建谱面表单；存在配置目录，不进谱面文件）
//! · [`session`] **单会话**（同一时刻只允许一个进程；锁随句柄走，被强杀也由内核放掉）与
//!   "上次没退干净"的判定（解压缓存里还躺着 GUI 留下的目录 = 崩溃遗留）
//! · [`view`]    视图模型（左侧判定线列表行 / 右侧检查器快照；纯派生，可单测）
//! · [`zip`]     最小 ZIP 读写（自研；opm 的容器形态需要它，本机取不到 crates）
//! · [`shot`]    自截屏（`--shot`）的决策逻辑（纯函数；没配置时**永远不动**这条有回归测试）

pub mod autosave;
pub mod broadcast;
pub mod audio;
pub mod codec;
pub mod cli;
pub mod cmd;
pub mod control;
pub mod core;
pub mod dialog;
pub mod demo;
pub mod dirty;
pub mod edit;
pub mod doc;
pub mod journal;
pub mod keymap;
pub mod filedialog;
pub mod fonts;
pub mod fps;
pub mod gpu;
pub mod headless;
pub mod perf;
pub mod recents;
pub mod render;
pub mod session;
pub mod shot;
pub mod state;
pub mod timeline;
pub mod view;
pub mod zip;

/// **测试公用件**：只在 `cfg(test)` 下编译（不进产物）。放的是"在多个模块里逐字出现过"的
/// 测试助手 —— 判据见模块头。
#[cfg(test)]
pub mod testkit;
