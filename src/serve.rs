//! `amux serve`: amux on your phone. A small web app, served by amux itself, that lists
//! the runs (on this machine and the others), shows each one's screen, and lets you reply,
//! press a key (Yes, No, Esc, ↑…) or stop it. Add it to the home screen to use it like an
//! app.
//!
//! It is reached with a secret token: the link `amux serve` prints sets it as a cookie
//! once. By default it listens on this machine only (127.0.0.1); reach it from a phone
//! through something private such as Tailscale, a VPN or an SSH tunnel. `--tailscale`
//! puts it behind Tailscale's HTTPS, with a real certificate; `--tls` serves HTTPS itself
//! (self-signed, or `--cert`/`--key`). Plain HTTP on an open network would show the token
//! to anyone listening.
//!
//! Every action runs the same `amux` commands the CLI has (`ps --json`, `peek`, `send`,
//! `send --key`, `stop`), so runs on other machines work too, and there is one code path.

use crate::tls::Conn;
use crate::{fsx, paths};
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const HTML: &str = include_str!("assets/app.html");
const JS: &str = include_str!("assets/app.js");
const CSS: &str = include_str!("assets/app.css");
const SW: &str = include_str!("assets/sw.js");
const ICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" rx="14" fill="#16161e"/><text x="32" y="42" font-size="28" text-anchor="middle" fill="#7aa2f7" font-family="monospace">ax</text></svg>"##;
const MANIFEST: &str = r##"{"name":"amux","short_name":"amux","start_url":"/","display":"standalone","background_color":"#16161e","theme_color":"#16161e","icons":[{"src":"/icon.svg","sizes":"any","type":"image/svg+xml"}]}"##;

const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;

/// The token, kept in ~/.amux/serve-token (0600) so a saved home-screen app keeps working.
fn token(renew: bool) -> Result<String> {
    let path = paths::AMUX_HOME.join("serve-token");
    if !renew {
        if let Ok(t) = std::fs::read_to_string(&path) {
            let t = t.trim().to_string();
            if t.len() >= 32 {
                return Ok(t);
            }
        }
    }
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("could not read /dev/urandom")?;
    let t: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    fsx::mkdir_private(&paths::AMUX_HOME)?;
    fsx::write_atomic(&path, t.as_bytes())?;
    Ok(t)
}

/// Compares in time that doesn't depend on where the strings differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

struct Request {
    method: String,
    path: String,
    query: String,
    cookie: Option<String>,
    bearer: Option<String>,
    amux_header: bool,
    json: bool,
    body: Vec<u8>,
}

fn read_request(stream: &Conn) -> Result<Request> {
    let mut r = BufReader::new(stream.take((MAX_HEAD + MAX_BODY) as u64));
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let mut req = Request {
        method,
        path,
        query,
        cookie: None,
        bearer: None,
        amux_header: false,
        json: false,
        body: vec![],
    };
    let mut len = 0usize;
    let mut head = line.len();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 {
            break;
        }
        head += h.len();
        if head > MAX_HEAD {
            bail!("headers too large");
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let Some((k, v)) = h.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.to_ascii_lowercase().as_str() {
            "content-length" => len = v.parse().unwrap_or(usize::MAX),
            "cookie" => {
                req.cookie = v
                    .split(';')
                    .filter_map(|c| c.trim().strip_prefix("amux="))
                    .map(str::to_string)
                    .next()
            }
            "authorization" => req.bearer = v.strip_prefix("Bearer ").map(str::to_string),
            "x-amux" => req.amux_header = v == "1",
            "content-type" => req.json = v.starts_with("application/json"),
            _ => {}
        }
    }
    if len > MAX_BODY {
        bail!("body too large");
    }
    req.body = vec![0; len];
    r.read_exact(&mut req.body)?;
    Ok(req)
}

fn respond(mut s: &Conn, status: &str, ctype: &str, extra: &[(&str, String)], body: &[u8]) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         X-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'self'; img-src 'self' data:; frame-ancestors 'none'\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
}

fn text(s: &Conn, status: &str, msg: &str) {
    respond(s, status, "text/plain; charset=utf-8", &[], msg.as_bytes());
}

