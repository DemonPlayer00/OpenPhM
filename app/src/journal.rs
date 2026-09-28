//! 更改日志（记录更改模式的撤销/重做）。
//!
//! 与"快照式"的区别：**内存随改动量增长，而不是随文档大小增长**。
//! 旧实现每执行一条命令就 `doc.clone()` —— 20 万音符的谱面每改一次克隆整份文档，代价 O(文档)。
//! 现在每个 [`Change`] 精确记录"改了哪一处、改前是什么、改后是什么"，逆操作即还原，代价 O(改动)。
//!
//! 另外，Change 是可序列化的 ⇒ 日志可导出成补丁（`opm-ctl --journal`），便于 agent 之间交接与复核。

use serde::{Deserialize, Serialize};

use crate::doc::{BpmEntry, Document, Event, JudgeLine, Layer, Meta, Note};

/// 判定线可变属性（只记录会被 `set_line` 改到的部分，避免整线克隆）
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LineProps {
    pub name: String,
    pub bpm_factor: f32,
    pub z_order: i32,
    pub is_cover: bool,
}

impl LineProps {
    pub fn of(l: &JudgeLine) -> Self {
        Self {
            name: l.name.clone(),
            bpm_factor: l.bpm_factor,
            z_order: l.z_order,
            is_cover: l.is_cover,
        }
    }
    pub fn apply_to(&self, l: &mut JudgeLine) {
        l.name = self.name.clone();
        l.bpm_factor = self.bpm_factor;
        l.z_order = self.z_order;
        l.is_cover = self.is_cover;
    }
}

/// 轨道标识（日志里必须可读，故用字符串而不是下标）。
///
/// **不另立一份表**：与 `doc::TRACKS` 是同一件事 —— 两份手抄的轨道名会在加轨道那天分家，
/// 而那时报错里列的"可选 track"还是旧的那几条。
pub use crate::doc::TRACKS;

fn track_of<'a>(layer: &'a Layer, track: &str) -> Option<&'a Vec<Event>> {
    layer.track(track)
}
fn track_of_mut<'a>(layer: &'a mut Layer, track: &str) -> Option<&'a mut Vec<Event>> {
    layer.track_mut(track)
}

/// 一条可逆改动。每个变体的 `apply`/`revert` 互为逆操作。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum Change {
    SetMeta {
        before: Box<Meta>,
        after: Box<Meta>,
    },
    SetBpm {
        index: usize,
        before: Box<BpmEntry>,
        after: Box<BpmEntry>,
    },
    InsertLine {
        index: usize,
        line: Box<JudgeLine>,
    },
    RemoveLine {
        index: usize,
        line: Box<JudgeLine>,
    },
    SetLine {
        index: usize,
        before: Box<LineProps>,
        after: Box<LineProps>,
    },
    InsertNote {
        line: usize,
        index: usize,
        note: Box<Note>,
    },
    RemoveNote {
        line: usize,
        index: usize,
        note: Box<Note>,
    },
    SetNote {
        line: usize,
        index: usize,
        before: Box<Note>,
        after: Box<Note>,
    },
    /// 整线批量改动（如 `move_notes` 平移）：只记录受影响的音符数组
    NotesOfLine {
        line: usize,
        before: Vec<Note>,
        after: Vec<Note>,
    },
    InsertEvent {
        line: usize,
        layer: usize,
        track: String,
        index: usize,
        event: Box<Event>,
    },
    RemoveEvent {
        line: usize,
        layer: usize,
        track: String,
        index: usize,
        event: Box<Event>,
    },
    SetEvent {
        line: usize,
        layer: usize,
        track: String,
        index: usize,
        before: Box<Event>,
        after: Box<Event>,
    },
    /// 整条轨道替换（`split_event` / `set_track_constant` / `normalize` 的最小粒度）
    ReplaceTrack {
        line: usize,
        layer: usize,
        track: String,
        before: Vec<Event>,
        after: Vec<Event>,
    },
    /// 事务：多条改动作为一次撤销单元（agent 的一次批量提交 = 一次撤销）
    Transaction {
        label: String,
        changes: Vec<Change>,
    },
}

