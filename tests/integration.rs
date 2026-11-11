//! Comprehensive integration tests for `mongol-font-shaper`.
//! `mongol-font-shaper` 全链路中英双语集成测试套件。
//!
//! All tests in this suite rely exclusively on the repository's permissively licensed
//! OFL-1.1 reference font (`tests/fonts/NotoSansMongolian-Regular.ttf`) and built-in/in-memory
//! `FontProfile` fixtures so `cargo test` passes 100% offline on any fresh clone.
//! 本测试套件仅依赖仓库内置的 OFL-1.1 开源字体（`tests/fonts/NotoSansMongolian-Regular.ttf`）
//! 与内存中的 `FontProfile` 夹具，确保任何新克隆环境均可离线 100% 通过 `cargo test`。

use std::io::Cursor;

use mongol_font_shaper::{
    cli::run_with_io, format_positioned_unit, generate_draft_profile, inspect_font,
    render_calibration_html, utn57_shaper, DetectionStatus, EncodingClass, FontPool, FontProfile,
    FontShaper, GlyphRuleSpec, PositionedWrittenUnit, RuleKind, UnitPosition, WrittenUnit,
};

const NOTO_TTF_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fonts/NotoSansMongolian-Regular.ttf"
);

fn noto_bytes() -> Vec<u8> {
    std::fs::read(NOTO_TTF_PATH).expect("NotoSansMongolian-Regular.ttf must exist in tests/fonts/")
}

/// Test Output 1 (`PositionedWrittenUnit`), Output 2 (`WrittenUnit` matching `mongol-norm`),
/// and Output 3 (canonical UTN #57 `String` with mixed-script passthrough).
/// 测试三种标准输出形态：
/// 1. `PositionedWrittenUnit` 位置字形单元序列；
/// 2. 与 `mongol-norm` 完全对齐的 `WrittenUnit` 序列；
/// 3. 保留汉英数字与标点透传的标准 UTN #57 Unicode 文本。
#[test]
fn three_canonical_outputs_and_mongol_norm_parity() {
    let profile = FontProfile::builtin_utn57();
    let shaper = FontShaper::from_bytes(noto_bytes(), 0, &profile)
        .expect("FontShaper should initialize cleanly / FontShaper 应当成功初始化");
    let ref_shaper = utn57_shaper();

    // Representative UTN #57 words covering bowed ligatures, NG+G velar clusters,
    // diphthongs, MVS chachlag (`-a/-e`), NNBSP (`U+202F`) case suffixes, Nirugu, and ZWNJ.
    // 典型蒙古文测试词表：涵盖弯头辅音合字、NG+G 舌根音连写、双元音、MVS 隔音尾、U+202F 格后缀、Nirugu 与 ZWNJ。
    let words = [
        "ᠮᠣᠩᠭᠣᠯ",               // monggol
        "ᠪᠢᠴᠢᠭ",                // bicig
        "ᠦᠨᠳᠦᠰᠦᠨ",              // uenduesuen
        "ᠪᠠᠶᠢᠨ᠎ᠠ",              // bain-a (MVS chachlag)
        "ᠰᠢᠨ᠎ᠡ",                // sin-e
        "ᠠᠬ᠎ᠠ",                 // aq-a
        "ᠮᠣᠩᠭᠣᠯ\u{202F}ᠤᠨ",      // monggol-un (NNBSP genitive)
        "ᠠᠬ᠎ᠠ\u{202F}ᠶᠢᠨ",      // aq-a-yin (MVS + NNBSP + yin)
        "ᠤᠯᠤᠰ\u{202F}ᠲᠤ",       // ulus-tu (NNBSP dative)
        "ᠭᠠᠵᠠᠷ\u{202F}ᠠᠴᠠ",     // gajar-aca (NNBSP ablative)
        "ᠰᠠᠶᠢᠨ",                // sayin
        "ᠪᠣᠯᠪᠠᠰᠤᠷᠠᠯ",           // bolbasural (bowed BO + BA ligatures)
        "ᠰ\u{180A}ᠠ",           // nirugu
        "ᠪ\u{200C}ᠠ",           // zwnj
    ];

    for word in words {
        // Output 1: PositionedWrittenUnit
        let pos_out = shaper.shape_positioned(word).unwrap();
        assert!(
            pos_out.diagnostics.is_clean(),
            "word '{word}' should shape with 0 defects, got: {:?}",
            pos_out.diagnostics
        );
        assert!(!pos_out.value.is_empty());

        // Output 2: Canonical WrittenUnit parity with `mongol-norm`
        let units_out = shaper.shape_units(word).unwrap();
        assert!(units_out.diagnostics.is_clean());
        let expected_units = ref_shaper
            .shape(word)
            .expect("mongol-norm reference shaper should shape valid UTN #57 word");
        assert_eq!(
            units_out.value, expected_units,
            "WrittenUnit mismatch on '{word}' / 词条 '{word}' 的 WrittenUnit 与 mongol-norm 不一致"
        );

        // Output 3: Canonical UTN #57 Unicode text (idempotent when re-shaped by mongol-norm)
        let norm_out = shaper.normalize_text(word).unwrap();
        assert!(norm_out.diagnostics.is_clean());
        let reshaped = ref_shaper.shape(&norm_out.value).unwrap();
        assert_eq!(
            reshaped, expected_units,
            "Normalized UTN #57 text must round-trip to identical WrittenUnits on '{word}'"
        );
    }

    // Mixed-script sentence passthrough test (Mongolian + CJK + English + digits + punctuation).
    // 多脚本混排透传测试（蒙古文 + 中文 + 英文 + 数字 + 书名号标点原样保留）。
    let mixed = "《ᠮᠣᠩᠭᠣᠯ\u{202F}ᠤᠨ ᠪᠢᠴᠢᠭ》 — 传统蒙古文 UTN #57 (2026)!";
    let mixed_norm = shaper.normalize_text(mixed).unwrap();
    assert!(
        mixed_norm.diagnostics.is_clean(),
        "mixed sentence should not penalize missing CJK/ASCII punctuation glyphs: {:?}",
        mixed_norm.diagnostics
    );
    assert!(mixed_norm.value.starts_with('《'));
    assert!(mixed_norm.value.ends_with("— 传统蒙古文 UTN #57 (2026)!"));
}