/// Runs this amux with `args` and returns (success, stdout, stderr).
fn amux(args: &[&str]) -> (bool, String, String) {
    let out = std::env::current_exe().and_then(|exe| {
        Command::new(exe)
            .args(args)
            .env("AMUX_NO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .output()
    });
    match out {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Err(e) => (false, String::new(), e.to_string()),
    }
}

/// A run name from a URL: `machine:run` at most, in the characters run names use.
fn run_id(s: &str) -> Option<String> {
    let id = s.replace("%3A", ":").replace("%3a", ":");
    let ok = !id.is_empty()
        && id.len() <= 100
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))
        && !id.starts_with(['-', '.']);
    ok.then_some(id)
}

/// How this server is reached: its token, and whether over HTTPS (so the cookie is
/// marked Secure).
struct Site {
    token: String,
    https: bool,
    /// The self-signed certificate, offered at /amux-cert.pem to install on the phone.
    cert_pem: Option<&'static [u8]>,
}

fn accept(tcp: TcpStream, site: &Site, tls: Option<&Arc<rustls::ServerConfig>>) {
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(30)));
    let conn = match tls {
        Some(cfg) => match Conn::tls(tcp, cfg) {
            Ok(c) => c,
            Err(_) => return,
        },
        None => Conn::plain(tcp),
    };
    handle(&conn, site);
}

