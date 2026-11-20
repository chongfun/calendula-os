//! Platform-neutral `embassy-net` Wi-Fi shelf HTTP server, onboarding portal,
//! and captive DHCP/DNS responders shared across firmware targets.
//!
//! Chip crates own `esp-radio` controller bring-up, station scan/join state
//! machines, and loaned-heap buffer allocation, then hand the initialized
//! network stack and static buffers into this module.

use crate::upload::{self, UploadBegin, UploadChunk};
use crate::{
    PowerEvent, StorageCommand, SyncCommand, SyncEvent, POWER_EVENTS, STORAGE_COMMANDS,
    SYNC_COMMANDS, SYNC_EVENTS, UPLOAD_BEGINS, UPLOAD_CHUNKS, UPLOAD_INTERRUPTS, UPLOAD_RESULTS,
    UPLOAD_RETURNS, UPLOAD_SESSION_READY, UPLOAD_STOPPED, UPLOAD_STOP_REQUESTS,
    WIFI_STORAGE_RESULTS,
};
use app_core::{SyncError, WifiCredentials};
use embassy_futures::select::{select, Either};
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, Stack};
use embassy_time::{with_timeout, Duration, Instant};
use proto::captive;

pub const PORTAL_IP: [u8; 4] = [192, 168, 4, 1];
pub const PORTAL_SUBNET_PREFIX: u8 = 24;
pub const DNS_PORT: u16 = 53;
pub const DHCP_SERVER_PORT: u16 = 67;
pub const DHCP_CLIENT_PORT: u16 = 68;
pub const UPLOAD_STAGING_BUF_LEN: usize = 4096;
pub const DHCP_UDP_BUF_LEN: usize = 1536;
pub const DNS_UDP_BUF_LEN: usize = 1024;

/// Alphabet for the per-session portal PSK; lives in app-core next to
/// `PortalPsk` so the emulators' fixed demo value is host-tested against
/// it.
const PSK_ALPHABET: &[u8] = app_core::PSK_ALPHABET;

// mint_portal_psk's 6-bit draws can only reach indexes 0..=63; a longer
// alphabet would silently leave its tail characters unmintable.
const _: () = assert!(PSK_ALPHABET.len() <= 64);

/// Caller-supplied RX/TX byte buffers for a captive-portal UDP socket.
/// Allocated from the Wi-Fi session's loaned heap by the chip crate so
/// `fw-common` stays allocation-free.
pub struct SyncUdpBuffers {
    pub rx: &'static mut [u8],
    pub tx: &'static mut [u8],
}

/// Caller-supplied static/loaned buffers for the shelf HTTP upload server.
pub struct UploadServerBuffers {
    pub tcp_rx: &'static mut [u8],
    pub tcp_tx: &'static mut [u8],
    pub request_buf: &'static mut [u8],
    pub catalog: &'static mut [u8],
    pub catalog_len: usize,
    pub staging: [&'static mut [u8]; 2],
}

/// Caller-supplied static/loaned buffers for the onboarding portal HTTP server.
pub struct PortalHttpBuffers {
    pub tcp_rx: &'static mut [u8],
    pub tcp_tx: &'static mut [u8],
    pub request_buf: &'static mut [u8],
    pub network_options: &'static mut [u8],
    pub network_options_len: usize,
}

/// Platform callbacks needed by the shared upload server.
#[derive(Clone, Copy)]
pub struct WifiServerHooks {
    /// Returns `(used_bytes, free_bytes)` from the Wi-Fi session heap for
    /// post-upload slack logging.
    pub heap_stats: fn() -> (usize, usize),
}

/// Mutable upload-session context exposed to [`ExtraUploadRoutes`] handlers.
pub struct UploadSessionContext<'a> {
    pool: &'a mut heapless::Vec<&'static mut [u8], 2>,
    browser_activity: &'a mut Option<Instant>,
}

impl UploadSessionContext<'_> {
    pub async fn ensure_upload_session(&mut self) {
        ensure_upload_session(self.browser_activity).await;
    }

    pub fn reclaim_upload_pipeline(&mut self) {
        reclaim_upload_pipeline(self.pool);
    }
}

/// Optional extra HTTP route handler (e.g., `fw-c3`'s `powercut-selftest`
/// endpoints). Normal builds use [`NoExtraRoutes`], which compiles away to
/// nothing.
pub trait ExtraUploadRoutes {
    /// Handle a non-standard request before the built-in shelf routes run.
    ///
    /// Return `true` if the route was handled and a response was written to
    /// `socket`, or `false` to fall through to the built-in shelf routes
    /// without mutating `path`.
    fn handle<'a, 's>(
        &'a mut self,
        socket: &'a mut TcpSocket<'s>,
        is_post: bool,
        path: &'a mut [u8],
        ctx: UploadSessionContext<'a>,
    ) -> impl core::future::Future<Output = bool> + 'a
    where
        's: 'a;
}

/// Zero-sized no-op [`ExtraUploadRoutes`] for normal firmware builds.
pub struct NoExtraRoutes;

impl ExtraUploadRoutes for NoExtraRoutes {
    async fn handle<'a, 's>(
        &'a mut self,
        _socket: &'a mut TcpSocket<'s>,
        _is_post: bool,
        _path: &'a mut [u8],
        _ctx: UploadSessionContext<'a>,
    ) -> bool
    where
        's: 'a,
    {
        false
    }
}

