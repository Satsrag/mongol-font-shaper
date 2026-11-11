//! Error and shaping diagnostic types.
//! 错误类型与塑形异常诊断类型。

use std::fmt;

use mongol_norm::{UnitPosition, WrittenUnit};
use serde::{Deserialize, Serialize};

/// Errors produced when loading a font, compiling a [`FontProfile`](crate::FontProfile),
/// or shaping/normalizing text.
///
/// 加载字体、编译 [`FontProfile`](crate::FontProfile) 或塑形/规范化文本时产生的错误。
#[derive(Debug)]
pub enum Error {
    /// Failed to parse the font binary (`TTF` / `OTF` / `TTC`).
    /// 无法解析字体二进制数据（`TTF` / `OTF` / `TTC`）。
    InvalidFont(String),

    /// Failed to parse or serialize a JSON profile.
    /// 解析或序列化 JSON 家族配置文件失败。
    ProfileJson(serde_json::Error),

    /// An invalid written-unit or positioned-unit token in a profile rule.
    /// 配置文件规则中包含非法的字形单元（`WrittenUnit`）或带位置字形单元写法。
    InvalidUnitToken {
        /// The offending token string / 出错的字符串标记。
        token: String,
        /// Explanation of why parsing failed / 解析失败的原因说明。
        reason: String,
    },

    /// `harfrust` returned an internal shaping failure.
    /// `harfrust` 底层塑形引擎返回错误。
    ShapeFailure(String),

    /// A glyph produced during shaping has no mapping rule in the compiled profile
    /// (when strict mode is enabled).
    /// 塑形产出的某个字形在当前配置中未映射（严格模式下报错）。
    UnmappedGlyph {
        /// Glyph ID in the font / 字体中的 Glyph ID。
        glyph_id: u16,
        /// Optional `post` glyph name / 可选的 `post` 字形名。
        glyph_name: Option<String>,
        /// Character index in the input / 输入文本中的字符索引。
        cluster: usize,
        /// Original character at `cluster` / 该位置对应的原始字符。
        ch: char,
    },

    /// `mongol-norm` rejected a shaped written-unit run during canonical normalization.
    /// `mongol-norm` 在将字形单元序列还原为标准 UTN #57 文本时报错。
    Normalize(mongol_norm::Error),

