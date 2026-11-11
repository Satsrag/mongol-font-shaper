//! Font-driven shaping engine (`FontShaper`) producing `PositionedWrittenUnit`, `WrittenUnit`,
//! and canonical UTN #57 Unicode text.
//!
//! 通用字体塑形引擎（`FontShaper`），统一支持产出 `PositionedWrittenUnit`、`WrittenUnit`
//! 以及标准 UTN #57 Unicode 文本，并内建错码与位置连写自检。

use std::path::Path;
use std::sync::OnceLock;

use harfrust::{
    shape, Buffer, BufferFlags, ClusterLevel, Direction, Font, Language, Script, ShapeOptions,
    ShapePlan, ShaperFont,
};
use mongol_norm::{Locale, PositionedWrittenUnit, Shaper, UnitPosition, WrittenUnit};
use serde::{Deserialize, Serialize};

use crate::error::{DiagnosticIssue, Diagnostics, Error};
use crate::profile::{
    compile_profile_for_font, extract_font_identity_name, CompiledGlyph, EncodingClass,
    FontProfile, JoiningSpan,
};

/// Shared zero-dependency `mongol-norm` rule shaper/normalizer instance (`font` feature disabled).
/// 进程级共享的 `mongol-norm` 纯规则规范化器实例（不启用其内置 `font` 特性）。
pub fn utn57_shaper() -> &'static Shaper {
    static SHAPER: OnceLock<Shaper> = OnceLock::new();
    SHAPER.get_or_init(|| Shaper::new(Locale::Mng))
}

/// Container pairing a shaped output value with its shaping [`Diagnostics`].
/// 塑形输出结果及其伴随的 [`Diagnostics`] 诊断报告。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShapedOutput<T> {
    /// Primary output payload / 塑形主输出结果。
    pub value: T,
    /// Diagnostics collected during shaping / 塑形过程中收集的错码与连写诊断报告。
    pub diagnostics: Diagnostics,
}

/// Glyph-level debug record emitted by [`FontShaper::shape_glyphs`].
/// [`FontShaper::shape_glyphs`] 返回的底层单字形调试记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShapedGlyphRecord {
    /// Glyph ID in the font / 字体中的 Glyph ID。
    pub glyph_id: u16,
    /// Optional `post` / `CFF` glyph name / 可选的字形名称。
    pub glyph_name: Option<String>,
    /// Character cluster index in the input / 对应的输入字符索引。
    pub cluster: usize,
    /// Original character at `cluster` / 该位置的原始字符。
    pub ch: char,
    /// Compiled semantic entry / 该字形编译后的语义动作。
    pub entry: CompiledGlyph,
}

/// Universal font-backed Mongolian shaper & UTN #57 normalizer.
///
/// Safe to share across threads (`Send + Sync`); cloning is cheap (`Arc`-backed `Font`).
///
/// 通用外部字体驱动蒙古文塑形与 UTN #57 规范化器。
/// 线程安全（`Send + Sync`），底层共享字体表缓存。
pub struct FontShaper {
    font: Font,
    font_name: String,
    profile_id: String,
    encoding_class: EncodingClass,
    language: Option<Language>,
    plan: ShapePlan,
    table: Vec<CompiledGlyph>,
    bound_rule_count: usize,
}

impl FontShaper {
    /// Build a [`FontShaper`] from in-memory `.ttf` / `.otf` / `.ttc` bytes and a [`FontProfile`].
    /// 从内存中的 `.ttf` / `.otf` / `.ttc` 字节切片与 [`FontProfile`] 构建 [`FontShaper`]。
    pub fn from_bytes(
        font_bytes: impl Into<Vec<u8>>,
        face_index: u32,
        profile: &FontProfile,
    ) -> Result<Self, Error> {
        let bytes: Vec<u8> = font_bytes.into();
        let font = Font::new(bytes, face_index).ok_or_else(|| {
            Error::InvalidFont(format!(
                "unsupported or corrupted SFNT/TTF/OTF font at face index {face_index}"
            ))
        })?;
        Self::from_font(font, profile)
    }

    /// Build a [`FontShaper`] from font file path and [`FontProfile`].
    /// 从磁盘字体文件路径与 [`FontProfile`] 构建 [`FontShaper`]。
    pub fn from_path(
        font_path: impl AsRef<Path>,
        face_index: u32,
        profile: &FontProfile,
    ) -> Result<Self, Error> {
        let bytes = std::fs::read(font_path)?;
        Self::from_bytes(bytes, face_index, profile)
    }

