"""Where the timberfs instances are, and how to reach each one.

A TARGET is a name and the command that reaches it. The command belongs
to the TARGET rather than to the session, which is the whole point: an
`ssh mail01 timberfs …` and a site wrapper taking the host as an
argument are one fleet, and neither has to be expressible as the other
with a placeholder swapped into it.

The document a resolver prints, and the file that holds the same thing:

    {"v": "1.0-EXPERIMENTAL",
     "targets": [
       {"name": "mail01", "cmd": ["ssh", "mail01", "timberfs", "query",
                                  "--query", "-"]},
       {"name": "web01",  "cmd": ["site-wrapper", "query", "web01"]},
       {"cmd": ["timberfs", "query", "--query", "-"]}
     ]}

`cmd` is a LIST because a command line written as one string has to be
split again at the far end, and the rules for that are ours to invent
and get wrong. Same call the query document makes for `stores.select`.

A `name` is what the HOST column and a `timber://<host>/…` address say.
It is a HINT and not identity — the store id is what resolves — so
renaming a target, or changing how it is reached, leaves every written
address valid.

⚠ The resolver is asked ONE question: what is the fleet. "Who has this
store" is a different question that has not been thought through, and
reserving an argument for it now would be designing it by accident.

Also here: the time a document has to carry already resolved, because
`11:10` means today, where the reader is.
"""
import datetime
import json
import os
import shlex
import subprocess

DOC_V = "1.0-EXPERIMENTAL"
DEFAULT_CMD = ["timberfs", "query", "--query", "-"]
HOST_TOKEN = "_TIMBERHOST_"
CONFIG_PATHS = ("~/.config/timberfs/targets.json",
                "/etc/timberfs/targets.json")


class Target:
    """One timberfs, and the argv that reaches it."""

    def __init__(self, name, cmd):
        self.name = name or None
        self.cmd = list(cmd)

    @property
    def label(self):
        return self.name or "(local)"

    def __repr__(self):
        return f"Target({self.label}, {' '.join(self.cmd)})"


class Fleet:
    """The targets, where they came from, and the ones this build cannot
    reach — named rather than dropped, because a host that was never
    asked must not read as a host that had nothing."""

    def __init__(self, targets, source, unusable=(), template=None, how=None):
        self.targets = list(targets)
        self.source = source
        self.unusable = list(unusable)
        # The `--cmd` with its placeholder still in it, where there was
        # one: `add host` needs something to reach a new host WITH.
        self.template = template
        self._how = how or {}

    def again(self):
        """Re-resolve, for `refresh`. A resolver derives its answer, so
        asking again is how a fleet that changed becomes visible."""
        return resolve(**self._how)

    @property
    def names(self):
        return [t.name for t in self.targets]

    def by_name(self, name):
        for t in self.targets:
            if t.name == name:
                return t
        return None


def parse(text, where):
    """The document, as targets. Unknown members INSIDE a target are
    tolerated and leave it unreachable-with-a-reason rather than fatal:
    a target this build cannot reach is a fact about the build, and
    refusing the whole fleet over one would be the wrong blast radius.
    The envelope is small and stable, so an unknown member THERE is a
    mistake worth naming."""
    try:
        doc = json.loads(text)
    except json.JSONDecodeError as e:
        raise ValueError(f"{where} is not JSON: {e}") from None
    if not isinstance(doc, dict):
        raise ValueError(
            f"{where}: a target document is an object with `v` and "
            f"`targets`, not a {type(doc).__name__}")
    extra = sorted(set(doc) - {"v", "targets"})
    if extra:
        raise ValueError(f"{where}: unknown member(s) {', '.join(extra)}")
    if doc.get("v") != DOC_V:
        raise ValueError(
            f"{where}: `v` is {doc.get('v')!r}, and this build speaks "
            f"{DOC_V!r}")
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
        cmd = t.get("cmd")
        if cmd is None:
            declared = ", ".join(sorted(k for k in t if k != "name"))
            unusable.append((
                name, f"declares {declared or 'no way to reach it'}, and "
                      f"this build reaches a target by `cmd`"))
            continue
        if not (isinstance(cmd, list) and cmd
                and all(isinstance(x, str) for x in cmd)):
            raise ValueError(
                f"{where}: target {i} (`{name or 'unnamed'}`) has a `cmd` "
                f"that is not a non-empty list of strings")
        targets.append(Target(name, cmd))
    return targets, unusable


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
        return Fleet([Target(None, argv)], source)
    return Fleet([Target(n, [a.replace(HOST_TOKEN, n) for a in argv])
                  for n in names], source, template=argv)


def from_file(path, source):
    try:
        with open(os.path.expanduser(path)) as f:
            text = f.read()
    except OSError as e:
        raise ValueError(f"{source}: {e}") from None
    targets, unusable = parse(text, source)
    return Fleet(targets, source, unusable)


def from_resolver(command, source):
    """Run the resolver and read the fleet it prints.

    A resolver that failed is FATAL. Falling back to a default would
    answer a question about one fleet with a different one, and the
    empty case is worse still: a session that quietly asks the local
    machine instead looks exactly like a fleet that held nothing."""
    argv = shlex.split(command)
    try:
        p = subprocess.run(argv, input=b"", capture_output=True)
    except OSError as e:
        raise ValueError(f"{source}: could not run {argv[0]!r}: {e}") from None
    if p.returncode != 0:
        why = p.stderr.decode("utf-8", "replace").strip()
        raise ValueError(
            f"{source} failed (exit {p.returncode})"
            + (f":\n      " + "\n      ".join(why.splitlines()) if why else ""))
    targets, unusable = parse(
        p.stdout.decode("utf-8", "replace"), f"{source} ({argv[0]})")
    return Fleet(targets, source, unusable)


def resolve(resolver=None, targets=None, cmd=None, hosts=None):
    """The fleet, from the most explicit source that says anything.

    A flag beats an export, as everywhere else here. Two FLAGS is a
    usage error: they are three ways to answer one question and picking
    one silently is the mistake this whole change is about. Two EXPORTS
    is not — a stale one in a shell profile is ordinary — so the order
    decides and `show hosts` says which won."""
    how = {"resolver": resolver, "targets": targets, "cmd": cmd,
           "hosts": hosts}
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
            [Target(None, DEFAULT_CMD)], "the default")
    check(fleet.targets, fleet.source)
    fleet._how = how
    return fleet


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
