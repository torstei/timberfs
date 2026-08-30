"""Where the timberfs instances are, and how to reach each one.

A TARGET is a name and the way to reach it: an argv to run, or a URL to
POST the document to. The transport belongs to the TARGET rather than to
the session, which is the whole point: an `ssh mail01 timberfs …`, a site
wrapper taking the host as an argument, and an HTTP endpoint on a unix
socket are one fleet, and none has to be expressible as the others with a
placeholder swapped into it.

The document a resolver prints, and the file that holds the same thing:

    {"v": "1.0-EXPERIMENTAL",
     "refresh": ["fleet-sweep", "--cached"],
     "targets": [
       {"name": "mail01", "cmd": ["ssh", "mail01", "timberfs", "query",
                                  "--query", "-"]},
       {"name": "web01",  "cmd": ["site-wrapper", "query", "web01"]},
       {"name": "db01",   "url": "http://db01:9099/query"},
       {"name": "local",  "url": "http://localhost/query",
                          "socket": "/run/timberfs.sock"},
       {"cmd": ["timberfs", "query", "--query", "-"]}
     ]}

`cmd` is a LIST because a command line written as one string has to be
split again at the far end, and the rules for that are ours to invent
and get wrong. Same call the query document makes for `stores.select`.

`socket` is a MEMBER of its own rather than something folded into the
URL, for that same reason. A unix socket is a filesystem path and a URL
has nowhere to put one, so every scheme that tries either percent-encodes
it into the authority or invents a separator we would then own — and a
socket path containing that separator would break silently. Here the URL
stays an ordinary URL that `urlsplit` reads, and `socket` says what to
dial instead of resolving its host. Its host is still the `Host:` header,
which is what a server routing several names off one socket needs.

`refresh` is the command to RE-DERIVE this fleet, where that is not the
command that produced it — a full sweep to open a session with and a
cheap one to re-ask. It applies to the fleet it arrived with: a refresh
whose document names no `refresh` of its own leaves the next one
re-running whatever produced the fleet originally.

A `name` is what the HOST column and a `timber://<host>/…` address say.
It is a HINT and not identity — the store id is what resolves — so
renaming a target, or changing how it is reached, leaves every written
address valid.

⚠ The resolver is asked ONE question: what is the fleet. "Who has this
store" is a different question that has not been thought through, and
reserving an argument for it now would be designing it by accident.

⚠ A URL target has NO stderr. timberfs writes the sentences that explain
an answer which looks wrong — `no store matches …`, `retention overtook
this follower` — to fd 2 and still exits 0, and HTTP has no second
channel to carry them on. So a url target's notes arrive only when the
request FAILS, out of the response body. That is a real gap and not an
oversight: closing it means deciding what a timberfs server puts on the
wire, which this repository has deliberately not decided yet.

Also here: the time a document has to carry already resolved, because
`11:10` means today, where the reader is.
"""
import datetime
import http.client
import json
import os
import shlex
import socket as socketlib
import subprocess
import threading
import urllib.parse

DOC_V = "1.0-EXPERIMENTAL"
DEFAULT_CMD = ["timberfs", "query", "--query", "-"]
HOST_TOKEN = "_TIMBERHOST_"
CONFIG_PATHS = ("~/.config/timberfs/targets.json",
                "/etc/timberfs/targets.json")
SCHEMES = ("http", "https")
# Enough of a failed response to hold the reason, and not so much that a
# server answering an error page fills the screen with it.
ERR_BYTES = 8192


# ------------------------------------------------------------ transports
class Call:
    """One request in flight: bytes as they arrive, then how it ended.

    Both transports present this, so a caller streams an answer without
    knowing whether it came off a pipe or a socket. `finish` returns
    `(explanation, rc)` with rc 0 for success — a process's exit status,
    or an HTTP status mapped onto one — so the callers' existing "rc != 0
    is a host that failed" reading holds for both."""

    def read1(self, n):
        raise NotImplementedError

    def finish(self):
        raise NotImplementedError


