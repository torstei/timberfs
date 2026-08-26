# Contributing

## Before you push

Run the same gates CI enforces:

```sh
scripts/check.sh
```

It runs `cargo fmt -- --check`, `cargo clippy --all-targets -- -D warnings`,
and `cargo build --release` — the CI `checks` job, locally, so a formatting or
clippy slip is caught before it costs a round-trip.

To run it automatically on every `git push`, enable the tracked hook once:

```sh
git config core.hooksPath .githooks
```

(Bypass in a pinch with `git push --no-verify`.)

## Bigger changes

The VM test suite exercises the built `.deb` end to end (systemd units,
mount, queries, rotation, upgrade). It needs QEMU:

```sh
cargo deb && tests/vm/run-vm-test.sh
```

## Before cutting a release

Read the documentation against the diff. `git log vX.Y.Z..HEAD` is the
work-list, which makes this a bounded pass rather than an audit.

Three questions, ordered by how badly each has bitten:

1. **Is what CHANGED still described correctly?** A behaviour change tends to
   *add* a correct paragraph and leave the wrong one standing. 0.21.1 shipped
   `incus-intake` prose saying the console ring is drained "once, immediately
   after attaching" — seventy lines above the paragraph describing the
   continuous drain that had replaced it. Search out the old description and
   delete it; writing the new one is the easy half.

2. **Is anything NEW reachable**, not merely written down?
   `timberfs-query-document(5)` was written, installed by the package, and
   linked from no man page, so `man timberfs` showed `--query FILE` with
   nowhere to go. A new concept needs a pointer from wherever a reader first
   meets it.

3. **Did anything removed or renamed leave references behind?** Look in the
   shipped systemd units and the bash completion too, not only in `docs/`.

`cargo test --bin timberfs` covers the *absence* half — every subcommand and
every flag must appear in the man page, and an omission has to be recorded in
an allowlist with a reason. No test can see a page that contradicts itself or
one that nothing links to, which is why this stays a read.
