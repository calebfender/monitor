// Tiny system monitor: samples CPU, memory and per-app usage every 2 s and
// serves them (plus the page itself) over HTTP (default 127.0.0.1:8620).
// One binary per machine; the page talks to every machine's monitor directly.
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
use mac as plat;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as plat;
mod mcp;

const PAGE: &str = include_str!("../index.html");
const FAVICON: &[u8] = include_bytes!("../icons/favicon.png");
const TOUCH_ICON: &[u8] = include_bytes!("../icons/apple-touch-icon.png");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const EVERY: Duration = Duration::from_secs(2);
const RECENT: usize = 900; // 30 min of 2 s samples
const DAY: usize = 1440; // 24 h of 1 min averages

const USAGE: &str = "monitor: live CPU, memory and per-app usage, plus a disk map

Usage: monitor [options]

  --port N           port to listen on (default 8620, or $PORT)
  --bind ADDR        address to listen on (default 127.0.0.1; use 0.0.0.0 for your LAN)
  --name NAME        what to call this machine on the page (default: its hostname)
  --peer NAME=URL    another machine's monitor to show as a tab, e.g.
                     --peer \"Server=https://server.example.ts.net:8620\" (repeatable)
  --allow-origin URL let a page at URL read this monitor (peers are allowed already)
  --dir PATH         where history.tsv and the disk scan (data.json) live
                     (default ~/.local/share/monitor)
  --allow-cleanup    let MCP clients run the disk tab's cleanup commands, after
                     a confirmation step (off by default)
  --version, --help

MCP: every monitor serves an MCP endpoint at /mcp (streamable HTTP). For clients
that only start local commands, `monitor mcp [--url URL]` bridges stdio to it
(default http://127.0.0.1:8620).
";

/// Command-line settings, read once at startup.
struct Config {
    port: u16,
    bind: String,
    name: String,
    peers: Vec<(String, String)>,
    origins: Vec<String>,
    dir: PathBuf,
    allow_cleanup: bool,
}
static CONFIG: OnceLock<Config> = OnceLock::new();
fn cfg() -> &'static Config {
    CONFIG.get().expect("config")
}

fn parse_args() -> Config {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let mut c = Config {
        port: std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8620),
        bind: "127.0.0.1".into(),
        name: String::new(),
        peers: Vec::new(),
        origins: Vec::new(),
        dir: PathBuf::from(home).join(".local/share/monitor"),
        allow_cleanup: false,
    };
    let fail = |m: &str| -> ! {
        eprintln!("monitor: {m}\n\n{USAGE}");
        std::process::exit(2)
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let (k, inline) = match a.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        let mut val = || inline.clone().or_else(|| args.next()).unwrap_or_else(|| fail(&format!("{k} needs a value")));
        match k.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0)
            }
            "-V" | "--version" => {
                println!("monitor {VERSION}");
                std::process::exit(0)
            }
            "--port" => c.port = val().parse().unwrap_or_else(|_| fail("--port must be a number")),
            "--bind" => c.bind = val(),
            "--name" => c.name = val(),
            "--dir" => c.dir = PathBuf::from(val()),
            "--allow-cleanup" => c.allow_cleanup = true,
            "--allow-origin" => c.origins.push(origin_of(&val())),
            "--peer" => {
                let v = val();
                let Some((n, u)) = v.split_once('=') else { fail("--peer takes NAME=URL") };
                let u = u.trim_end_matches('/').to_string();
                if !u.starts_with("http://") && !u.starts_with("https://") {
                    fail("--peer URL must start with http:// or https://");
                }
                c.origins.push(origin_of(&u));
                c.peers.push((n.trim().to_string(), u));
            }
            _ => fail(&format!("unknown option {a}")),
        }
    }
    if c.name.is_empty() {
        c.name = plat::hostname();
    }
    c
}

/// "https://host:8620/some/path" -> "https://host:8620"
fn origin_of(u: &str) -> String {
    let u = u.trim_end_matches('/');
    match u.find("://").and_then(|i| u[i + 3..].find('/').map(|j| i + 3 + j)) {
        Some(end) => u[..end].to_string(),
        None => u.to_string(),
    }
}

