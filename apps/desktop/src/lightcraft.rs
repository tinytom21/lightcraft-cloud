//! A small client for LightCraft's control channel (`lightcraft --control PORT`, docs/control-protocol.md in LightCraft):
//! JSON lines on `127.0.0.1:PORT`, one request per line and one reply per line.
//!
//! The app uses it for one thing: asking a running LightCraft to **add** photos that are already in the photo folder
//! (`library.import`, mode `add`: the library keeps a reference to each file where it is; nothing is copied, moved or deleted).
//! What this module will and will not do:
//!
//! * It sends two kinds of request and no others: `engine.commands` (to find out that what answers is LightCraft) and
//!   `engine.execute` with `library.import` and `{paths, mode: "add"}`. [`allowed`] checks every line before it is written
//!   and refuses anything else, so a change that adds a library-changing command fails there and in the tests.
//! * It never names a library, never starts LightCraft and never passes `--library`: it only talks to a LightCraft someone
//!   else started, with the library that LightCraft has open.
//! * Nothing here can block for long: the connect, each read and the whole wait for a reply have deadlines, a reply line is
//!   size-limited, and any error drops the connection (a late reply must never be taken for the answer to the next request).
//! * What answers is checked before an import is sent: the reply to `engine.commands` must be LightCraft's command list with
//!   `library.import` in it. Anything else (another program on that port, a web server) gets nothing more.
//!
//! Standard-library TCP only, so it works the same on Windows.

use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The port LightCraft's documentation uses, and this app's default (`lightcraft_control_port`; 0 turns the feature off).
pub const DEFAULT_PORT: u16 = 7980;

/// The longest reply line read. `engine.commands` is about 100 KB; this is far above it and far below a memory problem.
const MAX_LINE: usize = 8 << 20;

/// Why a request did not work. Every one leaves the photos queued; they differ in what is worth telling the user.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkError {
    /// Nothing answered, the connection closed, or it timed out: LightCraft is closed or the channel is off.
    Unreachable(String),
    /// Something answered, but it is not LightCraft (or not one this client understands). Nothing was sent to it.
    NotLightCraft(String),
    /// LightCraft answered and said import is not possible right now.
    NotReady(String),
    /// LightCraft refused the request as a whole.
    Refused(String),
    /// The reply is not what the protocol says.
    Protocol(String),
}

impl LinkError {
    pub fn text(&self) -> &str {
        match self {
            LinkError::Unreachable(m) | LinkError::NotLightCraft(m) | LinkError::NotReady(m) | LinkError::Refused(m) | LinkError::Protocol(m) => m,
        }
    }

    /// The port has not answered (as opposed to LightCraft answering with a problem).
    pub fn is_unreachable(&self) -> bool {
        matches!(self, LinkError::Unreachable(_) | LinkError::NotLightCraft(_))
    }
}

/// LightCraft's reply to `library.import`. It names the photos that were duplicates and the ones that failed, but only counts
/// the ones it added (their photo ids): see [`crate::pending::classify`] for what that allows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportReply {
    /// How many photos were added.
    pub imported: usize,
    /// Paths of photos the library already had.
    pub duplicates: Vec<String>,
    /// Paths that could not be added, with LightCraft's reason.
    pub failed: Vec<(String, String)>,
    /// How many files LightCraft looked at, when it says.
    pub scanned: Option<usize>,
}

/// What the bridge needs from LightCraft. The real one is [`TcpLink`]; tests use a fake or a fake server.
pub trait LightCraftLink {
    /// Connect and check that what answers is LightCraft (and that it can import). Must be called before [`import_add`].
    fn identify(&mut self) -> Result<(), LinkError>;
    /// Ask LightCraft to add these photos (absolute paths of files) to the library it has open, where they are.
    fn import_add(&mut self, paths: &[String]) -> Result<ImportReply, LinkError>;
}

/// The two requests this client can make.
enum Request<'a> {
    Commands,
    ImportAdd(&'a [String]),
}

fn request_value(req: &Request, id: u64) -> Value {
    match req {
        Request::Commands => json!({"id": id, "method": "engine.commands"}),
        Request::ImportAdd(paths) => {
            json!({"id": id, "method": "engine.execute", "params": {"command": "library.import", "params": {"paths": paths, "mode": "add"}}})
        }
    }
}

