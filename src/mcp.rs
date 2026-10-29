// MCP server: lets agents ask how this machine and its peers are doing.
// Served at POST /mcp (streamable HTTP, JSON responses, no sessions), and over
// stdio by `monitor mcp`, which forwards each line to that endpoint.
//
// Tools read the same JSON API the page uses: this machine's directly, peers'
// over HTTP with curl. Actions (scan_disk, run_cleanup) on a peer are forwarded
// to that peer's own /mcp, so each machine decides what it allows.
use crate::{api, cfg, plat, State, VERSION};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROTOCOLS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const GIB: f64 = (1u64 << 30) as f64;
const MIB: f64 = (1u64 << 20) as f64;

static SCANNING: AtomicBool = AtomicBool::new(false);
static LAST_SCAN: Mutex<String> = Mutex::new(String::new());
/// One-time cleanup confirmations: (code, ids, the exact commands shown, expiry)
static PENDING: Mutex<Vec<(String, Vec<String>, Vec<String>, Instant)>> = Mutex::new(Vec::new());

/// A random 6-digit code from the OS.
fn random_code() -> String {
    let mut b = [0u8; 4];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    format!("{:06}", u32::from_le_bytes(b) % 1_000_000)
}

pub fn scanning() -> bool {
    SCANNING.load(Ordering::Relaxed)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// ---------------------------------------------------------------- JSON-RPC

/// Handle one JSON-RPC message. None for notifications (HTTP 202, no body).
pub fn handle(body: &str, st: &Mutex<State>) -> Option<String> {
    let msg: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
    };
    if msg.is_array() {
        return Some(error(Value::Null, -32600, "batches aren't supported; send one message per request"));
    }
    let Some(method) = msg.get("method").and_then(|m| m.as_str()).filter(|_| msg["jsonrpc"] == "2.0") else {
        return Some(error(msg.get("id").cloned().unwrap_or(Value::Null), -32600, "invalid request"));
    };
    let id = msg.get("id").cloned()?; // notifications (initialized, cancelled) need no answer
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let v = PROTOCOLS.iter().find(|p| **p == asked).unwrap_or(&PROTOCOLS[0]);
            json!({
                "protocolVersion": v,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "monitor", "title": "Monitor", "version": VERSION },
                "instructions": instructions(),
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call(name, &args, st) {
                Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
                Err(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": true }),
            }
        }
        m => return Some(error(id, -32601, &format!("unknown method {m}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string())
}

fn error(id: Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

fn instructions() -> String {
    let c = cfg();
    let mut names = vec![c.name.clone()];
    names.extend(c.peers.iter().map(|p| p.0.clone()));
    format!(
        "Monitor reports live health, recent history and disk usage for these machines: {}. \
         Start with `status` for a quick read on all of them. Use `history` to explain slowness \
         that already happened, `disk` for what is filling a drive and what can be cleaned up. \
         Cleanup commands are only suggestions: show them to the user before anything runs.",
        names.join(", ")
    )
}

fn tools() -> Value {
    let machine = json!({ "type": "string", "description": "Machine name (see `machines`). Defaults to this machine." });
    let ro = json!({ "readOnlyHint": true, "openWorldHint": false });
    json!([
        {
            "name": "status",
            "title": "Machine status",
            "description": "How each machine is doing right now: a plain-English verdict (e.g. 'Memory is tight'), CPU, memory, swap, memory pressure, uptime and the apps using the most CPU and memory.",
            "inputSchema": { "type": "object", "properties": {
                "machine": { "type": "string", "description": "Machine name, or \"all\" (default)." } } },
            "annotations": ro,
        },
        {
            "name": "top_apps",
            "title": "Top apps",
            "description": "The apps using the most CPU or memory on one machine right now, with process counts. Helper processes are grouped under their app (macOS) or systemd service (Linux).",
            "inputSchema": { "type": "object", "properties": {
                "machine": machine,
                "by": { "type": "string", "enum": ["cpu", "memory"], "description": "Sort by (default cpu)." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 10, "description": "How many (default 10)." } } },
            "annotations": ro,
        },
        {
            "name": "history",
            "title": "Recent history",
            "description": "What happened over the last 30 minutes (2-second samples) or 24 hours (1-minute averages): average and peak CPU and memory, when memory pressure was high, CPU spikes with the app behind each, the busiest apps overall, and gaps when the machine was asleep or offline.",
            "inputSchema": { "type": "object", "properties": {
                "machine": machine,
                "range": { "type": "string", "enum": ["30m", "24h"], "description": "Default 24h." } } },
            "annotations": ro,
        },
        {
            "name": "disk",
            "title": "Disk usage",
            "description": "Free space on each drive, the biggest folders from the last disk scan, and cleanup suggestions grouped into 'safe to remove' and 'review first', each with its size, id and the exact command. Nothing is deleted.",
            "inputSchema": { "type": "object", "properties": {
                "machine": machine,
                "folder": { "type": "string", "description": "Optional folder path from a previous answer to list what is inside it." } } },
            "annotations": ro,
        },
        {
            "name": "machines",
            "title": "Machines",
            "description": "Every machine this monitor knows about: online or not, model, chip, memory, OS, uptime, whether cleanup is allowed, and its MCP URL.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": ro,
        },
        {
            "name": "scan_disk",
            "title": "Scan disk",
            "description": "Start a fresh disk scan in the background (1 to 3 minutes; it only reads). Call `disk` afterwards for the results.",
            "inputSchema": { "type": "object", "properties": { "machine": machine } },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        },
        {
            "name": "run_cleanup",
            "title": "Run cleanup",
            "description": "Run cleanup suggestions from `disk` by id. Two steps: call without `confirm` to get the exact commands and a one-time code, show them to the user and get their explicit approval, then call again with the same ids and `confirm` set to that code. Only works on machines started with --allow-cleanup. Commands that need sudo are never run; the user has to run those.",
            "inputSchema": { "type": "object", "required": ["ids"], "properties": {
                "machine": machine,
                "ids": { "type": "array", "items": { "type": "string" }, "description": "Suggestion ids from `disk`." },
                "confirm": { "type": "string", "description": "The one-time code from the first call, after the user approved." } } },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false },
        },
    ])
}

// ---------------------------------------------------------------- machines

struct Machine {
    name: String,
    url: Option<String>, // None = this machine
}

fn machines() -> Vec<Machine> {
    let c = cfg();
    let mut v = vec![Machine { name: c.name.clone(), url: None }];
    v.extend(c.peers.iter().map(|(n, u)| Machine { name: n.clone(), url: Some(u.clone()) }));
    v
}

fn pick(args: &Value, all_by_default: bool) -> Result<Vec<Machine>, String> {
    let want = args["machine"].as_str().unwrap_or("").trim();
    let ms = machines();
    if want.is_empty() {
        return Ok(if all_by_default { ms } else { ms.into_iter().take(1).collect() });
    }
    if want.eq_ignore_ascii_case("all") {
        return Ok(ms);
    }
    let names: Vec<String> = ms.iter().map(|m| m.name.clone()).collect();
    ms.into_iter()
        .find(|m| m.name.eq_ignore_ascii_case(want))
        .map(|m| vec![m])
        .ok_or_else(|| format!("No machine called \"{want}\". Known machines: {}.", names.join(", ")))
}

fn one(args: &Value) -> Result<Machine, String> {
    let want = args["machine"].as_str().unwrap_or("");
    if want.eq_ignore_ascii_case("all") {
        return Err("This tool works on one machine at a time; pass its name.".into());
    }
    Ok(pick(args, false)?.remove(0))
}

fn curl(url: &str, secs: u32, body: Option<&str>) -> Result<String, String> {
    let mut cmd = Command::new("curl");
    cmd.args(["-fsS", "--max-time", &secs.to_string()]);
    if body.is_some() {
        cmd.args(["-H", "Content-Type: application/json", "-H", "Accept: application/json, text/event-stream", "--data-binary", "@-"]);
    }
    let mut child = cmd.arg(url).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
        .map_err(|e| format!("can't run curl: {e}"))?;
    if let Some(b) = body {
        let _ = child.stdin.take().map(|mut i| i.write_all(b.as_bytes()));
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().replace("curl: ", "");
        Err(if err.is_empty() { "no answer".into() } else { err })
    }
}

/// GET one of the API paths from a machine, parsed.
fn get(m: &Machine, path: &str, st: &Mutex<State>) -> Result<Value, String> {
    let text = match &m.url {
        None => {
            let (p, q) = path.split_once("?since=").unwrap_or((path, "0"));
            let b = api(p, q.parse().unwrap_or(0), st).ok_or("not found")?;
            String::from_utf8_lossy(&b).into_owned()
        }
        Some(u) => curl(&format!("{u}{path}"), 6, None)?,
    };
    serde_json::from_str(&text).map_err(|e| format!("bad answer: {e}"))
}

/// Run a tool on a peer through its own /mcp.
fn forward(m: &Machine, tool: &str, args: &Value) -> Result<String, String> {
    let url = m.url.as_ref().expect("peer");
    let mut args = args.clone();
    if let Some(o) = args.as_object_mut() {
        o.remove("machine");
    }
    let req = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": tool, "arguments": args } });
    let out = curl(&format!("{url}/mcp"), 900, Some(&req.to_string()))
        .map_err(|e| format!("{} didn't answer ({e}). Is it running a monitor new enough to have MCP?", m.name))?;
    let v: Value = serde_json::from_str(&out).map_err(|e| format!("bad answer from {}: {e}", m.name))?;
    if let Some(e) = v.get("error") {
        return Err(format!("{}: {}", m.name, e["message"].as_str().unwrap_or("error")));
    }
    let text = v["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
    if v["result"]["isError"].as_bool() == Some(true) { Err(text) } else { Ok(text) }
}

// ---------------------------------------------------------------- formatting

fn gb(bytes: f64) -> String {
    let g = bytes / GIB;
    if g >= 10.0 { format!("{g:.0} GB") } else if g >= 1.0 { format!("{g:.1} GB") } else { format!("{:.0} MB", bytes / MIB) }
}
fn pct(v: f64) -> String {
    if v < 10.0 { format!("{v:.1}%") } else { format!("{v:.0}%") }
}
#[allow(deprecated)] // libc::time_t on musl; the cast is the same either way
fn local(ms: u64) -> libc::tm {
    let t = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm
}
/// "3:05 PM", or "3:05 PM yesterday"
fn clock(ms: u64) -> String {
    let (tm, today) = (local(ms), local(now_ms()));
    let h = if tm.tm_hour % 12 == 0 { 12 } else { tm.tm_hour % 12 };
    let day = if (tm.tm_year, tm.tm_yday) == (today.tm_year, today.tm_yday) { "" } else { " yesterday" };
    format!("{h}:{:02} {}{day}", tm.tm_min, if tm.tm_hour < 12 { "AM" } else { "PM" })
}
fn dur(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if d > 0 { format!("{d} day{} {h} h", if d > 1 { "s" } else { "" }) } else if h > 0 { format!("{h} h {m} min") } else { format!("{m} min") }
}
/// Friendly names for Mac model ids; anything else shows as reported.
fn model(id: &str) -> &str {
    match id {
        "Mac16,10" => "Mac mini (M4, 2024)",
        "Mac16,11" => "Mac mini (M4 Pro, 2024)",
        _ => id,
    }
}
fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

/// Same verdict as the page shows.
fn verdict(live: &Value) -> (String, String) {
    let n = &live["now"];
    let m = &n["mem"];
    let t = f(&n["t"]) as u64;
    let recent: Vec<f64> = live["points"].as_array().map(|ps| ps.iter().filter(|p| f(&p[0]) as u64 + 30_000 > t).map(|p| f(&p[1])).collect()).unwrap_or_default();
    let cpu30 = if recent.is_empty() { f(&n["cpu"]) } else { recent.iter().sum::<f64>() / recent.len() as f64 };
    let top_m = &n["topMem"][0];
    let top_c = &n["topCpu"][0];
    let (mname, mmem) = (top_m["name"].as_str().unwrap_or("?"), gb(f(&top_m["mem"])));
    let (cname, ccpu) = (top_c["name"].as_str().unwrap_or("?"), f(&top_c["cpu"]));
    let pressure = m["pressure"].as_str().unwrap_or("normal");
    if pressure == "critical" {
        return ("Memory is full".into(), format!("{mname} is using {mmem}. Quitting it would help."));
    }
    if pressure == "warn" || f(&m["swap"]) > 0.4 * f(&m["total"]) {
        return ("Memory is tight".into(), format!("{mname} is using {mmem}, and {} has spilled onto the disk.", gb(f(&m["swap"]))));
    }
    if cpu30 > 85.0 {
        return ("CPU is maxed out".into(), format!("{cname} is using {} of the processor.", pct(ccpu)));
    }
    if cpu30 > 60.0 {
        return ("Working hard".into(), format!("{cname} is the busiest at {}.", pct(ccpu)));
    }
    ("Running smoothly".into(), if ccpu > 5.0 { format!("Busiest right now: {cname} ({}).", pct(ccpu)) } else { String::new() })
}

fn apps_line(list: &Value, by_mem: bool, n: usize) -> String {
    list.as_array().map(|a| a.iter().take(n).map(|x| {
        let v = if by_mem { gb(f(&x["mem"])) } else { pct(f(&x["cpu"])) };
        format!("{} {v}", x["name"].as_str().unwrap_or("?"))
    }).collect::<Vec<_>>().join(", ")).unwrap_or_default()
}

// ---------------------------------------------------------------- tools

fn call(name: &str, args: &Value, st: &Mutex<State>) -> Result<String, String> {
    match name {
        "status" => status(args, st),
        "top_apps" => top_apps(args, st),
        "history" => history(args, st),
        "disk" => disk(args, st),
        "machines" => list_machines(st),
        "scan_disk" => {
            let m = one(args)?;
            if m.url.is_some() { forward(&m, name, args) } else { scan_disk() }
        }
        "run_cleanup" => {
            let m = one(args)?;
            if m.url.is_some() { forward(&m, name, args) } else { run_cleanup(args) }
        }
        _ => Err(format!("Unknown tool {name}.")),
    }
}

fn status(args: &Value, st: &Mutex<State>) -> Result<String, String> {
    let since = now_ms().saturating_sub(35_000);
    let mut out = String::new();
    for m in pick(args, true)? {
        let live = match get(&m, &format!("/api/live?since={since}"), st) {
            Ok(v) if !v["now"].is_null() => v,
            Ok(_) => { let _ = writeln!(out, "{}: starting up, no sample yet.\n", m.name); continue; }
            Err(e) => { let _ = writeln!(out, "{}: offline ({e}).\n", m.name); continue; }
        };
        let info = get(&m, "/api/info", st).unwrap_or(json!({}));
        let n = &live["now"];
        let mem = &n["mem"];
        let (v, why) = verdict(&live);
        let _ = writeln!(out, "{}: {v}. {why}", m.name);
        let _ = writeln!(out, "- CPU {} of {} cores (load {:.1})", pct(f(&n["cpu"])), n["cores"], f(&n["load"]));
        let _ = writeln!(out, "- Memory {} of {}, pressure {}, swap {}", gb(f(&mem["used"])), gb(f(&mem["total"])), mem["pressure"].as_str().unwrap_or("?"), gb(f(&mem["swap"])));
        if let Some(b) = info["boot"].as_u64() {
            let _ = writeln!(out, "- Up {}", dur((now_ms() / 1000).saturating_sub(b)));
        }
        if let Some(ds) = info["disks"].as_array() {
            let d: Vec<String> = ds.iter().map(|d| format!("{} {} free of {}", d["name"].as_str().unwrap_or("?"), gb(f(&d["free"])), gb(f(&d["total"])))).collect();
            let _ = writeln!(out, "- Disks: {}", d.join("; "));
        }
        let _ = writeln!(out, "- Top CPU: {}", apps_line(&n["topCpu"], false, 5));
        let _ = writeln!(out, "- Top memory: {}\n", apps_line(&n["topMem"], true, 5));
    }
    Ok(out.trim_end().to_string())
}

fn top_apps(args: &Value, st: &Mutex<State>) -> Result<String, String> {
    let m = one(args)?;
    let by_mem = args["by"].as_str() == Some("memory");
    let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 10) as usize;
    let live = get(&m, &format!("/api/live?since={}", now_ms()), st).map_err(|e| format!("{} is offline ({e}).", m.name))?;
    let n = &live["now"];
    let list = n[if by_mem { "topMem" } else { "topCpu" }].as_array().cloned().unwrap_or_default();
    let mut out = format!("{}: top apps by {} (CPU is a share of all {} cores)\n", m.name, if by_mem { "memory" } else { "CPU" }, n["cores"]);
    for a in list.iter().take(limit) {
        let count = a["n"].as_u64().unwrap_or(0);
        let _ = writeln!(out, "- {}: CPU {}, memory {}{}", a["name"].as_str().unwrap_or("?"), pct(f(&a["cpu"])), gb(f(&a["mem"])),
            if count > 1 { format!(" ({count} processes)") } else { String::new() });
    }
    Ok(out.trim_end().to_string())
}

fn history(args: &Value, st: &Mutex<State>) -> Result<String, String> {
    let m = one(args)?;
    let day = args["range"].as_str() != Some("30m");
    let data = get(&m, if day { "/api/day" } else { "/api/live?since=0" }, st).map_err(|e| format!("{} is offline ({e}).", m.name))?;
    let total = get(&m, "/api/info", st).ok().and_then(|i| i["ram"].as_f64()).unwrap_or(0.0);
    let ps = data["points"].as_array().cloned().unwrap_or_default();
    if ps.len() < 2 {
        return Ok(format!("{}: not enough history yet{}.", m.name, if day { " (24 h history fills in one minute at a time)" } else { "" }));
    }
    let step = if day { 60_000.0 } else { 2_000.0 };
    let gap = if day { 180_000 } else { 8_000 };
    let t = |p: &Value| f(&p[0]) as u64;
    let mem = |p: &Value| (f(&p[2]) + f(&p[3]) + f(&p[4])) * MIB;
    let n = ps.len() as f64;
    let avg_cpu = ps.iter().map(|p| f(&p[1])).sum::<f64>() / n;
    let peak_cpu = ps.iter().max_by(|a, b| f(&a[1]).total_cmp(&f(&b[1]))).unwrap();
    let avg_mem = ps.iter().map(mem).sum::<f64>() / n;
    let peak_mem = ps.iter().max_by(|a, b| mem(a).total_cmp(&mem(b))).unwrap();
    let peak_swap = ps.iter().map(|p| f(&p[5]) * MIB).fold(0.0, f64::max);

    let mut out = format!("{}: last {} ({} to {})\n", m.name, if day { "24 hours, 1-minute averages" } else { "30 minutes, 2-second samples" }, clock(t(&ps[0])), clock(t(ps.last().unwrap())));
    let _ = writeln!(out, "- CPU: average {}, peak {} at {}", pct(avg_cpu), pct(f(&peak_cpu[1])), clock(t(peak_cpu)));
    let _ = writeln!(out, "- Memory: average {}, peak {} at {}{}", gb(avg_mem), gb(mem(peak_mem)), clock(t(peak_mem)), if total > 0.0 { format!(" (of {})", gb(total)) } else { String::new() });
    if peak_swap > 0.0 {
        let _ = writeln!(out, "- Swap peaked at {}", gb(peak_swap));
    }

    // runs of consecutive samples, for pressure, CPU spikes and gaps
    let runs = |hit: &dyn Fn(&Value) -> bool| {
        let mut v: Vec<(usize, usize)> = Vec::new();
        for (i, p) in ps.iter().enumerate() {
            if !hit(p) { continue; }
            match v.last_mut() {
                Some(r) if r.1 + 1 == i && t(p) - t(&ps[i - 1]) <= gap => r.1 = i,
                _ => v.push((i, i)),
            }
        }
        v
    };
    let span = |r: &(usize, usize)| format!("{}–{}", clock(t(&ps[r.0])), clock(t(&ps[r.1]) + step as u64));
    let pr = runs(&|p| f(&p[6]) >= 1.0);
    if pr.is_empty() {
        let _ = writeln!(out, "- Memory pressure stayed normal");
    } else {
        let mins: f64 = pr.iter().map(|r| (r.1 - r.0 + 1) as f64 * step / 60_000.0).sum();
        let list: Vec<String> = pr.iter().rev().take(5).map(|r| {
            let crit = ps[r.0..=r.1].iter().any(|p| f(&p[6]) >= 2.0);
            format!("{}{}", span(r), if crit { " (critical)" } else { "" })
        }).collect();
        let _ = writeln!(out, "- Memory pressure was high for {:.0} min in total: {}", mins.max(1.0), list.join(", "));
    }
    let spikes = runs(&|p| f(&p[1]) >= 80.0);
    if !spikes.is_empty() {
        let list: Vec<String> = spikes.iter().rev().take(5).map(|r| {
            let mut who: HashMap<String, f64> = HashMap::new();
            for p in &ps[r.0..=r.1] {
                if let Some(a) = p[7][0].as_array() {
                    *who.entry(a[0].as_str().unwrap_or("?").to_string()).or_default() += f(&a[1]);
                }
            }
            let top = who.into_iter().max_by(|a, b| a.1.total_cmp(&b.1)).map(|w| format!(", mostly {}", w.0)).unwrap_or_default();
            format!("{}{top}", span(r))
        }).collect();
        let _ = writeln!(out, "- CPU above 80%: {}", list.join("; "));
    }
    let mut busy: HashMap<String, f64> = HashMap::new();
    for p in &ps {
        for a in p[7].as_array().into_iter().flatten() {
            *busy.entry(a[0].as_str().unwrap_or("?").to_string()).or_default() += f(&a[1]) / n;
        }
    }
    let mut busy: Vec<_> = busy.into_iter().collect();
    busy.sort_by(|a, b| b.1.total_cmp(&a.1));
    let busy: Vec<String> = busy.iter().take(5).map(|(k, v)| format!("{k} {}", pct(*v))).collect();
    let _ = writeln!(out, "- Busiest apps on average: {}", busy.join(", "));
    let gaps: Vec<String> = ps.windows(2).filter(|w| t(&w[1]) - t(&w[0]) > gap)
        .map(|w| format!("{}–{}", clock(t(&w[0])), clock(t(&w[1])))).collect();
    if !gaps.is_empty() {
        let _ = writeln!(out, "- No data (asleep, off or monitor restarting): {}", gaps.iter().rev().take(5).cloned().collect::<Vec<_>>().join(", "));
    }
    Ok(out.trim_end().to_string())
}

fn find<'a>(node: &'a Value, path: &str) -> Option<&'a Value> {
    if node["path"].as_str() == Some(path) {
        return Some(node);
    }
    node["children"].as_array()?.iter().find_map(|c| {
        let p = c["path"].as_str().unwrap_or("");
        (p == path || path.starts_with(&format!("{}/", p.trim_end_matches('/')))).then(|| find(c, path)).flatten()
    })
}

fn disk(args: &Value, st: &Mutex<State>) -> Result<String, String> {
    let m = one(args)?;
    let info = get(&m, "/api/info", st).map_err(|e| format!("{} is offline ({e}).", m.name))?;
    let mut out = format!("{}: drives\n", m.name);
    for d in info["disks"].as_array().into_iter().flatten() {
        let (t, fr) = (f(&d["total"]), f(&d["free"]));
        let _ = writeln!(out, "- {}: {} free of {} ({:.0}% used)", d["name"].as_str().unwrap_or("?"), gb(fr), gb(t), if t > 0.0 { (1.0 - fr / t) * 100.0 } else { 0.0 });
    }
    if info["scanning"].as_bool() == Some(true) {
        out.push_str("A disk scan is running now; call `disk` again in a minute or two.\n");
    }
    let Ok(scan) = get(&m, "/data.json", st) else {
        out.push_str("\nNo disk scan yet. Run `scan_disk` (1 to 3 minutes), then call `disk` again.");
        return Ok(out);
    };
    let _ = writeln!(out, "\nLast scan: {} (took {} s). Folder sizes below are from then.", scan["scannedAt"].as_str().unwrap_or("?").replace('T', " "), scan["scanSeconds"]);
    let root = &scan["tree"];
    let folder = args["folder"].as_str().unwrap_or("");
    let node = if folder.is_empty() { root } else { find(root, folder).ok_or_else(|| format!("{folder} isn't in the last scan (only folders over 40 MB are kept)."))? };
    let _ = writeln!(out, "\nBiggest in {} ({}):", node["path"].as_str().unwrap_or("/"), gb(f(&node["size"])));
    for c in node["children"].as_array().into_iter().flatten().take(12) {
        let inner: Vec<String> = c["children"].as_array().into_iter().flatten().filter(|k| k["other"].as_bool() != Some(true)).take(4)
            .map(|k| format!("{} {}", k["name"].as_str().unwrap_or("?"), gb(f(&k["size"])))).collect();
        let _ = writeln!(out, "- {} {} [{}]{}", c["name"].as_str().unwrap_or("?"), gb(f(&c["size"])), c["path"].as_str().unwrap_or(""),
            if inner.is_empty() { String::new() } else { format!(": {}", inner.join(", ")) });
    }
    if !folder.is_empty() {
        return Ok(out.trim_end().to_string());
    }
    let recs = scan["recs"].as_array().cloned().unwrap_or_default();
    for (group, label) in [("safe", "Safe to remove (rebuilt automatically when needed)"), ("review", "Review first (the user should decide)")] {
        let rs: Vec<&Value> = recs.iter().filter(|r| r["group"].as_str() == Some(group)).collect();
        if rs.is_empty() { continue; }
        let total: f64 = rs.iter().map(|r| f(&r["size"])).sum();
        let _ = writeln!(out, "\n{label}, {} in total:", gb(total));
        for r in rs {
            let _ = writeln!(out, "- [{}] {} {}: {} Command: {}", r["id"].as_str().unwrap_or("?"), r["title"].as_str().unwrap_or("?"), gb(f(&r["size"])),
                r["note"].as_str().unwrap_or(""), r["cmd"].as_str().unwrap_or("").replace('\n', " ; "));
        }
    }
    let allowed = if m.url.is_none() { cfg().allow_cleanup } else { get(&m, "/api/config", st).ok().and_then(|c| c["allowCleanup"].as_bool()).unwrap_or(false) };
    out.push_str(if allowed {
        "\nThese can be run with `run_cleanup` (ids above), after the user approves the exact commands."
    } else {
        "\nCleanup can't be run from here on this machine (it wasn't started with --allow-cleanup). The user can paste the commands into a terminal, or use the Disk tab's Copy script button."
    });
    Ok(out)
}

fn list_machines(st: &Mutex<State>) -> Result<String, String> {
    let mut out = String::new();
    for m in machines() {
        let base = m.url.clone().unwrap_or_else(|| format!("http://127.0.0.1:{}", cfg().port));
        match get(&m, "/api/info", st) {
            Ok(i) => {
                let cleanup = if m.url.is_none() { Some(cfg().allow_cleanup) } else { get(&m, "/api/config", st).ok().and_then(|c| c["allowCleanup"].as_bool()) };
                let _ = writeln!(out, "- {}{}: online. {} · {} · {} cores · {} memory · {} · up {}. Cleanup {}. MCP: {base}/mcp",
                    m.name, if m.url.is_none() { " (this machine)" } else { "" },
                    model(i["model"].as_str().unwrap_or("?")), i["chip"].as_str().unwrap_or("?"), i["cores"], gb(f(&i["ram"])), i["os"].as_str().unwrap_or("?"),
                    dur((now_ms() / 1000).saturating_sub(i["boot"].as_u64().unwrap_or(0))),
                    if cleanup == Some(true) { "allowed" } else { "off" });
            }
            Err(e) => { let _ = writeln!(out, "- {}: offline ({e}). MCP: {base}/mcp", m.name); }
        }
    }
    Ok(out.trim_end().to_string())
}

// ---------------------------------------------------------------- actions

fn scan_disk() -> Result<String, String> {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let script = [exe_dir, Some(cfg().dir.clone())].into_iter().flatten().map(|d| d.join("scan.py")).find(|p| p.exists())
        .ok_or("scan.py isn't installed next to the monitor. Re-run install.sh, or run scan.py from the repo.")?;
    if SCANNING.swap(true, Ordering::SeqCst) {
        return Ok("A scan is already running. Call `disk` in a minute or two.".into());
    }
    let dir = cfg().dir.clone();
    std::thread::spawn(move || {
        let out = Command::new("python3").arg(&script).arg("--dir").arg(&dir).stdin(Stdio::null()).output();
        let msg = match out {
            Ok(o) => {
                let s = String::from_utf8_lossy(if o.status.success() { &o.stdout } else { &o.stderr }).trim().to_string();
                s.lines().last().unwrap_or("").to_string()
            }
            Err(e) => format!("couldn't run python3: {e}"),
        };
        *LAST_SCAN.lock().unwrap() = msg;
        SCANNING.store(false, Ordering::SeqCst);
    });
    let last = LAST_SCAN.lock().unwrap().clone();
    let mut s = String::from("Scan started. It only reads, and takes 1 to 3 minutes; call `disk` afterwards.");
    if cfg!(target_os = "macos") {
        s.push_str(" On macOS, if Home comes out much smaller than expected, macOS blocked the background scan from your files: give the monitor Full Disk Access, or run scan.py from Terminal.");
    }
    if !last.is_empty() {
        let _ = write!(s, " Previous scan: {last}");
    }
    Ok(s)
}

fn run_cleanup(args: &Value) -> Result<String, String> {
    if !cfg().allow_cleanup {
        return Err(format!("Cleanup is turned off on {}. To allow it, restart the monitor with --allow-cleanup (re-run install.sh with that option). Until then, give the user the commands from `disk` to run themselves.", cfg().name));
    }
    let mut ids: Vec<String> = args["ids"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err("Pass the ids of the suggestions to run (from `disk`).".into());
    }
    let scan: Value = std::fs::read_to_string(cfg().dir.join("data.json")).ok().and_then(|s| serde_json::from_str(&s).ok())
        .ok_or("There's no disk scan. Run `scan_disk` first.")?;
    let recs = scan["recs"].as_array().cloned().unwrap_or_default();
    let mut chosen = Vec::new();
    for id in &ids {
        chosen.push(recs.iter().find(|r| r["id"].as_str() == Some(id)).ok_or_else(|| format!("No suggestion with id \"{id}\" in the last scan."))?.clone());
    }
    // Never run here: anything needing a password, or that the scan marked as needing a terminal
    let needs_sudo = |r: &Value| r["terminal"].as_bool() == Some(true) || r["cmd"].as_str().unwrap_or("").split_whitespace().any(|w| w == "sudo");
    let cmds: Vec<String> = chosen.iter().map(|r| r["cmd"].as_str().unwrap_or("").to_string()).collect();
    let mut pending = PENDING.lock().unwrap();
    pending.retain(|p| p.3 > Instant::now());

    let Some(code) = args["confirm"].as_str().filter(|c| !c.is_empty()) else {
        // Step 1: the plan, and a one-time code for step 2
        let code = loop {
            let c = random_code();
            if !pending.iter().any(|p| p.0 == c) { break c; }
        };
        pending.push((code.clone(), ids.clone(), cmds.clone(), Instant::now() + Duration::from_secs(600)));
        let mut out = format!("Plan for {} (nothing has run yet). Show the user these exact commands and ask them to approve:\n", cfg().name);
        for r in &chosen {
            let _ = writeln!(out, "\n[{}] {} ({}, {}): {}", r["id"].as_str().unwrap_or(""), r["title"].as_str().unwrap_or(""), gb(f(&r["size"])),
                if r["group"].as_str() == Some("safe") { "safe to remove" } else { "review first" }, r["note"].as_str().unwrap_or(""));
            if needs_sudo(r) {
                out.push_str("  Needs a password or a terminal, so it will NOT be run here. The user must run it themselves:\n");
            }
            for l in r["cmd"].as_str().unwrap_or("").lines() {
                let _ = writeln!(out, "    {l}");
            }
        }
        let total: f64 = chosen.iter().filter(|r| !needs_sudo(r)).map(|r| f(&r["size"])).sum();
        let _ = write!(out, "\nFrees about {}. If the user approves, call run_cleanup again with the same ids and confirm: \"{code}\". The code works once and expires in 10 minutes.", gb(total));
        return Ok(out);
    };

    // Step 2: run, if the code matches these ids
    let Some(i) = pending.iter().position(|p| p.0 == code && p.1 == ids) else {
        return Err("That confirmation code doesn't match these ids, was already used, or expired. Call run_cleanup without confirm to get a new plan.".into());
    };
    let approved = pending.remove(i);
    if approved.2 != cmds {
        return Err("The commands changed since the user approved them (a new disk scan finished). Nothing ran. Call run_cleanup without confirm, and show the user the new plan.".into());
    }
    drop(pending);
    let before = plat::statvfs(plat::DATA_VOLUME).map(|d| d.1).unwrap_or(0);
    let mut out = format!("Ran on {}:\n", cfg().name);
    for r in &chosen {
        let title = r["title"].as_str().unwrap_or("");
        if needs_sudo(r) {
            let _ = writeln!(out, "- {title}: skipped, needs a password or a terminal. The user should run it themselves: {}", r["cmd"].as_str().unwrap_or(""));
            continue;
        }
        let res = run_shell(r["cmd"].as_str().unwrap_or(""), Duration::from_secs(600));
        let _ = writeln!(out, "- {title}: {res}");
    }
    let after = plat::statvfs(plat::DATA_VOLUME).map(|d| d.1).unwrap_or(0);
    let _ = write!(out, "\nFree space went from {} to {} ({}). Run `scan_disk` to refresh the suggestions.",
        gb(before as f64), gb(after as f64), if after >= before { format!("+{}", gb((after - before) as f64)) } else { "less, something else is writing".into() });
    Ok(out)
}

fn run_shell(cmd: &str, limit: Duration) -> String {
    use std::os::unix::process::CommandExt;
    // Its own process group, so a timeout can stop everything it started
    let child = Command::new("/bin/sh").arg("-c").arg(cmd).process_group(0)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return format!("couldn't start ({e})"),
    };
    // Drain both pipes while it runs, so a chatty command can't block on a full pipe
    let drain = |p: Option<Box<dyn Read + Send>>| std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut p) = p { let _ = p.read_to_string(&mut s); }
        s
    });
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if start.elapsed() > limit => {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => break None,
        }
    };
    if status.is_some() {
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) }; // leftovers still holding the pipes
    }
    let text = out.join().unwrap_or_default() + &err.join().unwrap_or_default();
    let tail: String = text.trim().chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
    let tail = if tail.is_empty() { String::new() } else { format!(" Output: {}", tail.replace('\n', " / ")) };
    match status {
        Some(s) if s.success() => format!("done.{tail}"),
        Some(s) => format!("failed (exit {}).{tail}", s.code().unwrap_or(-1)),
        None => format!("stopped after {} min.{tail}", limit.as_secs() / 60),
    }
}

