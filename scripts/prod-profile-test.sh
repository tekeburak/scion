#!/bin/sh
# Brings deploy/compose.prod.yml up with Caddy's local certificate authority
# and checks that `scn`, given a token, inits, clones, commits and lands
# through the one https:// address. The image
# ghcr.io/tekeburak/scion-server:<version> must already be loaded; the bucket
# comes from a RustFS added for the test, and Caddy's port is a free one.
#
#   scripts/prod-profile-test.sh <version> <path to scn>
set -eu

version="$1"
scn="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"
root="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d)"
project="scion-prod-test-$$"

# Compose prefers the shell's variables to the env file's.
unset SCION_VERSION SCION_DOMAIN POSTGRES_PASSWORD SCION_TOKEN_KEY AWS_ENDPOINT AWS_ALLOW_HTTP \
	AWS_BUCKET AWS_REGION AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY SCION_GITHUB_CLIENT_ID \
	SCION_GITHUB_CLIENT_SECRET
cat >"$tmp/prod.env" <<EOF
SCION_VERSION=$version
SCION_DOMAIN=localhost
POSTGRES_PASSWORD=$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')
SCION_TOKEN_KEY=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
AWS_ENDPOINT=http://rustfs:9000
AWS_ALLOW_HTTP=true
AWS_BUCKET=scion-objects
AWS_REGION=us-east-1
AWS_ACCESS_KEY_ID=scion
AWS_SECRET_ACCESS_KEY=scionscion
EOF

# The test's own bucket, and Caddy on a free loopback port instead of 80 and 443.
cat >"$tmp/test.yml" <<'EOF'
services:
  rustfs:
    image: rustfs/rustfs:latest
    environment:
      RUSTFS_ACCESS_KEY: scion
      RUSTFS_SECRET_KEY: scionscion
      RUSTFS_VOLUMES: /data
      RUSTFS_ADDRESS: ":9000"
  bucket-init:
    image: amazon/aws-cli:latest
    depends_on: [rustfs]
    environment:
      AWS_ACCESS_KEY_ID: scion
      AWS_SECRET_ACCESS_KEY: scionscion
      AWS_DEFAULT_REGION: us-east-1
    entrypoint: >
      /bin/sh -c "
        for i in $$(seq 1 30); do
          aws --endpoint-url http://rustfs:9000 s3 ls >/dev/null 2>&1 && break; sleep 1;
        done;
        aws --endpoint-url http://rustfs:9000 s3 mb s3://scion-objects
      "
  scion-server:
    depends_on:
      bucket-init:
        condition: service_completed_successfully
  caddy:
    ports: !override
      - "127.0.0.1::443"
EOF

compose() {
	docker compose -p "$project" -f "$root/deploy/compose.prod.yml" -f "$tmp/test.yml" \
		--env-file "$tmp/prod.env" "$@"
}
cleanup() {
	status=$?
	[ "$status" -eq 0 ] || compose logs --tail 30 scion-server caddy rustfs >&2 || true
	compose down -v >/dev/null 2>&1 || true
	rm -rf "$tmp"
}
trap cleanup EXIT

compose up -d --quiet-pull
port="$(compose port caddy 443 | sed 's/.*://')"
url="https://localhost:$port"

# Caddy makes its authority when it starts; clients trust it the way they would
# trust any private one.
for _ in $(seq 1 30); do
	compose exec -T caddy cat /data/caddy/pki/authorities/local/root.crt >"$tmp/root.crt" 2>/dev/null && break
	sleep 1
done
export SSL_CERT_FILE="$tmp/root.crt"
for _ in $(seq 1 60); do
	curl -sf -o /dev/null --cacert "$tmp/root.crt" "$url/" && break
	sleep 1
done
curl -sf -o /dev/null --cacert "$tmp/root.crt" "$url/" || { echo "FAIL: no web interface at $url" >&2; exit 1; }

compose exec -T scion-server scion-server grant '*' sp:prod admin
SCN_TOKEN="$(compose exec -T scion-server scion-server token sp:prod --ttl 1h)"
export SCN_TOKEN SCN_AUTHOR="Prod Test <prod@example.com>"

"$scn" whoami "$url"
"$scn" init "$url" prod-test
cd "$tmp"
"$scn" clone "$url" prod-test work
cd work
echo "through the proxy" >README.md
"$scn" add README.md
"$scn" commit -m "first change through the production profile"
"$scn" land
cd "$tmp"
"$scn" clone "$url" prod-test check
[ "$(cat check/README.md)" = "through the proxy" ] || { echo "FAIL: main does not hold the landed change" >&2; exit 1; }
echo "ok: login, init, clone, commit and land through $url"
