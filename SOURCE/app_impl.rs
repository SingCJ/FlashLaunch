use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::Diagnostics::Debug::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::ProcessStatus::EmptyWorkingSet;
use windows_sys::Win32::System::SystemServices::*;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::plugins;
use crate::*;

#[derive(Clone, Copy)]
pub(crate) struct SettingsTableView {
    pub(crate) list: HWND,
    pub(crate) label: HWND,
    pub(crate) status: HWND,
    pub(crate) toggle_button: HWND,
    pub(crate) add_button: HWND,
    pub(crate) delete_button: HWND,
    pub(crate) move_up_button: HWND,
    pub(crate) move_down_button: HWND,
    pub(crate) reset_button: HWND,
    pub(crate) modifier_help_button: HWND,
}

impl SettingsTableView {
    pub(crate) unsafe fn selected_indices(self) -> Vec<usize> {
        list_view_selected_indices(self.list)
    }

    pub(crate) unsafe fn restore_selection(self, indices: &[usize]) {
        restore_list_view_selection(self.list, indices);
    }

    pub(crate) unsafe fn set_status(self, text: &str) {
        set_window_text(self.status, text);
    }
}

#[derive(Clone)]
pub(crate) struct ResultContextMenuRequest {
    pub(crate) hwnd: HWND,
    pub(crate) language: AppLanguage,
    pub(crate) index: usize,
    pub(crate) signature: VisibleResultSignature,
    pub(crate) from_query_launch_rule: bool,
    pub(crate) source_path: Option<PathBuf>,
    pub(crate) linked_target: Option<PathBuf>,
    pub(crate) explore_folder: Option<PathBuf>,
    pub(crate) linked_folder: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NumericEditKind {
    SignedInteger,
    UnsignedInteger,
    StrictUnsignedInteger,
    Depth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NumericEditRule {
    pub(crate) kind: NumericEditKind,
    pub(crate) min: Option<i64>,
    pub(crate) max: Option<i64>,
}

impl NumericEditRule {
    pub(crate) const fn signed_integer() -> Self {
        Self {
            kind: NumericEditKind::SignedInteger,
            min: None,
            max: None,
        }
    }

    pub(crate) const fn unsigned_integer_min(min: i64) -> Self {
        Self {
            kind: NumericEditKind::UnsignedInteger,
            min: Some(min),
            max: None,
        }
    }

    pub(crate) const fn strict_unsigned_integer(min: i64, max: i64) -> Self {
        Self {
            kind: NumericEditKind::StrictUnsignedInteger,
            min: Some(min),
            max: Some(max),
        }
    }

    pub(crate) const fn depth() -> Self {
        Self {
            kind: NumericEditKind::Depth,
            min: Some(-1),
            max: None,
        }
    }

    pub(crate) fn candidate_allowed(self, text: &str) -> bool {
        let text = text.trim();
        match self.kind {
            NumericEditKind::SignedInteger => signed_integer_edit_candidate_allowed(text),
            NumericEditKind::UnsignedInteger => unsigned_integer_edit_candidate_allowed(text),
            NumericEditKind::StrictUnsignedInteger => self.final_allowed(text) || text.is_empty(),
            NumericEditKind::Depth => depth_edit_candidate_allowed(text),
        }
    }

    pub(crate) fn final_allowed(self, text: &str) -> bool {
        let text = text.trim();
        let Ok(value) = text.parse::<i64>() else {
            return false;
        };
        match self.kind {
            NumericEditKind::Depth if value != -1 && value < 0 => return false,
            NumericEditKind::UnsignedInteger if value < 0 => return false,
            _ => {}
        }
        self.min.is_none_or(|min| value >= min) && self.max.is_none_or(|max| value <= max)
    }
}

const NUMERIC_EDIT_SUBCLASS_ID: usize = 3;

unsafe fn window_has_visible_style(hwnd: HWND) -> bool {
    get_window_long(hwnd, GWL_STYLE) as u32 & WS_VISIBLE != 0
}

unsafe extern "system" fn suspend_config_child_redraw(hwnd: HWND, state: LPARAM) -> BOOL {
    let children = &mut *(state as *mut Vec<(HWND, bool)>);
    children.push((hwnd, window_has_visible_style(hwnd)));
    SendMessageW(hwnd, WM_SETREDRAW, 0, 0);
    TRUE
}

pub(crate) fn config_child_visibility_after_redraw(
    was_visible: bool,
    visibility_touched: bool,
    currently_visible: bool,
) -> bool {
    if visibility_touched {
        currently_visible
    } else {
        was_visible
    }
}

#[derive(Clone, Copy)]
struct ConfigLayoutMetrics {
    right_x: i32,
    right_w: i32,
    pad: i32,
    page_bottom: i32,
    label_h: i32,
    edit_h: i32,
    button_h: i32,
    gap: i32,
}

const LAUNCHER_ICON_BUTTON_WIDTH: i32 = 28;
const SETTINGS_ACTION_BUTTON_WIDTH: i32 = 140;

fn format_search_elapsed(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        return format!("{} ms", duration.as_millis());
    }
    let seconds = duration.as_secs_f64();
    if seconds < 10.0 {
        format!("{seconds:.2}s")
    } else if seconds < 100.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{}s", duration.as_secs())
    }
}

fn format_count(value: usize) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    let first_group = digits.len() % 3;
    for (index, character) in digits.chars().enumerate() {
        if index > 0
            && (index == first_group || (index > first_group && (index - first_group) % 3 == 0))
        {
            formatted.push(',');
        }
        formatted.push(character);
    }
    formatted
}

fn decimal_digit_count(value: usize) -> usize {
    value.max(1).ilog10() as usize + 1
}

fn result_index_extra_width(visible_count: usize) -> i32 {
    decimal_digit_count(visible_count).saturating_sub(3) as i32 * 9
}

fn result_index_label(index: usize, is_alias: bool) -> Option<String> {
    if index < 9 {
        Some(format!("F{}", index + 1))
    } else if is_alias {
        None
    } else {
        Some((index + 1).to_string())
    }
}

fn result_text_left(
    index: usize,
    is_alias: bool,
    from_query_launch_rule: bool,
    index_extra_width: i32,
) -> i32 {
    if is_alias {
        if result_index_label(index, true).is_some() {
            48
        } else {
            18
        }
    } else if from_query_launch_rule {
        94 + index_extra_width
    } else {
        76 + index_extra_width
    }
}

pub(crate) static FLASH_LAUNCH_SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

fn launch_settings_process(page: Option<SettingsPage>) -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command.arg("--settings");
    if let Some(page) = page {
        command
            .arg("--settings-page")
            .arg(config_nav_index_for_page(page).to_string());
    }
    command.spawn()?;
    Ok(())
}

fn notify_launcher_settings_changed() {
    unsafe {
        let hwnd = FindWindowW(wide(CLASS_NAME).as_ptr(), null());
        if !hwnd.is_null() {
            PostMessageW(hwnd, WM_SETTINGS_CHANGED, 0, 0);
        }
    }
}

impl AppState {
    #[cfg(test)]
    pub(crate) fn new(items: Vec<LaunchItem>, hotkey: Hotkey) -> Self {
        Self::new_with_settings(
            items,
            AppSettingsSnapshot::first_run_defaults_with_hotkey(hotkey),
        )
    }

    pub(crate) fn new_with_settings(items: Vec<LaunchItem>, settings: AppSettingsSnapshot) -> Self {
        let hotkey = settings.hotkey.clone();
        #[cfg(not(test))]
        let plugin_state = plugins::PluginState::load(&app_config_dir());
        #[cfg(test)]
        let plugin_state = plugins::PluginState::default();
        #[cfg(not(test))]
        let search_history = load_search_history(&plugin_state);
        #[cfg(test)]
        let search_history = Vec::new();
        #[cfg(not(test))]
        let query_launch_rules = load_query_launch_rules();
        #[cfg(test)]
        let query_launch_rules = Vec::new();
        let query_launch_rules_snapshot = Arc::new(query_launch_rules.clone());
        #[cfg(not(test))]
        let search_roots = load_index_roots();
        #[cfg(test)]
        let search_roots = Vec::new();
        let search_root_plan = Arc::new(RootOwnershipPlan::build(&search_roots));
        #[cfg(not(test))]
        let search_scoring = load_scoring_config();
        #[cfg(test)]
        let search_scoring = ScoringConfig::default();
        let search_scoring_snapshot = Arc::new(search_scoring.clone());
        #[cfg(not(test))]
        let recent_items = load_recent_items();
        #[cfg(test)]
        let recent_items = Vec::new();
        let recent_items_snapshot = Arc::new(recent_items.clone());
        let result_limit = settings.result_limit;
        let search_threads = settings.search_threads;
        let search_worker = Some(SearchWorker::new(search_threads.resolve()));
        let icon_worker = Some(IconWorker::new());
        let launch_worker = Some(LaunchWorker::new());
        let save_worker = Some(SaveWorker::new());
        Self {
            hwnd: null_mut(),
            owner_hwnd: null_mut(),
            config_hwnd: null_mut(),
            add_index_hwnd: null_mut(),
            score_breakdown_hwnd: null_mut(),
            score_breakdown_header: null_mut(),
            score_breakdown_list: null_mut(),
            score_breakdown_copy: null_mut(),
            score_breakdown_close: null_mut(),
            score_explanation: None,
            edit: null_mut(),
            list: null_mut(),
            plugin_textbox: null_mut(),
            status: null_mut(),
            config_button: null_mut(),
            plugin_help_button: null_mut(),
            cfg_hotkey: null_mut(),
            cfg_index: null_mut(),
            cfg_result_limit: null_mut(),
            cfg_search_thread_mode: null_mut(),
            cfg_search_threads: null_mut(),
            cfg_sound: null_mut(),
            cfg_score_breakdown: null_mut(),
            cfg_score_breakdown_tooltip: null_mut(),
            cfg_tooltip_opacity: null_mut(),
            cfg_tooltip_opacity_value: null_mut(),
            cfg_show_cpu_in_title: null_mut(),
            cfg_show_ram_in_title: null_mut(),
            cfg_show_build_timestamp_in_title: null_mut(),
            cfg_autostart: null_mut(),
            cfg_language: null_mut(),
            result_tooltip: null_mut(),
            result_tooltip_text: Vec::new(),
            active_tooltip: None,
            cfg_query_launch_filter: null_mut(),
            cfg_query_launch_count: null_mut(),
            cfg_status: null_mut(),
            cfg_nav: null_mut(),
            cfg_modifier_help_button: null_mut(),
            search_generation: 0,
            search_scanned_total: 0,
            search_running: false,
            search_started_at: None,
            last_search_elapsed: None,
            search_worker,
            icon_worker,
            launch_worker,
            save_worker,
            settings_process_mode: false,
            helper_focus_active: false,
            helper_session_generation: 0,
            file_task_generation: 0,
            launcher_app_active: false,
            icon_helper_error_reported: false,
            shell_helper_error_reported: false,
            search_roots,
            search_root_plan,
            search_scoring,
            search_scoring_snapshot,
            search_threads,
            add_path: null_mut(),
            add_score: null_mut(),
            add_depth: null_mut(),
            add_label: null_mut(),
            add_keywords: null_mut(),
            add_enabled: null_mut(),
            add_status: null_mut(),
            add_modifier_help_button: null_mut(),
            scoring_list: null_mut(),
            scoring_status: null_mut(),
            instance: null_mut(),
            app_icon: null_mut(),
            app_icon_small: null_mut(),
            title_font: null_mut(),
            detail_font: null_mut(),
            list_bold_font: null_mut(),
            setting_help_font: null_mut(),
            tray_added: false,
            taskbar_created_message: 0,
            capturing_hotkey: false,
            ime_composing: false,
            config_scroll_y: 0,
            config_content_height: 0,
            config_redraw_transaction_depth: 0,
            config_redraw_children: Vec::new(),
            config_redraw_visibility_touched: Vec::new(),
            reload_in_progress: false,
            config_reload_generation: 0,
            config_save_generation: 0,
            config_save_in_progress: false,
            config_close_after_save: false,
            last_reload_started: None,
            config_roots: Vec::new(),
            scoring_rules: Vec::new(),
            config_recent_items: Vec::new(),
            config_search_history: Vec::new(),
            config_query_launch_rules: Vec::new(),
            config_plugin_aliases: Vec::new(),
            config_snapshot: None,
            settings_page: SettingsPage::SearchFolders,
            language: settings.language,
            loading_config_entry: false,
            loading_scoring_entry: false,
            pending_hotkey: None,
            items,
            recent_items,
            recent_items_snapshot,
            plugin_state,
            plugin_supervisor: crate::plugin_worker::PluginSupervisor::default(),
            icon_cache: HashMap::new(),
            icon_cache_lru: VecDeque::new(),
            icon_failures: HashMap::new(),
            pending_icon_requests: HashMap::new(),
            hotkey,
            result_limit,
            show_score_breakdown: settings.show_score_breakdown,
            show_score_breakdown_tooltip: settings.show_score_breakdown_tooltip,
            tooltip_opacity_percent: settings.tooltip_opacity_percent,
            show_cpu_in_title: settings.show_cpu_in_title,
            show_ram_in_title: settings.show_ram_in_title,
            show_build_timestamp_in_title: settings.show_build_timestamp_in_title,
            popup_sound: settings.popup_sound,
            search_effective_limit: result_limit,
            show_all_results_override: false,
            last_search_query: String::new(),
            search_stage: SearchStage::Idle,
            search_history,
            search_history_cursor: None,
            query_launch_rules,
            query_launch_rules_snapshot,
            query_launch_rule_cursor: None,
            query_launch_rule_filter: String::new(),
            query_launch_rule_visible_indices: Vec::new(),
            search_frozen_by_user_selection: false,
            selected_result_index: None,
            pending_launch: None,
            pending_drag: None,
            results: Vec::new(),
            result_store: None,
            inline_edit_hwnd: 0 as HWND,
            inline_edit_list: 0 as HWND,
            inline_edit_item: 0,
            inline_edit_subitem: 0,
        }
    }

    pub(crate) fn request_background_shutdown(&mut self) {
        self.cancel_active_search();
        self.end_helper_focus_session();
        if let Some(worker) = &mut self.search_worker {
            worker.request_shutdown();
        }
        if let Some(worker) = &mut self.icon_worker {
            worker.request_shutdown();
        }
        if let Some(worker) = &mut self.launch_worker {
            worker.request_shutdown();
        }
        if let Some(worker) = &mut self.save_worker {
            worker.request_shutdown();
        }
        self.plugin_supervisor.request_shutdown();
    }

    pub(crate) fn join_background_workers(&mut self) {
        if let Some(worker) = &mut self.search_worker {
            worker.join();
        }
        if let Some(worker) = &mut self.icon_worker {
            worker.join();
        }
        if let Some(worker) = &mut self.launch_worker {
            worker.join();
        }
        if let Some(worker) = &mut self.save_worker {
            worker.join();
        }
        self.plugin_supervisor.join();
    }

    pub(crate) fn default_status_text(&self) -> String {
        let hint = localized(
            self.language,
            "Shift+(Enter/double-click): Target folder | Ctrl+(Home/End): First/Last | Ctrl+PgDn: All",
        );
        if self.search_running {
            format!(
                "{} {} | {}",
                localized(self.language, "Scanning"),
                format_count(self.search_scanned_total),
                hint,
            )
        } else {
            let elapsed = self
                .last_search_elapsed
                .map(format_search_elapsed)
                .map(|value| format!(" | {value}"))
                .unwrap_or_default();
            format!(
                "{} {}{} | {}",
                localized(self.language, "Done"),
                format_count(self.search_scanned_total),
                elapsed,
                hint,
            )
        }
    }

    pub(crate) unsafe fn set_main_status(&self, text: &str) {
        if self.status.is_null() {
            return;
        }
        set_window_text(self.status, text);
        InvalidateRect(self.status, null(), TRUE);
    }

    pub(crate) fn result_count(&self) -> usize {
        self.result_store
            .as_ref()
            .map(ResultStoreReader::len)
            .unwrap_or(self.results.len())
    }

    pub(crate) fn result_at(&self, index: usize) -> Option<SearchResult> {
        if let Some(store) = self.result_store.as_ref() {
            return store.get(index).ok().flatten();
        }
        self.results.get(index).cloned()
    }

    pub(crate) fn clear_result_source(&mut self) {
        self.result_store = None;
        self.results.clear();
    }

    fn result_signature(&self, index: usize) -> Option<VisibleResultSignature> {
        let result = self.result_at(index)?;
        visible_results_signature(std::slice::from_ref(&result), 1)
            .into_iter()
            .next()
    }

    pub(crate) fn visible_result_limit(&self) -> usize {
        self.search_effective_limit.max(1)
    }

    pub(crate) fn effective_result_limit_for_spec(&self, spec: &SearchQuerySpec) -> usize {
        if spec.show_all || self.show_all_results_override {
            usize::MAX
        } else {
            self.result_limit
        }
    }

    pub(crate) unsafe fn install_numeric_edit_subclasses(&self) {
        for hwnd in [
            self.cfg_result_limit,
            self.cfg_search_threads,
            self.add_score,
            self.add_depth,
        ] {
            if !hwnd.is_null() {
                SetWindowSubclass(
                    hwnd,
                    Some(numeric_edit_subclass_proc),
                    NUMERIC_EDIT_SUBCLASS_ID,
                    0,
                );
            }
        }
    }

    pub(crate) fn numeric_edit_rule_for_hwnd(&self, hwnd: HWND) -> Option<NumericEditRule> {
        if hwnd == self.cfg_result_limit {
            Some(NumericEditRule::unsigned_integer_min(
                MIN_RESULT_LIMIT as i64,
            ))
        } else if hwnd == self.cfg_search_threads {
            Some(NumericEditRule::strict_unsigned_integer(
                MIN_SEARCH_THREADS as i64,
                available_search_threads() as i64,
            ))
        } else if hwnd == self.add_score {
            Some(NumericEditRule::signed_integer())
        } else if hwnd == self.add_depth {
            Some(NumericEditRule::depth())
        } else {
            None
        }
    }

    pub(crate) unsafe fn create_controls(&mut self, instance: HINSTANCE) {
        let edit_class = wide("EDIT");
        let list_class = wide("LISTBOX");
        let static_class = wide("STATIC");
        let button_class = wide("BUTTON");

        self.edit = CreateWindowExW(
            0,
            edit_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32,
            0,
            0,
            0,
            0,
            self.hwnd,
            ID_EDIT as isize as _,
            instance,
            null(),
        );

        self.list = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            list_class.as_ptr(),
            null(),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | LBS_NOTIFY as u32
                | LBS_OWNERDRAWFIXED as u32
                | LBS_NODATA as u32
                | LBS_NOINTEGRALHEIGHT as u32,
            0,
            0,
            0,
            0,
            self.hwnd,
            ID_LIST as isize as _,
            instance,
            null(),
        );

        self.create_app_tooltip();

