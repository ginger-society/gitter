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