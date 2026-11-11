//! Bilingual command-line interface (`mongol-font-shaper`) implementation.
//! 中英双语命令行界面（`mongol-font-shaper`）实现模块。

use std::io::{self, Read, Write};
use std::path::Path;

use serde::Serialize;

use crate::error::{format_positioned_unit, Diagnostics, Error};
use crate::init_map::{generate_draft_profile, inspect_font, render_calibration_html};
use crate::pool::FontPool;
use crate::profile::{CompiledGlyph, FontProfile};
use crate::shaper::FontShaper;

const HELP_MAIN: &str = "\
mongol-font-shaper — Font-driven Mongolian shaping, UTN #57 normalization & multi-font encoding detection
mongol-font-shaper — 字体驱动的传统蒙古文塑形、UTN #57 规范化与多字体编码判别工具

[CAUTION / 注意] AI-generated & unverified — please verify outputs and use with caution!
[谨慎使用] 本工具及映射规则由 AI 生成，未经全面人工验证，请务必核实输出结果、谨慎使用！

USAGE / 用法:
  mongol-font-shaper <SUBCOMMAND> [OPTIONS]

SUBCOMMANDS / 子命令:
  shape      Shape Mongolian text into UTN #57 Unicode, WrittenUnit, or PositionedWrittenUnit
             使用指定字体与家族配置对蒙古文塑形并输出标准 UTN #57 文本或字形单元序列
  detect     Evaluate text or a multi-line corpus across a FontPool to identify the right font/encoding
             使用多字体池对单条文本或整批词典语料进行错码筛查与字体归属投票鉴定
  init-map   Pre-generate a human-editable *.profile.json (and optional local HTML/SVG calibration page)
             从字体文件预生成可人工修改的家族级 *.profile.json 映射表（及可选本地 HTML 标定页）
  inspect    Inspect font SFNT tables, glyph names, PUA ranges, and inferred encoding class
             检查字体内部排版表、字形名覆盖率、PUA 私用区范围及自动推断的编码类别

GLOBAL OPTIONS / 全局选项:
  -h, --help       Print bilingual help information / 打印中英双语帮助信息
  -V, --version    Print version / 打印版本号
";

const HELP_SHAPE: &str = "\
mongol-font-shaper shape — Shape & normalize text with a font and FontProfile
mongol-font-shaper shape — 使用外部字体与家族配置执行塑形及 UTN #57 规范化

USAGE / 用法:
  mongol-font-shaper shape --font <FONT_FILE> [--profile <utn57|auto|PROFILE.json>] [OPTIONS] [TEXT]

OPTIONS / 选项:
  -f, --font <FILE>        Path to .ttf / .otf / .ttc font file / 字体文件路径
      --face-index <N>     Font face index for .ttc collections (default: 0) / 字体集合索引（默认 0）
  -p, --profile <SPEC>     'utn57' (built-in), 'auto' (on-the-fly draft), or path to *.profile.json (default: utn57)
                           内置 'utn57'、实时自动推断 'auto' 或 *.profile.json 路径（默认 utn57）
      --format <FMT>       Output format: text | units | positioned | glyphs | json (default: text)
                           输出格式：text（标准文本）| units（字形单元）| positioned（带位置单元）| glyphs | json
  -i, --input <FILE>       Read input text from file (otherwise [TEXT] arg or stdin) / 输入文件路径
  -o, --output <FILE>      Write output to file (default: stdout) / 输出文件路径
  -h, --help               Show help for 'shape' / 显示本命令帮助
";

const HELP_DETECT: &str = "\
mongol-font-shaper detect — Multi-font encoding discrimination & corpus-level voting
mongol-font-shaper detect — 多字体池错码排除与语料级字体自动鉴定

USAGE / 用法:
  mongol-font-shaper detect --pool <LABEL=FONT[:PROFILE]> [--pool ...] [OPTIONS] [TEXT]

OPTIONS / 选项:
      --pool <SPEC>            Register a candidate font: LABEL=FONT_PATH or LABEL=FONT_PATH:PROFILE_SPEC
                               向字体池注册候选字体：格式为 标签=字体路径 或 标签=字体路径:配置路径(或 utn57/auto)
      --corpus                 Evaluate line-by-line across an entire file/dictionary (corpus voting mode)
                               启用整本词典/多行语料批量投票模式（逐行累计错码率锁定唯一胜出字体）
      --include-latin-mapped   Include 1990s ASCII-mapped fonts (Danzan/HuaGuang) in auto-detection
                               允许占用英文 ASCII 码位的 90 年代老字体参与盲测（默认关闭以防误判英文）
      --json                   Emit machine-readable JSON report / 输出结构化 JSON 报告
  -i, --input <FILE>           Read input text/corpus from file / 从文件读取待检测文本或语料
  -h, --help                   Show help for 'detect' / 显示本命令帮助
