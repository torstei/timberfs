"""timberview — a store read as a tape, not as a result set.

`select` walks an answer; this walks the log. They share the absolute
tape offset and nothing else, which is why the handle here is a chunk
number rather than a cursor: a cursor is a place in an answer, and there
is no answer to be in.

It does no entry parsing — no timestamps, no window verification, no
`.grain`. Chunks, decompressed, shown as lines. The two stores `select`
serves worst, one whose lines cannot be parsed and one with no index,
are exactly the ones you most want to look at, and this asks nothing of
the content.

Everything reaches the log through four operations, and nothing else:

    stores()                        the list, to switch between
    bounds(store)                   first_seq, last_seq, what was dropped
    chunk(store, seq=N | at=MS)     lines + the ring around them
    search(token)                   addresses

Written against those, the viewer is testable against a fake, and a
second kind of source is a second implementation of the same four.

⚠ The fan-out is not here. `search` is asked for a token and does not
know there are ten hosts; whoever provides the backend does.
"""
import base64
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import time

import timberfs_client
from timberfs_client import when, when_ms                     # noqa: F401

DOC_V = "1.0-EXPERIMENTAL"

# Two different things, and conflating them made a UUID unselectable.
#
# The INDEX holds maximal ASCII-alphanumeric runs of 3..=64 bytes, exact
# case — so `9da3dcf1-5a4b-…` is five of them, none of which is the id.
MIN_TOKEN, MAX_TOKEN = 3, 64
RUN = re.compile(r"[A-Za-z0-9]+")
# A `has` TERM is wider: it may carry the separators an identifier is
# written with, and timberfs ANDs the runs inside it on the index and
# then matches the whole thing word-anchored. So the UUID is one term,
# and it is the one worth offering — `5a4b` on its own matched every
# entry in a store where the whole id matched one.
#
# The joiners are the characters that appear INSIDE an identifier. `=`,
# `/` and the rest separate fields, so `path=/api/v1/x` stays four
# terms rather than becoming one.
TERM = re.compile(r"[A-Za-z0-9]+(?:[-._:][A-Za-z0-9]+)*")

SCHEME = "timber"


# ------------------------------------------------------------- address
class Address:
    """A place: a store, optionally a host to find it on, optionally a
    position in it.

    The store id is the name and the host is a HINT — bake the host in
    and a pasted link breaks the day a store moves. The position says
    which coordinate it is, because `#offset=` and `#chunk=` are
    different numbers and one resolved to the wrong place silently is
    worse than one that will not parse."""

    KINDS = ("offset", "chunk", "at")

    def __init__(self, store, host=None, kind=None, value=None):
        self.store, self.host, self.kind, self.value = store, host, kind, value

    def __eq__(self, other):
        return (isinstance(other, Address)
                and (self.store, self.host, self.kind, self.value)
                == (other.store, other.host, other.kind, other.value))

    def __repr__(self):
        return f"Address({str(self)!r})"

    def __str__(self):
        base = (f"{SCHEME}://{self.host}/{self.store}" if self.host
                else f"{SCHEME}:{self.store}")
        return f"{base}#{self.kind}={self.value}" if self.kind else base


def parse_address(text):
    """`timber://host/id#offset=N`, `timber://host/id#chunk=N`,
    `timber:id`. A full id, never a prefix: a short one is right at a
    prompt, where an ambiguity is discovered by the person who typed it,
    and wrong in a link, where it is discovered by whoever pasted it."""
    text = text.strip()
    for pre in (f"{SCHEME}://", f"{SCHEME}:"):
        if text.startswith(pre):
            rest = text[len(pre):]
            break
    else:
        raise ValueError(f"not a {SCHEME}: address: {text!r}")
    body, _, frag = rest.partition("#")
    host = None
    if pre.endswith("//"):
        host, _, body = body.partition("/")
        if not host:
            raise ValueError(f"{text!r} has no host between the slashes")
    store = body.strip("/")
    if not store:
        raise ValueError(f"{text!r} names no store")
    if not frag:
        return Address(store, host)
    kind, sep, value = frag.partition("=")
    if not sep or kind not in Address.KINDS:
        raise ValueError(
            f"{frag!r}? a position says which coordinate it is — "
            + ", ".join(f"#{k}=" for k in Address.KINDS))
    if kind in ("offset", "chunk"):
        if not value.isdigit():
            raise ValueError(f"#{kind}= wants a number, not {value!r}")
        value = int(value)
    return Address(store, host, kind, value)


# --------------------------------------------------------- the wire
def unzstd(frame):
    """A chunk arrives compressed and is decompressed HERE: asking the
    far end to do it costs 13x the bytes for nothing."""
    try:
        from compression.zstd import decompress   # stdlib from 3.14
        return decompress(frame)
    except ImportError:
        pass
    try:
        p = subprocess.run(["zstd", "-dcq"], input=frame, capture_output=True)
    except OSError as e:
        raise RuntimeError(
            "cannot decompress a chunk: this Python has no "
            f"`compression.zstd` (3.14+) and `zstd` did not run ({e}). "
            "Install the zstd package.") from None
    if p.returncode:
        raise RuntimeError(
            "zstd could not decompress a chunk: "
            + (p.stderr.decode("utf-8", "replace").strip() or f"exit {p.returncode}"))
    return p.stdout


def _record_at(buf, i):
    """One record at or after `i` as `(kind, fields, payload, next_i)`, or
    None where the buffer does not hold all of it.

    The framing rules live here ONCE: `frames` walks a buffer that is
    whole with it, `frames_from` one that is still arriving, and a stream
    read as it comes must not be parsed by a second set of rules that can
    drift from these."""
    start = buf.find(b"\x1e", i)
    if start < 0:
        return None
    j = buf.find(b"\0", start)
    if j < 0:
        return None                       # the head is not finished
    head = buf[start + 1:j].split(b"\x1f")
    fields = {}
    for p in head[1:]:
        k, _, v = p.partition(b"=")
        fields[k.decode("utf-8", "replace")] = v.decode("utf-8", "replace")
    end = j + 1
    payload = None
    if "len" in fields:
        n = int(fields["len"])
        # ⚠ ALL of it, plus its NUL. A short buffer is not a short
        # payload, it is a payload that has not arrived — and half an
        # entry handed on as whole is the one thing this format refuses.
        if end + n + 1 > len(buf):
            return None
        payload = buf[end:end + n]
        end += n + 1
    return head[0].decode("utf-8", "replace"), fields, payload, end


def frames(buf):
    """`timberfs-records(5)` over BYTES. A record is RS, fields, NUL —
    and where a `len` field says so, that many payload bytes and a NUL.
    The payload is read by length rather than scanned for, because a
    compressed frame contains every byte value including the separators."""
    i = 0
    while True:
        got = _record_at(buf, i)
        if got is None:
            return
        kind, fields, payload, i = got
        yield kind, fields, payload


def frames_from(buf):
    """The same records, plus WHAT IS LEFT — for a stream read as it
    arrives rather than waited for.

    A record is yielded only once every byte of it is here; the remainder
    goes back to the reader to be added to. Returns `(records, leftover)`."""
    out, i = [], 0
    while True:
        start = buf.find(b"\x1e", i)
        if start < 0:
            return out, b""               # nothing half-arrived
        got = _record_at(buf, start)
        if got is None:
            return out, buf[start:]       # this one is still coming
        kind, fields, payload, i = got
        out.append((kind, fields, payload))


# The tape is addressed on the WRITE axis and an investigation window is
# stated on the LOGLINE one, so a chunk written at 14:05 can hold an entry
# stamped 13:00. timberfs widens a logline selection by the same margin
# for the same reason; the bound can then only over-INCLUDE, which is the
# safe direction — a line belonging to the window is never lost, and a few
# that do not may be seen.
WIDEN_MS = 60_000


class Chunk:
    """One chunk and the ring around it: where it sits on the tape, and
    the write window it covers."""

    def __init__(self, seq, start, length, wf, wl, data):
        self.seq, self.start, self.length = seq, start, length
        self.wf, self.wl, self.data = wf, wl, data

    @property
    def end(self):
        return self.start + self.length


class Hit:
    """A search result: an address, enough to recognise it, and the
    write window it arrived in.

    That window is the fallback coordinate. An entry record has carried
    `wf` since long before it carried `offset`, and a write-axis window
    of one millisecond IS a seek to the chunk that covers it — measured
    against a live store, seeking by `wf` alone lands on the entry's own
    chunk. So an answer that cannot say where an entry is can still say
    WHEN, and when is enough to open the log around it."""

    def __init__(self, address, store, text, ts=None, wf=None):
        self.address, self.store, self.text, self.ts = address, store, text, ts
        self.wf = wf


class Refused(Exception):
    """The far end said no, in its own words, and which end said it."""

    def __init__(self, message, said=None, host=None):
        super().__init__(message)
        self.said, self.host = said, host


# The refusal a timberfs from before the bounded seek gives. It is the
# FIRST wall anyone meets, and relayed as-is it reads as the caller's
# mistake — a chunk number with `max: {chunks: 1}` is exactly the legal
# thing to have sent. Read from the refusal because a version cannot say
# it: the builds either side of that change both report 0.25.0.
NO_SEEK = "a chunk number is a resume position"

# The release that put `chunk` and `offset` ON AN ENTRY RECORD. Before
# it an answer cannot say where its entries are — and that reads exactly
# like an entry still at the live edge, which is a claim about the DATA
# where the truth is a fact about the server.
PLACED_FROM = (0, 26, 0)


def server_of(text):
    """`(0, 26, 0)` from `timberfs, 0.26.0` — the answer's own account
    of what produced it, which is authoritative for this stream in a way
    no registration or installed version is."""
    m = re.search(r"(\d+)\.(\d+)\.(\d+)", text or "")
    return tuple(int(g) for g in m.groups()) if m else None


def too_old_to_place(where):
    return ("that timberfs predates the coordinate an answer needs — "
            f"`offset` on an entry record landed in {'.'.join(map(str, PLACED_FROM))}"
            f". Upgrade {where}, and its answers can be opened.")


def too_old(host, said):
    """What to DO first, what it is second, and the far end's own words
    last. Someone meeting this wants a fix, not a protocol lesson — and
    the remedy is not the same at both ends, since nothing on this
    machine upgrades a timberfs on another one."""
    who = f"{host}: " if host else ""
    fix = (f"upgrade it there" if host else
           "upgrade it, or put a newer build earlier on PATH")
    other = ("Or reach a different build by changing that target's `cmd`."
             if host else "TIMBERFS_CMD names one without installing it.")
    return (f"{who}timberfs is too old — {fix}."
            f"\n      A pager seeks to a chunk by number and this build "
            f"refuses that; the change landed after 0.25.0. {other}"
            f"\n      ⚠ Its version will not tell you which you have: the "
            f"builds either side of that change report the same one."
            f"\n      It said: {said}")


