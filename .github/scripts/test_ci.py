"""Exercise the CI change classifier with local Git history."""

import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap


def run(argv, cwd, env=None):
    """Capture failures so rejected workflow variants remain available for assertions."""
    return subprocess.run(
        argv, cwd=cwd, env=env, text=True, capture_output=True, timeout=10
    )


workflow_path = (
    Path(sys.argv[1])
    if len(sys.argv) > 1
    else Path(__file__).resolve().parents[1] / "workflows/ci.yml"
)
workflow = workflow_path.read_text()
match = re.search(
    r"- id: diff\n(?:        [^\n]*\n)*?        run: \|\n((?:          [^\n]*\n|\n)+)",
    workflow,
)
assert match, "the changes job has no diff step"
classify = textwrap.dedent(match[1])

with tempfile.TemporaryDirectory(prefix="vault-mirror-ci-changes-") as directory:
    root = Path(directory) / "repo"
    output = Path(directory) / "outputs"
    run(["git", "init", "-b", "main", str(root)], directory).check_returncode()
    run(["git", "config", "user.name", "Local CI test"], root).check_returncode()
    run(
        ["git", "config", "user.email", "test@example.invalid"], root
    ).check_returncode()
    for name in (
        "README.md",
        "src/main.rs",
        "docs/configuration.md",
        "docs/getting-started.md",
        "assets/logo.svg",
    ):
        (root / name).parent.mkdir(parents=True, exist_ok=True)
        (root / name).write_text("initial\n")
    run(["git", "add", "."], root).check_returncode()
    run(["git", "commit", "-m", "test: base"], root).check_returncode()
    base = run(["git", "rev-parse", "HEAD"], root).stdout.strip()

    def classify_change(label, files, base_sha, code, image):
        """Reset each case to the same base and execute the workflow's own classifier."""
        run(["git", "checkout", "-q", "--detach", base], root).check_returncode()
        for name in files:
            (root / name).write_text(f"changed by {label}\n")
        run(
            ["git", "commit", "-q", "--allow-empty", "-am", f"test: {label}"], root
        ).check_returncode()
        output.write_text("")
        env = dict(os.environ, BASE=base_sha, GITHUB_OUTPUT=str(output))
        result = run(["bash", "-e", "-o", "pipefail", "-c", classify], root, env)
        assert result.returncode == 0, (label, result.stderr)
        values = dict(line.split("=", 1) for line in output.read_text().splitlines())
        expected = {"code": str(code).lower(), "image": str(image).lower()}
        assert values == expected, (label, values, expected)
        print(f"{label}: code={values['code']} image={values['image']}")

    classify_change(
        "configuration_guide_only", ["docs/configuration.md"], base, False, True
    )
    classify_change(
        "docs_and_assets_only",
        ["README.md", "docs/getting-started.md", "assets/logo.svg"],
        base,
        False,
        False,
    )
    classify_change("code_only", ["src/main.rs"], base, True, True)
    classify_change(
        "code_and_docs", ["src/main.rs", "docs/getting-started.md"], base, True, True
    )
    classify_change("no_change", [], base, False, False)
    classify_change("no_base", ["README.md"], "", True, True)
    classify_change("unknown_base", ["README.md"], "0" * 40, True, True)

image_job = workflow.split("\n  image:\n", 1)[1].split("\n  # ", 1)[0]
assert "if: needs.changes.outputs.image == 'true'" in image_job, (
    "the image job does not follow the image output"
)
assert "image: ${{ steps.diff.outputs.image }}" in workflow
