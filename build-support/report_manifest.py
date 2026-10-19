"""Read actual linked image identities; no executable is run or modified."""
import json
from pathlib import Path
import struct
import uuid


def unpack(data, fmt, offset):
    try:
        return struct.unpack_from('<' + fmt, data, offset)
    except struct.error as error:
        raise ValueError('Truncated native image') from error


def image_identity(path):
    data = Path(path).read_bytes()
    if data[:4] == b'\xcf\xfa\xed\xfe':
        cpu, _, _, count, size, _, _ = unpack(data, 'iiIIIII', 4)
        if cpu != 0x100000c or size > len(data) - 32:
            raise ValueError('Expected a thin arm64 Mach-O image')
        offset, identifier, text_size = 32, None, None
        for _ in range(count):
            command, length = unpack(data, 'II', offset)
            if length < 8 or offset + length > 32 + size:
                raise ValueError('Invalid Mach-O load command')
            if command == 0x1b and length == 24:  # LC_UUID
                identifier = data[offset + 8:offset + 24]
            if command == 0x19 and data[offset + 8:offset + 24].rstrip(b'\0') == b'__TEXT':
                _, text_size, file_offset = unpack(data, 'QQQ', offset + 24)
                if file_offset != 0:
                    raise ValueError('Mach-O text is not based at the image header')
            offset += length
        if offset != 32 + size:
            raise ValueError('Mach-O load command size mismatch')
    elif data[:2] == b'MZ':
        pe, = unpack(data, 'I', 0x3c)
        if data[pe:pe + 4] != b'PE\0\0':
            raise ValueError('Invalid PE signature')
        machine, count = unpack(data, 'HH', pe + 4)
        optional_size, = unpack(data, 'H', pe + 20)
        optional = pe + 24
        magic, = unpack(data, 'H', optional)
        if machine != 0x8664 or magic != 0x20b or optional_size < 168:
            raise ValueError('Expected an x64 PE32+ image')
        sections, text_size = [], 0
        for index in range(count):
            at = optional + optional_size + index * 40
            virtual_size, rva, raw_size, raw_offset = unpack(data, 'IIII', at + 8)
            characteristics, = unpack(data, 'I', at + 36)
            sections.append((rva, raw_size, raw_offset))
            if characteristics & 0x20000000:  # IMAGE_SCN_MEM_EXECUTE
                text_size = max(text_size, rva + virtual_size)
        debug_rva, debug_size = unpack(data, 'II', optional + 112 + 6 * 8)
        debug = next((raw + debug_rva - rva for rva, size, raw in sections
                      if rva <= debug_rva and debug_rva + debug_size <= rva + size), None)
        if debug is None or debug_size % 28:
            raise ValueError('PE debug directory is missing or malformed')
        identifiers = []
        for at in range(debug, debug + debug_size, 28):
            kind, size, _, pointer = unpack(data, 'IIII', at + 12)
            if kind == 2 and size >= 24 and data[pointer:pointer + 4] == b'RSDS':
                identifiers.append(uuid.UUID(bytes_le=data[pointer + 4:pointer + 20]).bytes)
        if len(identifiers) != 1:
            raise ValueError('Expected one PE CodeView GUID')
        identifier = identifiers[0]
    else:
        raise ValueError('Unsupported native image')
    if not identifier or identifier == bytes(16) or not text_size:
        raise ValueError('Native image UUID or executable text range missing')
    # Crash offsets are image-relative, so this is the executable range's upper
    # bound, including the PE section RVA, rather than the .text byte count.
    return {'debug_id': list(identifier), 'text_size': text_size}


def write_manifest(path, source_tree, version, build, images):
    major, minor, patch = map(int, version.split('-')[0].split('.'))
    value = {'schema': 1, 'source_tree_sha256': source_tree,
             'version': {'major': major, 'minor': minor, 'patch': patch, 'build': build},
             'images': [{'slot': slot, **image_identity(image)} for slot, image in images]}
    encoded = json.dumps(value, separators=(',', ':')) + '\n'
    if len(encoded.encode()) > 8192:
        raise ValueError('Report manifest exceeds the runtime bound')
    Path(path).write_text(encoded)
    return value
