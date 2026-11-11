//! Family-level [`FontProfile`] configuration and dynamic binding to a concrete font's glyph table.
//! 家族级字体画像配置（[`FontProfile`]）及其向具体字体 `Glyph ID` 查找表的动态编译。

use std::collections::HashMap;
use std::path::Path;

use harfrust::{
    shape, Buffer, BufferFlags, ClusterLevel, Direction, Font, Language, Script, ShapeOptions,
    ShaperFont,
};
use mongol_norm::{PositionedWrittenUnit, UnitPosition, WrittenUnit};
use read_fonts::tables::name::NameId;
use read_fonts::types::GlyphId;
use serde::{Deserialize, Serialize};

use crate::error::{format_positioned_unit, Error};

/// Schema identifier for human-editable profile JSON files.
/// 人类可读可编辑的 JSON 配置文件 Schema 版本标识。
pub const PROFILE_SCHEMA_V1: &str = "mongol-font-shaper-profile-v1";

/// Script/encoding class of a font profile, controlling how [`FontPool`](crate::FontPool)
/// schedules it during automatic detection.
///
/// 字体画像的编码归属类别，用于控制 [`FontPool`](crate::FontPool) 在自动探测时的调度策略。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncodingClass {
    /// Standard Unicode Mongolian (`U+1800..U+18AF` + `U+200C/200D/202F`), e.g. UTN #57, Menk, Oyun, Delehi, Baiti.
    /// 标准 Unicode 蒙古文编码体系（如 UTN #57、蒙科立 GB2010/2023、奥云、德力海、微软 Baiti）。
    #[default]
    Unicode,

    /// Private Use Area shape-code fonts (`U+E000..U+F8FF`), e.g. Menk-PUA (FangZheng/GLT/CMs), ZvvnMod, Delehi-PUA.
    /// 私用区字形码字体体系（如方正/GLT/CMs 等 Menk-PUA `0xE234..`、ZvvnMod `0xE000..`、德力海 PUA）。
    Pua,

    /// Borrowed Mongolian block codepoints (`U+184A..U+18AA`), e.g. Z52 pseudo-Unicode fonts.
    /// 借用托忒/锡伯/满文码位存放胡都木字形的编码体系（如 Z52 字体家族）。
    Z52,

    /// 1990s legacy ASCII/Latin-mapped fonts (`0x21..0x7E`), e.g. Danzan, MNZ, HuaGuang HASMW.
    /// Excluded from default `FontPool` auto-detection unless explicitly enabled so English text is not misclassified.
    ///
    /// 90 年代占用 ASCII 英文键盘码位（`0x21..0x7E`）的老式蒙文字体（如 Danzan、华光 HASMW）。
    /// 默认不参与常规多字体池盲测（除非显式开启），避免将普通英文单词误判为蒙文。
    LatinMapped,
}

/// How a single glyph contributes to the shaped stream.
/// 单个 Glyph 在塑形结果流中的语义动作。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    /// Emits one or more [`PositionedWrittenUnit`] records (e.g. `["B:init"]` or ligature `["B:init", "A:medi"]`).
    /// 产出一个或多个带位置字形单元（例如单字形 `["B:init"]`，或合字 `["B:init", "A:medi"]`）。
    Units,

    /// Zero-ink / consumed helper glyph (e.g. effective zero-width `FVS1` in Oyun/Menk2023, consumed joiner).
    /// 已被正常消费的零宽辅助字形（如 Oyun/Menk2023 中合法生效后保留的零宽 `FVS1`、内部静默标记等）。
    Silent,

    /// Non-Mongolian character or punctuation/whitespace to preserve verbatim from input text.
    /// 原样保留原始输入字符的非蒙文字形（如空格、标点、数字、汉字、拉丁字母等）。
    Passthrough,

    /// Visible unconsumed FVS or dotted-circle error glyph (`U+25CC` / invalid FVS box).
    /// 可见的未消化 FVS 虚线圆圈或错码占位字形（触发错码诊断扣分）。
    VisibleFvs,

    /// `.notdef` missing-glyph tofu box.
    /// `.notdef` 缺字豆腐块（触发缺字诊断扣分）。
    NotDef,
}

