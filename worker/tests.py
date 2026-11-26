"""Worker tests: python3 -m unittest worker.tests

A simulated house stands in for the boards. A body near the line between two boards weakens
that link and changes its amplitude profile, and a moving body makes it fluctuate. That is
enough to exercise calibration, training and replay end to end; real accuracy needs a real house.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

import numpy as np

from . import csi, session
from .csi import HEADER, SAMPLE, Sample, decode
from .features import Windower
from .model import Model, Pipeline, evaluate, replay, train
from .session import Recorder, Segment

BOARDS = {
    "a00000000001": (0.2, 0.2, 0.4), "a00000000002": (11.0, 0.2, 1.8),
    "a00000000003": (5.6, 9.2, 1.2), "a00000000004": (0.2, 9.2, 3.5),
    "a00000000005": (11.0, 9.2, 4.6), "a00000000006": (5.6, 0.2, 3.9),
}
MARKS = {
    "kitchen": (2.0, 2.0, 1.0), "lounge": (9.0, 2.0, 1.0), "hall": (5.6, 6.0, 1.0),
    "bedroom": (2.0, 7.0, 3.7), "study": (9.0, 7.0, 3.7), "landing": (5.6, 3.0, 3.7),
}
T0 = 1_764_000_000.0


def encode(rx: str, channel: int, samples: list[tuple]) -> bytes:
    """The firmware's batch format, for (tx, seq, unix_ms, rssi, noise, csi) tuples."""
    out = HEADER.pack(csi.MAGIC, csi.VERSION, 0, channel, len(samples), bytes.fromhex(rx))
    for tx, seq, unix_ms, rssi, noise, data in samples:
        out += SAMPLE.pack(bytes.fromhex(tx), seq, unix_ms, rssi, noise, len(data)) + data
    return out


class House:
    """Six boards exchanging probes at 10 Hz, with people standing or walking at points."""

    def __init__(self, seed: int = 1):
        self.rng = np.random.default_rng(seed)
        layout = np.random.default_rng(0)  # the same house whatever the noise seed
        self.links = [(tx, rx) for tx in BOARDS for rx in BOARDS if tx != rx]
        self.base = {link: 20 + 15 * layout.random(64) for link in self.links}
        for amplitude in self.base.values():
            amplitude[[0, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37]] = 0  # null subcarriers
        self.pattern = {link: layout.random(64) for link in self.links}
        self.seq = 0

    def effect(self, point, link) -> float:
        a, b, p = (np.array(BOARDS[link[0]]), np.array(BOARDS[link[1]]), np.array(point))
        t = np.clip(np.dot(p - a, b - a) / np.dot(b - a, b - a), 0, 1)
        return float(np.exp(-(np.linalg.norm(a + t * (b - a) - p) / 1.0) ** 2))

    def packets(self, start: float, seconds: float, people=(), all_links_drop: float = 0.0):
        """Datagrams as the worker would receive them. people: [(point, moving)]."""
        out = []
        for tick in range(int(seconds * 10)):
            t = start + tick / 10
            self.seq += 1
            for rx in BOARDS:
                samples = []
                for tx in BOARDS:
                    if tx == rx:
                        continue
                    link = (tx, rx)
                    effect = min(1.0, sum(self.effect(point, link) for point, _ in people))
                    moving = min(1.0, sum(self.effect(point, link) for point, m in people if m))
                    amplitude = self.base[link] * (1 - 0.5 * effect * self.pattern[link])
                    amplitude = amplitude * (1 + 0.01 * self.rng.normal(size=64))
                    amplitude = amplitude * (1 + 0.2 * moving * self.rng.normal(size=64))
                    amplitude = np.abs(amplitude) * self.rng.uniform(0.8, 1.2)  # receiver gain
                    phase = self.rng.uniform(0, 2 * np.pi, 64)
                    iq = np.stack([amplitude * np.sin(phase), amplitude * np.cos(phase)], axis=1)
                    data = np.clip(np.round(iq), -127, 127).astype(np.int8).tobytes()
                    distance = np.linalg.norm(np.subtract(BOARDS[tx], BOARDS[rx]))
                    rssi = -35 - 2.5 * distance - 6 * effect - all_links_drop + self.rng.normal(0, 0.5)
                    samples.append((tx, self.seq % 4096, int(t * 1000), int(rssi), -95, data))
                out.append((t + 0.05, encode(rx, 6, samples)))
        return out


def calibration(house: House, start: float = T0):
    """An empty reference then each mark standing and walking. Returns (packets, segments)."""
    packets, segments, t = [], [], start
    plan = [("empty", "empty", None, 40.0)]
    plan += [("person", activity, name, 10.0) for name in MARKS for activity in ("standing", "walking")]
    for label, activity, mark, seconds in plan:
        people = [] if mark is None else [(MARKS[mark], activity == "walking")]
        packets += house.packets(t, seconds, people)
        x, y, z = MARKS[mark] if mark else (None, None, None)
        segments.append(Segment(t, t + seconds, label, activity, mark or "", x, y, z))
        t += seconds
    return packets, segments


def trained(house: House):
    packets, segments = calibration(house)
    pipeline = Pipeline(None, "test-layout", 6)
    windows = [(s.captured_at, s.stats) for s in replay(packets, pipeline)]
    return train(windows, segments, "test-layout")