class QueryBackend:
    """The four operations over `timberfs-query-document(5)`.

    `ask(doc, host, raw)` is the whole transport: it hands a document to
    one host and returns `(out, stderr, rc)`, bytes when `raw`. timbersh
    passes its own, which is what makes the viewer's fan-out the shell's
    fan-out rather than a second one."""

    CACHE = 8
    # How long a store's bounds are believed. Retention moves the floor
    # while you read, so they cannot be remembered from when the store
    # was opened — but re-reading them before every seek puts a whole
    # round trip in front of a chunk that is often already cached.
    FRESH = 10.0

    def __init__(self, ask, hosts=(None,)):
        self.ask, self.hosts = ask, list(hosts)
        self._stores = None
        # host -> the version it reported, for a target too old to say
        # where its entries are.
        self.stale = {}
        # A chunk's bytes never change once written, so a cached one
        # cannot go stale — which is what makes reading ahead safe.
        self._chunks, self._inflight = {}, set()
        self._lock = threading.Lock()
        # A host that could not be reached is NAMED. A short answer and a
        # broken one look identical, and this is the difference between
        # "not in the logs" and "one machine did not answer".
        self.unreachable = {}

    def _run(self, doc, host, raw=False):
        out, err, rc = self.ask(doc, host, raw)
        if rc != 0:
            raise Refused(f"{host or '(local)'} did not answer: "
                          + (err or f"exit {rc}"), said=err, host=host)
        return out

    def stores(self, fresh=False):
        """Every store on every host, each tagged with where it lives.

        The whole catalogue, not the one a predicate selected: a hit can
        land in a store the predicate never covered, and switching to it
        is the point of having searched."""
        if self._stores is not None and not fresh:
            return self._stores
        return self.select_stores([], remember=True)

    def each_host(self, work):
        """`work(host)` on every host AT ONCE. There is nothing to be
        gained by asking them in turn: the cost is the latency, not the
        search, and a reader waiting on ten hosts serially is waiting
        nine times longer than the fleet needs."""
        out, lock = {}, threading.Lock()

        def one(host):
            try:
                value = work(host)
            except (Refused, json.JSONDecodeError) as e:
                with lock:
                    self.unreachable[host] = str(e)
                return
            with lock:
                out[host] = value
                self.unreachable.pop(host, None)

        if len(self.hosts) == 1:
            one(self.hosts[0])
            return out
        threads = [threading.Thread(target=one, args=(h,), daemon=True)
                   for h in self.hosts]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        return out

    def select_stores(self, terms, remember=False):
        doc = {"v": DOC_V, "stores": {"select": list(terms)},
               "response_format": {"kind": "stores"}}
        answers = self.each_host(lambda h: json.loads(self._run(doc, h) or "[]"))
        got = []
        for host in self.hosts:
            payload = answers.get(host)
            if payload is None:
                continue
            found = (payload.get("stores", []) if isinstance(payload, dict)
                     else payload)
            for st in found:
                st["_host"] = host
                st["_read_at"] = time.monotonic()
            got.extend(found)
        got.sort(key=lambda s: (s.get("_host") or "", s.get("name") or ""))
        if remember:
            self._stores = got
        return got

    def bounds(self, store):
        """The store as it is NOW. Retention moves the floor while you
        read, so the numbers a screen states have to be re-read rather
        than remembered from when it was opened."""
        doc = {"v": DOC_V,
               "stores": {"select": [{"key": "id", "op": "=",
                                      "value": store["id"]}]},
               "response_format": {"kind": "stores"}}
        payload = json.loads(self._run(doc, store.get("_host")) or "[]")
        found = (payload.get("stores", []) if isinstance(payload, dict)
                 else payload)
        if not found:
            return None
        found[0]["_host"] = store.get("_host")
        found[0]["_read_at"] = time.monotonic()
        return found[0]

    def prefetch(self, store, *seqs):
        """Read ahead, off the critical path.

        Optional: a backend without it simply never reads ahead. One
        chunk is ten screenfuls, so fetching the neighbours while the
        reader is looking at this one is the difference between
        scrolling and waiting."""
        for seq in seqs:
            if seq is None or seq < 0:
                continue
            key = (store.get("id"), seq)
            with self._lock:
                if key in self._chunks or key in self._inflight:
                    continue
                self._inflight.add(key)
            threading.Thread(target=self._read_ahead, args=(store, seq, key),
                             daemon=True).start()

    def _read_ahead(self, store, seq, key):
        try:
            self.chunk(store, seq=seq)
        except (Refused, ValueError, RuntimeError):
            pass          # the foreground will meet the same refusal, and say so
        finally:
            with self._lock:
                self._inflight.discard(key)

    def _remember(self, key, chunk):
        with self._lock:
            self._chunks[key] = chunk
            while len(self._chunks) > self.CACHE:
                self._chunks.pop(next(iter(self._chunks)))

    def chunk(self, store, seq=None, at=None):
        """One chunk, by number or by the instant it covers.

        `max: {chunks: 1}` beside a start is a seek, and a pager is
        nothing but seeks."""
        key = (store.get("id"), seq) if seq is not None else None
        if key is not None:
            with self._lock:
                hit = self._chunks.get(key)
            if hit is not None:
                return hit
        win = {"axis": "write"}
        if seq is not None:
            win["from_chunk"] = int(seq)
        elif at is not None:
            win["from"] = win["to"] = int(at)
        else:
            raise ValueError("chunk() wants a seq or an at")
        doc = {"v": DOC_V,
               "stores": {"select": [{"key": "id", "op": "=",
                                      "value": store["id"]}]},
               "window": win, "max": {"chunks": 1},
               "response_format": {"kind": "chunks"}}
        try:
            out = self._run(doc, store.get("_host"), raw=True)
        except Refused as e:
            if e.said and NO_SEEK in e.said:
                raise Refused(too_old(e.host, e.said.strip())) from None
            raise
        for kind, f, payload in frames(out):
            if kind == "chunk":
                got = Chunk(int(f["chunk"]), int(f["uncomp_start"]),
                            int(f["uncomp_len"]), int(f.get("wf", 0)),
                            int(f.get("wl", 0)), unzstd(payload))
                self._remember((store.get("id"), got.seq), got)
                return got
        return None

    def records(self, doc):
        """A `records` answer from every target, in the order they were
        given. The streams stay separate: each is self-contained, and
        joining them would lose which host attributed what."""
        answers = self.each_host(lambda h: self._run(doc, h, raw=True))
        return [answers[h] for h in self.hosts if answers.get(h)]

    def search(self, token, limit=200):
        """Where that token appears, as addresses.

        Every store on every host, because the loop this exists for
        starts with an identifier and no idea which log holds it."""
        doc = {"v": DOC_V, "stores": {"select": []},
               "match": {"granularity": "entries", "all": [{"has": token}]},
               "max": {"entries": limit},
               "response_format": {"kind": "records"}}
        by_id = {s.get("id"): s for s in self.stores()}
        answers = self.each_host(lambda h: self._run(doc, h, raw=True))
        hits, unplaced, capped = [], 0, False
        for host in self.hosts:
            out = answers.get(host)
            if out is None:
                continue
            # An entry names its store only where the read spanned
            # several, so with one source the `source` record is the
            # attribution and the entries inherit it.
            sources, version = [], None
            for kind, f, payload in frames(out):
                if kind == "stream-start":
                    version = server_of(f.get("server_version"))
                    continue
                if kind == "source":
                    sources.append(f.get("id"))
                    continue
                if kind == "stream-end":
                    capped = capped or f.get("status") == "limited"
                    continue
                if kind != "entry":
                    continue
                sid = f.get("id") or (sources[0] if len(sources) == 1 else None)
                store = by_id.get(sid)
                text = (payload or b"").decode("utf-8", "replace")
                # A hit with no PLACE is still a hit. Dropping it made a
                # term that matched only on a target which cannot place
                # one read as "no hit" — which is false, and the one
                # thing an answer must never say. It is listed, it can
                # be read, its terms can be searched; only OPENING it is
                # refused, and then with the reason.
                if "chunk" in f and "offset" in f:
                    where = Address(sid, host, "offset", int(f["offset"]))
                else:
                    where = Address(sid, host)      # the store, no position
                    if version and version < PLACED_FROM:
                        self.stale[host] = version
                    else:
                        unplaced += 1
                hits.append(Hit(where, store, text.split("\n")[0].rstrip(),
                                int(f["ts"]) if f.get("ts") else None,
                                int(f["wf"]) if f.get("wf") else None))
        return hits, unplaced, capped


