"""Exercise the README demo recorder with real terminals and fake renderers.

Set VAULT_MIRROR_TEST_VHS=1 to also render through the installed VHS.
"""

import base64
import collections
import contextlib
import io
import itertools
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import demo

# Two 1x1 frames: the smallest GIF the recorder accepts as rendered.
FRAME = b"!\xf9\x04\x00\x0a\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00"
GIF = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff" + FRAME * 2 + b";"
PREVIOUS = b"previous published GIF"


def fixture(code, timeout=10, **kwargs):
    with tempfile.TemporaryDirectory() as work:
        return demo.capture(
            [sys.executable, "-c", code], dict(os.environ), work, timeout, **kwargs
        )


def output(events):
    return b"".join(base64.b64decode(data) for _, data in events)


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


SPAWN = """
import subprocess, sys
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
open(sys.argv[1], "w").write(str(child.pid))
"""


class CaptureTest(unittest.TestCase):
    def test_stdout_and_stderr_share_one_real_terminal(self):
        events = fixture(
            "import os, sys\n"
            "size = os.get_terminal_size(1)\n"
            "print('out', os.isatty(1), size.columns, flush=True)\n"
            "print('err', os.isatty(2), file=sys.stderr, flush=True)\n"
        )
        self.assertEqual(
            output(events), f"out True {demo.COLUMNS}\r\nerr True\r\n".encode()
        )

    def test_terminal_bytes_are_kept_exactly(self):
        payload = (
            "\x1b[1;32m✓\x1b[0m Quellvault geprüft — 🔐\n"
            "\r  10%\r\x1b[2K  55%\r\x1b[2K 100%\n"
            + "".join(f"\x1b[38;5;{i % 200}m{i % 10}" for i in range(260))
            + "\x1b[0m\n"
        ).encode()
        events = fixture(
            "import os, sys, time\n"
            f"data = {payload!r}\n"
            # Split a multibyte character across writes to catch per-chunk decoding.
            "cut = data.index('✓'.encode()) + 1\n"
            "os.write(1, data[:cut]); time.sleep(0.05); os.write(2, data[cut:])\n"
        )
        self.assertEqual(output(events), payload.replace(b"\n", b"\r\n"))

    def test_records_real_gaps_between_output(self):
        events = fixture(
            "import time\nprint('a', flush=True)\ntime.sleep(0.6)\nprint('b', flush=True)"
        )
        gap = next(delay for delay, data in events if b"b" in base64.b64decode(data))
        self.assertGreaterEqual(gap, 0.5)

    def test_slow_quiet_command_finishes_within_its_own_timeout(self):
        events = fixture("import time\ntime.sleep(1.5)\nprint('done')", timeout=5)
        self.assertEqual(output(events), b"done\r\n")
        self.assertGreaterEqual(sum(delay for delay, _ in events), 1.4)

    def test_failed_command_is_an_error_without_its_output(self):
        with self.assertRaises(RuntimeError) as caught:
            fixture("import sys\nprint('hunter2-secret')\nsys.exit(3)")
        self.assertIn("3", str(caught.exception))
        self.assertNotIn("hunter2", str(caught.exception))

    def test_timeout_kills_the_whole_process_group(self):
        with tempfile.TemporaryDirectory() as work:
            pid_file = Path(work) / "pid"
            started = time.monotonic()
            with self.assertRaisesRegex(RuntimeError, "timed out"):
                demo.capture(
                    [
                        sys.executable,
                        "-c",
                        SPAWN + "import time; time.sleep(60)",
                        str(pid_file),
                    ],
                    dict(os.environ),
                    work,
                    timeout=1,
                )
            self.assertLess(time.monotonic() - started, 10)
            self.assertTrue(gone(int(pid_file.read_text())))

    def test_child_that_keeps_the_terminal_open_is_killed(self):
        with tempfile.TemporaryDirectory() as work:
            pid_file = Path(work) / "pid"
            with self.assertRaisesRegex(RuntimeError, "timed out"):
                demo.capture(
                    [sys.executable, "-c", SPAWN, str(pid_file)],
                    dict(os.environ),
                    work,
                    timeout=1,
                )
            self.assertTrue(gone(int(pid_file.read_text())))


