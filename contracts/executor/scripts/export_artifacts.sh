#!/usr/bin/env bash
# Regenerate WHI-501 executor artifacts (no broadcast).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="${HOME}/.foundry/bin:${PATH}"
cd "$ROOT"
forge build
python3 - <<'PY'
import json, pathlib, subprocess
p = pathlib.Path("out/ArbitrageExecutor.sol/ArbitrageExecutor.json")
data = json.loads(p.read_text())
obj = data["deployedBytecode"]["object"]
if obj.startswith("0x"):
    obj = obj[2:]
kh = subprocess.check_output(["cast", "keccak", "0x" + obj], text=True).strip()
art = pathlib.Path("artifacts")
art.mkdir(exist_ok=True)
(art / "ArbitrageExecutor.deployed.hex").write_text(obj + "\n")
(art / "ArbitrageExecutor.codehash.txt").write_text(kh + "\n")
(art / "ArbitrageExecutor.abi.json").write_text(json.dumps(data["abi"], indent=2) + "\n")
slim = {
    "contractName": "ArbitrageExecutor",
    "codehash": kh,
    "deployedBytecodeLength": len(bytes.fromhex(obj)),
    "abi": data["abi"],
}
(art / "ArbitrageExecutor.artifact.json").write_text(json.dumps(slim, indent=2) + "\n")
import shutil
shutil.copy(p, art / "ArbitrageExecutor.full.json")
print("codehash", kh)
print("runtime bytes", len(bytes.fromhex(obj)))
PY
