use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Media::Audio::*;
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};
#[cfg(not(test))]
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::*;

pub(crate) const QUICK_LAUNCH_INDEX_ROOT: &str =
    "%APPDATA%\\Microsoft\\Internet Explorer\\Quick Launch";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileIoImportance {
    Required,
    Optional,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileIoFailureChoice {
    Retry,
    Ignore,
}

#[cfg(not(test))]
thread_local! {
    static FILE_IO_DIALOG_IMPORTANCE: Cell<Option<FileIoImportance>> = const { Cell::new(None) };
}

#[cfg(not(test))]
unsafe extern "system" fn file_io_dialog_hook(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code == HCBT_ACTIVATE as i32 {
        let dialog = wparam as HWND;
        FILE_IO_DIALOG_IMPORTANCE.with(|importance| match importance.get() {
            Some(FileIoImportance::Required) => {
                let button = GetDlgItem(dialog, IDOK);
                if !button.is_null() {
                    let retry = wide(localized_active("Retry"));
                    SetWindowTextW(button, retry.as_ptr());
                }
            }
            Some(FileIoImportance::Optional) => {
                let retry_button = GetDlgItem(dialog, IDRETRY);
                if !retry_button.is_null() {
                    let retry = wide(localized_active("Retry"));
                    SetWindowTextW(retry_button, retry.as_ptr());
                }
                let ignore_button = GetDlgItem(dialog, IDCANCEL);
                if !ignore_button.is_null() {
                    let ignore = wide(localized_active("Ignore"));
                    SetWindowTextW(ignore_button, ignore.as_ptr());
                }
            }
            None => {}
        });
    }
    CallNextHookEx(null_mut(), code, wparam, lparam)
}

fn retry_file_operation<T>(
    importance: FileIoImportance,
    mut action: impl FnMut() -> io::Result<T>,
    mut choose: impl FnMut(&io::Error) -> FileIoFailureChoice,
) -> Option<T> {
    loop {
        match action() {
            Ok(value) => return Some(value),
            Err(error) => {
                let choice = choose(&error);
                if importance == FileIoImportance::Optional && choice == FileIoFailureChoice::Ignore
                {
                    return None;
                }
            }
        }
    }
}

fn show_file_io_failure_dialog(
    label: &str,
    path: &Path,
    operation: &str,
    error: &io::Error,
    importance: FileIoImportance,
) -> FileIoFailureChoice {
    #[cfg(test)]
    {
        if importance == FileIoImportance::Required {
            panic!(
                "required file operation failed during a headless test: {label} {} {operation}: {error}",
                path.display()
            );
        }
        return FileIoFailureChoice::Ignore;
    }

    #[cfg(not(test))]
    {
        let title = wide(APP_NAME);
        let failure_text = if importance == FileIoImportance::Required {
            localized_active("Required application file access failed.")
        } else {
            localized_active("Optional application file access failed.")
        };
        let operation_text = match operation {
            "read" => localized_active("read"),
            "write" => localized_active("write"),
            "create directory" => localized_active("create directory"),
            "read directory" => localized_active("read directory"),
            _ => operation,
        };
        let content = wide(&format!(
            "{failure_text}\n\n{}: {label}\n{}: {}\n{}: {operation_text}\n{}: {error}",
            localized_active("File"),
            localized_active("Path"),
            path.display(),
            localized_active("Operation"),
            localized_active("Error"),
        ));
        FILE_IO_DIALOG_IMPORTANCE.with(|current| current.set(Some(importance)));
        let hook = unsafe {
            SetWindowsHookExW(
                WH_CBT,
                Some(file_io_dialog_hook),
                null_mut(),
                GetCurrentThreadId(),
            )
        };
        let style = if importance == FileIoImportance::Required {
            MB_ICONERROR | MB_OK
        } else {
            MB_ICONERROR | MB_RETRYCANCEL
        };
        let selected = unsafe { MessageBoxW(null_mut(), content.as_ptr(), title.as_ptr(), style) };
        if !hook.is_null() {
            unsafe {
                UnhookWindowsHookEx(hook);
            }
        }
        FILE_IO_DIALOG_IMPORTANCE.with(|current| current.set(None));
        if importance == FileIoImportance::Optional && selected == IDCANCEL {
            FileIoFailureChoice::Ignore
        } else {
            FileIoFailureChoice::Retry
        }
    }
}

pub(crate) fn required_file_operation<T>(
    label: &str,
    path: &Path,
    operation: &str,
    action: impl FnMut() -> io::Result<T>,
) -> T {
    retry_file_operation(FileIoImportance::Required, action, |error| {
        show_file_io_failure_dialog(label, path, operation, error, FileIoImportance::Required)
    })
    .expect("required file operations cannot be ignored")
}

pub(crate) fn optional_file_operation<T>(
    label: &str,
    path: &Path,
    operation: &str,
    action: impl FnMut() -> io::Result<T>,
) -> Option<T> {
    retry_file_operation(FileIoImportance::Optional, action, |error| {
        show_file_io_failure_dialog(label, path, operation, error, FileIoImportance::Optional)
    })
}

pub(crate) fn read_required_text(label: &str, path: &Path) -> String {
    required_file_operation(label, path, "read", || fs::read_to_string(path))
}

pub(crate) fn read_optional_text(label: &str, path: &Path) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(_) => optional_file_operation(label, path, "read", || fs::read_to_string(path)),
    }
}

pub(crate) fn write_required_text(label: &str, path: &Path, content: &str) {
    static REQUIRED_FILE_WRITE_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
        std::sync::OnceLock::new();
    let _guard = REQUIRED_FILE_WRITE_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    required_file_operation(label, path, "write", || atomic_write_text(path, content));
}

pub(crate) fn write_optional_text(label: &str, path: &Path, content: &str) -> bool {
    optional_file_operation(label, path, "write", || atomic_write_text(path, content)).is_some()
}

pub(crate) fn ensure_required_text_file(
    label: &str,
    path: &Path,
    default_content: &str,
    hydrate: impl Fn(&str, &str) -> String,
) -> String {
    let mut content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            write_required_text(label, path, default_content);
            read_required_text(label, path)
        }
        Err(_) => read_required_text(label, path),
    };
    let hydrated = hydrate(&content, default_content);
    if hydrated != content {
        write_required_text(label, path, &hydrated);
        content = read_required_text(label, path);
    }
    content
}

pub(crate) fn ensure_optional_text_file(
    label: &str,
    path: &Path,
    default_content: &str,
    hydrate: impl Fn(&str, &str) -> String,
) -> Option<String> {
    let mut content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !write_optional_text(label, path, default_content) {
                return None;
            }
            read_optional_text(label, path)?
        }
        Err(_) => read_optional_text(label, path)?,
    };
    let hydrated = hydrate(&content, default_content);
    if hydrated != content {
        if !write_optional_text(label, path, &hydrated) {
            return None;
        }
        content = read_optional_text(label, path)?;
    }
    Some(content)
}

