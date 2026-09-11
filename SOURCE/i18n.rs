use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use crate::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettingsPage {
    General,
    SearchFolders,
    HeuristicScoring,
    PatternScoring,
    PluginAliases,
    QueryLaunchRules,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AppLanguage {
    Source,
    Loaded(usize),
}

pub(crate) struct LanguagePack {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) translations: HashMap<&'static str, &'static str>,
}

pub(crate) static LANGUAGE_PACKS: OnceLock<Vec<LanguagePack>> = OnceLock::new();
const SOURCE_LANGUAGE_INDEX: usize = usize::MAX;
static ACTIVE_LANGUAGE_INDEX: AtomicUsize = AtomicUsize::new(SOURCE_LANGUAGE_INDEX);
pub(crate) const LANGUAGE_FOLDER_NAME: &str = "Languages";
const BUILTIN_ENGLISH_ID: &str = "en";
pub(crate) const BUILTIN_ENGLISH_NAME: &str = "English";

impl AppLanguage {
    pub(crate) fn try_from_setting(value: &str) -> Option<Self> {
        let wanted = normalize_language_id(value);
        if wanted.is_empty() || wanted == BUILTIN_ENGLISH_ID || wanted == "english" {
            return Some(AppLanguage::Source);
        }
        language_packs()
            .iter()
            .enumerate()
            .find(|(_, pack)| {
                normalize_language_id(pack.id) == wanted
                    || normalize_language_id(pack.name) == wanted
            })
            .map(|(index, _)| AppLanguage::Loaded(index))
    }

    pub(crate) fn setting_value(self) -> &'static str {
        language_pack(self)
            .map(|pack| pack.id)
            .unwrap_or(BUILTIN_ENGLISH_ID)
    }

    pub(crate) fn combo_index(self) -> usize {
        match self {
            AppLanguage::Source => 0,
            AppLanguage::Loaded(index) if index < language_packs().len() => index + 1,
            AppLanguage::Loaded(_) => 0,
        }
    }

    pub(crate) fn from_combo_index(index: i32) -> Self {
        if index < 0 {
            AppLanguage::Source
        } else {
            language_from_index(index as usize)
        }
    }
}

pub(crate) fn language_from_index(index: usize) -> AppLanguage {
    if index == 0 {
        return AppLanguage::Source;
    }
    language_packs()
        .get(index - 1)
        .map(|_| AppLanguage::Loaded(index - 1))
        .unwrap_or(AppLanguage::Source)
}

pub(crate) fn default_language() -> AppLanguage {
    AppLanguage::Source
}

pub(crate) fn language_pack(language: AppLanguage) -> Option<&'static LanguagePack> {
    match language {
        AppLanguage::Source => None,
        AppLanguage::Loaded(index) => language_packs().get(index),
    }
}

pub(crate) fn language_packs() -> &'static [LanguagePack] {
    LANGUAGE_PACKS.get_or_init(load_language_packs).as_slice()
}

pub(crate) fn load_language_packs() -> Vec<LanguagePack> {
    load_language_packs_from(&languages_dir())
}

fn load_language_packs_from(language_dir: &Path) -> Vec<LanguagePack> {
    required_file_operation("Languages", language_dir, "create directory", || {
        fs::create_dir_all(language_dir)
    });
    let entries = required_file_operation("Languages", language_dir, "read directory", || {
        fs::read_dir(language_dir)?.collect::<std::io::Result<Vec<_>>>()
    });
    let mut paths = entries
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|value| value.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("ini"))
        })
        .collect::<Vec<_>>();
    paths.sort_by_key(|path| path.file_name().map(OsString::from).unwrap_or_default());

    paths
        .iter()
        .filter_map(|path| {
            let content = read_optional_text("language file", path)?;
            parse_language_content(path, &content)
        })
        .collect()
}

pub(crate) fn parse_language_file(path: &Path) -> Option<LanguagePack> {
    let content = read_optional_text("language file", path)?;
    parse_language_content(path, &content)
}

fn parse_language_content(path: &Path, content: &str) -> Option<LanguagePack> {
    let mut id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("custom")
        .to_string();
    let mut name = id.clone();
    let mut translations = HashMap::new();

    for line in content.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with(';')
            || line.starts_with('[')
        {
            continue;
        }
        let Some((key, value)) = split_language_line(line) else {
            continue;
        };
        let key = unescape_language_value(key.trim());
        let value = unescape_language_value(value.trim());
        match key.to_ascii_lowercase().as_str() {
            "id" if !value.is_empty() => id = value,
            "name" if !value.is_empty() => name = value,
            "id" | "name" => {}
            _ if !key.is_empty() => {
                translations.insert(leak_static(key), leak_static(value));
            }
            _ => {}
        }
    }

    Some(LanguagePack {
        id: leak_static(id),
        name: leak_static(name),
        translations,
    })
}

