use crate::{
    BotCredentials, DiscordFileUrlError, Error, ResponseError, Result, WebhookCredentials,
    http::HttpStatusParser,
};
use futures::{AsyncRead, AsyncWrite, AsyncWriteExt};
use std::collections::HashMap;

const MAX_RESPONSE_BODY_SIZE: usize = 16384;
const REFRESH_BATCH_SIZE: usize = 50;
const MAX_REFRESH_RESPONSE_BODY_SIZE: usize = 1024 * 1024;

#[derive(serde::Serialize)]
struct RefreshUrlsRequest<'a> {
    attachment_urls: Vec<&'a str>,
}

#[derive(serde::Deserialize)]
struct RefreshUrlsResponse {
    refreshed_urls: Vec<RefreshedUrl>,
}

#[derive(serde::Deserialize)]
struct RefreshedUrl {
    original: String,
    refreshed: String,
}

/// Renew attachment download URLs in place using bot credentials.
///
/// Pass a mutable vector, slice, or array of [`DiscordFileUrl`] values, or an
/// iterator of mutable references. To renew stored files directly, pass
/// `files.iter_mut().map(|file| &mut file.url)`. Both expired and still-valid
/// URLs can be renewed. Empty input does nothing.
///
/// `connection` must already be a secure HTTP/1.1 connection to
/// `discord.com:443`. Pass `&mut connection` to retain ownership.
///
/// On success, all supplied URLs have been updated. If renewal fails, some
/// URLs may already have been updated. Failures, including rate limits, are
/// returned without automatic retries. Discard the connection after an error
/// or interrupted renewal. Reuse it after success only if it is still open.
///
/// # Errors
///
/// Invalid bot credentials return [`Error::InvalidBotToken`]. Connection
/// failures return [`Error::IoError`]. If Discord rejects the renewal,
/// [`Error::HttpStatus`] contains its status code and error response.
///
/// Unusable responses return [`Error::HttpParseError`], [`Error::JsonError`],
/// or [`Error::InvalidResponse`]. Invalid renewed URLs return
/// [`Error::InvalidDiscordFileUrl`].
pub async fn renew_urls<'a, T, I>(
    mut connection: T,
    credentials: &BotCredentials<'_>,
    urls: I,
) -> Result<()>
where
    T: AsyncRead + AsyncWrite + Unpin,
    I: IntoIterator<Item = &'a mut DiscordFileUrl>,
{
    if credentials.token.is_empty()
        || !credentials
            .token
            .bytes()
            .all(|byte| (b'!'..=b'~').contains(&byte))
    {
        return Err(Error::InvalidBotToken);
    }

    let mut urls = urls.into_iter();
    let mut completed = 0;
    loop {
        let batch: Vec<&mut DiscordFileUrl> = urls.by_ref().take(REFRESH_BATCH_SIZE).collect();
        if batch.is_empty() {
            return Ok(());
        }
        let originals: Vec<String> = batch.iter().map(|url| url.to_string()).collect();
        let request = RefreshUrlsRequest {
            attachment_urls: originals.iter().map(|url| url.as_str()).collect(),
        };
        let body = serde_json::to_vec(&request)?;
        let headers = format!(
            "POST /api/v9/attachments/refresh-urls HTTP/1.1\r\nHost: discord.com\r\nAuthorization: Bot {}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\n\r\n",
            credentials.token,
            body.len(),
        );
        connection.write_all(headers.as_bytes()).await?;
        connection.write_all(&body).await?;
        connection.flush().await?;

        let mut parser = HttpStatusParser::new(&mut connection);
        let status = parser.status().await?;
        let mut parser = parser.into_headers();
        while !parser.is_complete() {
            parser.next_header().await?;
        }
        if status < 200 || matches!(status, 204 | 304) {
            return Err(Error::HttpStatus {
                status,
                body: Vec::new(),
            });
        }
        let limit = if status == 200 {
            MAX_REFRESH_RESPONSE_BODY_SIZE
        } else {
            MAX_RESPONSE_BODY_SIZE
        };
        let body = parser.body(limit).await?;
        if status != 200 {
            return Err(Error::HttpStatus { status, body });
        }

        let response: RefreshUrlsResponse = serde_json::from_slice(&body)?;
        let mut results = HashMap::new();
        for entry in response.refreshed_urls {
            if !request.attachment_urls.contains(&entry.original.as_str()) {
                return Err(ResponseError::InvalidRefreshedUrls.into());
            }
            let url = DiscordFileUrl::parse(&entry.refreshed)?;
            if let Some(previous) = results.get(&entry.original) {
                if previous != &url {
                    return Err(ResponseError::InvalidRefreshedUrls.into());
                }
            } else {
                results.insert(entry.original, url);
            }
        }
        let updates: Vec<DiscordFileUrl> = originals
            .iter()
            .enumerate()
            .map(|(offset, original)| {
                results.get(original).cloned().ok_or_else(|| {
                    ResponseError::MissingRefreshedUrl {
                        index: completed + offset,
                    }
                    .into()
                })
            })
            .collect::<Result<_>>()?;
        completed += batch.len();
        for (target, url) in batch.into_iter().zip(updates) {
            *target = url;
        }
    }
}

