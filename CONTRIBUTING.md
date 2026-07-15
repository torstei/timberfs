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
