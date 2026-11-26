"""Person-mode worker. Run from the repository root on the always-on LAN computer:

  python3 -m worker run [--record data/sessions/NAME]     capture, infer and publish
  python3 -m worker record data/sessions/NAME --plan calibration.json
  python3 -m worker train data/sessions/NAME [...]
  python3 -m worker replay data/sessions/NAME

Raw CSI, sessions and models stay under data/ on this computer. Only snapshots go to Supabase.
"""

from __future__ import annotations

import argparse
import json
import queue
import sys
import threading
import time
from pathlib import Path
from typing import Optional

from . import csi, session
from .model import STEP_S, LATENCY_S, Model, Pipeline, evaluate, replay, train
from .session import HEIGHTS, Recorder, Segment
from .supabase import Supabase, load_config

# Publish every ACTIVE_S while there are dots, otherwise every IDLE_S as a keepalive, and
# whenever the status or the empty/occupied state changes. A snapshot counts as live for three
# intervals. If the worker stops, the app keeps showing it as the last data received.
ACTIVE_S = 2.0
IDLE_S = 20.0
# How often to check the mode while the site is in Device mode.
SYNC_S = 30.0


def model_for(models: Path, layout: str) -> Optional[Model]:
    path = models / f"{layout}.json"
    return Model.load(path) if path.exists() else None


def start_capture(port: int, recorder: Optional[Recorder], live: bool = True) -> "queue.Queue":
    """Receive datagrams on a thread, recording them if asked and queueing them if `live`."""
    inbox: queue.Queue = queue.Queue(maxsize=50_000)

    def loop():
        for arrival, packet in csi.receive(port):
            if recorder:
                recorder.write(arrival, packet)
            if live:
                try:
                    inbox.put_nowait((arrival, packet))
                except queue.Full:
                    pass

    threading.Thread(target=loop, daemon=True).start()
    return inbox


def port_of(config: dict) -> int:
    return int(config.get("worker_addr", ":5005").rsplit(":", 1)[-1])


def run(args) -> None:
    config = load_config(args.config)
    client = Supabase(config)
    state = client.exchange(None)
    recorder = None
    if args.record:
        recorder = Recorder(args.record, {"layout": state["layout"], "sensors": state["sensors"],
                                          "primary_channel": state["primary_channel"],
                                          "started_at": time.time()})
    inbox = start_capture(port_of(config), recorder)
    pipeline: Optional[Pipeline] = None
    next_sync = time.time() + SYNC_S
    last_publish = 0.0
    last_state = None
    print(f"Listening on UDP {port_of(config)}; site is in {state['mode']} mode", flush=True)
    while True:
        time.sleep(STEP_S)
        packets = []
        while not inbox.empty():
            packets.append(inbox.get_nowait())
        now = time.time()
        try:
            if state["mode"] != "person":
                pipeline = None
                if now >= next_sync:
                    state = client.exchange(None)
                    next_sync = now + SYNC_S
                continue
            if pipeline is None or pipeline.layout != state["layout"]:
                pipeline = Pipeline(model_for(args.models, state["layout"]), state["layout"],
                                    state["primary_channel"], state["bounds"])
                print(f"Layout {state['layout']}: "
                      f"{'model loaded' if pipeline.model else 'calibration required'}", flush=True)
            for _, packet in packets:
                pipeline.feed(packet)
            snapshot = pipeline.step(now - LATENCY_S)
            interval = ACTIVE_S if snapshot.targets else IDLE_S
            current = (snapshot.status, bool(snapshot.targets))
            if current != last_state or now - last_publish >= interval:
                state = client.exchange(snapshot.payload(pipeline.layout, ttl=3 * interval))
                last_publish, last_state = now, current
        except (OSError, ValueError) as error:
            # Network, HTTP or reply error: keep capturing and try again on the next step.
            print(f"Supabase: {error}", file=sys.stderr, flush=True)


def plan_steps(plan: dict) -> list[tuple[str, float, float, Segment]]:
    """(instruction, seconds to get ready, seconds to record, segment) for a calibration plan."""
    marks = {mark["name"]: mark for mark in plan["marks"]}
    seconds = plan.get("step_seconds", 30)
    steps = [("Everyone, and the dog, leave the house. Recording the empty reference.",
              plan.get("leave_seconds", 60), plan.get("empty_seconds", 180),
              Segment(0, 0, "empty", "empty"))]
    how = {"standing": "stand still on", "sitting": "sit on a chair at",
           "walking": "walk slowly within a metre of"}
    for mark in plan["marks"]:
        for activity in plan.get("activities", ["standing", "sitting", "walking"]):
            steps.append((f"One person: {how.get(activity, activity)} mark '{mark['name']}'. "
                          "Everyone else stays out of the house.", 5, seconds,
                          Segment(0, 0, "person", activity, mark["name"], mark["x"], mark["y"],
                                  mark["z"] + HEIGHTS[activity])))
    for item in plan.get("dog", []):
        mark = marks[item["mark"]]
        for activity in item.get("activities", ["resting", "moving"]):
            steps.append((f"Dog {activity} at mark '{mark['name']}'. People leave the room, "
                          "or the house if they can.", 10, seconds,
                          Segment(0, 0, "dog", activity, mark["name"], mark["x"], mark["y"],
                                  mark["z"] + HEIGHTS[activity])))
    steps.append(("Everyone, and the dog, leave the house again, to check for drift.",
                  plan.get("leave_seconds", 60), plan.get("empty_seconds", 180) / 2,
                  Segment(0, 0, "empty", "empty")))
    return steps