    /// Build a [`FontShaper`] from an already-parsed `harfrust::Font`.
    /// 从已解析的 `harfrust::Font` 实例构建 [`FontShaper`]。
    pub fn from_font(font: Font, profile: &FontProfile) -> Result<Self, Error> {
        let font_name = extract_font_identity_name(&font);
        let (table, bound_rule_count) = compile_profile_for_font(&font, profile)?;
        let language = Language::new(&profile.language);
        let plan = ShapePlan::new(
            &font,
            Direction::LeftToRight,
            Some(Script::MONGOLIAN),
            language.as_ref(),
            &[],
        );
        Ok(Self {
            font,
            font_name,
            profile_id: profile.id.clone(),
            encoding_class: profile.encoding_class,
            language,
            plan,
            table,
            bound_rule_count,
        })
    }

    /// Font internal PostScript/family name.
    /// 字体内部标识名称（PostScript 名或家族名）。
    pub fn font_name(&self) -> &str {
        &self.font_name
    }

    /// Profile identifier bound to this shaper.
    /// 当前绑定的家族 Profile ID。
    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    /// Encoding class of the bound profile.
    /// 当前绑定配置的编码类别。
    pub fn encoding_class(&self) -> EncodingClass {
        self.encoding_class
    }

    /// Number of glyph slots matched when compiling the profile against this font.
    /// 该字体成功命中绑定的字形槽位数量。
    pub fn bound_rule_count(&self) -> usize {
        self.bound_rule_count
    }

    /// Underlying `harfrust::Font` reference.
    /// 底层 `harfrust::Font` 引用。
    pub fn font(&self) -> &Font {
        &self.font
    }

