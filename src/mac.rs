#![allow(deprecated)] // libc points at the mach2 crate; these calls are stable
// macOS: Mach host statistics, libproc per-process usage, sysctl.
// Runs unprivileged, so processes owned by root or other users (WindowServer,
// the kernel) can't be read; their CPU shows up as "macOS system".
use crate::{esc, Mem, Names, Proc};
use std::collections::HashMap;
use std::ffi::CString;
use std::fmt::Write as _;
use std::mem::{size_of, zeroed};

pub const SYSTEM_NAME: &str = "macOS system";
/// The volume that holds the user's files.
pub const DATA_VOLUME: &str = "/System/Volumes/Data";

pub struct Plat {
    host: libc::mach_port_t,
    numer: u64,
    denom: u64,
    pids: Vec<i32>,
    names: HashMap<u64, u16>,
    page: u64,
}

fn sysctl<T>(name: &str) -> Option<T> {
    let c = CString::new(name).ok()?;
    let mut v: T = unsafe { zeroed() };
    let mut len = size_of::<T>();
    let r = unsafe { libc::sysctlbyname(c.as_ptr(), &mut v as *mut T as *mut _, &mut len, std::ptr::null_mut(), 0) };
    (r == 0).then_some(v)
}
fn sysctl_str(name: &str) -> String {
    let Ok(c) = CString::new(name) else { return String::new() };
    let mut len = 0usize;
    unsafe {
        if libc::sysctlbyname(c.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) != 0 || len == 0 {
            return String::new();
        }
        let mut buf = vec![0u8; len];
        if libc::sysctlbyname(c.as_ptr(), buf.as_mut_ptr() as *mut _, &mut len, std::ptr::null_mut(), 0) != 0 {
            return String::new();
        }
        String::from_utf8_lossy(&buf[..len]).trim_end_matches('\0').to_string()
    }
}

pub fn hostname() -> String {
    let h = sysctl_str("kern.hostname");
    h.strip_suffix(".local").unwrap_or(&h).to_string()
}

pub fn ncpu() -> u32 {
    sysctl::<i32>("hw.ncpu").unwrap_or(1) as u32
}
pub fn load() -> f64 {
    let mut l = [0f64; 3];
    unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    l[0]
}

fn rusage(pid: i32) -> Option<libc::rusage_info_v2> {
    let mut ri: libc::rusage_info_v2 = unsafe { zeroed() };
    let r = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, &mut ri as *mut _ as *mut libc::rusage_info_t) };
    (r == 0).then_some(ri)
}
pub fn own_mem() -> u64 {
    rusage(std::process::id() as i32).map(|r| r.ri_phys_footprint).unwrap_or(0)
}

fn app_name(path: &str) -> &str {
    if path.contains("/CoreSimulator") || path.contains("/Simulator.app/") {
        return "iOS Simulator";
    }
    if let Some(i) = path.find(".app/") {
        let start = path[..i].rfind('/').map(|j| j + 1).unwrap_or(0);
        return &path[start..i];
    }
    path.rsplit('/').next().unwrap_or(path)
}

impl Plat {
    pub fn new() -> Plat {
        let mut tb = libc::mach_timebase_info { numer: 1, denom: 1 };
        unsafe { libc::mach_timebase_info(&mut tb) };
        Plat {
            host: unsafe { libc::mach_host_self() },
            numer: tb.numer as u64,
            denom: tb.denom.max(1) as u64,
            pids: Vec::new(),
            names: HashMap::new(),
            page: unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64,
        }
    }

    /// (busy ticks, total ticks) across all cores
    pub fn cpu(&mut self) -> (u64, u64) {
        let mut info: libc::host_cpu_load_info = unsafe { zeroed() };
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        unsafe { libc::host_statistics(self.host, libc::HOST_CPU_LOAD_INFO, &mut info as *mut _ as libc::host_info_t, &mut count) };
        let t = info.cpu_ticks;
        let busy = t[libc::CPU_STATE_USER as usize] as u64 + t[libc::CPU_STATE_SYSTEM as usize] as u64 + t[libc::CPU_STATE_NICE as usize] as u64;
        (busy, busy + t[libc::CPU_STATE_IDLE as usize] as u64)
    }

    /// Activity Monitor's buckets: App = internal - purgeable, Wired, Compressed.
    pub fn mem(&mut self) -> Mem {
        let mut vm: libc::vm_statistics64 = unsafe { zeroed() };
        let mut count = libc::HOST_VM_INFO64_COUNT;
        unsafe { libc::host_statistics64(self.host, libc::HOST_VM_INFO64, &mut vm as *mut _ as libc::host_info64_t, &mut count) };
        let pg = self.page;
        let a = (vm.internal_page_count as u64).saturating_sub(vm.purgeable_count as u64) * pg;
        let b = vm.wire_count as u64 * pg;
        let c = vm.compressor_page_count as u64 * pg;
        let swap: libc::xsw_usage = sysctl("vm.swapusage").unwrap_or(unsafe { zeroed() });
        let level = sysctl::<i32>("kern.memorystatus_vm_pressure_level").unwrap_or(1);
        Mem {
            total: sysctl::<u64>("hw.memsize").unwrap_or(0),
            a, b, c,
            cached: (vm.external_page_count as u64 + vm.purgeable_count as u64) * pg,
            used: a + b + c,
            swap: swap.xsu_used,
            swap_total: swap.xsu_total,
            pressure: match level { 4 => 2, 2 => 1, _ => 0 },
        }
    }

