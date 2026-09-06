# The repo-wide checks live here; everything scoped to a subtree lives in that
# subtree's justfile module:
#   just conformance             — the full loopback + interop conformance suite
#   just conformance::<recipe>   — individual targets, labs, builders
#   just examples::<recipe>      — demo components, hosts, and compositions
#   just gha::<job>              — one recipe per CI job; CI job bodies live in
#                                   .github/justfile
# Run `just --list` (or `just --list <module>`) to see every recipe.

# GitHub Actions plumbing: CI job entry points.
mod gha '.github'
mod conformance
mod examples

# List the available recipes.
default:
    @just --list

# The exact set of checks CI runs (ci.yml and conformance.yml): each CI
# job runs exactly one gha:: job recipe. The Shadow lab job
# (gha::shadow-lab) is excluded: it needs the prebuilt shadow binary
# (scripts/download-shadow.sh). Body form rather than dependencies:
# module recipes as dependencies need just 1.42+, newer than the just
# this repository pins.
ci:
    @just gha::rust-checks gha::conformance-build gha::conformance-matrix

# Run the fast pre-commit checks (fmt, clippy, WIT, Rust tests); see AGENTS.md.
check: fmt-check clippy validate-wit test

# Type-check and unit-test the polyengine-native host module (polyengine-impl)
# and type-check the polyengine conformance leg. Needs Deno; the installs
# materialize the pinned module graph + the node-datachannel addon.
# Also asserts the one-version-everywhere pin gate across every deno.json
# that imports a jsr:@polyengine/* package (replaces the retired
# assertPinConsistency(); see conformance/driver-ct/polyengine/README.md),
# and the sibling one-version gate for the JS runner core
# (@jsr/polymorph__test, from JSR; see scripts/check-runner-js-pin.sh).
polyengine-check:
    ./scripts/check-polyengine-pin.sh
    ./scripts/check-runner-js-pin.sh
    cd polyengine-impl && deno install --frozen --allow-scripts=npm:node-datachannel
    cd polyengine-impl && deno task check && deno task test
    cd conformance/driver-ct/polyengine && deno install --frozen --allow-scripts=npm:node-datachannel
    cd conformance/driver-ct/polyengine && deno task check

# Check formatting across all crates.
fmt-check:
    cargo fmt --all -- --check

# Run clippy across all crates: the native crates (the workspace
# default-members), then the wasm-only crates per target.
#
# Run clippy across all crates.
clippy:
    cargo clippy -- -D warnings
    cargo clippy --target wasm32-unknown-unknown \
        -p echo-demo \
        -p echo-remote \
        -- -D warnings
    cargo clippy --target wasm32-wasip2 \
        -p cli-signaling \
        -p rendezvous-http \
        -p echo-remote-driver \
        -p wasip3-webrtc-datachannels \
        -p webrtc-consumer \
        -p conformance-wasip3-mailbox \
        -p conformance-suite-body \
        -p conformance-guest-ct \
        -p conformance-guest-pair-ct \
        -- -D warnings

# Validate WIT packages.
validate-wit:
    wasm-tools component wit wit
    wasm-tools component wit examples/echo-demo/wit
    wasm-tools component wit examples/cli-signaling/wit
    wasm-tools component wit wasip3-impl/wit
    wasm-tools component wit examples/webrtc-consumer/wit
    wasm-tools component wit conformance/wit

# Run the Rust / Wasmtime tests for the native crates (the workspace
# default-members; includes the cli-signaling and echo-remote integration
# tests). nextest runs faster but does not execute doctests, so run those
# separately.
#
# Run the Rust / Wasmtime tests for the native crates.
test:
    cargo nextest run
    cargo test --doc
