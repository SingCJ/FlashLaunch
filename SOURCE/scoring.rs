use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::ProcessStatus::*;
use windows_sys::Win32::System::SystemInformation::*;
use windows_sys::Win32::System::Threading::*;

use crate::*;

#[derive(Clone)]
pub(crate) struct PreparedCandidateName {
    pub(crate) title: String,
    pub(crate) folded_title: String,
    pub(crate) folded_stem: String,
}

impl PreparedCandidateName {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        Self::from_path_and_file_name(path, path.file_name())
    }

    pub(crate) fn from_path_and_file_name(
        path: &Path,
        file_name: Option<&std::ffi::OsStr>,
    ) -> Option<Self> {
        let title = file_name
            .map(|value| value.to_string_lossy().trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        if title.is_empty() {
            return None;
        }
        let folded_title = fold_text(&title);
        let folded_stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(fold_text)
            .unwrap_or_default();
        Some(Self {
            title,
            folded_title,
            folded_stem,
        })
    }
}

#[derive(Clone)]
enum CompiledPatternMatcher {
    Always,
    Exact(String),
    Extension(String),
    Prefix(String),
    Suffix(String),
    Contains(String),
    Wildcard(Vec<char>),
}

impl CompiledPatternMatcher {
    fn compile(pattern: &str) -> Self {
        if pattern == "*" {
            return Self::Always;
        }
        if !pattern.contains('?') {
            let star_count = pattern.bytes().filter(|value| *value == b'*').count();
            if star_count == 0 {
                return Self::Exact(pattern.to_string());
            }
            if star_count == 1 && pattern.ends_with('*') {
                return Self::Prefix(pattern[..pattern.len() - 1].to_string());
            }
            if star_count == 1 && pattern.starts_with("*.") {
                return Self::Extension(pattern[1..].to_string());
            }
            if star_count == 1 && pattern.starts_with('*') {
                return Self::Suffix(pattern[1..].to_string());
            }
            if star_count == 2 && pattern.starts_with('*') && pattern.ends_with('*') {
                return Self::Contains(pattern[1..pattern.len() - 1].to_string());
            }
        }
        Self::Wildcard(pattern.chars().collect())
    }

    fn matches(&self, candidate: &str) -> bool {
        match self {
            Self::Always => true,
            Self::Exact(value) => candidate == value,
            Self::Extension(value) | Self::Suffix(value) => candidate.ends_with(value),
            Self::Prefix(value) => candidate.starts_with(value),
            Self::Contains(value) => candidate.contains(value),
            Self::Wildcard(pattern) => wildcard_match_chars(pattern, candidate),
        }
    }
}

#[derive(Clone)]
struct CompiledPatternRule {
    matcher: CompiledPatternMatcher,
    full_path: bool,
    score: i32,
}

#[derive(Clone)]
pub(crate) struct PreparedQuery {
    folded_search_text: String,
    tokens: Vec<String>,
    compiled_pattern_rules: Vec<CompiledPatternRule>,
    needs_folded_path: bool,
    positive_pattern_score_ceiling: i32,
}

impl PreparedQuery {
    pub(crate) fn new(spec: &SearchQuerySpec, scoring: &ScoringConfig) -> Self {
        let folded_search_text = spec.folded_search_text.trim().to_string();
        let tokens = folded_search_text
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let any_modifier_match = any_modifier_keywords_match(
            scoring
                .pattern_rules
                .iter()
                .map(|rule| rule.modifiers.as_slice()),
            &spec.scoring_modifiers,
        );
        let compiled_pattern_rules = scoring
            .pattern_rules
            .iter()
            .filter(|rule| {
                modifier_keywords_apply(
                    &rule.modifiers,
                    &spec.scoring_modifiers,
                    any_modifier_match,
                )
            })
            .filter_map(|rule| {
                let pattern = rule.pattern.trim();
                (!pattern.is_empty()).then(|| CompiledPatternRule {
                    matcher: CompiledPatternMatcher::compile(&rule.folded_pattern),
                    full_path: pattern.contains(['\\', '/']),
                    score: rule.score,
                })
            })
            .collect::<Vec<_>>();
        let needs_folded_path = compiled_pattern_rules.iter().any(|rule| rule.full_path);
        let positive_pattern_score_ceiling = compiled_pattern_rules
            .iter()
            .filter(|rule| rule.score > 0)
            .fold(0i32, |total, rule| total.saturating_add(rule.score));
        Self {
            folded_search_text,
            tokens,
            compiled_pattern_rules,
            needs_folded_path,
            positive_pattern_score_ceiling,
        }
    }

    pub(crate) fn folded_search_text(&self) -> &str {
        &self.folded_search_text
    }

    pub(crate) fn positive_pattern_score_ceiling(&self) -> i32 {
        self.positive_pattern_score_ceiling
    }

    pub(crate) fn score_candidate_name(
        &self,
        candidate: &PreparedCandidateName,
        folded_parent: &str,
        is_dir: bool,
        scoring: &ScoringConfig,
    ) -> Option<i32> {
        let whole_query_score = score_text_numeric_with_config(
            &self.folded_search_text,
            &candidate.folded_title,
            scoring,
        );
        let token_average_score = score_query_tokens_across_fields_with_config(
            self.tokens.iter().map(String::as_str),
            &candidate.folded_title,
            folded_parent,
            scoring,
        )
        .map(|matched| matched.text_score);
        let mut score = whole_query_score.max(token_average_score)?;
        if !is_dir
            && !candidate.folded_stem.is_empty()
            && candidate.folded_stem == self.folded_search_text
            && candidate.folded_title != self.folded_search_text
        {
            score = score.saturating_add(scoring.exact_match_bonus);
        }
        Some(score)
    }

    pub(crate) fn pattern_score(&self, path: &Path, folded_file_name: &str) -> i32 {
        let folded_path = self
            .needs_folded_path
            .then(|| fold_text(&path.to_string_lossy()));
        self.compiled_pattern_rules
            .iter()
            .filter(|rule| {
                let candidate = if rule.full_path {
                    folded_path.as_deref().unwrap_or_default()
                } else {
                    folded_file_name
                };
                rule.matcher.matches(candidate)
            })
            .fold(0i64, |score, rule| {
                score.saturating_add(i64::from(rule.score))
            })
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    }
}

pub(crate) fn score_item_text_with_config<I: SearchItemView + ?Sized>(
    query: &str,
    item: &I,
    scoring: &ScoringConfig,
) -> Option<i32> {
    let candidate = item.folded_title();
    let whole_query_score = score_text_with_config(query, candidate, false, scoring);
    let token_average_score = score_query_tokens_across_fields_with_config(
        query.split_whitespace(),
        candidate,
        item.folded_parent(),
        scoring,
    )
    .map(|matched| matched.text_score);
    let mut score = whole_query_score.max(token_average_score)?;

    if !item.is_dir()
        && !item.folded_stem().is_empty()
        && item.folded_stem() == query.trim()
        && candidate != query.trim()
    {
        score = score.saturating_add(scoring.exact_match_bonus);
    }
    Some(score)
}

#[derive(Clone, Copy)]
struct QueryTokenMatch {
    text_score: i32,
    matched_parent: bool,
}

fn score_query_tokens_across_fields_with_config<'a>(
    tokens: impl IntoIterator<Item = &'a str>,
    candidate: &str,
    parent: &str,
    scoring: &ScoringConfig,
) -> Option<QueryTokenMatch> {
    let mut tokens = tokens.into_iter();
    let first_token = tokens.next()?;
    let second_token = tokens.next()?;

    let mut title_total = 0i32;
    let mut title_count = 0i32;
    let mut matched_parent = false;
    for token in [first_token, second_token].into_iter().chain(tokens) {
        let title_score = score_text_numeric_with_config(token, candidate, scoring);
        let parent_matches = !parent.is_empty() && parent.contains(token);
        matched_parent |= parent_matches;
        if let Some(token_score) = title_score {
            title_total = title_total.saturating_add(token_score);
            title_count += 1;
        } else if !parent_matches {
            return None;
        }
    }
    if title_count == 0 {
        return None;
    }

    let text_score = title_total.saturating_div(title_count);
    (text_score > 0).then_some(QueryTokenMatch {
        text_score,
        matched_parent,
    })
}

pub(crate) fn score_item_path_with_config<I: SearchItemView + ?Sized>(
    query: &str,
    item: &I,
    scoring: &ScoringConfig,
) -> i32 {
    let query = query.trim();
    if query.is_empty() {
        return 0;
    }
    if item.folded_parent().is_empty() {
        return 0;
    }

    let parent = item.folded_parent();
    let mut tokens = query.split_whitespace();
    let first = tokens.next();
    let has_multiple_tokens = first.is_some() && tokens.clone().next().is_some();
    let all_tokens_match_parent = has_multiple_tokens
        && first
            .into_iter()
            .chain(tokens)
            .all(|token| parent.contains(token));
    let mixed_match_names_parent = score_query_tokens_across_fields_with_config(
        query.split_whitespace(),
        item.folded_title(),
        parent,
        scoring,
    )
    .is_some_and(|matched| matched.matched_parent);
    if parent.contains(query) || all_tokens_match_parent || mixed_match_names_parent {
        scoring.explicit_folder_name_match_adjustment
    } else {
        0
    }
}

#[cfg(test)]
pub(crate) fn score_text(query: &str, candidate: &str, is_title: bool) -> Option<i32> {
    score_text_with_config(query, candidate, is_title, &ScoringConfig::default())
}

pub(crate) fn score_text_with_config(
    query: &str,
    candidate: &str,
    _is_title: bool,
    scoring: &ScoringConfig,
) -> Option<i32> {
    let query = query.trim();
    if query.is_empty() {
        return Some(1);
    }
    if candidate.is_empty() {
        return None;
    }

    let mut score = fuzzy_score(query, candidate, scoring)?;
    if candidate == query {
        score = score.saturating_add(scoring.exact_match_bonus);
    }
    if exact_word_match(candidate, query) {
        score = score.saturating_add(scoring.exact_word_bonus);
    }
    if candidate.starts_with(query) {
        score = score.saturating_add(scoring.prefix_match_bonus);
    }
    if word_boundary_match(candidate, query) {
        score = score.saturating_add(scoring.word_boundary_bonus);
    }
    if acronym_starts_with(candidate, query) {
        score = score.saturating_add(scoring.acronym_match_bonus);
    }
    if candidate.contains(query) {
        score = score.saturating_add(scoring.consecutive_match_bonus);
    }
    (score > 0).then_some(score)
}

pub(crate) fn word_boundary_match(candidate: &str, query: &str) -> bool {
    word_starts(candidate).any(|(_, rest)| rest.starts_with(query))
}

pub(crate) fn exact_word_match(candidate: &str, query: &str) -> bool {
    word_starts(candidate).any(|(_, rest)| {
        rest.strip_prefix(query)
            .is_some_and(|suffix| suffix.chars().next().is_none_or(|ch| !ch.is_alphanumeric()))
    })
}

fn word_starts(candidate: &str) -> impl Iterator<Item = (usize, &str)> {
    candidate.char_indices().filter_map(|(index, ch)| {
        let is_start = index == 0
            || candidate[..index]
                .chars()
                .next_back()
                .is_some_and(|previous| !previous.is_alphanumeric());
        (is_start && ch.is_alphanumeric()).then_some((index, &candidate[index..]))
    })
}

