use std::io;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::plugins::{self, PluginResult};
use crate::{
    app_config_dir, set_current_thread_background_priority, AppLanguage, SearchResult,
    WM_PLUGIN_EVENT_READY,
};

#[derive(Debug)]
pub(crate) enum PluginEvent {
    QueryResults {
        plugin_id: String,
        generation: u64,
        results: Vec<PluginResultDto>,
    },
    InvokeResult {
        plugin_id: String,
        generation: u64,
        next_query: Option<String>,
    },
    Error {
        plugin_id: String,
        generation: u64,
        message: String,
    },
}

#[derive(Debug)]
pub(crate) struct PluginResultDto {
    pub(crate) title: String,
    pub(crate) subtitle: String,
    pub(crate) score: i32,
    pub(crate) action_token: Vec<u8>,
}

static PENDING_PLUGIN_EVENTS: OnceLock<Mutex<Vec<PluginEvent>>> = OnceLock::new();

pub(crate) fn pending_plugin_events() -> &'static Mutex<Vec<PluginEvent>> {
    PENDING_PLUGIN_EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}

enum PluginCommand {
    SetWindow(isize),
    Query {
        plugin_id: String,
        generation: u64,
        query: String,
        alias: String,
        language: AppLanguage,
    },
    Invoke {
        plugin_id: String,
        generation: u64,
        action_token: Vec<u8>,
        alias: String,
        language: AppLanguage,
    },
    CancelGeneration(u64),
    Shutdown,
}

pub(crate) struct PluginSupervisor {
    sender: Option<Sender<PluginCommand>>,
    handle: Option<JoinHandle<()>>,
}

impl Default for PluginSupervisor {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("flashlaunch-plugin".to_string())
            .spawn(move || run_plugin_worker(receiver))
            .ok();
        Self {
            sender: Some(sender),
            handle,
        }
    }
}

impl PluginSupervisor {
    pub(crate) fn set_window(&self, hwnd_value: isize) {
        self.send(PluginCommand::SetWindow(hwnd_value));
    }

    pub(crate) fn query(
        &self,
        plugin_id: &str,
        alias: &str,
        language: AppLanguage,
        generation: u64,
        query: &str,
    ) -> io::Result<()> {
        validate_plugin_id(plugin_id)?;
        self.send_result(PluginCommand::Query {
            plugin_id: plugin_id.to_string(),
            generation,
            query: query.to_string(),
            alias: alias.to_string(),
            language,
        })
    }

    pub(crate) fn invoke(
        &self,
        plugin_id: &str,
        generation: u64,
        action_token: &[u8],
        alias: &str,
        language: AppLanguage,
    ) -> io::Result<()> {
        validate_plugin_id(plugin_id)?;
        self.send_result(PluginCommand::Invoke {
            plugin_id: plugin_id.to_string(),
            generation,
            action_token: action_token.to_vec(),
            alias: alias.to_string(),
            language,
        })
    }

    pub(crate) fn cancel_all(&self, generation: u64) {
        self.send(PluginCommand::CancelGeneration(generation));
    }

    pub(crate) fn update_alias(&self, plugin_id: &str, alias: &str) {
        let _ = (plugin_id, alias);
    }

    pub(crate) fn request_shutdown(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(PluginCommand::Shutdown);
        }
    }

    pub(crate) fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    fn send(&self, command: PluginCommand) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(command);
        }
    }

    fn send_result(&self, command: PluginCommand) -> io::Result<()> {
        self.sender
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "Plugin worker stopped"))?
            .send(command)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "Plugin worker stopped"))
    }
}

impl Drop for PluginSupervisor {
    fn drop(&mut self) {
        self.request_shutdown();
        self.join();
    }
}

fn validate_plugin_id(plugin_id: &str) -> io::Result<()> {
    if plugin_id == "calculator" {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unknown plugin ID",
        ))
    }
}

