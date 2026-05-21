import java.io.*;
import java.nio.*;
import java.nio.charset.*;
import java.util.*;
import java.util.zip.*;

/**
 * Generate binary AndroidManifest.xml (AXML) with Activity declaration,
 * then package APK with manifest + classes.dex + native libs.
 *
 * Based on reverse-engineering the aapt2 binary format.
 */
public class BuildArtApk {

    // --- AXML binary manifest generator ---

    static final int TYPE_STRING_POOL = 0x0001;
    static final int TYPE_RES_MAP     = 0x0180;
    static final int TYPE_NS_START    = 0x0100;
    static final int TYPE_NS_END      = 0x0101;
    static final int TYPE_START_ELEM  = 0x0102;
    static final int TYPE_END_ELEM    = 0x0103;

    static final int TYPE_STRING    = 0x03;
    static final int TYPE_INT_BOOL  = 0x12;

    static final int NO_RAW = 0xFFFFFFFF;

    static byte[] u16(int v) {
        return new byte[]{(byte)v, (byte)(v >>> 8)};
    }

    static byte[] u32(int v) {
        return new byte[]{(byte)v, (byte)(v >>> 8), (byte)(v >>> 16), (byte)(v >>> 24)};
    }

    static byte[] chunk(int type, int hdrSize, byte[] body) {
        byte[] out = new byte[8 + body.length];
        System.arraycopy(u16(type), 0, out, 0, 2);
        System.arraycopy(u16(hdrSize), 0, out, 2, 2);
        System.arraycopy(u32(8 + body.length), 0, out, 4, 4);
        System.arraycopy(body, 0, out, 8, body.length);
        return out;
    }

    static byte[] encodeStringUTF16(String s) {
        // u16(charCount) + UTF-16LE bytes + null(u16 0x0000)
        byte[] encoded = s.getBytes(StandardCharsets.UTF_16LE);
        byte[] out = new byte[2 + encoded.length + 2];
        System.arraycopy(u16(s.length()), 0, out, 0, 2);
        System.arraycopy(encoded, 0, out, 2, encoded.length);
        // null terminator already 0x00 0x00
        return out;
    }

    static byte[] buildStringPool(String[] strings) {
        int num = strings.length;

        // Build string data
        byte[][] encoded = new byte[num][];
        int[] offsets = new int[num];
        int dataLen = 0;
        for (int i = 0; i < num; i++) {
            offsets[i] = dataLen;
            encoded[i] = encodeStringUTF16(strings[i]);
            dataLen += encoded[i].length;
        }
        // Pad to 4-byte boundary
        int padLen = (4 - dataLen % 4) % 4;

        int stringsStart = 28 + num * 4; // offset from chunk start

        // Build body: stringCount + styleCount + flags + stringsStart + stylesStart + offsets + data
        int bodySize = 20 + num * 4 + dataLen + padLen;
        byte[] body = new byte[bodySize];
        int pos = 0;

        // Header fields
        System.arraycopy(u32(num), 0, body, pos, 4); pos += 4;       // stringCount
        System.arraycopy(u32(0), 0, body, pos, 4); pos += 4;          // styleCount
        System.arraycopy(u32(0), 0, body, pos, 4); pos += 4;          // flags (0 = UTF-16)
        System.arraycopy(u32(stringsStart), 0, body, pos, 4); pos += 4; // stringsStart
        System.arraycopy(u32(0), 0, body, pos, 4); pos += 4;          // stylesStart

        // Offsets
        for (int i = 0; i < num; i++) {
            System.arraycopy(u32(offsets[i]), 0, body, pos, 4);
            pos += 4;
        }

        // String data
        for (int i = 0; i < num; i++) {
            System.arraycopy(encoded[i], 0, body, pos, encoded[i].length);
            pos += encoded[i].length;
        }
        // Padding already zero

        return chunk(TYPE_STRING_POOL, 28, body);
    }

    static byte[] buildResMap(int numStrings, Map<Integer, Integer> resourceIds) {
        byte[] body = new byte[numStrings * 4];
        for (int i = 0; i < numStrings; i++) {
            int resId = resourceIds.getOrDefault(i, 0);
            System.arraycopy(u32(resId), 0, body, i * 4, 4);
        }
        return chunk(TYPE_RES_MAP, 8, body);
    }

