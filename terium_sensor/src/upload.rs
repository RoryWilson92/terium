//! Publishing: the HTTPS exchange with `rpc/ingest`, and CSI batches to the LAN worker over UDP.

use crate::{sniffer::Mac, CONFIG};
use anyhow::{anyhow, ensure, Result};
use esp_idf_svc::{
    http::{
        client::{Configuration, EspHttpConnection},
        Method,
    },
    sys::esp_crt_bundle_attach,
};
use serde::Deserialize;
use std::{
    io::{self, BufWriter, Write},
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    time::Duration,
};

/// MAC in the low 48 bits, so `{:012x}` is the 12 hex digits the database stores.
pub fn mac_number([a, b, c, d, e, f]: Mac) -> u64 {
    u64::from_be_bytes([0, 0, a, b, c, d, e, f])
}

/// Which pipeline the board runs. Set in `site_config.mode`.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Device,
    Person,
}

/// The site configuration returned by every successful exchange.
#[derive(Deserialize)]
pub struct Reply {
    /// Version of this configuration. The board reports it back once applied.
    pub config: u64,
    pub mode: Mode,
    pub scan_all_devices: bool,
    /// Participating boards as 12 hex digits, in turn order.
    pub peers: Vec<String>,
}

/// What this board reports about itself alongside its readings.
pub struct Status<'a> {
    pub sensor_id: &'a str,
    /// The `config` of the last reply this board applied. 0 before the first.
    pub applied_config: u64,
    pub channel: u8,
    pub firmware: &'a str,
}

/// POST the status and `readings`, and return the current configuration.
/// `readings` is walked twice: once to measure `Content-Length`, then to write the body.
pub fn send(
    url: &str,
    status: &Status,
    readings: impl Iterator<Item = (Mac, u64, i8)> + Clone,
) -> Result<Reply> {
    // Count and stream the same payload to avoid holding another batch in RAM.
    let mut counter = Body(None, 0);
    write_json(&mut counter, status, readings.clone())?;
    let length = counter.1.to_string();
    let mut http = EspHttpConnection::new(&Configuration {
        timeout: Some(Duration::from_secs(4)),
        crt_bundle_attach: Some(esp_crt_bundle_attach),
        ..Default::default()
    })?;
    http.initiate_request(
        Method::Post,
        url,
        &[
            ("apikey", CONFIG.supabase_key),
            ("x-sensor-key", CONFIG.sensor_key),
            ("Content-Type", "application/json"),
            ("Content-Length", &length),
        ],
    )?;
    {
        let mut body = BufWriter::with_capacity(1024, Body(Some(&mut http), 0));
        write_json(&mut body, status, readings)?;
        body.flush()?;
    }
    http.initiate_response()?;
    ensure!(
        (200..300).contains(&http.status()),
        "Ingest returned HTTP {}",
        http.status()
    );
    // Sixteen peers fit with room to spare.
    let mut reply = [0; 1024];
    let mut len = 0;
    while len < reply.len() {
        let n = http.read(&mut reply[len..])?;
        if n == 0 {
            return Ok(serde_json::from_slice(&reply[..len])?);
        }
        len += n;
    }
    anyhow::bail!("Ingest reply too long")
}

/// `None` counts bytes for `Content-Length`. `Some` writes those bytes to the POST.
struct Body<'a>(Option<&'a mut EspHttpConnection>, usize);

impl Write for Body<'_> {
    /// Forward to the connection, or add `bytes.len()` when this pass is only counting.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(http) = &mut self.0 {
            return http.write(bytes).map_err(io::Error::other);
        }
        self.1 += bytes.len();
        Ok(bytes.len())
    }

    /// No buffer of its own. `BufWriter` calls this after `write` has already accepted the bytes.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `{"sensor_id":"...","applied_config":N,"channel":N,"firmware":"...",
/// "readings":[{"device":"<12 hex>","ts":<even unix secs>,"rssi":...}]}`.
/// The firmware string is a compile-time constant with no characters JSON must escape.
fn write_json(
    mut out: impl Write,
    status: &Status,
    readings: impl Iterator<Item = (Mac, u64, i8)>,
) -> io::Result<()> {
    write!(
        out,
        "{{\"sensor_id\":\"{}\",\"applied_config\":{},\"channel\":{},\"firmware\":\"{}\",\"readings\":[",
        status.sensor_id, status.applied_config, status.channel, status.firmware
    )?;
    for (i, (mac, ts, rssi)) in readings.enumerate() {
        if i > 0 {
            out.write_all(b",")?;
        }
        write!(
            out,
            "{{\"device\":\"{:012x}\",\"ts\":{ts},\"rssi\":{rssi}}}",
            mac_number(mac)
        )?;
    }
    out.write_all(b"]}")
}

/// The LAN worker that receives CSI batches. Raw CSI never goes to Supabase.
pub struct Worker {
    socket: UdpSocket,
    addr: SocketAddr,
}

impl Worker {
    /// Resolve `host:port` from `cfg.toml`.
    pub fn connect(addr: &str) -> Result<Self> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow!("Worker address did not resolve"))?;
        Ok(Self {
            socket: UdpSocket::bind("0.0.0.0:0")?,
            addr,
        })
    }

    /// Fire and forget. A lost datagram is a gap in that link's samples.
    pub fn send(&self, bytes: &[u8]) -> bool {
        self.socket.send_to(bytes, self.addr).is_ok()
    }
}
