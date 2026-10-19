"""Minimal native headers for package readers; never executable programs."""
import struct
import uuid

GUID = uuid.UUID('00112233-4455-6677-8899-aabbccddeeff')


def pe_image(guid=GUID):
    data = bytearray(0x600)
    data[:2] = b'MZ'
    struct.pack_into('<I', data, 0x3c, 0x80)
    data[0x80:0x84] = b'PE\0\0'
    struct.pack_into('<HH', data, 0x84, 0x8664, 2)
    struct.pack_into('<H', data, 0x94, 240)
    struct.pack_into('<H', data, 0x98, 0x20b)
    struct.pack_into('<II', data, 0x98 + 112 + 6 * 8, 0x3000, 28)
    for index, (name, size, rva, raw, flags) in enumerate([
            (b'.text', 0x500, 0x1000, 0x200, 0x60000020),
            (b'.rdata', 0x200, 0x3000, 0x400, 0x40000040)]):
        at = 0x98 + 240 + index * 40
        data[at:at + len(name)] = name
        struct.pack_into('<IIII', data, at + 8, size, rva, 0x200, raw)
        struct.pack_into('<I', data, at + 36, flags)
    struct.pack_into('<IIII', data, 0x400 + 12, 2, 24, 0x3040, 0x440)
    data[0x440:0x444] = b'RSDS'
    data[0x444:0x454] = guid.bytes_le
    struct.pack_into('<I', data, 0x454, 1)
    return bytes(data)


def macho_image():
    header = struct.pack('<IiiIIIII', 0xfeedfacf, 0x100000c, 0, 2, 2, 96, 0, 0)
    segment = struct.pack('<II16sQQQQiiII', 0x19, 72, b'__TEXT',
                          0x100000000, 0x8000, 0, 128, 5, 5, 0, 0)
    return header + segment + struct.pack('<II16s', 0x1b, 24, GUID.bytes)
