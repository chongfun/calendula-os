//! KOReader progress-sync (kosync) client protocol pieces.
//!
//! Sans-IO: this module computes the KOReader document digest, builds
//! HTTP/1.1 requests into caller-owned buffers, and parses responses
//! back out of them. The firmware's wifi task owns the socket; host
//! tests drive everything here with byte slices.

/// MD5 (RFC 1321). Small, allocation-free, and only used for kosync
/// document identity and the password key — not for security.
pub struct Md5 {
    state: [u32; 4],
    len_bytes: u64,
    buf: [u8; 64],
    buf_len: usize,
}

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

impl Md5 {
    pub const fn new() -> Self {
        Self {
            state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476],
            len_bytes: 0,
            buf: [0; 64],
            buf_len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len_bytes = self.len_bytes.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 16] {
        let bit_len = self.len_bytes.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0]);
        }
        self.update(&bit_len.to_le_bytes());
        let mut out = [0u8; 16];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.state.iter()) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut m = [0u32; 16];
        for (word, chunk) in m.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        let [mut a, mut b, mut c, mut d] = self.state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(MD5_K[i])
                    .wrapping_add(m[g])
                    .rotate_left(MD5_S[i]),
            );
            a = tmp;
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
    }
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut md5 = Md5::new();
    md5.update(data);
    md5.finalize()
}

/// KOReader's partial digest for binary document matching: 1 KB samples
/// at offset 0 and then 1024 << (2*i) for i in 0..=10, stopping at the
/// first empty read. (The 0 offset is LuaJIT's `lshift(1024, -2)`
/// wrapping to zero.) `read_at` returns the number of bytes read.
pub fn partial_md5(read_at: &mut dyn FnMut(u32, &mut [u8]) -> usize) -> [u8; 16] {
    let mut md5 = Md5::new();
    let mut buf = [0u8; 1024];
    for i in -1i32..=10 {
        let offset = if i < 0 {
            0u64
        } else {
            1024u64 << (2 * i as u32)
        };
        let Ok(offset) = u32::try_from(offset) else {
            break;
        };
        let read = read_at(offset, &mut buf);
        if read == 0 {
            break;
        }
        md5.update(&buf[..read]);
    }
    md5.finalize()
}

pub fn hex_digest(digest: [u8; 16]) -> [u8; 32] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 32];
    for (index, byte) in digest.iter().enumerate() {
        out[index * 2] = HEX[(byte >> 4) as usize];
        out[index * 2 + 1] = HEX[(byte & 0x0f) as usize];
    }
    out
}

/// Everything needed to talk to one kosync server. `key_hex` is the
/// lowercase MD5 of the account password, which is what the protocol
/// sends as `x-auth-key`.
pub struct Account<'a> {
    pub host: &'a str,
    pub port: u16,
    pub username: &'a str,
    pub key_hex: &'a [u8; 32],
}

struct RequestWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
    overflow: bool,
}

impl<'a> RequestWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            len: 0,
            overflow: false,
        }
    }

    fn push(&mut self, text: &str) {
        self.push_bytes(text.as_bytes());
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        if self.len + bytes.len() > self.buf.len() {
            self.overflow = true;
            return;
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    fn push_usize(&mut self, value: usize) {
        let mut digits = [0u8; 20];
        let mut len = 0;
        let mut value = value;
        if value == 0 {
            digits[0] = b'0';
            len = 1;
        }
        while value > 0 {
            digits[len] = b'0' + (value % 10) as u8;
            value /= 10;
            len += 1;
        }
        while len > 0 {
            len -= 1;
            self.push_bytes(&[digits[len]]);
        }
    }

    fn finish(self) -> Option<usize> {
        if self.overflow {
            None
        } else {
            Some(self.len)
        }
    }
}

fn push_common_headers(writer: &mut RequestWriter<'_>, account: &Account<'_>) {
    writer.push("Host: ");
    writer.push(account.host);
    if account.port != 80 {
        writer.push(":");
        writer.push_usize(account.port as usize);
    }
    writer.push("\r\naccept: application/vnd.koreader.v1+json\r\nx-auth-user: ");
    writer.push(account.username);
    writer.push("\r\nx-auth-key: ");
    writer.push_bytes(account.key_hex);
    writer.push("\r\nconnection: close\r\n");
}

/// GET /users/auth — checks the account before any progress exchange.
pub fn build_auth_request(buf: &mut [u8], account: &Account<'_>) -> Option<usize> {
    let mut writer = RequestWriter::new(buf);
    writer.push("GET /users/auth HTTP/1.1\r\n");
    push_common_headers(&mut writer, account);
    writer.push("\r\n");
    writer.finish()
}

/// GET /syncs/progress/:document — fetches the server's position.
pub fn build_get_progress_request(
    buf: &mut [u8],
    account: &Account<'_>,
    document_hex: &[u8; 32],
) -> Option<usize> {
    let mut writer = RequestWriter::new(buf);
    writer.push("GET /syncs/progress/");
    writer.push_bytes(document_hex);
    writer.push(" HTTP/1.1\r\n");
    push_common_headers(&mut writer, account);
    writer.push("\r\n");
    writer.finish()
}

/// PUT /syncs/progress — pushes our position. `percent_permille` is the
/// whole-book position in 0..=1000; the xpath-ish `progress` string is
/// what KOReader devices jump to, so a DocFragment index keeps them
/// chapter-accurate.
#[allow(clippy::too_many_arguments)]
pub fn build_put_progress_request(
    buf: &mut [u8],
    account: &Account<'_>,
    document_hex: &[u8; 32],
    percent_permille: u16,
    doc_fragment_1based: usize,
    device: &str,
    device_id_hex: &[u8; 32],
) -> Option<usize> {
    let mut body = [0u8; 256];
    let body_len = {
        let mut writer = RequestWriter::new(&mut body);
        writer.push("{\"document\":\"");
        writer.push_bytes(document_hex);
        writer.push("\",\"progress\":\"/body/DocFragment[");
        writer.push_usize(doc_fragment_1based);
        writer.push("]\",\"percentage\":");
        let permille = percent_permille.min(1000);
        writer.push_usize((permille / 1000) as usize);
        writer.push(".");
        let frac = permille % 1000;
        writer.push_usize((frac / 100) as usize);
        writer.push_usize((frac / 10 % 10) as usize);
        writer.push_usize((frac % 10) as usize);
        writer.push(",\"device\":\"");
        writer.push(device);
        writer.push("\",\"device_id\":\"");
        writer.push_bytes(device_id_hex);
        writer.push("\"}");
        writer.finish()?
    };

    let mut writer = RequestWriter::new(buf);
    writer.push("PUT /syncs/progress HTTP/1.1\r\n");
    push_common_headers(&mut writer, account);
    writer.push("content-type: application/json\r\ncontent-length: ");
    writer.push_usize(body_len);
    writer.push("\r\n\r\n");
    writer.push_bytes(&body[..body_len]);
    writer.finish()
}

pub struct Response<'a> {
    pub status: u16,
    pub body: &'a [u8],
}

