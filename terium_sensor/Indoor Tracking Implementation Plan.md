# Terium indoor tracking plan: Device and Person modes

This is a hobby project for one house and one user. Keep the codebase small, readable and modular, and build on what the repository already has. This plan sets out what to build, the constraints and when each stage is done. How to build it is up to the implementing agent. Choose the simplest approach that meets the requirements. Note any rejected alternatives in a code comment or README line rather than building them.

## Goal

Eight ESP32‑C5 boards around a two-storey house work on 5 GHz in one of two modes. Both modes cover the whole house.

- **Device mode** estimates an `(x, y, z)` position for each phone from RSSI trilateration. This is the existing tracker, ported to the C5.
- **Person mode** uses Wi‑Fi CSI between the boards to estimate a live set of anonymous `(x, y, z)` detections for people. It also detects the dog where the signal supports it.

The mode is switched in Supabase without reflashing the boards, and Device is the default. A separate mobile app reads the results and edits the mode and sensor placement through Supabase. That app is outside this plan.

The C5s arrive in a few weeks. Until then the current classic ESP32 boards on 2.4 GHz remain the default, and both modes are developed and exercised on them, including Person mode. The C5 build is selected with a build-time switch. Once the C5s are installed, a final change removes the 2.4 GHz path.

## Starting point

The repository at `d78cb31` already provides:

- **Firmware (`src/`).** The boards stay associated while sniffing on one channel. The firmware includes a frame parser and network filter, per-slot RSSI medians and bounded uploads to `public.ingest`. The reply returns `scan_all_devices` as configuration.
- **Supabase (`supabase/migrations/…_terium.sql`).** Tables `site_config`, `sensors`, `observations` and `device_positions`. Sensors register themselves and store their position and calibration. A Gauss–Newton `private.solve` runs from pg_cron every 5 seconds. Sensors authenticate with a shared secret, and signed-in users have read access.
- **Checks and pitfalls.** The README lists the local checks, and the pitfalls in `PLAN.md` §9 still apply.

Before adding migrations, confirm that the hosted schema and cron job match the checked-in migration.

## Constraints

### Code

- Extend existing modules, tables and functions before adding new ones. Keep a single firmware crate. Device mode continues to solve in Postgres.
- Keep chip- and band-specific code behind one boundary. Both builds should share the mode, upload and parsing logic, so that removing the legacy path later touches little else.
- Give each component one job. Capture, processing and publishing should be separable, so recorded data can be replayed through the same code used live.
- Write schema changes as new incremental migrations. All boards are reflashed together, so the exchange between the boards and Supabase may change without keeping the old contract.
- Keep the existing checks passing, and add simple unit tests for any pure logic you touch, such as parsers, solvers and feature extraction. Test matrices, staging environments and formal evaluation reports are unnecessary.

### Radio and hardware

- Leave household devices and the router on their normal settings, including channel, bandwidth and band steering. Don't change the router to make the tracker work. If coverage falls short, record where.
- Keep every board on the house network's primary channel in both modes. This is 5 GHz for the C5s and 2.4 GHz for the classic boards. A board on a different primary should report that and be left out of results.
- Keep Wi‑Fi power save off. Boards stay associated while sniffing or sending probes.
- Start the eight-board layout with four boards per floor at varied heights. Put some on interior walls and one low in a room the dog uses. Adjust the layout using walk-through results from both modes.

### Backend and infrastructure

- Use one site on the Supabase Free plan. Requests from the boards and the worker must stay within the Free plan log-ingest allowance described in the README. Eight boards at the current 5-second upload interval make about 4.15 million requests per 30 days, so choose the worker's publish rate with that in mind.
- Run Person-mode capture, training and inference on one always-on computer on the home LAN. A small Python program is the expected choice. Keep high-rate CSI and training data on the LAN, and send only live Person snapshots to Supabase.
- Use one coordinate frame in metres, with `z` measured upwards from the ground-floor datum. The house envelope is about `11.2 × 9.4 × 5.3 m`. Use surveyed board and floor heights. The seed values are only examples.

### Access and privacy