def distance(target: dict, point) -> float:
    return float(np.linalg.norm(np.subtract([target["x"], target["y"], target["z"]], point)))


class FormatTests(unittest.TestCase):
    def test_decodes_the_firmware_batch(self):
        data = bytes([253, 4] * 64)
        packet = encode("090909090909", 6, [("010203040506", 0x0102, 1_764_000_000_123, -60, -95, data)])
        # The same bytes the Rust test checks in src/csi.rs.
        self.assertEqual(packet[:8], b"TCSI\x01\x00\x06\x01")
        self.assertEqual(packet[14:22], bytes([1, 2, 3, 4, 5, 6, 2, 1]))
        [sample] = decode(packet)
        self.assertEqual(sample, Sample("090909090909", "010203040506", 0x0102, 1_764_000_000_123,
                                        -60, -95, 6, 0, data))
        with self.assertRaises(ValueError):
            decode(b"XCSI" + packet[4:])
        with self.assertRaises(ValueError):
            decode(packet[:-1])

    def test_sessions_round_trip(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "s"
            recorder = Recorder(path, {"layout": "abc"})
            recorder.write(1.5, b"one")
            recorder.write(2.5, b"two")
            recorder.add(Segment(1, 2, "person", "standing", "hall", 1, 2, 3))
            recorder.close()
            meta, segments = session.load(path)
            self.assertEqual(meta["layout"], "abc")
            self.assertEqual(segments[0].mark, "hall")
            self.assertEqual(list(session.packets(path)), [(1.5, b"one"), (2.5, b"two")])

    def test_windows_hold_only_their_samples(self):
        windower = Windower(window_s=2.0)
        data = bytes([0, 10] * 64)
        for i in range(30):
            windower.add(Sample("02", "01", 0, T0 * 1000 + i * 100, -50, -95, 6, 0, data))
        stats = windower.stats(T0 + 2.9)
        self.assertEqual(stats[("01", "02")].count, 20)
        self.assertAlmostEqual(stats[("01", "02")].motion, 0.0)
        self.assertEqual(windower.stats(T0 + 10.0), {})


class ModelTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.house = House(seed=1)
        cls.model, cls.report = trained(cls.house)

    def run_scene(self, seconds=12.0, people=(), **kwargs):
        start = T0 + 10_000
        pipeline = Pipeline(self.model, "test-layout", 6, [0, 11.2, 0, 9.4, 0, 5.3])
        snapshots = list(replay(self.house.packets(start, seconds, people, **kwargs), pipeline))
        return [s for s in snapshots if s.captured_at >= start + 2.5]

    def test_training_separates_empty_from_occupied(self):
        self.assertLessEqual(self.report["empty false alarms"], 0.05)
        self.assertGreaterEqual(self.report["occupied detected"], 0.9)
        self.assertEqual(self.report["templates"], 2 * len(MARKS))

    def test_empty_house_has_no_dots(self):
        snapshots = self.run_scene()
        quiet = [s for s in snapshots if s.status == "ready" and not s.targets]
        self.assertGreaterEqual(len(quiet), 0.9 * len(snapshots))

    def test_one_person_is_located(self):
        for mark, moving in (("hall", False), ("study", True)):
            snapshots = self.run_scene(people=[(MARKS[mark], moving)])
            errors = [min(distance(t, MARKS[mark]) for t in s.targets) for s in snapshots if s.targets]
            self.assertGreaterEqual(len(errors), 0.8 * len(snapshots), mark)
            self.assertLess(np.median(errors), 1.5, mark)

    def test_two_people_in_different_rooms(self):
        people = [(MARKS["kitchen"], False), (MARKS["study"], True)]
        snapshots = self.run_scene(people=people)
        both = [s for s in snapshots
                if all(min((distance(t, point) for t in s.targets), default=99) < 2.0
                       for point, _ in people)]
        self.assertGreaterEqual(len(both), 0.6 * len(snapshots))

    def test_unexplained_change_is_insufficient_signal(self):
        snapshots = self.run_scene(all_links_drop=10.0)
        weak = [s for s in snapshots if s.status == "insufficient_signal" and not s.targets]
        self.assertGreaterEqual(len(weak), 0.8 * len(snapshots))

    def test_without_a_model_or_data(self):
        self.assertEqual(Pipeline(None, "new-layout").step(T0).status, "calibration_required")
        self.assertEqual(Pipeline(self.model, "test-layout").step(T0).status, "insufficient_signal")

    def test_saved_model_gives_the_same_answers(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "m.json"
            self.model.save(path)
            loaded = Model.load(path)
        packets = self.house.packets(T0 + 20_000, 6, [(MARKS["lounge"], False)])
        a = [s.targets for s in replay(packets, Pipeline(self.model, "test-layout", 6))]
        b = [s.targets for s in replay(packets, Pipeline(loaded, "test-layout", 6))]
        self.assertEqual(a, b)

    def test_replay_report(self):
        packets, segments = calibration(House(seed=2), start=T0 + 30_000)
        snapshots = list(replay(packets, Pipeline(self.model, "test-layout", 6)))
        rows = {row["segment"]: row for row in evaluate(snapshots, segments)}
        self.assertEqual(rows["empty empty"]["mean dots"], 0)
        self.assertLess(rows["person standing hall"]["median error m"], 1.5)


if __name__ == "__main__":
    unittest.main()
