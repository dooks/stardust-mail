//! `gtoken login [--hint <email>] | list | check <email>`. Never prints tokens.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["login"] => login(None).await,
        ["login", "--hint", h] => login(Some(h)).await,
        ["list"] => list(),
        ["check", email] => check(email).await,
        _ => {
            eprintln!("usage: gtoken login [--hint <email>] | list | check <email>");
            return ExitCode::from(2);
        }
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn login(hint: Option<&str>) -> Result<(), gtoken::Error> {
    let a = gtoken::login(hint).await?;
    println!("signed in: {} (messagesTotal {})", a.info.email, a.profile.messages_total);
    Ok(())
}

fn list() -> Result<(), gtoken::Error> {
    let all = gtoken::accounts()?;
    if all.is_empty() {
        println!("(no accounts; run `gtoken login`)");
    }
    for a in all {
        println!("{}  [{}]  {}", a.email, a.scopes.join(" "), gtoken::format_utc(a.created_at));
    }
    Ok(())
}

async fn check(email: &str) -> Result<(), gtoken::Error> {
    let t = gtoken::access_token_full(email).await?;
    let p = gtoken::profile(email).await?; // reuses the cached access token
    println!("email:         {}", p.email_address);
    println!("messagesTotal: {}", p.messages_total);
    println!("historyId:     {}", p.history_id);
    println!("scopes:        {}", t.scope);
    Ok(())
}
