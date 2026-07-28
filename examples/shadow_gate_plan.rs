//! Create, sign, and verify a WHI-554 shadow-mode `GatePlan` artifact.
//!
//! ```bash
//! cargo run --example shadow_gate_plan -- create \
//!   --chain-id 5000 --git-commit "$(git rev-parse HEAD)" \
//!   --service v2_monitor_executor_service --service moe_monitor_executor_service \
//!   --thresholds config/gas_profiles/shadow_thresholds_evidence.example.json \
//!   --config-digest 0x... --profile-digest 0x... --runtime-identity-digest 0x... \
//!   --out shadow_gate_plan.json
//!
//! cargo run --example shadow_gate_plan -- sign \
//!   --gate-plan shadow_gate_plan.json --key ~/.ssh/id_ed25519 \
//!   --principal operator --sig-out shadow_gate_plan.sig
//!
//! cargo run --example shadow_gate_plan -- verify \
//!   --gate-plan shadow_gate_plan.json --signature shadow_gate_plan.sig \
//!   --principal operator --chain-id 5000 --git-commit "$(git rev-parse HEAD)" \
//!   --service v2_monitor_executor_service --service moe_monitor_executor_service
//! ```
//!
//! `create` writes the unsigned canonical envelope; `sign` rewrites
//! `--gate-plan` in-place to its canonical signed form (the exact bytes the
//! signature covers) and writes the detached signature to `--sig-out` —
//! refusing to overwrite an existing `--sig-out` unless `--force`, since a
//! `GatePlan` must never be silently re-signed. `verify` re-derives the
//! expected scope from CLI arguments rather than trusting anything recorded
//! in the file, and always resolves the committed, code-constant trust roots
//! (never a path supplied on the command line).

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use amms::execution::shadow_gate_plan::{
    build_envelope, digest_file_bytes, GatePlanPayload, GatePlanVerifier, ProductionGatePlanVerifier,
    ShadowGateScope, GATE_PLAN_DOMAIN, GATE_PLAN_SCHEMA_VERSION,
};
use amms::execution::shadow_thresholds;
use amms::signing::{self, CanonicalEnvelope};
use clap::{Parser, Subcommand};
use eyre::{eyre, Context, Result};

#[derive(Parser, Debug)]
#[command(name = "shadow_gate_plan")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// The `(chain_id, git_commit, services)` triple every subcommand needs to
/// rebuild the [`ShadowGateScope`] an artifact is signed against. Bundled into
/// one `#[command(flatten)]`-ed type so the three fields travel together
/// (they always do) instead of being re-declared per subcommand, threaded
/// through as three separate function parameters, and re-assembled into a
/// `ShadowGateScope` by hand at each call site.
#[derive(clap::Args, Debug)]
struct ScopeArgs {
    #[arg(long)]
    chain_id: u64,
    #[arg(long)]
    git_commit: String,
    /// Repeatable: one per required service. At least one is required.
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
    /// Build the unsigned GatePlan envelope from a thresholds artifact and
    /// the environment digests in effect right now.
    Create {
        #[command(flatten)]
        scope: ScopeArgs,
        #[arg(long)]
        thresholds: PathBuf,
        #[arg(long)]
        config_digest: String,
        #[arg(long)]
        profile_digest: String,
        #[arg(long)]
        runtime_identity_digest: String,
        #[arg(long)]
        out: PathBuf,
    },
    /// Sign an unsigned GatePlan file produced by `create`.
    Sign {
        #[arg(long)]
        gate_plan: PathBuf,
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
    /// Verify a signed GatePlan against an independently-supplied expected
    /// scope.
    Verify {
        #[arg(long)]
        gate_plan: PathBuf,
        #[arg(long)]
        signature: PathBuf,
        #[arg(long)]
        principal: String,
        #[command(flatten)]
        scope: ScopeArgs,
    },
}

/// DI-16 partial mitigation: fail with an operator-facing message before
/// spawning `ssh-keygen` against a trust-root path that doesn't exist yet,
/// instead of an opaque subprocess failure.
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
        eprintln!("shadow_gate_plan failed: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Create {
            scope,
            thresholds,
            config_digest,
            profile_digest,
            runtime_identity_digest,
            out,
        } => cmd_create(
            scope,
            &thresholds,
            config_digest,
            profile_digest,
            runtime_identity_digest,
            &out,
        ),
        Cmd::Sign {
            gate_plan,
            key,
            principal,
            sig_out,
            force,
        } => cmd_sign(&gate_plan, &key, &principal, &sig_out, force),
        Cmd::Verify {
            gate_plan,
            signature,
            principal,
            scope,
        } => cmd_verify(&gate_plan, &signature, &principal, scope),
    }
}

