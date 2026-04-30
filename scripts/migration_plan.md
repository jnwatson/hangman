# Migration plan — DigitalOcean → ServaRICA

Move the dead-letters production server from DigitalOcean droplet + 250 GB volume (~$31/mo) to a ServaRICA Slim Slice 4 VPS ($12/mo). Single 500 GB NVMe means no separate volume to manage.

## Pre-cutover (in parallel with ServaRICA provisioning)

### 1. DNS TTL — already handled

User set TTL to 30 min (Squarespace minimum). No active users, so 30-min propagation gap is immaterial.

### 2. Inventory what's on current prod

Known:
- `/opt/dead-letters/bin/dead-letters-server` — Axum binary
- `/opt/dead-letters/static/` — built SPA
- `/opt/dead-letters/data/enable1.txt` — dictionary
- `/mnt/volume_nyc3_01/cache/` — ~128 GB of LMDB caches (k=2..15)
- `/etc/nginx/sites-enabled/dead-letters` — our config
- `/etc/letsencrypt/live/deadletters.fun/` — SSL certs (renewed via cert bot)
- `/etc/systemd/system/dead-letters.service` — systemd unit

Nothing else important on this box.

### 3. Preflight checks

```bash
ssh nic@64.225.9.155 'sudo systemctl status dead-letters --no-pager | head'
ssh nic@64.225.9.155 'sudo certbot certificates 2>&1 | head -20'
ssh nic@64.225.9.155 'sudo crontab -u root -l; sudo crontab -u nic -l'
```

Confirm no cron jobs or hidden services we'd miss.

## New-box setup

Assumes: ServaRICA Slim Slice 4 provisioned with Ubuntu 24.04, root SSH with key, IP = `$NEW_IP`.

### 4. Base provisioning (~15 min)

```bash
# As root on the new box:
apt-get update && apt-get upgrade -y
apt-get install -y nginx certbot python3-certbot-nginx rsync ufw

# Create nic user (with sudo), www user (service)
adduser --disabled-password --gecos '' nic
usermod -aG sudo nic
mkdir -p /home/nic/.ssh
# paste your laptop pubkey into /home/nic/.ssh/authorized_keys
chown -R nic:nic /home/nic/.ssh && chmod 700 /home/nic/.ssh && chmod 600 /home/nic/.ssh/authorized_keys

adduser --system --group --no-create-home --home /opt/dead-letters www

# Sudoers rules mirroring current prod (needed for chain.sh/safe_push.sh):
#   nic may cp/mv/chown/rmdir under /opt/dead-letters/cache and restart the service
echo 'nic ALL=(ALL) NOPASSWD: /usr/bin/cp, /usr/bin/mv, /bin/chown, /usr/bin/rmdir, /bin/systemctl restart dead-letters, /bin/mkdir -p /opt/dead-letters/cache_stage/*' | sudo tee /etc/sudoers.d/dead-letters
chmod 440 /etc/sudoers.d/dead-letters

# Firewall
ufw allow 22/tcp && ufw allow 80/tcp && ufw allow 443/tcp && ufw --force enable
```

Note: cache moves to `/opt/dead-letters/cache` (same filesystem as binaries — NVMe, plenty of room). `cache_stage/` sits alongside it. Atomic `mv` stays cheap since it's a rename within one FS.

### 5. Sync the cache

Rsync is restartable, so even if it takes hours it's hands-off. Simplest path: from the laptop (which has SSH to both boxes):

```bash
rsync -a --info=progress2 nic@64.225.9.155:/mnt/volume_nyc3_01/cache/ /tmp/cache_mirror/
rsync -a --info=progress2 /tmp/cache_mirror/ nic@$NEW_IP:/home/nic/cache_bootstrap/
```

Or direct if you set up SSH keys between old and new prod (saves laptop disk):
```bash
ssh nic@64.225.9.155 'rsync -a /mnt/volume_nyc3_01/cache/ nic@'$NEW_IP':/home/nic/cache_bootstrap/'
```

Then: `ssh root@$NEW_IP 'mkdir -p /opt/dead-letters && mv /home/nic/cache_bootstrap /opt/dead-letters/cache && chown -R www:www /opt/dead-letters/cache'`.

This cache (128 GB, ~14 k entries) doesn't change between runs — once synced, it's synced. Follow-up incremental rsyncs during cutover are cheap (only net-new entries from any chain pushes since).

