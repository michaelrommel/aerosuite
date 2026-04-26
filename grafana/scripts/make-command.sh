#!/usr/bin/env bash
# make-command.sh — regenerate the "command" JSON array for the config-init
# container in task-definition.json.
#
# Reads the source config files from ./configs/ and embeds them verbatim as
# heredocs in the Alpine shell command that runs at task startup.
#
# The generated container command:
#   1. Writes all service configs to the shared in-memory volumes.
#   2. Writes the Grafana dashboard provisioning YAML and every *.json file
#      found in configs/grafana/provisioning/dashboards/ to the same volume.
#      Drop a new dashboard JSON there and re-run this script to publish it.
#   3. Touches /tmp/aeromon-ready so the HEALTHY condition fires and lets
#      prometheus / tempo / mimir / grafana start (they depend on HEALTHY,
#      not SUCCESS, because this container stays up to run sshd).
#   4. Installs openssh-server via apk, drops the authorised public key for
#      root, and starts sshd in the foreground.
#
# SSH port-forward usage (once the task IP is known):
#   ssh -L 3000:localhost:3000 root@<task-ip>
#   open http://localhost:3000
#
# Usage:
#   ./scripts/make-command.sh            → prints the JSON "command" array
#   ./scripts/make-command.sh --patch    → patches task-definition.json (needs jq)
#
# Requirements: python3

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."   # repo root (grafana/)

CONFIGS=configs
TASK_DEF=task-definition.json

DASH_SRC="$CONFIGS/grafana/provisioning/dashboards"

# ── 1.  Build the inner shell script ─────────────────────────────────────────

TMP=$(mktemp)
trap 'rm -f "$TMP"' EXIT

# emit_heredoc DEST MARKER SRCFILE
# Writes a heredoc block that creates DEST from SRCFILE content.
emit_heredoc() {
    printf "cat > %s << '%s'\n" "$1" "$2"
    cat "$3"
    # Ensure terminator is always on its own line even if the file lacks a
    # trailing newline.
    printf '\n%s\n\n' "$2"
}

{
    cat << 'HEADER'
set -e
mkdir -p \
    /cfg/prometheus \
    /cfg/tempo \
    /cfg/mimir \
    /cfg/grafana/datasources \
    /cfg/grafana/dashboards

HEADER

    # ── service configs ───────────────────────────────────────────────────────

    emit_heredoc /cfg/prometheus/prometheus.yml \
                 PROMEOF \
                 "$CONFIGS/prometheus/prometheus.yml"

    emit_heredoc /cfg/tempo/tempo.yaml \
                 TEMPOEOF \
                 "$CONFIGS/tempo/tempo.yaml"

    emit_heredoc /cfg/mimir/mimir.yaml \
                 MIMIREOF \
                 "$CONFIGS/mimir/mimir.yaml"

    emit_heredoc /cfg/grafana/datasources/datasources.yaml \
                 GRAFANAEOF \
                 "$CONFIGS/grafana/provisioning/datasources/datasources.yaml"

    # ── Grafana dashboard provisioning ────────────────────────────────────────

    emit_heredoc /cfg/grafana/dashboards/dashboards.yaml \
                 DASHPROVEOF \
                 "$DASH_SRC/dashboards.yaml"

    # Embed every *.json file found in the dashboards source directory.
    # To publish a new dashboard: save its JSON there and re-run this script.
    shopt -s nullglob
    for json_src in "$DASH_SRC"/*.json; do
        json_name=$(basename "$json_src")
        # Use the filename stem (uppercased) as the heredoc terminator to keep
        # each one unique.  JSON files cannot realistically contain a line
        # that is exactly e.g. "AEROFTPJSONEOF".
        stem=$(echo "$json_name" | tr '[:lower:].' '[:upper:]_' | sed 's/_JSON$//')
        marker="${stem}JSONEOF"
        emit_heredoc "/cfg/grafana/dashboards/${json_name}" \
                     "${marker}" \
                     "$json_src"
    done
    shopt -u nullglob

    # The SSHDEOF heredoc is nested inside the FOOTER heredoc.
    # Bash does not interpret << markers found *inside* a heredoc body, so
    # SSHDEOF is plain text until the FOOTER terminator is reached.  Once
    # the generated script actually runs in the container, /bin/sh will
    # process it normally.
    cat << 'FOOTER'
echo 'Config files written.'
touch /tmp/aeromon-ready

# ── OpenSSH daemon ─────────────────────────────────────────────────────────
# apk runs against the public Alpine mirrors; the task must have outbound
# internet access (public IP or NAT gateway).
apk add --no-cache openssh-server 1>/dev/null

# Generate fresh host keys (ephemeral; update ~/.ssh/known_hosts each run).
ssh-keygen -A -q

mkdir -p /root/.ssh
chmod 700 /root/.ssh

printf 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDMrciGtKUQozFiA/miO85TKw9mRw3mnO8WPPIGc0U36 rommel@md1maxec\n' \
    > /root/.ssh/authorized_keys
chmod 600 /root/.ssh/authorized_keys

cat > /etc/ssh/sshd_config << 'SSHDEOF'
Port 22
PermitRootLogin prohibit-password
PubkeyAuthentication yes
AuthorizedKeysFile .ssh/authorized_keys
PasswordAuthentication no
AllowTcpForwarding yes
GatewayPorts yes
X11Forwarding no
PrintMotd no
LogLevel INFO
SSHDEOF

echo 'Starting sshd ...'
mkdir -p /run/sshd
/usr/sbin/sshd
exec sleep infinity
FOOTER
} > "$TMP"

# ── 2.  JSON-encode and emit the command array ────────────────────────────────

json_command() {
    python3 - "$TMP" << 'PYEOF'
import sys, json

with open(sys.argv[1]) as fh:
    script = fh.read()

# ECS command array: the third element is the entire shell script as a
# single JSON string (newlines encoded as \n, backslashes doubled, etc.).
cmd = ["/bin/sh", "-c", script]
print(json.dumps(cmd, indent=2))
PYEOF
}

if [[ "${1:-}" == "--patch" ]]; then
    command -v jq >/dev/null 2>&1 || { echo "jq is required for --patch" >&2; exit 1; }
    JSON=$(json_command)
    # Replace the command array of the container named "config-init".
    jq --argjson cmd "$JSON" \
       '(.containerDefinitions[] | select(.name == "config-init") | .command) = $cmd' \
       "$TASK_DEF" > "${TASK_DEF}.tmp" && mv "${TASK_DEF}.tmp" "$TASK_DEF"
    echo "Patched $TASK_DEF" >&2
else
    echo "# Paste this as the \"command\" field of the config-init container:" >&2
    echo >&2
    json_command
fi
