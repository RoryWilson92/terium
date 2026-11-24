create schema private;
create extension if not exists pg_cron;

-- One row: house bounds in metres, and the scan mode returned to each sensor.
create table public.site_config (
  id int primary key check (id = 1),
  scan_all_devices boolean not null default false,
  x_min real not null, x_max real not null,
  y_min real not null, y_max real not null,
  z_min real not null, z_max real not null,
  check (x_min <= x_max and y_min <= y_max and z_min <= z_max)
);

-- A board registers itself on first upload. Null coordinates exclude it from the solve.
create table public.sensors (
  id text primary key,
  x real, y real, z real,
  ref_rssi real not null default -45,
  path_loss real not null default 2.5 check (path_loss > 0),
  last_seen_at timestamptz
);

-- Median RSSI per sensor, device, and two-second slot. Not readable by clients.
create table public.observations (
  ts timestamptz,
  sensor_id text,
  device_mac text,
  rssi smallint not null,
  primary key (ts, sensor_id, device_mac)
);

-- Latest fix per device. error_m is the RMS of the distance residuals.
create table public.device_positions (
  device_mac text primary key,
  x real not null, y real not null, z real not null,
  error_m real not null,
  updated_at timestamptz not null
);

-- SHA-256 hex of the shared sensor secret. The plaintext stays on the boards.
create table private.secret (
  key_sha256 text primary key check (key_sha256 ~ '^[0-9a-f]{64}$')
);
-- ((true)) collapses every row onto one key, so a second secret cannot be inserted.
create unique index one_sensor_key on private.secret ((true));

alter table public.site_config enable row level security;
alter table public.sensors enable row level security;
alter table public.observations enable row level security;
alter table public.device_positions enable row level security;
alter table private.secret enable row level security;

-- Clients can read config, sensors, and positions. Observations and the secret stay closed.
revoke all on public.site_config, public.sensors, public.observations, public.device_positions
  from public, anon, authenticated;
grant select on public.site_config, public.sensors, public.device_positions to authenticated;

create policy read_site on public.site_config for select to authenticated using (true);
create policy read_sensors on public.sensors for select to authenticated using (true);
create policy read_positions on public.device_positions for select to authenticated using (true);

-- Checks the sensor key, upserts the sensor, and inserts bounded readings.
-- security definer with an empty search_path. Unexposed, so anon writes without table grants.
create function private.ingest(sensor_id text, readings jsonb) returns jsonb
language plpgsql security definer set search_path = '' as $$
begin
  -- PostgREST puts request headers in this setting. Compare the SHA-256 hex, not the secret.
  if not exists (
    select 1 from private.secret
    where key_sha256 = encode(sha256(convert_to(
      current_setting('request.headers', true)::json ->> 'x-sensor-key', 'UTF8'
    )), 'hex')
  ) then
    raise insufficient_privilege using message = 'Invalid sensor key';
  end if;

  if sensor_id is null or sensor_id !~ '^esp-[0-9a-f]{12}$' then
    raise invalid_parameter_value using message = 'Invalid sensor ID';
  end if;

  -- First contact creates the row with null coordinates. Later calls only refresh last_seen_at.
  insert into public.sensors (id, last_seen_at) values (sensor_id, now())
  on conflict (id) do update set last_seen_at = greatest(sensors.last_seen_at, excluded.last_seen_at);

  -- Even slot from the last five minutes, integer RSSI in -100..-1, at most 2000 rows.
  -- Sensor MACs are skipped. A retried row hits the primary key and is ignored.
  insert into public.observations (ts, sensor_id, device_mac, rssi)
  select to_timestamp(r.ts::double precision), ingest.sensor_id, r.device, r.rssi
  from jsonb_to_recordset(readings) as r(device text, ts numeric, rssi numeric)
  where r.device ~ '^[0-9a-f]{12}$'
    and r.ts between extract(epoch from now()) - 300 and extract(epoch from now()) + 300
    and mod(r.ts, 2) = 0
    and r.rssi between -100 and -1 and r.rssi = trunc(r.rssi)
    and not exists (select 1 from public.sensors s where s.id = 'esp-' || r.device)
  limit 2000
  on conflict do nothing;

  return jsonb_build_object('scan_all_devices', coalesce(
    (select scan_all_devices from public.site_config where id = 1), false
  ));