pub(crate) fn acronym_starts_with(candidate: &str, query: &str) -> bool {
    let mut query_chars = query.chars();
    let Some(mut expected) = query_chars.next() else {
        return true;
    };
    for acronym_char in word_starts(candidate).filter_map(|(_, rest)| rest.chars().next()) {
        if acronym_char != expected {
            return false;
        }
        let Some(next_expected) = query_chars.next() else {
            return true;
        };
        expected = next_expected;
    }
    false
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FuzzyScoreParts {
    pub(crate) positions: Vec<usize>,
    pub(crate) first_character_index: i32,
    pub(crate) adjacent_pairs: i32,
    pub(crate) query_length: i32,
    pub(crate) candidate_length: i32,
    pub(crate) length_score: i32,
    pub(crate) leftmost_score: i32,
    pub(crate) compact_score: i32,
    pub(crate) total: i32,
}

pub(crate) fn fuzzy_score(query: &str, candidate: &str, scoring: &ScoringConfig) -> Option<i32> {
    fuzzy_score_numeric(query, candidate, scoring)
}

fn score_text_numeric_with_config(
    query: &str,
    candidate: &str,
    scoring: &ScoringConfig,
) -> Option<i32> {
    let query = query.trim();
    if query.is_empty() {
        return Some(1);
    }
    if candidate.is_empty() {
        return None;
    }

    let mut score = fuzzy_score_numeric(query, candidate, scoring)?;
    if candidate == query {
        score = score.saturating_add(scoring.exact_match_bonus);
    }
    if exact_word_match(candidate, query) {
        score = score.saturating_add(scoring.exact_word_bonus);
    }
    if candidate.starts_with(query) {
        score = score.saturating_add(scoring.prefix_match_bonus);
    }
    if word_boundary_match(candidate, query) {
        score = score.saturating_add(scoring.word_boundary_bonus);
    }
    if acronym_starts_with(candidate, query) {
        score = score.saturating_add(scoring.acronym_match_bonus);
    }
    if candidate.contains(query) {
        score = score.saturating_add(scoring.consecutive_match_bonus);
    }
    (score > 0).then_some(score)
}

pub(crate) fn fuzzy_score_numeric(
    query: &str,
    candidate: &str,
    scoring: &ScoringConfig,
) -> Option<i32> {
    if query.is_ascii() && candidate.is_ascii() {
        return fuzzy_score_ascii(query.as_bytes(), candidate.as_bytes(), scoring);
    }
    fuzzy_score_unicode(query, candidate, scoring)
}

fn fuzzy_total(
    first_character_index: i32,
    adjacent_pairs: i32,
    query_length: i32,
    candidate_length: i32,
    scoring: &ScoringConfig,
) -> i32 {
    let length_score = query_length
        .saturating_mul(scoring.length_score_weight.max(0))
        .saturating_div(candidate_length.max(1));
    let leftmost_score = scoring
        .leftmost_match_bonus
        .saturating_sub(first_character_index.saturating_mul(scoring.leftmost_distance_penalty))
        .max(0);
    let compact_score = if query_length > 1 {
        adjacent_pairs
            .saturating_mul(scoring.compact_match_bonus.max(0))
            .saturating_div(query_length - 1)
    } else {
        scoring.compact_match_bonus.max(0)
    };
    length_score
        .saturating_add(leftmost_score)
        .saturating_add(compact_score)
}

fn fuzzy_score_ascii(query: &[u8], candidate: &[u8], scoring: &ScoringConfig) -> Option<i32> {
    let first_query = *query.first()?;
    let query_length = query.len().max(1) as i32;
    let candidate_length = candidate.len().max(1) as i32;
    let mut best = None;
    for (start_index, candidate_char) in candidate.iter().copied().enumerate() {
        if candidate_char != first_query {
            continue;
        }
        let mut next_candidate_index = start_index + 1;
        let mut previous_index = start_index;
        let mut adjacent_pairs = 0i32;
        let mut matched = true;
        for query_char in query.iter().copied().skip(1) {
            let Some(relative_index) = candidate[next_candidate_index..]
                .iter()
                .position(|candidate_char| *candidate_char == query_char)
            else {
                matched = false;
                break;
            };
            let matched_index = next_candidate_index + relative_index;
            if matched_index == previous_index + 1 {
                adjacent_pairs += 1;
            }
            previous_index = matched_index;
            next_candidate_index = matched_index + 1;
        }
        if matched {
            let total = fuzzy_total(
                start_index as i32,
                adjacent_pairs,
                query_length,
                candidate_length,
                scoring,
            );
            if best.is_none_or(|current| total > current) {
                best = Some(total);
            }
        }
    }
    best
}

fn fuzzy_score_unicode(query: &str, candidate: &str, scoring: &ScoringConfig) -> Option<i32> {
    let first_query = query.chars().next()?;
    let query_length = query.chars().count().max(1) as i32;
    let candidate_length = candidate.chars().count().max(1) as i32;
    let mut best = None;
    for (start_character_index, (start_byte, candidate_char)) in
        candidate.char_indices().enumerate()
    {
        if candidate_char != first_query {
            continue;
        }
        let mut next_byte = start_byte + candidate_char.len_utf8();
        let mut previous_character_index = start_character_index;
        let mut next_character_index = start_character_index + 1;
        let mut adjacent_pairs = 0i32;
        let mut matched = true;
        for query_char in query.chars().skip(1) {
            let mut found = None;
            for (relative_character_index, (relative_byte, candidate_char)) in
                candidate[next_byte..].char_indices().enumerate()
            {
                if candidate_char == query_char {
                    found = Some((
                        next_byte + relative_byte,
                        next_character_index + relative_character_index,
                        candidate_char.len_utf8(),
                    ));
                    break;
                }
            }
            let Some((matched_byte, matched_character_index, matched_len)) = found else {
                matched = false;
                break;
            };
            if matched_character_index == previous_character_index + 1 {
                adjacent_pairs += 1;
            }
            previous_character_index = matched_character_index;
            next_character_index = matched_character_index + 1;
            next_byte = matched_byte + matched_len;
        }
        if matched {
            let total = fuzzy_total(
                start_character_index as i32,
                adjacent_pairs,
                query_length,
                candidate_length,
                scoring,
            );
            if best.is_none_or(|current| total > current) {
                best = Some(total);
            }
        }
    }
    best
}

pub(crate) fn fuzzy_score_parts(
    query: &str,
    candidate: &str,
    scoring: &ScoringConfig,
) -> Option<FuzzyScoreParts> {
    let query_chars: Vec<char> = query.chars().collect();
    let candidate_chars: Vec<(usize, char)> = candidate.char_indices().collect();
    let first_query_char = *query_chars.first()?;
    let mut positions = Vec::with_capacity(query_chars.len());
    let mut best: Option<FuzzyScoreParts> = None;

    for (start_index, (start_position, candidate_char)) in candidate_chars.iter().enumerate() {
        if *candidate_char != first_query_char {
            continue;
        }
        positions.clear();
        positions.push(*start_position);
        let mut next_candidate_index = start_index + 1;
        for query_char in query_chars.iter().skip(1) {
            let Some((relative_index, (position, _))) = candidate_chars
                .iter()
                .enumerate()
                .skip(next_candidate_index)
                .find(|(_, (_, candidate_char))| candidate_char == query_char)
            else {
                positions.clear();
                break;
            };
            positions.push(*position);
            next_candidate_index = relative_index + 1;
        }
        if positions.len() == query_chars.len() {
            let candidate_parts = fuzzy_positions_score_parts(candidate, &positions, scoring);
            if best
                .as_ref()
                .is_none_or(|current| candidate_parts.total > current.total)
            {
                best = Some(candidate_parts);
            }
        }
    }
    best
}

fn fuzzy_positions_score_parts(
    candidate: &str,
    positions: &[usize],
    scoring: &ScoringConfig,
) -> FuzzyScoreParts {
    let candidate_length = candidate.chars().count().max(1) as i32;
    let query_length = positions.len().max(1) as i32;
    let first_character_index = candidate[..positions[0]].chars().count() as i32;
    let adjacent_pairs = positions
        .windows(2)
        .filter(|pair| {
            let left = candidate[..pair[0]].chars().count();
            let right = candidate[..pair[1]].chars().count();
            left + 1 == right
        })
        .count() as i32;
    let length_score = query_length
        .saturating_mul(scoring.length_score_weight.max(0))
        .saturating_div(candidate_length);
    let leftmost_score = scoring
        .leftmost_match_bonus
        .saturating_sub(first_character_index.saturating_mul(scoring.leftmost_distance_penalty))
        .max(0);
    let compact_score = if query_length > 1 {
        adjacent_pairs
            .saturating_mul(scoring.compact_match_bonus.max(0))
            .saturating_div(query_length - 1)
    } else {
        scoring.compact_match_bonus.max(0)
    };
    let total = length_score
        .saturating_add(leftmost_score)
        .saturating_add(compact_score);
    FuzzyScoreParts {
        positions: positions.to_vec(),
        first_character_index,
        adjacent_pairs,
        query_length,
        candidate_length,
        length_score,
        leftmost_score,
        compact_score,
        total,
    }
}

pub(crate) fn folder_score_adjustment<I: SearchItemView + ?Sized>(
    item: &I,
    file_score: i32,
    scoring: &ScoringConfig,
) -> i32 {
    if !item.is_dir() {
        return 0;
    }

    let folder_score = i64::from(file_score)
        .saturating_mul(i64::from(scoring.folder_score_as_file_score_percent.max(0)))
        .saturating_div(100)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    folder_score.saturating_sub(file_score)
}

pub(crate) fn path_penalty<I: SearchItemView + ?Sized>(item: &I, scoring: &ScoringConfig) -> i32 {
    i64::try_from(item.relative_depth())
        .unwrap_or(i64::MAX)
        .saturating_mul(i64::from(scoring.path_depth_penalty.max(0)))
        .clamp(0, i64::from(i32::MAX)) as i32
}

#[cfg(test)]
pub(crate) fn pattern_score_with_config(path: &Path, scoring: &ScoringConfig) -> i32 {
    pattern_score_with_config_and_modifiers(path, scoring, &[])
}

pub(crate) fn pattern_score_with_config_and_modifiers(
    path: &Path,
    scoring: &ScoringConfig,
    scoring_modifiers: &[String],
) -> i32 {
    let folded_path = fold_text(&path.to_string_lossy());
    let mut score = 0i64;
    let any_modifier_match = any_modifier_keywords_match(
        scoring
            .pattern_rules
            .iter()
            .map(|rule| rule.modifiers.as_slice()),
        scoring_modifiers,
    );

    for rule in &scoring.pattern_rules {
        if modifier_keywords_apply(&rule.modifiers, scoring_modifiers, any_modifier_match)
            && wildcard_rule_matches(rule, path, &folded_path)
        {
            score = score.saturating_add(i64::from(rule.score));
        }
    }

    score.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

pub(crate) fn any_modifier_keywords_match<'a>(
    keyword_sets: impl IntoIterator<Item = &'a [String]>,
    query_modifiers: &[String],
) -> bool {
    keyword_sets
        .into_iter()
        .any(|keywords| modifier_keywords_match(keywords, query_modifiers))
}

pub(crate) fn modifier_keywords_apply(
    keywords: &[String],
    query_modifiers: &[String],
    any_modifier_match: bool,
) -> bool {
    if keywords.is_empty() {
        return query_modifiers.is_empty() || !any_modifier_match;
    }
    modifier_keywords_match(keywords, query_modifiers)
}

pub(crate) fn modifier_keywords_match(keywords: &[String], query_modifiers: &[String]) -> bool {
    if keywords.is_empty() {
        return false;
    }
    if keywords
        .iter()
        .filter_map(|keyword| keyword.strip_prefix('-'))
        .any(|rejected| query_modifiers.iter().any(|modifier| modifier == rejected))
    {
        return false;
    }
    let has_blank = keywords.iter().any(|keyword| keyword == "[blank]");
    if query_modifiers.is_empty() {
        return has_blank;
    }
    keywords.iter().any(|keyword| {
        keyword == "*"
            || (!keyword.starts_with('-')
                && keyword != "[blank]"
                && query_modifiers.iter().any(|modifier| modifier == keyword))
    })
}

pub(crate) fn unix_seconds_from_system_time(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).ok(),
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs()).ok()?.checked_neg()?;
            if duration.subsec_nanos() == 0 {
                Some(seconds)
            } else {
                seconds.checked_sub(1)
            }
        }
    }
}

pub(crate) fn current_unix_seconds() -> i64 {
    unix_seconds_from_system_time(SystemTime::now()).unwrap_or_default()
}

pub(crate) fn modified_at_unix_seconds(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(unix_seconds_from_system_time)
}

fn modified_at_for_scoring(
    scoring: &ScoringConfig,
    load: impl FnOnce() -> Option<i64>,
) -> Option<i64> {
    if scoring.recency_date_enabled {
        load()
    } else {
        None
    }
}

pub(crate) fn recency_date_score_at(
    modified_at_unix_seconds: Option<i64>,
    now_unix_seconds: i64,
    scoring: &ScoringConfig,
) -> i32 {
    if !scoring.recency_date_enabled || scoring.recency_date_bonus <= 0 {
        return 0;
    }
    let Some(modified_at_unix_seconds) = modified_at_unix_seconds else {
        return 0;
    };
    if modified_at_unix_seconds > now_unix_seconds {
        return scoring.recency_date_bonus;
    }
    let age_seconds = now_unix_seconds.saturating_sub(modified_at_unix_seconds) as u64;
    let days = age_seconds / 86_400;
    if days == 0 {
        scoring.recency_date_bonus
    } else if days < 30 {
        scoring.recency_date_bonus / 2
    } else if days < 365 {
        scoring.recency_date_bonus / 4
    } else {
        0
    }
}

#[cfg(test)]
pub(crate) fn push_item(
    path: PathBuf,
    root: &IndexRoot,
    seen: &mut HashSet<String>,
    items: &mut Vec<LaunchItem>,
) {
    push_item_with_config(path, root, &ScoringConfig::default(), seen, items)
}

#[cfg(test)]
pub(crate) fn push_item_with_config(
    path: PathBuf,
    root: &IndexRoot,
    scoring: &ScoringConfig,
    seen: &mut HashSet<String>,
    items: &mut Vec<LaunchItem>,
) {
    let key = fold_text(&path.to_string_lossy());
    if !seen.insert(key) {
        return;
    }

    if let Some(item) = launch_item_from_path(path, root, scoring) {
        items.push(item);
    }
}

pub(crate) fn launch_item_from_path(
    path: PathBuf,
    root: &IndexRoot,
    scoring: &ScoringConfig,
) -> Option<LaunchItem> {
    let is_dir = path.is_dir();
    launch_item_from_path_with_type(path, is_dir, root, scoring)
}

pub(crate) fn launch_item_from_path_with_type(
    path: PathBuf,
    is_dir: bool,
    root: &IndexRoot,
    scoring: &ScoringConfig,
) -> Option<LaunchItem> {
    let prepared_name = PreparedCandidateName::from_path(&path)?;
    let subtitle = indexed_parent_display(&path, root);
    let folded_parent = path
        .parent()
        .map(|parent| searchable_text(&parent.to_string_lossy()))
        .unwrap_or_default();
    let root_path = root.path.as_deref();
    let relative_depth = root_path
        .and_then(|root_path| {
            path_key_relative_depth(
                &normalized_root_path_key(&path),
                &normalized_root_path_key(root_path),
            )
        })
        .map(|depth| depth.saturating_sub(usize::from(!is_dir)))
        .unwrap_or(0);
    Some(launch_item_from_prepared_candidate(
        path,
        is_dir,
        root,
        scoring,
        prepared_name,
        subtitle,
        folded_parent,
        relative_depth,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn launch_item_from_prepared_candidate(
    path: PathBuf,
    is_dir: bool,
    root: &IndexRoot,
    scoring: &ScoringConfig,
    prepared_name: PreparedCandidateName,
    subtitle: String,
    folded_parent: String,
    relative_depth: usize,
) -> LaunchItem {
    let modified_at_unix_seconds =
        modified_at_for_scoring(scoring, || modified_at_unix_seconds(&path));
    let item = LaunchItem {
        title: prepared_name.title,
        subtitle,
        path,
        is_dir,
        folded_title: prepared_name.folded_title,
        folded_stem: prepared_name.folded_stem,
        folded_parent,
        index_score: root.score,
        modified_at_unix_seconds,
        search_root: None,
        relative_depth: 0,
    };
    match root.path.as_deref() {
        Some(root_path) => item.with_search_root(root_path.to_path_buf(), relative_depth),
        None => item,
    }
}

pub(crate) fn indexed_parent_display(path: &Path, root: &IndexRoot) -> String {
    let item_parent = path.parent().unwrap_or(path);
    let Some(root_path) = root.path.as_ref() else {
        return item_parent.to_string_lossy().to_string();
    };
    let root_name = root_path
        .file_name()
        .map(|value| {
            value
                .to_string_lossy()
                .trim_matches(['\\', '/'])
                .to_string()
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| root.raw.trim_matches(['\\', '/']).to_string());

    if let Ok(relative) = item_parent.strip_prefix(root_path) {
        let relative = relative.to_string_lossy();
        let relative = relative.trim_matches(['\\', '/']);
        if relative.is_empty() {
            format!("{root_name}\\")
        } else {
            format!("{root_name}\\{relative}\\")
        }
    } else {
        item_parent.to_string_lossy().to_string()
    }
}

pub(crate) fn is_reparse_point(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_VALUE != 0)
        .unwrap_or(false)
}

#[cfg(test)]
pub(crate) fn is_allowed_by_pattern_scoring(path: &Path, scoring: &ScoringConfig) -> bool {
    let folded_path = fold_text(&path.to_string_lossy());
    scoring
        .pattern_rules
        .iter()
        .any(|rule| wildcard_rule_matches(rule, path, &folded_path))
}

pub(crate) fn load_index_roots() -> Vec<IndexRoot> {
    read_configured_index_roots()
}

pub(crate) fn read_configured_index_roots() -> Vec<IndexRoot> {
    ensure_index_file(None);
    read_required_text("index_folders.txt", &index_path())
        .lines()
        .filter_map(parse_hydrated_index_root_line)
        .collect()
}

fn parse_hydrated_index_root_line(line: &str) -> Option<IndexRoot> {
    let root = parse_index_root_line(line)?;
    let parts = split_index_root_line(line);
    let mut has_enabled = false;
    let mut has_score = false;
    let mut has_depth = false;
    for part in parts.iter().skip(1) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim() {
            "enabled" if matches!(value.trim(), "0" | "1") => has_enabled = true,
            "score" if value.trim().parse::<i32>().is_ok() => has_score = true,
            "depth" if value.trim().parse::<isize>().is_ok() => has_depth = true,
            _ => {}
        }
    }
    (has_enabled && has_score && has_depth).then_some(root)
}

pub(crate) fn merge_index_roots_text(existing: &str, roots: &[IndexRoot]) -> String {
    let mut opaque_fields = HashMap::<String, Vec<String>>::new();
    for line in existing.lines() {
        let Some(root) = parse_index_root_line(line) else {
            continue;
        };
        let extras = split_index_root_line(line)
            .into_iter()
            .skip(1)
            .filter(|part| {
                let Some((key, value)) = part.split_once('=') else {
                    return true;
                };
                match key.trim() {
                    "enabled" => !matches!(value.trim(), "0" | "1"),
                    "score" => value.trim().parse::<i32>().is_err(),
                    "depth" => value.trim().parse::<isize>().is_err(),
                    "keywords" => false,
                    _ => true,
                }
            })
            .collect::<Vec<_>>();
        if !extras.is_empty() {
            opaque_fields
                .entry(fold_text(root.raw.trim()))
                .or_default()
                .extend(extras);
        }
    }
    let replacement = roots
        .iter()
        .map(|root| {
            let mut line = index_root_line(root);
            if let Some(extras) = opaque_fields.get(&fold_text(root.raw.trim())) {
                for extra in extras {
                    line.push_str(" | ");
                    line.push_str(extra);
                }
            }
            line
        })
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut inserted = false;
    for line in existing.lines() {
        if parse_hydrated_index_root_line(line).is_some() {
            if !inserted {
                output.extend(replacement.iter().cloned());
                inserted = true;
            }
        } else {
            output.push(line.to_string());
        }
    }
    if !inserted {
        output.extend(replacement);
    }
    let newline = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut content = output.join(newline);
    if existing.ends_with(['\r', '\n']) {
        content.push_str(newline);
    }
    content
}

pub(crate) fn index_root_line(root: &IndexRoot) -> String {
    let keywords = if !root.keywords.is_empty() {
        format!(" | keywords={}", root.keywords.join(","))
    } else {
        String::new()
    };
    format!(
        "{} | enabled={} | score={} | depth={}",
        escape_index_root_field(&root.raw),
        if root.enabled { 1 } else { 0 },
        root.score,
        depth_display(root.max_depth)
    ) + &keywords
}

pub(crate) fn escape_index_root_field(value: &str) -> String {
    value.replace('\\', "\\\\").replace('|', "\\|")
}

pub(crate) fn split_index_root_line(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek().copied() {
                Some('|') | Some('\\') => {
                    current.push(chars.next().unwrap());
                }
                _ => current.push(ch),
            }
        } else if ch == '|' {
            parts.push(current.trim().to_string());
            current.clear();
        } else {
            current.push(ch);
        }
    }
    parts.push(current.trim().to_string());
    parts
}

