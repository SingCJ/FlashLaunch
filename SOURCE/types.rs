use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::null;
use std::sync::atomic::AtomicU32;

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Com::*;
use windows_sys::Win32::System::Ole::*;

use crate::plugins;
use crate::*;

#[derive(Clone)]
pub(crate) struct LaunchItem {
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
    pub(crate) folded_title: String,
    pub(crate) folded_stem: String,
    pub(crate) folded_parent: String,
    pub(crate) index_score: i32,
    pub(crate) modified_at_unix_seconds: Option<i64>,
    pub(crate) search_root: Option<PathBuf>,
    pub(crate) relative_depth: usize,
}

impl LaunchItem {
    pub(crate) fn new(
        title: impl Into<String>,
        subtitle: impl Into<String>,
        path: impl Into<PathBuf>,
        is_dir: bool,
        index_score: i32,
        modified_at_unix_seconds: Option<i64>,
    ) -> Self {
        let title = title.into();
        let path = path.into();
        let folded_title = fold_text(&title);
        let folded_stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(fold_text)
            .unwrap_or_default();
        let folded_parent = path
            .parent()
            .map(|parent| searchable_text(&parent.to_string_lossy()))
            .unwrap_or_default();

        Self {
            title,
            subtitle: subtitle.into(),
            path,
            is_dir,
            folded_title,
            folded_stem,
            folded_parent,
            index_score,
            modified_at_unix_seconds,
            search_root: None,
            relative_depth: 0,
        }
    }

    pub(crate) fn with_search_root(mut self, search_root: PathBuf, relative_depth: usize) -> Self {
        self.search_root = Some(search_root);
        self.relative_depth = relative_depth;
        self
    }
}

pub(crate) trait SearchItemView {
    fn title(&self) -> &str;
    fn subtitle(&self) -> &str;
    fn path(&self) -> &std::path::Path;
    fn is_dir(&self) -> bool;
    fn folded_title(&self) -> &str;
    fn folded_stem(&self) -> &str;
    fn folded_parent(&self) -> &str;
    fn index_score(&self) -> i32;
    fn modified_at_unix_seconds(&self) -> Option<i64>;
    fn search_root(&self) -> Option<&std::path::Path>;
    fn relative_depth(&self) -> usize;
}

