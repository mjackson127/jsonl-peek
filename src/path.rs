//! Field paths for reaching into nested records.
//!
//! Syntax:
//!
//! | Path | Meaning |
//! | --- | --- |
//! | `role` | the top level member `role` |
//! | `meta.source` | a member of a nested object |
//! | `messages[0].content` | the first element of an array |
//! | `messages[].role` | every element of an array |
//! | `messages[-1]` | the last element of an array |
//! | `[0].id` | the file holds top level arrays |
//!
//! A path can select more than one value (`[]` fans out), so [`FieldPath::select`]
//! returns a list. Member names are taken literally: dots inside a key cannot
//! be escaped.

use crate::json::Value;
use std::fmt;

/// One step of a [`FieldPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Object member lookup.
    Key(String),
    /// Array element by position; negative counts from the end.
    Index(i64),
    /// Every element of an array.
    Each,
}

/// A compiled field path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath {
    source: String,
    segments: Vec<Segment>,
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl FieldPath {
    /// Compiles a path expression.
    pub fn parse(source: &str) -> Result<FieldPath, PathError> {
        let bytes = source.as_bytes();
        let mut segments = Vec::new();
        let mut i = 0usize;
        let mut expect_key = true;

        while i < bytes.len() {
            match bytes[i] {
                b'[' => {
                    let close = source[i..]
                        .find(']')
                        .map(|off| i + off)
                        .ok_or(PathError::UnclosedBracket(i))?;
                    let inner = &source[i + 1..close];
                    if inner.is_empty() {
                        segments.push(Segment::Each);
                    } else {
                        let index: i64 = inner
                            .parse()
                            .map_err(|_| PathError::BadIndex(i + 1, inner.to_string()))?;
                        segments.push(Segment::Index(index));
                    }
                    i = close + 1;
                    expect_key = false;
                }
                b'.' => {
                    if expect_key {
                        return Err(PathError::EmptySegment(i));
                    }
                    i += 1;
                    expect_key = true;
                }
                _ => {
                    if !expect_key {
                        return Err(PathError::MissingSeparator(i));
                    }
                    let end = source[i..]
                        .find(['.', '['])
                        .map(|off| i + off)
                        .unwrap_or(source.len());
                    segments.push(Segment::Key(source[i..end].to_string()));
                    i = end;
                    expect_key = false;
                }
            }
        }

        if segments.is_empty() {
            return Err(PathError::Empty);
        }
        if expect_key {
            return Err(PathError::EmptySegment(bytes.len()));
        }
        Ok(FieldPath {
            source: source.to_string(),
            segments,
        })
    }

    /// The original path text.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The compiled steps.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Collects every value the path selects inside `root`.
    ///
    /// An empty result means the path does not exist in this record, which the
    /// callers report as "missing" rather than as an error.
    pub fn select<'v>(&self, root: &'v Value) -> Vec<&'v Value> {
        let mut current: Vec<&'v Value> = vec![root];
        let mut next: Vec<&'v Value> = Vec::new();
        for segment in &self.segments {
            next.clear();
            for value in current.drain(..) {
                match segment {
                    Segment::Key(name) => {
                        if let Some(found) = value.get(name) {
                            next.push(found);
                        }
                    }
                    Segment::Index(index) => {
                        if let Some(items) = value.as_array() {
                            let resolved = if *index < 0 {
                                items.len() as i64 + index
                            } else {
                                *index
                            };
                            if resolved >= 0 {
                                if let Some(found) = items.get(resolved as usize) {
                                    next.push(found);
                                }
                            }
                        }
                    }
                    Segment::Each => {
                        if let Some(items) = value.as_array() {
                            next.extend(items.iter());
                        }
                    }
                }
            }
            std::mem::swap(&mut current, &mut next);
            if current.is_empty() {
                return Vec::new();
            }
        }
        current
    }
}

