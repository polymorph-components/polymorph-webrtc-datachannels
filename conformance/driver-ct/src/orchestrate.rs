//! Orchestrator modes: provision the signaling server, spawn one child
//! results stream per suite instance (a solo stream, or an
//! offerer/answerer pair — self-exec, a Node script, or per-case
//! reference-peer processes), fold the pair sides case-wise, and emit
//! one canonical stream per target.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context as _, Result};
use component_test_results::{CaseResult, Status};

use crate::stream;

/// How one side of a run produces its stream.
#[derive(Clone)]
pub enum PeerKind {
    /// Self-exec: the wasmtime leg (hosted or composed) over `artifact`.
    SelfExec {
        artifact: PathBuf,
        composed: bool,
        suite_artifact: Option<PathBuf>,
    },
    /// A Node script (the polyengine browser leg: the page driver runs under
    /// Node, no engine flag): emits the same JSONL contract.
    Node { script: PathBuf, args: Vec<String> },
    /// A Deno script (the polyengine leg): stock Deno — no engine flag — with
    /// the script directory's own `deno.json`/`deno.lock` governing its
    /// module graph; emits the same JSONL contract.
    Deno { script: PathBuf, args: Vec<String> },
    /// The native libwebrtc reference peer: one process per case,
    /// verdicts synthesized into the stream by this orchestrator.
    Reference { bin: PathBuf },
}

pub struct RunContext {
    pub signaling_url: String,
    pub run_id: String,
    pub case_timeout_secs: u64,
    pub budget_secs: u64,
}

/// The pair cases of `inventory_artifact`, in suite order (the order
/// every side runs, so lockstep rendezvous holds).
pub fn selected_cases(inventory_artifact: &Path, prefix: &str) -> Result<Vec<String>> {
    let bytes = std::fs::read(inventory_artifact)
        .with_context(|| format!("reading {}", inventory_artifact.display()))?;
    let inv = component_test_formats::inventory::inventory(&bytes)?;
    Ok(inv
        .cases
        .into_iter()
        .map(|c| c.name.to_string())
        .filter(|n| n.starts_with(prefix))
        .collect())
}

/// Spawn one child stream for `kind` and collect it. `role: None` is a
/// solo run over `select`; `Some` is one side of a pair run.
pub fn child_stream(
    kind: &PeerKind,
    ctx: &RunContext,
    select: &str,
    role: Option<&str>,
    jobs: usize,
    cases: &[String],
) -> Result<component_test_results::Document> {
    let raw = match kind {
        PeerKind::SelfExec {
            artifact,
            composed,
            suite_artifact,
        } => {
            let mut cmd = Command::new(std::env::current_exe()?);
            cmd.arg("exec")
                .arg(artifact)
                .arg("--jsonl")
                .arg("--select")
                .arg(select)
                .arg("--jobs")
                .arg(jobs.to_string())
                .arg("--budget")
                .arg(ctx.budget_secs.to_string())
                .arg("--case-timeout")
                .arg(ctx.case_timeout_secs.to_string());
            if *composed {
                cmd.arg("--composed");
            }
            if let Some(sa) = suite_artifact {
                cmd.arg("--suite-artifact").arg(sa);
            }
            if let Some(role) = role {
                cmd.arg("--role")
                    .arg(role)
                    .arg("--signaling")
                    .arg(&ctx.signaling_url)
                    .arg("--run-id")
                    .arg(&ctx.run_id);
            }
            run_capture(cmd, &format!("{}[{}]", "self-exec", role.unwrap_or("solo")))?
        }
        PeerKind::Node { script, args } => {
            let script = script
                .canonicalize()
                .with_context(|| format!("resolving script {}", script.display()))?;
            let mut cmd = Command::new("node");
            cmd.arg(&script).arg("--select").arg(select).args(args);
            cmd.env("RTC_CT_SIGNALING_URL", &ctx.signaling_url)
                .env("RTC_CT_RUN_ID", &ctx.run_id)
                .env(
                    "RTC_CT_CASE_TIMEOUT_SECS",
                    ctx.case_timeout_secs.to_string(),
                );
            if let Some(role) = role {
                cmd.env("RTC_CT_ROLE", role);
            }
            cmd.current_dir(script.parent().context("script has no parent dir")?);
            run_capture(cmd, &format!("node[{}]", role.unwrap_or("solo")))?
        }
        PeerKind::Deno { script, args } => {
            let script = script
                .canonicalize()
                .with_context(|| format!("resolving script {}", script.display()))?;
            let dir = script.parent().context("script has no parent dir")?;
            let mut cmd = Command::new("deno");
            cmd.arg("run")
                .arg("--allow-all")
                .arg("--frozen")
                .arg("--config")
                .arg(dir.join("deno.json"))
                .arg(&script)
                .arg("--select")
                .arg(select)
                .args(args);
            cmd.env("RTC_CT_SIGNALING_URL", &ctx.signaling_url)
                .env("RTC_CT_RUN_ID", &ctx.run_id)
                .env(
                    "RTC_CT_CASE_TIMEOUT_SECS",
                    ctx.case_timeout_secs.to_string(),
                );
            if let Some(role) = role {
                cmd.env("RTC_CT_ROLE", role);
            }
            cmd.current_dir(dir);
            run_capture(cmd, &format!("deno[{}]", role.unwrap_or("solo")))?
        }
        PeerKind::Reference { bin } => {
            let role = role.context("the reference peer only runs pair roles")?;
            return reference_stream(bin, ctx, role, cases);
        }
    };
    stream::parse_stream(&raw, cases)
}

