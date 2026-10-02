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
| `formatVersion` | int | ✅ | 当前 `2`；见 2.3（版本与迁移） |
| `minClientCapability` | int | ✅ | 0~3，见第 7 节；由内容计算得出 |
| `extensions` | string[] | ✅ | 用到的 `x-opm:` 扩展名；无扩展时为空数组 |
| `meta` | object | ✅ | 见 2.1 |
| `bpmList` | Bpm[] | ✅ | 首元素 `startBeat` 必须为 0，`startBeat` 严格递增，`bpm > 0` |
| `judgeLines` | JudgeLine[] | ✅ | 判定线；导出官谱时不得超过 100 条 |
| `maskZones` | MaskZone[] | ⭕ | **遮蔽区**（游戏里的「躁域」），见 4.6。**空数组不写进文件**（没有遮蔽区的谱面与"加这个字段之前"逐字节相同） |

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

> **「必需（可空）」是字面意思：键必须在，值可以是 `null`。** "这份谱面没有音乐"写成
> `"audio": null`，**不是把键省掉** —— 少了键就有两种表达同一件事的写法，而读取方只会按其中
> 一种理解。序列化器因此**永远写出这两个键**（`app/src/doc.rs` 的 `Meta`，2026-10-03 前带
> `skip_serializing_if`，存一次就把 `"background": null` 抹掉），`check.py` 也照此要求。
> `constant` 不受这条约束：它是 ⭕，**缺席有语义**（键不在 = 没填，`null` = SP 谱）。

### 2.3 `formatVersion` 与迁移

| 版本 | 线 `alpha` 轨量纲 | 说明 |
|---|---|---|
| `1` | **0~1** | 首个冻结版本 |
| `2` | **0~255** | **当前**。线 alpha 改成与 RPE、与音符 `alpha` 同量纲（用户口径 2026-10-03） |

**v2 只改了这一处语义**，别的字段一个没动。理由不只是"整齐"：RPE 的 alpha 本来就是 0~255 的
**整数**，存成 0~1 的浮点等于每次往返都做一次**有损除法**（`200/255` 不是二进制精确值）；
而同量纲之后线 alpha(0~1) 与音符 alpha(0~255) 在同一个文档里是两套量纲，读代码的人得一直记着。

**载入 v1 文件时自动迁移**（唯一实现：`app/src/doc.rs` 的 `Document::migrate_to_current`）：

- 线 `alpha` 轨的每个值 **×255**，负值夹到 0（v1 时代的既有口径）；
- `formatVersion` 就地升为 `2`；
- **无损** —— v1 的值只可能来自"手写 `k/255`"或"RPE 整数 ÷255"，乘回去正好是原整数；
- **幂等** —— 靠版本号判断，迁移过的文档再读一次不会重复乘；
- 迁移是"读进来就发生"的，**保存即写 v2**；导入/载入路径会把它写进**保真度报告**
  （"文件被改过了"这件事不该只说给日志听）。

> 这正是第 1 节第 6 条"**永不静默改变既有字段语义**"的字面执行：改语义就要抬版本号，
> 并且给出迁移路径 —— 而不是让旧文件里的 `1.0`（不透明）在新代码里变成 `1/255`（几乎全透明）。

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
  - **alpha 统一用 0~255**（v2 起）：RPE 的 `alpha` 事件范围就是 **0~255**（**负数**会连该线所有
    Note 一起隐藏），音符 `alpha` 同为 0~255。**opm 与它同量纲** ⇒ RPE 往返是恒等映射
    （v1 时代线 alpha 是 0~1、要靠 ÷255/×255 换算，见 2.3）。负 alpha 无对应表达，
    降为 0 并在保真度报告里说明。
  - **判定线的 `alpha` 只作用于判定线本体，不影响它上面的音符。** 两边的 alpha 是**两件独立的事**：
    线的管线的可见性，音符的管音符自己。
    出处：RPE 的 `Alpha` 事件**正常范围 0~255** 控制的就是判定线自身的不透明度，"连这条线上的所有
    Note 一起隐藏"是**负数**那条分支的**附加**效果（Phira Documents「[普通事件](https://teamflos.github.io/phira-docs/chart-standard/chart-format/rpe/event.html)」；
    作者 cmdysj 自述该功能**废弃**但仍然有效）。opm 表达不了负数 ⇒ 也表达不了"连音符一起隐藏"
    ⇒ **opm 里音符的可见性只由它自己的 `alpha` 决定**，与所在线的 alpha 无关。
    编辑器的预览按这条口径渲染（2026-10-03 修正；此前音符会跟着线一起淡出，是把 RPE 那条废弃分支
    的附加效果错当成了正常语义）。
