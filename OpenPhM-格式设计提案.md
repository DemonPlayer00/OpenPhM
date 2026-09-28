# OpenPhM 谱面格式设计提案（v0.1 草案）

> 定位：OpenPhM 是**制谱器**，拥有**自有格式**（编辑真源），并**能转换为 RPE**。
> 保真要求：**导入无损**（读得进、语义不丢）、**导出语义等价**（不要求字节级一致）。
> 编制日期：2026-09-27。事实来源见第 11 节；**标注「未复核」的条目来自调研代理转述，本机未独立验证**。

---

## 0. 结论摘要

1. **你点名的非官方特性，三个已是 RPE 1.7.0 官方字段**（假音符 `isFake`、音符级透明度 `alpha` + `alphaControl`、自定义判定区 `judgeArea`、音符级染色 `tint`/`color`——后两项 1.7.0 新增）。⇒ **原生格式只需"覆盖 RPE 1.7.0"，不需要为这些另设扩展**。
2. **变速 Hold 暂缓，不纳入格式设计**（2026-09-27 裁决）：该表现由**播放器**决定，编辑器只原样透传 Hold 既有字段（`holdTime`/`speed`/`speedEvents`），不做解释、不做重算。⇒ **v1 不含任何有损扩展**，`opm` 就是 RPE 1.7.0 的纯超集，`x-opm:` 命名空间保留但为空。详见 4.5。
3. **RPE 不是"可选支持"，而是硬性交换层**：生态里所有存量工具（Phira 2681★、sim-phi 316★、PhiZone player 91★、phichain 47★）都读 RPE。⇒ 原生格式必须是 **RPE 1.7.0 的超集**，否则编辑器无法无损往返。
4. **RPE 没有公开格式规范**：Re:PhiEdit 闭源，作者 `cmdysj` 的 GitHub `public_repos = 0`；唯一文档是社区从 **RPE 1.4.1** 起重新编写的逆向文档（CC-BY-4.0），且 Controls 等部分自标"行为待补充"。⇒ **导出保真度的对标对象是"Phira 的实现行为"，不是一份稳定规范**——这直接决定了第 6、8 节的工程策略。
5. **已裁决（2026-09-27）**：格式定名 **`opm`**，作为第一主力；**RPE 兼容层自研**（方案 C，不链接 `prpr`）；许可证 **GPL-3.0-or-later，全程干净**。规范性文档见 [`spec/opm-format.md`](./spec/opm-format.md)，可执行校验器见 [`spec/check.py`](./spec/check.py)。决策记录见第 12 节。

> **文档分工**：本文件是**设计理由与取舍**（为什么这样设计）；[`spec/`](./spec/) 是**规范本身**（字段、约束、校验器、样例）。两者冲突时以 `spec/` 为准。

---

## 1. 定位与边界

| 层 | 角色 | 读写 | 说明 |
|---|---|---|---|
| **opm 格式**（`*.opm.json` / 打包 `.opmz`） | 编辑真源（第一主力） | 读写 | 编辑器的一切编辑操作只作用于它；覆盖 RPE 1.7.0 全字段；规范见 `spec/` |
| **RPE JSON / `.pez`** | 交换与发行 | 读（无损）+ 写（语义等价） | 对标 RPE 1.7.0 语义；**自研 codec，不链接 `prpr`**；不承诺字节级一致 |
| **Phigros Official** | 只读导入 | 只读 | 用于导入官谱做参考/移植；**不导出**（官方格式无假音符、无音符透明度，反向导出必然有损） |
| **PEC** | 只读导入（低优先） | 只读 | 已停止更新、上限 30 条判定线、不支持 XY 分离 |
| **PBC / `.nrc`** | 暂不支持 | — | PBC 文档"待完善"；`.nrc` 规范停更（2025-06-25，2★） |

**明确不做**：不发明一套对外分发的社区标准格式。理由见 bmson 的先例（技术正确、无人维护、规范仓库至今无许可证文件）。`opm` 的定位是**编辑器工程格式**，不是社区标准。

**明确不做（方案 C 的推论）**：不链接 `prpr`。原因是许可证阻塞（第 9 节）与依赖代价（为一个解析层背负整套 `macroquad`/`sasa`/`symphonia` 游戏渲染栈），不是因为技术不可行。

---

## 2. 需求重写：哪些是"扩展"，哪些只是"实现 RPE"

