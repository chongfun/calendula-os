//! Runtime encoder for the onboarding hotspot's join QR.
//!
//! The hotspot's WPA2 PSK is minted per portal session on the device, so
//! the join code cannot be a build-time constant the way `qr_generated`'s
//! portal-URL QR is: the `WIFI:T:WPA;S:<ssid>;P:<psk>;;` payload is
//! assembled and encoded when the sync-portal screen renders, which is
//! not latency-sensitive. Encoding is Nayuki's qrcodegen in its no-heap
//! port (`qrcodegen-no-heap`, MIT) — `no_std`, caller-provided buffers —
//! so the same path serves the firmware, the host emulator, and the wasm
//! web emulator.

use qrcodegen_no_heap::{QrCode, QrCodeEcc, Version};

/// Highest QR version the scratch buffers accommodate. The payload is 18
/// bytes of `WIFI:` scaffolding around the SSID and the 16-byte PSK — 50
/// bytes for a [`app_core::PortalSsid`] — which byte mode fits in version 4
/// at EC level M (62-byte capacity), a 33-module symbol. Version 5 leaves
/// the encoder one version of slack without growing the buffers past 173
/// bytes each, and the 64-byte payload scratch below caps the SSID at 30.
pub const MAX_VERSION: Version = Version::new(5);

/// Required length of both scratch buffers handed to [`encode`].
pub const BUFFER_LEN: usize = 173;

/// Encodes `WIFI:T:WPA;S:{ssid};P:{psk};;` at EC level M with the
/// smallest version that fits (version 4 for the 16-char PSK) and an
/// automatically chosen mask. The PSK alphabet excludes every character
/// the `WIFI:` payload would need escaped (`\ ; , : "`), so `psk` is
/// spliced in verbatim. Returns `None` only when the payload cannot fit
/// [`MAX_VERSION`]; a portal-shaped PSK never triggers that.
pub fn encode<'a>(
    ssid: &str,
    psk: &str,
    temp: &mut [u8; BUFFER_LEN],
    out: &'a mut [u8; BUFFER_LEN],
) -> Option<QrCode<'a>> {
    let mut payload = [0u8; 64];
    let mut len = 0;
    for part in ["WIFI:T:WPA;S:", ssid, ";P:", psk, ";;"] {
        let bytes = part.as_bytes();
        if len + bytes.len() > payload.len() {
            return None;
        }
        payload[len..len + bytes.len()].copy_from_slice(bytes);
        len += bytes.len();
    }
    let text = core::str::from_utf8(&payload[..len]).ok()?;
    QrCode::encode_text(
        text,
        temp,
        out,
        QrCodeEcc::Medium,
        Version::MIN,
        MAX_VERSION,
        None,
        false,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO_PSK: &str = "emudemqpsk234567";

    fn demo_qr(out: &mut [u8; BUFFER_LEN]) -> QrCode<'_> {
        let mut temp = [0u8; BUFFER_LEN];
        let mut ssid = [0u8; app_core::PortalSsid::LEN];
        let ssid = app_core::PortalSsid::EMULATOR_DEMO.write_into(&mut ssid);
        encode(ssid, DEMO_PSK, &mut temp, out).expect("demo payload must encode")
    }

    /// The payload is 50 bytes for a [`app_core::PortalSsid`] name and a
    /// 16-character PSK, which byte mode fits in version 4 at EC level M.
    #[test]
    fn the_portal_payload_lands_in_version_4() {
        let mut out = [0u8; BUFFER_LEN];
        let qr = demo_qr(&mut out);
        assert_eq!(qr.version().value(), 4);
        assert_eq!(qr.size(), 33);
    }

    #[test]
    fn symbol_carries_the_fixed_structure() {
        let mut out = [0u8; BUFFER_LEN];
        let qr = demo_qr(&mut out);
        let size = qr.size();
        // Finder pattern corners are dark in every QR.
        for &(x, y) in &[(0, 0), (size - 1, 0), (0, size - 1)] {
            assert!(qr.get_module(x, y), "finder corner ({x},{y}) must be dark");
        }
        // The horizontal timing pattern alternates along row 6.
        for x in 8..size - 8 {
            assert_eq!(qr.get_module(x, 6), x % 2 == 0, "timing row at x={x}");
        }
        // The dark module the spec mandates at (8, 4 * version + 9).
        assert!(qr.get_module(8, size - 8));
    }

    #[test]
    fn oversized_psk_is_refused_not_truncated() {
        let long = "23456789ABCDEFGH23456789ABCDEFGH23456789ABCDEFGH";
        let mut temp = [0u8; BUFFER_LEN];
        let mut out = [0u8; BUFFER_LEN];
        let mut ssid = [0u8; app_core::PortalSsid::LEN];
        let ssid = app_core::PortalSsid::EMULATOR_DEMO.write_into(&mut ssid);
        assert!(encode(ssid, long, &mut temp, &mut out).is_none());
    }

    /// The name is per device, so the symbol has to change with it -- a QR
    /// that encoded one device's network on another's screen would join the
    /// wrong reader.
    #[test]
    fn a_different_device_encodes_a_different_symbol() {
        let mut a_buf = [0u8; BUFFER_LEN];
        let a = demo_qr(&mut a_buf);
        let a_modules: Vec<bool> = (0..a.size())
            .flat_map(|y| (0..a.size()).map(move |x| (x, y)))
            .map(|(x, y)| a.get_module(x, y))
            .collect();

        let mut temp = [0u8; BUFFER_LEN];
        let mut b_buf = [0u8; BUFFER_LEN];
        let mut other = [0u8; app_core::PortalSsid::LEN];
        let other = app_core::PortalSsid::from_mac_tail([0x2A, 0x2D, 0x6C]).write_into(&mut other);
        let b = encode(other, DEMO_PSK, &mut temp, &mut b_buf).expect("encodes");

        assert_eq!(a.size(), b.size(), "both still version 4");
        let b_modules: Vec<bool> = (0..b.size())
            .flat_map(|y| (0..b.size()).map(move |x| (x, y)))
            .map(|(x, y)| b.get_module(x, y))
            .collect();
        assert_ne!(a_modules, b_modules, "the symbol must carry the name");
    }
}