- 角度单位为度，与 RPE 一致。
- `bpmFactor` 的语义是**除**：`线 BPM = 谱面 BPM / bpmFactor`。`bpmFactor` 不得为 0。
- **遮蔽区**（4.6）的顶点坐标**用同一套坐标系**（X ∈ ±675、Y ∈ ±450，Y 向上），但它**不属于任何判定线**：
  区域是屏幕空间的，不跟着线的移动/旋转/缩放走。

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

**轨道缺席时的表演状态**（唯一实现：`app/src/perf.rs` 的 `LinePerf::default` / `perf_of`）。
整条轨道空着是**合法且常见**的写法 —— "这条线不动"不必写一条常量事件出来。此时该量取：

| 轨道 | 缺席时的值 | 备注 |
|---|---|---|
| `moveX` / `moveY` | `0` | |
| `rotate` | `0` | |
| `alpha` | `255` | **不是 0**：缺席的线是**不透明**的（写 `0` 才是不显示）。**只管这条线本体**，不碰它的音符（§3） |
| `speed` | **`10`** | ← **最容易踩的一个**：RPE 的流速基准是 10，不是 1 |

> `speed` 那一格值得单独说：它是**倍率**，`1` 表示"以基准速度的十分之一下落"。谱面里写
> `"speed": 1` 不会报错（校验器不管数值大小），表现却是音符爬着掉 —— 这类"语法对、观感全错"
> 的坑校验器抓不住，只能靠这里写清楚。编辑器的新文档写 `10`，`spec/examples/minimal.opm.json`
> 也写 `10`。
>
> 求值时 `alpha` 会夹到 **`0~255`**（v2 量纲）；其余四条不夹。

### 4.2 事件 `Event`

| 字段 | 类型 | 必需 | 说明 |
|---|---|---|---|
| `startBeat` / `endBeat` | Beat | ✅ | `endBeat > startBeat`（相等的事件按无效处理） |
| `startValue` / `endValue` | number \| [int,int,int] \| string | ✅ | 类型由轨道决定：`color` 为 RGB（0~255 整数），`text` 为字符串，其余为数字。**`alpha` 轨的数值是 0~255**（§2.3）；校验器对它只给警告（越界表现为"线看不见了"，不是读不动文件） |
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
| `alpha` | int | ⭕ | 255 | 0~255；**读入时不得截断**（RPE 实际存在 >255 的值）。**音符的可见性只看它**，与所在线的 `alpha` 无关（§3） |
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

### 4.6 遮蔽区 `MaskZone`（游戏里的「躁域」）

> 这是 **opm 独有**的对象：实测官方谱面格式与 RPE 的字段里**都没有**它
> （调研记录见 `OpenPhM-格式设计提案.md`；2026-10-02 复核了 Phira 文档 / prpr / sim-phi / lchzh 文档），所以它没有 RPE 映射 —— 导出 pez 时**必须丢弃并报告**。

一块**三角形**区域。屏幕空间，红，游戏里点进这块区域**无法与音符交互**。

