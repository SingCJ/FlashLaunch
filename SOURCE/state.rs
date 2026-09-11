use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::plugins;
use crate::*;

/// Snapshot of all config state when Settings opens.
/// Used to detect real changes at close time, replacing dirty flags.
#[derive(Clone, PartialEq)]
pub(crate) struct ConfigSnapshot {
    pub(crate) hotkey: Hotkey,
    pub(crate) result_limit: usize,
    pub(crate) search_threads: SearchThreadMode,
    pub(crate) show_score_breakdown: bool,
    pub(crate) show_score_breakdown_tooltip: bool,
    pub(crate) tooltip_opacity_percent: u8,
    pub(crate) show_cpu_in_title: bool,
    pub(crate) show_ram_in_title: bool,
    pub(crate) show_build_timestamp_in_title: bool,
    pub(crate) autostart: bool,
    pub(crate) language: AppLanguage,
    pub(crate) popup_sound: bool,
    pub(crate) config_roots: Vec<IndexRoot>,
    pub(crate) scoring_rules: Vec<ScoringRuleEntry>,
    pub(crate) config_recent_items: Vec<RecentConfigEntry>,
    pub(crate) config_search_history: Vec<String>,
    pub(crate) config_query_launch_rules: Vec<QueryLaunchRule>,
    pub(crate) config_plugin_aliases: Vec<plugins::PluginAliasConfigEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingLaunchDecision {
    Wait,
    Launch,
    Clear,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingLaunchRequest {
    pub(crate) generation: u64,
    pub(crate) query: String,
}

impl PendingLaunchRequest {
    pub(crate) fn new(generation: u64, query: String) -> Self {
        Self { generation, query }
    }

    pub(crate) fn decide(
        &self,
        generation: u64,
        query: &str,
        has_results: bool,
        search_done: bool,
    ) -> PendingLaunchDecision {
        if self.generation != generation || self.query != query {
            PendingLaunchDecision::Clear
        } else if has_results {
            PendingLaunchDecision::Launch
        } else if search_done {
            PendingLaunchDecision::Clear
        } else {
            PendingLaunchDecision::Wait
        }
    }
}

pub(crate) struct AppState {
    pub(crate) hwnd: HWND,
    pub(crate) owner_hwnd: HWND,
    pub(crate) config_hwnd: HWND,
    pub(crate) add_index_hwnd: HWND,
    pub(crate) score_breakdown_hwnd: HWND,
    pub(crate) score_breakdown_header: HWND,
    pub(crate) score_breakdown_list: HWND,
    pub(crate) score_breakdown_copy: HWND,
    pub(crate) score_breakdown_close: HWND,
    pub(crate) score_explanation: Option<ScoreExplanation>,
    pub(crate) edit: HWND,
    pub(crate) list: HWND,
    pub(crate) plugin_textbox: HWND,
    pub(crate) status: HWND,
    pub(crate) config_button: HWND,
    pub(crate) plugin_help_button: HWND,
    pub(crate) cfg_hotkey: HWND,
    pub(crate) cfg_index: HWND,
    pub(crate) cfg_result_limit: HWND,
    pub(crate) cfg_search_thread_mode: HWND,
    pub(crate) cfg_search_threads: HWND,
    pub(crate) cfg_sound: HWND,
    pub(crate) cfg_score_breakdown: HWND,
    pub(crate) cfg_score_breakdown_tooltip: HWND,
    pub(crate) cfg_tooltip_opacity: HWND,
    pub(crate) cfg_tooltip_opacity_value: HWND,
    pub(crate) cfg_show_cpu_in_title: HWND,
    pub(crate) cfg_show_ram_in_title: HWND,
    pub(crate) cfg_show_build_timestamp_in_title: HWND,
    pub(crate) cfg_autostart: HWND,
    pub(crate) cfg_language: HWND,
    pub(crate) result_tooltip: HWND,
    pub(crate) result_tooltip_text: Vec<u16>,
    pub(crate) active_tooltip: Option<TooltipContent>,
    pub(crate) cfg_query_launch_filter: HWND,
    pub(crate) cfg_query_launch_count: HWND,
    pub(crate) cfg_status: HWND,
    pub(crate) cfg_nav: HWND,
    pub(crate) cfg_modifier_help_button: HWND,
    pub(crate) search_generation: u64,
    pub(crate) search_scanned_total: usize,
    pub(crate) search_running: bool,
    pub(crate) search_started_at: Option<Instant>,
    pub(crate) last_search_elapsed: Option<Duration>,
    pub(crate) search_worker: Option<SearchWorker>,
    pub(crate) icon_worker: Option<IconWorker>,
    pub(crate) launch_worker: Option<LaunchWorker>,
    pub(crate) save_worker: Option<SaveWorker>,
    pub(crate) settings_process_mode: bool,
    pub(crate) helper_focus_active: bool,
    pub(crate) helper_session_generation: u64,
    pub(crate) file_task_generation: u64,
    pub(crate) launcher_app_active: bool,
    pub(crate) icon_helper_error_reported: bool,
    pub(crate) shell_helper_error_reported: bool,
    pub(crate) search_roots: Vec<IndexRoot>,
    pub(crate) search_root_plan: Arc<RootOwnershipPlan>,
    pub(crate) search_scoring: ScoringConfig,
    pub(crate) search_scoring_snapshot: Arc<ScoringConfig>,
    pub(crate) add_path: HWND,
    pub(crate) add_score: HWND,
    pub(crate) add_depth: HWND,
    pub(crate) add_label: HWND,
    pub(crate) add_keywords: HWND,
    pub(crate) add_enabled: HWND,
    pub(crate) add_status: HWND,
    pub(crate) add_modifier_help_button: HWND,
    pub(crate) scoring_list: HWND,
    pub(crate) scoring_status: HWND,
    pub(crate) instance: HINSTANCE,
    pub(crate) app_icon: HICON,
    pub(crate) app_icon_small: HICON,
    pub(crate) title_font: HFONT,
    pub(crate) detail_font: HFONT,
    pub(crate) list_bold_font: HFONT,
    pub(crate) setting_help_font: HFONT,
    pub(crate) tray_added: bool,
    pub(crate) taskbar_created_message: u32,
    pub(crate) capturing_hotkey: bool,
    pub(crate) ime_composing: bool,
    pub(crate) config_scroll_y: i32,
    pub(crate) config_content_height: i32,
    pub(crate) config_redraw_transaction_depth: usize,
    pub(crate) config_redraw_children: Vec<(HWND, bool)>,
    pub(crate) config_redraw_visibility_touched: Vec<HWND>,
    pub(crate) reload_in_progress: bool,
    pub(crate) config_reload_generation: u64,
    pub(crate) config_save_generation: u64,
    pub(crate) config_save_in_progress: bool,
    pub(crate) config_close_after_save: bool,
    pub(crate) last_reload_started: Option<Instant>,
    pub(crate) config_snapshot: Option<ConfigSnapshot>,
    pub(crate) config_roots: Vec<IndexRoot>,
    pub(crate) scoring_rules: Vec<ScoringRuleEntry>,
    pub(crate) config_recent_items: Vec<RecentConfigEntry>,
    pub(crate) config_search_history: Vec<String>,
    pub(crate) config_query_launch_rules: Vec<QueryLaunchRule>,
    pub(crate) config_plugin_aliases: Vec<plugins::PluginAliasConfigEntry>,
    pub(crate) settings_page: SettingsPage,
    pub(crate) language: AppLanguage,
    pub(crate) loading_config_entry: bool,
    pub(crate) loading_scoring_entry: bool,
    pub(crate) pending_hotkey: Option<Hotkey>,
    pub(crate) items: Vec<LaunchItem>,
    pub(crate) recent_items: Vec<String>,
    pub(crate) recent_items_snapshot: Arc<Vec<String>>,
    pub(crate) plugin_state: plugins::PluginState,
    pub(crate) plugin_supervisor: crate::plugin_worker::PluginSupervisor,
    pub(crate) icon_cache: HashMap<String, HICON>,
    pub(crate) icon_cache_lru: VecDeque<String>,
    pub(crate) icon_failures: HashMap<String, Instant>,
    pub(crate) pending_icon_requests: HashMap<String, u64>,
    pub(crate) hotkey: Hotkey,
    pub(crate) result_limit: usize,
    pub(crate) search_threads: SearchThreadMode,
    pub(crate) show_score_breakdown: bool,
    pub(crate) show_score_breakdown_tooltip: bool,
    pub(crate) tooltip_opacity_percent: u8,
    pub(crate) show_cpu_in_title: bool,
    pub(crate) show_ram_in_title: bool,
    pub(crate) show_build_timestamp_in_title: bool,
    pub(crate) popup_sound: bool,
    pub(crate) search_effective_limit: usize,
    pub(crate) show_all_results_override: bool,
    pub(crate) last_search_query: String,
    pub(crate) search_stage: SearchStage,
    pub(crate) search_history: Vec<String>,
    pub(crate) search_history_cursor: Option<usize>,
    pub(crate) query_launch_rules: Vec<QueryLaunchRule>,
    pub(crate) query_launch_rules_snapshot: Arc<Vec<QueryLaunchRule>>,
    pub(crate) query_launch_rule_cursor: Option<usize>,
    pub(crate) query_launch_rule_filter: String,
    pub(crate) query_launch_rule_visible_indices: Vec<usize>,
    pub(crate) search_frozen_by_user_selection: bool,
    pub(crate) selected_result_index: Option<usize>,
    pub(crate) pending_launch: Option<PendingLaunchRequest>,
    pub(crate) pending_drag: Option<PendingDrag>,
    pub(crate) results: Vec<SearchResult>,
    pub(crate) result_store: Option<ResultStoreReader>,
    pub(crate) inline_edit_hwnd: HWND,
    pub(crate) inline_edit_list: HWND,
    pub(crate) inline_edit_item: i32,
    pub(crate) inline_edit_subitem: i32,
}

thread_local! {
    static APP: RefCell<Option<AppState>> = const { RefCell::new(None) };
}

pub(crate) fn with_app_tls<T>(
    callback: impl FnOnce(&std::cell::RefCell<Option<AppState>>) -> T,
) -> T {
    APP.with(callback)
}

pub(crate) static PENDING_INDEX: OnceLock<Mutex<Option<Vec<LaunchItem>>>> = OnceLock::new();
pub(crate) static PENDING_SEARCH: OnceLock<Mutex<Vec<SearchBatch>>> = OnceLock::new();
pub(crate) static PENDING_ICONS: OnceLock<Mutex<Vec<IconLoadResult>>> = OnceLock::new();
pub(crate) static PENDING_LAUNCHES: OnceLock<Mutex<Vec<LaunchWorkerResult>>> = OnceLock::new();
pub(crate) static SEARCH_BATCH_FORWARDER: OnceLock<Mutex<Option<mpsc::Sender<SearchBatch>>>> =
    OnceLock::new();
pub(crate) static ACTIVE_SEARCH_GENERATION: AtomicU64 = AtomicU64::new(0);
pub(crate) fn pending_index_slot() -> &'static Mutex<Option<Vec<LaunchItem>>> {
    PENDING_INDEX.get_or_init(|| Mutex::new(None))
}

pub(crate) fn pending_search_slot() -> &'static Mutex<Vec<SearchBatch>> {
    PENDING_SEARCH.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) fn pending_icon_slot() -> &'static Mutex<Vec<IconLoadResult>> {
    PENDING_ICONS.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) fn pending_launch_slot() -> &'static Mutex<Vec<LaunchWorkerResult>> {
    PENDING_LAUNCHES.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) fn search_batch_forwarder() -> &'static Mutex<Option<mpsc::Sender<SearchBatch>>> {
    SEARCH_BATCH_FORWARDER.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_launch_waits_for_matching_results() {
        let request = PendingLaunchRequest::new(7, "query".to_string());

        assert_eq!(
            request.decide(7, "query", false, false),
            PendingLaunchDecision::Wait
        );
        assert_eq!(
            request.decide(7, "query", true, false),
            PendingLaunchDecision::Launch
        );
    }

    #[test]
    fn pending_launch_clears_after_empty_completion() {
        let request = PendingLaunchRequest::new(7, "query".to_string());

        assert_eq!(
            request.decide(7, "query", false, true),
            PendingLaunchDecision::Clear
        );
    }

    #[test]
    fn pending_launch_rejects_stale_generation_or_query() {
        let request = PendingLaunchRequest::new(7, "query".to_string());

        assert_eq!(
            request.decide(8, "query", true, false),
            PendingLaunchDecision::Clear
        );
        assert_eq!(
            request.decide(7, "other", true, false),
            PendingLaunchDecision::Clear
        );
    }
}
