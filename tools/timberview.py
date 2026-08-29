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
import threading
import time

import timberfs_client
from timberfs_client import when                              # noqa: F401

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

    CACHE = 8
    # How long a store's bounds are believed. Retention moves the floor
    # while you read, so they cannot be remembered from when the store
    # was opened — but re-reading them before every seek puts a whole
    # round trip in front of a chunk that is often already cached.
    FRESH = 10.0

    def __init__(self, ask, hosts=(None,)):
        self.ask, self.hosts = ask, list(hosts)
        self._stores = None
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


class Line:
    __slots__ = ("offset", "text", "_terms")

    def __init__(self, offset, raw):
        self.offset = offset
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
        """The store as it is now — unless it was just read. A seek to a
        cached chunk otherwise pays a round trip for numbers that cannot
        have moved in the time since."""
        age = time.monotonic() - (self.store.get("_read_at") or 0)
        if age < getattr(self.backend, "FRESH", 0):
            return
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
        anchor = offset if offset is not None else c.start
        landed_at_the_end = seq is None and at is None and offset is None
        if landed_at_the_end and self.tape.at_bottom():
            self.cur = max(0, len(self.tape.lines) - 1)
        else:
            self.cur = self.tape.index_of(anchor)
        # What you land ON has to be real. A run that does not begin at
        # the store's first chunk holds its first line back as a
        # possible fragment, so landing at the top of one means reaching
        # for the chunk before it — the one read the read-ahead cannot
        # be left to do, because it is on the screen you asked for.
        if self.cur < 2 and not self.tape.at_top() and self.tape.extend_up():
            self.cur = self.tape.index_of(anchor)
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

    # -- terms
    def line_terms(self):
        ln = self.line()
        return ln.terms if ln else []

    def pick(self, step):
        """Tab between the selectable terms, and on past the end of the
        line: what can be picked is exactly what can be searched, so the
        motion never lands anywhere a search cannot follow."""
        toks = self.line_terms()
        if not toks:
            self.message = self.nothing_pickable()
            return None
        self.tok = (self.tok + step) % len(toks)
        return toks[self.tok][2]

    def selected(self):
        toks = self.line_terms()
        return toks[self.tok][2] if toks and self.tok < len(toks) else None

    def nothing_pickable(self):
        ln = self.line()
        if not ln or not ln.text.strip():
            return "nothing on this line"
        longest = max(ln.text.split(), key=len)
        return "no searchable term on this line — " + why_not_a_term(longest)

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
            for i, (_, _, t) in enumerate(self.line_terms()):
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
        bits = ["q quit", "Tab term", "Enter search", "n/N hits",
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

  Tab  ⇧Tab      the searchable terms on this line — an identifier is
                 ONE of them, separators and all
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
            self.scroll(stdscr, lambda: v.move(1))
        elif key in (ord("k"), c.KEY_UP):
            self.scroll(stdscr, lambda: v.move(-1))
        elif key in (ord(" "), c.KEY_NPAGE, ord("f")):
            self.scroll(stdscr, lambda: v.page(1, h - 2))
        elif key in (ord("b"), c.KEY_PPAGE):
            self.scroll(stdscr, lambda: v.page(-1, h - 2))
        elif key == ord("g"):
            self.reach(stdscr, "the top of the log", v.home)
        elif key == ord("G"):
            self.reach(stdscr, "the end of the log", v.end)
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
        return f"{where[:self.HIT_PREFIX - 1]:{self.HIT_PREFIX - 1}} " \
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
        c = self.curses
        stdscr.erase()
        for y, line in enumerate(HELP_KEYS.strip("\n").splitlines()):
            self.put(stdscr, y, line)
        h, _ = stdscr.getmaxyx()
        self.put(stdscr, h - 1, "  any key to go back", c.A_REVERSE)
        stdscr.refresh()
        stdscr.getch()

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
        sel, top, tok = 0, 0, 0
        while True:
            h, _ = stdscr.getmaxyx()
            body = max(1, h - 2)
            top = min(top, sel)
            if sel >= top + body:
                top = sel - body + 1
            picked = (self.term_of(rows[sel][terms_from:], tok)
                      if searchable else None)
            stdscr.erase()
            self.put(stdscr, 0, f"── {title}", c.A_REVERSE)
            for y, i in enumerate(range(top, min(len(rows), top + body)),
                                  start=1):
                base = c.A_REVERSE if i == sel else 0
                self.put(stdscr, y, rows[i], base)
                if searchable and i == sel:
                    found = terms(rows[i][terms_from:])
                    for n, (a, b, _t) in enumerate(found):
                        a, b = a + terms_from, b + terms_from
                        if b <= stdscr.getmaxyx()[1] - 1:
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
                sel, tok = min(len(rows) - 1, sel + 1), 0
            elif key in (ord("k"), c.KEY_UP):
                sel, tok = max(0, sel - 1), 0
            elif key in (ord(" "), c.KEY_NPAGE):
                sel, tok = min(len(rows) - 1, sel + body), 0
            elif key == c.KEY_PPAGE:
                sel, tok = max(0, sel - body), 0

    @staticmethod
    def term_of(row, n):
        found = terms(row)
        return found[n % len(found)][2] if found else None




def watch(backend, store, seq=None, at=None, offset=None):
    """Open the viewer and return the address it was left at.

    The FIRST read happens inside curses, because it is the slowest one
    and the one with nothing on screen yet to explain it. A failure
    there still unwinds the terminal and reaches the caller intact —
    which matters for the refusals that are several lines long."""
    import curses
    view = View(backend, store)
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
