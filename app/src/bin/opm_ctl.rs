//! `opm-ctl` —— 给 agent 用的编辑入口。
//!
//! 两种工作方式，**操作的是同一套编辑核心**：
//! · **无头**：进程内建一个 `EditCore`，载入文件 → 执行命令 → 落盘；
//! · **附着（`--attach`）**：连进正在运行的 GUI 进程的 Unix socket，改的是**那份正在被编辑的文档**，
//!   GUI 会在下一帧自动重建视图（靠 `EditCore::revision`）。
//!
//! 退出码：0 全部成功；1 有命令失败；2 用法/IO 错误；3 校验存在 ERROR。

use std::io::Write;
use std::path::PathBuf;

use opm_app::cmd::{parse_commands, response_line, validate, Severity};
use opm_app::control;
use opm_app::core::{CacheClaim, EditCore};
use opm_app::headless;
use serde_json::{json, Value};

const USAGE: &str = r#"opm-ctl —— opm 谱面编辑入口（无头 / 附着到运行中的进程）

用法:
  opm-ctl new [--out FILE] [--name NAME] [--bpm BPM] [--demo-notes N]
  opm-ctl convert IN [--to opm|opm-dir|rpe|rpe-dir] [--out PATH] [--rpe-version N] [--quiet]
  opm-ctl --file FILE [--cmd JSON]... [--script FILE] [--stdin] [--save] [--json] [--quiet] [--atomic]
  opm-ctl --attach [SOCKET|auto] [--cmd JSON]... [--script FILE] [--stdin] [--json]
  opm-ctl --file FILE validate [--json]
  opm-ctl --file FILE lines [--at SEC] [--json]
  opm-ctl --file FILE overlaps [--json]      # 事件重叠（0 处 → 退出码 0；有 → 4）
  opm-ctl --file FILE summary | dump | journal
  opm-ctl --file FILE render [--at SEC] [--lookahead SEC] [--width W] [--height H] --out PNG
  opm-ctl help

格式:  `--file`/`convert` 的输入**按内容判格式**（opm 有 `format:"opm"`；RPE 有 `judgeLineList`/`BPMList`），
       不看扩展名 —— 两种都是 `.json`。转换一律打印保真度报告（做了什么、丢了什么）。

附着模式: GUI 用 `opm-app --control auto` 启动后会打印 socket 路径；
          `--attach auto` 自动发现 $XDG_RUNTIME_DIR 下最新的 opm-*.sock。

命令（JSON，一次一条；字段语义见 spec/opm-format.md）:
  音符:   add_note / set_note / del_note / move_notes
  判定线: add_line / set_line / del_line
  事件（移动/透明度/流速同一套）: add_event / set_event / del_event / split_event
          track ∈ moveX|moveY|rotate|alpha|speed；easing 用名字（29 种）
  不变量: set_track_constant（一步铺满全谱） / normalize（排序/补空隙/裁重叠/延到谱末）
  事务:   begin / commit / abort；或 CLI 的 --atomic（整批一次撤销）
  粒度:   默认「一条命令 = 一步撤销」—— 批内 undo 因此可用
  日志:   undo / redo / journal / patch
  其它:   set_bpm / set_meta / summary / dump / validate / save / render / ping

退出码: 0 成功 | 1 有命令失败 | 2 用法或 IO 错误 | 3 校验存在 ERROR
"#;

fn main() {
    std::process::exit(run());
}

struct Cli {
    file: Option<PathBuf>,
    attach: Option<String>,
    cmds: Vec<String>,
    scripts: Vec<PathBuf>,
    stdin: bool,
    save: bool,
    json: bool,
    quiet: bool,
    sub: Option<String>,
    sub_args: Vec<String>,
    /// 整批作为一次事务（一次撤销整体回退）
    atomic: bool,
}

