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

  TIMBERFS_CMD   argv prefix, default `timberfs query --query -`
  TIMBERFS_RC    views loaded at startup, default ~/.timberfsrc
"""
import json, os, re, readline, shlex, subprocess, sys, time

CMD = shlex.split(os.environ.get("TIMBERFS_CMD", "timberfs query --query -"))
RC = os.environ.get("TIMBERFS_RC", os.path.expanduser("~/.timberfsrc"))
KINDS = ("records", "loglines", "stores", "chunks")
OPS = ("!=", "!~", "!*", "=~", "=*", "=")

HELP = """
  \\d                     the stores, and the labels that still split them
  \\d NAME                that one in full (NAME is a substring)
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
            terms.append({"key": "name", "op": "=*", "value": part.strip()})
    return terms


# ------------------------------------------------------------- timberfs
def run(doc, stream=False):
    p = subprocess.run(CMD, input=json.dumps(doc), capture_output=True, text=True)
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


def when_ms(ms):
    return time.strftime("%Y-%m-%d %H:%M:%SZ", time.gmtime(ms / 1000)) if ms else "-"


def describe_store(s):
    """One store in full, the way `\\d name` describes a relation. Every
    field the answer carries, because the point of asking about ONE is
    that you want what the table view had to leave out."""
    lab = s.get("labels") or {}
    rows = [
        ("name", s.get("name")),
        ("id", s.get("id", "(none)")),
        ("forest", s.get("forest") or "-"),
        ("kind", s.get("kind")),
        ("labels", " ".join(f"{k}={v}" for k, v in sorted(lab.items())) or "(none)"),
        ("chunks", f"{s.get('chunks', 0)}  (seq {s.get('first_seq','-')}..{s.get('last_seq','-')})"),
        ("size", f"{human(s.get('compressed_bytes',0))} compressed"
                 f"  /  {human(s.get('logical_bytes',0))} logical"),
        ("write span", f"{when_ms(s.get('first_write_ms'))}  ..  {when_ms(s.get('last_write_ms'))}"),
    ]
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
    print(f"\nStore \"{s.get('name')}\"")
    for k, v in rows:
        print(f"  {k:12} {v}")


def show_stores(stores, verbose=False):
    n = len(stores)
    c = common(stores)
    hdr = f"{n} store" + ("" if n == 1 else "s")
    if c: hdr += "   ·   all " + " ".join(f"{k}={v}" for k, v in sorted(c.items()))
    print(hdr)
    f = facets(stores)
    if n <= 25 or not f:
        # No row numbers. A store is named, and its NAME is what every
        # other command takes; an ordinal changes the moment another store
        # appears, which is the same trap as naming one by its path.
        for s in sorted(stores, key=lambda s: s["name"]):
            lab = " ".join(f"{k}={v}" for k, v in sorted((s.get("labels") or {}).items())
                           if k not in c)
            row = f"  {s['name']:36} {human(s.get('compressed_bytes',0)):>9}"
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
        if os.path.exists(RC):
            self.load(RC, quiet=True)

    def universe(self):
        """Every store, for completion only. Cached: completion must not
        cost a subprocess per keystroke."""
        if self._universe is None:
            out, err, rc = run({"v": "1.0-EXPERIMENTAL", "stores": {"select": []},
                                "response_format": {"kind": "stores"}})
            self._universe = json.loads(out or "[]") if rc == 0 else []
        return self._universe

    def keys_of(self):
        return {k for s in self.universe() for k in (s.get("labels") or {})}

    def values_of(self, key):
        return {str(v) for s in self.universe()
                for k, v in (s.get("labels") or {}).items() if k == key}

    def source(self, tok):
        if tok[0] == "pred": return parse_pred(tok[1])
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

    def stores_matching(self, terms):
        out, err, rc = run({"v": "1.0-EXPERIMENTAL", "stores": {"select": terms},
                            "response_format": {"kind": "stores"}})
        if rc != 0: raise ValueError(err or f"exit {rc}")
        return json.loads(out or "[]")

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
        # A bare word is a NAME SUBSTRING, not a pattern: it is what a
        # person means by "the apache one", and `=*` is literal so a dot
        # in a store name cannot behave like a wildcard.
        terms = [{"key": "name", "op": "=*", "value": arg}] if arg else []
        stores = self.stores_matching(terms)
        if not stores:
            print(f"  no store matches {arg!r}" if arg else "  no stores")
            return
        # Named one thing: describe it, as `\\d relation` does. Named
        # several: list them, so the name can be narrowed.
        if arg and len(stores) == 1:
            return describe_store(stores[0])
        return show_stores(stores, verbose=cmd.endswith("+"))

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
        if cur["at"]:
            doc["cursor"] = cur["at"]
        out, err, rc = run(doc)
        if rc != 0:
            print(f"  ! {err}"); return
        # Every store EXAMINED reports one, barren ones included — drop
        # those and the next page rescans them from the window's start.
        at, shown = [], 0
        for k, f, payload in records(out):
            if k == "source":
                self.names[f.get("id", "?")] = os.path.basename(f.get("path", "?"))
            elif k == "entry":
                who = self.names.get(f.get("id", ""), "")
                print(f"  {who:28} {(payload or '').rstrip()}")
                shown += 1
            elif k == "position" and f.get("id"):
                p = {"id": f["id"]}
                if "offset" in f:
                    p["offset"] = int(f["offset"])
                at.append(p)
            elif k == "stream-end":
                if f.get("status") == "exhausted" and not shown:
                    print("  -- nothing more (for now)")
                elif f.get("status") == "limited":
                    print(f"  -- more: `fetch {n} from ...` again")
        cur["at"] = at

    def once(self, doc, kind):
        out, err, rc = run(doc)
        if rc != 0: print(f"  ! {err}"); return
        if kind == "stores": show_stores(json.loads(out or "[]"))
        elif kind == "records": show_records(out, self.names)
        else: sys.stdout.write(out)

    def tail(self, doc):
        """The ONE statement with no document behind it: a poll loop. By
        TIMESTAMP, which is inexact at a chunk boundary — the cursor in
        docs/plans/paging.md is what makes it exact."""
        print("  tailing (ctrl-c to stop).  ⚠ polling by timestamp: at a chunk"
              "\n  boundary this can duplicate or miss. The cursor fixes that.")
        doc = dict(doc, response_format={"kind": "records"})
        seen = doc.get("window", {}).get("from", when(time.strftime("%H:%M")))
        try:
            while True:
                d = dict(doc, window=dict(doc.get("window", {"axis": "logline"}),
                                          **{"from": seen}))
                out, err, rc = run(d)
                if rc == 0:
                    for kind, f, payload in records(out):
                        if kind == "source":
                            self.names[f.get("id", "?")] = os.path.basename(
                                f.get("path", "?"))
                        elif kind == "entry" and int(f.get("ts", 0)) >= seen:
                            who = self.names.get(f.get("id", ""), "")
                            print(f"  {who:28} {(payload or '').rstrip()}")
                            seen = max(seen, int(f.get("ts", 0)) + 1)
                time.sleep(2)
        except KeyboardInterrupt:
            print("\n  stopped.")


def main():
    sh = Shell()
    # A space is the only delimiter: `[type=console` is ONE word to the
    # completer, which is what lets a predicate literal complete its own
    # keys and values.
    readline.set_completer(Complete(sh))
    readline.set_completer_delims(" ")
    readline.parse_and_bind("tab: complete")
    print(HELP if "-q" not in sys.argv else "")
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
