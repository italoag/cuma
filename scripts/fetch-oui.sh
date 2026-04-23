#!/usr/bin/env bash
# Downloads the latest IEEE OUI database and replaces data/oui.csv
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$SCRIPT_DIR/../data/oui.csv"

echo "Downloading IEEE OUI database..."
curl -fsSL "https://standards-oui.ieee.org/oui/oui.csv" -o "$OUT.tmp"

lines=$(wc -l < "$OUT.tmp")
echo "Downloaded $lines entries."

mv "$OUT.tmp" "$OUT"
echo "Saved to $OUT"
