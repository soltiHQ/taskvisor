//! Shared benchmark presentation and result reporting.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use serde::Deserialize;

const REPORT_WIDTH: usize = 92;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Lifecycle,
    Intake,
    Policy,
    Query,
}

impl Scope {
    const fn badge(self) -> &'static str {
        match self {
            Self::Lifecycle => "FULL LIFECYCLE",
            Self::Intake => "INTAKE ONLY",
            Self::Policy => "POLICY DECISION",
            Self::Query => "QUERY",
        }
    }

    const fn color(self) -> AnsiColor {
        match self {
            Self::Lifecycle => AnsiColor::BrightGreen,
            Self::Intake => AnsiColor::BrightBlue,
            Self::Policy => AnsiColor::BrightYellow,
            Self::Query => AnsiColor::BrightMagenta,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CaseFamily {
    pub group_id: &'static str,
    pub title: &'static str,
    pub scope: Scope,
    pub unit_singular: &'static str,
    pub unit_plural: &'static str,
    pub boundary: &'static str,
    pub outside: &'static str,
    pub interpretation: Interpretation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interpretation {
    ManagedTaskLifecycle,
    Neutral,
}

impl CaseFamily {
    pub const fn lifecycle(
        group_id: &'static str,
        title: &'static str,
        unit_singular: &'static str,
        unit_plural: &'static str,
        boundary: &'static str,
        outside: &'static str,
    ) -> Self {
        Self {
            group_id,
            title,
            scope: Scope::Lifecycle,
            unit_singular,
            unit_plural,
            boundary,
            outside,
            interpretation: Interpretation::ManagedTaskLifecycle,
        }
    }

    pub const fn intake(
        group_id: &'static str,
        title: &'static str,
        unit_singular: &'static str,
        unit_plural: &'static str,
        boundary: &'static str,
        outside: &'static str,
    ) -> Self {
        Self {
            group_id,
            title,
            scope: Scope::Intake,
            unit_singular,
            unit_plural,
            boundary,
            outside,
            interpretation: Interpretation::Neutral,
        }
    }

    pub const fn policy(
        group_id: &'static str,
        title: &'static str,
        unit_singular: &'static str,
        unit_plural: &'static str,
        boundary: &'static str,
        outside: &'static str,
    ) -> Self {
        Self {
            group_id,
            title,
            scope: Scope::Policy,
            unit_singular,
            unit_plural,
            boundary,
            outside,
            interpretation: Interpretation::Neutral,
        }
    }

    pub const fn query(
        group_id: &'static str,
        title: &'static str,
        unit_singular: &'static str,
        unit_plural: &'static str,
        boundary: &'static str,
        outside: &'static str,
    ) -> Self {
        Self {
            group_id,
            title,
            scope: Scope::Query,
            unit_singular,
            unit_plural,
            boundary,
            outside,
            interpretation: Interpretation::Neutral,
        }
    }

    pub const fn without_lifecycle_interpretation(mut self) -> Self {
        self.interpretation = Interpretation::Neutral;
        self
    }
}

#[derive(Clone, Debug)]
struct RecordedCase {
    full_id: String,
    family: CaseFamily,
}

static RECORDED_CASES: OnceLock<Mutex<Vec<RecordedCase>>> = OnceLock::new();

pub fn record_case(family: CaseFamily, function_id: &str, value_str: Option<String>) {
    let full_id = match value_str {
        Some(value) => format!("{}/{function_id}/{value}", family.group_id),
        None => format!("{}/{function_id}", family.group_id),
    };
    let cases = RECORDED_CASES.get_or_init(|| Mutex::new(Vec::new()));
    let mut cases = cases.lock().expect("benchmark result recorder is poisoned");
    if !cases.iter().any(|case| case.full_id == full_id) {
        cases.push(RecordedCase { full_id, family });
    }
}

pub fn print_suite_header(suite: &str) {
    if !statistical_run_requested() {
        return;
    }
    static PRINTED: OnceLock<()> = OnceLock::new();
    PRINTED.get_or_init(|| {
        let logical_cpus = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        let cpu = cpu_model();
        let revision = git_revision();
        let cyan = style(AnsiColor::BrightCyan, true);
        let dim = Style::new().dimmed();
        let mut out = output();
        let title = format!("TASKVISOR BENCHMARK · {}", suite.to_uppercase());
        let platform = format!(
            "{} · {} · {logical_cpus} logical CPUs",
            display_os(std::env::consts::OS),
            std::env::consts::ARCH,
        );
        let build = revision.map_or_else(
            || format!("taskvisor {}", env!("CARGO_PKG_VERSION")),
            |revision| format!("taskvisor {} · {revision}", env!("CARGO_PKG_VERSION")),
        );

        writeln!(out).ok();
        write_header_top(&mut out, &title, cyan);
        if let Some(cpu) = cpu {
            write_header_row(&mut out, "CPU", &cpu, cyan);
        }
        write_header_row(&mut out, "Platform", &platform, cyan);
        write_header_row(&mut out, "Build", &build, cyan);
        write_header_row(&mut out, "Features", &enabled_features(), cyan);
        write_header_bottom(&mut out, cyan);
        writeln!(
            out,
            "{dim}MEASURED = statistical estimate from this run · PROJECT HEURISTIC = orientation, not an SLA{dim:#}"
        )
        .ok();
        writeln!(
            out,
            "{dim}Tip: add --quiet for a clean product snapshot; --color always forces color.{dim:#}"
        )
        .ok();
        writeln!(out).ok();
    });
}

fn write_header_top(out: &mut AutoStream<std::io::Stdout>, title: &str, accent: Style) {
    let fill = REPORT_WIDTH.saturating_sub(title.chars().count() + 5);
    writeln!(out, "{accent}╭─ {title} {}╮{accent:#}", "─".repeat(fill)).ok();
}

fn write_header_row(
    out: &mut AutoStream<std::io::Stdout>,
    label: &str,
    value: &str,
    accent: Style,
) {
    const LABEL_WIDTH: usize = 10;

    let inner_width = REPORT_WIDTH - 4;
    let value_width = inner_width - LABEL_WIDTH;
    for (index, line) in wrap_words(value, value_width).iter().enumerate() {
        let label = if index == 0 { label } else { "" };
        let label = format!("{label:<width$}", width = LABEL_WIDTH);
        let padding = inner_width.saturating_sub(label.chars().count() + line.chars().count());
        writeln!(
            out,
            "{accent}│{accent:#} {accent}{label}{accent:#}{line}{} {accent}│{accent:#}",
            " ".repeat(padding),
        )
        .ok();
    }
}

fn write_header_bottom(out: &mut AutoStream<std::io::Stdout>, accent: Style) {
    writeln!(out, "{accent}╰{}╯{accent:#}", "─".repeat(REPORT_WIDTH - 2),).ok();
}

fn display_os(os: &str) -> &str {
    match os {
        "linux" => "Linux",
        "macos" => "macOS",
        "windows" => "Windows",
        other => other,
    }
}

pub fn benchmark_main(suite: &'static str, run: fn()) {
    let saved_estimates = if statistical_run_requested() && !discard_baseline_requested() {
        snapshot_saved_estimates(&criterion_root())
    } else {
        HashMap::new()
    };
    run();
    criterion::Criterion::default()
        .configure_from_args()
        .final_summary();
    print_performance_snapshot(suite, &saved_estimates);
}

#[derive(Deserialize)]
struct SavedBenchmark {
    group_id: String,
    function_id: Option<String>,
    value_str: Option<String>,
    throughput: Option<HashMap<String, u64>>,
    full_id: String,
}

#[derive(Clone, Copy, Deserialize)]
struct ConfidenceInterval {
    confidence_level: f64,
    lower_bound: f64,
    upper_bound: f64,
}

#[derive(Clone, Copy, Deserialize)]
struct Estimate {
    confidence_interval: ConfidenceInterval,
    point_estimate: f64,
}

#[derive(Deserialize)]
struct Estimates {
    mean: Estimate,
    slope: Option<Estimate>,
}

struct Observation {
    case: RecordedCase,
    function_id: String,
    value_str: Option<String>,
    units: u64,
    time: Estimate,
}

#[derive(PartialEq, Eq)]
struct SavedEstimateState {
    modified: SystemTime,
    bytes: Vec<u8>,
}

fn print_performance_snapshot(suite: &str, saved_estimates: &HashMap<PathBuf, SavedEstimateState>) {
    if !statistical_run_requested() {
        return;
    }
    if discard_baseline_requested() {
        let mut out = output();
        writeln!(
            out,
            "\nNo Taskvisor snapshot: --discard-baseline does not save estimates."
        )
        .ok();
        return;
    }

    let cases = RECORDED_CASES
        .get()
        .map(|cases| {
            cases
                .lock()
                .expect("benchmark result recorder is poisoned")
                .clone()
        })
        .unwrap_or_default();
    if cases.is_empty() {
        return;
    }

    let root = criterion_root();
    let mut observations = Vec::new();
    for case in cases {
        match load_observation(&root, case, saved_estimates) {
            Ok(observation) => observations.push(observation),
            Err(error) => {
                let yellow = style(AnsiColor::BrightYellow, true);
                let mut out = output();
                writeln!(
                    out,
                    "{yellow}Taskvisor report skipped one case: {error}{yellow:#}"
                )
                .ok();
            }
        }
    }
    if observations.is_empty() {
        return;
    }

    let cyan = style(AnsiColor::BrightCyan, true);
    let dim = Style::new().dimmed();
    let mut out = output();
    let title = format!("TASKVISOR PERFORMANCE SNAPSHOT · {}", suite.to_uppercase());
    writeln!(out).ok();
    write_header_top(&mut out, &title, cyan);
    write_header_row(&mut out, "Cases", &observations.len().to_string(), cyan);
    write_header_row(
        &mut out,
        "Source",
        "absolute estimates from this benchmark invocation",
        cyan,
    );
    write_header_bottom(&mut out, cyan);
    writeln!(out).ok();

    let mut lifecycle_rates = Vec::new();
    for observation in &observations {
        print_observation(&mut out, observation);
        if observation.case.family.interpretation == Interpretation::ManagedTaskLifecycle {
            lifecycle_rates.push((
                rate(observation.units, observation.time.point_estimate),
                rate(
                    observation.units,
                    observation.time.confidence_interval.upper_bound,
                ),
                rate(
                    observation.units,
                    observation.time.confidence_interval.lower_bound,
                ),
            ));
        }
    }

    writeln!(out, "{cyan}BOTTOM LINE{cyan:#}").ok();
    if lifecycle_rates.is_empty() {
        writeln!(
            out,
            "  Managed-task range  not measured in this filtered run"
        )
        .ok();
    } else {
        let min = lifecycle_rates
            .iter()
            .map(|rates| rates.0)
            .fold(f64::INFINITY, f64::min);
        let max = lifecycle_rates
            .iter()
            .map(|rates| rates.0)
            .fold(f64::NEG_INFINITY, f64::max);
        writeln!(
            out,
            "  Managed-task range  {}–{} completed task lifecycles/s",
            format_rate(min),
            format_rate(max),
        )
        .ok();
        let reading = if let [(_, low, high)] = lifecycle_rates.as_slice() {
            lifecycle_grade(*low, *high).label.to_ascii_lowercase()
        } else {
            "see the CI-aware label on each lifecycle case".to_owned()
        };
        writeln!(out, "  Project reading     {reading}").ok();
    }
    writeln!(
        out,
        "  Other measurements  reported separately; never mixed into managed-task throughput"
    )
    .ok();
    writeln!(
        out,
        "  Validation          every reported case completed without assertion failure"
    )
    .ok();
    writeln!(
        out,
        "  Host conditions     {dim}background load and power mode are not verified by this reporter{dim:#}"
    )
    .ok();
    if noplot_requested() {
        writeln!(out, "  HTML report         disabled by --noplot").ok();
    } else {
        let report_path = report_path_for_display(&root);
        writeln!(
            out,
            "  HTML report         {}",
            report_path.display()
        )
        .ok();
    }
    writeln!(
        out,
        "{dim}Project reading aid only; not an SLO, certification, or production capacity promise.{dim:#}"
    )
    .ok();
    writeln!(out).ok();
}

fn load_observation(
    root: &Path,
    case: RecordedCase,
    saved_estimates: &HashMap<PathBuf, SavedEstimateState>,
) -> Result<Observation, String> {
    let mut candidates = Vec::new();
    collect_benchmark_files(root, &mut candidates).map_err(|error| error.to_string())?;
    let mut matched = None;
    for benchmark_path in candidates {
        let bytes = fs::read(&benchmark_path).map_err(|error| error.to_string())?;
        let benchmark: SavedBenchmark =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if benchmark.full_id == case.full_id {
            matched = Some((benchmark_path, benchmark));
            break;
        }
    }
    let (benchmark_path, benchmark) =
        matched.ok_or_else(|| format!("missing Criterion result for {}", case.full_id))?;
    if benchmark.group_id != case.family.group_id {
        return Err(format!("unexpected benchmark family for {}", case.full_id));
    }
    let units = benchmark
        .throughput
        .as_ref()
        .and_then(|throughput| throughput.get("Elements"))
        .copied()
        .ok_or_else(|| format!("missing Elements throughput for {}", case.full_id))?;
    let estimates_path = benchmark_path
        .parent()
        .expect("benchmark.json has a parent")
        .join("estimates.json");
    let current_estimate =
        saved_estimate_state(&estimates_path).map_err(|error| error.to_string())?;
    if saved_estimates
        .get(&estimates_path)
        .is_some_and(|saved| saved == &current_estimate)
    {
        return Err(format!("stale Criterion estimate for {}", case.full_id));
    }
    let estimates: Estimates =
        serde_json::from_slice(&current_estimate.bytes).map_err(|error| error.to_string())?;
    let time = estimates.slope.unwrap_or(estimates.mean);

    Ok(Observation {
        case,
        function_id: benchmark.function_id.unwrap_or_else(|| "case".to_owned()),
        value_str: benchmark.value_str,
        units,
        time,
    })
}

fn snapshot_saved_estimates(root: &Path) -> HashMap<PathBuf, SavedEstimateState> {
    let mut benchmark_files = Vec::new();
    if collect_benchmark_files(root, &mut benchmark_files).is_err() {
        return HashMap::new();
    }
    benchmark_files
        .into_iter()
        .filter_map(|benchmark_path| {
            let estimates_path = benchmark_path.parent()?.join("estimates.json");
            saved_estimate_state(&estimates_path)
                .ok()
                .map(|state| (estimates_path, state))
        })
        .collect()
}

fn saved_estimate_state(path: &Path) -> std::io::Result<SavedEstimateState> {
    Ok(SavedEstimateState {
        modified: fs::metadata(path)?.modified()?,
        bytes: fs::read(path)?,
    })
}

fn collect_benchmark_files(root: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "new") {
                let benchmark = path.join("benchmark.json");
                if benchmark.is_file() {
                    files.push(benchmark);
                }
            } else {
                collect_benchmark_files(&path, files)?;
            }
        }
    }
    Ok(())
}

