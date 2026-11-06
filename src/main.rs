//! sonos — tiny CLI to control a Sonos speaker on the LAN (volume, mute, playback).
//!
//! Config lives in `sonos.json` next to the executable (symlinks resolved).
//! Run `sonos help` for usage.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, BufRead, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

const CONFIG_NAME: &str = "sonos.json";
const SONOS_PORT: u16 = 1400;
const RC_PATH: &str = "/MediaRenderer/RenderingControl/Control";
const RC_NS: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const AV_PATH: &str = "/MediaRenderer/AVTransport/Control";
const AV_NS: &str = "urn:schemas-upnp-org:service:AVTransport:1";
/// Commands that may be bound to a shortcut.
const BINDABLE: &[&str] = &["up", "down", "mute", "play-pause", "play", "pause", "next", "previous"];
const GS_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
const GS_BASE: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings";

type Res<T> = Result<T, String>;

// ---------------------------------------------------------------- config

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
struct Config {
    /// IP address of the speaker.
    ip: String,
    /// Sonos UUID (RINCON_...). Used to find the speaker again if its IP changes.
    uuid: String,
    /// Room name, informational.
    name: String,
    /// Model name, informational.
    model: String,
    /// Volume change per key press.
    step: i32,
    /// Volume is never raised above this (0-100).
    max_volume: i32,
    /// HTTP timeout in milliseconds.
    timeout_ms: u64,
    /// command -> GNOME accelerator, e.g. "up": "<Super><Alt>XF86AudioRaiseVolume".
    /// Remove an entry (or set it to "") to disable that shortcut.
    shortcuts: BTreeMap<String, String>,
    /// Run `sonos refresh` at login (re-finds the speaker if its IP changed and
    /// re-applies the shortcuts).
    autostart: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            ip: String::new(),
            uuid: String::new(),
            name: String::new(),
            model: String::new(),
            step: 2,
            max_volume: 100,
            timeout_ms: 1500,
            // Super+Alt + the standard media keys.
            shortcuts: [
                ("previous", "<Super><Alt>XF86AudioPrev"),
                ("play-pause", "<Super><Alt>XF86AudioPlay"),
                ("next", "<Super><Alt>XF86AudioNext"),
                ("mute", "<Super><Alt>XF86AudioMute"),
                ("down", "<Super><Alt>XF86AudioLowerVolume"),
                ("up", "<Super><Alt>XF86AudioRaiseVolume"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            autostart: true,
        }
    }
}

fn exe_path() -> Res<PathBuf> {
    let p = std::env::current_exe().map_err(|e| format!("cannot locate executable: {e}"))?;
    Ok(p.canonicalize().unwrap_or(p))
}

fn config_path() -> Res<PathBuf> {
    Ok(exe_path()?.with_file_name(CONFIG_NAME))
}

fn load_config() -> Res<Option<Config>> {
    let path = config_path()?;
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| format!("invalid {}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn require_config() -> Res<Config> {
    load_config()?.ok_or_else(|| "not configured yet — run `sonos init` first".to_string())
}

fn save_config(cfg: &Config) -> Res<()> {
    let path = config_path()?;
    let mut s = serde_json::to_string_pretty(cfg).unwrap();
    s.push('\n');
    std::fs::write(&path, s).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

// ---------------------------------------------------------------- http / soap

fn http(ip: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &str, timeout: Duration) -> Res<(u16, String)> {
    let addr: SocketAddr = (ip, SONOS_PORT)
        .to_socket_addrs()
        .map_err(|e| format!("bad address {ip}: {e}"))?
        .next()
        .ok_or_else(|| format!("bad address {ip}"))?;
    let mut s = TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("cannot reach {ip}: {e}"))?;
    s.set_read_timeout(Some(timeout)).ok();
    s.set_write_timeout(Some(timeout)).ok();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {ip}:{SONOS_PORT}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).map_err(|e| format!("send to {ip} failed: {e}"))?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).map_err(|e| format!("read from {ip} failed: {e}"))?;
    let split = find(&buf, b"\r\n\r\n").ok_or_else(|| format!("bad HTTP response from {ip}"))?;
    let head = String::from_utf8_lossy(&buf[..split]).to_ascii_lowercase();
    let body = &buf[split + 4..];
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| format!("bad HTTP response from {ip}"))?;
    let body = if head.contains("transfer-encoding: chunked") { dechunk(body) } else { body.to_vec() };
    Ok((code, String::from_utf8_lossy(&body).into_owned()))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn dechunk(mut s: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = find(s, b"\r\n") {
        let line = String::from_utf8_lossy(&s[..eol]);
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        let rest = &s[eol + 2..];
        if size == 0 || rest.len() < size {
            break;
        }
        out.extend_from_slice(&rest[..size]);
        s = rest[size..].strip_prefix(b"\r\n").unwrap_or(&rest[size..]);
    }
    out
}

