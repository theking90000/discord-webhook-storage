use crate::{DiscordFileUrlError, Error, Result};

/// A message identifier and its parsed attachment URL.
///
/// Serialize this value with Serde to persist the message and attachment fields.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiscordFile {
    /// Identifier of the message containing the attachment.
    pub id: String,
    /// Parsed attachment URL.
    pub url: DiscordFileUrl,
}

impl DiscordFile {
    /// Parse `discord://<id>/<url>`, preserving the complete attachment URL.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDiscordFileUrl`] for an invalid prefix, an empty
    /// identifier, reserved or whitespace characters in the identifier, or an
    /// invalid attachment URL. Errors do not retain the input.
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

impl TryFrom<&str> for DiscordFile {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

/// Owned fields parsed from a signed Discord CDN attachment URL.
///
/// Formatting reconstructs the URL with query parameters ordered as `ex`, `is`,
/// and `hm`. Additional query parameters are ignored during parsing.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiscordFileUrl {
    /// Identifier of the channel containing the attachment.
    pub channel_id: u64,
    /// Identifier of the attachment.
    pub attachment_id: u64,
    /// Attachment name, preserving its URL percent encoding.
    pub attachment_name: String,
    /// Hexadecimal `ex` query parameter parsed as an integer.
    pub ex: u64,
    /// Hexadecimal `is` query parameter parsed as an integer.
    pub is: u64,
    /// Hexadecimal `hm` query parameter, preserving its original spelling.
    pub hm: String,
}

impl DiscordFileUrl {
    /// Parse `https://cdn.discordapp.com/attachments/<channel>/<attachment>/<name>`
    /// followed by the required `ex`, `is`, and `hm` query parameters.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDiscordFileUrl`] for an invalid prefix, missing or
    /// invalid fields, or repeated signing parameters. Errors do not retain the URL.
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