    static byte[] nsStartEnd(int type, int prefix, int uri) {
        byte[] body = new byte[16];
        System.arraycopy(u32(0), 0, body, 0, 4);           // line
        System.arraycopy(u32(0xFFFFFFFF), 0, body, 4, 4);   // comment
        System.arraycopy(u32(prefix), 0, body, 8, 4);       // prefix
        System.arraycopy(u32(uri), 0, body, 12, 4);         // uri
        return chunk(type, 16, body);
    }

    static byte[] attr(int ns, int name, int rawVal, int valType, int valData) {
        byte[] out = new byte[20];
        System.arraycopy(u32(ns), 0, out, 0, 4);
        System.arraycopy(u32(name), 0, out, 4, 4);
        System.arraycopy(u32(rawVal), 0, out, 8, 4);
        // Res_value: size(2) + res0(1) + type(1) + data(4)
        System.arraycopy(u16(8), 0, out, 12, 2);
        out[14] = 0; // res0
        out[15] = (byte)valType;
        System.arraycopy(u32(valData), 0, out, 16, 4);
        return out;
    }

    static byte[] startElem(int line, int ns, int name, byte[]... attrs) {
        int numAttrs = attrs.length;
        int attrsLen = numAttrs * 20;
        int bodySize = 28 + attrsLen;
        byte[] body = new byte[bodySize];
        int pos = 0;

        System.arraycopy(u32(line), 0, body, pos, 4); pos += 4;
        System.arraycopy(u32(0xFFFFFFFF), 0, body, pos, 4); pos += 4;
        System.arraycopy(u32(ns), 0, body, pos, 4); pos += 4;
        System.arraycopy(u32(name), 0, body, pos, 4); pos += 4;
        System.arraycopy(u16(20), 0, body, pos, 2); pos += 2; // attrStart
        System.arraycopy(u16(20), 0, body, pos, 2); pos += 2; // attrSize
        System.arraycopy(u16(numAttrs), 0, body, pos, 2); pos += 2;
        System.arraycopy(u16(0), 0, body, pos, 2); pos += 2; // idIdx
        System.arraycopy(u16(0), 0, body, pos, 2); pos += 2; // classIdx
        System.arraycopy(u16(0), 0, body, pos, 2); pos += 2; // styleIdx

        for (byte[] a : attrs) {
            System.arraycopy(a, 0, body, pos, 20);
            pos += 20;
        }

        return chunk(TYPE_START_ELEM, 16, body);
    }

    static byte[] endElem(int line, int ns, int name) {
        byte[] body = new byte[16];
        System.arraycopy(u32(line), 0, body, 0, 4);
        System.arraycopy(u32(0xFFFFFFFF), 0, body, 4, 4);
        System.arraycopy(u32(ns), 0, body, 8, 4);
        System.arraycopy(u32(name), 0, body, 12, 4);
        return chunk(TYPE_END_ELEM, 16, body);
    }