    /// Low-level glyph view: runs `harfrust` on `text` and attaches compiled rules to each output glyph.
    /// 底层字形视图：使用 `harfrust` 对 `text` 塑形，并返回每个输出字形及其编译规则。
    pub fn shape_glyphs(&self, text: &str) -> Result<Vec<ShapedGlyphRecord>, Error> {
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            return Ok(Vec::new());
        }
        let raw_infos = self.run_harfrust(&chars)?;
        let mut out = Vec::with_capacity(raw_infos.len());
        for (gid, cluster) in raw_infos {
            let idx = (cluster as usize).min(chars.len() - 1);
            let ch = chars[idx];
            let entry = self
                .table
                .get(gid as usize)
                .cloned()
                .unwrap_or(CompiledGlyph::NotDef);
            let glyph_name = self
                .font
                .glyph_name(read_fonts::types::GlyphId::new(u32::from(gid)))
                .filter(|n| !n.is_synthesized())
                .map(|n| n.as_str().to_owned());
            out.push(ShapedGlyphRecord {
                glyph_id: gid,
                glyph_name,
                cluster: idx,
                ch,
                entry,
            });
        }
        Ok(out)
    }

    /// Output 1: Shape `text` into [`PositionedWrittenUnit`] records (per Mongolian word/run, flattened).
    /// Non-Mongolian separators between words split runs; within each word, ligatures expand to their
    /// constituent positioned units and `Mvs` / `Nirugu` controls are deduplicated cleanly.
    ///
    /// 输出格式 1：将输入文本塑形为 [`PositionedWrittenUnit`] 序列（自动拆解合字、校验词串连写位置并合并冗余控制符）。
    pub fn shape_positioned(
        &self,
        text: &str,
    ) -> Result<ShapedOutput<Vec<PositionedWrittenUnit>>, Error> {
        let plan = self.analyze_text(text, true)?;
        let mut all_units = Vec::new();
        for seg in plan.segments {
            if let Segment::MongolianRun { units, .. } = seg {
                all_units.extend(units);
            }
        }
        Ok(ShapedOutput {
            value: all_units,
            diagnostics: plan.diagnostics,
        })
    }

    /// Output 2: Shape `text` into canonical [`WrittenUnit`] sequences (matching `mongol-norm`'s
    /// [`Shaper::shape`] contract: duplicate encodings unified and redundant controls removed).
    ///
    /// 输出格式 2：将输入文本塑形为标准 [`WrittenUnit`] 序列（与 `mongol-norm` 的 [`Shaper::shape`]
    /// 输出契约完全对齐：统一同形异码并消除冗余控制符）。
    pub fn shape_units(&self, text: &str) -> Result<ShapedOutput<Vec<WrittenUnit>>, Error> {
        let mut plan = self.analyze_text(text, true)?;
        let shaper = utn57_shaper();
        let mut out_units = Vec::new();
        for seg in plan.segments {
            if let Segment::MongolianRun {
                start_cluster,
                units,
                ..
            } = seg
            {
                match normalize_positioned_run(shaper, &units) {
                    Ok(canonical_str) => {
                        let canonical = shaper.shape(&canonical_str)?;
                        out_units.extend(canonical);
                    }
                    Err(err) => {
                        plan.diagnostics.push(
                            DiagnosticIssue::NormalizationFallback {
                                cluster: start_cluster,
                                detail: err.to_string(),
                            },
                            true,
                        );
                        out_units.extend(units.iter().map(|u| u.unit));
                    }
                }
            }
        }
        Ok(ShapedOutput {
            value: out_units,
            diagnostics: plan.diagnostics,
        })
    }

    /// Convenience helper formatting [`FontShaper::shape_units`] with `+` (`"S+I+N+Mvs+Aa"`),
    /// separating multiple words in a sentence with spaces.
    ///
    /// 便捷输出函数：将每个词段的 `WrittenUnit` 用 `+` 连接（如 `"S+I+N+Mvs+Aa"`），多词之间保留空格。
    pub fn shape_units_str(&self, text: &str) -> Result<ShapedOutput<String>, Error> {
        let mut plan = self.analyze_text(text, true)?;
        let shaper = utn57_shaper();
        let mut out = String::with_capacity(text.len() * 2);
        for seg in plan.segments {
            match seg {
                Segment::Passthrough(s) => out.push_str(&s),
                Segment::MongolianRun {
                    start_cluster,
                    units,
                    ..
                } => {
                    let word_units: Vec<WrittenUnit> = match normalize_positioned_run(shaper, &units)
                    {
                        Ok(canonical_str) => shaper.shape(&canonical_str)?,
                        Err(err) => {
                            plan.diagnostics.push(
                                DiagnosticIssue::NormalizationFallback {
                                    cluster: start_cluster,
                                    detail: err.to_string(),
                                },
                                true,
                            );
                            units.iter().map(|u| u.unit).collect()
                        }
                    };
                    for (i, u) in word_units.iter().enumerate() {
                        if i > 0 {
                            out.push('+');
                        }
                        out.push_str(u.as_str());
                    }
                }
            }
        }
        Ok(ShapedOutput {
            value: out,
            diagnostics: plan.diagnostics,
        })
    }

    /// Output 3: Convert complete text (words, phrases, dictionary entries, mixed sentences) into
    /// canonical UTN #57 Mongolian Unicode text via `mongol-norm`, preserving passthrough punctuation,
    /// whitespace, digits, CJK, Cyrillic, and Latin spans verbatim.
    ///
    /// 输出格式 3：将完整文本（单词、词典词头、双语例句）转换为标准 UTN #57 蒙古文 Unicode 文本，
    /// 自动原样保留空格、标点、数字以及汉/西里尔/拉丁等非蒙文片段。
    pub fn normalize_text(&self, text: &str) -> Result<ShapedOutput<String>, Error> {
        self.normalize_text_internal(text, true)
    }

    /// Fast defect-only evaluation used during corpus-wide batch font detection (`keep_detail = false`).
    /// 语料级批量字体探测专用的快速评估接口（不分配详细异常字符串，仅统计各项错码计数与归一化文本）。
    pub(crate) fn evaluate_fast(&self, text: &str) -> Result<ShapedOutput<String>, Error> {
        self.normalize_text_internal(text, false)
    }

    fn normalize_text_internal(
        &self,
        text: &str,
        keep_detail: bool,
    ) -> Result<ShapedOutput<String>, Error> {
        let mut plan = self.analyze_text(text, keep_detail)?;
        let shaper = utn57_shaper();
        let mut out = String::with_capacity(text.len());
        for seg in plan.segments {
            match seg {
                Segment::Passthrough(s) => out.push_str(&s),
                Segment::MongolianRun {
                    start_cluster,
                    raw_slice,
                    units,
                } => match normalize_positioned_run(shaper, &units) {
                    Ok(canonical) => out.push_str(&canonical),
                    Err(err) => {
                        plan.diagnostics.push(
                            DiagnosticIssue::NormalizationFallback {
                                cluster: start_cluster,
                                detail: if keep_detail {
                                    err.to_string()
                                } else {
                                    String::new()
                                },
                            },
                            keep_detail,
                        );
                        out.push_str(&raw_slice);
                    }
                },
            }
        }
        Ok(ShapedOutput {
            value: out,
            diagnostics: plan.diagnostics,
        })
    }

    fn run_harfrust(&self, chars: &[char]) -> Result<Vec<(u16, u32)>, Error> {
        let mut buffer = Buffer::new();
        buffer.set_cluster_level(ClusterLevel::MonotoneCharacters);
        buffer.set_flags(
            BufferFlags::BEGINNING_OF_TEXT
                | BufferFlags::END_OF_TEXT
                | BufferFlags::PRESERVE_DEFAULT_IGNORABLES,
        );
        for (idx, &ch) in chars.iter().enumerate() {
            buffer.push(ch as u32, idx as u32);
        }
        buffer.set_direction(Direction::LeftToRight);
        buffer.set_script(Some(Script::MONGOLIAN));
        buffer.set_language(self.language.clone());

        let shaper_font = ShaperFont::new(&self.font);
        shape(
            &shaper_font,
            &mut buffer,
            ShapeOptions::new().plan(Some(&self.plan)),
        )
        .map_err(|e| Error::ShapeFailure(format!("{e:?}")))?;

        Ok(buffer
            .glyph_infos()
            .iter()
            .map(|info| (info.glyph_id as u16, info.cluster))
            .collect())
    }

    fn analyze_text(&self, text: &str, keep_detail: bool) -> Result<TextAnalysis, Error> {
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            return Ok(TextAnalysis::default());
        }

        let raw_infos = self.run_harfrust(&chars)?;
        let mut diagnostics = Diagnostics::default();

        // Group consecutive glyphs by monotone character cluster ranges `[c_start, c_end)`.
        // 按单调递增的 `cluster` 将输出字形分组到对应的输入字符半开区间 `[c_start, c_end)`。
        let mut groups: Vec<ClusterGroup> = Vec::new();
        let mut i = 0;
        while i < raw_infos.len() {
            let c_start = (raw_infos[i].1 as usize).min(chars.len() - 1);
            let mut j = i + 1;
            while j < raw_infos.len() && (raw_infos[j].1 as usize) <= c_start {
                j += 1;
            }
            let c_end = if j < raw_infos.len() {
                (raw_infos[j].1 as usize).min(chars.len()).max(c_start + 1)
            } else {
                chars.len()
            };
            groups.push(ClusterGroup {
                c_start,
                c_end,
                glyphs: &raw_infos[i..j],
            });
            i = j;
        }

        // Handle any leading characters before the first cluster (defensive guard).
        let mut segments: Vec<Segment> = Vec::new();
        let mut current_run: Option<ActiveRun> = None;

        if let Some(first) = groups.first() {
            if first.c_start > 0 {
                let prefix: String = chars[..first.c_start].iter().collect();
                push_passthrough(&mut segments, &prefix);
            }
        }

        for group in &groups {
            let slice_chars = &chars[group.c_start..group.c_end];
            let first_ch = slice_chars[0];

            // Classify the glyphs produced by this cluster group.
            let mut group_units: Vec<(PositionedWrittenUnit, JoiningSpan)> = Vec::new();
            let mut has_unit_glyph = false;
            let mut is_pure_silent = true;

            for &(gid, _) in group.glyphs {
                let entry = self
                    .table
                    .get(gid as usize)
                    .unwrap_or(&CompiledGlyph::NotDef);

                match entry {
                    CompiledGlyph::Units { units, joining } => {
                        has_unit_glyph = true;
                        is_pure_silent = false;
                        let n = units.len();
                        for (u_idx, &u) in units.iter().enumerate() {
                            let span = if n == 1 {
                                *joining
                            } else if u_idx == 0 {
                                match *joining {
                                    JoiningSpan::Isol | JoiningSpan::Init => JoiningSpan::Init,
                                    _ => JoiningSpan::Medi,
                                }
                            } else if u_idx + 1 == n {
                                match *joining {
                                    JoiningSpan::Isol | JoiningSpan::Fina => JoiningSpan::Fina,
                                    JoiningSpan::Init => JoiningSpan::Init,
                                    other => other,
                                }
                            } else {
                                JoiningSpan::Medi
                            };
                            group_units.push((u, span));
                        }
                    }
                    CompiledGlyph::Silent => {}
                    CompiledGlyph::Passthrough => {
                        is_pure_silent = false;
                    }
                    CompiledGlyph::VisibleFvs => {
                        is_pure_silent = false;
                        diagnostics.push(
                            DiagnosticIssue::VisibleFvs {
                                cluster: group.c_start,
                                glyph_id: gid,
                            },
                            keep_detail,
                        );
                    }
                    CompiledGlyph::NotDef => {
                        is_pure_silent = false;
                        // Check if ALL characters in this cluster are non-Mongolian scripts/punctuation
                        // missing from a Mongolian-only font (like '!' in hinted NotoSansMongolian!).
                        if slice_chars
                            .iter()
                            .all(|&c| is_safe_external_passthrough(c, self.encoding_class))
                        {
                            // Do NOT penalize a Mongolian-only font for lacking Latin '!' or CJK '中':
                            // preserve the external character via passthrough!
                            // 关键实战保护：像 Noto hinted 子集或纯 Z52/ZvvnMod 字体本身不含英文 `!` 或汉字，
                            // 对明确的外部脚本/标点字符直接透传，不误扣 `.notdef` 错码分！
                        } else {
                            diagnostics.push(
                                DiagnosticIssue::NotDef {
                                    cluster: group.c_start,
                                    codepoint: first_ch as u32,
                                },
                                keep_detail,
                            );
                        }
                    }
                    CompiledGlyph::Unmapped { has_ink } => {
                        is_pure_silent = false;
                        if !*has_ink && is_ignorable_mongolian_control(first_ch) {
                            // Zero-ink unmapped variation selector / joiner -> treat as silent.
                        } else if slice_chars
                            .iter()
                            .all(|&c| is_safe_external_passthrough(c, self.encoding_class))
                        {
                            // Ordinary punctuation / whitespace / CJK / Latin / Cyrillic -> passthrough.
                        } else {
                            diagnostics.push(
                                DiagnosticIssue::UnmappedGlyph {
                                    cluster: group.c_start,
                                    glyph_id: gid,
                                    codepoint: first_ch as u32,
                                },
                                keep_detail,
                            );
                        }
                    }
                }
            }

            // Special rule for Legacy Menk2010 / Baiti where `U+180E (MVS)` before chachlag `-a/-e`
            // is mapped by GSUB to `space` (Passthrough) followed by `U1820_MVS` (`Aa:isol`):
            // If `first_ch` is `U+180E` or `U+202F` inside/adjacent to a Mongolian run, it represents
            // a structural `Mvs` separator rather than an ASCII `0x20` word break!
            //
            // 关键兼容规则：在 Menk GB2010 / Baiti / Oyun GB2010 中，`U+180E` 或 `U+202F` 常被字体
            // 渲染成普通 `space` 字形紧跟 `U1820_MVS`（隔音尾 `Aa:isol`）或格后缀。
            // 若原始输入码位本身就是 `U+180E` 或 `U+202F`，且处于蒙文词上下文中，应识别为 `Mvs` 控制单元！
            if matches!(first_ch, '\u{180E}' | '\u{202F}') && current_run.is_some() {
                if !has_unit_glyph {
                    has_unit_glyph = true;
                    group_units.push((
                        PositionedWrittenUnit::new(WrittenUnit::Mvs, UnitPosition::Control),
                        JoiningSpan::Control,
                    ));
                } else if group_units
                    .first()
                    .is_none_or(|(u, _)| u.unit != WrittenUnit::Mvs)
                {
                    group_units.insert(
                        0,
                        (
                            PositionedWrittenUnit::new(WrittenUnit::Mvs, UnitPosition::Control),
                            JoiningSpan::Control,
                        ),
                    );
                }
            }

            if has_unit_glyph {
                let run = current_run.get_or_insert_with(|| ActiveRun {
                    start_cluster: group.c_start,
                    end_cluster: group.c_end,
                    raw_slice: String::new(),
                    items: Vec::new(),
                });
                run.end_cluster = group.c_end;
                run.raw_slice.extend(slice_chars.iter());
                for (u, span) in group_units {
                    run.items.push((group.c_start, u, span));
                }
            } else if is_pure_silent && (current_run.is_some() || first_ch == '\u{200D}') {
                let run = current_run.get_or_insert_with(|| ActiveRun {
                    start_cluster: group.c_start,
                    end_cluster: group.c_end,
                    raw_slice: String::new(),
                    items: Vec::new(),
                });
                run.end_cluster = group.c_end;
                run.raw_slice.extend(slice_chars.iter());
            } else {
                if let Some(finished) = current_run.take() {
                    finalize_run(&chars, finished, &mut segments, &mut diagnostics, keep_detail);
                }
                let raw: String = slice_chars.iter().collect();
                push_passthrough(&mut segments, &raw);
            }
        }

        if let Some(finished) = current_run.take() {
            finalize_run(&chars, finished, &mut segments, &mut diagnostics, keep_detail);
        }

        Ok(TextAnalysis {
            segments,
            diagnostics,
        })
    }
}

