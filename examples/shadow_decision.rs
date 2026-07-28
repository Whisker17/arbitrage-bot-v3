//! Create, sign, and verify a WHI-554 human go/no-go `Decision` artifact over
//! a completed shadow run.
//!
//! ```bash
//! cargo run --example shadow_decision -- create \
//!   --gate-plan shadow_gate_plan.json --gate-plan-signature shadow_gate_plan.sig \
//!   --gate-plan-principal operator \
//!   --chain-id 5000 --git-commit "$(git rev-parse HEAD)" \
//!   --service v2_monitor_executor_service --service moe_monitor_executor_service \
//!   --report shadow_report.json \
//!   --ledger v2_monitor.jsonl --ledger moe_monitor.jsonl \
//!   --verdict approve --decision-principal operator \
//!   --out shadow_decision.json
//!
//! cargo run --example shadow_decision -- sign \
//!   --decision shadow_decision.json --key ~/.ssh/id_ed25519 \
//!   --principal operator --sig-out shadow_decision.sig
//!
//! cargo run --example shadow_decision -- verify \
//!   --decision shadow_decision.json --signature shadow_decision.sig \
//!   --principal operator --chain-id 5000 --git-commit "$(git rev-parse HEAD)" \
//!   --service v2_monitor_executor_service --service moe_monitor_executor_service
//! ```
//!
//! `create` freshly re-verifies `--gate-plan`/`--gate-plan-signature` in this
//! process (never trusts a prior `shadow_gate_plan verify` run) and
//! recomputes `ledger_digest`/`report_digest`/`gate_plan_digest` from its own
//! `--ledger`/`--report`/`--gate-plan` inputs, rejecting a substituted ledger
//! or report before a verdict is ever recorded. `--verdict approve` is
//! refused (nonzero exit) unless the parsed `--report` is itself
//! `verdict_eligible`; `--verdict reject` is always permitted. `sign`/
//! `verify` mirror `shadow_gate_plan`'s shape exactly, fixed to the Decision
//! domain/schema, with the same sign-overwrite guard and code-constant trust
//! roots.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use amms::execution::shadow_decision::{
    build_envelope, check_approve_eligibility, DecisionPayload, DecisionVerifier,
    ProductionDecisionVerifier, Verdict, GATE_DECISION_DOMAIN, GATE_DECISION_SCHEMA_VERSION,
};
use amms::execution::shadow_gate_plan::{
    digest_file_bytes, GatePlanVerifier, ProductionGatePlanVerifier, ShadowGateScope,
    GATE_PLAN_SCHEMA_VERSION,
};
use amms::execution::shadow_report::{ledger_digest, ledger_header_service, ShadowReport};
use amms::signing::{self, CanonicalEnvelope};
use clap::{Parser, Subcommand};
use eyre::{eyre, Context, Result};

