# OpenPhM 框架选型（Windows + Linux，要求真实 GPU 加速）

> 目标：为 **opm 制谱器**选定跨平台 GUI/图形框架。
> 编制日期：2026-09-27。**所有版本号与许可证均为本次直读上游文件所得**（见第 10 节来源）；标注「未核实」的不臆断。
> 相关文档：[`OpenPhM-格式设计提案.md`](./OpenPhM-格式设计提案.md)、[`spec/opm-format.md`](./spec/opm-format.md)

---

## 1. 先把"图形加速"翻译成可验收的指标

"完善的图形加速"是模糊需求；制谱器的真实负载是 **2D 密集渲染**，不是 3D。落到可测指标：

| # | 指标 | 为什么 |
|---|---|---|
| M1 | **10 万级四边形实例化批渲染 ≥ 60 fps**（1080p，含每实例变换矩阵） | 一张谱面 1 万~10 万音符 + 判定线 + 时间轴刻度；这是主渲染路径 |
| M2 | **自定义着色器 + 自定义渲染通道** | 判定线的不透明度叠加（`a = a₁+a₂−a₁a₂`）、音符染色、镜像、谱面预览都需要 |
| M3 | **CJK 文本 + 输入法（IME）** | 谱面名/谱师名/搜索框；IME 是中文用户的硬需求，也是最容易被忽略的一环 |
| M4 | **音频时钟驱动**，漂移需可测量且长期不累积 | 音游编辑器的命门：时间轴与判定线动画必须以**音频时钟**为准，不是帧时钟 |
| M5 | **Linux 上必须走 Vulkan 或 OpenGL 4.x**，可检测并拒绝软件渲染 | 不接受"在 Linux 上悄悄退化成 llvmpipe 软渲染" |
| M6 | 原生文件对话框、剪贴板、拖放 | 工程文件、音频、曲绘导入 |

**M1/M2 是选型的决定项；M4 是独立的库选择，与框架解耦。**

---

## 2. 先淘汰一批（省下比较成本）

| 方案 | 淘汰理由 |
|---|---|
| **Tauri / Electron / WebView 套壳** | Linux 侧是 WebKitGTK，GPU 路径依赖 DMA-BUF 与发行版打包，历史上频繁退化为软件渲染 ⇒ **直接违反 M5**。Chromium 系（Electron）GPU 路径好得多，但为 10 万精灵背一个浏览器运行时，性价比不成立 |
| **Canvas 2D / tiny-skia / Skia CPU 后端** | CPU 光栅化，违反 M1 |
| **纯 Web + WASM** | 桌面端交付形态不符（opm 要读写本地工程目录、跑 ffmpeg、处理大音频） |

---

## 3. 候选矩阵（均为直读上游核实）

| 路线 | 技术栈 | GPU 后端 | 许可证 | M1 十万级 2D | UI 成熟度 | 结论 |
|---|---|---|---|---|---|---|
| **R1** | Rust + `winit` 0.30.x + `wgpu` 30 + `egui`/`egui-wgpu` 0.36 | Vulkan（Win/Linux 一等）、DX12（Win 一等）、GL 3.3+/GLES3 降级 | MIT OR Apache-2.0 | ✅ 自己写实例化渲染，完全可控 | 中（即时模式，工具型界面够用） | **推荐** |
| **R2** | Rust + **Bevy 0.19.1**（当前稳定；phichain 钉 0.18.1）+ `bevy_kira_audio` + `bevy_prototype_lyon` | wgpu（同上） | MIT OR Apache-2.0 | ✅ 引擎自带 sprite 批渲染 | 中低（`bevy_ui` 对复杂编辑器面板偏弱） | **备选**，有先例 |
| **R3** | Rust + `iced` 0.14.0（`iced_wgpu`） | wgpu | MIT | ⚠️ 自定义 GPU 绘制通道不如 egui 的 callback 直接 | 高（Elm 式响应式） | 若 UI 复杂度压过渲染需求再考虑（注：0.14 后已 9 个月无发版） |
| **N1** | C# + **osu!framework** | 自有 GPU 渲染层（Veldrid → D3D11/GL/Vulkan/Metal） | **框架 MIT，但音频硬依赖专有 BASS** | ✅ 为音游而生 | 高（自带 UI 控件、文本、输入） | 音游血统最正 + 有编辑器先例（GDEdit）；**但 BASS 与 GPL-3.0-or-later 存在 copyleft 冲突（见 5 节），必须替换音频层后才可用** |
| **N2** | C# + Avalonia 12 | Skia | MIT | ⚠️ 以 UI 为主，密集自定义绘制需自建 | 高 | **Linux 默认渲染模式是 `[Glx, Software]`，Vulkan 不在默认列表** ⇒ 默认允许静默软渲染，与 M5 冲突 |
| **C1** | C++ + Qt 6.11（QML + RHI） | Vulkan / D3D11 / **D3D12** / OpenGL / Metal | GPL / **LGPLv3** / 商业 | ⚠️ **Qt Quick 场景树只支持 16 位索引（单批 ≤16384 四边形，官方目标 batches < 10）⇒ 直接塞 QML 不达标，必须自研 `QSGRenderNode`** | 最高（时间轴、树、停靠面板、无障碍、原生 IME 都现成） | 想要"最像桌面软件"就选它；代价是 C++、LGPL 动态链接义务、自研渲染 |
| **E1** | **Godot 4.7.2**（MIT） | Forward+/Mobile 驱动 = **Vulkan 或 D3D12**（或 Metal）；Compatibility = 仅 OpenGL；**4.4 起 Vulkan↔D3D12 自动互回退** | MIT | ✅ `MultiMeshInstance2D`（官方文案：单次 draw 可达数百万实例） | 中（要在游戏引擎里做工具 app） | 若团队想用引擎省事；**唯一内建视频导出（Movie Maker）**，但音频时钟是已知弱点 |

**淘汰判据回顾**：R1/R2/N1/C1/E1 都能满足 M1/M2/M5；差异在**UI 成熟度**与**语言生态**。

### 3.1 补充排除项（复核后新增，均为硬性理由）

| 方案 | 排除理由 |
|---|---|
| **JUCE** | `LICENSE.md` 原文为 **AGPLv3 或商业**双许可。**AGPLv3 与 GPL-3.0 不兼容**（GPL-3.0 无法满足 AGPL §13 的网络条款组合），要么买商业授权要么不能用 |
| **Flutter（Linux 桌面）** | 【源码链核实】Linux embedder 走 **GTK3 + `GdkGLContext`**（`fl_view_renderer_opengl.cc`），`BUILD.gn` 只有 `opengl`/`software`/`subsurface` 三种 view renderer，**无 Vulkan 合成路径** ⇒ **不满足"Linux 必须 Vulkan"**。且 3.47 起 Impeller 成为 Linux 默认（2026-08-12），**随即出现渲染回归**（#192915 Mesa SVGA3D 整窗闪烁、#181441 用户要求保留 Skia 退路、#185882 Wayland+NVIDIA 创建 GL 上下文失败）；**Impeller 不自带软件光栅器**（官方 FAQ："Impeller doesn't have a software backend unlike Skia"）⇒ 只有 llvmpipe 时照走 GL 但用 CPU 跑，**且无告警**。自绘能力也弱：自定义 shader 仅片元阶段，无顶点着色器/UBO/SSBO |
| **N2（Avalonia）** | 【源码核实】`X11Platform.cs` 把 **`llvmpipe`、`SVGA3D` 硬编码进 `GlxRendererBlacklist`**（注释：软件 GL 光栅器，"sometimes attempts to use GLX might cause a segfault"）⇒ 在软件栈/VM 上会被**主动**拒绝 GLX 并落到 CPU 位图面；而 `RenderingMode` 默认值是 `[Glx, Software]`（**默认列表里没有 Egl，也没有 Vulkan**），且 Vulkan 后端失败时**仅记日志返回 null**、静默进入下一模式。**正好踩中 M5**。若仍要用：显式覆盖为 `[Egl, Glx, Software]` 并自检 `GL_RENDERER` 非 llvmpipe/SVGA3D 后告警 |
| **N1（osu!framework）** | 见 5 节：**BASS 与 GPL-3.0-or-later 结构性冲突**，非署名问题 |
| **R3（iced）** | 0.14 之后**约 9.5 个月无发版**；master 把你锁进它自带的 winit/cosmic-text fork 与 wgpu 29（不是 30） |
| **slint** | crates.io `license = "GPL-3.0-only"`（未授予 or-later），且**不支持反向嵌入你的 render pass**（#4499 仍 open），wgpu 集成藏在 `unstable-wgpu-30` feature 后 |
| **gpui / xilem / floem** | gpui 在 Linux 用 **Blade/Vulkan 而非 wgpu**（放弃复用自研管线）；xilem 约 11 个月无发版；floem 约 22 个月无发版 |

---

## 4. 推荐：R1（Rust + wgpu + winit + egui）

### 4.1 为什么是它