/// Mints the onboarding hotspot's WPA2 PSK for this portal session from
/// the hardware RNG. Home credentials POST to /save over the hotspot
/// link, so it must not be open; and a PSK fixed at build time would be
/// public — committed to the repo or extractable from the released
/// firmware.bin — so it is drawn fresh here and travels only on the
/// screen's QR. Six-bit rejection sampling keeps the draw uniform over
/// the 55-character alphabet.
pub fn mint_portal_psk(mut next_u32: impl FnMut() -> u32) -> app_core::PortalPsk {
    let mut bytes = [0u8; app_core::PortalPsk::LEN];
    let mut filled = 0;
    while filled < bytes.len() {
        for byte in next_u32().to_le_bytes() {
            let draw = (byte & 0x3F) as usize;
            if draw < PSK_ALPHABET.len() && filled < bytes.len() {
                bytes[filled] = PSK_ALPHABET[draw];
                filled += 1;
            }
        }
    }
    // Every byte was drawn from PSK_ALPHABET, so validation cannot fail.
    app_core::PortalPsk::new(bytes).expect("minted PSK must be valid")
}

pub fn send_event(event: SyncEvent) {
    if SYNC_EVENTS.try_send(event).is_err() {
        crate::runtime_log!("wifi: sync event queue full");
    }
}

/// Persist the AP this session actually associated through, when it is not
/// already what the card holds.
///
/// Taken from what the radio reported on association rather than assumed
/// from the plan, because the scan path chooses an AP this task never names
/// — that choice is exactly what is worth remembering. Comparing against
/// the card's copy rather than the session's working hint keeps a repeat
/// session from spending an SD write to store what is already there, while
/// still writing after a scan corrected a stale hint.
pub async fn record_ap_hint(
    ssid: &str,
    learned: app_core::WifiApHint,
    stored: Option<app_core::WifiApHint>,
) {
    if stored == Some(learned) {
        return;
    }
    let Some(ssid) = app_core::WifiSsid::new(ssid) else {
        return;
    };
    crate::runtime_log!(
        "wifi: learned ap channel={} bssid={:02x?}",
        learned.channel,
        learned.bssid
    );
    STORAGE_COMMANDS
        .send(StorageCommand::StoreWifiApHint {
            ssid,
            hint: learned,
        })
        .await;
}

/// All Wi-Fi exits must flush pending frontlight settings and close any open
/// SD upload writer before reset, even in the onboarding portal or after a
/// failed join.
pub async fn persist_frontlight_before_exit() -> bool {
    loop {
        let changed = crate::latest_frontlight_change().is_some()
            || crate::UPLOAD_FRONTLIGHT_SAVE_FAILED.load(portable_atomic::Ordering::Acquire);
        match upload::frontlight_exit_action(upload::session_state(), changed) {
            upload::FrontlightExitAction::Done => return true,
            upload::FrontlightExitAction::WaitForClosing => {
                UPLOAD_SESSION_READY.wait().await;
            }
            upload::FrontlightExitAction::StartSession => {
                // The radio loan prevents ordinary queued storage writes.
                // Start the existing SD upload writer solely for its
                // durable frontlight teardown and acknowledgement.
                // The display task can enter and close the session (e.g. if
                // Sleep is already queued) before this task is scheduled.
                // A single readiness notification means state has changed;
                // re-evaluate it instead of waiting forever for Active.
                UPLOAD_SESSION_READY.try_take();
                STORAGE_COMMANDS.send(StorageCommand::ReceiveUpload).await;
                UPLOAD_SESSION_READY.wait().await;
            }
            upload::FrontlightExitAction::StopSession => {
                UPLOAD_STOP_REQUESTS.send(()).await;
                return UPLOAD_STOPPED.receive().await;
            }
        }
    }
}

/// Exit during the serving phase defers the reset until any in-flight book
/// finishes writing (bounded) and any pending frontlight edit is persisted.
/// Refuses reset on SD failure and allows the next Exit to retry.
pub async fn exit_after_uploads(reset_now: fn() -> !) -> ! {
    loop {
        if let SyncCommand::Exit = SYNC_COMMANDS.receive().await {
            if persist_frontlight_before_exit().await {
                reset_now();
            }
            crate::runtime_log!("wifi: exit refused; frontlight settings not persisted");
            send_event(SyncEvent::Failed(SyncError::Storage));
        }
    }
}

pub async fn park_until_exit(reset_now: fn() -> !) -> ! {
    // No Wi-Fi phase may bypass the upload/frontlight durability barrier.
    exit_after_uploads(reset_now).await
}

// ------------------------------------------------------------------
// Book upload server
// ------------------------------------------------------------------