/// RenderingControl call (volume / mute).
fn soap(cfg: &Config, action: &str, args: &str) -> Res<String> {
    soap_call(cfg, RC_PATH, RC_NS, action, args)
}

/// AVTransport call (play / pause / next / previous).
fn soap_av(cfg: &Config, action: &str, args: &str) -> Res<String> {
    soap_call(cfg, AV_PATH, AV_NS, action, args)
}

fn soap_call(cfg: &Config, path: &str, ns: &str, action: &str, args: &str) -> Res<String> {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{ns}"><InstanceID>0</InstanceID>{args}</u:{action}></s:Body></s:Envelope>"#
    );
    let sa = format!("\"{ns}#{action}\"");
    let (code, resp) = http(
        &cfg.ip,
        "POST",
        path,
        &[("Content-Type", "text/xml; charset=\"utf-8\""), ("SOAPACTION", &sa)],
        &body,
        Duration::from_millis(cfg.timeout_ms),
    )?;
    if code != 200 {
        let detail = tag(&resp, "errorCode").map(|c| format!(" (UPnP error {c})")).unwrap_or_default();
        let hint = match tag(&resp, "errorCode").as_deref() {
            Some("701") => " — nothing to play/skip (e.g. TV input or empty queue)",
            Some("800") => " — speaker is grouped; control the group's main speaker",
            _ => "",
        };
        return Err(format!("{action} failed: HTTP {code}{detail}{hint}"));
    }
    Ok(resp)
}

/// Text content of the first `<name>` element.
fn tag(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))? + start;
    Some(xml[start..end].trim().to_string())
}

fn get_volume(cfg: &Config) -> Res<i32> {
    let r = soap(cfg, "GetVolume", "<Channel>Master</Channel>")?;
    tag(&r, "CurrentVolume").and_then(|v| v.parse().ok()).ok_or("unexpected GetVolume reply".into())
}

fn set_volume(cfg: &Config, v: i32) -> Res<i32> {
    let v = v.clamp(0, cfg.max_volume.clamp(0, 100));
    soap(cfg, "SetVolume", &format!("<Channel>Master</Channel><DesiredVolume>{v}</DesiredVolume>"))?;
    Ok(v)
}

fn change_volume(cfg: &Config, delta: i32) -> Res<i32> {
    let r = soap(cfg, "SetRelativeVolume", &format!("<Channel>Master</Channel><Adjustment>{delta}</Adjustment>"))?;
    let new: i32 = tag(&r, "NewVolume").and_then(|v| v.parse().ok()).ok_or("unexpected SetRelativeVolume reply")?;
    if new > cfg.max_volume { set_volume(cfg, cfg.max_volume) } else { Ok(new) }
}

fn get_mute(cfg: &Config) -> Res<bool> {
    let r = soap(cfg, "GetMute", "<Channel>Master</Channel>")?;
    Ok(tag(&r, "CurrentMute").as_deref() == Some("1"))
}

fn set_mute(cfg: &Config, m: bool) -> Res<()> {
    soap(cfg, "SetMute", &format!("<Channel>Master</Channel><DesiredMute>{}</DesiredMute>", m as u8)).map(|_| ())
}

/// PLAYING, PAUSED_PLAYBACK, STOPPED or TRANSITIONING.
fn transport_state(cfg: &Config) -> Res<String> {
    let r = soap_av(cfg, "GetTransportInfo", "")?;
    tag(&r, "CurrentTransportState").ok_or("unexpected GetTransportInfo reply".into())
}

