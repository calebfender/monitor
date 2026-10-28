#!/usr/bin/env python3
"""Scan this machine's disk and write data.json for Monitor's Disk tab.

Sizes come from du. On macOS, if Mole (`brew install mole`) is installed, its
dry run decides which caches are safe to clear. Nothing here deletes anything:
each recommendation carries the command that would, and the page builds a
script from the ones you tick.

  python3 scan.py               writes ~/.local/share/monitor/data.json
  python3 scan.py --dir PATH    writes PATH/data.json (match monitor --dir)
"""
import argparse, json, os, re, shlex, shutil, subprocess, sys, time

HOME = os.path.expanduser("~")
H = HOME
LINUX = sys.platform.startswith("linux")
MIN_REC = 100e6  # recommendations smaller than this aren't worth showing


def du(path, depth, timeout=900):
    """Return {abs_path: bytes} for path down to depth.

    du prints each folder once it has finished it, so if it runs out of time the
    folders it already measured are kept and the slow one counts as unreadable.
    """
    if LINUX:  # GNU du
        cmd = ["du", "-x", "-k", "-d", str(depth), path]
    else:  # skip cloud-synced folders: reading them can trigger downloads or hang
        cmd = ["du", "-x", "-k", "-d", str(depth), "-I", "CloudStorage", "-I", "Mobile Documents", path]
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout).stdout
    except subprocess.TimeoutExpired as e:
        out = e.stdout or ""
        out = out.decode(errors="replace") if isinstance(out, bytes) else out
        out = out.rsplit("\n", 1)[0]  # drop a half-written last line
        print(f"{path}: gave up after {timeout}s, keeping what was measured", file=sys.stderr)
    sizes = {}
    for line in out.splitlines():
        kb, _, p = line.partition("\t")
        if kb.isdigit():
            sizes[os.path.normpath(p)] = int(kb) * 1024
    return sizes


def size_of(path):
    p = os.path.expanduser(path)
    if not os.path.exists(p):
        return 0
    return du(p, 0).get(os.path.normpath(p), 0)


def build_tree(root, sizes, name=None, min_bytes=40 * 2**20):
    """Turn a flat du listing into a nested tree, dropping tiny folders."""
    if root not in sizes:  # du ran out of time: total up the unfinished folders from what it measured
        done = dict(sizes)
        for p, b in done.items():
            parent = os.path.dirname(p)
            if parent in done:
                continue
            while p != root and parent.startswith(root):
                sizes[parent] = sizes.get(parent, 0) + b
                p, parent = parent, os.path.dirname(parent)
    kids = {}
    for p in sizes:
        if p != root:
            kids.setdefault(os.path.dirname(p), []).append(p)

    def node(p):
        b = sizes.get(p, 0)
        n = {"name": name if p == root else os.path.basename(p), "path": p, "size": b}
        ch = [c for c in kids.get(p, []) if sizes[c] >= min_bytes]
        if ch:
            ch.sort(key=lambda c: -sizes[c])
            n["children"] = [node(c) for c in ch]
            rest = b - sum(sizes[c] for c in ch)
            if rest > min_bytes:
                n["children"].append({"name": "Other files", "path": p + "/…", "size": rest, "other": True})
        return n

    return node(root)


def run(cmd, timeout=120):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout).stdout
    except (OSError, subprocess.TimeoutExpired):
        return ""


def q(p):
    return shlex.quote(p)


def rm(paths):
    return "\n".join(f"rm -rf {q(p)}" for p in paths)


def tilde(p):
    return "~" + p[len(H):] if p.startswith(H + "/") else p


class Recs(list):
    def add(self, **r):
        if r.get("size", 0) > MIN_REC:
            r.setdefault("breakdown", [])
            r.setdefault("sub", tilde(r["match"][0]) if r.get("match") else "")
            self.append(r)

    def folder(self, id, group, title, path, cmd=None, note="", sub=None):
        """A recommendation for one folder, if it exists."""
        p = os.path.expanduser(path)
        if os.path.exists(p):
            self.add(id=id, group=group, title=title, sub=sub or tilde(p), size=size_of(p), match=[p],
                     cmd=cmd or f"rm -rf {q(p)}", note=note)


