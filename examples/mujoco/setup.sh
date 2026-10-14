#!/usr/bin/env bash
# Fetches what the MuJoCo example needs and keel doesn't ship, at pinned
# versions, into ~/.cache/keel (or $KEEL_CACHE):
#
# - MuJoCo's release for Linux x86-64: the library keel-mujoco links;
# - the AgileX Piper arm from MuJoCo Menagerie (MIT), linked here as
#   examples/mujoco/piper for the scene to include.
#
# Then `cargo build` builds keel-mujoco with MuJoCo.
#
#   examples/mujoco/setup.sh
set -euo pipefail

cache=${KEEL_CACHE:-$HOME/.cache/keel}
mujoco=3.16.0
mujoco_sha256=139577c541d43716a63992ab698cbc7b09788b1d547fb82b682d59b40ef49b9d
menagerie=d7b481bac63d3d600ab404ee74f7474c5efe8a67
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$cache"

if [[ ! -f $cache/mujoco-$mujoco/lib/libmujoco.so ]]; then
  archive=$cache/mujoco-$mujoco-linux-x86_64.tar.gz
  echo "fetching MuJoCo $mujoco" >&2
  curl -sSLf -o "$archive" \
    "https://github.com/google-deepmind/mujoco/releases/download/$mujoco/mujoco-$mujoco-linux-x86_64.tar.gz"
  echo "$mujoco_sha256  $archive" | sha256sum -c --quiet
  tar xzf "$archive" -C "$cache"
fi

if [[ ! -f $cache/menagerie-$menagerie/agilex_piper/piper.xml ]]; then
  echo "fetching the Piper from MuJoCo Menagerie" >&2
  dir=$cache/menagerie-$menagerie
  rm -rf "$dir"
  git init -q "$dir"
  git -C "$dir" remote add origin https://github.com/google-deepmind/mujoco_menagerie.git
  git -C "$dir" sparse-checkout set agilex_piper
  git -C "$dir" fetch -q --depth 1 --filter=blob:none origin "$menagerie"
  git -C "$dir" checkout -q FETCH_HEAD
fi
ln -sfn "$cache/menagerie-$menagerie/agilex_piper" "$here/piper"

echo "MuJoCo in $cache/mujoco-$mujoco, the Piper in $here/piper: now cargo build" >&2
