//! `s1cli`: token-error matrix harness. Prints HTTP status + `error` field only, never a token.
//!
//! * `garbage`           labels.list with a garbage bearer, with no Authorization header
//! * `revoke-garbage`    POST /revoke with a garbage token (revokes nothing)
//! * `refresh-garbage`   refresh_token grant with a garbage refresh token
//! * `tokeninfo-garbage` POST /tokeninfo with a garbage access token
//! * `matrix`            all of the four above
//! * `gtoken-check <email>`  S0's account: refresh, tokeninfo (POST), read-only verdict, labels.list count
//! * `gtoken-list`       S0's accounts with sign-in dates
//!
//! The only non-GET calls are POST /token, /revoke (garbage token only) and /tokeninfo.

use authcore::Error;
use std::process::ExitCode;

fn show<T: std::fmt::Debug>(label: &str, r: Result<T, Error>) {
    match r {
        Ok(v) => println!("{label}: {v:?}"),
        Err(Error::OAuth { status, error }) => println!("{label}: HTTP {status} error={error}"),
        Err(Error::Api { status, error }) => println!("{label}: HTTP {status} error={error}"),
        Err(e) => println!("{label}: {e}"),
    }
}

async fn garbage() {
    match authcore::labels_probe(Some("ya29.garbage-not-a-token")).await {
        Ok(p) => println!("labels.list garbage bearer : {}", p.describe()),
        Err(e) => println!("labels.list garbage bearer : {e}"),
    }
    match authcore::labels_probe(None).await {
        Ok(p) => println!("labels.list no auth header : {}", p.describe()),
        Err(e) => println!("labels.list no auth header : {e}"),
    }
}

async fn revoke_garbage() {
    let o = authcore::revoke("garbage-not-a-token", authcore::REVOKE_ENDPOINT).await;
    println!("revoke garbage token       : result={} status={:?} error={:?}", o.result, o.status, o.error);
}

async fn refresh_garbage() {
    let c = match authcore::load_client() {
        Ok(c) => c,
        Err(e) => return println!("refresh garbage            : {e}"),
    };
    let resp = authcore::http()
        .post(authcore::TOKEN_ENDPOINT)
        .form(&[
            ("client_id", c.client_id.as_str()),
            ("client_secret", c.client_secret.as_str()),
            ("refresh_token", "1//garbage-not-a-token"),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let v: serde_json::Value = r.json().await.unwrap_or_default();
            println!("refresh garbage token      : HTTP {status} error={}", v["error"].as_str().unwrap_or("?"));
        }
        Err(e) => println!("refresh garbage token      : {}", e.without_url()),
    }
}

async fn tokeninfo_garbage() {
    show("tokeninfo garbage (POST)  ", authcore::tokeninfo("ya29.garbage-not-a-token").await);
}

async fn gtoken_check(email: &str) {
    let at = match gtoken::access_token_full(email).await {
        Ok(t) => t,
        Err(gtoken::Error::Revoked(_)) => return println!("{email}: refresh -> invalid_grant (HTTP 400): grant revoked/expired"),
        Err(e) => return println!("{email}: refresh failed: {e}"),
    };
    println!("{email}: refresh OK (refresh response scopes: {})", at.scope.replace("https://www.googleapis.com/auth/", ""));
    match authcore::tokeninfo(&at.token).await {
        Ok(ti) => {
            let v = authcore::judge_scopes(&ti.scopes);
            println!(
                "{email}: tokeninfo(POST) scopes: {}  read_only={} extra={:?}",
                ti.scopes.join(" ").replace("https://www.googleapis.com/auth/", ""),
                v.read_only,
                v.extra
            );
        }
        Err(e) => println!("{email}: tokeninfo failed: {e}"),
    }
    match authcore::labels_probe(Some(&at.token)).await {
        Ok(p) => println!("{email}: {}", p.describe()),
        Err(e) => println!("{email}: labels probe failed: {e}"),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["garbage"] => garbage().await,
        ["revoke-garbage"] => revoke_garbage().await,
        ["refresh-garbage"] => refresh_garbage().await,
        ["tokeninfo-garbage"] => tokeninfo_garbage().await,
        ["matrix"] => {
            garbage().await;
            tokeninfo_garbage().await;
            refresh_garbage().await;
            revoke_garbage().await;
        }
        ["gtoken-check", email] => gtoken_check(email).await,
        ["gtoken-list"] => match gtoken::accounts() {
            Ok(a) => a.iter().for_each(|a| println!("{}  signed in {}", a.email, gtoken::format_utc(a.created_at))),
            Err(e) => println!("{e}"),
        },
        _ => {
            eprintln!("usage: s1cli garbage|revoke-garbage|refresh-garbage|tokeninfo-garbage|matrix|gtoken-check <email>|gtoken-list");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}
