"""Publishing: the worker's one exchange with Supabase, `rpc/publish_snapshot`, with the sensor key."""

from __future__ import annotations

import json
import re
import urllib.request
from pathlib import Path
from typing import Optional


def load_config(path: Path) -> dict[str, str]:
    """String settings from the boards' `cfg.toml`, so the worker shares their keys."""
    values = {}
    for line in path.read_text().splitlines():
        match = re.match(r'\s*(\w+)\s*=\s*"([^"]*)"', line)
        if match:
            values[match.group(1)] = match.group(2)
    return values


class Supabase:
    def __init__(self, config: dict[str, str]):
        url = config.get("supabase_url", "")
        # Plain HTTP only for the local stack.
        local = url.startswith(("http://127.0.0.1", "http://localhost"))
        if not (url.startswith("https://") or local) or not config.get("supabase_key") \
                or not config.get("sensor_key"):
            raise ValueError("cfg.toml needs supabase_url, supabase_key and sensor_key")
        self.endpoint = url.rstrip("/") + "/rest/v1/rpc/publish_snapshot"
        self.headers = {
            "apikey": config["supabase_key"],
            "x-sensor-key": config["sensor_key"],
            "Content-Type": "application/json",
        }

    def exchange(self, snapshot: Optional[dict] = None) -> dict:
        """Publish `snapshot` if given. Returns the mode, layout, sensors and whether it was accepted."""
        body = json.dumps({"snapshot": snapshot}).encode()
        request = urllib.request.Request(self.endpoint, data=body, headers=self.headers, method="POST")
        with urllib.request.urlopen(request, timeout=10) as response:
            return json.load(response)
