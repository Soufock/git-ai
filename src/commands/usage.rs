//! `git-ai usage` — local statistics from persisted metric events.

use crate::git::repository::find_repository_in_path;
use crate::metrics::local_stats::{
    BucketGranularity, LocalActivityStats, RepoActivitySummary, compute_all,
};
use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};
use serde::Serialize;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize)]
struct UsageJsonOutput<'a> {
    #[serde(flatten)]
    stats: &'a LocalActivityStats,
    repos: &'a [RepoActivitySummary],
}

const DEFAULT_PERIOD: &str = "30d";

/// A parsed `--period` value: either a rolling window or explicit local dates.
#[derive(Debug, PartialEq, Eq)]
enum PeriodSpec {
    /// `1d`/`3d`/`7d`/`30d` — rolling window ending now.
    RelativeDays(u64),
    /// `<YYYY-MM-DD>` — that day's local midnight through now.
    Since(NaiveDate),
    /// `<YYYY-MM-DD>..<YYYY-MM-DD>` — both days included.
    Range(NaiveDate, NaiveDate),
}

const PERIOD_ERROR_HINT: &str =
    "Expected one of 1d, 3d, 7d, 30d, <YYYY-MM-DD>, or <YYYY-MM-DD>..<YYYY-MM-DD>.";

/// Parse a `--period` token: a relative window or an explicit date/date range.
fn resolve_period(token: &str) -> Result<PeriodSpec, String> {
    match token {
        "1d" => return Ok(PeriodSpec::RelativeDays(1)),
        "3d" => return Ok(PeriodSpec::RelativeDays(3)),
        "7d" => return Ok(PeriodSpec::RelativeDays(7)),
        "30d" => return Ok(PeriodSpec::RelativeDays(30)),
        _ => {}
    }

    let invalid = || format!("Invalid --period value: {token}. {PERIOD_ERROR_HINT}");
    let parse_date = |value: &str| {
        // Enforce the strict `YYYY-MM-DD` shape first: chrono's parser is
        // lenient about padding and would otherwise accept "2026-9-1".
        let padded = value.len() == 10
            && value.as_bytes()[4] == b'-'
            && value.as_bytes()[7] == b'-'
            && value[0..4].bytes().all(|b| b.is_ascii_digit())
            && value[5..7].bytes().all(|b| b.is_ascii_digit())
            && value[8..10].bytes().all(|b| b.is_ascii_digit());
        if !padded {
            return Err(invalid());
        }
        NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| invalid())
    };

    if let Some((start, end)) = token.split_once("..") {
        let start = parse_date(start)?;
        let end = parse_date(end)?;
        if end < start {
            return Err(format!(
                "Invalid --period value: {token}. The end date must not precede the start date."
            ));
        }
        return Ok(PeriodSpec::Range(start, end));
    }

    Ok(PeriodSpec::Since(parse_date(token)?))
}

/// Convert a resolved period into its `(since_ts, end_ts, label, granularity)`
/// window. Date-based periods always bucket by day; relative windows keep their
/// existing granularity (daily up to a week, weekly for the 30-day window).
fn resolve_window(spec: PeriodSpec) -> (u32, u32, String, BucketGranularity) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(u32::MAX as u64) as u32;

    match spec {
        PeriodSpec::RelativeDays(days) => {
            let label = match days {
                1 => "last 24 hours",
                3 => "last 3 days",
                7 => "last 7 days",
                _ => "last 30 days",
            };
            let granularity = if days <= 7 {
                BucketGranularity::Daily
            } else {
                BucketGranularity::Weekly
            };
            // Derive both bounds from the same clock read so the window is
            // exactly `days` long rather than a few microseconds longer.
            (
                now.saturating_sub((days * 24 * 3600).min(u32::MAX as u64) as u32),
                now,
                label.to_string(),
                granularity,
            )
        }
        PeriodSpec::Since(date) => (
            date_to_ts(date, 0, 0, 0),
            now,
            format!("since {date}"),
            BucketGranularity::Daily,
        ),
        PeriodSpec::Range(start, end) => (
            date_to_ts(start, 0, 0, 0),
            date_to_ts(end, 23, 59, 59),
            format!("{start} to {end}"),
            BucketGranularity::Daily,
        ),
    }
}