";

const HELP_INIT_MAP: &str = "\
mongol-font-shaper init-map — Pre-generate a human-editable family FontProfile
mongol-font-shaper init-map — 预生成可人工修改的家族级 FontProfile 映射配置

USAGE / 用法:
  mongol-font-shaper init-map <FONT_FILE> -o <OUT.profile.json> [--id <ID>] [--html <CALIB.html>]

OPTIONS / 选项:
  -f, --font <FILE>        Path to .ttf / .otf / .ttc font file (or pass as first positional arg) / 字体路径
      --face-index <N>     Font face index for .ttc collections (default: 0) / 字体集合索引（默认 0）
      --id <ID>            Profile identifier (e.g. 'menk-gb2023', 'oyun-gb2023', 'menk-pua') / 配置 ID
  -o, --output <FILE>      Output path for *.profile.json / 生成的 JSON 配置文件保存路径
      --html <FILE>        Optional local HTML/SVG visual calibration page path (do NOT commit to git!)
                           可选：生成本地离线 HTML/SVG 竖排字形人工标定网页（含轮廓预览，切勿提交公开仓库！）
  -h, --help               Show help for 'init-map' / 显示本命令帮助
";

const HELP_INSPECT: &str = "\
mongol-font-shaper inspect — Inspect font tables, glyph naming, PUA ranges & encoding class
mongol-font-shaper inspect — 检查字体 SFNT 表、字形名、私用区范围与编码流派

USAGE / 用法:
  mongol-font-shaper inspect <FONT_FILE> [--face-index <N>] [--json]
";

/// Entry point taking process CLI arguments (`std::env::args().skip(1)`).
/// 接收命令行参数并执行对应子命令的入口函数。
pub fn run_env() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    match run_with_io(&args, &mut io::stdin(), &mut stdout, &mut stderr) {
        Ok(()) => 0,
        Err(msg) => {
            let _ = writeln!(stderr, "error (错误): {msg}");
            1
        }
    }
}