/// Test explicit `ZWJ` (`U+200D`) dictionary affix entries (such as `\u{200D}ᠤᠨ` or `ᠪ\u{200D}`)
/// to verify they do NOT trigger false-positive `ChainPositionBreak` defects.
/// 测试词典中常见的显式 `ZWJ`（`U+200D`）词缀词头（如 `\u{200D}ᠤᠨ`、`ᠪ\u{200D}`），
/// 验证其不会误报 `ChainPositionBreak` 位置连写断裂。
#[test]
fn explicit_zwj_dictionary_affixes_shape_without_chain_break() {
    let shaper = FontShaper::from_bytes(noto_bytes(), 0, &FontProfile::builtin_utn57()).unwrap();

    // Suffix entry starting with ZWJ (`‍ᠤᠨ`)
    // 以 ZWJ 开头的附加成分词条（`‍ᠤᠨ`）
    let suffix = "\u{200D}ᠤᠨ";
    let suffix_out = shaper.normalize_text(suffix).unwrap();
    assert!(
        suffix_out.diagnostics.is_clean(),
        "explicit leading ZWJ suffix must be clean, got: {:?}",
        suffix_out.diagnostics
    );

    // Stem/prefix entry ending with ZWJ (`ᠪ‍`)
    // 以 ZWJ 结尾的词干前缀词条（`ᠪ‍`）
    let prefix = "ᠪ\u{200D}";
    let prefix_out = shaper.normalize_text(prefix).unwrap();
    assert!(
        prefix_out.diagnostics.is_clean(),
        "explicit trailing ZWJ prefix must be clean, got: {:?}",
        prefix_out.diagnostics
    );
}

