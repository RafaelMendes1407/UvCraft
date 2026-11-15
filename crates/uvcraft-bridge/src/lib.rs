//! Versioned loopback HTTP bridge. Each remote object has an isolated UvCraft
//! session. The CLI and desktop use the same parser and routing.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};
use uvcraft_commands::{Command, Session};
use uvcraft_core::EdgeKind;

const PROTOCOL: u64 = 1;
const MAX_BODY: usize = 16 * 1024 * 1024;
const MAX_HEADERS: usize = 16 * 1024;

struct RemoteSession {
    session: Session,
    path: PathBuf,
    object_id: String,
    addon_version: String,
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

struct State {
    token: String,
    sessions: HashMap<String, RemoteSession>,
    active_id: Option<String>,
    revision: u64,
}

impl State {
    fn new(token: String) -> Self {
        Self {
            token,
            sessions: HashMap::new(),
            active_id: None,
            revision: 0,
        }
    }

    fn changed(&mut self, id: Option<String>) {
        self.active_id = id;
        self.revision = self.revision.wrapping_add(1);
    }
}

/// A snapshot is copied out of the HTTP thread before the UI adopts it.
#[derive(Debug)]
pub struct BridgeSnapshot {
    pub session_id: String,
    pub object_id: String,
    pub revision: u64,
    pub session: Session,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeBusy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishError {
    Busy,
    Stale,
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(f, "bridge is busy; will retry after the current request"),
            Self::Stale => write!(f, "bridge session changed while the desktop was editing"),
        }
    }
}

/// Desktop bridge server. The listener stops when this handle is dropped.
pub struct BridgeHandle {
    state: Arc<Mutex<State>>,
    token: String,
    revision_cache: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    address: SocketAddr,
}

impl BridgeHandle {
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn token(&self) -> String {
        self.token.clone()
    }

    pub fn revision(&self) -> u64 {
        self.revision_cache.load(Ordering::Acquire)
    }

    pub fn session_count(&self) -> Option<usize> {
        self.state.try_lock().ok().map(|state| state.sessions.len())
    }

    /// Returns `BridgeBusy` while a remote command is running; the UI retries next frame.
    pub fn snapshot_after(&self, revision: u64) -> Result<Option<BridgeSnapshot>, BridgeBusy> {
        let state = self.state.try_lock().map_err(|_| BridgeBusy)?;
        if state.revision == revision {
            return Ok(None);
        }
        let Some(id) = state.active_id.as_ref() else {
            return Ok(None);
        };
        let Some(remote) = state.sessions.get(id) else {
            return Ok(None);
        };
        Ok(Some(BridgeSnapshot {
            session_id: id.clone(),
            object_id: remote.object_id.clone(),
            revision: state.revision,
            session: remote.session.clone(),
        }))
    }

