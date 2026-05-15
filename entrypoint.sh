#!/usr/bin/env bash
set -euo pipefail

GIT_HOME=/home/git

if [[ ! -f $GIT_HOME/.gitolite/conf/gitolite.conf ]]; then
  echo "[entrypoint] Initializing gitolite..."

  ADMIN_KEY="${ADMIN_KEY_PATH:-/run/secrets/admin_key.pub}"
  [[ ! -f "$ADMIN_KEY" ]] && ADMIN_KEY=/tmp/admin_key.pub
  if [[ ! -f "$ADMIN_KEY" ]]; then
    echo "[entrypoint] ERROR: No admin key found" >&2; exit 1
  fi

  chown -R git:git $GIT_HOME
  HOME=$GIT_HOME su git -s /bin/bash -c "gitolite setup -pk '$ADMIN_KEY'" && \
    echo "[entrypoint] gitolite setup OK" || \
    { echo "[entrypoint] ERROR: gitolite setup failed" >&2; exit 1; }

  [[ -f $GIT_HOME/.gitolite/conf/gitolite.conf ]] || \
    { echo "[entrypoint] ERROR: gitolite.conf missing after setup" >&2; exit 1; }

  echo "[entrypoint] Gitolite initialized."
else
  echo "[entrypoint] Gitolite already initialized."
fi

# ── Patch .gitolite.rc ─────────────────────────────────────────────────────
# Runs on EVERY container start — both fresh installs and existing PVCs.
#
# Why outside the init block:
#   Existing pods already have a PVC with .gitolite.rc written by the first
#   setup. Re-deploying the image would skip the init block entirely, so any
#   patch inside it would never apply to live clusters. Running here ensures
#   the rc is always in the expected state regardless of when the pod was
#   first initialised.
#
# Safety: every change is guarded by grep -q so it is fully idempotent —
#   restarting the pod repeatedly will not duplicate lines.
RC_FILE="$GIT_HOME/.gitolite.rc"
if [[ -f "$RC_FILE" ]]; then
  echo "[entrypoint] Checking .gitolite.rc for required settings..."

  # 1. WILDREPOS — required for `C` (create) permission to work.
  #    Without this, gitolite silently ignores C permissions in the conf
  #    and rejects pushes to non-existent wildcard repos.
  if ! grep -q "WILDREPOS" "$RC_FILE"; then
    perl -i -pe "s/^(%RC\s*=\s*\()/%RC = (
    WILDREPOS => 1,/" "$RC_FILE"
    echo "[entrypoint]   WILDREPOS => 1 added"
  else
    # Already present — make sure it is not explicitly disabled
    perl -i -pe "s/WILDREPOS\s*=>\s*0/WILDREPOS => 1/" "$RC_FILE"
    echo "[entrypoint]   WILDREPOS already present — ensured set to 1"
  fi

  # 2. repo-specific-hooks — allows per-repo hook scripts.
  if ! grep -q "'repo-specific-hooks'" "$RC_FILE"; then
    perl -i -pe "s/(ENABLE\s*=>\s*\[)/\$1
        'repo-specific-hooks',/" "$RC_FILE"
    echo "[entrypoint]   repo-specific-hooks added to ENABLE"
  else
    echo "[entrypoint]   repo-specific-hooks already enabled"
  fi

  echo "[entrypoint] .gitolite.rc check complete"
else
  echo "[entrypoint] ERROR: .gitolite.rc not found at $RC_FILE" >&2
  exit 1
fi
# ── End .gitolite.rc patch ─────────────────────────────────────────────────

echo "[entrypoint] Compiling gitolite keys..."
HOME=$GIT_HOME su git -s /bin/bash -c "gitolite compile"

[[ -f $GIT_HOME/.ssh/authorized_keys ]] || \
  { echo "[entrypoint] ERROR: authorized_keys missing after compile" >&2; exit 1; }
echo "[entrypoint] authorized_keys ready."

sync_principals() {
  HOME=$GIT_HOME su git -s /bin/bash -c \
    'gitolite list-users 2>/dev/null' \
    | grep -v '^@' \
    > /etc/ssh/auth_principals/git.tmp 2>/dev/null \
  && mv /etc/ssh/auth_principals/git.tmp /etc/ssh/auth_principals/git \
  && chmod 644 /etc/ssh/auth_principals/git \
  && echo "[entrypoint] Synced principals: $(cat /etc/ssh/auth_principals/git | tr '\n' ' ')"
}

mkdir -p /etc/ssh/auth_principals
sync_principals || echo "[entrypoint] WARNING: Could not sync principals"

(while true; do sleep 30; sync_principals 2>/dev/null || true; done) &

echo "[entrypoint] Starting sshd..."
exec /usr/sbin/sshd -D -e