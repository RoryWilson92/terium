"""Processing, part one: per-link statistics over a sliding window of CSI samples.

A link is one (transmitter, receiver) pair of boards. For each window it gets the mean RSSI,
the mean amplitude profile across subcarriers (each sample divided by its own mean, which
removes the receiver's gain changes), and motion: how much each subcarrier's amplitude varies
over the window. People and the dog change all three relative to the empty house.
"""

from __future__ import annotations

from collections import Counter, deque
from dataclasses import dataclass

import numpy as np

from .csi import Sample

WINDOW_S = 2.0
# A link with fewer samples in a window is treated as missing for that window.
MIN_SAMPLES = 5

Link = tuple[str, str]


@dataclass
class LinkStats:
    count: int
    rssi: float
    profile: np.ndarray
    motion: float


def amplitudes(csi: bytes) -> np.ndarray:
    """|H| per subcarrier from the (imaginary, real) byte pairs."""
    iq = np.frombuffer(csi[: len(csi) // 2 * 2], dtype=np.int8).astype(np.float32).reshape(-1, 2)
    return np.hypot(iq[:, 0], iq[:, 1])


class Windower:
    """Holds recent samples per link. Feed samples in time order and ask for windows in order."""

    def __init__(self, window_s: float = WINDOW_S):
        self.window_s = window_s
        self.links: dict[Link, deque[tuple[float, int, np.ndarray]]] = {}

    def add(self, sample: Sample) -> None:
        amplitude = amplitudes(sample.csi)
        mean = float(amplitude.mean()) if amplitude.size else 0.0
        if mean <= 0:
            return
        self.links.setdefault((sample.tx, sample.rx), deque()).append(
            (sample.unix_ms / 1000, sample.rssi, amplitude / mean)
        )

    def stats(self, end: float) -> dict[Link, LinkStats]:
        """Statistics for samples captured in (end - window, end]. Older samples are dropped."""
        start = end - self.window_s
        out = {}
        for link, rows in self.links.items():
            while rows and rows[0][0] <= start:
                rows.popleft()
            window = [row for row in rows if start < row[0] <= end]
            if len(window) < MIN_SAMPLES:
                continue
            # A frame in another PHY format can report a different number of subcarriers.
            size = Counter(row[2].size for row in window).most_common(1)[0][0]
            window = [row for row in window if row[2].size == size]
            if len(window) < MIN_SAMPLES:
                continue
            profiles = np.stack([row[2] for row in window])
            out[link] = LinkStats(
                count=len(window),
                rssi=float(np.mean([row[1] for row in window])),
                profile=profiles.mean(axis=0),
                motion=float(profiles.std(axis=0).mean()),
            )
        return out