pub(crate) fn append_missing_default_lines(
    content: &str,
    default_content: &str,
    mut identity: impl FnMut(&str) -> Option<String>,
) -> String {
    if content.is_empty() {
        return default_content.to_string();
    }
    let mut present = content
        .lines()
        .filter_map(&mut identity)
        .collect::<HashSet<_>>();
    let missing = default_content
        .lines()
        .filter_map(|line| identity(line).map(|key| (key, line)))
        .filter_map(|(key, line)| present.insert(key).then_some(line))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return content.to_string();
    }
    let newline = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let keep_final_newline = content.ends_with(['\r', '\n']);
    let mut output = content.to_string();
    if !output.ends_with(['\r', '\n']) {
        output.push_str(newline);
    }
    let missing_count = missing.len();
    for (index, line) in missing.into_iter().enumerate() {
        output.push_str(line);
        if index + 1 < missing_count || keep_final_newline {
            output.push_str(newline);
        }
    }
    output
}

#[derive(Clone)]
pub(crate) struct PendingConfigWrite {
    pub(crate) label: &'static str,
    pub(crate) path: PathBuf,
    pub(crate) content: String,
}

#[derive(Debug)]
pub(crate) struct ConfigTransactionError {
    label: String,
    operation: &'static str,
    source: io::Error,
    rollback_status: Option<String>,
}

impl fmt::Display for ConfigTransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {} failed: {}",
            self.label, self.operation, self.source
        )?;
        if let Some(status) = &self.rollback_status {
            write!(formatter, " ({status})")?;
        }
        Ok(())
    }
}

impl ConfigTransactionError {
    fn file_importance(&self) -> FileIoImportance {
        match self.label.as_str() {
            "settings.ini" | "scoring.ini" | "index_folders.txt" => FileIoImportance::Required,
            _ => FileIoImportance::Optional,
        }
    }
}

struct StagedConfigWrite {
    label: &'static str,
    path: PathBuf,
    stage_path: PathBuf,
    backup_path: PathBuf,
    existed: bool,
}

#[derive(Clone)]
pub(crate) struct AppSettingsSnapshot {
    pub(crate) result_limit: usize,
    pub(crate) search_threads: SearchThreadMode,
    pub(crate) tooltip_opacity_percent: u8,
    pub(crate) show_cpu_in_title: bool,
    pub(crate) show_ram_in_title: bool,
    pub(crate) show_build_timestamp_in_title: bool,
    pub(crate) autostart: bool,
    pub(crate) help_font_size: i32,
    pub(crate) language: AppLanguage,
    pub(crate) popup_sound: bool,
    pub(crate) show_score_breakdown: bool,
    pub(crate) show_score_breakdown_tooltip: bool,
    pub(crate) hotkey: Hotkey,
    pub(crate) window: WindowSettings,
}

pub(crate) fn default_settings_text() -> String {
    let defaults = AppSettingsSnapshot::first_run_defaults_with_hotkey(default_hotkey());
    let bool_value = |value| if value { "1" } else { "0" };
    let lines = [
        format!("hotkey={}", defaults.hotkey.display),
        format!("result_limit={}", defaults.result_limit),
        format!("search_threads={}", defaults.search_threads.setting_value()),
        format!(
            "tooltip_opacity_percent={}",
            defaults.tooltip_opacity_percent
        ),
        format!(
            "show_cpu_in_title={}",
            bool_value(defaults.show_cpu_in_title)
        ),
        format!(
            "show_ram_in_title={}",
            bool_value(defaults.show_ram_in_title)
        ),
        format!(
            "show_build_timestamp_in_title={}",
            bool_value(defaults.show_build_timestamp_in_title)
        ),
        format!("autostart={}", bool_value(defaults.autostart)),
        format!("help_font_size={}", defaults.help_font_size),
        format!("language={}", defaults.language.setting_value()),
        format!("popup_sound={}", bool_value(defaults.popup_sound)),
        format!(
            "show_score_breakdown={}",
            bool_value(defaults.show_score_breakdown)
        ),
        format!(
            "show_score_breakdown_tooltip={}",
            bool_value(defaults.show_score_breakdown_tooltip)
        ),
        format!("x={}", defaults.window.x),
        format!("y={}", defaults.window.y),
        format!("width={}", defaults.window.width),
        format!("height={}", defaults.window.height),
        format!("settings_x={CW_USEDEFAULT}"),
        format!("settings_y={CW_USEDEFAULT}"),
        "settings_width=760".to_string(),
        "settings_height=560".to_string(),
        format!("add_index_x={CW_USEDEFAULT}"),
        format!("add_index_y={CW_USEDEFAULT}"),
        format!("add_index_width={ADD_WIDTH}"),
        format!("add_index_height={ADD_WINDOW_DEFAULT_HEIGHT}"),
        format!("help_x={CW_USEDEFAULT}"),
        format!("help_y={CW_USEDEFAULT}"),
        "help_width=860".to_string(),
        "help_height=620".to_string(),
    ];
    let mut text = lines.join("\r\n");
    text.push_str("\r\n");
    text
}

fn settings_line_identity(line: &str) -> Option<String> {
    let (raw_key, raw_value) = line.trim().trim_start_matches('\u{feff}').split_once('=')?;
    let key = raw_key.trim();
    let value = raw_value.trim();
    let (canonical, valid) = match key {
        "result_limit" => ("result_limit", value.parse::<usize>().is_ok()),
        "search_threads" => (
            "search_threads",
            value == "auto" || value == "max" || value.parse::<usize>().is_ok(),
        ),
        "tooltip_opacity_percent" => ("tooltip_opacity_percent", value.parse::<u8>().is_ok()),
        "help_font_size" => ("help_font_size", value.parse::<i32>().is_ok()),
        "language" => ("language", AppLanguage::try_from_setting(value).is_some()),
        "popup_sound" => ("popup_sound", matches!(value, "0" | "1")),
        "show_cpu_in_title"
        | "show_ram_in_title"
        | "show_build_timestamp_in_title"
        | "autostart"
        | "show_score_breakdown"
        | "show_score_breakdown_tooltip" => (key, matches!(value, "0" | "1")),
        "hotkey" => ("hotkey", parse_hotkey(value).is_some()),
        "x" | "y" | "width" | "height" | "settings_x" | "settings_y" | "settings_width"
        | "settings_height" | "add_index_x" | "add_index_y" | "add_index_width"
        | "add_index_height" | "help_x" | "help_y" | "help_width" | "help_height" => {
            (key, value.parse::<i32>().is_ok())
        }
        _ => return None,
    };
    valid.then(|| canonical.to_string())
}

pub(crate) fn hydrate_settings_text(content: &str, defaults: &str) -> String {
    append_missing_default_lines(content, defaults, settings_line_identity)
}

fn read_hydrated_settings_text() -> String {
    let path = settings_path();
    let defaults = default_settings_text();
    ensure_required_text_file("settings.ini", &path, &defaults, hydrate_settings_text)
}

pub(crate) fn load_app_settings_snapshot() -> AppSettingsSnapshot {
    let content = read_hydrated_settings_text();
    parse_app_settings_snapshot(&content)
}

fn setting_value<'a>(content: &'a str, canonical_key: &str) -> Option<&'a str> {
    content
        .lines()
        .filter_map(|line| {
            (settings_line_identity(line).as_deref() == Some(canonical_key))
                .then(|| line.split_once('=').map(|(_, value)| value.trim()))
                .flatten()
        })
        .last()
}