fn play(cfg: &Config) -> Res<()> {
    soap_av(cfg, "Play", "<Speed>1</Speed>").map(|_| ())
}

fn pause(cfg: &Config) -> Res<()> {
    soap_av(cfg, "Pause", "").map(|_| ())
}

/// Returns true if now playing.
fn play_pause(cfg: &Config) -> Res<bool> {
    match transport_state(cfg)?.as_str() {
        "PLAYING" | "TRANSITIONING" => pause(cfg).map(|_| false),
        _ => play(cfg).map(|_| true),
    }
}

// ---------------------------------------------------------------- discovery

#[derive(Clone, Debug)]
struct Device {
    ip: String,
    uuid: String,
    name: String,
    model: String,
}

fn describe(ip: &str, timeout: Duration) -> Res<Device> {
    let (code, xml) = http(ip, "GET", "/xml/device_description.xml", &[], "", timeout)?;
    if code != 200 {
        return Err(format!("{ip}: HTTP {code}"));
    }
    Ok(Device {
        ip: ip.to_string(),
        uuid: tag(&xml, "UDN").unwrap_or_default().trim_start_matches("uuid:").to_string(),
        name: tag(&xml, "roomName").unwrap_or_default(),
        model: tag(&xml, "modelName").unwrap_or_default(),
    })
}

/// Find Sonos speakers: SSDP, then mDNS (avahi), then a TCP scan of the local /24.
/// Many networks/firmwares drop SSDP, so the fallbacks matter.
fn discover(wait: Duration) -> Res<Vec<Device>> {
    let mut ips = ssdp_ips(wait).unwrap_or_default();
    if ips.is_empty() {
        ips = avahi_ips();
    }
    if ips.is_empty() {
        ips = scan_ips();
    }
    let mut out: BTreeMap<String, Device> = BTreeMap::new();
    for ip in ips {
        if let Ok(d) = describe(&ip, Duration::from_secs(2)) {
            if !d.uuid.is_empty() {
                out.insert(d.uuid.clone(), d);
            }
        }
    }
    let mut v: Vec<Device> = out.into_values().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(v)
}

fn avahi_ips() -> Vec<String> {
    let Ok(out) = Command::new("avahi-browse").args(["-rpt", "_sonos._tcp"]).output() else {
        return vec![];
    };
    let mut ips = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let f: Vec<&str> = line.split(';').collect();
        if f.len() > 7 && f[0] == "=" && f[2] == "IPv4" && !ips.contains(&f[7].to_string()) {
            ips.push(f[7].to_string());
        }
    }
    ips
}

/// Probe port 1400 on every host of the local /24 subnet.
fn scan_ips() -> Vec<String> {
    // "Connecting" a UDP socket sends nothing; it only makes the OS pick the
    // outgoing interface, which tells us our own LAN address. Any non-local
    // address works; this one is reserved for documentation (RFC 5737).
    let local = UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").map(|_| s))
        .and_then(|s| s.local_addr());
    let Ok(SocketAddr::V4(local)) = local else { return vec![] };
    let o = local.ip().octets();
    let handles: Vec<_> = (1..=254u8)
        .filter(|&h| h != o[3])
        .map(|h| {
            std::thread::spawn(move || {
                let addr = SocketAddr::from(([o[0], o[1], o[2], h], SONOS_PORT));
                TcpStream::connect_timeout(&addr, Duration::from_millis(400)).ok().map(|_| addr.ip().to_string())
            })
        })
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
}

fn ssdp_ips(wait: Duration) -> Res<Vec<String>> {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("udp bind: {e}"))?;
    let msg = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n";
    for _ in 0..2 {
        sock.send_to(msg.as_bytes(), "239.255.255.250:1900").map_err(|e| format!("ssdp send: {e}"))?;
    }
    let deadline = Instant::now() + wait;
    let mut ips: Vec<String> = Vec::new();
    let mut buf = [0u8; 2048];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if left.is_zero() {
            break;
        }
        sock.set_read_timeout(Some(left)).ok();
        let Ok((n, from)) = sock.recv_from(&mut buf) else { break };
        let txt = String::from_utf8_lossy(&buf[..n]);
        if !txt.to_ascii_lowercase().contains("zoneplayer") {
            continue;
        }
        let ip = from.ip().to_string();
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    Ok(ips)
}

