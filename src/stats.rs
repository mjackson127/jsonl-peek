//! Single-pass health check for a JSONL file.
//!
//! [`Stats`] accumulates everything the `stats` command reports: how many lines
//! parsed, where the broken ones are, the distribution of line lengths, which
//! top level keys exist and with which types, and optionally the value
//! distribution of any number of [`FieldPath`]s.
//!
//! Memory use is bounded: line lengths go into a [`Histogram`], the key table
//! and the per-field value tables are capped, and only the first few parse
//! errors are kept verbatim (the rest are still counted).

use std::collections::BTreeMap;
use std::io::{self, BufRead};

use crate::hist::Histogram;
use crate::json::{escape_into, parse, Value};
use crate::lines::{Line, LineReader};
use crate::path::FieldPath;

/// Longest value rendering kept in a field distribution table.
const MAX_VALUE_LEN: usize = 96;

/// A line that could not be turned into a JSON value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIssue {
    /// 1-based line number.
    pub line: u64,
    /// 1-based byte column where the problem was detected.
    pub column: usize,
    /// Human readable reason.
    pub reason: String,
}

/// How often a top level key appears, and with which value types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyStat {
    /// Number of occurrences across all records.
    pub count: u64,
    /// Value type histogram for this key.
    pub types: BTreeMap<&'static str, u64>,
}

/// Value distribution for one [`FieldPath`].
#[derive(Debug, Clone)]
pub struct FieldStat {
    /// The path this table describes.
    pub path: FieldPath,
    /// Records in which the path selected at least one value.
    pub records_matched: u64,
    /// Records in which the path selected nothing.
    pub records_missing: u64,
    /// Total number of selected values (a `[]` path can match many per record).
    pub values: u64,
    /// Type histogram of the selected values.
    pub types: BTreeMap<&'static str, u64>,
    /// Distinct value counts, capped at `max_distinct`.
    pub distinct: BTreeMap<String, u64>,
    /// Values dropped because the distinct table was full.
    pub distinct_dropped: u64,
}

impl FieldStat {
    /// The `n` most frequent values, most frequent first.
    pub fn top(&self, n: usize) -> Vec<(&str, u64)> {
        let mut rows: Vec<(&str, u64)> = self
            .distinct
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        rows.truncate(n);
        rows
    }
}

/// Knobs for [`Stats`].
#[derive(Debug, Clone)]
pub struct StatsOptions {
    /// How many broken lines to keep verbatim.
    pub max_issues: usize,
    /// How many distinct top level keys to track.
    pub max_keys: usize,
    /// How many distinct values to track per field.
    pub max_distinct: usize,
    /// Field paths to profile.
    pub fields: Vec<FieldPath>,
}

impl Default for StatsOptions {
    fn default() -> Self {
        StatsOptions {
            max_issues: 10,
            max_keys: 512,
            max_distinct: 10_000,
            fields: Vec::new(),
        }
    }
}

/// Accumulated statistics for one file.
#[derive(Debug)]
pub struct Stats {
    options: StatsOptions,
    /// Lines seen.
    pub lines: u64,
    /// Bytes consumed, line endings included.
    pub bytes: u64,
    /// Lines that were empty or whitespace only.
    pub blank: u64,
    /// Lines that parsed as JSON.
    pub valid: u64,
    /// Lines that did not parse.
    pub invalid: u64,
    /// Valid lines whose top level value is an object.
    pub objects: u64,
    /// The first `max_issues` parse failures.
    pub issues: Vec<LineIssue>,
    /// Byte length of every non-blank line.
    pub lengths: Histogram,
    /// Top level value types.
    pub types: BTreeMap<&'static str, u64>,
    /// Top level object keys.
    pub keys: BTreeMap<String, KeyStat>,
    /// Key occurrences dropped because the key table was full.
    pub keys_dropped: u64,
    /// Per-field value distributions, in the order they were requested.
    pub fields: Vec<FieldStat>,
}

