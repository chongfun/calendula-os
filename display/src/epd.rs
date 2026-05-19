#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpiOp {
    /// Write command byte, then data bytes.
    Cmd { cmd: u8, data: &'static [u8] },
    /// Delay in milliseconds.
    DelayMs(u16),
    /// Assert/deassert reset pin.
    Reset,
}

/// Authoritative initialization sequence for the Good Display GDEQ0426T82
/// (SSD1677 controller) configured for 800x480 resolution.
pub static INIT_SEQUENCE: &[SpiOp] = &[
    SpiOp::Reset,
    SpiOp::Cmd { cmd: 0x12, data: &[] }, // SW Reset
    SpiOp::DelayMs(10),
    // Driver Output Control: 480 gate lines (0x01DF), scan direction
    SpiOp::Cmd { cmd: 0x01, data: &[0xDF, 0x01, 0x00] },
    // Data Entry Mode: X increment, Y increment
    SpiOp::Cmd { cmd: 0x11, data: &[0x03] },
    // Set RAM X: 0..99 bytes (800 pixels / 8)
    SpiOp::Cmd { cmd: 0x44, data: &[0x00, 0x63] },
    // Set RAM Y: 0..479 lines
    SpiOp::Cmd { cmd: 0x45, data: &[0x00, 0x00, 0xDF, 0x01] },
    // Border Waveform Control
    SpiOp::Cmd { cmd: 0x3C, data: &[0x01] },
    // Temperature Sensor: Internal
    SpiOp::Cmd { cmd: 0x18, data: &[0x80] },
    // Display Update Control 2: Load temp, enable clock
    SpiOp::Cmd { cmd: 0x22, data: &[0xB1] },
    SpiOp::Cmd { cmd: 0x20, data: &[] }, // Master Activation
];
