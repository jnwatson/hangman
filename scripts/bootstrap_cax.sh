#!/bin/bash
# bootstrap_cax.sh <ip> <k> <shard_i> <total_shards>
# Waits for cloud-init, rsyncs code, builds, launches precompute in tmux.
set -eu
IP="$1"; K="$2"; S="$3"; N="$4"
SSH_OPTS="-o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/home/nic/.ssh/known_hosts_hangman -o ConnectTimeout=10"
SSH="ssh $SSH_OPTS"
log() { echo "[$(date -Iseconds)] [${IP} k=${K} s=${S}/${N}] $*"; }

log "waiting for SSH"
until $SSH root@$IP 'true' 2>/dev/null; do sleep 10; done

log "SSH up; waiting for /root/rust_ready"
until $SSH root@$IP 'test -f /root/rust_ready' 2>/dev/null; do sleep 15; done

log "rust ready; rsyncing source"
rsync -az --delete \
    --exclude=target --exclude=game_cache --exclude='game_cache_*' \
    --exclude=server/target --exclude=web/build --exclude=web/node_modules \
    --exclude='.git' --exclude='env' --exclude='notes' --exclude='old' --exclude='docs' \
    -e "$SSH" \
    /home/nic/proj/hangman2/ root@$IP:/root/hangman2/

log "rsync done; building release"
$SSH root@$IP 'source $HOME/.cargo/env && cd /root/hangman2 && cargo build --release --bin precompute 2>&1 | tail -5'

log "build done; installing run script + launching tmux"
$SSH root@$IP bash <<EOF
set -eu
cat > /root/run_precompute.sh <<'RUN'
#!/bin/bash
NTFY=hangman2-compute-nic
HOST=\$(hostname)
curl -sd "START \$HOST k=${K} shard=${S}/${N}" "https://ntfy.sh/\$NTFY" > /dev/null
cd /root/hangman2
/usr/bin/time -v ./target/release/precompute --dict enable1.txt --lengths ${K} --cache-dir game_cache --depth 2 --max-words 999999 --shard ${S}/${N} > /root/precompute.log 2>&1
RC=\$?
curl -sd "END \$HOST k=${K} shard=${S}/${N} rc=\$RC" "https://ntfy.sh/\$NTFY" > /dev/null
RUN
chmod +x /root/run_precompute.sh
tmux new-session -d -s pc /root/run_precompute.sh
echo "tmux session started"
EOF
log "DONE"
