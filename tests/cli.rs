//! End-to-end tests: run the compiled binary against fixture files and check
//! its stdout, stderr and exit status. The unit tests in `src/` exercise the
//! library directly; these exist to catch argument parsing and formatting
//! bugs that only show up once the pieces are wired together in `main.rs`.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use jsonl_peek::json::parse;
use jsonl_peek::Value;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_jsonl-peek"))
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn run(args: &[&str]) -> Output {
    bin().args(args).output().expect("failed to run binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout must be UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr must be UTF-8")
}

#[test]
fn head_prints_the_first_n_lines_in_order() {
    let path = fixture("sample.jsonl");
    let output = run(&["head", "-n", "2", path.to_str().unwrap()]);
    assert!(output.status.success());
    let lines: Vec<&str> = stdout(&output).lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("\"id\":1"));
    assert!(lines[1].contains("\"id\":2"));
}

#[test]
fn head_stops_at_end_of_file_when_default_count_exceeds_it() {
    let path = fixture("sample.jsonl");
    // The fixture has 6 lines total, fewer than the default of 10, so head
    // should stop at end of file rather than error.
    let output = run(&["head", path.to_str().unwrap()]);
    assert!(output.status.success());
    assert_eq!(stdout(&output).lines().count(), 6);
}

#[test]
fn sample_with_a_seed_is_deterministic_and_a_subset() {
    let path = fixture("sample.jsonl");
    let args = ["sample", "-n", "3", "--seed", "42", path.to_str().unwrap()];
    let first = run(&args);
    let second = run(&args);
    assert!(first.status.success());
    assert_eq!(stdout(&first), stdout(&second));

    let source = std::fs::read_to_string(&path).unwrap();
    let source_lines: Vec<&str> = source.lines().filter(|l| !l.trim().is_empty()).collect();
    let sampled: Vec<&str> = stdout(&first).lines().collect();
    assert_eq!(sampled.len(), 3);
    for line in &sampled {
        assert!(source_lines.contains(line), "unexpected line in sample: {}", line);
    }
}

#[test]
fn sample_keeps_the_original_file_order() {
    let path = fixture("sample.jsonl");
    // Sampling every non-blank line must reproduce the file's order exactly.
    let output = run(&["sample", "-n", "5", "--seed", "1", path.to_str().unwrap()]);
    assert!(output.status.success());
    let sampled: Vec<&str> = stdout(&output).lines().collect();
    let source = std::fs::read_to_string(&path).unwrap();
    let source_lines: Vec<&str> = source.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(sampled, source_lines);
}

#[test]
fn stats_text_report_counts_lines_correctly() {
    let path = fixture("sample.jsonl");
    let output = run(&["stats", path.to_str().unwrap()]);
    assert!(output.status.success());
    let report = stdout(&output);
    assert!(report.contains("lines"));
    assert!(report.contains("blank 1"));
    assert!(report.contains("invalid 1"));
    assert!(report.contains("valid 4"));
    assert!(report.contains("invalid lines (1 total, showing 1)"));
}

#[test]
fn stats_json_report_matches_the_fixture() {
    let path = fixture("sample.jsonl");
    let output = run(&["stats", "--json", path.to_str().unwrap()]);
    assert!(output.status.success());
    let report = parse(&stdout(&output)).expect("stats --json must emit valid JSON");
    assert_eq!(report.get("lines").and_then(Value::as_i64), Some(6));
    assert_eq!(report.get("blank").and_then(Value::as_i64), Some(1));
    assert_eq!(report.get("invalid").and_then(Value::as_i64), Some(1));
    assert_eq!(report.get("valid").and_then(Value::as_i64), Some(4));
    assert_eq!(report.get("objects").and_then(Value::as_i64), Some(3));
}

#[test]
fn stats_field_option_profiles_a_fan_out_path() {
    let path = fixture("sample.jsonl");
    let output = run(&["stats", "--field", "tags[]", path.to_str().unwrap()]);
    assert!(output.status.success());
    let report = stdout(&output);
    assert!(report.contains("field tags[]"));
    assert!(report.contains("3 distinct values"));
}

#[test]
fn schema_text_report_lists_paths_and_skipped_lines() {
    let path = fixture("sample.jsonl");
    let output = run(&["schema", path.to_str().unwrap()]);
    assert!(output.status.success());
    let report = stdout(&output);
    assert!(report.starts_with("4 records, depth 3"));
    assert!(report.contains("tags[]"));
    assert!(report.contains("1 unparseable lines skipped"));
}

#[test]
fn schema_json_report_matches_the_fixture() {
    let path = fixture("sample.jsonl");
    let output = run(&["schema", "--json", path.to_str().unwrap()]);
    assert!(output.status.success());
    let report = parse(&stdout(&output)).expect("schema --json must emit valid JSON");
    assert_eq!(report.get("records").and_then(Value::as_i64), Some(4));
    assert!(report.get("paths").and_then(|p| p.get("id")).is_some());
}

#[test]
fn missing_file_is_a_runtime_error() {
    let output = run(&["stats", "/no/such/file.jsonl"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("/no/such/file.jsonl"));
}

#[test]
fn unknown_command_is_a_usage_error() {
    let output = run(&["frobnicate"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("unknown command"));
}

#[test]
fn missing_option_value_is_a_usage_error() {
    let output = run(&["head", "-n"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("-n requires a value"));
}

#[test]
fn reads_from_stdin_when_no_file_is_given() {
    let mut child = bin()
        .arg("head")
        .arg("-n")
        .arg("1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn binary");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"from\":\"stdin\"}\n{\"from\":\"ignored\"}\n")
        .unwrap();
    let output = child.wait_with_output().expect("failed to wait on child");
    assert!(output.status.success());
    assert_eq!(stdout(&output), "{\"from\":\"stdin\"}\n");
}
