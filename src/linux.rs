// Linux: /proc. Processes are grouped by their systemd service (cgroup), so a
// server shows "nginx", "postgresql", "my-app" etc. instead of a pile of "node".
use crate::{esc, Mem, Names, Proc};
use std::collections::HashMap;
use std::ffi::CString;
use std::fmt::Write as _;
use std::fs;
use std::mem::zeroed;

pub const SYSTEM_NAME: &str = "Linux kernel";

pub struct Plat {
    tck: u64,
    page: u64,
    names: HashMap<u64, u16>,
    buf: String,
}

pub fn hostname() -> String {
    first_line("/proc/sys/kernel/hostname")
}

pub fn ncpu() -> u32 {
    unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as u32
}
pub fn load() -> f64 {
    fs::read_to_string("/proc/loadavg").ok().and_then(|s| s.split_whitespace().next()?.parse().ok()).unwrap_or(0.0)
}
pub fn own_mem() -> u64 {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    fs::read_to_string("/proc/self/statm").ok().and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok()).unwrap_or(0) * page
}

fn read_into(path: &str, buf: &mut String) -> bool {
    use std::io::Read;
    buf.clear();
    fs::File::open(path).and_then(|mut f| f.read_to_string(buf)).is_ok()
}

/// Service name from /proc/<pid>/cgroup, using the deepest named unit:
/// ".../app.slice/my-api.service" -> "my-api",
/// ".../app-editor-2ffd8e9f.slice/editor-worker-cd2800c9.scope" -> "editor-worker".
fn service(cg: &str) -> Option<String> {
    let path = cg.lines().find_map(|l| l.strip_prefix("0::"))?;
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let rest: &[&str] = if let Some(i) = segs.iter().position(|s| s.starts_with("user@")) {
        &segs[i + 1..]
    } else if segs.first() == Some(&"system.slice") {
        &segs[1..]
    } else {
        return None;
    };
    let last = *rest.last()?;
    if last.starts_with("session-") || last.starts_with("init.scope") || last.starts_with("run-") {
        return None; // ssh sessions and systemd-run units: the command name says more
    }
    if last.starts_with("docker-") {
        return Some("docker".into());
    }
    let mut s = last.trim_end_matches(".service").trim_end_matches(".scope").trim_end_matches(".slice");
    s = s.strip_prefix("app-").unwrap_or(s);
    s = s.split('@').next().unwrap_or(s);
    // drop instance ids: "-cd2800c9", "-1241084"
    if let Some((head, tail)) = s.rsplit_once('-') {
        if tail.len() >= 6 && tail.chars().all(|c| c.is_ascii_hexdigit()) {
            s = head;
        }
    }
    // reverse-DNS app ids: "org.chromium.Chromium" -> "Chromium"
    if s.matches('.').count() >= 2 {
        s = s.rsplit('.').next().unwrap_or(s);
    }
    (!s.is_empty()).then(|| s.to_string())
}

impl Plat {
    pub fn new() -> Plat {
        Plat {
            tck: unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64,
            page: unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64,
            names: HashMap::new(),
            buf: String::with_capacity(1024),
        }
    }

    pub fn cpu(&mut self) -> (u64, u64) {
        if !read_into("/proc/stat", &mut self.buf) {
            return (0, 0);
        }
        let v: Vec<u64> = self.buf.lines().next().unwrap_or("").split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        let total: u64 = v.iter().take(8).sum();
        let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
        (total - idle, total)
    }

