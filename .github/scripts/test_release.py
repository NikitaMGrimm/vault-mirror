"""Exercise tagged-release scripts with local Git history and a fake CI API."""

import json
from http.server import BaseHTTPRequestHandler, HTTPServer
import os
from pathlib import Path
import re
import subprocess
import shutil
import sys
import tempfile
import textwrap
import threading


def script(text, marker):
    section = text.split(marker, 1)[1]
    match = re.search(r"        run: \|\n((?:          [^\n]*\n|\n)+)", section)
    assert match, marker
    return textwrap.dedent(match[1])


def run(argv, cwd, env=None):
    return subprocess.run(
        argv, cwd=cwd, env=env, text=True, capture_output=True, timeout=10
    )


workflow_path = (
    Path(sys.argv[1])
    if len(sys.argv) > 1
    else Path(__file__).resolve().parents[1] / "workflows/release.yml"
)
workflow = workflow_path.read_text()
assert "name: Verify CI for the release commit" in workflow, (
    "Tagged publication has no CI guard"
)
guard = script(workflow, "name: Verify CI for the release commit")
version = script(workflow, "- id: version")
assert (
    "if: steps.version.outputs.publish == 'true'"
    in workflow.split("name: Verify CI for the release commit", 1)[1].split(
        "run: |", 1
    )[0]
)
assert workflow.count("ref: ${{ needs.version.outputs.commit }}") == 3
with tempfile.TemporaryDirectory(prefix="vault-mirror-tagged-release-") as directory:
    root = Path(directory)
    run(["git", "init", "-b", "main"], root).check_returncode()
    run(["git", "config", "user.name", "Local release test"], root).check_returncode()
    run(
        ["git", "config", "user.email", "test@example.invalid"], root
    ).check_returncode()
    (root / "Cargo.toml").write_text('[package]\nversion = "1.2.3"\n')
    (root / "CHANGELOG.md").write_text("## [1.2.3]\n\nA release.\n")
    run(["git", "add", "."], root).check_returncode()
    run(["git", "commit", "-m", "test: release commit"], root).check_returncode()
    tested = run(["git", "rev-parse", "HEAD"], root).stdout.strip()
    run(["git", "tag", "-a", "v1.2.3", "-m", "Release"], root).check_returncode()
    (root / "later").write_text("A later unrelated commit.\n")
    run(["git", "add", "."], root).check_returncode()
    run(["git", "commit", "-m", "test: later commit"], root).check_returncode()
    later = run(["git", "rev-parse", "HEAD"], root).stdout.strip()
    assert tested != later
    bindir = root / "bin"
    bindir.mkdir()

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            body = (root / "filtered.json").read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_):
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    gh = bindir / "gh"
    gh.write_text("""#!/usr/bin/env python3
import json, os, subprocess, sys
from pathlib import Path
args = sys.argv[1:]
Path(os.environ["REQUEST_LOG"]).write_text(json.dumps(args))
def arg(name): return args[args.index(name) + 1]
runs = json.loads(Path(os.environ["CI_FIXTURE"]).read_text())
runs = [r for r in runs if r["event"] == arg("--event") and r["headBranch"] == arg("--branch")]
runs = runs[:int(arg("--limit"))]
Path(os.environ["CI_FILTERED"]).write_text(json.dumps(runs))
result = subprocess.run([os.environ["REAL_GH"], "api", os.environ["CI_URL"], "--jq", arg("--jq")], text=True)
sys.exit(result.returncode)
""")
    gh.chmod(0o700)
    sleep = bindir / "sleep"
    sleep.write_text("#!/bin/sh\nexit 42\n")
    sleep.chmod(0o700)
    output = root / "outputs"
    env = dict(
        os.environ,
        PATH=f"{bindir}:{os.environ['PATH']}",
        TAG="v1.2.3",
        GITHUB_SHA=later,
        GITHUB_OUTPUT=str(output),
        RELEASE_SHA=tested,
        REQUEST_LOG=str(root / "request"),
        CI_FIXTURE=str(root / "ci.json"),
        REAL_GH=shutil.which("gh"),
        CI_URL=f"http://127.0.0.1:{server.server_port}",
        CI_FILTERED=str(root / "filtered.json"),
        GH_TOKEN="synthetic-token",
    )
    result = run(["bash", "-e", "-o", "pipefail", "-c", version], root, env)
    assert result.returncode == 0, result.stderr
    values = dict(line.split("=", 1) for line in output.read_text().splitlines())
    assert values["commit"] == tested and values["publish"] == "true", values
    print(
        "annotated_manual_tag: resolves to the tagged commit, independent of GITHUB_SHA"
    )
    for label, sha, event, branch, conclusion, succeeds in [
        ("exact_push_main", tested, "push", "main", "success", True),
        ("wrong_commit", later, "push", "main", "success", False),
        ("failed_ci", tested, "push", "main", "failure", False),
        ("cancelled_ci", tested, "push", "main", "cancelled", False),
        ("manual_ci", tested, "workflow_dispatch", "main", "success", False),
        ("pull_request_ci", tested, "pull_request", "main", "success", False),
        ("another_branch", tested, "push", "feature", "success", False),
    ]:
        (root / "ci.json").write_text(
            json.dumps(
                [
                    dict(
                        headSha=sha,
                        event=event,
                        headBranch=branch,
                        status="completed",
                        conclusion=conclusion,
                    )
                ]
            )
        )
        result = run(["bash", "-e", "-o", "pipefail", "-c", guard], root, env)
        assert (result.returncode == 0) == succeeds, (
            label,
            result.stdout,
            result.stderr,
        )
        args = json.loads((root / "request").read_text())
        assert args[args.index("--commit") + 1] == tested
        assert args[args.index("--workflow") + 1] == "ci.yml"
        print(f"{label}: {'allowed' if succeeds else 'refused'}")
    (root / "notes/notes.md").unlink()
    (root / "notes").rmdir()
    output.write_text("")
    env["TAG"] = ""
    result = run(["bash", "-e", "-o", "pipefail", "-c", version], root, env)
    assert result.returncode == 0, result.stderr
    values = dict(line.split("=", 1) for line in output.read_text().splitlines())
    assert values["publish"] == "false" and values["commit"] == later, values
    print(
        "untagged_smoke_build: remains unpublished and uses the selected workflow commit"
    )
    server.shutdown()
