//! Signing in to Filen without keeping the password.
//!
//! Filen encrypts everything with keys made from the account password, so
//! the password is needed once. SyncHopper does what Filen's own apps do. It
//! works out two things from the password: a key, and a value to sign in
//! with. It signs in with that value (the password itself is never sent),
//! fetches the account's keys, unlocks them with the key, and keeps only the
//! unlocked keys. rclone then runs from those keys in what it calls INTERNAL
//! mode, which is also how Filen's desktop app runs it.
//!
//! The steps, addresses and formats follow Filen's own libraries: the Go one
//! inside rclone (filen-sdk-go) and the JS one Filen's apps use (@filen/sdk).
//! The tests check this file against values made with Filen's Go code and,
//! separately, with Python.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, KeyInit};
use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::time::Duration;

/// Where Filen's apps sign in.
pub const GATEWAY: &str = "https://gateway.filen.io";

/// What rclone needs to work with a Filen account, and nothing more.
#[derive(Clone, PartialEq, Eq)]
pub struct Keys {
    pub email: String,
    pub api_key: String,
    /// Accounts on sign-in version 1 or 2: the account's master keys, the
    /// current one first. Version 3: the account's data key, alone.
    pub master_keys: Vec<String>,
    /// The account's private key, base64 PKCS#8, as Filen keeps it.
    pub private_key: String,
    /// The account's public key, base64 PKCS#1, the form rclone reads.
    pub public_key: String,
    pub auth_version: u32,
    pub base_folder: String,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the keys themselves.
        f.debug_struct("Keys")
            .field("email", &self.email)
            .field("master_keys", &self.master_keys.len())
            .field("auth_version", &self.auth_version)
            .finish_non_exhaustive()
    }
}

impl Keys {
    /// The account's lines in rclone's saved sign-ins. `password` and
    /// `api_key` must already be obscured by rclone; the password is the
    /// word INTERNAL, which tells rclone to use the keys.
    pub fn config_lines(&self, kind: &str, password: &str, api_key: &str) -> Result<String, String> {
        let master_keys = self.master_keys.join("|");
        let auth_version = self.auth_version.to_string();
        let values = [
            kind,
            &self.email,
            password,
            api_key,
            &master_keys,
            &self.private_key,
            &self.public_key,
            &auth_version,
            &self.base_folder,
        ];
        if self.master_keys.iter().any(|k| k.is_empty() || k.contains('|')) || values.iter().any(|v| v.is_empty() || v.trim() != *v || v.chars().any(char::is_control)) {
            return Err("Filen sent something SyncHopper can't save, so nothing was saved.".into());
        }
        Ok(format!(
            "type = {kind}\nemail = {}\npassword = {password}\napi_key = {api_key}\nmaster_keys = {master_keys}\nprivate_key = {}\npublic_key = {}\nauth_version = {auth_version}\nbase_folder_uuid = {}\n",
            self.email, self.private_key, self.public_key, self.base_folder
        ))
    }
}

/// Why signing in didn't work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The account uses two-factor sign-in and no code was given.
    NeedsCode,
    /// The two-factor code was wrong or too old.
    WrongCode,
    /// Filen turned the sign-in down, in its own words.
    Refused(String),
    /// Filen is limiting how often this PC may try.
    Busy,
    /// Filen could not be reached.
    Unreachable(String),
    /// The account has never been opened in one of Filen's apps, so its
    /// keys don't exist yet. Filen's apps make them on first sign-in.
    NotSetUp,
    /// Filen answered in a way that doesn't match its own libraries.
    Unexpected(String),
}

/// The words the screens look for to show the box for a two-factor code.
pub const CODE_WORDS: &str = "two-factor code";

