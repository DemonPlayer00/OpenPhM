// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 DemonPlayer
//! 广播机制的验收测试：**改完必须广播；话题不命中的订阅者必须收不到**。
//!
//! 这两条是"GUI 不私自更新、无关控件不参与更新"这条架构要求的可执行证据。

use opm_app::broadcast::{TopicFilter, TopicKind};
use opm_app::doc::{Beat, Event};
use opm_app::core::EditCore;
use serde_json::json;

/// 收一条（若有），带超时保护 —— 通道是同步的，用 try_recv 足够
fn drain(rx: &std::sync::mpsc::Receiver<opm_app::broadcast::Broadcast>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(b) = rx.try_recv() {
        out.push(b.summary());
    }
    out
}

#[test]
fn only_matching_subscribers_receive() {
    let mut core = EditCore::new();

    // 控件 A：只关心元信息（比如顶部工具栏）
    let a = core.subscribe(TopicFilter::of(&[TopicKind::Meta]));
    // 控件 B：只关心 3 号线的音符集合（比如该线的音符列表）
    let b = core.subscribe(TopicFilter::of_lines(
        &[TopicKind::Notes, TopicKind::Note],
        vec![3],
    ));
    // 渲染层：几乎什么都关心
    let r = core.subscribe(TopicFilter::all());
    assert_eq!(core.subscriber_count(), 3);

    // 建 4 条线，供后面按线下标操作
    for i in 0..4 {
        core.exec(&json!({"op": "add_line", "name": format!("L{i}")}));
    }
    let _ = drain(&a.rx);
    let _ = drain(&b.rx);
    let _ = drain(&r.rx);

    // ---- 1. 只改元信息 ----
    let resp = core.exec(&json!({"op": "set_meta", "set": {"name": "Test"}}));
    assert_eq!(resp["ok"], json!(true));
    assert_eq!(drain(&a.rx).len(), 1, "A 关心 Meta，必须收到");
    assert_eq!(drain(&b.rx).len(), 0, "B 不关心 Meta，不该收到");
    assert_eq!(drain(&r.rx).len(), 1, "渲染层全收");
    assert_eq!(core.last_delivered(), 2, "只有 A 与渲染层命中");

    // ---- 2. 改 0 号线的音符：B 只订阅 3 号线，不该被叫醒 ----
    let resp = core.exec(&json!({
        "op": "add_note", "line": 0, "kind": "tap", "startBeat": [1, 1], "laneX": 0.0
    }));
    assert_eq!(resp["ok"], json!(true));
    assert_eq!(drain(&a.rx).len(), 0, "A 不关心音符");
    assert_eq!(drain(&b.rx).len(), 0, "B 只订阅 3 号线，0 号线的改动不该叫醒它");
    assert_eq!(drain(&r.rx).len(), 1);
    assert_eq!(core.last_delivered(), 1);

    // ---- 3. 改 3 号线的音符：B 这次必须醒 ----
    let resp = core.exec(&json!({
        "op": "add_note", "line": 3, "kind": "hold", "startBeat": [2, 1], "endBeat": [4, 1], "laneX": 0.5
    }));
    assert_eq!(resp["ok"], json!(true));
    assert_eq!(drain(&b.rx).len(), 1, "3 号线的改动必须投给 B");
    assert_eq!(core.last_delivered(), 2);

    // ---- 4. 事件轨道改动 → 只命中 Track 类订阅者 ----
    let t = core.subscribe(TopicFilter::of(&[TopicKind::Track]));
    core.exec(&json!({
        "op": "add_event", "line": 3, "layer": 0, "track": "alpha",
        "startBeat": [0, 1], "endBeat": [8, 1], "startValue": 0, "endValue": 255, "easing": "linear"
    }));
    assert_eq!(drain(&t.rx).len(), 1, "Track 订阅者必须收到");
    assert_eq!(drain(&b.rx).len(), 0, "只订阅 Notes/Note 的控件不该因轨道改动被叫醒");
    let _ = drain(&r.rx); // 清掉前面几步留给渲染层的积压，下面只看 undo 这一条

    // ---- 5. 撤销也必须广播（否则界面会停在幻影状态）----
    core.exec(&json!({"op": "undo"}));
    assert_eq!(drain(&r.rx).len(), 1, "undo 之后必须有广播");
    assert_eq!(drain(&t.rx).len(), 1, "undo 掉的是轨道改动 → Track 订阅者收到");

    // ---- 6. 失败的命令不得广播（文档没变就不该叫醒任何人）----
    let resp = core.exec(&json!({"op": "add_note", "line": 99, "kind": "tap", "laneX": 0.0, "startBeat": [1, 1]}));
    assert_eq!(resp["ok"], json!(false));
    assert_eq!(drain(&r.rx).len(), 0, "失败命令不广播");
    assert_eq!(drain(&b.rx).len(), 0);

    // ---- 7. 事务：**每条改动都广播**，但撤销只算一步 ----
    //
    // 早先这里是"事务内不广播、commit 时合并成一条"。改成逐条广播是因为**拖拽编辑**：
    // 一次手势里要连续看到变化，而撤销又必须整体一步。订阅者要的是"文档变了"，
    // 撤销怎么分组是另一件事 —— 两者不必绑在一起。
    // 前面几步已经改过文档，所以撤销深度要取**增量**，不能假设从 0 起
    let d0 = core.journal().undo_depth();
    core.exec(&json!({"op": "begin", "label": "batch"}));
    core.exec(&json!({"op": "add_note", "line": 3, "kind": "tap", "laneX": 0.0, "startBeat": [9, 1]}));
    core.exec(&json!({"op": "add_note", "line": 3, "kind": "tap", "laneX": 0.0, "startBeat": [10, 1]}));
    assert_eq!(drain(&b.rx).len(), 2, "事务中每条改动都要广播（拖拽要实时看到）");
    assert_eq!(core.journal().undo_depth(), d0, "事务未提交前不产生撤销步");
    core.exec(&json!({"op": "commit"}));
    assert_eq!(drain(&b.rx).len(), 0, "commit 不再重复广播（改动发生时已发）");
    // 关键性质：**广播逐条、撤销一步** —— 两条改动合并成一个事务步
    assert_eq!(core.journal().undo_depth(), d0 + 1, "两条改动应只占一个撤销步");
    assert_eq!(drain(&r.rx).len(), 2);
    // 撤销这一步应该把两条改动一起撤掉
    let n_before_undo = core.doc().judge_lines[3].notes.len();
    core.exec(&json!({"op": "undo"}));
    assert_eq!(
        core.doc().judge_lines[3].notes.len(),
        n_before_undo - 2,
        "撤销一个事务步应撤掉整批"
    );

    // ---- 8. abort：事务里的改动**先广播过**，abort 再发一条全量话题让订阅者重取 ----
    // （逐条广播的代价：被丢弃的改动也曾被订阅者看到过；abort 的全量广播就是纠偏手段）
    // 清掉上一步 undo 的广播（这次 undo 撤的是 3 号线，B 也会收到）
    let _ = drain(&r.rx);
    let _ = drain(&b.rx);
    let _ = drain(&t.rx);
    core.exec(&json!({"op": "begin", "label": "doomed"}));
    core.exec(&json!({"op": "add_note", "line": 0, "kind": "tap", "laneX": 0.0, "startBeat": [20, 1]}));
    // 这条改动在 0 号线：全收的渲染层收到，只订阅 3 号线的 B 收不到（话题过滤器在起作用）
    assert_eq!(drain(&r.rx).len(), 1, "事务中被丢弃的改动当时也广播过");
    let leaked = drain(&b.rx);
    assert_eq!(leaked.len(), 0, "0 号线的改动不该叫醒只订阅 3 号线的控件；实收 {leaked:?}");
    core.exec(&json!({"op": "abort"}));
    assert_eq!(drain(&b.rx).len(), 1, "abort 让所有订阅者重取一次");
    assert_eq!(drain(&t.rx).len(), 1);
    // 而且文档里确实没有那条音符
    assert!(
        !core.doc().judge_lines[0]
            .notes
            .iter()
            .any(|n| n.start == Beat::new(20, 1)),
        "abort 后文档不该留下未提交的改动"
    );
}
#[test]
fn broadcast_log_is_introspectable() {
    let mut core = EditCore::new();
    core.exec(&json!({"op": "add_line"}));
    core.exec(&json!({"op": "add_note", "line": 0, "kind": "tap", "laneX": 0.0, "startBeat": [1, 1]}));
    let r = core.exec(&json!({"op": "broadcasts", "recent": 5}));
    assert_eq!(r["ok"], json!(true));
    let res = &r["result"];
    assert_eq!(res["count"], json!(2));
    assert_eq!(res["subscribers"], json!(0));
    let items = res["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    // 最新一条在最前：add_note → Notes[0]/Note[0]
    let topics: Vec<String> = items[0]["topics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert!(topics.contains(&"Notes[0]".to_string()), "topics={topics:?}");
    assert_eq!(items[0]["origin"], json!("Local"));
}
/// **失败与 abort 都必须真的回滚文档**（不只是丢掉日志）。
///
/// 抓这个 bug 的过程值得记：给"逐条广播"写断言时才发现 `abort()` 只做了 `pending = None`，
/// 而 `mutate` 是直接改文档的 —— 于是事务里的改动"改了但撤不回"（不在撤销栈里、也没还原）。
#[test]
fn abort_and_failure_roll_back_the_document() {
    let mut core = EditCore::new();
    core.exec(&json!({"op": "add_line"}));
    let notes_before = core.doc().judge_lines[0].notes.len();
    let d0 = core.journal().undo_depth(); // add_line 本身是一步，别把它算进来

    // 事务里改两条，然后 abort：文档必须回到事务前
    core.exec(&json!({"op": "begin", "label": "doomed"}));
    core.exec(&json!({"op": "add_note", "line": 0, "kind": "tap", "laneX": 0.0, "startBeat": [3, 1]}));
    core.exec(&json!({"op": "add_note", "line": 0, "kind": "tap", "laneX": 0.0, "startBeat": [4, 1]}));
    assert_eq!(core.doc().judge_lines[0].notes.len(), notes_before + 2, "事务中确实改到了文档");
    core.exec(&json!({"op": "abort"}));
    assert_eq!(
        core.doc().judge_lines[0].notes.len(),
        notes_before,
        "abort 必须把文档回滚（不是只丢日志）"
    );
    assert_eq!(core.journal().undo_depth(), d0, "回滚后不该留下撤销步");

    // 单条命令失败：同样不该留下痕迹
    let resp = core.exec(&json!({"op": "set_note", "line": 0, "index": 999, "set": {"laneX": 1.0}}));
    assert_eq!(resp["ok"], json!(false));
    assert_eq!(core.doc().judge_lines[0].notes.len(), notes_before);
    assert_eq!(core.journal().undo_depth(), d0);

    // 而且回滚之后还能正常继续用（pending 已清空，不会串到下一次事务）
    let resp = core.exec(&json!({"op": "add_note", "line": 0, "kind": "tap", "laneX": 0.0, "startBeat": [5, 1]}));
    assert_eq!(resp["ok"], json!(true));
    assert_eq!(core.doc().judge_lines[0].notes.len(), notes_before + 1);
}
/// `resize_event`：控制柄**只控制它所属的那一个事件**。
///
/// 用户要求改过一次语义：早先"同步邻块以维持无空隙"会让一次拖拽同时改两个事件（被明确否掉）。
/// 现在的规则是：
/// 1. 只改这一个事件的 start 或 end（不碰邻居）；
/// 2. 由此产生的**空隙/重叠**交给检测机制报（重叠进冲突浏览器、空隙进 `validate`）；
/// 3. 空隙里判定线的行为由求值器保证"保持前值"而不是跳变 —— 这才是"某时间没有事件则不移动判定线"。
#[test]
fn resize_event_touches_one_event_only() {
    let mut core = EditCore::new();
    core.exec(&json!({"op": "add_line"}));
    core.exec(&json!({"op": "add_event", "line": 0, "layer": 0, "track": "moveX",
        "startBeat": [0, 1], "endBeat": [32, 1], "startValue": 0.0, "endValue": 0.0, "easing": "linear"}));
    core.exec(&json!({"op": "add_event", "line": 0, "layer": 0, "track": "moveX",
        "startBeat": [32, 1], "endBeat": [64, 1], "startValue": 400.0, "endValue": 400.0, "easing": "linear"}));
    let track = |core: &EditCore| -> Vec<Event> {
        core.doc().judge_lines[0].layers[0].track("moveX").unwrap().clone()
    };

    // 抓事件 0 的尾拖到 20 ⇒ 只有事件 0 的 end 变，事件 1 一点不动（于是出现空隙 [20,32)）
    let r = core.exec(&json!({"op": "resize_event", "line": 0, "layer": 0, "track": "moveX",
        "index": 0, "edge": "end", "toBeat": [20, 1]}));
    assert_eq!(r["ok"], json!(true), "{r}");
    let ev = track(&core);
    assert_eq!(ev[0].end, Beat::new(20, 1), "被拖的事件终点应到 20");
    assert_eq!(ev[1].start, Beat::new(32, 1), "**邻块不得被改动**");
    assert_eq!(ev[1].start_value, json!(400.0), "邻块的值也不得被改写");

    // 空隙被检测出来（validate 会报"间隙"）
    let issues = opm_app::cmd::validate(core.doc());
    let msgs: Vec<&str> = issues.iter().map(|i| i.message.as_str()).collect();
    assert!(
        msgs.iter().any(|m| m.contains("空隙")),
        "空隙应被校验报出：{msgs:?}"
    );

    // 抓事件 1 的头拖到 24 ⇒ 与事件 0 的 [0,20) 不重叠；再拖到 10 ⇒ 重叠，被重叠检测报出
    core.exec(&json!({"op": "resize_event", "line": 0, "layer": 0, "track": "moveX",
        "index": 1, "edge": "start", "toBeat": [24, 1]}));
    assert_eq!(track(&core)[1].start, Beat::new(24, 1));
    assert_eq!(track(&core)[0].end, Beat::new(20, 1), "仍然不碰邻居");
    assert!(opm_app::cmd::overlaps(core.doc()).is_empty(), "20 与 24 之间是空隙，不是重叠");
    core.exec(&json!({"op": "resize_event", "line": 0, "layer": 0, "track": "moveX",
        "index": 1, "edge": "start", "toBeat": [10, 1]}));
    let ov = opm_app::cmd::overlaps(core.doc());
    assert_eq!(ov.len(), 1, "拖出重叠后应被检测到：{ov:?}");
    assert_eq!(ov[0].start, Beat::new(10, 1));
    assert_eq!(ov[0].end, Beat::new(20, 1), "重叠区 = [10,20)");

    // 越界仍要拒：不能把自己的头拖过自己的尾
    assert_eq!(
        core.exec(&json!({"op": "resize_event", "line": 0, "layer": 0, "track": "moveX",
            "index": 1, "edge": "start", "toBeat": [99, 1]}))["ok"],
        json!(false)
    );

    // 整段拖拽仍是一个撤销步
    let before = track(&core);
    core.exec(&json!({"op": "begin", "label": "拖事件头"}));
    core.exec(&json!({"op": "resize_event", "line": 0, "layer": 0, "track": "moveX",
        "index": 1, "edge": "start", "toBeat": [12, 1]}));
    core.exec(&json!({"op": "resize_event", "line": 0, "layer": 0, "track": "moveX",
        "index": 1, "edge": "start", "toBeat": [14, 1]}));
    core.exec(&json!({"op": "commit"}));
    assert_ne!(track(&core), before);
    core.exec(&json!({"op": "undo"}));
    assert_eq!(track(&core), before, "撤销一步应回到拖拽前");
}
