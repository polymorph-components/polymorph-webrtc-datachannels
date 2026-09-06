# AGENTS.md

Guidance for automated agents (and humans) working in this repository.

## What this repository is

`polymorph:webrtc-datachannels`: a WIT interface plus multiple implementations that
run the *same* guest component over a real WebRTC data channel: two hosts
(Wasmtime and polyengine) and one in-guest component (`wasip3-impl`). It is
intentionally small — prefer clarity and correctness over features, and keep the
implementations behaviourally in sync (asserted by the conformance suite under
[`conformance/`](conformance)). See [`README.md`](README.md) for the findings
and the big picture.

## Living knowledge base: `lann/wasm-component-starter`

Before designing a world, changing WIT, or touching the async/streaming plumbing,
consult **[`lann/wasm-component-starter`](https://github.com/lann/wasm-component-starter)**.
Treat it as a *living knowledge base* for this project — it is expected to evolve,
so re-read it rather than relying on a cached summary:

- **[`OUTLINE.md`](https://github.com/lann/wasm-component-starter/blob/main/OUTLINE.md)** —
  a high-density agent reference for the Component Model & WASI: canonical specs,
  the toolchain ecosystem (`wasmtime`, `wasm-tools`, `wac`, `wit-bindgen`, `jco`),
  Rust authoring targets, `wasmtime` host-provisioning flags (e.g. `-S http`,
  `-S p3`, `-W component-model-async=y`), and the WASI 0.2 → 0.3 shift
  (`wasi:io` pollables replaced by native async; `wasi:http` incoming/outgoing
  merged). Read it before designing an interface.
- **[`examples/`](https://github.com/lann/wasm-component-starter/tree/main/examples)** —
  runnable projects that demonstrate patterns this repo relies on: exporting an
  async `run`, async streaming imports/exports flowing guest → host → guest,
  returning a stream from an async export via `wit_bindgen::spawn`, mapping an
  import to a JS adapter with `jco --map`, and fetching URLs over async
  `wasi:http` with the `wasip3` crate. The `browser-tgz-maker` and
  `cli-metadata-printer` apps are the closest analogues to the work here.

When a task involves a capability not yet used in this repo (most notably
`wasi:http@0.3` signaling — see below), look for a matching pattern in the
starter's examples first.

## Repository layout

```
wit/                                   # polymorph:webrtc-datachannels package
  webrtc.wit                           #   types (structural), connections (resources)
wasmtime-impl/                         # Wasmtime host crate (webrtc-rs),
                                       #   modeled after wasmtime_wasi_http::p3;
                                       #   add_to_linker + WebrtcView (types + connections.data-channel-options/data-channel);
                                       #   crate name: wasmtime-webrtc-datachannels
polyengine-impl/                           # polyengine-native host module (stock Deno,
                                       #   runtime-linked; node-datachannel-backed):
                                       #   polyengine's ports/webrtc reference-host port,
                                       #   upstreamed per polymorph-components/polyengine#14; pinned to a
                                       #   polyengine prerelease via JSR import maps (see
                                       #   polyengine-impl/README.md)
wasip3-impl/                           # wasm COMPONENT on `rtc` 0.21: runs the
                                       #   sans-I/O stack in-guest (SansIoPeer core
                                       #   + a wasi:sockets/clocks runtime driver)
                                       #   and EXPORTS polymorph:webrtc-datachannels/
                                       #   connections; composable via `wac plug`;
                                       #   crate: wasip3-webrtc-datachannels
examples/                              # guest components + the demo hosts
  echo-demo/                           # example guest component (Rust)
    wit/                               #   demo-only WIT for this component
      webrtc-echo-demo.wit             #     demo:webrtc-echo (rendezvous, demo)
      deps/polymorph-webrtc-datachannels -> ../../../../wit   # symlink to the root package
  cli-signaling/                       # manual-signaling CLI guest component (Rust):
    wit/                               #   drives connections.peer-connection with
      world.wit                        #     guest-side vanilla (non-trickle) ICE
      deps/polymorph-webrtc-datachannels -> ../../../../wit   # symlink to the root package
  webrtc-consumer/                     # minimal consumer that IMPORTS connections;
                                       #   composed (`wac plug`) with wasip3-impl for
                                       #   the in-guest round-trip integration test
    wit/deps/polymorph-webrtc-datachannels -> ../../../../wit  # symlink to the root package
  wasmtime-demo/                       # native host (Wasmtime + webrtc-rs): thin demo
                                       #   binaries over wasmtime-impl's add_to_linker
                                       #   + the end-to-end cli-signaling integration
                                       #   test (tests/cli_signaling.rs)
conformance/                           # cross-implementation conformance suite, on the
                                       #   polymorph:test harness (rev-pinned git deps in
                                       #   the root Cargo.toml; see conformance/README.md)
  suite-body/                          #   the case bodies + SUT bindings (shared)
  guest-ct/                            #   the full suite component (solo/* + pair/*)
                                       #     + its committed tests.lock
  guest-pair-ct/                       #   the pair-only sibling suite (interop + labs)
                                       #     + its committed tests.lock
  driver-ct/                           #   rtc-ct-driver (child exec mode + loopback/
                                       #     interop/shadow/netns orchestrators, the
                                       #     pair fold, the ported netns topology and
                                       #     Shadow syscall shim), the polyengine
                                       #     children (polyengine/),
                                       #     targets*.toml manifests, and the
                                       #     committed matrix.md + matrix-interop.md
  reference/                           #   the non-wasm reference peer (libwebrtc via
                                       #     LiveKit's Rust bindings)
  wasip3-mailbox/                      #   in-guest wasi:http mailbox client (composed
                                       #     artifacts)
  signaling/                           #   conformance-signalingd HTTP mailbox server
scripts/setup.sh                       # one-shot dependency setup (see below)
```

### WIT is organized by ownership — one copy of the shared package

The **`polymorph:webrtc-datachannels`** package is defined exactly once, at the root
[`wit/`](wit). Each demo component owns its **demo-only** WIT under its own
`examples/<name>/wit/` and pulls the package in through a
`wit/deps/polymorph-webrtc-datachannels` **symlink** back to the root, so there is a
single copy of the shared surface to edit. Do **not** copy the root package into
a component or replace those `deps` symlinks with real directories.

The WIT is split into two packages, keeping the shared and demo-only surfaces
separate:

- **`polymorph:webrtc-datachannels`** (`wit/webrtc.wit`) — the shared interfaces,
  split by ownership: `types` holds every structural (non-resource) type, while
  `connections` holds the stateful resources — the `data-channel-options`
  configuration builder, the `data-channel` transport, and the
  `RTCPeerConnection`-style `peer-connection` design target. Structural
  types can be shared across a composition; the resources are each owned by the
  one component that implements them.
- **`demo:webrtc-echo`** — the demo-only interfaces, split across the demo
  components that use them:
  - `examples/echo-demo/wit/webrtc-echo-demo.wit` — `rendezvous`, `demo`,
    `remote`, and the `webrtc-echo-demo` / `webrtc-echo-remote` worlds.
  - `examples/cli-signaling/wit/world.wit` — the `cli-signaling` world
    (`demo:cli-signaling`), which imports only the standard `connections`
    interface; the vanilla (non-trickle) ICE handling is guest-side.

Cross-package `use` must include the version, e.g.
`use polymorph:webrtc-datachannels/types@0.1.0.{error}`.

Terminology: the standardized connection surface is **`peer-connection`** — do
not describe it as a "signaling" interface or design target in docs or prose.
"Signaling" legitimately names only the manual-signaling CLI demo
(`examples/cli-signaling`) and the conformance signaling server
(`conformance-signalingd`).

Changing an interface identifier (package, interface, or function name) means
updating the consumers that name them as strings:

- the guest bindings in `examples/echo-demo/src/lib.rs` and
  `examples/cli-signaling/src/lib.rs`,
- the host bindings in
  `wasmtime-impl/src/bindings.rs` (whose
  `wit/world.wit` also pulls in the root package through a
  `deps/polymorph-webrtc-datachannels` symlink),
- the Wasmtime host bindings in `examples/wasmtime-demo/src/main.rs`,
- the conformance suite bodies and driver under `conformance/`, and
- the polyengine import-record assembly in `polyengine-impl/src/webrtc.ts` (the
  interface keys and error-class wiring the conformance children and any
  embedder use).

## Build & run

Prerequisites: Rust with the `wasm32-unknown-unknown` target, `wasm-tools`,
Deno 2.x for the polyengine paths (`polyengine-impl/` and the polyengine conformance
legs; stock Deno, no flags), and Node 24+ for the polyengine browser page
drivers (plain `node`, no engine flag).

### One-shot dependency setup: `scripts/setup.sh`

[`scripts/setup.sh`](scripts/setup.sh) installs everything the build steps below
need and is the single source of truth shared by local developers, CI
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)), and the Copilot cloud
agent ([`.github/workflows/copilot-setup-steps.yml`](.github/workflows/copilot-setup-steps.yml)).
It is idempotent, so it is safe to re-run. Assuming a Rust toolchain (via
`rustup`) and Node 24+ are already present, run it once from the repository root:

```sh
./scripts/setup.sh
```

It adds the `wasm32-unknown-unknown` and `wasm32-wasip2` Rust targets; installs
`wasm-tools`, `just`, `cargo-nextest`, `wac`, and `wasmtime` (each skipped if
already on `PATH`; versions pinned via `*_VERSION` variables); installs the
netns-lab tools (iproute2, nftables, coturn; skip with `SKIP_NETNS_LAB=1`).
The polyengine legs' runtime dependencies are not installed here: their just
recipes materialize the pinned module graph and npm trees on first run. It does
**not** install the Shadow network simulator (see below). CI is kept in sync by
calling this same script rather than duplicating the install steps.

Shadow ships no upstream prebuilt binary and is slow to build, so it is built
once by the `shadow-build` workflow (`.github/workflows/shadow-build.yml`, a
`workflow_dispatch`-only job that runs `scripts/build-shadow.sh`) and published
to this repository's `shadow-dev` GitHub prerelease. Install it into `~/.local`
either by downloading that binary (`./scripts/download-shadow.sh`) or by building
it locally (`./scripts/build-shadow.sh`); CI's Shadow-lab job and
`copilot-setup-steps.yml` download it from the release. The `just
conformance::shadow` recipe prints this guidance and fails if the binary is
missing when the lab runs.

```sh
# Guest component (produces examples/echo-demo/build/echo-demo.component.wasm):
just examples::build-component

# polyengine (runtime-linked JS) host — the conformance legs are the maintained
# entry points (stock Deno; headless Chromium for the browser leg, which is
# also the CI check for the browser path; set CHROME_PATH to override):
just conformance::run-polyengine
just conformance::run-polyengine-browser

# Wasmtime (native) host (defaults: 1000 messages of 4096 bytes):
just examples::demo-wasmtime          # or: just examples::demo-wasmtime 1000 4096

# In-guest WASIp3 integration test: build the wasip3-impl provider component
# and the webrtc-consumer, compose them with `wac plug`, and run the single
# composed component under `wasmtime` — two peers connect over wasi:sockets UDP
# loopback entirely in-guest and exchange a message each way. Needs `wasmtime`
# (v46+) and `wac` on PATH; the recipe passes the async + WASIp3 flags:
just examples::test-webrtc-composed

# Rust tests, including the end-to-end cli-signaling integration test (two
# host processes exchanging copy/paste SDP blobs over a real connection):
just test

# Cross-implementation conformance suite: builds the two suite
# components, runs every loopback target (wasmtime, composed,
# polyengine-deno, polyengine-browser) and every interop direction (every
# implementation against the non-wasm reference peer in both orders, the
# reference self-pair, and the direct implementation pairs), aggregates
# against the committed lockfiles + manifests, and diffs the committed
# matrices. Needs Node 24+, Deno 2.x, and a Chrome/Chromium binary:
just conformance

# Conformance netns lab (real non-loopback candidate paths via network
# namespaces; needs sudo and coturn — see the recipe comments):
just conformance::netns lan

# Conformance Shadow lab (the pair corpus for the wasmtime,
# wasip3-guest, and reference rows plus the wasmtime<->wasip3-guest
# interop pair in both orders, over a non-loopback path inside the
# Shadow discrete-event network simulator — deterministic, no root or
# network namespaces). Needs `shadow` on PATH (install with
# scripts/download-shadow.sh or scripts/build-shadow.sh):
just conformance::shadow
```

The recipes above are the underlying npm/cargo invocations documented in
[`README.md`](README.md); the [`justfile`](justfile) is the single entry point so
humans, agents, and CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml))
all run the same commands. The repo-wide checks live in the top-level justfile;
subtree-scoped recipes live in that subtree's module —
[`conformance/justfile`](conformance/justfile) (`just conformance::<recipe>`;
bare `just conformance` runs the full loopback suite),
[`examples/justfile`](examples/justfile) (`just examples::<recipe>`), and
[`.github/justfile`](.github/justfile) (`just gha::<job>`, one recipe per CI
job; CI job bodies live there). Run `just`
with no arguments to list every recipe, or `just --list <module>` for a
module's recipes.