fn required_setting_value<'a>(content: &'a str, canonical_key: &str) -> &'a str {
    setting_value(content, canonical_key)
        .unwrap_or_else(|| panic!("hydrated settings are missing {canonical_key}"))
}

fn required_bool_setting(content: &str, canonical_key: &str) -> bool {
    parse_bool_setting(required_setting_value(content, canonical_key))
        .expect("hydrated boolean settings must be valid")
}

fn required_i32_setting(content: &str, canonical_key: &str) -> i32 {
    required_setting_value(content, canonical_key)
        .parse::<i32>()
        .expect("hydrated integer settings must be valid")
}

fn parse_app_settings_snapshot(content: &str) -> AppSettingsSnapshot {
    AppSettingsSnapshot {
        result_limit: required_setting_value(content, "result_limit")
            .parse::<usize>()
            .expect("hydrated result limit must be valid")
            .max(MIN_RESULT_LIMIT),
        search_threads: parse_search_threads(required_setting_value(content, "search_threads")),
        tooltip_opacity_percent: required_setting_value(content, "tooltip_opacity_percent")
            .parse::<u8>()
            .expect("hydrated tooltip opacity must be valid")
            .clamp(MIN_TOOLTIP_OPACITY_PERCENT, MAX_TOOLTIP_OPACITY_PERCENT),
        show_cpu_in_title: required_bool_setting(content, "show_cpu_in_title"),
        show_ram_in_title: required_bool_setting(content, "show_ram_in_title"),
        show_build_timestamp_in_title: required_bool_setting(content, "show_build_timestamp_in_title"),
        autostart: required_bool_setting(content, "autostart"),
        help_font_size: required_i32_setting(content, "help_font_size")
            .clamp(MIN_HELP_FONT_SIZE, MAX_HELP_FONT_SIZE),
        language: AppLanguage::try_from_setting(required_setting_value(content, "language"))
            .unwrap_or_else(default_language),
        popup_sound: required_bool_setting(content, "popup_sound"),
        show_score_breakdown: required_bool_setting(content, "show_score_breakdown"),
        show_score_breakdown_tooltip: required_bool_setting(
            content,
            "show_score_breakdown_tooltip",
        ),
        hotkey: parse_hotkey(required_setting_value(content, "hotkey"))
            .expect("hydrated hotkey must be valid"),
        window: WindowSettings {
            x: required_i32_setting(content, "x"),
            y: required_i32_setting(content, "y"),
            width: required_i32_setting(content, "width").max(MIN_WIDTH),
            height: required_i32_setting(content, "height").max(MIN_HEIGHT),
        },
    }
}

impl AppSettingsSnapshot {
    pub(crate) fn first_run_defaults_with_hotkey(hotkey: Hotkey) -> Self {
        Self {
            result_limit: DEFAULT_RESULT_LIMIT,
            search_threads: SearchThreadMode::Auto,
            tooltip_opacity_percent: DEFAULT_TOOLTIP_OPACITY_PERCENT,
            show_cpu_in_title: false,
            show_ram_in_title: false,
            show_build_timestamp_in_title: false,
            autostart: true,
            help_font_size: DEFAULT_HELP_FONT_SIZE,
            language: default_language(),
            popup_sound: true,
            show_score_breakdown: false,
            show_score_breakdown_tooltip: false,
            hotkey,
            window: WindowSettings {
                x: CW_USEDEFAULT,
                y: CW_USEDEFAULT,
                width: 760,
                height: 480,
            },
        }
    }
}

pub(crate) fn app_config_dir() -> PathBuf {
    app_dir().join("CONFIG")
}

pub(crate) fn app_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn transactional_write_text_files(
    files: Vec<PendingConfigWrite>,
) -> Result<(), ConfigTransactionError> {
    transactional_write_text_files_inner(files, None)
}

pub(crate) fn transactional_write_text_files_with_prompts(mut files: Vec<PendingConfigWrite>) {
    loop {
        match transactional_write_text_files(files.clone()) {
            Ok(()) => {
                for file in &files {
                    if matches!(
                        file.label,
                        "settings.ini" | "scoring.ini" | "index_folders.txt"
                    ) {
                        drop(read_required_text(file.label, &file.path));
                    }
                }
                return;
            }
            Err(error) => {
                let importance = if error.operation == "prepare transaction directory"
                    && files.iter().any(|file| {
                        matches!(
                            file.label,
                            "settings.ini" | "scoring.ini" | "index_folders.txt"
                        )
                    }) {
                    FileIoImportance::Required
                } else {
                    error.file_importance()
                };
                let path = files
                    .iter()
                    .find(|file| file.label == error.label)
                    .map(|file| file.path.as_path())
                    .unwrap_or_else(|| Path::new("."));
                let choice = show_file_io_failure_dialog(
                    &error.label,
                    path,
                    error.operation,
                    &error.source,
                    importance,
                );
                if importance == FileIoImportance::Optional && choice == FileIoFailureChoice::Ignore
                {
                    files.retain(|file| file.label != error.label);
                    if files.is_empty() {
                        return;
                    }
                }
            }
        }
    }
}

fn transactional_write_text_files_inner(
    files: Vec<PendingConfigWrite>,
    fail_replace_label: Option<&str>,
) -> Result<(), ConfigTransactionError> {
    if files.is_empty() {
        return Ok(());
    }

    let transaction_dir = match create_transaction_dir(transaction_base_dir(&files)) {
        Ok(path) => path,
        Err(source) => {
            return Err(ConfigTransactionError {
                label: files[0].label.to_string(),
                operation: "prepare transaction directory",
                source,
                rollback_status: None,
            })
        }
    };

    let result = transactional_write_text_files_in_dir(files, &transaction_dir, fail_replace_label);
    if result.is_ok() {
        let _ = fs::remove_dir_all(&transaction_dir);
    }
    result
}

