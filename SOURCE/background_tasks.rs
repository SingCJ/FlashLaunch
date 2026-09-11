use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::*;

const ICON_WORKER_BATCH_LIMIT: usize = 32;
const ICON_WORKER_PENDING_LIMIT: usize = 64;
const UI_RESULT_BUDGET: Duration = Duration::from_millis(4);

pub(crate) struct IconWorker {
    sender: Option<mpsc::Sender<IconWorkerCommand>>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IconLoadRequest {
    pub(crate) session_generation: u64,
    pub(crate) generation: u64,
    pub(crate) hwnd_value: isize,
    pub(crate) key: String,
    pub(crate) path: PathBuf,
}

pub(crate) struct IconLoadResult {
    pub(crate) session_generation: u64,
    pub(crate) generation: u64,
    pub(crate) key: String,
    pub(crate) icon_value: isize,
    pub(crate) helper_error: Option<String>,
}

enum IconWorkerCommand {
    BeginSession {
        session_generation: u64,
        hwnd_value: isize,
    },
    EndSession(u64),
    Load(IconLoadRequest),
    CancelGeneration(u64),
    Shutdown,
}

impl IconWorker {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("flashlaunch-icon".to_string())
            .spawn(move || run_icon_worker(receiver))
            .ok();
        Self {
            sender: Some(sender),
            handle,
        }
    }

    pub(crate) fn begin_session(&self, session_generation: u64, hwnd_value: isize) {
        self.send(IconWorkerCommand::BeginSession {
            session_generation,
            hwnd_value,
        });
    }

    pub(crate) fn end_session(&self, session_generation: u64) {
        self.send(IconWorkerCommand::EndSession(session_generation));
    }

    pub(crate) fn load(&self, request: IconLoadRequest) {
        self.send(IconWorkerCommand::Load(request));
    }

    pub(crate) fn advance_generation(&self, generation: u64) {
        self.send(IconWorkerCommand::CancelGeneration(generation));
    }

    pub(crate) fn request_shutdown(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(IconWorkerCommand::Shutdown);
        }
    }

    pub(crate) fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    fn send(&self, command: IconWorkerCommand) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(command);
        }
    }
}

impl Drop for IconWorker {
    fn drop(&mut self) {
        self.request_shutdown();
        self.join();
    }
}

