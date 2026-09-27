#!/usr/bin/env bash
# Generates a random API key for bootstrap
set -euo pipefail
openssl rand -hex 32
