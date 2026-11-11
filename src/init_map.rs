//! Font inspection, draft `FontProfile` pre-generation (`init-map`), and local HTML/SVG
//! visual calibration page builder.
//!
//! 字体结构检查、家族级 `FontProfile` 预生成（`init-map`）以及本地离线 HTML/SVG 可视化人工标定页生成器。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use harfrust::{
    shape, Buffer, BufferFlags, ClusterLevel, Direction, Font, Language, Script, ShapeOptions,
    ShaperFont,
};
use mongol_norm::{Position, PositionedWrittenUnit, TokenDetail, UnitPosition, WrittenUnit};
use read_fonts::model::glyph::outline::OutlinePen;
use read_fonts::ps::cff::CffFontRef;
use read_fonts::tables::glyf::outline::{Outline, OutlineTables, PathContourStart, Unscaled};
use read_fonts::types::{GlyphId, Tag};
use read_fonts::{FontRef, TableProvider};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::profile::{
    extract_font_identity_name, format_positioned_unit_list, glyph_has_ink, EncodingClass,
    FontProfile, GlyphRuleSpec, RuleKind, ShapingProbe, PROFILE_SCHEMA_V1,
};
use crate::shaper::utn57_shaper;

/// Structural summary of a font file returned by [`inspect_font`].
/// [`inspect_font`] 返回的字体文件内部结构与编码特征体检摘要。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontInspectionReport {
    /// Internal PostScript or family name / 字体内部 PostScript 名或家族名。
    pub font_name: String,
    /// Face index inside a `.ttc` collection (`0` for `.ttf`/`.otf`) / 字体集合中的子字体索引。
    pub face_index: u32,
    /// Total number of glyphs / 字形总数。
    pub num_glyphs: u32,
    /// Design units per em (`UPEM`) / 每 Em 设计单位数。
    pub units_per_em: u16,
    /// Outline format (`"glyf"`, `"CFF"`, `"CFF2"`, or `"none"`) / 字形轮廓表格式。
    pub outline_format: String,
    /// Present SFNT layout/shaping tables (e.g. `["GSUB", "GPOS", "GDEF"]` or `["morx"]`) / 包含的关键排版表标签。
    pub layout_tables: Vec<String>,
    /// Count of non-synthesized glyph names in `post`/`CFF` (`0` means `post v3.0` nameless) / 非合成真实字形名数量（为 `0` 表示 `post v3.0` 无名黑盒字体）。
    pub named_glyph_count: usize,
    /// Count of mapped codepoints in `U+1800..=U+18AF` / 标准蒙古文区映射码位数。
    pub mongolian_codepoint_count: usize,
    /// Count of mapped Hudum letter codepoints in `U+1820..=U+1842` / 胡都木基础字母区映射码位数。
    pub hudum_letter_count: usize,
    /// Count of mapped Todo/Sibe/Manchu/AliGali codepoints in `U+1843..=U+18AA` / 托忒/锡伯/满/阿礼嘎礼扩展区码位数。
    pub extended_mongolian_count: usize,
    /// Count of mapped Private Use Area codepoints in `U+E000..=U+F8FF` / 私用区（PUA）映射码位数。
    pub pua_codepoint_count: usize,
    /// Min..Max PUA codepoint range formatted as `"U+E234..U+E34F"`, if any / 私用区码位起止范围。
    pub pua_range: Option<String>,
    /// Count of mapped printable ASCII codepoints in `0x21..=0x7E` / 可见 ASCII 区域映射码位数。
    pub ascii_codepoint_count: usize,
    /// Automatically inferred [`EncodingClass`] / 自动推断的编码流派类别。
    pub inferred_encoding_class: EncodingClass,
}

/// Inspect a font binary and summarize its tables, naming coverage, codepoint ranges, and encoding class.
/// 检查字体二进制结构，汇总其排版表、字形名覆盖度、码位分布及推断的编码类别。
pub fn inspect_font(font_bytes: &[u8], face_index: u32) -> Result<FontInspectionReport, Error> {
    let font = Font::new(font_bytes.to_vec(), face_index).ok_or_else(|| {
        Error::InvalidFont(format!(
            "cannot parse SFNT/TTF/OTF font at face index {face_index} / 无法解析索引 {face_index} 处的字体文件"
        ))
    })?;
    let font_ref = FontRef::from_index(font_bytes, face_index).map_err(|e| {
        Error::InvalidFont(format!(
            "read-fonts failed to parse font header: {e} / 字体表头解析失败: {e}"
        ))
    })?;

    let font_name = extract_font_identity_name(&font);
    let num_glyphs = font.num_glyphs();
    let units_per_em = font.units_per_em();

    let outline_format = if font_ref.data_for_tag(Tag::new(b"glyf")).is_some() {
        "glyf".to_owned()
    } else if font_ref.data_for_tag(Tag::new(b"CFF2")).is_some() {
        "CFF2".to_owned()
    } else if font_ref.data_for_tag(Tag::new(b"CFF ")).is_some() {
        "CFF".to_owned()
    } else {
        "none".to_owned()
    };

    let mut layout_tables = Vec::new();
    for tag_bytes in [
        b"GSUB", b"GPOS", b"GDEF", b"morx", b"kerx", b"aat ", b"cmap", b"post",
    ] {
        if font_ref.data_for_tag(Tag::new(tag_bytes)).is_some() {
            layout_tables.push(
                std::str::from_utf8(tag_bytes)
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            );
        }
    }

    let named_glyph_count = font
        .glyph_names()
        .filter(|(_, gname)| !gname.is_synthesized())
        .count();

    let mut mongolian_codepoint_count = 0usize;
    let mut hudum_letter_count = 0usize;
    let mut extended_mongolian_count = 0usize;
    let mut pua_codepoint_count = 0usize;
    let mut pua_min: Option<u32> = None;
    let mut pua_max: Option<u32> = None;
    let mut ascii_codepoint_count = 0usize;

    for (cp, gid) in font.charmap().iter_unicodes() {
        if gid.to_u32() == 0 {
            continue;
        }
        if (0x1800..=0x18AF).contains(&cp) {
            mongolian_codepoint_count += 1;
        }
        if (0x1820..=0x1842).contains(&cp) {
            hudum_letter_count += 1;
        }
        if (0x1843..=0x18AA).contains(&cp) {
            extended_mongolian_count += 1;
        }
        if (0xE000..=0xF8FF).contains(&cp) {
            pua_codepoint_count += 1;
            pua_min = Some(pua_min.map_or(cp, |m| m.min(cp)));
            pua_max = Some(pua_max.map_or(cp, |m| m.max(cp)));
        }
        if (0x0021..=0x007E).contains(&cp) {
            ascii_codepoint_count += 1;
        }
    }

    let has_dynamic_layout = layout_tables
        .iter()
        .any(|t| t == "GSUB" || t == "morx");

    let inferred_encoding_class = infer_encoding_class(
        &font_name,
        has_dynamic_layout,
        hudum_letter_count,
        extended_mongolian_count,
        pua_codepoint_count,
        ascii_codepoint_count,
    );

    let pua_range = match (pua_min, pua_max) {
        (Some(lo), Some(hi)) => Some(format!("U+{lo:04X}..U+{hi:04X}")),
        _ => None,
    };

    Ok(FontInspectionReport {
        font_name,
        face_index,
        num_glyphs,
        units_per_em,
        outline_format,
        layout_tables,
        named_glyph_count,
        mongolian_codepoint_count,
        hudum_letter_count,
        extended_mongolian_count,
        pua_codepoint_count,
        pua_range,
        ascii_codepoint_count,
        inferred_encoding_class,
    })
}

fn infer_encoding_class(
    font_name: &str,
    has_dynamic_layout: bool,
    hudum_letter_count: usize,
    extended_mongolian_count: usize,
    pua_codepoint_count: usize,
    ascii_codepoint_count: usize,
) -> EncodingClass {
    let lower = font_name.to_ascii_lowercase();
    if lower.contains("z52")
        || (!has_dynamic_layout && extended_mongolian_count >= 40 && hudum_letter_count >= 20)
    {
        return EncodingClass::Z52;
    }
    if hudum_letter_count >= 25 && has_dynamic_layout {
        return EncodingClass::Unicode;
    }
    if pua_codepoint_count >= 50 {
        return EncodingClass::Pua;
    }
    if hudum_letter_count == 0 && pua_codepoint_count == 0 && ascii_codepoint_count >= 40 {
        return EncodingClass::LatinMapped;
    }
    if hudum_letter_count > 0 {
        EncodingClass::Unicode
    } else {
        EncodingClass::Pua
    }
}