fn cmd_create(
    scope_args: ScopeArgs,
    thresholds: &PathBuf,
    config_digest: String,
    profile_digest: String,
    runtime_identity_digest: String,
    out: &PathBuf,
) -> Result<()> {
    let scope = scope_args.into_scope()?;

    let (allowed_signers_path, revoked_keys_path) = require_trust_roots()?;

    let thresholds_bytes = fs::read(thresholds)
        .with_context(|| format!("read {}", thresholds.display()))?;
    let validated = shadow_thresholds::validate(&thresholds_bytes)
        .map_err(|e| eyre!("thresholds at {} invalid: {e}", thresholds.display()))?;

    let allowed_signers_digest = digest_file_bytes(&allowed_signers_path)
        .map_err(|e| eyre!("digest allowed_signers: {e}"))?;
    let revoked_keys_digest =
        digest_file_bytes(&revoked_keys_path).map_err(|e| eyre!("digest revoked_keys: {e}"))?;

    let payload = GatePlanPayload {
        thresholds_digest: validated.digest,
        git_commit: scope.git_commit.clone(),
        config_digest,
        profile_digest,
        runtime_identity_digest,
        allowed_signers_digest,
        revoked_keys_digest,
        required_services: scope.required_services.clone(),
    };
    let envelope = build_envelope(&scope, payload);

    let encoded = serde_json::to_vec_pretty(&envelope).context("encode gate plan envelope")?;
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(out, &encoded).with_context(|| format!("write {}", out.display()))?;

    println!("wrote {}", out.display());
    println!("thresholds_digest={}", envelope.payload.thresholds_digest);
    Ok(())
}

fn cmd_sign(gate_plan: &PathBuf, key: &PathBuf, principal: &str, sig_out: &PathBuf, force: bool) -> Result<()> {
    if sig_out.exists() && !force {
        return Err(eyre!(
            "{} already exists; pass --force to overwrite (a GatePlan must never be re-signed silently)",
            sig_out.display()
        ));
    }

    let envelope_bytes =
        fs::read(gate_plan).with_context(|| format!("read {}", gate_plan.display()))?;
    let envelope: CanonicalEnvelope<GatePlanPayload> =
        serde_json::from_slice(&envelope_bytes).context("parse gate plan envelope")?;

    let (payload_bytes, signature) = signing::sign_envelope(key, GATE_PLAN_DOMAIN, &envelope)
        .map_err(|e| eyre!("sign gate plan: {e}"))?;

    // Rewrite the gate-plan file to its exact canonical form so `verify`
    // operates on precisely the bytes the signature covers.
    fs::write(gate_plan, &payload_bytes)
        .with_context(|| format!("write canonical {}", gate_plan.display()))?;
    if let Some(parent) = sig_out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(sig_out, &signature).with_context(|| format!("write {}", sig_out.display()))?;

    println!(
        "signed {} as {principal} -> {}",
        gate_plan.display(),
        sig_out.display()
    );
    Ok(())
}

fn cmd_verify(
    gate_plan: &PathBuf,
    signature: &PathBuf,
    principal: &str,
    scope_args: ScopeArgs,
) -> Result<()> {
    let scope = scope_args.into_scope()?;

    require_trust_roots()?;

    let payload_bytes =
        fs::read(gate_plan).with_context(|| format!("read {}", gate_plan.display()))?;
    let signature_bytes =
        fs::read(signature).with_context(|| format!("read {}", signature.display()))?;

    let expected_scope = scope
        .to_expected_scope()
        .map_err(|e| eyre!("build expected scope: {e}"))?;

    let verified = ProductionGatePlanVerifier
        .verify(
            &payload_bytes,
            &signature_bytes,
            principal,
            &[GATE_PLAN_SCHEMA_VERSION],
            &expected_scope,
        )
        .map_err(|e| eyre!("verify gate plan: {e}"))?;

    let payload = verified.payload();
    println!("verified gate plan for principal {principal}");
    println!("thresholds_digest={}", payload.thresholds_digest);
    println!("config_digest={}", payload.config_digest);
    println!("profile_digest={}", payload.profile_digest);
    println!("runtime_identity_digest={}", payload.runtime_identity_digest);
    Ok(())
}
