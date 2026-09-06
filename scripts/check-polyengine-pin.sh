#!/usr/bin/env bash
# The one-version-everywhere gate for polyengine's JSR packages, replacing the
# retired one-tag-everywhere assertion that used to live in the (now
# deleted) deno translator-fetch script (see polyengine-jsr-contract.md).
#
# As of A22 (the protocol/runtime split), polyengine ships two independently
# versioned lines: `@polyengine/{runtime,translator,wasi,ct-runner}`
# (lockstep) and `@polyengine/protocol` (the host-ABI version, versioned
# separately). polyengine-impl/deno.json is JSR-published AND a host module,
# so per A22 it must not name `@polyengine/runtime` at all — it maps only
# `@polyengine/protocol`, to a caret RANGE (published dependency constraints
# must be ranges). The other two configs (conformance/driver-ct/polyengine
# and its browser/ import map) still map both `@polyengine/runtime` and
# `@polyengine/protocol` to exact pins, since the runtime ones load the same
# on-disk polyengine module and must resolve identically (stateful handles
# minted by one copy are refused by another). Manifests can therefore no
# longer be compared directly: this gate instead asserts, from the three
# repo deno.locks —
#   1. every jsr:@polyengine/* package EXCLUDING @polyengine/protocol
#      resolves to the same version across the two driver-ct locks that
#      still load the embedder (conformance/driver-ct/polyengine[/browser]);
#   2. @polyengine/protocol resolves to the same version across ALL THREE
#      locks (it is the vocabulary that crosses every module boundary here);
#   3. polyengine-impl/deno.lock names no @polyengine/runtime specifier at
#      all (the A22 host-module MUST).
# Run as part of `just polyengine-check` (CI: gha::conformance-matrix), the
# natural fail-loud point since every leg's lock is on disk there.
set -euo pipefail

impl_lock=polyengine-impl/deno.lock
runtime_locks=(
    conformance/driver-ct/polyengine/deno.lock
    conformance/driver-ct/polyengine/browser/deno.lock
)
all_locks=("$impl_lock" "${runtime_locks[@]}")

# (1) one @polyengine/runtime-line version across the two driver-ct locks.
v=$(grep -ohP '"@polyengine/(?!protocol)[a-zA-Z-]+@[^"]+"(?=: \{)' "${runtime_locks[@]}" | sed 's/.*@//;s/"$//' | sort -u)
n=$(printf '%s\n' "$v" | wc -l)
if [ "$n" != 1 ]; then
    echo "polyengine runtime-line pin drift across ${runtime_locks[*]}: $v" >&2
    exit 1
fi

# (2) one @polyengine/protocol version across all three locks.
p=$(grep -ohP '"@polyengine/protocol@[^"]+"(?=: \{)' "${all_locks[@]}" | sed 's/.*@//;s/"$//' | sort -u)
pn=$(printf '%s\n' "$p" | wc -l)
if [ "$pn" != 1 ] || [ -z "$p" ]; then
    echo "@polyengine/protocol pin drift (or missing) across ${all_locks[*]}: $p" >&2
    exit 1
fi

# (3) polyengine-impl (a published host module, A22) must name no
# @polyengine/runtime specifier at all.
if grep -qP '"@polyengine/runtime[^"]*"' "$impl_lock" polyengine-impl/deno.json; then
    echo "polyengine-impl must not import @polyengine/runtime (A22 host-module MUST); found a reference in polyengine-impl/deno.json or its lock" >&2
    exit 1
fi

echo "polyengine pin OK: runtime=$v protocol=$p; polyengine-impl names no @polyengine/runtime"

