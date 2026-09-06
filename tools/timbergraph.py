"""Graphs of a tally store: what the numbers did, rather than what they are.

A tally line is text (`timberfs.1`, tally), so this reads one from
anywhere — a pipe, a file, `timberfs query` — and needs no timberfs
change to exist.

Most of what a plot must know is already IN the line, because the
coarsening invariant put it there: the five measure names ARE the
aggregation rules, so a line says how to combine itself, and an `le`
label says a series is cumulative. What a line cannot say is the UNIT and
which metrics exist but were silent, so an extractor document may be
named to supply those.

⚠ The markers are the graph's error bars. `!drop` says the numbers in
that bucket are WRONG, `!cap` that they are understated, `!late` that
they hold entries displaced from another minute. A plot that hid them
would be lying about being exact, which is the one thing this format has
refused throughout.
"""
import json
import os
import shutil
import subprocess
import sys
import time

VERSION = "1.0-EXPERIMENTAL"

#: Markers that say something WENT WRONG, as against `!meta`, which
#: says what a number is. Only these are drawn and warned about.
TROUBLE = ("!drop", "!cap", "!late", "!gap")

#: The five measures, and how each one combines. The name IS the rule —
#: which is what lets a plot aggregate a series it was not told about.
COMBINE = {
    "count": "add",
    "sum": "add",
    "min": "min",
    "max": "max",
    # ⚠ Not combinable across series: the newest wins, and "newest" is
    # not a thing a group of series has. Plotted per series or not at all.
    "last": "newest",
}

MARKER = "!"


class Bad(Exception):
    """Something the input said that cannot be drawn."""


# ------------------------------------------------------------ the line

def tokenize(line):
    """Whitespace, except inside double quotes — the same splitting the
    writer's own renderer does, so a quoted label survives the round
    trip."""
    out, cur, quoted, started = [], [], False, False
    it = iter(line)
    for c in it:
        if c == '"':
            quoted, started = not quoted, True
        elif c == "\\" and quoted:
            nxt = next(it, "")
            cur.append({"n": "\n", "t": "\t"}.get(nxt, nxt))
        elif c.isspace() and not quoted:
            if started:
                out.append("".join(cur))
                cur, started = [], False
        else:
            cur.append(c)
            started = True
    if started:
        out.append("".join(cur))
    return out


def parse_stamp(text):
    """RFC3339 with milliseconds, as the writer renders it."""
    t = text.replace("Z", "+00:00")
    try:
        import datetime
        return int(datetime.datetime.fromisoformat(t).timestamp() * 1000)
    except ValueError as e:
        raise Bad(f"{text!r} is not an RFC3339 timestamp") from e


class Sample:
    __slots__ = ("ts", "width_ms", "metric", "labels", "fields", "cite")

    def __init__(self, ts, width_ms, metric, labels, fields, cite):
        self.ts, self.width_ms, self.metric = ts, width_ms, metric
        self.labels, self.fields, self.cite = labels, fields, cite

    @property
    def is_marker(self):
        return self.metric.startswith(MARKER)

    @property
    def key(self):
        """What makes two lines the same bucket said twice."""
        return (self.ts, self.width_ms, self.metric,
                tuple(sorted(self.labels.items())))

    def series_key(self, by):
        return tuple(self.labels.get(k, "") for k in by)


def parse_line(line):
    """One tally line, or None for a blank. Raises on a line that is not
    one — a plot that silently skipped a tenth of its input would be
    drawing a picture of nothing in particular."""
    toks = tokenize(line)
    if not toks:
        return None
    if len(toks) < 3:
        raise Bad(f"a tally line is `<ts> <width> <metric> ...` — got {line!r}")
    ts = parse_stamp(toks[0])
    if not toks[1].endswith("s"):
        raise Bad(f"a width is seconds, as `60s` — got {toks[1]!r}")
    width_ms = int(float(toks[1][:-1]) * 1000)
    metric, labels, fields, cite = toks[2], {}, {}, None
    for tok in toks[3:]:
        if tok.startswith("@"):
            off, _, ln = tok[1:].partition("+")
            cite = (int(off), int(ln or 0))
            continue
        k, sep, v = tok.partition("=")
        if not sep:
            raise Bad(f"expected key=value or a citation — got {tok!r}")
        if k in COMBINE:
            fields[k] = float(v)
        else:
            labels[k] = v
    return Sample(ts, width_ms, metric, labels, fields, cite)


