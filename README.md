# discord-webhook-storage

Use Discord as file storage from Rust. Upload a file through a webhook, keep the
reference it gives you, and stream it back later, whole or by byte range.

Everything goes through the standard `futures::AsyncWrite` / `AsyncRead`
traits, so uploading looks like writing to a file and downloading looks like
reading from one.

## Installation

```toml
[dependencies]
discord-webhook-storage = "0.1.0"
futures = "0.3"
```

## Quick start

### Upload a file

Open a `WriteFile`, write to it, then call `finish()`. That last call is what
gives you a `DiscordFile`, the reference you'll use to get the file back.

```rust,no_run
use discord_webhook_storage::{DiscordFile, Result, WebhookCredentials, WriteConfig, WriteFile};
use futures::{AsyncRead, AsyncWrite, AsyncWriteExt};

async fn upload<T>(connection: T, webhook_url: &str) -> Result<DiscordFile>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let credentials = WebhookCredentials::parse(webhook_url)?;
    let mut file = WriteFile::open(connection, &credentials, &WriteConfig::default()).await?;

    file.write_all(b"hello from Rust").await?;
    file.finish().await
}
```

The file lands in the channel the webhook points to.

> **Always call `finish()`.** It's the only confirmation that Discord stored the
> file. Dropping the writer, or calling `close()`, does not complete the upload.
> (`flush()` just sends the bytes written so far.)

**Size limit:** files are named `file.bin` and capped at 20,000,000 bytes.
These defaults can't be changed yet. A write that would go over the cap fails
without consuming any of its bytes. Discord may enforce its own, different maximum.

### Download it back

Pass the reference to `ReadFile::open()` and read as usual.

```rust,no_run
use discord_webhook_storage::{DiscordFile, ReadFile, Result};
use futures::{AsyncRead, AsyncReadExt, AsyncWrite};

async fn download<T>(connection: T, file: &DiscordFile) -> Result<Vec<u8>>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut reader = ReadFile::open(connection, file).await?;
    let mut contents = Vec::new();
    reader.read_to_end(&mut contents).await?;
    Ok(contents)
}
```

To read only part of a file:

```rust,ignore
ReadFile::open_with_range(connection, &file, start, Some(end)).await?
```

Offsets start at 0 and `end` is inclusive (and must be greater than `start`).
Use `None` as the end to read until the end of the file.

If a download is interrupted, you get an error rather than truncated data.

## Saving references

A `DiscordFile` is a message ID plus a download URL. You can store it two ways:

- **With Serde**, like any other struct.
- **As a string**: `file.to_string()` gives `discord://<id>/<url>`, and
  `DiscordFile::parse()` reads it back.

## Download URLs expire

Discord download URLs don't last forever, and saving a reference doesn't extend
them. Two things to know:

- `file.is_valid()` tells you whether the URL is still within its expiry date,
  based on your local clock. It does **not** check that the file still exists on Discord.
- If the URL has expired, **renew it before opening the file**. Renewing updates
  the URL inside your reference, so you can keep using the same one.

Renewal works on URLs that are expired or still valid. Pick the method that fits:

| You want to renew | Method | Credentials | URLs per request |
| --- | --- | --- | --- |
| One file | `DiscordFile::renew()` | The webhook that uploaded it | 1 |
| Many files | `renew_urls()` | A bot token (`BotCredentials`) | Up to 50 |

### One file, with the webhook

```rust,no_run
use discord_webhook_storage::{DiscordFile, Result, WebhookCredentials};
use futures::{AsyncRead, AsyncWrite};

async fn refresh_one<T>(
    connection: T,
    file: &mut DiscordFile,
    credentials: &WebhookCredentials<'_>,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    file.renew(connection, credentials).await
}
```

Use the same webhook that created the file. Another webhook returns an HTTP
error. If renewal fails, the reference is left untouched.

### Many files, with a bot token

If you have a collection of files, prefer `renew_urls()`: one request covers up
to 50 URLs, which means fewer round trips and far less pressure on the rate limit.

