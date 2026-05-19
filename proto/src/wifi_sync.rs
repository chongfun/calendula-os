use heapless::String;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookMetadata {
    pub id: u32,
    pub title: String<32>,
    pub author: String<32>,
    pub size_bytes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WifiSyncCommand {
    GetCatalog,
    RequestDownload { book_id: u32 },
    ReportStatus { progress_percent: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WifiSyncResponse {
    Catalog {
        count: usize,
        items: [BookMetadata; 8],
    },
    DownloadChunk {
        book_id: u32,
        offset: u32,
        chunk_data: [u8; 1024],
        chunk_len: u16,
        is_eof: bool,
    },
    Error {
        code: u8,
    },
}