    pub fn mem(&mut self) -> Mem {
        let mut m: HashMap<&str, u64> = HashMap::new();
        let text = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        for l in text.lines() {
            if let Some((k, v)) = l.split_once(':') {
                m.insert(k, v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0) * 1024);
            }
        }
        let g = |k: &str| m.get(k).copied().unwrap_or(0);
        let total = g("MemTotal");
        let psi = fs::read_to_string("/proc/pressure/memory").unwrap_or_default();
        let avg10 = |kind: &str| psi.lines().find(|l| l.starts_with(kind)).and_then(|l| l.split_whitespace().find_map(|f| f.strip_prefix("avg10=")?.parse::<f64>().ok())).unwrap_or(0.0);
        let (some, full) = (avg10("some"), avg10("full"));
        Mem {
            total,
            a: g("AnonPages"),
            b: g("SUnreclaim") + g("KernelStack") + g("PageTables"),
            c: g("Shmem"),
            cached: g("Cached") + g("Buffers") + g("SReclaimable"),
            used: total.saturating_sub(g("MemAvailable")),
            swap: g("SwapTotal").saturating_sub(g("SwapFree")),
            swap_total: g("SwapTotal"),
            pressure: if full >= 10.0 || some >= 40.0 { 2 } else if some >= 10.0 { 1 } else { 0 },
        }
    }

    pub fn procs(&mut self, out: &mut Vec<Proc>, names: &mut Names) {
        out.clear();
        let me = std::process::id();
        let mut seen = HashMap::with_capacity(self.names.len() + 64);
        let Ok(rd) = fs::read_dir("/proc") else { return };
        let mut cg = String::new();
        for e in rd.flatten() {
            let fname = e.file_name();
            let Some(pid) = fname.to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            if !read_into(&format!("/proc/{pid}/stat"), &mut self.buf) {
                continue;
            }
            let (Some(l), Some(r)) = (self.buf.find('('), self.buf.rfind(')')) else { continue };
            let comm = &self.buf[l + 1..r];
            let f: Vec<&str> = self.buf[r + 1..].split_whitespace().collect();
            if f.len() < 22 {
                continue;
            }
            let num = |i: usize| f[i].parse::<u64>().unwrap_or(0);
            let (ppid, start) = (num(1), num(19));
            let key = ((pid as u64) << 40) ^ start;
            let name = match self.names.get(&key) {
                Some(&id) => id,
                None => {
                    let label = if pid == me {
                        "This monitor".to_string()
                    } else if ppid == 2 || pid == 2 {
                        SYSTEM_NAME.to_string()
                    } else {
                        cg.clear();
                        let svc = if read_into(&format!("/proc/{pid}/cgroup"), &mut cg) { service(&cg) } else { None };
                        svc.unwrap_or_else(|| comm.to_string())
                    };
                    names.id(&label)
                }
            };
            seen.insert(key, name);
            out.push(Proc { key, cpu_ns: (num(11) + num(12)) * 1_000_000_000 / self.tck, mem: num(21) * self.page, name });
        }
        self.names = seen;
    }
}

fn first_line(path: &str) -> String {
    fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

pub fn info() -> String {
    let os = fs::read_to_string("/etc/os-release").unwrap_or_default().lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string())).unwrap_or_else(|| "Linux".into());
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let chip = cpuinfo.lines().find_map(|l| l.strip_prefix("model name")?.split_once(':').map(|(_, v)| v.trim().to_string())).unwrap_or_default();
    let vendor = first_line("/sys/devices/virtual/dmi/id/sys_vendor");
    let vendor = if vendor.starts_with("Micro-Star") { "MSI".to_string() } else { vendor.trim_end_matches(" Inc.").trim_end_matches(", Ltd.").to_string() };
    let product = first_line("/sys/devices/virtual/dmi/id/product_name");
    let uptime: f64 = fs::read_to_string("/proc/uptime").ok().and_then(|s| s.split_whitespace().next()?.parse().ok()).unwrap_or(0.0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let ram = fs::read_to_string("/proc/meminfo").ok().and_then(|s| s.lines().next()?.split_whitespace().nth(1)?.parse::<u64>().ok()).unwrap_or(0) * 1024;
    let mut o = String::from("{\"kind\":\"linux\",\"os\":");
    o.push_str(&esc(&os));
    let _ = write!(o, ",\"host\":{},\"model\":{},\"chip\":{},\"cores\":{},\"perf\":0,\"eff\":0,\"ram\":{},\"boot\":{},\"layers\":[\"Apps\",\"Kernel\",\"Shared\"],\"disks\":[",
        esc(&first_line("/proc/sys/kernel/hostname")), esc(format!("{vendor} {product}").trim()), esc(&chip), ncpu(), ram, now.saturating_sub(uptime as u64));
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mut first = true;
    let mut devs = Vec::new();
    for l in mounts.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 3 || !f[0].starts_with("/dev/") || !matches!(f[2], "ext4" | "xfs" | "btrfs" | "zfs" | "f2fs") || f[1].starts_with("/boot") || f[1].starts_with("/snap") || devs.contains(&f[0]) {
            continue;
        }
        devs.push(f[0]);
        let Ok(c) = CString::new(f[1]) else { continue };
        let mut s: libc::statvfs = unsafe { zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 { continue; }
        if !first { o.push(','); }
        first = false;
        let name = if f[1] == "/" { "System" } else { f[1] };
        let _ = write!(o, "{{\"name\":{},\"total\":{},\"free\":{}}}", esc(name), s.f_blocks as u64 * s.f_frsize as u64, s.f_bavail as u64 * s.f_frsize as u64);
    }
    o.push(']');
    o
}