#[derive(Parser, Debug)]
#[command(name = "shadow_decision")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Re-verify a signed GatePlan, cross-check the supplied report/ledgers
    /// against it, and build the (unsigned) Decision envelope.
    Create {
        #[arg(long)]
        gate_plan: PathBuf,
        #[arg(long)]
        gate_plan_signature: PathBuf,
        #[arg(long)]
        gate_plan_principal: String,
        #[arg(long)]
        chain_id: u64,
        #[arg(long)]
        git_commit: String,
        /// Repeatable: the GatePlan's full required_services set, used to
        /// rebuild the expected scope it was signed against.
        #[arg(long = "service")]
        services: Vec<String>,
        #[arg(long)]
        report: PathBuf,
        /// Repeatable: one ledger JSONL file per required service, re-supplied
        /// so `ledger_digest` can be recomputed and cross-checked against the
        /// value embedded in `--report`.
        #[arg(long = "ledger")]
        ledgers: Vec<PathBuf>,
        #[arg(long)]
        verdict: VerdictArg,
        #[arg(long)]
        decision_principal: String,
        #[arg(long)]
        out: PathBuf,
    },
    /// Sign an unsigned Decision file produced by `create`.
    Sign {
        #[arg(long)]
        decision: PathBuf,
        #[arg(long)]
        key: PathBuf,
        /// Operator identity this key is expected to verify as. Not passed
        /// to the signing call itself (OpenSSH establishes that only at
        /// verify time) -- recorded here purely for the operator's own
        /// bookkeeping.
        #[arg(long)]
        principal: String,
        #[arg(long)]
        sig_out: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Verify a signed Decision against an independently-supplied expected
    /// scope.
    Verify {
        #[arg(long)]
        decision: PathBuf,
        #[arg(long)]
        signature: PathBuf,
        #[arg(long)]
        principal: String,
        #[arg(long)]
        chain_id: u64,
        #[arg(long)]
        git_commit: String,
        #[arg(long = "service")]
        services: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum VerdictArg {
    Approve,
    Reject,
}

impl From<VerdictArg> for Verdict {
    fn from(v: VerdictArg) -> Self {
        match v {
            VerdictArg::Approve => Verdict::Approve,
            VerdictArg::Reject => Verdict::Reject,
        }
    }
}

/// DI-16 partial mitigation, mirroring `examples/shadow_gate_plan.rs`. Returns
/// the resolved paths so callers needing to digest their bytes (`cmd_create`)
/// don't re-resolve them.
fn require_trust_roots() -> Result<(PathBuf, PathBuf)> {
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
    Ok((allowed_signers_path, revoked_keys_path))
}

fn main() -> ExitCode {
    if let Err(e) = run() {
        eprintln!("shadow_decision failed: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Create {
            gate_plan,
            gate_plan_signature,
            gate_plan_principal,
            chain_id,
            git_commit,
            services,
            report,
            ledgers,
            verdict,
            decision_principal,
            out,
        } => cmd_create(
            &gate_plan,
            &gate_plan_signature,
            &gate_plan_principal,
            chain_id,
            git_commit,
            services,
            &report,
            &ledgers,
            verdict.into(),
            decision_principal,
            &out,
        ),
        Cmd::Sign {
            decision,
            key,
            principal,
            sig_out,
            force,
        } => cmd_sign(&decision, &key, &principal, &sig_out, force),
        Cmd::Verify {
            decision,
            signature,
            principal,
            chain_id,
            git_commit,
            services,
        } => cmd_verify(&decision, &signature, &principal, chain_id, git_commit, services),
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_create(
    gate_plan: &PathBuf,
    gate_plan_signature: &PathBuf,
    gate_plan_principal: &str,
    chain_id: u64,
    git_commit: String,
    services: Vec<String>,
    report: &PathBuf,
    ledgers: &[PathBuf],
    verdict: Verdict,
    decision_principal: String,
    out: &PathBuf,
) -> Result<()> {
    let (allowed_signers_path, revoked_keys_path) = require_trust_roots()?;

    if services.is_empty() {
        return Err(eyre!("at least one --service is required"));
    }
    if ledgers.is_empty() {
        return Err(eyre!("at least one --ledger is required"));
    }

    let gate_plan_bytes =
        fs::read(gate_plan).with_context(|| format!("read {}", gate_plan.display()))?;
    let gate_plan_signature_bytes = fs::read(gate_plan_signature)
        .with_context(|| format!("read {}", gate_plan_signature.display()))?;

    let scope = ShadowGateScope {
        chain_id,
        git_commit,
        required_services: services,
    };
    let expected_scope = scope
        .to_expected_scope()
        .map_err(|e| eyre!("build expected scope: {e}"))?;

    // Freshly re-verify the GatePlan in this process -- never trust a prior
    // `shadow_gate_plan verify` run.
    ProductionGatePlanVerifier
        .verify(
            &gate_plan_bytes,
            &gate_plan_signature_bytes,
            gate_plan_principal,
            &[GATE_PLAN_SCHEMA_VERSION],
            &expected_scope,
        )
        .map_err(|e| eyre!("verify gate plan: {e}"))?;

    let report_bytes = fs::read(report).with_context(|| format!("read {}", report.display()))?;
    let parsed_report: ShadowReport =
        serde_json::from_slice(&report_bytes).context("parse shadow report")?;

    let gate_plan_digest = to_hex0x_keccak256(&gate_plan_bytes);
    if gate_plan_digest != parsed_report.gate_plan_digest {
        return Err(eyre!(
            "gate_plan_digest mismatch: --gate-plan hashes to {gate_plan_digest}, but --report was evaluated against {}",
            parsed_report.gate_plan_digest
        ));
    }

    let mut raw_by_service = std::collections::BTreeMap::new();
    for path in ledgers {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let service = ledger_header_service(&path.display().to_string(), &bytes)
            .map_err(|e| eyre!("read ledger service from {}: {e}", path.display()))?;
        if raw_by_service.insert(service.clone(), bytes).is_some() {
            return Err(eyre!("duplicate ledger for service {service}"));
        }
    }
    let recomputed_ledger_digest = ledger_digest(&raw_by_service);
    if recomputed_ledger_digest != parsed_report.ledger_digest {
        return Err(eyre!(
            "ledger_digest mismatch: --ledger files hash to {recomputed_ledger_digest}, but --report was evaluated against {}",
            parsed_report.ledger_digest
        ));
    }

    check_approve_eligibility(verdict, parsed_report.verdict_eligible)
        .map_err(|e| eyre!("{e}"))?;

    let allowed_signers_digest = digest_file_bytes(&allowed_signers_path)
        .map_err(|e| eyre!("digest allowed_signers: {e}"))?;
    let revoked_keys_digest =
        digest_file_bytes(&revoked_keys_path).map_err(|e| eyre!("digest revoked_keys: {e}"))?;

    let report_digest = to_hex0x_keccak256(&report_bytes);
    let payload = DecisionPayload {
        gate_plan_digest,
        ledger_digest: recomputed_ledger_digest,
        report_digest,
        verdict,
        decision_principal,
        allowed_signers_digest,
        revoked_keys_digest,
    };
    let envelope = build_envelope(&scope, payload);

    let encoded = serde_json::to_vec_pretty(&envelope).context("encode decision envelope")?;
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(out, &encoded).with_context(|| format!("write {}", out.display()))?;

    println!("wrote {}", out.display());
    println!("verdict={:?}", envelope.payload.verdict);
    println!("report_digest={}", envelope.payload.report_digest);
    Ok(())
}

fn to_hex0x_keccak256(bytes: &[u8]) -> String {
    use alloy::primitives::keccak256;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = keccak256(bytes);
    let mut out = String::with_capacity(2 + 64);
    out.push_str("0x");
    for &b in digest.as_slice() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn cmd_sign(decision: &PathBuf, key: &PathBuf, principal: &str, sig_out: &PathBuf, force: bool) -> Result<()> {
    if sig_out.exists() && !force {
        return Err(eyre!(
            "{} already exists; pass --force to overwrite (a Decision must never be re-signed silently)",
            sig_out.display()
        ));
    }

    let envelope_bytes =
        fs::read(decision).with_context(|| format!("read {}", decision.display()))?;
    let envelope: CanonicalEnvelope<DecisionPayload> =
        serde_json::from_slice(&envelope_bytes).context("parse decision envelope")?;

    let (payload_bytes, signature) = signing::sign_envelope(key, GATE_DECISION_DOMAIN, &envelope)
        .map_err(|e| eyre!("sign decision: {e}"))?;

    // Rewrite the decision file to its exact canonical form so `verify`
    // operates on precisely the bytes the signature covers.
    fs::write(decision, &payload_bytes)
        .with_context(|| format!("write canonical {}", decision.display()))?;
    if let Some(parent) = sig_out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(sig_out, &signature).with_context(|| format!("write {}", sig_out.display()))?;

    println!(
        "signed {} as {principal} -> {}",
        decision.display(),
        sig_out.display()
    );
    Ok(())
}

fn cmd_verify(
    decision: &PathBuf,
    signature: &PathBuf,
    principal: &str,
    chain_id: u64,
    git_commit: String,
    services: Vec<String>,
) -> Result<()> {
    require_trust_roots()?;

    let payload_bytes =
        fs::read(decision).with_context(|| format!("read {}", decision.display()))?;
    let signature_bytes =
        fs::read(signature).with_context(|| format!("read {}", signature.display()))?;

    let scope = ShadowGateScope {
        chain_id,
        git_commit,
        required_services: services,
    };
    let expected_scope = scope
        .to_expected_scope()
        .map_err(|e| eyre!("build expected scope: {e}"))?;

    let verified = ProductionDecisionVerifier
        .verify(
            &payload_bytes,
            &signature_bytes,
            principal,
            &[GATE_DECISION_SCHEMA_VERSION],
            &expected_scope,
        )
        .map_err(|e| eyre!("verify decision: {e}"))?;

    println!("verified decision for principal {principal}");
    println!("verdict={:?}", verified.payload().verdict);
    println!("decision_principal={}", verified.payload().decision_principal);
    println!("gate_plan_digest={}", verified.payload().gate_plan_digest);
    println!("ledger_digest={}", verified.payload().ledger_digest);
    println!("report_digest={}", verified.payload().report_digest);
    Ok(())
}