struct ClusterGroup<'a> {
    c_start: usize,
    c_end: usize,
    glyphs: &'a [(u16, u32)],
}

struct ActiveRun {
    start_cluster: usize,
    end_cluster: usize,
    raw_slice: String,
    items: Vec<(usize, PositionedWrittenUnit, JoiningSpan)>,
}

enum Segment {
    Passthrough(String),
    MongolianRun {
        start_cluster: usize,
        raw_slice: String,
        units: Vec<PositionedWrittenUnit>,
    },
}

#[derive(Default)]
struct TextAnalysis {
    segments: Vec<Segment>,
    diagnostics: Diagnostics,
}

fn push_passthrough(segments: &mut Vec<Segment>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Segment::Passthrough(prev)) = segments.last_mut() {
        prev.push_str(text);
    } else {
        segments.push(Segment::Passthrough(text.to_owned()));
    }
}

/// Finalize one continuous Mongolian run:
/// 1. Merge consecutive `Mvs` controls (e.g. when `180E` emitted `mvs` and a chachlag glyph also carried `Mvs`);
/// 2. Check positional joining continuity across letter glyphs (catching e.g. `medi` at word end in Baiti `170` dict);
/// 3. Pass validated `PositionedWrittenUnit` list to `mongol-norm` normalization.
///
/// 收尾处理单个连续蒙古文词段：
/// 1. 合并相邻重复的 `Mvs` 控制单元；
/// 2. 执行词首/词中/词尾连写连续性检查（精准捕获如 `170` 词典在 Baiti 下词尾出现 `medi` 连写齿的错码，同时放行显式 `ZWJ` 词缀）；
/// 3. 整理 `PositionedWrittenUnit` 列表以喂给 `mongol-norm` 归一化。
fn finalize_run(
    chars: &[char],
    run: ActiveRun,
    segments: &mut Vec<Segment>,
    diagnostics: &mut Diagnostics,
    keep_detail: bool,
) {
    let mut filtered: Vec<(usize, PositionedWrittenUnit, JoiningSpan)> =
        Vec::with_capacity(run.items.len());
    for (cluster, unit, span) in run.items {
        if unit.unit == WrittenUnit::Mvs
            && filtered
                .last()
                .is_some_and(|(_, prev, _)| prev.unit == WrittenUnit::Mvs)
        {
            continue;
        }
        filtered.push((cluster, unit, span));
    }

    if !filtered.is_empty() {
        check_subchain_joining(
            chars,
            run.start_cluster,
            run.end_cluster,
            &filtered,
            diagnostics,
            keep_detail,
        );
    }

    let units: Vec<PositionedWrittenUnit> = filtered.into_iter().map(|(_, u, _)| u).collect();
    diagnostics.shaped_unit_count += units.len();

    if units.is_empty() {
        push_passthrough(segments, &run.raw_slice);
    } else {
        segments.push(Segment::MongolianRun {
            start_cluster: run.start_cluster,
            raw_slice: run.raw_slice,
            units,
        });
    }
}

