"""Prints the size label for the changes between two commits: size:S, size:M
or size:L.

Only lines that change what vault-mirror does count: documentation, tests
(including `#[cfg(test)]` modules inside a source file), the changelog and
the lockfile don't. Whitespace-only changes and moves don't count either.

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
TEST_MODULE = re.compile(r"^(pub(\(\w+\))? )?mod \w+ ?(\{|;)")
MODULE_END = re.compile(r"^\}\s*(//.*)?$")


def git(*args):
    return subprocess.run(
        ["git", *args], check=True, capture_output=True, text=True
    ).stdout


def test_lines(commit, path):
    """The line numbers of the file's `#[cfg(test)]` modules."""
    if not path.endswith(".rs"):
        return set()
    try:
        lines = git("show", f"{commit}:{path}").splitlines()
    except subprocess.CalledProcessError:
        return set()
    numbers = set()
    index = 0
    while index < len(lines):
        if lines[index].startswith("#[cfg(test)]"):
            start = index
            index += 1
            while index < len(lines) and lines[index].startswith("#["):
                index += 1
            if index < len(lines) and TEST_MODULE.match(lines[index]):
                # rustfmt closes a top-level module with `}` in column one.
                # A raw string could hold such a line too; then the rest of
                # the module counts, which only makes the label too large.
                if lines[index].rstrip().endswith("{"):
                    while index < len(lines) and not MODULE_END.match(lines[index]):
                        index += 1
                numbers.update(range(start + 1, index + 2))
        index += 1
    return numbers


def changed_files(base, head):
    """(old path, new path) pairs; renames and copies keep both."""
    fields = git("diff", "--name-status", "-z", "-M", base, head).split("\0")
    pairs = []
    index = 0
    while index < len(fields) - 1:
        status = fields[index]
        if status[:1] in ("R", "C"):
            pairs.append((fields[index + 1], fields[index + 2]))
            index += 3
        else:
            pairs.append((fields[index + 1], fields[index + 1]))
            index += 2
    return pairs


def counted_lines(base, head):
    total = 0
    for old_path, new_path in changed_files(base, head):
        if IGNORED.search(new_path):
            continue
        old_tests = test_lines(base, old_path)
        new_tests = test_lines(head, new_path)
        diff = git(
            "diff", "-U0", "-M", "--ignore-all-space", "--ignore-blank-lines",
            base, head, "--", *{old_path, new_path},
        )
        in_hunk = False
        old = new = 0
        for line in diff.splitlines():
            if match := HUNK.match(line):
                in_hunk = True
                old, new = int(match[1]), int(match[2])
            elif not in_hunk:
                continue
            elif line.startswith("-"):
                total += bool(line[1:].strip()) and old not in old_tests
                old += 1
            elif line.startswith("+"):
                total += bool(line[1:].strip()) and new not in new_tests
                new += 1
    return total


def label(lines):
    return next((name for bound, name in SIZES if lines < bound), LARGE)


if __name__ == "__main__":
    base, head = sys.argv[1:3]
    lines = counted_lines(base, head)
    print(f"{lines} counted lines", file=sys.stderr)
    print(label(lines))
