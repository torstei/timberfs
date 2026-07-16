//! `timberfs list`: the directory-level complement to `info` — what stores
//! exist, and their vital signs, across the configured forests (or a given
//! set of directories). Read-only and lock-free, like `info`: it uses the
//! same `StoreSummary` (see query.rs), so the two commands never disagree
//! about a store's size, span, writer state, index or retention.
//!
//! Unlike handle resolution, which refuses an ambiguous handle, `list` is
//! how a user SEES the ambiguity: the same handle in two forests shows up
//! as two rows, never deduped or merged.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::query::StoreSummary;

/// One discovered store, ready to become a row.
struct Row {
    handle: String,
    forest: String,
    dir: PathBuf,
    path: PathBuf,
    summary: StoreSummary,
}

/// `timberfs list [DIR ...]`: every store in every configured forest, or —
/// when one or more directories are given — exactly the stores in those
/// directories (ad-hoc; they need not be configured forests).
pub fn cmd_list(dirs: &[PathBuf], names_only: bool, json: bool) -> anyhow::Result<()> {
    let forests = crate::forest::forests_for_list(dirs);
    if dirs.is_empty() && forests.is_empty() {
        crate::note!("timberfs: no forests configured (see /etc/timberfs/forests.d/)");
        return Ok(());
    }

    let mut rows: Vec<Row> = Vec::new();
    for forest in &forests {
        if !forest.dir.is_dir() {
            crate::note!(
                "timberfs: forest `{}` ({}) not found; skipping",
                forest.name,
                forest.dir.display()
            );
            continue;
        }
        for (handle, path) in crate::forest::scan_forest(&forest.dir) {
            match open_summary(&path) {
                Ok((dir, summary)) => rows.push(Row {
                    handle,
                    forest: forest.name.clone(),
                    dir,
                    path,
                    summary,
                }),
                Err(e) => crate::note!("timberfs: {}: {e}", path.display()),
            }
        }
    }
    rows.sort_by(|a, b| {
        (a.forest.as_str(), a.handle.as_str()).cmp(&(b.forest.as_str(), b.handle.as_str()))
    });

    if names_only {
        for r in &rows {
            println!("{}", r.handle);
        }
        return Ok(());
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&rows_to_json(&rows))?);
        return Ok(());
    }
    print_table(&rows);
    Ok(())
}

/// Read a store's index and manifest directly (no trunk file needed — list
/// never reads data), and summarize it.
fn open_summary(logical: &Path) -> anyhow::Result<(PathBuf, StoreSummary)> {
    let (dir, name) = crate::query::resolve_backing(logical)?;
    let rings = crate::format::rings_path(&dir, &name);
    let records = crate::format::read_index(&rings)
        .with_context(|| format!("reading index {}", rings.display()))?;
    let bark = crate::bark::load(&dir, &name);
    let summary = crate::query::summarize_store(&dir, &name, &records, bark.as_ref());
    Ok((dir, summary))
}

/// The RETAIN column: the declared policy, or `-` when none is declared.
fn retain_text(s: &StoreSummary) -> String {
    match (&s.retain, &s.retain_size) {
        (None, None) => "-".to_string(),
        (Some(r), None) => r.clone(),
        (None, Some(r)) => r.clone(),
        (Some(r), Some(rs)) => format!("{r}, {rs}"),
    }
}

/// The SPAN column: the write-time window covered, or `empty` for a store
/// with no chunks yet.
fn span_text(s: &StoreSummary) -> String {
    match (s.first_write_ms, s.last_write_ms) {
        (Some(f), Some(l)) => format!("{} .. {}", crate::query::fmt_ms(f), crate::query::fmt_ms(l)),
        _ => "empty".to_string(),
    }
}

const COLUMNS: [&str; 7] = [
    "HANDLE", "FOREST", "SIZE", "SPAN", "WRITER", "INDEX", "RETAIN",
];

/// One row's cells, in `COLUMNS` order — a pure function of a `Row`, so it
/// (and the table it feeds) is unit-testable without touching disk.
fn row_cells(r: &Row) -> [String; 7] {
    [
        r.handle.clone(),
        r.forest.clone(),
        crate::rotate::human_bytes(r.summary.compressed_bytes),
        span_text(&r.summary),
        if r.summary.writer.is_live() {
            "live"
        } else {
            "-"
        }
        .to_string(),
        if r.summary.indexed() { "grain" } else { "-" }.to_string(),
        retain_text(&r.summary),
    ]
}

/// Render an aligned table: a header plus one row per store, columns
/// left-aligned and sized to the widest cell (handles/forest names have no
/// fixed width, unlike `info`'s fixed-width tables).
fn format_table(header: &[&str], rows: &[[String; 7]]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let format_row = |cells: &[&str]| -> String {
        let line: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<width$}", width = widths[i]))
            .collect();
        line.join("  ").trim_end().to_string()
    };
    let mut out = String::new();
    out.push_str(&format_row(header));
    for row in rows {
        out.push('\n');
        out.push_str(&format_row(
            &row.iter().map(String::as_str).collect::<Vec<_>>(),
        ));
    }
    out
}

fn print_table(rows: &[Row]) {
    let data: Vec<[String; 7]> = rows.iter().map(row_cells).collect();
    println!("{}", format_table(&COLUMNS, &data));
}