fn handle(stream: &Conn, site: &Site) {
    let token = site.token.as_str();
    let Ok(req) = read_request(stream) else {
        return text(stream, "400 Bad Request", "bad request");
    };
    let s = stream;
    // Public: what a phone fetches to install the app, which carries no secrets.
    match req.path.as_str() {
        "/manifest.webmanifest" => {
            return respond(
                s,
                "200 OK",
                "application/manifest+json",
                &[],
                MANIFEST.as_bytes(),
            )
        }
        "/icon.svg" => return respond(s, "200 OK", "image/svg+xml", &[], ICON.as_bytes()),
        // The service worker that shows push notifications (`amux phone webpush`).
        "/sw.js" => return respond(s, "200 OK", "text/javascript", &[], SW.as_bytes()),
        // The chosen theme's colors (`amux theme`); empty keeps the app's light/dark.
        "/theme.css" => {
            let css = crate::ui::theme::configured()
                .and_then(|n| crate::ui::theme::palette_of(&n))
                .map(|p| p.css())
                .unwrap_or_default();
            return respond(s, "200 OK", "text/css", &[], css.as_bytes());
        }
        "/amux-cert.pem" if site.cert_pem.is_some() => {
            let pem = site.cert_pem.unwrap_or_default();
            return respond(s, "200 OK", "application/x-x509-ca-cert", &[], pem);
        }
        _ => {}
    }
    // The link `amux serve` printed: remember the token as a cookie, then drop it from
    // the address bar so it isn't saved in history or shown on screen.
    if let Some(t) = req.query.split('&').find_map(|p| p.strip_prefix("t=")) {
        if same(t, token) {
            let secure = if site.https { "; Secure" } else { "" };
            let cookie = format!(
                "amux={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=31536000{secure}"
            );
            return respond(
                s,
                "303 See Other",
                "text/plain",
                &[("Set-Cookie", cookie), ("Location", "/".into())],
                b"",
            );
        }
    }
    let authed = [req.cookie.as_deref(), req.bearer.as_deref()]
        .into_iter()
        .flatten()
        .any(|t| same(t, token));
    if !authed {
        return text(
            s,
            "401 Unauthorized",
            "Open the link that `amux serve` printed (with ?t=…) on this device first.",
        );
    }
    if req.method == "POST" && !(req.amux_header && req.json) {
        // Only the app's own requests: a page elsewhere can't set this header.
        return text(s, "403 Forbidden", "missing X-Amux header");
    }
    let path = req.path.clone();
    let body: serde_json::Value =
        serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
    match (req.method.as_str(), path.as_str()) {
        ("GET", "/") => respond(
            s,
            "200 OK",
            "text/html; charset=utf-8",
            &[],
            HTML.as_bytes(),
        ),
        ("GET", "/app.js") => respond(s, "200 OK", "text/javascript", &[], JS.as_bytes()),
        ("GET", "/app.css") => respond(s, "200 OK", "text/css", &[], CSS.as_bytes()),
        ("GET", "/api/runs") => reply_json(s, amux(&["ps", "--json"])),
        ("GET", "/api/accounts") => reply_json(s, amux(&["account", "list", "--json"])),
        ("GET", "/api/slots") => reply_json(s, amux(&["slots", "--json"])),
        ("GET", "/api/overlap") => reply_json(s, amux(&["overlap", "--json"])),
        ("GET", "/api/usage") => reply_json(s, amux(&["usage", "--json"])),
        ("GET", "/api/themes") => reply_json(s, amux(&["theme", "--json"])),
        ("POST", "/api/theme") => match body["name"].as_str() {
            Some(n) if crate::store::valid_name(n) => reply_text(s, amux(&["theme", n])),
            _ => text(s, "400 Bad Request", "a theme name"),
        },
        ("GET", "/api/machines") => {
            let m: Vec<serde_json::Value> = crate::remote::list()
                .into_iter()
                .map(|m| serde_json::json!({ "name": m.name, "target": m.target }))
                .collect();
            json_ok(s, &serde_json::Value::from(m))
        }
        ("GET", "/api/dirs") => json_ok(s, &serde_json::Value::from(known_dirs())),
        ("GET", "/api/events") => events(stream),
        ("GET", "/api/push") => match crate::runs::webpush::public_key() {
            Ok(key) => {
                // `?endpoint=`: whether amux still has this browser's subscription, and so
                // whether waiting runs reach it by Web Push (the app then doesn't notify too).
                let endpoint = url::form_urlencoded::parse(req.query.as_bytes())
                    .find(|(k, _)| k == "endpoint")
                    .map(|(_, v)| v.into_owned());
                let subscribed = endpoint
                    .as_deref()
                    .is_some_and(crate::runs::webpush::subscribed);
                let phone = crate::runs::push::load();
                let active = subscribed
                    && phone
                        .as_ref()
                        .is_some_and(|p| p.service == "webpush" && p.wants("waiting"));
                json_ok(
                    s,
                    &serde_json::json!({
                        "publicKey": key,
                        "service": phone.map(|p| p.service),
                        "devices": crate::runs::webpush::load_subs().len(),
                        "subscribed": subscribed,
                        "active": active,
                    }),
                )
            }
            Err(e) => text(s, "500 Internal Server Error", &format!("{e:#}")),
        },
        ("POST", "/api/push") => push_subscribe(s, &body),
        ("POST", "/api/push/remove") => match body["endpoint"].as_str() {
            Some(e) => match crate::runs::webpush::remove(e) {
                Ok(_) => text(s, "200 OK", "This device no longer gets notifications."),
                Err(e) => text(s, "500 Internal Server Error", &format!("{e:#}")),
            },
            None => text(s, "400 Bad Request", "endpoint is missing"),
        },
        ("POST", "/api/spawn") => match spawn_args(&body) {
            Ok(args) => {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                reply_text(s, amux(&args))
            }
            Err(e) => text(s, "400 Bad Request", &e),
        },
        (m, p) if p.starts_with("/api/runs/") => {
            let rest = &p["/api/runs/".len()..];
            let Some((id, action)) = rest.rsplit_once('/') else {
                return text(s, "404 Not Found", "no such action");
            };
            let Some(id) = run_id(id) else {
                return text(s, "400 Bad Request", "bad run name");
            };
            run_action(s, m, &id, action, &body)
        }
        _ => text(s, "404 Not Found", "not found"),
    }
}