pub(crate) fn languages_dir() -> PathBuf {
    app_dir().join(LANGUAGE_FOLDER_NAME)
}

pub(crate) fn normalize_language_id(value: &str) -> String {
    fold_text(value.trim()).replace(['-', '_', ' '], "")
}

pub(crate) fn split_language_line(line: &str) -> Option<(&str, &str)> {
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '=' => return Some((&line[..index], &line[index + ch.len_utf8()..])),
            _ => {}
        }
    }
    None
}

pub(crate) fn unescape_language_value(value: &str) -> String {
    let mut output = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => output.push('\n'),
                Some('r') => output.push('\r'),
                Some('t') => output.push('\t'),
                Some('\\') => output.push('\\'),
                Some('=') => output.push('='),
                Some(other) => {
                    output.push('\\');
                    output.push(other);
                }
                None => output.push('\\'),
            }
        } else {
            output.push(ch);
        }
    }
    output
}

pub(crate) fn leak_static(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

pub(crate) fn localized_format1(
    language: AppLanguage,
    english: &'static str,
    arg1: impl std::fmt::Display,
) -> String {
    let mut text = localized(language, english).to_string();
    replace_next_placeholder(&mut text, &arg1.to_string());
    text
}

pub(crate) fn localized_format2(
    language: AppLanguage,
    english: &'static str,
    arg1: impl std::fmt::Display,
    arg2: impl std::fmt::Display,
) -> String {
    let mut text = localized_format1(language, english, arg1);
    replace_next_placeholder(&mut text, &arg2.to_string());
    text
}

pub(crate) fn localized_format3(
    language: AppLanguage,
    english: &'static str,
    arg1: impl std::fmt::Display,
    arg2: impl std::fmt::Display,
    arg3: impl std::fmt::Display,
) -> String {
    let mut text = localized_format2(language, english, arg1, arg2);
    replace_next_placeholder(&mut text, &arg3.to_string());
    text
}

fn replace_next_placeholder(text: &mut String, value: &str) {
    if let Some(index) = text.find("{}") {
        text.replace_range(index..index + 2, value);
    }
}

pub(crate) fn settings_page_from_nav_index(index: i32) -> SettingsPage {
    match index {
        1 => SettingsPage::SearchFolders,
        2 => SettingsPage::HeuristicScoring,
        3 => SettingsPage::PatternScoring,
        4 => SettingsPage::PluginAliases,
        5 => SettingsPage::QueryLaunchRules,
        _ => SettingsPage::General,
    }
}

pub(crate) fn config_nav_index_for_page(page: SettingsPage) -> usize {
    match page {
        SettingsPage::General => 0,
        SettingsPage::SearchFolders => 1,
        SettingsPage::HeuristicScoring => 2,
        SettingsPage::PatternScoring => 3,
        SettingsPage::PluginAliases => 4,
        SettingsPage::QueryLaunchRules => 5,
    }
}

pub(crate) fn settings_page_description(page: SettingsPage, language: AppLanguage) -> &'static str {
    match page {
        SettingsPage::General => localized(language, "General options combine popup behavior, display choices, and the global popup hotkey."),
        SettingsPage::SearchFolders => localized(language, "Directories are searched in the order listed, so put small directories and Start Menu entries near the top. Space or Enable / Disable temporarily toggles items."),
        SettingsPage::HeuristicScoring => localized(language, "Heuristic scoring controls exact, leftmost, history, folder, and recency boosts."),
        SettingsPage::PatternScoring => localized(language, "Pattern scoring adds or subtracts points using wildcard path patterns and optional query modifiers."),
        SettingsPage::PluginAliases => localized(language, "Plugin Alias controls the search prefix used to invoke each built-in plugin, such as /c for Calculator."),
        SettingsPage::QueryLaunchRules => localized(language, "Query Launch Rules remember the last result launched for each search query and prioritize it next time."),
    }
}