/// Shorthand shaping probe that dynamically locates a glyph ID in fonts stripped of `post` names
/// and PUA mappings (such as `monbaiti-v5.53`, `DraftNew-Regular.otf`, and Noto `.otf` builds).
///
/// 塑形探针：针对剥离了 `post` 字形名且无 PUA 码位的黑盒字体（如 Win10/11 版 `monbaiti-v5.53`、
/// `DraftNew.otf`、Noto `.otf` 等），通过实际运行测试输入串动态定位目标 `Glyph ID`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShapingProbe {
    /// Input Unicode string fed into `harfrust` / 喂给 `harfrust` 的探针输入字符串。
    pub text: String,
    /// Index in the resulting glyph buffer (`0`-based) / 目标字形在塑形输出缓冲区中的下标（从 0 开始）。
    pub glyph_index: usize,
    /// Optional start character cluster (`0`-based) for FVS-shift-resilient matching / 目标字形对应的起始字符索引（防 FVS 零宽字形偏移）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<usize>,
    /// Optional exclusive end character cluster (`0`-based) to distinguish ligatures from single letters / 目标字形覆盖的结束字符开区间（防止合字探针误绑单字母）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_cluster: Option<usize>,
}

/// One human-editable mapping rule inside a [`FontProfile`].
///
/// Anchors are tried in priority order when binding to a font:
/// `glyph_names` -> `codepoints` -> `probes` -> fallback `gid`.
///
/// [`FontProfile`] 中的单条人类可编辑字形映射规则。
/// 绑定具体字体时按优先级依次匹配：`glyph_names`（字形名） -> `codepoints`（码位） -> `probes`（塑形探针） -> `gid`（兜底编号）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlyphRuleSpec {
    /// Action kind for matched glyph(s) / 匹配字形的语义类别。
    pub kind: RuleKind,

    /// Space- or comma-separated `PositionedWrittenUnit` specs when `kind == RuleKind::Units`
    /// (for example `"B:init A:medi"` or `"N:fina Mvs Aa:isol"`).
    ///
    /// 当 `kind == RuleKind::Units` 时的目标字形单元序列（以空格分隔，如 `"B:init A:medi"` 或 `"N:fina Mvs Aa:isol"`）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub units: String,

    /// Optional explicit joining span `(entry_pos, exit_pos)` override when a glyph's visual tooth
    /// differs from its unit inventory slot (e.g. isolated consonant borrowing `init` form: `"isol"`).
    ///
    /// 可选的连写上下文位置声明（如孤立辅音借用词首字形 `"B:init"` 时，标记实际连接环境为 `"isol"`，避免连写检查误判）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joining: Option<String>,

    /// Candidate `post` / `CFF` glyph names (e.g. `["uni1820.Aa.fina", "u1820.Aa.fina"]`).
    /// 候选 `post` 或 `CFF` 字形名列表（同家族字体共享）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub glyph_names: Vec<String>,

    /// Candidate `cmap` Unicode/PUA/ASCII codepoints formatted as `"U+E264"` or `"U+0041"`.
    /// 候选 `cmap` 码位列表（格式如 `"U+E264"` 或 `"U+0041"`，通吃 Menk-PUA、ZvvnMod、Z52、Latin-Mapped）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codepoints: Vec<String>,

    /// Candidate dynamic shaping probes (works even on nameless `post v3.0` fonts).
    /// 候选动态塑形探针列表（在无字形名的 `post v3.0` 字体上同样生效）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<ShapingProbe>,

    /// Optional direct glyph ID (only used when none of the portable anchors above are present, or in per-font overrides).
    /// 可选的直接 Glyph ID（仅在无通用锚点或单字体局部覆盖时使用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u16>,

    /// Flagged `true` by `init-map` when heuristic confidence is < 100% so a human reviewer can filter quickly.
    /// 预生成工具 `init-map` 对自动推断置信度不足 100% 的条目会标为 `true`，便于人工快速检索核对。
    #[serde(default, skip_serializing_if = "is_false")]
    pub review: bool,

    /// Optional human-readable comment / sample word note.
    /// 可选的人类可读备注或触发示例词说明。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Per-font lightweight override block applied after family rules when the font's PostScript
