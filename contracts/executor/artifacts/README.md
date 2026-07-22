# ArbitrageExecutor artifacts (WHI-501 / WHI-551)

Reproducible optimized runtime **template** bytecode + ABI + AST + storage layout for
M0-9 gas profiling, M2-7 E2E, and the WHI-551 runtime-identity tooling
(`src/execution/runtime_identity.rs`, `config/executor_identity.json`).

"Template" means the `WMNT` immutable slot is still zero-filled, exactly as solc emits
`deployedBytecode.object` before deployment linking. `ArbitrageExecutor.codehash.txt`
is therefore a **template** hash, never a live on-chain identity — see
`WHI501_EXECUTOR_CODEHASH` (`src/execution/gas_profile.rs`, frozen historical pin for
the existing gas-profile data) vs `WHI501_EXECUTOR_PATCHED_RUNTIME_HASH`
(`src/execution/gas_runtime.rs`, the live mainnet identity, derived from this template
by `cargo run --example derive_runtime_identity`).

Regenerate:

```bash
export PATH="$HOME/.foundry/bin:$PATH"
cd contracts/executor
scripts/export_artifacts.sh
```

`export_artifacts.sh` runs `forge build --skip test` (the template must depend only on
`ArbitrageExecutor.sol`'s own source, not on whatever test files happen to live
alongside it in this Foundry root's shared via-IR compilation unit) and extracts:

| File | Purpose |
|------|---------|
| `ArbitrageExecutor.abi.json` | Final ABI |
| `ArbitrageExecutor.deployed.hex` | Template runtime bytecode (no 0x prefix, WMNT slot zero-filled) |
| `ArbitrageExecutor.codehash.txt` | `keccak256(template runtime)` |
| `ArbitrageExecutor.artifact.json` | Slim metadata + ABI + codehash |
| `ArbitrageExecutor.full.json` | Full forge compiler output: ABI, bytecode, `deployedBytecode` (incl. `immutableReferences`), `ast`, `storageLayout`, `metadata` |

`foundry.toml`'s `ast = true` / `extra_output = ["storageLayout"]` (added in WHI-551)
are output-selection flags only — they do not change `deployedBytecode`.

**WHI-501 does not deploy or fund.** Deployment is M2-7 / M2-9. Derive/verify the
patched runtime identity via `src/execution/runtime_identity.rs` and
`examples/derive_runtime_identity.rs` — see that module's doc comment for the
end-to-end command.