fn print_observation(out: &mut AutoStream<std::io::Stdout>, observation: &Observation) {
    let family = observation.case.family;
    let accent = style(family.scope.color(), true);
    let dim = Style::new().dimmed();
    let point_rate = rate(observation.units, observation.time.point_estimate);
    let low_rate = rate(
        observation.units,
        observation.time.confidence_interval.upper_bound,
    );
    let high_rate = rate(
        observation.units,
        observation.time.confidence_interval.lower_bound,
    );
    let unit_ns = observation.time.point_estimate / observation.units as f64;
    let details = observation.value_str.as_deref().map_or_else(
        || display_runtime(&observation.function_id),
        |value| {
            format!(
                "{} · {}",
                display_runtime(&observation.function_id),
                humanize(value)
            )
        },
    );

    writeln!(
        out,
        "{accent}┌─ ● MEASURED · {} · {}{accent:#}",
        family.scope.badge(),
        family.title,
    )
    .ok();
    writeln!(out, "{accent}│{accent:#} {details}").ok();
    writeln!(out, "{accent}│{accent:#}").ok();
    writeln!(
        out,
        "{accent}│ {} {}/s{accent:#}",
        format_rate(point_rate),
        family.unit_plural,
    )
    .ok();
    let readable_rate = if family.scope == Scope::Lifecycle {
        format!(
            "{} {} each second across this measured lifecycle",
            format_count(point_rate),
            family.unit_plural,
        )
    } else {
        format!(
            "{} {} each second at this measured boundary",
            format_count(point_rate),
            family.unit_plural,
        )
    };
    write_wrapped_field(out, accent, "≈ ", &readable_rate, None);
    let cost_label = if observation.units > 1 {
        "amortized per"
    } else {
        "per"
    };
    writeln!(
        out,
        "{accent}│{accent:#} {} {cost_label} {}",
        format_duration(unit_ns),
        family.unit_singular,
    )
    .ok();
    if observation.units > 1 {
        let unit_label =
            pluralize_for_count(family.unit_singular, family.unit_plural, observation.units);
        writeln!(
            out,
            "{accent}│{accent:#} {} for the complete batch of {} {}",
            format_duration(observation.time.point_estimate),
            observation.units,
            unit_label,
        )
        .ok();
    }
    writeln!(
        out,
        "{accent}│{accent:#} {:.0}% CI: {}–{} {}/s",
        observation.time.confidence_interval.confidence_level * 100.0,
        format_rate(low_rate),
        format_rate(high_rate),
        family.unit_plural,
    )
    .ok();
    write_wrapped_field(out, accent, "Boundary: ", family.boundary, None);
    write_wrapped_field(out, accent, "Outside:  ", family.outside, Some(dim));

    if family.interpretation == Interpretation::ManagedTaskLifecycle {
        let grade = lifecycle_grade(low_rate, high_rate);
        let grade_style = style(grade.color, true);
        writeln!(out, "{accent}│{accent:#}").ok();
        writeln!(
            out,
            "{accent}│{accent:#} {grade_style}◆ PROJECT HEURISTIC · {}{grade_style:#}",
            grade.label,
        )
        .ok();
        writeln!(out, "{accent}│{accent:#} Project band: {}", grade.reference).ok();
        writeln!(
            out,
            "{accent}│{accent:#} Reference uses complete managed-task lifecycles only."
        )
        .ok();
    } else if family.scope == Scope::Lifecycle {
        writeln!(out, "{accent}│{accent:#}").ok();
        writeln!(
            out,
            "{accent}│{accent:#} {dim}No project band: this lifecycle uses a different semantic unit.{dim:#}"
        )
        .ok();
    } else {
        writeln!(out, "{accent}│{accent:#}").ok();
        writeln!(
            out,
            "{accent}│{accent:#} {dim}No lifecycle grade: this is not completed-task throughput.{dim:#}"
        )
        .ok();
    }
    writeln!(out, "{accent}└{}{accent:#}", "─".repeat(REPORT_WIDTH - 1),).ok();
    writeln!(out).ok();
}

