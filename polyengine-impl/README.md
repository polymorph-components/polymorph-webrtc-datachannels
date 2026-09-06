# `polyengine-impl` — the polyengine-native `polymorph:webrtc-datachannels` host module

`src/webrtc.ts` is the [polyengine](https://github.com/polymorph-components/polyengine)-native
port of the browser-first reference host `jco-impl/webrtc.js` **at
commit 65bc15b** (retired with the jco legs;
`git show 65bc15b:jco-impl/webrtc.js`): the same
behavioral reference host, rewritten over polyengine's embedder API (typed
`Stream<T>` / `ReadableStream` rather than jco streams, and `ComponentException`
throws rather than `throw { tag, val }`). It was developed as polyengine's
own `ports/webrtc` reference-host port and is upstreamed here per
[polymorph-components/polyengine#14](https://github.com/polymorph-components/polyengine/issues/14); the WIT
contract is [`wit/webrtc.wit`](../wit/webrtc.wit), and every doc comment
quoting a contract quotes that file. The conformance suite's
`polyengine-deno` target asserts the behavioral parity with the other
implementations (`just conformance`).

## Backend

Isomorphic `RTCPeerConnection` resolution, as in the reference: a
browser global when one exists, otherwise the
[`node-datachannel`](https://www.npmjs.com/package/node-datachannel)
polyfill — a Node-API addon that works under Deno's node compatibility
(install it with `deno install --allow-scripts=npm:node-datachannel` in
this directory; the addon's install script needs the explicit grant).
`useWerift()` switches to the pure-TS
[`werift`](https://www.npmjs.com/package/werift) backend (no native
code); the conformance matrix runs the default backend only.

## Checks

```sh
just polyengine-check      # type-check + unit tests (this dir), leg type-check
```

The unit tests (`tests/webrtc_test.ts`) are in-process loopback pairs —
a subset of the protocol surface; the conformance suite
(`conformance/driver-ct/polyengine/`) is the real gate.

## Module identity

As of A22 (`@polyengine/runtime@0.5.0` / `@polyengine/protocol@0.2.2`), this
package depends on `@polyengine/protocol` only — host modules must not import
`@polyengine/runtime` (protocol copies are harmless by construction, so no
module-identity constraint applies to this package's pin). `deno.json` maps
`@polyengine/protocol` to a caret range (published dependency constraints
must be ranges). The runtime-module-identity constraint still applies between
the two driver configs that load the embedder,
[`conformance/driver-ct/polyengine/deno.json`](../conformance/driver-ct/polyengine/deno.json)
and its `browser/deno.json` sibling: see
[`conformance/driver-ct/polyengine/README.md`](../conformance/driver-ct/polyengine/README.md).
`just polyengine-check` asserts one resolved `@polyengine/runtime` version
across those two configs, one resolved `@polyengine/protocol` version across
all three, and that this package names no `@polyengine/runtime` specifier
(the pin gate, `scripts/check-polyengine-pin.sh`).
