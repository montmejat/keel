#!/bin/sh
# Runs a dataflow across containers standing in for machines: `robot` and
# `base` each run a keel daemon, and a third container runs the coordinator
# in the foreground. Ctrl-C stops the dataflow, then removes the containers.
#
#   examples/containers/run.sh [dataflow.yml]     # default: pipeline.yml here
#
# While it runs, from another terminal:
#   podman exec -it keel-robot $PWD/target/debug/keel top
set -eu

repo=$(cd "$(dirname "$0")/../.." && pwd)
dataflow=$(realpath "${1:-$repo/examples/containers/pipeline.yml}")
keel="$repo/target/debug/keel"
# Same glibc as a Fedora 44 host, so host-built binaries run as they are.
image=registry.fedoraproject.org/fedora-minimal:44
# The repository, at the same path as on the host. SELinux labelling is
# disabled rather than relabelling your files.
mount="--security-opt label=disable -v $repo:$repo:ro"

cargo build --quiet --manifest-path "$repo/Cargo.toml"
podman network exists keel || podman network create keel >/dev/null

cleanup() { podman rm --force --time 0 keel-robot keel-base >/dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup
for machine in robot base; do
    # shellcheck disable=SC2086
    podman run --detach --init --name "keel-$machine" --hostname "$machine" \
        --network keel --network-alias "$machine" --shm-size 1g $mount \
        "$image" "$keel" daemon --listen 0.0.0.0:7400 >/dev/null
done
sleep 1

# shellcheck disable=SC2086
podman run --rm --init --network keel $mount "$image" "$keel" run "$dataflow"
