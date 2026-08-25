//! CLI front end. Thin: argument parsing and output formatting only, the
//! actual work lives in the library.

#![forbid(unsafe_code)]

use std::fmt;
use std::fs;
use std::io::{self, BufRead, Write};

use jsonl_peek::{parse, FieldPath, LineReader, Reservoir, Schema, SchemaOptions, Stats, StatsOptions};

const USAGE: &str = "\
jsonl-peek head   [-n N] [FILE]
jsonl-peek sample [-n N] [--seed S] [FILE]
jsonl-peek stats  [--field PATH]... [--top N] [--max-errors N] [--json] [FILE]
jsonl-peek schema [--depth N] [--min-rate R] [--json] [FILE]

Reads standard input when FILE is omitted or is '-'.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(&args));
}

fn run(args: &[String]) -> i32 {
    let Some((cmd, rest)) = args.split_first() else {
        return usage_error("no command given");
    };
    match cmd.as_str() {
        "head" => cmd_head(rest),
        "sample" => cmd_sample(rest),
        "stats" => cmd_stats(rest),
        "schema" => cmd_schema(rest),
        "-h" | "--help" => {
            print!("{}", USAGE);
            0
        }
        other => usage_error(&format!("unknown command '{}'", other)),
    }
}

fn usage_error(msg: &str) -> i32 {
    eprintln!("jsonl-peek: {}\n", msg);
    eprint!("{}", USAGE);
    2
}

fn runtime_error(context: &str, err: &dyn fmt::Display) -> i32 {
    eprintln!("jsonl-peek: {}: {}", context, err);
    1
}

fn parse_num<T: std::str::FromStr>(text: &str, opt: &str) -> Result<T, String> {
    text.parse::<T>()
        .map_err(|_| format!("{} expects a number, got '{}'", opt, text))
}

fn open_input(path: Option<&str>) -> io::Result<Box<dyn BufRead>> {
    match path {
        None | Some("-") => Ok(Box::new(io::BufReader::new(io::stdin()))),
        Some(p) => Ok(Box::new(io::BufReader::new(fs::File::open(p)?))),
    }
}

fn default_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as u64 ^ (std::process::id() as u64),
        Err(_) => 0x2545_F491_4F6C_DD1D,
    }
}

fn cmd_head(args: &[String]) -> i32 {
    let mut n: usize = 10;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("-n requires a value");
                };
                match parse_num::<usize>(value, "-n") {
                    Ok(v) => n = v,
                    Err(msg) => return usage_error(&msg),
                }
            }
            "-h" | "--help" => {
                print!("{}", USAGE);
                return 0;
            }
            s if s.starts_with('-') && s != "-" => {
                return usage_error(&format!("head: unknown option '{}'", s))
            }
            s => {
                if file.is_some() {
                    return usage_error("head: too many arguments");
                }
                file = Some(s.to_string());
            }
        }
        i += 1;
    }

    let source = file.as_deref().unwrap_or("-").to_string();
    let reader = match open_input(file.as_deref()) {
        Ok(r) => r,
        Err(e) => return runtime_error(&source, &e),
    };

    let mut lines = LineReader::new(reader);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut shown = 0usize;
    while shown < n {
        match lines.next_line() {
            Ok(Some(line)) => {
                if let Err(e) = out.write_all(line.bytes).and_then(|_| out.write_all(b"\n")) {
                    return runtime_error("stdout", &e);
                }
                shown += 1;
            }
            Ok(None) => break,
            Err(e) => return runtime_error(&source, &e),
        }
    }
    0
}