/// `POST /api/push`: a device's push subscription, as the browser gives it
/// (`{endpoint, keys: {p256dh, auth}}`). With no phone set up yet, this sets up Web Push.
fn push_subscribe(s: &Conn, body: &serde_json::Value) {
    use crate::runs::{push, webpush};
    let get = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
    let sub = webpush::validate(
        &get(&body["endpoint"]),
        &get(&body["keys"]["p256dh"]),
        &get(&body["keys"]["auth"]),
        &get(&body["device"]),
    );
    let sub = match sub {
        Ok(sub) => sub,
        Err(e) => return text(s, "400 Bad Request", &format!("{e:#}")),
    };
    if let Err(e) = webpush::add(sub) {
        return text(s, "500 Internal Server Error", &format!("{e:#}"));
    }
    let on = "Notifications from amux come to this device.";
    let msg = match push::load() {
        Some(p) if p.service == "webpush" => on.to_string(),
        Some(p) => format!(
            "Saved, but amux sends notifications to {} now. Run `amux phone webpush` on the computer to send them here.",
            p.service
        ),
        // phone.json is there but can't be read: say so rather than replace it.
        None if push::file().exists() => format!(
            "Saved, but {} can't be read, so amux doesn't know where it sends notifications. Run `amux phone webpush` on the computer to send them here.",
            crate::paths::tildify(&push::file())
        ),
        None => {
            let p = push::Phone {
                service: "webpush".into(),
                ..Default::default()
            };
            if let Err(e) = push::save(&p) {
                return text(s, "500 Internal Server Error", &format!("{e:#}"));
            }
            on.to_string()
        }
    };
    text(s, "200 OK", &msg)
}

fn json_ok(s: &Conn, v: &serde_json::Value) {
    respond(
        s,
        "200 OK",
        "application/json",
        &[],
        v.to_string().as_bytes(),
    )
}

fn reply_json(s: &Conn, (ok, out, err): (bool, String, String)) {
    if ok {
        respond(s, "200 OK", "application/json", &[], out.as_bytes())
    } else {
        text(s, "500 Internal Server Error", err.trim())
    }
}

/// The command's output, or the last line of its error as the reason.
fn reply_text(s: &Conn, (ok, out, err): (bool, String, String)) {
    if ok {
        let mut all = out;
        all.push_str(&err);
        respond(
            s,
            "200 OK",
            "text/plain; charset=utf-8",
            &[],
            all.trim().as_bytes(),
        )
    } else {
        let why = err
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("failed");
        text(s, "409 Conflict", why.trim_start_matches("amux: "))
    }
}

/// Starts amux in the background without waiting, for work that takes minutes (tests).
fn detach(args: &[&str]) {
    use std::os::unix::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut c = Command::new(exe);
    c.args(args)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    if let Ok(mut child) = c.spawn() {
        std::thread::spawn(move || child.wait());
    }
}

fn run_action(s: &Conn, method: &str, id: &str, action: &str, body: &serde_json::Value) {
    let str_of = |k: &str| body[k].as_str().map(str::trim).filter(|v| !v.is_empty());
    match (method, action) {
        ("GET", "screen") => reply_text(s, amux(&["peek", id])),
        ("GET", "diff") => reply_text(s, amux(&["diff", id])),
        ("GET", "verify") => reply_text(s, amux(&["verify", id])),
        ("GET", "checks") => {
            // Failing checks exit 1, which is still an answer.
            let (_, out, err) = amux(&["checks", id]);
            reply_text(s, (!out.is_empty(), out, err))
        }
        ("GET", "comments") => reply_json(s, amux(&["comments", id, "--json"])),
        ("POST", "send") => match str_of("text") {
            Some(t) => reply_text(s, amux(&["send", id, "--", t])),
            None => text(s, "400 Bad Request", "text is missing"),
        },
        ("POST", "key") => match str_of("key") {
            Some(k) if crate::runs::client::key_bytes(k).is_some() => {
                reply_text(s, amux(&["send", id, "--key", k]))
            }
            _ => text(s, "400 Bad Request", "unknown key"),
        },
        ("POST", "stop") => reply_text(s, amux(&["stop", id])),
        ("POST", "rm") => reply_text(s, amux(&["rm", id])),
        ("POST", "test") => {
            detach(&["verify", id, "--test"]);
            text(
                s,
                "202 Accepted",
                "Testing; it waits for a test slot if the machine is busy.",
            )
        }
        ("POST", "push") => reply_text(s, amux(&["verify", id, "--push"])),
        ("POST", "failover") => reply_text(s, amux(&["failover", id])),
        ("POST", "revive") => reply_text(s, amux(&["revive", id])),
        ("POST", "land") => {
            detach(&["land", id]);
            text(
                s,
                "202 Accepted",
                "Landing: rebase, test in a slot, fast-forward. The run shows how it went.",
            )
        }
        ("POST", "send-failures") => reply_text(s, amux(&["verify", id, "--send-failures"])),
        ("POST", "pr") => {
            let draft = body["draft"].as_bool() == Some(true);
            let mut args = vec!["pr", id];
            if draft {
                args.push("--draft");
            }
            reply_text(s, amux(&args))
        }
        ("POST", "checks-send") => reply_text(s, amux(&["checks", id, "--send-failures"])),
        ("POST", "comment") => {
            let (Some(file), Some(line), Some(note)) =
                (str_of("file"), body["line"].as_u64(), str_of("note"))
            else {
                return text(s, "400 Bad Request", "file, line and note are needed");
            };
            let at = format!("{file}:{line}");
            let mut args = vec!["comment", id, &at];
            if body["removed"].as_bool() == Some(true) {
                args.push("--removed");
            }
            args.push(note);
            reply_text(s, amux(&args))
        }
        ("POST", "comment-rm") => match body["n"].as_u64() {
            Some(n) => reply_text(s, amux(&["comments", id, "--rm", &n.to_string()])),
            None => text(s, "400 Bad Request", "n is missing"),
        },
        ("POST", "comments-send") => reply_text(s, amux(&["comments", id, "--send"])),
        _ => text(s, "404 Not Found", "no such action"),
    }
}

