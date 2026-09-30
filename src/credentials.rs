use crate::{Error, Result, WebhookUrlError};

/// Credentials that select the Discord webhook used to store files.
///
/// Create them from a webhook URL with [`Self::parse`]. Keep that URL string
/// alive for as long as the credentials are needed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookCredentials<'a> {
    pub(crate) id: &'a str,
    pub(crate) token: &'a str,
}

impl<'a> WebhookCredentials<'a> {
    /// Create upload credentials from a Discord webhook URL.
    ///
    /// The expected format is `https://discord.com/api/webhooks/<id>/<token>`.
    /// This checks the URL format without contacting Discord.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidWebhookUrl`] if the URL has the wrong format or
    /// lacks an identifier or token. The error does not contain the secret token.
    pub fn parse(url: &'a str) -> Result<WebhookCredentials<'a>> {
        let credentials = url
            .strip_prefix("https://discord.com/api/webhooks/")
            .ok_or(WebhookUrlError::InvalidPrefix)?;
        let (id, token) = credentials
            .split_once('/')
            .ok_or(WebhookUrlError::MissingTokenSeparator)?;
        if id.is_empty() {
            return Err(WebhookUrlError::EmptyId.into());
        }
        if token.is_empty() {
            return Err(WebhookUrlError::EmptyToken.into());
        }

        Ok(WebhookCredentials { id, token })
    }
}

impl<'a> TryFrom<&'a str> for WebhookCredentials<'a> {
    type Error = Error;
    fn try_from(value: &'a str) -> Result<Self> {
        Self::parse(value)
    }
}

#[cfg(test)]
mod test {
    use crate::{Error, WebhookCredentials};

    #[test]
    fn test_credentials_parsing() {
        let test = "https://discord.com/api/webhooks/webhookid/supertoken";

        let result = WebhookCredentials::parse(test);

        assert_eq!(
            result.unwrap(),
            WebhookCredentials {
                id: "webhookid",
                token: "supertoken"
            }
        )
    }

    #[test]
    fn test_invalid_url() {
        for url in [
            "",
            "tokenid",
            "https://discord.com/api/",
            "https://discord.com/api/webhookid",
            "https://discord.com/api/webhookid/",
        ] {
            assert!(matches!(
                WebhookCredentials::parse(url),
                Err(Error::InvalidWebhookUrl(_))
            ));
        }
    }
}
