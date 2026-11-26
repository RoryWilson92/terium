-- Device and Person modes, sensor health, layouts, the latest Person snapshot and Realtime.
-- All boards are reflashed with this migration, so the board exchange changes in place.

-- Mode control. config_version counts every change the boards or the worker must act on, so
-- a sensor whose applied_config is lower has not caught up yet. mode_since marks the last switch:
-- readings and snapshots captured before it are not used for the new mode's results. A switch
-- deletes nothing, so each mode's stored results are still there when the site switches back.
-- primary_channel is the house network's primary. Null means boards are not checked against it.
alter table public.site_config
  add column mode text not null default 'device' check (mode in ('device', 'person')),
  add column primary_channel smallint check (primary_channel between 1 and 233),
  add column layout text not null default 'none',
  add column config_version bigint not null default 1,
  add column mode_since timestamptz not null default now();

-- enabled lets the app take a board out without unplacing it. The board reports the config it
-- has applied, its primary channel and its firmware on every exchange.
alter table public.sensors
  add column enabled boolean not null default true,
  add column applied_config bigint,
  add column channel smallint,
  add column firmware text,
  add constraint ref_rssi_range check (ref_rssi between -100 and 0);

-- captured_at is the newest RSSI slot behind the fix. updated_at stays the publish time.
-- Existing fixes take their publish time.
alter table public.device_positions add column captured_at timestamptz;
update public.device_positions set captured_at = updated_at where true;
alter table public.device_positions alter column captured_at set not null;

-- The latest Person-mode result for the site, replaced by each worker publish and kept across
-- mode switches. No history is kept. targets is a JSON array of {x, y, z, confidence, kind};
-- empty means confidently empty when status is 'ready'. The snapshot is live until live_until,
-- when the worker's next publish is overdue; after that it is the last data received.
create table public.person_snapshot (
  id int primary key default 1 check (id = 1),
  status text not null check (status in ('ready', 'calibration_required', 'insufficient_signal')),
  layout text not null,
  captured_at timestamptz not null,
  published_at timestamptz not null,
  live_until timestamptz not null,
  targets jsonb not null check (jsonb_typeof(targets) = 'array')
);

alter table public.person_snapshot enable row level security;
revoke all on public.person_snapshot from public, anon, authenticated;
grant select on public.person_snapshot to authenticated;
create policy read_snapshot on public.person_snapshot for select to authenticated using (true);

-- Signups are disabled, so any signed-in user may switch the mode and place sensors.
-- Column grants keep config_version, layout, bounds and board-reported health server-owned.
grant update (mode, scan_all_devices, primary_channel) on public.site_config to authenticated;
grant update (x, y, z, enabled, ref_rssi, path_loss) on public.sensors to authenticated;
create policy edit_site on public.site_config for update to authenticated
  using ((select auth.uid()) is not null) with check ((select auth.uid()) is not null);
create policy edit_sensors on public.sensors for update to authenticated
  using ((select auth.uid()) is not null) with check ((select auth.uid()) is not null);

-- Layout id of the participating sensors: enabled and placed. Moving a sensor back to where it
-- was restores the old id, so a model trained for that layout applies again.
create function private.layout() returns text
language sql stable set search_path = '' as $$
  select coalesce(left(md5(string_agg(format('%s %s %s %s', id, x, y, z), ',' order by id)), 12), 'none')
  from public.sensors where enabled and x is not null;
$$;

-- Keeps layout current and bumps config_version when anything the boards or worker use changes.
create function private.site_config_changed() returns trigger
language plpgsql security definer set search_path = '' as $$
begin
  new.layout := private.layout();
  if tg_op = 'UPDATE' then
    if (new.mode, new.scan_all_devices, new.primary_channel, new.layout)
       is distinct from (old.mode, old.scan_all_devices, old.primary_channel, old.layout) then
      new.config_version := old.config_version + 1;
    end if;
    if new.mode <> old.mode then
      new.mode_since := now();
    end if;
  end if;
  return new;