impl Change {
    /// 这条改动落到**哪条判定线**上（`None` = 说不清或整表：元信息、BPM、增删线、事务）。
    ///
    /// 用途之一是"只重查动过的那条线"的重叠检测（见 `EditCore::refresh_overlaps`）：
    /// 改 3 号线的音符不该让 4 号线的检测结果重算一遍。
    pub fn line(&self) -> Option<usize> {
        match self {
            Change::SetLine { index, .. } => Some(*index),
            Change::InsertNote { line, .. }
            | Change::RemoveNote { line, .. }
            | Change::SetNote { line, .. }
            | Change::NotesOfLine { line, .. }
            | Change::InsertEvent { line, .. }
            | Change::RemoveEvent { line, .. }
            | Change::SetEvent { line, .. }
            | Change::ReplaceTrack { line, .. } => Some(*line),
            // 元信息 / BPM / 增删线（下标会整体挪动）/ 事务（自己的 changes 各自会说话）
            Change::SetMeta { .. }
            | Change::SetBpm { .. }
            | Change::InsertLine { .. }
            | Change::RemoveLine { .. }
            | Change::Transaction { .. } => None,
        }
    }

    /// 粗略内存占用（用于给日志设字节上限）
    pub fn approx_bytes(&self) -> usize {
        let ev = std::mem::size_of::<Event>();
        let note = std::mem::size_of::<Note>();
        match self {
            Change::SetMeta { .. } => 256,
            Change::SetBpm { .. } => 128,
            Change::InsertLine { line, .. } | Change::RemoveLine { line, .. } => {
                note * line.notes.len() + ev * 16 + 512
            }
            Change::SetLine { .. } => 128,
            Change::InsertNote { .. } | Change::RemoveNote { .. } | Change::SetNote { .. } => note,
            Change::NotesOfLine { before, after, .. } => note * (before.len() + after.len()),
            Change::InsertEvent { .. } | Change::RemoveEvent { .. } | Change::SetEvent { .. } => ev,
            Change::ReplaceTrack { before, after, .. } => ev * (before.len() + after.len()),
            Change::Transaction { changes, .. } => {
                64 + changes.iter().map(Change::approx_bytes).sum::<usize>()
            }
        }
    }

    /// 观察用标签（日志/补丁可读）
    pub fn label(&self) -> String {
        match self {
            Change::SetMeta { .. } => "set_meta".into(),
            Change::SetBpm { index, .. } => format!("set_bpm[{index}]"),
            Change::InsertLine { index, .. } => format!("add_line[{index}]"),
            Change::RemoveLine { index, .. } => format!("del_line[{index}]"),
            Change::SetLine { index, .. } => format!("set_line[{index}]"),
            Change::InsertNote { line, index, .. } => format!("add_note[{line}:{index}]"),
            Change::RemoveNote { line, index, .. } => format!("del_note[{line}:{index}]"),
            Change::SetNote { line, index, .. } => format!("set_note[{line}:{index}]"),
            Change::NotesOfLine { line, .. } => format!("notes_of_line[{line}]"),
            Change::InsertEvent { line, track, index, .. } => format!("add_event[{line}:{track}:{index}]"),
            Change::RemoveEvent { line, track, index, .. } => format!("del_event[{line}:{track}:{index}]"),
            Change::SetEvent { line, track, index, .. } => format!("set_event[{line}:{track}:{index}]"),
            Change::ReplaceTrack { line, track, .. } => format!("track[{line}:{track}]"),
            Change::Transaction { label, changes } => format!("{label}({} 条)", changes.len()),
        }
    }

