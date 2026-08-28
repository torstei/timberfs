#!/usr/bin/env python3
"""timberfs SQL-ish console — prototype.

    create logview [type=console] console;
    select stores  from console;
    select records from console where entry has 'ERROR' limit 100;
    select records from [] where chunk may have 'req-8f3a';
    tail   records from console;

A LOGVIEW is a NAMED PREDICATE, re-resolved every time it is used — so a
store that appears after you defined it is in it. That is the whole
reason the selection is a query and not a list.

`[...]` is a predicate literal, so a view name and a label can never be
mistaken for one another, and a completer always knows which it is
completing.

Two things this deliberately does NOT implement, because timberfs
already has them and a second copy would drift:

  * the selector — every resolution is a real `kind: "stores"` query
  * time parsing — `--from X --dump-json` is asked what X means

`--help` lists the flags; each has an environment variable beside it.

  TIMBERFS_CMD   argv prefix, default `timberfs query --query -`
  TIMBERFS_HOSTS comma-separated hosts to fan out to. Every occurrence of
                 _TIMBERHOST_ in TIMBERFS_CMD is replaced with the host:

                   TIMBERFS_HOSTS=web01,web02,db01
                   TIMBERFS_CMD="ssh _TIMBERHOST_ timberfs query --query -"

                 Any command that reaches a timberfs works, so a wrapper
                 that takes the host as an argument does too.

                 Stores from every host are presented as one set, and each
                 remembers where it lives. Unset, there is one unnamed host
                 and nothing is substituted.
  TIMBERFS_RC    views loaded at startup, default ~/.timberfsrc
"""
import argparse, json, os, re, readline, shlex, subprocess, sys, threading, time

CMD = shlex.split(os.environ.get("TIMBERFS_CMD", "timberfs query --query -"))
HOST_TOKEN = "_TIMBERHOST_"
HOSTS = [h.strip() for h in os.environ.get("TIMBERFS_HOSTS", "").split(",") if h.strip()]
# `None` is the one unnamed host: no fan-out, no substitution, and every
# host-aware path below collapses to what it did before.
TARGETS = HOSTS or [None]
# How long the store list is reused for completion and `\d`. Short, because
# a store that appears mid-session is exactly what a predicate is for.
STORE_TTL = float(os.environ.get("TSQL_STORE_TTL", "30"))
RC = os.environ.get("TIMBERFS_RC", os.path.expanduser("~/.timberfsrc"))
KINDS = ("records", "loglines", "stores", "chunks")
OPS = ("!=", "!~", "!*", "=~", "=*", "=")

HELP = """
  \\d                     the stores, their short ids, and what still
                         splits them by label
  \\d NAME | \\d ID        that one in full -- a substring of the name, or a
                         prefix of the id (the 8 chars \\d prints)

  A short id works as INPUT anywhere -- `[id=79d7f23a]` -- and is resolved
  to the whole one before the query is sent, the way a short SHA is. An
  ambiguous prefix is refused, never picked from.

  With TIMBERFS_HOSTS set, every host is asked and the stores show as one
  set with a HOST column. Listings go out in parallel; reads go host by
  host (no order is claimed between them) and `limit N` is N in total, so
  a later host may go unread -- it says so when that happens. A host that
  cannot be reached is named, never quietly missing.
  \\d+                    ...the listing with chunk counts and write spans
  \\dv                    the logviews      \\?  this      \\q  quit

  \\d is the quick look. Once there are more stores than fit a screen, the
  QUERY is the tool -- it can select on any label, not just the name:

  select stores from [];                   every store, as a query
  select stores from [type=console,host=*rc];

  create logview [type=console] console;   name a predicate
  drop logview console;                    forget it
  show logviews;                           what is defined

  select <kind> from <source> [where ...] [limit N [chunks]];
  tail   <kind> from <source> [where ...];

    <kind>    records | loglines | stores | chunks
    <source>  a view name, or a predicate literal like [type=console].
              [] is the EMPTY predicate, and an empty predicate is every
              store -- enumerating is not a separate verb

  where, joined by `and` -- the SUBJECT says what is being asked:

    entry has 'ERROR'          the entries that contain it
    entry not has 'noise'      ...and that do not
    entry substring 'req-8f'   a literal anywhere, even inside a word
    entry regex '^\\d+ ERR'     a pattern
    chunk may have 'req-8f3a'  the chunks that MAY contain it, whole.
                               `may` because a Bloom filter cannot say more
    logline since '12:00'      the timestamps the lines carry
    logline until '13:00'
    written since '2026-08-27' when the data arrived

  save 'file';                 write the logviews out as statements
  load 'file';                 run a file of statements (as ~/.timberfsrc
                               is run at startup — it is a SCRIPT, so it
                               can hold anything you could type)
  help;  quit
"""


# ---------------------------------------------------------------- lexing
def lex(s):
    out, i = [], 0
    while i < len(s):
        c = s[i]
        if c.isspace():
            i += 1
        elif c == ";":
            out.append((";", ";")); i += 1
        elif c == "[":
            j = s.index("]", i)
            out.append(("pred", s[i + 1 : j])); i = j + 1
        elif c in "'\"":
            j = s.index(c, i + 1)
            out.append(("str", s[i + 1 : j])); i = j + 1
        else:
            j = i
            while j < len(s) and not s[j].isspace() and s[j] not in ";[":
                j += 1
            out.append(("word", s[i:j])); i = j
    return out