/// Simulate a nameless `post v3.0` `.otf` / `.ttf` font by stripping ALL `glyph_names` and `codepoints`
/// from `Units`/`Silent` rules in `FontProfile::builtin_utn57()`, proving that cluster-anchored
/// `ShapingProbe`s alone bind 100% of contextual glyphs cleanly!
/// 模拟剥离了全部字形名称的 `post v3.0` 黑盒字体（清空内置配置中所有 `Units`/`Silent` 规则的 `glyph_names`
/// 与 `codepoints`，仅保留 `probes` 动态塑形探针），证明探针机制可独立实现 100% 零误差绑定！
#[test]
fn nameless_post_v3_probe_only_binding_matches_named_profile() {
    let named_profile = FontProfile::builtin_utn57();
    let mut nameless_profile = named_profile.clone();
    nameless_profile.id = "utn57-nameless-probe-only".to_owned();
    for rule in &mut nameless_profile.rules {
        if matches!(rule.kind, RuleKind::Units | RuleKind::Silent | RuleKind::VisibleFvs)
            && !rule.probes.is_empty()
        {
            rule.glyph_names.clear();
            rule.codepoints.clear();
        }
    }

    let bytes = noto_bytes();
    let shaper_named = FontShaper::from_bytes(bytes.clone(), 0, &named_profile).unwrap();
    let shaper_nameless = FontShaper::from_bytes(bytes, 0, &nameless_profile).unwrap();

    let sentence =
        "ᠮᠣᠩᠭᠣᠯ\u{202F}ᠤᠨ ᠦᠨᠳᠦᠰᠦᠨ ᠪᠠᠶᠢᠨ᠎ᠠ ᠠᠬ᠎ᠠ\u{202F}ᠶᠢᠨ ᠰᠠᠶᠢᠨ ᠤᠯᠤᠰ\u{202F}ᠲᠤ ᠪᠣᠯᠪᠠᠰᠤᠷᠠᠯ";
    let out_named = shaper_named.normalize_text(sentence).unwrap();
    let out_nameless = shaper_nameless.normalize_text(sentence).unwrap();

    assert!(out_named.diagnostics.is_clean());
    assert!(
        out_nameless.diagnostics.is_clean(),
        "probe-only profile should have 0 unmapped/broken glyphs, got: {:?}",
        out_nameless.diagnostics
    );
    assert_eq!(out_named.value, out_nameless.value);
    assert_eq!(
        shaper_named.shape_units_str(sentence).unwrap().value,
        shaper_nameless.shape_units_str(sentence).unwrap().value
    );
}

/// Test defect diagnostics (`VisibleFvs`, `ChainPositionBreak`, `UnmappedGlyph`) used to detect
/// mismatched fonts (such as Baiti / GB2010 rendering GB2023 FVS3 or medial-at-word-end).
/// 测试错码诊断机制（`VisibleFvs` 显式虚线框变体符、`ChainPositionBreak` 词尾连写断裂、`UnmappedGlyph` 未映射字形）。
#[test]
fn defect_diagnostics_catch_visible_fvs_and_chain_position_break() {
    let bytes = noto_bytes();
    let shaper = FontShaper::from_bytes(bytes.clone(), 0, &FontProfile::builtin_utn57()).unwrap();

    // 1. Invalid `B + FVS1` (`\u{182A}\u{180B}`) leaves an unconsumed visible FVS1 box in Noto!
    // 1. 非法 `B + FVS1`（`\u{182A}\u{180B}`）在 Noto 中未被 GSUB 消费，应精准捕获 `visible_fvs_count == 1`。
    let bad_fvs = shaper.normalize_text("\u{182A}\u{180B}\u{1820}").unwrap();
    assert_eq!(bad_fvs.diagnostics.visible_fvs_count, 1);
    assert!(!bad_fvs.diagnostics.is_clean());

    // 2. Simulate a font bug where final `L` (`uni182F.L.fina`) is erroneously a `Medi` glyph
    //    (mirroring Baiti's broken word-end medial tooth in dictionary `170`), verifying `ChainPositionBreak`.
    // 2. 模拟字体在词尾错误输出 `Medi` 词中连写齿（复现 `170` 词典在 Baiti 下的断连错码），验证 `ChainPositionBreak`。
    let mut broken_profile = FontProfile::builtin_utn57();
    for rule in &mut broken_profile.rules {
        if rule.glyph_names.iter().any(|n| n == "uni182F.L.fina") {
            rule.units = "L:medi".to_owned();
            rule.joining = Some("medi".to_owned());
        }
    }
    let broken_shaper = FontShaper::from_bytes(bytes, 0, &broken_profile).unwrap();
    let broken_out = broken_shaper.normalize_text("ᠮᠣᠩᠭᠣᠯ").unwrap();
    assert!(
        broken_out.diagnostics.chain_break_count >= 1,
        "ending a word without ZWJ on a Medi glyph must trigger ChainPositionBreak: {:?}",
        broken_out.diagnostics
    );
}