| 字段 | 类型 | 必需 | 默认 | 说明 |
|---|---|---|---|---|
| `name` | string | ⭕ | `"遮蔽区"` | 只给人看（列表/检查器） |
| `x1` / `y1` | Event[] | ⭕ | `[]` | 第 1 个顶点的 X / Y（RPE 单位） |
| `x2` / `y2` | Event[] | ⭕ | `[]` | 第 2 个顶点 |
| `x3` / `y3` | Event[] | ⭕ | `[]` | 第 3 个顶点 |
| `active` | Event[] | ⭕ | `[]` | **外观开关**，值二值化（写 `true`/`false`，也认数字） |
| `foreign` | object | ⭕ | `{}` | 未知字段保留袋 |

七条通道的**顺序就是编辑区里的列序**（`doc::MASK_TRACKS` = `x1,y1,x2,y2,x3,y3,active`）。
没有第 8 条：以后要加通道时按第 1 节第 6 条（永不删除字段）**追加**即可，不预先挖坑。

**通道不变量（与判定线轨道刻意不同，别按那边的直觉读）**

- ✅ 允许**空隙**：空档里保持前一条事件的终值（"延续最后值"）；
- ✅ 允许**首事件晚于拍 0** —— "这块区域什么时候出现"就是靠它表达的；
- ✅ 允许**末事件早于谱面末尾**（之后一直保持终值）。⚠️ 但它**抬高了谱面末尾本身**
  （§8 第 5 条把遮蔽区通道计入 `chart_end`）—— 拖一块区域到很晚，会让**所有判定线轨道**
  都必须够到那里。这是有意的：区域还在动，谱面就没结束。
- ❌ 必须按 `startBeat` 升序（求值以二分为基础）；
- ❌ 同一条通道内**不得重叠**（`endBeat > startBeat` 同样适用）。

**出现/消失（与判定线最大的差别）**

三条**顶点**通道里，**至少有一条有一个"覆盖当前拍"的事件块**（`startBeat ≤ 当前拍 < endBeat`）时，
这块区域才存在；三条都没有块 ⇒ **整个区域不显示**。

**块的跨度就是这块区域存在的时段** —— 于是"这块躁域第 8~12 拍出现"就写成"一个 `[8,12)` 的块"。
判定线没有这个问题（线本体永远在），所以这条规则是遮蔽区独有的。
注意它与下面"延续最后值"**不是一回事**：值可以延续，**存在**不行。

- 某个顶点**两维都没有事件** ⇒ 该顶点按 `(0, 0)` 参与（"总保底都是 (0,0)"）；
  某一维在**空档里**（块之间）按"延续最后值"参与；
- 三条通道**都没有块覆盖当前拍** ⇒ 整块不显示（**不是**"只要曾经有过块就一直显示"）；
- `active` 与其它通道走**同一套插值**，只在对外的最后一步按 `≥ 0.5` 二值化；
  **没有 `active` 事件时为 `false`**；
- `active=false` ⇒ 纯色（α ≈ 0.18）；`active=true` ⇒ **更不透明**（α ≈ 0.62）+ 一层**更透明的网格线**
  （α ≈ 0.18，间距 56.25 RPE 单位、**方格纹理旋转 45°**）—— 即 `active` 那档整体更重，
  网格只是它上面"透出来的那部分"。α 是**观感取舍**（规范只钉住"active 更不透明"这个序）；
  播放时光标附近的柔光只作用于 `active` 那一档；
- **一个 `active` 事件块只能是一种状态**（用户口径 2026-10-02）：它的
  `startValue` 与 `endValue` 必须落在同一档（`false`→`true` 这种渐变**不合法**）。
  想中途换外观就放**两块**（各是一种状态、各自是常量）—— 于是外观是**分段切换**的，
  永远不会有"一格网格渐渐淡出"的中间态。
  写侧（`add_zone_event` / `set_zone_event`）与两个校验器共用 `doc_active_state` 这一份判据。

**新建遮蔽区**（`{"op":"add_zone"}`）会写 6 条常量事件 = 屏幕中央的正三角形，跨度是
**`[起点, 起点 + 1 拍]`**（种子块；"初始屏蔽区事件区间为 0~1 拍"）—— 想要更长就拖事件块的尾巴，
或者在别处再放块。`active` **不写事件**（于是新建出来是 `false` 那一档）。命令层的细节见 4.6.1。