pub(crate) fn normalize_search_depth(value: isize) -> usize {
    if value < 0 {
        SEARCH_DEPTH_ALL
    } else {
        value as usize
    }
}

pub(crate) fn depth_display(value: usize) -> String {
    if value == SEARCH_DEPTH_ALL {
        "-1".to_string()
    } else {
        value.to_string()
    }
}

pub(crate) fn normalized_index_root(mut root: IndexRoot) -> IndexRoot {
    let path = expand_path(&root.raw);
    root.path = Some(path);
    root
}

pub(crate) fn parse_index_root_line(line: &str) -> Option<IndexRoot> {
    let line = line.trim().trim_start_matches('\u{feff}').trim_start();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let parts = split_index_root_line(line);
    let mut parts = parts.iter().map(String::as_str);
    let raw_path = parts.next()?.trim_matches('"').to_string();
    if raw_path.is_empty() {
        return None;
    }

    let mut score = 100;
    let mut max_depth = DEFAULT_SEARCH_DEPTH;
    let mut enabled = true;
    let mut keywords = Vec::new();

    for part in parts {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim() {
            "enabled" if matches!(value.trim(), "0" | "1") => enabled = value.trim() == "1",
            "score" => score = value.trim().parse().unwrap_or(score),
            "depth" => {
                max_depth = value
                    .trim()
                    .parse::<isize>()
                    .ok()
                    .map(normalize_search_depth)
                    .unwrap_or(max_depth)
            }
            "keywords" => keywords = normalize_keywords(value),
            _ => {}
        }
    }

    let path = expand_path(&raw_path);
    Some(IndexRoot {
        raw: raw_path,
        path: Some(path),
        enabled,
        score,
        max_depth,
        label: String::new(),
        keywords,
    })
}

pub(crate) fn normalize_keywords(value: &str) -> Vec<String> {
    let mut keywords = Vec::new();
    for part in value.split([',', ';', '|', ' ']) {
        let folded = fold_text(part.trim().trim_start_matches('+'))
            .trim()
            .to_string();
        if !folded.is_empty() && !keywords.contains(&folded) {
            keywords.push(folded);
        }
    }
    keywords
}

pub(crate) fn search_folder_modifier_display(keywords: &[String]) -> String {
    if keywords.is_empty() {
        String::new()
    } else {
        keywords
            .iter()
            .map(|keyword| modifier_keyword_display(keyword))
            .collect::<Vec<_>>()
            .join(",")
    }
}

pub(crate) fn modifier_keyword_display(keyword: &str) -> String {
    if keyword.eq_ignore_ascii_case("[blank]") {
        "[Blank]".to_string()
    } else {
        keyword.to_string()
    }
}

pub(crate) fn expand_path(raw: &str) -> PathBuf {
    let mut value = raw.to_string();
    for alias in path_aliases() {
        if let Some(replacement) = alias_path(alias) {
            value = replace_ignore_ascii_case(&value, alias.name, &replacement.to_string_lossy());
        }
    }
    PathBuf::from(value)
}

pub(crate) fn path_aliases() -> &'static [PathAlias] {
    &[
        PathAlias {
            name: "%USERPROFILE%",
            env_key: Some("USERPROFILE"),
            suffix: "",
        },
        PathAlias {
            name: "%APPDATA%",
            env_key: Some("APPDATA"),
            suffix: "",
        },
        PathAlias {
            name: "%PROGRAMDATA%",
            env_key: Some("PROGRAMDATA"),
            suffix: "",
        },
        PathAlias {
            name: "%PUBLIC%",
            env_key: Some("PUBLIC"),
            suffix: "",
        },
        PathAlias {
            name: "%PROGRAMFILES%",
            env_key: Some("ProgramFiles"),
            suffix: "",
        },
        PathAlias {
            name: "%PROGRAMFILES86%",
            env_key: Some("ProgramFiles(x86)"),
            suffix: "",
        },
        PathAlias {
            name: "%WINDIR%",
            env_key: Some("WINDIR"),
            suffix: "",
        },
        PathAlias {
            name: "%MYDOCUMENTS%",
            env_key: Some("USERPROFILE"),
            suffix: "Documents",
        },
        PathAlias {
            name: "%MYDESKTOP%",
            env_key: Some("USERPROFILE"),
            suffix: "Desktop",
        },
        PathAlias {
            name: "%ALLDESKTOP%",
            env_key: Some("PUBLIC"),
            suffix: "Desktop",
        },
        PathAlias {
            name: "%MYSTARTMENU%",
            env_key: Some("APPDATA"),
            suffix: "Microsoft\\Windows\\Start Menu\\Programs",
        },
        PathAlias {
            name: "%COMMONSTARTMENU%",
            env_key: Some("PROGRAMDATA"),
            suffix: "Microsoft\\Windows\\Start Menu\\Programs",
        },
        PathAlias {
            name: "%MYRECENTDOCS%",
            env_key: Some("APPDATA"),
            suffix: "Microsoft\\Windows\\Recent",
        },
        PathAlias {
            name: "%MYFAVORITES%",
            env_key: Some("USERPROFILE"),
            suffix: "Favorites",
        },
        PathAlias {
            name: "%MYPICTURES%",
            env_key: Some("USERPROFILE"),
            suffix: "Pictures",
        },
        PathAlias {
            name: "%MYMUSIC%",
            env_key: Some("USERPROFILE"),
            suffix: "Music",
        },
        PathAlias {
            name: "%MYVIDEO%",
            env_key: Some("USERPROFILE"),
            suffix: "Videos",
        },
        PathAlias {
            name: "%COMMONMUSIC%",
            env_key: Some("PUBLIC"),
            suffix: "Music",
        },
        PathAlias {
            name: "%COMMONPICTURES%",
            env_key: Some("PUBLIC"),
            suffix: "Pictures",
        },
        PathAlias {
            name: "%COMMONVIDEO%",
            env_key: Some("PUBLIC"),
            suffix: "Videos",
        },
        PathAlias {
            name: "%launcherDIR%",
            env_key: None,
            suffix: "",
        },
        PathAlias {
            name: "%CONFIGDIR%",
            env_key: None,
            suffix: "CONFIG",
        },
    ]
}

pub(crate) fn alias_path(alias: &PathAlias) -> Option<PathBuf> {
    alias_path_with_env(alias, |key| env::var_os(key))
}

fn alias_path_with_env(
    alias: &PathAlias,
    get_env: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let base = if alias.name.eq_ignore_ascii_case("%PROGRAMFILES%") {
        get_env("ProgramW6432")
            .or_else(|| get_env("ProgramFiles"))
            .map(PathBuf::from)
    } else if alias.name.eq_ignore_ascii_case("%PROGRAMFILES86%") {
        get_env("ProgramFiles(x86)").map(PathBuf::from).or_else(|| {
            let program_files = PathBuf::from(get_env("ProgramFiles")?);
            program_files
                .parent()
                .map(|parent| parent.join("Program Files (x86)"))
        })
    } else if let Some(env_key) = alias.env_key {
        get_env(env_key).map(PathBuf::from)
    } else {
        Some(app_dir())
    }?;
    if alias.suffix.is_empty() {
        Some(base)
    } else {
        Some(base.join(alias.suffix))
    }
}

pub(crate) fn alias_display_text(alias: &PathAlias) -> String {
    match alias_path(alias) {
        Some(path) => format!("{}  ->  {}", alias.name, path.to_string_lossy()),
        None => alias.name.to_string(),
    }
}

pub(crate) fn alias_name_from_combo_text(value: &str) -> String {
    if let Some((alias, _)) = value.split_once("->") {
        let alias = alias.trim();
        if path_aliases()
            .iter()
            .any(|known_alias| known_alias.name.eq_ignore_ascii_case(alias))
        {
            return alias.to_string();
        }
    }
    value.trim().to_string()
}

pub(crate) fn replace_ignore_ascii_case(value: &str, pattern: &str, replacement: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let lower_value = value.to_ascii_lowercase();
    let lower_pattern = pattern.to_ascii_lowercase();
    let mut index = 0;
    while let Some(relative) = lower_value[index..].find(&lower_pattern) {
        let start = index + relative;
        output.push_str(&value[index..start]);
        output.push_str(replacement);
        index = start + pattern.len();
    }
    output.push_str(&value[index..]);
    output
}

#[cfg(test)]
thread_local! {
    static RECENT_ITEM_KEY_CALL_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_recent_item_key_call_count() {
    RECENT_ITEM_KEY_CALL_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn recent_item_key_call_count() -> usize {
    RECENT_ITEM_KEY_CALL_COUNT.with(std::cell::Cell::get)
}

pub(crate) fn recent_item_key(path: &Path) -> String {
    #[cfg(test)]
    RECENT_ITEM_KEY_CALL_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    fold_text(&path.to_string_lossy())
}

pub(crate) fn load_recent_items() -> Vec<String> {
    let path = recent_items_path();
    let Some(content) = ensure_optional_text_file("recent_items.txt", &path, "", |content, _| {
        content.to_string()
    }) else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(parse_recent_config_entry)
        .filter(|entry| entry.enabled)
        .map(|entry| {
            let key = fold_text(&entry.path);
            (entry.score, key, entry.path)
        })
        .filter(|(_, key, _)| seen.insert(key.clone()))
        .map(|(score, _, path)| {
            recent_config_entry_line(&RecentConfigEntry {
                enabled: true,
                score,
                path,
            })
        })
        .take(RECENT_LIMIT)
        .collect()
}

pub(crate) fn save_recent_items(items: &[String]) -> std::io::Result<()> {
    let path = recent_items_path();
    let Some(existing) = ensure_optional_text_file("recent_items.txt", &path, "", |content, _| {
        content.to_string()
    }) else {
        return Ok(());
    };
    let content = merge_recent_items_text(&existing, items);
    write_optional_text("recent_items.txt", &path, &content);
    Ok(())
}

pub(crate) fn recent_items_to_text(items: &[String]) -> String {
    let mut content = items.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    content
}

pub(crate) fn load_recent_config_entries() -> Vec<RecentConfigEntry> {
    let path = recent_items_path();
    let Some(content) = ensure_optional_text_file("recent_items.txt", &path, "", |content, _| {
        content.to_string()
    }) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(parse_recent_config_entry)
        .filter(|entry| seen.insert(fold_text(&entry.path)))
        .take(RECENT_LIMIT)
        .collect()
}

pub(crate) fn recent_config_entries_to_text(entries: &[RecentConfigEntry]) -> String {
    let lines: Vec<String> = entries.iter().map(recent_config_entry_line).collect();
    recent_items_to_text(&lines)
}

fn merge_recent_document(existing: &str, replacement: &str) -> String {
    let replacement_lines = replacement.lines().map(str::to_string).collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut inserted = false;
    for line in existing.lines() {
        if parse_recent_config_entry(line).is_some() {
            if !inserted {
                output.extend(replacement_lines.iter().cloned());
                inserted = true;
            }
        } else {
            output.push(line.to_string());
        }
    }
    if !inserted {
        output.extend(replacement_lines);
    }
    let newline = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut content = output.join(newline);
    if existing.ends_with(['\r', '\n']) {
        content.push_str(newline);
    }
    content
}

pub(crate) fn merge_recent_items_text(existing: &str, items: &[String]) -> String {
    merge_recent_document(existing, &recent_items_to_text(items))
}

pub(crate) fn merge_recent_config_entries_text(
    existing: &str,
    entries: &[RecentConfigEntry],
) -> String {
    merge_recent_document(existing, &recent_config_entries_to_text(entries))
}

pub(crate) fn recent_score_text(score: f32) -> String {
    if score.fract().abs() < f32::EPSILON {
        (score as i32).to_string()
    } else {
        format!("{score:.3}")
    }
}

pub(crate) fn parse_recent_config_entry(entry: &str) -> Option<RecentConfigEntry> {
    let entry = entry.trim();
    if entry.is_empty() || entry.starts_with('#') || entry.starts_with(';') {
        return None;
    }
    if let Some((score, path)) = entry.split_once(">>>") {
        let score = score
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|score| score.is_finite())?;
        let path = path.trim().to_string();
        return (!path.is_empty()).then_some(RecentConfigEntry {
            enabled: true,
            score,
            path,
        });
    }
    if let Some((score, path)) = entry.split_once("<<<") {
        let score = score
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|score| score.is_finite())?;
        let path = path.trim().to_string();
        return (!path.is_empty()).then_some(RecentConfigEntry {
            enabled: false,
            score,
            path,
        });
    }
    None
}

pub(crate) fn recent_config_entry_line(entry: &RecentConfigEntry) -> String {
    let separator = if entry.enabled { ">>>" } else { "<<<" };
    format!(
        "{}{separator}{}",
        recent_score_text(entry.score),
        entry.path
    )
}
pub(crate) fn record_recent_item_with_config(
    items: &mut Vec<String>,
    path: &Path,
    scoring: &ScoringConfig,
) {
    let key = recent_item_key(path);
    let old_score = items
        .iter()
        .filter_map(|item| parse_recent_config_entry(item))
        .find_map(|entry| (fold_text(&entry.path) == key).then_some(entry.score));
    items.retain(|item| recent_entry_key(item).as_deref() != Some(key.as_str()));
    let raw_path = path.to_string_lossy().to_string();
    let score = old_score
        .map(|score| score + scoring.recent_launch_increment.max(0) as f32)
        .unwrap_or(scoring.recent_first_launch_score.max(0) as f32);
    items.insert(0, recent_entry_line_with_config(score, &raw_path, scoring));
    items.truncate(RECENT_LIMIT);
}

pub(crate) fn remove_recent_item(items: &mut Vec<String>, path: &Path) -> bool {
    let key = recent_item_key(path);
    let original_len = items.len();
    items.retain(|item| recent_entry_key(item).as_deref() != Some(key.as_str()));
    items.len() != original_len
}

#[cfg(test)]
pub(crate) fn recent_item_score(path: &Path, recent_items: &HashMap<String, RecentEntry>) -> i32 {
    let key = recent_item_key(path);
    recent_items.get(&key).map(recent_entry_score).unwrap_or(0)
}