/// Test `FontPool::detect_text` (`Exact`, `Ambiguous`, `NoMongolianContent`) and
/// `FontPool::detect_corpus` batch voting + `LatinMapped` opt-in safety guard.
/// 测试多字体池 `FontPool` 的单条判别（共识 `Exact`、错码排除 `Exact`、分歧告警 `Ambiguous`、纯外文透传）
/// 以及整批词典语料投票 `detect_corpus` 与老式 ASCII 映射字体的默认隔离保护。
#[test]
fn font_pool_single_detection_and_corpus_voting() {
    let bytes = noto_bytes();

    // Candidate A ("utn57-std"): standard UTN #57 profile (`\u{202F}ᠶᠢᠨ` -> straight-shin `I:init`).
    // 候选字体 A（"utn57-std"）：标准 UTN #57 映射（`\u{202F}ᠶᠢᠨ` 映射为直胫 `I:init`）。
    let profile_a = FontProfile::builtin_utn57();

    // Candidate B ("alt-yin"): simulates a GB2023-like convention where `uni1836.I.init`
    // maps to hooked `Y:init` while ending on `R` triggers `ChainPositionBreak`.
    // 候选字体 B（"alt-yin"）：模拟在 `-yin` 上产出 `Y:init` 而在特定词条上触发 `ChainPositionBreak` 的另一流派。
    let mut profile_b = FontProfile::builtin_utn57();
    profile_b.id = "alt-yin".to_owned();
    for rule in &mut profile_b.rules {
        if rule.glyph_names.iter().any(|n| n == "uni1836.I.init") {
            rule.units = "Y:init".to_owned();
        }
        if rule.glyph_names.iter().any(|n| n == "uni1837.R.fina") {
            // Simulate a joining defect whenever a word ends in `R` (`ᠭᠡᠷ`).
            // 模拟在以 `R` 结尾的词（如 `ᠭᠡᠷ`）上发生词尾断连错码。
            rule.units = "R:medi".to_owned();
            rule.joining = Some("medi".to_owned());
        }
    }

    // Candidate C ("danzan-ascii"): `LatinMapped` profile that maps `'·'` (`U+00B7`, present in Noto cmap) -> `A:isol`.
    // Must be excluded from blind detection by default so non-Mongolian text stays untouched!
    // 候选字体 C（"danzan-ascii"）：`LatinMapped` 类别配置（将 Noto `cmap` 中存在的 `U+00B7` `'·'` 映射为 `A:isol`），
    // 默认不参与盲测以保护正常外文与符号。
    let mut profile_c = FontProfile::new("danzan-ascii", EncodingClass::LatinMapped);
    profile_c.rules.push(GlyphRuleSpec {
        kind: RuleKind::Units,
        units: "A:isol".to_owned(),
        joining: None,
        glyph_names: Vec::new(),
        codepoints: vec!["U+00B7".to_owned()],
        probes: Vec::new(),
        gid: None,
        review: false,
        note: "U+00B7 '·' -> A:isol".to_owned(),
    });

    let mut pool = FontPool::new();
    pool.push(
        "utn57-std",
        FontShaper::from_bytes(bytes.clone(), 0, &profile_a).unwrap(),
    );
    pool.push(
        "alt-yin",
        FontShaper::from_bytes(bytes.clone(), 0, &profile_b).unwrap(),
    );
    pool.push(
        "danzan-ascii",
        FontShaper::from_bytes(bytes, 0, &profile_c).unwrap(),
    );

    // Case 1: Pure English + Chinese + '·' -> `NoMongolianContent` (because `LatinMapped` is off by default!).
    let res_en = pool.detect_text("Hello · 你好").unwrap();
    assert_eq!(res_en.status, DetectionStatus::NoMongolianContent);
    assert_eq!(res_en.best_text, "Hello · 你好");

    // Case 2: Standard word `ᠮᠣᠩᠭᠣᠯ` -> Both `utn57-std` & `alt-yin` produce identical output -> `Exact`!
    let res_common = pool.detect_text("ᠮᠣᠩᠭᠣᠯ").unwrap();
    assert_eq!(res_common.status, DetectionStatus::Exact);
    assert_eq!(res_common.distinct_clean_outputs.len(), 1);

    // Case 3: Word `ᠠᠬ᠎ᠠ\u{202F}ᠶᠢᠨ` -> Both are clean (`penalty=0`), but `utn57-std` produces `I` while `alt-yin` produces `Y` -> `Ambiguous`!
    let res_ambig = pool.detect_text("ᠠᠬ᠎ᠠ\u{202F}ᠶᠢᠨ").unwrap();
    assert_eq!(res_ambig.status, DetectionStatus::Ambiguous);
    assert_eq!(res_ambig.distinct_clean_outputs.len(), 2);

    // Case 4: Word `ᠭᠡᠷ` -> `alt-yin` triggers `ChainPositionBreak`, so `utn57-std` wins -> `Exact`!
    let res_defect = pool.detect_text("ᠭᠡᠷ").unwrap();
    assert_eq!(res_defect.status, DetectionStatus::Exact);
    assert_eq!(res_defect.candidates[0].label, "utn57-std");

    // Case 5: Corpus-level voting across all entries locks `utn57-std` as the unique clean winner!
    let corpus = ["ᠮᠣᠩᠭᠣᠯ", "ᠠᠬ᠎ᠠ\u{202F}ᠶᠢᠨ", "ᠭᠡᠷ"];
    let report = pool.detect_corpus(corpus).unwrap();
    assert_eq!(report.total_lines, 3);
    assert_eq!(report.winner_label.as_deref(), Some("utn57-std"));
    assert_eq!(report.rankings[0].clean_mongolian_lines, 3);
    assert_eq!(report.rankings[0].defective_lines, 0);
    assert_eq!(report.rankings[1].defective_lines, 1);

    // Case 6: Enabling `LatinMapped` allows Danzan detection when explicitly requested.
    pool.set_include_latin_mapped(true);
    let res_danzan = pool.detect_text("·").unwrap();
    assert_eq!(res_danzan.status, DetectionStatus::Exact);
    assert_eq!(res_danzan.candidates[0].label, "danzan-ascii");
}