**放一块的跨度规则**（编辑器手势、属性编辑器、命令层**共用一份实现**）：
起点与已有块的起点重合 ⇒ 不许放；终点**不越过下一块的起点**；缺省长度 = `MASK_EVENT_BEATS` = 1 拍
（命令行不带 `endBeat` 时）。落点在某一块**里面**是合法的插入 —— 前一块被裁到新起点，切点上的值不变。

### 4.6.1 遮蔽区相关命令（`app/src/core.rs`）

| 命令 | 说明 |
|---|---|
| `add_zone` | `{startBeat?, endBeat?, name?, empty?, set?}`；默认写中央正三角形；`empty:true` 建一个没有任何事件、**不显示**的区 |
| `del_zone` | `{index}`（`zone` 也认） |
| `set_zone` | `{zone, set:{name, active?}}`；`set.active` = **整区切档**：已有 `active` 块的时间跨度不动、值全部改写成这一档（于是"一块区一种状态"这件事有一个一键入口）；一条 `active` 块都没有时按**坐标事件的包络** `[最早起点, 最晚终点)` 写一块；一条坐标事件都没有的区拒绝（`active` 在那时没有意义） |
| `add_zone_event` | `{zone, track, startBeat, endBeat?, startValue?, endValue?, easing?}`；缺省值 = 该通道**此刻的值**；`active` 通道还要求头尾同档（否则报错）；缺省终点 = 起点 + 1 拍（`MASK_EVENT_BEATS`）**且不越过下一块**；**起点与已有块的起点重合、或显式终点越过下一块 ⇒ 报错**；插入时会把被压住的前一块**裁到新块起点**（切点值不变） |
| `set_zone_event` | `{zone, track, index, set:{startBeat?, endBeat?, startValue?, endValue?, easing?}}`；改完**与邻块重叠 ⇒ 报错**（通道不变量） |
| `del_zone_event` | `{zone, track, index}` |
| `resize_zone_event` | `{zone, track, index, edge:"start"\|"end", toBeat}` —— 只动这一个端点；与邻块重叠会被拒 |
| `move_zone_event` | `{zone, track, index, delta}` —— 整块平移；与邻块重叠会被拒 |

---

## 5. 枚举

| 枚举 | 取值 | 单一数据源 |
|---|---|---|
| 音符类型 | `tap` / `hold` / `drag` / `flick` | [`note-types.json`](./note-types.json) |
| 缓动 | 29 种具名缓动 | [`easing.json`](./easing.json) |
| 正反面 | `above` / `below` | 本文件 4.3 |
| 难度 | `EZ` / `HD` / `IN` / `AT` / `SP` / `Legacy` | 本文件 2.1 |
| UI 绑定 | `pause` / `combonumber` / `combo` / `score` / `bar` / `name` / `level` | 本文件 4 |
| 遮蔽区通道 | `x1` / `y1` / `x2` / `y2` / `x3` / `y3` / `active` | 本文件 4.6 |

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
| 4 | `opm-mask` | 含**遮蔽区**（4.6） |

- `minClientCapability` 由内容计算：出现 `judgeAreaScale` ≠ 1 或 `tint` ≠ 白 等 → 至少 2；出现扩展 → 3；
  出现**遮蔽区** → 至少 4（判据只有一处实现：`doc::capability_of`）。
- 读取方遇到高于自身能力的谱面应**明确拒绝并说明**，而不是降级渲染（Quaver 的做法）。
  遮蔽区尤其如此：默默忽略它，玩家看到的是一份"该挡的地方没挡"的谱面 —— 那是**错的呈现**，不是降级。

---

## 8. 规范性约束（校验器实现的规则）