/// Splits status line and headers off a complete HTTP/1.1 response held
/// in one buffer. Tolerates a missing body.
pub fn parse_response(raw: &[u8]) -> Option<Response<'_>> {
    let after_version = raw.split_first_chunk::<9>()?.1; // "HTTP/1.1 "
    if !raw.starts_with(b"HTTP/1.") {
        return None;
    }
    let digits = after_version.split_first_chunk::<3>()?.0;
    let status = digits.iter().try_fold(0u16, |acc, byte| match byte {
        b'0'..=b'9' => Some(acc * 10 + (byte - b'0') as u16),
        _ => None,
    })?;
    let body_start = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .unwrap_or(raw.len());
    Some(Response {
        status,
        body: &raw[body_start.min(raw.len())..],
    })
}

/// Pulls `"percentage": <float>` out of a progress body as permille.
/// Bounded scan, no JSON tree: kosync bodies are one flat object.
pub fn parse_percentage_permille(body: &[u8]) -> Option<u16> {
    let key = b"\"percentage\"";
    let key_at = body.windows(key.len()).position(|window| window == key)?;
    let mut rest = &body[key_at + key.len()..];
    while let Some((&first, tail)) = rest.split_first() {
        match first {
            b':' | b' ' | b'\t' => rest = tail,
            _ => break,
        }
    }
    let mut permille = 0u32;
    let mut seen_digit = false;
    let mut frac_scale: Option<u32> = None;
    for &byte in rest {
        match byte {
            b'0'..=b'9' => {
                seen_digit = true;
                let digit = (byte - b'0') as u32;
                match frac_scale {
                    None => permille = (permille * 10 + digit * 1000).min(2_000_000),
                    Some(scale) if scale <= 100 => {
                        permille += digit * scale;
                        frac_scale = Some(scale / 10);
                    }
                    Some(_) => {}
                }
            }
            b'.' if frac_scale.is_none() => frac_scale = Some(100),
            _ => break,
        }
    }
    if !seen_digit {
        return None;
    }
    Some(permille.min(1000) as u16)
}