/// Pre-generate a human-editable [`FontProfile`] from a `.ttf` / `.otf` / `.ttc` font binary.
///
/// Strategy:
/// 1. Parse explicit `post` / `CFF` glyph names (Noto / `mongfontbuilder` exact names + vendor control names);
/// 2. Run a built-in canonical Mongolian probe vocabulary through `harfrust` + `mongol-norm` token alignment
///    to populate portable `probes` AND infer glyph units on vendor/nameless Unicode fonts;
/// 3. For `Pua` / `Z52` / `LatinMapped` fonts, enumerate mapped `cmap` codepoints into editable rules.
///
/// 从 `.ttf` / `.otf` / `.ttc` 字体二进制预生成人类可读、可手工编辑的家族级 [`FontProfile`]：
/// 1. 解析 `post` / `CFF` 字形名（对 Noto / `mongfontbuilder` 标准命名及控制符直接给出 100% 精确规则）；
/// 2. 在字体上实测内置蒙古文标准探针词表，自动补充跨 `.ttf`/无名字形 `.otf` 通用的 `probes`，并为蒙科立/奥云/德力海/Baiti 等字体自动推断字形单元映射；
/// 3. 对 `Pua` / `Z52` / `LatinMapped` 等纯码位字体，按 `cmap` 码位导出待人工核对的规则骨架。
pub fn generate_draft_profile(
    font_bytes: &[u8],
    face_index: u32,
    profile_id_opt: Option<&str>,
) -> Result<FontProfile, Error> {
    let report = inspect_font(font_bytes, face_index)?;
    let font = Font::new(font_bytes.to_vec(), face_index).ok_or_else(|| {
        Error::InvalidFont("failed to load font / 无法加载字体".to_owned())
    })?;

    let profile_id = profile_id_opt
        .map(str::to_owned)
        .unwrap_or_else(|| sanitize_profile_id(&report.font_name));

    let num_glyphs = font.num_glyphs() as usize;

    // Collect non-synthesized glyph names per GID.
    // 收集每个 GID 的真实非合成字形名称。
    let mut gid_to_name: HashMap<u16, String> = HashMap::new();
    for (gid, gname) in font.glyph_names() {
        if !gname.is_synthesized() {
            if let Ok(id16) = u16::try_from(gid.to_u32()) {
                gid_to_name.insert(id16, gname.as_str().to_owned());
            }
        }
    }

    // Collect cmap codepoints per GID.
    // 收集每个 GID 在 `cmap` 中绑定的码位列表。
    let mut gid_to_cps: HashMap<u16, Vec<u32>> = HashMap::new();
    for (cp, gid) in font.charmap().iter_unicodes() {
        if let Ok(id16) = u16::try_from(gid.to_u32()) {
            if id16 != 0 {
                gid_to_cps.entry(id16).or_default().push(cp);
            }
        }
    }
    for cps in gid_to_cps.values_mut() {
        cps.sort_unstable();
        cps.dedup();
    }

    let rules = match report.inferred_encoding_class {
        EncodingClass::Unicode => generate_unicode_rules(&font, num_glyphs, &gid_to_name, &gid_to_cps)?,
        EncodingClass::Pua | EncodingClass::Z52 | EncodingClass::LatinMapped => {
            generate_codepoint_rules(
                &font,
                report.inferred_encoding_class,
                &gid_to_name,
                &gid_to_cps,
            )
        }
    };

    let description = format!(
        "Draft family profile generated from {} ({:?}, {} rules) / 基于 {} 自动预生成的家族级字形映射配置（{:?}，共 {} 条规则）",
        report.font_name,
        report.inferred_encoding_class,
        rules.len(),
        report.font_name,
        report.inferred_encoding_class,
        rules.len(),
    );

    Ok(FontProfile {
        schema: PROFILE_SCHEMA_V1.to_owned(),
        id: profile_id,
        description,
        encoding_class: report.inferred_encoding_class,
        language: "mn".to_owned(),
        rules,
        overrides: Vec::new(),
    })
}