    /// Publish desktop edits only if no newer remote request changed this session.
    pub fn publish(
        &self,
        session_id: &str,
        expected_revision: u64,
        session: &Session,
    ) -> Result<u64, PublishError> {
        let mut state = self.state.try_lock().map_err(|_| PublishError::Busy)?;
        if state.revision != expected_revision || state.active_id.as_deref() != Some(session_id) {
            return Err(PublishError::Stale);
        }
        let remote = state
            .sessions
            .get_mut(session_id)
            .ok_or(PublishError::Stale)?;
        remote.session = session.clone();
        state.changed(Some(session_id.to_owned()));
        self.revision_cache.store(state.revision, Ordering::Release);
        Ok(state.revision)
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    origin: Option<String>,
    body: Vec<u8>,
}

fn random_hex() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Resolves `bind` to a loopback socket address; any other interface is
/// rejected. Shared by the bridge and the JSON-RPC server.
pub fn loopback_addr(bind: &str) -> Result<SocketAddr, String> {
    let address = bind
        .to_socket_addrs()
        .map_err(|error| error.to_string())?
        .find(|address| address.ip().is_loopback())
        .ok_or_else(|| format!("{bind}: address must resolve to a loopback interface"))?;
    if !matches!(address.ip(), IpAddr::V4(_) | IpAddr::V6(_)) {
        return Err(format!("{bind}: invalid address"));
    }
    Ok(address)
}

pub fn serve(bind: &str) -> Result<(), String> {
    let address = loopback_addr(bind)?;
    let listener = TcpListener::bind(address).map_err(|error| error.to_string())?;
    let mut state = State::new(random_hex()?);
    eprintln!(
        "UvCraft bridge listening on {}",
        listener.local_addr().map_err(|error| error.to_string())?
    );
    eprintln!("UvCraft bridge token: {}", state.token);
    for incoming in listener.incoming() {
        match incoming {
            Ok(mut stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
                if let Err(error) = serve_client(&mut stream, &mut state) {
                    eprintln!("bridge request error: {error}");
                }
            }
            Err(error) => eprintln!("bridge accept error: {error}"),
        }
    }
    Ok(())
}

/// Starts the same HTTP bridge inside the desktop process. `on_change` only
/// requests a repaint; UI state is read through `snapshot_after` on the UI thread.
pub fn start_desktop(
    bind: &str,
    on_change: impl Fn() + Send + 'static,
) -> Result<BridgeHandle, String> {
    let listener = TcpListener::bind(loopback_addr(bind)?).map_err(|error| error.to_string())?;
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let token = random_hex()?;
    let state = Arc::new(Mutex::new(State::new(token.clone())));
    let revision_cache = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let state_for_thread = state.clone();
    let revision_for_thread = revision_cache.clone();
    let stop_for_thread = stop.clone();
    let worker = std::thread::spawn(move || {
        while !stop_for_thread.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
                    let request = read_request(&mut stream);
                    let (answer, changed) = match request {
                        Ok(request) => {
                            let mut state = state_for_thread.lock().expect("bridge state");
                            let before = state.revision;
                            let answer = route(&mut state, request);
                            let changed = state.revision != before;
                            revision_for_thread.store(state.revision, Ordering::Release);
                            (answer, changed)
                        }
                        Err(error) => (problem(400, "bad_request", error), false),
                    };
                    if let Err(error) = respond(&mut stream, answer.0, &answer.1) {
                        eprintln!("bridge request error: {error}");
                    }
                    if changed {
                        on_change();
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(30));
                }
                Err(error) => eprintln!("bridge accept error: {error}"),
            }
        }
    });
    Ok(BridgeHandle {
        state,
        token,
        revision_cache,
        stop,
        worker: Some(worker),
        address,
    })
}

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let mut reader = BufReader::new(stream);
    let first = read_line_limited(&mut reader, 4096)?;
    let mut parts = first.split_whitespace();
    let (Some(method), Some(path), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("invalid request line".into());
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err("unsupported HTTP version".into());
    }
    let (method, path) = (method.to_owned(), path.to_owned());
    let mut authorization = None;
    let mut origin = None;
    let mut content_length = None;
    let mut header_bytes = first.len();
    loop {
        let line = read_line_limited(&mut reader, MAX_HEADERS - header_bytes)?;
        header_bytes += line.len();
        if header_bytes > MAX_HEADERS {
            return Err("headers are too large".into());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line.split_once(':').ok_or("invalid header")?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("origin") {
            origin = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| "invalid Content-Length")?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("Transfer-Encoding is unsupported".into());
        }
    }
    let length = content_length.unwrap_or(0);
    if length > MAX_BODY {
        return Err("request body is too large".into());
    }
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| error.to_string())?;
    Ok(Request {
        method,
        path,
        authorization,
        origin,
        body,
    })
}

fn read_line_limited(reader: &mut impl BufRead, limit: usize) -> Result<String, String> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|error| error.to_string())?;
        if available.is_empty() {
            return Err("incomplete HTTP header".into());
        }
        let count = available
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(available.len(), |pos| pos + 1);
        if line.len() + count > limit {
            return Err("HTTP header is too large".into());
        }
        let complete = available[count - 1] == b'\n';
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if complete {
            break;
        }
    }
    String::from_utf8(line).map_err(|_| "HTTP header is not UTF-8".into())
}