/// or family name matches `font_name_contains`.
///
/// 针对同家族内极个别差异衍生字体的轻量级局部覆盖块（按字体名称子串匹配生效，无需复制整张家族表）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontOverrideSpec {
    /// Case-insensitive substring matched against the font's PostScript or family name.
    /// 与字体 PostScript 名或家族名进行大小写不敏感匹配的子串。
    pub font_name_contains: String,

    /// Override rules applied on top of the base family rules.
    /// 叠加在基础家族规则之上的局部覆盖规则。
    pub rules: Vec<GlyphRuleSpec>,
}

/// Portable family-level font mapping profile (`*.profile.json`).
/// 可移植的家族级字体映射配置（`*.profile.json`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontProfile {
    /// Schema identifier (`"mongol-font-shaper-profile-v1"`).
    /// 配置文件 Schema 版本号。
    #[serde(default = "default_schema")]
    pub schema: String,

    /// Short identifier of this profile (e.g. `"utn57"`, `"menk-gb2023"`, `"menk-pua"`).
    /// 家族配置短标识名（如 `"utn57"`、`"menk-gb2023"`、`"menk-pua"`）。
    pub id: String,

    /// Human-readable bilingual description.
    /// 人类可读的中英双语描述。
    #[serde(default)]
    pub description: String,

    /// Encoding category of this font family.
    /// 该字体家族所属的编码类别。
    #[serde(default)]
    pub encoding_class: EncodingClass,

    /// BCP-47 language tag passed to HarfBuzz/harfrust (defaults to `"mn"` -> OpenType `MNG `).
    /// 传给 HarfBuzz/harfrust 的语言标签（默认 `"mn"`，对应 OpenType `MNG ` 语言系统）。
    #[serde(default = "default_language")]
    pub language: String,

    /// Base family rules resolved dynamically against each loaded font.
    /// 针对所加载字体动态解析的基础家族字形规则列表。
    pub rules: Vec<GlyphRuleSpec>,

    /// Optional per-font overrides for minor glyph variants within the same family.
    /// 同家族内个别字体差异的局部覆盖规则。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<FontOverrideSpec>,
}

fn default_schema() -> String {
    PROFILE_SCHEMA_V1.to_owned()
}

fn default_language() -> String {
    "mn".to_owned()
}

impl FontProfile {
    /// Create a new empty [`FontProfile`] with the given identifier and [`EncodingClass`].
    /// 使用给定的标识符与编码类别 [`EncodingClass`] 创建一个新的空 [`FontProfile`]。
    pub fn new(id: impl Into<String>, encoding_class: EncodingClass) -> Self {
        Self {
            schema: default_schema(),
            id: id.into(),
            description: String::new(),
            encoding_class,
            language: default_language(),
            rules: Vec::new(),
            overrides: Vec::new(),
        }
    }

    /// Parse a [`FontProfile`] from a JSON string.
    /// 从 JSON 字符串解析 [`FontProfile`]。
    pub fn from_json_str(json: &str) -> Result<Self, Error> {
        Ok(serde_json::from_str(json)?)
    }

    /// Load a [`FontProfile`] from a JSON file on disk.
    /// 从磁盘 JSON 文件加载 [`FontProfile`]。
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)?;
        Self::from_json_str(&text)
    }

    /// Serialize this [`FontProfile`] as pretty-printed human-editable JSON.
    /// 将 [`FontProfile`] 序列化为格式美观、便于人工修改的 JSON 字符串。
    pub fn to_json_pretty(&self) -> Result<String, Error> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Count how many rules are still marked `"review": true`.
    /// 统计当前配置中仍标记为 `"review": true` 待人工确认的规则条数。
    pub fn pending_review_count(&self) -> usize {
        self.rules.iter().filter(|r| r.review).count()
    }
}

/// Joining behavior of a compiled glyph entry for positional continuity verification.
/// 编译后单个字形的实际连接位置属性，用于检测词首/词中/词尾连写断裂。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoiningSpan {
    /// Non-letter or control (`Mvs`, `Nirugu`, `Zwnj`).
    /// 控制符或非字母字形。
    Control,
    /// Isolated word form (neither joins left nor right).
    /// 独立形（左右均不连写）。
    Isol,
    /// Initial form (joins right, starts a chain).
    /// 词首形（向后连写）。
    Init,
    /// Medial form (joins both left and right).
    /// 词中形（前后均连写）。
    Medi,
    /// Final form (joins left, ends a chain).
    /// 词尾形（向前连写）。
    Fina,
    /// Complete multi-unit chain ending at `fina` / `isol` (e.g. `Hx:fina Mvs Aa:isol`).
    /// 自包含完整词段或跨 MVS 组合段。
    CompleteRun,
}

