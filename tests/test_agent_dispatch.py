"""scripts/agent-dispatch.sh: role/effort resolution and fail-closed behaviour.

Fake runtimes stand in for claude/codex/pi, so no model is ever called. These tests
prove argv, stdin and exit-code handling of the dispatcher only -- not that a real
runtime accepts the flags or is authenticated (see docs/agents/runtime.md § Preflight).
"""

import os
import shutil
import subprocess
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "agent-dispatch.sh"
BASH = shutil.which("bash") or "/bin/bash"

# A fake runtime: records argv (one per line), stdin and the content of any @file
# argument (pi's prompt), then exits with $FAKE_EXIT.
FAKE = """#!/bin/sh
printf '%s\\n' "$(basename "$0")" "$@" > "$FAKE_LOG/argv"
cat > "$FAKE_LOG/stdin"
for a in "$@"; do case "$a" in @*) cat "${a#@}" > "$FAKE_LOG/atfile" ;; esac; done
echo call >> "$FAKE_LOG/calls"
echo "fake-output"
exit "${FAKE_EXIT:-0}"
"""

BASE_CONF = {
    "IMPLEMENTER_RUNTIME": "claude",
    "IMPLEMENTER_MODEL": "impl-model",
    "IMPLEMENTER_EFFORT_MEDIUM": "medium",
    "IMPLEMENTER_EFFORT_HIGH": "max",
    "REVIEWER_RUNTIME": "codex",
    "REVIEWER_MODEL": "rev-model",
    "REVIEWER_EFFORT_HIGH": "xhigh",
    "ORCHESTRATOR_RUNTIME": "pi",
    "ORCHESTRATOR_MODEL": "prov/orch-model",
    "ORCHESTRATOR_EFFORT_HIGH": "high",
}


@pytest.fixture
def env(tmp_path: Path) -> dict[str, str]:
    """Isolated PATH: fake runtimes plus only the tools the script needs."""
    fakebin = tmp_path / "bin"
    fakebin.mkdir()
    for tool in ("dirname", "cat", "mktemp", "rm", "basename", "sh"):
        found = shutil.which(tool)
        assert found, tool
        (fakebin / tool).symlink_to(found)
    for runtime in ("claude", "codex", "pi"):
        path = fakebin / runtime
        path.write_text(FAKE)
        path.chmod(0o755)
    log = tmp_path / "log"
    log.mkdir()
    return {"PATH": str(fakebin), "FAKE_LOG": str(log), "TMPDIR": str(tmp_path)}


def write_conf(tmp_path: Path, env: dict[str, str], **overrides: str | None) -> None:
    conf = {**BASE_CONF, **overrides}
    body = "".join(f'{k}="{v}"\n' for k, v in conf.items() if v is not None)
    path = tmp_path / "roles.conf"
    path.write_text(body)
    env["AGENT_ROLES_CONF"] = str(path)


def run(env: dict[str, str], *args: str, stdin: str = "") -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [BASH, str(SCRIPT), *args], env=env, input=stdin, capture_output=True, text=True
    )


def calls(env: dict[str, str]) -> int:
    path = Path(env["FAKE_LOG"]) / "calls"
    return len(path.read_text().splitlines()) if path.exists() else 0


def argv(env: dict[str, str]) -> list[str]:
    return (Path(env["FAKE_LOG"]) / "argv").read_text().splitlines()


@pytest.fixture
def prompt(tmp_path: Path) -> Path:
    path = tmp_path / "prompt with space.txt"
    path.write_text("Reply with exactly: DISPATCH-OK\n")
    return path


def test_claude_maps_semantic_effort_and_pipes_prompt_file(tmp_path, env, prompt):
    write_conf(tmp_path, env)
    for effort, native in (("medium", "medium"), ("high", "max")):
        result = run(env, "IMPLEMENTER", str(prompt), "--effort", effort)
        assert result.returncode == 0, result.stderr
        assert result.stdout == "fake-output\n"
        assert argv(env) == ["claude", "-p", "--model", "impl-model", "--effort", native]
        assert (Path(env["FAKE_LOG"]) / "stdin").read_text() == prompt.read_text()


def test_codex_reads_stdin_prompt(tmp_path, env):
    write_conf(tmp_path, env)
    result = run(env, "REVIEWER", "-", "--effort", "high", stdin="from stdin\n")
    assert result.returncode == 0, result.stderr
    assert argv(env) == [
        "codex",
        "exec",
        "--model",
        "rev-model",
        "-c",
        'model_reasoning_effort="xhigh"',
        "-",
    ]
    assert (Path(env["FAKE_LOG"]) / "stdin").read_text() == "from stdin\n"


def test_pi_gets_prompt_as_file_argument_including_spooled_stdin(tmp_path, env, prompt):
    write_conf(tmp_path, env)
    result = run(env, "ORCHESTRATOR", str(prompt), "--effort", "high")
    assert result.returncode == 0, result.stderr
    assert argv(env) == [
        "pi",
        "-p",
        "--model",
        "prov/orch-model",
        "--thinking",
        "high",
        f"@{prompt}",
    ]
    assert (Path(env["FAKE_LOG"]) / "atfile").read_text() == prompt.read_text()
    result = run(env, "ORCHESTRATOR", "-", "--effort", "high", stdin="spooled prompt\n")
    assert result.returncode == 0, result.stderr
    spool = argv(env)[-1]
    # The child read the piped prompt from the spool file, which is removed afterwards.
    assert (Path(env["FAKE_LOG"]) / "atfile").read_text() == "spooled prompt\n"
    assert spool.startswith("@") and not Path(spool[1:]).exists()