        self.plugin_textbox = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            edit_class.as_ptr(),
            null(),
            WS_CHILD
                | ES_MULTILINE as u32
                | ES_AUTOVSCROLL as u32
                | ES_READONLY as u32
                | WS_VSCROLL,
            0,
            0,
            0,
            0,
            self.hwnd,
            ID_PLUGIN_TEXTBOX as isize as _,
            instance,
            null(),
        );

        self.status = CreateWindowExW(
            0,
            static_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | SS_OWNERDRAW | SS_NOTIFY,
            0,
            0,
            0,
            0,
            self.hwnd,
            ID_STATUS as isize as _,
            instance,
            null(),
        );

        self.config_button = CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide("\u{2699}").as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            0,
            0,
            0,
            self.hwnd,
            ID_CONFIG_BUTTON as isize as _,
            instance,
            null(),
        );

        self.plugin_help_button = CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide("?").as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            0,
            0,
            0,
            self.hwnd,
            ID_PLUGIN_HELP_BUTTON as isize as _,
            instance,
            null(),
        );

        let font = GetStockObject(DEFAULT_GUI_FONT);
        for child in [
            self.edit,
            self.list,
            self.plugin_textbox,
            self.status,
            self.config_button,
            self.plugin_help_button,
        ] {
            SendMessageW(child, WM_SETFONT, font as usize, TRUE as isize);
        }
        SendMessageW(self.list, LB_SETITEMHEIGHT, 0, RESULT_ROW_HEIGHT as isize);
        SetWindowSubclass(self.list, Some(list_subclass_proc), LIST_SUBCLASS_ID, 0);
        self.resize_controls();
    }

    pub(crate) unsafe fn ensure_fonts(&mut self) {
        if self.title_font.is_null() {
            self.title_font = CreateFontW(
                -16,
                0,
                0,
                0,
                FW_BOLD as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                DEFAULT_QUALITY as u32,
                DEFAULT_PITCH as u32 | FF_DONTCARE as u32,
                wide("Segoe UI").as_ptr(),
            );
        }
        if self.detail_font.is_null() {
            self.detail_font = CreateFontW(
                -12,
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                DEFAULT_QUALITY as u32,
                DEFAULT_PITCH as u32 | FF_DONTCARE as u32,
                wide("Segoe UI").as_ptr(),
            );
        }
        if self.list_bold_font.is_null() {
            let stock_font = GetStockObject(DEFAULT_GUI_FONT);
            let mut log_font: LOGFONTW = std::mem::zeroed();
            let loaded = GetObjectW(
                stock_font,
                std::mem::size_of::<LOGFONTW>() as i32,
                &mut log_font as *mut _ as *mut _,
            ) != 0;
            if loaded {
                log_font.lfWeight = FW_BOLD as i32;
                self.list_bold_font = CreateFontIndirectW(&log_font);
            }
            if self.list_bold_font.is_null() {
                self.list_bold_font = CreateFontW(
                    -12,
                    0,
                    0,
                    0,
                    FW_BOLD as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET as u32,
                    OUT_DEFAULT_PRECIS as u32,
                    CLIP_DEFAULT_PRECIS as u32,
                    DEFAULT_QUALITY as u32,
                    DEFAULT_PITCH as u32 | FF_DONTCARE as u32,
                    wide("Segoe UI").as_ptr(),
                );
            }
        }
    }

    pub(crate) unsafe fn ensure_setting_help_font(&mut self) {
        if !self.setting_help_font.is_null() {
            return;
        }
        let stock_font = GetStockObject(DEFAULT_GUI_FONT);
        let mut log_font: LOGFONTW = std::mem::zeroed();
        if GetObjectW(
            stock_font,
            std::mem::size_of::<LOGFONTW>() as i32,
            &mut log_font as *mut _ as *mut _,
        ) != 0
        {
            log_font.lfUnderline = TRUE as u8;
            self.setting_help_font = CreateFontIndirectW(&log_font);
        }
    }

    pub(crate) unsafe fn resize_controls(&self) {
        if self.hwnd.is_null()
            || self.edit.is_null()
            || self.list.is_null()
            || self.plugin_textbox.is_null()
            || self.status.is_null()
            || self.config_button.is_null()
            || self.plugin_help_button.is_null()
        {
            return;
        }

        let mut rect: RECT = std::mem::zeroed();
        GetClientRect(self.hwnd, &mut rect);

        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        let pad = 12;
        let edit_height = 20;
        let config_width = LAUNCHER_ICON_BUTTON_WIDTH;
        let help_width = LAUNCHER_ICON_BUTTON_WIDTH;
        let gap = 8;
        let status_height = 24;
        let list_top = pad + edit_height + gap;
        let list_height = (height - list_top - status_height - pad).max(80);

        MoveWindow(
            self.edit,
            pad,
            pad,
            (width - pad * 2 - config_width - help_width - gap * 2).max(120),
            edit_height,
            TRUE,
        );
        MoveWindow(
            self.plugin_help_button,
            (width - pad - config_width - help_width - gap).max(pad),
            pad,
            help_width,
            edit_height,
            TRUE,
        );
        MoveWindow(
            self.config_button,
            (width - pad - config_width).max(pad),
            pad,
            config_width,
            edit_height,
            TRUE,
        );
        MoveWindow(
            self.list,
            pad,
            list_top,
            (width - pad * 2).max(120),
            list_height,
            TRUE,
        );
        MoveWindow(
            self.plugin_textbox,
            pad,
            list_top,
            (width - pad * 2).max(120),
            list_height,
            TRUE,
        );
        MoveWindow(
            self.status,
            pad,
            list_top + list_height + gap,
            (width - pad * 2).max(120),
            status_height,
            TRUE,
        );
    }

    pub(crate) unsafe fn refresh_results(&mut self) {
        self.clear_pending_launch();
        let query = get_window_text(self.edit);
        let trimmed = query.trim().to_string();
        let spec = parse_search_query(&query);
        let query_changed = self.last_search_query != query;
        let previous_spec = parse_search_query(&self.last_search_query);
        let preserve_refinement_results = query_changed
            && query_specs_allow_refinement(&previous_spec, &spec)
            && self.result_store.is_none()
            && !self.results.is_empty();
        if query_changed {
            self.show_all_results_override = false;
            self.last_search_query = query.clone();
            if !preserve_refinement_results {
                self.clear_result_source();
            }
        }
        let preserve_top_while_show_all = !query_changed
            && self.show_all_results_override
            && self.result_store.is_none()
            && !self.results.is_empty();
        self.search_frozen_by_user_selection = false;
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        ACTIVE_SEARCH_GENERATION.store(generation, Ordering::Relaxed);
        self.clear_pending_search_batches();
        self.search_effective_limit = self.effective_result_limit_for_spec(&spec);
        self.search_stage = SearchStage::Idle;
        self.search_started_at = Some(Instant::now());
        self.last_search_elapsed = None;
        if let Some(plugin_id) = self.plugin_state.matching_plugin_id(&trimmed) {
            if let Some(worker) = &self.search_worker {
                worker.invalidate_session();
            }
            self.search_running = true;
            self.search_scanned_total = 0;
            self.search_stage = SearchStage::Idle;
            self.clear_result_source();
            self.render_results(None);
            self.update_plugin_textbox();
            let alias = self
                .plugin_state
                .alias_for(plugin_id)
                .unwrap_or_default()
                .to_string();
            if let Err(error) =
                self.plugin_supervisor
                    .query(plugin_id, &alias, self.language, generation, &query)
            {
                self.search_running = false;
                self.search_stage = SearchStage::Done;
                self.set_main_status(&localized_format1(self.language, "Plugin error: {}", error));
            }
            return;
        }

        self.search_running = true;
        self.search_scanned_total = 0;
        self.search_stage = match spec.mode {
            SearchQueryMode::DirectoryBrowse => SearchStage::Directory,
            SearchQueryMode::BlankHistory | SearchQueryMode::NormalSearch => SearchStage::History,
        };
        if !preserve_top_while_show_all && !preserve_refinement_results {
            self.clear_result_source();
        }
        let hwnd_value = self.hwnd as isize;
        self.enqueue_search_request(SearchWorkerRequest {
            generation,
            hwnd_value,
            spec,
            root_plan: Arc::clone(&self.search_root_plan),
            scoring: Arc::clone(&self.search_scoring_snapshot),
            recent_items: Arc::clone(&self.recent_items_snapshot),
            query_launch_rules: Arc::clone(&self.query_launch_rules_snapshot),
            effective_limit: self.search_effective_limit,
            search_threads: self.search_threads.resolve(),
            include_score_detail: self.show_score_breakdown || self.show_score_breakdown_tooltip,
            include_explanation: self.show_score_breakdown
                && self.search_effective_limit != usize::MAX,
        });

        if !preserve_top_while_show_all && !preserve_refinement_results {
            self.render_results(None);
        }
        let plugin_active = self.update_plugin_textbox();
        if !plugin_active {
            self.set_main_status(&self.default_status_text());
        }
    }

    pub(crate) unsafe fn schedule_refresh_results(&mut self) {
        if self.hwnd.is_null() {
            return;
        }
        KillTimer(self.hwnd, SEARCH_REFRESH_TIMER_ID);
        let query = get_window_text(self.edit);
        let trimmed = query.trim().to_string();
        if self.plugin_state.is_plugin_query(&trimmed) {
            self.refresh_results();
            return;
        }

        self.cancel_active_search();
        let spec = parse_search_query(&query);
        self.search_effective_limit = self.effective_result_limit_for_spec(&spec);
        self.search_running = true;
        self.search_started_at = Some(Instant::now());
        self.search_stage = SearchStage::Idle;
        self.update_plugin_textbox();
        SetTimer(
            self.hwnd,
            SEARCH_REFRESH_TIMER_ID,
            SEARCH_REFRESH_DEBOUNCE_MS,
            None,
        );
    }

    pub(crate) fn refresh_search_config_snapshots(&mut self) {
        self.search_root_plan = Arc::new(RootOwnershipPlan::build(&self.search_roots));
        self.search_scoring_snapshot = Arc::new(self.search_scoring.clone());
    }

    pub(crate) fn refresh_recent_items_snapshot(&mut self) {
        self.recent_items_snapshot = Arc::new(self.recent_items.clone());
    }

    pub(crate) fn refresh_query_launch_rules_snapshot(&mut self) {
        self.query_launch_rules_snapshot = Arc::new(self.query_launch_rules.clone());
    }

    pub(crate) unsafe fn render_results(&mut self, preserve_path: Option<String>) {
        self.ensure_fonts();
        let visible_count = self.result_count().min(self.visible_result_limit());
        self.sync_result_list_row_count(visible_count);

        let top_before = self.result_list_top_index();
        if visible_count == 0 {
            self.selected_result_index = None;
            SendMessageW(self.list, LB_SETCURSEL, usize::MAX, 0);
            SendMessageW(self.list, LB_SETTOPINDEX, 0, 0);
        } else if let Some(selected) = preserve_path.as_deref().and_then(|path| {
            let preserve_search_count = visible_count.min(self.results.len().max(1));
            (0..preserve_search_count).find(|index| {
                self.result_at(*index)
                    .and_then(|result| result_target_text(&result))
                    .as_deref()
                    == Some(path)
            })
        }) {
            SendMessageW(self.list, LB_SETCURSEL, selected, 0);
            SendMessageW(self.list, LB_SETCARETINDEX, selected, 0);
            self.selected_result_index = Some(selected);
            self.ensure_result_index_visible_from_top(selected, visible_count, top_before);
        } else {
            SendMessageW(self.list, LB_SETCURSEL, usize::MAX, 0);
            SendMessageW(self.list, LB_SETTOPINDEX, 0, 0);
            self.selected_result_index = None;
        }
        self.request_result_list_repaint();
    }

    pub(crate) unsafe fn result_list_count(&self) -> usize {
        let count = SendMessageW(self.list, LB_GETCOUNT, 0, 0);
        if count < 0 {
            0
        } else {
            count as usize
        }
    }

    pub(crate) unsafe fn sync_result_list_row_count(&self, visible_count: usize) {
        let current_count = self.result_list_count();
        if current_count == visible_count {
            return;
        }

        SendMessageW(self.list, WM_SETREDRAW, 0, 0);
        SendMessageW(self.list, LB_SETCOUNT, visible_count, 0);
        SendMessageW(self.list, WM_SETREDRAW, TRUE as usize, 0);
    }

    pub(crate) fn clear_pending_search_batches(&self) {
        if let Ok(mut pending) = pending_search_slot().lock() {
            pending.clear();
        }
    }

    pub(crate) fn enqueue_search_request(&self, request: SearchWorkerRequest) {
        if let Some(worker) = &self.search_worker {
            worker.search(request);
        }
    }

    pub(crate) fn ensure_search_worker_running(&mut self) -> bool {
        if self.search_worker.is_some() {
            return false;
        }
        self.search_worker = Some(SearchWorker::new(self.search_threads.resolve()));
        true
    }

    pub(crate) fn stop_search_worker(&mut self) -> bool {
        let Some(mut worker) = self.search_worker.take() else {
            return false;
        };
        worker.request_shutdown();
        worker.join();
        true
    }

    fn cancel_active_search_with_result_policy(&mut self, clear_result_source: bool) {
        self.clear_pending_launch();
        self.search_running = false;
        self.search_scanned_total = 0;
        self.search_stage = SearchStage::Done;
        self.search_generation = self.search_generation.wrapping_add(1);
        ACTIVE_SEARCH_GENERATION.store(self.search_generation, Ordering::Relaxed);
        if let Some(worker) = &self.icon_worker {
            worker.advance_generation(self.search_generation);
        }
        self.pending_icon_requests.clear();
        self.clear_pending_search_batches();
        if clear_result_source {
            self.result_store = None;
        }
    }

    pub(crate) fn cancel_active_search(&mut self) {
        self.cancel_active_search_with_result_policy(true);
    }

    pub(crate) fn cancel_active_search_preserving_result_source(&mut self) {
        self.cancel_active_search_with_result_policy(false);
    }

    pub(crate) fn release_show_all_results_on_hide(&mut self) {
        if let Some(worker) = &self.search_worker {
            worker.invalidate_session();
        }
        if self.show_all_results_override
            || self.search_effective_limit == usize::MAX
            || self.result_store.is_some()
        {
            self.cancel_active_search();
            self.show_all_results_override = false;
            self.search_effective_limit = self.result_limit.max(1);
            self.clear_result_source();
            self.selected_result_index = None;
        }
    }

    pub(crate) unsafe fn freeze_search_due_to_user_selection(&mut self) {
        if self.search_frozen_by_user_selection {
            return;
        }
        self.clear_pending_launch();
        self.search_frozen_by_user_selection = true;
        self.search_running = false;
        self.search_stage = SearchStage::Done;
        self.search_generation = self.search_generation.wrapping_add(1);
        ACTIVE_SEARCH_GENERATION.store(self.search_generation, Ordering::Relaxed);
        self.clear_pending_search_batches();
        self.hide_app_tooltip();
        self.set_main_status(&self.default_status_text());
    }

    unsafe fn create_app_tooltip(&mut self) {
        if self.list.is_null() {
            return;
        }
        self.result_tooltip = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            wide(TOOLTIP_CLASS_NAME).as_ptr(),
            null(),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            null_mut(),
            null_mut(),
            self.instance,
            null(),
        );
        if !self.result_tooltip.is_null() {
            SendMessageW(
                self.result_tooltip,
                WM_SETFONT,
                GetStockObject(DEFAULT_GUI_FONT) as usize,
                TRUE as isize,
            );
            self.apply_app_tooltip_opacity();
        }
    }

    pub(crate) unsafe fn apply_app_tooltip_opacity(&self) {
        if self.result_tooltip.is_null() {
            return;
        }
        let percent = self
            .tooltip_opacity_percent
            .clamp(MIN_TOOLTIP_OPACITY_PERCENT, MAX_TOOLTIP_OPACITY_PERCENT);
        let ex_style = get_window_long(self.result_tooltip, GWL_EXSTYLE) as u32;
        set_window_long(
            self.result_tooltip,
            GWL_EXSTYLE,
            (ex_style | WS_EX_LAYERED) as isize,
        );
        let alpha = ((percent as u16 * 255) / 100) as u8;
        SetLayeredWindowAttributes(self.result_tooltip, 0, alpha, LWA_ALPHA);
    }

    pub(crate) unsafe fn update_tooltip_opacity_value_label(&self) {
        if self.cfg_tooltip_opacity_value.is_null() {
            return;
        }
        let value = if self.cfg_tooltip_opacity.is_null() {
            self.tooltip_opacity_percent as i32
        } else {
            SendMessageW(self.cfg_tooltip_opacity, TBM_GETPOS, 0, 0) as i32
        }
        .clamp(
            MIN_TOOLTIP_OPACITY_PERCENT as i32,
            MAX_TOOLTIP_OPACITY_PERCENT as i32,
        );
        set_window_text(self.cfg_tooltip_opacity_value, &format!("{value}%"));
    }

    unsafe fn update_result_tooltip(&mut self, index: usize) {
        let text = if self.show_score_breakdown_tooltip {
            self.result_at(index).and_then(|result| {
                result_score_breakdown_tooltip_text_for_setting(
                    &result,
                    self.show_score_breakdown_tooltip,
                )
                .map(str::to_string)
            })
        } else {
            None
        };
        let mut point: POINT = std::mem::zeroed();
        if GetCursorPos(&mut point) == 0 {
            self.hide_app_tooltip();
            return;
        }
        if let Some(text) = text {
            self.show_tooltip_content(
                TooltipContent {
                    kind: TooltipKind::ScoreBreakdown,
                    title: String::new(),
                    body: text,
                },
                point.x,
                point.y,
            );
        } else {
            self.hide_app_tooltip();
        }
    }

    unsafe fn result_text_hit_test(&mut self, index: usize, x: i32, y: i32) -> bool {
        let Some(result) = self.result_at(index) else {
            return false;
        };
        let title_text = result.title.clone();
        let detail_text = result_detail_text(&result, self.show_score_breakdown);
        let is_alias = result.is_alias_result();
        let from_query_launch_rule = result.from_query_launch_rule;
        let top = index as i32 * RESULT_ROW_HEIGHT;
        if y < top + 5 || y > top + RESULT_ROW_HEIGHT - 3 {
            return false;
        }
        let index_extra_w = result_index_extra_width(self.result_list_count());
        let text_left = result_text_left(index, is_alias, from_query_launch_rule, index_extra_w);
        if x < text_left {
            return false;
        }

        self.ensure_fonts();
        let title = wide(&title_text);
        let detail = wide(&detail_text);
        let hdc = GetDC(self.list);
        let old_font = SelectObject(hdc, self.title_font as _);
        let mut title_size: SIZE = std::mem::zeroed();
        GetTextExtentPoint32W(
            hdc,
            title.as_ptr(),
            title.len().saturating_sub(1) as i32,
            &mut title_size,
        );
        SelectObject(hdc, self.detail_font as _);
        let mut detail_size: SIZE = std::mem::zeroed();
        GetTextExtentPoint32W(
            hdc,
            detail.as_ptr(),
            detail.len().saturating_sub(1) as i32,
            &mut detail_size,
        );
        SelectObject(hdc, old_font);
        ReleaseDC(self.list, hdc);

        let content_right = text_left + title_size.cx.max(detail_size.cx).min(620) + 8;
        x <= content_right
    }

    pub(crate) unsafe fn show_app_tooltip(&mut self, text: &str, screen_x: i32, screen_y: i32) {
        if self.result_tooltip.is_null() {
            return;
        }
        if text.is_empty() {
            self.hide_app_tooltip();
            return;
        }
        self.result_tooltip_text = wide(text);
        set_window_text(self.result_tooltip, text);

        let hdc = GetDC(self.result_tooltip);
        let old_font = SelectObject(hdc, GetStockObject(DEFAULT_GUI_FONT));
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: 520,
            bottom: 0,
        };
        DrawTextW(
            hdc,
            self.result_tooltip_text.as_ptr(),
            -1,
            &mut rect,
            DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
        );
        SelectObject(hdc, old_font);
        ReleaseDC(self.result_tooltip, hdc);

        SetWindowPos(
            self.result_tooltip,
            HWND_TOPMOST,
            screen_x + 16,
            screen_y + 20,
            (rect.right - rect.left + 14).max(24),
            (rect.bottom - rect.top + 8).max(24),
            SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
        );
        InvalidateRect(self.result_tooltip, null(), TRUE);
    }

    pub(crate) unsafe fn show_tooltip_content(
        &mut self,
        content: TooltipContent,
        screen_x: i32,
        screen_y: i32,
    ) {
        if content.kind == TooltipKind::ScoreBreakdown && !self.show_score_breakdown_tooltip {
            self.hide_app_tooltip();
            return;
        }
        let text = if content.title.is_empty() {
            content.body.clone()
        } else {
            format!("{}\n{}", content.title, content.body)
        };
        self.active_tooltip = Some(content);
        self.show_app_tooltip(&text, screen_x, screen_y);
    }

    pub(crate) unsafe fn hide_app_tooltip(&mut self) {
        if !self.result_tooltip.is_null() {
            ShowWindow(self.result_tooltip, SW_HIDE);
        }
        self.active_tooltip = None;
    }

    pub(crate) unsafe fn update_settings_tooltip(
        &mut self,
        source_hwnd: HWND,
        screen_x: i32,
        screen_y: i32,
    ) {
        if source_hwnd != self.cfg_index || self.settings_page != SettingsPage::SearchFolders {
            self.hide_app_tooltip();
            return;
        }

        let mut point = POINT {
            x: screen_x,
            y: screen_y,
        };
        ScreenToClient(self.cfg_index, &mut point);
        let mut hit: LVHITTESTINFO = std::mem::zeroed();
        hit.pt = point;
        let index = SendMessageW(
            self.cfg_index,
            LVM_SUBITEMHITTEST,
            0,
            &mut hit as *mut LVHITTESTINFO as isize,
        ) as i32;
        if index < 0
            || hit.iSubItem != 0
            || (hit.flags & (LVHT_ONITEMICON | LVHT_ONITEMLABEL | LVHT_ONITEMSTATEICON)) == 0
        {
            self.hide_app_tooltip();
            return;
        }

        let text = self.search_folder_tooltip_text(index);
        if text.is_empty() {
            self.hide_app_tooltip();
            return;
        }
        self.show_app_tooltip(&text, screen_x, screen_y);
    }

    pub(crate) unsafe fn apply_search_results_batch(&mut self) {
        let mut batches = Vec::new();
        if let Ok(mut pending) = pending_search_slot().lock() {
            batches.append(&mut *pending);
        }
        if batches.is_empty() {
            return;
        }
        if self.search_frozen_by_user_selection {
            self.clear_pending_search_batches();
            return;
        }
        let generation = self.search_generation;
        let budget_started = Instant::now();
        let mut deferred_batches = Vec::new();
        let mut budget_exhausted = false;
        let mut changed = false;
        let mut received_results = false;
        let mut search_done = false;
        let mut error_status = None;
        let preserve_path = self
            .selected_index()
            .and_then(|index| self.result_at(index))
            .and_then(|result| result_target_text(&result));
        for mut batch in batches {
            if budget_exhausted || ui_result_budget_exhausted(budget_started) {
                budget_exhausted = true;
                deferred_batches.push(batch);
                continue;
            }
            if batch.generation != generation {
                continue;
            }
            self.search_scanned_total = self.search_scanned_total.max(batch.scanned_total);
            self.search_stage = batch.stage;
            self.search_effective_limit = batch.effective_limit.max(1);
            if batch.done {
                search_done = true;
                self.search_running = false;
                self.search_stage = SearchStage::Done;
                if let Some(started) = self.search_started_at.take() {
                    self.last_search_elapsed = Some(started.elapsed());
                }
            }
            match batch.result_store.take() {
                Some(ResultStoreCompletion::Ready(manifest))
                    if manifest.generation == generation =>
                {
                    match ResultStoreReader::open(manifest) {
                        Ok(store) => {
                            self.results = batch.results;
                            self.result_store = Some(store);
                            received_results = true;
                            changed = true;
                        }
                        Err(error) => {
                            self.show_all_results_override = false;
                            self.search_effective_limit = self.result_limit.max(1);
                            error_status = Some(localized_format1(
                                self.language,
                                "Show All failed: {}",
                                error,
                            ));
                        }
                    }
                }
                Some(ResultStoreCompletion::Ready(_)) => {}
                Some(ResultStoreCompletion::Error(error)) => {
                    self.show_all_results_override = false;
                    self.search_effective_limit = self.result_limit.max(1);
                    error_status = Some(localized_format1(
                        self.language,
                        "Show All failed: {}",
                        error,
                    ));
                }
                None if batch.effective_limit == usize::MAX => {}
                None => {
                    self.result_store = None;
                    let visible_limit = self.visible_result_limit();
                    let visible_changed = visible_results_signature(&self.results, visible_limit)
                        != visible_results_signature(&batch.results, visible_limit);
                    self.results = batch.results;
                    received_results = true;
                    changed |= visible_changed;
                }
            }
        }
        if !deferred_batches.is_empty() {
            if let Ok(mut pending) = pending_search_slot().lock() {
                deferred_batches.append(&mut *pending);
                *pending = deferred_batches;
            }
            PostMessageW(self.hwnd, WM_SEARCH_RESULTS_READY, 0, 0);
        }
        if changed {
            self.render_results(preserve_path);
        }
        if self.resolve_pending_launch(generation, received_results, search_done) {
            return;
        }
        if let Some(error) = error_status {
            self.set_main_status(&error);
        } else {
            self.set_main_status(&self.default_status_text());
        }
    }

    pub(crate) unsafe fn update_plugin_textbox(&self) -> bool {
        let query = get_window_text(self.edit);
        let plugin_active = self.plugin_state.is_plugin_query(&query);
        ShowWindow(self.list, if plugin_active { SW_HIDE } else { SW_SHOW });
        ShowWindow(
            self.plugin_textbox,
            if plugin_active { SW_SHOW } else { SW_HIDE },
        );

        if !plugin_active {
            set_window_text(self.plugin_textbox, "");
            return false;
        }

        let mut lines = Vec::new();
        let has_instant_result = self
            .results
            .first()
            .is_some_and(|first| first.score == i32::MAX);
        if let Some(first) = self.results.first().filter(|_| has_instant_result) {
            lines.push(first.title.clone());
        } else {
            lines.push(String::new());
        }
        lines.push(String::new());
        lines.push(
            "--------------------------------------------------------------------------------"
                .to_string(),
        );

        for result in self.results.iter().take(self.visible_result_limit()) {
            if has_instant_result && result.score == i32::MAX {
                continue;
            }
            lines.push(result.title.clone());
            if !result.subtitle.is_empty() {
                lines.push(result.subtitle.clone());
            }
        }
        let text = lines.join("\r\n");
        set_window_text(self.plugin_textbox, &text);
        self.set_main_status(&self.default_status_text());
        true
    }

    pub(crate) unsafe fn show_hover_path(&mut self, x: i32, y: i32) {
        let hit = SendMessageW(self.list, LB_ITEMFROMPOINT, 0, make_lparam(x, y));
        if hit >= 0 && hiword(hit as usize) == 0 {
            let index = loword(hit as usize) as usize;
            if index < self.result_count().min(self.visible_result_limit()) {
                if let Some(path) = self.result_target_text(index) {
                    if self.result_text_hit_test(index, x, y) {
                        self.update_result_tooltip(index);
                    } else {
                        self.hide_app_tooltip();
                    }
                    self.set_main_status(&path);
                    return;
                }
            }
        }
        self.hide_app_tooltip();
        self.set_main_status(&self.default_status_text());
    }

    pub(crate) unsafe fn draw_status_item(&mut self, draw: &DRAWITEMSTRUCT) {
        self.ensure_fonts();
        let mut rect = draw.rcItem;
        FillRect(draw.hDC, &rect, GetSysColorBrush(COLOR_WINDOW));
        let old_font = SelectObject(draw.hDC, GetStockObject(DEFAULT_GUI_FONT));
        SetBkMode(draw.hDC, TRANSPARENT as i32);
        SetTextColor(draw.hDC, GetSysColor(COLOR_WINDOWTEXT));
        let text = get_window_text(self.status);
        for (run, bold) in status_text_runs(&text) {
            if rect.left >= rect.right {
                break;
            }
            SelectObject(
                draw.hDC,
                if bold && !self.list_bold_font.is_null() {
                    self.list_bold_font as _
                } else {
                    GetStockObject(DEFAULT_GUI_FONT)
                },
            );
            let wide_text = wide(run);
            let mut size: SIZE = std::mem::zeroed();
            GetTextExtentPoint32W(
                draw.hDC,
                wide_text.as_ptr(),
                (wide_text.len() - 1) as i32,
                &mut size,
            );
            DrawTextW(
                draw.hDC,
                wide_text.as_ptr(),
                -1,
                &mut rect,
                DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
            rect.left += size.cx;
        }
        SelectObject(draw.hDC, old_font);
    }

    pub(crate) unsafe fn draw_result_item(&mut self, draw: &DRAWITEMSTRUCT) {
        if draw.itemID == u32::MAX {
            return;
        }
        let index = draw.itemID as usize;
        let Some(result) = self.result_at(index) else {
            return;
        };
        let title = result.title.clone();
        let detail_text = result_detail_text(&result, self.show_score_breakdown);
        let path = result_target_path(&result);
        let is_dir = result.is_dir;
        let is_alias = result.is_alias_result();
        let from_query_launch_rule = result.from_query_launch_rule;

        self.ensure_fonts();
        let selected = self.selected_result_index == Some(index);
        let background = GetSysColorBrush(if selected {
            COLOR_HIGHLIGHT
        } else {
            COLOR_WINDOW
        });
        FillRect(draw.hDC, &draw.rcItem, background);
        SetBkMode(draw.hDC, TRANSPARENT as i32);

        let index_extra_w = result_index_extra_width(self.result_list_count());
        if !is_alias {
            if let Some(path) = path.as_deref() {
                if let Some(icon) = self.cached_icon_for_path(path, is_dir) {
                    DrawIconEx(
                        draw.hDC,
                        draw.rcItem.left + 42 + index_extra_w,
                        draw.rcItem.top + 11,
                        icon,
                        32,
                        32,
                        0,
                        null_mut(),
                        DI_NORMAL,
                    );
                } else {
                    draw_fallback_path_icon(
                        draw.hDC,
                        draw.rcItem.left + 42 + index_extra_w,
                        draw.rcItem.top + 11,
                        is_dir,
                    );
                }
            }
        }

        let old_font = SelectObject(draw.hDC, self.title_font as _);
        if let Some(number_text) = result_index_label(index, is_alias) {
            let number_wide = wide(&number_text);
            let mut number_rect = draw.rcItem;
            number_rect.left += 8;
            number_rect.right = number_rect.left + 28 + index_extra_w;
            number_rect.top += 7;
            number_rect.bottom -= 4;
            SetTextColor(draw.hDC, if selected { 0x00FF_FFFF } else { 0x0080_8080 });
            DrawTextW(
                draw.hDC,
                number_wide.as_ptr(),
                -1,
                &mut number_rect,
                DT_LEFT | DT_SINGLELINE | DT_NOPREFIX,
            );
            if from_query_launch_rule {
                let badge_wide = wide("★");
                let mut badge_rect = draw.rcItem;
                badge_rect.left += 76 + index_extra_w;
                badge_rect.right = badge_rect.left + 16;
                badge_rect.top += 5;
                badge_rect.bottom = badge_rect.top + 20;
                SetTextColor(draw.hDC, if selected { 0x0000_FFFF } else { 0x0000_CCFF });
                DrawTextW(
                    draw.hDC,
                    badge_wide.as_ptr(),
                    -1,
                    &mut badge_rect,
                    DT_LEFT | DT_SINGLELINE | DT_NOPREFIX,
                );
            }
        }

        let text_left = result_text_left(index, is_alias, from_query_launch_rule, index_extra_w);
        let mut title_rect = draw.rcItem;
        title_rect.left += text_left;
        title_rect.right -= 8;
        title_rect.top += 5;
        title_rect.bottom = title_rect.top + 20;

        let mut detail_rect = draw.rcItem;
        detail_rect.left += text_left;
        detail_rect.right -= 8;
        detail_rect.top += 26;
        detail_rect.bottom -= 3;

        let title_wide = wide(&title);
        let detail_wide = wide(&detail_text);

        SelectObject(draw.hDC, self.title_font as _);
        SetTextColor(draw.hDC, if selected { 0x00FF_FFFF } else { 0x0000_0000 });
        DrawTextW(
            draw.hDC,
            title_wide.as_ptr(),
            -1,
            &mut title_rect,
            DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );

        SelectObject(draw.hDC, self.detail_font as _);
        SetTextColor(draw.hDC, if selected { 0x00FF_FFFF } else { 0x0080_4000 });
        DrawTextW(
            draw.hDC,
            detail_wide.as_ptr(),
            -1,
            &mut detail_rect,
            DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
        SelectObject(draw.hDC, old_font);
    }

    pub(crate) unsafe fn cleanup_fonts(&mut self) {
        if !self.title_font.is_null() {
            DeleteObject(self.title_font as _);
            self.title_font = null_mut();
        }
        if !self.detail_font.is_null() {
            DeleteObject(self.detail_font as _);
            self.detail_font = null_mut();
        }
        if !self.list_bold_font.is_null() {
            DeleteObject(self.list_bold_font as _);
            self.list_bold_font = null_mut();
        }
        if !self.setting_help_font.is_null() {
            DeleteObject(self.setting_help_font as _);
            self.setting_help_font = null_mut();
        }
    }

    pub(crate) unsafe fn cleanup_icon_cache(&mut self) {
        for icon in self.icon_cache.drain().map(|(_, icon)| icon) {
            if !icon.is_null() {
                DestroyIcon(icon);
            }
        }
        self.icon_cache_lru.clear();
        self.icon_failures.clear();
        self.pending_icon_requests.clear();
    }

    pub(crate) unsafe fn evict_icon_cache_lru(&mut self) {
        for _ in 0..ICON_CACHE_EVICT_BATCH.min(self.icon_cache.len()) {
            let Some(key) = self.icon_cache_lru.pop_front() else {
                break;
            };
            if let Some(icon) = self.icon_cache.remove(&key) {
                if !icon.is_null() {
                    DestroyIcon(icon);
                }
            }
        }
    }

    pub(crate) unsafe fn reload_settings_from_disk(&mut self) {
        self.schedule_config_reload(true, true);
    }

    pub(crate) unsafe fn schedule_search_configuration_reload(&mut self, force: bool) {
        self.schedule_config_reload(force, false);
    }

    unsafe fn schedule_config_reload(&mut self, force: bool, full: bool) {
        if !force {
            if let Some(started) = self.last_reload_started {
                if started.elapsed() < BACKGROUND_RELOAD_DEBOUNCE {
                    return;
                }
            }
        }
        self.last_reload_started = Some(Instant::now());
        self.reload_in_progress = true;
        self.config_reload_generation = self.config_reload_generation.wrapping_add(1).max(1);
        self.cancel_active_search();
        if let Some(worker) = &self.save_worker {
            worker.reload_config(self.config_reload_generation, self.hwnd as isize, full);
        }
        self.set_main_status(localized(self.language, "Reloading config..."));
    }

    pub(crate) unsafe fn apply_config_reload_results(&mut self) {
        let results = if let Ok(mut pending) = pending_config_reload_slot().lock() {
            std::mem::take(&mut *pending)
        } else {
            Vec::new()
        };
        for result in results {
            if result.generation != self.config_reload_generation {
                continue;
            }
            self.reload_in_progress = false;
            if let Some(full) = result.full {
                if self.settings_process_mode && !self.config_hwnd.is_null() {
                    self.apply_settings_window_reload(
                        full,
                        result.search_roots,
                        result.search_scoring,
                    );
                    continue;
                }
                let hotkey_changed = self.hotkey != full.settings.hotkey;
                self.hotkey = full.settings.hotkey;
                if hotkey_changed && !self.apply_hotkey() {
                    show_error(
                        self.hwnd,
                        localized(
                            self.language,
                            "Could not register the configured popup hotkey.",
                        ),
                    );
                }
                self.result_limit = full.settings.result_limit;
                self.search_effective_limit = full.settings.result_limit.max(1);
                self.search_threads = full.settings.search_threads;
                self.tooltip_opacity_percent = full.settings.tooltip_opacity_percent;
                self.show_cpu_in_title = full.settings.show_cpu_in_title;
                self.show_ram_in_title = full.settings.show_ram_in_title;
                self.show_build_timestamp_in_title = full.settings.show_build_timestamp_in_title;
                self.popup_sound = full.settings.popup_sound;
                self.show_score_breakdown = full.settings.show_score_breakdown;
                self.show_score_breakdown_tooltip = full.settings.show_score_breakdown_tooltip;
                self.language = full.settings.language;
                set_active_language(self.language);
                self.apply_app_tooltip_opacity();
                self.plugin_state = full.plugin_state;
                if let Some(alias) = self.plugin_state.alias_for("calculator") {
                    self.plugin_supervisor.update_alias("calculator", alias);
                }
                self.search_history = full.search_history;
                self.search_history_cursor = None;
                self.query_launch_rules = full.query_launch_rules;
                self.refresh_query_launch_rules_snapshot();
                self.query_launch_rule_cursor = None;
                self.recent_items = full.recent_items;
                self.refresh_recent_items_snapshot();
                set_window_text(
                    self.hwnd,
                    &window_title_text(
                        self.language,
                        self.show_cpu_in_title,
                        self.show_ram_in_title,
                        self.show_build_timestamp_in_title,
                    ),
                );
            }
            self.search_roots = result.search_roots;
            self.search_scoring = result.search_scoring;
            self.refresh_search_config_snapshots();
            if IsWindowVisible(self.hwnd) != 0 {
                self.refresh_results();
            } else {
                self.search_running = false;
                self.search_stage = SearchStage::Idle;
                self.search_started_at = None;
                self.clear_result_source();
            }
            self.set_main_status(localized(
                self.language,
                "Config reloaded. Search streams on demand.",
            ));
        }
    }

    pub(crate) unsafe fn finish_background_rebuild(&mut self, items: Vec<LaunchItem>) {
        self.reload_in_progress = false;
        self.cancel_active_search();
        self.items = items;
        self.refresh_results();
    }

    pub(crate) unsafe fn cached_icon_for_path(
        &mut self,
        path: &Path,
        is_dir: bool,
    ) -> Option<HICON> {
        let key = icon_cache_key(path, is_dir);
        if let Some(icon) = self.icon_cache.get(&key).copied() {
            self.icon_cache_lru.retain(|cached| cached != &key);
            self.icon_cache_lru.push_back(key);
            return Some(icon);
        }
        if let Some(failed_at) = self.icon_failures.get(&key).copied() {
            if failed_at.elapsed() < ICON_FAILURE_RETRY {
                return None;
            }
            self.icon_failures.remove(&key);
        }
        if self.pending_icon_requests.contains_key(&key) {
            return None;
        }
        self.pending_icon_requests
            .insert(key.clone(), self.search_generation);
        self.enqueue_icon_request(IconLoadRequest {
            session_generation: self.helper_session_generation,
            generation: self.search_generation,
            hwnd_value: self.hwnd as isize,
            key,
            path: path.to_path_buf(),
        });
        None
    }

    pub(crate) fn enqueue_icon_request(&mut self, request: IconLoadRequest) {
        if let Some(worker) = &self.icon_worker {
            worker.load(request);
        }
    }

    pub(crate) unsafe fn apply_icon_results(&mut self) {
        let mut results = Vec::new();
        if let Ok(mut pending) = pending_icon_slot().lock() {
            results.append(&mut *pending);
        }
        if results.is_empty() {
            return;
        }
        let budget_started = Instant::now();
        let mut deferred_results = Vec::new();
        let mut changed = false;
        for result in results {
            if ui_result_budget_exhausted(budget_started) {
                deferred_results.push(result);
                continue;
            }
            if result.session_generation != self.helper_session_generation {
                if result.icon_value != 0 {
                    DestroyIcon(result.icon_value as HICON);
                }
                continue;
            }
            if let Some(error) = result.helper_error.as_deref() {
                self.pending_icon_requests.clear();
                self.show_helper_error_once("Icon", error);
            }
            if self.pending_icon_requests.get(&result.key).copied() == Some(result.generation) {
                self.pending_icon_requests.remove(&result.key);
            }
            if result.generation != self.search_generation {
                if result.icon_value != 0 {
                    DestroyIcon(result.icon_value as HICON);
                }
                continue;
            }
            if result.icon_value == 0 {
                self.icon_failures.insert(result.key, Instant::now());
                continue;
            }
            if self.icon_cache.len() >= ICON_CACHE_LIMIT {
                self.evict_icon_cache_lru();
            }
            let key = result.key;
            if let Some(previous) = self
                .icon_cache
                .insert(key.clone(), result.icon_value as HICON)
            {
                if !previous.is_null() {
                    DestroyIcon(previous);
                }
            }
            self.icon_cache_lru.retain(|cached| cached != &key);
            self.icon_cache_lru.push_back(key);
            changed = true;
        }
        if !deferred_results.is_empty() {
            if let Ok(mut pending) = pending_icon_slot().lock() {
                deferred_results.append(&mut *pending);
                *pending = deferred_results;
            }
            PostMessageW(self.hwnd, WM_ICON_READY, 0, 0);
        }
        if changed {
            RedrawWindow(
                self.list,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ERASE | RDW_FRAME,
            );
        }
    }

    pub(crate) unsafe fn begin_helper_focus_session(&mut self) {
        if self.settings_process_mode || self.helper_focus_active {
            return;
        }
        self.helper_focus_active = true;
        self.helper_session_generation = self.helper_session_generation.wrapping_add(1).max(1);
        self.icon_helper_error_reported = false;
        self.shell_helper_error_reported = false;
        if let Some(worker) = &self.icon_worker {
            worker.begin_session(self.helper_session_generation, self.hwnd as isize);
        }
        if let Some(worker) = &self.launch_worker {
            worker.begin_session(self.helper_session_generation, self.hwnd as isize);
        }
        PostMessageW(self.hwnd, WM_REPAINT_RESULTS, 0, 0);
    }

    pub(crate) fn end_helper_focus_session(&mut self) {
        if !self.helper_focus_active {
            return;
        }
        self.helper_focus_active = false;
        self.pending_icon_requests.clear();
        if let Some(worker) = &self.icon_worker {
            worker.end_session(self.helper_session_generation);
        }
        if let Some(worker) = &self.launch_worker {
            worker.end_session(self.helper_session_generation);
        }
        unsafe {
            self.clear_pending_icon_results();
        }
    }

    pub(crate) fn show_helper_error_once(&mut self, helper: &str, error: &str) {
        let reported = if helper == "Icon" {
            &mut self.icon_helper_error_reported
        } else {
            &mut self.shell_helper_error_reported
        };
        if *reported {
            return;
        }
        *reported = true;
        log_helper_error(helper, error);
        show_error(
            self.hwnd,
            &format!(
                "{helper} helper failed. It will not be restarted until the launcher receives focus again.\n\nReason:\n{error}"
            ),
        );
    }

    pub(crate) unsafe fn apply_hotkey(&self) -> bool {
        UnregisterHotKey(self.hwnd, HOTKEY_ID);
        RegisterHotKey(
            self.hwnd,
            HOTKEY_ID,
            self.hotkey.modifiers | MOD_NOREPEAT,
            self.hotkey.key,
        ) != 0
    }

    pub(crate) unsafe fn selected_index(&self) -> Option<usize> {
        if let Some(index) = self.selected_result_index {
            if index < self.result_count().min(self.visible_result_limit()) {
                return Some(index);
            }
        }
        let index = SendMessageW(self.list, LB_GETCURSEL, 0, 0);
        if index < 0 {
            return None;
        }
        let index = index as usize;
        (index < self.result_count().min(self.visible_result_limit())).then_some(index)
    }

    pub(crate) fn default_launch_index(&self) -> Option<usize> {
        first_visible_result_index(self.result_count(), self.visible_result_limit())
    }

    pub(crate) unsafe fn select_result_at_point(&mut self, x: i32, y: i32) -> Option<usize> {
        let hit = SendMessageW(self.list, LB_ITEMFROMPOINT, 0, make_lparam(x, y));
        if hit < 0 || hiword(hit as usize) != 0 {
            return None;
        }
        let index = loword(hit as usize) as usize;
        if index >= self.result_count().min(self.visible_result_limit()) {
            return None;
        }
        self.select_result_index_from_user(index);
        Some(index)
    }

    pub(crate) unsafe fn select_result_index_from_user(&mut self, index: usize) {
        self.freeze_search_due_to_user_selection();
        self.select_result_index(index);
    }

    pub(crate) unsafe fn select_result_index(&mut self, index: usize) {
        let count = SendMessageW(self.list, LB_GETCOUNT, 0, 0);
        if count <= 0 || index >= count as usize {
            return;
        }
        let current = SendMessageW(self.list, LB_GETCURSEL, 0, 0);
        let top_before = self.result_list_top_index();
        let previous = self.selected_result_index;
        SendMessageW(self.list, WM_SETREDRAW, 0, 0);
        self.selected_result_index = Some(index);
        SendMessageW(self.list, LB_SETCURSEL, index, 0);
        SendMessageW(self.list, LB_SETCARETINDEX, index, 0);
        let viewport_changed =
            self.ensure_result_index_visible_from_top(index, count as usize, top_before);
        SendMessageW(self.list, WM_SETREDRAW, TRUE as usize, 0);
        if viewport_changed {
            self.request_result_list_repaint();
        } else {
            for row in [current, previous.map(|value| value as isize).unwrap_or(-1)] {
                self.invalidate_result_row(row);
            }
            self.invalidate_result_row(index as isize);
        }
    }

    pub(crate) unsafe fn ensure_result_index_visible(&self, index: usize, count: usize) -> bool {
        let current_top = self.result_list_top_index();
        self.ensure_result_index_visible_from_top(index, count, current_top)
    }

    pub(crate) unsafe fn ensure_result_index_visible_from_top(
        &self,
        index: usize,
        count: usize,
        current_top: usize,
    ) -> bool {
        if count == 0 {
            SendMessageW(self.list, LB_SETTOPINDEX, 0, 0);
            return true;
        }
        let current_top = current_top.min(count - 1);
        let next_top =
            top_index_after_select(current_top, index, count, self.visible_result_row_count());
        SendMessageW(self.list, LB_SETTOPINDEX, next_top, 0);
        self.result_list_top_index() != current_top
    }

    pub(crate) unsafe fn result_list_top_index(&self) -> usize {
        let top = SendMessageW(self.list, LB_GETTOPINDEX, 0, 0);
        if top < 0 {
            0
        } else {
            top as usize
        }
    }

    pub(crate) unsafe fn request_result_list_repaint(&self) {
        PostMessageW(self.hwnd, WM_REPAINT_RESULTS, 0, 0);
    }

    pub(crate) unsafe fn visible_result_row_count(&self) -> usize {
        let mut rect: RECT = std::mem::zeroed();
        if GetClientRect(self.list, &mut rect) == 0 {
            return 1;
        }
        ((rect.bottom - rect.top).max(RESULT_ROW_HEIGHT) / RESULT_ROW_HEIGHT) as usize
    }

    pub(crate) unsafe fn sync_selected_result_from_list(&mut self) {
        let index = SendMessageW(self.list, LB_GETCURSEL, 0, 0);
        if index < 0 {
            self.selected_result_index = None;
            InvalidateRect(self.list, null(), TRUE);
            return;
        }
        self.select_result_index(index as usize);
    }

    pub(crate) unsafe fn begin_result_drag_at_point(&mut self, x: i32, y: i32) -> bool {
        let Some(index) = self.select_result_at_point(x, y) else {
            self.pending_drag = None;
            return false;
        };
        let Some(path) = self.result_path(index) else {
            self.pending_drag = None;
            return false;
        };
        let title = self
            .results
            .get(index)
            .map(|result| result.title.clone())
            .unwrap_or_else(|| path_display_name(&path));
        self.pending_drag = Some(PendingDrag {
            index,
            path,
            title,
            start_x: x,
            start_y: y,
        });
        true
    }

    pub(crate) unsafe fn take_ready_result_drag(&mut self, x: i32, y: i32) -> Option<PendingDrag> {
        let drag = self.pending_drag.as_ref()?;
        if (x - drag.start_x).abs() < DRAG_START_THRESHOLD
            && (y - drag.start_y).abs() < DRAG_START_THRESHOLD
        {
            return None;
        }
        let drag = self.pending_drag.take()?;
        if drag.index >= self.result_count().min(self.visible_result_limit()) {
            return None;
        }
        Some(drag)
    }

    pub(crate) fn clear_result_drag(&mut self) {
        self.pending_drag = None;
    }

    pub(crate) unsafe fn invalidate_result_row(&self, row: isize) {
        if row < 0 {
            return;
        }
        let mut rect: RECT = std::mem::zeroed();
        if SendMessageW(
            self.list,
            LB_GETITEMRECT,
            row as usize,
            &mut rect as *mut _ as isize,
        ) >= 0
        {
            InvalidateRect(self.list, &rect, 0);
        }
    }

    pub(crate) unsafe fn move_selection(&mut self, delta: i32) {
        let count = SendMessageW(self.list, LB_GETCOUNT, 0, 0);
        if count <= 0 {
            return;
        }

        let current_raw = SendMessageW(self.list, LB_GETCURSEL, 0, 0);
        let current = if current_raw < 0 {
            None
        } else {
            Some(current_raw as usize)
        };
        let Some(next) = selection_after_move(current, count as usize, delta) else {
            return;
        };
        if current == Some(next) {
            SendMessageW(self.list, WM_SETREDRAW, 0, 0);
            if self.ensure_result_index_visible(next, count as usize) {
                SendMessageW(self.list, WM_SETREDRAW, TRUE as usize, 0);
                self.request_result_list_repaint();
            } else {
                SendMessageW(self.list, WM_SETREDRAW, TRUE as usize, 0);
            }
            return;
        }
        self.select_result_index_from_user(next);
    }

    pub(crate) unsafe fn move_selection_by_page(&mut self, direction: i32) {
        let count = SendMessageW(self.list, LB_GETCOUNT, 0, 0);
        if count <= 0 {
            return;
        }
        let current_raw = SendMessageW(self.list, LB_GETCURSEL, 0, 0);
        let current = (current_raw >= 0).then_some(current_raw as usize);
        let current_top = self.result_list_top_index();
        let Some((next, next_top)) = page_selection_after_move(
            current,
            current_top,
            count as usize,
            self.visible_result_row_count(),
            direction,
        ) else {
            return;
        };
        if current == Some(next) && current_top == next_top {
            return;
        }

        self.freeze_search_due_to_user_selection();
        SendMessageW(self.list, WM_SETREDRAW, 0, 0);
        self.selected_result_index = Some(next);
        SendMessageW(self.list, LB_SETCURSEL, next, 0);
        SendMessageW(self.list, LB_SETCARETINDEX, next, 0);
        SendMessageW(self.list, LB_SETTOPINDEX, next_top, 0);
        SendMessageW(self.list, WM_SETREDRAW, TRUE as usize, 0);
        self.request_result_list_repaint();
    }

    pub(crate) unsafe fn select_first_result(&mut self) {
        if SendMessageW(self.list, LB_GETCOUNT, 0, 0) > 0 {
            self.select_result_index_from_user(0);
        }
    }

    pub(crate) unsafe fn select_last_visible_result(&mut self) {
        let count = SendMessageW(self.list, LB_GETCOUNT, 0, 0);
        if count > 0 {
            self.select_result_index_from_user((count - 1) as usize);
        }
    }

    pub(crate) unsafe fn toggle_show_all_results(&mut self) {
        self.last_search_query = get_window_text(self.edit);
        self.show_all_results_override = !self.show_all_results_override;
        self.refresh_results();
    }

    pub(crate) unsafe fn recall_search_history(&mut self, delta: i32) -> bool {
        if self.query_launch_rules.is_empty() {
            return false;
        }
        let next = if let Some(current) = self.query_launch_rule_cursor {
            if delta < 0 {
                current
                    .saturating_add(1)
                    .min(self.query_launch_rules.len() - 1)
            } else {
                current.saturating_sub(1)
            }
        } else {
            0
        };
        let query = self.query_launch_rules[next].query.clone();
        set_edit_text_with_caret_at_end(self.edit, &query);
        self.query_launch_rule_cursor = Some(next);
        self.refresh_results();
        true
    }

    pub(crate) unsafe fn handle_escape_key(&mut self) {
        let query = get_window_text(self.edit);
        if !query.is_empty() {
            let mut start = 0usize;
            let mut end = 0usize;
            SendMessageW(
                self.edit,
                EM_GETSEL,
                &mut start as *mut usize as usize,
                &mut end as *mut usize as isize,
            );
            let total = query.encode_utf16().count();
            if !(start == 0 && end == total) {
                SendMessageW(self.edit, EM_SETSEL, 0, -1);
                SetFocus(self.edit);
                return;
            }
        }
        self.clear_pending_launch();
        self.hide_launcher();
    }

    pub(crate) fn clear_pending_launch(&mut self) {
        self.pending_launch = None;
    }

    pub(crate) unsafe fn resolve_pending_launch(
        &mut self,
        generation: u64,
        received_results: bool,
        search_done: bool,
    ) -> bool {
        let has_results = received_results && self.result_count() > 0;
        let decision = self
            .pending_launch
            .as_ref()
            .map(|request| {
                request.decide(
                    generation,
                    &self.last_search_query,
                    has_results,
                    search_done,
                )
            })
            .unwrap_or(PendingLaunchDecision::Wait);
        match decision {
            PendingLaunchDecision::Wait => false,
            PendingLaunchDecision::Launch => {
                self.clear_pending_launch();
                self.launch_result(0, true);
                true
            }
            PendingLaunchDecision::Clear => {
                self.clear_pending_launch();
                false
            }
        }
    }

    pub(crate) unsafe fn launch_selected(&mut self) {
        let query = get_window_text(self.edit);
        if self.last_search_query == query && !self.search_running {
            let Some(index) = self
                .selected_index()
                .or_else(|| self.default_launch_index())
            else {
                self.clear_pending_launch();
                return;
            };
            self.clear_pending_launch();
            self.launch_result(index, true);
            return;
        }

        if self.last_search_query != query {
            KillTimer(self.hwnd, SEARCH_REFRESH_TIMER_ID);
            self.refresh_results();
        }
        if self.search_running {
            self.pending_launch = Some(PendingLaunchRequest::new(
                self.search_generation,
                self.last_search_query.clone(),
            ));
        }
    }

    pub(crate) unsafe fn launch_result(&mut self, index: usize, hide_after_launch: bool) {
        let Some(result) = self.result_at(index) else {
            return;
        };
        let launched_query = get_window_text(self.edit);
        match result.target {
            LaunchTarget::Path(path) => {
                self.enqueue_launch_request(LaunchWorkerRequest {
                    action: ShellWorkerAction::Launch,
                    session_generation: self.helper_session_generation,
                    generation: self.search_generation,
                    hwnd_value: self.hwnd as isize,
                    title: result.title,
                    path,
                    launched_query,
                });
                if hide_after_launch {
                    self.prepare_for_hidden_launch();
                    self.hide_launcher();
                }
            }
            LaunchTarget::Plugin(target) => {
                let alias = self
                    .plugin_state
                    .alias_for(&target.plugin_id)
                    .unwrap_or_default()
                    .to_string();
                if let Err(error) = self.plugin_supervisor.invoke(
                    &target.plugin_id,
                    self.search_generation,
                    &target.action_token,
                    &alias,
                    self.language,
                ) {
                    self.set_main_status(&localized_format1(
                        self.language,
                        "Plugin error: {}",
                        error,
                    ));
                }
            }
        }
    }

    pub(crate) unsafe fn apply_plugin_events(&mut self) {
        let events = if let Ok(mut pending) = crate::plugin_worker::pending_plugin_events().lock() {
            std::mem::take(&mut *pending)
        } else {
            Vec::new()
        };
        let budget_started = Instant::now();
        let mut deferred_events = Vec::new();
        for event in events {
            if ui_result_budget_exhausted(budget_started) {
                deferred_events.push(event);
                continue;
            }
            match event {
                crate::plugin_worker::PluginEvent::QueryResults {
                    plugin_id,
                    generation,
                    results,
                } if generation == self.search_generation => {
                    self.result_store = None;
                    self.results = results
                        .into_iter()
                        .take(self.search_effective_limit)
                        .map(|result| {
                            crate::plugin_worker::plugin_result_to_search_result(
                                &plugin_id,
                                result,
                                self.show_score_breakdown,
                            )
                        })
                        .collect();
                    self.search_running = false;
                    self.search_stage = SearchStage::Done;
                    if let Some(started) = self.search_started_at.take() {
                        self.last_search_elapsed = Some(started.elapsed());
                    }
                    self.render_results(None);
                    self.update_plugin_textbox();
                    if self.resolve_pending_launch(generation, true, true) {
                        return;
                    }
                }
                crate::plugin_worker::PluginEvent::InvokeResult {
                    plugin_id,
                    generation,
                    next_query: Some(next_query),
                } if generation == self.search_generation
                    && self.plugin_state.alias_for(&plugin_id).is_some() =>
                {
                    set_edit_text_with_caret_at_end(self.edit, &next_query);
                    self.refresh_results();
                }
                crate::plugin_worker::PluginEvent::InvokeResult { .. } => {}
                crate::plugin_worker::PluginEvent::Error {
                    plugin_id,
                    generation,
                    message,
                } if generation == self.search_generation => {
                    self.set_main_status(&localized_format2(
                        self.language,
                        "Plugin {}: {}",
                        plugin_id,
                        message,
                    ));
                }
                _ => {}
            }
        }
        if !deferred_events.is_empty() {
            if let Ok(mut pending) = crate::plugin_worker::pending_plugin_events().lock() {
                deferred_events.append(&mut *pending);
                *pending = deferred_events;
            }
            PostMessageW(self.hwnd, WM_PLUGIN_EVENT_READY, 0, 0);
        }
    }

    pub(crate) fn enqueue_launch_request(&self, request: LaunchWorkerRequest) {
        if let Some(worker) = &self.launch_worker {
            let _ = worker.launch(request);
        }
    }

    pub(crate) unsafe fn apply_launch_worker_results(&mut self) {
        let mut results = Vec::new();
        if let Ok(mut pending) = pending_launch_slot().lock() {
            results.append(&mut *pending);
        }
        for result in results {
            if result.action != ShellWorkerAction::Launch
                && result.generation != self.search_generation
            {
                continue;
            }
            match result.kind {
                LaunchWorkerResultKind::Launched if result.action == ShellWorkerAction::Launch => {
                    if record_query_launch_rule_in_memory(
                        &mut self.query_launch_rules,
                        &result.launched_query,
                        &result.path,
                        &self.plugin_state,
                    ) {
                        self.refresh_query_launch_rules_snapshot();
                        self.enqueue_save_query_rules(self.query_launch_rules.clone());
                        self.sync_open_config_query_launch_rules();
                    }
                    self.query_launch_rule_cursor = None;
                    record_recent_item_with_config(
                        &mut self.recent_items,
                        &result.path,
                        &self.search_scoring,
                    );
                    self.refresh_recent_items_snapshot();
                    self.enqueue_save_recent(self.recent_items.clone());
                    set_window_text(self.edit, "");
                    if IsWindowVisible(self.hwnd) != 0 {
                        self.refresh_results();
                    } else {
                        self.clear_hidden_search_state();
                    }
                }
                LaunchWorkerResultKind::Launched => {}
                LaunchWorkerResultKind::NoLinkedLocation => {
                    self.set_main_status(localized(
                        self.language,
                        "No linked location for this item.",
                    ));
                }
                LaunchWorkerResultKind::MissingShortcutTarget { target } => {
                    let message = localized_format1(
                        self.language,
                        "The item '{}' that this shortcut refers to has been changed or moved, so this shortcut will no longer work properly.\n\nDo you want to delete this shortcut?",
                        target.to_string_lossy(),
                    );
                    let wide_title = wide(APP_NAME);
                    let wide_message = wide(&message);
                    let answer = MessageBoxW(
                        self.hwnd,
                        wide_message.as_ptr(),
                        wide_title.as_ptr(),
                        MB_ICONQUESTION | MB_YESNO,
                    );
                    if answer == IDYES {
                        self.file_task_generation =
                            self.file_task_generation.wrapping_add(1).max(1);
                        if let Some(worker) = &self.save_worker {
                            worker.delete_shortcut(
                                self.file_task_generation,
                                self.hwnd as isize,
                                result.path,
                            );
                        }
                    }
                }
                LaunchWorkerResultKind::HelperError(error) => {
                    if result.session_generation == self.helper_session_generation {
                        self.show_helper_error_once("Shell", &error);
                        if result.action == ShellWorkerAction::Launch {
                            self.show_launcher();
                        }
                    }
                }
                LaunchWorkerResultKind::Error(error) => {
                    if result.action == ShellWorkerAction::Launch {
                        self.show_launch_error(&result.title, &result.path, &error);
                    } else {
                        show_error(
                            self.hwnd,
                            &format!(
                                "Shell operation failed.

Path:
{}

Reason:
{}",
                                result.path.to_string_lossy(),
                                error,
                            ),
                        );
                    }
                }
            }
        }
    }

    pub(crate) fn apply_helper_status_results(&mut self) {
        let results = if let Ok(mut pending) = pending_helper_status_slot().lock() {
            std::mem::take(&mut *pending)
        } else {
            Vec::new()
        };
        for result in results {
            if result.session_generation != self.helper_session_generation {
                continue;
            }
            if let Some(error) = result.error {
                self.show_helper_error_once(result.helper, &error);
            }
        }
    }

    pub(crate) unsafe fn apply_file_task_results(&mut self) {
        let results = if let Ok(mut pending) = pending_file_task_slot().lock() {
            std::mem::take(&mut *pending)
        } else {
            Vec::new()
        };
        for result in results {
            if result.generation != self.file_task_generation {
                continue;
            }
            match result.kind {
                FileTaskResultKind::ShortcutDeleted => {
                    self.items.retain(|item| item.path != result.path);
                    if remove_recent_item(&mut self.recent_items, &result.path) {
                        self.refresh_recent_items_snapshot();
                        self.enqueue_save_recent(self.recent_items.clone());
                    }
                    self.refresh_results();
                }
                FileTaskResultKind::Error(error) => {
                    show_error(
                        self.hwnd,
                        &format!(
                            "Could not delete shortcut.

Reason:
{error}"
                        ),
                    );
                }
            }
        }
    }

    pub(crate) unsafe fn prepare_for_hidden_launch(&mut self) {
        self.clear_hidden_search_state();
        self.plugin_supervisor.cancel_all(self.search_generation);
    }

    unsafe fn clear_hidden_search_state(&mut self) {
        KillTimer(self.hwnd, SEARCH_REFRESH_TIMER_ID);
        self.cancel_active_search();
        self.search_running = false;
        self.search_scanned_total = 0;
        self.search_stage = SearchStage::Idle;
        self.search_started_at = None;
        self.clear_result_source();
        self.selected_result_index = None;
        self.result_tooltip_text.clear();
        self.pending_icon_requests.clear();
        self.hide_app_tooltip();
        self.clear_pending_icon_results();
    }

    unsafe fn clear_pending_icon_results(&self) {
        if let Ok(mut pending) = pending_icon_slot().lock() {
            for result in pending.drain(..) {
                if result.icon_value != 0 {
                    DestroyIcon(result.icon_value as HICON);
                }
            }
        }
    }

    pub(crate) fn enqueue_save_recent(&self, items: Vec<String>) {
        if let Some(worker) = &self.save_worker {
            worker.save_recent(items);
        }
    }

    pub(crate) fn enqueue_save_query_rules(&self, items: Vec<QueryLaunchRule>) {
        if let Some(worker) = &self.save_worker {
            worker.save_query_rules(items);
        }
    }

    pub(crate) unsafe fn cleanup_after_popup_hidden(&mut self) {
        self.cancel_active_search();
        self.cleanup_icon_cache();
        self.clear_result_source();
        self.results = Vec::new();
        self.result_tooltip_text = Vec::new();
        if let Ok(mut pending) = pending_search_slot().lock() {
            *pending = Vec::new();
        }
        self.clear_pending_icon_results();
        if let Ok(mut pending) = crate::plugin_worker::pending_plugin_events().lock() {
            *pending = Vec::new();
        }
    }

    pub(crate) unsafe fn release_inactive_launcher_memory(&mut self) {
        self.plugin_supervisor.cancel_all(self.search_generation);
        self.end_helper_focus_session();
        KillTimer(self.hwnd, SEARCH_REFRESH_TIMER_ID);
        self.cancel_active_search_preserving_result_source();
        self.stop_search_worker();
        self.icon_failures = HashMap::new();
        self.pending_icon_requests = HashMap::new();
        self.hide_app_tooltip();
        if let Ok(mut pending) = pending_search_slot().lock() {
            *pending = Vec::new();
        }
        self.clear_pending_icon_results();
        if let Ok(mut pending) = crate::plugin_worker::pending_plugin_events().lock() {
            *pending = Vec::new();
        }
        let _ = EmptyWorkingSet(GetCurrentProcess());
    }

    pub(crate) unsafe fn sync_open_config_query_launch_rules(&mut self) {
        if self.config_hwnd.is_null() {
            return;
        }
        let runtime_rules = self.query_launch_rules.clone();
        sync_query_launch_rules_model_from_runtime(
            &mut self.config_query_launch_rules,
            &runtime_rules,
        );
        if let Some(snapshot) = self.config_snapshot.as_mut() {
            sync_query_launch_rules_model_from_runtime(
                &mut snapshot.config_query_launch_rules,
                &runtime_rules,
            );
        }
        if self.settings_page == SettingsPage::QueryLaunchRules {
            self.populate_query_launch_rules_list();
        }
    }

    pub(crate) unsafe fn result_context_menu_request(&self) -> Option<ResultContextMenuRequest> {
        let index = self.selected_index()?;
        let result = self.result_at(index)?;
        let from_query_launch_rule = result.from_query_launch_rule;

        Some(ResultContextMenuRequest {
            hwnd: self.hwnd,
            language: self.language,
            index,
            signature: visible_results_signature(std::slice::from_ref(&result), 1)
                .into_iter()
                .next()?,
            from_query_launch_rule,
            source_path: self.result_path(index),
            linked_target: self.result_linked_target(index),
            explore_folder: self.result_explore_folder(index),
            linked_folder: self.result_linked_folder(index),
        })
    }

    pub(crate) unsafe fn track_result_context_menu(
        request: &ResultContextMenuRequest,
        x: i32,
        y: i32,
    ) -> usize {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return 0;
        }

        AppendMenuW(
            menu,
            MF_STRING,
            ID_RESULT_OPEN,
            wide(localized(request.language, "Open")).as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING,
            ID_RESULT_OPEN_KEEP,
            wide(localized(request.language, "Open and keep popup")).as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING,
            ID_RESULT_PROPERTIES,
            wide(localized(request.language, "Properties")).as_ptr(),
        );
        if request.source_path.is_some() {
            AppendMenuW(
                menu,
                MF_STRING,
                ID_RESULT_SHELL_CONTEXT_MENU,
                wide(localized(request.language, "Shell Context Menu...")).as_ptr(),
            );
        }
        AppendMenuW(
            menu,
            MF_STRING,
            ID_RESULT_SCORE_BREAKDOWN,
            wide(localized(request.language, "Score Breakdown...")).as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        if let Some(folder) = request.explore_folder.as_ref() {
            AppendMenuW(
                menu,
                MF_STRING,
                ID_RESULT_OPEN_FOLDER,
                wide(&localized_format1(
                    request.language,
                    "Explore here ({})",
                    folder_menu_path(folder),
                ))
                .as_ptr(),
            );
        }
        if let Some(folder) = request.linked_folder.as_ref() {
            AppendMenuW(
                menu,
                MF_STRING,
                ID_RESULT_EXPLORE_LINKED_LOCATION,
                wide(&localized_format1(
                    request.language,
                    "Explore at linked location ({})",
                    folder_menu_path(folder),
                ))
                .as_ptr(),
            );
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        if let Some(path) = request.source_path.as_ref() {
            AppendMenuW(
                menu,
                MF_STRING,
                ID_RESULT_COPY_SOURCE_PATH,
                wide(&localized_format1(
                    request.language,
                    "Copy source item path ({})",
                    path.to_string_lossy(),
                ))
                .as_ptr(),
            );
        }
        if let Some(path) = request.linked_target.as_ref() {
            AppendMenuW(
                menu,
                MF_STRING,
                ID_RESULT_COPY_TARGET_PATH,
                wide(&localized_format1(
                    request.language,
                    "Copy target item path ({})",
                    path.to_string_lossy(),
                ))
                .as_ptr(),
            );
        }
        AppendMenuW(
            menu,
            MF_STRING,
            ID_RESULT_COPY_ALL,
            wide(localized(request.language, "Copy all result paths")).as_ptr(),
        );
        if request.from_query_launch_rule {
            AppendMenuW(
                menu,
                MF_STRING,
                ID_RESULT_REMOVE_QUERY_LAUNCH_RULE,
                wide(localized(
                    request.language,
                    "Unstar (Remove Query Launch Rule)",
                ))
                .as_ptr(),
            );
        }
        AppendMenuW(
            menu,
            MF_STRING,
            ID_RESULT_REMOVE_RECENT,
            wide(localized(
                request.language,
                "Remove item from recent launch history",
            ))
            .as_ptr(),
        );
        AppendMenuW(
            menu,
            MF_STRING,
            ID_RESULT_SETTINGS,
            wide(localized(request.language, "Settings")).as_ptr(),
        );

        SetForegroundWindow(request.hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            x,
            y,
            0,
            request.hwnd,
            null(),
        );
        PostMessageW(request.hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        command as usize
    }

    pub(crate) unsafe fn execute_result_context_menu_command(
        &mut self,
        request: &ResultContextMenuRequest,
        command: usize,
    ) {
        let index = request.index;
        let current_signature = self.result_signature(index);
        if current_signature.as_ref() != Some(&request.signature) {
            self.set_main_status(&self.default_status_text());
            return;
        }
        match command {
            ID_RESULT_OPEN => self.launch_result(index, true),
            ID_RESULT_OPEN_KEEP => self.launch_result(index, false),
            ID_RESULT_OPEN_FOLDER => self.open_result_folder(index),
            ID_RESULT_EXPLORE_LINKED_LOCATION => self.open_result_linked_location(index),
            ID_RESULT_PROPERTIES => self.open_result_properties(index),
            ID_RESULT_SHELL_CONTEXT_MENU => self.open_result_shell_context_menu(index),
            ID_RESULT_SCORE_BREAKDOWN => self.show_score_breakdown(index),
            ID_RESULT_COPY_SOURCE_PATH => self.copy_result_source_path(index),
            ID_RESULT_COPY_TARGET_PATH => self.copy_result_linked_target_path(index),
            ID_RESULT_COPY_ALL => self.copy_all_result_paths(),
            ID_RESULT_REMOVE_QUERY_LAUNCH_RULE => self.remove_result_query_launch_rule(index),
            ID_RESULT_REMOVE_RECENT => self.remove_result_from_recent(index),
            ID_RESULT_SETTINGS => self.show_config_window(),
            _ => {}
        }
    }

    pub(crate) unsafe fn open_selected_target_folder(&mut self) {
        self.clear_pending_launch();
        let Some(index) = self.selected_index() else {
            return;
        };
        let Some(path) = self.result_path(index) else {
            return;
        };
        if is_shortcut_file(&path) {
            self.open_result_linked_location(index);
        } else {
            self.open_result_folder(index);
        }
    }

    pub(crate) unsafe fn open_result_folder(&mut self, index: usize) {
        let Some(path) = self.result_explore_folder(index) else {
            return;
        };
        self.enqueue_shell_action(ShellWorkerAction::OpenFolder, path);
    }

    pub(crate) unsafe fn open_result_linked_location(&mut self, index: usize) {
        let Some(path) = self.result_path(index) else {
            return;
        };
        self.enqueue_shell_action(ShellWorkerAction::OpenLinkedLocation, path);
    }

    pub(crate) unsafe fn open_result_properties(&mut self, index: usize) {
        let Some(path) = self.result_path(index) else {
            return;
        };
        self.enqueue_shell_action(ShellWorkerAction::Properties, path);
    }

    pub(crate) unsafe fn open_result_shell_context_menu(&mut self, index: usize) {
        let Some(path) = self.result_path(index) else {
            return;
        };
        self.enqueue_shell_action(ShellWorkerAction::ContextMenu, path);
    }

    fn enqueue_shell_action(&self, action: ShellWorkerAction, path: PathBuf) {
        self.enqueue_launch_request(LaunchWorkerRequest {
            action,
            session_generation: self.helper_session_generation,
            generation: self.search_generation,
            hwnd_value: self.hwnd as isize,
            title: String::new(),
            path,
            launched_query: String::new(),
        });
    }

    pub(crate) unsafe fn copy_result_source_path(&self, index: usize) {
        let Some(path) = self.result_path(index) else {
            return;
        };
        let text = path.to_string_lossy();
        if set_clipboard_text(self.hwnd, &text) {
            self.set_main_status(localized(self.language, "Copied source item path."));
        }
    }

    pub(crate) unsafe fn copy_result_linked_target_path(&self, index: usize) {
        let Some(path) = self.result_linked_target(index) else {
            return;
        };
        let text = path.to_string_lossy();
        if set_clipboard_text(self.hwnd, &text) {
            self.set_main_status(localized(self.language, "Copied target item path."));
        }
    }

    pub(crate) unsafe fn copy_all_result_paths(&self) {
        let text = self
            .results
            .iter()
            .take(self.visible_result_limit())
            .filter_map(result_target_text)
            .collect::<Vec<_>>()
            .join("\r\n");
        if !text.is_empty() && set_clipboard_text(self.hwnd, &text) {
            self.set_main_status(localized(self.language, "Copied all result paths."));
        }
    }

    pub(crate) unsafe fn remove_result_from_recent(&mut self, index: usize) {
        let Some(path) = self.result_path(index) else {
            return;
        };
        if remove_recent_item(&mut self.recent_items, &path) {
            self.refresh_recent_items_snapshot();
            self.enqueue_save_recent(self.recent_items.clone());
        }
        self.refresh_results();
        self.set_main_status(localized(
            self.language,
            "Removed from recent launch history.",
        ));
    }

    pub(crate) unsafe fn remove_result_query_launch_rule(&mut self, index: usize) {
        let Some(path) = self.result_path(index) else {
            return;
        };
        let query = get_window_text(self.edit);
        let removed = remove_query_launch_rule(&mut self.query_launch_rules, &query, &path);
        if removed {
            self.refresh_query_launch_rules_snapshot();
            self.enqueue_save_query_rules(self.query_launch_rules.clone());
            self.sync_open_config_query_launch_rules();
            self.query_launch_rule_cursor = None;
        }
        self.refresh_results();
        self.set_main_status(localized(
            self.language,
            if removed {
                "Removed Query Launch Rule."
            } else {
                "No matching Query Launch Rule was found."
            },
        ));
    }

    pub(crate) fn result_path(&self, index: usize) -> Option<PathBuf> {
        self.result_at(index)
            .and_then(|result| result_target_path(&result))
    }

    pub(crate) fn result_explore_folder(&self, index: usize) -> Option<PathBuf> {
        let path = self.result_path(index)?;
        if path.is_dir() {
            Some(path)
        } else {
            path.parent().map(Path::to_path_buf).or(Some(path))
        }
    }

    pub(crate) fn result_linked_folder(&self, index: usize) -> Option<PathBuf> {
        let path = self.result_path(index)?;
        linked_target_folder(&path)
    }

    pub(crate) fn result_linked_target(&self, index: usize) -> Option<PathBuf> {
        let path = self.result_path(index)?;
        resolve_shortcut_target(&path)
    }

    pub(crate) fn result_target_text(&self, index: usize) -> Option<String> {
        self.result_at(index)
            .and_then(|result| result_target_text(&result))
    }

    pub(crate) unsafe fn show_launch_error(&mut self, title: &str, path: &Path, error: &str) {
        show_error(
            self.hwnd,
            &localized_format3(
                self.language,
                "Could not launch:\n{}\n\nPath:\n{}\n\nReason:\n{}",
                title,
                path.to_string_lossy(),
                error,
            ),
        );
        self.show_launcher();
    }

    pub(crate) unsafe fn show_launcher(&mut self) {
        self.ensure_search_worker_running();
        ShowWindow(self.hwnd, SW_RESTORE);
        ShowWindow(self.hwnd, SW_SHOW);
        SetWindowPos(
            self.hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        SetWindowPos(
            self.hwnd,
            HWND_NOTOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        BringWindowToTop(self.hwnd);
        self.resize_controls();
        play_popup_sound(self.popup_sound);
        SetForegroundWindow(self.hwnd);
        SetFocus(self.edit);
        self.begin_helper_focus_session();
        SendMessageW(self.edit, EM_SETSEL, 0, -1);
        PostMessageW(self.hwnd, WM_REFRESH_RESULTS, 0, 0);
    }

    pub(crate) unsafe fn hide_launcher(&self) {
        KillTimer(self.hwnd, HELPER_FOCUS_GRACE_TIMER_ID);
        ShowWindow(self.hwnd, SW_HIDE);
        PostMessageW(self.hwnd, WM_END_HELPER_SESSION, 0, 0);
    }

    pub(crate) unsafe fn toggle_launcher(&mut self) {
        if IsWindowVisible(self.hwnd) != 0 && GetForegroundWindow() == self.hwnd {
            self.hide_launcher();
        } else {
            self.show_launcher();
        }
    }

    fn enqueue_settings_process(&self, page: Option<SettingsPage>) {
        if let Some(worker) = &self.save_worker {
            worker.run_task(move || {
                if FLASH_LAUNCH_SHUTDOWN_REQUESTED.load(Ordering::Acquire) {
                    return;
                }
                let _ = launch_settings_process(page);
            });
        }
    }

    pub(crate) unsafe fn show_config_window_for_page(&mut self, page: SettingsPage) {
        if !self.settings_process_mode {
            self.enqueue_settings_process(Some(page));
            return;
        }
        self.settings_page = page;
        if !self.config_hwnd.is_null() {
            self.apply_config_page();
            ShowWindow(self.config_hwnd, SW_RESTORE);
            ShowWindow(self.config_hwnd, SW_SHOW);
            SetForegroundWindow(self.config_hwnd);
            return;
        }
        self.show_config_window();
    }

    pub(crate) unsafe fn show_config_window(&mut self) {
        if !self.settings_process_mode {
            self.enqueue_settings_process(None);
            return;
        }
        if !self.config_hwnd.is_null() {
            ShowWindow(self.config_hwnd, SW_RESTORE);
            ShowWindow(self.config_hwnd, SW_SHOW);
            SetForegroundWindow(self.config_hwnd);
            return;
        }

        let class_name = wide(CONFIG_CLASS_NAME);
        let title = wide(&settings_window_title(self.language));
        let settings = load_config_window_settings();
        let hwnd = CreateWindowExW(
            WS_EX_APPWINDOW,
            class_name.as_ptr(),
            title.as_ptr(),
            window_style_no_minimize() | WS_VSCROLL | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            settings.x,
            settings.y,
            settings.width,
            settings.height,
            null_mut(),
            null_mut(),
            self.instance,
            null(),
        );

        if hwnd.is_null() {
            show_error(
                self.hwnd,
                localized(self.language, "Could not create settings window."),
            );
            return;
        }

        self.config_hwnd = hwnd;
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, self.app_icon as isize);
        SendMessageW(
            hwnd,
            WM_SETICON,
            ICON_SMALL as usize,
            self.app_icon_small as isize,
        );
        self.create_config_controls();
        self.populate_config_window();
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
    }

    pub(crate) unsafe fn show_modifier_keyword_help(&self) {
        self.show_modifier_keyword_help_for_owner(self.config_hwnd);
    }

    pub(crate) unsafe fn show_modifier_keyword_help_for_owner(&self, owner: HWND) {
        let content = localized(self.language, MODIFIER_KEYWORD_HELP_TEXT).replace('\n', "\r\n");
        show_help_form(
            owner,
            localized(self.language, "Modifier Keywords"),
            &content,
            self.language,
        );
    }

    pub(crate) unsafe fn show_plugin_help(&self) {
        show_help_form(
            null_mut(),
            APP_NAME,
            &search_bar_help_text(&self.plugin_state, self.language),
            self.language,
        );
    }

    pub(crate) unsafe fn show_add_index_window(&mut self) {
        if !self.add_index_hwnd.is_null() {
            ShowWindow(self.add_index_hwnd, SW_RESTORE);
            ShowWindow(self.add_index_hwnd, SW_SHOW);
            SetForegroundWindow(self.add_index_hwnd);
            SetFocus(self.add_path);
            return;
        }

        let settings = load_add_index_window_settings();
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            wide(ADD_INDEX_CLASS_NAME).as_ptr(),
            wide(localized(self.language, "Add Search Folder")).as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_THICKFRAME,
            settings.x,
            settings.y,
            settings.width,
            settings.height,
            self.config_hwnd,
            null_mut(),
            self.instance,
            null(),
        );

        if hwnd.is_null() {
            show_error(
                self.config_hwnd,
                localized(self.language, "Could not create Add Search Folder window."),
            );
            return;
        }

        self.add_index_hwnd = hwnd;
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, self.app_icon as isize);
        SendMessageW(
            hwnd,
            WM_SETICON,
            ICON_SMALL as usize,
            self.app_icon_small as isize,
        );
        self.create_add_index_controls();
        self.install_numeric_edit_subclasses();
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
    }

    pub(crate) unsafe fn create_add_index_controls(&mut self) {
        let static_class = wide("STATIC");
        let edit_class = wide("EDIT");
        let button_class = wide("BUTTON");

        for (id, text) in [
            (
                ID_ADD_PATH_LABEL,
                localized(self.language, "Directory Path (type % for aliases)"),
            ),
            (ID_ADD_SCORE_LABEL, localized(self.language, "Score")),
            (ID_ADD_DEPTH_LABEL, localized(self.language, "Depth")),
            (ID_ADD_ALIAS_HELP, ""),
            (
                ID_ADD_KEYWORDS_LABEL,
                localized(
                    self.language,
                    "Modifier keywords ([Blank] = no +keyword, * = any +keyword, -keyword = exclude)",
                ),
            ),
        ] {
            CreateWindowExW(
                0,
                static_class.as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | SS_LEFT,
                0,
                0,
                0,
                0,
                self.add_index_hwnd,
                id as isize as _,
                self.instance,
                null(),
            );
        }

        self.add_modifier_help_button = CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide(localized(self.language, "Modifier guide")).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            0,
            0,
            0,
            self.add_index_hwnd,
            ID_ADD_MODIFIER_HELP_BUTTON as isize as _,
            self.instance,
            null(),
        );

        self.add_path = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            wide("COMBOBOX").as_ptr(),
            null(),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | CBS_DROPDOWN as u32
                | CBS_AUTOHSCROLL as u32
                | WS_VSCROLL,
            0,
            0,
            0,
            0,
            self.add_index_hwnd,
            ID_ADD_PATH as isize as _,
            self.instance,
            null(),
        );
        self.populate_add_alias_combo();
        self.add_score = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            edit_class.as_ptr(),
            wide("100").as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            0,
            0,
            0,
            0,
            self.add_index_hwnd,
            ID_ADD_SCORE as isize as _,
            self.instance,
            null(),
        );
        self.add_depth = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            edit_class.as_ptr(),
            wide("-1").as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            0,
            0,
            0,
            0,
            self.add_index_hwnd,
            ID_ADD_DEPTH as isize as _,
            self.instance,
            null(),
        );
        self.add_keywords = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            edit_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            0,
            0,
            0,
            0,
            self.add_index_hwnd,
            ID_ADD_KEYWORDS as isize as _,
            self.instance,
            null(),
        );

        for (id, text) in [
            (ID_ADD_BROWSE, localized(self.language, "Browse...")),
            (ID_ADD_OK, localized(self.language, "Add")),
            (ID_ADD_CANCEL, localized(self.language, "Cancel")),
        ] {
            CreateWindowExW(
                0,
                button_class.as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
                0,
                0,
                0,
                0,
                self.add_index_hwnd,
                id as isize as _,
                self.instance,
                null(),
            );
        }

        self.add_enabled = CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide(localized(self.language, "Use this search folder")).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            0,
            0,
            0,
            0,
            self.add_index_hwnd,
            ID_ADD_ENABLED as isize as _,
            self.instance,
            null(),
        );
        SendMessageW(self.add_enabled, BM_SETCHECK, BST_CHECKED as usize, 0);

        self.add_status = CreateWindowExW(
            0,
            static_class.as_ptr(),
            wide(localized(
                self.language,
                "Add creates a new row. Edit selected rows in Settings; Save persists changes.",
            ))
            .as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            self.add_index_hwnd,
            ID_ADD_STATUS as isize as _,
            self.instance,
            null(),
        );

        let font = GetStockObject(DEFAULT_GUI_FONT);
        for child in [
            GetDlgItem(self.add_index_hwnd, ID_ADD_PATH_LABEL),
            GetDlgItem(self.add_index_hwnd, ID_ADD_ALIAS_HELP),
            GetDlgItem(self.add_index_hwnd, ID_ADD_SCORE_LABEL),
            GetDlgItem(self.add_index_hwnd, ID_ADD_DEPTH_LABEL),
            GetDlgItem(self.add_index_hwnd, ID_ADD_KEYWORDS_LABEL),
            self.add_modifier_help_button,
            self.add_path,
            self.add_score,
            self.add_depth,
            self.add_keywords,
            self.add_enabled,
            GetDlgItem(self.add_index_hwnd, ID_ADD_BROWSE),
            GetDlgItem(self.add_index_hwnd, ID_ADD_OK),
            GetDlgItem(self.add_index_hwnd, ID_ADD_CANCEL),
            self.add_status,
        ] {
            if !child.is_null() {
                SendMessageW(child, WM_SETFONT, font as usize, TRUE as isize);
            }
        }
        self.resize_add_index_controls();
        SetFocus(self.add_path);
    }

    pub(crate) unsafe fn populate_add_alias_combo(&self) {
        for alias in path_aliases() {
            let text = wide(&alias_display_text(alias));
            SendMessageW(self.add_path, CB_ADDSTRING, 0, text.as_ptr() as isize);
        }
        self.update_add_alias_hint();
    }

    pub(crate) unsafe fn update_add_alias_hint(&self) {
        let value = get_window_text(self.add_path);
        let alias_name = alias_name_from_combo_text(&value);
        let hint = path_aliases()
            .iter()
            .find(|alias| alias.name.eq_ignore_ascii_case(alias_name.trim()))
            .and_then(|alias| {
                alias_path(alias).map(|path| {
                    localized_format2(
                        self.language,
                        "{} resolves to: {}",
                        alias.name,
                        path.to_string_lossy(),
                    )
                })
            })
            .unwrap_or_else(|| {
                localized(
                    self.language,
                    "Choose an alias or type a path. Alias rows show current machine path.",
                )
                .to_string()
            });
        set_window_text(GetDlgItem(self.add_index_hwnd, ID_ADD_ALIAS_HELP), &hint);
    }

    pub(crate) unsafe fn select_add_alias(&self) {
        let index = SendMessageW(self.add_path, CB_GETCURSEL, 0, 0);
        if index < 0 {
            return;
        }
        let length = SendMessageW(self.add_path, CB_GETLBTEXTLEN, index as usize, 0);
        if length <= 0 {
            return;
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let copied = SendMessageW(
            self.add_path,
            CB_GETLBTEXT,
            index as usize,
            buffer.as_mut_ptr() as isize,
        );
        if copied < 0 {
            return;
        }
        let selected_text = OsString::from_wide(&buffer[..copied as usize])
            .to_string_lossy()
            .to_string();
        let alias = alias_name_from_combo_text(&selected_text);
        if path_aliases()
            .iter()
            .any(|known_alias| known_alias.name.eq_ignore_ascii_case(&alias))
        {
            set_combo_text_with_caret_at_end(self.add_path, &alias);
            self.update_add_alias_hint();
        }
    }

    pub(crate) unsafe fn resize_add_index_controls(&self) {
        if self.add_index_hwnd.is_null() {
            return;
        }
        let mut rect: RECT = std::mem::zeroed();
        GetClientRect(self.add_index_hwnd, &mut rect);
        let width = rect.right - rect.left;
        let pad = 14;
        let label_h = 20;
        let edit_h = 30;
        let button_h = 32;
        let browse_w = 100;
        let small_w = 90;
        let y1 = pad;
        let y2 = y1 + label_h;
        let y_alias = y2 + edit_h + 2;
        let y3 = y_alias + label_h + pad;
        let y4 = y3 + label_h;
        let y7 = y4 + edit_h + pad;
        let y8 = y7 + label_h;
        let y9 = y8 + edit_h + pad;
        let y10 = y9 + button_h + 8;

        MoveWindow(
            GetDlgItem(self.add_index_hwnd, ID_ADD_PATH_LABEL),
            pad,
            y1,
            (width - pad * 2).max(180),
            label_h,
            TRUE,
        );
        MoveWindow(
            self.add_path,
            pad,
            y2,
            (width - pad * 2 - browse_w - 8).max(160),
            240,
            TRUE,
        );
        MoveWindow(
            GetDlgItem(self.add_index_hwnd, ID_ADD_BROWSE),
            width - pad - browse_w,
            y2,
            browse_w,
            edit_h,
            TRUE,
        );
        MoveWindow(
            GetDlgItem(self.add_index_hwnd, ID_ADD_ALIAS_HELP),
            pad,
            y_alias,
            (width - pad * 2).max(160),
            label_h,
            TRUE,
        );

        MoveWindow(
            GetDlgItem(self.add_index_hwnd, ID_ADD_SCORE_LABEL),
            pad,
            y3,
            small_w,
            label_h,
            TRUE,
        );
        MoveWindow(self.add_score, pad, y4, small_w, edit_h, TRUE);
        MoveWindow(
            GetDlgItem(self.add_index_hwnd, ID_ADD_DEPTH_LABEL),
            pad + small_w + 10,
            y3,
            small_w,
            label_h,
            TRUE,
        );
        MoveWindow(
            self.add_depth,
            pad + small_w + 10,
            y4,
            small_w,
            edit_h,
            TRUE,
        );

        MoveWindow(
            GetDlgItem(self.add_index_hwnd, ID_ADD_KEYWORDS_LABEL),
            pad,
            y7,
            (width - pad * 2).max(160),
            label_h,
            TRUE,
        );
        MoveWindow(
            self.add_keywords,
            pad,
            y8,
            (width - pad * 2).max(160),
            edit_h,
            TRUE,
        );

        let gap = 8;
        let enabled_w = 170;
        let help_w = 142;
        let action_w = 90;
        MoveWindow(self.add_enabled, pad, y9, enabled_w, button_h, TRUE);
        MoveWindow(
            self.add_modifier_help_button,
            pad + enabled_w + gap,
            y9,
            help_w,
            button_h,
            TRUE,
        );
        MoveWindow(
            GetDlgItem(self.add_index_hwnd, ID_ADD_OK),
            width - pad - action_w * 2 - gap,
            y9,
            action_w,
            button_h,
            TRUE,
        );
        MoveWindow(
            GetDlgItem(self.add_index_hwnd, ID_ADD_CANCEL),
            width - pad - action_w,
            y9,
            action_w,
            button_h,
            TRUE,
        );
        MoveWindow(
            self.add_status,
            pad,
            y10,
            (width - pad * 2).max(160),
            label_h,
            TRUE,
        );
    }

    pub(crate) unsafe fn browse_add_index_folder(&self) {
        if let Some(path) = browse_for_folder(self.add_index_hwnd, self.language) {
            set_window_text(self.add_path, &path.to_string_lossy());
        }
    }

    pub(crate) unsafe fn add_index_entry_from_dialog(&mut self) {
        let path = alias_name_from_combo_text(&get_window_text(self.add_path))
            .trim()
            .to_string();
        if path.is_empty() {
            show_error(
                self.add_index_hwnd,
                localized(self.language, "Directory path is required."),
            );
            return;
        }

        let score_text = get_window_text(self.add_score);
        if !NumericEditRule::signed_integer().final_allowed(&score_text) {
            show_error(
                self.add_index_hwnd,
                localized(self.language, "Score must be an integer."),
            );
            return;
        }
        let depth_text = get_window_text(self.add_depth);
        if !NumericEditRule::depth().final_allowed(&depth_text) {
            show_error(
                self.add_index_hwnd,
                localized(self.language, "Depth must be -1 or a non-negative integer."),
            );
            return;
        }

        let root = IndexRoot {
            raw: path,
            path: None,
            enabled: SendMessageW(self.add_enabled, BM_GETCHECK, 0, 0) as u32 == BST_CHECKED,
            score: score_text.trim().parse::<i32>().unwrap_or(100),
            max_depth: depth_text
                .trim()
                .parse::<isize>()
                .ok()
                .map(normalize_search_depth)
                .unwrap_or(SEARCH_DEPTH_ALL),
            label: String::new(),
            keywords: normalize_keywords(&get_window_text(self.add_keywords)),
        };

        self.config_roots.push(normalized_index_root(root));
        self.populate_index_list();
        select_list_view_item(self.cfg_index, self.config_roots.len().saturating_sub(1));
        self.load_selected_index_entry();
        set_window_text(
            self.cfg_status,
            &settings_model::status_message(self.language, "Entry added"),
        );
        save_add_index_window_settings(self.add_index_hwnd);
        ShowWindow(self.add_index_hwnd, SW_HIDE);
    }

    pub(crate) unsafe fn create_scoring_rules_controls_for_parent(&mut self, parent: HWND) {
        let static_class = wide("STATIC");
        let _edit_class = wide("EDIT");
        let button_class = wide("BUTTON");

        let (id, text) = (ID_SCORING_LIST_LABEL, localized(self.language, "Rules"));
        CreateWindowExW(
            0,
            static_class.as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            parent,
            id as isize as _,
            self.instance,
            null(),
        );

        self.scoring_list = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            WC_LISTVIEWW,
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | LVS_REPORT | LVS_SHOWSELALWAYS,
            0,
            0,
            0,
            0,
            parent,
            ID_SCORING_LIST as isize as _,
            self.instance,
            null(),
        );
        configure_report_list_view(self.scoring_list);
        reset_list_view_columns(
            self.scoring_list,
            &settings_model::column_defs_for_page(SettingsPage::HeuristicScoring, self.language),
        );
        for (id, text) in [
            (
                ID_SCORING_ENABLED,
                localized(self.language, "Enable / Disable"),
            ),
            (ID_SCORING_ADD, localized(self.language, "Add Pattern")),
            (ID_SCORING_DELETE, localized(self.language, "Delete")),
            (ID_SCORING_MOVE_UP, localized(self.language, "Move Up")),
            (ID_SCORING_MOVE_DOWN, localized(self.language, "Move Down")),
            (ID_SCORING_RESET, localized(self.language, "Reset Default")),
        ] {
            CreateWindowExW(
                0,
                button_class.as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
                0,
                0,
                0,
                0,
                parent,
                id as isize as _,
                self.instance,
                null(),
            );
        }
        self.scoring_status = CreateWindowExW(
            0,
            static_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            parent,
            ID_SCORING_STATUS as isize as _,
            self.instance,
            null(),
        );

        let font = GetStockObject(DEFAULT_GUI_FONT);
        for child in [
            GetDlgItem(parent, ID_SCORING_LIST_LABEL),
            self.scoring_list,
            GetDlgItem(parent, ID_SCORING_ENABLED),
            GetDlgItem(parent, ID_SCORING_ADD),
            GetDlgItem(parent, ID_SCORING_DELETE),
            GetDlgItem(parent, ID_SCORING_MOVE_UP),
            GetDlgItem(parent, ID_SCORING_MOVE_DOWN),
            GetDlgItem(parent, ID_SCORING_RESET),
            self.scoring_status,
        ] {
            if !child.is_null() {
                SendMessageW(child, WM_SETFONT, font as usize, TRUE as isize);
            }
        }
        self.ensure_fonts();
        SendMessageW(
            GetDlgItem(self.config_hwnd, ID_CFG_SAVE),
            WM_SETFONT,
            self.title_font as usize,
            TRUE as isize,
        );
    }

    pub(crate) unsafe fn populate_scoring_list_for_page(&mut self, page: SettingsPage) {
        self.loading_scoring_entry = true;
        let kind = match page {
            SettingsPage::PatternScoring => ScoringRuleKind::Pattern,
            _ => ScoringRuleKind::Heuristic,
        };
        let model = settings_model::ScoringModel {
            rules: &mut self.scoring_rules,
            kind,
            language: self.language,
        };
        settings_model::populate_list_from_model(
            self.scoring_list,
            &model,
            self.language,
            true,
            false,
        );
        if list_view_item_count(self.scoring_list) > 0 {
            select_list_view_item(self.scoring_list, 0);
            self.loading_scoring_entry = false;
            self.load_selected_scoring_rule();
        } else {
            self.loading_scoring_entry = false;
        }
    }

    pub(crate) unsafe fn selected_scoring_rule_index(&self) -> Option<usize> {
        let selected = list_view_selected_index(self.scoring_list);
        if selected < 0 {
            return None;
        }
        self.scoring_rule_index_from_visible_index(selected)
    }

    pub(crate) fn scoring_rule_index_from_visible_index(&self, visible: i32) -> Option<usize> {
        if visible < 0 {
            return None;
        }
        let wanted_kind = match self.settings_page {
            SettingsPage::HeuristicScoring => Some(ScoringRuleKind::Heuristic),
            SettingsPage::PatternScoring => Some(ScoringRuleKind::Pattern),
            _ => None,
        };
        let mut visible_index = 0usize;
        for (index, rule) in self.scoring_rules.iter().enumerate() {
            if wanted_kind.is_none_or(|kind| rule.kind == kind) {
                if visible_index == visible as usize {
                    return Some(index);
                }
                visible_index += 1;
            }
        }
        None
    }

    pub(crate) unsafe fn load_selected_scoring_rule(&mut self) {
        let Some(index) = self.selected_scoring_rule_index() else {
            return;
        };
        let rule = &self.scoring_rules[index];
        self.loading_scoring_entry = true;
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_SCORING_ENABLED),
            if rule.enabled {
                localized(self.language, "Disable")
            } else {
                localized(self.language, "Enable")
            },
        );
        self.loading_scoring_entry = false;
    }

    pub(crate) unsafe fn add_scoring_pattern_rule(&mut self) {
        let rule = ScoringRuleEntry {
            kind: ScoringRuleKind::Pattern,
            key: "*.ext".to_string(),
            value: "0".to_string(),
            modifiers: Vec::new(),
            enabled: true,
        };
        self.scoring_rules.push(rule);
        self.populate_scoring_list_for_page(SettingsPage::PatternScoring);
        let index = list_view_item_count(self.scoring_list).saturating_sub(1) as usize;
        select_list_view_item(self.scoring_list, index);
        self.load_selected_scoring_rule();
        set_window_text(
            self.scoring_status,
            localized(self.language, "Pattern added. Edit it, then Save + Apply."),
        );
    }

    pub(crate) unsafe fn delete_selected_scoring_rule(&mut self) {
        let table = self.current_settings_table();
        let selected_indices = table.selected_indices();
        if selected_indices.is_empty() {
            return;
        }
        let deleted = {
            let mut count = 0usize;
            self.with_list_model_mut(table.list, |model| {
                if !model.supports_delete() {
                    return;
                }
                for &index in selected_indices.iter().rev() {
                    if model.delete_row(index) {
                        count += 1;
                    }
                }
            });
            count
        };
        if deleted > 0 {
            self.populate_scoring_list_for_page(self.settings_page);
            table.restore_selection(&selected_indices);
            table.set_status(&settings_model::status_message(
                self.language,
                "Pattern deleted",
            ));
        } else {
            table.set_status(localized(
                self.language,
                "Built-in heuristic rules cannot be deleted; disable or edit value instead.",
            ));
        }
    }

    pub(crate) unsafe fn move_selected_scoring_rule(&mut self, delta: i32) {
        let Some(old) = self.selected_scoring_rule_index() else {
            return;
        };
        let indexes: Vec<usize> = self
            .scoring_rules
            .iter()
            .enumerate()
            .filter_map(|(index, rule)| match self.settings_page {
                SettingsPage::HeuristicScoring if rule.kind == ScoringRuleKind::Heuristic => {
                    Some(index)
                }
                SettingsPage::PatternScoring if rule.kind == ScoringRuleKind::Pattern => {
                    Some(index)
                }
                _ => None,
            })
            .collect();
        let Some(visible_old) = indexes.iter().position(|index| *index == old) else {
            return;
        };
        let visible_new = (visible_old as i32 + delta).clamp(0, indexes.len() as i32 - 1) as usize;
        let new = indexes[visible_new];
        if new == old {
            return;
        }
        self.scoring_rules.swap(old, new);
        self.populate_scoring_list_for_page(self.settings_page);
        select_list_view_item(self.scoring_list, visible_new);
        self.load_selected_scoring_rule();
        set_window_text(
            self.scoring_status,
            &settings_model::status_message(self.language, "Rule moved"),
        );
    }

    pub(crate) unsafe fn reset_selected_scoring_rule_to_default(&mut self) {
        self.reset_selected_current_table_rows_to_default();
    }

    pub(crate) unsafe fn create_config_controls(&mut self) {
        let static_class = wide("STATIC");
        let edit_class = wide("EDIT");
        let list_class = wide("LISTBOX");
        let combo_class = wide("COMBOBOX");
        let button_class = wide("BUTTON");

        self.cfg_nav = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            list_class.as_ptr(),
            null(),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | LBS_NOTIFY as u32
                | LBS_NOINTEGRALHEIGHT as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_NAV as isize as _,
            self.instance,
            null(),
        );
        SetWindowSubclass(self.cfg_nav, Some(settings_nav_subclass_proc), 4, 0);

        for item in settings_nav_labels(self.language) {
            let text = wide(item);
            SendMessageW(self.cfg_nav, LB_ADDSTRING, 0, text.as_ptr() as isize);
        }
        SendMessageW(
            self.cfg_nav,
            LB_SETCURSEL,
            config_nav_index_for_page(self.settings_page),
            0,
        );

        CreateWindowExW(
            0,
            static_class.as_ptr(),
            wide(localized(self.language, "Directories are searched in the order listed, so put small directories and Start Menu entries near the top. Use Add New / Move buttons to rearrange; Space or Enable / Disable temporarily toggles items.")).as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_DESCRIPTION as isize as _,
            self.instance,
            null(),
        );

        for (id, text) in [
            (ID_CFG_HEADER_DIR, localized(self.language, "Directory")),
            (
                ID_CFG_HEADER_MODIFIER,
                localized(self.language, "Modifier Keywords"),
            ),
            (ID_CFG_HEADER_SCORE, localized(self.language, "Score")),
            (
                ID_CFG_TIP,
                localized(
                    self.language,
                    "TIP: Right-click / buttons to add, edit, delete and move items.",
                ),
            ),
        ] {
            CreateWindowExW(
                0,
                static_class.as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | SS_LEFT,
                0,
                0,
                0,
                0,
                self.config_hwnd,
                id as isize as _,
                self.instance,
                null(),
            );
        }

        CreateWindowExW(
            0,
            static_class.as_ptr(),
            wide(localized(self.language, "Popup hotkey")).as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_HOTKEY_LABEL as isize as _,
            self.instance,
            null(),
        );

        self.cfg_hotkey = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            edit_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32 | ES_READONLY as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_HOTKEY as isize as _,
            self.instance,
            null(),
        );
        CreateWindowExW(
            0,
            static_class.as_ptr(),
            wide(localized(self.language, "Popup result count (min 9)")).as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_RESULT_LIMIT_LABEL as isize as _,
            self.instance,
            null(),
        );

        self.cfg_result_limit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            edit_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32 | ES_NUMBER as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_RESULT_LIMIT as isize as _,
            self.instance,
            null(),
        );

        CreateWindowExW(
            0,
            static_class.as_ptr(),
            wide(localized(self.language, "Search threads")).as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_SEARCH_THREADS_LABEL as isize as _,
            self.instance,
            null(),
        );

        self.cfg_search_thread_mode = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            combo_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | CBS_DROPDOWNLIST as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_SEARCH_THREAD_MODE as isize as _,
            self.instance,
            null(),
        );

        self.cfg_search_threads = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            edit_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32 | ES_NUMBER as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_SEARCH_THREADS as isize as _,
            self.instance,
            null(),
        );

        CreateWindowExW(
            0,
            static_class.as_ptr(),
            wide(localized(self.language, "Language")).as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_LANGUAGE_LABEL as isize as _,
            self.instance,
            null(),
        );

        self.cfg_language = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            combo_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_LANGUAGE as isize as _,
            self.instance,
            null(),
        );
        self.refresh_language_combo();

        self.cfg_sound = CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide(localized(self.language, "Sound")).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_SOUND as isize as _,
            self.instance,
            null(),
        );

        self.cfg_score_breakdown = CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide(localized(self.language, "Show score breakdown in results")).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_SCORE_BREAKDOWN as isize as _,
            self.instance,
            null(),
        );
        self.cfg_score_breakdown_tooltip = CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide(localized(self.language, "Show score breakdown tooltip")).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_SCORE_BREAKDOWN_TOOLTIP as isize as _,
            self.instance,
            null(),
        );

        CreateWindowExW(
            0,
            static_class.as_ptr(),
            wide(localized(self.language, "Tooltip opacity")).as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_TOOLTIP_OPACITY_LABEL as isize as _,
            self.instance,
            null(),
        );
        self.cfg_tooltip_opacity = CreateWindowExW(
            0,
            TRACKBAR_CLASSW,
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | TBS_AUTOTICKS,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_TOOLTIP_OPACITY as isize as _,
            self.instance,
            null(),
        );
        SendMessageW(
            self.cfg_tooltip_opacity,
            TBM_SETRANGE,
            TRUE as usize,
            ((MAX_TOOLTIP_OPACITY_PERCENT as isize) << 16) | MIN_TOOLTIP_OPACITY_PERCENT as isize,
        );
        SendMessageW(self.cfg_tooltip_opacity, TBM_SETTICFREQ, 10, 0);
        self.cfg_tooltip_opacity_value = CreateWindowExW(
            0,
            static_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_TOOLTIP_OPACITY_VALUE as isize as _,
            self.instance,
            null(),
        );

        self.cfg_show_cpu_in_title = CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide(localized(self.language, "Show CPU in window title")).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_SHOW_CPU_IN_TITLE as isize as _,
            self.instance,
            null(),
        );
        self.cfg_show_ram_in_title = CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide(localized(self.language, "Show RAM in window title")).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_SHOW_RAM_IN_TITLE as isize as _,
            self.instance,
            null(),
        );
        self.cfg_show_build_timestamp_in_title = CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide(localized(self.language, "Show build timestamp in window title")).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_SHOW_BUILD_TIMESTAMP_IN_TITLE as isize as _,
            self.instance,
            null(),
        );
        self.cfg_autostart = CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide(localized(self.language, "Start with Windows")).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX as u32,
            0, 0, 0, 0,
            self.config_hwnd,
            ID_CFG_AUTOSTART as isize as _,
            self.instance,
            null(),
        );
        SendMessageW(
            self.cfg_autostart,
            BM_SETCHECK,
            if load_app_settings_snapshot().autostart { BST_CHECKED as usize } else { 0 },
            0,
        );
        CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide(localized(self.language, "Reset Defaults")).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_GENERAL_RESET as isize as _,
            self.instance,
            null(),
        );

        CreateWindowExW(
            0,
            static_class.as_ptr(),
            wide(localized(
                self.language,
                "Search folders (select entry to edit; changes apply here immediately, Save persists)",
            ))
            .as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_INDEX_LABEL as isize as _,
            self.instance,
            null(),
        );

        self.cfg_index = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            WC_LISTVIEWW,
            null(),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WS_VSCROLL
                | LVS_REPORT
                | LVS_SINGLESEL
                | LVS_SHOWSELALWAYS,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_INDEX as isize as _,
            self.instance,
            null(),
        );
        configure_report_list_view_with_options(self.cfg_index, true, false);
        SetWindowSubclass(
            self.cfg_index,
            Some(settings_tooltip_subclass_proc),
            SETTINGS_TOOLTIP_SUBCLASS_ID,
            0,
        );
        reset_list_view_columns(
            self.cfg_index,
            &settings_model::column_defs_for_page(SettingsPage::SearchFolders, self.language),
        );
        self.cfg_modifier_help_button = CreateWindowExW(
            0,
            button_class.as_ptr(),
            wide(localized(self.language, "Modifier guide")).as_ptr(),
            WS_CHILD | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_MODIFIER_HELP_BUTTON as isize as _,
            self.instance,
            null(),
        );

        CreateWindowExW(
            0,
            static_class.as_ptr(),
            wide(localized(self.language, "Filter")).as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_QUERY_LAUNCH_FILTER_LABEL as isize as _,
            self.instance,
            null(),
        );

        self.cfg_query_launch_filter = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            edit_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_QUERY_LAUNCH_FILTER as isize as _,
            self.instance,
            null(),
        );

        self.cfg_query_launch_count = CreateWindowExW(
            0,
            static_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_QUERY_LAUNCH_COUNT as isize as _,
            self.instance,
            null(),
        );

        for (id, text) in [
            (ID_CFG_APPLY_ENTRY, localized(self.language, "Add New")),
            (
                ID_CFG_TOGGLE_ENTRY,
                localized(self.language, "Enable / Disable"),
            ),
            (
                ID_CFG_RESET_DEFAULT,
                localized(self.language, "Reset Default"),
            ),
            (ID_CFG_DELETE_ENTRY, localized(self.language, "Delete")),
            (ID_CFG_MOVE_UP, localized(self.language, "Move Up")),
            (ID_CFG_MOVE_DOWN, localized(self.language, "Move Down")),
            (
                ID_CFG_CLEAR_HISTORY,
                localized(self.language, "Clear History"),
            ),
            (ID_CFG_SAVE, localized(self.language, "Save + Apply")),
            (
                ID_CFG_OPEN_FOLDER,
                localized(self.language, "Open Config Folder"),
            ),
            (ID_CFG_CLOSE, localized(self.language, "Close")),
        ] {
            CreateWindowExW(
                0,
                button_class.as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
                0,
                0,
                0,
                0,
                self.config_hwnd,
                id as isize as _,
                self.instance,
                null(),
            );
        }

        self.cfg_status = CreateWindowExW(
            0,
            static_class.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            0,
            0,
            0,
            0,
            self.config_hwnd,
            ID_CFG_STATUS as isize as _,
            self.instance,
            null(),
        );

        let font = GetStockObject(DEFAULT_GUI_FONT);
        for child in [
            self.cfg_nav,
            GetDlgItem(self.config_hwnd, ID_CFG_DESCRIPTION),
            GetDlgItem(self.config_hwnd, ID_CFG_HEADER_DIR),
            GetDlgItem(self.config_hwnd, ID_CFG_HEADER_MODIFIER),
            GetDlgItem(self.config_hwnd, ID_CFG_HEADER_SCORE),
            GetDlgItem(self.config_hwnd, ID_CFG_TIP),
            GetDlgItem(self.config_hwnd, ID_CFG_HOTKEY_LABEL),
            self.cfg_hotkey,
            GetDlgItem(self.config_hwnd, ID_CFG_RESULT_LIMIT_LABEL),
            self.cfg_result_limit,
            GetDlgItem(self.config_hwnd, ID_CFG_SEARCH_THREADS_LABEL),
            self.cfg_search_thread_mode,
            self.cfg_search_threads,
            GetDlgItem(self.config_hwnd, ID_CFG_LANGUAGE_LABEL),
            self.cfg_language,
            self.cfg_sound,
            self.cfg_score_breakdown,
            self.cfg_score_breakdown_tooltip,
            GetDlgItem(self.config_hwnd, ID_CFG_TOOLTIP_OPACITY_LABEL),
            self.cfg_tooltip_opacity,
            self.cfg_tooltip_opacity_value,
            self.cfg_show_cpu_in_title,
            self.cfg_show_ram_in_title,
            self.cfg_show_build_timestamp_in_title,
            self.cfg_autostart,
            GetDlgItem(self.config_hwnd, ID_CFG_GENERAL_RESET),
            GetDlgItem(self.config_hwnd, ID_CFG_INDEX_LABEL),
            self.cfg_index,
            self.cfg_modifier_help_button,
            GetDlgItem(self.config_hwnd, ID_CFG_QUERY_LAUNCH_FILTER_LABEL),
            self.cfg_query_launch_filter,
            self.cfg_query_launch_count,
            GetDlgItem(self.config_hwnd, ID_CFG_APPLY_ENTRY),
            GetDlgItem(self.config_hwnd, ID_CFG_TOGGLE_ENTRY),
            GetDlgItem(self.config_hwnd, ID_CFG_RESET_DEFAULT),
            GetDlgItem(self.config_hwnd, ID_CFG_DELETE_ENTRY),
            GetDlgItem(self.config_hwnd, ID_CFG_MOVE_UP),
            GetDlgItem(self.config_hwnd, ID_CFG_MOVE_DOWN),
            GetDlgItem(self.config_hwnd, ID_CFG_CLEAR_HISTORY),
            GetDlgItem(self.config_hwnd, ID_CFG_SAVE),
            GetDlgItem(self.config_hwnd, ID_CFG_OPEN_FOLDER),
            GetDlgItem(self.config_hwnd, ID_CFG_CLOSE),
            self.cfg_status,
        ] {
            if !child.is_null() {
                SendMessageW(child, WM_SETFONT, font as usize, TRUE as isize);
            }
        }
        self.ensure_fonts();
        self.ensure_setting_help_font();
        SendMessageW(
            GetDlgItem(self.config_hwnd, ID_CFG_SAVE),
            WM_SETFONT,
            self.title_font as usize,
            TRUE as isize,
        );
        self.create_scoring_rules_controls_for_parent(self.config_hwnd);
        self.install_numeric_edit_subclasses();
        self.resize_config_controls();
    }

    pub(crate) unsafe fn current_config_snapshot_from_gui(&self) -> Option<ConfigSnapshot> {
        let hotkey = self
            .pending_hotkey
            .clone()
            .or_else(|| parse_hotkey(get_window_text(self.cfg_hotkey).trim()))?;
        let result_limit_text = get_window_text(self.cfg_result_limit);
        let result_limit_rule = NumericEditRule::unsigned_integer_min(MIN_RESULT_LIMIT as i64);
        if !result_limit_rule.final_allowed(&result_limit_text) {
            return None;
        }
        let result_limit = result_limit_text.trim().parse::<usize>().ok()?;
        let search_thread_mode_index =
            SendMessageW(self.cfg_search_thread_mode, CB_GETCURSEL, 0, 0) as i32;
        let search_threads = match search_thread_mode_index {
            1 => {
                let search_threads_text = get_window_text(self.cfg_search_threads);
                let search_threads_rule = NumericEditRule::strict_unsigned_integer(
                    MIN_SEARCH_THREADS as i64,
                    available_search_threads() as i64,
                );
                if !search_threads_rule.final_allowed(&search_threads_text) {
                    return None;
                }
                SearchThreadMode::Manual(search_threads_text.trim().parse::<usize>().ok()?)
            }
            2 => SearchThreadMode::Maximum,
            _ => SearchThreadMode::Auto,
        };
        Some(ConfigSnapshot {
            hotkey,
            result_limit,
            search_threads,
            language: AppLanguage::from_combo_index(SendMessageW(
                self.cfg_language,
                CB_GETCURSEL,
                0,
                0,
            ) as i32),
            popup_sound: SendMessageW(self.cfg_sound, BM_GETCHECK, 0, 0) as u32 == BST_CHECKED,
            show_score_breakdown: SendMessageW(self.cfg_score_breakdown, BM_GETCHECK, 0, 0) as u32
                == BST_CHECKED,
            show_score_breakdown_tooltip: SendMessageW(
                self.cfg_score_breakdown_tooltip,
                BM_GETCHECK,
                0,
                0,
            ) as u32
                == BST_CHECKED,
            tooltip_opacity_percent: (SendMessageW(self.cfg_tooltip_opacity, TBM_GETPOS, 0, 0)
                as i32)
                .clamp(
                    MIN_TOOLTIP_OPACITY_PERCENT as i32,
                    MAX_TOOLTIP_OPACITY_PERCENT as i32,
                ) as u8,
            show_cpu_in_title: SendMessageW(self.cfg_show_cpu_in_title, BM_GETCHECK, 0, 0) as u32
                == BST_CHECKED,
            show_ram_in_title: SendMessageW(self.cfg_show_ram_in_title, BM_GETCHECK, 0, 0) as u32
                == BST_CHECKED,
            show_build_timestamp_in_title: SendMessageW(
                self.cfg_show_build_timestamp_in_title,
                BM_GETCHECK,
                0,
                0,
            ) as u32
                == BST_CHECKED,
            autostart: SendMessageW(self.cfg_autostart, BM_GETCHECK, 0, 0) as u32 == BST_CHECKED,
            config_roots: self.config_roots.clone(),
            scoring_rules: self.scoring_rules.clone(),
            config_recent_items: self.config_recent_items.clone(),
            config_search_history: self.config_search_history.clone(),
            config_query_launch_rules: self.config_query_launch_rules.clone(),
            config_plugin_aliases: self.config_plugin_aliases.clone(),
        })
    }

    pub(crate) unsafe fn config_has_unsaved_changes(&self) -> bool {
        match (
            &self.config_snapshot,
            self.current_config_snapshot_from_gui(),
        ) {
            (Some(saved), Some(current)) => saved != &current,
            (Some(_), None) => true,
            (None, _) => true,
        }
    }

    pub(crate) unsafe fn restore_config_snapshot(&mut self) {
        let Some(snapshot) = self.config_snapshot.clone() else {
            return;
        };
        self.pending_hotkey = None;
        self.hotkey = snapshot.hotkey.clone();
        self.config_roots = snapshot.config_roots;
        self.scoring_rules = snapshot.scoring_rules;
        self.config_recent_items = snapshot.config_recent_items;
        self.config_search_history = snapshot.config_search_history;
        self.config_query_launch_rules = snapshot.config_query_launch_rules;
        self.config_plugin_aliases = snapshot.config_plugin_aliases;
        self.language = snapshot.language;
        self.result_limit = snapshot.result_limit;
        self.popup_sound = snapshot.popup_sound;
        self.show_score_breakdown_tooltip = snapshot.show_score_breakdown_tooltip;
        self.tooltip_opacity_percent = snapshot.tooltip_opacity_percent;
        self.show_cpu_in_title = snapshot.show_cpu_in_title;
        self.show_ram_in_title = snapshot.show_ram_in_title;
        self.show_build_timestamp_in_title = snapshot.show_build_timestamp_in_title;
        self.loading_config_entry = true;
        set_window_text(self.cfg_hotkey, &self.hotkey.display);
        set_window_text(self.cfg_result_limit, &snapshot.result_limit.to_string());
        self.refresh_search_thread_controls(snapshot.search_threads);
        self.refresh_language_combo();
        SendMessageW(
            self.cfg_sound,
            BM_SETCHECK,
            if snapshot.popup_sound {
                BST_CHECKED as usize
            } else {
                0
            },
            0,
        );
        SendMessageW(
            self.cfg_score_breakdown,
            BM_SETCHECK,
            if snapshot.show_score_breakdown {
                BST_CHECKED as usize
            } else {
                0
            },
            0,
        );
        SendMessageW(
            self.cfg_score_breakdown_tooltip,
            BM_SETCHECK,
            if snapshot.show_score_breakdown_tooltip {
                BST_CHECKED as usize
            } else {
                0
            },
            0,
        );
        self.apply_score_breakdown_tooltip_controls_visibility();
        SendMessageW(
            self.cfg_tooltip_opacity,
            TBM_SETPOS,
            TRUE as usize,
            snapshot.tooltip_opacity_percent as isize,
        );
        self.update_tooltip_opacity_value_label();
        for (control, checked) in [
            (self.cfg_show_cpu_in_title, snapshot.show_cpu_in_title),
            (self.cfg_show_ram_in_title, snapshot.show_ram_in_title),
            (
                self.cfg_show_build_timestamp_in_title,
                snapshot.show_build_timestamp_in_title,
            ),
            (self.cfg_autostart, snapshot.autostart),
        ] {
            SendMessageW(
                control,
                BM_SETCHECK,
                if checked { BST_CHECKED as usize } else { 0 },
                0,
            );
        }
        self.loading_config_entry = false;
        self.apply_config_page();
    }

    pub(crate) fn validate_config_models(&self) -> Result<(), &'static str> {
        if self
            .config_roots
            .iter()
            .any(|root| root.raw.trim().is_empty())
        {
            return Err("Search Folder path cannot be blank.");
        }
        if self
            .config_query_launch_rules
            .iter()
            .any(|entry| entry.query.trim().is_empty() || entry.target.trim().is_empty())
        {
            return Err("Query Launch Rules contain an invalid entry.");
        }
        for entry in &self.scoring_rules {
            match entry.kind {
                ScoringRuleKind::Heuristic => {
                    if !scoring_rule_value_is_valid(&entry.key, &entry.value) {
                        return Err(
                            "Heuristic Scoring values must be integers; fuzzy weights and thresholds cannot be negative.",
                        );
                    }
                }
                ScoringRuleKind::Pattern => {
                    if entry.key.trim().is_empty() {
                        return Err("Pattern Scoring pattern cannot be blank.");
                    }
                    if entry.value.trim().parse::<i32>().is_err() {
                        return Err("Pattern Scoring values must be integers.");
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) unsafe fn populate_config_window(&mut self) {
        self.config_reload_generation = self.config_reload_generation.wrapping_add(1).max(1);
        self.reload_in_progress = true;
        EnableWindow(GetDlgItem(self.config_hwnd, ID_CFG_SAVE), FALSE);
        set_window_text(
            self.cfg_status,
            localized(self.language, "Loading config..."),
        );
        if let Some(worker) = &self.save_worker {
            worker.reload_config(
                self.config_reload_generation,
                self.config_hwnd as isize,
                true,
            );
        }
    }

    unsafe fn apply_settings_window_reload(
        &mut self,
        full: FullConfigReload,
        search_roots: Vec<IndexRoot>,
        search_scoring: ScoringConfig,
    ) {
        let settings = full.settings;
        self.reload_in_progress = false;
        self.config_roots = search_roots;
        self.search_scoring = search_scoring;
        self.refresh_search_config_snapshots();
        self.config_recent_items = full.config_recent_items;
        self.config_search_history = full.search_history.clone();
        self.config_query_launch_rules = full.query_launch_rules;
        self.query_launch_rule_filter.clear();
        self.query_launch_rule_visible_indices.clear();
        self.plugin_state = full.plugin_state;
        self.config_plugin_aliases = self.plugin_state.alias_entries();
        self.scoring_rules = full.scoring_rules;
        self.search_history = full.search_history;
        self.query_launch_rules = self.config_query_launch_rules.clone();
        self.recent_items = full.recent_items;
        self.loading_config_entry = true;
        set_window_text(self.cfg_query_launch_filter, "");
        self.hotkey = settings.hotkey.clone();
        set_window_text(self.cfg_hotkey, &self.hotkey.display);
        set_window_text(self.cfg_result_limit, &settings.result_limit.to_string());
        self.search_threads = settings.search_threads;
        self.refresh_search_thread_controls(self.search_threads);
        self.language = settings.language;
        self.popup_sound = settings.popup_sound;
        self.tooltip_opacity_percent = settings.tooltip_opacity_percent;
        SendMessageW(
            self.cfg_tooltip_opacity,
            TBM_SETPOS,
            TRUE as usize,
            self.tooltip_opacity_percent as isize,
        );
        self.update_tooltip_opacity_value_label();
        self.apply_app_tooltip_opacity();
        self.show_cpu_in_title = settings.show_cpu_in_title;
        self.show_ram_in_title = settings.show_ram_in_title;
        self.show_build_timestamp_in_title = settings.show_build_timestamp_in_title;
        for (control, checked) in [
            (self.cfg_show_cpu_in_title, settings.show_cpu_in_title),
            (self.cfg_show_ram_in_title, settings.show_ram_in_title),
            (
                self.cfg_show_build_timestamp_in_title,
                settings.show_build_timestamp_in_title,
            ),
            (self.cfg_autostart, settings.autostart),
        ] {
            SendMessageW(
                control,
                BM_SETCHECK,
                if checked { BST_CHECKED as usize } else { 0 },
                0,
            );
        }
        self.refresh_language_combo();
        SendMessageW(
            self.cfg_sound,
            BM_SETCHECK,
            if settings.popup_sound {
                BST_CHECKED as usize
            } else {
                0
            },
            0,
        );
        SendMessageW(
            self.cfg_score_breakdown,
            BM_SETCHECK,
            if settings.show_score_breakdown {
                BST_CHECKED as usize
            } else {
                0
            },
            0,
        );
        SendMessageW(
            self.cfg_score_breakdown_tooltip,
            BM_SETCHECK,
            if settings.show_score_breakdown_tooltip {
                BST_CHECKED as usize
            } else {
                0
            },
            0,
        );
        self.apply_score_breakdown_tooltip_controls_visibility();
        self.loading_config_entry = false;
        self.populate_index_list();
        self.apply_config_page();
        self.config_snapshot = Some(ConfigSnapshot {
            hotkey: self.hotkey.clone(),
            result_limit: settings.result_limit,
            search_threads: settings.search_threads,
            language: self.language,
            popup_sound: settings.popup_sound,
            show_score_breakdown: settings.show_score_breakdown,
            show_score_breakdown_tooltip: settings.show_score_breakdown_tooltip,
            tooltip_opacity_percent: self.tooltip_opacity_percent,
            show_cpu_in_title: settings.show_cpu_in_title,
            show_ram_in_title: settings.show_ram_in_title,
            show_build_timestamp_in_title: settings.show_build_timestamp_in_title,
            autostart: settings.autostart,
            config_roots: self.config_roots.clone(),
            scoring_rules: self.scoring_rules.clone(),
            config_recent_items: self.config_recent_items.clone(),
            config_search_history: self.config_search_history.clone(),
            config_query_launch_rules: self.config_query_launch_rules.clone(),
            config_plugin_aliases: self.config_plugin_aliases.clone(),
        });
        EnableWindow(GetDlgItem(self.config_hwnd, ID_CFG_SAVE), TRUE);
        set_window_text(
            self.cfg_status,
            &format!(
                "{}: {}",
                localized(self.language, "Settings folder"),
                app_config_dir().to_string_lossy()
            ),
        );
    }

    pub(crate) unsafe fn populate_index_list(&mut self) {
        self.loading_config_entry = true;
        let model = settings_model::SearchFoldersModel {
            roots: &mut self.config_roots,
        };
        settings_model::populate_list_from_model(self.cfg_index, &model, self.language, true, true);
        if list_view_item_count(self.cfg_index) > 0 {
            select_list_view_item(self.cfg_index, 0);
            self.loading_config_entry = false;
            self.load_selected_index_entry();
        } else {
            self.loading_config_entry = false;
        }
    }
    pub(crate) unsafe fn populate_query_launch_rules_list(&mut self) {
        self.loading_config_entry = true;
        self.query_launch_rule_visible_indices = query_launch_rule_visible_indices(
            &self.config_query_launch_rules,
            &self.query_launch_rule_filter,
        );

        configure_report_list_view_with_options(self.cfg_index, false, false);
        clear_list_view(self.cfg_index);
        reset_list_view_columns(
            self.cfg_index,
            &settings_model::column_defs_for_page(SettingsPage::QueryLaunchRules, self.language),
        );
        let row_style = settings_model::encode_cell_styles([
            settings_model::CellStyle::ReadonlyDefault,
            settings_model::CellStyle::ReadonlyDefault,
        ]);
        for &actual_index in &self.query_launch_rule_visible_indices {
            if let Some(rule) = self.config_query_launch_rules.get(actual_index) {
                let columns = [rule.query.as_str(), rule.target.as_str()];
                add_list_view_row_with_param(self.cfg_index, true, &columns, row_style);
            }
        }
        self.loading_config_entry = false;
        self.update_query_launch_rules_selection_count();
    }

    pub(crate) unsafe fn populate_plugin_alias_list(&mut self) {
        self.loading_config_entry = true;
        let model = settings_model::PluginAliasModel {
            entries: &mut self.config_plugin_aliases,
        };
        settings_model::populate_list_from_model(
            self.cfg_index,
            &model,
            self.language,
            false,
            false,
        );
        if list_view_item_count(self.cfg_index) > 0 {
            select_list_view_item(self.cfg_index, 0);
            self.loading_config_entry = false;
            self.load_selected_plugin_alias_entry();
        } else {
            self.loading_config_entry = false;
        }
    }

    pub(crate) unsafe fn load_selected_index_entry(&mut self) {
        let index = list_view_selected_index(self.cfg_index);
        if index < 0 {
            return;
        }
        let Some(_root) = self.config_roots.get(index as usize) else {
            return;
        };
        self.loading_config_entry = true;
        self.loading_config_entry = false;
    }
    pub(crate) unsafe fn load_selected_query_launch_rule_entry(&mut self) {
        let index = list_view_selected_index(self.cfg_index);
        if index < 0 {
            return;
        }
        let Some(actual_index) = self.query_launch_rule_actual_index_from_visible(index as usize)
        else {
            return;
        };
        let Some(_entry) = self.config_query_launch_rules.get(actual_index) else {
            return;
        };
        self.loading_config_entry = true;
        self.loading_config_entry = false;
    }

    pub(crate) fn query_launch_rule_actual_index_from_visible(
        &self,
        visible_index: usize,
    ) -> Option<usize> {
        self.query_launch_rule_visible_indices
            .get(visible_index)
            .copied()
    }

    pub(crate) unsafe fn apply_query_launch_rules_filter(&mut self) {
        if self.settings_page != SettingsPage::QueryLaunchRules {
            return;
        }
        self.query_launch_rule_filter = get_window_text(self.cfg_query_launch_filter);
        self.populate_query_launch_rules_list();
    }

    pub(crate) unsafe fn update_query_launch_rules_selection_count(&self) {
        if self.cfg_query_launch_count.is_null() {
            return;
        }
        let selected = if self.settings_page == SettingsPage::QueryLaunchRules {
            list_view_selected_indices(self.cfg_index).len()
        } else {
            0
        };
        let showing = if self.settings_page == SettingsPage::QueryLaunchRules {
            list_view_item_count(self.cfg_index).max(0) as usize
        } else {
            0
        };
        set_window_text(
            self.cfg_query_launch_count,
            &query_launch_rule_selection_count_text(
                self.language,
                selected,
                showing,
                self.config_query_launch_rules.len(),
            ),
        );
    }

    pub(crate) unsafe fn load_selected_plugin_alias_entry(&mut self) {
        let index = list_view_selected_index(self.cfg_index);
        if index < 0 {
            return;
        }
        let Some(_entry) = self.config_plugin_aliases.get(index as usize) else {
            return;
        };
        self.loading_config_entry = true;
        self.loading_config_entry = false;
    }

    pub(crate) unsafe fn show_config_id(&mut self, id: i32, visible: bool) {
        let hwnd = GetDlgItem(self.config_hwnd, id);
        self.show_config_hwnd(hwnd, visible);
    }

    pub(crate) unsafe fn show_config_hwnd(&mut self, hwnd: HWND, visible: bool) {
        if hwnd.is_null() {
            return;
        }
        if self.config_redraw_transaction_depth > 0
            && !self.config_redraw_visibility_touched.contains(&hwnd)
        {
            self.config_redraw_visibility_touched.push(hwnd);
        }
        ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
    }

    pub(crate) unsafe fn begin_config_redraw_transaction(&mut self) {
        if self.config_hwnd.is_null() {
            return;
        }
        if self.config_redraw_transaction_depth == 0 {
            self.config_redraw_children.clear();
            self.config_redraw_visibility_touched.clear();
            SendMessageW(self.config_hwnd, WM_SETREDRAW, 0, 0);
            EnumChildWindows(
                self.config_hwnd,
                Some(suspend_config_child_redraw),
                &mut self.config_redraw_children as *mut _ as LPARAM,
            );
        }
        self.config_redraw_transaction_depth += 1;
    }

    pub(crate) unsafe fn end_config_redraw_transaction(&mut self) {
        if self.config_redraw_transaction_depth == 0 {
            return;
        }
        self.config_redraw_transaction_depth -= 1;
        if self.config_redraw_transaction_depth == 0 && !self.config_hwnd.is_null() {
            for &(hwnd, was_visible) in &self.config_redraw_children {
                if IsWindow(hwnd) == 0 {
                    continue;
                }
                let visible = config_child_visibility_after_redraw(
                    was_visible,
                    self.config_redraw_visibility_touched.contains(&hwnd),
                    window_has_visible_style(hwnd),
                );
                SendMessageW(hwnd, WM_SETREDRAW, TRUE as usize, 0);
                ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
            }
            self.config_redraw_children.clear();
            self.config_redraw_visibility_touched.clear();
            SendMessageW(self.config_hwnd, WM_SETREDRAW, TRUE as usize, 0);
            self.request_config_repaint();
        }
    }

    pub(crate) unsafe fn request_config_repaint(&self) {
        if !self.config_hwnd.is_null() {
            PostMessageW(self.config_hwnd, WM_REPAINT_SETTINGS, 0, 0);
        }
    }

    pub(crate) unsafe fn resize_config_controls(&mut self) {
        if self.config_hwnd.is_null() {
            return;
        }
        let mut rect: RECT = std::mem::zeroed();
        GetClientRect(self.config_hwnd, &mut rect);
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        let pad = 8;
        let gap = 8;
        let nav_w = settings_nav_labels(self.language)
            .iter()
            .map(|label| control_text_width(self.cfg_nav, label) + 24)
            .max()
            .unwrap_or(180)
            .clamp(180, (width - 400).clamp(180, 320));
        let button_h = 32;
        let label_h = 22;
        let edit_h = 28;
        let status_h = 24;
        let right_x = pad + nav_w + gap;
        let right_w = (width - right_x - pad).max(360);
        let action_buttons = [
            (ID_CFG_SAVE, SETTINGS_ACTION_BUTTON_WIDTH),
            (ID_CFG_OPEN_FOLDER, SETTINGS_ACTION_BUTTON_WIDTH),
            (ID_CFG_CLOSE, SETTINGS_ACTION_BUTTON_WIDTH),
        ];
        let action_h = self.button_rows_height(&action_buttons, right_w, button_h, gap);
        let action_top = (height - pad - action_h).max(pad + 300);
        let status_top = action_top - status_h - 4;
        let page_bottom = status_top - gap;

        self.config_content_height = height.max(420);
        self.config_scroll_y = 0;
        let mut scroll_info: SCROLLINFO = std::mem::zeroed();
        scroll_info.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
        scroll_info.fMask = SIF_RANGE | SIF_PAGE | SIF_POS;
        scroll_info.nMin = 0;
        scroll_info.nMax = 0;
        scroll_info.nPage = height.max(0) as u32;
        scroll_info.nPos = 0;
        SetScrollInfo(self.config_hwnd, SB_VERT, &scroll_info, TRUE);

        MoveWindow(
            self.cfg_nav,
            pad,
            pad,
            nav_w,
            (height - pad * 2).max(240),
            TRUE,
        );
        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_DESCRIPTION),
            right_x,
            pad,
            right_w,
            44,
            TRUE,
        );

        for id in [
            ID_CFG_HEADER_DIR,
            ID_CFG_HEADER_MODIFIER,
            ID_CFG_HEADER_SCORE,
            ID_CFG_TIP,
            ID_CFG_HOTKEY_LABEL,
            ID_CFG_RESULT_LIMIT_LABEL,
            ID_CFG_SEARCH_THREADS_LABEL,
            ID_CFG_LANGUAGE_LABEL,
            ID_CFG_TOOLTIP_OPACITY_LABEL,
            ID_CFG_TOOLTIP_OPACITY_VALUE,
            ID_CFG_GENERAL_RESET,
            ID_CFG_INDEX_LABEL,
            ID_CFG_QUERY_LAUNCH_FILTER_LABEL,
            ID_CFG_QUERY_LAUNCH_COUNT,
            ID_CFG_APPLY_ENTRY,
            ID_CFG_TOGGLE_ENTRY,
            ID_CFG_RESET_DEFAULT,
            ID_CFG_DELETE_ENTRY,
            ID_CFG_MOVE_UP,
            ID_CFG_MOVE_DOWN,
            ID_CFG_CLEAR_HISTORY,
            ID_SCORING_ENABLED,
            ID_SCORING_LIST_LABEL,
            ID_SCORING_ADD,
            ID_SCORING_DELETE,
            ID_SCORING_MOVE_UP,
            ID_SCORING_MOVE_DOWN,
            ID_SCORING_RESET,
        ] {
            self.show_config_id(id, false);
        }
        for hwnd in [
            self.cfg_hotkey,
            self.cfg_result_limit,
            self.cfg_search_thread_mode,
            self.cfg_search_threads,
            self.cfg_language,
            self.cfg_sound,
            self.cfg_score_breakdown,
            self.cfg_score_breakdown_tooltip,
            self.cfg_tooltip_opacity,
            self.cfg_tooltip_opacity_value,
            self.cfg_show_cpu_in_title,
            self.cfg_show_ram_in_title,
            self.cfg_show_build_timestamp_in_title,
            self.cfg_autostart,
            self.cfg_index,
            self.cfg_query_launch_filter,
            self.cfg_query_launch_count,
            self.cfg_modifier_help_button,
            self.scoring_list,
            self.scoring_status,
        ] {
            self.show_config_hwnd(hwnd, false);
        }

        for (id, _) in action_buttons {
            let child = GetDlgItem(self.config_hwnd, id);
            self.show_config_hwnd(child, true);
        }
        self.layout_button_row(right_x, action_top, right_w, button_h, gap, &action_buttons);
        MoveWindow(
            self.cfg_status,
            right_x,
            status_top,
            right_w,
            status_h,
            TRUE,
        );

        let layout = ConfigLayoutMetrics {
            right_x,
            right_w,
            pad,
            page_bottom,
            label_h,
            edit_h,
            button_h,
            gap,
        };

        match self.settings_page {
            SettingsPage::SearchFolders => {
                let index_label_y = pad + 50;
                let list_top = index_label_y + label_h + 2;
                let tip_top = page_bottom - label_h;
                let entry_buttons = [
                    (ID_CFG_APPLY_ENTRY, SETTINGS_ACTION_BUTTON_WIDTH),
                    (ID_CFG_TOGGLE_ENTRY, SETTINGS_ACTION_BUTTON_WIDTH),
                    (ID_CFG_RESET_DEFAULT, SETTINGS_ACTION_BUTTON_WIDTH),
                    (ID_CFG_DELETE_ENTRY, SETTINGS_ACTION_BUTTON_WIDTH),
                    (ID_CFG_MOVE_UP, SETTINGS_ACTION_BUTTON_WIDTH),
                    (ID_CFG_MOVE_DOWN, SETTINGS_ACTION_BUTTON_WIDTH),
                    (ID_CFG_MODIFIER_HELP_BUTTON, SETTINGS_ACTION_BUTTON_WIDTH),
                ];
                let entry_buttons_h =
                    self.button_rows_height(&entry_buttons, right_w, button_h, gap);
                let entry_button_top = tip_top - entry_buttons_h - gap;
                let list_h = (entry_button_top - list_top - gap).max(120);

                for id in [
                    ID_CFG_INDEX_LABEL,
                    ID_CFG_TIP,
                    ID_CFG_APPLY_ENTRY,
                    ID_CFG_TOGGLE_ENTRY,
                    ID_CFG_RESET_DEFAULT,
                    ID_CFG_DELETE_ENTRY,
                    ID_CFG_MOVE_UP,
                    ID_CFG_MOVE_DOWN,
                ] {
                    self.show_config_id(id, true);
                }
                for hwnd in [self.cfg_index, self.cfg_modifier_help_button] {
                    self.show_config_hwnd(hwnd, true);
                }

                MoveWindow(
                    GetDlgItem(self.config_hwnd, ID_CFG_INDEX_LABEL),
                    right_x,
                    index_label_y,
                    right_w,
                    label_h,
                    TRUE,
                );
                MoveWindow(self.cfg_index, right_x, list_top, right_w, list_h, TRUE);

                self.layout_button_row(
                    right_x,
                    entry_button_top,
                    right_w,
                    button_h,
                    gap,
                    &entry_buttons,
                );
                MoveWindow(
                    GetDlgItem(self.config_hwnd, ID_CFG_TIP),
                    right_x,
                    tip_top,
                    right_w,
                    label_h,
                    TRUE,
                );
            }
            SettingsPage::General => {
                self.layout_general_config_page(
                    right_x,
                    right_w,
                    pad,
                    page_bottom,
                    label_h,
                    edit_h,
                    button_h,
                );
            }
            SettingsPage::HeuristicScoring | SettingsPage::PatternScoring => {
                self.layout_scoring_config_page(layout);
            }
            SettingsPage::PluginAliases => {
                self.layout_plugin_alias_config_page(layout);
            }
            SettingsPage::QueryLaunchRules => {
                self.layout_query_launch_rules_config_page(layout);
            }
        }

        if self.config_redraw_transaction_depth == 0 {
            self.request_config_repaint();
        }
    }

    fn localized_button_width(&self, id: i32, minimum: i32) -> i32 {
        unsafe {
            let hwnd = GetDlgItem(self.config_hwnd, id);
            minimum.max(control_text_width(hwnd, &get_window_text(hwnd)) + 24)
        }
    }

    pub(crate) fn button_rows_height(
        &self,
        buttons: &[(i32, i32)],
        available_w: i32,
        button_h: i32,
        gap: i32,
    ) -> i32 {
        if buttons.is_empty() {
            return 0;
        }
        let mut rows = 1;
        let mut x = 0;
        let max_w = available_w.max(1);
        for &(id, min_w) in buttons {
            let w = self.localized_button_width(id, min_w).min(max_w);
            if x > 0 && x + gap + w > max_w {
                rows += 1;
                x = 0;
            }
            x += if x == 0 { w } else { gap + w };
        }
        rows * button_h + (rows - 1) * gap
    }

    pub(crate) unsafe fn layout_button_row(
        &self,
        x: i32,
        y: i32,
        available_w: i32,
        button_h: i32,
        gap: i32,
        buttons: &[(i32, i32)],
    ) {
        let mut row_y = y;
        let mut row_x = x;
        let max_w = available_w.max(1);
        let right = x + max_w;
        for &(id, min_w) in buttons {
            let button_w = self.localized_button_width(id, min_w).min(max_w);
            if row_x > x && row_x + button_w > right {
                row_y += button_h + gap;
                row_x = x;
            }
            MoveWindow(
                GetDlgItem(self.config_hwnd, id),
                row_x,
                row_y,
                button_w,
                button_h,
                TRUE,
            );
            row_x += button_w + gap;
        }
    }

    pub(crate) unsafe fn layout_general_config_page(
        &mut self,
        right_x: i32,
        right_w: i32,
        pad: i32,
        page_bottom: i32,
        label_h: i32,
        edit_h: i32,
        button_h: i32,
    ) {
        let row_y = pad + 58;
        let row_step = 42;
        let wide_layout = right_w >= 520;
        let column_gap = 24;
        let column_w = if wide_layout {
            (right_w - column_gap) / 2
        } else {
            right_w
        };
        let left_x = right_x;
        let right_column_x = right_x + column_w + column_gap;
        let label_w = if wide_layout { 150 } else { 190 };
        let left_edit_x = left_x + label_w;
        let right_edit_x = right_column_x + label_w;
        let field_w = (column_w - label_w).max(90);
        for id in [
            ID_CFG_HOTKEY_LABEL,
            ID_CFG_RESULT_LIMIT_LABEL,
            ID_CFG_SEARCH_THREADS_LABEL,
            ID_CFG_LANGUAGE_LABEL,
            ID_CFG_TOOLTIP_OPACITY_LABEL,
            ID_CFG_TOOLTIP_OPACITY_VALUE,
            ID_CFG_GENERAL_RESET,
        ] {
            self.show_config_id(id, true);
        }
        for hwnd in [
            self.cfg_hotkey,
            self.cfg_result_limit,
            self.cfg_search_thread_mode,
            self.cfg_search_threads,
            self.cfg_language,
            self.cfg_sound,
            self.cfg_score_breakdown,
            self.cfg_score_breakdown_tooltip,
            self.cfg_tooltip_opacity,
            self.cfg_tooltip_opacity_value,
            self.cfg_show_cpu_in_title,
            self.cfg_show_ram_in_title,
            self.cfg_show_build_timestamp_in_title,
            self.cfg_autostart,
        ] {
            self.show_config_hwnd(hwnd, true);
        }
        self.apply_score_breakdown_tooltip_controls_visibility();

        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_HOTKEY_LABEL),
            left_x,
            row_y,
            label_w,
            label_h,
            TRUE,
        );
        MoveWindow(
            self.cfg_hotkey,
            left_edit_x,
            row_y - 3,
            field_w,
            edit_h,
            TRUE,
        );

        let result_y = if wide_layout { row_y } else { row_y + row_step };
        let result_x = if wide_layout { right_column_x } else { left_x };
        let result_edit_x = if wide_layout {
            right_edit_x
        } else {
            left_edit_x
        };
        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_RESULT_LIMIT_LABEL),
            result_x,
            result_y,
            label_w,
            label_h,
            TRUE,
        );
        MoveWindow(
            self.cfg_result_limit,
            result_edit_x,
            result_y - 3,
            field_w.min(100),
            edit_h,
            TRUE,
        );

        let search_y = row_y + if wide_layout { row_step } else { row_step * 2 };
        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_SEARCH_THREADS_LABEL),
            left_x,
            search_y,
            label_w,
            label_h,
            TRUE,
        );
        let search_mode_w = field_w.min(150).max(100);
        MoveWindow(
            self.cfg_search_thread_mode,
            left_edit_x,
            search_y - 3,
            search_mode_w,
            edit_h * 4,
            TRUE,
        );
        MoveWindow(
            self.cfg_search_threads,
            left_edit_x + search_mode_w + 8,
            search_y - 3,
            (field_w - search_mode_w - 8).max(38),
            edit_h,
            TRUE,
        );
        self.apply_search_thread_mode_selection();

        let language_y = row_y + if wide_layout { row_step } else { row_step * 3 };
        let language_x = if wide_layout { right_column_x } else { left_x };
        let language_edit_x = if wide_layout {
            right_edit_x
        } else {
            left_edit_x
        };
        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_LANGUAGE_LABEL),
            language_x,
            language_y,
            label_w,
            label_h,
            TRUE,
        );
        MoveWindow(
            self.cfg_language,
            language_edit_x,
            language_y - 3,
            field_w,
            edit_h * 6,
            TRUE,
        );

        let options_y = row_y
            + if wide_layout {
                row_step * 2
            } else {
                row_step * 4
            };
        MoveWindow(self.cfg_sound, left_x, options_y, 110, edit_h, TRUE);
        MoveWindow(
            self.cfg_score_breakdown_tooltip,
            left_x + 120,
            options_y,
            (column_w - 120).max(165),
            edit_h,
            TRUE,
        );
        let score_breakdown_x = if wide_layout { right_column_x } else { left_x };
        let score_breakdown_y = if wide_layout {
            options_y
        } else {
            options_y + row_step
        };
        MoveWindow(
            self.cfg_score_breakdown,
            score_breakdown_x,
            score_breakdown_y,
            if wide_layout { column_w } else { right_w },
            edit_h,
            TRUE,
        );

        let opacity_y = row_y
            + if wide_layout {
                row_step * 3
            } else {
                row_step * 6
            };
        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_TOOLTIP_OPACITY_LABEL),
            left_x,
            opacity_y,
            label_w,
            label_h,
            TRUE,
        );
        let opacity_w = (right_w - label_w - 70).clamp(180, 360);
        MoveWindow(
            self.cfg_tooltip_opacity,
            left_edit_x,
            opacity_y - 8,
            opacity_w,
            36,
            TRUE,
        );
        MoveWindow(
            self.cfg_tooltip_opacity_value,
            left_edit_x + opacity_w + 10,
            opacity_y,
            60,
            label_h,
            TRUE,
        );

        let title_options_y = opacity_y + row_step;
        let title_option_w = (right_w - column_gap) / 2;
        MoveWindow(
            self.cfg_show_cpu_in_title,
            left_x,
            title_options_y,
            title_option_w,
            label_h + 4,
            TRUE,
        );
        MoveWindow(
            self.cfg_show_ram_in_title,
            left_x + title_option_w + column_gap,
            title_options_y,
            title_option_w,
            label_h + 4,
            TRUE,
        );
        MoveWindow(
            self.cfg_show_build_timestamp_in_title,
            left_x,
            title_options_y + row_step,
            title_option_w,
            label_h + 4,
            TRUE,
        );
        MoveWindow(
            self.cfg_autostart,
            left_x + title_option_w + column_gap,
            title_options_y + row_step,
            title_option_w,
            label_h + 4,
            TRUE,
        );
        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_GENERAL_RESET),
            right_x,
            page_bottom - button_h,
            SETTINGS_ACTION_BUTTON_WIDTH,
            button_h,
            TRUE,
        );
    }

    unsafe fn layout_scoring_config_page(&mut self, layout: ConfigLayoutMetrics) {
        let ConfigLayoutMetrics {
            right_x,
            right_w,
            pad,
            page_bottom,
            label_h,
            button_h,
            gap,
            ..
        } = layout;
        for id in [ID_SCORING_LIST_LABEL, ID_SCORING_ENABLED, ID_SCORING_RESET] {
            self.show_config_id(id, true);
        }
        let is_pattern = self.settings_page == SettingsPage::PatternScoring;
        for id in [
            ID_SCORING_ADD,
            ID_SCORING_DELETE,
            ID_SCORING_MOVE_UP,
            ID_SCORING_MOVE_DOWN,
        ] {
            self.show_config_id(id, is_pattern);
        }
        for hwnd in [self.scoring_list, self.scoring_status] {
            self.show_config_hwnd(hwnd, true);
        }
        self.show_config_hwnd(self.cfg_modifier_help_button, is_pattern);

        let header_y = pad + 50;
        let list_top = header_y + label_h + 2;
        let scoring_buttons = if is_pattern {
            vec![
                (ID_SCORING_ENABLED, SETTINGS_ACTION_BUTTON_WIDTH),
                (ID_SCORING_ADD, SETTINGS_ACTION_BUTTON_WIDTH),
                (ID_SCORING_DELETE, SETTINGS_ACTION_BUTTON_WIDTH),
                (ID_SCORING_MOVE_UP, SETTINGS_ACTION_BUTTON_WIDTH),
                (ID_SCORING_MOVE_DOWN, SETTINGS_ACTION_BUTTON_WIDTH),
                (ID_SCORING_RESET, SETTINGS_ACTION_BUTTON_WIDTH),
                (ID_CFG_MODIFIER_HELP_BUTTON, SETTINGS_ACTION_BUTTON_WIDTH),
            ]
        } else {
            vec![
                (ID_SCORING_ENABLED, SETTINGS_ACTION_BUTTON_WIDTH),
                (ID_SCORING_RESET, SETTINGS_ACTION_BUTTON_WIDTH),
            ]
        };
        let buttons_h = self.button_rows_height(&scoring_buttons, right_w, button_h, gap);
        let status_top = page_bottom - label_h;
        let button_top = status_top - gap - buttons_h;
        let list_h = (button_top - list_top - gap).max(120);

        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_SCORING_LIST_LABEL),
            right_x,
            header_y,
            right_w,
            label_h,
            TRUE,
        );
        MoveWindow(self.scoring_list, right_x, list_top, right_w, list_h, TRUE);

        self.layout_button_row(
            right_x,
            button_top,
            right_w,
            button_h,
            gap,
            &scoring_buttons,
        );
        MoveWindow(
            self.scoring_status,
            right_x,
            status_top,
            right_w,
            label_h,
            TRUE,
        );
    }

    unsafe fn layout_query_launch_rules_config_page(&mut self, layout: ConfigLayoutMetrics) {
        let ConfigLayoutMetrics {
            right_x,
            right_w,
            pad,
            page_bottom,
            label_h,
            edit_h,
            button_h,
            gap,
        } = layout;
        for id in [
            ID_CFG_INDEX_LABEL,
            ID_CFG_QUERY_LAUNCH_FILTER_LABEL,
            ID_CFG_QUERY_LAUNCH_COUNT,
            ID_CFG_DELETE_ENTRY,
            ID_CFG_CLEAR_HISTORY,
            ID_CFG_TIP,
        ] {
            self.show_config_id(id, true);
        }
        self.show_config_hwnd(self.cfg_query_launch_filter, true);
        self.show_config_hwnd(self.cfg_query_launch_count, true);
        self.show_config_hwnd(self.cfg_index, true);
        self.show_config_id(ID_CFG_TOGGLE_ENTRY, false);

        let label_y = pad + 50;
        let filter_y = label_y + label_h + 4;
        let count_y = filter_y + edit_h + 2;
        let list_top = count_y + label_h + 4;
        let history_buttons = vec![
            (ID_CFG_DELETE_ENTRY, SETTINGS_ACTION_BUTTON_WIDTH),
            (ID_CFG_CLEAR_HISTORY, SETTINGS_ACTION_BUTTON_WIDTH),
        ];
        let buttons_h = self.button_rows_height(&history_buttons, right_w, button_h, gap);
        let tip_top = page_bottom - label_h;
        let button_top = tip_top - gap - buttons_h;
        let list_h = (button_top - list_top - gap).max(120);

        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_INDEX_LABEL),
            right_x,
            label_y,
            right_w,
            label_h,
            TRUE,
        );
        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_QUERY_LAUNCH_FILTER_LABEL),
            right_x,
            filter_y + 3,
            64,
            label_h,
            TRUE,
        );
        MoveWindow(
            self.cfg_query_launch_filter,
            right_x + 72,
            filter_y,
            (right_w - 72).max(120),
            edit_h,
            TRUE,
        );
        MoveWindow(
            self.cfg_query_launch_count,
            right_x,
            count_y,
            right_w,
            label_h,
            TRUE,
        );
        MoveWindow(self.cfg_index, right_x, list_top, right_w, list_h, TRUE);

        self.layout_button_row(
            right_x,
            button_top,
            right_w,
            button_h,
            gap,
            &history_buttons,
        );
        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_TIP),
            right_x,
            tip_top,
            right_w,
            label_h,
            TRUE,
        );
    }

    unsafe fn layout_plugin_alias_config_page(&mut self, layout: ConfigLayoutMetrics) {
        let ConfigLayoutMetrics {
            right_x,
            right_w,
            pad,
            page_bottom,
            label_h,
            button_h,
            gap,
            ..
        } = layout;
        for id in [ID_CFG_INDEX_LABEL, ID_CFG_RESET_DEFAULT, ID_CFG_TIP] {
            self.show_config_id(id, true);
        }
        self.show_config_hwnd(self.cfg_index, true);

        let label_y = pad + 50;
        let list_top = label_y + label_h + 2;
        let tip_top = page_bottom - label_h;
        let button_top = tip_top - gap - button_h;
        let list_h = (button_top - list_top - gap).max(120);
        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_INDEX_LABEL),
            right_x,
            label_y,
            right_w,
            label_h,
            TRUE,
        );
        MoveWindow(self.cfg_index, right_x, list_top, right_w, list_h, TRUE);

        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_RESET_DEFAULT),
            right_x,
            button_top,
            SETTINGS_ACTION_BUTTON_WIDTH,
            button_h,
            TRUE,
        );
        MoveWindow(
            GetDlgItem(self.config_hwnd, ID_CFG_TIP),
            right_x,
            tip_top,
            right_w,
            label_h,
            TRUE,
        );
    }

    pub(crate) unsafe fn apply_score_breakdown_tooltip_controls_visibility(&mut self) {
        let visible = self.settings_page == SettingsPage::General;
        self.show_config_id(ID_CFG_TOOLTIP_OPACITY_LABEL, visible);
        self.show_config_hwnd(self.cfg_tooltip_opacity, visible);
        self.show_config_hwnd(self.cfg_tooltip_opacity_value, visible);
    }

    pub(crate) unsafe fn reset_general_settings_to_defaults(&mut self) {
        let answer = MessageBoxW(
            self.config_hwnd,
            wide(localized(
                self.language,
                "This will reset all General settings to their first-run defaults.\nChanges will not take effect until you click Save + Apply.",
            ))
            .as_ptr(),
            wide(localized(self.language, "Reset General Settings?")).as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        );
        if answer != IDYES {
            return;
        }
        let defaults = AppSettingsSnapshot::first_run_defaults_with_hotkey(default_hotkey());
        self.loading_config_entry = true;
        self.pending_hotkey = Some(defaults.hotkey.clone());
        set_window_text(self.cfg_hotkey, &defaults.hotkey.display);
        set_window_text(self.cfg_result_limit, &defaults.result_limit.to_string());
        self.refresh_search_thread_controls(defaults.search_threads);
        SendMessageW(
            self.cfg_language,
            CB_SETCURSEL,
            defaults.language.combo_index(),
            0,
        );
        SendMessageW(
            self.cfg_sound,
            BM_SETCHECK,
            if defaults.popup_sound {
                BST_CHECKED as usize
            } else {
                0
            },
            0,
        );
        SendMessageW(
            self.cfg_score_breakdown,
            BM_SETCHECK,
            if defaults.show_score_breakdown {
                BST_CHECKED as usize
            } else {
                0
            },
            0,
        );
        SendMessageW(
            self.cfg_score_breakdown_tooltip,
            BM_SETCHECK,
            if defaults.show_score_breakdown_tooltip {
                BST_CHECKED as usize
            } else {
                0
            },
            0,
        );
        SendMessageW(
            self.cfg_tooltip_opacity,
            TBM_SETPOS,
            TRUE as usize,
            defaults.tooltip_opacity_percent as isize,
        );
        self.update_tooltip_opacity_value_label();
        for (control, checked) in [
            (self.cfg_show_cpu_in_title, defaults.show_cpu_in_title),
            (self.cfg_show_ram_in_title, defaults.show_ram_in_title),
            (
                self.cfg_show_build_timestamp_in_title,
                defaults.show_build_timestamp_in_title,
            ),
            (self.cfg_autostart, defaults.autostart),
        ] {
            SendMessageW(
                control,
                BM_SETCHECK,
                if checked { BST_CHECKED as usize } else { 0 },
                0,
            );
        }
        self.loading_config_entry = false;
        set_window_text(
            self.cfg_status,
            localized(
                self.language,
                "General defaults loaded. Click Save + Apply to persist.",
            ),
        );
    }

    pub(crate) unsafe fn refresh_search_thread_controls(&mut self, mode: SearchThreadMode) {
        if self.cfg_search_thread_mode.is_null() || self.cfg_search_threads.is_null() {
            return;
        }
        let available = available_search_threads();
        SendMessageW(self.cfg_search_thread_mode, CB_RESETCONTENT, 0, 0);
        for label in [
            format!("Auto ({})", SearchThreadMode::Auto.resolve()),
            "Manual".to_string(),
            format!("Maximum ({available})"),
        ] {
            let text = wide(&label);
            SendMessageW(
                self.cfg_search_thread_mode,
                CB_ADDSTRING,
                0,
                text.as_ptr() as isize,
            );
        }
        let selected = match mode {
            SearchThreadMode::Auto => 0,
            SearchThreadMode::Manual(_) => 1,
            SearchThreadMode::Maximum => 2,
        };
        SendMessageW(self.cfg_search_thread_mode, CB_SETCURSEL, selected, 0);
        set_window_text(self.cfg_search_threads, &mode.manual_value().to_string());
        let manual = matches!(mode, SearchThreadMode::Manual(_));
        self.show_config_hwnd(self.cfg_search_threads, manual);
        EnableWindow(self.cfg_search_threads, if manual { TRUE } else { FALSE });
    }

    pub(crate) unsafe fn apply_search_thread_mode_selection(&mut self) {
        let selected = SendMessageW(self.cfg_search_thread_mode, CB_GETCURSEL, 0, 0) as i32;
        let manual = selected == 1;
        self.show_config_hwnd(self.cfg_search_threads, manual);
        EnableWindow(self.cfg_search_threads, if manual { TRUE } else { FALSE });
    }

    pub(crate) unsafe fn refresh_language_combo(&self) {
        if self.cfg_language.is_null() {
            return;
        }
        SendMessageW(self.cfg_language, CB_RESETCONTENT, 0, 0);
        let english = wide(BUILTIN_ENGLISH_NAME);
        SendMessageW(
            self.cfg_language,
            CB_ADDSTRING,
            0,
            english.as_ptr() as isize,
        );
        for pack in language_packs() {
            let text = wide(pack.name);
            SendMessageW(self.cfg_language, CB_ADDSTRING, 0, text.as_ptr() as isize);
        }
        let dropdown_width = language_packs()
            .iter()
            .map(|pack| control_text_width(self.cfg_language, pack.name) + 36)
            .max()
            .unwrap_or(180)
            .max(180);
        SendMessageW(
            self.cfg_language,
            CB_SETDROPPEDWIDTH,
            dropdown_width as usize,
            0,
        );
        SendMessageW(
            self.cfg_language,
            CB_SETCURSEL,
            self.language.combo_index(),
            0,
        );
    }

    pub(crate) unsafe fn refresh_config_nav_labels(&self) {
        if self.cfg_nav.is_null() {
            return;
        }
        let selected = config_nav_index_for_page(self.settings_page);
        // Keep the native list's mouse tracking intact while switching pages.
        let labels = settings_nav_labels(self.language);
        let labels_unchanged = SendMessageW(self.cfg_nav, LB_GETCOUNT, 0, 0) == labels.len() as isize
            && labels.iter().enumerate().all(|(index, label)| {
                let expected = wide(label);
                let length = SendMessageW(self.cfg_nav, LB_GETTEXTLEN, index, 0);
                if length < 0 || length as usize + 1 != expected.len() {
                    return false;
                }
                let mut actual = vec![0u16; expected.len()];
                SendMessageW(self.cfg_nav, LB_GETTEXT, index, actual.as_mut_ptr() as isize);
                actual == expected
            });
        if labels_unchanged {
            if SendMessageW(self.cfg_nav, LB_GETCURSEL, 0, 0) != selected as isize {
                SendMessageW(self.cfg_nav, LB_SETCURSEL, selected, 0);
            }
            return;
        }
        SendMessageW(self.cfg_nav, LB_RESETCONTENT, 0, 0);
        for item in settings_nav_labels(self.language) {
            let text = wide(item);
            SendMessageW(self.cfg_nav, LB_ADDSTRING, 0, text.as_ptr() as isize);
        }
        SendMessageW(self.cfg_nav, LB_SETCURSEL, selected, 0);
    }

    pub(crate) unsafe fn apply_language_selection_from_gui(&mut self) {
        if self.loading_config_entry {
            return;
        }
        self.language =
            AppLanguage::from_combo_index(
                SendMessageW(self.cfg_language, CB_GETCURSEL, 0, 0) as i32
            );
        set_active_language(self.language);
        self.apply_config_page();
        set_window_text(
            self.cfg_status,
            &settings_model::status_message(self.language, "Language changed"),
        );
    }

    pub(crate) unsafe fn update_config_static_texts(&mut self) {
        set_window_text(self.config_hwnd, &settings_window_title(self.language));
        self.refresh_config_nav_labels();
        self.refresh_language_combo();
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_HOTKEY_LABEL),
            localized(self.language, "Popup hotkey"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_RESULT_LIMIT_LABEL),
            localized(self.language, "Popup result count (min 9)"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_SEARCH_THREADS_LABEL),
            localized(self.language, "Search threads"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_LANGUAGE_LABEL),
            localized(self.language, "Language"),
        );
        set_window_text(self.cfg_sound, localized(self.language, "Sound"));
        set_window_text(
            self.cfg_score_breakdown,
            localized(self.language, "Show score breakdown in results"),
        );
        set_window_text(
            self.cfg_score_breakdown_tooltip,
            localized(self.language, "Show score breakdown tooltip"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_TOOLTIP_OPACITY_LABEL),
            localized(self.language, "Tooltip opacity"),
        );
        set_window_text(
            self.cfg_show_cpu_in_title,
            localized(self.language, "Show CPU in window title"),
        );
        set_window_text(
            self.cfg_show_ram_in_title,
            localized(self.language, "Show RAM in window title"),
        );
        set_window_text(
            self.cfg_show_build_timestamp_in_title,
            localized(self.language, "Show build timestamp in window title"),
        );
        set_window_text(self.cfg_autostart, localized(self.language, "Start with Windows"));
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_GENERAL_RESET),
            localized(self.language, "Reset Defaults"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_APPLY_ENTRY),
            localized(self.language, "Add New"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_TOGGLE_ENTRY),
            localized(self.language, "Enable / Disable"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_DELETE_ENTRY),
            localized(self.language, "Delete"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_MOVE_UP),
            localized(self.language, "Move Up"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_MOVE_DOWN),
            localized(self.language, "Move Down"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_CLEAR_HISTORY),
            localized(self.language, "Clear History"),
        );
        set_window_text(
            self.cfg_modifier_help_button,
            localized(self.language, "Modifier guide"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_QUERY_LAUNCH_FILTER_LABEL),
            localized(self.language, "Filter"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_SAVE),
            localized(self.language, "Save + Apply"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_OPEN_FOLDER),
            localized(self.language, "Open Config Folder"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_CLOSE),
            localized(self.language, "Close"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_SCORING_LIST_LABEL),
            localized(self.language, "Rules"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_SCORING_ADD),
            localized(self.language, "Add Pattern"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_SCORING_DELETE),
            localized(self.language, "Delete"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_SCORING_MOVE_UP),
            localized(self.language, "Move Up"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_SCORING_MOVE_DOWN),
            localized(self.language, "Move Down"),
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_SCORING_RESET),
            localized(self.language, "Reset Default"),
        );
    }

    pub(crate) unsafe fn select_config_page_from_nav(&mut self) {
        if self.cfg_nav.is_null() {
            return;
        }
        let selected = SendMessageW(self.cfg_nav, LB_GETCURSEL, 0, 0) as i32;
        if selected < 0 || settings_page_from_nav_index(selected) == self.settings_page {
            return;
        }
        self.begin_config_redraw_transaction();
        self.settings_page = settings_page_from_nav_index(selected);
        self.apply_config_page();
        match self.settings_page {
            SettingsPage::HeuristicScoring | SettingsPage::PatternScoring => {
                set_window_text(
                    self.cfg_status,
                    localized(
                        self.language,
                        "Scoring rules selected. Edit entries, then Save + Apply.",
                    ),
                );
            }
            SettingsPage::SearchFolders => {
                set_window_text(
                    self.cfg_status,
                    localized(
                        self.language,
                        "Search folders selected. Edit entries, then Save + Apply.",
                    ),
                );
            }
            SettingsPage::PluginAliases => {
                set_window_text(
                    self.cfg_status,
                    localized(
                        self.language,
                        "Plugin Alias selected. Edit aliases, then Save + Apply.",
                    ),
                );
            }
            SettingsPage::QueryLaunchRules => {
                set_window_text(
                    self.cfg_status,
                    localized(
                        self.language,
                        "Query Launch Rules selected. Delete rows, then Save + Apply.",
                    ),
                );
            }
            SettingsPage::General => {
                set_window_text(
                    self.cfg_status,
                    localized(self.language, "Edit settings, then Save + Apply."),
                );
            }
        }
        self.end_config_redraw_transaction();
    }

    pub(crate) unsafe fn apply_config_page(&mut self) {
        if self.config_hwnd.is_null() {
            return;
        }
        self.begin_config_redraw_transaction();
        self.update_config_static_texts();
        self.apply_cfg_index_selection_mode_for_page();
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_DESCRIPTION),
            settings_page_description(self.settings_page, self.language),
        );
        let (header_dir, header_modifier, header_score) =
            settings_page_detail_lines(self.settings_page, self.language);
        set_window_text(GetDlgItem(self.config_hwnd, ID_CFG_HEADER_DIR), header_dir);
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_HEADER_MODIFIER),
            header_modifier,
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_HEADER_SCORE),
            header_score,
        );
        set_window_text(
            GetDlgItem(self.config_hwnd, ID_CFG_TIP),
            settings_page_tip(self.settings_page, self.language),
        );
        match self.settings_page {
            SettingsPage::SearchFolders => {
                set_window_text(
                    GetDlgItem(self.config_hwnd, ID_CFG_INDEX_LABEL),
                    localized(self.language, "Search folders (select a row to edit below)"),
                );
                set_window_text(
                    GetDlgItem(self.config_hwnd, ID_CFG_TOGGLE_ENTRY),
                    localized(self.language, "Enable / Disable"),
                );
                set_window_text(
                    GetDlgItem(self.config_hwnd, ID_CFG_RESET_DEFAULT),
                    localized(self.language, "Reset Default"),
                );
                set_window_text(
                    GetDlgItem(self.config_hwnd, ID_CFG_MOVE_UP),
                    localized(self.language, "Move Up"),
                );
                set_window_text(
                    GetDlgItem(self.config_hwnd, ID_CFG_MOVE_DOWN),
                    localized(self.language, "Move Down"),
                );
            }
            SettingsPage::PluginAliases => {
                set_window_text(
                    GetDlgItem(self.config_hwnd, ID_CFG_INDEX_LABEL),
                    localized(self.language, "Plugin aliases (select a row to edit below)"),
                );
                set_window_text(
                    GetDlgItem(self.config_hwnd, ID_CFG_RESET_DEFAULT),
                    localized(self.language, "Reset Default"),
                );
            }
            SettingsPage::QueryLaunchRules => {
                set_window_text(
                    GetDlgItem(self.config_hwnd, ID_CFG_INDEX_LABEL),
                    localized(self.language, "Query Launch Rules (select rows to delete)"),
                );
                set_window_text(
                    GetDlgItem(self.config_hwnd, ID_CFG_CLEAR_HISTORY),
                    localized(self.language, "Select All"),
                );
                set_window_text(
                    GetDlgItem(self.config_hwnd, ID_CFG_DELETE_ENTRY),
                    localized(self.language, "Delete Selected"),
                );
                self.update_query_launch_rules_selection_count();
            }
            _ => {}
        }
        match self.settings_page {
            SettingsPage::SearchFolders => self.populate_index_list(),
            SettingsPage::PluginAliases => self.populate_plugin_alias_list(),
            SettingsPage::QueryLaunchRules => self.populate_query_launch_rules_list(),
            SettingsPage::HeuristicScoring | SettingsPage::PatternScoring => {
                self.populate_scoring_list_for_page(self.settings_page)
            }
            _ => {}
        }
        self.resize_config_controls();
        if !matches!(self.settings_page, SettingsPage::General) {
            self.apply_current_settings_table_capabilities();
        }
        self.end_config_redraw_transaction();
    }

    pub(crate) unsafe fn apply_cfg_index_selection_mode_for_page(&self) {
        if self.cfg_index.is_null() {
            return;
        }
        set_list_view_multi_select(
            self.cfg_index,
            matches!(
                self.settings_page,
                SettingsPage::SearchFolders
                    | SettingsPage::QueryLaunchRules
                    | SettingsPage::PluginAliases
            ),
        );
    }

    pub(crate) unsafe fn apply_current_settings_table_capabilities(&mut self) {
        let table = self.current_settings_table();
        let mut multi_select = false;
        let mut can_toggle = false;
        let mut can_add = false;
        let mut can_delete = false;
        let mut can_reset = false;
        let mut can_move = false;
        let mut can_help = false;
        self.with_list_model_mut(table.list, |model| {
            multi_select = model.supports_multi_select();
            can_toggle = model.supports_toggle();
            can_add = model.supports_add();
            can_delete = model.supports_delete();
            can_reset = model.supports_reset_default();
            can_move = model.supports_move();
            can_help = model.supports_modifier_help();
        });
        set_list_view_multi_select(table.list, multi_select);
        self.show_config_hwnd(table.label, true);
        self.show_config_hwnd(table.toggle_button, can_toggle);
        self.show_config_hwnd(table.add_button, can_add);
        self.show_config_hwnd(table.delete_button, can_delete);
        self.show_config_hwnd(table.move_up_button, can_move);
        self.show_config_hwnd(table.move_down_button, can_move);
        self.show_config_hwnd(table.reset_button, can_reset);
        self.show_config_hwnd(table.modifier_help_button, can_help);
    }

    pub(crate) unsafe fn scroll_config_by(&mut self, delta: i32) {
        self.set_config_scroll(self.config_scroll_y.saturating_add(delta));
    }

    pub(crate) unsafe fn set_config_scroll(&mut self, value: i32) {
        if self.config_hwnd.is_null() {
            return;
        }
        if self.config_content_height == 0 {
            self.resize_config_controls();
        }
        let mut rect: RECT = std::mem::zeroed();
        GetClientRect(self.config_hwnd, &mut rect);
        let height = rect.bottom - rect.top;
        let max_scroll = (self.config_content_height - height).max(0);
        let next = value.clamp(0, max_scroll);
        if next != self.config_scroll_y {
            let delta = self.config_scroll_y - next;
            self.config_scroll_y = next;
            let mut scroll_info: SCROLLINFO = std::mem::zeroed();
            scroll_info.cbSize = std::mem::size_of::<SCROLLINFO>() as u32;
            scroll_info.fMask = SIF_RANGE | SIF_PAGE | SIF_POS;
            scroll_info.nMin = 0;
            scroll_info.nMax = (self.config_content_height - 1).max(0);
            scroll_info.nPage = height.max(0) as u32;
            scroll_info.nPos = self.config_scroll_y;
            SetScrollInfo(self.config_hwnd, SB_VERT, &scroll_info, TRUE);
            ScrollWindowEx(
                self.config_hwnd,
                0,
                delta,
                null(),
                null(),
                null_mut(),
                null_mut(),
                SW_SCROLLCHILDREN | SW_INVALIDATE,
            );
        }
    }

    pub(crate) unsafe fn save_config_from_gui(&mut self) -> bool {
        if self.config_save_in_progress {
            return false;
        }
        if self.inline_edit_hwnd != 0 as HWND {
            self.finish_inline_edit(self.inline_edit_hwnd, true);
            if self.inline_edit_hwnd != 0 as HWND {
                return false;
            }
        }

        let Some(mut current) = self.current_config_snapshot_from_gui() else {
            show_error(
                self.config_hwnd,
                localized(
                    self.language,
                    "Invalid settings value. Check the hotkey, result count, thread count, and tooltip opacity, then Save.",
                ),
            );
            return false;
        };
        current.config_search_history =
            normalize_search_history_items(&current.config_search_history, &self.plugin_state);
        self.config_search_history = current.config_search_history.clone();
        if let Err(message) = self.validate_config_models() {
            show_error(self.config_hwnd, localized(self.language, message));
            return false;
        }

        self.config_save_generation = self.config_save_generation.wrapping_add(1).max(1);
        self.config_save_in_progress = true;
        let save_button = GetDlgItem(self.config_hwnd, ID_CFG_SAVE);
        EnableWindow(save_button, FALSE);
        set_window_text(
            self.cfg_status,
            localized(self.language, "Saving config..."),
        );
        if let Some(worker) = &self.save_worker {
            worker.save_config(
                self.config_save_generation,
                self.config_hwnd as isize,
                current,
                self.config_snapshot.clone(),
            );
            true
        } else {
            self.config_save_in_progress = false;
            EnableWindow(save_button, TRUE);
            false
        }
    }

    pub(crate) unsafe fn apply_config_save_results(&mut self) {
        let results = if let Ok(mut pending) = pending_config_save_slot().lock() {
            std::mem::take(&mut *pending)
        } else {
            Vec::new()
        };
        for result in results {
            if result.generation != self.config_save_generation {
                continue;
            }
            self.config_save_in_progress = false;
            EnableWindow(GetDlgItem(self.config_hwnd, ID_CFG_SAVE), TRUE);
            if let Some(error) = result.error {
                show_error(
                    self.config_hwnd,
                    &localized_format1(
                        self.language,
                        "Could not save settings:
{}",
                        error,
                    ),
                );
                set_window_text(self.cfg_status, localized(self.language, "Save failed."));
                continue;
            }

            let current = result.current;
            if let Err(error) = crate::autostart::set_enabled(current.autostart) {
                show_error(self.config_hwnd, &error);
                SendMessageW(
                    self.cfg_autostart,
                    BM_SETCHECK,
                    if crate::autostart::is_enabled() { BST_CHECKED as usize } else { 0 },
                    0,
                );
                set_window_text(self.cfg_status, localized(self.language, "Save failed."));
                continue;
            }
            self.hotkey = current.hotkey.clone();
            self.result_limit = current.result_limit;
            self.search_threads = current.search_threads;
            self.show_score_breakdown = current.show_score_breakdown;
            self.show_score_breakdown_tooltip = current.show_score_breakdown_tooltip;
            self.tooltip_opacity_percent = current.tooltip_opacity_percent;
            self.show_cpu_in_title = current.show_cpu_in_title;
            self.show_ram_in_title = current.show_ram_in_title;
            self.show_build_timestamp_in_title = current.show_build_timestamp_in_title;
            self.popup_sound = current.popup_sound;
            self.language = current.language;
            set_active_language(self.language);
            self.plugin_state
                .set_alias_entries_in_memory(&current.config_plugin_aliases);
            if let Some(alias) = self.plugin_state.alias_for("calculator") {
                self.plugin_supervisor.update_alias("calculator", alias);
            }
            self.config_snapshot = Some(current);
            let status = if result.ignored_optional.is_empty() {
                localized(self.language, "Saved config.")
            } else {
                localized(
                    self.language,
                    "Required settings were saved. Some optional files were ignored.",
                )
            };
            set_window_text(self.cfg_status, status);
            notify_launcher_settings_changed();
            if self.config_close_after_save {
                self.config_close_after_save = false;
                PostMessageW(self.config_hwnd, WM_CLOSE, 0, 0);
            }
        }
    }

    pub(crate) unsafe fn search_folder_tooltip_text(&self, index: i32) -> String {
        if self.settings_page != SettingsPage::SearchFolders || index < 0 {
            return String::new();
        }
        let Some(root) = self.config_roots.get(index as usize) else {
            return String::new();
        };
        if let Some(path) = root.path.as_ref() {
            path.to_string_lossy().to_string()
        } else {
            root.raw.clone()
        }
    }

    pub(crate) unsafe fn current_settings_table(&self) -> SettingsTableView {
        if matches!(
            self.settings_page,
            SettingsPage::HeuristicScoring | SettingsPage::PatternScoring
        ) {
            SettingsTableView {
                list: self.scoring_list,
                label: GetDlgItem(self.config_hwnd, ID_SCORING_LIST_LABEL),
                status: self.scoring_status,
                toggle_button: GetDlgItem(self.config_hwnd, ID_SCORING_ENABLED),
                add_button: GetDlgItem(self.config_hwnd, ID_SCORING_ADD),
                delete_button: GetDlgItem(self.config_hwnd, ID_SCORING_DELETE),
                move_up_button: GetDlgItem(self.config_hwnd, ID_SCORING_MOVE_UP),
                move_down_button: GetDlgItem(self.config_hwnd, ID_SCORING_MOVE_DOWN),
                reset_button: GetDlgItem(self.config_hwnd, ID_SCORING_RESET),
                modifier_help_button: self.cfg_modifier_help_button,
            }
        } else {
            SettingsTableView {
                list: self.cfg_index,
                label: GetDlgItem(self.config_hwnd, ID_CFG_INDEX_LABEL),
                status: self.cfg_status,
                toggle_button: GetDlgItem(self.config_hwnd, ID_CFG_TOGGLE_ENTRY),
                add_button: GetDlgItem(self.config_hwnd, ID_CFG_APPLY_ENTRY),
                delete_button: GetDlgItem(self.config_hwnd, ID_CFG_DELETE_ENTRY),
                move_up_button: GetDlgItem(self.config_hwnd, ID_CFG_MOVE_UP),
                move_down_button: GetDlgItem(self.config_hwnd, ID_CFG_MOVE_DOWN),
                reset_button: GetDlgItem(self.config_hwnd, ID_CFG_RESET_DEFAULT),
                modifier_help_button: self.cfg_modifier_help_button,
            }
        }
    }

    pub(crate) unsafe fn reset_selected_default_entry(&mut self) {
        self.reset_selected_current_table_rows_to_default();
    }

    pub(crate) unsafe fn reset_selected_current_table_rows_to_default(&mut self) {
        let table = self.current_settings_table();
        let selected_indices = table.selected_indices();
        if selected_indices.is_empty() {
            return;
        }
        let reset_count = {
            let mut count = 0usize;
            self.with_list_model_mut(table.list, |model| {
                if !model.supports_reset_default() {
                    return;
                }
                for &index in &selected_indices {
                    if model.reset_row_to_default(index) {
                        count += 1;
                    }
                }
            });
            count
        };
        if reset_count == 0 {
            table.set_status(localized(
                self.language,
                "No first-run default for this row.",
            ));
            return;
        }
        self.repopulate_current_settings_table();
        table.restore_selection(&selected_indices);
        self.load_selected_current_settings_table_entry();
        table.set_status(&settings_model::status_message(
            self.language,
            "Row reset to first-run default",
        ));
    }

    pub(crate) unsafe fn toggle_selected_index_entry(&mut self) {
        let table = self.current_settings_table();
        let selected_indices = table.selected_indices();
        if selected_indices.is_empty() {
            return;
        }
        let toggled = {
            let mut done = false;
            self.with_list_model_mut(table.list, |model| {
                if !model.supports_toggle() {
                    return;
                }
                for &index in &selected_indices {
                    done |= model.toggle_row(index);
                }
            });
            done
        };
        if toggled {
            self.repopulate_current_settings_table();
            table.restore_selection(&selected_indices);
            self.load_selected_current_settings_table_entry();
            table.set_status(&settings_model::status_message(
                self.language,
                "Entry toggled",
            ));
        }
    }

    pub(crate) unsafe fn toggle_selected_scoring_rule(&mut self) {
        let selected_indices = list_view_selected_indices(self.scoring_list);
        if selected_indices.is_empty() {
            return;
        }
        let mut toggled = false;
        for &visible_index in &selected_indices {
            if let Some(index) = self.scoring_rule_index_from_visible_index(visible_index as i32) {
                self.scoring_rules[index].enabled = !self.scoring_rules[index].enabled;
                toggled = true;
            }
        }
        if toggled {
            self.populate_scoring_list_for_page(self.settings_page);
            restore_list_view_selection(self.scoring_list, &selected_indices);
            self.load_selected_scoring_rule();
            set_window_text(
                self.scoring_status,
                &settings_model::status_message(self.language, "Rule toggled"),
            );
        }
    }

    pub(crate) unsafe fn sync_index_check_from_list_view(
        &mut self,
        visible_index: i32,
        checked: bool,
    ) {
        if self.loading_config_entry || visible_index < 0 {
            return;
        }
        if self.settings_page == SettingsPage::QueryLaunchRules
            || self.settings_page == SettingsPage::PluginAliases
        {
            return;
        }
        let index = visible_index as usize;
        // Get current enabled state from model
        let current_enabled = match self.settings_page {
            SettingsPage::SearchFolders => self.config_roots.get(index).map(|r| r.enabled),
            _ => None,
        };
        if let Some(enabled) = current_enabled {
            if enabled != checked {
                // Apply the check state change
                if self.settings_page == SettingsPage::SearchFolders {
                    self.config_roots[index].enabled = checked;
                }
                self.load_selected_cfg_index_entry();
                set_window_text(
                    self.cfg_status,
                    &settings_model::status_message(self.language, "Entry toggled"),
                );
            }
        }
    }

    pub(crate) unsafe fn sync_scoring_check_from_list_view(
        &mut self,
        visible_index: i32,
        checked: bool,
    ) {
        if self.loading_scoring_entry {
            return;
        }
        let Some(index) = self.scoring_rule_index_from_visible_index(visible_index) else {
            return;
        };
        if self.scoring_rules[index].enabled != checked {
            self.scoring_rules[index].enabled = checked;
            self.load_selected_scoring_rule();
            set_window_text(
                self.scoring_status,
                &settings_model::status_message(self.language, "Rule toggled"),
            );
        }
    }

    pub(crate) unsafe fn delete_selected_index_entry(&mut self) {
        if self.settings_page == SettingsPage::QueryLaunchRules {
            let selected_visible_indices = list_view_selected_indices(self.cfg_index);
            if selected_visible_indices.is_empty() {
                return;
            }
            let actual_indices = query_launch_rule_actual_indices_for_visible_selection(
                &self.query_launch_rule_visible_indices,
                &selected_visible_indices,
            );
            let deleted = delete_query_launch_rules_by_indices(
                &mut self.config_query_launch_rules,
                &actual_indices,
            );
            if deleted > 0 {
                self.populate_query_launch_rules_list();
                set_window_text(
                    self.cfg_status,
                    &settings_model::status_message(self.language, "Entries deleted"),
                );
            }
            return;
        }
        let selected_indices = list_view_selected_indices(self.cfg_index);
        if selected_indices.is_empty() {
            return;
        }
        let selected_count = selected_indices.len();
        let deleted = {
            let mut count = 0usize;
            self.with_list_model_mut(self.cfg_index, |model| {
                if !model.supports_delete() {
                    return;
                }
                for &index in selected_indices.iter().rev() {
                    if model.delete_row(index) {
                        count += 1;
                    }
                }
            });
            count
        };
        if deleted > 0 {
            self.repopulate_current_cfg_index_list();
            let status = if deleted < selected_count {
                settings_model::status_message(
                    self.language,
                    "Custom entries deleted; default folders kept",
                )
            } else if deleted == 1 {
                settings_model::status_message(self.language, "Entry deleted")
            } else {
                settings_model::status_message(self.language, "Entries deleted")
            };
            set_window_text(self.cfg_status, &status);
        } else if self.settings_page == SettingsPage::SearchFolders {
            set_window_text(
                self.cfg_status,
                &settings_model::status_message(
                    self.language,
                    "Default folders can only be enabled or disabled",
                ),
            );
        }
    }

    pub(crate) unsafe fn move_selected_index_entry(&mut self, delta: i32) {
        let mut can_move = false;
        self.with_list_model_mut(self.cfg_index, |model| {
            can_move = model.supports_move();
        });
        if !can_move {
            return;
        }
        let index = list_view_selected_index(self.cfg_index);
        if index < 0 {
            return;
        }
        let old = index as usize;
        let count = {
            let mut c = 0usize;
            self.with_list_model_mut(self.cfg_index, |model| {
                c = model.row_count();
            });
            c
        };
        if old >= count {
            return;
        }
        let new = (old as i32 + delta).clamp(0, count as i32 - 1) as usize;
        if new == old {
            return;
        }
        let swapped = {
            let mut done = false;
            self.with_list_model_mut(self.cfg_index, |model| {
                done = model.swap_rows(old, new);
            });
            done
        };
        if swapped {
            self.repopulate_current_cfg_index_list();
            select_list_view_item(self.cfg_index, new);
            self.load_selected_cfg_index_entry();
            set_window_text(
                self.cfg_status,
                &settings_model::status_message(self.language, "Entry moved"),
            );
        }
    }

    pub(crate) unsafe fn clear_history_from_config_page(&mut self) {
        if self.settings_page == SettingsPage::QueryLaunchRules {
            select_all_list_view_items(self.cfg_index);
            self.update_query_launch_rules_selection_count();
            set_window_text(
                self.cfg_status,
                localized(self.language, "All rows selected"),
            );
        }
    }

    pub(crate) unsafe fn start_hotkey_capture(&mut self) {
        self.capturing_hotkey = true;
        self.pending_hotkey = None;
        set_window_text(self.cfg_hotkey, localized(self.language, "Press hotkey..."));
        set_window_text(
            self.cfg_status,
            localized(
                self.language,
                "Press the hotkey combination now, then Save + Apply.",
            ),
        );
        SendMessageW(self.cfg_hotkey, EM_SETSEL, 0, -1);
    }

    pub(crate) unsafe fn capture_hotkey_key(&mut self, key: u16) -> bool {
        if !self.capturing_hotkey
            || key == VK_CONTROL
            || key == VK_SHIFT
            || key == VK_MENU
            || key == VK_LWIN
            || key == VK_RWIN
        {
            return false;
        }
        if key == VK_ESCAPE {
            self.capturing_hotkey = false;
            set_window_text(self.cfg_hotkey, &self.hotkey.display);
            set_window_text(
                self.cfg_status,
                localized(self.language, "Hotkey capture cancelled."),
            );
            return true;
        }

        let mut modifiers = 0u32;
        let mut parts = Vec::new();
        if key_down(VK_CONTROL) {
            modifiers |= MOD_CONTROL;
            parts.push("Ctrl".to_string());
        }
        if key_down(VK_MENU) {
            modifiers |= MOD_ALT;
            parts.push("Alt".to_string());
        }
        if key_down(VK_SHIFT) {
            modifiers |= MOD_SHIFT;
            parts.push("Shift".to_string());
        }
        if key_down(VK_LWIN) || key_down(VK_RWIN) {
            modifiers |= MOD_WIN;
            parts.push("Win".to_string());
        }

        let display = hotkey_key_display(key);
        parts.push(display);
        let parsed = Hotkey {
            modifiers,
            key: key as u32,
            display: parts.join("+"),
        };
        self.pending_hotkey = Some(parsed.clone());
        self.capturing_hotkey = false;
        set_window_text(self.cfg_hotkey, &parsed.display);
        set_window_text(
            self.cfg_status,
            localized(
                self.language,
                "Hotkey captured. Click Save + Apply to use it.",
            ),
        );
        true
    }

    pub(crate) unsafe fn start_inline_edit(&mut self, hwnd_list: HWND, item: i32, subitem: i32) {
        let is_editable = self.is_list_col_editable(hwnd_list, subitem as usize);

        if !is_editable {
            return;
        }

        if self.inline_edit_hwnd != 0 as HWND {
            self.finish_inline_edit(self.inline_edit_hwnd, true);
        }

        let mut rect: RECT = std::mem::zeroed();
        rect.top = subitem;
        rect.left = if subitem == 0 {
            LVIR_LABEL as i32
        } else {
            LVIR_BOUNDS as i32
        };
        SendMessageW(
            hwnd_list,
            LVM_GETSUBITEMRECT,
            item as usize,
            &mut rect as *mut _ as isize,
        );

        let mut pt = POINT {
            x: rect.left,
            y: rect.top,
        };
        MapWindowPoints(hwnd_list, self.config_hwnd, &mut pt, 1);
        let mut pt2 = POINT {
            x: rect.right,
            y: rect.bottom,
        };
        MapWindowPoints(hwnd_list, self.config_hwnd, &mut pt2, 1);

        let mut buffer = vec![0u16; 1024];
        let mut lvi: LVITEMW = std::mem::zeroed();
        lvi.iSubItem = subitem;
        lvi.pszText = buffer.as_mut_ptr();
        lvi.cchTextMax = buffer.len() as i32;
        let len = SendMessageW(
            hwnd_list,
            LVM_GETITEMTEXTW,
            item as usize,
            &mut lvi as *mut _ as isize,
        );
        let text = OsString::from_wide(&buffer[..len as usize])
            .to_string_lossy()
            .to_string();

        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            wide("EDIT").as_ptr(),
            wide(&text).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL as u32,
            pt.x,
            pt.y,
            pt2.x - pt.x,
            pt2.y - pt.y,
            self.config_hwnd,
            0_isize as _,
            self.instance,
            null(),
        );

        let font = SendMessageW(hwnd_list, WM_GETFONT, 0, 0);
        SendMessageW(edit, WM_SETFONT, font as usize, 0);

        let orig = set_window_long(edit, GWLP_WNDPROC, floating_edit_proc as *const () as isize);
        set_window_long(edit, GWLP_USERDATA, orig);

        self.inline_edit_hwnd = edit;
        self.inline_edit_list = hwnd_list;
        self.inline_edit_item = item;
        self.inline_edit_subitem = subitem;

        SetFocus(edit);
        SendMessageW(edit, EM_SETSEL, 0, -1isize);
    }

    pub(crate) unsafe fn finish_inline_edit(&mut self, edit_hwnd: HWND, save: bool) {
        if self.inline_edit_hwnd != edit_hwnd || edit_hwnd == 0 as HWND {
            return;
        }
        let text = get_window_text(edit_hwnd);

        if save {
            if let Err(message) = self.validate_inline_edit_value(&text) {
                set_window_text(self.cfg_status, localized(self.language, message));
                MessageBeep(MB_ICONWARNING);
                FlashWindow(edit_hwnd, TRUE);
                SetFocus(edit_hwnd);
                SendMessageW(edit_hwnd, EM_SETSEL, 0, -1isize);
                return;
            }
        }

        let list = self.inline_edit_list;
        let item = self.inline_edit_item;
        let subitem = self.inline_edit_subitem;
        DestroyWindow(edit_hwnd);
        self.inline_edit_hwnd = 0 as HWND;

        if !save {
            return;
        }

        let mut lvi: LVITEMW = std::mem::zeroed();
        lvi.iItem = item;
        lvi.iSubItem = subitem;
        let mut wide_text = wide(&text);
        lvi.pszText = wide_text.as_mut_ptr();
        SendMessageW(
            list,
            LVM_SETITEMTEXTW,
            item as usize,
            &mut lvi as *mut _ as isize,
        );

        self.save_inline_edit_to_model(list, item, subitem, &text);
        if list == self.cfg_index && self.settings_page == SettingsPage::SearchFolders {
            self.repopulate_current_cfg_index_list();
            select_list_view_item(self.cfg_index, item as usize);
            self.load_selected_cfg_index_entry();
        }
    }

    pub(crate) unsafe fn validate_inline_edit_value(&self, text: &str) -> Result<(), &'static str> {
        let text_trimmed = text.trim();
        let subitem = self.inline_edit_subitem as usize;
        if self.inline_edit_list == self.cfg_index {
            match self.settings_page {
                SettingsPage::SearchFolders => match subitem {
                    0 => (!text_trimmed.is_empty())
                        .then_some(())
                        .ok_or("Search Folder path cannot be blank."),
                    1 => Ok(()),
                    2 => text_trimmed
                        .parse::<i32>()
                        .map(|_| ())
                        .map_err(|_| "Score must be an integer."),
                    3 => text_trimmed
                        .parse::<isize>()
                        .ok()
                        .filter(|value| *value == -1 || *value >= 0)
                        .map(|_| ())
                        .ok_or("Depth must be -1 or a non-negative integer."),
                    _ => Ok(()),
                },
                SettingsPage::QueryLaunchRules => Ok(()),
                SettingsPage::PluginAliases => Ok(()),
                _ => Ok(()),
            }
        } else if self.inline_edit_list == self.scoring_list {
            match self.settings_page {
                SettingsPage::HeuristicScoring => {
                    if subitem == 1 {
                        let key = self
                            .scoring_rules
                            .iter()
                            .filter(|entry| entry.kind == ScoringRuleKind::Heuristic)
                            .nth(self.inline_edit_item as usize)
                            .map(|entry| entry.key.as_str())
                            .unwrap_or_default();
                        scoring_rule_value_is_valid(key, text_trimmed)
                            .then_some(())
                            .ok_or(
                                "Heuristic Scoring values must be integers; fuzzy weights and thresholds cannot be negative.",
                            )
                    } else {
                        Ok(())
                    }
                }
                SettingsPage::PatternScoring => match subitem {
                    0 => (!text_trimmed.is_empty())
                        .then_some(())
                        .ok_or("Pattern Scoring pattern cannot be blank."),
                    1 => Ok(()),
                    2 => text_trimmed
                        .parse::<i32>()
                        .map(|_| ())
                        .map_err(|_| "Pattern Scoring values must be integers."),
                    _ => Ok(()),
                },
                _ => Ok(()),
            }
        } else {
            Ok(())
        }
    }

    pub(crate) unsafe fn save_inline_edit_to_model(
        &mut self,
        list: HWND,
        item: i32,
        subitem: i32,
        text: &str,
    ) {
        let language = self.language;
        let mut row_style_param = 0;
        self.with_list_model_mut(list, |model| {
            model.set_cell_text(item as usize, subitem as usize, text);
            row_style_param = model.row_style_param(item as usize, language);
        });
        set_list_view_item_param(list, item as usize, row_style_param);
    }

    // -----------------------------------------------------------------------
    // Model-dispatch helpers for SettingsListModel trait
    // -----------------------------------------------------------------------

    /// Check if a column in the given list is editable, using ColumnDef metadata.
    pub(crate) fn is_list_col_editable(&self, hwnd_list: HWND, col: usize) -> bool {
        if hwnd_list != self.cfg_index && hwnd_list != self.scoring_list {
            return false;
        }
        let page = self.settings_page;
        let cols = settings_model::columns_for_page(page, self.language);
        cols.get(col).is_some_and(|c| c.editable)
    }

    /// Run a mutating closure with the appropriate model for `hwnd_list`.
    pub(crate) fn with_list_model_mut(
        &mut self,
        hwnd_list: HWND,
        f: impl FnOnce(&mut dyn settings_model::SettingsListModel),
    ) {
        use settings_model::*;
        if hwnd_list == self.cfg_index {
            match self.settings_page {
                SettingsPage::SearchFolders => {
                    let mut model = SearchFoldersModel {
                        roots: &mut self.config_roots,
                    };
                    f(&mut model);
                }
                SettingsPage::QueryLaunchRules => {
                    let mut model = QueryLaunchRulesModel {
                        items: &mut self.config_query_launch_rules,
                    };
                    f(&mut model);
                }
                SettingsPage::PluginAliases => {
                    let mut model = PluginAliasModel {
                        entries: &mut self.config_plugin_aliases,
                    };
                    f(&mut model);
                }
                _ => {}
            }
        } else if hwnd_list == self.scoring_list {
            let kind = match self.settings_page {
                SettingsPage::HeuristicScoring => ScoringRuleKind::Heuristic,
                SettingsPage::PatternScoring => ScoringRuleKind::Pattern,
                _ => return,
            };
            let mut model = ScoringModel {
                rules: &mut self.scoring_rules,
                kind,
                language: self.language,
            };
            f(&mut model);
        }
    }

    pub(crate) unsafe fn repopulate_current_settings_table(&mut self) {
        if matches!(
            self.settings_page,
            SettingsPage::HeuristicScoring | SettingsPage::PatternScoring
        ) {
            self.populate_scoring_list_for_page(self.settings_page);
        } else {
            self.repopulate_current_cfg_index_list();
        }
    }

    pub(crate) unsafe fn load_selected_current_settings_table_entry(&mut self) {
        if matches!(
            self.settings_page,
            SettingsPage::HeuristicScoring | SettingsPage::PatternScoring
        ) {
            self.load_selected_scoring_rule();
        } else {
            self.load_selected_cfg_index_entry();
        }
    }

    /// Re-populate the cfg_index ListView for the current settings page.
    pub(crate) unsafe fn repopulate_current_cfg_index_list(&mut self) {
        match self.settings_page {
            SettingsPage::SearchFolders => self.populate_index_list(),
            SettingsPage::QueryLaunchRules => self.populate_query_launch_rules_list(),
            SettingsPage::PluginAliases => self.populate_plugin_alias_list(),
            _ => {}
        }
    }

    /// Load the selected entry details for the current settings page.
    pub(crate) unsafe fn load_selected_cfg_index_entry(&mut self) {
        match self.settings_page {
            SettingsPage::SearchFolders => self.load_selected_index_entry(),
            SettingsPage::QueryLaunchRules => self.load_selected_query_launch_rule_entry(),
            SettingsPage::PluginAliases => self.load_selected_plugin_alias_entry(),
            _ => {}
        }
    }
}