// ---------------------------------------------------------------- stdio bridge

/// `monitor mcp [--url http://127.0.0.1:8620]`: one JSON-RPC message per line on
/// stdin, forwarded to the monitor's /mcp; answers go to stdout.
pub fn stdio(args: Vec<String>) -> ! {
    let port = std::env::var("PORT").ok().unwrap_or_else(|| "8620".into());
    let mut url = format!("http://127.0.0.1:{port}");
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--url" => url = it.next().unwrap_or_default(),
            s if s.starts_with("--url=") => url = s[6..].to_string(),
            "-h" | "--help" => {
                println!("monitor mcp [--url URL]\n\nStdio MCP server that forwards to a monitor's /mcp endpoint (default {url}).");
                std::process::exit(0)
            }
            _ => {
                eprintln!("monitor mcp: unknown option {a}");
                std::process::exit(2)
            }
        }
    }
    let url = url.trim_end_matches('/').trim_end_matches("/mcp").to_string();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() { continue; }
        let id = serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("id").cloned());
        let reply = if url.starts_with("http://") { post_local(&url, &line) } else { curl(&format!("{url}/mcp"), 900, Some(&line)) };
        let out = match reply {
            Ok(b) if b.trim().is_empty() => continue,
            Ok(b) => b,
            Err(e) => match id {
                Some(id) => error(id, -32000, &format!("Monitor isn't answering at {url} ({e}). Is it running?")),
                None => continue,
            },
        };
        let _ = writeln!(stdout, "{}", out.trim());
        let _ = stdout.flush();
    }
    std::process::exit(0)
}

/// Minimal HTTP/1.1 POST over plain TCP, so the bridge needs nothing else.
fn post_local(url: &str, body: &str) -> Result<String, String> {
    let hostport = url.trim_start_matches("http://").split('/').next().unwrap_or("");
    let mut c = std::net::TcpStream::connect(hostport).map_err(|e| e.to_string())?;
    let _ = c.set_read_timeout(Some(Duration::from_secs(900)));
    let req = format!("POST /mcp HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    c.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut resp = String::new();
    c.read_to_string(&mut resp).map_err(|e| e.to_string())?;
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((&resp, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("");
    if status.starts_with('2') { Ok(body.to_string()) } else { Err(format!("HTTP {status}")) }
}
