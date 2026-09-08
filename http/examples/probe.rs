//! Explicit read-only probe. Never loads Sentry credentials or application state.
use std::{env, error};

fn main() -> Result<(), Box<dyn error::Error>> {
    let url = env::args()
        .nth(1)
        .ok_or("usage: tls-probe https://host/path")?;
    let client = navigato_http::Client::from_system_roots(Default::default())?;
    let response = client.get(&url)?;
    println!("HTTP {}; {} bytes", response.status, response.body.len());
    Ok(())
}