/// Whether a request line is one of the two this client may send: `engine.commands` with nothing else, or `library.import` with
/// only `paths` (a list of text) and `mode` `add`. Everything that writes goes through this.
pub fn allowed(line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return false };
    let Some(o) = v.as_object() else { return false };
    if !o.keys().all(|k| matches!(k.as_str(), "id" | "method" | "params")) {
        return false;
    }
    match o.get("method").and_then(Value::as_str) {
        Some("engine.commands") => !o.contains_key("params"),
        Some("engine.execute") => {
            let Some(p) = o.get("params").and_then(Value::as_object) else { return false };
            if p.len() != 2 || p.get("command").and_then(Value::as_str) != Some("library.import") {
                return false;
            }
            let Some(q) = p.get("params").and_then(Value::as_object) else { return false };
            q.len() == 2
                && q.get("mode").and_then(Value::as_str) == Some("add")
                && q.get("paths").and_then(Value::as_array).is_some_and(|a| !a.is_empty() && a.iter().all(Value::is_string))
        }
        _ => false,
    }
}

/// The checks on `engine.commands`' reply that say "this is LightCraft": a list of command objects with `library.import` among
/// them (and enabled). Pure, for tests.
pub fn check_commands(result: &Value) -> Result<(), LinkError> {
    let Some(list) = result.as_array() else { return Err(LinkError::NotLightCraft("the reply to engine.commands is not a list".into())) };
    let has = |id: &str| list.iter().find(|c| c.get("id").and_then(Value::as_str) == Some(id));
    // two ids that belong to LightCraft's engine, so a program that merely echoes "library.import" is not enough
    if has("library.folders").is_none() {
        return Err(LinkError::NotLightCraft("the command list is not LightCraft's (no library.folders)".into()));
    }
    match has("library.import") {
        None => Err(LinkError::NotLightCraft("the command list is not LightCraft's (no library.import)".into())),
        Some(c) if c.get("enabled").and_then(Value::as_bool) == Some(false) => Err(LinkError::NotReady("LightCraft cannot import right now".into())),
        Some(_) => Ok(()),
    }
}

/// Read `library.import`'s result. The three lists must be there: a reply without them is not trusted to say anything.
pub fn parse_import_result(result: &Value) -> Result<ImportReply, LinkError> {
    let bad = |what: &str| LinkError::Protocol(format!("the import reply has no usable {what}"));
    let o = result.as_object().ok_or_else(|| bad("result"))?;
    let imported = o.get("imported").and_then(Value::as_array).ok_or_else(|| bad("`imported` list"))?.len();
    let mut duplicates = Vec::new();
    for d in o.get("duplicates").and_then(Value::as_array).ok_or_else(|| bad("`duplicates` list"))? {
        duplicates.push(d.get("path").and_then(Value::as_str).ok_or_else(|| bad("duplicate path"))?.to_string());
    }
    let mut failed = Vec::new();
    for f in o.get("failed").and_then(Value::as_array).ok_or_else(|| bad("`failed` list"))? {
        let pair = f.as_array().filter(|a| a.len() == 2);
        match pair.and_then(|a| Some((a[0].as_str()?, a[1].as_str()?))) {
            Some((p, why)) => failed.push((p.to_string(), why.to_string())),
            None => return Err(bad("failed entry")),
        }
    }
    let scanned = o.get("scanned").and_then(Value::as_u64).map(|n| n as usize);
    Ok(ImportReply { imported, duplicates, failed, scanned })
}

/// How long each step may take.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub connect: Duration,
    /// The whole wait for the reply to `engine.commands`.
    pub identify: Duration,
    /// The whole wait for the reply to an import. LightCraft answers on its UI thread (up to 60 s when it is busy); a batch is
    /// small, and giving up only means the photos are asked about again (they then come back as duplicates).
    pub import: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts { connect: Duration::from_millis(800), identify: Duration::from_secs(5), import: Duration::from_secs(30) }
    }
}

struct Conn {
    stream: TcpStream,
    /// Bytes read past the end of the last line.
    buf: Vec<u8>,
}

