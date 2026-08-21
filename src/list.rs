//! `timberfs list`: the directory-level complement to `info` — what stores
//! exist, and their vital signs, across the configured forests (or a given
//! set of directories). Read-only and lock-free, like `info`: it uses the
//! same `StoreSummary` (see query.rs), so the two commands never disagree
//! about a store's size, span, writer state, index or retention.
//!
//! Unlike handle resolution, which refuses an ambiguous handle, `list` is
//! how a user SEES the ambiguity: the same handle in two forests shows up
//! as two rows, never deduped or merged.

use std::collections::HashMap;
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

    // Once per listing, not once per store: every scan of the registry
    // reads each follower's rings to place its position.
    let mut registry = crate::follower::by_store(&crate::follower::registry_dir());
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
            match open_summary(&path, &mut registry) {
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
fn open_summary(
    logical: &Path,
    registry: &mut HashMap<String, Vec<crate::follower::Registered>>,
) -> anyhow::Result<(PathBuf, StoreSummary)> {
    let (dir, name) = crate::query::resolve_backing(logical)?;
    let rings = crate::format::rings_path(&dir, &name);
    let records = crate::format::read_index(&rings)
        .with_context(|| format!("reading index {}", rings.display()))?;
    let bark = crate::bark::load(&dir, &name);
    // Taken, not cloned: a store is summarised once per listing, and its
    // followers belong to nobody else.
    let anchor = crate::cursor::store_anchor(&dir, &name, bark.as_ref());
    let followers = registry.remove(&anchor).unwrap_or_default();
    let summary = crate::query::summarize_store(&dir, &name, &records, bark.as_ref(), followers);
    Ok((dir, summary))
}

/// The RETAIN column: the declared policy, or `-` when none is declared.
fn retain_text(s: &StoreSummary) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(r) = &s.retain {
        parts.push(r.clone());
    }
    if let Some(r) = &s.retain_size {
        parts.push(r.clone());
    }
    // The third axis is a flag, not a quantity, so the column names it
    // rather than measuring it: how much it holds is a per-follower fact
    // and lives in the FOLLOWERS column beside it.
    if s.retain_unconsumed {
        parts.push("unconsumed".to_string());
    }
    if parts.is_empty() {
        return "-".to_string();
    }
    parts.join(", ")
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

/// The FOLLOWERS column: how many readers hold a position in this store,
/// and the worst of them — the one that decides how much of the store is
/// unread. `0` where something is declared and nothing is reading, which
/// is a real state and a dangerous-looking one; `-` only where there is
/// nothing to say at all.
fn followers_text(s: &StoreSummary) -> String {
    if !s.has_readers() {
        return "-".to_string();
    }
    match (s.reader_count(), s.worst_lag()) {
        (0, _) | (_, None) => "0".to_string(),
        (n, Some(lag)) => format!("{n}, {lag}"),
    }
}

/// Most stores have no followers, so the column appears when at least
/// one row fills it rather than taxing every table with a column of
/// dashes.
fn columns(rows: &[Row]) -> Vec<&'static str> {
    let mut cols = COLUMNS.to_vec();
    if rows.iter().any(|r| r.summary.has_readers()) {
        cols.push("FOLLOWERS");
    }
    cols
}

/// One row's cells, in `columns` order — a pure function of a `Row`, so
/// it (and the table it feeds) is unit-testable without touching disk.
fn row_cells(r: &Row, with_followers: bool) -> Vec<String> {
    let mut cells = vec![
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
    ];
    if with_followers {
        cells.push(followers_text(&r.summary));
    }
    cells
}