pub const UPLOAD_PAGE: &str = concat!(
    r##"<!doctype html><html><head>"##,
    r##"<meta charset=utf-8>"##,
    r##"<meta name=viewport content="width=device-width,initial-scale=1">"##,
    r##"<title>Books · CalendulaOS</title><style>"##,
    r##"body{font-family:Georgia,'Times New Roman',serif;margin:3em auto;"##,
    r##"max-width:26em;padding:0 1.2em;color:#1a1a1a;background:#fbfbf8}"##,
    r##"h2{font-size:.8em;font-weight:600;letter-spacing:.25em;"##,
    r##"text-transform:uppercase;margin:2.2em 0 .8em}"##,
    r##"h2:first-of-type{margin-top:.5em}"##,
    r##"h2:before{content:'— '}"##,
    r##"ul{list-style:none;margin:0;padding:0}"##,
    r##"li{display:flex;align-items:baseline;justify-content:space-between;"##,
    r##"gap:1em;padding:.45em 0;border-bottom:1px dotted #bbb}"##,
    r##"li i{color:#777}"##,
    r##".del{font:inherit;font-size:.7em;letter-spacing:.2em;text-transform:uppercase;"##,
    r##"color:#888;background:none;border:0;padding:0;white-space:nowrap;cursor:pointer}"##,
    r##".del:hover,.del:focus{color:#1a1a1a}"##,
    r##"#drop{display:block;width:100%;font:inherit;font-style:italic;background:none;"##,
    r##"border:1px dashed #999;border-radius:3px;padding:2.2em 1em;"##,
    r##"text-align:center;color:#666;cursor:pointer}"##,
    r##"#drop.over{border-color:#1a1a1a;color:#1a1a1a}"##,
    r##"progress{width:7em;height:.45em;accent-color:#1a1a1a}"##,
    r##"footer{margin-top:3em;text-align:center;font-style:italic;"##,
    r##"color:#777;font-size:.85em}"##,
    r##"</style></head><body>"##,
    r##"<h2>Books</h2><ul id=shelf><li><i>reading the card …</i></li></ul>"##,
    r##"<h2>Add books</h2>"##,
    r##"<button id=drop type=button>drop EPUB files here — or click to choose</button>"##,
    r##"<input id=files type=file accept=.epub multiple hidden>"##,
    r##"<ul id=queue></ul>"##,
    r##"<footer>changes appear on the reader after it restarts ·"##,
    r##" press <b>done</b> there to finish</footer>"##,
    r##"<script>"##,
    r##"const shelf=document.getElementById('shelf'),"##,
    r##"queue=document.getElementById('queue'),"##,
    r##"drop=document.getElementById('drop'),"##,
    r##"input=document.getElementById('files'),"##,
    r##"dash=' '+String.fromCharCode(8212)+' ',"##,
    r##"pause=()=>new Promise(d=>setTimeout(d,800));"##,
    // The reader serves one connection at a time and listens again only
    // after closing the last connection, so every request waits its turn in
    // one queue with a pause between turns to let the reader return to accept.
    r##"let tail=Promise.resolve();"##,
    r##"function later(job){const run=tail.then(job);tail=run.then(pause,pause);return run}"##,
    r##"function row(label){const li=document.createElement('li');"##,
    r##"const span=document.createElement('span');span.textContent=label;"##,
    r##"li.appendChild(span);return li}"##,
    r##"function note(li,text){let n=li.querySelector('.note');"##,
    r##"if(!n){n=document.createElement('span');n.className='note';li.appendChild(n)}"##,
    r##"n.textContent=text}"##,
    r##"async function load(){let text=null;"##,
    r##"for(let i=0;i<10&&text===null;i++){try{"##,
    r##"const r=await fetch('/list');if(r.ok)text=await r.text();}"##,
    r##"catch(e){}if(text===null)await pause()}"##,
    r##"if(text===null){shelf.textContent='';"##,
    r##"shelf.appendChild(row('— the card did not answer —'));return}"##,
    r##"shelf.textContent='';"##,
    r##"const lines=text.split(String.fromCharCode(10)).filter(Boolean);"##,
    r##"if(!lines.length){shelf.appendChild(row('— nothing yet —'))}"##,
    r##"for(const line of lines){const[flag,open,label]=line.split('|');"##,
    r##"const li=row(label||open);"##,
    r##"const b=document.createElement('button');b.type='button';b.className='del';"##,
    r##"b.textContent='remove';b.onclick=()=>{"##,
    r##"if(!confirm('Remove '+(label||open)+' from the card?'))return;"##,
    r##"b.disabled=true;const token=Math.floor(Math.random()*1e9)+1;"##,
    r##"later(async()=>{let ok=false;const end=Date.now()+25000;"##,
    r##"for(let i=0;i<10&&Date.now()<end;i++){try{"##,
    r##"const r=await fetch('/delete?name='+encodeURIComponent(open)+"##,
    r##"(flag==='R'?'&root=1':'')+'&token='+token,{method:'POST'});"##,
    r##"ok=r.ok;break}"##,
    r##"catch(e){if(i<9&&Date.now()<end)await pause()}}"##,
    r##"if(ok)li.remove();else{b.disabled=false;note(li,dash+'not removed')}})};"##,
    r##"li.appendChild(b);shelf.appendChild(li)}}"##,
    r##"function send(files){for(const f of [...files]){"##,
    r##"const li=row(f.name);const bar=document.createElement('progress');"##,
    r##"bar.max=1;bar.value=0;li.appendChild(bar);queue.appendChild(li);"##,
    // The reader listens again only after it has closed the last connection,
    // so a turn that starts at once can be refused, as the shelf's can. No
    // answer and no byte sent is that refusal, and is tried again; anything
    // else is not, though a resend would be safe: a cut-off book is
    // discarded, and one stored but unanswered is replaced by its own bytes.
    r##"later(()=>new Promise(done=>{let tries=0;function go(){"##,
    r##"const xhr=new XMLHttpRequest();let sent=false;"##,
    r##"xhr.open('POST','/upload?name='+encodeURIComponent(f.name));"##,
    r##"xhr.upload.onprogress=e=>{if(e.loaded>0)sent=true;"##,
    r##"if(e.lengthComputable)bar.value=e.loaded/e.total};"##,
    r##"xhr.onloadend=()=>{if(xhr.status===0&&!sent&&++tries<10){setTimeout(go,800);return}"##,
    r##"bar.remove();note(li,xhr.status===200?' ✓':dash+'failed');"##,
    r##"done()};xhr.send(f)}go()}))}}"##,
    r##"drop.onclick=()=>input.click();"##,
    // Cleared once read, so picking the same file again (a retry after a
    // failure) is a change the browser reports.
    r##"input.onchange=()=>{send(input.files);input.value=''};"##,
    r##"drop.ondragover=e=>{e.preventDefault();drop.classList.add('over')};"##,
    r##"drop.ondragleave=()=>drop.classList.remove('over');"##,
    r##"drop.ondrop=e=>{e.preventDefault();drop.classList.remove('over');"##,
    r##"send(e.dataTransfer.files)};"##,
    // The shelf's own fetch holds the one connection too, so it takes the
    // first turn: a book dropped while the card is still being read waits.
    r##"later(load);"##,
    r##"</script></body></html>"##,
);

