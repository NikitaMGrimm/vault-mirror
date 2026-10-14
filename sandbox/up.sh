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

docker compose up -d --wait "$@"