/// Make sure cfg.ip still points at cfg.uuid; if not, find it again and save.
fn ensure_reachable(cfg: &mut Config, verbose: bool) -> Res<()> {
    let t = Duration::from_millis(cfg.timeout_ms);
    if let Ok(d) = describe(&cfg.ip, t) {
        if cfg.uuid.is_empty() || d.uuid == cfg.uuid {
            return Ok(());
        }
    }
    if cfg.uuid.is_empty() {
        return Err(format!("speaker at {} unreachable; run `sonos init`", cfg.ip));
    }
    if verbose {
        eprintln!("speaker not at {}, searching network…", cfg.ip);
    }
    let found = discover(Duration::from_millis(1500))?.into_iter().find(|d| d.uuid == cfg.uuid);
    match found {
        Some(d) => {
            if verbose {
                eprintln!("found {} at {}", d.name, d.ip);
            }
            cfg.ip = d.ip;
            save_config(cfg)
        }
        None => Err(format!("speaker {} ({}) not found on the network", cfg.name, cfg.uuid)),
    }
}

/// Run an action; if it fails (e.g. IP changed), re-discover once and retry.
fn with_retry<T>(cfg: &mut Config, f: impl Fn(&Config) -> Res<T>) -> Res<T> {
    match f(cfg) {
        Ok(v) => Ok(v),
        Err(first) => {
            let old = cfg.ip.clone();
            ensure_reachable(cfg, false).map_err(|_| first.clone())?;
            if cfg.ip == old { Err(first) } else { f(cfg) }
        }
    }
}

// ---------------------------------------------------------------- desktop integration

