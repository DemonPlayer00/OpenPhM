# OpenPhM Agent 接口设计（`opm-ctl` 与命令语言）

> 面向"由 agent 驱动制谱"的入口设计。核心判断：**命令语言只有一套，两个前端**——
> 无头 CLI（`opm-ctl`，给 agent）与 GUI 控制台（给人），共用同一份 `cmd::Session` 与同一批命令。
> 这样人在界面上做的事与 agent 在脚本里做的事**语义完全一致**，不会出现"只有 GUI 能做"的操作。

---

## 1. Agent 需要什么（设计约束）

| 需求 | 设计对策 |
|---|---|
| **确定性** | 命令是 JSON，无隐式状态；同一输入必然同一输出（无随机、无时间依赖） |
| **可脚本化** | 一次调用可喂一批命令（`--cmd` 多次 / `--script` JSONL / `--stdin`），批量原子生效后再落盘 |
| **失败可回滚** | 单条命令失败即回滚该条（记录更改模式：类型化逆操作），文档不留半成品；退出码区分"命令失败"与"校验失败" |
| **能核对结果** | `validate` 返回结构化问题清单（带 JSON 指针）；`summary` 给轻量状态；`dump` 给全文 |
| **能"看见"改动** | `render` 无头出 PNG —— agent 读图即可判断音符位置/长条长度是否符合预期 |
| **不用猜约定** | 时间用**有理拍**、坐标用 **RPE 单位**、枚举用**字符串**，全部写在下面 |
| **不必手写不变量** | `opm` 要求事件轨道无空隙无重叠；提供 `set_track_constant` / `normalize` 一步满足，而不是让 agent 逐条拼事件 |

---

## 2. 两个前端，一套语言

```
                    ┌──────────────────────────────┐
   agent ──CLI──▶   │  cmd::Session                │   ◀──GUI 控制台── 人
   (opm-ctl)        │  · 命令解释 / 撤销栈 / 校验   │      (调试工作区)
                    │  · 与渲染无关                │
                    └──────────────┬───────────────┘
                                   ▼
                         opm 文档模型 (doc.rs)
                                   ▼
                    演奏区渲染 / 无头出图 / 落盘 JSON
```

- **CLI（agent）**：`opm-ctl --file chart.opm.json --script edits.jsonl --save`
- **GUI 控制台（人）**：`调试` 工作区底部面板，粘贴同一段 JSONL，点"执行"

---

### 判定线是父对象：`line` 参数的含义

模型是 `Document → judgeLines[] → {layers[]→事件轨道, notes[]}`。**音符是判定线的子对象**：
它只存"线本地坐标"（`laneX`），屏幕位置由父线在该时刻的表演（五条轨道求值）决定。
所以命令里凡是有 `line` 的地方，指的都是这个父对象：

```sh
# 在 2 号线上加一个音符（它跟着 2 号线移动/旋转）
opm-ctl --file F --cmd '{"op":"add_note","line":2,"kind":"tap","startBeat":[8,1],"laneX":-120}'
# 给 2 号线加旋转事件（0→90°，outCubic）—— 该线的子音符会一起转
opm-ctl --file F --cmd '{"op":"add_event","line":2,"layer":0,"track":"rotate","startBeat":[0,1],"endBeat":[32,1],"startValue":0,"endValue":90,"easing":"outCubic"}'
# 核对：此刻每条线的表演值 + 每条轨道的事件数与取值
opm-ctl --file F lines --at 5.333
```

`lines` 输出形如：

```json
{"playheadSec":5.333,"playheadBeat":16.0,"bpmSegments":1,"duration":57.3,
 "lines":[{"index":1,"name":"L1","zOrder":1,"isCover":true,"bpmFactor":1.0,"notes":100,
           "perf":{"moveX":0.0,"moveY":250.0,"rotate":78.75,"alpha":0.8,"speed":8.0},
           "tracks":[{"track":"rotate","events":2,"valueAt":78.75}, …]}]}
```

注意 `rotate: 78.75` 而不是 45：事件用的是 `outCubic`，缓动**真的**参与求值。

### 连续编辑（拖拽/批量）的正确姿势

一次连续编辑要"过程可见、撤销一步"，用**事务**包住：

```sh
# GUI 里拖动一个音符时发的就是这套（实测：3 帧 = 3 条广播、撤销 +1 步）
opm-ctl --attach auto --cmd '{"op":"begin","label":"拖动音符"}'
opm-ctl --attach auto --cmd '{"op":"set_note","line":0,"index":0,"set":{"startBeat":[9,4],"laneX":84.375}}'
opm-ctl --attach auto --cmd '{"op":"set_note","line":0,"index":0,"set":{"startBeat":[11,4],"laneX":168.75}}'
opm-ctl --attach auto --cmd '{"op":"commit"}'
opm-ctl --attach auto --cmd '{"op":"undo"}'      # 一步回到拖拽前
```