fn run_icon_worker(receiver: Receiver<IconWorkerCommand>) {
    set_current_thread_background_priority();
    let mut helper: Option<crate::icon_helper::IconHelperClient> = None;
    let mut active_session = 0;
    let mut failed_session = 0;
    let mut latest_generation = 0;
    let mut hwnd_value = 0;
    let mut requests = VecDeque::new();

    while let Ok(command) = receiver.recv() {
        if !collect_icon_command(
            command,
            &mut helper,
            &mut active_session,
            &mut failed_session,
            &mut latest_generation,
            &mut hwnd_value,
            &mut requests,
        ) {
            break;
        }
        while requests.len() < ICON_WORKER_BATCH_LIMIT {
            match receiver.try_recv() {
                Ok(command) => {
                    if !collect_icon_command(
                        command,
                        &mut helper,
                        &mut active_session,
                        &mut failed_session,
                        &mut latest_generation,
                        &mut hwnd_value,
                        &mut requests,
                    ) {
                        if let Some(mut helper) = helper.take() {
                            helper.shutdown();
                        }
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        for request in requests.drain(..requests.len().min(ICON_WORKER_BATCH_LIMIT)) {
            if request.session_generation != active_session
                || request.generation < latest_generation
                || failed_session == active_session
            {
                continue;
            }
            let Some(current_helper) = helper.as_mut() else {
                continue;
            };
            let icon_value = match current_helper.load_icon(&request.path) {
                Ok(icon) => icon.unwrap_or(null_mut()) as isize,
                Err(error) => {
                    crate::log_helper_error(
                        "Icon",
                        &format!(
                            "Skipped icon for {} after a communication failure: {error}",
                            request.path.display()
                        ),
                    );
                    publish_icon_result(
                        request.hwnd_value,
                        IconLoadResult {
                            session_generation: request.session_generation,
                            generation: request.generation,
                            key: request.key,
                            icon_value: 0,
                            helper_error: None,
                        },
                    );
                    match crate::icon_helper::IconHelperClient::start() {
                        Ok(restarted_helper) => helper = Some(restarted_helper),
                        Err(restart_error) => {
                            failed_session = active_session;
                            publish_icon_result(
                                request.hwnd_value,
                                IconLoadResult {
                                    session_generation: request.session_generation,
                                    generation: request.generation,
                                    key: String::new(),
                                    icon_value: 0,
                                    helper_error: Some(format!(
                                        "Icon helper restart failed after {error}: {restart_error}"
                                    )),
                                },
                            );
                            break;
                        }
                    }
                    continue;
                }
            };
            if request.session_generation != active_session
                || request.generation < latest_generation
            {
                if icon_value != 0 {
                    unsafe { DestroyIcon(icon_value as HICON) };
                }
                continue;
            }
            publish_icon_result(
                request.hwnd_value,
                IconLoadResult {
                    session_generation: request.session_generation,
                    generation: request.generation,
                    key: request.key,
                    icon_value,
                    helper_error: None,
                },
            );
        }
    }
    if let Some(mut helper) = helper {
        helper.shutdown();
    }
}

fn collect_icon_command(
    command: IconWorkerCommand,
    helper: &mut Option<crate::icon_helper::IconHelperClient>,
    active_session: &mut u64,
    failed_session: &mut u64,
    latest_generation: &mut u64,
    hwnd_value: &mut isize,
    requests: &mut VecDeque<IconLoadRequest>,
) -> bool {
    match command {
        IconWorkerCommand::BeginSession {
            session_generation,
            hwnd_value: next_hwnd,
        } => {
            if let Some(mut current) = helper.take() {
                current.shutdown();
            }
            *active_session = session_generation;
            *failed_session = 0;
            *hwnd_value = next_hwnd;
            requests.clear();
            match crate::icon_helper::IconHelperClient::start() {
                Ok(client) => *helper = Some(client),
                Err(error) => {
                    *failed_session = session_generation;
                    publish_icon_result(
                        next_hwnd,
                        IconLoadResult {
                            session_generation,
                            generation: *latest_generation,
                            key: String::new(),
                            icon_value: 0,
                            helper_error: Some(error.to_string()),
                        },
                    );
                }
            }
        }
        IconWorkerCommand::EndSession(session_generation) => {
            if session_generation == *active_session {
                requests.clear();
                if let Some(mut current) = helper.take() {
                    current.shutdown();
                }
                *active_session = 0;
            }
        }
        IconWorkerCommand::Load(request) => {
            if request.session_generation == *active_session
                && request.generation >= *latest_generation
                && *failed_session != *active_session
            {
                *latest_generation = (*latest_generation).max(request.generation);
                requests.retain(|pending| pending.key != request.key);
                while requests.len() >= ICON_WORKER_PENDING_LIMIT {
                    requests.pop_front();
                }
                requests.push_back(request);
            }
        }
        IconWorkerCommand::CancelGeneration(generation) => {
            *latest_generation = (*latest_generation).max(generation);
            requests.retain(|request| request.generation >= *latest_generation);
        }
        IconWorkerCommand::Shutdown => return false,
    }
    true
}

fn publish_icon_result(hwnd_value: isize, result: IconLoadResult) {
    if let Ok(mut pending) = pending_icon_slot().lock() {
        pending.retain(|item| item.generation >= result.generation && item.key != result.key);
        pending.push(result);
        while pending.len() > ICON_WORKER_PENDING_LIMIT {
            let removed = pending.remove(0);
            if removed.icon_value != 0 {
                unsafe { DestroyIcon(removed.icon_value as HICON) };
            }
        }
    }
    unsafe {
        PostMessageW(hwnd_value as HWND, WM_ICON_READY, 0, 0);
    }
}

pub(crate) struct LaunchWorker {
    sender: Option<mpsc::Sender<LaunchWorkerCommand>>,
    handle: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShellWorkerAction {
    Launch,
    OpenFolder,
    OpenLinkedLocation,
    Properties,
    ContextMenu,
}

impl ShellWorkerAction {
    fn operation(self) -> crate::shell_helper::ShellOperation {
        match self {
            Self::Launch => crate::shell_helper::ShellOperation::Launch,
            Self::OpenFolder => crate::shell_helper::ShellOperation::Open,
            Self::OpenLinkedLocation => crate::shell_helper::ShellOperation::OpenLinkedLocation,
            Self::Properties => crate::shell_helper::ShellOperation::Properties,
            Self::ContextMenu => crate::shell_helper::ShellOperation::ContextMenu,
        }
    }
}

#[derive(Clone)]
pub(crate) struct LaunchWorkerRequest {
    pub(crate) action: ShellWorkerAction,
    pub(crate) session_generation: u64,
    pub(crate) generation: u64,
    pub(crate) hwnd_value: isize,
    pub(crate) title: String,
    pub(crate) path: PathBuf,
    pub(crate) launched_query: String,
}

pub(crate) struct LaunchWorkerResult {
    pub(crate) action: ShellWorkerAction,
    pub(crate) session_generation: u64,
    pub(crate) generation: u64,
    pub(crate) hwnd_value: isize,
    pub(crate) title: String,
    pub(crate) path: PathBuf,
    pub(crate) launched_query: String,
    pub(crate) kind: LaunchWorkerResultKind,
}

pub(crate) enum LaunchWorkerResultKind {
    Launched,
    MissingShortcutTarget { target: PathBuf },
    HelperError(String),
    NoLinkedLocation,
    Error(String),
}

pub(crate) struct HelperStatusResult {
    pub(crate) session_generation: u64,
    pub(crate) helper: &'static str,
    pub(crate) error: Option<String>,
}

static PENDING_HELPER_STATUS: OnceLock<Mutex<Vec<HelperStatusResult>>> = OnceLock::new();

pub(crate) fn pending_helper_status_slot() -> &'static Mutex<Vec<HelperStatusResult>> {
    PENDING_HELPER_STATUS.get_or_init(|| Mutex::new(Vec::new()))
}

enum LaunchWorkerCommand {
    BeginSession {
        session_generation: u64,
        hwnd_value: isize,
    },
    EndSession(u64),
    Launch(LaunchWorkerRequest),
    Shutdown,
}

impl LaunchWorker {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("flashlaunch-launch".to_string())
            .spawn(move || run_launch_worker(receiver))
            .ok();
        Self {
            sender: Some(sender),
            handle,
        }
    }

    pub(crate) fn begin_session(&self, session_generation: u64, hwnd_value: isize) {
        self.send(LaunchWorkerCommand::BeginSession {
            session_generation,
            hwnd_value,
        });
    }

    pub(crate) fn end_session(&self, session_generation: u64) {
        self.send(LaunchWorkerCommand::EndSession(session_generation));
    }

    pub(crate) fn launch(&self, request: LaunchWorkerRequest) -> bool {
        self.sender
            .as_ref()
            .is_some_and(|sender| sender.send(LaunchWorkerCommand::Launch(request)).is_ok())
    }

    pub(crate) fn request_shutdown(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(LaunchWorkerCommand::Shutdown);
        }
    }

    pub(crate) fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    fn send(&self, command: LaunchWorkerCommand) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(command);
        }
    }
}

