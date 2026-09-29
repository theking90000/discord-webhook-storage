# Changelog

## 0.1.0

- Stream a file to a Discord webhook through a caller-supplied async transport.
- Enforce a default file payload limit of 20,000,000 bytes.
- Read length-delimited or chunked HTTP responses and return serializable upload metadata.
- Provide a TLS upload example using `tokio-tcp-pool`.