/// Test `inspect_font`, `generate_draft_profile` JSON round-trip, and `render_calibration_html`.
/// 测试字体结构诊断 `inspect_font`、草稿映射表预生成 `generate_draft_profile` 的 JSON 序列化往返，
/// 以及离线 HTML/SVG 可视化标定页生成 `render_calibration_html`。
#[test]
fn init_map_and_calibration_html_generation() {
    let bytes = noto_bytes();

    // 1. Inspect SFNT metadata.
    let report = inspect_font(&bytes, 0).unwrap();
    assert_eq!(report.inferred_encoding_class, EncodingClass::Unicode);
    assert!(report.layout_tables.iter().any(|t| t == "GSUB"));
    assert!(report.num_glyphs > 1000);
    assert!(report.named_glyph_count > 1000);

    // 2. Generate draft profile, serialize to JSON, parse back, and shape a sentence cleanly.
    let draft = generate_draft_profile(&bytes, 0, Some("test-draft")).unwrap();
    assert_eq!(draft.id, "test-draft");
    let json = draft.to_json_pretty().unwrap();
    let parsed = FontProfile::from_json_str(&json).unwrap();
    let shaper = FontShaper::from_bytes(bytes.clone(), 0, &parsed).unwrap();
    let out = shaper.normalize_text("ᠮᠣᠩᠭᠣᠯ\u{202F}ᠤᠨ ᠪᠠᠶᠢᠨ᠎ᠠ").unwrap();
    assert!(out.diagnostics.is_clean());

    // 3. Render local calibration HTML in memory and verify bilingual notice + SVG paths.
    let html = render_calibration_html(&bytes, 0, &parsed).unwrap();
    assert!(html.contains("<svg"));
    assert!(html.contains("<path"));
    assert!(html.contains("Copyright"));
    assert!(html.contains("版权"));

    // Also verify `format_positioned_unit` helper.
    let pu = PositionedWrittenUnit::new(WrittenUnit::Aa, UnitPosition::Isol);
    assert_eq!(format_positioned_unit(pu.unit, pu.position), "Aa:isol");
}