end;
$$;

create trigger site_config_changed before insert or update on public.site_config
for each row execute function private.site_config_changed();

-- x, y and z are set or cleared together and must lie inside the site bounds.
create function private.check_sensor_position() returns trigger
language plpgsql security definer set search_path = '' as $$
declare
  site public.site_config%rowtype;
begin
  if num_nulls(new.x, new.y, new.z) = 3 then
    return new;
  elsif num_nulls(new.x, new.y, new.z) > 0 then
    raise check_violation using message = 'Set x, y and z together, or clear all three';
  end if;
  select * into site from public.site_config where id = 1;
  if found and not (
    new.x between site.x_min and site.x_max and
    new.y between site.y_min and site.y_max and
    new.z between site.z_min and site.z_max
  ) then
    raise check_violation using message = 'Sensor position is outside the site bounds';
  end if;
  return new;
end;
$$;

create trigger check_sensor_position before insert or update of x, y, z on public.sensors
for each row execute function private.check_sensor_position();

-- Placing, moving, enabling or disabling a sensor makes a new layout. The site_config trigger
-- recomputes it and bumps config_version, so the boards pick up the new probe schedule.
create function private.sensors_changed() returns trigger
language plpgsql security definer set search_path = '' as $$
begin
  update public.site_config set layout = private.layout()
  where id = 1 and layout is distinct from private.layout();
  return null;
end;
$$;

create trigger sensors_changed after delete or update of x, y, z, enabled on public.sensors
for each statement execute function private.sensors_changed();
-- Row-level for inserts: a board registering itself is unplaced, so its upsert skips this.
create trigger sensor_placed after insert on public.sensors
for each row when (new.x is not null) execute function private.sensors_changed();

update public.site_config set layout = private.layout() where id = 1;

-- Shared by the board and worker entry points. Compares the SHA-256 hex, never the secret.
create function private.check_sensor_key() returns void
language plpgsql stable security definer set search_path = '' as $$
begin
  -- PostgREST puts request headers in this setting.
  if not exists (
    select 1 from private.secret
    where key_sha256 = encode(sha256(convert_to(
      current_setting('request.headers', true)::json ->> 'x-sensor-key', 'UTF8'
    )), 'hex')
  ) then
    raise insufficient_privilege using message = 'Invalid sensor key';
  end if;
end;
$$;

drop function public.ingest(text, jsonb);
drop function private.ingest(text, jsonb);

-- One board exchange: heartbeat, applied config and RSSI readings in; the current config out.
-- Readings are stored only in Device mode, from an enabled and placed sensor, and only if they
-- were captured after the switch to Device mode. Late uploads from the previous mode are dropped.
create function private.ingest(
  sensor_id text, readings jsonb, applied_config bigint, channel int, firmware text
) returns jsonb
language plpgsql security definer set search_path = '' as $$
declare
  site public.site_config%rowtype;
