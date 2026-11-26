//! Person-mode capture. Boards take turns broadcasting an ESP-NOW probe, keep the CSI of the
//! other boards' probes, and send it to the LAN worker in UDP batches. The turn schedule, MAC
//! parsing and batch format are pure and tested on the host; the driver code is ESP-only.
//!
//! A sample is linked to its probe by the transmitter MAC and the 802.11 sequence number that
//! ESP-IDF reports as `rx_seq`, which every receiver of that frame sees. Turns follow SNTP time,
//! so they overlap by a few milliseconds at worst; the Wi-Fi MAC's carrier sense covers that.

/// Six-byte 802.11 address.
pub type Mac = [u8; 6];

/// CSI bytes kept per sample: the L-LTF, 64 subcarriers × (imaginary, real).
pub const CSI_LEN: usize = 128;
/// Length of one transmit turn. Eight boards give each a probe every 160 ms.
pub const TURN_MS: u64 = 20;
/// Batch format version. The worker rejects other versions.
pub const VERSION: u8 = 1;
/// Largest UDP payload, under a 1500-byte MTU.
pub const MAX_BATCH: usize = 1400;
/// `b"TCSI"`, version, chip, channel, sample count, then the receiver's MAC.
const HEADER: usize = 14;
/// Transmitter, sequence number, unix ms, RSSI, noise floor, CSI length.
const SAMPLE_HEADER: usize = 19;

/// One CSI capture of another board's probe, copied out of the driver.
#[derive(Clone, Copy)]
pub struct Sample {
    /// The probe's transmitter.
    pub tx: Mac,
    /// 802.11 sequence number of the probe, the same at every receiver.
    pub seq: u16,
    /// Capture time in unix milliseconds.
    pub unix_ms: u64,
    /// dBm, already sign-extended.
    pub rssi: i8,
    /// Noise floor in dBm.
    pub noise: i8,
    /// Valid bytes at the start of `csi`.
    pub len: u8,
    pub csi: [i8; CSI_LEN],
}

/// Start of turn `index` of `count` at or after `now_ms`. Every board computes the same schedule.
pub fn next_turn(now_ms: u64, index: usize, count: usize) -> u64 {
    let period = TURN_MS * count.max(1) as u64;
    let start = now_ms / period * period + TURN_MS * index as u64;
    if start >= now_ms {
        start
    } else {
        start + period
    }
}

/// Twelve hex digits, as Supabase sends a peer, to a MAC.
pub fn parse_mac(hex: &str) -> Option<Mac> {
    if hex.len() != 12 {
        return None;
    }
    let number = u64::from_str_radix(hex, 16).ok()?;
    number.to_be_bytes()[2..].try_into().ok()
}

/// One UDP datagram of samples from this board. Little-endian throughout.
pub struct Batch {
    buf: Vec<u8>,
}

impl Batch {
    pub fn new(rx: Mac, chip: u8) -> Self {
        let mut buf = Vec::with_capacity(MAX_BATCH);
        buf.extend_from_slice(b"TCSI");
        buf.extend_from_slice(&[VERSION, chip, 0, 0]);
        buf.extend_from_slice(&rx);
        Self { buf }
    }

    pub fn is_empty(&self) -> bool {
        self.buf[7] == 0
    }

    /// Append a sample. False when it does not fit, so send the batch and push again.
    pub fn push(&mut self, sample: &Sample) -> bool {
        let len = usize::from(sample.len).min(CSI_LEN);
        if self.buf.len() + SAMPLE_HEADER + len > MAX_BATCH || self.buf[7] == u8::MAX {
            return false;
        }
        self.buf.extend_from_slice(&sample.tx);
        self.buf.extend_from_slice(&sample.seq.to_le_bytes());
        self.buf.extend_from_slice(&sample.unix_ms.to_le_bytes());
        self.buf
            .extend_from_slice(&[sample.rssi as u8, sample.noise as u8, len as u8]);
        self.buf
            .extend(sample.csi[..len].iter().map(|&value| value as u8));
        self.buf[7] += 1;
        true
    }