| 你的需求 | RPE 1.7.0 现状 | 原生格式要做的事 |
|---|---|---|
| 假音符 | ✅ `isFake`（官方字段，长期存在） | 直接建模，无扩展 |
| 音符级透明度 | ✅ `alpha` 0~255 + `alphaControl` 关键帧 | 直接建模，无扩展 |
| 音符级染色 / 自定义判定区 | ✅ `tint`\|`color`、`judgeArea`（1.7.0 新增） | 直接建模，无扩展 |
| 音符自定义打击音 | ⚠️ RPE 有 `hitsound`（142 加入），但 **Phira 的解析器读入后未实现**（源码留 TODO） | 建模，但导出前提示"目标播放器可能忽略" |
| **变速 Hold** | ❌ RPE 无原生表达（Controls 对 Hold 无效） | **暂不设计**：由播放器决定表现，编辑器只透传 Hold 既有字段（4.5 节） |

⇒ **v1 的扩展面为零**：原生格式 = RPE 1.7.0 的纯超集，全部工作量在于**"把 RPE 1.7.0 的语义实现正确"**与**"导出时不丢字段、不静默改语义"**，而不是设计新特性。

---

## 3. 架构

```
                 ┌───────────────────────────────┐
   RPE .json ───▶│                               │
   .pez 包   ───▶│   Codec 层（自研）             │
   Official  ───▶│   (rpe / pgr / pec)           │
   PEC       ───▶│   保留 foreign 字段袋          │
                 └──────────────┬────────────────┘
                                ▼
                 ┌───────────────────────────────┐
                 │  opm 数据模型（唯一真源）       │
                 │  · 有理拍时间 + BPM 表         │
                 │  · 判定线 / 事件层 / 缓动      │
                 │  · 音符（字符串类型枚举）      │
                 │  · x-opm: 扩展命名空间         │
                 └──────────────┬────────────────┘
                                ▼
                 ┌───────────────────────────────┐
                 │  校验器 + 能力分析              │
                 │  · spec/check.py（语义 lint）  │
                 │  · requiredCapabilities        │
                 │  · 导出保真度报告               │
                 └──────────────┬────────────────┘
                                ▼
                        RPE .json / .pez 导出
```

**工程纪律（三条，全部有先例支撑）**

1. **单一真源 + 多 codec**：编辑器只认 opm 数据模型，格式差异全部收敛在 codec 层。
2. **版本号表达"最低能力需求"，不表达"编辑器版本"**（照搬 Quaver 的 `DetermineMinimumQuaVersion()`：只有真用了新特性的谱面才被标成高版本，其余老客户端都能打开）。
3. **未知字段必须保留**：导入时把无法识别的字段原样存进 `foreign` 袋，导出时回填。这是"导入无损"的技术实现方式。

**语言无关优先**：规范、映射表、校验器全部落在 [`spec/`](./spec/)，不绑定实现语言——`note-types.json` / `easing.json` 是 codec 与测试的**单一数据源**，实现语言无论选什么，都从这里生成或读取。

---

## 4. 数据模型（v0.1）

### 4.1 时间：有理拍为规范，秒为派生

```jsonc
{ "bpmList": [ { "startBeat": { "n": 0, "d": 1 }, "bpm": 180.0 } ] }
```

- **拍用精确有理数**（`{n, d}`，约分后），**不用浮点**。理由：RPE 的 beat 本身就是 `int[3]`（`[0]:[1]/[2]`），浮点会让"导入→导出"在数值上漂移，直接违反"导入无损"。
- **秒是派生量**：`seconds = beat × 60 / bpm`，仅在判定、渲染、音频同步时计算，不落盘。
- 多 BPM：`bpmList` 按 `startBeat` 升序，与 RPE 语义一致。
- 事件时间同样用有理拍；允许负拍（官谱的事件列表以 `-999999` 起头）。

### 4.2 判定线

字段与 RPE 1.7.0 对齐（下表为**opm 字段名**，与 `spec/opm-format.md` 第 4 节一致）：

| 概念 | opm 表达 | 备注 |
|---|---|---|
| 事件层 | `layers: Layer[1..5]` | 层内并列：`moveX` / `moveY` / `rotate` / `alpha` / `speed` |
| 事件 | `{ startBeat, endBeat, startValue, endValue, easing, bezier?, easingRange?, linkGroup? }` | `easing` = 1~29 的**具名枚举**，不写裸整数 |
| 扩展事件 | `extended: { color, scaleX, scaleY, text, gif, incline }` | 与 RPE 第 5 层同构；`paintEvents` 143 起已废弃，仅保留只读 |
| 控制曲线 | `controls: { pos, size, skew, y, alpha }` | 关键帧轴 = **距判定线的纵向距离 x**（RPE 语义），不是时间 |
| 父子线 | `father: int`、`inheritRotation: bool` | **字段缺失时按 `false` 解析**（兼容 163 以前版本） |
| 其余 | `bpmFactor`、`isCover`、`zOrder`、`texture`、`anchor`、`attachUI`、`isGif` | `bpmFactor` 语义是**除**：线 BPM = 谱面 BPM / bpmFactor |