pub(crate) unsafe extern "system" fn numeric_edit_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    let rule = with_app(|app| app.numeric_edit_rule_for_hwnd(hwnd)).flatten();
    match (msg, rule) {
        (WM_CHAR, Some(rule)) => {
            let ch_u16 = wparam as u16;
            if (32..=255).contains(&ch_u16) {
                let replacement = char::from(ch_u16 as u8).to_string();
                if !numeric_edit_replacement_allowed(hwnd, rule, &replacement) {
                    return 0;
                }
            }
        }
        (WM_PASTE, Some(rule)) => {
            if let Some(text) = clipboard_text(hwnd) {
                if !numeric_edit_replacement_allowed(hwnd, rule, &text) {
                    MessageBeep(MB_ICONWARNING);
                    return 0;
                }
            }
        }
        (WM_NCDESTROY, _) => {
            RemoveWindowSubclass(
                hwnd,
                Some(numeric_edit_subclass_proc),
                NUMERIC_EDIT_SUBCLASS_ID,
            );
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

pub(crate) unsafe extern "system" fn floating_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let orig_proc: WNDPROC = std::mem::transmute(get_window_long(hwnd, GWLP_USERDATA));
    let orig_proc = orig_proc.unwrap();

    match msg {
        WM_CHAR => {
            let ch_u16 = wparam as u16;
            if ch_u16 < 32 {
                return CallWindowProcW(Some(orig_proc), hwnd, msg, wparam, lparam);
            }
            if ch_u16 > 255 {
                return CallWindowProcW(Some(orig_proc), hwnd, msg, wparam, lparam);
            }
            let ch = char::from(ch_u16 as u8);
            let numeric_rule = floating_edit_numeric_rule();
            if let Some(rule) = numeric_rule {
                let replacement = ch.to_string();
                if !numeric_edit_replacement_allowed(hwnd, rule, &replacement) {
                    return 0;
                }
            }
        }
        WM_PASTE => {
            if let Some(rule) = floating_edit_numeric_rule() {
                if let Some(text) = clipboard_text(hwnd) {
                    if !numeric_edit_replacement_allowed(hwnd, rule, &text) {
                        MessageBeep(MB_ICONWARNING);
                        return 0;
                    }
                }
            }
        }
        WM_KEYDOWN => {
            if wparam == VK_RETURN as usize {
                let parent = GetParent(hwnd);
                PostMessageW(parent, WM_INLINE_EDIT_FINISH, hwnd as usize, 1);
                return 0;
            } else if wparam == VK_ESCAPE as usize {
                let parent = GetParent(hwnd);
                PostMessageW(parent, WM_INLINE_EDIT_FINISH, hwnd as usize, 0);
                return 0;
            }
        }
        WM_KILLFOCUS => {
            let parent = GetParent(hwnd);
            PostMessageW(parent, WM_INLINE_EDIT_FINISH, hwnd as usize, 1);
        }
        _ => {}
    }
    CallWindowProcW(Some(orig_proc), hwnd, msg, wparam, lparam)
}

fn floating_edit_numeric_rule() -> Option<NumericEditRule> {
    with_app(|app| {
        let subitem = app.inline_edit_subitem as usize;
        let page = app.settings_page;
        let list = app.inline_edit_list;
        if list == app.cfg_index {
            match page {
                SettingsPage::SearchFolders if subitem == 2 => {
                    Some(NumericEditRule::signed_integer())
                }
                SettingsPage::SearchFolders if subitem == 3 => Some(NumericEditRule::depth()),
                _ => None,
            }
        } else if list == app.scoring_list {
            match page {
                SettingsPage::HeuristicScoring if subitem == 1 => {
                    Some(NumericEditRule::signed_integer())
                }
                SettingsPage::PatternScoring if subitem == 2 => {
                    Some(NumericEditRule::signed_integer())
                }
                _ => None,
            }
        } else {
            None
        }
    })
    .unwrap_or(None)
}

unsafe fn numeric_edit_replacement_allowed(
    hwnd: HWND,
    rule: NumericEditRule,
    replacement: &str,
) -> bool {
    let current = get_window_text(hwnd);
    let mut start = 0u32;
    let mut end = 0u32;
    SendMessageW(
        hwnd,
        EM_GETSEL,
        &mut start as *mut u32 as usize,
        &mut end as *mut u32 as isize,
    );
    let next = replace_utf16_range(&current, start as usize, end as usize, replacement);
    rule.candidate_allowed(&next)
}

fn signed_integer_edit_candidate_allowed(text: &str) -> bool {
    if text.is_empty() || text == "-" {
        return true;
    }
    text.strip_prefix('-')
        .unwrap_or(text)
        .chars()
        .all(|ch| ch.is_ascii_digit())
}

fn unsigned_integer_edit_candidate_allowed(text: &str) -> bool {
    text.is_empty() || text.chars().all(|ch| ch.is_ascii_digit())
}

fn depth_edit_candidate_allowed(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() || text == "-" {
        return true;
    }
    if text.starts_with('-') {
        "-1".starts_with(text)
    } else {
        text.chars().all(|ch| ch.is_ascii_digit())
            && text.parse::<isize>().is_ok_and(|value| value >= 0)
    }
}

fn replace_utf16_range(text: &str, start: usize, end: usize, replacement: &str) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let start = start.min(units.len());
    let end = end.min(units.len()).max(start);
    let mut next = String::new();
    next.push_str(&String::from_utf16_lossy(&units[..start]));
    next.push_str(replacement);
    next.push_str(&String::from_utf16_lossy(&units[end..]));
    next
}

