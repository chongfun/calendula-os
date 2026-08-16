//! The Wi-Fi session task behind the Wireless screen.
//!
//! Parked until the app sends `SyncCommand::Start`. The session is one
//! way by design: it asks the display task to loan the reader's scratch
//! memory as radio heap, joins the saved network in STA mode, and
//! serves the browser book shelf until the session ends; the
//! only path back to reading is the software reset on
//! `SyncCommand::Exit`. With no saved network the session runs the
//! AP-mode onboarding portal instead.

use crate::sync_mem::{self, SyncLoan};
use crate::upload::{UploadBegin, UploadChunk};
use crate::{
    StorageCommand, SyncCommand, SyncEvent, STORAGE_COMMANDS, SYNC_COMMANDS, SYNC_EVENTS,
    SYNC_LOANS, UPLOAD_BEGINS, UPLOAD_CHUNKS, UPLOAD_INTERRUPTS, UPLOAD_RESULTS, UPLOAD_RETURNS,
};
use app_core::{SyncError, WifiCredentials};
use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_futures::select::{select, Either};
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{
    Config as NetConfig, IpAddress, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources,
    StaticConfigV4,
};
use embassy_time::{with_timeout, Duration, Timer};
use esp_hal::peripherals::WIFI;
use esp_hal::rng::Rng;
use esp_radio::wifi::{
    ap::AccessPointConfig,
    scan::ScanConfig,
    sta::{ScanMethod, StationConfig},
    AuthenticationMethod, Config as WifiConfig, ControllerConfig, Interface, WifiController,
};
use proto::captive;

// Measured first-association joins ran ~21 s; give them headroom.
const JOIN_TIMEOUT: Duration = Duration::from_secs(35);
/// A directed join talks to one AP on one channel and does no scanning, so
/// it either associates quickly or the hint is wrong. Bounded well below
/// [`JOIN_TIMEOUT`] so a stale hint costs seconds before the scan fallback,
/// not most of the budget the fallback still needs.
const DIRECTED_JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Grace on top of a join deadline for the radio to report a terminal
/// result.
///
/// A deadline here can only stop this task waiting; it cannot stop the
/// radio. esp-radio has no way to cancel an association in flight —
/// `disconnect_async` refuses while the station is still connecting, and
/// `set_config` only stops the controller when the *mode* changes, which
/// Directed->Scan does not — so the attempt has to be seen through to a
/// terminal event before another plan is applied. Also bounds the
/// disconnect that retires a leftover association.
const RADIO_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
const DHCP_TIMEOUT: Duration = Duration::from_secs(15);
/// This device's hotspot name, from the low three bytes of its MAC.
///
/// Read here rather than fixed at build time because two readers running this
/// firmware raise two hotspots, and a name keyed on the board could not tell
/// a pair of the same model apart. The screen names the network the join QR
/// points at, so one string beacons, renders, and encodes -- the AP and the QR
/// cannot drift.
fn portal_ssid() -> app_core::PortalSsid {
    let mac = esp_hal::efuse::base_mac_address();
    let bytes = mac.as_bytes();
    app_core::PortalSsid::from_mac_tail([bytes[3], bytes[4], bytes[5]])
}
const PORTAL_IP: [u8; 4] = [192, 168, 4, 1];

/// Alphabet for the per-session portal PSK; lives in app-core next to
/// `PortalPsk` so the emulators' fixed demo value is host-tested against
/// it.
const PSK_ALPHABET: &[u8] = app_core::PSK_ALPHABET;

// mint_portal_psk's 6-bit draws can only reach indexes 0..=63; a longer
// alphabet would silently leave its tail characters unmintable.
const _: () = assert!(PSK_ALPHABET.len() <= 64);