end;
$$;

-- Data API entry point (POST /rest/v1/rpc/ingest). Invoker rights; the writes happen in private.ingest.
create function public.ingest(sensor_id text, readings jsonb) returns jsonb
language sql security invoker set search_path = '' as $$
  select private.ingest(sensor_id, readings);
$$;

revoke all on function private.ingest(text, jsonb), public.ingest(text, jsonb)
  from public, anon, authenticated;
-- anon must be able to name private.ingest from the invoker wrapper. It has no table grants.
grant usage on schema private to anon;
grant execute on function private.ingest(text, jsonb), public.ingest(text, jsonb) to anon;

-- Weighted Gauss-Newton in 3D. Needs four or more distances and a site_config row.
-- Each iterate is clamped to the site bounds, which also rejects a below-floor mirror.
-- error_m is the RMS residual. Returns no row when it cannot run.
create function private.solve(xs real[], ys real[], zs real[], ds real[])
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

    -- Cramer's rule via the cofactors shared by the three numerators. A tiny det means stop.
    aa := d*f - e*e; ab := c*e - b*f; ac := b*e - c*d;
    dd := a*f - c*c; de := b*c - a*e; ff := a*d - b*b;
    det := a*aa + b*ab + c*ac;
    exit when det <= 1e-12;
    px := px + (aa*u + ab*v + ac*w) / det;
    py := py + (ab*u + dd*v + de*w) / det;
    pz := pz + (ac*u + de*v + ff*w) / det;
  end loop;

  return query select px::real, py::real, pz::real,
    sqrt(avg((sqrt((px-sx)^2 + (py-sy)^2 + (pz-sz)^2) - dist)^2))::real
  from unnest(xs, ys, zs, ds) as s(sx, sy, sz, dist);
end;
$$;

-- Cron entry point. Turn the last 5 seconds of RSSI into positions, then drop rows older than an hour.
create function private.compute_positions() returns void
language sql set search_path = '' as $$
  insert into public.device_positions (device_mac, x, y, z, error_m, updated_at)
  with distances as (
    select o.device_mac, s.id, s.x, s.y, s.z,
      -- d = 10 ^ ((ref_rssi - rssi) / (10 * path_loss)), clamped to 0.3-25 m.
      least(25, greatest(0.3, 10 ^ ((s.ref_rssi - avg(o.rssi)) / (10 * s.path_loss))))::real as d
    from public.observations o join public.sensors s on s.id = o.sensor_id
    -- Placed sensors only. A MAC that belongs to a board is not a phone.
    where o.ts between now() - interval '5 seconds' and now()
      and s.x is not null and s.y is not null and s.z is not null
      and not exists (select 1 from public.sensors own where own.id = 'esp-' || o.device_mac)
    group by o.device_mac, s.id
  ), devices as (
    -- A 3D fix needs four ranges.
    select device_mac, array_agg(x order by id) as xs, array_agg(y order by id) as ys,
      array_agg(z order by id) as zs, array_agg(d order by id) as ds
    from distances group by device_mac having count(*) >= 4
  )
  select device_mac, p.x, p.y, p.z, p.error_m, now()
  from devices cross join lateral private.solve(xs, ys, zs, ds) p
  on conflict (device_mac) do update
  set (x, y, z, error_m, updated_at) =
      (excluded.x, excluded.y, excluded.z, excluded.error_m, excluded.updated_at);

  -- Positions, observations, and pg_cron history are all kept for one hour.
  delete from public.device_positions where updated_at < now() - interval '1 hour';
  delete from public.observations where ts < now() - interval '1 hour';
  delete from cron.job_run_details where end_time < now() - interval '1 hour';
$$;

revoke all on function private.solve(real[], real[], real[], real[]),
  private.compute_positions() from public, anon, authenticated;
