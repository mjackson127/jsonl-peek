//! Schema inference over a stream of records.
//!
//! Training sets rarely ship with a schema, and the one in the README is
//! usually out of date. [`Schema`] walks every record and records, for each
//! dotted path, how many documents contain it and which types the value took.
//! Arrays are traversed with a `[]` step so that `messages[].role` shows up as
//! its own path.
//!
//! ```
//! use jsonl_peek::{json::parse, schema::{Schema, SchemaOptions}};
//!
//! let mut schema = Schema::new(SchemaOptions::default());
//! schema.observe(&parse(r#"{"a":{"b":1}}"#).unwrap());
//! schema.observe(&parse(r#"{"a":{"b":"x"},"c":true}"#).unwrap());
//!
//! let a_b = schema.paths.get("a.b").unwrap();
//! assert_eq!(a_b.documents, 2);
//! assert_eq!(a_b.types.get("int"), Some(&1));
//! assert_eq!(a_b.types.get("string"), Some(&1));
//! ```

use std::collections::{BTreeMap, HashSet};

use crate::json::{escape_into, Value};

/// Statistics for one inferred path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathStat {
    /// Documents in which the path occurs at least once.
    pub documents: u64,
    /// Total occurrences (a `[]` step can match many times per document).
    pub occurrences: u64,
    /// Value types seen at this path.
    pub types: BTreeMap<&'static str, u64>,
}

impl PathStat {
    /// Types, most frequent first.
    pub fn types_by_frequency(&self) -> Vec<(&'static str, u64)> {
        let mut rows: Vec<(&'static str, u64)> =
            self.types.iter().map(|(k, v)| (*k, *v)).collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        rows
    }
}

/// Knobs for [`Schema`].
#[derive(Debug, Clone)]
pub struct SchemaOptions {
    /// How many levels to descend. `1` records only top level members.
    pub max_depth: usize,
    /// Upper bound on the number of distinct paths tracked.
    pub max_paths: usize,
}

impl Default for SchemaOptions {
    fn default() -> Self {
        SchemaOptions {
            max_depth: 3,
            max_paths: 2_000,
        }
    }
}

/// Inferred structure of a collection of records.
#[derive(Debug)]
pub struct Schema {
    options: SchemaOptions,
    /// Documents folded in so far.
    pub documents: u64,
    /// Path to statistics, in lexicographic path order.
    pub paths: BTreeMap<String, PathStat>,
    /// True when [`SchemaOptions::max_paths`] stopped new paths being tracked.
    pub truncated: bool,
    seen: HashSet<String>,
    buffer: String,
}

impl Schema {
    /// Creates an empty schema.
    pub fn new(options: SchemaOptions) -> Self {
        Schema {
            options,
            documents: 0,
            paths: BTreeMap::new(),
            truncated: false,
            seen: HashSet::new(),
            buffer: String::new(),
        }
    }

    /// Folds one record into the schema.
    pub fn observe(&mut self, value: &Value) {
        self.documents += 1;
        self.seen.clear();
        let mut prefix = std::mem::take(&mut self.buffer);
        prefix.clear();
        self.walk(&mut prefix, value, 0);
        self.buffer = prefix;
    }

    fn walk(&mut self, prefix: &mut String, value: &Value, depth: usize) {
        if depth >= self.options.max_depth {
            return;
        }
        match value {
            Value::Object(members) => {
                for (name, member) in members {
                    let restore = prefix.len();
                    if !prefix.is_empty() {
                        prefix.push('.');
                    }
                    prefix.push_str(name);
                    self.touch(prefix, member);
                    self.walk(prefix, member, depth + 1);
                    prefix.truncate(restore);
                }
            }
            Value::Array(items) => {
                let restore = prefix.len();
                prefix.push_str("[]");
                for item in items {
                    self.touch(prefix, item);
                    self.walk(prefix, item, depth + 1);
                }
                prefix.truncate(restore);
            }
            _ => {}
        }
    }

    fn touch(&mut self, path: &str, value: &Value) {
        let fresh_document = !self.seen.contains(path);
        if let Some(stat) = self.paths.get_mut(path) {
            stat.occurrences += 1;
            if fresh_document {
                stat.documents += 1;
            }
            *stat.types.entry(value.type_name()).or_insert(0) += 1;
        } else if self.paths.len() < self.options.max_paths {
            let mut stat = PathStat {
                documents: 1,
                occurrences: 1,
                types: BTreeMap::new(),
            };
            stat.types.insert(value.type_name(), 1);
            self.paths.insert(path.to_string(), stat);
        } else {
            self.truncated = true;
            return;
        }
        if fresh_document {
            self.seen.insert(path.to_string());
        }
    }

    /// Paths that occur in at least `min_rate` of documents, sorted by path.
    pub fn filtered(&self, min_rate: f64) -> Vec<(&str, &PathStat)> {
        self.paths
            .iter()
            .filter(|(_, stat)| {
                self.documents == 0
                    || stat.documents as f64 / self.documents as f64 >= min_rate
            })
            .map(|(path, stat)| (path.as_str(), stat))
            .collect()
    }

