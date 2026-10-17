"""Exercise the README demo recorder with real terminals and fake sandbox binaries.

Set VAULT_MIRROR_TEST_VHS=1 to also render through the installed VHS.
"""

import itertools
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import demo

PREVIOUS = b"previous published GIF"
FOREGROUND = (0xD0, 0xD4, 0xDA)

# Starts a child that ignores SIGTERM, then exits or stays until SIGTERM.
SPAWN = """
import signal, subprocess, sys, time
child = subprocess.Popen([sys.executable, "-c",
    "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"])
open(sys.argv[1], "w").write(str(child.pid))
if sys.argv[2] == "stay":
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    time.sleep(60)
"""


def capture(code, *args, timeout=10):
    with tempfile.TemporaryDirectory() as work:
        return demo.capture(
            [sys.executable, "-c", code, *args], dict(os.environ), work, timeout
        )


def gone(pid, wait=5):
    deadline = time.monotonic() + wait
    while time.monotonic() < deadline:
        try:
            if Path(f"/proc/{pid}/stat").read_text().split(")")[-1].split()[0] == "Z":
                return True
        except FileNotFoundError:
            return True
        time.sleep(0.05)
    return False


class CaptureTest(unittest.TestCase):
    def test_stdout_and_stderr_share_one_real_terminal(self):
        output = capture(
            "import os, sys\n"
            "size = os.get_terminal_size(1)\n"
            "print('out', os.isatty(1), size.columns, flush=True)\n"
            "print('err', os.isatty(2), file=sys.stderr, flush=True)\n"
        )
        self.assertEqual(output, f"out True {demo.COLUMNS}\r\nerr True\r\n".encode())

    def test_terminal_bytes_are_kept_exactly(self):
        payload = (
            "\x1b[1;32m✓\x1b[0m Quellvault geprüft — 🔐\n"
            "\r  10%\r\x1b[2K  55%\r\x1b[2K 100%\n"
            + "".join(f"\x1b[38;5;{i % 200}m{i % 10}" for i in range(260))
            + "\x1b[0m\n"
        ).encode()
        output = capture(
            "import os, sys, time\n"
            f"data = {payload!r}\n"
            # Split a multibyte character across writes to catch per-chunk decoding.
            "cut = data.index('✓'.encode()) + 1\n"
            "os.write(1, data[:cut]); time.sleep(0.05); os.write(2, data[cut:])\n"
        )
        self.assertEqual(output, payload.replace(b"\n", b"\r\n"))

    def test_slow_quiet_command_finishes_within_its_timeout(self):
        output = capture("import time\ntime.sleep(1.5)\nprint('done')", timeout=5)
        self.assertEqual(output, b"done\r\n")

    def test_failed_command_is_an_error_without_its_output(self):
        with self.assertRaisesRegex(RuntimeError, "exit status 3") as caught:
            capture("import sys\nprint('hunter2-secret')\nsys.exit(3)")
        self.assertNotIn("hunter2", str(caught.exception))

    def test_timeout_stops_every_process_the_command_started(self):
        with tempfile.TemporaryDirectory() as work:
            pid = Path(work) / "pid"
            started = time.monotonic()
            with self.assertRaisesRegex(RuntimeError, "timed out"):
                capture(SPAWN, str(pid), "stay", timeout=1)
            self.assertLess(time.monotonic() - started, 15)
            self.assertTrue(gone(int(pid.read_text())))

    def test_child_left_behind_is_stopped(self):
        with tempfile.TemporaryDirectory() as work:
            pid = Path(work) / "pid"
            capture(SPAWN, str(pid), "exit")
            self.assertTrue(gone(int(pid.read_text())))


class RunTest(unittest.TestCase):
    def test_timeout_stops_children_after_a_graceful_shutdown(self):
        with tempfile.TemporaryDirectory() as work:
            pid = Path(work) / "pid"
            with self.assertRaisesRegex(RuntimeError, "VHS timed out"):
                demo.run("VHS", [sys.executable, "-c", SPAWN, str(pid), "stay"], 1)
            self.assertTrue(gone(int(pid.read_text())))


