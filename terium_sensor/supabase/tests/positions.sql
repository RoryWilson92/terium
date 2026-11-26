begin;
create extension if not exists pgtap with schema extensions;
set local search_path = public, extensions;
select plan(16);

-- Ideal distances to a ground-floor point, an upstairs point, and one below the floor.
-- No estimate may be below the floor.
select ok(case when p.z < 0 then s.z >= 0
               else sqrt((s.x-p.x)^2 + (s.y-p.y)^2 + (s.z-p.z)^2) < 0.1 end,
          case when p.z < 0 then 'never places an estimate below the floor'
               else 'recovers point at height ' || p.z end)
from (values (4.0, 6.0, 1.0), (6.0, 3.0, 4.0), (5.6, 4.7, -2.0)) as p(x, y, z)
cross join lateral (
  select array_agg(x order by id) as xs, array_agg(y order by id) as ys,
         array_agg(z order by id) as zs,
         array_agg(sqrt((x-p.x)^2 + (y-p.y)^2 + (z-p.z)^2)::real order by id) as ds
  from public.sensors
) a
cross join lateral private.solve(a.xs, a.ys, a.zs, a.ds) s;

-- Variances 1/3, 1/3 and 1/12 on the axes, so the thinnest spread is sqrt(1/12).
select ok(abs(private.thinnest_spread(
  array[1, -1, 0, 0, 0, 0], array[0, 0, 1, -1, 0, 0], array[0, 0, 0, 0, 0.5, -0.5]
) - sqrt(1.0 / 12)) < 1e-4, 'measures the thinnest spread of the anchors');

-- Four anchors at one height determine a point only up to its mirror, so nothing is published.
select is_empty(
  $$select * from private.solve(array[0, 11, 0, 11], array[0, 0, 9, 9], array[1, 1, 1, 1],
                                array[5, 7, 6, 8])$$,
  'publishes nothing for coplanar anchors'
);

-- anon is the role PostgREST uses for a sensor. The key is the local seed secret.
create temp table expected as select config_version from site_config;
grant select on expected to anon;
set local role anon;
set local request.headers = '{"x-sensor-key":"wrong"}';
select throws_ok(
  $$select public.ingest('esp-a1b2c3d4e5f6', '[]', 0, 6, 'test')$$,
  '42501', 'Invalid sensor key', 'rejects the wrong key'
);

set local request.headers = '{"x-sensor-key":"local-test-key"}';
select is(
  public.ingest('esp-a1b2c3d4e5f6', '[]', 0, 6, 'test') - 'peers',
  jsonb_build_object('config', (select config_version from expected), 'mode', 'device',
                     'scan_all_devices', false),
  'accepts the right key and returns the config'
);

reset role;
select is(
  (select (applied_config, channel, firmware)::text from sensors where id = 'esp-a1b2c3d4e5f6'),
  '(0,6,test)', 'records the applied config, channel and firmware'
);

-- An unplaced board's readings are not stored. Once placed, they are.
set local role anon;
select public.ingest('esp-a1b2c3d4e5f6', jsonb_build_array(jsonb_build_object(
  'device', '3a7bd3e2360a', 'ts', floor(extract(epoch from now()) / 2) * 2, 'rssi', -51
)), 0, 6, 'test');
reset role;
update sensors set x = 1, y = 1, z = 1 where id = 'esp-a1b2c3d4e5f6';
set local role anon;
select public.ingest('esp-a1b2c3d4e5f6', jsonb_build_array(jsonb_build_object(
  'device', '3a7bd3e2360a', 'ts', floor(extract(epoch from now()) / 2) * 2, 'rssi', -52
)), 0, 6, 'test');
reset role;
select is(
  (select array_agg(rssi) from observations where sensor_id = 'esp-a1b2c3d4e5f6'),
  array[-52::smallint], 'stores readings only from a placed sensor'
);

-- Readings from the eight placed example sensors give a fix that records when it was captured.
insert into observations (ts, sensor_id, device_mac, rssi)
select now() - interval '2 seconds', id, '0a0b0c0d0e0f', -60
from sensors where id like 'esp-down-%' or id like 'esp-up-%';
select private.compute_positions();
select is(
  (select captured_at from device_positions where device_mac = '0a0b0c0d0e0f'),
  now() - interval '2 seconds', 'publishes a fix with its capture time'
);

-- A switch to Person mode keeps fixes, and readings are no longer stored.
update site_config set mode = 'person';
select is((select count(*) from device_positions), 1::bigint, 'keeps fixes on a switch');
set local role anon;
select is(
  public.ingest('esp-a1b2c3d4e5f6', jsonb_build_array(jsonb_build_object(
    'device', '3a7bd3e2360a', 'ts', floor(extract(epoch from now()) / 2) * 2 + 2, 'rssi', -53
  )), 0, 6, 'test') ->> 'mode',
  'person', 'tells the board to switch'
);
reset role;
-- Each phone's last fix is kept for a week, in either mode.
insert into device_positions (device_mac, x, y, z, error_m, captured_at, updated_at) values
  ('0a0b0c0d0e01', 1, 1, 1, 1, now() - interval '6 days 23 hours', now() - interval '6 days 23 hours'),
  ('0a0b0c0d0e02', 1, 1, 1, 1, now() - interval '7 days 1 hour', now() - interval '7 days 1 hour');
select private.compute_positions();
select is(
  (select count(*) from observations where rssi = -53), 0::bigint,
  'ignores readings in Person mode'
);
select is(
  (select array_agg(device_mac order by device_mac) from device_positions),
  array['0a0b0c0d0e01', '0a0b0c0d0e0f'], 'keeps each last fix for a week'
);

-- Back in Device mode, a late reading captured before the switch is dropped.
update site_config set mode = 'device';
set local role anon;
select public.ingest('esp-a1b2c3d4e5f6', jsonb_build_array(
  jsonb_build_object('device', '3a7bd3e2360a', 'ts', floor(extract(epoch from now()) / 2) * 2 - 2, 'rssi', -54),
  jsonb_build_object('device', '3a7bd3e2360a', 'ts', floor(extract(epoch from now()) / 2) * 2 + 2, 'rssi', -55)
), 0, 6, 'test');
reset role;
select is(
  (select array_agg(rssi order by ts) from observations where rssi in (-54, -55)),
  array[-55::smallint], 'drops readings captured before the switch'
);

-- A board's own MAC is never a device.
set local role anon;
select public.ingest('esp-a1b2c3d4e5f6', jsonb_build_array(jsonb_build_object(
  'device', 'a1b2c3d4e5f6', 'ts', floor(extract(epoch from now()) / 2) * 2 + 2, 'rssi', -56
)), 0, 6, 'test');
reset role;
select is(
  (select count(*) from observations where device_mac = 'a1b2c3d4e5f6'), 0::bigint,
  'skips sensor MACs'
);

select * from finish();
rollback;