def old_node_modules(days=30):
    """node_modules folders under home in projects untouched for `days` days."""
    out = run(["find", H, "-maxdepth", "6", "(", "-name", "Library", "-o", "-name", ".*", "-o", "-name", "snap", ")",
               "-prune", "-o", "-type", "d", "-name", "node_modules", "-prune", "-mtime", f"+{days}", "-print"], timeout=300)
    return [p for p in out.splitlines() if p]


def mole_dry_run():
    if not shutil.which("mo"):
        return None
    text = re.sub(r"\x1b\[[0-9;]*m", "", run(["mo", "clean", "--dry-run"], timeout=900))
    unit = {"GB": 1e9, "MB": 1e6, "KB": 1e3}
    m = re.search(r"Potential space:\s*([\d.]+)\s*(GB|MB|KB)", text)
    total = float(m.group(1)) * unit[m.group(2)] if m else 0
    sections, cur = [], None
    for line in text.splitlines():
        if line.startswith("➤"):
            cur = {"name": line[1:].strip(), "size": 0}
            sections.append(cur)
        elif cur and "→" in line:
            for v, u in re.findall(r"([\d.]+)(GB|MB|KB)", line):
                cur["size"] += float(v) * unit[u]
    sections = sorted((s for s in sections if s["size"] > 50e6), key=lambda s: -s["size"])
    return {"total": total, "sections": sections}


def dev_caches(recs, skip=()):
    """Package-manager caches every OS has. Each tool downloads again what it needs."""
    mac = not LINUX
    caches = [
        ("npm", "npm cache", "~/.npm/_cacache", "npm cache clean --force"),
        ("pnpm", "pnpm store", "~/Library/pnpm/store" if mac else "~/.local/share/pnpm/store", "pnpm store prune"),
        ("yarn", "Yarn cache", "~/Library/Caches/Yarn" if mac else "~/.cache/yarn", "yarn cache clean"),
        ("bun", "Bun cache", "~/.bun/install/cache", None),
        ("pip", "pip cache", "~/Library/Caches/pip" if mac else "~/.cache/pip", None),
        ("uv", "uv cache", "~/.cache/uv", "uv cache clean"),
        ("cargo", "Cargo download cache", "~/.cargo/registry/cache", None),
        ("gobuild", "Go build cache", "~/Library/Caches/go-build" if mac else "~/.cache/go-build", "go clean -cache"),
    ]
    for id, title, path, cmd in caches:
        if id in skip:
            continue
        tool = (cmd or "").split(" ")[0]
        if cmd and not shutil.which(tool):
            cmd = None  # tool is gone: deleting the folder is the same thing
        recs.folder(id, "safe", title, path, cmd=cmd, note="Downloaded packages, not your code. The tool fetches them again when needed.")


def common_review(recs):
    recs.folder("playwright", "review", "Playwright browsers", "~/Library/Caches/ms-playwright" if not LINUX else "~/.cache/ms-playwright",
                note="Browsers used by test tools. `npx playwright install` brings them back.")
    recs.folder("hf", "review", "Hugging Face models", "~/.cache/huggingface",
                note="Downloaded AI models. Anything that needs one downloads it again, which can be slow.")
    recs.folder("ollama", "review", "Ollama models", "~/.ollama/models", cmd="ollama list   # then: ollama rm <model>",
                note="Local AI models. Remove the ones you don't use with `ollama rm`.")
    nm = old_node_modules()
    recs.add(id="nodemods", group="review", title="Old project dependencies", sub="node_modules untouched for 30+ days",
             size=sum(size_of(p) for p in nm), match=nm, cmd=rm(nm),
             note="Downloaded packages in projects you haven't touched in a month, not your code. `npm install` brings them back.")
    if shutil.which("docker"):
        out = run(["docker", "system", "df", "--format", "{{.Type}}\t{{.Reclaimable}}"], timeout=30)
        unit = {"GB": 1e9, "MB": 1e6, "kB": 1e3, "B": 1}
        size = sum(float(v) * unit.get(u, 0) for v, u in re.findall(r"\t([\d.]+)\s*(GB|MB|kB|B)", out))
        recs.add(id="docker", group="review", title="Unused Docker data", sub="stopped containers, unused images, build cache",
                 size=size, match=[], cmd="docker system prune", note="Asks before it removes anything. Add -a to also remove images no container uses.")