fn check_subchain_joining(
    chars: &[char],
    run_start: usize,
    run_end: usize,
    items: &[(usize, PositionedWrittenUnit, JoiningSpan)],
    diagnostics: &mut Diagnostics,
    keep_detail: bool,
) {
    let mut idx = 0;
    while idx < items.len() {
        if items[idx].1.position == UnitPosition::Control {
            idx += 1;
            continue;
        }
        let sub_start = idx;
        while idx < items.len() && items[idx].1.position != UnitPosition::Control {
            idx += 1;
        }
        let sub = &items[sub_start..idx];
        let (first_cluster, first_unit, first_span) = sub[0];
        let (last_cluster, last_unit, last_span) = sub[sub.len() - 1];

        let left_bound = if sub_start > 0 {
            items[sub_start - 1].0
        } else {
            run_start
        };
        let right_bound = if idx < items.len() {
            items[idx].0
        } else {
            run_end.min(chars.len())
        };

        let explicit_left_zwj = chars
            .get(left_bound..=first_cluster.min(chars.len().saturating_sub(1)))
            .is_some_and(|s| s.contains(&'\u{200D}'));
        let explicit_right_zwj = chars
            .get(last_cluster..right_bound)
            .is_some_and(|s| s.contains(&'\u{200D}'));

        let joined_left = explicit_left_zwj
            || (sub_start > 0 && items[sub_start - 1].1.unit == WrittenUnit::Nirugu);
        let joined_right = explicit_right_zwj
            || (idx < items.len() && items[idx].1.unit == WrittenUnit::Nirugu);

        // Inspect the last unit's glyph joining span: if the chain ends at a word/MVS boundary
        // (neither before Nirugu nor before an explicit ZWJ), ending with `Medi` or multi-letter `Init` is a broken join!
        // 检查子词串末尾字形的连接属性：如果不接 Nirugu 也不带显式 ZWJ 却以 `Medi` 词中齿结尾（如 Baiti 的 `uni1826.medivar1`），判定为连写断裂！
        if !joined_right {
            let single_borrowed_init = sub.len() == 1
                && !joined_left
                && last_unit.position == UnitPosition::Init
                && matches!(last_span, JoiningSpan::Isol | JoiningSpan::Init);

            if last_span == JoiningSpan::Medi
                || (last_span == JoiningSpan::Init && !single_borrowed_init)
            {
                diagnostics.push(
                    DiagnosticIssue::ChainPositionBreak {
                        cluster: last_cluster,
                        detail: if keep_detail {
                            format!(
                                "chain ends with non-final form {:?} ({:?})",
                                last_unit.unit, last_span
                            )
                        } else {
                            String::new()
                        },
                    },
                    keep_detail,
                );
            }
        }

        // Inspect the first unit's glyph joining span: starting a chain (not after Nirugu/ZWJ)
        // with `Medi` or `Fina` is a broken join, EXCEPT immediately after `Mvs` (`U+180E`/`U+202F`)
        // where legacy GB2010 / Baiti fonts intentionally reuse crownless `Medi`/`Fina` vowel/glide
        // glyphs (`U1824_MEDI` for `-un`, `U1822_FINA` for `-i`, `Aa` for `-a/-e`).
        // 检查子词串开头字形的连接属性：非 Nirugu / 非显式 ZWJ 后若以 `Medi` 或 `Fina` 开头则判定为连写断裂；
        // 但紧接 `Mvs`（`U+180E` / `U+202F`）之后时，GB2010 与 Baiti 字体会合法复用无字冠的 `Medi`/`Fina`
        // 元音/半元音字形（如 `-un` 复用 `U1824_MEDI`、单字母 `-i` 复用 `I:Fina`、隔音尾 `-a/-e`），需予以放行。
        if !joined_left {
            let after_mvs = sub_start > 0 && items[sub_start - 1].1.unit == WrittenUnit::Mvs;
            let valid_post_mvs_stem = after_mvs
                && ((sub.len() == 1
                    && first_span == JoiningSpan::Fina
                    && matches!(
                        first_unit.unit,
                        WrittenUnit::Aa
                            | WrittenUnit::A
                            | WrittenUnit::I
                            | WrittenUnit::O
                            | WrittenUnit::U
                            | WrittenUnit::Ue
                    ))
                    || (sub.len() >= 2
                        && first_span == JoiningSpan::Medi
                        && matches!(
                            first_unit.unit,
                            WrittenUnit::A
                                | WrittenUnit::I
                                | WrittenUnit::O
                                | WrittenUnit::U
                                | WrittenUnit::Ue
                                | WrittenUnit::Y
                        )));

            if matches!(first_span, JoiningSpan::Medi | JoiningSpan::Fina) && !valid_post_mvs_stem {
                diagnostics.push(
                    DiagnosticIssue::ChainPositionBreak {
                        cluster: first_cluster,
                        detail: if keep_detail {
                            format!(
                                "chain starts with non-initial form {:?} ({:?})",
                                first_unit.unit, first_span
                            )
                        } else {
                            String::new()
                        },
                    },
                    keep_detail,
                );
            }
        }
    }
}

