"""Processing, part two: the model trained from calibration sessions, and the pipeline that
turns datagrams into snapshots. The live worker and replay both go through `Pipeline`.

Detection follows the plan's three steps:
1. Empty or occupied. Each link gives three features (RSSI drop, change in amplitude profile,
   motion), scaled so the empty house is roughly N(0, 1) and squashed with tanh so no single
   link dominates. A window whose RMS feature is above the empty reference's 99th percentile,
   plus a margin, is occupied.
2. One target. An occupied window is matched to templates: the mean features recorded at each
   tape mark and activity during calibration. Close matches of the same kind are blended.
3. Several. Templates are added one at a time while each explains enough of what is left.
   People in different rooms disturb mostly different links, so their raw features roughly
   add up, and squashing the sum caps a link that passes near both.
Evidence that no template explains is `insufficient_signal`, never a guessed coordinate.

Not built: radio tomographic imaging (no training, but weaker through walls and floors) and
learned regressors (they need far more labelled data than a short session gives).
"""

from __future__ import annotations

import json
import warnings
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable, Iterator, Optional

import numpy as np

from .csi import decode
from .features import WINDOW_S, Link, LinkStats, Windower
from .session import Segment

# Windows that start within this long after a segment starts are skipped while people settle.
SETTLE_S = 2.0
# Empty-house windows needed to train, and per link to use that link.
MIN_EMPTY_WINDOWS = 20
# Share of the model's features that must be present to say anything.
MIN_PRESENT = 0.5
# A template must explain this share of the remaining RMS to count as a target.
MIN_GAIN = 0.3
# Features are squashed towards this many empty-house standard deviations.
SATURATE = 8.0
MAX_TARGETS = 6
# Nearby templates within this distance of the best are blended into its position.
BLEND_M = 3.0
# Snapshot step, and how long to wait for batches still in flight.
STEP_S = 1.0
LATENCY_S = 0.5


def squash(values: np.ndarray) -> np.ndarray:
    return SATURATE * np.tanh(values / SATURATE)


def rms(values: np.ndarray, axis: Optional[int] = None) -> np.ndarray:
    """RMS ignoring NaN, which marks a missing link. All-NaN gives inf."""
    squares = np.square(values)
    present = (~np.isnan(squares)).sum(axis=axis)
    total = np.nansum(squares, axis=axis)
    with np.errstate(divide="ignore", invalid="ignore"):
        return np.where(present > 0, np.sqrt(total / np.maximum(present, 1)), np.inf)


@dataclass
class Template:
    kind: str  # "person" or "dog"
    activity: str
    mark: str
    x: float
    y: float
    z: float
    vector: np.ndarray


