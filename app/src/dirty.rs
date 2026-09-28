//! **广播话题 → 面板脏位**的唯一映射表。
//!
//! "只重建脏掉的那块"这句话的全部实现就是这里：`EditCore` 发出带话题的广播，
//! 这一层把话题翻成"哪些面板要重建"，GUI 再照着重建。
//!
//! 为什么放在库里：它是一条**纯函数**（话题列表 → 脏位），而且规则细微到踩过坑 ——
//! 比如"改一次透明度/流速事件（`Track`）不该牵动**音符列表**"，
//! 以及"`LineProps` 会牵连 `bpmFactor`，所以事件话题不得附带 `LineProps`"。
//! 放在 `main.rs` 里它就永远只能靠肉眼看；放这里就能拿测试钉住每一条落点。
//!
//! **检查器（`inspector`）是个例外：它几乎每条话题都要刷。** 原因很实在：
//! 检查器显示并**可编辑**当前选中对象的字段 —— 选中事件时显示它的起止拍/值/缓动，
//! 选中音符时显示音符字段，选中线时显示线名/zOrder/bpmFactor。任何一条能改到这些字段的
//! 广播不刷它，面板就会**停在你改之前的那份快照上**（用户报的"改完看不出来 /
//! 下拉框总显示上一次选的"就是这个，2026-09-28 修）。

use crate::broadcast::{Topic, TopicKind};

/// 面板脏位：广播话题 → 哪些派生视图需要重建。**粒度到"哪条判定线的哪一部分"**。
///
/// 线是这个编辑器的父对象，所以脏位也按线分：改 3 号线的音符只重建 3 号线的音符缓存，
/// 其余线的缓存与其余面板一帧都不参与（计数可见）。
#[derive(Clone, Default, Debug, PartialEq)]
pub struct Dirty {
    /// 判定线集合本身（增删、顺序、BPM ⇒ 时间映射）
    pub structure: bool,
    /// 判定线属性（doc 下标）
    pub props: Vec<usize>,
    /// 某条线的**子音符**缓存（doc 下标）
    pub notes: Vec<usize>,
    /// 某条线的事件轨道缓存（doc 下标）
    pub tracks: Vec<usize>,
    /// 工具栏/状态栏的文档摘要文本
    pub meta: bool,
    /// 右侧属性检查器
    pub inspector: bool,
    /// 演奏区：无缓存可重建（实例每帧现构），置位只表示"该重绘了"
    pub render: bool,
}

impl Dirty {
    /// 这一帧有没有事情要做（全 false 时 GUI 一次重建都不该做）
    pub fn any(&self) -> bool {
        self.structure
            || !self.props.is_empty()
            || !self.notes.is_empty()
            || !self.tracks.is_empty()
            || self.meta
            || self.inspector
            || self.render
    }

    /// 合并另一份脏位（同一帧收到多条广播时用）。按线去重：同一条线脏两次也只重建一次。
    pub fn merge(&mut self, o: &Dirty) {
        self.structure |= o.structure;
        self.meta |= o.meta;
        self.inspector |= o.inspector;
        self.render |= o.render;
        for (dst, src) in [
            (&mut self.props, &o.props),
            (&mut self.notes, &o.notes),
            (&mut self.tracks, &o.tracks),
        ] {
            for v in src {
                if !dst.contains(v) {
                    dst.push(*v);
                }
            }
        }
    }
}