impl Drop for LaunchWorker {
    fn drop(&mut self) {
        self.request_shutdown();
        self.join();
    }
}

fn run_launch_worker(receiver: Receiver<LaunchWorkerCommand>) {
    let mut helper: Option<crate::shell_helper::ShellHelperClient> = None;
    let mut active_session = 0;
    let mut failed_session = 0;
    while let Ok(command) = receiver.recv() {
        match command {
            LaunchWorkerCommand::BeginSession {
                session_generation,
                hwnd_value: next_hwnd,
            } => {
                if let Some(client) = helper.take() {
                    client.shutdown();
                }
                active_session = session_generation;
                failed_session = 0;
                match crate::shell_helper::ShellHelperClient::start() {
                    Ok(client) => {
                        helper = Some(client);
                        publish_helper_status(
                            next_hwnd,
                            HelperStatusResult {
                                session_generation,
                                helper: "Shell",
                                error: None,
                            },
                        );
                    }
                    Err(error) => {
                        failed_session = session_generation;
                        publish_helper_status(
                            next_hwnd,
                            HelperStatusResult {
                                session_generation,
                                helper: "Shell",
                                error: Some(error.to_string()),
                            },
                        );
                    }
                }
            }
            LaunchWorkerCommand::EndSession(session_generation) => {
                if session_generation == active_session {
                    if let Some(client) = helper.take() {
                        client.shutdown();
                    }
                    active_session = 0;
                }
            }
            LaunchWorkerCommand::Launch(request) => {
                let kind = if request.session_generation != active_session {
                    LaunchWorkerResultKind::HelperError(
                        "Shell helper is not available for this focus session.".to_string(),
                    )
                } else if failed_session == active_session {
                    LaunchWorkerResultKind::HelperError(
                        "Shell helper failed to start for this focus session.".to_string(),
                    )
                } else if let Some(client) = helper.as_mut() {
                    match client.transact(request.action.operation(), &request.path) {
                        Ok(crate::shell_helper::ShellHelperResult::Success) => {
                            LaunchWorkerResultKind::Launched
                        }
                        Ok(crate::shell_helper::ShellHelperResult::MissingShortcutTarget(
                            target,
                        )) => LaunchWorkerResultKind::MissingShortcutTarget { target },
                        Ok(crate::shell_helper::ShellHelperResult::Error(error)) => {
                            LaunchWorkerResultKind::Error(error)
                        }
                        Ok(crate::shell_helper::ShellHelperResult::NoLinkedLocation) => {
                            LaunchWorkerResultKind::NoLinkedLocation
                        }
                        Ok(unexpected) => LaunchWorkerResultKind::Error(format!(
                            "Shell helper returned an unexpected result: {unexpected:?}"
                        )),
                        Err(error) => {
                            client.abort();
                            helper = None;
                            failed_session = active_session;
                            LaunchWorkerResultKind::HelperError(error.to_string())
                        }
                    }
                } else {
                    LaunchWorkerResultKind::HelperError(
                        "Shell helper is not connected for this focus session.".to_string(),
                    )
                };
                publish_launch_result(LaunchWorkerResult {
                    action: request.action,
                    session_generation: request.session_generation,
                    generation: request.generation,
                    hwnd_value: request.hwnd_value,
                    title: request.title,
                    path: request.path,
                    launched_query: request.launched_query,
                    kind,
                });
            }
            LaunchWorkerCommand::Shutdown => break,
        }
    }
    if let Some(client) = helper {
        client.shutdown();
    }
}