    pub(crate) fn apply(&self, doc: &mut Document) -> Result<(), String> {
        match self {
            Change::SetMeta { after, .. } => doc.meta = (**after).clone(),
            Change::SetBpm { index, after, .. } => {
                *doc.bpm_list.get_mut(*index).ok_or("bpm 下标越界")? = (**after).clone();
            }
            Change::InsertLine { index, line } => {
                let i = (*index).min(doc.judge_lines.len());
                doc.judge_lines.insert(i, (**line).clone());
            }
            Change::RemoveLine { index, .. } => {
                if *index >= doc.judge_lines.len() {
                    return Err(format!("判定线下标 {index} 越界"));
                }
                doc.judge_lines.remove(*index);
            }
            Change::SetLine { index, after, .. } => {
                after.apply_to(doc.judge_lines.get_mut(*index).ok_or("判定线下标越界")?);
            }
            Change::InsertNote { line, index, note } => {
                let l = doc.judge_lines.get_mut(*line).ok_or("判定线下标越界")?;
                let i = (*index).min(l.notes.len());
                l.notes.insert(i, (**note).clone());
            }
            Change::RemoveNote { line, index, .. } => {
                let l = doc.judge_lines.get_mut(*line).ok_or("判定线下标越界")?;
                if *index >= l.notes.len() {
                    return Err(format!("音符下标 {index} 越界"));
                }
                l.notes.remove(*index);
            }
            Change::SetNote { line, index, after, .. } => {
                let l = doc.judge_lines.get_mut(*line).ok_or("判定线下标越界")?;
                *l.notes.get_mut(*index).ok_or("音符下标越界")? = (**after).clone();
            }
            Change::NotesOfLine { line, after, .. } => {
                let l = doc.judge_lines.get_mut(*line).ok_or("判定线下标越界")?;
                l.notes = after.clone();
            }
            Change::InsertEvent { line, layer, track, index, event } => {
                let t = get_track_mut(doc, *line, *layer, track)?;
                let i = (*index).min(t.len());
                t.insert(i, (**event).clone());
            }
            Change::RemoveEvent { line, layer, track, index, .. } => {
                let t = get_track_mut(doc, *line, *layer, track)?;
                if *index >= t.len() {
                    return Err(format!("事件下标 {index} 越界"));
                }
                t.remove(*index);
            }
            Change::SetEvent { line, layer, track, index, after, .. } => {
                let t = get_track_mut(doc, *line, *layer, track)?;
                *t.get_mut(*index).ok_or("事件下标越界")? = (**after).clone();
            }
            Change::ReplaceTrack { line, layer, track, after, .. } => {
                let t = get_track_mut(doc, *line, *layer, track)?;
                *t = after.clone();
            }
            Change::Transaction { changes, .. } => {
                for c in changes {
                    c.apply(doc)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn revert(&self, doc: &mut Document) -> Result<(), String> {
        match self {
            Change::SetMeta { before, .. } => doc.meta = (**before).clone(),
            Change::SetBpm { index, before, .. } => {
                *doc.bpm_list.get_mut(*index).ok_or("bpm 下标越界")? = (**before).clone();
            }
            Change::InsertLine { index, .. } => {
                if *index >= doc.judge_lines.len() {
                    return Err(format!("判定线下标 {index} 越界"));
                }
                doc.judge_lines.remove(*index);
            }
            Change::RemoveLine { index, line } => {
                let i = (*index).min(doc.judge_lines.len());
                doc.judge_lines.insert(i, (**line).clone());
            }
            Change::SetLine { index, before, .. } => {
                before.apply_to(doc.judge_lines.get_mut(*index).ok_or("判定线下标越界")?);
            }
            Change::InsertNote { line, index, .. } => {
                let l = doc.judge_lines.get_mut(*line).ok_or("判定线下标越界")?;
                if *index >= l.notes.len() {
                    return Err(format!("音符下标 {index} 越界"));
                }
                l.notes.remove(*index);
            }
            Change::RemoveNote { line, index, note } => {
                let l = doc.judge_lines.get_mut(*line).ok_or("判定线下标越界")?;
                let i = (*index).min(l.notes.len());
                l.notes.insert(i, (**note).clone());
            }
            Change::SetNote { line, index, before, .. } => {
                let l = doc.judge_lines.get_mut(*line).ok_or("判定线下标越界")?;
                *l.notes.get_mut(*index).ok_or("音符下标越界")? = (**before).clone();
            }
            Change::NotesOfLine { line, before, .. } => {
                let l = doc.judge_lines.get_mut(*line).ok_or("判定线下标越界")?;
                l.notes = before.clone();
            }
            Change::InsertEvent { line, layer, track, index, .. } => {
                let t = get_track_mut(doc, *line, *layer, track)?;
                if *index >= t.len() {
                    return Err(format!("事件下标 {index} 越界"));
                }
                t.remove(*index);
            }
            Change::RemoveEvent { line, layer, track, index, event } => {
                let t = get_track_mut(doc, *line, *layer, track)?;
                let i = (*index).min(t.len());
                t.insert(i, (**event).clone());
            }
            Change::SetEvent { line, layer, track, index, before, .. } => {
                let t = get_track_mut(doc, *line, *layer, track)?;
                *t.get_mut(*index).ok_or("事件下标越界")? = (**before).clone();
            }
            Change::ReplaceTrack { line, layer, track, before, .. } => {
                let t = get_track_mut(doc, *line, *layer, track)?;
                *t = before.clone();
            }
            Change::Transaction { changes, .. } => {
                // 逆序回滚
                for c in changes.iter().rev() {
                    c.revert(doc)?;
                }
            }
        }
        Ok(())
    }
}

fn get_track_mut<'a>(
    doc: &'a mut Document,
    line: usize,
    layer: usize,
    track: &str,
) -> Result<&'a mut Vec<Event>, String> {
    let l = doc.judge_lines.get_mut(line).ok_or("判定线下标越界")?;
    let lay = l.layers.get_mut(layer).ok_or("层下标越界")?;
    track_of_mut(lay, track).ok_or_else(|| format!("未知轨道 {track}（可选 {TRACKS:?}）"))
}

/// 读取一条轨道（记录 before 用）
pub fn read_track(doc: &Document, line: usize, layer: usize, track: &str) -> Option<Vec<Event>> {
    let events = doc.judge_lines.get(line)?.layers.get(layer).and_then(|l| track_of(l, track))?;
    Some(events.to_vec())
}

/// 更改日志：撤销栈 + 重做栈 + 事务支持 + 字节上限
pub struct Journal {
    undo: Vec<Change>,
    redo: Vec<Change>,
    /// 正在进行的事务（`begin` 之后、`commit` 之前记录的改动都进这里）
    pending: Option<(String, Vec<Change>)>,
    bytes: usize,
    cap_bytes: usize,
    pub applied: u64,
    pub undone: u64,
}

impl Default for Journal {
    fn default() -> Self {
        Self::new(64 * 1024 * 1024) // 64 MiB 上限
    }
}

impl Journal {
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            pending: None,
            bytes: 0,
            cap_bytes,
            applied: 0,
            undone: 0,
        }
    }