    /// The datagram, stamped with the receiver's current primary channel.
    pub fn bytes(&mut self, channel: u8) -> &[u8] {
        self.buf[6] = channel;
        &self.buf
    }

    pub fn clear(&mut self) {
        self.buf.truncate(HEADER);
        self.buf[7] = 0;
    }
}

#[cfg(target_os = "espidf")]
pub use driver::{send_probe, set_peers, start, DROPS};

#[cfg(target_os = "espidf")]
mod driver {
    use super::{Mac, Sample, CSI_LEN};
    use crate::{radio, unix_ms};
    use esp_idf_svc::sys::*;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
    use std::sync::OnceLock;

    /// Samples queued from the Wi-Fi task. About three seconds at eight boards.
    const CHANNEL_SIZE: usize = 128;
    /// More than the boards in the house; extra peers are ignored.
    const MAX_PEERS: usize = 16;
    const BROADCAST: Mac = [0xff; 6];

    /// Samples dropped because the queue was full.
    pub static DROPS: AtomicU32 = AtomicU32::new(0);
    static SENDER: OnceLock<SyncSender<Sample>> = OnceLock::new();
    /// Transmitters whose probes are kept, read by the callback without a lock. The classic
    /// ESP32 has no 64-bit atomics, so each MAC is two words; see `words`. A zero high word is
    /// an empty slot. A probe seen mid-update is at worst kept or dropped by mistake.
    static PEERS: [[AtomicU32; 2]; MAX_PEERS] =
        [const { [AtomicU32::new(0), AtomicU32::new(0)] }; MAX_PEERS];
    static PROBES: AtomicU32 = AtomicU32::new(0);

    /// The first two bytes with a presence bit, and the last four.
    fn words([a, b, c, d, e, f]: Mac) -> [u32; 2] {
        [
            0x1_0000 | u32::from(a) << 8 | u32::from(b),
            u32::from_be_bytes([c, d, e, f]),
        ]
    }

    /// Register the CSI callback and the ESP-NOW broadcast peer, with CSI off. Call after the
    /// sniffer has enabled promiscuous mode, which the CSI configuration needs.
    pub fn start() -> anyhow::Result<Receiver<Sample>> {
        anyhow::ensure!(SENDER.get().is_none(), "CSI already started");
        let peer = esp_now_peer_info_t {
            peer_addr: BROADCAST,
            // 0 follows the station's channel.
            channel: 0,
            ifidx: wifi_interface_t_WIFI_IF_STA,
            encrypt: false,
            ..Default::default()
        };
        // An OFDM rate on both bands, because 802.11b frames carry no L-LTF.
        let mut rate = esp_now_rate_config_t {
            phymode: wifi_phy_mode_t_WIFI_PHY_MODE_HT20,
            rate: wifi_phy_rate_t_WIFI_PHY_RATE_MCS0_LGI,
            ersu: false,
            dcm: false,
        };
        unsafe {
            esp!(esp_wifi_set_csi_config(&radio::csi_config()))?;
            esp!(esp_wifi_set_csi_rx_cb(Some(receive), std::ptr::null_mut()))?;
            esp!(esp_now_init())?;
            esp!(esp_now_add_peer(&peer))?;
            esp!(esp_now_set_peer_rate_config(BROADCAST.as_ptr(), &mut rate))?;
        }
        // Claimed last, so a failure above can be retried on the next exchange. Until then
        // the callback has nowhere to send and returns at once.
        let (tx, rx) = sync_channel(CHANNEL_SIZE);
        anyhow::ensure!(SENDER.set(tx).is_ok(), "CSI already started");
        Ok(rx)
    }

    /// Keep CSI from `peers` other than `own`. An empty list turns CSI off.
    pub fn set_peers(peers: &[Mac], own: Mac) -> Result<(), EspError> {
        let mut kept = peers.iter().filter(|&&mac| mac != own);
        for slot in &PEERS {
            let [high, low] = kept.next().map_or([0, 0], |&mac| words(mac));
            slot[0].store(high, Ordering::Relaxed);
            slot[1].store(low, Ordering::Relaxed);
        }
        esp!(unsafe { esp_wifi_set_csi(!peers.is_empty()) })
    }