begin
  perform private.check_sensor_key();
  if sensor_id is null or sensor_id !~ '^esp-[0-9a-f]{12}$' then
    raise invalid_parameter_value using message = 'Invalid sensor ID';
  end if;

  -- First contact creates the row unplaced. last_seen_at is only refreshed every 15 seconds, so
  -- a heartbeat does not become a Realtime event on every exchange. Health changes write at once.
  insert into public.sensors as s (id, last_seen_at, applied_config, channel, firmware)
  values (
    ingest.sensor_id, now(), ingest.applied_config,
    case when ingest.channel between 1 and 233 then ingest.channel end,
    left(ingest.firmware, 40)
  )
  on conflict (id) do update
  set (last_seen_at, applied_config, channel, firmware) =
      (excluded.last_seen_at, excluded.applied_config, excluded.channel, excluded.firmware)
  where s.last_seen_at is null or s.last_seen_at < now() - interval '15 seconds'
     or (s.applied_config, s.channel, s.firmware)
        is distinct from (excluded.applied_config, excluded.channel, excluded.firmware);

  select * into site from public.site_config where id = 1;

  -- Even slot from the last five minutes, integer RSSI in -100..-1, at most 2000 rows.
  -- Sensor MACs are skipped. A retried row hits the primary key and is ignored.
  insert into public.observations (ts, sensor_id, device_mac, rssi)
  select to_timestamp(r.ts::double precision), ingest.sensor_id, r.device, r.rssi
  from jsonb_to_recordset(readings) as r(device text, ts numeric, rssi numeric)
  where site.mode = 'device'
    and exists (
      select 1 from public.sensors own
      where own.id = ingest.sensor_id and own.enabled and own.x is not null
    )
    and r.device ~ '^[0-9a-f]{12}$'
    and r.ts between extract(epoch from now()) - 300 and extract(epoch from now()) + 300
    and r.ts >= extract(epoch from site.mode_since)
    and mod(r.ts, 2) = 0
    and r.rssi between -100 and -1 and r.rssi = trunc(r.rssi)
    and not exists (select 1 from public.sensors s where s.id = 'esp-' || r.device)
  limit 2000
  on conflict do nothing;

  -- peers are the participating boards' MACs in a fixed order. In Person mode a board transmits
  -- in the turn given by its index, and only keeps CSI from these transmitters.
  return jsonb_build_object(
    'config', coalesce(site.config_version, 0),
    'mode', coalesce(site.mode, 'device'),
    'scan_all_devices', coalesce(site.scan_all_devices, false),
    'peers', (
      select coalesce(jsonb_agg(substr(id, 5) order by id), '[]')
      from public.sensors where enabled and x is not null
    )
  );
end;
$$;

-- Data API entry point (POST /rest/v1/rpc/ingest). Invoker rights; the writes happen in private.ingest.
create function public.ingest(
  sensor_id text, readings jsonb, applied_config bigint, channel int, firmware text
) returns jsonb
language sql security invoker set search_path = '' as $$
  select private.ingest(sensor_id, readings, applied_config, channel, firmware);
$$;

-- The LAN worker's exchange. With a snapshot, it replaces the stored Person result if the site is
-- in Person mode, the snapshot is for the current layout, and it was captured after the switch.
-- Without one, or when it is not accepted, it only returns what the worker needs to run.
-- snapshot: {status, layout, captured_at (unix seconds), ttl (seconds it counts as live),
-- targets: [{x, y, z, confidence, kind}]}.
create function private.publish_snapshot(snapshot jsonb) returns jsonb
language plpgsql security definer set search_path = '' as $$
declare
  site public.site_config%rowtype;
  captured timestamptz;
  accepted boolean := false;