impl Stats {
    /// Creates an empty accumulator.
    pub fn new(options: StatsOptions) -> Self {
        let fields = options
            .fields
            .iter()
            .map(|path| FieldStat {
                path: path.clone(),
                records_matched: 0,
                records_missing: 0,
                values: 0,
                types: BTreeMap::new(),
                distinct: BTreeMap::new(),
                distinct_dropped: 0,
            })
            .collect();
        Stats {
            options,
            lines: 0,
            bytes: 0,
            blank: 0,
            valid: 0,
            invalid: 0,
            objects: 0,
            issues: Vec::new(),
            lengths: Histogram::new(),
            types: BTreeMap::new(),
            keys: BTreeMap::new(),
            keys_dropped: 0,
            fields,
        }
    }

    /// Reads a whole stream and returns the accumulated statistics.
    pub fn from_reader<R: BufRead>(reader: R, options: StatsOptions) -> io::Result<Stats> {
        let mut stats = Stats::new(options);
        let mut lines = LineReader::new(reader);
        while let Some(line) = lines.next_line()? {
            stats.observe(&line);
        }
        Ok(stats)
    }

    /// Folds one line into the accumulator.
    pub fn observe(&mut self, line: &Line<'_>) {
        self.lines += 1;
        self.bytes += line.raw_len as u64;
        if line.is_blank() {
            self.blank += 1;
            return;
        }
        self.lengths.record(line.bytes.len() as u64);

        let text = match std::str::from_utf8(line.bytes) {
            Ok(text) => text,
            Err(err) => {
                self.invalid += 1;
                self.push_issue(line.number, err.valid_up_to() + 1, "invalid UTF-8".to_string());
                return;
            }
        };
        match parse(text) {
            Ok(value) => {
                self.valid += 1;
                self.record(&value);
            }
            Err(err) => {
                self.invalid += 1;
                self.push_issue(line.number, err.offset + 1, err.kind.to_string());
            }
        }
    }

    fn push_issue(&mut self, line: u64, column: usize, reason: String) {
        if self.issues.len() < self.options.max_issues {
            self.issues.push(LineIssue {
                line,
                column,
                reason,
            });
        }
    }

    fn record(&mut self, value: &Value) {
        *self.types.entry(value.type_name()).or_insert(0) += 1;
        if let Some(members) = value.as_object() {
            self.objects += 1;
            for (name, member) in members {
                if let Some(stat) = self.keys.get_mut(name) {
                    stat.count += 1;
                    *stat.types.entry(member.type_name()).or_insert(0) += 1;
                } else if self.keys.len() < self.options.max_keys {
                    let mut stat = KeyStat {
                        count: 1,
                        types: BTreeMap::new(),
                    };
                    stat.types.insert(member.type_name(), 1);
                    self.keys.insert(name.clone(), stat);
                } else {
                    self.keys_dropped += 1;
                }
            }
        }
        let max_distinct = self.options.max_distinct;
        for stat in &mut self.fields {
            let selected = stat.path.select(value);
            if selected.is_empty() {
                stat.records_missing += 1;
                continue;
            }
            stat.records_matched += 1;
            for found in selected {
                stat.values += 1;
                *stat.types.entry(found.type_name()).or_insert(0) += 1;
                let rendered = render_value(found);
                if let Some(count) = stat.distinct.get_mut(&rendered) {
                    *count += 1;
                } else if stat.distinct.len() < max_distinct {
                    stat.distinct.insert(rendered, 1);
                } else {
                    stat.distinct_dropped += 1;
                }
            }
        }
    }

