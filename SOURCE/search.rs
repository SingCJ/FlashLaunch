use rayon::ThreadPool;
use std::cmp::{Ordering as CmpOrdering, Reverse};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::plugins;
use crate::*;

#[cfg(test)]
const TEST_UNCANCELLED_GENERATION: u64 = 0xf1a5_1a00_0000_0001;
#[cfg(test)]
static REFINEMENT_TEST_MATCH_DELAY_MS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static BRANCH_PRUNE_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static BRANCH_PRUNING_DISABLED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResultBoundaryShortcut {
    First,
    Last,
}

fn result_boundary_shortcut(key: u16, control_pressed: bool) -> Option<ResultBoundaryShortcut> {
    if !control_pressed {
        return None;
    }
    match key {
        VK_HOME => Some(ResultBoundaryShortcut::First),
        VK_END => Some(ResultBoundaryShortcut::Last),
        _ => None,
    }
}

fn should_route_launcher_key_to_search(
    key: u16,
    control_pressed: bool,
    alt_pressed: bool,
    windows_pressed: bool,
) -> bool {
    if control_pressed || alt_pressed || windows_pressed {
        return false;
    }
    if matches!(
        key,
        VK_BACK | VK_DELETE | VK_LEFT | VK_RIGHT | VK_HOME | VK_END | VK_PROCESSKEY
    ) {
        return true;
    }
    let mapped = unsafe { MapVirtualKeyW(key as u32, MAPVK_VK_TO_CHAR) } & 0x7fff_ffff;
    mapped >= 0x20
}

pub(crate) fn handle_key_message(message: &mut MSG) -> bool {
    if message.message != WM_KEYDOWN && message.message != WM_SYSKEYDOWN {
        return false;
    }

    with_app(|app| unsafe {
        if message.hwnd == app.cfg_hotkey && app.capture_hotkey_key(message.wParam as u16) {
            return true;
        }
        if message.hwnd == app.cfg_index && message.wParam as u16 == VK_SPACE {
            if app.settings_page != SettingsPage::QueryLaunchRules {
                app.toggle_selected_index_entry();
            }
            return true;
        }
        if message.hwnd == app.scoring_list && message.wParam as u16 == VK_SPACE {
            app.toggle_selected_scoring_rule();
            return true;
        }
        let is_launcher_control = message.hwnd == app.hwnd
            || message.hwnd == app.edit
            || message.hwnd == app.list
            || IsChild(app.hwnd, message.hwnd) != 0;
        if !is_launcher_control {
            return false;
        }

        let control_pressed = GetKeyState(VK_CONTROL as i32) < 0;
        let alt_pressed = GetKeyState(VK_MENU as i32) < 0;
        let windows_pressed =
            GetKeyState(VK_LWIN as i32) < 0 || GetKeyState(VK_RWIN as i32) < 0;
        if let Some(shortcut) = result_boundary_shortcut(message.wParam as u16, control_pressed) {
            match shortcut {
                ResultBoundaryShortcut::First => app.select_first_result(),
                ResultBoundaryShortcut::Last => app.select_last_visible_result(),
            }
            SetFocus(app.edit);
            return true;
        }

        if message.hwnd == app.edit && message.wParam as u16 == b'A' as u16 && control_pressed {
            SendMessageW(app.edit, EM_SETSEL, 0, -1);
            return true;
        }

        match message.wParam as u16 {
            VK_RETURN => {
                if GetKeyState(VK_SHIFT as i32) < 0 {
                    app.open_selected_target_folder();
                } else {
                    app.launch_selected();
                }
                true
            }
            VK_ESCAPE => {
                app.handle_escape_key();
                true
            }
            VK_UP => {
                if message.hwnd == app.edit && control_pressed {
                    return app.recall_search_history(-1);
                }
                app.move_selection(-1);
                SetFocus(app.edit);
                true
            }
            VK_DOWN => {
                if message.hwnd == app.edit && control_pressed {
                    return app.recall_search_history(1);
                }
                app.move_selection(1);
                SetFocus(app.edit);
                true
            }
            VK_PRIOR => {
                app.move_selection_by_page(-1);
                SetFocus(app.edit);
                true
            }
            VK_NEXT => {
                if control_pressed {
                    app.toggle_show_all_results();
                } else {
                    app.move_selection_by_page(1);
                }
                SetFocus(app.edit);
                true
            }
            key if (VK_F1..=VK_F1 + 8).contains(&key) => {
                app.launch_result((key - VK_F1) as usize, true);
                true
            }
            key => {
                if message.message == WM_KEYDOWN
                    && message.hwnd != app.edit
                    && should_route_launcher_key_to_search(
                        key,
                        control_pressed,
                        alt_pressed,
                        windows_pressed,
                    )
                {
                    SetFocus(app.edit);
                    message.hwnd = app.edit;
                }
                false
            }
        }
    })
    .unwrap_or(false)
}

pub(crate) fn search_bar_help_text(
    plugin_state: &plugins::PluginState,
    language: AppLanguage,
) -> String {
    let calculator_alias = plugin_state
        .alias_entries()
        .into_iter()
        .find(|entry| entry.id == "calculator")
        .map(|entry| entry.alias)
        .unwrap_or_else(|| "/c".to_string());
    let alias_example = format!("{} 2+2", calculator_alias);
    let alias_history = calculator_alias.clone();
    let path_aliases = path_aliases()
        .iter()
        .map(|alias| alias.name)
        .collect::<Vec<_>>()
        .join(", ");

    let rows = [
        ("+sall", localized(language, "Show all matches.")),
        (
            "C:\\Folder\\filter",
            localized(
                language,
                "Browse a local directory and filter its children.",
            ),
        ),
        (
            "\\\\server\\share\\filter",
            localized(language, "Browse a UNC/network directory."),
        ),
        (
            "%MYSTARTMENU%\\chrome",
            localized(language, "Browse by using a built-in path alias."),
        ),
        (
            alias_example.as_str(),
            localized(language, "Calculator plugin: evaluate an expression."),
        ),
        (
            alias_history.as_str(),
            localized(language, "Calculator plugin: show calculator history."),
        ),
        (
            "Ctrl+Up / Ctrl+Down",
            localized(language, "Recall learned queries from Query Launch Rules."),
        ),
        (
            "Enter",
            localized(language, "Launch the selected or default result."),
        ),
        (
            "Shift+Enter",
            localized(language, "Open the selected item's real target folder, not the shortcut folder."),
        ),
        (
            "Esc",
            localized(
                language,
                "Select the query text, or hide when already selected or empty.",
            ),
        ),
        (
            "Up / Down",
            localized(language, "Move the result selection."),
        ),
        (
            "PageUp / PageDown",
            localized(language, "Move the result selection by one visible page."),
        ),
        (
            "Ctrl+Home / Ctrl+End",
            localized(language, "Select the first / last result."),
        ),
        (
            "Ctrl+PageDown",
            localized(
                language,
                "Toggle showing all matches without changing the search text.",
            ),
        ),
        (
            "F1..F9",
            localized(language, "Launch result 1..9 directly."),
        ),
    ];

    let mut lines = vec![
        format!(
            "{:<34} | {}",
            localized(language, "Input"),
            localized(language, "Action")
        ),
        format!("{}-+-{}", "-".repeat(34), "-".repeat(54)),
    ];
    for (input, note) in rows {
        lines.push(format!("{input:<34} | {note}"));
    }
    lines.push(String::new());
    lines.push(format!(
        "{} {}",
        localized(language, "Path aliases:"),
        path_aliases
    ));
    lines.push(String::new());
    lines.push(format!(
        "{} {}",
        localized(language, "Plugin aliases:"),
        plugins::help_text(plugin_state, language)
    ));
    lines.join("\r\n")
}

#[cfg(test)]
pub(crate) fn collect_results(
    query: &str,
    items: &[LaunchItem],
    recent_items: &[String],
    plugin_state: &plugins::PluginState,
) -> Vec<SearchResult> {
    collect_results_with_config(
        query,
        items,
        recent_items,
        plugin_state,
        &ScoringConfig::default(),
        usize::MAX,
    )
}

pub(crate) fn collect_results_with_config(
    query: &str,
    items: &[LaunchItem],
    recent_items: &[String],
    plugin_state: &plugins::PluginState,
    scoring: &ScoringConfig,
    effective_limit: usize,
) -> Vec<SearchResult> {
    let effective_limit = effective_limit.max(1);
    let trimmed = query.trim();
    if let Some(plugin_results) = plugins::collect_plugin_results(trimmed, plugin_state) {
        return plugin_results
            .into_iter()
            .take(effective_limit)
            .map(|result| SearchResult {
                title: result.title,
                subtitle: result.subtitle,
                target: LaunchTarget::Plugin(plugins::PluginTarget {
                    plugin_id: "calculator".to_string(),
                    action_token: result.action_token,
                }),
                is_dir: false,
                from_history: false,
                from_query_launch_rule: false,
                ranking_kind: ResultRankingKind::Plugin,
                explanation: Some(special_score_explanation(
                    ResultRankingKind::Plugin,
                    trimmed,
                    "",
                    result.score,
                )),
                display_score: 0,
                score_detail: "plugin".to_string(),
                score: result.score,
            })
            .collect();
    }
    let spec = parse_search_query(trimmed).effective_for_scoring(scoring);
    let searchable_query = spec.folded_search_text.as_str();
    if searchable_query.is_empty() && spec.modifiers.is_empty() {
        return collect_recent_order_results(items, recent_items, effective_limit);
    }
    let recent_map = recent_lookup(recent_items);
    let item_by_key = items
        .iter()
        .map(|item| (recent_item_key(&item.path), item))
        .collect::<HashMap<_, _>>();
    let mut results = Vec::new();
    let mut seen = HashSet::new();

    for recent in recent_items {
        let Some(key) = recent_entry_key(recent) else {
            continue;
        };
        let Some(item) = item_by_key.get(&key).copied() else {
            continue;
        };
        if !seen.insert(key) {
            continue;
        }
        if let Some(breakdown) =
            score_breakdown_with_config(item, &spec, &recent_map, scoring, true)
        {
            results.push(breakdown.result);
        }
    }

    for item in items {
        let key = recent_item_key(&item.path);
        if !seen.insert(key) {
            continue;
        }
        if let Some(breakdown) =
            score_breakdown_with_config(item, &spec, &recent_map, scoring, false)
        {
            results.push(breakdown.result);
        }
    }

    sort_search_results(&mut results);
    results.truncate(effective_limit);
    results
}

pub(crate) fn collect_recent_order_results(
    items: &[LaunchItem],
    recent_items: &[String],
    effective_limit: usize,
) -> Vec<SearchResult> {
    let effective_limit = effective_limit.max(1);
    let by_key = items
        .iter()
        .map(|item| (recent_item_key(&item.path), item))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::new();
    recent_items
        .iter()
        .filter_map(|recent| {
            let key = recent_entry_key(recent)?;
            if !seen.insert(key.clone()) {
                return None;
            }
            let item = by_key.get(&key)?;
            Some(SearchResult {
                title: item.title.clone(),
                subtitle: item.subtitle.clone(),
                target: LaunchTarget::Path(item.path.clone()),
                is_dir: item.is_dir,
                from_history: true,
                from_query_launch_rule: false,
                ranking_kind: ResultRankingKind::RecentOrder,
                explanation: Some(special_item_score_explanation(
                    ResultRankingKind::RecentOrder,
                    "",
                    *item,
                    0,
                )),
                display_score: 0,
                score_detail: "recent order".to_string(),
                score: 0,
            })
        })
        .take(effective_limit)
        .collect()
}

fn collect_recent_path_order_results_with_explanation(
    recent_items: &[String],
    effective_limit: usize,
    include_explanation: bool,
) -> Vec<SearchResult> {
    let effective_limit = effective_limit.max(1);
    let mut seen = HashSet::new();
    recent_items
        .iter()
        .filter_map(|recent| {
            let (_, key, raw_path) = parse_recent_entry(recent)?;
            if !seen.insert(key.clone()) {
                return None;
            }
            let path = PathBuf::from(raw_path);
            if !path.exists() {
                return None;
            }
            let title = path_display_name(&path);
            let subtitle = path
                .parent()
                .map(|parent| parent.to_string_lossy().to_string())
                .unwrap_or_default();
            Some(SearchResult {
                title,
                subtitle,
                target: LaunchTarget::Path(path.clone()),
                is_dir: path.is_dir(),
                from_history: true,
                from_query_launch_rule: false,
                ranking_kind: ResultRankingKind::RecentOrder,
                explanation: include_explanation.then(|| {
                    special_score_explanation(
                        ResultRankingKind::RecentOrder,
                        "",
                        &path.to_string_lossy(),
                        0,
                    )
                }),
                display_score: 0,
                score_detail: "recent order".to_string(),
                score: 0,
            })
        })
        .take(effective_limit)
        .collect()
}

fn collect_recent_path_matches_with_detail(
    spec: &SearchQuerySpec,
    root_plan: &RootOwnershipPlan,
    recent_items: &[String],
    recent_map: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
    limit: usize,
    include_score_detail: bool,
    include_explanation: bool,
) -> Vec<SearchResult> {
    collect_recent_path_match_breakdowns_with_detail(
        spec,
        root_plan,
        recent_items,
        recent_map,
        scoring,
        limit,
        include_score_detail,
        include_explanation,
    )
    .into_iter()
    .map(|breakdown| breakdown.result)
    .collect()
}

#[cfg(any(test, feature = "debug-tools"))]
pub(crate) fn collect_recent_path_match_breakdowns(
    spec: &SearchQuerySpec,
    root_plan: &RootOwnershipPlan,
    recent_items: &[String],
    recent_map: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
    limit: usize,
) -> Vec<ScoreBreakdown> {
    collect_recent_path_match_breakdowns_with_detail(
        spec,
        root_plan,
        recent_items,
        recent_map,
        scoring,
        limit,
        true,
        true,
    )
}

fn collect_recent_path_match_breakdowns_with_detail(
    spec: &SearchQuerySpec,
    root_plan: &RootOwnershipPlan,
    recent_items: &[String],
    recent_map: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
    limit: usize,
    include_score_detail: bool,
    include_explanation: bool,
) -> Vec<ScoreBreakdown> {
    let any_root_modifier_match = any_modifier_keywords_match(
        root_plan
            .entries()
            .iter()
            .map(|entry| entry.root.keywords.as_slice()),
        &spec.modifiers,
    );
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for recent in recent_items {
        let Some((_, key, raw_path)) = parse_recent_entry(recent) else {
            continue;
        };
        if !seen.insert(key) {
            continue;
        }
        let path = PathBuf::from(raw_path);
        if !path.exists() {
            continue;
        }
        let root = deepest_eligible_root_for_path(root_plan, &path, spec, any_root_modifier_match)
            .map(|entry| entry.root.clone())
            .unwrap_or_else(|| recent_path_root(&path, root_plan));
        if !root_matches_query_modifiers(&root, &spec.modifiers, any_root_modifier_match) {
            continue;
        }
        if !path_is_within_root_depth(&path, &root) {
            continue;
        }
        let Some(item) = launch_item_from_path(path, &root, scoring) else {
            continue;
        };
        let Some(breakdown) = score_breakdown_with_config_and_detail(
            &item,
            spec,
            recent_map,
            scoring,
            true,
            include_score_detail,
            include_explanation,
        ) else {
            continue;
        };
        results.push(breakdown);
    }
    results.sort_by_cached_key(|breakdown| {
        (
            Reverse(breakdown.result.from_query_launch_rule),
            Reverse(breakdown.result.score),
            breakdown.result.title.to_lowercase(),
        )
    });
    results.truncate(limit.max(1));
    results
}

pub(crate) fn recent_path_root(path: &Path, root_plan: &RootOwnershipPlan) -> IndexRoot {
    root_plan
        .owner(path)
        .map(|entry| entry.root.clone())
        .unwrap_or_else(|| IndexRoot {
            raw: path
                .parent()
                .map(|parent| parent.to_string_lossy().to_string())
                .unwrap_or_default(),
            path: path.parent().map(Path::to_path_buf),
            enabled: true,
            score: 0,
            max_depth: DEFAULT_SEARCH_DEPTH,
            label: String::new(),
            keywords: Vec::new(),
        })
}

fn ranking_priority(kind: ResultRankingKind) -> u8 {
    match kind {
        ResultRankingKind::QueryLaunchRule => 3,
        ResultRankingKind::Plugin => 2,
        ResultRankingKind::RecentOrder => 1,
        ResultRankingKind::Heuristic => 0,
    }
}

fn search_result_sort_key(result: &SearchResult) -> (Reverse<u8>, Reverse<i32>, String, String) {
    (
        Reverse(ranking_priority(result.ranking_kind)),
        Reverse(result.score),
        result.title.to_lowercase(),
        result.subtitle.to_lowercase(),
    )
}

pub(crate) fn sort_search_results(results: &mut [SearchResult]) {
    results.sort_by_cached_key(search_result_sort_key);
}

fn compare_search_results(left: &SearchResult, right: &SearchResult) -> CmpOrdering {
    search_result_sort_key(left).cmp(&search_result_sort_key(right))
}

fn ranked_candidate_sort_key(
    candidate: &RankedCandidate,
) -> (Reverse<u8>, Reverse<i32>, String, String) {
    (
        Reverse(ranking_priority(candidate.ranking_kind)),
        Reverse(candidate.score),
        candidate.title().to_lowercase(),
        candidate.subtitle().to_lowercase(),
    )
}

fn compare_ranked_candidates(left: &RankedCandidate, right: &RankedCandidate) -> CmpOrdering {
    ranked_candidate_sort_key(left).cmp(&ranked_candidate_sort_key(right))
}

fn ranked_candidate_from_prebuilt(result: SearchResult) -> RankedCandidate {
    RankedCandidate {
        ranking_kind: result.ranking_kind,
        display_score: result.display_score,
        score: result.score,
        payload: RankedCandidatePayload::Prebuilt(result),
    }
}

fn insert_bounded_ranked_candidate(
    results: &mut Vec<RankedCandidate>,
    candidate: RankedCandidate,
    effective_limit: usize,
) -> bool {
    if effective_limit == usize::MAX {
        results.push(candidate);
        return true;
    }
    let limit = effective_limit.max(1);
    if results.len() >= limit
        && results
            .last()
            .is_some_and(|worst| compare_ranked_candidates(&candidate, worst) != CmpOrdering::Less)
    {
        return false;
    }
    let insertion_index = results.partition_point(|current| {
        compare_ranked_candidates(current, &candidate) != CmpOrdering::Greater
    });
    results.insert(insertion_index, candidate);
    if results.len() > limit {
        results.pop();
    }
    true
}

fn primary_ranked_candidate_rank(candidate: &RankedCandidate) -> u64 {
    let priority = u64::from(candidate.ranking_kind == ResultRankingKind::QueryLaunchRule);
    let score = (i64::from(candidate.score) - i64::from(i32::MIN)) as u64;
    ((priority << 32) | score).saturating_add(1)
}

fn bounded_ranked_threshold(results: &[RankedCandidate], effective_limit: usize) -> u64 {
    if effective_limit == usize::MAX || results.len() < effective_limit.max(1) {
        return 0;
    }
    results
        .last()
        .map(primary_ranked_candidate_rank)
        .unwrap_or(0)
}

fn heuristic_primary_rank(score: i32) -> u64 {
    let shifted = (i64::from(score) - i64::from(i32::MIN)) as u64;
    shifted.saturating_add(1)
}

fn heuristic_score_upper_bound(
    item: &LaunchItem,
    text_score: i32,
    prepared_query: &PreparedQuery,
    scoring: &ScoringConfig,
) -> i32 {
    let path_score_ceiling = scoring.explicit_folder_name_match_adjustment.max(0);
    let history_ceiling = scoring.recent_score_ceiling.max(0);
    let recency_ceiling = if scoring.recency_date_enabled {
        scoring.recency_date_bonus.max(0)
    } else {
        0
    };
    let file_score = text_score
        .saturating_add(path_score_ceiling)
        .saturating_add(item.index_score)
        .saturating_add(prepared_query.positive_pattern_score_ceiling())
        .saturating_add(history_ceiling)
        .saturating_add(recency_ceiling)
        .saturating_sub(path_penalty(item, scoring));
    file_score.saturating_add(folder_score_adjustment(item, file_score, scoring))
}

fn should_branch_prune(upper_bound: i32, threshold: u64) -> bool {
    #[cfg(test)]
    if BRANCH_PRUNING_DISABLED.load(Ordering::Relaxed) {
        return false;
    }
    threshold != 0 && heuristic_primary_rank(upper_bound) < threshold
}

fn insert_bounded_search_result(
    results: &mut Vec<SearchResult>,
    candidate: SearchResult,
    effective_limit: usize,
) -> bool {
    if effective_limit == usize::MAX {
        results.push(candidate);
        return true;
    }

    let limit = effective_limit.max(1);
    if results.len() >= limit
        && results
            .last()
            .is_some_and(|worst| compare_search_results(&candidate, worst) != CmpOrdering::Less)
    {
        return false;
    }

    let insertion_index = results.partition_point(|current| {
        compare_search_results(current, &candidate) != CmpOrdering::Greater
    });
    results.insert(insertion_index, candidate);
    if results.len() > limit {
        results.pop();
    }
    true
}

fn normalize_bounded_search_results(results: &mut Vec<SearchResult>, effective_limit: usize) {
    if effective_limit == usize::MAX {
        return;
    }
    let mut bounded = Vec::with_capacity(results.len().min(effective_limit.max(1)));
    for result in results.drain(..) {
        insert_bounded_search_result(&mut bounded, result, effective_limit);
    }
    *results = bounded;
}

fn primary_search_rank(result: &SearchResult) -> u64 {
    let priority = u64::from(result.from_query_launch_rule);
    let score = (i64::from(result.score) - i64::from(i32::MIN)) as u64;
    ((priority << 32) | score).saturating_add(1)
}

fn bounded_search_threshold(results: &[SearchResult], effective_limit: usize) -> u64 {
    if effective_limit == usize::MAX || results.len() < effective_limit.max(1) {
        return 0;
    }
    results.last().map(primary_search_rank).unwrap_or(0)
}

pub(crate) fn first_visible_result_index(
    result_count: usize,
    visible_limit: usize,
) -> Option<usize> {
    (result_count > 0 && visible_limit > 0).then_some(0)
}

pub(crate) fn selection_after_move(
    current: Option<usize>,
    count: usize,
    delta: i32,
) -> Option<usize> {
    if count == 0 {
        return None;
    }
    match current {
        Some(index) => Some((index as i32 + delta).rem_euclid(count as i32) as usize),
        None if delta < 0 => Some(count - 1),
        None => Some(0),
    }
}

pub(crate) fn top_index_after_select(
    current_top: usize,
    selected: usize,
    count: usize,
    visible_rows: usize,
) -> usize {
    if count == 0 {
        return 0;
    }
    let rows = visible_rows.max(1).min(count);
    let max_top = count.saturating_sub(rows);
    let mut top = current_top.min(max_top);
    if selected < top {
        top = selected.min(max_top);
    } else if selected >= top + rows {
        top = (selected + 1).saturating_sub(rows).min(max_top);
    }
    top
}

pub(crate) fn page_selection_after_move(
    current: Option<usize>,
    current_top: usize,
    count: usize,
    visible_rows: usize,
    direction: i32,
) -> Option<(usize, usize)> {
    if count == 0 {
        return None;
    }
    let rows = visible_rows.max(1).min(count);
    let max_top = count.saturating_sub(rows);
    let current_top = current_top.min(max_top);
    let Some(current) = current else {
        return if direction < 0 {
            Some((count - 1, max_top))
        } else {
            Some((0, 0))
        };
    };
    let current = current.min(count - 1);
    if direction < 0 {
        Some((
            current.saturating_sub(rows),
            current_top.saturating_sub(rows),
        ))
    } else {
        Some(((current + rows).min(count - 1), (current_top + rows).min(max_top)))
    }
}