fn run_plugin_worker(receiver: mpsc::Receiver<PluginCommand>) {
    set_current_thread_background_priority();
    let config_dir = app_config_dir();
    let mut calculator_state = plugins::calculator::CalculatorState::load(&config_dir);
    let mut hwnd_value = 0;
    let mut cancelled_generation = 0;
    while let Ok(command) = receiver.recv() {
        let event = match command {
            PluginCommand::SetWindow(value) => {
                hwnd_value = value;
                None
            }
            PluginCommand::Query {
                plugin_id,
                generation,
                query,
                alias,
                language,
            } => {
                if generation <= cancelled_generation {
                    None
                } else {
                    let results = plugins::calculator::collect_results(
                        &query,
                        &calculator_state,
                        &alias,
                        language,
                    )
                    .unwrap_or_default()
                    .into_iter()
                    .map(plugin_result_to_dto)
                    .collect();
                    Some(PluginEvent::QueryResults {
                        plugin_id,
                        generation,
                        results,
                    })
                }
            }
            PluginCommand::Invoke {
                plugin_id,
                generation,
                action_token,
                alias,
                language,
            } => Some(
                match plugins::calculator::invoke_token(
                    &mut calculator_state,
                    &config_dir,
                    &action_token,
                    &alias,
                    language,
                ) {
                    Ok(next_query) => PluginEvent::InvokeResult {
                        plugin_id,
                        generation,
                        next_query,
                    },
                    Err(error) => PluginEvent::Error {
                        plugin_id,
                        generation,
                        message: error.to_string(),
                    },
                },
            ),
            PluginCommand::CancelGeneration(generation) => {
                cancelled_generation = cancelled_generation.max(generation);
                None
            }
            PluginCommand::Shutdown => break,
        };
        if let Some(event) = event {
            publish_plugin_event(event, hwnd_value);
        }
    }
}

fn plugin_result_to_dto(result: PluginResult) -> PluginResultDto {
    PluginResultDto {
        title: result.title,
        subtitle: result.subtitle,
        score: result.score,
        action_token: result.action_token,
    }
}

fn publish_plugin_event(event: PluginEvent, hwnd_value: isize) {
    if let Ok(mut pending) = pending_plugin_events().lock() {
        pending.push(event);
    }
    if hwnd_value != 0 {
        unsafe {
            PostMessageW(hwnd_value as _, WM_PLUGIN_EVENT_READY, 0, 0);
        }
    }
}

pub(crate) fn plugin_result_to_search_result(
    plugin_id: &str,
    result: PluginResultDto,
    include_explanation: bool,
) -> SearchResult {
    SearchResult {
        title: result.title,
        subtitle: result.subtitle,
        target: crate::LaunchTarget::Plugin(plugins::PluginTarget {
            plugin_id: plugin_id.to_string(),
            action_token: result.action_token,
        }),
        is_dir: false,
        from_history: false,
        from_query_launch_rule: false,
        ranking_kind: crate::ResultRankingKind::Plugin,
        explanation: include_explanation.then(|| {
            crate::special_score_explanation(crate::ResultRankingKind::Plugin, "", "", result.score)
        }),
        display_score: 0,
        score_detail: "plugin".to_string(),
        score: result.score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_result_conversion_preserves_action_token() {
        let dto = plugin_result_to_dto(PluginResult {
            title: "2+2 = 4".to_string(),
            subtitle: "Result".to_string(),
            score: 99,
            action_token: vec![1, 2, 3, 4],
        });
        assert_eq!(dto.action_token, vec![1, 2, 3, 4]);
    }

    #[test]
    fn unknown_plugin_is_rejected_without_starting_another_thread() {
        let mut supervisor = PluginSupervisor::default();
        assert_eq!(
            supervisor
                .query("unknown", "/x", AppLanguage::Source, 1, "test")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        supervisor.request_shutdown();
        supervisor.join();
    }

    #[test]
    fn persistent_plugin_worker_shuts_down_cleanly() {
        let mut supervisor = PluginSupervisor::default();
        supervisor
            .query("calculator", "/c", AppLanguage::Source, 1, "/c 2+2")
            .unwrap();
        supervisor.request_shutdown();
        supervisor.join();
        assert!(supervisor.handle.is_none());
    }
}