### 4.3 音符

**关键设计：类型用字符串，不用整数。**

```jsonc
{ "kind": "hold", "startBeat": {...}, "endBeat": {...}, "laneX": 0.0,
  "side": "above", "isFake": false, "alpha": 255, "speed": 1.0,
  "widthScale": 1.0, "yOffset": 0.0, "visibleTime": 999999.0,
  "judgeAreaScale": 1.0, "tint": [255,255,255], "hitEffectTint": null,
  "hitsound": null }
```

> v1 **没有** Hold 速度曲线字段：`speed` 就是 RPE 语义的「Hold 尾速度」，头速度恒为 1，长度由 `holdTime`/`endBeat` 决定，全部原样透传（见 4.5）。

- 整数枚举只存在于 codec 层，且**必须按目标格式分别映射**——这是本生态最贵的坑（见 6.2）。
- `side` 用 `"above" | "below"`，不用 RPE 的 `1`/其它。
- 单位在字段名上体现（`laneX` = RPE 坐标系的 X 单位），并在 schema 中写明换算（X 单位 = 0.05625 屏宽、Y 单位 = 0.6 屏高）。

### 4.4 扩展命名空间（v1 保留为空）

- 所有自有扩展字段以 **`x-opm:`** 前缀命名，集中在音符/判定线的扩展槽中。**v1 不定义任何具体扩展字段**（变速 Hold 已移出，见 4.5），此节仅约定日后新增时的命名与纪律。
- 规则（照搬 bmson 的扩展纪律）：**扩展不得修改 `formatVersion`**；能力声明独立成字段。
- 导出到 RPE 时：能表达的降级表达，不能表达的**丢弃并记入保真度报告**，绝不静默丢。

### 4.5 变速 Hold：**暂缓，不纳入 v1**（2026-09-27 裁决）

**裁决内容**：变速 Hold 的表现**由谱面播放器决定**，不由编辑器格式定义。⇒ 编辑器侧不引入任何 Hold 速度曲线字段。

**由此产生的三条后果，必须写进实现约定：**

1. **编辑器只做透传，不做解释**。Hold 的既有字段原样保留、原样导出：`holdTime` / `endTime`（长度）、`speed`（**Hold 尾速度，头速度恒为 1**）、判定线的 `speedEvents`（沿时间轴的流速）。编辑器**不得**对它们做归一化、压平或"聪明"的重算——播放器怎么解释是播放器的事，编辑器改了就是替播放器做决定。
2. **`x-opm:` 命名空间保留但为空**。v1 不含任何扩展字段，`extensions` 数组恒为空；能力等级 3 保留未启用（第 5 节）。
3. **日后若要重新引入**，纪律不变：走 `x-opm:` 独立字段、不改 `formatVersion`、导出时降级并报报告；若某天由播放器社区定义了自己的承载方式，**优先对齐对方**，不要另起一套。

**遗留的实测项（仍建议做，理由与格式扩展无关）**：RPE/官谱中 Hold 体的速度究竟是端点线性插值还是刚性平移，文档里查不到。编辑器要正确**预览** Hold 长度就必须知道这条；做法：固定 `holdTime`，扫 `note.speed ∈ {1,2,4}` 与线 `speedEvent.value`，逐帧测量 Hold 体长度与尾端到达时刻，反推插值式。**在实测完成前，编辑器的 Hold 预览须标注"长度显示未经实测校准"。**

**同时明确禁止的一件事**（无论将来怎样设计）：**不得通过把一个 Hold 拆成多个 Hold 来表达变速**——每个 Hold 只计 1 次连击，拆分直接改变物量与成绩语义。

---

## 5. 版本与能力

```jsonc
{
  "format": "opm",
  "formatVersion": 1,
  "minClientCapability": 2,        // 由内容计算，不由编辑器版本决定
  "extensions": []                 // v1 恒为空：变速 Hold 已改为播放器侧行为，无扩展字段
}
```

