begin;
create extension if not exists pgtap with schema extensions;
set local search_path = public, extensions;
select plan(16);

-- A signed-in app user. Signups are disabled, so every such user is provisioned by hand.
set local request.jwt.claims = '{"sub":"00000000-0000-0000-0000-000000000001","role":"authenticated"}';
create temp table before as select config_version, layout from site_config;
grant select on before to authenticated;

set local role authenticated;
update site_config set mode = 'person';
select is(
  (select config_version from site_config), (select config_version + 1 from before),
  'a mode switch is a new config for the boards'
);
select throws_ok(
  $$update site_config set config_version = 99$$, '42501', null,
  'the app cannot set config_version'
);
select throws_ok(
  $$update sensors set x = 20 where id = 'esp-down-1'$$, '23514',
  'Sensor position is outside the site bounds', 'rejects a position outside the site'
);
select throws_ok(
  $$update sensors set x = null where id = 'esp-down-1'$$, '23514',
  'Set x, y and z together, or clear all three', 'rejects a partial position'
);
update sensors set x = 0.5 where id = 'esp-down-1';
select isnt(
  (select layout from site_config), (select layout from before),
  'moving a sensor makes a new layout'
);
update sensors set x = 0.2 where id = 'esp-down-1';
select is(
  (select layout from site_config), (select layout from before),
  'moving it back restores the old layout'
);
update sensors set enabled = false where id = 'esp-up-4';
select is(
  (select count(*) from sensors where enabled), 7::bigint, 'the app can disable a sensor'
);
reset role;

-- The worker publishes with the sensor key, as anon.
set local role anon;
set local request.headers = '{"x-sensor-key":"wrong"}';
select throws_ok(
  $$select public.publish_snapshot(null)$$, '42501', 'Invalid sensor key',
  'rejects the wrong key'
);
set local request.headers = '{"x-sensor-key":"local-test-key"}';
select is(
  jsonb_array_length(public.publish_snapshot(null) -> 'sensors'), 7,
  'returns the participating sensors'
);
select throws_ok(
  format($$select public.publish_snapshot(%L)$$, jsonb_build_object(
    'status', 'ready', 'layout', public.publish_snapshot(null) ->> 'layout',
    'captured_at', extract(epoch from now()), 'ttl', 10,
    'targets', jsonb_build_array(jsonb_build_object('x', 50, 'y', 1, 'z', 1, 'confidence', 0.9, 'kind', 'person'))
  )), '22023', 'Invalid snapshot', 'rejects a target outside the site'
);
select throws_ok(
  format($$select public.publish_snapshot(%L)$$, jsonb_build_object(
    'status', 'insufficient_signal', 'layout', public.publish_snapshot(null) ->> 'layout',
    'captured_at', extract(epoch from now()), 'ttl', 10,
    'targets', jsonb_build_array(jsonb_build_object('x', 1, 'y', 1, 'z', 1, 'confidence', 0.9, 'kind', 'person'))
  )), '22023', 'Invalid snapshot', 'rejects a guessed coordinate with weak evidence'
);
select is(
  public.publish_snapshot(jsonb_build_object(
    'status', 'ready', 'layout', 'old-layout', 'captured_at', extract(epoch from now()), 'ttl', 10,
    'targets', '[]'::jsonb
  )) -> 'accepted', 'false'::jsonb, 'ignores a snapshot for another layout'
);
select is(
  public.publish_snapshot(jsonb_build_object(
    'status', 'ready', 'layout', public.publish_snapshot(null) ->> 'layout',
    'captured_at', extract(epoch from now()), 'ttl', 10,
    'targets', jsonb_build_array(jsonb_build_object('x', 3.21, 'y', 4, 'z', 1.2, 'confidence', 0.8, 'kind', 'person'))
  )) -> 'accepted', 'true'::jsonb, 'accepts a snapshot for the current layout'
);
reset role;

set local role authenticated;
select is(
  (select targets from person_snapshot),
  '[{"x": 3.21, "y": 4, "z": 1.2, "kind": "person", "confidence": 0.8}]'::jsonb,
  'the app reads the latest snapshot'
);
select is(
  (select live_until - captured_at from person_snapshot), interval '10 seconds',
  'the snapshot is live for its ttl'
);
update site_config set mode = 'device';
select is(
  (select targets from person_snapshot),
  '[{"x": 3.21, "y": 4, "z": 1.2, "kind": "person", "confidence": 0.8}]'::jsonb,
  'a switch keeps the last snapshot'
);
reset role;

select * from finish();
rollback;