/// Run the reference peer once per case — all cases concurrently, each
/// process blocking in its own mailbox room until the counterpart
/// arrives, so the suite side's execution order is irrelevant —
/// synthesizing the stream the wasm sides emit natively.
fn reference_stream(
    bin: &Path,
    ctx: &RunContext,
    role: &str,
    cases: &[String],
) -> Result<component_test_results::Document> {
    let results: Vec<CaseResult> = std::thread::scope(|scope| {
        let handles: Vec<_> = cases
            .iter()
            .map(|case| scope.spawn(move || reference_case(bin, ctx, role, case)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .map_err(|_| anyhow::anyhow!("reference case thread panicked"))?
            })
            .collect::<Result<Vec<_>>>()
    })?;
    Ok(component_test_results::Document {
        envelope: component_test_results::Envelope::new("reference", ""),
        results,
        run_errors: Vec::new(),
        unknown_statuses: BTreeMap::new(),
        terminated: true,
    })
}

fn reference_case(bin: &Path, ctx: &RunContext, role: &str, case: &str) -> Result<CaseResult> {
    let leaf = case.rsplit('/').next().unwrap_or(case);
    let (count, size) = reference_params(leaf);
    let started = std::time::Instant::now();
    let out = Command::new(bin)
        .arg("--test")
        .arg(leaf)
        .arg("--role")
        .arg(role)
        .arg("--server")
        .arg(&ctx.signaling_url)
        .arg("--room")
        .arg(format!("{}-{leaf}", ctx.run_id))
        .arg("--message-count")
        .arg(count.to_string())
        .arg("--message-size")
        .arg(size.to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("spawning reference peer {}", bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().last().unwrap_or("");
    let verdict: serde_json::Value = serde_json::from_str(line).with_context(|| {
        format!("reference peer emitted no verdict for {case} (stdout: {stdout:?})")
    })?;
    let (status, detail) = match verdict.get("tag").and_then(|t| t.as_str()) {
        Some("pass") => (Status::Pass, None),
        Some("fail") => (
            Status::Fail,
            verdict
                .get("val")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        ),
        Some("skipped") => (
            Status::Skipped,
            verdict
                .get("val")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        ),
        other => bail!("reference peer verdict has unknown tag {other:?}"),
    };
    Ok(CaseResult {
        case: case.to_string(),
        status,
        provenance: None,
        detail,
        seed: None,
        duration_ms: Some(started.elapsed().as_millis() as u64),
        diagnostics: Vec::new(),
        diagnostics_complete: true,
    })
}

/// The per-case message count and size the reference peer is driven
/// with. Must agree with the suite's own stimulus
/// (`conformance-suite-body`'s `params`); a mismatch fails the
/// count-parameterized cases loudly on both sides.
pub fn reference_params(leaf: &str) -> (u32, u32) {
    match leaf {
        "large-message" => (1, 16 * 1024),
        "message-boundaries"
        | "ordering"
        | "payload-integrity"
        | "concurrent-send-receive"
        | "interop-handshake" => (16, 512),
        _ => (4, 256),
    }
}

fn run_capture(mut cmd: Command, label: &str) -> Result<String> {
    cmd.stdin(Stdio::null()).stderr(Stdio::inherit());
    let out = cmd
        .output()
        .with_context(|| format!("spawning {label} child"))?;
    let raw = String::from_utf8(out.stdout).context("child stream is not UTF-8")?;
    // A child that failed may still have emitted a full stream (a failing
    // case exits nonzero by design); stream parsing decides.
    if let Ok(dir) = std::env::var("RTC_CT_DEBUG_DIR") {
        let name: String = label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let _ = std::fs::write(format!("{dir}/{name}-{}.jsonl", std::process::id()), &raw);
    }
    Ok(raw)
}

/// Spawn the two sides of a pair run concurrently and fold their streams.
pub fn pair_streams(
    offerer: &PeerKind,
    answerer: &PeerKind,
    ctx: &RunContext,
    select: &str,
    cases: &[String],
) -> Result<(Vec<CaseResult>, Vec<String>)> {
    let (off, ans) = std::thread::scope(|scope| {
        let off = scope.spawn(|| child_stream(offerer, ctx, select, Some("offerer"), 1, cases));
        let ans = scope.spawn(|| child_stream(answerer, ctx, select, Some("answerer"), 1, cases));
        (off.join(), ans.join())
    });
    let off = off.map_err(|_| anyhow::anyhow!("offerer thread panicked"))??;
    let ans = ans.map_err(|_| anyhow::anyhow!("answerer thread panicked"))??;
    let mut run_errors: Vec<String> = Vec::new();
    run_errors.extend(off.run_errors.iter().map(|e| format!("offerer: {e}")));
    run_errors.extend(ans.run_errors.iter().map(|e| format!("answerer: {e}")));
    Ok((stream::fold_pair(off, ans), run_errors))
}

/// Start the suite signaling server on an ephemeral loopback port and
/// keep it alive on its own runtime for the process lifetime.
pub fn start_signaling() -> Result<String> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<String>>();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(e.into()));
                return;
            }
        };
        rt.block_on(async move {
            match conformance_signalingd::spawn(
                "127.0.0.1:0".parse().unwrap(),
                conformance_signalingd::Config::default(),
            )
            .await
            {
                Ok(server) => {
                    let _ = tx.send(Ok(server.base_url()));
                    let _server = server;
                    std::future::pending::<()>().await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        });
    });
    rx.recv().context("signaling server setup")?
}

