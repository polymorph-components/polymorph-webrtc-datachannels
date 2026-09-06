// Browser-bundle stub for the npm WebRTC backends: in the page,
// polyengine-impl's isomorphic resolution takes `globalThis.RTCPeerConnection`
// and the dynamic backend imports are never executed — this stub exists so
// `deno bundle --platform browser` (browser/deno.json maps the specifiers
// here) does not inline node-datachannel's Node-API graph and its `node:*`
// externals into the page module.
export const RTCPeerConnection = undefined;
export function cleanup(): void {}
export default { cleanup };
