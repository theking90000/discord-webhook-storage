//! Upload four 1 MB files concurrently, wait three seconds, renew their URLs,
//! then download and verify all four files.
//!
//! Run with `DISCORD_WEBHOOK_URL=... DISCORD_BOT_TOKEN=... cargo run --example webhook_parallel_upload_renew_download --features tokio-tcp-pool`.
//! The uploaded attachments remain in the webhook's channel after the example.

use std::{error::Error, io, sync::Arc, time::Duration};

use discord_webhook_storage::{
    BotCredentials, ReadFile, WebhookCredentials, WriteConfig, WriteFile, renew_urls,
};
use futures::{AsyncReadExt, AsyncWriteExt, future::try_join_all};
use tokio_tcp_pool::{Pool, Route, rustls};

const FILE_COUNT: usize = 4;
const FILE_SIZE: usize = 1_000_000;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let webhook_url = std::env::var("DISCORD_WEBHOOK_URL")?;
    let credentials = WebhookCredentials::parse(&webhook_url)?;
    let bot_token = std::env::var("DISCORD_BOT_TOKEN")?;
    let bot_credentials = BotCredentials { token: &bot_token };

    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let api_pool = Pool::builder(Route::Direct {
        target: "discord.com:443".parse()?,
    })
    .max_open(FILE_COUNT)
    .tls(Arc::clone(&tls))
    .build()?;
    let download_pool = Pool::builder(Route::Direct {
        target: "cdn.discordapp.com:443".parse()?,
    })
    .max_open(FILE_COUNT)
    .tls(tls)
    .build()?;

    let payloads: Vec<Vec<u8>> = (0..FILE_COUNT)
        .map(|index| {
            (0..FILE_SIZE)
                .map(|offset| ((offset + index) % 251) as u8)
                .collect()
        })
        .collect();

    let upload_pool = &api_pool;
    let upload_credentials = &credentials;
    let mut files = try_join_all(
        payloads
            .iter()
            .enumerate()
            .map(|(index, payload)| async move {
                let mut connection = upload_pool.acquire().await?;
                let mut writer =
                    WriteFile::open(&mut connection, upload_credentials, &WriteConfig::default())
                        .await?;
                writer.write_all(payload).await?;
                let mut file = writer.finish().await?;
                // Only successful operations release connections back to the pool.
                connection.release();
                println!(
                    "Uploaded file {}: {FILE_SIZE} bytes, message {}",
                    index + 1,
                    file.id
                );
                file.url.hm = String::new();
                Ok::<_, Box<dyn Error>>(file)
            }),
    )
    .await?;

    println!("All uploads finished. Waiting three seconds before renewal.");
    tokio::time::sleep(Duration::from_secs(3)).await;

    let mut connection = api_pool.acquire().await?;
    // Four URLs fit in one request and are updated directly in the file references.
    renew_urls(
        &mut connection,
        &bot_credentials,
        files.iter_mut().map(|file| &mut file.url),
    )
    .await?;
    connection.release();
    println!("Renewed all four URLs in one request.");

    let download_pool = &download_pool;
    try_join_all(files.iter().zip(&payloads).enumerate().map(
        |(index, (file, payload))| async move {
            let mut connection = download_pool.acquire().await?;
            let mut downloaded = Vec::with_capacity(FILE_SIZE);
            {
                let mut reader = ReadFile::open(&mut connection, file).await?;
                reader.read_to_end(&mut downloaded).await?;
            }
            if downloaded.as_slice() != payload.as_slice() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("downloaded file {} differs from its upload", index + 1),
                )
                .into());
            }
            connection.release();
            println!(
                "Downloaded and verified file {}: {} bytes",
                index + 1,
                downloaded.len()
            );
            Ok::<_, Box<dyn Error>>(())
        },
    ))
    .await?;

    Ok(())
}
