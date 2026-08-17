//! Fast, single-pass inspection of JSONL datasets.
//!
//! The crate is built around one constraint: a training set does not fit in
//! memory, and you only get to read it once. Every piece here is a streaming
//! accumulator with a bounded footprint.
//!
//! * [`json`] - a strict, hand written JSON parser that reports the byte offset
//!   of whatever broke.
//! * [`lines`] - a reusable-buffer line reader that copes with CRLF, a BOM and
//!   a missing final newline.
//! * [`hist`] - a log-bucketed histogram, for percentiles without keeping the
//!   samples.
//! * [`rng`] - SplitMix64 plus reservoir sampling, for unbiased samples of a
//!   stream of unknown length.
//! * [`path`] - `a.b[].c` field paths.
//! * [`stats`] - the file health check.
//! * [`schema`] - path/type inference across records.
//!
//! ```
//! use jsonl_peek::{FieldPath, Stats, StatsOptions};
//!
//! let data = "{\"role\":\"user\"}\n{\"role\":\"bot\"}\nbroken\n";
//! let options = StatsOptions {
//!     fields: vec![FieldPath::parse("role").unwrap()],
//!     ..StatsOptions::default()
//! };
//! let stats = Stats::from_reader(data.as_bytes(), options).unwrap();
//!
//! assert_eq!(stats.lines, 3);
//! assert_eq!(stats.valid, 2);
//! assert_eq!(stats.invalid, 1);
//! assert_eq!(stats.issues[0].line, 3);
//! assert_eq!(stats.fields[0].values, 2);
//! ```

#![forbid(unsafe_code)]

pub mod hist;
pub mod json;
pub mod lines;
pub mod path;
pub mod rng;
pub mod schema;
pub mod stats;

pub use hist::Histogram;
pub use json::{parse, ParseError, Value};
pub use lines::{Line, LineReader};
pub use path::{FieldPath, PathError};
pub use rng::{Reservoir, SplitMix64};
pub use schema::{Schema, SchemaOptions};
pub use stats::{Stats, StatsOptions};

/// Crate version, taken from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
