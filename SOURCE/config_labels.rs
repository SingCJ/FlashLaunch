use crate::*;

pub(crate) fn settings_window_title(language: AppLanguage) -> String {
    format!("{} {}", APP_NAME, localized(language, "Settings"))
}

pub(crate) fn settings_nav_labels(language: AppLanguage) -> [&'static str; 6] {
    [
        localized(language, "General"),
        localized(language, "Search Folders"),
        localized(language, "Heuristic Scoring"),
        localized(language, "Pattern Scoring"),
        localized(language, "Plugin Alias"),
        localized(language, "Query Launch Rules"),
    ]
}

pub(crate) fn heuristic_scoring_columns(language: AppLanguage) -> Vec<(&'static str, i32)> {
    vec![
        (localized(language, "Rule"), 220),
        (localized(language, "Score"), 70),
        (localized(language, "Note"), 420),
        (localized(language, "Default Value"), 140),
    ]
}

pub(crate) fn pattern_scoring_columns(language: AppLanguage) -> Vec<(&'static str, i32)> {
    vec![
        (localized(language, "File/Directory Pattern"), 290),
        (localized(language, "Modifier Keywords"), 150),
        (localized(language, "Score"), 70),
        (localized(language, "Default Value"), 140),
    ]
}

pub(crate) fn search_folder_columns(language: AppLanguage) -> Vec<(&'static str, i32)> {
    vec![
        (localized(language, "Folder"), 330),
        (localized(language, "Modifier Keywords"), 170),
        (localized(language, "Score"), 70),
        (localized(language, "Depth"), 60),
        (localized(language, "Default Value"), 180),
    ]
}
pub(crate) fn query_launch_rule_columns(language: AppLanguage) -> Vec<(&'static str, i32)> {
    vec![
        (localized(language, "Search Query"), 240),
        (localized(language, "Last Launched Result"), 420),
    ]
}

pub(crate) fn plugin_alias_columns(language: AppLanguage) -> Vec<(&'static str, i32)> {
    vec![
        (localized(language, "Plugin"), 300),
        (localized(language, "Plugin Alias"), 180),
        (localized(language, "Default Value"), 160),
    ]
}

pub(crate) fn heuristic_scoring_label(key: &str, language: AppLanguage) -> String {
    match key.trim() {
        "Exact Match Bonus" => localized(language, "Exact Match Bonus").to_string(),
        "Exact Word Bonus" => localized(language, "Exact Word Bonus").to_string(),
        "Prefix Match Bonus" => localized(language, "Prefix Match Bonus").to_string(),
        "Word Boundary Bonus" => localized(language, "Word Boundary Bonus").to_string(),
        "Consecutive Match Bonus" => localized(language, "Consecutive Match Bonus").to_string(),
        "Acronym Match Bonus" => localized(language, "Acronym Match Bonus").to_string(),
        "Leftmost Match Bonus" => localized(language, "Leftmost Match Bonus").to_string(),
        "Leftmost Distance Penalty" => localized(language, "Leftmost Distance Penalty").to_string(),
        "Length Score Weight" => localized(language, "Length Score Weight").to_string(),
        "Compact Match Bonus" => localized(language, "Compact Match Bonus").to_string(),
        "Recent First Launch Score" => localized(language, "Recent First Launch Score").to_string(),
        "Recent Launch Increment" => localized(language, "Recent Launch Increment").to_string(),
        "Recent Score Ceiling" => localized(language, "Recent Score Ceiling").to_string(),
        "Folder Score As % of File Score" => {
            localized(language, "Folder Score As % of File Score").to_string()
        }
        "Explicit Folder Name Match Adjustment" => {
            localized(language, "Explicit Folder Name Match Adjustment").to_string()
        }
        "Path Depth Penalty" => localized(language, "Path Depth Penalty").to_string(),
        "Recency Date Bonus" => localized(language, "Recency Date Bonus").to_string(),
        _ => key.to_string(),
    }
}

pub(crate) fn heuristic_scoring_note(key: &str, language: AppLanguage) -> &'static str {
    match key.trim() {
        "Exact Match Bonus" => {
            localized(language, "Boost when the name exactly matches the query.")
        }
        "Exact Word Bonus" => localized(language, "Boost when the query matches a complete word."),
        "Prefix Match Bonus" => localized(language, "Boost when the name starts with the query."),
        "Word Boundary Bonus" => {
            localized(language, "Boost when the query starts at a word boundary.")
        }
        "Consecutive Match Bonus" => localized(
            language,
            "Boost when the query appears as one contiguous substring.",
        ),
        "Acronym Match Bonus" => localized(
            language,
            "Boost when the query matches initials from separated words.",
        ),
        "Leftmost Match Bonus" => {
            localized(language, "Maximum boost for a match at the left edge.")
        }
        "Leftmost Distance Penalty" => localized(
            language,
            "Points removed from the leftmost boost per preceding character.",
        ),
        "Length Score Weight" => localized(
            language,
            "Weight for query length relative to the full filename length.",
        ),
        "Compact Match Bonus" => localized(
            language,
            "Maximum boost for adjacent fuzzy-match character pairs.",
        ),
        "Recent First Launch Score" => {
            localized(language, "Starting score for the first successful launch.")
        }
        "Recent Launch Increment" => localized(
            language,
            "Score added each time the same item is launched again.",
        ),
        "Recent Score Ceiling" => localized(language, "Maximum recent score used for ranking."),
        "Folder Score As % of File Score" => localized(
            language,
            "Sets the folder score as a percentage of the equivalent file score.",
        ),
        "Explicit Folder Name Match Adjustment" => localized(
            language,
            "Boost when the query explicitly names a parent folder.",
        ),
        "Path Depth Penalty" => localized(
            language,
            "Penalty per folder level relative to the owning Search Folder.",
        ),
        "Recency Date Bonus" => localized(language, "Optional modified-date boost."),
        _ => "",
    }
}