    /// Renders the human readable report printed by `stats`.
    pub fn report_text(&self, source: &str, top_values: usize) -> String {
        let mut out = String::new();
        out.push_str("file    ");
        out.push_str(source);
        out.push('\n');
        out.push_str(&format!(
            "lines   {:>12}   blank {}   invalid {}   valid {}\n",
            thousands(self.lines),
            thousands(self.blank),
            thousands(self.invalid),
            thousands(self.valid)
        ));
        out.push_str(&format!(
            "bytes   {:>12}   ({})\n",
            thousands(self.bytes),
            human_bytes(self.bytes)
        ));

        if !self.types.is_empty() {
            out.push_str(&format!("top level  {}\n", render_types(&self.types)));
        }

        if self.lengths.count() > 0 {
            out.push_str("\nline length in bytes\n");
            out.push_str(&format!(
                "  min {}   p50 {}   p90 {}   p99 {}   max {}   mean {:.1}\n",
                self.lengths.min().unwrap_or(0),
                self.lengths.quantile(0.50).unwrap_or(0),
                self.lengths.quantile(0.90).unwrap_or(0),
                self.lengths.quantile(0.99).unwrap_or(0),
                self.lengths.max().unwrap_or(0),
                self.lengths.mean().unwrap_or(0.0)
            ));
        }

        if !self.keys.is_empty() {
            out.push_str(&format!(
                "\ntop level keys over {} objects\n",
                thousands(self.objects)
            ));
            let mut rows: Vec<(&String, &KeyStat)> = self.keys.iter().collect();
            rows.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
            out.push_str("  key                          count     rate  types\n");
            for (name, stat) in rows {
                out.push_str(&format!(
                    "  {:<24} {:>10}  {:>6}  {}\n",
                    truncate(name, 24),
                    thousands(stat.count),
                    percent(stat.count, self.objects),
                    render_types(&stat.types)
                ));
            }
            if self.keys_dropped > 0 {
                out.push_str(&format!(
                    "  ... {} more key occurrences not tracked (limit {})\n",
                    thousands(self.keys_dropped),
                    self.options.max_keys
                ));
            }
        }

        for stat in &self.fields {
            out.push_str(&format!("\nfield {}\n", stat.path));
            out.push_str(&format!(
                "  present in {} of {} records ({}), {} values, types {}\n",
                thousands(stat.records_matched),
                thousands(self.valid),
                percent(stat.records_matched, self.valid),
                thousands(stat.values),
                render_types(&stat.types)
            ));
            let distinct = stat.distinct.len() as u64 + stat.distinct_dropped;
            out.push_str(&format!("  {} distinct values\n", thousands(distinct)));
            for (value, count) in stat.top(top_values) {
                out.push_str(&format!(
                    "    {:>10}  {:>6}  {}\n",
                    thousands(count),
                    percent(count, stat.values),
                    value
                ));
            }
        }

        if !self.issues.is_empty() {
            out.push_str(&format!(
                "\ninvalid lines ({} total, showing {})\n",
                thousands(self.invalid),
                self.issues.len()
            ));
            for issue in &self.issues {
                out.push_str(&format!(
                    "  line {} col {}: {}\n",
                    issue.line, issue.column, issue.reason
                ));
            }
        }
        out
    }

    /// Renders the same report as a single JSON object.
    pub fn report_json(&self, source: &str) -> String {
        let mut out = String::new();
        out.push('{');
        out.push_str("\"file\":");
        escape_into(source, &mut out);
        out.push_str(&format!(
            ",\"lines\":{},\"bytes\":{},\"blank\":{},\"valid\":{},\"invalid\":{},\"objects\":{}",
            self.lines, self.bytes, self.blank, self.valid, self.invalid, self.objects
        ));

        out.push_str(",\"line_length\":");
        if self.lengths.count() > 0 {
            let mean = format!("{:.4}", self.lengths.mean().unwrap_or(0.0));
            out.push('{');
            out.push_str(&format!(
                "\"min\":{},\"p50\":{},\"p90\":{},\"p99\":{},\"max\":{},\"mean\":{}",
                self.lengths.min().unwrap_or(0),
                self.lengths.quantile(0.50).unwrap_or(0),
                self.lengths.quantile(0.90).unwrap_or(0),
                self.lengths.quantile(0.99).unwrap_or(0),
                self.lengths.max().unwrap_or(0),
                mean
            ));
            out.push('}');
        } else {
            out.push_str("null");
        }

        out.push_str(",\"types\":");
        write_counts(&self.types, &mut out);

        out.push_str(",\"keys\":");
        out.push('{');
        for (i, (name, stat)) in self.keys.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            escape_into(name, &mut out);
            out.push_str(":{\"count\":");
            out.push_str(&stat.count.to_string());
            out.push_str(",\"types\":");
            write_counts(&stat.types, &mut out);
            out.push('}');
        }
        out.push('}');