pub(crate) fn recent_item_score_with_config(
    path: &Path,
    recent_items: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
) -> i32 {
    let key = recent_item_key(path);
    recent_items
        .get(&key)
        .map(|entry| recent_entry_score_with_config(entry, scoring))
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn recent_entry_score(entry: &RecentEntry) -> i32 {
    recent_entry_score_with_config(entry, &ScoringConfig::default())
}

pub(crate) fn recent_entry_score_with_config(entry: &RecentEntry, scoring: &ScoringConfig) -> i32 {
    entry
        .score
        .min(scoring.recent_score_ceiling.max(0) as f32)
        .round() as i32
}

pub(crate) fn recent_lookup(recent_items: &[String]) -> HashMap<String, RecentEntry> {
    recent_items
        .iter()
        .filter_map(|item| {
            parse_recent_entry(item).map(|(score, key, _)| (key, RecentEntry { score }))
        })
        .collect()
}

pub(crate) fn recent_entry_key(entry: &str) -> Option<String> {
    parse_recent_config_entry(entry).map(|entry| fold_text(&entry.path))
}

pub(crate) fn parse_recent_entry(entry: &str) -> Option<(f32, String, String)> {
    let parsed = parse_recent_config_entry(entry)?;
    if !parsed.enabled {
        return None;
    }
    let key = fold_text(&parsed.path);
    (!key.is_empty()).then_some((parsed.score, key, parsed.path))
}

#[cfg(test)]
pub(crate) fn recent_entry_line(score: f32, path: &str) -> String {
    recent_entry_line_with_config(score, path, &ScoringConfig::default())
}

pub(crate) fn recent_entry_line_with_config(
    score: f32,
    path: &str,
    _scoring: &ScoringConfig,
) -> String {
    if score.fract().abs() < f32::EPSILON {
        format!("{}>>>{path}", score as i32)
    } else {
        format!("{score:.3}>>>{path}")
    }
}

pub(crate) fn recent_items_path() -> PathBuf {
    app_config_dir().join("recent_items.txt")
}

pub(crate) fn index_path() -> PathBuf {
    app_config_dir().join("index_folders.txt")
}

pub(crate) fn settings_path() -> PathBuf {
    app_config_dir().join("settings.ini")
}

pub(crate) fn scoring_path() -> PathBuf {
    app_config_dir().join("scoring.ini")
}

pub(crate) fn popup_sound_path() -> PathBuf {
    app_dir().join("Assets").join("fping.wav")
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ProcessIdentity {
    pid: u32,
    creation_time_100ns: u64,
}

#[derive(Clone, Copy, Debug)]
struct ProcessResourceSample {
    identity: ProcessIdentity,
    cpu_time_100ns: u64,
    private_working_set_bytes: Option<u64>,
}

struct TitleProcessTreeClock {
    wall_time: Instant,
    cpu_times: HashMap<ProcessIdentity, u64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ProcessTreeMetrics {
    cpu_percent: u32,
    private_working_set_bytes: Option<u64>,
}

static TITLE_PROCESS_TREE_CLOCK: OnceLock<Mutex<TitleProcessTreeClock>> = OnceLock::new();

pub(crate) fn window_title_text(
    language: AppLanguage,
    show_cpu: bool,
    show_ram: bool,
    show_build_timestamp: bool,
) -> String {
    let metrics = (show_cpu || show_ram).then(current_process_tree_metrics);
    window_title_text_with_metrics(
        language,
        show_build_timestamp,
        show_cpu.then(|| metrics.map_or(0, |sample| sample.cpu_percent)),
        show_ram.then(|| {
            metrics
                .and_then(|sample| sample.private_working_set_bytes)
                .map(format_memory_size)
                .unwrap_or_else(|| "n/a".to_string())
        }),
    )
}

fn window_title_text_with_metrics(
    language: AppLanguage,
    show_build_timestamp: bool,
    cpu_percent: Option<u32>,
    memory_text: Option<String>,
) -> String {
    let mut metrics = Vec::new();
    if let Some(cpu_percent) = cpu_percent {
        metrics.push(format!("{cpu_percent}%"));
    }
    if let Some(memory_text) = memory_text {
        metrics.push(memory_text);
    }
    let metrics = if metrics.is_empty() {
        String::new()
    } else {
        format!(" | {}", metrics.join(" "))
    };
    let build_timestamp = show_build_timestamp
        .then(|| format!(" ({})", APP_BUILD_TIME))
        .unwrap_or_default();
    format!(
        "{} {}{}{} | {}",
        APP_TITLE_NAME,
        APP_VERSION,
        build_timestamp,
        metrics,
        local_title_timestamp(language)
    )
}

fn file_time_100ns(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

fn descendant_process_ids(root_pid: u32, entries: &[(u32, u32)]) -> HashSet<u32> {
    let mut process_ids = HashSet::from([root_pid]);
    loop {
        let previous_len = process_ids.len();
        for &(pid, parent_pid) in entries {
            if process_ids.contains(&parent_pid) {
                process_ids.insert(pid);
            }
        }
        if process_ids.len() == previous_len {
            return process_ids;
        }
    }
}

fn snapshot_process_entries() -> Option<Vec<(u32, u32)>> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut entries = Vec::new();
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                entries.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        Some(entries)
    }
}

fn read_process_resource_sample(pid: u32) -> Option<ProcessResourceSample> {
    unsafe {
        let current_pid = GetCurrentProcessId();
        let handle = if pid == current_pid {
            GetCurrentProcess()
        } else {
            OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid)
        };
        if handle.is_null() {
            return None;
        }

        let mut created: FILETIME = std::mem::zeroed();
        let mut exited: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let times_ok =
            GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0;

        let mut counters: PROCESS_MEMORY_COUNTERS_EX2 = std::mem::zeroed();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32;
        let memory_ok = GetProcessMemoryInfo(
            handle,
            &mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            counters.cb,
        ) != 0;

        if pid != current_pid {
            CloseHandle(handle);
        }
        if !times_ok {
            return None;
        }

        Some(ProcessResourceSample {
            identity: ProcessIdentity {
                pid,
                creation_time_100ns: file_time_100ns(created),
            },
            cpu_time_100ns: file_time_100ns(kernel).saturating_add(file_time_100ns(user)),
            private_working_set_bytes: memory_ok.then_some(counters.PrivateWorkingSetSize as u64),
        })
    }
}

fn snapshot_process_tree_resources() -> Option<Vec<ProcessResourceSample>> {
    let entries = snapshot_process_entries()?;
    let process_ids = descendant_process_ids(unsafe { GetCurrentProcessId() }, &entries);
    Some(
        process_ids
            .into_iter()
            .filter_map(read_process_resource_sample)
            .collect(),
    )
}

fn process_cpu_delta_100ns(
    previous: &HashMap<ProcessIdentity, u64>,
    current: &[ProcessResourceSample],
) -> u64 {
    current
        .iter()
        .filter_map(|sample| {
            previous
                .get(&sample.identity)
                .map(|previous_time| sample.cpu_time_100ns.saturating_sub(*previous_time))
        })
        .fold(0u64, u64::saturating_add)
}

fn process_tree_private_working_set_bytes(samples: &[ProcessResourceSample]) -> Option<u64> {
    samples
        .iter()
        .filter_map(|sample| sample.private_working_set_bytes)
        .reduce(u64::saturating_add)
}

fn normalized_cpu_percent(process_delta: u64, wall_100ns: u64, processor_count: usize) -> u32 {
    if wall_100ns == 0 || processor_count == 0 {
        return 0;
    }
    let denominator = wall_100ns as f64 * processor_count as f64;
    (process_delta as f64 * 100.0 / denominator)
        .round()
        .clamp(0.0, 100.0) as u32
}

fn current_process_tree_metrics() -> ProcessTreeMetrics {
    let now = Instant::now();
    let Some(samples) = snapshot_process_tree_resources() else {
        return ProcessTreeMetrics::default();
    };
    let current_cpu_times = samples
        .iter()
        .map(|sample| (sample.identity, sample.cpu_time_100ns))
        .collect::<HashMap<_, _>>();
    let private_working_set_bytes = process_tree_private_working_set_bytes(&samples);
    let clock = TITLE_PROCESS_TREE_CLOCK.get_or_init(|| {
        Mutex::new(TitleProcessTreeClock {
            wall_time: now,
            cpu_times: current_cpu_times.clone(),
        })
    });
    let Ok(mut clock) = clock.lock() else {
        return ProcessTreeMetrics {
            private_working_set_bytes,
            ..Default::default()
        };
    };
    let wall_100ns = now
        .saturating_duration_since(clock.wall_time)
        .as_nanos()
        .saturating_div(100) as u64;
    let process_delta = process_cpu_delta_100ns(&clock.cpu_times, &samples);
    clock.wall_time = now;
    clock.cpu_times = current_cpu_times;
    ProcessTreeMetrics {
        cpu_percent: normalized_cpu_percent(
            process_delta,
            wall_100ns,
            std::thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(1),
        ),
        private_working_set_bytes,
    }
}

pub(crate) fn local_title_timestamp(language: AppLanguage) -> String {
    unsafe {
        let mut time = std::mem::zeroed();
        GetLocalTime(&mut time);
        format_title_timestamp(&time, language)
    }
}

pub(crate) fn format_title_timestamp(time: &SYSTEMTIME, language: AppLanguage) -> String {
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];

    let day_name = DAYS
        .get(time.wDayOfWeek as usize)
        .copied()
        .map(|value| localized(language, value))
        .unwrap_or_else(|| localized(language, "Unknown"));
    let month_name = MONTHS
        .get(time.wMonth.saturating_sub(1) as usize)
        .copied()
        .map(|value| localized(language, value))
        .unwrap_or_else(|| localized(language, "Unknown"));
    let mut hour = time.wHour % 12;
    if hour == 0 {
        hour = 12;
    }
    let meridiem = localized(language, if time.wHour < 12 { "AM" } else { "PM" });
    let template = localized(
        language,
        "{weekday} {month} {day}, {year} - {hour}:{minute} {meridiem}",
    );
    template
        .replace("{weekday}", day_name)
        .replace("{month}", month_name)
        .replace("{day}", &format!("{:02}", time.wDay))
        .replace("{year}", &time.wYear.to_string())
        .replace("{hour}", &hour.to_string())
        .replace("{minute}", &format!("{:02}", time.wMinute))
        .replace("{meridiem}", meridiem)
}

pub(crate) fn default_pattern_rules() -> Vec<PatternRule> {
    let rules = [
        ("*.lnk", 50),
        ("*.exe", 150),
        ("*.bak", -100),
        ("*.tmp", -100),
        ("*.chm", -50),
        ("*help*", -100),
        ("*uninstall*", -200),
        ("*readme*", -100),
        ("*read me*", -100),
    ];
    rules
        .into_iter()
        .map(|(pattern, score)| PatternRule {
            pattern: pattern.to_string(),
            folded_pattern: fold_text(pattern),
            score,
            modifiers: Vec::new(),
        })
        .collect()
}

pub(crate) fn wildcard_rule_matches(rule: &PatternRule, path: &Path, folded_path: &str) -> bool {
    let pattern = rule.pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if pattern.contains(['\\', '/']) {
        return wildcard_match(&rule.folded_pattern, folded_path);
    }
    path.file_name()
        .and_then(|value| value.to_str())
        .map(fold_text)
        .is_some_and(|file_name| wildcard_match(&rule.folded_pattern, &file_name))
}

pub(crate) fn wildcard_match(pattern: &str, candidate: &str) -> bool {
    let pattern_chars = pattern.chars().collect::<Vec<_>>();
    wildcard_match_chars(&pattern_chars, candidate)
}

fn wildcard_match_chars(pattern_chars: &[char], candidate: &str) -> bool {
    let candidate_chars = candidate.chars().collect::<Vec<_>>();
    let mut pattern_index = 0usize;
    let mut candidate_index = 0usize;
    let mut star_index = None;
    let mut star_candidate_index = 0usize;

    while candidate_index < candidate_chars.len() {
        if pattern_index < pattern_chars.len()
            && (pattern_chars[pattern_index] == '?'
                || pattern_chars[pattern_index] == candidate_chars[candidate_index])
        {
            pattern_index += 1;
            candidate_index += 1;
        } else if pattern_index < pattern_chars.len() && pattern_chars[pattern_index] == '*' {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_candidate_index = candidate_index;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_candidate_index += 1;
            candidate_index = star_candidate_index;
        } else {
            return false;
        }
    }

    while pattern_index < pattern_chars.len() && pattern_chars[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern_chars.len()
}

fn scoring_section(line: &str) -> Option<String> {
    let line = line.trim();
    (line.starts_with('[') && line.ends_with(']'))
        .then(|| line.trim_matches(['[', ']']).to_string())
}

fn heuristic_rule_identity(key: &str) -> Option<&'static str> {
    match key.trim() {
        "Recent First Launch Score" => Some("Recent First Launch Score"),
        "Recent Launch Increment" => Some("Recent Launch Increment"),
        "Exact Match Bonus" => Some("Exact Match Bonus"),
        "Exact Word Bonus" => Some("Exact Word Bonus"),
        "Prefix Match Bonus" => Some("Prefix Match Bonus"),
        "Word Boundary Bonus" => Some("Word Boundary Bonus"),
        "Consecutive Match Bonus" => Some("Consecutive Match Bonus"),
        "Acronym Match Bonus" => Some("Acronym Match Bonus"),
        "Leftmost Match Bonus" => Some("Leftmost Match Bonus"),
        "Leftmost Distance Penalty" => Some("Leftmost Distance Penalty"),
        "Length Score Weight" => Some("Length Score Weight"),
        "Compact Match Bonus" => Some("Compact Match Bonus"),
        "Recent Score Ceiling" => Some("Recent Score Ceiling"),
        "Explicit Folder Name Match Adjustment" => Some("Explicit Folder Name Match Adjustment"),
        "Folder Score As % of File Score" => Some("Folder Score As % of File Score"),
        "Path Depth Penalty" => Some("Path Depth Penalty"),
        "Recency Date Bonus" => Some("Recency Date Bonus"),
        _ => None,
    }
}

fn scoring_rule_identity(section: &str, line: &str) -> Option<String> {
    let line = line.trim().trim_start_matches('\u{feff}').trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
        return None;
    }
    let (key, raw_value) = line.split_once("<<<").or_else(|| line.split_once('='))?;
    let (value, modifiers) = parse_scoring_value_and_modifiers(raw_value);
    value.trim().parse::<i32>().ok()?;
    match section {
        "HeuristicScoring" => {
            heuristic_rule_identity(key).map(|identity| format!("heuristic:{identity}"))
        }
        "PatternScoring" if !key.trim().is_empty() => Some(format!(
            "pattern:{}:{}",
            fold_text(key.trim()),
            modifiers.join("\u{1f}")
        )),
        _ => None,
    }
}

fn scoring_rule_lines(content: &str) -> Vec<(String, String, String)> {
    let mut section = String::new();
    let mut rules = Vec::new();
    for line in content.lines() {
        if let Some(next_section) = scoring_section(line) {
            section = next_section;
            continue;
        }
        if let Some(identity) = scoring_rule_identity(&section, line) {
            rules.push((section.clone(), identity, line.to_string()));
        }
    }
    rules
}

fn insert_missing_scoring_section_lines(
    lines: &mut Vec<String>,
    section_name: &str,
    missing: Vec<String>,
) {
    if missing.is_empty() {
        return;
    }
    let section_start = lines
        .iter()
        .position(|line| scoring_section(line).as_deref() == Some(section_name));
    if let Some(section_start) = section_start {
        let section_end = lines
            .iter()
            .enumerate()
            .skip(section_start + 1)
            .find_map(|(index, line)| scoring_section(line).map(|_| index))
            .unwrap_or(lines.len());
        lines.splice(section_end..section_end, missing);
        return;
    }
    if !lines.is_empty() && !lines.last().is_some_and(|line| line.is_empty()) {
        lines.push(String::new());
    }
    lines.push(format!("[{section_name}]"));
    lines.extend(missing);
}

pub(crate) fn hydrate_scoring_text(content: &str, defaults: &str) -> String {
    if content.is_empty() {
        return defaults.to_string();
    }
    let present = scoring_rule_lines(content)
        .into_iter()
        .map(|(_, identity, _)| identity)
        .collect::<HashSet<_>>();
    let mut missing_heuristics = Vec::new();
    let mut missing_patterns = Vec::new();
    for (section, identity, line) in scoring_rule_lines(defaults) {
        if present.contains(&identity) {
            continue;
        }
        if section == "HeuristicScoring" {
            missing_heuristics.push(line);
        } else if section == "PatternScoring" {
            missing_patterns.push(line);
        }
    }
    if missing_heuristics.is_empty() && missing_patterns.is_empty() {
        return content.to_string();
    }
    let mut lines = content.lines().map(str::to_string).collect::<Vec<_>>();
    insert_missing_scoring_section_lines(&mut lines, "HeuristicScoring", missing_heuristics);
    insert_missing_scoring_section_lines(&mut lines, "PatternScoring", missing_patterns);
    let newline = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut output = lines.join(newline);
    if content.ends_with(['\r', '\n']) {
        output.push_str(newline);
    }
    output
}