/// Ensure an active SD upload session is ready, coordinating with any session
/// currently tearing down so commands do not begin against a dying session.
pub async fn ensure_upload_session(browser_activity: &mut Option<Instant>) {
    note_browser_activity(browser_activity);
    loop {
        match upload::session_state() {
            upload::SessionState::Active => {
                UPLOAD_INTERRUPTS.try_take();
                return;
            }
            upload::SessionState::Closing => {
                UPLOAD_SESSION_READY.wait().await;
            }
            upload::SessionState::Inactive => {
                // Sleep can tear down the writer before this task resumes.
                // Observe one notification and recheck the current state.
                UPLOAD_SESSION_READY.try_take();
                STORAGE_COMMANDS.send(StorageCommand::ReceiveUpload).await;
                UPLOAD_SESSION_READY.wait().await;
            }
        }
    }
}

/// Serve the browser book-shelf HTTP server on port 80 with standard routes.
pub async fn serve_uploads(
    stack: Stack<'static>,
    buffers: UploadServerBuffers,
    hooks: WifiServerHooks,
) -> ! {
    serve_uploads_with_routes(stack, buffers, hooks, NoExtraRoutes).await
}

/// Serve the browser book-shelf HTTP server on port 80, allowing optional
/// build-specific routes to run ahead of the built-in shelf routes.
pub async fn serve_uploads_with_routes<R: ExtraUploadRoutes>(
    stack: Stack<'static>,
    buffers: UploadServerBuffers,
    hooks: WifiServerHooks,
    mut extra_routes: R,
) -> ! {
    let UploadServerBuffers {
        tcp_rx,
        tcp_tx,
        request_buf,
        catalog,
        catalog_len,
        staging: [staging_a, staging_b],
    } = buffers;
    let mut pool: heapless::Vec<&'static mut [u8], 2> = heapless::Vec::new();
    let _ = pool.push(staging_a);
    let _ = pool.push(staging_b);
    let mut browser_activity = None;
    let mut delete_cache = proto::upload::DeleteTokenCache::new();

    loop {
        upload::UPLOAD_IN_FLIGHT.store(false, portable_atomic::Ordering::SeqCst);
        let mut socket = TcpSocket::new(stack, &mut *tcp_rx, &mut *tcp_tx);
        socket.set_timeout(Some(Duration::from_secs(30)));
        if socket.accept(80).await.is_err() {
            continue;
        }
        upload::UPLOAD_IN_FLIGHT.store(true, portable_atomic::Ordering::SeqCst);

        let mut filled = 0;
        let head = loop {
            if filled == request_buf.len() {
                break None;
            }
            let read = match select(
                socket.read(&mut request_buf[filled..]),
                UPLOAD_INTERRUPTS.wait(),
            )
            .await
            {
                Either::First(Ok(read)) => read,
                Either::First(Err(_)) => break None,
                // Nothing is pipelined while headers trickle in, but a
                // consumed signal obliges the cleanup here (the post-parse
                // check below can no longer see it), and dropping the read
                // frees this serial server from a stalled client for the
                // retry that follows a cancelled sleep.
                Either::Second(()) => {
                    reclaim_upload_pipeline(&mut pool);
                    break None;
                }
            };
            if read == 0 {
                break None;
            }
            filled += read;
            if let Some(head) = captive::parse_request_head(&request_buf[..filled]) {
                break Some((
                    head.method.len(),
                    head.path.len(),
                    head.content_length,
                    head.body_start,
                ));
            }
        };
        let Some((method_len, path_len, content_length, body_start)) = head else {
            socket.close();
            continue;
        };
        note_browser_activity(&mut browser_activity);
        // A sleep-ended session may have died while this server was idle
        // (or between requests): consume the interrupt now so this request
        // starts a fresh session instead of feeding a writer that is gone.
        if UPLOAD_INTERRUPTS.try_take().is_some() {
            reclaim_upload_pipeline(&mut pool);
        }
        // Reborrow the pieces by index so the buffer stays usable for the
        // body bytes that arrived with the headers.
        let path_at = method_len + 1;
        let is_post = request_buf.get(..method_len).is_some_and(|m| m == b"POST");

        let handled_extra = if let Some(path_mut) = request_buf.get_mut(path_at..path_at + path_len)
        {
            let ctx = UploadSessionContext {
                pool: &mut pool,
                browser_activity: &mut browser_activity,
            };
            extra_routes
                .handle(&mut socket, is_post, path_mut, ctx)
                .await
        } else {
            false
        };

        if !handled_extra {
            let path = request_buf.get(path_at..path_at + path_len).unwrap_or(b"/");
            let route = path.split(|&b| b == b'?').next().unwrap_or(b"/");
            let is_upload_post = is_post && route == b"/upload";
            let is_list = route == b"/list";
            let is_delete = is_post && route == b"/delete";

            if is_list {
                let listing =
                    core::str::from_utf8(&catalog[..catalog_len.min(catalog.len())]).unwrap_or("");
                let _ = write_http_response_with_type(
                    &mut socket,
                    "200 OK",
                    "text/plain; charset=utf-8",
                    listing,
                )
                .await;
            } else if is_delete {
                let mut path_bytes = request_buf.get_mut(path_at..path_at + path_len);
                let in_books = path_bytes
                    .as_ref()
                    .map(|p| !proto::upload::has_query_param(p, b"root=1"))
                    .unwrap_or(true);
                let token = path_bytes
                    .as_ref()
                    .and_then(|p| proto::upload::parse_u32_param(p, b"token"));
                let name = path_bytes
                    .as_mut()
                    .and_then(|p| proto::upload::raw_query_name(p))
                    .and_then(|decoded| valid_short_name(decoded));
                let arrival_ms = Instant::now().as_millis();
                let (status, body) = match (name, token) {
                    (Some(name), Some(token))
                        if delete_cache.is_duplicate(&name, in_books, token, arrival_ms) =>
                    {
                        // This deletion already completed. Do not repeat it against the card,
                        // which could delete a newly uploaded replacement book reusing the same alias.
                        ("200 OK", "deleted")
                    }
                    (Some(name), token) => {
                        // If capacity is exhausted by active unexpired tokens, fail safely
                        // rather than silently deleting without idempotency protection.
                        if token.is_some() && delete_cache.is_full(arrival_ms) {
                            ("503 Service Unavailable", "capacity exhausted")
                        } else {
                            ensure_upload_session(&mut browser_activity).await;
                            UPLOAD_BEGINS
                                .send(UploadBegin {
                                    name: name.clone(),
                                    long_name: proto::upload::UploadFilename::new(),
                                    delete: true,
                                    in_books,
                                    legacy: None,
                                })
                                .await;
                            let success =
                                match select(UPLOAD_RESULTS.receive(), UPLOAD_INTERRUPTS.wait())
                                    .await
                                {
                                    Either::First(ok) => ok,
                                    Either::Second(()) => {
                                        reclaim_upload_pipeline(&mut pool);
                                        false
                                    }
                                };
                            if success {
                                if let Some(token) = token {
                                    let completed_ms = Instant::now().as_millis();
                                    let _ = delete_cache.record_success(
                                        name,
                                        in_books,
                                        token,
                                        completed_ms,
                                        proto::upload::DELETE_TOKEN_RETENTION_MS,
                                    );
                                }
                                ("200 OK", "deleted")
                            } else {
                                ("404 Not Found", "failed")
                            }
                        }
                    }
                    _ => ("404 Not Found", "failed"),
                };
                let _ = write_http_response(&mut socket, status, body).await;
            } else if is_upload_post {
                let client_name = request_buf
                    .get_mut(path_at..path_at + path_len)
                    .and_then(proto::upload::raw_query_name)
                    .map(|s| &*s)
                    .unwrap_or(b"book");
                // The long name is the whole of what this layer decides. The
                // 8.3 alias FAT needs alongside it is chosen by the installer,
                // which is the only place that can see which aliases are free.
                // Where the same book would be if it were uploaded before long
                // names existed: that alias plus the identity its sidecar
                // carries. The installer uses it only when nothing on the shelf
                // holds the long name.
                let legacy = Some(upload_store::install::LegacyKey {
                    alias: upload::sanitized_name(client_name),
                    identity: upload::hash_identity(client_name),
                });
                let begin = UploadBegin {
                    name: upload::UploadName::new(),
                    long_name: proto::upload::wireless_epub_filename(client_name),
                    delete: false,
                    in_books: true,
                    legacy,
                };

                ensure_upload_session(&mut browser_activity).await;
                let leftover_range = body_start..filled;
                let ok = match stream_book(
                    &mut socket,
                    request_buf,
                    leftover_range,
                    content_length,
                    begin,
                    &mut pool,
                    &mut browser_activity,
                    hooks,
                )
                .await
                {
                    StreamOutcome::Done(ok) => ok,
                    StreamOutcome::Interrupted => false,
                };
                let _ = write_http_response(
                    &mut socket,
                    if ok {
                        "200 OK"
                    } else {
                        "507 Insufficient Storage"
                    },
                    if ok { "stored" } else { "failed" },
                )
                .await;
            } else {
                let _ = write_http_response(&mut socket, "200 OK", UPLOAD_PAGE).await;
            }
        }
        socket.close();
        let _ = with_timeout(Duration::from_secs(2), socket.flush()).await;
    }
}

