# WHI-1572 discovery gas estimator — predeclared calibration protocol

This file and `protocol.json` are committed **before** the fit stage runs, so the
ordered-pool-cycle split membership and every parameter convention are fixed before
any calibration or untouched-test row is evaluated (AC3).

- Command: `python3 scripts/gas_estimate/fit_discovery_estimator.py protocol`
  (stdlib only). The later `fit` stage re-derives this protocol and refuses to run
  if the inputs or membership changed.
- Inputs: the five named clean attempt files (WHI-1422, WHI-1520 initial/run2/run3,
  WHI-1413), each pinned by sha256 in `protocol.json`. WHI-1413 `run2` is not a named
  input and is not read.
- Row filter: `lever == v2_boost`, `outcome == success`, `sample.source ==
  fork_replay`, `chain_id == 5000`, executor code hash `0x50f51b77…26ef`, a consistent
  row shape; dedup key `(block_hash, calldata_digest, lever_param)`. `inflate` /
  `displace` rows are excluded (contaminated levers, PR108-F3 / DI-51).
- Result: 900 clean rows, exclusions `lever != v2_boost` 878, `outcome != success`
  1426, duplicate dedup key 3. 120 ordered pool cycles → 72 train / 24 calibration /
  24 test groups.
- Conventions (seed, group order, proportions, rounding, nearest-rank quantiles,
  multiplier rules, `5/4` limit headroom, 50 000 gas limit overhead, extrapolation
  envelope, leave-block-out): `protocol.json` → `parameters`.

Every margin derived under this protocol is a **new, unvalidated** discovery-ranking
parameter. It does not qualify any route for sending.
