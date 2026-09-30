# Discord webhook storage

Upload files through Discord webhooks, save their references, and read them
back with an asynchronous file API. Stream file contents with the standard
`futures::AsyncWrite` and `futures::AsyncRead` operations, read selected byte
ranges, and renew download URLs when needed.

Uploads and individual renewals use webhook credentials. Renewing many URLs
together requires a bot token and is more efficient, with up to 50 URLs renewed
per request.

## Installation

```toml
[dependencies]
discord-webhook-storage = "0.1.0"
futures = "0.3"
```

## Writing a file

Open a `WriteFile`, write its contents, then call `finish()` to obtain the
`DiscordFile` reference. The credentials select the webhook's destination
channel.

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

Only a successful `finish()` confirms that Discord stored the file. Dropping
the writer does not complete the upload. `flush()` sends pending bytes;
`close()` ends writing, but `finish()` is still required to obtain the reference.

`WriteConfig::default()` names the file `file.bin` and limits its size to
20,000,000 bytes. These settings cannot currently be customized. A write that
would exceed the limit fails without accepting bytes from that call. Discord
may enforce a different maximum.

## Keeping a file reference

`DiscordFile` contains a message identifier and a `DiscordFileUrl`. Save the
reference with Serde, or call `to_string()` to obtain a `discord://<id>/<url>`
string that `DiscordFile::parse()` can read back.

Download URLs expire. `file.is_valid()` checks the URL's expiration using the
local clock; it does not check whether the file still exists on Discord.
Saving a reference does not extend the URL's validity. Renewal updates its URL
so the same reference can be used for another download.

## Reading a file

Open a `ReadFile` with the saved reference, then use the usual asynchronous
read operations. `ReadFile::open()` accepts a `DiscordFile` or a `DiscordFileUrl`.

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

For part of a file, use
`ReadFile::open_with_range(connection, &file, start, Some(end))`. Offsets start
at zero, and `end` is inclusive and must be greater than `start`. Passing `None`
reads from `start` to the end of the file.

Renew an expired URL before opening it for reading. An interrupted download
returns an error.

## Renewing download URLs

Both renewal methods update URLs in place and accept URLs that have already
expired or are still valid.

| Operation | Credentials | URLs per request |
| --- | --- | --- |
| `DiscordFile::renew()` | The webhook that created the file's message | One |
| `renew_urls()` | A bot token through `BotCredentials` | Up to 50 |

### One file with webhook credentials

Use `file.renew()` to keep an individual file reference usable with the same
webhook credentials used for its upload. Credentials for another webhook cause
an HTTP error. A failed renewal leaves that file reference unchanged.

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

### Multiple files with a bot token

Use `renew_urls()` for collections of files. A bot token is required for this
method. Renewing up to 50 URLs together takes one request instead of one request
per file, reducing round trips and the number of requests charged against the
renewal rate limit.

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

For a collection of `DiscordFileUrl` values, pass `&mut urls` directly. Mutable
vectors, slices, arrays, and iterators are accepted. The function handles larger
collections with multiple requests and updates every repeated URL. Empty input
does nothing. If renewal fails partway through a collection, earlier successful
updates remain in place.

## Rate limits and errors

Indicative request budgets for planning uploads and renewals:

| Scope | Bucket capacity | Refill rate |
| --- | --- | --- |
| Webhook requests with `WebhookCredentials` | 5 requests | 2.5 requests/s |
| Uploads to the same channel, shared across webhooks | 30 requests | 0.5 requests/s |
| Grouped renewal with `BotCredentials` | 10 requests | 5 requests/s |

Bucket capacity describes the available burst; refill rate describes the
sustained request budget. Uploads must respect both the webhook and shared
channel budgets. Grouped renewal counts requests, not individual URLs, so
renewing several files together makes better use of the available budget.

The library does not schedule requests or retry failures automatically.
`Error::HttpStatus { status, body }` preserves Discord's HTTP status and error
response. On status `429`, wait for the response body's `retry_after` duration
before retrying. Discord's limits can change, so these figures are planning
estimates rather than guarantees. See
[Discord's rate limit documentation](https://docs.discord.com/developers/topics/rate-limits).

Other errors distinguish connection failures, invalid file references, and
unusable responses. Discard a retained connection after an error or interrupted
operation.

## Connections

Supply an already connected TLS connection using HTTP/1.1:

| Operation | Destination |
| --- | --- |
| Upload or renew URLs | `discord.com:443` |
| Download files | `cdn.discordapp.com:443` |

Connections must implement `futures::AsyncRead`, `futures::AsyncWrite`, and
`Unpin`. Pass `&mut connection` to keep ownership. After successful upload or
renewal, reuse it only if it is still open. For downloads, read to the end and
drop the reader before reusing the connection.

## Runnable demos

The demos set up TLS with the optional `tokio-tcp-pool` feature. The library
also accepts other connections that meet the requirements above. Uploaded files
remain in the webhook's channel after a demo.

Upload generated data:

```sh
DISCORD_WEBHOOK_URL='https://discord.com/api/webhooks/<id>/<token>' \
    cargo run --example webhook_upload --features tokio-tcp-pool
```

Upload a file, read a byte range, and verify its contents:

```sh
DISCORD_WEBHOOK_URL='https://discord.com/api/webhooks/<id>/<token>' \
    cargo run --example webhook_upload_download --features tokio-tcp-pool
```

Upload a file and renew its URL with the same webhook:

```sh
DISCORD_WEBHOOK_URL='https://discord.com/api/webhooks/<id>/<token>' \
    cargo run --example webhook_upload_renew --features tokio-tcp-pool
```

Upload four 1 MB files concurrently, wait three seconds, renew all four URLs
with a bot token in one request, then download and verify all four files:

```sh
DISCORD_WEBHOOK_URL='https://discord.com/api/webhooks/<id>/<token>' \
DISCORD_BOT_TOKEN='<bot-token>' \
    cargo run --example webhook_parallel_upload_renew_download --features tokio-tcp-pool
```

## Documentation

```sh
cargo doc --all-features --no-deps --open
```

MIT licensed.