pub(crate) fn ensure_scoring_file() {
    let path = scoring_path();
    let defaults = default_scoring_text();
    drop(ensure_required_text_file(
        "scoring.ini",
        &path,
        &defaults,
        hydrate_scoring_text,
    ));
}

pub(crate) fn load_scoring_config() -> ScoringConfig {
    let path = scoring_path();
    let defaults = default_scoring_text();
    let content = ensure_required_text_file("scoring.ini", &path, &defaults, hydrate_scoring_text);
    parse_scoring_config(&content)
}

pub(crate) fn parse_scoring_config(content: &str) -> ScoringConfig {
    let mut config = ScoringConfig {
        recent_first_launch_score: 0,
        recent_launch_increment: 0,
        exact_match_bonus: 0,
        exact_word_bonus: 0,
        prefix_match_bonus: 0,
        word_boundary_bonus: 0,
        consecutive_match_bonus: 0,
        acronym_match_bonus: 0,
        leftmost_match_bonus: 0,
        leftmost_distance_penalty: 0,
        length_score_weight: 0,
        compact_match_bonus: 0,
        recent_score_ceiling: 0,
        explicit_folder_name_match_adjustment: 0,
        folder_score_as_file_score_percent: 100,
        path_depth_penalty: 0,
        recency_date_bonus: 0,
        recency_date_enabled: false,
        pattern_rules: Vec::new(),
    };
    let mut section = String::new();
    let mut pattern_rules = Vec::new();
    let mut saw_pattern_section = false;

    for raw_line in content.lines() {
        let line = raw_line.trim().trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.trim_matches(['[', ']']).to_string();
            if section == "PatternScoring" {
                saw_pattern_section = true;
            }
            continue;
        }
        let (enabled, raw_key, raw_value) = if let Some((key, value)) = line.split_once("<<<") {
            (false, key, value)
        } else if let Some((key, value)) = line.split_once('=') {
            (true, key, value)
        } else {
            continue;
        };
        let key = raw_key.trim();
        let (value, modifiers) = parse_scoring_value_and_modifiers(raw_value);

        match section.as_str() {
            "HeuristicScoring" => {
                apply_scoring_heuristic(&mut config, key, value.as_str(), enabled);
            }
            "PatternScoring" => {
                saw_pattern_section = true;
                if enabled {
                    if let Ok(score) = value.parse::<i32>() {
                        pattern_rules.push(PatternRule {
                            pattern: key.to_string(),
                            folded_pattern: fold_text(key),
                            score,
                            modifiers,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    if saw_pattern_section {
        config.pattern_rules = pattern_rules;
    }
    config
}

pub(crate) fn parse_scoring_rule_entries(content: &str) -> Vec<ScoringRuleEntry> {
    let mut entries = Vec::new();
    let mut section = String::new();

    for raw_line in content.lines() {
        let line = raw_line.trim().trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line.trim_matches(['[', ']'].as_ref()).to_string();
            continue;
        }

        let (enabled, key, raw_value) = if let Some((key, value)) = line.split_once("<<<") {
            (false, key.trim(), value.trim())
        } else if let Some((key, value)) = line.split_once('=') {
            (true, key.trim(), value.trim())
        } else {
            continue;
        };
        let (value, modifiers) = parse_scoring_value_and_modifiers(raw_value);
        if key.is_empty() || value.parse::<i32>().is_err() {
            continue;
        }

        match section.as_str() {
            "HeuristicScoring" => {
                if heuristic_rule_identity(key).is_none() {
                    continue;
                }
                entries.push(ScoringRuleEntry {
                    kind: ScoringRuleKind::Heuristic,
                    key: key.to_string(),
                    value: value.to_string(),
                    modifiers: Vec::new(),
                    enabled,
                });
            }
            "PatternScoring" => {
                entries.push(ScoringRuleEntry {
                    kind: ScoringRuleKind::Pattern,
                    key: key.to_string(),
                    value: value.to_string(),
                    modifiers,
                    enabled,
                });
            }
            _ => {}
        }
    }

    entries
}

pub(crate) fn scoring_rule_entries_to_text(entries: &[ScoringRuleEntry]) -> String {
    let mut lines = vec![
        "# Flash Launch scoring rules.".to_string(),
        "# Use <<< instead of = to disable a rule.".to_string(),
        "".to_string(),
        "[HeuristicScoring]".to_string(),
    ];

    for entry in entries
        .iter()
        .filter(|entry| entry.kind == ScoringRuleKind::Heuristic)
    {
        lines.push(format_scoring_rule_line(entry));
    }
    lines.push(String::new());
    lines.push("[PatternScoring]".to_string());
    for entry in entries
        .iter()
        .filter(|entry| entry.kind == ScoringRuleKind::Pattern)
    {
        lines.push(format_scoring_rule_line(entry));
    }

    let mut text = lines.join("\r\n");
    text.push_str("\r\n");
    text
}

pub(crate) fn merge_scoring_rule_entries_text(
    existing: &str,
    entries: &[ScoringRuleEntry],
) -> String {
    let heuristic_lines = entries
        .iter()
        .filter(|entry| entry.kind == ScoringRuleKind::Heuristic)
        .map(format_scoring_rule_line)
        .collect::<Vec<_>>();
    let pattern_lines = entries
        .iter()
        .filter(|entry| entry.kind == ScoringRuleKind::Pattern)
        .map(format_scoring_rule_line)
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut section = String::new();
    let mut heuristic_inserted = false;
    let mut pattern_inserted = false;

    let insert_current_section = |output: &mut Vec<String>,
                                  section: &str,
                                  heuristic_inserted: &mut bool,
                                  pattern_inserted: &mut bool| {
        if section == "HeuristicScoring" && !*heuristic_inserted {
            output.extend(heuristic_lines.iter().cloned());
            *heuristic_inserted = true;
        } else if section == "PatternScoring" && !*pattern_inserted {
            output.extend(pattern_lines.iter().cloned());
            *pattern_inserted = true;
        }
    };

    for line in existing.lines() {
        if let Some(next_section) = scoring_section(line) {
            insert_current_section(
                &mut output,
                &section,
                &mut heuristic_inserted,
                &mut pattern_inserted,
            );
            section = next_section;
            output.push(line.to_string());
            continue;
        }
        let is_known = scoring_rule_identity(&section, line).is_some();
        if is_known {
            insert_current_section(
                &mut output,
                &section,
                &mut heuristic_inserted,
                &mut pattern_inserted,
            );
        } else {
            output.push(line.to_string());
        }
    }
    insert_current_section(
        &mut output,
        &section,
        &mut heuristic_inserted,
        &mut pattern_inserted,
    );
    if !heuristic_inserted {
        if !output.is_empty() && !output.last().is_some_and(|line| line.is_empty()) {
            output.push(String::new());
        }
        output.push("[HeuristicScoring]".to_string());
        output.extend(heuristic_lines);
    }
    if !pattern_inserted {
        if !output.is_empty() && !output.last().is_some_and(|line| line.is_empty()) {
            output.push(String::new());
        }
        output.push("[PatternScoring]".to_string());
        output.extend(pattern_lines);
    }

    let newline = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut content = output.join(newline);
    if existing.ends_with(['\r', '\n']) {
        content.push_str(newline);
    }
    content
}

pub(crate) fn format_scoring_rule_line(entry: &ScoringRuleEntry) -> String {
    let operator = if entry.enabled { "=" } else { "<<<" };
    let modifiers = if entry.kind == ScoringRuleKind::Pattern && !entry.modifiers.is_empty() {
        let keywords = entry
            .modifiers
            .iter()
            .map(|keyword| modifier_keyword_display(keyword))
            .collect::<Vec<_>>()
            .join(",");
        format!(" | modifiers={keywords}")
    } else {
        String::new()
    };
    format!(
        "{}{}{}{}",
        entry.key.trim(),
        operator,
        entry.value.trim(),
        modifiers
    )
}

pub(crate) fn parse_scoring_value_and_modifiers(raw_value: &str) -> (String, Vec<String>) {
    let mut score = String::new();
    let mut modifiers = Vec::new();
    for (index, part) in raw_value.split('|').map(str::trim).enumerate() {
        if index == 0 {
            score = part.to_string();
            continue;
        }
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim() {
            "modifiers" => modifiers = normalize_keywords(value),
            _ => {}
        }
    }
    (score, modifiers)
}

pub(crate) fn scoring_modifiers_display(modifiers: &[String]) -> String {
    modifiers
        .iter()
        .map(|keyword| modifier_keyword_display(keyword))
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn apply_scoring_heuristic(
    config: &mut ScoringConfig,
    key: &str,
    value: &str,
    enabled: bool,
) {
    let int_value = value.trim().parse::<i32>().ok();

    match key.trim() {
        "Recent First Launch Score" => {
            config.recent_first_launch_score =
                scoring_int_value(int_value, config.recent_first_launch_score, enabled, 0)
        }
        "Recent Launch Increment" => {
            config.recent_launch_increment =
                scoring_int_value(int_value, config.recent_launch_increment, enabled, 0)
        }
        "Exact Match Bonus" => {
            config.exact_match_bonus =
                scoring_int_value(int_value, config.exact_match_bonus, enabled, 0)
        }
        "Exact Word Bonus" => {
            config.exact_word_bonus =
                scoring_int_value(int_value, config.exact_word_bonus, enabled, 0)
        }
        "Prefix Match Bonus" => {
            config.prefix_match_bonus =
                scoring_int_value(int_value, config.prefix_match_bonus, enabled, 0)
        }
        "Word Boundary Bonus" => {
            config.word_boundary_bonus =
                scoring_int_value(int_value, config.word_boundary_bonus, enabled, 0)
        }
        "Consecutive Match Bonus" => {
            config.consecutive_match_bonus =
                scoring_int_value(int_value, config.consecutive_match_bonus, enabled, 0)
        }
        "Acronym Match Bonus" => {
            config.acronym_match_bonus =
                scoring_int_value(int_value, config.acronym_match_bonus, enabled, 0)
        }
        "Leftmost Match Bonus" => {
            config.leftmost_match_bonus =
                scoring_int_value(int_value, config.leftmost_match_bonus, enabled, 0)
        }
        "Leftmost Distance Penalty" => {
            config.leftmost_distance_penalty =
                scoring_int_value(int_value, config.leftmost_distance_penalty, enabled, 0)
        }
        "Length Score Weight" => {
            config.length_score_weight =
                scoring_non_negative_value(int_value, config.length_score_weight, enabled, 0)
        }
        "Compact Match Bonus" => {
            config.compact_match_bonus =
                scoring_non_negative_value(int_value, config.compact_match_bonus, enabled, 0)
        }
        "Recent Score Ceiling" => {
            config.recent_score_ceiling =
                scoring_int_value(int_value, config.recent_score_ceiling, enabled, i32::MAX)
        }
        "Explicit Folder Name Match Adjustment" => {
            config.explicit_folder_name_match_adjustment = scoring_int_value(
                int_value,
                config.explicit_folder_name_match_adjustment,
                enabled,
                0,
            )
        }
        "Folder Score As % of File Score" => {
            config.folder_score_as_file_score_percent = scoring_non_negative_value(
                int_value,
                config.folder_score_as_file_score_percent,
                enabled,
                0,
            )
        }
        "Path Depth Penalty" => {
            config.path_depth_penalty =
                scoring_int_value(int_value, config.path_depth_penalty, enabled, 0)
        }
        "Recency Date Bonus" => {
            config.recency_date_bonus = int_value.unwrap_or(config.recency_date_bonus);
            config.recency_date_enabled = enabled;
        }
        _ => {}
    }
}

pub(crate) fn scoring_int_value(
    value: Option<i32>,
    current: i32,
    enabled: bool,
    disabled_value: i32,
) -> i32 {
    if enabled {
        value.unwrap_or(current)
    } else {
        disabled_value
    }
}

pub(crate) fn is_non_negative_scoring_rule(key: &str) -> bool {
    matches!(key.trim(), "Length Score Weight" | "Compact Match Bonus")
}

pub(crate) fn scoring_rule_value_is_valid(key: &str, value: &str) -> bool {
    value
        .trim()
        .parse::<i32>()
        .is_ok_and(|value| !is_non_negative_scoring_rule(key) || value >= 0)
}

fn scoring_non_negative_value(
    value: Option<i32>,
    current: i32,
    enabled: bool,
    disabled_value: i32,
) -> i32 {
    if enabled {
        value.filter(|value| *value >= 0).unwrap_or(current)
    } else {
        disabled_value
    }
}

pub(crate) fn parse_bool_setting(value: &str) -> Option<bool> {
    match value.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

pub(crate) fn default_index_roots() -> Vec<IndexRoot> {
    default_index_text()
        .lines()
        .filter_map(parse_index_root_line)
        .map(normalized_index_root)
        .collect()
}

pub(crate) fn default_index_root_value(root: &IndexRoot) -> String {
    default_index_root_for(root)
        .as_ref()
        .map(compact_index_root_value)
        .unwrap_or_default()
}

pub(crate) fn default_index_root_for(root: &IndexRoot) -> Option<IndexRoot> {
    let defaults = default_index_roots();
    defaults
        .iter()
        .find(|candidate| candidate.raw.eq_ignore_ascii_case(&root.raw))
        .cloned()
}

pub(crate) fn compact_index_root_value(root: &IndexRoot) -> String {
    let mut parts = vec![
        format!("enabled={}", if root.enabled { 1 } else { 0 }),
        format!("score={}", root.score),
        format!("depth={}", depth_display(root.max_depth)),
    ];
    if !root.keywords.is_empty() {
        parts.push(format!("keywords={}", root.keywords.join(",")));
    }
    parts.join(" | ")
}

pub(crate) fn default_scoring_rule_value(rule: &ScoringRuleEntry) -> String {
    default_scoring_rule_for(rule)
        .map(|candidate| {
            format!(
                "{} ({})",
                candidate.value,
                if candidate.enabled { "On" } else { "Off" }
            )
        })
        .unwrap_or_default()
}

pub(crate) fn default_scoring_rule_for(rule: &ScoringRuleEntry) -> Option<ScoringRuleEntry> {
    parse_scoring_rule_entries(&default_scoring_text())
        .into_iter()
        .find(|candidate| {
            candidate.kind == rule.kind
                && candidate.key.eq_ignore_ascii_case(&rule.key)
                && candidate.modifiers == rule.modifiers
        })
}

pub(crate) fn default_scoring_text() -> String {
    let mut lines = vec![
        "# Flash Launch scoring rules.".to_string(),
        "# Use <<< instead of = to disable a rule.".to_string(),
        "".to_string(),
        "[HeuristicScoring]".to_string(),
        "# Strong boost when the query exactly matches the file/folder name.".to_string(),
        "Exact Match Bonus=250".to_string(),
        "# Boost when the query matches a complete word.".to_string(),
        "Exact Word Bonus=75".to_string(),
        "# Boost when the name starts with the query.".to_string(),
        "Prefix Match Bonus=110".to_string(),
        "# Boost when the query starts at a word boundary.".to_string(),
        "Word Boundary Bonus=60".to_string(),
        "# Boost when the query appears as one contiguous substring.".to_string(),
        "Consecutive Match Bonus=40".to_string(),
        "# Boost when the query matches initials from separated words.".to_string(),
        "Acronym Match Bonus=75".to_string(),
        "# Maximum boost for a match at the left edge.".to_string(),
        "Leftmost Match Bonus=25".to_string(),
        "# Points removed from the leftmost boost per preceding character.".to_string(),
        "Leftmost Distance Penalty=5".to_string(),
        "# Weight for query length relative to the full filename length.".to_string(),
        "Length Score Weight=90".to_string(),
        "# Maximum boost for adjacent fuzzy-match character pairs.".to_string(),
        "Compact Match Bonus=10".to_string(),
        "# Maximum score contributed by the fuzzy matching formula.".to_string(),
        "# Starting score for the first successful launch of a recent item.".to_string(),
        "Recent First Launch Score=90".to_string(),
        "# Score added each time the same recent item is launched again.".to_string(),
        "Recent Launch Increment=5".to_string(),
        "# Maximum recent score used for ranking.".to_string(),
        "Recent Score Ceiling=250".to_string(),
        "# Boost for script files that can be launched directly.".to_string(),
        "# Boost when the query explicitly names a parent folder.".to_string(),
        "Explicit Folder Name Match Adjustment=40".to_string(),
        "# Penalty per folder level relative to the owning Search Folder.".to_string(),
        "Path Depth Penalty=4".to_string(),
        "# Small penalty for long paths, applied per path length unit.".to_string(),
        "# Path length where the long-path penalty begins.".to_string(),
        "# Boost for folder results.".to_string(),
        "Folder Score As % of File Score=90".to_string(),
        "# Optional modified-date boost.".to_string(),
        "Recency Date Bonus<<<50".to_string(),
        "".to_string(),
        "[PatternScoring]".to_string(),
    ];
    for rule in default_pattern_rules() {
        lines.push(format!("{}={}", rule.pattern, rule.score));
    }
    let mut text = lines.join("\r\n");
    text.push_str("\r\n");
    text
}

pub(crate) fn format_memory_size(bytes: u64) -> String {
    let mib = bytes as f64 / 1024.0 / 1024.0;
    format!("{mib:.1} MB")
}

#[cfg(test)]
mod launcher_scoring_tests {
    use super::*;

    #[test]
    fn shared_assets_are_resolved_beside_the_executable() {
        assert_eq!(popup_sound_path(), app_dir().join("Assets").join("fping.wav"));
        assert_eq!(app_dir().join(APP_ICON_FILE), app_dir().join("Assets").join("Flash Launch.ico"));
        let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("Assets");
        assert!(assets.join("fping.wav").is_file());
        assert!(assets.join("Flash Launch.ico").is_file());
    }

    #[test]
    fn memory_size_uses_task_manager_style_megabytes() {
        assert_eq!(format_memory_size(0), "0.0 MB");
        assert_eq!(format_memory_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_memory_size(15 * 1024 * 1024 + 512 * 1024), "15.5 MB");
    }

    #[test]
    fn cpu_percent_is_normalized_across_logical_processors() {
        assert_eq!(normalized_cpu_percent(10_000_000, 10_000_000, 4), 25);
        assert_eq!(normalized_cpu_percent(40_000_000, 10_000_000, 4), 100);
        assert_eq!(normalized_cpu_percent(1, 0, 4), 0);
    }

    #[test]
    fn descendant_process_ids_include_nested_children_only() {
        let entries = [(10, 1), (11, 10), (12, 11), (20, 2), (21, 20)];
        assert_eq!(
            descendant_process_ids(10, &entries),
            HashSet::from([10, 11, 12])
        );
    }

    #[test]
    fn process_cpu_delta_ignores_new_and_reused_processes() {
        let existing = ProcessIdentity {
            pid: 10,
            creation_time_100ns: 100,
        };
        let reused = ProcessIdentity {
            pid: 11,
            creation_time_100ns: 300,
        };
        let previous = HashMap::from([
            (existing, 1_000),
            (
                ProcessIdentity {
                    pid: 11,
                    creation_time_100ns: 200,
                },
                5_000,
            ),
        ]);
        let current = [
            ProcessResourceSample {
                identity: existing,
                cpu_time_100ns: 1_750,
                private_working_set_bytes: Some(100),
            },
            ProcessResourceSample {
                identity: reused,
                cpu_time_100ns: 250,
                private_working_set_bytes: Some(200),
            },
            ProcessResourceSample {
                identity: ProcessIdentity {
                    pid: 12,
                    creation_time_100ns: 400,
                },
                cpu_time_100ns: 900,
                private_working_set_bytes: None,
            },
        ];

        assert_eq!(process_cpu_delta_100ns(&previous, &current), 750);
    }

    #[test]
    fn process_tree_memory_sums_available_samples() {
        let samples = [
            ProcessResourceSample {
                identity: ProcessIdentity {
                    pid: 10,
                    creation_time_100ns: 100,
                },
                cpu_time_100ns: 0,
                private_working_set_bytes: Some(128),
            },
            ProcessResourceSample {
                identity: ProcessIdentity {
                    pid: 11,
                    creation_time_100ns: 200,
                },
                cpu_time_100ns: 0,
                private_working_set_bytes: None,
            },
            ProcessResourceSample {
                identity: ProcessIdentity {
                    pid: 12,
                    creation_time_100ns: 300,
                },
                cpu_time_100ns: 0,
                private_working_set_bytes: Some(256),
            },
        ];

        assert_eq!(process_tree_private_working_set_bytes(&samples), Some(384));
        assert_eq!(process_tree_private_working_set_bytes(&[]), None);
    }

    #[test]
    fn window_title_metrics_follow_independent_visibility_settings() {
        let both = window_title_text_with_metrics(
            AppLanguage::Source,
            true,
            Some(3),
            Some("140 MB".to_string()),
        );
        assert!(both.contains(" | 3% 140 MB | "));

        let cpu_only = window_title_text_with_metrics(AppLanguage::Source, true, Some(3), None);
        assert!(cpu_only.contains(" | 3% | "));
        assert!(!cpu_only.contains("MB"));

        let ram_only = window_title_text_with_metrics(
            AppLanguage::Source,
            true,
            None,
            Some("140 MB".to_string()),
        );
        assert!(ram_only.contains(" | 140 MB | "));
        assert!(!ram_only.contains("%"));

        let neither = window_title_text_with_metrics(AppLanguage::Source, true, None, None);
        assert!(!neither.contains(" MB"));
        assert!(!neither.contains('%'));

        let hidden_build = window_title_text_with_metrics(AppLanguage::Source, false, None, None);
        assert!(!hidden_build.contains(APP_BUILD_TIME));
    }

    fn test_item(title: &str, path: &str) -> LaunchItem {
        LaunchItem::new(title, path, PathBuf::from(path), false, 0, None)
    }

    #[test]
    fn launch_item_caches_searchable_path_parts() {
        let item = LaunchItem::new(
            "Chrome.exe",
            "Application",
            r"C:\Program Files\Google\Chrome\Application\Chrome.exe",
            false,
            0,
            None,
        );

        assert_eq!(item.folded_title, fold_text("Chrome.exe"));
        assert_eq!(item.folded_stem, fold_text("Chrome"));
        assert_eq!(
            item.folded_parent,
            searchable_text(r"C:\Program Files\Google\Chrome\Application")
        );
    }

    #[test]
    fn multi_word_file_uses_best_full_filename_or_token_average_score() {
        let scoring = ScoringConfig {
            folder_score_as_file_score_percent: 100,
            ..Default::default()
        };
        let file = LaunchItem::new(
            "Work Tool.exe",
            "Apps\\",
            r"C:\Apps\Work Tool.exe",
            false,
            0,
            None,
        );
        let folder = LaunchItem::new("Work Tool", "Apps\\", r"C:\Apps\Work Tool", true, 0, None);
        let whole_filename_score =
            score_text_with_config("work tool", "work tool.exe", false, &scoring).unwrap();
        let file_text_score = score_item_text_with_config("work tool", &file, &scoring).unwrap();
        let reversed_text_score =
            score_item_text_with_config("tool work", &file, &scoring).unwrap();

        assert_eq!(
            file_text_score,
            whole_filename_score + scoring.exact_match_bonus
        );
        assert!(reversed_text_score < file_text_score);
        assert!(
            ranked_score_with_config("work tool", &[], &file, &HashMap::new(), &scoring)
                > ranked_score_with_config("work tool", &[], &folder, &HashMap::new(), &scoring)
        );
    }

    #[test]
    fn multi_word_query_matches_file_name_and_parent_in_any_order() {
        let scoring = ScoringConfig {
            folder_score_as_file_score_percent: 100,
            ..Default::default()
        };
        let item = LaunchItem::new(
            "Ultrasurf.lnk",
            r"C:\Users\TestUser\Desktop\IP",
            r"C:\Users\TestUser\Desktop\IP\Ultrasurf.lnk",
            false,
            0,
            None,
        );
        let recent_items = HashMap::new();

        let forward_text = score_item_text_with_config("ip ultrasurf", &item, &scoring).unwrap();
        let reversed_text = score_item_text_with_config("ultrasurf ip", &item, &scoring).unwrap();
        assert_eq!(forward_text, reversed_text);
        assert_eq!(
            score_item_path_with_config("ip ultrasurf", &item, &scoring),
            scoring.explicit_folder_name_match_adjustment
        );
        assert_eq!(
            score_item_path_with_config("ultrasurf ip", &item, &scoring),
            scoring.explicit_folder_name_match_adjustment
        );

        let forward_score =
            ranked_score_with_config("ip ultrasurf", &[], &item, &recent_items, &scoring).unwrap();
        let reversed_score =
            ranked_score_with_config("ultrasurf ip", &[], &item, &recent_items, &scoring).unwrap();
        assert_eq!(forward_score, reversed_score);
        assert!(ranked_score_with_config("ip", &[], &item, &recent_items, &scoring).is_none());
        assert!(
            ranked_score_with_config("missing ultrasurf", &[], &item, &recent_items, &scoring)
                .is_none()
        );
    }

    #[test]
    fn empty_text_query_never_receives_folder_path_adjustment() {
        let scoring = ScoringConfig {
            pattern_rules: vec![PatternRule {
                pattern: "*help*".to_string(),
                folded_pattern: "*help*".to_string(),
                score: -100,
                modifiers: Vec::new(),
            }],
            ..Default::default()
        };
        let mut item = test_item("Help.txt", r"C:\Docs\Help.txt");
        item.index_score = 50;
        let spec = parse_search_query("+docs").effective_for_scoring(&scoring);

        assert!(spec.folded_search_text.is_empty());
        assert_eq!(score_item_path_with_config("", &item, &scoring), 0);
        assert_eq!(score_item_path_with_config("   ", &item, &scoring), 0);
        let breakdown =
            score_breakdown_with_config(&item, &spec, &HashMap::new(), &scoring, false).unwrap();
        assert_eq!(breakdown.path_score, 0);
        assert!(breakdown.final_score <= 0);
    }

    #[test]
    fn pattern_score_sums_in_i64_before_clamping() {
        let rule = |score| PatternRule {
            pattern: "*".to_string(),
            folded_pattern: "*".to_string(),
            score,
            modifiers: Vec::new(),
        };
        let path = Path::new(r"C:\Apps\Tool.exe");
        let positive_overflow = ScoringConfig {
            pattern_rules: vec![rule(i32::MAX), rule(1)],
            ..Default::default()
        };
        let negative_overflow = ScoringConfig {
            pattern_rules: vec![rule(i32::MIN), rule(-1)],
            ..Default::default()
        };
        let mixed = ScoringConfig {
            pattern_rules: vec![rule(i32::MAX), rule(i32::MAX), rule(i32::MIN)],
            ..Default::default()
        };

        assert_eq!(
            pattern_score_with_config(path, &positive_overflow),
            i32::MAX
        );
        assert_eq!(
            pattern_score_with_config(path, &negative_overflow),
            i32::MIN
        );
        assert_eq!(pattern_score_with_config(path, &mixed), i32::MAX - 1);
    }

    #[test]
    fn recency_date_score_obeys_all_day_boundaries() {
        const DAY: i64 = 86_400;
        let scoring = ScoringConfig {
            recency_date_bonus: 100,
            recency_date_enabled: true,
            ..Default::default()
        };
        let now = 400 * DAY;

        assert_eq!(
            recency_date_score_at(Some(now - DAY + 1), now, &scoring),
            100
        );
        assert_eq!(recency_date_score_at(Some(now - DAY), now, &scoring), 50);
        assert_eq!(
            recency_date_score_at(Some(now - 29 * DAY), now, &scoring),
            50
        );
        assert_eq!(
            recency_date_score_at(Some(now - 30 * DAY), now, &scoring),
            25
        );
        assert_eq!(
            recency_date_score_at(Some(now - 364 * DAY), now, &scoring),
            25
        );
        assert_eq!(
            recency_date_score_at(Some(now - 365 * DAY), now, &scoring),
            0
        );
        assert_eq!(recency_date_score_at(Some(now + 1), now, &scoring), 100);
        assert_eq!(recency_date_score_at(None, now, &scoring), 0);
    }

    #[test]
    fn indexed_item_recency_decreases_without_rebuilding_item() {
        const DAY: i64 = 86_400;
        let scoring = ScoringConfig {
            recency_date_bonus: 100,
            recency_date_enabled: true,
            pattern_rules: Vec::new(),
            ..Default::default()
        };
        let modified_at = 1_700_000_000;
        let item = LaunchItem::new(
            "Report.txt",
            "Docs\\",
            r"C:\Docs\Report.txt",
            false,
            100,
            Some(modified_at),
        );
        let mut spec = parse_search_query("report").effective_for_scoring(&scoring);
        spec.scoring_time_unix_seconds = modified_at + DAY - 1;
        let fresh =
            score_breakdown_with_config(&item, &spec, &HashMap::new(), &scoring, false).unwrap();
        spec.scoring_time_unix_seconds = modified_at + 30 * DAY;
        let aged =
            score_breakdown_with_config(&item, &spec, &HashMap::new(), &scoring, false).unwrap();
        spec.scoring_time_unix_seconds = modified_at + 365 * DAY;
        let expired =
            score_breakdown_with_config(&item, &spec, &HashMap::new(), &scoring, false).unwrap();

        assert_eq!(fresh.recency_score, 100);
        assert_eq!(aged.recency_score, 25);
        assert_eq!(expired.recency_score, 0);
        assert_eq!(item.modified_at_unix_seconds, Some(modified_at));
    }

    #[test]
    fn title_timestamp_uses_english_format() {
        let time = SYSTEMTIME {
            wYear: 2026,
            wMonth: 7,
            wDayOfWeek: 3,
            wDay: 1,
            wHour: 6,
            wMinute: 10,
            wSecond: 0,
            wMilliseconds: 0,
        };

        assert_eq!(
            format_title_timestamp(&time, AppLanguage::Source),
            "Wednesday July 01, 2026 - 6:10 AM"
        );
    }

    #[test]
    fn numeric_fuzzy_matches_reference_for_ascii_and_unicode() {
        let scoring = ScoringConfig::default();
        for (query, candidate) in [
            ("chr", "chrome.exe"),
            ("cde", "visual code.exe"),
            ("aa", "a-long-a-name"),
            ("中文", "快速中文搜索"),
            ("đn", "điện năng"),
            ("ệ", "điện"),
        ] {
            assert_eq!(
                fuzzy_score_numeric(query, candidate, &scoring),
                fuzzy_score_parts(query, candidate, &scoring).map(|parts| parts.total),
                "query={query:?} candidate={candidate:?}"
            );
        }
    }

    #[test]
    fn prepared_multi_token_score_matches_reference_item_score() {
        let scoring = ScoringConfig::default();
        for (query, path) in [
            ("visual code", r"C:\\Apps\\Visual Studio Code.exe"),
            ("code studio", r"C:\\Apps\\Visual Studio Code.exe"),
            ("中文 搜索", r"C:\\Apps\\快速中文搜索.exe"),
            (
                "ip ultrasurf",
                r"C:\\Users\\TestUser\\Desktop\\IP\\Ultrasurf.lnk",
            ),
            (
                "ultrasurf ip",
                r"C:\\Users\\TestUser\\Desktop\\IP\\Ultrasurf.lnk",
            ),
        ] {
            let spec = parse_search_query(query).effective_for_scoring(&scoring);
            let prepared_query = PreparedQuery::new(&spec, &scoring);
            let prepared_name = PreparedCandidateName::from_path(Path::new(path)).unwrap();
            let item = test_item(
                Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .as_ref(),
                path,
            );
            assert_eq!(
                prepared_query.score_candidate_name(
                    &prepared_name,
                    item.folded_parent(),
                    false,
                    &scoring,
                ),
                score_item_text_with_config(&spec.folded_search_text, &item, &scoring),
                "query={query:?} path={path:?}"
            );
        }
    }

    #[test]
    fn compiled_pattern_matchers_equal_reference_wildcards() {
        let scoring = ScoringConfig {
            pattern_rules: [
                ("*", 1),
                ("exact.txt", 2),
                ("*.exe", 4),
                ("chrome*", 8),
                ("*readme", 16),
                ("*help*", 32),
                ("c?de*.rs", 64),
                (r"c:\\apps\\*\\tool.exe", 128),
            ]
            .into_iter()
            .map(|(pattern, score)| PatternRule {
                pattern: pattern.to_string(),
                folded_pattern: fold_text(pattern),
                score,
                modifiers: Vec::new(),
            })
            .collect(),
            ..Default::default()
        };
        let spec = parse_search_query("tool").effective_for_scoring(&scoring);
        let prepared_query = PreparedQuery::new(&spec, &scoring);
        for path in [
            Path::new(r"C:\\Apps\\Browser\\chrome.exe"),
            Path::new(r"C:\\Apps\\Docs\\readme"),
            Path::new(r"C:\\Apps\\Code\\code-main.rs"),
            Path::new(r"C:\\Apps\\Suite\\tool.exe"),
            Path::new(r"C:\\Other\\exact.txt"),
        ] {
            let folded_file_name = path
                .file_name()
                .and_then(|value| value.to_str())
                .map(fold_text)
                .unwrap_or_default();
            assert_eq!(
                prepared_query.pattern_score(path, &folded_file_name),
                pattern_score_with_config_and_modifiers(path, &scoring, &[]),
                "path={path:?}"
            );
        }
    }

    #[test]
    fn disabled_recency_does_not_load_metadata() {
        let disabled = ScoringConfig {
            recency_date_enabled: false,
            ..Default::default()
        };
        let enabled = ScoringConfig {
            recency_date_enabled: true,
            ..Default::default()
        };
        let calls = std::cell::Cell::new(0usize);
        let disabled_value = modified_at_for_scoring(&disabled, || {
            calls.set(calls.get() + 1);
            Some(123)
        });
        assert_eq!(disabled_value, None);
        assert_eq!(calls.get(), 0);
        let enabled_value = modified_at_for_scoring(&enabled, || {
            calls.set(calls.get() + 1);
            Some(123)
        });
        assert_eq!(enabled_value, Some(123));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn fuzzy_score_rewards_consecutive_and_leftmost_matches() {
        let scoring = ScoringConfig::default();

        assert!(fuzzy_score("abc", "abc", &scoring) > fuzzy_score("abc", "axbyc", &scoring));
        assert!(fuzzy_score("abc", "abc", &scoring) > fuzzy_score("abc", "zzzabc", &scoring));
    }

    #[test]
    fn exact_word_bonus_prefers_code_word_over_codex_prefix() {
        let scoring = ScoringConfig::default();

        let code_word = score_text_with_config("code", "code tool", false, &scoring).unwrap();
        let codex_prefix = score_text_with_config("code", "codex tool", false, &scoring).unwrap();

        assert!(code_word > codex_prefix);
    }

    #[test]
    fn filename_scoring_uses_full_name_preserves_separators_and_tokenizes_spaces() {
        let scoring = ScoringConfig::default();
        let underscore = test_item("codex_config.toml", r"C:\Apps\codex_config.toml");
        let hyphen = test_item("codex-config.toml", r"C:\Apps\codex-config.toml");

        assert!(
            score_item_text_with_config("codex_config", &underscore, &scoring)
                > score_item_text_with_config("codex_config", &hyphen, &scoring)
        );
        assert!(score_item_text_with_config("codex-config", &hyphen, &scoring).is_some());
        assert!(score_item_text_with_config("codex config", &underscore, &scoring).is_some());
        assert!(score_item_text_with_config("codex config", &hyphen, &scoring).is_some());
        assert!(score_item_text_with_config(
            "c++",
            &test_item("C++ Tools", r"C:\Apps\C++ Tools.txt"),
            &scoring
        )
        .is_some());
        assert!(score_item_text_with_config(
            "c#",
            &test_item("C# Tools", r"C:\Apps\C# Tools.txt"),
            &scoring
        )
        .is_some());
        assert!(score_item_text_with_config(
            "(beta)",
            &test_item("App (Beta).exe", r"C:\Apps\App (Beta).exe"),
            &scoring
        )
        .is_some());
        assert!(score_item_text_with_config(
            "alpha—beta",
            &test_item("Alpha—Beta.txt", r"C:\Apps\Alpha—Beta.txt"),
            &scoring
        )
        .is_some());
    }

    #[test]
    fn full_filename_and_exact_stem_scoring_cover_extension_and_cumulative_bonuses() {
        let item = test_item("tool.exe", r"C:\Apps\tool.exe");
        let with_exact = ScoringConfig::default();
        let without_exact = ScoringConfig {
            exact_match_bonus: 0,
            ..with_exact.clone()
        };

        let exact_score = score_item_text_with_config("tool", &item, &with_exact).unwrap();
        let baseline = score_item_text_with_config("tool", &item, &without_exact).unwrap();

        assert_eq!(exact_score - baseline, with_exact.exact_match_bonus);
        assert!(score_item_text_with_config("to", &item, &with_exact).is_some());
        assert!(score_item_text_with_config("exe", &item, &with_exact).is_some());
    }

    #[test]
    fn multi_token_code_to_prefers_real_words_while_to_matches_toml_extension() {
        let scoring = ScoringConfig::default();
        let merger = test_item("Code Merger Tool.exe", r"C:\Apps\Code Merger Tool.exe");
        let config = test_item(
            "codex_config_config.toml",
            r"C:\Apps\codex_config_config.toml",
        );

        let merger_score = score_item_text_with_config("code to", &merger, &scoring).unwrap();
        let config_score = score_item_text_with_config("code to", &config, &scoring).unwrap();

        assert!(merger_score > config_score);
        assert!(score_item_text_with_config("to", &config, &scoring).is_some());
    }

    #[test]
    fn leftmost_formula_uses_configured_bonus_and_distance_penalty() {
        let default = ScoringConfig::default();
        let custom = ScoringConfig {
            leftmost_match_bonus: 100,
            leftmost_distance_penalty: 3,
            ..default.clone()
        };
        let disabled = ScoringConfig {
            leftmost_match_bonus: 0,
            leftmost_distance_penalty: 0,
            ..default.clone()
        };

        assert_eq!(fuzzy_score("a", "zabcdefghi", &default), Some(39));
        assert_eq!(fuzzy_score("a", "zabcdefghi", &custom), Some(116));
        assert_eq!(fuzzy_score("a", "zabcdefghi", &disabled), Some(19));
    }

    #[test]
    fn fuzzy_formula_uses_independent_length_and_compact_rules() {
        let default = ScoringConfig::default();
        assert_eq!(default.length_score_weight, 90);
        assert_eq!(default.compact_match_bonus, 10);
        assert_eq!(fuzzy_score("abc", "abc", &default), Some(125));

        let custom = ScoringConfig {
            length_score_weight: 300,
            compact_match_bonus: 100,
            ..default.clone()
        };
        assert_eq!(fuzzy_score("ab", "abcde", &custom), Some(245));

        let disabled_components = ScoringConfig {
            length_score_weight: 0,
            compact_match_bonus: 0,
            ..default.clone()
        };
        assert_eq!(fuzzy_score("ab", "abcde", &disabled_components), Some(25));

        let uncapped = ScoringConfig {
            length_score_weight: 300,
            compact_match_bonus: 100,
            ..default.clone()
        };
        assert_eq!(fuzzy_score("abc", "abc", &uncapped), Some(425));
    }

    #[test]
    fn exact_prefix_substring_and_scattered_rank_in_order() {
        let scoring = ScoringConfig::default();
        let exact = score_text_with_config("chrome", "chrome", false, &scoring).unwrap();
        let prefix = score_text_with_config("chr", "chrome", false, &scoring).unwrap();
        let substring = score_text_with_config("ome", "chrome", false, &scoring).unwrap();
        let scattered = score_text_with_config("coe", "chrome", false, &scoring).unwrap();

        assert!(exact > prefix);
        assert!(prefix > substring);
        assert!(substring > scattered);
    }

    #[test]
    fn score_ranges_stay_near_human_scale() {
        let scoring = ScoringConfig::default();
        let item = test_item("Chrome", r#"C:\Apps\Chrome.exe"#);
        let exact =
            ranked_score_with_config("chrome", &[], &item, &HashMap::new(), &scoring).unwrap();
        let scattered = score_text_with_config("coe", "chrome", false, &scoring).unwrap();

        assert!((450..=1000).contains(&exact), "exact score was {exact}");
        assert!(scattered <= 200, "scattered score was {scattered}");
    }

    #[test]
    fn prefix_can_beat_unrelated_history_fuzzy_match() {
        let scoring = ScoringConfig::default();
        let prefix = test_item("Chrome", r#"C:\Apps\Chrome.exe"#);
        let fuzzy = test_item("Character Map", r#"C:\Tools\Character Map.exe"#);
        let mut recent = HashMap::new();
        recent.insert(
            fuzzy.path.to_string_lossy().to_string(),
            RecentEntry { score: 220.0 },
        );

        let prefix_score =
            ranked_score_with_config("chr", &[], &prefix, &recent, &scoring).unwrap();
        let fuzzy_score = ranked_score_with_config("chr", &[], &fuzzy, &recent, &scoring).unwrap();

        assert!(prefix_score > fuzzy_score);
    }

    #[test]
    fn pattern_bonus_and_index_root_still_affect_rank() {
        let scoring = ScoringConfig::default();
        let plain = test_item("Tool", r#"C:\Apps\Tool.dat"#);
        let exe = test_item("Tool", r#"C:\Apps\Tool.exe"#);
        let mut indexed = test_item("Tool", r#"C:\Preferred\Tool.dat"#);
        indexed.index_score = 50;

        let plain_score =
            ranked_score_with_config("tool", &[], &plain, &HashMap::new(), &scoring).unwrap();
        let exe_score =
            ranked_score_with_config("tool", &[], &exe, &HashMap::new(), &scoring).unwrap();
        let indexed_score =
            ranked_score_with_config("tool", &[], &indexed, &HashMap::new(), &scoring).unwrap();

        assert!(exe_score > plain_score);
        assert!(indexed_score > plain_score);
    }

    #[test]
    fn executable_and_shortcut_scores_come_only_from_pattern_rules() {
        let scoring = ScoringConfig::default();
        let spec = parse_search_query("tool").effective_for_scoring(&scoring);
        let exe = test_item("Tool", r"C:\Apps\Tool.exe");
        let shortcut = test_item("Tool", r"C:\Apps\Tool.lnk");

        let exe_breakdown =
            score_breakdown_with_config(&exe, &spec, &HashMap::new(), &scoring, false).unwrap();
        let shortcut_breakdown =
            score_breakdown_with_config(&shortcut, &spec, &HashMap::new(), &scoring, false)
                .unwrap();

        assert_eq!(exe_breakdown.pattern_score, 150);
        assert_eq!(exe_breakdown.folder_score, 0);
        assert_eq!(shortcut_breakdown.pattern_score, 50);
        assert_eq!(shortcut_breakdown.folder_score, 0);
        assert!(exe_breakdown.result.score_detail.contains("pattern 150"));
        assert!(exe_breakdown.result.score_detail.contains("folder 0"));
    }

    #[test]
    fn files_receive_no_adjustment_while_folder_and_recency_scoring_remain() {
        let scoring = ScoringConfig {
            folder_score_as_file_score_percent: 125,
            recency_date_bonus: 100,
            recency_date_enabled: true,
            ..Default::default()
        };
        let script = test_item("Tool", r"C:\Apps\Tool.bat");
        let folder = LaunchItem::new("Tool", "Apps", r"C:\Apps\Tool", true, 0, None);

        assert_eq!(folder_score_adjustment(&script, 200, &scoring), 0);
        assert_eq!(folder_score_adjustment(&folder, 200, &scoring), 50);
        assert_eq!(
            folder_score_adjustment(&folder, 200, &ScoringConfig::default()),
            -20
        );
        assert_eq!(recency_date_score_at(Some(1_000), 1_000, &scoring), 100);
        let disabled = ScoringConfig {
            recency_date_enabled: false,
            ..scoring
        };
        assert_eq!(recency_date_score_at(Some(1_000), 1_000, &disabled), 0);
    }

    #[test]
    fn legacy_scoring_sections_rules_and_modifiers_are_ignored() {
        let content = concat!(
            "[Heuristics]\n",
            "Exact Match Bonus=999\n",
            "[HeuristicScoring]\n",
            "Word Prefix Match Bonus=777\n",
            "Exact Match Bonus=500\n",
            "[Patterns]\n",
            "*.exe=999\n",
            "[PatternScoring]\n",
            "*.exe=150 | modifier=tools\n",
        );
        let parsed = parse_scoring_config(content);
        let entries = parse_scoring_rule_entries(content);

        assert_eq!(parsed.exact_match_bonus, 500);
        assert_eq!(parsed.word_boundary_bonus, 0);
        assert_eq!(parsed.pattern_rules.len(), 1);
        assert_eq!(parsed.pattern_rules[0].score, 150);
        assert!(parsed.pattern_rules[0].modifiers.is_empty());
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .all(|entry| entry.key != "Word Prefix Match Bonus"));
    }

    #[test]
    fn recent_items_require_canonical_scored_syntax() {
        assert!(parse_recent_config_entry(r"C:\Apps\Tool.exe").is_none());
        assert!(parse_recent_config_entry(r"100>C:\Apps\Tool.exe").is_none());
        assert!(parse_recent_config_entry(r"100>>>C:\Apps\Tool.exe").is_some());
        assert!(parse_recent_config_entry(r"100<<<C:\Apps\Tool.exe").is_some());
    }

    #[test]
    fn default_scoring_text_has_comments_and_disabled_rules_parse() {
        let text = default_scoring_text();

        assert!(text.contains("# Strong boost when the query exactly matches"));
        assert!(text.contains("# Penalty per folder level relative to the owning Search Folder."));
        assert!(text.contains("Recency Date Bonus<<<50"));
        for removed in [
            "Script Bonus",
            "Long Path Penalty",
            "Long Path Threshold",
            "Fuzzy Score Ceiling",
        ] {
            assert!(!text.contains(removed));
        }
        let folder_score_percent_index = text
            .find("Folder Score As % of File Score=90")
            .expect("Folder score percentage default");
        let recency_date_index = text
            .find("Recency Date Bonus<<<50")
            .expect("Recency Date Bonus default");
        assert!(folder_score_percent_index < recency_date_index);

        let entries = parse_scoring_rule_entries(&text);
        assert!(entries.iter().any(|entry| entry.key == "Exact Match Bonus"));
        assert!(entries
            .iter()
            .any(|entry| entry.key == "Recency Date Bonus" && !entry.enabled));
        assert!(!entries.iter().any(|entry| entry.key.starts_with('#')));
    }

    #[test]
    fn scoring_hydration_inserts_missing_rules_into_their_sections() {
        let content = concat!(
            "# Keep\n",
            "[HeuristicScoring]\n",
            "Recent Launch Increment=25\n",
            "custom malformed line\n",
        );

        let hydrated = hydrate_scoring_text(content, &default_scoring_text());
        let pattern_section = hydrated.find("[PatternScoring]").unwrap();
        let executable_rule = hydrated.find("*.exe=150").unwrap();
        let parsed = parse_scoring_config(&hydrated);

        assert!(hydrated.contains("# Keep\n"));
        assert!(hydrated.contains("custom malformed line\n"));
        assert!(executable_rule > pattern_section);
        assert_eq!(parsed.recent_launch_increment, 25);
        assert_eq!(parsed.exact_word_bonus, 75);
        assert_eq!(parsed.leftmost_match_bonus, 25);
        assert_eq!(parsed.leftmost_distance_penalty, 5);
        assert_eq!(parsed.length_score_weight, 90);
        assert_eq!(parsed.compact_match_bonus, 10);
        assert_eq!(
            pattern_score_with_config(Path::new(r"C:\Apps\Tool.exe"), &parsed),
            150
        );
    }

    #[test]
    fn disabled_default_scoring_rule_counts_as_present_during_hydration() {
        let content = concat!(
            "[HeuristicScoring]\n",
            "Folder Score As % of File Score<<<90\n",
            "\n",
            "[PatternScoring]\n",
        );

        let hydrated = hydrate_scoring_text(content, &default_scoring_text());

        assert_eq!(
            hydrated
                .matches("Folder Score As % of File Score<<<90")
                .count(),
            1
        );
        assert_eq!(
            parse_scoring_config(&hydrated).folder_score_as_file_score_percent,
            0
        );
    }

    #[test]
    fn scoring_default_matches_hydrated_production_defaults() {
        let expected = ScoringConfig::default();
        let actual = parse_scoring_config(&default_scoring_text());
        assert_eq!(
            actual.recent_first_launch_score,
            expected.recent_first_launch_score
        );
        assert_eq!(
            actual.recent_launch_increment,
            expected.recent_launch_increment
        );
        assert_eq!(actual.exact_match_bonus, expected.exact_match_bonus);
        assert_eq!(actual.prefix_match_bonus, expected.prefix_match_bonus);
        assert_eq!(actual.word_boundary_bonus, expected.word_boundary_bonus);
        assert_eq!(
            actual.consecutive_match_bonus,
            expected.consecutive_match_bonus
        );
        assert_eq!(actual.acronym_match_bonus, expected.acronym_match_bonus);
        assert_eq!(actual.recent_score_ceiling, expected.recent_score_ceiling);
        assert_eq!(
            actual.explicit_folder_name_match_adjustment,
            expected.explicit_folder_name_match_adjustment
        );
        assert_eq!(
            actual.folder_score_as_file_score_percent,
            expected.folder_score_as_file_score_percent
        );
        assert_eq!(actual.path_depth_penalty, expected.path_depth_penalty);
        assert_eq!(actual.length_score_weight, expected.length_score_weight);
        assert_eq!(actual.compact_match_bonus, expected.compact_match_bonus);
        assert_eq!(actual.recency_date_bonus, expected.recency_date_bonus);
        assert_eq!(actual.recency_date_enabled, expected.recency_date_enabled);
        let actual_patterns = actual
            .pattern_rules
            .iter()
            .map(|rule| (&rule.pattern, rule.score, &rule.modifiers))
            .collect::<Vec<_>>();
        let expected_patterns = expected
            .pattern_rules
            .iter()
            .map(|rule| (&rule.pattern, rule.score, &rule.modifiers))
            .collect::<Vec<_>>();
        assert_eq!(actual_patterns, expected_patterns);
    }

    #[test]
    fn parser_supports_custom_and_disabled_new_scoring_rules() {
        let parsed = parse_scoring_config(concat!(
            "[HeuristicScoring]\n",
            "Exact Word Bonus=275\n",
            "Leftmost Match Bonus<<<90\n",
            "Leftmost Distance Penalty=3\n",
        ));

        assert_eq!(parsed.exact_word_bonus, 275);
        assert_eq!(parsed.leftmost_match_bonus, 0);
        assert_eq!(parsed.leftmost_distance_penalty, 3);
    }

    #[test]
    fn parser_supports_custom_and_disabled_independent_fuzzy_rules() {
        let custom = parse_scoring_config(concat!(
            "[HeuristicScoring]\n",
            "Length Score Weight=240\n",
            "Compact Match Bonus=35\n",
        ));
        assert_eq!(custom.length_score_weight, 240);
        assert_eq!(custom.compact_match_bonus, 35);

        let disabled = parse_scoring_config(concat!(
            "[HeuristicScoring]\n",
            "Length Score Weight<<<180\n",
            "Compact Match Bonus<<<20\n",
        ));
        assert_eq!(disabled.length_score_weight, 0);
        assert_eq!(disabled.compact_match_bonus, 0);
    }

    #[test]
    fn independent_fuzzy_rules_require_non_negative_integers() {
        for key in ["Length Score Weight", "Compact Match Bonus"] {
            assert!(scoring_rule_value_is_valid(key, "0"));
            assert!(scoring_rule_value_is_valid(key, "250"));
            assert!(!scoring_rule_value_is_valid(key, "-1"));
            assert!(!scoring_rule_value_is_valid(key, "1.5"));
        }
        assert!(scoring_rule_value_is_valid("Exact Match Bonus", "-100"));
    }

    #[test]
    fn required_config_merges_preserve_opaque_lines() {
        let root = IndexRoot {
            raw: r"C:\New".to_string(),
            path: Some(PathBuf::from(r"C:\New")),
            enabled: true,
            score: 33,
            max_depth: 2,
            label: String::new(),
            keywords: vec!["new".to_string()],
        };
        let index_source = concat!(
            "# Keep\r\n",
            "future=value\r\n",
            "C:\\Old | enabled=1 | score=1 | depth=-1\r\n",
            "malformed | score=bad\r\n",
        );
        let merged_index = merge_index_roots_text(index_source, &[root]);
        assert!(merged_index.contains("# Keep\r\nfuture=value\r\n"));
        assert!(merged_index.contains("malformed | score=bad\r\n"));
        assert!(merged_index.contains("C:\\\\New | enabled=1 | score=33 | depth=2 | keywords=new"));
        assert!(!merged_index.contains("C:\\Old"));

        let scoring_source = concat!(
            "# Keep\n",
            "[HeuristicScoring]\n",
            "Exact Match Bonus=250\n",
            "future malformed\n",
            "\n",
            "[PatternScoring]\n",
            "*.exe=150\n",
            "broken=not-a-number\n",
        );
        let entries = parse_scoring_rule_entries(&default_scoring_text());
        let merged_scoring = merge_scoring_rule_entries_text(scoring_source, &entries);
        assert!(merged_scoring.contains("# Keep\n"));
        assert!(merged_scoring.contains("future malformed\n"));
        assert!(merged_scoring.contains("broken=not-a-number\n"));
        assert_eq!(merged_scoring.matches("Exact Match Bonus=250").count(), 1);
        assert_eq!(merged_scoring.matches("Length Score Weight=90").count(), 1);
        assert_eq!(merged_scoring.matches("Compact Match Bonus=10").count(), 1);
    }

    #[test]
    fn search_folder_delete_keeps_defaults_and_removes_custom() {
        let mut roots = default_index_roots();
        let default_count = roots.len();
        let custom = normalized_index_root(IndexRoot {
            raw: r"D:\Apps".to_string(),
            path: None,
            enabled: true,
            score: 100,
            max_depth: SEARCH_DEPTH_ALL,
            label: String::new(),
            keywords: Vec::new(),
        });
        roots.push(custom);

        use crate::settings_model::SettingsListModel;

        let mut model = crate::settings_model::SearchFoldersModel { roots: &mut roots };

        assert!(!model.delete_row(0));
        assert_eq!(model.row_count(), default_count + 1);
        assert!(model.delete_row(default_count));
        assert_eq!(model.row_count(), default_count);
    }

    #[test]
    fn settings_table_models_advertise_shared_capabilities() {
        use crate::settings_model::{
            PluginAliasModel, QueryLaunchRulesModel, ScoringModel, SearchFoldersModel,
            SettingsListModel,
        };

        let mut roots = default_index_roots();
        let search_model = SearchFoldersModel { roots: &mut roots };
        assert!(search_model.supports_multi_select());
        assert!(search_model.supports_toggle());
        assert!(search_model.supports_add());
        assert!(search_model.supports_delete());
        assert!(search_model.supports_reset_default());
        assert!(search_model.supports_move());

        let mut query_rules = vec![QueryLaunchRule {
            query: "calc".to_string(),
            target: "Calculator".to_string(),
        }];
        let query_model = QueryLaunchRulesModel {
            items: &mut query_rules,
        };
        assert!(query_model.supports_multi_select());
        assert!(query_model.supports_delete());
        assert!(!query_model.supports_toggle());

        let mut aliases = vec![plugins::PluginAliasConfigEntry {
            id: "calculator".to_string(),
            name: "Calculator".to_string(),
            alias: "c".to_string(),
            default_alias: "calc".to_string(),
        }];
        let mut alias_model = PluginAliasModel {
            entries: &mut aliases,
        };
        assert!(alias_model.supports_multi_select());
        assert!(alias_model.supports_reset_default());
        assert!(alias_model.reset_row_to_default(0));
        assert_eq!(alias_model.cell_text(0, 1), "calc");

        let mut scoring_rules = parse_scoring_rule_entries(&default_scoring_text());
        let heuristic_model = ScoringModel {
            rules: &mut scoring_rules,
            kind: ScoringRuleKind::Heuristic,
            language: AppLanguage::Source,
        };
        assert!(heuristic_model.supports_multi_select());
        assert!(heuristic_model.supports_toggle());
        assert!(heuristic_model.supports_reset_default());
        assert!(!heuristic_model.supports_delete());
        assert!(!heuristic_model.supports_move());

        let pattern_model = ScoringModel {
            rules: &mut scoring_rules,
            kind: ScoringRuleKind::Pattern,
            language: AppLanguage::Source,
        };
        assert!(pattern_model.supports_add());
        assert!(pattern_model.supports_delete());
        assert!(pattern_model.supports_move());
        assert!(pattern_model.supports_modifier_help());
    }

    #[test]
    fn heuristic_settings_model_edits_toggles_and_resets_new_rules() {
        use crate::settings_model::{ScoringModel, SettingsListModel};

        let mut scoring_rules = parse_scoring_rule_entries(&default_scoring_text());
        let mut model = ScoringModel {
            rules: &mut scoring_rules,
            kind: ScoringRuleKind::Heuristic,
            language: AppLanguage::Source,
        };
        let label = heuristic_scoring_label("Leftmost Match Bonus", AppLanguage::Source);
        assert_eq!(label, "Leftmost Match Bonus");
        let row = (0..model.row_count())
            .find(|row| model.cell_text(*row, 0) == label)
            .unwrap();

        assert_eq!(model.cell_text(row, 1), "25");
        assert!(!model.cell_text(row, 2).is_empty());
        assert_eq!(model.cell_text(row, 3), "25 (On)");
        model.set_cell_text(row, 1, "95");
        assert_eq!(model.cell_text(row, 1), "95");
        assert!(model.toggle_row(row));
        assert!(!model.is_row_enabled(row));
        assert!(model.reset_row_to_default(row));
        assert_eq!(model.cell_text(row, 1), "25");
        assert!(model.is_row_enabled(row));
    }

    #[test]
    fn heuristic_settings_model_resets_each_independent_fuzzy_rule() {
        use crate::settings_model::{ScoringModel, SettingsListModel};

        let mut scoring_rules = parse_scoring_rule_entries(&default_scoring_text());
        let mut model = ScoringModel {
            rules: &mut scoring_rules,
            kind: ScoringRuleKind::Heuristic,
            language: AppLanguage::Source,
        };

        for (key, default_value) in [
            ("Length Score Weight", "90"),
            ("Compact Match Bonus", "10"),
        ] {
            let row = (0..model.row_count())
                .find(|row| model.cell_text(*row, 0) == key)
                .unwrap();
            assert_eq!(model.cell_text(row, 1), default_value);
            assert_eq!(model.cell_text(row, 3), format!("{default_value} (On)"));
            assert!(!model.cell_text(row, 2).is_empty());

            model.set_cell_text(row, 1, "999");
            assert!(model.toggle_row(row));
            assert!(!model.is_row_enabled(row));
            assert!(model.reset_row_to_default(row));
            assert_eq!(model.cell_text(row, 1), default_value);
            assert!(model.is_row_enabled(row));
        }
    }

    #[test]
    fn folder_score_and_history_adjust_final_rank() {
        let scoring = ScoringConfig::default();
        let plain = test_item("Chrome", "C:\\Apps\\Chrome.exe");
        let mut boosted = test_item("Chrome", "C:\\Preferred\\Chrome.exe");
        boosted.index_score = 50;
        let mut recent = HashMap::new();
        recent.insert(
            boosted.path.to_string_lossy().to_string(),
            RecentEntry { score: 100.0 },
        );

        let plain_score =
            ranked_score_with_config("chrome", &[], &plain, &recent, &scoring).unwrap();
        let boosted_score =
            ranked_score_with_config("chrome", &[], &boosted, &recent, &scoring).unwrap();

        assert!(boosted_score > plain_score);
    }

    #[test]
    fn pattern_penalty_keeps_zero_and_negative_results_visible() {
        let item = test_item("Chrome", "C:\\Temp\\Chrome.exe");
        let base_scoring = ScoringConfig {
            pattern_rules: Vec::new(),
            ..Default::default()
        };
        let baseline_score =
            ranked_score_with_config("chrome", &[], &item, &HashMap::new(), &base_scoring).unwrap();

        for (pattern_score, expected_score) in [(-baseline_score, 0), (-baseline_score - 1, -1)] {
            let scoring = ScoringConfig {
                pattern_rules: vec![PatternRule {
                    pattern: "chrome*".to_string(),
                    folded_pattern: "chrome*".to_string(),
                    score: pattern_score,
                    modifiers: Vec::new(),
                }],
                ..Default::default()
            };

            assert_eq!(
                ranked_score_with_config("chrome", &[], &item, &HashMap::new(), &scoring),
                Some(expected_score)
            );
        }

        assert!(
            ranked_score_with_config("firefox", &[], &item, &HashMap::new(), &base_scoring)
                .is_none()
        );
    }

    #[test]
    fn program_files_aliases_follow_os_architecture_variables() {
        let program_files = path_aliases()
            .iter()
            .find(|alias| alias.name == "%PROGRAMFILES%")
            .unwrap();
        let program_files_x86 = path_aliases()
            .iter()
            .find(|alias| alias.name == "%PROGRAMFILES86%")
            .unwrap();
        let x64_values = HashMap::from([
            ("ProgramW6432", OsString::from(r"D:\Program Files")),
            ("ProgramFiles", OsString::from(r"D:\Program Files (x86)")),
            (
                "ProgramFiles(x86)",
                OsString::from(r"D:\Program Files (x86)"),
            ),
        ]);

        assert_eq!(
            alias_path_with_env(program_files, |key| x64_values.get(key).cloned()),
            Some(PathBuf::from(r"D:\Program Files"))
        );
        assert_eq!(
            alias_path_with_env(program_files_x86, |key| x64_values.get(key).cloned()),
            Some(PathBuf::from(r"D:\Program Files (x86)"))
        );

        let x86_values = HashMap::from([("ProgramFiles", OsString::from(r"E:\Program Files"))]);
        assert_eq!(
            alias_path_with_env(program_files, |key| x86_values.get(key).cloned()),
            Some(PathBuf::from(r"E:\Program Files"))
        );
        assert_eq!(
            alias_path_with_env(program_files_x86, |key| x86_values.get(key).cloned()),
            Some(PathBuf::from(r"E:\Program Files (x86)"))
        );
    }
}
