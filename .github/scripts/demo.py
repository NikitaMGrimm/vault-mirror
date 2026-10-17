"""Capture the sandbox commands once, then fit their real terminal output with VHS."""

import argparse
import base64
import errno
import fcntl
import json
import math
import os
import pty
import selectors
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
COLUMNS = 100
MIN_ROWS = 13
MAX_ROWS = 24
MAX_MEASURE_ROWS = 512
SEPARATOR = "─" * 80


def capture(argv, env, cwd, timeout=300):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", MAX_ROWS, COLUMNS, 0, 0))
    events = []
    started = previous = time.monotonic()
    process = None
    succeeded = False
    try:
        process = subprocess.Popen(
            argv,
            cwd=cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=slave,
            stderr=slave,
            start_new_session=True,
        )
        os.close(slave)
        slave = None
        with selectors.DefaultSelector() as selector:
            selector.register(master, selectors.EVENT_READ)
            while True:
                remaining = timeout - (time.monotonic() - started)
                if remaining <= 0:
                    raise RuntimeError("sandbox command timed out")
                if not selector.select(min(remaining, 0.1)):
                    continue
                try:
                    data = os.read(master, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    break
                if not data:
                    break
                now = time.monotonic()
                events.append([now - previous, base64.b64encode(data).decode()])
                previous = now
        remaining = timeout - (time.monotonic() - started)
        try:
            status = process.wait(timeout=max(remaining, 0.001))
        except subprocess.TimeoutExpired:
            raise RuntimeError("sandbox command timed out") from None
        if status != 0:
            raise RuntimeError(f"sandbox command failed with exit status {status}")
        succeeded = True
        return events
    finally:
        if process is not None and not succeeded:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
        os.close(master)
        if slave is not None:
            os.close(slave)


def replay(path, measure=False):
    recording = json.loads(Path(path).read_text())
    for delay, encoded in recording["events"]:
        if not measure:
            # Waiting for the server is not useful footage; retain the actual output.
            time.sleep(min(delay, 0.25))
        sys.stdout.buffer.write(base64.b64decode(encoded))
        sys.stdout.buffer.flush()
    if measure:
        sys.stdout.buffer.write(f"\r\n{recording['marker']}\r\n".encode())
        sys.stdout.buffer.flush()
    else:
        Path(path).with_suffix(".done").write_text("complete\n")


def pages(total_rows, visible_rows):
    offsets = [0]
    while offsets[-1] + visible_rows < total_rows:
        offsets.append(min(offsets[-1] + visible_rows - 2, total_rows - visible_rows))
    return offsets


def measured_rows(text, recordings):
    frames = [frame.splitlines() for frame in text.split(SEPARATOR + "\n")]
    sizes = []
    for name, recording in recordings.items():
        matches = [
            rows
            for rows in frames
            if f"$ vault-mirror {name}" in [line.strip() for line in rows]
            and recording["marker"] in [line.strip() for line in rows]
        ]
        if not matches:
            return None
        rows = matches[-1]
        # The measurement marker occupies the blank line before the normal prompt.
        sizes.append(
            next(
                i for i, line in enumerate(rows) if line.strip() == recording["marker"]
            )
            + 1
        )
    return sizes


def tape_text(style, directory, recordings, rows, measure, totals=None):
    output = directory / ("measure.ascii" if measure else "demo.gif")
    lines = [
        f"Output {json.dumps(str(output))}",
        style,
        f"Set Rows {rows}",
        f"Set Columns {COLUMNS}",
        "Set TypingSpeed 1ms",
    ]
    longest = max(
        sum(min(event[0], 0.25) for event in r["events"]) for r in recordings.values()
    )
    lines.append(f"Set WaitTimeout {math.ceil(longest + 15)}s")
    session = directory / ("measure.sh" if measure else "replay.sh")
    replay_argv = shlex.join([sys.executable, str(Path(__file__).resolve()), "replay"])
    session.write_text(
        "unset PROMPT_COMMAND\nPS1='\\[\\e[90m\\]$ \\[\\e[0m\\]'\n"
        "vm_clear() { printf '\\033[3J\\033[H\\033[2J'; }\n"
        f'vault-mirror() {{ {replay_argv} {shlex.quote(str(directory))}/"$1.json"'
        + (" --measure" if measure else "")
        + "; }\n"
    )
    lines += [
        "Hide",
        f"Type {json.dumps('source ' + shlex.quote(str(session)))}",
        "Enter",
    ]
    for index, name in enumerate(recordings):
        clear = "vm_clear"
        lines += [
            "Hide",
            f"Type {json.dumps(clear)}",
            "Enter",
            "Wait+Line /^\\$$/",
            "Show",
        ]
        lines += [
            f"Set TypingSpeed {'1ms' if measure else '35ms'}",
            f'Type "vault-mirror {name}"',
        ]
        offsets = [0] if measure else pages(totals[index], rows)
        if not measure and len(offsets) > 1:
            lines.append("Hide")
        lines += ["Enter", "Wait+Line /^\\$$/"]
        if not measure:
            if len(offsets) > 1:
                lines += [f"ScrollUp@1ms {totals[index]}", "Show"]
            for page, offset in enumerate(offsets):
                if page:
                    lines.append(f"ScrollDown@25ms {offset - offsets[page - 1]}")
                lines.append("Sleep 100ms")
                lines.append(
                    f"Screenshot {json.dumps(str(directory / f'{name}-{page}.png'))}"
                )
                lines.append(
                    f"Sleep {'6s' if len(offsets) > 1 else ('2.5s' if name == 'check' else '3.5s')}"
                )
    lines.append("Hide")
    return "\n".join(lines) + "\n"


def run_vhs(tape, timeout=300):
    with (
        tempfile.TemporaryFile() as log,
        subprocess.Popen(
            ["vhs", str(tape)],
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        ) as process,
    ):
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            # Let VHS close its browser and terminal before forcing the group down.
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                pass
            finally:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
            raise RuntimeError("VHS rendering timed out") from None
        if process.returncode:
            log.seek(0)
            sys.stderr.buffer.write(log.read())
            raise RuntimeError("VHS rendering failed")


def render(recordings, output, style, directory, renderer=run_vhs):
    for name, recording in recordings.items():
        (directory / f"{name}.json").write_text(json.dumps(recording))
    rows = 32
    while rows <= MAX_MEASURE_ROWS:
        tape = directory / "measure.tape"
        tape.write_text(tape_text(style, directory, recordings, rows, True))
        (directory / "measure.ascii").unlink(missing_ok=True)
        renderer(tape)
        measurement = directory / "measure.ascii"
        if not measurement.is_file():
            raise RuntimeError("VHS did not produce the terminal measurement")
        totals = measured_rows(measurement.read_text(), recordings)
        if totals is not None:
            break
        rows *= 2
    else:
        raise RuntimeError(
            "demo output exceeds the recording limit; the published GIF was not changed"
        )
    visible = min(MAX_ROWS, max(MIN_ROWS, max(totals)))
    tape = directory / "demo.tape"
    tape.write_text(tape_text(style, directory, recordings, visible, False, totals))
    gif = directory / "demo.gif"
    gif.unlink(missing_ok=True)
    for name in recordings:
        (directory / f"{name}.done").unlink(missing_ok=True)
    renderer(tape)
    if any(not (directory / f"{name}.done").exists() for name in recordings):
        raise RuntimeError(
            "the terminal playback did not finish; the previous GIF was not changed"
        )
    probe = subprocess.run(
        [
            "ffprobe",
            "-v",
            "error",
            "-count_frames",
            "-show_entries",
            "stream=codec_name,width,height,nb_read_frames",
            "-of",
            "json",
            str(gif),
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    streams = json.loads(probe.stdout)["streams"]
    if (
        len(streams) != 1
        or streams[0]["codec_name"] != "gif"
        or int(streams[0]["nb_read_frames"]) < 2
        or any(int(streams[0][key]) <= 0 for key in ("width", "height"))
    ):
        raise RuntimeError("the rendered GIF is empty or invalid")
    # Keep the previous GIF until every capture and render check has succeeded.
    output.parent.mkdir(parents=True, exist_ok=True)
    fd, staged = tempfile.mkstemp(prefix=".demo-", suffix=".gif", dir=output.parent)
    try:
        with os.fdopen(fd, "wb") as file:
            file.write(gif.read_bytes())
        os.replace(staged, output)
    finally:
        Path(staged).unlink(missing_ok=True)
    return {
        "rows": visible,
        "columns": COLUMNS,
        "total_rows": totals,
        "pages": [pages(total, visible) for total in totals],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=ROOT / "target/release")
    parser.add_argument("--output", type=Path, default=ROOT / "assets/demo.gif")
    parser.add_argument("--artifacts", type=Path)
    args = parser.parse_args()
    env = {
        key: os.environ[key] for key in ("PATH", "HOME", "LANG") if key in os.environ
    }
    env.update(TERM="xterm-256color", CLICOLOR_FORCE="1")
    binary = (args.bin_dir / "vault-mirror").resolve()
    setup_binary = (args.bin_dir / "examples/sandbox").resolve()
    with tempfile.TemporaryDirectory(prefix="vault-mirror-demo-") as work:
        directory = Path(work)
        setup = subprocess.run(
            [str(setup_binary), "--seed"],
            check=False,
            env=env,
            capture_output=True,
            text=True,
            timeout=300,
        )
        if setup.returncode:
            raise RuntimeError("sandbox account setup failed")
        env.update(
            line.split("=", 1) for line in setup.stdout.splitlines() if "=" in line
        )
        if (
            env.get("SOURCE_SERVER") != "http://127.0.0.1:18091"
            or env.get("DEST_SERVER") != "http://127.0.0.1:18092"
        ):
            raise RuntimeError("the demo requires the throwaway localhost sandbox")
        env["BACKUPS_DIR"] = str(directory / "backups")
        recordings = {}
        for name in ("check", "run"):
            recordings[name] = {
                "marker": "VM_DEMO_END_" + uuid.uuid4().hex,
                "events": capture([str(binary), name], env, directory),
            }
        recorded = b"".join(
            base64.b64decode(event[1])
            for r in recordings.values()
            for event in r["events"]
        )
        secrets = [
            value
            for key, value in env.items()
            if key.endswith(("_PASSWORD", "_CLIENT_SECRET", "_PASSPHRASE"))
        ]
        if any(value and value.encode() in recorded for value in secrets):
            raise RuntimeError("the command output contains sandbox credentials")
        style = (ROOT / "assets/demo.tape").read_text()
        report = render(recordings, args.output.resolve(), style, directory)
        if args.artifacts:
            args.artifacts.mkdir(parents=True, exist_ok=True)
            for path in directory.iterdir():
                if path.is_file():
                    shutil.copyfile(path, args.artifacts / path.name)
            (args.artifacts / "layout.json").write_text(
                json.dumps(report, indent=2) + "\n"
            )
        print(json.dumps(report))


if __name__ == "__main__":
    os.umask(0o077)
    try:
        if len(sys.argv) > 1 and sys.argv[1] == "replay":
            replay(sys.argv[2], "--measure" in sys.argv[3:])
        else:
            main()
    except (RuntimeError, subprocess.SubprocessError) as error:
        sys.exit(str(error))
