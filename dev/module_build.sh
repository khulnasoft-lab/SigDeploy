#!/usr/bin/env bash

set -euo pipefail
set -x

SRC_DIR="modules"
MODULE="copilot"
OUT_DIR="$SRC_DIR/$MODULE/build/"
TAR_FILE="$OUT_DIR/copilot.tar.gz"
MODULES_TOML="modules.toml"

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"

if [[ ! -d "$SRC_DIR" ]]; then
  echo "Source directory '$SRC_DIR' does not exist!" >&2
  exit 1
fi

if [[ ! -f "$MODULES_TOML" ]]; then
  echo "Modules file '$MODULES_TOML' does not exist!" >&2
  exit 1
fi

# Extract table names as module names
MODULES=($(grep -oP '^\[\K[^\]]+' "$MODULES_TOML"))

if [[ ${#MODULES[@]} -eq 0 ]]; then
  echo "No modules found in '$MODULES_TOML'!" >&2
  exit 1
fi

for MODULE in "${MODULES[@]}"; do
  if [[ ! -d "$SRC_DIR/$MODULE" ]]; then
    echo "Module directory '$SRC_DIR/$MODULE' does not exist!" >&2
    exit 1
  fi
done

tar -czf "$TAR_FILE" -C "$SRC_DIR" "${MODULES[@]}"

echo "Build complete: $TAR_FILE"