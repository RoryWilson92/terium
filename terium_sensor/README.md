# terium sensor

ESP32 boards around the house stay connected to the house Wi-Fi on its primary channel and run one of two modes, chosen in Supabase without reflashing.

- **Device mode** (the default) estimates an `(x, y, z)` position for each phone. Each board sends the median RSSI per device per two-second slot to Supabase every five seconds, and Postgres trilaterates every five seconds. Each phone's last position is kept for a week.
- **Person mode** estimates anonymous `(x, y, z)` detections for people, and the dog where the signal allows. The boards take turns broadcasting ESP-NOW probes and stream the CSI of the other boards' probes to a small Python worker on an always-on LAN computer. The worker keeps one current snapshot in Supabase, with no history.

The classic ESP32 on 2.4 GHz is the default build. The ESP32-C5 on 5 GHz is selected with a build switch, and the 2.4 GHz path will be removed once the C5s are installed.

This folder is the sensing half of [Terium](../README.md). The app is specified in [`terium_app/`](../terium_app/). Run every command in this README from this folder.

```
src/main.rs      setup, main loop, mode switching
src/radio.rs     everything that differs between the classic ESP32 and the C5
src/sniffer.rs   Device capture: promiscuous callback and 802.11 parsing
src/rssi.rs      Device processing: per-slot RSSI medians
src/csi.rs       Person capture: probe turns, CSI callback, UDP batch format
src/upload.rs    publishing: the Supabase exchange and UDP batches to the worker
src/wifi.rs      station, reconnect, SNTP
worker/          Person-mode worker: csi (capture), session (recordings), features and model (processing), supabase (publishing)
supabase/        schema migrations, local seed and pgTAP tests
```

## Local checks

Start Docker and run these. The local database uses example positions and a test key. Its API listens on port 55321. The worker needs Python 3.9 or newer with numpy.

```sh
supabase start
supabase test db --local
supabase db advisors --local --type security --level warn --fail-on warn
mkdir -p target
for module in sniffer rssi csi; do
  rustc +stable --edition=2021 --test "src/$module.rs" -o "target/$module-test" && "./target/$module-test"
done
python3 -m unittest worker.tests
```

