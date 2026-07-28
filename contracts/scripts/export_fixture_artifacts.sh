#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="${HOME}/.foundry/bin:${PATH}"
cd "$ROOT"
forge build --skip test

python3 - <<'PY'
import json, pathlib, subprocess

CONTRACTS = [
    ("FixtureERC20", "out/FixtureERC20.sol/FixtureERC20.json"),
    ("E2EFixturePoolV2", "out/E2EFixturePoolV2.sol/E2EFixturePoolV2.json"),
    ("E2EFixturePoolAgniV3", "out/E2EFixturePoolAgniV3.sol/E2EFixturePoolAgniV3.json"),
]

art = pathlib.Path("fixtures/artifacts")
art.mkdir(parents=True, exist_ok=True)

for name, out_rel in CONTRACTS:
    p = pathlib.Path(out_rel)
    data = json.loads(p.read_text())
    deployed = data["deployedBytecode"]["object"]
    if deployed.startswith("0x"):
        deployed = deployed[2:]
    ctor_bytecode = data["bytecode"]["object"]
    if ctor_bytecode.startswith("0x"):
        ctor_bytecode = ctor_bytecode[2:]
    kh = subprocess.check_output(["cast", "keccak", "0x" + deployed], text=True).strip()

    (art / f"{name}.deployed.hex").write_text(deployed + "\n")
    (art / f"{name}.bytecode.hex").write_text(ctor_bytecode + "\n")
    (art / f"{name}.codehash.txt").write_text(kh + "\n")
    (art / f"{name}.abi.json").write_text(json.dumps(data["abi"], indent=2) + "\n")

    slim = {
        "contractName": name,
        "codehash": kh,
        "deployedBytecodeLength": len(bytes.fromhex(deployed)),
        "abi": data["abi"],
    }
    (art / f"{name}.artifact.json").write_text(json.dumps(slim, indent=2) + "\n")

    import shutil
    shutil.copy(p, art / f"{name}.full.json")

    print(name, "codehash", kh, "runtime bytes", len(bytes.fromhex(deployed)))
PY
