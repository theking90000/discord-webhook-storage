use crate::{Error::HttpParseError, Result};

 struct LineParser {
    buf: Vec<u8>,
    start: usize,
    search_pos: usize,
}

impl LineParser {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            start: 0,
            search_pos: 0,
        }
    }

    fn feed(&mut self, data: &[u8]) {
        if self.start >= 4096 && self.start >= self.buf.len() / 2 {
            let remaining = self.buf.len() - self.start;

            self.buf.copy_within(self.start.., 0);
            self.buf.truncate(remaining);

            self.search_pos -= self.start;
            self.start = 0;
        }

        self.buf.extend_from_slice(data);
    }

    fn next_line(&mut self) -> Option<&[u8]> {
        let relative = self.buf[self.search_pos..]
            .iter()
            .position(|&b| b == b'\n');

        let pos = match relative {
            Some(pos) => self.search_pos + pos,
            None => {
                self.search_pos = self.buf.len();
                return None;
            }
        };

        let start = self.start;

        self.start = pos + 1;
        self.search_pos = self.start;

        Some(&self.buf[start..pos])
    }

    fn remaining(mut self) -> Vec<u8> {
        if self.start > 0 {
            let remaining = self.buf.len() - self.start;

            self.buf.copy_within(self.start.., 0);
            self.buf.truncate(remaining);
        }

        self.buf
    }
}

pub(crate) struct HttpStatusParser {
    lines: LineParser,
}

pub(crate) struct HttpHeaderParser {
    lines: LineParser,
    content_length: Option<usize>,
    done: bool,
}

impl HttpStatusParser {
    pub(crate) fn new() -> Self {
        Self { lines: LineParser::new() }
    }

    pub(crate) fn feed(&mut self, data: &[u8]) {
        self.lines.feed(data);
    }

    pub(crate) fn status(&mut self) -> Result<Option<u16>> {
        let Some(line) = self.lines.next_line() else {
            return Ok(None);
        };

        let line = str::from_utf8(line)?;

        let rest = line
            .strip_prefix("HTTP/1.1 ")
            .ok_or(HttpParseError)?;

        let (status, _) = rest
            .split_once(' ')
            .ok_or(HttpParseError)?;

        Ok(Some(status.parse()?))
    }

    pub(crate) fn into_headers(self) -> HttpHeaderParser {
        HttpHeaderParser {
            lines: self.lines,
            content_length: None,
            done: false,
        }
    }
}

impl HttpHeaderParser {
    pub(crate) fn feed(&mut self, data: &[u8]) {
        self.lines.feed(data);
    }

    pub(crate) fn next_header(&mut self) -> Result<Option<(&str, &str)>> {
        if self.done {
            return Ok(None);
        }

        let Some(line) = self.lines.next_line() else {
            return Ok(None);
        };

        let line = std::str::from_utf8(line)?;
        let line = line.strip_suffix('\r').unwrap_or(line);

        // Empty line => end of headers
        if line.is_empty() {
            self.done = true;
            return Ok(None);
        }

        let (name, value) = line
            .split_once(':')
            .ok_or(HttpParseError)?;

        let value = value.trim_ascii();

        if name.eq_ignore_ascii_case("content-length") {
            let len = value
                .parse::<usize>()
                .map_err(|_| HttpParseError)?;

            if let Some(previous) = self.content_length {
                if previous != len {
                    return Err(HttpParseError);
                }
            }

            self.content_length = Some(len);
        }

        Ok(Some((name, value)))
    }

    pub(crate) fn body_size(&self) -> Option<usize> {
        self.content_length
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.done
    }

    pub(crate) fn remaining(self) -> Vec<u8> {
        self.lines.remaining()
    }
}