pub(crate) fn settings_page_detail_lines(
    page: SettingsPage,
    language: AppLanguage,
) -> (&'static str, &'static str, &'static str) {
    match page {
        SettingsPage::General => (
            localized(language, "General"),
            localized(
                language,
                "Popup hotkey, result count, and sound are saved here.",
            ),
            localized(
                language,
                "Click Save + Apply after changing anything on this page.",
            ),
        ),
        SettingsPage::SearchFolders => (
            localized(language, "File"),
            "",
            localized(language, "Score"),
        ),
        SettingsPage::HeuristicScoring => (
            localized(language, "Heuristic Scoring"),
            localized(
                language,
                "Edit all heuristic weights and toggle any rule on or off.",
            ),
            localized(
                language,
                "Disabled heuristic rules are saved with <<< and contribute no score.",
            ),
        ),
        SettingsPage::PatternScoring => (
            localized(language, "Pattern Scoring"),
            localized(
                language,
                "Add, remove, reorder, or toggle wildcard pattern rules.",
            ),
            localized(
                language,
                "Pattern rules are applied during ranking after the folder score is known.",
            ),
        ),
        SettingsPage::PluginAliases => (
            localized(language, "Plugin Alias"),
            localized(language, "Edit the search prefix that invokes each plugin."),
            localized(
                language,
                "Leaving an alias empty restores that plugin's default alias.",
            ),
        ),
        SettingsPage::QueryLaunchRules => (
            localized(language, "Query Launch Rules"),
            localized(language, "Search Query"),
            localized(language, "Last Launched Result"),
        ),
    }
}

pub(crate) fn settings_page_tip(page: SettingsPage, language: AppLanguage) -> &'static str {
    match page {
        SettingsPage::General => "",
        SettingsPage::SearchFolders => "",
        SettingsPage::HeuristicScoring | SettingsPage::PatternScoring => localized(language, "TIP: Scoring saves to CONFIG\\scoring.ini and reloads ranking immediately."),
        SettingsPage::PluginAliases => localized(language, "TIP: Plugin Alias is matched against the typed search query, not against file names."),
        SettingsPage::QueryLaunchRules => localized(language, "TIP: Ctrl+Up / Ctrl+Down recalls queries from this table; delete selected rows to make the app learn again."),
    }
}

pub(crate) fn set_active_language(language: AppLanguage) {
    let index = match language {
        AppLanguage::Source => SOURCE_LANGUAGE_INDEX,
        AppLanguage::Loaded(index) => index,
    };
    ACTIVE_LANGUAGE_INDEX.store(index, Ordering::Relaxed);
}

