# Setting up Monitor (for AI agents)

You're probably here because your user asked you to install Monitor on one or more of their machines. Everything below is non-interactive. Supported: macOS (Apple silicon, Intel) and Linux (x86_64, arm64) with systemd. No Windows.

## 1. Install on each machine

```sh
curl -fsSL https://raw.githubusercontent.com/calebfender/monitor/main/install.sh | sh -s -- --name "<display name>"
```

- Installs to `~/.local/share/monitor` (binary, `scan.py`, `history.tsv`, `data.json`). No root needed.
- macOS: launchd agent `io.github.calebfender.monitor`, log in `~/.local/share/monitor/monitor.log`.
- Linux: `systemctl --user` units `monitor.service`, `monitor-scan.service`, `monitor-scan.timer` (nightly 04:30). On a server, also run `sudo loginctl enable-linger "$USER"` or it stops at logout.
- Re-running the installer upgrades in place and rewrites the service with the new options. History is kept.
- Every option after `--` goes to the monitor (`--port`, `--bind`, `--name`, `--peer`, `--allow-origin`, `--dir`); see `monitor --help`.
- No prebuilt binary for the platform: clone the repo, then `./install.sh --from-source` (needs Rust).

Verify:

```sh
curl -fsS http://127.0.0.1:8620/api/config     # {"version":...,"name":...,"peers":[...]}
curl -fsS http://127.0.0.1:8620/api/live | head -c 200
```

## 2. Disk tab

The Disk tab appears once `data.json` exists.

- Linux: `python3 ~/.local/share/monitor/scan.py` (also runs nightly).
- macOS: run the same command from the user's Terminal session, not from a launchd job: macOS privacy protection blocks background services from reading the home folder. A scan takes 1 to 3 minutes. If your harness has a shell on the Mac, running it there works.
- Optional on macOS: `brew install mole` makes the cache recommendations more complete. Ask before installing software if your user expects that.

The page never deletes anything; it builds a script for the user to run. Don't run cleanup commands on the user's behalf without asking.

## 3. Let the user open it from other devices

The monitor listens on `127.0.0.1:8620` with no login, so don't expose it publicly. If the machine is on Tailscale:

```sh
tailscale serve --bg --https=8620 http://127.0.0.1:8620
tailscale serve status    # gives https://<machine>.<tailnet>.ts.net:8620
```

Give the user that HTTPS URL. Otherwise a reverse proxy on a private network, or `--bind 0.0.0.0` on a trusted LAN only.

## 4. Several machines in one page

Install on each machine, then re-run the installer on each with the others as peers, using the URLs from step 3:

```sh
# on machine A
curl -fsSL .../install.sh | sh -s -- --name "A" --peer "B=https://b.tailnet.ts.net:8620"
# on machine B
curl -fsSL .../install.sh | sh -s -- --name "B" --peer "A=https://a.tailnet.ts.net:8620"
```

Rules that trip people up:
- Peers are listed both ways: each monitor only answers cross-origin requests from origins in its own `--peer`/`--allow-origin` list.
- The origin must match exactly, including scheme and port (`https://a.tailnet.ts.net:8620`, not `http://...` or no port).
- A page served over HTTPS can't fetch an `http://` peer. Use HTTPS everywhere (`tailscale serve` does this).
- Check from a browser on the page's origin, or: `curl -sI -H "Origin: https://a.tailnet.ts.net:8620" https://b.tailnet.ts.net:8620/api/live | grep -i access-control` should print the origin back.

## Troubleshooting

| Symptom | Check |
|---|---|
| Nothing on port 8620 | macOS: `cat ~/.local/share/monitor/monitor.log`, `launchctl print gui/$(id -u)/io.github.calebfender.monitor`. Linux: `journalctl --user -u monitor -n 50` |
| "can't listen" in the log | Port taken (`lsof -i :8620`); re-install with `--port N` |
| A peer shows "offline" | Its URL from the page's browser, the CORS check above, and that both sides list each other |
| No Disk tab | `ls ~/.local/share/monitor/data.json`; run the scan (step 2). With `--dir`, the scan needs the same `--dir` |
| Disk scan slow | Huge trees (backups, snapshot folders) outside home are capped at 3 minutes each and shown as unscanned |
| Uninstall | `curl -fsSL .../install.sh \| sh -s -- --uninstall` (keeps data; then `rm -r ~/.local/share/monitor` to remove it) |

## Working on the code

- `src/main.rs`: sampler thread, HTTP server, CLI flags, JSON API. `src/mac.rs` / `src/linux.rs`: OS readers. Only dependency: `libc`.
- `index.html` and `icons/` are compiled into the binary with `include_str!`/`include_bytes!`: rebuild after editing them.
- `scan.py`: Python 3 standard library only. Recommendations are rules in `mac_recommendations()` / `linux_recommendations()`; each has a `group` (`safe` or `review`), the absolute paths it covers (`match`), and the shell command (`cmd`). Quote every path with `shlex.quote`.
- Release: push a `v*` tag; `.github/workflows/release.yml` builds `monitor-macos` (universal), `monitor-linux-x86_64`, `monitor-linux-aarch64`, and attaches `scan.py` and `install.sh`. `install.sh` downloads those exact names from the latest release.
