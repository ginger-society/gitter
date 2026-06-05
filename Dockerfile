FROM alpine:3.19

RUN apk add --no-cache openssh bash git perl curl make

# Install gitolite
RUN git clone https://github.com/sitaramc/gitolite /opt/gitolite && \
    /opt/gitolite/install -to /usr/local/bin

RUN adduser -D -s /bin/bash git && \
    passwd -u git || true

# ── kubectl ───────────────────────────────────────────────────────────────────
# Download the latest stable kubectl binary for the host architecture.
# The binary lands at /usr/local/bin/kubectl — the path probed first by
# find_kubectl() in the pipeline hook, so no PATH tricks are needed.
RUN set -eux; \
    ARCH=$(uname -m); \
    case "$ARCH" in \
      x86_64)  KUBECTL_ARCH=amd64 ;; \
      aarch64) KUBECTL_ARCH=arm64 ;; \
      armv7l)  KUBECTL_ARCH=arm ;; \
      *)        echo "Unsupported arch: $ARCH" >&2; exit 1 ;; \
    esac; \
    KUBECTL_VERSION=$(curl -fsSL https://dl.k8s.io/release/stable.txt); \
    curl -fsSL "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/${KUBECTL_ARCH}/kubectl" \
         -o /usr/local/bin/kubectl; \
    chmod 755 /usr/local/bin/kubectl; \
    kubectl version --client

# CA public key
COPY signing-keys/ca_key.pub /etc/ssh/ca_key.pub
RUN chmod 644 /etc/ssh/ca_key.pub

# authorized-principals: echo back the cert principal
RUN printf '#!/bin/bash\necho "$2"\n' > /usr/local/bin/authorized-principals && \
    chmod 755 /usr/local/bin/authorized-principals

RUN cat > /usr/local/bin/gitolite-cert-shell <<'WRAPPER'
#!/bin/bash
set -euo pipefail
AUTH_KEYS=/home/git/.ssh/authorized_keys
AUTH_INFO=$(cat "${SSH_USER_AUTH:-/dev/null}")
KEY_TYPE=$(echo "$AUTH_INFO" | awk '{print $2}')

TMPF1=$(mktemp /tmp/sshkeyXXXXXX)
TMPF2=$(mktemp /tmp/sshkeyXXXXXX)
trap 'rm -f "$TMPF1" "$TMPF2"' EXIT

case "$KEY_TYPE" in
  *-cert-v01@openssh.com)
    KEY_B64=$(echo "$AUTH_INFO" | awk '{print $3}')
    echo "$KEY_TYPE $KEY_B64" > "$TMPF1"
    GL_USER=$(ssh-keygen -L -f "$TMPF1" 2>/dev/null \
      | awk '/Principals:/,/Critical Options:|Extensions:/' \
      | grep -v 'Principals:\|Critical Options:\|Extensions:' \
      | tr -d ' \t' | grep -v '^$' | head -1)

    if [[ -z "$GL_USER" ]]; then
      echo "ERROR: Could not extract principal from certificate" >&2
      exit 1
    fi

    export GL_USER
    exec gitolite-shell "$GL_USER"
    ;;

  ssh-*|ecdsa-*|sk-*)
    if [[ ! -f "$AUTH_KEYS" ]]; then
      echo "ERROR: authorized_keys not found at $AUTH_KEYS" >&2; exit 1
    fi

    echo "$AUTH_INFO" > "$TMPF1"
    CONNECTING_FP=$(ssh-keygen -lf "$TMPF1" 2>/dev/null | awk '{print $2}')

    if [[ -z "$CONNECTING_FP" ]]; then
      echo "ERROR: Could not fingerprint connecting key" >&2; exit 1
    fi

    GL_USER=""
    while IFS= read -r line; do
      case "$line" in
        *gitolite-shell\ *) ;;
        *) continue ;;
      esac

      CANDIDATE_USER=$(echo "$line" | sed 's|.*gitolite-shell \([^"]*\)".*|\1|')

      BARE_KEY=$(echo "$line" | awk '{
        for(i=1;i<=NF;i++) {
          if ($i ~ /^(ssh-|ecdsa-|sk-)/) {
            print $i" "$(i+1)
            exit
          }
        }
      }')

      [[ -z "$BARE_KEY" ]] && continue

      echo "$BARE_KEY" > "$TMPF2"
      CANDIDATE_FP=$(ssh-keygen -lf "$TMPF2" 2>/dev/null | awk '{print $2}')

      if [[ "$CANDIDATE_FP" == "$CONNECTING_FP" ]]; then
        GL_USER="$CANDIDATE_USER"
        break
      fi
    done < "$AUTH_KEYS"

    if [[ -z "$GL_USER" ]]; then
      echo "ERROR: Could not match pubkey fingerprint to gitolite user (FP=${CONNECTING_FP})" >&2
      exit 1
    fi

    export GL_USER
    exec gitolite-shell "$GL_USER"
    ;;

  *)
    echo "ERROR: Unrecognised key type '${KEY_TYPE}'" >&2
    echo "DEBUG: AUTH_INFO='${AUTH_INFO}'" >&2
    exit 1
    ;;
esac
WRAPPER
RUN chmod 755 /usr/local/bin/gitolite-cert-shell

COPY target/release/ginger-gitter-pipeline-hook /usr/local/bin/ginger-gitter-pipeline-hook
RUN chmod 755 /usr/local/bin/ginger-gitter-pipeline-hook

# Post-receive hook — fires after every push to any repo.
# gitolite copies hooks/common/* into every repo during compile,
# so this automatically applies to all existing and future repos.
# LOCAL_CODE in .gitolite.rc tells gitolite where to find this directory.
RUN mkdir -p /home/git/.gitolite/local/hooks/common && \
    cat > /home/git/.gitolite/local/hooks/common/post-receive <<'HOOK'
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
RUN chmod +x /home/git/.gitolite/local/hooks/common/post-receive

# Patch sshd_config
RUN cat >> /etc/ssh/sshd_config <<'EOF'

TrustedUserCAKeys /etc/ssh/ca_key.pub
AuthorizedPrincipalsFile /etc/ssh/auth_principals/%u
AuthorizedPrincipalsCommand /usr/local/bin/authorized-principals %u %i
AuthorizedPrincipalsCommandUser root
ExposeAuthInfo yes
PasswordAuthentication no

Match User git
    ForceCommand /usr/local/bin/gitolite-cert-shell

Match All
EOF

# Generate host keys
RUN ssh-keygen -A

COPY entrypoint.sh /entrypoint.sh
RUN chmod 755 /entrypoint.sh

EXPOSE 22
ENTRYPOINT ["/entrypoint.sh"]
CMD ["sshd"]