/// The real thing: loopback TCP to a LightCraft started with its control port on.
pub struct TcpLink {
    port: u16,
    timeouts: Timeouts,
    conn: Option<Conn>,
    next_id: u64,
}

impl TcpLink {
    pub fn new(port: u16) -> TcpLink {
        TcpLink::with_timeouts(port, Timeouts::default())
    }

    pub fn with_timeouts(port: u16, timeouts: Timeouts) -> TcpLink {
        TcpLink { port, timeouts, conn: None, next_id: 1 }
    }

    /// One request, one reply, with the deadline. Any error drops the connection.
    fn call(&mut self, req: &Request, wait: Duration) -> Result<Value, LinkError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = request_value(req, id).to_string();
        // the one gate every request passes
        if !allowed(&line) {
            return Err(LinkError::Refused("this request is not one the app may send".into()));
        }
        line.push('\n');
        let result = self.exchange(&line, id, wait);
        if result.is_err() {
            self.conn = None;
        }
        result
    }

    fn exchange(&mut self, line: &str, id: u64, wait: Duration) -> Result<Value, LinkError> {
        let deadline = Instant::now() + wait;
        let conn = self.conn.as_mut().ok_or_else(|| LinkError::Unreachable("not connected".into()))?;
        conn.stream.set_write_timeout(Some(wait.max(Duration::from_millis(1)))).map_err(io_err)?;
        conn.stream.write_all(line.as_bytes()).and_then(|()| conn.stream.flush()).map_err(io_err)?;
        let reply = read_line(conn, deadline)?;
        let v: Value = serde_json::from_str(&reply).map_err(|_| LinkError::NotLightCraft("the reply is not JSON".into()))?;
        let Some(o) = v.as_object() else { return Err(LinkError::NotLightCraft("the reply is not a JSON object".into())) };
        if o.get("closed").is_some() {
            // (the idle notice has no id; the connection is closing)
            return Err(LinkError::Unreachable("LightCraft closed the connection".into()));
        }
        if o.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(LinkError::NotLightCraft("the reply does not carry the request's id".into()));
        }
        match o.get("ok").and_then(Value::as_bool) {
            Some(true) => Ok(o.get("result").cloned().unwrap_or(Value::Null)),
            Some(false) => Err(LinkError::Refused(o.get("error").and_then(Value::as_str).unwrap_or("no reason given").to_string())),
            None => Err(LinkError::NotLightCraft("the reply has no ok".into())),
        }
    }
}

fn io_err(e: std::io::Error) -> LinkError {
    LinkError::Unreachable(match e.kind() {
        ErrorKind::TimedOut | ErrorKind::WouldBlock => "no answer in time".to_string(),
        _ => e.to_string(),
    })
}

/// A line from the connection, within the deadline as a whole (a peer sending one byte at a time does not stretch it).
fn read_line(conn: &mut Conn, deadline: Instant) -> Result<String, LinkError> {
    loop {
        if let Some(pos) = conn.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = conn.buf.drain(..=pos).collect();
            return String::from_utf8(line[..pos].to_vec()).map_err(|_| LinkError::NotLightCraft("the reply is not text".into()));
        }
        if conn.buf.len() > MAX_LINE {
            return Err(LinkError::NotLightCraft("the reply is too long".into()));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(LinkError::Unreachable("no answer in time".into()));
        }
        conn.stream.set_read_timeout(Some(left.max(Duration::from_millis(1)))).map_err(io_err)?;
        let mut chunk = [0u8; 16 * 1024];
        match conn.stream.read(&mut chunk) {
            Ok(0) => return Err(LinkError::Unreachable("the connection was closed".into())),
            Ok(n) => conn.buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(io_err(e)),
        }
    }
}

impl LightCraftLink for TcpLink {
    fn identify(&mut self) -> Result<(), LinkError> {
        self.conn = None;
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let stream = TcpStream::connect_timeout(&addr, self.timeouts.connect).map_err(io_err)?;
        let _ = stream.set_nodelay(true);
        self.conn = Some(Conn { stream, buf: Vec::new() });
        let r = self.call(&Request::Commands, self.timeouts.identify).and_then(|v| check_commands(&v));
        if r.is_err() {
            self.conn = None;
        }
        r
    }