/// `amux spawn` arguments for a new run from the app. Each value goes in as one argument
/// (no shell), and spawn checks names, branches, folders and accounts itself.
fn spawn_args(b: &serde_json::Value) -> Result<Vec<String>, String> {
    let get = |k: &str| b[k].as_str().map(str::trim).filter(|v| !v.is_empty());
    let account = get("account").ok_or("Pick an account.")?;
    let mut args = vec!["spawn".to_string()];
    let dir = get("dir");
    match get("machine") {
        Some(m) => {
            if !crate::remote::valid_name(m) {
                return Err("bad machine name".into());
            }
            args.extend(["--on".into(), format!("{m}:{}", dir.unwrap_or("~"))]);
        }
        None => {
            if let Some(d) = dir {
                args.extend(["--dir".into(), d.to_string()]);
            }
        }
    }
    if let Some(n) = get("name") {
        args.extend(["--name".into(), n.to_string()]);
    }
    if b["worktree"].as_bool() == Some(true) {
        args.push("--worktree".into());
        if let Some(br) = get("branch") {
            args.extend(["--branch".into(), br.to_string()]);
        }
    }
    if let Some(p) = get("prompt") {
        if p.starts_with('-') {
            return Err("A prompt can't start with '-'.".into());
        }
        args.extend(["--prompt".into(), p.to_string()]);
    }
    if account.starts_with('-') {
        return Err("bad account".into());
    }
    args.push(account.to_string());
    Ok(args)
}

/// Folders runs were started in, newest first: where a new run probably goes.
fn known_dirs() -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut add = |d: &str| {
        let p = std::path::Path::new(d);
        // Skip amux's own worktrees: new runs start from the main checkout.
        if p.is_dir()
            && !p.starts_with(paths::worktrees_dir())
            && !out.iter().any(|o| o == d)
            && out.len() < 20
        {
            out.push(d.to_string());
        }
    };
    let mut runs = crate::runs::list();
    runs.reverse();
    for r in &runs {
        add(r.worktree.as_ref().map_or(&r.cwd, |w| &w.repo));
    }
    if let Ok(log) = std::fs::read_to_string(paths::launch_log()) {
        for line in log.lines().rev().take(500) {
            if let Some(cwd) = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v["cwd"].as_str().map(str::to_string))
            {
                add(&cwd);
            }
        }
    }
    out
}

/// One `amux events` for every phone watching: started for the first, stopped after the
/// last goes, its lines handed to each.
struct EventFeed {
    subs: Vec<(u64, std::sync::mpsc::Sender<String>)>,
    next: u64,
    child: Option<std::process::Child>,
}

static FEED: std::sync::Mutex<EventFeed> = std::sync::Mutex::new(EventFeed {
    subs: Vec::new(),
    next: 0,
    child: None,
});