fn transactional_write_text_files_in_dir(
    files: Vec<PendingConfigWrite>,
    transaction_dir: &Path,
    fail_replace_label: Option<&str>,
) -> Result<(), ConfigTransactionError> {
    let mut staged = Vec::new();
    for (index, file) in files.into_iter().enumerate() {
        if let Some(parent) = file.path.parent() {
            if let Err(source) = fs::create_dir_all(parent) {
                cleanup_transaction_dir(transaction_dir, false);
                return Err(ConfigTransactionError {
                    label: file.label.to_string(),
                    operation: "prepare target directory",
                    source,
                    rollback_status: None,
                });
            }
        }

        let safe_label = safe_transaction_name(file.label);
        let stage_path = transaction_dir.join(format!("{index:02}.{safe_label}.stage"));
        let backup_path = transaction_dir.join(format!("{index:02}.{safe_label}.backup"));
        if let Err(source) = write_new_text_file(&stage_path, &file.content) {
            cleanup_transaction_dir(transaction_dir, false);
            return Err(ConfigTransactionError {
                label: file.label.to_string(),
                operation: "stage write",
                source,
                rollback_status: None,
            });
        }

        staged.push(StagedConfigWrite {
            label: file.label,
            path: file.path,
            stage_path,
            backup_path,
            existed: false,
        });
    }

    for item in &mut staged {
        match fs::metadata(&item.path) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    cleanup_transaction_dir(transaction_dir, false);
                    return Err(ConfigTransactionError {
                        label: item.label.to_string(),
                        operation: "backup",
                        source: io::Error::new(
                            io::ErrorKind::IsADirectory,
                            "target is a directory",
                        ),
                        rollback_status: None,
                    });
                }
                if let Err(source) = fs::copy(&item.path, &item.backup_path) {
                    cleanup_transaction_dir(transaction_dir, false);
                    return Err(ConfigTransactionError {
                        label: item.label.to_string(),
                        operation: "backup",
                        source,
                        rollback_status: None,
                    });
                }
                item.existed = true;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                cleanup_transaction_dir(transaction_dir, false);
                return Err(ConfigTransactionError {
                    label: item.label.to_string(),
                    operation: "inspect target",
                    source,
                    rollback_status: None,
                });
            }
        }
    }

    let mut committed: Vec<&StagedConfigWrite> = Vec::new();
    for item in &staged {
        let replace_result = if fail_replace_label == Some(item.label) {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected replace failure",
            ))
        } else {
            move_file_replace(&item.stage_path, &item.path)
        };

        match replace_result {
            Ok(()) => committed.push(item),
            Err(source) => {
                let rollback_status = rollback_committed_files(&committed);
                let keep_transaction_dir = rollback_status.as_ref().is_some_and(|status| {
                    status.starts_with("rollback failed")
                        || status.starts_with("rollback incomplete")
                });
                cleanup_transaction_dir(transaction_dir, keep_transaction_dir);
                return Err(ConfigTransactionError {
                    label: item.label.to_string(),
                    operation: "replace",
                    source,
                    rollback_status: Some(
                        rollback_status.unwrap_or_else(|| "rollback completed".to_string()),
                    ),
                });
            }
        }
    }

    Ok(())
}

fn transaction_base_dir(files: &[PendingConfigWrite]) -> PathBuf {
    files
        .first()
        .and_then(|file| file.path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn create_transaction_dir(base_dir: PathBuf) -> io::Result<PathBuf> {
    fs::create_dir_all(&base_dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let mut last_error = None;
    for attempt in 0..100u32 {
        let path = base_dir.join(format!(
            ".settings-save-transaction.{}.{}.{}",
            std::process::id(),
            stamp,
            attempt
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create transaction directory",
        )
    }))
}

fn safe_transaction_name(label: &str) -> String {
    let safe: String = label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "file".to_string()
    } else {
        safe
    }
}

fn write_new_text_file(path: &Path, content: &str) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn move_file_replace(source: &Path, destination: &Path) -> io::Result<()> {
    let source = os_wide(source.as_os_str());
    let destination = os_wide(destination.as_os_str());
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } != 0;
    if moved {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn rollback_committed_files(committed: &[&StagedConfigWrite]) -> Option<String> {
    if committed.is_empty() {
        return Some("rollback completed".to_string());
    }

    let mut errors = Vec::new();
    for item in committed.iter().rev() {
        let result = if item.existed {
            move_file_replace(&item.backup_path, &item.path)
        } else {
            match fs::remove_file(&item.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        };
        if let Err(error) = result {
            errors.push(format!("{}: {}", item.label, error));
        }
    }

    if errors.is_empty() {
        Some("rollback completed".to_string())
    } else {
        Some(format!("rollback failed: {}", errors.join("; ")))
    }
}

fn cleanup_transaction_dir(transaction_dir: &Path, keep: bool) {
    if !keep {
        let _ = fs::remove_dir_all(transaction_dir);
    }
}

pub(crate) fn atomic_write_text(path: &Path, content: &str) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| "file".into());
    let mut last_error = None;
    for attempt in 0..100u32 {
        let temp_path = parent.join(format!(
            ".{file_name}.tmp.{}.{}",
            std::process::id(),
            attempt
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(mut file) => {
                if let Err(error) = file.write_all(content.as_bytes()) {
                    let _ = fs::remove_file(&temp_path);
                    return Err(error);
                }
                if let Err(error) = file.sync_all() {
                    let _ = fs::remove_file(&temp_path);
                    return Err(error);
                }
                drop(file);
                let source = os_wide(temp_path.as_os_str());
                let destination = os_wide(path.as_os_str());
                let moved = unsafe {
                    MoveFileExW(
                        source.as_ptr(),
                        destination.as_ptr(),
                        MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                    )
                } != 0;
                if !moved {
                    let error = io::Error::last_os_error();
                    let _ = fs::remove_file(&temp_path);
                    return Err(error);
                }
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AlreadyExists, "could not create temp file")
    }))
}

pub(crate) fn default_index_text() -> String {
    let lines = vec![
        "# path | enabled=1 | score=100 | depth=-1 | keywords=optional".to_string(),
        "%MYSTARTMENU% | enabled=1 | score=100 | depth=-1".to_string(),
        "%COMMONSTARTMENU% | enabled=1 | score=100 | depth=-1".to_string(),
        "%MYRECENTDOCS% | enabled=1 | score=200 | depth=-1".to_string(),
        "%MYDESKTOP% | enabled=1 | score=75 | depth=-1".to_string(),
        "%MYDOCUMENTS% | enabled=1 | score=50 | depth=-1".to_string(),
        format!("{QUICK_LAUNCH_INDEX_ROOT} | enabled=1 | score=75 | depth=-1"),
        "%ALLDESKTOP% | enabled=1 | score=50 | depth=-1".to_string(),
        "%MYPICTURES% | enabled=1 | score=0 | depth=-1".to_string(),
        "%MYMUSIC% | enabled=1 | score=0 | depth=-1".to_string(),
        "%MYVIDEO% | enabled=1 | score=0 | depth=-1".to_string(),
        "%PROGRAMFILES86% | enabled=0 | score=60 | depth=-1".to_string(),
        "%PROGRAMFILES% | enabled=0 | score=60 | depth=-1".to_string(),
        "%USERPROFILE% | enabled=0 | score=0 | depth=-1".to_string(),
        "%MYFAVORITES% | enabled=0 | score=0 | depth=-1".to_string(),
        "# Add your folders below, for example:".to_string(),
        "# D:\\Apps | score=100 | depth=-1 | keywords=tools".to_string(),
    ];
    let mut text = lines.join("\r\n");
    text.push_str("\r\n");
    text
}

fn index_line_identity(line: &str) -> Option<String> {
    parse_index_root_line(line).map(|root| fold_text(root.raw.trim()))
}

pub(crate) fn hydrate_index_text(content: &str, defaults: &str) -> String {
    if content.is_empty() {
        return defaults.to_string();
    }
    let default_roots = defaults
        .lines()
        .filter_map(parse_index_root_line)
        .map(|root| (fold_text(&root.raw), root))
        .collect::<HashMap<_, _>>();
    let mut changed = false;
    let mut lines = content
        .lines()
        .map(|line| {
            let line = line.to_string();
            let Some(parsed) = parse_index_root_line(&line) else {
                return line;
            };
            let configured_default = default_roots.get(&fold_text(&parsed.raw));
            let parts = split_index_root_line(line.trim().trim_start_matches('\u{feff}'));
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
            let mut output = line;
            if !has_enabled {
                let value = configured_default.map(|root| root.enabled).unwrap_or(true);
                output.push_str(&format!(" | enabled={}", if value { 1 } else { 0 }));
                changed = true;
            }
            if !has_score {
                let value = configured_default.map(|root| root.score).unwrap_or(100);
                output.push_str(&format!(" | score={value}"));
                changed = true;
            }
            if !has_depth {
                let value = configured_default
                    .map(|root| root.max_depth)
                    .unwrap_or(DEFAULT_SEARCH_DEPTH);
                output.push_str(&format!(" | depth={}", depth_display(value)));
                changed = true;
            }
            output
        })
        .collect::<Vec<_>>();
    let mut present = lines
        .iter()
        .filter_map(|line| index_line_identity(line))
        .collect::<HashSet<_>>();
    for line in defaults.lines() {
        let Some(identity) = index_line_identity(line) else {
            continue;
        };
        if present.insert(identity) {
            lines.push(line.to_string());
            changed = true;
        }
    }
    if !changed {
        return content.to_string();
    }
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

pub(crate) fn ensure_index_file(_owner: Option<HWND>) {
    let path = index_path();
    let defaults = default_index_text();
    drop(ensure_required_text_file(
        "index_folders.txt",
        &path,
        &defaults,
        hydrate_index_text,
    ));
}

pub(crate) fn parse_result_limit(value: &str) -> usize {
    value
        .trim()
        .parse::<usize>()
        .ok()
        .unwrap_or(DEFAULT_RESULT_LIMIT)
        .max(MIN_RESULT_LIMIT)
}

pub(crate) fn parse_search_threads(value: &str) -> SearchThreadMode {
    let value = value.trim();
    if value == "auto" {
        return SearchThreadMode::Auto;
    }
    if value == "max" {
        return SearchThreadMode::Maximum;
    }
    value
        .parse::<usize>()
        .ok()
        .map(|threads| {
            SearchThreadMode::Manual(threads.clamp(MIN_SEARCH_THREADS, available_search_threads()))
        })
        .unwrap_or(SearchThreadMode::Auto)
}

#[cfg(feature = "debug-tools")]
pub(crate) fn load_result_limit() -> usize {
    load_app_settings_snapshot().result_limit
}

pub(crate) fn parse_tooltip_opacity_percent(value: &str) -> u8 {
    value
        .trim()
        .parse::<u8>()
        .ok()
        .unwrap_or(DEFAULT_TOOLTIP_OPACITY_PERCENT)
        .clamp(MIN_TOOLTIP_OPACITY_PERCENT, MAX_TOOLTIP_OPACITY_PERCENT)
}

pub(crate) fn parse_help_font_size(value: &str) -> i32 {
    value
        .trim()
        .parse::<i32>()
        .ok()
        .unwrap_or(DEFAULT_HELP_FONT_SIZE)
        .clamp(MIN_HELP_FONT_SIZE, MAX_HELP_FONT_SIZE)
}

pub(crate) fn load_help_font_size() -> i32 {
    load_app_settings_snapshot().help_font_size
}

pub(crate) fn save_help_font_size(value: i32) -> io::Result<()> {
    update_setting_line(
        "help_font_size",
        &value
            .clamp(MIN_HELP_FONT_SIZE, MAX_HELP_FONT_SIZE)
            .to_string(),
    )
}

pub(crate) fn play_popup_sound(enabled: bool) {
    if !enabled {
        return;
    }
    let sound_path = popup_sound_path();
    optional_file_operation("fping.wav", &sound_path, "read and play", || {
        let metadata = fs::metadata(&sound_path)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sound path is not a file",
            ));
        }
        let wide_path = os_wide(sound_path.as_os_str());
        let succeeded = unsafe {
            PlaySoundW(
                wide_path.as_ptr(),
                null_mut(),
                SND_FILENAME | SND_ASYNC | SND_NODEFAULT,
            ) != 0
        };
        if succeeded {
            Ok(())
        } else {
            Err(io::Error::other(
                "Windows could not read or play the sound file",
            ))
        }
    });
}