fn parse_cli(argv: &[String]) -> Result<Cli, String> {
    let mut c = Cli {
        file: None,
        attach: None,
        cmds: Vec::new(),
        scripts: Vec::new(),
        stdin: false,
        save: false,
        json: false,
        quiet: false,
        sub: None,
        sub_args: Vec::new(),
        atomic: false,
    };
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        let next = |i: &mut usize| -> Option<String> {
            *i += 1;
            argv.get(*i).cloned()
        };
        match a {
            "--file" | "-f" => c.file = next(&mut i).map(PathBuf::from),
            "--attach" => {
                let v = argv.get(i + 1).filter(|v| !v.starts_with('-')).cloned();
                match v {
                    Some(p) => {
                        c.attach = Some(p);
                        i += 1;
                    }
                    None => c.attach = Some("auto".into()),
                }
            }
            "--cmd" | "-c" => {
                if let Some(v) = next(&mut i) {
                    c.cmds.push(v);
                }
            }
            "--script" | "-s" => {
                if let Some(v) = next(&mut i) {
                    c.scripts.push(PathBuf::from(v));
                }
            }
            "--stdin" => c.stdin = true,
            "--save" => c.save = true,
            "--json" => c.json = true,
            "--quiet" | "-q" => c.quiet = true,
            "--atomic" => c.atomic = true,
            "validate" | "summary" | "dump" | "journal" | "render" | "lines" | "overlaps" => {
                c.sub = Some(a.to_owned());
                c.sub_args = argv[i + 1..].to_vec();
                break;
            }
            other if other.starts_with('-') => return Err(format!("未知参数 {other}")),
            other => return Err(format!("未知子命令 {other}")),
        }
        i += 1;
    }
    Ok(c)
}

