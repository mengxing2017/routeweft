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

def fixture(dense=False, compressed=True, restriction_case=None, maxspeed="36"):
    strings = ['', 'highway', 'residential', 'maxspeed', maxspeed, 'name', 'Synthetic Road',
               'type', 'restriction', 'no_right_turn', 'from', 'to', 'via']
    if restriction_case == 'conditional-nested-only':
        strings += ['restriction:conditional', 'no_right_turn @ (Mo)']
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
    ways = [(10,[1,2,3]), (20,[3,4])]
    if restriction_case in ('empty-roles', 'nested-member', 'unapproved', 'absent-from-to'):
        ways.append((30, [1,2]))  # Independent retained road after quarantine.
    for way_id, refs in ways:
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
        elif restriction_case in ('empty-roles', 'nested-member', 'unapproved'):
            members = [(10, 1, 0), (20, 1, 0)]
            if restriction_case == 'nested-member':
                members.append((999, 2, 12))
        elif restriction_case in ('nested-only', 'conditional-nested-only'):
            members = [(999, 2, 12)]
        elif restriction_case == 'absent-from-to':
            members = [(998, 1, 10), (10, 1, 12), (999, 1, 11)]
        ids = [m[0] for m in members]
        deltas = [ids[0]] + [ids[i] - ids[i-1] for i in range(1, len(ids))]
        relation_id = 5652903 if restriction_case in ('empty-roles', 'nested-member', 'unapproved', 'absent-from-to', 'nested-only', 'conditional-nested-only') else 6843783
        rule_index = 6 if restriction_case == 'unapproved' else 9
        keys, values = ([7, 13], [8, 14]) if restriction_case == 'conditional-nested-only' else ([7, 8], [8, rule_index])
        relation = (integer(1, relation_id) + packed(2, keys) + packed(3, values)
                    + packed(8, [m[2] for m in members]) + packed(9, map(zig, deltas))
                    + packed(10, [m[1] for m in members]))
        group += data(4, relation)
    header = data(4,b'OsmSchema-V0.6') + (data(4,b'DenseNodes') if dense else b'')
    return block('OSMHeader',header,compressed) + block('OSMData',data(1,table)+data(2,group),compressed)

def append_padding(output, count):
    """Stream isolated nodes in small blocks to exercise cumulative decode limits.

    IDs start after the four routable fixture nodes. No roads or coverage from
    the base fixture are removed. Each block stays well below per-block limits.
    """
    for start in range(5, count + 5, 8192):
        size = min(8192, count + 5 - start)
        nodes = (packed(1, [zig(start)] + [zig(1)] * (size - 1))
                 + packed(8, [0] * size) + packed(9, [0] * size))
        output.write(block('OSMData', data(1, data(1, b'')) + data(2, data(2, nodes)), True))

def chain(output, count):
    """Stream a connected synthetic road exceeding legacy graph capacity."""
    output.write(block('OSMHeader', data(4, b'OsmSchema-V0.6') + data(4, b'DenseNodes'), True))
    for start in range(1, count + 1, 8192):
        size = min(8192, count + 1 - start)
        nodes = (packed(1, [zig(start)] + [zig(1)] * (size - 1))
                 + packed(8, [0] * size)
                 + packed(9, [zig((start - 1) * 10)] + [zig(10)] * (size - 1)))
        output.write(block('OSMData', data(1, data(1, b'')) + data(2, data(2, nodes)), True))
    table = b''.join(data(1, value) for value in [b'', b'highway', b'residential'])
    group = bytearray()
    for way, start in enumerate(range(1, count, 256), 1):
        size = min(257, count + 1 - start)
        payload = (integer(1, way) + packed(2, [1]) + packed(3, [2])
                   + packed(8, [zig(start)] + [zig(1)] * (size - 1)))
        group.extend(data(3, payload))
        if way % 128 == 0:
            output.write(block('OSMData', data(1, table) + data(2, group), True))
            group.clear()
    if group:
        output.write(block('OSMData', data(1, table) + data(2, group), True))

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output',type=Path)
    parser.add_argument('--dense',action='store_true')
    parser.add_argument('--raw',action='store_true')
    parser.add_argument('--restriction-case', choices=['missing-via', 'missing-reference', 'valid', 'empty-roles', 'nested-member', 'unapproved', 'absent-from-to', 'nested-only', 'conditional-nested-only'])
    parser.add_argument('--padding-nodes', type=int, default=0)
    parser.add_argument('--maxspeed', default='36')
    parser.add_argument('--chain-nodes', type=int, default=0)
    args = parser.parse_args()
    if args.padding_nodes < 0:
        parser.error('--padding-nodes must be nonnegative')
    if args.chain_nodes and not 2 <= args.chain_nodes <= 10000000:
        parser.error('--chain-nodes must be between 2 and 10000000')
    with args.output.open('wb') as output:
        if args.chain_nodes:
            chain(output, args.chain_nodes)
        else:
            output.write(fixture(args.dense or args.padding_nodes > 0,not args.raw,args.restriction_case,args.maxspeed))
            append_padding(output, args.padding_nodes)