    pub fn begin(&mut self, label: &str) {
        if self.pending.is_none() {
            self.pending = Some((label.to_owned(), Vec::new()));
        }
    }

    pub fn in_transaction(&self) -> bool {
        self.pending.is_some()
    }

    /// 放弃当前事务：**把事务里已经落到文档上的改动逐条回滚**。
    ///
    /// 这里曾经只写 `self.pending = None`（丢弃待提交列表就完事）—— 那是错的：
    /// `EditCore::mutate` 是**直接改文档**再把逆操作记进日志的，所以丢掉列表 ≠ 文档回滚，
    /// 结果是"改了但撤不回"（既不在撤销栈里，也没还原）。这个 bug 是给逐条广播写测试时被抓出来的。
    /// 回滚必须**逆序**（后做的先撤）。
    pub(crate) fn abort(&mut self, doc: &mut Document) -> Result<(), String> {
        let Some((_, list)) = self.pending.take() else {
            return Ok(());
        };
        let mut first_err = None;
        for ch in list.iter().rev() {
            if let Err(e) = ch.revert(doc) {
                // 尽力回滚：一条失败不阻止其余条，但把错误带回去（不静默）
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(format!("回滚不完整: {e}")),
            None => Ok(()),
        }
    }

    /// 回滚**当前事务里从第 `n` 条起**的改动，返回退掉了几条（命令失败时用）。
    ///
    /// 为什么不用 [`Self::abort`]：`abort` 退的是**整个事务**，而事务里可能还有前面几条**成功**的
    /// 命令 —— 一条命令失败不该把别人做的好事一起抹掉。`pending` 里的改动还没进撤销栈
    /// （`bytes` 也没计过），所以这里只把文档退回去、把这几条丢掉。
    pub(crate) fn rollback_since(&mut self, doc: &mut Document, n: usize) -> Result<usize, String> {
        let mut first_err = None;
        let mut done = 0usize;
        while self.pending_len() > n {
            let Some((_, list)) = self.pending.as_mut() else { break };
            let Some(change) = list.pop() else { break };
            if let Err(e) = change.revert(doc) {
                // 尽力回滚：一条失败不阻止其余条，但把错误带回去（不静默）
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            done += 1;
        }
        match first_err {
            Some(e) => Err(format!("回滚不完整: {e}")),
            None => Ok(done),
        }
    }

    /// 结束当前事务；若事务内无改动则不产生撤销步
    pub fn commit(&mut self) -> Option<Change> {
        let (label, changes) = self.pending.take()?;
        if changes.is_empty() {
            return None;
        }
        let change = if changes.len() == 1 {
            changes.into_iter().next().unwrap()
        } else {
            Change::Transaction { label, changes }
        };
        self.push(change.clone());
        Some(change)
    }

    /// 记录一条改动（处于事务中则累积）
    pub fn record(&mut self, change: Change) {
        match &mut self.pending {
            Some((_, list)) => list.push(change),
            None => self.push(change),
        }
    }

    fn push(&mut self, change: Change) {
        self.bytes += change.approx_bytes();
        self.redo.clear();
        self.undo.push(change);
        self.applied += 1;
        // 超上限则从最旧开始丢弃（内存有界优先于撤销深度）
        while self.bytes > self.cap_bytes && self.undo.len() > 1 {
            let dropped = self.undo.remove(0);
            self.bytes = self.bytes.saturating_sub(dropped.approx_bytes());
        }
    }

    pub(crate) fn undo(&mut self, doc: &mut Document) -> Result<Option<String>, String> {
        let Some(change) = self.undo.pop() else {
            return Ok(None);
        };
        change.revert(doc)?;
        self.bytes = self.bytes.saturating_sub(change.approx_bytes());
        let label = change.label();
        self.redo.push(change);
        self.undone += 1;
        Ok(Some(label))
    }

    pub(crate) fn redo(&mut self, doc: &mut Document) -> Result<Option<String>, String> {
        let Some(change) = self.redo.pop() else {
            return Ok(None);
        };
        change.apply(doc)?;
        self.bytes += change.approx_bytes();
        let label = change.label();
        self.undo.push(change);
        self.applied += 1;
        Ok(Some(label))
    }

    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }
    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }
    /// 当前事务里累积了多少条改动（广播要拿"本次命令新增的那几条"来推话题）
    pub fn pending_len(&self) -> usize {
        self.pending.as_ref().map(|(_, l)| l.len()).unwrap_or(0)
    }

