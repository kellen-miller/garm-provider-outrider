#!/usr/bin/env bash
# Copyright 2022 Cloudbase Solutions SRL
# Modified by Kellen Miller for Outrider.
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

stage 'downloading runner'
RUNNER_DIRECTORY=/home/runner/actions-runner
mkdir -p "$RUNNER_DIRECTORY"
archive="/home/runner/$FILENAME"
headers=(-H 'Accept: application/octet-stream')
if [[ -n "$DOWNLOAD_TOKEN" ]]; then headers+=(-H "Authorization: Bearer $DOWNLOAD_TOKEN"); fi
curl --silent --show-error --fail --location --retry 5 --retry-connrefused \
    "${headers[@]}" -o "$archive" "$DOWNLOAD_URL"
printf '%s  %s\n' "$EXPECTED_SHA256" "$archive" | sha256sum --check --status
tar -xf "$archive" -C "$RUNNER_DIRECTORY"
cd "$RUNNER_DIRECTORY"

stage 'installing runner dependencies'
sudo ./bin/installdependencies.sh

if [[ "$JIT_ENABLED" == true ]]; then
    stage 'retrieving runner credentials'
    for item in runner credentials credentials_rsaparams; do
        curl --silent --show-error --fail --retry 5 --retry-connrefused \
            -H 'Accept: application/json' -H "Authorization: Bearer $BEARER_TOKEN" \
            -o ".${item}" "${METADATA_URL%/}/credentials/${item}"
    done
    chmod 0600 .runner .credentials .credentials_rsaparams
    curl --silent --show-error --fail --retry 5 --retry-connrefused \
        -H 'Accept: application/json' -H "Authorization: Bearer $BEARER_TOKEN" \
        "${METADATA_URL%/}/system/service-name" -o /tmp/outrider-service-name
    service_name="$(cat /tmp/outrider-service-name).service"
    [[ "$service_name" =~ ^[A-Za-z0-9_.-]+\.service$ ]] || fail
    curl --silent --show-error --fail --retry 5 --retry-connrefused \
        -H 'Accept: application/json' -H "Authorization: Bearer $BEARER_TOKEN" \
        "${METADATA_URL%/}/systemd/unit-file?runAsUser=runner" -o "$service_name"
    sudo install -o root -g root -m 0644 "$service_name" "/etc/systemd/system/$service_name"
    cp bin/runsvc.sh ./runsvc.sh
    sudo systemctl daemon-reload
    sudo systemctl enable --now "$service_name"
else
    stage 'retrieving repository registration token'
    registration_token="$(curl --silent --show-error --fail --retry 5 --retry-connrefused \
        -H 'Accept: application/json' -H "Authorization: Bearer $BEARER_TOKEN" \
        "${METADATA_URL%/}/runner-registration-token/")"
    ./config.sh --unattended --url "$REPOSITORY_URL" \
        --token "$registration_token" --name "$RUNNER_NAME" --labels "$RUNNER_LABELS" \
        --no-default-labels --ephemeral
    sudo ./svc.sh install runner
    sudo ./svc.sh start
fi

agent_id="$(jq -er '(if has("agent_id") then .agent_id else .agentId end) | if type == "number" and . > 0 and . == floor then . else error("invalid runner agent id") end' .runner)"
base_callback="${CALLBACK_URL%/}"
base_callback="${base_callback%/status}"
os_name="$(. /etc/os-release && printf '%s' "${NAME:-Linux}")"
os_version="$(. /etc/os-release && printf '%s' "${VERSION_ID:-unknown}")"
curl --silent --show-error --fail --retry 3 --retry-connrefused \
    -H 'Accept: application/json' -H "Authorization: Bearer $BEARER_TOKEN" \
    -H 'Content-Type: application/json' -d "{\"os_name\":\"$os_name\",\"os_version\":\"$os_version\",\"agent_id\":$agent_id}" \
    "${base_callback}/system-info/" >/dev/null || true
curl --silent --show-error --fail --retry 3 --retry-connrefused \
    -H 'Accept: application/json' -H "Authorization: Bearer $BEARER_TOKEN" \
    -H 'Content-Type: application/json' -d "{\"status\":\"idle\",\"message\":\"runner successfully installed\",\"agent_id\":$agent_id}" \
    "${base_callback}/status" >/dev/null
trap - ERR