def read(text):
    """Every line, split into samples and markers, with the NEWEST line
    for a bucket winning.

    ⚠ That last rule is not about lateness — nothing emits a revision.
    It is what makes a RECOMPUTE idempotent: re-running an extractor over
    a window writes its buckets again, and a reader that summed both
    would double every count.
    """
    seen, order = {}, []
    markers = []
    for n, line in enumerate(text.splitlines(), 1):
        # A blank line, and a `#` comment — which a tally store never
        # holds, but a golden file or a hand-annotated extract does.
        # Not silent skipping: anything else that will not parse is
        # still fatal.
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        try:
            s = parse_line(line)
        except Bad as e:
            raise Bad(f"line {n}: {e}") from None
        if s is None:
            continue
        if s.is_marker:
            markers.append(s)
            continue
        if s.key in seen:
            order[seen[s.key]] = None
        seen[s.key] = len(order)
        order.append(s)
    return [s for s in order if s is not None], markers


# ------------------------------------------------------- what to plot

def local_epoch(ms):
    """The epoch second SHIFTED into local wall clock.

    ⚠ gnuplot renders a time axis as though every value were UTC, so the
    shift has to be in the number. Computed per row rather than once, so
    a window spanning a daylight-saving change stays right on both sides
    of it.
    """
    secs = ms / 1000.0
    return secs + time.localtime(secs).tm_gmtoff


def is_histogram(samples):
    """A cumulative ladder, which is the one thing a plot gets WRONG
    rather than ugly: summing `le` series multiplies the count."""
    return any("le" in s.labels for s in samples)


def combine(field, have, value):
    rule = COMBINE[field]
    if rule == "add":
        return have + value
    if rule == "min":
        return min(have, value)
    if rule == "max":
        return max(have, value)
    return value


def pick_field(samples, want=None):
    """Which measure to draw. `sum` before `count` because a metric that
    states both is measuring the thing it sums; a bare count has nothing
    else to be."""
    present = set()
    for s in samples:
        present.update(s.fields)
    if want:
        if want not in present:
            raise Bad(
                f"no {want!r} in these samples — they carry "
                f"{', '.join(sorted(present)) or 'nothing'}"
            )
        return want
    for f in ("sum", "count", "last", "max", "min"):
        if f in present:
            return f
    raise Bad("these samples carry no measure")


def matching(samples, metric, where):
    out = [s for s in samples if s.metric == metric]
    if not out:
        seen = sorted({s.metric for s in samples})
        raise Bad(
            f"no metric {metric!r} in this window — it holds "
            f"{', '.join(seen) or 'nothing'}"
        )
    for k, v in (where or {}).items():
        out = [s for s in out if s.labels.get(k, "") == v]
    if not out:
        raise Bad(f"no series of {metric!r} matches {where!r}")
    return out


def series(samples, metric, by=(), where=None, field=None, rate=False):
    """One line per distinct value of `by`, aggregated over every label
    it does not name.

    ⚠ Aggregating is legal only for an ADDITIVE measure. `last` is a
    gauge and has no meaning summed across series, so a plot that
    collapsed one would be inventing a number — refused rather than
    drawn.
    """
    chosen = matching(samples, metric, where)
    f = pick_field(chosen, field)
    labels = {k for s in chosen for k in s.labels}
    collapsing = labels - set(by)
    if COMBINE[f] == "newest" and collapsing:
        raise Bad(
            f"{metric}.{f} is a gauge, and these samples vary by "
            f"{', '.join(sorted(collapsing))} — a gauge has no meaning summed "
            f"across series, so name them: --by {' --by '.join(sorted(collapsing))}"
        )

    lines = {}
    for s in chosen:
        if f not in s.fields:
            continue
        key = s.series_key(by)
        at = lines.setdefault(key, {})
        at[s.ts] = combine(f, at[s.ts], s.fields[f]) if s.ts in at else s.fields[f]

    if rate:
        if COMBINE[f] != "add":
            raise Bad(f"{f} is not a total, so a rate of it means nothing")
        for at in lines.values():
            for ts in at:
                width = next(s.width_ms for s in chosen if s.ts == ts) / 1000.0
                at[ts] = at[ts] / width if width else at[ts]
    return f, lines


