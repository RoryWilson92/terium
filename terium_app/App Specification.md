# Terium app specification

This is the specification for the Terium app: a Flutter app for Android, macOS and the web that shows where phones and people are in one house, using the data that the boards and worker in [`terium_sensor/`](../terium_sensor/) publish to Supabase.

**This is a hobby project for one house and its owner. It is not a product and will not be shipped to the public.** Keep the app small, readable and modular. This document sets out what the app does, what it doesn't do, the constraints, the backend contract and when the work is done. How to build it is up to the implementing agent. Choose the simplest approach that meets each requirement. If you reject an alternative, note it in a code comment rather than building it.

Production practices are out of scope. That includes large test suites, CI pipelines, crash reporting, analytics, app store releases, feature flags, localisation, accessibility audits and support for old OS versions.

## Background

ESP32 boards around a two-storey house run one of two modes, which is chosen in Supabase:

- **Device mode** estimates an `(x, y, z)` position for each phone from Wi-Fi signal strength. Postgres recalculates the positions every 5 seconds and keeps each phone's last fix for a week.
- **Person mode** estimates anonymous `(x, y, z)` detections of people (and the dog, where the signal allows) from Wi-Fi CSI. A worker on the home LAN produces these detections and keeps a single current snapshot in Supabase. No history is kept.

The app reads the results, switches the mode and edits sensor placement. It does all of this through the Supabase Data API and Realtime, signed in as a user. The [sensor README](../terium_sensor/README.md) has a "Mobile contract" section, and the migrations in `terium_sensor/supabase/migrations/` are the backend's source of truth. If this spec and those ever disagree, the README and migrations win. Any backend change the app needs goes into `terium_sensor/` as a new migration with a pgTAP test, and the README contract is updated to match.