class _Dead(Call):
    """A call that never started: ssh missing, a socket with nothing on
    it, a name that does not resolve. Reported as the transport failing
    rather than as an empty answer, because those look identical."""

    def __init__(self, why, rc=127):
        self._why, self._rc = why, rc

    def read1(self, n):
        return b""

    def finish(self):
        return self._why, self._rc


class _ProcCall(Call):
    """A subprocess, written to and then read as it answers.

    ⚠ stderr gets a thread of its own. Two pipes and one reader is a
    process that stops when the one nobody drains fills up, and timberfs
    writes its explanations there."""

    def __init__(self, argv, payload):
        self.p = subprocess.Popen(argv, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE)
        self._err = []
        self._drain = threading.Thread(
            target=lambda: self._err.append(self.p.stderr.read()), daemon=True)
        self._drain.start()
        try:
            self.p.stdin.write(payload)
            self.p.stdin.close()
        except (BrokenPipeError, OSError):
            pass                      # it died before reading; rc says so

    def read1(self, n):
        return self.p.stdout.read1(n)

    def finish(self):
        self.p.stdout.close()
        rc = self.p.wait()
        self._drain.join()
        err = self._err[0].decode("utf-8", "replace").strip() if self._err else ""
        return err, rc


class _UnixHTTPConnection(http.client.HTTPConnection):
    """`http.client` over AF_UNIX. Only the dialling differs: the request,
    the headers and the chunked answer are ordinary HTTP, and the URL's
    host is still what goes in `Host:`."""

    def __init__(self, path, host, **kw):
        super().__init__(host, **kw)
        self._socket_path = path

    def connect(self):
        s = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
        # `timeout` defaults to a sentinel object meaning "whatever the
        # global default is", which a fresh socket already has. Only a
        # real value — a number, or None for blocking — is worth setting.
        if self.timeout is None or isinstance(self.timeout, (int, float)):
            s.settimeout(self.timeout)
        try:
            s.connect(self._socket_path)
        except OSError:
            s.close()
            raise
        self.sock = s


class _HttpCall(Call):
    """A POST whose response body is the answer, read as it arrives.

    A non-2xx is NOT streamed as an answer: the body is read as the
    reason instead, so a proxy's error page cannot be parsed as records
    that happen to be unreadable."""

    def __init__(self, conn, path, payload, headers):
        self._conn = conn
        conn.request("POST", path, body=payload, headers=headers)
        self._resp = conn.getresponse()
        self._rc, self._why = 0, ""
        if not 200 <= self._resp.status < 300:
            body = self._resp.read(ERR_BYTES).decode("utf-8", "replace").strip()
            self._rc = 1
            self._why = f"HTTP {self._resp.status} {self._resp.reason}" + (
                f"\n{body}" if body else "")
            self._resp.close()

    def read1(self, n):
        if self._rc:
            return b""
        try:
            return self._resp.read1(n)
        except (OSError, http.client.HTTPException) as e:
            # The 200 is committed before any of the body exists, so a
            # connection that died mid-answer is not distinguishable from
            # a short one by the status. Say it happened.
            self._rc, self._why = 1, f"the answer stopped early: {e}"
            return b""

    def finish(self):
        try:
            self._resp.close()
        except OSError:
            pass
        self._conn.close()
        return self._why, self._rc


