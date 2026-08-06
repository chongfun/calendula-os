//! The logical-book HTTP contract (image-rendering PRD, Part 2): parsing
//! for the request side, JSON formatting for the response side.
//!
//! Pure and bounded, like `captive` and `upload`, so host `cargo test`
//! covers every accept/reject path; the firmware's Wi-Fi task supplies raw
//! header/body bytes and sends back whatever these writers produce. No
//! JSON library: the accepted grammar is exactly the PRD's two request
//! bodies and nothing more, and the writers emit from typed values, so a
//! hand-rolled scanner keeps the whole contract inspectable and the
//! firmware image small.
//!
//! Nothing here validates *authority* — tokens, epochs, and idempotency
//! belong to `source-store` on the storage-owner side. This layer only
//! decides whether bytes are well-formed enough to name an operation.

use heapless::Vec;

/// Wire lengths, all lowercase hexadecimal.
pub const REQUEST_ID_HEX: usize = 48;
pub const BOOK_TOKEN_HEX: usize = 32;
pub const SHA256_HEX: usize = 64;

/// A parsed epoch-scoped request ID: 16 hex chars of device epoch, then a
/// 32-hex-char client nonce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestId {
    pub epoch: u64,
    pub nonce: [u8; 16],
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        // Lowercase only, per the PRD's wire format: accepting uppercase
        // would create two spellings of one request ID, and the ID is an
        // identity key.
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Exactly `N` bytes from exactly `2N` lowercase hex characters.
pub fn parse_hex<const N: usize>(hex: &[u8]) -> Option<[u8; N]> {
    if hex.len() != 2 * N {
        return None;
    }
    let mut out = [0u8; N];
    for (i, pair) in hex.chunks_exact(2).enumerate() {
        out[i] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(out)
}

/// Parse the PRD's 48-character request ID.
pub fn parse_request_id(hex: &[u8]) -> Option<RequestId> {
    if hex.len() != REQUEST_ID_HEX {
        return None;
    }
    let epoch_bytes: [u8; 8] = parse_hex(&hex[..16])?;
    let nonce: [u8; 16] = parse_hex(&hex[16..])?;
    Some(RequestId {
        epoch: u64::from_be_bytes(epoch_bytes),
        nonce,
    })
}

pub fn parse_book_token(hex: &[u8]) -> Option<[u8; 16]> {
    parse_hex::<16>(hex)
}

pub fn parse_sha256(hex: &[u8]) -> Option<[u8; 32]> {
    parse_hex::<32>(hex)
}

/// The value of one header in a raw request head, name compared
/// case-insensitively (HTTP header names are), value trimmed of optional
/// whitespace. `head` is everything up to the blank line, request line
/// included; the request line is skipped outright — a path containing a
/// colon must never read as a header.
pub fn header_value<'a>(head: &'a [u8], name: &str) -> Option<&'a [u8]> {
    for line in head.split(|&byte| byte == b'\n').skip(1) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let mut split = line.splitn(2, |&byte| byte == b':');
        let (Some(key), Some(value)) = (split.next(), split.next()) else {
            continue;
        };
        if key.len() == name.len()
            && key
                .iter()
                .zip(name.as_bytes())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            let start = value.iter().position(|&byte| byte != b' ' && byte != b'\t');
            let end = value
                .iter()
                .rposition(|&byte| byte != b' ' && byte != b'\t');
            return match (start, end) {
                (Some(start), Some(end)) => Some(&value[start..=end]),
                _ => Some(&value[..0]),
            };
        }
    }
    None
}

