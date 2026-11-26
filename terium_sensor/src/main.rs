//! Stay associated on the house network's primary channel and run the mode Supabase selects.
//! Device mode sniffs phones and uploads the median RSSI per device per two-second slot.
//! Person mode takes turns broadcasting probes and streams the CSI of the other boards' probes
//! to the LAN worker. Every exchange with Supabase reports the configuration this board applied.

mod csi;
mod radio;
mod rssi;
mod sniffer;
mod upload;
mod wifi;

use anyhow::{ensure, Result};
use esp_idf_svc::{sys::*, wifi::WifiDeviceId};
use sniffer::{Event, Frame, Mac};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{atomic::Ordering, mpsc::Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use upload::Mode;

/// How often one HTTPS batch is sent in Device mode. Sniffing continues between uploads.
const UPLOAD_INTERVAL: Duration = Duration::from_secs(5);
/// Person mode has no readings to upload, so the exchange is a heartbeat. The longer interval
/// leaves the Free-plan request budget to the worker's snapshots. A switch back takes up to this.
const PERSON_INTERVAL: Duration = Duration::from_secs(15);
/// After applying a new configuration, report it this soon so the app stops showing it pending.
const ACK_DELAY: Duration = Duration::from_secs(1);
/// Longest a CSI sample waits on the board before its batch is sent.
const CSI_FLUSH: Duration = Duration::from_millis(100);
/// Width of one stored sample. A slot is the unix time rounded down to this.
const SLOT: u64 = 2;
/// Most `(device, slot)` pairs held in RAM waiting for a successful upload.
const MAX_READINGS: usize = 2000;
/// Most mesh BSSIDs remembered from beacons, plus the associated AP.
const MAX_LOCAL_APS: usize = 8;
/// Most phone MACs remembered as belonging to our network.
const MAX_LOCAL_DEVICES: usize = 64;
/// How long a phone stays "on our network" after its last local frame.
const LOCAL_DEVICE_TTL: Duration = Duration::from_secs(600);
/// Reboot after this long with no successful upload, to recover a wedged stack.
const RESTART_AFTER_NO_UPLOAD: Duration = Duration::from_secs(600);

/// Build-time settings from `cfg.toml`. One image is flashed to every board.
#[toml_cfg::toml_config]
struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_psk: &'static str,
    #[default("")]
    supabase_url: &'static str,
    /// Supabase publishable key, sent as `apikey`.
    #[default("")]
    supabase_key: &'static str,
    /// Shared ingest secret, sent as `x-sensor-key`. The database stores its SHA-256.
    #[default("")]
    sensor_key: &'static str,
    /// The Person-mode worker on the LAN, as `host:port`. Empty keeps CSI on the board.
    #[default("")]
    worker_addr: &'static str,
    /// USB logging. Leave false on a board that is mounted on the wall.
    #[default(false)]
    log_serial: bool,
}

/// Seconds since the unix epoch, or 0 if the clock is unset.
fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Milliseconds since the unix epoch, or 0 if the clock is unset.
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Frames kept since the last successful upload, and which MACs count as local.
#[derive(Default)]
struct Scan {
    /// Median RSSI for each finished `(device MAC, slot timestamp)` still awaiting upload.
    readings: BTreeMap<(Mac, u64), i8>,
    /// Counts for slots whose two-second window is still open.
    open: HashMap<(Mac, u64), rssi::RssiHist>,
    /// BSSIDs advertising our SSID. The associated AP is always retained.
    aps: Vec<Mac>,
    /// Phones seen on our mesh, and when they were last seen.
    devices: HashMap<Mac, Instant>,
    /// Set from `site_config` after each successful upload. When set, foreign frames are kept too.
    all_devices: bool,
}

impl Scan {
    /// Remember an associated BSSID. When the list is full, drop its newest entry so this one fits.
    fn associate(&mut self, bssid: Mac) {
        if !self.aps.contains(&bssid) {
            if self.aps.len() == MAX_LOCAL_APS {
                self.aps.pop();
            }
            self.aps.push(bssid);
        }
    }

