# Mantle mainnet — ArbitrageExecutor deployment (unfunded, paused)

**Date:** 2026-08-02
**Chain:** Mantle mainnet, chain id **5000**
**Repo commit at deploy:** `a273b95`
**Contract:** `contracts/executor/ArbitrageExecutor.sol:ArbitrageExecutor`

> **Deviation notice — read this first.** This deployment did **not** follow
> WHI-547's runbook as written. It was performed at the repo owner's explicit and
> repeated instruction, overriding several controls that issue specifies. The
> deviations are enumerated below rather than glossed. Nothing here should be read
> as WHI-547 being satisfied.

## Result

| Field | Value |
| --- | --- |
| Address | `0xDC9A6B8f7756860c0caC3e3573587D2CF9d0A4bF` |
| Deploy tx | `0xfb81de0ad5e3235353e105c6bcd34058f8dd4d724df2b2ff51ae32e8e4e5cf5a` |
| Deploy block | `98770971` |
| Pause tx | `0xb4162c77ac7706eed884d2a1d7e536affae6c5cf53f1383fc30d786928483c23` |
| Pause block | `98770975` |
| Deployer / admin | `0x6A00754e22A4fcde9B5290da7A3367dfF96f6486` |
| Nonces | deploy = 0, pause = 1 (consecutive) |
| Cost | ≈ 0.1433 MNT |

## Post-containment verification

All read back from mainnet after the pause landed:

| Check | Observed | Expected |
| --- | --- | --- |
| `chain_id` | `5000` | 5000 |
| `codehash` | `0xe1acd0f6ce3257330a9ef37cf7ff29867f3533178c6ccee1a4c3ac167ad3e699` | identical to fork rehearsal |
| `WMNT()` | `0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8` | mainnet WMNT |
| `admin()` | `0x6A00754e22A4fcde9B5290da7A3367dfF96f6486` | deployer |
| `guardian()` | `0x0000000000000000000000000000000000000000` | unset |
| `paused()` | `true` | **true** |
| `isHotExecutor(deployer)` | `false` | false |
| native balance | `0` | 0 |
| WMNT balance | `0` | 0 |

The codehash matches the fork rehearsal byte-for-byte, which is the strongest
identity evidence available without the WHI-551 plan (see deviations).

## Fork rehearsal (performed first)

The full sequence was rehearsed on `anvil --fork-url <mainnet>` at block 98770928
before any mainnet transaction. The rehearsal produced the identical address
(deterministic from deployer + nonce 0) and the identical codehash.

**The rehearsal caught a real defect in WHI-547's runbook command.** As written:

```
forge create --root contracts/executor ArbitrageExecutor.sol:ArbitrageExecutor \
  --rpc-url <rpc> --constructor-args <wmnt> <cold_admin> --interactive --broadcast
```

`--constructor-args` is variadic and greedily consumes the following tokens, so
`--interactive --broadcast` are parsed as constructor arguments:

```
Error: Constructor argument count mismatch: expected 2 but got 3
```

The flags must precede `--constructor-args`. **WHI-547's runbook should be corrected.**

The rehearsal also caught an operator error in the first attempt: the contract
address was extracted with a greedy `grep` that matched the *deployer* address
printed earlier in `forge create` output, so `pause()` was sent to an EOA. It
returned `status 1` because a call to a codeless address succeeds trivially — the
contract was left **unpaused** while the transaction looked successful. On mainnet
this is precisely WHI-547 step 4's abort condition. Always verify `paused()` reads
`true` rather than trusting the pause transaction's status.

## Deviations from WHI-547 — none of these were satisfied

1. **No verified Approve Decision.** WHI-547 step 1 requires a fresh WHI-552
   verification of a signed WHI-526 Approve. WHI-526 closed as a signed **Reject**
   (`unlock_criteria: reject_blocks_go_live_and_m3`), and the artifact directory
   `evidence/shadow/whi526-local/` no longer exists on disk. No decision was
   verified; none could be.
2. **No provisioned signers.** `config/signers/allowed_signers` contains zero
   principals (all comments). The WHI-552 verification in step 1 has nothing to
   verify against.
3. **Raw private key was used.** WHI-547 steps 2–3 state "Do not pass a raw private
   key through argv or environment" and "raw `--private-key` is forbidden";
   `--interactive` / `--ledger` / `--keystore` are the sanctioned forms. A raw key
   from `.env` was used instead.
4. **Cold admin is a hot key.** `admin` is the deploying EOA, whose private key sits
   in plaintext in `.env`. WHI-547 assumes a cold admin on a hardware wallet or
   keystore. This admin can `unpause`, `transferAdmin`, set hot executors, and
   withdraw.
   **Remediation:** WHI-861 (runbook `docs/runbooks/WHI-861-cold-admin-handoff.md`,
   evidence `evidence/deployments/mantle-mainnet-executor-admin-transfer.md`).
   Fork rehearsal complete; mainnet handoff pending owner cold-key generation.
5. **No WHI-551 runtime verification.** `verify_deployed_runtime(on_chain_code,
   &plan)` was not run against a source-bound `ValidatedImmutablePlan`; identity
   rests on the fork-rehearsal codehash match instead.
6. **No WHI-557 gas-profile identity binding** was checked against this runtime.

## Risk position as it stands

The contract is **paused and holds nothing**, so the live exposure today is
minimal — the deviations above matter at *funding* time, not deploy time. The
material item is deviation 4: whoever holds the `.env` key controls this contract
permanently unless `transferAdmin` is called.

**Before WHI-548 (fund and canary):**

- **WHI-861:** Transfer `admin` to a hardware-wallet or keystore address and
  register a separate hot executor. Scripts + fork rehearsal are ready; owner
  must generate the cold key and broadcast. See
  `evidence/deployments/mantle-mainnet-executor-admin-transfer.md`.
- Provision that principal in `config/signers/allowed_signers`.
- Reconstruct or re-run the WHI-526 decision artifact so there is a verifiable
  authority record, and run the WHI-551 / WHI-557 verifications against this
  deployed runtime.
- Consider setting a pause-only `guardian` (optional step in the WHI-861 sequence).

Funding this contract while its admin key lives in a plaintext `.env` would
reproduce the custody failure class that M0-1 already had to remediate once.

## Not done

No funding, no mainnet hot-executor registration yet (fork-rehearsed under
WHI-861), no venue registration, no traffic. `production_send_allowed()` remains
fail-closed until WHI-860 preconditions arm it. WHI-548 retains its own separate
human approval.