有两份**独立实现**：`spec/check.py`（Python，只读入文件）与 `app/src/cmd.rs` 的 `validate`
（Rust，跑在编辑器里）。两者**必须在同一份文件上给出相同结论**（连指针与措辞都应当对齐）——
这条不变量曾经只是 `cmd.rs` 里的两句注释、没有任何东西在守，2026-10-03 才发现它们真的分家了
（见第 5 条的说明）。`spec/examples/` 就是用来核对的：

```sh
python3 spec/check.py spec/examples/*.json
for f in spec/examples/*.json; do opm-ctl --file "$f" validate; done
```

| 样例 | 期望 |
|---|---|
| `minimal.opm.json` / `mask.opm.json` | 两份都 **PASS（0 error 0 warning）** |
| `bad.opm.json` | 两份都 **FAIL**，逐条指针与措辞一致 |
| `two-lines.opm.json` | 两份都 **FAIL**，且都指到 `/judgeLines[1].layers[0].moveX` |

**错误（拒绝载入）**

1. `format != "opm"`；`formatVersion` 非正整数。
2. `bpmList` 为空、首元素 `startBeat` ≠ 0、`startBeat` 非严格递增、`bpm ≤ 0`。
3. `bpmFactor == 0`。
4. 轨道有空隙或重叠；首事件 `startBeat > 0`。
5. **任一轨道末事件 `endBeat` < 谱面末尾**（官谱语义下会导致谱面停顿）。
   **"谱面末尾" = 全文档 `max(音符的 endBeat, 五条基础轨事件的 endBeat, 遮蔽区七条通道的 endBeat)`，
   跨所有判定线与所有遮蔽区取最大**，唯一实现是 `app/src/doc.rs` 的 `Document::chart_end`
   （`cmd::validate`、`TimeMap` 的时长、`check.py` 的 `chart_end_of` 都以它为准）。
   三条容易搞错的边界：
   · 它是**全局**的 —— 多线谱面里"每线各自铺到自己最后一个音符"是**不够**的，
     B 线的轨道必须也铺到 A 线撑出来的那个末尾（2026-10-03 前 `check.py` 是逐线算的，
     于是这种谱面 Rust 报错、`check.py` 放行 —— 同一份文件两个答案，已收口）；
   · **遮蔽区的通道也计入**（一块区域的顶点表演常常跟在最后一个音符之后）。别与 §4.6 那条搞混：
     遮蔽区通道**自己**不受本条约束、**允许**早于谱末结束，但它的末事件会**抬高**这个末尾，
     从而抬高**判定线轨道**必须够到的地方 —— 加一块拖到很晚的遮蔽区，会让所有判定线轨道都不合格；
   · `extended` / `controls` 的事件**不参与**取值（`chart_end` 只扫五条基础轨与七条遮蔽区通道）。
     但 `extended` 轨**自身**仍受本条约束（它们是 §4.4 的事件轨，§4.2 的不变量照用）——
     这一半目前只有 `check.py` 在查，`cmd::validate` 还没走到那两个词典（见第 10 节第 8 条）。
6. 事件 `endBeat ≤ startBeat`；`bearer` 为 true 而 `bezierPoints` 长度 ≠ 4。
7. 缓动名不在 `easing.json` 中。
8. 音符 `kind` 不在枚举内；`hold` 缺 `endBeat` 或 `endBeat ≤ startBeat`；非 `hold` 携带 `endBeat`。
9. `father` 越界或形成**环**。
10. `extensions` 非空但 `minClientCapability < 3`；扩展名不以 `x-opm:` 开头。
11. 遮蔽区：通道未按 `startBeat` 升序 / 同通道内重叠 / `endBeat <= startBeat` / 缓动名不在表里。
12. `maskZones` 非空但 `minClientCapability < 4`。
13. 遮蔽区的 `active` 事件块**头尾不同档**（`false`→`true` 这类渐变）：一个块只能是一种状态，
    要换外观请分两块（§4.6）。

**警告（可载入，但需报告）**