fn gvariant_str(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn gsettings(args: &[&str]) -> Res<String> {
    let out = Command::new("gsettings").args(args).output().map_err(|e| format!("gsettings: {e}"))?;
    if !out.status.success() {
        return Err(format!("gsettings {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn binding_list() -> Res<Vec<String>> {
    let raw = gsettings(&["get", GS_SCHEMA, "custom-keybindings"])?;
    Ok(raw.split('\'').skip(1).step_by(2).map(String::from).collect())
}

fn set_binding_list(list: &[String]) -> Res<()> {
    let v = format!("[{}]", list.iter().map(|s| gvariant_str(s)).collect::<Vec<_>>().join(", "));
    gsettings(&["set", GS_SCHEMA, "custom-keybindings", &v]).map(|_| ())
}

fn apply_shortcuts(cfg: &Config) -> Res<()> {
    let exe = shell_quote(&exe_path()?.to_string_lossy());
    let mut list = binding_list()?;
    let before = list.clone();
    let prefix = format!("{GS_BASE}/sonos-");
    let mut wanted = Vec::new();
    for (cmd, accel) in &cfg.shortcuts {
        if !BINDABLE.contains(&cmd.as_str()) {
            eprintln!("warning: shortcut for unknown command {cmd:?} ignored (valid: {})", BINDABLE.join(", "));
            continue;
        }
        if accel.trim().is_empty() {
            continue;
        }
        let path = format!("{prefix}{cmd}/");
        wanted.push(path.clone());
        let schema = format!("{GS_SCHEMA}.custom-keybinding:{path}");
        gsettings(&["set", &schema, "name", &gvariant_str(&format!("Sonos {cmd}"))])?;
        gsettings(&["set", &schema, "command", &gvariant_str(&format!("{exe} {cmd}"))])?;
        gsettings(&["set", &schema, "binding", &gvariant_str(accel)])?;
        if !list.contains(&path) {
            list.push(path);
        }
    }
    // Drop our bindings that are no longer in the config.
    for stale in list.iter().filter(|p| p.starts_with(&prefix) && !wanted.contains(p)) {
        reset_binding(stale);
    }
    list.retain(|p| !p.starts_with(&prefix) || wanted.contains(p));
    if list != before {
        set_binding_list(&list)?;
    }
    Ok(())
}

fn remove_shortcuts() -> Res<()> {
    let mut list = binding_list()?;
    let prefix = format!("{GS_BASE}/sonos-");
    for p in list.iter().filter(|p| p.starts_with(&prefix)) {
        reset_binding(p);
    }
    let n = list.len();
    list.retain(|p| !p.starts_with(&prefix));
    if list.len() != n {
        set_binding_list(&list)?;
    }
    Ok(())
}

fn reset_binding(path: &str) {
    let schema = format!("{GS_SCHEMA}.custom-keybinding:{path}");
    for key in ["name", "command", "binding"] {
        let _ = gsettings(&["reset", &schema, key]);
    }
}

fn autostart_file() -> Res<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or("cannot determine config dir")?;
    Ok(base.join("autostart").join("sonos.desktop"))
}

fn apply_autostart(cfg: &Config) -> Res<()> {
    let file = autostart_file()?;
    if !cfg.autostart {
        let _ = std::fs::remove_file(&file);
        return Ok(());
    }
    let exe = shell_quote(&exe_path()?.to_string_lossy());
    let content = format!(
        "[Desktop Entry]\nType=Application\nName=Sonos volume hotkeys\nComment=Find the Sonos speaker and register volume shortcuts\nExec={exe} refresh\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\nX-GNOME-Autostart-Delay=5\n"
    );
    std::fs::create_dir_all(file.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&file, content).map_err(|e| format!("cannot write {}: {e}", file.display()))
}

fn apply_desktop(cfg: &Config) {
    match apply_shortcuts(cfg) {
        Ok(()) => {
            println!("shortcuts:");
            for (cmd, accel) in cfg.shortcuts.iter().filter(|(_, a)| !a.trim().is_empty()) {
                println!("  {cmd:<11} {accel}");
            }
        }
        Err(e) => eprintln!("warning: could not set GNOME shortcuts: {e}"),
    }
    match apply_autostart(cfg) {
        Ok(()) if cfg.autostart => println!("autostart: {}", autostart_file().map(|p| p.display().to_string()).unwrap_or_default()),
        Ok(()) => println!("autostart: disabled"),
        Err(e) => eprintln!("warning: could not set autostart: {e}"),
    }
}

// ---------------------------------------------------------------- commands

fn prompt(q: &str) -> String {
    print!("{q}");
    io::stdout().flush().ok();
    let mut s = String::new();
    io::stdin().lock().read_line(&mut s).ok();
    s.trim().to_string()
}

fn cmd_init(args: &[String]) -> Res<()> {
    let mut cfg = load_config()?.unwrap_or_default();
    let ip_arg = args.iter().position(|a| a == "--ip").and_then(|i| args.get(i + 1));
    let no_desktop = args.iter().any(|a| a == "--no-desktop");

    let dev = if let Some(ip) = ip_arg {
        describe(ip, Duration::from_secs(3))?
    } else {
        println!("Searching for Sonos speakers…");
        let devs = discover(Duration::from_millis(1500))?;
        match devs.len() {
            0 => return Err("no Sonos speakers found (try `sonos init --ip <address>`)".into()),
            1 => devs[0].clone(),
            _ => {
                for (i, d) in devs.iter().enumerate() {
                    println!("  [{}] {} — {} ({})", i + 1, d.name, d.model, d.ip);
                }
                let pick: usize = prompt(&format!("Choose speaker [1-{}]: ", devs.len()))
                    .parse()
                    .ok()
                    .filter(|n| (1..=devs.len()).contains(n))
                    .ok_or("invalid choice")?;
                devs[pick - 1].clone()
            }
        }
    };
    println!("Using {} — {} at {}", dev.name, dev.model, dev.ip);
    cfg.ip = dev.ip;
    cfg.uuid = dev.uuid;
    cfg.name = dev.name;
    cfg.model = dev.model;
    save_config(&cfg)?;
    println!("config: {}", config_path()?.display());
    if !no_desktop {
        apply_desktop(&cfg);
    }
    match get_volume(&cfg) {
        Ok(v) => println!("current volume: {v}"),
        Err(e) => eprintln!("warning: {e}"),
    }
    Ok(())
}

fn parse_amount(args: &[String], default: i32) -> Res<i32> {
    match args.first() {
        Some(a) => a.parse::<i32>().map_err(|_| format!("not a number: {a}")).map(i32::abs),
        None => Ok(default),
    }
}

fn run(args: Vec<String>) -> Res<()> {
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let rest = &args[args.len().min(1)..];
    match cmd {
        "init" => cmd_init(rest),
        "up" | "down" => {
            let mut cfg = require_config()?;
            let n = parse_amount(rest, cfg.step)?;
            let delta = if cmd == "up" { n } else { -n };
            let v = with_retry(&mut cfg, |c| change_volume(c, delta))?;
            println!("{v}");
            Ok(())
        }
        "set" => {
            let mut cfg = require_config()?;
            let v: i32 = rest.first().and_then(|s| s.parse().ok()).ok_or("usage: sonos set <0-100>")?;
            println!("{}", with_retry(&mut cfg, |c| set_volume(c, v))?);
            Ok(())
        }
        "get" | "volume" => {
            let mut cfg = require_config()?;
            println!("{}", with_retry(&mut cfg, get_volume)?);
            Ok(())
        }
        "mute" => {
            let mut cfg = require_config()?;
            let m = with_retry(&mut cfg, |c| {
                let m = !get_mute(c)?;
                set_mute(c, m)?;
                Ok(m)
            })?;
            println!("{}", if m { "muted" } else { "unmuted" });
            Ok(())
        }
        "play-pause" | "toggle" => {
            let mut cfg = require_config()?;
            let playing = with_retry(&mut cfg, play_pause)?;
            println!("{}", if playing { "playing" } else { "paused" });
            Ok(())
        }
        "play" | "pause" | "next" | "previous" | "prev" => {
            let mut cfg = require_config()?;
            with_retry(&mut cfg, |c| match cmd {
                "play" => play(c),
                "pause" => pause(c),
                "next" => soap_av(c, "Next", "").map(|_| ()),
                _ => soap_av(c, "Previous", "").map(|_| ()),
            })
        }
        "state" => {
            let mut cfg = require_config()?;
            println!("{}", with_retry(&mut cfg, transport_state)?);
            Ok(())
        }
        "refresh" => {
            let mut cfg = require_config()?;
            let r = ensure_reachable(&mut cfg, true);
            apply_desktop(&cfg);
            r.map(|_| println!("speaker: {} at {}", cfg.name, cfg.ip))
        }
        "discover" => {
            for d in discover(Duration::from_millis(1500))? {
                println!("{}\t{}\t{}\t{}", d.ip, d.name, d.model, d.uuid);
            }
            Ok(())
        }
        "uninstall" => {
            remove_shortcuts()?;
            let _ = std::fs::remove_file(autostart_file()?);
            println!("removed shortcuts and autostart entry (config kept)");
            Ok(())
        }
        "help" | "-h" | "--help" => {
            print!(
                "sonos — control your Sonos speaker

USAGE:
  sonos init [--ip ADDR] [--no-desktop]
                    find the speaker, write {CONFIG_NAME} next to this binary,
                    register GNOME shortcuts (\"shortcuts\" in config) and
                    the login autostart entry
  sonos up [N]      raise volume by N (default: step from config)
  sonos down [N]    lower volume by N
  sonos set N       set volume to N
  sonos get         print current volume
  sonos mute        toggle mute
  sonos play-pause  toggle play/pause   (also: play, pause)
  sonos next        next track
  sonos previous    previous track
  sonos state       print playback state
  sonos refresh     re-find the speaker if its IP changed; re-apply shortcuts
                    and autostart from the config (runs at login)
  sonos discover    list Sonos speakers on the network
  sonos uninstall   remove shortcuts and autostart entry

Config: {}
",
                config_path().map(|p| p.display().to_string()).unwrap_or_default()
            );
            Ok(())
        }
        other => Err(format!("unknown command `{other}` (see `sonos help`)")),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(args) {
        eprintln!("sonos: {e}");
        std::process::exit(1);
    }
}