struct Grade {
    label: &'static str,
    reference: &'static str,
    color: AnsiColor,
}

fn lifecycle_grade(low: f64, high: f64) -> Grade {
    let band = |rate: f64| {
        if rate < 10_000.0 {
            0
        } else if rate < 50_000.0 {
            1
        } else if rate < 200_000.0 {
            2
        } else {
            3
        }
    };
    let low_band = band(low);
    let high_band = band(high);
    if low_band != high_band {
        return Grade {
            label: "BAND EDGE; CONFIDENCE INTERVAL CROSSES A REFERENCE",
            reference: "95% CI crosses 10 K/s, 50 K/s, or 200 K/s",
            color: AnsiColor::BrightYellow,
        };
    }
    match low_band {
        0 => Grade {
            label: "BELOW HIGH-THROUGHPUT RANGE",
            reference: "below 10 K complete lifecycles/s",
            color: AnsiColor::BrightYellow,
        },
        1 => Grade {
            label: "SUBSTANTIAL LIFECYCLE THROUGHPUT",
            reference: "10 K to below 50 K complete lifecycles/s",
            color: AnsiColor::BrightCyan,
        },
        2 => Grade {
            label: "HIGH-THROUGHPUT RANGE",
            reference: "50 K to below 200 K complete lifecycles/s",
            color: AnsiColor::BrightGreen,
        },
        _ => Grade {
            label: "VERY-HIGH-THROUGHPUT RANGE",
            reference: "200 K or more complete lifecycles/s",
            color: AnsiColor::BrightGreen,
        },
    }
}