impl Failure {
    /// What the sign-in screen says.
    pub fn text(&self) -> String {
        match self {
            Failure::NeedsCode => format!("Your Filen account asks for a {CODE_WORDS}. Enter the code from your authenticator app and try again."),
            Failure::WrongCode => format!("Filen did not accept that {CODE_WORDS}. Enter the current code from your authenticator app and try again."),
            Failure::Refused(m) if m.trim().is_empty() => "Filen did not accept that email and password.".into(),
            Failure::Refused(m) => format!("Filen did not accept the sign-in. Filen says: {}", clip(m.trim(), 200)),
            Failure::Busy => "Filen is limiting sign-in attempts from this PC right now. Wait a few minutes and try again.".into(),
            Failure::Unreachable(d) => format!("Could not reach Filen. Check the internet connection and try again. Details: {}", clip(d, 200)),
            Failure::NotSetUp => "This Filen account hasn't been opened in one of Filen's apps yet, so its keys don't exist. Sign in once on Filen's website or in its app, then try again.".into(),
            Failure::Unexpected(d) => format!("Filen answered in a way SyncHopper didn't expect, so nothing was saved. Details: {}", clip(d, 200)),
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
    }
}

// ----- the sign-in --------------------------------------------------------

/// Signs in to the Filen account and returns its keys. `code` is the
/// two-factor code, empty when the account doesn't use one. `gateway` is
/// [`GATEWAY`] except in tests.
pub async fn sign_in(gateway: &str, email: &str, password: &str, code: &str) -> Result<Keys, Failure> {
    let mut api = Api::new(gateway)?;
    let info = api.call(false, "/v3/auth/info", json!({ "email": email })).await.map_err(turned_down)?;
    let version = info["authVersion"].as_u64().unwrap_or(0) as u32;
    let salt = info["salt"].as_str().unwrap_or("").to_string();
    // The key math is slow on purpose, so it runs off the async threads.
    let typed = password.to_string();
    let derived = tokio::task::spawn_blocking(move || derive(version, &typed, &salt))
        .await
        .map_err(|e| Failure::Unexpected(e.to_string()))?
        .map_err(Failure::Unexpected)?;
    let code: String = code.split_whitespace().collect();
    let login = api
        .call(
            false,
            "/v3/login",
            json!({
                "email": email,
                "password": derived.login,
                // What Filen's apps send when there is no code.
                "twoFactorCode": if code.is_empty() { "XXXXXX" } else { code.as_str() },
                "authVersion": version,
            }),
        )
        .await
        .map_err(turned_down)?;
    let api_key = text_field(&login, "apiKey", "signing in")?;
    api.key = Some(api_key.clone());

    let (master_keys, private_blob, public_key) = match version {
        1 | 2 => {
            // As Filen's apps do: send the current key, locked with itself,
            // and get back every key the account has had.
            let sent = encrypt_meta_v2(&derived.key, &derived.key);
            let data = api.call(false, "/v3/user/masterKeys", json!({ "masterKeys": sent })).await.map_err(|e| failed(e, "fetching the account's keys"))?;
            let mut keys = vec![derived.key.clone()];
            if let Some(blob) = data["keys"].as_str().filter(|b| !b.is_empty()) {
                let list = decrypt_meta(&derived.key, blob).ok_or_else(|| Failure::Unexpected("the account's keys could not be unlocked".into()))?;
                for k in list.split('|') {
                    if !k.is_empty() && !keys.iter().any(|have| have == k) {
                        keys.push(k.to_string());
                    }
                }
            }
            let (public, blob) = key_pair(&api).await?;
            (keys, blob, public)
        }
        3 => {
            let data = api.call(true, "/v3/user/dek", Value::Null).await.map_err(|e| failed(e, "fetching the account's data key"))?;
            let blob = data["dek"].as_str().unwrap_or("");
            if blob.is_empty() {
                return Err(Failure::NotSetUp);
            }
            let dek = decrypt_meta_v3(&derived.key, blob).ok_or_else(|| Failure::Unexpected("the account's data key could not be unlocked".into()))?;
            if dek.len() != 64 || !dek.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Failure::Unexpected("the account's data key is not in the expected form".into()));
            }
            let (public, blob) = key_pair(&api).await?;
            (vec![dek], blob, public)
        }
        v => return Err(Failure::Unexpected(format!("this account uses sign-in version {v}, which SyncHopper doesn't know"))),
    };
    // The private key is locked with one of the keys. Older accounts may
    // have it locked with a key from before a password change.
    let private_key = master_keys
        .iter()
        .find_map(|k| {
            let opened = if version == 3 { decrypt_meta_v3(k, &private_blob) } else { decrypt_meta(k, &private_blob) };
            opened.filter(|p| looks_like_pkcs8(p))
        })
        .ok_or_else(|| Failure::Unexpected("the account's private key could not be unlocked".into()))?;
    let public_key = public_key_pkcs1(&public_key).ok_or_else(|| Failure::Unexpected("the account's public key is not in the expected form".into()))?;
    let folder = api.call(true, "/v3/user/baseFolder", Value::Null).await.map_err(|e| failed(e, "finding the account's top folder"))?;
    let base_folder = text_field(&folder, "uuid", "finding the account's top folder")?;
    Ok(Keys { email: email.to_string(), api_key, master_keys, private_key, public_key, auth_version: version, base_folder })
}