# Rust's regex metacharacters. Escaping them is what turns a typed name
# into a LITERAL inside the anchored `=~` match below.
RX_META = "\\.+*?()|[]{}^$"


def rx_literal(text):
    return "".join("\\" + c if c in RX_META else c for c in text)


def parse_pred(text):
    """`type=console,host=web01` -> document terms. The SYNTAX is parsed
    here; what it MEANS is timberfs's business, and asking it is the only
    way to know."""
    terms = []
    for part in [p for p in text.split(",") if p.strip()]:
        for op in OPS:
            if op in part:
                k, v = part.split(op, 1)
                terms.append({"key": k.strip(), "op": op, "value": v.strip()})
                break
        else:
            # Something that LOOKS like an operator but is not one is a
            # typo, not a store called that. Reading it as a name would
            # answer "0 stores" for a search that was never run.
            if any(c in part for c in "=~!*"):
                raise ValueError(
                    f"`{part.strip()}` has no operator I know — one of "
                    + ", ".join(OPS))
            # A bare word is the NAME, matched anywhere in it. Spelled as
            # an escaped anchored regex rather than `=*` for the reason in
            # `backslash`: a timberfs before 0.24.0 truncates `=*` to `=`
            # and reads the `*` as part of the value, silently.
            terms.append({"key": "name", "op": "=~",
                          "value": f".*{rx_literal(part.strip())}.*"})
    return terms


# ------------------------------------------------------------- timberfs
def argv_for(host):
    return CMD if host is None else [a.replace(HOST_TOKEN, host) for a in CMD]


def run(doc, host=None):
    """One document to one host. A transport that never started — ssh
    refusing, a wrapper missing — comes back as a non-zero rc with its own
    words, and is reported rather than counted as an empty answer."""
    try:
        p = subprocess.run(argv_for(host), input=json.dumps(doc),
                           capture_output=True, text=True)
    except OSError as e:
        return "", str(e), 127
    return p.stdout, p.stderr.strip(), p.returncode


def when(text):
    """What does this time mean? Ask timberfs, which owns the answer."""
    probe = subprocess.run(
        ["timberfs", "query", "--from", text, "--dump-json"],
        capture_output=True, text=True)
    try:
        return json.loads(probe.stdout)["window"]["from"]
    except Exception:
        raise ValueError(f"timberfs does not read {text!r} as a time")


# --------------------------------------------------------------- render
def facets(stores):
    keys, n = {}, len(stores)
    for s in stores:
        for k, v in (s.get("labels") or {}).items():
            keys.setdefault(k, {}).setdefault(str(v), 0); keys[k][str(v)] += 1
    return {k: (vs, n - sum(vs.values())) for k, vs in keys.items()
            if not (len(vs) == 1 and sum(vs.values()) == n)}


def common(stores):
    if not stores: return {}
    f = stores[0].get("labels") or {}
    return {k: v for k, v in f.items()
            if all((s.get("labels") or {}).get(k) == v for s in stores)}


def human(n):
    for u in ("B", "KiB", "MiB", "GiB"):
        if n < 1024 or u == "GiB":
            return f"{n:.0f} {u}" if u == "B" else f"{n:.1f} {u}"
        n /= 1024


SHORT_ID = 8


def short_id(s):
    return (s.get("id") or "-")[:SHORT_ID]


def when_ms(ms):
    return time.strftime("%Y-%m-%d %H:%M:%SZ", time.gmtime(ms / 1000)) if ms else "-"


def describe_store(s):
    """One store in full, the way `\\d name` describes a relation. Every
    field the answer carries, because the point of asking about ONE is
    that you want what the table view had to leave out."""
    lab = s.get("labels") or {}
    rows = ([("name", s.get("name")), ("id", s.get("id", "(none)"))]
            + ([("host", s["_host"])] if s.get("_host") else [])
            + [
        ("forest", s.get("forest") or "-"),
        ("kind", s.get("kind")),
        ("labels", " ".join(f"{k}={v}" for k, v in sorted(lab.items())) or "(none)"),
        ("chunks", f"{s.get('chunks', 0)}  (seq {s.get('first_seq','-')}..{s.get('last_seq','-')})"),
        ("size", f"{human(s.get('compressed_bytes',0))} compressed"
                 f"  /  {human(s.get('logical_bytes',0))} logical"),
        ("write span", f"{when_ms(s.get('first_write_ms'))}  ..  {when_ms(s.get('last_write_ms'))}"),
    ])
    if s.get("dropped_chunks"):
        rows.append(("dropped", f"{s['dropped_chunks']} chunks, "
                                f"{human(s.get('dropped_uncompressed_bytes',0))} off the tape"))
    rows.append(("index", f"grain over {s['grain_chunks']} chunks" if s.get("grain_chunks")
                 else ("declared, not built yet" if s.get("index_declared") else "none")))
    # A writer is reported only while it LIVES, so its presence is the
    # liveness — there is no separate "is it running" field to disagree.
    rows.append(("writer", s.get("writer") or "none (nothing appending)"))
    rows.append(("wal", "yes (live edge is tailable)" if s.get("wal_declared") else "no"))
    if s.get("followers"):
        rows.append(("followers", ", ".join(f.get("name", "?") for f in s["followers"])))
    rows.append(("path", s.get("path")))
    on = f"  on {s['_host']}" if s.get("_host") else ""
    print(f"\nStore \"{s.get('name')}\"{on}")
    for k, v in rows:
        print(f"  {k:12} {v}")