/// Run the CLI with injectable streams (enabling clean integration testing).
/// 使用可注入输入输出流运行 CLI（便于集成测试直接调用校验）。
pub fn run_with_io(
    args: &[String],
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), String> {
    let Some(first) = args.first().map(String::as_str) else {
        let _ = write!(stdout, "{HELP_MAIN}");
        return Ok(());
    };

    match first {
        "-h" | "--help" | "help" => {
            let _ = write!(stdout, "{HELP_MAIN}");
            Ok(())
        }
        "-V" | "--version" | "version" => {
            let _ = writeln!(stdout, "mongol-font-shaper {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "shape" => cmd_shape(&args[1..], stdin, stdout, stderr),
        "detect" => cmd_detect(&args[1..], stdin, stdout),
        "init-map" => cmd_init_map(&args[1..], stdout),
        "inspect" => cmd_inspect(&args[1..], stdout),
        other => Err(format!(
            "unknown subcommand '{other}' (run --help for usage) / 未知子命令 '{other}'（请运行 --help 查看帮助）"
        )),
    }
}

fn load_profile_spec(spec: &str, font_bytes: &[u8], face_index: u32) -> Result<FontProfile, Error> {
    match spec {
        "utn57" | "builtin:utn57" => Ok(FontProfile::builtin_utn57()),
        "auto" => generate_draft_profile(font_bytes, face_index, None),
        path => FontProfile::from_path(path),
    }
}

fn read_input_source(
    input_file: Option<&str>,
    positional: Option<&str>,
    stdin: &mut dyn Read,
) -> Result<String, String> {
    if let Some(path) = input_file {
        std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read '{path}': {e} / 读取输入文件 '{path}' 失败: {e}"))
    } else if let Some(text) = positional {
        Ok(text.to_owned())
    } else {
        let mut buf = String::new();
        stdin
            .read_to_string(&mut buf)
            .map_err(|e| format!("failed to read stdin: {e} / 读取标准输入失败: {e}"))?;
        if buf.ends_with('\n') {
            buf.pop();
            if buf.ends_with('\r') {
                buf.pop();
            }
        }
        Ok(buf)
    }
}

fn write_output_dest(output_file: Option<&str>, content: &str, stdout: &mut dyn Write) -> Result<(), String> {
    if let Some(path) = output_file {
        std::fs::write(path, content)
            .map_err(|e| format!("failed to write '{path}': {e} / 写入输出文件 '{path}' 失败: {e}"))
    } else {
        writeln!(stdout, "{content}").map_err(|e| e.to_string())
    }
}

fn cmd_shape(
    args: &[String],
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), String> {
    let mut font_path: Option<String> = None;
    let mut face_index: u32 = 0;
    let mut profile_spec = "utn57".to_owned();
    let mut format = "text".to_owned();
    let mut input_file: Option<String> = None;
    let mut output_file: Option<String> = None;
    let mut positional_text: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                let _ = write!(stdout, "{HELP_SHAPE}");
                return Ok(());
            }
            "-f" | "--font" => {
                i += 1;
                font_path = Some(next_arg(args, i, "--font")?);
            }
            "--face-index" => {
                i += 1;
                face_index = next_arg(args, i, "--face-index")?
                    .parse()
                    .map_err(|_| "invalid --face-index integer / --face-index 必须是非负整数".to_owned())?;
            }
            "-p" | "--profile" => {
                i += 1;
                profile_spec = next_arg(args, i, "--profile")?;
            }
            "--format" => {
                i += 1;
                format = next_arg(args, i, "--format")?;
            }
            "-i" | "--input" => {
                i += 1;
                input_file = Some(next_arg(args, i, "--input")?);
            }
            "-o" | "--output" => {
                i += 1;
                output_file = Some(next_arg(args, i, "--output")?);
            }
            arg if arg.starts_with('-') => {
                return Err(format!("unknown option '{arg}' / 未知选项 '{arg}'"));
            }
            pos => {
                positional_text = Some(pos.to_owned());
            }
        }
        i += 1;
    }

    let font_path = font_path.ok_or_else(|| {
        "missing required --font <FILE> option / 缺少必选参数 --font <字体文件>".to_owned()
    })?;
    let font_bytes = std::fs::read(&font_path)
        .map_err(|e| format!("cannot read font '{font_path}': {e} / 无法读取字体 '{font_path}': {e}"))?;
    let profile = load_profile_spec(&profile_spec, &font_bytes, face_index)
        .map_err(|e| e.to_string())?;
    let shaper = FontShaper::from_bytes(font_bytes, face_index, &profile)
        .map_err(|e| e.to_string())?;

    let input = read_input_source(input_file.as_deref(), positional_text.as_deref(), stdin)?;

    #[derive(Serialize)]
    struct JsonShapeResponse {
        font_name: String,
        profile_id: String,
        text: String,
        units: Vec<String>,
        positioned: Vec<String>,
        diagnostics: Diagnostics,
    }

    let rendered = match format.as_str() {
        "text" => {
            let out = shaper.normalize_text(&input).map_err(|e| e.to_string())?;
            emit_diag_warnings(&out.diagnostics, stderr);
            out.value
        }
        "units" => {
            let out = shaper.shape_units_str(&input).map_err(|e| e.to_string())?;
            emit_diag_warnings(&out.diagnostics, stderr);
            out.value
        }
        "positioned" => {
            let out = shaper.shape_positioned(&input).map_err(|e| e.to_string())?;
            emit_diag_warnings(&out.diagnostics, stderr);
            out.value
                .iter()
                .map(|u| format_positioned_unit(u.unit, u.position))
                .collect::<Vec<_>>()
                .join(" ")
        }
        "glyphs" => {
            let recs = shaper.shape_glyphs(&input).map_err(|e| e.to_string())?;
            let mut lines = Vec::with_capacity(recs.len());
            for r in recs {
                let gname = r.glyph_name.as_deref().unwrap_or("-");
                lines.push(format!(
                    "gid=#{:<4} name={:<24} cluster={:<3} char=U+{:04X} -> {:?}",
                    r.glyph_id, gname, r.cluster, r.ch as u32, r.entry
                ));
            }
            lines.join("\n")
        }
        "json" => {
            let pos = shaper.shape_positioned(&input).map_err(|e| e.to_string())?;
            let units = shaper.shape_units(&input).map_err(|e| e.to_string())?;
            let norm = shaper.normalize_text(&input).map_err(|e| e.to_string())?;
            let resp = JsonShapeResponse {
                font_name: shaper.font_name().to_owned(),
                profile_id: shaper.profile_id().to_owned(),
                text: norm.value,
                units: units.value.iter().map(|u| u.as_str().to_owned()).collect(),
                positioned: pos
                    .value
                    .iter()
                    .map(|u| format_positioned_unit(u.unit, u.position))
                    .collect(),
                diagnostics: norm.diagnostics,
            };
            serde_json::to_string_pretty(&resp).map_err(|e| e.to_string())?
        }
        other => {
            return Err(format!(
                "unsupported --format '{other}' (expected text|units|positioned|glyphs|json) / 不支持的输出格式 '{other}'"
            ))
        }
    };

    write_output_dest(output_file.as_deref(), &rendered, stdout)
}

