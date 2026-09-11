use crate::*;
use std::path::PathBuf;
use std::sync::Arc;
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::*;

fn build_debug_search_report(query: &str, limit: usize) -> String {
    use std::fmt::Write as _;

    let scoring = load_scoring_config();
    let spec = parse_search_query(query).effective_for_scoring(&scoring);
    let limit = if limit == 0 {
        DEFAULT_RESULT_LIMIT
    } else {
        limit
    };

    let mut report = String::new();
    writeln!(&mut report, "Flash Launch debug search").ok();
    writeln!(&mut report, "query: {}", query).ok();
    writeln!(&mut report, "effective_text: {}", spec.folded_search_text).ok();
    writeln!(
        &mut report,
        "modifiers: {}",
        spec.scoring_modifiers.join(",")
    )
    .ok();
    writeln!(
        &mut report,
        "history_bonus: {} ceiling: {} folder_score_percent: {}",
        scoring.recent_launch_increment,
        scoring.recent_score_ceiling,
        scoring.folder_score_as_file_score_percent
    )
    .ok();
    writeln!(&mut report).ok();

    let breakdowns = debug_search_breakdowns(query, limit);
    for (index, breakdown) in breakdowns.iter().enumerate() {
        let path = match &breakdown.result.target {
            LaunchTarget::Path(path) => path.to_string_lossy().to_string(),
            LaunchTarget::Plugin(_) => String::new(),
        };
        writeln!(
            &mut report,
            "#{} [{}] score={} history_priority={} title={}",
            index + 1,
            if breakdown.result.from_history {
                "history"
            } else {
                "index"
            },
            breakdown.final_score,
            breakdown.result.history_priority(),
            breakdown.result.title
        )
        .ok();
        writeln!(&mut report, "    subtitle: {}", breakdown.result.subtitle).ok();
        writeln!(&mut report, "    path: {}", path).ok();
        writeln!(
            &mut report,
            "    text={} index={} pattern={} history={} recency={} type={} path_penalty={} final={}",
            breakdown.text_score,
            breakdown.index_score,
            breakdown.pattern_score,
            breakdown.history_score,
            breakdown.recency_score,
            breakdown.folder_score,
            breakdown.path_penalty,
            breakdown.final_score
        )
        .ok();
    }
    if breakdowns.is_empty() {
        writeln!(&mut report, "No matches.").ok();
    }
    report
}

fn write_debug_search_report(output_path: &str, report: &str) -> std::io::Result<()> {
    let path = PathBuf::from(output_path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, report)
}

fn write_debug_stdout(report: &str) -> std::io::Result<()> {
    {
        use std::io::Write as _;
        let mut stdout = std::io::stdout();
        if stdout
            .write_all(report.as_bytes())
            .and_then(|_| stdout.flush())
            .is_ok()
        {
            return Ok(());
        }
    }
    unsafe {
        let mut handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            AttachConsole(ATTACH_PARENT_PROCESS);
            handle = GetStdHandle(STD_OUTPUT_HANDLE);
        }
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            use std::io::Write as _;
            return std::io::stdout().write_all(report.as_bytes());
        }

        let mut mode = 0u32;
        if GetConsoleMode(handle, &mut mode) != 0 {
            let wide = report.encode_utf16().collect::<Vec<_>>();
            let mut offset = 0usize;
            while offset < wide.len() {
                let chunk_len = (wide.len() - offset).min(32_768);
                let mut written = 0u32;
                if WriteConsoleW(
                    handle,
                    wide[offset..offset + chunk_len].as_ptr().cast(),
                    chunk_len as u32,
                    &mut written,
                    null_mut(),
                ) == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if written == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to write debug search output",
                    ));
                }
                offset += written as usize;
            }
            Ok(())
        } else {
            let bytes = report.as_bytes();
            let mut offset = 0usize;
            while offset < bytes.len() {
                let chunk_len = (bytes.len() - offset).min(1_048_576);
                let mut written = 0u32;
                if WriteFile(
                    handle,
                    bytes[offset..offset + chunk_len].as_ptr().cast(),
                    chunk_len as u32,
                    &mut written,
                    null_mut(),
                ) == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if written == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to write redirected debug search output",
                    ));
                }
                offset += written as usize;
            }
            Ok(())
        }
    }
}

pub(crate) fn run_debug_search(
    query: &str,
    limit: usize,
    output_path: Option<&str>,
) -> std::io::Result<()> {
    let report = build_debug_search_report(query, limit);
    if let Some(path) = output_path.filter(|path| !path.trim().is_empty()) {
        if path == "-" {
            write_debug_stdout(&report)?;
        } else {
            write_debug_search_report(path, &report)?;
        }
    } else {
        write_debug_stdout(&report)?;
    }
    Ok(())
}

