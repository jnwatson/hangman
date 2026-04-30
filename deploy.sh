#!/usr/bin/env bash
set -euo pipefail

# Deploy Dead Letters to production (currently hc4 / ServaRICA).
#
# What this does:
#   1. Builds the server binary (release)
#   2. Builds the Svelte frontend (static)
#   3. Uploads binary, static files, and dictionary to prod
#   4. Optionally uploads game cache (--push-cache only — see below)
#   5. Installs/updates the systemd service
#   6. Reloads nginx if config changed
#
# Cache handling:
#   By default the game cache is NOT pushed. Local game_cache/ is rarely the
#   canonical copy — fleet boxes (ccx63/hc4/ovh1/local) each push their own
#   caches via chain.sh, and a wholesale local push would clobber recent
#   fleet pushes. Pass --push-cache only when you actually want to overwrite
#   prod with local cache state.
#
# Prerequisites:
#   - SSH access as $DEPLOY_HOST (default root@38.49.208.110 = hc4)
#   - nginx already configured (done during initial setup)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# hc4 (ServaRICA) is current prod since the migration from the DigitalOcean
# droplet. Override via DEPLOY_HOST env var if prod moves again.
HOST="${DEPLOY_HOST:-root@38.49.208.110}"
REMOTE_DIR="/opt/dead-letters"
CACHE_DIR="$SCRIPT_DIR/game_cache"
WEB_DIR="$SCRIPT_DIR/web"
SERVER_DIR="$SCRIPT_DIR/server"
# Cache push is opt-in. Local game_cache is rarely the canonical copy —
# precompute fleet boxes push their own caches, and a stale local push
# would clobber recent fleet pushes.
PUSH_CACHE=false

for arg in "$@"; do
    case "$arg" in
        --push-cache) PUSH_CACHE=true ;;
        --skip-cache) ;;  # legacy no-op (skipping cache is now the default)
    esac
done

# --- Pre-flight checks ---

if [ "$PUSH_CACHE" = true ] && [ ! -d "$CACHE_DIR" ]; then
    echo "ERROR: --push-cache requested but game_cache/ not found at $CACHE_DIR"
    exit 1
fi

if ! ssh -o ConnectTimeout=5 -o BatchMode=yes "$HOST" "echo ok" > /dev/null 2>&1; then
    echo "ERROR: Cannot SSH to $HOST (set DEPLOY_HOST env var to override)"
    exit 1
fi

echo "=== Building server binary ==="
cargo build --release --manifest-path "$SERVER_DIR/Cargo.toml" 2>&1 | tail -3
BINARY="$SERVER_DIR/target/release/dead-letters-server"
if [ ! -x "$BINARY" ]; then
    echo "ERROR: Binary not found at $BINARY"
    exit 1
fi
echo "  Binary: $(ls -lh "$BINARY" | awk '{print $5}')"

echo ""
echo "=== Building frontend ==="
cd "$WEB_DIR"

# Ensure adapter-static is installed
if ! grep -q adapter-static package.json; then
    npm install -D @sveltejs/adapter-static 2>&1 | tail -3
fi

# Swap to adapter-static for build
cp svelte.config.js svelte.config.js.bak
cat > svelte.config.js <<'EOF'
import adapter from '@sveltejs/adapter-static';
import { relative, sep } from 'node:path';

const config = {
	compilerOptions: {
		runes: ({ filename }) => {
			const relativePath = relative(import.meta.dirname, filename);
			const pathSegments = relativePath.toLowerCase().split(sep);
			const isExternalLibrary = pathSegments.includes('node_modules');
			return isExternalLibrary ? undefined : true;
		}
	},
	kit: {
		adapter: adapter({
			pages: 'build',
			assets: 'build',
			fallback: 'index.html',
			strict: false
		})
	}
};

export default config;
EOF

npm run build 2>&1 | tail -10
mv svelte.config.js.bak svelte.config.js
cd "$SCRIPT_DIR"

if [ ! -d "$WEB_DIR/build" ]; then
    echo "ERROR: Frontend build output not found"
    exit 1