def quantile(samples, q, by=(), where=None):
    """A quantile of a cumulative `le` ladder, per bucket.

    Interpolated linearly inside the bucket it falls in, which is what a
    cumulative histogram supports and the reason a stored percentile was
    never allowed: this can be computed from additive data, a stored one
    could not be re-bucketed.
    """
    ladders = {}
    for s in samples:
        if "le" not in s.labels or "count" not in s.fields:
            continue
        rest = {k: v for k, v in s.labels.items() if k != "le"}
        if where and any(rest.get(k, "") != v for k, v in where.items()):
            continue
        key = (tuple(rest.get(k, "") for k in by), s.ts)
        ladders.setdefault(key, {})[s.labels["le"]] = s.fields["count"]

    lines = {}
    for (skey, ts), ladder in ladders.items():
        total = ladder.get("+Inf")
        if not total:
            continue
        bounds = sorted(
            (float(le), n) for le, n in ladder.items() if le != "+Inf"
        )
        want = q * total
        prev_bound, prev_n = 0.0, 0.0
        value = None
        for bound, n in bounds:
            if n >= want:
                span, took = n - prev_n, want - prev_n
                value = prev_bound + (bound - prev_bound) * (took / span if span else 0)
                break
            prev_bound, prev_n = bound, n
        if value is None:
            # Past the last finite bound: the ladder cannot say where in
            # the tail it sits, so it says the tail began.
            value = bounds[-1][0] if bounds else 0.0
        lines.setdefault(skey, {})[ts] = value
    return lines


def one(samples, metric, by=(), where=None, field=None, rate=False,
        want_quantile=None, facts=None):
    """One metric, as `(measure, {series: {ts: value}}, unit)`.

    The judgement a plot has to make about a single metric, in one place:
    what may be aggregated, what a histogram needs, and what the numbers
    are measured in.
    """
    facts = facts or {}
    chosen = matching(samples, metric, where)
    if want_quantile is not None and is_histogram(chosen):
        got = quantile(chosen, want_quantile, tuple(by), where)
        what = f"p{want_quantile * 100:g}"
    elif is_histogram(chosen) and "le" not in by:
        raise Bad(
            f"{metric} is a cumulative `le` ladder, and summing one "
            f"multiplies the count. Ask for a quantile, or draw the "
            f"ladder itself (by le)"
        )
    else:
        what, got = series(chosen, metric, tuple(by), where, field, rate)
    unit = facts.get(metric, {}).get("unit") or ""
    if rate:
        unit = f"{unit}/s" if unit else "per second"
    return what, got, unit


def render_width(ms):
    """As the writer renders it: whole seconds."""
    return f"{ms // 1000}s"


def widths(samples, metric):
    return {s.width_ms for s in samples if s.metric == metric}