1. **架构正好对上**：`egui-wgpu` 提供 [`CallbackTrait`](https://docs.rs/egui-wgpu/latest/egui_wgpu/trait.CallbackTrait.html)（`prepare` / `finish_prepare` / `paint`），可以把**自己的实例化渲染直接画进 egui 的同一个 render pass**，并可用返回 `CommandBuffer` 的方式完全接管命令录制。⇒ **编辑器外壳用 egui，演奏区/时间轴用自研 wgpu 渲染器**，两者共享一个 device/queue，不需要两套窗口系统。
2. **GPU 后端矩阵够硬**：wgpu README 明确列出 Vulkan（Windows/Linux，一等）、DX12（Windows，一等）、OpenGL（GL 3.3+ / GLES 3.0+，降级档）。M5 可满足，且 `WGPU_BACKEND` 环境变量提供运行时切换/诊断。
3. **许可证干净（已逐项复核）**：`wgpu` MIT OR Apache-2.0、`egui`/`eframe`/`egui-wgpu` MIT OR Apache-2.0、**`winit` 是 Apache-2.0 单一许可**（不是双许可——早期草案此处写错，已修正）。三者都与 GPL-3.0-or-later 兼容且允许静态链接，不需要像 `prpr` 那样做许可证审计。
4. **与既有决定一致**：RPE 兼容层是自研 Rust 也能直接复用；生态参考实现（`prpr`）是 Rust，读源码学语义时语言无障碍（**只读不抄**，见提案 §9.2）。
5. **构建与发布简单**：单一 `cargo build`，Windows/Linux 产物一致，无运行时依赖（对比 Qt 要打包一堆 .so、.NET 要带运行时）。

### 4.2 具体依赖清单（版本为 crates.io 实测，2026-09-27）

| 关注点 | 选择 | 版本（已核实） | 备注 |
|---|---|---|---|
| 窗口/输入 | `winit` | 稳定线 0.30.x（0.31 仍在 `0.31.0-beta.3`） | MSRV 1.86 |
| GPU | `wgpu` | **30.0.1**（2026-08-22） | MSRV 1.87；默认 feature 已含 `vulkan`/`dx12`/`gles`/`metal`/`wgsl` |
| UI 外壳 | `egui` + `eframe` + `egui-wgpu` | **0.36.2**（2026-09-08） | MSRV **1.95**、edition 2024 ⇒ 需要较新的 Rust 工具链 |
| 演奏区渲染 | **自研**：`wgpu` 实例化四边形 + 自定义 WGSL | — | 走 `CallbackTrait` 挂进 egui 的 render pass |
| 2D 路径三角化 | `lyon` | MIT | 判定线、曲线、自定义形状 |
| 文本（CJK） | egui 自带字体栈（**需自备中文字体**） | — | egui 官方 FAQ 明确：非拉丁字符必须自己 `set_fonts` |
| 文件对话框 | `rfd` | MIT/Apache-2.0 | 原生对话框 |
| 音频 | **`cpal` 做音频主时钟**（不用高层库担保同步） | **0.18.2**（2026-08-16，Apache-2.0） | `OutputStreamTimestamp.playback` 是唯一有官方"送达 DAC 时刻"语义的时间戳；可选 `pipewire` feature |
| 音频（便利层） | `kira` + `symphonia` | kira 0.12.5 / symphonia 0.6.1 | kira 用于音效与混音；**0.12 已无 `TrackingClock`**，不要拿它担保时钟精度；symphonia 为 MPL-2.0 |
| 视频导出（可选） | 外挂 ffmpeg | — | 不阻塞主线 |

### 4.3 风险与对策

| 风险 | 对策 |
|---|---|
| **即时模式 UI 的布局短板**（滚动区巨大时每帧全量布局） | 时间轴等密集区域**不走 egui 控件**，走自研 GPU 渲染 + 命中测试；egui 只用于面板/工具栏 |
| egui 破坏性更新（官方自述 "interfaces are still in flux"） | 锁定小版本；把 egui 用量限制在外壳，降低替换成本 |
| Linux 显卡/驱动碎片 | 启动时打印 adapter 名称与后端；检测到 llvmpipe/softpipe 时**显式告警**；提供 `WGPU_BACKEND=gl` 回退 |
| Wayland / X11 差异 | winit 两者都支持；开发期两种会话都过一遍 |
| **IME（中文输入法）** | 必须 spike 验证（第 7 节 S3）——这是最容易被框架拖后腿的一项 |

---

## 5. 备选路线与触发条件

- **R2（Bevy）**：同类先例最硬——**phichain**（Phigros 制谱工具链，LGPL-3.0）用 `bevy 0.18.1` + `bevy_kira_audio` + `bevy_prototype_lyon`，已拆出 `phichain-renderer`/`game`/`converter` 等 crate。补充核实（若走这条线要用到）：
  - **2D 就是 GPU 实例化批渲染**（源码 `bevy_sprite_render/src/render/mod.rs`）：每 sprite 一条 **80 字节实例**，共享 6 索引/4 顶点四边形，一批 = 一次 `draw_indexed`。
  - **官方压力测试规模就是 320×320 = 102,400 个 sprite**（`examples/stress_tests/many_sprites.rs`）。但注意：**这只证明规模被跑过，不证明 60 fps**——官方公开的性能数字全是 3D。
  - **批的切分条件决定真实成本**：需同一 `AssetId<Image>`（同图集）+ 透明相位中 z 序连续；换贴图或插入非精灵项即断批，且**每帧要对透明相位按 z 全量排序** ⇒ 谱面渲染必须做**图集化 + 稳定层序**设计，否则"改层序"会同时付出排序与断批双重代价。
  - **编辑器 UI 比预期好**：0.19 起第一方 `EditableText` 已含选区/剪贴板/Unicode/**IME（明示 CJK）**/多行滚动；未实现的是 placeholder、undo/redo、密码掩码。**缺的是 dock 与 undo/redo**（官方编辑器路线图里 resizable panes / workspaces / command palette / 布局持久化 全未勾选）。替代：`bevy_egui` 0.42 + `egui_dock` 0.21。
  - **文件对话框是生态短板**：`bevy_file_dialog` 锁 0.18、不支持 0.19 ⇒ **直连 `rfd`**。剪贴板虽第一方（内部 arboard），但**刻意不含 Ctrl+C/V 集成**，且 **Wayland 需显式启用非默认的 `wayland-data-control`**。
  - 当前稳定版是 **0.19.1**（0.20 未发布）；**0.18.1 是 phichain 钉的版本，不是 Bevy 当前版**。
- **N1（osu!framework）**：C# 背景下**音游领域血统最正**的选择（osu! 本体、fluXis 音游、GDEdit 关卡编辑器）。**但 BASS 是阻断级问题，不是 NOTICE 署名问题**：BASS 许可明确"非商用免费"且**禁止再许可（sublicensing）**，而 GPLv3 要求整个作品按 GPL 条款传递给下游 ⇒ 把不可再许可的专有库链接进 GPL-3.0-or-later 作品并分发，属于**需要法务确认的 copyleft 冲突**。osu! 自己能这么做是因为 **osu! 本体是 MIT**，该先例不能平移。⇒ **N1 在 GPL-3.0-or-later 下仅在替换音频层后才可用**（osu!framework 是否可无 BASS 构建：[未能核实]）。
- **C1（Qt 6.11）**：只有在"UI 观感 / 无障碍 / 原生 IME / 原生桌面集成"优先级最高、且团队 C++ 熟练时才选。LGPLv3 与 GPL-3.0-or-later **天然相容**（LGPLv3 正文即 GPLv3 的附加许可集；GPLv3-only 模块如 Qt Graphs/Quick 3D 可直接用，但用了就**永久不能闭源或转宽松许可**）。硬约束：**别静态链接 Qt**（LGPL §4(d)(0) 才需随附可重链接目标文件；动态链接是履行 §4(d)(1) 的常规手段）。另注意 **LTS 补丁只给商业许可持有者**（OSS 侧 6.8 止于 6.8.3）。**渲染上必须自研 `QSGRenderNode`**——Qt Quick 批处理只有 16 位索引，单批 ≤16384 个四边形。
- **E1（Godot 4.7.2，MIT）**：适合"想尽快出界面"或"**视频导出**与编辑/播放同内核"优先的场景；Vulkan↔D3D12 自 4.4 起自动互回退，Linux 覆盖面最宽。代价：用引擎做纯工具 app 会持续与运行时模型角力，且**音频时钟是已知弱点**（`get_playback_position()` 有多个 open issue：桌面/Web 语义不一致、数值错误、更新频率过低）⇒ 若走这条线，M4 必须单独 spike，可能仍需绕过引擎音频层。

---

## 6. 与框架解耦的两件事（别混进框架选型里）

1. **音频时钟（M4）**：音游编辑器必须用**音频设备时钟**驱动播放头。复核后的结论**有变化**：
   - **`cpal` 0.18.2**（Apache-2.0）的 `OutputStreamTimestamp { callback, playback }` 中，`playback` 官方文档原文是「**predicted** instant that data written will be **delivered to the device for playback**. E.g. The instant data will be **played by a DAC**」⇒ **这是唯一有官方语义指向"DAC 播放时刻"的时间戳**，取用入口是 `OutputCallbackInfo::timestamp()`。
   - **`kira` 0.12.5 的 `clock` 模块只有 `ClockHandle`/`ClockId`/`ClockTime`/`ClockSpeed`——不存在 `TrackingClock`**（0.12 已移除/改名），且 `ClockHandle::time()` 的精度语义未文档化，也没有"从音频线程推送当前采样位置"的回调式 API。
   - ⇒ **推荐：用 `cpal` 自建音频输出线程做主时钟，把 `kira` 当作"播放/混音的便利层"而非同步正确性的担保**。`kira` 仍可用来做音效与解码（默认带 `cpal-realtime`，Linux 经 D-Bus 申请 RT 调度）；解码用 `symphonia`（MPL-2.0，与 GPL 兼容）。
   - Linux 侧额外优势：**`cpal` 有可选 `pipewire` feature**（依赖 `pipewire ^0.10`），而 `miniaudio`/`PortAudio` **都没有原生 PipeWire 后端**（只能走 pipewire-alsa/pulse 兼容层）。这是 Rust 路线在 Linux 音频上优于 C++ 路线的一点。
   - 官方措辞是 "predicted"，**没有任何保证性文档说明所有后端都准确**，各后端时间戳精度需实测（S2）。
2. **视频导出**：外挂 `ffmpeg` CLI（子进程 + 逐帧管道）是各路线唯一稳健选择，**但要纠正一个常见误解**：**Godot 的内建 Movie Maker 并不接 ffmpeg**——只有 3 个内建 MovieWriter（**OGV** Theora+Vorbis、且仅编辑器构建可录；**AVI** MJPEG、上限 4 GB；**PNG 序列**+WAV），**自定义格式必须自己继承 `MovieWriter`（官方建议走 GDExtension）**。Godot 的真正价值在于它验证了**正确范式**：官方定位是 **non-real-time / offline rendering**，保证 "perfect frame pacing; it will never exhibit dropped frames or stuttering"，`--fixed-fps` 让逻辑 delta 与真实渲染耗时脱钩。
   ⇒ **谱面预览视频必须以固定时间步的确定性离线渲染实现（逻辑时间由帧号推出，而非墙钟），再写 PNG 序列或裸帧管道交给编码器。实时渲染 + 抓帧是错误范式。** 附带约束：Godot 侧**混音率必须能被录制 FPS 整除**，否则音频随时间失同步；`--resolution` 受显示器分辨率钳制。
   **许可与专利分层**：约束来自链接的 `libav*`（LGPL/GPL 取决于 FFmpeg 构建选项）。**对 GPL-3.0 应用，直接链 GPL 构建的 FFmpeg（含 libx264）反而最省事**（与 GPL-3.0 同许可）；LGPL 构建 + 动态链接是更保守、保留将来放宽许可余地的选择。**真正的风险是 H.264 专利而非版权**（FFmpeg 官方 legal 页点名 MPEG LA 收费）⇒ **建议默认导出 AV1（`rav1e`，BSD-2）或 VP9/Theora，把 H.264 作为用户自担责任的可选路径**。纯 Rust 编码器在 H.264 上**断档**（`x264` 绑定停更于 2022-12、`vpx-encode` 停更于 2022-08），只剩 `rav1e` 与 `openh264`（后者带 Cisco 二进制专利授权条款，非纯 BSD 语义）。

---

## 7. 选型验证计划（先测后定，约 2 天）

不做纸面结论，用四个 spike 定生死：

| # | 实验 | 通过标准 | 不通过则 |
|---|---|---|---|
| **S1** | wgpu 实例化渲染 10 万个带独立变换的四边形（1080p，含判定线旋转） | ≥ 60 fps 且帧时间 p99 < 20 ms | 换 Bevy（引擎批渲染更成熟）或降低 M1 目标并改架构（分块剔除 + LOD） |
| **S2** | 音频时钟漂移：连续播放 5 分钟，比对音频位置与帧累积时间的偏差 | 无累积漂移（偏差有界，不随时间增长） | 换音频方案（`kira` ⇄ `cpal` 自建），并确认是库问题而非实现问题 |
| **S3** | CJK 文本渲染 + **IME 输入**（Windows 微软拼音 / Linux fcitx5） | 候选框位置正确、组合串不丢字 | 换 UI 层（iced / Qt）——这条往往决定框架生死 |
| **S4** | 三档后端启动矩阵（Vulkan / GL / 软件渲染）在 Win + Linux 各跑一遍 | Vulkan 与 GL 均可用；软渲染能被检测并告警 | 补后端回退策略或换框架 |

**顺序建议**：先 S3（最可能否决框架）→ 再 S1（核心性能）→ 再 S2（音频）→ 最后 S4（打包发布前）。

### 7.1 已完成实测（2026-09-27，本机 DPBox，Vulkan 1.4.357）

工程：`spikes/`（Rust，`cargo build --release` 通过；依赖版本与本文一致）。

**S1 — 实例化 2D 渲染（离屏 1920×1080，单次 instanced draw，alpha 混合 + 纹理采样）**

适配器：`NVIDIA GeForce RTX 5070 Laptop GPU`，backend=Vulkan，device_type=DiscreteGpu，驱动 615.71.09。
环境同时枚举出 `AMD Radeon 610M (RADV)`（Vulkan，IntegratedGpu）与 `NVIDIA .../PCIe/SSE2`（GL 后端，注意 **device_type 报为 `Other`**，不能靠 device_type 判断 GL 适配器）。

| 实例数 | p50 (ms) | p99 (ms) | min (ms) | max (ms) | 等效 FPS |
|---|---|---|---|---|---|
| 10 000 | 0.080 | 0.231 | 0.071 | 0.724 | 12424 |
| 50 000 | 0.157 | 0.180 | 0.146 | 1.306 | 6388 |
| **100 000** | **0.268** | **0.300** | 0.237 | 5.200 | **3733** |
| 200 000 | 0.520 | 0.539 | 0.477 | 0.544 | 1921 |

输出校验：中心 4×4 区域 16/16 为非背景像素（确实画出来了，不是空转）。
**结论：M1 通过，且余量约 60 倍**（判定线 60 fps 预算 16.7 ms）。耗时随实例数**线性增长**（100k→200k 为 0.268→0.520 ms）⇒ 瓶颈在 CPU 侧命令录制/顶点提交，不是填充率。
⚠️ **这条实测的边界**：测的是**离屏渲染 + submit→wait**，不含 swapchain、vsync、合成器。真实编辑器帧时间将由 **present/vsync（60 Hz 下 16.7 ms 起步）与 UI 层成本**主导，而不是这个 draw。所以 S1 的真正结论是「**GPU 不是风险，UI 与呈现路径才是**」。

**S3 — CJK 文本渲染（部分通过；IME 待人工验证）**

环境：Wayland（`wayland-0`，KDE），fcitx5 运行中，`XMODIFIERS=@im=fcitx`、`SDL_IM_MODULE=wayland`、`INPUT_METHOD=wayland`，系统 84 个中文字体。
窗口正常创建并渲染（`eframe` + wgpu 后端），截图见 `spikes/artifacts/`。字形结果：

| 语种 | 结果 |
|---|---|
| 简体 / 繁体 / 日文（含假名） | ✅ 全部正常 |
| 符号 φ ★ ☆ ①②③ | ✅ |
| **韩文（谚文）** | ❌ **豆腐块** —— Source Han Sans CN 是 SC 子集，不含 Hangul |

**根因与修复（这是 S3 最有价值的产出）**：单一 SC 字体覆盖不了 CJK 全语种，**必须做回退链**。已在 spike 中实现：主字体（单字面 SC）+ 韩文回退 + emoji 回退。
**并且发现一个会静默出错的坑**：`NotoSansCJK-Regular.ttc` 的字面索引是 **0=JP、1=KR、2=SC、3=TC、4=HK**（5..9 为 Mono 变体），而 egui 的 `FontData::from_owned` **固定用索引 0** ⇒ 直接加载 Noto CJK 会让**中文用上日文字形**（直/骨/门 等写法不同），且不会报错。修法：不要用 `from_owned`，显式构造 `FontData { font, index, tweak }` 并给 SC 传 2、KR 传 1。
**IME**：需人工在窗口内用输入法打字验证（本机无法注入输入事件——egui 跑的是原生 Wayland 窗口，`xdotool` 属 X11 注入不到）。
**第二次实测（字体回退链修好后）**：韩文 `한국어 테스트, 노트, 판정선` ✅ 恢复正常 ⇒ 回退链方案成立。
**IME 链路状态：已通，但中文提交尚未证明。** 实测到窗口在运行期间收到 **8116 个 `ImeEvent`**，日志内容为持续的 `Preedit { text: "", active_range_chars: None }`，而 **`Text` 事件为 0**（无任何字符提交）。⇒ 结论分三层：
1. **协议是活的**：winit（Wayland text-input）↔ fcitx5 的事件通道确实把 `ImeEvent` 送到了 egui —— 这条以前是"最可能否决框架"的风险，现已排除。
2. **但空 preedit 的高速刷屏本身是个问题**：8116 个空 preedit 说明实现里存在"每帧重发空 preedit"的行为（egui 0.36 侧或 fcitx5 侧），**真实项目必须去重/过滤空 preedit**，否则事件队列与日志会被灌满。
3. **中文提交未验证**：`Text` 事件为 0，所以"用输入法输入「判定线」能否正确上屏"**仍未证实**，需要人工敲一次。
另注：`egui::ImeEvent::Enabled/Disabled` 在 0.36 **已废弃**（源码注释 "No longer used by egui"），真正可观测的证据是 `Preedit { .. }` / `Commit(..)` 事件。

**S2 — 音频时钟漂移（通过）**

环境：cpal 0.18.2，**默认 host 只有 `Alsa`**（`可用 host: [Alsa]`——PipeWire/JACK 需显式启用 feature 或走 ALSA 插件）；物理设备 `AB13X USB Audio` 报 "temporarily busy"（被 PipeWire 占用），故显式指定 AI 走 PipeWire 的 ALSA 设备。实测配置：48 kHz / F32 / 2ch。

| 指标 | 实测值 |
|---|---|
| 时长 | 60.02 s（墙钟） |
| 音频时钟（`OutputStreamTimestamp.playback` 推算） | 60.031980 s |
| 帧数推算时长（frames / sample_rate） | 60.042667 s |
| **初始固定偏移** | **+10.75 ms**（前 10 s 内建立，属延迟/基线，**不是漂移**） |
| **稳态速率漂移** | **+7.4 ppm**（10 s 后线性回归斜率） |
| 输出延迟（`playback − callback`） | **34.6 ms**（全程恒定） |
| 流错误 | 0（5629 次 callback） |

**结论：S2 通过，但结论的形状和预期不同——不需要"漂移校正伺服"，需要的是一次性偏移标定。**
- 速率偏差仅 **7.4 ppm ≈ 0.44 ms/分钟**，5 分钟的曲子累计约 2.2 ms，**已在编辑器 < 5 ms 的目标内**；偏差**有界**、不随时间发散。
- 真正要处理的是**固定项**：10.75 ms 基线 + **34.6 ms 输出延迟**。后者是主项，来自 ALSA→PipeWire 插件的缓冲（PipeWire 默认 quantum 1024 @48 kHz ≈ 21.3 ms；官方 `min-quantum` 可到 32 ≈ 0.67 ms）。⇒ 想做低延迟，就启用 cpal 的原生 `pipewire` feature 或压小 quantum，而不是去修时钟。
- **方法论教训（记在 spike 里）**：把"初始偏移"与"稳态速率"混算成单一 ppm，会得到 184.9 ppm 这种误导数字。本 spike 已改为分离报告：偏移归偏移、斜率归斜率。**任何音频同步结论都必须这样分开给。**

**S1b — egui 内嵌自研实例化渲染（集成形态，1600×900，599 帧/组）**

同一套 wgpu 实例化管线经 `egui_wgpu::CallbackTrait` 挂进 egui 的同一个 render pass。左侧 200 行"音符列表"模拟编辑器外壳成本。

| 配置 | 整帧 p50 | 整帧 p99 | egui UI 构建 p50 / p99 | 回调 paint p50 |
|---|---|---|---|---|
| 纯 egui（0 实例） | 4.174 ms（239.6 fps） | 8.315 ms | 0.205 / 0.415 ms | — |
| egui + **10 万实例** | **4.175 ms** | 8.319 ms | 0.214 / 0.391 ms | 0.0002 ms |
| egui + 10 万实例（关 vsync） | 4.178 ms | 8.218 ms | 0.232 / 0.400 ms | 0.0004 ms |

**结论三条**
1. **10 万实例的集成代价 ≈ 0**（p50 从 4.174 → 4.175 ms）。与 S1 离屏测得的 0.268 ms 一致——GPU 余量极大，把渲染挂进 egui 的同一个 pass 没有任何额外惩罚。**M1/M2 在集成形态下同样通过。**
2. **egui 外壳成本 0.2 ms（p99 0.4 ms）**，距 60 Hz 预算（16.7 ms）约 40 倍余量。注意本 UI 仍是简单的；真实编辑器的停靠面板/时间轴会显著抬高这一项，而它才是真正的风险位。
3. ⚠️ **本环境下 vsync 并未生效**：默认 `AutoVsync` 与 `AutoNoVsync` 的帧时间完全相同（4.17 ms ≈ 239 fps，远低于 60 Hz 的 16.7 ms）。⇒ **不能依赖 vsync 给编辑器定节奏**；必须显式节流（按音频时钟或帧定时器驱动 repaint），否则应用会以 240 fps 空转、白烧 GPU 与电量。这对音游编辑器尤其要紧——**播放头的节奏应当来自音频时钟**（见 S2）。

**实现踩坑（两条，都会静默失败，记下来）**
- **回调矩形被裁剪 ⇒ egui 直接丢弃整个 paint callback**，且不报错。现象：帧时间与"纯 egui"完全一致、演奏区一片黑、回调计数恒为 0。根因：在 `ui.horizontal` 里用外部算好的 `avail` 宽度分配矩形，被横向布局中的 `ScrollArea` 挤到可视区外。修法：**先把可用区域手动切成左右两块 `Rect`，再用 `UiBuilder::new().max_rect(...)` 给列表独立 Ui**。
- **计时采样 bug**：无条件把回调耗时 push 进样本，导致"0 实例"那组也打印出 0.0000 ms 的假数据。修法：只在回调真的跑过（计数增长）时才记录。

**尚未做**：S4（三档后端启动矩阵，Windows 侧需在目标机执行）。S3 的 CJK 与 IME 均已通过。

---

## 8. 结论

**采用 R1：Rust + winit + wgpu + egui（`CallbackTrait` 挂自研渲染器）**，音频与视频导出作为独立决策（第 6 节），并用第 7 节的四个 spike 在动工前验证。

一句话理由：**它的 GPU 后端矩阵足够硬（Vulkan/DX12 一等 + GL 降级），许可证与本项目完全相容，且"egui 做外壳 + 自研 wgpu 做演奏区"正好匹配制谱器的负载形状**——不需要为 10 万个精灵背一个完整游戏引擎，也不用赌 WebView 在 Linux 上的 GPU 路径。

**待你确认**：① 是否接受 Rust；② 是否要我把 R2（Bevy，参照 phichain 的 crate 划分）也写成等价方案做 A/B。确认后我出 `Cargo.toml` 骨架 + S1/S3 两个 spike 的最小可运行工程。

---

## 9. 未核实 / 待补充

- 各框架在 **Windows + Linux 的实测帧率对比**：本文给的是能力矩阵与风险，**没有本机实测数据**——必须由第 7 节 spike 补齐。
- Avalonia 的 Skia GPU 后端在 Linux + 多显卡切换场景的具体表现。
- Qt 6 RHI 在 Linux 各平台插件（xcb/wayland/eglfs）下的默认后端选择细节。
- `winit` 0.31 的稳定时间表。
- 音频方案在本机的**实际延迟与时钟精度**（未测）。

---

## 10. 来源（本次直读核实）

- [wgpu README](https://github.com/gfx-rs/wgpu)（后端矩阵表：Vulkan/DX12 一等，GL 降级）+ [crates.io: wgpu](https://crates.io/crates/wgpu)（**30.0.1**，2026-08-22，MIT OR Apache-2.0，MSRV 1.87.0）
- [egui README](https://github.com/emilk/egui)（MIT OR Apache-2.0；`egui-wgpu` 官方后端；CJK 需自备字体；即时模式布局的固有短板）+ [crates.io: egui](https://crates.io/crates/egui)（**0.36.2**，2026-09-08，MSRV 1.95，edition 2024）
- [`egui_wgpu::CallbackTrait` 文档](https://docs.rs/egui-wgpu/latest/egui_wgpu/trait.CallbackTrait.html)（0.36.2，2026-09-08；`prepare`/`finish_prepare`/`paint`；可返回自有 `CommandBuffer`；依赖 `wgpu ^30.0`、`winit ^0.30.13`）
- [winit README](https://github.com/rust-windowing/winit)（master 为 `0.31.0-beta.3`；MSRV 1.86）
- [Bevy README](https://github.com/bevyengine/bevy)（MIT OR Apache-2.0；官方明示"约每 3 个月破坏性发布"、功能仍在早期）
- [iced README](https://github.com/iced-rs/iced)（`iced_wgpu` 支持 Vulkan/Metal/DX12；自述 experimental）+ [crates.io: iced](https://crates.io/crates/iced)（**0.14.0**，2025-12-07，MIT，MSRV 1.88；默认 feature 含 `wgpu`+`tiny-skia`+`x11`+`wayland`）
- [crates.io: kira](https://crates.io/crates/kira)（**0.12.5**，2026-09-26，MIT OR Apache-2.0；默认含 `cpal`+`cpal-realtime`+`cpal-realtime-dbus`）
- [phichain `Cargo.toml`](https://github.com/Ivan-1F/phichain)（`bevy 0.18.1` + `bevy_kira_audio 0.25` + `bevy_prototype_lyon 0.16`；workspace 含 renderer/game/converter 等 crate）
- [osu!framework README](https://github.com/ppy/osu-framework)（MIT；.NET 10；**BASS 为商业库、非商用免费**；使用方含音游 fluXis 与关卡编辑器 **GDEdit**）
- [Godot README](https://github.com/godotengine/godot)（MIT，2D/3D 跨平台）

---

## 11. 修正记录（本文档自身的错误，留痕）

本文档多处以「直读上游核实」自称，因此错误必须显式更正。以下 8 条由独立复核发现（2026-09-27）。

| # | 原表述 | 更正 |
|---|---|---|
| 1 | 「wgpu / winit / egui 全部 MIT OR Apache-2.0」 | **`winit` 是 Apache-2.0 单一许可**（`Cargo.toml` 实测，`LICENSE-MIT` 404）。结论不变（Apache-2.0 与 GPLv3 兼容），但原表述不实 |
| 2 | 候选矩阵写「Bevy 0.18.1」 | **Bevy 当前稳定是 0.19.1**（0.20 未发布）；0.18.1 是 **phichain 钉的版本** |
| 3 | Qt 行「M1 十万级 2D：✅」 | **误导**。Qt Quick 批处理只支持 **16 位索引**（单批 ≤16384 四边形，官方目标 batches < 10），直接塞 QML 场景树**不达标** ⇒ 必须自研 `QSGRenderNode` |
| 4 | 「BASS 是必须写进 NOTICE 的第三方约束」「风险可控」 | **升级为阻断级**：BASS 禁止再许可（sublicensing），与 GPLv3「整体按 GPL 传递」结构性冲突。**N1（osu!framework）在 GPL-3.0-or-later 下不可用**（osu! 能用是因为它是 MIT）。两处独立取证链确认 |
| 5 | Godot「Vulkan / Compatibility(GLES3)」 | **漏了 Windows 的 D3D12 路径**：Forward+/Mobile 驱动是 **Vulkan 或 D3D12**（或 Metal），Compatibility 才仅 OpenGL；**4.4 起 Vulkan↔D3D12 自动互回退**。版本应为 **4.7.2** |
| 6 | §9「Avalonia 的 Skia GPU 后端在 Linux 表现」列为未核实 | **已核实**：`X11RenderingMode` 默认 `[Glx, Software]`（**无 Egl、无 Vulkan**），且源码 `GlxRendererBlacklist` **硬编码 `llvmpipe`/`SVGA3D`** ⇒ 软件栈上主动落 CPU。**默认即踩 M5** |
| 7 | §6「kira 是否提供足够精度的播放位置查询，需 S2 实测」 | **kira 0.12 的 `clock` 只有 `ClockHandle`/`ClockId`/`ClockTime`/`ClockSpeed`，已无 `TrackingClock`**，精度未文档化；**唯一有官方"DAC 播放时刻"语义的是 `cpal` 的 `OutputStreamTimestamp.playback`** ⇒ 改为「用 cpal 自建主时钟」 |
| 8 | §6「音频用 kira」 | 补充：**cpal 有可选 `pipewire` feature**，而 miniaudio/PortAudio **都无原生 PipeWire 后端**（只能走兼容层）。Rust 路线在 Linux 音频上优于 C++ 路线 |

**同时新增的排除项**（原文未覆盖）：**JUCE**（AGPLv3/商业双许可，AGPLv3 与 GPL-3.0 不兼容）、**Flutter Linux**（GTK+OpenGL，无 Vulkan 路径，且 3.47 换默认渲染器后有回归）、**slint**（`GPL-3.0-only` + 不支持反向嵌入 render pass）、**iced 0.14+**（9.5 个月无发版，master 锁自家 winit/cosmic-text fork 与 wgpu 29）、**gpui/xilem/floem**（分别用 Blade 而非 wgpu / 11 个月无发版 / 22 个月无发版）。

**音频时钟的可执行设计（补 §6，来自 StepMania 的成熟做法）**：`RageSound::GetPositionSeconds(bool *approximate, RageTimer*)` 三步——① 取当前**硬件帧 + 时间戳**；② 若尚无位置数据则置 `*approximate = true` 并回退；③ 经 **硬件帧 → 流帧 → 源帧两级映射**换算成秒。两条精华：**`approximate` 标志位**（位置不可信时显式降级，而不是静默返回错值——这正是编辑器需要的诚实接口）与**锁序纪律**（先取驱动位置再上锁，绝不持锁做驱动调用）。Linux 侧还可直接读 PipeWire 的 `spa_io_clock.rate_diff`（驱动时钟相对单调时钟的速率比）**当漂移校正比例用，不必自己跑长时回归**；注意 cpal 各后端时间源不同（ALSA `htstamp`、PipeWire `pw_stream_get_time_n`、JACK `jack_get_time`、WASAPI `QPC`、**PulseAudio 用普通 `Instant`，精度最差**），且**跨流的时间原点不保证相同，别把两条流的时间戳直接相减**。

---

## 7.2 UI 骨架实测（Linux 侧，2026-09-27）

工程：[`app/`](./app/)（真正的应用 crate，非 spike）。布局：顶部工具条 / 底部状态栏 / 左侧虚拟化音符列表 / 右侧属性检查器与诊断 / 中央演奏区 + 时间轴。
显示环境：**2560×1600 @ 240 Hz，scale=1**（这解释了 §7.1 里"vsync 未生效"的疑点——**vsync 一直在工作，只是封顶在 240 Hz**：1000/240 = 4.1667 ms，与实测 4.174 ms 相差 0.2%）。

| 场景 | 整帧 p50 | UI 构建 p50/p99 | 实例构建 p50 |
|---|---|---|---|
| 2 万音符（可见 ≈29 实例） | 4.171 ms（239.8 fps） | 0.142 / 0.345 ms | 0.002 ms |
| 20 万音符（可见 ≈29 实例） | 4.173 ms（239.7 fps） | 0.143 / 0.248 ms | 0.002 ms |
| 20 万音符 + **全量 20 万实例** | 4.175 ms（239.5 fps） | 1.127 / 1.381 ms | 0.996 ms |

**结论**：带真实编辑器布局 + 20 万实例每帧重建，仍打满 240 Hz。**可见物量只有约 30 个实例**（12 音符/秒 × 2 秒前瞻）——**"10 万实例"从来不是演奏区的问题，那是压力上限**；真正的成本在 UI 侧。

### 本轮抓到的两个真 bug（都已修）

1. **时间轴按整谱长度画拍线 ⇒ UI 成本随谱面长度增长，而非随可见内容增长。**
   20 万音符 = 16666 s 的谱面画了 **5 万条**拍线，帧时间从 4.17 ms 涨到 **20.5 ms（48.9 fps）**，`--stress` 下 23.5 ms。
   更隐蔽的是：**这些线的镶嵌发生在 `ui()` 返回之后，所以我的 `ui_ms` 指标完全没算进去**——只看 `ui_ms` 会误判成"UI 很轻"。
   修法：自适应抽稀（步长加倍直到 ≥6 px/条）。修后三组全部回到 4.17 ms。
   ⇒ **教训：帧时间必须用整帧间隔衡量，只看自己 `ui()` 内的计时会漏掉 egui 的镶嵌开销。**
2. **中文全是豆腐块**：egui 默认字体不含 CJK 字形，S3 已验证的字体装载没有搬进应用。修法：`app/src/fonts.rs`（优先单字面 SC；退化到 `.ttc` 时用 `fc-scan` 取 SC 的正确字面索引，避开"索引 0 = JP 让中文用日文字形"的静默坑）。

### 对齐自检（可验证，而非"看着对"）

`--verify-align` 让 **同一批 RPE 坐标**同时由自研管线（品红方块）与 egui 画笔（青色十字，使用与着色器一致的映射公式）绘制。
在 **scale=1 与 scale=1.5** 下，四角 (±675, ±450) 与中心 (0,0) **逐一重合** ⇒ 演奏区的 viewport 映射在分数 DPI 下正确。

### Linux 侧剩余问题

1. **`--fps-cap` 不可靠**：`request_repaint_after` 实测 cap=30 → 59 fps、cap=10 → 235 fps（完全忽略）。**不能拿它做节奏控制**；音游编辑器应改由音频时钟驱动 repaint（配合 §7.1 的 S2 结论）。
2. **p99 帧时间 9~11 ms** > 240 Hz 的 4.17 ms 预算 ⇒ 约 1% 帧错过 vsync。
3. **剪贴板**：`Failed to initialize arboard clipboard: X11 server connection timed out`（Wayland 会话下 arboard 尝试连 X11）⇒ 复制粘贴需单独验证。
4. 撤销/重做、工程文件读写、音频时钟接入均未做。

**Windows 兼容测试按用户要求排在 Linux 完善之后。**


---

## 7.3 节奏控制与工作区方案（Linux 侧，2026-09-27 续）

### 节奏：空闲 1 帧 / 工作满帧率（已实现并实测）

| 状态 | 判定条件 | 实测（240 Hz 屏） |
|---|---|---|
| **工作** | 播放中 / 正在按住拖拽（`egui_is_using_pointer()`）/ 首帧布局稳定前 / bench 活跃阶段 | **239.6 fps**（4.174 ms = 屏幕帧率） |
| **空闲** | 以上皆否 | `--idle-fps 1` → **2.50 fps**；`--idle-fps 0` → **1.25 fps**（纯事件驱动） |

**关键教训（两次踩坑）**
1. **活动信号不能用 `egui_wants_pointer_input()`**：其语义是"egui 想接收指针事件"，**鼠标悬停即为真**；误用会把空闲态在鼠标停靠时判为工作态——实测把空闲 fps 顶到 **88 ~ 152**。
   正确信号是 `egui_is_using_pointer()`（正在按住/拖拽）。**这是本项目第二次因"想当然的 API 语义"而误判**（第一次是 egui 丢弃越界回调）。
2. **测量逻辑本身会毁掉测量**：bench 的活跃阶段若不被显式标记为"工作态"，事件驱动下永远跑不满目标帧数，进程会被 timeout 杀掉；
   而管道场景下 stdout 是块缓冲，被 kill 时**输出整体丢失**（表现为"什么都没打印"）。⇒ 关键输出要显式 `flush`，且 bench 必须自带活跃态。
3. `egui` 提供 `Context::repaint_causes()`，能看到**是谁在请求重绘**（精确到 file:line）。定位空闲不降帧时，这是第一个该用的工具。

### 工作区排布方案

见 [`app/WORKSPACE.md`](./app/WORKSPACE.md)：四个预设（制谱 / 时间轴 / 表演 / 调试），按任务显隐面板，尺寸用比例，
含快捷键约定、与 opm 数据模型的映射、空闲成本对照，以及"何时才该上 dock 系统"的触发条件。

**明确不做（现阶段）**：可停靠面板与布局持久化。在 opm 数据模型与 codec 固化前，硬编码预设的性价比更高；
触发条件是"用户明确要求自定义布局"或"面板数量 > 8"。


---

## 7.4 编辑功能与 Agent 入口（Linux 侧，2026-09-27 续）

### 已实现

- **文档模型**（`app/src/doc.rs`）：对齐 `spec/opm-format.md` —— 有理拍 `{n,d}`、字符串枚举、`foreign` 字段袋（未知字段原样保留）。
- **编辑命令与会话**（`app/src/cmd.rs`）：增删改判定线与音符；**移动/透明度/流速统一走事件模式**（5 条轨道 + 29 种具名缓动 + `split_event`）；
  `set_track_constant` / `normalize` 一步满足"轨道无空隙无重叠"的不变量；**记录更改模式**的撤销/重做（`journal.rs`：逐条记录类型化逆操作，64 MiB 上限），命令失败自动回滚。
- **两个前端一套语言**：`opm-ctl`（无头 CLI，给 agent）与 GUI 控制台（调试工作区，给人）共用同一个 `Session`。
- **无头出图**（`app/src/headless.rs`）：`render` 子命令直接出 PNG，供 agent 读图核对几何。

### 实测（本轮）

```
$ opm-ctl new --out agent.opm.json --name agent-demo
$ opm-ctl --file agent.opm.json --script edits.jsonl --save     # 15 条命令全部 ok，normalize 修 8 处
$ opm-ctl --file agent.opm.json validate                        # [PASS] 0 error / 0 warning，exit 0
$ python3 spec/check.py agent.opm.json                          # [PASS]（独立实现）
$ opm-ctl --file agent.opm.json render --at 0 --lookahead 2.5 --out preview.png
  渲染完成：1280×720，实例 8（3 条判定线 + Tap/Hold+长条体/Flick/Drag）
```

**双校验器交叉验证**：在人为破坏轨道连续性的文件上，Rust `opm-ctl validate`（exit 3）与 Python `spec/check.py`（exit 1）
给出**完全相同的 JSON 指针与消息**：`/judgeLines[1].layers[0].alpha[1] 轨道不连续`。⇒ 规则若单边漂移会被立刻发现。

**失败路径**：`hold` 缺 `endBeat` → exit 1 且回滚；非法缓动名 `easeInOut` → exit 1；校验有 ERROR → exit 3。三者互不混淆。

### 设计要点（给 agent 的接口）

见 [`app/AGENT-API.md`](./app/AGENT-API.md)。核心判断：**不要只给 agent 一个校验器，要给它眼睛** ——
`validate` 只能证明结构合法，证明不了"看起来对"；把 `render` 放进闭环（编辑 → 校验 → 出图 → 读图）才能真正收敛。
另外两条刻意的设计：命令语言只有一套（人机语义一致，不存在"只有 GUI 能做"的操作）；
轨道不变量由 `set_track_constant`/`normalize` 承担，不让 agent 逐条拼事件。

### 已知限制

CLI 每次调用是独立会话（`undo` 不跨调用；附着进 GUI 进程时共享同一个会话，撤销栈也是同一份）；
多 BPM 时间映射未完成（出图按首个 BPM）；父子线/控制曲线/扩展事件未入模型（走 `foreign` 保留但不编辑）；codec 未接；
演奏区尚未真正消费轨道事件（`Track` 话题已置"需重绘"，但渲染管线还只按音符位置出图）。

---

## 7.5 更新广播：改完就广播，无关控件不参与更新（Linux 侧，2026-09-27 续）

### 需求与结论

用户给的是架构约束，不是功能点：**EditCore 更改资源后要发 update 广播与更改；要设计机制让无关控件不参与更新；
GUI 内部不要私自更新 note 等，而是向 EditCore 发送更新后等待 update 广播。**

落地成三条不可逆的规则：

1. **EditCore 是唯一可写方，广播是它唯一的"通知"出口**。`exec`/`undo`/`redo`/`commit` 成功后各发一条
   `Broadcast{revision, origin, label, topics[], changes[]}`；失败的命令**不发**（文档没变就不该叫醒任何人）。
   事务内的改动**逐条广播**（`commit` 只结束事务；撤销粒度仍是一步 —— 见 §7.10 的修正），
`abort` 回滚文档并发一条全量话题让订阅者重取。
2. **订阅带过滤器**：`TopicFilter{any|kinds|lines}`，命中的订阅者才收到投递，未命中者**一条都收不到**
   （`Subscribers::emit` 逐个比对，并清理断开的订阅）。
3. **GUI 只订阅**：`App::dispatch()` 是运行期唯一的写路径（`exec_batch`）；面板读的全是派生缓存，
   只在广播驱动下重建。命令发出后界面刻意停在旧状态，等广播到达才更新（状态栏亮 `⏳ 等广播`）。

### 话题的粒度定在哪

话题是 `(类别, 判定线下标)`：`Meta` / `Bpm` / `LineList` / `LineProps(line)` / `Notes(line)` / `Note(line)` / `Track(line)`。
粒度选择的依据是**脏位映射**而不是数据结构本身 —— 话题唯一存在的意义是"哪些派生视图要重算"：

| 话题 | 谱面派生视图 | 音符列表 | 工具栏聚合 | 检查器 | 演奏区 |
|---|---|---|---|---|---|
| `Meta` | — | — | ✅ | — | — |
| `Bpm`/`LineList`/`LineProps` | ✅（时刻要重算） | ✅ | ✅ | — | ✅ |
| `Notes`/`Note` | ✅ | ✅ | ✅ | ✅ | ✅ |
| `Track` | — | — | — | — | ✅ |

最费的一步是**谱面派生视图**（`chart_from_doc`：拍 → 秒展平，20 万音符时是唯一有感的开销），
因此 `Track` 与它无关这件事本身就是最大的收益：事件模式（移动/透明度/流速）是这套格式的主力编辑操作，
改一次 alpha 事件的代价是**零次重建**。

### 观测机制（让"无关控件没被更新"可被外部验证）

光有架构不够 —— "无关控件不参与更新"必须能被测量，否则它只是口号。因此：

- `ui_stats`（由控制线程直接回答，不进 EditCore，因为 EditCore 不该知道界面长什么样）暴露：
  `broadcasts`、`builds_{chart,list,meta,inspector}`、`skipped_{chart,list,inspector}`、`pending`、`seen_revision`、
  `wake_p50_ms`、`last_broadcast`、`last_topics`。
- `broadcasts`（EditCore 侧环形日志，512 条）暴露每条广播的来源与话题，供追溯。
- `scripts/accept-broadcast.py` 直接说 UDS 协议（第二份客户端实现），跑完话题分级 + 端到端延迟。

**计数口径**（自己先踩过才写清楚）：`skipped_*` 逐**广播**统计（话题与该面板无关），`builds_*` 逐**批**统计
（同帧多条广播合并成一次重建）。两者量纲不同：`broadcasts - skipped_x` = 触及该面板的广播数，
它与 `builds_x` 的差值就是合并收益（实测连发 20 条 → 2 次重建）。
（§7.6 把粒度进一步细化到**每条判定线**，计数按线拆成 `builds_structure/props/notes/tracks`。）

### 实测（GUI + `opm-ctl --attach`，3000 音符）

| 命令 | 话题 | Δ重建（谱面/列表/元/检查） | Δ跳过 |
|---|---|---|---|
| `set_meta` | `Meta` | 0 / 0 / **1** / 0 | 1 / 1 / 1 |
| `add_event`（alpha） | `Track[0]` | **0 / 0 / 0 / 0** | 1 / 1 / 1 |
| `set_track_constant`（speed） | `Track[0]` | **0 / 0 / 0 / 0** | 1 / 1 / 1 |
| `add_note` | `Notes[0],Note[0]` | **1 / 1** / 0 / **1** | 0 / 0 / 0 |
| 远端 `undo` | `Notes[0],Note[0]` | 1 / 1 / 0 / 1 | 0 / 0 / 0 |

延迟拆解：唤醒 → 应用广播 p50 **8.3 ms**（事件驱动空闲）/ **4.56 ms**（`--idle-fps 240` 连续出帧，≈1 帧 @240 Hz）；
外部客户端观测的 12–32 ms 里约 7–8 ms 是脚本轮询 `sleep` 的粒度。
⇒ 剩下的都是**窗口系统出帧调度**（窗口被遮挡时更长），广播链路自身在同一帧内完成（微秒级）。
远端改动能被立刻看到，靠的是控制线程改完后 `waker()`（`ctx.request_repaint()`）—— 否则空闲心跳下最多等 1 秒。

### 本轮抓到的两个真 bug（都靠实测数据暴露，不是靠读代码）

1. **话题开得过宽，机制直接失效**：`InsertEvent` 最初附带 `LineProps[line]`（想着"线也要更新"），
   而 `LineProps` 会置脏谱面派生视图 —— 于是"改一次 alpha 事件不重建任何缓存"变成"重建 20 万音符的展平视图"。
   数据先露馅：`skipped_chart` 只有 1（本该是 2）。**教训：话题要按"谁需要重算"定，不能按"涉及哪个对象"定。**
2. **派生聚合漏刷**：工具栏的"音符：N"读的是缓存，而我只把它挂在 `Meta` 脏位上 ——
   加音符走 `Notes` 话题，于是截图里出现"加了 3 个音符、工具栏仍显示 3001（真实 3003）"。
   截图比计数器更容易发现这类错：计数器会说"重建了 1 次"，但没人保证重建的**内容**正确。
   修法：文档级聚合（音符总数/判定线数）随任何结构话题一起刷新。

其余测试侧教训：`ui_stats` 里的 `f64::NAN` 会被序列化成 `null`，脚本按数字格式化会直接抛异常（已按"无样本"处理）。


## 7.6 判定线与事件：父对象优先（Linux 侧，2026-09-27 续）

### 需求

用户给的顺序是**依赖顺序**，不是功能清单：**"音符应依赖于判定线。判定线可以有多条。先实现判定线和事件，再加入音符。"**
所以这一轮先把父对象（判定线 + 事件轨道）做扎实，再把音符作为子对象挂上去 —— 反过来做（先铺音符再补线）
会得到一个"音符不知道自己在哪条线上"的模型，判定线一转就全错。

### 依赖方向落到代码里

```text
Document
  └─ judgeLines[]                 # 父对象
       ├─ layers[] → 事件轨道（moveX/moveY/rotate/alpha/speed）
       └─ notes[]                 # 子对象：只存线本地坐标 laneX
```

| 层 | 文件 | 谁依赖谁 |
|---|---|---|
| 求值 | `perf.rs` | 拍↔秒分段映射、29 个缓动的函数本体、`eval_events`/`line_perf`/`sample_track` |
| 视图 | `state.rs` | `Chart` = **判定线的列表**（不再有"整谱扁平音符表"）；`Line{notes, tracks[5], perf()}`；音符带 `doc_index` 以便回写命令 |
| 渲染 | `render.rs` | 线与它的子音符走**同一个**变换（先旋转后平移）；实例新增 `angle`，着色器里旋转角点 |
| 面板 | `main.rs` | 左侧四级：判定线 → 事件轨道 → 事件 → 子音符；时间轴画**事件曲线 + 事件条 + 子音符** |

关键决定：
1. **取消扁平音符表**。`Chart.notes` 被删掉，改为 `Chart.lines[i].notes`。所有按时间裁剪的查询变成"逐线"（`visible_range_of(line)`）。
2. **缓动必须真求值**，不能线性近似 —— 29 个具名缓动的函数本体落在 `perf::ease`（口径与 `spec/easing.json` 一致：函数本体用通用实现）。
3. **多 BPM 时间映射**：事件全是拍域的，`bpmList` 分段线性映射在这一轮补上（此前只按首个 BPM 换算）。
4. **音符只存本地坐标**。屏幕位置 = `perf.apply([laneX, y_from_time])`；父线旋转时，子音符的中心与倾角一起转，不需要给音符记绝对位置。
5. `alpha = 0` 的线与它的子音符**不上报实例**（透明的东西不该占渲染预算）。
6. 实例发射顺序成为**合同**：按 zOrder 逐线、每条线先本体后子音符 —— 测试据此精确断言"改动只影响了某条线的实例"。

### 逐线脏位（把 §7.5 的机制细化一层）

| 话题 | 整表 | 该线属性 | 该线子音符 | 该线事件轨道 | 检查器 |
|---|---|---|---|---|---|
| `Meta` | | | | | |
| `Bpm`/`LineList` | ✅ | | ✅ | ✅ | ✅ |
| `LineProps[i]` | | ✅ | | | |
| `Notes[i]`/`Note[i]` | | | ✅ | | ✅ |
| `Track[i]` | | | | ✅ | |

### 实测

数值（180 BPM，`opm-ctl lines --at`）：

| 时刻 | 1 号线 rotate（`outCubic` 0→90°） | 2 号线 moveY（`inOutSine` -250→250） |
|---|---|---|
| 0 拍 | 0.00° | -250.00 |
| 16 拍（中点） | **78.75°** ← 线性会给 45° | **0.00** |
| 32 拍 | 90.00° | 250.00 |

几何（无头出图 + 应用自截屏，`artifacts/lines-events-ui.png`、`artifacts/multiline-demo.opm.json`）：
4 条线各自处于不同的平移/旋转/透明度，**旋转 78.75° 的那条线，它下面的音符也是 78.75° 的斜块**；
`alpha` 淡出的那条线与它的音符一起变淡；`-45°` 起手的线是斜的、到中点回到水平。

逐线更新（4 线 400 音符，`--attach`）：`Track[0]`→只有该线轨道缓存 +1；`Track[3]`→同样只 +1；
`Notes[2]`→只有该线音符缓存 +1（+检查器）；`LineProps[1]`→只有该线属性 +1。**6 条改动，0 次整表重建。**

测试：`cargo test` 15 条全绿 —— `tests/perf.rs` 6 条（29 缓动端点/镜像/已知值、多 BPM 往返映射、事件求值用自身缓动、
`rotate→translate` 顺序）、`tests/lines.rs` 7 条（音符归属、**旋转/平移下子音符同步变换**、改一条线不动另一条线的实例、
alpha=0 不上报、逐线轨道缓存、`lines_report` 数值）、`tests/broadcast.rs` 2 条。

### 给 agent 的新入口

- `opm-ctl … lines [--at SEC]`：判定线的**数值快照**（属性 + 每条轨道的事件数 + 该时刻求值）。
  读图回答"看起来对不对"，`lines` 回答"事件到底在不在、此刻取到什么值"。
- `opm-app --shot PATH --shot-frame N [--shot-exit]`：**应用自截屏**。

### 本轮踩的坑（都记下来，避免重犯）

1. **`Document::default()` 自带一条判定线**，测试里 `push` 之后读 `[0]` 拿到的是那条空线（events 全是 0）。
   表现极具迷惑性：`move_x.len()` 在 push 前是 1、push 后读 `[0]` 变 0 —— 像是"push 丢了数据"。
   用探针逐层打印才定位。**教训：默认值里藏着实体时，测试要先 `clear()`。**
2. **面板行文本超宽会折行**，而折行让"这一行的值"看起来属于下一行 —— 我自己据此误判成"表演值错位"，查了半天才发现是排版。
   现在行文本按面板宽度（300px ≈ 41 个等宽字符）压到单行。
3. **Wayland 下 `with_position` 只是提示**（实测被无视），据此外部猜坐标裁剪会裁错；
   "按深色像素找窗口"也不可靠（终端底色接近）。→ 改成**应用自截屏**（egui viewport 截图），
   与合成器、遮挡、缩放全无关，顺带给 agent 一个"看界面"的能力。
4. **测试断言里的"可见音符数"要按前瞻窗口算**：前瞻 2.0s 只覆盖每线第 1 个音符（1.333s），
   我按"每条线 2 个音符都可见"写断言，错了 3 处。
5. 结构体字面量里的 `insp: None` 把上一行刚算好的 `insp` 初值盖掉了 —— 检查器启动后一直空白，
   靠截图才发现（编译器不会提醒"你有个同名局部变量没被用"）。
6. `cargo build | grep -E "^error"` 看着"没报错"就启动了旧二进制 —— **构建失败时旧二进制仍然存在**，
   必须看 `cargo build` 的最终状态（`Finished` / 退出码），不能只看 grep 有没有命中。


## 7.7 预览窗口的边界框与判定线长度（Linux 侧，2026-09-27 续）

### 需求与口径核实

用户要求：**给 GUI 的预览窗口加一个框以提示边界，参照 RPE 设置窗口边界值和判定线长度。**
"参照 RPE"就得先把口径查准，不能凭印象 —— 直读 [Phira Documents · 普通事件](https://teamflos.github.io/phira-docs/chart-standard/chart-format/rpe/event.html)
与 [音符](https://teamflos.github.io/phira-docs/chart-standard/chart-format/rpe/note.html)（CC-BY-4.0）后确认：

- **窗口边界**：坐标系锚点在屏幕中心，**X ∈ [−675, 675]，Y ∈ [−450, 450]** ⇒ 1350×900 的 3:2 矩形；
- **判定线长度**：RPE 的判定线对象**没有长度字段**（只有 name/bpmFactor/zOrder/isCover/layers/notes），
  线画多长是编辑器/播放器的呈现约定；
- 顺带核到两条会影响后续 codec 的差异：RPE 的线 `alpha` 与音符 `alpha` 都是 **0~255**（线 alpha 为负还会连该线所有 Note 一起隐藏），
  而 opm 用 0~1（导入须 ÷255）；音符 `positionX` 就是"相对判定线中心点的 X"（与 §7.6 的模型一致，验证了那个设计）。

于是 opm 的处理是：**边界是坐标系事实（进规范），线长是编辑器设置（不进格式）** —— 与"变速 hold 的语义交播放器"同一个判断。

### 实现

- `state.rs`：`RPE_WINDOW_HALF_W/H`、`RPE_WINDOW_W/H` 常量；`EditorState{show_boundary, line_half_w, boundary_dim}`（都是视图设置）。
- `render.rs`：`push_window_dim`（4 块压暗）+ `push_window_frame`（4 边 + 8 角标）+ `push_window_overlay`（收尾调用）。
  **用实例画而不是 egui 画笔**：GUI 与无头出图共用同一份几何，避免两套实现漂移 —— 而"边界在哪"必须两边一致。
- 绘制顺序是**内容 → 压暗 → 边框**：压暗在内容之上 ⇒ 跑到窗口外的音符会变暗但仍看得见
  （那是个该被发现的错误，不该被藏起来，也不该和窗口内一样亮）；边框最后 ⇒ 边界始终清晰。
- GUI：工具栏加 `边界框` 勾选 + `线半长` 拖动框；预览里画线端点（黄色圈 + 刻度）与"线 #N 长 L"；
  边界左上角标注 `RPE 窗口 1350×900（X ±675 / Y ±450） 预览缩放 s×`；检查器与状态栏同步显示。
- CLI：`--boundary on|off`、`--line-len N`；出图子命令 `--line-len` / `--no-boundary`；
  `render` 命令也接受 `lineLen` / `boundary` 字段。无头出图默认画边界（agent 的图需要同一套参照）。

### 实测与验证

- **几何断言**（`tests/lines.rs::window_boundary_marks_rpe_extent`）：4 条边正好落在 (0,±450)/(±675,0)；
  压暗块的**绘制下标必须 ≥ 内容长度**（否则盖不住窗口外的东西）；压暗之后恰好剩 4 边 + 8 角标；
  关掉开关后实例数恰好少 16。
- **像素核对**（无头出图 1280×720，scale=0.8）：窗口 x∈[100,1180]、
  边界外 (27,27,34) / 边界内 (56,56,69)、边框线正好落在 x=1180 —— 与 ±675×0.8 精确吻合。
- `--line-len` 的断言：线本体半宽 = 设置值，且**改线长不增减音符实例**（音符位置只由 laneX 与时间决定）。

### 本轮抓到的两个真问题

1. **边框画成了 1350×1350 的方块**：`for y in [-HALF_H, HALF_W]` —— 变量别名（`HW`=半高、`HH`=半宽）用混了，
   水平边被放到 y=±675。**两张截图都没让我看出来**（16:9 视口下 y=±675 的水平边落在视口外/边缘，我把它读成了"窗口占满高度"），
   是新写的几何断言一句话把它钉出来的（"缺上边"）。**结论：能被量化的东西就别靠看图。**
2. **压暗观感与色彩空间有关**：目标是 sRGB 时硬件在线性空间混合，名义 0.42 只得到 0.78 的观感变暗（56 → 42，
   数值正好等于线性混合公式）。GUI 的目标是 `Rgba8Unorm`（混合发生在存储空间）、无头是 `Rgba8UnormSrgb`，
   同一个 alpha 两边观感不同 ⇒ 加 `render::dim_alpha_for()` 按目标格式换算。
   这是**近似**：sRGB 不是纯幂函数（有 +0.055 的线性段），不存在对所有亮度都精确相等的 alpha，
   实测换算后暗背景约 0.48、中灰约 0.53（目标 0.58）。**能保证的是"窗口外一定明显更暗"，不是"两边逐像素相同"。**


## 7.8 音频播放与自动播放：播放头 = 音频时钟（Linux 侧，2026-09-27 续）

### 需求

用户要求：**实现音频播放和自动播放（按空格键或对应 cli 命令）。**

### 设计：为什么"播放头必须等于听到的位置"

S2 spike 的结论是"偏差**有界**即通过"，而真正要解决的问题是：谱面与音乐若各走各的时钟，
误差会一直累积（同机实测两者相对漂移 7.4 ppm ≈ 每小时 27 ms），编辑器里"看着谱面打拍子"就没意义了。
所以这里不做"双时钟 + 伺服校正"，而是**只留一个时钟**：

```text
cpal 输出回调（实时线程）──写──▶ Arc<AtomicU64> 已送入设备的帧数（唯一真相）
                                      │
GUI 线程 ──读──▶ playhead = 帧数/采样率 − 输出延迟 + 用户校准
```

- **输出延迟自校准**：首个回调里取 `info.timestamp()`，`playback.duration_since(callback)` 就是
  "这批样本多久之后才会响"（实测本机 24.00 ms），不写死经验值，换设备/缓冲都不用改代码。
- 于是 `playhead(t)` 恰好等于"此刻正在响的那个样本的时间戳"，而不是"已送去的时间戳"。
- 墙钟只在**无音频**时兜底（`state::advance(audio: Option<&Audio>)`）。

### 实现要点

| 关注点 | 做法 |
|---|---|
| 解码 | 自己写 `read_wav`（PCM 8/16/24/32 与 float32/64、`WAVE_FORMAT_EXTENSIBLE`）——零新增解码器依赖；**ogg/mp3 明确报错**，不静默无声 |
| 声道/采样率 | 载入时线性重采样到设备规格（预览够用，不吹音质）；只走 f32 输出，别的格式直接报错 |
| 共享状态 | 回调写 `AtomicU64` 帧游标；GUI 只读 —— 不需要锁，也不会因 UI 卡顿影响实时线程 |
| 混音 | 抽成纯函数 `fill_frames(samples, ch, cur, playing, out) -> (新游标, 状态)`，**可单测**（见下） |
| 视图命令 | `play/pause/toggle_play/seek{to|beat}/audio/audio_offset/view` 走**独立队列**（`control::ViewQueue`），不进 EditCore —— 播放头是视图状态，不属于文档、不进撤销栈 |
| 双入口一条路径 | 空格键与 CLI 都调 `App::toggle_play()`；空格带"控制台输入时不触发"的守卫（抽成 `should_toggle_play` 并单测） |
| 观测 | `ui_stats` 增 `playing/playhead_sec/playhead_beat/audio*/audio_dev_ms/audio_window_s`，视图命令的效果靠它确认 |

### 实测（180 BPM 节拍器 WAV / 48 kHz / PulseAudio → USB 声卡）

| 项 | 实测 |
|---|---|
| 播放头 vs 音频游标 | 同一位置（快照内 0.00 ms；差值是"一帧内的推进"） |
| 输出延迟（自校准） | 24.00 ms |
| 速率（跳过 1 s 稳定期按斜率） | −84 ~ +715 ppm（多次窗口）；偏差 ±7 ms **有界振荡、不增长** |
| 欠载 | 0（20 s 连续播放） |
| `seek beat=48` @180 BPM | 16.0000 s（48×60/180） |
| 暂停 1.2 s | 播放头 0.00 ms 不动 |
| 播到音频末尾 | 干净停下（`playing=false`），无欠载 |
| 服务器侧 | `pactl`：`float32le 2ch 48000Hz` + `Corked: no` |

### 本轮抓到的两个坑

1. **帧 ≠ 样本（单位混用，差正好 2×）**：游标存的是交错样本下标，而 `position = 游标/采样率` 把它当帧数，
   立体声下播放头跑得比音频**快一倍**（声音本身速度却是对的，所以听不出来，只能靠量）。
   **过程教训更值钱**：我第一次"修"它时改的是一段已经被改过名字的代码，`str.replace` 静默失配 —— 补丁根本没打上，
   再测仍然 2×。修法：替换后**断言真值变化**（`assert old in s`），并顺手把混音抽成纯函数 + 回归测试钉住单位。
2. **别只报 ppm**：偏差是**有界量**，而短窗口的固定偏差除以窗口会算出几千 ppm 的假数字
   （实测 2.4 s 窗口报 −5235 ppm，同一次播放 20 s 窗口只报 −1735 ppm，且偏差始终在 −25~−35 ms 振荡）。
   S2 spike 的文档里就写过"混算成单一 ppm 会误导"。最终口径：**跳过 1 秒稳定期、按斜率算速率，同时给出累计偏差(ms)+窗口(s)**。


## 7.9 编辑区叠加层 + 主流音频格式（Linux 侧，2026-09-27 续）

### 编辑区：为什么是"叠加层"而不是又一个面板

用户要求：**左半边音符轨道区、右半边选中判定线事件区，纵轴是节拍数，叠加在预览区上，自动播放或按住 H 时隐藏。**

按这条要求，编辑区不是"第五个面板"而是**演奏区里的一层**，理由是任务本身需要同时看两件事：
演奏区是"游戏里会变成什么样"的唯一视图，而编辑要盯数据。做成并排面板就得来回挪视线/被挤窄；
叠加 + `H` 一键藏，就把"编"与"看"的切换成本压到一次按键。

- **纵轴在中间、用拍（不是秒）**：编谱时想的是"第 32 拍"。轴画在两区**之间**（34px 轴带，拍号居中写在带里、
  两侧小刻度指向相邻半区、两半的网格线在带边停住）—— 因为它是两区**共用**的轴，贴左边缘会让人以为它只属于左半；
  点在轴带上不选中任何东西（轴是骨架不是内容）。窗口 = `[播放头−2 拍, +30 拍)`（`OverlayCfg{beats_visible, lead_beats}`），
  暂停在哪就停在哪；可见拍数由工具栏/`--overlay-beats` 调。
- **左半 = 当前判定线的音符**（x 轴 = `laneX` ±675 铺满半宽，Hold 画竖条、假音符画空心框）；
  **右半 = 当前判定线的 5 条事件轨道**（每个轨道一列，事件 = 起止拍之间的块，块内标起止值与非线性缓动名）。
  恒定事件会跨满整个可见窗口 —— 所以块宽只占列宽 55%，并**必须写数值**，否则"一条事件"和"列底色"分不出来。
- **半透明 150/255**：叠加不是替换，下面的预览要看得见；实测第一版用了 205 就"把预览盖没了"。
- 只产出动作（选中/定位），由调用方施加 —— 与判定线树同一条规矩：**界面不直接改数据**。
- 可见性 `enabled && !playing && !h_held` 抽成纯函数并单测（这条规则窄，容易被"播放时也显示吧"改坏）。
- 左上角是 RPE 窗口标注的地盘：编辑区开着时不画那段文字（两套 chrome 叠字会互相糊掉），边框本身仍在。
- **滚轮 = 改当前时间**（用户要求）：编辑区的窗口是跟随播放头的，所以"滚动视图"与"移动播放头"在这里本来就是同一件事，
  不需要额外的滚动偏移状态。约定向上滚 = 时间往后（与纵轴"越上越晚"一致），步长随可见拍数缩放
  （32 拍可见时一格 2 拍；放大到 8 拍时一格 0.5 拍 —— 无论缩放级别，"一格走过的屏幕距离"都差不多）。
  换算抽成纯函数 `scroll_delta_to_beats` 单测；施加路径用等价的 `{"op":"nudge","beats":N}` 端到端验证
  （滚轮事件注入不了 Wayland 窗口，这是本机唯一能做的诚实验证）。
- **底板黑度做成旋钮**（用户要求"更黑"）：默认 0.82（早先我按"叠加不是替换"设成 0.59，数据可读性不够）。
  这条取舍没有唯一答案 —— 编谱时以数据为主、看谱时以预览为主，所以给工具栏一个 `暗度` 拖动框
  与 `--overlay-alpha`，而不是把某个值写死。
- 实测：`nudge` +2 拍 → 18.00 拍、−4 → 14.00、10×+1 → 24.00、往前滚过头停在 0 拍，全部精确落位。

### 音频格式：把手写的 WAV 解码器换成 symphonia

上一轮的 WAV 解码器是**零依赖**的（自己读 RIFF + PCM/float），唯一优势就是这点。
一旦需求变成"主流格式"，自己写 OGG/MP3 解码器就不是省事而是冒险 —— 那是成熟库的活儿，
自研该留在判定线/事件/格式这些**本项目独有**的地方。于是改用 **symphonia**（纯 Rust、活跃维护，features 按需开）。

实测同一份 48 kHz 单声道素材的五个格式，全部解出 60 s：

| 文件 | 解码器 | 结果 |
|---|---|---|
| wav | PCM (wav) | 48000 Hz / 1ch / 60.000 s |
| flac | FLAC | 48000 Hz / 1ch / 60.000 s |
| mp3 | MP3 | 48000 Hz / 1ch / 60.000 s |
| ogg | OGG Vorbis | 48000 Hz / 1ch / 60.000 s |
| m4a | AAC (m4a) | 48000 Hz / 1ch / 60.032 s（AAC 编码器补帧） |

配套加了 `opm-app --audio-probe FILE`：只解码、不开窗口、不需要音频设备，输出 JSON 规格
（agent 判断"这个音频能不能用、多长、什么编码"用得上）。`symphonia` 的 `Display` 给的是十六进制 id（`0x1006`），
所以自己映射了可读名（MP3 / OGG Vorbis / FLAC / AAC…）。

**踩到的小坑**：`--audio-probe` 的 JSON 混在启动 banner 之后输出，脚本直接 `json.loads(stdout)` 会炸 ——
解析时要从第一个 `{` 开始截（这是脚本侧的坑，不是程序问题，但值得记：机器可读输出别和人类 banner 混流）。


## 7.10 顶栏 / 网格吸附 / 修掉两个核心 bug（Linux 侧，2026-09-27 续）

用户要求：**放置顶栏，包含基础的设置/保存/编辑网格线数量（横向坐标吸附或纵向节拍吸附）。清理用于调试的按钮和文字提示，保留 cli 接口。**

### 顶栏

只有真设置，没有调试开关：`播放/暂停 · 回到开头 · 保存(•) · 谱面摘要 · 网格 1/N · 横向吸附 · 纵向吸附 · 设置⚙`。
`设置⚙` 里收着编辑区（开关/可见拍数/暗度）、窗口边界框、判定线半长、命令控制台、校验、工作区。
保存走 `{"op":"save"}` —— 和 CLI **同一条命令**，不另开一条"直接写文件"的路径。
脏标记口径**保守**（`revision != saved_revision`：撤回保存点之后仍算脏，只有再保存才清），
理由与"宁可多提示一次、不要看起来干净其实没存"一致。

清理：`压力模式`、帧时间/实例数、广播重建计数、适配器这些只留在**调试工作区**与 CLI
（`--stress`、`--verify-align`、`ui_stats`、`--verbose-updates`）；顶栏与状态栏平时只有
播放头（秒/拍）、音频与校准、网格/吸附、编辑区状态。

### 网格与吸附

一个"网格线数量"（每拍细分 `1/N`，N ∈ 1/2/3/4/6/8/12/16）**同时**决定：画多少格线、以及吸附到哪。
两轴共用同一个 N（编谱时想的是"这个 1/4 拍的格子上有没有东西"），但吸附可分别开关：

- 纵向：拍 → 1/N 拍（`snap_beat`）；
- 横向：`laneX` → 1350/(N×4) RPE 单位的整数倍（`snap_lane`），音符区里画成竖线。

**吸附不只是手感**：它保证写回文档的拍是**有理数 k/N**（`beat_json` 给 `[n,d]`），
而不是浮点抖出来的 `0.3333333` —— 对"opm 用有理拍"的设计来说，这是正确性而非体验问题。
细分太密时（可见拍数 × N 超过像素预算）细线自动不画，避免糊成一片。

编辑手势：拖动音符块（横向改 laneX、纵向改拍，Hold 保持时长）、双击空白放 Tap（同样吸附）。
**整段拖拽 = 一个撤销步**：`begin` → 逐帧 `set_note` → `commit`。

### 这轮被迫修掉的两个核心 bug

做拖拽时发现"事务内不广播"这条早早定下的规则站不住：拖拽是一串连续改动，
若都压到 `commit` 才广播，**拖动过程中界面根本不更新**。于是改成**逐条广播、撤销仍按事务一步** ——
"广播粒度"与"撤销粒度"本来就是两件事，早先被我绑在一起。
（`abort` 仍发一条全量话题让订阅者重取，用来纠正"被丢弃的改动也曾被订阅者看到过"。）

顺着这条改，又抓出一个更严重的：**`abort()` 只做了 `pending = None`，没有回滚文档**。
`mutate` 是直接改文档再把逆操作记进日志的，所以丢掉待提交列表 ≠ 文档回滚 ⇒ 事务里的改动变成
"改了但撤不回"（既不在撤销栈里、也没还原）——这直接推翻了我在 `core.rs` 注释里写了很久的
"失败的命令不改文档"。改成 `abort(doc)` **逆序逐条 revert**，失败路径也走它；
并补了断言：abort 后文档回到事务前、无撤销步残留、之后还能正常继续用。

两个 bug 都是**写测试写出来的**（逐条广播的断言 → 发现 abort 不回滚），不是读代码看出来的。

### 验证

- `cargo test` 26 条全绿（broadcast 3 + lines 12 + perf 6 + audio 5），0 警告；
  新增：网格吸附精确性（含 `[1,3]` 有理数）、abort/失败回滚、事务"广播逐条 + 撤销一步"。
- 拖拽等价序列实测：3 帧 → **3 条广播**、撤销 0→**1** 步、文档里是 `startBeat 11/4` 与
  `laneX 168.75`（= 2×84.375，吸附网格上）、撤销后回到 `0/1` 与 `-480.0`。
- 滚轮换算仍是纯函数单测（鼠标/滚轮事件注入不了 Wayland 窗口）。


## 7.11 数据边界：文档 vs 视图（Linux 侧，2026-09-27 续）

用户把边界说清楚了：**EditCore 只管最终会保存到谱面文件的数据；GUI 不能私自更新可能未到 EditCore 的数据，
但 GUI 内部的规则可以自行更新（例如网格吸附和谱面文件无关，谱面只有坐标）。**

### 把纪律交给编译器，而不是注释

这条规则最容易的失败方式是"某处图省事直接改了 `doc`"。所以 `EditCore::doc` 改成**私有字段**，
外部只能 `doc()` 拿 `&Document` —— 拿不到 `&mut`，也就没有第二条写路径。所有客户端
（GUI 控件、`opm-ctl`、`--attach`、控制台）都只能发命令。

迁移时抓出两处"飞地"（都在核心之外直接写文档）：
1. **GUI 的演示谱面引导构建**：原来 `build_demo_doc(&mut core0.doc, …)` 直接构造 `Document`；
   现在改为 **449 条命令**（`set_bpm`/`set_meta`/`add_line`/`set_line`/`add_event`/`add_note`），
   实测 **0.01 s** 建完 400 音符的演示谱面。顺带的好处：这条路径每天都在验证命令语言本身。
2. **`opm-ctl new`** 的元信息与 BPM 直写：改成走 `set_meta`/`set_bpm`。

改动后**等价性已验证**：命令路径生成的演示谱面与旧的直接构造版
（`artifacts/multiline-demo.opm.json`）**逐字段一致** —— 曲名、BPM、4 条线的 name/zOrder/isCover/bpmFactor、
每条线 5 条轨道的全部事件（起止拍/起止值/缓动）、400 个音符（拍/类型/laneX/时长）全部相同。

### 视图状态：改了又改，文档一个字节都不变

音频是这条边界最好的例子，两条路刻意分开：

| 目的 | 命令 | 走哪 |
|---|---|---|
| 只换**预览**用的音频 | `{"op":"audio","path":"x.ogg"}` | 视图队列（`ViewCmd::LoadAudio`），**不动文档** |
| 改**文档**里的音频 | `{"op":"set_meta","set":{"audio":"x.ogg"}}` | EditCore：广播（话题 `Meta`）、可撤销、会写进文件 |

实测：换预览音频后 `dump` 的 `meta.audio` 不变；`set_meta` 改完广播话题是 `Meta`，撤销能还原；
撤销文档字段不影响正在预览的音频。

`tests/boundary.rs` 三条断言：
1. **文档核心不认识视图命令**（`play`/`pause`/`toggle_play`/`nudge`/`ui_stats` 一律 `ok:false`），
   而它们都能被控制通道翻译成视图命令；`view` 是查询、不排队；
2. 音频的文档路径可广播/可撤销/可为 `null`，视图路径在核心侧根本不认识（`ok:false`）；
3. **改一圈视图状态**（网格 div、两轴吸附、前瞻、线长、边界框、暗度、叠加层开关与可见拍数、
   选中线/轨道/音符、播放头、播放状态）之后：文档 JSON **逐字节相同**、`revision` 不动、
   "是否未保存"也不变。

顺带把叠加层的开关与可见拍数从渲染参数（`OverlayCfg`）挪进 `EditorState`：**视图状态统一在一处**，
渲染配置只留纯渲染参数（黑度、领先拍数），避免"同一件事两个地方存"。

### 本轮顺带发现（记录，未修）

换预览音频会**卡住一帧**：`Audio::load`（解码 + 声道/采样率对齐 + 建流）跑在 UI 线程上。
实测 60 s 文件 0.1~0.45 s 解码（含进程启动：wav 358 ms / ogg 647 / flac 444 / mp3 756 / m4a 446），
端到端从发命令到界面反映约 0.9 s。改法很明确（解码丢工作线程，回 UI 线程再建流 —— `cpal::Stream`
不一定 `Send`），属于下一步；这里如实记下而不是让它埋在"音频已经能放了"的印象里。


## 7.12 事件头尾拖拽 / 属性编辑器 / 网格即格点集（Linux 侧，2026-09-27 续）

用户四条要求一起提：**① 事件块对准头/尾能快速调起止时间，且"某时间没有事件就不移动判定线"；
② 属性检查器变属性编辑器；③ 音符拖拽遵循网格吸附、只能放在横纵网格交叉点；
④ 横向/纵向吸附改成"拍方向每拍 N 条 / 坐标方向窗口 N 等分"两个可指定的数。**

### ③④ 合成一个模型：网格 = 可以放音符的格点集

原来的两个吸附勾选框换成**两个数**（用户说的两种形式），音符的拖拽与双击放置**总是**吸附到两轴网格的交叉点：

| 方向 | 规格 | 步长 |
|---|---|---|
| 拍（纵向轴） | 每拍 `beat_div` 条 | `1/beat_div` 拍 |
| 坐标（横向轴） | 可见窗口 `lane_div` 等分 | `1350/lane_div` RPE 单位 |

"只能放在交叉点"是字面实现的：界面上不存在格点之间的音符（自由坐标仍可走命令，那是 agent 的事）。
这同时解决了格式侧的一件事：写回文档的拍是**有理数 k/N**（`beat_json`），不是浮点抖出来的 `0.3333333`。
测试 `grid_is_a_lattice_and_snapping_lands_on_it` 直接断言"吸附后的值必须是步长整数倍且不超出窗口"。

### ① 事件头尾：一条命令，**只控制一个事件**（语义改过一次）

`{"op":"resize_event", …, "edge":"start"|"end", "toBeat":[n,d]}`。

第一版把"如果某时间没有事件则不移动判定线"读成了"必须保持无空隙"，于是实现成**同步邻块**
（拖 A 的尾就把 B 的起也挪过去）。用户随后明确否掉：**一次拖拽同时改了两个事件**。
现在的分工是：

| 关注点 | 落在哪 |
|---|---|
| 控制柄只控制**它自己**那个事件 | `resize_event`（只改 start 或 end，不碰邻居；越界"头拖过自己的尾"仍拒） |
| 拖出来的**重叠** | 重叠检测 → 底栏红字 + 冲突浏览器（§7.14 ③④） |
| 拖出来的**空隙** | `validate` 报"轨道不连续（空隙）" |
| 空隙里判定线**不许自己动** | 求值器 `perf::eval_events`：空隙中**保持前一条事件的终值** |

最后一条这里修过一个真 bug：早先落在"不在任何事件内"时会返回**最后一条事件的终值** ——
于是事件之间留了空隙时（[0,10] 与 [20,30]，拍 15）线会直接跳到末值，看上去就是"没有事件却动了线"。
现在按"最近的前一条事件的终值"保持，首个事件之前取它的起始值；测试 `events_evaluate_with_their_own_easing`
里加了空隙断言（15 拍 ⇒ 5 而不是 99）。

UI 侧：光标进入块上下 6px 内算"抓头/抓尾"（选中块的该边会亮），拖动走事务 ⇒ 整段调整 = **一个撤销步**。

### ② 检查器 → 编辑器

右侧栏改成可编辑：判定线（名字/zOrder/isCover/线速）、事件（起止拍、起止值、缓动下拉）、
音符（类型、拍、时长、laneX、alpha、假音符、speed、宽度、yOffset）。
**所有改动都发命令**（`set_line`/`set_event`/`resize_event`/`set_note`）—— 与 agent 走 `--attach` 是同一套命令；
编辑器不缓存文档，刷新靠广播。事件头尾用 `resize_event`（会同步邻块），值/缓动用 `set_event`。

活体往返实测（GUI 在跑，`--attach` 发编辑器同款命令）：`set_note{kind:flick,startBeat:[13,4],laneX:84.375,alpha:128,isFake:true}`
→ 文档 `[13,4] / 84.375 / 128 / isFake=true` + 广播 1 条 → `undo` 回到 `[0,1] / -480 / 255`；
`resize_event` 把 alpha 事件起点从 32 拖到 20 → 该事件起 20、**邻块止 20 且止值保持 1.0**。

### 顺手加的一条视图命令

`{"op":"select","line":L,"track":"alpha","note":N,"event":M}`：选中是**视图状态**，走视图通道（不进 EditCore）。
它的用处是让 agent/脚本把界面"指到"某个对象再截图 —— 也是我能给属性编辑器截图的原因（否则没法用命令制造选中态）。

测试：**30 条**全绿（broadcast 4 + lines 12 + boundary 3 + perf 6 + audio 5），0 警告。


## 7.13 修"事件头尾有提示但拖不动"（Linux 侧，2026-09-27 续）

用户报：**光标移到事件块头尾时有提示但无法拖动；并且希望此时光标变成双头箭头。**

这次没有靠肉眼试鼠标，而是先补了一个**无头 egui 合成事件测试**（把 `RawInput` 喂给 `Context::run_ui`，
脚本化"悬停 → 按下 → 移动 → 松开"，断言 overlay 产出的动作序列）。它一次抓出三个真 bug：

1. **拖拽处理被嵌在 `else if resp.clicked()` 里** —— 拖拽时 `clicked()` 为假，整段代码根本执行不到。
   这正是用户看到的"有提示但拖不动"（提示是绘制循环里的独立逻辑，所以照常显示）。改成"先算命中、再分派事件"，
   拖拽与点选互不嵌套。
2. **拖拽阈值 vs 6px 把手段**：egui 要等指针移动几个像素才认定为拖拽，而那时指针**已经离开**把手段，
   用"当前命中"判断必然失败 ⇒ 改为记住**按下时**的命中（存 egui 临时内存）。
   同时发现 `if drag_started {..} else if dragged {..}` 会把**判定为拖拽的那一帧**跳过 ——
   于是"按下→小幅移动→松开"这类快速拖拽**一次位置更新都发不出去**（还是拖不动）。改成"开始或进行中都发位置"。
   测试里那条 `EventResize(Start, …)` 断言，就是把这两条钉住的。
3. **起点/终点方向反了**：纵轴是拍且越上越晚 ⇒ 事件**起点在下方**，而判定里把块的**上边**当成了 Start。
   后果是"抓到头却去改尾"（测试输出 `EventResize(End, 0.000)`）。改成按语义传 `start_y/end_y`，
   不再靠 rect 的上下关系猜方向；hover 高亮也走同一个判定（避免"亮上边却抓下边"）。

顺带清理：交互块与滚轮块各重复了一份（滚轮一格会走两倍），现在各只有一处。

### 光标：双头箭头

悬停在头/尾 → `CursorIcon::ResizeVertical`（上下双头箭头）；音符 → `Grab`/`Grabbing`；标尺 → `PointingHand`。
**验证方式**：系统光标是合成器画的，自截屏（应用自己的帧缓冲）里**永远不含光标**，
所以改成断言 egui 的输出：`FullOutput::platform_output.cursor_icon` 必须是 `ResizeVertical`（无头测试里可读）。

### 为什么这次的测试能抓住

前几轮的鼠标交互只能"发命令 + 断言文档结果"（等价路径），抓不到**UI 结构错误**；
而无头 egui 能把指针事件真的喂进去，于是"命中算出来了但分支进不去""方向反了""阈值吃掉了第一帧"
这类错误才暴露出来。代价是要处理两个细节：字体图集的 `textures_delta` 必须 `clear()`
（无头跑没有渲染器消费它，直接 drop 会 panic）；egui 的**第一帧没有交互状态**，所以"只在未按下时记录悬停命中"
会漏掉按下那一帧。

测试：**32 条**全绿（含 2 条 bin 内的 overlay 测试），0 警告。


## 7.14 事件块渐变 / 边界优先规则 / 重叠检测与冲突浏览器（Linux 侧，2026-09-27 续）

用户四条：**① 事件块整块颜色渐变以分清头尾；② 头尾相接时优先选中的事件，都没选中则选尾巴；
③ 加事件重叠检测（每次更改后查这次改动、加载时全量查），必要时底栏红字报错；④ 加冲突浏览器，显示重叠区域，点击跳转。**

### ① 渐变：用**色相偏移**（不是透明度/明暗）

用户明确要求"渐变不要用透明度，用色相偏移"。第一版做成了"暗端乘 0.55 + 降 alpha"，
在深色预览上会糊 —— 改成：起点（下方）与终点（上方）**只差色相 40°**，饱和度、明度、alpha 两端完全相同；
选中态只是整体更实一档（两端仍一致）。`Mesh` 两个顶点色插值，比"画很多细条"干净，也不随块高变化。

**实测口径的一个坑**：`egui::ecolor::Hsva` 在**线性**空间算色相，于是"偏 0.11 圈"在屏幕上只有约 28°
（gamma 是逐通道幂函数，会改变非灰颜色的色相）——我一开始的注释就把 0.11 写成"≈40°"，实测才发现。
现在改为自己在 **sRGB** 空间偏色相（`shift_hue_srgb`），常量的字面值就等于肉眼看到的度数。
单测断言：两端 alpha 相同、色相差 = 40°（±1.5° 量化容差）、max 通道（明度）不变。
屏上抽查（一块 32 拍事件在窗口内的可见部分）：色相 225°→253°、明度恒定 0.984~0.988 —— 即"色相在变、明暗不变"。

顺带把 hover 把手带上了方向：**头**是实线 + 短竖标记，尾是实线。

### ② 边界优先规则（`overlay::prefer_edge`，有单测）

头尾相接时同一条 y 上会有两个候选（前一个的 End、后一个的 Start）。
规则：**优先选中的那个**；都没选中 → **选尾巴**（End，即"止于这一点"的前一个事件）。
候选之间还会按距离挑最近的一批（0.75px 容差），避免相邻但不相接的边界互相抢。

这条规则之所以值得单测：它是"点下去抓到谁"的全部依据，而**拖拽的语义取决于抓到哪一头**
（抓 A 的尾 → A 延长/截断、B 的起值按 B 的曲线重算；抓 B 的头 → 反过来）。

**补的一个视觉 bug**：用户报"拖动头尾相接的事件时两个把手会同时亮"。原因是**决策分了两处**：
拖拽用 `prefer_edge`（只选一个），而把手高亮是在绘制循环里**按块各自判定**的 ——
共享边界上 A 的上边（End）与 B 的下边（Start）在同一条 y，于是两个把手一起亮。
修法不是"再补一次判断"，而是让高亮与拖拽**共用同一个值**：绘制阶段只把块矩形收进 `blocks`，
交互阶段算出唯一命中后用 `handle_to_highlight(dragging, hover)` 决定亮哪一个（拖拽中亮抓住的那一头）。
无头 egui 测试 `hover_on_shared_boundary_hits_single_event` 钉住两端行为：
未选中 ⇒ 只 `SelectEvent(0)` 且 `edge: End`；把事件 1 设为选中 ⇒ 仍然只 `SelectEvent(1)` 且 `edge: Start`。

### ③ 重叠检测：加载全量、改动增量

`cmd::overlaps_in_track` / `overlaps_of_line` / `overlaps`：
按起拍排序后，只有"后一个的起拍早于前一个的止拍**且真的多覆盖了一段**"才算重叠 ——
**头尾相接（前止 == 后起）不算**，那正是格式要求的"无空隙不重叠"。
GUI 侧：加载时全量查一次；之后每次广播只重查这次涉及的线（`Track[L]`/`LineProps[L]`/`Notes[L]`），
结构变化（增删线/BPM）才回到全量。`ui_stats.conflicts` 暴露处数，脚本可断言。

实测（活体 GUI + `--attach`）：加载 0 → 造一处重叠 **1** → 改别的线仍 **1**（增量确实只看动过的线）
→ 撤销掉重叠 **0**；`validate` 给出同源的指针 `/judgeLines[0].layers[0].moveX[1] 轨道不连续（重叠）`。

### ④ 冲突浏览器

底栏红字 `⚠ N 处事件重叠`（可点开/关）；冲突**一出现就自动展开**浏览器（否则用户得先找到入口，太绕；
关掉后不会自己弹回来），每条一行 `线 #L · 轨道 · 事件 i→j 重叠 [起, 止) 拍`，
点击 = 选中该线/轨道/事件 + 把播放头定位到重叠起点（跳转本身是视图状态，不碰文档）。

### 测试

**35 条**全绿：新增 `overlap_detection_reports_real_overlaps_only`（只报真重叠、增量==全量、越界线号不 panic）、
`boundary_prefers_selected_then_tail`（规则本身）、`overlay::tests::prefer_edge_rule`（实现），
以及原有的无头 egui 拖拽测试。0 警告。


## 7.15 修"吸附不落在横轴上"（Linux 侧，2026-09-27 续）

用户报：**音符和事件的网格吸附不遵循规范，每次吸附没有严格吸附到横轴上。**

一量就现形：拍网格的**档位判定用了自增之后的循环下标**（`b += 1` 写在算 `is_beat/is_bar` 之前）：

```rust
let beat = b as f64 * step_beats;   // 用 b 算位置
b += 1;                             // 先自增
let is_beat = b % div == 0;         // 再用 (b+1) 判档 ⟹ 线落在 (div-1)/div + n 拍上
```

于是 div=4 时**带拍号的粗线落在 0.75、1.75、2.75…**，而吸附发生在整拍上 —— 吸附后的音符/事件边界
正好落在细线（甚至被抽稀掉）的位置，看上去就是"没吸附上"。而且抽稀逻辑是"非整拍线可跳过"，
判反了以后**整拍线反而可能被跳过**，更没法对格。

修法不是"把 +1 挪个位置"，而是把"线在哪"抽成纯函数 `overlay::beat_grid_lines(anchor, beats, div)`，
让**绘制与吸附共用同一份定义**：`beat = k / div`、`is_beat = k % div == 0`、`is_bar = k % (4·div) == 0`、
`label = k / div`。同时明确一条规则：**整拍线在抽稀时永不跳过**（它是吸附目标）。

测试 `beat_grid_lines_coincide_with_snap_targets` 钉住三条：
① 每条线都是**吸附的定点**（对线所在拍再吸附不应移动）；
② 小节线落在 4 的倍数上、拍号 == 该整数拍；
③ 每个整拍都必须有且被标记为整拍线（含 div=3 这类"可指定"的细分）。

顺手修掉一个相邻小问题：`lane_div` 为奇数时最外侧那对竖网格线会超出 ±675，被画到轴带/事件区上
（音符区的映射对越界值是钳制的，不能拿它当"线在窗内"的依据）—— 现在只画落在窗口内的线。

证据：`artifacts/grid-align.png`（拍号 12/16/20/24/28 正坐在粗线上，1/4 细分线在它们之间）。
测试 **39 条**全绿，0 警告。


## 7.16 核"横轴是否还有四等分吸附"（Linux 侧，2026-09-27 续）

用户问：**横轴间还有四等分吸附是有意的，还是 bug？** 这类问题不该靠记忆答，先量。

加了 `{"op":"grid","beatDiv":N,"laneDiv":M}` 视图命令（视图状态，进程内改网格，顺带给 agent 一个入口），
然后在运行中的应用里切换设置读实际步长：

| 设置 | 横向步长（laneX） | 纵向步长（拍） |
|---|---|---|
| 每拍 4 条 / 窗口 16 等分 | 84.375 | 0.25 |
| 每拍 **8** 条 / 窗口 16 等分 | **84.375** | 0.125 |
| 每拍 **2** 条 / 窗口 16 等分 | **84.375** | 0.5 |
| 每拍 4 条 / 窗口 5 等分 | 270.0 | 0.25 |
| 每拍 4 条 / 窗口 7 等分 | 192.857 | 0.25 |

结论：**横向步长只由 `lane_div` 决定（1350/N），与节拍细分无关** ⇒ 没有"横向四等分吸附"。
代码里唯一的 `4` 有两处，都是**有意的**：
① `beat_div` 的**默认值** = 4（即默认 1/4 拍吸附，顶栏可改 1..64）；
② `is_bar = k % (div*4) == 0` ⇒ **小节线**（每 4 拍一条粗线带拍号），它只是刻度、不参与吸附。
历史上确实有过真正的"横向四等分"：旧公式是 `1350/(beat_div×4)`（横向步长被绑在节拍细分上还带 ×4）。
换成 `1350/lane_div` 之后，新增断言 `lane_step_is_independent_of_beat_subdivision` 钉住这条独立性 ——
否则哪天有人改回去，"横向跟着节拍走"这种怪现象会悄悄回来。

### 顺带抓到的真问题：奇数等分会让吸附越界

写这条断言时发现：`lane_div` 为**奇数**时，以 0 为中心的格点里窗口边界（±675）不是格点，
而最外侧的格点会落到窗口外（例如 5 等分时 676 最近的是 **810**）—— 而 `laneX` 超出 ±675 是格式不允许的
（`validate` 会报），也破坏了刚立的规矩"吸附就是格点"。修法：**只允许偶数等分**
（`GridCfg::normalize_lane_div` 把任意值规整到偶数、最小 2；UI 与 `grid` 命令都过这道规整），
并在 `snap_lane` 末尾钳到 ±675 兜底。偶数等分下 0 与 ±675 同时是格点，步长必然整除 1350。

实测（release 活体）：16→16（84.375）、5→**4**（337.5）、7→**6**（225）、1→**2**（675），
四者的步长都能整除 1350 ⇒ 0 与 ±675 都是格点。测试 **40 条**全绿，0 警告。


## 7.17 网格"改了没变化"的真因 + Ctrl+滚轮缩放 / 标注自适应（Linux 侧，2026-09-27 续）

用户报：**更改横轴数量后，编辑区的横轴没有改变。** 先量再改。

### 先纠正我上一轮的归错轴

§7.16 我把"横轴"读成了 **laneX（横向坐标）**，于是整篇在证明"横向步长与节拍细分无关"。
先做像素比对来定位：固定 `--beat-div 4`，只改 `--lane-div`（16 / 4 / 32），截同一帧比对音符面板
⇒ 三张图**确实不同**（7375 / 9845 / 17220 个差异像素）。所以横向坐标网格没问题，
用户说的"横轴"是**横线**（拍网格线），"横轴数量"就是顶栏的 **`拍 每拍 N 条`**。
§7.16 的结论（横向没有四等分吸附）依然成立，只是没回答用户真正问的那件事。

### 真因：两套抽稀规则不一致 —— 画线偷偷降级，吸附仍用设定值

绘制里有一条按像素密度的抽稀：`subdiv_visible(beats, div)` 在细线密于 3px 时**只画整拍线**；
而吸附始终按设定的 `beat_div` 走。默认 32 拍可见、面板约 650px 时：

| 设定 `beat_div` | 旧行为 | 用户看到 |
|---|---|---|
| 4 | 128 条 ≤ 阈值 133 ⇒ 全部画 | 细线在 |
| 8 | 256 条 > 阈值 ⇒ **细线全不画** | 与 4 **完全一样** |
| 16 / 32 | 同上 | 还是**一样** |

改数字画面不变、吸附却跑到没画出来的线上 —— **用户前后两次报的其实是同一个 bug**
（§7.15 的"吸附不落在横轴上"是判档写错，这次是画线与吸附的**抽稀口径**不一致）。

修法：只留**一个数**说了算 —— `GridCfg::effective_beat_div(beats_visible)`：

```rust
// 一条细线至少 1.2px；顶不住就退到"设定值的最大约数"（保持 1/2、1/4、1/8 这类整齐步长）
budget = 600.0 / 1.2 = 500
effective = 最大的 d（d | beat_div）使 beats_visible · d ≤ budget
```

`EditorState::effective_beat_div()/snap_beat()/beat_json()` 全部读它：
**画多少线 = 吸附到哪 = 写回文档的有理数分母**。被顶掉时**明说**，不静默：
顶栏琥珀色 `→ 实际 1/8`、状态栏 `每拍 32 条(实际 1/8)`、编辑区标题 `网格 1/8`。
放大（缩小可见拍数）更细的网格就重新可用 —— 是"当前画不出来"，不是"永久禁用"。

实测（`--shot` 自截屏，音符面板 x∈[400,700] 的亮行计数；线加亮后计数整体更高，见 §7.18）：

| `--beat-div` | 1 | 2 | 4 | 8 | 16 | 32 |
|---|---|---|---|---|---|---|
| 亮行数（加亮前） | 80 | 93 | **149** | **156** | 156 | 156 |
| 亮行数（加亮后） | 56 | 83 | 137 | **248** | 248 | 248 |

1→2→4→8 线数确实变密（≥16 因 32 拍可见而顶到 1/8，与 8 相同 —— 此时顶栏写明"实际 1/8"）。
计数在 8 之后饱和是**检测分辨率**（1/8 拍 ≈ 2.5px）而非绘制不变。

顺带一句：**`--shot` 默认 `--shot-frame 30` 且不会自己退出**，脚本里必须配 `--shot-exit`（+ `--shot-frame 4`
够用，0.85s 一张）；上一轮我漏了这个参数，进程不退，循环卡死到超时 —— 这是脚本问题，不是应用问题。

### Ctrl+滚轮缩放时间轴 + 标注精度自适应

新交互：**编辑区内 Ctrl+滚轮 = 缩放**（向上滚放大；等比；夹在 4～256 拍），不按 Ctrl 仍是"滚轮改当前时间"。

踩到的坑（无头测试实测，不是推理）：egui 0.36 的 `InputState::begin_pass` 发现 `wheel.modifiers`
命中 `options.zoom_modifier`（默认 ctrl/cmd）时，**不把滚动写进 `smooth_scroll_delta`**，而是
`zoom_factor_delta *= exp(scroll_zoom_speed · Δ)`。第一版按 `ctrl` 去读 `smooth_scroll_delta`，
于是按住 Ctrl 读到的一直是 0 —— 无头探针打印 `dy=0 ctrl=true` 当场现形。改为读 `i.zoom_delta()`
（`>1` = 放大 ⇒ 可见拍数 × 倒数），它同时覆盖触控板捏合。

**标注密度与网格密度是两件事**（新纯函数 `overlay::axis_ticks` / `axis_label_step` / `axis_label_decimals`）：
网格多密由设置决定，标注多密由"放不放得下字"决定（`AXIS_LABEL_MIN_PX = 34`），
步长只走 4·2^k 阶梯（… 1/4、1/2、1、2、4、8、16 …），**精度由步长定**：

| 可见拍数 | 每拍像素 | 标注步长 | 实测标签 |
|---|---|---|---|
| 4 | 150 | 1/4 拍 | `-1.75 -1.50 … 0.00 0.25 … 1.75` |
| 32（默认） | 18.75 | 2 拍 | `0 2 4 … 30` |
| 256 | 2.3 | 16 拍 | `0 16 32 … 240` |

（截图 `artifacts/zoom-labels.png`：三条轴带并排；`artifacts/grid-density.png`：每拍 1/2/4/8 条对比。）

### 验收方式（注意可注入性）

滚轮**注入不进 Wayland 窗口**，所以分三层验：
① 纯函数单测：换算、阶梯、精度、夹取；
② **无头 egui 合成滚轮事件**（`ctrl_wheel_zooms_while_plain_wheel_seeks`）：不按 Ctrl 只出 `ScrollBeats`、
按住 Ctrl 只出 `ZoomBeats`（就是这条抓出了上面那个读 0 的坑）；
③ 端到端：新增视图命令 `{"op":"zoom","beats":B|"factor":F}`（与 Ctrl+滚轮同一条状态路径），
附着到运行中的 GUI：`beats:8` → 读回 `overlay_beats=8`；再 `factor:2.0` → `16`；`beats:9999` → **256**（夹取生效）。

测试 **43 条**全绿，`cargo build`/`cargo test` 0 警告。


## 7.18 网格以"一拍"为基准 + 线画明显（Linux 侧，2026-09-27 续）

用户：**将横轴线画明显一些，以一拍为基准而不是 4 拍。**

问题在档位配色上：原先三档是"小节最亮（1.0px α120）> 整拍（0.7px α85）> 细分（0.4px α40）"，
于是**看得见的结构是 4 拍一格**，一拍线只是陪衬（α85 叠在压暗的预览上几乎只剩一层灰）。
改成一拍线当正线：

| 档位 | 初版 | 一轮 | 二轮（最细档加亮） | 角色 |
|---|---|---|---|---|
| 小节线（每 4 拍） | 1.0px α120 | 2.0px α200 | 2.2px α215 | 定位用，**不是基准** |
| **整拍线（每 1 拍）** | 0.7px α85 | 1.35px α155 | **1.5px α170** | **基准：结构读起来是一拍一格** |
| 细分线（1/div，**拍方向最细**） | 0.4px α40 | 0.6px α68 | **0.9px α110** | 细节，但看得见 |
| 坐标分格线（**竖方向最细**） | 0.4px α38 | — | **0.9px α110** | 与拍方向最细档**同色同宽** |
| 坐标中线（laneX=0） | 0.8px α95 | — | 1.3px α160 | 参考线 |

（二轮见 §7.19；两个方向的"最细档"用同一组数值，看起来才是一档东西。）

另外纵轴轴带里**每一拍补一个小刻度**（3.5px；标注位置的刻度加长到 4.5/7px 压在上面）：
标注密度是"放得下字就标"（可能每 2 拍、每 16 拍一个），但**节奏基元是一拍** —— 轴上也该看得见。
实测（轴带内缘 x∈[834,839] 的亮段计数 / 平均间距，32 拍可见、一拍 ≈ 23.1px）：

| | 亮段数 | 平均间距 |
|---|---|---|
| 改前 | 16 | 46.3px（每 2 拍，只有标注位） |
| 改后 | **32** | **23.1px（正好一拍）** |

线加亮的量化效果（同一区域亮行均值）：beat_div=1 时 23.4 → **42.7**；div=4 时 26.0 → **40.5**；
div=8 时 26.3 → **42.5**（≈ ×1.6）。亮行计数随之变多（div=8：156 → **248**）—— 因为亮线更容易被检测到，
这也正是"画明显"的直接证据。工件 `artifacts/beat-base.png`（改前 / 改后 / 每拍 1 条 三栏对比）、
`artifacts/grid-density.png`、`artifacts/zoom-labels.png`（后两者按新配色重出）。
测试 **43 条**全绿、0 警告。

## 7.19 最细的线也画明显 + 默认缩放拉长 4 倍（Linux 侧，2026-09-27 续）

用户：**将最细的横轴和纵轴线变明显，调整默认时间轴缩放，拉长 4 倍。**

### 最细档加亮（两个方向同一档）

| | 改前 | 改后 |
|---|---|---|
| 拍方向细分线（1/div） | 0.6px α68 | **0.9px α110** |
| 坐标方向分格线（竖） | 0.4px α38 | **0.9px α110**（与拍方向同色同宽） |
| 坐标中线 laneX=0 | 0.8px α95 | 1.3px α160 |

同时把基准/小节也各抬一档（1.5px α170 / 2.2px α215），保持 α 阶梯 **110 < 170 < 215** ——
"加亮最细档"如果不同时抬高上面两档，层次就没了，"一拍为基准"会退化成一片均匀的网。

实测（同一区域，32 拍可见）：
- 拍方向：细分线的行亮度峰值 40 → **63**（估算均值 20 → 27）；
- 坐标方向：最亮 16 列均值 32 → **63**、峰值 58 → **94**（≈ 翻倍）。

### 默认缩放拉长 4 倍：32 拍可见 → 8 拍可见

"把时间轴拉长"= 同样的音乐占更多屏幕 = 放大。默认可见拍数 **32 → 8**（每拍 ≈75px），
常量收敛到一处 `EditorState::DEFAULT_OVERLAY_BEATS`（此前 `state.rs` 与 `main.rs` **各写了一份 32.0**，
改一处就会不一致 —— 顺手合成一个来源）。8 拍可见下：1/4 拍细分 ≈19px、1/16 拍 ≈4.7px 都看得清，
轴带标注自动变成**每 1/2 拍一条、一位小数**（`-2.0 … 0.0 … 5.5`，实测截图）。

**这次改动立刻抓出一类测试脆弱性**：两条无头测试（共享边界命中、拖事件头）用默认 `overlay_beats`
去算指针坐标，默认一变就红 —— 但面板是按默认画、测试却按旧值算，属于"测试吃了视图默认值"。
修法：**测试自己钉住缩放**（`st.overlay_beats = 32.0`），并且测试算坐标与面板画图必须用同一个可见拍数。
（第一轮我只改了测试里的局部变量、没改 `st`，于是"飘"得更厉害：两处不一致，两条一起红。）

另一条教训：收尾时用 `pkill -f "opm.sock"` 清进程，**匹配到了自己这条 bash 命令行**，把执行中的命令一起杀了
（`[killed by signal: SIGTERM]`）—— 清进程一律按 **pid**（这是本工作区早就定下的规矩，这次是我自己破的）。

测试 **43 条**全绿、0 警告。

## 7.20 坐标等分改成"整窗平均切割"（撤销偶数限制）（Linux 侧，2026-09-27 续）

用户：**修改编辑区音符纵轴等分的算法：中轴持续存在，写几等分，就对整个窗口平均切割并放纵轴。
避免奇数无法放入的问题。**

§7.16 里我为了保住"吸附就是格点"这条不变量，把 `lane_div` **限制成偶数** —— 因为当时格点是
"以 0 为中心"的 `k·(1350/N)`（k 取正负各 N/2 条），奇数 N 时最外那条落到窗口外
（5 等分时 676 最近的是 **810**，超出格式允许的 laneX 范围）。用户否掉了这个用限制换便利的做法。

**新算法（边界对齐）**：格点 = `-675 + k·(1350/N)`，k = 0..N：

```rust
pub fn lane_of_index(&self, k: i32) -> f32 { -675.0 + k as f32 * self.h_step_rpe() }
pub fn snap_lane(&self, lane: f32) -> f32 {
    let k = ((lane + 675.0) / self.h_step_rpe()).round();
    (-675.0 + k * self.h_step_rpe()).clamp(-675.0, 675.0)
}
pub fn normalize_lane_div(d: u32) -> u32 { d.clamp(1, 128) }   // 不再强制偶数
```

两端恒为窗口边界 ⇒ **任何 N 都放得下**，"奇数放不进去"从算法层面消失。
代价（用户已明确接受）：**奇数 N 时 laneX = 0 不是格点**，吸附不落在中轴上；
但**中轴照画** —— `overlay` 在格线循环之后**单独**画一条 laneX = 0 的中轴线（1.3px α160），
它是"窗口正中"的视觉参考。偶数 N 时它本身就是第 N/2 条格线（两者重合）。
`GridCfg::center_is_lattice()` 把这个奇偶性显式表达出来，顶栏/状态栏在奇数等分时补一句
"（奇数等分：中轴不是格点）"，省得用户以为吸不到 0 是 bug。

**实测**（`--shot` 自截屏，检测"整列都亮"的竖线 = 格线，避开音符列；音符面板 x∈[308,833]）：

| `--lane-div` | 期望格线 | 实测格线 | 中轴 570 是否出现 |
|---|---|---|---|
| 3（奇数） | 308 / 483 / 658 / 833 | 308 / 483 / 658 / 833 | **是**（不是格点，单独画） |
| 5（奇数） | 308 / 413 / 518 / 623 / 728 / 833 | 同上，完全一致 | **是** |
| 7（奇数） | 308 / 383 / 458 / 533 / 608 / 683 / 758 / 833 | 同上，完全一致 | **是** |
| 16（偶数，默认） | 17 条，步长 32.8px | 17 条，步长 32.8px | 是（=第 8 条格线，重合） |

**测试也跟着改了两处口径**（这类"测试里写着旧模型"的地方最容易漏）：
① `normalize_lane_div` 的断言从"必为偶数"改成"只夹范围、不改奇偶"（1→1、3→3、200→128、0→1）；
② `grid_is_a_lattice_and_snapping_lands_on_it` 里"吸附后的值必须是步长整数倍"改成
"`(s+675)/step` 必须是整数"（旧式只在偶数下碰巧成立，是**旧模型的化石**）；
并补上 5 等分的格点贴边、中轴非格点、`snap_lane(0) → 135`（半格处取上）三条断言。
测试 **43 条**全绿、0 警告。工件 `artifacts/lane-division.png`（5 等分 vs 16 等分对照）。

## 7.21 顶栏多行 + 音符区窗口 X 偏移（编辑窗口外的音符）（Linux 侧，2026-09-27 续）

用户：**将顶栏改造为多行以便放置更多工具。添加音符 x 轴窗口偏移，调整数值可以偏移编辑区的音符区窗口，
可以编辑超过边界坐标的音符。**

### 顶栏改成两行

原来是**单个** `ui.horizontal`：标题/传输/保存/摘要/网格/设置全挤在一行。拆成：
第一行 = 身份/传输/保存/谱面摘要；第二行 = 视图与网格（`horizontal_wrapped`，窗口再窄也自己折行）。
实测两行内容：

```
OpenPhM | ▶ 播放 | ⏮ | 💾 保存 • | demo-400 ♪ 400
网格 拍 每拍 4 条 | 坐标 窗口 16 等分 | 可见 8 拍 | 窗口 X 偏移 675 RPE | ⟲ | 显示 0…1350 | 设置 ⚙
```

顺手把"可见 N 拍"从 `设置 ⚙` 菜单里提到第二行（它是常用旋钮），设置菜单只留低频项。

### 窗口 X 偏移

背景：`laneX` 越界**不是**错误 —— `spec/opm-format.md` 把它列在**警告**里（"4. `laneX` 超出 ±675"），
`validate` 实测 `[PASS] 0 error(s), 1 warning(s)`。所以编辑器本来就该能**看**到这些音符，
否则它们只能靠 CLI 盲改。新增 `EditorState::window_offset_x`（**视图状态**，文档里没有这个字段）：

```rust
lane_to_pane_x(pane, lane, off) = pane.min.x + (lane - off + 675)/1350 * pane.width()
pane_x_to_lane(pane, x,    off) = off - 675 + (x - pane.min.x)/pane.width() * 1350
```

三条设计要点：
1. **格点锚在官方坐标系上并向两侧延伸**：`lane_index_range(off)` 给出覆盖显示窗口的下标区间，
   吸附 `snap_lane_windowed(lane, off)` 在**下标**上夹取（不是夹数值），所以结果**必定仍是格点**；
   `off = 0` 时下标范围正好是 `0..=lane_div`，与改动前逐位一致（行为不变）。
2. **中轴（laneX=0）持续画**（§7.20 的规则），另外把**官方窗口边界 ±675 画成橙色竖线**：
   平移之后一眼能看出"橙线外侧就是窗口外"。
3. **窗口外的音符裁到音符区**绘制：映射里的 `t.clamp(-2,3)` 会让很远的音符落到面板之外，
   以前它们会糊到轴带/事件区上（平移后必然遇到），现在 `Rect::intersect(note_pane)` + `is_positive()` 跳过。

映射抽成纯函数 `overlay::{lane_to_pane_x, pane_x_to_lane}`（绘制/命中/测试共用一份），
逆映射在拖拽与双击放置两处替换（原先各写了一遍 `((x-min)/w)*1350-675`，平移后必错位）。

### 实测

- **映射与吸附**（单测）：正/逆映射在 `off ∈ {0, ±400, 675}` 上互逆；`off=0` 时 `laneX=±675` 恰是音符区两端、
  `0` 在正中（与旧公式一致）；`off=675` 时把指针放到音符区 **90% 处**得到 `laneX > 675`，吸附**不会被拉回窗口内**
  且仍是格点；`off=0` 时同样的指针位置落回 ±675 内；夹取 9999→675、NaN→0。
- **截图量相位**（`--shot` 自截屏，16 等分）：`off=0` 格线 308/340/…/833；`off=+400` 格线整体平移，
  实测首条 308→316（相位差 8px = 32.8px 步长下的 24.3px，mod 步长等价）。
- **橙色边界线**跟着移动：`off=0` 时在 x=308/833（音符区两端）；`off=+400` 时只剩 x=677（laneX=+675 在视野内，
  −675 已移出 ⇒ **不画**，这一步我一开始还以为是 bug，算了一遍才确认"移出视野就不该画"）；`off=−400` 时在 x=463。
- **端到端**（控制通道，与顶栏同一个 setter）：`{"op":"window","offsetX":400}` → 读回 `window_offset_x=400.0`；
  `9999` → **675.0**（夹取）。
- **真正的越界音符**：CLI 加一个 `laneX=1000` 的 tap（`validate`：0 error / 1 warning），然后同一屏幕位置
  （x≈697、第 4 拍那行）取样 —— `off=0` 得到 `[141,150,187]`（那里的 laneX 是 325，没有音符），
  `off=675` 得到 **[89,165,255] = tap 的蓝**，而该点此刻正对应 **laneX=1000** ⇒ 窗口外的音符确实"看得见、可拖"。

### 踩到的坑（第一版就中）

顶栏拖动框的临时值 `window_offset_x_ui` 我写死成 `0.0` 初始化，于是 `--window-offset 400` 在**第一帧**
就被同步逻辑（"拖动框与状态不一致 ⇒ 用户改了 ⇒ 写回状态"）当成"用户拉回 0"**抹掉**：
三张不同偏移的截图**逐像素相同**，格线与橙线全都一动不动 —— 这才发现。
修法：临时值初始化**从状态取**（`state.window_offset_x`）。教训：**双向同步的"临时值"必须用状态初始化**，
否则"默认值"会被当成用户输入。这条也解释了为什么我坚持每轮都截图量相位而不是"看代码觉得对"。

测试 **45 条**全绿、0 警告。工件 `artifacts/window-offset.png`（偏移 0 vs +675 对照）。

## 7.22 RPE 保存/加载（codec 层落地）（Linux 侧，2026-09-27 续）

用户：**实现保存和加载功能，参考 RPE 格式。**（这是提案里挂着的那条 codec 待办。）

### 分层

新增 `app/src/codec/{mod.rs, rpe.rs}`：编辑器只认 opm 模型，格式差异全部收敛在 codec 层
（提案 D1 的"单一真源 + 多 codec"）。枚举表**不是代码里再写一份 switch**，而是编译期
`include_str!("../../../spec/{easing,note-types}.json")` 读进来 —— 单一数据源这条规矩由测试钉住
（`note_type_tables_come_from_the_spec_file` 逐条比对 spec 文件）。

读路径收敛成一条：`EditCore::load` → `codec::detect`（**按内容**，不看扩展名：两种格式都是 `.json`）
→ `codec::to_document`。于是 `opm-ctl --file <RPE 谱面> validate/summary/dump/render/lines` 直接可用，
GUI「文件 📂」与控制通道 `{"op":"load"}` 走的是同一条路。

**格式守恒**：`EditCore` 记住载入格式，`Ctrl+S` 写回**同一种**格式。打开 RPE 然后保存不会把它悄悄变成 opm ——
这是数据事故，不是便利。另存为按扩展名（`*.opm.json` → opm，其余 `*.json` → RPE）。

### 导入侧：规范化是 codec 的职责（规范 §9）

排序、丢零长度/倒挂事件、**空隙按前值延拓补齐**、重叠裁剪、首事件补到拍 0、末事件延拓到谱面结束、
`hold` 的 `endTime ≤ startTime` 补 1/64 拍、`BPMList` 空或首项不在拍 0 时补默认条目。
判据很硬：**真实谱面导入后必须 `validate` 零 ERROR**（`imported_rpe_is_valid_opm` 钉住）。

### 保真度报告（规范明确要求"必须产出"）

`Fidelity { source, version, conversions, warnings }`，`convert` 打印、GUI 进控制台、`ui_stats` 也带。
同类问题**合并计数**：真实谱面每条线都有 `extended`/`*Control`，逐条报会得到 34 行几乎一样的文字
（Prismatic 实测），把该看的淹掉 ⇒ 按类别累计成一行（带首次出现的 JSON 指针）。
有降级时 `convert` 退出码 **1**（用法错误 2、校验 ERROR 3），脚本能发现。

### 真实谱面验证（这轮最重要的部分）

本机对境外主机（GitHub raw / jsdelivr / 各 gh-proxy / npm）**全部不可达**（curl TLS `unexpected eof`，
`web_fetch` 同样失败）；可达替代是 **PhiZone**（SSR 页面内嵌 `res.phizone.cn/.../Chart_*.json` 直链）。
拿到 3 份真实谱面（RPEVersion 113/141，7/17/71 条判定线，591/886/1114 音符）。结果：

| 检查 | 结果 |
|---|---|
| `validate` 导入结果 | 三份全部 `[PASS] 0 error(s), 0 warning(s)` |
| `RPE → opm → RPE → opm` | 三份**建模数据逐字段完全一致** |
| 无头出图 | 正常（Prismatic 首帧 17 实例） |
| 事件空隙 | 真实存在：Prismatic 每条线 moveX/moveY 各 37 处空隙，导入时前值延拓补齐 |

**被真实数据逼出来的两个修正**（合成 fixture 抓不到，因为它只用 1/2、1/4 这种二进制友好的时间）：

1. **音符时间必须导出成三元组**。此前默认写浮点（"RPE 音符写浮点"是文档里的印象，且我的合成
   fixture 全用 1/4、1/2，跑得通）。真实数据里分母出现 3/6/20/24，`37+1/3` 走浮点退化成
   `37333333/1000000`，往返立刻不等。改成默认三元组后三份全部一致，并补了
   `non_power_of_two_times_roundtrip_exactly` 这条测试（fixture 里加了 112/3 与 65/6 两个时间）。
2. **`-0.0` 要归一成 `0.0`**。Belle de Nuit（RPEVersion 141）里真有 `-0.0`，往返一趟变 `0.0`，
   于是 dump 对拍每次都报"1926 行差异"却查无实据。导入时 `norm0()` 统一。

另外**独立确认了那个最贵的坑**：真实数据里唯一带时长（`endTime ≠ startTime`）的类型是 **2**，
共 370 个 100% 有长度，1/3/4 全部零长度 ⇒ RPE `2 = Hold` 成立，与官谱（2 = Drag）相反。

验证记录（含出处 URL 与 md5）落在 `app/artifacts/rpe-verify.txt`；**谱面本体是第三方作品，不入库**。

测试 **58 条**全绿、0 警告；`app/README.md`、`app/AGENT-API.md`、`spec/opm-format.md` §9 已更新。

## 7.23 文件入口改成"系统规范"：调系统文件对话框（Linux 侧，2026-09-27 续）

用户：**将文件保存入口设计为遵循系统规范（调用系统文件管理器，使用文件展开框放置保存和另存为按钮）。**

上一轮我把打开/另存为做成了顶栏菜单里的**自绘路径输入框** —— 能用，但不合规范：桌面用户期待的是
自己的文件对话框（最近位置、书签、覆盖确认、键盘习惯都在那里）。这一轮改成：

### 系统对话框怎么调

新增 `app/src/filedialog.rs`。**先量环境**（Plasma 6 / Wayland，本机实测）：
`kdialog` ✓ `zenity` ✓ `dolphin` ✓ `xdg-open` ✓，`xdg-desktop-portal-kde` 在跑。
探测顺序 **kdialog → zenity**，都没有才退回内置路径输入（不让用户卡死）。argv 形状对着
`kdialog --help` 核过：`--getsavefilename [startDir] [name or mimetype filter]` ——
保存时把"目录 + 建议文件名"一起给出去，系统框才会预填名字并做覆盖确认。

为什么不是 `rfd` / 门户：
- `rfd` 要往工程里塞 GTK/Qt 绑定，只为一个对话框不值当；
- `org.freedesktop.portal.FileChooser` 是"最规范"的那条，但它是 Request/Response **异步**协议：
  `gdbus call` 只拿到 handle，等 `Response` 信号得自己写 D-Bus 客户端（本机没有 zbus 之类可用依赖）。
- 而 `kdialog`/`zenity` 弹出的**就是** KDE/GTK 自己的文件对话框 —— "遵循系统规范"的实质就在这里。
  门户留作后续（等有 D-Bus 客户端再说，届时它是替换实现而不是推翻设计）。

**"调用系统文件管理器"的另一半**：`🗂 在文件管理器中显示` → `dolphin --select <file>`（KDE，会选中该文件），
退化到 `xdg-open <dir>`。

### 对话框（"文件展开框"）里放什么

`egui::Modal`（背景遮罩 + 吞掉输入，模态语义交给框架，别自绘浮层）：

```
文件
当前：tests/data/messy.rpe.json  [rpe]
[📂 打开…] [💾 保存] [⤓ 另存为…] [🗂 在文件管理器中显示]
另存为格式  自动 / opm / RPE
系统文件对话框：kdialog（KDE 系统文件对话框）
路径 [____________]  [打开此路径] [保存到此路径]
```

保存/另存为/打开是**互斥的一次性决定**，且要看当前路径与格式 —— 挤在工具栏上既占地方又容易点错，
所以放进对话框。Esc/点遮罩关闭。

### 三个工程细节（都是踩过的）

1. **对话框动作在 UI 之外执行**：系统对话框是阻塞的（弹出期间本进程不出帧），不能在画帧中间弹窗 ——
   Modal 里只记录 `FileAction`，帧画完再执行。
2. **模态打开时屏蔽其它快捷键**：否则空格会把谱面播起来、H 会藏编辑区 —— 模态框在上面，
   底下不该有东西响应。
3. **`Ctrl+S` 在没有路径时转"另存为"**，而不是报一句"未指定保存路径"。

### 验证

- `filedialog` 单测 4 条：argv 拼装（KDE/GTK 两套语法、保存必须带覆盖确认）、起始位置与建议名、
  **管道**（用临时 shell 脚本当假对话框：输出路径 → 返回该路径；空输出 → 视为取消；程序不存在 → 明确报错
  而不是当成"用户取消"）、探测结果只可能是受支持的两个程序。
- 截图 `artifacts/file-dialog.png`（`--file-dialog` + `--shot`）。
- **未做**：真机上点一次系统对话框需要有人在屏幕前按（我这轮没有在你桌面上弹窗）。argv 对着
  `kdialog --help` 核过、管道用假程序测过，但"点下去真的出现 KDE 文件框并返回路径"这一步
  得你按一次 Ctrl+O 才算验证完。

测试 **63 条**全绿、0 警告。

## 7.24 顶栏去掉保存按钮 + 保存窗口拆成"文件夹 + 谱面名字"（Linux 侧，2026-09-27 续）

用户：**移除主界面的保存按钮，将保存窗口的路径指定分为文件夹 + 谱面名字（将编辑谱面名字的功能移到这里）。**

### 顶栏去掉保存按钮

顶栏原来是 `💾 保存 •` ＋ `📂 文件…` 两个入口。现在只留「📂 文件…」：保存是**一次性决定**
（写哪儿、什么格式），放在对话框里更合适；顶栏那格让给真正需要常驻的东西。
"是否脏"由状态栏的 `•` 与对话框里的"有未保存改动"负责，信息没丢。
`Ctrl+S` 保留（写回载入格式；没有路径时自动转"另存为"）。

### 路径拆两段 + 把"编辑谱面名字"搬进来

对话框里原来的单个"路径"输入框拆成：

```
文件夹   tests/data                    [选择文件夹…] [浏览…]
谱面名字 codec test                    [应用名字]
将写入   /var/www/OpenPhM/app/tests/data/codec test.opm.json
```

- **文件夹**：`选择文件夹…` 走系统目录对话框（新增 `filedialog::Which::Directory`：
  KDE `--getexistingdirectory`、GTK `--file-selection --directory`，两者都**不带**文件过滤器，
  有单测）。
- **谱面名字**：它**就是**文档里的 `meta.name`（顶栏显示的那个曲名）—— 用户要的"编辑谱面名字的功能"
  就落在这里，不必再开一个面板。名字同时当默认文件名，所以"改一次名字，文件与文档一起对齐"。
- **不逐字生效**：回车或「应用名字」/「保存·另存为」时才发一条 `set_meta`
  （逐字生效 = 每敲一个字一个撤销步，这条以前在别的地方踩过）。

### 名字是给人看的，文件名必须安全

`meta.name` 常带空格、冒号、书名号，甚至 `/`。两者必须分开处理：

| 输入 | 文件名主干 | 文档里的 `meta.name` |
|---|---|---|
| `我的 谱面/名 : 测试` | `我的 谱面-名 - 测试` | `我的 谱面/名 : 测试`（原样） |

规则（`filedialog::sanitize_stem`，**已从 GUI 挪进库**所以能单测）：分隔符与 Windows 保留字符
换 `-`、控制字符丢、首尾空白与点去掉、空名字兜底 `untitled`；**全角标点保留** ——
`名字：测试` 在任何文件系统上都合法，顺手换掉等于替用户改名（这条第一版我写错过，是测试纠正的）。

测试：`chart_names_become_safe_file_stems`（含 `../../etc/passwd` 这类穿越输入，断言"结果就是个普通
文件名、不是隐藏文件、不含分隔符"）、`derived_path_is_absolute_and_uses_the_extension`
（相对文件夹要补成**绝对**路径 —— 预览里给绝对路径，"存哪去了"不靠猜）。

### 实测

控制通道走了一遍与按钮**同一条**命令链（`{"op":"load"}` → `set_meta` → `{"op":"save","path":…,"format":"auto"}`）：
RPE 谱面导入 → 名字改成 `我的 谱面/名 : 测试` → 落到 `我的 谱面-名 - 测试.opm.json`（8240 字节，
`format: "opm"`，`fidelity.lossless = true`，文档里 `meta.name` 仍是原名）。
截图 `artifacts/file-dialog.png`（顶栏已无保存按钮，对话框两段式路径 + 绝对路径预览）。

测试 **65 条**全绿、0 警告。

## 7.25 合并保存目标 + 新建对话框 + 未保存守卫（Krita 语义）（Linux 侧，2026-09-27 续）

用户：**合并回文件夹+谱面名字为保存目标，修复在尝试保存新文件的时候无法指定路径（没有对应的文件）的问题
（参考 kryta/krita）。在文件窗口加入新建按钮，如果会话处于需要保存状态，则提示是否保存（保存|不保存|返回）。
进行保存操作时检查用户是否已经指定保存目标，没有则弹出保存窗口。如果从文件加载谱面，默认存回对应文件。
新建窗口需要填写曲名、谱面作者、音乐作者、音乐路径、基础 bpm。**

### 保存目标合并成一个

上一轮把路径拆成"文件夹 + 谱面名字"两个输入框，我当时的解读是"两段各自生效"—— 用起来是别扭的：
同一件事要填两处，还不知道哪个优先。现在**保存目标就是一个路径**（文件夹 + 谱面名字合成它），
界面上只有一行：

```
保存目标  tests/data/messy.rpe.json        （没有目标时：「（未指定 —— 保存时会弹保存窗口）」）
格式 [rpe]  曲名 codec test  [应用曲名]
```

**曲名（`meta.name`）与保存目标分开**：曲名是文档字段（顶栏显示的那个），当另存为的默认文件名用；
目标是要写哪儿。这与 Krita 一致（文档标题 vs 文件路径），也回答了上一轮"名字到底算谁的"。

### "保存新文件时无法指定路径"的真因

不是我们代码里的分支错，是**给系统框的起始位置不存在**：
`kdialog --getsavefilename <路径>` 的第一个参数是 **startDir**（对着 `kdialog --help` 核实），
路径不存在时 KDE 会把它当目录并报"目录不存在" —— 于是"新文件还没有对应文件"恰恰是无法指定路径的场景。

修法（`filedialog`）：
- `nearest_existing_dir(dir)`：向上退到最近的**已存在**目录，起始位置一律过它；
- `start_for_new_save(dir, stem, ext)`：存在的目录 + 曲名 + 目标扩展名；
- `ensure_extension(path, ext)`：系统框回来没扩展名就按格式补（Krita 同款）；
- `EditCore::save_as`：目标目录不存在时返回**能看懂**的错（`目标目录不存在：…（先建好目录，
  或用「选择文件夹…」）`），而不是内核那句 `No such file or directory`。

### 生命周期（Krita 那套）

| 动作 | 行为 | 实现 |
|---|---|---|
| 新建… | 弹"新建谱面"：曲名/谱面作者/音乐作者/音乐路径/基础 BPM | 核心新命令 `{"op":"new",…}` + `Document::fresh` |
| 新建前有未保存改动 | **保存｜不保存｜返回** | `GuardAction` + `GuardChoice`，守卫放行后继续原动作 |
| 保存时无目标 | **弹保存窗口** | `ensure_target_then_save()` |
| 从文件加载 | 目标 = 该文件，格式跟着它 | 上一轮的 `source_format` 已保证 |
| 新建之后 | 目标清空、撤销栈清空、**文档是脏的** | 单测钉住 |

`Document::fresh(meta, bpm)` 放进库里（而不是 GUI 里拼）：CLI（`opm-ctl new`）、GUI、测试要的是**同一份**
初始形态（`bpmList` 从拍 0 起、至少一条判定线），这种"初始不变量"只能有一处实现。

**守卫为什么是三选一**：少了「返回」，用户点开"新建"只是想看看，就被迫在"保存"和"丢弃"之间选一个。
点遮罩/Esc = 返回（取消），不做任何破坏性动作。

### 实测

- 单测 6 条新增：`filedialog` 的"起始目录必须存在"、"缺扩展名要补"；`tests/lifecycle.rs` 的
  "新建必须合法/脏/无目标/撤销栈干净/广播全量话题"、"无目标保存被明确拒绝 + 目录不存在给能看懂的错"。
- 端到端（控制通道，与 GUI 按钮同一条命令链）：`{"op":"new",…}` → `path: null`；
  `{"op":"save"}` → **明确拒绝** `未指定保存路径`（GUI 据此弹保存窗口）；
  `{"op":"save","path":"/tmp/nf/新曲.opm.json"}` → 写出、`format: opm`、`validate` 零 ERROR。
- 截图 `artifacts/file-dialogs.png`（守卫 / 新建 / 文件三个对话框；**已随 §7.34 进回收站** ——
  那张拼图里的"新建"是当时编辑页上的对话框，现在是启动页上的模态，见 `artifacts/new-chart-modal.png`）。

测试 **69 条**全绿、0 警告。

## 7.26 空格键：单点切播 + 长按试听（Linux 侧，2026-09-27 续）

用户：**优化：单点空格进入/退出自动播放，长按空格则在松开时退出自动播放。**

原来是"按下就 toggle"一行逻辑。要支持长按，就多了四种组合（停/播 × 单点/长按）与若干边界
（重复按下事件、打字期间、模态框期间、失焦），散在 UI 代码里必然长出互相打架的特例 ——
所以抽成库内的纯状态机 `keymap::SpacePlayback`：

| 场景 | 按下 | 松开 |
|---|---|---|
| 停着 + **单点** | 开始播放 | 继续播（**进入**自动播放） |
| 播放中 + **单点** | 立刻暂停（**退出**） | 不做事 |
| 停着 + **长按** | 开始播放 | **暂停**（松手即退出） |
| 播放中 + **长按** | 立刻暂停 | 不做事 |

两个刻意的选择：
1. **播放中按下就停**（不等松手）：单点退出要即时反馈；
2. **长按松手只停"本次按下才开始播"的播放**：否则松手会把用户自己之前的播放状态也停掉
   （单测 `release_never_stops_playback_it_did_not_start` 钉住）。

阈值 `SPACE_HOLD_SECS = 0.28`：比最快的连点（~0.15 s）长，又不至于让人等。**故意不做成旋钮** ——
多一个旋钮就多一种"说不清"的手感。

### 容易漏的三条（都有单测）

- **重复按下事件不能刷新按下时刻**：一直按住时 egui 会重复给 `key_pressed`，
  若刷新时刻，松手那一刻会被误判成"单点" ⇒ 长按试听永远退不出来；
- **打字/模态框期间按下要复位**：否则松手时拿一个陈旧的按下时刻算长按，冒出莫名其妙的暂停；
- **失焦复位**：回到窗口时不该以为空格一直按着。

### 顺手收拾的两处"规则副本"

- `main.rs::should_toggle_play`（旧的"按下就 toggle"）**删除**：同一件事有两份实现，早晚有人用错那一份；
- `tests/audio.rs` 里那条测试以前只能手抄一份规则（`|kb, sp| sp && !kb`）来断言，因为规则在 `main.rs`
  里拿不到 —— 实现一改，抄件不会跟着改，测试反而变成"保证旧行为"的锁。现在规则在库里，测试直接调真身。

### 验证边界（说实话）

键序语义有 7 条单测（四种组合 + 阈值边界 + 重复按下 + 无按下即松手 + 复位 + 打字/模态框让路），
主循环只留 3 行喂参数。**但"真按一下空格"这一步本机注入不了**：Wayland 窗口收不到合成的键事件
（xdotool 需要 DISPLAY，wtype/ydotool 没装）。所以"手感"要你按一次才算验完 —— 规则本身是测过的。

测试 **77 条**全绿、0 警告。

## 7.27 谱面列表（起始界面）（Linux 侧，2026-09-27 续）

用户：**设计一个谱面列表界面，左侧展示最近打开的谱面，右侧放置打开、新建按钮。打开程序时先显示此窗口。**

- **起始界面**：`--doc` 之外启动时先显示（`--bench/--stress` 这类无头用途不显示）。
  左＝最近打开（曲名 + `[格式] 多久以前`），右＝打开…/新建…/跳过。
  用 `egui::Modal` 铺满整屏而不是 `CentralPanel`：①它的背景遮罩**吞掉输入**，编辑器不会在下面偷响应；
  ②不必给编辑器那 1500 行 UI 加缩进（少一次大范围改动）。
- **记录**（`app/src/recents.rs`，库内可单测）：存 `$XDG_CONFIG_HOME/OpenPhM/recents.json`，
  **去重 + 最近在前 + 最多 20 条**，相对路径绝对化，载入时清理已不存在的条目，坏文件当空（便利功能不拦启动）。
- 打开/保存成功后自动记一条；`✕` 只从列表移除、不删文件。

第一版布局踩了个坑：两栏宽度靠 `ui.available_width()` 一层层吃下去，右栏被挤成一条缝
（截图里"开始"只剩一个字宽）⇒ 改成**先算清楚 `left_w/right_w` 再 `allocate_ui_with_layout`**。
截图 `artifacts/start-screen.png`。

## 7.28 opm 容器：`.opm` 是 ZIP，打包交给 7z（Linux 侧，2026-09-27 续）

用户：**注意：opm 实际上应该是 zip 文件，内部存放音乐，曲绘，谱面等** / **zip 尝试引用用 7z 实现**。

### 形态定了

`spec/opm-format.md` §6 原来写着"`*.opmz` 单文件分发（后期），命名待定"——现在定案：

| 形态 | 用途 |
|---|---|
| `*.opm`（**ZIP**） | 分发：`opm.json` + 音乐 + 曲绘 + 其它资源（未建模条目**原样保留**） |
| `*.opm.json` | 工程文件：纯 JSON，可 diff、可入版本库、agent/测试用 |

与 Krita 的 `.kra`(zip) / `.krita`(纯 XML) 同一个取舍；判格式**按内容**（ZIP 魔数 vs JSON），
所以 `opm-ctl --file X` 三种形态都吃。

### 打包：优先 7z，内置实现兜底

本机实测 7z 26.03 可用，且：
- `7z a -tzip -mtm=off -mta=off -mtc=off` ⇒ **两次打包字节相同**（时间戳不写进去）——
  这条很关键：否则每次保存都是一个新二进制，diff/校验/缓存全废；
- **两遍策略**成立：`-mm=Deflate` 加谱面/文本，`-mm=Copy` 追加音乐/图片（对已压缩数据再 deflate 是白烧 CPU）；
  `7z l -slt` 实测：`opm.json` = Deflate、`bg.png`/`song.ogg` = Store ✓。

于是 `zip::Backend { SevenZip, Builtin }`：打包**首选 7z**，没装 7z 的机器退化到内置实现（全 STORE）；
解包**先内置**（纯内存、快，覆盖 STORE/DEFLATE），遇到 ZIP64/特殊方法再交给 7z 并把"换了后端"写进报告。

### 自研 zip 是为了"不引依赖"，不是为了炫技

本机对 crates.io **不可达**（`cargo add flate2` 拿不到包），而容器是硬需求 ⇒ `app/src/zip.rs` 自己实现：
CRC32、STORE 写、STORE/DEFLATE 读（含 fixed/dynamic Huffman、stored block）。覆盖范围写死在注释里：
单文件 ≤ 4 GiB、方法 0/8、不支持 ZIP64/加密/多卷，遇到**明确报错**。
**互验**（这轮最值钱的测试）：7z 打的包内置读得回、内置打的包 7z 解得开、Python `zipfile` 造的
STORE 与 DEFLATE 包都能读、CRC 对不上要报错、截断要报错 —— 单靠自己往返，一致地写错也一样通过。

### 资源不进 JSON，进容器

保存 `.opm` 时：把 `meta.audio`/`meta.background` 指向的文件**读进包**（条目名取**文件名**，不是宿主绝对路径），
并把文档字段改写成包内相对名 —— 这一步走 `journal.record(SetMeta)`，**可撤销**，不是保存时的暗改。
读不到的只报警告（不静默产出缺音乐的包）。载入容器时资源摊到 `$XDG_CACHE_HOME/OpenPhM/assets/<hash>/`
（播放器/图片按路径工作），条目名只取文件名，`../` 不会越出缓存目录。

### 实测

端到端（控制通道）：`{"op":"new"}` → 设音频/曲绘 → `{"op":"save","path":"容器曲.opm","format":"opm"}` ⇒
容器 90 KB（音乐）+ 361 B（曲绘）+ 1 KB（谱面，Deflate）；`7z l -slt` 看到三个条目与压缩方法；
`opm-ctl --file 容器曲.opm summary` 读回正常，容器内 `meta.audio='song.ogg'`、`background='bg.png'`（相对名）✓。

测试 **94 条**全绿、0 警告。

## 7.29 启动检查 7z（兼容 Windows）+ 缺失提示窗（Linux 侧，2026-09-27 续）

用户：**在启动时检查是否能调用 7z（兼容 windows），如果没有则弹窗提示（文本和退出按钮，
在 windows 上额外有一个获取 7z 按钮，按下调用浏览器到 7z 网站）。**

### "能调用"而不是"文件存在"

探测写成一个**平台参数化**的纯函数 `seven_zip_candidates(is_windows, env)` —— 于是 Windows 那份候选表
**在 Linux 上也能单测**（把"哪个平台 + 环境变量是什么"当输入），这是本轮唯一能真正验证 Windows 逻辑的办法。
候选顺序：`OPM_7Z`（显式指定，指了就只认它，调不动就算不可用 —— 不偷偷换成别的）→ `PATH`
（`7z`/`7za`/`7zr`，Windows 上是 `7z.exe` …）→ **Windows 常见安装目录**
（`ProgramFiles` / `ProgramFiles(x86)` / `ProgramW6432` / `LOCALAPPDATA\\Programs\\7-Zip`）。
PATH 的分隔符按**目标平台**取：第一版用了 `std::env::split_paths`，它按**宿主**平台切，
在 Linux 上模拟 Windows 时把 `C:\\Windows;C:\\Tools` 切错 —— 单元测试当场抓到，改成显式传分隔符。

判定"可调用"= 起一次 `7z i`、退出码 0：装了一半 / 权限不对 / 架构不符都会露出来
（只 `is_file()` 会把"不可执行的同名文件"和"目录"都算成可用，测试里两条都钉住了）。
Windows 上给子进程加 `CREATE_NO_WINDOW`（`CommandExt::creation_flags`），否则每次探测闪一个控制台窗口。

### 提示窗

文本讲清楚"为什么需要 7z"（`.opm` 是 ZIP 容器）+ 具体安装方法（按平台给命令/下载页）+ **退出**按钮；
**Windows 上多一个「获取 7z…」**，按下走 `filedialog::open_url` → `cmd /C start "" <url>`
（macOS `open`、Linux `xdg-open`）；`open_url` 只收 http(s)，`file://`/`javascript:` 一律拒。
窗口**不给 Esc/点遮罩关闭**：它是启动门槛，只有"装好再来"一个明确选择。

**画序坑**：提示窗一开始插在起始界面之前，结果被后面的面板与起始界面的遮罩盖住、暗到看不清 ——
截图一眼看出来。修法：把它移到 UI 的**最后**画（画序决定谁在上面），并在它未消失时不画起始界面。

### 验证边界（说实话）

- 单测 5 条新增：Windows 候选表（含常见安装目录）、"不可执行的同名文件/目录不算可用"、
  `OPM_7Z` 显式优先且调不动不复用候选、`open_url` 只收 http(s)。
- 提示窗**截图验证**（`OPM_7Z=/nonexistent/7z` 触发）：`artifacts/missing-7z.png`；
  有 7z 时启动打印 `7z: /usr/bin/7z` 并正常显示起始界面。
- **Windows 分支本机无法编译验证**：`rustup target add x86_64-pc-windows-gnu` 失败
  （static.rust-lang.org TLS 被重置）。`#[cfg(windows)]` 那几处（`cmd /C start`、`CREATE_NO_WINDOW`、
  `.exe` 候选）在 Linux 上不参与编译 —— 逻辑已被平台参数化的单测覆盖，但**编译**要等真在 Windows 上过一遍。

测试 **98 条**全绿、0 警告。

## 7.30 起始界面"越滑越宽"：布局反馈回路（Linux 侧，2026-09-27 续）

用户：**鼠标在开始界面滑动时，谱面列表的横向长度会持续变长。**

这是 egui 里典型的**反馈回路**：内容宽度由 `ui.available_width()` 算出，而内容宽度又决定
`Area`（模态框）的尺寸 ⇒ 每帧在上一帧的结果上再放大一点点。egui 空闲时只按 `idle_fps` 重绘，
**鼠标一动就持续重绘**，所以症状是"滑动时一直变长"。

### 修法：把"尺寸从哪来"钉死

- 新增纯函数 `recents::start_screen_columns(screen) -> (left_w, right_w, body_h)`：**只吃屏幕矩形**，
  同样的窗口尺寸必然同样的结果（`columns_are_stable_across_calls` 连算 50 次断言相等）；
- 栏内改用 `ui.set_width(w)`（= min+max 都钉住），不再让内容反过来撑父容器；
- 起始界面整体**搬进库里**（`recents::start_screen_ui`），main.rs 只剩"喂数据、收动作、施加动作"。

搬进库不是为了好看：只有真实那段 UI 能在无头 egui 里连跑若干帧、把**每帧画出来的宽度**量出来，
才谈得上"钉住不再犯"。

### 验证与诚实结论

- 新增回归测试 `start_screen_drawn_width_is_stable_across_frames`：真实 UI 连跑 12 帧，
  用 `FullOutput.shapes` 里最大的填充矩形（模态框底板）当量尺 —— 形状故意用**很长的中文/英文混排曲名**，
  逼出任何"被内容撑大"的路径。实测：第 0 帧无尺寸（egui 首帧只是建立布局），第 1..11 帧**恒为 1600×…**，
  且不超出屏幕 ✓。
- `start_screen_returns_actions_for_clicks`：无头合成 Esc，断言给出"跳过"动作（按钮点击与它同一条返回路径）。
- **诚实说明**：我**没能**在无头环境里把"旧版越画越宽"复现出来（照旧结构写的几个复现件都稳定），
  所以这条修复的定位是"**去掉那一类反馈**（尺寸不再来自 `available_width`）+ 证明**现在的实现帧间稳定**"，
  而不是"我复现了原症状再修好它"。真机确认还是要你滑一次鼠标。

测试 **101 条**全绿、0 警告。

## 7.31 启动页独立成页 + 清掉内建谱面（Linux 侧，2026-09-27 续）

用户：**将启动页放到单独的窗口里，选中文件后才切换到编辑页。清理编辑页的内建谱面。** /
**7z安装提示页也这么做。**

### 先量 eframe 的视口约束，再下方案

eframe 0.36 的 `App::ui` **只对根视口调用**；子视口走 `show_viewport_deferred(id, builder, |ui, class|)`
（回调是 `'static + Send + Sync`，状态得进 `Arc<Mutex<_>>`）。更要紧的是三条实测约束：
- `if is_root_viewport && close_requested { handle_close_request() }` ⇒ **关掉根 = 退出程序**；
- 隐藏根 ⇒ eframe **完全不跑 egui pass**（只跑 `logic()`）⇒ 子视口也不会被绘制；
- 于是"启动窗口关掉、编辑窗口留下"在 eframe 里做不到。

结论：**同一个窗口的两个页面**（启动页有自己的标题与尺寸，选完换成编辑页）。
真要做两个并存窗口，得换自管 winit 多窗口，或让启动窗口常驻 —— 这条写进 README，别让后来人再试一遍。

### 实现

- 新增 `LaunchPhase { Missing7z, StartScreen, Editor }`：窗口初始标题/尺寸按阶段来，
  `enter_editor()` 换标题（带曲名与文件名）与尺寸；`--doc`/`--bench`/`--stress` 直接是 `Editor`。
  （**这一版后来被 §7.34 收掉**：`Missing7z` 并进启动页，成为盖在列表上的门槛模态。）
- 7z 提示与起始界面各抽成**库函数**（`recents::missing_7z_ui` / `start_screen_ui`），
  App 只做"喂数据、收动作、施加动作"。起始界面从"模态"降级为普通页面（不再套 `Modal`）。
  （`missing_7z_ui` 在 §7.34 被 `missing_7z_modal` 取代：同一件事在启动页与编辑页只有一份实现。）
- **内建谱面清掉**：`Args::default().notes` 从 20_000 改成 **0** —— 没选文件就进来是空的未命名谱面；
  `--notes N` 仍可显式要（bench/造图）。
- 自动化钩子 `OPM_LAUNCH_AUTO=skip|new|open:<path>|recent:<n>`：没人点鼠标也能走完"选完切编辑页"。

### 这一轮踩到并修掉的四个坑（都不是"理论问题"，全是截图/实测逼出来的）

1. **启动页中文是豆腐块**：字体装载在 `if !self.inited` 块里，而启动分支写在它**之前**就早退了 ⇒
   把启动分支挪到初始化之后。
2. **启动分支早退漏了"请求下一帧"**：egui 没人 `request_repaint` 就出几帧就停（探针显示 frames 卡在 3）⇒
   抽出 `App::pace()`，早退分支也要调。
3. **矮窗口 panic**：`(h*frac).clamp(64.0, h*0.7)` 在 `h*0.7 < 64` 时 `min > max` 直接 panic
   （转场把窗口从 620 高换成 900 高的那一帧就踩到；用户把窗口拖矮同样会踩）。
   抽成纯函数 `state::timeline_height` + 边界单测（0/1/20/60/91/200/900 高 × 6 种比例）。
4. **启动页像素糊在编辑页底下**：eframe 默认 `clear_color` 的 alpha 是 **180**（不清干净）；
   加上"进编辑页先铺一层不透明底色"。真正的元凶其实是**旧的那段起始界面代码还在编辑页里**
   （本回合我删过一次、只删掉一部分），每帧把启动页又画了一遍 —— 截图一眼看出来，grep 一下确认。

### 实测

`--shot` 三张：`artifacts/start-window.png`（980×620，标题"OpenPhM — 选择谱面"）、
`artifacts/missing-7z-window.png`（640×360，`OPM_7Z=/nonexistent/7z` 触发；**已随 §7.34 进回收站** ——
缺 7z 现在是启动页上的黏性模态，见 `artifacts/missing-7z-modal.png`）、
`artifacts/editor-after-launch.png`（`OPM_LAUNCH_AUTO=skip` 走完转场后的编辑页：✅ 空谱面 ♪ 0、无启动页残留）。

测试 **102 条**全绿、0 警告。

### 自我批评（记下来）

删代码时我又一次"按区间一把删"，把**未保存守卫**与**新建对话框**连带删掉了（两次，同一类错误）。
好在先前把整块提取到 `/tmp` 过，两次都从那份提取里恢复了 160 行并重新编译验证。
**教训**：删区间前先 `grep` 出区间内有多少个"独立块"，或者按块删、删完立刻 `cargo build` 看有没有
`never constructed`/`never used` 的警告 —— 那正是"某块 UI 被误删"的信号。

## 7.32 "鼠标一在启动页移动就崩"：抽方法时漏掉守卫（Linux 侧，2026-09-27 续）

用户：**为什么鼠标一在启动页移动就会停止** + panic 栈
`thread 'main' panicked at src/main.rs:2066:59: called Option::unwrap() on a None value`。

### 根因

上一轮我把内联在 `App::ui` 里的自截屏逻辑抽成 `App::handle_shot()`，**抽取时把最外层的
`if self.args.shot.is_some()` 守卫一起丢掉了**。于是没给 `--shot` 的普通运行也会走到：

```rust
None if self.frames as u32 == self.args.shot_frame =>   // 默认 30
    ctx.send_viewport_cmd(ViewportCommand::Screenshot(..))   // 没人要截图，它却发了请求
```

下一帧 egui 把图回灌成事件 ⇒ `Some(img)` 分支 ⇒ `self.args.shot.clone().unwrap()` ⇒ **None ⇒ panic**。
而"为什么是鼠标一动就崩"：编辑器空闲只按 `idle_fps`（1 fps）出帧，鼠标移动会触发连续重绘 ⇒ 30 帧几秒就到
⇒ 必然踩到。**症状（移动就崩）与根因（帧数）之间隔着"谁在触发重绘"这一层，不量帧数很难想到。**

### 修法：决策抽成纯函数 + 回归测试

新增 `app/src/shot.rs`：`shot_step(configured, frames, shot_frame, got_image) -> Idle|Request|Save(path)`，
三条不变量各有单测，其中第一条就是这次的病：

1. **没配置 `--shot` 时永远 `Idle`** —— 哪怕帧号正好等于默认目标帧（`without_shot_flag_nothing_happens_even_on_the_target_frame`）；
2. 配置了但没到目标帧 → `Idle`；正好到 → `Request`（只请求一次）；
3. 收到图且配置了 → `Save`（**不再有 `unwrap()`**：`configured` 是 `Option<&str>`，`match` 决定一切）。

`handle_shot` 现在只做"读输入、执行动作"。

### 顺手扫掉同类隐患

同一轮把 3 处 `v.sort_by(|a,b| a.partial_cmp(b).unwrap())`（帧时间/latency 统计的排序）换成
`unwrap_or(Ordering::Equal)`：**NaN 会让 `partial_cmp` 返回 `None`，`unwrap()` 就是"某个数坏掉 = 整个编辑器崩"**；
统计值坏掉最多统计不准，不该拖垮程序。

### 实测

- 复现条件跑通：不给 `--shot`、把帧率拉高（`--idle-fps 300`）跑到 30 帧以上 ⇒ **0 次 panic**（修前必崩）。
- 三条 `--shot` 路径复测：启动页 980×620、转场后编辑页 1600×900、7z 提示页 640×360 ✓。
- 测试 **104 条**全绿、0 警告（连跑 5 轮无 flake）。

**教训**：把一段代码从"内联"抽成"方法"时，**外层的守卫条件是最容易被丢的东西**（它不在缩进块里，
而在块的上一行）。抽完立刻 `cargo build` 之外，还要问一句："这段原来在什么条件下才会执行？"

## 7.33 "新建谱面"搬进启动页（Linux 侧，2026-09-27 续）

用户：**填写谱面信息的窗口也放在启动页。**

上一轮把启动页与 7z 提示做成了独立页面，但"新建谱面"还是编辑页上的一个 `egui::Modal`
（`new_dialog_open` + 五个字段散在 `App` 里）。这一轮把它搬成**启动页的一个屏**：

```
选择谱面（列表）  ⇄  新建谱面（填表）  →  编辑页
```

- 库里新增 `StartScreenPage { List, NewChart }` 与 **`NewChartForm`**（曲名/谱面作者/音乐作者/音乐路径/基础 BPM）。
  表单**值**由调用方持有、库里只改它（`&mut NewChartForm`），`start_screen_ui` 多两个参数按屏分派。
- 校验与提交参数都在库里（`validate()` / `to_new_command()`）：**曲名必填**（没名字在最近列表里只能显示文件名）、
  **BPM 必须为正且有限**；不通过就留在表单那一屏并显示原因（截图里那句红字"曲名不能为空"就是它）。
- 新增动作：`ShowNewForm` / `BackToList` / `Create` / `PickAudio`。**填表那一屏的 Esc = 返回列表**，
  不是"跳过直接进编辑器"——用户在填表，别把他踢进编辑器（有单测钉住）。
- 窗口标题/尺寸跟着屏走：列表 980×620、新建 760×480。切屏需要 `Context`，而 `start_guarded`
  这类调用点拿不到 ⇒ 记 `pending_launch_page` 待办，在帧里由 `pump_launch_switch` 处理。
  **启动分支与编辑页都要调它** —— 启动分支早退，漏调就永远切不过去（这一版就踩了，截图仍是列表页）。
- 编辑页里的「新建…」不再弹本地模态，而是**切回启动页那一屏**（有未保存改动时先过"保存｜不保存｜返回"）。
  于是"新建谱面"这件事在程序里只有一个实现、一个位置。

### 实测

- 单测：`new_chart_form_validates_and_builds_the_command`（空/全空格曲名、BPM 0/负/NaN/∞ 都要被拒；
  提交参数去掉首尾空白、空曲名兜底 `untitled`）、`new_chart_page_esc_goes_back_to_the_list`
  （Esc 给 `BackToList` 且**不给** `Skip`）。
- 端到端（自动化钩子）：`OPM_LAUNCH_AUTO=new` → 截到表单那一屏（760×480）；
  `OPM_LAUNCH_AUTO=create:从表单建的谱面` → 建谱面 → 进编辑页，顶栏显示曲名、`♪ 0`（空谱面无内建内容）、
  状态栏 `[opm]（未命名）•`（脏且还没保存目标 ✓）。
- 工件：`artifacts/{start-window,new-chart-page,editor-after-form}.png`
  —— **`new-chart-page.png` 与 `editor-after-form.png` 已随 §7.34 进回收站**（它们拍的是"整屏填表"，
  那个界面不存在了），替代品是 `new-chart-modal.png` 与 `editor-after-modal.png`。

测试 **106 条**全绿、0 警告。

## 7.34 弹窗只有一套外观（"新建谱面"退回模态；Linux 侧，2026-09-27 续）

用户：**新建谱面窗口采用和编辑窗口的弹窗一样的样式（不直接切换整个界面），统一不同窗口的样式。**

§7.33 的写法把"填表"做成了启动页的**第二屏**：点「新建谱面…」→ 整个界面被换掉、窗口从 980×620
改成 760×480。用户要的不是这个 —— 他要的是**一个弹窗**：底下的列表还在，只是被遮住。

于是这一轮做两件事：把"新建谱面"退回模态，**并且**把"弹窗长什么样"从四处各写一份收成一个模块。

### 新增 `dialog` 模块：弹窗外观的唯一出处

原先四处弹窗各写各的：文件对话框（`main.rs` 内联）、未保存守卫（`main.rs` 内联）、
缺少 7z（**两份**：`recents::missing_7z_ui` 一屏 + `main.rs` 里一个 `egui::Modal`，后者其实**够不着**）、
新建谱面（先是 `main.rs` 的模态，后是 `recents` 的一屏）。三套颜色、三档随手写的宽度、三种关法
（有的自己判 `key_pressed(Escape)`、有的压根不判）。

`app/src/dialog.rs` 现在提供：

- **颜色**：`HINT`（次要说明，唯一的灰）/ `PATH`（等宽路径）/ `WARN` / `OK` / `ERR`；
- **宽度三档**：`W_NARROW 480` / `W_FORM 560` / `W_WIDE 660` —— 调用点不再随手写数字；
- `modal(...)`：可关闭（Esc 或点遮罩 ⇒ `dismissed`）；`sticky_modal(...)`：关不掉（启动门槛）；
- 文本助手 `title/hint/warn/path/message`。

**Esc 的归属在这里收口**：两个函数都调 `ModalResponse::should_close()`，它内部用
`consume_key` 消费 Esc，且**只在最上层模态**上生效。所以"弹窗开着时底下的界面看不到 Esc"
是**框架保证**的，不用在每个调用点各判一次 —— 这也顺手修掉了编辑页原先"Esc 判两次"的写法。

### 启动页只剩一屏

```
启动页（谱面列表，980×620）
   ├─ 模态：新建谱面（可关：Esc / 点遮罩 = 返回列表）
   └─ 模态：缺少 7z（关不掉，出口只有"退出"[+ Windows 的"获取 7z…"]）
```

- `LaunchPhase { Missing7z, StartScreen, Editor }` → `{ StartScreen, Editor }`：
  缺 7z 不再是"另一个窗口"，而是启动页上的一层**黏性模态**（底下的列表照画、被压暗、吞掉输入）。
  窗口尺寸也不再随屏变化 —— `StartScreenPage` 与 `pump_launch_switch` 一并删除，
  换成 `new_form_open: bool` + `pending_launch_new: bool`（编辑页「新建…」仍要切回启动页，
  换标题/尺寸需要 `Context`，只能在帧里做）。
- **门槛不许被绕过**：黏性模态虽然关不掉，仍然消费 Esc（否则底下列表的 "Esc=跳过" 会把门槛漏掉）；
  缺 7z 期间 `OPM_LAUNCH_AUTO` 一律不生效（否则 `skip` 就是后门）。
- 编辑页那份 7z `egui::Modal` 从"够不着"变成**可达**（`--doc` + 系统无 7z 这条路），
  现在直接调 `recents::missing_7z_modal` —— 同一件事两处画成两种长相，是这个文件里最容易长出来的不一致。
- `--dialog new` 现在会顺手把阶段拨回启动页：那个模态只属于启动页，否则带 `--doc` 时会"给了参数却什么都没弹"。
- **顺手补的一个洞**：`create_new_doc` 现在返回"是否真的建成"，调用方据此决定进编辑页还是留在模态里。
  原先的判据是 `path.is_none() && validate().is_ok()` —— **命令失败也会进编辑页**，把错误提示留在了一个
  已经没有上下文的界面后面。校验不过时也不再往按钮旁贴第二份同样的字（表单下方那行实时校验就是原因）。

### 实测

- 单测（新增 5 条）：`dialog::escape_dismisses_a_closable_modal_and_is_consumed`（Esc 关得掉、
  **而且之后的代码再也看不见这个 Esc**）、`dialog::sticky_modal_never_dismisses_but_still_eats_escape`、
  `dialog::inner_value_is_passed_through`、
  `recents::new_chart_modal_esc_goes_back_to_the_list_and_never_skips`（列表 + 模态同帧绘制，
  一份 Esc **只产生一个动作**）、`recents::start_screen_returns_actions_for_clicks`（模态开着时列表一个动作都不给）。
- 截图（`--shot`）：`artifacts/new-chart-modal.png`（980×620，模态盖在列表上，**尺寸与标题都没变**）、
  `artifacts/missing-7z-modal.png`（`OPM_7Z=/nonexistent/7z` 触发的门槛）、
  `artifacts/guard-dialog.png` 与 `artifacts/file-dialog.png`（编辑页两个弹窗，改用 `dialog` 后外观不变）、
  `artifacts/editor-after-modal.png`（`OPM_LAUNCH_AUTO=create:` 走完"填表→建谱→编辑页"）。
- 另外两条路也复查过：`OPM_LAUNCH_AUTO=skip` → 编辑页；`--doc … OPM_7Z=/nonexistent/7z` → 编辑页 + 门槛模态。
- 失败路径也拍了：`OPM_LAUNCH_AUTO=create:`（曲名为空）⇒ **留在模态里**、窗口仍是 980×620、
  表单下方给出"曲名不能为空"（`artifacts/new-chart-modal-invalid.png`）。
- 进回收站的旧截图（拍的是已不存在的界面）：`file-dialogs.png`、`missing-7z.png`、`start-screen.png`、
  `new-chart-page.png`、`missing-7z-window.png`、`editor-after-form.png`。

测试 **109 条**全绿、0 警告。
## 7.35 结构性重构：边界交给编译器、文本说人话、表现层也事件驱动（Linux 侧，2026-09-27 续）

用户：**使用 goal 对代码进行重构。抽出可复用的代码，确保 editcore-[gui|cli] 架构逻辑准确。
优化应用文本指示。保持事件驱动并优化不必要的反复调用路径。增强可维护性，避免反复测试。
测试时不要再打开 example.com 和 7zip 网站。**

先把"哪里是错的"量清楚再动手：跑了两份**只读审计**（EditCore 边界 + 每帧开销），
两份都给出 `file:line` 证据。审计的价值在于**否定我自己以为没问题的地方** —— 结果确实翻出四个真问题。

### 一、EditCore 边界：从"约定"变成"编译器"

审计结论原文：*只有文档本体是编译期强制的（`doc` 私有 + `&Document`），保存时的改写、
journal 的写原语、revision/dirty/保存目标这些字段、以及广播发射，全靠约定。*

- **`doc` 私有、无 `&mut Document` 出口**（这条本来就是对的，审计确认没有 `RefCell`/`unsafe`/`Deref` 后门）。
- 14 个 `pub` 字段 → 私有 + 窄接口（`path()` / `revision()` / `source_format()` …）；
  外部只有两处需要**写**（GUI 开 verbose、控制通道给远端命令打 origin），走显式 `set_*` 方法。
- `journal` 里五个"拿 `&mut Document` 直接改文档"的写原语 → `pub(crate)`：
  GUI 与 CLI 是**另一个 crate**，从此**编译不过**，而不是"靠自觉"。
- **保存链路上的暗改（真 bug）**：写 `.opm` 容器时把 `meta.audio`/`meta.background` 规范成包内相对名，
  原先**在 `save_as` 里直接改内存文档**：没 `+revision`、没广播（界面缓存看不见），
  而且 `write_file` 在它**之后** ⇒ 写盘失败会留下"文档被改、文件没写、没有任何信号"的状态。
  现在改名先算在**副本**上写出文件，写成功才**走 `{"op":"set_meta"}` 命令**落到内存文档
  （记 journal、可撤销、按话题广播）—— 保存要么完整发生，要么什么都没发生。
- **空话题广播静默丢失（真 bug）**：`emit` 的注释写着"空话题 = 什么都可能变了，按全量话题发出"，
  但代码根本没做这件事；而 `Subscribers::emit` 是"话题命中才投递" ⇒ 一条记录不出改动的命令
  会发出**投递 0 个订阅者**的广播：revision 涨了、界面不知道。修在 `emit` 里（顺手补两个方向单测）。
- **订阅表只涨不落（真 bug）**：`Subscribers::emit` 的 retain 两个分支都返回 `true`，
  "顺带清理已断开的订阅"从来没生效过 ⇒ `ui_stats.subscribers` 只增不减。修 + 单测。

### 二、抽出可复用的：消灭"同一件事三份说法"

- `save_ext()` 自己写了一份格式→扩展名匹配，而**对话框的提示文字是第三份说法**：
  提示说"opm → `.opm.json`"，实际写出的是 `.opm`。现在扩展名的**唯一出处**是
  `codec::Format::extension()`，判据与真正写盘用的 `SaveFormat::resolve` 完全同一份；
  提示文字改成从它拼出来（`opm 包 → .opm；裸 opm → .opm.json；RPE → .json`）——**文字不再可能和实际行为不一致**。
- 模态外观上一轮已收进 `dialog`；这一轮把**状态栏的文档标识**收成一处（原先顶栏与状态栏各算一遍，
  每帧两次锁 + 两次路径解析），并且把它写成能读懂的话：`● 有未保存改动` / `✓ 已保存` / `尚未保存`，
  悬停给完整路径或"第一次保存会弹保存窗口"。
- 测试里手抄的规则副本：`tests/boundary.rs` 自己实现了一遍 `overlay::prefer_edge`。
  真身搬进库里（`state::prefer_edge` + `EventEdge`，`overlay` 再导出），测试改为调真身 ——
  与 `keymap::shortcut_playback` 那次同一条纪律：**测试不许有第二份规则实现**。
- 删掉确认无调用方的死代码：`control::VIEW_OPS`（而且内容已过期）、`control::Shared`、
  `state::chart_of_lines`、`doc::Track`/`sort`/`value_at`。
  `doc::to_official` / `notes_sorted` **保留**：它们是"官谱导出"这条待办的占位，删了等于丢工作；
  但把"目前没有任何调用方"写进 AGENT-API 的限制清单，免得 agent 当成已实现的功能。

### 三、文本指示：过期的话比没有话更糟

除了上面那条扩展名提示，还清掉三处**与代码矛盾**的文字：

- AGENT-API §7「多 BPM 的时间映射未完成」—— `perf::TimeMap` 早就分段线性了；
- 同节「codec 未接：RPE / 官谱的导入导出尚未实现」—— RPE 与 `.opm` 容器都已实现，只剩官谱；
- `main.rs` 里"resize_event 会同步邻块"的注释 —— 那个语义用户明确否掉了（`core.rs` 的注释写着）。
- README 顶栏示例还留着早已移除的 `💾 保存 •` 按钮 → 按当前真实文案重写，并补上状态栏那一格。

### 四、事件驱动：表现层也不许每帧白做工

文档层本来就事件驱动（广播 → 脏位 → 只重建脏的那块），**表现层**还留着几处与帧无关的重算，
逐条按"多久跑一次 / 花什么代价"清掉：`availability()`（每帧读 PATH + 每个候选两次 `stat`，约 10 次分配）、
最近列表（每条记录每帧一次 `is_file()` + 一次 `format!`）、`env::var("OPM_LAUNCH_AUTO")`（每帧一个 String）、
`publish_stats()`（每帧克隆并排序最多 512 个样本、抢 4 次锁）、状态栏/顶栏各一次锁 + 路径解析、
音频文件名每帧解析。做法一律是**缓存 + 在事件上失效**（`OnceLock` / 行快照 / `file_badge` / 10 Hz 节流）。

**按实测拒绝的两处**：审计把"演奏区实例每帧重建"列为 HOT #4，但实测实例构建 p50 **0.002 ms**
（20 万音符也一样，只有可见的约 29 个实例会上报）⇒ 为 2 µs 加门控，换来"缓存键漏一个字段 =
画面停在旧的一帧"这类难查的 bug，不划算；叠加层的网格/刻度同理。**判断依据是数字，不是"每帧 = 坏"。**

### 五、测试不再有副作用

`filedialog` 那条 URL 测试**真的调了 `open_url`**，于是每跑一次 `cargo test` 就弹出
example.com 与 7-zip 官网两个标签页（用户点名）。现在校验与"真的去开"分开：
纯函数 `check_url()` 单测（含 `file://`、裸路径、`javascript:`、空串、`ftp://`、大写 scheme 的拒绝），
`open_url` 只负责过校验 + 起进程。AGENT-API 里补上"测试不许有外部副作用"这条约定。

### 顺手做的结构整理

`App::ui` 里的启动页分支（约 140 行，含它自己的"本帧到此为止"收尾）抽成 `App::launch_page()`：
主函数只留编辑页那一条路。收尾（截屏/统计/帧计数/心跳）跟着一起搬，并在注释里写明为什么 ——
**早退分支最容易被漏掉某一步**，这个项目已经踩过两次（漏字体装载 ⇒ 中文豆腐块；漏 `pace` ⇒ 只出几帧就停）。

### 实测

- `cargo test` **114 条全绿、0 警告**（新增 5 条：空话题广播两个方向、订阅表清理、`check_url`、
  列表行快照；`open_url` 那条改为不联网）。
- 截图复核：`artifacts/status-badge.png`（状态栏新文案：`[opm] multiline-demo.opm.json ✓ 已保存` /
  `[opm]（未命名） ● 有未保存改动`）、`artifacts/launcher-list-rows.png`（行快照画出来的行：
  曲名 + `[opm] 2 分钟前` + ✕，用的是临时 `XDG_CONFIG_HOME` 造的假记录，不碰用户的配置）、
  `artifacts/file-dialog.png`（扩展名提示改成从 `Format::extension()` 拼出来）。
  复核时**抓到一个我自己引入的 bug**：`file_badge` 原先只在 `sync_file_fields` / `apply_dirty` 里算，
  而 `--doc FILE` 这条启动路径两个都不经过 ⇒ 已载入的文件被显示成「尚未保存」。
  修法是把拼装逻辑抽成纯读函数 `file_badge_of(&SharedCore)`，`App::new` 的初值与事件刷新共用同一份。
- 编译期证据：把 `journal` 的文档写原语降为 `pub(crate)` 后 GUI/CLI 仍能编译 ⇒
  证明它们本来就只走 `exec`（不是靠"我认为它没走"）。

## 7.36 重构第二轮：模块边界（cli / view / dirty）+ 重叠检测归 EditCore（Linux 侧，2026-09-27 续）

用户中途加了一条：**"事件重叠检测算法放到 EditCore，可供 GUI 和 CLI 调用。"**
这一轮先把"纯派生逻辑搬出 main.rs"做完，再把重叠检测的归属改对。

### 一、把三块纯逻辑搬进库（main.rs 4497 → 3921 行）

| 新模块 | 搬进去的东西 | 为什么 |
|---|---|---|
| `cli` | `Args` / `Workspace` / `parse(argv) -> Parsed` / `USAGE` | 这串参数是 **agent 与 CI 的唯一入口**，而它原先直接读 `std::env::args()`、`--help` 时 `exit(0)` ⇒ **一条测试都写不了**。现在解析是纯函数：返回参数 + 警告 + 是否打用法，`main` 只负责打印。顺手让 `--ws nonsense` **出声**（原先静默忽略，用户会以为"我明明设了"）、`--pos 100` 明确报"要 X,Y 两个数" |
| `view` | `LineRow` / `line_rows_of` / `Inspector` / `EventView` / `NoteView` / `NoteEdit` / `EventEdit` / `inspector_of` | 纯派生快照（选中的线/音符/事件 → 那一栏该显示什么），与 egui 无关 ⇒ 可以**断言**："选中第 1 条线的第 2 个音符之后，可编辑字段里的 `isFake`/`speed` 必须来自文档" |
| `dirty` | `Dirty` / `dirty_of_topics` / `merge` / `any` | "只重建脏掉的那块"的全部实现，而它**此前零测试**。现在每条落点都有断言，包括那条踩过坑的规则：`Track` 只落事件轨道缓存，**不许牵动音符列表与检查器** |

**搬常量时差点编数**：`Workspace::timeline_frac`（时间轴占中央区高度的比例）没写进任何文档、也没有测试钉住。
搬走时我先填了 0.30，然后**用像素比对反推**：拿重构前的二进制与候选值各截一帧（1600×900）逐像素比，
找到**完全一致**的那一个（0.22；0.25/0.28 各差约 670 行），再在 1600×1100 上复核 —— 两个高度都逐像素一致。
**教训**：界面比例这类常量只在渲染结果里，重构前该先量一遍；"看着像"是会写错的。
（另记一条方法论：**前 ~10 帧的截图不能当基准** —— 布局还在收尾，同一份代码的第 8 帧也会不同。
1100 高度上第 8 帧比对曾报 642 行差异，第 20 帧就完全一致。）

### 二、事件重叠：算法在纯函数，**归属权**在 EditCore

原先：GUI 自己维护 `conflicts` 缓存 + "这次动过哪条线"的判断（`cmd::overlaps` / `overlaps_of_line`），
CLI 完全没有这个能力 —— 同一件事两套实现。

现在：`EditCore` 持有 `overlaps: Vec<Overlap>`，在**每个文档变更点**刷新
（`exec` 成功按"记录的改动"增量、`undo`/`redo`/`abort`/`load_into`/`replace_doc` 全量），
对外两个读口：`EditCore::overlaps()` 与查询命令 `{"op":"overlaps"}`（**不改文档**，revision 不动）。
GUI 只把这份缓存抄出来显示；CLI 多了 `opm-ctl --file F overlaps [--json]`（有重叠 → 退出码 4）。

- **顺序稳定**：增量刷新是"删该线的旧结果再追加"，不排序 GUI 里那几行会跳 ⇒ 统一按
  `(line, layer, track, next, start)` 排序，于是 GUI 列表与 CLI JSON 可逐字比对。
- `Change::line()` 是新加的纯访问器：把"这条改动落在哪条线"从 `journal` 里问出来（事务/增删线返回 `None`
  ⇒ 走全量），有单测。
- 实测：同一份谱面，GUI 的冲突浏览器列的两条与 `opm-ctl … overlaps` 列的两条**完全一致**
  （`artifacts/conflict-browser.png`）。

### 三、修掉一个真 flake（上一轮留下的未解之谜）

上一轮记录过一次"lib 测试偶发 1 条失败，但没抓住名字"。这一轮复现并抓住了：
`zip::tests::detection_requires_an_invocable_program` 与 `explicit_env_override…` 交替失败，
探针打出真实 errno：**ETXTBSY（26，"Text file busy"）**。

根因：内核拒绝 exec "正被以某进程写方式打开"的文件。测试是**多线程**跑的 ——
一个线程刚写完假 7z 脚本、另一个线程恰好在同一瞬间 `fork()`（别的测试在起进程），
**子进程继承了那个写 fd** ⇒ 紧接着的 exec 撞上 ETXTBSY。12 次里约 1 次。

修法：`can_invoke` 对 ETXTBSY 有界重试（5 次 × 20 ms），其它错误（EACCES/ENOENT）**照旧立即失败** ——
别把真错误拖成启动变慢。修后 zip 那组测试连跑 **14 轮全绿**（修前 12 轮里必挂 1 次）。
`is_text_file_busy` 是纯函数，有单测钉住"只对 26 重试"。

### 四、实测

- `cargo test` **139 条全绿、0 警告**（本轮新增：cli 9 条、dirty 6 条、view 5 条、core 重叠 3 条、ETXTBSY 1 条）。
- CLI 端到端：`opm-ctl --file ov-test.opm.json overlaps` → 列出 2 处、退出码 4；`--json` 给出结构化条目；
  干净谱面 → "没有事件重叠"、退出码 0。
- GUI 端到端：`--doc ov-test.opm.json` 截图 → 冲突浏览器 2 条、底栏 `⚠ 2 处事件重叠`，与 CLI 同源同文案。
- 渲染等价性：1600×900 与 1600×1100 两处截图与**重构前的二进制逐像素完全一致**（每 3 列采样）。

## 7.37 重构第三轮：编辑意图收口 + 判定线树独立（Linux 侧，2026-09-27 续）

这一轮把"界面手势 → 命令"这条**写路径**也收进库里，并把左侧面板拆成自己的模块。

### 一、`edit` 模块：手势翻译成命令（纯函数 + 6 条单测）

原先命令 JSON 散在 UI 里手写（`json!({"op":"set_note", …})` 一处一处拼），字段名写错、hold 忘了带
`endBeat`、切分点写成浮点拍 —— **都是看不见的功能性 bug**，只有"拖一下看看"能发现。现在统一到
`opm_app::edit`：

| 函数 | 干什么 | 单测钉住的规则 |
|---|---|---|
| `note_drag_command` | 拖音符 → `set_note` | tap 不带 `endBeat`；hold **保持时长**（起点吸附结果决定终点） |
| `event_resize_command` | 拖事件头尾 → `resize_event` | `edge` 映射、轨道取当前选中、`layer=0` |
| `place_note_command` | 双击放音符 → `add_note` | 落在**已吸附**的 laneX/拍 上 |
| `set_line_command` / `set_event_command` / `set_note_command` / `split_event_command` / `del_event_command` | 检查器与树上的编辑 | 文档字段名（`zOrder`/`isCover`/`bpmFactor`）、`layer=0` |
| `milli_beat` | 拍 → **毫拍**有理数 | 切分点由播放头决定、不在格线上 ⇒ 用毫拍，**别把浮点拍写进文档** |
| `begin_command`/`commit_command` | 一段拖拽包成一个撤销步 | 标签是人话（会出现在撤销提示里） |

还有一条端到端测试：把 `begin` → 拖动命令 → `commit` 真的喂给 `EditCore`，断言文档里的音符动了、
hold 时长没变、**一次撤销整段退回**。

### 二、`tree` 模块：判定线树独立

左侧面板（判定线 → 事件轨道 → 事件 → 子音符，含"在播放头切分/删除选中"）184 行搬进 `src/tree.rs`：
它只读 `&EditorState`、把点击写成 `TreeAction`，**自己不施加动作** —— 于是"怎么画"与"点了之后发哪条命令"
分开，后者统一走命令路径。`main.rs` 只剩调用点与动作施加。

### 三、死代码

审计里最后两个无调用方的辅助（`Broadcast::touches` / `touches_line`）删掉 —— 复核 grep 确认零引用。

### 四、实测（**环境限制也记下来**）

- `cargo test` **145 条全绿、0 警告**（本轮新增 6 条：`edit`）。
- **渲染等价性是逐像素的**：1600×900 与 1600×1100 两个尺寸下，重构后的二进制与**重构前的二进制**
  截图完全一致（每 3 列采样比对）⇒ 这一轮所有搬动（cli/view/dirty/edit/tree + 重叠归属）对界面零影响。
  启动页列表、冲突浏览器同样逐像素一致。
- 新建模态那张有 **±1/通道的全局差**：两帧相隔数分钟拍摄，判断是合成器对"活动/非活动窗口"的轻微
  压暗差别（不是代码差异）—— **未能二次确认**：随后整个会话失去了帧回调。
- **环境故障（不是代码问题，有证据）**：本会话连续创建约 60 个 GUI 窗口之后，KWin **不再给新窗口
  投递帧回调** —— `WAYLAND_DEBUG=1` 显示 `wl_surface.frame` 请求发出后**永远没有 `wl_callback.done`**，
  于是应用停在第 0 帧。对照实验：**重构前的旧二进制同样卡住**、编辑页同样卡住、换位置/换尺寸/等 20 秒
  都不行 ⇒ 与本次改动无关。当时的临时探针（帧号打印）已删除。
  **教训**：截图验证要**批量、少次**地做；`--shot` 依赖合成器活着，而它是会被本会话用量拖垮的外部状态。
  无合成器时仍有两条可用的端到端检查：`opm-ctl render`（无头 wgpu 出图，实测 800×600 实例 47 正常）
  与无头 egui 单测（弹窗/启动页/表单逻辑都在里面）。

## 7.38 重构第四轮：演示谱面 / 音频路径决策 / 可见区间（Linux 侧，2026-09-27 续）

这一轮**全程不依赖截图**（合成器仍未恢复，见 §7.37 末尾），只搬"测试能覆盖"的东西。

| 搬什么 | 搬到哪 | 为什么 |
|---|---|---|
| `build_demo_via_core`（`--notes N` 的演示谱面） | 库 `opm_app::demo` | `--notes` 是 bench/压测/造图共用的入口，而它原先只长在 GUI 里；现在 GUI 与 `opm-ctl new --demo-notes N` **同一份实现**。搬时顺手删掉一条**与实现矛盾的旧注释**（"直接写模型，不走命令循环"——它说的正是这份实现刻意**不**做的事） |
| `resolve_audio` 的路径决策 | 库 `audio::resolve_source` | 纯路径规则（`off`/显式/谱面字段相对**谱面所在目录**/没有路径时按 CWD），原先混在"开音频设备"里，一条测试都写不了；现在 6 种情形都有断言，来源标签（`--audio` vs `谱面 meta.audio`）也一并测 |
| `state_visible` | `EditorState::visible_range()` | 三行的纯视图计算，放在状态里顺手补了"末尾截断"的断言（曲末不该显示到谱面之外） |

顺带把 `demo` 的失败处理改成**返回值**（`DemoBuild { commands, failed }`）：库里只管造，"怎么报"属于界面。

### 一个值得记的坑：插入测试块时**偷走了上一条测试的 `#[test]`**

搬完之后 `cargo test` 报告 **151 条通过**，但编译警告里有两行：
`duplicated attribute` 与 ``function `timeline_height_survives_tiny_windows` is never used``。
原因是我把新测试插到了**已有 `#[test]` 属性的下一行** —— 那个属性于是落到了我的函数上（它的 `#[test]` 被当成重复属性），
而原来那条测试**静默地不再运行**。测试数是涨的（+1），所以只看"通过数"根本发现不了。

教训与项目里既有那条一脉相承：**"never used / duplicated attribute" 这类警告就是"测试/代码丢了"的信号**，
`cargo test` 必须保持 0 警告；只看通过条数会漏。

### 实测

- `cargo test` **151 条全绿、0 警告**（本轮新增 6 条：`demo` 4、`audio` 1、`state` 1）。
- CLI 端到端（不依赖合成器）：`opm-ctl new --demo-notes 20 --out …` 正常造谱；`opm-ctl render` 出图正常。
- `main.rs`：3716 → **3610 行**（本轮 -106）。

## 7.39 重构第五轮：属性编辑器搬出 + **一个静默失效的真 bug**（Linux 侧，2026-09-27 续）

### 一、真 bug：属性编辑器只在"调试"工作区生效

搬右侧属性检查器时读代码，发现它把命令攒在局部 `ec` 里，而**施加**那几行写在
`if self.ws == Workspace::Debug { … }` 里面：

```rust
if self.ws == Workspace::Debug {
    … 一屏诊断信息 …
    if !ec.is_empty() { self.dispatch(&ec); self.insp = self.build_inspector(); }   // ← 在这里面！
    … 更多诊断 …
}
```

也就是说：**在默认的"制谱"工作区里，改线名/zOrder/isCover/线速、改事件的值与缓动、改音符的
kind/拍/laneX/alpha/isFake/speed/宽度/yOffset —— 命令被算出来又被丢掉，什么都不发生。**
CLI 走同一条命令语言是好的，所以"同一件事两处行为不一致"就这么藏了很久：
GUI 看着有控件、能拖、数字在动（本地草稿），文档却一笔没改。

修法：把面板抽成 `inspector::inspector_ui(ui, &state, insp) -> Vec<Value>`（**返回**命令），
调用点**无条件**施加：

```rust
pending_edits.extend(inspector::inspector_ui(ui, &self.state, self.insp.as_ref()));
…
if !pending_edits.is_empty() { self.dispatch(&pending_edits); self.insp = self.build_inspector(); }
```

"什么时候才 dispatch"这种分支现在**不存在**了 —— 这类 bug 的根因就是"算"与"用"被隔在条件分支两侧。
顺手去掉每帧的 `self.insp.clone()`（改用 `as_ref()`）。

**同类排查**（同一个 bug 类，一次查干净）：树动作（`TreeAction::Cmd`）、叠加层动作
（`apply_overlay_actions`）、控制台、文件对话框 —— 命令的施加点都在无条件路径上；
`serde_json::json!` 在 `main.rs` 里只剩 5 处直接 `exec` 的生命周期命令（load/save/validate/set_meta 名）。
**这条排查值得成为习惯**：只要看到"收集命令的 Vec"就顺手确认它的施加点没有被包在某个条件里。

### 二、抽出 `inspector` 模块（218 行）+ 无头回归网

`src/inspector.rs`：只读 `EditorState`（网格吸附/选中项）与 `Inspector` 快照，**返回**命令。
`main.rs` 3610 → **3398 行**。

配套两条**无头 egui** 测试（不需要合成器）：
- 画出"线名 / isCover / 事件 · moveX / 音符 doc#0 / 此刻表演"等文本 ⇒ 面板还在、没被改哑；
  并断言 tap **不该**出现 hold 才有的结束拍字段；
- **没碰任何控件 ⇒ 一条命令都不发**（面板每帧乱发命令会让撤销栈爆炸 —— 这条不变量之前没人钉住）；
- 没选中判定线时画"（没有判定线）"，而不是一片空白（用户要知道为什么右边是空的）。

### 三、实测

- `cargo test` **153 条全绿、0 警告**（本轮新增 2 条：无头 inspector 面板）。
- **合成器仍未恢复**（`wl_surface.frame` 无回调，见 §7.37），所以"拖一下滑块文档真的变了"这句
  还需要一次肉眼确认 —— 我把它标成**未验证**，不写成已验证。

## 7.40 重构第六轮：冲突浏览器 / 状态栏搬出 + 收口（Linux 侧，2026-09-27 续）

### 一、又搬两块（`main.rs` 3670 → 3324 行）

| 新模块 | 内容 | 无头测试钉住什么 |
|---|---|---|
| `conflicts`（179 行） | 冲突浏览器面板 + `ConflictJump`（跳转目标） | 每条重叠都列出来（文案取自 `Overlap::label`，与 `opm-ctl … overlaps` 同一批字）、**没点就不跳**（否则一打开就自己跳走）、空列表画"0 处事件重叠" |
| `statusbar`（273 行） | 底部状态栏 + 三态保存标识 | 一行里该有的读数都在（播放头/拍、播放中、`♪ 无音频`、网格、`[opm] demo.opm`、`● 有未保存改动`、窗口偏移、`⚠ N 处事件重叠`）、冲突清零后换 `✓ 无事件重叠`（且只在浏览器还开着时显示）、偏移为 0 时不占地方 |

`statusbar` 把"保存状态"抽成纯函数 `file_mark(dirty, has_target) -> (文案, 颜色)`：
三态分别是 `● 有未保存改动` / `✓ 已保存` / `尚未保存` —— 原先这些字埋在一个 120 行的 egui 块里，
只能靠开窗口看，现在有断言。面板**不锁核心、不解析路径**：显示要用的东西由调用点算好放进
`StatusView`（`file_badge`、网格文本都是缓存过的），校准偏移是状态栏**唯一**的写（而且写的是视图设置）。

### 二、量 UI 的规矩又多一条：**滚动区要第二帧才有内容**

`conflicts` 的第一版测试在单帧里断言"每条都画出来了"，结果只找到标题：`ScrollArea` 在第一帧还不知
道自己多大，内容会被裁掉。跑**两帧**（第一帧建布局、第二帧断言）就对了 —— 与 `recents` 那条
"第一帧只是建立布局"是同一个规矩，这次是它在滚动区上的表现。

### 三、收口：不依赖合成器的端到端证据（本会话合成器仍未恢复）

| 检查 | 结果 |
|---|---|
| `cargo test` | **160 条全绿、0 警告** |
| `opm-ctl --file … render`（无头 wgpu 出图） | 640×480，实例 47，正常写出 |
| `opm-ctl --file … overlaps` | 2 处，退出码 4，label/pointer 与 GUI 冲突浏览器**同一批字** |
| `opm-ctl new --demo-notes 12`（库里 `demo` 模块） | 正常造谱 |
| `opm-ctl convert … --to rpe` | 4 条线 / 400 音符 / 1 条 BPM，**无降级** |
| `opm-app --audio-probe FILE`（不开窗口） | 解码信息正常（48 kHz / 60 s / 2 880 000 帧） |
| `opm-app --help` | 用法正常（`cli::USAGE` 单一出处） |
| GUI 启动到首帧 | 适配器/字体正常、**无 panic**；随后停在合成器（见 §7.37） |
| 测试副作用 | 测试里没有任何 `open_url` 调用（"获取 7z"的真身只在交互路径） |

**未验证（如实列出）**：① 属性编辑器"拖一下文档真的变了"需要肉眼确认（修好了但合成器不给我截图）；
② 状态栏/冲突浏览器/属性编辑器这三块搬出后的**像素等价性**没做（前五轮的等价性已用像素比对确认过）。

### 四、这一系列重构的账（六轮累计）

- `main.rs`：4497 → **3324 行**；新模块 11 个（`dialog` / `cli` / `view` / `dirty` / `edit` / `demo` /
  `tree` / `inspector` / `conflicts` / `statusbar` + 库内已有的 `shot`）。
- 测试：106 → **160 条**；`cargo build`/`cargo test` 全程 **0 警告**。
- 真 bug 四个：保存时的暗改（绕过命令路径）、空话题广播静默丢失、订阅表只涨不落、
  属性编辑器只在调试工作区生效（最严重的一个 —— 默认工作区里改什么都不发生）。
- flake 一个：`can_invoke` 撞 ETXTBSY（12 轮必挂 1 次 → 修后 14 轮全绿）。

## 7.41 新功能：Q/W/E/R 快速放置 + hold 跟随（Linux 侧，2026-09-27 续）

用户：**"qwer按键快速放置不同note（tap, flick, drag, hold）。hold放置时，第一次按下r进入hold跟随状态，
hold长度随鼠标移动。按esc取消，按r或回车放置。hold支持事件块同款时刻控制杆（抽出控制杆代码）"**，
随后追加：**"在hold跟随状态下，保持编辑区的滚动功能和缩放功能不变"**。

### 一分钟看懂的分工

| 谁 | 干什么 | 为什么放这里 |
|---|---|---|
| `keymap::quick_place_kind` | Q/W/E/R → tap/flick/drag/hold | 纯映射，可单测；不管"现在能不能按" |
| `overlay` | 指针对音符区的命中 + 吸附（laneX/拍）+ **产动作** | 位置解析只有它懂（它才知道窗口偏移与网格） |
| `state::PendingHold` | 跟随状态的**视图数据**（起点/终点/保底长度/拖控制杆） | Esc 取消、鼠标改长度都是"看与操作"，不进撤销栈 |
| `edit::place_note_command` | 手势 → `add_note` 命令（kind + hold 的 endBeat） | 与双击放置、拖拽共用同一条命令构造路径 |
| `main.rs` | 施加动作：改视图状态 / 发命令 / 报一句人话 | 与其它面板同一条写路径 |

**键门控只有一处**：`overlay::draw(..., keys_enabled, ...)`，由 `main.rs` 用
`!typing && !modal_open` 算一次 —— 在控制台打字或弹窗打开时，Q/W/E/R 与 R/回车/Esc 都不生效。

### 抽出"时间控制杆"（用户点名要抽的）

事件块的头/尾把手原先散在绘制循环里（命中用 `hit_event_part`、画线用几行 painter、拖拽时又自己算
"指针 → 拍"）。待放置的 hold 要用同一套，于是收成 `overlay::TimeHandle`：

- `hit(pointer)` —— x 先落在范围内，y 交给 `hit_event_part`（**与事件块同一份规则**）；
- `paint(painter, edge, color)` —— 头 = 实线 + 短竖标记、尾 = 实线（事件块高亮与 hold 草稿同源）；
- 事件块的高亮改成"构造一个 `TimeHandle` 再 paint"，行为不变、代码少一半。

### 三个实现细节（都是测试逼出来的）

1. **自动重复**：egui 的 `key_pressed` 把 `repeat: true` 也算"按下"（`num_presses` 只看 `pressed`）⇒
   按住 `R` 会"放下 → 又开始放 → 又放下"。改用只认 `repeat: false` 的 `key_pressed_once`，
   并加了单测（连续 4 帧同一键、后 3 帧是重复 ⇒ 只允许产出一次 `QuickPlace`、不许 `Commit`）。
2. **滚动/缩放不受影响（用户要求）**：跟随只认"指针**移动**"（`pointer.delta() != 0`），
   不读也不消费滚轮事件 ⇒ 滚轮仍产出 `ScrollBeats`、Ctrl+滚轮仍 `ZoomBeats`，而长度**不变**。
   单测：跟随状态下跑 8 帧滚轮 ⇒ 有 `ScrollBeats`、没有 `PendingHoldFollow`、`PendingHold` 逐字段不变；
   Ctrl 变体同理。
3. **第一帧没有悬停状态**：egui 首帧 `hover_pos()` 还是 None ⇒ 测试要用"第 0 帧放指针、第 1 帧按键"，
   并且**补一个 release 事件**（同一 `Context` 里键盘有状态，不发 release 的话第二次按下会被标成重复）。

### 实测

- `cargo test` **170 条全绿、0 警告**（本轮新增 3 条 overlay：控制杆命中、快速放置动作与吸附、
  跟随/提交/取消 + 键门控；另有自动重复与草稿几何各一条）。
- **合成器在本轮恢复**，于是这次能真看：`artifacts/pending-hold.png` 与
  `artifacts/pending-hold-handles.png`（放大）——草稿从 0.0 拍到 4.0 拍，**头（下方，带短竖）与尾（上方）
  两个控制杆都在**，位置与左侧拍标注对齐。
- 恢复后顺手复核了第 4~5 轮那三块（状态栏 / 属性编辑器 / 冲突浏览器）在真实窗口里正常。

## 7.42 草稿也能造事件 + 左键确认（Linux 侧，2026-09-27 续）

用户："**hold的确认放置方式添加鼠标左键；在事件区按可以跟hold一样创建事件。**"

### 一个"草稿"概念，两处落点

`hold` 与**事件块**的放置只有落点不同（音符区 / 事件列），流程完全一样：起点定在指针处 ⇒
长度随鼠标 ⇒ 控制杆可拖 ⇒ `Esc` 取消 ⇒ **`R` / 回车 / 左键**放下。于是：

| 层 | 变化 |
|---|---|
| `state` | 新增 `PendingEvent { track, layer, start, end }`；跟随/拖控制杆/取值三条规则抽成**自由函数**（`follow_span` / `resize_span` / `span_of`），`PendingHold` 与 `PendingEvent` 共用一份 —— 两份实现迟早会分叉（一份允许反向、一份不允许，用户就会觉得"手感时好时坏"）。`begin_*` 互相清除：**同时只放一个东西** |
| `edit` | `place_event_command`（新事件是**平段**，不带跳变）+ `new_event_value`（**优先取该轨道此刻的值**，空轨道才用中性值：移动/旋转 0、透明度 1、流速 10 —— 依据 `state::TrackId` 里写的量纲） |
| `overlay` | 动作从"hold 专用"改名成**草稿通用**（`DraftFollow/Resize/Commit/Cancel`）；新增 `StartEventDraft{track, beat}`；事件区按键取**指针所在那一列**的轨道；**左键单击 = 放下**（`clicked_by(Primary)` 且未拖动 —— 拖控制杆仍改起止、拖别处仍跟随）；草稿期间左键不再点选 |
| `main.rs` | 施加：哪个草稿在跟随就改哪个；放下时按类型发 `add_note`(hold) 或 `add_event` |

### 实测

- `cargo test` **173 条全绿、0 警告**（本轮 +3：事件草稿规则与互斥、事件区起草稿/跟随/左键放下、
  事件放置命令与初值）。
- 截图（合成器已恢复）：`artifacts/pending-event.png` —— 草稿落在 **moveX 那一列**、2→6 拍，
  头（下方，带短竖）与尾（上方）两个控制杆齐全；`artifacts/pending-hold.png` 是 hold 版。
- 顺带把 `OPM_EDIT_AUTO` 扩成两式：`hold:<lane>,<start>,<end>` / `event:<track>,<start>,<end>`。

## 7.43 显卡策略：默认核显，独显只在显式指定时用（Linux 侧，2026-09-28）

用户："**在linux上让显卡调用策略遵循prime-run，不要在没有指定的情况下去调用功耗更高的nvidia显卡。**"

先说现状（实测）：启动日志里 `适配器: NVIDIA GeForce RTX 5070 Laptop GPU [Vulkan/DiscreteGpu]` ——
**什么都没设，程序就在用独显**。根因在 wgpu/egui-wgpu 的默认值：
`WgpuSetupCreateNew::from_env_or_default()` 里 `power_preference` = `WGPU_POWER_PREF` 解析值，
**没有就是 `HighPerformance`**。

### 做法：自己当"适配器选择器"

`WgpuSetupCreateNew.native_adapter_selector`（`Arc<dyn Fn(&[Adapter], Option<&Surface>) -> Result<Adapter, String>>`）
可以完全接管选卡，而且**比 `power_preference` 更可控**：它拿到全部候选，能按策略挑、能把决定打出来。
于是策略判定收进库里 `opm_app::gpu`（纯逻辑，不依赖 wgpu 类型 ⇒ 可单测），`main.rs` 只做
"`wgpu::DeviceType` → `GpuKind`"的映射与选卡闭包。

| 输入 | 策略 |
|---|---|
| `OPM_GPU=discrete|integrated` | 明确要独显 / 核显（优先级最高） |
| `__NV_PRIME_RENDER_OFFLOAD=1`、`__VK_LAYER_NV_optimus=NVIDIA_only`、`__GLX_VENDOR_LIBRARY_NAME=nvidia`、`DRI_PRIME≠0` | 独显（**这就是 prime-run**） |
| `WGPU_POWER_PREF=high|low|none` | 独显 / 核显 / 不干预（尊重 wgpu 自己的变量） |
| 都没有（Linux） | **核显优先** |
| 非 Linux | 不干预（交给平台默认） |

打分表保证"没有核显的机器仍然用独显"（核显 4 / 独显 3 / 虚拟 2 / 其它 1 / **软件渲染 0**）——
"默认不用独显"不等于"宁可用 llvmpipe 也不给用硬件"。选择器还会先过滤掉
`!adapter.is_surface_supported(surface)` 的候选（选一个不能呈现的等于自找黑屏）。

### 实测（用启动日志核对，不靠猜）

| 场景 | 选中的适配器 |
|---|---|
| 默认 | **AMD Radeon 610M（RADV，IntegratedGpu）** |
| `DRI_PRIME=1` | NVIDIA RTX 5070（DiscreteGpu） |
| `OPM_GPU=discrete` | NVIDIA RTX 5070 |
| `OPM_GPU=integrated` | AMD Radeon 610M |
| `prime-run`（系统真脚本） | NVIDIA RTX 5070（日志里还能看到 GL 那条 NVIDIA 适配器被标"不能出图到这个 surface"而被排除） |

候选清单也一并打出来（`适配器候选: [i] 名字 [后端/类型]`），谁都能核对程序为什么挑它。
**只验证了"用了哪块卡"，没量功耗** —— 功耗差别是常识判断，不是本次实测数据。

`cargo test` 新增 4 条（策略判定：默认/prime-run 标记/开关优先级/挑卡打分），共 **177 条全绿、0 警告**。
