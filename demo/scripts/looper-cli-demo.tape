# looper-cli — live OKF index demo (a VHS tape: https://github.com/charmbracelet/vhs).
#
# Records: an agent-style stream of markdown edits, looper-cli's Activity TUI maintaining the OKF
# index live, opening a doc in the markdown viewer, then the generated index on disk.
#
# Prereqs: vhs (+ its ttyd + ffmpeg deps). Then, from the looper-cli repo ROOT:
#     cargo build --release
#     vhs demo/scripts/looper-cli-demo.tape
# Produces demo/scripts/looper-cli-demo.gif (+ .mp4).

Output demo/scripts/looper-cli-demo.gif
Output demo/scripts/looper-cli-demo.mp4

Set Shell "bash"
Set FontSize 18
Set Width 1320
Set Height 760
Set Padding 24
Set TypingSpeed 45ms

# --- hidden setup (one line, so it doesn't eat the timeline): release binary on PATH, remember the
#     repo root, start from a fresh index, move into demo/. ---
Hide
Type `export PATH="$PWD/target/release:$PATH"; ROOT="$PWD"; git clean -fdX -- demo/kb >/dev/null 2>&1; cd demo; clear`
Enter
Show

# --- narrate (read before the TUI takes the screen) ---
Type "# You work with specs, you have markdown across 20+ repos. Good luck remembering where anything is."
Enter
Enter
Sleep 1400ms
Type "# looper-cli will scan, index, and watch all markdown files for changes,"
Enter
Type "# maintaining a knowledge base for you"
Enter
Enter
Sleep 1600ms

# --- start the change-generator in the background from the repo root, then clear its command
#     off-screen — all hidden. It self-stops + restores the docs after ~4s (SIGINT), which lands
#     during the watch below, so the doc we open afterward is already clean. ---
Hide
Type `( cd "$ROOT" && timeout -s INT 4 looper-cli demo >/dev/null 2>&1 & )`
Enter
Type "clear"
Enter
Show

# --- launch the live Activity TUI (overlaps the generator → live "changed" rows) ---
Type "looper-cli tui frontend backend sre --kb kb"
Sleep 1s
Enter
Sleep 2s

# --- Tab into the activity list, pick a doc, open it in the markdown viewer ---
Tab
Sleep 1200ms
Down
Sleep 600ms
Down
Sleep 700ms
Enter
Sleep 3500ms
Type "q"
Sleep 1s

# --- show the generated index on disk ---
Enter
Type "# let's look at the generated index!"
Enter
Sleep 1200ms
Type "clear"
Enter
Type "ls -la kb"
Enter
Sleep 1800ms
Type `for dir in frontend backend sre; do count=$(find "$dir" -type f -name '*.md' | wc -l | tr -d ' '); printf '%-8s | %s markdown files\n' "$dir" "$count"; done`
Enter
Sleep 2200ms
Enter
Enter
Type "# We have our OKF index, maintained automatically."
Enter
Sleep 1400ms
Type "# Now you can add tooling around your knowledge base, to become DRY with your team, and agents."
Enter
Sleep 2800ms

# --- hidden cleanup: strip any leftover demo markers (no-op if the generator already restored) ---
Hide
Type "cd .. && just demo reset >/dev/null 2>&1 || true"
Enter
Sleep 1s
