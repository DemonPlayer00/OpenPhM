//! 更新广播与**细粒度话题**。
//!
//! 设计目标（按用户要求）：
//! 1. **EditCore 是唯一的可写方**。任何改动（本地控制台、远端 attach、撤销/重做）都在核心内完成后
//!    **广播一条 update**，带上"改了哪些话题"与"改了哪些东西"。
//! 2. **GUI 不私自改文档**：它只负责「发命令 → 等广播 → 按话题重建自己那一小块」。
//! 3. **无关控件不参与更新**：订阅时带过滤器，只有话题命中的订阅者才收到广播；
//!    GUI 内部再把话题映射成每个面板的 dirty 位，未脏的面板一帧都不重建（有计数器可验证）。

use std::sync::mpsc::{Receiver, Sender};

use crate::journal::Change;

/// 话题类别：粒度到"哪条线的哪个对象"
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TopicKind {
    /// 谱面元信息（曲名/谱师/难度…）
    Meta,
    /// BPM 表
    Bpm,
    /// 判定线集合本身（增删、顺序）
    LineList,
    /// 单条判定线的属性（name/bpmFactor/zOrder/isCover）
    LineProps,
    /// 某条线上的**音符集合**（增删、批量平移）
    Notes,
    /// 单个音符
    Note,
    /// 某条轨道（事件模式：移动/透明度/流速）
    Track,
}

/// 一个具体话题：类别 + （可选）判定线下标
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Topic {
    pub kind: TopicKind,
    pub line: Option<usize>,
}

impl Topic {
    pub fn new(kind: TopicKind, line: Option<usize>) -> Self {
        Self { kind, line }
    }
    pub fn label(&self) -> String {
        match self.line {
            Some(l) => format!("{:?}[{l}]", self.kind),
            None => format!("{:?}", self.kind),
        }
    }
}

/// 订阅过滤器。`kinds` 为空且 `any == false` 表示不关心任何话题（收不到广播）。
#[derive(Clone, Debug, Default)]
pub struct TopicFilter {
    pub any: bool,
    pub kinds: Vec<TopicKind>,
    /// 为空表示不限定线；否则只收这些线的话题
    pub lines: Vec<usize>,
}

impl TopicFilter {
    /// 全收（渲染层用：几乎什么都影响画面）
    pub fn all() -> Self {
        Self {
            any: true,
            kinds: Vec::new(),
            lines: Vec::new(),
        }
    }
    /// 只关心这些类别（不限线）
    pub fn of(kinds: &[TopicKind]) -> Self {
        Self {
            any: false,
            kinds: kinds.to_vec(),
            lines: Vec::new(),
        }
    }
    /// 只关心这些类别、且限定在这些线上（音符列表按线订阅时用）
    pub fn of_lines(kinds: &[TopicKind], lines: Vec<usize>) -> Self {
        Self {
            any: false,
            kinds: kinds.to_vec(),
            lines,
        }
    }
    pub fn matches(&self, t: &Topic) -> bool {
        if self.any {
            return true;
        }
        if !self.kinds.contains(&t.kind) {
            return false;
        }
        if self.lines.is_empty() {
            return true;
        }
        match t.line {
            Some(l) => self.lines.contains(&l),
            // 不限定线的话题（Meta/Bpm/LineList）对"只关心某条线"的订阅者也算命中
            None => true,
        }
    }
}

/// 改动来源，便于 UI 区分「我自己发起的」与「远端发起的」
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    Local,
    Remote,
    Undo,
    Redo,
}

/// 广播内容：一次改动集合的摘要
#[derive(Clone, Debug)]
pub struct Broadcast {
    pub revision: u64,
    pub origin: Origin,
    pub label: String,
    pub topics: Vec<Topic>,
    /// 人类可读的改动摘要（调试面板/日志用）
    pub changes: Vec<String>,
}

impl Broadcast {
    pub fn summary(&self) -> String {
        let mut kinds: Vec<String> = self.topics.iter().map(Topic::label).collect();
        kinds.sort();
        kinds.dedup();
        format!(
            "#{} {:?} {} → [{}]",
            self.revision,
            self.origin,
            self.label,
            kinds.join(", ")
        )
    }
}

/// 订阅句柄：`Receiver` + 过滤器（过滤器只在订阅时用一次，这里保留便于打印）
pub struct Subscription {
    pub filter: TopicFilter,
    pub rx: Receiver<Broadcast>,
}

