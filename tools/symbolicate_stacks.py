#!/usr/bin/env python3
"""Merge a dtrace kernel+user stack capture with symbol sources.

dtrace sees JIT-compiled guest code only as bare hex addresses (anonymous mmap,
no Mach-O image), and on Apple Silicon it can't name most kernel frames either
(the running kernel's symbol table is stripped). We re-attach both:

  * wasm/guest frames  <- wasmtime's `--profile=perfmap` table (address -> fn)
  * kernel frames      <- a symbol-rich kernel (e.g. from the KDK), nm'd to a
                          static symbol table, plus the live KASLR slide so
                          runtime addresses can be mapped back to static ones.

Usage:
  symbolicate_stacks.py <dtrace.stacks> <perf-PID.map> \
      [--ksyms <nm-sorted.txt>] [--kslide 0xHEX]  > folded.txt

Capture is assumed to be `@[stack(), ustack()] = count()` (kernel frames
leaf-first, then user frames leaf-first); we reverse the whole list to get
root->leaf so kernel frames nest as the deepest children of user frames.
"""
import sys
import re
import bisect
import argparse
from collections import defaultdict

HEX = re.compile(r'^0x[0-9a-fA-F]+$')
OFFSET = re.compile(r'\+0x[0-9a-fA-F]+$')
# dtrace renders an unnamed kernel frame as `0xADDR` or `0xADDR+0xOFF`; the real
# PC is ADDR+OFF (it printed the nearest reference address plus the offset).
KADDR = re.compile(r'^(0x[0-9a-fA-F]+)(?:\+0x([0-9a-fA-F]+))?$')


def load_perfmap(path):
    best = {}
    with open(path, 'r', errors='replace') as f:
        for line in f:
            line = line.rstrip('\n')
            if not line:
                continue
            parts = line.split(' ', 2)
            if len(parts) < 3:
                continue
            try:
                start = int(parts[0], 16)
                size = int(parts[1], 16)
            except ValueError:
                continue
            name = parts[2]
            cur = best.get(start)
            if cur is None or (cur[1].startswith('wasm[') and not name.startswith('wasm[')):
                best[start] = (start + size, name)
    rows = sorted((s, e, n) for s, (e, n) in best.items())
    return [r[0] for r in rows], rows


def load_ksyms(path):
    """Read `nm`-style 'ADDR NAME' lines into a sorted (addr, name) table."""
    rows = []
    with open(path, 'r', errors='replace') as f:
        for line in f:
            parts = line.split()
            if len(parts) < 2:
                continue
            try:
                addr = int(parts[0], 16)
            except ValueError:
                continue
            rows.append((addr, parts[-1]))
    rows.sort()
    return [r[0] for r in rows], rows


def lookup_range(starts, rows, addr):
    i = bisect.bisect_right(starts, addr) - 1
    if i >= 0:
        s, e, name = rows[i]
        if s <= addr < e:
            return name
    return None


def lookup_nearest(starts, rows, addr):
    """Nearest preceding symbol + byte offset (nm output has no sizes).

    A small offset means a confident hit; a multi-KB offset means the address
    fell in a gap of this (sparse) symbol table and the name is the previous
    function, not necessarily the real one -- a denser KDK kernel fixes those.
    """
    i = bisect.bisect_right(starts, addr) - 1
    if i >= 0:
        return rows[i][1], addr - rows[i][0]
    return None, 0


def make_clean(wstarts, wrows, ksraw, kslide):
    ksyms = kstarts = None
    if ksraw is not None:
        kstarts, ksyms = ksraw

    def clean(frame):
        f = frame.strip()
        # Kernel frame that dtrace left as module`0xADDR
        if kslide is not None and ksyms is not None and '`0x' in f:
            mod, _, sym = f.partition('`')
            mk = KADDR.match(sym)
            if (mod.startswith('kernel') or mod == 'mach_kernel') and mk:
                runtime = int(mk.group(1), 16) + (int(mk.group(2), 16) if mk.group(2) else 0)
                name, off = lookup_nearest(kstarts, ksyms, runtime - kslide)
                if name is not None:
                    return 'kernel`%s+0x%x' % (name, off)
        # Bare JIT address -> wasm function
        if HEX.match(f):
            name = lookup_range(wstarts, wrows, int(f, 16))
            return 'wasm`' + name if name is not None else '[jit ' + f + ']'
        return OFFSET.sub('', f)
    return clean


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('stacks')
    ap.add_argument('perfmap')
    ap.add_argument('--ksyms')
    ap.add_argument('--kslide')
    a = ap.parse_args()

    wstarts, wrows = load_perfmap(a.perfmap)
    ksraw = load_ksyms(a.ksyms) if a.ksyms else None
    kslide = int(a.kslide, 16) if a.kslide else None
    clean = make_clean(wstarts, wrows, ksraw, kslide)

    folded = defaultdict(int)
    frames = []
    with open(a.stacks, 'r', errors='replace') as f:
        for raw in f:
            s = raw.strip()
            if s == '':
                continue
            if s.isdigit():
                if frames:
                    folded[';'.join(clean(fr) for fr in reversed(frames))] += int(s)
                frames = []
            else:
                frames.append(s)
    for k, v in sorted(folded.items()):
        sys.stdout.write('%s %d\n' % (k, v))


if __name__ == '__main__':
    main()