def record(args) -> None:
    config = load_config(args.config)
    state = Supabase(config).exchange(None)
    if state["mode"] != "person":
        sys.exit("The boards only send CSI in Person mode: update public.site_config set mode = 'person';")
    plan = json.loads(args.plan.read_text())
    recorder = Recorder(args.session, {"layout": state["layout"], "sensors": state["sensors"],
                                       "primary_channel": state["primary_channel"],
                                       "started_at": time.time(), "plan": plan})
    start_capture(port_of(config), recorder, live=False)
    steps = plan_steps(plan)
    print(f"Layout {state['layout']}, {len(steps)} steps. Ctrl-C keeps the steps done so far.")
    try:
        for number, (instruction, lead, seconds, segment) in enumerate(steps, 1):
            input(f"\n[{number}/{len(steps)}] {instruction}\nPress Enter to start. ")
            print(f"Starting in {lead:.0f} s...", flush=True)
            time.sleep(lead)
            segment.start = time.time()
            print(f"Recording {seconds:.0f} s...", flush=True)
            time.sleep(seconds)
            segment.end = time.time()
            recorder.add(segment)
            print(f"Done. {recorder.count} datagrams so far.", flush=True)
    except KeyboardInterrupt:
        print("\nStopped.")
    finally:
        recorder.close()
    print(f"Saved {args.session}. Next: python3 -m worker train {args.session}")


def sessions_for(paths: list[Path]):
    """Load each session, checking they were recorded for one layout."""
    layout = None
    for path in paths:
        meta, segments = session.load(path)
        if layout not in (None, meta["layout"]):
            sys.exit(f"{path} is for layout {meta['layout']}, not {layout}")
        layout = meta["layout"]
        yield path, meta, segments


def train_cmd(args) -> None:
    windows, segments, layout = [], [], None
    for path, meta, found in sessions_for(args.sessions):
        layout = meta["layout"]
        pipeline = Pipeline(None, layout, meta.get("primary_channel"))
        windows += [(s.captured_at, s.stats) for s in replay(session.packets(path), pipeline)]
        segments += found
    model, report = train(windows, segments, layout)
    path = args.models / f"{layout}.json"
    model.save(path)
    print(json.dumps(report, indent=2))
    print(f"Saved {path}. The live worker picks it up when the layout is next loaded.")


def replay_cmd(args) -> None:
    for path, meta, segments in sessions_for(args.sessions):
        model = model_for(args.models, meta["layout"])
        if model is None:
            print(f"No model for layout {meta['layout']}; every step is calibration_required.")
        pipeline = Pipeline(model, meta["layout"], meta.get("primary_channel"))
        snapshots = list(replay(session.packets(path), pipeline))
        print(f"{path}: {len(snapshots)} steps, {pipeline.rejected} bad datagrams")
        for row in evaluate(snapshots, segments):
            print("  " + ", ".join(f"{key}: {value}" for key, value in row.items()))


def main() -> None:
    parser = argparse.ArgumentParser(prog="python3 -m worker", description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", type=Path, default=Path("cfg.toml"))
    parser.add_argument("--models", type=Path, default=Path("data/models"))
    commands = parser.add_subparsers(dest="command", required=True)
    live = commands.add_parser("run", help="capture, infer and publish snapshots")
    live.add_argument("--record", type=Path, help="also save the raw datagrams to this new session")
    calibrate = commands.add_parser("record", help="record a labelled calibration session")
    calibrate.add_argument("session", type=Path)
    calibrate.add_argument("--plan", type=Path, required=True)
    fit = commands.add_parser("train", help="train the model for the sessions' layout")
    fit.add_argument("sessions", type=Path, nargs="+")
    again = commands.add_parser("replay", help="run sessions through the live pipeline")
    again.add_argument("sessions", type=Path, nargs="+")
    args = parser.parse_args()
    {"run": run, "record": record, "train": train_cmd, "replay": replay_cmd}[args.command](args)


if __name__ == "__main__":
    main()