pub struct Mem {
    pub total: u64,
    pub a: u64, // apps
    pub b: u64, // system / kernel
    pub c: u64, // compressed (mac) / shared (linux)
    pub cached: u64,
    pub used: u64,
    pub swap: u64,
    pub swap_total: u64,
    pub pressure: u8, // 0 normal, 1 warn, 2 critical
}

pub struct Proc {
    pub key: u64,
    pub cpu_ns: u64,
    pub mem: u64,
    pub name: u16,
}

/// Interned app names, so history stores 2-byte ids instead of strings.
#[derive(Default)]
pub struct Names {
    list: Vec<String>,
    map: HashMap<String, u16>,
}
impl Names {
    pub fn id(&mut self, s: &str) -> u16 {
        if let Some(&i) = self.map.get(s) {
            return i;
        }
        let i = self.list.len().min(u16::MAX as usize - 1) as u16;
        if (i as usize) == self.list.len() {
            self.list.push(s.to_string());
            self.map.insert(s.to_string(), i);
        }
        i
    }
    fn get(&self, i: u16) -> &str {
        self.list.get(i as usize).map(|s| s.as_str()).unwrap_or("")
    }
}

#[derive(Clone, Copy)]
struct Point {
    t: u64,
    cpu: f32,
    a: u32, // MB
    b: u32,
    c: u32,
    swap: u32,
    pr: u8,
    top: [(u16, f32); 3],
}
const NONE: u16 = u16::MAX;

struct App {
    name: u16,
    cpu: f64,
    mem: u64,
    n: u32,
}

struct Now {
    t: u64,
    cpu: f64,
    load: f64,
    mem: Mem,
    top_cpu: Vec<App>,
    top_mem: Vec<App>,
    own: u64,
}

pub struct State {
    names: Names,
    now: Option<Now>,
    recent: VecDeque<Point>,
    day: VecDeque<Point>,
}

fn ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
const MB: u64 = 1 << 20;

fn sampler(st: Arc<Mutex<State>>) {
    let mut p = plat::Plat::new();
    let ncpu = plat::ncpu() as f64;
    let mut procs = Vec::with_capacity(2048);
    let mut last_ticks = p.cpu();
    let mut last: HashMap<u64, u64> = HashMap::new();
    let mut last_at = Instant::now();
    {
        let mut s = st.lock().unwrap();
        p.procs(&mut procs, &mut s.names);
    }
    for pr in &procs {
        last.insert(pr.key, pr.cpu_ns);
    }
    let mut minute: Vec<Point> = Vec::with_capacity(30);
    let mut apps: HashMap<u16, App> = HashMap::new();
    loop {
        std::thread::sleep(EVERY.saturating_sub(last_at.elapsed()));
        let at = Instant::now();
        let mut s = st.lock().unwrap();
        let State { names, .. } = &mut *s;
        p.procs(&mut procs, names);
        let sys_name = names.id(plat::SYSTEM_NAME);
        drop(s);
        let dt = at.duration_since(last_at).as_nanos() as f64;
        last_at = at;
        let ticks = p.cpu();
        let (db, dtot) = (ticks.0.saturating_sub(last_ticks.0), ticks.1.saturating_sub(last_ticks.1));
        last_ticks = ticks;
        let cpu = if dtot > 0 { db as f64 / dtot as f64 * 100.0 } else { 0.0 };

        apps.clear();
        let mut next = HashMap::with_capacity(procs.len());
        for pr in &procs {
            let a = apps.entry(pr.name).or_insert(App { name: pr.name, cpu: 0.0, mem: 0, n: 0 });
            a.mem += pr.mem;
            a.n += 1;
            if let Some(&prev) = last.get(&pr.key) {
                a.cpu += pr.cpu_ns.saturating_sub(prev) as f64 / dt / ncpu * 100.0;
            }
            next.insert(pr.key, pr.cpu_ns);
        }
        last = next;
        let mut v: Vec<App> = apps.drain().map(|(_, a)| a).collect();
        // CPU no visible process accounts for (kernel, other users' processes)
        let seen: f64 = v.iter().map(|a| a.cpu).sum();
        let rest = cpu - seen;
        if rest > 2.0 {
            if let Some(a) = v.iter_mut().find(|a| a.name == sys_name) {
                a.cpu += rest;
            } else {
                v.push(App { name: sys_name, cpu: rest, mem: 0, n: 0 });
            }
        }
        v.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        let top_cpu: Vec<App> = v.iter().take(10).map(|a| App { ..*a }).collect();
        v.sort_by(|a, b| b.mem.cmp(&a.mem));
        v.truncate(10);
        let top_mem = v;

        let mem = p.mem();
        let t = ms();
        let mut top = [(NONE, 0f32); 3];
        for (i, a) in top_cpu.iter().take(3).enumerate() {
            top[i] = (a.name, a.cpu as f32);
        }
        let pt = Point {
            t, cpu: cpu as f32, a: (mem.a / MB) as u32, b: (mem.b / MB) as u32, c: (mem.c / MB) as u32,
            swap: (mem.swap / MB) as u32, pr: mem.pressure, top,
        };
        let mut s = st.lock().unwrap();
        s.now = Some(Now { t, cpu, load: plat::load(), mem, top_cpu, top_mem, own: plat::own_mem() });
        if s.recent.len() >= RECENT {
            s.recent.pop_front();
        }
        s.recent.push_back(pt);
        minute.push(pt);
        if minute.len() >= 30 {
            let m = roll(&minute);
            minute.clear();
            if s.day.len() >= DAY {
                s.day.pop_front();
            }
            s.day.push_back(m);
            let _ = save(&s);
        }
    }
}