fn publish_helper_status(hwnd_value: isize, result: HelperStatusResult) {
    if let Ok(mut pending) = pending_helper_status_slot().lock() {
        pending.push(result);
    }
    unsafe {
        PostMessageW(hwnd_value as HWND, WM_HELPER_STATUS_READY, 0, 0);
    }
}

fn publish_launch_result(result: LaunchWorkerResult) {
    let hwnd_value = result.hwnd_value;
    if let Ok(mut pending) = pending_launch_slot().lock() {
        pending.push(result);
    }
    unsafe {
        PostMessageW(hwnd_value as HWND, WM_LAUNCH_RESULT_READY, 0, 0);
    }
}

pub(crate) struct ConfigSaveResult {
    pub(crate) generation: u64,
    pub(crate) current: ConfigSnapshot,
    pub(crate) ignored_optional: Vec<String>,
    pub(crate) error: Option<String>,
}

static PENDING_CONFIG_SAVES: OnceLock<Mutex<Vec<ConfigSaveResult>>> = OnceLock::new();

pub(crate) fn pending_config_save_slot() -> &'static Mutex<Vec<ConfigSaveResult>> {
    PENDING_CONFIG_SAVES.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) struct FullConfigReload {
    pub(crate) settings: AppSettingsSnapshot,
    pub(crate) plugin_state: plugins::PluginState,
    pub(crate) search_history: Vec<String>,
    pub(crate) query_launch_rules: Vec<QueryLaunchRule>,
    pub(crate) recent_items: Vec<String>,
    pub(crate) config_recent_items: Vec<RecentConfigEntry>,
    pub(crate) scoring_rules: Vec<ScoringRuleEntry>,
}

pub(crate) struct ConfigReloadResult {
    pub(crate) generation: u64,
    pub(crate) full: Option<FullConfigReload>,
    pub(crate) search_roots: Vec<IndexRoot>,
    pub(crate) search_scoring: ScoringConfig,
}