fn run() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv[0] == "help" || argv[0] == "--help" || argv[0] == "-h" {
        print!("{USAGE}");
        return if argv.is_empty() { 2 } else { 0 };
    }
    if argv[0] == "new" {
        return cmd_new(&argv[1..]);
    }
    if argv[0] == "convert" {
        return cmd_convert(&argv[1..]);
    }
    let cli = match parse_cli(&argv) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            return 2;
        }
    };

    // 收集命令
    let mut batch: Vec<Value> = Vec::new();
    let push_text = |text: &str, what: &str, batch: &mut Vec<Value>| -> Result<(), String> {
        match parse_commands(text) {
            Ok(v) => {
                batch.extend(v);
                Ok(())
            }
            Err(e) => Err(format!("{what} 解析失败: {e}")),
        }
    };
    for c in &cli.cmds {
        if let Err(e) = push_text(c, "--cmd", &mut batch) {
            eprintln!("{e}");
            return 2;
        }
    }
    for s in &cli.scripts {
        match std::fs::read_to_string(s) {
            Ok(t) => {
                if let Err(e) = push_text(&t, &format!("脚本 {}", s.display()), &mut batch) {
                    eprintln!("{e}");
                    return 2;
                }
            }
            Err(e) => {
                eprintln!("读取脚本 {} 失败: {e}", s.display());
                return 2;
            }
        }
    }
    if cli.stdin {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if let Err(e) = push_text(&line, "stdin", &mut batch) {
                eprintln!("{e}");
            }
        }
    }

    // ---------------- 附着到运行中的进程 ----------------
    if let Some(spec) = &cli.attach {
        let path = if spec == "auto" {
            match control::find_socket() {
                Some(p) => p,
                None => {
                    eprintln!("未发现运行中的 opm socket（GUI 是否带 --control 启动？）");
                    return 2;
                }
            }
        } else {
            PathBuf::from(spec)
        };
        return match control::attach(&path, &batch, cli.json, cli.quiet) {
            Ok((failed, errors)) => {
                if errors > 0 {
                    3
                } else if failed > 0 {
                    1
                } else {
                    0
                }
            }
            Err(e) => {
                eprintln!("{e}");
                2
            }
        };
    }

    // ---------------- 无头 ----------------
    let mut core = match &cli.file {
        // **一次性读取**（`CacheClaim::ReadOnly`）：别人已经认领的缓存目录一个字节都不碰 ——
        // `opm-ctl` 只是读一下这个包，不能把 GUI 崩溃留下的未保存快照顺手覆盖掉
        Some(p) => match EditCore::stage_file_as(p, CacheClaim::ReadOnly) {
            Ok(staged) => {
                let mut c = EditCore::new();
                if let Err(e) = c.load_staged(staged) {
                    eprintln!("载入 {} 失败: {e}", p.display());
                    return 2;
                }
                c
            }
            Err(e) => {
                eprintln!("载入 {} 失败: {e}", p.display());
                return 2;
            }
        },
        None => EditCore::new(),
    };

    let mut failed = 0;
    if !batch.is_empty() {
        // 默认逐条（一条命令 = 一步撤销）；--atomic 才整批一个事务
        let (resps, f) = if cli.atomic {
            core.exec_batch_atomic(&batch, "batch")
        } else {
            core.exec_batch(&batch)
        };
        failed = f;
        if !cli.quiet {
            for r in &resps {
                if cli.json {
                    println!("{r}");
                } else {
                    println!("{}", response_line(r));
                }
            }
        }
    }

    if cli.save && !batch.is_empty() {
        match core.save(None) {
            Ok(p) => {
                if !cli.quiet {
                    println!("已保存 {}", p.display());
                }
            }
            Err(e) => {
                eprintln!("保存失败: {e}");
                return 2;
            }
        }
    }

    match cli.sub.as_deref() {
        Some("validate") => {
            let issues = validate(core.doc());
            let errors = issues.iter().filter(|i| i.severity == Severity::Error).count();
            if cli.json {
                println!("{}", opm_app::cmd::validate_json(core.doc()));
            } else {
                for i in &issues {
                    println!(
                        "  {}  {}  {}",
                        match i.severity {
                            Severity::Error => "ERROR",
                            Severity::Warn => " WARN",
                        },
                        i.pointer,
                        i.message
                    );
                }
                println!(
                    "[{}] {} error(s), {} warning(s)",
                    if errors == 0 { "PASS" } else { "FAIL" },
                    errors,
                    issues.len() - errors
                );
            }
            if errors > 0 {
                return 3;
            }
        }
        // 事件重叠：与 GUI 状态栏那个"⚠ N 处事件重叠"**同一份实现**（`EditCore::overlaps`）。
        // 退出码：0 = 没有重叠，4 = 有（便于脚本 `opm-ctl --file x overlaps && …` 判断）
        Some("overlaps") => {
            // JSON 那一支**直接问核心要**（`{"op":"overlaps"}`）：字段映射在 `core` 里已经有一份，
            // 这里再抄一遍就会出现"命令通道多一个字段、CLI 少一个"（而且没人会发现）。
            if cli.json {
                let v = {
                    let mut c = core;
                    c.exec(&serde_json::json!({"op": "overlaps"}))
                };
                // 打**结果体**，不打 `exec` 的外壳（`{"ok":…,"revision":…}` 是 `--cmd` 那条路的形状）：
                // `--json overlaps` 的输出是脚本在用的契约，`{"count":…,"items":[…]}` 不能改层级
                let body = v.get("result").cloned().unwrap_or(v);
                println!("{body}");
                let n = body.get("count").and_then(|x| x.as_u64()).unwrap_or(0);
                if n > 0 {
                    return 4;
                }
            } else {
                let list = core.overlaps();
                if list.is_empty() {
                    println!("  没有事件重叠");
                } else {
                    for o in list {
                        println!("  {}  {}", o.pointer(), o.label());
                    }
                    println!(
                        "[{}] {} 处事件重叠",
                        if list.is_empty() { "PASS" } else { "WARN" },
                        list.len()
                    );
                    return 4;
                }
            }
        }
        Some("summary") => {
            let v = core.doc().summary();
            println!("{}", if cli.json { v.to_string() } else { serde_json::to_string_pretty(&v).unwrap_or_default() });
        }
        Some("dump") => {
            println!("{}", serde_json::to_string_pretty(&core.doc().to_json()).unwrap_or_default());
        }
        Some("lines") => {
            // 判定线 + 事件在某一时刻的**数值快照**：agent 核对"事件是否真的生效"用这个，
            // 不必依赖读图（读图用来核对"看起来对不对"）。`--at SEC` 指定播放头。
            let mut at = 0.0_f64;
            let mut j = 0;
            while j < cli.sub_args.len() {
                match cli.sub_args[j].as_str() {
                    "--at" if j + 1 < cli.sub_args.len() => {
                        at = cli.sub_args[j + 1].parse().unwrap_or(0.0);
                        j += 2;
                    }
                    _ => j += 1,
                }
            }
            let v = opm_app::headless::lines_report(core.doc(), at);
            println!(
                "{}",
                if cli.json { v.to_string() } else { serde_json::to_string_pretty(&v).unwrap_or_default() }
            );
        }
        Some("journal") => {
            let v = core.journal().patch();
            println!("{}", if cli.json { v.to_string() } else { serde_json::to_string_pretty(&v).unwrap_or_default() });
        }
        Some("render") => {
            let (mut at, mut lookahead, mut w, mut h) = (0.0_f64, 2.0_f64, 1920_u32, 1080_u32);
            let mut out: Option<PathBuf> = None;
            let mut j = 0;
            while j < cli.sub_args.len() {
                match cli.sub_args[j].as_str() {
                    "--at" if j + 1 < cli.sub_args.len() => {
                        at = cli.sub_args[j + 1].parse().unwrap_or(0.0);
                        j += 2;
                    }
                    "--lookahead" if j + 1 < cli.sub_args.len() => {
                        lookahead = cli.sub_args[j + 1].parse().unwrap_or(2.0);
                        j += 2;
                    }
                    "--width" if j + 1 < cli.sub_args.len() => {
                        w = cli.sub_args[j + 1].parse().unwrap_or(1920);
                        j += 2;
                    }
                    "--height" if j + 1 < cli.sub_args.len() => {
                        h = cli.sub_args[j + 1].parse().unwrap_or(1080);
                        j += 2;
                    }
                    "--out" | "-o" if j + 1 < cli.sub_args.len() => {
                        out = Some(PathBuf::from(&cli.sub_args[j + 1]));
                        j += 2;
                    }
                    _ => j += 1,
                }
            }
            let Some(out) = out else {
                eprintln!("render 需要 --out PNG");
                return 2;
            };
            let mut line_len = opm_app::state::RPE_LINE_LEN_DEFAULT;
            let mut boundary = true;
            let mut k = 0;
            while k < cli.sub_args.len() {
                match cli.sub_args[k].as_str() {
                    "--line-len" if k + 1 < cli.sub_args.len() => {
                        line_len = cli.sub_args[k + 1].parse().unwrap_or(line_len);
                        k += 2;
                    }
                    "--no-boundary" => {
                        boundary = false;
                        k += 1;
                    }
                    _ => k += 1,
                }
            }
            let opts = headless::RenderOpts { lookahead, line_len, boundary };
            if let Err(e) = headless::render_png(core.doc(), at, w, h, &out, opts) {
                eprintln!("渲染失败: {e}");
                return 2;
            }
        }
        _ => {}
    }

    if failed > 0 {
        1
    } else {
        0
    }
}

