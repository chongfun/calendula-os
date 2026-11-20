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
use crate::SYNC_LOANS;
use app_core::{SyncError, WifiCredentials};
use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_futures::select::{select, Either};
use embassy_net::{
    Config as NetConfig, Ipv4Address, Ipv4Cidr, Runner, Stack, StackResources, StaticConfigV4,
};
use embassy_time::{with_timeout, Duration, Timer};
use esp_hal::peripherals::WIFI;
use esp_hal::rng::Rng;
use esp_radio::wifi::{
    ap::AccessPointConfig,
    scan::ScanConfig,
    sta::{ScanMethod, StationConfig},
    AuthenticationMethodConfig, Config as WifiConfig, ControllerConfig, Interface, Password, Ssid,
    WifiController,
};
use fw_common::wifi_server::{
    exit_after_uploads, mint_portal_psk, park_until_exit, persist_frontlight_before_exit,
    push_network_option, record_ap_hint, send_event, serve_captive_dhcp, serve_captive_dns,
    serve_onboarding_http, serve_uploads, PortalHttpBuffers, SyncUdpBuffers, UploadServerBuffers,
    WifiServerHooks, DHCP_UDP_BUF_LEN, DNS_UDP_BUF_LEN, PORTAL_IP, PORTAL_SUBNET_PREFIX,
    UPLOAD_STAGING_BUF_LEN,
};
use fw_common::{
    StorageCommand, SyncCommand, SyncEvent, STORAGE_COMMANDS, SYNC_COMMANDS, SYNC_EVENTS,
};

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
    // The one-way radio session ends in a reset. Hold across all awaits.
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
            park_until_exit(reset_now).await;
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
            SyncCommand::Exit => {
                if persist_frontlight_before_exit().await {
                    reset_now();
                }
                send_event(SyncEvent::Failed(SyncError::Storage));
                park_until_exit(reset_now).await;
            }
        }
    };

    let stack = session.stack;
    esp_println::println!("upload: serving at {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
    send_event(SyncEvent::Serving(ip));
    let buffers = UploadServerBuffers {
        tcp_rx,
        tcp_tx,
        request_buf: http_a,
        catalog: http_b,
        catalog_len,
        staging: [
            alloc::vec![0u8; UPLOAD_STAGING_BUF_LEN].leak(),
            alloc::vec![0u8; UPLOAD_STAGING_BUF_LEN].leak(),
        ],
    };
    let hooks = WifiServerHooks {
        heap_stats: || (esp_alloc::HEAP.used(), esp_alloc::HEAP.free()),
    };
    select(
        exit_after_uploads(reset_now),
        serve_uploads(stack, buffers, hooks),
    )
    .await;
    unreachable!()
}

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
    let rng = Rng::new();
    let psk = mint_portal_psk(|| rng.random());
    let ssid = portal_ssid();
    let mut ssid_buf = [0u8; app_core::PortalSsid::LEN];
    let ssid_text = ssid.write_into(&mut ssid_buf);
    let device = Interface::access_point();
    let (Ok(ap_ssid), Ok(ap_pass)) = (Ssid::try_from(ssid_text), Password::try_from(psk.as_str()))
    else {
        esp_println::println!("portal: invalid ap credentials");
        SYNC_EVENTS
            .send(SyncEvent::Failed(SyncError::RadioInit))
            .await;
        park_until_exit(reset_now).await;
    };
    let config = WifiConfig::AccessPoint(
        AccessPointConfig::default()
            .with_ssid(ap_ssid)
            .with_authentication(AuthenticationMethodConfig::Wpa2Personal(ap_pass)),
    );
    if controller.set_config(&config).is_err() {
        esp_println::println!("portal: ap start failed");
        SYNC_EVENTS
            .send(SyncEvent::Failed(SyncError::RadioInit))
            .await;
        park_until_exit(reset_now).await;
    }

    let portal = Ipv4Address::new(PORTAL_IP[0], PORTAL_IP[1], PORTAL_IP[2], PORTAL_IP[3]);
    let mut dns_servers: heapless::Vec<Ipv4Address, 3> = heapless::Vec::new();
    let _ = dns_servers.push(portal);
    let resources: &'static mut StackResources<6> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(StackResources::new()));
    let (stack, runner) = embassy_net::new(
        device,
        NetConfig::ipv4_static(StaticConfigV4 {
            address: Ipv4Cidr::new(portal, PORTAL_SUBNET_PREFIX),
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

    let dhcp_buffers = SyncUdpBuffers {
        rx: alloc::vec![0u8; DHCP_UDP_BUF_LEN].leak(),
        tx: alloc::vec![0u8; DHCP_UDP_BUF_LEN].leak(),
    };
    let dns_buffers = SyncUdpBuffers {
        rx: alloc::vec![0u8; DNS_UDP_BUF_LEN].leak(),
        tx: alloc::vec![0u8; DNS_UDP_BUF_LEN].leak(),
    };
    let http_buffers = PortalHttpBuffers {
        tcp_rx,
        tcp_tx,
        request_buf: http_a,
        network_options: http_b,
        network_options_len: options_len,
    };

    // Three servers share the task; Exit interrupts them with the reset.
    select(
        park_until_exit(reset_now),
        join3(
            serve_captive_dhcp(stack, dhcp_buffers),
            serve_captive_dns(stack, dns_buffers),
            serve_onboarding_http(stack, http_buffers),
        ),
    )
    .await;
    // park_until_exit resets and join3 never completes.
    unreachable!()
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
        if !push_network_option(out, &mut at, ssid) {
            break;
        }
    }
    esp_println::println!("portal: listed {} bytes of nearby networks", at);
    at
}

fn reset_now() -> ! {
    esp_println::println!("wifi: sync session over, resetting");
    // Let the message drain the UART before the reset takes the port.
    esp_hal::system::software_reset()
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
        let sta_ssid = Ssid::try_from(ssid).map_err(|_| JoinMiss::Settled)?;
        let auth = if password.is_empty() {
            AuthenticationMethodConfig::Open
        } else {
            let sta_pass = Password::try_from(password).map_err(|_| JoinMiss::Settled)?;
            AuthenticationMethodConfig::Wpa2Personal(sta_pass)
        };
        let station = StationConfig::default()
            .with_ssid(sta_ssid)
            .with_authentication(auth);
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