/// O(1) runtime entry indexed by `GlyphId` after binding a [`FontProfile`] to a [`Font`].
/// 将 [`FontProfile`] 绑定到具体 [`Font`] 后生成的按 `GlyphId` 下标 O(1) 访问的运行时条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompiledGlyph {
    /// Emits positioned written units.
    /// 产出带位置的蒙古文字形单元序列。
    Units {
        /// Positioned UTN #57 written units drawn by this glyph / 该字形对应的带位置字形单元序列。
        units: Box<[PositionedWrittenUnit]>,
        /// Effective joining span in the word / 该字形在词串中的实际连接位置角色。
        joining: JoiningSpan,
    },
    /// Consumed silent helper glyph.
    /// 已被正常消化的静默零宽辅助字形。
    Silent,
    /// Preserves original input character(s) verbatim.
    /// 原样透传输入字符（空格、标点、外文等）。
    Passthrough,
    /// Visible unconsumed FVS / dotted-circle placeholder defect.
    /// 可见的未消化 FVS 虚线圆圈或报错占位符。
    VisibleFvs,
    /// `.notdef` missing glyph box.
    /// `.notdef` 缺字豆腐块。
    NotDef,
    /// Glyph not covered by any explicit rule; runtime inspects ink & codepoint to classify as passthrough or defect.
    /// 未被规则显式覆盖的字形；运行时根据是否有笔墨轮廓及码位判断透传或告警。
    Unmapped {
        /// Whether the glyph draws visible ink (`extents` non-empty) / 该字形是否带有可见轮廓笔墨。
        has_ink: bool,
    },
}

/// Parse a compact positioned unit token (`"B:init"`, `"Aa:isol"`, `"Mvs"`, `"Nirugu"`, `"Zwnj"`).
/// 解析单个紧凑格式的带位置字形单元标记（如 `"B:init"`、`"Aa:isol"`、`"Mvs"`、`"Nirugu"`、`"Zwnj"`）。
pub fn parse_positioned_unit_token(token: &str) -> Result<PositionedWrittenUnit, Error> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidUnitToken {
            token: token.to_owned(),
            reason: "empty token / 空标记".to_owned(),
        });
    }
    if let Some((unit_str, pos_str)) = trimmed.split_once(':') {
        let unit = parse_unit_name(unit_str, trimmed)?;
        let position = match pos_str {
            "isol" => UnitPosition::Isol,
            "init" => UnitPosition::Init,
            "medi" => UnitPosition::Medi,
            "fina" => UnitPosition::Fina,
            "control" => UnitPosition::Control,
            _ => {
                return Err(Error::InvalidUnitToken {
                    token: trimmed.to_owned(),
                    reason: format!(
                        "unknown position '{pos_str}' (expected isol|init|medi|fina|control) / 未知位置 '{pos_str}'"
                    ),
                })
            }
        };
        Ok(PositionedWrittenUnit::new(unit, position))
    } else {
        let unit = parse_unit_name(trimmed, trimmed)?;
        match unit {
            WrittenUnit::Mvs | WrittenUnit::Nirugu | WrittenUnit::Zwnj | WrittenUnit::Zwj => {
                Ok(PositionedWrittenUnit::new(unit, UnitPosition::Control))
            }
            _ => Err(Error::InvalidUnitToken {
                token: trimmed.to_owned(),
                reason: "letter unit requires explicit ':isol', ':init', ':medi', or ':fina' / 字母单元必须显式指定位置后缀".to_owned(),
            }),
        }
    }
}

fn parse_unit_name(unit_str: &str, full_token: &str) -> Result<WrittenUnit, Error> {
    let normalized = match unit_str {
        "MVS" => "Mvs",
        "ZWJ" => "Zwj",
        "ZWNJ" => "Zwnj",
        other => other,
    };
    normalized
        .parse::<WrittenUnit>()
        .map_err(|_| Error::InvalidUnitToken {
            token: full_token.to_owned(),
            reason: format!("unknown UTN #57 WrittenUnit '{unit_str}' / 未知的字形单元名 '{unit_str}'"),
        })
}