begin
  perform private.check_sensor_key();
  select * into site from public.site_config where id = 1;
  if not found then
    raise invalid_parameter_value using message = 'Insert the site_config row first';
  end if;

  if snapshot is not null then
    captured := to_timestamp((snapshot ->> 'captured_at')::double precision);
    if captured is null or captured > now() + interval '5 seconds'
       or snapshot ->> 'status' not in ('ready', 'calibration_required', 'insufficient_signal')
       or jsonb_typeof(snapshot -> 'targets') is distinct from 'array'
       or jsonb_array_length(snapshot -> 'targets') > 20
       or (snapshot ->> 'status' <> 'ready' and jsonb_array_length(snapshot -> 'targets') > 0)
       or exists (
         select 1 from jsonb_to_recordset(snapshot -> 'targets')
           as t(x real, y real, z real, confidence real, kind text)
         where not coalesce(
           t.x between site.x_min and site.x_max and t.y between site.y_min and site.y_max and
           t.z between site.z_min and site.z_max and t.confidence between 0 and 1 and
           t.kind in ('person', 'dog'), false)
       )
    then
      raise invalid_parameter_value using message = 'Invalid snapshot';
    end if;

    -- A snapshot from before the switch, for another layout, or older than the stored one is ignored.
    if site.mode = 'person' and snapshot ->> 'layout' = site.layout
       and captured >= site.mode_since and captured > now() - interval '1 minute' then
      insert into public.person_snapshot as p
        (id, status, layout, captured_at, published_at, live_until, targets)
      values (
        1, snapshot ->> 'status', site.layout, captured, now(),
        captured + make_interval(secs => least(120, greatest(5,
          coalesce((snapshot ->> 'ttl')::double precision, 10)))),
        (select coalesce(jsonb_agg(jsonb_build_object(
            'x', round(t.x::numeric, 2), 'y', round(t.y::numeric, 2), 'z', round(t.z::numeric, 2),
            'confidence', round(t.confidence::numeric, 2), 'kind', t.kind)), '[]')
         from jsonb_to_recordset(snapshot -> 'targets')
           as t(x real, y real, z real, confidence real, kind text))
      )
      on conflict (id) do update
      set (status, layout, captured_at, published_at, live_until, targets) =
          (excluded.status, excluded.layout, excluded.captured_at, excluded.published_at,
           excluded.live_until, excluded.targets)
      where p.captured_at < excluded.captured_at;
      accepted := found;
    end if;
  end if;

  return jsonb_build_object(
    'accepted', accepted,
    'mode', site.mode,
    'layout', site.layout,
    'config', site.config_version,
    'primary_channel', site.primary_channel,
    'bounds', jsonb_build_array(site.x_min, site.x_max, site.y_min, site.y_max, site.z_min, site.z_max),
    'sensors', (
      select coalesce(jsonb_agg(jsonb_build_object(
        'id', id, 'x', x, 'y', y, 'z', z, 'channel', channel, 'last_seen_at', last_seen_at) order by id), '[]')
      from public.sensors where enabled and x is not null
    )
  );
end;
$$;

-- Data API entry point (POST /rest/v1/rpc/publish_snapshot), used with the sensor key.
create function public.publish_snapshot(snapshot jsonb) returns jsonb
language sql security invoker set search_path = '' as $$
  select private.publish_snapshot(snapshot);
$$;

revoke all on function
  private.ingest(text, jsonb, bigint, int, text), public.ingest(text, jsonb, bigint, int, text),
  private.publish_snapshot(jsonb), public.publish_snapshot(jsonb),
  private.check_sensor_key(), private.layout(), private.site_config_changed(),
  private.check_sensor_position(), private.sensors_changed()
  from public, anon, authenticated;
-- anon must be able to name the private functions from the invoker wrappers. It has no table grants.
grant execute on function
  private.ingest(text, jsonb, bigint, int, text), public.ingest(text, jsonb, bigint, int, text),
  private.publish_snapshot(jsonb), public.publish_snapshot(jsonb)
  to anon;

-- Thinnest spread of the anchors in metres: the square root of the smallest eigenvalue of the
-- covariance of their positions (closed form for a symmetric 3x3). Zero when they are coplanar.
create function private.thinnest_spread(xs real[], ys real[], zs real[]) returns double precision
language plpgsql immutable set search_path = '' as $$
declare
  -- Covariance [a b c; b d e; c e f].
  a double precision; b double precision; c double precision;
  d double precision; e double precision; f double precision;
  q double precision; p double precision; r double precision;
