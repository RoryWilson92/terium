begin;
create extension if not exists pgtap with schema extensions;
set local search_path = public, extensions;
select plan(6);

-- Ideal distances to (6, 5, z) for z in 1, 6, and -2. The negative height must clamp to the floor.
select ok(case when p.z < 0 then s.z = 0
               else sqrt((s.x-6)^2 + (s.y-5)^2 + (s.z-p.z)^2) < 0.1 end,
          case when p.z < 0 then 'clamps a below-floor estimate'
               else 'recovers point at height ' || p.z end)
from (values (1), (6), (-2)) as p(z)
cross join lateral (
  select array_agg(x order by id) as xs, array_agg(y order by id) as ys,
         array_agg(z order by id) as zs,
         array_agg(sqrt((x-6)^2 + (y-5)^2 + (z-p.z)^2)::real order by id) as ds
  from public.sensors
) a
cross join lateral private.solve(a.xs, a.ys, a.zs, a.ds) s;

update public.site_config set scan_all_devices = true;
-- anon is the role PostgREST uses for a sensor. The key is the local seed secret.
set local role anon;
set local request.headers = '{"x-sensor-key":"wrong"}';
select throws_ok(
  $$select public.ingest('esp-a1b2c3d4e5f6', '[]')$$,
  '42501', 'Invalid sensor key', 'rejects the wrong key'
);

set local request.headers = '{"x-sensor-key":"local-test-key"}';
select is(
  public.ingest('esp-a1b2c3d4e5f6', jsonb_build_array(jsonb_build_object(
    'device', '3a7bd3e2360a', 'ts', floor(extract(epoch from now()) / 2) * 2, 'rssi', -52
  ))),
  '{"scan_all_devices":true}'::jsonb, 'accepts the right key and returns the flag'
);

reset role;
select is(
  (select rssi from public.observations where sensor_id = 'esp-a1b2c3d4e5f6'),
  -52::smallint, 'stores the reading'
);

select * from finish();
rollback;