unsafe fn clipboard_text(owner: HWND) -> Option<String> {
    if OpenClipboard(owner) == 0 {
        return None;
    }
    let handle = GetClipboardData(CF_UNICODETEXT_ID);
    if handle.is_null() {
        CloseClipboard();
        return None;
    }
    let data = GlobalLock(handle) as *const u16;
    if data.is_null() {
        CloseClipboard();
        return None;
    }
    let mut len = 0usize;
    while *data.add(len) != 0 {
        len += 1;
    }
    let text = String::from_utf16_lossy(std::slice::from_raw_parts(data, len));
    GlobalUnlock(handle);
    CloseClipboard();
    Some(text)
}

fn status_text_runs(text: &str) -> Vec<(&str, bool)> {
    let mut runs = Vec::new();
    for part in text.split_inclusive('|') {
        if let Some(label) = part.strip_suffix('|') {
            if !label.is_empty() {
                runs.push((label, false));
            }
            runs.push(("|", true));
        } else {
            runs.push((part, false));
        }
    }
    runs
}

pub(crate) fn parse_default_value_into(text: &str, root: &mut IndexRoot) {
    root.max_depth = DEFAULT_SEARCH_DEPTH;
    for part in text.split('|') {
        let part = part.trim();
        if let Some(d) = part.strip_prefix("depth=") {
            root.max_depth = normalize_search_depth(
                d.trim()
                    .parse::<isize>()
                    .unwrap_or(DEFAULT_SEARCH_DEPTH as isize),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_runs_bold_only_separators_and_preserve_unicode() {
        for text in [
            "",
            "Đã xong",
            "|Đầu||Cuối|",
            "Đã xong | Shift+(Enter/nhấp đúp): Mở thư mục đích",
        ] {
            let runs = status_text_runs(text);
            assert_eq!(runs.iter().map(|(run, _)| *run).collect::<String>(), text);
            for (run, bold) in runs {
                assert_eq!(bold, run == "|");
            }
        }
    }

    #[test]
    fn effective_result_limit_uses_temporary_show_all_override() {
        let mut app = AppState::new(
            Vec::new(),
            Hotkey {
                modifiers: 0,
                key: 0,
                display: "Alt+Space".to_string(),
            },
        );
        app.result_limit = 25;

        let plain = parse_search_query("note");
        assert_eq!(app.effective_result_limit_for_spec(&plain), 25);

        app.show_all_results_override = true;
        assert_eq!(app.effective_result_limit_for_spec(&plain), usize::MAX);

        app.show_all_results_override = false;
        let explicit = parse_search_query("note +sall");
        assert_eq!(app.effective_result_limit_for_spec(&explicit), usize::MAX);
    }

    #[test]
    fn search_worker_restarts_after_inactive_memory_release() {
        let mut app = AppState::new(
            Vec::new(),
            Hotkey {
                modifiers: 0,
                key: 0,
                display: "Alt+Space".to_string(),
            },
        );

        assert!(app.stop_search_worker());
        assert!(app.search_worker.is_none());
        assert!(app.ensure_search_worker_running());
        assert!(app.search_worker.is_some());
        assert!(!app.ensure_search_worker_running());
    }

    #[test]
    fn inactive_search_cancellation_preserves_displayed_results() {
        let mut app = AppState::new(
            Vec::new(),
            Hotkey {
                modifiers: 0,
                key: 0,
                display: "Alt+Space".to_string(),
            },
        );
        app.results.push(SearchResult {
            title: "Visible Result".to_string(),
            subtitle: r"C:\Results".to_string(),
            target: LaunchTarget::Path(PathBuf::from(r"C:\Results\Visible.exe")),
            is_dir: false,
            from_history: false,
            from_query_launch_rule: false,
            ranking_kind: ResultRankingKind::Heuristic,
            explanation: None,
            display_score: 100,
            score_detail: "score 100".to_string(),
            score: 100,
        });
        let mut builder = ResultStoreBuilder::new(0xdef).unwrap();
        builder
            .push(SearchResult {
                title: "Paged Result".to_string(),
                subtitle: r"C:\Paged".to_string(),
                target: LaunchTarget::Path(PathBuf::from(r"C:\Paged\Visible.exe")),
                is_dir: false,
                from_history: false,
                from_query_launch_rule: false,
                ranking_kind: ResultRankingKind::Heuristic,
                explanation: None,
                display_score: 90,
                score_detail: "score 90".to_string(),
                score: 90,
            })
            .unwrap();
        app.result_store = Some(ResultStoreReader::open(builder.finish().unwrap()).unwrap());
        app.selected_result_index = Some(0);

        app.cancel_active_search_preserving_result_source();

        assert_eq!(app.result_count(), 1);
        assert_eq!(app.results[0].title, "Visible Result");
        assert_eq!(app.result_at(0).unwrap().title, "Paged Result");
        assert!(app.result_store.is_some());
        assert_eq!(app.selected_result_index, Some(0));
    }

    #[test]
    fn paged_result_source_supports_navigation_and_actions() {
        let mut builder = ResultStoreBuilder::new(0xabc).unwrap();
        for index in 0..300 {
            builder
                .push(SearchResult {
                    title: format!("Result {index:03}"),
                    subtitle: format!(r"C:\Results\Group {}", index / 10),
                    target: LaunchTarget::Path(PathBuf::from(format!(
                        r"C:\Results\Result {index:03}.exe"
                    ))),
                    is_dir: false,
                    from_history: false,
                    from_query_launch_rule: false,
                    ranking_kind: ResultRankingKind::Heuristic,
                    explanation: None,
                    display_score: 300 - index as i32,
                    score_detail: format!("score {}", 300 - index),
                    score: 300 - index as i32,
                })
                .unwrap();
        }
        let mut app = AppState::new(
            Vec::new(),
            Hotkey {
                modifiers: 0,
                key: 0,
                display: "Alt+Space".to_string(),
            },
        );
        app.search_effective_limit = usize::MAX;
        app.result_store = Some(ResultStoreReader::open(builder.finish().unwrap()).unwrap());

        assert_eq!(app.result_count(), 300);
        assert_eq!(app.default_launch_index(), Some(0));
        assert_eq!(app.result_at(128).unwrap().title, "Result 128");
        assert_eq!(
            app.result_path(299).unwrap(),
            PathBuf::from(r"C:\Results\Result 299.exe")
        );
        assert!(app
            .result_target_text(256)
            .unwrap()
            .ends_with("Result 256.exe"));
        assert!(app.result_signature(42).is_some());
        assert_eq!(selection_after_move(Some(299), 300, 1), Some(0));
        assert_eq!(selection_after_move(Some(0), 300, -1), Some(299));
    }

    #[test]
    fn result_index_extra_width_grows_after_three_digits() {
        assert_eq!(result_index_extra_width(9), 0);
        assert_eq!(result_index_extra_width(999), 0);
        assert_eq!(result_index_extra_width(1000), 9);
        assert_eq!(result_index_extra_width(10000), 18);
    }

    #[test]
    fn result_index_labels_expose_function_key_shortcuts() {
        for index in 0..9 {
            assert_eq!(
                result_index_label(index, false),
                Some(format!("F{}", index + 1))
            );
            assert_eq!(
                result_index_label(index, true),
                Some(format!("F{}", index + 1))
            );
        }
        assert_eq!(result_index_label(9, false), Some("10".to_string()));
        assert_eq!(result_index_label(9, true), None);
    }

    #[test]
    fn alias_result_text_moves_right_only_when_shortcut_is_visible() {
        assert_eq!(result_text_left(0, true, false, 0), 48);
        assert_eq!(result_text_left(8, true, false, 0), 48);
        assert_eq!(result_text_left(9, true, false, 0), 18);
    }

    #[test]
    fn numeric_edit_rule_filters_candidates() {
        assert!(NumericEditRule::signed_integer().candidate_allowed("-12"));
        assert!(!NumericEditRule::signed_integer().candidate_allowed("12a"));
        assert!(NumericEditRule::unsigned_integer_min(1).candidate_allowed("12"));
        assert!(!NumericEditRule::unsigned_integer_min(1).candidate_allowed("-1"));
        assert!(NumericEditRule::strict_unsigned_integer(1, 50).candidate_allowed("50"));
        assert!(!NumericEditRule::strict_unsigned_integer(1, 50).candidate_allowed("51"));
        assert!(!NumericEditRule::strict_unsigned_integer(1, 50).candidate_allowed("0"));
        assert!(NumericEditRule::depth().candidate_allowed("0"));
        assert!(NumericEditRule::depth().candidate_allowed("10"));
        assert!(NumericEditRule::depth().candidate_allowed("-1"));
        assert!(!NumericEditRule::depth().candidate_allowed("-2"));
    }

    #[test]
    fn numeric_edit_rule_validates_final_range() {
        let bounded = NumericEditRule::strict_unsigned_integer(1, 9);
        assert!(bounded.final_allowed("1"));
        assert!(bounded.final_allowed("9"));
        assert!(!bounded.final_allowed("0"));
        assert!(!bounded.final_allowed("10"));
        assert!(!bounded.final_allowed(""));
        assert!(NumericEditRule::depth().final_allowed("-1"));
        assert!(!NumericEditRule::depth().final_allowed("-"));
    }

    #[test]
    fn status_shows_done_when_search_is_not_running() {
        let app = AppState::new(
            Vec::new(),
            Hotkey {
                modifiers: 0,
                key: 0,
                display: "Alt+Space".to_string(),
            },
        );

        let status = app.default_status_text();
        assert_eq!(
            status,
            "Done 0 | Shift+(Enter/double-click): Target folder | Ctrl+(Home/End): First/Last | Ctrl+PgDn: All"
        );
        assert!(!status.contains("streaming"));
    }

    #[test]
    fn status_shows_running_stage_and_scanned_count() {
        let mut app = AppState::new(
            Vec::new(),
            Hotkey {
                modifiers: 0,
                key: 0,
                display: "Alt+Space".to_string(),
            },
        );
        app.search_running = true;
        app.search_scanned_total = 12_340;
        app.search_stage = SearchStage::Folders;

        let status = app.default_status_text();
        assert_eq!(status, "Scanning 12,340 | Shift+(Enter/double-click): Target folder | Ctrl+(Home/End): First/Last | Ctrl+PgDn: All");
    }

    #[test]
    fn status_shows_scan_done_counts() {
        let mut app = AppState::new(
            Vec::new(),
            Hotkey {
                modifiers: 0,
                key: 0,
                display: "Alt+Space".to_string(),
            },
        );
        app.last_search_elapsed = Some(Duration::from_millis(571));

        let status = app.default_status_text();
        assert_eq!(status, "Done 0 | 571 ms | Shift+(Enter/double-click): Target folder | Ctrl+(Home/End): First/Last | Ctrl+PgDn: All");
    }

    #[test]
    fn freeze_user_selection_keeps_scan_counts() {
        let mut app = AppState::new(
            Vec::new(),
            Hotkey {
                modifiers: 0,
                key: 0,
                display: "Alt+Space".to_string(),
            },
        );
        app.search_running = true;
        app.search_scanned_total = 579;

        unsafe { app.freeze_search_due_to_user_selection() };

        let status = app.default_status_text();
        assert_eq!(
            status,
            "Done 579 | Shift+(Enter/double-click): Target folder | Ctrl+(Home/End): First/Last | Ctrl+PgDn: All"
        );
    }

    #[test]
    fn status_shows_scan_done_counts_after_search() {
        let app = AppState::new(
            Vec::new(),
            Hotkey {
                modifiers: 0,
                key: 0,
                display: "Alt+Space".to_string(),
            },
        );

        let status = app.default_status_text();
        assert_eq!(
            status,
            "Done 0 | Shift+(Enter/double-click): Target folder | Ctrl+(Home/End): First/Last | Ctrl+PgDn: All"
        );
        assert!(!status.contains("fallback"));
    }
}