fn sanitize_profile_id(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "custom-mongol".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SemanticTarget {
    kind_rank: u8,
    units: String,
    joining: Option<String>,
}

impl SemanticTarget {
    fn from_parts(kind: RuleKind, units: String, joining: Option<String>) -> Self {
        let kind_rank = match kind {
            RuleKind::Units => 0,
            RuleKind::Silent => 1,
            RuleKind::VisibleFvs => 2,
            RuleKind::Passthrough => 3,
            RuleKind::NotDef => 4,
        };
        Self {
            kind_rank,
            units,
            joining,
        }
    }

    fn kind(&self) -> RuleKind {
        match self.kind_rank {
            0 => RuleKind::Units,
            1 => RuleKind::Silent,
            2 => RuleKind::VisibleFvs,
            3 => RuleKind::Passthrough,
            _ => RuleKind::NotDef,
        }
    }
}

#[derive(Default)]
struct GidInference {
    exact_target: Option<SemanticTarget>,
    probe_votes: BTreeMap<SemanticTarget, usize>,
    probes: Vec<ShapingProbe>,
}

fn generate_unicode_rules(
    font: &Font,
    num_glyphs: usize,
    gid_to_name: &HashMap<u16, String>,
    gid_to_cps: &HashMap<u16, Vec<u32>>,
) -> Result<Vec<GlyphRuleSpec>, Error> {
    let hud_inventory: HashSet<(WrittenUnit, Position)> = utn57_shaper()
        .positioned_written_units()?
        .into_iter()
        .collect();
    let hudum_vocab = build_hudum_unit_vocab();

    let mut per_gid: HashMap<u16, GidInference> = HashMap::new();

    // Pass 1: Parse explicit glyph names (`post` / `CFF`).
    // 第一轮：直接解析明确的 `post` / `CFF` 字形名称（如 Noto / `mongfontbuilder` 标准名及通用控制符名）。
    for gid in 1..(num_glyphs.min(u16::MAX as usize) as u16) {
        if let Some(name) = gid_to_name.get(&gid) {
            if let Some(target) = parse_known_glyph_name(font, gid, name, &hudum_vocab, &hud_inventory)
            {
                per_gid.entry(gid).or_default().exact_target = Some(target);
            }
        }
        // Also check direct cmap control characters (`U+25CC`, `U+180A`, `U+200C`, `U+200D`, `U+180E`, `U+202F`).
        if let Some(cps) = gid_to_cps.get(&gid) {
            for &cp in cps {
                let direct_target = match cp {
                    0x25CC => Some(SemanticTarget::from_parts(
                        RuleKind::VisibleFvs,
                        String::new(),
                        None,
                    )),
                    0x180A => Some(SemanticTarget::from_parts(
                        RuleKind::Units,
                        "Nirugu".to_owned(),
                        None,
                    )),
                    0x200C => Some(SemanticTarget::from_parts(
                        RuleKind::Units,
                        "Zwnj".to_owned(),
                        None,
                    )),
                    0x200D => Some(SemanticTarget::from_parts(
                        RuleKind::Silent,
                        String::new(),
                        None,
                    )),
                    0x180B..=0x180F => {
                        let kind = if glyph_has_ink(font, GlyphId::new(u32::from(gid)))
                            && cp != 0x180E
                        {
                            RuleKind::VisibleFvs
                        } else if cp == 0x180E {
                            RuleKind::Units
                        } else {
                            RuleKind::Silent
                        };
                        let units = if cp == 0x180E {
                            "Mvs".to_owned()
                        } else {
                            String::new()
                        };
                        Some(SemanticTarget::from_parts(kind, units, None))
                    }
                    0x202F => Some(SemanticTarget::from_parts(
                        RuleKind::Units,
                        "Mvs".to_owned(),
                        None,
                    )),
                    _ => None,
                };
                if let Some(t) = direct_target {
                    let entry = per_gid.entry(gid).or_default();
                    if entry.exact_target.is_none() {
                        entry.exact_target = Some(t);
                    }
                }
            }
        }
    }

    // Pass 2: Run canonical shaping probes to attach portable `ShapingProbe`s AND infer vendor/nameless glyphs!
    // 第二轮：运行内置标准蒙古文探针词库，为已有规则挂载便携 `probes`（通吃无名字形 `.otf`），并为蒙科立/奥云/德力海/Baiti 推断映射！
    let language = Language::new("mn");
    let shaper_font = ShaperFont::new(font);

    // 2a. Explicit invalid FVS probes (`B + FVS1..4`) to discover visible error FVS glyphs.
    for fvs_cp in ['\u{180B}', '\u{180C}', '\u{180D}', '\u{180F}'] {
        let probe_str = format!("\u{182A}{fvs_cp}");
        if let Some(glyphs) = shape_probe_raw(&shaper_font, language.as_ref(), &probe_str) {
            for (g_idx, &(gid, cluster)) in glyphs.iter().enumerate() {
                if cluster == 1 && gid != 0 {
                    let kind = if glyph_has_ink(font, GlyphId::new(u32::from(gid))) {
                        RuleKind::VisibleFvs
                    } else {
                        RuleKind::Silent
                    };
                    let target = SemanticTarget::from_parts(kind, String::new(), None);
                    let entry = per_gid.entry(gid).or_default();
                    *entry.probe_votes.entry(target).or_insert(0) += 1;
                    push_gid_probe(
                        &mut entry.probes,
                        ShapingProbe {
                            text: probe_str.clone(),
                            glyph_index: g_idx,
                            cluster: Some(1),
                            end_cluster: None,
                        },
                    );
                }
            }
        }
    }

    // 2b. Canonical word & syllable probe suite.
    for probe_str in build_canonical_probe_words() {
        let Ok(tokens) = utn57_shaper().shape_detailed(&probe_str) else {
            continue;
        };
        let Some(glyphs) = shape_probe_raw(&shaper_font, language.as_ref(), &probe_str) else {
            continue;
        };
        if glyphs.iter().any(|&(gid, _)| gid == 0) {
            continue;
        }
        align_probe_tokens_and_glyphs(
            font,
            &probe_str,
            &tokens,
            &glyphs,
            &hud_inventory,
            &mut per_gid,
        );
    }

    // Consolidate per-GID inferences into portable family-level `GlyphRuleSpec` rules.
    // Keep 1 clean representative probe per distinct GID so nameless `post v3.0` fonts bind 100% of variant GIDs!
    // 将单字形推断结果归并为家族级规则，并为每个不同的 GID 保留 1 条最优探针，确保 `post v3.0` 无名黑盒字体 100% 命中全部变体字形！
    #[derive(Default)]
    struct MergedRule {
        glyph_names: BTreeSet<String>,
        probes: Vec<ShapingProbe>,
        fallback_gids: Vec<u16>,
        review: bool,
        sample_notes: Vec<String>,
    }

    let mut merged: BTreeMap<SemanticTarget, MergedRule> = BTreeMap::new();

    let mut sorted_gids: Vec<u16> = per_gid.keys().copied().collect();
    sorted_gids.sort_unstable();

    for gid in sorted_gids {
        let inf = &per_gid[&gid];
        let (target, needs_review) = if let Some(exact) = &inf.exact_target {
            (exact.clone(), false)
        } else if !inf.probe_votes.is_empty() {
            let total_distinct = inf.probe_votes.len();
            let best = inf
                .probe_votes
                .iter()
                .max_by_key(|(_, &cnt)| cnt)
                .map(|(t, _)| t.clone())
                .unwrap();
            (best, total_distinct > 1)
        } else {
            continue;
        };

        let bucket = merged.entry(target).or_default();
        if needs_review {
            bucket.review = true;
        }
        if let Some(gname) = gid_to_name.get(&gid) {
            bucket.glyph_names.insert(gname.clone());
            // Also add mongfontbuilder <-> Noto alias (`uniXXXX.` <-> `uXXXX.`) when applicable!
            // 自动补充 Noto 与 `mongfontbuilder` 常见命名前缀同义词（`uni1820.` <-> `u1820.`），增强跨字体兼容性！
            if let Some(rest) = gname.strip_prefix("uni18") {
                bucket.glyph_names.insert(format!("u18{rest}"));
            } else if let Some(rest) = gname.strip_prefix("u18") {
                if rest.len() >= 3 && rest.as_bytes()[2] == b'.' {
                    bucket.glyph_names.insert(format!("uni18{rest}"));
                }
            }
        }
        if let Some(best_probe) = inf.probes.first() {
            if !bucket
                .probes
                .iter()
                .any(|p| p.text == best_probe.text && p.glyph_index == best_probe.glyph_index)
            {
                bucket.probes.push(best_probe.clone());
            }
            if bucket.sample_notes.len() < 2 {
                let sample = format!("{}[{}]", best_probe.text, best_probe.glyph_index);
                if !bucket.sample_notes.contains(&sample) {
                    bucket.sample_notes.push(sample);
                }
            }
        }
        if !gid_to_name.contains_key(&gid) && inf.probes.is_empty() {
            bucket.fallback_gids.push(gid);
        }
    }

    let mut out_rules = Vec::with_capacity(merged.len());
    for (target, item) in merged {
        let gid = if item.glyph_names.is_empty() && item.probes.is_empty() {
            item.fallback_gids.first().copied()
        } else {
            None
        };
        let note = if item.review && !item.sample_notes.is_empty() {
            format!(
                "inferred from probes ({}) / 由探针自动推断，建议核对",
                item.sample_notes.join(", ")
            )
        } else {
            String::new()
        };
        out_rules.push(GlyphRuleSpec {
            kind: target.kind(),
            units: target.units,
            joining: target.joining,
            glyph_names: item.glyph_names.into_iter().collect(),
            codepoints: Vec::new(),
            probes: item.probes,
            gid,
            review: item.review,
            note,
        });
    }

    Ok(out_rules)
}

fn generate_codepoint_rules(
    font: &Font,
    encoding_class: EncodingClass,
    gid_to_name: &HashMap<u16, String>,
    gid_to_cps: &HashMap<u16, Vec<u32>>,
) -> Vec<GlyphRuleSpec> {
    let mut cp_entries: Vec<(u32, u16)> = Vec::new();
    for (&gid, cps) in gid_to_cps {
        for &cp in cps {
            let include = match encoding_class {
                EncodingClass::Pua => (0xE000..=0xF8FF).contains(&cp),
                EncodingClass::Z52 => (0x1820..=0x18AA).contains(&cp) || (0xE000..=0xF8FF).contains(&cp),
                EncodingClass::LatinMapped => (0x0021..=0x007E).contains(&cp),
                EncodingClass::Unicode => false,
            };
            if include && glyph_has_ink(font, GlyphId::new(u32::from(gid))) {
                cp_entries.push((cp, gid));
            }
        }
    }
    cp_entries.sort_unstable();

    let mut rules = Vec::with_capacity(cp_entries.len());
    for (cp, gid) in cp_entries {
        let gname_opt = gid_to_name.get(&gid).cloned();
        let is_known_punct = matches!(cp, 0xE234..=0xE243)
            || gname_opt
                .as_deref()
                .is_some_and(|n| n.to_ascii_lowercase().contains("birga"));

        let (kind, units, review, note) = if is_known_punct {
            (
                RuleKind::Passthrough,
                String::new(),
                true,
                format!("PUA punctuation candidate U+{cp:04X} / 疑似标点码位，请核对"),
            )
        } else {
            (
                RuleKind::Units,
                "A:isol".to_owned(),
                true,
                format!(
                    "TODO: fill PositionedWrittenUnit for U+{cp:04X} (gid #{gid}) / 待填写该码位对应的字形单元"
                ),
            )
        };

        rules.push(GlyphRuleSpec {
            kind,
            units,
            joining: None,
            glyph_names: gname_opt.into_iter().collect(),
            codepoints: vec![format!("U+{cp:04X}")],
            probes: Vec::new(),
            gid: None,
            review,
            note,
        });
    }
    rules
}

fn has_fvs_char(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c as u32, 0x180B..=0x180D | 0x180F))
}

fn push_gid_probe(probes: &mut Vec<ShapingProbe>, candidate: ShapingProbe) {
    if probes
        .iter()
        .any(|p| p.text == candidate.text && p.glyph_index == candidate.glyph_index)
    {
        return;
    }
    // Prefer FVS-free probes at index 0 so cross-font binding is maximally stable!
    if !has_fvs_char(&candidate.text) && probes.first().is_some_and(|p| has_fvs_char(&p.text)) {
        probes.insert(0, candidate);
        probes.truncate(3);
    } else if probes.len() < 3 {
        probes.push(candidate);
    }
}

fn shape_probe_raw(
    shaper_font: &ShaperFont<'_, '_>,
    language: Option<&Language>,
    text: &str,
) -> Option<Vec<(u16, usize)>> {
    let mut buffer = Buffer::new();
    buffer.set_cluster_level(ClusterLevel::MonotoneCharacters);
    buffer.set_flags(
        BufferFlags::BEGINNING_OF_TEXT
            | BufferFlags::END_OF_TEXT
            | BufferFlags::PRESERVE_DEFAULT_IGNORABLES,
    );
    for (idx, ch) in text.chars().enumerate() {
        buffer.push(ch as u32, idx as u32);
    }
    buffer.set_direction(Direction::LeftToRight);
    buffer.set_script(Some(Script::MONGOLIAN));
    buffer.set_language(language.cloned());
    shape(shaper_font, &mut buffer, ShapeOptions::new()).ok()?;
    let mut out = Vec::with_capacity(buffer.len());
    for info in buffer.glyph_infos() {
        out.push((u16::try_from(info.glyph_id).ok()?, info.cluster as usize));
    }
    Some(out)
}