1. 判定线数 > 100（导出官谱会停顿）。
2. `zOrder` 超出 ±100。
3. `alpha` > 255（RPE 实际数据存在，不得截断，但要提示）。
4. `laneX` 超出 ±675；`yOffset` 超出合理范围。
5. `controls` 轨道作用于 `hold`（RPE 原义无效）。
6. `speed == 0` 的 `hold`（长度为 0，不渲染）。
7. `hitsound` 非空（目标播放器可能忽略）。
8. 音符合计（含假音符）超过软上限（性能护栏，官谱实测加载近似 O(n²)）。

> **遮蔽区没有"轨道必须连续"这条警告**（见 4.6 的不变量）：稀疏是它的正常形态。

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

**遮蔽区没有 RPE 映射**：官方谱面格式与 RPE（1.4~1.7 全量字段）里都没有对应物
（2026-10-02 调研：Phira 文档、prpr 源码、sim-phi、lchzh 文档均无）。因此
**导出 pez 时必须丢弃，并在保真度报告里逐个数报出来**（"RPE 无法表达遮蔽区，已丢弃 N 个区 / M 条事件"）——
静默丢掉会让作者以为换个格式没损失。

导出侧：目标 RPE 版本档位可切换，且**必须产出保真度报告**。

---

## 10. 未决问题

1. **Hold 体的速度插值方式**（端点线性 vs 刚性平移）——文档查不到，需实测；在实测前，编辑器预览须标注"长度显示未经实测校准"。
2. ~~打包容器命名与是否内嵌资源~~ **已定**：`.opm` = ZIP（根 `opm.json` + 资源），裸 `.opm.json` 继续支持。
3. opm 是否对外分发（决定 schema 承诺强度）。
4. 实现语言/技术栈（未定）。
5. **遮蔽区的"点不进去"要不要在编辑器里模拟**：当前**不模拟**（编辑器里被盖住的音符照常可选可编 ——
   否则那块区域里的谱就没法编了）。"点不进去"是播放器的行为。
6. 遮蔽区通道的**重叠检测还没进冲突浏览器**（只有 `validate` 会报）：`cmd::overlap` 是按判定线建模的，
   而遮蔽区没有图层/线号。当前靠"写入侧的跨度规则"让它很难发生 —— `add_zone_event` 拒绝越界与起点重合、
   `set_zone_event` / `resize_zone_event` / `move_zone_event` 拒绝压到邻块（判定见 §4.6.1），
   编辑器的拖动还会在邻块边界处停住。
7. 遮蔽区编辑模式下的**事件块多选**（框选/Ctrl+点）还没做 —— 当前是单选。
8. **`extended` / `controls` 的结构校验只有一半**（2026-10-03 记）：`check.py` 会查它们的
   连续性/末事件/缓动/keyframe 轴序，而 `cmd::validate`（app 里那份）**完全不查** ——
   那两个词典落在 `JudgeLine.foreign` 袋里，没有被建模。方向是**危险的**那一侧：
   `check.py` 拒的文件 app 可能照收照渲染，于是"校验通过"与"能打开"之间没有蕴含关系。
   修法有两条，都还没做：① 把两个词典建模进 `doc.rs` 并让 `validate` 走同一套规则；
   ② 至少让 `validate` 把 `check.py` 已实现的几条照搬过去（`extended`/`controls` 的
   `endBeat > startBeat`、连续性、末事件 ≥ 谱面末尾、`easing` 合法性、`atDistance` 升序）。
   顺带：`extended`/`controls` **渲染**是另一条独立的功能线（当前只做原样往返，
   RPE 导入器把它记成一条保真度警告"`extended` 故事板特殊事件层"），别和上面这条混在一起。
9. **`speed` 轨缺席 = 10，这一条以前没有任何一处写下来**（2026-10-03 补进 §4.1）。
   校验器抓不住"语法对、观感全错"（写 `"speed": 1` 会通过校验，音符却爬着掉），
   所以它只能靠文档；如果以后要更狠，可以加一条**警告**：`speed` 常量轨的值 < 1 时提示。
