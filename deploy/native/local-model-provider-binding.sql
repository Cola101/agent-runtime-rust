begin;

select set_config('app.tenant_id', '11111111-1111-4111-8111-111111111111', true);

delete from model_policy_candidates
 where tenant_id = '11111111-1111-4111-8111-111111111111'
   and application_id = '22222222-2222-4222-8222-222222222222'
   and model_policy_id = '88888888-8888-4888-8888-888888888888';

insert into model_policy_candidates (
  tenant_id, application_id, model_policy_id, provider_id, priority)
values (
  '11111111-1111-4111-8111-111111111111',
  '22222222-2222-4222-8222-222222222222',
  '88888888-8888-4888-8888-888888888888',
  :'provider_id',
  0);

delete from model_providers provider
 where provider.tenant_id = '11111111-1111-4111-8111-111111111111'
   and provider.application_id = '22222222-2222-4222-8222-222222222222'
   and provider.name like 'Native Local Provider %'
   and provider.id <> :'provider_id'
   and not exists (
     select 1
       from model_policy_candidates candidate
      where candidate.tenant_id = provider.tenant_id
        and candidate.provider_id = provider.id);

commit;