/// Align `mongol-norm`'s `TokenDetail` sequence with `harfrust`'s output `(gid, cluster)` stream
/// to infer exact `PositionedWrittenUnit` mappings and record `ShapingProbe`s.
///
/// 将 `mongol-norm` 的 `TokenDetail` 序列与 `harfrust` 产出的 `(gid, cluster)` 字形流按字符簇对齐，
/// 自动提取准确的 `PositionedWrittenUnit` 映射并记录动态探针 `ShapingProbe`。
fn align_probe_tokens_and_glyphs(
    font: &Font,
    probe_str: &str,
    tokens: &[TokenDetail],
    glyphs: &[(u16, usize)],
    hud_inventory: &HashSet<(WrittenUnit, Position)>,
    per_gid: &mut HashMap<u16, GidInference>,
) {
    // Map each TokenDetail to its starting char index in `probe_str`.
    // 计算每个 `TokenDetail` 在 `probe_str` 中的起始字符下标及覆盖区间 `[start, end)`。
    let chars: Vec<char> = probe_str.chars().collect();
    let mut token_spans: Vec<(usize, usize, &TokenDetail)> = Vec::with_capacity(tokens.len());
    let mut cursor = 0usize;
    for tok in tokens {
        // Note: `mongol-norm` folds `U+202F` (NNBSP) into `U+180E` (MVS) during tokenization.
        // 注意：`mongol-norm` 分词阶段会将 `U+202F` 折叠为 `U+180E`，对齐探针字符簇时需兼容两者。
        while cursor < chars.len()
            && !(chars[cursor] == tok.cp || (chars[cursor] == '\u{202F}' && tok.cp == '\u{180E}'))
        {
            cursor += 1;
        }
        if cursor >= chars.len() {
            return;
        }
        let start = cursor;
        cursor += 1;
        while cursor < chars.len() && matches!(chars[cursor] as u32, 0x180B..=0x180D | 0x180F) {
            cursor += 1;
        }
        token_spans.push((start, cursor, tok));
    }

    // Group output glyphs into non-silent primary glyphs + zero-ink trailing FVS/helper glyphs.
    let mut g_idx = 0usize;
    while g_idx < glyphs.len() {
        let (gid, c_start) = glyphs[g_idx];
        let ch = chars.get(c_start).copied().unwrap_or('\0');

        // Zero-ink FVS / ZWJ helper glyph -> Silent.
        if matches!(ch as u32, 0x180B..=0x180D | 0x180F | 0x200D)
            && !glyph_has_ink(font, GlyphId::new(u32::from(gid)))
        {
            let target = SemanticTarget::from_parts(RuleKind::Silent, String::new(), None);
            let entry = per_gid.entry(gid).or_default();
            *entry.probe_votes.entry(target).or_insert(0) += 1;
            push_gid_probe(
                &mut entry.probes,
                ShapingProbe {
                    text: probe_str.to_owned(),
                    glyph_index: g_idx,
                    cluster: Some(c_start),
                    end_cluster: None,
                },
            );
            g_idx += 1;
            continue;
        }

        // Find next non-helper glyph's cluster to determine how many tokens `gid` ligated!
        let mut next_g = g_idx + 1;
        while next_g < glyphs.len() {
            let (ngid, nc) = glyphs[next_g];
            let nch = chars.get(nc).copied().unwrap_or('\0');
            let is_zero_fvs = (nc == c_start
                || matches!(nch as u32, 0x180B..=0x180D | 0x180F | 0x200D))
                && !glyph_has_ink(font, GlyphId::new(u32::from(ngid)));
            if is_zero_fvs {
                let target = SemanticTarget::from_parts(RuleKind::Silent, String::new(), None);
                let entry = per_gid.entry(ngid).or_default();
                *entry.probe_votes.entry(target).or_insert(0) += 1;
                push_gid_probe(
                    &mut entry.probes,
                    ShapingProbe {
                        text: probe_str.to_owned(),
                        glyph_index: next_g,
                        cluster: Some(nc),
                        end_cluster: None,
                    },
                );
                next_g += 1;
            } else {
                break;
            }
        }

        let c_end = if next_g < glyphs.len() {
            glyphs[next_g].1
        } else {
            chars.len()
        };

        if c_end > c_start {
            let covered: Vec<&TokenDetail> = token_spans
                .iter()
                .filter(|&&(ts, te, _)| ts >= c_start && te <= c_end)
                .map(|&(_, _, t)| t)
                .collect();

            if let Some(target) = infer_target_from_covered_tokens(&covered, hud_inventory) {
                let entry = per_gid.entry(gid).or_default();
                *entry.probe_votes.entry(target).or_insert(0) += 1;
                push_gid_probe(
                    &mut entry.probes,
                    ShapingProbe {
                        text: probe_str.to_owned(),
                        glyph_index: g_idx,
                        cluster: Some(c_start),
                        end_cluster: Some(c_end),
                    },
                );
            }
        } else if c_end == c_start && matches!(ch, '\u{180E}' | '\u{202F}') {
            // Shared-cluster MVS + chachlag vowel (`[mvs.vert, U1820_MVS]` both at cluster `c_start`):
            // the leading glyph at `g_idx` is the `Mvs` separator glyph itself.
            // 共享字符簇的 MVS + 隔音尾元音（如 `[mvs.vert, U1820_MVS]` 同属簇 `c_start`）：前导字形即为 `Mvs` 分隔字形。
            let target = SemanticTarget::from_parts(RuleKind::Units, "Mvs".to_owned(), None);
            let entry = per_gid.entry(gid).or_default();
            *entry.probe_votes.entry(target).or_insert(0) += 1;
            push_gid_probe(
                &mut entry.probes,
                ShapingProbe {
                    text: probe_str.to_owned(),
                    glyph_index: g_idx,
                    cluster: Some(c_start),
                    end_cluster: None,
                },
            );
        }

        g_idx = next_g;
    }
}

fn infer_target_from_covered_tokens(
    covered: &[&TokenDetail],
    hud_inventory: &HashSet<(WrittenUnit, Position)>,
) -> Option<SemanticTarget> {
    if covered.is_empty() {
        return None;
    }
    if covered.len() == 1 {
        let tok = covered[0];
        match tok.cp {
            '\u{180E}' | '\u{202F}' => {
                return Some(SemanticTarget::from_parts(
                    RuleKind::Units,
                    "Mvs".to_owned(),
                    None,
                ))
            }
            '\u{180A}' => {
                return Some(SemanticTarget::from_parts(
                    RuleKind::Units,
                    "Nirugu".to_owned(),
                    None,
                ))
            }
            '\u{200C}' => {
                return Some(SemanticTarget::from_parts(
                    RuleKind::Units,
                    "Zwnj".to_owned(),
                    None,
                ))
            }
            '\u{200D}' => {
                return Some(SemanticTarget::from_parts(
                    RuleKind::Silent,
                    String::new(),
                    None,
                ))
            }
            _ => {}
        }
    }

    // Filter to letter tokens with non-empty `written` units (ignoring ZWJ padding tokens).
    let letter_toks: Vec<&&TokenDetail> = covered
        .iter()
        .filter(|t| !t.written.is_empty() && !matches!(t.cp, '\u{200D}' | '\u{200C}' | '\u{180E}' | '\u{202F}' | '\u{180A}'))
        .collect();
    if letter_toks.is_empty() {
        return None;
    }

    let units: Vec<WrittenUnit> = letter_toks
        .iter()
        .flat_map(|t| t.written.iter().copied())
        .collect();
    let first_pos = letter_toks[0].position;
    let last_pos = letter_toks[letter_toks.len() - 1].position;
    let effective_joining = combine_token_positions(first_pos, last_pos);
    let first_cp = letter_toks[0].cp;

    build_letter_semantic_target(&units, None, effective_joining, Some(first_cp), hud_inventory)
}

fn combine_token_positions(first: Position, last: Position) -> Position {
    match (first, last) {
        (Position::Isol, Position::Isol) => Position::Isol,
        (Position::Init, Position::Fina) => Position::Isol,
        (Position::Init, _) => Position::Init,
        (_, Position::Fina) => Position::Fina,
        _ => Position::Medi,
    }
}