begin
  select var_pop(x), covar_pop(x, y), covar_pop(x, z), var_pop(y), covar_pop(y, z), var_pop(z)
  into a, b, c, d, e, f
  from unnest(xs, ys, zs) as s(x, y, z);
  q := (a + d + f) / 3;
  p := sqrt(((a - q)^2 + (d - q)^2 + (f - q)^2 + 2 * (b*b + c*c + e*e)) / 6);
  if p < 1e-9 then
    return sqrt(greatest(q, 0));
  end if;
  -- r = det((C - qI) / p) / 2, clamped against rounding. The smallest root is at phi + 2pi/3.
  r := ((a - q) * ((d - q) * (f - q) - e*e) - b * (b * (f - q) - e*c) + c * (b*e - (d - q) * c))
       / (2 * p^3);
  r := greatest(-1, least(1, r));
  return sqrt(greatest(0, q + 2 * p * cos(acos(r) / 3 + 2 * pi() / 3)));
end;
$$;

-- Weighted Gauss-Newton in 3D. Needs four or more distances, anchors that span 3D, and a
-- site_config row. Each iterate is clamped to the site bounds, which also rejects a below-floor
-- mirror. error_m is the RMS residual. Returns no row when the geometry cannot fix a point,
-- including when the first step is singular, so the starting centroid is never published.
create or replace function private.solve(xs real[], ys real[], zs real[], ds real[])
returns table (x real, y real, z real, error_m real)
language plpgsql stable set search_path = '' as $$
declare
  bounds public.site_config%rowtype;
  px double precision; py double precision; pz double precision;
  a double precision; b double precision; c double precision;
  d double precision; e double precision; f double precision;
  u double precision; v double precision; w double precision;
  aa double precision; ab double precision; ac double precision;
  dd double precision; de double precision; ff double precision;
  det double precision;
begin
  select * into bounds from public.site_config where id = 1;
  if not found or cardinality(ds) < 4 then return; end if;
  -- Coplanar anchors leave a mirror ambiguity, so require 0.3 m of spread in every direction.
  if private.thinnest_spread(xs, ys, zs) < 0.3 then return; end if;

  -- Start at the centroid of the sensors, weighted by 1/d².
  select sum(sx / dist^2) / sum(1 / dist^2),
         sum(sy / dist^2) / sum(1 / dist^2),
         sum(sz / dist^2) / sum(1 / dist^2)
  into px, py, pz
  from unnest(xs, ys, zs, ds) as s(sx, sy, sz, dist);

  for i in 0..10 loop
    -- Clamp before the step. An unclamped solve often mirrors an upstairs point below the floor.
    px := greatest(bounds.x_min, least(bounds.x_max, px));
    py := greatest(bounds.y_min, least(bounds.y_max, py));
    pz := greatest(bounds.z_min, least(bounds.z_max, pz));
    -- Ten correction steps (i = 0..9). i = 10 only clamps the step just applied.
    exit when i = 10;

    -- J is d(predicted range)/d(position). residual is measured minus predicted, so the step is added.
    -- Jᵀ W J (symmetric) and Jᵀ W times the distance residual.
    select sum(weight*jx*jx), sum(weight*jx*jy), sum(weight*jx*jz),
           sum(weight*jy*jy), sum(weight*jy*jz), sum(weight*jz*jz),
           sum(weight*jx*residual), sum(weight*jy*residual), sum(weight*jz*residual)
    into a, b, c, d, e, f, u, v, w
    from unnest(xs, ys, zs, ds) as s(sx, sy, sz, dist)
    cross join lateral (select px-sx as dx, py-sy as dy, pz-sz as dz) delta
    cross join lateral (select sqrt(dx^2 + dy^2 + dz^2) as radius) r
    -- Floor the range so an estimate sitting on a sensor does not divide by zero.
    cross join lateral (select greatest(radius, 0.000001) as denom) n
    cross join lateral (
      select dx/denom as jx, dy/denom as jy, dz/denom as jz,
             1/dist^2 as weight, dist-radius as residual
    ) j;

    -- Cramer's rule via the cofactors shared by the three numerators. A tiny det means stop,
    -- and a tiny det before the first step means the ranges do not fix a point.
    aa := d*f - e*e; ab := c*e - b*f; ac := b*e - c*d;
    dd := a*f - c*c; de := b*c - a*e; ff := a*d - b*b;
    det := a*aa + b*ab + c*ac;
    if det <= 1e-12 then
      if i = 0 then return; end if;
      exit;
    end if;
    px := px + (aa*u + ab*v + ac*w) / det;
    py := py + (ab*u + dd*v + de*w) / det;
    pz := pz + (ac*u + de*v + ff*w) / det;
  end loop;

  return query select px::real, py::real, pz::real,
    sqrt(avg((sqrt((px-sx)^2 + (py-sy)^2 + (pz-sz)^2) - dist)^2))::real
  from unnest(xs, ys, zs, ds) as s(sx, sy, sz, dist);
