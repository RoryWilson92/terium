//! 802.11 header parsing, plus the promiscuous callback when built for the ESP32.
//! `parse` allocates nothing and is tested on the host.

/// Six-byte 802.11 address.
pub type Mac = [u8; 6];

/// A frame the scan keeps. Anything else parsed from the air is discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame {
    /// Beacon. `bssid` is address 3.
    Beacon { bssid: Mac, ssid_matches: bool },
    /// Probe request. `mac` is address 2, the transmitter.
    Probe { mac: Mac, ssid_matches: bool },
    /// Station-to-AP data. `mac` is address 2 and `bssid` is address 1.
    Data { mac: Mac, bssid: Mac },
}

/// Parse a probe request, beacon, or station-to-AP data frame.
/// `ssid` is compared with information element 0. Other frames return `None`.
pub fn parse(frame: &[u8], ssid: &[u8]) -> Option<Frame> {
    // Low two bits are the protocol version and must be 0. A MAC header is 24 bytes.
    if frame.len() < 24 || frame[0] & 3 != 0 {
        return None;
    }
    // Address 2: transmitter.
    let mac = frame[10..16].try_into().ok()?;
    match frame[0] {
        // Management, subtype probe request. Tagged elements start at byte 24.
        0x40 => Some(Frame::Probe {
            mac,
            ssid_matches: has_ssid(&frame[24..], ssid),
        }),
        // Management, subtype beacon. Address 3 is the BSSID. The SSID element follows
        // the 12 fixed beacon bytes (timestamp, interval, capability), so it starts at 36.
        0x80 => Some(Frame::Beacon {
            bssid: frame[16..22].try_into().ok()?,
            ssid_matches: has_ssid(frame.get(36..)?, ssid),
        }),
        // Type is bits 2-3, and 2 means data (`fc & 0x0c == 8`). Byte 1 low bits: To DS = 1, From DS = 0.
        fc if fc & 0x0c == 8 && frame[1] & 3 == 1 => Some(Frame::Data {
            mac,
            bssid: frame[4..10].try_into().ok()?,
        }),
        _ => None,
    }
}

/// Report whether element ID 0 holds exactly `ssid`. An empty (wildcard) SSID does not match.
fn has_ssid(mut tags: &[u8], ssid: &[u8]) -> bool {
    // Each element is id, length, then that many bytes.
    while let [id, len, rest @ ..] = tags {
        let Some((value, next)) = rest.split_at_checked(*len as usize) else {
            break;
        };
        if *id == 0 {
            return !value.is_empty() && value == ssid;
        }
        tags = next;
    }
    false
}

#[cfg(target_os = "espidf")]
pub use driver::{enable, Event, CHANNEL_SIZE, DROPS};

#[cfg(target_os = "espidf")]
mod driver {
    use super::{parse, Frame};
    use crate::{unix_secs, CONFIG};
    use esp_idf_svc::sys::*;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
    use std::sync::OnceLock;

    /// One accepted frame, copied out of the driver so the callback can return.
    #[derive(Clone, Copy)]
    pub struct Event {
        pub frame: Frame,
        /// dBm, already sign-extended.
        pub rssi: i8,
        /// Unix seconds at the callback. Slotting happens later, in the main loop.
        pub ts: u64,
    }

    /// Events queued from the Wi-Fi task to the main loop.
    pub const CHANNEL_SIZE: usize = 1024;
    /// Frames dropped because that queue was full.
    pub static DROPS: AtomicU32 = AtomicU32::new(0);
    static SENDER: OnceLock<SyncSender<Event>> = OnceLock::new();

    /// Enable promiscuous RX of management and data frames, and return the queue.
    pub fn enable() -> anyhow::Result<Receiver<Event>> {
        let (tx, rx) = sync_channel(CHANNEL_SIZE);
        anyhow::ensure!(SENDER.set(tx).is_ok(), "Sniffer already enabled");
        let filter = wifi_promiscuous_filter_t {
            filter_mask: WIFI_PROMIS_FILTER_MASK_MGMT | WIFI_PROMIS_FILTER_MASK_DATA,
        };
        unsafe {
            esp!(esp_wifi_set_promiscuous_filter(&filter))?;
            esp!(esp_wifi_set_promiscuous_rx_cb(Some(receive)))?;
            esp!(esp_wifi_set_promiscuous(true))?;
        }
        Ok(rx)
    }

    /// Runs in the Wi-Fi task. Do not allocate or log here.
    unsafe extern "C" fn receive(buf: *mut c_void, kind: wifi_promiscuous_pkt_type_t) {
        if kind != wifi_promiscuous_pkt_type_t_WIFI_PKT_MGMT
            && kind != wifi_promiscuous_pkt_type_t_WIFI_PKT_DATA
        {
            return;
        }
        // ESP-IDF owns this packet for the duration of the callback. sig_len includes the FCS.
        let Some(packet) = buf.cast::<wifi_promiscuous_pkt_t>().as_ref() else {
            return;
        };
        let len = (packet.rx_ctrl.sig_len() as usize).saturating_sub(4);
        let bytes = packet.payload.as_slice(len);
        if let (Some(frame), Some(tx)) = (parse(bytes, CONFIG.wifi_ssid.as_bytes()), SENDER.get()) {
            let event = Event {
                frame,
                // The binding returns the raw byte. `as u8 as i8` sign-extends, so 196 is -60 dBm.
                rssi: packet.rx_ctrl.rssi() as u8 as i8,
                ts: unix_secs(),
            };
            if let Err(TrySendError::Full(_)) = tx.try_send(event) {
                DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Supported layouts only: short input, To-DS data, From-DS data, and a truncated SSID element.
    #[test]
    fn parses_only_supported_frames_and_bounds_checks_tags() {
        let mut frame = [0; 42];
        let mac = [2, 3, 4, 5, 6, 7];
        let bssid = [4, 5, 6, 7, 8, 9];
        frame[4..10].copy_from_slice(&bssid);
        frame[10..16].copy_from_slice(&mac);
        frame[16..22].copy_from_slice(&bssid);
        frame[0] = 0x88;
        frame[1] = 1;
        assert_eq!(parse(&frame[..23], b"mesh"), None);
        assert_eq!(parse(&frame, b"mesh"), Some(Frame::Data { mac, bssid }));
        frame[1] = 2;
        assert_eq!(parse(&frame, b"mesh"), None);
        frame[1] = 0;
        frame[0] = 0x40;
        frame[24..30].copy_from_slice(b"\0\x04mesh");
        assert_eq!(
            parse(&frame[..30], b"mesh"),
            Some(Frame::Probe {
                mac,
                ssid_matches: true
            })
        );
        assert_eq!(
            parse(&frame[..29], b"mesh"),
            Some(Frame::Probe {
                mac,
                ssid_matches: false
            })
        );
        frame[0] = 0x80;
        frame[36..].copy_from_slice(b"\0\x04mesh");
        assert_eq!(
            parse(&frame, b"mesh"),
            Some(Frame::Beacon {
                bssid,
                ssid_matches: true
            })
        );
    }
}