def against(samples, y_metric, x_metric, by=(), where=None, field=None,
            rate=False, want_quantile=None, facts=None):
    """One point per BUCKET: x is one metric, y the other.

    ⚠ This is the honest plot for "are these two related". Two lines on a
    shared time axis — especially on two y scales — is the picture that
    invites seeing a relationship that is not there, because almost any
    pair can be made to look correlated by choosing the scales. A cloud
    stays a cloud.
    """
    wx, wy = widths(samples, x_metric), widths(samples, y_metric)
    if wx != wy or len(wx) != 1:
        raise Bad(
            f"{y_metric} is bucketed at "
            + ", ".join(render_width(w) for w in sorted(wy) or [0])
            + f" and {x_metric} at "
            + ", ".join(render_width(w) for w in sorted(wx) or [0])
            + " — points that are not pairs are not a scatter. Re-bucket one "
              "to the other's width first"
        )
    xwhat, xlines, xunit = one(samples, x_metric, by, where, field, rate,
                               want_quantile, facts)
    ywhat, ylines, yunit = one(samples, y_metric, by, where, field, rate,
                               want_quantile, facts)

    # ⚠ Each side must resolve to ONE series, or be paired by the same
    # labels. Otherwise the pairing is whichever series happened to sort
    # first, which is a picture of nothing.
    if set(xlines) != set(ylines):
        only_x = sorted(name(k) for k in set(xlines) - set(ylines))
        only_y = sorted(name(k) for k in set(ylines) - set(xlines))
        raise Bad(
            f"the two sides do not pair: {x_metric} has "
            + (", ".join(only_x) or "none") + f" that {y_metric} does not, and "
            + (", ".join(only_y) or "none") + " the other way. Name the labels "
              "they share with `by`, or narrow with `where`"
        )

    points, dropped = {}, 0
    for key in sorted(xlines):
        at_x, at_y = xlines[key], ylines[key]
        for ts in sorted(at_x):
            if ts not in at_y:
                # ⚠ Counted, never quietly halved: a bucket present on one
                # side and absent on the other is not a pair, and how many
                # were like that is the reader's business.
                dropped += 1
                continue
            points.setdefault(key, []).append((at_x[ts], at_y[ts], ts))
        dropped += sum(1 for ts in at_y if ts not in at_x)
    if not points:
        raise Bad(
            f"no bucket holds both {x_metric} and {y_metric} — they were "
            f"measured over different windows"
        )
    label = lambda what, unit, m: f"{m} {what} ({unit})" if unit else f"{m} {what}"
    return (points, label(xwhat, xunit, x_metric),
            label(ywhat, yunit, y_metric), dropped)


def terms(samples, metrics, by=(), where=None, field=None, rate=False,
          want_quantile=None, facts=None):
    """Every metric asked for, as one set of lines ready to draw.

    Shared by both front ends so the judgement — what may be aggregated,
    what a histogram needs, which axis a unit belongs on — is made once
    rather than twice and differently.

    ⚠ Two metrics with DIFFERENT units do not share a y axis. Drawing
    them as though they did is how a plot makes any two things look
    related, which matters most in the case this exists for: somebody
    checking whether they are.
    """
    facts = facts or {}
    lines, units, several = {}, {}, len(metrics) > 1
    for metric in metrics:
        what, got, unit = one(samples, metric, by, where, field, rate,
                              want_quantile, facts)
        units[metric] = (unit, what)
        for key, at in got.items():
            lines[(metric,) + tuple(key) if several else tuple(key)] = at

    # ⚠ One axis per distinct UNIT, and the unit alone: it is what makes
    # two numbers comparable. Keying on the measure as well put a count
    # and a sum of the same thing on two axes, which says they are
    # incomparable when they are not.
    distinct = []
    for metric in metrics:
        if units[metric][0] not in distinct:
            distinct.append(units[metric][0])
    if len(distinct) > 2:
        raise Bad(
            "these metrics are measured in "
            + ", ".join(u or "no unit" for u in distinct)
            + " — a plot with three y axes is one nobody can read, so draw "
              "them separately"
        )

    def axis_label(unit):
        measures = sorted({w for u, w in units.values() if u == unit})
        what = measures[0] if len(measures) == 1 else "/".join(measures)
        return f"{what} ({unit})" if unit else what

    ylabel = axis_label(distinct[0])
    y2label = axis_label(distinct[1]) if len(distinct) > 1 else None
    on_y2 = set()
    if y2label:
        for metric in metrics:
            if units[metric][0] == distinct[1]:
                on_y2.update(k for k in lines if k and k[0] == metric)
    return lines, ylabel, y2label, on_y2


# --------------------------------------------------------- the drawing

def tape_units(markers):
    """The unit a `!meta` in this window announced.

    ⚠ It is there when it is there. The marker is written once per RUN,
    and a follower's run is however long systemd keeps it up, so a window
    inside a long one holds none — which is why `--using` exists and why
    the marker's placement is recorded as a defect in
    docs/plans/tally.md. When it IS present it is authoritative.
    """
    return {
        s.labels["metric"]: s.labels["unit"]
        for s in markers
        if s.metric == "!meta" and "metric" in s.labels and "unit" in s.labels
    }


