#!/usr/bin/env bash
# Re-symbolicate an existing dtrace capture (/tmp/p2-host.stacks) with
# kernel symbols. Reuses the prior capture + perfmap; only the KASLR slide needs
# root (one dtrace read). Run via the `!` prefix so you can type the password:
#   ! bash tools/resymbolicate-kernel.sh
set -uo pipefail
cd "$(dirname "$0")/.."

STACKS=/tmp/p2-host.stacks
INFERNO="$HOME/.cargo/bin/inferno-flamegraph"
OUT=p2-host-kernel.svg

[ -f "$STACKS" ] || { echo "no $STACKS (run tools/p2-flamegraph.sh first)"; exit 1; }
MAP=$(ls -t /tmp/perf-*.map 2>/dev/null | head -1)
[ -n "$MAP" ] || { echo "no /tmp/perf-*.map found"; exit 1; }

# Prefer a (dense) KDK kernel that matches BOTH the running build and arch; else
# the on-disk one (sparse but has copyin/sosend/etc.). A KDK whose build differs
# from the running kernel has different symbol addresses, so it is worse than
# useless -- we refuse it rather than produce silently-wrong names.
ARCHSUF=$(uname -a | grep -oE 'T[0-9]+' | head -1 | tr 'A-Z' 'a-z')   # e.g. t6000
RUNBUILD=$(sw_vers -buildVersion)                                     # e.g. 23B81
KERN=""
for k in /Library/Developer/KDKs/*"$RUNBUILD"*/System/Library/Kernels/kernel.release.$ARCHSUF; do
  [ -f "$k" ] && KERN="$k" && break
done
if [ -z "$KERN" ]; then
  for k in /Library/Developer/KDKs/*/System/Library/Kernels/kernel.release.$ARCHSUF; do
    [ -f "$k" ] && echo "WARNING: $k exists but does NOT match running build $RUNBUILD; ignoring (its addresses would be wrong)."
  done
  echo "No build-matched KDK for $RUNBUILD/$ARCHSUF -> falling back to the SPARSE on-disk kernel."
  echo "         kernel names will be APPROXIMATE (large +offsets). Install KDK build $RUNBUILD for accurate names."
  KERN="/System/Library/Kernels/kernel.release.$ARCHSUF"
fi
[ -f "$KERN" ] || { echo "no kernel image found for $ARCHSUF"; exit 1; }

# A KDK ships the full symbol table in the .dSYM DWARF, not the kernel binary.
SYMSRC="$KERN"
DSYM="$KERN.dSYM/Contents/Resources/DWARF/$(basename "$KERN")"
[ -f "$DSYM" ] && SYMSRC="$DSYM"
echo "kernel image : $KERN"
echo "symbol source: $SYMSRC"

# Static symbol table (sorted by address) and the anchor symbol's static address.
nm -n "$SYMSRC" 2>/dev/null | awk 'NF>=2 && $1 ~ /^[0-9a-f]+$/ {print $1, $NF}' > /tmp/kernel.ksyms
NSYMS=$(wc -l < /tmp/kernel.ksyms)
if [ "$NSYMS" -lt 15000 ]; then
  echo "kernel syms  : $NSYMS  [SPARSE -> names approximate; install build-matched KDK for a dense (~43k) table]"
else
  echo "kernel syms  : $NSYMS  [dense -> names reliable]"
fi
ANCHOR=_copyin
STATIC=$(awk -v s="$ANCHOR" '$2==s{print $1; exit}' /tmp/kernel.ksyms)
[ -n "$STATIC" ] || { echo "anchor $ANCHOR not in symbol table"; exit 1; }

echo ">>> sudo dtrace reads the live address of $ANCHOR to compute the KASLR slide <<<"
RUNTIME=$(sudo dtrace -qn "BEGIN{ printf(\"0x%llx\n\", (unsigned long long)&\`copyin); exit(0); }" 2>/dev/null | grep -oE '0x[0-9a-f]+' | head -1)
if [ -z "$RUNTIME" ]; then
  echo "could not read live &copyin via dtrace; cannot determine slide"; exit 1
fi
SLIDE=$(python3 -c "print(hex(int('$RUNTIME',16) - int('$STATIC',16)))")
echo "anchor $ANCHOR: static=0x$STATIC runtime=$RUNTIME  ->  KASLR slide=$SLIDE"

python3 tools/symbolicate_stacks.py "$STACKS" "$MAP" --ksyms /tmp/kernel.ksyms --kslide "$SLIDE" > /tmp/p2-host-kernel.folded
echo "folded       : $(wc -l < /tmp/p2-host-kernel.folded) unique stacks"

"$INFERNO" \
  --title "netperf wasip2: kernel(symbolicated) + wasmtime host + wasm guest" \
  --subtitle "client sender 127.0.0.1 @997Hz; kernel=$(basename "$KERN") slide=$SLIDE" \
  --colors aqua \
  /tmp/p2-host-kernel.folded > "$OUT"
echo "WROTE $(pwd)/$OUT"
echo

# Show the top symbolicated kernel leaves (and their +offset = confidence).
echo "=== top kernel leaves (self-time; large +offset = sparse-table guess) ==="
python3 - "$OUT" <<'PY'
import re,collections,sys
tot=collections.Counter()
for line in open('/tmp/p2-host-kernel.folded'):
    i=line.rfind(' '); frames=line[:i].split(';'); cnt=int(line[i+1:])
    lf=frames[-1]
    if lf.startswith('kernel`'): tot[lf]+=cnt
total=sum(v for _,v in tot.items()) or 1
for fr,v in tot.most_common(15):
    print(f"  {100*v/total:5.1f}%  {fr}")
PY