class Target:
    """One timberfs, and the transport that reaches it."""

    def __init__(self, name, cmd=None, url=None, socket=None):
        self.name = name or None
        self.cmd = list(cmd) if cmd else None
        self.url = url
        self.socket = socket
        self._scheme = self._host = self._path = None
        if url is not None:
            self._scheme = urllib.parse.urlsplit(url).scheme
            self._host, self._path = split_url(url, socket)

    @property
    def label(self):
        return self.name or "(local)"

    @property
    def via(self):
        """What actually runs, for `show hosts`. A fleet reached several
        different ways says so there rather than being assumed uniform."""
        if self.cmd:
            return " ".join(self.cmd)
        return f"POST {self.url}" + (f" over {self.socket}" if self.socket
                                     else "")

    def open(self, payload):
        """Start the call. A transport that never started comes back as a
        `_Dead` rather than raising: something in the system being down
        must not stop the rest of the fleet being asked."""
        if self.cmd:
            try:
                return _ProcCall(self.cmd, payload)
            except OSError as e:
                return _Dead(str(e))
        try:
            if self.socket:
                conn = _UnixHTTPConnection(self.socket, self._host)
            elif self._scheme == "https":
                conn = http.client.HTTPSConnection(self._host)
            else:
                conn = http.client.HTTPConnection(self._host)
            return _HttpCall(conn, self._path, payload,
                             {"Content-Type": "application/json"})
        except (OSError, http.client.HTTPException) as e:
            return _Dead(f"{self.via}: {e}")

    def run(self, payload):
        """The whole answer, waited for: `(bytes, explanation, rc)`."""
        call, out = self.open(payload), []
        while True:
            chunk = call.read1(65536)
            if not chunk:
                break
            out.append(chunk)
        err, rc = call.finish()
        return b"".join(out), err, rc

    def __repr__(self):
        return f"Target({self.label}, {self.via})"


def split_url(url, socket=None):
    """A target URL as `(host, path)`, or a ValueError saying what is
    wrong with it. The query string is kept: a `?` is part of the address
    the endpoint was named by, not decoration."""
    u = urllib.parse.urlsplit(url)
    if u.scheme not in SCHEMES:
        raise ValueError(
            f"`{url}`: {'no scheme' if not u.scheme else f'scheme `{u.scheme}`'}"
            f" — a url target is " + " or ".join(f"{s}://…" for s in SCHEMES)
            + (", with `socket` for a unix socket" if not socket else ""))
    host = u.netloc or ("localhost" if socket else "")
    if not host:
        raise ValueError(
            f"`{url}` names no host, and there is no `socket` to dial instead")
    path = u.path or "/"
    return host, (f"{path}?{u.query}" if u.query else path)


# ---------------------------------------------------------------- fleets
class Fleet:
    """The targets, where they came from, and the ones this build cannot
    reach — named rather than dropped, because a host that was never
    asked must not read as a host that had nothing."""

    def __init__(self, targets, source, unusable=(), template=None, how=None,
                 doc_refresh=None):
        self.targets = list(targets)
        self.source = source
        self.unusable = list(unusable)
        # The `--cmd` with its placeholder still in it, where there was
        # one: `add host` needs something to reach a new host WITH.
        self.template = template
        self._how = how or {}
        # The envelope's own `refresh`, before precedence is applied.
        self.doc_refresh = list(doc_refresh) if doc_refresh else None
        self.refresh = None
        self.refresh_source = None

    def again(self):
        """Re-resolve, for `refresh`. A resolver derives its answer, so
        asking again is how a fleet that changed becomes visible.

        Where a refresh command is in force it is asked instead — getting
        the fleet and re-asking for it are allowed to be different
        commands, since the first can afford to be the expensive one."""
        if self.refresh:
            fleet = from_resolver_argv(self.refresh, self.refresh_source)
            check(fleet.targets, fleet.source)
            return _apply_refresh(fleet, self._how)
        return resolve(**self._how)

    @property
    def names(self):
        return [t.name for t in self.targets]

    def by_name(self, name):
        for t in self.targets:
            if t.name == name:
                return t
        return None

    def with_targets(self, targets):
        """The same fleet over a different set of targets, which is what
        `add host` and `drop host` produce: where it came from and how it
        is re-derived are unchanged by editing the set."""
        f = Fleet(targets, self.source, self.unusable, self.template,
                  self._how, self.doc_refresh)
        f.refresh, f.refresh_source = self.refresh, self.refresh_source
        return f


