use std::error::Error as _;

use discord_webhook_storage::{DiscordFile, DiscordFileUrl, DiscordFileUrlError, Error};

const URL: &str = "https://cdn.discordapp.com/attachments/123/456/file%20name.bin?ex=6abe3fd5&is=6abcee55&hm=aabbcc";

#[test]
fn parses_owned_attachment_fields_and_rebuilds_url() {
    let source = URL.to_owned();
    let parsed = DiscordFileUrl::try_from(source.as_str()).unwrap();
    drop(source);

    assert_eq!(parsed.channel_id, 123);
    assert_eq!(parsed.attachment_id, 456);
    assert_eq!(parsed.attachment_name, "file%20name.bin");
    assert_eq!(parsed.ex, 0x6abe3fd5);
    assert_eq!(parsed.is, 0x6abcee55);
    assert_eq!(parsed.hm, "aabbcc");
    assert_eq!(parsed.to_string(), URL);
}

#[test]
fn accepts_reordered_parameters_and_trailing_separator() {
    let url = "https://cdn.discordapp.com/attachments/123/456/file%20name.bin?hm=aabbcc&is=6abcee55&other=ignored&ex=6abe3fd5&";
    assert_eq!(
        DiscordFileUrl::parse(url).unwrap(),
        DiscordFileUrl::parse(URL).unwrap()
    );
}

#[test]
fn serializes_structured_fields_and_preserves_discord_file_display() {
    let file = DiscordFile {
        id: "message-1".to_owned(),
        url: DiscordFileUrl::parse(URL).unwrap(),
    };
    let json = serde_json::to_value(&file).unwrap();
    assert_eq!(json["id"], "message-1");
    assert_eq!(json["url"]["channel_id"], 123);
    assert_eq!(json["url"]["attachment_name"], "file%20name.bin");
    assert_eq!(json["url"]["ex"], 0x6abe3fd5_u64);
    assert_eq!(serde_json::from_value::<DiscordFile>(json).unwrap(), file);
    assert_eq!(file.to_string(), format!("discord://message-1/{URL}"));
}

#[test]
fn rejects_invalid_attachment_urls_with_field_errors() {
    use DiscordFileUrlError::{DuplicateParameter, InvalidField, InvalidPrefix, MissingField};

    let prefix = "https://cdn.discordapp.com/attachments/";
    for (suffix, expected) in [
        ("", MissingField { field: "query" }),
        (
            "123?ex=1&is=2&hm=aa",
            MissingField {
                field: "attachment_id",
            },
        ),
        (
            "123/456?ex=1&is=2&hm=aa",
            MissingField {
                field: "attachment_name",
            },
        ),
        (
            "/456/f?ex=1&is=2&hm=aa",
            InvalidField {
                field: "channel_id",
            },
        ),
        (
            "123/no/f?ex=1&is=2&hm=aa",
            InvalidField {
                field: "attachment_id",
            },
        ),
        (
            "18446744073709551616/456/f?ex=1&is=2&hm=aa",
            InvalidField {
                field: "channel_id",
            },
        ),
        (
            "123/456/f/extra?ex=1&is=2&hm=aa",
            InvalidField {
                field: "attachment_name",
            },
        ),
        (
            "123/456/f name?ex=1&is=2&hm=aa",
            InvalidField {
                field: "attachment_name",
            },
        ),
        ("123/456/f?is=2&hm=aa", MissingField { field: "ex" }),
        ("123/456/f?ex=1&hm=aa", MissingField { field: "is" }),
        ("123/456/f?ex=1&is=2", MissingField { field: "hm" }),
        ("123/456/f?ex=&is=2&hm=aa", InvalidField { field: "ex" }),
        ("123/456/f?ex=xyz&is=2&hm=aa", InvalidField { field: "ex" }),
        (
            "123/456/f?ex=10000000000000000&is=2&hm=aa",
            InvalidField { field: "ex" },
        ),
        ("123/456/f?ex=1&is=+2&hm=aa", InvalidField { field: "is" }),
        ("123/456/f?ex=1&is=2&hm=zz", InvalidField { field: "hm" }),
        ("123/456/f?ex=1&is=2&hm=", InvalidField { field: "hm" }),
        (
            "123/456/f?ex=1&ex=2&is=2&hm=aa",
            DuplicateParameter { field: "ex" },
        ),
        (
            "123/456/f?ex=1&is=2&is=3&hm=aa",
            DuplicateParameter { field: "is" },
        ),
        (
            "123/456/f?ex=1&is=2&hm=aa&hm=bb",
            DuplicateParameter { field: "hm" },
        ),
        (
            "123/456/f?ex=1&is=2&hm=aa#fragment",
            InvalidField { field: "query" },
        ),
    ] {
        let url = format!("{prefix}{suffix}");
        let error = DiscordFileUrl::parse(&url).unwrap_err();
        assert!(matches!(error, Error::InvalidDiscordFileUrl(actual) if actual == expected));
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<DiscordFileUrlError>(),
            Some(&expected)
        );
    }

    for url in [
        "",
        "https://example.com/attachments/123/456/f",
        "http://cdn.discordapp.com/attachments/123/456/f",
    ] {
        assert!(matches!(
            DiscordFileUrl::parse(url),
            Err(Error::InvalidDiscordFileUrl(InvalidPrefix))
        ));
    }
}