/// How one book stream ended: a writer verdict, or the session dying
/// underneath it (sleep won while the body was still streaming), which
/// obliges the caller to start a fresh session for the next request.
enum StreamOutcome {
    Done(bool),
    Interrupted,
}

/// How often browser traffic moves the idle-sleep deadline. Well inside the
/// Wireless leash, and sparse enough that a streaming upload does not crowd
/// the power queue.
const BROWSER_ACTIVITY_EVERY: Duration = Duration::from_secs(30);

/// Count the browser's requests, and the bytes of a long upload, as activity.
/// A sync session has no button presses, so without this the idle timer cut
/// an upload off 600 s after the press that started the session.
fn note_browser_activity(last: &mut Option<Instant>) {
    let now = Instant::now();
    if last.is_some_and(|at| now.saturating_duration_since(at) < BROWSER_ACTIVITY_EVERY) {
        return;
    }
    // Preserve activity even when the bounded notification queue is full.
    let seq = crate::note_browser_power_activity();
    let _ = POWER_EVENTS.try_send(PowerEvent::BrowserActivity(seq));
    *last = Some(now);
}

/// The writer exited on Sleep while this task may have been mid-pipeline:
/// pull every stale message out of the upload channels, take the loaned
/// buffers back into the pool, and drop the in-flight claim. Chunk sends
/// never block (a send is always preceded by acquiring one of the two
/// buffers, so at most one buffered chunk is ever queued), which is why
/// unblocking the two receive sides is enough to cancel the producer.
fn reclaim_upload_pipeline(pool: &mut heapless::Vec<&'static mut [u8], 2>) {
    crate::runtime_log!("upload: session interrupted; reclaiming pipeline");
    while UPLOAD_BEGINS.try_receive().is_ok() {}
    while let Ok(chunk) = UPLOAD_CHUNKS.try_receive() {
        if let Some(buffer) = chunk.buffer {
            let _ = pool.push(buffer);
        }
    }
    while let Ok(buffer) = UPLOAD_RETURNS.try_receive() {
        let _ = pool.push(buffer);
    }
    while UPLOAD_RESULTS.try_receive().is_ok() {}
    upload::UPLOAD_IN_FLIGHT.store(false, portable_atomic::Ordering::SeqCst);
}