pub(crate) fn localized_active(english: &'static str) -> &'static str {
    let index = ACTIVE_LANGUAGE_INDEX.load(Ordering::Relaxed);
    if index == SOURCE_LANGUAGE_INDEX {
        return english;
    }
    LANGUAGE_PACKS
        .get()
        .and_then(|packs| packs.get(index))
        .and_then(|pack| pack.translations.get(english).copied())
        .unwrap_or(english)
}

pub(crate) fn localized_active_format1(
    english: &'static str,
    arg1: impl std::fmt::Display,
) -> String {
    let mut text = localized_active(english).to_string();
    replace_next_placeholder(&mut text, &arg1.to_string());
    text
}

pub(crate) fn localized_active_format2(
    english: &'static str,
    arg1: impl std::fmt::Display,
    arg2: impl std::fmt::Display,
) -> String {
    let mut text = localized_active_format1(english, arg1);
    replace_next_placeholder(&mut text, &arg2.to_string());
    text
}

pub(crate) fn localized_active_format3(
    english: &'static str,
    arg1: impl std::fmt::Display,
    arg2: impl std::fmt::Display,
    arg3: impl std::fmt::Display,
) -> String {
    let mut text = localized_active_format2(english, arg1, arg2);
    replace_next_placeholder(&mut text, &arg3.to_string());
    text
}

pub(crate) fn localized(language: AppLanguage, english: &'static str) -> &'static str {
    if let Some(pack) = language_pack(language) {
        if let Some(value) = pack.translations.get(english) {
            return value;
        }
    }
    english
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholders(text: &str) -> Vec<&str> {
        let mut values = Vec::new();
        let mut remaining = text;
        while let Some(start) = remaining.find('{') {
            remaining = &remaining[start..];
            let end = remaining.find('}').expect("Unclosed translation placeholder");
            values.push(&remaining[..=end]);
            remaining = &remaining[end + 1..];
        }
        values.sort_unstable();
        values
    }

    #[test]
    fn shipped_packs_have_complete_keys_and_preserve_formatting() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("Languages");
        let reference = parse_language_file(&directory.join("vi.ini")).unwrap();
        let expected_ids = ["vi", "zh-CN", "es", "pt-BR", "ja", "de", "fr", "ko", "zh-TW"];
        for id in expected_ids {
            let path = directory.join(format!("{id}.ini"));
            let pack = parse_language_file(&path).unwrap();
            assert_eq!(pack.id, id);
            assert_eq!(pack.translations.len(), reference.translations.len(), "{id}: key count");
            let mut raw_keys = std::collections::HashSet::new();
            for line in fs::read_to_string(&path).unwrap().lines() {
                if line.starts_with(['#', ';', '[']) || line.trim().is_empty() {
                    continue;
                }
                if let Some((key, _)) = split_language_line(line) {
                    assert!(raw_keys.insert(unescape_language_value(key.trim())), "{id}: duplicate {key}");
                }
            }
            for key in reference.translations.keys() {
                let value = pack.translations.get(key).unwrap_or_else(|| panic!("{id}: missing {key}"));
                assert!(!value.trim().is_empty(), "{id}: empty {key}");
                assert_eq!(placeholders(key), placeholders(value), "{id}: placeholders in {key}");
                assert_eq!(key.matches('\n').count(), value.matches('\n').count(), "{id}: line breaks in {key}");
                for syntax in ["CONFIG\\scoring.ini", "recent_items.txt", "/c", "+keyword", "-keyword", "+mp3", "<<<"] {
                    if key.contains(syntax) {
                        assert!(value.contains(syntax), "{id}: missing syntax {syntax} in {key}");
                    }
                }
            }
        }
    }

    fn test_language_dir(name: &str) -> PathBuf {
        let path = app_dir()
            .join("TEMP")
            .join("language-loader-tests")
            .join(format!("{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn parser_preserves_escaped_equals_and_metadata_fallbacks() {
        let pack = parse_language_content(
            Path::new("custom_language.ini"),
            r#"id=
name=
[Strings]
Open\=File=Translated equals \= sign
"#,
        )
        .unwrap();

        assert_eq!(pack.id, "custom_language");
        assert_eq!(pack.name, "custom_language");
        assert_eq!(
            pack.translations.get("Open=File").copied(),
            Some("Translated equals = sign")
        );
    }

    #[test]
    fn folder_loader_tracks_ini_files_without_bundled_defaults() {
        let directory = test_language_dir("dynamic-files");
        fs::write(
            directory.join("zeta.ini"),
            "id=zeta\nname=Zeta\n[Strings]\nOpen=Zeta Open\n",
        )
        .unwrap();
        fs::write(
            directory.join("alpha.ini"),
            "id=alpha\nname=Alpha\n[Strings]\nOpen=Alpha Open\n",
        )
        .unwrap();
        fs::write(directory.join("ignored.txt"), "id=ignored\n").unwrap();

        let packs = load_language_packs_from(&directory);
        assert_eq!(
            packs.iter().map(|pack| pack.id).collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );

        fs::remove_file(directory.join("alpha.ini")).unwrap();
        let packs = load_language_packs_from(&directory);
        assert_eq!(
            packs.iter().map(|pack| pack.id).collect::<Vec<_>>(),
            vec!["zeta"]
        );
        assert!(!directory.join("alpha.ini").exists());

        fs::remove_file(directory.join("zeta.ini")).unwrap();
        assert!(load_language_packs_from(&directory).is_empty());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn builtin_english_uses_stable_setting_and_combo_slot() {
        assert_eq!(AppLanguage::try_from_setting(""), Some(AppLanguage::Source));
        assert_eq!(
            AppLanguage::try_from_setting("en"),
            Some(AppLanguage::Source)
        );
        assert_eq!(
            AppLanguage::try_from_setting("English"),
            Some(AppLanguage::Source)
        );
        assert_eq!(AppLanguage::Source.setting_value(), "en");
        assert_eq!(AppLanguage::Source.combo_index(), 0);
        assert_eq!(AppLanguage::from_combo_index(0), AppLanguage::Source);
        assert_eq!(default_language(), AppLanguage::Source);

        if !language_packs().is_empty() {
            assert_eq!(AppLanguage::Loaded(0).combo_index(), 1);
            assert_eq!(AppLanguage::from_combo_index(1), AppLanguage::Loaded(0));
        }
    }
}
