//! JSON body and the HTTPS POST to `rpc/ingest`.

use crate::{sniffer::Mac, CONFIG};
use anyhow::{ensure, Result};
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
    time::Duration,
};

/// MAC in the low 48 bits, so `{:012x}` is the 12 hex digits the database stores.
pub fn mac_number([a, b, c, d, e, f]: Mac) -> u64 {
    u64::from_be_bytes([0, 0, a, b, c, d, e, f])
}

/// Successful ingest body. Applied to the scan on the next frames.
#[derive(Deserialize)]
struct Reply {
    scan_all_devices: bool,
}

/// POST `readings` and return the server's `scan_all_devices` flag.
/// `readings` is walked twice: once to measure `Content-Length`, then to write the body.
pub fn send(
    url: &str,
    sensor_id: &str,
    readings: impl Iterator<Item = (Mac, u64, i8)> + Clone,
) -> Result<bool> {
    // Count and stream the same payload to avoid holding another batch in RAM.
    let mut counter = Body(None, 0);
    write_json(&mut counter, sensor_id, readings.clone())?;
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
        write_json(&mut body, sensor_id, readings)?;
        body.flush()?;
    }
    http.initiate_response()?;
    ensure!(
        (200..300).contains(&http.status()),
        "Ingest returned HTTP {}",
        http.status()
    );
    let mut reply = [0; 64];
    let mut len = 0;
    while len < reply.len() {
        let n = http.read(&mut reply[len..])?;
        if n == 0 {
            return Ok(serde_json::from_slice::<Reply>(&reply[..len])?.scan_all_devices);
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

/// `{"sensor_id":"...","readings":[{"device":"<12 hex>","ts":<even unix secs>,"rssi":...}]}`.
fn write_json(
    mut out: impl Write,
    sensor_id: &str,
    readings: impl Iterator<Item = (Mac, u64, i8)>,
) -> io::Result<()> {
    write!(out, "{{\"sensor_id\":\"{sensor_id}\",\"readings\":[")?;
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