事务内每条改动**都会广播**（订阅者实时更新），撤销只占**一步**；`abort` 会回滚文档。
拍建议写成有理数 `[n,d]`（`1/4` 网格就是 `[k,4]`），避免浮点误差写进文档。

**GUI 的多选编辑就是这套事务的另一个调用方**（逻辑在库内 `opm_app::edit`，可单测、可复用）：

| 库内函数 | 干什么 |
|---|---|
| `edit::GrabIntent` + `edit::grab_selection` | 按下那一刻**冻结**选区里每一条的原点（成员、原点、手指位置）。拖拽期间文档每帧都在变，从文档反推"原来在哪"会让位移逐帧累积 |
| `edit::grab_delta` | 一次拖动的位移：锚先吸附到网格，再夹住（负拍 / 可见窗口；事件还要过"最近合法位置"） |
| `edit::move_grab_commands` | 位移 → 命令序列（音符 `set_note`、事件 `set_event`，都用冻结原点算**绝对**位置） |
| `edit::nearest_free_delta` | 事件整块平移的**最近合法位置**（kdenlive 式）：请求的位移若与未选中的邻居重叠，就退到补集里离请求最近的点 —— 拖得够远就越过障碍落到空档里；铺满的轨道上就是原地 |
| `edit::event_items_overlap` / `edit::event_drag_disabled` | **卷进重叠的事件禁止移动**的判据（两两比较，相接不算重叠） |
| `edit::delete_selection_commands` | **Del** 的一整批：`begin` + 删除 + `commit`（一个撤销步）。**同一张表内按下标降序发** —— 顺序错了会删错东西 |

想从脚本/agent 复现"多选 + 整组平移"，等价写法是 `{"op":"select","notes":[…]}` 之后
逐帧发 `set_note`（本编辑器 GUI 走的就是这个）。

### 播放控制（视图命令，不进 EditCore）

播放头/播放状态是**视图状态**，不属于文档、也不该进撤销栈 —— 所以它们不走 EditCore，而是走独立队列：

```sh
opm-ctl --attach auto --cmd '{"op":"play"}'
opm-ctl --attach auto --cmd '{"op":"seek","beat":48}'      # 也可以 {"op":"seek","to":16.0}
opm-ctl --attach auto --cmd '{"op":"pause"}'
opm-ctl --attach auto --cmd '{"op":"audio","path":"song.wav"}'   # 运行中换音频
opm-ctl --attach auto --cmd '{"op":"view"}' --json | tail -1     # 观察效果
```

| 命令 | 说明 |
|---|---|
| `{"op":"play"}` / `{"op":"pause"}` / `{"op":"toggle_play"}` | 播放控制（与 GUI 里按**空格**同一条路径） |
| `{"op":"seek","to":SEC}` / `{"op":"seek","beat":B}` | 定位；按拍定位会用 `bpmList` 换算成秒 |
| `{"op":"nudge","beats":N}` | **相对**挪动播放头 N 拍（编辑区滚轮就是它的一个触发器）；负数往前 |
| `{"op":"zoom","beats":B}` / `{"op":"zoom","factor":F}` | **缩放时间轴**（编辑区 Ctrl+滚轮的同一条路径）：设绝对可见拍数，或乘一个倍率；夹在 4～256 拍。纯视图状态，**不进文档** |
| `{"op":"load","path":"FILE.json"}` | **打开谱面**（opm 或 RPE，**按内容判格式**）。整体替换当前文档：撤销栈清空、按全量话题广播；返回 `format`/`lines`/`notes`/`fidelity` |
| `{"op":"save"}` | 保存到当前路径。**写回载入时的格式**（RPE 进 RPE 出） |
| `{"op":"save","path":"F","format":"auto\|opm\|opm-bare\|rpe"}` | 另存为。`auto` 按扩展名：`*.opm` → **容器**、`*.opm.json` → 裸 opm、其余 `*.json` → RPE。返回 `fidelity` |
| `{"op":"window","offsetX":X}` | **音符区窗口 X 偏移**（顶栏同一条路径）：音符区显示 `[X−675, X+675]` 的 laneX 区间，X 夹 ±675。用于查看/编辑**官方窗口外**的音符。纯视图状态，**不进文档**；读 `ui_stats.window_offset_x` |
| `{"op":"audio","path":"FILE"}` | **只换预览用的音频**（视图命令，不改文档里的 `meta.audio`） |
| `{"op":"audio_offset","ms":F}` | 手动校准偏移（听到的与游标算出来的差多少） |
| `{"op":"view"}` 里的 `conflicts` | 当前**事件重叠**处数（加载时全量检测、之后每次改动增量检测） |
| `{"op":"select","line":L,"track":"alpha","note":N,"event":M}` | **选中**（视图状态）：把界面指到某个对象，便于截图/检查 |
| `{"op":"select","notes":[0,2,5]}` / `{"op":"select","events":[["alpha",0],["moveX",3]]}` | **多选**（视图状态，整批替换选区）：音符用**视图下标**（该线内按时间序），事件用 `[轨道名, 该轨道合并视图里的下标]`。认不出的轨道名会被丢掉（不整条命令失败）。**选区同时只有一类**（音符 xor 事件），两个都给时以 `notes` 为准 |
| `{"op":"view"}` 里的 `window_offset_x` | 当前音符区窗口 X 偏移（0 = 显示官方窗口 ±675） |
| `{"op":"view"}` | `ui_stats` 的别名：读 `playing` / `playhead_sec` / `playhead_beat` / `audio_*` / `overlay_*` |

