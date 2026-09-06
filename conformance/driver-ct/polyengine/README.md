# `conformance/driver-ct/polyengine` — the polyengine-native conformance leg

The `polyengine-deno` target in the loopback conformance matrix: the suite
runs runtime-linked under stock Deno — no transpile step, no generated
tree, no engine flag (the WIT contract's async exports run on the
callback ABI) — against
[`polyengine-impl/src/webrtc.ts`](../../../polyengine-impl/src/webrtc.ts) and the
fetch mailbox ([`signaling.ts`](signaling.ts)). These children carry
the same child contract the retired jco legs did (see git history)
under `rtc-ct-driver`'s loopback orchestration (a solo stream over
`solo/` plus a role pair over `pair/`, folded), the same JSONL wire on
stdout, the same 512 KiB inbound-buffer bound exported through
`WEBRTC_MAX_INBOUND_BUFFER_BYTES`.

The driver spawns each child as
`deno run --allow-all --frozen --config <here>/deno.json run.ts --select
<prefix>` with the `RTC_CT_*` environment (see `run.ts`'s header);
`--allow-all` is not a security boundary here — the leg exists to run a
native WebRTC addon (`node-datachannel`), whose code Deno's sandbox
cannot confine anyway.

## Running it

```sh
just conformance::run-polyengine
```

which installs the leg's pinned module graph + the `node-datachannel`
addon (`just conformance::polyengine-deps`, idempotent) and runs the
loopback leg through the driver, writing
`conformance/driver-ct/results/polyengine-deno.jsonl`. The translator shim
comes from the pinned `@polyengine/translator` JSR package through the
module graph — no fetch step, no network.

## The pin

polyengine ships as exact-pinned JSR prereleases (one per green upstream
commit; the hash in the version names the commit — see the [polyengine
README's "Consuming the unstable
prereleases"](https://github.com/polymorph-components/polyengine#readme)). As
of A22, the `@polyengine/protocol` line versions independently of the
`@polyengine/{runtime,translator,wasi,ct-runner}` lockstep line: `just
polyengine-check`'s pin gate (`../../../scripts/check-polyengine-pin.sh`)
asserts one resolved `@polyengine/runtime` version across the two configs
that load the embedder, one resolved `@polyengine/protocol` version across
all three configs, and that `polyengine-impl` names no `@polyengine/runtime`
specifier at all (published host modules must not import
`@polyengine/runtime` — see `../../../polyengine-impl/README.md`).

- `deno.json` (this directory) — import-map versions
  (`jsr:@polyengine/<pkg>@<version>`) for `@polyengine/ct-runner`,
  `@polyengine/runtime/embedder`, `@polyengine/runtime/shim`,
  `@polyengine/wasi`, `@polyengine/translator`, `@polyengine/protocol`.
  `deno.lock` carries integrity hashes for that module graph, enforced
  with `--frozen`.
- [`browser/deno.json`](browser/deno.json) — the SAME `@polyengine/runtime`
  and `@polyengine/protocol` pins again (the module-identity constraint:
  stateful handles minted by one embedder copy are refused by another, so
  every config that loads the embedder must agree), with the npm WebRTC
  backends stubbed out (never executed in a page).
- [`../../../polyengine-impl/deno.json`](../../../polyengine-impl/deno.json) —
  the SAME `@polyengine/protocol` version only. As of A22 this package (a
  published host module) does not map `@polyengine/runtime` at all: its
  copies of `@polyengine/protocol` are harmless by construction, so it no
  longer participates in the runtime module-identity constraint above.


Each `deno.json` also carries a
`"minimumDependencyAge": { "age": "P1D", "exclude": ["jsr:@polyengine/*"] }`
stanza: the JSR prereleases are typically published well under 24h
before consumption, and without the exclude the default supply-chain
age gate blocks resolution.

To bump: update the version in all three `deno.json` files, delete all
three `deno.lock` files (this directory, `browser/`, and
`polyengine-impl/`), re-run
`deno install --frozen --allow-scripts=npm:node-datachannel` in this
directory and in `polyengine-impl/`, and `deno install` in `browser/`, to
regenerate the locks (`just polyengine-check` also does the first two
installs). Then re-run `just conformance::run-polyengine` and commit the
diff (including the regenerated `matrix.md`, via
`just conformance::matrix-update`, if behavior changed).