    /// Fold one frame into the scan. Beacons only update `aps`; they are not readings.
    fn record(&mut self, event: Event, own_mac: Mac) {
        let (mac, local, probe) = match event.frame {
            Frame::Beacon {
                bssid,
                ssid_matches,
            } => {
                if ssid_matches && self.aps.len() < MAX_LOCAL_APS && !self.aps.contains(&bssid) {
                    self.aps.push(bssid);
                }
                return;
            }
            // `local` when address 1 is one of our APs. The third value is false: this is not a probe.
            Frame::Data { mac, bssid } => (mac, self.aps.contains(&bssid), false),
            // Remember a probe that names our SSID only in the default mode. All-devices mode
            // keeps the frame below without adding the MAC to `devices`.
            Frame::Probe { mac, ssid_matches } => (mac, ssid_matches && !self.all_devices, true),
        };
        // I/G bit: multicast or broadcast. Also skip our station and our own APs.
        if mac[0] & 1 != 0 || mac == own_mac || self.aps.contains(&mac) {
            return;
        }
        if local {
            // At the cap, forget the phone that has been quiet the longest.
            if self.devices.len() == MAX_LOCAL_DEVICES && !self.devices.contains_key(&mac) {
                if let Some(oldest) = self
                    .devices
                    .iter()
                    .min_by_key(|(_, seen)| *seen)
                    .map(|(&id, _)| id)
                {
                    self.devices.remove(&oldest);
                }
            }
            self.devices.insert(mac, Instant::now());
        } else if !(self.all_devices
            || probe
                && self
                    .devices
                    .get(&mac)
                    .is_some_and(|seen| seen.elapsed() < LOCAL_DEVICE_TTL))
        {
            // Default mode drops foreign data, and probes from phones we do not remember.
            return;
        }

        // One histogram per device per open slot. A finished slot already holds its median;
        // frames that arrive after that, usually during the HTTPS call, do not open a second copy.
        // A full map still updates a slot it already holds; only new devices are skipped.
        let key = (mac, event.ts / SLOT * SLOT);
        if let Some(hist) = self.open.get_mut(&key) {
            hist.record(event.rssi);
        } else if !self.readings.contains_key(&key)
            && self.readings.len() + self.open.len() < MAX_READINGS
        {
            let mut hist = rssi::RssiHist::default();
            if hist.record(event.rssi) {
                self.open.insert(key, hist);
            }
        }
    }

    /// Replace each closed slot with its median so a failed upload keeps one byte, not the counts.
    fn collapse_finished(&mut self, cutoff: u64) {
        while let Some(key) = self
            .open
            .iter()
            .find(|(&(_, ts), _)| ts < cutoff)
            .map(|(&key, _)| key)
        {
            let Some(hist) = self.open.remove(&key) else {
                continue;
            };
            if let Some(median) = hist.median() {
                self.readings.insert(key, median);
            }
        }
    }
}

/// Person-mode state: this board's turn, the batch being filled, and where batches go.
struct Person {
    /// `(index, count)` in the probe schedule. `None` when this board is not participating.
    turn: Option<(usize, usize)>,
    next_probe_ms: u64,
    batch: csi::Batch,
    worker: Option<upload::Worker>,
    flush_at: Instant,
    /// Primary channel at the last exchange, stamped on each batch.
    channel: u8,
    samples: u32,
    failed_sends: u32,
}

impl Person {
    fn new(own: Mac, channel: u8) -> Self {
        let mut person = Self {
            turn: None,
            next_probe_ms: 0,
            batch: csi::Batch::new(own, radio::CHIP),
            worker: None,
            flush_at: Instant::now() + CSI_FLUSH,
            channel,
            samples: 0,
            failed_sends: 0,
        };
        person.connect_worker();
        person
    }

    /// Resolve the worker if that has not worked yet. Retried after each exchange.
    fn connect_worker(&mut self) {
        if self.worker.is_none() && !CONFIG.worker_addr.is_empty() {
            self.worker = upload::Worker::connect(CONFIG.worker_addr).ok();
        }
    }

    /// Take this board's turn among `peers` and keep CSI from the others. A board that is
    /// not participating (disabled or unplaced) neither transmits nor captures.
    fn set_peers(&mut self, peers: &[Mac], own: Mac) -> Result<()> {
        self.turn = peers
            .iter()
            .position(|&peer| peer == own)
            .map(|index| (index, peers.len()));
        if let Some((index, count)) = self.turn {
            self.next_probe_ms = csi::next_turn(unix_ms(), index, count);
        }
        csi::set_peers(if self.turn.is_some() { peers } else { &[] }, own)?;
        Ok(())
    }