class PagesTest(unittest.TestCase):
    def test_pages_show_every_row_with_overlap_and_end_at_the_last_row(self):
        for visible in range(demo.MIN_ROWS, demo.MAX_ROWS + 1):
            for total in range(1, 200):
                offsets = demo.pages(total, visible)
                with self.subTest(visible=visible, total=total):
                    self.assertEqual(offsets[0], 0)
                    shown = set()
                    for offset in offsets:
                        shown.update(range(offset, offset + visible))
                    self.assertLessEqual(set(range(total)), shown)
                    if total <= visible:
                        self.assertEqual(offsets, [0])
                        continue
                    self.assertEqual(offsets[-1] + visible, total)
                    for before, after in itertools.pairwise(offsets):
                        self.assertGreater(after, before)
                        self.assertGreaterEqual(before + visible - after, 2)


class MainTest(unittest.TestCase):
    def setUp(self):
        work = tempfile.TemporaryDirectory()
        self.addCleanup(work.cleanup)
        self.root = Path(work.name)
        self.bin = self.root / "bin"
        (self.bin / "examples").mkdir(parents=True)
        self.output = self.root / "demo.gif"
        self.output.write_bytes(PREVIOUS)
        self.calls = self.root / "calls"

    def fake(self, path, body):
        path.write_text(f"#!{sys.executable}\nimport json, os, sys\n{body}\n")
        path.chmod(0o755)

    def sandbox(self, source="http://127.0.0.1:18091", status=0):
        self.fake(
            self.bin / "examples/sandbox",
            f"print('SOURCE_SERVER={source}\\nDEST_SERVER=http://127.0.0.1:18092\\n"
            "SOURCE_PASSWORD=pw-7Qx\\nDEST_CLIENT_SECRET=cs-9Lm')\n"
            f"sys.exit({status})",
        )

    def vault_mirror(self, failing=None, echo=""):
        self.fake(
            self.bin / "vault-mirror",
            f"open({str(self.calls)!r}, 'a').write(json.dumps([sys.argv[1], sorted(os.environ)]) + '\\n')\n"
            "print('\\x1b[32mok\\x1b[0m', sys.argv[1])\n"
            f"print({echo!r} if sys.argv[1] == 'run' else '', end='')\n"
            f"sys.exit(4 if sys.argv[1] == {failing!r} else 0)",
        )

    def main(self, render=None):
        render = render or mock.Mock(side_effect=AssertionError("render must not run"))
        with mock.patch.object(demo, "render", render):
            demo.main(bin_dir=self.bin, output=self.output)
        return render

    def calls_made(self):
        if not self.calls.exists():
            return []
        return [json.loads(line)[0] for line in self.calls.read_text().splitlines()]

    def assertPreserved(self):
        self.assertEqual(self.output.read_bytes(), PREVIOUS)

    def test_failed_setup_runs_nothing(self):
        self.sandbox(status=1)
        self.vault_mirror()
        with self.assertRaisesRegex(RuntimeError, "setup failed"):
            self.main()
        self.assertEqual(self.calls_made(), [])
        self.assertPreserved()

    def test_refuses_servers_outside_the_sandbox(self):
        self.sandbox(source="https://vault.example.com")
        self.vault_mirror()
        with self.assertRaisesRegex(RuntimeError, "sandbox"):
            self.main()
        self.assertEqual(self.calls_made(), [])
        self.assertPreserved()

    def test_failed_check_prevents_the_run(self):
        self.sandbox()
        self.vault_mirror(failing="check")
        with self.assertRaisesRegex(RuntimeError, "exit status 4"):
            self.main()
        self.assertEqual(self.calls_made(), ["check"])
        self.assertPreserved()

    def test_failed_run_keeps_the_previous_gif(self):
        self.sandbox()
        self.vault_mirror(failing="run")
        with self.assertRaisesRegex(RuntimeError, "exit status 4"):
            self.main()
        self.assertEqual(self.calls_made(), ["check", "run"])
        self.assertPreserved()

    def test_output_that_shows_a_sandbox_credential_is_not_published(self):
        for secret in ("pw-7Qx", "cs-9Lm"):
            with self.subTest(secret=secret):
                self.sandbox()
                self.vault_mirror(echo=f"token {secret}\n")
                with self.assertRaisesRegex(RuntimeError, "credentials") as caught:
                    self.main()
                self.assertNotIn(secret, str(caught.exception))
                self.assertPreserved()

    def test_renders_both_outputs_from_a_terminal_without_the_callers_environment(self):
        self.sandbox()
        self.vault_mirror()
        with mock.patch.dict(os.environ, VAULT_MIRROR_DEMO_LEAK="1"):
            render = self.main(mock.Mock(return_value=demo.MIN_ROWS))
        outputs, output, _ = render.call_args.args
        self.assertEqual(list(outputs), ["check", "run"])
        self.assertEqual(outputs["run"], b"\x1b[32mok\x1b[0m run\r\n")
        self.assertEqual(output, self.output)
        for line in self.calls.read_text().splitlines():
            self.assertNotIn("VAULT_MIRROR_DEMO_LEAK", json.loads(line)[1])