pub(crate) fn parse_search_query(raw: &str) -> SearchQuerySpec {
    let raw_trimmed = raw.trim().to_string();
    let scoring_time_unix_seconds = current_unix_seconds();
    let mut text_parts = Vec::new();
    let mut modifiers = Vec::new();
    let mut show_all = false;

    for part in raw_trimmed.split_whitespace() {
        if let Some(keyword) = part.strip_prefix('+') {
            let keyword = keyword.trim();
            if keyword.is_empty() {
                continue;
            }
            let folded_keyword = fold_text(keyword).trim().to_string();
            if folded_keyword.eq_ignore_ascii_case("sall") {
                show_all = true;
            } else if !folded_keyword.is_empty() && !modifiers.contains(&folded_keyword) {
                modifiers.push(folded_keyword);
            }
        } else {
            text_parts.push(part);
        }
    }

    let search_text = text_parts.join(" ");
    let folded_search_text = fold_text(&search_text);
    let scoring_modifiers = query_scoring_modifiers(&folded_search_text, &modifiers);
    if raw_trimmed.is_empty()
        || (search_text.trim().is_empty() && !show_all && modifiers.is_empty())
    {
        return SearchQuerySpec {
            raw: raw_trimmed,
            search_text,
            folded_search_text,
            modifiers,
            scoring_modifiers,
            show_all,
            mode: SearchQueryMode::BlankHistory,
            directory: None,
            scoring_time_unix_seconds,
        };
    }

    if let Some(directory) = parse_directory_browse_spec(&search_text) {
        return SearchQuerySpec {
            raw: raw_trimmed,
            search_text,
            folded_search_text,
            modifiers,
            scoring_modifiers,
            show_all,
            mode: SearchQueryMode::DirectoryBrowse,
            directory: Some(directory),
            scoring_time_unix_seconds,
        };
    }

    SearchQuerySpec {
        raw: raw_trimmed,
        search_text,
        folded_search_text,
        modifiers,
        scoring_modifiers,
        show_all,
        mode: SearchQueryMode::NormalSearch,
        directory: None,
        scoring_time_unix_seconds,
    }
}

pub(crate) fn query_scoring_modifiers(
    _folded_search_text: &str,
    explicit_modifiers: &[String],
) -> Vec<String> {
    explicit_modifiers.to_vec()
}

pub(crate) fn effective_search_text_for_scoring(
    spec: &SearchQuerySpec,
    _scoring: &ScoringConfig,
) -> String {
    spec.folded_search_text.clone()
}

pub(crate) fn parse_directory_browse_spec(search_text: &str) -> Option<DirectoryBrowseSpec> {
    let value = search_text.trim().trim_matches('"');
    if value.is_empty() || !looks_like_directory_query(value) {
        return None;
    }
    let expanded = expand_path(value);
    if expanded.is_dir() {
        return Some(DirectoryBrowseSpec {
            base: expanded,
            filter: String::new(),
        });
    }
    let base = expanded.parent()?.to_path_buf();
    if !base.is_dir() {
        return None;
    }
    let filter = expanded
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    Some(DirectoryBrowseSpec { base, filter })
}

pub(crate) fn looks_like_directory_query(value: &str) -> bool {
    let bytes = value.as_bytes();
    if value.starts_with("\\\\") {
        return true;
    }
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return true;
    }
    path_aliases().iter().any(|alias| {
        value
            .get(..alias.name.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(alias.name))
    })
}

pub(crate) fn search_history_path() -> PathBuf {
    app_config_dir().join("search_history.txt")
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn load_search_history(plugin_state: &plugins::PluginState) -> Vec<String> {
    let path = search_history_path();
    let Some(content) = ensure_optional_text_file("search_history.txt", &path, "", |content, _| {
        content.to_string()
    }) else {
        return Vec::new();
    };
    normalize_search_history_items(
        content.lines().filter(|line| {
            let line = line.trim();
            !line.starts_with('#') && !line.starts_with(';')
        }),
        plugin_state,
    )
}
pub(crate) fn search_history_items_to_text(items: &[String]) -> String {
    let mut content = items.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    content
}

pub(crate) fn query_launch_rules_path() -> PathBuf {
    app_config_dir().join("query_launch_rules.txt")
}

pub(crate) fn load_query_launch_rules() -> Vec<QueryLaunchRule> {
    let path = query_launch_rules_path();
    let Some(content) =
        ensure_optional_text_file("query_launch_rules.txt", &path, "", |content, _| {
            content.to_string()
        })
    else {
        return Vec::new();
    };
    normalize_query_launch_rules(content.lines().filter_map(parse_query_launch_rule))
}

pub(crate) fn save_query_launch_rules(items: &[QueryLaunchRule]) -> std::io::Result<()> {
    let path = query_launch_rules_path();
    let Some(existing) =
        ensure_optional_text_file("query_launch_rules.txt", &path, "", |content, _| {
            content.to_string()
        })
    else {
        return Ok(());
    };
    let content = merge_query_launch_rules_text(&existing, items);
    write_optional_text("query_launch_rules.txt", &path, &content);
    Ok(())
}

pub(crate) fn query_launch_rules_to_text(items: &[QueryLaunchRule]) -> String {
    let mut content = String::new();
    for item in items {
        if item.query.trim().is_empty() || item.target.trim().is_empty() {
            continue;
        }
        content.push_str(&item.query.replace(['\t', '\r', '\n'], " "));
        content.push('\t');
        content.push_str(&item.target.replace(['\t', '\r', '\n'], " "));
        content.push('\n');
    }
    content
}

fn merge_document_lines(
    existing: &str,
    replacement: &str,
    mut is_known: impl FnMut(&str) -> bool,
) -> String {
    let replacement_lines = replacement.lines().map(str::to_string).collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut inserted = false;
    for line in existing.lines() {
        if is_known(line) {
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

pub(crate) fn merge_search_history_items_text(existing: &str, items: &[String]) -> String {
    merge_document_lines(existing, &search_history_items_to_text(items), |line| {
        let line = line.trim();
        !line.is_empty() && !line.starts_with('#') && !line.starts_with(';')
    })
}

pub(crate) fn merge_query_launch_rules_text(existing: &str, items: &[QueryLaunchRule]) -> String {
    merge_document_lines(existing, &query_launch_rules_to_text(items), |line| {
        parse_query_launch_rule(line).is_some()
    })
}

pub(crate) fn parse_query_launch_rule(line: &str) -> Option<QueryLaunchRule> {
    let (query, target) = line.split_once('\t')?;
    let query = query.trim();
    let target = target.trim();
    (!query.is_empty() && !target.is_empty()).then_some(QueryLaunchRule {
        query: query.to_string(),
        target: target.to_string(),
    })
}

pub(crate) fn normalize_query_launch_rules<I>(items: I) -> Vec<QueryLaunchRule>
where
    I: IntoIterator<Item = QueryLaunchRule>,
{
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for item in items {
        let query = item.query.trim();
        let target = item.target.trim();
        if query.is_empty() || target.is_empty() || !seen.insert(fold_text(query)) {
            continue;
        }
        normalized.push(QueryLaunchRule {
            query: query.to_string(),
            target: target.to_string(),
        });
        if normalized.len() >= RECENT_LIMIT {
            break;
        }
    }
    normalized
}
pub(crate) fn record_query_launch_rule_in_memory(
    rules: &mut Vec<QueryLaunchRule>,
    query: &str,
    target: &Path,
    plugin_state: &plugins::PluginState,
) -> bool {
    let before = rules.clone();
    let query = query.trim();
    if query.is_empty() || plugin_state.is_plugin_query(query) {
        return false;
    }
    upsert_query_launch_rule(rules, query, &target.to_string_lossy());
    *rules != before
}

pub(crate) fn sync_query_launch_rules_model_from_runtime(
    config_rules: &mut Vec<QueryLaunchRule>,
    runtime_rules: &[QueryLaunchRule],
) {
    if config_rules != runtime_rules {
        *config_rules = runtime_rules.to_vec();
    }
}

pub(crate) fn upsert_query_launch_rule(
    rules: &mut Vec<QueryLaunchRule>,
    query: &str,
    target: &str,
) {
    let query = query.trim();
    let target = target.trim();
    if query.is_empty() || target.is_empty() {
        return;
    }
    let key = fold_text(query);
    rules.retain(|item| fold_text(&item.query) != key);
    rules.insert(
        0,
        QueryLaunchRule {
            query: query.to_string(),
            target: target.to_string(),
        },
    );
    rules.truncate(RECENT_LIMIT);
}

pub(crate) fn remove_query_launch_rule(
    rules: &mut Vec<QueryLaunchRule>,
    query: &str,
    target: &Path,
) -> bool {
    let query_key = fold_text(query.trim());
    if query_key.is_empty() {
        return false;
    }
    let target_key = query_launch_rule_target_key(&target.to_string_lossy());
    let before = rules.len();
    rules.retain(|item| {
        fold_text(&item.query) != query_key
            || query_launch_rule_target_key(&item.target) != target_key
    });
    rules.len() != before
}

fn query_launch_rule_target_key(value: &str) -> String {
    fold_text(&value.trim().replace('/', "\\"))
}

pub(crate) fn query_launch_rule_visible_indices(
    rules: &[QueryLaunchRule],
    filter: &str,
) -> Vec<usize> {
    rules
        .iter()
        .enumerate()
        .filter_map(|(index, rule)| query_launch_rule_matches_filter(rule, filter).then_some(index))
        .collect()
}

pub(crate) fn query_launch_rule_matches_filter(rule: &QueryLaunchRule, filter: &str) -> bool {
    let tokens = query_launch_rule_filter_tokens(filter);
    if tokens.is_empty() {
        return true;
    }
    let searchable = searchable_text(&format!("{} {}", rule.query, rule.target));
    tokens.iter().all(|token| searchable.contains(token))
}

pub(crate) fn query_launch_rule_filter_tokens(filter: &str) -> Vec<String> {
    searchable_text(filter)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

pub(crate) fn query_launch_rule_actual_indices_for_visible_selection(
    visible_indices: &[usize],
    selected_visible_indices: &[usize],
) -> Vec<usize> {
    selected_visible_indices
        .iter()
        .filter_map(|visible| visible_indices.get(*visible).copied())
        .collect()
}

pub(crate) fn delete_query_launch_rules_by_indices(
    rules: &mut Vec<QueryLaunchRule>,
    indices: &[usize],
) -> usize {
    let mut indices = indices.to_vec();
    indices.sort_unstable_by(|a, b| b.cmp(a));
    indices.dedup();
    let mut deleted = 0usize;
    for index in indices {
        if index < rules.len() {
            rules.remove(index);
            deleted += 1;
        }
    }
    deleted
}

pub(crate) fn query_launch_rule_selection_count_text(
    language: AppLanguage,
    selected: usize,
    showing: usize,
    total: usize,
) -> String {
    localized_format3(
        language,
        "Selected: {} | Showing: {} | Total: {}",
        selected,
        showing,
        total,
    )
}

pub(crate) fn query_launch_rule_for_query<'a>(
    rules: &'a [QueryLaunchRule],
    query: &str,
) -> Option<&'a QueryLaunchRule> {
    let key = fold_text(query.trim());
    if key.is_empty() {
        return None;
    }
    rules.iter().find(|item| fold_text(&item.query) == key)
}

fn query_launch_rule_result_with_explanation(
    query: &str,
    rules: &[QueryLaunchRule],
    scoring: &ScoringConfig,
    include_explanation: bool,
) -> Option<SearchResult> {
    let rule = query_launch_rule_for_query(rules, query)?;
    let path = PathBuf::from(rule.target.trim());
    if !path.exists() {
        return None;
    }
    let root = IndexRoot {
        raw: path
            .parent()
            .map(|parent| parent.to_string_lossy().to_string())
            .unwrap_or_default(),
        path: path.parent().map(Path::to_path_buf),
        enabled: true,
        score: 0,
        max_depth: DEFAULT_SEARCH_DEPTH,
        label: "Query Launch Rules".to_string(),
        keywords: Vec::new(),
    };
    let item = launch_item_from_path(path, &root, scoring)?;
    Some(SearchResult {
        title: item.title.clone(),
        subtitle: item.subtitle.clone(),
        target: LaunchTarget::Path(item.path.clone()),
        is_dir: item.is_dir,
        from_history: true,
        from_query_launch_rule: true,
        ranking_kind: ResultRankingKind::QueryLaunchRule,
        explanation: include_explanation.then(|| {
            special_item_score_explanation(
                ResultRankingKind::QueryLaunchRule,
                "",
                &item,
                item.index_score,
            )
        }),
        display_score: item.index_score,
        score_detail: "starred".to_string(),
        score: item.index_score,
    })
}

pub(crate) fn prioritize_query_launch_rule_result(
    results: &mut Vec<SearchResult>,
    query: &str,
    rules: &[QueryLaunchRule],
    scoring: &ScoringConfig,
) {
    prioritize_query_launch_rule_result_with_explanation(results, query, rules, scoring, true);
}

fn prioritize_query_launch_rule_result_with_explanation(
    results: &mut Vec<SearchResult>,
    query: &str,
    rules: &[QueryLaunchRule],
    scoring: &ScoringConfig,
    include_explanation: bool,
) {
    let Some(priority) =
        query_launch_rule_result_with_explanation(query, rules, scoring, include_explanation)
    else {
        return;
    };
    let Some(priority_path) = result_target_path(&priority) else {
        return;
    };
    let priority_key = recent_item_key(&priority_path);
    results.retain(|result| {
        result_target_path(result)
            .map(|path| recent_item_key(&path) != priority_key)
            .unwrap_or(true)
    });
    results.insert(0, priority);
}
pub(crate) fn normalize_search_history_items<I, S>(
    items: I,
    plugin_state: &plugins::PluginState,
) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for item in items {
        let line = item.as_ref().trim();
        if line.is_empty() || plugin_state.is_plugin_query(line) || !seen.insert(fold_text(line)) {
            continue;
        }
        normalized.push(line.to_string());
        if normalized.len() >= SEARCH_HISTORY_LIMIT {
            break;
        }
    }
    normalized
}

#[cfg(feature = "debug-tools")]
#[derive(Clone)]
pub(crate) struct SearchBenchmarkResult {
    pub(crate) scanned_total: usize,
    pub(crate) workers: usize,
    pub(crate) live_scan_strategy: &'static str,
    pub(crate) results: Vec<SearchResult>,
}

#[cfg(feature = "debug-tools")]
impl SearchBenchmarkResult {
    pub(crate) fn result_count(&self) -> usize {
        self.results.len()
    }
}

#[derive(Clone)]
struct EligibleSearchRoot {
    path: PathBuf,
    source_root: IndexRoot,
    effective_root: IndexRoot,
    exclusions: Arc<Vec<String>>,
    effective_depth: usize,
}

fn path_depth_from_root(path: &Path, root: &IndexRoot) -> Option<usize> {
    let root_path = root.path.as_deref()?;
    let path_key = normalized_root_path_key(path);
    let root_key = normalized_root_path_key(root_path);
    path_key_relative_depth(&path_key, &root_key)
}

fn path_is_within_root_depth(path: &Path, root: &IndexRoot) -> bool {
    path_depth_from_root(path, root).is_some_and(|depth| {
        root.max_depth == SEARCH_DEPTH_ALL || depth <= root.max_depth.saturating_add(1)
    })
}

fn deepest_eligible_root_for_path<'a>(
    root_plan: &'a RootOwnershipPlan,
    path: &Path,
    spec: &SearchQuerySpec,
    any_root_modifier_match: bool,
) -> Option<&'a RootOwnershipEntry> {
    let path_key = normalized_root_path_key(path);
    root_plan
        .entries()
        .iter()
        .filter(|entry| path_key_is_within(&path_key, &entry.key))
        .filter(|entry| {
            root_matches_query_modifiers(&entry.root, &spec.modifiers, any_root_modifier_match)
        })
        .max_by_key(|entry| entry.key.len())
}

fn eligible_search_roots(
    root_plan: &RootOwnershipPlan,
    spec: &SearchQuerySpec,
    any_root_modifier_match: bool,
) -> Vec<EligibleSearchRoot> {
    root_plan
        .entries()
        .iter()
        .filter_map(|entry| {
            let effective = deepest_eligible_root_for_path(
                root_plan,
                &entry.path,
                spec,
                any_root_modifier_match,
            )?;
            let effective_depth = path_depth_from_root(&entry.path, &effective.root)?;
            path_is_within_root_depth(&entry.path, &effective.root).then(|| EligibleSearchRoot {
                path: entry.path.clone(),
                source_root: entry.root.clone(),
                effective_root: effective.root.clone(),
                exclusions: Arc::clone(&entry.exclusions),
                effective_depth,
            })
        })
        .collect()
}

pub(crate) const REFINEMENT_CACHE_MAX_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RefinementMetrics {
    pub(crate) filesystem_entries: usize,
    pub(crate) cache_candidates: usize,
    pub(crate) cache_bytes: usize,
    pub(crate) reused_candidates: usize,
    pub(crate) full_scan_fallback: bool,
    pub(crate) query_version: u64,
    pub(crate) scan_done: bool,
}

#[cfg(feature = "debug-tools")]
#[derive(Clone, Debug)]
pub(crate) struct RefinementBenchmarkSample {
    pub(crate) query: String,
    pub(crate) elapsed_ms: f64,
    pub(crate) metrics: RefinementMetrics,
}

#[derive(Clone)]
struct PreparedCandidate {
    item: LaunchItem,
    path_key: String,
    estimated_bytes: usize,
}

impl PreparedCandidate {
    fn new(item: LaunchItem, path_key: String) -> Self {
        let estimated_bytes = std::mem::size_of::<Self>()
            .saturating_add(item.title.capacity())
            .saturating_add(item.subtitle.capacity())
            .saturating_add(item.path.as_os_str().len())
            .saturating_add(item.folded_title.capacity())
            .saturating_add(item.folded_stem.capacity())
            .saturating_add(item.folded_parent.capacity())
            .saturating_add(
                item.search_root
                    .as_ref()
                    .map(|path| path.as_os_str().len())
                    .unwrap_or(0),
            )
            .saturating_add(path_key.capacity());
        Self {
            item,
            path_key,
            estimated_bytes,
        }
    }
}

struct RefinementSessionState {
    request: SearchWorkerRequest,
    effective_spec: SearchQuerySpec,
    prepared_query: PreparedQuery,
    recent_map: HashMap<String, RecentEntry>,
    query_version: u64,
    cache: Vec<PreparedCandidate>,
    cache_bytes: usize,
    cache_limit: usize,
    reusable: bool,
    scan_seen: HashSet<String>,
    ranking_seen: HashSet<String>,
    best: Vec<RankedCandidate>,
    scanned_total: usize,
    reused_candidates: usize,
    scan_done: bool,
    shutdown: bool,
    last_publish: Instant,
}

struct RefinementShared {
    state: Mutex<RefinementSessionState>,
    changed: Condvar,
}

pub(crate) struct RefinementSession {
    shared: Arc<RefinementShared>,
    handle: Option<JoinHandle<()>>,
}

impl RefinementSession {
    pub(crate) fn start(request: SearchWorkerRequest) -> Self {
        Self::start_with_cache_limit(request, REFINEMENT_CACHE_MAX_BYTES)
    }

