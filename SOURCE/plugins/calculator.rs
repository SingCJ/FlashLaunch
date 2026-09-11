use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use super::{append_document_line, document_newline, split_line_ending, PluginResult};
use crate::{localized, localized_format1, localized_format2, AppLanguage};

pub const DEFAULT_ALIAS: &str = "/c";
const HISTORY_LIMIT: usize = 200;
const MAX_RESULT_LIMIT: usize = 200;

#[derive(Clone)]
pub enum CalculatorTarget {
    Save { expression: String, result: String },
    Recall { expression: String },
}

#[derive(Clone)]
struct CalculatorHistoryEntry {
    expression: String,
    result: String,
}

#[derive(Default)]
pub struct CalculatorState {
    history: Vec<CalculatorHistoryEntry>,
    history_document: Option<String>,
}

impl CalculatorState {
    pub fn load(config_dir: &Path) -> Self {
        let (history, history_document) = load_history(config_dir);
        Self {
            history,
            history_document,
        }
    }
}

pub fn collect_results(
    query: &str,
    state: &CalculatorState,
    alias: &str,
    language: AppLanguage,
) -> Option<Vec<PluginResult>> {
    let expression = calculator_query(query.trim(), alias)?;
    Some(collect_calculator_results(
        expression,
        &state.history,
        language,
    ))
}

pub fn is_query(query: &str, alias: &str) -> bool {
    calculator_query(query, alias).is_some()
}

pub fn help_text(alias: &str, language: AppLanguage) -> String {
    localized_format1(language, "{} calculator", effective_alias(alias))
}

pub fn invoke_token(
    state: &mut CalculatorState,
    config_dir: &Path,
    token: &[u8],
    alias: &str,
    language: AppLanguage,
) -> io::Result<Option<String>> {
    let target = decode_target(token)?;
    let alias = effective_alias(alias);
    match target {
        CalculatorTarget::Save { expression, result } => {
            record_history(&mut state.history, &expression, &result);
            let saved_document = save_history(
                config_dir,
                &state.history,
                state.history_document.as_deref(),
            )
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Other,
                    localized(language, "Could not save history"),
                )
            })?;
            state.history_document = Some(saved_document);
            Ok(Some(format!("{alias} ")))
        }
        CalculatorTarget::Recall { expression } => Ok(Some(format!("{alias} {expression}"))),
    }
}

fn calculator_query<'a>(query: &'a str, alias: &str) -> Option<&'a str> {
    let alias = effective_alias(alias);
    let prefix = query.get(..alias.len())?;
    if !prefix.eq_ignore_ascii_case(alias) {
        return None;
    }
    let rest = &query[alias.len()..];
    if rest.is_empty() {
        Some("")
    } else if rest.chars().next().is_some_and(char::is_whitespace) {
        Some(rest.trim_start())
    } else {
        None
    }
}

fn effective_alias(alias: &str) -> &str {
    let alias = alias.trim();
    if alias.is_empty() {
        DEFAULT_ALIAS
    } else {
        alias
    }
}

fn collect_calculator_results(
    expression: &str,
    history: &[CalculatorHistoryEntry],
    language: AppLanguage,
) -> Vec<PluginResult> {
    let mut results = Vec::new();
    let expression = expression.trim();
    if !expression.is_empty() {
        match evaluate_expression(expression) {
            Ok(value) => {
                let result = format_number(value);
                results.push(PluginResult {
                    title: format!("{expression} = {result}"),
                    subtitle: String::new(),
                    action_token: encode_target(&CalculatorTarget::Save {
                        expression: expression.to_string(),
                        result: result.clone(),
                    }),
                    score: i32::MAX,
                });
            }
            Err(error) => {
                let error = if error == "Result is not finite" {
                    localized(language, "Result is not finite").to_string()
                } else {
                    error
                };
                results.push(PluginResult {
                    title: localized_format2(language, "{} = [ERR: {}]", expression, error),
                    subtitle: String::new(),
                    action_token: encode_target(&CalculatorTarget::Recall {
                        expression: expression.to_string(),
                    }),
                    score: i32::MAX,
                });
            }
        }
    }

    for (index, entry) in history
        .iter()
        .take(MAX_RESULT_LIMIT - results.len())
        .enumerate()
    {
        results.push(PluginResult {
            title: format!("{} = {}", entry.expression, entry.result),
            subtitle: String::new(),
            action_token: encode_target(&CalculatorTarget::Recall {
                expression: entry.expression.clone(),
            }),
            score: i32::MAX - 1 - index as i32,
        });
    }
    results
}

fn encode_target(target: &CalculatorTarget) -> Vec<u8> {
    let mut payload = Vec::new();
    match target {
        CalculatorTarget::Save { expression, result } => {
            payload.push(1);
            write_token_string(&mut payload, expression);
            write_token_string(&mut payload, result);
        }
        CalculatorTarget::Recall { expression } => {
            payload.push(2);
            write_token_string(&mut payload, expression);
        }
    }
    payload
}