def mac_recommendations():
    recs = Recs()
    mole = mole_dry_run()
    lib = f"{H}/Library"
    if mole and mole["total"]:
        recs.add(id="mole", group="safe", title="Caches, logs & Trash", sub="via Mole · browsers, dev tools, apps, system temp",
                 size=mole["total"], breakdown=mole["sections"], cmd="mo clean",
                 match=[f"{lib}/Caches", f"{H}/.npm", f"{H}/.Trash", f"{lib}/Logs"],
                 note="Mole's tested rules. Everything here rebuilds itself as apps need it.")
        dev_caches(recs, skip={"npm", "yarn", "pip", "gobuild"})  # Mole already counts these
    else:
        recs.folder("trash", "safe", "Trash", "~/.Trash", cmd="osascript -e 'tell application \"Finder\" to empty trash'",
                    note="Empties the Trash. Install Mole (`brew install mole`) for a fuller, tested list of safe caches.")
        dev_caches(recs)
        if shutil.which("brew"):
            cache = run(["brew", "--cache"]).strip()
            if cache:
                recs.folder("brew", "safe", "Homebrew downloads", cache, cmd="brew cleanup --prune=all",
                            note="Installers Homebrew already used.")
        recs.folder("caches", "review", "App caches", "~/Library/Caches", cmd="rm -rf ~/Library/Caches/*",
                    note="Apps rebuild these, but quit them first. Mole (`brew install mole`) knows which are safe one by one.")
    recs.folder("derived", "safe", "Xcode build files", "~/Library/Developer/Xcode/DerivedData",
                cmd="rm -rf ~/Library/Developer/Xcode/DerivedData/*", note="Xcode rebuilds these the next time you build.")
    recs.folder("simulators", "review", "iOS simulators", "~/Library/Developer/CoreSimulator/Devices",
                cmd="xcrun simctl delete unavailable", note="Removes simulators for iOS versions Xcode no longer has. Your other simulators stay.")
    recs.folder("devsupport", "review", "Xcode device support", "~/Library/Developer/Xcode/iOS DeviceSupport",
                cmd="rm -rf ~/Library/Developer/Xcode/iOS\\ DeviceSupport/*",
                note="Rebuilt the next time you plug a device into Xcode (takes a few minutes).")
    recs.folder("archives", "review", "Xcode archives", "~/Library/Developer/Xcode/Archives", cmd="open ~/Library/Developer/Xcode/Archives",
                note="Builds you archived for the App Store. Keep the ones you may need to symbolicate crash reports.")
    recs.folder("chromeai", "review", "Chrome on-device AI model", "~/Library/Application Support/Google/Chrome/OptGuideOnDeviceModel",
                cmd="osascript -e 'quit app \"Google Chrome\"'; sleep 2\nrm -rf ~/Library/Application\\ Support/Google/Chrome/OptGuideOnDeviceModel",
                note="Gemini Nano for Chrome's AI features. Chrome downloads it again unless you turn off on-device AI in chrome://settings/system.")
    recs.folder("lmstudio", "review", "LM Studio models", "~/.lmstudio/models", cmd="open -a 'LM Studio'",
                note="Remove models you don't use from LM Studio.")
    common_review(recs)
    recs.folder("messages", "review", "Messages attachments", "~/Library/Messages/Attachments",
                cmd='open "x-apple.systempreferences:com.apple.settings.Storage"',
                note="Don't delete these by hand. Review large attachments in System Settings → General → Storage → Messages.")
    recs.folder("downloads", "review", "Downloads", "~/Downloads", cmd="open ~/Downloads", note="Sort by size and clear what you don't need.")
    return recs


def journal_size():
    out = run(["journalctl", "--disk-usage"], timeout=30)
    m = re.search(r"([\d.]+)([KMGT])", out)
    return float(m.group(1)) * {"K": 1e3, "M": 1e6, "G": 1e9, "T": 1e12}[m.group(2)] if m else 0