/// Why a path expression could not be compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// The expression was empty.
    Empty,
    /// A `.` with nothing on one side, at the given offset.
    EmptySegment(usize),
    /// A `[` without a matching `]`, at the given offset.
    UnclosedBracket(usize),
    /// The text between brackets was not an integer.
    BadIndex(usize, String),
    /// A key started right after `]` without a `.` in between.
    MissingSeparator(usize),
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::Empty => f.write_str("empty field path"),
            PathError::EmptySegment(at) => write!(f, "empty path segment at offset {}", at),
            PathError::UnclosedBracket(at) => write!(f, "unclosed '[' at offset {}", at),
            PathError::BadIndex(at, text) => {
                write!(f, "invalid array index '{}' at offset {}", text, at)
            }
            PathError::MissingSeparator(at) => {
                write!(f, "expected '.' before the key at offset {}", at)
            }
        }
    }
}

impl std::error::Error for PathError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn doc() -> Value {
        parse(
            r#"{
                "id": 7,
                "meta": {"source": "web", "tags": ["a", "b"]},
                "messages": [
                    {"role": "system", "content": "be nice"},
                    {"role": "user", "content": "hi"},
                    {"role": "assistant"}
                ]
            }"#,
        )
        .unwrap()
    }

    fn select_json(path: &str, value: &Value) -> Vec<String> {
        FieldPath::parse(path)
            .unwrap()
            .select(value)
            .into_iter()
            .map(|v| v.to_json())
            .collect()
    }

    #[test]
    fn compiles_segments() {
        let p = FieldPath::parse("messages[].role").unwrap();
        assert_eq!(
            p.segments(),
            &[
                Segment::Key("messages".to_string()),
                Segment::Each,
                Segment::Key("role".to_string())
            ]
        );
        assert_eq!(p.source(), "messages[].role");
        assert_eq!(p.to_string(), "messages[].role");
    }

    #[test]
    fn selects_scalars_and_nested_objects() {
        let d = doc();
        assert_eq!(select_json("id", &d), vec!["7"]);
        assert_eq!(select_json("meta.source", &d), vec![r#""web""#]);
        assert_eq!(select_json("meta.tags[1]", &d), vec![r#""b""#]);
    }

    #[test]
    fn fans_out_over_arrays() {
        let d = doc();
        assert_eq!(
            select_json("messages[].role", &d),
            vec![r#""system""#, r#""user""#, r#""assistant""#]
        );
        // The third message has no "content", so only two values come back.
        assert_eq!(
            select_json("messages[].content", &d),
            vec![r#""be nice""#, r#""hi""#]
        );
    }

    #[test]
    fn supports_negative_and_out_of_range_indices() {
        let d = doc();
        assert_eq!(select_json("messages[-1].role", &d), vec![r#""assistant""#]);
        assert!(select_json("messages[9]", &d).is_empty());
        assert!(select_json("messages[-9]", &d).is_empty());
    }

    #[test]
    fn missing_and_type_mismatched_paths_select_nothing() {
        let d = doc();
        assert!(select_json("nope", &d).is_empty());
        assert!(select_json("meta.source.deeper", &d).is_empty());
        assert!(select_json("id[]", &d).is_empty());
        assert!(select_json("meta[0]", &d).is_empty());
    }

    #[test]
    fn top_level_arrays_work() {
        let d = parse(r#"[{"a":1},{"a":2}]"#).unwrap();
        assert_eq!(select_json("[].a", &d), vec!["1", "2"]);
        assert_eq!(select_json("[0].a", &d), vec!["1"]);
    }

    #[test]
    fn rejects_malformed_paths() {
        assert_eq!(FieldPath::parse(""), Err(PathError::Empty));
        assert_eq!(FieldPath::parse("a."), Err(PathError::EmptySegment(2)));
        assert_eq!(FieldPath::parse(".a"), Err(PathError::EmptySegment(0)));
        assert_eq!(FieldPath::parse("a..b"), Err(PathError::EmptySegment(2)));
        assert_eq!(FieldPath::parse("a[0"), Err(PathError::UnclosedBracket(1)));
        assert_eq!(
            FieldPath::parse("a[x]"),
            Err(PathError::BadIndex(2, "x".to_string()))
        );
        assert_eq!(FieldPath::parse("a[0]b"), Err(PathError::MissingSeparator(4)));
        assert_eq!(
            FieldPath::parse("a[0").unwrap_err().to_string(),
            "unclosed '[' at offset 1"
        );
    }
}