class ReplayTest(unittest.TestCase):
    def replay(self, recording, *flags):
        with tempfile.TemporaryDirectory() as work:
            path = Path(work) / "run.json"
            path.write_text(json.dumps(recording))
            started = time.monotonic()
            result = subprocess.run(
                [sys.executable, demo.__file__, "replay", str(path), *flags],
                capture_output=True,
                timeout=30,
                check=True,
            )
            return (
                result.stdout,
                time.monotonic() - started,
                path.with_suffix(".done").exists(),
            )

    def test_long_server_waits_are_shortened_without_losing_output(self):
        chunks = [
            b"\x1b[1mbackup\x1b[0m\r\n",
            b"\rcomparing\r\x1b[2K",
            "fertig ✓\r\n".encode(),
        ]
        recording = {
            "marker": "M",
            "events": [[90.0, base64.b64encode(c).decode()] for c in chunks],
        }
        stdout, elapsed, done = self.replay(recording)
        self.assertEqual(stdout, b"".join(chunks))
        self.assertLess(elapsed, 10)
        self.assertTrue(done)

    def test_measurement_ends_with_the_marker_and_does_not_complete_playback(self):
        recording = {
            "marker": "VM_DEMO_END_x",
            "events": [[5.0, base64.b64encode(b"a\r\n").decode()]],
        }
        stdout, _, done = self.replay(recording, "--measure")
        self.assertTrue(stdout.startswith(b"a\r\n"))
        self.assertEqual(stdout.split()[-1], b"VM_DEMO_END_x")
        self.assertFalse(done)


class VhsTimeoutTest(unittest.TestCase):
    def test_timeout_stops_renderer_children_after_graceful_shutdown(self):
        with tempfile.TemporaryDirectory() as work:
            directory = Path(work)
            executable = directory / "vhs"
            pid_file = directory / "child.pid"
            executable.write_text(
                f"#!{sys.executable}\n"
                "import signal, subprocess, sys, time\n"
                "signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))\n"
                "child = subprocess.Popen([sys.executable, '-c', "
                "'import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)'])\n"
                f"open({str(pid_file)!r}, 'w').write(str(child.pid))\n"
                "time.sleep(60)\n"
            )
            executable.chmod(0o755)
            with (
                mock.patch.dict(os.environ, PATH=f"{work}:{os.environ['PATH']}"),
                self.assertRaisesRegex(RuntimeError, "timed out"),
            ):
                demo.run_vhs(directory / "demo.tape", timeout=1)
            self.assertTrue(gone(int(pid_file.read_text())))


def frame(lines, rows):
    """One VHS .ascii frame: the newest `rows` lines, padded to the terminal size."""
    visible = lines[-rows:]
    return (
        "\n".join(
            line.ljust(demo.COLUMNS) for line in visible + [""] * (rows - len(visible))
        )
        + "\n"
    )


def ascii_text(frames):
    return "".join(f + demo.SEPARATOR + "\n" for f in frames)


MARKERS = {"check": {"marker": "M1"}, "run": {"marker": "M2"}}


class LayoutTest(unittest.TestCase):
    def test_measures_each_command_from_its_last_frame(self):
        check = ["$ vault-mirror check", "one", "two", "", "M1", "$"]
        run = ["$ vault-mirror run", *[f"line {i}" for i in range(30)], "", "M2", "$"]
        text = ascii_text(
            [frame(check[:2], 40), frame(check, 40), frame(["$"], 40), frame(run, 40)]
        )
        self.assertEqual(demo.measured_rows(text, MARKERS), [5, 33])

    def test_output_taller_than_the_measurement_terminal_is_not_measured(self):
        check = ["$ vault-mirror check", "", "M1", "$"]
        run = ["$ vault-mirror run", *[f"line {i}" for i in range(40)], "", "M2", "$"]
        self.assertIsNone(
            demo.measured_rows(ascii_text([frame(check, 32), frame(run, 32)]), MARKERS)
        )
        self.assertIsNone(demo.measured_rows(ascii_text([frame(check, 32)]), MARKERS))

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


