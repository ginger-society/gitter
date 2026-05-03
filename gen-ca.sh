#!/usr/bin/env bash
# gen-ca.sh
# Generates an SSH Certificate Authority keypair in ./signing-keys/
# Usage: ./gen-ca.sh

set -euo pipefail

SIGNING_DIR="$(cd "$(dirname "$0")" && pwd)/signing-keys"

if [[ -d "$SIGNING_DIR" ]]; then
  echo "[INFO] Directory $SIGNING_DIR already exists."
else
  mkdir -p "$SIGNING_DIR"
  echo "[INFO] Created $SIGNING_DIR"
fi

CA_KEY="$SIGNING_DIR/ca_key"

if [[ -f "$CA_KEY" ]]; then
  echo "[WARN] CA key already exists at $CA_KEY — skipping generation."
  echo "       Delete it manually if you want to regenerate."
  exit 0
fi

echo "[INFO] Generating CA keypair at $CA_KEY ..."
ssh-keygen \
  -t ed25519 \
  -f "$CA_KEY" \
  -C "gitolite-ssh-ca" \
  -N ""          # no passphrase — add one for production use

chmod 600 "$CA_KEY"
chmod 644 "${CA_KEY}.pub"

echo ""
echo "[OK] CA keypair generated:"
echo "     Private key : $CA_KEY"
echo "     Public key  : ${CA_KEY}.pub"
echo ""
echo "Next steps:"
echo "  1. Mount ${CA_KEY}.pub into the Docker image (see docker-compose.yml)."
echo "  2. Run ./gen-user-cert.sh <identity> to issue a short-lived user cert."