fn build_letter_semantic_target(
    units: &[WrittenUnit],
    explicit_form: Option<Position>,
    joining: Position,
    source_cp: Option<char>,
    hud_inventory: &HashSet<(WrittenUnit, Position)>,
) -> Option<SemanticTarget> {
    if units.is_empty() {
        return None;
    }
    let positioned: Vec<PositionedWrittenUnit> = if units.len() == 1 {
        let u = units[0];
        let form = if let Some(f) = explicit_form {
            f
        } else if joining == Position::Isol
            && ((u == WrittenUnit::A && source_cp.is_some_and(|c| !matches!(c, '\u{1820}' | '\u{1821}')))
                || (u == WrittenUnit::I && source_cp.is_some_and(|c| c != '\u{1822}')))
        {
            // Consonant borrowing single tooth `A:init` or `I:init` when isolated (e.g. `u1828.A.init._isol`, `u1835.I.init._isol`).
            Position::Init
        } else if hud_inventory.contains(&(u, joining)) {
            joining
        } else if hud_inventory.contains(&(u, Position::Init)) {
            Position::Init
        } else if hud_inventory.contains(&(u, Position::Medi)) {
            Position::Medi
        } else if hud_inventory.contains(&(u, Position::Fina)) {
            Position::Fina
        } else if hud_inventory.contains(&(u, Position::Isol)) {
            Position::Isol
        } else {
            return None;
        };
        if !hud_inventory.contains(&(u, form)) {
            return None;
        }
        vec![PositionedWrittenUnit::new(u, to_unit_position(form))]
    } else {
        let first_p = match joining {
            Position::Isol | Position::Init => Position::Init,
            _ => Position::Medi,
        };
        let last_p = match joining {
            Position::Isol | Position::Fina => Position::Fina,
            _ => Position::Medi,
        };
        let mut out = Vec::with_capacity(units.len());
        for (idx, &u) in units.iter().enumerate() {
            let p = if idx == 0 {
                first_p
            } else if idx + 1 == units.len() {
                last_p
            } else {
                Position::Medi
            };
            if !hud_inventory.contains(&(u, p)) {
                return None;
            }
            out.push(PositionedWrittenUnit::new(u, to_unit_position(p)));
        }
        out
    };

    let units_str = format_positioned_unit_list(&positioned);

    // Check if default inferred joining span matches `joining`; only emit explicit `joining` when needed!
    let default_joining = if positioned.len() == 1 {
        positioned[0].position
    } else {
        match (positioned[0].position, positioned[positioned.len() - 1].position) {
            (UnitPosition::Init, UnitPosition::Fina) => UnitPosition::Isol,
            (UnitPosition::Init, _) => UnitPosition::Init,
            (_, UnitPosition::Fina) => UnitPosition::Fina,
            _ => UnitPosition::Medi,
        }
    };
    let target_unit_pos = to_unit_position(joining);
    let joining_override = if default_joining != target_unit_pos {
        Some(position_tag(joining).to_owned())
    } else {
        None
    };

    Some(SemanticTarget::from_parts(
        RuleKind::Units,
        units_str,
        joining_override,
    ))
}

fn to_unit_position(pos: Position) -> UnitPosition {
    match pos {
        Position::Isol => UnitPosition::Isol,
        Position::Init => UnitPosition::Init,
        Position::Medi => UnitPosition::Medi,
        Position::Fina => UnitPosition::Fina,
    }
}

fn position_tag(pos: Position) -> &'static str {
    match pos {
        Position::Isol => "isol",
        Position::Init => "init",
        Position::Medi => "medi",
        Position::Fina => "fina",
    }
}

fn parse_pos_str(s: &str) -> Option<Position> {
    match s {
        "isol" => Some(Position::Isol),
        "init" => Some(Position::Init),
        "medi" => Some(Position::Medi),
        "fina" => Some(Position::Fina),
        _ => None,
    }
}

fn build_hudum_unit_vocab() -> Vec<(&'static str, WrittenUnit)> {
    let mut v: Vec<(&'static str, WrittenUnit)> = WrittenUnit::ALL
        .iter()
        .copied()
        .filter(|u| {
            !matches!(
                u,
                WrittenUnit::Mvs | WrittenUnit::Nirugu | WrittenUnit::Zwj | WrittenUnit::Zwnj
            )
        })
        .map(|u| (u.as_str(), u))
        .collect();
    v.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(b.0)));
    v
}