fn decode_target(token: &[u8]) -> io::Result<CalculatorTarget> {
    let mut cursor = 0usize;
    let kind = *token
        .get(cursor)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "Missing action token type"))?;
    cursor += 1;
    let target = match kind {
        1 => CalculatorTarget::Save {
            expression: read_token_string(token, &mut cursor)?,
            result: read_token_string(token, &mut cursor)?,
        },
        2 => CalculatorTarget::Recall {
            expression: read_token_string(token, &mut cursor)?,
        },
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unknown calculator action token",
            ))
        }
    };
    if cursor != token.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Trailing calculator action token data",
        ));
    }
    Ok(target)
}

fn write_token_string(output: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(bytes);
}

fn read_token_string(input: &[u8], cursor: &mut usize) -> io::Result<String> {
    let length_end = cursor
        .checked_add(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Action token overflow"))?;
    let length_bytes = input
        .get(*cursor..length_end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "Missing string length"))?;
    let length = u32::from_le_bytes(length_bytes.try_into().unwrap()) as usize;
    *cursor = length_end;
    let value_end = cursor
        .checked_add(length)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Action token overflow"))?;
    let value = input
        .get(*cursor..value_end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "Missing string data"))?;
    *cursor = value_end;
    String::from_utf8(value.to_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid UTF-8 action token"))
}

pub fn evaluate_expression(expression: &str) -> Result<f64, String> {
    let value = meval::eval_str(expression).map_err(|error| error.to_string())?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err("Result is not finite".to_string())
    }
}

pub fn format_number(value: f64) -> String {
    let rounded = value.round();
    if (value - rounded).abs() < 0.0000000001 {
        return format!("{rounded:.0}");
    }
    let mut output = format!("{value:.12}");
    while output.contains('.') && output.ends_with('0') {
        output.pop();
    }
    if output.ends_with('.') {
        output.pop();
    }
    output
}

fn load_history(config_dir: &Path) -> (Vec<CalculatorHistoryEntry>, Option<String>) {
    let path = history_path(config_dir);
    let Some(content) =
        crate::ensure_optional_text_file("calculator_history.txt", &path, "", |content, _| {
            content.to_string()
        })
    else {
        return (Vec::new(), None);
    };

    let history = parse_history_text(&content);
    (history, Some(content))
}

fn parse_history_text(content: &str) -> Vec<CalculatorHistoryEntry> {
    let mut seen = HashSet::new();
    content
        .lines()
        .filter_map(|line| {
            let entry = parse_history_line(line)?;
            let key = format!("{}\t{}", entry.expression.to_lowercase(), entry.result);
            if !seen.insert(key) {
                return None;
            }
            Some(entry)
        })
        .take(HISTORY_LIMIT)
        .collect()
}

fn parse_history_line(line: &str) -> Option<CalculatorHistoryEntry> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (expression, result) = line.split_once('\t')?;
    let expression = expression.trim();
    let result = result.trim();
    if expression.is_empty() || result.is_empty() {
        return None;
    }
    Some(CalculatorHistoryEntry {
        expression: expression.to_string(),
        result: result.to_string(),
    })
}

fn save_history(
    config_dir: &Path,
    entries: &[CalculatorHistoryEntry],
    source_text: Option<&str>,
) -> Option<String> {
    let path = history_path(config_dir);
    let content = merge_history_text(source_text.unwrap_or_default(), entries);
    if crate::write_optional_text("calculator_history.txt", &path, &content) {
        Some(content)
    } else {
        None
    }
}

fn merge_history_text(source_text: &str, entries: &[CalculatorHistoryEntry]) -> String {
    let entries = &entries[..entries.len().min(HISTORY_LIMIT)];
    let mut output = String::with_capacity(source_text.len().saturating_add(128));
    let mut next_entry = 0usize;
    for line in source_text.split_inclusive('\n') {
        let (body, ending) = split_line_ending(line);
        if parse_history_line(body).is_some() {
            if let Some(entry) = entries.get(next_entry) {
                output.push_str(&history_entry_line(entry));
                output.push_str(ending);
                next_entry += 1;
            }
        } else {
            output.push_str(line);
        }
    }

    let newline = document_newline(source_text);
    let preserve_final_newline = source_text.is_empty() || source_text.ends_with(['\r', '\n']);
    for entry in entries.iter().skip(next_entry).take(HISTORY_LIMIT) {
        append_document_line(
            &mut output,
            &history_entry_line(entry),
            newline,
            preserve_final_newline,
        );
    }
    output
}

fn history_entry_line(entry: &CalculatorHistoryEntry) -> String {
    format!(
        "{}\t{}",
        clean_history_field(&entry.expression),
        clean_history_field(&entry.result)
    )
}

fn record_history(entries: &mut Vec<CalculatorHistoryEntry>, expression: &str, result: &str) {
    let expression = expression.trim();
    let result = result.trim();
    if expression.is_empty() || result.is_empty() {
        return;
    }

    let expression_key = expression.to_lowercase();
    entries.retain(|entry| entry.expression.to_lowercase() != expression_key);
    entries.insert(
        0,
        CalculatorHistoryEntry {
            expression: expression.to_string(),
            result: result.to_string(),
        },
    );
    entries.truncate(HISTORY_LIMIT);
}

