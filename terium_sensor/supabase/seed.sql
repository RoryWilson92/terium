-- Local only. Bounds are the house envelope in metres, with z up from the ground-floor datum.
-- The eight sensors are an example four-down / four-up layout at varied heights, with the upper
-- floor at about 2.7 m. Production boards register themselves; place those with UPDATE.
insert into public.site_config (id, x_min, x_max, y_min, y_max, z_min, z_max)
values (1, 0, 11.2, 0, 9.4, 0, 5.3);

insert into private.secret values (encode(sha256(convert_to('local-test-key', 'UTF8')), 'hex'));

insert into public.sensors (id, x, y, z) values
  ('esp-down-1', 0.2, 0.2, 0.4), ('esp-down-2', 11.0, 0.2, 1.8),
  ('esp-down-3', 5.6, 4.7, 1.2), ('esp-down-4', 0.2, 9.2, 2.2),
  ('esp-up-1', 11.0, 9.2, 3.1), ('esp-up-2', 0.2, 4.7, 4.6),
  ('esp-up-3', 5.6, 0.2, 3.9), ('esp-up-4', 8.4, 6.5, 4.9);
