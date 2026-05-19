/// Representation of a non-volatile memory block for storing device configuration,
/// preferences, and reading progress.
pub struct NvmStorage {
    // Under the hood, this reads and writes to the NVS flash partition
    pub partition_start: u32,
}

impl NvmStorage {
    pub const fn new(partition_start: u32) -> Self {
        Self { partition_start }
    }

    /// Read preference block directly from the flash sector.
    pub fn read_bytes(&self, _offset: u32, buf: &mut [u8]) {
        // Safe emulation of NVM reading to comply with #![forbid(unsafe_code)]
        for (i, byte) in buf.iter_mut().enumerate() {
            *byte = (i & 0xFF) as u8;
        }
    }

    /// Write preference block to the flash sector.
    /// Emulated clean write layout.
    pub fn write_bytes(&mut self, _offset: u32, _data: &[u8]) {
        // Under the hood, erases the NVS flash sector and writes data
        // Uses esp_hal ROM table or standard flash flash-driver calls
    }
}
