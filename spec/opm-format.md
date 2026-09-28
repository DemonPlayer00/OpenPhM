# opm 谱面格式规范 v0.1（规范性文档）

> 状态：**草案**，实现前可改；一旦有谱面文件落盘即冻结 `formatVersion: 1` 的字段语义。
> 格式名：`opm`。本文件是**规范性**的；设计理由、生态调研与许可证判定见 [`../OpenPhM-格式设计提案.md`](../OpenPhM-格式设计提案.md)。
> 校验器：[`check.py`](./check.py)（仅标准库）——规范与校验器必须同步演进，改规范就改校验器。

---

## 1. 设计原则（不可协商）

1. **拍用精确有理数**，不用浮点：`{"n": 分子, "d": 分母}`，`d ≥ 1`。秒是派生量，不落盘。
2. **枚举用字符串**，不用整数。整数只存在于 codec 边界，且**按目标格式分别映射**（见 [`note-types.json`](./note-types.json)）。
3. **未知字段必须保留**：任何节点可带 `foreign` 对象，按来源格式名存放无法识别的原始字段，导出时回填。
4. **`formatVersion` 表达格式语义版本；`minClientCapability` 表达"这份谱面实际依赖的最低能力"**。不得用版本号表达特性有无。
5. **扩展不改 `formatVersion`**：扩展走 `x-opm:` 前缀字段 + `extensions` 数组声明（bmson 的扩展纪律）。
6. **永不删除字段、永不静默改变既有字段语义。**
7. **opm 是"规范化"形态**：事件轨道无空隙、无重叠，首事件从拍 0 或更早开始。补空隙/裁重叠是 **codec 导入时**的职责，不是校验器放行的理由。

---

## 2. 根结构

| 字段 | 类型 | 必需 | 说明 |
|---|---|---|---|
| `format` | string | ✅ | 恒为 `"opm"` |
| `formatVersion` | int | ✅ | 当前 `1` |
| `minClientCapability` | int | ✅ | 0~3，见第 7 节；由内容计算得出 |
| `extensions` | string[] | ✅ | 用到的 `x-opm:` 扩展名；无扩展时为空数组 |
| `meta` | object | ✅ | 见 2.1 |
| `bpmList` | Bpm[] | ✅ | 首元素 `startBeat` 必须为 0，`startBeat` 严格递增，`bpm > 0` |
| `judgeLines` | JudgeLine[] | ✅ | 判定线；导出官谱时不得超过 100 条 |

### 2.1 `meta`

| 字段 | 类型 | 必需 | 说明 |
|---|---|---|---|
| `name` / `composer` / `charter` / `illustrator` | string | ✅（可空串） | 谱面信息 |
| `difficulty` | enum | ✅ | `EZ` / `HD` / `IN` / `AT` / `SP` / `Legacy` |
| `level` | string | ✅ | 人类可读，如 `"IN 15"`；**定数是独立概念，不塞在这里** |
| `constant` | number \| null | ⭕ | 定数（精确到 0.1）；SP 谱为 `null` |
| `offsetMs` | int | ✅ | 谱面偏移，毫秒（正 = 谱面延后） |
| `audio` / `background` | string \| null | ✅ | 相对工程根目录的路径 |
| `id` | string | ⭕ | 工程标识，不参与游戏逻辑 |

### 2.2 `source`（可选，导入溯源）

| 字段 | 类型 | 说明 |
|---|---|---|
| `source.format` | enum | `rpe` / `phigros-official` / `pec` |
| `source.version` | string \| int | 来源声明的版本（**RPE 的 `RPEVersion` 不可信**，仅作记录） |
| `source.behaviorProfile` | string | 行为档位，如 `rpe-pre-1.7` / `rpe-1.7`（流速缓动语义有别） |

---

## 3. 单位与坐标系

| 量 | 单位 | 换算 |
|---|---|---|
| `laneX`（音符横向位置） | X 单位 | 1 X = 0.05625 × 屏宽（1920×1080 下 108 px） |
| 事件纵向值（`speed` 的 `value`、`yOffset` 等） | Y 单位 | 1 Y = 0.6 × 屏高（1920×1080 下 648 px） |
| 时间 | 拍 | `秒 = 拍 × 60 / bpm` |