#[derive(serde::Deserialize)]
struct WebhookMessage {
    attachments: Vec<WebhookAttachment>,
}

#[derive(serde::Deserialize)]
struct WebhookAttachment {
    id: String,
    url: String,
}

/// A reference to a file stored on Discord.
///
/// Returned by [`crate::WriteFile::finish`] and accepted by
/// [`crate::ReadFile::open`]. Use Serde to save this reference, or
/// [`ToString::to_string`] to obtain a `discord://<id>/<url>` string.
/// The download URL expires even if the reference is saved.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiscordFile {
    /// Identifier of the Discord message that stores the file.
    pub id: String,
    /// Download URL for the file, including its expiration time.
    pub url: DiscordFileUrl,
}

impl DiscordFile {
    /// Check whether the download URL has not yet expired.
    ///
    /// Delegates to [`DiscordFileUrl::is_valid`] using the local clock without
    /// contacting Discord. This does not check whether the file still exists.
    pub fn is_valid(&self) -> bool {
        self.url.is_valid()
    }

    /// Renew the download URL by fetching the message that stores this file.
    ///
    /// `connection` must already be a secure HTTP/1.1 connection to
    /// `discord.com:443`. Pass `&mut connection` to retain ownership.
    /// `credentials` must belong to the webhook that created this message.
    /// Credentials for another webhook cause an HTTP error from Discord;
    /// inspect [`Error::HttpStatus`] for its status code and response body.
    ///
    /// Sends `GET /api/webhooks/{id}/{token}/messages/{message_id}` and selects
    /// the attachment with the stored attachment identifier. This also works
    /// when the current URL has expired. Only a successful renewal updates
    /// [`Self::url`]; the message identifier remains the same.
    ///
    /// # Errors
    ///
    /// Returns [`Error::IoError`] on connection failure, [`Error::HttpStatus`]
    /// on a status other than 200, or [`Error::HttpParseError`] on an unreadable
    /// or oversized HTTP response. The response body is limited to 16 KiB.
    /// Invalid JSON or attachment fields return [`Error::JsonError`]. A missing
    /// attachment returns [`Error::InvalidResponse`], and an invalid download
    /// URL returns [`Error::InvalidDiscordFileUrl`].
    ///
    /// Discard the connection after an error or interrupted renewal. Reuse it
    /// after success only if it is still open.
    pub async fn renew<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        mut connection: T,
        credentials: &WebhookCredentials<'_>,
    ) -> Result<()> {
        let request = format!(
            "GET /api/webhooks/{}/{}/messages/{} HTTP/1.1\r\nHost: discord.com\r\nAccept: application/json\r\n\r\n",
            path_segment(credentials.id),
            path_segment(credentials.token),
            path_segment(&self.id),
        );
        connection.write_all(request.as_bytes()).await?;
        connection.flush().await?;

        let mut parser = HttpStatusParser::new(&mut connection);
        let status = parser.status().await?;
        let mut parser = parser.into_headers();
        while !parser.is_complete() {
            parser.next_header().await?;
        }
        // These statuses cannot carry an HTTP response body.
        if status < 200 || matches!(status, 204 | 304) {
            return Err(Error::HttpStatus {
                status,
                body: Vec::new(),
            });
        }
        let body = parser.body(MAX_RESPONSE_BODY_SIZE).await?;
        if status != 200 {
            return Err(Error::HttpStatus { status, body });
        }

        let message: WebhookMessage = serde_json::from_slice(&body)?;
        let attachment_id = self.url.attachment_id.to_string();
        let attachment = message
            .attachments
            .into_iter()
            .find(|attachment| attachment.id == attachment_id)
            .ok_or(ResponseError::AttachmentNotFound {
                attachment_id: self.url.attachment_id,
            })?;
        let url = DiscordFileUrl::parse(&attachment.url)?;
        self.url = url;
        Ok(())
    }

    /// Read a file reference saved as a `discord://<id>/<url>` string.
    ///
    /// This checks the format without contacting Discord or checking expiration.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDiscordFileUrl`] if the string has the wrong
    /// format, an invalid message identifier, or an invalid download URL.
    /// The error does not contain the input string.
    pub fn parse(value: &str) -> Result<Self> {
        let file = value
            .strip_prefix("discord://")
            .ok_or(DiscordFileUrlError::InvalidField { field: "prefix" })?;
        let (id, url) = file
            .split_once('/')
            .ok_or(DiscordFileUrlError::MissingField { field: "url" })?;
        let id = required(Some(id), "id")?;
        if id.contains(['?', '#'])
            || id.chars().any(char::is_whitespace)
            || id.chars().any(char::is_control)
        {
            return Err(DiscordFileUrlError::InvalidField { field: "id" }.into());
        }
        let url = DiscordFileUrl::parse(required(Some(url), "url")?)?;
        Ok(Self {
            id: id.to_owned(),
            url,
        })
    }
}