The firmware checks are in [Build and flash](#build-and-flash). Both builds must pass `cargo build` and clippy.

## Hosted setup

Once you have created a Supabase Free project in a new organisation in London, push the schema. The example seed is for local use only. Keep `private` out of the Data API's exposed schemas.

```sh
supabase link --project-ref YOUR_PROJECT_REF
supabase migration list
supabase db push
```

`supabase migration list` should show the remote at the previous migration before you push. Pushing also renames the existing project's cron job to `terium`, and it changes the board exchange, so boards on older firmware stop uploading until they are reflashed.

For a new project, generate a shared sensor secret with `openssl rand -hex 32`. Save the secret in `cfg.toml` and compute its SHA-256 hex digest locally using `printf '%s' "$SENSOR_KEY" | shasum -a 256`, with `SENSOR_KEY` set to the generated value. Paste only the digest into the SQL editor. Set the bounds to the house envelope in metres, with z measured up from the ground-floor datum, and schedule the job.

```sql
insert into private.secret (key_sha256) values ('REPLACE_WITH_64_HEX_CHARACTERS');
insert into public.site_config (id, x_min, x_max, y_min, y_max, z_min, z_max)
values (1, 0, 11.2, 0, 9.4, 0, 5.3);
select cron.schedule('terium', '5 seconds', 'select private.compute_positions()');
```

Create a user in Supabase Auth and disable open signups so access stays with users you provision. Any signed-in user can switch the mode and place sensors.

Once the boards report their channel, set the house network's primary channel. A board reporting another channel is then left out of results in both modes. While it is null, no board is excluded.

```sql
select id, channel, firmware from public.sensors;
update public.site_config set primary_channel = 36 where id = 1;
```

### Logs and usage

Disable the following database logs before starting the sensors. The cron setting requires a brief database restart. Keep PostgREST at its default `error` level and leave pgAudit disabled. If PostgREST is logging successful requests on the hosted project, check its configuration with Supabase. PostgREST's log level is a server setting. [Supabase Postgres log settings](https://supabase.com/docs/guides/database/postgres/postgres-log-config), [PostgREST configuration](https://docs.postgrest.org/en/stable/references/configuration.html#log-level)

```sh
supabase postgres-config update --project-ref YOUR_PROJECT_REF \
  --config cron.log_statement=false --config log_checkpoints=false \
  --config log_connections=false --config log_disconnections=false
```

Run in the SQL editor:

```sql
do $$
declare
  role_name text;
begin
  foreach role_name in array array['anon', 'authenticated', 'authenticator', 'service_role', 'postgres']
  loop
    execute format('alter role %I set log_statement = none', role_name);
    execute format('alter role %I set log_duration = off', role_name);
    execute format('alter role %I set log_min_duration_statement = -1', role_name);
    execute format('alter role %I set log_lock_waits = off', role_name);
    execute format('alter role %I set log_min_messages = error', role_name);
  end loop;
end
$$;
notify pgrst, 'reload config';
```

Supabase records every Data API request in the gateway log, against the Free plan's 1 GB monthly log-ingest allowance. Requests per 30 days:

| | Boards | Worker | Total |
| --- | --- | --- | --- |
| Device mode, eight boards every 5 s | 4.15 M | 0.09 M (mode check every 30 s) | 4.24 M |
| Person mode, eight boards every 15 s | 1.38 M | 0.13 M empty (every 20 s) to 1.30 M occupied (every 2 s) | 1.5–2.7 M |

Six boards at five-second intervals were already estimated at about 1 GB, so check the usage page after 24 hours in each mode. If Device mode is tracking above the allowance, increase `UPLOAD_INTERVAL` in the firmware and reflash; seven seconds brings eight boards down to 2.96 M. Removing stored logs doesn't reduce ingest already counted. Realtime messages count against a separate quota and only while the app is subscribed. A sensor's `last_seen_at` is refreshed at most every 15 seconds, so heartbeats do not reach the app on every exchange. [Supabase log sources](https://supabase.com/docs/guides/observability/log-field-reference), [Supabase log-ingest quota](https://supabase.com/docs/guides/platform/manage-your-usage/logs-ingest)

## Build and flash

Install the Rust toolchain using [espup](https://github.com/esp-rs/espup), then source the environment file printed by `espup install`. Install `ldproxy` and `espflash` for your computer. The repository pins ESP-IDF v6.0.3 and the Rust dependencies. ESP-IDF v6 needs `python3` to be Python 3.10 or newer when it builds.

```sh
cargo +stable install --locked ldproxy espflash --target "$(rustc +stable -vV | sed -n 's/^host: //p')"
cp cfg.toml.example cfg.toml
# Edit cfg.toml: Wi-Fi details, project URL, publishable key, sensor secret and worker address.
```

The classic ESP32 is the default:

```sh
cargo build --release
cargo clippy --release -- -D warnings
firmware=target/xtensa-esp32-espidf/release
espflash flash --bootloader "$firmware/bootloader.bin" \
  --partition-table "$firmware/partition-table.bin" "$firmware/terium_sensor"
```

The ESP32-C5 build adds `.cargo/esp32c5.toml`, which sets the RISC-V target and `MCU=esp32c5`. Its build output is separate, so both builds can be kept.

```sh
cargo --config .cargo/esp32c5.toml build --release
cargo --config .cargo/esp32c5.toml clippy --release -- -D warnings
firmware=target/riscv32imac-esp-espidf/release
espflash flash --bootloader "$firmware/bootloader.bin" \
  --partition-table "$firmware/partition-table.bin" "$firmware/terium_sensor"
```

Flash the same release image to every board of a kind. Each registers as `esp-<station MAC in lowercase hex>` on its first successful request, even when it has heard no devices, and reports its firmware (`0.2.0-esp32` or `0.2.0-esp32c5`), channel and applied configuration on every exchange. The C5 joins only on 5 GHz.

For a board on USB, set `log_serial = true`, rebuild and add `--monitor` to the flash command. Every 60 seconds it prints one line. In Device mode it shows the applied config, events per second, cumulative channel drops, held readings, counts of local APs and devices, the Wi-Fi channel and free heap. In Person mode it shows the applied config, CSI samples per second, CSI queue drops, failed UDP sends, its probe turn, whether the worker address resolved, the channel and free heap. Return wall boards to `log_serial = false`. The application and ESP-IDF bootloader are quiet in that mode. The classic ESP32's ROM may still emit its own reset message, and panics print.

## Place and calibrate sensors

Start the eight-board layout with four boards per floor at varied heights. Put some on interior walls and one low in a room the dog uses. Adjust the layout using walk-throughs in both modes. Use surveyed positions in metres in one frame: x and y within the house envelope, z up from the ground-floor datum. The local seed's layout is only an example.

Check `select * from public.sensors;` in the SQL editor, then place each registered board. Positions outside the site bounds, or with only some of x, y and z set, are rejected.

```sql
update public.sensors set x = 0.2, y = 0.2, z = 0.4 where id = 'esp-REPLACE_WITH_MAC';
update public.sensors set enabled = false where id = 'esp-REPLACE_WITH_MAC';
```

Placing, moving, enabling or disabling a board creates a new layout (`site_config.layout`). Device mode uses the new positions on its next run. Person mode reports `calibration_required` until a model exists for the new layout. Moving a board back to its old position restores the old layout and its model.

### Device mode

Results use only boards that are enabled, placed, on the primary channel and heard from in the last 5 seconds. A fix needs at least four of them, spread at least 0.3 m in every direction (four boards at one height determine a point only up to its mirror image). When the geometry is not enough, nothing is published for that phone. A fix records `captured_at`, the newest two-second slot behind it, and stays until a newer fix replaces it or it is a week old.

The default scan keeps traffic from the local mesh, and probes that name the SSID or come from recently seen local phones. Set `scan_all_devices = true` to also keep other phones' data frames and probes. Board MACs are never reported as devices, and the boards' ESP-NOW probes are action frames, which the parser ignores.

To calibrate a board, hold a phone one metre from it for 30 seconds, average its readings and store the result as `ref_rssi`. Recalibrate for 5 GHz after moving to the C5s, where walls and floors attenuate more; expect `ref_rssi` to drop and `path_loss` to rise from the 2.4 GHz defaults (−45 dBm and 2.5). Adjust `path_loss` if readings at other distances warrant it.

```sql
select sensor_id, round(avg(rssi), 1) from public.observations
where device_mac = 'REPLACE_WITH_PHONE_MAC' and ts > now() - interval '30 seconds'
group by sensor_id;
update public.sensors set ref_rssi = -52, path_loss = 3.0 where id = 'esp-REPLACE_WITH_MAC';
```

Expect errors of a few metres. Floors weaken signals in a way the distance model doesn't capture, so z is the least reliable axis. A floor with fewer than three boards is rougher. The boards keep the existing per-slot medians; per-frame timing or cross-board frame matching is worth adding only if measurements show it clearly improves fixes. On 2.4 GHz, a phone connected only on 5 GHz is seen only when it probes on 2.4 GHz.

## Switch modes

```sql
update public.site_config set mode = 'person' where id = 1;
update public.site_config set mode = 'device' where id = 1;
```

A switch increments `site_config.config_version` and records `mode_since`. It deletes nothing: each phone's last fix and the last Person snapshot stay, so both are still there when the site switches back. Readings and snapshots captured before `mode_since` are not used for the new mode's results, so a late upload from before the switch never updates them. Each board applies the change on its next exchange, then reports `applied_config` about a second later. The change is pending until every online, enabled board has `applied_config = config_version`. Boards exchange every 5 seconds in Device mode and every 15 seconds in Person mode, so a switch back to Device mode takes up to 15 seconds. Changing `scan_all_devices`, `primary_channel` or the layout also increments `config_version`.

```sql
select s.id, s.applied_config = c.config_version as applied, s.channel, s.firmware,
       s.last_seen_at > now() - interval '45 seconds' as online
from public.sensors s cross join public.site_config c order by s.id;
```

## Person mode

### Worker setup

Run the worker on an always-on computer on the home LAN, with Python 3.9 or newer and numpy (`python3 -m pip install numpy`). Clone the repository there, copy the same `cfg.toml` the boards use into `terium_sensor/`, and run the worker from that folder. Set `worker_addr` to that computer's address and a UDP port, for example `192.168.1.20:5005`, before building the firmware, and reserve the address in the router's DHCP settings. Allow inbound UDP on that port in the computer's firewall. The address is a build setting rather than a database row, so the app cannot redirect CSI anywhere else. Raw CSI, recordings and models stay in `terium_sensor/data/` on that computer (gitignored). Only snapshots go to Supabase, signed with the sensor secret.

```sh
python3 -m worker run
```

Run it as a service (launchd or systemd) so it restarts with the computer. In Device mode it only checks the mode every 30 seconds. In Person mode it builds a snapshot every second and publishes every 2 seconds while there are dots, every 20 seconds otherwise, and on any change of status. Each snapshot counts as live for three publish intervals.

In Person mode each enabled, placed board broadcasts an ESP-NOW probe in its 20 ms turn, so eight boards each send about six probes a second at an OFDM rate on the primary channel. Every other board captures the L-LTF CSI of those probes, stays associated for control traffic, and sends batches to the worker at least every 100 ms. Each sample carries the receiver, the transmitter (`wifi_csi_info_t.mac`), the probe's 802.11 sequence number (`rx_seq`, which every receiver of that frame shares), the capture time, RSSI and noise floor. A board that is disabled, unplaced, or not on the primary channel is left out.

### Calibrate

Calibration is a recorded session for the current layout. It takes about an hour for eight marks and can be repeated whenever the boards move.

1. Put tape marks in each room on both floors. Measure each mark's x and y, and the height of the floor there (0 downstairs, the surveyed upstairs floor height upstairs). Copy `worker/calibration.example.json` and list the marks. Target heights are body centres: the mark's floor height plus 1.0 m standing or walking, 0.6 m sitting, 0.2 m for the dog resting and 0.3 m moving.
2. Switch to Person mode and wait until every board has applied it. Stop `worker run`, which holds the same UDP port.
3. Record. The worker reads the layout from Supabase and steps through an empty-house reference (everyone and the dog out of the house), then each mark standing, sitting and walking slowly within a metre, then the dog resting and moving at its marks, then a shorter empty reference to check for drift. Press Enter at each step.

   ```sh
   python3 -m worker record data/sessions/2026-12-01 --plan calibration.json
   ```

4. Train. The report shows the empty-house false-alarm rate, how many occupied windows were detected, and a leave-one-out position error: each segment located using only the other marks' templates.

   ```sh
   python3 -m worker train data/sessions/2026-12-01
   ```

5. Start `worker run` again. It loads `data/models/<layout>.json` for the current layout.

Detection is built in three steps. First empty versus occupied: per-link RSSI drop, change in amplitude profile and motion are compared with the empty reference. Then one target: an occupied window is matched to the templates recorded at each mark. Then several: templates are combined one at a time while each explains enough of the remaining change. When nothing explains the change, the status is `insufficient_signal` and no coordinate is published.

### Replay

Replay runs a session through the same code as the live worker and prints, per segment, the statuses, the mean number of dots and the median distance to the mark. Use it to compare changes to the processing, or a new model against an old session. To check a walk between rooms and floors, record it live and replay it.

```sh
python3 -m worker run --record data/sessions/walk-1
python3 -m worker replay data/sessions/2026-12-01 data/sessions/walk-1
```

## Mobile contract

The app signs in with Supabase Auth and uses the Data API with `apikey: <publishable key>` and `Authorization: Bearer <user access token>`. Raw observations, CSI and the sensor secret are outside its reach. All four tables below are in the `supabase_realtime` publication, so the app can subscribe to `postgres_changes` on them for live updates. Read policies apply to Realtime too. Delete events carry only the primary key.

### Configuration and sensor health

`GET /rest/v1/site_config` returns the one row:

| Field | Meaning |
| --- | --- |
| `mode` | `device` or `person` |
| `config_version` | Increments on every change the boards must apply |
| `mode_since` | Time of the last mode switch |
| `layout` | Id of the current set of enabled, placed boards and their positions |
| `primary_channel` | The house network's primary channel, or null |
| `scan_all_devices` | Device-mode scan setting |
| `x_min` … `z_max` | Site bounds in metres |

`GET /rest/v1/sensors` returns one row per board: `id`, `x`, `y`, `z` (null until placed), `enabled`, `ref_rssi`, `path_loss`, `last_seen_at`, `applied_config`, `channel` and `firmware`. For each board:

- **online**: `last_seen_at` is within 45 seconds.
- **on the primary channel**: `primary_channel` is null, or `channel` equals it.
- **participating**: enabled, placed, online and on the primary channel. Only participating boards contribute to results.
- **applied**: `applied_config = site_config.config_version`. Show a change as pending until every online, enabled board has applied it.

### Edits

```http
PATCH /rest/v1/site_config?id=eq.1        {"mode": "person"}
PATCH /rest/v1/sensors?id=eq.esp-a1b2c3d4e5f6   {"x": 3.1, "y": 4.0, "z": 1.2}
PATCH /rest/v1/sensors?id=eq.esp-a1b2c3d4e5f6   {"x": null, "y": null, "z": null}
PATCH /rest/v1/sensors?id=eq.esp-a1b2c3d4e5f6   {"enabled": false}
PATCH /rest/v1/sensors?id=eq.esp-a1b2c3d4e5f6   {"ref_rssi": -52, "path_loss": 3.0}
```

The app can change `mode`, `scan_all_devices` and `primary_channel`, and a sensor's `x`, `y`, `z`, `enabled`, `ref_rssi` and `path_loss`. Other columns are server-owned. A position outside the site bounds, or with only some coordinates set, returns HTTP 400 with the reason.

### Results

Show `device_positions` in Device mode and `person_snapshot` in Person mode, following `site_config.mode`. Switching mode deletes neither. Coordinates are metres in one frame: x and y within the site bounds, z up from the ground-floor datum. Use the server's clock (`now()` at the time of a read, or a Realtime event's timestamp) for second-scale checks such as `live_until`; the phone's clock is close enough for the Device filter.

`device_positions` has one row per phone, holding its last fix: `device_mac` (12 lowercase hex digits), `x`, `y`, `z`, `error_m` (RMS range residual), `captured_at` (the newest RSSI slot behind the fix) and `updated_at` (when it was published). A newer fix replaces the row, and the server deletes it once `captured_at` is 7 days old, in either mode. `captured_at` is when the phone was last seen: a phone heard by fewer than four boards keeps its previous fix and time. For the map's "seen within" filter, query with any threshold up to 7 days, for example the last hour with `device_positions?captured_at=gte.2026-12-01T17:04:15Z`. With Realtime, drop a phone from the map yourself once its `captured_at` falls outside the threshold, since the server only sends a delete at 7 days. Device identity is the MAC, so a phone that changes its private address appears as a new device.

`person_snapshot` has at most one row (`id = 1`), the latest Person-mode result: `status`, `layout`, `captured_at`, `published_at`, `live_until` and `targets`. Each publish replaces it, a mode switch keeps it, and no history is kept. In Person mode, always show this row. Until `live_until` it is live. After that the worker's next publish is overdue, either still being processed or because the worker has stopped, so keep showing the row as the last data received, with its `captured_at`. Right after a switch to Person mode it is the previous session's last result until the worker publishes, which takes up to about half a minute. No row means the worker has never published.

- `ready`: `targets` is the current list of detections, of any length. Empty means the house is confidently empty.
- `calibration_required`: no model exists for `layout` yet. `targets` is empty.
- `insufficient_signal`: something changed that the model cannot place. `targets` is empty.

Each target is `{"x", "y", "z", "confidence", "kind"}`. `confidence` is 0 to 1, `kind` is `person` or `dog`, and `z` is the body centre (see [Calibrate](#calibrate)). Detections are anonymous and are not tracked from one snapshot to the next.

### Examples

```json
{"device_mac": "3a7bd3e2360a", "x": 4.12, "y": 6.03, "z": 1.1, "error_m": 1.84,
 "captured_at": "2026-12-01T18:04:10+00:00", "updated_at": "2026-12-01T18:04:15.02+00:00"}
```

```json
{"id": 1, "status": "ready", "layout": "5c1d0e9a7b31",
 "captured_at": "2026-12-01T18:04:14.5+00:00", "published_at": "2026-12-01T18:04:15.1+00:00",
 "live_until": "2026-12-01T18:04:20.5+00:00",
 "targets": [{"x": 3.2, "y": 4.1, "z": 1.0, "kind": "person", "confidence": 0.8}]}
```

Check the contract without the app. With `SUPABASE_URL` and `SUPABASE_KEY` set to the project URL and publishable key:

```sh
read -r -p 'Email: ' EMAIL; read -r -s -p 'Password: ' PASSWORD; echo
TOKEN=$(curl -s "$SUPABASE_URL/auth/v1/token?grant_type=password" \
  -H "apikey: $SUPABASE_KEY" -H 'Content-Type: application/json' \
  -d "{\"email\":\"$EMAIL\",\"password\":\"$PASSWORD\"}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["access_token"])')
api() { curl -s "$SUPABASE_URL/rest/v1/$1" -H "apikey: $SUPABASE_KEY" -H "Authorization: Bearer $TOKEN" "${@:2}"; }
api 'site_config?select=mode,config_version,layout,primary_channel'
api 'sensors?select=id,enabled,channel,applied_config,last_seen_at&order=id'
api 'site_config?id=eq.1' -X PATCH -H 'Content-Type: application/json' -d '{"mode":"person"}'
api 'person_snapshot'
SINCE=$(python3 -c 'import datetime as d; print((d.datetime.now(d.timezone.utc) - d.timedelta(hours=1)).strftime("%Y-%m-%dT%H:%M:%SZ"))')
api "device_positions?select=device_mac,x,y,z,captured_at&captured_at=gte.$SINCE&order=captured_at.desc"
```

Or in the SQL editor: `select * from public.device_positions;` and `select * from public.person_snapshot;`.

## Status and known limitations

| Stage | State |
| --- | --- |
| 1. Mode control on the current boards | Schema, firmware and tests done and passing locally. Pushing to the hosted project, flashing the six boards and the switch round trip are still to do. |
| 2. Person mode on the current boards | Probes, CSI capture, recording, training, replay and publishing are built and tested against a simulated house. The walk-through on real boards is still to do. |
| 3. C5 build | `cargo build` and clippy pass for the C5. ESP-IDF v6.0.3 and esp-idf-sys 0.38.1 support the C5, its 5 GHz band mode, ESP-NOW rate control and the CSI API used here. |
| 4–6. C5 bring-up and installation | Need the boards. |
| 7. Remove the legacy path | After the C5s are installed: delete the `esp32` items in `src/radio.rs` and `cfg(esp32)` in `build.rs`, move `.cargo/esp32c5.toml` into `.cargo/config.toml`, and update this README. |

To check on the first C5s (stage 4): whether 8-bit L-LTF CSI (`lltf_bit_mode`, `acquire_csi_force_lltf`) gives 128 bytes in the classic layout, whether `rx_seq` is the sender's sequence number, whether `sig_len` includes the FCS as on the classic chip, which phone uplinks the C5 cannot capture at the router's current bandwidth (it sniffs 20 MHz, and may see only the length of some MIMO frames; healthy beacons do not prove client frames are captured), the flash size and heap of the C5-Zero (the build assumes 2 MB), and whether mesh nodes use different 5 GHz primaries in different parts of the house.

- Person-mode accuracy with a handful of low-cost boards is unproven, and z is the least reliable axis in both modes. Results depend on the calibration; furniture moved since then or a door left in a new position can look like a person until the house is recalibrated.
- Positions snap towards the calibrated marks. Areas without marks are located poorly, and two people near one mark may be reported as one.
- The dog is only detected at marks where it was recorded, and whoever handled it during calibration may be part of its templates.
- The worker trusts any CSI batch from the LAN.
- A board's probe turns and CSI pause for the second or two of each HTTPS exchange.
- If Device coverage on 5 GHz falls short because the C5 cannot capture some phone uplinks, a different receiver is a follow-up rather than part of this build.

## Privacy

The HTTPS upload sends the real MAC address as 12 lowercase hex digits, including any private address chosen by the phone. A phone may change that address, and unassociated probes often rotate it. The database deletes observations older than one hour and keeps each phone's last position for a week. With `scan_all_devices` on, that includes phones that only passed by. The gateway records request metadata separately and can record MACs that appear in query filters. The documented gateway fields do not list the JSON upload body. [Supabase log field reference](https://supabase.com/docs/guides/observability/log-field-reference)

Person detections carry no identity. CSI, calibration recordings and models stay on the LAN computer.
