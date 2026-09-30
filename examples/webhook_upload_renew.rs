//! Upload a file, then renew its download URL with the same webhook credentials.
//!
//! Run with `DISCORD_WEBHOOK_URL=... cargo run --example webhook_upload_renew --features tokio-tcp-pool`.
//! The uploaded attachment remains in the webhook's channel after the example.
//! An immediate renewal may return the same URL while it is still valid.

use std::{error::Error, sync::Arc};

use discord_webhook_storage::{WebhookCredentials, WriteConfig, WriteFile};
use futures::AsyncWriteExt;
use tokio_tcp_pool::{Pool, Route, rustls};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let webhook_url = std::env::var("DISCORD_WEBHOOK_URL")?;
    let credentials = WebhookCredentials::parse(&webhook_url)?;

    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    // Upload and renewal both require TLS connections to discord.com:443.
    let pool = Pool::builder(Route::Direct {
        target: "discord.com:443".parse()?,
    })
    .max_open(1)
    .tls(Arc::new(tls))
    .build()?;

    let mut stored_file = {
        let mut connection = pool.acquire().await?;
        let mut writer =
            WriteFile::open(&mut connection, &credentials, &WriteConfig::default()).await?;
        writer.write_all(b"hello from Rust").await?;
        let file = writer.finish().await?;
        // Release only after success. Errors drop the unreleased connection.
        connection.release();
        file
    };
    println!("Uploaded file in message {}", stored_file.id);
    println!("Upload URL: {}", stored_file.url);
    println!("URL is valid: {}", stored_file.is_valid());

    let mut connection = pool.acquire().await?;
    // Use the credentials of the webhook that created the message.
    // Wrong credentials return Error::HttpStatus with Discord's response body.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    stored_file.renew(&mut connection, &credentials).await?;
    connection.release();

    println!("Renewed URL: {}", stored_file.url);
    println!("URL is valid: {}", stored_file.is_valid());
    Ok(())
}
