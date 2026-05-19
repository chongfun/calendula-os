pub trait FlashReader {
    type Error;
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>;
    fn seek(&mut self, pos: u32) -> Result<(), Self::Error>;
}

pub struct EpubReader<R> {
    pub reader: R,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XmlToken<'a> {
    StartTag { name: &'a str },
    EndTag { name: &'a str },
    Text { content: &'a str },
}

impl<R: FlashReader> EpubReader<R> {
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    /// Scans a ZIP archive for a file by name.
    /// Returns the offset within flash to the file's raw compressed data, and its compressed size.
    pub fn locate_file(&mut self, filename: &str) -> Result<Option<(u32, u32)>, R::Error> {
        let mut offset = 0u32;
        let mut header = [0u8; 30];

        loop {
            self.reader.seek(offset)?;
            let bytes_read = self.reader.read(&mut header)?;
            if bytes_read < 30 {
                break; // End of file or truncated
            }

            // Check local file header signature: "PK\x03\x04"
            if header[0] != b'P' || header[1] != b'K' || header[2] != 0x03 || header[3] != 0x04 {
                break; // Signature mismatch, end of local files or corrupt
            }

            let comp_size = u32::from_le_bytes([header[18], header[19], header[20], header[21]]);
            let name_len = u16::from_le_bytes([header[26], header[27]]) as usize;
            let extra_len = u16::from_le_bytes([header[28], header[29]]) as usize;

            // Read the filename
            let mut name_buf = [0u8; 64];
            let name_to_read = name_len.min(name_buf.len());
            self.reader.read(&mut name_buf[..name_to_read])?;

            if let Ok(name_str) = core::str::from_utf8(&name_buf[..name_to_read]) {
                if name_str.starts_with(filename) {
                    let data_offset = offset + 30 + name_len as u32 + extra_len as u32;
                    return Ok(Option::Some((data_offset, comp_size)));
                }
            }

            // Move to next local file header
            offset += 30 + name_len as u32 + extra_len as u32 + comp_size;
        }

        Ok(Option::None)
    }
}

/// A lightweight, completely zero-alloc XML pull-parser.
/// Operates on a string slice containing a chunk of XML.
pub struct XmlPullParser<'a> {
    pub input: &'a str,
    pub cursor: usize,
}

impl<'a> XmlPullParser<'a> {
    pub fn new(input: &'a str) -> Self {
        Self { input, cursor: 0 }
    }

    /// Decodes the next token from the input slice on the fly.
    pub fn next_token(&mut self) -> Option<XmlToken<'a>> {
        let remainder = &self.input[self.cursor..];
        if remainder.is_empty() {
            return Option::None;
        }

        if remainder.starts_with('<') {
            // Find end of tag
            if let Some(end_idx) = remainder.find('>') {
                let tag_content = &remainder[1..end_idx];
                self.cursor += end_idx + 1;

                if tag_content.starts_with('/') {
                    let name = &tag_content[1..].trim();
                    Option::Some(XmlToken::EndTag { name })
                } else {
                    // Extract tag name (splitting off any attributes)
                    let name = tag_content
                        .split_whitespace()
                        .next()
                        .unwrap_or(tag_content)
                        .trim_end_matches('/');
                    Option::Some(XmlToken::StartTag { name })
                }
            } else {
                // Malformed tag, treat rest as text
                self.cursor += remainder.len();
                Option::Some(XmlToken::Text { content: remainder })
            }
        } else {
            // Read until next tag
            let end_idx = remainder.find('<').unwrap_or(remainder.len());
            let content = remainder[..end_idx].trim();
            self.cursor += end_idx;

            if !content.is_empty() {
                Option::Some(XmlToken::Text { content })
            } else {
                // If content was just whitespace, tail recurse to next token
                self.next_token()
            }
        }
    }
}