- 判定线坐标系取 RPE 编辑器坐标系：**原点在屏幕中心，X ∈ [−675, 675]，Y ∈ [−450, 450]**（即窗口 1350×900，3:2）。理由：它是生态通用语，且无损（官谱坐标可精确换算入此空间）。
  - 这个矩形就是**窗口边界**（游戏画面），编辑器预览里会画出边框并把边界外压暗（`app/README.md` 有实测）。
  - **判定线长度不是格式字段**：RPE 的判定线对象只有 name/bpmFactor/zOrder/isCover/layers/notes，
    线画多长是编辑器与播放器的呈现约定。因此 opm 也把它留给编辑器设置（默认与窗口同宽 = 1350），不进格式 ——
    与"变速 hold 的语义交播放器"同一个判断：格式不替播放器/编辑器做呈现决定。
  - 对照：RPE 的 `alpha` 事件范围是 **0~255**（负数会连该线所有 Note 一起隐藏），音符 `alpha` 同为 0~255；
    opm 的线/音符 alpha 用 **0~1**，故 RPE 导入时须 ÷255（**已实现**：`app/src/codec/rpe.rs`；
    负 alpha 无对应表达，降为 0 并在保真度报告里说明）。
- 角度单位为度，与 RPE 一致。
- `bpmFactor` 的语义是**除**：`线 BPM = 谱面 BPM / bpmFactor`。`bpmFactor` 不得为 0。

---

## 4. 判定线 `JudgeLine`

| 字段 | 类型 | 必需 | 默认 | 说明 |
|---|---|---|---|---|
| `name` | string | ⭕ | `"Untitled"` | |
| `group` | int | ⭕ | 0 | 组 |
| `bpmFactor` | number | ✅ | 1.0 | 不得为 0 |
| `zOrder` | int | ⭕ | 0 | 图层，建议 ±100（越界给警告） |
| `isCover` | bool | ⭕ | true | 遮罩：判定线背面的音符不渲染 |
| `attachUI` | enum \| null | ⭕ | null | `pause`/`combonumber`/`combo`/`score`/`bar`/`name`/`level` |
| `isGif` | bool | ⭕ | false | 纹理是否为 GIF |
| `texture` | string \| null | ⭕ | null | 相对路径；null = 默认纹理 |
| `anchor` | [number, number] | ⭕ | [0.5, 0.5] | 纹理锚点，取值 0~1 |
| `father` | int | ⭕ | −1 | 父线索引；−1 = 无父线。**必须无环** |
| `inheritRotation` | bool | ⭕ | false | 与 RPE 一致：字段缺失按 `false` |
| `layers` | Layer[] | ✅ | — | 1~5 层 |
| `extended` | object | ⭕ | {} | 故事板事件，见 4.4 |
| `controls` | object | ⭕ | {} | 控制曲线，见 4.5 |
| `notes` | Note[] | ✅ | — | 该线上的音符 |
| `foreign` | object | ⭕ | {} | 未知字段保留袋 |

### 4.1 事件层 `Layer`

每层可含 5 条轨道，缺省即空数组：`moveX`、`moveY`、`rotate`、`alpha`、`speed`。

### 4.2 事件 `Event`

| 字段 | 类型 | 必需 | 说明 |
|---|---|---|---|
| `startBeat` / `endBeat` | Beat | ✅ | `endBeat > startBeat`（相等的事件按无效处理） |
| `startValue` / `endValue` | number \| [int,int,int] \| string | ✅ | 类型由轨道决定：`color` 为 RGB（0~255 整数），`text` 为字符串，其余为数字 |
| `easing` | enum | ⭕ | 缓动名，见 [`easing.json`](./easing.json)；缺省 `linear` |
| `bezier` | bool | ⭕ | 是否贝塞尔缓动（速度轨道不支持） |
| `bezierPoints` | [number ×4] | ⭕ | `bezier` 为 true 时必需 |
| `easingRange` | [number, number] | ⭕ | 缓动作用区间，0~1，默认 [0,1] |
| `linkGroup` | int | ⭕ | RPE 的 `linkgroup` |

**轨道不变量（opm 特有）**：同一轨道内按 `startBeat` 升序、**无空隙、无重叠**（`event[i].startBeat == event[i−1].endBeat`），首事件 `startBeat ≤ 0`，末事件 `endBeat ≥` 该谱最后一个音符的 `endBeat`。

### 4.3 音符 `Note`

