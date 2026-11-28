//! Signing in to Filen the way Filen's own apps do, against a stand-in for
//! Filen's servers whose accounts were made with Filen's own Go code. The
//! password must never be sent or kept, and what comes back must be exactly
//! the keys rclone needs.

mod common;
use common::filen_server::FakeFilen;
use synchopper_lib::filen::{self, Failure};

fn s(v: &serde_json::Value) -> &str {
    v.as_str().unwrap_or("")
}

#[tokio::test(flavor = "multi_thread")]
async fn every_kind_of_account_signs_in_and_hands_over_exactly_its_keys() {
    let fake = FakeFilen::start().await;
    let mut versions = [0; 4];
    for a in fake.accounts() {
        let name = s(&a["name"]);
        let got = filen::sign_in(&fake.url, s(&a["email"]), s(&a["password"]), s(&a["two_factor"])).await;
        if name == "v3-not-set-up" {
            assert_eq!(got, Err(Failure::NotSetUp), "{name}");
            continue;
        }
        let keys = got.unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let e = &a["expect"];
        let master_keys: Vec<String> = e["master_keys"].as_array().unwrap().iter().map(|k| s(k).to_string()).collect();
        assert_eq!(keys.email, s(&a["email"]), "{name}");
        assert_eq!(keys.api_key, s(&e["api_key"]), "{name}");
        assert_eq!(keys.master_keys, master_keys, "{name}: the current key first, then the older ones, each once");
        assert_eq!(keys.private_key, s(&e["private_key"]), "{name}");
        assert_eq!(keys.public_key, s(&e["public_key"]), "{name}: the public key as PKCS#1");
        assert_eq!(keys.auth_version as u64, e["auth_version"].as_u64().unwrap(), "{name}");
        assert_eq!(keys.base_folder, s(&e["base_folder"]), "{name}");
        versions[keys.auth_version as usize] += 1;
    }
    assert!(versions[1] >= 2 && versions[2] >= 3 && versions[3] >= 2, "{versions:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_password_never_leaves_this_pc() {
    let fake = FakeFilen::start().await;
    for a in fake.accounts() {
        let _ = filen::sign_in(&fake.url, s(&a["email"]), s(&a["password"]), s(&a["two_factor"])).await;
    }
    let requests = fake.requests();
    assert!(requests.len() > 20);
    for a in fake.accounts() {
        let password = s(&a["password"]);
        let key = s(&a["derived_key"]);
        // The password could also show up escaped in JSON.
        let escaped = serde_json::to_string(password).unwrap();
        for r in &requests {
            assert!(!r.body.contains(password) && !r.body.contains(escaped.trim_matches('"')), "{} sent the password in {}", s(&a["name"]), r.path);
            assert!(!r.body.contains(key), "{} sent its unlocked key in {}", s(&a["name"]), r.path);
        }
    }
    // What was sent to sign in is what Filen's apps send.
    let a = fake.account("v2");
    let login = requests.iter().find(|r| r.path == "/v3/login" && r.body.contains("v2@example.com")).unwrap();
    let body: serde_json::Value = serde_json::from_str(&login.body).unwrap();
    assert_eq!(body["password"], a["login"]);
    assert_eq!(body["twoFactorCode"], "XXXXXX", "no code: Filen's placeholder");
    assert_eq!(body["authVersion"], 2);
    assert!(login.auth.is_none(), "nothing secret goes in the headers before sign-in");
    let after: Vec<_> = requests.iter().filter(|r| r.path.starts_with("/v3/user/")).collect();
    assert!(after.iter().all(|r| r.auth.as_deref().is_some_and(|h| h.starts_with("Bearer "))));
}

#[tokio::test(flavor = "multi_thread")]
async fn two_factor_codes() {
    let fake = FakeFilen::start().await;
    for name in ["v2-2fa-oldest-key-format", "v3-2fa"] {
        let a = fake.account(name);
        let (email, password, code) = (s(&a["email"]), s(&a["password"]), s(&a["two_factor"]));
        assert_eq!(filen::sign_in(&fake.url, email, password, "").await, Err(Failure::NeedsCode), "{name}");
        assert_eq!(filen::sign_in(&fake.url, email, password, "000000").await, Err(Failure::WrongCode), "{name}");
        // Pasted with spaces, as authenticator apps often show it.
        let spaced = format!(" {} {} ", &code[..3], &code[3..]);
        assert!(filen::sign_in(&fake.url, email, password, &spaced).await.is_ok(), "{name}");
        // The screens show a box for the code when they see these words.
        assert!(Failure::NeedsCode.text().contains(filen::CODE_WORDS) && Failure::WrongCode.text().contains(filen::CODE_WORDS));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_or_email_is_turned_down_in_filens_words() {
    let fake = FakeFilen::start().await;
    let a = fake.account("v2");
    let refused = Failure::Refused("Invalid email or password.".into());
    assert_eq!(filen::sign_in(&fake.url, s(&a["email"]), "not the password", "").await, Err(refused.clone()));
    assert_eq!(filen::sign_in(&fake.url, "nobody@example.com", "whatever", "").await, Err(refused.clone()));
    // With two-factor sign-in on, a wrong password still says so first.
    let b = fake.account("v3-2fa");
    assert_eq!(filen::sign_in(&fake.url, s(&b["email"]), "not the password", "").await, Err(refused.clone()));
    // A password is used exactly as typed, spaces included.
    let c = fake.account("v2-spaces-duplicate-keys");
    let password = s(&c["password"]);
    assert!(password.starts_with(' ') && password.ends_with(' '));
    assert!(filen::sign_in(&fake.url, s(&c["email"]), password, "").await.is_ok());
    assert_eq!(filen::sign_in(&fake.url, s(&c["email"]), password.trim(), "").await, Err(refused));
}

#[tokio::test(flavor = "multi_thread")]
async fn trouble_reaching_filen_or_reading_its_answers() {
    let fake = FakeFilen::start().await;
    assert_eq!(filen::sign_in(&fake.url, "busy@example.com", "x", "").await, Err(Failure::Busy));
    match filen::sign_in(&fake.url, "broken@example.com", "x", "").await {
        Err(Failure::Unreachable(d)) => assert!(d.contains("503"), "{d}"),
        other => panic!("{other:?}"),
    }
    // Nothing listening.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    assert!(matches!(filen::sign_in(&format!("http://{closed}"), "a@b.c", "x", "").await, Err(Failure::Unreachable(_))));
    // Keys that don't open with this account's password.
    let a = fake.account("v2");
    match filen::sign_in(&fake.url, "bad-keys@example.com", s(&a["password"]), "").await {
        Err(Failure::Unexpected(d)) => assert!(d.contains("could not be unlocked"), "{d}"),
        other => panic!("{other:?}"),
    }
    // None of these say anything that would show the box for a code.
    for f in [Failure::Busy, Failure::Unreachable("x".into()), Failure::Unexpected("x".into()), Failure::NotSetUp] {
        assert!(!f.text().contains(filen::CODE_WORDS));
    }
}
