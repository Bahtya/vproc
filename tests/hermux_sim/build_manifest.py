#!/usr/bin/env python3
"""Generate a valid binary AndroidManifest.xml with Activity declaration.

Based on reverse-engineering the aapt2-generated format.
"""

import struct
import sys

def u16(val): return struct.pack('<H', val)
def u32(val): return struct.pack('<I', val & 0xFFFFFFFF)
def i32(val): return struct.pack('<i', val)
def chunk(ctype, hdr_size, body):
    return u16(ctype) + u16(hdr_size) + u32(hdr_size + len(body)) + body

def write_axml_manifest(output_path):
    # Strings: match aapt2 format exactly (UTF-16-LE, flags=0x0000)
    strings = [
        'android',                           # 0: ns prefix
        'application',                       # 1
        'com.vproc.arttest',                 # 2
        'http://schemas.android.com/apk/res/android',  # 3: ns URI
        'manifest',                          # 4
        'package',                           # 5
        'name',                              # 6
        'exported',                          # 7
        'activity',                          # 8
        'intent-filter',                     # 9
        'action',                            # 10
        'category',                          # 11
        'android.intent.action.MAIN',        # 12
        'android.intent.category.LAUNCHER',  # 13
        'ArtTestActivity',                   # 14
    ]
    num = len(strings)

    # Build string data (UTF-16-LE)
    str_data = b''
    offsets = []
    for s in strings:
        offsets.append(len(str_data))
        encoded = s.encode('utf-16-le')
        str_data += u16(len(s)) + encoded + b'\x00\x00'  # null terminator
    str_data += b'\x00' * ((4 - len(str_data) % 4) % 4)

    strings_start = 28 + num * 4  # offset from chunk start

    # String pool chunk
    sp_body = (u32(num) + u32(0) + u32(0x0000) + u32(strings_start) + u32(0)
               + b''.join(u32(o) for o in offsets) + str_data)
    sp = chunk(0x0001, 28, sp_body)

    # Resource map (empty - no framework resource IDs resolved)
    resmap = chunk(0x0180, 8, b'')

    # Namespace
    ns_body = u32(0) + u32(0) + i32(0) + i32(3)  # line, comment, prefix, uri
    ns_start = chunk(0x0100, 16, ns_body)

    # Attribute helper: ns(4) name(4) rawValue(4) + Res_value: size(2) res0(1) type(1) data(4)
    def attr(ns, name_idx, raw_val, val_type, val_data):
        return (i32(ns) + i32(name_idx) + u32(raw_val) +
                u16(8) + struct.pack('<BB', 0, val_type) + u32(val_data))

    # Start element helper
    def start_elem(line, ns, name, attrs_bytes):
        num_attrs = len(attrs_bytes) // 20
        body = (u32(line) + u32(0xFFFFFFFF) +  # line, comment
                i32(ns) + i32(name) +  # ns, name
                u16(20) + u16(20) + u16(num_attrs) +  # attrStart, attrSize, attrCount
                u16(0) + u16(0) + u16(0) +  # idIdx, classIdx, styleIdx
                attrs_bytes)
        return chunk(0x0102, 16, body)

    # End element helper
    def end_elem(line, ns, name):
        body = u32(line) + u32(0xFFFFFFFF) + i32(ns) + i32(name)
        return chunk(0x0103, 16, body)

    # TYPE_STRING = 0x03, TYPE_INT_BOOLEAN = 0x12
    NO_RAW = 0xFFFFFFFF

    # Build XML tree
    xml = ns_start

    # <manifest package="com.vproc.arttest">
    xml += start_elem(1, -1, 4, attr(-1, 5, 2, 0x03, 2))

    # <application>
    xml += start_elem(2, -1, 1, b'')

    # <activity name="ArtTestActivity" exported="true">
    xml += start_elem(3, -1, 8,
        attr(0, 6, 14, 0x03, 14) +       # name="ArtTestActivity" (ns=android)
        attr(0, 7, NO_RAW, 0x12, 0xFFFFFFFF))  # exported=true (ns=android)

    # <intent-filter>
    xml += start_elem(4, -1, 9, b'')

    # <action name="android.intent.action.MAIN"/>
    xml += start_elem(5, -1, 10, attr(0, 6, 12, 0x03, 12))
    xml += end_elem(5, -1, 10)

    # <category name="android.intent.category.LAUNCHER"/>
    xml += start_elem(6, -1, 11, attr(0, 6, 13, 0x03, 13))
    xml += end_elem(6, -1, 11)

    xml += end_elem(4, -1, 9)   # </intent-filter>
    xml += end_elem(3, -1, 8)   # </activity>
    xml += end_elem(2, -1, 1)   # </application>
    xml += end_elem(1, -1, 4)   # </manifest>

    # Namespace end
    ns_end = chunk(0x0101, 16, ns_body)
    xml += ns_end

    # Final assembly
    total = 8 + len(sp) + len(resmap) + len(xml)
    header = u16(0x0003) + u16(8) + u32(total)

    with open(output_path, 'wb') as f:
        f.write(header + sp + resmap + xml)
    print(f"Wrote {output_path} ({total} bytes)")

if __name__ == '__main__':
    write_axml_manifest(sys.argv[1] if len(sys.argv) > 1 else 'build_art/AndroidManifest.xml')