def swatch(rgb, width):
    return "\x1b[48;2;{};{};{}m".format(*rgb) + " " * width + "\x1b[0m"


def palette(count):
    """Colors far from each other and from the theme, so GIF quantization cannot confuse them."""
    levels = range(40, 241, 40)
    theme = [(0x14, 0x17, 0x1C), FOREGROUND, (0x6B, 0x73, 0x80)]
    colors = [
        (r, g, b)
        for r in levels
        for g in levels
        for b in levels
        if not any(near((r, g, b), t, 30) for t in theme)
    ]
    return colors[:count]


def lines(*rows):
    return "".join(row + "\r\n" for row in rows).encode()


def near(pixel, color, tolerance):
    return max(abs(x - y) for x, y in zip(pixel, color)) <= tolerance


def size(gif):
    probe = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-show_entries",
            "stream=width,height",
            "-of",
            "json",
            str(gif),
        ],
        capture_output=True,
        check=True,
        timeout=60,
    )
    stream = json.loads(probe.stdout)["streams"][0]
    return stream["width"], stream["height"]


def held_frames(gif, colors):
    """What each frame held for at least a second shows in the left quarter of the terminal.

    Returns, per held frame, the swatch colors from top to bottom, whether
    command text is drawn above the first swatch, and where each swatch starts.
    """
    width, height = size(gif)
    strip = width // 4
    # Two frames a second are enough to tell held pages from scrolling and typing.
    decoded = subprocess.run(
        [
            "ffmpeg",
            "-v",
            "error",
            "-i",
            str(gif),
            "-vf",
            f"fps=2,crop={strip}:ih:0:0",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-",
        ],
        capture_output=True,
        check=True,
        timeout=60,
    ).stdout
    frame_size = strip * height * 3
    samples = []
    for start in range(0, len(decoded), frame_size):
        frame = decoded[start : start + frame_size]
        pixels = [frame[i : i + 3] for i in range(0, len(frame), 3)]
        column = [pixels[y * strip + strip // 2] for y in range(height)]
        swatches, y = [], 0
        for color, run in itertools.groupby(
            next((c for c in colors if near(p, c, 20)), None) for p in column
        ):
            length = len(list(run))
            if color and length >= 8:
                swatches.append((color, y))
            y += length
        above = swatches[0][1] if swatches else height
        text = sum(near(p, FOREGROUND, 40) for p in pixels[: above * strip])
        samples.append(
            (tuple(c for c, _ in swatches), text > 20, [y for _, y in swatches])
        )
    held = []
    for shown, run in itertools.groupby(samples, key=lambda s: s[0]):
        run = list(run)
        if shown and len(run) >= 3:
            held.append((shown, any(s[1] for s in run), run[0][2]))
    return held, height


@unittest.skipUnless(
    os.environ.get("VAULT_MIRROR_TEST_VHS") == "1",
    "set VAULT_MIRROR_TEST_VHS=1 to render with VHS",
)
class VhsTest(unittest.TestCase):
    def setUp(self):
        work = tempfile.TemporaryDirectory()
        self.addCleanup(work.cleanup)
        self.work = Path(work.name)
        self.published = self.work / "assets/demo.gif"
        self.published.parent.mkdir()
        self.published.write_bytes(PREVIOUS)

    def render(self, outputs, name):
        directory = self.work / name
        directory.mkdir()
        rows = demo.render(outputs, self.published, directory)
        self.assertNotEqual(self.published.read_bytes(), PREVIOUS)
        gif = self.work / f"{name}.gif"
        self.published.rename(gif)
        self.published.write_bytes(PREVIOUS)
        return rows, gif

    def test_terminal_reports_the_rows_it_draws(self):
        outputs = {
            "short": lines("one", "two"),
            "exact": lines("x" * demo.COLUMNS),
            "wrapped": lines("y" * (2 * demo.COLUMNS + 10)),
        }
        for name, data in outputs.items():
            (self.work / f"{name}.out").write_bytes(data)
        self.assertEqual(demo.measure(self.work, list(outputs)), [4, 3, 5])

    def test_short_output_fits_and_long_output_pages_through_every_row(self):
        colors = palette(98)
        rows, exact, chunks = colors[:80], colors[80], colors[81:92]
        check, short = colors[92:95], colors[95:]
        # One line exactly as wide as the terminal, then one that wraps onto three
        # rows, where the left quarter shows chunks 0, 5 and 10.
        expected = [*rows, exact, chunks[0], chunks[5], chunks[10]]
        run = lines(
            *(swatch(rgb, 40) for rgb in rows),
            swatch(exact, demo.COLUMNS),
            "".join(swatch(rgb, 20) for rgb in chunks),
        )
        check_output = lines(*(swatch(rgb, 40) for rgb in check))

        tall_rows, tall = self.render({"check": check_output, "run": run}, "tall")
        self.assertEqual(tall_rows, demo.MAX_ROWS)
        held, tall_height = held_frames(tall, colors)
        self.assertEqual(held[0][:2], (tuple(check), True))
        shown = [frame for frame, _, _ in held[1:]]
        self.assertGreaterEqual(len(shown), 4)
        self.assertEqual(shown[0][0], rows[0])
        self.assertTrue(held[1][1], "the first page shows the command")
        self.assertEqual(
            shown[-1][-1], chunks[10], "the last page ends on the wrapped tail"
        )
        for page in shown:
            start = expected.index(page[0])
            self.assertEqual(list(page), expected[start : start + len(page)])
            self.assertGreaterEqual(len(page), demo.MAX_ROWS - 2)
        self.assertEqual(set().union(*shown), set(expected))
        for before, after in itertools.pairwise(shown):
            self.assertGreaterEqual(len(set(before) & set(after)), 2)
        starts = held[2][2]
        row_height = (starts[-1] - starts[0]) / (len(starts) - 1)

        short_rows, gif = self.render(
            {
                "check": lines(swatch(short[0], 40)),
                "run": lines(*(swatch(c, 40) for c in short[1:])),
            },
            "short",
        )
        self.assertEqual(short_rows, demo.MIN_ROWS)
        held, short_height = held_frames(gif, colors)
        self.assertEqual(
            [frame[:2] for frame in held],
            [((short[0],), True), (tuple(short[1:]), True)],
        )
        self.assertAlmostEqual(
            (tall_height - short_height) / (demo.MAX_ROWS - demo.MIN_ROWS),
            row_height,
            delta=0.5,
        )

    def test_output_beyond_the_recording_limit_keeps_the_previous_gif(self):
        with (
            mock.patch.object(demo, "MEASURE_ROWS", 32),
            self.assertRaisesRegex(RuntimeError, "exceeds"),
        ):
            demo.render(
                {"check": lines("ok"), "run": lines(*map(str, range(40)))},
                self.published,
                self.work,
            )
        self.assertEqual(self.published.read_bytes(), PREVIOUS)

    def test_unfinished_playback_keeps_the_previous_gif(self):
        # Without a final newline the prompt never starts its own line, so VHS times out waiting for it.
        with (
            mock.patch.object(demo, "measure", return_value=[3, 3]),
            self.assertRaisesRegex(RuntimeError, "VHS failed"),
        ):
            demo.render(
                {"check": lines("ok"), "run": b"no newline"}, self.published, self.work
            )
        self.assertEqual(self.published.read_bytes(), PREVIOUS)
        self.assertEqual(
            sorted(p.name for p in self.published.parent.iterdir()), ["demo.gif"]
        )


if __name__ == "__main__":
    unittest.main()