### 6. Deploy the server (via modified deploy.sh)

Make a copy of `deploy.sh` with `HOST="nic@$NEW_IP"`, `REMOTE_DIR="/opt/dead-letters"`, and in the systemd unit change `--cache-dir /mnt/volume_nyc3_01/cache` to `--cache-dir /opt/dead-letters/cache`.

Also drop the `ReadWritePaths` reference to `/mnt/volume_nyc3_01/cache` and replace with `/opt/dead-letters/cache`.

Run with `--skip-cache` since the cache was pre-synced in step 5:
```bash
bash deploy.sh.new --skip-cache
```

### 7. Nginx + SSL

nginx config (`nginx/dead-letters`) already uses `/opt/dead-letters/static` — no change needed.

SSL via HTTP-01: after DNS flips, run `sudo certbot --nginx -d deadletters.fun -d www.deadletters.fun`. There will be a brief window with a broken cert after the DNS cutover; since there are no active users this is fine.

## Cutover (~5 minutes)

### 8. Final incremental rsync (caches may have moved)

Do one more pass to catch any chain pushes that landed between bootstrap and cutover:
```bash
ssh nic@64.225.9.155 'sudo rsync -a --info=progress2 /mnt/volume_nyc3_01/cache/ nic@'$NEW_IP':/opt/dead-letters/cache/'
ssh root@$NEW_IP 'chown -R www:www /opt/dead-letters/cache'
```

### 9. Switch DNS

Point A records to `$NEW_IP`. With TTL 60, propagation is near-instant.

### 10. Verify service

```bash
curl -sf https://deadletters.fun/api/health
curl -sf https://deadletters.fun/  # check SPA loads
# Play a test game end-to-end
```

## Post-cutover

### 11. Update all compute-box references to new prod

On each of **hc4, cax41-k6, cax41-k7, local, and anywhere in `scripts/`**:

```bash
# hc4, cax41-k6, cax41-k7 — update chain.sh and safe_push.sh:
sed -i 's/nic@64\.225\.9\.155/nic@NEW_IP/g; s|/mnt/volume_nyc3_01/cache|/opt/dead-letters/cache|g' /root/chain.sh /root/safe_push.sh

# local — /tmp/chain_local.sh, /tmp/watcher_local.sh, /tmp/safe_push_local.sh
sed -i 's/nic@64\.225\.9\.155/nic@NEW_IP/g; s|/mnt/volume_nyc3_01/cache|/opt/dead-letters/cache|g' /tmp/chain_local.sh /tmp/watcher_local.sh /tmp/safe_push_local.sh

# project repo
sed -i 's/64\.225\.9\.155/NEW_IP/g; s|/mnt/volume_nyc3_01/cache|/opt/dead-letters/cache|g' deploy.sh CLAUDE.md scripts/fleet.json
```

Don't forget `CLAUDE.md`'s "Production server" section.

### 12. Test a chain push end-to-end

Run `trace-sim` against the new prod:
```bash
scp target/release/trace-sim enable1.txt nic@$NEW_IP:/tmp/
ssh nic@$NEW_IP 'sudo -u www /tmp/trace-sim --dict /tmp/enable1.txt --length 10 --cache-dir /opt/dead-letters/cache --strategy adversarial --traces 1 --verbose'
```

Match the output against our baseline. Mismatch = data lost in transfer; match = clean migration.

### 13. Watch for 24-48 hours, then decommission

Keep the DO droplet + volume running but idle. After 1-2 days of stable operation:
- DO dashboard: destroy droplet
- DO dashboard: delete volume
- Cancel billing

Savings start immediately on destroy.

## Rollback (if something breaks in first 24 hours)

- Flip DNS back to `64.225.9.155` (TTL 60, so ~1 min to propagate)
- Old service is still running; games resume
- Diagnose new-box issue without pressure

## Backup

Cache is effectively immutable in operation (only grows when chains push). User keeps a local copy on laptop as the backup — adequate. No daily rsync needed.

## Estimated timing

- Pre-sync cache while provisioning: ~2-4 hours in parallel
- New-box setup: ~30-60 min
- Deploy + test: ~15 min
- Cutover: ~5 min (final rsync + DNS)
- **Total active attention: ~1-2 hours**
- Wall-clock from provisioning start to cutover: ~4-6 hours
