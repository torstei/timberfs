//! Streaming, push-based log-ENTRY processing for the read path: group
//! pushed chunk bytes into entries (a stamped line plus its continuation
//! lines), filter them by the timestamps the lines themselves carry, and
//! frame the output — newline text or NUL-terminated records (-0), with
//! optional write-time annotation. The trunk is its own timestamp index:
//! nothing here is persisted; chunks are selected by the write-time rings
//! and entries are verified against the asked window on the fly.

use std::cell::Cell;
use std::io::{self, Write};
use std::rc::Rc;

use crate::import::Extractor;
use crate::query::fmt_ms;

/// A shared entry cap: (running count across all sinks that share it, max).
/// Once the count reaches max, sinks stop emitting — a total --max across
/// interleaved sources.
pub type EntryLimit = (Rc<Cell<u64>>, u64);

/// A timestamp-less flood can't balloon memory (same cap as grep).
const ENTRY_CAP: usize = 16 << 20;

/// How entries leave the sink.
pub struct Framing {
    /// NUL-terminated records instead of newline text (-0): the entry's
    /// trailing newline is stripped, internal newlines kept.
    pub null_sep: bool,
    /// Typed record stream (--records): each entry is preceded by a
    /// metadata record (RS-marked, US-separated k=v) carrying len, the
    /// entry's own timestamp when it has one, its chunk write window,
    /// and the source label in multi-source streams. Payload bytes are
    /// VERBATIM (trailing newline kept); len is authoritative and the
    /// closing NUL is a resync marker, so even NUL bytes in an entry
    /// are representable. See timberfs-records(5).
    pub records: bool,
    /// Annotate each record with the write time it arrived at and, when
    /// the entry carries its own stamp, the offset between the two — the
    /// invisible second field, made visible.
    pub show_write: bool,
    /// Multi-file "path:" prefix — per line in text mode, once per record
    /// in -0 mode.
    pub label: Option<Vec<u8>>,
}

/// One file's entry stream. The output writer is passed per call so
/// several sinks can interleave through a single stream.
pub struct EntrySink {
    extractor: Extractor,
    /// Logline window to verify entries against; None = framing only.
    window: Option<(u64, u64)>,
    framing: Framing,
    /// Optional total-entries cap (--max), shared across sibling sinks.
    limit: Option<EntryLimit>,
    display: String,

    /// Entries this sink DROPPED because the shared cap was already
    /// reached. Nonzero proves more existed than was emitted — the
    /// difference between "that was all" and "your limit stopped me",
    /// which a count alone cannot tell.
    pub suppressed: u64,
    line: Vec<u8>,
    entry: Vec<u8>,
    entry_ts: Option<u64>,
    entry_write_win: (u64, u64),
    cur_write_win: (u64, u64),
    /// The number of the chunk being pushed, and the one the open entry
    /// started in. `None` for the live edge: those entries are in no chunk
    /// yet, so there is no resumable position to name — a consumer counts
    /// them as delivered but cannot durably stand inside them.
    entry_chunk: Option<u64>,
    cur_chunk: Option<u64>,

    pub emitted: u64,
    pub filtered_out: u64,
    pub stamped: u64,
    offset_sum_ms: i64,
    offset_n: i64,
}

impl EntrySink {
    pub fn new(
        extractor: Extractor,
        window: Option<(u64, u64)>,
        framing: Framing,
        limit: Option<EntryLimit>,
        display: &str,
    ) -> EntrySink {
        EntrySink {
            extractor,
            window,
            framing,
            limit,
            display: display.to_string(),
            suppressed: 0,
            line: Vec::new(),
            entry: Vec::new(),
            entry_ts: None,
            entry_write_win: (0, 0),
            cur_write_win: (0, 0),
            entry_chunk: None,
            cur_chunk: None,
            emitted: 0,
            filtered_out: 0,
            stamped: 0,
            offset_sum_ms: 0,
            offset_n: 0,
        }
    }

    /// Feed one chunk's decompressed bytes with the chunk's write window
    /// (entries are annotated with the chunk they START in — per-chunk
    /// granularity, tight for live data).
    /// `chunk` is the number of the chunk `data` came from, or `None` for
    /// the live edge (see `cur_chunk`).
    pub fn push_chunk(
        &mut self,
        data: &[u8],
        chunk: Option<u64>,
        write_win: (u64, u64),
        out: &mut dyn Write,
    ) -> io::Result<()> {
        self.cur_write_win = write_win;
        self.cur_chunk = chunk;
        let mut start = 0;
        for (i, &b) in data.iter().enumerate() {
            if b == b'\n' {
                self.line.extend_from_slice(&data[start..=i]);
                start = i + 1;
                let line = std::mem::take(&mut self.line);
                self.take_line(line, out)?;
            }
        }
        self.line.extend_from_slice(&data[start..]);
        Ok(())
    }