#: Where a document is looked up by NAME, in order — later shadows
#: earlier, which is the rule a same-named file already follows.
#:
#: ⚠ A reader's list. `timberfs tally --provision` has its own and never
#: includes a home directory: it runs as a service, and what a service
#: does must not depend on whose home it looked in.
EXTRACTOR_DIRS = (
    "/usr/lib/timberfs/tally.extractors.d",
    "/etc/timberfs/tally.extractors.d",
    "~/.config/timberfs/tally.extractors.d",
)


def extractor_dirs():
    base = os.environ.get("XDG_CONFIG_HOME")
    out = []
    for d in EXTRACTOR_DIRS:
        if d.startswith("~"):
            d = (os.path.join(base, "timberfs", "tally.extractors.d") if base
                 else os.path.expanduser(d))
        out.append(d)
    return [d for d in out if os.path.isdir(d)]


def resolve_extractor(name_or_path):
    """A path, or a NAME looked up in the reading directories.

    An argument that exists is taken as given; anything else is a name,
    which is what makes a per-user directory worth having rather than
    another place to type a long path from.
    """
    if os.path.exists(name_or_path):
        return name_or_path
    for d in reversed(extractor_dirs()):
        candidate = os.path.join(d, f"{name_or_path}.json")
        if os.path.isfile(candidate):
            return candidate
    where = ", ".join(extractor_dirs()) or "no extractor directory that exists"
    raise Bad(
        f"no extractor {name_or_path!r} — neither a path that exists nor a "
        f"document in {where}"
    )


def extractor_facts(paths):
    """What a plot cannot read off a line: the UNIT, and which metrics
    exist at all.

    ⚠ Named at plot time and nowhere else. Recording it in the tally
    store's manifest would have to be kept in step with the provisioning
    that applies the documents, and would travel to hosts where the name
    resolves to a different document or to none — see docs/plans/tally.md.
    Absent, everything but these two facts is still known.
    """
    facts = {}
    for path in (resolve_extractor(p) for p in paths):
        with open(path, encoding="utf-8") as fh:
            doc = json.load(fh)
        for m in doc.get("metrics", []):
            unit = (m.get("histogram") or {}).get("unit")
            for measure in m.get("measure", []):
                unit = measure.get("unit", unit)
            facts[m["name"]] = {
                "unit": unit,
                "description": m.get("description"),
                "extractor": doc.get("name"),
            }
    return facts


#: Tic intervals worth landing on, in seconds. A reader looks for
#: :00, :15, :30 — never :07.
TICS = (60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200,
        86400, 604800)


