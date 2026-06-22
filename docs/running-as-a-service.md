# Running `looper-cli watch` as a background service

`watch` is the headless form: it scans + keeps a folder's OKF index fresh, streams events, and exits
cleanly on **SIGTERM/SIGINT** (drains + joins workers — in-flight index writes finish). That makes it
a good fit for `systemd` (Linux) or `launchd` (macOS). Use `--json` so the supervisor's log captures
structured, line-delimited events.

## Prerequisites

```sh
# 1. Install the binary (so the service can use an absolute path):
cargo install --path crates/looper-cli          # → ~/.cargo/bin/looper-cli

# 2. Create a saved workspace the service will index (one-time):
looper-cli workspace create                      # → name it e.g. "mydocs"
looper-cli workspace list                        # confirm it's there
```

A saved `--workspace` is recommended for a service (stable config in `workspaces.json`), but
`watch <folder>... --kb <dir>` works too.

## Linux — systemd (user service)

`~/.config/systemd/user/looper-watch.service`:

```ini
[Unit]
Description=Looper — watch + index a workspace
After=default.target

[Service]
# Absolute path to the installed binary; %h is your home dir.
ExecStart=%h/.cargo/bin/looper-cli watch --workspace mydocs --json
Restart=always
RestartSec=5
# `systemctl stop` sends SIGTERM → looper-cli drains + exits 0 within this window.
KillSignal=SIGTERM
TimeoutStopSec=30

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now looper-watch.service
journalctl --user -u looper-watch -f      # follow the JSONL event stream
systemctl --user stop looper-watch        # clean drain (SIGTERM)
```

(For a system-wide service, drop the unit in `/etc/systemd/system/`, add `User=`/`WorkingDirectory=`,
and use `systemctl` without `--user`.)

## macOS — launchd (LaunchAgent)

`~/Library/LaunchAgents/dev.looper.watch.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>            <string>dev.looper.watch</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/YOU/.cargo/bin/looper-cli</string>
    <string>watch</string>
    <string>--workspace</string>
    <string>mydocs</string>
    <string>--json</string>
  </array>
  <key>RunAtLoad</key>        <true/>
  <key>KeepAlive</key>        <true/>
  <key>StandardOutPath</key>  <string>/tmp/looper-watch.log</string>
  <key>StandardErrorPath</key><string>/tmp/looper-watch.err</string>
</dict>
</plist>
```

```sh
launchctl load   ~/Library/LaunchAgents/dev.looper.watch.plist   # start (RunAtLoad)
tail -f /tmp/looper-watch.log                                    # JSONL events
launchctl unload ~/Library/LaunchAgents/dev.looper.watch.plist   # clean drain (SIGTERM)
```

## Notes

- **Graceful stop is built in:** `systemctl stop` / `launchctl unload` send SIGTERM, which `watch`
  catches → drains + joins → exits 0. (Index writes are also atomic, so even a hard kill can't
  corrupt the index — but the clean path lets the current pass finish.)
- **`--json`** is the right choice under a supervisor: each event is one JSON object per line; pipe
  the log into your aggregator.
- **Not built in:** PID files, log rotation, multi-workspace fan-out — let the supervisor handle
  those (or run one service per workspace).