/// Unambiguously segment compact Hudum unit names such as `"BAa"` -> `[B, Aa]` or `"AGHx"` -> `[A, G, Hx]`.
/// 无歧义切分紧凑胡都木字形单元名（如 `"BAa"` -> `[B, Aa]`，`"AGHx"` -> `[A, G, Hx]`）。
fn segment_hudum_units(s: &str, vocab: &[(&'static str, WrittenUnit)]) -> Option<Vec<WrittenUnit>> {
    if s.is_empty() {
        return None;
    }
    let n = s.len();
    let mut ways = vec![0u8; n + 1];
    let mut next_choice: Vec<Option<(WrittenUnit, usize)>> = vec![None; n + 1];
    ways[n] = 1;
    for i in (0..n).rev() {
        if !s.is_char_boundary(i) {
            continue;
        }
        let mut count = 0u8;
        let mut pick = None;
        for &(name, unit) in vocab {
            let end = i + name.len();
            if end <= n && s[i..].starts_with(name) && ways[end] > 0 {
                if count == 0 && ways[end] == 1 {
                    pick = Some((unit, end));
                } else {
                    pick = None;
                }
                count = (count + ways[end]).min(2);
                if count == 2 {
                    break;
                }
            }
        }
        ways[i] = count;
        if count == 1 {
            next_choice[i] = pick;
        }
    }
    if ways[0] != 1 {
        return None;
    }
    let mut out = Vec::new();
    let mut cur = 0;
    while cur < n {
        let (u, nxt) = next_choice[cur]?;
        out.push(u);
        cur = nxt;
    }
    Some(out)
}

/// Parse well-known `post` / `CFF` glyph names (Noto Sans Mongolian, `mongfontbuilder`, and standard controls).
/// 解析行业通用的明确字形名（Noto Sans Mongolian、`mongfontbuilder` 以及常见控制符名称）。
fn parse_known_glyph_name(
    font: &Font,
    gid: u16,
    name: &str,
    vocab: &[(&'static str, WrittenUnit)],
    hud_inventory: &HashSet<(WrittenUnit, Position)>,
) -> Option<SemanticTarget> {
    if name.starts_with('_') || name == ".notdef" {
        return None;
    }

    match name {
        "mvs" | "mvs.vert" | "mvs.narrow" | "mvs.nominal" | "mvs.wide" | "nnbsp" | "MVS"
        | "MSC" | "MMSC" => {
            return Some(SemanticTarget::from_parts(
                RuleKind::Units,
                "Mvs".to_owned(),
                None,
            ))
        }
        "nirugu" | "nirugu.extend" | "NIRUGU" | "uni180A.nirugu" => {
            return Some(SemanticTarget::from_parts(
                RuleKind::Units,
                "Nirugu".to_owned(),
                None,
            ))
        }
        "zwnj" | "ZWNJ" => {
            return Some(SemanticTarget::from_parts(
                RuleKind::Units,
                "Zwnj".to_owned(),
                None,
            ))
        }
        "zwj" | "zwj.ignored" | "zwnj.ignored" | "nirugu.ignored" | "lvs.ignored" | "ZWJ"
        | "zerowidth" | "masculine" | "feminine" | ".null" | "NUL" | "NULL"
        | "nonmarkingreturn" | "fvs1.effective" | "fvs2.effective" | "fvs3.effective"
        | "fvs4.effective" => {
            return Some(SemanticTarget::from_parts(
                RuleKind::Silent,
                String::new(),
                None,
            ))
        }
        "uni25CC" | "dottedcircle" | "DFVS1" | "DFVS2" | "DFVS3" | "DFVS4" | "DZWJ" | "DZWNJ"
        | "DMVS" | "DMSC" | "fvs1.ignored" | "fvs2.ignored" | "fvs3.ignored" | "fvs4.ignored"
        | "fvs1" | "fvs2" | "fvs3" | "fvs4" | "FVS1" | "FVS2" | "FVS3" | "FVS4" => {
            let kind = if glyph_has_ink(font, GlyphId::new(u32::from(gid))) {
                RuleKind::VisibleFvs
            } else {
                RuleKind::Silent
            };
            return Some(SemanticTarget::from_parts(kind, String::new(), None));
        }
        "N.fina.mvs" => {
            return Some(SemanticTarget::from_parts(
                RuleKind::Units,
                "N:fina".to_owned(),
                None,
            ))
        }
        "Hx.fina.mvs" => {
            return Some(SemanticTarget::from_parts(
                RuleKind::Units,
                "Hx:fina".to_owned(),
                None,
            ))
        }
        _ => {}
    }

    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() >= 3 && (parts[0].starts_with("uni18") || parts[0].starts_with("u18")) {
        // Filter out non-Hudum single codepoints (`U+1843..U+18AA` Todo/Sibe/Manchu/AliGali and `U+1807`).
        let first_hex = parts[0]
            .split('_')
            .next()
            .unwrap_or("")
            .trim_start_matches("uni")
            .trim_start_matches('u');
        if let Ok(cp) = u32::from_str_radix(first_hex, 16) {
            if !(0x1820..=0x1842).contains(&cp) {
                return None;
            }
        }
        let units = segment_hudum_units(parts[1], vocab)?;
        let form = parse_pos_str(parts[2])?;
        let joining = if parts.len() >= 4 {
            parts[3]
                .strip_prefix('_')
                .and_then(parse_pos_str)
                .unwrap_or(form)
        } else {
            form
        };
        return build_letter_semantic_target(&units, Some(form), joining, None, hud_inventory);
    }

    if parts.len() == 2 {
        let units = segment_hudum_units(parts[0], vocab)?;
        let form = parse_pos_str(parts[1])?;
        return build_letter_semantic_target(&units, Some(form), form, None, hud_inventory);
    }

    None
}

/// Generate a thorough canonical Mongolian probe corpus covering:
/// - All Hudum base letters (`U+1820..U+1842`) in `isol`, `init`, `medi`, `fina` contexts;
/// - All FVS1..FVS4 combinations on base letters in `isol`, `init`, `medi`, `fina`;
/// - All bowed consonant (`B`, `P`, `H`, `G`, `F`, `K`, `K2`) + vowel/liquid (`A..W`, `M`, `L`) ligatures;
/// - MVS chachlag (`-a`/`-e` after `N`, `H`, `G`, `B`, `S`, `R`, `L`, `M`) and NNBSP (`U+202F`) case suffixes;
/// - Nirugu (`U+180A`) and `ZWNJ` (`U+200C`) contexts.
///
/// 生成全面覆盖胡都木字形与连写环境的标准探针词表（孤立/词首/词中/词尾、全部 FVS1..4 组合、
/// 弯头辅音+元音合字、MVS 隔音尾与 U+202F 附加格后缀、Nirugu 延音符及 ZWNJ 断连环境）。
fn build_canonical_probe_words() -> Vec<String> {
    let mut probes = Vec::with_capacity(640);

    let hudum_letters: Vec<char> = (0x1820u32..=0x1842u32)
        .filter_map(char::from_u32)
        .collect();

    // 1. Bare letters in isolated, initial, medial (pre-vocalic & pre-consonantal), and final contexts.
    // 1. 无变体选择符的基础字母（孤立、词首、元音前/辅音前词中、词尾语境）。
    for &ch in &hudum_letters {
        probes.push(format!("{ch}"));
        probes.push(format!("{ch}\u{1820}\u{182F}")); // X + a + l (init masculine / 阳性词首)
        probes.push(format!("{ch}\u{1821}\u{182F}")); // X + e + l (init feminine / 阴性词首)
        probes.push(format!("{ch}\u{1828}")); // X + n (init pre-consonantal / 辅音前词首)
        probes.push(format!("\u{1830}\u{1820}{ch}\u{1820}\u{182F}")); // sa + X + al (medi masc / 阳性元音前词中)
        probes.push(format!("\u{1830}\u{1821}{ch}\u{1821}\u{182F}")); // se + X + el (medi fem / 阴性元音前词中)
        probes.push(format!("\u{1830}\u{1820}{ch}\u{1828}")); // sa + X + n (medi masc pre-consonantal / 阳性辅音前词中)
        probes.push(format!("\u{1830}\u{1821}{ch}\u{1828}")); // se + X + n (medi fem pre-consonantal / 阴性辅音前词中)
        probes.push(format!("\u{1830}\u{1820}{ch}")); // sa + X (fina masc / 阳性词尾)
        probes.push(format!("\u{1830}\u{1821}{ch}")); // se + X (fina fem / 阴性词尾)
    }

    // 2. FVS-free natural contextual words (diphthongs, velar clusters, all consonant MVS chachlag
    //    contexts, and NNBSP case suffixes) placed BEFORE FVS probes so natural contextual shapes
    //    are recorded ahead of explicit FVS overrides.
    // 2. 无 FVS 的自然上下文探针词（双元音、舌根音连写、全辅音 MVS 隔音尾、全套 U+202F 格后缀），
    //    置于 FVS 探针之前，使跨字体兼容的自然上下文探针优先于显式 FVS 探针。
    for cp in 0x1828u32..=0x1842u32 {
        if let Some(ch) = char::from_u32(cp) {
            probes.push(format!("\u{1820}{ch}\u{180E}\u{1820}")); // a + C + MVS + a (e.g. aq-a)
            probes.push(format!("\u{1821}{ch}\u{180E}\u{1821}")); // e + C + MVS + e (e.g. ek-e)
            probes.push(format!("\u{1830}\u{1820}{ch}\u{180E}\u{1820}")); // sa + C + MVS + a
            probes.push(format!("\u{1830}\u{1821}{ch}\u{180E}\u{1821}")); // se + C + MVS + e
        }
    }

    probes.extend(
        [
            "\u{1820}\u{1822}",                                 // ai (diphthong / 双元音)
            "\u{1821}\u{1822}",                                 // ei
            "\u{1823}\u{1822}",                                 // oi
            "\u{1824}\u{1822}",                                 // ui
            "\u{1825}\u{1822}",                                 // oei
            "\u{1826}\u{1822}",                                 // uei
            "\u{1830}\u{1820}\u{1822}\u{1828}",                 // sain (medi double-shin I / 词中双齿 I)
            "\u{1832}\u{1821}\u{1822}\u{1828}",                 // tein (medi fem double-shin I / 阴性词中双齿 I)
            "\u{1830}\u{1820}\u{1822}",                         // sai (fina double-shin I / 词尾双元音)
            "\u{1832}\u{1821}\u{1822}",                         // tei
            "\u{1829}",                                         // ng isol
            "\u{1829}\u{1820}",                                 // nga
            "\u{1820}\u{1829}\u{182D}\u{180E}\u{1820}",         // angg-a
            "\u{1821}\u{1829}\u{182D}\u{180E}\u{1821}",         // engg-e
            "\u{182E}\u{1823}\u{1829}\u{182D}\u{1823}\u{182F}", // monggol
            "\u{1830}\u{1822}\u{1828}\u{180E}\u{1820}",         // sin-a (N.fina.mvs + mvs + Aa.isol)
            "\u{182A}\u{1820}\u{1822}\u{1828}\u{180E}\u{1820}", // bain-a
            "\u{182E}\u{1823}\u{1829}\u{182D}\u{1823}\u{182F}\u{202F}\u{1822}", // monggol-i
            "\u{182E}\u{1823}\u{1829}\u{182D}\u{1823}\u{182F}\u{202F}\u{1822}\u{180B}", // monggol-i(FVS1)
            "\u{182E}\u{1823}\u{1829}\u{182D}\u{1823}\u{182F}\u{202F}\u{1824}", // monggol-u
            "\u{182E}\u{1823}\u{1829}\u{182D}\u{1823}\u{182F}\u{202F}\u{1824}\u{1828}", // monggol-un
            "\u{182D}\u{1821}\u{1837}\u{202F}\u{1826}\u{1828}", // ger-uen
            "\u{182E}\u{1823}\u{1829}\u{182D}\u{1823}\u{182F}\u{202F}\u{1822}\u{1828}", // monggol-in
            "\u{1820}\u{182C}\u{1820}\u{202F}\u{1836}\u{1822}\u{1828}", // aqa-yin
            "\u{1821}\u{182C}\u{1821}\u{202F}\u{1836}\u{1822}\u{1828}", // eke-yin
            "\u{1824}\u{182F}\u{1824}\u{1830}\u{202F}\u{1832}\u{1824}", // ulus-tu
            "\u{182D}\u{1821}\u{1837}\u{202F}\u{1832}\u{1826}", // ger-tue
            "\u{182E}\u{1823}\u{1829}\u{182D}\u{1823}\u{182F}\u{202F}\u{1833}\u{1824}", // monggol-du
            "\u{182D}\u{1821}\u{1837}\u{202F}\u{1833}\u{1826}", // ger-due
            "\u{182D}\u{1820}\u{1836}\u{1820}\u{1837}\u{202F}\u{1820}\u{1834}\u{1820}", // gayar-aca
            "\u{182D}\u{1821}\u{1837}\u{202F}\u{1821}\u{1834}\u{1821}", // ger-ece
            "\u{182D}\u{1820}\u{1837}\u{202F}\u{182A}\u{1820}\u{1837}", // gar-bar
            "\u{182D}\u{1821}\u{1837}\u{202F}\u{182A}\u{1821}\u{1837}", // ger-ber
            "\u{182D}\u{1820}\u{1837}\u{202F}\u{1822}\u{1836}\u{1820}\u{1837}", // gar-iyar
            "\u{182D}\u{1821}\u{1837}\u{202F}\u{1822}\u{1836}\u{1821}\u{1837}", // ger-iyer
            "\u{182D}\u{1820}\u{1837}\u{202F}\u{182F}\u{1824}\u{182D}\u{180E}\u{1820}", // gar-lug-a
            "\u{182D}\u{1821}\u{1837}\u{202F}\u{182F}\u{1826}\u{182D}\u{180E}\u{1821}", // ger-lueg-e
            "\u{1830}\u{180A}\u{1820}",                         // nirugu
            "\u{182A}\u{200C}\u{1820}",                         // zwnj
        ]
        .into_iter()
        .map(str::to_owned),
    );

    // 3. Bowed consonants (`B`, `P`, `H`, `G`, `F`, `K2`, `K`, plus FVS variants) + vowels & `M`/`L` (`BM`, `BL`, `BOI`).
    // 3. 弯头辅音（B/P/H/G/F/K2/K 及其 FVS 变体）与元音及 M/L/NG/LH 的合字探针。
    let bowed = [
        "\u{182A}",
        "\u{182B}",
        "\u{182C}",
        "\u{182C}\u{180B}",
        "\u{182C}\u{180C}",
        "\u{182D}",
        "\u{182D}\u{180B}",
        "\u{182D}\u{180C}",
        "\u{1839}",
        "\u{183A}",
        "\u{183B}",
    ];
    let followers = [
        "\u{1820}",
        "\u{1821}",
        "\u{1822}",
        "\u{1823}",
        "\u{1824}",
        "\u{1825}",
        "\u{1825}\u{180B}",
        "\u{1826}",
        "\u{1826}\u{180B}",
        "\u{1826}\u{1822}",
        "\u{1827}",
        "\u{182E}",
        "\u{182F}",
        "\u{1829}",
        "\u{1840}",
    ];
    for b in bowed {
        for v in followers {
            probes.push(format!("{b}{v}")); // isol / 孤立
            probes.push(format!("{b}{v}\u{182F}")); // init / 词首
            probes.push(format!("\u{1830}{b}{v}\u{182F}")); // medi / 词中
            probes.push(format!("\u{1830}\u{1820}{b}{v}")); // fina / 词尾
        }
    }

    // 4. FVS1..FVS4 variants in isolated, initial, medial, and final positions.
    // 4. FVS1..FVS4 自由变体选择符在孤立、词首、词中、词尾位置的显式探针。
    for &ch in &hudum_letters {
        for &fvs in &['\u{180B}', '\u{180C}', '\u{180D}', '\u{180F}'] {
            probes.push(format!("{ch}{fvs}"));
            probes.push(format!("{ch}{fvs}\u{1820}\u{182F}"));
            probes.push(format!("{ch}{fvs}\u{1821}\u{182F}"));
            probes.push(format!("\u{1830}\u{1820}{ch}{fvs}\u{1820}\u{182F}"));
            probes.push(format!("\u{1830}\u{1821}{ch}{fvs}\u{1821}\u{182F}"));
            probes.push(format!("\u{1830}\u{1820}{ch}{fvs}"));
            probes.push(format!("\u{1830}\u{1821}{ch}{fvs}"));
        }
    }

    probes
}

// ============================================================================
// Local Offline Interactive HTML/SVG Calibration Page Generator
// 本地离线交互式 HTML/SVG 字形轮廓标定工作台生成器（绝不提交至公开仓库）
// ============================================================================

struct FlipYSvgPen {
    path: String,
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

impl FlipYSvgPen {
    fn new() -> Self {
        Self {
            path: String::new(),
            min_x: f32::INFINITY,
            min_y: f32::INFINITY,
            max_x: f32::NEG_INFINITY,
            max_y: f32::NEG_INFINITY,
        }
    }

    fn record(&mut self, sx: f32, sy: f32) {
        self.min_x = self.min_x.min(sx);
        self.min_y = self.min_y.min(sy);
        self.max_x = self.max_x.max(sx);
        self.max_y = self.max_y.max(sy);
    }
}

impl OutlinePen for FlipYSvgPen {
    fn move_to(&mut self, x: f32, y: f32) {
        let (sx, sy) = (x, -y);
        self.record(sx, sy);
        let _ = write!(self.path, "M{sx:.1} {sy:.1}");
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (sx, sy) = (x, -y);
        self.record(sx, sy);
        let _ = write!(self.path, "L{sx:.1} {sy:.1}");
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let (scx, scy, sx, sy) = (cx0, -cy0, x, -y);
        self.record(scx, scy);
        self.record(sx, sy);
        let _ = write!(self.path, "Q{scx:.1} {scy:.1} {sx:.1} {sy:.1}");
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let (scx0, scy0, scx1, scy1, sx, sy) = (cx0, -cy0, cx1, -cy1, x, -y);
        self.record(scx0, scy0);
        self.record(scx1, scy1);
        self.record(sx, sy);
        let _ = write!(
            self.path,
            "C{scx0:.1} {scy0:.1} {scx1:.1} {scy1:.1} {sx:.1} {sy:.1}"
        );
    }

    fn close(&mut self) {
        self.path.push('Z');
    }
}

fn extract_glyph_svg(font_ref: &FontRef<'_>, upem: u16, gid: GlyphId) -> Option<(String, [f32; 4])> {
    let mut pen = FlipYSvgPen::new();
    if let Ok(tables) = OutlineTables::new(font_ref) {
        let mut outline = Outline::<Unscaled>::new();
        if let Ok(loaded) = outline.load(&tables, gid) {
            let _ = loaded.to_path(PathContourStart::ScanBackward, &mut pen);
        }
    } else if let Some(cff_data) = font_ref
        .data_for_tag(Tag::new(b"CFF2"))
        .or_else(|| font_ref.data_for_tag(Tag::new(b"CFF ")))
    {
        if let Ok(cff) = CffFontRef::new(cff_data.as_ref(), 0, Some(i32::from(upem))) {
            let sub_idx = cff.subfont_index(gid).unwrap_or(0);
            if let Ok(subfont) = cff.subfont(sub_idx, &[]) {
                let _ = cff.draw(&subfont, gid, &[], None, &mut pen);
            }
        }
    }

    if pen.path.is_empty() || !pen.min_x.is_finite() {
        return None;
    }
    let pad = f32::from(upem) * 0.08;
    let w = (pen.max_x - pen.min_x).max(1.0) + pad * 2.0;
    let h = (pen.max_y - pen.min_y).max(1.0) + pad * 2.0;
    Some((pen.path, [pen.min_x - pad, pen.min_y - pad, w, h]))
}

/// Render a local offline interactive HTML/SVG calibration page for human verification and editing
/// of a [`FontProfile`].
///
/// **Copyright Notice**: Because this HTML embeds vector glyph previews extracted from `font_bytes`,
/// save it only to a gitignored local scratch path (such as `var/calibration/`). Never commit it!
///
/// 生成本地离线交互式 HTML/SVG 字形标定网页，供人工直观核对竖排字形轮廓并一键导出修改后的 JSON。
/// **版权提示**：该网页包含从本地字体提取的矢量预览路径，请仅保存在被 `.gitignore` 忽略的本地目录（如 `var/calibration/`），切勿提交至公开仓库！
pub fn render_calibration_html(
    font_bytes: &[u8],
    face_index: u32,
    profile: &FontProfile,
) -> Result<String, Error> {
    let font = Font::new(font_bytes.to_vec(), face_index).ok_or_else(|| {
        Error::InvalidFont("failed to parse font for HTML calibration / 无法解析字体以生成标定页".to_owned())
    })?;
    let font_ref = FontRef::from_index(font_bytes, face_index)
        .map_err(|e| Error::InvalidFont(format!("FontRef error: {e}")))?;

    let font_name = extract_font_identity_name(&font);
    let upem = font.units_per_em();

    let mut name_to_gid: HashMap<String, u16> = HashMap::new();
    for (gid, gname) in font.glyph_names() {
        if !gname.is_synthesized() {
            if let Ok(id16) = u16::try_from(gid.to_u32()) {
                name_to_gid.entry(gname.as_str().to_owned()).or_insert(id16);
            }
        }
    }
    let charmap = font.charmap();
    let language = Language::new(&profile.language);
    let shaper_font = ShaperFont::new(&font);

    #[derive(Serialize)]
    struct CardPreview {
        rule_index: usize,
        gid: Option<u16>,
        svg_path: String,
        bbox: [f32; 4],
    }

    let mut previews = Vec::with_capacity(profile.rules.len());
    for (r_idx, rule) in profile.rules.iter().enumerate() {
        let mut rep_gid: Option<u16> = rule
            .glyph_names
            .iter()
            .find_map(|n| name_to_gid.get(n).copied());
        if rep_gid.is_none() {
            rep_gid = rule.codepoints.iter().find_map(|cp_s| {
                crate::profile::parse_codepoint_str(cp_s)
                    .ok()
                    .and_then(|cp| charmap.map_unicode(cp))
                    .and_then(|g| u16::try_from(g.to_u32()).ok())
            });
        }
        if rep_gid.is_none() {
            rep_gid = rule.probes.iter().find_map(|p| {
                shape_probe_raw(&shaper_font, language.as_ref(), &p.text)
                    .and_then(|gs| gs.get(p.glyph_index).map(|&(gid, _)| gid))
            });
        }
        if rep_gid.is_none() {
            rep_gid = rule.gid;
        }

        let (svg_path, bbox) = rep_gid
            .and_then(|gid| extract_glyph_svg(&font_ref, upem, GlyphId::new(u32::from(gid))))
            .unwrap_or_else(|| (String::new(), [0.0, 0.0, 100.0, 100.0]));

        previews.push(CardPreview {
            rule_index: r_idx,
            gid: rep_gid,
            svg_path,
            bbox,
        });
    }

    let profile_json = serde_json::to_string(profile)?;
    let previews_json = serde_json::to_string(&previews)?;
    let escaped_font_name = html_escape(&font_name);
    let escaped_profile_id = html_escape(&profile.id);

    Ok(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Mongol Font Profile Calibration / 蒙古文字体映射标定台 — {escaped_profile_id}</title>
<style>
  :root {{ --bg:#0f172a; --card:#1e293b; --border:#334155; --text:#f8fafc; --muted:#94a3b8; --accent:#38bdf8; --warn:#f59e0b; }}
  * {{ box-sizing: border-box; }}
  body {{ margin:0; font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background:var(--bg); color:var(--text); padding:20px; }}
  header {{ background:var(--card); border:1px solid var(--border); border-radius:10px; padding:16px 20px; margin-bottom:16px; }}
  h1 {{ margin:0 0 6px 0; font-size:18px; }}
  .warn-banner {{ color:var(--warn); font-size:13px; margin-top:6px; }}
  .toolbar {{ display:flex; flex-wrap:wrap; gap:10px; align-items:center; margin-top:12px; }}
  button, select, input[type="search"] {{ background:#0f172a; color:var(--text); border:1px solid var(--border); border-radius:6px; padding:7px 12px; font-size:13px; cursor:pointer; }}
  button.primary {{ background:var(--accent); color:#0f172a; font-weight:600; border:none; }}
  .grid {{ display:grid; grid-template-columns:repeat(auto-fill, minmax(270px, 1fr)); gap:12px; }}
  .card {{ background:var(--card); border:1px solid var(--border); border-radius:8px; padding:12px; display:flex; flex-direction:column; gap:8px; }}
  .card.review {{ border-color:var(--warn); }}
  .svg-box {{ height:120px; background:#020617; border-radius:6px; display:flex; align-items:center; justify-content:center; overflow:hidden; }}
  svg {{ width:100px; height:100px; fill:#e2e8f0; transition:transform 0.15s ease; }}
  body.vertical svg {{ transform: rotate(90deg); }}
  .meta {{ font-size:11px; color:var(--muted); word-break:break-all; }}
  .field {{ display:flex; gap:6px; align-items:center; font-size:12px; }}
  .field input[type="text"], .field select {{ flex:1; padding:4px 6px; font-size:12px; background:#0f172a; color:var(--text); border:1px solid var(--border); border-radius:4px; }}
</style>
</head>
<body class="vertical">
<header>
  <h1>Mongol Font Shaper Calibration Studio / 蒙古文字体字形映射人工标定台 — <code>{escaped_profile_id}</code> ({escaped_font_name})</h1>
  <div class="warn-banner">⚠️ Local Calibration Only (Copyright Notice): This HTML embeds local font outline previews — do NOT commit to public Git (export pure JSON profile instead). / 仅供本地标定核对（版权提示）：此 HTML 含有本地字体轮廓预览，请勿提交到公开 Git 仓库（导出纯 JSON 配置文件即可安全提交）。</div>
  <div class="toolbar">
    <button id="toggleOrient">Orientation: Vertical (90° CW) / 方向：竖排蒙文</button>
    <select id="filterMode">
      <option value="all">Show All Rules / 显示全部规则</option>
      <option value="review">Needs Review Only / 仅看需复核项 (review=true)</option>
      <option value="units">Units Only / 仅看字形单元</option>
    </select>
    <input type="search" id="searchBox" placeholder="Search unit, glyph name, U+XXXX / 搜索字形名或单元..." />
    <span id="statsLabel" style="font-size:12px;color:var(--muted)"></span>
    <button class="primary" id="exportBtn">Export Profile JSON / 导出修改后的 JSON 配置</button>
  </div>
</header>
<div class="grid" id="cardGrid"></div>
<script>
const PROFILE = {profile_json};
const PREVIEWS = {previews_json};

const grid = document.getElementById('cardGrid');
const filterMode = document.getElementById('filterMode');
const searchBox = document.getElementById('searchBox');
const statsLabel = document.getElementById('statsLabel');

function renderCards() {{
  const mode = filterMode.value;
  const q = searchBox.value.trim().toLowerCase();
  grid.innerHTML = '';
  let shown = 0, reviewCount = 0;
  PROFILE.rules.forEach((r, idx) => {{
    if (r.review) reviewCount++;
    if (mode === 'review' && !r.review) return;
    if (mode === 'units' && r.kind !== 'units') return;
    const hay = [r.units || '', ...(r.glyph_names || []), ...(r.codepoints || []), r.note || ''].join(' ').toLowerCase();
    if (q && !hay.includes(q)) return;
    shown++;
    const pv = PREVIEWS[idx] || {{ bbox:[0,0,100,100], svg_path:'' }};
    const [bx, by, bw, bh] = pv.bbox;
    const card = document.createElement('div');
    card.className = 'card' + (r.review ? ' review' : '');
    const anchors = [
      pv.gid != null ? ('GID #' + pv.gid) : '',
      (r.glyph_names || []).slice(0, 2).join(', '),
      (r.codepoints || []).join(', ')
    ].filter(Boolean).join(' · ');
    card.innerHTML = `
      <div class="svg-box">
        ${{pv.svg_path ? `<svg viewBox="${{bx}} ${{by}} ${{bw}} ${{bh}}"><path d="${{pv.svg_path}}"/></svg>` : `<span style="color:#64748b;font-size:12px">(Zero-ink / 无轮廓)</span>`}}
      </div>
      <div class="meta">${{anchors}}</div>
      <div class="field"><label>Kind/类别</label>
        <select data-idx="${{idx}}" data-key="kind">
          ${{['units','silent','passthrough','visible_fvs','not_def'].map(k => `<option value="${{k}}" ${{r.kind===k?'selected':''}}>${{k}}</option>`).join('')}}
        </select>
      </div>
      <div class="field"><label>Units/单元</label>
        <input type="text" data-idx="${{idx}}" data-key="units" value="${{(r.units || '').replace(/"/g, '&quot;')}}" placeholder="e.g. B:init A:medi" />
      </div>
      <div class="field"><label>Join/连写</label>
        <input type="text" data-idx="${{idx}}" data-key="joining" value="${{(r.joining || '').replace(/"/g, '&quot;')}}" placeholder="auto (or isol|init|medi|fina)" />
      </div>
      <label style="font-size:12px;display:flex;align-items:center;gap:6px;color:#cbd5e1">
        <input type="checkbox" data-idx="${{idx}}" data-key="review" ${{r.review ? 'checked' : ''}} />
        Needs Review / 待复核
      </label>
    `;
    grid.appendChild(card);
  }});
  statsLabel.textContent = `Showing ${{shown}} / ${{PROFILE.rules.length}} rules (Pending review / 待复核: ${{reviewCount}})`;
}}

grid.addEventListener('change', (e) => {{
  const t = e.target;
  const idx = Number(t.dataset.idx);
  const key = t.dataset.key;
  if (Number.isNaN(idx) || !key) return;
  const rule = PROFILE.rules[idx];
  if (key === 'review') {{
    rule.review = t.checked;
  }} else if (key === 'joining') {{
    const v = t.value.trim();
    if (v) rule.joining = v; else delete rule.joining;
  }} else {{
    rule[key] = t.value.trim();
  }}
  renderCards();
}});

document.getElementById('toggleOrient').addEventListener('click', (e) => {{
  document.body.classList.toggle('vertical');
  const isV = document.body.classList.contains('vertical');
  e.target.textContent = isV ? 'Orientation: Vertical (90° CW) / 方向：竖排蒙文' : 'Orientation: Horizontal / 方向：横排原始坐标';
}});

filterMode.addEventListener('change', renderCards);
searchBox.addEventListener('input', renderCards);

document.getElementById('exportBtn').addEventListener('click', () => {{
  const blob = new Blob([JSON.stringify(PROFILE, null, 2) + '\n'], {{ type: 'application/json' }});
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = (PROFILE.id || 'draft') + '.profile.json';
  a.click();
}});

renderCards();
</script>
</body>
</html>
"#
    ))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