/// `convert IN [--to opm|opm-dir|rpe|rpe-dir] [--out PATH] [--rpe-version N] [--quiet]`
///
/// 输入按**内容**判格式；输出格式默认取反（RPE → opm，opm → RPE）。
/// 无论成功与否都打印保真度报告 —— "能转"不等于"没丢东西"。
fn cmd_convert(args: &[String]) -> i32 {
    use opm_app::core::SaveFormat;

    let mut input: Option<String> = None;
    let mut to: Option<String> = None;
    let mut out: Option<String> = None;
    let mut rpe_version: i64 = 160;
    let mut quiet = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let take = |i: &mut usize| -> Option<String> {
            *i += 1;
            args.get(*i).cloned()
        };
        match a {
            "--to" => to = take(&mut i),
            "--out" | "-o" => out = take(&mut i),
            "--rpe-version" => {
                rpe_version = take(&mut i).and_then(|v| v.parse().ok()).unwrap_or(160)
            }
            "--quiet" | "-q" => quiet = true,
            other if !other.starts_with('-') && input.is_none() => input = Some(other.to_owned()),
            other => {
                eprintln!("convert: 未知参数 {other}\n\n{USAGE}");
                return 2;
            }
        }
        i += 1;
    }
    let Some(input) = input else {
        eprintln!("convert: 需要输入文件\n\n{USAGE}");
        return 2;
    };
    let in_path = std::path::Path::new(&input);
    // **走核心的"载入文件"这一步**（`EditCore::stage_file`）：各种格式与打包情况（opm 容器 /
    // RPE 谱面包 `.pez` / 无压缩文件夹 / 裸 JSON）都在那里处理，转换工具不必自己认一遍 ——
    // 否则就是第二个装载实现，迟早和编辑器那份不一致（`.pez` 曾经就只导得出去、读不回来）。
    let staged = match opm_app::core::EditCore::stage_file_as(in_path, CacheClaim::ReadOnly) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("convert: 读取 {input} 失败: {e}");
            return 2;
        }
    };
    let in_fid = staged.fid.clone();
    if !quiet {
        println!("输入 {} —— {}", input, in_fid.report());
    }
    // 四种形态 = 格式（opm|rpe）× 打包开关（包 | 无压缩文件夹）；单文件 JSON 不再是保存形态
    let target = match to.as_deref() {
        Some("opm") | Some("opmz") | Some("container") => SaveFormat::OpmPacked,
        Some("opm-dir") | Some("opm-folder") => SaveFormat::OpmFolder,
        Some("rpe") | Some("pez") => SaveFormat::RpePacked,
        Some("rpe-dir") | Some("rpe-folder") => SaveFormat::RpeFolder,
        None => match in_fid.source.as_str() {
            "rpe" => SaveFormat::OpmPacked, // RPE 谱面默认转成 opm 包（正式形态）
            _ => SaveFormat::RpePacked,
        },
        Some(other) => {
            eprintln!(
                "convert: --to 只能是 opm | opm-dir | rpe | rpe-dir（得到 {other}）\
                 \n  四种形态 = 格式 × 打包开关；单文件 JSON 不再是保存形态"
            );
            return 2;
        }
    };
    let out_path: std::path::PathBuf = match out {
        Some(o) => std::path::PathBuf::from(o),
        None => {
            let stem = in_path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
            let dir = in_path.parent().unwrap_or(std::path::Path::new("."));
            match (target.chart_format(), target.packed()) {
                // 文件夹形态：目标是一个**目录**（里面放 opm.json / chart.json + 资源）
                (_, Some(false)) => dir.join(stem),
                (Some(opm_app::codec::Format::Rpe), _) => {
                    dir.join(format!("{stem}{}", opm_app::codec::package::PACKAGE_EXTENSION))
                }
                _ => dir.join(format!("{stem}.opm")),
            }
        }
    };
    // **所有形态都从 `EditCore` 出去**（保存路径**单一入口**）：以前这里对"单文件 RPE json"直接调
    // `rpe::save_file`，等于在核心之外又开了一个写盘口子 —— 于是"资源要不要装进包、文档字段要不要
    // 规范成包内名、脏标记怎么算"这些规矩都会在那一支里被绕过。现在四种形态只有一条路。
    let mut core = opm_app::core::EditCore::new();
    // 装载（同一条"摊开 → 装进会话"的路）：资源与保存目标都跟着进来
    if let Err(e) = core.load_staged(staged) {
        eprintln!("convert: 装载 {input} 失败: {e}");
        return 2;
    }
    if !quiet {
        // 目标 RPE 版档位（规范 §9 要求可切换）：载入侧的默认值在这里覆盖
        core.set_rpe_target(opm_app::codec::rpe::RpeTarget {
            version: rpe_version,
            ..Default::default()
        });
    }
    let out_fid = match core.save_as(&out_path, target) {
        Ok((_, f)) => f,
        Err(e) => {
            eprintln!("convert: 写出失败: {e}");
            return 2;
        }
    };
    if !quiet {
        println!("输出 {} —— {}", out_path.display(), out_fid.report());
    }
    if !out_fid.is_lossless() {
        // 有降级就不是"干净转换"：退出码区分开，脚本才能发现
        return 1;
    }
    0
}

