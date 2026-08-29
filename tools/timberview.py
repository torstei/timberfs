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
import json
import os
import re
import subprocess
import sys

import timberfs_client
from timberfs_client import when                              # noqa: F401

DOC_V = "1.0-EXPERIMENTAL"

# What `--has` matches, and therefore what can be selected: the index
# holds maximal ASCII-alphanumeric runs of 3..=64 bytes, exact case.
# Highlighting anything else would offer a search that cannot be run.
MIN_TOKEN, MAX_TOKEN = 3, 64
RUN = re.compile(r"[A-Za-z0-9]+")

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


def frames(buf):
    """`timberfs-records(5)` over BYTES. A record is RS, fields, NUL —
    and where a `len` field says so, that many payload bytes and a NUL.
    The payload is read by length rather than scanned for, because a
    compressed frame contains every byte value including the separators."""
    i = 0
    while True:
        i = buf.find(b"\x1e", i)
        if i < 0:
            return
        j = buf.find(b"\0", i)
        if j < 0:
            return
        head = buf[i + 1:j].split(b"\x1f")
        fields = {}
        for p in head[1:]:
            k, _, v = p.partition(b"=")
            fields[k.decode("utf-8", "replace")] = v.decode("utf-8", "replace")
        i = j + 1
        payload = None
        if "len" in fields:
            n = int(fields["len"])
            payload = buf[i:i + n]
            i += n + 1
        yield head[0].decode("utf-8", "replace"), fields, payload


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
    """A search result, which is an address plus enough to recognise it."""

    def __init__(self, address, store, text, ts=None):
        self.address, self.store, self.text, self.ts = address, store, text, ts


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

    def __init__(self, ask, hosts=(None,)):
        self.ask, self.hosts = ask, list(hosts)
        self._stores = None
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

    def select_stores(self, terms, remember=False):
        doc = {"v": DOC_V, "stores": {"select": list(terms)},
               "response_format": {"kind": "stores"}}
        got = []
        for host in self.hosts:
            try:
                payload = json.loads(self._run(doc, host) or "[]")
            except (Refused, json.JSONDecodeError) as e:
                self.unreachable[host] = str(e)
                continue
            self.unreachable.pop(host, None)
            found = (payload.get("stores", []) if isinstance(payload, dict)
                     else payload)
            for st in found:
                st["_host"] = host
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
        return found[0]

    def chunk(self, store, seq=None, at=None):
        """One chunk, by number or by the instant it covers.

        `max: {chunks: 1}` beside a start is a seek, and a pager is
        nothing but seeks."""
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
                return Chunk(int(f["chunk"]), int(f["uncomp_start"]),
                             int(f["uncomp_len"]), int(f.get("wf", 0)),
                             int(f.get("wl", 0)), unzstd(payload))
        return None

    def search(self, token, limit=200):
        """Where that token appears, as addresses.

        Every store on every host, because the loop this exists for
        starts with an identifier and no idea which log holds it."""
        doc = {"v": DOC_V, "stores": {"select": []},
               "match": {"granularity": "entries", "all": [{"has": token}]},
               "max": {"entries": limit},
               "response_format": {"kind": "records"}}
        by_id = {s.get("id"): s for s in self.stores()}
        hits, unplaced, capped = [], 0, False
        for host in self.hosts:
            try:
                out = self._run(doc, host, raw=True)
            except Refused as e:
                self.unreachable[host] = str(e)
                continue
            self.unreachable.pop(host, None)
            # An entry names its store only where the read spanned
            # several, so with one source the `source` record is the
            # attribution and the entries inherit it.
            sources = []
            for kind, f, payload in frames(out):
                if kind == "source":
                    sources.append(f.get("id"))
                    continue
                if kind == "stream-end":
                    capped = capped or f.get("status") == "limited"
                    continue
                if kind != "entry":
                    continue
                if "chunk" not in f or "offset" not in f:
                    # The live edge: real, and not in a chunk yet, so
                    # there is no coordinate to hand back.
                    unplaced += 1
                    continue
                sid = f.get("id") or (sources[0] if len(sources) == 1 else None)
                store = by_id.get(sid)
                text = (payload or b"").decode("utf-8", "replace")
                hits.append(Hit(
                    Address(sid, host, "offset", int(f["offset"])),
                    store, text.split("\n")[0].rstrip(),
                    int(f["ts"]) if f.get("ts") else None))
        return hits, unplaced, capped