def show_unreachable(bad):
    """Named FIRST, not as a footnote: a short list and a broken one look
    the same, and the point of merging hosts is that you stop counting
    them yourself."""
    for host, why in sorted((bad or {}).items(), key=lambda x: x[0] or ""):
        print(f"  ⚠ {label(host)}: UNREACHABLE — {why.splitlines()[0][:90]}")


def show_stores(stores, verbose=False, unreachable=None):
    show_unreachable(unreachable)
    n = len(stores)
    c = common(stores)
    hdr = f"{n} store" + ("" if n == 1 else "s")
    if c: hdr += "   ·   all " + " ".join(f"{k}={v}" for k, v in sorted(c.items()))
    print(hdr)
    f = facets(stores)
    if n <= 25 or not f:
        # The ID column is a PREFIX and is labelled as one, because it is
        # hex either way and `id=` is an exact match: pasting what is shown
        # here selects nothing.
        # The host column appears only when there is more than one, so a
        # single-host session looks exactly as it did.
        multi = len(TARGETS) > 1
        hw = max([len(s.get("_host") or "") for s in stores] + [4]) if multi else 0
        hdr = f"  {'HOST':{hw}}  " if multi else "  "
        hdr += f"{'NAME':30} {'ID(8)':{SHORT_ID}}  {'SIZE':>9}"
        print(hdr + ("      CHUNKS  WRITE SPAN" if verbose else "   LABELS"))
        # No row numbers. A store is named, and its NAME is what every
        # other command takes; an ordinal changes the moment another store
        # appears, which is the same trap as naming one by its path.
        for s in sorted(stores, key=lambda s: (s.get("_host") or "", s["name"])):
            lab = " ".join(f"{k}={v}" for k, v in sorted((s.get("labels") or {}).items())
                           if k not in c)
            row = f"  {s.get('_host') or '':{hw}}  " if multi else "  "
            row += (f"{s['name']:30} {short_id(s):{SHORT_ID}}  "
                    f"{human(s.get('compressed_bytes',0)):>9}")
            if verbose:
                row += (f" {s.get('chunks',0):>7} ch  "
                        f"{when_ms(s.get('first_write_ms'))} .. {when_ms(s.get('last_write_ms'))} ")
            print(f"{row}  {lab}")
    else:
        print("  (too many to list)")
    if f:
        seen, alias = {}, {}
        for k in list(f):
            sig = tuple(sorted(f[k][0].items())) + (f[k][1],)
            if sig in seen: alias.setdefault(seen[sig], []).append(k); del f[k]
            else: seen[sig] = k
        print("  narrow by:")
        for k, (vs, miss) in sorted(f.items(), key=lambda x: -len(x[1][0])):
            top = sorted(vs.items(), key=lambda x: (-x[1], x[0]))
            bits = "  ".join(f"{v}·{c2}" for v, c2 in top[:5])
            if len(top) > 5: bits += f"  +{len(top)-5} more"
            if miss: bits += f"   (no {k}·{miss})"
            if alias.get(k): bits += f"   (= {', '.join(alias[k])})"
            print(f"    {k:16} {bits}")


def records(out):
    """The typed stream. Entries carry which store they came from, which
    is the whole reason to ask for `records` over `loglines`."""
    for rec in out.split("\x1e"):
        if not rec: continue
        head, _, rest = rec.partition("\0")
        parts = head.split("\x1f")
        kind, fields = parts[0], dict(
            p.split("=", 1) for p in parts[1:] if "=" in p)
        payload = rest.split("\0", 1)[0] if kind == "entry" else None
        yield kind, fields, payload


def show_records(out, names):
    last, shown = None, 0
    for kind, f, payload in records(out):
        if kind == "source":
            names[f.get("id", "?")] = os.path.basename(f.get("path", "?"))
        elif kind == "entry":
            who = names.get(f.get("id", ""), "")
            for i, line in enumerate((payload or "").rstrip("\n").split("\n")):
                print(f"  {who:28} {line}" if i == 0 else f"  {'':28} {line}")
            shown += 1
        elif kind == "stream-end":
            note = f"  -- {f.get('entries','?')} entries"
            if f.get("status") == "limited":
                note += f", STOPPED by {f.get('limit','a bound')}"
            print(note)
    return shown


def label(host):
    return host or "(local)"


# ---------------------------------------------------------------- build
def build(kind, terms, conds, limit):
    doc = {"v": "1.0-EXPERIMENTAL", "stores": {"select": terms}}
    win, match = {}, {"all": [], "any": [], "none": []}
    gran = None
    for c in conds:
        t = c["t"]
        if t == "time":
            win["axis"] = c["axis"]
            win[c["end"]] = c["ms"]
        else:
            gran = c["gran"] if gran is None else gran
            if c["gran"] != gran:
                raise ValueError(
                    "a query asks about entries or about chunks, not both — "
                    "`entry has` and `chunk may have` in one where")
            (match["none"] if c["neg"] else match["all"]).append(
                {c["kind"]: c["text"]})
    if kind in ("records", "loglines") and "axis" not in win:
        win["axis"] = "logline"
    if kind == "chunks":
        win["axis"] = "write"
    if win: doc["window"] = win
    if gran is not None:
        match = {k: v for k, v in match.items() if v}
        match["granularity"] = gran
        doc["match"] = match
    if limit: doc["max"] = limit
    doc["response_format"] = {"kind": kind}
    return doc


