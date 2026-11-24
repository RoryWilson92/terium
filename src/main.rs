//! Sniff 2.4 GHz frames while staying associated, keep the median RSSI per
//! device per two-second slot, and upload finished slots to Supabase.

mod rssi;
mod sniffer;
mod upload;
mod wifi;

use anyhow::{ensure, Result};
use esp_idf_svc::{sys::*, wifi::WifiDeviceId};
use sniffer::{Event, Frame, Mac};
use std::{
    collections::{BTreeMap, HashMap},
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// How often one HTTPS batch is sent. Sniffing continues between uploads.
const UPLOAD_INTERVAL: Duration = Duration::from_secs(5);
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

/// Connect, sync the clock, then sniff and upload until something fatal fails.
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
    if let Some((bssid, _)) = wifi::associated_ap() {
        scan.associate(bssid);
    }
    let events = sniffer::enable()?;
    let mut next_upload = Instant::now() + UPLOAD_INTERVAL;
    let mut last_log = Instant::now();
    let mut event_count = 0u32;

    loop {
        let remaining = next_upload.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            // Block for the next frame, but wake when the upload is due.
            if let Ok(event) = events.recv_timeout(remaining) {
                event_count += 1;
                scan.record(event, mac);
            }
            continue;
        }

        let now = Instant::now();
        next_upload = now + UPLOAD_INTERVAL;
        ensure!(
            now.duration_since(last_upload) < RESTART_AFTER_NO_UPLOAD,
            "No successful upload for 10 minutes"
        );
        wifi::reconnect(&mut wifi);
        let mut channel = 0;
        if let Some((bssid, primary)) = wifi::associated_ap() {
            scan.associate(bssid);
            channel = primary;
        }
        scan.devices
            .retain(|_, seen| seen.elapsed() < LOCAL_DEVICE_TTL);

        // Start of the slot still in progress. Older slots are closed and can be sent.
        let cutoff = unix_secs() / SLOT * SLOT;
        // Take queued frames into account before closing the slots.
        for event in events.try_iter().take(sniffer::CHANNEL_SIZE) {
            event_count += 1;
            scan.record(event, mac);
        }
        scan.collapse_finished(cutoff);
        if wifi.is_up().unwrap_or(false) {
            let finished = scan
                .readings
                .iter()
                .filter_map(|(&(mac, ts), &rssi)| (ts < cutoff).then_some((mac, ts, rssi)));
            if let Ok(all_devices) = upload::send(&url, &sensor_id, finished) {
                // Drop what was accepted. The open slot stays so a later frame still joins its median.
                scan.readings.retain(|&(_, ts), _| ts >= cutoff);
                scan.all_devices = all_devices;
                last_upload = Instant::now();
            }
        }

        let elapsed = last_log.elapsed().as_secs();
        if elapsed >= 60 {
            if CONFIG.log_serial {
                println!(
                    "events/s={} drops={} readings held={} local APs={} local devices={} channel={} free heap={}",
                    u64::from(event_count) / elapsed,
                    sniffer::DROPS.load(Ordering::Relaxed),
                    scan.readings.len() + scan.open.len(),
                    scan.aps.len(),
                    scan.devices.len(),
                    channel,
                    unsafe { esp_get_free_heap_size() },
                );
            }
            event_count = 0;
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
