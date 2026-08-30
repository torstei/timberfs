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
# On a TAG the stamp is that tag. Anywhere else it is the NEXT version —
# the last tag with its minor incremented — so every build on main is
# already the version it would be released as, and any of them can be
# tagged whenever. The artifact CI tested and the artifact a release
# publishes are then the same thing, which is the point of the exercise.
#
# ⚠ Not `git describe`'s `0.27.0-3.gcee4152`. That names the release a
# build came AFTER, which makes every build on main un-releasable without
# rebuilding it under a different version — exactly the coupling this
# removes. It also lies about ordering: semver reads it as a prerelease of
# 0.27.0, which the code is newer than.
#
# ⚠ `--match` PER LINEAGE, never a bare `git describe`. Two release trains
# share this repository and the newest tag is usually the other one's — on
# main today a bare describe answers `timbersh-v0.3.0`, which would stamp
# a timberfs build with the console's version.
#
# The default increment is the MINOR. A release that should be a major or
# a patch is tagged as one, and then the tag and the last main build
# disagree — the release rebuilds, as it always did, and the next cycle
# derives from the new tag. Deviating costs a rebuild, not correctness.

set -euo pipefail
cd "$(dirname "$0")/.."

# No argument on purpose: the git ref is the source of truth, and a
# version passed by hand is the committed version back again under
# another name.
stamp() {  # $1 = the lineage's tag prefix
    local exact last
    # Building a tag? That tag is the version, exactly.
    if exact=$(git describe --tags --match "$1*" --exact-match 2>/dev/null); then
        printf '%s' "${exact#"$1"}"
        return
    fi
    last=$(git describe --tags --match "$1*" --abbrev=0 2>/dev/null) || return 1
    last=${last#"$1"}
    awk -F. '{ print $1 "." ($2 + 1) ".0" }' <<<"$last"
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
