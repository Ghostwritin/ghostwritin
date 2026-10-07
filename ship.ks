# Ghostwritin API: a Cratefield Worker on Cloudflare, no database.
#
# NOTHING HERE RUNS YET. The Worker is not deployed and api.ghostwrit.in does
# not route anywhere (see the "Deploy the Worker" issue). This file is the
# pipeline the deploy will follow, written for Keep Shipping
# (https://keepshipping.run) the way the Owlpost backend's is: publish the
# Worker as a version with no traffic, smoke-test that version alone, shift
# traffic 0% -> 10% -> 100% with a health gate between steps, and put the
# previous version back on any failure.
#
# `fn.deploy` on Cloudflare Workers (Keep-Shipping/harness#77, #182) is not in
# Keep Shipping's catalog yet; `keepshipping check` treats an unknown kind as
# unchecked.
name:  ghostwritin-api
on:    manual

envs:
  staging:
    on:       manual
    secrets:  env
    vars:
      worker:    ghostwritin-api
      requires:  GHOSTWRITIN_API_KEYS, HARNESS_SECRET
  production:
    on:         manual
    secrets:    env
    approvers:  @nick
    vars:
      worker:    ghostwritin-api-production
      requires:  GHOSTWRITIN_API_KEYS, HARNESS_SECRET, ANTHROPIC_API_KEY

policy:
  agents:
    can:     check, build, deploy staging
    before:  deploy production
    ask:     @nick

steps:
  deploy:    fn.deploy                       # planned: Keep-Shipping/harness#77, #182
    host:      cloudflare
    function:  env.worker
    artifact:  ./crates/ghostwritin-worker   # wrangler.toml; built by its [build] command
    requires:  env.requires                  # secrets that must exist on the Worker (names only)
    smoke:     "curl -fsS https://$KS_FN_URL/v1/health"
    shift:     0%, 10%, 100%
    hold:      2m
    health:    "error rate below 1%"
    rollback:  auto
    keep:      previous
    token:     secrets.cloudflare_api_token