@dataclass
class Model:
    layout: str
    links: list[Link]
    rssi: np.ndarray  # empty-house mean RSSI per link
    rssi_sd: np.ndarray
    profile: list[np.ndarray]  # empty-house mean amplitude profile per link
    shape: np.ndarray  # empty-house mean distance from that profile
    shape_sd: np.ndarray
    motion: np.ndarray
    motion_sd: np.ndarray
    threshold: float = np.inf
    templates: list[Template] = field(default_factory=list)

    def vector(self, stats: dict[Link, LinkStats]) -> np.ndarray:
        """Three features per model link, NaN where the link is missing."""
        out = np.full(3 * len(self.links), np.nan)
        for i, link in enumerate(self.links):
            link_stats = stats.get(link)
            if link_stats is None or link_stats.profile.size != self.profile[i].size:
                continue
            shape = float(np.abs(link_stats.profile - self.profile[i]).mean())
            out[3 * i] = (self.rssi[i] - link_stats.rssi) / self.rssi_sd[i]
            out[3 * i + 1] = (shape - self.shape[i]) / self.shape_sd[i]
            out[3 * i + 2] = (link_stats.motion - self.motion[i]) / self.motion_sd[i]
        return out

    def infer(self, stats: dict[Link, LinkStats], templates: Optional[list[Template]] = None):
        """(status, targets) for one window. `templates` overrides the model's, for evaluation."""
        templates = self.templates if templates is None else templates
        vector = self.vector(stats)
        if np.count_nonzero(~np.isnan(vector)) < MIN_PRESENT * vector.size:
            return "insufficient_signal", []
        observed = squash(vector)
        error = float(rms(observed))
        if error <= self.threshold:
            return "ready", []
        if not templates:
            return "insufficient_signal", []
        matrix = np.stack([template.vector for template in templates])
        total = np.zeros_like(vector)
        targets = []
        # A window that matches a template still differs from it by about √2 × the empty noise.
        while len(targets) < MAX_TARGETS and error > self.threshold * np.sqrt(2):
            errors = rms(observed[None, :] - squash(total[None, :] + matrix), axis=1)
            best = int(np.argmin(errors))
            gain = 1 - float(errors[best]) / error
            if gain < MIN_GAIN:
                break
            targets.append(blend(templates, errors, best, gain))
            total = total + np.nan_to_num(matrix[best])
            error = float(errors[best])
        return ("ready", targets) if targets else ("insufficient_signal", [])

    def save(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        data = {
            "layout": self.layout,
            "links": [list(link) for link in self.links],
            "rssi": self.rssi.tolist(), "rssi_sd": self.rssi_sd.tolist(),
            "profile": [profile.tolist() for profile in self.profile],
            "shape": self.shape.tolist(), "shape_sd": self.shape_sd.tolist(),
            "motion": self.motion.tolist(), "motion_sd": self.motion_sd.tolist(),
            "threshold": self.threshold,
            "templates": [
                dict(kind=t.kind, activity=t.activity, mark=t.mark, x=t.x, y=t.y, z=t.z,
                     vector=[None if np.isnan(v) else v for v in t.vector.tolist()])
                for t in self.templates
            ],
        }
        path.write_text(json.dumps(data))

    @classmethod
    def load(cls, path: Path) -> "Model":
        data = json.loads(path.read_text())
        array = lambda key: np.array(data[key], dtype=float)  # noqa: E731
        return cls(
            layout=data["layout"],
            links=[tuple(link) for link in data["links"]],
            rssi=array("rssi"), rssi_sd=array("rssi_sd"),
            profile=[np.array(profile, dtype=float) for profile in data["profile"]],
            shape=array("shape"), shape_sd=array("shape_sd"),
            motion=array("motion"), motion_sd=array("motion_sd"),
            threshold=float(data["threshold"]),
            templates=[
                Template(t["kind"], t["activity"], t["mark"], t["x"], t["y"], t["z"],
                         np.array([np.nan if v is None else v for v in t["vector"]], dtype=float))
                for t in data["templates"]
            ],
        )


def blend(templates: list[Template], errors: np.ndarray, best: int, gain: float) -> dict:
    """The best template's position, averaged with close runners-up of the same kind nearby."""
    top = templates[best]
    near = [
        i for i in np.argsort(errors)[:3]
        if templates[i].kind == top.kind
        and np.hypot(np.hypot(templates[i].x - top.x, templates[i].y - top.y),
                     templates[i].z - top.z) <= BLEND_M
    ]
    weights = 1 / (np.square(errors[near]) + 1e-6)
    position = [
        float(np.average([getattr(templates[i], axis) for i in near], weights=weights))
        for axis in "xyz"
    ]
    x, y, z = (round(value, 2) for value in position)
    return dict(x=x, y=y, z=z, confidence=round(min(1.0, max(0.0, gain)), 2), kind=top.kind)


def labelled(windows: Iterable[tuple[float, dict]], segments: list[Segment]):
    """(segment index, stats) for windows wholly inside a segment, after it has settled."""
    for end, stats in windows:
        for index, segment in enumerate(segments):
            if segment.start + SETTLE_S <= end - WINDOW_S and end <= segment.end:
                yield index, stats
                break


def train(windows: list[tuple[float, dict]], segments: list[Segment], layout: str):
    """Fit the empty reference, threshold and templates. Returns (model, report)."""
    rows = list(labelled(windows, segments))
    empty = [stats for index, stats in rows if segments[index].label == "empty"]
    if len(empty) < MIN_EMPTY_WINDOWS:
        raise ValueError(f"{len(empty)} empty-house windows; record at least {MIN_EMPTY_WINDOWS + 10} s")
    heard = Counter(link for stats in empty for link in stats)
    links = sorted(link for link, n in heard.items() if n >= max(MIN_EMPTY_WINDOWS, len(empty) / 2))
    if not links:
        raise ValueError("No link was heard throughout the empty reference")

    columns: dict[str, list] = {key: [] for key in ("rssi", "rssi_sd", "profile", "shape",
                                                    "shape_sd", "motion", "motion_sd")}
    for link in links:
        samples = [stats[link] for stats in empty if link in stats]
        size = Counter(s.profile.size for s in samples).most_common(1)[0][0]
        samples = [s for s in samples if s.profile.size == size]
        profile = np.mean([s.profile for s in samples], axis=0)
        shapes = [float(np.abs(s.profile - profile).mean()) for s in samples]
        rssi = [s.rssi for s in samples]
        motion = [s.motion for s in samples]
        columns["rssi"].append(np.mean(rssi))
        columns["rssi_sd"].append(max(np.std(rssi), 0.5))
        columns["profile"].append(profile)
        columns["shape"].append(np.mean(shapes))
        columns["shape_sd"].append(max(np.std(shapes), 1e-3))
        columns["motion"].append(np.mean(motion))
        columns["motion_sd"].append(max(np.std(motion), 1e-3))
    model = Model(layout, links, **{
        key: value if key == "profile" else np.array(value) for key, value in columns.items()
    })

    scores = np.array([rms(squash(model.vector(stats))) for stats in empty])
    model.threshold = float(np.quantile(scores, 0.99) * 1.1)

    by_segment: dict[int, list[np.ndarray]] = {}
    for index, stats in rows:
        if segments[index].label in ("person", "dog") and segments[index].x is not None:
            by_segment.setdefault(index, []).append(model.vector(stats))
    owners = []
    for index, vectors in by_segment.items():
        segment = segments[index]
        with warnings.catch_warnings():
            # A link missing from every window of a segment stays NaN in its template.
            warnings.simplefilter("ignore", RuntimeWarning)
            vector = np.nanmean(np.stack(vectors), axis=0)
        model.templates.append(Template(segment.label, segment.activity, segment.mark,
                                        segment.x, segment.y, segment.z, vector))
        owners.append(index)

    # Leave each segment's template out and locate that segment's windows with the rest.
    errors, detected, occupied = [], 0, 0
    for index, stats in rows:
        segment = segments[index]
        if segment.label not in ("person", "dog") or segment.x is None:
            continue
        occupied += 1
        detected += bool(rms(squash(model.vector(stats))) > model.threshold)
        others = [t for t, owner in zip(model.templates, owners) if owner != index]
        _, targets = model.infer(stats, others)
        if targets:
            errors.append(min(np.hypot(np.hypot(t["x"] - segment.x, t["y"] - segment.y),
                                       t["z"] - segment.z) for t in targets))
    report = {
        "layout": layout,
        "links": len(links),
        "empty windows": len(empty),
        "threshold": round(model.threshold, 3),
        "empty false alarms": round(float(np.mean(scores > model.threshold)), 3),
        "templates": len(model.templates),
        "occupied detected": round(detected / occupied, 3) if occupied else None,
        "leave-one-out median error m": round(float(np.median(errors)), 2) if errors else None,
        "leave-one-out located": round(len(errors) / occupied, 3) if occupied else None,
    }
    return model, report


@dataclass
class Snapshot:
    status: str
    captured_at: float
    targets: list[dict]
    stats: dict = field(default_factory=dict, repr=False)

    def payload(self, layout: str, ttl: float) -> dict:
        """The body `public.publish_snapshot` expects."""
        return {"status": self.status, "layout": layout, "captured_at": self.captured_at,
                "ttl": ttl, "targets": self.targets}


class Pipeline:
    """Datagrams in, snapshots out. `model` is None when the layout has no model yet."""

    def __init__(self, model: Optional[Model], layout: str, channel: Optional[int] = None,
                 bounds: Optional[list[float]] = None):
        self.model = model
        self.layout = layout
        self.channel = channel
        self.bounds = bounds
        self.windower = Windower()
        self.rejected = 0

    def feed(self, packet: bytes) -> None:
        try:
            samples = decode(packet)
        except ValueError:
            self.rejected += 1
            return
        for sample in samples:
            # A board on another primary channel is left out of results.
            if self.channel is None or sample.channel == self.channel:
                self.windower.add(sample)

    def step(self, end: float) -> Snapshot:
        stats = self.windower.stats(end)
        if self.model is None:
            return Snapshot("calibration_required", end, [], stats)
        if not stats:
            return Snapshot("insufficient_signal", end, [], stats)
        status, targets = self.model.infer(stats)
        if self.bounds:
            x0, x1, y0, y1, z0, z1 = self.bounds
            for target in targets:
                target["x"] = round(min(max(target["x"], x0), x1), 2)
                target["y"] = round(min(max(target["y"], y0), y1), 2)
                target["z"] = round(min(max(target["z"], z0), z1), 2)
        return Snapshot(status, end, targets, stats)


def replay(packets: Iterable[tuple[float, bytes]], pipeline: Pipeline) -> Iterator[Snapshot]:
    """Step the pipeline once a second of arrival time, as the live worker does."""
    next_step = None
    for arrival, packet in packets:
        if next_step is None:
            next_step = arrival + STEP_S
        while arrival >= next_step:
            yield pipeline.step(next_step - LATENCY_S)
            next_step += STEP_S
        pipeline.feed(packet)


def evaluate(snapshots: list[Snapshot], segments: list[Segment]) -> list[dict]:
    """Per segment: how often each status came up, how many dots, and the distance to truth."""
    rows = []
    for segment in segments:
        inside = [s for s in snapshots
                  if segment.start + SETTLE_S <= s.captured_at - WINDOW_S
                  and s.captured_at <= segment.end]
        if not inside:
            continue
        statuses = Counter(s.status for s in inside)
        errors = []
        if segment.x is not None:
            for snapshot in inside:
                if snapshot.targets:
                    errors.append(min(
                        np.hypot(np.hypot(t["x"] - segment.x, t["y"] - segment.y), t["z"] - segment.z)
                        for t in snapshot.targets))
        rows.append({
            "segment": f"{segment.label} {segment.activity} {segment.mark}".strip(),
            "windows": len(inside),
            "ready": statuses["ready"],
            "insufficient": statuses["insufficient_signal"],
            "calibration": statuses["calibration_required"],
            "mean dots": round(float(np.mean([len(s.targets) for s in inside])), 2),
            "median error m": round(float(np.median(errors)), 2) if errors else None,
        })
    return rows
