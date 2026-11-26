//! The chip and band boundary. Everything that differs between the classic ESP32 on 2.4 GHz and
//! the ESP32-C5 on 5 GHz is here. Removing the legacy build deletes the `esp32` items below and
//! the default build settings, and touches no other module.

use esp_idf_svc::sys::*;

#[cfg(not(any(esp32, esp32c5)))]
compile_error!("Build for the classic ESP32 (the default) or the ESP32-C5 (see the README)");

/// Reported to Supabase with each exchange.
#[cfg(esp32)]
pub const FIRMWARE: &str = concat!(env!("CARGO_PKG_VERSION"), "-esp32");
#[cfg(esp32c5)]
pub const FIRMWARE: &str = concat!(env!("CARGO_PKG_VERSION"), "-esp32c5");

/// Sent in each CSI batch so the worker knows which chip's subcarrier layout it holds.
#[cfg(esp32)]
pub const CHIP: u8 = 0;
#[cfg(esp32c5)]
pub const CHIP: u8 = 1;

/// Keep the station on the band of the house network's primary channel. Call after `esp_wifi_start`.
pub fn set_band() -> Result<(), EspError> {
    // The classic ESP32 has only 2.4 GHz. The C5 must not fall back to it.
    #[cfg(esp32c5)]
    esp!(unsafe { esp_wifi_set_band_mode(wifi_band_mode_t_WIFI_BAND_MODE_5G_ONLY) })?;
    Ok(())
}

/// Capture the L-LTF only, unsmoothed: 64 subcarriers as 128 signed bytes, imaginary then real.
/// Both chips then report the same shape, so the worker's feature code is shared.
#[cfg(esp32)]
pub fn csi_config() -> wifi_csi_config_t {
    wifi_csi_config_t {
        lltf_en: true,
        htltf_en: false,
        stbc_htltf2_en: false,
        ltf_merge_en: false,
        channel_filter_en: false,
        manu_scale: false,
        shift: 0,
        dump_ack_en: false,
    }
}

/// See the classic version. `force_lltf` takes the L-LTF from the HT20 probes, and 8-bit mode
/// matches the classic byte layout. Both still need checking on a C5 (README, stage 4).
#[cfg(esp32c5)]
pub fn csi_config() -> wifi_csi_config_t {
    let mut config = wifi_csi_config_t::default();
    config.set_enable(1);
    config.set_acquire_csi_legacy(1);
    config.set_acquire_csi_force_lltf(1);
    config.set_lltf_bit_mode(1);
    config
}