/// Render an aligned table: a header plus one row per store, columns
/// left-aligned and sized to the widest cell (handles/forest names have no
/// fixed width, unlike `info`'s fixed-width tables).
pub(crate) fn format_table(header: &[&str], rows: &[Vec<String>]) -> String {
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
    let cols = columns(rows);
    let with_followers = cols.len() > COLUMNS.len();
    let data: Vec<Vec<String>> = rows.iter().map(|r| row_cells(r, with_followers)).collect();
    println!("{}", format_table(&cols, &data));
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
                // Always an array, never null: the registry knows every
                // follower of every store, so empty means empty.
                o.insert(
                    "followers".to_string(),
                    serde_json::Value::Array(
                        s.followers.iter().map(crate::follower::to_json).collect(),
                    ),
                );
                match &s.consumers {
                    // Null distinguishes "no cursor directory declared"
                    // from an empty array: "declared, nobody reading".
                    // Superseded by `followers`, still reported while the
                    // key is honoured.
                    None => {
                        o.insert("cursors_dir".to_string(), serde_json::Value::Null);
                        o.insert("consumers".to_string(), serde_json::Value::Null);
                    }
                    Some(sv) => {
                        o.insert(
                            "cursors_dir".to_string(),
                            sv.dir.display().to_string().into(),
                        );
                        o.insert("consumers".to_string(), crate::query::consumers_json(sv));
                        o.insert("held_bytes".to_string(), sv.held_bytes().into());
                    }
                }
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
            wal_declared: false,
            sap_pending_bytes: None,
            retain: retain.map(str::to_string),
            retain_size: retain_size.map(str::to_string),
            retain_unconsumed: false,
            followers: Vec::new(),
            consumers: None,
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

    fn survey(consumers: Vec<(&str, u64, u64, Option<u64>)>) -> crate::cursor::Survey {
        crate::cursor::Survey {
            dir: PathBuf::from("/var/lib/timberfs"),
            consumers: consumers
                .into_iter()
                .map(
                    |(name, behind_bytes, behind_ms, gap_chunks)| crate::cursor::Consumer {
                        name: name.to_string(),
                        path: PathBuf::from(format!("/var/lib/timberfs/{name}.cursor")),
                        cursor: crate::cursor::Cursor::new(name, "id", "/p"),
                        standing: crate::cursor::Standing {
                            consumed_chunks: 1,
                            behind_chunks: if behind_bytes > 0 { 1 } else { 0 },
                            behind_bytes,
                            behind_ms,
                            gap_chunks,
                        },
                    },
                )
                .collect(),
            unreadable: 0,
        }
    }

    #[test]
    fn followers_text_separates_nothing_to_say_from_nobody_reading() {
        let mut s = summary(0, None, WriterState::Idle, false, None, None);
        // Nothing registered and nothing declared: no claim on the column.
        assert_eq!(followers_text(&s), "-");
        // A declared-but-empty cursors directory is a real state, and the
        // dangerous one — nothing holds anything back.
        s.consumers = Some(survey(vec![]));
        assert_eq!(followers_text(&s), "0");
        s.consumers = Some(survey(vec![("otlp", 0, 0, None)]));
        assert_eq!(followers_text(&s), "1, caught up");
        s.consumers = Some(survey(vec![
            ("splitter", 4096, 90_000, None),
            ("otlp", 0, 0, None),
        ]));
        assert_eq!(followers_text(&s), "2, 1m 30s behind");
        s.consumers = Some(survey(vec![("splitter", 4096, 90_000, Some(9))]));
        assert_eq!(followers_text(&s), "1, GAP");
    }

    #[test]
    fn the_followers_column_appears_only_where_something_fills_it() {
        let plain = [row(
            "nginx",
            "default",
            summary(2048, Some((0, 1000)), WriterState::Idle, false, None, None),
        )];
        assert_eq!(columns(&plain), COLUMNS.to_vec());

        let mut declared = summary(2048, Some((0, 1000)), WriterState::Idle, false, None, None);
        declared.consumers = Some(survey(vec![("otlp", 0, 0, None)]));
        let rows = [
            row("nginx", "default", declared),
            row(
                "db",
                "default",
                summary(0, None, WriterState::Idle, false, None, None),
            ),
        ];
        let cols = columns(&rows);
        assert_eq!(cols.last(), Some(&"FOLLOWERS"));
        let data: Vec<Vec<String>> = rows.iter().map(|r| row_cells(r, true)).collect();
        assert_eq!(data[0][7], "1, caught up");
        // A store with nothing reading it keeps a dash in the shared
        // column.
        assert_eq!(data[1][7], "-");
        let table = format_table(&cols, &data);
        assert!(table.lines().next().unwrap().contains("FOLLOWERS"));
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
            summary(
                2048,
                Some((0, 1000)),
                WriterState::Active(None),
                true,
                None,
                None,
            ),
        );
        let cells = row_cells(&live, false);
        assert_eq!(cells[0], "nginx");
        assert_eq!(cells[1], "default");
        assert_eq!(cells[4], "live");
        assert_eq!(cells[5], "grain");

        let idle = row(
            "db",
            "default",
            summary(0, None, WriterState::Idle, false, None, None),
        );
        let cells = row_cells(&idle, false);
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
                    WriterState::Active(None),
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
        let data: Vec<Vec<String>> = rows.iter().map(|r| row_cells(r, false)).collect();
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
                WriterState::Active(None),
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
