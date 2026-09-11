use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::{extract_json_object, isolated_metrics_db_path};
use chrono::NaiveDate;
use serde_json::Value;
use std::time::{Duration, Instant};

fn seed_ai_commit(repo: &TestRepo) {
    let mut file = repo.filename("app.rs");
    file.set_contents(crate::lines!["fn main() {}", "let answer = 42;".ai()]);
    repo.stage_all_and_commit("AI commit")
        .expect("AI commit should succeed");
    file.assert_committed_lines(crate::lines![
        "fn main() {}".human(),
        "let answer = 42;".ai()
    ]);
}

/// Run `git-ai usage` with the metrics DB pointed at the daemon's isolated path,
/// retrying until the just-committed activity is persisted or the deadline passes.
fn usage_json(repo: &TestRepo, metrics_db_path: &str, extra_args: &[&str]) -> Value {
    let mut args = vec!["usage"];
    args.extend_from_slice(extra_args);
    args.push("--json");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        repo.sync_daemon_force();
        let result =
            repo.git_ai_with_env(&args, &[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path)]);
        if let Ok(output) = &result {
            let json = extract_json_object(output);
            if let Ok(value) = serde_json::from_str::<Value>(&json) {
                return value;
            }
        }

        if Instant::now() >= deadline {
            panic!("usage {args:?} did not return activity data: {result:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The window spans `calendar_start` to `calendar_end` as local dates. A fixed
/// `days * 86400` second subtraction lands on the same wall-clock time N days back,
/// so the span is N days except within an hour of midnight on a DST-transition day,
/// where it can shift by one. Callers assert a `[N - 1, N]` range to tolerate that.
fn window_span_days(value: &Value) -> i64 {
    let parse = |key: &str| {
        let text = value[key].as_str().expect("date field should be a string");
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("date field should parse")
    };
    (parse("calendar_end") - parse("calendar_start")).num_days()
}

fn assert_window(value: &Value, expected_label: &str, expected_days: i64) {
    assert_eq!(value["period_label"].as_str().unwrap(), expected_label);
    let span = window_span_days(value);
    assert!(
        (expected_days - 1..=expected_days).contains(&span),
        "expected a {expected_days}-day window (tolerating one DST day), got span {span}"
    );
}

#[test]
fn usage_period_valid_tokens_report_their_label_and_window() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    seed_ai_commit(&repo);
    let cases = [
        ("1d", "last 24 hours", 1),
        ("3d", "last 3 days", 3),
        ("7d", "last 7 days", 7),
        ("30d", "last 30 days", 30),
    ];

    for (token, expected_label, expected_days) in cases {
        let value = usage_json(&repo, &metrics_db_path, &["--period", token]);
        assert_window(&value, expected_label, expected_days);
    }
}

#[test]
fn usage_period_accepts_the_equals_form() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    seed_ai_commit(&repo);

    let value = usage_json(&repo, &metrics_db_path, &["--period=3d"]);

    assert_window(&value, "last 3 days", 3);
}

#[test]
fn usage_without_period_defaults_to_thirty_day_window() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    seed_ai_commit(&repo);

    let value = usage_json(&repo, &metrics_db_path, &[]);

    assert_window(&value, "last 30 days", 30);
}

#[test]
fn usage_period_invalid_token_exits_nonzero_with_message() {
    let repo = TestRepo::new();

    let result = repo.git_ai(&["usage", "--period", "90d"]);

    let err = result.expect_err("an invalid --period value should exit non-zero");
    assert!(
        err.contains(
            "Invalid --period value: 90d. Expected one of 1d, 3d, 7d, 30d, <YYYY-MM-DD>, \
             or <YYYY-MM-DD>..<YYYY-MM-DD>."
        ),
        "unexpected error output: {err}"
    );
}

#[test]
fn usage_period_accepts_a_date_range_inclusive_of_both_days() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    seed_ai_commit(&repo);

    let today = chrono::Local::now().date_naive();
    let yesterday = today - chrono::Days::new(1);
    let range = format!("{yesterday}..{today}");
    let value = usage_json(&repo, &metrics_db_path, &["--period", &range]);

    assert_eq!(
        value["period_label"].as_str().unwrap(),
        range.replace("..", " to ")
    );
    assert_eq!(
        value["calendar_start"].as_str().unwrap(),
        yesterday.to_string()
    );
    assert_eq!(value["calendar_end"].as_str().unwrap(), today.to_string());
    let buckets = value["buckets"].as_array().unwrap();
    assert_eq!(
        buckets.len(),
        2,
        "the calendar and bucket fill must stop exactly at the range end"
    );
    assert_eq!(value["summary"]["total_days"], 2);
}

#[test]
fn usage_period_single_date_runs_through_today() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    seed_ai_commit(&repo);

    let today = chrono::Local::now().date_naive();
    let since = today - chrono::Days::new(2);
    let value = usage_json(&repo, &metrics_db_path, &["--period", &since.to_string()]);

    assert_eq!(
        value["period_label"].as_str().unwrap(),
        format!("since {since}")
    );
    assert_eq!(value["calendar_start"].as_str().unwrap(), since.to_string());
    assert_eq!(value["calendar_end"].as_str().unwrap(), today.to_string());
    assert_eq!(value["buckets"].as_array().unwrap().len(), 3);
    assert_eq!(value["summary"]["total_days"], 3);
}

#[test]
fn usage_period_rejects_malformed_dates_and_reversed_ranges() {
    let repo = TestRepo::new();

    for (token, expected_fragment) in [
        // Unpadded dates are not accepted.
        ("2026-9-1", "Invalid --period value: 2026-9-1."),
        // Garbage in either slot of a range.
        (
            "2026-09-01..garbage",
            "Invalid --period value: 2026-09-01..garbage.",
        ),
        // The end date must not precede the start date.
        (
            "2026-09-11..2026-09-01",
            "Invalid --period value: 2026-09-11..2026-09-01. The end date must not precede the start date.",
        ),
    ] {
        let result = repo.git_ai(&["usage", "--period", token]);
        let err = result.expect_err("a malformed --period value should exit non-zero");
        assert!(
            err.contains(expected_fragment),
            "expected {expected_fragment:?} for {token:?}, got:\n{err}"
        );
    }
}

#[test]
fn usage_period_excludes_activity_outside_the_range() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    seed_ai_commit(&repo);

    // Today's commit happened after a range that ended two days ago — the
    // window must come back empty rather than containing it.
    let today = chrono::Local::now().date_naive();
    let start = today - chrono::Days::new(3);
    let end = today - chrono::Days::new(2);
    let range = format!("{start}..{end}");

    let result = repo.git_ai_with_env(
        &["usage", "--period", &range, "--json"],
        &[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())],
    );
    let err = result.expect_err("an inactive range should report no data");
    assert!(
        err.contains("No activity data found"),
        "unexpected error output: {err}"
    );
}

#[test]
fn usage_period_missing_value_exits_nonzero_with_message() {
    let repo = TestRepo::new();

    let result = repo.git_ai(&["usage", "--period"]);

    let err = result.expect_err("a missing --period value should exit non-zero");
    assert!(
        err.contains("Missing value for --period."),
        "unexpected error output: {err}"
    );
}