fn cmd_sample(args: &[String]) -> i32 {
    let mut n: usize = 10;
    let mut seed: Option<u64> = None;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("-n requires a value");
                };
                match parse_num::<usize>(value, "-n") {
                    Ok(v) => n = v,
                    Err(msg) => return usage_error(&msg),
                }
            }
            "--seed" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--seed requires a value");
                };
                match parse_num::<u64>(value, "--seed") {
                    Ok(v) => seed = Some(v),
                    Err(msg) => return usage_error(&msg),
                }
            }
            "-h" | "--help" => {
                print!("{}", USAGE);
                return 0;
            }
            s if s.starts_with('-') && s != "-" => {
                return usage_error(&format!("sample: unknown option '{}'", s))
            }
            s => {
                if file.is_some() {
                    return usage_error("sample: too many arguments");
                }
                file = Some(s.to_string());
            }
        }
        i += 1;
    }

    let source = file.as_deref().unwrap_or("-").to_string();
    let reader = match open_input(file.as_deref()) {
        Ok(r) => r,
        Err(e) => return runtime_error(&source, &e),
    };

    let mut lines = LineReader::new(reader);
    let mut reservoir: Reservoir<Vec<u8>> = Reservoir::new(n, seed.unwrap_or_else(default_seed));
    let mut idx: u64 = 0;
    loop {
        match lines.next_line() {
            Ok(Some(line)) => {
                if line.is_blank() {
                    continue;
                }
                let bytes = line.bytes;
                reservoir.offer(idx, || bytes.to_vec());
                idx += 1;
            }
            Ok(None) => break,
            Err(e) => return runtime_error(&source, &e),
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (_, bytes) in reservoir.into_sorted() {
        if let Err(e) = out.write_all(&bytes).and_then(|_| out.write_all(b"\n")) {
            return runtime_error("stdout", &e);
        }
    }
    0
}

fn cmd_stats(args: &[String]) -> i32 {
    let mut fields: Vec<FieldPath> = Vec::new();
    let mut top: usize = 10;
    let mut max_errors: usize = 10;
    let mut json = false;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--field" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--field requires a value");
                };
                match FieldPath::parse(value) {
                    Ok(path) => fields.push(path),
                    Err(e) => {
                        return usage_error(&format!("invalid field path '{}': {}", value, e))
                    }
                }
            }
            "--top" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--top requires a value");
                };
                match parse_num::<usize>(value, "--top") {
                    Ok(v) => top = v,
                    Err(msg) => return usage_error(&msg),
                }
            }
            "--max-errors" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--max-errors requires a value");
                };
                match parse_num::<usize>(value, "--max-errors") {
                    Ok(v) => max_errors = v,
                    Err(msg) => return usage_error(&msg),
                }
            }
            "--json" => json = true,
            "-h" | "--help" => {
                print!("{}", USAGE);
                return 0;
            }
            s if s.starts_with('-') && s != "-" => {
                return usage_error(&format!("stats: unknown option '{}'", s))
            }
            s => {
                if file.is_some() {
                    return usage_error("stats: too many arguments");
                }
                file = Some(s.to_string());
            }
        }
        i += 1;
    }

    let source = file.as_deref().unwrap_or("-").to_string();
    let reader = match open_input(file.as_deref()) {
        Ok(r) => r,
        Err(e) => return runtime_error(&source, &e),
    };

    let options = StatsOptions {
        fields,
        max_issues: max_errors,
        ..StatsOptions::default()
    };
    let stats = match Stats::from_reader(reader, options) {
        Ok(s) => s,
        Err(e) => return runtime_error(&source, &e),
    };

    if json {
        println!("{}", stats.report_json(&source));
    } else {
        print!("{}", stats.report_text(&source, top));
    }
    0
}

fn cmd_schema(args: &[String]) -> i32 {
    let mut depth: usize = 3;
    let mut min_rate: f64 = 0.0;
    let mut json = false;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--depth" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--depth requires a value");
                };
                match parse_num::<usize>(value, "--depth") {
                    Ok(v) => depth = v,
                    Err(msg) => return usage_error(&msg),
                }
            }
            "--min-rate" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return usage_error("--min-rate requires a value");
                };
                match parse_num::<f64>(value, "--min-rate") {
                    Ok(v) => min_rate = v,
                    Err(msg) => return usage_error(&msg),
                }
            }
            "--json" => json = true,
            "-h" | "--help" => {
                print!("{}", USAGE);
                return 0;
            }
            s if s.starts_with('-') && s != "-" => {
                return usage_error(&format!("schema: unknown option '{}'", s))
            }
            s => {
                if file.is_some() {
                    return usage_error("schema: too many arguments");
                }
                file = Some(s.to_string());
            }
        }
        i += 1;
    }

    let source = file.as_deref().unwrap_or("-").to_string();
    let reader = match open_input(file.as_deref()) {
        Ok(r) => r,
        Err(e) => return runtime_error(&source, &e),
    };

    let mut schema = Schema::new(SchemaOptions {
        max_depth: depth,
        ..SchemaOptions::default()
    });
    let mut lines = LineReader::new(reader);
    let mut skipped: u64 = 0;
    loop {
        match lines.next_line() {
            Ok(Some(line)) => {
                if line.is_blank() {
                    continue;
                }
                match std::str::from_utf8(line.bytes) {
                    Ok(text) => match parse(text) {
                        Ok(value) => schema.observe(&value),
                        Err(_) => skipped += 1,
                    },
                    Err(_) => skipped += 1,
                }
            }
            Ok(None) => break,
            Err(e) => return runtime_error(&source, &e),
        }
    }

    if json {
        println!("{}", schema.report_json(min_rate));
    } else {
        print!("{}", schema.report_text(min_rate));
        if skipped > 0 {
            println!();
            println!("{} unparseable lines skipped", skipped);
        }
    }
    0
}