```rust,no_run
use discord_webhook_storage::{BotCredentials, DiscordFile, Result, renew_urls};
use futures::{AsyncRead, AsyncWrite};

async fn refresh_many<T>(
    connection: T,
    files: &mut [DiscordFile],
    bot_token: &str,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let credentials = BotCredentials { token: bot_token };
    renew_urls(
        connection,
        &credentials,
        files.iter_mut().map(|file| &mut file.url),
    )
    .await
}
```

- If you hold `DiscordFileUrl` values directly, pass `&mut urls`. Mutable
  vectors, slices, arrays and iterators all work.
- Collections larger than 50 are split into several requests automatically.
- Duplicate URLs are all updated.
- Empty input does nothing.
- If a failure happens midway, the URLs already renewed stay renewed.

## Rate limits and errors

The library **doesn't schedule or retry requests for you**, so plan your
throughput with these approximate budgets:

| Scope | Burst capacity | Sustained rate |
| --- | --- | --- |
| Webhook requests (`WebhookCredentials`) | 5 requests | 2.5 / s |
| Uploads to one channel (shared by all its webhooks) | 30 requests | 0.5 / s |
| Grouped renewal (`BotCredentials`) | 10 requests | 5 / s |

Uploads have to fit within both the webhook budget and the channel budget.
Grouped renewal is counted per request, not per URL, which is why renewing in
batches goes a long way.

These numbers are estimates: Discord can change its limits at any time (see
[their documentation](https://docs.discord.com/developers/topics/rate-limits)).

When Discord rejects a request you get `Error::HttpStatus { status, body }`,
with the original status and error response. On `429`, wait for the
`retry_after` duration in the body, then try again. Other errors cover
connection failures, invalid file references and unusable responses.

After any error or interrupted operation, **throw away the connection** and open a new one.

## Bringing your own connection

The library doesn't open connections itself. You give it an already established
**TLS, HTTP/1.1** connection that implements `futures::AsyncRead`,
`futures::AsyncWrite` and `Unpin`:

| Operation | Connect to |
| --- | --- |
| Upload, renew URLs | `discord.com:443` |
| Download | `cdn.discordapp.com:443` |

Pass `&mut connection` if you want to keep ownership. To reuse a connection:

- **After an upload or renewal**: reuse it if it's still open.
- **After a download**: read to the end and drop the reader first.

## Try it

The examples use the optional `tokio-tcp-pool` feature, which sets up TLS for
you. Files uploaded by the demos stay in the webhook's channel.

```sh
# Upload generated data
DISCORD_WEBHOOK_URL='https://discord.com/api/webhooks/<id>/<token>' \
    cargo run --example webhook_upload --features tokio-tcp-pool

# Upload a file, read a byte range, verify the contents
DISCORD_WEBHOOK_URL='https://discord.com/api/webhooks/<id>/<token>' \
    cargo run --example webhook_upload_download --features tokio-tcp-pool

# Upload a file and renew its URL with the same webhook
DISCORD_WEBHOOK_URL='https://discord.com/api/webhooks/<id>/<token>' \
    cargo run --example webhook_upload_renew --features tokio-tcp-pool

# Upload four 1 MB files in parallel, wait 3 s, renew all four URLs with a bot
# token in a single request, then download and verify them
DISCORD_WEBHOOK_URL='https://discord.com/api/webhooks/<id>/<token>' \
DISCORD_BOT_TOKEN='<bot-token>' \
    cargo run --example webhook_parallel_upload_renew_download --features tokio-tcp-pool
```

## Benchmark Discord

One run with ten 20 MB files on a 1,000/500 Mbps connection:

| Operation | Observed throughput | Mean time per file |
| --- | --- | --- |
| Upload, including Discord confirmation | 5.1 MB/s | 3.95 s |
| First download | 26.9 MB/s | 0.74 s |
| Repeated download | 75.3 MB/s | 0.27 s |

Includes TCP/TLS setup; excludes pauses between uploads. Results vary by host
and network. [Full report](docs/benchmark1.md) · [Run the benchmark](docs/benchmark.md).

```sh
DISCORD_WEBHOOK_URL='https://discord.com/api/webhooks/<id>/<token>' \
    cargo bench --locked --bench discord_benchmark --features tokio-tcp-pool
```

## API docs

```sh
cargo doc --all-features --no-deps --open
```

## License

MIT
