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

- 根：`BPMList`（时间三元组或浮点都吃）、`META`（`offset` 是**毫秒**；`song`→`audio`、
  `illustration`→`illustrator`；`RPEVersion` 只作记录并保留）、`judgeLineList`；
  `chartTime`/`judgeLineGroup`/`multiLineString` 等编辑器字段原样保留。
- 音符：`type` 走 `spec/note-types.json`（**RPE 2=Hold、3=Flick**，与官谱相反）、
  `alpha` 0~255（**>255 不截断**）、`above`、`isFake`、`speed`、`size`→`widthScale`、`yOffset`、`judgeArea`。
- 事件：5 条轨道 + 29 种 `easingType`（表来自 `spec/easing.json`）、`bezier`/`bezierPoints`；
  时间写成**整数三元组** `[整拍,分子,分母]`（实测真实谱面 2591 个音符时间全是数组）。
- 目标版本档位可切换：`--rpe-version 150|160` 或 `RpeTarget{version}`（`META.RPEVersion` 不可信，只作记录）。

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
| `{"op":"split_event","line":0,"track":"moveX","index":0,"atBeat":[2,1]}` | 在中点按线性插值切分（有缓动时先近似，随后用 `set_event` 修正） |
| `{"op":"set_track_constant","line":0,"track":"speed","value":10}` | **一步满足轨道不变量**：清空该轨并铺一条覆盖全谱的恒定事件。**流速的默认/基准值是 10**（RPE 口径：1 单位流速 = 120 RPE y 单位/秒 ⇒ 10 = 1× = 1200 单位/秒 = 0.75 秒划过 900 高的窗口）；**整条轨道没有流速事件时预览也按 10 走** |

⚠️ **流速轨只按 `linear` 求值**（音符位置是流速的积分，线性有闭式解 —— 见 README「下落速度」）。
给 `speed` 事件写别的 `easing` **不报错、也不改写**（原样保留、导出照旧写回），
但**预览与音符位置按线性算**；`opm-ctl --file F lines` 的 `valueAt` 与检查器显示的也是线性值。
所以 agent 想控制音符位置时，直接改 `startValue`/`endValue`/起止拍即可，不必绕缓动。
| `{"op":"normalize"}` | 排序 / 补空隙 / 裁重叠 / 首事件回退到 ≤0 / 末事件延到谱末之后 |

⚠️ **事件索引是"图层内下标"，而 GUI 的编辑区用的是"合并视图下标"** —— 两者只有在单图层时相同。
视图把一条线的五个图层合并成一条时间线并按起拍排序（求值要的就是这个），所以多层文档里
`del_event`/`set_event` 的 `layer` 必须写对：视图侧靠 `doc::EventRef`（第几层 + 该层下标）回去。
写脚本时建议显式带 `layer`。

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