fi
echo "  Static files: $(du -sh "$WEB_DIR/build" | awk '{print $1}')"

echo ""
echo "=== Preparing remote directories ==="
ssh "$HOST" "sudo mkdir -p $REMOTE_DIR/{bin,data,static,cache} && sudo chown -R www:www $REMOTE_DIR && sudo chmod -R 775 $REMOTE_DIR"

echo ""
echo "=== Uploading server binary ==="
rsync -avz --progress "$BINARY" "$HOST:/tmp/dead-letters-server"
ssh "$HOST" "sudo mv /tmp/dead-letters-server $REMOTE_DIR/bin/ && sudo chmod +x $REMOTE_DIR/bin/dead-letters-server"

echo ""
echo "=== Uploading frontend ==="
rsync -avz --delete "$WEB_DIR/build/" "$HOST:/tmp/dead-letters-static/"
ssh "$HOST" "sudo rsync -a --delete /tmp/dead-letters-static/ $REMOTE_DIR/static/ && rm -rf /tmp/dead-letters-static"

echo ""
echo "=== Uploading dictionary ==="
rsync -avz "$SCRIPT_DIR/enable1.txt" "$HOST:/tmp/enable1.txt"
ssh "$HOST" "sudo mv /tmp/enable1.txt $REMOTE_DIR/data/"

if [ "$PUSH_CACHE" = true ]; then
    echo ""
    echo "=== Uploading game cache (--push-cache) ==="
    echo "  Cache size: $(du -sh "$CACHE_DIR" | awk '{print $1}')"
    rsync -avz --progress "$CACHE_DIR/" "$HOST:/tmp/game_cache/"
    # Cache pushes need lock.mdb cleanup so saferlmdb re-indexes the new
    # data.mdb (issue #70 — without this readers see stale ABSENT entries).
    ssh "$HOST" "sudo rsync -a /tmp/game_cache/ $REMOTE_DIR/cache/ && sudo chown -R www:www $REMOTE_DIR/cache && sudo find $REMOTE_DIR/cache -name lock.mdb -delete && rm -rf /tmp/game_cache"
else
    echo ""
    echo "=== Skipping cache upload (default; pass --push-cache to override) ==="
fi

echo ""
echo "=== Installing systemd service ==="
ssh "$HOST" "sudo tee /etc/systemd/system/dead-letters.service > /dev/null" <<'SERVICE'
[Unit]
Description=Dead Letters Game Server
After=network.target

[Service]
Type=simple
User=www
Group=www
WorkingDirectory=/opt/dead-letters
ExecStart=/opt/dead-letters/bin/dead-letters-server \
    --dictionary /opt/dead-letters/data/enable1.txt \
    --cache-dir /opt/dead-letters/cache \
    --port 3000
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info

# Hardening
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/opt/dead-letters /opt/dead-letters/cache
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
SERVICE

ssh "$HOST" "sudo systemctl daemon-reload && sudo systemctl enable dead-letters && sudo systemctl restart dead-letters"
echo "  Service started"

echo ""
echo "=== Updating nginx config ==="
rsync -avz "$SCRIPT_DIR/nginx/dead-letters" "$HOST:/tmp/dead-letters-nginx"
ssh "$HOST" "sudo cp /tmp/dead-letters-nginx /etc/nginx/sites-enabled/dead-letters && rm /tmp/dead-letters-nginx && sudo nginx -t && sudo systemctl reload nginx"
echo "  nginx reloaded"

echo ""
echo "=== Verifying ==="
sleep 2
ssh "$HOST" "sudo systemctl status dead-letters --no-pager | head -12"
echo ""

# Test health endpoint
if curl -sf "http://deadletters.fun/api/health" > /dev/null 2>&1; then
    echo "Health check: OK"
else
    echo "Health check: FAILED (may need a moment to load caches)"
    echo "Check logs: ssh $HOST 'sudo journalctl -u dead-letters -f'"
fi

echo ""
echo "=== Deploy complete ==="
echo "  https://deadletters.fun"
