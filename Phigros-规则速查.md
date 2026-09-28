# Phigros 规则速查

> 用途：玩法机制 + 谱面格式的双层速查，供 OpenPhM 相关开发/写作引用。
> 编制日期：2026-09-27。所有数值均标注来源；**未经交叉验证的条目标注「存疑」**，见第 5 节。
> 游戏版本口径：Fandom 中文 Wiki 部分页面截至 3.11.0.1，定数页已更新至 3.19.0；版本敏感项一律显式标注。
> 来源性质：Phigros 官方未发布公开的完整规则文档，以下全部由社区 Wiki / 模拟器文档 / 实测记录推导。

---

## 1. 玩法机制

### 1.1 音符类型与操作

| 类型 | 别名 | 操作 | 判定特性 |
|---|---|---|---|
| Tap | 蓝键 | 音符与判定线重合时点击 | 有 Perfect/Good/Bad/Miss |
| Hold | 长条 | 头部重合时按下并保持到尾部结束 | **无 Bad 判定**；中途短暂松手不影响判定；尾部按完成度结算 |
| Drag | 黄键 | 重合时手指位于判定区内即可，**可换手** | **无 Good / Bad 判定**（只有 Perfect 或 Miss） |
| Flick | 粉键/红键 | 重合时在判定区内向任意方向滑动，一根手指一个方向只判一次 | **无 Good / Bad 判定** |

- 判定区是一个**与判定线垂直、向两侧无限延伸的矩形**；设置里的「按键大小」只改外观，**判定区固定不可调**。有效命中还需落在音符向屏幕边缘延伸的竖直列内。
- 单次点击只能判定一个 Tap/Hold，且优先判定「仍可判定中最早出现」的那一个。
- **Hold 只累积 1 次连击**；终点**没有「松手时机」判定**，按满规定时间即续连。中途可换指，但完全松手掉落之后**不能重按**。
- **Flick 取到必为 Perfect**，且不要求时机精准（提前按住的手指擦过也能判到）；因此 Flick 与 Drag 永远不会产生 Good/Bad，也就**不会拉低准确率**。
- 判定线宽度约为屏幕的 250%，可任意平移、旋转、隐藏（用于「两音符重合时打击」这类配置）；判定线本身也可以是音符或屏幕边缘。