| 字段 | 类型 | 必需 | 默认 | 说明 |
|---|---|---|---|---|
| `kind` | enum | ✅ | — | `tap` / `hold` / `drag` / `flick` |
| `startBeat` | Beat | ✅ | — | 判定时刻（Hold 为头） |
| `endBeat` | Beat | ⭕ | — | **仅 `hold` 使用**，必须 `> startBeat` |
| `laneX` | number | ✅ | — | 横向位置（X 单位） |
| `side` | enum | ⭕ | `above` | `above` / `below` |
| `isFake` | bool | ⭕ | false | 假音符：无判定、无特效、无音效、不计分、不计物量 |
| `alpha` | int | ⭕ | 255 | 0~255；**读入时不得截断**（RPE 实际存在 >255 的值） |
| `speed` | number | ⭕ | 1.0 | 流速倍率；**Hold 的头速度恒为 1，此值指 Hold 尾速度**（原样透传，见提案 4.5） |
| `widthScale` | number | ⭕ | 1.0 | 宽度倍率（RPE 的 `size` 实际只影响宽度） |
| `yOffset` | number | ⭕ | 0.0 | Y 偏移；实际偏移量为 `yOffset × speed` |
| `visibleTime` | number | ⭕ | 999999.0 | 可见时间（秒） |
| `judgeAreaScale` | number | ⭕ | 1.0 | 判定区宽度倍率（RPE 1.7.0 `judgeArea`） |
| `tint` | [int ×3] \| null | ⭕ | [255,255,255] | 顶点色乘法染色（RPE `tint` / 旧名 `color`） |
| `hitEffectTint` | [int ×3] \| null | ⭕ | null | 打击特效染色（RPE 1.7.0 `tintHitEffects`） |
| `hitsound` | string \| null | ⭕ | null | 自定义打击音路径；**目标播放器可能忽略** |
| `foreign` | object | ⭕ | {} | 未知字段保留袋 |

### 4.4 `extended`（故事板）

可选轨道：`color`、`scaleX`、`scaleY`、`text`、`gif`、`incline`。其中 `incline` 已被 RPE 弃用，仅保留只读回放；`paintEvents`（RPE 143 起废弃）不建模。

### 4.5 `controls`（控制曲线）

可选轨道：`pos`、`size`、`skew`、`y`、`alpha`。**关键帧轴是"距判定线的纵向距离"，不是时间**：

| 字段 | 类型 | 说明 |
|---|---|---|
| `atDistance` | number | 距判定线的纵向距离（Y 单位） |
| `value` | number | 该控制项在此距离的值 |
| `easing` | enum | 到下一关键帧的缓动 |

- 统一用 `value`，**不沿用 RPE 的 `alpha`/`size`/`pos` 三个不同键名**（映射见 codec 表）。
- 语义（RPE 原义）：`alpha` 与音符 `alpha` **相乘**（`noteAlpha × nowAlpha`）。
- **这些轨道对 `hold` 无效**（RPE 原义），校验器给出警告。

---

## 5. 枚举

| 枚举 | 取值 | 单一数据源 |
|---|---|---|
| 音符类型 | `tap` / `hold` / `drag` / `flick` | [`note-types.json`](./note-types.json) |
| 缓动 | 29 种具名缓动 | [`easing.json`](./easing.json) |
| 正反面 | `above` / `below` | 本文件 4.3 |
| 难度 | `EZ` / `HD` / `IN` / `AT` / `SP` / `Legacy` | 本文件 2.1 |
| UI 绑定 | `pause` / `combonumber` / `combo` / `score` / `bar` / `name` / `level` | 本文件 4 |

---

## 6. 打包

| 形态 | 用途 | 说明 |
|---|---|---|
| `*.opm.json` | **编辑器工程文件（真源）** | 纯 JSON，可 diff、可入版本库 |
| `*.opm` | **单文件分发（已实现）** | **ZIP**：根目录 `opm.json`（与 `.opm.json` 完全同构）+ 资源。资源条目名 = `meta.audio` / `meta.background` 的文件名；未建模的条目**原样保留** |

资源路径一律相对工程根目录。**不把音频/图片内嵌进 JSON**（与 `.nrc` 的内嵌思路相反：编辑器要的是可 diff）；
要"一个文件带走全部资源"就用 `.opm` 容器（ZIP），谱面本身仍是同一份 JSON。

**容器细则**（实现：`app/src/codec/container.rs` + `app/src/zip.rs`）：
- 打包**优先系统 `7z`**（`-tzip`，**两遍**：谱面/文本 Deflate、音乐/图片 Copy 直存；
  `-mtm/-mta/-mtc=off` 保证**同样输入同样字节**）；没装 7z 时用内置实现（全 STORE）。
- 解包**先内置**（纯内存，覆盖 STORE/DEFLATE），遇到 ZIP64/特殊方法再交给 7z。
- 保存容器时把 `meta.audio`/`meta.background` 里的外部路径**改写成包内相对名**（可撤销的 `setMeta`），
  并把对应文件读进包；读不到的只**报警告**，不静默产出缺音乐的包。