/// The account's public key and its locked private key.
async fn key_pair(api: &Api) -> Result<(String, String), Failure> {
    let pair = api.call(true, "/v3/user/keyPair/info", Value::Null).await.map_err(|e| failed(e, "fetching the account's key pair"))?;
    let public = pair["publicKey"].as_str().unwrap_or("").to_string();
    let private = pair["privateKey"].as_str().unwrap_or("").to_string();
    if public.is_empty() || private.is_empty() {
        return Err(Failure::NotSetUp);
    }
    Ok((public, private))
}

fn text_field(data: &Value, name: &str, what: &str) -> Result<String, Failure> {
    data[name]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Failure::Unexpected(format!("no {name} came back when {what}")))
}

// ----- talking to Filen ---------------------------------------------------

struct Api {
    http: reqwest::Client,
    base: String,
    key: Option<String>,
}

#[derive(Debug)]
enum ApiError {
    Net(String),
    Busy,
    Filen { code: String, message: String },
    Bad(String),
}

impl Api {
    fn new(base: &str) -> Result<Self, Failure> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(45))
            .user_agent(concat!("SyncHopper/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Failure::Unexpected(e.to_string()))?;
        Ok(Api { http, base: base.trim_end_matches('/').to_string(), key: None })
    }

    /// One request. Filen answers `{status, message, code, data}` and puts
    /// what was asked for in `data`.
    async fn call(&self, get: bool, path: &str, body: Value) -> Result<Value, ApiError> {
        let url = format!("{}{path}", self.base);
        let mut req = if get {
            self.http.get(&url)
        } else {
            self.http.post(&url).header(reqwest::header::CONTENT_TYPE, "application/json").body(body.to_string())
        };
        if let Some(key) = &self.key {
            req = req.bearer_auth(key);
        }
        let res = req.send().await.map_err(|e| ApiError::Net(e.to_string()))?;
        let status = res.status();
        if status.as_u16() == 429 {
            return Err(ApiError::Busy);
        }
        let text = res.text().await.map_err(|e| ApiError::Net(e.to_string()))?;
        let answer: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) if status.is_server_error() => return Err(ApiError::Net(format!("Filen answered with error {}", status.as_u16()))),
            Err(_) => return Err(ApiError::Bad(format!("{path} answered {} without the usual reply", status.as_u16()))),
        };
        if answer["status"].as_bool() != Some(true) {
            let code = answer["code"].as_str().unwrap_or("").to_string();
            let message = answer["message"].as_str().unwrap_or("").to_string();
            if busy(&code, &message) {
                return Err(ApiError::Busy);
            }
            return Err(ApiError::Filen { code, message });
        }
        Ok(answer["data"].clone())
    }
}

fn busy(code: &str, message: &str) -> bool {
    let code = code.to_ascii_lowercase();
    code.contains("rate_limit") || code.contains("too_many") || message.to_ascii_lowercase().contains("too many requests")
}

