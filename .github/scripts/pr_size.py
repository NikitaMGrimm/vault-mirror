"""Prints the size label for the changes between two commits: size:S, size:M
or size:L.

Only lines that change what vault-mirror does count: documentation, tests
(including `#[cfg(test)]` modules at the end of a source file), the
changelog and the lockfile don't. Whitespace-only changes don't count either.

Usage: pr_size.py BASE HEAD
"""

import re
import subprocess
import sys

# Upper bounds: fewer than 50 lines is small, fewer than 300 medium.
SIZES = [(50, "size:S"), (300, "size:M")]
LARGE = "size:L"

IGNORED = re.compile(
    r"(\.md$|^docs/|^assets/|^tests/|/tests\.rs$|^Cargo\.lock$|^LICENSE$)"
)
HUNK = re.compile(r"^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@")


def git(*args):
    return subprocess.run(
        ["git", *args], check=True, capture_output=True, text=True
    ).stdout


def test_module_start(commit, path):
    """The first line of the file's `#[cfg(test)]` module, or None."""
    try:
        text = git("show", f"{commit}:{path}")
    except subprocess.CalledProcessError:
        return None
    for number, line in enumerate(text.splitlines(), start=1):
        if line.startswith("#[cfg(test)]"):
            return number
    return None


def counted_lines(base, head):
    total = 0
    paths = git("diff", "--name-only", "-z", "--no-renames", base, head)
    for path in filter(None, paths.split("\0")):
        if IGNORED.search(path):
            continue
        old_tests = new_tests = None
        if path.endswith(".rs"):
            old_tests = test_module_start(base, path)
            new_tests = test_module_start(head, path)
        diff = git(
            "diff", "-U0", "--no-renames", "--ignore-all-space",
            "--ignore-blank-lines", base, head, "--", path,
        )
        old = new = 0
        for line in diff.splitlines():
            if match := HUNK.match(line):
                old, new = int(match[1]), int(match[2])
            elif line.startswith("-") and not line.startswith("---"):
                total += old_tests is None or old < old_tests
                old += 1
            elif line.startswith("+") and not line.startswith("+++"):
                total += new_tests is None or new < new_tests
                new += 1
    return total


def label(lines):
    return next((name for bound, name in SIZES if lines < bound), LARGE)


if __name__ == "__main__":
    base, head = sys.argv[1:3]
    lines = counted_lines(base, head)
    print(f"{lines} counted lines", file=sys.stderr)
    print(label(lines))
