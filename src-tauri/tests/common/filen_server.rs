//! A stand-in for Filen's servers, for the sign-in tests. Its accounts come
//! from tests/fixtures/filen-accounts.json, which was made with Filen's own
//! Go code (the library inside rclone): their salts, the values Filen checks
//! at sign-in, and the locked keys are what Filen's code produces. The
//! private keys in them are stand-ins shaped like keys, not keys.
//!
//! It answers the requests Filen's own libraries make to sign in, in the
//! same `{status, message, code, data}` form, and records every request.

use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: String,
    pub auth: Option<String>,
}

pub struct FakeFilen {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Request>>>,
    accounts: Arc<Vec<Value>>,
}

pub fn fixtures() -> Value {
    serde_json::from_str(include_str!("../fixtures/filen-accounts.json")).unwrap()
}

impl FakeFilen {
    pub async fn start() -> FakeFilen {
        FakeFilen::start_with(fixtures()).await
    }

    /// The same with other accounts, in the fixtures' form.
    pub async fn start_with(f: Value) -> FakeFilen {
        let mut accounts: Vec<Value> = f["accounts"].as_array().unwrap().clone();
        // An account whose key list is locked with some other key, so it
        // can't be opened.
        let mut bad = accounts[0].clone();
        bad["email"] = json!("bad-keys@example.com");
        bad["api_key"] = json!("api-key-of-the-bad-keys-account");
        bad["master_keys_blob"] = accounts[2]["master_keys_blob"].clone();
        accounts.push(bad);
        let accounts = Arc::new(accounts);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (acc, reqs) = (accounts.clone(), requests.clone());
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let (acc, reqs) = (acc.clone(), reqs.clone());
                tokio::spawn(async move {
                    let Some(req) = read_request(&mut conn).await else { return };
                    reqs.lock().unwrap().push(req.clone());
                    let (status, body, kind) = answer(&acc, &req);
                    let head = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = conn.write_all(head.as_bytes()).await;
                    let _ = conn.write_all(body.as_bytes()).await;
                    let _ = conn.shutdown().await;
                });
            }
        });
        FakeFilen { url, requests, accounts }
    }

    pub fn account(&self, name: &str) -> Value {
        self.accounts.iter().find(|a| a["name"] == name).unwrap_or_else(|| panic!("no fixture account {name}")).clone()
    }

    pub fn accounts(&self) -> Vec<Value> {
        self.accounts.iter().filter(|a| a["email"] != "bad-keys@example.com").cloned().collect()
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

async fn read_request(conn: &mut tokio::net::TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = conn.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let mut len = 0usize;
    let mut auth = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().unwrap_or(0),
                "authorization" => auth = Some(v.trim().to_string()),
                _ => {}
            }
        }
    }
    while buf.len() < head_end + len {
        let n = conn.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[head_end..(head_end + len).min(buf.len())]).to_string();
    Some(Request { method, path, body, auth })
}

fn ok(data: Value) -> (u16, String, &'static str) {
    (200, json!({ "status": true, "message": "OK", "code": "ok", "data": data }).to_string(), "application/json")
}

fn no(code: &str, message: &str) -> (u16, String, &'static str) {
    (200, json!({ "status": false, "message": message, "code": code }).to_string(), "application/json")
}

fn answer(accounts: &[Value], req: &Request) -> (u16, String, &'static str) {
    let body: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
    let by_email = |e: &Value| accounts.iter().find(|a| &a["email"] == e);
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/v3/auth/info") => {
            match body["email"].as_str() {
                Some("busy@example.com") => return (429, "Too Many Requests".into(), "text/plain"),
                Some("broken@example.com") => return (503, "<html><body>Service Unavailable</body></html>".into(), "text/html"),
                _ => {}
            }
            match by_email(&body["email"]) {
                Some(a) => ok(json!({ "authVersion": a["auth_version"], "salt": a["salt"] })),
                // Like Filen, an unknown email gets an answer too, so
                // nobody can find out which emails have accounts.
                None => ok(json!({ "authVersion": 2, "salt": "0".repeat(128) })),
            }
        }
        ("POST", "/v3/login") => {
            let Some(a) = by_email(&body["email"]) else { return no("email_or_password_wrong", "Invalid email or password.") };
            if body["authVersion"] != a["auth_version"] || body["password"] != a["login"] {
                return no("email_or_password_wrong", "Invalid email or password.");
            }
            let code = body["twoFactorCode"].as_str().unwrap_or("");
            let wanted = a["two_factor"].as_str().unwrap_or("");
            if !wanted.is_empty() {
                if code == "XXXXXX" {
                    return no("enter_2fa", "Please enter your Two Factor Authentication code.");
                }
                if code != wanted {
                    return no("wrong_2fa", "Invalid Two Factor Authentication code.");
                }
            }
            ok(json!({ "apiKey": a["api_key"] }))
        }
        (method, path) => {
            let key = req.auth.as_deref().and_then(|a| a.strip_prefix("Bearer ")).unwrap_or("");
            let Some(a) = accounts.iter().find(|a| a["api_key"] == key) else { return no("api_key_not_found", "Invalid API key.") };
            match (method, path) {
                ("POST", "/v3/user/masterKeys") => {
                    // What Filen's apps send: the current key, locked with
                    // itself.
                    let derived = a["derived_key"].as_str().unwrap();
                    let sent = body["masterKeys"].as_str().unwrap_or("");
                    if synchopper_lib::filen::decrypt_meta(derived, sent).as_deref() != Some(derived) {
                        return no("invalid_params", "Invalid master keys.");
                    }
                    ok(json!({ "keys": a["master_keys_blob"] }))
                }
                ("GET", "/v3/user/keyPair/info") => ok(json!({ "publicKey": a["public_key"], "privateKey": a["private_key_blob"] })),
                ("GET", "/v3/user/dek") => ok(json!({ "dek": a["dek_blob"] })),
                ("GET", "/v3/user/baseFolder") => ok(json!({ "uuid": a["base_folder"] })),
                _ => no("not_found", "Endpoint not found."),
            }
        }
    }
}