fn feed() -> std::sync::MutexGuard<'static, EventFeed> {
    FEED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts receiving `amux events` lines; None if it can't be started.
fn subscribe() -> Option<(u64, std::sync::mpsc::Receiver<String>)> {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut f = feed();
    if f.child.is_none() {
        let exe = std::env::current_exe().ok()?;
        let mut child = Command::new(exe)
            .arg("events")
            .env("AMUX_NO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let out = child.stdout.take();
        let pid = child.id();
        f.child = Some(child);
        std::thread::spawn(move || {
            if let Some(out) = out {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let mut f = feed();
                    // Stopped and replaced: what it still had buffered is not for the
                    // phones watching the new one.
                    if !f.child.as_ref().is_some_and(|c| c.id() == pid) {
                        break;
                    }
                    f.subs.retain(|(_, t)| t.send(line.clone()).is_ok());
                }
            }
            // It ended: let the phones' streams end too, so they reconnect.
            let mut f = feed();
            if f.child.as_ref().is_some_and(|c| c.id() == pid) {
                f.subs.clear();
                if let Some(mut c) = f.child.take() {
                    let _ = c.wait();
                }
            }
        });
    }
    let id = f.next;
    f.next += 1;
    f.subs.push((id, tx));
    Some((id, rx))
}

fn unsubscribe(id: u64) {
    let mut f = feed();
    f.subs.retain(|(i, _)| *i != id);
    if f.subs.is_empty() {
        if let Some(mut c) = f.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// `GET /api/events`: Server-Sent Events from `amux events`, so the app updates the
/// moment a run changes. Ends when the phone goes away. All phones share one reader.
fn events(mut s: &Conn) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n\
                X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n: hi\n\n";
    if s.write_all(head.as_bytes()).is_err() {
        return;
    }
    let Some((id, rx)) = subscribe() else {
        return;
    };
    loop {
        // A comment every 20s keeps proxies and phones from closing an idle stream, and
        // finds out when the phone has gone.
        let msg = match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(line) => format!("data: {line}\n\n"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => ": ping\n\n".into(),
            Err(_) => break,
        };
        if s.write_all(msg.as_bytes()).and_then(|_| s.flush()).is_err() {
            break;
        }
    }
    unsubscribe(id);
}

const USAGE: &str =
    "amux serve [--listen <host:port>] [--tls | --cert <pem> --key <pem> | --tailscale] [--new-token]";

/// `--tailscale`: Tailscale serves https://<machine>.ts.net with a real certificate and
/// passes requests to amux on this machine. Stops when amux does.
fn start_tailscale(port: u16) -> Result<(String, std::process::Child)> {
    let Some(bin) = crate::tls::tailscale_bin() else {
        bail!("Tailscale isn't installed (tailscale.com/download), or use --tls.");
    };
    let name = crate::tls::tailscale_name(&bin)?;
    let mut child = Command::new(&bin)
        .args(["serve", "--https=443", &format!("http://127.0.0.1:{port}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run tailscale serve")?;
    // It fails at once when HTTPS isn't turned on for the tailnet.
    for _ in 0..15 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(st) = child.try_wait()? {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            bail!(
                "tailscale serve stopped ({st}): {}\nIt needs HTTPS certificates turned on for your tailnet (admin console → DNS → HTTPS Certificates).",
                err.trim()
            );
        }
    }
    Ok((format!("https://{name}"), child))
}

/// Prints a line, ignoring a closed stdout: the server keeps serving when whoever started
/// it stops reading (`amux serve | head`, a closed terminal).
macro_rules! say {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($t)*);
    }};
}