/// Streams one book body to the display task; `Done(true)` when the card
/// write succeeded end to end.
#[allow(clippy::too_many_arguments)]
async fn stream_book(
    socket: &mut TcpSocket<'_>,
    request_buf: &[u8],
    leftover: core::ops::Range<usize>,
    content_length: usize,
    begin: UploadBegin,
    pool: &mut heapless::Vec<&'static mut [u8], 2>,
    browser_activity: &mut Option<Instant>,
    hooks: WifiServerHooks,
) -> StreamOutcome {
    crate::runtime_log!("upload: '{}' {} bytes", begin.name, content_length);
    upload::UPLOAD_IN_FLIGHT.store(true, portable_atomic::Ordering::SeqCst);
    UPLOAD_BEGINS.send(begin).await;

    let mut leftover = &request_buf[leftover];
    if leftover.len() > content_length {
        leftover = &leftover[..content_length];
    }
    let mut remaining = content_length;
    let mut failed = false;
    while remaining > 0 && !failed {
        let buffer = match pool.pop() {
            Some(buffer) => buffer,
            None => match select(UPLOAD_RETURNS.receive(), UPLOAD_INTERRUPTS.wait()).await {
                Either::First(buffer) => buffer,
                Either::Second(()) => {
                    reclaim_upload_pipeline(pool);
                    return StreamOutcome::Interrupted;
                }
            },
        };
        let mut len = 0;
        if !leftover.is_empty() {
            let take = leftover.len().min(buffer.len());
            buffer[..take].copy_from_slice(&leftover[..take]);
            leftover = &leftover[take..];
            len = take;
        }
        while len < buffer.len() && len < remaining {
            let window = buffer.len().min(remaining);
            match select(
                socket.read(&mut buffer[len..window]),
                UPLOAD_INTERRUPTS.wait(),
            )
            .await
            {
                Either::First(Ok(0)) | Either::First(Err(_)) => {
                    failed = true;
                    break;
                }
                Either::First(Ok(read)) => len += read,
                // The writer is gone, so the bytes read so far describe a
                // book no one will finish: don't sit out a stalled client's
                // socket timeout for them. Dropping the read future is
                // cancel-safe; the buffer in hand goes straight back.
                Either::Second(()) => {
                    let _ = pool.push(buffer);
                    reclaim_upload_pipeline(pool);
                    return StreamOutcome::Interrupted;
                }
            }
        }
        remaining -= len.min(remaining);
        note_browser_activity(browser_activity);
        UPLOAD_CHUNKS
            .send(UploadChunk {
                buffer: Some(buffer),
                len,
                last: remaining == 0 && !failed,
                abort: failed,
            })
            .await;
    }
    if content_length == 0 {
        // Nothing will flow; tell the writer to finish an empty file.
        UPLOAD_CHUNKS
            .send(UploadChunk {
                buffer: None,
                len: 0,
                last: true,
                abort: true,
            })
            .await;
    }
    // Refill the pool for the next file.
    let result = match select(UPLOAD_RESULTS.receive(), UPLOAD_INTERRUPTS.wait()).await {
        Either::First(result) => result,
        Either::Second(()) => {
            reclaim_upload_pipeline(pool);
            return StreamOutcome::Interrupted;
        }
    };
    upload::UPLOAD_IN_FLIGHT.store(false, portable_atomic::Ordering::SeqCst);
    // Heap slack per upload: the join-time log plus this one bound the
    // radio buffering budget (AMPDU reorder buffers allocate under load).
    let (used, free) = (hooks.heap_stats)();
    crate::runtime_log!("upload: heap used={} free={}", used, free);
    while pool.len() < 2 {
        match UPLOAD_RETURNS.try_receive() {
            Ok(buffer) => {
                let _ = pool.push(buffer);
            }
            Err(_) => break,
        }
    }
    StreamOutcome::Done(result && !failed)
}

// ------------------------------------------------------------------
// Onboarding portal
// ------------------------------------------------------------------

/// The credential form, served in three pieces so the nearby-network
/// `<option>` list (scanned once at portal start, HTML-escaped, held in a
/// loaned buffer) can sit between the static prefix and suffix.
const PORTAL_PAGE_PREFIX: &str = concat!(
    "<!doctype html><html><head>",
    "<meta name=viewport content=\"width=device-width,initial-scale=1\">",
    "<title>CalendulaOS</title>",
    "<style>body{font-family:Georgia,serif;margin:2.5em auto;max-width:22em;",
    "padding:0 1em;color:#222}h1{font-size:1.25em;letter-spacing:.08em}",
    "label{display:block;margin:1em 0 .2em}",
    "input,select{width:100%;font-size:1.05em;padding:.5em;border:1px solid #999;",
    "border-radius:4px;box-sizing:border-box}",
    "button{margin-top:1.2em;font-size:1.05em;padding:.6em 1.6em;",
    "border:1px solid #222;background:#222;color:#fff;border-radius:4px}",
    "</style></head><body><h1>CalendulaOS</h1>",
    "<p>Connect this reader to your Wi-Fi network.</p>",
    "<form method=post action=/save>",
    "<label>Network</label><select name=ssid>",
);