    static byte[] generateManifest() {
        // String pool indices:
        //   0: "android" (ns prefix)
        //   1: "application"
        //   2: "com.vproc.arttest"
        //   3: "http://schemas.android.com/apk/res/android" (ns URI)
        //   4: "manifest"
        //   5: "package"
        //   6: "name"
        //   7: "exported"
        //   8: "activity"
        //   9: "intent-filter"
        //  10: "action"
        //  11: "category"
        //  12: "android.intent.action.MAIN"
        //  13: "android.intent.category.LAUNCHER"
        //  14: "ArtTestActivity"
        String[] strings = {
            "android",
            "application",
            "com.vproc.arttest",
            "http://schemas.android.com/apk/res/android",
            "manifest",
            "package",
            "name",
            "exported",
            "activity",
            "intent-filter",
            "action",
            "category",
            "android.intent.action.MAIN",
            "android.intent.category.LAUNCHER",
            "ArtTestActivity",
        };

        byte[] sp = buildStringPool(strings);

        // Resource map: maps string pool index → framework resource ID
        // System package parser needs these for known elements like <activity>
        Map<Integer, Integer> resIds = new HashMap<>();
        resIds.put(6, 0x01010003);  // "name" → attr/name
        resIds.put(7, 0x01010010);  // "exported" → attr/exported
        byte[] resmap = buildResMap(strings.length, resIds);

        // Namespace start
        byte[] nsStart = nsStartEnd(TYPE_NS_START, 0, 3);

        // Build XML tree
        ByteArrayOutputStream xml = new ByteArrayOutputStream();

        try {
            xml.write(nsStart);

            // <manifest package="com.vproc.arttest">
            xml.write(startElem(1, -1, 4,
                attr(-1, 5, 2, TYPE_STRING, 2)));

            // <application>
            xml.write(startElem(2, -1, 1));

            // <activity name="ArtTestActivity" exported="true">
            xml.write(startElem(3, -1, 8,
                attr(0, 6, 14, TYPE_STRING, 14),
                attr(0, 7, NO_RAW, TYPE_INT_BOOL, 0xFFFFFFFF)));

            // <intent-filter>
            xml.write(startElem(4, -1, 9));

            // <action name="android.intent.action.MAIN"/>
            xml.write(startElem(5, -1, 10,
                attr(0, 6, 12, TYPE_STRING, 12)));
            xml.write(endElem(5, -1, 10));

            // <category name="android.intent.category.LAUNCHER"/>
            xml.write(startElem(6, -1, 11,
                attr(0, 6, 13, TYPE_STRING, 13)));
            xml.write(endElem(6, -1, 11));

            xml.write(endElem(4, -1, 9));   // </intent-filter>
            xml.write(endElem(3, -1, 8));   // </activity>
            xml.write(endElem(2, -1, 1));   // </application>
            xml.write(endElem(1, -1, 4));   // </manifest>

            // Namespace end
            xml.write(nsStartEnd(TYPE_NS_END, 0, 3));

        } catch (IOException e) {
            throw new RuntimeException(e);
        }

        // Final assembly: file header + string pool + resmap + xml
        byte[] xmlBytes = xml.toByteArray();
        int totalSize = 8 + sp.length + resmap.length + xmlBytes.length;

        ByteArrayOutputStream out = new ByteArrayOutputStream();
        try {
            out.write(u16(0x0003));       // type
            out.write(u16(8));             // headerSize
            out.write(u32(totalSize));     // total size
            out.write(sp);
            out.write(resmap);
            out.write(xmlBytes);
        } catch (IOException e) {
            throw new RuntimeException(e);
        }

        return out.toByteArray();
    }

    // --- APK packaging ---

    static void buildApk(String outputApk, String manifestPath, String classesDexPath,
                          String[] nativeLibPaths) throws Exception {
        byte[] manifestData;
        if (manifestPath.equals("--generate")) {
            manifestData = generateManifest();
            System.out.println("Generated binary manifest: " + manifestData.length + " bytes");
        } else {
            manifestData = readFile(manifestPath);
            System.out.println("Read manifest: " + manifestPath + " (" + manifestData.length + " bytes)");
        }

        byte[] classesDex = readFile(classesDexPath);

        try (ZipOutputStream zout = new ZipOutputStream(new FileOutputStream(outputApk))) {
            // AndroidManifest.xml (STORED)
            addStored(zout, "AndroidManifest.xml", manifestData);

            // Minimal resources.arsc (required by package installer)
            addStored(zout, "resources.arsc", minimalResourcesArsc());

            // classes.dex (STORED)
            addStored(zout, "classes.dex", classesDex);

            // Native libraries under lib/arm64-v8a/
            for (String libPath : nativeLibPaths) {
                String libName = new File(libPath).getName();
                String arcName = "lib/arm64-v8a/" + libName;
                byte[] libData = readFile(libPath);
                addStored(zout, arcName, libData);
                System.out.println("  Added: " + arcName + " (" + libData.length + " bytes)");
            }
        }

        System.out.println("Created: " + outputApk + " (" + new File(outputApk).length() + " bytes)");
    }

    static void addStored(ZipOutputStream zout, String name, byte[] data) throws Exception {
        ZipEntry entry = new ZipEntry(name);
        entry.setMethod(ZipEntry.STORED);
        entry.setSize(data.length);
        entry.setCompressedSize(data.length);
        entry.setCrc(crc32(data));
        zout.putNextEntry(entry);
        zout.write(data);
        zout.closeEntry();
    }

    static long crc32(byte[] data) {
        java.util.zip.CRC32 crc = new java.util.zip.CRC32();
        crc.update(data);
        return crc.getValue();
    }

