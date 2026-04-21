#!/bin/bash
# Redeploy precompute with batched flushes to a CAX41.
# Usage: deploy_batched_flush.sh <ip> <k> <flush_every_n> <flush_at_entries>
set -eu
IP="$1"; K="$2"; FLUSH_EVERY="${3:-100}"; FLUSH_AT="${4:-10000000}"
SSH_OPTS="-o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/home/nic/.ssh/known_hosts_hangman -o BatchMode=yes"
SSH="ssh $SSH_OPTS"
log() { echo "[$(date -Iseconds)] [${IP} k=${K}] $*"; }

log "rsyncing updated source"
rsync -az --delete \
    --exclude=target --exclude=game_cache --exclude='game_cache_*' \
    --exclude=server/target --exclude=web/build --exclude=web/node_modules \
    --exclude='.git' --exclude='env' --exclude='notes' --exclude='old' --exclude='docs' \
    -e "$SSH" \
    /home/nic/proj/hangman2/ root@$IP:/root/hangman2/

log "building release (will reuse existing target dir)"
$SSH root@$IP 'source $HOME/.cargo/env && cd /root/hangman2 && cargo build --release --bin precompute 2>&1 | tail -3'

log "killing current precompute + tmux session; then relaunching"
$SSH root@$IP bash <<EOF
set -eu
tmux kill-session -t pc 2>/dev/null || true
pkill -TERM -f 'target/release/precompute' 2>/dev/null || true
sleep 3
pkill -KILL -f 'target/release/precompute' 2>/dev/null || true

cat > /root/run_precompute.sh <<RUN
#!/bin/bash
NTFY=hangman2-compute-nic
HOST=\\\$(hostname)
curl -sd "RESTART \\\$HOST k=${K} depth=3 (batched flush every ${FLUSH_EVERY})" "https://ntfy.sh/\\\$NTFY" > /dev/null
cd /root/hangman2
/usr/bin/time -v ./target/release/precompute \\
  --dict enable1.txt --lengths ${K} --cache-dir game_cache --depth 3 \\
  --max-words 999999 \\
  --flush-every-n-positions ${FLUSH_EVERY} \\
  --flush-at-cache-entries ${FLUSH_AT} \\
  > /root/precompute.log 2>&1
RC=\\\$?
curl -sd "END \\\$HOST k=${K} depth=3 rc=\\\$RC" "https://ntfy.sh/\\\$NTFY" > /dev/null
RUN
chmod +x /root/run_precompute.sh
tmux new-session -d -s pc /root/run_precompute.sh
sleep 3
echo "--- tmux ---"
tmux list-sessions
echo "--- log head ---"
head -8 /root/precompute.log 2>/dev/null || echo "(no log yet)"
EOF

log "DONE"
