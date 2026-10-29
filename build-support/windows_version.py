"""Set Explorer's display strings without putting in-house labels in numeric PE fields."""
import ctypes
from ctypes import wintypes as W
from pathlib import Path
import struct


def display_version_resource(data, version):
    """Rewrite StringFileInfo, preserving VS_FIXEDFILEINFO and other resources."""
    def align(offset):
        return (offset + 3) & ~3

    def block(start, limit):
        length, size, kind = struct.unpack_from('<HHH', data, start)
        end = start + length
        if length < 8 or end > limit:
            raise ValueError('Invalid Windows version resource')
        at = start + 6
        while at + 2 <= end and data[at:at + 2] != b'\0\0':
            at += 2
        if at + 2 > end:
            raise ValueError('Unterminated Windows version key')
        key = data[start + 6:at].decode('utf-16le')
        value_at = align(at + 2)
        value_end = value_at + size * (2 if kind == 1 else 1)
        if value_end > end:
            raise ValueError('Truncated Windows version value')
        value = data[value_at:value_end]
        if kind == 1 and key in ('FileVersion', 'ProductVersion'):
            value = (version + '\0').encode('utf-16le')
            size = len(value) // 2
        result = bytearray(struct.pack('<HHH', 0, size, kind) + (key + '\0').encode('utf-16le'))
        result.extend(bytes(align(len(result)) - len(result)))
        result.extend(value)
        child = align(value_end)
        while child + 6 <= end:
            result.extend(bytes(align(len(result)) - len(result)))
            result.extend(block(child, end))
            child = align(child + struct.unpack_from('<H', data, child)[0])
        struct.pack_into('<H', result, 0, len(result))
        return result

    return bytes(block(0, len(data)))


def set_display_version(path, version):
    """Before hashing/signing, update every language of the PE version resource."""
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    signatures = {
        'LoadLibraryExW': ([W.LPCWSTR, W.HANDLE, W.DWORD], W.HMODULE),
        'FreeLibrary': ([W.HMODULE], W.BOOL),
        'FindResourceExW': ([W.HMODULE, ctypes.c_void_p, ctypes.c_void_p, W.WORD], W.HRSRC),
        'SizeofResource': ([W.HMODULE, W.HRSRC], W.DWORD),
        'LoadResource': ([W.HMODULE, W.HRSRC], W.HGLOBAL),
        'LockResource': ([W.HGLOBAL], ctypes.c_void_p),
        'BeginUpdateResourceW': ([W.LPCWSTR, W.BOOL], W.HANDLE),
        'UpdateResourceW': ([W.HANDLE, ctypes.c_void_p, ctypes.c_void_p, W.WORD, ctypes.c_void_p, W.DWORD], W.BOOL),
        'EndUpdateResourceW': ([W.HANDLE, W.BOOL], W.BOOL),
    }
    for name, (args, result) in signatures.items():
        function = getattr(kernel, name)
        function.argtypes, function.restype = args, result
    callback_type = ctypes.WINFUNCTYPE(W.BOOL, W.HMODULE, ctypes.c_void_p, ctypes.c_void_p, W.WORD, W.LPARAM)
    kernel.EnumResourceLanguagesW.argtypes = [W.HMODULE, ctypes.c_void_p, ctypes.c_void_p, callback_type, W.LPARAM]
    kernel.EnumResourceLanguagesW.restype = W.BOOL
    path = str(Path(path).resolve())
    module = kernel.LoadLibraryExW(path, None, 2)  # Data only; never execute product code.
    if not module:
        raise ctypes.WinError(ctypes.get_last_error())
    resources = []
    languages = []
    callback = callback_type(lambda _module, _type, _name, language, _param: languages.append(language) or True)
    try:
        if not kernel.EnumResourceLanguagesW(module, 16, 1, callback, 0):
            error = ctypes.get_last_error()
            if error not in (1812, 1813, 1814, 1815):
                raise ctypes.WinError(error)
        for language in languages:
            resource = kernel.FindResourceExW(module, 16, 1, language)
            pointer = kernel.LockResource(kernel.LoadResource(module, resource))
            size = kernel.SizeofResource(module, resource)
            if not pointer or not size:
                raise ctypes.WinError(ctypes.get_last_error())
            resources.append((language, display_version_resource(ctypes.string_at(pointer, size), version)))
    finally:
        kernel.FreeLibrary(module)
    if not resources:
        return False
    update = kernel.BeginUpdateResourceW(path, False)
    if not update:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        for language, data in resources:
            buffer = ctypes.create_string_buffer(data)
            if not kernel.UpdateResourceW(update, 16, 1, language, buffer, len(data)):
                raise ctypes.WinError(ctypes.get_last_error())
        if not kernel.EndUpdateResourceW(update, False):
            raise ctypes.WinError(ctypes.get_last_error())
        update = None
    finally:
        if update:
            kernel.EndUpdateResourceW(update, True)
    return True
