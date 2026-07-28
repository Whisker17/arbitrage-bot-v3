//! Evaluate a completed WHI-554 shadow run's per-service ledgers against a
//! signed `GatePlan`'s thresholds, producing a byte-reproducible `ShadowReport`.
//!
//! ```bash
//! cargo run --example shadow_report -- generate \
//!   --gate-plan shadow_gate_plan.json --gate-plan-signature shadow_gate_plan.sig \
//!   --gate-plan-principal operator \
//!   --chain-id 5000 --git-commit "$(git rev-parse HEAD)" \
//!   --service v2_monitor_executor_service --service moe_monitor_executor_service \
//!   --thresholds config/gas_profiles/shadow_thresholds_evidence.example.json \
//!   --ledger v2_monitor.jsonl --ledger moe_monitor.jsonl \
//!   --json-out shadow_report.json
//! ```
//!
//! Freshly re-verifies `--gate-plan`/`--gate-plan-signature` in this process
//! (never trusts a prior `shadow_gate_plan verify` run) and always resolves
//! the committed, code-constant trust roots. A report is written to
//! `--json-out` even when the run is ineligible -- only the process exit code
//! (nonzero whenever `!verdict_eligible` or generation itself failed) signals
//! pass/fail to a calling script.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use amms::execution::shadow_gate_plan::{
    GatePlanVerifier, ProductionGatePlanVerifier, ShadowGateScope, GATE_PLAN_SCHEMA_VERSION,
};
use amms::execution::shadow_report::{evaluate, LedgerInput};
use amms::execution::shadow_thresholds;
use amms::signing::{self, canonical};
use clap::{Parser, Subcommand};
use eyre::{eyre, Context, Result};

#[derive(Parser, Debug)]
#[command(name = "shadow_report")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// The `(chain_id, git_commit, services)` triple needed to rebuild the
/// [`ShadowGateScope`] the GatePlan was signed against, bundled so the three
/// fields travel as one named thing (mirrors
/// `examples/shadow_gate_plan.rs`'s `ScopeArgs`).
#[derive(clap::Args, Debug)]
struct ScopeArgs {
    #[arg(long)]
    chain_id: u64,
    #[arg(long)]
    git_commit: String,
    /// Repeatable: the GatePlan's full required_services set, used to
    /// rebuild the expected scope it was signed against. At least one is
    /// required.
    #[arg(long = "service")]
    services: Vec<String>,
}

impl ScopeArgs {
    fn into_scope(self) -> Result<ShadowGateScope> {
        if self.services.is_empty() {
            return Err(eyre!("at least one --service is required"));
        }
        Ok(ShadowGateScope {
            chain_id: self.chain_id,
            git_commit: self.git_commit,
            required_services: self.services,
        })
    }
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Re-verify a signed GatePlan, evaluate the supplied per-service
    /// ledgers against its thresholds, and write a canonical ShadowReport.
    Generate {
        #[arg(long)]
        gate_plan: PathBuf,
        #[arg(long)]
        gate_plan_signature: PathBuf,
        #[arg(long)]
        gate_plan_principal: String,
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(long)]
        thresholds: PathBuf,
        /// Repeatable: one ledger JSONL file per required service.
        #[arg(long = "ledger")]
        ledgers: Vec<PathBuf>,
        #[arg(long)]
        json_out: PathBuf,
    },
}

/// DI-16 partial mitigation, mirroring `examples/shadow_gate_plan.rs`.
fn require_trust_roots() -> Result<()> {
    let allowed_signers_path = signing::config::allowed_signers_path();
    let revoked_keys_path = signing::config::revoked_keys_path();
    if !allowed_signers_path.exists() {
        return Err(eyre!(
            "expected allowed_signers at {}; has config/signers/ been provisioned for this deployment?",
            allowed_signers_path.display()
        ));
    }
    if !revoked_keys_path.exists() {
        return Err(eyre!(
            "expected revoked_keys at {}; has config/signers/ been provisioned for this deployment?",
            revoked_keys_path.display()
        ));
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("shadow_report: run is NOT verdict_eligible (see --json-out for details)");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("shadow_report failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<bool> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Generate {
            gate_plan,
            gate_plan_signature,
            gate_plan_principal,
            scope,
            thresholds,
            ledgers,
            json_out,
        } => cmd_generate(
            &gate_plan,
            &gate_plan_signature,
            &gate_plan_principal,
            scope,
            &thresholds,
            &ledgers,
            &json_out,
        ),
    }
}

fn cmd_generate(
    gate_plan: &PathBuf,
    gate_plan_signature: &PathBuf,
    gate_plan_principal: &str,
    scope_args: ScopeArgs,
    thresholds: &PathBuf,
    ledgers: &[PathBuf],
    json_out: &PathBuf,
) -> Result<bool> {
    let scope = scope_args.into_scope()?;

    require_trust_roots()?;

    if ledgers.is_empty() {
        return Err(eyre!("at least one --ledger is required"));
    }

    let thresholds_bytes =
        fs::read(thresholds).with_context(|| format!("read {}", thresholds.display()))?;
    let validated = shadow_thresholds::validate(&thresholds_bytes)
        .map_err(|e| eyre!("thresholds at {} invalid: {e}", thresholds.display()))?;

    let gate_plan_bytes =
        fs::read(gate_plan).with_context(|| format!("read {}", gate_plan.display()))?;
    let gate_plan_signature_bytes = fs::read(gate_plan_signature)
        .with_context(|| format!("read {}", gate_plan_signature.display()))?;

    let expected_scope = scope
        .to_expected_scope()
        .map_err(|e| eyre!("build expected scope: {e}"))?;

    let verified = ProductionGatePlanVerifier
        .verify(
            &gate_plan_bytes,
            &gate_plan_signature_bytes,
            gate_plan_principal,
            &[GATE_PLAN_SCHEMA_VERSION],
            &expected_scope,
        )
        .map_err(|e| eyre!("verify gate plan: {e}"))?;

    let mut ledger_inputs = Vec::with_capacity(ledgers.len());
    for path in ledgers {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        ledger_inputs.push(LedgerInput {
            label: path.display().to_string(),
            bytes,
        });
    }

    let report = evaluate(
        &gate_plan_bytes,
        verified.payload(),
        &validated,
        &ledger_inputs,
        scope.chain_id,
    )
    .map_err(|e| eyre!("evaluate shadow report: {e}"))?;

    let report_value = serde_json::to_value(&report).context("serialize shadow report")?;
    let canonical_bytes =
        canonical::canonicalize_value(&report_value).map_err(|e| eyre!("canonicalize report: {e}"))?;
    if let Some(parent) = json_out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(json_out, &canonical_bytes)
        .with_context(|| format!("write {}", json_out.display()))?;

    println!("wrote {}", json_out.display());
    println!("verdict_eligible={}", report.verdict_eligible);
    for (service, evaluation) in &report.per_service {
        println!("  service={service} passed={}", evaluation.passed);
        for reason in &evaluation.failure_reasons {
            println!("    - {reason}");
        }
    }
    if !report.overall.passed {
        for reason in &report.overall.failure_reasons {
            println!("  overall: {reason}");
        }
    }
    if !report.invariant_violations.is_empty() {
        println!("invariant_violations={}", report.invariant_violations.len());
        for violation in &report.invariant_violations {
            println!(
                "  service={} digest={} kind={} detail={}",
                violation.service, violation.digest, violation.kind, violation.detail
            );
        }
    }

    Ok(report.verdict_eligible)
}