pub(crate) fn update_setting_line(key_name: &str, value: &str) -> io::Result<()> {
    update_setting_lines(&[(key_name, value.to_string())])
}

pub(crate) fn read_settings_text_with_updates(updates: &[(&str, String)]) -> io::Result<String> {
    let content = read_hydrated_settings_text();
    Ok(render_settings_text_with_updates(&content, updates))
}

pub(crate) fn render_settings_text_with_updates(
    content: &str,
    updates: &[(&str, String)],
) -> String {
    let mut found = vec![false; updates.len()];
    let mut lines = Vec::new();

    for line in content.lines() {
        let canonical_key = settings_line_identity(line);
        if let Some(index) = updates
            .iter()
            .position(|(key_name, _)| canonical_key.as_deref() == Some(*key_name))
        {
            if !found[index] {
                let (key_name, value) = &updates[index];
                lines.push(format!("{key_name}={value}"));
                found[index] = true;
            }
        } else {
            lines.push(line.to_string());
        }
    }

    for (index, (key_name, value)) in updates.iter().enumerate() {
        if !found[index] {
            lines.push(format!("{key_name}={value}"));
        }
    }

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

fn join_lines_like(content: &str, lines: &[String]) -> String {
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

pub(crate) fn update_setting_lines(updates: &[(&str, String)]) -> io::Result<()> {
    let path = settings_path();
    let output = read_settings_text_with_updates(updates)?;
    write_required_text("settings.ini", &path, &output);
    Ok(())
}

pub(crate) fn default_hotkey() -> Hotkey {
    Hotkey {
        modifiers: 0,
        key: VK_PAUSE as u32,
        display: "Pause".to_string(),
    }
}

pub(crate) fn parse_hotkey(value: &str) -> Option<Hotkey> {
    let parts: Vec<&str> = value
        .split(['+', '-'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }

    let mut modifiers = 0u32;
    let mut key = None;
    let mut display_parts = Vec::new();

    for part in parts {
        match fold_text(part).as_str() {
            "ctrl" | "control" => {
                modifiers |= MOD_CONTROL;
                display_parts.push("Ctrl".to_string());
            }
            "alt" => {
                modifiers |= MOD_ALT;
                display_parts.push("Alt".to_string());
            }
            "shift" => {
                modifiers |= MOD_SHIFT;
                display_parts.push("Shift".to_string());
            }
            "win" | "windows" | "super" => {
                modifiers |= MOD_WIN;
                display_parts.push("Win".to_string());
            }
            other => {
                if key.is_some() {
                    return None;
                }
                let (parsed_key, display) = parse_hotkey_key(other)?;
                key = Some(parsed_key);
                display_parts.push(display);
            }
        }
    }

    let key = key?;

    Some(Hotkey {
        modifiers,
        key,
        display: display_parts.join("+"),
    })
}

pub(crate) fn parse_hotkey_key(value: &str) -> Option<(u32, String)> {
    match value {
        "pause" | "break" => Some((VK_PAUSE as u32, "Pause".to_string())),
        "space" => Some((VK_SPACE as u32, "Space".to_string())),
        "enter" | "return" => Some((VK_RETURN as u32, "Enter".to_string())),
        "tab" => Some((VK_TAB as u32, "Tab".to_string())),
        "escape" | "esc" => Some((VK_ESCAPE as u32, "Esc".to_string())),
        value if value.len() == 1 => {
            let ch = value.chars().next()?.to_ascii_uppercase();
            if ch.is_ascii_alphanumeric() {
                Some((ch as u32, ch.to_string()))
            } else {
                None
            }
        }
        value if value.starts_with('f') => {
            let number = value[1..].parse::<u32>().ok()?;
            if (1..=24).contains(&number) {
                Some((VK_F1 as u32 + number - 1, format!("F{number}")))
            } else {
                None
            }
        }
        _ => None,
    }
}

pub(crate) fn key_down(key: u16) -> bool {
    unsafe { (GetKeyState(key as i32) as u16 & 0x8000) != 0 }
}

pub(crate) fn hotkey_key_display(key: u16) -> String {
    match key {
        VK_PAUSE => "Pause".to_string(),
        VK_SPACE => "Space".to_string(),
        VK_RETURN => "Enter".to_string(),
        VK_TAB => "Tab".to_string(),
        VK_ESCAPE => "Esc".to_string(),
        key if (VK_F1..=VK_F1 + 23).contains(&key) => format!("F{}", key - VK_F1 + 1),
        key if (b'0' as u16..=b'9' as u16).contains(&key)
            || (b'A' as u16..=b'Z' as u16).contains(&key) =>
        {
            (key as u8 as char).to_string()
        }
        _ => format!("VK{}", key),
    }
}

pub(crate) fn load_config_window_settings() -> WindowSettings {
    let content = read_hydrated_settings_text();
    WindowSettings {
        x: required_i32_setting(&content, "settings_x"),
        y: required_i32_setting(&content, "settings_y"),
        width: required_i32_setting(&content, "settings_width").max(SETTINGS_MIN_WIDTH),
        height: required_i32_setting(&content, "settings_height").max(SETTINGS_MIN_HEIGHT),
    }
}

pub(crate) fn load_add_index_window_settings() -> WindowSettings {
    let content = read_hydrated_settings_text();
    WindowSettings {
        x: required_i32_setting(&content, "add_index_x"),
        y: required_i32_setting(&content, "add_index_y"),
        width: required_i32_setting(&content, "add_index_width").max(ADD_MIN_WIDTH),
        height: required_i32_setting(&content, "add_index_height").max(ADD_MIN_HEIGHT),
    }
}

pub(crate) fn load_help_window_settings() -> WindowSettings {
    let content = read_hydrated_settings_text();
    WindowSettings {
        x: required_i32_setting(&content, "help_x"),
        y: required_i32_setting(&content, "help_y"),
        width: required_i32_setting(&content, "help_width").max(640),
        height: required_i32_setting(&content, "help_height").max(420),
    }
}

pub(crate) fn window_style_no_minimize() -> u32 {
    WS_OVERLAPPEDWINDOW & !WS_MINIMIZEBOX
}

pub(crate) fn save_window_settings(hwnd: HWND) {
    unsafe {
        if hwnd.is_null() || IsIconic(hwnd) != 0 {
            return;
        }

        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return;
        }

        let path = settings_path();
        let existing = read_hydrated_settings_text();
        let mut lines: Vec<String> = existing
            .lines()
            .filter(|line| {
                let key = line
                    .split_once('=')
                    .map(|(key, _)| key.trim())
                    .unwrap_or("");
                !matches!(key, "x" | "y" | "width" | "height")
            })
            .map(str::to_string)
            .collect();

        lines.push(format!("x={}", rect.left));
        lines.push(format!("y={}", rect.top));
        lines.push(format!("width={}", rect.right - rect.left));
        lines.push(format!("height={}", rect.bottom - rect.top));

        let content = join_lines_like(&existing, &lines);
        write_required_text("settings.ini", &path, &content);
    }
}

pub(crate) fn save_config_window_settings(hwnd: HWND) {
    unsafe {
        if hwnd.is_null() || IsIconic(hwnd) != 0 {
            return;
        }

        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return;
        }

        let path = settings_path();
        let existing = read_hydrated_settings_text();
        let mut lines: Vec<String> = existing
            .lines()
            .filter(|line| {
                let key = line
                    .split_once('=')
                    .map(|(key, _)| key.trim())
                    .unwrap_or("");
                !matches!(
                    key,
                    "settings_x" | "settings_y" | "settings_width" | "settings_height"
                )
            })
            .map(str::to_string)
            .collect();

        lines.push(format!("settings_x={}", rect.left));
        lines.push(format!("settings_y={}", rect.top));
        lines.push(format!("settings_width={}", rect.right - rect.left));
        lines.push(format!("settings_height={}", rect.bottom - rect.top));

        let content = join_lines_like(&existing, &lines);
        write_required_text("settings.ini", &path, &content);
    }
}

pub(crate) fn save_add_index_window_settings(hwnd: HWND) {
    save_prefixed_window_settings(
        hwnd,
        &[
            "add_index_x",
            "add_index_y",
            "add_index_width",
            "add_index_height",
        ],
        |rect| {
            vec![
                format!("add_index_x={}", rect.left),
                format!("add_index_y={}", rect.top),
                format!("add_index_width={}", rect.right - rect.left),
                format!("add_index_height={}", rect.bottom - rect.top),
            ]
        },
    );
}

pub(crate) fn save_help_window_settings(hwnd: HWND) {
    save_prefixed_window_settings(
        hwnd,
        &["help_x", "help_y", "help_width", "help_height"],
        |rect| {
            vec![
                format!("help_x={}", rect.left),
                format!("help_y={}", rect.top),
                format!("help_width={}", rect.right - rect.left),
                format!("help_height={}", rect.bottom - rect.top),
            ]
        },
    );
}

pub(crate) fn save_prefixed_window_settings(
    hwnd: HWND,
    keys: &[&str],
    make_lines: impl FnOnce(&RECT) -> Vec<String>,
) {
    unsafe {
        if hwnd.is_null() || IsIconic(hwnd) != 0 {
            return;
        }

        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return;
        }

        let path = settings_path();
        let existing = read_hydrated_settings_text();
        let mut lines: Vec<String> = existing
            .lines()
            .filter(|line| {
                let key = line
                    .split_once('=')
                    .map(|(key, _)| key.trim())
                    .unwrap_or("");
                !keys.contains(&key)
            })
            .map(str::to_string)
            .collect();

        lines.extend(make_lines(&rect));

        let content = join_lines_like(&existing, &lines);
        write_required_text("settings.ini", &path, &content);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config_dir(test_name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("TEMP")
            .join(format!(
                "flashlaunch-{test_name}-{}-{stamp}",
                std::process::id()
            ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn parse_test_app_settings(content: &str) -> AppSettingsSnapshot {
        let defaults = default_settings_text();
        let hydrated = hydrate_settings_text(content, &defaults);
        parse_app_settings_snapshot(&hydrated)
    }

    #[test]
    fn serialized_defaults_match_first_run_snapshot() {
        let expected = AppSettingsSnapshot::first_run_defaults_with_hotkey(default_hotkey());
        let actual = parse_app_settings_snapshot(&default_settings_text());

        assert_eq!(actual.result_limit, expected.result_limit);
        assert_eq!(actual.search_threads, expected.search_threads);
        assert_eq!(
            actual.tooltip_opacity_percent,
            expected.tooltip_opacity_percent
        );
        assert_eq!(actual.show_cpu_in_title, expected.show_cpu_in_title);
        assert_eq!(actual.show_ram_in_title, expected.show_ram_in_title);
        assert_eq!(actual.help_font_size, expected.help_font_size);
        assert_eq!(actual.language, expected.language);
        assert_eq!(actual.popup_sound, expected.popup_sound);
        assert_eq!(actual.show_score_breakdown, expected.show_score_breakdown);
        assert_eq!(
            actual.show_score_breakdown_tooltip,
            expected.show_score_breakdown_tooltip
        );
        assert_eq!(actual.hotkey.display, expected.hotkey.display);
        assert_eq!(actual.window.x, expected.window.x);
        assert_eq!(actual.window.y, expected.window.y);
        assert_eq!(actual.window.width, expected.window.width);
        assert_eq!(actual.window.height, expected.window.height);
    }

    #[test]
    fn app_settings_snapshot_parses_score_breakdown_tooltip() {
        let defaults = parse_test_app_settings("");
        assert!(!defaults.show_score_breakdown);
        assert!(!defaults.show_score_breakdown_tooltip);

        let disabled = parse_test_app_settings(
            "show_score_breakdown_tooltip=0
show_score_breakdown=1
",
        );
        assert!(!disabled.show_score_breakdown_tooltip);
        assert!(disabled.show_score_breakdown);

        let invalid = parse_test_app_settings(
            "show_score_breakdown=invalid
show_score_breakdown_tooltip=1
",
        );
        assert!(!invalid.show_score_breakdown);
        assert!(invalid.show_score_breakdown_tooltip);
    }

    #[test]
    fn parse_tooltip_opacity_percent_clamps_values() {
        assert_eq!(
            parse_tooltip_opacity_percent(""),
            DEFAULT_TOOLTIP_OPACITY_PERCENT
        );
        assert_eq!(
            parse_tooltip_opacity_percent("abc"),
            DEFAULT_TOOLTIP_OPACITY_PERCENT
        );
        assert_eq!(
            parse_tooltip_opacity_percent("10"),
            MIN_TOOLTIP_OPACITY_PERCENT
        );
        assert_eq!(
            parse_tooltip_opacity_percent("120"),
            MAX_TOOLTIP_OPACITY_PERCENT
        );
        assert_eq!(parse_tooltip_opacity_percent("80"), 80);
    }

    #[test]
    fn title_resource_settings_parse_defaults_and_values() {
        let defaults = parse_test_app_settings("");
        assert!(!defaults.show_cpu_in_title);
        assert!(!defaults.show_ram_in_title);

        let configured = parse_test_app_settings("show_cpu_in_title=1\nshow_ram_in_title=1\n");
        assert!(configured.show_cpu_in_title);
        assert!(configured.show_ram_in_title);
    }

    #[test]
    fn parse_search_threads_supports_canonical_modes_and_manual_counts() {
        assert_eq!(parse_search_threads("auto"), SearchThreadMode::Auto);
        assert_eq!(parse_search_threads("max"), SearchThreadMode::Maximum);
        assert_eq!(
            parse_search_threads("0"),
            SearchThreadMode::Manual(MIN_SEARCH_THREADS)
        );
        assert_eq!(
            parse_search_threads("7"),
            SearchThreadMode::Manual(7.min(available_search_threads()))
        );
        assert_eq!(parse_search_threads("abc"), SearchThreadMode::Auto);
    }

    #[test]
    fn transaction_writes_multiple_files() {
        let dir = temp_config_dir("transaction-writes-multiple-files");
        let settings = dir.join("settings.ini");
        let index = dir.join("index_folders.txt");
        fs::write(&settings, "hotkey=Ctrl+Space\n").unwrap();

        transactional_write_text_files(vec![
            PendingConfigWrite {
                label: "index_folders.txt",
                path: index.clone(),
                content: "folder-a\n".to_string(),
            },
            PendingConfigWrite {
                label: "settings.ini",
                path: settings.clone(),
                content: "hotkey=Alt+Space\n".to_string(),
            },
        ])
        .unwrap();

        assert_eq!(fs::read_to_string(settings).unwrap(), "hotkey=Alt+Space\n");
        assert_eq!(fs::read_to_string(index).unwrap(), "folder-a\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn transaction_rolls_back_after_replace_failure() {
        let dir = temp_config_dir("transaction-rolls-back-after-replace-failure");
        let index = dir.join("index_folders.txt");
        let scoring = dir.join("scoring.ini");
        let recent = dir.join("recent_items.txt");
        fs::write(&index, "old-index\n").unwrap();
        fs::write(&scoring, "old-scoring\n").unwrap();
        fs::write(&recent, "old-recent\n").unwrap();

        let error = transactional_write_text_files_inner(
            vec![
                PendingConfigWrite {
                    label: "index_folders.txt",
                    path: index.clone(),
                    content: "new-index\n".to_string(),
                },
                PendingConfigWrite {
                    label: "scoring.ini",
                    path: scoring.clone(),
                    content: "new-scoring\n".to_string(),
                },
                PendingConfigWrite {
                    label: "recent_items.txt",
                    path: recent.clone(),
                    content: "new-recent\n".to_string(),
                },
            ],
            Some("scoring.ini"),
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("scoring.ini"));
        assert!(message.contains("rollback completed"));
        assert_eq!(fs::read_to_string(index).unwrap(), "old-index\n");
        assert_eq!(fs::read_to_string(scoring).unwrap(), "old-scoring\n");
        assert_eq!(fs::read_to_string(recent).unwrap(), "old-recent\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn transaction_removes_new_file_on_rollback() {
        let dir = temp_config_dir("transaction-removes-new-file-on-rollback");
        let recent = dir.join("recent_items.txt");
        let history = dir.join("search_history.txt");
        fs::write(&history, "old-history\n").unwrap();

        let error = transactional_write_text_files_inner(
            vec![
                PendingConfigWrite {
                    label: "recent_items.txt",
                    path: recent.clone(),
                    content: "new-recent\n".to_string(),
                },
                PendingConfigWrite {
                    label: "search_history.txt",
                    path: history.clone(),
                    content: "new-history\n".to_string(),
                },
            ],
            Some("search_history.txt"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("rollback completed"));
        assert!(!recent.exists());
        assert_eq!(fs::read_to_string(history).unwrap(), "old-history\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn render_settings_updates_preserves_unknown_lines() {
        let output = render_settings_text_with_updates(
            "# comment\nhotkey=Ctrl+Space\ncustom=value\n",
            &[
                ("hotkey", "Alt+Space".to_string()),
                ("result_limit", "25".to_string()),
            ],
        );

        assert_eq!(
            output,
            "# comment\nhotkey=Alt+Space\ncustom=value\nresult_limit=25\n"
        );
    }

    #[test]
    fn help_font_size_clamps_to_supported_range() {
        assert_eq!(parse_help_font_size("7"), MIN_HELP_FONT_SIZE);
        assert_eq!(parse_help_font_size("20"), 20);
        assert_eq!(parse_help_font_size("99"), MAX_HELP_FONT_SIZE);
        assert_eq!(parse_help_font_size("bad"), DEFAULT_HELP_FONT_SIZE);
    }

    #[test]
    fn required_file_operation_retries_even_if_prompt_requests_ignore() {
        let attempts = Cell::new(0usize);
        let prompts = Cell::new(0usize);
        let result = retry_file_operation(
            FileIoImportance::Required,
            || {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                if attempt < 3 {
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "locked"))
                } else {
                    Ok(42)
                }
            },
            |_| {
                prompts.set(prompts.get() + 1);
                FileIoFailureChoice::Ignore
            },
        );

        assert_eq!(result, Some(42));
        assert_eq!(attempts.get(), 3);
        assert_eq!(prompts.get(), 2);
    }

    #[test]
    fn optional_file_operation_retries_then_succeeds() {
        let attempts = Cell::new(0usize);
        let prompts = Cell::new(0usize);
        let result = retry_file_operation(
            FileIoImportance::Optional,
            || {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                if attempt == 1 {
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "locked"))
                } else {
                    Ok(7)
                }
            },
            |_| {
                prompts.set(prompts.get() + 1);
                FileIoFailureChoice::Retry
            },
        );

        assert_eq!(result, Some(7));
        assert_eq!(attempts.get(), 2);
        assert_eq!(prompts.get(), 1);
    }

    #[test]
    fn optional_ignore_only_stops_the_current_operation() {
        let attempts = Cell::new(0usize);
        let run = || {
            retry_file_operation(
                FileIoImportance::Optional,
                || {
                    attempts.set(attempts.get() + 1);
                    Err::<(), _>(io::Error::new(io::ErrorKind::PermissionDenied, "locked"))
                },
                |_| FileIoFailureChoice::Ignore,
            )
        };

        assert_eq!(run(), None);
        assert_eq!(run(), None);
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn settings_hydration_preserves_unknown_and_invalid_lines() {
        let content = concat!(
            "# keep\n",
            "custom=value\n",
            "result_limit=bad\n",
            "language=missing\n",
        );
        let defaults = format!(
            "result_limit=9\r\nlanguage={}\r\nshow_cpu_in_title=1\r\n",
            default_language().setting_value()
        );

        let hydrated = hydrate_settings_text(content, &defaults);

        assert!(hydrated.starts_with(content));
        assert!(hydrated.contains("result_limit=bad"));
        assert!(hydrated.contains("result_limit=9"));
        assert!(hydrated.contains("show_cpu_in_title=1"));
        assert_eq!(hydrated.matches("language=").count(), 2);
    }

    #[test]
    fn index_hydration_repairs_fields_and_adds_missing_default_roots() {
        let content = "# keep\nD:\\Apps | custom=value | score=bad\n";
        let defaults = "%MYDOCUMENTS% | enabled=1 | score=50 | depth=-1\r\n";

        let hydrated = hydrate_index_text(content, defaults);

        assert!(hydrated.contains("# keep"));
        assert!(hydrated.contains("custom=value"));
        assert!(hydrated.contains("score=bad"));
        assert!(hydrated.contains("enabled=1"));
        assert!(hydrated.contains("score=100"));
        assert!(hydrated.contains("depth=-1"));
        assert!(hydrated.contains("%MYDOCUMENTS%"));
    }

    #[test]
    fn search_folder_legacy_fields_are_ignored_and_canonical_fields_are_hydrated() {
        let content = concat!(
            "# Keep\r\n",
            "D:\\Legacy | enable=0 | score=9 | max_depth=7 | modifier=tools | label=Old\r\n",
        );
        let defaults = "D:\\Default | enabled=1 | score=50 | depth=-1\r\n";

        let hydrated = hydrate_index_text(content, defaults);
        let legacy = hydrated
            .lines()
            .find(|line| line.starts_with("D:\\Legacy"))
            .and_then(parse_index_root_line)
            .unwrap();

        assert!(legacy.enabled);
        assert_eq!(legacy.score, 9);
        assert_eq!(legacy.max_depth, DEFAULT_SEARCH_DEPTH);
        assert!(legacy.keywords.is_empty());
        assert!(legacy.label.is_empty());
        assert!(hydrated.contains("enabled=1"));
        assert!(hydrated.contains("depth=2"));
        assert!(hydrated.contains("D:\\Default"));
    }

    #[test]
    fn app_settings_accept_only_canonical_keys_and_values() {
        let parsed = parse_test_app_settings(concat!(
            "ui_language=vi\n",
            "locale=vi\n",
            "sound=0\n",
            "show_cpu_in_title=false\n",
            "search_threads=maximum\n",
            "language=en\n",
            "popup_sound=1\n",
        ));

        assert_eq!(parsed.language, default_language());
        assert!(parsed.popup_sound);
        assert!(!parsed.show_cpu_in_title);
        assert_eq!(parsed.search_threads, SearchThreadMode::Auto);
    }

    #[test]
    fn fresh_install_creates_canonical_settings_search_folders_and_scoring_defaults() {
        let dir = temp_config_dir("fresh-config-defaults");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);

        let settings_defaults = default_settings_text();
        let settings_path = dir.join("settings.ini");
        let settings = ensure_required_text_file(
            "settings.ini",
            &settings_path,
            &settings_defaults,
            hydrate_settings_text,
        );
        let snapshot = parse_app_settings_snapshot(&settings);
        assert_eq!(snapshot.result_limit, DEFAULT_RESULT_LIMIT);
        assert!(!settings.contains("query_launch_rules_history_reset"));

        let index_defaults = default_index_text();
        let index_path = dir.join("index_folders.txt");
        let index = ensure_required_text_file(
            "index_folders.txt",
            &index_path,
            &index_defaults,
            hydrate_index_text,
        );
        let roots = index
            .lines()
            .filter_map(parse_index_root_line)
            .collect::<Vec<_>>();
        assert_eq!(roots.len(), default_index_roots().len());

        let scoring_defaults = default_scoring_text();
        let scoring_path = dir.join("scoring.ini");
        let scoring = ensure_required_text_file(
            "scoring.ini",
            &scoring_path,
            &scoring_defaults,
            hydrate_scoring_text,
        );
        let parsed_scoring = parse_scoring_config(&scoring);
        assert_eq!(
            pattern_score_with_config(Path::new(r"C:\Apps\Tool.exe"), &parsed_scoring),
            150
        );
        assert!(!scoring.contains("Executable Shortcut Bonus"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn generic_hydration_preserves_comments_newlines_and_final_newline_state() {
        let content = "# Keep this\ncustom=value";
        let hydrated = append_missing_default_lines(content, "alpha=1\nbeta=2\n", |line| {
            line.split_once('=').map(|(key, _)| key.to_string())
        });

        assert!(hydrated.starts_with(content));
        assert!(hydrated.contains("\nalpha=1\nbeta=2"));
        assert!(!hydrated.contains("\r\n"));
        assert!(!hydrated.ends_with('\n'));
    }

    #[test]
    fn required_missing_file_is_written_and_freshly_read_before_return() {
        let dir = temp_config_dir("required-missing-file-fresh-read");
        let path = dir.join("required.ini");

        let content =
            ensure_required_text_file("required.ini", &path, "alpha=1\n", |content, defaults| {
                append_missing_default_lines(content, defaults, |line| {
                    line.split_once('=').map(|(key, _)| key.to_string())
                })
            });

        assert_eq!(content, "alpha=1\n");
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
        let _ = fs::remove_dir_all(dir);
    }
}
