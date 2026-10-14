# Terminal demo for the README, recorded with VHS (https://github.com/charmbracelet/vhs).
# Regenerate from the repository root, with Docker running:
#
#   cargo build --release --bin vault-mirror --example sandbox && sandbox/up.sh && vhs assets/demo.tape
#
# CI does this for every release and publishes the result; see .github/workflows/demo.yml.
#
# The hidden setup creates two throwaway sandbox accounts with sample data and
# loads their settings from a temporary directory without printing them.

Output assets/demo.gif

Set Shell "bash"
Set FontFamily "DejaVu Sans Mono"
Set FontSize 16
Set LineHeight 1.2
Set Width 1240
Set Height 700
Set Padding 24
Set Margin 0
Set BorderRadius 0
Set Framerate 20
Set PlaybackSpeed 1
Set TypingSpeed 45ms
Set CursorBlink false
Set WaitTimeout 60s
Set WaitPattern /\$$/
Set Theme { "name": "vault-mirror", "background": "#14171c", "foreground": "#d0d4da", "cursor": "#d0d4da", "selection": "#2c3340", "black": "#14171c", "red": "#e06c6c", "green": "#8cc085", "yellow": "#d8b36a", "blue": "#7aa6da", "magenta": "#b48ead", "cyan": "#78b8b8", "white": "#d0d4da", "brightBlack": "#6b7380", "brightRed": "#e06c6c", "brightGreen": "#8cc085", "brightYellow": "#d8b36a", "brightBlue": "#7aa6da", "brightMagenta": "#b48ead", "brightCyan": "#78b8b8", "brightWhite": "#ffffff" }

Hide
Type `export PATH="$PWD/target/release:$PATH" DEMO=$(mktemp -d)` Enter
Type `target/release/examples/sandbox --seed > "$DEMO/env"` Enter
Type `set -a && . "$DEMO/env" && set +a && cd "$DEMO" && export BACKUPS_DIR=backups` Enter
Type `PS1='\[\e[90m\]$\[\e[0m\] ' && clear` Enter
Sleep 500ms
Show

Sleep 800ms
Type "vault-mirror check"
Sleep 400ms
Enter
Wait
Sleep 3.5s

Type "vault-mirror run"
Sleep 400ms
Enter
Wait
Sleep 5s

Type "vault-mirror run"
Sleep 400ms
Enter
Wait
Sleep 4s

Type "vault-mirror snapshots"
Sleep 400ms
Enter
Wait
Sleep 5s

Hide
Type `cd / && rm -rf "$DEMO"` Enter
Sleep 300ms