@pytest.mark.parametrize(
    "args",
    [
        ("ESCALATOR", "-", "--effort", "high"),  # removed role
        ("IMPLEMENTER", "-", "--effort", "low"),  # unknown effort
        ("IMPLEMENTER", "-"),  # effort is mandatory
        ("IMPLEMENTER", "-", "--effort", "high", "--model", "other"),  # no pass-through
        ("IMPLEMENTER", "/no/such/prompt", "--effort", "high"),
    ],
)
def test_usage_errors_exit_2_without_calling(tmp_path, env, args):
    write_conf(tmp_path, env)
    assert run(env, *args).returncode == 2
    assert calls(env) == 0


@pytest.mark.parametrize(
    ("role", "overrides", "effort", "reason"),
    [
        ("IMPLEMENTER", {"IMPLEMENTER_MODEL": None}, "high", "IMPLEMENTER_MODEL is unset"),
        ("IMPLEMENTER", {"IMPLEMENTER_RUNTIME": "gemini"}, "high", "unknown runtime"),
        # REVIEWER maps only high: medium must fail, not silently run at high or default.
        ("REVIEWER", {}, "medium", "REVIEWER_EFFORT_MEDIUM is unset"),
        ("REVIEWER", {"REVIEWER_MODEL": "-m"}, "high", "single plain token"),
        ("REVIEWER", {"REVIEWER_MODEL": "rev-model\t-m other"}, "high", "single plain token"),
        ("REVIEWER", {"REVIEWER_EFFORT_HIGH": "x' -c model='o"}, "high", "single plain token"),
        ("ORCHESTRATOR", {"ORCHESTRATOR_MODEL": "prov/m:low"}, "high", "embeds a thinking"),
    ],
)
def test_unavailable_role_exits_3_without_calling(tmp_path, env, role, overrides, effort, reason):
    write_conf(tmp_path, env, **overrides)
    result = run(env, role, "-", "--effort", effort)
    assert result.returncode == 3, result.stderr
    assert reason in result.stderr
    assert calls(env) == 0


@pytest.mark.parametrize("key", ["EXTRA_ARGS", "CMD"])
@pytest.mark.parametrize(
    "value",
    [
        "-m\tother-model",  # tab-separated model override
        "--model other-model",
        "\n--model=other-model",
        "  --resume  last",  # session reuse breaks the fresh context
        "--continue",
        "claude -p --model opus",  # a v0.1-style _CMD line
    ],
)
def test_free_form_flags_cannot_override_model_or_session(tmp_path, env, key, value):
    write_conf(
        tmp_path,
        env,
        REVIEWER_RUNTIME="pi",
        REVIEWER_MODEL="prov/review-model",
        REVIEWER_EFFORT_HIGH="high",
        **{f"REVIEWER_{key}": value},
    )
    result = run(env, "REVIEWER", "-", "--effort", "high", stdin="x")
    assert result.returncode == 3, (result.stdout, result.stderr)
    assert f"REVIEWER_{key} is not supported" in result.stderr
    assert calls(env) == 0


def test_missing_binary_exits_3(tmp_path, env):
    write_conf(tmp_path, env)
    os.remove(Path(env["PATH"]) / "claude")
    result = run(env, "IMPLEMENTER", "-", "--effort", "high")
    assert result.returncode == 3
    assert "not on PATH" in result.stderr


def test_runtime_failure_propagates_once_with_no_fallback(tmp_path, env):
    write_conf(tmp_path, env)
    env["FAKE_EXIT"] = "7"  # e.g. authentication failure
    result = run(env, "IMPLEMENTER", "-", "--effort", "high", stdin="x")
    assert result.returncode == 7
    assert calls(env) == 1
    env["FAKE_EXIT"] = "9"
    assert run(env, "ORCHESTRATOR", "-", "--effort", "high", stdin="x").returncode == 9


def test_probe_is_static_and_fails_closed(tmp_path, env):
    write_conf(tmp_path, env)
    result = run(env, "--probe")
    assert result.returncode == 0, result.stdout
    assert "ok        REVIEWER -> codex rev-model (effort: high)" in result.stdout
    assert "not an authentication or real-call result" in result.stdout
    assert calls(env) == 0

    write_conf(tmp_path, env, REVIEWER_MODEL=None)
    result = run(env, "--probe", "REVIEWER")
    assert result.returncode == 3
    assert "UNUSABLE  REVIEWER" in result.stdout
    assert run(env, "--probe", "EXPLORER").returncode == 2


def test_shipped_conf_parses_and_probe_never_calls_a_model(env):
    result = run(env, "--probe")
    assert result.returncode in (0, 3), result.stderr
    assert calls(env) == 0
