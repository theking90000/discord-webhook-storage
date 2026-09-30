//! Check that public errors preserve causes instead of reducing them to labels.

use std::{error::Error as _, io};

use discord_webhook_storage::Error;

#[test]
fn io_conversion_keeps_os_code_kind_and_source() {
    let original = io::Error::from_raw_os_error(2);
    let expected_kind = original.kind();
    let error = Error::from(original);
    let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
    assert_eq!(source.raw_os_error(), Some(2));
    assert_eq!(source.kind(), expected_kind);
    assert!(matches!(error, Error::IoError(original)
        if original.raw_os_error() == Some(2) && original.kind() == expected_kind));
}

#[test]
fn io_conversion_keeps_custom_error_details() {
    let error = Error::from(io::Error::new(io::ErrorKind::ConnectionReset, "peer reset during upload"));
    assert_eq!(error.source().unwrap().to_string(), "peer reset during upload");
    assert!(error.to_string().contains("peer reset during upload"));
}

#[test]
fn json_conversion_keeps_location_and_category() {
    let original = serde_json::from_str::<serde_json::Value>("{\n  invalid\n}").unwrap_err();
    let line = original.line();
    let column = original.column();
    let category = original.classify();
    let error = Error::from(original);
    let source = error.source().unwrap().downcast_ref::<serde_json::Error>().unwrap();
    assert_eq!(source.line(), line);
    assert_eq!(source.column(), column);
    assert_eq!(source.classify(), category);
}
