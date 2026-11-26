"""Recorded sessions: the raw datagrams plus labelled time segments, so a recording can be
replayed through exactly the code that runs live.

A session is a directory holding `packets.bin` (arrival time, length, datagram, repeated) and
`session.json` (layout, sensors and segments).
"""

from __future__ import annotations

import json
import struct
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import BinaryIO, Iterator, Optional

RECORD = struct.Struct("<dH")

# Body-centre height above the floor mark for each activity, in metres. A target's z is the
# mark's floor height plus this, so standing and sitting at one mark are two heights.
HEIGHTS = {"standing": 1.0, "walking": 1.0, "sitting": 0.6, "resting": 0.2, "moving": 0.3}


@dataclass
class Segment:
    """A timed stretch with one known state. label is 'empty', 'person' or 'dog'."""

    start: float
    end: float
    label: str
    activity: str = ""
    mark: str = ""
    x: Optional[float] = None
    y: Optional[float] = None
    z: Optional[float] = None


class Recorder:
    """Appends datagrams as they arrive and writes the metadata on close."""

    def __init__(self, path: Path, meta: dict):
        path.mkdir(parents=True, exist_ok=False)
        self.path = path
        self.meta = dict(meta, segments=[])
        self.count = 0
        self._file: BinaryIO = open(path / "packets.bin", "wb")

    def write(self, arrival: float, packet: bytes) -> None:
        self._file.write(RECORD.pack(arrival, len(packet)))
        self._file.write(packet)
        self.count += 1

    def add(self, segment: Segment) -> None:
        self.meta["segments"].append(asdict(segment))
        self.save_meta()

    def save_meta(self) -> None:
        (self.path / "session.json").write_text(json.dumps(self.meta, indent=2))

    def close(self) -> None:
        self._file.close()
        self.save_meta()


def load(path: Path) -> tuple[dict, list[Segment]]:
    meta = json.loads((path / "session.json").read_text())
    return meta, [Segment(**segment) for segment in meta.get("segments", [])]


def packets(path: Path) -> Iterator[tuple[float, bytes]]:
    """Datagrams in arrival order. A record cut short by a crash ends the session."""
    with open(path / "packets.bin", "rb") as file:
        while True:
            head = file.read(RECORD.size)
            if len(head) < RECORD.size:
                return
            arrival, length = RECORD.unpack(head)
            packet = file.read(length)
            if len(packet) < length:
                return
            yield arrival, packet