### Checks to run before committing

Run the check recipes that cover what you changed **before committing**, and fix
anything they report. `just check` is the fast pre-commit gate; `just ci` runs
every CI job's body (ci.yml and conformance.yml) except the Shadow lab, which
needs the shadow binary. Match the recipe to the change — and only the
change: a recipe whose scope your edit does not touch can be skipped (e.g. a
docs-, comments-, or workflow-only change needs no `just test`; a change
confined to one crate does not need the recipes that only cover others). When
in doubt about whether a recipe's scope is touched, run it.

| Recipe | Run it when you change… |
| --- | --- |
| `just fmt-check` | any Rust source (formatting). |
| `just clippy` | any Rust source (lints all crates and every wasm target, including `wasip3-webrtc-datachannels` and `webrtc-consumer`). |
| `just validate-wit` | any `.wit` file (root `wit/`, `wasip3-impl/wit/`, or a demo `examples/<name>/wit/`). |
| `just test` | any Rust host/guest code, or the cli-signaling demo. |
| `just examples::build-component` | the `echo-demo` guest or its WIT. |
| `just examples::test-webrtc-composed` | the `wasip3-impl` provider component, the `webrtc-consumer`, or the `connections` WIT (composes them with `wac plug` and runs the round trip under `wasmtime`). |
| `just examples::test-echo-remote-composed` | the `echo-remote` guest, `rendezvous-http`, the `wasip3-impl` provider, or the `rendezvous`/`remote` WIT (composes the fully in-guest peer and connects two `wasmtime run` processes over a signaling server). |
| `just polyengine-check` | the polyengine host module (`polyengine-impl/`) or the polyengine conformance leg's TS (`conformance/driver-ct/polyengine/`): type-checks both and runs the `polyengine-impl` unit tests. Behavior changes also need `just conformance` (the `polyengine-deno` target). |
| `just conformance` | any host/guest behavior the suite asserts — the WIT surface, a host implementation, the suite bodies, the driver, or the manifests (CI runs it in `.github/workflows/conformance.yml`). Intentional case changes also need `just conformance::lock-update`; intentional behavior changes, `just conformance::matrix-update` — commit the diffs. |
| `just check` | broad Rust/WIT changes — the quick gate for most commits. |
| `just ci` | anything broad — reproduces CI locally: the rust checks and the full conformance suite; the Shadow lab runs separately (`just gha::shadow-lab`, needs the shadow binary). |