/// The raw (undecoded) value of `key=` in a path's query string. Enough
/// for hex-valued parameters like `replace=`; percent-decoded parameters
/// (the display label) keep using [`crate::upload::raw_query_name`].
pub fn query_param<'a>(path: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let query_at = path.iter().position(|&byte| byte == b'?')? + 1;
    for pair in path[query_at..].split(|&byte| byte == b'&') {
        let mut split = pair.splitn(2, |&byte| byte == b'=');
        if split.next() == Some(key) {
            return split.next();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// JSON request bodies
// ---------------------------------------------------------------------------

/// One field value in a flat JSON object: a raw string body (escapes
/// intact) or an unsigned integer. The only two value kinds the PRD's
/// request bodies use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlatValue<'a> {
    Str(&'a [u8]),
    UInt(u64),
}

/// Walk `body` as exactly one flat JSON object — `{ "key": value, ... }`
/// with string or unsigned-integer values, JSON whitespace, and nothing
/// but whitespace after the closing brace — calling `field` for each
/// pair. Returns `false` for any structural violation or when `field`
/// does.
///
/// These bodies authorize mutations, so the envelope is validated
/// whole rather than key-hunted: a deletion must never execute because
/// token-shaped bytes happened to appear in something that is not the
/// documented request. Unknown *keys* are still passed through to
/// `field` (which tolerates them), so adding an optional field later
/// does not break older firmware; garbage instead of an object always
/// rejects.
fn for_each_flat_field(body: &[u8], mut field: impl FnMut(&[u8], FlatValue<'_>) -> bool) -> bool {
    let mut at = skip_ws(body, 0);
    if body.get(at) != Some(&b'{') {
        return false;
    }
    at = skip_ws(body, at + 1);
    if body.get(at) == Some(&b'}') {
        return skip_ws(body, at + 1) == body.len();
    }
    loop {
        // Key: a string, then a colon.
        if body.get(at) != Some(&b'"') {
            return false;
        }
        let Some(key_len) = raw_string_len(&body[at + 1..]) else {
            return false;
        };
        let key = &body[at + 1..at + 1 + key_len];
        at = skip_ws(body, at + 1 + key_len + 1);
        if body.get(at) != Some(&b':') {
            return false;
        }
        at = skip_ws(body, at + 1);
        // Value: a string or an unsigned integer, nothing else.
        let value = match body.get(at) {
            Some(b'"') => {
                let Some(value_len) = raw_string_len(&body[at + 1..]) else {
                    return false;
                };
                let value = FlatValue::Str(&body[at + 1..at + 1 + value_len]);
                at += 1 + value_len + 1;
                value
            }
            Some(byte) if byte.is_ascii_digit() => {
                let digits_end = at
                    + body[at..]
                        .iter()
                        .position(|byte| !byte.is_ascii_digit())
                        .unwrap_or(body.len() - at);
                let Some(value) = parse_u64_digits(&body[at..digits_end]) else {
                    return false;
                };
                at = digits_end;
                FlatValue::UInt(value)
            }
            _ => return false,
        };
        if !field(key, value) {
            return false;
        }
        at = skip_ws(body, at);
        match body.get(at) {
            Some(b',') => at = skip_ws(body, at + 1),
            Some(b'}') => return skip_ws(body, at + 1) == body.len(),
            _ => return false,
        }
    }
}

fn skip_ws(bytes: &[u8], mut at: usize) -> usize {
    while matches!(bytes.get(at), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        at += 1;
    }
    at
}

/// Plain decimal digits into a `u64`, overflow-checked. The PRD's lengths
/// are exact byte counts — never signed, fractional, or exponent-form.
fn parse_u64_digits(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || digits.len() > 20 {
        return None;
    }
    let mut value: u64 = 0;
    for &byte in digits {
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(value)
}

/// Length of a JSON string's raw contents starting just past its opening
/// quote — the index of the closing quote, escape-aware.
fn raw_string_len(bytes: &[u8]) -> Option<usize> {
    let mut at = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b'"' => return Some(at),
            b'\\' => at += 2,
            _ => at += 1,
        }
    }
    None
}

/// Resolve a raw string body's escapes into `out`. Only `\"`, `\\`, and
/// `\/` are accepted: every other escape either encodes a control
/// character (invalid in a display label anyway) or is `\uXXXX`, which
/// protocol v1 has no need to accept — a browser sending labels as UTF-8
/// text never needs it.
fn unescape_into(raw: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut written = 0usize;
    let mut at = 0usize;
    while at < raw.len() {
        let byte = match raw[at] {
            b'\\' => {
                at += 1;
                match raw.get(at)? {
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'/' => b'/',
                    _ => return None,
                }
            }
            byte => byte,
        };
        *out.get_mut(written)? = byte;
        written += 1;
        at += 1;
    }
    Some(written)
}

/// A required-once field slot: setting it twice is a duplicate, which
/// rejects — two values under one key means the request contradicts
/// itself, and this layer refuses rather than letting order decide.
fn set_once<T>(slot: &mut Option<T>, value: Option<T>) -> bool {
    match value {
        Some(value) if slot.is_none() => {
            *slot = Some(value);
            true
        }
        _ => false,
    }
}

/// A parsed `POST /delete-book` body: `{"book_token": "<32 hex>"}`.
pub fn parse_delete_body(body: &[u8]) -> Option<[u8; 16]> {
    let mut token: Option<[u8; 16]> = None;
    let well_formed = for_each_flat_field(body, |key, value| match (key, value) {
        (b"book_token", FlatValue::Str(raw)) => {
            let mut hex = [0u8; BOOK_TOKEN_HEX];
            let parsed = unescape_into(raw, &mut hex).and_then(|len| parse_book_token(&hex[..len]));
            set_once(&mut token, parsed)
        }
        (b"book_token", _) => false,
        // Unknown keys of either value kind are tolerated for forward
        // compatibility; their values were already structurally checked.
        _ => true,
    });
    if !well_formed {
        return None;
    }
    token
}

/// A parsed `POST /recover-book` body. The label is returned as raw bytes
/// for the storage layer's own validation — this layer only bounds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoverBody {
    pub book_token: [u8; 16],
    pub observed_length: u64,
    pub observed_sha256: [u8; 32],
    pub display_label: Option<Vec<u8, 64>>,
}

pub fn parse_recover_body(body: &[u8]) -> Option<RecoverBody> {
    let mut token: Option<[u8; 16]> = None;
    let mut length: Option<u64> = None;
    let mut sha256: Option<[u8; 32]> = None;
    let mut label: Option<Vec<u8, 64>> = None;
    let well_formed = for_each_flat_field(body, |key, value| match (key, value) {
        (b"book_token", FlatValue::Str(raw)) => {
            let mut hex = [0u8; BOOK_TOKEN_HEX];
            let parsed = unescape_into(raw, &mut hex).and_then(|len| parse_book_token(&hex[..len]));
            set_once(&mut token, parsed)
        }
        (b"observed_source_length", FlatValue::UInt(value)) => set_once(&mut length, Some(value)),
        (b"observed_source_sha256", FlatValue::Str(raw)) => {
            let mut hex = [0u8; SHA256_HEX];
            let parsed = unescape_into(raw, &mut hex).and_then(|len| parse_sha256(&hex[..len]));
            set_once(&mut sha256, parsed)
        }
        (b"display_label", FlatValue::Str(raw)) => {
            let mut bytes = [0u8; 64];
            let parsed = unescape_into(raw, &mut bytes).and_then(|len| {
                let mut owned = Vec::new();
                owned.extend_from_slice(&bytes[..len]).ok()?;
                Some(owned)
            });
            set_once(&mut label, parsed)
        }
        (
            b"book_token"
            | b"observed_source_length"
            | b"observed_source_sha256"
            | b"display_label",
            _,
        ) => false,
        _ => true,
    });
    if !well_formed {
        return None;
    }
    Some(RecoverBody {
        book_token: token?,
        observed_length: length?,
        observed_sha256: sha256?,
        display_label: label,
    })
}

// ---------------------------------------------------------------------------
// JSON responses
// ---------------------------------------------------------------------------

/// Bounded JSON assembly into a caller buffer. Every push returns `false`
/// on overflow and poisons the writer, so one check at [`finish`]
/// suffices; a partially written response is never sent.
///
/// [`finish`]: JsonOut::finish
pub struct JsonOut<'a> {
    out: &'a mut [u8],
    at: usize,
    overflowed: bool,
}