fn emit_diag_warnings(diag: &Diagnostics, stderr: &mut dyn Write) {
    if !diag.is_clean() {
        let _ = writeln!(
            stderr,
            "warning (塑形告警): {} defect(s) detected (notdef={}, visible_fvs={}, chain_break={}, unmapped={})",
            diag.total_defects(),
            diag.notdef_count,
            diag.visible_fvs_count,
            diag.chain_break_count,
            diag.unmapped_glyph_count
        );
        for issue in diag.issues.iter().take(5) {
            let _ = writeln!(stderr, "  - {}", issue.summary());
        }
    }
}

fn cmd_detect(args: &[String], stdin: &mut dyn Read, stdout: &mut dyn Write) -> Result<(), String> {
    let mut pool_specs: Vec<String> = Vec::new();
    let mut corpus_mode = false;
    let mut include_latin_mapped = false;
    let mut json_mode = false;
    let mut input_file: Option<String> = None;
    let mut positional_text: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                let _ = write!(stdout, "{HELP_DETECT}");
                return Ok(());
            }
            "--pool" => {
                i += 1;
                pool_specs.push(next_arg(args, i, "--pool")?);
            }
            "--corpus" => corpus_mode = true,
            "--include-latin-mapped" => include_latin_mapped = true,
            "--json" => json_mode = true,
            "-i" | "--input" => {
                i += 1;
                input_file = Some(next_arg(args, i, "--input")?);
            }
            arg if arg.starts_with('-') => {
                return Err(format!("unknown option '{arg}' / 未知选项 '{arg}'"));
            }
            pos => positional_text = Some(pos.to_owned()),
        }
        i += 1;
    }

    if pool_specs.is_empty() {
        return Err(
            "at least one --pool <LABEL=FONT[:PROFILE]> is required / 至少需要通过 --pool 指定一个候选字体"
                .to_owned(),
        );
    }

    let mut pool = FontPool::new();
    pool.set_include_latin_mapped(include_latin_mapped);

    for spec in &pool_specs {
        let (label, rest) = spec.split_once('=').ok_or_else(|| {
            format!(
                "invalid --pool '{spec}': expected LABEL=FONT_PATH[:PROFILE] / --pool 格式错误，应为 标签=字体路径[:配置]"
            )
        })?;
        let (font_path, profile_spec) = parse_font_and_profile_pair(rest);
        let font_bytes = std::fs::read(font_path).map_err(|e| {
            format!("cannot read font '{font_path}': {e} / 无法读取字体 '{font_path}': {e}")
        })?;
        let profile =
            load_profile_spec(profile_spec, &font_bytes, 0).map_err(|e| e.to_string())?;
        let shaper = FontShaper::from_bytes(font_bytes, 0, &profile).map_err(|e| e.to_string())?;
        pool.push(label.to_owned(), shaper);
    }

    let input = read_input_source(input_file.as_deref(), positional_text.as_deref(), stdin)?;

    if corpus_mode {
        let report = pool.detect_corpus(input.lines()).map_err(|e| e.to_string())?;
        if json_mode {
            let s = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
            writeln!(stdout, "{s}").map_err(|e| e.to_string())?;
        } else {
            writeln!(
                stdout,
                "Corpus Font Identification Report / 语料级字体自动鉴定报告 (Lines / 总行数: {})",
                report.total_lines
            )
            .map_err(|e| e.to_string())?;
            writeln!(
                stdout,
                "Winner / 最佳匹配胜出字体: {}",
                report.winner_label.as_deref().unwrap_or("(none / 无)")
            )
            .map_err(|e| e.to_string())?;
            for (idx, r) in report.rankings.iter().enumerate() {
                writeln!(
                    stdout,
                    "  #{:<2} {:<18} clean={} defective={} penalty={} (notdef={}, fvs={}, break={}, unmapped={})",
                    idx + 1,
                    r.label,
                    r.clean_mongolian_lines,
                    r.defective_lines,
                    r.total_penalty,
                    r.total_diagnostics.notdef_count,
                    r.total_diagnostics.visible_fvs_count,
                    r.total_diagnostics.chain_break_count,
                    r.total_diagnostics.unmapped_glyph_count,
                )
                .map_err(|e| e.to_string())?;
            }
        }
    } else {
        let res = pool.detect_text(&input).map_err(|e| e.to_string())?;
        if json_mode {
            let s = serde_json::to_string_pretty(&res).map_err(|e| e.to_string())?;
            writeln!(stdout, "{s}").map_err(|e| e.to_string())?;
        } else {
            writeln!(
                stdout,
                "Status / 判别状态: {:?}\nBest UTN #57 Output / 推荐标准输出: {}",
                res.status, res.best_text
            )
            .map_err(|e| e.to_string())?;
            if res.distinct_clean_outputs.len() > 1 {
                writeln!(
                    stdout,
                    "Note / 提示: Multiple clean fonts produced different spellings ({}) — run --corpus on the whole document to lock the font! / 多款零错码字体产出不同拼写，建议对整篇文档加 --corpus 锁定唯一字体！",
                    res.distinct_clean_outputs.join(" | ")
                )
                .map_err(|e| e.to_string())?;
            }
            for c in &res.candidates {
                writeln!(
                    stdout,
                    "  - {:<18} [{}] penalty={} units={} -> {}",
                    c.label,
                    if c.diagnostics.is_clean() {
                        "CLEAN/干净"
                    } else {
                        "DEFECT/错码"
                    },
                    c.diagnostics.penalty_score(),
                    c.diagnostics.shaped_unit_count,
                    c.normalized_text
                )
                .map_err(|e| e.to_string())?;
            }
        }
    }

    Ok(())
}

