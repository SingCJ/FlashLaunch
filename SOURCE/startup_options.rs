use crate::*;

#[cfg(feature = "debug-tools")]
pub(crate) const REFINEMENT_BENCHMARK_SENTINEL: &str = "__refinement_chr_chro_chrom__";

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct StartupOptions {
    pub(crate) open_settings: bool,
    pub(crate) settings_page: Option<SettingsPage>,
    pub(crate) show_main: bool,
    pub(crate) hide_existing: bool,
    #[cfg(feature = "debug-tools")]
    pub(crate) debug_search: Option<String>,
    #[cfg(feature = "debug-tools")]
    pub(crate) debug_limit: usize,
    #[cfg(feature = "debug-tools")]
    pub(crate) debug_output: Option<String>,
    #[cfg(feature = "debug-tools")]
    pub(crate) bench_search: Option<String>,
    #[cfg(feature = "debug-tools")]
    pub(crate) bench_threads: usize,
    #[cfg(feature = "debug-tools")]
    pub(crate) bench_runs: usize,
}

pub(crate) fn parse_startup_options<I, S>(args: I) -> StartupOptions
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_string())
        .collect::<Vec<_>>();
    let mut options = StartupOptions::default();
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].to_ascii_lowercase();
        match arg.as_str() {
            "--settings" | "/settings" => options.open_settings = true,
            "--settings-general" | "/settings-general" => {
                options.open_settings = true;
                options.settings_page = Some(SettingsPage::General);
            }
            "--settings-page" | "/settings-page" => {
                options.open_settings = true;
                if let Some(page) = args
                    .get(index + 1)
                    .and_then(|value| value.parse::<i32>().ok())
                {
                    options.settings_page = Some(settings_page_from_nav_index(page));
                    index += 1;
                }
            }
            "--show" | "/show" => {
                options.show_main = true;
                options.hide_existing = false;
            }
            "--hidden" | "/hidden" | "--hide" | "/hide" => {
                options.show_main = false;
                options.hide_existing = true;
            }
            #[cfg(feature = "debug-tools")]
            "--debug-search" | "/debug-search" => {
                if let Some(query) = args.get(index + 1) {
                    options.debug_search = Some(query.clone());
                    index += 1;
                }
            }
            #[cfg(feature = "debug-tools")]
            "--debug-limit" | "/debug-limit" => {
                if let Some(limit) = args.get(index + 1).and_then(|value| value.parse().ok()) {
                    options.debug_limit = limit;
                    index += 1;
                }
            }
            #[cfg(feature = "debug-tools")]
            "--debug-output" | "/debug-output" => {
                if let Some(path) = args.get(index + 1) {
                    options.debug_output = Some(path.clone());
                    index += 1;
                }
            }
            #[cfg(feature = "debug-tools")]
            "--bench-refinement" | "/bench-refinement" => {
                options.bench_search = Some(REFINEMENT_BENCHMARK_SENTINEL.to_string());
            }
            #[cfg(feature = "debug-tools")]
            "--bench-search" | "/bench-search" => {
                if let Some(query) = args.get(index + 1) {
                    options.bench_search = Some(query.clone());
                    index += 1;
                }
            }
            #[cfg(feature = "debug-tools")]
            "--threads" | "/threads" => {
                if let Some(threads) = args.get(index + 1) {
                    options.bench_threads = parse_search_threads(threads).resolve();
                    index += 1;
                }
            }
            #[cfg(feature = "debug-tools")]
            "--runs" | "/runs" => {
                if let Some(runs) = args.get(index + 1).and_then(|value| value.parse().ok()) {
                    options.bench_runs = runs;
                    index += 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_startup_visibility_options() {
        assert_eq!(
            parse_startup_options(std::iter::empty::<&str>()),
            StartupOptions::default()
        );

        let show = parse_startup_options(["--show"]);
        assert!(show.show_main);
        assert!(!show.hide_existing);

        let hidden = parse_startup_options(["/hidden"]);
        assert!(!hidden.show_main);
        assert!(hidden.hide_existing);

        let settings = parse_startup_options(["/settings"]);
        assert!(settings.open_settings);

        let settings_page = parse_startup_options(["--settings", "--settings-page", "3"]);
        assert!(settings_page.open_settings);
        assert_eq!(
            settings_page.settings_page,
            Some(SettingsPage::PatternScoring)
        );
    }

    #[cfg(feature = "debug-tools")]
    #[test]
    fn parses_debug_startup_options() {
        let debug = parse_startup_options([
            "--debug-search",
            "note",
            "--debug-limit",
            "12",
            "--debug-output",
            "TEMP\\debug-note.txt",
        ]);
        assert_eq!(debug.debug_search.as_deref(), Some("note"));
        assert_eq!(debug.debug_limit, 12);
        assert_eq!(debug.debug_output.as_deref(), Some("TEMP\\debug-note.txt"));

        let refinement = parse_startup_options(["--bench-refinement"]);
        assert_eq!(
            refinement.bench_search.as_deref(),
            Some(REFINEMENT_BENCHMARK_SENTINEL)
        );

        let bench =
            parse_startup_options(["--bench-search", "tnt", "--threads", "20", "--runs", "5"]);
        assert_eq!(bench.bench_search.as_deref(), Some("tnt"));
        assert_eq!(bench.bench_threads, 20);
        assert_eq!(bench.bench_runs, 5);
    }
}
