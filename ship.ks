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
# unchecked. Until it lands, tools/deploy.sh runs this pipeline by hand:
#   tools/deploy.sh staging      # then tools/deploy.sh production
# (it checks the secrets, records the previous version, deploys, and rolls
# back on a failed smoke test — the same steps, one shell script).
name:  ghostwritin-api
on:    manual

envs:
  staging:
    on:       manual
    secrets:  env
    vars:
      worker:    ghostwritin-api
      requires:  GHOSTWRITIN_API_KEYS, HARNESS_SECRET, ANTHROPIC_API_KEY  # or OPENAI_API_KEY
  production:
    on:         manual
    secrets:    env
    approvers:  @nick
    vars:
      worker:    ghostwritin-api-production
      requires:  GHOSTWRITIN_API_KEYS, HARNESS_SECRET, ANTHROPIC_API_KEY  # or OPENAI_API_KEY

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
    # The gate pins the deploy: /v1/health must answer 200 with ok true,
    # model_configured true, and version equal to the workspace version —
    # read from the workspace root's Cargo.toml by way of git, since the
    # step runs in the artifact dir, whose own Cargo.toml only says
    # `version.workspace`. The same assertions tools/deploy.sh makes with
    # node: a deploy that shipped stale code (or no provider key) fails
    # here instead of taking traffic.
    smoke:     "h=\"$(curl -fsS https://$KS_FN_URL/v1/health)\" && printf '%s' \"$h\" | grep -q '\"ok\":true' && printf '%s' \"$h\" | grep -q '\"model_configured\":true' && printf '%s' \"$h\" | grep -q \"\\\"version\\\":\\\"$(sed -n 's/^version = \"\\(.*\\)\"/\\1/p' \"$(git rev-parse --show-toplevel)/Cargo.toml\" | head -1)\\\"\""
    shift:     0%, 10%, 100%
    hold:      2m
    health:    "error rate below 1%"
    rollback:  auto
    keep:      previous
    token:     secrets.cloudflare_api_token

# The Worker throttles itself too, so the gate above is not the only line of
# defense: wrangler.toml wires a REWRITE_LIMITER rate-limit binding (10
# rewrites / 60 s per key — the caller's hashed bearer token, else their IP)
# in front of POST /v1/rewrite, answering 429 rate-limited with
# Retry-After: 60. Over the limit is a refundable client error, not an
# incident; the "error rate below 1%" gate counts 5xx, not 429s.
