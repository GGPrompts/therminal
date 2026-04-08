//! Tiny output helpers for the CLI.
//!
//! The default output is **TSV** — one record per line, fields separated by
//! tabs, no headers, no padding, no ANSI color. This is intentional: the
//! whole point of the CLI surface (tn-k13n) is that the parent agent's
//! prompt cache shouldn't take a hit when an MCP client polls a swarm.
//! 5 panes should fit in <300 bytes.
//!
//! `--json` produces structured output via `serde_json::to_string`. We do
//! not pretty-print: a one-line JSON document keeps the cache footprint
//! flat and is just as parseable downstream.

use std::io::{self, Write};

use serde::Serialize;

/// Print a single line to stdout, mapped from a tab-separated field iterator.
///
/// Tabs and newlines inside fields are silently replaced with spaces so the
/// output stays one-record-per-line and machine-parseable.
pub fn write_tsv_row<I, S>(out: &mut impl Write, fields: I) -> io::Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut first = true;
    for field in fields {
        if !first {
            out.write_all(b"\t")?;
        }
        first = false;
        let raw = field.as_ref();
        // Cheap sanitization — covers the realistic cases (cwd, tag values).
        if raw.contains('\t') || raw.contains('\n') {
            let cleaned: String = raw
                .chars()
                .map(|c| match c {
                    '\t' | '\n' | '\r' => ' ',
                    other => other,
                })
                .collect();
            out.write_all(cleaned.as_bytes())?;
        } else {
            out.write_all(raw.as_bytes())?;
        }
    }
    out.write_all(b"\n")?;
    Ok(())
}

/// Emit a structured value as a single-line JSON document on stdout.
pub fn write_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    let s = serde_json::to_string(value)?;
    let mut out = io::stdout().lock();
    out.write_all(s.as_bytes())?;
    out.write_all(b"\n")?;
    Ok(())
}

/// Convenience: format an `Option<String>` as the empty string when `None`.
/// CLI consumers parsing TSV can detect the absence by an empty field.
pub fn opt_str(o: &Option<String>) -> &str {
    o.as_deref().unwrap_or("")
}

/// Convenience: format an `Option<i32>` as the empty string when `None`.
pub fn opt_i32(o: Option<i32>) -> String {
    match o {
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// Render an opaque `HashMap<String,String>` tag bag as a stable
/// `key=value,key=value` form, sorted by key for byte-stable output.
pub fn tags_compact(tags: &std::collections::HashMap<String, String>) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut entries: Vec<_> = tags.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = String::new();
    for (i, (k, v)) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out
}