pub fn serve_cmd(argv: &[String]) -> Result<i32> {
    let mut listen = "127.0.0.1:7878".to_string();
    let (mut renew, mut tls, mut tailscale) = (false, false, false);
    let (mut cert, mut key) = (None, None);
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--listen" | "-l" => listen = crate::util::need(it.next(), USAGE)?.to_string(),
            "--new-token" => renew = true,
            "--tls" => tls = true,
            "--cert" => cert = Some(paths::expand_home(crate::util::need(it.next(), USAGE)?)),
            "--key" => key = Some(paths::expand_home(crate::util::need(it.next(), USAGE)?)),
            "--tailscale" => tailscale = true,
            _ => bail!("Unknown argument {a}. Usage: {USAGE}"),
        }
    }
    if cert.is_some() != key.is_some() {
        bail!("--cert and --key go together.");
    }
    tls |= cert.is_some();
    if tls && tailscale {
        bail!("--tailscale brings its own HTTPS: leave out --tls, --cert and --key.");
    }
    let token = token(renew)?;
    let listener =
        TcpListener::bind(&listen).with_context(|| format!("could not listen on {listen}"))?;
    let addr = listener.local_addr()?;
    let (ip, port) = (addr.ip(), addr.port());
    if tailscale && !ip.is_loopback() {
        bail!("With --tailscale, amux listens on this machine only (leave out --listen, or use 127.0.0.1:<port>).");
    }

    let mut self_signed = None;
    let config = if tls {
        let (c, k) = match (cert, key) {
            (Some(c), Some(k)) => (c, k),
            _ => {
                let names: Vec<String> = if ip.is_unspecified() || ip.is_loopback() {
                    vec![]
                } else {
                    vec![ip.to_string()]
                };
                let pair = crate::tls::self_signed(&names)?;
                self_signed = Some(pair.0.clone());
                pair
            }
        };
        Some(crate::tls::server_config(&c, &k)?)
    } else {
        None
    };
    let mut proxy = None;
    let base = if tailscale {
        let (url, child) = start_tailscale(port)?;
        proxy = Some(child);
        url
    } else {
        let scheme = if tls { "https" } else { "http" };
        if ip.is_unspecified() {
            format!("{scheme}://<this machine's address>:{port}")
        } else {
            format!("{scheme}://{addr}")
        }
    };

    say!("amux serve: open this on your phone, then add it to the home screen:\n");
    say!("  {base}/?t={token}\n");
    if tailscale {
        say!(
            "Served by Tailscale over HTTPS with a real certificate, to your tailnet's devices only.\n\
             Anyone with the link can control your runs: keep it private."
        );
    } else if let Some(c) = &self_signed {
        say!(
            "HTTPS with a self-signed certificate: the browser warns once. Check the fingerprint is\n  {}\n\
             To stop the warning, install {base}/amux-cert.pem on the phone and trust it\n\
             (iOS: Settings → General → About → Certificate Trust Settings).",
            crate::tls::fingerprint(c).unwrap_or_default()
        );
    } else if tls {
        say!("HTTPS with your certificate.");
    } else if ip.is_loopback() {
        say!(
            "It listens on this machine only. From a phone: `--tailscale` (HTTPS, nothing to set up\n\
             if you use Tailscale), `--listen <address>:{port} --tls`, or an SSH tunnel.\n\
             Anyone with the link can control your runs: keep it private."
        );
    } else {
        say!(
            "Warning: no TLS. Use this only on a private network, or add --tls (or --tailscale).\n\
             The link is a password; `amux serve --new-token` replaces it."
        );
    }
    // So phone notifications can link to the run here (`amux phone`).
    if !ip.is_unspecified() {
        let _ = crate::store::write_json(
            &paths::AMUX_HOME.join("serve.json"),
            &serde_json::json!({ "url": base }),
        );
    }
    let cert_pem: Option<&'static [u8]> = self_signed
        .and_then(|c| std::fs::read(c).ok())
        .map(|b| &*Box::leak(b.into_boxed_slice()));
    let site: &'static Site = Box::leak(Box::new(Site {
        token,
        https: tls || tailscale,
        cert_pem,
    }));
    let config: Option<&'static Arc<rustls::ServerConfig>> =
        config.map(|c| &*Box::leak(Box::new(c)));
    for stream in listener.incoming().flatten() {
        std::thread::spawn(move || accept(stream, site, config));
    }
    drop(proxy);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_tokens_and_run_names() {
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
        assert_eq!(run_id("box%3Aw").as_deref(), Some("box:w"));
        assert_eq!(run_id("claude-1").as_deref(), Some("claude-1"));
        assert!(run_id("../x").is_none() && run_id("-rf").is_none() && run_id("a b").is_none());
    }
}