### 保真度报告（`fidelity`）

每次导入/导出都带：

```json
{"source":"rpe","version":"RPEVersion=113（此值不可信，仅作记录）","lossless":false,
 "conversions":["/judgeLineList[0].eventLayers[0].moveXEvents：37 处空隙按「前值延拓」补齐（RPE 原义）"],
 "warnings":["`extended` 故事板特殊事件层：共 17 处（首次于 /judgeLineList[0]）—— opm v1 未建模，已原样保留"]}
```

`lossless:false` 不等于"打开失败"，而是"有字段没被建模/被降级"——**读它再决定要不要人工核对**。
`opm-ctl convert` 在 `lossless:false` 时退出码为 **1**（用法错误是 2、校验 ERROR 是 3）。

### 无头/agent 注意

GUI 的「打开…/另存为…」走的是**系统文件对话框**（kdialog/zenity），它会弹在用户的桌面上 ——
agent 不要通过 GUI 按钮做文件操作，直接用控制通道的 `{"op":"load"}` / `{"op":"save","path":…}`（不弹窗）。
`opm-app --file-dialog` 可以让"文件"对话框在启动时摊开，仅供截图/人工检查。

### 载入/保存的形态（**按内容判，不看扩展名**）

| 输入 | 判据 | 说明 |
|---|---|---|
| opm 容器 `.opm` | ZIP 魔数，里面有 `opm.json` | 谱面 + 音乐 + 曲绘，一个文件带走 |
| RPE 谱面包 `.pez` | ZIP 魔数，里面有 `info.yml` | Phira 标准：`info.yml` + `chart.json` + 资源 |
| opm 无压缩文件夹 | 目录里有 `opm.json` | 里面是**平的**（资源放在同一层） |
| RPE 无压缩文件夹 | 目录里有 `info.yml` | 同上 |
| 裸 opm / RPE JSON | 既不是 ZIP 也不是目录 | `format:"opm"` / `judgeLineList`·`BPMList` |

**这五种 `opm-ctl --file X` 与 GUI「打开」都吃**（实现只有一份：`EditCore::stage_file`）。

写：`--to opm`（容器）/ `opm-dir`（无压缩文件夹）/ `rpe`（`.pez`）/ `rpe-dir`；四者**装卸对称**
（写得出就读得回，有测试钉住）。**容器优先用系统 `7z` 打包**（谱面 Deflate、媒体 Copy 直存、字节确定），
没有 7z 时用内置实现；有降级时 `convert` 退出码仍为 1。单文件 JSON 不再是保存形态（老的仍写得回去）。

agent 建议：**改谱面用裸 `.opm.json`**（可 diff、可读、无二进制），交付时再 `convert` 成 `.opm` 容器。

### 解压缓存与"别踩别人的会话"（`opm-ctl` 必读）

一次载入会把容器内容摊到 `<临时目录>/opm/<内容 hash>/`（Linux `/tmp/opm`、Windows `%TEMP%\opm`），
因为 `meta.audio` 里写的是**包内文件名**，只有落成真实文件"按路径装载音乐"才找得到它。
GUI 的每次会话至多留一份、正常退出即清；`opm-ctl` 的目录按设计留着（上限 512 MB，超出按 mtime 修剪）。

**`opm-ctl` 读一个包时走 `CacheClaim::ReadOnly`：别人（例如 GUI 的某个会话）已经认领的缓存目录
一个字节都不碰** —— 包括那份可能带着未保存改动的 `opm.json` 快照与 `session.json`。
（不是洁癖：一次 `opm-ctl --file X dump` 就能把 GUI 崩溃留下的快照抹成容器里的旧内容，实测过。）