/// Pulls `"device_id": "<hex>"` out of a progress body. Sync clients
/// must ignore their own echoes on pull, or a single device can be
/// yanked around by positions it wrote itself.
pub fn parse_device_id(body: &[u8], out: &mut [u8; 32]) -> Option<usize> {
    let key = b"\"device_id\"";
    let key_at = body.windows(key.len()).position(|window| window == key)?;
    let mut rest = &body[key_at + key.len()..];
    while let Some((&first, tail)) = rest.split_first() {
        match first {
            b':' | b' ' | b'\t' => rest = tail,
            _ => break,
        }
    }
    let (&quote, tail) = rest.split_first()?;
    if quote != b'\"' {
        return None;
    }
    let end = tail.iter().position(|byte| *byte == b'\"')?;
    let value = &tail[..end.min(32)];
    out[..value.len()].copy_from_slice(value);
    Some(value.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_str(digest: [u8; 16]) -> std::string::String {
        let hex = hex_digest(digest);
        core::str::from_utf8(&hex).unwrap().into()
    }

    extern crate std;

    #[test]
    fn md5_matches_rfc_1321_vectors() {
        assert_eq!(hex_str(md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex_str(md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex_str(md5(b"abcdefghijklmnopqrstuvwxyz")),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        assert_eq!(
            hex_str(md5(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            )),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn md5_streams_across_block_boundaries() {
        let mut streaming = Md5::new();
        let data = [0xa5u8; 1500];
        streaming.update(&data[..1]);
        streaming.update(&data[1..77]);
        streaming.update(&data[77..]);
        assert_eq!(streaming.finalize(), md5(&data));
    }

    #[test]
    fn partial_md5_samples_koreader_offsets() {
        // A 5000-byte file: samples land at 0, 1024, and 4096 (short),
        // then the 16384 read returns nothing and stops the loop.
        let file: std::vec::Vec<u8> = (0..5000u32).map(|value| value as u8).collect();
        let mut offsets = std::vec::Vec::new();
        let digest = partial_md5(&mut |offset, out: &mut [u8]| {
            let start = offset as usize;
            if start >= file.len() {
                return 0;
            }
            let take = (file.len() - start).min(out.len());
            out[..take].copy_from_slice(&file[start..start + take]);
            offsets.push(offset);
            take
        });
        assert_eq!(offsets, [0, 1024, 4096]);

        let mut expected = Md5::new();
        expected.update(&file[0..1024]);
        expected.update(&file[1024..2048]);
        expected.update(&file[4096..5000]);
        assert_eq!(digest, expected.finalize());
    }

    fn account_with(key_hex: &[u8; 32]) -> Account<'_> {
        Account {
            host: "sync.example.org",
            port: 8080,
            username: "jonatan",
            key_hex,
        }
    }

    #[test]
    fn auth_request_carries_kosync_headers() {
        let key_hex = hex_digest(md5(b"hunter2"));
        let mut buf = [0u8; 512];
        let len = build_auth_request(&mut buf, &account_with(&key_hex)).unwrap();
        let text = core::str::from_utf8(&buf[..len]).unwrap();
        assert!(text.starts_with("GET /users/auth HTTP/1.1\r\n"));
        assert!(text.contains("Host: sync.example.org:8080\r\n"));
        assert!(text.contains("accept: application/vnd.koreader.v1+json\r\n"));
        assert!(text.contains("x-auth-user: jonatan\r\n"));
        assert!(text.contains("x-auth-key: 2ab96390c7dbe3439de74d0c9b0b1767\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn put_progress_builds_flat_json() {
        let key_hex = hex_digest(md5(b"pw"));
        let document = hex_digest(md5(b"book"));
        let device_id = hex_digest(md5(b"device"));
        let mut buf = [0u8; 768];
        let len = build_put_progress_request(
            &mut buf,
            &account_with(&key_hex),
            &document,
            423,
            12,
            "xteink-x4",
            &device_id,
        )
        .unwrap();
        let text = core::str::from_utf8(&buf[..len]).unwrap();
        assert!(text.starts_with("PUT /syncs/progress HTTP/1.1\r\n"));
        assert!(text.contains("content-type: application/json\r\n"));
        let body = text.split("\r\n\r\n").nth(1).unwrap();
        assert!(body.contains("\"progress\":\"/body/DocFragment[12]\""));
        assert!(body.contains("\"percentage\":0.423"));
        assert!(body.contains("\"device\":\"xteink-x4\""));
        let content_length: usize = text
            .split("content-length: ")
            .nth(1)
            .unwrap()
            .split("\r\n")
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(content_length, body.len());
    }

    #[test]
    fn parses_progress_response() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"device\":\"kobo\",\"percentage\":0.6181,\"progress\":\"/body/DocFragment[8]/body/p[3]\"}";
        let response = parse_response(raw).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(parse_percentage_permille(response.body), Some(618));
    }

    #[test]
    fn parses_device_id_for_self_echo_detection() {
        let body = b"{\"device\":\"kobo\",\"device_id\":\"a1b2c3\",\"percentage\":0.5}";
        let mut out = [0u8; 32];
        let len = parse_device_id(body, &mut out).unwrap();
        assert_eq!(&out[..len], b"a1b2c3");
        assert!(parse_device_id(b"{}", &mut out).is_none());
    }

    #[test]
    fn parses_whole_number_percentage() {
        assert_eq!(parse_percentage_permille(b"{\"percentage\":1}"), Some(1000));
        assert_eq!(
            parse_percentage_permille(b"{\"percentage\": 0.05}"),
            Some(50)
        );
        assert_eq!(parse_percentage_permille(b"{}"), None);
    }

    #[test]
    fn rejects_garbage_response() {
        assert!(parse_response(b"NOPE").is_none());
        assert!(parse_response(b"HTTP/1.1 ").is_none());
    }
}
