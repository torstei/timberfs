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
pub fn cmd_list(
    dirs: &[PathBuf],
    names_only: bool,
    json: bool,
    select: Option<&str>,
    full_id: bool,
) -> anyhow::Result<()> {
    let Some(rows) = scan(dirs, select)? else {
        return Ok(());
    };
    if names_only {
        for r in &rows {
            println!("{}", display_name(r));
        }
        return Ok(());
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&rows_to_json(&rows))?);
        return Ok(());
    }
    print_table(&rows, full_id);
    Ok(())
}

/// The stores a selector picks, for an ANSWER to carry.
///
/// The same scan and the same store objects as `list --json`. What the
/// answer wraps them in is the answer's business, not this function's —
/// a CLI listing is a listing, an answer says what produced it.
pub fn stores_json(dirs: &[PathBuf], select: Option<&str>) -> anyhow::Result<serde_json::Value> {
    Ok(match scan(dirs, select)? {
        Some(rows) => rows_to_json(&rows),
        None => serde_json::Value::Array(Vec::new()),
    })
}

fn scan(dirs: &[PathBuf], select: Option<&str>) -> anyhow::Result<Option<Vec<Row>>> {
    // Parse before scanning: a malformed predicate is a usage error, and
    // reporting it after a full forest walk would read as "matched
    // nothing".
    let selector = match select {
        Some(expr) => crate::select::Selector::parse(expr)?,
        None => crate::select::Selector::all(),
    };
    let forests = crate::forest::forests_for_list(dirs);
    if dirs.is_empty() && forests.is_empty() {
        crate::note!("timberfs: no forests configured (see /etc/timberfs/forests.d/)");
        return Ok(None);
    }

    // Once per listing, not once per store: every scan of the registry
    // reads each follower's rings to place its position. Matched per
    // store rather than looked up, a selection not being groupable —
    // which is also why it is no longer taken from the map.
    let registry = crate::follower::all(&crate::follower::registry_dir());
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
            match open_summary(&path, &registry) {
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
    // By what the store is CALLED, not by its directory — an opaque path
    // sorts as a uuid, which shuffles the listing every time a store is
    // created. Identity breaks the tie, because declared names are not
    // unique and a listing must still have one stable order.
    rows.sort_by(|a, b| {
        (
            a.forest.as_str(),
            display_name(a),
            a.summary.id.clone().unwrap_or_default(),
        )
            .cmp(&(
                b.forest.as_str(),
                display_name(b),
                b.summary.id.clone().unwrap_or_default(),
            ))
    });

    // A selection owes coverage: an empty result has to say how much was
    // searched, or "matched nothing" reads as "nothing was there". The
    // predicate is applied through the same `select::resolve` a writer
    // uses to pick a store, so the two can never disagree about what a
    // selector means.
    if !selector.is_all() {
        let examined = rows.len();
        let matched: std::collections::HashSet<(PathBuf, String)> =
            crate::select::resolve(dirs, &selector)
                .into_iter()
                .map(|m| (m.dir, m.name))
                .collect();
        rows.retain(|r| {
            crate::query::resolve_backing(&r.path)
                .ok()
                .is_some_and(|(d, n)| matched.contains(&(d, n)))
        });
        if rows.is_empty() {
            crate::note!(
                "timberfs: no store matches `{}` ({examined} store(s) in {} forest(s) examined)",
                select.unwrap_or("*"),
                forests.len()
            );
        }
    }

    Ok(Some(rows))
}

/// Read a store's index and manifest directly (no trunk file needed — list
/// never reads data), and summarize it.
fn open_summary(
    logical: &Path,
    registry: &[crate::follower::Registered],
) -> anyhow::Result<(PathBuf, StoreSummary)> {
    let (dir, name) = crate::query::resolve_backing(logical)?;
    let rings = crate::format::rings_path(&dir, &name);
    let records = crate::format::read_index(&rings)
        .with_context(|| format!("reading index {}", rings.display()))?;
    let bark = crate::bark::load(&dir, &name);
    // One follower can cover many stores, so this is a match against
    // every declaration rather than a lookup that removes one.
    let fields = crate::follower::subject_of(&dir, &name);
    let followers = crate::follower::covering(registry, &fields);
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

const COLUMNS: [&str; 8] = [
    "ID", "NAME", "FOREST", "SIZE", "SPAN", "WRITER", "INDEX", "RETAIN",
];

/// How much of an `id` the table prints. A UUID's first group is exactly
/// this long, so the short form ends on a boundary rather than mid-field —
/// and `info` takes it back as a prefix, so what is printed is typeable.
const SHORT_ID: usize = 8;

/// Which CONTINGENT columns this listing has anything to put in. Each
/// appears only when at least one row fills it, rather than taxing every
/// table with a column of dashes.
///
/// `ID` is deliberately not among them. A store having no reader is
/// ordinary; a store having no identity is not — it is what a store IS,
/// so the column is structural and a table of dashes there is a finding
/// rather than noise.
#[derive(Clone, Copy, Debug)]
struct Optional {
    followers: bool,
    labels: bool,
    full_id: bool,
}

/// A store's labels as `k=v, k=v`, sorted so two runs of `list` order them
/// the same way. Rendered through the same `stringify` a selector matches
/// with, so what you read is what `--select` compares.
fn labels_text(s: &StoreSummary) -> String {
    if s.labels.is_empty() {
        return "-".to_string();
    }
    s.labels
        .iter()
        .map(|(k, v)| format!("{k}={}", crate::select::stringify(v)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a store is called: the name it declares, else the one its path
/// gives it. One column for both, because it answers the same question
/// either way — and because an opaque path would otherwise leave the
/// column repeating the id.
fn display_name(r: &Row) -> String {
    r.summary
        .declared_name
        .clone()
        .unwrap_or_else(|| r.handle.clone())
}

/// The id column: short by default, whole with `--full-id`. `-` where the
/// store has no manifest and so declares no identity — a real state, not
/// an error.
fn id_text(s: &StoreSummary, full: bool) -> String {
    match &s.id {
        None => "-".to_string(),
        Some(id) if full => id.clone(),
        Some(id) => id.chars().take(SHORT_ID).collect(),
    }
}

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
fn columns(rows: &[Row]) -> (Vec<&'static str>, Optional) {
    let opt = Optional {
        followers: rows.iter().any(|r| r.summary.has_readers()),
        labels: rows.iter().any(|r| !r.summary.labels.is_empty()),
        full_id: false,
    };
    // Identity leads, then the name, then what the store is doing. The
    // contingent columns are appended; labels go last, being the widest
    // and the only column with no fixed shape.
    let mut cols: Vec<&'static str> = COLUMNS.to_vec();
    if opt.followers {
        cols.push("FOLLOWERS");
    }
    if opt.labels {
        cols.push("LABELS");
    }
    (cols, opt)
}

/// One row's cells, in `columns` order — a pure function of a `Row`, so
/// it (and the table it feeds) is unit-testable without touching disk.
fn row_cells(r: &Row, opt: Optional) -> Vec<String> {
    let mut cells = Vec::from([
        id_text(&r.summary, opt.full_id),
        display_name(r),
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
    ]);
    if opt.followers {
        cells.push(followers_text(&r.summary));
    }
    if opt.labels {
        cells.push(labels_text(&r.summary));
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

fn print_table(rows: &[Row], full_id: bool) {
    let (cols, mut opt) = columns(rows);
    opt.full_id = full_id;
    let data: Vec<Vec<String>> = rows.iter().map(|r| row_cells(r, opt)).collect();
    println!("{}", format_table(&cols, &data));
}

/// The one store shape, one row per store. `info --json` writes the same
/// object for a single store — see `store_json` for why there is no
/// per-surface projection.
fn rows_to_json(rows: &[Row]) -> serde_json::Value {
    serde_json::Value::Array(
        rows.iter()
            .map(|r| {
                let loc = crate::store_json::Location {
                    forest: Some(r.forest.clone()),
                    handle: r.handle.clone(),
                    dir: r.dir.display().to_string(),
                    path: r.path.display().to_string(),
                    kind: crate::store_json::Kind::Pair,
                    bundle_bytes: None,
                };
                serde_json::to_value(crate::store_json::Store::new(&r.summary, &loc))
                    .unwrap_or(serde_json::Value::Null)
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
            chunk_seq: if span.is_some() { Some((0, 0)) } else { None },
            next_seq: if span.is_some() { 1 } else { 0 },
            dropped: crate::format::Dropped::default(),
            id: None,
            created: None,
            declared_name: None,
            origin_id: None,
            labels: serde_json::Map::new(),
            grain: if indexed { Some((10, 1)) } else { None },
            index_declared: false,
            wal_declared: false,
            sap_pending_bytes: None,
            retain: retain.map(str::to_string),
            retain_size: retain_size.map(str::to_string),
            retain_unconsumed: false,
            derived_from: None,
            derived_op: None,
            window_from: None,
            window_to: None,
            command: None,
            pattern: None,
            followers: Vec::new(),
            consumers: None,
            writer,
        }
    }

    /// The optional-column set `columns()` would compute for these rows,
    /// so a test never pairs cells with a header it would not get.
    fn opts(rows: &[Row]) -> Optional {
        columns(rows).1
    }

    impl Row {
        /// A copy with an independent summary, so a test can vary one
        /// field without a full Clone on StoreSummary.
        fn clone_for_name_test(&self) -> Row {
            let mut s = summary(1, Some((1, 2)), WriterState::Idle, false, None, None);
            s.labels = self.summary.labels.clone();
            s.id = self.summary.id.clone();
            row(&self.handle, &self.forest, s)
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
        assert_eq!(columns(&plain).0, COLUMNS.to_vec());

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
        let (cols, _) = columns(&rows);
        assert_eq!(cols.last(), Some(&"FOLLOWERS"));
        let o = opts(&rows);
        let data: Vec<Vec<String>> = rows.iter().map(|r| row_cells(r, o)).collect();
        assert_eq!(data[0][8], "1, caught up");
        // A store with nothing reading it keeps a dash in the shared
        // column.
        assert_eq!(data[1][8], "-");
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
    fn the_id_column_is_a_prefix_info_accepts_and_full_id_is_the_whole_thing() {
        let mut s = summary(1, Some((1, 2)), WriterState::Idle, false, None, None);
        s.id = Some("5e86897c-4d6a-4c7e-a757-8cf846838bad".to_string());
        // A UUID's first group is exactly SHORT_ID long, so the short form
        // ends on a boundary instead of mid-field.
        assert_eq!(id_text(&s, false), "5e86897c");
        assert_eq!(id_text(&s, true), "5e86897c-4d6a-4c7e-a757-8cf846838bad");
        // No manifest, no identity — a real state, and not an error.
        s.id = None;
        assert_eq!(id_text(&s, false), "-");
    }

    #[test]
    fn optional_columns_appear_only_when_something_fills_them() {
        // The rule FOLLOWERS already set: a table is not taxed with a
        // column of dashes for a fact none of its rows has.
        let bare = vec![row(
            "bare",
            "default",
            summary(1, Some((1, 2)), WriterState::Idle, false, None, None),
        )];
        let (cols, o) = columns(&bare);
        assert!(!o.labels && !o.followers);
        // Identity is structural: present even for a store that declares
        // none, where it reads as a dash.
        assert_eq!(cols.first(), Some(&"ID"));
        assert_eq!(row_cells(&bare[0], o)[0], "-");
        assert_eq!(cols, COLUMNS.to_vec());
        assert_eq!(row_cells(&bare[0], o).len(), cols.len());

        let mut s = summary(1, Some((1, 2)), WriterState::Idle, false, None, None);
        s.id = Some("abcdef01-0000-4000-8000-000000000000".to_string());
        s.labels.insert("type".into(), "console".into());
        let rich = vec![row("rich", "default", s)];
        let (cols, o) = columns(&rich);
        assert!(o.labels);
        assert_eq!(cols.first(), Some(&"ID"), "identity leads");
        assert_eq!(cols[1], "NAME");
        assert_eq!(cols.last(), Some(&"LABELS"), "the widest column trails");
        // NAME is what the store is CALLED: what it declares, else what
        // its path gives it. An opaque path would otherwise leave the
        // column repeating the id.
        assert_eq!(row_cells(&rich[0], o)[1], "rich", "falls back to the path");
        let mut declared = rich[0].clone_for_name_test();
        declared.summary.declared_name = Some("web01-console".to_string());
        assert_eq!(row_cells(&declared, o)[1], "web01-console");
        // Header and cells must stay in step, which is the bug a bool
        // per optional column invites.
        assert_eq!(row_cells(&rich[0], o).len(), cols.len());
    }

    #[test]
    fn labels_render_sorted_and_the_way_a_selector_compares_them() {
        let mut s = summary(1, Some((1, 2)), WriterState::Idle, false, None, None);
        assert_eq!(labels_text(&s), "-");
        s.labels.insert("type".into(), "console".into());
        s.labels.insert("host".into(), "web01".into());
        // Sorted, so two runs of `list` order them identically...
        assert_eq!(labels_text(&s), "host=web01, type=console");
        // ...and a non-string label reads as the text `--select` matches.
        s.labels.insert("replicas".into(), 3.into());
        assert!(labels_text(&s).contains("replicas=3"));
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
        let cells = row_cells(&live, opts(std::slice::from_ref(&live)));
        assert_eq!(cells[0], "-", "no manifest, no identity");
        assert_eq!(cells[1], "nginx");
        assert_eq!(cells[2], "default");
        assert_eq!(cells[5], "live");
        assert_eq!(cells[6], "grain");

        let idle = row(
            "db",
            "default",
            summary(0, None, WriterState::Idle, false, None, None),
        );
        let cells = row_cells(&idle, opts(std::slice::from_ref(&idle)));
        assert_eq!(cells[4], "empty");
        assert_eq!(cells[5], "-");
        assert_eq!(cells[6], "-");
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
        let o = opts(&rows);
        let data: Vec<Vec<String>> = rows.iter().map(|r| row_cells(r, o)).collect();
        let table = format_table(&COLUMNS, &data);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 rows
        assert!(lines[0].starts_with("ID"));
        // the HANDLE column widens to fit the longest handle
        assert!(lines[2].contains("a-very-long-handle-name"));
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
        assert_eq!(obj["compressed_bytes"], 2048);
        assert_eq!(obj["first_write_ms"], 5);
        assert_eq!(obj["last_write_ms"], 10);
        // The split the old single `indexed` boolean could not express:
        // this store HOLDS a token index covering one chunk while having
        // DECLARED none. A consumer that has to tell "has an index" from
        // "promised one" now can.
        assert_eq!(obj["index_declared"], false);
        assert_eq!(obj["grain_chunks"], 1);
        assert_eq!(obj["grain_bytes"], 10);
        assert_eq!(obj["retain"], "30d");
        assert_eq!(obj["retain_size"], "50G");
        // A writer is NAMED, and its presence is liveness — there is no
        // separate boolean that could come to disagree with it.
        assert_eq!(obj["writer"], "active");

        // The names this shape replaced. They were `info`'s or `list`'s but
        // never both, and the same data under two names is what made the
        // two surfaces unjoinable.
        for gone in [
            "size_bytes",
            "from_ms",
            "to_ms",
            "writer_live",
            "indexed",
            "provenance",
        ] {
            assert!(obj.get(gone).is_none(), "`{gone}` came back");
        }
    }

    #[test]
    fn an_absent_value_is_an_absent_key_not_a_null() {
        // One rule for the whole shape: a consumer tests for the key, and
        // the schema marks it not-required. `list` used to write nulls
        // where `info` omitted, which is the same disagreement in a
        // different costume.
        let rows = [row(
            "empty",
            "default",
            summary(0, None, WriterState::Idle, false, None, None),
        )];
        let v = rows_to_json(&rows);
        let obj = v[0].as_object().unwrap();
        for absent in [
            "first_write_ms",
            "last_write_ms",
            "retain",
            "retain_size",
            "writer",
        ] {
            assert!(
                !obj.contains_key(absent),
                "`{absent}` should be absent, not null"
            );
        }
        // ...and what is always true of a store is always present, so a
        // consumer never has to tell "zero" from "not reported".
        for present in [
            "chunks",
            "next_seq",
            "index_declared",
            "wal_declared",
            "followers",
        ] {
            assert!(
                obj.contains_key(present),
                "`{present}` should always be there"
            );
        }
    }
}