| 能力等级 | 名称 | 含义 | v1 状态 |
|---|---|---|---|
| 0 | `official` | 仅官谱可表达的子集 | 启用 |
| 1 | `rpe-base` | RPE 基础（判定线事件、音符基础字段） | 启用 |
| 2 | `rpe-1.7` | RPE 1.7.0 全量（`tint`/`judgeArea`/Controls/bezier/1.7 流速缓动语义） | 启用（**v1 的实际上限**） |
| 3 | `openphm-ext` | 含自有扩展 | **保留未启用**：v1 无扩展字段 |

三条铁律：**永不删除字段**、**永不静默改变既有字段语义**、**扩展不改版本号**。

---

## 6. 编解码器

### 6.1 RPE 导入（无损）

除常规字段映射外，**必须处理的已知坑**：

| 坑 | 事实 | 对策 |
|---|---|---|
| `META.RPEVersion` 不可靠 | 1.5.0~1.6.0（不含 1.6.0）恒写 `150`；1.6.1 恒写 `160` | **不能靠它选解析路径**；改为"结构嗅探 + 用户可覆盖的行为档位" |
| `color` / `tint` 双名共存 | 文档原文：color 版本被公测后改名 tint，"两个字段可能都有被使用，**定义不变，请注意兼容**" | 两个键都读，导出时按目标版本写 |
| `alpha` 越界 | Phira 源码把 RPE 音符 alpha 读成 `u16` 并注释 `// some alpha has 256...`（未复核，来自调研转述） | IR 用更宽类型，**不做 0~255 截断**，原值保留 |
| 1.7 流速缓动语义变更 | Phira 解析函数带 `use_rpe_170_speed: bool` 与 `SpeedEasingMode`（未复核） | 导入时记录源版本档位；导出时**由用户选择目标档位**，不写死一条代码路径 |
| `bpmFactor` 是除不是乘 | 线 BPM = `nowBpm / bpmFactor` | 写单元测试钉死 |
| 层为空时字段存在性随版本变 | 早期 `null`，143 起无字段；全空则 `eventLayers` 不出现 | 解析时全部按"可选"处理 |
| 未知字段 | 生态里字段持续增加（170 加 `judgeArea`/`tint`） | 存进 `foreign` 袋，导出回填 |
| `hitsound` 被解析但未实现 | Phira 源码留 TODO（未复核） | 导入保留；导出前提示"目标可能忽略" |

### 6.2 音符类型映射（**本生态最贵的坑**）

| IR `kind` | Official | RPE | PEC | PhiCommonChart |
|---|---|---|---|---|
| `tap` | 1 | 1 | 1 | 0 |
| `drag` | **2** | **4** | **4** | **3** |
| `hold` | **3** | **2** | **2** | **1** |
| `flick` | **4** | **3** | **3** | **2** |

四套枚举互不相同，且**读错不会报错**——Official 的 Hold(3) 在 RPE 里是 Flick，Drag(2) 在 RPE 里是 Hold。⇒ 映射表必须是 codec 层的**单一数据源**，并配一张交叉单测矩阵（4 格式 × 4 类型）。

### 6.3 RPE 导出（语义等价）

- 目标版本可切换（至少 `1.4.x` 与 `1.7.0` 两档），因为 1.7 的流速缓动语义与新增字段不可混用。
- 导出流程：构建 RPE 文档 → 运行校验（第 7 节）→ 生成**保真度报告** → 写盘。
- 保真度报告必须逐项列出：目标客户端大概率忽略的字段（如 `hitsound`）、为通过校验而做的自动修补、以及字段级降级（如 `alpha` 超范围时的处理）。**v1 不含扩展字段，因此报告里不应出现"扩展被丢弃"这一类条目——一旦出现，说明有人在偷偷加扩展。**

### 6.4 `.pez` 打包

- 结构：zip 根目录直接放 `info.yml`（YAML 序列化的 ChartInfo）+ 谱面文件 + 资源。
- `info.yml` 的 `format` 字段决定载体（`rpe` / `pec` / `pbc`）；**Phira 忽略文件后缀名**，靠内容与 `info.yml` 推断。
- RPE 内嵌的 META 元数据不被 Phira 采信（**以 `info.yml` 为准**）⇒ 导出时两处都写，但以 `info.yml` 为准。

---

## 7. 校验层（生态空洞，可作差异化）

调研确认：**生态里不存在任何独立的谱面 linter / JSON Schema / 校验器**。而官谱格式有大量"不满足就静默停顿"的条件，因此校验器不是锦上添花，是刚需。

校验规则（来自官谱实测，导出 RPE 与生成官谱子集时适用）：

