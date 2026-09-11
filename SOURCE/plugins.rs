use std::path::{Path, PathBuf};

use crate::AppLanguage;

pub mod calculator;

const ALIAS_FILE: &str = "plugin_aliases.ini";
const CALCULATOR_ID: &str = "calculator";
const CALCULATOR_NAME: &str = "Calculator";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginTarget {
    pub plugin_id: String,
    pub action_token: Vec<u8>,
}

pub struct PluginResult {
    pub title: String,
    pub subtitle: String,
    pub action_token: Vec<u8>,
    pub score: i32,
}

#[derive(Clone, PartialEq)]
pub struct PluginAliasConfigEntry {
    pub id: String,
    pub name: String,
    pub alias: String,
    pub default_alias: String,
}

#[derive(Clone)]
struct PluginAliases {
    calculator: String,
    source_text: Option<String>,
}

impl Default for PluginAliases {
    fn default() -> Self {
        Self {
            calculator: calculator::DEFAULT_ALIAS.to_string(),
            source_text: None,
        }
    }
}

impl PluginAliases {
    #[cfg_attr(test, allow(dead_code))]
    fn load(config_dir: &Path) -> Self {
        let mut aliases = Self::default();
        let path = alias_path(config_dir);
        let defaults = aliases_to_text(&aliases);
        let Some(content) =
            crate::ensure_optional_text_file(ALIAS_FILE, &path, &defaults, hydrate_aliases_text)
        else {
            return aliases;
        };

        for line in content.lines() {
            if let Some(alias) = parse_alias_line(line) {
                aliases.calculator = alias;
            }
        }
        aliases.source_text = Some(content);
        aliases
    }

    fn entries(&self) -> Vec<PluginAliasConfigEntry> {
        vec![PluginAliasConfigEntry {
            id: CALCULATOR_ID.to_string(),
            name: CALCULATOR_NAME.to_string(),
            alias: self.calculator.clone(),
            default_alias: calculator::DEFAULT_ALIAS.to_string(),
        }]
    }

    fn set_entries(&mut self, entries: &[PluginAliasConfigEntry]) {
        for entry in entries {
            if normalize_alias_key(&entry.id) == CALCULATOR_ID {
                self.calculator = normalize_plugin_alias(&entry.alias, calculator::DEFAULT_ALIAS);
            }
        }
    }

    fn calculator_alias(&self) -> &str {
        &self.calculator
    }
}

#[derive(Default)]
pub struct PluginRegistry {
    aliases: PluginAliases,
}

impl PluginRegistry {
    #[cfg_attr(test, allow(dead_code))]
    pub fn load(config_dir: &Path) -> Self {
        Self {
            aliases: PluginAliases::load(config_dir),
        }
    }

    pub fn alias_entries(&self) -> Vec<PluginAliasConfigEntry> {
        self.aliases.entries()
    }

    pub fn alias_entries_to_text(&self, entries: &[PluginAliasConfigEntry]) -> String {
        let mut aliases = self.aliases.clone();
        aliases.set_entries(entries);
        aliases_to_text(&aliases)
    }

    pub fn merge_alias_entries_text(
        &self,
        source_text: &str,
        entries: &[PluginAliasConfigEntry],
    ) -> String {
        let mut aliases = self.aliases.clone();
        aliases.set_entries(entries);
        merge_aliases_text(source_text, &aliases)
    }

    pub fn set_alias_entries_in_memory(&mut self, entries: &[PluginAliasConfigEntry]) {
        let mut aliases = self.aliases.clone();
        aliases.set_entries(entries);
        self.aliases = aliases;
    }

    pub fn is_plugin_query(&self, query: &str) -> bool {
        calculator::is_query(query.trim(), self.aliases.calculator_alias())
    }

    pub fn matching_plugin_id(&self, query: &str) -> Option<&'static str> {
        self.is_plugin_query(query).then_some(CALCULATOR_ID)
    }

    pub fn alias_for(&self, plugin_id: &str) -> Option<&str> {
        (plugin_id == CALCULATOR_ID).then(|| self.aliases.calculator_alias())
    }
}

pub type PluginState = PluginRegistry;

#[cfg(test)]
pub fn collect_plugin_results(query: &str, state: &PluginRegistry) -> Option<Vec<PluginResult>> {
    calculator::collect_results(
        query,
        &calculator::CalculatorState::default(),
        state.aliases.calculator_alias(),
        AppLanguage::Source,
    )
}

#[cfg(not(test))]
pub fn collect_plugin_results(_query: &str, _state: &PluginRegistry) -> Option<Vec<PluginResult>> {
    None
}