# ---------------------------------------------------------------- text
def scrolled_to(span, col, width):
    """The sideways offset that shows `span`, given the current one.

    A little context either side, so the term does not sit against the
    edge it just came from — a term flush against the right reads as the
    end of the line.

    Shared by the pager and the hit list because Tab means the same thing
    on both screens, and a pick the window does not follow is a pick you
    cannot see."""
    margin = min(8, max(0, (width - (span[1] - span[0])) // 2))
    if span[0] < col + margin:
        return max(0, span[0] - margin)
    if span[1] > col + width - margin:
        return max(0, span[1] - width + margin)
    return col


def sanitise(s):
    """A raw-mode screen has to be told what to draw. Tabs become
    columns; every other control byte becomes one visible glyph, so an
    ANSI escape in a log line reads as text instead of repainting the
    terminal. Nothing is dropped."""
    out, col = [], 0
    for ch in s:
        o = ord(ch)
        if ch == "\t":
            n = 8 - (col % 8)
            out.append(" " * n)
            col += n
        elif o < 32 or 127 <= o < 160:
            out.append("·")
            col += 1
        else:
            out.append(ch)
            col += 1
    return "".join(out)


def indexable(text):
    """The runs of `text` the index can actually hold. Maximal runs, so
    a 65-character one yields nothing rather than its first 64 — that is
    what the index does, and a prefix would find a different thing."""
    return [m.group() for m in RUN.finditer(text)
            if MIN_TOKEN <= len(m.group()) <= MAX_TOKEN]


def terms(text):
    """The searchable spans of a line, as (start, end, text).

    A term is offered only where the index can hold at least ONE of its
    runs: that run is what lets a search skip chunks, and without one
    nothing could find it. That is the same test timberfs applies, so
    what can be picked is still exactly what can be searched."""
    return [(m.start(), m.end(), m.group()) for m in TERM.finditer(text)
            if indexable(m.group())]


def why_not_a_term(word):
    """Why nothing could find this one. `26.1.18` is the case: three
    runs of one and two characters, so the index holds none of them and
    it is refused where you point at it rather than discovered later as
    an empty answer."""
    runs = [m.group() for m in RUN.finditer(word)]
    if not runs:
        return (f"{word!r} has no letters or digits in it — the index "
                f"holds runs of {MIN_TOKEN}-{MAX_TOKEN} of them")
    short = [r for r in runs if len(r) < MIN_TOKEN]
    if len(short) == len(runs):
        return (f"{word!r} is {len(runs)} run(s) of under {MIN_TOKEN} "
                f"characters ({', '.join(runs)}) — the index holds none "
                f"of them, so no search can find it")
    return (f"{word!r} has no run the index can hold: every one is under "
            f"{MIN_TOKEN} characters or over {MAX_TOKEN}")


def human(n):
    for u in ("B", "KiB", "MiB", "GiB", "TiB"):
        if n < 1024 or u == "TiB":
            return f"{n:.0f} {u}" if u == "B" else f"{n:.1f} {u}"
        n /= 1024


def grouped(n):
    return f"{n:,}".replace(",", " ")


# ----------------------------------------------------------- clipboard
# What OSC 52 is trusted with. A terminal that dislikes the size does not
# say so — it truncates, or drops the sequence whole — so past this the
# text goes to a file, where a truncation cannot be silent.
OSC_LIMIT = 100_000


def helpers():
    """The clipboard commands that could work HERE, in order.

    Each is gated on the display it writes to, because one without it
    does not fail usefully: it writes a clipboard nothing will read, or
    reports a connection error the reader never asked about."""
    import shutil
    out = []
    if os.environ.get("WAYLAND_DISPLAY"):
        out.append(["wl-copy"])
    if os.environ.get("DISPLAY"):
        out += [["xclip", "-selection", "clipboard"], ["xsel", "-ib"]]
    if sys.platform == "darwin":
        out.append(["pbcopy"])
    return [c for c in out if shutil.which(c[0])]


def osc52(text):
    """Hand the bytes to the TERMINAL — the one route that reaches a
    workstation from the far end of an ssh or a multiplexer.

    ⚠ Its failure is SILENCE: a terminal that does not implement it
    does nothing and says nothing. So a copy says which route it took
    rather than that the text is on a clipboard.

    Written to /dev/tty rather than through curses, because it is a
    message to the terminal and not output: the screen curses is holding
    is neither drawn on nor invalidated."""
    seq = b"\033]52;c;" + base64.b64encode(text.encode("utf-8")) + b"\a"
    with open("/dev/tty", "wb", buffering=0) as tty:
        tty.write(seq)


def spill(data):
    """The last resort: the text in a file, and the path said.

    A copy that could not be made must still leave the selection
    somewhere it can be got at."""
    fd, path = tempfile.mkstemp(prefix="timberview-", suffix=".txt")
    with os.fdopen(fd, "wb") as f:
        f.write(data)
    return path


def to_clipboard(text):
    """`text` out of the pager, and the phrase saying by which route.

    A helper first where there is a display to use one on: it writes the
    clipboard of the machine this process is on, and its exit status says
    whether it did. OSC 52 next, the only route that crosses an ssh. A
    file last, so a selection is never lost quietly.

    ⚠ The helper's output is DISCARDED rather than captured: `wl-copy`
    daemonises to serve the selection, and a captured pipe it inherits
    keeps `run()` waiting until the clipboard is next replaced.

    ⚠ It runs on the CALLING thread, and must: `osc52` writes the
    terminal curses is also writing, and two writers can split an escape
    sequence between them."""
    data = text.encode("utf-8")
    for cmd in helpers():
        try:
            p = subprocess.run(cmd, input=data, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL, timeout=2)
        except (OSError, subprocess.TimeoutExpired):
            continue
        if p.returncode == 0:
            return f"on the clipboard ({cmd[0]})"
    if len(data) <= OSC_LIMIT:
        try:
            osc52(text)
            # Short on purpose: this is a status LINE, and the hedge has
            # to survive being read next to what was copied.
            return "to the terminal (OSC 52) — if nothing pastes, it was dropped"
        except OSError:
            pass
    why = (f"{human(len(data))} is more than OSC 52 can be trusted with"
           if len(data) > OSC_LIMIT else "no clipboard is reachable from here")
    return f"{why} — written to {spill(data)}"


class Line:
    __slots__ = ("offset", "text", "_terms", "at", "store", "first", "wf")

    def __init__(self, offset, raw):
        self.offset = offset
        self.at = self.store = self.wf = None
        self.first = True
        self.text = sanitise(raw.decode("utf-8", "replace"))
        self._terms = None

    @property
    def terms(self):
        if self._terms is None:
            self._terms = terms(self.text)
        return self._terms


# ---------------------------------------------------------------- tape
class Tape:
    """A contiguous run of chunks, as lines with absolute offsets.

    One chunk is ten screenfuls for a few KiB on the wire, so the run is
    held either side of where you are and scrolling never falls out of a
    window. A line that straddles a chunk boundary is held back while
    there is more tape on that side: showing half of it would be a
    truncation the reader cannot see."""

    KEEP = 6

    def __init__(self, backend, store, window=None):
        self.backend, self.store = backend, store
        # The investigation's window as `(from_ms, to_ms)`, either end
        # possibly None. The tape will not scroll out of it — see
        # `stopped_by_window`.
        self.window = window
        self.chunks, self.lines = [], []
        # (seq, why) for a chunk that could not be read. Scrolling into
        # a host that went away must not look like the end of the tape.
        self.trouble = None

    # -- the store's own numbers, which retention moves under us
    @property
    def first_seq(self):
        return self.store.get("first_seq")

    @property
    def last_seq(self):
        return self.store.get("last_seq")

    def refresh(self):
        """The store as it is now — unless it was just read. A seek to a
        cached chunk otherwise pays a round trip for numbers that cannot
        have moved in the time since."""
        age = time.monotonic() - (self.store.get("_read_at") or 0)
        if age < getattr(self.backend, "FRESH", 0):
            return
        fresh = self.backend.bounds(self.store)
        if fresh:
            self.store = fresh

    def leave_for_the_log(self):
        """From a result set into the LOG around the entry you are on.

        The one motion a result set cannot do for itself: an answer is
        the entries that matched, and what you usually want next is what
        was happening around one of them."""
        if not isinstance(self.source, Records):
            self.message = "this is the log — you are already in it"
            return False
        self.source_old = getattr(self.source, "old", None)
        line = self.line()
        at, when = self.source.address_of(line), getattr(line, "wf", None)
        if at is None and when is None:
            self.message = ("that entry says neither where nor when it is, so "
                            "there is nothing to open it by")
            return False
        store = (getattr(line, "store", None) or {})
        if not store.get("id"):
            self.message = "that entry does not say which store it came from"
            return False
        text = line.text
        self.source = Tape(self.backend, store)
        if at is not None:
            self.open(offset=at.value)
            self.message = f"the log around {at}"
        else:
            # No offset, but the write window is a seek to the chunk it
            # arrived in — which is where it is.
            self.open(at=when)
            self.find(text)
            old = "a target on " + ".".join(map(str, self.source_old)) \
                if getattr(self, "source_old", None) else "the answer"
            self.message = (f"{self.address()} — by its write window, since "
                            f"{old} gave no exact offset")
        return True

    def open(self, seq=None, at=None, offset=None):
        """Land somewhere and load the neighbours. With nothing named,
        the last chunk — which is what a pager opening a file does."""
        if offset is not None:
            c = self.locate(offset)
        elif at is not None:
            c = self.backend.chunk(self.store, at=at)
        else:
            if seq is None:
                seq = self.last_seq
            if seq is None:
                raise Refused(f"{self.store.get('name')} holds no chunks")
            c = self.backend.chunk(self.store, seq=int(seq))
        if c is None:
            raise Refused(
                "no chunk there — the store holds "
                f"{self.first_seq}..{self.last_seq}")
        self.chunks = [c]
        self._rebuild()
        # The neighbours are READ AHEAD rather than waited for: one chunk
        # is already ten screenfuls, so a screen can be drawn now and the
        # scroll that needs them will find them there. The line each edge
        # holds back until they land is off the screen you landed on.
        self.read_ahead()
        return c

    def read_ahead(self):
        """Ask for the chunks either side, off the critical path."""
        ahead = getattr(self.backend, "prefetch", None)
        if not ahead or not self.chunks:
            return
        lo, hi = self.chunks[0].seq, self.chunks[-1].seq
        want = []
        if self.first_seq is not None and lo > self.first_seq:
            want.append(lo - 1)
        if self.last_seq is not None and hi < self.last_seq:
            want.append(hi + 1)
        if want:
            ahead(self.store, *want)

    def locate(self, offset):
        """Which chunk holds an absolute offset.

        There is no seek-by-offset on the wire, so this bisects the
        chunk numbers on what each fetched chunk says its own range is —
        exact, and a handful of round trips at any store size."""
        lo, hi = self.first_seq, self.last_seq
        if lo is None:
            raise Refused(f"{self.store.get('name')} holds no chunks")
        guess = lo + (hi - lo) // 2
        for _ in range(48):
            c = self.backend.chunk(self.store, seq=guess)
            if c is None:
                raise Refused(f"chunk {guess} is not there")
            if offset < c.start:
                hi = c.seq - 1
            elif offset >= c.end:
                lo = c.seq + 1
            else:
                return c
            if lo > hi:
                raise Refused(
                    f"offset {grouped(offset)} is not on this tape — it "
                    f"holds {grouped(self.store.get('dropped_uncompressed_bytes', 0))}"
                    f"..{grouped(self.tape_end)}"
                    + (f", and {self.store['dropped_chunks']} chunk(s) were "
                       "dropped" if self.store.get("dropped_chunks") else ""))
            guess = lo + (hi - lo) // 2
        raise Refused(f"could not place offset {grouped(offset)}")

    @property
    def tape_start(self):
        return self.store.get("dropped_uncompressed_bytes", 0) or 0

    @property
    def tape_end(self):
        return self.tape_start + (self.store.get("logical_bytes", 0) or 0)

    def at_top(self):
        if not self.chunks:
            return False
        if self.chunks[0].seq == self.first_seq:
            return True
        return self.stopped_by_window(above=True)

    def at_bottom(self):
        if not self.chunks:
            return False
        if self.chunks[-1].seq == self.last_seq:
            return True
        return self.stopped_by_window(above=False)

    def stopped_by_window(self, above):
        """Is the investigation's window the reason there is no more?

        The chunk already loaded at this edge is checked, never the one
        beyond it: if the top chunk's write window STARTS at or before the
        window's floor, the chunk above it lies entirely before the window
        and there is nothing there to want. So the bound costs no read.

        ⚠ Widened, and on the WRITE clock the chunks carry. The window is
        stated in logline time and a chunk written after it can hold
        entries stamped inside it — so this can only ever stop LATE,
        showing a little either side rather than losing a line that
        belongs."""
        if not self.window:
            return False
        lo, hi = self.window
        if above:
            return lo is not None and self.chunks[0].wf <= lo - WIDEN_MS
        return hi is not None and self.chunks[-1].wl >= hi + WIDEN_MS

    def _rebuild(self):
        buf = b"".join(c.data for c in self.chunks)
        base = self.chunks[0].start
        lines, pos = [], 0
        for raw in buf.split(b"\n"):
            lines.append(Line(base + pos, raw))
            pos += len(raw) + 1
        # A trailing newline yields an empty final piece that is not a
        # line; a missing one leaves a fragment that the next chunk
        # completes.
        if buf.endswith(b"\n") or not self.at_bottom():
            lines.pop()
        if not self.at_top() and lines:
            lines.pop(0)
        self.lines = lines

    def _load(self, seq):
        try:
            c = self.backend.chunk(self.store, seq=seq)
        except Refused as e:
            self.trouble = (seq, str(e))
            return None
        if c is None:
            self.trouble = (seq, "the store no longer holds it")
        else:
            self.trouble = None
        return c

    def extend_up(self):
        if not self.chunks or self.at_top():
            return False
        c = self._load(self.chunks[0].seq - 1)
        if c is None or c.seq >= self.chunks[0].seq:
            return False
        self.chunks.insert(0, c)
        del self.chunks[self.KEEP:]
        self._rebuild()
        self.read_ahead()
        return True

    def extend_down(self):
        if not self.chunks or self.at_bottom():
            return False
        c = self._load(self.chunks[-1].seq + 1)
        if c is None or c.seq <= self.chunks[-1].seq:
            return False
        self.chunks.append(c)
        if len(self.chunks) > self.KEEP:
            del self.chunks[0]
        self._rebuild()
        self.read_ahead()
        return True

    def address_of(self, line):
        return Address(self.store.get("id"), self.store.get("_host"),
                       "offset", line.offset if line else self.tape_start)

    def describe(self, line):
        """The header: which store, and where on its tape."""
        s = self.store
        off = line.offset if line else self.tape_start
        span = max(1, self.tape_end - self.tape_start)
        pct = min(100, max(0, int(100 * (off - self.tape_start) / span)))
        where = f"{s.get('name')}"
        if s.get("_host"):
            where += f" @ {s['_host']}"
        return (f"── {where} · chunk {self.chunk_of(off)} · "
                f"offset {grouped(off)} · {pct}%")

    def top_notes(self):
        s = self.store
        if self.chunks and self.chunks[0].seq != self.first_seq \
                and self.stopped_by_window(above=True):
            return [self.window_note(above=True)]
        notes = [f"── top of the log · chunk {s.get('first_seq')}"]
        if s.get("dropped_chunks"):
            notes.append(
                f"── {grouped(s['dropped_chunks'])} chunk(s) older were "
                f"dropped ({human(s.get('dropped_uncompressed_bytes', 0))} "
                f"off the tape)")
        return notes

    def window_note(self, above):
        """An edge that is the SESSION's and not the log's says so, and
        says on which clock — otherwise it reads as the end of the store,
        which is the one thing an edge must never be mistaken for."""
        lo, hi = self.window
        end = when_ms(lo) if above else when_ms(hi)
        return (f"── the session window {'starts' if above else 'ends'} about "
                f"here ({end}) · write clock, widened {WIDEN_MS // 1000}s · "
                f"`t` to change it")

    def bottom_note(self):
        s = self.store
        if self.chunks and self.chunks[-1].seq != self.last_seq \
                and self.stopped_by_window(above=False):
            return self.window_note(above=False)
        tail = (f"a writer holds it ({s['writer']}), so newer lines may not "
                "be flushed yet" if s.get("writer")
                else "nothing is appending")
        return f"── end of chunk {s.get('last_seq')} · {tail}"

    def index_of(self, offset):
        """The line holding an offset, or the nearest one after it. Line
        numbers move when the run does; an offset does not, so every
        position is remembered as one."""
        lo, hi = 0, len(self.lines) - 1
        best = 0
        while lo <= hi:
            mid = (lo + hi) // 2
            if self.lines[mid].offset <= offset:
                best = mid
                lo = mid + 1
            else:
                hi = mid - 1
        return best

    def chunk_of(self, offset):
        for c in self.chunks:
            if c.start <= offset < c.end:
                return c.seq
        return self.chunks[0].seq if self.chunks else None


class Records:
    """A RESULT SET, read the way the tape is read.

    `select` walks an answer and `view` walks the log — different
    motions, and this is the first one given the second's screen. What
    makes it fit is that an entry record carries `chunk` and `offset`,
    so every line here knows the PLACE it came from: the same coordinate
    the tape is addressed by, and the reason `Enter` can leave a result
    set for the log around one of its entries.

    A multi-line entry is one entry — the lines of a stack trace belong
    to the entry that raised it, and splitting them would be the same
    lie as splitting a line across a chunk boundary.

    It is a CLOSED set: both ends are ends, nothing extends, and the
    header counts entries rather than naming a chunk."""

    def __init__(self, streams, stores=(), what="a result"):
        self.what = what
        self.by_id = {s.get("id"): s for s in stores}
        self.lines, self.entries = [], 0
        self.unplaced, self.trouble = 0, None
        # A bound stopped the read, so the bottom of this screen is not the
        # end of the answer. Saying otherwise is the same lie as a tape
        # that stops scrolling without saying why.
        self.limited = False
        # Entries from a timberfs too old to say where they are, which
        # is not the same fact as an entry at the live edge.
        self.stale, self.old = 0, None
        self.chunks = []
        for stream in ([streams] if isinstance(streams, bytes) else streams):
            self._read(stream)
        self.store = {"name": what, "id": None}

    def _read(self, stream):
        # Per stream: each host's answer names its own sources, and a
        # one-source stream's entries carry no id of their own.
        names, seen, version = {}, [], None
        for kind, f, payload in frames(stream):
            if kind == "stream-start":
                version = server_of(f.get("server_version"))
                continue
            if kind == "source":
                names[f.get("id")] = os.path.basename(f.get("path", "?"))
                seen.append(f.get("id"))
                continue
            if kind == "stream-end":
                self.limited = self.limited or f.get("status") == "limited"
                continue
            if kind != "entry":
                continue
            sid = f.get("id") or (seen[0] if len(seen) == 1 else None)
            store = self.by_id.get(sid) or {"id": sid, "name": names.get(sid)}
            # An entry still at the live edge is real and has no place
            # yet, so it is SHOWN and simply cannot be opened.
            where = (Address(sid, store.get("_host"), "offset",
                             int(f["offset"])) if "offset" in f else None)
            if where is None:
                if version and version < PLACED_FROM:
                    self.stale += 1
                    self.old = version
                else:
                    self.unplaced += 1
            body = (payload or b"").rstrip(b"\n").split(b"\n")
            when = int(f["wf"]) if f.get("wf") else None
            for n, raw in enumerate(body):
                line = Line(int(f.get("offset", 0)), raw)
                line.at = where
                line.wf = when
                line.store = store
                line.first = n == 0
                self.lines.append(line)
            self.entries += 1

    # -- a closed set: there is nothing either side of it
    def at_top(self):
        return True

    def at_bottom(self):
        return True

    def extend_up(self):
        return False

    def extend_down(self):
        return False

    def read_ahead(self):
        pass

    def refresh(self):
        pass

    def index_of(self, offset):
        for i, ln in enumerate(self.lines):
            if ln.offset >= offset:
                return i
        return max(0, len(self.lines) - 1)

    def chunk_of(self, offset):
        return None

    def address_of(self, line):
        return getattr(line, "at", None)

    def describe(self, line):
        """Which entry's store you are ON — which is not the same one
        line to line, and is the point of showing an answer this way."""
        store = getattr(line, "store", None) or {}
        where = store.get("name") or "?"
        if store.get("_host"):
            where += f" @ {store['_host']}"
        at = self.address_of(line)
        return (f"── {where}"
                + (f" · offset {grouped(at.value)}" if at else " · live edge")
                + f" · {self.entries} entr{'y' if self.entries == 1 else 'ies'}"
                + f" · {self.what}")

    def top_notes(self):
        note = (f"── {'the first ' if self.limited else ''}{self.entries} entr"
                f"{'y' if self.entries == 1 else 'ies'}"
                f"{'' if self.limited else ' matched'} · {self.what}")
        if self.unplaced:
            note += f" · {self.unplaced} at a live edge, with no place to open"
        if self.stale:
            note += (f" · {self.stale} answered on "
                     f"{'.'.join(map(str, self.old))}, which cannot say where "
                     f"an entry is")
        return [note]

    def bottom_note(self):
        if self.limited:
            return ("── the answer continues past here · a bound stopped the "
                    "read · `o` opens the log around an entry")
        return ("── end of the answer · `o` opens the log around an entry"
                if self.entries else "── nothing matched")


class Joined:
    """A source with each multi-line entry rendered as ONE line.

    Ten stack traces in an answer is four hundred lines, and every one of
    them pushes the next entry off the screen — so the thing you are
    actually doing, deciding whether these ten are the same failure, has
    no screen to do it on. Joined, each entry is a row: the message at the
    left where the eye compares them, the frames trailing off to the
    right where `h`/`l` can go and read them.

    ⚠ The continuation lines are LSTRIPPED as they are joined. Their
    indent is what puts the frames in a column under a message that is no
    longer above them, and it is the difference between ten rows that
    line up and ten that do not.

    Nothing is hidden: every line of the entry is on the row. That is why
    this is a rendering and not a fold — search, Tab, the hit list and
    the entry motion all keep working, because every line on screen is a
    real line with a real address.

    A DECORATOR rather than a mode inside each source: `Tape` and
    `Records` both have entries in this sense, the view swaps its source
    at runtime (a hit in another store), and neither of them should learn
    about a display option."""

    SEP = " ↵ "

    def __init__(self, source):
        self.source = source
        self._n = None
        self._lines = []
        self._runs = []

    def __getattr__(self, name):
        # Everything not about lines is the source's, including the
        # extends — which grow `source.lines` and so invalidate the cache
        # by changing its length.
        return getattr(self.source, name)

    @property
    def lines(self):
        self._fresh()
        return self._lines

    def _fresh(self):
        if self._n != len(self.source.lines):
            self._rebuild()

    def rows_as_lines(self, lo, hi):
        """The LOG's lines behind rows `lo`..`hi`, inclusive.

        A join is a rendering: `↵` between the frames of a stack trace
        is a thing to read, and not a thing to paste into something that
        analyses one. So what is taken away is the lines the row was made
        of, in the order the log holds them."""
        self._fresh()
        out = []
        for run in self._runs[lo:hi + 1]:
            out.extend(run)
        return out

    def _rebuild(self):
        out, runs, run = [], [], []

        def flush():
            if not run:
                return
            head = run[0]
            ln = Line(head.offset, b"")
            ln.text = head.text + "".join(
                self.SEP + x.text.lstrip() for x in run[1:])
            ln.at, ln.store, ln.wf, ln.first = head.at, head.store, head.wf, True
            out.append(ln)
            runs.append(list(run))

        for line in self.source.lines:
            if getattr(line, "first", True) and run:
                flush()
                run = []
            run.append(line)
        flush()
        self._lines, self._runs = out, runs
        self._n = len(self.source.lines)

    # An offset lands on the ENTRY holding it, which is the row it is now
    # part of. The base implementations would scan the source's lines and
    # answer with an index into the wrong list.
    def index_of(self, offset):
        for i, ln in enumerate(self.lines):
            if ln.offset >= offset:
                return i
        return max(0, len(self.lines) - 1)

    def chunk_of(self, offset):
        return self.source.chunk_of(offset)


# ---------------------------------------------------------------- view
NEAR = 200          # lines from an edge at which the next chunk is fetched


class View:
    """Where the reader is, and what a screen of that size would show.

    Free of curses on purpose: this is the half worth testing, and it is
    tested against a fake backend rather than a terminal."""

    def __init__(self, backend, store=None, source=None):
        self.backend = backend
        self.join = False
        self.source = source if source is not None else Tape(backend, store)
        self.wrap = False
        self.col = 0
        self.top = self.cur = 0
        self.tok = 0
        self.message = ""
        self.hits, self.hit = [], -1
        self.term = None
        # The row a region starts at, or None. A ROW index and not an
        # offset: it is a place on this screen, which is what the reader
        # marked — and `_keep_place` carries it when the run renumbers.
        self.mark = None
        # Set when the PICK moved, so the next layout brings the term
        # into view. Only then: scrolling sideways by hand and having
        # the screen snap back would be the same fight from the other
        # side.
        self.follow_term = False

    # `source` is a PROPERTY so the join survives the view swapping it —
    # a hit in another store replaces the source outright, and a display
    # option must not be something each of those sites remembers.
    @property
    def source(self):
        return self._joined or self._source

    @source.setter
    def source(self, s):
        # A new tape INHERITS the investigation's window. `o` into the log
        # around an entry, and following a hit into another store, both
        # replace the source outright — and they are exactly when the
        # guard matters, since a tape has no predicate of its own to keep
        # it near the incident.
        if hasattr(s, "window") and s.window is None:
            s.window = getattr(getattr(self, "_source", None), "window", None)
        self._source = s
        self._joined = Joined(s) if self.join else None
        # A mark is a row of the tape it was set on. Following a hit into
        # another store keeps neither end of a region, so it is dropped
        # here rather than quietly re-used against different lines.
        self.mark = None

    def set_window(self, text):
        """`from T to T`, `to T`, `none` — the words the shell uses.

        Parsed here rather than in the screen so the rule has one home,
        and so a test can drive it without a terminal."""
        toks = text.split()
        if len(toks) == 1 and toks[0].lower() in ("none", "off", "drop"):
            self.source.window = None
            self.message = "window dropped"
            return
        lo = hi = None
        i = 0
        while i < len(toks):
            w = toks[i].lower()
            if w in ("from", "since", "after") and i + 1 < len(toks):
                lo = when(toks[i + 1]); i += 2
            elif w in ("to", "until", "before") and i + 1 < len(toks):
                hi = when(toks[i + 1]); i += 2
            elif w == "and":
                i += 1
            else:
                raise ValueError(f"`{toks[i]}`? from <time> to <time>, "
                                 f"or `none`")
        if lo is None and hi is None:
            raise ValueError("from <time> to <time>, or `none`")
        if lo is not None and hi is not None and lo > hi:
            raise ValueError("that window starts after it ends")
        self.source.window = (lo, hi)
        self.message = f"window {when_ms(lo)} .. {when_ms(hi)}"

    def join_entries(self, on=None):
        """Show each multi-line entry as one row, or stop.

        The line under the cursor is kept: its offset addresses the ENTRY
        either way, so the same entry is under you before and after, and
        a toggle is not also a jump."""
        where = self.line()
        lines = self.source.lines
        mark_off = (lines[self.mark].offset
                    if self.mark is not None and self.mark < len(lines)
                    else None)
        self.join = (not self.join) if on is None else on
        self.source = self._source          # re-wrap, or unwrap
        if where is not None:
            self.cur = self.source.index_of(where.offset)
            self.top = min(self.top, self.cur)
        # Both ends of the region are kept for the same reason the cursor
        # is: an offset addresses the entry either way, so a display
        # toggle is not also a change of what you selected.
        if mark_off is not None:
            self.mark = self.source.index_of(mark_off)
        self.tok, self.col = 0, 0
        self.message = ("multi-line entries as one row" if self.join
                        else "entries as they are written")

    # -- opening
    def leave_for_the_log(self):
        """From a result set into the LOG around the entry you are on.

        The one motion a result set cannot do for itself: an answer is
        the entries that matched, and what you usually want next is what
        was happening around one of them."""
        if not isinstance(self.source, Records):
            self.message = "this is the log — you are already in it"
            return False
        self.source_old = getattr(self.source, "old", None)
        line = self.line()
        at, when = self.source.address_of(line), getattr(line, "wf", None)
        if at is None and when is None:
            self.message = ("that entry says neither where nor when it is, so "
                            "there is nothing to open it by")
            return False
        store = (getattr(line, "store", None) or {})
        if not store.get("id"):
            self.message = "that entry does not say which store it came from"
            return False
        text = line.text
        self.source = Tape(self.backend, store)
        if at is not None:
            self.open(offset=at.value)
            self.message = f"the log around {at}"
        else:
            # No offset, but the write window is a seek to the chunk it
            # arrived in — which is where it is.
            self.open(at=when)
            self.find(text)
            old = "a target on " + ".".join(map(str, self.source_old)) \
                if getattr(self, "source_old", None) else "the answer"
            self.message = (f"{self.address()} — by its write window, since "
                            f"{old} gave no exact offset")
        return True

    def open(self, seq=None, at=None, offset=None):
        # A pair with no manifest is not a store, so there is no id to
        # write a place in it as — and an address is the point.
        if not self.store.get("id"):
            raise Refused(
                f"{self.store.get('name')!r} carries no identity, so a "
                "place in it cannot be written down — `timberfs identity "
                "--mint` makes the pair a store")
        self.source.refresh()
        # A seek loads a different run, so a row index into the old one
        # is not a place any more.
        self.mark = None
        c = self.source.open(seq=seq, at=at, offset=offset)
        anchor = offset if offset is not None else c.start
        landed_at_the_end = seq is None and at is None and offset is None
        if landed_at_the_end and self.source.at_bottom():
            self.cur = max(0, len(self.source.lines) - 1)
        else:
            self.cur = self.source.index_of(anchor)
        # What you land ON has to be real. A run that does not begin at
        # the store's first chunk holds its first line back as a
        # possible fragment, so landing at the top of one means reaching
        # for the chunk before it — the one read the read-ahead cannot
        # be left to do, because it is on the screen you asked for.
        if self.cur < 2 and not self.source.at_top() and self.source.extend_up():
            self.cur = self.source.index_of(anchor)
        self.top = self.cur
        self.tok = 0
        return c

    @property
    def store(self):
        return self.source.store

    def line(self):
        return self.source.lines[self.cur] if self.source.lines else None

    def address(self):
        """Where the cursor line IS. On a result set that is the entry's
        own store, which is not the same one line to line."""
        return self.source.address_of(self.line())

    # -- movement. Every mutation of the run keeps the place by OFFSET,
    # because a line number belongs to a run and an offset to the tape.
    def _keep_place(self, fn):
        lines = self.source.lines
        top_off = lines[self.top].offset if lines else None
        cur_off = lines[self.cur].offset if lines else None
        mark_off = (lines[self.mark].offset
                    if self.mark is not None and self.mark < len(lines)
                    else None)
        fn()
        if top_off is not None:
            self.top = self.source.index_of(top_off)
            self.cur = self.source.index_of(cur_off)
        if mark_off is not None:
            self.mark = self.source.index_of(mark_off)
            # The run is bounded, so scrolling far enough drops the end
            # the mark was on. The cursor may SLIDE to the nearest line —
            # you were moving it — but a mark that slides is a selection
            # of lines nobody chose, so it goes, and says so.
            got = self.source.lines[self.mark] if self.source.lines else None
            if got is None or got.offset != mark_off:
                self.mark = None
                self.message = ("the mark scrolled out of the run and was "
                                "dropped: the tape holds a few chunks either "
                                "side of you, not the whole log")

    def _widen(self):
        if self.cur < NEAR and not self.source.at_top():
            self._keep_place(self.source.extend_up)
        if len(self.source.lines) - self.cur < NEAR and not self.source.at_bottom():
            self._keep_place(self.source.extend_down)

    def move(self, n):
        self.cur = max(0, min(len(self.source.lines) - 1, self.cur + n))
        self.tok = 0
        self._widen()

    def page(self, n, height):
        self.move(n * max(1, height - 2))

    def move_entry(self, step):
        """To the start of the next entry, over the continuation lines.

        A stack trace is forty lines of one entry, and `j` walks every one
        of them. What is stepped over is exactly what the record stream
        said belongs together — `first` comes from the entry framing, not
        from a guess at what a continuation line looks like.

        ⚠ On the TAPE every line is its own start, because the tape parses
        nothing and cannot know otherwise; there the motion is a line, and
        the help says so. Better than a heuristic that would disagree with
        timberfs about where an entry begins."""
        lines = self.source.lines
        rng = (range(self.cur + 1, len(lines)) if step > 0
               else range(self.cur - 1, -1, -1))
        for i in rng:
            if getattr(lines[i], "first", True):
                self.cur = i
                self.tok = 0
                self._widen()
                return
        self.message = ("the last entry" if step > 0 else "the first entry")

    def home(self):
        """The top of the LOG, which is a seek to its first chunk —
        never a walk back through the run. On a store of 400,000 chunks
        those are not the same operation."""
        self.open(seq=self.source.first_seq)
        self.cur = self.top = 0
        self.tok = 0

    def end(self):
        self.open(seq=self.source.last_seq)
        self.cur = max(0, len(self.source.lines) - 1)
        self.tok = 0

    def scroll_h(self, n):
        self.col = max(0, self.col + n)

    def toggle_wrap(self):
        self.wrap = not self.wrap
        self.col = 0

    # -- terms
    def line_terms(self):
        ln = self.line()
        return ln.terms if ln else []

    def pick(self, step):
        """Tab between the selectable terms, and ON PAST the end of the
        line to the next one that has any.

        It used to wrap inside the line, which makes Tab a loop over four
        tokens when what you are doing is reading down a screen. Off the
        end of a line is the next line's first term; off the front is the
        previous line's last.

        Lines with nothing pickable are STEPPED OVER rather than landed
        on. What can be picked is exactly what can be searched, so a stop
        with nothing to search is a stop with nothing to do — and on the
        tape the walk pulls more of it in as it nears an edge, so the end
        is the source's end and not the buffer's."""
        toks = self.line_terms()
        n = self.tok + step
        if toks and 0 <= n < len(toks):
            self.tok = n
            self.follow_term = True
            return toks[self.tok][2]
        return self.pick_on(step)

    def pick_on(self, step):
        """The next line with a term on it, entered from the side Tab
        arrived at: forwards lands on its first, backwards on its last."""
        lines = self.source.lines
        rng = (range(self.cur + 1, len(lines)) if step > 0
               else range(self.cur - 1, -1, -1))
        for i in rng:
            toks = lines[i].terms
            if not toks:
                continue
            self.cur = i
            self.tok = 0 if step > 0 else len(toks) - 1
            self.follow_term = True
            self._widen()
            return toks[self.tok][2]
        # Nothing ahead has one. Say which way ran out, and stay put:
        # wrapping to the far end is the loop this motion just stopped
        # being, one size larger.
        self.message = ("no searchable term below" if step > 0
                        else "no searchable term above")
        if not self.line_terms():
            self.message = self.nothing_pickable()
        return None

    def selected(self):
        toks = self.line_terms()
        return toks[self.tok][2] if toks and self.tok < len(toks) else None

    def selected_span(self):
        toks = self.line_terms()
        return toks[self.tok] if toks and self.tok < len(toks) else None

    def bring_term_into_view(self, width):
        """Tab across a 200-character log line walks straight off the
        screen otherwise: the pick moves and the sideways scroll does
        not follow it."""
        span = self.selected_span()
        if self.wrap or not span:
            return
        self.col = scrolled_to(span, self.col, width)

    def nothing_pickable(self):
        ln = self.line()
        if not ln or not ln.text.strip():
            return "nothing on this line"
        longest = max(ln.text.split(), key=len)
        return "no searchable term on this line — " + why_not_a_term(longest)

    # -- the region, and taking it away
    def set_mark(self):
        """One end of a region, here; again and it is dropped.

        `set-mark`, because the other design — a start typed as a
        coordinate — makes you name what is already under your eyes. The
        cursor is the other end, so every motion the pager has is a way
        of choosing one."""
        if self.mark is not None:
            self.mark = None
            self.message = "mark dropped"
            return
        self.mark = self.cur
        self.message = "mark set — move to the other end, then c copies"

    def exchange_mark(self):
        """Point and mark swapped, to see the end you are not on.

        A region can be longer than the screen, and the cheapest way to
        check what its far end actually is, is to go and look."""
        if self.mark is None:
            self.message = "no mark to exchange with — m sets one"
            return
        self.mark, self.cur = self.cur, self.mark
        self.tok = 0
        self._widen()

    def region(self):
        """The rows a region covers as `(lo, hi)`, inclusive, or None.

        Neither end is privileged: the region is the same whichever way
        round it was made, so marking and walking BACK works."""
        if self.mark is None:
            return None
        last = len(self.source.lines) - 1
        if last < 0:
            return None
        lo, hi = sorted((self.mark, self.cur))
        return max(0, lo), min(last, hi)

    def entry_span(self):
        """The rows of the ENTRY under the cursor, as `(lo, hi)`.

        A stack trace is one entry and forty lines, so the line the
        cursor happens to be on is one frame of what you are looking at.
        Where the source knows the framing — an answer, where timberfs
        said which lines belong together — this is the whole entry; on
        the TAPE, which parses nothing, every line is its own, exactly as
        the entry motion is a line there."""
        lines = self.source.lines
        if not lines:
            return None
        lo = hi = min(self.cur, len(lines) - 1)
        while lo > 0 and not getattr(lines[lo], "first", True):
            lo -= 1
        while hi + 1 < len(lines) and not getattr(lines[hi + 1], "first", True):
            hi += 1
        return lo, hi

    def selection(self):
        """What a copy would take: `(text, what)`, or None.

        The region if there is a mark, else the entry under the cursor --
        the one that needs no mark being the one you reach for most.

        ⚠ The LOG's lines, never the rows: `z` renders an entry as one
        row with `↵` between its lines, which is for reading rather than
        for pasting into something that analyses a stack trace.

        ⚠ And the lines AS SHOWN — tabs in columns, every other control
        byte one glyph. What is copied is what was on the screen, and an
        ANSI escape in a log line must not reach a clipboard as an
        escape."""
        span = self.region() or self.entry_span()
        if span is None:
            return None
        lo, hi = span
        lines = (self._joined.rows_as_lines(lo, hi) if self._joined
                 else self.source.lines[lo:hi + 1])
        if not lines:
            return None
        n, rows = len(lines), hi - lo + 1
        if self.mark is not None:
            what = f"the region, {rows} row{'' if rows == 1 else 's'}"
            if n != rows:
                what += f" — {n} lines"
        elif n > 1:
            what = f"this entry, {n} lines"
        else:
            what = "this line"
        return "".join(ln.text + "\n" for ln in lines), what

    def copy(self):
        """The selection out, and the mark dropped with it.

        Dropped because the region was made FOR the copy: leaving it
        behind leaves a highlight that means nothing, and the next `c`
        would take what you already have."""
        got = self.selection()
        if got is None:
            self.message = "nothing here to copy"
            return
        text, what = got
        # Whether this was a REGION is the one thing the message can teach:
        # a reader who has just copied an entry and wanted several rows is
        # standing exactly where the mark needs naming.
        marked = self.mark is not None
        route = to_clipboard(text)
        self.mark = None
        self.message = f"{what}, {human(len(text.encode('utf-8')))} · {route}"
        if not marked:
            self.message += " · m marks a region"

    def copy_address(self):
        """The address, shown AND copied.

        Shown because the copy cannot always be confirmed — selecting it
        with the mouse is the fallback that is always there, and it needs
        the address on the screen to select."""
        at = self.address()
        if at is None:
            self.message = ("this entry is at a live edge, so it has no "
                            "address yet — no chunk holds it")
            return
        self.message = f"{at} · {to_clipboard(str(at))}"

    # -- search
    def search(self, token):
        if not token:
            return
        # A term rides the index on the runs INSIDE it, so ONE indexable
        # run is enough — which is what makes a UUID searchable whole.
        if not indexable(token):
            self.message = why_not_a_term(token)
            self.hits, self.hit = [], -1
            return
        try:
            self.hits, unplaced, capped = self.backend.search(token)
        except Refused as e:
            self.message = str(e)
            return
        self.hit = -1
        self.term = token
        notes = []
        if capped:
            notes.append("stopped at a bound, so there may be more")
        if unplaced:
            notes.append(f"{unplaced} not in a chunk yet, so opened by their "
                         f"write window")
        stale = getattr(self.backend, "stale", {})
        if stale:
            notes.append(
                "opened by write window from "
                + ", ".join(f"{h or '(local)'} on "
                                   f"{'.'.join(map(str, v))}"
                                   for h, v in sorted(stale.items(),
                                                      key=lambda x: x[0] or ""))
                + f", which cannot say where an entry is — "
                  f"{'.'.join(map(str, PLACED_FROM))} can")
        bad = getattr(self.backend, "unreachable", {})
        if bad:
            notes.append("not searched: "
                         + ", ".join(sorted(h or "(local)" for h in bad)))
        tail = f"  ({'; '.join(notes)})" if notes else ""
        self.message = (f"{len(self.hits)} hit(s) for {token!r}{tail}"
                        if self.hits else f"no hit for {token!r}{tail}")

    def jump(self, hit):
        """Open where a hit is, switching store and host if that is where
        it turned out to be.

        An exact offset is a seek to the line. Without one — a target too
        old to give it, or an entry not yet in a chunk — the write window
        is a seek to the CHUNK, which is ten screenfuls around it, and
        the line is then found in what came back."""
        if hit.address.kind != "offset" and hit.wf is None:
            self.message = ("that hit says neither where nor when it is, so "
                            "there is nothing to open it by")
            return False
        store = hit.store
        if store and store.get("id") != self.store.get("id"):
            self.source = Tape(self.backend, store)
        if hit.address.kind == "offset":
            self.open(offset=hit.address.value)
        else:
            self.open(at=hit.wf)
            self.find(hit.text)
        if self.term:
            for i, (_, _, t) in enumerate(self.line_terms()):
                if t == self.term:
                    self.tok = i
                    self.follow_term = True
                    break
        if hit.address.kind == "offset":
            self.message = str(hit.address)
        else:
            stale = getattr(self.backend, "stale", {}).get(hit.address.host)
            self.message = (
                f"{self.address()} — by its write window, not an exact offset"
                + (f" ({hit.address.host or 'that target'} answered on "
                   f"{'.'.join(map(str, stale))})" if stale
                   else "; it was not in a chunk when the answer was made"))
        return True

    def find(self, text):
        """Put the cursor on the line that IS this entry, in what a
        window-seek brought back. The chunk is the right one; this is
        which line of it."""
        want = (text or "").strip()
        if not want:
            return False
        for i, ln in enumerate(self.source.lines):
            if ln.text.strip() == want:
                self.cur = self.top = i
                self.tok = 0
                return True
        return False

    def cycle(self, step):
        if not self.hits:
            self.message = "no hits to cycle — pick a token and press Enter"
            return
        self.hit = (self.hit + step) % len(self.hits)
        moved = self.jump(self.hits[self.hit])
        self.message = (f"hit {self.hit + 1}/{len(self.hits)}  "
                        + ("" if moved else "not opened: ") + self.message)

    # -- what a screen of this size shows
    def rowcount(self, i, width):
        if not self.wrap:
            return 1
        n = len(self.source.lines[i].text)
        return max(1, -(-n // max(1, width)))

    def _top_for(self, cur, width, height):
        used, i = 0, cur
        while i >= 0:
            used += self.rowcount(i, width)
            if used > height:
                return i + 1
            i -= 1
        return 0

    def layout(self, width, height):
        """The rows to draw: text, which line each came from, and the
        spans to highlight. An edge row is the tape's own boundary, and
        it appears only at the real one — the top of what is LOADED gets
        no marker, because the two are different facts."""
        lines = self.source.lines
        if not lines:
            return [{"text": "  (this store holds no lines)", "line": None,
                     "spans": [], "edge": True}]
        self.top = max(0, min(self.top, len(lines) - 1))
        self.cur = max(0, min(self.cur, len(lines) - 1))
        if self.cur < self.top:
            self.top = self.cur
        if self.follow_term:
            self.bring_term_into_view(width)
            self.follow_term = False
        rows = self._build(width, height)
        seen = [r["line"] for r in rows if r["line"] is not None]
        if seen and self.cur > seen[-1]:
            self.top = self._top_for(self.cur, width, height)
            rows = self._build(width, height)
        # A screen with room to spare and tape above it is a pager that
        # stopped short: back up rather than leave the bottom blank.
        if len(rows) < height and self.top > 0:
            reserve = sum(1 for r in rows if r["edge"])
            self.top = self._top_for(len(lines) - 1, width,
                                     max(1, height - reserve))
            rows = self._build(width, height)
        return rows

    def _build(self, width, height):
        rows, lines = [], self.source.lines
        # A selection you cannot see is not one, so both of them are
        # marked for the drawing: the region, and — since `c` with no
        # mark takes the ENTRY — the entry the cursor is in. On the tape
        # that is the cursor's own line and the screen is unchanged.
        span = self.region()
        here = self.entry_span()
        if self.top == 0:
            for text in (self.source.top_notes() if self.source.at_top()
                         else self.stuck_note(above=True)):
                rows.append({"text": text, "line": None, "spans": [],
                             "edge": True})
        i = self.top
        sel = self.selected()
        while i < len(lines) and len(rows) < height:
            ln = lines[i]
            spans = []
            if i == self.cur:
                for n, (s, e, t) in enumerate(ln.terms):
                    spans.append((s, e, "sel" if (n == self.tok and t == sel)
                                  else "tok"))
            if self.wrap:
                text = ln.text or ""
                pieces = [text[p:p + width] for p in
                          range(0, max(1, len(text)), width)] or [""]
                for k, piece in enumerate(pieces):
                    if len(rows) >= height:
                        break
                    base = k * width
                    rows.append({
                        "text": piece, "line": i,
                        "spans": [(max(0, s - base), min(len(piece), e - base), a)
                                  for s, e, a in spans
                                  if e > base and s < base + width],
                        "edge": False, "cursor": i == self.cur,
                        "region": bool(span) and span[0] <= i <= span[1],
                        "entry": bool(here) and here[0] <= i <= here[1]})
            else:
                text = ln.text[self.col:self.col + width]
                rows.append({
                    "text": text, "line": i,
                    "spans": [(max(0, s - self.col), min(len(text), e - self.col), a)
                              for s, e, a in spans
                              if e > self.col and s < self.col + width],
                    "edge": False, "cursor": i == self.cur,
                    "region": bool(span) and span[0] <= i <= span[1],
                    "entry": bool(here) and here[0] <= i <= here[1]})
            i += 1
        if i >= len(lines) and len(rows) < height:
            if self.source.at_bottom():
                rows.append({"text": self.source.bottom_note(), "line": None,
                             "spans": [], "edge": True})
            else:
                for text in self.stuck_note(above=False):
                    rows.append({"text": text, "line": None, "spans": [],
                                 "edge": True})
        return rows

    def stuck_note(self, above):
        """There is more tape this way and it could not be read. Said
        where the boundary marker would go, because an edge that stops
        without saying why is the same screen as the end of the log."""
        if not self.source.trouble:
            return []
        seq, why = self.source.trouble
        run = self.source.chunks
        if not run or (seq < run[0].seq) != above:
            return []
        return [f"── chunk {seq} could not be read · {why}"]

    def header(self):
        """What the source says it is showing, and where in it."""
        return self.source.describe(self.line())

    def status(self):
        if self.message:
            return self.message
        sel = self.selected()
        bits = ["q quit", "Tab term", "Enter search",
                "o log around" if isinstance(self.source, Records)
                else "n/N hits",
                "w wrap" if not self.wrap else "w nowrap", "S stores",
                "m mark", "c copy", "? help"]
        if sel:
            bits.insert(0, f"[{sel}]")
        # A region can be longer than the screen, so its size is said
        # rather than left to be counted off the rows that happen to show.
        span = self.region()
        if span:
            rows = span[1] - span[0] + 1
            bits.insert(0, f"region {rows} row{'' if rows == 1 else 's'} — "
                           "c copies · x the other end · ^G drops it")
        return "  ".join(bits)


# -------------------------------------------------------------- screen
KEY_TAB, KEY_ESC, KEY_CR, KEY_LF = 9, 27, 13, 10
# What a terminal sends for Ctrl-Space and Ctrl-G: the set-mark and the
# cancel a hand that has used emacs reaches for. `m` is the key that is
# always there — a terminal is free to send nothing for a modifier.
KEY_NUL, KEY_BEL = 0, 7


HELP_KEYS = """
  j k  ↑ ↓        a line              g G           the log's top / end
  space b        a page              h l  ← →      sideways (no wrap)
  ⇧↓ ⇧↑          an ENTRY, over its continuation lines — a stack trace is
   (J K)         one entry and forty lines. The arrow keeps the hand where
                 the line motion already is; J/K is the same thing for a
                 terminal that sends no shifted arrow. On the tape every
                 line is its own, since it parses nothing
  z              a multi-line entry as ONE row — a stack trace beside the
                 message that raised it, so ten of them can be compared
                 at all. Nothing is hidden: h/l read along it
  t              the investigation's window — the tape stops at it
                 rather than scrolling out of the period you are looking
                 at. `from T to T`, or `none`
  w              wrap / no wrap

  m  ^Space      SET THE MARK here; again drops it (^G too). x swaps the
                 mark and the cursor, so the far end of a long region can
                 be looked at
  c              COPY — the region if a mark is set, else the whole ENTRY
                 under the cursor, which is the forty lines of a stack
                 trace and not the one frame the cursor is on. Joined
                 rows copy as the log's own lines, not with the ↵ in

                 What is on the screen says which is which: the region is
                 the REVERSED block, and the entry a bare c would take is
                 the BOLD one — so what a copy will take is visible before
                 it is made. On the tape every line is its own entry, so
                 there the bold is the line you are on
  y              this line's address, shown AND copied

  Tab  ⇧Tab      the searchable terms, and on past the end of a line to
                 the next one that has any — an identifier is ONE of
                 them, separators and all
  Enter  *       search the picked one, everywhere
  /              search a term you type
  n  N           the next / previous hit

  In the hit list the same keys work on the hits themselves: Tab walks
  the terms of the highlighted one and * searches it, so the identifier
  you are really after can be followed without opening anything. Enter
  there still means go to that hit.

  S              another store       ?             this      q  quit

  A hit is an address: timber://host/store-id#offset=N, which is where
  it is and what you paste into a ticket.

  A copy goes to a clipboard helper where there is a display for one, to
  the TERMINAL by OSC 52 otherwise — which is the route that crosses an
  ssh, and whose failure is silence, so the status line says which was
  used. Where neither answers it is written to a file, named there.
"""


class Screen:
    """The terminal half. Every key is one call into `View`, so nothing
    here decides anything the model does not."""

    def __init__(self, view):
        self.view = view

    SPIN = "|/-\\"

    def busy(self, stdscr, what, fn):
        """Run a round trip on a worker and keep saying so.

        A pager over a fleet spends real time waiting, and a screen that
        stops answering is indistinguishable from one that has hung. It
        redraws only the STATUS line — the model is being mutated on the
        other thread, and reading it here to redraw would be reading it
        half-built.

        Interrupting gives up on the answer rather than the session: the
        subprocess is left to finish into nothing, which beats a viewer
        that cannot be got out of when a host stops responding."""
        box, done = {}, threading.Event()

        def work():
            try:
                box["value"] = fn()
            except BaseException as e:            # noqa: BLE001 — re-raised below
                box["error"] = e
            finally:
                done.set()

        threading.Thread(target=work, daemon=True).start()
        began, n = time.monotonic(), 0
        stdscr.timeout(120)
        try:
            while not done.is_set():
                waited = time.monotonic() - began
                # Nothing is drawn for a fast answer: a flash of "waiting"
                # on every keystroke is its own kind of noise.
                if waited > 0.3:
                    h, _ = stdscr.getmaxyx()
                    self.put(stdscr, h - 1,
                             f" {self.SPIN[n % len(self.SPIN)]}  {what}"
                             f"   {waited:.1f}s   ^C gives up",
                             self.curses.A_REVERSE)
                    stdscr.refresh()
                    n += 1
                stdscr.getch()          # paces the loop, and eats held keys
        except KeyboardInterrupt:
            # Anywhere in the loop, not just in the read: an interrupt
            # gives up on the ANSWER rather than on the session.
            raise Refused(f"gave up waiting for {what}") from None
        finally:
            stdscr.timeout(-1)
        if "error" in box:
            raise box["error"]
        return box.get("value")

    def setup(self, stdscr):
        import curses
        curses.curs_set(0)
        stdscr.keypad(True)
        self.curses = curses

    def run(self, stdscr):
        self.setup(stdscr)
        return self.loop(stdscr)

    def loop(self, stdscr):
        while True:
            self.draw(stdscr)
            try:
                key = stdscr.getch()
            except KeyboardInterrupt:
                return self.view.address()
            if self.step(stdscr, key) is False:
                return self.view.address()

    def attr(self, kind):
        c = self.curses
        return {"tok": c.A_UNDERLINE, "sel": c.A_REVERSE}[kind]

    def draw(self, stdscr):
        c = self.curses
        h, w = stdscr.getmaxyx()
        body = max(1, h - 2)
        v = self.view
        stdscr.erase()
        self.put(stdscr, 0, v.header()[:w - 1], c.A_REVERSE)
        for y, row in enumerate(v.layout(w - 1, body), start=1):
            base = c.A_DIM if row["edge"] else (
                c.A_BOLD if row.get("entry") or row.get("cursor") else 0)
            text = row["text"][:w - 1]
            if row.get("region"):
                base |= c.A_REVERSE
                # PADDED to the width: a bar that stops where the text
                # does is a ragged edge that reads as chrome, and the
                # header and status lines are already reverse. A solid
                # block is what a selection looks like.
                text = text.ljust(w - 1)
            self.put(stdscr, y, text, base)
            for s, e, kind in row["spans"]:
                if 0 <= s < e <= w - 1:
                    try:
                        # Inside the block the picked term is punched OUT
                        # of it rather than reversed again, which would
                        # be the same attribute and so invisible.
                        stdscr.chgat(y, s, e - s,
                                     self.attr(kind) ^ base if base & c.A_REVERSE
                                     else self.attr(kind) | base)
                    except c.error:
                        pass
        self.put(stdscr, h - 1, v.status()[:w - 1], c.A_REVERSE)
        stdscr.refresh()

    def put(self, win, y, text, attr=0):
        try:
            win.addnstr(y, 0, text, win.getmaxyx()[1] - 1, attr)
        except self.curses.error:
            pass

    def step(self, stdscr, key):
        c, v = self.curses, self.view
        h, w = stdscr.getmaxyx()
        v.message = ""
        if key in (ord("q"), KEY_ESC):
            return False
        elif key in (ord("j"), c.KEY_DOWN):
            self.scroll(stdscr, lambda: v.move(1))
        elif key in (ord("k"), c.KEY_UP):
            self.scroll(stdscr, lambda: v.move(-1))
        elif key in (ord(" "), c.KEY_NPAGE, ord("f")):
            self.scroll(stdscr, lambda: v.page(1, h - 2))
        elif key in (ord("b"), c.KEY_PPAGE):
            self.scroll(stdscr, lambda: v.page(-1, h - 2))
        elif key in (c.KEY_SF, ord("J")):
            self.scroll(stdscr, lambda: v.move_entry(1))
        elif key in (c.KEY_SR, ord("K")):
            self.scroll(stdscr, lambda: v.move_entry(-1))
        elif key == ord("g"):
            self.reach(stdscr, "the top of the log", v.home)
        elif key == ord("G"):
            self.reach(stdscr, "the end of the log", v.end)
        elif key in (ord("h"), c.KEY_LEFT):
            v.scroll_h(-8)
        elif key in (ord("l"), c.KEY_RIGHT):
            v.scroll_h(8)
        elif key == ord("t"):
            self.set_window(stdscr)
        elif key == ord("z"):
            v.join_entries()
        elif key == ord("w"):
            v.toggle_wrap()
        elif key == KEY_TAB:
            v.pick(1)
        elif key in (c.KEY_BTAB, ord("p")):
            v.pick(-1)
        elif key in (KEY_CR, KEY_LF, c.KEY_ENTER, ord("*")):
            # The same thing on every pager screen, answer or tape:
            # picking a term and pressing Enter searches it. Making it
            # depend on the source is exactly the surprise a mode is.
            self.do_search(stdscr, v.selected())
        elif key == ord("o"):
            self.reach(stdscr, "the log around this entry",
                       v.leave_for_the_log)
        elif key == ord("/"):
            self.do_search(stdscr, self.prompt(stdscr, "search token: "))
        elif key == ord("n"):
            v.cycle(1)
        elif key == ord("N"):
            v.cycle(-1)
        elif key == ord("S"):
            self.pick_store(stdscr)
        elif key in (ord("m"), KEY_NUL):
            v.set_mark()
        elif key == KEY_BEL and v.mark is not None:
            v.set_mark()                        # ^G cancels the region
        elif key == ord("x"):
            self.scroll(stdscr, v.exchange_mark)
        elif key == ord("c"):
            v.copy()
        elif key == ord("y"):
            v.copy_address()
        elif key == ord("?"):
            self.help(stdscr)
        elif key == c.KEY_RESIZE or key == 12:
            stdscr.clear()
        return True

    def reach(self, stdscr, what, fn):
        """A move that is a seek, so it says which one it is waiting on."""
        try:
            self.busy(stdscr, f"{what} · {self.view.store.get('name')}", fn)
        except Refused as e:
            self.view.message = str(e)

    def scroll(self, stdscr, fn):
        """A move that MIGHT reach for a chunk. Usually it is local and
        nothing is said; where the run has to grow, the read shows up as
        the wait it is rather than as a frozen screen."""
        v = self.view
        try:
            self.busy(stdscr, f"reading {v.store.get('name')}"
                      + (f" on {v.store['_host']}" if v.store.get("_host") else ""),
                      fn)
        except Refused as e:
            v.message = str(e)

    def prompt(self, stdscr, label):
        c = self.curses
        h, w = stdscr.getmaxyx()
        c.echo()
        c.curs_set(1)
        try:
            self.put(stdscr, h - 1, label + " " * (w - 1 - len(label)),
                     c.A_REVERSE)
            stdscr.move(h - 1, len(label))
            return stdscr.getstr(h - 1, len(label), 64).decode(
                "utf-8", "replace").strip()
        except Exception:
            return ""
        finally:
            c.noecho()
            c.curs_set(0)

    def set_window(self, stdscr):
        """Change the investigation's window from inside the pager.

        A SEPARATE act, deliberately, and not something scrolling can do
        by accident: the window is what makes the set in front of you
        complete, so a bound you can drift out of is not one you can sort
        or count against. Changing it is therefore typed, once.

        `from T to T`, either end droppable, and empty to leave it."""
        v = self.view
        if not hasattr(v.source, "window"):
            v.message = ("this is an answer, already bounded when it was "
                         "fetched — `create session ...` in the shell, then "
                         "ask again")
            return
        now = v.source.window or (None, None)
        shown = " ".join(
            p for p in (f"from {when_ms(now[0])}" if now[0] else "",
                        f"to {when_ms(now[1])}" if now[1] else "") if p)
        typed = self.prompt(stdscr, f"window [{shown or 'unbounded'}]: ")
        if not typed:
            return
        try:
            v.set_window(typed)
        except ValueError as e:
            v.message = str(e)
            return
        self.reach(stdscr, "that window", lambda: v.open(at=v.source.window[0]))

    def do_search(self, stdscr, token):
        """Search, show the hits, and let a term picked out of THEM be
        the next search. Following an identifier is rarely one hop, and
        the second one is usually visible in the answer to the first."""
        v = self.view
        if not token:
            v.message = v.nothing_pickable()
            return
        while token:
            try:
                self.busy(stdscr, f"searching {len(v.backend.hosts)} "
                                  f"target(s) for {token}",
                          lambda t=token: v.search(t))
            except Refused as e:
                v.message = str(e)
                return
            if not v.hits:
                return
            # A list, because an identifier on six hosts is six answers
            # and jumping to one of them silently picks for you.
            what, value = self.choose(
                stdscr, f"{token} — {len(v.hits)} hit(s)",
                [self.hit_row(x) for x in v.hits],
                searchable=True, terms_from=self.HIT_PREFIX)
            if what == "term":
                token = value
                continue
            if what == "open":
                v.hit = value
                self.reach(stdscr, f"hit {value + 1}",
                           lambda: v.jump(v.hits[value]))
                v.message = f"hit {value + 1}/{len(v.hits)}  {v.message}"
            return

    # Where a hit row's TEXT begins. Fixed, and truncated to it, so the
    # terms offered are the log line's and never the store's name.
    HIT_PREFIX = 29

    def hit_row(self, hit):
        where = (hit.store or {}).get("name", "?")
        if hit.address.host:
            where += f" @ {hit.address.host}"
        # A hit with no position is readable and not openable, and the
        # list says which is which rather than letting Enter find out.
        # `·` reads as "by its write window": the right chunk, found by
        # when rather than where.
        mark = " " if hit.address.kind == "offset" else "·"
        return f"{mark}{where[:self.HIT_PREFIX - 2]:{self.HIT_PREFIX - 2}} " \
               f"{hit.text[:200]}"

    def pick_store(self, stdscr):
        v = self.view
        try:
            stores = self.busy(stdscr, "the store list",
                               self.view.backend.stores)
        except Refused as e:
            v.message = str(e)
            return
        if not stores:
            v.message = "no stores"
            return
        rows = []
        for s in stores:
            name = s.get("name", "?")
            if s.get("_host"):
                name += f" @ {s['_host']}"
            rows.append(f"{name:32} {s.get('chunks', 0):>8} chunk(s)  "
                        f"{human(s.get('logical_bytes', 0))}")
        bad = getattr(self.view.backend, "unreachable", {})
        title = "stores" if not bad else (
            "stores  ⚠ no answer from "
            + ", ".join(sorted(h or "(local)" for h in bad)))
        what, i = self.choose(stdscr, title, rows)
        if what != "open":
            return
        v.tape = Tape(v.backend, stores[i])
        self.reach(stdscr, stores[i].get("name", "that store"), v.open)

    def help(self, stdscr):
        """The keys, on the pager's own motions.

        They stopped fitting a 24-row terminal, and a help screen that
        silently ends at the bottom of one loses whichever keys are last
        — the same failure as an edge that stops without saying why. So
        it scrolls, and says when there is more."""
        c = self.curses
        lines = HELP_KEYS.strip("\n").splitlines()
        top = 0
        while True:
            h, _ = stdscr.getmaxyx()
            body = max(1, h - 1)
            top = max(0, min(top, len(lines) - body))
            stdscr.erase()
            for y, line in enumerate(lines[top:top + body]):
                self.put(stdscr, y, line)
            left = len(lines) - (top + body)
            note = ("  any key to go back" if not left and not top
                    else f"  {left} more below · j k space b · "
                         "any other key to go back" if left > 0
                    else "  j k space b to go back up · "
                         "any other key to leave")
            self.put(stdscr, h - 1, note, c.A_REVERSE)
            stdscr.refresh()
            key = stdscr.getch()
            if key in (ord("j"), c.KEY_DOWN):
                top += 1
            elif key in (ord("k"), c.KEY_UP):
                top -= 1
            elif key in (ord(" "), c.KEY_NPAGE, ord("f")):
                top += body
            elif key in (ord("b"), c.KEY_PPAGE):
                top -= body
            elif key != c.KEY_RESIZE:
                return

    def choose(self, stdscr, title, rows, searchable=False, terms_from=0):
        """Pick a row, or — where the rows are hits — a TERM out of one.

        A hit list is text you are reading, so the identifier you are
        really after is often sitting in it: `Tab` walks the terms of
        the highlighted row and `*` searches the picked one, without
        first opening the hit to get at it. `Enter` still means the
        primary thing here, which is going there.

        Answers `(what, value)`: `("open", index)`, `("term", text)`,
        or `(None, None)` for a cancel."""
        c = self.curses
        sel, top, tok, col = 0, 0, 0, 0
        while True:
            h, w = stdscr.getmaxyx()
            body = max(1, h - 2)
            top = min(top, sel)
            if sel >= top + body:
                top = sel - body + 1
            picked = (self.term_of(rows[sel][terms_from:], tok)
                      if searchable else None)
            # The pick moves sideways and the window follows it, exactly
            # as it does on the pager. A hit line is often long — a fleet
            # path, a store name and the entry — so a term picked past the
            # edge was highlighted nowhere and only named in the footer.
            found = terms(rows[sel][terms_from:]) if searchable else []
            if found:
                a, b, _t = found[tok % len(found)]
                col = scrolled_to((a + terms_from, b + terms_from), col, w - 1)
            stdscr.erase()
            self.put(stdscr, 0, f"── {title}", c.A_REVERSE)
            for y, i in enumerate(range(top, min(len(rows), top + body)),
                                  start=1):
                base = c.A_REVERSE if i == sel else 0
                self.put(stdscr, y, rows[i][col:], base)
                if searchable and i == sel:
                    for n, (a, b, _t) in enumerate(found):
                        a, b = a + terms_from - col, b + terms_from - col
                        if 0 <= a and b <= w - 1:
                            try:
                                stdscr.chgat(y, a, b - a, base | (
                                    c.A_BOLD if n == tok % max(1, len(found))
                                    else c.A_UNDERLINE))
                            except c.error:
                                pass
            # The picked term goes FIRST: on a narrow screen the key
            # hints are what can be spared, and it is not.
            foot = f"  [{picked}]  " if picked else "  "
            foot += "Enter open   j/k move   q back"
            if searchable:
                foot += "   Tab term   * search it   / type one"
            self.put(stdscr, h - 1, foot, c.A_REVERSE)
            stdscr.refresh()
            key = stdscr.getch()
            if key in (ord("q"), KEY_ESC):
                return None, None
            if key in (KEY_CR, KEY_LF, c.KEY_ENTER):
                return "open", sel
            if searchable and key == KEY_TAB:
                tok += 1
            elif searchable and key in (c.KEY_BTAB, ord("p")):
                tok -= 1
            elif searchable and key == ord("*"):
                if picked:
                    return "term", picked
            elif searchable and key == ord("/"):
                typed = self.prompt(stdscr, "search term: ")
                if typed:
                    return "term", typed
            elif key in (ord("j"), c.KEY_DOWN):
                sel, tok, col = min(len(rows) - 1, sel + 1), 0, 0
            elif key in (ord("k"), c.KEY_UP):
                sel, tok, col = max(0, sel - 1), 0, 0
            elif key in (ord(" "), c.KEY_NPAGE):
                sel, tok, col = min(len(rows) - 1, sel + body), 0, 0
            elif key == c.KEY_PPAGE:
                sel, tok, col = max(0, sel - body), 0, 0

    @staticmethod
    def term_of(row, n):
        found = terms(row)
        return found[n % len(found)][2] if found else None




def watch_source(backend, source, what=None):
    """Open the viewer on a source that is already read — a result set.

    Nothing is fetched here, so there is no first read to explain and
    the screen is up immediately."""
    import curses
    view = View(backend, source=source)
    view.cur = view.top = 0
    screen = Screen(view)
    return curses.wrapper(lambda stdscr: (screen.setup(stdscr),
                                          screen.loop(stdscr))[1])


def watch(backend, store, seq=None, at=None, offset=None, window=None):
    """Open the viewer and return the address it was left at.

    The FIRST read happens inside curses, because it is the slowest one
    and the one with nothing on screen yet to explain it. A failure
    there still unwinds the terminal and reaches the caller intact —
    which matters for the refusals that are several lines long."""
    import curses
    view = View(backend, source=Tape(backend, store, window))
    screen = Screen(view)
    where = view.store.get("name") or "the store"
    if view.store.get("_host"):
        where += f" on {view.store['_host']}"

    def go(stdscr):
        screen.setup(stdscr)
        screen.busy(stdscr, f"opening {where}",
                    lambda: view.open(seq=seq, at=at, offset=offset))
        return screen.loop(stdscr)

    return curses.wrapper(go)


# ------------------------------------------------------------ opening
def resolve(backend, target):
    """A store, from whatever was typed: an address, a name, a short id,
    or a path. Ambiguity is refused rather than picked from."""
    stores = backend.stores()
    bad = getattr(backend, "unreachable", {})
    if not stores:
        if bad:
            raise Refused(
                f"{len(bad)} target(s) did not answer, so nothing was "
                "listed:\n      "
                + "\n      ".join(w for _, w in
                                   sorted(bad.items(), key=lambda x: x[0] or "")))
        raise Refused("no store to view")
    missed = ("" if not bad else
              f" ({len(bad)} target(s) did not answer: "
              + ", ".join(sorted(h or "(local)" for h in bad)) + ")")
    if target is None:
        return stores[0]
    if target.startswith(f"{SCHEME}:"):
        addr = parse_address(target)
        hit = [s for s in stores if s.get("id") == addr.store]
        if not hit:
            raise Refused(f"no store here has id {addr.store}"
                          + (f" (the address says {addr.host})"
                             if addr.host else "") + missed)
        return hit[0]
    t = target.lower()
    for pick in (lambda s: (s.get("name") or "").lower() == t,
                 lambda s: (s.get("path") or "").lower() == t,
                 lambda s: os.path.basename(s.get("path") or "").lower() == t,
                 lambda s: (s.get("id") or "").lower().startswith(t),
                 lambda s: t in (s.get("name") or "").lower()):
        hit = [s for s in stores if pick(s)]
        if len(hit) == 1:
            return hit[0]
        if len(hit) > 1:
            raise Refused(
                f"{target!r} matches {len(hit)} stores: "
                + ", ".join(sorted(s.get("name", "?") for s in hit)))
    raise Refused(f"no store matches {target!r} — {len(stores)} here{missed}")


def position(target=None, at=None, chunk=None):
    """The three coordinates, as keyword arguments for `open`. An
    address carries its own; the flags are the other way to say it."""
    if target and target.startswith(f"{SCHEME}:"):
        addr = parse_address(target)
        if addr.kind == "offset":
            return {"offset": addr.value}
        if addr.kind == "chunk":
            return {"seq": addr.value}
        if addr.kind == "at":
            return {"at": when(str(addr.value))}
    if at is not None:
        return {"at": when(at) if isinstance(at, str) else at}
    if chunk is not None:
        return {"seq": int(chunk)}
    return {}


def main(argv=None):
    import argparse
    ap = argparse.ArgumentParser(
        prog="timberview", add_help=True,
        formatter_class=argparse.RawDescriptionHelpFormatter,
        description="timberview — read a timberfs store as a tape.",
        epilog="TARGET is a store name, a path, a short id, or a "
               f"{SCHEME}: address.\nWith none, the first store is opened "
               "at its last chunk; S switches.")
    ap.add_argument("target", metavar="TARGET", nargs="?")
    ap.add_argument("--resolver", metavar="CMD",
                    help="a command that prints the fleet as a target "
                         "document. $TIMBERFS_RESOLVER")
    ap.add_argument("--targets", metavar="FILE",
                    help="the same document, from a file. $TIMBERFS_TARGETS; "
                         "else ~/.config/timberfs/targets.json, else "
                         "/etc/timberfs/targets.json")
    ap.add_argument("--cmd", metavar="ARGV",
                    help="one command reaching every host, with "
                         "_TIMBERHOST_ substituted per --hosts. Default "
                         "`timberfs query --query -`. $TIMBERFS_CMD")
    ap.add_argument("--hosts", metavar="H,H",
                    help="the hosts that command reaches. $TIMBERFS_HOSTS")
    ap.add_argument("--at", metavar="TIME",
                    help="open at the chunk covering this instant")
    ap.add_argument("--chunk", metavar="N", help="open at this chunk number")
    a = ap.parse_args(argv)

    try:
        fleet = timberfs_client.resolve(a.resolver, a.targets, a.cmd, a.hosts)
    except ValueError as e:
        sys.exit(f"timberview: {e}")
    for name, why in fleet.unusable:
        print(f"timberview: {name or '(local)'} was not asked: {why}",
              file=sys.stderr)

    def ask(doc, host=None, raw=False):
        t = fleet.by_name(host)
        if t is None:
            return (b"" if raw else ""), f"no target named {host!r}", 127
        try:
            p = subprocess.run(t.cmd, input=json.dumps(doc).encode(),
                               capture_output=True)
        except OSError as e:
            return (b"" if raw else ""), str(e), 127
        err = p.stderr.decode("utf-8", "replace").strip()
        return (p.stdout if raw
                else p.stdout.decode("utf-8", "replace")), err, p.returncode

    backend = QueryBackend(ask, fleet.names)

    # A `records` stream on stdin is an ANSWER to page, not a store.
    if not sys.stdin.isatty():
        stream = sys.stdin.buffer.read()
        # The keyboard has to come from somewhere else: stdin is the
        # answer, so curses would read the log as keystrokes. Every
        # pager does this, and a session with no controlling terminal
        # is told rather than left with a screen it cannot drive.
        try:
            tty = open("/dev/tty", "rb")
        except OSError as e:
            sys.exit(f"timberview: reading an answer from a pipe needs a "
                     f"terminal to take keys from, and /dev/tty is not "
                     f"open to this session ({e})")
        os.dup2(tty.fileno(), 0)
        answer = Records(stream, stores=backend.stores() if a.hosts or a.cmd
                         or a.resolver or a.targets else [],
                         what=a.target or "an answer on stdin")
        if not answer.entries:
            sys.exit("timberview: that stream carried no entry — `--records` "
                     "is the form that does")
        print(watch_source(backend, answer) or "")
        return 0

    try:
        store = resolve(backend, a.target)
        addr = watch(backend, store,
                     **position(a.target, a.at, a.chunk))
    except Refused as e:
        sys.exit(f"timberview: {e}")
    except ValueError as e:
        sys.exit(f"timberview: {e}")
    print(addr)
    return 0