    /// Filesystem I/O failure (used by CLI and file-loading helpers).
    /// 文件系统读写错误（用于 CLI 及文件加载辅助函数）。
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFont(msg) => {
                write!(f, "invalid font data (无效的字体数据): {msg}")
            }
            Self::ProfileJson(err) => {
                write!(f, "profile JSON error (配置文件 JSON 错误): {err}")
            }
            Self::InvalidUnitToken { token, reason } => write!(
                f,
                "invalid unit token '{token}' (非法的字形单元标记 '{token}'): {reason}"
            ),
            Self::ShapeFailure(msg) => {
                write!(f, "OpenType/AAT shaping failed (塑形失败): {msg}")
            }
            Self::UnmappedGlyph {
                glyph_id,
                glyph_name,
                cluster,
                ch,
            } => {
                let name_part = glyph_name
                    .as_deref()
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default();
                write!(
                    f,
                    "unmapped glyph #{glyph_id}{name_part} at char index {cluster} (U+{:04X} '{ch}') / \
                     字符索引 {cluster} 处的字形 #{glyph_id}{name_part} 未配置映射",
                    *ch as u32
                )
            }
            Self::Normalize(err) => {
                write!(
                    f,
                    "UTN #57 normalization error (UTN #57 规范化错误): {err}"
                )
            }
            Self::Io(err) => write!(f, "I/O error (读写错误): {err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ProfileJson(err) => Some(err),
            Self::Normalize(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::ProfileJson(err)
    }
}

impl From<mongol_norm::Error> for Error {
    fn from(err: mongol_norm::Error) -> Self {
        Self::Normalize(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Specific defect observed while shaping text with a candidate font.
/// 使用某款字体塑形文本时检测到的具体异常项。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiagnosticIssue {
    /// `.notdef` (glyph ID 0) was emitted because the font lacks a mapping for `ch`.
    /// 字体缺少字符 `ch` 的映射，吐出了 `.notdef`（0 号缺字豆腐块）。
    NotDef {
        /// Character index in the input / 输入字符索引。
        cluster: usize,
        /// Unicode code point scalar / Unicode 码位值。
        codepoint: u32,
    },

    /// A visible unconsumed FVS / dotted-circle placeholder glyph was drawn.
    /// Zero-width effective FVS markers (as in Oyun v2 / Menk GB2023) are `Silent` and NOT flagged here.
    ///
    /// 吐出了可见的未消费 FVS 虚线圆圈或报错占位字形。
    /// 注：Oyun v2 / Menk GB2023 中合法生效的零宽 FVS 标记属于 `Silent`，不会在此误报。
    VisibleFvs {
        /// Character index in the input / 输入字符索引。
        cluster: usize,
        /// Glyph ID emitted by the font / 字体吐出的 Glyph ID。
        glyph_id: u16,
    },

    /// A glyph with ink was encountered that is neither mapped to Mongolian units nor known punctuation/ASCII.
    /// 遇到了带笔墨轮廓但未映射为蒙文字形单元或常规标点/ASCII 的未知字形。
    UnmappedGlyph {
        /// Character index in the input / 输入字符索引。
        cluster: usize,
        /// Glyph ID emitted by the font / 字体吐出的 Glyph ID。
        glyph_id: u16,
        /// Unicode code point scalar / 原始字符码位。
        codepoint: u32,
    },

    /// Positional joining break inside a continuous Mongolian chain (e.g. a word ending in a `medi` tooth,
    /// or `Baiti` rendering `1832 1826 180D` with `uni1826.medivar1` at the end of a word).
    ///
    /// 连续蒙文词串内部发生位置形态连写断裂（例如词尾出现了词中连写齿 `medi`，
    /// 典型如 `170` 词典的 `1832 1826 180D` 在微软 Baiti 下词尾吐出 `uni1826.medivar1`）。
    ChainPositionBreak {
        /// Character index near the break / 断裂处的输入字符索引。
        cluster: usize,
        /// Human-readable description of the mismatch / 位置冲突说明。
        detail: String,
    },

    /// Canonical UTN #57 encoder could not encode a shaped Mongolian run without fallback.
    /// 标准 UTN #57 编码器无法直接编码该字形序列。
    NormalizationFallback {
        /// Character index of the run start / 词段起始字符索引。
        cluster: usize,
        /// Underlying `mongol-norm` error message / 底层 `mongol-norm` 错误信息。
        detail: String,
    },
}

impl DiagnosticIssue {
    /// Format a bilingual summary of this issue.
    /// 格式化该异常项的中英双语简述。
    pub fn summary(&self) -> String {
        match self {
            Self::NotDef { cluster, codepoint } => format!(
                ".notdef at char #{cluster} (U+{codepoint:04X}) / 字符 #{cluster} (U+{codepoint:04X}) 缺字(.notdef)"
            ),
            Self::VisibleFvs { cluster, glyph_id } => format!(
                "visible unconsumed FVS/placeholder glyph #{glyph_id} at char #{cluster} / 字符 #{cluster} 残留可见 FVS 报错字形 #{glyph_id}"
            ),
            Self::UnmappedGlyph {
                cluster,
                glyph_id,
                codepoint,
            } => format!(
                "unmapped glyph #{glyph_id} at char #{cluster} (U+{codepoint:04X}) / 字符 #{cluster} (U+{codepoint:04X}) 产出未映射字形 #{glyph_id}"
            ),
            Self::ChainPositionBreak { cluster, detail } => format!(
                "joining position break at char #{cluster}: {detail} / 字符 #{cluster} 位置连写断裂: {detail}"
            ),
            Self::NormalizationFallback { cluster, detail } => format!(
                "UTN #57 normalization fallback at char #{cluster}: {detail} / 字符 #{cluster} 规范化回退: {detail}"
            ),
        }
    }
}

/// Aggregated shaping diagnostics and defect counts for one input string or corpus.
/// 单条文本或整批语料的塑形诊断汇总与错码计数。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostics {
    /// Count of `.notdef` glyphs emitted / `.notdef` 缺字豆腐块数量。
    pub notdef_count: usize,
    /// Count of visible unconsumed FVS or dotted-circle glyphs / 可见残留 FVS 或虚线圆圈数量。
    pub visible_fvs_count: usize,
    /// Count of unmapped ink glyphs / 未映射字形数量。
    pub unmapped_glyph_count: usize,
    /// Count of positional joining breaks (e.g. `medi` at word end) / 词首词中词尾连写断裂数量。
    pub chain_break_count: usize,
    /// Count of runs that required normalization fallback / 规范化失败或回退次数。
    pub normalization_error_count: usize,
    /// Count of Mongolian written units successfully produced / 成功产出的蒙古文字形单元总数。
    pub shaped_unit_count: usize,
    /// Detailed issue list (recorded for single-line inspection) / 详细异常列表。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<DiagnosticIssue>,
}

impl Diagnostics {
    /// Returns `true` when zero defects occurred (`defect_score() == 0`).
    /// 当没有任何错码或连写断裂时返回 `true`。
    pub const fn is_clean(&self) -> bool {
        self.notdef_count == 0
            && self.visible_fvs_count == 0
            && self.unmapped_glyph_count == 0
            && self.chain_break_count == 0
            && self.normalization_error_count == 0
    }