1. 事件列表按 `startTime` 升序；流速事件首个事件 `startTime = 0`；其它事件首个 `startTime` 为足够小的负数；事件**紧接不重叠**；末事件 `endTime` 为足够大的数。
2. 判定线数量 ≤ 100（超出会导致谱面停顿）。
3. 任何判定线 `bpm` > 0。
4. 任何事件列表不得为空数组。
5. 谱面运行时刻不得超出任一事件列表的末事件 `endTime`。
6. `holdTime`（取整后）为 0 的 Hold 不渲染；`speed = 0` 的 Hold 长度为 0。
7. 音符不可见阈值：`speed × currentFloorPosition > 3.3333336`。
8. 重复 JSON 键：**前者生效**（与 JS `JSON.parse` 不同）；数字支持科学计数法；`int` 字段强制截断（`"3.9Music"` → `3`）。
9. 性能护栏：音符加载近似 O(n²)（16384 → 0.25s，262144 → 159.75s），编辑器应设软上限并给出预估。

---

## 8. 测试策略

| 类型 | 内容 |
|---|---|
| 往返测试 | 官谱 / RPE 谱 → 导入 IR → 导出 RPE → 再导入，做结构 diff（允许等价重排，不允许语义差异） |
| 映射矩阵 | 4 格式 × 4 类型的枚举单测；坐标系三档（`formatVersion` 1 / 3 / 其它）换算单测 |
| 性质测试 | 有理拍约分与还原；缓动 29 种的插值采样；`bpmFactor` 除法；事件列表规范化的幂等性 |
| 模糊测试 | 针对实测怪癖：重复键、科学计数法、超范围 `alpha`、空事件数组、`bpm ≤ 0` |
| 行为对标 | 以 Phira 的 RPE 解析行为为参照（**只读源码学行为，不 vendor 代码**，见第 9 节） |
| Hold 透传 | Hold 既有语义（`holdTime`/`endTime`/`speed` = 尾速度而头速度恒为 1）导入导出**零改动**断言：字段值、排序、精度全等 |
| Hold 预览校准 | 第 4.5 节的实测实验脚本（固定 `holdTime`，扫 `note.speed` × 线 `speedEvent.value`，量 Hold 体长度与尾端到达时刻）——**只校准编辑器预览，不改格式语义** |

---

## 9. 许可证与合规（已定：OpenPhM = GPL-3.0-or-later）

### 9.1 与 `prpr`（GPL-3.0-only）的兼容性判定

**已直读上游核实（非转述）**：

| 项 | 事实 | 出处 |
|---|---|---|
| Phira workspace 许可 | `license = "GPL-3.0-only"` | `TeamFlos/phira` 根 `Cargo.toml` |
| `prpr` crate 许可 | `license = { workspace = true }` → **继承 GPL-3.0-only** | `prpr/Cargo.toml` |
| 文件头是否含 "or later" | **无**。`prpr/src/lib.rs` 只有 `pub mod` 声明，无任何许可证头 | `prpr/src/lib.rs` |
| 许可正文 | GPLv3 全文（§14「Revised Versions」原文：只有写明 "or any later version" 时才有后续版本选择权） | `TeamFlos/phira` `LICENSE` |

**判定：可以组合，但不能把"整体"声明成 `-or-later`。**

- 版本不冲突：两者同为 GPLv3，不存在 GPLv2-only × GPLv3 那类硬冲突。
- 但 GPLv3 **§5(c)** 要求：基于本程序的作品，**整体**必须按"本许可证"（GPLv3）授权给任何取得副本的人；而 **§14** 的后续版本选择权**只有著作权人写明了才存在**——`prpr` 没写。
- ⇒ **`prpr` 的部分永远拿不到 "or later" 授权**。你能对自己新增的文件用 §7 附加 "or later"（因为你拥有那部分版权），但**分发出去的那个组合作品整体，实际效力是 GPL-3.0-only**。
- **今天零实际影响**：GPLv4 尚不存在，"or later" 与 "only" 当下都落在 GPLv3。真正会咬人的时点是 FSF 发布 GPLv4 之后——那时链接 `prpr` 的部分不能自行升版，需要 Phira/`prpr` 著作权人另行授权。

**三条干净的处理方式（择一，别混着写）**

| 方案 | 做法 | 代价 |
|---|---|---|
| **A（推荐，最省心）** | OpenPhM 整体声明 **GPL-3.0-only** | 放弃"or later"的象征意义，但零歧义 |
| **B** | 自己的文件写 **GPL-3.0-or-later**，同时在 `README`/`NOTICE` 明确：**分发物含 GPL-3.0-only 组件（`prpr` 及其 workspace 同级 crate），组合作品整体按 GPLv3 生效，不含后续版本选择权** | 需要写清楚，否则是"过度授权"表述 |
| **C** | **不链接 `prpr`**，自研 RPE 解析（复刻的是**行为与格式语义**，不是代码） | 工作量最大，但"or later"全程干净 |