    fn start_with_cache_limit(request: SearchWorkerRequest, cache_limit: usize) -> Self {
        let scoring = Arc::clone(&request.scoring);
        let effective_spec = request.spec.effective_for_scoring(&scoring);
        let prepared_query = PreparedQuery::new(&effective_spec, &scoring);
        let recent_map = recent_lookup(&request.recent_items);
        let (best, ranking_seen) =
            refinement_initial_results(&request, &effective_spec, &recent_map);
        let shared = Arc::new(RefinementShared {
            state: Mutex::new(RefinementSessionState {
                request,
                effective_spec,
                prepared_query,
                recent_map,
                query_version: 1,
                cache: Vec::new(),
                cache_bytes: 0,
                cache_limit,
                reusable: true,
                scan_seen: HashSet::new(),
                ranking_seen,
                best,
                scanned_total: 0,
                reused_candidates: 0,
                scan_done: false,
                shutdown: false,
                last_publish: Instant::now(),
            }),
            changed: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("flashlaunch-refinement-session".to_string())
            .spawn(move || run_refinement_scan(worker_shared))
            .ok();
        Self { shared, handle }
    }

    pub(crate) fn can_refine(&self, request: &SearchWorkerRequest) -> bool {
        self.shared.state.lock().ok().is_some_and(|state| {
            !state.shutdown
                && state.reusable
                && refinement_requests_compatible(&state.request, request)
        })
    }

    pub(crate) fn update(&self, request: SearchWorkerRequest) -> bool {
        let publish = {
            let Ok(mut state) = self.shared.state.lock() else {
                return false;
            };
            if state.shutdown
                || !state.reusable
                || !refinement_requests_compatible(&state.request, &request)
            {
                return false;
            }
            state.query_version = state.query_version.wrapping_add(1).max(1);
            state.request = request;
            state.effective_spec = state
                .request
                .spec
                .effective_for_scoring(&state.request.scoring);
            state.prepared_query =
                PreparedQuery::new(&state.effective_spec, &state.request.scoring);
            state.reused_candidates = state.cache.len();
            rebuild_refinement_ranking(&mut state);
            refinement_publish_snapshot(&state, state.scan_done)
        };
        self.shared.changed.notify_all();
        publish_refinement_snapshot(publish);
        true
    }

    pub(crate) fn metrics(&self) -> RefinementMetrics {
        self.shared
            .state
            .lock()
            .ok()
            .map(|state| RefinementMetrics {
                filesystem_entries: state.scanned_total,
                cache_candidates: state.cache.len(),
                cache_bytes: state.cache_bytes,
                reused_candidates: state.reused_candidates,
                full_scan_fallback: !state.reusable,
                query_version: state.query_version,
                scan_done: state.scan_done,
            })
            .unwrap_or_default()
    }

    pub(crate) fn wait_until_complete(&self, timeout: Duration) -> bool {
        let Ok(state) = self.shared.state.lock() else {
            return false;
        };
        if state.scan_done {
            return true;
        }
        self.shared
            .changed
            .wait_timeout_while(state, timeout, |state| !state.scan_done && !state.shutdown)
            .ok()
            .is_some_and(|(state, _)| state.scan_done)
    }

    pub(crate) fn invalidate(&self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.shutdown = true;
            state.cache.clear();
            state.cache_bytes = 0;
            state.best.clear();
            state.scan_seen.clear();
            state.ranking_seen.clear();
            self.shared.changed.notify_all();
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.invalidate();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for RefinementSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn refinement_requests_compatible(
    previous: &SearchWorkerRequest,
    next: &SearchWorkerRequest,
) -> bool {
    if !query_specs_allow_refinement(&previous.spec, &next.spec)
        || previous.search_threads != next.search_threads
        || !Arc::ptr_eq(&previous.root_plan, &next.root_plan)
        || !Arc::ptr_eq(&previous.scoring, &next.scoring)
        || !Arc::ptr_eq(&previous.recent_items, &next.recent_items)
        || !Arc::ptr_eq(&previous.query_launch_rules, &next.query_launch_rules)
    {
        return false;
    }

    true
}

pub(crate) fn query_specs_allow_refinement(
    previous: &SearchQuerySpec,
    next: &SearchQuerySpec,
) -> bool {
    if previous.mode != SearchQueryMode::NormalSearch
        || next.mode != SearchQueryMode::NormalSearch
        || previous.modifiers != next.modifiers
        || previous.scoring_modifiers != next.scoring_modifiers
    {
        return false;
    }

    let previous_text = previous.folded_search_text.trim();
    let next_text = next.folded_search_text.trim();
    if previous_text == next_text {
        return true;
    }
    let previous_tokens = previous_text.split_whitespace().collect::<Vec<_>>();
    let next_tokens = next_text.split_whitespace().collect::<Vec<_>>();
    if previous_tokens.is_empty() || previous_tokens.len() != next_tokens.len() {
        return false;
    }
    let last = previous_tokens.len() - 1;
    previous_tokens[..last] == next_tokens[..last]
        && next_tokens[last].len() > previous_tokens[last].len()
        && next_tokens[last].starts_with(previous_tokens[last])
}

fn refinement_initial_results(
    request: &SearchWorkerRequest,
    spec: &SearchQuerySpec,
    recent_map: &HashMap<String, RecentEntry>,
) -> (Vec<RankedCandidate>, HashSet<String>) {
    let preview_limit = refinement_preview_limit(request.effective_limit);
    let initial_limit = if request.effective_limit == usize::MAX {
        usize::MAX
    } else {
        preview_limit
    };
    let mut results = collect_recent_path_matches_with_detail(
        spec,
        &request.root_plan,
        &request.recent_items,
        recent_map,
        &request.scoring,
        initial_limit,
        request.include_score_detail,
        request.include_explanation,
    );
    prioritize_query_launch_rule_result_with_explanation(
        &mut results,
        &request.spec.raw,
        &request.query_launch_rules,
        &request.scoring,
        request.include_explanation,
    );
    let mut seen = HashSet::new();
    let mut best = Vec::new();
    for result in results {
        if let Some(path) = result_target_path(&result) {
            seen.insert(recent_item_key(&path));
        }
        insert_bounded_ranked_candidate(
            &mut best,
            ranked_candidate_from_prebuilt(result),
            preview_limit,
        );
    }
    (best, seen)
}

fn refinement_preview_limit(effective_limit: usize) -> usize {
    if effective_limit == usize::MAX {
        DEFAULT_RESULT_LIMIT.max(1)
    } else {
        effective_limit.max(1)
    }
}

fn rebuild_refinement_ranking(state: &mut RefinementSessionState) {
    let (mut best, mut ranking_seen) =
        refinement_initial_results(&state.request, &state.effective_spec, &state.recent_map);
    let preview_limit = refinement_preview_limit(state.request.effective_limit);
    for candidate in &state.cache {
        if !ranking_seen.insert(candidate.path_key.clone()) {
            continue;
        }
        let Some(text_score) = state.prepared_query.score_candidate_name(
            &PreparedCandidateName {
                title: candidate.item.title.clone(),
                folded_title: candidate.item.folded_title.clone(),
                folded_stem: candidate.item.folded_stem.clone(),
            },
            candidate.item.folded_parent(),
            candidate.item.is_dir,
            &state.request.scoring,
        ) else {
            continue;
        };
        let components = score_components_with_prepared_query(
            &candidate.item,
            &state.effective_spec,
            &state.prepared_query,
            &candidate.path_key,
            text_score,
            &state.recent_map,
            &state.request.scoring,
        );
        insert_bounded_ranked_candidate(
            &mut best,
            ranked_heuristic_candidate(candidate.item.clone(), components, false),
            preview_limit,
        );
    }
    state.best = best;
    state.ranking_seen = ranking_seen;
}

struct RefinementPublish {
    request: SearchWorkerRequest,
    results: Vec<SearchResult>,
    all_results: Option<Vec<RankedCandidate>>,
    scanned_total: usize,
    done: bool,
}

fn refinement_publish_snapshot(state: &RefinementSessionState, done: bool) -> RefinementPublish {
    let all_results = if done && state.request.effective_limit == usize::MAX && state.reusable {
        Some(refinement_all_results(state))
    } else {
        None
    };
    RefinementPublish {
        request: state.request.clone(),
        results: materialize_ranked_candidates(
            &state.best,
            &state.effective_spec,
            &state.recent_map,
            &state.request.scoring,
            state.request.include_score_detail,
            state.request.include_explanation,
        ),
        all_results,
        scanned_total: state.scanned_total,
        done,
    }
}

fn refinement_all_results(state: &RefinementSessionState) -> Vec<RankedCandidate> {
    let mut results = collect_recent_path_matches_with_detail(
        &state.effective_spec,
        &state.request.root_plan,
        &state.request.recent_items,
        &state.recent_map,
        &state.request.scoring,
        usize::MAX,
        state.request.include_score_detail,
        false,
    );
    prioritize_query_launch_rule_result_with_explanation(
        &mut results,
        &state.request.spec.raw,
        &state.request.query_launch_rules,
        &state.request.scoring,
        false,
    );
    let mut seen = results
        .iter()
        .filter_map(result_target_path)
        .map(|path| recent_item_key(&path))
        .collect::<HashSet<_>>();
    let mut ranked_results = results
        .into_iter()
        .map(ranked_candidate_from_prebuilt)
        .collect::<Vec<_>>();
    for candidate in &state.cache {
        if !seen.insert(candidate.path_key.clone()) {
            continue;
        }
        let prepared_name = PreparedCandidateName {
            title: candidate.item.title.clone(),
            folded_title: candidate.item.folded_title.clone(),
            folded_stem: candidate.item.folded_stem.clone(),
        };
        let Some(text_score) = state.prepared_query.score_candidate_name(
            &prepared_name,
            candidate.item.folded_parent(),
            candidate.item.is_dir,
            &state.request.scoring,
        ) else {
            continue;
        };
        let components = score_components_with_prepared_query(
            &candidate.item,
            &state.effective_spec,
            &state.prepared_query,
            &candidate.path_key,
            text_score,
            &state.recent_map,
            &state.request.scoring,
        );
        ranked_results.push(ranked_heuristic_candidate(
            candidate.item.clone(),
            components,
            false,
        ));
    }
    ranked_results.sort_by_cached_key(ranked_candidate_sort_key);
    ranked_results
}

fn publish_refinement_snapshot(mut publish: RefinementPublish) {
    if publish.request.hwnd_value == 0 {
        return;
    }
    if publish.request.effective_limit == usize::MAX && publish.done {
        let Some(mut all_results) = publish.all_results.take() else {
            run_refinement_full_scan_fallback(publish.request);
            return;
        };
        match ResultStoreWriter::start(publish.request.generation) {
            Ok(writer) => {
                if let Some(sender) = writer.sender() {
                    for candidate in all_results.drain(..) {
                        if sender.send_ranked(candidate).is_err() {
                            break;
                        }
                    }
                }
                let completion = match writer.finish() {
                    Ok(manifest) => ResultStoreCompletion::Ready(manifest),
                    Err(error) => ResultStoreCompletion::Error(error.to_string()),
                };
                publish_search_batch_with_store(
                    publish.request.generation,
                    publish.request.hwnd_value,
                    publish.results,
                    publish.scanned_total,
                    SearchStage::Done,
                    publish.request.effective_limit,
                    completion,
                );
            }
            Err(error) => publish_search_batch_with_store(
                publish.request.generation,
                publish.request.hwnd_value,
                publish.results,
                publish.scanned_total,
                SearchStage::Done,
                publish.request.effective_limit,
                ResultStoreCompletion::Error(error.to_string()),
            ),
        }
    } else {
        publish_search_batch(
            publish.request.generation,
            publish.request.hwnd_value,
            publish.results,
            publish.scanned_total,
            if publish.done {
                SearchStage::Done
            } else {
                SearchStage::Folders
            },
            publish.done,
            publish.request.effective_limit,
        );
    }
}

fn run_refinement_full_scan_fallback(request: SearchWorkerRequest) {
    let scan_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(
            request
                .search_threads
                .clamp(MIN_SEARCH_THREADS, available_search_threads()),
        )
        .thread_name(|index| format!("flashlaunch-refinement-fallback-{index}"))
        .build()
        .expect("failed to create refinement fallback scan pool");
    search_streaming(
        request.generation,
        request.hwnd_value,
        request.spec,
        request.root_plan,
        request.scoring,
        request.recent_items,
        request.query_launch_rules,
        request.effective_limit,
        &scan_pool,
        request.include_score_detail,
        request.include_explanation,
    );
}

fn run_refinement_scan(shared: Arc<RefinementShared>) {
    let (request, spec, eligible_roots) = {
        let Ok(state) = shared.state.lock() else {
            return;
        };
        let request = state.request.clone();
        let spec = request.spec.effective_for_scoring(&request.scoring);
        let any_root_modifier_match = any_modifier_keywords_match(
            request
                .root_plan
                .entries()
                .iter()
                .map(|entry| entry.root.keywords.as_slice()),
            &spec.modifiers,
        );
        let roots = eligible_search_roots(&request.root_plan, &spec, any_root_modifier_match);
        (request, spec, roots)
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(
            request
                .search_threads
                .clamp(MIN_SEARCH_THREADS, available_search_threads()),
        )
        .thread_name(|index| format!("flashlaunch-refine-{index}"))
        .build()
        .expect("failed to create refinement scan pool");

    for root in &eligible_roots {
        let root_context = RootScanContext::new(root.effective_root.clone());
        let parent = root.path.parent().unwrap_or(&root.path).to_path_buf();
        let directory = DirectoryScanContext::new(parent, root.effective_depth, &root_context);
        process_refinement_path(
            root.path.clone(),
            root.path.file_name(),
            root.path.is_dir(),
            &root_context,
            &directory,
            &shared,
        );
    }

    let queue = Arc::new((
        Mutex::new(ScanTaskQueue {
            tasks: eligible_roots
                .into_iter()
                .map(|root| {
                    let root_context = RootScanContext::new(root.effective_root);
                    let directory =
                        DirectoryScanContext::new(root.path, root.effective_depth, &root_context);
                    ScanTask {
                        kind: ScanTaskKind::Directory(directory),
                        root: root_context,
                        exclusions: root.exclusions,
                        stage: SearchStage::Folders,
                    }
                })
                .collect(),
            active_workers: 0,
        }),
        Condvar::new(),
    ));
    pool.scope(|scope| {
        for _ in 0..pool.current_num_threads() {
            let queue = Arc::clone(&queue);
            let shared = Arc::clone(&shared);
            scope.spawn(move |_| refinement_scan_worker(queue, shared));
        }
    });

    let publish = {
        let Ok(mut state) = shared.state.lock() else {
            return;
        };
        if state.shutdown {
            return;
        }
        state.scan_done = true;
        refinement_publish_snapshot(&state, true)
    };
    shared.changed.notify_all();
    publish_refinement_snapshot(publish);
    let _ = spec;
}

fn refinement_scan_worker(
    queue: Arc<(Mutex<ScanTaskQueue>, Condvar)>,
    shared: Arc<RefinementShared>,
) {
    while let Some(task) = take_refinement_scan_task(&queue, &shared) {
        match task.kind {
            ScanTaskKind::Directory(directory) => {
                if task.root.root.max_depth == SEARCH_DEPTH_ALL
                    || directory.depth <= task.root.root.max_depth
                {
                    let enumerated = enumerate_directory_batches(&directory.path, &task.exclusions);
                    if task.root.root.max_depth == SEARCH_DEPTH_ALL
                        || directory.depth < task.root.root.max_depth
                    {
                        for child_path in enumerated.child_directories {
                            let child = DirectoryScanContext::new(
                                child_path,
                                directory.depth.saturating_add(1),
                                &task.root,
                            );
                            enqueue_scan_task(
                                &queue,
                                ScanTask {
                                    kind: ScanTaskKind::Directory(child),
                                    root: task.root.clone(),
                                    exclusions: Arc::clone(&task.exclusions),
                                    stage: task.stage,
                                },
                            );
                        }
                    }
                    for entries in enumerated.batches {
                        for entry in entries {
                            process_refinement_path(
                                entry.path,
                                Some(entry.file_name.as_os_str()),
                                entry.is_dir,
                                &task.root,
                                &directory,
                                &shared,
                            );
                        }
                    }
                }
            }
            ScanTaskKind::Entries(directory, entries) => {
                for entry in entries {
                    process_refinement_path(
                        entry.path,
                        Some(entry.file_name.as_os_str()),
                        entry.is_dir,
                        &task.root,
                        &directory,
                        &shared,
                    );
                }
            }
        }
        finish_scan_task(&queue);
    }
}

fn take_refinement_scan_task(
    queue: &Arc<(Mutex<ScanTaskQueue>, Condvar)>,
    shared: &RefinementShared,
) -> Option<ScanTask> {
    let (tasks, changed) = &**queue;
    let mut tasks = tasks.lock().ok()?;
    loop {
        if shared.state.lock().ok().is_none_or(|state| state.shutdown) {
            return None;
        }
        if let Some(task) = tasks.tasks.pop_front() {
            tasks.active_workers = tasks.active_workers.saturating_add(1);
            return Some(task);
        }
        if tasks.active_workers == 0 {
            changed.notify_all();
            return None;
        }
        tasks = changed.wait(tasks).ok()?;
    }
}

fn process_refinement_path(
    path: PathBuf,
    file_name: Option<&std::ffi::OsStr>,
    is_dir: bool,
    root: &RootScanContext,
    directory: &DirectoryScanContext,
    shared: &RefinementShared,
) {
    let prepared_name = match PreparedCandidateName::from_path_and_file_name(&path, file_name) {
        Some(value) => value,
        None => {
            if let Ok(mut state) = shared.state.lock() {
                state.scanned_total = state.scanned_total.saturating_add(1);
            }
            maybe_publish_refinement_progress(shared);
            return;
        }
    };
    let (version, matched, scoring) = {
        let Ok(mut state) = shared.state.lock() else {
            return;
        };
        if state.shutdown {
            return;
        }
        state.scanned_total = state.scanned_total.saturating_add(1);
        let version = state.query_version;
        let matched = state
            .prepared_query
            .score_candidate_name(
                &prepared_name,
                &directory.folded_parent,
                is_dir,
                &state.request.scoring,
            )
            .is_some();
        (version, matched, Arc::clone(&state.request.scoring))
    };
    if !matched {
        maybe_publish_refinement_progress(shared);
        return;
    }
    #[cfg(test)]
    {
        let delay_ms = REFINEMENT_TEST_MATCH_DELAY_MS.load(Ordering::Relaxed);
        if delay_ms > 0 {
            thread::sleep(Duration::from_millis(delay_ms));
        }
    }

    let path_key = recent_item_key(&path);
    let relative_depth = directory.depth.saturating_add(usize::from(is_dir));
    let item = launch_item_from_prepared_candidate(
        path,
        is_dir,
        &root.root,
        &scoring,
        prepared_name,
        directory.subtitle.clone(),
        directory.folded_parent.clone(),
        relative_depth,
    );
    let candidate = PreparedCandidate::new(item, path_key);
    let publish = {
        let Ok(mut state) = shared.state.lock() else {
            return;
        };
        if state.shutdown || !state.scan_seen.insert(candidate.path_key.clone()) {
            return;
        }
        if state.reusable {
            let next_bytes = state.cache_bytes.saturating_add(candidate.estimated_bytes);
            if next_bytes <= state.cache_limit {
                state.cache_bytes = next_bytes;
                state.cache.push(candidate.clone());
            } else {
                state.reusable = false;
            }
        }

        let prepared_name = PreparedCandidateName {
            title: candidate.item.title.clone(),
            folded_title: candidate.item.folded_title.clone(),
            folded_stem: candidate.item.folded_stem.clone(),
        };
        let current_text_score = state.prepared_query.score_candidate_name(
            &prepared_name,
            candidate.item.folded_parent(),
            candidate.item.is_dir,
            &state.request.scoring,
        );
        let ranked = current_text_score.and_then(|text_score| {
            let preview_limit = refinement_preview_limit(state.request.effective_limit);
            let threshold = bounded_ranked_threshold(&state.best, preview_limit);
            let upper_bound = heuristic_score_upper_bound(
                &candidate.item,
                text_score,
                &state.prepared_query,
                &state.request.scoring,
            );
            if state.request.effective_limit != usize::MAX
                && should_branch_prune(upper_bound, threshold)
            {
                #[cfg(test)]
                BRANCH_PRUNE_COUNT.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            let components = score_components_with_prepared_query(
                &candidate.item,
                &state.effective_spec,
                &state.prepared_query,
                &candidate.path_key,
                text_score,
                &state.recent_map,
                &state.request.scoring,
            );
            Some(ranked_heuristic_candidate(
                candidate.item.clone(),
                components,
                false,
            ))
        });
        if let Some(ranked) = ranked {
            if state.ranking_seen.insert(candidate.path_key.clone()) {
                let preview_limit = refinement_preview_limit(state.request.effective_limit);
                insert_bounded_ranked_candidate(&mut state.best, ranked, preview_limit);
            }
        }

        let stale = version != state.query_version;
        let due = refinement_progress_due(state.scanned_total, state.last_publish);
        if due {
            state.last_publish = Instant::now();
            Some(refinement_publish_snapshot(&state, false))
        } else if stale {
            None
        } else {
            None
        }
    };
    if let Some(publish) = publish {
        publish_refinement_snapshot(publish);
    }
}

fn refinement_progress_due(scanned_total: usize, last_publish: Instant) -> bool {
    scanned_total > 0
        && scanned_total.is_multiple_of(SEARCH_BATCH_ITEM_STEP)
        && last_publish.elapsed() >= SEARCH_BATCH_INTERVAL
}

fn maybe_publish_refinement_progress(shared: &RefinementShared) {
    let publish = {
        let Ok(mut state) = shared.state.lock() else {
            return;
        };
        let due = refinement_progress_due(state.scanned_total, state.last_publish);
        if due {
            state.last_publish = Instant::now();
            Some(refinement_publish_snapshot(&state, false))
        } else {
            None
        }
    };
    if let Some(publish) = publish {
        publish_refinement_snapshot(publish);
    }
}

#[cfg(feature = "debug-tools")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn benchmark_refinement_sequence_once(
    root_plan: Arc<RootOwnershipPlan>,
    scoring: Arc<ScoringConfig>,
    recent_items: Arc<Vec<String>>,
    query_launch_rules: Arc<Vec<QueryLaunchRule>>,
    effective_limit: usize,
    search_threads: usize,
) -> Vec<RefinementBenchmarkSample> {
    const QUERIES: [&str; 3] = ["chr", "chro", "chrom"];
    let mut samples = Vec::with_capacity(QUERIES.len());
    let mut session: Option<RefinementSession> = None;
    for (index, query) in QUERIES.into_iter().enumerate() {
        let generation = ACTIVE_SEARCH_GENERATION
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let request = SearchWorkerRequest {
            generation,
            hwnd_value: 0,
            spec: parse_search_query(query),
            root_plan: Arc::clone(&root_plan),
            scoring: Arc::clone(&scoring),
            recent_items: Arc::clone(&recent_items),
            query_launch_rules: Arc::clone(&query_launch_rules),
            effective_limit: effective_limit.max(1),
            search_threads,
            include_score_detail: false,
            include_explanation: false,
        };
        let started = Instant::now();
        let mut full_scan_fallback = index > 0;
        if session
            .as_ref()
            .is_some_and(|current| current.can_refine(&request))
            && session
                .as_ref()
                .is_some_and(|current| current.update(request.clone()))
        {
            full_scan_fallback = false;
        } else {
            if let Some(mut current) = session.take() {
                current.shutdown();
            }
            session = Some(RefinementSession::start(request));
        }
        if let Some(current) = session.as_ref() {
            current.wait_until_complete(Duration::from_secs(300));
            let mut metrics = current.metrics();
            metrics.full_scan_fallback |= full_scan_fallback;
            samples.push(RefinementBenchmarkSample {
                query: query.to_string(),
                elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                metrics,
            });
        }
    }
    if let Some(mut current) = session {
        current.shutdown();
    }
    samples
}

#[cfg(feature = "debug-tools")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn benchmark_live_scan_once(
    query: &str,
    root_plan: Arc<RootOwnershipPlan>,
    scoring: Arc<ScoringConfig>,
    recent_items: Arc<Vec<String>>,
    query_launch_rules: Arc<Vec<QueryLaunchRule>>,
    effective_limit: usize,
    search_threads: usize,
) -> SearchBenchmarkResult {
    let effective_limit = effective_limit.max(1);
    let generation = ACTIVE_SEARCH_GENERATION
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    let spec = parse_search_query(query).effective_for_scoring(&scoring);
    let any_root_modifier_match = any_modifier_keywords_match(
        root_plan
            .entries()
            .iter()
            .map(|entry| entry.root.keywords.as_slice()),
        &spec.modifiers,
    );
    let recent_map = recent_lookup(&recent_items);
    let mut seen = HashSet::new();
    let mut initial_results = Vec::new();
    if let Some(priority) =
        query_launch_rule_result_with_explanation(&spec.raw, &query_launch_rules, &scoring, true)
    {
        initial_results.push(priority);
    }
    initial_results.extend(collect_recent_path_matches_with_detail(
        &spec,
        &root_plan,
        &recent_items,
        &recent_map,
        &scoring,
        effective_limit,
        true,
        true,
    ));
    prioritize_query_launch_rule_result_with_explanation(
        &mut initial_results,
        &spec.raw,
        &query_launch_rules,
        &scoring,
        true,
    );
    normalize_bounded_search_results(&mut initial_results, effective_limit);
    for result in &initial_results {
        if let Some(path) = result_target_path(result) {
            seen.insert(recent_item_key(&path));
        }
    }
    let mut best = initial_results
        .into_iter()
        .map(ranked_candidate_from_prebuilt)
        .collect::<Vec<_>>();

    let mut scanned_total = 0usize;
    let eligible_roots = eligible_search_roots(&root_plan, &spec, any_root_modifier_match);
    let root_count = eligible_roots.len();
    let mut last_publish = Instant::now();
    let scan_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(search_threads.clamp(MIN_SEARCH_THREADS, available_search_threads()))
        .build()
        .expect("failed to create benchmark scan pool");
    for root in &eligible_roots {
        let prepared_query = PreparedQuery::new(&spec, &scoring);
        if let Some(candidate) = consider_ranked_search_path_with_type(
            root.path.clone(),
            root.path.is_dir(),
            &root.effective_root,
            &scoring,
            &recent_map,
            &spec,
            &prepared_query,
            &mut seen,
            &mut scanned_total,
            bounded_ranked_threshold(&best, effective_limit),
        ) {
            insert_bounded_ranked_candidate(&mut best, candidate, effective_limit);
        }
    }
    let live_roots = eligible_roots
        .into_iter()
        .map(|root| {
            (
                root.path,
                root.effective_depth,
                root.effective_root,
                root.exclusions,
                SearchStage::Folders,
            )
        })
        .collect();
    scan_search_roots(
        live_roots,
        &scan_pool,
        &scoring,
        &recent_map,
        &spec,
        &mut seen,
        &mut best,
        &mut scanned_total,
        generation,
        0,
        effective_limit,
        effective_limit,
        &mut last_publish,
        true,
        true,
        None,
    );
    if effective_limit == usize::MAX {
        best.sort_by_cached_key(ranked_candidate_sort_key);
    }
    let mut results =
        materialize_ranked_candidates(&best, &spec, &recent_map, &scoring, true, true);
    prioritize_query_launch_rule_result(&mut results, &spec.raw, &query_launch_rules, &scoring);
    SearchBenchmarkResult {
        scanned_total,
        workers: scan_pool.current_num_threads(),
        live_scan_strategy: live_scan_strategy(root_count, scan_pool.current_num_threads()),
        results,
    }
}

#[cfg(any(test, feature = "debug-tools"))]
fn live_scan_strategy(live_root_count: usize, worker_count: usize) -> &'static str {
    if live_root_count == 0 {
        "none"
    } else if worker_count <= 1 {
        "sequential"
    } else {
        "parallel-task-queue"
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn search_streaming(
    generation: u64,
    hwnd_value: isize,
    spec: SearchQuerySpec,
    root_plan: Arc<RootOwnershipPlan>,
    scoring: Arc<ScoringConfig>,
    recent_items: Arc<Vec<String>>,
    query_launch_rules: Arc<Vec<QueryLaunchRule>>,
    effective_limit: usize,
    scan_pool: &ThreadPool,
    include_score_detail: bool,
    include_explanation: bool,
) {
    let effective_limit = effective_limit.max(1);
    let show_all = effective_limit == usize::MAX;
    let preview_limit = if show_all {
        DEFAULT_RESULT_LIMIT.max(1)
    } else {
        effective_limit
    };
    if spec.mode == SearchQueryMode::BlankHistory {
        let results = collect_recent_path_order_results_with_explanation(
            &recent_items,
            effective_limit,
            include_explanation,
        );
        publish_completed_search_results(generation, hwnd_value, results, 0, effective_limit);
        return;
    }
    if spec.mode == SearchQueryMode::DirectoryBrowse {
        search_directory_streaming(
            generation,
            hwnd_value,
            &spec,
            &scoring,
            effective_limit,
            include_score_detail,
            include_explanation,
        );
        return;
    }

    let mut store_writer = if show_all {
        match ResultStoreWriter::start(generation) {
            Ok(writer) => Some(writer),
            Err(error) => {
                publish_search_batch_with_store(
                    generation,
                    hwnd_value,
                    Vec::new(),
                    0,
                    SearchStage::Done,
                    effective_limit,
                    ResultStoreCompletion::Error(error.to_string()),
                );
                return;
            }
        }
    } else {
        None
    };
    let store_sender = store_writer.as_ref().and_then(ResultStoreWriter::sender);
    let spec = spec.effective_for_scoring(&scoring);
    let any_root_modifier_match = any_modifier_keywords_match(
        root_plan
            .entries()
            .iter()
            .map(|entry| entry.root.keywords.as_slice()),
        &spec.modifiers,
    );
    let recent_map = recent_lookup(&recent_items);
    let mut seen = HashSet::new();
    let initial_limit = if show_all { usize::MAX } else { preview_limit };
    let mut initial_results = collect_recent_path_matches_with_detail(
        &spec,
        &root_plan,
        &recent_items,
        &recent_map,
        &scoring,
        initial_limit,
        include_score_detail,
        include_explanation,
    );
    prioritize_query_launch_rule_result_with_explanation(
        &mut initial_results,
        &spec.raw,
        &query_launch_rules,
        &scoring,
        include_explanation,
    );
    let mut best = Vec::new();
    for result in initial_results {
        if let Some(path) = result_target_path(&result) {
            seen.insert(recent_item_key(&path));
        }
        let ranked = ranked_candidate_from_prebuilt(result);
        if let Some(sender) = store_sender.as_ref() {
            let _ = sender.send_ranked(ranked.clone());
        }
        insert_bounded_ranked_candidate(&mut best, ranked, preview_limit);
    }
    publish_search_batch(
        generation,
        hwnd_value,
        materialize_ranked_candidates(
            &best,
            &spec,
            &recent_map,
            &scoring,
            include_score_detail,
            include_explanation,
        ),
        0,
        SearchStage::History,
        false,
        effective_limit,
    );
    let mut scanned_total = 0usize;
    let mut last_publish = Instant::now();
    let prepared_query = PreparedQuery::new(&spec, &scoring);

    let eligible_roots = eligible_search_roots(&root_plan, &spec, any_root_modifier_match);
    for root in &eligible_roots {
        if !is_active_search_generation(generation) {
            return;
        }
        if let Some(ranked) = consider_ranked_search_path_with_type(
            root.path.clone(),
            root.path.is_dir(),
            &root.effective_root,
            &scoring,
            &recent_map,
            &spec,
            &prepared_query,
            &mut seen,
            &mut scanned_total,
            if show_all {
                0
            } else {
                bounded_ranked_threshold(&best, preview_limit)
            },
        ) {
            if let Some(sender) = store_sender.as_ref() {
                let _ = sender.send_ranked(ranked.clone());
            }
            insert_bounded_ranked_candidate(&mut best, ranked, preview_limit);
        }
        maybe_publish_ranked_search_batch(
            generation,
            hwnd_value,
            &best,
            scanned_total,
            SearchStage::Folders,
            effective_limit,
            &mut last_publish,
            &spec,
            &recent_map,
            &scoring,
            include_score_detail,
            include_explanation,
        );
    }

    let live_roots = eligible_roots
        .into_iter()
        .map(|root| {
            (
                root.path,
                root.effective_depth,
                root.effective_root,
                root.exclusions,
                SearchStage::Folders,
            )
        })
        .collect();

    scan_search_roots(
        live_roots,
        scan_pool,
        &scoring,
        &recent_map,
        &spec,
        &mut seen,
        &mut best,
        &mut scanned_total,
        generation,
        hwnd_value,
        preview_limit,
        effective_limit,
        &mut last_publish,
        include_score_detail,
        include_explanation,
        store_sender,
    );

    if !is_active_search_generation(generation) {
        return;
    }
    let final_results = materialize_ranked_candidates(
        &best,
        &spec,
        &recent_map,
        &scoring,
        include_score_detail,
        include_explanation,
    );
    if let Some(writer) = store_writer.take() {
        let completion = match writer.finish() {
            Ok(manifest) => ResultStoreCompletion::Ready(manifest),
            Err(error) => ResultStoreCompletion::Error(error.to_string()),
        };
        publish_search_batch_with_store(
            generation,
            hwnd_value,
            final_results,
            scanned_total,
            SearchStage::Done,
            effective_limit,
            completion,
        );
    } else {
        publish_search_batch(
            generation,
            hwnd_value,
            final_results,
            scanned_total,
            SearchStage::Done,
            true,
            effective_limit,
        );
    }
}

fn publish_completed_search_results(
    generation: u64,
    hwnd_value: isize,
    mut results: Vec<SearchResult>,
    scanned_total: usize,
    effective_limit: usize,
) {
    if effective_limit != usize::MAX {
        publish_search_batch(
            generation,
            hwnd_value,
            results,
            scanned_total,
            SearchStage::Done,
            true,
            effective_limit,
        );
        return;
    }
    let mut preview = results.clone();
    sort_search_results(&mut preview);
    preview.truncate(DEFAULT_RESULT_LIMIT.max(1));
    match ResultStoreWriter::start(generation) {
        Ok(writer) => {
            if let Some(sender) = writer.sender() {
                for result in results.drain(..) {
                    if sender.send(result).is_err() {
                        break;
                    }
                }
            }
            let completion = match writer.finish() {
                Ok(manifest) => ResultStoreCompletion::Ready(manifest),
                Err(error) => ResultStoreCompletion::Error(error.to_string()),
            };
            publish_search_batch_with_store(
                generation,
                hwnd_value,
                preview,
                scanned_total,
                SearchStage::Done,
                effective_limit,
                completion,
            );
        }
        Err(error) => publish_search_batch_with_store(
            generation,
            hwnd_value,
            preview,
            scanned_total,
            SearchStage::Done,
            effective_limit,
            ResultStoreCompletion::Error(error.to_string()),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_search_roots(
    scan_roots: Vec<(PathBuf, usize, IndexRoot, Arc<Vec<String>>, SearchStage)>,
    scan_pool: &ThreadPool,
    scoring: &ScoringConfig,
    recent_map: &HashMap<String, RecentEntry>,
    spec: &SearchQuerySpec,
    seen: &mut HashSet<String>,
    best: &mut Vec<RankedCandidate>,
    scanned_total: &mut usize,
    generation: u64,
    hwnd_value: isize,
    preview_limit: usize,
    published_effective_limit: usize,
    last_publish: &mut Instant,
    include_score_detail: bool,
    include_explanation: bool,
    result_store_sender: Option<ResultStoreSender>,
) {
    if scan_roots.is_empty() {
        return;
    }
    let prepared_query = PreparedQuery::new(spec, scoring);
    let worker_count = scan_pool.current_num_threads();
    if worker_count <= 1 {
        for (path, effective_depth, root, exclusions, stage) in scan_roots {
            scan_root_for_query(
                &path,
                effective_depth,
                root.max_depth,
                &root,
                scoring,
                recent_map,
                spec,
                &prepared_query,
                seen,
                best,
                scanned_total,
                generation,
                hwnd_value,
                preview_limit,
                published_effective_limit,
                last_publish,
                stage,
                include_score_detail,
                include_explanation,
                &exclusions,
                result_store_sender.as_ref(),
            );
        }
        return;
    }

    let shared_queue = Arc::new((
        Mutex::new(ScanTaskQueue {
            tasks: scan_roots
                .into_iter()
                .map(|(path, effective_depth, root, exclusions, stage)| {
                    let root = RootScanContext::new(root);
                    let directory = DirectoryScanContext::new(path, effective_depth, &root);
                    ScanTask {
                        kind: ScanTaskKind::Directory(directory),
                        root,
                        exclusions,
                        stage,
                    }
                })
                .collect(),
            active_workers: 0,
        }),
        Condvar::new(),
    ));
    let initial_threshold = if result_store_sender.is_some() {
        0
    } else {
        bounded_ranked_threshold(best, preview_limit)
    };
    let progress = Arc::new(SharedParallelSearchProgress {
        state: Mutex::new(ParallelSearchProgress {
            seen: std::mem::take(seen),
            best: std::mem::take(best),
            scanned_total: *scanned_total,
            last_publish: *last_publish,
        }),
        threshold: AtomicU64::new(initial_threshold),
        result_store_sender,
    });
    scan_pool.scope(|scope| {
        for _ in 0..worker_count {
            let shared_queue = shared_queue.clone();
            let progress = progress.clone();
            let prepared_query = &prepared_query;
            scope.spawn(move |_| {
                while let Some(task) = take_scan_task(&shared_queue, generation) {
                    scan_task_for_query(
                        task,
                        &shared_queue,
                        &progress,
                        scoring,
                        recent_map,
                        spec,
                        &prepared_query,
                        generation,
                        hwnd_value,
                        preview_limit,
                        published_effective_limit,
                        include_score_detail,
                        include_explanation,
                    );
                    finish_scan_task(&shared_queue);
                }
            });
        }
    });

    if let Ok(mut progress) = progress.state.lock() {
        *seen = std::mem::take(&mut progress.seen);
        *best = std::mem::take(&mut progress.best);
        *scanned_total = progress.scanned_total;
        *last_publish = progress.last_publish;
    };
}

#[derive(Clone)]
struct RootScanContext {
    root: IndexRoot,
    root_key: String,
    root_name: String,
}

impl RootScanContext {
    fn new(root: IndexRoot) -> Self {
        let root_key = root
            .path
            .as_deref()
            .map(normalized_root_path_key)
            .unwrap_or_default();
        let root_name = root
            .path
            .as_deref()
            .and_then(Path::file_name)
            .map(|value| {
                value
                    .to_string_lossy()
                    .trim_matches(['\\', '/'])
                    .to_string()
            })
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| root.raw.trim_matches(['\\', '/']).to_string());
        Self {
            root,
            root_key,
            root_name,
        }
    }
}

#[derive(Clone)]
struct DirectoryScanContext {
    path: PathBuf,
    subtitle: String,
    folded_parent: String,
    depth: usize,
}

impl DirectoryScanContext {
    fn new(path: PathBuf, depth: usize, root: &RootScanContext) -> Self {
        let subtitle = if let Some(root_path) = root.root.path.as_deref() {
            if let Ok(relative) = path.strip_prefix(root_path) {
                let relative = relative.to_string_lossy();
                let relative = relative.trim_matches(['\\', '/']);
                if relative.is_empty() {
                    format!("{}\\", root.root_name)
                } else {
                    format!("{}\\{}\\", root.root_name, relative)
                }
            } else {
                path.to_string_lossy().to_string()
            }
        } else {
            path.to_string_lossy().to_string()
        };
        let folded_parent = searchable_text(&path.to_string_lossy());
        Self {
            path,
            subtitle,
            folded_parent,
            depth,
        }
    }
}

#[derive(Clone)]
struct ScanTask {
    kind: ScanTaskKind,
    root: RootScanContext,
    exclusions: Arc<Vec<String>>,
    stage: SearchStage,
}

#[derive(Clone)]
enum ScanTaskKind {
    Directory(DirectoryScanContext),
    Entries(DirectoryScanContext, Vec<FilesystemEntry>),
}

struct ScanTaskQueue {
    tasks: VecDeque<ScanTask>,
    active_workers: usize,
}

struct ParallelSearchProgress {
    seen: HashSet<String>,
    best: Vec<RankedCandidate>,
    scanned_total: usize,
    last_publish: Instant,
}

struct LocalSearchProgress {
    seen: HashSet<String>,
    best: Vec<RankedCandidate>,
    scanned_total: usize,
}

type SharedScanTaskQueue = Arc<(Mutex<ScanTaskQueue>, Condvar)>;

struct SharedParallelSearchProgress {
    state: Mutex<ParallelSearchProgress>,
    threshold: AtomicU64,
    result_store_sender: Option<ResultStoreSender>,
}

fn take_scan_task(shared_queue: &SharedScanTaskQueue, generation: u64) -> Option<ScanTask> {
    let (queue, changed) = &**shared_queue;
    let mut queue = queue.lock().ok()?;
    loop {
        if !is_active_search_generation(generation) {
            return None;
        }
        if let Some(task) = queue.tasks.pop_front() {
            queue.active_workers = queue.active_workers.saturating_add(1);
            return Some(task);
        }
        if queue.active_workers == 0 {
            changed.notify_all();
            return None;
        }
        queue = changed.wait(queue).ok()?;
    }
}

fn finish_scan_task(shared_queue: &SharedScanTaskQueue) {
    let (queue, changed) = &**shared_queue;
    if let Ok(mut queue) = queue.lock() {
        queue.active_workers = queue.active_workers.saturating_sub(1);
        changed.notify_all();
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_task_for_query(
    task: ScanTask,
    shared_queue: &SharedScanTaskQueue,
    progress: &SharedParallelSearchProgress,
    scoring: &ScoringConfig,
    recent_map: &HashMap<String, RecentEntry>,
    spec: &SearchQuerySpec,
    prepared_query: &PreparedQuery,
    generation: u64,
    hwnd_value: isize,
    preview_limit: usize,
    published_effective_limit: usize,
    include_score_detail: bool,
    include_explanation: bool,
) {
    let mut local = LocalSearchProgress {
        seen: HashSet::new(),
        best: Vec::new(),
        scanned_total: 0,
    };
    scan_task_for_query_local(
        task,
        shared_queue,
        progress,
        scoring,
        recent_map,
        spec,
        prepared_query,
        generation,
        hwnd_value,
        preview_limit,
        published_effective_limit,
        &mut local,
        include_score_detail,
        include_explanation,
    );
    flush_parallel_search_progress(
        progress,
        &mut local,
        generation,
        hwnd_value,
        preview_limit,
        published_effective_limit,
        None,
        spec,
        recent_map,
        scoring,
        include_score_detail,
        include_explanation,
    );
}

#[allow(clippy::too_many_arguments)]
fn scan_task_for_query_local(
    task: ScanTask,
    shared_queue: &SharedScanTaskQueue,
    progress: &SharedParallelSearchProgress,
    scoring: &ScoringConfig,
    recent_map: &HashMap<String, RecentEntry>,
    spec: &SearchQuerySpec,
    prepared_query: &PreparedQuery,
    generation: u64,
    hwnd_value: isize,
    preview_limit: usize,
    published_effective_limit: usize,
    local: &mut LocalSearchProgress,
    include_score_detail: bool,
    include_explanation: bool,
) {
    if let ScanTaskKind::Directory(directory) = &task.kind {
        if task.root.root.max_depth != SEARCH_DEPTH_ALL
            && directory.depth > task.root.root.max_depth
        {
            return;
        }
        let batches = enumerate_directory_task(directory, &task, shared_queue, generation);
        for entries in batches {
            if !is_active_search_generation(generation) {
                return;
            }
            scan_task_for_query_local(
                ScanTask {
                    kind: ScanTaskKind::Entries(directory.clone(), entries),
                    root: task.root.clone(),
                    exclusions: Arc::clone(&task.exclusions),
                    stage: task.stage,
                },
                shared_queue,
                progress,
                scoring,
                recent_map,
                spec,
                prepared_query,
                generation,
                hwnd_value,
                preview_limit,
                published_effective_limit,
                local,
                include_score_detail,
                include_explanation,
            );
        }
        return;
    }
    let ScanTaskKind::Entries(directory, entries) = task.kind else {
        return;
    };
    let mut next_flush = SEARCH_BATCH_ITEM_STEP;
    let mut last_flush = Instant::now();
    for entry in entries {
        if !is_active_search_generation(generation) {
            return;
        }
        let result = consider_prepared_search_path(
            entry.path,
            Some(entry.file_name.as_os_str()),
            entry.is_dir,
            &task.root,
            &directory,
            scoring,
            recent_map,
            spec,
            prepared_query,
            &mut local.seen,
            &mut local.scanned_total,
            if progress.result_store_sender.is_some() {
                0
            } else {
                progress.threshold.load(Ordering::Acquire)
            },
            include_score_detail,
            include_explanation,
        );
        if let Some(result) = result {
            offer_parallel_search_result(progress, result, preview_limit);
        }
        if local.scanned_total >= next_flush || last_flush.elapsed() >= SEARCH_BATCH_INTERVAL {
            flush_parallel_search_progress(
                progress,
                local,
                generation,
                hwnd_value,
                preview_limit,
                published_effective_limit,
                Some(task.stage),
                spec,
                recent_map,
                scoring,
                include_score_detail,
                include_explanation,
            );
            next_flush = local.scanned_total.saturating_add(SEARCH_BATCH_ITEM_STEP);
            last_flush = Instant::now();
        }
    }
}

fn offer_parallel_search_result(
    progress: &SharedParallelSearchProgress,
    candidate: PreparedSearchResult,
    effective_limit: usize,
) {
    let threshold = progress.threshold.load(Ordering::Acquire);
    if progress.result_store_sender.is_none()
        && threshold != 0
        && primary_ranked_candidate_rank(&candidate.candidate) < threshold
    {
        return;
    }

    let Ok(mut state) = progress.state.lock() else {
        return;
    };
    if !state.seen.insert(candidate.path_key) {
        return;
    }
    if let Some(sender) = progress.result_store_sender.as_ref() {
        let _ = sender.send_ranked(candidate.candidate.clone());
    }
    if insert_bounded_ranked_candidate(&mut state.best, candidate.candidate, effective_limit) {
        let threshold = bounded_ranked_threshold(&state.best, effective_limit);
        progress.threshold.store(threshold, Ordering::Release);
    }
}

fn enumerate_directory_task(
    directory: &DirectoryScanContext,
    task: &ScanTask,
    shared_queue: &SharedScanTaskQueue,
    generation: u64,
) -> Vec<Vec<FilesystemEntry>> {
    let enumerated = enumerate_directory_batches(&directory.path, &task.exclusions);
    if !is_active_search_generation(generation) {
        return Vec::new();
    }
    if task.root.root.max_depth == SEARCH_DEPTH_ALL || directory.depth < task.root.root.max_depth {
        for child_path in enumerated.child_directories {
            let child_directory = DirectoryScanContext::new(
                child_path,
                directory.depth.saturating_add(1),
                &task.root,
            );
            enqueue_scan_task(
                shared_queue,
                ScanTask {
                    kind: ScanTaskKind::Directory(child_directory),
                    root: task.root.clone(),
                    exclusions: Arc::clone(&task.exclusions),
                    stage: task.stage,
                },
            );
        }
    }
    enumerated.batches
}

fn enqueue_scan_task(shared_queue: &SharedScanTaskQueue, task: ScanTask) {
    let (queue, changed) = &**shared_queue;
    if let Ok(mut queue) = queue.lock() {
        queue.tasks.push_back(task);
        changed.notify_one();
    }
}

fn flush_parallel_search_progress(
    progress: &SharedParallelSearchProgress,
    local: &mut LocalSearchProgress,
    generation: u64,
    hwnd_value: isize,
    _preview_limit: usize,
    published_effective_limit: usize,
    publish_stage: Option<SearchStage>,
    spec: &SearchQuerySpec,
    recent_map: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
    include_score_detail: bool,
    include_explanation: bool,
) {
    if local.scanned_total == 0 && local.best.is_empty() {
        return;
    }
    let mut pending_publish = None;
    if let Ok(mut progress_state) = progress.state.lock() {
        let ParallelSearchProgress {
            seen: _,
            best,
            scanned_total,
            last_publish,
        } = &mut *progress_state;
        *scanned_total = scanned_total.saturating_add(local.scanned_total);
        if let Some(stage) = publish_stage {
            let due_by_items = scanned_total.is_multiple_of(SEARCH_BATCH_ITEM_STEP);
            let due_by_time = last_publish.elapsed() >= SEARCH_BATCH_INTERVAL;
            if (due_by_items || due_by_time) && is_active_search_generation(generation) {
                let preview = if published_effective_limit == usize::MAX {
                    let mut ranked = best.clone();
                    ranked.sort_by_cached_key(ranked_candidate_sort_key);
                    ranked.truncate(DEFAULT_RESULT_LIMIT.max(1));
                    ranked
                } else {
                    best.clone()
                };
                pending_publish = Some((preview, *scanned_total, stage));
                *last_publish = Instant::now();
            }
        }
    }
    local.seen.clear();
    local.scanned_total = 0;
    if let Some((ranked, scanned_total, stage)) = pending_publish {
        let results = materialize_ranked_candidates(
            &ranked,
            spec,
            recent_map,
            scoring,
            include_score_detail,
            include_explanation,
        );
        publish_search_batch(
            generation,
            hwnd_value,
            results,
            scanned_total,
            stage,
            false,
            published_effective_limit,
        );
    }
}

pub(crate) fn root_matches_query_modifiers(
    root: &IndexRoot,
    modifiers: &[String],
    any_modifier_match: bool,
) -> bool {
    modifier_keywords_apply(&root.keywords, modifiers, any_modifier_match)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn search_directory_streaming(
    generation: u64,
    hwnd_value: isize,
    spec: &SearchQuerySpec,
    scoring: &ScoringConfig,
    effective_limit: usize,
    include_score_detail: bool,
    include_explanation: bool,
) {
    let Some(directory) = spec.directory.as_ref() else {
        publish_completed_search_results(generation, hwnd_value, Vec::new(), 0, effective_limit);
        return;
    };
    let show_all = effective_limit == usize::MAX;
    let preview_limit = if show_all {
        DEFAULT_RESULT_LIMIT.max(1)
    } else {
        effective_limit.max(1)
    };
    let mut store_writer = if show_all {
        match ResultStoreWriter::start(generation) {
            Ok(writer) => Some(writer),
            Err(error) => {
                publish_search_batch_with_store(
                    generation,
                    hwnd_value,
                    Vec::new(),
                    0,
                    SearchStage::Done,
                    effective_limit,
                    ResultStoreCompletion::Error(error.to_string()),
                );
                return;
            }
        }
    } else {
        None
    };
    let store_sender = store_writer.as_ref().and_then(ResultStoreWriter::sender);
    let root = IndexRoot {
        raw: directory.base.to_string_lossy().to_string(),
        path: Some(directory.base.clone()),
        enabled: true,
        score: 0,
        max_depth: 0,
        label: String::new(),
        keywords: Vec::new(),
    };
    let query_spec = parse_search_query(&directory.filter).effective_for_scoring(scoring);
    let recent_map = HashMap::new();
    let mut seen = HashSet::new();
    let mut best = Vec::new();
    let mut scanned_total = 0usize;
    let mut last_publish = Instant::now();
    let Ok(entries) = fs::read_dir(&directory.base) else {
        publish_completed_search_results(
            generation,
            hwnd_value,
            Vec::new(),
            scanned_total,
            effective_limit,
        );
        return;
    };
    for entry in entries.flatten() {
        if !is_active_search_generation(generation) {
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() && is_reparse_point(&path) {
            continue;
        }
        if let Some(result) = consider_search_path(
            path,
            &root,
            scoring,
            &recent_map,
            &query_spec,
            &mut seen,
            &mut scanned_total,
            include_score_detail,
            include_explanation,
        ) {
            if let Some(sender) = store_sender.as_ref() {
                let _ = sender.send(result.clone());
            }
            insert_bounded_search_result(&mut best, result, preview_limit);
        }
        maybe_publish_search_batch(
            generation,
            hwnd_value,
            &mut best,
            scanned_total,
            SearchStage::Directory,
            effective_limit,
            &mut last_publish,
        );
    }
    if !is_active_search_generation(generation) {
        return;
    }
    if let Some(writer) = store_writer.take() {
        let completion = match writer.finish() {
            Ok(manifest) => ResultStoreCompletion::Ready(manifest),
            Err(error) => ResultStoreCompletion::Error(error.to_string()),
        };
        publish_search_batch_with_store(
            generation,
            hwnd_value,
            best,
            scanned_total,
            SearchStage::Done,
            effective_limit,
            completion,
        );
    } else {
        publish_search_batch(
            generation,
            hwnd_value,
            best,
            scanned_total,
            SearchStage::Done,
            true,
            effective_limit,
        );
    }
}

#[derive(Clone)]
struct PreparedSearchResult {
    path_key: String,
    candidate: RankedCandidate,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_root_for_query(
    path: &Path,
    depth: usize,
    max_depth: usize,
    root: &IndexRoot,
    scoring: &ScoringConfig,
    recent_map: &HashMap<String, RecentEntry>,
    spec: &SearchQuerySpec,
    prepared_query: &PreparedQuery,
    seen: &mut HashSet<String>,
    best: &mut Vec<RankedCandidate>,
    scanned_total: &mut usize,
    generation: u64,
    hwnd_value: isize,
    preview_limit: usize,
    published_effective_limit: usize,
    last_publish: &mut Instant,
    stage: SearchStage,
    include_score_detail: bool,
    include_explanation: bool,
    exclusions: &[String],
    result_store_sender: Option<&ResultStoreSender>,
) {
    let mut effective_root = root.clone();
    effective_root.max_depth = max_depth;
    let root_context = RootScanContext::new(effective_root);
    let directory = DirectoryScanContext::new(path.to_path_buf(), depth, &root_context);
    scan_directory_for_query(
        directory,
        &root_context,
        scoring,
        recent_map,
        spec,
        prepared_query,
        seen,
        best,
        scanned_total,
        generation,
        hwnd_value,
        preview_limit,
        published_effective_limit,
        last_publish,
        stage,
        include_score_detail,
        include_explanation,
        exclusions,
        result_store_sender,
    );
}

#[allow(clippy::too_many_arguments)]
fn scan_directory_for_query(
    directory: DirectoryScanContext,
    root: &RootScanContext,
    scoring: &ScoringConfig,
    recent_map: &HashMap<String, RecentEntry>,
    spec: &SearchQuerySpec,
    prepared_query: &PreparedQuery,
    seen: &mut HashSet<String>,
    best: &mut Vec<RankedCandidate>,
    scanned_total: &mut usize,
    generation: u64,
    hwnd_value: isize,
    preview_limit: usize,
    published_effective_limit: usize,
    last_publish: &mut Instant,
    stage: SearchStage,
    include_score_detail: bool,
    include_explanation: bool,
    exclusions: &[String],
    result_store_sender: Option<&ResultStoreSender>,
) {
    if !is_active_search_generation(generation) {
        return;
    }
    if root.root.max_depth != SEARCH_DEPTH_ALL && directory.depth > root.root.max_depth {
        return;
    }
    let Ok(entries) = fs::read_dir(&directory.path) else {
        return;
    };

    for entry in entries.flatten() {
        if !is_active_search_generation(generation) {
            return;
        }
        let entry_path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        if is_dir
            && (is_reparse_point(&entry_path)
                || path_is_in_excluded_subtree(&entry_path, exclusions))
        {
            continue;
        }
        if let Some(candidate) = consider_prepared_search_path(
            entry_path.clone(),
            Some(entry.file_name().as_os_str()),
            is_dir,
            root,
            &directory,
            scoring,
            recent_map,
            spec,
            prepared_query,
            seen,
            scanned_total,
            if result_store_sender.is_some() {
                0
            } else {
                bounded_ranked_threshold(best, preview_limit)
            },
            include_score_detail,
            include_explanation,
        ) {
            if let Some(sender) = result_store_sender {
                let _ = sender.send_ranked(candidate.candidate.clone());
            }
            insert_bounded_ranked_candidate(best, candidate.candidate, preview_limit);
        }
        maybe_publish_ranked_search_batch(
            generation,
            hwnd_value,
            best,
            *scanned_total,
            stage,
            published_effective_limit,
            last_publish,
            spec,
            recent_map,
            scoring,
            include_score_detail,
            include_explanation,
        );
        if is_dir
            && (root.root.max_depth == SEARCH_DEPTH_ALL || directory.depth < root.root.max_depth)
        {
            let child_directory =
                DirectoryScanContext::new(entry_path, directory.depth.saturating_add(1), root);
            scan_directory_for_query(
                child_directory,
                root,
                scoring,
                recent_map,
                spec,
                prepared_query,
                seen,
                best,
                scanned_total,
                generation,
                hwnd_value,
                preview_limit,
                published_effective_limit,
                last_publish,
                stage,
                include_score_detail,
                include_explanation,
                exclusions,
                result_store_sender,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn consider_prepared_search_path(
    path: PathBuf,
    file_name: Option<&std::ffi::OsStr>,
    is_dir: bool,
    root: &RootScanContext,
    directory: &DirectoryScanContext,
    scoring: &ScoringConfig,
    recent_map: &HashMap<String, RecentEntry>,
    spec: &SearchQuerySpec,
    prepared_query: &PreparedQuery,
    seen: &mut HashSet<String>,
    scanned_total: &mut usize,
    prune_threshold: u64,
    include_score_detail: bool,
    include_explanation: bool,
) -> Option<PreparedSearchResult> {
    *scanned_total = scanned_total.saturating_add(1);
    let prepared_name = PreparedCandidateName::from_path_and_file_name(&path, file_name)?;
    let text_score = prepared_query.score_candidate_name(
        &prepared_name,
        &directory.folded_parent,
        is_dir,
        scoring,
    )?;
    let path_key = recent_item_key(&path);
    if !seen.insert(path_key.clone()) {
        return None;
    }
    let relative_depth = directory.depth.saturating_add(usize::from(is_dir));
    let item = launch_item_from_prepared_candidate(
        path,
        is_dir,
        &root.root,
        scoring,
        prepared_name,
        directory.subtitle.clone(),
        directory.folded_parent.clone(),
        relative_depth,
    );
    let upper_bound = heuristic_score_upper_bound(&item, text_score, prepared_query, scoring);
    if should_branch_prune(upper_bound, prune_threshold) {
        #[cfg(test)]
        BRANCH_PRUNE_COUNT.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let components = score_components_with_prepared_query(
        &item,
        spec,
        prepared_query,
        &path_key,
        text_score,
        recent_map,
        scoring,
    );
    let candidate = ranked_heuristic_candidate(item, components, false);
    let _ = (include_score_detail, include_explanation);
    Some(PreparedSearchResult {
        path_key,
        candidate,
    })
}

#[allow(clippy::too_many_arguments)]
fn consider_ranked_search_path_with_type(
    path: PathBuf,
    is_dir: bool,
    root: &IndexRoot,
    scoring: &ScoringConfig,
    recent_map: &HashMap<String, RecentEntry>,
    spec: &SearchQuerySpec,
    prepared_query: &PreparedQuery,
    seen: &mut HashSet<String>,
    scanned_total: &mut usize,
    prune_threshold: u64,
) -> Option<RankedCandidate> {
    let root_context = RootScanContext::new(root.clone());
    let parent = path.parent().unwrap_or(&path).to_path_buf();
    let path_depth = if root_context.root_key.is_empty() {
        0
    } else {
        path_key_relative_depth(&normalized_root_path_key(&parent), &root_context.root_key)
            .unwrap_or(0)
    };
    let directory = DirectoryScanContext::new(parent, path_depth, &root_context);
    consider_prepared_search_path(
        path,
        None,
        is_dir,
        &root_context,
        &directory,
        scoring,
        recent_map,
        spec,
        prepared_query,
        seen,
        scanned_total,
        prune_threshold,
        false,
        false,
    )
    .map(|prepared| prepared.candidate)
}

#[allow(clippy::too_many_arguments)]
fn consider_search_path_with_type(
    path: PathBuf,
    is_dir: bool,
    root: &IndexRoot,
    scoring: &ScoringConfig,
    recent_map: &HashMap<String, RecentEntry>,
    spec: &SearchQuerySpec,
    seen: &mut HashSet<String>,
    scanned_total: &mut usize,
    include_score_detail: bool,
    include_explanation: bool,
) -> Option<SearchResult> {
    let prepared_query = PreparedQuery::new(spec, scoring);
    let root_context = RootScanContext::new(root.clone());
    let parent = path.parent().unwrap_or(&path).to_path_buf();
    let path_depth = if root_context.root_key.is_empty() {
        0
    } else {
        path_key_relative_depth(&normalized_root_path_key(&parent), &root_context.root_key)
            .unwrap_or(0)
    };
    let directory = DirectoryScanContext::new(parent, path_depth, &root_context);
    consider_prepared_search_path(
        path,
        None,
        is_dir,
        &root_context,
        &directory,
        scoring,
        recent_map,
        spec,
        &prepared_query,
        seen,
        scanned_total,
        0,
        include_score_detail,
        include_explanation,
    )
    .map(|candidate| {
        materialize_ranked_candidate(
            &candidate.candidate,
            spec,
            recent_map,
            scoring,
            include_score_detail,
            include_explanation,
        )
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn consider_search_path(
    path: PathBuf,
    root: &IndexRoot,
    scoring: &ScoringConfig,
    recent_map: &HashMap<String, RecentEntry>,
    spec: &SearchQuerySpec,
    seen: &mut HashSet<String>,
    scanned_total: &mut usize,
    include_score_detail: bool,
    include_explanation: bool,
) -> Option<SearchResult> {
    let is_dir = path.is_dir();
    consider_search_path_with_type(
        path,
        is_dir,
        root,
        scoring,
        recent_map,
        spec,
        seen,
        scanned_total,
        include_score_detail,
        include_explanation,
    )
}

pub(crate) fn score_detail_text_from_components(components: &HeuristicScoreComponents) -> String {
    format!(
        "text {} + path {} + index {} + pattern {} + history {} + recency {} + folder {} - path_penalty {} = {}",
        components.text_score,
        components.path_score,
        components.index_score,
        components.pattern_score,
        components.history_score,
        components.recency_score,
        components.folder_score,
        components.path_penalty,
        components.final_score
    )
}

pub(crate) fn score_detail_text(breakdown: &ScoreBreakdown) -> String {
    score_detail_text_from_components(&HeuristicScoreComponents {
        text_score: breakdown.text_score,
        path_score: breakdown.path_score,
        index_score: breakdown.index_score,
        pattern_score: breakdown.pattern_score,
        history_score: breakdown.history_score,
        recency_score: breakdown.recency_score,
        folder_score: breakdown.folder_score,
        path_penalty: breakdown.path_penalty,
        final_score: breakdown.final_score,
    })
}

#[allow(clippy::too_many_arguments)]
fn maybe_publish_ranked_search_batch(
    generation: u64,
    hwnd_value: isize,
    best: &[RankedCandidate],
    scanned_total: usize,
    stage: SearchStage,
    effective_limit: usize,
    last_publish: &mut Instant,
    spec: &SearchQuerySpec,
    recent_map: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
    include_score_detail: bool,
    include_explanation: bool,
) {
    if scanned_total == 0 {
        return;
    }
    let due_by_items = scanned_total.is_multiple_of(SEARCH_BATCH_ITEM_STEP);
    let due_by_time = last_publish.elapsed() >= SEARCH_BATCH_INTERVAL;
    if !due_by_items && !due_by_time {
        return;
    }
    if !is_active_search_generation(generation) {
        return;
    }
    let ranked = if effective_limit == usize::MAX {
        let mut preview = best.to_vec();
        preview.sort_by_cached_key(ranked_candidate_sort_key);
        preview.truncate(DEFAULT_RESULT_LIMIT.max(1));
        preview
    } else {
        best.to_vec()
    };
    let results = materialize_ranked_candidates(
        &ranked,
        spec,
        recent_map,
        scoring,
        include_score_detail,
        include_explanation,
    );
    publish_search_batch(
        generation,
        hwnd_value,
        results,
        scanned_total,
        stage,
        false,
        effective_limit,
    );
    *last_publish = Instant::now();
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn maybe_publish_search_batch(
    generation: u64,
    hwnd_value: isize,
    best: &mut [SearchResult],
    scanned_total: usize,
    stage: SearchStage,
    effective_limit: usize,
    last_publish: &mut Instant,
) {
    if scanned_total == 0 {
        return;
    }
    let due_by_items = scanned_total.is_multiple_of(SEARCH_BATCH_ITEM_STEP);
    let due_by_time = last_publish.elapsed() >= SEARCH_BATCH_INTERVAL;
    if due_by_items || due_by_time {
        if !is_active_search_generation(generation) {
            return;
        }
        let published_results = progressive_search_results(best, effective_limit);
        publish_search_batch(
            generation,
            hwnd_value,
            published_results,
            scanned_total,
            stage,
            false,
            effective_limit,
        );
        *last_publish = Instant::now();
    }
}

fn progressive_search_results(
    best: &mut [SearchResult],
    effective_limit: usize,
) -> Vec<SearchResult> {
    if effective_limit == usize::MAX {
        sort_search_results(best);
        best.iter()
            .take(DEFAULT_RESULT_LIMIT.max(1))
            .cloned()
            .collect()
    } else {
        best.to_vec()
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_search_batch(
    generation: u64,
    hwnd_value: isize,
    results: Vec<SearchResult>,
    scanned_total: usize,
    stage: SearchStage,
    done: bool,
    effective_limit: usize,
) {
    if !is_active_search_generation(generation) {
        return;
    }
    let batch = SearchBatch {
        generation,
        results,
        scanned_total,
        stage,
        done,
        effective_limit,
        result_store: None,
    };
    if let Ok(forwarder) = search_batch_forwarder().lock() {
        if let Some(forwarder) = forwarder.as_ref() {
            let _ = forwarder.send(batch);
            return;
        }
    }
    let should_notify = pending_search_slot()
        .lock()
        .ok()
        .is_some_and(|mut pending| coalesce_pending_search_batch(&mut pending, batch));
    if should_notify {
        unsafe {
            PostMessageW(hwnd_value as HWND, WM_SEARCH_RESULTS_READY, 0, 0);
        }
    }
}

pub(crate) fn publish_search_batch_with_store(
    generation: u64,
    hwnd_value: isize,
    results: Vec<SearchResult>,
    scanned_total: usize,
    stage: SearchStage,
    effective_limit: usize,
    result_store: ResultStoreCompletion,
) {
    if !is_active_search_generation(generation) {
        return;
    }
    let batch = SearchBatch {
        generation,
        results,
        scanned_total,
        stage,
        done: true,
        effective_limit,
        result_store: Some(result_store),
    };
    if let Ok(forwarder) = search_batch_forwarder().lock() {
        if let Some(forwarder) = forwarder.as_ref() {
            let _ = forwarder.send(batch);
            return;
        }
    }
    let should_notify = pending_search_slot()
        .lock()
        .ok()
        .is_some_and(|mut pending| coalesce_pending_search_batch(&mut pending, batch));
    if should_notify {
        unsafe {
            PostMessageW(hwnd_value as HWND, WM_SEARCH_RESULTS_READY, 0, 0);
        }
    }
}

fn is_active_search_generation(generation: u64) -> bool {
    #[cfg(test)]
    if generation == TEST_UNCANCELLED_GENERATION {
        return true;
    }
    ACTIVE_SEARCH_GENERATION.load(Ordering::Relaxed) == generation
}

fn coalesce_pending_search_batch(pending: &mut Vec<SearchBatch>, batch: SearchBatch) -> bool {
    let generation = batch.generation;
    pending.retain(|queued| queued.generation == generation);
    let should_notify = pending.is_empty();
    if should_notify {
        pending.push(batch);
    } else {
        pending[0] = batch;
        pending.truncate(1);
    }
    should_notify
}

pub(crate) fn special_score_explanation(
    ranking_kind: ResultRankingKind,
    query: &str,
    item_path: &str,
    final_score: i32,
) -> ScoreExplanation {
    let (summary, rule, condition, formula) = match ranking_kind {
        ResultRankingKind::Plugin => (
            localized_active("Plugin-provided ordering is used; heuristic scoring is bypassed."),
            localized_active("Plugin priority"),
            localized_active("Plugin result"),
            localized_active("Use plugin-provided score"),
        ),
        ResultRankingKind::RecentOrder => (
            localized_active(
                "Blank-query results follow recent launch order; heuristic scoring is bypassed.",
            ),
            localized_active("Recent order"),
            localized_active("Blank query"),
            localized_active("Preserve recent_items.txt order"),
        ),
        ResultRankingKind::QueryLaunchRule => (
            localized_active(
                "The matching Query Launch Rule is pinned ahead of heuristic results.",
            ),
            localized_active("Query Launch Rule priority"),
            localized_active("Exact normalized query rule matched"),
            localized_active("Priority tier before heuristic score sorting"),
        ),
        ResultRankingKind::Heuristic => (
            localized_active("Heuristic scoring is active."),
            localized_active("Heuristic score"),
            localized_active("Normal search result"),
            localized_active("Sum enabled scoring contributions"),
        ),
    };
    ScoreExplanation {
        ranking_kind,
        query: query.to_string(),
        item_path: item_path.to_string(),
        search_root: String::new(),
        relative_depth: 0,
        final_score,
        summary: summary.to_string(),
        rows: vec![ScoreExplanationRow {
            rule: rule.to_string(),
            input_condition: condition.to_string(),
            formula: formula.to_string(),
            score: final_score,
        }],
    }
}

fn special_item_score_explanation<I: SearchItemView + ?Sized>(
    ranking_kind: ResultRankingKind,
    query: &str,
    item: &I,
    final_score: i32,
) -> ScoreExplanation {
    let mut explanation = special_score_explanation(
        ranking_kind,
        query,
        &item.path().to_string_lossy(),
        final_score,
    );
    explanation.search_root = item
        .search_root()
        .map(|root| root.to_string_lossy().to_string())
        .unwrap_or_default();
    explanation.relative_depth = item.relative_depth();
    explanation
}
fn explanation_row(
    rule: impl Into<String>,
    input_condition: impl Into<String>,
    formula: impl Into<String>,
    score: i32,
) -> ScoreExplanationRow {
    ScoreExplanationRow {
        rule: rule.into(),
        input_condition: input_condition.into(),
        formula: formula.into(),
        score,
    }
}

fn rule_condition(enabled: bool, matched: bool, matched_text: impl Into<String>) -> String {
    if !enabled {
        localized_active("Disabled").to_string()
    } else if matched {
        matched_text.into()
    } else {
        localized_active("Not matched").to_string()
    }
}

fn append_text_rule_rows(
    rows: &mut Vec<ScoreExplanationRow>,
    label: &str,
    query: &str,
    candidate: &str,
    scoring: &ScoringConfig,
    contributes: bool,
) -> Option<i32> {
    let parts = fuzzy_score_parts(query, candidate, scoring)?;
    let exact = candidate == query;
    let exact_word = exact_word_match(candidate, query);
    let prefix = candidate.starts_with(query);
    let boundary = word_boundary_match(candidate, query);
    let acronym = acronym_starts_with(candidate, query);
    let consecutive = candidate.contains(query);
    let total = score_text_with_config(query, candidate, false, scoring)?;
    let score_or_zero = |score| if contributes { score } else { 0 };
    let char_positions = parts
        .positions
        .iter()
        .map(|position| candidate[..*position].chars().count().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    rows.push(explanation_row(
        localized_active_format1("{} fuzzy positions", label),
        localized_active_format3(
            "Query '{}' in '{}' at [{}]",
            query,
            candidate,
            char_positions,
        ),
        localized_active("Best ordered character route"),
        0,
    ));
    rows.push(explanation_row(
        localized_active_format1("{} length component", label),
        localized_active_format2(
            "{} query chars / {} candidate chars",
            parts.query_length,
            parts.candidate_length,
        ),
        format!(
            "{} * {} / {}",
            parts.query_length,
            scoring.length_score_weight.max(0),
            parts.candidate_length
        ),
        score_or_zero(parts.length_score),
    ));
    rows.push(explanation_row(
        localized_active_format1("{} leftmost component", label),
        localized_active_format1(
            "First matched character index {}",
            parts.first_character_index,
        ),
        format!(
            "max(0, {} - {} * {})",
            scoring.leftmost_match_bonus,
            parts.first_character_index,
            scoring.leftmost_distance_penalty
        ),
        score_or_zero(parts.leftmost_score),
    ));
    rows.push(explanation_row(
        localized_active_format1("{} compact component", label),
        localized_active_format1("{} adjacent pairs", parts.adjacent_pairs),
        format!(
            "{} * {} / max(1, {} - 1)",
            parts.adjacent_pairs,
            scoring.compact_match_bonus.max(0),
            parts.query_length
        ),
        score_or_zero(parts.compact_score),
    ));
    for (rule, enabled, matched, value, formula) in [
        (
            "Exact Match Bonus",
            scoring.exact_match_bonus != 0,
            exact,
            scoring.exact_match_bonus,
            "candidate == query",
        ),
        (
            "Exact Word Bonus",
            scoring.exact_word_bonus != 0,
            exact_word,
            scoring.exact_word_bonus,
            "query is a complete word",
        ),
        (
            "Prefix Match Bonus",
            scoring.prefix_match_bonus != 0,
            prefix,
            scoring.prefix_match_bonus,
            "candidate starts with query",
        ),
        (
            "Word Boundary Bonus",
            scoring.word_boundary_bonus != 0,
            boundary,
            scoring.word_boundary_bonus,
            "query starts at a word boundary",
        ),
        (
            "Consecutive Match Bonus",
            scoring.consecutive_match_bonus != 0,
            consecutive,
            scoring.consecutive_match_bonus,
            "candidate contains query contiguously",
        ),
        (
            "Acronym Match Bonus",
            scoring.acronym_match_bonus != 0,
            acronym,
            scoring.acronym_match_bonus,
            "query matches word initials",
        ),
    ] {
        rows.push(explanation_row(
            localized_active_format2("{} {}", label, localized_active(rule)),
            rule_condition(
                enabled,
                matched,
                localized_active_format1("Matched '{}'", query),
            ),
            localized_active(formula),
            score_or_zero(if enabled && matched { value } else { 0 }),
        ));
    }
    if !contributes {
        rows.push(explanation_row(
            localized_active_format1("{} calculated score", label),
            localized_active_format1("Calculated {}; route not selected", total),
            localized_active("Informational only"),
            0,
        ));
    }
    Some(total)
}

fn heuristic_score_explanation<I: SearchItemView + ?Sized>(
    item: &I,
    spec: &SearchQuerySpec,
    recent_map: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
    breakdown: &ScoreBreakdown,
) -> ScoreExplanation {
    let query = spec.folded_search_text.trim();
    let candidate = item.folded_title();
    let whole = score_text_with_config(query, candidate, false, scoring);
    let token_scores = query
        .split_whitespace()
        .map(|token| {
            (
                token,
                score_text_with_config(token, candidate, false, scoring),
            )
        })
        .collect::<Vec<_>>();
    let token_average =
        if token_scores.len() > 1 && token_scores.iter().all(|(_, score)| score.is_some()) {
            Some(
                token_scores
                    .iter()
                    .map(|(_, score)| score.unwrap_or(0))
                    .sum::<i32>()
                    / token_scores.len() as i32,
            )
        } else {
            None
        };
    let whole_selected = whole >= token_average;
    let mut rows = Vec::new();
    if query.is_empty() {
        rows.push(explanation_row(
            localized_active("Blank query baseline"),
            localized_active("Blank heuristic query"),
            localized_active("Baseline match score"),
            breakdown.text_score,
        ));
    } else {
        let _ = append_text_rule_rows(
            &mut rows,
            localized_active("Whole query"),
            query,
            candidate,
            scoring,
            whole_selected,
        );
        for (index, (token, score)) in token_scores.iter().enumerate() {
            rows.push(explanation_row(
                localized_active_format1("Token {} score", index + 1),
                score.map_or_else(
                    || localized_active("Not matched").to_string(),
                    |value| {
                        localized_active_format2("'{}' matched with raw score {}", token, value)
                    },
                ),
                localized_active("Token score; averaged only when all tokens match"),
                0,
            ));
        }
        if !whole_selected {
            rows.push(explanation_row(
                localized_active("Selected text route"),
                localized_active("Token-average route selected"),
                localized_active_format2(
                    "max(whole {:?}, token average {:?})",
                    format!("{whole:?}"),
                    format!("{token_average:?}"),
                ),
                breakdown.text_score,
            ));
        } else {
            rows.push(explanation_row(
                localized_active("Selected text route"),
                localized_active("Whole-query route selected"),
                localized_active_format2(
                    "max(whole {:?}, token average {:?})",
                    format!("{whole:?}"),
                    format!("{token_average:?}"),
                ),
                0,
            ));
        }
        let stem_exact = !item.is_dir()
            && !item.folded_stem().is_empty()
            && item.folded_stem() == query
            && candidate != query;
        rows.push(explanation_row(
            localized_active("Filename stem exact bonus"),
            rule_condition(
                scoring.exact_match_bonus != 0,
                stem_exact,
                localized_active("Stem exactly matched without extension"),
            ),
            localized_active("Add Exact Match Bonus once more for an exact file stem"),
            if stem_exact {
                scoring.exact_match_bonus
            } else {
                0
            },
        ));
    }
    rows.push(explanation_row(
        localized_active("Parent-folder query match"),
        rule_condition(
            scoring.explicit_folder_name_match_adjustment != 0,
            breakdown.path_score != 0,
            localized_active("Parent folder matched query"),
        ),
        localized_active("Explicit Folder Name Match Adjustment"),
        breakdown.path_score,
    ));
    rows.push(explanation_row(
        localized_active("Search Folder score"),
        item.search_root().map_or_else(
            || localized_active("No owning Search Folder").to_string(),
            |root| root.to_string_lossy().to_string(),
        ),
        localized_active("Configured Search Folder score"),
        breakdown.index_score,
    ));
    let folded_path = fold_text(&item.path().to_string_lossy());
    let any_modifier_match = any_modifier_keywords_match(
        scoring
            .pattern_rules
            .iter()
            .map(|rule| rule.modifiers.as_slice()),
        &spec.scoring_modifiers,
    );
    for rule in &scoring.pattern_rules {
        let modifiers_apply =
            modifier_keywords_apply(&rule.modifiers, &spec.scoring_modifiers, any_modifier_match);
        let matched = modifiers_apply && wildcard_rule_matches(rule, item.path(), &folded_path);
        rows.push(explanation_row(
            localized_active_format1("Pattern {}", &rule.pattern),
            if !modifiers_apply {
                localized_active_format1(
                    "Modifier condition not met: {}",
                    search_folder_modifier_display(&rule.modifiers),
                )
            } else if matched {
                localized_active("Matched").to_string()
            } else {
                localized_active("Not matched").to_string()
            },
            localized_active_format1("Pattern score {}", rule.score),
            if matched { rule.score } else { 0 },
        ));
    }
    let recent_key = recent_item_key(item.path());
    let raw_history = recent_map
        .get(&recent_key)
        .map(|entry| entry.score)
        .unwrap_or(0.0);
    rows.push(explanation_row(
        localized_active("History raw score"),
        if raw_history > 0.0 {
            localized_active_format1("Raw history score {}", raw_history)
        } else {
            localized_active("Not matched").to_string()
        },
        localized_active_format1(
            "min(raw score, Recent Score Ceiling {})",
            scoring.recent_score_ceiling,
        ),
        breakdown.history_score,
    ));
    rows.push(explanation_row(
        localized_active("Modified-date recency"),
        if !scoring.recency_date_enabled {
            localized_active("Disabled").to_string()
        } else if item.modified_at_unix_seconds().is_some() {
            localized_active("Modified date available").to_string()
        } else {
            localized_active("Not matched").to_string()
        },
        localized_active("100%, 50%, 25%, or 0% by age band"),
        breakdown.recency_score,
    ));
    rows.push(explanation_row(
        localized_active("Folder Score As % of File Score"),
        rule_condition(
            scoring.folder_score_as_file_score_percent != 100,
            item.is_dir(),
            localized_active("Item is a folder"),
        ),
        localized_active_format1(
            "Folder score is {}% of file score",
            scoring.folder_score_as_file_score_percent.max(0),
        ),
        breakdown.folder_score,
    ));
    rows.push(explanation_row(
        localized_active("Path Depth Penalty"),
        localized_active_format1("Relative depth {}", item.relative_depth()),
        format!(
            "-{} * {}",
            item.relative_depth(),
            scoring.path_depth_penalty.max(0)
        ),
        -breakdown.path_penalty,
    ));
    rows.push(explanation_row(
        localized_active("Final score"),
        localized_active("Sum of contribution rows"),
        format!(
            "{} + {} + {} + {} + {} + {} + {} - {}",
            breakdown.text_score,
            breakdown.path_score,
            breakdown.index_score,
            breakdown.pattern_score,
            breakdown.history_score,
            breakdown.recency_score,
            breakdown.folder_score,
            breakdown.path_penalty
        ),
        0,
    ));
    ScoreExplanation {
        ranking_kind: ResultRankingKind::Heuristic,
        query: spec.raw.clone(),
        item_path: item.path().to_string_lossy().to_string(),
        search_root: item.search_root().map(|root| root.to_string_lossy().to_string()).unwrap_or_default(),
        relative_depth: item.relative_depth(),
        final_score: breakdown.final_score,
        summary: localized_active(
            "Heuristic score uses shared ranking primitives; all enabled contributions are shown below.",
        )
        .to_string(),
        rows,
    }
}

#[cfg(test)]
pub(crate) fn ranked_score(
    query: &str,
    scoring_modifiers: &[String],
    item: &LaunchItem,
    recent_items: &HashMap<String, RecentEntry>,
) -> Option<i32> {
    ranked_score_with_config(
        query,
        scoring_modifiers,
        item,
        recent_items,
        &ScoringConfig::default(),
    )
}

#[cfg(test)]
pub(crate) fn ranked_score_with_config(
    query: &str,
    scoring_modifiers: &[String],
    item: &LaunchItem,
    recent_items: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
) -> Option<i32> {
    let spec = SearchQuerySpec {
        raw: query.to_string(),
        search_text: query.to_string(),
        folded_search_text: fold_text(query),
        modifiers: Vec::new(),
        scoring_modifiers: scoring_modifiers.to_vec(),
        show_all: false,
        mode: SearchQueryMode::NormalSearch,
        directory: None,
        scoring_time_unix_seconds: current_unix_seconds(),
    };
    score_breakdown_with_config(item, &spec, recent_items, scoring, false)
        .map(|breakdown| breakdown.final_score)
}

#[derive(Clone)]
pub(crate) struct ScoreBreakdown {
    pub(crate) result: SearchResult,
    pub(crate) text_score: i32,
    pub(crate) path_score: i32,
    pub(crate) index_score: i32,
    pub(crate) pattern_score: i32,
    pub(crate) history_score: i32,
    pub(crate) recency_score: i32,
    pub(crate) folder_score: i32,
    pub(crate) path_penalty: i32,
    pub(crate) final_score: i32,
}

#[allow(clippy::too_many_arguments)]
fn score_components_with_prepared_query<I: SearchItemView + ?Sized>(
    item: &I,
    spec: &SearchQuerySpec,
    prepared_query: &PreparedQuery,
    path_key: &str,
    text_score: i32,
    recent_map: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
) -> HeuristicScoreComponents {
    let path_score =
        score_item_path_with_config(prepared_query.folded_search_text(), item, scoring);
    let index_score = item.index_score();
    let pattern_score = prepared_query.pattern_score(item.path(), item.folded_title());
    let history_score = recent_map
        .get(path_key)
        .map(|entry| recent_entry_score_with_config(entry, scoring))
        .unwrap_or(0);
    let recency_score = recency_date_score_at(
        item.modified_at_unix_seconds(),
        spec.scoring_time_unix_seconds,
        scoring,
    );
    let path_penalty = path_penalty(item, scoring);
    let file_score = text_score
        .saturating_add(path_score)
        .saturating_add(index_score)
        .saturating_add(pattern_score)
        .saturating_add(history_score)
        .saturating_add(recency_score)
        .saturating_sub(path_penalty);
    let folder_score = folder_score_adjustment(item, file_score, scoring);
    let final_score = file_score.saturating_add(folder_score);
    HeuristicScoreComponents {
        text_score,
        path_score,
        index_score,
        pattern_score,
        history_score,
        recency_score,
        folder_score,
        path_penalty,
        final_score,
    }
}

fn ranked_heuristic_candidate(
    item: LaunchItem,
    components: HeuristicScoreComponents,
    from_history: bool,
) -> RankedCandidate {
    RankedCandidate {
        payload: RankedCandidatePayload::Heuristic {
            item,
            components,
            from_history,
        },
        ranking_kind: ResultRankingKind::Heuristic,
        display_score: components.final_score,
        score: components.final_score,
    }
}

#[allow(clippy::too_many_arguments)]
fn materialize_ranked_candidate(
    candidate: &RankedCandidate,
    spec: &SearchQuerySpec,
    recent_map: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
    include_score_detail: bool,
    include_explanation: bool,
) -> SearchResult {
    match &candidate.payload {
        RankedCandidatePayload::Prebuilt(result) => result.clone(),
        RankedCandidatePayload::Heuristic {
            item,
            components,
            from_history,
        } => {
            let mut breakdown = ScoreBreakdown {
                result: SearchResult {
                    title: item.title.clone(),
                    subtitle: item.subtitle.clone(),
                    target: LaunchTarget::Path(item.path.clone()),
                    is_dir: item.is_dir,
                    from_history: *from_history,
                    from_query_launch_rule: false,
                    ranking_kind: candidate.ranking_kind,
                    explanation: None,
                    display_score: candidate.display_score,
                    score_detail: String::new(),
                    score: candidate.score,
                },
                text_score: components.text_score,
                path_score: components.path_score,
                index_score: components.index_score,
                pattern_score: components.pattern_score,
                history_score: components.history_score,
                recency_score: components.recency_score,
                folder_score: components.folder_score,
                path_penalty: components.path_penalty,
                final_score: components.final_score,
            };
            if include_explanation {
                breakdown.result.explanation = Some(heuristic_score_explanation(
                    item, spec, recent_map, scoring, &breakdown,
                ));
            }
            if include_score_detail {
                breakdown.result.score_detail = score_detail_text_from_components(components);
            }
            breakdown.result
        }
    }
}

fn materialize_ranked_candidates(
    candidates: &[RankedCandidate],
    spec: &SearchQuerySpec,
    recent_map: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
    include_score_detail: bool,
    include_explanation: bool,
) -> Vec<SearchResult> {
    candidates
        .iter()
        .map(|candidate| {
            materialize_ranked_candidate(
                candidate,
                spec,
                recent_map,
                scoring,
                include_score_detail,
                include_explanation,
            )
        })
        .collect()
}

#[allow(dead_code, clippy::too_many_arguments)]
fn score_breakdown_with_prepared_query<I: SearchItemView + ?Sized>(
    item: &I,
    spec: &SearchQuerySpec,
    prepared_query: &PreparedQuery,
    path_key: &str,
    text_score: i32,
    recent_map: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
    from_history: bool,
    include_score_detail: bool,
    include_explanation: bool,
) -> Option<ScoreBreakdown> {
    let components = score_components_with_prepared_query(
        item,
        spec,
        prepared_query,
        path_key,
        text_score,
        recent_map,
        scoring,
    );
    let candidate = ranked_heuristic_candidate(
        LaunchItem {
            title: item.title().to_string(),
            subtitle: item.subtitle().to_string(),
            path: item.path().to_path_buf(),
            is_dir: item.is_dir(),
            folded_title: item.folded_title().to_string(),
            folded_stem: item.folded_stem().to_string(),
            folded_parent: item.folded_parent().to_string(),
            index_score: item.index_score(),
            modified_at_unix_seconds: item.modified_at_unix_seconds(),
            search_root: item.search_root().map(Path::to_path_buf),
            relative_depth: item.relative_depth(),
        },
        components,
        from_history,
    );
    let result = materialize_ranked_candidate(
        &candidate,
        spec,
        recent_map,
        scoring,
        include_score_detail,
        include_explanation,
    );
    Some(ScoreBreakdown {
        result,
        text_score: components.text_score,
        path_score: components.path_score,
        index_score: components.index_score,
        pattern_score: components.pattern_score,
        history_score: components.history_score,
        recency_score: components.recency_score,
        folder_score: components.folder_score,
        path_penalty: components.path_penalty,
        final_score: components.final_score,
    })
}

pub(crate) fn score_breakdown_with_config<I: SearchItemView + ?Sized>(
    item: &I,
    spec: &SearchQuerySpec,
    recent_map: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
    from_history: bool,
) -> Option<ScoreBreakdown> {
    score_breakdown_with_config_and_detail(
        item,
        spec,
        recent_map,
        scoring,
        from_history,
        true,
        true,
    )
}

pub(crate) fn score_breakdown_with_config_and_detail<I: SearchItemView + ?Sized>(
    item: &I,
    spec: &SearchQuerySpec,
    recent_map: &HashMap<String, RecentEntry>,
    scoring: &ScoringConfig,
    from_history: bool,
    include_score_detail: bool,
    include_explanation: bool,
) -> Option<ScoreBreakdown> {
    let text_score = if spec.folded_search_text.is_empty() {
        1
    } else {
        score_item_text_with_config(&spec.folded_search_text, item, scoring)?
    };
    let path_score = score_item_path_with_config(&spec.folded_search_text, item, scoring);
    let index_score = item.index_score();
    let pattern_score =
        pattern_score_with_config_and_modifiers(item.path(), scoring, &spec.scoring_modifiers);
    let history_score = recent_item_score_with_config(item.path(), recent_map, scoring);
    let recency_score = recency_date_score_at(
        item.modified_at_unix_seconds(),
        spec.scoring_time_unix_seconds,
        scoring,
    );
    let path_penalty = path_penalty(item, scoring);
    let file_score = text_score
        .saturating_add(path_score)
        .saturating_add(index_score)
        .saturating_add(pattern_score)
        .saturating_add(history_score)
        .saturating_add(recency_score)
        .saturating_sub(path_penalty);
    let folder_score = folder_score_adjustment(item, file_score, scoring);
    let final_score = file_score.saturating_add(folder_score);
    let mut breakdown = ScoreBreakdown {
        result: SearchResult {
            title: item.title().to_string(),
            subtitle: item.subtitle().to_string(),
            target: LaunchTarget::Path(item.path().to_path_buf()),
            is_dir: item.is_dir(),
            from_history,
            from_query_launch_rule: false,
            ranking_kind: ResultRankingKind::Heuristic,
            explanation: None,
            display_score: final_score,
            score_detail: String::new(),
            score: final_score,
        },
        text_score,
        path_score,
        index_score,
        pattern_score,
        history_score,
        recency_score,
        folder_score,
        path_penalty,
        final_score,
    };
    if include_explanation {
        breakdown.result.explanation = Some(heuristic_score_explanation(
            item, spec, recent_map, scoring, &breakdown,
        ));
    }
    if include_score_detail {
        breakdown.result.score_detail = score_detail_text(&breakdown);
    }
    Some(breakdown)
}

#[cfg(feature = "debug-tools")]
pub(crate) fn debug_search_breakdowns(query: &str, limit: usize) -> Vec<ScoreBreakdown> {
    let scoring = load_scoring_config();
    let spec = parse_search_query(query).effective_for_scoring(&scoring);
    let recent_items = load_recent_items();
    let recent_map = recent_lookup(&recent_items);
    let root_plan = RootOwnershipPlan::build(&load_index_roots());
    let mut seen = HashSet::new();
    let mut breakdowns = collect_recent_path_match_breakdowns(
        &spec,
        &root_plan,
        &recent_items,
        &recent_map,
        &scoring,
        limit,
    );

    for breakdown in &breakdowns {
        if let Some(path) = result_target_path(&breakdown.result) {
            seen.insert(recent_item_key(&path));
        }
    }

    let any_root_modifier_match = any_modifier_keywords_match(
        root_plan
            .entries()
            .iter()
            .map(|entry| entry.root.keywords.as_slice()),
        &spec.modifiers,
    );
    for entry in eligible_search_roots(&root_plan, &spec, any_root_modifier_match) {
        collect_debug_root_breakdowns(
            &entry.path,
            entry.effective_depth,
            entry.effective_root.max_depth,
            &entry.effective_root,
            &scoring,
            &recent_map,
            &spec,
            &mut seen,
            &mut breakdowns,
            &entry.exclusions,
        );
    }

    breakdowns.sort_by_cached_key(|breakdown| {
        (
            Reverse(breakdown.final_score),
            breakdown.result.title.to_lowercase(),
        )
    });
    breakdowns.truncate(limit.max(1));
    breakdowns
}

#[cfg(feature = "debug-tools")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_debug_root_breakdowns(
    path: &Path,
    depth: usize,
    max_depth: usize,
    root: &IndexRoot,
    scoring: &ScoringConfig,
    recent_map: &HashMap<String, RecentEntry>,
    spec: &SearchQuerySpec,
    seen: &mut HashSet<String>,
    breakdowns: &mut Vec<ScoreBreakdown>,
    exclusions: &[String],
) {
    if max_depth != SEARCH_DEPTH_ALL && depth > max_depth.saturating_add(1) {
        return;
    }
    let key = recent_item_key(path);
    if seen.insert(key) {
        if let Some(item) = launch_item_from_path(path.to_path_buf(), root, scoring) {
            if let Some(breakdown) =
                score_breakdown_with_config(&item, spec, recent_map, scoring, false)
            {
                breakdowns.push(breakdown);
            }
        }
    }

    if max_depth != SEARCH_DEPTH_ALL && depth > max_depth {
        return;
    }

    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if path_is_in_excluded_subtree(&entry_path, exclusions) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let is_dir = file_type.is_dir();
        if is_dir && is_reparse_point(&entry_path) {
            continue;
        }
        let key = recent_item_key(&entry_path);
        if seen.insert(key) {
            if let Some(item) = launch_item_from_path(entry_path.clone(), root, scoring) {
                if let Some(breakdown) =
                    score_breakdown_with_config(&item, spec, recent_map, scoring, false)
                {
                    breakdowns.push(breakdown);
                }
            }
        }
        if is_dir && (max_depth == SEARCH_DEPTH_ALL || depth < max_depth) {
            collect_debug_root_breakdowns(
                &entry_path,
                depth.saturating_add(1),
                max_depth,
                root,
                scoring,
                recent_map,
                spec,
                seen,
                breakdowns,
                exclusions,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn launcher_text_keys_route_to_search_without_shortcut_modifiers() {
        assert!(should_route_launcher_key_to_search(
            b'A' as u16, false, false, false
        ));
        assert!(should_route_launcher_key_to_search(
            VK_SPACE, false, false, false
        ));
        assert!(should_route_launcher_key_to_search(
            VK_BACK, false, false, false
        ));
        assert!(should_route_launcher_key_to_search(
            VK_DELETE, false, false, false
        ));
        assert!(should_route_launcher_key_to_search(
            VK_LEFT, false, false, false
        ));
        assert!(should_route_launcher_key_to_search(
            VK_PROCESSKEY, false, false, false
        ));
    }

    #[test]
    fn launcher_navigation_and_shortcut_keys_keep_existing_targets() {
        assert!(!should_route_launcher_key_to_search(
            VK_TAB, false, false, false
        ));
        assert!(!should_route_launcher_key_to_search(
            VK_RETURN, false, false, false
        ));
        assert!(!should_route_launcher_key_to_search(
            VK_ESCAPE, false, false, false
        ));
        assert!(!should_route_launcher_key_to_search(
            VK_UP, false, false, false
        ));
        assert!(!should_route_launcher_key_to_search(
            VK_F1, false, false, false
        ));
        assert!(!should_route_launcher_key_to_search(
            b'A' as u16, true, false, false
        ));
        assert!(!should_route_launcher_key_to_search(
            b'A' as u16, false, true, false
        ));
        assert!(!should_route_launcher_key_to_search(
            b'A' as u16, false, false, true
        ));
    }

    static SEARCH_GENERATION_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock_search_generation() -> MutexGuard<'static, ()> {
        SEARCH_GENERATION_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn temporary_search_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::current_dir()
            .unwrap()
            .join("TEMP")
            .join("search-plan-tests")
            .join(format!("{name}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn plan_test_root(path: PathBuf, raw: &str, keywords: &[&str]) -> IndexRoot {
        IndexRoot {
            raw: raw.to_string(),
            path: Some(path),
            enabled: true,
            score: 0,
            max_depth: SEARCH_DEPTH_ALL,
            label: String::new(),
            keywords: keywords.iter().map(|value| value.to_string()).collect(),
        }
    }

    fn refinement_test_request(
        root_plan: Arc<RootOwnershipPlan>,
        query: &str,
    ) -> SearchWorkerRequest {
        SearchWorkerRequest {
            generation: TEST_UNCANCELLED_GENERATION,
            hwnd_value: 0,
            spec: parse_search_query(query),
            root_plan,
            scoring: Arc::new(ScoringConfig::default()),
            recent_items: Arc::new(Vec::new()),
            query_launch_rules: Arc::new(Vec::new()),
            effective_limit: DEFAULT_RESULT_LIMIT,
            search_threads: 2,
            include_score_detail: false,
            include_explanation: false,
        }
    }

    fn wait_for_refinement_progress(session: &RefinementSession) {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            let metrics = session.metrics();
            if metrics.filesystem_entries > 0 && !metrics.scan_done {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("refinement scan did not enter an in-flight state");
    }

    #[test]
    fn visual_refinement_requires_only_the_last_token_to_extend() {
        let base = parse_search_query("google chr");
        assert!(query_specs_allow_refinement(
            &base,
            &parse_search_query("google chro"),
        ));
        assert!(!query_specs_allow_refinement(
            &base,
            &parse_search_query("google chrome browser"),
        ));
        assert!(!query_specs_allow_refinement(
            &base,
            &parse_search_query("bing chro"),
        ));
        assert!(!query_specs_allow_refinement(
            &base,
            &parse_search_query("google chro +work"),
        ));
    }

    #[test]
    fn refinement_progress_requires_item_and_time_thresholds() {
        let recent = Instant::now();
        let elapsed = Instant::now() - SEARCH_BATCH_INTERVAL;

        assert!(!refinement_progress_due(SEARCH_BATCH_ITEM_STEP, recent));
        assert!(!refinement_progress_due(
            SEARCH_BATCH_ITEM_STEP - 1,
            elapsed
        ));
        assert!(refinement_progress_due(SEARCH_BATCH_ITEM_STEP, elapsed));
    }

    #[test]
    fn completed_refinement_reuses_cache_without_rescanning() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("refinement-completed");
        for name in ["chrome.exe", "chromium.txt", "chr-only.txt", "other.txt"] {
            fs::write(path.join(name), b"candidate").unwrap();
        }
        let root_plan = Arc::new(RootOwnershipPlan::build(&[plan_test_root(
            path.clone(),
            "refinement-completed",
            &[],
        )]));
        let first = refinement_test_request(Arc::clone(&root_plan), "chr");
        let mut session = RefinementSession::start(first.clone());
        assert!(session.wait_until_complete(Duration::from_secs(10)));
        let before = session.metrics();

        let mut refined = first;
        refined.generation = refined.generation.wrapping_add(1);
        refined.spec = parse_search_query("chro");
        assert!(session.update(refined));
        let after = session.metrics();

        assert_eq!(after.filesystem_entries, before.filesystem_entries);
        assert_eq!(after.reused_candidates, before.cache_candidates);
        assert_eq!(after.query_version, 2);
        assert!(after.scan_done);
        session.shutdown();
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn in_flight_refinement_rescores_stale_candidates_for_latest_query() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("refinement-in-flight");
        for index in 0..80 {
            fs::write(path.join(format!("chr-only-{index:03}.txt")), b"candidate").unwrap();
        }
        fs::write(path.join("chromium.txt"), b"candidate").unwrap();
        let root_plan = Arc::new(RootOwnershipPlan::build(&[plan_test_root(
            path.clone(),
            "refinement-in-flight",
            &[],
        )]));
        REFINEMENT_TEST_MATCH_DELAY_MS.store(2, Ordering::Relaxed);
        let first = refinement_test_request(root_plan, "chr");
        let mut session = RefinementSession::start(first.clone());
        wait_for_refinement_progress(&session);

        let mut refined = first;
        refined.generation = refined.generation.wrapping_add(1);
        refined.spec = parse_search_query("chrom");
        assert!(session.update(refined));
        assert!(session.wait_until_complete(Duration::from_secs(10)));
        REFINEMENT_TEST_MATCH_DELAY_MS.store(0, Ordering::Relaxed);

        let state = session.shared.state.lock().unwrap();
        assert_eq!(state.query_version, 2);
        let materialized = materialize_ranked_candidates(
            &state.best,
            &state.effective_spec,
            &state.recent_map,
            &state.request.scoring,
            false,
            false,
        );
        assert!(materialized
            .iter()
            .filter_map(result_target_path)
            .all(|path| path
                .file_name()
                .is_some_and(|name| { fold_text(&name.to_string_lossy()).contains("chrom") })));
        drop(state);
        session.shutdown();
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn cache_cap_finishes_current_query_then_forces_full_scan_fallback() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("refinement-cap");
        for index in 0..12 {
            fs::write(path.join(format!("chrome-{index:03}.txt")), b"candidate").unwrap();
        }
        let root_plan = Arc::new(RootOwnershipPlan::build(&[plan_test_root(
            path.clone(),
            "refinement-cap",
            &[],
        )]));
        let first = refinement_test_request(root_plan, "chr");
        let mut session = RefinementSession::start_with_cache_limit(first.clone(), 1);
        assert!(session.wait_until_complete(Duration::from_secs(10)));
        let metrics = session.metrics();
        assert!(metrics.scan_done);
        assert!(metrics.full_scan_fallback);
        assert_eq!(metrics.cache_candidates, 0);
        assert!(!session.shared.state.lock().unwrap().best.is_empty());

        let mut refined = first;
        refined.spec = parse_search_query("chro");
        assert!(!session.can_refine(&refined));
        assert!(!session.update(refined));
        session.shutdown();
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn show_all_cache_overflow_runs_full_scan_fallback() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("zoom-fallback");
        for index in 0..8 {
            fs::write(path.join(format!("chrome-{index:03}.txt")), b"candidate").unwrap();
        }
        let root_plan = Arc::new(RootOwnershipPlan::build(&[plan_test_root(
            path.clone(),
            "refinement-show-all-fallback",
            &[],
        )]));
        let generation = TEST_UNCANCELLED_GENERATION;
        ACTIVE_SEARCH_GENERATION.store(generation, Ordering::Relaxed);
        if let Ok(mut pending) = pending_search_slot().lock() {
            pending.clear();
        }
        let mut request = refinement_test_request(root_plan, "chr");
        request.generation = generation;
        request.hwnd_value = 1;
        request.effective_limit = usize::MAX;
        publish_refinement_snapshot(RefinementPublish {
            request,
            results: Vec::new(),
            all_results: None,
            scanned_total: 0,
            done: true,
        });

        let batch = pending_search_slot()
            .lock()
            .unwrap()
            .pop()
            .expect("full scan fallback final batch");
        assert!(batch.done);
        assert_eq!(batch.scanned_total, 9);
        let Some(ResultStoreCompletion::Ready(manifest)) = batch.result_store else {
            panic!("full scan fallback did not create a result store");
        };
        let reader = ResultStoreReader::open(manifest).unwrap();
        let chrome_results = (0..reader.len())
            .filter_map(|index| reader.get(index).ok().flatten())
            .filter(|result| result.title.starts_with("chrome-"))
            .count();
        assert_eq!(chrome_results, 8);
        drop(reader);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn refinement_compatibility_covers_all_invalidation_snapshots() {
        let path = temporary_search_root("refinement-compatibility");
        let root_plan = Arc::new(RootOwnershipPlan::build(&[plan_test_root(
            path.clone(),
            "refinement-compatibility",
            &[],
        )]));
        let base = refinement_test_request(root_plan, "one chr");
        let mut compatible = base.clone();
        compatible.spec = parse_search_query("one chro");
        compatible.effective_limit = usize::MAX;
        compatible.include_score_detail = true;
        compatible.include_explanation = true;
        assert!(refinement_requests_compatible(&base, &compatible));

        let mut cases = Vec::new();
        let mut changed = base.clone();
        changed.spec = parse_search_query("one xyz");
        cases.push(changed);
        let mut changed = base.clone();
        changed.spec = parse_search_query("one chr extra");
        cases.push(changed);
        let mut changed = base.clone();
        changed.spec.mode = SearchQueryMode::DirectoryBrowse;
        cases.push(changed);
        let mut changed = base.clone();
        changed.spec.modifiers.push("root".to_string());
        cases.push(changed);
        let mut changed = base.clone();
        changed.spec.scoring_modifiers.push("score".to_string());
        cases.push(changed);
        let mut changed = base.clone();
        changed.root_plan = Arc::new(RootOwnershipPlan::default());
        cases.push(changed);
        let mut changed = base.clone();
        changed.scoring = Arc::new(ScoringConfig::default());
        cases.push(changed);
        let mut changed = base.clone();
        changed.recent_items = Arc::new(Vec::new());
        cases.push(changed);
        let mut changed = base.clone();
        changed.query_launch_rules = Arc::new(Vec::new());
        cases.push(changed);
        let mut changed = base.clone();
        changed.search_threads = changed.search_threads.saturating_add(1);
        cases.push(changed);

        for changed in cases {
            assert!(!refinement_requests_compatible(&base, &changed));
        }
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn show_all_and_score_detail_rebuild_from_completed_cache() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("refinement-show-all");
        for index in 0..20 {
            fs::write(path.join(format!("chrome-{index:03}.txt")), b"candidate").unwrap();
        }
        let root_plan = Arc::new(RootOwnershipPlan::build(&[plan_test_root(
            path.clone(),
            "refinement-show-all",
            &[],
        )]));
        let mut first = refinement_test_request(root_plan, "chr");
        first.effective_limit = 3;
        let mut session = RefinementSession::start(first.clone());
        assert!(session.wait_until_complete(Duration::from_secs(10)));
        let scanned = session.metrics().filesystem_entries;

        let mut show_all = first;
        show_all.effective_limit = usize::MAX;
        show_all.include_score_detail = true;
        assert!(session.update(show_all));
        let state = session.shared.state.lock().unwrap();
        let all_ranked = refinement_all_results(&state);
        let all_results = materialize_ranked_candidates(
            &all_ranked,
            &state.effective_spec,
            &state.recent_map,
            &state.request.scoring,
            true,
            false,
        );
        assert_eq!(state.scanned_total, scanned);
        assert_eq!(all_results.len(), 20);
        assert!(all_results
            .iter()
            .all(|result| !result.score_detail.is_empty()));
        drop(state);
        session.shutdown();
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn invalidate_and_shutdown_clear_refinement_session_state() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("refinement-cleanup");
        fs::write(path.join("chrome.txt"), b"candidate").unwrap();
        let root_plan = Arc::new(RootOwnershipPlan::build(&[plan_test_root(
            path.clone(),
            "refinement-cleanup",
            &[],
        )]));
        let request = refinement_test_request(root_plan, "chr");
        let mut session = RefinementSession::start(request);
        assert!(session.wait_until_complete(Duration::from_secs(10)));
        session.invalidate();
        {
            let state = session.shared.state.lock().unwrap();
            assert!(state.shutdown);
            assert!(state.cache.is_empty());
            assert!(state.best.is_empty());
            assert!(state.scan_seen.is_empty());
            assert!(state.ranking_seen.is_empty());
        }
        session.shutdown();
        assert!(session.handle.is_none());
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn lazy_ranked_materialization_preserves_detail_and_signature() {
        let scoring = ScoringConfig::default();
        let root = plan_test_root(PathBuf::from(r"C:\ROOT"), "root", &[]);
        let item = launch_item_from_path_with_type(
            PathBuf::from(r"C:\ROOT\Tools\chrome.exe"),
            false,
            &root,
            &scoring,
        )
        .unwrap();
        let spec = parse_search_query("chrome").effective_for_scoring(&scoring);
        let recent_map = HashMap::new();
        let eager =
            score_breakdown_with_config(&item, &spec, &recent_map, &scoring, false).unwrap();
        let prepared = PreparedQuery::new(&spec, &scoring);
        let text_score = prepared
            .score_candidate_name(
                &PreparedCandidateName {
                    title: item.title.clone(),
                    folded_title: item.folded_title.clone(),
                    folded_stem: item.folded_stem.clone(),
                },
                item.folded_parent(),
                false,
                &scoring,
            )
            .unwrap();
        let components = score_components_with_prepared_query(
            &item,
            &spec,
            &prepared,
            &recent_item_key(&item.path),
            text_score,
            &recent_map,
            &scoring,
        );
        let lazy = materialize_ranked_candidate(
            &ranked_heuristic_candidate(item, components, false),
            &spec,
            &recent_map,
            &scoring,
            true,
            true,
        );
        assert_eq!(
            visible_results_signature(&[lazy.clone()], 1),
            visible_results_signature(&[eager.result.clone()], 1)
        );
        assert_eq!(lazy.explanation, eager.result.explanation);
    }

    #[test]
    fn equal_upper_bound_is_not_pruned() {
        for score in [i32::MIN, -1, 0, 1, i32::MAX] {
            assert!(!should_branch_prune(score, heuristic_primary_rank(score)));
        }
    }

    #[cfg(feature = "debug-tools")]
    #[test]
    fn branch_pruning_matches_disabled_for_broad_tie_negative_recent_folder_and_pattern() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("branch-equivalence");
        fs::write(path.join("abc.exe"), []).unwrap();
        fs::write(path.join("abc-tie-a.exe"), []).unwrap();
        fs::write(path.join("abc-tie-b.exe"), []).unwrap();
        fs::write(path.join("a---b---c.txt"), []).unwrap();
        fs::write(path.join("alpha-beta-charlie.tmp"), []).unwrap();
        fs::create_dir_all(path.join("abc-folder")).unwrap();
        let recent_path = path.join("a---b---c.txt");

        let mut variants = Vec::new();
        variants.push((0, ScoringConfig::default(), Vec::new()));
        variants.push((-500, ScoringConfig::default(), Vec::new()));
        variants.push((
            0,
            ScoringConfig {
                folder_score_as_file_score_percent: 200,
                ..Default::default()
            },
            Vec::new(),
        ));
        variants.push((
            0,
            ScoringConfig {
                pattern_rules: vec![PatternRule {
                    pattern: "*.tmp".to_string(),
                    folded_pattern: "*.tmp".to_string(),
                    score: 700,
                    modifiers: Vec::new(),
                }],
                ..Default::default()
            },
            Vec::new(),
        ));
        variants.push((
            0,
            ScoringConfig::default(),
            vec![recent_entry_line(50.0, &recent_path.to_string_lossy())],
        ));

        for (root_score, scoring, recent_items) in variants {
            let mut root = plan_test_root(path.clone(), "branch", &[]);
            root.score = root_score;
            let root_plan = Arc::new(RootOwnershipPlan::build(&[root]));
            BRANCH_PRUNING_DISABLED.store(true, Ordering::Relaxed);
            let disabled = benchmark_live_scan_once(
                "abc",
                Arc::clone(&root_plan),
                Arc::new(scoring.clone()),
                Arc::new(recent_items.clone()),
                Arc::new(Vec::new()),
                3,
                4,
            );
            BRANCH_PRUNING_DISABLED.store(false, Ordering::Relaxed);
            let enabled = benchmark_live_scan_once(
                "abc",
                root_plan,
                Arc::new(scoring),
                Arc::new(recent_items),
                Arc::new(Vec::new()),
                3,
                4,
            );
            assert_eq!(disabled.scanned_total, enabled.scanned_total);
            assert_eq!(
                visible_results_signature(&disabled.results, disabled.results.len()),
                visible_results_signature(&enabled.results, enabled.results.len())
            );
        }
        BRANCH_PRUNING_DISABLED.store(false, Ordering::Relaxed);
        fs::remove_dir_all(path).unwrap();
    }

    #[cfg(feature = "debug-tools")]
    #[test]
    fn show_all_disables_branch_pruning() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("branch-show-all");
        for index in 0..20 {
            fs::write(path.join(format!("abc-{index:03}.txt")), []).unwrap();
        }
        let root = plan_test_root(path.clone(), "branch-show-all", &[]);
        BRANCH_PRUNE_COUNT.store(0, Ordering::Relaxed);
        let result = benchmark_live_scan_once(
            "abc",
            Arc::new(RootOwnershipPlan::build(&[root])),
            Arc::new(ScoringConfig::default()),
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            usize::MAX,
            4,
        );
        assert_eq!(result.scanned_total, 21);
        assert_eq!(BRANCH_PRUNE_COUNT.load(Ordering::Relaxed), 0);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn branch_pruned_candidate_remains_in_refinement_cache() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("branch-retained");
        let candidate_path = path.join("a---b---c.txt");
        fs::write(&candidate_path, []).unwrap();
        let root_plan = Arc::new(RootOwnershipPlan::build(&[plan_test_root(
            path.clone(),
            "branch-retained",
            &[],
        )]));
        let mut request = refinement_test_request(root_plan, "abc");
        request.effective_limit = 1;
        let effective_spec = request.spec.effective_for_scoring(&request.scoring);
        let prepared_query = PreparedQuery::new(&effective_spec, &request.scoring);
        let recent_map = HashMap::new();
        let high = SearchResult {
            title: "Priority".to_string(),
            subtitle: String::new(),
            target: LaunchTarget::Path(path.join("priority.exe")),
            is_dir: false,
            from_history: false,
            from_query_launch_rule: true,
            ranking_kind: ResultRankingKind::QueryLaunchRule,
            explanation: None,
            display_score: i32::MAX,
            score_detail: "priority".to_string(),
            score: i32::MAX,
        };
        let shared = RefinementShared {
            state: Mutex::new(RefinementSessionState {
                request,
                effective_spec,
                prepared_query,
                recent_map,
                query_version: 1,
                cache: Vec::new(),
                cache_bytes: 0,
                cache_limit: REFINEMENT_CACHE_MAX_BYTES,
                reusable: true,
                scan_seen: HashSet::new(),
                ranking_seen: HashSet::new(),
                best: vec![ranked_candidate_from_prebuilt(high)],
                scanned_total: 0,
                reused_candidates: 0,
                scan_done: false,
                shutdown: false,
                last_publish: Instant::now(),
            }),
            changed: Condvar::new(),
        };
        let root = RootScanContext::new(plan_test_root(path.clone(), "branch-retained", &[]));
        let directory = DirectoryScanContext::new(path.clone(), 0, &root);
        BRANCH_PRUNE_COUNT.store(0, Ordering::Relaxed);
        process_refinement_path(
            candidate_path.clone(),
            candidate_path.file_name(),
            false,
            &root,
            &directory,
            &shared,
        );
        let state = shared.state.lock().unwrap();
        assert_eq!(state.cache.len(), 1);
        assert_eq!(state.cache[0].item.path, candidate_path);
        assert_eq!(state.best.len(), 1);
        assert!(BRANCH_PRUNE_COUNT.load(Ordering::Relaxed) > 0);
        drop(state);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn one_live_root_uses_parallel_queue_with_multiple_workers() {
        assert_eq!(live_scan_strategy(1, 1), "sequential");
        assert_eq!(live_scan_strategy(1, 2), "parallel-task-queue");
        assert_eq!(live_scan_strategy(0, 20), "none");
    }

    #[test]
    fn parallel_live_scan_counts_every_item() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("parallel-source-counts");
        let child = path.join("child");
        let nested = child.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(path.join("root.txt"), b"root").unwrap();
        fs::write(child.join("child.txt"), b"child").unwrap();
        fs::write(nested.join("nested.txt"), b"nested").unwrap();
        let root = plan_test_root(path.clone(), "parallel", &[]);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let generation = TEST_UNCANCELLED_GENERATION;
        let mut seen = HashSet::new();
        let mut best = Vec::new();
        let mut scanned_total = 0usize;
        let mut last_publish = Instant::now();

        scan_search_roots(
            vec![(
                path.clone(),
                0,
                root,
                Arc::new(Vec::new()),
                SearchStage::Folders,
            )],
            &pool,
            &ScoringConfig::default(),
            &HashMap::new(),
            &parse_search_query("txt"),
            &mut seen,
            &mut best,
            &mut scanned_total,
            generation,
            0,
            DEFAULT_RESULT_LIMIT,
            DEFAULT_RESULT_LIMIT,
            &mut last_publish,
            true,
            true,
            None,
        );

        assert_eq!(scanned_total, 5);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn parallel_live_scan_matches_single_worker_bounded_top_results() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("parallel-bounded-top");
        fs::create_dir_all(&path).unwrap();
        for index in 0..40 {
            fs::write(path.join(format!("needle-{index:02}.txt")), b"candidate").unwrap();
        }

        let run_scan = |threads| {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let mut seen = HashSet::new();
            let mut best = Vec::new();
            let mut scanned_total = 0usize;
            let mut last_publish = Instant::now();
            scan_search_roots(
                vec![(
                    path.clone(),
                    0,
                    plan_test_root(path.clone(), "parallel-bounded", &[]),
                    Arc::new(Vec::new()),
                    SearchStage::Folders,
                )],
                &pool,
                &ScoringConfig::default(),
                &HashMap::new(),
                &parse_search_query("needle"),
                &mut seen,
                &mut best,
                &mut scanned_total,
                TEST_UNCANCELLED_GENERATION,
                0,
                5,
                5,
                &mut last_publish,
                true,
                true,
                None,
            );
            (best, scanned_total)
        };

        let (single_ranked, single_scanned) = run_scan(1);
        let (parallel_ranked, parallel_scanned) = run_scan(4);
        let spec = parse_search_query("needle");
        let scoring = ScoringConfig::default();
        let recent_map = HashMap::new();
        let single_results =
            materialize_ranked_candidates(&single_ranked, &spec, &recent_map, &scoring, true, true);
        let parallel_results = materialize_ranked_candidates(
            &parallel_ranked,
            &spec,
            &recent_map,
            &scoring,
            true,
            true,
        );

        assert_eq!(single_scanned, parallel_scanned);
        assert_eq!(single_results.len(), 5);
        let signatures = |results: &[SearchResult]| {
            results
                .iter()
                .map(|result| {
                    (
                        result.title.clone(),
                        result.subtitle.clone(),
                        result_target_text(result),
                        result.is_dir,
                        result.from_history,
                        result.from_query_launch_rule,
                        result.ranking_kind,
                        result.display_score,
                        result.score_detail.clone(),
                        result.score,
                        result.explanation.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(signatures(&parallel_results), signatures(&single_results));
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn directory_scan_context_preserves_parent_text_and_relative_depth() {
        let path = temporary_search_root("directory-context-depth");
        let nested = path.join("one").join("two");
        fs::create_dir_all(&nested).unwrap();
        let root = RootScanContext::new(plan_test_root(path.clone(), "context-root", &[]));
        let directory = DirectoryScanContext::new(nested.clone(), 2, &root);
        assert_eq!(root.root_key, normalized_root_path_key(&path));
        assert_eq!(directory.depth, 2);
        assert_eq!(
            directory.folded_parent,
            searchable_text(&nested.to_string_lossy())
        );
        assert_eq!(
            directory.subtitle,
            format!("{}\\one\\two\\", root.root_name)
        );

        let candidate_path = nested.join("needle.txt");
        let candidate_name = PreparedCandidateName::from_path(&candidate_path).unwrap();
        let item = launch_item_from_prepared_candidate(
            candidate_path,
            false,
            &root.root,
            &ScoringConfig::default(),
            candidate_name,
            directory.subtitle.clone(),
            directory.folded_parent.clone(),
            directory.depth,
        );
        assert_eq!(item.relative_depth, 2);
        assert_eq!(item.subtitle, directory.subtitle);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn prepared_candidate_creates_one_path_key_and_reuses_it_for_parallel_dedupe() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("single-path-key");
        let candidate_path = path.join("needle.exe");
        fs::write(&candidate_path, []).unwrap();
        let root = RootScanContext::new(plan_test_root(path.clone(), "single-key", &[]));
        let directory = DirectoryScanContext::new(path.clone(), 0, &root);
        let scoring = ScoringConfig::default();
        let spec = parse_search_query("needle").effective_for_scoring(&scoring);
        let prepared_query = PreparedQuery::new(&spec, &scoring);
        let mut seen = HashSet::new();
        let mut scanned_total = 0usize;
        reset_recent_item_key_call_count();
        let candidate = consider_prepared_search_path(
            candidate_path.clone(),
            candidate_path.file_name(),
            false,
            &root,
            &directory,
            &scoring,
            &HashMap::new(),
            &spec,
            &prepared_query,
            &mut seen,
            &mut scanned_total,
            0,
            false,
            false,
        )
        .unwrap();
        assert_eq!(recent_item_key_call_count(), 1);
        assert_eq!(
            candidate.path_key,
            fold_text(&candidate_path.to_string_lossy())
        );

        let progress = SharedParallelSearchProgress {
            state: Mutex::new(ParallelSearchProgress {
                seen: HashSet::new(),
                best: Vec::new(),
                scanned_total: 0,
                last_publish: Instant::now(),
            }),
            threshold: AtomicU64::new(0),
            result_store_sender: None,
        };
        offer_parallel_search_result(&progress, candidate, DEFAULT_RESULT_LIMIT);
        assert_eq!(recent_item_key_call_count(), 1);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn live_scan_skips_excluded_subtree_and_keeps_neighboring_files() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("live-exclusion");
        let excluded_path = path.join("excluded");
        fs::create_dir_all(&excluded_path).unwrap();
        fs::write(excluded_path.join("ignored.txt"), b"ignored").unwrap();
        let source_path = path.join("main.rs");
        fs::write(&source_path, b"source").unwrap();
        let root = plan_test_root(path.clone(), "cache-exclusion", &[]);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let mut seen = HashSet::new();
        let mut best = Vec::new();
        let mut scanned_total = 0usize;
        let mut last_publish = Instant::now();

        scan_search_roots(
            vec![(
                path.clone(),
                0,
                root,
                Arc::new(vec![normalized_root_path_key(&excluded_path)]),
                SearchStage::Folders,
            )],
            &pool,
            &ScoringConfig::default(),
            &HashMap::new(),
            &parse_search_query("main"),
            &mut seen,
            &mut best,
            &mut scanned_total,
            TEST_UNCANCELLED_GENERATION,
            0,
            DEFAULT_RESULT_LIMIT,
            DEFAULT_RESULT_LIMIT,
            &mut last_publish,
            true,
            true,
            None,
        );

        assert_eq!(scanned_total, 1);
        assert_eq!(best.len(), 1);
        let materialized = materialize_ranked_candidates(
            &best,
            &parse_search_query("main"),
            &HashMap::new(),
            &ScoringConfig::default(),
            true,
            true,
        );
        assert_eq!(
            result_target_path(&materialized[0]).as_deref(),
            Some(source_path.as_path())
        );
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn live_child_source_uses_parent_score_and_depth_for_parent_modifier() {
        let _generation_guard = lock_search_generation();
        let parent_path = temporary_search_root("live-parent-fallback");
        let child_path = parent_path.join("child");
        let nested_path = child_path.join("nested");
        fs::create_dir_all(&nested_path).unwrap();
        let direct_file = child_path.join("needle-direct.exe");
        let deep_file = nested_path.join("needle-deep.exe");
        fs::write(&direct_file, []).unwrap();
        fs::write(&deep_file, []).unwrap();

        let mut parent = plan_test_root(parent_path.clone(), "parent", &["parent"]);
        parent.score = 25;
        parent.max_depth = 1;
        let mut child = plan_test_root(child_path, "child", &["child"]);
        child.score = 90;
        let plan = RootOwnershipPlan::build(&[parent, child]);
        let scoring = ScoringConfig::default();
        let spec = parse_search_query("needle +parent").effective_for_scoring(&scoring);
        let any_modifier_match = any_modifier_keywords_match(
            plan.entries()
                .iter()
                .map(|entry| entry.root.keywords.as_slice()),
            &spec.modifiers,
        );
        let live_roots = eligible_search_roots(&plan, &spec, any_modifier_match)
            .into_iter()
            .map(|root| {
                (
                    root.path,
                    root.effective_depth,
                    root.effective_root,
                    root.exclusions,
                    SearchStage::Folders,
                )
            })
            .collect();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap();
        let mut seen = HashSet::new();
        let mut best = Vec::new();
        let mut scanned_total = 0usize;
        let mut last_publish = Instant::now();

        scan_search_roots(
            live_roots,
            &pool,
            &scoring,
            &HashMap::new(),
            &spec,
            &mut seen,
            &mut best,
            &mut scanned_total,
            TEST_UNCANCELLED_GENERATION,
            0,
            DEFAULT_RESULT_LIMIT,
            DEFAULT_RESULT_LIMIT,
            &mut last_publish,
            true,
            true,
            None,
        );

        assert_eq!(best.len(), 1);
        let materialized =
            materialize_ranked_candidates(&best, &spec, &HashMap::new(), &scoring, true, true);
        assert_eq!(
            result_target_path(&materialized[0]).as_deref(),
            Some(direct_file.as_path())
        );
        assert!(materialized[0].score_detail.contains("index 25"));
        assert!(!materialized
            .iter()
            .any(|result| { result_target_path(result).as_deref() == Some(deep_file.as_path()) }));
        fs::remove_dir_all(parent_path).unwrap();
    }

    #[cfg(feature = "debug-tools")]
    #[test]
    fn debug_child_source_uses_parent_score_and_depth_for_parent_modifier() {
        let parent_path = temporary_search_root("debug-parent-fallback");
        let child_path = parent_path.join("child");
        let nested_path = child_path.join("nested");
        fs::create_dir_all(&nested_path).unwrap();
        let direct_file = child_path.join("needle-direct.exe");
        let deep_file = nested_path.join("needle-deep.exe");
        fs::write(&direct_file, []).unwrap();
        fs::write(&deep_file, []).unwrap();

        let mut parent = plan_test_root(parent_path.clone(), "parent", &["parent"]);
        parent.score = 25;
        parent.max_depth = 1;
        let mut child = plan_test_root(child_path, "child", &["child"]);
        child.score = 90;
        let plan = RootOwnershipPlan::build(&[parent, child]);
        let scoring = ScoringConfig::default();
        let spec = parse_search_query("needle +parent").effective_for_scoring(&scoring);
        let any_modifier_match = any_modifier_keywords_match(
            plan.entries()
                .iter()
                .map(|entry| entry.root.keywords.as_slice()),
            &spec.modifiers,
        );
        let mut seen = HashSet::new();
        let mut breakdowns = Vec::new();

        for root in eligible_search_roots(&plan, &spec, any_modifier_match) {
            collect_debug_root_breakdowns(
                &root.path,
                root.effective_depth,
                root.effective_root.max_depth,
                &root.effective_root,
                &scoring,
                &HashMap::new(),
                &spec,
                &mut seen,
                &mut breakdowns,
                &root.exclusions,
            );
        }

        assert_eq!(breakdowns.len(), 1);
        assert_eq!(
            result_target_path(&breakdowns[0].result).as_deref(),
            Some(direct_file.as_path())
        );
        assert_eq!(breakdowns[0].index_score, 25);
        assert!(!breakdowns.iter().any(|breakdown| {
            result_target_path(&breakdown.result).as_deref() == Some(deep_file.as_path())
        }));
        fs::remove_dir_all(parent_path).unwrap();
    }

    #[test]
    fn result_boundary_shortcuts_require_control() {
        assert_eq!(result_boundary_shortcut(VK_HOME, false), None);
        assert_eq!(result_boundary_shortcut(VK_END, false), None);
        assert_eq!(
            result_boundary_shortcut(VK_HOME, true),
            Some(ResultBoundaryShortcut::First)
        );
        assert_eq!(
            result_boundary_shortcut(VK_END, true),
            Some(ResultBoundaryShortcut::Last)
        );
        assert_eq!(result_boundary_shortcut(VK_PRIOR, true), None);
    }

    #[test]
    fn selected_result_keeps_the_viewport_in_sync() {
        assert_eq!(top_index_after_select(0, 0, 20, 5), 0);
        assert_eq!(top_index_after_select(0, 4, 20, 5), 0);
        assert_eq!(top_index_after_select(0, 5, 20, 5), 1);
        assert_eq!(top_index_after_select(6, 4, 20, 5), 4);
        assert_eq!(top_index_after_select(15, 19, 20, 5), 15);
        assert_eq!(top_index_after_select(15, 0, 20, 5), 0);
    }

    #[test]
    fn page_navigation_moves_selection_and_viewport_by_visible_rows() {
        assert_eq!(page_selection_after_move(Some(7), 5, 30, 5, 1), Some((12, 10)));
        assert_eq!(page_selection_after_move(Some(7), 5, 30, 5, -1), Some((2, 0)));
    }

    #[test]
    fn page_navigation_stops_at_result_boundaries() {
        assert_eq!(page_selection_after_move(Some(2), 0, 30, 5, -1), Some((0, 0)));
        assert_eq!(page_selection_after_move(Some(27), 25, 30, 5, 1), Some((29, 25)));
        assert_eq!(page_selection_after_move(Some(0), 0, 30, 5, -1), Some((0, 0)));
        assert_eq!(page_selection_after_move(Some(29), 25, 30, 5, 1), Some((29, 25)));
    }

    #[test]
    fn page_navigation_handles_short_lists_and_no_selection() {
        assert_eq!(page_selection_after_move(Some(1), 0, 3, 5, 1), Some((2, 0)));
        assert_eq!(page_selection_after_move(Some(1), 0, 3, 5, -1), Some((0, 0)));
        assert_eq!(page_selection_after_move(None, 7, 30, 5, 1), Some((0, 0)));
        assert_eq!(page_selection_after_move(None, 7, 30, 5, -1), Some((29, 25)));
        assert_eq!(page_selection_after_move(None, 0, 0, 5, 1), None);
    }

    #[test]
    fn search_help_describes_page_and_boundary_navigation() {
        let help = search_bar_help_text(&plugins::PluginState::default(), AppLanguage::Source);
        assert!(help.contains("Move the result selection by one visible page."));
        assert!(help.contains("Ctrl+Home / Ctrl+End"));
        assert!(help.contains("Select the first / last result."));
        assert!(help.contains("Shift+Enter"));
        assert!(help.contains("Open the selected item's real target folder, not the shortcut folder."));
    }

    fn result(title: &str) -> SearchResult {
        SearchResult {
            title: title.to_string(),
            subtitle: String::new(),
            target: LaunchTarget::Path(PathBuf::from(format!(r"C:\Tools\{title}.exe"))),
            is_dir: false,
            from_history: false,
            from_query_launch_rule: false,
            ranking_kind: ResultRankingKind::Heuristic,
            explanation: None,
            display_score: 0,
            score_detail: String::new(),
            score: 0,
        }
    }

    #[test]
    fn bounded_results_insert_in_rank_order_and_keep_limit() {
        let mut best = Vec::new();
        for (title, score) in [
            ("Twenty", 20),
            ("One Hundred", 100),
            ("Sixty", 60),
            ("Forty", 40),
            ("Eighty", 80),
        ] {
            let mut candidate = result(title);
            candidate.score = score;
            insert_bounded_search_result(&mut best, candidate, 3);
        }

        assert_eq!(
            best.iter().map(|item| item.score).collect::<Vec<_>>(),
            vec![100, 80, 60]
        );
    }

    #[test]
    fn bounded_results_use_title_and_subtitle_for_equal_scores() {
        let mut best = Vec::new();
        for (title, subtitle) in [("Beta", "A"), ("Alpha", "Z"), ("Alpha", "A")] {
            let mut candidate = result(title);
            candidate.subtitle = subtitle.to_string();
            candidate.score = 50;
            insert_bounded_search_result(&mut best, candidate, 2);
        }

        assert_eq!(
            best.iter()
                .map(|item| (item.title.as_str(), item.subtitle.as_str()))
                .collect::<Vec<_>>(),
            vec![("Alpha", "A"), ("Alpha", "Z")]
        );
    }

    #[test]
    fn bounded_results_keep_query_rule_ahead_of_higher_scores() {
        let mut best = Vec::new();
        let mut regular = result("Regular");
        regular.score = 999;
        insert_bounded_search_result(&mut best, regular, 1);

        let mut priority = result("Priority");
        priority.score = 1;
        priority.from_query_launch_rule = true;
        priority.ranking_kind = ResultRankingKind::QueryLaunchRule;
        insert_bounded_search_result(&mut best, priority, 1);

        assert_eq!(best[0].title, "Priority");
    }

    #[test]
    fn bounded_results_match_full_sort_for_multiple_limits_and_orders() {
        let candidates = (0..40)
            .map(|index| {
                let mut candidate = result(&format!("Result {index:02}"));
                candidate.score = (index * 37 % 101) - 50;
                candidate
            })
            .collect::<Vec<_>>();

        for limit in [1, 3, 9, 25] {
            let mut expected = candidates.clone();
            sort_search_results(&mut expected);
            expected.truncate(limit);
            for ordered in [
                candidates.clone(),
                candidates.iter().rev().cloned().collect::<Vec<_>>(),
            ] {
                let mut actual = Vec::new();
                for candidate in ordered {
                    insert_bounded_search_result(&mut actual, candidate, limit);
                }
                assert_eq!(
                    actual.iter().map(|item| &item.title).collect::<Vec<_>>(),
                    expected.iter().map(|item| &item.title).collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn bounded_threshold_rejects_only_strictly_lower_primary_rank() {
        let mut best = Vec::new();
        for score in [100, 80, 60] {
            let mut candidate = result(&format!("Result {score}"));
            candidate.score = score;
            insert_bounded_search_result(&mut best, candidate, 3);
        }
        let threshold = bounded_search_threshold(&best, 3);

        let mut lower = result("Lower");
        lower.score = 59;
        let mut equal = result("A Equal");
        equal.score = 60;

        assert!(primary_search_rank(&lower) < threshold);
        assert_eq!(primary_search_rank(&equal), threshold);
    }

    #[test]
    fn show_all_streams_to_store_and_keeps_preview_bounded() {
        let _generation_guard = lock_search_generation();
        let path = temporary_search_root("show-all-store");
        fs::create_dir_all(&path).unwrap();
        for index in 0..(DEFAULT_RESULT_LIMIT + 40) {
            fs::write(path.join(format!("needle-{index:03}.txt")), b"candidate").unwrap();
        }
        let root = plan_test_root(path.clone(), "show-all", &[]);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        if let Ok(mut pending) = pending_search_slot().lock() {
            pending.clear();
        }
        search_streaming(
            TEST_UNCANCELLED_GENERATION,
            0,
            parse_search_query("needle"),
            Arc::new(RootOwnershipPlan::build(&[root])),
            Arc::new(ScoringConfig::default()),
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
            usize::MAX,
            &pool,
            true,
            false,
        );
        let batch = pending_search_slot()
            .lock()
            .unwrap()
            .pop()
            .expect("final show-all batch");
        assert!(batch.done);
        assert!(batch.results.len() <= DEFAULT_RESULT_LIMIT);
        let Some(ResultStoreCompletion::Ready(manifest)) = batch.result_store else {
            panic!("show-all result store was not produced");
        };
        let reader = ResultStoreReader::open(manifest).unwrap();
        assert_eq!(reader.len(), DEFAULT_RESULT_LIMIT + 40);
        assert!(reader.get(0).unwrap().is_some());
        assert!(reader.get(reader.len() / 2).unwrap().is_some());
        assert!(reader.get(reader.len() - 1).unwrap().is_some());
        drop(reader);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn show_all_progress_publishes_preview_without_truncating_final_candidates() {
        let mut best = (0..DEFAULT_RESULT_LIMIT + 7)
            .map(|index| result(&format!("Result {index:03}")))
            .collect::<Vec<_>>();

        let preview = progressive_search_results(&mut best, usize::MAX);

        assert_eq!(preview.len(), DEFAULT_RESULT_LIMIT);
        assert_eq!(best.len(), DEFAULT_RESULT_LIMIT + 7);
    }

    fn batch(generation: u64, title: &str, done: bool) -> SearchBatch {
        SearchBatch {
            generation,
            results: vec![result(title)],
            scanned_total: 1,
            stage: if done {
                SearchStage::Done
            } else {
                SearchStage::Folders
            },
            done,
            effective_limit: DEFAULT_RESULT_LIMIT,
            result_store: None,
        }
    }

    #[test]
    fn auto_learned_query_rule_syncs_settings_model() {
        let old_rule = QueryLaunchRule {
            query: "old".to_string(),
            target: r"C:\Tools\Old.exe".to_string(),
        };
        let mut runtime_rules = vec![old_rule.clone()];
        let mut config_rules = vec![old_rule];
        let target = PathBuf::from(r"C:\Tools\New.exe");

        assert!(record_query_launch_rule_in_memory(
            &mut runtime_rules,
            "new query",
            &target,
            &plugins::PluginState::default(),
        ));
        sync_query_launch_rules_model_from_runtime(&mut config_rules, &runtime_rules);

        assert_eq!(config_rules.len(), 2);
        assert_eq!(config_rules[0].query, "new query");
        assert_eq!(config_rules[0].target, r"C:\Tools\New.exe");
        assert!(config_rules == runtime_rules);
    }

    #[test]
    fn pending_search_batch_keeps_latest_for_generation() {
        let mut pending = vec![batch(7, "old", false)];

        assert!(!coalesce_pending_search_batch(
            &mut pending,
            batch(7, "latest", true),
        ));

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].generation, 7);
        assert_eq!(pending[0].results[0].title, "latest");
        assert!(pending[0].done);
        assert_eq!(pending[0].stage, SearchStage::Done);
    }

    #[test]
    fn query_rule_results_sort_before_higher_scores() {
        let mut regular = result("Regular");
        regular.score = 999;
        regular.display_score = 999;
        let mut starred = result("Starred");
        starred.score = 1;
        starred.display_score = 1;
        starred.from_query_launch_rule = true;
        starred.ranking_kind = ResultRankingKind::QueryLaunchRule;
        starred.score_detail = "starred".to_string();
        let mut results = vec![regular, starred];

        sort_search_results(&mut results);

        assert_eq!(results[0].title, "Starred");
        assert_eq!(results[1].title, "Regular");
    }

    #[test]
    fn pending_search_batch_drops_stale_generations() {
        let mut pending = vec![batch(4, "stale-a", false), batch(4, "stale-b", true)];

        assert!(coalesce_pending_search_batch(
            &mut pending,
            batch(5, "new", false),
        ));

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].generation, 5);
        assert_eq!(pending[0].results[0].title, "new");
        assert!(!pending[0].done);
    }

    #[test]
    fn modifier_filter_reuses_child_source_with_parent_configuration() {
        let parent_path = temporary_search_root("modifier-ownership");
        let child_path = parent_path.join("child");
        fs::create_dir_all(&child_path).unwrap();
        let roots = vec![
            plan_test_root(parent_path.clone(), "parent", &["parent"]),
            plan_test_root(child_path.clone(), "child", &["child"]),
        ];
        let plan = RootOwnershipPlan::build(&roots);
        let spec = parse_search_query("needle +parent");
        let any_modifier_match = any_modifier_keywords_match(
            plan.entries()
                .iter()
                .map(|entry| entry.root.keywords.as_slice()),
            &spec.modifiers,
        );

        let eligible = eligible_search_roots(&plan, &spec, any_modifier_match);

        assert_eq!(eligible.len(), 2);
        let parent_source = eligible
            .iter()
            .find(|entry| entry.source_root.raw == "parent")
            .unwrap();
        let child_source = eligible
            .iter()
            .find(|entry| entry.source_root.raw == "child")
            .unwrap();
        assert_eq!(parent_source.effective_root.raw, "parent");
        assert_eq!(
            parent_source.exclusions.as_slice(),
            &[normalized_root_path_key(&child_path)]
        );
        assert_eq!(child_source.effective_root.raw, "parent");
        assert_eq!(child_source.effective_depth, 1);
        assert_eq!(
            plan.owner(&child_path.join("file.exe")).unwrap().root.raw,
            "child"
        );
        fs::remove_dir_all(parent_path).unwrap();
    }

    #[test]
    fn recent_child_item_uses_parent_score_and_depth_for_parent_modifier() {
        let parent_path = temporary_search_root("recent-parent-fallback");
        let child_path = parent_path.join("child");
        let nested_path = child_path.join("nested");
        fs::create_dir_all(&nested_path).unwrap();
        let direct_file = child_path.join("needle-direct.exe");
        let deep_file = nested_path.join("needle-deep.exe");
        fs::write(&direct_file, []).unwrap();
        fs::write(&deep_file, []).unwrap();

        let mut parent = plan_test_root(parent_path.clone(), "parent", &["parent"]);
        parent.score = 25;
        parent.max_depth = 1;
        let mut child = plan_test_root(child_path, "child", &["child"]);
        child.score = 90;
        let plan = RootOwnershipPlan::build(&[parent, child]);
        let recent_items = [direct_file.clone(), deep_file.clone()]
            .iter()
            .map(|path| {
                recent_config_entry_line(&RecentConfigEntry {
                    enabled: true,
                    score: 110.0,
                    path: path.to_string_lossy().to_string(),
                })
            })
            .collect::<Vec<_>>();
        let recent_map = recent_lookup(&recent_items);
        let scoring = ScoringConfig::default();

        let parent_results = collect_recent_path_match_breakdowns(
            &parse_search_query("needle +parent").effective_for_scoring(&scoring),
            &plan,
            &recent_items,
            &recent_map,
            &scoring,
            10,
        );
        assert_eq!(parent_results.len(), 1);
        assert_eq!(
            result_target_path(&parent_results[0].result).as_deref(),
            Some(direct_file.as_path())
        );
        assert_eq!(parent_results[0].index_score, 25);

        let child_results = collect_recent_path_match_breakdowns(
            &parse_search_query("needle +child").effective_for_scoring(&scoring),
            &plan,
            &recent_items,
            &recent_map,
            &scoring,
            10,
        );
        assert_eq!(child_results.len(), 2);
        assert!(child_results.iter().all(|result| result.index_score == 90));
        fs::remove_dir_all(parent_path).unwrap();
    }

    #[test]
    fn recent_batch_reuses_one_prebuilt_plan_for_132_items() {
        let root_path = temporary_search_root("recent-batch");
        let root = plan_test_root(root_path.clone(), "recent-root", &[]);
        let plan = RootOwnershipPlan::build(&[root]);
        let mut recent_items = Vec::new();
        for index in 0..132 {
            let path = root_path.join(format!("recent-{index:03}.exe"));
            fs::write(&path, []).unwrap();
            recent_items.push(recent_config_entry_line(&RecentConfigEntry {
                enabled: true,
                score: 110.0,
                path: path.to_string_lossy().to_string(),
            }));
        }
        let recent_map = recent_lookup(&recent_items);
        let scoring = ScoringConfig::default();
        let spec = parse_search_query("recent").effective_for_scoring(&scoring);

        let results = collect_recent_path_match_breakdowns(
            &spec,
            &plan,
            &recent_items,
            &recent_map,
            &scoring,
            200,
        );

        assert_eq!(results.len(), 132);
        assert!(results.iter().all(|result| result.index_score == 0));
        fs::remove_dir_all(root_path).unwrap();
    }

    #[test]
    fn search_history_merge_preserves_comments_and_malformed_spacing() {
        let source = "# Keep\r\nold query\r\n\r\n; Keep too";
        let merged = merge_search_history_items_text(
            source,
            &["new query".to_string(), "next query".to_string()],
        );

        assert_eq!(
            merged,
            "# Keep\r\nnew query\r\nnext query\r\n\r\n; Keep too"
        );
        assert!(!merged.ends_with(['\r', '\n']));
    }

    #[test]
    fn query_rule_merge_preserves_unknown_lines_and_newline_style() {
        let source = "# Keep\nold query\tC:\\Old.exe\nmalformed=future\n";
        let merged = merge_query_launch_rules_text(
            source,
            &[QueryLaunchRule {
                query: "new query".to_string(),
                target: r"C:\New.exe".to_string(),
            }],
        );

        assert_eq!(merged, "# Keep\nnew query\tC:\\New.exe\nmalformed=future\n");
    }
    #[test]
    fn score_detail_is_available_without_explanation_for_tooltip() {
        let scoring = ScoringConfig::default();
        let root = plan_test_root(PathBuf::from(r"C:\ROOT"), "root", &[]);
        let item = launch_item_from_path_with_type(
            PathBuf::from(r"C:\ROOT\Tool.exe"),
            false,
            &root,
            &scoring,
        )
        .unwrap();
        let spec = parse_search_query("tool").effective_for_scoring(&scoring);
        let breakdown = score_breakdown_with_config_and_detail(
            &item,
            &spec,
            &HashMap::new(),
            &scoring,
            false,
            true,
            false,
        )
        .unwrap();

        assert!(breakdown.result.explanation.is_none());
        assert!(!breakdown.result.score_detail.is_empty());
        assert_eq!(
            result_score_breakdown_tooltip_text(&breakdown.result),
            Some(breakdown.result.score_detail.as_str())
        );
    }

    #[test]
    fn enabled_score_breakdown_builds_full_explanation() {
        let scoring = ScoringConfig::default();
        let root = plan_test_root(PathBuf::from(r"C:\ROOT"), "root", &[]);
        let item = launch_item_from_path_with_type(
            PathBuf::from(r"C:\ROOT\Tool.exe"),
            false,
            &root,
            &scoring,
        )
        .unwrap();
        let spec = parse_search_query("tool").effective_for_scoring(&scoring);
        let breakdown = score_breakdown_with_config_and_detail(
            &item,
            &spec,
            &HashMap::new(),
            &scoring,
            false,
            true,
            true,
        )
        .unwrap();

        let explanation = breakdown.result.explanation.unwrap();
        assert!(!explanation.rows.is_empty());
        assert_eq!(explanation.final_score, breakdown.final_score);
    }

    #[test]
    fn explanation_rows_sum_to_final_score_and_show_rule_states() {
        let scoring = ScoringConfig {
            folder_score_as_file_score_percent: 100,
            pattern_rules: vec![PatternRule {
                pattern: "*.exe".to_string(),
                folded_pattern: fold_text("*.exe"),
                score: 150,
                modifiers: Vec::new(),
            }],
            ..Default::default()
        };
        let root = IndexRoot {
            raw: r"C:\ROOT".to_string(),
            path: Some(PathBuf::from(r"C:\ROOT")),
            enabled: true,
            score: 25,
            max_depth: SEARCH_DEPTH_ALL,
            label: "Root".to_string(),
            keywords: Vec::new(),
        };
        let item = launch_item_from_path_with_type(
            PathBuf::from(r"C:\ROOT\games\Tool.exe"),
            false,
            &root,
            &scoring,
        )
        .unwrap();
        let spec = parse_search_query("tool").effective_for_scoring(&scoring);
        let breakdown =
            score_breakdown_with_config(&item, &spec, &HashMap::new(), &scoring, false).unwrap();
        let explanation = breakdown.result.explanation.as_ref().unwrap();
        let contribution_sum = explanation.rows.iter().map(|row| row.score).sum::<i32>();

        assert_eq!(explanation.relative_depth, 1);
        assert_eq!(contribution_sum, explanation.final_score);
        assert!(explanation.rows.iter().any(|row| {
            row.rule == "Folder Score As % of File Score"
                && row.input_condition == "Disabled"
                && row.score == 0
        }));
        assert!(explanation.rows.iter().any(|row| {
            row.rule == "Pattern *.exe" && row.input_condition == "Matched" && row.score == 150
        }));
        assert!(explanation
            .rows
            .iter()
            .any(|row| row.rule == "Whole query fuzzy positions"));
    }

    #[test]
    fn special_explanations_describe_bypassed_heuristics() {
        for kind in [
            ResultRankingKind::Plugin,
            ResultRankingKind::RecentOrder,
            ResultRankingKind::QueryLaunchRule,
        ] {
            let explanation = special_score_explanation(kind, "query", r"C:\Tool.exe", 42);
            assert_eq!(explanation.ranking_kind, kind);
            assert!(
                explanation.summary.contains("heuristic")
                    || kind == ResultRankingKind::QueryLaunchRule
            );
            assert_eq!(
                explanation.rows.iter().map(|row| row.score).sum::<i32>(),
                42
            );
        }
    }
}
