//! Streaming, push-based log-ENTRY processing for the read path: group
//! pushed chunk bytes into entries (a stamped line plus its continuation
//! lines), filter them by the timestamps the lines themselves carry, and
//! frame the output — newline text or NUL-terminated records (-0), with
//! optional write-time annotation. The trunk is its own timestamp index:
//! nothing here is persisted; chunks are selected by the write-time rings
//! and entries are verified against the asked window on the fly.

use std::io::{self, Write};

use crate::import::Extractor;
use crate::query::fmt_ms;

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
    display: String,

    line: Vec<u8>,
    entry: Vec<u8>,
    entry_ts: Option<u64>,
    entry_write_win: (u64, u64),
    cur_write_win: (u64, u64),

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
        display: &str,
    ) -> EntrySink {
        EntrySink {
            extractor,
            window,
            framing,
            display: display.to_string(),
            line: Vec::new(),
            entry: Vec::new(),
            entry_ts: None,
            entry_write_win: (0, 0),
            cur_write_win: (0, 0),
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
    pub fn push_chunk(
        &mut self,
        data: &[u8],
        write_win: (u64, u64),
        out: &mut dyn Write,
    ) -> io::Result<()> {
        self.cur_write_win = write_win;
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
                }
                if self.entry.len() + line.len() > ENTRY_CAP {
                    self.close_entry(out)?;
                    self.entry_write_win = self.cur_write_win;
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
