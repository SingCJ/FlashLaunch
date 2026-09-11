// Unified trait and adapters for Settings ListView pages.
//
// Each Settings page wraps its backing data in an adapter that implements
// `SettingsListModel`, giving every page the same populate / inline-edit /
// toggle / delete / move interface.  The original domain structs (IndexRoot,
// ScoringRuleEntry, etc.) are intentionally kept unchanged.

use windows_sys::Win32::Foundation::HWND;

use crate::app_impl::parse_default_value_into;
use crate::plugins;
use crate::*;

// ---------------------------------------------------------------------------
// ColumnDef - unified column metadata
// ---------------------------------------------------------------------------

/// Describes one column in a Settings ListView.
#[derive(Clone, Copy)]
pub(crate) struct ColumnDef {
    pub header: &'static str,
    pub width: i32,
    pub editable: bool,
    pub role: ColumnRole,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColumnRole {
    Normal,
    DefaultValue,
}

impl ColumnDef {
    fn normal(header: &'static str, width: i32, editable: bool) -> Self {
        Self {
            header,
            width,
            editable,
            role: ColumnRole::Normal,
        }
    }

    fn default_value(header: &'static str, width: i32) -> Self {
        Self {
            header,
            width,
            editable: false,
            role: ColumnRole::DefaultValue,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellStyle {
    Normal,
    ReadonlyDefault,
    ModifiedEditable,
}

const CELL_STYLE_BITS_PER_COLUMN: usize = 2;
const CELL_STYLE_MASK: usize = (1 << CELL_STYLE_BITS_PER_COLUMN) - 1;

impl CellStyle {
    fn encode(self) -> usize {
        match self {
            CellStyle::Normal => 0,
            CellStyle::ReadonlyDefault => 1,
            CellStyle::ModifiedEditable => 2,
        }
    }
}

pub(crate) fn encode_cell_styles(styles: impl IntoIterator<Item = CellStyle>) -> isize {
    let mut packed = 0usize;
    for (column, style) in styles.into_iter().enumerate() {
        let shift = column * CELL_STYLE_BITS_PER_COLUMN;
        if shift >= usize::BITS as usize {
            break;
        }
        packed |= style.encode() << shift;
    }
    packed as isize
}

pub(crate) fn decode_cell_style(packed: isize, column: usize) -> CellStyle {
    let shift = column * CELL_STYLE_BITS_PER_COLUMN;
    if shift >= usize::BITS as usize {
        return CellStyle::Normal;
    }
    match ((packed as usize) >> shift) & CELL_STYLE_MASK {
        1 => CellStyle::ReadonlyDefault,
        2 => CellStyle::ModifiedEditable,
        _ => CellStyle::Normal,
    }
}

fn default_cell_style(columns: &[ColumnDef], col: usize) -> CellStyle {
    match columns.get(col) {
        Some(column) if column.role == ColumnRole::DefaultValue || !column.editable => {
            CellStyle::ReadonlyDefault
        }
        _ => CellStyle::Normal,
    }
}

fn text_changed(current: &str, default: &str) -> bool {
    current.trim() != default.trim()
}

fn folded_text_changed(current: &str, default: &str) -> bool {
    !current.trim().eq_ignore_ascii_case(default.trim())
}

fn default_scoring_rule_for_key(rule: &ScoringRuleEntry) -> Option<ScoringRuleEntry> {
    parse_scoring_rule_entries(&default_scoring_text())
        .into_iter()
        .find(|candidate| {
            candidate.kind == rule.kind && candidate.key.eq_ignore_ascii_case(&rule.key)
        })
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Uniform interface for one Settings ListView page.
pub(crate) trait SettingsListModel {
    /// Column definitions (header, width, editable).
    fn columns(&self, language: AppLanguage) -> Vec<ColumnDef>;

    /// Number of visible rows.
    fn row_count(&self) -> usize;

    /// Number of columns.
    fn column_count(&self, language: AppLanguage) -> usize {
        self.columns(language).len()
    }

    fn supports_multi_select(&self) -> bool {
        true
    }

    fn supports_toggle(&self) -> bool {
        false
    }

    fn supports_add(&self) -> bool {
        false
    }

    fn supports_delete(&self) -> bool {
        false
    }

    fn supports_reset_default(&self) -> bool {
        false
    }

    fn supports_move(&self) -> bool {
        false
    }

    fn supports_modifier_help(&self) -> bool {
        false
    }

    /// Whether the row at `index` is checked/enabled.
    fn is_row_enabled(&self, index: usize) -> bool;

    /// Display text for a cell.
    fn cell_text(&self, row: usize, col: usize) -> String;

    /// Apply inline-edited text back to the model.
    fn set_cell_text(&mut self, row: usize, col: usize, text: &str);

    /// Visual style for a cell.
    fn cell_style(&self, _row: usize, col: usize, language: AppLanguage) -> CellStyle {
        default_cell_style(&self.columns(language), col)
    }

    /// Toggle enabled/disabled.  Returns `true` on success.
    fn toggle_row(&mut self, _index: usize) -> bool {
        false
    }

    /// Delete a row.  Returns `true` on success.
    fn delete_row(&mut self, _index: usize) -> bool {
        false
    }

    fn reset_row_to_default(&mut self, _index: usize) -> bool {
        false
    }

    /// Swap two rows (for move up/down).  Returns `true` on success.
    fn swap_rows(&mut self, _a: usize, _b: usize) -> bool {
        false
    }

    /// All cell texts for a single row, ready for `add_list_view_row`.
    fn row_cells(&self, row: usize, language: AppLanguage) -> Vec<String> {
        let cols = self.column_count(language);
        (0..cols).map(|c| self.cell_text(row, c)).collect()
    }

    fn row_style_param(&self, row: usize, language: AppLanguage) -> isize {
        let cols = self.column_count(language);
        encode_cell_styles((0..cols).map(|col| self.cell_style(row, col, language)))
    }

    /// Column headers and widths, extracted from `columns()`.
    fn column_defs(&self, language: AppLanguage) -> Vec<(&'static str, i32)> {
        self.columns(language)
            .iter()
            .map(|c| (c.header, c.width))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Status message helper
// ---------------------------------------------------------------------------

/// Build a user-facing status message after a mutation action.
pub(crate) fn status_message(language: AppLanguage, action: &'static str) -> String {
    localized_format2(
        language,
        "{}. {}",
        localized(language, action),
        localized(language, "Click Save + Apply to persist."),
    )
}

/// Get column definitions for a given page without needing data access.
/// This is used for read-only queries like editability checks.
pub(crate) fn columns_for_page(page: SettingsPage, language: AppLanguage) -> Vec<ColumnDef> {
    page_columns(page, language)
}

pub(crate) fn column_defs_for_page(
    page: SettingsPage,
    language: AppLanguage,
) -> Vec<(&'static str, i32)> {
    columns_for_page(page, language)
        .into_iter()
        .map(|column| (column.header, column.width))
        .collect()
}

fn page_columns(page: SettingsPage, language: AppLanguage) -> Vec<ColumnDef> {
    match page {
        SettingsPage::SearchFolders => {
            let c = search_folder_columns(language);
            vec![
                ColumnDef::normal(c[0].0, c[0].1, true),
                ColumnDef::normal(c[1].0, c[1].1, true),
                ColumnDef::normal(c[2].0, c[2].1, true),
                ColumnDef::normal(c[3].0, c[3].1, true),
                ColumnDef::default_value(c[4].0, c[4].1),
            ]
        }
        SettingsPage::QueryLaunchRules => {
            let c = query_launch_rule_columns(language);
            vec![
                ColumnDef::normal(c[0].0, c[0].1, false),
                ColumnDef::normal(c[1].0, c[1].1, false),
            ]
        }
        SettingsPage::PluginAliases => {
            let c = plugin_alias_columns(language);
            vec![
                ColumnDef::normal(c[0].0, c[0].1, false),
                ColumnDef::normal(c[1].0, c[1].1, true),
                ColumnDef::default_value(c[2].0, c[2].1),
            ]
        }
        SettingsPage::HeuristicScoring => {
            let c = heuristic_scoring_columns(language);
            vec![
                ColumnDef::normal(c[0].0, c[0].1, false),
                ColumnDef::normal(c[1].0, c[1].1, true),
                ColumnDef::normal(c[2].0, c[2].1, false),
                ColumnDef::default_value(c[3].0, c[3].1),
            ]
        }
        SettingsPage::PatternScoring => {
            let c = pattern_scoring_columns(language);
            vec![
                ColumnDef::normal(c[0].0, c[0].1, true),
                ColumnDef::normal(c[1].0, c[1].1, true),
                ColumnDef::normal(c[2].0, c[2].1, true),
                ColumnDef::default_value(c[3].0, c[3].1),
            ]
        }
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// 1. Search Folders
// ---------------------------------------------------------------------------

pub(crate) struct SearchFoldersModel<'a> {
    pub roots: &'a mut Vec<IndexRoot>,
}

impl SettingsListModel for SearchFoldersModel<'_> {
    fn supports_toggle(&self) -> bool {
        true
    }
    fn supports_add(&self) -> bool {
        true
    }
    fn supports_delete(&self) -> bool {
        true
    }
    fn supports_reset_default(&self) -> bool {
        true
    }
    fn supports_move(&self) -> bool {
        true
    }
    fn supports_modifier_help(&self) -> bool {
        true
    }
    fn columns(&self, language: AppLanguage) -> Vec<ColumnDef> {
        columns_for_page(SettingsPage::SearchFolders, language)
    }
    fn row_count(&self) -> usize {
        self.roots.len()
    }
    fn is_row_enabled(&self, index: usize) -> bool {
        self.roots.get(index).is_some_and(|r| r.enabled)
    }
    fn cell_text(&self, row: usize, col: usize) -> String {
        let Some(root) = self.roots.get(row) else {
            return String::new();
        };
        match col {
            0 => root.raw.clone(),
            1 => search_folder_modifier_display(&root.keywords),
            2 => root.score.to_string(),
            3 => depth_display(root.max_depth),
            4 => default_index_root_value(root),
            _ => String::new(),
        }
    }
    fn set_cell_text(&mut self, row: usize, col: usize, text: &str) {
        let Some(root) = self.roots.get_mut(row) else {
            return;
        };
        match col {
            0 => {
                let updated = normalized_index_root(IndexRoot {
                    raw: text.trim().to_string(),
                    path: root.path.clone(),
                    enabled: root.enabled,
                    score: root.score,
                    max_depth: root.max_depth,
                    label: root.label.clone(),
                    keywords: root.keywords.clone(),
                });
                *root = updated;
            }
            1 => {
                root.keywords = normalize_keywords(text);
            }
            2 => root.score = text.parse().unwrap_or(root.score),
            3 => {
                if let Ok(d) = text.trim().parse::<isize>() {
                    root.max_depth = normalize_search_depth(d);
                }
            }
            4 => parse_default_value_into(text, root),
            _ => {}
        }
    }
    fn cell_style(&self, row: usize, col: usize, language: AppLanguage) -> CellStyle {
        let base_style = default_cell_style(&self.columns(language), col);
        if base_style != CellStyle::Normal {
            return base_style;
        }
        let Some(root) = self.roots.get(row) else {
            return CellStyle::Normal;
        };
        let Some(default_root) = default_index_root_for(root) else {
            return CellStyle::Normal;
        };
        let changed = match col {
            0 => folded_text_changed(&root.raw, &default_root.raw),
            1 => root.keywords != default_root.keywords,
            2 => root.score != default_root.score,
            3 => root.max_depth != default_root.max_depth,
            _ => false,
        };
        if changed {
            CellStyle::ModifiedEditable
        } else {
            CellStyle::Normal
        }
    }
    fn toggle_row(&mut self, index: usize) -> bool {
        if let Some(root) = self.roots.get_mut(index) {
            root.enabled = !root.enabled;
            true
        } else {
            false
        }
    }
    fn delete_row(&mut self, index: usize) -> bool {
        if self
            .roots
            .get(index)
            .is_some_and(|root| default_index_root_for(root).is_some())
        {
            return false;
        }
        if index < self.roots.len() {
            self.roots.remove(index);
            true
        } else {
            false
        }
    }
    fn reset_row_to_default(&mut self, index: usize) -> bool {
        let Some(default_root) = self.roots.get(index).and_then(default_index_root_for) else {
            return false;
        };
        self.roots[index] = normalized_index_root(default_root);
        true
    }
    fn swap_rows(&mut self, a: usize, b: usize) -> bool {
        if a < self.roots.len() && b < self.roots.len() && a != b {
            self.roots.swap(a, b);
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Launch History
// ---------------------------------------------------------------------------
pub(crate) struct QueryLaunchRulesModel<'a> {
    pub items: &'a mut Vec<QueryLaunchRule>,
}

impl SettingsListModel for QueryLaunchRulesModel<'_> {
    fn supports_delete(&self) -> bool {
        true
    }
    fn columns(&self, language: AppLanguage) -> Vec<ColumnDef> {
        columns_for_page(SettingsPage::QueryLaunchRules, language)
    }
    fn row_count(&self) -> usize {
        self.items.len()
    }
    fn is_row_enabled(&self, _index: usize) -> bool {
        true
    }
    fn cell_text(&self, row: usize, col: usize) -> String {
        let Some(entry) = self.items.get(row) else {
            return String::new();
        };
        match col {
            0 => entry.query.clone(),
            1 => entry.target.clone(),
            _ => String::new(),
        }
    }
    fn set_cell_text(&mut self, _row: usize, _col: usize, _text: &str) {}
    fn toggle_row(&mut self, _index: usize) -> bool {
        false
    }
    fn delete_row(&mut self, index: usize) -> bool {
        if index < self.items.len() {
            self.items.remove(index);
            true
        } else {
            false
        }
    }
    fn swap_rows(&mut self, _a: usize, _b: usize) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// 4. Plugin Aliases
// ---------------------------------------------------------------------------

pub(crate) struct PluginAliasModel<'a> {
    pub entries: &'a mut Vec<plugins::PluginAliasConfigEntry>,
}

impl SettingsListModel for PluginAliasModel<'_> {
    fn supports_reset_default(&self) -> bool {
        true
    }
    fn columns(&self, language: AppLanguage) -> Vec<ColumnDef> {
        columns_for_page(SettingsPage::PluginAliases, language)
    }
    fn row_count(&self) -> usize {
        self.entries.len()
    }
    fn is_row_enabled(&self, _index: usize) -> bool {
        true
    }
    fn cell_text(&self, row: usize, col: usize) -> String {
        let Some(entry) = self.entries.get(row) else {
            return String::new();
        };
        match col {
            0 => entry.name.clone(),
            1 => entry.alias.clone(),
            2 => entry.default_alias.clone(),
            _ => String::new(),
        }
    }
    fn set_cell_text(&mut self, row: usize, col: usize, text: &str) {
        let Some(entry) = self.entries.get_mut(row) else {
            return;
        };
        match col {
            0 => entry.name = text.to_string(),
            1 => entry.alias = text.to_string(),
            2 => entry.default_alias = text.to_string(),
            _ => {}
        }
    }
    fn cell_style(&self, row: usize, col: usize, language: AppLanguage) -> CellStyle {
        let base_style = default_cell_style(&self.columns(language), col);
        if base_style != CellStyle::Normal {
            return base_style;
        }
        let Some(entry) = self.entries.get(row) else {
            return CellStyle::Normal;
        };
        if col == 1 && text_changed(&entry.alias, &entry.default_alias) {
            CellStyle::ModifiedEditable
        } else {
            CellStyle::Normal
        }
    }
    fn reset_row_to_default(&mut self, index: usize) -> bool {
        let Some(entry) = self.entries.get_mut(index) else {
            return false;
        };
        entry.alias = entry.default_alias.clone();
        true
    }
}

// ---------------------------------------------------------------------------
// 5 & 6. Scoring (Heuristic / Pattern - same adapter, different filter)
// ---------------------------------------------------------------------------

pub(crate) struct ScoringModel<'a> {
    pub rules: &'a mut Vec<ScoringRuleEntry>,
    pub kind: ScoringRuleKind,
    pub language: AppLanguage,
}

impl ScoringModel<'_> {
    /// Map visible row index to actual index in the backing Vec.
    fn actual_index(&self, visible: usize) -> Option<usize> {
        let mut count = 0usize;
        for (i, rule) in self.rules.iter().enumerate() {
            if rule.kind == self.kind {
                if count == visible {
                    return Some(i);
                }
                count += 1;
            }
        }
        None
    }

    /// Number of visible rows (entries matching `self.kind`).
    fn visible_count(&self) -> usize {
        self.rules.iter().filter(|r| r.kind == self.kind).count()
    }
}

impl SettingsListModel for ScoringModel<'_> {
    fn supports_toggle(&self) -> bool {
        true
    }
    fn supports_add(&self) -> bool {
        self.kind == ScoringRuleKind::Pattern
    }
    fn supports_delete(&self) -> bool {
        self.kind == ScoringRuleKind::Pattern
    }
    fn supports_reset_default(&self) -> bool {
        true
    }
    fn supports_move(&self) -> bool {
        self.kind == ScoringRuleKind::Pattern
    }
    fn supports_modifier_help(&self) -> bool {
        self.kind == ScoringRuleKind::Pattern
    }
    fn columns(&self, language: AppLanguage) -> Vec<ColumnDef> {
        let page = if self.kind == ScoringRuleKind::Pattern {
            SettingsPage::PatternScoring
        } else {
            SettingsPage::HeuristicScoring
        };
        columns_for_page(page, language)
    }
    fn row_count(&self) -> usize {
        self.visible_count()
    }
    fn is_row_enabled(&self, index: usize) -> bool {
        self.actual_index(index)
            .and_then(|i| self.rules.get(i))
            .is_some_and(|r| r.enabled)
    }
    fn cell_text(&self, row: usize, col: usize) -> String {
        let Some(idx) = self.actual_index(row) else {
            return String::new();
        };
        let rule = &self.rules[idx];
        if self.kind == ScoringRuleKind::Pattern {
            match col {
                0 => rule.key.clone(),
                1 => scoring_modifiers_display(&rule.modifiers),
                2 => rule.value.clone(),
                3 => default_scoring_rule_value(rule),
                _ => String::new(),
            }
        } else {
            match col {
                0 => heuristic_scoring_label(&rule.key, self.language),
                1 => rule.value.clone(),
                2 => heuristic_scoring_note(&rule.key, self.language).to_string(),
                3 => default_scoring_rule_value(rule),
                _ => String::new(),
            }
        }
    }
    fn set_cell_text(&mut self, row: usize, col: usize, text: &str) {
        let Some(idx) = self.actual_index(row) else {
            return;
        };
        let rule = &mut self.rules[idx];
        if rule.kind == ScoringRuleKind::Pattern {
            match col {
                0 => rule.key = text.to_string(),
                1 => {
                    rule.modifiers = normalize_keywords(text);
                }
                2 => rule.value = text.to_string(),
                _ => {}
            }
        } else {
            if col == 1 {
                rule.value = text.to_string()
            }
        }
    }
    fn cell_style(&self, row: usize, col: usize, language: AppLanguage) -> CellStyle {
        let base_style = default_cell_style(&self.columns(language), col);
        if base_style != CellStyle::Normal {
            return base_style;
        }
        let Some(idx) = self.actual_index(row) else {
            return CellStyle::Normal;
        };
        let rule = &self.rules[idx];
        let Some(default_rule) = default_scoring_rule_for_key(rule) else {
            return CellStyle::Normal;
        };
        let changed = if self.kind == ScoringRuleKind::Pattern {
            match col {
                0 => folded_text_changed(&rule.key, &default_rule.key),
                1 => rule.modifiers != default_rule.modifiers,
                2 => text_changed(&rule.value, &default_rule.value),
                _ => false,
            }
        } else {
            match col {
                1 => text_changed(&rule.value, &default_rule.value),
                _ => false,
            }
        };
        if changed {
            CellStyle::ModifiedEditable
        } else {
            CellStyle::Normal
        }
    }
    fn toggle_row(&mut self, index: usize) -> bool {
        if let Some(idx) = self.actual_index(index) {
            self.rules[idx].enabled = !self.rules[idx].enabled;
            true
        } else {
            false
        }
    }
    fn delete_row(&mut self, index: usize) -> bool {
        if self.kind != ScoringRuleKind::Pattern {
            return false;
        }
        if let Some(idx) = self.actual_index(index) {
            self.rules.remove(idx);
            true
        } else {
            false
        }
    }
    fn reset_row_to_default(&mut self, index: usize) -> bool {
        let Some(idx) = self.actual_index(index) else {
            return false;
        };
        let Some(default_rule) = default_scoring_rule_for_key(&self.rules[idx]) else {
            return false;
        };
        self.rules[idx] = default_rule;
        true
    }
    fn swap_rows(&mut self, a: usize, b: usize) -> bool {
        if self.kind != ScoringRuleKind::Pattern {
            return false;
        }
        let (Some(ai), Some(bi)) = (self.actual_index(a), self.actual_index(b)) else {
            return false;
        };
        if ai != bi {
            self.rules.swap(ai, bi);
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Generic helper - populate a ListView from any model
// ---------------------------------------------------------------------------

/// Fill a ListView from a model.  Returns the row count.
pub(crate) unsafe fn populate_list_from_model(
    hwnd: HWND,
    model: &dyn SettingsListModel,
    language: AppLanguage,
    checkboxes: bool,
    sort_header: bool,
) -> usize {
    configure_report_list_view_with_options(hwnd, checkboxes, sort_header);
    clear_list_view(hwnd);
    reset_list_view_columns(hwnd, &model.column_defs(language));
    for row in 0..model.row_count() {
        let cells = model.row_cells(row, language);
        let refs: Vec<&str> = cells.iter().map(|s| s.as_str()).collect();
        add_list_view_row_with_param(
            hwnd,
            model.is_row_enabled(row),
            &refs,
            model.row_style_param(row, language),
        );
    }
    model.row_count()
}