        out.push_str(",\"fields\":[");
        for (i, stat) in self.fields.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"path\":");
            escape_into(stat.path.source(), &mut out);
            out.push_str(&format!(
                ",\"records_matched\":{},\"records_missing\":{},\"values\":{}",
                stat.records_matched, stat.records_missing, stat.values
            ));
            out.push_str(",\"types\":");
            write_counts(&stat.types, &mut out);
            out.push_str(",\"top\":[");
            for (j, (value, count)) in stat.top(20).into_iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                out.push_str("{\"value\":");
                escape_into(value, &mut out);
                out.push_str(",\"count\":");
                out.push_str(&count.to_string());
                out.push('}');
            }
            out.push_str("]}");
        }
        out.push(']');

        out.push_str(",\"issues\":[");
        for (i, issue) in self.issues.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('{');
            out.push_str("\"line\":");
            out.push_str(&issue.line.to_string());
            out.push_str(",\"column\":");
            out.push_str(&issue.column.to_string());
            out.push_str(",\"reason\":");
            escape_into(&issue.reason, &mut out);
            out.push('}');
        }
        out.push(']');
        out.push('}');
        out
    }
}

fn write_counts(counts: &BTreeMap<&'static str, u64>, out: &mut String) {
    out.push('{');
    for (i, (name, count)) in counts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        escape_into(name, out);
        out.push(':');
        out.push_str(&count.to_string());
    }
    out.push('}');
}

fn render_value(value: &Value) -> String {
    let mut text = value.to_json();
    if text.len() > MAX_VALUE_LEN {
        let mut cut = MAX_VALUE_LEN;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("...");
    }
    text
}