def parse(text, where):
    """The document, as `(targets, unusable, refresh)`. Unknown members
    INSIDE a target are tolerated and leave it unreachable-with-a-reason
    rather than fatal: a target this build cannot reach is a fact about
    the build, and refusing the whole fleet over one would be the wrong
    blast radius. The envelope is small and stable, so an unknown member
    THERE is a mistake worth naming."""
    try:
        doc = json.loads(text)
    except json.JSONDecodeError as e:
        raise ValueError(f"{where} is not JSON: {e}") from None
    if not isinstance(doc, dict):
        raise ValueError(
            f"{where}: a target document is an object with `v` and "
            f"`targets`, not a {type(doc).__name__}")
    extra = sorted(set(doc) - {"v", "targets", "refresh"})
    if extra:
        raise ValueError(f"{where}: unknown member(s) {', '.join(extra)}")
    if doc.get("v") != DOC_V:
        raise ValueError(
            f"{where}: `v` is {doc.get('v')!r}, and this build speaks "
            f"{DOC_V!r}")
    refresh = doc.get("refresh")
    if refresh is not None and not (isinstance(refresh, list) and refresh
                                    and all(isinstance(x, str) for x in refresh)):
        raise ValueError(
            f"{where}: `refresh` is the command that re-derives this fleet, "
            f"as a non-empty list of strings")
    listed = doc.get("targets")
    if not isinstance(listed, list):
        raise ValueError(f"{where}: `targets` is a list of objects")
    targets, unusable = [], []
    for i, t in enumerate(listed):
        if not isinstance(t, dict):
            raise ValueError(f"{where}: target {i} is not an object")
        name = t.get("name")
        if name is not None and not isinstance(name, str):
            raise ValueError(f"{where}: target {i} has a non-string `name`")
        who = f"target {i} (`{name or 'unnamed'}`)"
        cmd, url, sock = t.get("cmd"), t.get("url"), t.get("socket")
        # Two transports on one target is a mistake in the document, not a
        # target this build cannot reach: preferring one silently is
        # exactly the error the target list exists to remove.
        if cmd is not None and url is not None:
            raise ValueError(
                f"{where}: {who} declares both `cmd` and `url` — a target is "
                f"reached one way, and choosing for you is the mistake this "
                f"list removes")
        if cmd is None and url is None:
            declared = ", ".join(sorted(k for k in t if k != "name"))
            unusable.append((
                name, f"declares {declared or 'no way to reach it'}, and "
                      f"this build reaches a target by `cmd` or `url`"))
            continue
        if sock is not None and not isinstance(sock, str):
            raise ValueError(f"{where}: {who} has a non-string `socket`")
        if cmd is not None:
            if sock is not None:
                raise ValueError(
                    f"{where}: {who} has a `socket`, which says how to dial a "
                    f"`url`; a `cmd` reaches its own timberfs")
            if not (isinstance(cmd, list) and cmd
                    and all(isinstance(x, str) for x in cmd)):
                raise ValueError(
                    f"{where}: {who} has a `cmd` that is not a non-empty list "
                    f"of strings")
            targets.append(Target(name, cmd=cmd))
            continue
        if not (isinstance(url, str) and url):
            raise ValueError(f"{where}: {who} has a `url` that is not a "
                             f"non-empty string")
        try:
            targets.append(Target(name, url=url, socket=sock))
        except ValueError as e:
            # A URL this build does not know how to dial leaves that ONE
            # target named-with-a-reason. A future scheme lands here
            # rather than taking the fleet down.
            unusable.append((name, str(e)))
    return targets, unusable, refresh


def check(targets, source):
    """What a usable fleet must be true of."""
    if not targets:
        raise ValueError(
            f"{source} named no target this build can reach; a session "
            "with none has nothing to ask")
    seen = {}
    for t in targets:
        if t.name in seen:
            raise ValueError(
                f"{source} names {t.label} twice — two answers under one "
                "name cannot be told apart"
                if t.name else
                f"{source} has two targets with no name — an answer from "
                "either would be labelled the same")
        seen[t.name] = t
    return targets