impl SearchItemView for LaunchItem {
    fn title(&self) -> &str {
        &self.title
    }
    fn subtitle(&self) -> &str {
        &self.subtitle
    }
    fn path(&self) -> &std::path::Path {
        &self.path
    }
    fn is_dir(&self) -> bool {
        self.is_dir
    }
    fn folded_title(&self) -> &str {
        &self.folded_title
    }
    fn folded_stem(&self) -> &str {
        &self.folded_stem
    }
    fn folded_parent(&self) -> &str {
        &self.folded_parent
    }
    fn index_score(&self) -> i32 {
        self.index_score
    }
    fn modified_at_unix_seconds(&self) -> Option<i64> {
        self.modified_at_unix_seconds
    }
    fn search_root(&self) -> Option<&std::path::Path> {
        self.search_root.as_deref()
    }
    fn relative_depth(&self) -> usize {
        self.relative_depth
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SearchThreadMode {
    #[default]
    Auto,
    Manual(usize),
    Maximum,
}

impl SearchThreadMode {
    pub(crate) fn resolve(self) -> usize {
        let available = available_search_threads();
        match self {
            Self::Auto => available
                .saturating_sub(1)
                .max(MIN_SEARCH_THREADS)
                .min(AUTO_SEARCH_THREADS_CAP),
            Self::Manual(value) => value.clamp(MIN_SEARCH_THREADS, available),
            Self::Maximum => available,
        }
    }

    pub(crate) fn setting_value(self) -> String {
        match self {
            Self::Auto => "auto".to_string(),
            Self::Manual(value) => value
                .clamp(MIN_SEARCH_THREADS, available_search_threads())
                .to_string(),
            Self::Maximum => "max".to_string(),
        }
    }

    pub(crate) fn manual_value(self) -> usize {
        match self {
            Self::Manual(value) => value.clamp(MIN_SEARCH_THREADS, available_search_threads()),
            Self::Auto => self.resolve(),
            Self::Maximum => available_search_threads(),
        }
    }
}

pub(crate) fn available_search_threads() -> usize {
    std::thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(FALLBACK_SEARCH_THREADS)
        .max(MIN_SEARCH_THREADS)
}

#[derive(Clone, PartialEq)]
pub(crate) struct IndexRoot {
    pub(crate) raw: String,
    pub(crate) path: Option<PathBuf>,
    pub(crate) enabled: bool,
    pub(crate) score: i32,
    pub(crate) max_depth: usize,
    pub(crate) label: String,
    pub(crate) keywords: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SearchQueryMode {
    BlankHistory,
    NormalSearch,
    DirectoryBrowse,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryBrowseSpec {
    pub(crate) base: PathBuf,
    pub(crate) filter: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SearchQuerySpec {
    pub(crate) raw: String,
    pub(crate) search_text: String,
    pub(crate) folded_search_text: String,
    pub(crate) modifiers: Vec<String>,
    pub(crate) scoring_modifiers: Vec<String>,
    pub(crate) show_all: bool,
    pub(crate) mode: SearchQueryMode,
    pub(crate) directory: Option<DirectoryBrowseSpec>,
    pub(crate) scoring_time_unix_seconds: i64,
}

impl SearchQuerySpec {
    pub(crate) fn effective_for_scoring(&self, scoring: &ScoringConfig) -> Self {
        let mut effective = self.clone();
        effective.folded_search_text = effective_search_text_for_scoring(self, scoring);
        effective.search_text = effective.folded_search_text.clone();
        effective
    }
}

#[derive(Clone, PartialEq)]
pub(crate) struct Hotkey {
    pub(crate) modifiers: u32,
    pub(crate) key: u32,
    pub(crate) display: String,
}

#[derive(Clone)]
pub(crate) enum LaunchTarget {
    Path(PathBuf),
    Plugin(plugins::PluginTarget),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResultRankingKind {
    Heuristic,
    RecentOrder,
    QueryLaunchRule,
    Plugin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScoreExplanationRow {
    pub(crate) rule: String,
    pub(crate) input_condition: String,
    pub(crate) formula: String,
    pub(crate) score: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScoreExplanation {
    pub(crate) ranking_kind: ResultRankingKind,
    pub(crate) query: String,
    pub(crate) item_path: String,
    pub(crate) search_root: String,
    pub(crate) relative_depth: usize,
    pub(crate) final_score: i32,
    pub(crate) summary: String,
    pub(crate) rows: Vec<ScoreExplanationRow>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HeuristicScoreComponents {
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

#[derive(Clone)]
pub(crate) enum RankedCandidatePayload {
    Heuristic {
        item: LaunchItem,
        components: HeuristicScoreComponents,
        from_history: bool,
    },
    Prebuilt(SearchResult),
}

#[derive(Clone)]
pub(crate) struct RankedCandidate {
    pub(crate) payload: RankedCandidatePayload,
    pub(crate) ranking_kind: ResultRankingKind,
    pub(crate) display_score: i32,
    pub(crate) score: i32,
}

impl RankedCandidate {
    pub(crate) fn title(&self) -> &str {
        match &self.payload {
            RankedCandidatePayload::Heuristic { item, .. } => &item.title,
            RankedCandidatePayload::Prebuilt(result) => &result.title,
        }
    }

    pub(crate) fn subtitle(&self) -> &str {
        match &self.payload {
            RankedCandidatePayload::Heuristic { item, .. } => &item.subtitle,
            RankedCandidatePayload::Prebuilt(result) => &result.subtitle,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SearchResult {
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) target: LaunchTarget,
    pub(crate) is_dir: bool,
    pub(crate) from_history: bool,
    pub(crate) from_query_launch_rule: bool,
    pub(crate) ranking_kind: ResultRankingKind,
    pub(crate) explanation: Option<ScoreExplanation>,
    pub(crate) display_score: i32,
    pub(crate) score_detail: String,
    pub(crate) score: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VisibleResultSignature {
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) target: String,
    pub(crate) is_dir: bool,
    pub(crate) from_history: bool,
    pub(crate) from_query_launch_rule: bool,
    pub(crate) ranking_kind: ResultRankingKind,
    pub(crate) display_score: i32,
    pub(crate) score_detail: String,
    pub(crate) score: i32,
}

#[derive(Clone)]
pub(crate) struct RecentEntry {
    pub(crate) score: f32,
}

pub(crate) enum ResultStoreCompletion {
    Ready(ResultStoreManifest),
    Error(String),
}

pub(crate) struct SearchBatch {
    pub(crate) generation: u64,
    pub(crate) results: Vec<SearchResult>,
    pub(crate) scanned_total: usize,
    pub(crate) stage: SearchStage,
    pub(crate) done: bool,
    pub(crate) effective_limit: usize,
    pub(crate) result_store: Option<ResultStoreCompletion>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SearchStage {
    Idle,
    History,
    Folders,
    Directory,
    Done,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TooltipKind {
    ScoreBreakdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TooltipContent {
    pub(crate) kind: TooltipKind,
    pub(crate) title: String,
    pub(crate) body: String,
}

#[derive(Clone)]
pub(crate) struct PatternRule {
    pub(crate) pattern: String,
    pub(crate) folded_pattern: String,
    pub(crate) score: i32,
    pub(crate) modifiers: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct ScoringConfig {
    pub(crate) recent_first_launch_score: i32,
    pub(crate) recent_launch_increment: i32,
    pub(crate) exact_match_bonus: i32,
    pub(crate) exact_word_bonus: i32,
    pub(crate) prefix_match_bonus: i32,
    pub(crate) word_boundary_bonus: i32,
    pub(crate) consecutive_match_bonus: i32,
    pub(crate) acronym_match_bonus: i32,
    pub(crate) leftmost_match_bonus: i32,
    pub(crate) leftmost_distance_penalty: i32,
    pub(crate) length_score_weight: i32,
    pub(crate) compact_match_bonus: i32,
    pub(crate) recent_score_ceiling: i32,
    pub(crate) explicit_folder_name_match_adjustment: i32,
    pub(crate) folder_score_as_file_score_percent: i32,
    pub(crate) path_depth_penalty: i32,
    pub(crate) recency_date_bonus: i32,
    pub(crate) recency_date_enabled: bool,
    pub(crate) pattern_rules: Vec<PatternRule>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScoringRuleKind {
    Heuristic,
    Pattern,
}

#[derive(Clone, PartialEq)]
pub(crate) struct ScoringRuleEntry {
    pub(crate) kind: ScoringRuleKind,
    pub(crate) key: String,
    pub(crate) value: String,
    pub(crate) modifiers: Vec<String>,
    pub(crate) enabled: bool,
}

#[derive(Clone, PartialEq)]
pub(crate) struct RecentConfigEntry {
    pub(crate) enabled: bool,
    pub(crate) score: f32,
    pub(crate) path: String,
}

#[derive(Clone, PartialEq)]
pub(crate) struct QueryLaunchRule {
    pub(crate) query: String,
    pub(crate) target: String,
}

pub(crate) struct PathAlias {
    pub(crate) name: &'static str,
    pub(crate) env_key: Option<&'static str>,
    pub(crate) suffix: &'static str,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        let mut config = Self {
            recent_first_launch_score: RANK_RECENT_FIRST_LAUNCH_SCORE,
            recent_launch_increment: RANK_RECENT_LAUNCH_INCREMENT,
            exact_match_bonus: RANK_EXACT_MATCH_BONUS,
            exact_word_bonus: RANK_EXACT_WORD_BONUS,
            prefix_match_bonus: RANK_PREFIX_MATCH_BONUS,
            word_boundary_bonus: RANK_WORD_BOUNDARY_BONUS,
            consecutive_match_bonus: RANK_CONSECUTIVE_MATCH_BONUS,
            acronym_match_bonus: RANK_ACRONYM_MATCH_BONUS,
            leftmost_match_bonus: RANK_LEFTMOST_MATCH_BONUS,
            leftmost_distance_penalty: RANK_LEFTMOST_DISTANCE_PENALTY,
            length_score_weight: RANK_LENGTH_SCORE_WEIGHT,
            compact_match_bonus: RANK_COMPACT_MATCH_BONUS,
            recent_score_ceiling: RANK_RECENT_SCORE_CEILING,
            explicit_folder_name_match_adjustment: RANK_EXPLICIT_FOLDER_NAME_MATCH_ADJUSTMENT,
            folder_score_as_file_score_percent: 90,
            path_depth_penalty: RANK_PATH_DEPTH_PENALTY,
            recency_date_bonus: RANK_RECENCY_DATE_BONUS,
            recency_date_enabled: false,
            pattern_rules: Vec::new(),
        };
        config.pattern_rules = default_pattern_rules();
        config
    }
}

impl SearchResult {
    pub(crate) fn is_alias_result(&self) -> bool {
        matches!(self.target, LaunchTarget::Plugin(_))
    }

    #[cfg(feature = "debug-tools")]
    pub(crate) fn history_priority(&self) -> u8 {
        u8::from(self.from_history)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct WindowSettings {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

#[repr(C)]
pub(crate) struct IUnknownVTable {
    pub(crate) query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    pub(crate) add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    pub(crate) release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
pub(crate) struct IPersistFileVTable {
    pub(crate) query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    pub(crate) add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    pub(crate) release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub(crate) get_class_id: unsafe extern "system" fn(*mut c_void, *mut GUID) -> i32,
    pub(crate) is_dirty: unsafe extern "system" fn(*mut c_void) -> i32,
    pub(crate) load: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32,
    pub(crate) save: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32,
    pub(crate) save_completed: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    pub(crate) get_cur_file: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32,
}

#[repr(C)]
pub(crate) struct IShellLinkWVTable {
    pub(crate) query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    pub(crate) add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    pub(crate) release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub(crate) get_path:
        unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut c_void, u32) -> i32,
    pub(crate) get_id_list: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32,
    pub(crate) set_id_list: unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32,
    pub(crate) get_description: unsafe extern "system" fn(*mut c_void, *mut u16, i32) -> i32,
    pub(crate) set_description: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    pub(crate) get_working_directory: unsafe extern "system" fn(*mut c_void, *mut u16, i32) -> i32,
    pub(crate) set_working_directory: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    pub(crate) get_arguments: unsafe extern "system" fn(*mut c_void, *mut u16, i32) -> i32,
    pub(crate) set_arguments: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
    pub(crate) get_hotkey: unsafe extern "system" fn(*mut c_void, *mut u16) -> i32,
    pub(crate) set_hotkey: unsafe extern "system" fn(*mut c_void, u16) -> i32,
    pub(crate) get_show_cmd: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32,
    pub(crate) set_show_cmd: unsafe extern "system" fn(*mut c_void, i32) -> i32,
    pub(crate) get_icon_location:
        unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut i32) -> i32,
    pub(crate) set_icon_location: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32,
    pub(crate) set_relative_path: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32,
    pub(crate) resolve: unsafe extern "system" fn(*mut c_void, HWND, u32) -> i32,
    pub(crate) set_path: unsafe extern "system" fn(*mut c_void, *const u16) -> i32,
}

#[repr(C)]
pub(crate) struct HdropDataObjectVTable {
    pub(crate) query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    pub(crate) add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    pub(crate) release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub(crate) get_data:
        unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut STGMEDIUM) -> i32,
    pub(crate) get_data_here:
        unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut STGMEDIUM) -> i32,
    pub(crate) query_get_data: unsafe extern "system" fn(*mut c_void, *const FORMATETC) -> i32,
    pub(crate) get_canonical_format_etc:
        unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut FORMATETC) -> i32,
    pub(crate) set_data:
        unsafe extern "system" fn(*mut c_void, *const FORMATETC, *const STGMEDIUM, i32) -> i32,
    pub(crate) enum_format_etc:
        unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32,
    pub(crate) d_advise:
        unsafe extern "system" fn(*mut c_void, *const FORMATETC, u32, *mut c_void, *mut u32) -> i32,
    pub(crate) d_unadvise: unsafe extern "system" fn(*mut c_void, u32) -> i32,
    pub(crate) enum_d_advise: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32,
}

#[repr(C)]
pub(crate) struct HdropDataObject {
    pub(crate) vtbl: *const HdropDataObjectVTable,
    pub(crate) refs: AtomicU32,
    pub(crate) path: PathBuf,
}

#[repr(C)]
pub(crate) struct DropSourceVTable {
    pub(crate) query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    pub(crate) add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    pub(crate) release: unsafe extern "system" fn(*mut c_void) -> u32,
    pub(crate) query_continue_drag: unsafe extern "system" fn(*mut c_void, i32, u32) -> i32,
    pub(crate) give_feedback: unsafe extern "system" fn(*mut c_void, DROPEFFECT) -> i32,
}

#[repr(C)]
pub(crate) struct DropSource {
    pub(crate) vtbl: *const DropSourceVTable,
    pub(crate) refs: AtomicU32,
}

#[derive(Clone)]
pub(crate) struct PendingDrag {
    pub(crate) index: usize,
    pub(crate) path: PathBuf,
    pub(crate) title: String,
    pub(crate) start_x: i32,
    pub(crate) start_y: i32,
}

pub(crate) struct OleGuard;

impl OleGuard {
    pub(crate) unsafe fn initialize() -> Self {
        OleInitialize(null());
        Self
    }
}

impl Drop for OleGuard {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}
