use crate::{Error, Result, WebhookUrlError};

/// A webhook identifier and token borrowed from a Discord webhook URL.
///
/// Parsing keeps references to the URL without allocating. Keep the source
/// string alive for as long as the credentials are needed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookCredentials<'a> {
    pub(crate) id: &'a str,
    pub(crate) token: &'a str,
}

impl<'a> WebhookCredentials<'a> {
    /// Extract credentials from `https://discord.com/api/webhooks/<id>/<token>`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidWebhookUrl`] when the URL lacks the expected
    /// prefix or the separator between the identifier and token, or either
    /// credential is empty. The error never retains the URL or secret token.
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
            assert!(matches!(WebhookCredentials::parse(url), Err(Error::InvalidWebhookUrl(_))));
        }
    }
}
