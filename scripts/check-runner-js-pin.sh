#!/usr/bin/env bash
# The one-version-everywhere gate for the JS runner core: every lockfile
# that resolves @jsr/polymorph__test (jsr:@polymorph/test through JSR's
# npm-compat registry, npm.jsr.io per conformance/driver-ct/polyengine/.npmrc)
# must agree on ONE version — a skewed bump runs the JS harness against
# a Rust runner from a different polymorph-test release, a subtle
# contract-mismatch generator. Sibling to check-polyengine-pin.sh (which
# scopes to @polyengine packages only); wired into `just polyengine-check`
# alongside it. Today there is exactly one npm tree
# (conformance/driver-ct/polyengine) plus its deno.lock's mirrored
# packageJson-driven npm resolution, so this currently asserts
# agreement between those two — the gate that stays meaningful the day
# a second npm tree is added.
set -euo pipefail

polyengine=conformance/driver-ct/polyengine

npm_v=$(jq -r '.packages["node_modules/@jsr/polymorph__test"].version // empty' \
    "$polyengine/package-lock.json")
deno_v=$(jq -r '.npm | keys[] | select(startswith("@jsr/polymorph__test@"))' \
    "$polyengine/deno.lock" | sed 's/.*@//')

v=$(printf '%s\n%s\n' "$npm_v" "$deno_v" | sort -u)
if [ -z "$npm_v" ] || [ "$(printf '%s\n' "$v" | wc -l)" != 1 ]; then
    echo "runner-js pin drift: package-lock.json=$npm_v deno.lock=$deno_v" >&2
    exit 1
fi
echo "runner-js pin OK: @jsr/polymorph__test $v"
