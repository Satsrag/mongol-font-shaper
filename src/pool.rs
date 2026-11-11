//! Multi-font pool (`FontPool`) for automatic encoding/font discrimination and corpus-level voting.
//!
//! 多字体池（`FontPool`）：实现共识捷径、单条错码排除与整本词典/语料级批量编码投票鉴定。

use serde::{Deserialize, Serialize};

use crate::error::{Diagnostics, Error};
use crate::profile::EncodingClass;
use crate::shaper::FontShaper;

/// Resolution status when evaluating a single input string against a [`FontPool`].
/// 使用 [`FontPool`] 对单条输入文本进行多字体判别时的决议状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionStatus {
    /// Every clean (zero-defect) candidate font produced the exact same UTN #57 text!
    /// All ~75% standard words and all uniquely constrained FVS/PUA/Z52 words land here.
    ///
    /// 所有零错码候选字体产出了完全一致的标准 UTN #57 结果（涵盖约 75% 常规词及所有唯一匹配某家族的词条）。
    Exact,

    /// Two or more clean (zero-defect) candidate fonts produced different valid UTN #57 spellings
    /// (for example `1830 1820 1836 1822 1828` is valid `sayin` in Menk/Oyun and valid `sain` in Baiti).
    /// Use corpus-level [`FontPool::detect_corpus`] to lock the whole document's font first!
    ///
    /// 有两款及以上零错码字体各自产出了合法但不同的字形结果（如孤立单词在 Baiti 与 Menk 中均合法但含义不同）。
    /// 建议先对该词所在整篇文档或整本词典运行 [`FontPool::detect_corpus`] 锁定字体！
    Ambiguous,

    /// Every candidate font exhibited at least one defect; the lowest-penalty candidate was chosen as best effort.
    /// 所有候选字体均存在至少一处录入瑕疵；已按最低错码罚分选出最接近的字体。
    BestEffortWithDefects,

    /// Input contained no Mongolian characters under any candidate font (pure passthrough text).
    /// 输入文本在所有候选字体下均不含蒙古文字形（纯外文/标点透传文本）。
    NoMongolianContent,
}

/// Evaluation of one candidate font on a single text input.
/// 单个候选字体在单条输入文本上的评估结果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateEvaluation {
    /// Display label of this candidate in the pool / 字体池中的候选名称标签。
    pub label: String,
    /// Bound profile ID / 绑定的家族 Profile ID。
    pub profile_id: String,
    /// Font internal name / 字体内部名称。
    pub font_name: String,
    /// Normalized UTN #57 text under this font / 在该字体下转换出的标准 UTN #57 文本。
    pub normalized_text: String,
    /// Shaping diagnostics & defect counts / 塑形诊断与错码统计。
    pub diagnostics: Diagnostics,
}

/// Result of [`FontPool::detect_text`] on a single text input.
/// 单条文本经 [`FontPool::detect_text`] 判别后的完整结果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SingleDetectionResult {
    /// Resolution status (`Exact`, `Ambiguous`, `BestEffortWithDefects`, `NoMongolianContent`).
    /// 判别决议状态。
    pub status: DetectionStatus,
    /// Recommended UTN #57 normalized text (from the top-ranked candidate).
    /// 推荐的标准 UTN #57 转换结果（取自排名第一的候选字体）。
    pub best_text: String,
    /// Distinct clean UTN #57 outputs observed across zero-defect candidates (length > 1 when `Ambiguous`).
    /// 零错码候选字体产出的去重 UTN #57 文本列表（当 `status == Ambiguous` 时长度大于 1）。
    pub distinct_clean_outputs: Vec<String>,
    /// All evaluated candidates sorted from lowest penalty (best) to highest penalty.
    /// 按错码罚分从低到高排序的全部候选字体评估详情。
    pub candidates: Vec<CandidateEvaluation>,
}

/// Aggregated corpus-level score of one candidate font across many lines/entries.
/// 单个候选字体在整批语料/整本词典上的聚合评分报告。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusCandidateRank {
    /// Display label in the pool / 候选字体标签。
    pub label: String,
    /// Bound profile ID / 家族 Profile ID。
    pub profile_id: String,
    /// Font internal name / 字体内部名称。
    pub font_name: String,
    /// Number of corpus lines with at least one shaped Mongolian unit and zero defects.
    /// 成功产出蒙文字形且零错码的干净行数。
    pub clean_mongolian_lines: usize,
    /// Number of corpus lines where at least one defect occurred.
    /// 出现错码或连写断裂的行数。
    pub defective_lines: usize,
    /// Aggregated defect counters across the entire corpus.
    /// 整批语料累计的各项错码计数。
    pub total_diagnostics: Diagnostics,
    /// Weighted total penalty score (lower is better).
    /// 累计加权错码罚分（越低越优）。
    pub total_penalty: usize,
}