fn rate(units: u64, time_ns: f64) -> f64 {
    units as f64 * 1_000_000_000.0 / time_ns
}

fn format_rate(value: f64) -> String {
    if value >= 1_000_000_000.0 {
        format!("{:.3} G", value / 1_000_000_000.0)
    } else if value >= 1_000_000.0 {
        format!("{:.3} M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.3} K", value / 1_000.0)
    } else {
        format!("{value:.3}")
    }
}

fn format_count(value: f64) -> String {
    let rounded = value.round() as u64;
    let digits = rounded.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted
}

fn display_runtime(value: &str) -> String {
    match value {
        "current_thread" => "Tokio current-thread".to_owned(),
        "multi_thread" => "Tokio multi-thread · 4 workers".to_owned(),
        other => humanize(other),
    }
}

fn humanize(value: &str) -> String {
    value.replace('_', " ")
}

fn format_duration(ns: f64) -> String {
    if ns >= 1_000_000_000.0 {
        format!("{:.3} s", ns / 1_000_000_000.0)
    } else if ns >= 1_000_000.0 {
        format!("{:.3} ms", ns / 1_000_000.0)
    } else if ns >= 1_000.0 {
        format!("{:.3} µs", ns / 1_000.0)
    } else {
        format!("{ns:.3} ns")
    }
}

