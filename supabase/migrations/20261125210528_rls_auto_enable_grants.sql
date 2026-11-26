-- Supabase adds public.rls_auto_enable() to hosted projects for its ensure_rls event trigger, which
-- turns on RLS for new tables in public. Postgres does not check EXECUTE when an event trigger
-- fires, so the API roles can lose it without changing that. Not present on the local stack.
do $$
begin
  if to_regprocedure('public.rls_auto_enable()') is not null then
    revoke execute on function public.rls_auto_enable() from public, anon, authenticated;
  end if;
end;
$$;