    fn import_add(&mut self, paths: &[String]) -> Result<ImportReply, LinkError> {
        if self.conn.is_none() {
            return Err(LinkError::Unreachable("not identified".into()));
        }
        if paths.is_empty() {
            return Ok(ImportReply::default());
        }
        let v = self.call(&Request::ImportAdd(paths), self.timeouts.import)?;
        parse_import_result(&v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A fake LightCraft on a loopback port: answers each line with `reply(line)`, and keeps every line it was sent.
    struct Fake {
        port: u16,
        seen: Arc<Mutex<Vec<String>>>,
    }

    fn serve(reply: impl Fn(&Value) -> Option<String> + Send + 'static) -> Fake {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        std::thread::spawn(move || {
            for stream in l.incoming().flatten() {
                let mut w = stream.try_clone().unwrap();
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    s.lock().unwrap().push(line.clone());
                    let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    match reply(&v) {
                        Some(r) => {
                            let _ = writeln!(w, "{r}");
                        }
                        None => std::thread::sleep(Duration::from_secs(30)),
                    }
                }
            }
        });
        Fake { port, seen }
    }

    fn commands_reply(id: &Value, extra: &[&str], enabled: bool) -> String {
        let mut list: Vec<Value> = ["library.folders", "library.state"].iter().chain(extra).map(|i| json!({"id": i, "label": i, "enabled": true})).collect();
        list.push(json!({"id": "library.import", "label": "Import Photos", "enabled": enabled}));
        json!({"id": id, "ok": true, "result": list}).to_string()
    }

    /// LightCraft as the spike saw it: ids for what it added, paths for duplicates and failures.
    fn lightcraft(v: &Value) -> Option<String> {
        let id = &v["id"];
        Some(match v["method"].as_str() {
            Some("engine.commands") => commands_reply(id, &[], true),
            Some("engine.execute") => {
                let paths: Vec<&str> = v["params"]["params"]["paths"].as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect();
                let (mut imported, mut dups, mut failed) = (vec![], vec![], vec![]);
                for (i, p) in paths.iter().enumerate() {
                    if p.ends_with("dup.jpg") {
                        dups.push(json!({"existing": 1, "path": p, "reason": "path"}));
                    } else if p.ends_with("gone.jpg") {
                        failed.push(json!([p, format!("{p}: No such file or directory (os error 2)")]));
                    } else {
                        imported.push(i + 10);
                    }
                }
                json!({"id": id, "ok": true, "result": {"duplicates": dups, "failed": failed, "imported": imported, "scanned": paths.len(), "sidecars": 0}}).to_string()
            }
            _ => json!({"id": id, "ok": false, "error": "unknown method"}).to_string(),
        })
    }

    fn quick() -> Timeouts {
        Timeouts { connect: Duration::from_millis(300), identify: Duration::from_millis(400), import: Duration::from_millis(400) }
    }

    #[test]
    fn identifies_lightcraft_and_reads_the_import_reply_the_spike_saw() {
        let f = serve(lightcraft);
        let mut l = TcpLink::with_timeouts(f.port, quick());
        l.identify().unwrap();
        let r = l.import_add(&["/p/a.jpg".into(), "/p/dup.jpg".into(), "/p/gone.jpg".into()]).unwrap();
        assert_eq!(r.imported, 1);
        assert_eq!(r.duplicates, vec!["/p/dup.jpg".to_string()]);
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.failed[0].0, "/p/gone.jpg");
        assert!(r.failed[0].1.contains("No such file"));
        assert_eq!(r.scanned, Some(3));
        let seen = f.seen.lock().unwrap().clone();
        assert_eq!(seen[0], r#"{"id":1,"method":"engine.commands"}"#);
        let sent: Value = serde_json::from_str(&seen[1]).unwrap();
        assert_eq!(
            sent,
            json!({"id": 2, "method": "engine.execute", "params": {"command": "library.import", "params": {"paths": ["/p/a.jpg", "/p/dup.jpg", "/p/gone.jpg"], "mode": "add"}}})
        );
    }

    #[test]
    fn the_client_cannot_send_anything_that_changes_the_library_except_import_in_add_mode() {
        // 1. the gate: only the two shapes pass
        assert!(allowed(r#"{"id":1,"method":"engine.commands"}"#));
        let ok = |mode: &str, extra: &str| {
            format!(r#"{{"id":2,"method":"engine.execute","params":{{"command":"library.import","params":{{"paths":["/a.jpg"],"mode":"{mode}"{extra}}}}}}}"#)
        };
        assert!(allowed(&ok("add", "")));
        for (mode, extra) in [("copy", ""), ("move", ""), ("", ""), ("add", r#","album":"x""#), ("add", r#","destination":"/lib""#), ("add", r#","local":false"#)] {
            assert!(!allowed(&ok(mode, extra)), "{mode} {extra}");
        }
        for bad in [
            r#"{"id":1,"method":"engine.execute","params":{"command":"photo.delete","params":{}}}"#,
            r#"{"id":1,"method":"engine.execute","params":{"command":"app.openLibrary","params":{"path":"/x"}}}"#,
            r#"{"id":1,"method":"engine.execute","params":{"command":"library.import","params":{"mode":"add"}}}"#,
            r#"{"id":1,"method":"engine.execute","params":{"command":"library.import","params":{"paths":[],"mode":"add"}}}"#,
            r#"{"id":1,"method":"engine.execute","params":{"command":"library.import","params":{"paths":[1],"mode":"add"}}}"#,
            r#"{"id":1,"method":"engine.execute","params":{"command":"library.import","extra":1,"params":{"paths":["/a"],"mode":"add"}}}"#,
            r#"{"id":1,"method":"engine.commands","params":{"x":1}}"#,
            r#"{"id":1,"method":"ui.menu.invoke","params":{"command":"library.import"}}"#,
            r#"{"id":1,"method":"engine.execute","params":{"command":"library.import","params":{"paths":["/a"],"mode":"add"}},"other":1}"#,
            "not json",
        ] {
            assert!(!allowed(bad), "{bad}");
        }
        // 2. everything this client really writes, over the wire, passes the gate and is one of the two
        let f = serve(lightcraft);
        let mut l = TcpLink::with_timeouts(f.port, quick());
        l.identify().unwrap();
        l.import_add(&["/p/a.jpg".into()]).unwrap();
        l.import_add(&[]).unwrap();
        let _ = l.import_add(&["/p/dup.jpg".into(), "/p/gone.jpg".into()]);
        let seen = f.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3, "an empty batch sends nothing: {seen:?}");
        for line in &seen {
            assert!(allowed(line), "{line}");
        }
        assert_eq!(seen.iter().filter(|l| l.contains("engine.commands")).count(), 1);
        assert!(seen[1..].iter().all(|l| l.contains(r#""command":"library.import""#) && l.contains(r#""mode":"add""#)));
    }

    #[test]
    fn something_that_is_not_lightcraft_gets_one_probe_and_no_import() {
        // a server that answers every line with a valid-looking reply that is not LightCraft's command list
        let f = serve(|v| Some(json!({"id": v["id"], "ok": true, "result": [{"id": "something.else"}]}).to_string()));
        let mut l = TcpLink::with_timeouts(f.port, quick());
        assert!(matches!(l.identify(), Err(LinkError::NotLightCraft(_))));
        assert!(matches!(l.import_add(&["/p/a.jpg".into()]), Err(LinkError::Unreachable(_))), "not connected after a failed identify");
        assert_eq!(f.seen.lock().unwrap().len(), 1, "only the probe was sent");
        // text, a web server, a wrong id
        for answer in ["HTTP/1.1 400 Bad Request", "[1,2,3]", r#"{"id":99,"ok":true,"result":[]}"#, r#"{"ok":true}"#] {
            let f = serve(move |_| Some(answer.to_string()));
            let mut l = TcpLink::with_timeouts(f.port, quick());
            assert!(matches!(l.identify(), Err(LinkError::NotLightCraft(_))), "{answer}");
            assert_eq!(f.seen.lock().unwrap().len(), 1);
        }
        // LightCraft's command list but import switched off
        let f = serve(|v| Some(commands_reply(&v["id"], &[], false)));
        assert!(matches!(TcpLink::with_timeouts(f.port, quick()).identify(), Err(LinkError::NotReady(_))));
    }

    #[test]
    fn a_closed_port_a_silent_port_and_a_slow_trickle_all_give_up_in_time() {
        // closed
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let t = Instant::now();
        assert!(matches!(TcpLink::with_timeouts(port, quick()).identify(), Err(LinkError::Unreachable(_))));
        assert!(t.elapsed() < Duration::from_secs(2));
        // silent: accepts and never answers
        let f = serve(|_| None);
        let t = Instant::now();
        assert!(matches!(TcpLink::with_timeouts(f.port, quick()).identify(), Err(LinkError::Unreachable(_))));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        // a peer that sends a byte every 100 ms and never ends the line is cut off at the deadline as a whole
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                for _ in 0..100 {
                    if s.write_all(b"x").is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        let t = Instant::now();
        assert!(TcpLink::with_timeouts(port, quick()).identify().is_err());
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }

    #[test]
    fn a_reply_that_never_ends_or_arrives_in_pieces_is_handled() {
        // in pieces: fine
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut w = s.try_clone().unwrap();
                let mut line = String::new();
                BufReader::new(s).read_line(&mut line).unwrap();
                let v: Value = serde_json::from_str(&line).unwrap();
                let r = commands_reply(&v["id"], &[], true);
                let (a, b) = r.split_at(r.len() / 2);
                w.write_all(a.as_bytes()).unwrap();
                std::thread::sleep(Duration::from_millis(50));
                writeln!(w, "{b}").unwrap();
            }
        });
        TcpLink::with_timeouts(port, quick()).identify().unwrap();
    }

    #[test]
    fn lightcraft_refusing_the_import_or_closing_the_idle_connection_is_an_error_and_drops_the_connection() {
        let f = serve(|v| {
            Some(if v["method"] == "engine.commands" {
                commands_reply(&v["id"], &[], true)
            } else {
                json!({"id": v["id"], "ok": false, "error": "invalid parameters for `library.import`: no paths"}).to_string()
            })
        });
        let mut l = TcpLink::with_timeouts(f.port, quick());
        l.identify().unwrap();
        match l.import_add(&["/a.jpg".into()]) {
            Err(LinkError::Refused(m)) => assert!(m.contains("no paths")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(l.import_add(&["/a.jpg".into()]), Err(LinkError::Unreachable(_))), "the connection is gone after an error");
        let f = serve(|_| Some(r#"{"ok":false,"error":"control connection idle read timed out; closing the connection","closed":"idle"}"#.into()));
        assert!(matches!(TcpLink::with_timeouts(f.port, quick()).identify(), Err(LinkError::Unreachable(_))));
    }

    #[test]
    fn an_import_reply_without_its_lists_is_not_trusted() {
        for r in [json!(null), json!({}), json!({"imported": [1]}), json!({"imported": [1], "duplicates": []}), json!({"imported": 1, "duplicates": [], "failed": []}), json!({"imported": [], "duplicates": [{"x": 1}], "failed": []}), json!({"imported": [], "duplicates": [], "failed": [["a"]]})] {
            assert!(matches!(parse_import_result(&r), Err(LinkError::Protocol(_))), "{r}");
        }
        let ok = parse_import_result(&json!({"imported": [1, 2], "duplicates": [], "failed": []})).unwrap();
        assert_eq!((ok.imported, ok.scanned), (2, None));
    }

    #[test]
    fn the_bridge_never_starts_lightcraft_names_a_library_or_uses_the_engine() {
        for (name, src) in [("lightcraft.rs", include_str!("lightcraft.rs")), ("pending.rs", include_str!("pending.rs"))] {
            // (the needles are split so that this test does not find itself)
            let code: String = src.split("#[cfg(test)]").next().unwrap().lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
            for needle in [["--", "library"].concat(), ["Command", "::new"].concat(), ["std::", "process"].concat(), ["lightcraft_", "engine"].concat(), ["LIGHTCRAFT", "_LIBRARY"].concat(), ["Pictures/", "LightCraft"].concat()] {
                assert!(!code.contains(&needle), "{name} mentions {needle}");
            }
        }
    }
}