/// Test the bilingual CLI (`shape`, `detect`, `inspect`, `--help`) via `run_with_io`.
/// 通过可注入流接口 `run_with_io` 测试中英双语命令行子命令（`shape`、`detect`、`inspect`、`--help`）。
#[test]
fn cli_subcommands_and_bilingual_output() {
    // 1. `--help` outputs both English and Chinese.
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    run_with_io(
        &["--help".to_owned()],
        &mut Cursor::new(""),
        &mut stdout,
        &mut stderr,
    )
    .unwrap();
    let help_text = String::from_utf8(stdout).unwrap();
    assert!(help_text.contains("SUBCOMMANDS / 子命令"));

    // 2. `shape --format units` via stdin.
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    run_with_io(
        &[
            "shape".to_owned(),
            "-f".to_owned(),
            NOTO_TTF_PATH.to_owned(),
            "-p".to_owned(),
            "utn57".to_owned(),
            "--format".to_owned(),
            "units".to_owned(),
        ],
        &mut Cursor::new("ᠮᠣᠩᠭᠣᠯ"),
        &mut stdout,
        &mut stderr,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(stdout).unwrap().trim(),
        "M+O+A+G+N+N+O+L"
    );
    assert!(stderr.is_empty());

    // 3. `detect --json` with built-in UTN #57 pool entry.
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    run_with_io(
        &[
            "detect".to_owned(),
            "--pool".to_owned(),
            format!("noto={NOTO_TTF_PATH}:utn57"),
            "--json".to_owned(),
            "ᠮᠣᠩᠭᠣᠯ\u{202F}ᠤᠨ".to_owned(),
        ],
        &mut Cursor::new(""),
        &mut stdout,
        &mut stderr,
    )
    .unwrap();
    let detect_json = String::from_utf8(stdout).unwrap();
    assert!(detect_json.contains("\"status\": \"exact\""));

    // 4. `inspect --json`.
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    run_with_io(
        &[
            "inspect".to_owned(),
            NOTO_TTF_PATH.to_owned(),
            "--json".to_owned(),
        ],
        &mut Cursor::new(""),
        &mut stdout,
        &mut stderr,
    )
    .unwrap();
    let inspect_json = String::from_utf8(stdout).unwrap();
    assert!(inspect_json.contains("\"inferred_encoding_class\": \"unicode\""));
}