    static byte[] readFile(String path) throws Exception {
        return java.nio.file.Files.readAllBytes(java.nio.file.Paths.get(path));
    }

    static byte[] minimalResourcesArsc() {
        // 40-byte minimal resources.arsc (matches aapt2 output)
        // ResTable header: type=0x0002, hdrSize=12, chunkSize=40, packageCount=0
        // Nested empty StringPool: type=0x0001, hdrSize=28, chunkSize=28
        return new byte[] {
            0x02, 0x00, 0x0c, 0x00, 0x28, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
            0x01, 0x00, 0x1c, 0x00, 0x1c, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x01, 0x00, 0x00, 0x00,
            0x1c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        };
    }

    // --- Verification ---

    static void verifyManifest(byte[] data) {
        // Parse the header
        if (data.length < 8) {
            System.err.println("ERROR: Manifest too small");
            return;
        }
        int type = u16at(data, 0);
        int hdrSize = u16at(data, 2);
        int totalSize = u32at(data, 4);
        System.out.printf("File header: type=0x%04X hdrSize=%d totalSize=%d%n", type, hdrSize, totalSize);

        // Parse string pool
        int pos = 8;
        int spType = u16at(data, pos);
        int spHdrSize = u16at(data, pos + 2);
        int spSize = u32at(data, pos + 4);
        System.out.printf("StringPool: type=0x%04X hdrSize=%d chunkSize=%d%n", spType, spHdrSize, spSize);

        int strCount = u32at(data, pos + 8);
        int stringsStart = u32at(data, pos + 20);
        System.out.printf("  strings=%d stringsStart=%d%n", strCount, stringsStart);

        // Verify chunk sizes add up
        int resmapStart = pos + spSize;
        int rmType = u16at(data, resmapStart);
        int rmSize = u32at(data, resmapStart + 4);
        System.out.printf("ResMap: type=0x%04X chunkSize=%d%n", rmType, rmSize);

        int xmlStart = resmapStart + rmSize;
        System.out.printf("XML chunks start at: %d%n", xmlStart);
        System.out.printf("Expected total: 8 + %d + %d + %d = %d (actual: %d)%n",
            spSize, rmSize, data.length - xmlStart, 8 + spSize + rmSize + (data.length - xmlStart), totalSize);

        if (totalSize == data.length) {
            System.out.println("Size check: PASS");
        } else {
            System.err.println("Size check: FAIL (header says " + totalSize + ", file is " + data.length + ")");
        }
    }

    static int u16at(byte[] d, int off) {
        return (d[off] & 0xFF) | ((d[off+1] & 0xFF) << 8);
    }

    static int u32at(byte[] d, int off) {
        return (d[off] & 0xFF) | ((d[off+1] & 0xFF) << 8)
             | ((d[off+2] & 0xFF) << 16) | ((d[off+3] & 0xFF) << 24);
    }

    // --- Main ---

    public static void main(String[] args) throws Exception {
        if (args.length >= 1 && args[0].equals("--verify")) {
            // Verify mode: read binary manifest and check structure
            String path = args.length > 1 ? args[1] : "build_art/AndroidManifest.xml";
            byte[] data = readFile(path);
            verifyManifest(data);
            return;
        }

        if (args.length >= 1 && args[0].equals("--manifest-only")) {
            // Generate only the binary manifest
            String outPath = args.length > 1 ? args[1] : "build_art/AndroidManifest.xml";
            byte[] data = generateManifest();
            java.nio.file.Files.write(java.nio.file.Paths.get(outPath), data);
            System.out.println("Wrote: " + outPath + " (" + data.length + " bytes)");
            verifyManifest(data);
            return;
        }

        // APK build mode: output.apk manifest classes.dex lib1.so [lib2.so ...]
        if (args.length < 4) {
            System.out.println("Usage:");
            System.out.println("  BuildArtApk --manifest-only [output.xml]");
            System.out.println("  BuildArtApk --verify [manifest.xml]");
            System.out.println("  BuildArtApk <output.apk> <manifest|\"--generate\"> <classes.dex> <lib.so> [lib2.so ...]");
            return;
        }

        String outputApk = args[0];
        String manifestPath = args[1];
        String classesDex = args[2];
        String[] libs = Arrays.copyOfRange(args, 3, args.length);

        buildApk(outputApk, manifestPath, classesDex, libs);
    }
}
