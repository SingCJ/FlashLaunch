use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::*;

const WINDOW_WIDTH: i32 = 980;
const WINDOW_HEIGHT: i32 = 620;
const MARGIN: i32 = 12;
const HEADER_HEIGHT: i32 = 104;
const BUTTON_WIDTH: i32 = 96;
const BUTTON_HEIGHT: i32 = 30;

fn ranking_kind_text(kind: ResultRankingKind, language: AppLanguage) -> &'static str {
    match kind {
        ResultRankingKind::Plugin => localized(language, "Plugin"),
        ResultRankingKind::RecentOrder => localized(language, "Recent order"),
        ResultRankingKind::QueryLaunchRule => localized(language, "Query Launch Rule"),
        ResultRankingKind::Heuristic => localized(language, "Heuristic"),
    }
}

pub(crate) fn score_explanation_text(
    explanation: &ScoreExplanation,
    language: AppLanguage,
) -> String {
    let mut lines = vec![
        localized_format1(
            language,
            "Ranking: {}",
            ranking_kind_text(explanation.ranking_kind, language),
        ),
        localized_format1(language, "Query: {}", &explanation.query),
        localized_format1(language, "Item path: {}", &explanation.item_path),
        localized_format1(
            language,
            "Search Folder root: {}",
            if explanation.search_root.is_empty() {
                localized(language, "N/A")
            } else {
                &explanation.search_root
            },
        ),
        localized_format1(language, "Relative depth: {}", explanation.relative_depth),
        localized_format1(language, "Final score: {}", explanation.final_score),
        String::new(),
        format!(
            "{}\t{}\t{}\t{}",
            localized(language, "Rule"),
            localized(language, "Input / Condition"),
            localized(language, "Formula"),
            localized(language, "Score"),
        ),
    ];
    lines.extend(explanation.rows.iter().map(|row| {
        format!(
            "{}\t{}\t{}\t{}",
            row.rule, row.input_condition, row.formula, row.score
        )
    }));
    lines.join("\r\n")
}

fn score_header_text(explanation: &ScoreExplanation, language: AppLanguage) -> String {
    localized_format3(
        language,
        "{}\r\nQuery: {}\r\nItem: {}",
        &explanation.summary,
        if explanation.query.is_empty() {
            localized(language, "(blank)")
        } else {
            &explanation.query
        },
        if explanation.item_path.is_empty() {
            localized(language, "N/A")
        } else {
            &explanation.item_path
        },
    ) + &localized_format3(
        language,
        "\r\nSearch Folder: {}    Relative depth: {}    Final score: {}",
        if explanation.search_root.is_empty() {
            localized(language, "N/A")
        } else {
            &explanation.search_root
        },
        explanation.relative_depth,
        explanation.final_score,
    )
}

impl AppState {
    pub(crate) unsafe fn show_score_breakdown(&mut self, index: usize) {
        let Some(result) = self.result_at(index) else {
            return;
        };
        let query = get_window_text(self.edit);
        let mut explanation = if result.ranking_kind == ResultRankingKind::Heuristic {
            result_target_path(&result)
                .and_then(|path| {
                    let root = recent_path_root(&path, &self.search_root_plan);
                    let item = launch_item_from_path(path, &root, &self.search_scoring)?;
                    let spec =
                        parse_search_query(&query).effective_for_scoring(&self.search_scoring);
                    let recent_map = recent_lookup(&self.recent_items);
                    score_breakdown_with_config(
                        &item,
                        &spec,
                        &recent_map,
                        &self.search_scoring,
                        result.from_history,
                    )
                    .and_then(|breakdown| breakdown.result.explanation)
                })
                .unwrap_or_else(|| {
                    special_score_explanation(
                        result.ranking_kind,
                        &query,
                        &result_target_text(&result).unwrap_or_default(),
                        result.display_score,
                    )
                })
        } else {
            special_score_explanation(
                result.ranking_kind,
                &query,
                &result_target_text(&result).unwrap_or_default(),
                result.display_score,
            )
        };
        explanation.query = query;
        if explanation.item_path.is_empty() {
            explanation.item_path =
                result_target_text(&result).unwrap_or_else(|| result.subtitle.clone());
        }
        self.score_explanation = Some(explanation);
        if self.score_breakdown_hwnd.is_null() {
            self.create_score_breakdown_window();
        }
        self.refresh_score_breakdown_window();
        ShowWindow(self.score_breakdown_hwnd, SW_SHOWNORMAL);
        SetForegroundWindow(self.score_breakdown_hwnd);
    }