/// Normalize a positioned written-unit run through `mongol-norm`.
///
/// Handles two real-world legacy-font nuances automatically:
/// 1. Legacy fonts often label post-MVS chachlag `-a/-e` as `Aa:fina` (`a_3.fina` / `U1820_MVS`),
///    whereas UTN #57 models post-MVS `-a/-e` as `Aa:isol`; we canonicalize `[..., Mvs, Aa:fina]` -> `[..., Mvs, Aa:isol]`.
/// 2. If a font outputs internal positional labels that slightly disagree with `mongol-norm`'s strict
///    `slot_position` check (e.g. a two-unit isolated vowel ligature `A:isol + I:isol`), we fall back
///    cleanly to `shaper.normalize_written_units` on the exact unit sequence.
///
/// 调用 `mongol-norm` 将带位置字形单元序列还原为标准 UTN #57 Unicode 文本，
/// 并自动处理传统字体常见的两处细节差异：
/// 1. 许多传统字体将 `MVS` 后的隔音元音标为 `fina`（如 `a_3.fina`），此处自动对齐为 UTN #57 的 `Aa:isol`；
/// 2. 若个别字体的内部位置标签与 `normalize_positioned_written_units` 槽位严格校验存在标签级出入，
///    平滑回退至同字形序列的 `normalize_written_units`。
fn normalize_positioned_run(
    shaper: &Shaper,
    units: &[PositionedWrittenUnit],
) -> Result<String, mongol_norm::Error> {
    if units.is_empty() {
        return Ok(String::new());
    }

    let mut adjusted: Vec<PositionedWrittenUnit> = units.to_vec();
    for i in 0..adjusted.len() {
        let prev_is_mvs = i > 0 && adjusted[i - 1].unit == WrittenUnit::Mvs;
        let next_is_end = i + 1 == adjusted.len()
            || adjusted[i + 1].position == UnitPosition::Control;
        if prev_is_mvs && next_is_end && adjusted[i].unit == WrittenUnit::Aa {
            adjusted[i] = PositionedWrittenUnit::new(WrittenUnit::Aa, UnitPosition::Isol);
        }
        // In UTN #57 inventory, consonants have no `Isol` slot (they borrow `Init` when isolated,
        // except `U`, `Ux`, `Aa`, `Ix`, `A`, `I`). Map `Consonant:isol` -> `Consonant:init`.
        if adjusted[i].position == UnitPosition::Isol
            && !matches!(
                adjusted[i].unit,
                WrittenUnit::A
                    | WrittenUnit::Aa
                    | WrittenUnit::I
                    | WrittenUnit::Ix
                    | WrittenUnit::U
                    | WrittenUnit::Ux
            )
        {
            adjusted[i] = PositionedWrittenUnit::new(adjusted[i].unit, UnitPosition::Init);
        }
    }

    match shaper.normalize_positioned_written_units(&adjusted) {
        Ok(s) => Ok(s),
        Err(_) => {
            let plain: Vec<WrittenUnit> = adjusted
                .iter()
                .filter(|u| u.unit != WrittenUnit::Zwnj)
                .map(|u| u.unit)
                .collect();
            shaper.normalize_written_units(&plain)
        }
    }
}

