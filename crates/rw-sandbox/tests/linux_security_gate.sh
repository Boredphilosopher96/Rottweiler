#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
image="${ROTTWEILER_LINUX_SANDBOX_IMAGE:-rust:1.94-bookworm}"

docker run --rm --privileged \
  --mount "type=bind,source=${root},target=/workspace" \
  --mount type=volume,source=rottweiler-linux-sandbox-cargo,target=/usr/local/cargo/registry \
  --mount type=volume,source=rottweiler-linux-sandbox-target,target=/workspace/target \
  --workdir /workspace \
  --env CARGO_INCREMENTAL=0 \
  --env ROTTWEILER_REQUIRE_LINUX_SANDBOX=1 \
  "$image" \
  bash -eu -o pipefail -c '
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends iproute2 python3 util-linux >/dev/null
    cargo test --locked -p rw-sandbox --test linux_egress
    cargo test --locked -p rw-sandbox --test linux_helper_driver
    cargo test --locked -p rw-tools --test linux_safe_list_network
    cargo test --locked -p rw-cli --test agent_runtime binary_records_then_replays_a_complete_offline_tool_turn -- --exact
    cargo test --locked -p rw-cli --test agent_runtime bash_replay_serves_recorded_output_without_spawning_or_opening_a_socket -- --exact
    cargo test --locked -p rw-cli --test agent_runtime sigkill_mid_bash_waits_for_watchdog_then_recovers_opaque_checkpoint -- --exact
    cargo test --locked -p rw-cli --test agent_runtime sigint_mid_bash_closes_the_log_and_kills_the_process_group -- --exact
  '