    /// Broadcast one probe. A full ESP-NOW queue skips this turn.
    pub fn send_probe() {
        let count = PROBES.fetch_add(1, Ordering::Relaxed);
        let mut body = *b"TPRB\0\0\0\0";
        body[4..].copy_from_slice(&count.to_le_bytes());
        let _ = unsafe { esp_now_send(BROADCAST.as_ptr(), body.as_ptr(), body.len()) };
    }

    /// Runs in the Wi-Fi task. Do not allocate or log here.
    unsafe extern "C" fn receive(_ctx: *mut c_void, info: *mut wifi_csi_info_t) {
        // ESP-IDF owns `info` and `buf` for the duration of the callback.
        let Some(info) = info.as_ref() else {
            return;
        };
        // Probes are broadcast. A peer's unicast traffic may use other PHY formats.
        let [high, low] = words(info.mac);
        if info.dmac != BROADCAST
            || info.buf.is_null()
            || !PEERS.iter().any(|peer| {
                peer[0].load(Ordering::Relaxed) == high && peer[1].load(Ordering::Relaxed) == low
            })
        {
            return;
        }
        let len = usize::from(info.len).min(CSI_LEN);
        let mut csi = [0; CSI_LEN];
        csi[..len].copy_from_slice(std::slice::from_raw_parts(info.buf, len));
        // The hardware can leave the first four bytes invalid.
        if info.first_word_invalid {
            csi[..len.min(4)].fill(0);
        }
        let sample = Sample {
            tx: info.mac,
            seq: info.rx_seq,
            unix_ms: unix_ms(),
            // As in the sniffer: the binding returns the raw byte, so sign-extend it.
            rssi: info.rx_ctrl.rssi() as u8 as i8,
            noise: info.rx_ctrl.noise_floor() as u8 as i8,
            len: len as u8,
            csi,
        };
        if let Some(sender) = SENDER.get() {
            if let Err(TrySendError::Full(_)) = sender.try_send(sample) {
                DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_follow_the_shared_schedule() {
        // Three boards: turns at 0, 20 and 40 ms of each 60 ms period.
        assert_eq!(next_turn(0, 0, 3), 0);
        assert_eq!(next_turn(1, 0, 3), 60);
        assert_eq!(next_turn(1, 2, 3), 40);
        assert_eq!(next_turn(41, 2, 3), 100);
        assert_eq!(next_turn(1_000_000_007, 1, 8), 1_000_000_020);
    }

    #[test]
    fn parses_peer_macs() {
        assert_eq!(
            parse_mac("a1b2c3d4e5f6"),
            Some([0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6])
        );
        assert_eq!(parse_mac("a1b2c3d4e5f"), None);
        assert_eq!(parse_mac("down-1000000"), None);
    }

    #[test]
    fn batches_fit_one_datagram() {
        let mut sample = Sample {
            tx: [1, 2, 3, 4, 5, 6],
            seq: 0x0102,
            unix_ms: 1_764_000_000_123,
            rssi: -60,
            noise: -95,
            len: CSI_LEN as u8,
            csi: [0; CSI_LEN],
        };
        sample.csi[0] = -3;
        let mut batch = Batch::new([9; 6], 0);
        assert!(batch.is_empty());
        let mut count = 0;
        while batch.push(&sample) {
            count += 1;
        }
        assert_eq!(count, 9);
        let bytes = batch.bytes(6);
        assert!(bytes.len() <= MAX_BATCH);
        assert_eq!(&bytes[..8], b"TCSI\x01\x00\x06\x09");
        assert_eq!(&bytes[8..14], &[9; 6]);
        assert_eq!(&bytes[14..22], &[1, 2, 3, 4, 5, 6, 2, 1]);
        assert_eq!(bytes[30..33], [-60i8 as u8, -95i8 as u8, 128]);
        assert_eq!(bytes[33], -3i8 as u8);
        batch.clear();
        assert!(batch.is_empty());
        assert_eq!(batch.bytes(6).len(), HEADER);
    }
}