def recording(marker, lines):
    data = "".join(line + "\r\n" for line in lines).encode()
    return {"marker": marker, "events": [[0.01, base64.b64encode(data).decode()]]}


class FakeVhs:
    """Measures with a terminal buffer that only keeps `Set Rows` lines, and plays back for real."""

    def __init__(self, gif=GIF, play=True, measure=True):
        self.gif, self.play, self.measure = gif, play, measure
        self.rows = []

    def __call__(self, tape):
        text = tape.read_text()
        rows = int(re.search(r"^Set Rows (\d+)$", text, re.MULTILINE)[1])
        out = Path(json.loads(re.search(r"^Output (.+)$", text, re.MULTILINE)[1]))
        names = re.findall(r'^Type "vault-mirror (\w+)"$', text, re.MULTILINE)
        directory = tape.parent
        if out.suffix == ".ascii":
            self.rows.append(rows)
            frames = []
            for name in names:
                recorded = json.loads((directory / f"{name}.json").read_text())
                body = output(recorded["events"]).decode().split("\r\n")[:-1]
                marker = [recorded["marker"]] if self.measure else []
                frames.append(
                    frame([f"$ vault-mirror {name}", *body, "", *marker, "$"], rows)
                )
            out.write_text(ascii_text(frames))
        else:
            if self.play:
                for name in names:
                    subprocess.run(
                        [
                            sys.executable,
                            demo.__file__,
                            "replay",
                            str(directory / f"{name}.json"),
                        ],
                        stdout=subprocess.DEVNULL,
                        check=True,
                        timeout=30,
                    )
            out.write_bytes(self.gif)


class RenderTest(unittest.TestCase):
    def setUp(self):
        work = tempfile.TemporaryDirectory()
        self.addCleanup(work.cleanup)
        self.directory = Path(work.name) / "work"
        self.directory.mkdir()
        self.published = Path(work.name) / "assets"
        self.published.mkdir()
        self.output = self.published / "demo.gif"
        self.output.write_bytes(PREVIOUS)
        self.recordings = {
            "check": recording("VM_DEMO_END_1", ["checked"]),
            "run": recording("VM_DEMO_END_2", [f"line {i}" for i in range(70)]),
        }

    def render(self, renderer):
        return demo.render(self.recordings, self.output, "", self.directory, renderer)

    def assertPreserved(self):
        self.assertEqual(self.output.read_bytes(), PREVIOUS)
        self.assertEqual([p.name for p in self.published.iterdir()], ["demo.gif"])

    def test_grows_the_measurement_and_pages_long_output(self):
        vhs = FakeVhs()
        report = self.render(vhs)
        self.assertEqual(vhs.rows, [32, 64, 128])
        self.assertEqual(self.output.read_bytes(), GIF)
        self.assertEqual(report["rows"], demo.MAX_ROWS)
        self.assertEqual(report["total_rows"], [4, 73])
        self.assertEqual(report["pages"][0], [0])
        self.assertEqual(report["pages"][1][-1] + demo.MAX_ROWS, 73)

    def test_short_output_uses_the_smallest_terminal(self):
        self.recordings["run"] = recording("VM_DEMO_END_2", ["done"])
        self.assertEqual(self.render(FakeVhs())["rows"], demo.MIN_ROWS)

    def test_failed_measurement_keeps_the_previous_gif(self):
        def renderer(tape):
            raise RuntimeError("VHS rendering failed")

        with self.assertRaises(RuntimeError):
            self.render(renderer)
        self.assertPreserved()

    def test_missing_measurement_keeps_the_previous_gif(self):
        with self.assertRaisesRegex(RuntimeError, "terminal measurement"):
            self.render(lambda _: None)
        self.assertPreserved()

    def test_output_beyond_the_recording_limit_keeps_the_previous_gif(self):
        vhs = FakeVhs(measure=False)
        with self.assertRaisesRegex(RuntimeError, "exceeds"):
            self.render(vhs)
        self.assertEqual(vhs.rows[-1], demo.MAX_MEASURE_ROWS)
        self.assertPreserved()

    def test_failed_playback_keeps_the_previous_gif(self):
        vhs = FakeVhs()

        def renderer(tape):
            if tape.name != "measure.tape":
                raise RuntimeError("VHS rendering failed")
            vhs(tape)

        with self.assertRaises(RuntimeError):
            self.render(renderer)
        self.assertPreserved()

    def test_unfinished_playback_keeps_the_previous_gif_even_after_an_earlier_run(self):
        self.render(FakeVhs())
        self.output.write_bytes(PREVIOUS)
        with self.assertRaisesRegex(RuntimeError, "did not finish"):
            self.render(FakeVhs(play=False))
        self.assertPreserved()

    def test_invalid_gif_keeps_the_previous_gif(self):
        for gif in (b"", b"not a gif", GIF.replace(FRAME * 2, FRAME)):
            with (
                self.subTest(gif=gif[:9]),
                self.assertRaises((RuntimeError, subprocess.SubprocessError)),
            ):
                self.render(FakeVhs(gif=gif))
            self.assertPreserved()


