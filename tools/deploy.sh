#!/usr/bin/env bash
# Deploys the open-source Worker: staging (the top-level environment) or
# production (api.ghostwrit.in). Refuses to deploy an environment whose
# secrets are not all set, records the version it can roll back to, and
# smoke-tests /v1/health — including that the answer names the workspace
# version — rolling back when it does not.
#
#   tools/deploy.sh staging      # STAGING_URL must say where staging is
#   tools/deploy.sh production   # https://api.ghostwrit.in
#
# WRANGLER overrides the pinned package (default wrangler@4.149.0).
set -euo pipefail

env="${1:?usage: tools/deploy.sh <staging|production>}"
case "$env" in
	# The top-level environment, named with an explicit empty --env (the
	# config also carries [env.production], so wrangler wants the intent said).
	# flag repeats it for the human-facing advice below.
	staging)    env_args=(--env ""); flag=' --env ""' ;;
	production) env_args=(--env production); flag=' --env production' ;;
	*) printf 'no such environment: %s (staging or production)\n' "$env" >&2; exit 2 ;;
esac
spec="${WRANGLER:-wrangler@4.149.0}"

# Wrangler finds wrangler.toml only at or above its working directory, so run
# from the crate that owns the config, not from the workspace root.
cd "$(dirname "$0")/../crates/ghostwritin-worker"
# The pin the smoke test holds the deploy to: the Worker must answer with
# the workspace's own version, so stale code fails the gate.
workspace_version="$(sed -n 's/^version = "\(.*\)"/\1/p' ../../Cargo.toml | head -1)"

wrangler() { npx --yes "$spec" "$@"; }

# The secrets this environment needs, by name only — never their values.
# One provider key is enough: the Worker prefers Anthropic and falls back
# to an OpenAI-compatible endpoint (text_model in its lib.rs), and the key
# is what makes model_configured true, which the smoke test asserts.
missing_secrets() {
	local secrets required
	# Wrangler's own error (e.g. "Worker … not found" before the first deploy)
	# goes straight to stderr; only its stdout is captured here. `wrangler
	# secret put` creates a missing Worker and answers yes to that prompt in
	# CI, so the hint is to just run it.
	if ! secrets="$(wrangler secret list "${env_args[@]}" --format json)"; then
		# shellcheck disable=SC2016  # the backticks are printed, not run
		printf 'wrangler secret list failed — if the Worker does not exist yet (first deploy), `wrangler secret put <NAME>%s` creates it; then re-run\n' "$flag" >&2
		return 1
	fi
	required=(GHOSTWRITIN_API_KEYS HARNESS_SECRET)
	# shellcheck disable=SC2016  # the script is passed to node verbatim
	node -e '
		const listed = new Set(JSON.parse(process.argv[1]).map((s) => s.name));
		const names = process.argv.slice(2);
		const providers = names.slice(-2);
		const missing = names.slice(0, -2).filter((n) => !listed.has(n));
		if (!providers.some((n) => listed.has(n)))
			missing.push(`${providers[0]} or ${providers[1]} (either one)`);
		process.exit(missing.length === 0
			? 0
			: (console.error(`not set: ${missing.join(", ")}`), 1));' \
		"$secrets" "${required[@]}" ANTHROPIC_API_KEY OPENAI_API_KEY
}

smoke() {
	local url="$1" attempt
	for attempt in 1 2 3 4 5; do
		if curl -fsS "$url/v1/health" 2>/dev/null | node -e '
			let raw = "";
			process.stdin.on("data", (c) => (raw += c));
			process.stdin.on("end", () => {
				let health;
				try { health = JSON.parse(raw); } catch { process.exit(1); }
				process.exit(health.ok === true
					&& health.model_configured === true
					&& health.version === process.argv[1] ? 0 : 1);});' \
			"$workspace_version" 2>/dev/null;
		then
			return 0
		fi
		printf 'smoke test attempt %s failed against %s\n' "$attempt" "$url" >&2
		sleep 3
	done
	return 1
}

printf 'checking the secrets %s needs…\n' "$env"
if ! missing_secrets; then
	# The advice repeats the deploy's own --env: staging is the top-level
	# environment (an explicit empty --env), production [env.production].
	# shellcheck disable=SC2016  # the backticks are printed, not run
	printf 'set them with `wrangler secret put <NAME>%s`, then re-run\n' "$flag"
	exit 1
fi

# The list is printed oldest-first, and a deployment carries the versions it
# serves in `versions` — there is no `version` field to read. A failed listing
# (no Worker yet, a transient API error) only means there is nothing to roll
# back to: its stderr is dropped and previous_version comes out empty.
previous_version="$(wrangler deployments list "${env_args[@]}" --json 2>/dev/null | node -e '
	let raw = "";
	process.stdin.on("data", (c) => (raw += c));
	process.stdin.on("end", () => {
		try {
			const list = JSON.parse(raw);
			const items = Array.isArray(list) ? list : list.items ?? [];
			const latest = items.at(-1) ?? {};
			process.stdout.write(latest.versions?.[0]?.version_id ?? "");
		} catch { process.stdout.write(""); }
	});' || true)"

case "$env" in
	# The workers.dev subdomain is not chosen yet, so staging's URL comes
	# from the caller — before the deploy, not after it.
	staging)    url="${STAGING_URL:?set STAGING_URL to the staging base URL}" ;;
	production) url="https://api.ghostwrit.in" ;;
esac

printf 'deploying %s (rolling back to %s if the smoke test fails)…\n' \
	"$env" "${previous_version:-nothing, there is no earlier version}"
# The deploy runs the config's [build] command, which shells out to this.
if ! command -v worker-build >/dev/null; then
	# shellcheck disable=SC2016  # the backticks are printed, not run
	printf 'worker-build is not on PATH — install it with `cargo install worker-build` (and `rustup target add wasm32-unknown-unknown`)\n' >&2
	exit 1
fi
wrangler deploy "${env_args[@]}"

if smoke "$url"; then
	printf '%s is healthy at %s (version %s)\n' "$env" "$url" "$workspace_version"
	exit 0
fi

printf 'the smoke test failed\n' >&2
if [ -n "$previous_version" ]; then
	wrangler rollback "$previous_version" "${env_args[@]}" \
		--message "tools/deploy.sh: the /v1/health smoke test failed" -y
else
	printf 'there is no earlier version to roll back to; %s stays broken\n' "$env" >&2
fi
exit 1