def x_tics(first_local, span_secs, want=6):
    """Where the tics go, snapped to a round unit of LOCAL wall clock.

    ⚠ Local, not epoch. The x column is already shifted into local
    seconds, so snapping there lands on the hour a reader sees — where
    snapping the epoch would put an hourly tic on the half hour in a
    half-hour zone.
    """
    ideal = max(span_secs, 1) / want
    step = next((t for t in TICS if t >= ideal), TICS[-1])
    start = -(-int(first_local) // step) * step
    return start, step


def x_format(span_ms):
    if span_ms <= 6 * 3600_000:
        return "%H:%M"
    if span_ms <= 5 * 86400_000:
        return "%m-%d %H:%M"
    return "%m-%d"


def script(lines, title, ylabel, terminal, size, marks=(), output=None,
           y2label=None, on_y2=()):
    """The gnuplot script, with its data inline so it re-runs on its own.

    One script for every output: `dumb` is ASCII in a terminal over ssh,
    `pngcairo` is a file to attach, `qt` is a window. Two renderers would
    drift, and the ASCII one would be the one that quietly stopped
    showing a series.
    """
    if not lines:
        raise Bad("nothing to draw")
    every = sorted({ts for at in lines.values() for ts in at})
    span = every[-1] - every[0] if len(every) > 1 else 0
    out = [
        f"# timbergraph {VERSION} — re-runs on its own: gnuplot -p THIS",
        # ⚠ noenhanced, or gnuplot reads `_` as a subscript and
        # `http_requests` is drawn as `httprequests`. Every metric name
        # in this format has one.
        f"set terminal {terminal} size {size[0]},{size[1]} noenhanced",
    ]
    if output:
        out.append(f"set output {gp_quote(output)}")
    out += [
        "set xdata time",
        "set timefmt '%s'",
        f"set format x '{x_format(span)}'",
        "set xtics {},{}".format(
            *x_tics(local_epoch(every[0]), span / 1000.0)
        ),
        "set datafile separator '\\t'",
        f"set title {gp_quote(title)}",
        f"set ylabel {gp_quote(ylabel)}",
        # ⚠ `width 2` because gnuplot's dumb terminal packs the key
        # entries against each other — `200 *******404 #######` reads as
        # one legend entry with a strange name.
        "set key outside below width 2",
        "set grid",
    ]
    if y2label:
        # ⚠ Said on the axis, because two scales on one picture is how a
        # plot makes unrelated things look related.
        out += [f"set y2label {gp_quote(y2label)}", "set y2tics", "set ytics nomirror"]
    # ⚠ Only where the terminal can draw one. On `dumb` an arrow is
    # invisible, so the markers are said in words instead — never
    # dropped, because a bucket whose numbers are wrong must not look
    # like one whose numbers are right.
    if terminal != "dumb":
        for at, what in marks:
            out.append(
                f"set arrow from {local_epoch(at)}, graph 0 to "
                f"{local_epoch(at)}, graph 1 nohead lc rgb "
                f"{gp_quote('#cc0000' if what == '!drop' else '#cc8800')} lw 1"
            )
    plots = ", ".join(
        "'-' using 1:2 with lines"
        + (" axes x1y2" if k in on_y2 else "")
        + f" title {gp_quote(name(k) + (' (right)' if k in on_y2 else ''))}"
        for k in sorted(lines)
    )
    out.append("plot " + plots)
    for k in sorted(lines):
        at = lines[k]
        for ts in sorted(at):
            out.append(f"{local_epoch(ts)}\t{at[ts]}")
        out.append("e")
    return "\n".join(out) + "\n"


def scatter(points, title, xlabel, ylabel, terminal, size, output=None):
    """A scatter, with its data inline like any other script here.

    ⚠ Not a time axis: time becomes the COLOUR instead, where the
    terminal has one — so a relationship that drifted looks different
    from one that held, which a cloud of undated points cannot show.
    """
    if not points:
        raise Bad("nothing to draw")
    coloured = terminal != "dumb" and len(points) == 1
    out = [
        f"# timbergraph {VERSION} — re-runs on its own: gnuplot -p THIS",
        f"set terminal {terminal} size {size[0]},{size[1]} noenhanced",
    ]
    if output:
        out.append(f"set output {gp_quote(output)}")
    out += [
        "set datafile separator '\t'",
        f"set title {gp_quote(title)}",
        f"set xlabel {gp_quote(xlabel)}",
        f"set ylabel {gp_quote(ylabel)}",
        "set grid",
        "set key outside below width 2",
    ]
    if coloured:
        out += ["set cbdata time", "set timefmt '%s'", "set format cb '%H:%M'",
                "set cblabel 'when'", "unset key"]
    plots = ", ".join(
        f"'-' using 1:2:3 with points palette pt 7 ps 0.7 notitle" if coloured
        else f"'-' using 1:2 with points title {gp_quote(name(k))}"
        for k in sorted(points)
    )
    out.append("plot " + plots)
    for k in sorted(points):
        for x, y, ts in points[k]:
            out.append(f"{x}\t{y}\t{local_epoch(ts)}")
        out.append("e")
    return "\n".join(out) + "\n"


def name(key):
    return ",".join(k for k in key if k) or "all"


def gp_quote(text):
    return "'" + str(text).replace("'", "''") + "'"


def draw(text, terminal):
    """Run gnuplot, or say what to install. ⚠ A SOFT dependency: the
    tools carry no third-party imports and nothing here should be the
    first hard one."""
    exe = shutil.which("gnuplot")
    if not exe:
        raise Bad(
            "gnuplot is not installed, and it is what draws these — "
            "`apt install gnuplot` (or gnuplot-nox for files only; a "
            "window needs gnuplot-qt or gnuplot-x11)"
        )
    proc = subprocess.run(
        [exe, "-p" if terminal in ("qt", "x11", "wxt") else "-"],
        input=text, text=True, capture_output=True, check=False,
    )
    if proc.returncode != 0:
        raise Bad(f"gnuplot refused it: {proc.stderr.strip()}")
    return proc.stdout


# ------------------------------------------------------------- the CLI

def terminal_size():
    try:
        cols, rows = shutil.get_terminal_size((100, 30))
        return max(60, min(cols, 200)), max(15, min(rows - 6, 40))
    except Exception:
        return 100, 30


def summarise(samples, markers, out):
    """What is in here, for somebody who does not yet know what to ask."""
    by_metric = {}
    for s in samples:
        m = by_metric.setdefault(s.metric, {"buckets": set(), "series": set(),
                                            "fields": set(), "hist": False})
        m["buckets"].add(s.ts)
        m["series"].add(tuple(sorted(s.labels.items())))
        m["fields"].update(s.fields)
        m["hist"] = m["hist"] or "le" in s.labels
    for metric in sorted(by_metric):
        m = by_metric[metric]
        print(
            f"{metric:<24} {len(m['buckets'])} bucket(s), {len(m['series'])} "
            f"series, {', '.join(sorted(m['fields']))}"
            f"{'  [histogram]' if m['hist'] else ''}",
            file=out,
        )
    for s in markers:
        print(f"{s.metric:<24} {s.labels} {s.fields}", file=out)


def note_markers(markers, window, out):
    """⚠ Said in words wherever they cannot be drawn. A bucket carrying a
    `!drop` holds numbers that are WRONG, not merely surprising, and a
    plot that showed it like any other would be the lie this format has
    refused everywhere else."""
    lo, hi = window
    seen = {}
    for s in markers:
        if s.metric not in TROUBLE or not (lo <= s.ts <= hi):
            continue
        w = seen.setdefault(s.metric, {"n": 0, "buckets": 0, "worst": 0.0})
        w["n"] += s.fields.get("count", 0)
        w["buckets"] += 1
        w["worst"] = max(w["worst"], s.fields.get("max", 0.0))
    for what in sorted(seen):
        w = seen[what]
        why = {
            "!drop": "entries claimed and unreadable — these numbers are WRONG",
            "!cap": "series over the cap — these numbers are UNDERSTATED",
            "!late": "entries displaced from another bucket",
            "!gap": "the follower could not see this window",
        }.get(what, "")
        extra = f", worst {w['worst']:.0f}" if w["worst"] else ""
        print(
            f"⚠ {what}: {w['n']:.0f} in {w['buckets']} bucket(s){extra} — {why}",
            file=out,
        )


def main(argv=None):
    import argparse

    ap = argparse.ArgumentParser(
        prog="timbergraph",
        description="Graph a tally store. EXPERIMENTAL.",
        epilog="timberfs query app-tally --from 13:00 | timbergraph -m http_requests --by status",
    )
    ap.add_argument("file", nargs="*", help="tally lines; default stdin")
    ap.add_argument("-m", "--metric", action="append", default=[],
                    help="which metric to draw; repeatable, or comma-separated")
    ap.add_argument("--by", action="append", default=[], metavar="LABEL",
                    help="one line per value of this label; repeatable")
    ap.add_argument("--where", action="append", default=[], metavar="K=V",
                    help="only series with this label; repeatable")
    ap.add_argument("--field", help="which measure (count, sum, min, max, last)")
    ap.add_argument("--rate", action="store_true",
                    help="divide by the bucket width: per second")
    ap.add_argument("--against", metavar="METRIC",
                    help="a SCATTER: one point per bucket, x this metric and "
                         "y the one named by -m. The honest plot for `are "
                         "these two related`")
    ap.add_argument("--quantile", type=float, metavar="Q",
                    help="of a histogram's cumulative ladder, e.g. 0.95")
    ap.add_argument("--using", action="append", default=[], metavar="PATH",
                    help="an extractor document — a path, or a NAME in "
                         "~/.config/timberfs/tally.extractors.d (or the site "
                         "or packaged ones)")
    ap.add_argument("--list", action="store_true",
                    help="say what is in the input and draw nothing")
    ap.add_argument("--png", metavar="FILE")
    ap.add_argument("--svg", metavar="FILE")
    ap.add_argument("--window", action="store_true",
                    help="an interactive window; needs a display and gnuplot-qt")
    ap.add_argument("--width", type=int,
                    help="characters on the terminal, pixels in an image")
    ap.add_argument("--height", type=int)
    ap.add_argument("--tsv", metavar="FILE",
                    help="the table behind the picture")
    ap.add_argument("--gnuplot", metavar="FILE",
                    help="the script that drew it, data inline, so it re-runs alone")
    ap.add_argument("--version", action="version", version=f"timbergraph {VERSION}")
    args = ap.parse_args(argv)

    text = ""
    if args.file:
        for f in args.file:
            with open(f, encoding="utf-8") as fh:
                text += fh.read()
    else:
        text = sys.stdin.read()

    samples, markers = read(text)
    if not samples and not markers:
        raise Bad("no tally lines on the input")
    metrics = [m for spec in args.metric for m in spec.split(",") if m]
    if args.list or not metrics:
        summarise(samples, markers, sys.stdout)
        if not args.metric:
            print("\nname a metric with -m to draw one", file=sys.stderr)
        return 0

    where = {}
    for kv in args.where:
        k, _, v = kv.partition("=")
        where[k] = v

    facts = extractor_facts(args.using)
    if args.against:
        metrics = metrics + [args.against]
    for m in metrics:
        facts.setdefault(m, {})
        facts[m].setdefault("unit", tape_units(markers).get(m))

    if args.against:
        if len(metrics) != 2:
            raise Bad("--against takes one -m and one --against, not more")
        points, xlabel, ylabel, dropped = against(
            samples, metrics[0], args.against, tuple(args.by), where,
            args.field, args.rate, args.quantile, facts)
        title = f"{metrics[0]} against {args.against}"
        every = sorted({ts for pts in points.values() for _, _, ts in pts})
        window = (every[0], every[-1]) if every else (0, 0)
        if dropped:
            print(f"⚠ {dropped} bucket(s) held one of them and not the other, "
                  f"and are not points", file=sys.stderr)
        y2label, on_y2, lines = None, (), points
    else:
        lines, ylabel, y2label, on_y2 = terms(
            samples, metrics, tuple(args.by), where, args.field, args.rate,
            args.quantile, facts,
        )
        first = facts.get(metrics[0], {})
        title = (first.get("description") if len(metrics) == 1 else None) \
            or ", ".join(metrics)
        every = sorted({ts for at in lines.values() for ts in at})
        window = (every[0], every[-1]) if every else (0, 0)

    if args.tsv:
        with open(args.tsv, "w", encoding="utf-8") as fh:
            if args.against:
                fh.write("series\tx\ty\tepoch_ms\n")
                for k in sorted(lines):
                    for x, y, ts in lines[k]:
                        fh.write(f"{name(k)}\t{x}\t{y}\t{ts}\n")
            else:
                fh.write("series\tepoch_ms\tvalue\n")
                for k in sorted(lines):
                    for ts in sorted(lines[k]):
                        fh.write(f"{name(k)}\t{ts}\t{lines[k][ts]}\n")

    marks = [(s.ts, s.metric) for s in markers
             if s.metric in TROUBLE
             and window[0] <= s.ts <= window[1]
             and s.labels.get("metric") in metrics]

    dest = args.png or args.svg
    terminal = ("pngcairo" if args.png else "svg" if args.svg
                else "qt" if args.window else "dumb")
    if terminal == "dumb":
        size = (args.width or terminal_size()[0],
                args.height or terminal_size()[1])
    else:
        size = (args.width or (900 if args.window else 1000),
                args.height or 500)
    if args.against:
        text_out = scatter(lines, title, xlabel, ylabel, terminal, size, dest)
    else:
        text_out = script(lines, title, ylabel, terminal, size, marks, dest,
                          y2label, on_y2)

    if args.gnuplot:
        with open(args.gnuplot, "w", encoding="utf-8") as fh:
            fh.write(text_out)

    drawn = draw(text_out, terminal)
    if drawn:
        sys.stdout.write(drawn)
    note_markers(markers, window, sys.stderr)
    if args.png or args.svg:
        print(f"wrote {args.png or args.svg}", file=sys.stderr)
    return 0


def cli(argv=None):
    try:
        return main(argv)
    except Bad as e:
        print(f"timbergraph: {e}", file=sys.stderr)
        return 2
    except BrokenPipeError:
        os._exit(0)
