use crate::{Error, Result};

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
    /// prefix or the separator between the identifier and token.
    pub fn parse(url: &'a str) -> Result<WebhookCredentials<'a>> {
        if !url.starts_with("https://discord.com/api/webhooks/") {
            return Err(Error::InvalidWebhookUrl);
        }

        let id = match &url[33..].find('/') {
            Some(i) => *i,
            None => return Err(Error::InvalidWebhookUrl),
        };

        if id + 1 == url.len() {
            // no token, ending with '/'
            return Err(Error::InvalidWebhookUrl);
        }

        Ok(WebhookCredentials {
            id: &url[33..(33 + id)],
            token: &url[id + 33 + 1..],
        })
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
    use crate::{Error::InvalidWebhookUrl, WebhookCredentials};

    #[test]
    fn test_credentials_parsing() {
        let test = "https://discord.com/api/webhooks/webhookid/supertoken";

        let result = WebhookCredentials::parse(test);

        assert_eq!(
            result,
            Ok(WebhookCredentials {
                id: "webhookid",
                token: "supertoken"
            })
        )
    }

    #[test]
    fn test_invalid_url() {
        assert_eq!(WebhookCredentials::parse(""), Err(InvalidWebhookUrl));
        assert_eq!(WebhookCredentials::parse("tokenid"), Err(InvalidWebhookUrl));
        assert_eq!(
            WebhookCredentials::parse("https://discord.com/api/"),
            Err(InvalidWebhookUrl)
        );
        assert_eq!(
            WebhookCredentials::parse("https://discord.com/api/webhookid"),
            Err(InvalidWebhookUrl)
        );
        assert_eq!(
            WebhookCredentials::parse("https://discord.com/api/webhookid/"),
            Err(InvalidWebhookUrl)
        );
    }
}