/// While signing in, Filen saying no is about the email, the password or
/// the two-factor code.
fn turned_down(e: ApiError) -> Failure {
    match e {
        ApiError::Filen { code, message } => match code.as_str() {
            "enter_2fa" => Failure::NeedsCode,
            "wrong_2fa" => Failure::WrongCode,
            _ => Failure::Refused(message),
        },
        other => failed(other, "signing in"),
    }
}

/// After signing in, Filen saying no is unexpected.
fn failed(e: ApiError, what: &str) -> Failure {
    match e {
        ApiError::Net(d) => Failure::Unreachable(d),
        ApiError::Busy => Failure::Busy,
        ApiError::Filen { code, message } => Failure::Unexpected(format!("{what}: {message} ({code})")),
        ApiError::Bad(d) => Failure::Unexpected(format!("{what}: {d}")),
    }
}

// ----- the key math -------------------------------------------------------

/// What a password turns into: the key that unlocks the account's keys,
/// and the value Filen checks at sign-in.
#[derive(Debug, PartialEq, Eq)]
pub struct Derived {
    pub key: String,
    pub login: String,
}

/// The password math for each of Filen's sign-in versions, as in Filen's
/// libraries (`DeriveMKAndAuthFromPassword`, `DeriveKEKAndAuthFromPassword`
/// and the version 1 functions).
pub fn derive(version: u32, password: &str, salt: &str) -> Result<Derived, String> {
    match version {
        // Filen's earliest accounts. Kept by Filen's own libraries for the
        // few accounts that still use it.
        1 => Ok(Derived { key: v2_hash(password.as_bytes()), login: v1_login(password) }),
        2 => {
            let mut out = [0u8; 64];
            pbkdf2::pbkdf2_hmac::<Sha512>(password.as_bytes(), salt.as_bytes(), 200_000, &mut out);
            let all = hex(&out);
            Ok(Derived { key: all[..64].to_string(), login: hex(&Sha512::digest(&all.as_bytes()[64..])) })
        }
        3 => {
            let salt = unhex(salt).ok_or("the account's salt is not in the expected form")?;
            let params = argon2::Params::new(65536, 3, 4, Some(64)).map_err(|e| e.to_string())?;
            let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
            let mut out = [0u8; 64];
            argon.hash_password_into(password.as_bytes(), &salt, &mut out).map_err(|e| e.to_string())?;
            let all = hex(&out);
            Ok(Derived { key: all[..64].to_string(), login: all[64..].to_string() })
        }
        v => Err(format!("this account uses sign-in version {v}, which SyncHopper doesn't know")),
    }
}

fn hex_of<D: Digest>(data: &[u8]) -> String {
    hex(&D::digest(data))
}

/// `V2Hash`: SHA-1 of the hex SHA-512.
fn v2_hash(data: &[u8]) -> String {
    hex_of::<sha1::Sha1>(hex_of::<Sha512>(data).as_bytes())
}

/// `V1HashPassword`: two chains of hashes, each over the hex of the last.
fn v1_login(password: &str) -> String {
    let p = password.as_bytes();
    let first = hex_of::<Sha512>(hex_of::<Sha384>(hex_of::<Sha256>(hex_of::<sha1::Sha1>(p).as_bytes()).as_bytes()).as_bytes());
    let second = hex_of::<Sha512>(hex_of::<md5::Md5>(hex_of::<md4::Md4>(hex_of::<md2::Md2>(p).as_bytes()).as_bytes()).as_bytes());
    first + &second
}

// ----- Filen's locked strings ("metadata") ---------------------------------

/// The AES-256-GCM key behind a version 1 or 2 master key.
fn master_cipher(key: &str) -> Aes256Gcm {
    let mut k = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha512>(key.as_bytes(), key.as_bytes(), 1, &mut k);
    Aes256Gcm::new(GenericArray::from_slice(&k))
}