/// Corpus-level encoding identification report returned by [`FontPool::detect_corpus`].
/// 整本词典或整批语料的字体/编码自动鉴定报告（由 [`FontPool::detect_corpus`] 返回）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusDetectionReport {
    /// Total non-empty lines inspected / 参与检测的非空总行数。
    pub total_lines: usize,
    /// Label of the winning font/profile (if the pool is non-empty) / 胜出的最佳匹配字体标签。
    pub winner_label: Option<String>,
    /// Ranked list of all candidates from best match to worst match / 按匹配度从高到低排列的候选字体榜单。
    pub rankings: Vec<CorpusCandidateRank>,
}

struct PoolEntry {
    label: String,
    shaper: FontShaper,
}

/// A collection of [`FontShaper`] instances representing candidate font families
/// (e.g. `utn57`, `menk-gb2023`, `menk-gb2010`, `oyun-gb2023`, `oyun-gb2010`, `delehi`, `baiti`, `menk-pua`, `zvvnmod`, `z52`).
///
/// 多字体池（[`FontPool`]）：管理多款不同编码流派的 [`FontShaper`]，用于未知语料的错码筛查与字体归属鉴定。
#[derive(Default)]
pub struct FontPool {
    entries: Vec<PoolEntry>,
    include_latin_mapped: bool,
}

impl FontPool {
    /// Create an empty [`FontPool`].
    /// 创建一个空的 [`FontPool`]。
    pub fn new() -> Self {
        Self::default()
    }

    /// Control whether `EncodingClass::LatinMapped` fonts (Danzan / HuaGuang ASCII-mapped) participate
    /// in automatic detection (`false` by default so ordinary English words are not misread as Danzan).
    ///
    /// 设置是否允许 `LatinMapped`（占用英文 ASCII 键盘码位的 Danzan/华光等老字体）参与自动盲测（默认关闭以防误判英文）。
    pub fn set_include_latin_mapped(&mut self, include: bool) {
        self.include_latin_mapped = include;
    }

    /// Register a [`FontShaper`] under a human-readable `label`.
    /// 向字体池注册一个带名称标签的 [`FontShaper`]。
    pub fn push(&mut self, label: impl Into<String>, shaper: FontShaper) {
        self.entries.push(PoolEntry {
            label: label.into(),
            shaper,
        });
    }

    /// Number of registered shapers in this pool.
    /// 当前池中已注册的字体数量。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the pool contains no shapers.
    /// 判断字体池是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up a shaper by its `label`.
    /// 按标签名查找对应的 [`FontShaper`]。
    pub fn get(&self, label: &str) -> Option<&FontShaper> {
        self.entries
            .iter()
            .find(|e| e.label == label)
            .map(|e| &e.shaper)
    }

    /// Evaluate a single piece of text across active fonts in the pool.
    /// 对单条文本运行多字体判别（自动完成共识检查、错码剔除与分歧提示）。
    pub fn detect_text(&self, text: &str) -> Result<SingleDetectionResult, Error> {
        let mut candidates = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            if !self.include_latin_mapped
                && entry.shaper.encoding_class() == EncodingClass::LatinMapped
            {
                continue;
            }
            let out = entry.shaper.normalize_text(text)?;
            candidates.push(CandidateEvaluation {
                label: entry.label.clone(),
                profile_id: entry.shaper.profile_id().to_owned(),
                font_name: entry.shaper.font_name().to_owned(),
                normalized_text: out.value,
                diagnostics: out.diagnostics,
            });
        }

        // Sort: prefer candidates that actually shaped >= 1 Mongolian unit and have the lowest penalty.
        // 排序规则：优先选择确实产出了蒙古文字形单元且错码罚分最低的候选字体。
        candidates.sort_by(|a, b| {
            let a_has_units = a.diagnostics.shaped_unit_count > 0;
            let b_has_units = b.diagnostics.shaped_unit_count > 0;
            b_has_units
                .cmp(&a_has_units)
                .then_with(|| {
                    a.diagnostics
                        .penalty_score()
                        .cmp(&b.diagnostics.penalty_score())
                })
                .then_with(|| {
                    b.diagnostics
                        .shaped_unit_count
                        .cmp(&a.diagnostics.shaped_unit_count)
                })
        });