def from_cmd_hosts(cmd, hosts, source):
    """The old shape, as targets: one command with `_TIMBERHOST_`
    substituted per host. Kept because it is what is exported today, and
    it is now ONE producer of a target list rather than the only way to
    describe a fleet."""
    argv = shlex.split(cmd) if cmd else list(DEFAULT_CMD)
    names = [h.strip() for h in (hosts or "").split(",") if h.strip()]
    token = any(HOST_TOKEN in a for a in argv)
    # Both directions are wrong in the same silent way: hosts with
    # nowhere to put them read one forest N times under N names, and a
    # placeholder with no host runs with the placeholder still in it.
    if names and not token:
        raise ValueError(
            f"{source}: the command has no {HOST_TOKEN} to put a host in:"
            f"\n      {' '.join(argv)}")
    if token and not names:
        raise ValueError(
            f"{source}: the command has a {HOST_TOKEN} and no host to put "
            f"in it, so it would run with the placeholder still in it:"
            f"\n      {' '.join(argv)}")
    if not names:
        return Fleet([Target(None, cmd=argv)], source)
    return Fleet([Target(n, cmd=[a.replace(HOST_TOKEN, n) for a in argv])
                  for n in names], source, template=argv)


def from_file(path, source):
    try:
        with open(os.path.expanduser(path)) as f:
            text = f.read()
    except OSError as e:
        raise ValueError(f"{source}: {e}") from None
    targets, unusable, refresh = parse(text, source)
    return Fleet(targets, source, unusable, doc_refresh=refresh)


def from_resolver(command, source):
    """Run the resolver and read the fleet it prints."""
    return from_resolver_argv(shlex.split(command), source)


def from_resolver_argv(argv, source):
    """The same, from an argv already split — which is what a `refresh`
    in the document carries, and what a `--refresh` becomes.

    A resolver that failed is FATAL. Falling back to a default would
    answer a question about one fleet with a different one, and the
    empty case is worse still: a session that quietly asks the local
    machine instead looks exactly like a fleet that held nothing."""
    if not argv:
        raise ValueError(f"{source} is empty — there is no command to run")
    try:
        p = subprocess.run(argv, input=b"", capture_output=True)
    except OSError as e:
        raise ValueError(f"{source}: could not run {argv[0]!r}: {e}") from None
    if p.returncode != 0:
        why = p.stderr.decode("utf-8", "replace").strip()
        raise ValueError(
            f"{source} failed (exit {p.returncode})"
            + (f":\n      " + "\n      ".join(why.splitlines()) if why else ""))
    targets, unusable, refresh = parse(
        p.stdout.decode("utf-8", "replace"), f"{source} ({argv[0]})")
    return Fleet(targets, source, unusable, doc_refresh=refresh)


def _apply_refresh(fleet, how):
    """Which command `refresh` will run, most explicit first: `--refresh`,
    `$TIMBERFS_REFRESH`, the document's own `refresh`, and otherwise
    whatever produced the fleet. Same flag-beats-export precedence as
    everywhere else here, and `show hosts` says which won."""
    fleet._how = how
    flag = how.get("refresh")
    env = os.environ.get("TIMBERFS_REFRESH")
    for value, source in ((flag, "--refresh"), (env, "$TIMBERFS_REFRESH")):
        if not value:
            continue
        argv = shlex.split(value)
        # Named and empty is a mistake worth saying, not a silent
        # fallback to the resolver — the whole point of naming it is
        # that the two commands are not the same.
        if not argv:
            raise ValueError(f"{source} names no command to re-derive with")
        fleet.refresh, fleet.refresh_source = argv, source
        return fleet
    if fleet.doc_refresh:
        fleet.refresh = list(fleet.doc_refresh)
        fleet.refresh_source = f"the `refresh` in {fleet.source}"
    return fleet