fn path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    encoded
}

impl TryFrom<&str> for DiscordFile {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl AsRef<DiscordFileUrl> for DiscordFile {
    fn as_ref(&self) -> &DiscordFileUrl {
        &self.url
    }
}

/// A temporary URL for downloading a stored file.
///
/// Pass it to [`crate::ReadFile::open`] to read the file. Use [`Self::is_valid`]
/// to check whether it has expired, or [`ToString::to_string`] to obtain the URL.
/// Serde serialization saves the URL fields without extending its lifetime.
///
/// Only the file path and the `ex`, `is`, and `hm` URL parameters are kept.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiscordFileUrl {
    /// Identifier of the Discord channel that stores the file.
    pub channel_id: u64,
    /// Identifier of the file attachment in Discord.
    pub attachment_id: u64,
    /// File name as it appears in the URL, with characters such as spaces still encoded.
    pub attachment_name: String,
    /// Download URL expiration time, in seconds since the Unix epoch.
    pub ex: u64,
    /// Value of the `is` URL parameter required by Discord, stored as an integer.
    pub is: u64,
    /// Signature in the `hm` URL parameter required by Discord.
    pub hm: String,
}

impl DiscordFileUrl {
    /// Check whether the download URL has not yet expired.
    ///
    /// Uses the local clock without contacting Discord. A `true` result does
    /// not guarantee that the file still exists or can be downloaded.
    /// Returns `false` if the system clock is before the Unix epoch.
    pub fn is_valid(&self) -> bool {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .is_ok_and(|now| now.as_secs() < self.ex)
    }