fn cmd_new(args: &[String]) -> i32 {
    let mut out: Option<PathBuf> = None;
    let mut name = "untitled".to_owned();
    let mut bpm = 180.0_f32;
    let mut notes = 0usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" | "-o" if i + 1 < args.len() => {
                out = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--name" if i + 1 < args.len() => {
                name = args[i + 1].clone();
                i += 2;
            }
            "--bpm" if i + 1 < args.len() => {
                bpm = args[i + 1].parse().unwrap_or(bpm);
                i += 2;
            }
            "--demo-notes" if i + 1 < args.len() => {
                notes = args[i + 1].parse().unwrap_or(0);
                i += 2;
            }
            _ => i += 1,
        }
    }

    let mut core = EditCore::new();
    // 元信息与 BPM 也走命令 —— 与 GUI 同一条纪律：**文档只有 EditCore 能写**，客户端一律发命令
    let mut preset: Vec<Value> = vec![
        json!({"op":"set_meta","set":{"name": name}}),
        json!({"op":"set_bpm","index":0,"bpm": bpm}),
        // 预置五条恒定轨道：省得 agent 每建一条判定线都要补不变量
        json!({"op":"set_track_constant","line":0,"track":"moveX","value":0.0}),
        json!({"op":"set_track_constant","line":0,"track":"moveY","value":0.0}),
        json!({"op":"set_track_constant","line":0,"track":"rotate","value":0.0}),
        json!({"op":"set_track_constant","line":0,"track":"alpha","value":1.0}),
        json!({"op":"set_track_constant","line":0,"track":"speed","value":10.0}),
    ];
    for k in 0..notes {
        preset.push(json!({
            "op": "add_note", "line": 0, "kind": "tap",
            "startBeat": [k as i64, 4], "laneX": ((k as i64 % 9) - 4) as f64 * 120.0
        }));
    }
    let (resps, failed) = core.exec_batch_atomic(&preset, "new");
    if failed > 0 {
        for r in &resps {
            if r.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                eprintln!("{}", response_line(r));
            }
        }
    }

    match out {
        Some(p) => match core.save(Some(&p)) {
            Ok(p) => {
                println!("已创建 {}", p.display());
                0
            }
            Err(e) => {
                eprintln!("写入失败: {e}");
                2
            }
        },
        None => {
            let mut stdout = std::io::stdout();
            let _ = writeln!(
                stdout,
                "{}",
                serde_json::to_string_pretty(&core.doc().to_json()).unwrap_or_default()
            );
            0
        }
    }
}