def parse_where(toks, i):
    """`entry has 'X'` / `chunk may have 'X'` / `logline since 'T'`. The
    SUBJECT carries what the document makes required — granularity and
    axis — so neither can be defaulted by accident."""
    conds = []
    while i < len(toks) and toks[i][1] not in (";", "limit"):
        w = toks[i][1].lower()
        if w == "and": i += 1; continue
        if w in ("entry", "chunk"):
            gran = "entries" if w == "entry" else "chunks"
            i += 1
            neg = False
            if toks[i][1].lower() == "not": neg = True; i += 1
            if toks[i][1].lower() == "may": i += 1          # `chunk may have`
            verb = toks[i][1].lower(); i += 1
            if verb == "have": verb = "has"
            if verb not in ("has", "substring", "regex"):
                raise ValueError(f"`{verb}`? expected has, substring or regex")
            if toks[i][0] != "str":
                raise ValueError("the text must be quoted")
            conds.append({"t": "p", "gran": gran, "neg": neg,
                          "kind": verb, "text": toks[i][1]}); i += 1
        elif w in ("logline", "written", "write"):
            axis = "logline" if w == "logline" else "write"
            i += 1
            rel = toks[i][1].lower(); i += 1
            if rel in ("since", "after", "from"): ends = [("from", toks[i][1])]; i += 1
            elif rel in ("until", "before", "to"): ends = [("to", toks[i][1])]; i += 1
            elif rel == "between":
                a = toks[i][1]; i += 1
                if toks[i][1].lower() == "and": i += 1
                b = toks[i][1]; i += 1
                ends = [("from", a), ("to", b)]
            else: raise ValueError(f"`{rel}`? expected since, until or between")
            for end, text in ends:
                conds.append({"t": "time", "axis": axis, "end": end,
                              "ms": when(text)})
        else:
            raise ValueError(
                f"`{w}`? a condition starts with entry, chunk, logline or written")
    return conds, i


# ----------------------------------------------------------- completion
BACKSLASH = ["\\d", "\\d+", "\\dv", "\\?", "\\q"]
VERBS = ["select", "tail", "declare", "fetch", "close", "create", "drop",
         "show", "save", "load", "help", "quit"]
AFTER_ENTRY = ["has", "not", "substring", "regex"]
AFTER_TIME = ["since", "until", "between"]
SUBJECTS = ["entry", "chunk", "logline", "written"]


class Complete:
    """The grammar earns its keep here: at every point exactly one kind of
    thing can come next, so the completer never has to guess. A verb, a
    kind, a view name, a label key, that key's values — and label keys and
    values come from the stores that ARE there, so it cannot offer one
    nothing carries."""

    def __init__(self, sh):
        self.sh = sh

    def options(self, line, word):
        toks = line.split()
        prev = toks[-2] if len(toks) >= 2 and not line.endswith(" ") else (
            toks[-1] if toks and line.endswith(" ") else None)
        n = len(toks) + (1 if line.endswith(" ") else 0)

        # A backslash command takes a store NAME, so once past the command
        # itself the names are the only thing that can follow.
        if line.lstrip().startswith("\\"):
            if n > 1:
                return sorted(x["name"] for x in self.sh.universe())
            return BACKSLASH
        if n <= 1:
            return VERBS
        # inside a predicate literal: a key, then that key's values
        if word.startswith("["):
            body = word[1:]
            head, _, tail = body.rpartition(",")
            for op in OPS:
                if op in tail:
                    k, part = tail.split(op, 1)
                    pre = f"[{head},{k}{op}" if head else f"[{k}{op}"
                    return [pre + v for v in sorted(self.sh.values_of(k))]
            pre = f"[{head}," if head else "["
            return [pre + k + "=" for k in sorted(self.sh.keys_of())]
        if prev in ("select", "tail", "fetch"):
            return KINDS if prev != "fetch" else []
        if prev == "from":
            return sorted(self.sh.views) + ["["] if toks[0] != "fetch" \
                else sorted(self.sh.cursors)
        if prev in KINDS and toks[0] in ("select", "tail"):
            return ["from"]
        if prev == "where" or prev == "and":
            return SUBJECTS
        if prev == "entry":
            return AFTER_ENTRY
        if prev == "chunk":
            return ["may"]
        if prev == "may":
            return ["have"]
        if prev == "not":
            return ["has", "substring", "regex"]
        if prev in ("logline", "written", "write"):
            return AFTER_TIME
        if prev in ("create", "drop"):
            return ["logview"]
        if prev == "show":
            return ["logviews", "cursors"]
        if prev == "close":
            return sorted(self.sh.cursors)
        if prev == "declare":
            return []
        if toks[0] in ("select", "tail") and prev not in ("where",):
            return ["where", "limit"]
        return []

    def __call__(self, text, state):
        line = readline.get_line_buffer()[: readline.get_endidx()]
        try:
            opts = [o for o in self.options(line, text) if o.startswith(text)]
        except Exception:
            opts = []
        return opts[state] if state < len(opts) else None


