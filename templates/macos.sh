#!/usr/bin/env bash
set -Eeuo pipefail

CALLBACK_URL=@@CALLBACK_URL@@
METADATA_URL=@@METADATA_URL@@
BEARER_TOKEN=@@INSTANCE_TOKEN@@
RUNNER_NAME=@@RUNNER_NAME@@
DOWNLOAD_URL=@@DOWNLOAD_URL@@
DOWNLOAD_TOKEN=@@DOWNLOAD_TOKEN@@
FILENAME=@@FILENAME@@
EXPECTED_SHA256=@@SHA256@@
REPOSITORY_URL=@@REPOSITORY_URL@@
RUNNER_LABELS=@@RUNNER_LABELS@@
JIT_ENABLED=@@JIT_ENABLED@@
CA_BUNDLE_B64=@@CA_BUNDLE_B64@@

stage() {
    printf 'outrider bootstrap: %s\n' "$1"
    local url="$CALLBACK_URL"
    [[ "$url" == */status || "$url" == */status/ ]] || url="${url%/}/status"
    curl --silent --show-error --fail --retry 3 --retry-connrefused \
        -H 'Accept: application/json' -H "Authorization: Bearer $BEARER_TOKEN" \
        -H 'Content-Type: application/json' -d "{\"status\":\"installing\",\"message\":\"$1\"}" "$url" >/dev/null || true
}

fail() {
    trap - ERR
    local url="$CALLBACK_URL"
    [[ "$url" == */status || "$url" == */status/ ]] || url="${url%/}/status"
    curl --silent --show-error --fail --retry 2 --retry-connrefused \
        -H 'Accept: application/json' -H "Authorization: Bearer $BEARER_TOKEN" \
        -H 'Content-Type: application/json' -d '{"status":"failed","message":"runner bootstrap failed"}' "$url" >/dev/null || true
    exit 1
}
trap fail ERR

if ! id runner >/dev/null 2>&1; then
    runner_password="$(openssl rand -hex 32)"
    sysadminctl -addUser runner -password "$runner_password" -home /Users/runner -shell /bin/bash >/dev/null
fi
@@SSH_KEYS@@
if [[ -n "$CA_BUNDLE_B64" ]]; then
    printf '%s' "$CA_BUNDLE_B64" | base64 -D > /tmp/outrider-ca.pem
    awk 'BEGIN { count=0; active=0 } /-----BEGIN CERTIFICATE-----/ { count++; active=1; file=sprintf("/tmp/outrider-ca-%03d.pem", count) } active { print > file } /-----END CERTIFICATE-----/ { close(file); active=0 }' /tmp/outrider-ca.pem
    for certificate in /tmp/outrider-ca-*.pem; do security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain "$certificate"; done
    rm -f /tmp/outrider-ca.pem /tmp/outrider-ca-*.pem
fi

stage 'downloading runner'
install -d -m 0755 /opt/outrider /Users/runner/actions-runner
archive="/opt/outrider/$FILENAME"
headers=(-H 'Accept: application/octet-stream')
if [[ -n "$DOWNLOAD_TOKEN" ]]; then headers+=(-H "Authorization: Bearer $DOWNLOAD_TOKEN"); fi
curl --silent --show-error --fail --location --retry 5 --retry-connrefused \
    "${headers[@]}" -o "$archive" "$DOWNLOAD_URL"
printf '%s  %s\n' "$EXPECTED_SHA256" "$archive" | shasum -a 256 -c -s
tar -xf "$archive" -C /Users/runner/actions-runner
chown -R runner:staff /Users/runner/actions-runner
cd /Users/runner/actions-runner

if [[ "$JIT_ENABLED" == true ]]; then
    stage 'retrieving runner credentials'
    for item in runner credentials credentials_rsaparams; do
        curl --silent --show-error --fail --retry 5 --retry-connrefused \
            -H 'Accept: application/json' -H "Authorization: Bearer $BEARER_TOKEN" \
            -o ".${item}" "${METADATA_URL%/}/credentials/${item}"
    done
    chown runner:staff .runner .credentials .credentials_rsaparams
    chmod 0600 .runner .credentials .credentials_rsaparams
else
    stage 'retrieving repository registration token'
    registration_token="$(curl --silent --show-error --fail --retry 5 --retry-connrefused \
        -H 'Accept: application/json' -H "Authorization: Bearer $BEARER_TOKEN" \
        "${METADATA_URL%/}/runner-registration-token/")"
    sudo -u runner ./config.sh --unattended --url "$REPOSITORY_URL" \
        --token "$registration_token" --name "$RUNNER_NAME" --labels "$RUNNER_LABELS" \
        --no-default-labels --ephemeral
fi

cat > /Library/LaunchDaemons/com.outrider.runner.plist <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.outrider.runner</string>
<key>ProgramArguments</key><array><string>/Users/runner/actions-runner/run.sh</string></array>
<key>WorkingDirectory</key><string>/Users/runner/actions-runner</string>
<key>UserName</key><string>runner</string>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><false/>
</dict></plist>
PLIST
launchctl bootstrap system /Library/LaunchDaemons/com.outrider.runner.plist
agent_id="$(plutil -extract agent_id raw -o - .runner 2>/dev/null || plutil -extract agentId raw -o - .runner 2>/dev/null)" || fail
[[ "$agent_id" =~ ^[1-9][0-9]*$ ]] || fail
base_callback="${CALLBACK_URL%/}"
base_callback="${base_callback%/status}"
os_name="$(sw_vers -productName | tr -d '\r\n')"
os_version="$(sw_vers -productVersion | tr -d '\r\n')"
curl --silent --show-error --fail --retry 3 --retry-connrefused \
    -H 'Accept: application/json' -H "Authorization: Bearer $BEARER_TOKEN" \
    -H 'Content-Type: application/json' -d "{\"os_name\":\"$os_name\",\"os_version\":\"$os_version\",\"agent_id\":$agent_id}" \
    "${base_callback}/system-info/" >/dev/null || true
curl --silent --show-error --fail --retry 3 --retry-connrefused \
    -H 'Accept: application/json' -H "Authorization: Bearer $BEARER_TOKEN" \
    -H 'Content-Type: application/json' -d "{\"status\":\"idle\",\"message\":\"runner successfully installed\",\"agent_id\":$agent_id}" \
    "${base_callback}/status" >/dev/null
trap - ERR
