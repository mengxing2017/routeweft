#!/usr/bin/env python3
"""Write a tiny synthetic PBF using only Python stdlib. No real OSM data.

Usage: python3 generate_fixture.py OUTPUT.osm.pbf [--dense] [--raw]
The four-node, two-way network is bidirectional and uses explicit 36 km/h.
This generator is original test code, not generated/copied protobuf code.
"""
import argparse
import struct
import zlib
from pathlib import Path

def var(n):
    n &= (1 << 64) - 1
    out = bytearray()
    while n > 127:
        out.append((n & 127) | 128)
        n >>= 7
    out.append(n)
    return bytes(out)

def integer(k, n):
    return var(k << 3) + var(n)

def data(k, b):
    return var((k << 3) | 2) + var(len(b)) + b

def zig(n):
    return (n << 1) ^ (n >> 63)

def packed(k, values):
    return data(k, b''.join(var(x) for x in values))

def block(kind, raw, compressed):
    blob = integer(2, len(raw)) + data(3, zlib.compress(raw)) if compressed else data(1, raw)
    header = data(1, kind.encode()) + integer(3, len(blob))
    return struct.pack('>I', len(header)) + header + blob

def fixture(dense=False, compressed=True, restriction_case=None):
    strings = ['', 'highway', 'residential', 'maxspeed', '36', 'name', 'Synthetic Road',
               'type', 'restriction', 'no_right_turn', 'from', 'to', 'via']
    table = b''.join(data(1, s.encode()) for s in strings)
    points = [(0, 0), (10000, 0), (20000, 0), (20000, 10000)]  # lon, lat / 1e7
    group = b''
    if dense:
        lons = [points[0][0]] + [points[i][0] - points[i-1][0] for i in range(1,4)]
        lats = [points[0][1]] + [points[i][1] - points[i-1][1] for i in range(1,4)]
        payload = packed(1, [zig(1)]*4) + packed(8, map(zig,lats)) + packed(9, map(zig,lons))
        group += data(2, payload)
    else:
        for i, (lon, lat) in enumerate(points,1):
            group += data(1, integer(1,zig(i)) + integer(8,zig(lat)) + integer(9,zig(lon)))
    for way_id, refs in [(10,[1,2,3]), (20,[3,4])]:
        deltas = [refs[0]] + [refs[i]-refs[i-1] for i in range(1,len(refs))]
        way = integer(1,way_id) + packed(2,[1,3,5]) + packed(3,[2,4,6]) + packed(8,map(zig,deltas))
        group += data(3,way)
    if restriction_case:
        # Original synthetic relation shaped like a malformed real-world input.
        members = [(10, 1, 10), (20, 1, 11)]
        if restriction_case == 'missing-reference':
            members.insert(1, (999, 0, 12))
        elif restriction_case == 'valid':
            members.insert(1, (3, 0, 12))
        ids = [m[0] for m in members]
        deltas = [ids[0]] + [ids[i] - ids[i-1] for i in range(1, len(ids))]
        relation = (integer(1, 6843783) + packed(2, [7, 8]) + packed(3, [8, 9])
                    + packed(8, [m[2] for m in members]) + packed(9, map(zig, deltas))
                    + packed(10, [m[1] for m in members]))
        group += data(4, relation)
    header = data(4,b'OsmSchema-V0.6') + (data(4,b'DenseNodes') if dense else b'')
    return block('OSMHeader',header,compressed) + block('OSMData',data(1,table)+data(2,group),compressed)

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output',type=Path)
    parser.add_argument('--dense',action='store_true')
    parser.add_argument('--raw',action='store_true')
    parser.add_argument('--restriction-case', choices=['missing-via', 'missing-reference', 'valid'])
    args = parser.parse_args()
    args.output.write_bytes(fixture(args.dense,not args.raw,args.restriction_case))
