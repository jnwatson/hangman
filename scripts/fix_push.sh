#!/bin/bash
# Recover from chain.sh silent push failure: removes stale lock.mdb on prod
# and restarts dead-letters service so saferlmdb re-indexes the new data.mdb.
# Use for any push that landed before the chain.sh fix was applied.
#
# Usage: scripts/fix_push.sh [K1] [K2] ...
#   if no args, fixes all 14 lengths.
set -u

KS="${@:-2 3 4 5 6 7 8 9 10 11 12 13 14 15}"

declare -A HASHES
HASHES[2]=07a0ba4703b5ae64
HASHES[3]=2b1858705b11a320
HASHES[4]=1c5a446e3c21477f
HASHES[5]=001681db3f9e04df
HASHES[6]=e5395d4ba5709f78
HASHES[7]=3bed095b96f6c1b9
HASHES[8]=0b6115514cee00b4
HASHES[9]=c3230bb401c0d42e
HASHES[10]=484da118e847b268
HASHES[11]=93f2e667c4b26199
HASHES[12]=120c84e50203ce6f
HASHES[13]=a7a882a8d4474446
HASHES[14]=a6f7dee94ccf66df
HASHES[15]=5691b3f2a75913e3

CMDS=""
for K in $KS; do
  H=${HASHES[$K]}
  CMDS+=" sudo rm -f /opt/dead-letters/cache/tt_len${K}_${H}/lock.mdb;"
done
CMDS+=" sudo systemctl restart dead-letters; sleep 2; cd /root/hangman2;"
for K in $KS; do
  H=${HASHES[$K]}
  CMDS+=" echo k=$K:; ./target/release/cache_diag --dict enable1.txt --length $K --cache-dir /opt/dead-letters/cache --path q 2>&1 | grep -E 'cache_entries|summary' | head -3;"
done

ssh -o BatchMode=yes hc4-prod "$CMDS"