fn roll(ps: &[Point]) -> Point {
    let n = ps.len() as f64;
    let avg = |f: &dyn Fn(&Point) -> f64| ps.iter().map(f).sum::<f64>() / n;
    let mut tot: HashMap<u16, f64> = HashMap::new();
    for p in ps {
        for &(id, v) in &p.top {
            if id != NONE {
                *tot.entry(id).or_default() += v as f64 / n;
            }
        }
    }
    let mut tv: Vec<_> = tot.into_iter().collect();
    tv.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut top = [(NONE, 0f32); 3];
    for (i, (id, v)) in tv.into_iter().take(3).enumerate() {
        top[i] = (id, v as f32);
    }
    Point {
        t: ps[ps.len() - 1].t,
        cpu: avg(&|p| p.cpu as f64) as f32,
        a: avg(&|p| p.a as f64) as u32,
        b: avg(&|p| p.b as f64) as u32,
        c: avg(&|p| p.c as f64) as u32,
        swap: avg(&|p| p.swap as f64) as u32,
        pr: ps.iter().map(|p| p.pr).max().unwrap_or(0),
        top,
    }
}

// history.tsv: t cpu a b c swap pr (name value)*
fn save(s: &State) -> std::io::Result<()> {
    let mut out = String::with_capacity(s.day.len() * 80);
    for p in &s.day {
        let _ = write!(out, "{}\t{:.1}\t{}\t{}\t{}\t{}\t{}", p.t, p.cpu, p.a, p.b, p.c, p.swap, p.pr);
        for &(id, v) in &p.top {
            if id != NONE {
                let _ = write!(out, "\t{}\t{:.1}", s.names.get(id).replace(['\t', '\n'], " "), v);
            }
        }
        out.push('\n');
    }
    let dir = &cfg().dir;
    std::fs::write(dir.join("history.tsv.tmp"), out)?;
    std::fs::rename(dir.join("history.tsv.tmp"), dir.join("history.tsv"))
}