GUI 侧另有一条硬约束：**同一时刻只允许一个会话**（缓存根目录上一把 `File::try_lock` 独占锁）。
抢不到的实例不会碰任何文件，只开一个关不掉的模态说明"谁在跑"。所以脚本里要让 GUI 退出，
别起第二个实例去抢 —— 用控制通道，或让用户关窗（有未保存改动时会问保存/不保存/返回）。

### RPE 支持范围

- 根：`BPMList`（**读**：时间三元组或浮点都吃；**写**：默认三元组 `[整拍,分子,分母]`，只有
  `RpeTarget{triple_time:false}` 才写浮点）、`META`（`offset` 是**毫秒**；`song`→`audio`、
  `illustration`→`illustrator`；`RPEVersion` 只作记录并保留）、`judgeLineList`；
  编辑器辅助字段（`chartTime`/`judgeLineGroup`/`multiLineString`/`multiScale`/`timeTags`/`xybind`
  与判定线的 `father`/`rotateWithFather`/`Texture`/`Group`）**来源里有就原样写回**；
  只有 `judgeLineGroup`/`multiLineString`/`multiScale`（真实谱面 12/12 都有）在**来源缺失时**补默认值，
  `chartTime`/`timeTags`/`xybind`/`rotateWithFather` 缺了就不写（真实谱面本来就常缺）。
- 音符：`type` 走 `spec/note-types.json`（**RPE 2=Hold、3=Flick**，与官谱相反）、
  `alpha` 0~255（**>255 不截断**）、`above`、`isFake`、`speed`、`size`→`widthScale`、`yOffset`、`judgeArea`。
- 事件与时间：5 条轨道 + 29 种 `easingType`（表来自 `spec/easing.json`）、`bezier`/`bezierPoints`；
  **音符、事件、BPMList 的时间一律写整数三元组** `[整拍,分子,分母]`（`beat = b0 + b1/b2`）。
  实测 12 份真实谱面（RPEVersion 140/160/170）共 **269033 处时间全是三元组、0 个浮点**，
  其中 **10.68% 的分母含非 2 因子**（3/5/6/7/12/25/48/1000/3000…）—— 二进制浮点根本表示不了，
  所以三元组不是"可选写法"而是必需；而**数值**（`positionX`/`size`/`speed`/事件 `start`/`end` 值/
  `bpm`）是浮点，没有有理表示（`1/3` 在那里就是 `0.3333333333333335`）。
- 浮点解析开了 `serde_json` 的 `float_roundtrip`：默认解析偶尔差 1 ULP（实测真实谱面的
  `chartTime` 128027.70309200211 → 写回 128027.70309200212），"原样写回"不能有这种漂移。
- 目标版本档位可切换：`--rpe-version 150|160` 或 `RpeTarget{version}`（`META.RPEVersion` 不可信，只作记录）。
- **保存形态跟着"载入的是什么"走**：从文件夹打开就写回那个文件夹（`OpmFolder`/`RpeFolder`），
  从 `.opm`/`.pez` 打开就写回那个包，从裸 JSON 打开就写回那个文件。第一次保存也如此
  （曾经只看扩展名：目录里的 `opm.json` 被判成"一个 `.json`"，`--save` 会把 opm 工程写成 RPE JSON）。

## 音频格式**wav / flac / mp3 / ogg-vorbis / m4a-aac / alac / adpcm**（symphonia 解码，识别不出时给明确原因）。
查一个文件能不能用（不解码输出流、不需要音频设备、不开窗口）：

```sh
opm-app --audio-probe FILE      # → {"codec":"OGG Vorbis","sampleRate":48000,"channels":1,"durationSec":60.0}
```

**响应只承诺"已受理"**（`result.view = true`）：视图状态在下一帧生效。要确认就轮询 `view`/`ui_stats`，
这和"文档改动等广播"是同一个诚实口径 —— 不要拿命令的成功响应当作"界面已经那样了"。

### 附着（`--attach`）时的更新语义

附着进 GUI 进程后，你的每条命令走的是**和 GUI 控制台完全相同**的路径：
`EditCore::exec` 改动 → 发 update 广播（`origin:"Remote"`）→ GUI 按话题重建对应面板，并唤醒一帧重绘。

对你的影响只有两条：

- 命令响应返回 ≠ 界面已更新：响应只说明**文档改完了**，界面在下一帧应用广播。要确认界面跟上，读
  `{"op":"ui_stats"}` 的 `seen_revision`（对应响应的 `revision`），或看 `last_broadcast`。