    /// Receive samples until `deadline`, sending this board's probe on its turn and a batch
    /// at least every `CSI_FLUSH`.
    fn run_until(&mut self, samples: &Receiver<csi::Sample>, deadline: Instant) {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            let now_ms = unix_ms();
            let mut wait = deadline.min(self.flush_at).saturating_duration_since(now);
            if let Some((index, count)) = self.turn {
                if now_ms >= self.next_probe_ms {
                    // Skip a turn missed during the HTTPS call rather than talk over the next board.
                    if now_ms < self.next_probe_ms + csi::TURN_MS {
                        csi::send_probe();
                    }
                    self.next_probe_ms = csi::next_turn(now_ms + 1, index, count);
                }
                wait = wait.min(Duration::from_millis(self.next_probe_ms - now_ms));
            }
            if now >= self.flush_at {
                self.flush();
                self.flush_at = now + CSI_FLUSH;
            }
            if let Ok(sample) = samples.recv_timeout(wait) {
                self.samples += 1;
                if !self.batch.push(&sample) {
                    self.flush();
                    self.batch.push(&sample);
                }
            }
        }
    }

    fn flush(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        let channel = self.channel;
        let batch = &mut self.batch;
        if !self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.send(batch.bytes(channel)))
        {
            self.failed_sends += 1;
        }
        self.batch.clear();
    }
}

/// Apply a new configuration. On an error nothing is reported as applied, so the next exchange
/// retries. Results from the previous mode are dropped on the board as well as in Supabase.
fn apply(
    reply: &upload::Reply,
    own: Mac,
    channel: u8,
    scan: &mut Scan,
    person: &mut Option<Person>,
    frames: &Receiver<Event>,
    samples: &mut Option<Receiver<csi::Sample>>,
) -> Result<()> {
    scan.all_devices = reply.scan_all_devices;
    match reply.mode {
        Mode::Device => {
            if person.take().is_some() {
                csi::set_peers(&[], own)?;
                // Anything still queued was heard before the switch to Person mode.
                frames.try_iter().for_each(drop);
                sniffer::SNIFFING.store(true, Ordering::Relaxed);
            }
        }
        Mode::Person => {
            // CSI and ESP-NOW start on first use, so Device mode never depends on them.
            let samples = match samples {
                Some(samples) => samples,
                None => samples.insert(csi::start()?),
            };
            let peers: Vec<Mac> = reply
                .peers
                .iter()
                .filter_map(|hex| csi::parse_mac(hex))
                .collect();
            let person = person.get_or_insert_with(|| {
                samples.try_iter().for_each(drop);
                Person::new(own, channel)
            });
            person.set_peers(&peers, own)?;
            sniffer::SNIFFING.store(false, Ordering::Relaxed);
            scan.readings.clear();
            scan.open.clear();
        }
    }
    Ok(())
}