    /// 取当前事务里从第 `n` 条起的新增改动（克隆）。`n` 用 `pending_len()` 在命令执行前取。
    pub fn pending_since(&self, n: usize) -> Vec<Change> {
        match &self.pending {
            Some((_, list)) => list.iter().skip(n).cloned().collect(),
            None => Vec::new(),
        }
    }

    /// 栈顶改动（只读）：广播需要它来推出话题，而不必改动 undo/redo 的签名
    pub fn undo_top(&self) -> Option<&Change> {
        self.undo.last()
    }
    pub fn redo_top(&self) -> Option<&Change> {
        self.redo.last()
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn cap_bytes(&self) -> usize {
        self.cap_bytes
    }

    /// 导出日志（补丁）：供 agent 交接/复核
    pub fn patch(&self) -> serde_json::Value {
        serde_json::json!({
            "undo": self.undo.iter().map(|c| serde_json::to_value(c).unwrap_or_default()).collect::<Vec<_>>(),
            "redo": self.redo.iter().map(|c| serde_json::to_value(c).unwrap_or_default()).collect::<Vec<_>>(),
            "bytes": self.bytes,
            "applied": self.applied,
            "undone": self.undone,
        })
    }

    /// 人类可读的最近 N 条
    pub fn recent(&self, n: usize) -> Vec<String> {
        self.undo.iter().rev().take(n).map(Change::label).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Document;

    fn meta_named(name: &str) -> Meta {
        let mut d = Document::default();
        d.meta.name = name.to_owned();
        d.meta
    }

    /// `rollback_since`：**只退"这条命令"写进去的**，同一事务里前面成功的命令留着。
    ///
    /// 这正是它与 [`Journal::abort`] 的区别：`abort` 会把整个事务都退掉（那是"用户取消"的语义），
    /// 而"某一条命令失败"不该连累别人 —— `EditCore` 的失败分支用的就是这个。
    #[test]
    fn rollback_since_keeps_earlier_commands_of_the_same_transaction() {
        let mut doc = Document::default();
        let mut j = Journal::default();
        j.begin("事务");

        // 第一条：成功（甲）
        let m0 = doc.meta.clone();
        let m1 = meta_named("甲");
        doc.meta = m1.clone();
        j.record(Change::SetMeta { before: Box::new(m0), after: Box::new(m1.clone()) });

        // 第二条：写进去了，但这条命令随后失败 ⇒ 只退它
        let m2 = meta_named("乙");
        doc.meta = m2.clone();
        j.record(Change::SetMeta { before: Box::new(m1.clone()), after: Box::new(m2) });

        assert_eq!(j.rollback_since(&mut doc, 1).unwrap(), 1, "退掉 1 条");
        assert_eq!(doc.meta.name, "甲", "只退第二条，第一条留着");
        assert_eq!(j.pending_len(), 1, "待提交里只剩第一条");

        // 提交之后，撤销栈里是那条成功的第一步（回滚掉的那条从没进过栈）
        assert!(j.commit().is_some());
        assert_eq!(j.undo_depth(), 1);
        assert_eq!(j.applied, 1, "回滚掉的改动不该计入 applied");

        // 无可退时是 0（不是错误）
        assert_eq!(j.rollback_since(&mut doc, 0).unwrap(), 0);
    }
}
