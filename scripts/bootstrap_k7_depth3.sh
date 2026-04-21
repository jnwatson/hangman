#!/bin/bash
# Bootstrap CAX41 for k=7 depth=3: install, rsync source, rsync existing k=7 cache, build, run.
set -eu
IP="$1"
SSH_OPTS="-o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/home/nic/.ssh/known_hosts_hangman -o ConnectTimeout=15"
SSH="ssh $SSH_OPTS"
log() { echo "[$(date -Iseconds)] [${IP} k=7 d=3] $*"; }

log "waiting for SSH"
until $SSH root@$IP 'true' 2>/dev/null; do sleep 10; done
log "SSH up; waiting for /root/rust_ready"
until $SSH root@$IP 'test -f /root/rust_ready' 2>/dev/null; do sleep 15; done

log "rust ready; rsyncing source"
rsync -az --delete \
    --exclude=target --exclude=game_cache --exclude='game_cache_*' \
    --exclude=server/target --exclude=web/build --exclude=web/node_modules \
    --exclude='.git' --exclude='env' --exclude='notes' --exclude='old' --exclude='docs' \
    -e "$SSH" /home/nic/proj/hangman2/ root@$IP:/root/hangman2/

log "rsyncing existing k=7 depth=2 cache (~13 GB)"
$SSH root@$IP 'mkdir -p /root/hangman2/game_cache'
rsync -az --stats -e "$SSH" \
    /home/nic/proj/hangman2/game_cache/tt_len7_3bed095b96f6c1b9 \
    root@$IP:/root/hangman2/game_cache/ | tail -5

log "building release"
$SSH root@$IP 'source $HOME/.cargo/env && cd /root/hangman2 && cargo build --release --bin precompute 2>&1 | tail -3'

log "launching precompute --depth 3 in tmux"
$SSH root@$IP bash <<EOF
cat > /root/run_precompute.sh <<'RUN'
#!/bin/bash
NTFY=hangman2-compute-nic
HOST=\$(hostname)
curl -sd "START \$HOST k=7 depth=3 (cax41 nbg1)" "https://ntfy.sh/\$NTFY" > /dev/null
cd /root/hangman2
/usr/bin/time -v ./target/release/precompute --dict enable1.txt --lengths 7 --cache-dir game_cache --depth 3 --max-words 999999 > /root/precompute.log 2>&1
RC=\$?
curl -sd "END \$HOST k=7 depth=3 rc=\$RC" "https://ntfy.sh/\$NTFY" > /dev/null
RUN
chmod +x /root/run_precompute.sh
tmux new-session -d -s pc /root/run_precompute.sh
echo "tmux started"
EOF
log "DONE"