# ---------------------------------------------------------------- text
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


def tokens(text):
    """The searchable runs, as (start, end, text). Maximal runs, so a
    65-character one yields nothing rather than its first 64 — that is
    what the index does, and offering the prefix would offer a search
    that finds a different thing."""
    found = []
    for m in RUN.finditer(text):
        if MIN_TOKEN <= m.end() - m.start() <= MAX_TOKEN:
            found.append((m.start(), m.end(), m.group()))
    return found


def why_not_a_token(word):
    """Why the index cannot hold this one. `26.1.18` is the case: it is
    three runs of one and two characters, so it is refused where you
    point at it rather than discovered later as an empty answer."""
    runs = [m.group() for m in RUN.finditer(word)]
    if not runs:
        return (f"{word!r} has no letters or digits in it — the index "
                f"holds runs of {MIN_TOKEN}-{MAX_TOKEN} of them")
    if all(len(r) < MIN_TOKEN for r in runs):
        return (f"{word!r} is {len(runs)} run(s) of under {MIN_TOKEN} "
                f"characters ({', '.join(runs)}) — the index holds none "
                f"of them, so no search can find it")
    return (f"{word!r} is not one token: search "
            + " or ".join(repr(r) for r in runs
                          if MIN_TOKEN <= len(r) <= MAX_TOKEN))


def human(n):
    for u in ("B", "KiB", "MiB", "GiB", "TiB"):
        if n < 1024 or u == "TiB":
            return f"{n:.0f} {u}" if u == "B" else f"{n:.1f} {u}"
        n /= 1024


def grouped(n):
    return f"{n:,}".replace(",", " ")


class Line:
    __slots__ = ("offset", "text", "_tokens")

    def __init__(self, offset, raw):
        self.offset = offset
        self.text = sanitise(raw.decode("utf-8", "replace"))
        self._tokens = None

    @property
    def tokens(self):
        if self._tokens is None:
            self._tokens = tokens(self.text)
        return self._tokens


# ---------------------------------------------------------------- tape
class Tape:
    """A contiguous run of chunks, as lines with absolute offsets.

    One chunk is ten screenfuls for a few KiB on the wire, so the run is
    held either side of where you are and scrolling never falls out of a
    window. A line that straddles a chunk boundary is held back while
    there is more tape on that side: showing half of it would be a
    truncation the reader cannot see."""

    KEEP = 6

    def __init__(self, backend, store):
        self.backend, self.store = backend, store
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
        fresh = self.backend.bounds(self.store)
        if fresh:
            self.store = fresh

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
        self.extend_up()
        self.extend_down()
        return c

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
        return bool(self.chunks) and self.chunks[0].seq == self.first_seq

    def at_bottom(self):
        return bool(self.chunks) and self.chunks[-1].seq == self.last_seq

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
        return True

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


# ---------------------------------------------------------------- view
NEAR = 200          # lines from an edge at which the next chunk is fetched


