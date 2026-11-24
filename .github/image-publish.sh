#!/usr/bin/env bash
# Build, check and push one image per plugin to registry.abt.me (ferrobill-<plugin>), from
# docker/Dockerfile's targets. Run by ci.yml's image job on pushes to main (tags main-<sha> and
# latest) and v* tags (tag <version>).
# The registry login comes from OpenBao (secret/cp03/docker-registry, role
# ferrobill-plugins-image-publish on auth/jwt-github, main and v* tag pushes only; set up by talos
# scripts/create-ferrobill-plugins-image-publish-role.sh). The build image comes from the same
# registry, so nothing is pulled from a public registry.
set -euo pipefail

: "${BAO_ADDR:?BAO_ADDR must be set (repository secret)}"
: "${ACTIONS_ID_TOKEN_REQUEST_URL:?no GitHub OIDC token - the job needs permissions: id-token: write}"

REGISTRY=registry.abt.me
BUILDER="$REGISTRY/docker-gen:rust-ci-latest"
PLUGINS=(directadmin hestiacp nominet enom netearthone gandi domainnameapi cloudflare)
w=$(mktemp -d)
trap 'rm -rf "$w"; docker logout "$REGISTRY" >/dev/null 2>&1 || true' EXIT

# --- registry login from OpenBao
curl -fsS -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
  "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=$BAO_ADDR" | jq -r .value >"$w/oidc"
jq -n --rawfile jwt "$w/oidc" '{role: "ferrobill-plugins-image-publish", jwt: ($jwt | rtrimstr("\n"))}' >"$w/login.json"
rm -f "$w/oidc"
curl -fsS -X POST --data @"$w/login.json" "$BAO_ADDR/v1/auth/jwt-github/login" | jq -r .auth.client_token >"$w/token"
rm -f "$w/login.json"
echo "::add-mask::$(cat "$w/token")"
printf 'X-Vault-Token: %s\n' "$(cat "$w/token")" >"$w/header"
curl -fsS -H @"$w/header" "$BAO_ADDR/v1/secret/data/cp03/docker-registry" >"$w/secret.json"
rm -f "$w/token" "$w/header"
user=$(jq -r '.data.data["htpasswd-username"]' "$w/secret.json")
jq -r '.data.data["htpasswd-password"]' "$w/secret.json" >"$w/pass"
rm -f "$w/secret.json"
echo "::add-mask::$(cat "$w/pass")"
docker login "$REGISTRY" --username "$user" --password-stdin <"$w/pass" >/dev/null
rm -f "$w/pass"

# --- tags
sha=$(git rev-parse --short=12 HEAD)
case "${GITHUB_REF:-}" in
  refs/tags/v*) version=${GITHUB_REF#refs/tags/v}; suffixes=("$version") ;;
  refs/heads/main) version="main-$sha"; suffixes=("main-$sha" "latest") ;;
  *) echo "not a main or v* tag push: nothing to publish"; exit 0 ;;
esac

# --- build and check each image: it starts (and stops at once, asking for its settings), and
# there is no shell in it. The shared build stage is built once and cached for the rest.
pushed=()
for name in "${PLUGINS[@]}"; do
  image="$REGISTRY/ferrobill-$name"
  tags=()
  for s in "${suffixes[@]}"; do tags+=("$image:$s"); done
  args=()
  for t in "${tags[@]}"; do args+=(--tag "$t"); done
  docker build --file docker/Dockerfile --target "$name" --build-arg "BUILDER_IMAGE=$BUILDER" \
    --build-arg "VERSION=$version" --label "org.opencontainers.image.revision=$sha" "${args[@]}" .
  out=$(docker run --rm "${tags[0]}" 2>&1 || true)
  if ! grep -q "is not set" <<<"$out"; then
    echo "$image didn't start as expected: $out" >&2
    exit 1
  fi
  if docker run --rm --entrypoint /bin/sh "${tags[0]}" -c true >/dev/null 2>&1; then
    echo "$image has a shell; it should be scratch" >&2
    exit 1
  fi
  docker image inspect "${tags[0]}" --format 'user={{.Config.User}} size={{.Size}}'
  pushed+=("${tags[@]}")
done

# --- push (only once all built and passed their checks)
for t in "${pushed[@]}"; do docker push "$t"; done
echo "published: ${pushed[*]}"