class MainTest(unittest.TestCase):
    """Drives main() with fake sandbox binaries so setup and capture failures reach no renderer."""

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
        path.write_text(f"#!{sys.executable}\nimport sys\n{body}\n")
        path.chmod(0o755)

    def sandbox(self, source="http://127.0.0.1:18091", status=0):
        self.fake(
            self.bin / "examples/sandbox",
            f"print('SOURCE_SERVER={source}\\nDEST_SERVER=http://127.0.0.1:18092\\nSOURCE_PASSWORD=pw-7Qx\\nDEST_CLIENT_SECRET=cs-9Lm')\n"
            f"sys.exit({status})",
        )

    def vault_mirror(self, failing=None, echo=""):
        self.fake(
            self.bin / "vault-mirror",
            f"open({str(self.calls)!r}, 'a').write(sys.argv[1] + '\\n')\n"
            "print('\\x1b[32mok\\x1b[0m', sys.argv[1])\n"
            f"print({echo!r} if sys.argv[1] == 'run' else '', end='')\n"
            f"sys.exit(4 if sys.argv[1] == {failing!r} else 0)",
        )

    def main(self, render=None):
        argv = ["demo.py", "--bin-dir", str(self.bin), "--output", str(self.output)]
        render = render or mock.Mock(side_effect=AssertionError("render must not run"))
        with (
            mock.patch.object(sys, "argv", argv),
            mock.patch.object(demo, "render", render),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            demo.main()
        return render

    def calls_made(self):
        return self.calls.read_text().split() if self.calls.exists() else []

    def test_failed_setup_records_nothing(self):
        self.sandbox(status=1)
        self.vault_mirror()
        with self.assertRaisesRegex(RuntimeError, "setup failed"):
            self.main()
        self.assertEqual(self.calls_made(), [])
        self.assertEqual(self.output.read_bytes(), PREVIOUS)

    def test_refuses_servers_outside_the_sandbox(self):
        self.sandbox(source="https://vault.example.com")
        self.vault_mirror()
        with self.assertRaisesRegex(RuntimeError, "sandbox"):
            self.main()
        self.assertEqual(self.calls_made(), [])
        self.assertEqual(self.output.read_bytes(), PREVIOUS)

    def test_failed_command_keeps_the_previous_gif(self):
        self.sandbox()
        self.vault_mirror(failing="run")
        with self.assertRaisesRegex(RuntimeError, "exit status 4"):
            self.main()
        self.assertEqual(self.calls_made(), ["check", "run"])
        self.assertEqual(self.output.read_bytes(), PREVIOUS)

    def test_output_that_shows_a_sandbox_credential_is_not_published(self):
        for secret in ("pw-7Qx", "cs-9Lm"):
            with self.subTest(secret=secret):
                self.sandbox()
                self.vault_mirror(echo=f"token {secret}\n")
                with self.assertRaisesRegex(RuntimeError, "credentials") as caught:
                    self.main()
                self.assertNotIn(secret, str(caught.exception))
                self.assertEqual(self.output.read_bytes(), PREVIOUS)

    def test_renders_both_recordings_from_a_terminal(self):
        self.sandbox()
        self.vault_mirror()
        render = self.main(mock.Mock(return_value={}))
        recordings = render.call_args.args[0]
        self.assertEqual(list(recordings), ["check", "run"])
        self.assertEqual(
            output(recordings["run"]["events"]), b"\x1b[32mok\x1b[0m run\r\n"
        )
        self.assertNotEqual(recordings["check"]["marker"], recordings["run"]["marker"])


def swatch(rgb, width):
    return "\x1b[48;2;{};{};{}m".format(*rgb) + " " * width + "\x1b[0m"


def palette(count):
    """Colors far from each other and from the theme, so rendering noise cannot confuse them."""
    levels = range(40, 241, 40)
    theme = [(0x14, 0x17, 0x1C), (0xD0, 0xD4, 0xDA), (0x6B, 0x73, 0x80)]
    colors = [
        (r, g, b)
        for r in levels
        for g in levels
        for b in levels
        if all(max(abs(x - y) for x, y in zip((r, g, b), t)) > 30 for t in theme)
    ]
    return colors[:count]


def visible(png, wanted):
    """The wanted colors that fill at least a few cells of a rendered screenshot."""
    pixels = subprocess.run(
        [
            "ffmpeg",
            "-v",
            "error",
            "-i",
            str(png),
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
    counts = collections.Counter(pixels[i : i + 3] for i in range(0, len(pixels), 3))
    return {
        color
        for color in wanted
        if sum(
            n
            for p, n in counts.items()
            if max(abs(x - y) for x, y in zip(p, color)) <= 10
        )
        >= 1000
    }


@unittest.skipUnless(
    os.environ.get("VAULT_MIRROR_TEST_VHS") == "1",
    "set VAULT_MIRROR_TEST_VHS=1 to render with VHS",
)
class VhsTest(unittest.TestCase):
    """Proves from rendered pixels that the held pages show every row, including wrapped ones."""

    def test_pages_show_every_row_of_long_output(self):
        colors = palette(93)
        rows, chunks, check = colors[:80], colors[80:90], colors[90:]
        # Ends with a line wider than the terminal, so the last page is only right if wrapped rows count.
        run_lines = [swatch(rgb, 40) for rgb in rows] + [
            "".join(swatch(rgb, 26) for rgb in chunks)
        ]
        recordings = {
            "check": recording(
                "VM_DEMO_END_" + "1" * 8, [swatch(rgb, 40) for rgb in check]
            ),
            "run": recording("VM_DEMO_END_" + "2" * 8, run_lines),
        }
        style = (Path(demo.ROOT) / "assets/demo.tape").read_text()
        with tempfile.TemporaryDirectory() as work:
            directory = Path(work)
            published = directory / "assets/demo.gif"
            published.parent.mkdir()
            published.write_bytes(PREVIOUS)
            report = demo.render(recordings, published, style, directory)
            self.assertNotEqual(published.read_bytes(), PREVIOUS)

            self.assertEqual(report["rows"], demo.MAX_ROWS)
            wrapped = -(-26 * len(chunks) // demo.COLUMNS)
            self.assertGreaterEqual(
                report["total_rows"][1], 1 + len(rows) + wrapped + 1
            )
            self.assertEqual(visible(directory / "check-0.png", colors), set(check))

            shown = [
                visible(directory / f"run-{page}.png", colors)
                for page in range(len(report["pages"][1]))
            ]
            self.assertGreaterEqual(len(shown), 4)
            self.assertIn(rows[0], shown[0])
            self.assertIn(chunks[-1], shown[-1])
            self.assertEqual(set().union(*shown), set(rows + chunks))
            for before, after in itertools.pairwise(shown):
                self.assertGreaterEqual(len(before & after), 2)


if __name__ == "__main__":
    unittest.main()