        let any_mongolian = candidates
            .iter()
            .any(|c| c.diagnostics.shaped_unit_count > 0);
        if !any_mongolian {
            return Ok(SingleDetectionResult {
                status: DetectionStatus::NoMongolianContent,
                best_text: text.to_owned(),
                distinct_clean_outputs: Vec::new(),
                candidates,
            });
        }

        let mut distinct_clean_outputs: Vec<String> = Vec::new();
        for cand in &candidates {
            if cand.diagnostics.shaped_unit_count > 0
                && cand.diagnostics.is_clean()
                && !distinct_clean_outputs.contains(&cand.normalized_text)
            {
                distinct_clean_outputs.push(cand.normalized_text.clone());
            }
        }

        let status = match distinct_clean_outputs.len() {
            1 => DetectionStatus::Exact,
            n if n > 1 => DetectionStatus::Ambiguous,
            _ => DetectionStatus::BestEffortWithDefects,
        };

        let best_text = candidates
            .first()
            .map(|c| c.normalized_text.clone())
            .unwrap_or_else(|| text.to_owned());

        Ok(SingleDetectionResult {
            status,
            best_text,
            distinct_clean_outputs,
            candidates,
        })
    }

    /// Identify the underlying encoding/font of an entire dictionary or multi-line corpus
    /// (such as dictionary `170` or `174`) by aggregating defect penalties across all entries.
    ///
    /// 对整本词典或多行语料（例如内蒙人民出版社词典中的 `170` / `174` 分册）进行批量投票鉴定，
    /// 汇总所有词条在每款候选字体下的干净率与错码率，一锤定音锁定整本书的真实录入字体。
    pub fn detect_corpus<'a, I>(&self, lines: I) -> Result<CorpusDetectionReport, Error>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let active: Vec<&PoolEntry> = self
            .entries
            .iter()
            .filter(|e| {
                self.include_latin_mapped
                    || e.shaper.encoding_class() != EncodingClass::LatinMapped
            })
            .collect();

        let mut ranks: Vec<CorpusCandidateRank> = active
            .iter()
            .map(|e| CorpusCandidateRank {
                label: e.label.clone(),
                profile_id: e.shaper.profile_id().to_owned(),
                font_name: e.shaper.font_name().to_owned(),
                clean_mongolian_lines: 0,
                defective_lines: 0,
                total_diagnostics: Diagnostics::default(),
                total_penalty: 0,
            })
            .collect();

        let mut total_lines = 0usize;
        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            total_lines += 1;
            for (idx, entry) in active.iter().enumerate() {
                let out = entry.shaper.evaluate_fast(trimmed)?;
                let rank = &mut ranks[idx];
                if out.diagnostics.is_clean() {
                    if out.diagnostics.shaped_unit_count > 0 {
                        rank.clean_mongolian_lines += 1;
                    }
                } else {
                    rank.defective_lines += 1;
                }
                rank.total_penalty += out.diagnostics.penalty_score();
                rank.total_diagnostics.accumulate(&out.diagnostics);
            }
        }

        // Rank by: most clean Mongolian lines + lowest penalty (preferring fonts that actually decode the corpus!).
        // 排名准则：首先要求真正识别出蒙文内容（`shaped_unit_count > 0`），其次按总错码罚分升序、干净行数降序排列。
        ranks.sort_by(|a, b| {
            let a_decoded = a.total_diagnostics.shaped_unit_count > 0;
            let b_decoded = b.total_diagnostics.shaped_unit_count > 0;
            b_decoded
                .cmp(&a_decoded)
                .then_with(|| a.total_penalty.cmp(&b.total_penalty))
                .then_with(|| b.clean_mongolian_lines.cmp(&a.clean_mongolian_lines))
        });

        let winner_label = ranks
            .first()
            .filter(|r| r.total_diagnostics.shaped_unit_count > 0)
            .map(|r| r.label.clone());

        Ok(CorpusDetectionReport {
            total_lines,
            winner_label,
            rankings: ranks,
        })
    }
}
