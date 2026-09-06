// The polyengine Deno child of rtc-ct-driver: one suite instance's stream — a
// selection and (for pair runs) a role — against the polyengine-native host
// module (`polyengine-impl/src/webrtc.ts`, backed by node-datachannel) and the
// fetch mailbox (`signaling.ts`). Emits component-test results JSONL on
// stdout; the driver folds and merges.
//
// Child contract (set by the driver): `RTC_CT_SIGNALING_URL` and
// `RTC_CT_RUN_ID` always; `RTC_CT_ROLE` on pair instances;
// `RTC_CT_CASE_TIMEOUT_SECS` bounds each case; `--select` carries the
// case-name prefix. The suite artifact is the BARE full suite — the exact
// component the retired jco legs transpiled — loaded directly: polyengine is a runtime
// linker, so there is no transpile step, no generated tree, and no engine
// flag; the WIT contract's async exports run on the callback ABI under
// stock Deno.
//
// The translator shim comes packaged with `@polyengine/translator` (the SAME
// pinned commit as the rest of the polyengine graph); `defaultTranslator()`
// loads it through the module graph, permission-free. `--translator
// <path>` remains as an optional override (a documented interface for
// swapping in a locally built shim), but is no longer required to run.
//
// MODULE-IDENTITY CONSTRAINT (A22): this leg's `deno.json` AND
// `browser/deno.json` still load the embedder (`@polyengine/runtime/embedder`)
// and must map it to the IDENTICAL pinned JSR version, or stateful handles
// minted by one copy are refused by another. `polyengine-impl/deno.json` no
// longer maps `@polyengine/runtime` at all — as of A22 it depends only on
// `@polyengine/protocol`, whose copies are harmless by construction, so it
// no longer participates in this constraint.

import { Translator } from "@polyengine/runtime/shim";
import { defaultTranslator } from "@polyengine/translator";
import type { ComponentArtifacts } from "@polyengine/runtime/embedder";
import { runSuite } from "@polyengine/ct-runner";
import { wasi } from "@polyengine/wasi";
import { setMaxInboundBufferBytes, webrtcImports } from "../../../polyengine-impl/src/webrtc.ts";
import { mailboxImports } from "./signaling.ts";

// This file sits at conformance/driver-ct/polyengine/run.ts, so the repo root
// is three levels up.
const ROOT = new URL("../../../", import.meta.url);

/** The default suite artifact: the bare full suite (see header). */
const SUITE_WASM = new URL(
  "target/wasm32-wasip2/release/conformance_guest_ct.wasm",
  ROOT,
).pathname;

// The inbound-buffer bound every leg applies (and exports to the suite
// through `WEBRTC_MAX_INBOUND_BUFFER_BYTES`): small enough that the
// overflow probe's 1 MiB flood overflows it. Channels capture the bound at
// creation, so configuring the module once covers every instance.
const MAX_INBOUND_BUFFER_BYTES = 512 * 1024;

/** The single-attempt wall bound per case when the driver names none. */
const DEFAULT_CASE_TIMEOUT_SECS = 90;

interface Cli {
  select: string;
  suite: string;
  name: string;
  target: string;
  translator?: string;
  jspi: boolean;
}

function parseArgs(argv: string[]): Cli {
  const cli: Cli = {
    select: "",
    suite: SUITE_WASM,
    name: "conformance-guest-ct",
    target: "polyengine-deno",
    jspi: false,
  };
  for (let i = 0; i < argv.length; i++) {
    switch (argv[i]) {
      case "--select":
        cli.select = argv[++i];
        break;
      case "--suite":
        cli.suite = argv[++i];
        break;
      case "--name":
        cli.name = argv[++i];
        break;
      case "--target":
        cli.target = argv[++i];
        break;
      case "--translator":
        cli.translator = argv[++i];
        break;
      case "--jspi":
        cli.jspi = true;
        break;
      default:
        throw new Error(`unknown argument ${argv[i]}`);
    }
  }
  return cli;
}

async function loadArtifacts(
  translatorPath: string | undefined,
  suitePath: string,
): Promise<ComponentArtifacts> {
  const translator = translatorPath
    ? await Translator.create(await Deno.readFile(translatorPath))
    : await defaultTranslator();
  const componentBytes = await Deno.readFile(suitePath);
  const { plan, adapters } = translator.translate(componentBytes);
  return { plan, componentBytes, adapters };
}

const encoder = new TextEncoder();

/** Write one JSONL line to stdout, whole (the driver captures stdout). */
function emitLine(line: string): void {
  const bytes = encoder.encode(line + "\n");
  let written = 0;
  while (written < bytes.length) {
    written += Deno.stdout.writeSync(bytes.subarray(written));
  }
}

async function main() {
  const cli = parseArgs(Deno.args);

  setMaxInboundBufferBytes(MAX_INBOUND_BUFFER_BYTES);

  // The suite's store environment: the buffer bound always, the pair-run
  // channel (role/signaling/run id) whenever the driver supplied it.
  const env: Record<string, string> = {
    WEBRTC_MAX_INBOUND_BUFFER_BYTES: String(MAX_INBOUND_BUFFER_BYTES),
  };
  for (const name of ["RTC_CT_ROLE", "RTC_CT_SIGNALING_URL", "RTC_CT_RUN_ID"]) {
    const value = Deno.env.get(name);
    if (value !== undefined) env[name] = value;
  }

  const caseTimeoutSecs = Number(
    Deno.env.get("RTC_CT_CASE_TIMEOUT_SECS") ?? DEFAULT_CASE_TIMEOUT_SECS,
  );

  const artifacts = await loadArtifacts(cli.translator, cli.suite);
  const imports = {
    ...wasi({ cli: { env } }),
    ...webrtcImports(),
    ...mailboxImports(),
  };

  // `runSuite`'s `only` is a substring filter; the selection contract is a
  // case-name PREFIX (`solo/` or `pair/`). The two agree on this corpus:
  // every case name starts with exactly one of the two topology prefixes
  // and neither string occurs mid-name.
  const counts = await runSuite(artifacts, {
    imports,
    target: cli.target,
    suiteName: cli.name,
    only: cli.select || undefined,
    caseTimeoutMs: caseTimeoutSecs * 1000,
    jspi: cli.jspi,
    emit: emitLine,
    log: (msg) => console.error(msg),
  });

  // Release node-datachannel's native worker threads before exit: without
  // the explicit cleanup() the process can die in native teardown (SIGSEGV)
  // after the stream is already complete.
  try {
    const nodeDatachannel = await import("node-datachannel");
    nodeDatachannel.cleanup?.();
  } catch {
    // Not resolved to node-datachannel in this run — nothing to clean up.
  }

  if (counts.failed > 0) Deno.exitCode = 1;
}

if (import.meta.main) await main();
