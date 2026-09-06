//! `rtc-ct-driver`: the polymorph:test legs of the conformance harness.
//!
//! One binary, two layers. Child mode (`exec`) runs one suite
//! instance's stream: a role, a selection, a linker profile — the
//! component-test runner owns scheduling, isolation (fresh instance per
//! case), budgets, and the results wire format. Orchestrator modes
//! (`loopback`, `interop`) provision the signaling server, spawn one
//! child stream per instance (self-exec, a Node script, or per-case
//! reference-peer processes), fold the two sides of each pair case, and
//! emit one canonical JSONL stream per target.

mod exec;
mod host;
mod lab;
mod lab_types;
mod netns;
mod orchestrate;
mod shadow;
mod shadow_shim;
mod stream;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context as _, Result};

use orchestrate::PeerKind;

const USAGE: &str = "usage: rtc-ct-driver exec <suite.wasm> [--jsonl] [--select prefix] \
     [--role offerer|answerer --signaling url --run-id id] [--composed] \
     [--suite-artifact path] [--jobs n] [--budget secs] [--case-timeout secs]
        rtc-ct-driver loopback <suite.wasm> --target name --kind wasmtime|composed|polyengine-deno|polyengine-browser \
     [--artifact path] [--script path] [-o out.jsonl]
        rtc-ct-driver interop <pair-suite.wasm> --direction <offerer>-x-<answerer> \
     [--composed-pair path] [--deno-script path] [--browser-script path] \
     [--reference-bin path] [-o out.jsonl]";

/// Wall bound per case: the single-attempt hang guard (no retries). Must
/// exceed the hosts' own `wait-connected` timeouts (30 s at worst) so
/// genuine failures classify as failures and only true hangs trip it.
const CASE_TIMEOUT_SECS: u64 = 90;
/// Execution (CPU) budget per case: generous for the composed target,
/// where the whole SCTP/DTLS stack burns guest cycles.
const BUDGET_SECS: u64 = 30;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("exec") => exec_cmd(args.collect()),
        Some("loopback") => loopback_cmd(args.collect()),
        Some("interop") => interop_cmd(args.collect()),
        Some("shadow") => shadow_cmd(args.collect()),
        Some("netns") => netns_cmd(args.collect()),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        _ => bail!("{USAGE}"),
    }
}

fn exec_cmd(argv: Vec<String>) -> Result<ExitCode> {
    let mut args = exec::ExecArgs {
        suite: PathBuf::new(),
        target: "exec".into(),
        select: None,
        role: None,
        signaling_url: None,
        run_id: None,
        composed: false,
        ice: None,
        suite_artifact: None,
        jobs: 1,
        budget_secs: BUDGET_SECS,
        case_timeout_secs: CASE_TIMEOUT_SECS,
        jsonl: false,
    };
    let mut suite = None;
    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match arg.as_str() {
            "--jsonl" => args.jsonl = true,
            "--select" => args.select = Some(value("--select")?),
            "--role" => args.role = Some(value("--role")?),
            "--signaling" => args.signaling_url = Some(value("--signaling")?),
            "--run-id" => args.run_id = Some(value("--run-id")?),
            "--composed" => args.composed = true,
            "--ice-bind" => {
                args.ice.get_or_insert_with(Default::default).bind_addr = Some(value("--ice-bind")?)
            }
            "--ice-stun" => {
                args.ice.get_or_insert_with(Default::default).stun_url = Some(value("--ice-stun")?)
            }
            "--ice-turn" => {
                let spec = value("--ice-turn")?;
                let mut parts = spec.splitn(3, ',');
                let url = parts.next().unwrap_or_default().to_string();
                let user = parts.next().unwrap_or_default().to_string();
                let pass = parts.next().unwrap_or_default().to_string();
                args.ice.get_or_insert_with(Default::default).turn = Some((url, user, pass));
            }
            "--ice-relay-only" => args.ice.get_or_insert_with(Default::default).relay_only = true,
            "--disable-mdns" => args.ice.get_or_insert_with(Default::default).disable_mdns = true,
            "--suite-artifact" => {
                args.suite_artifact = Some(PathBuf::from(value("--suite-artifact")?))
            }
            "--target" => args.target = value("--target")?,
            "--jobs" => args.jobs = value("--jobs")?.parse().context("--jobs")?,
            "--budget" => args.budget_secs = value("--budget")?.parse().context("--budget")?,
            "--case-timeout" => {
                args.case_timeout_secs =
                    value("--case-timeout")?.parse().context("--case-timeout")?
            }
            s if s.starts_with('-') => bail!("unknown flag `{s}`\n{USAGE}"),
            other => {
                if suite.is_some() {
                    bail!("unexpected argument `{other}`");
                }
                suite = Some(PathBuf::from(other));
            }
        }
    }
    args.suite = suite.context("missing suite.wasm")?;
    exec::run(args)
}