At the time of writing, the backend is built and passes its tests locally, but not all of it has been pushed to the hosted project yet (see the sensor README's status table). Develop against the local stack first.

## What the app does

- Signs in a user who has been set up by hand, using Supabase Auth.
- Shows the house as a 3D volume. Sensors appear in both modes. Phones appear in Device mode and detections appear in Person mode.
- Switches the whole house between Device mode and Person mode.
- Lists the current phones or detections in a bottom drawer. Selecting one focuses the 3D view on it.
- Edits sensor placement and calibration, as well as the site-wide scan settings.
- Shows system health: when each sensor was last heard from, whether a config change is still pending, and how fresh the positioning data and person data are.
- Keeps a few app-only settings locally.

## What it doesn't do

- It doesn't calculate positions or detections. It only displays what the backend publishes.
- It doesn't read raw observations, CSI, calibration recordings or models. These are out of the app's reach by design.
- It doesn't run calibration, train models, flash boards or change the router. Those tasks use the worker CLI and the sensor README's procedures.
- It shows no history, playback, trails or heatmaps. The backend keeps only each phone's last fix and a single Person snapshot.
- It doesn't identify people or follow a detection over time. Detections are anonymous and aren't tracked from one snapshot to the next.
- It has no sign-up, password reset, user management or roles.
- It has no push notifications, background tracking, home-screen widgets or background refresh.
- It has no offline mode beyond keeping the last data it received on screen while the connection is down.
- It draws no floor plans, rooms or walls. The house is drawn as its bounding volume.
- It doesn't edit site bounds, layout ids, config versions or anything a board reports. The server owns these.
- It doesn't register or delete sensors. Boards register themselves, and the app has no delete permission.
- It doesn't give phones or sensors friendly names, because the backend has nowhere to store them. This could be added later.
- It doesn't support more than one house.

## Platform and stack

- Flutter and Dart on the stable channel, using versions that are current when the work starts.
- **The targets are Android, macOS and the web.** iOS, Windows and Linux don't need to work. The layout must work from an Android phone in portrait up to a resizable macOS or browser window. Tablet-specific layouts aren't needed.
- Use Riverpod for state, go_router for navigation and `supabase_flutter` for Auth, the Data API and Realtime. Settings are stored in a standard local key-value store that works on all three targets. Icons are Material Symbols or Material Icons.
- Every package must support all three targets. This matters most for 3D rendering, because some Flutter 3D packages only work with Impeller and don't run on the web.
- The macOS build needs outgoing network access in its entitlements, or every request fails.
- How the web build is hosted isn't covered here. Running it locally with `flutter run` is enough.
- Add other packages only when they clearly save code. Prefer well-maintained and widely used ones, and keep the dependency list short. Code generation (for example `riverpod_generator`, `freezed` or `json_serializable`) is allowed if it makes the code smaller, but isn't required.
- The 3D scene is simple: one box, a handful of sensors and at most a few hundred markers. Choose the lightest rendering approach that meets the requirements under [3D view](#3d-view). A game engine or a web view isn't needed.

## Constraints

### Code

- Use one Flutter project in `terium_app/`, next to this spec. It shares nothing with the firmware or worker beyond the API contract below.
- Give each part one job: backend access, the 3D view, the drawer and focus card, the menu pages, and settings. Keep all backend access in one place so the whole contract can be seen in one module.
- The backend is the source of truth for the mode, the sensors and the results. Only the auth session and the app settings are stored locally.
- `flutter analyze` must pass with the standard Flutter lints.
- A few unit tests for pure logic are welcome, such as the sensor states, liveness checks and the "seen within" filter. Widget tests, golden tests, integration tests, CI and coverage targets aren't wanted.

### Theme

- Keep every design token in one file: the light and dark colour schemes, typography including the font family, corner radii, the spacing and padding scale, elevations, icon and marker sizes, and animation durations. The same file also holds the marker colours (the device palette, the person colour, the dog colour and a colour for each sensor state) and the 3D scene colours (volume edges, faces, ground and background).
- Widgets get every colour, text style, radius, padding and size from the theme. Nothing outside the theme file hard-codes these values. Material components are styled through the theme, not through overrides on individual widgets.
- The app follows the system's light or dark mode, and both schemes come from the same file.
- **Done when:** changing the primary colour, the font family, the base radius or the base spacing in that one file changes every screen, the drawer, the cards and the 3D view, with no other edits.

### Design

- Follow Material 3. Use Material components, Material icons, and standard Material motion and gestures.
- There is one main screen and no bottom navigation bar. The side menu opens the other pages.
- Cover the basics: icon-only controls (the burger menu, mode toggle and card buttons) have tooltips and semantic labels, tap targets are at least 48 dp, and text is readable in both themes.

### Backend use

- The app holds only the project URL, the publishable key and the signed-in user's session. It never contains the secret or service-role key, the sensor secret or anything belonging to the worker. Supply the URL and key at build time, not in source, and keep the local config file out of git.
- **There is no polling.** On the Free plan, every Data API request counts against the 1 GB monthly log-ingest allowance. The boards and worker already use most of it (about 4.2 million requests per 30 days in Device mode). The app loads data over REST when it starts, when it resumes and when it reconnects, and gets live updates from Realtime.
- Realtime is open only while the app is visible. Close it when the app is hidden: in the background on Android, minimised or hidden on macOS, or in a hidden browser tab on the web.
- Realtime doesn't replay changes missed while disconnected, so reload the tables after every reconnect or resume.
- Don't put MAC addresses in request URLs (for example as query filters). The gateway log can record them. Fetch the list and filter it in the app.
- Don't use any third-party services. Data leaves the phone only to go to this Supabase project.

## The app

### Sign-in

- The user signs in with an email and password through Supabase Auth. There is no sign-up link and no password reset. Errors appear inline.
- The session lasts across app restarts. Every other route requires a session, and an expired or invalid session returns the user to sign-in.

### Main screen

From top to bottom: a top bar with the burger menu on the left and the mode toggle, then the 3D view filling the screen, then the bottom drawer.

#### Mode toggle

- The toggle sits at the top and has two options, each labelled with an icon only: Device (for example `smartphone`) and Person (for example `person`). Each has a tooltip.
- The selected option follows `site_config.mode` as stored in the backend. That includes changes made somewhere else, such as SQL or another phone, which arrive through Realtime. While the app is sending a change, the toggle may show the requested mode.
- Choosing the other option updates `mode` in the backend. **This switches the whole house, not only the view**, because the boards change what they capture. No confirmation is needed, because a switch is quick to undo.
- While the change is pending, the toggle or the area next to it shows that. A change is pending until every online, enabled board has applied the new `config_version`. If the update fails, the toggle goes back to the stored mode and shows the error.
- A switch closes any focus card and returns the camera to the home view.

#### Status

The main screen shows a short status whenever something needs attention, so you don't have to open a menu page:

- A config change is pending (for example "5 of 6 sensors updated").
- In Person mode: the status is `calibration_required` or `insufficient_signal`, the snapshot is past `live_until`, the snapshot comes from before the latest switch to Person mode, or no snapshot exists yet. If the status is `ready` with no targets, say that nobody was detected.
- In Device mode: no phones have been seen within the "seen within" period.
- The connection is lost or reconnecting.

#### 3D view

- **Volume.** Draw a box from the `site_config` bounds (`x_min` to `x_max`, `y_min` to `y_max` and `z_min` to `z_max`). Read the bounds from the backend rather than hard-coding them. They are currently 0–11.2 × 0–9.4 × 0–5.3 m. The edges must be clearly visible, and the ground face must look different from the rest so the view is never ambiguous.
- **Frame.** Backend `z` points up from the ground-floor datum, and `x` and `y` are horizontal. On screen, up is always up. Treat the frame as right-handed: seen from above, `y` is 90° anticlockwise from `x`. Include an orientation cue, such as a small axis indicator or a marked origin corner, so the view can be checked against the house.
- **Home view.** The camera looks at the centre of the volume (currently `(5.6, 4.7, 2.65)`) from an oblique angle above. The whole volume fits the screen or window when the drawer is collapsed, from a phone in portrait to a wide desktop window.
- **Gestures.** A horizontal drag rotates the view around the target without limit. A vertical drag tilts it up or down within limits, so the camera can't flip over the top or end up upside down. Zooming goes from a close-up of one marker to slightly beyond the whole-house view. On Android you zoom with a pinch. On macOS and the web you drag with the mouse to rotate, and zoom with the scroll wheel or a trackpad pinch. Drags in the 3D view must not open the side menu or move the drawer.
- **Motion.** Camera moves (focusing on an item and returning home) are animated rather than instant. Live updates change the markers without resetting the camera.
- **Markers.** Markers always face the viewer, stay a readable size at every zoom level, and make it clear which are sensors and which are phones or people. Each marker's height must be readable, not only its position on screen; a drop line or shadow on the ground is one way to do this. Markers are allowed to overlap, and no clustering is needed.
  - **Sensors**, in both modes, use a sensor icon (for example `sensors`), coloured by state from the theme: participating, enabled but not participating (offline or on the wrong channel), and disabled. Unplaced sensors are not drawn.
  - **Phones**, in Device mode, use a phone icon. Each phone has its own colour as far as the palette allows, and that colour stays the same for its MAC across updates and app restarts. The drawer and the card use the same colour. A phone appears while its `captured_at` is within the "seen within" setting, and disappears as soon as it ages out, even if no new event arrives.
  - **Detections**, in Person mode, use a person icon for `kind = person` and a pet icon (`pets`) for `kind = dog`. All people share one colour, and the dog has a colour of its own. Both come from the theme.
- Tapping a marker to focus it is optional.

#### Bottom drawer

- The drawer is always on the main screen in both modes. When collapsed, it shows a handle and a one-line summary, for example "6 devices seen in the last hour" or "2 people, 1 dog · 4 s ago". The user pulls it up to see a scrollable list and pulls it down to collapse it. It must also open and close with a mouse on macOS and the web.
- In Device mode, each row shows the phone icon in that phone's colour, the MAC address in colon-separated form, `x`, `y` and `z` in metres, and when the phone was last seen (as a relative time). Rows are sorted with the most recently seen first. Showing `error_m`, or a badge for a private (locally administered) MAC address, is optional.
- In Person mode, each row shows the person or pet icon in the person or dog colour, the kind, `x`, `y` and `z`, and the confidence. There are no MAC addresses. The snapshot's capture time is shown once for the whole list.
- When the list is empty, it says why: no phones were seen in the period, the house is empty, calibration is required, the signal is insufficient, or no snapshot exists yet.
- Live updates change the list in place without losing the scroll position.
- Coordinates are shown to one or two decimal places, because accuracy is only a few metres.

#### Focus card

- Tapping a row collapses the drawer, animates the camera to zoom in on that item and shows a floating card next to its marker. The card stays attached to the marker as the camera moves or the item moves, and never covers the marker.
- The card shows the same fields as the item's row in the list.
- **X** (top right) closes the card. The camera zooms out to the home view and re-centres on the house, and the drawer stays collapsed.
- **Back arrow** (top left) closes the card and restores the state from before the row was tapped: the drawer at the same height and scroll position, and the camera where it was.
- While the card is open, gestures still work, and rotating and zooming orbit the focused item.
- The card's values update live. If a focused phone ages out of the period or is deleted, the card says so instead of closing on its own or jumping to another item, and X and back still work. Person detections have no identity, so the app decides how (or whether) to carry the focus from one snapshot to the next. It must never present that as identity, and the card must show when its data comes from an earlier snapshot or when the detection has gone.

### Side menu

The burger menu at the top left opens a Material navigation drawer from the left. It lists **Sensor config**, **System health** and **Settings**, and may also show the signed-in email address. Each item opens its own page with a back button. Returning to the main screen keeps the camera and the drawer as they were.

#### Sensor config

- **Site settings:**
  - `scan_all_devices` is a switch. The page explains that turning it on also keeps phones that only pass by, which is a privacy trade-off.
  - `primary_channel` is a number from 1 to 233, or empty when the boards aren't checked against a channel. Show each board's reported channel to help choose the value.
  - The site bounds are shown read-only.
- **Sensors:** list every registered sensor, including unplaced and disabled ones, with its id, whether it's placed and enabled, and its current state. Editing a sensor covers:
  - `x`, `y` and `z` in metres. They are set together or cleared together; clearing them unplaces the sensor. They must lie within the bounds shown, with `z` measured up from the ground-floor datum.
  - `enabled`.
  - `ref_rssi` (dBm at 1 m, from −100 to 0) and `path_loss` (greater than 0). These are the Device-mode calibration values and default to −45 and 2.5.
- Check values in the app where that's easy, and always show the backend's error message when it rejects a change.
- Before saving a change to a sensor's position or `enabled`, warn that this creates a new layout. Person mode will then report `calibration_required` until a model exists for the new layout. Moving a sensor back to exactly where it was restores the old layout. Changes to `ref_rssi` and `path_loss` don't affect the layout.
- After any change that increments `config_version`, show that it is pending until the boards have applied it.
- If two people edit at once, the last write wins. Nothing more is needed.

#### System health

This is a read-only page that updates live. Relative times keep counting up on screen without sending extra requests.

- **Site:** the mode and when it was last switched; the config version and how many online, enabled boards have applied it (pending or applied); the layout id; and the primary channel.
- **Each sensor:** id, when it was last heard from (relative and absolute), online, enabled, placed, its channel and whether that matches the primary channel, firmware, whether it has applied the current config, and whether it is participating. Highlight problems. The backend refreshes `last_seen_at` at most every 15 seconds, so "last heard" is only accurate to about 15 seconds.
- **Device positioning:** the newest fix published (the latest `updated_at`), labelled "last fix published", how many phones are within the period, and how many are stored in total. Positioning only produces fixes in Device mode, and with no phones in range the last fix stops changing even though positioning keeps running.
- **Person data:** the status; `captured_at` and how old it is; whether the snapshot is live or overdue compared with `live_until`; `published_at`; whether its layout matches the current one; and whether it comes from before the latest switch to Person mode. If the worker has never published, say so.
- **App:** whether Realtime is connected, and when the data was last fully reloaded.

#### Settings

These settings are stored locally and aren't synced:

- **Seen within:** how long after a phone was last seen it stays on the map and in the list. It can be anything from a few minutes up to 7 days, which is how long the backend keeps fixes. The default is one hour.
- **Account:** the signed-in email address and a sign-out button.
- **About:** the app version and the backend URL, so you can check which backend the app is connected to.
- **Optional:** a light/dark/system appearance choice, a minimum confidence for showing detections, and whether to draw disabled sensors.

## API spec

### Connection and auth

| Item | Value |
| --- | --- |
| Hosted URL | `https://<project-ref>.supabase.co` |
| Local URL | `http://127.0.0.1:55321` from macOS and the web. An Android emulator reaches it at `10.0.2.2:55321`, and a physical Android phone at the computer's LAN address. |
| Key | The project's publishable key. This is the only key in the app. |
| Headers on every request | `apikey: <publishable key>` and `Authorization: Bearer <user access token>` (`supabase_flutter` adds both) |
| Sign-in | Email and password with Supabase Auth, with refresh tokens. Signups are disabled (`enable_signup = false`). Users are created by hand in the Supabase dashboard. |
| Exposed schema | `public` only. `private` isn't reachable. |
| Role | `authenticated`. Without a session (`anon`), the app can read nothing. |

There are no roles: every signed-in user can do everything in the next table.

### Access

| Resource | Read | Update (columns) | Insert or delete | Realtime |
| --- | --- | --- | --- | --- |
| `site_config` | Yes | `mode`, `scan_all_devices`, `primary_channel` | No | Yes |
| `sensors` | Yes | `x`, `y`, `z`, `enabled`, `ref_rssi`, `path_loss` | No | Yes |
| `device_positions` | Yes | No | No | Yes |
| `person_snapshot` | Yes | No | No | Yes |
| `observations` | No | No | No | No |
| `rpc/ingest`, `rpc/publish_snapshot` | These are for the boards and the worker only. They need the sensor secret, and the app must never call them. | | | |

### `site_config`

There is exactly one row, with `id = 1`. `GET /rest/v1/site_config` returns it as a one-element array.

| Column | Type | Who writes it | Meaning |
| --- | --- | --- | --- |
| `id` | int | Server | Always 1 |
| `mode` | text | **App** | `device` (the default) or `person` |
| `scan_all_devices` | boolean | **App** | Device-mode scan setting. When true, the boards also keep phones that only pass by. |
| `primary_channel` | smallint, nullable | **App** | The house network's primary channel (1–233). Null means boards aren't checked against it. |
| `config_version` | bigint | Server | Increments whenever `mode`, `scan_all_devices`, `primary_channel` or `layout` changes |
| `mode_since` | timestamptz | Server | When the mode last changed |
| `layout` | text | Server | 12-hex-digit id of the enabled, placed sensors and their positions, or `none`. Moving sensors back to earlier positions restores the earlier id. |
| `x_min`, `x_max`, `y_min`, `y_max`, `z_min`, `z_max` | real | Server | Site bounds in metres |

### `sensors`

There is one row per board.

| Column | Type | Who writes it | Meaning |
| --- | --- | --- | --- |
| `id` | text | Board | `esp-` followed by the station MAC as 12 lowercase hex digits. The local seed uses ids such as `esp-down-1`, so treat ids as plain text. |
| `x`, `y`, `z` | real, nullable | **App** | Position in metres. Null until the sensor is placed. All three are set or cleared together, and must lie within the bounds. |
| `enabled` | boolean | **App** | Defaults to true. A disabled board is left out of results without being unplaced. |
| `ref_rssi` | real | **App** | RSSI at 1 m in dBm, from −100 to 0. Defaults to −45. |
| `path_loss` | real | **App** | Path-loss exponent, greater than 0. Defaults to 2.5. |
| `last_seen_at` | timestamptz, nullable | Board | The board's last exchange. Refreshed at most every 15 seconds, unless its config, channel or firmware changed. |
| `applied_config` | bigint, nullable | Board | The `config_version` the board has applied |
| `channel` | smallint, nullable | Board | The board's Wi-Fi primary channel |
| `firmware` | text, nullable | Board | For example `0.2.0-esp32` or `0.2.0-esp32c5` |

The app works out these states from the columns:

| State | Rule |
| --- | --- |
| Online | `last_seen_at` is within 45 seconds of now |
| On the primary channel | `primary_channel` is null, or `channel` equals it |
| Placed | `x`, `y` and `z` are all set |
| Participating | Enabled, placed, online and on the primary channel. Only participating boards contribute to results. |
| Applied | `applied_config = site_config.config_version` |
| Change pending | At least one online, enabled board hasn't applied the current config. Offline boards don't count. |

Boards exchange with Supabase every 5 seconds in Device mode and every 15 seconds in Person mode. A board applies a change on its next exchange and reports it about a second later, so a change can take around 15–20 seconds to show as applied.

### `device_positions`

There is one row per phone, holding its last fix.

| Column | Type | Meaning |
| --- | --- | --- |
| `device_mac` | text | Primary key: 12 lowercase hex digits, with no separators |
| `x`, `y`, `z` | real | Position in metres, within the bounds |
| `error_m` | real | RMS range residual in metres. This is a rough quality indicator, not a confidence interval. |
| `captured_at` | timestamptz | The newest RSSI reading behind the fix. This is when the phone was last seen. |
| `updated_at` | timestamptz | When the fix was published |

- Fixes are only published in Device mode, at most every 5 seconds per phone. A new phone arrives as an INSERT and a newer fix as an UPDATE. The server deletes a row once its `captured_at` is 7 days old, in either mode. Switching mode deletes nothing.
- A phone heard by fewer than four boards keeps its previous fix and time.
- The MAC address is the phone's identity, so a phone that changes its private address appears as a new device.
- Accuracy is a few metres, and `z` is the least reliable axis.
- To load the phones within the period, use `GET /rest/v1/device_positions?select=device_mac,x,y,z,error_m,captured_at,updated_at&captured_at=gte.<ISO 8601 time>`. With Realtime, the app removes phones itself once they age out of the period, because the server only sends a delete at 7 days.

### `person_snapshot`

There is at most one row, with `id = 1`. It holds the latest Person-mode result.

| Column | Type | Meaning |
| --- | --- | --- |
| `id` | int | Always 1 |
| `status` | text | `ready`, `calibration_required` or `insufficient_signal` |
| `layout` | text | The layout the snapshot is for |
| `captured_at` | timestamptz | When the data behind the snapshot was captured |
| `published_at` | timestamptz | When the snapshot was stored |
| `live_until` | timestamptz | `captured_at` plus the worker's TTL (5–120 s). The worker currently uses 6 s while people are detected and 60 s when the house is empty. |
| `targets` | jsonb array, at most 20 | Each target is `{"x", "y", "z", "confidence", "kind"}`, rounded to 2 decimal places. `confidence` is from 0 to 1, and `kind` is `person` or `dog`. `z` is the body centre: about the floor height plus 1.0 m standing, plus 0.6 m sitting, and plus 0.2–0.3 m for the dog. The list is non-empty only when `status = ready`. |

| Situation | What it means |
| --- | --- |
| No row | The worker has never published |
| `status = ready`, `targets` empty | The house is confidently empty |
| `status = ready`, `targets` non-empty | The current detections. Their order means nothing. |
| `calibration_required` | No model exists for this layout yet |
| `insufficient_signal` | Something changed that the model can't place |
| Now is before `live_until` | Live |
| Now is after `live_until` | The last data received. The worker's next publish is overdue, either because it's still processing or because it has stopped. Keep showing the row, along with its `captured_at`. |
| `captured_at` is before `site_config.mode_since` | The result comes from the previous Person session. Right after a switch, it can take about half a minute before the worker publishes again. |

Each publish replaces the row: the first publish arrives as an INSERT and later ones as UPDATEs. The worker publishes only in Person mode, every 2 seconds while people are detected, every 20 seconds otherwise, and whenever the status changes. The backend never deletes the row, and it keeps the row across mode switches.

### Coordinates and time

- All positions are in metres and share one frame. `x` and `y` are horizontal and lie within the bounds. `z` points up from the ground-floor datum. Sensors, phones and detections all use this frame.
- Timestamps are ISO 8601 in UTC with an offset. Show them in local time.
- Second-scale checks, such as `live_until` and the 45-second online rule, use server time. Use the HTTP `Date` response header or a Realtime event's `commit_timestamp` to estimate it. On the web the browser may hide the `Date` header from the app, so rely on `commit_timestamp` there. The local clock is close enough for the "seen within" filter.

### Edits

Updates are `PATCH` requests filtered by primary key. Add `Prefer: return=representation` to get the updated row back.

```http
PATCH /rest/v1/site_config?id=eq.1                  {"mode": "person"}
PATCH /rest/v1/site_config?id=eq.1                  {"scan_all_devices": true}
PATCH /rest/v1/site_config?id=eq.1                  {"primary_channel": 36}
PATCH /rest/v1/sensors?id=eq.esp-a1b2c3d4e5f6       {"x": 3.1, "y": 4.0, "z": 1.2}
PATCH /rest/v1/sensors?id=eq.esp-a1b2c3d4e5f6       {"x": null, "y": null, "z": null}
PATCH /rest/v1/sensors?id=eq.esp-a1b2c3d4e5f6       {"enabled": false}
PATCH /rest/v1/sensors?id=eq.esp-a1b2c3d4e5f6       {"ref_rssi": -52, "path_loss": 3.0}
```

| Case | HTTP | Postgres code | Message |
| --- | --- | --- | --- |
| Only some of `x`, `y` and `z` are set | 400 | `23514` | `Set x, y and z together, or clear all three` |
| Position is outside the bounds | 400 | `23514` | `Sensor position is outside the site bounds` |
| `ref_rssi` is out of range, `path_loss` ≤ 0, `primary_channel` is out of range, or `mode` is unknown | 400 | `23514` | Postgres's check-constraint message |
| The column isn't writable by the app | 403 | `42501` | `permission denied for table …` |
| The session is missing or expired | 401 | | Refresh the session or sign in again |
| The id doesn't exist | 2xx | | No row is changed |

Side effects:

- `mode` increments `config_version` and sets `mode_since`. The boards switch on their next exchange, and nothing is deleted.
- `scan_all_devices` and `primary_channel` increment `config_version`.
- A change to a sensor's `x`, `y`, `z` or `enabled` recalculates `layout`. If the layout changes, `config_version` increments too, and `site_config` sends a Realtime UPDATE. Device mode uses the new positions on its next run, within 5 seconds. Person mode reports `calibration_required` until a model exists for the new layout.
- `ref_rssi` and `path_loss` are used from the next positioning run. They don't change the config version.
- Setting a column to the value it already has doesn't increment `config_version` or change the layout, although the row still sends a Realtime UPDATE.

### Realtime

- The `supabase_realtime` publication contains `public.site_config`, `public.sensors`, `public.device_positions` and `public.person_snapshot`. Subscribe to `postgres_changes` (INSERT, UPDATE and DELETE) on them. The same read policies apply, so the subscription needs the user's access token, kept current as the token refreshes.
- Each event carries the event type, the new row (for an insert or update), the old primary key (for an update or delete) and `commit_timestamp`. A delete carries only the primary key.
- Missed events aren't replayed. Reload all four tables after subscribing or resubscribing.
- Approximate event rates: about one `sensors` UPDATE per board every 15 seconds (the heartbeat), plus health changes. `device_positions` sends up to one event per phone every 5 seconds in Device mode. `person_snapshot` sends one every 2 seconds while people are detected and every 20 seconds otherwise, in Person mode. `site_config` sends events on edits and layout changes.
- Realtime messages count against the Free plan's Realtime quota, which is separate from log ingest, but only while the app is subscribed.

### Local development

- Run `supabase start` from `terium_sensor/`. The API is at `http://127.0.0.1:55321`, and `supabase status` shows the local publishable key.
- The seed provides `site_config` with the house bounds and eight example sensors (`esp-down-1` to `esp-down-4` and `esp-up-1` to `esp-up-4`). It provides no users, no phone positions and no Person snapshot.
- Signups are disabled locally too. Create a development user with the local Auth admin API, using the local service-role key from `supabase status`. That key stays on the development machine and never goes into the app.
- Put sample `device_positions` and `person_snapshot` rows into the local database with SQL (`psql postgresql://postgres:postgres@127.0.0.1:55322/postgres`). These inserts fire Realtime events, so live updates can be checked locally. Test data belongs in the local database, not in the app.

### Examples

```json
{"id": 1, "mode": "device", "scan_all_devices": false, "primary_channel": 6,
 "layout": "5c1d0e9a7b31", "config_version": 14, "mode_since": "2026-12-01T17:40:02+00:00",
 "x_min": 0, "x_max": 11.2, "y_min": 0, "y_max": 9.4, "z_min": 0, "z_max": 5.3}
```

```json
{"id": "esp-a1b2c3d4e5f6", "x": 3.1, "y": 4.0, "z": 1.2, "enabled": true,
 "ref_rssi": -45, "path_loss": 2.5, "last_seen_at": "2026-12-01T18:04:02+00:00",
 "applied_config": 14, "channel": 6, "firmware": "0.2.0-esp32"}
```

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

The [sensor README](../terium_sensor/README.md#mobile-contract)'s "Mobile contract" section has a `curl` script that checks the contract without the app.

## Definition of done

| Area | Done when |
| --- | --- |
| Build | The app runs on an Android phone, on macOS and in a browser, against both the local stack and the hosted project. `flutter analyze` is clean. A short README in `terium_app/` explains how to set the URL and key and how to run it on each target against each backend. |
| Auth | A user set up by hand can sign in, stays signed in after a restart, and can sign out. |
| 3D view | The house box is drawn from the backend bounds and centred. Rotating horizontally and vertically and zooming work within their limits, by touch on Android and by mouse or trackpad on macOS and the web. Up is up, and the orientation cue matches the house. Sensors appear in both modes, coloured by state. |
| Mode toggle | The toggle switches the mode in Supabase. A switch made in SQL shows up within a few seconds. The toggle shows the change as pending until the boards apply it, and goes back to the stored mode if the update fails. |
| Device mode | Phones seen within the period appear in stable colours, update live, and disappear when they age out, without a restart. |
| Person mode | Detections appear with person or pet icons, with all people in one shared colour and the dog in its own. The `calibration_required`, `insufficient_signal`, overdue, previous-session and no-snapshot cases are each shown. |
| Drawer and card | Rows show the required fields. Tapping a row collapses the drawer, zooms in and shows the card. X returns to the home view. Back returns to the list at the same scroll position. The card updates live and handles its item disappearing. |
| Sensor config | Position, unplacing, `enabled`, `ref_rssi`, `path_loss`, `scan_all_devices` and `primary_channel` can all be edited. Backend errors are shown, and the layout warning appears before position or `enabled` changes. |
| System health | Every item listed above is shown and updates live. |
| Settings | Settings survive a restart. Changing "seen within" updates the map and the list straight away. |
| Theme | Changing the primary colour, font, radius and spacing in the one theme file changes every screen and the 3D view. Light and dark both work. |
| Backend use | When the app sits idle in the foreground, it sends no periodic REST requests; you can check this in the Supabase API logs or a network log. Hiding the app closes Realtime. The publishable key is the only key in the app, and the local config isn't committed. |

Nothing beyond this table is required. In particular, no test suite beyond a few unit tests, no CI, no store release and no production hardening.