    /// Total defect count (lower is better; `0` means completely valid under this font).
    /// 总异常数（越低越好，`0` 表示在该字体下完全合法）。
    pub const fn total_defects(&self) -> usize {
        self.notdef_count
            + self.visible_fvs_count
            + self.unmapped_glyph_count
            + self.chain_break_count
            + self.normalization_error_count
    }

    /// Weighted penalty score used when ranking candidate fonts across a noisy corpus.
    /// 在含少量录入瑕疵的语料上对候选字体排序时使用的加权罚分。
    pub const fn penalty_score(&self) -> usize {
        self.notdef_count * 100
            + self.visible_fvs_count * 50
            + self.chain_break_count * 40
            + self.unmapped_glyph_count * 30
            + self.normalization_error_count * 20
    }

    /// Record one [`DiagnosticIssue`] and increment its corresponding counter.
    /// 记录一条 [`DiagnosticIssue`] 并累加对应计数器。
    pub fn push(&mut self, issue: DiagnosticIssue, keep_detail: bool) {
        match &issue {
            DiagnosticIssue::NotDef { .. } => self.notdef_count += 1,
            DiagnosticIssue::VisibleFvs { .. } => self.visible_fvs_count += 1,
            DiagnosticIssue::UnmappedGlyph { .. } => self.unmapped_glyph_count += 1,
            DiagnosticIssue::ChainPositionBreak { .. } => self.chain_break_count += 1,
            DiagnosticIssue::NormalizationFallback { .. } => self.normalization_error_count += 1,
        }
        if keep_detail {
            self.issues.push(issue);
        }
    }

    /// Merge counters from another [`Diagnostics`] (used in batch corpus detection).
    /// 合并另一份 [`Diagnostics`] 的统计计数（用于语料级批量字体判别）。
    pub fn accumulate(&mut self, other: &Diagnostics) {
        self.notdef_count += other.notdef_count;
        self.visible_fvs_count += other.visible_fvs_count;
        self.unmapped_glyph_count += other.unmapped_glyph_count;
        self.chain_break_count += other.chain_break_count;
        self.normalization_error_count += other.normalization_error_count;
        self.shaped_unit_count += other.shaped_unit_count;
    }
}

/// Format a [`PositionedWrittenUnit`](mongol_norm::PositionedWrittenUnit) into canonical
/// compact notation (`"B:init"`, `"Aa:isol"`, `"Mvs"`, `"Nirugu"`, `"Zwnj"`).
///
/// 将 [`PositionedWrittenUnit`](mongol_norm::PositionedWrittenUnit) 格式化为标准紧凑记法
/// （如 `"B:init"`、`"Aa:isol"`、`"Mvs"`、`"Nirugu"`、`"Zwnj"`）。
pub fn format_positioned_unit(unit: WrittenUnit, position: UnitPosition) -> String {
    match position {
        UnitPosition::Control => unit.as_str().to_owned(),
        UnitPosition::Isol => format!("{}:isol", unit.as_str()),
        UnitPosition::Init => format!("{}:init", unit.as_str()),
        UnitPosition::Medi => format!("{}:medi", unit.as_str()),
        UnitPosition::Fina => format!("{}:fina", unit.as_str()),
    }
}