def resolve(resolver=None, targets=None, cmd=None, hosts=None, refresh=None):
    """The fleet, from the most explicit source that says anything.

    A flag beats an export, as everywhere else here. Two FLAGS is a
    usage error: they are three ways to answer one question and picking
    one silently is the mistake this whole change is about. Two EXPORTS
    is not — a stale one in a shell profile is ordinary — so the order
    decides and `show hosts` says which won.

    `refresh` is not a fourth way to say where the fleet is, so it does
    not join that check: it says how to ask AGAIN, which is a different
    question and may legitimately have a different answer."""
    how = {"resolver": resolver, "targets": targets, "cmd": cmd,
           "hosts": hosts, "refresh": refresh}
    given = [n for n, v in (("--resolver", resolver), ("--targets", targets),
                            ("--cmd/--hosts", cmd or hosts)) if v]
    if len(given) > 1:
        raise ValueError(
            "give one of " + ", ".join(given)
            + " — they are three ways to say where the fleet is")
    env = os.environ
    if resolver:
        fleet = from_resolver(resolver, "--resolver")
    elif targets:
        fleet = from_file(targets, "--targets")
    elif cmd or hosts:
        fleet = from_cmd_hosts(cmd, hosts, "--cmd/--hosts")
    elif env.get("TIMBERFS_RESOLVER"):
        fleet = from_resolver(env["TIMBERFS_RESOLVER"], "$TIMBERFS_RESOLVER")
    elif env.get("TIMBERFS_TARGETS"):
        fleet = from_file(env["TIMBERFS_TARGETS"], "$TIMBERFS_TARGETS")
    elif env.get("TIMBERFS_CMD") or env.get("TIMBERFS_HOSTS"):
        fleet = from_cmd_hosts(env.get("TIMBERFS_CMD"),
                               env.get("TIMBERFS_HOSTS"),
                               "$TIMBERFS_CMD/$TIMBERFS_HOSTS")
    else:
        found = next((p for p in (os.path.expanduser(x) for x in CONFIG_PATHS)
                      if os.path.exists(p)), None)
        fleet = from_file(found, found) if found else Fleet(
            [Target(None, cmd=DEFAULT_CMD)], "the default")
    check(fleet.targets, fleet.source)
    return _apply_refresh(fleet, how)


# ------------------------------------------------------- typed times
# What `timberfs query --from` accepts, in its order. Zoneless forms are
# LOCAL time, and a bare time is today.
TIME_FORMATS = [
    ("%Y-%m-%d %H:%M:%S", "d"), ("%Y-%m-%dT%H:%M:%S", "d"),
    ("%Y-%m-%d %H:%M", "d"),    ("%Y-%m-%dT%H:%M", "d"),
    ("%Y.%m.%d %H:%M:%S", "d"), ("%Y.%m.%d %H:%M", "d"),
    ("%Y-%m-%d", "d"),          ("%Y.%m.%d", "d"),
    ("%H:%M:%S.%f", "t"),       ("%H:%M:%S", "t"), ("%H:%M", "t"),
]


def when_ms(ms):
    """A millisecond stamp as a person reads it, in UTC.

    Beside `when`, which goes the other way: both front ends and the
    viewer need the pair, and a second copy of either is a second answer
    to what a timestamp looks like."""
    import time
    return time.strftime("%Y-%m-%d %H:%M:%SZ", time.gmtime(ms / 1000)) if ms else "-"


def when(text):
    """A typed time, in milliseconds — resolved at the EDGE.

    A query document is self-contained, so building one must not need a
    timberfs on this machine to interpret a string. And `11:10` means
    today, where the reader is: carried as text it would mean a
    different instant tomorrow and another one at the far end."""
    text = text.strip()
    try:
        return int(datetime.datetime.fromisoformat(text).timestamp() * 1000)
    except ValueError:
        pass
    for fmt, kind in TIME_FORMATS:
        try:
            t = datetime.datetime.strptime(text, fmt)
        except ValueError:
            continue
        if kind == "t":
            t = datetime.datetime.combine(datetime.date.today(), t.time())
        return int(t.astimezone().timestamp() * 1000)
    if text.isdigit():
        n = int(text)
        return n if n > 100_000_000_000 else n * 1000
    raise ValueError(
        f"{text!r} is not a time I read — try RFC3339, "
        f"'YYYY-MM-DD [HH:MM[:SS]]', 'HH:MM[:SS]' for today, or unix seconds")
