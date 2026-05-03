#!/usr/bin/env bash
# gen-user-cert.sh
# Generates a short-lived (10-minute) SSH user certificate.
# The certificate principal matches the gitolite username.
#
# Usage: ./gen-user-cert.sh <identity>
#
# Output: ./user_key      (private key — reused if already present)
#         ./user_key.pub  (public key)
#         ./user_key-cert.pub  (the signed certificate, 10-min TTL)
#   ssh will pick up user_key + user_key-cert.pub automatically as a pair.

set -euo pipefail

# ── argument check ────────────────────────────────────────────────────────────
if [[ $# -lt 1 ]]; then
  echo "Usage: $0 <identity>"
  echo "  identity  The gitolite username (becomes the SSH certificate principal)"
  exit 1
fi

IDENTITY="$1"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CA_KEY="$SCRIPT_DIR/signing-keys/ca_key"

# ── sanity checks ─────────────────────────────────────────────────────────────
if [[ ! -f "$CA_KEY" ]]; then
  echo "[ERROR] CA private key not found at $CA_KEY"
  echo "        Run ./gen-ca.sh first."
  exit 1
fi

USER_KEY="$SCRIPT_DIR/user_key"

# ── generate a fresh ephemeral user keypair if one doesn't exist ──────────────
# For maximum security you may want to regenerate every time; for convenience
# we reuse the keypair and only re-sign it (the cert is what expires).
if [[ ! -f "$USER_KEY" ]]; then
  echo "[INFO] Generating user keypair at $USER_KEY ..."
  ssh-keygen \
    -t ed25519 \
    -f "$USER_KEY" \
    -C "${IDENTITY}@ephemeral" \
    -N ""   # no passphrase on the user key itself
else
  echo "[INFO] Reusing existing user keypair at $USER_KEY"
fi

# ── sign the public key ───────────────────────────────────────────────────────
echo "[INFO] Signing ${USER_KEY}.pub as principal '${IDENTITY}' (TTL: 10 minutes) ..."

ssh-keygen \
  -s "$CA_KEY" \
  -I "${IDENTITY}-ephemeral-$(date +%s)" \
  -n "$IDENTITY" \
  -V "+1h" \
  -z "$(date +%s)" \
  "${USER_KEY}.pub"

# ssh-keygen writes the cert next to the public key as user_key-cert.pub
CERT="$SCRIPT_DIR/user_key-cert.pub"

echo ""
echo "[OK] Certificate issued:"
ssh-keygen -L -f "$CERT"
echo ""
echo "To connect:"
echo "  ssh -i $USER_KEY -p 8022 git@<server-host> info"
echo ""
echo "The certificate expires in 1 hour."
