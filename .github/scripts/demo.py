import contextlib
import json
import os
import shlex
import signal
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
COMMANDS = ("check", "run")
COLUMNS = 100
MIN_ROWS = 13
MAX_ROWS = 24
MEASURE_ROWS = 512
SANDBOX = {
    "SOURCE_SERVER": "http://127.0.0.1:18091",
    "DEST_SERVER": "http://127.0.0.1:18092",
}
CREDENTIALS = ("_PASSWORD", "_CLIENT_SECRET", "_PASSPHRASE")
PROMPT = "Wait+Line /^\\$$/"

# Sourced in the VHS terminal. The commands already ran during capture, so
# `vault-mirror` only prints what they wrote.
SESSION = r"""unset PROMPT_COMMAND
PS1='\[\e[90m\]$ \[\e[0m\]'
clear() { printf '\e[3J\e[H\e[2J'; }
vault-mirror() { cat -- "$1.out"; }
"""
# Each command starts on the first row, so the cursor row the terminal reports
# afterwards is the height VHS draws, wrapped lines included.
MEASURE = r"""vault-mirror() {
  cat -- "$1.out"
  IFS='[;' read -rs -t 10 -d R -p $'\e[6n' _ row _
  echo "$row" > "$1.rows"
}
"""


def run(what, argv, timeout, **popen):
    """Returns the stdout of argv and stops whatever it left running.

    Errors never include the output, since it can hold vault content.
    """

    def signal_group(sig):
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, sig)

    with subprocess.Popen(argv, start_new_session=True, **popen) as process:
        try:
            stdout, _ = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            # Let VHS close its browser and terminal before the group is killed.
            signal_group(signal.SIGTERM)
            with contextlib.suppress(subprocess.TimeoutExpired):
                process.wait(timeout=10)
            raise RuntimeError(f"{what} timed out") from None
        finally:
            signal_group(signal.SIGKILL)
    if process.returncode:
        raise RuntimeError(f"{what} failed with exit status {process.returncode}")
    return stdout


def capture(argv, env, cwd, timeout=300):
    """The bytes argv writes to a 100-column terminal, stdout and stderr together."""
    command = f"stty cols {COLUMNS} rows {MAX_ROWS}; exec {shlex.join(argv)}"
    return run(
        "sandbox command",
        ["script", "-qefc", command, "/dev/null"],
        timeout,
        env=env,
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
    )


def pages(total_rows, visible_rows):
    """Scroll offsets that show every row, overlap by two rows and end on the last one."""
    offsets = [0]
    while offsets[-1] + visible_rows < total_rows:
        offsets.append(min(offsets[-1] + visible_rows - 2, total_rows - visible_rows))
    return offsets


def vhs(work, output, rows, steps):
    style = (ROOT / "assets/demo.tape").read_text()
    lines = [style, f"Output {output}", f"Set Rows {rows}", f"Set Columns {COLUMNS}"]
    lines += ["Hide", 'Type "source session.sh"', "Enter", *steps]
    tape = Path(output).with_suffix(".tape")
    (work / tape).write_text("\n".join(lines) + "\n")
    run("VHS", ["vhs", str(tape)], 300, cwd=work)


def measure(work, names):
    """The rows each command takes, from its command line to the prompt after it."""
    (work / "session.sh").write_text(SESSION + MEASURE)
    steps = []
    for name in names:
        steps += ['Type "clear"', "Enter", PROMPT]
        steps += [f'Type "vault-mirror {name}"', "Enter", PROMPT]
    # VHS fails a recording without frames.
    vhs(work, "measure.ascii", MEASURE_ROWS, [*steps, "Show", "Sleep 100ms"])
    try:
        totals = [int((work / f"{name}.rows").read_text()) for name in names]
    except (OSError, ValueError):
        raise RuntimeError("VHS did not report the terminal size") from None
    if max(totals) >= MEASURE_ROWS:
        raise RuntimeError("demo output exceeds the recording limit")
    return totals


def play(work, names, rows, totals):
    (work / "session.sh").write_text(SESSION)
    steps = []
    for name, total in zip(names, totals):
        offsets = pages(total, rows)
        steps += ["Hide", 'Type "clear"', "Enter", PROMPT, "Show"]
        steps.append(f'Type "vault-mirror {name}"')
        if len(offsets) > 1:
            # Start the first page at the command line, not at the end of the output.
            steps += ["Hide", "Enter", PROMPT, f"ScrollUp@1ms {total}", "Show"]
        else:
            steps += ["Enter", PROMPT]
        for previous, offset in zip([0, *offsets], offsets):
            if offset > previous:
                steps.append(f"ScrollDown@25ms {offset - previous}")
            hold = "6s" if len(offsets) > 1 else "2.5s" if name == "check" else "3.5s"
            steps.append(f"Sleep {hold}")
    vhs(work, "demo.gif", rows, steps)


def render(outputs, output, work):
    """Renders outputs and replaces output only once every check passed. Returns the rows shown."""
    for name, data in outputs.items():
        (work / f"{name}.out").write_bytes(data)
    names = list(outputs)
    totals = measure(work, names)
    rows = min(MAX_ROWS, max(MIN_ROWS, *totals))
    play(work, names, rows, totals)
    gif = work / "demo.gif"
    probe = run(
        "FFprobe",
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
        60,
        stdout=subprocess.PIPE,
    )
    streams = json.loads(probe)["streams"]
    if (
        len(streams) != 1
        or streams[0]["codec_name"] != "gif"
        or int(streams[0]["nb_read_frames"]) < 2
        or any(int(streams[0][key]) <= 0 for key in ("width", "height"))
    ):
        raise RuntimeError("the rendered GIF is empty or invalid")
    output.parent.mkdir(parents=True, exist_ok=True)
    fd, staged = tempfile.mkstemp(prefix=".demo-", suffix=".gif", dir=output.parent)
    try:
        with os.fdopen(fd, "wb") as file:
            file.write(gif.read_bytes())
        os.replace(staged, output)
    finally:
        Path(staged).unlink(missing_ok=True)
    return rows


def main(bin_dir=ROOT / "target/release", output=ROOT / "assets/demo.gif"):
    bin_dir = Path(bin_dir).resolve()
    env = {
        key: os.environ[key] for key in ("PATH", "HOME", "LANG") if key in os.environ
    }
    env.update(TERM="xterm-256color", CLICOLOR_FORCE="1")
    setup = run(
        "sandbox account setup",
        [str(bin_dir / "examples/sandbox"), "--seed"],
        300,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    env.update(
        line.split("=", 1) for line in setup.decode().splitlines() if "=" in line
    )
    if any(env.get(key) != value for key, value in SANDBOX.items()):
        raise RuntimeError("the demo requires the throwaway localhost sandbox")
    secrets = [
        value.encode()
        for key, value in env.items()
        if key.endswith(CREDENTIALS) and value
    ]
    with tempfile.TemporaryDirectory(prefix="vault-mirror-demo-") as work:
        env["BACKUPS_DIR"] = f"{work}/backups"
        outputs = {
            name: capture([str(bin_dir / "vault-mirror"), name], env, work)
            for name in COMMANDS
        }
        if any(secret in data for secret in secrets for data in outputs.values()):
            raise RuntimeError("the command output contains sandbox credentials")
        render(outputs, Path(output), Path(work))


if __name__ == "__main__":
    os.umask(0o077)
    try:
        main()
    except RuntimeError as error:
        sys.exit(str(error))