/// Mints the onboarding hotspot's WPA2 PSK for this portal session from
/// the hardware RNG. Home credentials POST to /save over the hotspot
/// link, so it must not be open; and a PSK fixed at build time would be
/// public — committed to the repo or extractable from the released
/// firmware.bin — so it is drawn fresh here and travels only on the
/// screen's QR. Six-bit rejection sampling keeps the draw uniform over
/// the 55-character alphabet.
fn mint_portal_psk(rng: Rng) -> app_core::PortalPsk {
    let mut bytes = [0u8; app_core::PortalPsk::LEN];
    let mut filled = 0;
    while filled < bytes.len() {
        for byte in rng.random().to_le_bytes() {
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

/// Compile-time station credentials for the dev phase:
/// `CALENDULA_WIFI_SSID=... CALENDULA_WIFI_PASS=... cargo build ...`
pub fn credentials() -> Option<(&'static str, &'static str)> {
    Some((
        option_env!("CALENDULA_WIFI_SSID")?,
        option_env!("CALENDULA_WIFI_PASS")?,
    ))
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn ap_net_task(mut runner: Runner<'static, Interface>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
pub async fn run(spawner: Spawner, wifi: WIFI<'static>) {
    // Idle until a Start whose loan is granted; Exit before any radio work
    // is a no-op because nothing has been loaned yet. A refused loan (the
    // display task could not flush the reading position to the card) lands
    // the Wireless screen in Error, where Confirm arms another Start, so
    // this loops rather than stranding the screen on a loan that will
    // never arrive.
    let loan = loop {
        loop {
            match SYNC_COMMANDS.receive().await {
                SyncCommand::Start => break,
                SyncCommand::Exit => {}
            }
        }

        // The loan request runs through the storage queue so it serializes
        // behind any in-flight SD work, then the memory comes back to us.
        STORAGE_COMMANDS.send(StorageCommand::LoanSyncMemory).await;
        match SYNC_LOANS.receive().await {
            Ok(loan) => break loan,
            Err(error) => send_event(SyncEvent::Failed(error)),
        }
    };
    sync_mem::donate_heap(loan.heap_a, loan.heap_b, loan.heap_c);
    let SyncLoan {
        tcp_rx,
        tcp_tx,
        http_a,
        http_b,
        wifi: stored_credentials,
        wifi_hint,
        catalog_len,
        ..
    } = loan;

    // Stored credentials from the portal beat the compile-time dev pair;
    // neither present means this session runs the onboarding portal.
    let resolved = stored_credentials.or_else(|| {
        credentials().and_then(|(ssid, password)| WifiCredentials::from_strs(ssid, password))
    });

    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    // Deliberately trimmed, and re-validated on hardware 2026-07-11: an
    // upload A/B (3.2 MB EPUB, X3) measured no win from 8/24 RX buffers
    // + AMPDU-RX — throughput sits near 160 KB/s with either config, so
    // radio RX is not the bottleneck — while the bigger buffers cost
    // ~6.6 KB of the loaned heap at join. Before spending heap here
    // again, first find what actually caps upload throughput (the
    // per-upload heap log makes the slack observable).
    let radio_config = ControllerConfig::default()
        .with_static_rx_buf_num(4)
        .with_dynamic_rx_buf_num(8)
        .with_dynamic_tx_buf_num(8)
        .with_ampdu_rx_enable(false)
        .with_ampdu_tx_enable(false);
    let mut controller = match WifiController::new(wifi, radio_config) {
        Ok(controller) => controller,
        Err(err) => {
            esp_println::println!("wifi: init failed: {:?}", err);
            send_event(SyncEvent::Failed(SyncError::RadioInit));
            park_until_exit().await;
        }
    };
    let Some(creds) = resolved else {
        run_portal(
            spawner,
            &mut controller,
            seed,
            tcp_rx,
            tcp_tx,
            http_a,
            http_b,
        )
        .await;
    };

    let device = Interface::station();

    let resources: &'static mut StackResources<4> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(StackResources::new()));
    let (stack, runner) = embassy_net::new(
        device,
        NetConfig::dhcpv4(Default::default()),
        resources,
        seed,
    );
    spawner.spawn(net_task(runner).unwrap());

    let mut session = Session {
        controller,
        stack,
        configured: None,
    };
    // Compile-time credentials never came from a join this device made, so
    // a stored hint cannot belong to them.
    let stored_hint = stored_credentials.and(wifi_hint);
    // The route the next attempt aims at. It starts as the card's copy but
    // moves with what this session learns: retired once it has missed, and
    // replaced by whatever a scan actually associated through. `stored_hint`
    // stays as the card's copy so the write only happens on a real change.
    let mut hint = stored_hint;

    // First Start already consumed; later Starts are Confirm retries
    // from the error screen. A successful join falls through to the
    // book server, which runs until the session's reset.
    let ip = loop {
        match session
            .attempt(creds.ssid(), creds.password(), &mut hint, stored_hint)
            .await
        {
            Ok(ip) => break ip,
            Err(error) => send_event(SyncEvent::Failed(error)),
        }
        // Start retries the session, Exit resets the device.
        match SYNC_COMMANDS.receive().await {
            SyncCommand::Start => {}
            SyncCommand::Exit => reset_now(),
        }
    };

    let stack = session.stack;
    esp_println::println!("upload: serving at {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
    send_event(SyncEvent::Serving(ip));
    select(
        exit_after_uploads(),
        upload_server(stack, tcp_rx, tcp_tx, http_a, http_b, catalog_len),
    )
    .await;
    unreachable!()
}

/// Exit during the serving phase defers the reset until any in-flight
/// book finishes writing (bounded), so a done press cannot truncate it.
async fn exit_after_uploads() -> ! {
    loop {
        if let SyncCommand::Exit = SYNC_COMMANDS.receive().await {
            if crate::upload::UPLOAD_SESSION_ACTIVE.load(portable_atomic::Ordering::SeqCst) {
                crate::UPLOAD_STOP_REQUESTS.send(()).await;
                crate::UPLOAD_STOPPED.receive().await;
            }
            reset_now();
        }
    }
}

// ------------------------------------------------------------------
// Book upload server
// ------------------------------------------------------------------

const UPLOAD_PAGE: &str = concat!(
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
    r##"a.del{font-size:.7em;letter-spacing:.2em;text-transform:uppercase;"##,
    r##"color:#888;text-decoration:none;white-space:nowrap;cursor:pointer}"##,
    r##"a.del:hover{color:#1a1a1a}"##,
    r##"#drop{border:1px dashed #999;border-radius:3px;padding:2.2em 1em;"##,
    r##"text-align:center;color:#666;font-style:italic;cursor:pointer}"##,
    r##"#drop.over{border-color:#1a1a1a;color:#1a1a1a}"##,
    r##"progress{width:7em;height:.45em;accent-color:#1a1a1a}"##,
    r##"footer{margin-top:3em;text-align:center;font-style:italic;"##,
    r##"color:#777;font-size:.85em}"##,
    r##"</style></head><body>"##,
    r##"<h2>Books</h2><ul id=shelf><li><i>reading the card …</i></li></ul>"##,
    r##"<h2>Add books</h2>"##,
    r##"<div id=drop>drop EPUB files here — or click to choose</div>"##,
    r##"<input id=files type=file accept=.epub multiple hidden>"##,
    r##"<ul id=queue></ul>"##,
    r##"<footer>changes appear on the reader after it restarts ·"##,
    r##" press <b>done</b> there to finish</footer>"##,
    r##"<script>"##,
    r##"const shelf=document.getElementById('shelf'),"##,
    r##"queue=document.getElementById('queue'),"##,
    r##"drop=document.getElementById('drop'),"##,
    r##"input=document.getElementById('files');"##,
    r##"function row(label){const li=document.createElement('li');"##,
    r##"const span=document.createElement('span');span.textContent=label;"##,
    r##"li.appendChild(span);return li}"##,
    r##"async function load(){let text=null;"##,
    r##"for(let i=0;i<10&&text===null;i++){try{"##,
    r##"const r=await fetch('/list');if(r.ok)text=await r.text();}"##,
    r##"catch(e){}if(text===null)await new Promise(d=>setTimeout(d,800))}"##,
    r##"if(text===null){shelf.textContent='';"##,
    r##"shelf.appendChild(row('— the card did not answer —'));return}"##,
    r##"shelf.textContent='';"##,
    r##"const lines=text.split(String.fromCharCode(10)).filter(Boolean);"##,
    r##"if(!lines.length){shelf.appendChild(row('— nothing yet —'))}"##,
    r##"for(const line of lines){const[flag,open,label]=line.split('|');"##,
    r##"const li=row(label||open);"##,
    r##"const a=document.createElement('a');a.className='del';"##,
    r##"a.textContent='remove';a.onclick=async()=>{"##,
    r##"if(!confirm('Remove '+(label||open)+' from the card?'))return;"##,
    r##"const r=await fetch('/delete?name='+encodeURIComponent(open)+"##,
    r##"(flag==='R'?'&root=1':''),"##,
    r##"{method:'POST'});if(r.ok)li.remove()};li.appendChild(a);"##,
    r##"shelf.appendChild(li)}}"##,
    r##"function send(files){[...files].reduce((chain,f)=>chain.then(()=>new Promise(done=>{"##,
    r##"const li=row(f.name);const bar=document.createElement('progress');"##,
    r##"bar.max=1;bar.value=0;li.appendChild(bar);queue.appendChild(li);"##,
    r##"const xhr=new XMLHttpRequest();"##,
    r##"xhr.open('POST','/upload?name='+encodeURIComponent(f.name));"##,
    r##"xhr.upload.onprogress=e=>{if(e.lengthComputable)bar.value=e.loaded/e.total};"##,
    r##"xhr.onloadend=()=>{bar.remove();"##,
    r##"li.appendChild(document.createTextNode(xhr.status===200?' ✓':' — failed'));"##,
    r##"done()};xhr.send(f)})),Promise.resolve())}"##,
    r##"drop.onclick=()=>input.click();"##,
    r##"input.onchange=()=>send(input.files);"##,
    r##"drop.ondragover=e=>{e.preventDefault();drop.classList.add('over')};"##,
    r##"drop.ondragleave=()=>drop.classList.remove('over');"##,
    r##"drop.ondrop=e=>{e.preventDefault();drop.classList.remove('over');"##,
    r##"send(e.dataTransfer.files)};"##,
    r##"load();"##,
    r##"</script></body></html>"##,
);

/// Serves the shelf page, streams POSTed books to the display task,
/// lists the catalog snapshot, and deletes /BOOKS entries on request.
async fn upload_server(
    stack: Stack<'static>,
    tcp_rx: &'static mut [u8],
    tcp_tx: &'static mut [u8],
    request_buf: &'static mut [u8],
    catalog: &'static mut [u8],
    catalog_len: usize,
) -> ! {
    // Staging ping-pong buffers live in the loaned heap.
    let mut pool: heapless::Vec<&'static mut [u8], 2> = heapless::Vec::new();
    let _ = pool.push(alloc::vec![0u8; 4096].leak());
    let _ = pool.push(alloc::vec![0u8; 4096].leak());
    let mut session_started = false;

    loop {
        let mut socket = TcpSocket::new(stack, &mut *tcp_rx, &mut *tcp_tx);
        socket.set_timeout(Some(Duration::from_secs(30)));
        if socket.accept(80).await.is_err() {
            continue;
        }

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
                    session_started = false;
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
        // A sleep-ended session may have died while this server was idle
        // (or between requests): consume the interrupt now so this request
        // starts a fresh session instead of feeding a writer that is gone.
        if UPLOAD_INTERRUPTS.try_take().is_some() {
            reclaim_upload_pipeline(&mut pool);
            session_started = false;
        }
        // Reborrow the pieces by index so the buffer stays usable for the
        // body bytes that arrived with the headers.
        let path_at = method_len + 1;
        let is_post = request_buf
            .get(..method_len)
            .map(|m| m == b"POST")
            .unwrap_or(false);
        let is_upload_post = is_post
            && request_buf
                .get(path_at..path_at + path_len)
                .map(|p| p.starts_with(b"/upload"))
                .unwrap_or(false);

        let path = request_buf.get(path_at..path_at + path_len).unwrap_or(b"/");
        let is_list = path.starts_with(b"/list");
        let is_delete = is_post && path.starts_with(b"/delete");
        // Test-only: the abrupt-reset arm for the install durability
        // campaign. Compiles to `false` (and the branch below to nothing)
        // outside `powercut-selftest` builds.
        let is_powercut = {
            #[cfg(feature = "powercut-selftest")]
            {
                is_post && path.starts_with(b"/test-powercut")
            }
            #[cfg(not(feature = "powercut-selftest"))]
            {
                false
            }
        };
        // Test-only: read a book back off the card and report what is in it.
        let is_digest = {
            #[cfg(feature = "powercut-selftest")]
            {
                path.starts_with(b"/test-digest")
            }
            #[cfg(not(feature = "powercut-selftest"))]
            {
                false
            }
        };

        if is_digest {
            #[cfg(feature = "powercut-selftest")]
            {
                // Read out of the immutable view first: extracting the name
                // decodes in place and takes the buffer mutably, which ends
                // this borrow.
                //
                // Ranged: `from`/`len` bound the read so no reply outlives
                // the socket's 30 s idle timeout, and `seed` carries the
                // running hash across the pieces.
                let from = crate::powercut::parse_u32(path, b"from").unwrap_or(0);
                let len = crate::powercut::parse_u32(path, b"len").unwrap_or(u32::MAX);
                let seed =
                    crate::powercut::parse_seed(path).unwrap_or(crate::powercut::DIGEST_SEED);
                let mut path_bytes = request_buf.get_mut(path_at..path_at + path_len);
                let in_books = path_bytes
                    .as_ref()
                    .map(|p| !proto::upload::has_query_param(p, b"root=1"))
                    .unwrap_or(true);
                let name = path_bytes
                    .as_mut()
                    .and_then(|p| proto::upload::raw_query_name(p))
                    .and_then(|decoded| valid_short_name(decoded));
                match name {
                    Some(name) => {
                        // Reading the card needs a session for the same
                        // reason a delete does: the storage owner holds the
                        // volume, and nothing else may open it.
                        if !session_started {
                            crate::upload::UPLOAD_SESSION_ACTIVE
                                .store(true, portable_atomic::Ordering::SeqCst);
                            STORAGE_COMMANDS.send(StorageCommand::ReceiveUpload).await;
                            session_started = true;
                        }
                        crate::powercut::DIGEST_REQUESTS
                            .send(crate::powercut::DigestRequest {
                                name,
                                in_books,
                                from,
                                len,
                                seed,
                            })
                            .await;
                        let reply = match select(
                            crate::powercut::DIGEST_RESULTS.receive(),
                            UPLOAD_INTERRUPTS.wait(),
                        )
                        .await
                        {
                            Either::First(reply) => reply,
                            Either::Second(()) => {
                                reclaim_upload_pipeline(&mut pool);
                                session_started = false;
                                None
                            }
                        };
                        let mut body = heapless::String::<64>::new();
                        match reply {
                            Some((length, read, hash)) => {
                                use core::fmt::Write as _;
                                let _ =
                                    write!(body, "size={} read={} fnv={:016x}", length, read, hash);
                            }
                            None => {
                                let _ = body.push_str("unreadable");
                            }
                        }
                        let _ = write_http_response(&mut socket, "200 OK", body.as_str()).await;
                    }
                    None => {
                        let _ =
                            write_http_response(&mut socket, "400 Bad Request", "bad name").await;
                    }
                }
            }
        } else if is_powercut {
            #[cfg(feature = "powercut-selftest")]
            {
                // `at_install_ms` defers the arm to the start of the next
                // install, which is the only way to land a cut inside a
                // window too narrow to aim at from the host.
                if let Some(ms) = crate::powercut::parse_at_install_ms(path) {
                    crate::powercut::CUT_AT_INSTALL_MS.store(ms, portable_atomic::Ordering::SeqCst);
                    let _ = write_http_response(&mut socket, "200 OK", "armed at install").await;
                } else if let Some(ms) = crate::powercut::parse_after_ms(path) {
                    crate::powercut::POWERCUT_ARM.signal(ms);
                    let _ = write_http_response(&mut socket, "200 OK", "armed").await;
                } else {
                    let _ =
                        write_http_response(&mut socket, "400 Bad Request", "bad after_ms").await;
                }
            }
        } else if is_list {
            let listing =
                core::str::from_utf8(&catalog[..catalog_len.min(catalog.len())]).unwrap_or("");
            let _ = write_http_response(&mut socket, "200 OK", listing).await;
        } else if is_delete {
            let mut path_bytes = request_buf.get_mut(path_at..path_at + path_len);
            let in_books = path_bytes
                .as_ref()
                .map(|p| !proto::upload::has_query_param(p, b"root=1"))
                .unwrap_or(true);
            let name = path_bytes
                .as_mut()
                .and_then(|p| proto::upload::raw_query_name(p))
                .and_then(|decoded| valid_short_name(decoded));
            let ok = match name {
                Some(name) => {
                    if !session_started {
                        crate::upload::UPLOAD_SESSION_ACTIVE
                            .store(true, portable_atomic::Ordering::SeqCst);
                        STORAGE_COMMANDS.send(StorageCommand::ReceiveUpload).await;
                        session_started = true;
                    }
                    UPLOAD_BEGINS
                        .send(UploadBegin {
                            name,
                            long_name: proto::upload::UploadFilename::new(),
                            delete: true,
                            in_books,
                            legacy: None,
                        })
                        .await;
                    match select(UPLOAD_RESULTS.receive(), UPLOAD_INTERRUPTS.wait()).await {
                        Either::First(ok) => ok,
                        Either::Second(()) => {
                            reclaim_upload_pipeline(&mut pool);
                            session_started = false;
                            false
                        }
                    }
                }
                None => false,
            };
            let _ = write_http_response(
                &mut socket,
                if ok { "200 OK" } else { "404 Not Found" },
                if ok { "deleted" } else { "failed" },
            )
            .await;
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
                alias: crate::upload::sanitized_name(client_name),
                identity: crate::upload::hash_identity(client_name),
            });
            let begin = UploadBegin {
                name: crate::upload::UploadName::new(),
                long_name: proto::upload::wireless_epub_filename(client_name),
                delete: false,
                in_books: true,
                legacy,
            };

            if !session_started {
                crate::upload::UPLOAD_SESSION_ACTIVE.store(true, portable_atomic::Ordering::SeqCst);
                STORAGE_COMMANDS.send(StorageCommand::ReceiveUpload).await;
                session_started = true;
            }
            let leftover_range = body_start..filled;
            let ok = match stream_book(
                &mut socket,
                request_buf,
                leftover_range,
                content_length,
                begin,
                &mut pool,
            )
            .await
            {
                StreamOutcome::Done(ok) => ok,
                StreamOutcome::Interrupted => {
                    session_started = false;
                    false
                }
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

/// The writer exited on Sleep while this task may have been mid-pipeline:
/// pull every stale message out of the upload channels, take the loaned
/// buffers back into the pool, and drop the in-flight claim. Chunk sends
/// never block (a send is always preceded by acquiring one of the two
/// buffers, so at most one buffered chunk is ever queued), which is why
/// unblocking the two receive sides is enough to cancel the producer.
fn reclaim_upload_pipeline(pool: &mut heapless::Vec<&'static mut [u8], 2>) {
    esp_println::println!("upload: session interrupted; reclaiming pipeline");
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
    crate::upload::UPLOAD_IN_FLIGHT.store(false, portable_atomic::Ordering::SeqCst);
}

/// Streams one book body to the display task; `Done(true)` when the card
/// write succeeded end to end.
async fn stream_book(
    socket: &mut TcpSocket<'_>,
    request_buf: &[u8],
    leftover: core::ops::Range<usize>,
    content_length: usize,
    begin: UploadBegin,
    pool: &mut heapless::Vec<&'static mut [u8], 2>,
) -> StreamOutcome {
    esp_println::println!("upload: '{}' {} bytes", begin.name, content_length);
    crate::upload::UPLOAD_IN_FLIGHT.store(true, portable_atomic::Ordering::SeqCst);
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
    crate::upload::UPLOAD_IN_FLIGHT.store(false, portable_atomic::Ordering::SeqCst);
    // Heap slack per upload: the join-time log plus this one bound the
    // radio buffering budget (AMPDU reorder buffers allocate under load).
    esp_println::println!(
        "upload: heap used={} free={}",
        esp_alloc::HEAP.used(),
        esp_alloc::HEAP.free()
    );
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

/// The onboarding hotspot: WPA2 AP under a PSK minted for this session
/// (joined via the QR the Wireless screen renders from it), captive
/// DHCP + DNS, and the credential form on port 80. Never returns; the
/// session ends with the reset that `SyncCommand::Exit` triggers.
#[allow(clippy::too_many_arguments)]
async fn run_portal(
    spawner: Spawner,
    controller: &mut WifiController<'static>,
    seed: u64,
    tcp_rx: &'static mut [u8],
    tcp_tx: &'static mut [u8],
    http_a: &'static mut [u8],
    http_b: &'static mut [u8],
) -> ! {
    // Scan while the controller is still unconfigured (scanning is not
    // supported once it runs AP-only); a failed scan just leaves the
    // dropdown with the manual-entry option.
    let options_len = scan_network_options(controller, http_b).await;
    let psk = mint_portal_psk(Rng::new());
    let ssid = portal_ssid();
    let mut ssid_buf = [0u8; app_core::PortalSsid::LEN];
    let ssid_text = ssid.write_into(&mut ssid_buf);
    let device = Interface::access_point();
    let config = WifiConfig::AccessPoint(
        AccessPointConfig::default()
            .with_ssid(ssid_text)
            .with_auth_method(AuthenticationMethod::Wpa2Personal)
            .with_password(psk.as_str().into()),
    );
    if controller.set_config(&config).is_err() {
        esp_println::println!("portal: ap start failed");
        SYNC_EVENTS
            .send(SyncEvent::Failed(SyncError::RadioInit))
            .await;
        park_until_exit().await;
    }

    let portal = Ipv4Address::new(PORTAL_IP[0], PORTAL_IP[1], PORTAL_IP[2], PORTAL_IP[3]);
    let mut dns_servers: heapless::Vec<Ipv4Address, 3> = heapless::Vec::new();
    let _ = dns_servers.push(portal);
    let resources: &'static mut StackResources<6> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(StackResources::new()));
    let (stack, runner) = embassy_net::new(
        device,
        NetConfig::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(portal, 24),
            gateway: Some(portal),
            dns_servers,
        }),
        resources,
        seed,
    );
    spawner.spawn(ap_net_task(runner).unwrap());

    // The PSK itself stays off the serial log; the screen is its only
    // channel.
    esp_println::println!("portal: up at 192.168.4.1 as {}", ssid_text);
    SYNC_EVENTS.send(SyncEvent::PortalUp(psk, ssid)).await;

    // Three servers share the task; Exit interrupts them with the reset.
    select(
        park_until_exit(),
        join3(
            dhcp_server(stack),
            dns_server(stack),
            credential_portal(stack, tcp_rx, tcp_tx, http_a, http_b, options_len),
        ),
    )
    .await;
    // park_until_exit resets and join3 never completes.
    unreachable!()
}

async fn dhcp_server(stack: Stack<'static>) -> ! {
    let rx_buf = alloc::vec![0u8; 1536].leak();
    let tx_buf = alloc::vec![0u8; 1536].leak();
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, rx_buf, &mut tx_meta, tx_buf);
    if socket.bind(67).is_err() {
        esp_println::println!("portal: dhcp bind failed");
        park_until_exit().await;
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
                .send_to(&reply[..reply_len], (IpAddress::v4(255, 255, 255, 255), 68))
                .await;
        }
    }
}

async fn dns_server(stack: Stack<'static>) -> ! {
    let rx_buf = alloc::vec![0u8; 1024].leak();
    let tx_buf = alloc::vec![0u8; 1024].leak();
    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, rx_buf, &mut tx_meta, tx_buf);
    if socket.bind(53).is_err() {
        esp_println::println!("portal: dns bind failed");
        park_until_exit().await;
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

async fn credential_portal(
    stack: Stack<'static>,
    tcp_rx: &'static mut [u8],
    tcp_tx: &'static mut [u8],
    request_buf: &'static mut [u8],
    network_options: &'static mut [u8],
    network_options_len: usize,
) -> ! {
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
    let mut ssid_buf = [0u8; 32];
    let mut custom_ssid_buf = [0u8; 32];
    let mut pass_buf = [0u8; 64];
    let selected = captive::form_value(request.body, "ssid", &mut ssid_buf).unwrap_or("");
    let custom =
        captive::form_value(request.body, "ssid_custom", &mut custom_ssid_buf).unwrap_or("");
    // A typed name always wins; the dropdown's empty "other" option falls
    // through to it naturally.
    let ssid = if custom.is_empty() { selected } else { custom };
    let password = captive::form_value(request.body, "pass", &mut pass_buf).unwrap_or("");
    let Some(credentials) = WifiCredentials::from_strs(ssid, password) else {
        return false;
    };
    esp_println::println!("portal: credentials captured for '{}'", credentials.ssid());
    let ssid = credentials.ssid_message();
    STORAGE_COMMANDS
        .send(StorageCommand::StoreWifiCredentials(credentials))
        .await;
    if !crate::WIFI_STORAGE_RESULTS.receive().await {
        // The card refused or corrupted the write; answering with the form
        // again (not the success page) tells the user to retry.
        esp_println::println!("portal: credential storage failed");
        return false;
    }
    send_event(SyncEvent::CredentialsSaved(ssid));
    true
}

/// Scan nearby networks into `out` as HTML-escaped `<option>` elements,
/// strongest RSSI first, deduplicated by SSID; the byte count written is
/// returned. Any failure or overflow just truncates the list — manual
/// entry remains available through the suffix's "other" option.
///
/// Memory: `scan_async` collects the results into an `alloc::vec::Vec` on
/// the Wi-Fi heap — at most 20 x 47-byte `AccessPointInfo` entries (~940
/// bytes of initialized payload; the allocation itself may be larger
/// since `Vec` growth and capacity are implementation-dependent), freed
/// on return — which is permitted here because the portal runs
/// inside the wireless session, the only phase where that allocator
/// exists. The collection never spans an await (the scan completes before
/// it is built; sorting and rendering are synchronous), so it adds
/// nothing to the caller's async state.
async fn scan_network_options(controller: &mut WifiController<'static>, out: &mut [u8]) -> usize {
    let config = ScanConfig::default().with_max(20);
    let Ok(mut networks) = controller.scan_async(&config).await else {
        esp_println::println!("portal: network scan failed; manual entry remains available");
        return 0;
    };
    networks.sort_by_key(|network| core::cmp::Reverse(network.signal_strength));
    let mut at = 0usize;
    for (index, network) in networks.iter().enumerate() {
        // `as_str()` yields only the valid UTF-8 prefix of the raw SSID
        // bytes; a truncated name would submit credentials for a different
        // network, and the string-based station config cannot represent the
        // full SSID anyway, so skip such entries.
        let ssid = network.ssid.as_str();
        if ssid.is_empty()
            || ssid.len() != network.ssid.len()
            || networks[..index].iter().any(|earlier| {
                // Compare only against entries that pass the same UTF-8
                // check; a skipped invalid SSID's valid prefix must not
                // suppress a later legitimate network with that name.
                let earlier_ssid = earlier.ssid.as_str();
                earlier_ssid.len() == earlier.ssid.len() && earlier_ssid == ssid
            })
        {
            continue;
        }
        let option_start = at;
        if !push_bytes(out, &mut at, b"<option value=\"")
            || !push_html_escaped(out, &mut at, ssid.as_bytes())
            || !push_bytes(out, &mut at, b"\">")
            || !push_html_escaped(out, &mut at, ssid.as_bytes())
            || !push_bytes(out, &mut at, b"</option>")
        {
            // Drop the partial entry so the output ends after a complete
            // `</option>` before the static suffix is appended.
            at = option_start;
            break;
        }
    }
    esp_println::println!("portal: listed {} bytes of nearby networks", at);
    at
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

/// Accepts an existing 8.3 catalog open-name verbatim: short, printable
/// ASCII, no path separators. Deletion must not invent or mangle names.
fn valid_short_name(raw: &[u8]) -> Option<crate::upload::UploadName> {
    if raw.is_empty() || raw.len() > 12 {
        return None;
    }
    let mut name = crate::upload::UploadName::new();
    for byte in raw.iter().copied() {
        if !byte.is_ascii_graphic() || byte == b'/' || byte == b'\\' {
            return None;
        }
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

async fn write_http_response(
    socket: &mut TcpSocket<'_>,
    status: &str,
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
    write_all(
        socket,
        b"\r\ncache-control: no-store\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: ",
    )
    .await?;
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

async fn park_until_exit() -> ! {
    loop {
        if let SyncCommand::Exit = SYNC_COMMANDS.receive().await {
            reset_now();
        }
    }
}

fn reset_now() -> ! {
    esp_println::println!("wifi: sync session over, resetting");
    // Let the message drain the UART before the reset takes the port.
    esp_hal::system::software_reset()
}

fn send_event(event: SyncEvent) {
    if SYNC_EVENTS.try_send(event).is_err() {
        esp_println::println!("wifi: sync event queue full");
    }
}

/// Which route a join attempt takes to the access point.
#[derive(Clone, Copy, PartialEq, Eq)]
enum JoinPlan {
    /// Straight at one AP on one channel: no scan at all, which is the
    /// whole saving. Only ever built from a hint the storage task has
    /// already matched to this network.
    Directed { bssid: [u8; 6], channel: u8 },
    /// Sweep every channel and let the radio's always-on sort-by-signal
    /// pick the strongest match. The default fast scan joins the first
    /// BSSID that answers, which on multi-AP and mesh networks can be a
    /// weak far node.
    Scan,
}

/// Why a join attempt did not associate, in the only terms the next plan
/// cares about: whether the radio is quiet enough to reconfigure.
enum JoinMiss {
    /// The radio reported a terminal result, or the attempt never got past
    /// configuring the controller. Either way nothing is in flight, so
    /// another plan can be applied on top.
    Settled,
    /// The association is still in flight past its deadline. Nothing here
    /// can cancel it (see [`RADIO_SETTLE_TIMEOUT`]), so no other plan may
    /// be applied and the attempt ends instead.
    InFlight,
}

struct Session {
    controller: WifiController<'static>,
    stack: Stack<'static>,
    /// The plan the controller is currently configured for, or `None` when
    /// the controller matches no plan — before the first join, and after
    /// any `set_config` that failed. Reconfiguring restarts the radio, so
    /// it happens only when the plan actually changes — which on a retry at
    /// the same plan is never.
    configured: Option<JoinPlan>,
}

impl Session {
    /// One join attempt: associate, wait for DHCP, report the address.
    ///
    /// A hint is tried first and the scan is the fallback, so a router that
    /// moved channel or an AP that went away costs
    /// [`DIRECTED_JOIN_TIMEOUT`] and then behaves exactly as it did before
    /// hints existed — once. `hint` is this session's working copy and is
    /// written through: a directed miss retires it, and an association
    /// replaces it, so no later Confirm pays that cost for the same
    /// already-disproven target. `stored` is the card's copy and stays put,
    /// so it is still what decides whether a write is worth making.
    async fn attempt(
        &mut self,
        ssid: &str,
        password: &str,
        hint: &mut Option<app_core::WifiApHint>,
        stored: Option<app_core::WifiApHint>,
    ) -> Result<[u8; 4], SyncError> {
        send_event(SyncEvent::Connecting);
        self.retire_association().await?;

        let directed = hint.map(|hint| JoinPlan::Directed {
            bssid: hint.bssid,
            channel: hint.channel,
        });
        let mut associated = None;
        for plan in directed.into_iter().chain(core::iter::once(JoinPlan::Scan)) {
            match self.join(ssid, password, plan).await {
                Ok(learned) => {
                    associated = Some(learned);
                    break;
                }
                Err(JoinMiss::InFlight) => return Err(SyncError::Join),
                Err(JoinMiss::Settled) => {
                    if matches!(plan, JoinPlan::Directed { .. }) {
                        // The saved route has now been proven wrong. Retiring
                        // it keeps every later Confirm in this session from
                        // spending DIRECTED_JOIN_TIMEOUT on it again before
                        // reaching the scan that does work.
                        esp_println::println!("wifi: directed join missed; falling back to scan");
                        *hint = None;
                    }
                }
            }
        }
        let Some(associated) = associated else {
            return Err(SyncError::Join);
        };
        // A channel outside 1-14 is useless to both sides: the storage record
        // refuses it, and a directed plan cannot be aimed at it.
        let learned = (1..=14).contains(&associated.channel).then_some(associated);
        // Adopt the AP the radio actually associated through before DHCP gets
        // a chance to fail, so a retry aims at the one that just answered
        // rather than reverting to a target that did not.
        if let Some(learned) = learned {
            *hint = Some(learned);
        }

        let config = with_timeout(DHCP_TIMEOUT, async {
            loop {
                if let Some(config) = self.stack.config_v4() {
                    return config;
                }
                Timer::after_millis(100).await;
            }
        })
        .await
        .map_err(|_| SyncError::Dhcp)?;
        let ip = config.address.address().octets();
        esp_println::println!("wifi: up at {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
        esp_println::println!(
            "wifi: heap used={} free={}",
            esp_alloc::HEAP.used(),
            esp_alloc::HEAP.free()
        );
        if let Some(learned) = learned {
            record_ap_hint(ssid, learned, stored).await;
        }
        send_event(SyncEvent::Connected(ip));
        Ok(ip)
    }

    /// Drop an association left over from an earlier attempt.
    ///
    /// A DHCP failure returns from [`Self::attempt`] with the station still
    /// associated, and the Confirm that follows starts a fresh attempt that
    /// wants to configure and connect from a quiet radio. Retiring the old
    /// association first is what makes that true.
    async fn retire_association(&mut self) -> Result<(), SyncError> {
        if !self.controller.is_connected() {
            return Ok(());
        }
        match with_timeout(RADIO_SETTLE_TIMEOUT, self.controller.disconnect_async()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => {
                esp_println::println!("wifi: disconnect failed: {:?}", err);
                Err(SyncError::Join)
            }
            Err(_) => {
                esp_println::println!("wifi: disconnect did not settle");
                Err(SyncError::Join)
            }
        }
    }

    /// Associate under `plan`, reporting the AP that answered.
    async fn join(
        &mut self,
        ssid: &str,
        password: &str,
        plan: JoinPlan,
    ) -> Result<app_core::WifiApHint, JoinMiss> {
        self.configure(ssid, password, plan)?;

        let deadline = match plan {
            JoinPlan::Directed { .. } => DIRECTED_JOIN_TIMEOUT,
            JoinPlan::Scan => JOIN_TIMEOUT,
        };
        let outcome = {
            let mut connect = core::pin::pin!(self.controller.connect_async());
            match select(&mut connect, Timer::after(deadline)).await {
                Either::First(result) => Some(result),
                // The deadline is this task's, not the radio's. Dropping the
                // future here would leave esp-radio inside
                // `esp_wifi_connect`, and the next plan's `set_config` would
                // then race an association still in flight. Hold the same
                // future — and with it the same event subscription, so no
                // event can slip through a gap — until the driver reports
                // one way or the other.
                Either::Second(()) => {
                    esp_println::println!("wifi: join deadline reached; settling");
                    with_timeout(RADIO_SETTLE_TIMEOUT, connect).await.ok()
                }
            }
        };

        match outcome {
            Some(Ok(info)) => Ok(app_core::WifiApHint {
                bssid: info.bssid,
                channel: info.channel,
            }),
            Some(Err(err)) => {
                esp_println::println!("wifi: join failed: {:?}", err);
                Err(JoinMiss::Settled)
            }
            None => {
                // The controller is neither connected nor idle, so it no
                // longer matches any plan this session can name. Clearing the
                // record is all that can be done: it at least makes a later
                // retry reapply the configuration rather than assume one.
                self.configured = None;
                esp_println::println!("wifi: join did not settle");
                Err(JoinMiss::InFlight)
            }
        }
    }

    /// Apply `plan` to the controller unless it is already the configured
    /// one.
    ///
    /// A `set_config` failure is a [`JoinMiss::Settled`]: esp-radio's own
    /// guard has already reset the mode to NULL and stopped the controller,
    /// so the next plan's `set_config` restarts it from a known state. The
    /// record is cleared before the call rather than only on success, so
    /// that failure cannot leave this task believing in a plan the
    /// controller no longer holds.
    fn configure(&mut self, ssid: &str, password: &str, plan: JoinPlan) -> Result<(), JoinMiss> {
        if self.configured == Some(plan) {
            return Ok(());
        }
        let auth = if password.is_empty() {
            AuthenticationMethod::None
        } else {
            AuthenticationMethod::Wpa2Personal
        };
        let station = StationConfig::default()
            .with_ssid(ssid)
            .with_password(password.into())
            .with_auth_method(auth);
        let station = match plan {
            JoinPlan::Directed { bssid, channel } => station
                .with_bssid(bssid)
                .with_channel(channel)
                // Naming the BSSID and channel is the point; a sweep
                // would throw the saving away.
                .with_scan_method(ScanMethod::Fast),
            JoinPlan::Scan => station.with_scan_method(ScanMethod::AllChannels),
        };
        self.configured = None;
        self.controller
            .set_config(&WifiConfig::Station(station))
            .map_err(|err| {
                esp_println::println!("wifi: set_config failed: {:?}", err);
                JoinMiss::Settled
            })?;
        self.configured = Some(plan);
        Ok(())
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
async fn record_ap_hint(
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
    esp_println::println!(
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