impl<'a> JsonOut<'a> {
    pub fn new(out: &'a mut [u8]) -> Self {
        Self {
            out,
            at: 0,
            overflowed: false,
        }
    }

    pub fn raw(&mut self, text: &str) -> &mut Self {
        self.bytes(text.as_bytes());
        self
    }

    fn bytes(&mut self, bytes: &[u8]) {
        if self.overflowed || self.at + bytes.len() > self.out.len() {
            self.overflowed = true;
            return;
        }
        self.out[self.at..self.at + bytes.len()].copy_from_slice(bytes);
        self.at += bytes.len();
    }

    /// A JSON string from raw text, escaping `"` and `\`. Control bytes
    /// (which validated labels cannot contain) are replaced with `?`
    /// rather than emitted, so the output is always valid JSON.
    pub fn string(&mut self, text: &[u8]) -> &mut Self {
        self.bytes(b"\"");
        for &byte in text {
            match byte {
                b'"' => self.bytes(b"\\\""),
                b'\\' => self.bytes(b"\\\\"),
                0x00..=0x1F => self.bytes(b"?"),
                byte => self.bytes(core::slice::from_ref(&byte)),
            }
        }
        self.bytes(b"\"");
        self
    }

    pub fn hex(&mut self, bytes: &[u8]) -> &mut Self {
        self.bytes(b"\"");
        for &byte in bytes {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            self.bytes(&[HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 0xF)]]);
        }
        self.bytes(b"\"");
        self
    }

    /// The 48-hex-character wire form of an epoch-scoped request ID.
    pub fn request_id(&mut self, epoch: u64, nonce: &[u8; 16]) -> &mut Self {
        self.bytes(b"\"");
        for &byte in epoch.to_be_bytes().iter().chain(nonce.iter()) {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            self.bytes(&[HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 0xF)]]);
        }
        self.bytes(b"\"");
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        let mut value = value;
        loop {
            at -= 1;
            digits[at] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.bytes(&digits[at..]);
        self
    }

    pub fn bool(&mut self, value: bool) -> &mut Self {
        self.raw(if value { "true" } else { "false" })
    }

    /// The assembled length, or `None` if anything overflowed.
    pub fn finish(self) -> Option<usize> {
        if self.overflowed {
            None
        } else {
            Some(self.at)
        }
    }
}