fn respond(stream: &mut TcpStream, status: u16, value: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).expect("JSON response serializes");
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Content Too Large",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)
}

fn problem(status: u16, code: &str, message: impl AsRef<str>) -> (u16, Value) {
    (
        status,
        json!({"error":{"code":code,"message":message.as_ref()}}),
    )
}

fn serve_client(stream: &mut TcpStream, state: &mut State) -> Result<(), String> {
    let answer = match read_request(stream) {
        Ok(request) => route(state, request),
        Err(error) => problem(400, "bad_request", error),
    };
    respond(stream, answer.0, &answer.1).map_err(|error| error.to_string())
}

fn seam_edges(session: &Session, seams: Option<&Value>) -> Result<Vec<usize>, String> {
    let Some(seams) = seams else {
        return Ok(Vec::new());
    };
    let pairs = seams
        .as_array()
        .ok_or("seams must be an array of vertex pairs")?;
    let mesh = &session.document().expect("loaded").mesh;
    let lookup: HashMap<[usize; 2], usize> = (0..mesh.edge_count())
        .map(|edge| {
            let mut pair = mesh.edge_vertices(edge);
            pair.sort_unstable();
            (pair, edge)
        })
        .collect();
    let mut edges = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let pair = pair
            .as_array()
            .filter(|p| p.len() == 2)
            .ok_or("seam must contain two vertex indices")?;
        let a = usize::try_from(
            pair[0]
                .as_u64()
                .ok_or("seam vertex index must be an integer")?,
        )
        .map_err(|_| "seam vertex index is too large")?;
        let b = usize::try_from(
            pair[1]
                .as_u64()
                .ok_or("seam vertex index must be an integer")?,
        )
        .map_err(|_| "seam vertex index is too large")?;
        let mut key = [a, b];
        key.sort_unstable();
        let edge = *lookup
            .get(&key)
            .ok_or("seam edge is absent from mesh topology")?;
        edges.push(edge);
    }
    edges.sort_unstable();
    edges.dedup();
    Ok(edges)
}

fn load_obj(
    obj: &str,
    object_id: &str,
    addon_version: &str,
    seams: Option<&Value>,
) -> Result<RemoteSession, String> {
    if obj.is_empty() || obj.len() > MAX_BODY {
        return Err("OBJ payload is empty or too large".into());
    }
    let path = std::env::temp_dir().join(format!("uvcraft-bridge-{}.obj", random_hex()?));
    std::fs::write(&path, obj).map_err(|error| error.to_string())?;
    let mut session = Session::new();
    if let Err(error) = session.execute(Command::Load { path: path.clone() }) {
        let _ = std::fs::remove_file(&path);
        return Err(error.to_string());
    }
    let edges = match seam_edges(&session, seams) {
        Ok(edges) => edges,
        Err(error) => {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
    };
    if !edges.is_empty()
        && let Err(error) = session.execute(Command::Cut { edges })
    {
        let _ = std::fs::remove_file(&path);
        return Err(error.to_string());
    }
    Ok(RemoteSession {
        session,
        path,
        object_id: object_id.to_owned(),
        addon_version: addon_version.to_owned(),
    })
}

fn topology(session: &Session) -> Value {
    let doc = session.document().expect("loaded remote session");
    let mesh = &doc.mesh;
    json!({
        "vertices": mesh.vertex_count(),
        "faces": (0..mesh.face_count()).map(|face| mesh.face_corners(face).map(|corner| mesh.corner_vertex(corner)).collect::<Vec<_>>()).collect::<Vec<_>>(),
    })
}

fn uv_data(session: &Session, revision: u64) -> Value {
    let doc = session.document().expect("loaded remote session");
    let mesh = &doc.mesh;
    json!({
        "protocol_version": PROTOCOL,
        "revision": revision,
        "topology": topology(session),
        "uvs": (0..mesh.corner_count()).map(|corner| doc.uv().uv_at_corner(corner).to_array()).collect::<Vec<_>>(),
        "seams": (0..mesh.edge_count()).filter(|&edge| doc.uv().edge_kind(mesh, edge) == EdgeKind::Seam).map(|edge| { let mut pair = mesh.edge_vertices(edge); pair.sort_unstable(); pair }).collect::<Vec<_>>(),
    })
}

fn permitted(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::AutoSeams { .. }
            | Command::Project { .. }
            | Command::Unfold { .. }
            | Command::Optimize { .. }
            | Command::Pack { .. }
            | Command::Transform { .. }
            | Command::Mirror { .. }
            | Command::EqualizeTexelDensity { .. }
            | Command::AverageIslandScale { .. }
            | Command::StraightenIsland { .. }
            | Command::Stack { .. }
            | Command::Unstack { .. }
            | Command::Cut { .. }
            | Command::ClearSeams { .. }
            | Command::Weld { .. }
            | Command::Undo
            | Command::Redo
    )
}