/// Locks `text` with a version 1 or 2 master key, in Filen's "002" form.
pub fn encrypt_meta_v2(key: &str, text: &str) -> String {
    let nonce = random_chars(12);
    let sealed = master_cipher(key).encrypt(Nonce::from_slice(nonce.as_bytes()), text.as_bytes()).expect("AES-GCM always seals");
    format!("002{nonce}{}", base64::engine::general_purpose::STANDARD.encode(sealed))
}

/// Opens a string locked with a version 1 or 2 master key: Filen's "002"
/// form, or the oldest form, which is openssl's "Salted__" encryption.
pub fn decrypt_meta(key: &str, locked: &str) -> Option<String> {
    let opened = if locked.starts_with("U2FsdGVk") {
        decrypt_salted(key, locked)?
    } else {
        let rest = locked.strip_prefix("002")?;
        let nonce = rest.get(..12)?;
        let sealed = base64::engine::general_purpose::STANDARD.decode(rest.get(12..)?).ok()?;
        master_cipher(key).decrypt(Nonce::from_slice(nonce.as_bytes()), sealed.as_ref()).ok()?
    };
    String::from_utf8(opened).ok()
}

/// Opens a string locked with a version 3 key (64 hex characters), in
/// Filen's "003" form.
pub fn decrypt_meta_v3(key_hex: &str, locked: &str) -> Option<String> {
    let rest = locked.strip_prefix("003")?;
    let nonce = unhex(rest.get(..24)?)?;
    let sealed = base64::engine::general_purpose::STANDARD.decode(rest.get(24..)?).ok()?;
    let key = unhex(key_hex).filter(|k| k.len() == 32)?;
    let cipher = Aes256Gcm::new(GenericArray::from_slice(&key));
    String::from_utf8(cipher.decrypt(Nonce::from_slice(&nonce), sealed.as_ref()).ok()?).ok()
}

/// openssl's "Salted__" AES-256-CBC, with the key and IV made from the
/// password by `EVP_BytesToKey` with MD5.
fn decrypt_salted(key: &str, locked: &str) -> Option<Vec<u8>> {
    let raw = base64::engine::general_purpose::STANDARD.decode(locked).ok()?;
    if raw.len() < 32 || &raw[..8] != b"Salted__" {
        return None;
    }
    let (salt, data) = (&raw[8..16], &raw[16..]);
    if data.len() % 16 != 0 {
        return None;
    }
    let mut made = Vec::with_capacity(48);
    let mut last: Vec<u8> = Vec::new();
    while made.len() < 48 {
        let mut h = md5::Md5::new();
        h.update(&last);
        h.update(key.as_bytes());
        h.update(salt);
        last = h.finalize().to_vec();
        made.extend_from_slice(&last);
    }
    let cipher = aes::Aes256::new_from_slice(&made[..32]).ok()?;
    let mut prev: [u8; 16] = made[32..48].try_into().ok()?;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        out.extend(block.iter().zip(prev.iter()).map(|(a, b)| a ^ b));
        prev.copy_from_slice(chunk);
    }
    let pad = *out.last()? as usize;
    if pad == 0 || pad > 16 || pad > out.len() || !out[out.len() - pad..].iter().all(|&b| b as usize == pad) {
        return None;
    }
    out.truncate(out.len() - pad);
    Some(out)
}

// ----- key formats --------------------------------------------------------

const RSA_OID: [u8; 11] = [0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];