fn load(s: &mut State) {
    let Ok(text) = std::fs::read_to_string(cfg().dir.join("history.tsv")) else { return };
    let cutoff = ms().saturating_sub(24 * 3600 * 1000);
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 7 {
            continue;
        }
        let n = |i: usize| f[i].parse::<f64>().unwrap_or(0.0);
        let mut top = [(NONE, 0f32); 3];
        for (i, pair) in f[7..].chunks(2).take(3).enumerate() {
            if pair.len() == 2 {
                top[i] = (s.names.id(pair[0]), pair[1].parse().unwrap_or(0.0));
            }
        }
        let p = Point { t: n(0) as u64, cpu: n(1) as f32, a: n(2) as u32, b: n(3) as u32, c: n(4) as u32, swap: n(5) as u32, pr: n(6) as u8, top };
        if p.t > cutoff {
            s.day.push_back(p);
        }
    }
    while s.day.len() > DAY {
        s.day.pop_front();
    }
}

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => { let _ = write!(o, "\\u{:04x}", c as u32); }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn points_json(o: &mut String, names: &Names, ps: impl Iterator<Item = Point>) {
    o.push('[');
    let mut first = true;
    for p in ps {
        if !first { o.push(','); }
        first = false;
        let _ = write!(o, "[{},{:.1},{},{},{},{},{},[", p.t, p.cpu, p.a, p.b, p.c, p.swap, p.pr);
        let mut f2 = true;
        for &(id, v) in &p.top {
            if id == NONE { continue; }
            if !f2 { o.push(','); }
            f2 = false;
            let _ = write!(o, "[{},{:.1}]", esc(names.get(id)), v);
        }
        o.push_str("]]");
    }
    o.push(']');
}

fn apps_json(o: &mut String, names: &Names, v: &[App]) {
    o.push('[');
    for (i, a) in v.iter().enumerate() {
        if i > 0 { o.push(','); }
        let _ = write!(o, "{{\"name\":{},\"cpu\":{:.2},\"mem\":{},\"n\":{}}}", esc(names.get(a.name)), a.cpu, a.mem, a.n);
    }
    o.push(']');
}

fn live(s: &State, since: u64) -> String {
    let mut o = String::with_capacity(4096);
    o.push_str("{\"now\":");
    match &s.now {
        None => o.push_str("null"),
        Some(n) => {
            let m = &n.mem;
            let pr = ["normal", "warn", "critical"][m.pressure.min(2) as usize];
            let _ = write!(o, "{{\"t\":{},\"cpu\":{:.2},\"load\":{:.2},\"cores\":{},\"own\":{},\"mem\":{{\"total\":{},\"a\":{},\"b\":{},\"c\":{},\"cached\":{},\"used\":{},\"swap\":{},\"swapTotal\":{},\"pressure\":\"{}\"}},\"topCpu\":",
                n.t, n.cpu, n.load, plat::ncpu(), n.own, m.total, m.a, m.b, m.c, m.cached, m.used, m.swap, m.swap_total, pr);
            apps_json(&mut o, &s.names, &n.top_cpu);
            o.push_str(",\"topMem\":");
            apps_json(&mut o, &s.names, &n.top_mem);
            o.push('}');
        }
    }
    o.push_str(",\"points\":");
    points_json(&mut o, &s.names, s.recent.iter().copied().filter(|p| p.t > since));
    o.push('}');
    o
}

/// The JSON API, shared by HTTP and the MCP tools. None means not found.
pub fn api(path: &str, since: u64, st: &Mutex<State>) -> Option<Vec<u8>> {
    Some(match path {
        "/api/live" => live(&st.lock().unwrap(), since).into_bytes(),
        "/api/day" => {
            let s = st.lock().unwrap();
            let mut o = String::from("{\"points\":");
            points_json(&mut o, &s.names, s.day.iter().copied());
            o.push('}');
            o.into_bytes()
        }
        "/api/info" => {
            let mut o = plat::info();
            let disk = cfg().dir.join("data.json").exists();
            let _ = write!(o, ",\"diskMap\":{},\"scanning\":{},\"own\":{}}}", disk, mcp::scanning(), plat::own_mem());
            o.into_bytes()
        }
        "/api/config" => {
            let c = cfg();
            let mut o = format!("{{\"version\":{},\"name\":{},\"allowCleanup\":{},\"peers\":[", esc(VERSION), esc(&c.name), c.allow_cleanup);
            for (i, (n, u)) in c.peers.iter().enumerate() {
                if i > 0 { o.push(','); }
                let _ = write!(o, "{{\"name\":{},\"url\":{}}}", esc(n), esc(u));
            }
            o.push_str("]}");
            o.into_bytes()
        }
        "/data.json" => std::fs::read(cfg().dir.join("data.json")).ok()?,
        _ => return None,
    })
}