/// The PRD's successful-upload (and recovery) response payload. The
/// render-bundle fields are honest v1 stubs: no bundle schema is accepted
/// until M0R lands, and saying so beats omitting them and springing a
/// schema change on clients later.
#[derive(Clone, Copy, Debug)]
pub struct OperationSuccess<'a> {
    pub logical_book_id: &'a [u8; 16],
    pub book_token: &'a [u8; 16],
    pub request_epoch: u64,
    pub request_nonce: &'a [u8; 16],
    pub source_length: u64,
    pub source_sha256: &'a [u8; 32],
    pub source_generation: u64,
    pub display_label: &'a [u8],
    pub board_profile: &'a str,
}

pub fn write_operation_success(out: &mut [u8], reply: &OperationSuccess<'_>) -> Option<usize> {
    let mut json = JsonOut::new(out);
    json.raw("{\"status\":\"ok\",\"logical_book_id\":")
        .hex(reply.logical_book_id)
        .raw(",\"book_token\":")
        .hex(reply.book_token)
        .raw(",\"operation_request_id\":")
        .request_id(reply.request_epoch, reply.request_nonce)
        .raw(",\"source_length\":")
        .u64(reply.source_length)
        .raw(",\"source_sha256\":")
        .hex(reply.source_sha256)
        .raw(",\"source_generation\":")
        .u64(reply.source_generation)
        .raw(",\"display_label\":")
        .string(reply.display_label)
        .raw(",\"active_board_profile\":\"")
        .raw(reply.board_profile)
        .raw("\",\"capabilities_version\":1")
        .raw(",\"accepted_render_bundle_schema\":null")
        .raw(",\"render_bundle_upload_enabled\":false}");
    json.finish()
}

/// A machine-readable refusal: a stable code plus a retry classification,
/// never a filesystem path or a prose-only answer.
pub fn write_error(out: &mut [u8], code: &str, retryable: bool) -> Option<usize> {
    let mut json = JsonOut::new(out);
    json.raw("{\"status\":\"error\",\"code\":\"")
        .raw(code)
        .raw("\",\"retryable\":")
        .bool(retryable)
        .raw("}");
    json.finish()
}

/// The capabilities the M0S contract requires the device to advertise:
/// the idempotency epoch and its budgets, plus the source-container
/// limits a browser must check before uploading.
#[derive(Clone, Copy, Debug)]
pub struct SourceCapabilities<'a> {
    pub idempotency_epoch: u64,
    pub max_new_requests_this_epoch: u64,
    pub retained_previous_epoch: u64,
    pub max_epub_bytes: u64,
    pub zip64_supported: bool,
    pub board_profile: &'a str,
}

pub fn write_capabilities(out: &mut [u8], caps: &SourceCapabilities<'_>) -> Option<usize> {
    let mut json = JsonOut::new(out);
    json.raw("{\"capabilities_version\":1,\"idempotency_epoch\":")
        .u64(caps.idempotency_epoch)
        .raw(",\"max_new_requests_this_epoch\":")
        .u64(caps.max_new_requests_this_epoch)
        .raw(",\"retained_previous_epoch_requests\":")
        .u64(caps.retained_previous_epoch)
        .raw(",\"max_epub_bytes\":")
        .u64(caps.max_epub_bytes)
        .raw(",\"zip64_supported\":")
        .bool(caps.zip64_supported)
        .raw(",\"active_board_profile\":\"")
        .raw(caps.board_profile)
        .raw("\"}");
    json.finish()
}