    fn take_line(&mut self, line: Vec<u8>, out: &mut dyn Write) -> io::Result<()> {
        let head = String::from_utf8_lossy(&line[..line.len().min(256)]);
        match self.extractor.extract(&head) {
            Some(ts) => {
                self.close_entry(out)?;
                self.entry_ts = Some(ts);
                self.entry_write_win = self.cur_write_win;
                self.entry_chunk = self.cur_chunk;
                self.entry = line;
                self.stamped += 1;
                // Divergence = distance OUTSIDE the chunk's write window;
                // a stamp inside it has nothing to report.
                let (wf, wl) = self.cur_write_win;
                let off = if ts < wf {
                    ts as i64 - wf as i64
                } else if ts > wl {
                    ts as i64 - wl as i64
                } else {
                    0
                };
                self.offset_sum_ms += off;
                self.offset_n += 1;
            }
            None => {
                if self.entry.is_empty() {
                    self.entry_write_win = self.cur_write_win;
                    self.entry_chunk = self.cur_chunk;
                }
                if self.entry.len() + line.len() > ENTRY_CAP {
                    self.close_entry(out)?;
                    self.entry_write_win = self.cur_write_win;
                    self.entry_chunk = self.cur_chunk;
                }
                self.entry.extend_from_slice(&line);
            }
        }
        Ok(())
    }

    fn close_entry(&mut self, out: &mut dyn Write) -> io::Result<()> {
        if self.entry.is_empty() {
            return Ok(());
        }
        let keep = match (self.window, self.entry_ts) {
            (Some((from, to)), Some(ts)) => ts >= from && ts <= to,
            // No stamp on the entry: include — never hide what we cannot
            // place in time (the read-side "missing means scan").
            _ => true,
        };
        let entry = std::mem::take(&mut self.entry);
        let ts = self.entry_ts.take();
        if !keep {
            self.filtered_out += 1;
            return Ok(());
        }
        // --max: once the shared count hits the cap, drop silently (the
        // read loop stops feeding chunks, but a chunk in flight can still
        // hold entries past the limit).
        if let Some((count, max)) = &self.limit {
            if count.get() >= *max {
                self.suppressed += 1;
                return Ok(());
            }
            count.set(count.get() + 1);
        }
        self.emitted += 1;

        let annotation = if self.framing.show_write {
            let (wf, wl) = self.entry_write_win;
            // The diff is only shown when the entry's own stamp falls
            // OUTSIDE the write window it arrived in — inside it, write
            // time and logline time agree to chunk precision.
            let diff = match ts {
                Some(t) if t < wf || t > wl => {
                    let d = if t < wf {
                        t as i64 - wf as i64
                    } else {
                        t as i64 - wl as i64
                    };
                    let (sign, d) = if d < 0 { ("-", -d) } else { ("+", d) };
                    let (s, ms) = (d / 1000, d % 1000);
                    if s >= 3600 {
                        format!(" {sign}{}h{:02}m", s / 3600, (s % 3600) / 60)
                    } else if s >= 60 {
                        format!(" {sign}{}m{:02}s", s / 60, s % 60)
                    } else {
                        format!(" {sign}{s}.{ms:03}s")
                    }
                }
                _ => String::new(),
            };
            Some(format!("[w {}{}] ", fmt_ms(wf), diff))
        } else {
            None
        };

        if self.framing.records {
            out.write_all(b"\x1eentry")?;
            write!(out, "\x1flen={}", entry.len())?;
            if let Some(t) = ts {
                write!(out, "\x1fts={t}")?;
            }
            let (wf, wl) = self.entry_write_win;
            write!(out, "\x1fwf={wf}\x1fwl={wl}")?;
            // Present only for an entry that is already in a chunk: its
            // ABSENCE is how a consumer knows this one came from the live
            // edge and cannot be resumed from yet.
            if let Some(seq) = self.entry_chunk {
                write!(out, "\x1fchunk={seq}")?;
            }
            if let Some(label) = &self.framing.label {
                out.write_all(b"\x1fsrc=")?;
                out.write_all(label)?;
            }
            out.write_all(b"\0")?;
            out.write_all(&entry)?;
            out.write_all(b"\0")?;
            return Ok(());
        }
        if self.framing.null_sep {
            if let Some(label) = &self.framing.label {
                out.write_all(label)?;
                out.write_all(b":")?;
            }
            if let Some(a) = &annotation {
                out.write_all(a.as_bytes())?;
            }
            let body = entry.strip_suffix(b"\n").unwrap_or(&entry);
            out.write_all(body)?;
            out.write_all(b"\0")?;
        } else {
            for (i, line) in entry.split_inclusive(|&b| b == b'\n').enumerate() {
                if let Some(label) = &self.framing.label {
                    out.write_all(label)?;
                    out.write_all(b":")?;
                }
                if i == 0 {
                    if let Some(a) = &annotation {
                        out.write_all(a.as_bytes())?;
                    }
                }
                out.write_all(line)?;
            }
        }
        Ok(())
    }

    /// Close the entry held open waiting for the next stamped line — the
    /// live-stream escape from "the newest entry is never the one you
    /// see". Only for a source that has been idle longer than any writer
    /// could hold a flush: a producer still writing an entry would have
    /// committed the rest of it by then. The partial LINE (bytes after
    /// the last newline) stays buffered; it is genuinely incomplete.
    pub fn flush_pending(&mut self, out: &mut dyn Write) -> io::Result<()> {
        self.close_entry(out)
    }