fn parse_font_and_profile_pair(rest: &str) -> (&str, &str) {
    // Handle optional `:profile` suffix without breaking Windows `C:\...` drive letters.
    if let Some((left, right)) = rest.rsplit_once(':') {
        if !left.is_empty() && !(left.len() == 1 && left.as_bytes()[0].is_ascii_alphabetic()) {
            return (left, right);
        }
    }
    let lower = rest.to_ascii_lowercase();
    let default_prof = if lower.contains("noto") || lower.contains("utn57") || lower.contains("draftnew") {
        "utn57"
    } else {
        "auto"
    };
    (rest, default_prof)
}

fn cmd_init_map(args: &[String], stdout: &mut dyn Write) -> Result<(), String> {
    let mut font_path: Option<String> = None;
    let mut face_index: u32 = 0;
    let mut profile_id: Option<String> = None;
    let mut output_json: Option<String> = None;
    let mut html_out: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                let _ = write!(stdout, "{HELP_INIT_MAP}");
                return Ok(());
            }
            "-f" | "--font" => {
                i += 1;
                font_path = Some(next_arg(args, i, "--font")?);
            }
            "--face-index" => {
                i += 1;
                face_index = next_arg(args, i, "--face-index")?
                    .parse()
                    .map_err(|_| "invalid --face-index / 无效的字体集合索引".to_owned())?;
            }
            "--id" => {
                i += 1;
                profile_id = Some(next_arg(args, i, "--id")?);
            }
            "-o" | "--output" => {
                i += 1;
                output_json = Some(next_arg(args, i, "--output")?);
            }
            "--html" => {
                i += 1;
                html_out = Some(next_arg(args, i, "--html")?);
            }
            arg if arg.starts_with('-') => {
                return Err(format!("unknown option '{arg}' / 未知选项 '{arg}'"));
            }
            pos if font_path.is_none() => {
                font_path = Some(pos.to_owned());
            }
            other => {
                return Err(format!("unexpected argument '{other}' / 多余的位置参数 '{other}'"));
            }
        }
        i += 1;
    }

    let font_path = font_path.ok_or_else(|| {
        "missing <FONT_FILE> argument / 缺少输入字体文件路径 <FONT_FILE>".to_owned()
    })?;
    let output_json = output_json.ok_or_else(|| {
        "missing -o / --output <PROFILE.json> / 缺少输出 JSON 路径参数 -o <PROFILE.json>".to_owned()
    })?;

    let font_bytes = std::fs::read(&font_path)
        .map_err(|e| format!("cannot read font '{font_path}': {e} / 无法读取字体 '{font_path}': {e}"))?;
    let profile = generate_draft_profile(&font_bytes, face_index, profile_id.as_deref())
        .map_err(|e| e.to_string())?;
    let json_text = profile.to_json_pretty().map_err(|e| e.to_string())?;

    if let Some(parent) = Path::new(&output_json).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    std::fs::write(&output_json, format!("{json_text}\n")).map_err(|e| e.to_string())?;

    if let Some(html_path) = &html_out {
        let html = render_calibration_html(&font_bytes, face_index, &profile)
            .map_err(|e| e.to_string())?;
        if let Some(parent) = Path::new(html_path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
        }
        std::fs::write(html_path, html).map_err(|e| e.to_string())?;
    }

    writeln!(
        stdout,
        "Generated profile '{}' ({:?}): {} rules ({} pending review) -> {}\n已生成家族配置 '{}' ({:?})：共 {} 条规则（{} 条待复核）-> {}",
        profile.id,
        profile.encoding_class,
        profile.rules.len(),
        profile.pending_review_count(),
        output_json,
        profile.id,
        profile.encoding_class,
        profile.rules.len(),
        profile.pending_review_count(),
        output_json,
    )
    .map_err(|e| e.to_string())?;

    if let Some(html_path) = &html_out {
        writeln!(
            stdout,
            "Calibration HTML studio written to (DO NOT COMMIT TO GIT) / 本地可视化标定页已写入（切勿提交至 Git）: {html_path}"
        )
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

fn cmd_inspect(args: &[String], stdout: &mut dyn Write) -> Result<(), String> {
    let mut font_path: Option<String> = None;
    let mut face_index: u32 = 0;
    let mut json_mode = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                let _ = write!(stdout, "{HELP_INSPECT}");
                return Ok(());
            }
            "-f" | "--font" => {
                i += 1;
                font_path = Some(next_arg(args, i, "--font")?);
            }
            "--face-index" => {
                i += 1;
                face_index = next_arg(args, i, "--face-index")?
                    .parse()
                    .map_err(|_| "invalid --face-index / 无效的字体集合索引".to_owned())?;
            }
            "--json" => json_mode = true,
            arg if arg.starts_with('-') => {
                return Err(format!("unknown option '{arg}' / 未知选项 '{arg}'"));
            }
            pos if font_path.is_none() => {
                font_path = Some(pos.to_owned());
            }
            other => {
                return Err(format!("unexpected argument '{other}' / 多余参数 '{other}'"));
            }
        }
        i += 1;
    }

    let font_path = font_path.ok_or_else(|| {
        "missing <FONT_FILE> argument / 缺少输入字体文件路径 <FONT_FILE>".to_owned()
    })?;
    let font_bytes = std::fs::read(&font_path)
        .map_err(|e| format!("cannot read font '{font_path}': {e} / 无法读取字体 '{font_path}': {e}"))?;
    let report = inspect_font(&font_bytes, face_index).map_err(|e| e.to_string())?;

    if json_mode {
        let s = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        writeln!(stdout, "{s}").map_err(|e| e.to_string())?;
    } else {
        writeln!(
            stdout,
            "Font / 字体名称:           {}\n\
             Outline / 轮廓格式:        {} ({} glyphs, UPEM={})\n\
             Tables / 关键排版表:       {}\n\
             Named Glyphs / 具名字形数: {} {}\n\
             Mongolian CPs / 蒙文码位:  {} (Hudum 1820..1842: {}, Extended: {})\n\
             PUA CPs / 私用区码位:      {} {}\n\
             Inferred Class / 推断类别: {:?}",
            report.font_name,
            report.outline_format,
            report.num_glyphs,
            report.units_per_em,
            report.layout_tables.join(", "),
            report.named_glyph_count,
            if report.named_glyph_count == 0 {
                "(post v3.0 nameless — use dynamic probes / 无字形名黑盒字体，将启用动态探针绑定)"
            } else {
                ""
            },
            report.mongolian_codepoint_count,
            report.hudum_letter_count,
            report.extended_mongolian_count,
            report.pua_codepoint_count,
            report.pua_range.as_deref().unwrap_or(""),
            report.inferred_encoding_class,
        )
        .map_err(|e| e.to_string())?;
    }

    let _ = CompiledGlyph::Silent; // keep import used cleanly
    Ok(())
}

fn next_arg(args: &[String], idx: usize, flag: &str) -> Result<String, String> {
    args.get(idx).cloned().ok_or_else(|| {
        format!("option '{flag}' requires a value / 选项 '{flag}' 需要提供参数值")
    })
}