/// Connect, sync the clock, then run the selected mode until something fatal fails.
fn run() -> Result<()> {
    ensure!(
        !CONFIG.wifi_ssid.is_empty()
            && CONFIG.supabase_url.starts_with("https://")
            && !CONFIG.supabase_key.is_empty()
            && !CONFIG.sensor_key.is_empty(),
        "Check cfg.toml"
    );
    let url = format!(
        "{}/rest/v1/rpc/ingest",
        CONFIG.supabase_url.trim_end_matches('/')
    );
    let mut last_upload = Instant::now();
    // `_nvs` stays alive so the PHY can keep the calibration stored at startup.
    let (mut wifi, _nvs) = wifi::start()?;
    // Held for the process lifetime. Dropping it stops SNTP.
    let _sntp = wifi::wait_for_time(&mut wifi, last_upload + RESTART_AFTER_NO_UPLOAD)?;
    let mac = wifi.get_mac(WifiDeviceId::Sta)?;
    // Identity is the station MAC, so the same build serves every board.
    let sensor_id = format!("esp-{:012x}", upload::mac_number(mac));
    let mut scan = Scan::default();
    let mut channel = 0;
    if let Some((bssid, primary)) = wifi::associated_ap() {
        scan.associate(bssid);
        channel = primary;
    }
    // Device mode until the first reply says otherwise.
    let frames = sniffer::start()?;
    let mut samples = None;
    let mut person: Option<Person> = None;
    let mut applied_config = 0;
    let mut next_exchange = Instant::now() + UPLOAD_INTERVAL;
    let mut last_log = Instant::now();
    let mut event_count = 0u32;

    loop {
        if Instant::now() < next_exchange {
            match (&mut person, &samples) {
                (Some(person), Some(samples)) => person.run_until(samples, next_exchange),
                // Block for the next frame, but wake when the upload is due.
                _ => {
                    let remaining = next_exchange.saturating_duration_since(Instant::now());
                    if let Ok(event) = frames.recv_timeout(remaining) {
                        event_count += 1;
                        scan.record(event, mac);
                    }
                }
            }
            continue;
        }

        let now = Instant::now();
        next_exchange = now
            + if person.is_some() {
                PERSON_INTERVAL
            } else {
                UPLOAD_INTERVAL
            };
        ensure!(
            now.duration_since(last_upload) < RESTART_AFTER_NO_UPLOAD,
            "No successful upload for 10 minutes"
        );
        wifi::reconnect(&mut wifi);
        if let Some((bssid, primary)) = wifi::associated_ap() {
            scan.associate(bssid);
            channel = primary;
        }
        scan.devices
            .retain(|_, seen| seen.elapsed() < LOCAL_DEVICE_TTL);

        // Start of the slot still in progress. Older slots are closed and can be sent.
        let cutoff = unix_secs() / SLOT * SLOT;
        if person.is_none() {
            // Take queued frames into account before closing the slots.
            for event in frames.try_iter().take(sniffer::CHANNEL_SIZE) {
                event_count += 1;
                scan.record(event, mac);
            }
            scan.collapse_finished(cutoff);
        }
        if wifi.is_up().unwrap_or(false) {
            let status = upload::Status {
                sensor_id: &sensor_id,
                applied_config,
                channel,
                firmware: radio::FIRMWARE,
            };
            let finished = scan
                .readings
                .iter()
                .filter_map(|(&(mac, ts), &rssi)| (ts < cutoff).then_some((mac, ts, rssi)));
            if let Ok(reply) = upload::send(&url, &status, finished) {
                // Drop what was accepted. The open slot stays so a later frame still joins its median.
                scan.readings.retain(|&(_, ts), _| ts >= cutoff);
                last_upload = Instant::now();
                if reply.config != applied_config {
                    let result = apply(
                        &reply,
                        mac,
                        channel,
                        &mut scan,
                        &mut person,
                        &frames,
                        &mut samples,
                    );
                    match result {
                        Ok(()) => {
                            applied_config = reply.config;
                            next_exchange = Instant::now() + ACK_DELAY;
                        }
                        Err(error) if CONFIG.log_serial => {
                            eprintln!("Config {} not applied: {error}", reply.config)
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        if let Some(person) = &mut person {
            person.channel = channel;
            person.connect_worker();
        }

        let elapsed = last_log.elapsed().as_secs();
        if elapsed >= 60 {
            if CONFIG.log_serial {
                let heap = unsafe { esp_get_free_heap_size() };
                match &mut person {
                    None => println!(
                        "device: config={} events/s={} drops={} readings held={} local APs={} local devices={} channel={} free heap={}",
                        applied_config,
                        u64::from(event_count) / elapsed,
                        sniffer::DROPS.load(Ordering::Relaxed),
                        scan.readings.len() + scan.open.len(),
                        scan.aps.len(),
                        scan.devices.len(),
                        channel,
                        heap,
                    ),
                    Some(person) => println!(
                        "person: config={} samples/s={} drops={} failed sends={} turn={:?} worker={} channel={} free heap={}",
                        applied_config,
                        u64::from(person.samples) / elapsed,
                        csi::DROPS.load(Ordering::Relaxed),
                        person.failed_sends,
                        person.turn,
                        person.worker.is_some(),
                        channel,
                        heap,
                    ),
                }
            }
            event_count = 0;
            if let Some(person) = &mut person {
                person.samples = 0;
            }
            last_log = Instant::now();
        }
    }
}

/// Quiet unless `log_serial` is set. Any error from `run` restarts the chip.
fn main() {
    // Required once, or some ESP-IDF runtime patches are not linked.
    link_patches();
    if CONFIG.log_serial {
        // The sdkconfig default is silent. `*` raises every tag, up to the compiled info ceiling.
        unsafe { esp_log_level_set(c"*".as_ptr(), esp_log_level_t_ESP_LOG_INFO) };
        esp_idf_svc::log::EspLogger::initialize_default();
    }
    if let Err(error) = run() {
        if CONFIG.log_serial {
            eprintln!("{error}");
        }
        thread::sleep(Duration::from_secs(1));
        unsafe { esp_restart() };
    }
}