# ----------------------------------------------------------------- main
class Shell:
    def __init__(self):
        self.views = {}
        self.cursors = {}
        self.names = {}
        self._universe = None
        self._universe_at = 0.0
        self.unreachable = {}
        self._universe_lock = threading.Lock()
        # Filled in the background: against a remote forest the round trip
        # is seconds, and paying it on the first TAB is what makes
        # completion feel broken rather than slow.
        threading.Thread(target=self.universe, daemon=True).start()
        if os.path.exists(RC):
            self.load(RC, quiet=True)

    def universe(self, fresh=False):
        """Every store on every host, cached for STORE_TTL seconds.

        Completion must not cost a round trip per keystroke, and against a
        remote forest that trip is seconds. But it cannot be cached forever
        either: a logview is a PREDICATE precisely so a store that appears
        later is in it, and a completer offering a set that can no longer
        change would contradict that. So it expires rather than being
        pinned, and `fresh` forces a read where being right beats being
        quick.

        Hosts are read in PARALLEL, so N of them cost the slowest one
        rather than the sum. Each store is tagged with where it lives; a
        host that could not be reached is remembered separately and named
        in every listing, because a short list and a broken one look
        identical."""
        with self._universe_lock:
            stale = (self._universe is None
                     or time.monotonic() - self._universe_at > STORE_TTL)
            if not (fresh or stale):
                return self._universe
            doc = {"v": "1.0-EXPERIMENTAL", "stores": {"select": []},
                   "response_format": {"kind": "stores"}}
            got, bad, lock = [], {}, threading.Lock()

            def one(host):
                out, err, rc = run(doc, host)
                with lock:
                    if rc != 0:
                        bad[host] = err or f"exit {rc}"
                        return
                    try:
                        stores = json.loads(out or "[]")
                    except json.JSONDecodeError as e:
                        bad[host] = f"not JSON: {e}"
                        return
                    for st in stores:
                        st["_host"] = host
                    got.extend(stores)

            ts = [threading.Thread(target=one, args=(h,)) for h in TARGETS]
            for t in ts:
                t.start()
            for t in ts:
                t.join()
            self._universe, self.unreachable = got, bad
            self._universe_at = time.monotonic()
            return self._universe

    def expand_ids(self, terms):
        """A short id is INPUT here and never leaves: it is resolved to the
        whole one before the document is built, the way a short SHA works.

        Nothing is guessed — the store list says which id it is, or that it
        is ambiguous, and an ambiguous one is refused rather than picked
        from. That is the difference between this and rewriting a query on
        a hunch: an exact `id=` on the wire still means exactly itself."""
        for t in terms:
            v = t.get("value", "")
            if t.get("key") != "id" or t.get("op") not in ("=", "!="):
                continue
            if len(v) >= 36 or not v:
                continue
            hits = self.ids_starting(v)
            if not hits:
                # A store created since the cache was filled is the one
                # reason to pay for a re-read here.
                hits = self.ids_starting(v, fresh=True)
            if len(hits) == 1:
                t["value"] = hits[0]
            elif not hits:
                raise ValueError(f"no store has an id starting {v!r}")
            else:
                raise ValueError(
                    f"{v!r} is ambiguous — {len(hits)} stores start with it: "
                    + ", ".join(h[:12] for h in sorted(hits)))
        return terms

    def ids_starting(self, prefix, fresh=False):
        p = prefix.lower()
        return [s["id"] for s in self.universe(fresh)
                if (s.get("id") or "").lower().startswith(p)]

    def keys_of(self):
        # `name` and `id` are not labels, but the selector matches the whole
        # manifest, so they are selectable and belong in completion.
        return {"name", "id"} | {k for s in self.universe()
                                 for k in (s.get("labels") or {})}

    def values_of(self, key):
        if key == "name":
            return {s["name"] for s in self.universe() if s.get("name")}
        # The WHOLE id: `id=` is an exact match, and the 8 characters the
        # listing prints are a prefix that would select nothing.
        if key == "id":
            return {s["id"] for s in self.universe() if s.get("id")}
        return {str(v) for s in self.universe()
                for k, v in (s.get("labels") or {}).items() if k == key}

    def source(self, tok):
        # Every path that turns text into terms comes through here, so this
        # is the one place a short id has to be expanded — including the
        # terms a `create logview` stores, which keeps a saved view exact.
        if tok[0] == "pred": return self.expand_ids(parse_pred(tok[1]))
        if tok[1] in self.views: return self.views[tok[1]]
        raise ValueError(f"no logview `{tok[1]}` — `show logviews;` lists them")

    def load(self, path, quiet=False):
        n = 0
        for line in open(path):
            line = line.strip()
            if line and not line.startswith("--"):
                self.do(line); n += 1
        if not quiet: print(f"  {n} statement(s) from {path}")

    def save(self, path):
        # Never defaults to the startup file. That file is a SCRIPT — it
        # may hold selects, settings, anything — and writing back only the
        # views would silently delete the rest of what it does.
        if path is None:
            raise ValueError(
                "`save` needs a file. It writes only the logviews, and the "
                f"startup script ({RC}) may do more than define them")
        with open(path, "w") as f:
            for name, terms in sorted(self.views.items()):
                pred = ",".join(f"{t['key']}{t['op']}{t['value']}" for t in terms)
                f.write(f"create logview [{pred}] {name};\n")
        print(f"  {len(self.views)} logview(s) -> {path}")

    def show_views(self):
        if not self.views: print("  no logviews"); return
        for n, t in sorted(self.views.items()):
            print(f"  {n:14} [{','.join(x['key']+x['op']+x['value'] for x in t)}]")

    def backslash(self, line):
        """psql's muscle memory. Terminated by the newline, not by `;` —
        typing `\\d;` is not what fingers do."""
        cmd, _, arg = line[1:].strip().partition(" ")
        arg = arg.strip().rstrip(";")
        if cmd in ("q", "quit"): raise SystemExit
        if cmd in ("?", "h", "help"): print(HELP); return
        if cmd == "dv": return self.show_views()
        if cmd.rstrip("+") != "d":
            raise ValueError(f"`\\{cmd}`? this shell knows \\d \\d+ \\dv \\? \\q")
        # Fetched whole and filtered here, rather than asked for by
        # predicate. The selector is a CONJUNCTION, and this has to match a
        # name OR an id prefix; a prefix is not an operator it has anyway.
        # It also cannot go wrong across versions, which `=*` did.
        # The cached list: `\\d` is the QUICK look, and the query path
        # below is the one that is always current.
        stores = self.universe()
        if arg:
            a = arg.lower()
            stores = [x for x in stores
                      if a in (x.get("name") or "").lower()
                      or (x.get("id") or "").lower().startswith(a)]
        if not stores:
            # Through the same reporting as a non-empty answer. An empty
            # list is exactly what a fleet of unreachable hosts produces,
            # and returning early here said "no stores" for "nothing
            # worked" — the failure this shell keeps being written to stop.
            show_unreachable(self.unreachable)
            if self.unreachable and len(self.unreachable) == len(TARGETS):
                print(f"  no host answered, so nothing was listed")
            else:
                print(f"  no store matches {arg!r}" if arg else "  no stores")
            return
        # Named one thing: describe it, as `\\d relation` does. Named
        # several: list them, so the name can be narrowed.
        if arg and len(stores) == 1:
            return describe_store(stores[0])
        return show_stores(stores, verbose=cmd.endswith("+"),
                           unreachable=self.unreachable)

    def do(self, line):
        if line.lstrip().startswith("\\"):
            return self.backslash(line.lstrip())
        toks = [t for t in lex(line) if t[1] != ";"]
        if not toks: return
        head = toks[0][1].lower()
        if head in ("quit", "exit"): raise SystemExit
        if head == "help": print(HELP); return
        if head == "save": self.save(toks[1][1] if len(toks) > 1 else None); return
        if head == "load": self.load(toks[1][1] if len(toks) > 1 else RC); return
        if head == "show":
            if toks[1][1].lower().startswith("cursor"):
                if not self.cursors: print("  no cursors"); return
                for n, c in sorted(self.cursors.items()):
                    at = (", ".join(f"{p['id'][:8]}@{p.get('offset','-')}"
                                    for p in c["at"]) or "(not started)")
                    print(f"  {n:14} at {at}\n  {'':14}    {c['stmt']}")
                return
            return self.show_views()
        if head == "drop":
            self.views.pop(toks[2][1], None); return
        if head == "create":
            self.views[toks[-1][1]] = self.source(toks[2]); return
        if head == "declare":
            # `declare errs cursor for select records from console where ...`
            name = toks[1][1]
            if toks[2][1].lower() != "cursor" or toks[3][1].lower() != "for":
                raise ValueError("declare <name> cursor for <select ...>")
            inner = line[line.lower().index(" for ") + 5:]
            self.cursors[name] = {"stmt": inner.rstrip(" ;"), "at": []}
            print(f"  cursor {name}")
            return
        if head == "close":
            self.cursors.pop(toks[1][1], None); return
        if head == "fetch":
            n = int(toks[1][1])
            if toks[2][1].lower() != "from": raise ValueError("fetch N from <cursor>")
            cur = self.cursors.get(toks[3][1])
            if cur is None: raise ValueError(f"no cursor `{toks[3][1]}`")
            return self.fetch(cur, n)
        if head in ("select", "tail"):
            kind = toks[1][1].lower()
            if kind not in KINDS: raise ValueError(f"`{kind}`? one of {KINDS}")
            if toks[2][1].lower() != "from": raise ValueError("expected `from`")
            terms = self.source(toks[3])
            conds, i, limit = [], 4, None
            if i < len(toks) and toks[i][1].lower() == "where":
                conds, i = parse_where(toks, i + 1)
            if i < len(toks) and toks[i][1].lower() == "limit":
                n = int(toks[i + 1][1])
                unit = (toks[i + 2][1].lower() if i + 2 < len(toks) else "entries")
                limit = {"chunks" if unit.startswith("chunk") else "entries": n}
            doc = build(kind, terms, conds, limit)
            if head == "tail": return self.tail(doc)
            return self.once(doc, kind)
        raise ValueError(f"`{head}`? try `help`")

    def fetch(self, cur, n):
        """One page, from a real cursor: the positions the last answer
        reported, handed straight back. Byte-exact on each store's tape,
        so entries sharing a timestamp are still six distinct places."""
        toks = [t for t in lex(cur["stmt"]) if t[1] != ";"]
        kind = toks[1][1].lower()
        terms = self.source(toks[3])
        conds, i = [], 4
        if i < len(toks) and toks[i][1].lower() == "where":
            conds, i = parse_where(toks, i + 1)
        doc = build(kind, terms, conds, {"entries": n})
        # The WHOLE cursor goes to every host. A position names a store by
        # id, ids are uuids, and a host simply never looks up one it does
        # not have — so nothing has to be routed and a page can straddle
        # machines. (Verified against timberfs: an unknown id in `cursor` is
        # ignored, and the known one still resumes.)
        if cur["at"]:
            doc["cursor"] = cur["at"]
        # Positions MERGE onto the ones already held rather than replacing
        # them. A store that delivered nothing this page reports a position
        # with NO offset — and an offsetless entry means "start of the
        # window", so taking the answer at face value would re-read every
        # store that happened to go quiet. Keep what we had for those.
        at = {p["id"]: p for p in cur["at"]}
        shown, more, bad = 0, False, {}
        for host in TARGETS:
            if shown >= n:
                break
            d = dict(doc, max=dict(doc["max"], entries=n - shown))
            out, err, rc = run(d, host)
            if rc != 0:
                bad[host] = err or f"exit {rc}"
                continue
            for k, f, payload in records(out):
                if k == "source":
                    self.names[f.get("id", "?")] = os.path.basename(f.get("path", "?"))
                elif k == "entry":
                    who = self.names.get(f.get("id", ""), "")
                    where = f"{label(host)} " if len(TARGETS) > 1 else ""
                    print(f"  {where}{who:28} {(payload or '').rstrip()}")
                    shown += 1
                # Every store EXAMINED reports one, barren ones included —
                # drop those and the next page rescans them from the
                # window's start. Across hosts they simply accumulate.
                elif k == "position" and f.get("id"):
                    if "offset" in f:
                        at[f["id"]] = {"id": f["id"], "offset": int(f["offset"])}
                    else:
                        at.setdefault(f["id"], {"id": f["id"]})
                elif k == "stream-end" and f.get("status") == "limited":
                    more = True
        for host, why in bad.items():
            print(f"  ⚠ {label(host)}: {why.splitlines()[0][:100]}")
        # A host left unread has more by definition, whatever the ones that
        # did run said.
        if shown >= n and len(TARGETS) > 1:
            more = True
        if more:
            print(f"  -- more: `fetch {n} from ...` again")
        elif not shown:
            print("  -- nothing more (for now)")
        cur["at"] = list(at.values())

    def why_empty(self, doc):
        """Nothing came back — was anything even SEARCHED?

        "no store matched the predicate" and "the stores held no matching
        entry" are opposite problems and look identical from an empty
        answer. Only asked when the answer IS empty, so the extra request
        is paid on the confusing case and nowhere else."""
        probe = {k: v for k, v in doc.items() if k in ("v", "stores")}
        probe["response_format"] = {"kind": "stores"}
        n = 0
        for host in TARGETS:
            out, err, rc = run(probe, host)
            if rc == 0:
                n += len(json.loads(out or "[]"))
        if n:
            return f"  -- nothing matched, in {n} store(s)"
        return ("  -- that predicate selects NO STORE"
                + (f" on any of {len(TARGETS)} hosts" if len(TARGETS) > 1 else "")
                + ", so nothing was searched")

    def once(self, doc, kind):
        # A `stores` answer is a SET, so the hosts merge into one listing.
        if kind == "stores":
            return self.list_stores(doc)
        # Everything else is a READ, and reads go host by host. Interleaving
        # them could only key on arrival, and would claim a timeline across
        # machines that nothing here can honour -- the same reason a bounded
        # timberfs answer is `order=sequential`.
        want = doc.get("max", {}).get("entries")
        total, bad = 0, {}
        for host in TARGETS:
            if want is not None and total >= want:
                # A cap is what fits on YOUR screen, not per host. Stopping
                # here is what makes that true, and is why hosts are read in
                # turn rather than all at once.
                print(f"  -- stopped at {want}; {label(host)} and any after "
                      f"it were not read")
                break
            d = dict(doc)
            if want is not None:
                d["max"] = dict(doc["max"], entries=want - total)
            out, err, rc = run(d, host)
            if rc != 0:
                bad[host] = err or f"exit {rc}"
                continue
            if len(TARGETS) > 1:
                print(f"  == {label(host)}")
            total += show_records(out, self.names) if kind == "records" \
                else (sys.stdout.write(out) or len(out.strip().splitlines()))
        for host, why in bad.items():
            print(f"  ⚠ {label(host)}: {why.splitlines()[0][:100]}")
        if not total and not bad:
            note = self.why_empty(doc)
            if note: print(note)

    def list_stores(self, doc):
        """One listing from every host, in parallel: a set has no order to
        preserve, so nothing is gained by waiting for them in turn."""
        got, bad, lock = [], {}, threading.Lock()

        def one(host):
            out, err, rc = run(doc, host)
            with lock:
                if rc != 0:
                    bad[host] = err or f"exit {rc}"
                    return
                try:
                    stores = json.loads(out or "[]")
                except json.JSONDecodeError as e:
                    bad[host] = f"not JSON: {e}"
                    return
                for st in stores:
                    st["_host"] = host
                got.extend(stores)

        ts = [threading.Thread(target=one, args=(h,)) for h in TARGETS]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        show_stores(got, unreachable=bad)

    def tail(self, doc):
        """The ONE statement with no document behind it: a poll loop. By
        TIMESTAMP, which is inexact at a chunk boundary — the cursor in
        docs/plans/paging.md is what makes it exact."""
        print("  tailing (ctrl-c to stop).  ⚠ polling by timestamp: at a chunk"
              "\n  boundary this can duplicate or miss. The cursor fixes that.")
        doc = dict(doc, response_format={"kind": "records"})
        seen = doc.get("window", {}).get("from", when(time.strftime("%H:%M")))
        # Said once per host per outage, not once per poll.
        complained = {}
        try:
            while True:
                d = dict(doc, window=dict(doc.get("window", {"axis": "logline"}),
                                          **{"from": seen}))
                for host in TARGETS:
                    out, err, rc = run(d, host)
                    if rc != 0:
                        if complained.get(host) != err:
                            print(f"  ⚠ {label(host)}: {err.splitlines()[0][:100]}")
                            complained[host] = err
                        continue
                    complained.pop(host, None)
                    for kind, f, payload in records(out):
                        if kind == "source":
                            self.names[f.get("id", "?")] = os.path.basename(
                                f.get("path", "?"))
                        elif kind == "entry" and int(f.get("ts", 0)) >= seen:
                            who = self.names.get(f.get("id", ""), "")
                            where = f"{label(host)} " if len(TARGETS) > 1 else ""
                            print(f"  {where}{who:28} {(payload or '').rstrip()}")
                            seen = max(seen, int(f.get("ts", 0)) + 1)
                time.sleep(2)
        except KeyboardInterrupt:
            print("\n  stopped.")


