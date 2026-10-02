#!/usr/bin/env bash
# Simulate a CDN cache purge (local prototype).
#
# Production example — Cloudflare zone purge via API:
#
#   curl -X POST "https://api.cloudflare.com/client/v4/zones/${CF_ZONE_ID}/purge_cache" \
#     -H "Authorization: Bearer ${CF_API_TOKEN}" \
#     -H "Content-Type: application/json" \
#     --data '{"purge_everything":true}'
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p ci
echo "$(date -u '+%Y-%m-%dT%H:%M:%SZ') purged CDN cache" >> ci/purge.log
echo "purged"