fn rows_to_json(rows: &[Row]) -> serde_json::Value {
    serde_json::Value::Array(
        rows.iter()
            .map(|r| {
                let s = &r.summary;
                let mut o = serde_json::Map::new();
                o.insert("handle".to_string(), r.handle.clone().into());
                o.insert("forest".to_string(), r.forest.clone().into());
                o.insert("dir".to_string(), r.dir.display().to_string().into());
                o.insert("path".to_string(), r.path.display().to_string().into());
                o.insert("size_bytes".to_string(), s.compressed_bytes.into());
                o.insert(
                    "from_ms".to_string(),
                    s.first_write_ms
                        .map(Into::into)
                        .unwrap_or(serde_json::Value::Null),
                );
                o.insert(
                    "to_ms".to_string(),
                    s.last_write_ms
                        .map(Into::into)
                        .unwrap_or(serde_json::Value::Null),
                );
                o.insert("writer_live".to_string(), s.writer.is_live().into());
                o.insert("indexed".to_string(), s.indexed().into());
                o.insert(
                    "retain".to_string(),
                    s.retain
                        .clone()
                        .map(Into::into)
                        .unwrap_or(serde_json::Value::Null),
                );
                o.insert(
                    "retain_size".to_string(),
                    s.retain_size
                        .clone()
                        .map(Into::into)
                        .unwrap_or(serde_json::Value::Null),
                );
                serde_json::Value::Object(o)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::WriterState;

    fn summary(
        compressed_bytes: u64,
        span: Option<(u64, u64)>,
        writer: WriterState,
        indexed: bool,
        retain: Option<&str>,
        retain_size: Option<&str>,
    ) -> StoreSummary {
        StoreSummary {
            chunks: if span.is_some() { 1 } else { 0 },
            logical_bytes: compressed_bytes * 2,
            compressed_bytes,
            first_write_ms: span.map(|(f, _)| f),
            last_write_ms: span.map(|(_, l)| l),
            rings_bytes: 64,
            grain: if indexed { Some((10, 1)) } else { None },
            index_declared: false,
            retain: retain.map(str::to_string),
            retain_size: retain_size.map(str::to_string),
            writer,
        }
    }

    fn row(handle: &str, forest: &str, summary: StoreSummary) -> Row {
        Row {
            handle: handle.to_string(),
            forest: forest.to_string(),
            dir: PathBuf::from("/var/log/timberfs"),
            path: PathBuf::from(format!("/var/log/timberfs/{handle}.log")),
            summary,
        }
    }

    #[test]
    fn retain_text_reports_declared_policy_or_dash() {
        assert_eq!(
            retain_text(&summary(0, None, WriterState::Idle, false, None, None)),
            "-"
        );
        assert_eq!(
            retain_text(&summary(
                0,
                None,
                WriterState::Idle,
                false,
                Some("30d"),
                None
            )),
            "30d"
        );
        assert_eq!(
            retain_text(&summary(
                0,
                None,
                WriterState::Idle,
                false,
                None,
                Some("50G")
            )),
            "50G"
        );
        assert_eq!(
            retain_text(&summary(
                0,
                None,
                WriterState::Idle,
                false,
                Some("30d"),
                Some("50G")
            )),
            "30d, 50G"
        );
    }

    #[test]
    fn span_text_reports_the_write_window_or_empty() {
        assert_eq!(
            span_text(&summary(0, None, WriterState::Idle, false, None, None)),
            "empty"
        );
        let s = summary(0, Some((0, 1000)), WriterState::Idle, false, None, None);
        assert!(span_text(&s).contains(".."));
    }

    #[test]
    fn row_cells_reflect_writer_and_index_state() {
        let live = row(
            "nginx",
            "default",
            summary(2048, Some((0, 1000)), WriterState::Active, true, None, None),
        );
        let cells = row_cells(&live);
        assert_eq!(cells[0], "nginx");
        assert_eq!(cells[1], "default");
        assert_eq!(cells[4], "live");
        assert_eq!(cells[5], "grain");

        let idle = row(
            "db",
            "default",
            summary(0, None, WriterState::Idle, false, None, None),
        );
        let cells = row_cells(&idle);
        assert_eq!(cells[3], "empty");
        assert_eq!(cells[4], "-");
        assert_eq!(cells[5], "-");
    }

    #[test]
    fn table_aligns_columns_to_the_widest_cell() {
        let rows = [
            row(
                "nginx",
                "default",
                summary(
                    2048,
                    Some((0, 1000)),
                    WriterState::Active,
                    true,
                    Some("30d"),
                    None,
                ),
            ),
            row(
                "a-very-long-handle-name",
                "default",
                summary(0, None, WriterState::Idle, false, None, None),
            ),
        ];
        let data: Vec<[String; 7]> = rows.iter().map(row_cells).collect();
        let table = format_table(&COLUMNS, &data);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 rows
        assert!(lines[0].starts_with("HANDLE"));
        // the HANDLE column widens to fit the longest handle
        assert!(lines[2].starts_with("a-very-long-handle-name"));
    }

    #[test]
    fn json_rows_carry_the_documented_fields() {
        let rows = [row(
            "nginx",
            "default",
            summary(
                2048,
                Some((5, 10)),
                WriterState::Active,
                true,
                Some("30d"),
                Some("50G"),
            ),
        )];
        let v = rows_to_json(&rows);
        let obj = &v[0];
        assert_eq!(obj["handle"], "nginx");
        assert_eq!(obj["forest"], "default");
        assert_eq!(obj["size_bytes"], 2048);
        assert_eq!(obj["from_ms"], 5);
        assert_eq!(obj["to_ms"], 10);
        assert_eq!(obj["writer_live"], true);
        assert_eq!(obj["indexed"], true);
        assert_eq!(obj["retain"], "30d");
        assert_eq!(obj["retain_size"], "50G");
    }

    #[test]
    fn json_span_is_null_for_an_empty_store() {
        let rows = [row(
            "empty",
            "default",
            summary(0, None, WriterState::Idle, false, None, None),
        )];
        let v = rows_to_json(&rows);
        assert!(v[0]["from_ms"].is_null());
        assert!(v[0]["to_ms"].is_null());
        assert!(v[0]["retain"].is_null());
    }
}