def parse_args(argv):
    ap = argparse.ArgumentParser(
        prog="tsql", add_help=True,
        formatter_class=argparse.RawDescriptionHelpFormatter,
        description="A SQL-ish console for timberfs.",
        epilog="Every option defaults to the environment variable beside it, so\n"
               "a flag overrides an export for one session and nothing else.\n"
               "Inside the shell, `\\?` prints the statement syntax.")
    ap.add_argument("--cmd", metavar="ARGV",
                    default=os.environ.get("TIMBERFS_CMD", "timberfs query --query -"),
                    help="how to reach a timberfs; it is handed the query document "
                         "on stdin. $TIMBERFS_CMD")
    ap.add_argument("--hosts", metavar="H,H",
                    default=os.environ.get("TIMBERFS_HOSTS", ""),
                    help="fan out to these hosts, substituting each for "
                         f"{HOST_TOKEN} in --cmd. $TIMBERFS_HOSTS")
    ap.add_argument("--rc", metavar="FILE",
                    default=os.environ.get("TIMBERFS_RC",
                                           os.path.expanduser("~/.timberfsrc")),
                    help="statements run at startup. $TIMBERFS_RC")
    ap.add_argument("--ttl", metavar="SECS", type=float,
                    default=float(os.environ.get("TSQL_STORE_TTL", "30")),
                    help="how long the store list is reused for completion and "
                         "`\\d`. $TSQL_STORE_TTL")
    ap.add_argument("-q", "--quiet", action="store_true",
                    help="start without printing the help")
    return ap.parse_args(argv)


