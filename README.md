# terium sensor

Classic ESP32s stay connected to the same 2.4 GHz Wi-Fi channel while listening for phones. Each board sends the median RSSI per device per two-second slot to Supabase every five seconds. Postgres estimates a position from the last 5 seconds of readings when at least four placed sensors heard the device.

## Local checks

Start Docker and run these from the repository root. The local database uses example positions and a test key. Its API listens on port 55321.

```sh
supabase start
supabase test db --local
supabase db advisors --local --type security --level warn --fail-on warn
mkdir -p target
rustc +stable --edition=2021 --test src/sniffer.rs -o target/sniffer-test
./target/sniffer-test
rustc +stable --edition=2021 --test src/rssi.rs -o target/rssi-test
./target/rssi-test
```

## Hosted setup

Once you have created a Supabase Free project in a new organisation in London, push the schema. The example seed is for local use only. Keep `private` out of the Data API's exposed schemas.

```sh
supabase link --project-ref YOUR_PROJECT_REF
supabase db push
```

Generate a shared sensor secret with `openssl rand -hex 32`. Save the secret in `cfg.toml` and compute its SHA-256 hex digest locally using `printf '%s' "$SENSOR_KEY" | shasum -a 256`, with `SENSOR_KEY` set to the generated value. Paste only the digest into the SQL editor. Set the bounds to the house's dimensions in metres and schedule the job.

```sql
insert into private.secret (key_sha256) values ('REPLACE_WITH_64_HEX_CHARACTERS');
insert into public.site_config (id, x_min, x_max, y_min, y_max, z_min, z_max)
values (1, 0, 12, 0, 10, 0, 10);
select cron.schedule('terium', '5 seconds', 'select private.compute_positions()');
```

Create a test user in Supabase Auth and disable open signups so access stays with users you provision.

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

Supabase records every Data API request in the gateway log. Six boards at five-second intervals send about 3.1 million requests in 30 days. Check the usage page after 24 hours against the Free plan's 1 GB monthly log-ingest allowance. If usage is tracking above it, increase `UPLOAD_INTERVAL` in the firmware and reflash the boards. Removing stored logs doesn't reduce ingest already counted. [Supabase log sources](https://supabase.com/docs/guides/observability/log-field-reference), [Supabase log-ingest quota](https://supabase.com/docs/guides/platform/manage-your-usage/logs-ingest)

## Build and flash

Install the Xtensa Rust toolchain using [espup](https://github.com/esp-rs/espup), then source the environment file printed by `espup install`. Install `ldproxy` and `espflash` for your computer. The repository pins ESP-IDF v6.0.3 and the Rust dependencies.

```sh
cargo +stable install --locked ldproxy espflash --target "$(rustc +stable -vV | sed -n 's/^host: //p')"
cp cfg.toml.example cfg.toml
# Edit cfg.toml with the shared Wi-Fi details, project URL, publishable key and sensor secret.
cargo build --release
cargo clippy --release -- -D warnings
firmware=target/xtensa-esp32-espidf/release
espflash flash --bootloader "$firmware/bootloader.bin" \
  --partition-table "$firmware/partition-table.bin" "$firmware/terium_sensor"
```

Flash the same release image to every board using the generated bootloader and partition table. Each registers as `esp-<station MAC in lowercase hex>` on its first successful request, even when it has heard no devices. For a board on USB, set `log_serial = true`, rebuild and add `--monitor` to the flash command. The 60-second line shows events per second, cumulative channel drops, held readings, counts of local APs and devices, the Wi-Fi channel, and free heap. Return wall boards to `log_serial = false`. The application and ESP-IDF bootloader are quiet in that mode. The classic ESP32's ROM may still emit its own reset message, and panics print.

## Place and calibrate sensors

Check `select * from public.sensors;` in the SQL editor. Use each registered ID and measured coordinates. A null coordinate excludes that sensor from the solve.

```sql
update public.sensors set x = 0, y = 0, z = 1 where id = 'esp-REPLACE_WITH_MAC';
update public.site_config set scan_all_devices = false where id = 1;
```

The default mode keeps traffic from the local mesh and probes naming the SSID or sent by recently seen local phones. Set `scan_all_devices = true` to also keep other phones' data frames and probes. The new setting reaches each board on its next successful upload.

Spread the boards across both storeys, for example three about 1 m above the downstairs floor and three at about z = 6 m. Move or add a board by updating its coordinates. Walk a phone through the house and check that its position follows. Verify that all mesh nodes and sensors use the same 2.4 GHz channel. To calibrate, hold a phone one metre from a sensor, average the RSSI and update that sensor's `ref_rssi` (default −45 dBm). Adjust `path_loss` (default 2.5) if measurements at other distances warrant it.

Expect errors of a few metres. Floor attenuation makes height the least reliable axis, so treat z above 5 m as an upstairs estimate. A storey with fewer than three sensors generally has rougher coverage. A phone connected only on 5 GHz is visible on 2.4 GHz when it probes there. In default mode it must name the SSID or already be remembered.

## Read API and privacy

A signed-in client sends `apikey: <publishable key>` and `Authorization: Bearer <user access token>` to `GET /rest/v1/device_positions`, `/rest/v1/sensors` or `/rest/v1/site_config`. The API gives signed-in users read access to all rows in these tables. Observations and the sensor secret stay outside client access.

The HTTPS upload sends the real MAC address as 12 lowercase hex digits, including any private address chosen by the phone. A phone may change that address, and unassociated probes often rotate it. The database deletes observations older than one hour and positions that haven't been updated in one hour. The gateway records request metadata separately and can record MACs that appear in query filters. The documented gateway fields do not list the JSON upload body. [Supabase log field reference](https://supabase.com/docs/guides/observability/log-field-reference)
