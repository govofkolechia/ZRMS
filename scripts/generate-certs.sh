#!/usr/bin/env bash
set -euo pipefail

OUTPUT_DIR="${1:-${XDG_CONFIG_HOME:-$HOME/.config}/zyfin/pki}"
SERVER_NAME="${2:-zrms-controller}"
AGENT_NAME="${3:-agent}"

for name in "$SERVER_NAME" "$AGENT_NAME"; do
    if [[ ! "$name" =~ ^[A-Za-z0-9.-]+$ ]]; then
        echo "Invalid certificate name: $name" >&2
        exit 1
    fi
done

if [[ -e "$OUTPUT_DIR/ca.key" || -e "$OUTPUT_DIR/server.key" || -e "$OUTPUT_DIR/agent.key" ]]; then
    echo "Certificate files already exist in: $OUTPUT_DIR" >&2
    echo "Refusing to overwrite existing credentials." >&2
    exit 1
fi

mkdir -p "$OUTPUT_DIR"
chmod 700 "$OUTPUT_DIR"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "Generating new ZRMS certificate authority..."
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:3072 \
    -out "$OUTPUT_DIR/ca.key"

openssl req -x509 -new -sha256 \
    -key "$OUTPUT_DIR/ca.key" \
    -days 3650 \
    -subj "/CN=ZRMS Certificate Authority" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -out "$OUTPUT_DIR/ca.crt"

echo "Generating controller certificate..."
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:3072 \
    -out "$OUTPUT_DIR/server.key"

openssl req -new \
    -key "$OUTPUT_DIR/server.key" \
    -subj "/CN=$SERVER_NAME" \
    -out "$TMP_DIR/server.csr"

cat > "$TMP_DIR/server.ext" <<EXT
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:$SERVER_NAME
EXT

openssl x509 -req -sha256 \
    -in "$TMP_DIR/server.csr" \
    -CA "$OUTPUT_DIR/ca.crt" \
    -CAkey "$OUTPUT_DIR/ca.key" \
    -CAcreateserial \
    -days 825 \
    -extfile "$TMP_DIR/server.ext" \
    -out "$OUTPUT_DIR/server.crt"

echo "Generating agent certificate..."
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:3072 \
    -out "$OUTPUT_DIR/agent.key"

openssl req -new \
    -key "$OUTPUT_DIR/agent.key" \
    -subj "/CN=$AGENT_NAME" \
    -out "$TMP_DIR/agent.csr"

cat > "$TMP_DIR/agent.ext" <<EXT
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=clientAuth
EXT

openssl x509 -req -sha256 \
    -in "$TMP_DIR/agent.csr" \
    -CA "$OUTPUT_DIR/ca.crt" \
    -CAkey "$OUTPUT_DIR/ca.key" \
    -CAcreateserial \
    -days 825 \
    -extfile "$TMP_DIR/agent.ext" \
    -out "$OUTPUT_DIR/agent.crt"

chmod 600 "$OUTPUT_DIR"/*.key
chmod 644 "$OUTPUT_DIR"/*.crt

echo
echo "Certificates generated in: $OUTPUT_DIR"
echo "Controller TLS identity: $SERVER_NAME"
echo "Agent identity: $AGENT_NAME"
echo "Keep ca.key private. Never commit private keys."