const PORTAL_PAGE_SUFFIX: &str = concat!(
    "<option value=\"\">Other or hidden network</option></select>",
    "<label>Other network name</label><input name=ssid_custom maxlength=32>",
    "<label>Password</label><input name=pass type=password maxlength=64>",
    "<button>Save</button></form></body></html>",
);

const SAVED_PAGE: &str = concat!(
    "<!doctype html><html><head>",
    "<meta name=viewport content=\"width=device-width,initial-scale=1\">",
    "<title>CalendulaOS</title>",
    "<style>body{font-family:Georgia,serif;margin:2.5em auto;max-width:22em;",
    "padding:0 1em;color:#222}h1{font-size:1.25em;letter-spacing:.08em}",
    "</style></head><body><h1>Saved</h1>",
    "<p>Back on the reader: press <i>done</i>, then run sync again to ",
    "connect to your network.</p></body></html>",
);

/// Serve the captive DHCP responder on UDP port 67.
pub async fn serve_captive_dhcp(stack: Stack<'static>, udp_buffers: SyncUdpBuffers) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        udp_buffers.rx,
        &mut tx_meta,
        udp_buffers.tx,
    );
    if socket.bind(DHCP_SERVER_PORT).is_err() {
        crate::runtime_log!("portal: dhcp bind failed");
        loop {
            core::future::pending::<()>().await;
        }
    }
    let mut server = captive::DhcpServer::new(PORTAL_IP);
    let mut packet = [0u8; 600];
    let mut reply = [0u8; captive::DHCP_REPLY_LEN];
    loop {
        let Ok((len, _meta)) = socket.recv_from(&mut packet).await else {
            continue;
        };
        if let Some(reply_len) = server.handle(&packet[..len], &mut reply) {
            let _ = socket
                .send_to(
                    &reply[..reply_len],
                    (IpAddress::v4(255, 255, 255, 255), DHCP_CLIENT_PORT),
                )
                .await;
        }
    }
}

/// Serve the captive DNS catch-all responder on UDP port 53.
pub async fn serve_captive_dns(stack: Stack<'static>, udp_buffers: SyncUdpBuffers) -> ! {
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut socket = UdpSocket::new(
        stack,
        &mut rx_meta,
        udp_buffers.rx,
        &mut tx_meta,
        udp_buffers.tx,
    );
    if socket.bind(DNS_PORT).is_err() {
        crate::runtime_log!("portal: dns bind failed");
        loop {
            core::future::pending::<()>().await;
        }
    }
    let mut query = [0u8; 300];
    let mut answer = [0u8; 360];
    loop {
        let Ok((len, meta)) = socket.recv_from(&mut query).await else {
            continue;
        };
        if let Some(answer_len) = captive::dns_answer(&query[..len], PORTAL_IP, &mut answer) {
            let _ = socket.send_to(&answer[..answer_len], meta).await;
        }
    }
}

/// Serve the onboarding credential form on TCP port 80.
pub async fn serve_onboarding_http(stack: Stack<'static>, buffers: PortalHttpBuffers) -> ! {
    let PortalHttpBuffers {
        tcp_rx,
        tcp_tx,
        request_buf,
        network_options,
        network_options_len,
    } = buffers;
    let mut browser_activity = None;

    loop {
        let mut socket = TcpSocket::new(stack, &mut *tcp_rx, &mut *tcp_tx);
        socket.set_timeout(Some(Duration::from_secs(10)));
        if socket.accept(80).await.is_err() {
            continue;
        }

        let mut filled = 0;
        let saved = loop {
            if filled == request_buf.len() {
                break false;
            }
            let Ok(read) = socket.read(&mut request_buf[filled..]).await else {
                break false;
            };
            if read == 0 {
                break false;
            }
            filled += read;
            if let Some(request) = captive::parse_request(&request_buf[..filled]) {
                note_browser_activity(&mut browser_activity);
                break handle_portal_request(&request).await;
            }
        };

        if saved {
            let _ = write_http_page(&mut socket, SAVED_PAGE).await;
        } else {
            let _ = write_portal_page(&mut socket, &network_options[..network_options_len]).await;
        }
        socket.close();
        let _ = with_timeout(Duration::from_secs(2), socket.flush()).await;
    }
}

/// Routes one parsed request; true means credentials were captured and
/// the success page should answer.
async fn handle_portal_request(request: &captive::HttpRequest<'_>) -> bool {
    if request.method != "POST" || request.path != "/save" {
        return false;
    }
    let Some(credentials) = WifiCredentials::from_portal_form(request.body) else {
        return false;
    };
    crate::runtime_log!("portal: credentials captured for '{}'", credentials.ssid());
    let ssid = credentials.ssid_message();
    STORAGE_COMMANDS
        .send(StorageCommand::StoreWifiCredentials(credentials))
        .await;
    if !WIFI_STORAGE_RESULTS.receive().await {
        // The card refused or corrupted the write; answering with the form
        // again (not the success page) tells the user to retry.
        crate::runtime_log!("portal: credential storage failed");
        return false;
    }
    send_event(SyncEvent::CredentialsSaved(ssid));
    true
}

/// Append one HTML-escaped `<option value="...">...</option>` entry for `ssid`
/// into `out` at `*at`. Returns `false` and restores `*at` if the entry does
/// not fit, so `out[..*at]` always ends on a complete `</option>`.
pub fn push_network_option(out: &mut [u8], at: &mut usize, ssid: &str) -> bool {
    let option_start = *at;
    if !push_bytes(out, at, b"<option value=\"")
        || !push_html_escaped(out, at, ssid.as_bytes())
        || !push_bytes(out, at, b"\">")
        || !push_html_escaped(out, at, ssid.as_bytes())
        || !push_bytes(out, at, b"</option>")
    {
        *at = option_start;
        return false;
    }
    true
}

