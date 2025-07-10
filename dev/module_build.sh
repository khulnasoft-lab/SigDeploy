#!/usr/bin/env bash

set -e

MODULE_NAME="copilot.vim"
SOURCE_DIR="modules/$MODULE_NAME"
DIST_DIR="$SOURCE_DIR/dist"

echo "Packaging $MODULE_NAME..."

# Remove old dist and recreate it
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

# Create archive excluding the dist directory itself
tar --exclude='./dist' -czf "$DIST_DIR/copilot.tar.gz" -C "$SOURCE_DIR" .