/// Parse a whitespace/comma-separated sequence of positioned written units.
/// 解析以空格或逗号分隔的带位置字形单元序列字符串。
pub fn parse_positioned_unit_list(spec: &str) -> Result<Vec<PositionedWrittenUnit>, Error> {
    let mut out = Vec::new();
    for raw in spec.split(|c: char| c.is_whitespace() || c == ',') {
        if raw.is_empty() {
            continue;
        }
        out.push(parse_positioned_unit_token(raw)?);
    }
    if out.is_empty() {
        return Err(Error::InvalidUnitToken {
            token: spec.to_owned(),
            reason: "RuleKind::Units requires at least one unit / Units 规则至少需要一个字形单元"
                .to_owned(),
        });
    }
    Ok(out)
}

/// Format a slice of [`PositionedWrittenUnit`] into space-separated canonical notation.
/// 将 [`PositionedWrittenUnit`] 切片格式化为空格分隔的标准字符串。
pub fn format_positioned_unit_list(units: &[PositionedWrittenUnit]) -> String {
    units
        .iter()
        .map(|u| format_positioned_unit(u.unit, u.position))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse a `"U+XXXX"` or `"0xXXXX"` codepoint string into `u32`.
/// 将 `"U+XXXX"` 或 `"0xXXXX"` 形式的码位字符串解析为 `u32`。
pub fn parse_codepoint_str(text: &str) -> Result<u32, Error> {
    let s = text.trim();
    let hex = s
        .strip_prefix("U+")
        .or_else(|| s.strip_prefix("u+"))
        .or_else(|| s.strip_prefix("0x"))
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u32::from_str_radix(hex, 16).map_err(|_| Error::InvalidUnitToken {
        token: text.to_owned(),
        reason: "expected hex codepoint like 'U+E264' / 需要形如 'U+E264' 的十六进制码位".to_owned(),
    })
}

fn infer_joining_span(
    units: &[PositionedWrittenUnit],
    explicit: Option<&str>,
) -> Result<JoiningSpan, Error> {
    if let Some(tag) = explicit {
        return match tag.trim() {
            "isol" => Ok(JoiningSpan::Isol),
            "init" => Ok(JoiningSpan::Init),
            "medi" => Ok(JoiningSpan::Medi),
            "fina" => Ok(JoiningSpan::Fina),
            "control" => Ok(JoiningSpan::Control),
            "complete" => Ok(JoiningSpan::CompleteRun),
            other => Err(Error::InvalidUnitToken {
                token: other.to_owned(),
                reason: "joining must be isol|init|medi|fina|control|complete / 非法的连接属性"
                    .to_owned(),
            }),
        };
    }

    if units.iter().all(|u| u.position == UnitPosition::Control) {
        return Ok(JoiningSpan::Control);
    }
    if units.iter().any(|u| u.position == UnitPosition::Control) {
        return Ok(JoiningSpan::CompleteRun);
    }
    let first = units[0].position;
    let last = units[units.len() - 1].position;
    let span = match (first, last) {
        (UnitPosition::Isol, UnitPosition::Isol) => JoiningSpan::Isol,
        (UnitPosition::Init, UnitPosition::Fina) => JoiningSpan::Isol,
        (UnitPosition::Init, _) => JoiningSpan::Init,
        (_, UnitPosition::Fina) => JoiningSpan::Fina,
        _ => JoiningSpan::Medi,
    };
    Ok(span)
}

/// Extract a human-readable font identifier (PostScript name preferred, then full/family name).
/// 提取字体内部标识名称（优先读取 PostScript 名，其次全名或家族名）。
pub fn extract_font_identity_name(font: &Font) -> String {
    let names = font.names();
    for id in [
        NameId::POSTSCRIPT_NAME,
        NameId::FULL_NAME,
        NameId::FAMILY_NAME,
    ] {
        if let Some(entry) = names.get(id).english_or_first() {
            let s = entry.name().to_string();
            if !s.trim().is_empty() {
                return s;
            }
        }
    }
    "UnknownFont".to_owned()
}

/// Returns `true` if `glyph` draws visible ink outlines in `font`.
/// 判断 `glyph` 在 `font` 中是否绘制了可见轮廓笔墨（用于区分零宽静默 FVS 与可见虚线圆圈报错字形）。
pub fn glyph_has_ink(font: &Font, glyph: GlyphId) -> bool {
    font.glyph_metrics()
        .extents(glyph)
        .is_some_and(|ext| ext.width > 0.0 || ext.height > 0.0)
}

/// Compile a [`FontProfile`] against a concrete [`Font`] into an O(1) `Vec<CompiledGlyph>` table.
/// 将家族级 [`FontProfile`] 动态绑定到具体 [`Font`]，生成按 `GlyphId` O(1) 索引的 `Vec<CompiledGlyph>`。
pub fn compile_profile_for_font(
    font: &Font,
    profile: &FontProfile,
) -> Result<(Vec<CompiledGlyph>, usize), Error> {
    let num_glyphs = font.num_glyphs() as usize;
    let mut table: Vec<CompiledGlyph> = (0..num_glyphs)
        .map(|gid| {
            if gid == 0 {
                CompiledGlyph::NotDef
            } else {
                CompiledGlyph::Unmapped {
                    has_ink: glyph_has_ink(font, GlyphId::new(gid as u32)),
                }
            }
        })
        .collect();

    // 1. Build name -> gid index from non-synthesized glyph names (`post` v2 or `CFF` charset).
    // 1. 从非合成的真实字形名（`post` v2 或 `CFF` 字符集）构建 `字形名 -> Glyph ID` 索引。
    let mut name_to_gids: HashMap<String, Vec<u16>> = HashMap::new();
    for (gid, gname) in font.glyph_names() {
        if !gname.is_synthesized() && gid.to_u32() < u16::MAX as u32 {
            name_to_gids
                .entry(gname.as_str().to_owned())
                .or_default()
                .push(gid.to_u32() as u16);
        }
    }

    // 2. Prepare charmap & shaping helper for `codepoints` and `probes`.
    // 2. 准备 `cmap` 映射与 `harfrust` 探针执行环境。
    let charmap = font.charmap();
    let language = Language::new(&profile.language);
    let shaper_font = ShaperFont::new(font);

    let mut bound_count = 0usize;

    let mut apply_rule = |spec: &GlyphRuleSpec, is_override: bool| -> Result<(), Error> {
        let compiled = build_compiled_glyph(spec)?;
        let mut matched_gids: Vec<u16> = Vec::new();
        let mut from_probe = false;

        // Anchor Priority 1: `post` / `CFF` glyph names.
        // 优先级 1：按 `post` / `CFF` 字形名匹配。
        for gname in &spec.glyph_names {
            if let Some(gids) = name_to_gids.get(gname) {
                matched_gids.extend(gids.iter().copied());
            }
        }

        // Anchor Priority 2: `cmap` Unicode / PUA / ASCII codepoints.
        // 优先级 2：按 `cmap` 码位（标准区 / PUA 私用区 / Z52 / ASCII）匹配。
        if matched_gids.is_empty() {
            for cp_str in &spec.codepoints {
                let cp = parse_codepoint_str(cp_str)?;
                if let Some(gid) = charmap.map_unicode(cp) {
                    if (gid.to_u32() as usize) < num_glyphs {
                        matched_gids.push(gid.to_u32() as u16);
                    }
                }
            }
        }

        // Anchor Priority 3: Dynamic shaping probes (handles `post v3.0` nameless fonts!).
        // 优先级 3：运行动态塑形探针（在剥离了全部字形名的 `post v3.0` 黑盒字体上照样精确命中！）。
        if matched_gids.is_empty() {
            from_probe = true;
            for probe in &spec.probes {
                if let Some(gid) = run_single_probe(font, &shaper_font, language.as_ref(), probe) {
                    if (gid as usize) < num_glyphs && gid != 0 {
                        matched_gids.push(gid);
                    }
                }
            }
        }

        // Anchor Priority 4: Direct `gid` fallback (only when portable anchors are all empty).
        // 优先级 4：仅当上述可移植锚点均为空时，才使用直接指定的 `gid`。
        if matched_gids.is_empty()
            && spec.glyph_names.is_empty()
            && spec.codepoints.is_empty()
            && spec.probes.is_empty()
        {
            if let Some(gid) = spec.gid {
                if (gid as usize) < num_glyphs {
                    matched_gids.push(gid);
                }
            }
        }

        for gid in matched_gids {
            if (gid as usize) < num_glyphs {
                if from_probe
                    && !is_override
                    && !matches!(table[gid as usize], CompiledGlyph::Unmapped { .. })
                {
                    continue;
                }
                // Smart FVS guard: if a rule marks an FVS glyph as `VisibleFvs`, verify whether this specific font
                // actually draws ink on it; zero-ink effective FVS markers become `Silent`.
                // 智能 FVS 保护：若某字形被标为未消费 FVS，但在当前字体中实际无任何笔墨轮廓（零宽），自动降级为 `Silent`。
                let final_entry = match &compiled {
                    CompiledGlyph::VisibleFvs
                        if !glyph_has_ink(font, GlyphId::new(u32::from(gid))) =>
                    {
                        CompiledGlyph::Silent
                    }
                    other => other.clone(),
                };
                table[gid as usize] = final_entry;
                bound_count += 1;
            }
        }
        Ok(())
    };

    for rule in &profile.rules {
        apply_rule(rule, false)?;
    }

    let font_ident = extract_font_identity_name(font).to_ascii_lowercase();
    for ov in &profile.overrides {
        if font_ident.contains(&ov.font_name_contains.to_ascii_lowercase()) {
            for rule in &ov.rules {
                apply_rule(rule, true)?;
            }
        }
    }

    Ok((table, bound_count))
}

fn build_compiled_glyph(spec: &GlyphRuleSpec) -> Result<CompiledGlyph, Error> {
    match spec.kind {
        RuleKind::Units => {
            let units = parse_positioned_unit_list(&spec.units)?;
            let joining = infer_joining_span(&units, spec.joining.as_deref())?;
            Ok(CompiledGlyph::Units {
                units: units.into_boxed_slice(),
                joining,
            })
        }
        RuleKind::Silent => Ok(CompiledGlyph::Silent),
        RuleKind::Passthrough => Ok(CompiledGlyph::Passthrough),
        RuleKind::VisibleFvs => Ok(CompiledGlyph::VisibleFvs),
        RuleKind::NotDef => Ok(CompiledGlyph::NotDef),
    }
}

fn run_single_probe(
    font: &Font,
    shaper_font: &ShaperFont<'_, '_>,
    language: Option<&Language>,
    probe: &ShapingProbe,
) -> Option<u16> {
    let chars: Vec<char> = probe.text.chars().collect();
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
    buffer.set_language(language.cloned());
    shape(shaper_font, &mut buffer, ShapeOptions::new()).ok()?;

    let infos = buffer.glyph_infos();
    let Some(expected_c) = probe.cluster else {
        let info = infos.get(probe.glyph_index)?;
        return u16::try_from(info.glyph_id).ok();
    };

    // Find the glyph at `probe.glyph_index` if its cluster matches, otherwise scan for `expected_c`.
    let candidate_idx = if infos
        .get(probe.glyph_index)
        .is_some_and(|i| i.cluster as usize == expected_c)
    {
        probe.glyph_index
    } else {
        infos
            .iter()
            .position(|i| i.cluster as usize == expected_c)?
    };

    let gid = u16::try_from(infos[candidate_idx].glyph_id).ok()?;
    if gid == 0 {
        return None;
    }

    if let Some(expected_end) = probe.end_cluster {
        // Compute actual end cluster skipping trailing zero-ink FVS/ZWJ helper glyphs.
        let mut next_idx = candidate_idx + 1;
        while next_idx < infos.len() {
            let ngid = infos[next_idx].glyph_id;
            let nc = infos[next_idx].cluster as usize;
            let nch = chars.get(nc).copied().unwrap_or('\0');
            let is_zero_helper = (nc == expected_c
                || matches!(nch as u32, 0x180B..=0x180D | 0x180F | 0x200D))
                && !glyph_has_ink(font, GlyphId::new(ngid));
            if is_zero_helper {
                next_idx += 1;
            } else {
                break;
            }
        }
        let actual_end = if next_idx < infos.len() {
            infos[next_idx].cluster as usize
        } else {
            chars.len()
        };
        if actual_end != expected_end {
            return None;
        }
    }

    Some(gid)
}
