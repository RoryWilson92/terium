"""Capture: the boards' UDP CSI batches (format in src/csi.rs) and the socket that receives them."""

from __future__ import annotations

import socket
import struct
import time
from dataclasses import dataclass
from typing import Iterator

MAGIC = b"TCSI"
VERSION = 1
# Magic, version, chip, channel, sample count, receiver MAC.
HEADER = struct.Struct("<4sBBBB6s")
# Transmitter MAC, 802.11 sequence number, unix ms, RSSI, noise floor, CSI length.
SAMPLE = struct.Struct("<6sHQbbB")


@dataclass(frozen=True)
class Sample:
    """One board's CSI capture of another board's probe."""

    rx: str  # receiving board, 12 hex digits
    tx: str  # transmitting board
    seq: int  # 802.11 sequence number of the probe, shared by every receiver of it
    unix_ms: int
    rssi: int
    noise: int
    channel: int  # the receiver's primary channel
    chip: int  # 0 classic ESP32, 1 ESP32-C5
    csi: bytes  # signed (imaginary, real) byte pairs, one per subcarrier


def decode(packet: bytes) -> list[Sample]:
    """Samples in one datagram. Raises ValueError for anything that is not a version-1 batch."""
    if len(packet) < HEADER.size:
        raise ValueError("short batch")
    magic, version, chip, channel, count, rx = HEADER.unpack_from(packet)
    if magic != MAGIC or version != VERSION:
        raise ValueError("not a terium CSI batch")
    samples = []
    offset = HEADER.size
    for _ in range(count):
        if offset + SAMPLE.size > len(packet):
            raise ValueError("truncated sample")
        tx, seq, unix_ms, rssi, noise, length = SAMPLE.unpack_from(packet, offset)
        offset += SAMPLE.size
        if offset + length > len(packet):
            raise ValueError("truncated CSI")
        samples.append(
            Sample(rx.hex(), tx.hex(), seq, unix_ms, rssi, noise, channel, chip,
                   packet[offset:offset + length])
        )
        offset += length
    return samples


def receive(port: int) -> Iterator[tuple[float, bytes]]:
    """Datagrams from the boards with their arrival time, forever. The LAN is trusted."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(("0.0.0.0", port))
        while True:
            packet, _ = sock.recvfrom(2048)
            yield time.time(), packet
