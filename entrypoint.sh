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
  HOME=$GIT_HOME su git -s /bin/bash -c "mkdir -p $GIT_HOME/.gitolite/logs"
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
RC_FILE="$GIT_HOME/.gitolite.rc"
if [[ -f "$RC_FILE" ]]; then
  echo "[entrypoint] Patching .gitolite.rc..."

  # 1. WILDREPOS — uncomment if commented, or add if missing, ensure set to 1.
  if grep -q "WILDREPOS" "$RC_FILE"; then
    # Uncomment and set to 1 (handles both commented and active lines)
    perl -i -pe "s/^(\s*)#?\s*WILDREPOS\s*=>\s*\d/\${1}WILDREPOS => 1/" "$RC_FILE"
    echo "[entrypoint]   WILDREPOS => 1 ensured"
  else
    perl -i -0777 -pe "s/(%RC\s*=\s*\()/\$1\n    WILDREPOS => 1,/" "$RC_FILE"
    echo "[entrypoint]   WILDREPOS => 1 added"
  fi

  # 2. LOCAL_CODE — use hardcoded /home/git path (NOT \$ENV{HOME} which resolves
  #    to /root when run as root, causing Permission denied for the git user).
  #    Remove all LOCAL_CODE lines (commented or not) and insert the correct one.
  perl -i -ne 'print unless /LOCAL_CODE/' "$RC_FILE"
  perl -i -0777 -pe 'BEGIN{$code=shift} s/(%RC\s*=\s*\()/$1\n    LOCAL_CODE => "$code\/.gitolite\/local",/' \
    "$GIT_HOME" "$RC_FILE"
  echo "[entrypoint]   LOCAL_CODE set to $GIT_HOME/.gitolite/local"

  # 3. repo-specific-hooks — uncomment if commented, add if missing.
  if grep -q "repo-specific-hooks" "$RC_FILE"; then
    perl -i -pe "s/^\s*#\s*('repo-specific-hooks')/        \$1,/" "$RC_FILE"
    echo "[entrypoint]   repo-specific-hooks uncommented/ensured"
  else
    perl -i -0777 -pe "s/(ENABLE\s*=>\s*\[)/\$1\n        'repo-specific-hooks',/" "$RC_FILE"
    echo "[entrypoint]   repo-specific-hooks added to ENABLE"
  fi

  echo "[entrypoint] .gitolite.rc patch complete"
  echo "[entrypoint] Verifying key settings:"
  grep -E "LOCAL_CODE|WILDREPOS|repo-specific-hooks" "$RC_FILE" | grep -v "^#" || true

else
  echo "[entrypoint] ERROR: .gitolite.rc not found at $RC_FILE" >&2
  exit 1
fi
# ── End .gitolite.rc patch ─────────────────────────────────────────────────

# ── Ensure common hooks are in place ──────────────────────────────────────
echo "[entrypoint] Installing common hooks..."
mkdir -p $GIT_HOME/.gitolite/local/hooks/common
cat > $GIT_HOME/.gitolite/local/hooks/common/post-receive <<'HOOK'
#!/bin/bash
set -euo pipefail
read GL_OLDREV GL_NEWREV GL_REFNAME
exec ginger-gitter-pipeline-hook \
    "$GL_USER" \
    "$GL_REPO" \
    "$GL_REFNAME" \
    "$GL_OLDREV" \
    "$GL_NEWREV"
HOOK
chmod +x $GIT_HOME/.gitolite/local/hooks/common/post-receive
chown -R git:git $GIT_HOME/.gitolite/local
echo "[entrypoint] Common hooks installed."

# ── Compile and propagate hooks ────────────────────────────────────────────
echo "[entrypoint] Compiling gitolite..."
HOME=$GIT_HOME su git -s /bin/bash -c "gitolite compile"

[[ -f $GIT_HOME/.ssh/authorized_keys ]] || \
  { echo "[entrypoint] ERROR: authorized_keys missing after compile" >&2; exit 1; }
echo "[entrypoint] authorized_keys ready."

# Propagate hooks into ALL existing repos. This is the critical step that
# gitolite compile alone does NOT do — setup --hooks-only copies LOCAL_CODE
# hooks into every repo's hooks/ directory. Without this, post-receive is
# missing from repos that existed before this entrypoint run.
echo "[entrypoint] Propagating hooks to all repos..."
HOME=$GIT_HOME su git -s /bin/bash -c "gitolite setup --hooks-only" && \
  echo "[entrypoint] Hooks propagated." || \
  echo "[entrypoint] WARNING: setup --hooks-only failed — check LOCAL_CODE path and permissions"

# ── Sync SSH principals ────────────────────────────────────────────────────
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