    /// Read a file download URL provided by Discord.
    ///
    /// The URL must start with `https://cdn.discordapp.com/attachments/` and
    /// include the `ex`, `is`, and `hm` parameters. This checks the format
    /// without contacting Discord or checking expiration.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDiscordFileUrl`] if the URL has the wrong format,
    /// lacks required details, or repeats a required parameter. The error does
    /// not contain the URL.
    pub fn parse(url: &str) -> Result<Self> {
        let attachment = url
            .strip_prefix("https://cdn.discordapp.com/attachments/")
            .ok_or(DiscordFileUrlError::InvalidPrefix)?;
        let (path, query) = attachment
            .split_once('?')
            .ok_or(DiscordFileUrlError::MissingField { field: "query" })?;
        let mut segments = path.split('/');
        let channel_id = parse_number(segments.next(), "channel_id", 10)?;
        let attachment_id = parse_number(segments.next(), "attachment_id", 10)?;
        let attachment_name = required(segments.next(), "attachment_name")?;
        if segments.next().is_some()
            || attachment_name.contains('#')
            || attachment_name.chars().any(char::is_whitespace)
            || attachment_name.chars().any(char::is_control)
        {
            return Err(DiscordFileUrlError::InvalidField {
                field: "attachment_name",
            }
            .into());
        }
        if query.contains('#') {
            return Err(DiscordFileUrlError::InvalidField { field: "query" }.into());
        }

        let mut ex = None;
        let mut is = None;
        let mut hm = None;
        for parameter in query.split('&').filter(|parameter| !parameter.is_empty()) {
            let (name, value) = parameter.split_once('=').unwrap_or((parameter, ""));
            let (field, target) = match name {
                "ex" => ("ex", &mut ex),
                "is" => ("is", &mut is),
                "hm" => ("hm", &mut hm),
                _ => continue,
            };
            if target.replace(value).is_some() {
                return Err(DiscordFileUrlError::DuplicateParameter { field }.into());
            }
        }
        let ex = parse_number(ex, "ex", 16)?;
        let is = parse_number(is, "is", 16)?;
        let hm = required(hm, "hm")?;
        if !hm.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DiscordFileUrlError::InvalidField { field: "hm" }.into());
        }

        Ok(Self {
            channel_id,
            attachment_id,
            attachment_name: attachment_name.to_owned(),
            ex,
            is,
            hm: hm.to_owned(),
        })
    }
}

fn required<'a>(value: Option<&'a str>, field: &'static str) -> Result<&'a str> {
    match value {
        None => Err(DiscordFileUrlError::MissingField { field }.into()),
        Some("") => Err(DiscordFileUrlError::InvalidField { field }.into()),
        Some(value) => Ok(value),
    }
}

fn parse_number(value: Option<&str>, field: &'static str, radix: u32) -> Result<u64> {
    let value = required(value, field)?;
    let valid = value.bytes().all(|byte| match radix {
        10 => byte.is_ascii_digit(),
        _ => byte.is_ascii_hexdigit(),
    });
    if !valid {
        return Err(DiscordFileUrlError::InvalidField { field }.into());
    }
    u64::from_str_radix(value, radix)
        .map_err(|_| DiscordFileUrlError::InvalidField { field }.into())
}

impl TryFrom<&str> for DiscordFileUrl {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl AsRef<DiscordFileUrl> for DiscordFileUrl {
    fn as_ref(&self) -> &DiscordFileUrl {
        self
    }
}

impl std::fmt::Display for DiscordFileUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "https://cdn.discordapp.com/attachments/{}/{}/{}?ex={:x}&is={:x}&hm={}",
            self.channel_id, self.attachment_id, self.attachment_name, self.ex, self.is, self.hm
        )
    }
}

impl std::fmt::Display for DiscordFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "discord://{}/{}", self.id, self.url)
    }
}