    pub fn procs(&mut self, out: &mut Vec<Proc>, names: &mut Names) {
        out.clear();
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return;
        }
        self.pids.resize(n as usize + 64, 0);
        let bytes = (self.pids.len() * size_of::<i32>()) as i32;
        let n = unsafe { libc::proc_listallpids(self.pids.as_mut_ptr() as *mut _, bytes) }.max(0) as usize;
        let me = std::process::id() as i32;
        let mut seen = HashMap::with_capacity(n);
        let mut path = [0u8; 4096];
        for &pid in &self.pids[..n.min(self.pids.len())] {
            if pid <= 0 {
                continue;
            }
            let Some(ri) = rusage(pid) else { continue };
            let key = ((pid as u64) << 40) ^ ri.ri_proc_start_abstime;
            let name = match self.names.get(&key) {
                Some(&id) => id,
                None => {
                    let len = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr() as *mut _, path.len() as u32) };
                    let p = if len > 0 { String::from_utf8_lossy(&path[..len as usize]).into_owned() } else { String::new() };
                    let label = if pid == me { "This monitor" } else if p.is_empty() { "unknown" } else { app_name(&p) };
                    names.id(label)
                }
            };
            seen.insert(key, name);
            let cpu = (ri.ri_user_time + ri.ri_system_time) * self.numer / self.denom;
            out.push(Proc { key, cpu_ns: cpu, mem: ri.ri_phys_footprint, name });
        }
        self.names = seen;
    }
}

pub fn info() -> String {
    let mut o = String::from("{\"os\":");
    o.push_str(&esc(&format!("macOS {}", sysctl_str("kern.osproductversion"))));
    let boot: libc::timeval = sysctl("kern.boottime").unwrap_or(unsafe { zeroed() });
    let _ = write!(o, ",\"kind\":\"mac\",\"host\":{},\"model\":{},\"chip\":{},\"cores\":{},\"perf\":{},\"eff\":{},\"ram\":{},\"boot\":{},\"layers\":[\"Apps\",\"System\",\"Compressed\"],\"disks\":[",
        esc(&sysctl_str("kern.hostname")), esc(&sysctl_str("hw.model")), esc(&sysctl_str("machdep.cpu.brand_string")),
        ncpu(), sysctl::<i32>("hw.perflevel0.logicalcpu").unwrap_or(0), sysctl::<i32>("hw.perflevel1.logicalcpu").unwrap_or(0),
        sysctl::<u64>("hw.memsize").unwrap_or(0), boot.tv_sec);
    // Sizes come from the mount table (getmntinfo with MNT_NOWAIT), so nothing on any
    // drive is touched: no privacy prompts for removable or network volumes.
    let mut list: *mut libc::statfs = std::ptr::null_mut();
    let n = unsafe { libc::getmntinfo(&mut list, libc::MNT_NOWAIT) }.max(0) as usize;
    let mounts = if list.is_null() { &[][..] } else { unsafe { std::slice::from_raw_parts(list, n) } };
    let cstr = |b: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(b.as_ptr()) }.to_string_lossy().into_owned();
    let mut seen = Vec::new();
    let mut first = true;
    // the data volume first, then other drives under /Volumes
    let mut order: Vec<&libc::statfs> = mounts.iter().filter(|m| cstr(&m.f_mntonname) == DATA_VOLUME).collect();
    order.extend(mounts.iter().filter(|m| cstr(&m.f_mntonname).starts_with("/Volumes/") && m.f_flags & libc::MNT_DONTBROWSE as u32 == 0));
    for m in order {
        let total = m.f_blocks * m.f_bsize as u64;
        let free = m.f_bavail * m.f_bsize as u64;
        // skip tiny volumes, and other volumes of the boot container (they report its numbers)
        if total < 1 << 30 || seen.contains(&(total, free)) { continue; }
        seen.push((total, free));
        let path = cstr(&m.f_mntonname);
        let name = if path == DATA_VOLUME { "Macintosh HD".to_string() } else { path.trim_start_matches("/Volumes/").to_string() };
        if !first { o.push(','); }
        first = false;
        let _ = write!(o, "{{\"name\":{},\"total\":{},\"free\":{}}}", esc(&name), total, free);
    }
    o.push(']');
    o
}

pub fn statvfs(path: &str) -> Option<(u64, u64)> {
    let c = CString::new(path).ok()?;
    let mut s: libc::statvfs = unsafe { zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0)
        .then(|| (s.f_blocks as u64 * s.f_frsize as u64, s.f_bavail as u64 * s.f_frsize as u64))
}