fn run_bench_refinement_sequence(
    threads: usize,
    runs: usize,
    output_path: Option<&str>,
) -> std::io::Result<()> {
    use std::fmt::Write as _;

    let runs = runs.max(1);
    let threads = threads.clamp(MIN_SEARCH_THREADS, available_search_threads());
    let roots = load_index_roots();
    let root_plan = Arc::new(RootOwnershipPlan::build(&roots));
    let scoring = Arc::new(load_scoring_config());
    let recent_items = Arc::new(load_recent_items());
    let query_launch_rules = Arc::new(load_query_launch_rules());
    let effective_limit = load_result_limit();
    let mut elapsed_by_step = vec![Vec::with_capacity(runs); 3];
    let mut last_samples = Vec::new();

    for _ in 0..runs {
        let samples = benchmark_refinement_sequence_once(
            Arc::clone(&root_plan),
            Arc::clone(&scoring),
            Arc::clone(&recent_items),
            Arc::clone(&query_launch_rules),
            effective_limit,
            threads,
        );
        for (index, sample) in samples.iter().enumerate() {
            elapsed_by_step[index].push(sample.elapsed_ms);
        }
        last_samples = samples;
    }

    let mut report = String::new();
    writeln!(&mut report, "Flash Launch refinement benchmark").ok();
    writeln!(&mut report, "sequence: chr -> chro -> chrom").ok();
    writeln!(&mut report, "threads: {}", threads).ok();
    writeln!(&mut report, "runs: {}", runs).ok();
    for (index, sample) in last_samples.iter().enumerate() {
        let mut elapsed = elapsed_by_step[index].clone();
        elapsed.sort_by(f64::total_cmp);
        let median = elapsed[elapsed.len() / 2];
        writeln!(
            &mut report,
            "step: {} | median_ms: {:.3} | filesystem_entries: {} | cache_candidates: {} | cache_bytes: {} | reused_candidates: {} | full_scan_fallback: {} | query_version: {} | scan_done: {}",
            sample.query,
            median,
            sample.metrics.filesystem_entries,
            sample.metrics.cache_candidates,
            sample.metrics.cache_bytes,
            sample.metrics.reused_candidates,
            sample.metrics.full_scan_fallback,
            sample.metrics.query_version,
            sample.metrics.scan_done,
        )
        .ok();
    }

    if let Some(path) = output_path.filter(|path| !path.trim().is_empty()) {
        if path == "-" {
            write_debug_stdout(&report)?;
        } else {
            write_debug_search_report(path, &report)?;
        }
    } else {
        write_debug_stdout(&report)?;
    }
    Ok(())
}

pub(crate) fn run_bench_search(
    query: &str,
    threads: usize,
    runs: usize,
    output_path: Option<&str>,
) -> std::io::Result<()> {
    if query == REFINEMENT_BENCHMARK_SENTINEL {
        return run_bench_refinement_sequence(threads, runs, output_path);
    }
    use std::fmt::Write as _;
    use std::time::Instant;

    let runs = runs.max(1);
    let threads = threads.clamp(MIN_SEARCH_THREADS, available_search_threads());
    let roots = load_index_roots();
    let root_plan = Arc::new(RootOwnershipPlan::build(&roots));
    let scoring = Arc::new(load_scoring_config());
    let recent_items = Arc::new(load_recent_items());
    let query_launch_rules = Arc::new(load_query_launch_rules());
    let effective_limit = load_result_limit();
    let mut elapsed_ms = Vec::with_capacity(runs);
    let mut last_result = None;

    for _ in 0..runs {
        let started = Instant::now();
        let result = benchmark_live_scan_once(
            query,
            Arc::clone(&root_plan),
            Arc::clone(&scoring),
            Arc::clone(&recent_items),
            Arc::clone(&query_launch_rules),
            effective_limit,
            threads,
        );
        elapsed_ms.push(started.elapsed().as_secs_f64() * 1000.0);
        last_result = Some(result);
    }

    let result = last_result.expect("benchmark must run at least once");
    let mut sorted = elapsed_ms.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    let mut report = String::new();
    writeln!(&mut report, "Flash Launch live scan benchmark").ok();
    writeln!(&mut report, "query: {}", query).ok();
    writeln!(&mut report, "threads: {}", threads).ok();
    writeln!(
        &mut report,
        "runs_ms: {}",
        elapsed_ms
            .iter()
            .map(|value| format!("{value:.3}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
    .ok();
    writeln!(&mut report, "median_ms: {median:.3}").ok();
    writeln!(&mut report, "scanned: {}", result.scanned_total).ok();
    writeln!(&mut report, "workers: {}", result.workers).ok();
    writeln!(
        &mut report,
        "live_scan_strategy: {}",
        result.live_scan_strategy
    )
    .ok();
    writeln!(&mut report, "results: {}", result.result_count()).ok();
    if let Some(top_result) = result.results.first() {
        writeln!(
            &mut report,
            "top_result: {} | {}",
            top_result.title, top_result.subtitle
        )
        .ok();
    }

    if let Some(path) = output_path.filter(|path| !path.trim().is_empty()) {
        if path == "-" {
            write_debug_stdout(&report)?;
        } else {
            write_debug_search_report(path, &report)?;
        }
    } else {
        write_debug_stdout(&report)?;
    }
    Ok(())
}
