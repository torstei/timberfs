#!/usr/bin/env bash
# Write the version this checkout describes into the manifests.
#
#   scripts/stamp-version.sh          # then build; `git checkout` undoes it
#
# The repository holds a PLACEHOLDER version. A build stamps; only a
# tagged CI run publishes. Nothing built anywhere can then claim to be a
# release that was never released — which a committed version could not
# help doing: every build between two releases reported the earlier one's
# number and was indistinguishable from it, including in the
# `server_version` a query answer carries.
#
# The stamp is a pure function of the git ref, so a build of a tag stamps
# exactly that tag's version wherever it runs, and a build three commits
# past it says so: 0.27.0-3.gcee4152.
#
# ⚠ `--match` PER LINEAGE, never a bare `git describe`. Two release trains
# share this repository and the newest tag is often the other one's — on
# main today a bare describe answers `timbersh-v0.3.0`, which would stamp
# a timberfs build with the console's version.
set -euo pipefail
cd "$(dirname "$0")/.."

# No argument on purpose: the git ref is the source of truth, and a
# version passed by hand is the committed version back again under
# another name.
stamp() {
    git describe --tags --match "$1*" --always 2>/dev/null \
        | sed "s/^$1//; s/-\([0-9]*\)-g/-\1.g/"
}

ver=$(stamp v)
sh_ver=$(stamp timbersh-v)
for v in "$ver" "$sh_ver"; do
    # No matching tag describes as a bare sha, which is not a version.
    # Say so rather than stamping something that will not parse.
    printf '%s' "$v" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+' || {
        echo "stamp-version: '$v' is not a version — no matching tag?" >&2
        exit 1
    }
done

sed -i "0,/^version = \".*\"$/s//version = \"$ver\"/" Cargo.toml
# The lock's entry for this package too. Cargo would rewrite that line
# itself on the next command — it is a record of Cargo.toml, not a
# constraint — but doing it here keeps the script offline and leaves no
# surprise diff. The 252 dependency pins, which are the reason the lock
# is committed at all, are untouched.
awk -v v="$ver" '
    /^name = "timberfs"$/ { seen = 1 }
    seen && /^version = / { print "version = \"" v "\""; seen = 0; next }
    { print }
' Cargo.lock > Cargo.lock.stamped && mv Cargo.lock.stamped Cargo.lock
printf '%s\n' "$sh_ver" > tools/VERSION

echo "stamped: timberfs $ver, timbersh $sh_ver"