/// One loopback target: a solo stream over `solo/` plus a role pair over
/// `pair/`, merged. `--kind` picks the child flavor; `--artifact` is the
/// executed artifact when it differs from the suite (the composed build).
fn loopback_cmd(argv: Vec<String>) -> Result<ExitCode> {
    let mut suite = None;
    let mut target = None;
    let mut kind = None;
    let mut artifact = None;
    let mut script = None;
    let mut out = None;
    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match arg.as_str() {
            "--target" => target = Some(value("--target")?),
            "--kind" => kind = Some(value("--kind")?),
            "--artifact" => artifact = Some(PathBuf::from(value("--artifact")?)),
            "--script" => script = Some(PathBuf::from(value("--script")?)),
            "-o" => out = Some(PathBuf::from(value("-o")?)),
            s if s.starts_with('-') => bail!("unknown flag `{s}`\n{USAGE}"),
            other => {
                if suite.is_some() {
                    bail!("unexpected argument `{other}`");
                }
                suite = Some(PathBuf::from(other));
            }
        }
    }
    let suite: PathBuf = suite.context("missing suite.wasm")?;
    let target = target.context("missing --target")?;
    let kind = kind.context("missing --kind")?;

    let peer = match kind.as_str() {
        "wasmtime" => PeerKind::SelfExec {
            artifact: suite.clone(),
            composed: false,
            suite_artifact: None,
        },
        "composed" => PeerKind::SelfExec {
            artifact: artifact
                .clone()
                .context("--kind composed needs --artifact")?,
            composed: true,
            suite_artifact: Some(suite.clone()),
        },
        "polyengine-deno" => PeerKind::Deno {
            script: script
                .clone()
                .context("--kind polyengine-deno needs --script")?,
            args: Vec::new(),
        },
        "polyengine-browser" => PeerKind::Node {
            script: script
                .clone()
                .context("--kind polyengine-browser needs --script")?,
            args: Vec::new(),
        },
        other => bail!("unknown --kind {other:?}"),
    };

    let signaling_url = orchestrate::start_signaling()?;
    let ctx = orchestrate::RunContext {
        signaling_url,
        run_id: orchestrate::run_id(&target),
        case_timeout_secs: CASE_TIMEOUT_SECS,
        budget_secs: BUDGET_SECS,
    };
    let solo_cases = orchestrate::selected_cases(&suite, "solo/")?;
    let pair_cases = orchestrate::selected_cases(&suite, "pair/")?;

    let solo = orchestrate::child_stream(&peer, &ctx, "solo/", None, 4, &solo_cases)?;
    let pair = orchestrate::pair_streams(&peer, &peer, &ctx, "pair/", &pair_cases)?;
    let merged = orchestrate::merge_target(&target, &suite, &ctx.run_id, Some(solo), pair)?;
    write_out(out.as_deref(), &merged)?;
    Ok(exit_for(&merged))
}