### 9.2 ⚠️ 真正的阻塞项：`sasa` 没有任何许可证

`prpr` 在**桌面/移动端**（`cfg(not(any(target_os = "android", target_env = "ohos")))`）**硬依赖** `sasa`（`git = Mivik/sasa`, rev `e76229b`，非 optional、无 feature 门控）⇒ 它会被静态链接进最终二进制。

而 `Mivik/sasa` 的完整文件树（经 jsDelivr 列全仓）：**只有 `.gitignore`、`Cargo.toml`、`src/**`**——

- **没有** `LICENSE` / `COPYING` / `LICENSE-MIT` / `LICENSE-APACHE`（逐个试过，全 404）；
- **没有** README；
- `Cargo.toml` 里**没有** `license` 字段，也**没有** `license-file` 字段。

⇒ **`sasa` 未授予任何许可，默认"保留所有权利"。** 后果是复合的：

1. Phira 自己能分发它，是因为其维护者与作者同源（Mivik）——**那份许可不延伸给你**。
2. 你一旦分发链接了它的二进制（或按 §6 提供 Corresponding Source 时连它的源码一起发），就是在无授权分发他人代码。
3. **不能靠"我只发二进制不发源码"绕过**：GPLv3 §6 要求提供 Corresponding Source，而链接进来的 `sasa` 不属于 §1 定义的 "System Library"。

**`prpr` 依赖树其余部分反而干净**（已抽查）：`prpr-macroquad`、`prpr-miniquad` 均声明 `MIT/Apache-2.0`；`symphonia` 为 MPL-2.0（与 GPL 兼容）；其余多为 MIT/Apache-2.0/ISC 系。**唯一的洞就是 `sasa`。**

**解决路径（三条，按代价排序）** —— **已选第 3 条（2026-09-27 裁决）**

1. ~~**向 Mivik / TeamFlos 索要许可**（最低成本）：请其为 `sasa` 补一个 `LICENSE`（MIT/Apache-2.0 即可）或书面确认。~~
2. ~~**把 `sasa` 从构建里换掉**：用 `[patch]` 指向自己写的空实现/自有音频后端。~~
3. ✅ **不链接 `prpr`（方案 C，已采用）**：自研 RPE 兼容层。顺带避免被拖入 `macroquad`/`miniquad`/`sasa`/`symphonia`/`jni`/`objc2` 这一整套窗口与音频栈——**对一个编辑器而言，为了约 46KB 的 `rpe.rs` + `pgr.rs` 解析逻辑去链接一个完整游戏渲染栈，本身就不划算。** 且因不链接任何 GPL-3.0-only 组件，`-or-later` 全程干净，第 9.1 节的"整体降级"问题不存在。

⇒ **自研的边界（重要）**：可以**读** `prpr` 源码学语义（读代码不产生副本），但**不得复制代码、注释、文档文本**。可复用的是**格式语义与行为**（事实与接口不受版权保护）；`phira-docs` 的文档与示例代码片段是 CC-BY-4.0，据此自行实现需**署名**。

### 9.3 GPL 合规清单（方案 C 下的实际口径）

- [x] 仓库根 `LICENSE`（GPLv3 全文）+ 各 `Cargo.toml` 的 SPDX 字段写 **`GPL-3.0-or-later`**（方案 C 下无冲突）。
      —— **2026-09-28 已落地**：`LICENSE` = GPL-3.0 标准文本（674 行 / 35147 字节，逐字节未改），
      `app/Cargo.toml` 的 `license = "GPL-3.0-or-later"`。
- [x] 每个源文件加 SPDX 头（`SPDX-License-Identifier: GPL-3.0-or-later`），避免"文件头与清单不一致"的歧义。
      —— **2026-09-28 已落地**：50 个 `.rs`/`.py` 都带 SPDX + 版权行（`// SPDX-License-Identifier: …`）。
- [x] `NOTICE`/`README` 列明第三方组件与**文档署名**：`phira-docs`（CC-BY-4.0）是 RPE 语义的主要依据，**必须署名**。
      —— **2026-09-28 已落地**：README 的「许可证 → 第三方与出处」列了 `phira-docs`（CC-BY-4.0）、
      `Lchzh Docs`、`TeamFlos/phira`（只作行为参考）、思源黑体（OFL-1.1，许可原文已随字体入库）、Rust 依赖。