fn pluralize_for_count<'a>(singular: &'a str, plural: &'a str, count: u64) -> &'a str {
    if count == 1 { singular } else { plural }
}

fn write_wrapped_field(
    out: &mut AutoStream<std::io::Stdout>,
    accent: Style,
    label: &str,
    value: &str,
    value_style: Option<Style>,
) {
    let available = REPORT_WIDTH
        .saturating_sub(2 + label.chars().count())
        .max(20);
    let lines = wrap_words(value, available);
    for (index, line) in lines.iter().enumerate() {
        let prefix = if index == 0 {
            format!("{accent}│{accent:#} {label}")
        } else {
            format!("{accent}│{accent:#} {}", " ".repeat(label.chars().count()))
        };
        if let Some(style) = value_style {
            writeln!(out, "{prefix}{style}{line}{style:#}").ok();
        } else {
            writeln!(out, "{prefix}{line}").ok();
        }
    }
}

fn wrap_words(value: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in value.split_whitespace() {
        let separator = usize::from(!line.is_empty());
        if !line.is_empty() && line.chars().count() + separator + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

fn style(color: AnsiColor, bold: bool) -> Style {
    let style = Style::new().fg_color(Some(color.into()));
    if bold { style.bold() } else { style }
}

fn output() -> AutoStream<std::io::Stdout> {
    AutoStream::new(std::io::stdout(), color_choice())
}

fn color_choice() -> ColorChoice {
    let args: Vec<String> = std::env::args().collect();
    for (index, arg) in args.iter().enumerate() {
        let value = arg
            .strip_prefix("--color=")
            .or_else(|| arg.strip_prefix("--colour="))
            .or_else(|| {
                arg.strip_prefix("-c")
                    .map(|value| value.strip_prefix('=').unwrap_or(value))
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| {
                if matches!(arg.as_str(), "--color" | "--colour" | "-c") {
                    args.get(index + 1).map(String::as_str)
                } else {
                    None
                }
            });
        match value {
            Some("always") => return ColorChoice::Always,
            Some("never") => return ColorChoice::Never,
            _ => {}
        }
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return ColorChoice::Never;
    }
    ColorChoice::Auto
}

fn statistical_run_requested() -> bool {
    let args: Vec<String> = std::env::args().collect();
    let has = |flag: &str| {
        args.iter()
            .any(|arg| arg == flag || arg.starts_with(&format!("{flag}=")))
    };
    let bench = has("--bench");
    let test = has("--test");
    let criterion_mode = bench && !test;
    criterion_mode
        && !has("--list")
        && !has("--profile-time")
        && !has("--load-baseline")
        && !args
            .windows(2)
            .any(|pair| pair == ["--output-format", "bencher"])
        && !args.iter().any(|arg| arg == "--output-format=bencher")
        && std::env::var_os("CARGO_CRITERION_PORT").is_none()
}

fn discard_baseline_requested() -> bool {
    std::env::args().any(|arg| arg == "--discard-baseline")
}

fn noplot_requested() -> bool {
    std::env::args().any(|arg| matches!(arg.as_str(), "--noplot" | "-n"))
}

fn criterion_root() -> PathBuf {
    if let Some(path) = std::env::var_os("CRITERION_HOME") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("CARGO_TARGET_DIR") {
        return PathBuf::from(path).join("criterion");
    }
    cargo_target_directory()
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
        .join("criterion")
}

fn report_path_for_display(root: &Path) -> PathBuf {
    let report = root.join("report/index.html");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));

    if manifest == Path::new("/workspace")
        && let Ok(host_relative) = report.strip_prefix("/tmp")
    {
        return host_relative.to_path_buf();
    }

    report
        .strip_prefix(manifest)
        .map(Path::to_path_buf)
        .unwrap_or(report)
}

fn cargo_target_directory() -> Option<PathBuf> {
    #[derive(Deserialize)]
    struct Metadata {
        target_directory: PathBuf,
    }

    let cargo = std::env::var_os("CARGO")?;
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    serde_json::from_slice::<Metadata>(&output.stdout)
        .ok()
        .map(|metadata| metadata.target_directory)
}

fn cpu_model() -> Option<String> {
    if let Ok(value) = std::env::var("TASKVISOR_BENCH_CPU")
        && !value.trim().is_empty()
    {
        return Some(value.trim().to_owned());
    }
    if std::env::consts::OS == "macos" {
        for key in ["machdep.cpu.brand_string", "hw.model"] {
            let output = Command::new("sysctl").args(["-n", key]).output().ok()?;
            if output.status.success() {
                let value = String::from_utf8(output.stdout).ok()?;
                if !value.trim().is_empty() {
                    return Some(value.trim().to_owned());
                }
            }
        }
    }
    if std::env::consts::OS == "linux" {
        let cpuinfo = fs::read_to_string("/proc/cpuinfo").ok()?;
        for line in cpuinfo.lines() {
            if let Some((key, value)) = line.split_once(':')
                && matches!(key.trim(), "model name" | "Hardware")
                && !value.trim().is_empty()
            {
                return Some(value.trim().to_owned());
            }
        }
    }
    std::env::var("PROCESSOR_IDENTIFIER").ok()
}

fn git_revision() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let revision = String::from_utf8(output.stdout).ok()?;
    let revision = revision.trim();
    if revision.is_empty() {
        return None;
    }
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .is_some_and(|status| status.status.success() && !status.stdout.is_empty());
    Some(format!("{revision}{}", if dirty { "-dirty" } else { "" }))
}

fn enabled_features() -> String {
    let mut features = Vec::new();
    if cfg!(feature = "controller") {
        features.push("controller");
    }
    if cfg!(feature = "logging") {
        features.push("logging");
    }
    if cfg!(feature = "tracing") {
        features.push("tracing");
    }
    if cfg!(feature = "test-util") {
        features.push("test-util");
    }
    if cfg!(feature = "tokio-util-interop") {
        features.push("tokio-util-interop");
    }
    if features.is_empty() {
        "none".to_owned()
    } else {
        features.join(", ")
    }
}