fn respond(mut c: TcpStream, st: &Mutex<State>) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = c.set_write_timeout(Some(Duration::from_secs(10)));
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") { break i + 4; }
        if buf.len() > 16 << 10 { return; }
        match c.read(&mut chunk) {
            Ok(0) | Err(_) => break buf.len(),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let req = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let header = |name: &str| req.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
    });
    // request body (only /mcp uses one)
    let want = header("content-length").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).min(1 << 20);
    let mut body_in = buf[head_end.min(buf.len())..].to_vec();
    while body_in.len() < want {
        match c.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => body_in.extend_from_slice(&chunk[..n]),
        }
    }
    let method = req.split_whitespace().next().unwrap_or("GET");
    let target = req.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let origin = header("origin");
    let allowed = |o: &str| cfg().origins.iter().any(|a| a == o || a == "*");
    let param = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(k)?.strip_prefix('=')).unwrap_or("");

    let json = "application/json";
    let (status, ctype, body): (&str, &str, Vec<u8>) = match path {
        "/" | "/index.html" => ("200 OK", "text/html; charset=utf-8", PAGE.as_bytes().to_vec()),
        "/favicon.png" => ("200 OK", "image/png", FAVICON.to_vec()),
        "/apple-touch-icon.png" => ("200 OK", "image/png", TOUCH_ICON.to_vec()),
        // MCP streamable HTTP: JSON responses only, no sessions. Browsers from other sites can't call it.
        "/mcp" if origin.as_deref().is_some_and(|o| !allowed(o)) => ("403 Forbidden", "text/plain", b"origin not allowed".to_vec()),
        "/mcp" if method == "POST" => match mcp::handle(&String::from_utf8_lossy(&body_in[..want.min(body_in.len())]), st) {
            Some(r) => ("200 OK", json, r.into_bytes()),
            None => ("202 Accepted", json, Vec::new()),
        },
        "/mcp" => ("405 Method Not Allowed", "text/plain", b"POST JSON-RPC to /mcp".to_vec()),
        _ => match api(path, param("since").parse().unwrap_or(0), st) {
            Some(b) => ("200 OK", json, b),
            None => ("404 Not Found", "text/plain", b"not found".to_vec()),
        },
    };
    let mut head = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n", body.len());
    if path == "/mcp" && status.starts_with("405") {
        head.push_str("Allow: POST\r\n");
    }
    if let Some(o) = origin.filter(|o| allowed(o)) {
        let _ = write!(head, "Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\n");
    }
    head.push_str("\r\n");
    let _ = c.write_all(head.as_bytes());
    let _ = c.write_all(&body);
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("mcp") {
        mcp::stdio(std::env::args().skip(2).collect());
    }
    let c = parse_args();
    if let Err(e) = std::fs::create_dir_all(&c.dir) {
        eprintln!("monitor: can't create {}: {e}", c.dir.display());
        std::process::exit(1);
    }
    let addr = (c.bind.clone(), c.port);
    let _ = CONFIG.set(c);
    let mut s = State { names: Names::default(), now: None, recent: VecDeque::with_capacity(RECENT), day: VecDeque::with_capacity(DAY) };
    load(&mut s);
    let st = Arc::new(Mutex::new(s));
    let st2 = st.clone();
    std::thread::Builder::new().stack_size(256 << 10).spawn(move || sampler(st2)).unwrap();
    let l = TcpListener::bind((addr.0.as_str(), addr.1)).unwrap_or_else(|e| {
        eprintln!("monitor: can't listen on {}:{}: {e}", addr.0, addr.1);
        std::process::exit(1)
    });
    eprintln!("monitor {VERSION} on http://{}:{} ({})", addr.0, addr.1, cfg().name);
    for c in l.incoming().flatten() {
        let st = st.clone();
        // one short-lived thread per request, so a slow MCP call doesn't hold up the page
        let _ = std::thread::Builder::new().stack_size(512 << 10).spawn(move || respond(c, &st));
    }
}