pub fn help_text(state: &PluginRegistry, language: AppLanguage) -> String {
    calculator::help_text(state.aliases.calculator_alias(), language)
}

pub fn normalize_plugin_alias(value: &str, default_alias: &str) -> String {
    let alias = value.split_whitespace().next().unwrap_or_default().trim();
    if alias.is_empty() {
        default_alias.to_string()
    } else {
        alias.to_string()
    }
}

fn normalize_alias_key(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '_', '-'], "")
}

fn parse_alias_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    if normalize_alias_key(key) != CALCULATOR_ID {
        return None;
    }
    let alias = value.split_whitespace().next()?.trim();
    (!alias.is_empty()).then(|| alias.to_string())
}

fn alias_path(config_dir: &Path) -> PathBuf {
    config_dir.join(ALIAS_FILE)
}

pub(crate) fn plugin_aliases_path(config_dir: &Path) -> PathBuf {
    alias_path(config_dir)
}

fn aliases_to_text(aliases: &PluginAliases) -> String {
    if let Some(source_text) = aliases.source_text.as_deref() {
        return merge_aliases_text(source_text, aliases);
    }
    default_aliases_text(&aliases.calculator)
}

fn default_aliases_text(calculator_alias: &str) -> String {
    format!(
        "# Flash Launch plugin aliases.\n# Format: plugin_id=alias\n{}={}\n",
        CALCULATOR_ID, calculator_alias
    )
}

fn alias_line_identity(line: &str) -> Option<String> {
    parse_alias_line(line).map(|_| CALCULATOR_ID.to_string())
}

pub(crate) fn hydrate_aliases_text(content: &str, defaults: &str) -> String {
    crate::append_missing_default_lines(content, defaults, alias_line_identity)
}

fn merge_aliases_text(content: &str, aliases: &PluginAliases) -> String {
    if content.is_empty() {
        return default_aliases_text(&aliases.calculator);
    }
    let mut output = String::with_capacity(content.len().saturating_add(32));
    let mut replaced = false;
    for line in content.split_inclusive('\n') {
        let (body, ending) = split_line_ending(line);
        if parse_alias_line(body).is_some() {
            if !replaced {
                output.push_str(CALCULATOR_ID);
                output.push('=');
                output.push_str(&aliases.calculator);
                output.push_str(ending);
                replaced = true;
            }
        } else {
            output.push_str(line);
        }
    }
    if !replaced {
        let line = format!("{}={}", CALCULATOR_ID, aliases.calculator);
        append_document_line(
            &mut output,
            &line,
            document_newline(content),
            content.ends_with(['\r', '\n']),
        );
    }
    output
}

fn document_newline(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn split_line_ending(line: &str) -> (&str, &str) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else {
        (line, "")
    }
}

fn append_document_line(
    output: &mut String,
    line: &str,
    newline: &str,
    preserve_final_newline: bool,
) {
    if !output.is_empty() && !output.ends_with(['\r', '\n']) {
        output.push_str(newline);
    }
    output.push_str(line);
    if preserve_final_newline {
        output.push_str(newline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_hydration_preserves_opaque_lines_and_appends_default() {
        let content = "# Keep this\r\nfuture=value\r\ncalculator=\r\nmalformed";
        let defaults = default_aliases_text(calculator::DEFAULT_ALIAS);
        let hydrated = hydrate_aliases_text(content, &defaults);

        assert!(hydrated.starts_with(content));
        assert!(hydrated.contains("calculator=\r\n"));
        assert!(hydrated.ends_with("calculator=/c"));
        assert!(!hydrated.ends_with(['\r', '\n']));
    }

    #[test]
    fn alias_hydration_keeps_valid_custom_value_without_duplicate() {
        let content = "# Keep this\ncalculator=/custom\nfuture=value\n";
        let defaults = default_aliases_text(calculator::DEFAULT_ALIAS);

        assert_eq!(hydrate_aliases_text(content, &defaults), content);
    }

    #[test]
    fn alias_save_updates_known_value_and_preserves_opaque_lines() {
        let source =
            "# Keep this\r\nfuture=value\r\ncalculator=\r\ncalculator=/old\r\nmalformed\r\n";
        let aliases = PluginAliases {
            calculator: "/new".to_string(),
            source_text: Some(source.to_string()),
        };
        let merged = aliases_to_text(&aliases);

        assert!(merged.contains("# Keep this\r\n"));
        assert!(merged.contains("future=value\r\n"));
        assert!(merged.contains("calculator=\r\n"));
        assert!(merged.contains("calculator=/new\r\n"));
        assert!(merged.contains("malformed\r\n"));
        assert!(!merged.contains("calculator=/old"));
    }
}