fn is_ignorable_mongolian_control(ch: char) -> bool {
    matches!(
        ch as u32,
        0x180B..=0x180F | 0x200C | 0x200D | 0xFEFF
    )
}

/// Returns `true` when `ch` is clearly outside the active Mongolian encoding's code space
/// (whitespace, Mongolian/general punctuation, digits, CJK, Cyrillic, or ASCII when not `LatinMapped`).
///
/// 判断字符 `ch` 是否明确属于当前蒙文编码字形空间之外的常规透传字符（如空格、标点、数字、汉字、西里尔文等）。
fn is_safe_external_passthrough(ch: char, encoding: EncodingClass) -> bool {
    let cp = ch as u32;
    if ch.is_whitespace() && cp != 0x180E && cp != 0x202F {
        return true;
    }
    // Standard Mongolian punctuation & digits (`U+1800..U+1806`, `U+1808..U+1809`, `U+1810..U+1819`).
    if matches!(cp, 0x1800..=0x1806 | 0x1808..=0x1809 | 0x1810..=0x1819) {
        return true;
    }
    // ASCII printable characters are external passthrough for Unicode / PUA / Z52 fonts,
    // EXCEPT for 1990s `LatinMapped` fonts (Danzan / HuaGuang) which draw Mongolian on `0x21..0x7E`!
    if matches!(cp, 0x0021..=0x007E) {
        return encoding != EncodingClass::LatinMapped;
    }
    // General Unicode scripts outside Mongolian (`U+1800..U+18AF`) and PUA (`U+E000..U+F8FF`).
    !matches!(cp, 0x1800..=0x18AF | 0x200C..=0x200D | 0x202F | 0xE000..=0xF8FF)
}