fn clean_history_field(value: &str) -> String {
    value.replace(['\r', '\n', '\t'], " ").trim().to_string()
}

fn history_path(config_dir: &Path) -> PathBuf {
    config_dir.join("calculator_history.txt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn alias_returns_instant_result() {
        let state = CalculatorState::default();
        let results =
            collect_results("/c 2+4*3", &state, DEFAULT_ALIAS, AppLanguage::Source).unwrap();
        assert_eq!(
            results.first().map(|result| result.title.as_str()),
            Some("2+4*3 = 14")
        );
    }

    #[test]
    fn custom_alias_returns_instant_result() {
        let state = CalculatorState::default();
        let results = collect_results("/calc 2+2", &state, "/calc", AppLanguage::Source).unwrap();
        assert_eq!(
            results.first().map(|result| result.title.as_str()),
            Some("2+2 = 4")
        );
        assert!(collect_results("/c 2+2", &state, "/calc", AppLanguage::Source).is_none());
    }

    #[test]
    fn evaluates_expression() {
        assert_eq!(format_number(evaluate_expression("2+4*3").unwrap()), "14");
        assert_eq!(format_number(evaluate_expression("28^3").unwrap()), "21952");
        assert_eq!(format_number(evaluate_expression("-2^2").unwrap()), "-4");
        assert_eq!(format_number(evaluate_expression("(-2)^2").unwrap()), "4");
        assert_eq!(format_number(evaluate_expression("2^-2").unwrap()), "0.25");
        assert_eq!(format_number(evaluate_expression("sqrt(9)").unwrap()), "3");
    }

    #[test]
    fn rejects_non_finite_results() {
        assert!(evaluate_expression("(-1)^0.5").is_err());
        assert!(evaluate_expression("1e309").is_err());
        assert!(evaluate_expression("2+").is_err());
    }

    #[test]
    fn history_parser_ignores_comments_malformed_lines_and_duplicates() {
        let content = "# Keep\nmalformed\n2+2\t4\n2+2\t4\nbroken\t\n3+3\t6\n";
        let entries = parse_history_text(content);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].expression, "2+2");
        assert_eq!(entries[0].result, "4");
        assert_eq!(entries[1].expression, "3+3");
        assert_eq!(entries[1].result, "6");
    }

    #[test]
    fn history_save_preserves_opaque_lines_and_newline_style() {
        let source = "# Keep\r\nmalformed\r\n2+2\t4\r\nbroken\t\r\nfuture=value";
        let entries = vec![
            CalculatorHistoryEntry {
                expression: "3+3".to_string(),
                result: "6".to_string(),
            },
            CalculatorHistoryEntry {
                expression: "2+2".to_string(),
                result: "4".to_string(),
            },
        ];
        let merged = merge_history_text(source, &entries);

        assert!(merged.starts_with("# Keep\r\nmalformed\r\n3+3\t6\r\n"));
        assert!(merged.contains("broken\t\r\n"));
        assert!(merged.contains("future=value\r\n2+2\t4"));
        assert!(!merged.ends_with(['\r', '\n']));
    }

    #[test]
    fn action_token_survives_state_restart_and_persists_history() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let config_dir = std::env::current_dir()
            .unwrap()
            .join("TEMP")
            .join("calculator-host-tests")
            .join(format!("{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&config_dir).unwrap();

        let state = CalculatorState::default();
        let result = collect_results("/c 7*6", &state, DEFAULT_ALIAS, AppLanguage::Source)
            .unwrap()
            .remove(0);
        let mut first_host_state = CalculatorState::default();
        assert_eq!(
            invoke_token(
                &mut first_host_state,
                &config_dir,
                &result.action_token,
                DEFAULT_ALIAS,
                AppLanguage::Source,
            )
            .unwrap()
            .as_deref(),
            Some("/c ")
        );

        let restarted_state = CalculatorState::load(&config_dir);
        let restarted_results =
            collect_results("/c", &restarted_state, DEFAULT_ALIAS, AppLanguage::Source).unwrap();
        assert!(restarted_results
            .iter()
            .any(|result| result.title == "7*6 = 42"));

        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn action_token_rejects_unknown_kind_and_trailing_bytes() {
        let mut state = CalculatorState::default();
        assert!(invoke_token(
            &mut state,
            Path::new("."),
            &[99],
            DEFAULT_ALIAS,
            AppLanguage::Source,
        )
        .is_err());

        let token = encode_target(&CalculatorTarget::Recall {
            expression: "2+2".to_string(),
        });
        let mut malformed = token;
        malformed.push(0);
        assert!(invoke_token(
            &mut state,
            Path::new("."),
            &malformed,
            DEFAULT_ALIAS,
            AppLanguage::Source,
        )
        .is_err());
    }
}
