// The polyengine browser child of rtc-ct-driver: the suite COMPONENT
// runtime-linked against this repo's polyengine host module (polyengine-impl
// over the browser's native RTCPeerConnection in a real headless
// Chromium) for one selection and (for pair runs) one role. Emits
// component-test results JSONL on stdout; the driver folds and merges.
// The server, launch, stall watchdog, and Chrome ladder live in the
// upstream browser driver; the run itself happens in the page
// (../../../target/polyengine-browser/webrtc-page.mjs, one deno-bundled
// module — RTCPeerConnection is Window-only, so no Web Worker pool, and
// the bundle needs no import map). This file is the frame: the
// signaling reverse-proxy, the role environment, target configuration,
// and the stdout contract — the browser sibling of ./run.ts (the retired
// jco legs carried the same sibling split; see git history).
//
// polyengine is a runtime linker: no transpile step, no generated tree, no
// engine flag (the callback ABI runs on stock Chromium).
import { access } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import {
  findChrome,
  runPageHarness,
} from "@jsr/polymorph__test/browser-driver";

const POLYENGINE_DIR = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(POLYENGINE_DIR, "..", "..", "..");
// The bundle + translator asset live in a version-free directory: the
// deno.lock files own the polyengine version now (no tag to keep in sync
// here; see justfile's `_polyengine-browser-bundle` recipe).
const PAGE_URL = "/target/polyengine-browser/webrtc-page.mjs";
const TRANSLATOR_URL = "/target/polyengine-browser/polyengine-translator-shim.wasm";
const MAX_INBOUND_BUFFER_BYTES = 512 * 1024;
// The single-attempt wall bound per case, matching the wasmtime leg.
const CASE_TIMEOUT_MS = 90_000;
// Pair cases rendezvous through the signaling server, so quiet time is
// bounded by the peer's slowest handshakes.
const STALL_TIMEOUT_MS = 120_000;

const { values } = parseArgs({
  options: {
    suite: {
      type: "string",
      default: "/target/wasm32-wasip2/release/conformance_guest_ct.wasm",
    },
    name: { type: "string", default: "conformance-guest-ct" },
    target: { type: "string", default: "polyengine-browser" },
    select: { type: "string", default: "" },
  },
});

/** Reverse-proxy one mailbox request to the driver's signaling server. */
async function proxy(req, res, signalingBase) {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  const body = Buffer.concat(chunks);
  let upstream;
  try {
    upstream = await fetch(`${signalingBase}${req.url}`, {
      method: req.method,
      headers: { "content-type": req.headers["content-type"] ?? "application/octet-stream" },
      body: req.method === "GET" || req.method === "HEAD" ? undefined : body,
    });
  } catch (err) {
    res.statusCode = 502;
    res.end(`proxy error: ${err}`);
    return;
  }
  res.statusCode = upstream.status;
  for (const [name, value] of upstream.headers) {
    if (["connection", "keep-alive", "transfer-encoding", "content-length"].includes(name)) {
      continue;
    }
    res.setHeader(name, value);
  }
  res.end(Buffer.from(await upstream.arrayBuffer()));
}

async function main() {
  for (const [what, rel] of [
    ["bundled page (run `just conformance::run-polyengine-browser`)", PAGE_URL],
    ["translator asset (run `just conformance::run-polyengine-browser`)", TRANSLATOR_URL],
    ["suite component (run `just conformance::build-suites`)", values.suite],
  ]) {
    try {
      await access(resolve(REPO_ROOT, `.${rel}`));
    } catch {
      throw new Error(`missing ${rel}: ${what}`);
    }
  }
  const signalingBase = process.env.RTC_CT_SIGNALING_URL;
  if (!signalingBase) {
    throw new Error("RTC_CT_SIGNALING_URL is not set (this child is spawned by rtc-ct-driver)");
  }

  const env = [["WEBRTC_MAX_INBOUND_BUFFER_BYTES", String(MAX_INBOUND_BUFFER_BYTES)]];
  if (process.env.RTC_CT_ROLE) env.push(["RTC_CT_ROLE", process.env.RTC_CT_ROLE]);
  if (process.env.RTC_CT_RUN_ID) env.push(["RTC_CT_RUN_ID", process.env.RTC_CT_RUN_ID]);
  const caseTimeoutMs = process.env.RTC_CT_CASE_TIMEOUT_SECS
    ? Number(process.env.RTC_CT_CASE_TIMEOUT_SECS) * 1000
    : CASE_TIMEOUT_MS;

  const config = {
    translatorUrl: TRANSLATOR_URL,
    suiteUrl: values.suite,
    name: values.name,
    target: values.target,
    select: values.select,
    env,
    maxInboundBufferBytes: MAX_INBOUND_BUFFER_BYTES,
    caseTimeoutMs,
  };
  const html = `<!doctype html>
<link rel="icon" href="data:,">
<title>conformance-ct polyengine browser leg</title>
<script type="module">
import { run } from "${PAGE_URL}";
await run(${JSON.stringify(config)});
</script>`;

  const playwright = await import("playwright-core");
  const outcome = await runPageHarness({
    playwright,
    engine: "chromium",
    executablePath: await findChrome(),
    repoRoot: REPO_ROOT,
    html,
    routes: async (req, res) => {
      const pathname = decodeURIComponent(req.url.split("?")[0]);
      if (pathname === "/healthz" || pathname.startsWith("/rooms/")) {
        await proxy(req, res, signalingBase);
        return true;
      }
      return false;
    },
    stallTimeoutMs: STALL_TIMEOUT_MS,
    launchArgs: [
      "--no-sandbox",
      "--disable-dev-shm-usage",
      "--use-fake-device-for-media-stream",
      "--use-fake-ui-for-media-stream",
    ],
  });

  process.stdout.write(`${outcome.lines.join("\n")}\n`);
  const { summary } = outcome;
  process.exit(summary.failed === 0 && summary.total > 0 ? 0 : 1);
}

main().then(
  () => {},
  (err) => {
    console.error(err);
    process.exit(2);
  },
);