def linux_recommendations():
    recs = Recs()
    dev_caches(recs)
    recs.folder("trash", "safe", "Trash", "~/.local/share/Trash", cmd="rm -rf ~/.local/share/Trash/files ~/.local/share/Trash/info",
                note="Files you already deleted from a desktop file manager.")
    if shutil.which("apt-get"):
        recs.add(id="apt", group="safe", title="Downloaded system updates", sub="/var/cache/apt", size=size_of("/var/cache/apt/archives"),
                 match=["/var/cache/apt"], cmd="sudo apt-get clean", note="Installers apt already used. Asks for your password.")
    elif shutil.which("dnf"):
        recs.add(id="dnf", group="safe", title="Downloaded system updates", sub="/var/cache/dnf", size=size_of("/var/cache/dnf"),
                 match=["/var/cache/dnf"], cmd="sudo dnf clean packages", note="Installers dnf already used. Asks for your password.")
    elif shutil.which("pacman"):
        recs.add(id="pacman", group="safe", title="Downloaded system updates", sub="/var/cache/pacman/pkg", size=size_of("/var/cache/pacman/pkg"),
                 match=["/var/cache/pacman"], cmd="sudo pacman -Sc", note="Old package versions pacman keeps. Asks for your password.")
    if shutil.which("journalctl"):
        recs.add(id="journal", group="review", title="System logs", sub="systemd journal · keeps the newest 200 MB",
                 size=max(journal_size() - 200e6, 0), match=["/var/log/journal"], cmd="sudo journalctl --vacuum-size=200M",
                 note="Old system logs. You lose history you might want when debugging something from weeks ago.")
    common_review(recs)
    recs.folder("downloads", "review", "Downloads", "~/Downloads", cmd="ls -lhS ~/Downloads | head -30",
                note="Lists the biggest files so you can decide.")
    return recs


def scan_linux():
    st = os.statvfs("/")
    total, free = st.f_blocks * st.f_frsize, st.f_bavail * st.f_frsize
    used = (st.f_blocks - st.f_bfree) * st.f_frsize  # ext4 reserves ~5% for root: neither used nor free
    parts = [build_tree(H, du(H, 5), name="Home")]
    skip = {"home", "proc", "sys", "dev", "run", "tmp", "mnt", "media", "lost+found", "snap"}
    for d in sorted(os.listdir("/")):
        p = "/" + d
        if d in skip or os.path.islink(p) or not os.path.isdir(p):
            continue
        n = build_tree(p, du(p, 2, timeout=180), name=p)
        if n["size"] > 40 * 2**20:
            parts.append(n)
    scanned = sum(p["size"] for p in parts)
    parts.append({"name": "System & unscanned files", "path": "/…", "size": max(used - scanned, 0), "other": True, "system": True})
    return "System disk", {"total": total, "used": used, "free": free}, parts, linux_recommendations()


def scan_mac():
    st = os.statvfs("/System/Volumes/Data")
    total, free = st.f_blocks * st.f_frsize, st.f_bavail * st.f_frsize
    used = total - free
    parts = [build_tree(H, du(H, 4), name="Home")]
    for path, name in [("/Applications", "Applications"), ("/opt/homebrew", "Homebrew"), ("/usr/local", "Homebrew (Intel)"),
                       ("/Library", "System Library"), ("/private/var/folders", "Temp files")]:
        if os.path.isdir(path) and not os.path.islink(path):
            n = build_tree(path, du(path, 1, timeout=300), name=name)
            if n["size"] > 40 * 2**20:
                parts.append(n)
    scanned = sum(p["size"] for p in parts)
    parts.append({"name": "macOS & system", "path": "/System", "size": max(used - scanned, 0), "other": True, "system": True})
    return "Macintosh HD", {"total": total, "used": used, "free": free}, parts, mac_recommendations()


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--dir", default=os.path.join(H, ".local/share/monitor"), help="where to write data.json (default %(default)s)")
    out = os.path.join(ap.parse_args().dir, "data.json")
    t0 = time.time()
    name, disk, parts, recs = scan_linux() if LINUX else scan_mac()
    parts.sort(key=lambda p: -p["size"])
    data = {
        "scannedAt": time.strftime("%Y-%m-%dT%H:%M:%S"), "scanSeconds": round(time.time() - t0),
        "host": os.uname().nodename.removesuffix(".local"), "os": "linux" if LINUX else "mac",
        "disk": disk, "tree": {"name": name, "path": "/", "size": disk["used"], "children": parts}, "recs": recs,
    }
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out + ".tmp", "w") as f:
        json.dump(data, f)
    os.replace(out + ".tmp", out)
    print(f"wrote {out} in {data['scanSeconds']}s · {len(recs)} recommendations")


if __name__ == "__main__":
    main()