pub fn handle_usage(args: &[String]) {
    let mut json = false;
    let mut period = DEFAULT_PERIOD.to_string();
    let mut repo_filter: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--json" => json = true,
            "--help" | "-h" => {
                print_help();
                return;
            }
            "--period" => {
                i += 1;
                match args.get(i) {
                    Some(value) => period = value.clone(),
                    None => {
                        eprintln!("Missing value for --period.");
                        eprintln!("Run 'git-ai usage --help' for usage.");
                        std::process::exit(1);
                    }
                }
            }
            "--repo" => {
                i += 1;
                match args.get(i) {
                    Some(value) => match resolve_repo_filter(value) {
                        Ok(resolved) => repo_filter = Some(resolved),
                        Err(err) => {
                            eprintln!("{}", err);
                            eprintln!("Run 'git-ai usage --help' for usage.");
                            std::process::exit(1);
                        }
                    },
                    None => {
                        eprintln!("Missing value for --repo.");
                        eprintln!("Run 'git-ai usage --help' for usage.");
                        std::process::exit(1);
                    }
                }
            }
            _ if arg.starts_with("--period=") => {
                period = arg["--period=".len()..].to_string();
            }
            _ if arg.starts_with("--repo=") => match resolve_repo_filter(&arg["--repo=".len()..]) {
                Ok(resolved) => repo_filter = Some(resolved),
                Err(err) => {
                    eprintln!("{}", err);
                    eprintln!("Run 'git-ai usage --help' for usage.");
                    std::process::exit(1);
                }
            },
            other => {
                eprintln!("Unknown argument: {}", other);
                eprintln!("Run 'git-ai usage --help' for usage.");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let (since_ts, end_ts, period_label, granularity) = match resolve_period(&period) {
        Ok(spec) => resolve_window(spec),
        Err(e) => {
            eprintln!("{}", e);
            eprintln!("Run 'git-ai usage --help' for usage.");
            std::process::exit(1);
        }
    };

    // Best-effort refresh of the models.dev pricing cache (at most daily)
    // before stats — and therefore cost estimates — are computed.
    crate::metrics::model_pricing::refresh_cache_if_stale();

    // Fetch events once and derive both views from the same snapshot so the
    // per-repo breakdown totals are always consistent with the headline stats.
    let repo_filter_ref = repo_filter.as_deref();
    let (stats, repos) =
        match compute_all(since_ts, end_ts, period_label, granularity, repo_filter_ref) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        };

    // Include human_lines/diff_added_lines so human-only periods aren't
    // falsely reported as empty (commits.total only counts AI-involved commits).
    // Also include checkpoint lines so checkpoint-only activity isn't missed.
    let no_data = stats.commits.total == 0
        && stats.commits.human_lines == 0
        && stats.commits.diff_added_lines == 0
        && stats.sessions.total == 0
        && stats.checkpoints.ai_lines_added == 0
        && stats.checkpoints.human_lines_added == 0
        && stats.tokens.input
            + stats.tokens.output
            + stats.tokens.cache_read
            + stats.tokens.cache_creation
            == 0;
    if no_data {
        eprintln!(
            "No activity data found for the {} window.",
            stats.period_label
        );
        std::process::exit(1);
    }

    if json {
        let output = UsageJsonOutput {
            stats: &stats,
            repos: &repos,
        };
        match serde_json::to_string_pretty(&output) {
            Ok(s) => println!("{}", s),
            Err(e) => {
                eprintln!("error serializing JSON: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        print_terminal(&stats, &repos);
    }
}

fn resolve_repo_filter(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("Invalid --repo value: empty string.".to_string());
    }
    if trimmed.contains("://") || trimmed.starts_with("git@") || trimmed.starts_with("ssh://") {
        return crate::repo_url::normalize_repo_url(trimmed);
    }
    let repo = find_repository_in_path(trimmed)
        .map_err(|_| format!("No git repository found at path '{}'.", value))?;
    crate::repo_url::resolve_repo_url_from_repo(&repo)
        .ok_or_else(|| format!("Failed to resolve repository URL from path '{}'.", value))
}

/// Unix seconds for a local wall-clock time on `date`. Local times can be
/// ambiguous or skipped by DST transitions, so prefer the earliest
/// interpretation when the wall time does not resolve uniquely.
fn date_to_ts(date: NaiveDate, hour: u32, min: u32, sec: u32) -> u32 {
    let ndt = date
        .and_hms_opt(hour, min, sec)
        .expect("requested wall time is a valid 24h clock time");
    Local
        .from_local_datetime(&ndt)
        .single()
        .or_else(|| Local.from_local_datetime(&ndt).earliest())
        .expect("local wall time should resolve to a timestamp")
        .timestamp() as u32
}

fn print_help() {
    eprintln!("git-ai usage - Show local activity statistics");
    eprintln!();
    eprintln!("Usage: git-ai usage [options]");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --period <1d|3d|7d|30d|DATE|DATE..DATE>  Time window (default: 30d)");
    eprintln!("                                     DATE is <YYYY-MM-DD>: a single date runs");
    eprintln!("                                     from that day to now; a range is inclusive");
    eprintln!("                                     of both days, e.g. 2026-09-01..2026-09-11");
    eprintln!("  --repo <url|path>                 Filter to one repository");
    eprintln!("  --json                            Output as JSON");
    eprintln!("  --help                            Show this help");
    eprintln!();
    eprintln!("Shows activity over the selected window from locally recorded metric events.");
    eprintln!("Metric rows older than approximately 365 days are pruned locally.");
}

fn print_terminal(stats: &LocalActivityStats, repos: &[RepoActivitySummary]) {
    const GRAY: &str = "\x1b[90m";
    const BOLD: &str = "\x1b[1m";
    const RESET: &str = "\x1b[0m";
    const ORANGE: &str = "\x1b[38;5;208m";

    // Capitalize the first letter for the header (period_label is lower-case
    // elsewhere where it reads mid-sentence, e.g. "Activity — last 30 days").
    let header = {
        let mut c = stats.period_label.chars();
        match c.next() {
            Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
            None => String::new(),
        }
    };
    println!();
    println!("  {BOLD}{header}{RESET}");

    // --- Top bar: AI vs Human split ---
    println!();
    let total_lines = stats.commits.ai_lines + stats.commits.human_lines;
    if let Some(ai_pct) = (stats.commits.ai_lines as u64 * 100)
        .checked_div(total_lines as u64)
        .map(|p| p as u32)
    {
        let human_pct = 100 - ai_pct;
        let filled = (ai_pct * 40 / 100).min(40) as usize;
        let bar = format!(
            "{ORANGE}{}{GRAY}{}{RESET}",
            "█".repeat(filled),
            "░".repeat(40 - filled),
        );
        println!(
            "  {}  {BOLD}AI{RESET} {:>3}% · {BOLD}Human{RESET} {:>3}%",
            bar, ai_pct, human_pct,
        );
    }

    // --- Per-repo breakdown ---
    // Only shown when there are multiple repos — a single-row table adds nothing.
    if repos.len() > 1 {
        println!();
        println!("  {BOLD}Repositories{RESET}");

        // Pre-compute display strings for column alignment.
        let names: Vec<&str> = repos
            .iter()
            .map(|r| {
                let d = strip_protocol(&r.repo_url);
                if d.is_empty() { "unknown" } else { d }
            })
            .collect();
        let lines_strs: Vec<String> = repos
            .iter()
            .map(|r| format!("+{}", format_num(r.ai_lines)))
            .collect();
        let session_strs: Vec<String> = repos.iter().map(|r| format_num(r.sessions)).collect();

        let max_name_w = names.iter().map(|n| n.len()).max().unwrap_or(0);
        let max_lines_w = lines_strs.iter().map(|s| s.len()).max().unwrap_or(0);
        let max_sessions_w = session_strs.iter().map(|s| s.len()).max().unwrap_or(0);

        for (i, r) in repos.iter().enumerate() {
            let name_col = format!("{:<width$}", names[i], width = max_name_w);
            let lines_col = format!("{:>width$}", lines_strs[i], width = max_lines_w);
            let sessions_col = format!("{:>width$}", session_strs[i], width = max_sessions_w);
            // Pad singular labels to match the width of the plural so columns stay aligned.
            let session_label = if r.sessions == 1 {
                "session "
            } else {
                "sessions"
            };
            let cost_str = if r.estimated_cost_usd > 0.0 {
                format!("  {GRAY}{}{RESET}", format_cost(r.estimated_cost_usd))
            } else {
                String::new()
            };
            println!(
                "    {GRAY}{}  {} lines  {} {}{}{RESET}",
                name_col, lines_col, sessions_col, session_label, cost_str,
            );
        }
    }

    // --- Activity strip (generated AI lines per day over the window) ---
    print_calendar(stats);

    // --- Compact stats block ---
    // Built as plain left/right cells and padded by visible width — ANSI codes
    // would corrupt fixed-width alignment, so this block is intentionally uncolored.
    let s = &stats.summary;
    let t = &stats.tokens;
    let total_tokens = t.input + t.output + t.cache_read + t.cache_creation;
    const COL_W: usize = 34;

    let row = |left: String, right: String| {
        println!("  {:<width$}{}", left, right, width = COL_W);
    };

    println!();
    if let Some(model) = s.favorite_model.as_deref() {
        row(
            format!("Favorite model: {model}"),
            format!("Total tokens: {}", format_count_short(total_tokens)),
        );
    } else {
        row(
            String::new(),
            format!("Total tokens: {}", format_count_short(total_tokens)),
        );
    }
    println!();
    row(
        format!(
            "Sessions: {}",
            format_count_short(stats.sessions.total as u64)
        ),
        format!(
            "Longest session: {}",
            format_duration(s.longest_session_secs)
        ),
    );
    row(
        format!("Token spend: {}", format_cost(t.estimated_cost_usd)),
        format!("Longest streak: {} days", s.longest_streak),
    );
    // Estimated token spend per committed AI line.
    let dollars_per_line = if stats.commits.ai_lines > 0 {
        format_cost(t.estimated_cost_usd / stats.commits.ai_lines as f64)
    } else {
        "—".to_string()
    };
    row(
        format!("{dollars_per_line} / committed line"),
        format!("Current streak: {} days", s.current_streak),
    );

    println!();
}

/// Render the activity heatmap as two horizontal strips of day-cells spanning the
/// window — generated lines/day (orange) and token spend/day (blue) — sharing a
/// date-tick header, each quartile-shaded over its own distribution. The spend row
/// is omitted when there's no token data, falling back to the original single strip.
/// For windows wider than the terminal, consecutive days are bucketed (max) into
/// columns so the strip always fits and fills the width.
fn print_calendar(stats: &LocalActivityStats) {
    const GRAY: &str = "\x1b[90m";
    const BOLD: &str = "\x1b[1m";
    const RESET: &str = "\x1b[0m";
    const ORANGE: &str = "\x1b[38;5;208m";
    // Distinct from orange so lines and spend don't blend together.
    const BLUE: &str = "\x1b[38;5;33m";
    // Quartile levels 1–4 (level 0 = empty `·`, rendered gray).
    const LEVEL_CHARS: [&str; 5] = ["·", "░", "▒", "▓", "█"];
    // Usable strip width after the 2-space indent (assumes ~80-col terminal).
    const MAX_W: usize = 74;
    // Left gutter for the "lines"/"spend" row labels (label + 2 spaces).
    const LABEL_W: usize = 7;

    if stats.calendar.is_empty() {
        return;
    }

    let line_days: HashMap<NaiveDate, u32> = stats
        .calendar
        .iter()
        .map(|d| (d.date, d.ai_lines))
        .collect();
    let spend_days: HashMap<NaiveDate, f64> = stats
        .calendar
        .iter()
        .map(|d| (d.date, d.estimated_cost_usd))
        .collect();
    let start = stats.calendar_start;
    let end = stats.calendar_end;
    let span = ((end - start).num_days() + 1).max(1) as usize;

    // Per-day series over the window (0 for inactive days).
    let per_day_lines: Vec<f64> = (0..span)
        .map(|i| {
            line_days
                .get(&(start + Duration::days(i as i64)))
                .copied()
                .unwrap_or(0) as f64
        })
        .collect();
    let per_day_spend: Vec<f64> = (0..span)
        .map(|i| {
            spend_days
                .get(&(start + Duration::days(i as i64)))
                .copied()
                .unwrap_or(0.0)
        })
        .collect();

    // Only show the spend row when there's actual token spend in the window;
    // otherwise fall back to the original single, unlabeled strip.
    let has_spend = per_day_spend.iter().any(|&v| v > 0.0);
    let label_w = if has_spend { LABEL_W } else { 0 };
    let max_w = MAX_W - label_w;

    // Choose layout: one cell per day when it fits (spaced if there's room),
    // otherwise bucket consecutive days so the strip fills exactly max_w columns.
    let (cols, spaced) = if span * 2 <= max_w {
        (span, true)
    } else if span <= max_w {
        (span, false)
    } else {
        (max_w, false)
    };

    // Bucket a per-day series into `cols` columns (max intensity per bucket).
    let bucket = |per_day: &[f64]| -> Vec<f64> {
        (0..cols)
            .map(|c| {
                let lo = c * span / cols;
                let hi = ((c + 1) * span / cols).max(lo + 1).min(span);
                per_day[lo..hi].iter().copied().fold(0.0, f64::max)
            })
            .collect()
    };
    let col_lines = bucket(&per_day_lines);
    let col_spend = bucket(&per_day_spend);
    let col_date: Vec<NaiveDate> = (0..cols)
        .map(|c| start + Duration::days((c * span / cols) as i64))
        .collect();

    let sep = if spaced { " " } else { "" };

    // Render one cell row: quartile-shade each column over the row's own non-zero
    // distribution (line counts and spend are both heavily right-skewed, so linear
    // scaling would collapse most cells to one level). Empty days render as gray dots.
    let render_row = |col_val: &[f64], color: &str| -> String {
        let mut nz: Vec<f64> = col_val.iter().copied().filter(|&v| v > 0.0).collect();
        nz.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let q = |p: f64| -> f64 {
            if nz.is_empty() {
                0.0
            } else {
                nz[(((nz.len() - 1) as f64) * p).round() as usize]
            }
        };
        let (t1, t2, t3) = (q(0.25), q(0.50), q(0.75));
        col_val
            .iter()
            .map(|&v| {
                let lvl = if v <= 0.0 {
                    0
                } else if v <= t1 {
                    1
                } else if v <= t2 {
                    2
                } else if v <= t3 {
                    3
                } else {
                    4
                };
                if lvl == 0 {
                    format!("{GRAY}{}{RESET}", LEVEL_CHARS[0])
                } else {
                    format!("{color}{}{RESET}", LEVEL_CHARS[lvl])
                }
            })
            .collect::<Vec<_>>()
            .join(sep)
    };

    let cell_w = if spaced { 2 } else { 1 };

    println!();
    if has_spend {
        println!(
            "  {BOLD}Activity{RESET} {GRAY}— {}{RESET}",
            stats.period_label
        );
    } else {
        println!(
            "  {BOLD}Activity{RESET} {GRAY}— generated lines/day · {}{RESET}",
            stats.period_label
        );
    }
    println!();

    // Date-tick row: ~6 evenly spaced labels ("Jun 11"), skipping any that would
    // overlap the previous one. Indented by the gutter so ticks align with cells.
    let n_ticks = 6.min(cols);
    let step = (cols / n_ticks.max(1)).max(1);
    let mut tick_row = vec![' '; cols * cell_w + 8];
    let mut last_end = 0usize;
    for c in (0..cols).step_by(step) {
        let pos = c * cell_w;
        if pos >= last_end {
            let label = format_day_label(col_date[c]);
            for (i, ch) in label.chars().enumerate() {
                if pos + i < tick_row.len() {
                    tick_row[pos + i] = ch;
                }
            }
            last_end = pos + label.chars().count() + 1;
        }
    }
    let tick_line: String = tick_row.iter().collect();
    let gutter = " ".repeat(label_w);
    println!("  {gutter}{GRAY}{}{RESET}", tick_line.trim_end());

    if has_spend {
        println!("  {GRAY}lines  {RESET}{}", render_row(&col_lines, ORANGE));
        println!("  {GRAY}spend  {RESET}{}", render_row(&col_spend, BLUE));
    } else {
        println!("  {}", render_row(&col_lines, ORANGE));
    }

    println!();
    if has_spend {
        // Two colors share one intensity scale — keep the gradient neutral.
        println!("  {GRAY}Less ░ ▒ ▓ █ More{RESET}");
    } else {
        println!("  {GRAY}Less{RESET} {ORANGE}░ ▒ ▓ █{RESET} {GRAY}More{RESET}");
    }
}

/// Abbreviate a count with k/m/b suffixes (e.g. 1_600 → "1.6k", 45_500_000 → "45.5m").
fn format_count_short(n: u64) -> String {
    let (val, suffix) = if n >= 1_000_000_000 {
        (n as f64 / 1e9, "b")
    } else if n >= 1_000_000 {
        (n as f64 / 1e6, "m")
    } else if n >= 1_000 {
        (n as f64 / 1e3, "k")
    } else {
        return n.to_string();
    };
    let s = format!("{val:.1}");
    let s = s.strip_suffix(".0").map(str::to_string).unwrap_or(s);
    format!("{s}{suffix}")
}

/// Format a duration in seconds as "3d 6h 4m" (non-zero units only).
fn format_duration(secs: u32) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    let mut parts = Vec::new();
    if d > 0 {
        parts.push(format!("{d}d"));
    }
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if parts.is_empty() {
        return "0m".to_string();
    }
    parts.join(" ")
}

/// Format a date as "Apr 2" (portable — avoids non-portable `%-d`/`%e`).
fn format_day_label(date: NaiveDate) -> String {
    format!("{} {}", date.format("%b"), date.day())
}

/// Strip `https://` or `http://` from a URL for display purposes.
fn strip_protocol(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
}

/// Format a USD cost estimate. Rounds to whole dollars for amounts >= $10
/// (estimates don't warrant cent-level precision at that scale); shows cents otherwise.
fn format_cost(usd: f64) -> String {
    if usd >= 10.0 {
        format!("~${:.0}", usd)
    } else {
        format!("~${:.2}", usd)
    }
}

fn format_num(n: u32) -> String {
    format_num_u64(n as u64)
}

fn format_num_u64(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_period_defaults_to_thirty_day_weekly_window() {
        assert_eq!(
            resolve_period(DEFAULT_PERIOD).unwrap(),
            PeriodSpec::RelativeDays(30)
        );
    }

    #[test]
    fn resolve_period_maps_short_windows_to_relative_days() {
        let resolved: Vec<_> = ["1d", "3d", "7d"]
            .iter()
            .map(|t| resolve_period(t).unwrap())
            .collect();

        assert_eq!(
            resolved,
            vec![
                PeriodSpec::RelativeDays(1),
                PeriodSpec::RelativeDays(3),
                PeriodSpec::RelativeDays(7),
            ]
        );
    }

    #[test]
    fn resolve_period_rejects_unknown_token() {
        assert_eq!(
            resolve_period("90d"),
            Err(
                "Invalid --period value: 90d. Expected one of 1d, 3d, 7d, 30d, \
                 <YYYY-MM-DD>, or <YYYY-MM-DD>..<YYYY-MM-DD>."
                    .to_string()
            )
        );
    }

    #[test]
    fn resolve_period_accepts_single_date() {
        assert_eq!(
            resolve_period("2026-09-01").unwrap(),
            PeriodSpec::Since(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap())
        );
    }

    #[test]
    fn resolve_period_accepts_date_range() {
        assert_eq!(
            resolve_period("2026-09-01..2026-09-11").unwrap(),
            PeriodSpec::Range(
                NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(),
                NaiveDate::from_ymd_opt(2026, 9, 11).unwrap()
            )
        );
    }

    #[test]
    fn resolve_period_rejects_unpadded_or_garbage_dates() {
        for bad in ["2026-9-1", "foo", "2026-09-01..foo", "2026-09-01..2026-9-1"] {
            assert!(resolve_period(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn resolve_period_rejects_reversed_range() {
        assert!(resolve_period("2026-09-11..2026-09-01").is_err());
    }

    #[test]
    fn resolve_window_maps_relative_and_date_specs() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        // Relative windows stay anchored at "now", granularity by window length.
        let (since, end, label, granularity) = resolve_window(PeriodSpec::RelativeDays(7));
        assert!((end as i64 - now as i64).abs() <= 2);
        assert_eq!(now.saturating_sub(since), 7 * 24 * 3600);
        assert_eq!(label, "last 7 days");
        assert_eq!(granularity, BucketGranularity::Daily);

        // A single date starts at that day's local midnight and runs to now.
        let date = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let (since, end, label, granularity) = resolve_window(PeriodSpec::Since(date));
        assert_eq!(since, date_to_ts(date, 0, 0, 0));
        assert_eq!(label, "since 2026-09-01");
        assert_eq!(granularity, BucketGranularity::Daily);
        assert!((end as i64 - now as i64).abs() <= 2);

        // A range is inclusive at both ends (end-of-day on the last day).
        let start = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap();
        let end_date = NaiveDate::from_ymd_opt(2026, 9, 11).unwrap();
        let (since, end, label, granularity) = resolve_window(PeriodSpec::Range(start, end_date));
        assert_eq!(since, date_to_ts(start, 0, 0, 0));
        assert_eq!(end, date_to_ts(end_date, 23, 59, 59));
        assert_eq!(label, "2026-09-01 to 2026-09-11");
        assert_eq!(granularity, BucketGranularity::Daily);
    }

    #[test]
    fn resolve_repo_filter_accepts_url_and_path_forms() {
        assert_eq!(
            resolve_repo_filter("https://github.com/org/repo.git").unwrap(),
            "https://github.com/org/repo"
        );
    }
}