/// 话题 → 脏位的唯一映射表。中央一处，避免各面板各自解释话题。
///
/// 注意 `Track` 的落点：它只更新**那条线的事件轨道缓存**并置"演奏区需重绘"。
/// 音符列表、工具栏、检查器都不动 —— 改一次透明度/流速事件的重建代价是零。
pub fn dirty_of_topics(topics: &[Topic]) -> Dirty {
    let mut d = Dirty::default();
    for t in topics {
        match t.kind {
            TopicKind::Meta => d.meta = true,
            // 时间映射变了 ⇒ 所有线都要重算（时刻是拍域换算来的）
            TopicKind::Bpm | TopicKind::LineList => {
                d.structure = true;
                d.inspector = true;
            }
            TopicKind::LineProps => {
                if let Some(l) = t.line {
                    d.props.push(l);
                } else {
                    d.structure = true;
                }
                // 检查器里显示着这条线的名字/zOrder/isCover/bpmFactor ⇒ 它也要跟着刷
                d.inspector = true;
                d.render = true;
            }
            TopicKind::Notes | TopicKind::Note => {
                if let Some(l) = t.line {
                    d.notes.push(l);
                } else {
                    d.structure = true;
                }
                d.inspector = true;
                d.render = true;
            }
            TopicKind::Track => {
                if let Some(l) = t.line {
                    d.tracks.push(l);
                } else {
                    d.structure = true;
                }
                // **检查器必须刷**：它显示（且可编辑）**选中事件**的起止拍/值/缓动 ——
                // 少这一条，面板就会停在你改之前那份快照上（改完看不出来、
                // 缓动下拉框"总显示上一次选的"，都是这一个原因）。
                // 音符列表仍然不重建（事件话题改不到音符），那一半的代价还是零。
                d.inspector = true;
                d.render = true;
            }
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(kind: TopicKind, line: Option<usize>) -> Topic {
        Topic::new(kind, line)
    }

    /// 元信息只动工具栏/状态栏那一行，谁都不牵连
    #[test]
    fn meta_touches_only_the_summary() {
        let d = dirty_of_topics(&[t(TopicKind::Meta, None)]);
        assert!(d.meta && d.any());
        assert!(!d.structure && d.props.is_empty() && d.notes.is_empty() && d.tracks.is_empty());
        assert!(!d.inspector && !d.render);
    }

    /// BPM / 线集合变化 ⇒ 整表重建（时间映射变了，每条线的时刻都要重算）
    #[test]
    fn bpm_or_line_list_rebuilds_everything_structural() {
        for kind in [TopicKind::Bpm, TopicKind::LineList] {
            let d = dirty_of_topics(&[t(kind, None)]);
            assert!(d.structure, "{kind:?}");
            assert!(d.inspector, "整表重建时检查器的快照也过期了");
            assert!(d.props.is_empty() && d.notes.is_empty() && d.tracks.is_empty());
        }
    }

    /// 属性话题：按线走；**没有线号**（整表改动）时才退化成整表重建
    #[test]
    fn line_props_is_scoped_to_that_line() {
        let d = dirty_of_topics(&[t(TopicKind::LineProps, Some(3))]);
        assert_eq!(d.props, vec![3]);
        assert!(d.render);
        assert!(d.inspector, "检查器显示线名/zOrder/isCover/bpmFactor ⇒ 必须跟着刷");
        assert!(!d.structure, "只改一条线的属性不该重建整表");
        assert!(d.notes.is_empty() && d.tracks.is_empty(), "属性话题不碰音符/事件轨道缓存");

        let d = dirty_of_topics(&[t(TopicKind::LineProps, None)]);
        assert!(d.structure && d.props.is_empty(), "没有线号 ⇒ 只能整表重建");
    }

    /// 音符话题：该线的音符缓存 + 检查器 + 演奏区（检查器里有音符快照，必须一起刷）
    #[test]
    fn notes_touch_the_line_and_the_inspector() {
        for kind in [TopicKind::Notes, TopicKind::Note] {
            let d = dirty_of_topics(&[t(kind, Some(2))]);
            assert_eq!(d.notes, vec![2], "{kind:?}");
            assert!(d.inspector && d.render);
            assert!(!d.structure && d.tracks.is_empty());
        }
    }

    /// **Track 落在那条线的事件轨道上 + 检查器**。
    ///
    /// 音符列表不重建（事件话题改不到音符，那一半的代价仍是零），但**检查器必须刷**：
    /// 它显示并可编辑**选中事件**的起止拍/值/缓动 —— 漏了这一条，面板就停在改之前那份
    /// 快照上（用户报的"改完看不出来 / 缓动下拉框总显示上一次选的"，2026-09-28）。
    #[test]
    fn track_events_refresh_the_track_cache_and_the_inspector() {
        let d = dirty_of_topics(&[t(TopicKind::Track, Some(5))]);
        assert_eq!(d.tracks, vec![5]);
        assert!(d.render);
        assert!(d.notes.is_empty(), "事件轨道变化不该重建音符缓存");
        assert!(d.inspector, "检查器显示选中事件的值 ⇒ 必须跟着刷");
        assert!(!d.structure && !d.meta && d.props.is_empty());
        // 没有线号的事件话题 ⇒ 整表
        assert!(dirty_of_topics(&[t(TopicKind::Track, None)]).structure);
    }

    /// 合并：按线去重（同一条线在多条广播里脏了两次，也只重建一次）
    #[test]
    fn merge_unions_and_dedupes_lines() {
        let mut a = dirty_of_topics(&[
            t(TopicKind::Notes, Some(1)),
            t(TopicKind::Meta, None),
        ]);
        let b = dirty_of_topics(&[
            t(TopicKind::Notes, Some(1)),
            t(TopicKind::Notes, Some(2)),
            t(TopicKind::Track, Some(2)),
            t(TopicKind::LineProps, Some(7)),
        ]);
        a.merge(&b);
        assert_eq!(a.notes, vec![1, 2], "同一条线只留一次");
        assert_eq!(a.tracks, vec![2]);
        assert_eq!(a.props, vec![7]);
        assert!(a.meta && a.render && a.inspector);
        assert!(a.any());
        // 空脏位：`any()` 必须是 false（GUI 靠它"什么都不做"）
        assert!(!Dirty::default().any());
        let mut empty = Dirty::default();
        empty.merge(&Dirty::default());
        assert!(!empty.any());
    }
}