来源：[Phigros Wiki 中文·游戏内容](https://phigros.fandom.com/zh/wiki/%E6%B8%B8%E6%88%8F%E5%86%85%E5%AE%B9)

### 1.2 判定等级与时间窗口

| 判定 | 常规模式 | 课题模式 | 适用音符 | 权重（准确率/判定分） |
|---|---|---|---|---|
| Perfect | ±80 ms | ±40 ms | **全部** | 100% |
| Good | ±160 ms | ±75 ms | 仅 Tap / Hold | 65% |
| Bad | ±180 ms | ±140 ms | **仅 Tap** | 0%，且重置连击 |
| Miss | 无窗口（未击中 / 不在判定列内） | 同左 | 全部 | 0%，且重置连击 |

- **判定窗口的适用音符是硬规则**：Hold 没有 Bad 判定（无法在 Bad 区间判到 Hold）；Flick 与 Drag 既无 Good 也无 Bad，只有 Perfect 或 Miss。
- Bad 会让音符变暗红并逐渐消失；Miss 不显示打击特效。
- 中文 Wiki 采用「区间段」写法（Good ±81~160、Bad ±161~180），英/日 Wiki 采用「边界」写法（Good ±160、Bad ±180）；**总跨度一致，实现取边界值**。课题模式 Bad 的中文写法（±75~90）与英/日（±140）冲突，采信 ±140。详见第 5 节。
- 历代版本是否使用过不同的判定窗口：**无任何来源可证**，勿臆断。

来源：[Phigros Wiki 中文·游戏内容](https://phigros.fandom.com/zh/wiki/%E6%B8%B8%E6%88%8F%E5%86%85%E5%AE%B9)、[Phigros Wiki（英）Game Mechanics](https://phigros.fandom.com/wiki/Game_Mechanics)、[Phigros Wiki*（日）·howtoplay](https://wikiwiki.jp/phigros/howtoplay)

### 1.3 准确率与分数

```
Accuracy = (Perfect数 + Good数 × 0.65) / 总物量        # Bad / Miss 权重为 0
Score    = round(900000 × Accuracy) + round(100000 × MaxCombo / 总物量)
```

- 满分 1,000,000 = 判定分 900,000 + 连击分 100,000。
- 总物量 = 谱面 Note 总数（不含假音符）。
- 连击在 Perfect/Good 时 +1，Bad/Miss 时归零；MaxCombo 上限即总物量。连击数 ≥ 3 后才在画面上方显示。
- **连击不影响 rks**——rks 只看准确率。
- 分数**与定数无关**；因此可能出现「分数更低但准确率更高」从而刷新 rks 的情况。
- 自检用推论：全 Good 且全连 → Accuracy = 65%，总分 = 900000×0.65 + 100000 = **685,000**。
- 课题模式**只取判定分**（上限 1,000,000），不含连击分。

来源：[Phigros Wiki 中文·游戏内容](https://phigros.fandom.com/zh/wiki/%E6%B8%B8%E6%88%8F%E5%86%85%E5%AE%B9)、[Phigros Wiki（英）Game Mechanics](https://phigros.fandom.com/wiki/Game_Mechanics)、[Phigros Wiki*（日）·howtoplay](https://wikiwiki.jp/phigros/howtoplay)（含「ACC×900000 + MC÷TN×100000」与「rks 不受连击影响」）

### 1.4 评级与通关

| 评级 | 条件 |
|---|---|
| φ (Phi) | 1,000,000 分（全 Perfect，无 Good/Bad/Miss） |
| V（蓝 V / FC） | Bad + Miss = 0 且 Good ≥ 1；**无分数下限**，覆盖除 φ 外的其它分数评级 |
| V（白 V） | 960,000 ~ 999,999 |
| S | 920,000 ~ 959,999 |
| A | 880,000 ~ 919,999 |
| B | 820,000 ~ 879,999 |
| C | 700,000 ~ 819,999 |
| F (False) | 0 ~ 699,999 |

- ≥ 700,000（C）视为通关（Cleared），但 **AT 难度需 B 及以上**才计入通过。注意 Wiki 自身记载的基准不一致：章节选择界面按 C 统计、个人资料按 B 统计（见第 5 节）。
- 2.0.0 起**全连（Good ≥ 1 且无 Bad/Miss）即判蓝 V，即使分数不足 960,000**；且已取得蓝 V 后，再打出含 Miss 的成绩，显示评级仍保持蓝 V。
- φ 在判定上实际是「分数达到 1,000,000」；若因 bug 全 Perfect 但分数未满，会判为蓝 V。
- 开「FC/AP 指示器」后判定线用颜色提示：AP 可能时为金色、FC 可能时为蓝色（实测色值 `#feffa9` / `#a2eeff`，默认 `#ffffff`）。
- 蓝 V 不应无条件视为高于其它分数评级的成绩。

来源：[Phigros Wiki 中文·游戏内容](https://phigros.fandom.com/zh/wiki/%E6%B8%B8%E6%88%8F%E5%86%85%E5%AE%B9)

### 1.5 定数与 rks（Ranking Score）

**单曲 rks**（取该谱最高准确率记录）：

```
Acc < 70%  →  rks = 0
Acc ≥ 70%  →  rks = [ (100 × Acc − 55) / 45 ]² × 定数
```

- Acc = 100% 时 rks = 定数；Acc = 70% 时 rks = 定数 / 9。
- 计算时应保留额外小数精度，最后再取整。

**总 rks**（公式随版本变更，这是最容易被记错的一处）：

| 版本区间 | 公式 |
|---|---|
| 3.11.0 起（现行） | `rks = (Best27 的单曲 rks 之和 + 已 φ 的定数最高 3 张谱的定数之和) / 30` |
| 3.11.0 之前 ~ 1.4.7 之后 | `rks = (Best19 之和 + 定数最高的 1 张 φ 谱定数) / 20` |
| 1.0.0 ~ 1.4.7 | `rks = Best19 之和 / 19` |

- **不存在「第 20 张折半 ×0.5」的规则**——中文/英文/日文 Wiki 与 3.11.0 官方规则说明中均无此写法（见第 5 节）。
- Best27 按**成绩定数（单曲 rks）**排序，不是按谱面定数排序；Phi3 与 Best27 可以重叠，同一曲目的不同难度可各占一个名额；**φ 谱不足 3 张时按 0 补齐**。
- 结果四舍五入到小数点后两位。

**定数（difficulty constant）**：

- 精确到小数点后一位，游戏内一般只显示整数部分；定数用于 rks，**不参与分数计算**。
- 2.5.0 后：7 级及以下只有 `+0.0` 与 `+0.5` 两种定数；7 级以上有 `+0.1` ~ `+0.9`，**但没有 `+0.0`**。
- 现行定数区间约 1.0 ~ 17.6（六周年更新前上限 16.8）；SP 谱面**没有定数**。
- 理论最高 rks 随版本与定数调整变动：中文 3.11.0.1 记 **16.80**，日文 3.19.4 记 **17.04**，英文 3.20.0 记 **17.05**。
- 定数下调会导致老玩家 rks 不升反降。

来源：[Phigros Wiki 中文·定数](https://phigros.fandom.com/zh/wiki/%E5%AE%9A%E6%95%B0)、[Phigros Wiki 中文·游戏内容](https://phigros.fandom.com/zh/wiki/%E6%B8%B8%E6%88%8F%E5%86%85%E5%AE%B9)、[Phigros Wiki（英）Game Mechanics](https://phigros.fandom.com/wiki/Game_Mechanics)（历代 rks 公式）、[TapTap·3.11.0 Rks 规则变动说明](https://www.taptap.cn/moment/640657705018589989)

### 1.6 难度分级

| 难度 | 含义 | 常见定数区间 | 说明 |
|---|---|---|---|
| EZ | Easy | 1 ~ 8 | 每首常规曲目均有 |
| HD | Hard | 6 ~ 13 | 每首常规曲目均有 |
| IN | Insane | 11 ~ 16 | 每首常规曲目均有 |
| AT | Another | 14 ~ 17 | 部分曲目才有 |
| SP | Special | 无定数，标为 `SP Lv.?` | 限时活动（愚人节、圣诞节等）曲目，活动后移除 |
| Legacy | 旧谱 | 继承原定数 | 官方用新谱替换 IN 时保留的旧 IN 谱；现存 4 张：`Break Through The Barrier`、`ENERGY SYNERGY MATRIX`、`Aleph-0`、`Lyrith -迷宮リリス-` |

- Legacy 进入方式：2.4.0 及之前点「Legacy」按钮；**2.4.1 起长按左侧大难度标牌**（中文 Wiki 记 15 秒，日文 Wiki 记 5 秒，见第 5 节）。
- 绝大多数 Legacy 谱已于 2.4.1 删除，仅存上表 4 张。
- 定数与难度等级不等价：同为 16 级，`Re: End of a Dream[AT]`（16.9）明显难于 `GOODRAGE[IN]`（16.0）。

来源：[Phigros Wiki 中文·游戏内容](https://phigros.fandom.com/zh/wiki/%E6%B8%B8%E6%88%8F%E5%86%85%E5%AE%B9)、[Phigros Wiki 中文·难度](https://phigros.fandom.com/zh/wiki/%E9%9A%BE%E5%BA%A6)

### 1.7 解锁机制

**曲目与章节**

- 章节内：`当前曲目任意难度 ≥ 880000（A）` → 解锁下一曲。
- 章节解锁（主线）：Chapter 5 需 Chapter Legacy 内取得 6 个 A 及以上；Chapter 6 需 `Leave All Behind` 任一难度 A；Chapter 7 需 `Igallta` A；Chapter 8 需 `Rrhar'il` A。
- 支线：Side Story 2 需 `Rrhar'il` A；Side Story 3 需 `NO ONE YES MAN` A；**Side Story 4 需阅读收集品「档案·无相乡调查报告」**。
- 单曲精选集：需消耗 Data 购买（旧曲 16MB、最新曲 4MB），购买仅解锁 EZ + HD；**rks ≥ 11.00 时可直接解锁 EZ/HD/IN**（中文 Wiki 此处写作「≤11」，与英文 Wiki 的「≥11」方向矛盾，实质均为「rks 达 11 会开放更多 IN」，见第 5 节）。

**难度解锁**

- IN：同曲 HD ≥ 920000（S），**或**本章节前一曲 IN ≥ 880000（A）。
- AT：同曲 IN ≥ 920000（S）。
- rks ≥ 11.00 时：Legacy 章节曲目与已购单曲精选集的 IN 直接解锁；其它章节**第一首**曲目的 IN 直接解锁。

**特殊曲目**：`Spasmodic`、`Igallta`、`Rrhar'il`、`Crave Wave`、`The Chariot ~REVIIVAL~`、`Luminescence`、`Retribution`、`DESTRUCTION 3,2,1`、`Distorted Fate` 的常规三难度，以及 `You are the Miserable`、`Stasis`、`Shadow`、`DESTRUCTION 3,2,1` 的 AT 难度，均有各自的特化解锁方法（**逐曲条件未逐条核实，见第 5 节**）。

**课题模式**

- 解锁：通关当前最新主线章节（Chapter 8）并观看制作人员名单；历史版本曾解锁 `Igallta` / `Rrhar'il` 的账号可直接游玩。
- 规则：任选 3 张**难度互不相同**的已解锁谱面连续游玩；判定收紧；只取判定分；三曲总分 ≥ 2,460,000 可获「综合评价」。
- 综合评价的数字部分 = 三张谱面的**难度之和**（最低 3、最高 51）；外框颜色由三曲总分决定：彩 = 3,000,000；金 2,940,000 ~ 2,999,999；橙 2,850,000 ~ 2,939,999；蓝 2,700,000 ~ 2,849,999；绿 2,460,000 ~ 2,699,999；**低于 2,460,000 无评级**。
- **课题模式成绩不记录、不能用于解锁章节/曲目/难度**。

来源：[Phigros Wiki 中文·游戏内容](https://phigros.fandom.com/zh/wiki/%E6%B8%B8%E6%88%8F%E5%86%85%E5%AE%B9)、[Phigros Wiki 中文·章节列表](https://phigros.fandom.com/zh/wiki/%E7%AB%A0%E8%8A%82%E5%88%97%E8%A1%A8)

### 1.8 Data、收藏品与存档

**Data（货币）**

```
得分 ≥ 880,000 才有 Data 奖励（仅该区间内适用）
Data = MaxData × (Score − 700000) / 300000
```

| 谱面等级 | MaxData |
|---|---|
| Lv 1 ~ 6 | 256 KB |
| Lv 7 ~ 9 | 512 KB |
| Lv 10 ~ 12 | 768 KB |
| Lv 13 ~ 14 | 1024 KB |
| Lv 15 | 1280 KB |
| Lv 16 ~ 17 | 1536 KB |

- 首次阅读一份收藏品额外 **+1.5MB**（1.4.3 时代为 512KB）。
- 现行用途：购买单曲精选集曲目（16MB，最新曲 4MB；3.0.0 曾临时全曲 8MB）。
- 历史（已移除）：1.2.1 加入商城（买曲 512KB 背景 / 2816KB 头像 + 抽奖）；1.3.0 加入「Data Mining」抽奖（1024KB 单抽 / 8192KB 十连，可出 Avatar / Illustration / Collection / Data / Null）；2.0.0 商城与抽奖整体移除，头像背景改为打歌获取。

**收藏品（Collection）**

- 1.3.0 加入；随特定曲目播放解锁「文件」，文件分 6 类：Main、Bold、Key、Souvenir、Nonsense、Nazo。
- 共 14 个收藏夹（Chapter I~VIII、Side story 1~4、Chapter EX、Extra Story - BassAreUs），文件数 1 ~ 70 不等。
- 部分文件分段解锁，并提供寻找其它文件与解锁隐藏曲的线索；**部分曲目的解锁条件就是「阅读某份收集品」**（如 Side Story 4）。

**账号与云存档**

- 1.6.10 加入账号系统（当时仅 offline 登录）；**2.4.0 加入云存档**；3.13.0 起 TapTap 账号需中国身份证验证才能登录。
- 离线可玩范围、云存档冲突处理、本地存档位置与备份方式：**未取得权威描述**（见第 5 节）。

**其它进度要素**

- 「重演 / Revisit」3.5.0 加入，通关最新主线章节后解锁，可重看已通关曲目的过场与解锁流程；「REVISION PRO」为 3.5.2 引入的重演变体（非 4/1 访问愚人节内容）。
- 课题模式 2.0.0 加入；随机曲目 2.5.1 加入。

来源：[Phigros Wiki 中文·商店](https://phigros.fandom.com/zh/wiki/%E5%95%86%E5%BA%97)、[Phigros Wiki 中文·游戏内容](https://phigros.fandom.com/zh/wiki/%E6%B8%B8%E6%88%8F%E5%86%85%E5%AE%B9)、[Phigros Wiki EN·Data](https://phigros.fandom.com/wiki/Data)、[EN·Collection](https://phigros.fandom.com/wiki/Collection)、[EN·Version History](https://phigros.fandom.com/wiki/Version_History)

### 1.9 其它影响游玩的机制

- **谱面镜像（Mirror）**：2.4.0 加入，将谱面左右翻转，按钮位于难度下方；用于解手癖或换手底力。镜像成绩是否计入 rks **无官方说明**（见第 5 节）。
- **判定偏移 / 谱面延时**：设置中可调，用于对齐音画；**打击音效延时不可调**（Android 端建议关闭打击音效）。取值范围自 1.0.1 起为 **−400 ms ~ 600 ms**；官方多次因音频系统改动要求玩家重新校准。
- **多押提示**：开启后需同时击打的音符边缘变黄（英文选项 `Highlight simul. notes`）；2.0.0 起 Hold 也显示多押提示。常见运指：Tap+Tap / Tap+Hold / Hold+Hold 用两指起手、持续只需一指；Tap+Flick、Hold+Flick 可单指「点完顺势擦」；Flick+Flick 通常双指；Drag 与任何音符重叠均可单指兼顾。
- **其它设置项**：按键缩放、背景亮度（可全关）、打击音效与音量、FC/AP 指示器、低分辨率模式（2.0.1）、游玩时锁定屏幕方向（2.0.0）。
- **愚人节 / 限时谱面**：4/1 等活动出现 SP 谱，活动后移除；**2024 年愚人节结算用过临时公式**：

```
Score = 800000 × √( min(Perfect, Good) / max(Perfect, Good) ) + 200000 × (Perfect + Good) / 总物量
```

  引用该公式时必须标注「限 2024 愚人节活动」，它不是通用规则。

来源：[Phigros Wiki EN·Game Mechanics](https://phigros.fandom.com/wiki/Game_Mechanics)、[EN·Version History](https://phigros.fandom.com/wiki/Version_History)、[Phigros Wiki*（日）·howtoplay](https://wikiwiki.jp/phigros/howtoplay)

---

## 2. 谱面格式总览

| 格式 | 载体 | 状态 | 说明 |
|---|---|---|---|
| **Phigros Official** | JSON（`.json`） | 官谱在用 | 游戏本体读取的格式，事件按判定线聚合 |
| **RPE** | JSON（`.json` / 打包分发） | 自制谱主流 | Re:PhiEdit 格式，事件分层 + 扩展事件 + 控制曲线 |
| **PEC** | 纯文本（`.pec`） | 已停止更新 | PhiEditer 格式，最大 30 条判定线，不支持 XY 分离 |
| **PhiCommonChart** | JSON / protobuf | 模拟器互通层 | NRLT 定义的公共格式，用于模拟器之间交换 |

- 格式推断（Phira 实现口径）：优先看 `info.yml` 的 `format` 字段，为空则按文件内容推断；**忽略文件后缀名**。

来源：[Phira Documents·谱面文件格式](https://teamflos.github.io/phira-docs/chart-standard/chart-format/index.html)

### ⚠️ 第一号陷阱：四套「音符类型」枚举互不相同

| 类型 | Official | RPE | PEC | PhiCommonChart |
|---|---|---|---|---|
| Tap | **1** | **1** | **1** | **0** |
| Drag | **2** | **4** | **4** | **3** |
| Hold | **3** | **2** | **2** | **1** |
| Flick | **4** | **3** | **3** | **2** |

按 Official 的枚举去解析 RPE 谱面，会把 Hold 读成 Drag、把 Drag 读成 Hold——错了也不会报错，只会安静地判错。

---

## 3. Phigros Official 格式

### 3.1 根结构

| 字段 | 类型 | 说明 |
|---|---|---|
| `formatVersion` | int | 格式版本，影响判定线移动事件的坐标解析；已知取值 `1`、`3` 及其它 |
| `offset` | float | 谱面延迟，单位**秒**；非负时音乐立即开始、谱面延后 `abs(offset)` 秒 |
| `judgeLineList` | JsonArray | 判定线数组 |

- 旧字段 `numOfNotes`（音符总数）在 **v2.5.0 起移除**，改为实时计算。

### 3.2 判定线

| 字段 | 类型 | 说明 |
|---|---|---|
| `bpm` | float | 该判定线的 BPM，决定时间单位 T |
| `notesAbove` | JsonArray | 正面下落的音符 |
| `notesBelow` | JsonArray | 反面下落的音符 |
| `speedEvents` | JsonArray | 流速事件 |
| `judgeLineMoveEvents` | JsonArray | 移动事件 |
| `judgeLineRotateEvents` | JsonArray | 旋转事件 |
| `judgeLineDisappearEvents` | JsonArray | 不透明度（消失）事件 |

- 旧字段 `numOfNotes` / `numOfNotesAbove` / `numOfNotesBelow` 同样在 v2.5.0 移除，读取时不使用。

### 3.3 音符

| 字段 | 类型 | 说明 |
|---|---|---|
| `type` | int | 1=Tap、2=Drag、3=Hold、4=Flick；**其它值表现为不可见也不可判定** |
| `time` | int | 判定时刻，单位 T |
| `positionX` | float | 距判定线中心的水平位置，单位 X |
| `holdTime` | int | Hold 持续时间，单位 T（**按整数读取**，即使写作 `x.0`）；非 Hold 恒为 0；Hold 该值为 0 时不可见 |
| `speed` | float | 速度倍率；Hold 头部倍率恒为 1，该值表示打击时尾部的倍率 |
| `floorPosition` | float | 判定时距判定线的高度，单位 Y（**仅为方便计算，游戏不直接信任**） |

### 3.4 单位与坐标系

- **X 单位** = `0.05625 × 屏幕宽`（1920×1080 下 108 px）。
- **Y 单位** = `0.6 × 屏幕高`（1920×1080 下 648 px）。
- **T 单位** = `1.875 / BPM` 秒（即 128 分音符）。

`formatVersion` 对应的移动事件坐标解码：

| 值 | 原点 | 右上角坐标 | 事件字段含义 |
|---|---|---|---|
| `1` | 左下 | (880, 520) | `start`/`end` = `1000x + y`（整数编码） |
| `3` | 左下 | (1, 1) | `start`/`end` = x，`start2`/`end2` = y |
| 其它 | **画面中心** | — | 两轴单位均为 `0.1 × 屏幕高`；`start`/`end` = x，`start2`/`end2` = y |

v1 → v3 的坐标换算：

```python
ne.start  = (e.start - e.start % 1000) // 1000   # x
ne.start2 =  e.start % 1000                      # y
ne.end    = (e.end   - e.end   % 1000) // 1000
ne.end2   =  e.end   % 1000
```

### 3.5 事件字段

| 事件 | 字段 | 说明 |
|---|---|---|
| `speedEvent` | `startTime` / `endTime` / `value` / `floorPosition` | 时间单位 T；`value` 单位 Y；`floorPosition` 高版本已不存在（游戏实时重算，不读原值） |
| `judgeLineMoveEvent` | v1：`start` / `end`；v3：`start` / `end` / `start2` / `end2` | 见 3.4 解码表 |
| `judgeLineRotateEvent` | `startTime` / `endTime` / `start` / `end` | 角度 |
| `judgeLineDisappearEvent` | `startTime` / `endTime` / `start` / `end` | 不透明度 |

流速事件的 `floorPosition` 递推（第 k 个事件）：

```
p₁ = t₁ × 1.875 / BPM = 0
p_k = p_{k−1} + v_{k−1} × (t_k − t_{k−1}) × 1.875 / BPM      (k ≥ 2)
```

### 3.6 事件列表规范性（不满足会导致谱面停顿/播放异常）

1. 按 `startTime` 升序。
2. 流速事件列表第一个事件的 `startTime` 必须为 `0`。
3. 其它事件列表第一个事件的 `startTime` 应为一个足够小的数（如 `-999999`）。
4. 每个事件的 `startTime` 应等于上一事件的 `endTime`（紧接）。
5. 最后一个事件的 `endTime` 应为足够大的数（如 `1000000000`）。

实测会导致停顿的其它条件（v2.3.1）：判定线数量 > 100；任一判定线 `bpm` ≤ 0；任一事件列表为空数组；谱面运行时刻超过任一事件列表所有事件的结束时刻。表现均为「音符不再垂直移动、该线及其后的动画中断」。

### 3.7 实测解析细节（v2.3.1，实现兼容时有用）

- **重复 JSON 键：前者生效**（`{"bpm":120,"bpm":60}` ≡ `{"bpm":120}`），与 JS `JSON.parse` 不同。
- 浮点精度为 `float` 而非 `double`；v2.5.0 前记录精度同 JS（`2.2` → `2.200000047683716`），v2.5.0 起尽量压缩（`29.217391967773439` → `29.217392`）。
- 数字支持科学计数法，正则参考：`/^-?(0|[1-9]\d*)(\.\d+)?([Ee][+-]?\d+)?$/`。
- 所有字段非必需，缺省取该类型空值；`int` 强制截断取整（非四舍五入），如 `"3.9Music"` → `3`。
- 音符不可见条件：`speed × currentFloorPosition > 3.3333336`（≈ 2 倍屏幕高，float32 边界很敏感）。
- Hold 长度为 0（`speed = 0` 或取整后的 `holdTime = 0`）时不渲染。
- 判定线不透明度叠加公式：`a = a₁ + a₂ − a₁a₂`。
- 谱面加载耗时随音符数近似 O(n²)：16384 → 0.25s；32768 → 1.50s；65536 → 6.00s；131072 → 30.25s；262144 → 159.75s。
- 右上角分数显示：实时分数 ≥ 1000000 恒显 `1000000`；其余情况先 `+0.5` 再取整后按 `0` + 五位小数去点拼接。

来源：[Lchzh Docs·Phigros 谱面格式说明](https://docs.lchzh.top/learning/phigros/)、[Lchzh Docs·相关计算](https://docs.lchzh.top/learning/phigros/calc)、[Lchzh Docs·实测数据](https://docs.lchzh.top/learning/phigros/metrics)、[Phira Documents·Phigros Official](https://teamflos.github.io/phira-docs/chart-standard/chart-format/phi/root.html)

---

## 4. RPE 与 PEC 格式

### 4.1 RPE 根结构

| 字段 | 类型 | 说明 |
|---|---|---|
| `BPMList` | JsonArray | `[{ bpm, startTime(beat) }]`，多 BPM 支持 |
| `META` | JsonObject | `RPEVersion`(100~160)、`offset`(**毫秒**)、`name`、`id`(string)、`song`、`background`、`composer`、`charter`、`illustration`、`level` |
| `chartTime` | double | 谱面编辑时长（秒），141 加入；模拟器不需要 |
| `judgeLineGroup` | string[] | 判定线组；模拟器不需要 |
| `judgeLineList` | JsonArray | 判定线数组 |
| `multiLineString` | string | 多线编辑选择串（如 `1:20`、`all`）；模拟器不需要 |
| `multiScale` / `timeTags` / `xybind` | — | 编辑器辅助字段；模拟器不需要 |

- `META.offset` 符号语义：负 → 音乐在谱面开始前 `-offset` ms 播放；正 → 谱面开始后 `offset` ms 播放。
- RPE 1.5.0 ~ 1.6.0（不含 1.6.0）`RPEVersion` 固定写 `150`；1.6.1 固定写 `160`——**不能拿它当版本判据**。

### 4.2 RPE 判定线

| 字段 | 默认 | 说明 |
|---|---|---|
| `Group` / `Name` | 0 / Untitled | 组与名称 |
| `Texture` | `line.png` | 相对谱面根目录的纹理路径 |
| `anchor` | `[0.5, 0.5]` | 纹理锚点（142 加入），也影响文字事件位置 |
| `eventLayers` | ≥1，最多 5 | 每层含 `speedEvents` / `moveXEvents` / `moveYEvents` / `rotateEvents` / `alphaEvents` |
| `extended` | — | 特殊事件层（第 5 层，故事板） |
| `father` | -1 | 父线索引，允许嵌套 |
| `rotateWithFather` | true | 子线是否继承父线旋转；字段缺失时按 **false** 兼容 163 以前版本 |
| `isCover` | 1 | 为 `1` 时遮罩：判定线背面的音符不渲染（音符 `Above` 不为 1 即为正面） |
| `notes` | — | 音符数组 |
| `numOfNotes` | 0 | 音符总数（**含 FakeNote，不含 Hold**） |
| `zOrder` | 0 | 图层，约 ±100 |
| `attachUI` | 无 | 绑定 UI（`pause`/`combonumber`/`combo`/`score`/`bar`/`name`/`level`）；绑定后判定线自动隐藏，实际位置不变 |
| `isGif` | false | 纹理是否为 GIF（150 加入） |
| `posControl` / `sizeControl` / `skewControl` / `yControl` / `alphaControl` | — | 按「距判定线的纵向距离 x」为关键帧控制 note 参数 |
| `bpmfactor` | 1.0 | 线 BPM = `当前谱面 BPM / bpmfactor`（**是除不是乘**） |

- **坐标系：原点在屏幕中心，X ∈ [−675, 675]，Y ∈ [−450, 450]。**
- 层为空时的字段存在性随版本变化：早期为 `null`，143 起无字段；所有层都空时 `eventLayers` 字段不出现。

### 4.3 RPE 音符

| 字段 | 默认 | 说明 |
|---|---|---|
| `type` | 1 | **1=Tap、2=Hold、3=Flick、4=Drag** |
| `startTime` / `endTime` | — | beat 类型；非 Hold 时二者相同 |
| `positionX` | — | 相对判定线中心的 X |
| `above` | 1 | `1` 从判定线正面下落，其它值从背面 |
| `alpha` | 255 | 0~255 |
| `isFake` | 0 | `1` 为假音符：无判定、无特效、无音效、不计分、不计物量；Hold 假音符始终显示未打击样式 |
| `speed` | 1.0 | 流速倍率 |
| `size` | 1.0 | 实际只控制**宽度**而非整体大小 |
| `visibleTime` | 999999 | 可见时间（秒） |
| `yOffset` | 0.0 | Y 偏移，实际偏移量为 `yOffset × speed`；`speed = 0` 时无效 |
| `hitsound` | 无 | 自定义打击音路径（142 加入） |
| `judgeArea` | 1.0 | 判定区宽度倍率（170 加入） |
| `tint` / `color` | [255,255,255] | 顶点色乘法染色；`color` 为旧名，两字段需兼容 |
| `tintHitEffects` | — | 打击特效染色（170 加入），出现时 Good/Perfect 均用该色 |

### 4.4 RPE 事件与缓动

普通事件字段：`bezier`、`bezierPoints[4]`、`easingLeft`、`easingRight`、`easingType`、`linkgroup`、`start`、`end`、`startTime`、`endTime`。时间单位为 **beat = `int[3]`，显示为 `[0]:[1]/[2]`**：

```
beat     = b[0] + b[1] / b[2]
seconds  = 60 / BPM × beat
```

`easingType` 对照（1~29）：

| 值 | 缓动 | 值 | 缓动 | 值 | 缓动 |
|---|---|---|---|---|---|
| 1 | Linear | 11 | In Quart | 21 | In Back |
| 2 | Out Sine | 12 | In Out Cubic | 22 | In Out Circ |
| 3 | In Sine | 13 | In Out Quart | 23 | In Out Back |
| 4 | Out Quad | 14 | Out Quint | 24 | Out Elastic |
| 5 | In Quad | 15 | In Quint | 25 | In Elastic |
| 6 | In Out Sine | 16 | Out Expo | 26 | Out Bounce |
| 7 | In Out Quad | 17 | In Expo | 27 | In Bounce |
| 8 | Out Cubic | 18 | Out Circ | 28 | In Out Bounce |
| 9 | In Cubic | 19 | In Circ | 29 | In Out Elastic（速度事件不可用） |
| 10 | Out Quart | 20 | Out Back | | |

- Alpha 事件正常范围 0~255；**负数会同时隐藏判定线与其上所有音符**（作者称为已废弃的非法功能，但仍然有效）。
- 流速事件：162 起支持缓动字段但**不支持贝塞尔**；`start`/`end` 缓动语义在 RPE 版本间变过（1.7.0 回归「缓动速度数值」）；流速为负时音符向上飞，Hold 行为与游戏本体不符。
- 特殊事件（`extended`）：`colorEvents`、`scaleXEvents`、`scaleYEvents`、`textEvents`（含 `%P%` 可动态变化数字；有文字事件的判定线始终隐藏并清除自定义纹理）、`paintEvents`（143 起被 shader 取代）、`gifEvents`（150 加入，控制 GIF 播放进度，0.0~1.0）、`inclineEvents`（疑似弃用）。
- 控制曲线（`*Control`）以「距判定线的纵向距离 x」为关键帧轴：`alphaControl` 与 `alpha` 相乘（`noteAlpha × nowAlpha`）；`sizeControl` 真正改大小但**对 Hold 无效**；`posControl` 控制 `positionX` 倍率且**对 Hold 无效**；`skewControl` 对 Hold 无效；`yControl` 行为待补充。

来源：[Phira Documents·RPE 判定线](https://teamflos.github.io/phira-docs/chart-standard/chart-format/rpe/judgeLine.html)、[音符](https://teamflos.github.io/phira-docs/chart-standard/chart-format/rpe/note.html)、[普通事件](https://teamflos.github.io/phira-docs/chart-standard/chart-format/rpe/event.html)、[特殊事件](https://teamflos.github.io/phira-docs/chart-standard/chart-format/rpe/extendEvent.html)、[扩展参数](https://teamflos.github.io/phira-docs/chart-standard/chart-format/rpe/extend.html)、[Controls](https://teamflos.github.io/phira-docs/chart-standard/chart-format/rpe/controls.html)

### 4.5 PEC 格式（已停止更新）

- 纯文本，**不保存元信息**（曲名/谱师需另处获取）；最多 **30 条判定线**；不支持 XY 分离。
- 第一行为 `offset`，单位毫秒的整数，**实际计算时需减去 175 ms**。
- 坐标系：编辑器中中心为 `0,0`，左下 `(-1024,-700)`，右上 `(1024,700)`，角度**逆时针为正**；文件中中心为 `(1024,700)`，左下 `(0,0)`，右上 `(2048,1400)`，角度**顺时针为正**。
- BPM 行：`bp <拍> <bpm>`，如 `bp 0.000 180.000`。
- 音符行：

```text
n1 0 0.500 -40.000 1 0      # n<类型> <判定线> <打击拍> <X> <是否从下方下落> <是否假note>
# 1.00                       # 速度倍率
& 1.00                       # 宽度倍率

n2 0 0.250 2.000 -320.000 1 0   # Hold：n2 <线> <开始拍> <结束拍> <X> <下落方向> <假note>
```

  下落方向 `2` = 从下方下落，`1` = 从上方；假 note 为 `1`。类型枚举同 RPE（1 Tap / 2 Hold / 3 Flick / 4 Drag）。

- 事件：瞬时 `cv`（流速，不支持缓动）/ `cp`（移动）/ `cd`（旋转）/ `ca`（不透明度）；缓动 `cm`（移动）/ `cr`（旋转）/ `cf`（不透明度，始终线性，无缓动类型）。缓动类型编号与 RPE 一致。
- 流速默认 `10.000`；不透明度为 int，`0` 全透明、`255` 不透明、**`-1` 隐藏该判定线上所有音符**。

来源：[Phira Documents·PE 文档](https://teamflos.github.io/phira-docs/chart-standard/chart-format/pe/index.html)、[基本信息](https://teamflos.github.io/phira-docs/chart-standard/chart-format/pe/basic.html)、[音符](https://teamflos.github.io/phira-docs/chart-standard/chart-format/pe/note.html)、[事件](https://teamflos.github.io/phira-docs/chart-standard/chart-format/pe/event.html)

### 4.6 PhiCommonChart（模拟器互通层）

- 事件只做**线性**变化，设计上不含缓动；事件同时带 `StartBeat/EndBeat` 与 `StartTime/EndTime`（毫秒），可任选其一。
- 判定线字段：`TextureData` / `IsGifTexture` / `XMoveEvents` / `YMoveEvents` / `RotateEvents` / `AlphaEvents` / `SpeedEvents` / `Notes` / `FatherIndex` / `RotateWithFather` / `IsCover` / `AttachUi` / `Anchor` / `BpmFactor` / `ZOrder` / `ExtendedEvents` / `TexturePath`。
- 事件默认值：XMove 0.0、YMove 0.0、Rotate 0.0、**Alpha 0**、Speed 10.0（注意 Alpha 缺省为 0，与 RPE 的 255 不同）。
- 音符枚举与 Official/RPE 都不同：Tap=0、Hold=1、Flick=2、Drag=3（见第 2 节陷阱表）。
- 父线位置需按父线角度旋转偏移量后再相加；`RotateWithFather` 为 true 时角度相加。

来源：[PhiCommonChartDocs·判定线](https://docs.nuanr-mxi.com/chart_format/judge_line.html)、[事件](https://docs.nuanr-mxi.com/chart_format/event.html)、[音符](https://docs.nuanr-mxi.com/chart_format/note.html)

---

## 5. 存疑与未核实

1. **Bad 判定区间分段冲突**：中文 Fandom 写「±161~180 ms」，英文 Fandom 与日文 Wiki 写「±180 ms」（总跨度一致，分段写法不同，英/日的「边界」写法更可信）；**课题模式 Bad**：中文同页自相矛盾（先 ±75~90 ms、后 ±140 ms），英/日均为 **±140 ms**，采信 ±140。实现建议：`Perfect ±80 / Good ±160 / Bad ±180（课题 ±40 / ±75 / ±140）`，并留可配置余量。
2. **「总 rks 第 20 张折半 ×0.5」不成立**——中/英/日 Wiki 与 3.11.0 官方规则说明均无此说法；3.11.0 前的正确公式是 `(Best19 + 1 张最高定数 φ 谱) / 20`（第 20 位是**满值 φ 加成位**，不折半）。这是流传最广的错误记忆，疑与第三方查分站的旧加权算法混淆。
3. **判定窗口的历代版本差异无据可证**：现存权威表格只区分「普通 / 课题」两套值；1.0.1 的「0~500ms → −400~600ms」改的是**谱面延时**而非判定窗口。不要断言早期版本用过不同窗口。
4. **定数上限与理论最高 rks 随版本漂移**：16.80（中文 3.11.0.1）/ 17.04（日文 3.19.4）/ 17.05（英文 3.20.0），区间上限 17.6。这不是来源冲突而是版本差异，引用必须带版本号。
5. **「通关（Cleared）」基准不一致**：日文 Wiki 明确记载章节选择界面按 C 以上统计、个人资料按 B 以上统计；中文 Wiki 称 AT 难度需 B。采信「C 通关 / AT 需 B」，但需知界面间存在偏差。
6. **单曲精选集解锁范围表述冲突**：中文 Wiki 写「rks ≤ 11 时可一次性解锁 EZ/HD/IN」，英文 Wiki 写「rks ≥ 11」。方向矛盾，疑中文笔误；实质一致（rks 达 11 开放更多 IN）。
7. **Legacy 相关**：绝大多数 Legacy 谱的移除版本有「3.0.0」与「2.4.1」两种记载；进入方式的中文记 15 秒、日文记 5 秒。
8. **逐曲特殊解锁条件**未逐条核实（`Crave Wave`、`The Chariot ~REVIIVAL~`、`Retribution`、`Luminescence`、`Distorted Fate`、`Rrhar'il`、`Igallta`、`Spasmodic`，以及 `You are the Miserable` / `Stasis` / `Shadow` / `DESTRUCTION 3,2,1` 的 AT，仅确证「存在特殊条件」；Fandom 歌曲页由模板生成，wikitext 中不含解锁段落）。
9. **谱面镜像（Mirror）成绩是否计入 rks**：官方更新日志只写 2.4.0「Added Chart Mirror feature」，无计入规则说明；仅见玩家帖推测。**来源弱，未证实。**
10. **联网/离线与云存档细节未证实**：仅确认 1.6.10 账号系统、2.4.0 云存档、3.13.0 TapTap 需中国身份证验证；离线可玩范围、云存档冲突覆盖规则、本地存档位置与备份方式均无权威描述。
11. **Official 格式 `judgeLineDisappearEvents` 的取值域**（0~1 还是 0~255）在本次取证的三份文档中表述不一致，实现前建议对官谱实测。
12. 本文件**不含任何官方文档背书**：Phigros 官方未发布公开的规则/谱面格式规范，以上全部来自社区 Wiki、模拟器文档与实测记录；关键数值建议在目标版本上实测复核。

---

## 6. 来源清单

- [Phigros Wiki 中文（Fandom）·游戏内容](https://phigros.fandom.com/zh/wiki/%E6%B8%B8%E6%88%8F%E5%86%85%E5%AE%B9)（玩法、判定、结算、评级、解锁）
- [Phigros Wiki 中文·定数](https://phigros.fandom.com/zh/wiki/%E5%AE%9A%E6%95%B0)（单曲 rks / 总 rks 公式）
- [Phigros Wiki 中文·难度](https://phigros.fandom.com/zh/wiki/%E9%9A%BE%E5%BA%A6)、[章节列表](https://phigros.fandom.com/zh/wiki/%E7%AB%A0%E8%8A%82%E5%88%97%E8%A1%A8)、[商店](https://phigros.fandom.com/zh/wiki/%E5%95%86%E5%BA%97)
- [Phigros Wiki（英）Game Mechanics](https://phigros.fandom.com/wiki/Game_Mechanics)、[Data](https://phigros.fandom.com/wiki/Data)、[Collection](https://phigros.fandom.com/wiki/Collection)、[Version History](https://phigros.fandom.com/wiki/Version_History)
- [Phigros Wiki*（日）·howtoplay](https://wikiwiki.jp/phigros/howtoplay)、[課題モード](https://wikiwiki.jp/phigros/%E8%AA%B2%E9%A1%8C%E3%83%A2%E3%83%BC%E3%83%89)（判定窗口、运指、设置项的交叉验证）
- [TapTap·3.11.0 Rks 规则变动说明](https://www.taptap.cn/moment/640657705018589989)（Best19+b0 → Best27+Phi3 的官方口径）
- [Lchzh Docs·Phigros 谱面格式说明](https://docs.lchzh.top/learning/phigros/) / [相关计算](https://docs.lchzh.top/learning/phigros/calc) / [实测数据](https://docs.lchzh.top/learning/phigros/metrics)（Official 格式与实测行为）
- [Phira Documents·谱面文件格式](https://teamflos.github.io/phira-docs/chart-standard/chart-format/index.html)（Official / RPE / PEC 规范）
- [PhiCommonChartDocs](https://docs.nuanr-mxi.com/chart_format/judge_line.html)（模拟器互通格式）
- [维基百科·Phigros](https://zh.wikipedia.org/wiki/Phigros)（游戏背景与音符种类概述）