- Keep the existing access model. Boards and the worker write with the shared sensor secret, and signed-in users read. Signups remain disabled, so any signed-in user may also change the mode and edit sensors. Don't add roles or put a service-role key on the boards, worker or app.
- Keep raw observations, CSI and secrets out of the app's reach. Person detections are anonymous.

## Requirements

### Mode control

- `site_config` holds the active mode, which defaults to `device`. Only one mode is active at a time.
- The app or SQL can change the mode and can enable, disable, place or recalibrate a sensor. Positions are checked against the site bounds.
- Boards apply a change on their next regular exchange with Supabase without a reflash. Each board reports the configuration it has applied, along with its channel and firmware version. The app can then show a change as pending until every board has applied it.
- After a mode switch, results from the previous mode are never published or shown. Late uploads are ignored and old dots are cleared.
- Sensors that are unplaced, disabled or stale are left out of results.
- Moving a sensor or changing which sensors participate creates a new layout. Device mode uses the new positions immediately. Person mode reports that calibration is needed until a model exists for the current layout.

### Device mode

- Port the existing pipeline to the C5 on 5 GHz, including the parser, RSSI medians, upload and SQL solver. Recalibrate `ref_rssi` and `path_loss` for 5 GHz.
- Keep the existing per-slot medians at first. Add per-frame timing or cross-board frame matching only if measurements show a clear improvement in fixes.
- A fix needs at least four placed and enabled anchors whose geometry determines a 3D point. If the geometry is insufficient, publish nothing, not even the solver's starting point.
- Each position records the capture time as well as the publish time, so the app can expire stale dots. If a phone is locked or reception has a gap, the phone has no current fix rather than a stale one.
- Don't report board MACs or ESP probe traffic as devices.

### Person mode

- In Person mode the boards take turns transmitting known probe frames on the primary channel. The other boards capture CSI from them and stay associated for control traffic. Captured CSI goes to the LAN worker in batches with enough metadata to identify the board and probe each sample came from.
- Calibration is a deliberate local recording session for the current layout. It includes an empty-house reference, then timed and labelled positions marked with tape at measured heights. Record people walking, standing and sitting, and the dog moving and resting, on both floors. Recording a session and training a model should be a short, documented procedure that can be repeated after the sensors move.
- The worker publishes one live snapshot for the site, replacing the previous snapshot each time. A snapshot includes a status, the capture time, the layout it applies to and a list of targets of any length. Each target has numeric `x, y, z` and a confidence. If the house is confidently empty, the list is empty.
- The status is `ready`, `calibration_required` or `insufficient_signal`. When the evidence is weak, publish `insufficient_signal` instead of a guessed coordinate. The app treats a snapshot that is too old as offline.
- Develop detection in steps: first empty versus occupied, then one target, then several. Replay recorded sessions through the same processing as live to compare changes.

### Mobile contract

The mobile app is a separate project. Document the following in the README:

- **Configuration and sensor health.** What to read, including whether each sensor has applied the current configuration.
- **Edits.** How to change the mode and edit a sensor.
- **Results.** Where Device positions and the Person snapshot are stored, their fields, the coordinate frame and when a result should be treated as stale. Device identity is the MAC, so a phone that changes its private address appears as a new device.
- **Examples.** A sample row of each kind, plus a `curl` or SQL example that checks the contract without the app.

All of these should support Supabase Realtime for live updates.

## Stages and definition of done