- 载入容器时资源摊到 `$XDG_CACHE_HOME/OpenPhM/assets/<hash>/`（播放器按路径工作）；条目名只取文件名，
  `../` 之类不会写到缓存目录之外。

---

## 7. 能力等级

| 等级 | 名称 | 含义 |
|---|---|---|
| 0 | `official` | 仅官谱可表达的子集 |
| 1 | `rpe-base` | RPE 基础（判定线事件、音符基础字段） |
| 2 | `rpe-1.7` | RPE 1.7.0 全量（`tint`/`judgeArea`/controls/bezier/1.7 流速缓动语义）——**v1 的实际上限** |
| 3 | `opm-ext` | 含 `x-opm:` 扩展。**v1 无任何扩展，故恒不出现** |

- `minClientCapability` 由内容计算：出现 `judgeAreaScale` ≠ 1 或 `tint` ≠ 白 等 → 至少 2；出现扩展 → 3。
- 读取方遇到高于自身能力的谱面应**明确拒绝并说明**，而不是降级渲染（Quaver 的做法）。

---

## 8. 规范性约束（校验器实现的规则）

**错误（拒绝载入）**

1. `format != "opm"`；`formatVersion` 非正整数。
2. `bpmList` 为空、首元素 `startBeat` ≠ 0、`startBeat` 非严格递增、`bpm ≤ 0`。
3. `bpmFactor == 0`。
4. 轨道有空隙或重叠；首事件 `startBeat > 0`。
5. **任一轨道末事件 `endBeat` < 谱面最后一个音符的 `endBeat`**（官谱语义下会导致谱面停顿）。
6. 事件 `endBeat ≤ startBeat`；`bearer` 为 true 而 `bezierPoints` 长度 ≠ 4。
7. 缓动名不在 `easing.json` 中。
8. 音符 `kind` 不在枚举内；`hold` 缺 `endBeat` 或 `endBeat ≤ startBeat`；非 `hold` 携带 `endBeat`。
9. `father` 越界或形成**环**。
10. `extensions` 非空但 `minClientCapability < 3`；扩展名不以 `x-opm:` 开头。

**警告（可载入，但需报告）**

1. 判定线数 > 100（导出官谱会停顿）。
2. `zOrder` 超出 ±100。
3. `alpha` > 255（RPE 实际数据存在，不得截断，但要提示）。
4. `laneX` 超出 ±675；`yOffset` 超出合理范围。
5. `controls` 轨道作用于 `hold`（RPE 原义无效）。
6. `speed == 0` 的 `hold`（长度为 0，不渲染）。
7. `hitsound` 非空（目标播放器可能忽略）。
8. 音符合计（含假音符）超过软上限（性能护栏，官谱实测加载近似 O(n²)）。

---

## 9. 与 RPE 的映射

映射表与逐字段对照由 codec 层维护，单一数据源为 [`note-types.json`](./note-types.json) 与 [`easing.json`](./easing.json)；导入侧必须处理：

- 四套音符类型枚举互不相同（本生态最贵的坑）；
- `META.RPEVersion` 不可信（恒写 `150`/`160`）；
- `color` / `tint` 双名共存；
- `alpha` 越界不截断；
- 层为空时字段存在性随版本变；
- `bpmFactor` 是除不是乘；
- 事件轨道的补空隙（解析延拓）与裁重叠。

**时间三元组的语义**（实测确证，2026-09-27）：`startTime`/`endTime` 的 `int[3]` 是
`[整拍, 分子, 分母]`，即 `beat = b[0] + b[1]/b[2]` —— **不是** `[小节, 拍, 分母]`。
依据：3 份真实谱面（PhiZone，RPEVersion 113/141）共 2591 个音符时间 **100% 满足 `b1 < b2`**；
按前者解释谱面时长 112/160/146 秒（合理），按后者 449/639/584 秒（不合理）。
导出侧因此也写三元组：真实数据里分母出现 3/6/20/24，`37+1/3` 走浮点会退化成 `37333333/1000000`。

导出侧：目标 RPE 版本档位可切换，且**必须产出保真度报告**。

---

## 10. 未决问题

1. **Hold 体的速度插值方式**（端点线性 vs 刚性平移）——文档查不到，需实测；在实测前，编辑器预览须标注"长度显示未经实测校准"。
2. ~~打包容器命名与是否内嵌资源~~ **已定**：`.opm` = ZIP（根 `opm.json` + 资源），裸 `.opm.json` 继续支持。
3. opm 是否对外分发（决定 schema 承诺强度）。
4. 实现语言/技术栈（未定）。