/// A unique, room-charset-safe run id.
pub fn run_id(target: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{target}-{}-{nanos}", std::process::id())
}

/// Merge a solo document and folded pair results into one target stream.
pub fn merge_target(
    target: &str,
    suite_artifact: &Path,
    run_id: &str,
    solo: Option<component_test_results::Document>,
    pair: (Vec<CaseResult>, Vec<String>),
) -> Result<String> {
    let suite_name = suite_artifact
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "suite".into());
    let bytes = std::fs::read(suite_artifact)
        .with_context(|| format!("reading {}", suite_artifact.display()))?;
    let sha = component_test_formats::sha256_hex(&bytes);
    let mut run_errors = Vec::new();
    // The streams carry disjoint selections (`--select solo/` vs `pair/`)
    // over one census, and each reports the remainder of the census as
    // `deselected` (selection policy — runner-policy.md, "Selection is
    // not capability"). Reassembling the target, a deselected row must
    // not displace the verdict from the stream selected to run the case:
    // a case stays deselected only when every stream deselected it.
    let mut results: BTreeMap<String, CaseResult> = BTreeMap::new();
    let merge =
        |map: &mut BTreeMap<String, CaseResult>, r: CaseResult| match map.entry(r.case.clone()) {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(r);
            }
            std::collections::btree_map::Entry::Occupied(mut o) => {
                if r.status != Status::Deselected || o.get().status == Status::Deselected {
                    o.insert(r);
                }
            }
        };
    if let Some(doc) = solo {
        run_errors.extend(doc.run_errors.iter().map(|e| format!("solo: {e}")));
        for r in doc.results {
            merge(&mut results, r);
        }
    }
    let (pair_results, pair_errors) = pair;
    run_errors.extend(pair_errors);
    for r in pair_results {
        merge(&mut results, r);
    }
    let results: Vec<CaseResult> = results.into_values().collect();
    stream::emit(
        target,
        &suite_name,
        Some(sha),
        run_id,
        &run_errors,
        &results,
    )
}