static PENDING_CONFIG_RELOADS: OnceLock<Mutex<Vec<ConfigReloadResult>>> = OnceLock::new();

pub(crate) fn pending_config_reload_slot() -> &'static Mutex<Vec<ConfigReloadResult>> {
    PENDING_CONFIG_RELOADS.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) struct SaveWorker {
    sender: Option<mpsc::Sender<SaveWorkerCommand>>,
    handle: Option<JoinHandle<()>>,
}

pub(crate) struct FileTaskResult {
    pub(crate) generation: u64,
    pub(crate) path: PathBuf,
    pub(crate) kind: FileTaskResultKind,
}

pub(crate) enum FileTaskResultKind {
    ShortcutDeleted,
    Error(String),
}

static PENDING_FILE_TASKS: OnceLock<Mutex<Vec<FileTaskResult>>> = OnceLock::new();

pub(crate) fn pending_file_task_slot() -> &'static Mutex<Vec<FileTaskResult>> {
    PENDING_FILE_TASKS.get_or_init(|| Mutex::new(Vec::new()))
}

enum SaveWorkerCommand {
    Recent(Vec<String>),
    QueryRules(Vec<QueryLaunchRule>),
    DeleteShortcut {
        generation: u64,
        hwnd_value: isize,
        path: PathBuf,
    },
    Task(Box<dyn FnOnce() + Send>),
    Shutdown,
}

impl SaveWorker {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("flashlaunch-save".to_string())
            .spawn(move || run_save_worker(receiver))
            .ok();
        Self {
            sender: Some(sender),
            handle,
        }
    }

    pub(crate) fn save_recent(&self, items: Vec<String>) {
        self.send(SaveWorkerCommand::Recent(items));
    }

    pub(crate) fn save_query_rules(&self, items: Vec<QueryLaunchRule>) {
        self.send(SaveWorkerCommand::QueryRules(items));
    }

    pub(crate) fn delete_shortcut(&self, generation: u64, hwnd_value: isize, path: PathBuf) {
        self.send(SaveWorkerCommand::DeleteShortcut {
            generation,
            hwnd_value,
            path,
        });
    }

    pub(crate) fn run_task(&self, task: impl FnOnce() + Send + 'static) {
        self.send(SaveWorkerCommand::Task(Box::new(task)));
    }

    pub(crate) fn save_config(
        &self,
        generation: u64,
        hwnd_value: isize,
        current: ConfigSnapshot,
        original: Option<ConfigSnapshot>,
    ) {
        self.run_task(move || {
            let result = run_config_save_io(generation, current, original);
            if let Ok(mut pending) = pending_config_save_slot().lock() {
                pending.push(result);
            }
            unsafe {
                PostMessageW(hwnd_value as HWND, WM_CONFIG_SAVE_READY, 0, 0);
            }
        });
    }

    pub(crate) fn reload_config(&self, generation: u64, hwnd_value: isize, full: bool) {
        self.run_task(move || {
            let plugin_state = full.then(|| plugins::PluginState::load(&app_config_dir()));
            let full = plugin_state.map(|plugin_state| {
                let scoring_text = read_hydrated_document(
                    &scoring_path(),
                    &default_scoring_text(),
                    hydrate_scoring_text,
                )
                .unwrap_or_else(|_| default_scoring_text());
                FullConfigReload {
                    settings: load_app_settings_snapshot(),
                    search_history: load_search_history(&plugin_state),
                    query_launch_rules: load_query_launch_rules(),
                    recent_items: load_recent_items(),
                    config_recent_items: load_recent_config_entries(),
                    scoring_rules: parse_scoring_rule_entries(&scoring_text),
                    plugin_state,
                }
            });
            let result = ConfigReloadResult {
                generation,
                full,
                search_roots: load_index_roots(),
                search_scoring: load_scoring_config(),
            };
            if let Ok(mut pending) = pending_config_reload_slot().lock() {
                pending.push(result);
            }
            unsafe {
                PostMessageW(hwnd_value as HWND, WM_CONFIG_RELOAD_READY, 0, 0);
            }
        });
    }

    pub(crate) fn request_shutdown(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(SaveWorkerCommand::Shutdown);
        }
    }

    pub(crate) fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    fn send(&self, command: SaveWorkerCommand) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(command);
        }
    }
}