fn push_html_escaped(out: &mut [u8], at: &mut usize, value: &[u8]) -> bool {
    for byte in value.iter().copied() {
        let escaped: &[u8] = match byte {
            b'&' => b"&amp;",
            b'<' => b"&lt;",
            b'>' => b"&gt;",
            b'\"' => b"&quot;",
            b'\'' => b"&#39;",
            _ => core::slice::from_ref(&byte),
        };
        if !push_bytes(out, at, escaped) {
            return false;
        }
    }
    true
}

fn push_bytes(out: &mut [u8], at: &mut usize, value: &[u8]) -> bool {
    let Some(end) = at.checked_add(value.len()) else {
        return false;
    };
    if end > out.len() {
        return false;
    }
    out[*at..end].copy_from_slice(value);
    *at = end;
    true
}

/// The portal form with the scanned network options spliced between its
/// static prefix and suffix, under no-store so a stale list is never
/// resurrected from browser cache.
async fn write_portal_page(
    socket: &mut TcpSocket<'_>,
    options: &[u8],
) -> Result<(), embassy_net::tcp::Error> {
    write_all(
        socket,
        b"HTTP/1.1 200 OK\r\ncache-control: no-store\r\ncontent-type: text/html; charset=utf-8\r\nconnection: close\r\n\r\n",
    )
    .await?;
    write_all(socket, PORTAL_PAGE_PREFIX.as_bytes()).await?;
    write_all(socket, options).await?;
    write_all(socket, PORTAL_PAGE_SUFFIX.as_bytes()).await
}

/// Accepts an existing 8.3 catalog open-name verbatim, and only a book's:
/// see `proto::upload::is_book_alias`. Deletion must not invent or mangle
/// names, nor reach a directory or any name of a kind the shelf does not
/// list.
pub fn valid_short_name(raw: &[u8]) -> Option<upload::UploadName> {
    if !proto::upload::is_book_alias(raw) {
        return None;
    }
    let mut name = upload::UploadName::new();
    for byte in raw.iter().copied() {
        let _ = name.push(byte as char);
    }
    Some(name)
}

async fn write_http_page(
    socket: &mut TcpSocket<'_>,
    body: &str,
) -> Result<(), embassy_net::tcp::Error> {
    write_http_response(socket, "200 OK", body).await
}

pub async fn write_http_response(
    socket: &mut TcpSocket<'_>,
    status: &str,
    body: &str,
) -> Result<(), embassy_net::tcp::Error> {
    write_http_response_with_type(socket, status, "text/html; charset=utf-8", body).await
}

pub async fn write_http_response_with_type(
    socket: &mut TcpSocket<'_>,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<(), embassy_net::tcp::Error> {
    let mut length = [0u8; 8];
    let mut at = length.len();
    let mut value = body.len();
    loop {
        at -= 1;
        length[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    write_all(socket, b"HTTP/1.1 ").await?;
    write_all(socket, status.as_bytes()).await?;
    write_all(socket, b"\r\ncache-control: no-store\r\ncontent-type: ").await?;
    write_all(socket, content_type.as_bytes()).await?;
    write_all(socket, b"\r\ncontent-length: ").await?;
    write_all(socket, &length[at..]).await?;
    write_all(socket, b"\r\nconnection: close\r\n\r\n").await?;
    write_all(socket, body.as_bytes()).await
}

async fn write_all(
    socket: &mut TcpSocket<'_>,
    mut data: &[u8],
) -> Result<(), embassy_net::tcp::Error> {
    while !data.is_empty() {
        let written = socket.write(data).await?;
        if written == 0 {
            return Err(embassy_net::tcp::Error::ConnectionReset);
        }
        data = &data[written..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_short_name_accepts_book_alias_and_rejects_directories() {
        assert_eq!(
            valid_short_name(b"BOOK01.EPU").as_deref(),
            Some("BOOK01.EPU")
        );
        assert_eq!(valid_short_name(b"README.TXT"), None);
        assert_eq!(valid_short_name(b"BOOKS"), None);
        assert_eq!(valid_short_name(b""), None);
    }

    #[test]
    fn push_network_option_escapes_html_and_rolls_back_on_overflow() {
        let mut buf = [0u8; 128];
        let mut at = 0;
        assert!(push_network_option(&mut buf, &mut at, "A&B<\"'>"));
        assert_eq!(
            &buf[..at],
            b"<option value=\"A&amp;B&lt;&quot;&#39;&gt;\">A&amp;B&lt;&quot;&#39;&gt;</option>"
        );
        let saved_at = at;
        // Too small for a second full option: must roll back cleanly.
        let mut small = [0u8; 90];
        small[..at].copy_from_slice(&buf[..at]);
        assert!(!push_network_option(&mut small, &mut at, "SecondNet"));
        assert_eq!(at, saved_at);
    }

    #[test]
    fn mint_portal_psk_uses_rejection_sampling_over_alphabet() {
        // Feed values >= PSK_ALPHABET.len() mixed with valid indexes.
        let mut state = 0x3F3F_3F00u32;
        let psk = mint_portal_psk(|| {
            let out = state;
            state = state.wrapping_add(0x0102_0304);
            out
        });
        assert_eq!(psk.as_str().len(), app_core::PortalPsk::LEN);
        for b in psk.as_str().bytes() {
            assert!(PSK_ALPHABET.contains(&b));
        }
    }
}