/// 由一条改动推出它命中的话题
pub fn topics_of(change: &Change) -> Vec<Topic> {
    use TopicKind::*;
    match change {
        Change::SetMeta { .. } => vec![Topic::new(Meta, None)],
        Change::SetBpm { .. } => vec![Topic::new(Bpm, None)],
        Change::InsertLine { index, .. } => vec![
            Topic::new(LineList, None),
            Topic::new(Notes, Some(*index)),
            Topic::new(Track, Some(*index)),
        ],
        Change::RemoveLine { index, .. } => vec![
            Topic::new(LineList, None),
            Topic::new(Notes, Some(*index)),
            Topic::new(Track, Some(*index)),
        ],
        Change::SetLine { index, .. } => vec![Topic::new(LineProps, Some(*index))],
        Change::InsertNote { line, .. } | Change::RemoveNote { line, .. } => {
            vec![Topic::new(Notes, Some(*line)), Topic::new(Note, Some(*line))]
        }
        Change::SetNote { line, .. } => vec![
            Topic::new(Notes, Some(*line)),
            Topic::new(Note, Some(*line)),
        ],
        Change::NotesOfLine { line, .. } => vec![
            Topic::new(Notes, Some(*line)),
            Topic::new(Note, Some(*line)),
        ],
        Change::InsertEvent { line, .. }
        | Change::RemoveEvent { line, .. }
        | Change::SetEvent { line, .. }
        | Change::ReplaceTrack { line, .. } => {
            // 事件只属于轨道。**不要**在这里附带 `LineProps`：事件不改判定线属性，
            // 而 LineProps 会置脏谱面派生视图（时间映射）—— 实测踩过：多带一个话题，
            // 改一次 alpha 事件仍然重建 200k 音符的展平视图，"无关控件不更新"直接失效。
            vec![Topic::new(Track, Some(*line))]
        }
        Change::Transaction { changes, .. } => {
            let mut out: Vec<Topic> = Vec::new();
            for c in changes {
                for t in topics_of(c) {
                    if !out.contains(&t) {
                        out.push(t);
                    }
                }
            }
            out
        }
    }
}

/// 订阅登记表（EditCore 持有）
#[derive(Default)]
pub struct Subscribers {
    /// `Sender` 的克隆分发给订阅者，这里只留一份
    list: Vec<(TopicFilter, Sender<Broadcast>)>,
}

impl Subscribers {
    pub fn subscribe(&mut self, filter: TopicFilter) -> Subscription {
        let (tx, rx) = std::sync::mpsc::channel();
        self.list.push((filter.clone(), tx));
        Subscription { filter, rx }
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// 只把广播发给**话题命中**的订阅者；**顺带清掉已经断开的订阅**。
    ///
    /// 括号里那句话原先没做到：命中分支无论 `send` 成不成功都 `retain(true)`，
    /// 于是接收端早就没了的订阅会永远留在表里 —— `ui_stats.subscribers` 只涨不落。
    /// 现在的规则是：**话题命中才谈生死**（不命中就不发，也不因此删掉它 —— 它可能还关心别的改动）。
    pub fn emit(&mut self, b: &Broadcast) -> usize {
        let mut delivered = 0;
        self.list.retain(|(filter, tx)| {
            if !b.topics.iter().any(|t| filter.matches(t)) {
                return true; // 不发，也不删
            }
            if tx.send(b.clone()).is_ok() {
                delivered += 1;
                true
            } else {
                false // 接收端已经没了：清掉
            }
        });
        delivered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broadcast(topics: Vec<Topic>) -> Broadcast {
        Broadcast {
            revision: 1,
            origin: Origin::Local,
            label: "test".into(),
            topics,
            changes: Vec::new(),
        }
    }

    /// 断开的订阅必须被清掉（否则表只涨不落）
    #[test]
    fn emit_drops_dead_subscribers() {
        let mut s = Subscribers::default();
        let live = s.subscribe(TopicFilter::all());
        {
            let _dead = s.subscribe(TopicFilter::all());
            assert_eq!(s.len(), 2);
        } // `_dead` 在这里被丢掉 ⇒ 它的接收端没了
        let b = broadcast(vec![Topic::new(TopicKind::Meta, None)]);
        assert_eq!(s.emit(&b), 1, "只有活着的那个订阅者收到");
        assert_eq!(s.len(), 1, "断开的订阅要被清掉，而不是留在表里");
        assert!(live.rx.try_recv().is_ok());
    }

    /// 话题不命中的订阅者：收不到，但**也不该被误删**
    #[test]
    fn emit_keeps_subscribers_whose_topics_do_not_match() {
        let mut s = Subscribers::default();
        let meta_only = s.subscribe(TopicFilter::of(&[TopicKind::Meta]));
        let b = broadcast(vec![Topic::new(TopicKind::Bpm, None)]);
        assert_eq!(s.emit(&b), 0);
        assert_eq!(s.len(), 1, "不命中 ≠ 断开：这条订阅还在");
        assert!(meta_only.rx.try_recv().is_err());
    }
}