impl Drop for SaveWorker {
    fn drop(&mut self) {
        self.request_shutdown();
        self.join();
    }
}

fn run_save_worker(receiver: Receiver<SaveWorkerCommand>) {
    set_current_thread_background_priority();
    while let Ok(command) = receiver.recv() {
        let mut recent = None;
        let mut query_rules = None;
        let mut file_tasks = Vec::new();
        let mut shutdown = false;
        collect_save_command(
            command,
            &mut recent,
            &mut query_rules,
            &mut file_tasks,
            &mut shutdown,
        );
        loop {
            match receiver.try_recv() {
                Ok(command) => collect_save_command(
                    command,
                    &mut recent,
                    &mut query_rules,
                    &mut file_tasks,
                    &mut shutdown,
                ),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    shutdown = true;
                    break;
                }
            }
        }
        if let Some(items) = recent {
            let _ = save_recent_items(&items);
        }
        if let Some(items) = query_rules {
            let _ = save_query_launch_rules(&items);
        }
        for (generation, hwnd_value, path) in file_tasks {
            let kind = match fs::remove_file(&path) {
                Ok(()) => FileTaskResultKind::ShortcutDeleted,
                Err(error) => FileTaskResultKind::Error(error.to_string()),
            };
            if let Ok(mut pending) = pending_file_task_slot().lock() {
                pending.push(FileTaskResult {
                    generation,
                    path,
                    kind,
                });
            }
            unsafe {
                PostMessageW(hwnd_value as HWND, WM_FILE_TASK_READY, 0, 0);
            }
        }
        if shutdown {
            break;
        }
    }
}

fn collect_save_command(
    command: SaveWorkerCommand,
    recent: &mut Option<Vec<String>>,
    query_rules: &mut Option<Vec<QueryLaunchRule>>,
    file_tasks: &mut Vec<(u64, isize, PathBuf)>,
    shutdown: &mut bool,
) {
    match command {
        SaveWorkerCommand::Recent(items) => *recent = Some(items),
        SaveWorkerCommand::QueryRules(items) => *query_rules = Some(items),
        SaveWorkerCommand::DeleteShortcut {
            generation,
            hwnd_value,
            path,
        } => file_tasks.push((generation, hwnd_value, path)),
        SaveWorkerCommand::Task(task) => task(),
        SaveWorkerCommand::Shutdown => *shutdown = true,
    }
}

