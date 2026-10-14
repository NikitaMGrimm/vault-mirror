#!/usr/bin/env bash
# Starts the sandbox servers. Creates a throwaway certificate authority and a
# certificate for 127.0.0.1 on first use; both stay in sandbox/tls, which is
# not committed.
set -euo pipefail
cd "$(dirname "$0")"

if [[ ! -f tls/cert.pem ]]; then
  mkdir -p tls
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 \
    -subj "/CN=vault-mirror sandbox CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign" \
    -keyout tls/ca-key.pem -out tls/ca.pem 2>/dev/null
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -subj "/CN=127.0.0.1" -keyout tls/key.pem -out tls/cert.csr 2>/dev/null
  openssl x509 -req -in tls/cert.csr -CA tls/ca.pem -CAkey tls/ca-key.pem -CAcreateserial -days 3650 \
    -extfile <(printf 'subjectAltName=IP:127.0.0.1,DNS:localhost\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n') \
    -out tls/cert.pem 2>/dev/null
  rm -f tls/cert.csr tls/ca.srl
  # The proxy container runs as another user and has to read the key.
  chmod 644 tls/key.pem
fi

if [[ "${1:-}" != --official ]]; then
  docker compose up -d --wait "$@"
  exit
fi
shift
docker compose --profile official up -d --wait "$@"

# The admin service migrates the database before it reports that it started.
for attempt in $(seq 180); do
  docker compose exec -T official grep -qs "Application started" /var/log/bitwarden/admin.log && break
  if (( attempt == 180 )); then
    echo "the official server did not finish migrating its database" >&2
    exit 1
  fi
  sleep 1
done

# Attachments need a premium account with storage, which a self-hosted server
# grants only through a license. The sandbox makes every account premium
# instead, except those whose email starts with `free-`, which stand in for
# accounts without a subscription.
docker compose exec -T official-db psql -U bitwarden -d vault -v ON_ERROR_STOP=1 -q <<'SQL'
CREATE OR REPLACE FUNCTION sandbox_premium() RETURNS trigger AS $$
BEGIN
  IF NEW."Email" COLLATE "C" NOT LIKE 'free-%' THEN
    NEW."Premium" := true;
    NEW."MaxStorageGb" := 10;
  END IF;
  RETURN NEW;
END $$ LANGUAGE plpgsql;
CREATE OR REPLACE TRIGGER sandbox_premium BEFORE INSERT OR UPDATE ON "User"
  FOR EACH ROW EXECUTE FUNCTION sandbox_premium();
SQL