    unsafe fn create_score_breakdown_window(&mut self) {
        self.score_breakdown_hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            wide(SCORE_BREAKDOWN_CLASS_NAME).as_ptr(),
            wide(localized(self.language, "Score Breakdown")).as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            WINDOW_WIDTH,
            WINDOW_HEIGHT,
            self.hwnd,
            null_mut(),
            self.instance,
            null(),
        );
        if self.score_breakdown_hwnd.is_null() {
            return;
        }
        self.score_breakdown_header = CreateWindowExW(
            0,
            wide("STATIC").as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            0,
            0,
            self.score_breakdown_hwnd,
            null_mut(),
            self.instance,
            null(),
        );
        self.score_breakdown_list = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            WC_LISTVIEWW,
            null(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_VSCROLL | LVS_REPORT | LVS_SHOWSELALWAYS,
            0,
            0,
            0,
            0,
            self.score_breakdown_hwnd,
            ID_SCORE_BREAKDOWN_LIST as isize as _,
            self.instance,
            null(),
        );
        configure_report_list_view_with_options(self.score_breakdown_list, false, true);
        reset_list_view_columns(
            self.score_breakdown_list,
            &[
                (localized(self.language, "Rule"), 220),
                (localized(self.language, "Input / Condition"), 290),
                (localized(self.language, "Formula"), 320),
                (localized(self.language, "Score"), 80),
            ],
        );
        self.score_breakdown_copy = CreateWindowExW(
            0,
            wide("BUTTON").as_ptr(),
            wide(localized(self.language, "Copy All")).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            0,
            0,
            0,
            self.score_breakdown_hwnd,
            ID_SCORE_BREAKDOWN_COPY as isize as _,
            self.instance,
            null(),
        );
        self.score_breakdown_close = CreateWindowExW(
            0,
            wide("BUTTON").as_ptr(),
            wide(localized(self.language, "Close")).as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON as u32,
            0,
            0,
            0,
            0,
            self.score_breakdown_hwnd,
            ID_SCORE_BREAKDOWN_CLOSE as isize as _,
            self.instance,
            null(),
        );
        self.layout_score_breakdown_window();
    }

    pub(crate) unsafe fn layout_score_breakdown_window(&self) {
        if self.score_breakdown_hwnd.is_null() {
            return;
        }
        let mut rect: RECT = std::mem::zeroed();
        if GetClientRect(self.score_breakdown_hwnd, &mut rect) == 0 {
            return;
        }
        let width = (rect.right - rect.left).max(1);
        let height = (rect.bottom - rect.top).max(1);
        let buttons_y = (height - MARGIN - BUTTON_HEIGHT).max(MARGIN);
        let list_y = MARGIN + HEADER_HEIGHT;
        let list_height = (buttons_y - MARGIN - list_y).max(80);
        MoveWindow(
            self.score_breakdown_header,
            MARGIN,
            MARGIN,
            width - MARGIN * 2,
            HEADER_HEIGHT,
            TRUE,
        );
        MoveWindow(
            self.score_breakdown_list,
            MARGIN,
            list_y,
            width - MARGIN * 2,
            list_height,
            TRUE,
        );
        MoveWindow(
            self.score_breakdown_close,
            width - MARGIN - BUTTON_WIDTH,
            buttons_y,
            BUTTON_WIDTH,
            BUTTON_HEIGHT,
            TRUE,
        );
        MoveWindow(
            self.score_breakdown_copy,
            width - MARGIN * 2 - BUTTON_WIDTH * 2,
            buttons_y,
            BUTTON_WIDTH,
            BUTTON_HEIGHT,
            TRUE,
        );
        reset_list_view_columns(
            self.score_breakdown_list,
            &[
                (localized(self.language, "Rule"), 220),
                (localized(self.language, "Input / Condition"), 290),
                (localized(self.language, "Formula"), 320),
                (localized(self.language, "Score"), 80),
            ],
        );
    }

    unsafe fn refresh_score_breakdown_window(&self) {
        let Some(explanation) = self.score_explanation.as_ref() else {
            return;
        };
        set_window_text(
            self.score_breakdown_header,
            &score_header_text(explanation, self.language),
        );
        clear_list_view(self.score_breakdown_list);
        for row in &explanation.rows {
            let score = row.score.to_string();
            add_list_view_row_with_param(
                self.score_breakdown_list,
                false,
                &[&row.rule, &row.input_condition, &row.formula, &score],
                0,
            );
        }
    }

    pub(crate) unsafe fn copy_score_breakdown(&self) {
        if let Some(explanation) = self.score_explanation.as_ref() {
            set_clipboard_text(
                self.score_breakdown_hwnd,
                &score_explanation_text(explanation, self.language),
            );
        }
    }
}

pub(crate) unsafe extern "system" fn score_breakdown_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_SIZE => {
            with_app(|app| unsafe { app.layout_score_breakdown_window() });
            0
        }
        WM_COMMAND => {
            match loword(wparam) as i32 {
                ID_SCORE_BREAKDOWN_COPY => {
                    with_app(|app| unsafe { app.copy_score_breakdown() });
                }
                ID_SCORE_BREAKDOWN_CLOSE => {
                    ShowWindow(hwnd, SW_HIDE);
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        WM_GETMINMAXINFO => {
            let info = lparam as *mut MINMAXINFO;
            if !info.is_null() {
                (*info).ptMinTrackSize.x = 720;
                (*info).ptMinTrackSize.y = 440;
            }
            0
        }
        WM_DESTROY => {
            with_app(|app| {
                app.score_breakdown_hwnd = null_mut();
                app.score_breakdown_header = null_mut();
                app.score_breakdown_list = null_mut();
                app.score_breakdown_copy = null_mut();
                app.score_breakdown_close = null_mut();
            });
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}