- 广播是**细粒度**的：只改元信息不会重建谱面视图，只改轨道（alpha/moveX/speed）不重建任何缓存。
  想自查"我的改动惊动了哪些面板"，对比操作前后的 `builds_*` / `skipped_*` 即可
  （口径见 [`README.md`](./README.md#更新广播editcore-是唯一可写方gui-只是订阅者)）。

---

## 3. 命令参考

单条命令即一个 JSON 对象；一批命令可以是 **JSONL**（每行一条，`//` 开头的行忽略）或 **JSON 数组**。

### 音符

| 命令 | 说明 |
|---|---|
| `{"op":"add_note","line":0,"kind":"tap\|hold\|drag\|flick","startBeat":[n,d],"endBeat":[n,d],"laneX":0,"set":{…}}` | `hold` 必须给 `endBeat`；`set` 可顺带覆盖字段 |
| `{"op":"set_note","line":0,"index":3,"set":{"laneX":200,"alpha":128}}` | `set` 里字段名同时接受 `laneX` 与 `lane_x` |
| `{"op":"del_note","line":0,"index":3}` | |
| `{"op":"move_notes","line":0,"delta":[4,1]}` | 整线平移（配合 `normalize` 调整全谱时序） |

### 判定线

| 命令 | 说明 |
|---|---|
| `{"op":"add_line","name":"L1","bpmFactor":1.0}` | 新线带一个空层 |
| `{"op":"set_line","line":0,"set":{"name":"L1","zOrder":2,"isCover":true}}` | |
| `{"op":"del_line","line":1}` | |

### 事件（**移动 / 透明度 / 流速统一走事件模式**）

轨道名：`moveX` · `moveY` · `rotate` · `alpha` · `speed`

| 命令 | 说明 |
|---|---|
| `{"op":"add_event","line":0,"track":"moveX","startBeat":[0,1],"endBeat":[4,1],"startValue":0,"endValue":200,"easing":"outQuad"}` | 缓动名见 `spec/easing.json` 的 29 个名字（字符串，不是编号） |
| `{"op":"set_event","line":0,"track":"alpha","index":0,"set":{"endValue":0.5}}` | |
| `{"op":"del_event","line":0,"track":"speed","index":0}` | |
| `{"op":"split_event","line":0,"track":"moveX","index":0,"atBeat":[2,1]}` | 在中点切分：**切点上的值问求值器**（带缓动，五条轨道同一口径），所以"切一刀不改变表演"（两半各自的缓动形状会重新算） |
| `{"op":"set_target","line":0,"atBeat":[4,1],"target":{"x":250.3,"y":-118.75,"angle":45,"alpha":0.42}}` | **块末就位**：一次给出"线在这一刻该在哪儿"，四轨（moveX/moveY/rotate/alpha）一起写、**一个撤销步**。`target` 至少给一个键；只写**真的变了**的轨道（已是该值的跳过）。要改的那一块按 `perf::active_event` 选（与求值器同一判据，重叠时也对）：块末写终值 / 块首写起值 / 块内先 `split_event` 再写两侧 / 空位写前一块的终值 / 首块之前写首块起值。返回 `{wrote, cmds, plan[], failed[]}`（`wrote` 数轨道、`cmds` 数子命令，块内那种情形一条轨道要 3 条）。**求值器端点是按定义取端值**，所以那一刻求值到的与你写下的数**按位相等** |
| `{"op":"set_track_constant","line":0,"track":"speed","value":10}` | **一步满足轨道不变量**：清空该轨并铺一条覆盖全谱的恒定事件。**流速的默认/基准值是 10**（RPE 口径：1 单位流速 = 120 RPE y 单位/秒 ⇒ 10 = 1× = 1200 单位/秒 = 0.75 秒划过 900 高的窗口）；**整条轨道没有流速事件时预览也按 10 走** |

⚠️ **缓动是"折线"，五条轨道（含 `speed`）同一口径**：从块开头起每 0.1 秒一个节点、节点之间线性；
**回弹类（`back`/`elastic`/`bounce`）的回弹点与折点一定落在节点上**；整块不足 0.1 秒 ⇒ 等价线性；
相邻节点不足 0.04 秒就合并。于是 `speed` 的 `easing` **真的生效**，而 `∫v dτ` 仍是闭式精确解。
`opm-ctl --file F lines` 的 `valueAt`、检查器、时间轴曲线都是这一条折线的值 —— 想看"这块被切成了几段"，
用 `event_knots`（源码）/ 时间轴上的折角。
| `{"op":"normalize"}` | 排序 / 补空隙 / 裁重叠 / 首事件回退到 ≤0 / 末事件延到谱末之后 |

⚠️ **事件索引是"图层内下标"，而 GUI 的编辑区用的是"合并视图下标"** —— 两者只有在单图层时相同。
视图把一条线的五个图层合并成一条时间线并按起拍排序（求值要的就是这个），所以多层文档里
`del_event`/`set_event` 的 `layer` 必须写对：视图侧靠 `doc::EventRef`（第几层 + 该层下标）回去。
写脚本时建议显式带 `layer`。

### 遮蔽区（游戏里的「躁域」）

一块**三角形区域**（屏幕空间，X ∈ ±675 / Y ∈ ±450），游戏里点进这块区域**无法与音符交互**。
**opm 独有**：官方谱面格式与 RPE 都没有它，导出 pez 会**丢弃并报告**（见 §7）。

七条通道：`x1` `y1` `x2` `y2` `x3` `y3`（三个顶点）+ `active`（外观开关，值写 `true`/`false`）。

| 命令 | 说明 |
|---|---|
| `{"op":"add_zone","startBeat":[0,1]}` | 新建一块：在拍 0 写 6 条常量事件 = **屏幕中央的正三角形**（`active` 不写事件）。`endBeat` 缺省 = `max(谱面末尾, startBeat+4拍)`；`empty:true` 建一个**没有任何事件、因此不显示**的区 |
| `{"op":"del_zone","index":0}` | `zone` 与 `index` 都认 |
| `{"op":"set_zone","zone":0,"set":{"name":"右侧躁域"}}` | 目前只有 `name` |
| `{"op":"add_zone_event","zone":0,"track":"x1","startBeat":[8,1],"endBeat":[16,1],"startValue":0,"endValue":-400,"easing":"inOutCubic"}` | **缺省值 = 该通道此刻的值**（不传 `startValue` 时，放下一刻不跳变）；`endBeat` 缺省 = 起点 + 4 拍 |
| `{"op":"set_zone_event","zone":0,"track":"active","index":0,"set":{"startValue":true,"endValue":true}}` | `active` 的值可以是布尔或数字 |
| `{"op":"del_zone_event","zone":0,"track":"y3","index":1}` | |
| `{"op":"resize_zone_event","zone":0,"track":"x1","index":0,"edge":"end","toBeat":[6,1]}` | 只动这一个端点（与 `resize_event` 同一语义） |
| `{"op":"move_zone_event","zone":0,"track":"x1","index":1,"delta":[4,1]}` | 整块平移；与邻块重叠会被拒（返回 `ok:false`，文档不动） |
| `opm-ctl --file F masks [--at SEC] [--json]` | **遮蔽区的数值快照**：此刻显不显示、`active`、三个顶点的坐标、七条通道各有几条事件。核对"区域此刻长什么样"用这个（图看形状、它看数值，两者同一份求值） |

⚠️ **遮蔽区通道的空隙是合法的**（与判定线轨道相反）：空档里保持前一条事件的终值，
**首事件也可以晚于拍 0** —— "这块区域什么时候出现"就是靠它表达的。
`add_zone_event` 插入时会把被它压住的前一块**裁到它的起点**（切点上的值保持不变），
所以"给一条铺满全谱的常量事件里插关键帧"不会留下重叠。

### 元信息与只读

| 命令 | 说明 |
|---|---|
| `{"op":"set_bpm","index":0,"bpm":200}` | |
| `{"op":"set_meta","set":{"name":"…","charter":"…","difficulty":"IN","level":"IN 15","audio":"song.ogg"}}` | `audio` 是**文档字段**（会写进文件）；只想换预览音频用视图命令 `{"op":"audio"}` |
| `{"op":"summary"}` | 行/音符/事件数、谱面末尾、能力等级（轻量，适合每步后自查） |
| `{"op":"dump"}` | 完整文档 JSON |
| `{"op":"validate"}` | 结构化问题清单 |
| `{"op":"overlaps"}` | **事件重叠**（同一轨道上两段同时覆盖）：`{count, items:[{line,layer,track,prev,next,startBeat,endBeat,pointer,label}]}`。查询**不改文档**（revision 不动）。GUI 的底栏红字与冲突浏览器读的是**同一份缓存** |
| `opm-ctl --file F overlaps [--json]` | 事件重叠；**没有 → 退出码 0，有 → 退出码 4**（脚本可直接 `opm-ctl --file x overlaps && …`） |
| `opm-ctl --file F lines [--at SEC] [--json]` | **判定线的数值快照**：每条线的属性、子音符数、五条轨道的事件数、以及该时刻求值出的 moveX/moveY/rotate/alpha/speed。核对"事件是否真的生效"用这个（读图用来核对"看起来对不对"） |
| `{"op":"undo"}` / `{"op":"redo"}` | **仅在单次调用/单个会话内有效**（见 §7） |
| `{"op":"ping"}` | 连通性自检 |
| `{"op":"broadcasts","recent":5}` | 最近的 update 广播（revision/origin/label/topics/changes）—— 看"谁改了什么、广播给谁" |
| `{"op":"ui_stats"}` | **界面侧**统计（仅 `--attach` 时有意义）：收到几条广播、各面板重建几次、多少条广播与本面板无关、唤醒→应用延迟 |

---

### 同时开多份谱面 / 缓存归谁（`--file` 与打开时的检查）

- **每份谱面一把锁**：解压缓存目录里的 `lock.pid`（pid 锁）+ 一句 `ping`。
  **不同谱面可以同时被不同进程读**；同一份容器被第二个进程打开时会**明确拒绝**。
- `opm-ctl --file X …` 走的是 `CacheClaim::ReadOnly`：**不抢锁、不写缓存**，
  所以 GUI 开着 X 时 agent 照样能读它（只是不覆盖它的快照与会话元数据）。
- **崩溃遗留**（缓存目录在 ∧ `lock.pid` 那个 pid ping 不通）在**打开时**才判定：
  界面会问「继续 / 丢弃」；`--doc` 启动那条路用 `OPM_RESUME_AUTO=continue|discard` 无人值守地走完，
  没给就拒绝启动（**不会**静默覆盖那份没保存的快照）。
- `{"op":"ping"}` 现在**自报身份**：`{pong, revision, pid, exe, chart, cacheDir}` ——
  判"那份缓存的主人还在不在"就靠它核对 pid。`opm-ctl --attach` 用的同一条控制通道。

## 4. 响应与退出码

每条命令回一行：

```json
{"ok":true,"op":"add_note","result":{"line":0,"index":3,"notes":4}}
{"ok":false,"op":"add_event","error":"未知缓动 \"easeInOut\""}
```

默认输出人类可读单行（`ok add_note {…}` / `FAIL add_event: …`）；`--json` 输出原始 JSON。

| 退出码 | 含义 |
|---|---|
| `0` | 全部命令成功（且未执行校验、或校验通过） |
| `1` | 至少一条命令失败（该条已回滚） |
| `2` | 用法错误 / IO 失败 / 渲染失败 |
| `3` | 校验存在 ERROR（`validate` 子命令专用） |
| `4` | 存在事件重叠（`overlaps` 子命令专用；它不是错误，是"需要人看一眼"的状态） |

---

## 5. 推荐的 agent 闭环

```
1. summary                     # 先看现状（行数/音符数/谱面末尾/事件数）
2. dump       （可选）          # 需要精确定位时读全文，拿到 index
3. 编辑（JSONL 脚本，一次提交）  # 失败即回滚，不会留下半成品
4. normalize                   # 补齐轨道不变量
5. validate                    # 必须 exit 0；否则读 issues 的 pointer 定点修
6. render --at T --out p.png   # 出图，agent 读图核对几何是否正确
7. 迭代 3–6
```

**为什么把 `render` 放进闭环**：`validate` 只能证明结构合法，证明不了"看起来对"。音符位置、长条长度、判定线透明度这类**几何/视觉语义**只有看图才能确认。这是本设计里最刻意的一环——给 agent 眼睛，而不是只给它一个校验器。

**示例（bash）**

```sh
# 建一份带默认轨道的谱面
opm-ctl new --out chart.opm.json --name my-chart --bpm 180

# 批量编辑并落盘（失败的命令会回滚，但同批其它命令仍生效）
opm-ctl --file chart.opm.json --script edits.jsonl --save

# 校验（两份独立实现可交叉验证）
opm-ctl --file chart.opm.json validate && python3 ../spec/check.py chart.opm.json

# 出图
opm-ctl --file chart.opm.json render --at 4.0 --lookahead 2.0 --out preview.png
# 出图默认画出**窗口边界**（RPE ±675 × ±450，即 1350×900）并把边界外压暗：
# 于是"音符跑到画面外了"在图上直接看得见（变暗但仍可见），不必自己算坐标。
# 判定线长度是编辑器设置（RPE 格式无此字段），默认 **3000**（比窗口宽），出图时可用 --line-len 指定：
opm-ctl --file chart.opm.json render --at 4.0 --line-len 1600 --out wide-line.png   # 线伸出窗口
opm-ctl --file chart.opm.json render --at 4.0 --no-boundary --out plain.png         # 不要边界框
# 附带的 RPE 对照（易错点）：RPE 的线/音符 alpha 是 0~255，opm 用 0~1；导入时 ÷255。
```

---

## 6. 约定：时间、坐标、索引

- **时间 = 有理拍**，写作 `[分子, 分母]` 或 `{"n":…,"d":…}`。**不要传小数**：浮点会让导入→导出产生数值漂移（这是 `spec/opm-format.md` 的硬约束）。
  换算：`秒 = 拍 × 60 / bpm`。
- **坐标 = RPE 单位**：判定线坐标系 x ∈ [-675, 675]、y ∈ [-450, 450]，原点在演奏区中心（`spec/opm-format.md` 第 3 节）。
- **枚举 = 字符串**：`kind` 是 `"tap"/"hold"/"drag"/"flick"`；`easing` 是 29 个名字之一。
  ⚠️ **不要用整数**：官方格式与 RPE 的整数映射**互不相同**（`spec/note-types.json`），传整数必然踩坑。`doc::NoteKind` 内部保留了 `to_official()` / `to_rpe()` 两套映射，只在 codec 边界使用。
- **索引语义**：`line` 是 `judgeLines` 数组下标；`index` 是该线 `notes`（或该轨 `events`）数组下标。
  **`del_*` 之后后续元素下标会前移** —— 批量删除建议按 index **从大到小**执行，或每步 `dump` 重新定位。
  GUI 的 Del 就是这么做的（`edit::delete_selection_commands`，有单测钉住降序）。
- **流速（`speed` 轨道）的值域**：默认/基准 **10** = 1×（RPE 口径：1 单位 = 120 RPE y 单位/秒，
  于是 10 = 1200 单位/秒 = 0.75 秒划过 900 高的窗口）。音符自身的 `speed` 字段是**另一个东西**，
  默认 **1.0**，乘在"离判定线的距离"上（不改到达时刻）。详见 README「下落速度（流速）：与 RPE 一致」。

---

## 7. 限制与未实现（明确列出，避免 agent 误用）

- 遮蔽区**已经进 wgpu 管线**（与判定线共用）：`render` 出的 PNG 里有它，`masks` 给同一份数值。
  唯一没有的是**播放期的那圈柔光**（它跟指针位置有关，而无头出图没有指针）。
- **导出 RPE/pez 会丢掉遮蔽区**（那边没有这个字段），保真度报告里会逐个数报出来。
- 遮蔽区编辑模式下的事件块当前只有**单选**（框选/Ctrl+多选还没做）。

1. **CLI 每次调用是独立会话**：`undo` 只在本次调用的命令序列内有效，跨调用无效。需要"试错"时建议：在同一次调用里用 `--script` 提交，或先备份文件。
2. **撤销栈有字节上限（64 MiB）**：超限时从最旧开始丢弃（内存有界优先于撤销深度）。实测 2 万音符文档上 2000 条改动耗时 0.19–0.31 s，日志字节量只与改动规模成正比（不再随文档大小放大）。
3. ~~**多 BPM 的时间映射未完成**~~ → **已实现**：`perf::TimeMap` 是分段线性的（`tests/perf.rs` 有拍↔秒往返断言），
   无头出图与 GUI 走**同一份**映射（`state::chart_from_doc`），多 BPM 谱面的预览位置不再偏。
4. **父子线 / 控制曲线 / 扩展事件**尚未进入模型（`spec/opm-format.md` 第 4.4/4.5 节），模型对未知字段走 `foreign` 袋原样保留，不会丢但也不能编辑。
5. ~~**codec 未接**~~ → **已实现**：RPE 导入/导出（含 `RPEVersion` 不可信、`color`/`tint` 双名等历史怪癖）
   与 `.opm` 容器（ZIP：谱面 + 音乐 + 曲绘）。**官谱（official）格式仍未接** —— `doc::to_official` 目前
   没有任何调用方，别把它当已完成的导出路径。
6. **校验只覆盖 `spec/opm-format.md` 第 8 节**：不含 RPE 导入侧的历史怪癖（`RPEVersion` 不可信、`color`/`tint` 双名等），那些属于 codec 的职责。
7. **无网络、无外部副作用**：`opm-ctl` 只读写被显式指定的文件路径。
   **测试同样不许有外部副作用**：`cargo test` 不得打开浏览器/URL（"获取 7z"这类动作在测试里只测
   **纯校验函数** `filedialog::check_url`，真身 `open_url` 由人工/交互路径覆盖）——
   曾经一条测试真的把 example.com 与 7-zip 官网打开了。

---

## 8. 双校验器：同一规则、两份实现

| 实现 | 位置 | 用途 |
|---|---|---|
| Rust | `app/src/cmd.rs::validate`（`opm-ctl validate`） | 编辑流程内即时校验 |
| Python | `spec/check.py` | 规范侧的独立校验器，可被 CI/外部工具调用 |

两者**共享同一份规则来源**（`spec/opm-format.md` 第 8 节），但代码独立。已在同一份故意损坏的文件上交叉验证：**两份实现给出相同的 JSON 指针与消息**（`/judgeLines[1].layers[0].alpha[1] 轨道不连续`）。
⇒ 任何一方改了规则而另一方没跟上，都能被这种交叉验证抓出来。