fn run_config_save_io(
    generation: u64,
    mut current: ConfigSnapshot,
    original: Option<ConfigSnapshot>,
) -> ConfigSaveResult {
    let changed = |predicate: fn(&ConfigSnapshot, &ConfigSnapshot) -> bool| {
        original
            .as_ref()
            .is_none_or(|saved| predicate(saved, &current))
    };
    let mut required_writes = Vec::new();
    let mut optional_writes = Vec::new();
    let mut ignored_optional = HashSet::new();
    let config_dir = app_config_dir();
    if let Err(error) = fs::create_dir_all(&config_dir) {
        return config_save_error(generation, current, error.to_string());
    }

    let mut settings_updates: Vec<(&str, String)> = Vec::new();
    if changed(|saved, current| saved.hotkey != current.hotkey) {
        settings_updates.push(("hotkey", current.hotkey.display.clone()));
    }
    if changed(|saved, current| saved.result_limit != current.result_limit) {
        settings_updates.push((
            "result_limit",
            current.result_limit.max(MIN_RESULT_LIMIT).to_string(),
        ));
    }
    if changed(|saved, current| saved.search_threads != current.search_threads) {
        settings_updates.push(("search_threads", current.search_threads.setting_value()));
    }
    if changed(|saved, current| saved.language != current.language) {
        settings_updates.push(("language", current.language.setting_value().to_string()));
    }
    if changed(|saved, current| saved.show_score_breakdown != current.show_score_breakdown) {
        settings_updates.push((
            "show_score_breakdown",
            bool_setting(current.show_score_breakdown),
        ));
    }
    if changed(|saved, current| {
        saved.show_score_breakdown_tooltip != current.show_score_breakdown_tooltip
    }) {
        settings_updates.push((
            "show_score_breakdown_tooltip",
            bool_setting(current.show_score_breakdown_tooltip),
        ));
    }
    if changed(|saved, current| saved.popup_sound != current.popup_sound) {
        settings_updates.push(("popup_sound", bool_setting(current.popup_sound)));
    }
    if changed(|saved, current| saved.tooltip_opacity_percent != current.tooltip_opacity_percent) {
        settings_updates.push((
            "tooltip_opacity_percent",
            current
                .tooltip_opacity_percent
                .clamp(MIN_TOOLTIP_OPACITY_PERCENT, MAX_TOOLTIP_OPACITY_PERCENT)
                .to_string(),
        ));
    }
    if changed(|saved, current| saved.show_cpu_in_title != current.show_cpu_in_title) {
        settings_updates.push(("show_cpu_in_title", bool_setting(current.show_cpu_in_title)));
    }
    if changed(|saved, current| saved.show_ram_in_title != current.show_ram_in_title) {
        settings_updates.push(("show_ram_in_title", bool_setting(current.show_ram_in_title)));
    }
    if changed(|saved, current| {
        saved.show_build_timestamp_in_title != current.show_build_timestamp_in_title
    }) {
        settings_updates.push((
            "show_build_timestamp_in_title",
            bool_setting(current.show_build_timestamp_in_title),
        ));
    }
    if changed(|saved, current| saved.autostart != current.autostart) {
        settings_updates.push(("autostart", bool_setting(current.autostart)));
    }
    if !settings_updates.is_empty() {
        match read_hydrated_document(
            &settings_path(),
            &default_settings_text(),
            hydrate_settings_text,
        ) {
            Ok(existing) => required_writes.push(PendingConfigWrite {
                label: "settings.ini",
                path: settings_path(),
                content: render_settings_text_with_updates(&existing, &settings_updates),
            }),
            Err(error) => return config_save_error(generation, current, error.to_string()),
        }
    }

    if changed(|saved, current| saved.config_roots != current.config_roots) {
        match read_hydrated_document(&index_path(), &default_index_text(), hydrate_index_text) {
            Ok(existing) => required_writes.push(PendingConfigWrite {
                label: "index_folders.txt",
                path: index_path(),
                content: merge_index_roots_text(&existing, &current.config_roots),
            }),
            Err(error) => return config_save_error(generation, current, error.to_string()),
        }
    }
    if changed(|saved, current| saved.scoring_rules != current.scoring_rules) {
        match read_hydrated_document(
            &scoring_path(),
            &default_scoring_text(),
            hydrate_scoring_text,
        ) {
            Ok(existing) => required_writes.push(PendingConfigWrite {
                label: "scoring.ini",
                path: scoring_path(),
                content: merge_scoring_rule_entries_text(&existing, &current.scoring_rules),
            }),
            Err(error) => return config_save_error(generation, current, error.to_string()),
        }
    }

    if changed(|saved, current| saved.config_recent_items != current.config_recent_items) {
        prepare_optional_write(
            "recent_items.txt",
            recent_items_path(),
            "",
            |existing| merge_recent_config_entries_text(existing, &current.config_recent_items),
            &mut optional_writes,
            &mut ignored_optional,
        );
    }
    if changed(|saved, current| saved.config_search_history != current.config_search_history) {
        prepare_optional_write(
            "search_history.txt",
            search_history_path(),
            "",
            |existing| merge_search_history_items_text(existing, &current.config_search_history),
            &mut optional_writes,
            &mut ignored_optional,
        );
    }
    if changed(|saved, current| {
        saved.config_query_launch_rules != current.config_query_launch_rules
    }) {
        prepare_optional_write(
            "query_launch_rules.txt",
            query_launch_rules_path(),
            "",
            |existing| merge_query_launch_rules_text(existing, &current.config_query_launch_rules),
            &mut optional_writes,
            &mut ignored_optional,
        );
    }
    if changed(|saved, current| saved.config_plugin_aliases != current.config_plugin_aliases) {
        let plugin_state = plugins::PluginState::load(&config_dir);
        let defaults = plugin_state.alias_entries_to_text(&current.config_plugin_aliases);
        let path = plugins::plugin_aliases_path(&config_dir);
        match read_hydrated_document(&path, &defaults, plugins::hydrate_aliases_text) {
            Ok(existing) => optional_writes.push(PendingConfigWrite {
                label: "plugin_aliases.ini",
                path,
                content: plugin_state
                    .merge_alias_entries_text(&existing, &current.config_plugin_aliases),
            }),
            Err(_) => {
                ignored_optional.insert("plugin_aliases.ini".to_string());
            }
        }
    }

    if let Err(error) = transactional_write_text_files(required_writes) {
        return config_save_error(generation, current, error.to_string());
    }
    for write in optional_writes {
        if atomic_write_text(&write.path, &write.content).is_err() {
            ignored_optional.insert(write.label.to_string());
        }
    }

    if let Some(saved) = &original {
        if ignored_optional.contains("recent_items.txt") {
            current.config_recent_items = saved.config_recent_items.clone();
        }
        if ignored_optional.contains("search_history.txt") {
            current.config_search_history = saved.config_search_history.clone();
        }
        if ignored_optional.contains("query_launch_rules.txt") {
            current.config_query_launch_rules = saved.config_query_launch_rules.clone();
        }
        if ignored_optional.contains("plugin_aliases.ini") {
            current.config_plugin_aliases = saved.config_plugin_aliases.clone();
        }
    }

    ConfigSaveResult {
        generation,
        current,
        ignored_optional: ignored_optional.into_iter().collect(),
        error: None,
    }
}

