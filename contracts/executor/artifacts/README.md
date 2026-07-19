# ArbitrageExecutor artifacts (WHI-501)

Reproducible optimized runtime bytecode + ABI for M0-9 gas profiling and M2-7 E2E.

Regenerate:

```bash
export PATH="$HOME/.foundry/bin:$PATH"
cd contracts/executor
forge build
# then re-run the artifact extract step from WHI-501 tooling / CI
```

| File | Purpose |
|------|---------|
| `ArbitrageExecutor.abi.json` | Final ABI |
| `ArbitrageExecutor.deployed.hex` | Runtime bytecode (no 0x prefix) |
| `ArbitrageExecutor.codehash.txt` | `keccak256(runtime)` |
| `ArbitrageExecutor.artifact.json` | Slim metadata + ABI + codehash |
| `ArbitrageExecutor.full.json` | Full forge compiler output |

**WHI-501 does not deploy or fund.** Deployment is M2-7 / M2-9.