    /// Flush pending state; call once after the last push.
    pub fn finish(&mut self, out: &mut dyn Write) -> io::Result<()> {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.take_line(line, out)?;
        }
        self.close_entry(out)?;

        // The timezone tripwire: on one host the clocks cancel, so a
        // persistent offset near a whole number of hours is a parsing or
        // timezone misconfiguration, not clock skew.
        if self.offset_n >= 20 {
            let avg = self.offset_sum_ms / self.offset_n;
            let hours = (avg as f64 / 3_600_000.0).round() as i64;
            if hours != 0 && (avg - hours * 3_600_000).abs() < 5 * 60_000 {
                crate::note!(
                    "timberfs: {}: line timestamps run ~{}h {} the write clock — timezone \
                     mismatch? (declare timestamp_utc with `timberfs set`)",
                    self.display,
                    hours.abs(),
                    if hours > 0 { "ahead of" } else { "behind" }
                );
            }
        }
        Ok(())
    }
}

/// Probe one decompressed chunk: do any of its first lines carry a
/// parseable timestamp? Decides whether the read path can filter (and
/// therefore widen the selection) or must fall back to the raw
/// write-time window.
pub fn probe_stamps(extractor: &Extractor, data: &[u8]) -> bool {
    let mut checked = 0;
    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let head = String::from_utf8_lossy(&line[..line.len().min(256)]);
        if extractor.extract(&head).is_some() {
            return true;
        }
        checked += 1;
        if checked >= 1000 {
            break;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records_sink() -> EntrySink {
        EntrySink::new(
            crate::import::Extractor::new(None, None, false).unwrap(),
            None,
            Framing {
                null_sep: false,
                records: true,
                show_write: false,
                label: None,
            },
            None,
            "test",
        )
    }

    /// Field heads only, one per emitted entry record.
    fn heads(buf: &[u8]) -> Vec<String> {
        buf.split(|&b| b == 0x1e)
            .filter(|r| r.starts_with(b"entry"))
            .map(|r| {
                let head = r.split(|&b| b == 0).next().unwrap_or_default();
                String::from_utf8_lossy(head).replace('\x1f', " ")
            })
            .collect()
    }

    #[test]
    fn an_entry_from_a_chunk_carries_its_number() {
        let mut sink = records_sink();
        let mut out = Vec::new();
        sink.push_chunk(
            b"2026-08-21T10:00:00Z first\n2026-08-21T10:00:01Z second\n",
            Some(41),
            (100, 200),
            &mut out,
        )
        .unwrap();
        sink.finish(&mut out).unwrap();
        let h = heads(&out);
        assert_eq!(h.len(), 2, "{h:?}");
        assert!(h[0].contains("chunk=41"), "{}", h[0]);
        assert!(h[1].contains("chunk=41"), "{}", h[1]);
    }

    #[test]
    fn a_live_edge_entry_carries_no_chunk_at_all() {
        // Absence is the signal: the entry is in no chunk yet, so there is
        // no position a consumer could durably resume from inside it. A
        // zero would be a lie — chunk 0 is a real chunk.
        let mut sink = records_sink();
        let mut out = Vec::new();
        sink.push_chunk(b"2026-08-21T10:00:00Z live\n", None, (100, 200), &mut out)
            .unwrap();
        sink.finish(&mut out).unwrap();
        let h = heads(&out);
        assert_eq!(h.len(), 1, "{h:?}");
        assert!(!h[0].contains("chunk="), "{}", h[0]);
        // The write window is still there — that is a fact about the entry,
        // unlike its position.
        assert!(h[0].contains("wf=100"), "{}", h[0]);
    }

    #[test]
    fn a_split_entry_is_attributed_where_it_completed_like_its_window() {
        // An entry whose line spans two chunks is attributed to the chunk
        // it COMPLETED in, because that is already what `wf`/`wl` report —
        // an entry becomes an entry when its stamped line closes. The
        // number must AGREE with the window rather than be cleverer than
        // it, or a consumer reading both gets two contradictory positions.
        //
        // The consequence is pre-existing and bounded: resuming there
        // re-reads that chunk from its start, without the leading bytes
        // that live in the previous one. Chunk-granular resume already
        // re-delivers the boundary chunk, which is why at-least-once is
        // the contract.
        let mut sink = records_sink();
        let mut out = Vec::new();
        sink.push_chunk(
            b"2026-08-21T10:00:00Z split ",
            Some(7),
            (100, 200),
            &mut out,
        )
        .unwrap();
        sink.push_chunk(b"continues here\n", Some(8), (200, 300), &mut out)
            .unwrap();
        sink.finish(&mut out).unwrap();
        let h = heads(&out);
        assert_eq!(h.len(), 1, "{h:?}");
        assert!(h[0].contains("chunk=8"), "{}", h[0]);
        assert!(
            h[0].contains("wf=200"),
            "the number agrees with the window: {}",
            h[0]
        );
    }
}
