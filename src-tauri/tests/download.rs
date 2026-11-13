//! The sync engine download: fetched from a (local, fake) download site,
//! checked against the published checksum list, and put in place.

mod common;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;
use synchopper_lib::rclone;

fn make_zip(version: &str) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(format!("rclone-v{version}-linux-amd64/README.txt"), opts).unwrap();
        zip.write_all(b"readme").unwrap();
        zip.start_file(format!("rclone-v{version}-linux-amd64/rclone"), opts).unwrap();
        zip.write_all(format!("#!/bin/sh\necho 'rclone v{version}'\n").as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf.into_inner()
}

fn publish(site: &Path, version: &str, zip: &[u8], listed_hash: Option<String>) {
    let dir = site.join(format!("v{version}"));
    std::fs::create_dir_all(&dir).unwrap();
    let name = format!("rclone-v{version}-linux-amd64.zip");
    std::fs::write(dir.join(&name), zip).unwrap();
    std::fs::write(site.join("version.txt"), format!("rclone v{version}\n")).unwrap();
    let mut sums = String::from("0000000000000000000000000000000000000000000000000000000000000000  rclone-other.zip\n");
    if let Some(h) = listed_hash {
        sums.push_str(&format!("{h}  {name}\n"));
    }
    std::fs::write(dir.join("SHA256SUMS"), sums).unwrap();
}

struct Server(std::process::Child, u16);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

async fn serve(site: &Path) -> Server {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let child = std::process::Command::new("python3")
        .args(["-m", "http.server", &port.to_string(), "--bind", "127.0.0.1"])
        .current_dir(site)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Server(child, port)
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_good_download_is_verified_and_installed() {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    let data = dir.path().join("data");
    let zip = make_zip("9.9.9");
    publish(&site, "9.9.9", &zip, Some(sha(&zip)));
    let server = serve(&site).await;
    let base = format!("http://127.0.0.1:{}", server.1);

    assert_eq!(rclone::latest_version(&base).await.unwrap(), "9.9.9");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let path = rclone::download(&data, &base, move |p| seen2.lock().unwrap().push(p)).await.unwrap();
    assert_eq!(path, rclone::managed_path(&data));
    assert!(path.is_file());
    assert_eq!(seen.lock().unwrap().last().copied(), Some(100));
    let found = rclone::locate(&data).await.expect("the downloaded copy should be found");
    assert!(found.managed);
    assert_eq!(found.version, "9.9.9");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tampered_or_unlisted_download_is_refused() {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let site = dir.path().join("site");
    let data = dir.path().join("data");
    let zip = make_zip("9.9.8");

    // The checksum list describes a different file.
    publish(&site, "9.9.8", &zip, Some(sha(b"something else")));
    let server = serve(&site).await;
    let base = format!("http://127.0.0.1:{}", server.1);
    let err = rclone::download(&data, &base, |_| {}).await.unwrap_err();
    assert!(err.contains("did not match"), "{err}");
    assert!(!rclone::managed_path(&data).exists());

    // The checksum list does not mention the file at all.
    publish(&site, "9.9.8", &zip, None);
    let err = rclone::download(&data, &base, |_| {}).await.unwrap_err();
    assert!(err.contains("no checksum"), "{err}");
    assert!(!rclone::managed_path(&data).exists());

    // The site is gone.
    drop(server);
    assert!(rclone::download(&data, &base, |_| {}).await.is_err());
    assert!(!rclone::managed_path(&data).exists());
}
