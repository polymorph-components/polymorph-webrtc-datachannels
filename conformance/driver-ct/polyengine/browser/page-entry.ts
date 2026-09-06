// The polyengine-browser page: the endpoint of one browser child of
// rtc-ct-driver — the suite COMPONENT runtime-linked against this repo's
// polyengine host module (polyengine-impl over the browser's native
// RTCPeerConnection) and the fetch mailbox, for one selection and (for
// pair runs) one role. RTCPeerConnection is a Window-only API, so the run
// happens here in the page; this module is served as ONE deno-bundled
// file (polyengine engine + host module + mailbox in a single graph, so
// exactly one embedder module instance exists) and needs no import map.
//
// The page's mailbox fetches are same-origin (`/rooms/*`, `/healthz`);
// the Node child (../run-browser.mjs) reverse-proxies them to the
// driver's signaling server and forwards this page's `__report`ed lines
// to stdout for the driver's fold.

import { runSuite, wasi } from "./browser-bundle-entry.ts";
import { Translator } from "./browser-bundle-entry.ts";
import { setMaxInboundBufferBytes, webrtcImports } from "../../../../polyengine-impl/src/webrtc.ts";
import { mailboxImports } from "../signaling.ts";

interface PageConfig {
  translatorUrl: string;
  suiteUrl: string;
  name: string;
  target: string;
  select: string;
  env: [string, string][];
  maxInboundBufferBytes: number;
  caseTimeoutMs: number;
}

declare global {
  interface Window {
    __progress(note: string): Promise<void> | void;
    // deno-lint-ignore no-explicit-any
    __report(outcome: any): Promise<void> | void;
  }
}

async function fetchBytes(url: string): Promise<Uint8Array> {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`fetching ${url}: ${res.status}`);
  return new Uint8Array(await res.arrayBuffer());
}

const beat = (note: string) => {
  try {
    // deno-lint-ignore no-explicit-any
    (window.__progress(note) as any)?.catch?.(() => {});
  } catch {
    // A closing page must not turn a heartbeat into an unhandled rejection.
  }
};

export async function run(config: PageConfig): Promise<void> {
  try {
    setMaxInboundBufferBytes(config.maxInboundBufferBytes);

    // The mailbox base is this page's own origin: the Node child
    // reverse-proxies `/rooms/*` to the driver's signaling server.
    const env = Object.fromEntries([
      ...config.env,
      ["RTC_CT_SIGNALING_URL", window.location.origin],
    ]);

    beat("fetching artifacts");
    const [translatorBytes, suiteBytes] = await Promise.all([
      fetchBytes(config.translatorUrl),
      fetchBytes(config.suiteUrl),
    ]);
    const translator = await Translator.create(translatorBytes);
    const { plan, adapters } = translator.translate(suiteBytes);

    const imports = {
      ...wasi({ cli: { env } }),
      ...webrtcImports(),
      ...mailboxImports(),
    };

    const lines: string[] = [];
    let rows = 0;
    const counts = await runSuite(
      { plan, componentBytes: suiteBytes, adapters },
      {
        imports,
        target: config.target,
        suiteName: config.name,
        only: config.select || undefined,
        caseTimeoutMs: config.caseTimeoutMs,
        emit: (line: string) => lines.push(line),
        log: (msg: string) => {
          rows += 1;
          if (rows % 10 === 0) beat(`row ${rows}: ${msg}`);
        },
      },
    );
    window.__report({ lines, summary: { failed: counts.failed, total: counts.total } });
  } catch (err) {
    // deno-lint-ignore no-explicit-any
    window.__report({ error: String((err as any)?.stack ?? err) });
  }
}