- [ ] **Corresponding Source**：自己发布二进制时，源码即本仓库；第三方 crate 走 `cargo vendor`（或等价手段）随附，确保可复现。
- [ ] CI 加**许可证门禁**（Rust 用 `cargo deny check licenses`；其他栈用等价工具，如 `license-checker`）。allow-list 至少含：MIT / Apache-2.0 / MPL-2.0 / ISC / BSD-* / Zlib / Unicode-3.0 / CC-BY-4.0（仅文档）/ GPL-3.0-or-later。**无许可证的 crate 会被判为 `unlicensed` 并直接拦住构建**——`sasa` 这类问题本该在这里就被发现；Phira 上游没有 `deny.toml`，别指望它替你把关。
- [ ] 若日后加入网络服务组件：GPLv3 **§13 明确允许**与 AGPLv3 组合，不必为此换证。

> 说明：以上是对许可证文本与上游文件的机械核对，不构成法律意见；涉及商业分发时请做正式审查。

### 9.4 其余上游许可（速查）

| 对象 | 许可证 | 能做什么 | 不能做什么 |
|---|---|---|---|
| [TeamFlos/phira](https://github.com/TeamFlos/phira)（含 `prpr`/`prpr-pbc`） | **GPL-3.0-only**（已直读核实） | **仅作行为参考**（读源码学语义，不复制代码）；已裁决不链接 | 不得复制代码/注释/文档；链接则组合作品无法整体标成 `-or-later`，且受 9.2 阻塞 |
| [TeamFlos/phira-docs](https://teamflos.github.io/phira-docs/) | **CC-BY-4.0** | 据此写自己的实现（**需署名**） | 不能当软件许可用来发布代码；文档内嵌示例代码片段同属 CC-BY-4.0 |
| [phichain](https://github.com/Ivan-1F/phichain)（官谱 ⇄ RPE CLI） | **LGPL-3.0** | 作对照与参考 | 抄代码前评估 LGPL 义务 |
| [NRLT_PhiCommonChart](https://github.com/NuanRMxi-Lazy-Team/NRLT_PhiCommonChart) | **CC-BY-4.0** | 可借鉴其**兼容等级机制**（自己实现一份） | 不宜采用其格式：缺 `tint`/`judgeArea`/Controls，事件无缓动（不可逆有损），规范已近停更 |
| `Mivik/sasa` | **无任何许可（保留所有权利）** | — | **不得分发**（见 9.2） |
| `Mivik/prpr-macroquad`、`Mivik/prpr-miniquad` | `MIT/Apache-2.0`（Cargo.toml 声明） | 可随分发，建议附许可证文本 | — |
| 生态内大量仓库 | `license: null` | — | **默认保留所有权利，不可合法复用**（如 `phasetida-core`） |

---

## 10. 待裁决（原三点 → 现一点）

1. **实现语言/技术栈**：未定。`spec/` 层的规范、映射表与校验器是语言无关的，选型不阻塞；但 codec 与编辑器骨架需要先定。候选：Rust（与生态同栈、便于日后发布 CLI）、TypeScript（编辑器 UI 与渲染栈成熟、`Kipphi` 有 MIT 先例）、其他。
2. **opm 是否对外分发**：若只是编辑器工程格式，schema 可以宽松；若要给第三方读，需要更严格的版本承诺与公开 schema（但**不要**因此把它包装成"社区标准"——bmson 的结局就在那里）。

**已裁决项（不再讨论）**

- ✅ **方案 C**：自研 RPE 兼容层，**不链接 `prpr`**（2026-09-27）。
- ✅ **格式定名 `opm`**，作为第一主力（2026-09-27）。
- ✅ **许可证 GPL-3.0-or-later**，方案 C 下全程干净（2026-09-27）。
- ✅ **变速 Hold**：播放器侧行为，不纳入格式（2026-09-27），见 4.5。

---

## 11. 来源

- **本仓库的规范性产物**（与本文档同级）：[`spec/opm-format.md`](./spec/opm-format.md)（opm v0.1 规范）、[`spec/note-types.json`](./spec/note-types.json)（四套音符类型映射）、[`spec/easing.json`](./spec/easing.json)（29 种缓动）、[`spec/check.py`](./spec/check.py)（可执行校验器）、[`spec/examples/`](./spec/examples/)（正/负样例）
- 玩法与格式事实：[`Phigros-规则速查.md`](./Phigros-规则速查.md)（同目录，含官方/RPE/PEC 字段表与实测行为）
- Phira 文档（RPE/PEC 规范、谱面标准、extra.json 扩展）：[teamflos.github.io/phira-docs](https://teamflos.github.io/phira-docs/chart-standard/chart-format/index.html)（CC-BY-4.0）
- 生态调研（许可证、活跃度、转换器清单、bmson/osu!lazer/Quaver/Etterna/StepMania 先例）：本会话调研代理报告，关键项均附 URL；**其中 Phira 源码内部结构（`RPENote` 字段、`use_rpe_170_speed`、`alpha: u16` 注释）为转述，本机未独立复核**
- 许可证判定（**本次直读上游文件核实**，非转述）：[Phira 根 `Cargo.toml`](https://raw.githubusercontent.com/TeamFlos/phira/main/Cargo.toml)（`license = "GPL-3.0-only"`）、[`prpr/Cargo.toml`](https://raw.githubusercontent.com/TeamFlos/phira/main/prpr/Cargo.toml)（`license = { workspace = true }`、`sasa` 为非 optional 的桌面端依赖）、[`prpr/src/lib.rs`](https://raw.githubusercontent.com/TeamFlos/phira/main/prpr/src/lib.rs)（无许可证文件头）、[Phira `LICENSE`](https://raw.githubusercontent.com/TeamFlos/phira/main/LICENSE)（GPLv3 全文，§5(c)/§14 原文）、[`Mivik/sasa` 完整文件树](https://data.jsdelivr.com/v1/package/gh/Mivik/sasa@master/flat)（无任何许可证文件）
- 先例出处：[Quaver `DetermineMinimumQuaVersion()`](https://github.com/Quaver/Quaver.API)、[osu! `LegacyBeatmapDecoder`](https://github.com/ppy/osu)、[bmson Extension tip](https://bmson-spec.readthedocs.io/)、[StepMania `Changelog_SSCformat.txt`](https://github.com/stepmania/stepmania)
- 文档署名义务：RPE 语义与字段表主要依据 [`phira-docs`](https://teamflos.github.io/phira-docs/chart-standard/chart-format/index.html)（**CC-BY-4.0，须署名**）；官谱格式依据 [`Lchzh Docs`](https://docs.lchzh.top/learning/phigros/) 与 [Phira Documents](https://teamflos.github.io/phira-docs/chart-standard/chart-format/phi/root.html)

---

## 12. 决策记录（Decision Log）

| # | 日期 | 决策 | 理由 | 影响 |
|---|---|---|---|---|
| D1 | 2026-09-27 | 项目定位：Phigros **制谱器**；自有格式 + 转换为 RPE；导入无损 / 导出语义等价 | 用户确认 | 决定 1、3 节 |
| D2 | 2026-09-27 | **变速 Hold 移出格式设计**，由播放器决定表现；编辑器只透传 Hold 既有字段 | 播放器侧行为，格式不该替播放器做决定 | v1 扩展面为零；4.5 |
| D3 | 2026-09-27 | 许可证 **GPL-3.0-or-later** | 用户指定 | 9.1；排除了"整体降级为 GPL-3.0-only"的表述 |
| D4 | 2026-09-27 | **方案 C：自研 RPE 兼容层，不链接 `prpr`** | `prpr` 桌面端硬依赖 `Mivik/sasa`，而该 crate **无任何许可证**；且为解析层背负整套游戏渲染/音频栈不划算；不链接则 `-or-later` 全程干净 | 9.2/9.3；架构见第 3 节 |
| D5 | 2026-09-27 | 格式定名 **`opm`**，作为第一主力；规范落在 `spec/`，语言无关 | 用户指定 | 全文改名；扩展前缀 `x-opm:` |
| D6 | 2026-09-27 | **编辑会话单向数据流**：EditCore 是唯一可写方，改完发细粒度 update 广播；GUI 只订阅、按话题定向重建，绝不私自改文档 | 用户指定（避免"多处各自更新"导致状态漂移） | 见 `OpenPhM-框架选型.md` §7.5；实现 `app/src/broadcast.rs` + `journal.rs` |
| D7 | 2026-09-27 | **判定线是父对象，音符是子对象**；实现顺序先"判定线+事件"、后"音符" | 用户指定。反过来做会得到"音符不知道自己在哪条线上"的模型，线一转就全错 | 见 `OpenPhM-框架选型.md` §7.6；视图模型 `app/src/state.rs`、求值 `app/src/perf.rs` |

**遗留待办**：实现语言选型（第 10 节）、Hold 体速度插值的实测校准（4.5 / `spec/opm-format.md` §10）、打包容器命名。