    /// Renders the table printed by the `schema` command.
    pub fn report_text(&self, min_rate: f64) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{} records, depth {}\n\n",
            self.documents, self.options.max_depth
        ));
        out.push_str(&format!("  {:<38} {:>7}  {}\n", "path", "rate", "types"));
        for (path, stat) in self.filtered(min_rate) {
            let rate = if self.documents == 0 {
                0.0
            } else {
                stat.documents as f64 * 100.0 / self.documents as f64
            };
            let types = stat
                .types_by_frequency()
                .into_iter()
                .map(|(name, count)| format!("{}:{}", name, count))
                .collect::<Vec<_>>()
                .join(" ");
            out.push_str(&format!("  {:<38} {:>6.1}%  {}\n", path, rate, types));
        }
        if self.truncated {
            out.push_str("  ... path limit reached, output is incomplete\n");
        }
        out
    }

    /// Renders the schema as a JSON object keyed by path.
    pub fn report_json(&self, min_rate: f64) -> String {
        let mut out = String::new();
        out.push('{');
        out.push_str("\"records\":");
        out.push_str(&self.documents.to_string());
        out.push_str(",\"truncated\":");
        out.push_str(if self.truncated { "true" } else { "false" });
        out.push_str(",\"paths\":");
        out.push('{');
        for (i, (path, stat)) in self.filtered(min_rate).into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            escape_into(path, &mut out);
            out.push_str(":{\"documents\":");
            out.push_str(&stat.documents.to_string());
            out.push_str(",\"occurrences\":");
            out.push_str(&stat.occurrences.to_string());
            out.push_str(",\"types\":");
            out.push('{');
            for (j, (name, count)) in stat.types_by_frequency().into_iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                escape_into(name, &mut out);
                out.push(':');
                out.push_str(&count.to_string());
            }
            out.push('}');
            out.push('}');
        }
        out.push('}');
        out.push('}');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn schema_of(lines: &[&str], options: SchemaOptions) -> Schema {
        let mut schema = Schema::new(options);
        for line in lines {
            schema.observe(&parse(line).unwrap());
        }
        schema
    }

    const CHAT: [&str; 3] = [
        r#"{"id":1,"messages":[{"role":"system","content":"a"},{"role":"user","content":"b"}]}"#,
        r#"{"id":2,"messages":[{"role":"user","content":"c"}],"meta":{"lang":"en"}}"#,
        r#"{"id":"3","messages":[]}"#,
    ];

    #[test]
    fn records_top_level_paths() {
        let s = schema_of(&CHAT, SchemaOptions::default());
        assert_eq!(s.documents, 3);
        let id = s.paths.get("id").unwrap();
        assert_eq!(id.documents, 3);
        assert_eq!(id.types.get("int"), Some(&2));
        assert_eq!(id.types.get("string"), Some(&1));
        assert_eq!(s.paths.get("meta").unwrap().documents, 1);
    }

    #[test]
    fn descends_through_arrays() {
        let s = schema_of(&CHAT, SchemaOptions::default());
        let role = s.paths.get("messages[].role").unwrap();
        // Three messages in total, but only two documents contain any.
        assert_eq!(role.occurrences, 3);
        assert_eq!(role.documents, 2);
        assert_eq!(role.types.get("string"), Some(&3));
        assert_eq!(s.paths.get("messages[]").unwrap().occurrences, 3);
    }

    #[test]
    fn depth_limits_the_walk() {
        let shallow = schema_of(&CHAT, SchemaOptions { max_depth: 1, ..SchemaOptions::default() });
        assert!(shallow.paths.contains_key("messages"));
        assert!(!shallow.paths.contains_key("messages[]"));

        let two = schema_of(&CHAT, SchemaOptions { max_depth: 2, ..SchemaOptions::default() });
        assert!(two.paths.contains_key("messages[]"));
        assert!(!two.paths.contains_key("messages[].role"));
        assert!(two.paths.contains_key("meta.lang"));
    }

    #[test]
    fn path_limit_sets_the_truncated_flag() {
        let s = schema_of(
            &CHAT,
            SchemaOptions {
                max_paths: 2,
                ..SchemaOptions::default()
            },
        );
        assert_eq!(s.paths.len(), 2);
        assert!(s.truncated);
        assert!(s.report_text(0.0).contains("path limit reached"));
    }

    #[test]
    fn min_rate_filters_rare_paths() {
        let s = schema_of(&CHAT, SchemaOptions::default());
        let common: Vec<&str> = s.filtered(0.9).into_iter().map(|(p, _)| p).collect();
        assert!(common.contains(&"id"));
        assert!(!common.contains(&"meta"), "meta appears in 1 of 3 records");
        assert!(s.filtered(0.0).len() > common.len());
    }

    #[test]
    fn non_object_records_are_still_counted() {
        let s = schema_of(&["[1,2]", "5", r#"{"a":1}"#], SchemaOptions::default());
        assert_eq!(s.documents, 3);
        assert_eq!(s.paths.get("[]").unwrap().occurrences, 2);
        assert_eq!(s.paths.get("a").unwrap().documents, 1);
    }

    #[test]
    fn reports_render() {
        let s = schema_of(&CHAT, SchemaOptions::default());
        let text = s.report_text(0.0);
        assert!(text.starts_with("3 records, depth 3"));
        assert!(text.contains("messages[].role"));

        let json = parse(&s.report_json(0.0)).expect("schema report must be valid JSON");
        assert_eq!(json.get("records").and_then(Value::as_i64), Some(3));
        let paths = json.get("paths").unwrap();
        assert_eq!(
            paths
                .get("messages[].role")
                .and_then(|p| p.get("occurrences"))
                .and_then(Value::as_i64),
            Some(3)
        );
    }

    #[test]
    fn repeated_paths_in_one_document_count_once_per_document() {
        let s = schema_of(
            &[r#"{"xs":[{"v":1},{"v":2},{"v":3}]}"#],
            SchemaOptions::default(),
        );
        let v = s.paths.get("xs[].v").unwrap();
        assert_eq!(v.documents, 1);
        assert_eq!(v.occurrences, 3);
    }
}