/// One interop direction over the pair-only suite: `<offerer>-x-<answerer>`,
/// each side by target kind, the reference peer as per-case processes.
fn interop_cmd(argv: Vec<String>) -> Result<ExitCode> {
    let mut suite = None;
    let mut direction = None;
    let mut composed_pair = None;
    let mut deno_script = None;
    let mut browser_script = None;
    let mut reference_bin = PathBuf::from("target/release/conformance-reference-peer");
    let mut target = None;
    let mut out = None;
    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match arg.as_str() {
            "--direction" => direction = Some(value("--direction")?),
            "--composed-pair" => composed_pair = Some(PathBuf::from(value("--composed-pair")?)),
            "--deno-script" => deno_script = Some(PathBuf::from(value("--deno-script")?)),
            "--browser-script" => browser_script = Some(PathBuf::from(value("--browser-script")?)),
            "--reference-bin" => reference_bin = PathBuf::from(value("--reference-bin")?),
            "--target" => target = Some(value("--target")?),
            "-o" => out = Some(PathBuf::from(value("-o")?)),
            s if s.starts_with('-') => bail!("unknown flag `{s}`\n{USAGE}"),
            other => {
                if suite.is_some() {
                    bail!("unexpected argument `{other}`");
                }
                suite = Some(PathBuf::from(other));
            }
        }
    }
    let suite: PathBuf = suite.context("missing pair-suite.wasm")?;
    let direction = direction.context("missing --direction")?;
    let target = target.unwrap_or_else(|| direction.clone());
    let Some((offerer_kind, answerer_kind)) = direction.split_once("-x-") else {
        bail!("--direction must be <offerer>-x-<answerer>");
    };

    let side = |name: &str| -> Result<PeerKind> {
        Ok(match name {
            "wasmtime" => PeerKind::SelfExec {
                artifact: suite.clone(),
                composed: false,
                suite_artifact: None,
            },
            "wasip3-guest" => PeerKind::SelfExec {
                artifact: composed_pair
                    .clone()
                    .context("direction includes wasip3-guest: needs --composed-pair")?,
                composed: true,
                suite_artifact: Some(suite.clone()),
            },
            "polyengine-deno" => PeerKind::Deno {
                script: deno_script
                    .clone()
                    .context("direction includes polyengine-deno: needs --deno-script")?,
                args: pair_suite_args(&suite, PairSuitePath::Filesystem)?,
            },
            "polyengine-browser" => PeerKind::Node {
                script: browser_script
                    .clone()
                    .context("direction includes polyengine-browser: needs --browser-script")?,
                args: pair_suite_args(&suite, PairSuitePath::Served)?,
            },
            "reference" => PeerKind::Reference {
                bin: reference_bin.clone(),
            },
            other => bail!("unknown interop side {other:?}"),
        })
    };
    let offerer = side(offerer_kind)?;
    let answerer = side(answerer_kind)?;

    let signaling_url = orchestrate::start_signaling()?;
    let ctx = orchestrate::RunContext {
        signaling_url,
        run_id: orchestrate::run_id(&target),
        case_timeout_secs: CASE_TIMEOUT_SECS,
        budget_secs: BUDGET_SECS,
    };
    let pair_cases = orchestrate::selected_cases(&suite, "pair/")?;
    let pair = orchestrate::pair_streams(&offerer, &answerer, &ctx, "pair/", &pair_cases)?;
    let merged = orchestrate::merge_target(&target, &suite, &ctx.run_id, None, pair)?;
    write_out(out.as_deref(), &merged)?;
    Ok(exit_for(&merged))
}

/// One Shadow-lab row: `<offerer-kind>` and `<answerer-kind>` peers on
/// simulated hosts over the pair suite.
fn shadow_cmd(argv: Vec<String>) -> Result<ExitCode> {
    let mut args = shadow::ShadowArgs {
        pair_suite: PathBuf::new(),
        composed_pair: None,
        offerer_kind: "wasmtime".into(),
        answerer_kind: "wasmtime".into(),
        target: String::new(),
        shadow_bin: PathBuf::from("shadow"),
        signaling_bin: PathBuf::from("target/debug/conformance-signalingd"),
        reference_bin: PathBuf::from("target/release/conformance-reference-peer"),
        data_dir: PathBuf::from("target/shadow-data"),
        out: None,
    };
    let mut suite = None;
    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match arg.as_str() {
            "--composed-pair" => {
                args.composed_pair = Some(PathBuf::from(value("--composed-pair")?))
            }
            "--offerer-kind" => args.offerer_kind = value("--offerer-kind")?,
            "--answerer-kind" => args.answerer_kind = value("--answerer-kind")?,
            "--target" => args.target = value("--target")?,
            "--shadow-bin" => args.shadow_bin = PathBuf::from(value("--shadow-bin")?),
            "--signaling-bin" => args.signaling_bin = PathBuf::from(value("--signaling-bin")?),
            "--reference-bin" => args.reference_bin = PathBuf::from(value("--reference-bin")?),
            "--data-dir" => args.data_dir = PathBuf::from(value("--data-dir")?),
            "-o" => args.out = Some(PathBuf::from(value("-o")?)),
            s if s.starts_with('-') => bail!("unknown flag `{s}`\n{USAGE}"),
            other => {
                if suite.is_some() {
                    bail!("unexpected argument `{other}`");
                }
                suite = Some(PathBuf::from(other));
            }
        }
    }
    args.pair_suite = suite.context("missing pair-suite.wasm")?;
    if args.target.is_empty() {
        let base = if args.offerer_kind == args.answerer_kind {
            args.offerer_kind.clone()
        } else {
            format!("{}-x-{}", args.offerer_kind, args.answerer_kind)
        };
        args.target = format!("{base}-shadow");
    }
    shadow::run(args)
}