/// One DER item: its tag, its contents, and what follows it.
fn der(d: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = d.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        (rest[..n].iter().fold(0usize, |acc, &b| (acc << 8) | b as usize), &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

/// Filen hands out public keys as SubjectPublicKeyInfo, and rclone reads
/// PKCS#1, which is the inside of it. A key that is already PKCS#1 is
/// returned as it is.
pub fn public_key_pkcs1(b64: &str) -> Option<String> {
    let engine = base64::engine::general_purpose::STANDARD;
    let all = engine.decode(b64.trim()).ok()?;
    let (0x30, body, rest) = der(&all)? else { return None };
    if !rest.is_empty() {
        return None;
    }
    let (tag, alg, after) = der(body)?;
    let inner = if tag == 0x02 {
        // Already PKCS#1: a sequence of two integers.
        &all[..]
    } else {
        if tag != 0x30 || !alg.starts_with(&RSA_OID) {
            return None;
        }
        let (0x03, bits, end) = der(after)? else { return None };
        if !end.is_empty() || bits.first() != Some(&0) {
            return None;
        }
        &bits[1..]
    };
    let (0x30, ints, end) = der(inner)? else { return None };
    let (0x02, _, more) = der(ints)? else { return None };
    let (0x02, _, more) = der(more)? else { return None };
    if !end.is_empty() || !more.is_empty() {
        return None;
    }
    Some(engine.encode(inner))
}

/// True for a base64 RSA private key in PKCS#8, the form rclone reads.
fn looks_like_pkcs8(b64: &str) -> bool {
    let Ok(all) = base64::engine::general_purpose::STANDARD.decode(b64) else { return false };
    let Some((0x30, body, rest)) = der(&all) else { return false };
    let Some((0x02, version, after)) = der(body) else { return false };
    let Some((0x30, alg, after)) = der(after) else { return false };
    let Some((0x04, key, _)) = der(after) else { return false };
    rest.is_empty() && version == [0] && alg.starts_with(&RSA_OID) && !key.is_empty()
}

// ----- small helpers ------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn random_chars(n: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("the system has randomness");
    bytes.iter().map(|b| CHARS[*b as usize % CHARS.len()] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values made with Filen's own Go code (the library inside rclone) and,
    /// separately, with Python's OpenSSL-backed hashes and Argon2. The
    /// private keys in it are stand-ins shaped like keys, not keys.
    fn fixtures() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/filen-accounts.json")).unwrap()
    }

    #[test]
    fn password_math_matches_filens_code_for_every_version() {
        let f = fixtures();
        let vectors = f["derive"].as_array().unwrap();
        let mut seen = [0; 4];
        for v in vectors {
            let version = v["version"].as_u64().unwrap() as u32;
            let got = derive(version, v["password"].as_str().unwrap(), v["salt"].as_str().unwrap_or("")).unwrap();
            assert_eq!(got.key, v["key"].as_str().unwrap(), "key, version {version}, {:?}", v["password"]);
            assert_eq!(got.login, v["login"].as_str().unwrap(), "sign-in value, version {version}, {:?}", v["password"]);
            seen[version as usize] += 1;
        }
        assert!(seen[1] >= 2 && seen[2] >= 3 && seen[3] >= 2, "{seen:?}");
        assert!(derive(4, "x", "").is_err());
        assert!(derive(3, "x", "not hex").is_err());
    }

    #[test]
    fn the_rare_hashes_match_their_published_test_values() {
        // RFC 1319 and RFC 1320.
        assert_eq!(hex_of::<md2::Md2>(b""), "8350e5a3e24c153df2275c9f80692773");
        assert_eq!(hex_of::<md2::Md2>(b"message digest"), "ab4f496bfb2a530b219ff33031fe06b0");
        assert_eq!(hex_of::<md4::Md4>(b"abc"), "a448017aaf21d8525fc10ae87aa6729d");
        assert_eq!(hex_of::<md4::Md4>(b"abcdefghijklmnopqrstuvwxyz"), "d79e1c308aa5bbcdeea8ed63df412da9");
    }

    #[test]
    fn locked_strings_made_by_filens_code_open() {
        let f = fixtures();
        for m in f["meta"].as_array().unwrap() {
            let (format, key, text, blob) = (m["format"].as_str().unwrap(), m["key"].as_str().unwrap(), m["text"].as_str().unwrap(), m["blob"].as_str().unwrap());
            let opened = if format == "003" { decrypt_meta_v3(key, blob) } else { decrypt_meta(key, blob) };
            assert_eq!(opened.as_deref(), Some(text), "{format}");
            // The wrong key opens nothing.
            let wrong = if format == "003" { decrypt_meta_v3(&"0".repeat(64), blob) } else { decrypt_meta("not the key", blob) };
            assert_eq!(wrong, None, "{format}");
        }
        // And what this file locks, Filen's form opens.
        let key = f["meta"][0]["key"].as_str().unwrap();
        let locked = encrypt_meta_v2(key, "a|b|c");
        assert!(locked.starts_with("002") && locked.len() > 15);
        assert_eq!(decrypt_meta(key, &locked).as_deref(), Some("a|b|c"));
        assert_ne!(encrypt_meta_v2(key, "a|b|c"), locked, "a new nonce each time");
        for junk in ["", "002", "002short", "003", "U2FsdGVk", "U2FsdGVkX1+not base64", "004abcdefghijkl"] {
            assert_eq!(decrypt_meta(key, junk), None, "{junk:?}");
            assert_eq!(decrypt_meta_v3(&"0".repeat(64), junk), None, "{junk:?}");
        }
    }

    #[test]
    fn public_keys_become_pkcs1() {
        let f = fixtures();
        for p in f["pkcs1"].as_array().unwrap() {
            let (spki, pkcs1) = (p["spki"].as_str().unwrap(), p["pkcs1"].as_str().unwrap());
            assert_eq!(public_key_pkcs1(spki).as_deref(), Some(pkcs1));
            assert_eq!(public_key_pkcs1(pkcs1).as_deref(), Some(pkcs1), "already PKCS#1");
        }
        for junk in ["", "bm90IGEga2V5", "MAA=", "MAMCAQA="] {
            assert_eq!(public_key_pkcs1(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn private_keys_are_checked_for_shape() {
        let f = fixtures();
        for a in f["accounts"].as_array().unwrap() {
            assert!(looks_like_pkcs8(a["expect"]["private_key"].as_str().unwrap()));
        }
        assert!(!looks_like_pkcs8(f["pkcs1"][0]["pkcs1"].as_str().unwrap()));
        assert!(!looks_like_pkcs8("garbage"));
    }

    #[test]
    fn saved_lines_are_what_filens_desktop_app_writes() {
        let keys = Keys {
            email: "a@b.c".into(),
            api_key: "k".into(),
            master_keys: vec!["one".into(), "two".into()],
            private_key: "PRIV==".into(),
            public_key: "PUB=".into(),
            auth_version: 2,
            base_folder: "uuid".into(),
        };
        assert_eq!(
            keys.config_lines("filen", "OBSCURED-INTERNAL", "OBSCURED-KEY").unwrap(),
            "type = filen\nemail = a@b.c\npassword = OBSCURED-INTERNAL\napi_key = OBSCURED-KEY\nmaster_keys = one|two\nprivate_key = PRIV==\npublic_key = PUB=\nauth_version = 2\nbase_folder_uuid = uuid\n"
        );
        for bad in [vec!["one\nevil = 1".to_string()], vec!["a|b".into()], vec![String::new()]] {
            assert!(Keys { master_keys: bad, ..keys.clone() }.config_lines("filen", "x", "y").is_err());
        }
        assert!(Keys { base_folder: " spaced".into(), ..keys.clone() }.config_lines("filen", "x", "y").is_err());
        // Never printed.
        let shown = format!("{keys:?}");
        assert!(!shown.contains("PRIV") && !shown.contains("one") && shown.contains("a@b.c"), "{shown}");
    }

    #[test]
    fn what_the_screens_say() {
        assert!(Failure::NeedsCode.text().contains(CODE_WORDS));
        assert!(Failure::WrongCode.text().contains(CODE_WORDS));
        assert_eq!(Failure::Refused(String::new()).text(), "Filen did not accept that email and password.");
        assert_eq!(Failure::Refused("Invalid email or password.".into()).text(), "Filen did not accept the sign-in. Filen says: Invalid email or password.");
        assert!(!Failure::Refused("x".repeat(500)).text().contains(&"x".repeat(201)));
        for f in [Failure::Busy, Failure::Unreachable("dns".into()), Failure::NotSetUp, Failure::Unexpected("odd".into())] {
            assert!(!f.text().contains(CODE_WORDS), "{f:?}");
        }
    }
}