class View:
    """Where the reader is, and what a screen of that size would show.

    Free of curses on purpose: this is the half worth testing, and it is
    tested against a fake backend rather than a terminal."""

    def __init__(self, backend, store):
        self.backend = backend
        self.tape = Tape(backend, store)
        self.wrap = False
        self.col = 0
        self.top = self.cur = 0
        self.tok = 0
        self.message = ""
        self.hits, self.hit = [], -1
        self.term = None

    # -- opening
    def open(self, seq=None, at=None, offset=None):
        # A pair with no manifest is not a store, so there is no id to
        # write a place in it as — and an address is the point.
        if not self.store.get("id"):
            raise Refused(
                f"{self.store.get('name')!r} carries no identity, so a "
                "place in it cannot be written down — `timberfs identity "
                "--mint` makes the pair a store")
        self.tape.refresh()
        c = self.tape.open(seq=seq, at=at, offset=offset)
        if offset is not None:
            self.cur = self.tape.index_of(offset)
        elif self.tape.at_bottom() and seq is None and at is None:
            self.cur = max(0, len(self.tape.lines) - 1)
        else:
            self.cur = self.tape.index_of(c.start)
        self.top = self.cur
        self.tok = 0
        return c

    @property
    def store(self):
        return self.tape.store

    def line(self):
        return self.tape.lines[self.cur] if self.tape.lines else None

    def address(self):
        ln = self.line()
        off = ln.offset if ln else self.tape.tape_start
        return Address(self.store.get("id"), self.store.get("_host"),
                       "offset", off)

    # -- movement. Every mutation of the run keeps the place by OFFSET,
    # because a line number belongs to a run and an offset to the tape.
    def _keep_place(self, fn):
        lines = self.tape.lines
        top_off = lines[self.top].offset if lines else None
        cur_off = lines[self.cur].offset if lines else None
        fn()
        if top_off is not None:
            self.top = self.tape.index_of(top_off)
            self.cur = self.tape.index_of(cur_off)

    def _widen(self):
        if self.cur < NEAR and not self.tape.at_top():
            self._keep_place(self.tape.extend_up)
        if len(self.tape.lines) - self.cur < NEAR and not self.tape.at_bottom():
            self._keep_place(self.tape.extend_down)

    def move(self, n):
        self.cur = max(0, min(len(self.tape.lines) - 1, self.cur + n))
        self.tok = 0
        self._widen()

    def page(self, n, height):
        self.move(n * max(1, height - 2))

    def home(self):
        """The top of the LOG, which is a seek to its first chunk —
        never a walk back through the run. On a store of 400,000 chunks
        those are not the same operation."""
        self.open(seq=self.tape.first_seq)
        self.cur = self.top = 0
        self.tok = 0

    def end(self):
        self.open(seq=self.tape.last_seq)
        self.cur = max(0, len(self.tape.lines) - 1)
        self.tok = 0

    def scroll_h(self, n):
        self.col = max(0, self.col + n)

    def toggle_wrap(self):
        self.wrap = not self.wrap
        self.col = 0

    # -- tokens
    def line_tokens(self):
        ln = self.line()
        return ln.tokens if ln else []

    def pick(self, step):
        """Tab between the selectable tokens, and on past the end of the
        line: what can be picked is exactly what can be searched, so the
        motion never lands anywhere a search cannot follow."""
        toks = self.line_tokens()
        if not toks:
            self.message = self.nothing_pickable()
            return None
        self.tok = (self.tok + step) % len(toks)
        return toks[self.tok][2]

    def selected(self):
        toks = self.line_tokens()
        return toks[self.tok][2] if toks and self.tok < len(toks) else None

    def nothing_pickable(self):
        ln = self.line()
        if not ln or not ln.text.strip():
            return "nothing on this line"
        longest = max(ln.text.split(), key=len)
        return "no searchable token on this line — " + why_not_a_token(longest)

    # -- search
    def search(self, token):
        if not token:
            return
        runs = [m.group() for m in RUN.finditer(token)]
        if len(runs) != 1 or not (MIN_TOKEN <= len(runs[0]) <= MAX_TOKEN) \
                or runs[0] != token:
            self.message = why_not_a_token(token)
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
            notes.append(f"{unplaced} at a live edge, not yet placed")
        bad = getattr(self.backend, "unreachable", {})
        if bad:
            notes.append("not searched: "
                         + ", ".join(sorted(h or "(local)" for h in bad)))
        tail = f"  ({'; '.join(notes)})" if notes else ""
        self.message = (f"{len(self.hits)} hit(s) for {token!r}{tail}"
                        if self.hits else f"no hit for {token!r}{tail}")

    def jump(self, hit):
        """Open where a hit is, switching store and host if that is where
        it turned out to be."""
        store = hit.store
        if store and store.get("id") != self.store.get("id"):
            self.tape = Tape(self.backend, store)
        self.open(offset=hit.address.value)
        if self.term:
            for i, (_, _, t) in enumerate(self.line_tokens()):
                if t == self.term:
                    self.tok = i
                    break
        self.message = str(hit.address)

    def cycle(self, step):
        if not self.hits:
            self.message = "no hits to cycle — pick a token and press Enter"
            return
        self.hit = (self.hit + step) % len(self.hits)
        self.jump(self.hits[self.hit])
        self.message = f"hit {self.hit + 1}/{len(self.hits)}  {self.message}"

    # -- what a screen of this size shows
    def rowcount(self, i, width):
        if not self.wrap:
            return 1
        n = len(self.tape.lines[i].text)
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
        lines = self.tape.lines
        if not lines:
            return [{"text": "  (this store holds no lines)", "line": None,
                     "spans": [], "edge": True}]
        self.top = max(0, min(self.top, len(lines) - 1))
        self.cur = max(0, min(self.cur, len(lines) - 1))
        if self.cur < self.top:
            self.top = self.cur
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
        rows, lines = [], self.tape.lines
        if self.top == 0:
            for text in (self.top_notes() if self.tape.at_top()
                         else self.stuck_note(above=True)):
                rows.append({"text": text, "line": None, "spans": [],
                             "edge": True})
        i = self.top
        sel = self.selected()
        while i < len(lines) and len(rows) < height:
            ln = lines[i]
            spans = []
            if i == self.cur:
                for n, (s, e, t) in enumerate(ln.tokens):
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
                        "edge": False, "cursor": i == self.cur})
            else:
                text = ln.text[self.col:self.col + width]
                rows.append({
                    "text": text, "line": i,
                    "spans": [(max(0, s - self.col), min(len(text), e - self.col), a)
                              for s, e, a in spans
                              if e > self.col and s < self.col + width],
                    "edge": False, "cursor": i == self.cur})
            i += 1
        if i >= len(lines) and len(rows) < height:
            if self.tape.at_bottom():
                rows.append({"text": self.bottom_note(), "line": None,
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
        if not self.tape.trouble:
            return []
        seq, why = self.tape.trouble
        run = self.tape.chunks
        if not run or (seq < run[0].seq) != above:
            return []
        return [f"── chunk {seq} could not be read · {why}"]

    # -- the boundaries, stated rather than discovered
    def top_notes(self):
        s = self.store
        notes = [f"── top of the log · chunk {s.get('first_seq')}"]
        if s.get("dropped_chunks"):
            notes.append(
                f"── {grouped(s['dropped_chunks'])} chunk(s) older were "
                f"dropped ({human(s.get('dropped_uncompressed_bytes', 0))} "
                f"off the tape)")
        return notes

    def bottom_note(self):
        s = self.store
        tail = (f"a writer holds it ({s['writer']}), so newer lines may not "
                "be flushed yet" if s.get("writer")
                else "nothing is appending")
        return f"── end of chunk {s.get('last_seq')} · {tail}"

    def header(self):
        s = self.store
        ln = self.line()
        off = ln.offset if ln else self.tape.tape_start
        span = max(1, self.tape.tape_end - self.tape.tape_start)
        pct = min(100, max(0, int(100 * (off - self.tape.tape_start) / span)))
        where = f"{s.get('name')}"
        if s.get("_host"):
            where += f" @ {s['_host']}"
        return (f"── {where} · chunk {self.tape.chunk_of(off)} · "
                f"offset {grouped(off)} · {pct}%")

    def status(self):
        if self.message:
            return self.message
        sel = self.selected()
        bits = ["q quit", "Tab token", "Enter search", "n/N hits",
                "w wrap" if not self.wrap else "w nowrap", "S stores",
                "? help"]
        if sel:
            bits.insert(0, f"[{sel}]")
        return "  ".join(bits)


# -------------------------------------------------------------- screen
KEY_TAB, KEY_ESC, KEY_CR, KEY_LF = 9, 27, 13, 10


HELP_KEYS = """
  j k  ↑ ↓        a line              g G           the log's top / end
  space b        a page              h l  ← →      sideways (no wrap)
  w              wrap / no wrap      y             this line's address

  Tab  ⇧Tab      the searchable tokens on this line — the runs of 3-64
                 letters or digits the index holds, and nothing else
  Enter  *       search the picked one, everywhere
  /              search a token you type
  n  N           the next / previous hit

  S              another store       ?             this      q  quit

  A hit is an address: timber://host/store-id#offset=N, which is where
  it is and what you paste into a ticket.
"""


class Screen:
    """The terminal half. Every key is one call into `View`, so nothing
    here decides anything the model does not."""

    def __init__(self, view):
        self.view = view

    def run(self, stdscr):
        import curses
        curses.curs_set(0)
        stdscr.keypad(True)
        self.curses = curses
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
                c.A_BOLD if row.get("cursor") else 0)
            self.put(stdscr, y, row["text"][:w - 1], base)
            for s, e, kind in row["spans"]:
                if 0 <= s < e <= w - 1:
                    try:
                        stdscr.chgat(y, s, e - s, self.attr(kind) | base)
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
            v.move(1)
        elif key in (ord("k"), c.KEY_UP):
            v.move(-1)
        elif key in (ord(" "), c.KEY_NPAGE, ord("f")):
            v.page(1, h - 2)
        elif key in (ord("b"), c.KEY_PPAGE):
            v.page(-1, h - 2)
        elif key == ord("g"):
            v.home()
        elif key == ord("G"):
            v.end()
        elif key in (ord("h"), c.KEY_LEFT):
            v.scroll_h(-8)
        elif key in (ord("l"), c.KEY_RIGHT):
            v.scroll_h(8)
        elif key == ord("w"):
            v.toggle_wrap()
        elif key == KEY_TAB:
            v.pick(1)
        elif key in (c.KEY_BTAB, ord("p")):
            v.pick(-1)
        elif key in (KEY_CR, KEY_LF, c.KEY_ENTER, ord("*")):
            self.do_search(stdscr, v.selected())
        elif key == ord("/"):
            self.do_search(stdscr, self.prompt(stdscr, "search token: "))
        elif key == ord("n"):
            v.cycle(1)
        elif key == ord("N"):
            v.cycle(-1)
        elif key == ord("S"):
            self.pick_store(stdscr)
        elif key == ord("y"):
            v.message = str(v.address())
        elif key == ord("?"):
            self.help(stdscr)
        elif key == c.KEY_RESIZE or key == 12:
            stdscr.clear()
        return True

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

    def do_search(self, stdscr, token):
        v = self.view
        if not token:
            v.message = v.nothing_pickable()
            return
        v.search(token)
        if not v.hits:
            return
        # A list, because an identifier on six hosts is six answers and
        # jumping to one of them silently picks for you.
        i = self.choose(stdscr, f"{token} — {len(v.hits)} hit(s)",
                        [self.hit_row(x) for x in v.hits])
        if i is not None:
            v.hit = i
            v.jump(v.hits[i])
            v.message = f"hit {i + 1}/{len(v.hits)}  {v.message}"

    def hit_row(self, hit):
        where = (hit.store or {}).get("name", "?")
        if hit.address.host:
            where += f" @ {hit.address.host}"
        return f"{where:28} {hit.text[:200]}"

    def pick_store(self, stdscr):
        v = self.view
        stores = self.view.backend.stores()
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
        i = self.choose(stdscr, title, rows)
        if i is None:
            return
        v.tape = Tape(v.backend, stores[i])
        try:
            v.open()
        except Refused as e:
            v.message = str(e)

    def help(self, stdscr):
        c = self.curses
        stdscr.erase()
        for y, line in enumerate(HELP_KEYS.strip("\n").splitlines()):
            self.put(stdscr, y, line)
        h, _ = stdscr.getmaxyx()
        self.put(stdscr, h - 1, "  any key to go back", c.A_REVERSE)
        stdscr.refresh()
        stdscr.getch()

    def choose(self, stdscr, title, rows):
        c = self.curses
        sel, top = 0, 0
        while True:
            h, w = stdscr.getmaxyx()
            body = max(1, h - 2)
            top = min(top, sel)
            if sel >= top + body:
                top = sel - body + 1
            stdscr.erase()
            self.put(stdscr, 0, f"── {title}", c.A_REVERSE)
            for y, i in enumerate(range(top, min(len(rows), top + body)),
                                  start=1):
                self.put(stdscr, y, rows[i],
                         c.A_REVERSE if i == sel else 0)
            self.put(stdscr, h - 1,
                     "  Enter open   j/k move   q back", c.A_REVERSE)
            stdscr.refresh()
            key = stdscr.getch()
            if key in (ord("q"), KEY_ESC):
                return None
            if key in (KEY_CR, KEY_LF, c.KEY_ENTER):
                return sel
            if key in (ord("j"), c.KEY_DOWN):
                sel = min(len(rows) - 1, sel + 1)
            elif key in (ord("k"), c.KEY_UP):
                sel = max(0, sel - 1)
            elif key in (ord(" "), c.KEY_NPAGE):
                sel = min(len(rows) - 1, sel + body)
            elif key == c.KEY_PPAGE:
                sel = max(0, sel - body)




def watch(backend, store, seq=None, at=None, offset=None):
    """Open the viewer and return the address it was left at."""
    import curses
    view = View(backend, store)
    view.open(seq=seq, at=at, offset=offset)
    return curses.wrapper(Screen(view).run)


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