/// One netns-lab row: the wasmtime pair over a routed scenario.
fn netns_cmd(argv: Vec<String>) -> Result<ExitCode> {
    let mut args = netns::NetnsArgs {
        pair_suite: PathBuf::new(),
        scenario: "lan".into(),
        target: None,
        signaling_bin: PathBuf::from("target/debug/conformance-signalingd"),
        out: None,
    };
    let mut suite = None;
    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{name} needs a value"))
        };
        match arg.as_str() {
            "--scenario" => args.scenario = value("--scenario")?,
            "--target" => args.target = Some(value("--target")?),
            "--signaling-bin" => args.signaling_bin = PathBuf::from(value("--signaling-bin")?),
            "-o" => args.out = Some(PathBuf::from(value("-o")?)),
            s if s.starts_with('-') => bail!("unknown flag `{s}`\n{USAGE}"),
            other => {
                if suite.is_some() {
                    bail!("unexpected argument `{other}`");
                }
                suite = Some(PathBuf::from(other));
            }
        }
    }
    args.pair_suite = suite.context("missing pair-suite.wasm")?;
    netns::run(args)
}

fn write_out(path: Option<&std::path::Path>, merged: &str) -> Result<()> {
    match path {
        Some(path) => std::fs::write(path, merged).with_context(|| format!("writing {path:?}")),
        None => {
            print!("{merged}");
            Ok(())
        }
    }
}

/// Exit red when the merged stream carries any failing case or run
/// error; the aggregate re-judges everything against the manifest, but a
/// red leg should fail fast at the recipe that ran it.
pub(crate) fn exit_for(merged: &str) -> ExitCode {
    let red = merged.lines().skip(1).any(|line| {
        line.contains("\"status\":\"fail\"")
            || line.contains("\"status\":\"not-reached\"")
            || line.contains("\"run-error\"")
    });
    if red {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// How an interop child addresses the pair-suite artifact.
enum PairSuitePath {
    /// A filesystem path (the Deno child reads the wasm directly; it runs
    /// with the script's directory as cwd, so the path must be absolute).
    Filesystem,
    /// A path under the browser child's static server, which serves the
    /// repository root (so a repo-root-relative path with a leading `/`).
    Served,
}

/// The polyengine children's pair-artifact selection (the interop matrix runs
/// the pair-only suite; the loopback legs run the full one — the children
/// default to it and take `--suite`/`--name` overrides).
fn pair_suite_args(suite: &std::path::Path, form: PairSuitePath) -> Result<Vec<String>> {
    let suite_arg = match form {
        PairSuitePath::Filesystem => suite
            .canonicalize()
            .with_context(|| format!("resolving pair suite {}", suite.display()))?
            .display()
            .to_string(),
        PairSuitePath::Served => {
            if suite.is_absolute() {
                bail!(
                    "pair suite {} must be repo-root-relative for the browser child's static server",
                    suite.display()
                );
            }
            format!("/{}", suite.display())
        }
    };
    Ok(vec![
        "--suite".into(),
        suite_arg,
        "--name".into(),
        "conformance-guest-pair-ct".into(),
    ])
}