/// One listed book, in wire-shape. Mirrors the storage layer's list entry
/// without depending on it — `proto` sits below `source-store`, so the
/// firmware maps between the two, and this struct is the response schema
/// clients actually see.
#[derive(Clone, Copy, Debug)]
pub struct ListEntryWire<'a> {
    pub display_label: &'a [u8],
    pub logical_book_id: &'a [u8; 16],
    pub book_token: &'a [u8; 16],
    pub source_generation: u64,
    /// `"managed"` or `"unmanaged"`.
    pub source_origin: &'a str,
    pub externally_recovered: bool,
    /// One of the PRD's five integrity states, snake_cased.
    pub source_integrity_status: &'a str,
    pub source_length: u64,
    pub observed_source_length: Option<u64>,
    pub observed_source_sha256: Option<&'a [u8; 32]>,
    pub may_replace: bool,
    pub may_delete: bool,
    pub may_recover_current_bytes: bool,
}

/// One list entry as a standalone JSON object. The caller frames the
/// stream (array brackets and commas), because the entry count is its
/// knowledge, not this writer's.
pub fn write_list_entry(out: &mut [u8], entry: &ListEntryWire<'_>) -> Option<usize> {
    let mut json = JsonOut::new(out);
    json.raw("{\"display_label\":")
        .string(entry.display_label)
        .raw(",\"logical_book_id\":")
        .hex(entry.logical_book_id)
        .raw(",\"book_token\":")
        .hex(entry.book_token)
        .raw(",\"source_generation\":")
        .u64(entry.source_generation)
        .raw(",\"source_origin\":\"")
        .raw(entry.source_origin)
        .raw("\",\"externally_recovered\":")
        .bool(entry.externally_recovered)
        .raw(",\"source_integrity_status\":\"")
        .raw(entry.source_integrity_status)
        .raw("\",\"source_length\":")
        .u64(entry.source_length);
    match entry.observed_source_length {
        Some(length) => {
            json.raw(",\"observed_source_length\":").u64(length);
        }
        None => {
            json.raw(",\"observed_source_length\":null");
        }
    }
    match entry.observed_source_sha256 {
        Some(sha256) => {
            json.raw(",\"observed_source_sha256\":").hex(sha256);
        }
        None => {
            json.raw(",\"observed_source_sha256\":null");
        }
    }
    json.raw(",\"allowed_operations\":{\"replace\":")
        .bool(entry.may_replace)
        .raw(",\"delete\":")
        .bool(entry.may_delete)
        .raw(",\"recover_current_bytes\":")
        .bool(entry.may_recover_current_bytes)
        .raw("}}");
    json.finish()
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::string::String;
    use std::vec::Vec as StdVec;

    #[test]
    fn request_ids_parse_exactly_and_reject_look_alikes() {
        let mut hex = StdVec::new();
        hex.extend_from_slice(b"00000000000000ff");
        hex.extend_from_slice(&[b'a'; 32]);
        let parsed = parse_request_id(&hex).expect("valid id parses");
        assert_eq!(parsed.epoch, 0xFF);
        assert_eq!(parsed.nonce, [0xAA; 16]);

        assert!(parse_request_id(&hex[..47]).is_none(), "short");
        let mut upper = hex.clone();
        upper[20] = b'A';
        assert!(parse_request_id(&upper).is_none(), "uppercase");
        let mut wide = hex.clone();
        wide.push(b'a');
        assert!(parse_request_id(&wide).is_none(), "long");

        assert_eq!(parse_book_token(&[b'0'; 32]), Some([0u8; 16]));
        assert!(parse_book_token(&[b'0'; 33]).is_none());
        assert_eq!(parse_sha256(&[b'f'; 64]), Some([0xFF; 32]));
    }

    #[test]
    fn headers_match_case_insensitively_and_trim() {
        let head = b"POST /upload HTTP/1.1\r\nHost: x\r\nx-source-sha256:  abc \r\nX-Upload-Request-Id: def\r\n";
        assert_eq!(header_value(head, "X-Source-SHA256"), Some(&b"abc"[..]));
        assert_eq!(header_value(head, "x-upload-request-id"), Some(&b"def"[..]));
        assert_eq!(header_value(head, "X-Delete-Request-Id"), None);
        // The request line never answers, even when its path holds a colon.
        assert_eq!(
            header_value(b"GET /a:b HTTP/1.1\r\nx: y\r\n", "GET /a"),
            None
        );
        assert_eq!(
            header_value(b"GET / HTTP/1.1\r\nx: y\r\n", "x"),
            Some(&b"y"[..])
        );
    }

    #[test]
    fn query_params_resolve_by_exact_key() {
        let path = b"/upload?name=A%20Book&replace=00ff";
        assert_eq!(query_param(path, b"replace"), Some(&b"00ff"[..]));
        assert_eq!(query_param(path, b"name"), Some(&b"A%20Book"[..]));
        assert_eq!(query_param(path, b"replac"), None);
        assert_eq!(query_param(b"/upload", b"replace"), None);
    }

    #[test]
    fn delete_and_recover_bodies_parse_and_reject() {
        let token_hex = "00112233445566778899aabbccddeeff";
        let body = std::format!("{{\"book_token\": \"{token_hex}\"}}");
        let token = parse_delete_body(body.as_bytes()).expect("delete body parses");
        assert_eq!(token[0], 0x00);
        assert_eq!(token[15], 0xFF);

        assert!(parse_delete_body(b"{}").is_none());
        assert!(parse_delete_body(b"{\"book_token\":\"zz\"}").is_none());

        let sha_hex: String = "a".repeat(64);
        let body = std::format!(
            "{{\"book_token\":\"{token_hex}\",\"observed_source_length\":12345,\
             \"observed_source_sha256\":\"{sha_hex}\",\"display_label\":\"He said \\\"hi\\\"\"}}"
        );
        let parsed = parse_recover_body(body.as_bytes()).expect("recover body parses");
        assert_eq!(parsed.observed_length, 12345);
        assert_eq!(parsed.observed_sha256, [0xAA; 32]);
        assert_eq!(
            parsed.display_label.as_ref().map(|label| label.as_slice()),
            Some(&b"He said \"hi\""[..])
        );

        // Label omitted is fine; broken escapes and non-numeric lengths
        // are not.
        let body =
            std::format!("{{\"book_token\":\"{token_hex}\",\"observed_source_length\":1,\"observed_source_sha256\":\"{sha_hex}\"}}");
        let parsed = parse_recover_body(body.as_bytes()).expect("label optional");
        assert_eq!(parsed.display_label, None);
        let body = std::format!(
            "{{\"book_token\":\"{token_hex}\",\"observed_source_length\":-1,\"observed_source_sha256\":\"{sha_hex}\"}}"
        );
        assert!(parse_recover_body(body.as_bytes()).is_none());
        let body = std::format!(
            "{{\"book_token\":\"{token_hex}\",\"observed_source_length\":1,\"observed_source_sha256\":\"{sha_hex}\",\"display_label\":\"\\n\"}}"
        );
        assert!(parse_recover_body(body.as_bytes()).is_none());
    }

    #[test]
    fn a_key_shaped_value_string_does_not_shadow_a_key() {
        // The label's *value* is "book_token": the scanner must not read
        // it as a key and return the bytes after it.
        let body = b"{\"display_label\":\"book_token\",\"book_token\":\"00112233445566778899aabbccddeeff\"}";
        let token = parse_delete_body(body).expect("real key wins");
        assert_eq!(token[0], 0x00);
    }

    #[test]
    fn mutating_bodies_must_be_whole_well_formed_objects() {
        let token_hex = "00112233445566778899aabbccddeeff";

        // Whitespace-rich but well-formed: accepted.
        let body = std::format!(" {{\r\n  \"book_token\" : \"{token_hex}\"\n}} \r\n");
        assert!(parse_delete_body(body.as_bytes()).is_some());

        // Token-shaped bytes inside something that is not the documented
        // object must never authorize a deletion.
        let body = std::format!("garbage \"book_token\":{token_hex} trailing");
        assert!(parse_delete_body(body.as_bytes()).is_none(), "no envelope");
        let body = std::format!("\"book_token\":\"{token_hex}\"");
        assert!(parse_delete_body(body.as_bytes()).is_none(), "no braces");
        let body = std::format!("{{\"book_token\":\"{token_hex}\"}} trailing");
        assert!(
            parse_delete_body(body.as_bytes()).is_none(),
            "trailing junk"
        );
        let body = std::format!("{{\"book_token\":{token_hex}}}");
        assert!(
            parse_delete_body(body.as_bytes()).is_none(),
            "unquoted value"
        );
        let body = std::format!("{{\"book_token\":\"{token_hex}\",}}");
        assert!(
            parse_delete_body(body.as_bytes()).is_none(),
            "trailing comma"
        );
        let body = std::format!("{{\"book_token\":\"{token_hex}\"");
        assert!(parse_delete_body(body.as_bytes()).is_none(), "unterminated");
        let body =
            std::format!("{{\"book_token\":\"{token_hex}\",\"book_token\":\"{token_hex}\"}}");
        assert!(
            parse_delete_body(body.as_bytes()).is_none(),
            "duplicate key"
        );
        assert!(parse_delete_body(b"{}").is_none(), "required field absent");
        assert!(
            parse_delete_body(b"{\"book_token\":[1]}").is_none(),
            "wrong value kind"
        );

        // Unknown keys of the accepted value kinds stay tolerated, so a
        // newer client field does not break older firmware.
        let body = std::format!("{{\"future\":\"x\",\"book_token\":\"{token_hex}\",\"n\":7}}");
        assert!(parse_delete_body(body.as_bytes()).is_some());

        // The same envelope rules hold for recovery, and a duplicated
        // required field rejects rather than letting order decide.
        let sha_hex = "a".repeat(64);
        let body = std::format!(
            "{{\"book_token\":\"{token_hex}\",\"observed_source_length\":1,\
             \"observed_source_length\":2,\"observed_source_sha256\":\"{sha_hex}\"}}"
        );
        assert!(parse_recover_body(body.as_bytes()).is_none());
    }

    #[test]
    fn responses_render_the_documented_shapes() {
        let mut out = [0u8; 1024];
        let len = write_operation_success(
            &mut out,
            &OperationSuccess {
                logical_book_id: &[0x11; 16],
                book_token: &[0x22; 16],
                request_epoch: 3,
                request_nonce: &[0x44; 16],
                source_length: 1234,
                source_sha256: &[0x55; 32],
                source_generation: 2,
                display_label: b"A \"Fine\" Book",
                board_profile: "x4",
            },
        )
        .expect("success renders");
        let text = core::str::from_utf8(&out[..len]).unwrap();
        assert!(text.starts_with("{\"status\":\"ok\""));
        assert!(text.contains("\"book_token\":\"22222222222222222222222222222222\""));
        assert!(text.contains("\"operation_request_id\":\"0000000000000003"));
        assert!(text.contains("\"display_label\":\"A \\\"Fine\\\" Book\""));
        assert!(text.contains("\"render_bundle_upload_enabled\":false"));

        let len = write_error(&mut out, "stale_epoch", true).expect("error renders");
        assert_eq!(
            core::str::from_utf8(&out[..len]).unwrap(),
            "{\"status\":\"error\",\"code\":\"stale_epoch\",\"retryable\":true}"
        );

        let len = write_capabilities(
            &mut out,
            &SourceCapabilities {
                idempotency_epoch: 7,
                max_new_requests_this_epoch: 16,
                retained_previous_epoch: 16,
                max_epub_bytes: 64 * 1024 * 1024,
                zip64_supported: false,
                board_profile: "x3",
            },
        )
        .expect("capabilities render");
        let text = core::str::from_utf8(&out[..len]).unwrap();
        assert!(text.contains("\"idempotency_epoch\":7"));
        assert!(text.contains("\"zip64_supported\":false"));

        let len = write_list_entry(
            &mut out,
            &ListEntryWire {
                display_label: b"Moby",
                logical_book_id: &[1; 16],
                book_token: &[2; 16],
                source_generation: 1,
                source_origin: "managed",
                externally_recovered: false,
                source_integrity_status: "externally_modified",
                source_length: 999,
                observed_source_length: Some(1000),
                observed_source_sha256: Some(&[3; 32]),
                may_replace: false,
                may_delete: true,
                may_recover_current_bytes: true,
            },
        )
        .expect("entry renders");
        let text = core::str::from_utf8(&out[..len]).unwrap();
        assert!(text.contains("\"observed_source_length\":1000"));
        assert!(text.contains("\"recover_current_bytes\":true"));

        // Overflow is a `None`, never a truncated response.
        let mut tiny = [0u8; 8];
        assert!(write_error(&mut tiny, "code", false).is_none());
    }
}
