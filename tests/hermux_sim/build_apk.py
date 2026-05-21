#!/usr/bin/env python3
"""Assemble APK from base.apk + classes.dex + native libraries."""

import sys
import zipfile
import os

def main():
    if len(sys.argv) < 5:
        print(f"Usage: {sys.argv[0]} <output.apk> <base.apk> <classes.dex> "
              "<lib1.so> [lib2.so ...]")
        sys.exit(1)

    output_apk = sys.argv[1]
    base_apk = sys.argv[2]
    classes_dex = sys.argv[3]
    native_libs = sys.argv[4:]

    with zipfile.ZipFile(base_apk, 'r') as zin:
        with zipfile.ZipFile(output_apk, 'w') as zout:
            # Copy entries from base APK (manifest, resources)
            for item in zin.infolist():
                data = zin.read(item.filename)
                zout.writestr(item, data)

            # Add classes.dex (STORED, no compression)
            with open(classes_dex, 'rb') as f:
                dex_data = f.read()
            info = zipfile.ZipInfo('classes.dex')
            info.compress_type = zipfile.ZIP_STORED
            zout.writestr(info, dex_data)

            # Add native libraries under lib/arm64-v8a/
            for lib_path in native_libs:
                lib_name = os.path.basename(lib_path)
                arc_name = f'lib/arm64-v8a/{lib_name}'
                with open(lib_path, 'rb') as f:
                    lib_data = f.read()
                info = zipfile.ZipInfo(arc_name)
                info.compress_type = zipfile.ZIP_STORED
                zout.writestr(info, lib_data)
                print(f"  Added: {arc_name} ({len(lib_data)} bytes)")

    size = os.path.getsize(output_apk)
    print(f"Created: {output_apk} ({size} bytes)")

if __name__ == '__main__':
    main()
