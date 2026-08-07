#!/usr/bin/env bash
set -euo pipefail

RUNTIME_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
LOCAL_ROOT=${AGENT_RUNTIME_LOCAL_ROOT:-"$RUNTIME_ROOT/.local"}
DEVCTL=${AGENT_RUNTIME_DEVCTL:-"$RUNTIME_ROOT/deploy/native/devctl"}
DOWNLOAD_WRAPPER=${AGENT_RUNTIME_DOWNLOAD_WRAPPER:-"$RUNTIME_ROOT/deploy/native/with-download-proxy"}
CARGO=${AGENT_RUNTIME_CARGO:-cargo}
MAVEN=${AGENT_RUNTIME_MAVEN:-mvn}

runtime_status=$($DEVCTL status)
grep -qx 'postgresql: running' <<<"$runtime_status" || {
  echo 'native PostgreSQL is not running; run make dev-native-start first' >&2
  exit 1
}
grep -qx 'nats: running' <<<"$runtime_status" || {
  echo 'native NATS is not running; run make dev-native-start first' >&2
  exit 1
}

environment_file="$LOCAL_ROOT/env/native.env"
[[ -f "$environment_file" ]] || {
  echo "native environment is missing: $environment_file" >&2
  exit 1
}
set -a
# shellcheck disable=SC1090
source "$environment_file"
set +a

required_variables=(
  AGENT_RUNTIME_LOCAL_NATS_URL
  AGENT_RUNTIME_NATS_USERNAME
  AGENT_RUNTIME_NATS_PASSWORD
  AGENT_RUNTIME_NATS_CA_CERT
  AGENT_RUNTIME_LOCAL_NATS_CONTROL_USERNAME
  AGENT_RUNTIME_LOCAL_NATS_CONTROL_PASSWORD
  AGENT_RUNTIME_LOCAL_NATS_TRUSTSTORE
  AGENT_RUNTIME_LOCAL_NATS_TRUSTSTORE_PASSWORD
)
for variable in "${required_variables[@]}"; do
  [[ -n "${!variable:-}" ]] || {
    echo "native environment variable is missing: $variable" >&2
    exit 1
  }
done

TEST_NATS_URL=$AGENT_RUNTIME_LOCAL_NATS_URL \
TEST_NATS_USERNAME=$AGENT_RUNTIME_NATS_USERNAME \
TEST_NATS_PASSWORD=$AGENT_RUNTIME_NATS_PASSWORD \
TEST_NATS_CA_CERT=$AGENT_RUNTIME_NATS_CA_CERT \
  "$DOWNLOAD_WRAPPER" "$CARGO" test --manifest-path "$RUNTIME_ROOT/runtime/Cargo.toml" \
    -p agent-nats-security --test live_tls -- --ignored --nocapture

TEST_NATS_URL=$AGENT_RUNTIME_LOCAL_NATS_URL \
TEST_NATS_USERNAME=$AGENT_RUNTIME_LOCAL_NATS_CONTROL_USERNAME \
TEST_NATS_PASSWORD=$AGENT_RUNTIME_LOCAL_NATS_CONTROL_PASSWORD \
TEST_NATS_TRUSTSTORE=$AGENT_RUNTIME_LOCAL_NATS_TRUSTSTORE \
TEST_NATS_TRUSTSTORE_PASSWORD=$AGENT_RUNTIME_LOCAL_NATS_TRUSTSTORE_PASSWORD \
  "$DOWNLOAD_WRAPPER" "$MAVEN" -q -f "$RUNTIME_ROOT/control-plane/pom.xml" \
    -Dtest=NatsConnectionSettingsLiveTest test

if grep -q 'Plaintext passwords detected' "$LOCAL_ROOT"/logs/nats.*.log 2>/dev/null; then
  echo 'native NATS loaded plaintext passwords' >&2
  exit 1
fi

echo 'verified native Rust and Java TLS clients, bcrypt authentication, invalid-password rejection, and worker subject ACL'