fn render_types(counts: &BTreeMap<&'static str, u64>) -> String {
    let mut rows: Vec<(&str, u64)> = counts.iter().map(|(k, v)| (*k, *v)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    rows.iter()
        .map(|(name, count)| format!("{}:{}", name, thousands(*count)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let head: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{}~", head)
}

/// Formats a percentage of `total`, or `-` when `total` is zero.
fn percent(count: u64, total: u64) -> String {
    if total == 0 {
        return "-".to_string();
    }
    format!("{:.1}%", count as f64 * 100.0 / total as f64)
}

/// Groups digits with thin separators for readability.
fn thousands(mut n: u64) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    while n > 0 {
        if n >= 1000 {
            parts.push(format!("{:03}", n % 1000));
        } else {
            parts.push(n.to_string());
        }
        n /= 1000;
    }
    parts.reverse();
    parts.join(",")
}

/// Renders a byte count with a binary unit suffix.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", n)
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = concat!(
        "{\"role\":\"user\",\"tokens\":12,\"meta\":{\"src\":\"web\"}}\n",
        "{\"role\":\"assistant\",\"tokens\":40,\"meta\":{\"src\":\"web\"}}\n",
        "\n",
        "{\"role\":\"user\",\"tokens\":null}\n",
        "{\"role\":\"user\",,}\n",
        "[1,2,3]\n",
    );

    fn stats_for(input: &str, fields: &[&str]) -> Stats {
        let options = StatsOptions {
            fields: fields
                .iter()
                .map(|f| FieldPath::parse(f).unwrap())
                .collect(),
            ..StatsOptions::default()
        };
        Stats::from_reader(input.as_bytes(), options).unwrap()
    }

    #[test]
    fn counts_lines_and_classifies_them() {
        let s = stats_for(SAMPLE, &[]);
        assert_eq!(s.lines, 6);
        assert_eq!(s.blank, 1);
        assert_eq!(s.invalid, 1);
        assert_eq!(s.valid, 4);
        assert_eq!(s.objects, 3);
        assert_eq!(s.bytes, SAMPLE.len() as u64);
        assert_eq!(s.types.get("object"), Some(&3));
        assert_eq!(s.types.get("array"), Some(&1));
    }

    #[test]
    fn reports_the_position_of_broken_lines() {
        let s = stats_for(SAMPLE, &[]);
        assert_eq!(s.issues.len(), 1);
        let issue = &s.issues[0];
        assert_eq!(issue.line, 5);
        assert_eq!(issue.column, 16);
        assert_eq!(issue.reason, "expected a quoted object key");
    }

    #[test]
    fn tracks_keys_and_their_types() {
        let s = stats_for(SAMPLE, &[]);
        let role = s.keys.get("role").unwrap();
        assert_eq!(role.count, 3);
        assert_eq!(role.types.get("string"), Some(&3));
        let tokens = s.keys.get("tokens").unwrap();
        assert_eq!(tokens.count, 3);
        assert_eq!(tokens.types.get("int"), Some(&2));
        assert_eq!(tokens.types.get("null"), Some(&1));
        // Nested keys are not top level keys.
        assert!(!s.keys.contains_key("src"));
        assert_eq!(s.keys.get("meta").unwrap().count, 2);
    }

    #[test]
    fn profiles_a_nested_field() {
        let s = stats_for(SAMPLE, &["meta.src"]);
        let field = &s.fields[0];
        assert_eq!(field.records_matched, 2);
        assert_eq!(field.records_missing, 2);
        assert_eq!(field.values, 2);
        assert_eq!(field.top(5), vec![("\"web\"", 2)]);
    }

    #[test]
    fn profiles_a_fan_out_field() {
        let input = concat!(
            "{\"messages\":[{\"role\":\"system\"},{\"role\":\"user\"}]}\n",
            "{\"messages\":[{\"role\":\"user\"}]}\n",
            "{\"messages\":[]}\n",
        );
        let s = stats_for(input, &["messages[].role"]);
        let field = &s.fields[0];
        assert_eq!(field.records_matched, 2);
        assert_eq!(field.records_missing, 1);
        assert_eq!(field.values, 3);
        assert_eq!(field.top(5), vec![("\"user\"", 2), ("\"system\"", 1)]);
    }

    #[test]
    fn caps_the_key_table() {
        let mut input = String::new();
        for i in 0..50 {
            input.push_str("{\"k");
            input.push_str(&i.to_string());
            input.push_str("\":1}\n");
        }
        let options = StatsOptions {
            max_keys: 10,
            ..StatsOptions::default()
        };
        let s = Stats::from_reader(input.as_bytes(), options).unwrap();
        assert_eq!(s.keys.len(), 10);
        assert_eq!(s.keys_dropped, 40);
        assert!(s.report_text("x", 5).contains("not tracked"));
    }

    #[test]
    fn caps_the_issue_list_but_not_the_count() {
        let input = "oops\n".repeat(25);
        let options = StatsOptions {
            max_issues: 3,
            ..StatsOptions::default()
        };
        let s = Stats::from_reader(input.as_bytes(), options).unwrap();
        assert_eq!(s.invalid, 25);
        assert_eq!(s.issues.len(), 3);
    }

    #[test]
    fn flags_invalid_utf8() {
        let input: Vec<u8> = vec![b'{', b'"', b'a', b'"', b':', b'"', 0xFF, b'"', b'}', b'\n'];
        let s = Stats::from_reader(&input[..], StatsOptions::default()).unwrap();
        assert_eq!(s.invalid, 1);
        assert_eq!(s.issues[0].reason, "invalid UTF-8");
        assert_eq!(s.issues[0].column, 7);
    }

    #[test]
    fn text_report_is_readable() {
        let s = stats_for(SAMPLE, &["meta.src"]);
        let report = s.report_text("sample.jsonl", 5);
        assert!(report.contains("file    sample.jsonl"));
        assert!(report.contains("top level keys over 3 objects"));
        assert!(report.contains("field meta.src"));
        assert!(report.contains("line 5 col 16"));
    }

    #[test]
    fn json_report_parses_back() {
        let s = stats_for(SAMPLE, &["meta.src"]);
        let report = s.report_json("sample.jsonl");
        let parsed = parse(&report).expect("report must be valid JSON");
        assert_eq!(parsed.get("lines").and_then(Value::as_i64), Some(6));
        assert_eq!(parsed.get("valid").and_then(Value::as_i64), Some(4));
        assert_eq!(
            parsed
                .get("keys")
                .and_then(|k| k.get("role"))
                .and_then(|r| r.get("count"))
                .and_then(Value::as_i64),
            Some(3)
        );
        assert_eq!(
            parsed
                .get("fields")
                .and_then(Value::as_array)
                .map(|f| f.len()),
            Some(1)
        );
    }

    #[test]
    fn empty_input_reports_cleanly() {
        let s = stats_for("", &[]);
        assert_eq!(s.lines, 0);
        let report = s.report_text("-", 5);
        assert!(report.contains("lines"));
        assert!(!report.contains("line length"));
        assert!(parse(&s.report_json("-")).is_ok());
    }

    #[test]
    fn helpers_format_numbers() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(percent(1, 4), "25.0%");
        assert_eq!(percent(1, 0), "-");
        assert_eq!(truncate("short", 24), "short");
        assert_eq!(truncate("abcdef", 3), "ab~");
    }
}
