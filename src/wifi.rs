//! Station connection and SNTP. Power save stays off so frames between beacons are received.

use crate::{radio, sniffer::Mac, CONFIG, UPLOAD_INTERVAL};
use anyhow::{anyhow, ensure, Result};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::peripherals::Peripherals,
    nvs::EspDefaultNvsPartition,
    sntp::{EspSntp, SyncStatus},
    sys::*,
    wifi::{AuthMethod, ClientConfiguration, Configuration, EspWifi},
};
use std::{
    thread,
    time::{Duration, Instant},
};

/// Join `cfg.toml`'s network on this chip's band and disable Wi-Fi power save.
///
/// The returned NVS partition lets the PHY store RF calibration. It is not passed
/// to the Wi-Fi driver, so the station configuration stays in RAM and comes from
/// `cfg.toml` on every boot. Hold the partition for the process lifetime.
pub fn start() -> Result<(EspWifi<'static>, EspDefaultNvsPartition)> {
    let nvs = EspDefaultNvsPartition::take()?;
    let peripherals = Peripherals::take()?;
    let mut wifi = EspWifi::new(peripherals.modem, EspSystemEventLoop::take()?, None)?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: CONFIG
            .wifi_ssid
            .try_into()
            .map_err(|_| anyhow!("SSID too long"))?,
        password: CONFIG
            .wifi_psk
            .try_into()
            .map_err(|_| anyhow!("Wi-Fi password too long"))?,
        auth_method: if CONFIG.wifi_psk.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2Personal
        },
        ..Default::default()
    }))?;
    wifi.start()?;
    radio::set_band()?;
    // Modem sleep would drop most of the frames the sniffer is here to hear.
    unsafe { esp!(esp_wifi_set_ps(wifi_ps_type_t_WIFI_PS_NONE))? };
    Ok((wifi, nvs))
}

/// Connect if the station is down. A failure is left for the next attempt.
pub fn reconnect(wifi: &mut EspWifi<'_>) {
    if !wifi.is_connected().unwrap_or(false) {
        let _ = wifi.connect();
    }
}

/// Block until SNTP has synced, or `deadline` passes.
/// The returned client must be kept alive; dropping it stops the service.
pub fn wait_for_time(wifi: &mut EspWifi<'_>, deadline: Instant) -> Result<EspSntp<'static>> {
    let mut sntp = None;
    let mut reconnect_at = Instant::now();
    loop {
        let now = Instant::now();
        ensure!(now < deadline, "Wi-Fi or SNTP timed out");
        if now >= reconnect_at {
            reconnect(wifi);
            reconnect_at = now + UPLOAD_INTERVAL;
        }
        // SNTP needs a route, so it starts only after the station is up.
        if sntp.is_none() && wifi.is_up().unwrap_or(false) {
            sntp = Some(EspSntp::new_default()?);
        }
        if sntp
            .as_ref()
            .is_some_and(|s| s.get_sync_status() == SyncStatus::Completed)
        {
            return sntp.ok_or_else(|| anyhow!("SNTP unavailable"));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// BSSID and primary channel of the associated AP, if the station is associated.
pub fn associated_ap() -> Option<(Mac, u8)> {
    let mut ap = wifi_ap_record_t::default();
    esp!(unsafe { esp_wifi_sta_get_ap_info(&mut ap) }).ok()?;
    Some((ap.bssid, ap.primary))
}
