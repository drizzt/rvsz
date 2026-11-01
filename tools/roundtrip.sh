#!/bin/bash
# SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
#
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# usage: roundtrip.sh <bin> <workdir> <ref.zip>... : extract, repack, cmp
[ $# -ge 3 ] || { echo "usage: $0 <bin> <workdir> <ref.zip>..." >&2; exit 2; }
B=$1; W=$2; shift 2
for f in "$@"; do
  rm -rf "$W/rt"; mkdir -p "$W/rt"
  cp "$f" "$W/rt/ref.zip"
  "$B" extract "$W/rt/ref.zip" -o "$W/rt/x" || { echo "FAIL extract $f"; continue; }
  "$B" pack "$W/rt/x" -o "$W/rt/out.zip" >/dev/null || { echo "FAIL pack $f"; continue; }
  if cmp -s "$W/rt/ref.zip" "$W/rt/out.zip"; then echo "SAME $(stat -c%s "$f") $f"; else echo "DIFF $f"; fi
done
