-- Local only. Bounds are metres. The six sensors are an example 3-down / 3-up layout.
-- Production boards register themselves; place those with UPDATE.
insert into public.site_config (id, x_min, x_max, y_min, y_max, z_min, z_max)
values (1, 0, 12, 0, 10, 0, 10);

insert into private.secret values (encode(sha256(convert_to('local-test-key', 'UTF8')), 'hex'));

insert into public.sensors (id, x, y, z) values
  ('esp-down-1', 0, 0, 1), ('esp-down-2', 12, 5, 1), ('esp-down-3', 0, 10, 1),
  ('esp-up-1', 12, 0, 6), ('esp-up-2', 0, 5, 6), ('esp-up-3', 12, 10, 6);