| Stage | Done when |
| --- | --- |
| 1. Mode control on the current boards | The migration is applied locally and to the hosted project. The six classic boards switch from Device to Person and back to Device after a single database update. They acknowledge each change, and each switch leaves no stale results. Device mode works as it does today. |
| 2. Person mode on the current boards | Probes and CSI capture run on 2.4 GHz. The worker records and replays a calibration session and publishes live snapshots. The empty house shows no dots. One person moving between rooms and floors produces a dot that roughly follows them. Limitations are noted in the README. |
| 3. C5 build | `cargo build` and clippy pass for the C5 target selected with the build switch. The classic build remains the default. The pinned ESP-IDF and Rust bindings are confirmed to support the C5 and its CSI APIs, or any gaps are noted. |
| 4. C5 bring-up, once the boards arrive | Two C5s associate on the 5 GHz primary and report their channel. They capture uplinks from a phone on its normal settings, both when the phone is in use and when its screen is locked. They exchange CSI probes. Results are noted, including any frames the C5 cannot capture at the router's current bandwidth. |
| 5. Device mode on eight C5s | All eight boards are surveyed and placed. Device mode produces fixes upstairs and downstairs with roughly the current accuracy of a few metres. Rooms without coverage are noted. |
| 6. Person mode on eight C5s | Person mode is calibrated for the installed layout. It distinguishes two positions in one room, two heights and two people in different rooms. Blind spots, merged detections and how well the dog is detected are noted. Switching from Device to Person and back works with one board unplugged. Free-plan usage is checked after 24 hours. |
| 7. Remove the legacy path | The 2.4 GHz and classic ESP32 code and build configuration are deleted. The C5 build is the only build, and the README is updated. |

At the end of every stage, the existing checks pass and the README is up to date. The README covers building and flashing, mode switching, calibration for both modes, worker setup, the mobile contract and known limitations.

## Out of scope

- The mobile app.
- Production practices, including role-based permissions, handling of concurrent edits, compatibility with old firmware, staging environments, monitoring, formal test plans and evaluation reports.
- OTA updates, multiple sites and router changes.
- A different receiver for client PHYs the C5 cannot capture. If Device coverage falls short, note this as a follow-up.

## Known risks

- The C5 sniffs 20 MHz transmissions. It can miss wider transmissions and may report only the length of some MIMO frames. Some phone uplinks at the router's current bandwidth may therefore be invisible, which would limit Device mode. Healthy beacon reception doesn't prove that client traffic is being captured.
- Mesh nodes may use different 5 GHz primary channels in different parts of the house.
- C5 support in the pinned ESP-IDF v6.0.3 and the `esp-idf-sys` bindings is unconfirmed, including CSI. Check it on the actual C5‑Zero, including the flash layout and heap.
- Check on the pinned SDK how a CSI sample is linked to its probe's sender and sequence number.
- Person-mode accuracy with eight low-cost boards is unproven. Published results used different hardware and layouts. Floors weaken signals in both modes, so `z` is the least reliable axis.

## References

- [Reviewed repository revision](https://github.com/RoryWilson92/terium/tree/d78cb31fbe19096bc4aa541ee930f9c6df52b49e) and [existing migration and solver](https://github.com/RoryWilson92/terium/blob/d78cb31fbe19096bc4aa541ee930f9c6df52b49e/supabase/migrations/20261108221619_trilateration.sql)
- [ESP32‑C5 Wi‑Fi modes and sniffer frame support](https://docs.espressif.com/projects/esp-idf/en/latest/esp32c5/api-guides/wifi-driver/wifi-modes.html) and [ESP32‑C5 Wi‑Fi FAQ](https://docs.espressif.com/projects/esp-faq/en/latest/esp-faq-en-master.pdf)
- [ESP32‑C5 CSI format and configuration](https://docs.espressif.com/projects/esp-idf/en/latest/esp32c5/api-guides/wifi-driver/wifi-vendor-features.html), [Espressif 5 GHz CSI example](https://github.com/espressif/esp-csi/blob/master/examples/esp-crab/master_recv/main/app_main.c) and [ESP-NOW channel rules](https://docs.espressif.com/projects/esp-idf/en/latest/esp32c5/api-reference/network/esp_now.html)
- [ESP32‑C5 Rust target support](https://github.com/esp-rs/esp-idf-sys/blob/master/CHANGELOG.md) and [C5‑Zero hardware reference](https://docs.waveshare.com/ESP32-C5-Zero)
- [Supabase Realtime database changes](https://supabase.com/docs/guides/realtime/postgres-changes)
- [Person-in-WiFi 3D](https://openaccess.thecvf.com/content/CVPR2024/papers/Yan_Person-in-WiFi_3D_End-to-End_Multi-Person_3D_Pose_Estimation_with_Wi-Fi_CVPR_2024_paper.pdf). The study used a different receiver layout, so validate Person mode locally.