end;
$$;

-- Cron entry point. In Device mode, turn the last 5 seconds of RSSI into positions. Anchors are
-- enabled, placed and on the primary channel; a stale sensor has no recent readings. Each phone
-- keeps its last fix until a newer one replaces it or it is a week old, in either mode, so the
-- app can choose how recently a phone must have been seen.
create or replace function private.compute_positions() returns void
language sql set search_path = '' as $$
  insert into public.device_positions (device_mac, x, y, z, error_m, captured_at, updated_at)
  with site as (
    select * from public.site_config where id = 1 and mode = 'device'
  ), distances as (
    select o.device_mac, s.id, s.x, s.y, s.z, max(o.ts) as captured_at,
      -- d = 10 ^ ((ref_rssi - rssi) / (10 * path_loss)), clamped to 0.3-25 m.
      least(25, greatest(0.3, 10 ^ ((s.ref_rssi - avg(o.rssi)) / (10 * s.path_loss))))::real as d
    from site
    join public.observations o on o.ts >= site.mode_since
    join public.sensors s on s.id = o.sensor_id
    where o.ts between now() - interval '5 seconds' and now()
      and s.enabled and s.x is not null and s.y is not null and s.z is not null
      and (site.primary_channel is null or s.channel = site.primary_channel)
      -- A MAC that belongs to a board is not a phone.
      and not exists (select 1 from public.sensors own where own.id = 'esp-' || o.device_mac)
    group by o.device_mac, s.id
  ), devices as (
    -- A 3D fix needs four ranges.
    select device_mac, array_agg(x order by id) as xs, array_agg(y order by id) as ys,
      array_agg(z order by id) as zs, array_agg(d order by id) as ds,
      max(captured_at) as captured_at
    from distances group by device_mac having count(*) >= 4
  )
  select device_mac, p.x, p.y, p.z, p.error_m, captured_at, now()
  from devices cross join lateral private.solve(xs, ys, zs, ds) p
  on conflict (device_mac) do update
  set (x, y, z, error_m, captured_at, updated_at) =
      (excluded.x, excluded.y, excluded.z, excluded.error_m, excluded.captured_at, excluded.updated_at);

  -- Fixes are kept for a week. Observations and pg_cron history are kept for one hour.
  delete from public.device_positions where captured_at < now() - interval '7 days';
  delete from public.observations where ts < now() - interval '1 hour';
  delete from cron.job_run_details where end_time < now() - interval '1 hour';
$$;

revoke all on function private.thinnest_spread(real[], real[], real[]) from public, anon, authenticated;

-- Live updates for the app. Realtime applies the same read policies.
do $$
begin
  if not exists (select 1 from pg_publication where pubname = 'supabase_realtime') then
    create publication supabase_realtime;
  end if;
end;
$$;
alter publication supabase_realtime add table
  public.site_config, public.sensors, public.device_positions, public.person_snapshot;

-- The hosted project's job was scheduled by hand under the old project name. Keep its schedule
-- and command under the current name. A no-op anywhere else.
do $$
declare
  old cron.job;
begin
  select * into old from cron.job where jobname = 'trilateration';
  if found then
    perform cron.unschedule(old.jobid);
    perform cron.schedule('terium', old.schedule, old.command);
  end if;
end;
$$;