Keep the implementations producing
the same result — the conformance suite is what asserts it.

### Awaiting PR checks

To wait for a pull request's CI checks, use `gh pr checks`' own watch mode
bounded by a timeout — e.g. `timeout 900 gh pr checks <pr> --watch` — rather
than polling with `sleep … && gh pr checks`. Watch mode returns as soon as the
checks settle (a fixed sleep either wastes the difference or wakes up too
early), and the `timeout` bound keeps a wedged run from hanging the session.

## Code comments

Code comments describe **what** something is or does, not the process by which
it was arrived at.  Rationale such as "we removed X because Y" or "no bridge is
needed because…" belongs in commit messages, PR descriptions, or chat — not in
source files.  Keeping process reasoning out of comments avoids cluttering the
codebase with context that quickly becomes stale and misleading.

Concretely: a comment must read as true to someone who has never seen any
earlier revision of the code.  Revision-relative words — "previously", "new",
"now", "replaces", "retained", "no longer", "matching the previous X" — are
red flags: rewrite the comment in present tense ("idle waiters wake only on
actual state changes", not "this replaces fixed-interval polling"), or move
the remark to the commit message.

Comments should also avoid claims about *other* files or implementations
("the other hosts do X", "its twin implements Y") unless a test — typically
the conformance suite — enforces the claim: nothing keeps an unenforced
cross-file claim true, and a reader who trusts it first is misdirected
exactly when it matters.

Design rationale that shapes an interface — most importantly, what an
interface deliberately omits and why (see the `RTCDataChannelInit` note in
`wit/webrtc.wit`) — belongs in the interface's WIT doc comment, phrased as
forward-facing contract material rather than as a decision record ("what this
excludes and what to do if you need it", not "we decided against X").
Rationale for a change belongs in the commit or PR; everything else is
omitted.

### WIT comments are all doc comments

WIT tooling makes no semantic distinction between `//` and `///`: every
comment attached to an item lands in the parsed `docs` (adjacent `//`/`///`
runs are merged, and even a comment separated from the item by a blank line
still attaches), and `wit-bindgen`, `bindgen!`, and the JS toolchains all render those docs
into generated bindings. So there is no maintainer-only comment position in a
`.wit` file — write every sentence in one as if it will appear in a
consumer's rendered documentation, because it will. Interface contracts and
consumer-facing design rationale belong there; repo-layout plumbing (symlink
arrangements, sibling-implementation inventories) and maintainer asides do
not — they go to AGENTS.md, a README, or the commit message.

## Environment variables

The cross-process environment surface, in one place (each variable is also
documented at its use site):

| Variable | Read by | Effect |
| --- | --- | --- |
| `WEBRTC_UDP_BIND_ADDR` | `wasip3-impl` provider | IP address the in-guest `peer-connection` binds its UDP socket to (and derives its host candidate from); default IPv4 loopback. An unparsable value constructs dead connections (methods fail `closed`; the cause is printed to stderr). |
| `WEBRTC_MAX_INBOUND_BUFFER_BYTES` | all three implementations, but only the `wasip3-impl` guest reads it directly (the env var is its only channel); the host libraries read no ambient state — the Wasmtime hosts (demo binaries, conformance adapter) wire the variable through `WebrtcCtx`, and the polyengine children wire it through `polyengine-impl`'s exported `setMaxInboundBufferBytes` hook (embedders call the hook directly; there is no `globalThis` channel) | Overrides the 8 MiB inbound buffer bound; primarily a test knob (the conformance overflow probe shrinks it). Set-but-invalid values fail loud. |
| `WEBRTC_INCLUDE_LOOPBACK` | the `wasmtime-demo` binaries | Enables loopback ICE candidates so same-host peers can pair. |
| `CONFORMANCE_SHADOW_SYSCALL_SHIM` | `rtc-ct-driver` | Arms the Shadow syscall shim; set only by the Shadow executor on simulated wasmtime-kind peers. |
| `RTC_CT_ROLE`, `RTC_CT_SIGNALING_URL`, `RTC_CT_RUN_ID` | the conformance suites (via the store environment) | The pair-instance channel: which half this instance drives, the mailbox base URL, and the room-derivation seed. Set by `rtc-ct-driver` on the children it spawns; never set by hand. |
| `CHROME_PATH` / `CHROME_BIN` / `PUPPETEER_EXECUTABLE_PATH` | the browser test and browser conformance adapter | Chrome/Chromium binary override (first set one wins; else auto-detected). |
| `SKIP_NODE`, `SKIP_NETNS_LAB` | `scripts/setup.sh` | Skip the npm installs / the netns-lab tooling install. |

Every env var is an undeclared API each deployer must discover, and this
table is its only registry — so add no new implementation-level env var
without first considering the proper channel: `WebrtcCtx` on the Wasmtime
host, an exported configure hook for the polyengine host module (a module-level
knob captured at channel creation), or the WIT surface itself (as
`peer-connection-config` demonstrates for in-guest configuration).
`WEBRTC_UDP_BIND_ADDR` is environment-shaped on purpose: the bind address is
deployment topology, owned by whoever runs the process, not by the guest —
which is why it is not a `peer-connection-config` field.

## Real signaling (`rendezvous`): the two-process echo demo

The `webrtc-echo-demo` world stands up *both* peers inside one component
instance. The **two-process** demo makes the peers genuinely separate: the
`webrtc-echo-remote` world (implemented by [`examples/echo-remote`](examples/echo-remote))
drives **one** peer per instance, exchanging SDP and trickled ICE with the
other instance through the demo-only `demo:webrtc-echo/rendezvous` mailbox
interface, relayed via an HTTP signaling server (`conformance-signalingd`'s
protocol). The guest never speaks HTTP; three `rendezvous` implementations
exist:

- the Wasmtime demo host implements it natively (the `echo-remote` binary in
  `examples/wasmtime-demo`; `just examples::demo-remote`),
- the conformance polyengine children implement the same mailbox protocol over
  `fetch` (`conformance/driver-ct/polyengine/signaling.ts`), and
- [`examples/rendezvous-http`](examples/rendezvous-http) is a **component**
  implementing it over in-guest `wasi:http@0.3`, composable under the guest so
  a plain `wasmtime run -S http` provisions it (the fully in-guest path below).

## In-guest sans-I/O WebRTC (`wasip3-impl`) — direction

The two demo hosts run the fully async `webrtc-rs` engine host-side. To move the
WebRTC stack *into a wasm guest*, the protocol logic must be separated from I/O
so the guest can drive it over `wasi:sockets` and WASI timers. The sans-I/O
`rtc` 0.21 stack makes that possible: it compiles for `wasm32-wasip2`
(`ifaces()` returns `Unsupported` on wasm). The `rtc`
dependency is pinned once at the workspace level in the root `Cargo.toml`.

[`wasip3-impl/`](wasip3-impl) is that component: a `cdylib` built for
`wasm32-wasip2` that imports only `wasi:sockets`/`wasi:clocks` and **exports**
`polymorph:webrtc-datachannels/connections`. It has one core and one driver:

- `SansIoPeer` (`src/peer.rs`) is the runtime-agnostic core — it wraps an `rtc`
  `RTCPeerConnection` and exposes signaling primitives plus the six sans-I/O
  stepping calls (`poll_transmit` / `handle_input` / `poll_timeout` /
  `handle_timeout` + drained events), performing no I/O itself.
- The `runtime` module (`src/runtime.rs`) is the **in-guest** driver: it feeds
  the `SansIoPeer` from WASIp3 `wasi:sockets` UDP and `wasi:clocks` timers,
  running the event loop as a detached task (`wit_bindgen::spawn`), since the
  component-model async model is single-threaded with no cross-thread `spawn`.
- The `provider` module (`src/provider.rs`) implements the exported
  `connections` resources (`data-channel-options`, `data-channel`,
  `peer-connection`) on top of the driver.

Because it exports the package surface and imports only WASIp3 interfaces, it is
composable: [`examples/webrtc-consumer`](examples/webrtc-consumer) imports
`connections` and is composed with the provider via `wac plug`, then run under
`wasmtime` (`just examples::test-webrtc-composed`) — two peers connect over `wasi:sockets`
UDP loopback entirely in-guest and exchange a message each way.

Because the sans-I/O model has no OS interface enumeration, each
`peer-connection` supplies its own host candidate explicitly
(`add_local_host_candidate`) from the socket it binds, rather than gathering from
mDNS. `peer-connection` binds the IP address named by the `WEBRTC_UDP_BIND_ADDR`
environment variable, defaulting to IPv4 loopback (same-host peers); a routable
address gives the peer a host candidate reachable across a real network path,
as the conformance Shadow lab exercises. Paired with the `rendezvous` signaling
above, the **fully in-guest** two-process peer exists: `just
examples::test-echo-remote-composed` composes the echo-remote guest + `rendezvous-http`
(wasi:http signaling) + `wasip3-impl` (wasi:sockets WebRTC) under a CLI driver
and connects two plain `wasmtime run` processes — point `WEBRTC_UDP_BIND_ADDR`
and `--server` at routable addresses to run the same pair across real
machines.