def main(argv=None):
    global CMD, HOSTS, TARGETS, RC, STORE_TTL
    a = parse_args(argv)
    CMD = shlex.split(a.cmd)
    HOSTS = [h.strip() for h in a.hosts.split(",") if h.strip()]
    TARGETS = HOSTS or [None]
    RC, STORE_TTL = a.rc, a.ttl
    # Several hosts and nowhere to put them: every one would get the SAME
    # command, so the same forest would be read N times and every entry
    # come back N times, labelled with a different host each time. A wrong
    # answer that looks like a busy fleet.
    if len(TARGETS) > 1 and not any(HOST_TOKEN in x for x in CMD):
        sys.exit(f"tsql: --hosts names {len(TARGETS)} hosts, but --cmd has no "
                 f"{HOST_TOKEN} to put them in:\n      {a.cmd}")
    sh = Shell()
    # A space is the only delimiter: `[type=console` is ONE word to the
    # completer, which is what lets a predicate literal complete its own
    # keys and values.
    readline.set_completer(Complete(sh))
    readline.set_completer_delims(" ")
    readline.parse_and_bind("tab: complete")
    if not a.quiet:
        print(HELP)
        if len(TARGETS) > 1:
            print(f"  {len(TARGETS)} hosts: {', '.join(TARGETS)}\n")
    if sh.views:
        print(f"  {len(sh.views)} logview(s) from {RC}: {', '.join(sorted(sh.views))}")
    while True:
        try:
            line = input("\ntimberfs=# ").strip()
        except (EOFError, KeyboardInterrupt):
            print(); return
        if not line: continue
        try:
            sh.do(line)
        except SystemExit:
            return
        except Exception as e:
            print(f"  ! {e}")


if __name__ == "__main__":
    main()