fn route(state: &mut State, request: Request) -> (u16, Value) {
    if request.origin.as_deref().is_some_and(|origin| {
        !matches!(
            origin,
            "http://127.0.0.1" | "http://localhost" | "http://[::1]"
        ) && !origin.starts_with("http://127.0.0.1:")
            && !origin.starts_with("http://localhost:")
            && !origin.starts_with("http://[::1]:")
    }) {
        return problem(401, "invalid_origin", "only local origins are accepted");
    }
    if request.method == "GET" && request.path == "/v1/health" {
        return (
            200,
            json!({"protocol_version":PROTOCOL,"app_version":env!("CARGO_PKG_VERSION"),"status":"ok"}),
        );
    }
    if request.authorization.as_deref() != Some(&format!("Bearer {}", state.token)) {
        return problem(401, "unauthorized", "session token required");
    }
    if request.method == "GET" && request.path == "/v1/status" {
        return (
            200,
            json!({"protocol_version":PROTOCOL,"sessions":state.sessions.len()}),
        );
    }
    if request.method == "POST" && request.path == "/v1/sessions" {
        let body: Value = match serde_json::from_slice(&request.body) {
            Ok(value) => value,
            Err(error) => return problem(400, "invalid_json", error.to_string()),
        };
        if body["protocol_version"].as_u64() != Some(PROTOCOL) {
            return problem(409, "protocol_mismatch", "protocol_version must be 1");
        }
        let Some(obj) = body["obj"].as_str() else {
            return problem(400, "missing_obj", "obj must be an OBJ string");
        };
        let object_id = body["object_id"].as_str().unwrap_or("");
        let Some(addon_version) = body["addon_version"].as_str() else {
            return problem(
                400,
                "missing_addon_version",
                "addon_version must be a string",
            );
        };
        let loaded = match load_obj(obj, object_id, addon_version, body.get("seams")) {
            Ok(loaded) => loaded,
            Err(error) => return problem(400, "invalid_mesh", error),
        };
        let id = match random_hex() {
            Ok(id) => id,
            Err(error) => return problem(500, "random", error),
        };
        let topo = topology(&loaded.session);
        state.sessions.insert(id.clone(), loaded);
        state.changed(Some(id.clone()));
        return (
            201,
            json!({"protocol_version":PROTOCOL,"session_id":id,"topology":topo,"revision":state.revision}),
        );
    }
    let Some(rest) = request.path.strip_prefix("/v1/sessions/") else {
        return problem(404, "not_found", "unknown endpoint");
    };
    let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
    let Some(remote) = state.sessions.get_mut(id) else {
        return problem(404, "not_found", "unknown session");
    };
    match (request.method.as_str(), action) {
        ("GET", "status") => (
            200,
            json!({"protocol_version":PROTOCOL,"app_version":env!("CARGO_PKG_VERSION"),"addon_version":remote.addon_version,"object_id":remote.object_id,"topology":topology(&remote.session),"can_undo":remote.session.can_undo(),"can_redo":remote.session.can_redo(),"revision":state.revision}),
        ),
        ("GET", "uv") => (200, uv_data(&remote.session, state.revision)),
        ("POST", "commands") => {
            let body: Value = match serde_json::from_slice(&request.body) {
                Ok(value) => value,
                Err(error) => return problem(400, "invalid_json", error.to_string()),
            };
            let Some(commands) = body["commands"].as_array() else {
                return problem(400, "invalid_commands", "commands must be an array");
            };
            let mut parsed = Vec::new();
            for value in commands {
                let command: Command = match serde_json::from_value(value.clone()) {
                    Ok(command) => command,
                    Err(error) => return problem(400, "invalid_command", error.to_string()),
                };
                if !permitted(&command) {
                    return problem(
                        400,
                        "invalid_command",
                        "command is unavailable over the bridge",
                    );
                }
                parsed.push(command);
            }
            for (index, command) in parsed.into_iter().enumerate() {
                if let Err(error) = remote.session.execute(command) {
                    state.changed(Some(id.to_owned()));
                    return problem(
                        409,
                        "command_failed",
                        format!("command {}: {error}", index + 1),
                    );
                }
            }
            let topo = topology(&remote.session);
            state.changed(Some(id.to_owned()));
            (
                200,
                json!({"status":"done","topology":topo,"revision":state.revision}),
            )
        }
        ("POST", "mesh") => {
            let body: Value = match serde_json::from_slice(&request.body) {
                Ok(value) => value,
                Err(error) => return problem(400, "invalid_json", error.to_string()),
            };
            let Some(obj) = body["obj"].as_str() else {
                return problem(400, "missing_obj", "obj must be an OBJ string");
            };
            let mut updated = match load_obj(
                obj,
                &remote.object_id,
                &remote.addon_version,
                body.get("seams"),
            ) {
                Ok(updated) => updated,
                Err(error) => return problem(400, "invalid_mesh", error),
            };
            if topology(&updated.session) != topology(&remote.session) {
                return problem(
                    409,
                    "topology_mismatch",
                    "updated mesh topology differs from this session",
                );
            }
            if let Err(error) = updated.session.retain_group_metadata_from(&remote.session) {
                return problem(409, "metadata_mismatch", error.to_string());
            }
            *remote = updated;
            let topo = topology(&remote.session);
            state.changed(Some(id.to_owned()));
            (
                200,
                json!({"status":"updated","topology":topo,"revision":state.revision}),
            )
        }
        ("DELETE", "") => {
            state.sessions.remove(id);
            state.changed(state.sessions.keys().next().cloned());
            (200, json!({"status":"disconnected"}))
        }
        _ => problem(404, "not_found", "unknown endpoint"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, path: &str, token: Option<&str>, body: Value) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            authorization: token.map(|t| format!("Bearer {t}")),
            origin: None,
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

    #[test]
    fn validates_loopback_and_session_topology() {
        assert!(loopback_addr("127.0.0.1:0").is_ok());
        assert!(loopback_addr("0.0.0.0:8766").is_err());
        let mut state = State::new("secret".into());
        assert_eq!(
            route(&mut state, req("GET", "/v1/health", None, json!({}))).0,
            200
        );
        assert_eq!(
            route(&mut state, req("GET", "/v1/status", None, json!({}))).0,
            401
        );
        let mut hostile = req("GET", "/v1/status", Some("secret"), json!({}));
        hostile.origin = Some("https://example.org".into());
        assert_eq!(
            route(&mut state, hostile).1["error"]["code"],
            "invalid_origin"
        );
        let mesh = "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";
        let created = route(
            &mut state,
            req(
                "POST",
                "/v1/sessions",
                Some("secret"),
                json!({"protocol_version":1,"addon_version":"0.1.0","object_id":"Cube","obj":mesh}),
            ),
        );
        assert_eq!(created.0, 201);
        let id = created.1["session_id"].as_str().unwrap();
        let uv = route(
            &mut state,
            req(
                "GET",
                &format!("/v1/sessions/{id}/uv"),
                Some("secret"),
                json!({}),
            ),
        );
        assert_eq!(uv.1["topology"]["faces"], json!([[0, 1, 2]]));
        assert_eq!(uv.1["uvs"].as_array().unwrap().len(), 3);
        let wrong = "v 0 0 0\nv 1 0 0\nv 0 1 0\nv 1 1 0\nf 1 2 4\n";
        assert_eq!(
            route(
                &mut state,
                req(
                    "POST",
                    &format!("/v1/sessions/{id}/mesh"),
                    Some("secret"),
                    json!({"obj":wrong})
                )
            )
            .0,
            409
        );
        assert_eq!(
            route(
                &mut state,
                req(
                    "DELETE",
                    &format!("/v1/sessions/{id}"),
                    Some("secret"),
                    json!({})
                )
            )
            .0,
            200
        );
    }

    #[test]
    fn http_transport_returns_versioned_json() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut state = State::new("secret".into());
            serve_client(&mut stream, &mut state).unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(b"GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        let body = response.split_once("\r\n\r\n").unwrap().1;
        let json: Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["protocol_version"], PROTOCOL);
        server.join().unwrap();
    }

    fn http(
        address: SocketAddr,
        token: &str,
        method: &str,
        path: &str,
        body: Value,
    ) -> (u16, Value) {
        let data = serde_json::to_vec(&body).unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        write!(client, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", data.len()).unwrap();
        client.write_all(&data).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, serde_json::from_str(body).unwrap())
    }

    #[test]
    fn desktop_http_round_trip_preserves_seams_and_publishes_ui_edits() {
        let bridge = start_desktop("127.0.0.1:0", || {}).unwrap();
        let address = bridge.address();
        let token = bridge.token();
        let obj = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nf 1/1 2/2 3/3\nf 1/1 3/3 4/4\n";
        let (created_status, created) = http(
            address,
            &token,
            "POST",
            "/v1/sessions",
            json!({
                "protocol_version": 1, "addon_version": "0.1", "object_id": "Quad",
                "obj": obj, "seams": [[0, 2]],
            }),
        );
        assert_eq!(created_status, 201, "{created}");
        let id = created["session_id"].as_str().unwrap();
        let path = format!("/v1/sessions/{id}");
        let (status, uv) = http(address, &token, "GET", &format!("{path}/uv"), json!({}));
        assert_eq!(status, 200);
        assert_eq!(uv["seams"], json!([[0, 2]]));
        assert_eq!(uv["uvs"].as_array().unwrap().len(), 6);
        assert_eq!(uv["revision"], created["revision"]);

        let mut snapshot = bridge.snapshot_after(0).unwrap().unwrap();
        assert_eq!(snapshot.object_id, "Quad");
        snapshot.session.execute(Command::Undo).unwrap();
        let in_flight_request = bridge.state.lock().unwrap();
        assert_eq!(
            bridge.publish(&snapshot.session_id, snapshot.revision, &snapshot.session),
            Err(PublishError::Busy)
        );
        drop(in_flight_request);
        let published = bridge
            .publish(&snapshot.session_id, snapshot.revision, &snapshot.session)
            .unwrap();
        let (_, uv) = http(address, &token, "GET", &format!("{path}/uv"), json!({}));
        assert_eq!(uv["revision"], published);
        assert_eq!(uv["seams"], json!([]));

        let (_, updated) = http(
            address,
            &token,
            "POST",
            &format!("{path}/mesh"),
            json!({
                "obj": obj, "seams": [[0, 2]],
            }),
        );
        assert_eq!(updated["status"], "updated");
        assert!(
            bridge
                .publish(&snapshot.session_id, snapshot.revision, &snapshot.session)
                .is_err()
        );
        let (_, status) = http(address, &token, "GET", &format!("{path}/status"), json!({}));
        assert_eq!(status["revision"], updated["revision"]);
        assert_eq!(status["object_id"], "Quad");
        let (deleted, _) = http(address, &token, "DELETE", &path, json!({}));
        assert_eq!(deleted, 200);
        assert_eq!(bridge.session_count(), Some(0));
    }

    #[test]
    fn blender_mesh_update_keeps_uv_groups_without_exposing_metadata() {
        let bridge = start_desktop("127.0.0.1:0", || {}).unwrap();
        let address = bridge.address();
        let token = bridge.token();
        let obj = "v 0 0 0\nv 1 0 0\nv 0 1 0\nv 2 0 0\nv 3 0 0\nv 2 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nvt 2 0\nvt 3 0\nvt 2 1\nf 1/1 2/2 3/3\nf 4/4 5/5 6/6\n";
        let (code, created) = http(
            address,
            &token,
            "POST",
            "/v1/sessions",
            json!({
                "protocol_version": 1, "addon_version": "0.1", "object_id": "Pair", "obj": obj,
            }),
        );
        assert_eq!(code, 201);
        let id = created["session_id"].as_str().unwrap();
        let path = format!("/v1/sessions/{id}");
        let mut snapshot = bridge.snapshot_after(0).unwrap().unwrap();
        snapshot
            .session
            .execute(Command::CreateIslandGroup {
                name: "Pair group".into(),
                parent: None,
                children: Vec::new(),
                islands: vec![0, 1],
            })
            .unwrap();
        bridge
            .publish(id, snapshot.revision, &snapshot.session)
            .unwrap();

        let changed_obj = obj.replace("vt 2 0", "vt 2.25 0");
        let (code, updated) = http(
            address,
            &token,
            "POST",
            &format!("{path}/mesh"),
            json!({"obj": changed_obj}),
        );
        assert_eq!(code, 200, "{updated}");
        let current = bridge.snapshot_after(0).unwrap().unwrap();
        let doc = current.session.document().unwrap();
        let group = doc.island_groups().get(1).unwrap();
        assert_eq!(group.name, "Pair group");
        assert_eq!(doc.island_groups().face_group(0), Some(1));
        assert_eq!(doc.island_groups().face_group(1), Some(1));
        let (_, uv) = http(address, &token, "GET", &format!("{path}/uv"), json!({}));
        assert!(uv.get("groups").is_none());
        assert_eq!(uv["uvs"][3], json!([2.25, 0.0]));
        assert_eq!(uv["topology"]["faces"], json!([[0, 1, 2], [3, 4, 5]]));
    }

    #[test]
    fn request_line_limit_is_enforced_before_allocating_entire_line() {
        let bytes = vec![b'x'; 32 * 1024];
        let mut reader = BufReader::new(std::io::Cursor::new(bytes));
        assert_eq!(
            read_line_limited(&mut reader, 4096).unwrap_err(),
            "HTTP header is too large"
        );
    }

    #[test]
    fn partial_http_request_does_not_lock_desktop_state() {
        let bridge = Arc::new(start_desktop("127.0.0.1:0", || {}).unwrap());
        let mut client = TcpStream::connect(bridge.address()).unwrap();
        client
            .write_all(b"GET /v1/health HTTP/1.1\r\nHost:")
            .unwrap();
        std::thread::sleep(Duration::from_millis(60));
        let (tx, rx) = std::sync::mpsc::channel();
        let from_ui = bridge.clone();
        std::thread::spawn(move || {
            let result = (
                from_ui.revision(),
                from_ui.snapshot_after(0).is_ok(),
                from_ui.session_count(),
            );
            tx.send(result).unwrap();
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(500)).unwrap(),
            (0, true, Some(0))
        );
        drop(client);
    }
}