fn bool_setting(value: bool) -> String {
    if value { "1" } else { "0" }.to_string()
}

fn config_save_error(generation: u64, current: ConfigSnapshot, error: String) -> ConfigSaveResult {
    ConfigSaveResult {
        generation,
        current,
        ignored_optional: Vec::new(),
        error: Some(error),
    }
}

fn read_hydrated_document(
    path: &std::path::Path,
    defaults: &str,
    hydrate: fn(&str, &str) -> String,
) -> std::io::Result<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(hydrate(&content, defaults)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(defaults.to_string()),
        Err(error) => Err(error),
    }
}

fn prepare_optional_write(
    label: &'static str,
    path: PathBuf,
    defaults: &str,
    render: impl FnOnce(&str) -> String,
    writes: &mut Vec<PendingConfigWrite>,
    ignored: &mut HashSet<String>,
) {
    match read_hydrated_document(&path, defaults, |content, _| content.to_string()) {
        Ok(existing) => writes.push(PendingConfigWrite {
            label,
            path,
            content: render(&existing),
        }),
        Err(_) => {
            ignored.insert(label.to_string());
        }
    }
}

pub(crate) fn ui_result_budget_exhausted(started: Instant) -> bool {
    started.elapsed() >= UI_RESULT_BUDGET
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icon_request(session_generation: u64, generation: u64, key: &str) -> IconLoadRequest {
        IconLoadRequest {
            session_generation,
            generation,
            hwnd_value: 0,
            key: key.to_string(),
            path: PathBuf::from(format!(r"C:\Icons\{key}.exe")),
        }
    }

    #[test]
    fn submitting_slow_file_task_returns_immediately() {
        let mut worker = SaveWorker::new();
        let started = Instant::now();
        worker.run_task(|| thread::sleep(Duration::from_millis(150)));
        assert!(started.elapsed() < Duration::from_millis(50));
        worker.request_shutdown();
        worker.join();
    }

    #[test]
    fn stale_icon_generation_is_discarded_without_waiting() {
        let (sender, receiver) = mpsc::channel();
        let mut helper = None;
        let mut active_session = 4;
        let mut failed_session = 0;
        let mut latest_generation = 8;
        let mut hwnd_value = 0;
        let mut requests = VecDeque::new();
        sender
            .send(IconWorkerCommand::Load(icon_request(4, 7, "old")))
            .unwrap();
        let command = receiver.recv().unwrap();
        assert!(collect_icon_command(
            command,
            &mut helper,
            &mut active_session,
            &mut failed_session,
            &mut latest_generation,
            &mut hwnd_value,
            &mut requests,
        ));
        assert!(requests.is_empty());
    }

    #[test]
    fn icon_queue_coalesces_duplicate_keys() {
        let mut requests = VecDeque::new();
        requests.push_back(icon_request(2, 5, "same"));
        let replacement = icon_request(2, 6, "same");
        requests.retain(|pending| pending.key != replacement.key);
        requests.push_back(replacement);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests.front().unwrap().generation, 6);
    }
}
