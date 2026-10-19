"""Read security and registration rows through MSI handles, without installation.

WiX 5.0.2 decompilation omits CreateFolder PermissionEx rows. Check the actual
MsiLockPermissionsEx/CreateFolder relationships rather than reconstructed XML.
"""
import ctypes
import hashlib
from ctypes import wintypes


def read_content_tables(path):
    msi = ctypes.WinDLL('msi')
    uint = wintypes.UINT
    handle_pointer = ctypes.POINTER(uint)

    def function(name, arguments):
        call = getattr(msi, name)
        call.argtypes, call.restype = arguments, uint
        return call

    open_database = function('MsiOpenDatabaseW', [wintypes.LPCWSTR, wintypes.LPCWSTR, handle_pointer])
    open_view = function('MsiDatabaseOpenViewW', [uint, wintypes.LPCWSTR, handle_pointer])
    execute = function('MsiViewExecute', [uint, uint])
    fetch = function('MsiViewFetch', [uint, handle_pointer])
    field_count = function('MsiRecordGetFieldCount', [uint])
    get_string = function('MsiRecordGetStringW', [uint, uint, wintypes.LPWSTR, ctypes.POINTER(wintypes.DWORD)])
    read_stream = function('MsiRecordReadStream', [uint, uint, ctypes.c_void_p, ctypes.POINTER(wintypes.DWORD)])
    close = function('MsiCloseHandle', [uint])

    def checked(code):
        if code:
            raise OSError(code, 'MSI database read failed')

    database = uint()
    checked(open_database(str(path), None, ctypes.byref(database)))  # MSIDBOPEN_READONLY
    try:
        tables = {}
        for table in ('MsiLockPermissionsEx', 'CreateFolder', 'Property', 'Registry',
                      'Component', 'CustomAction', 'InstallExecuteSequence', 'Upgrade',
                      'Icon', 'Binary', 'Dialog', 'Shortcut', 'Control', 'ControlEvent', 'ControlCondition', 'InstallUISequence',
                      'AppSearch', 'RegLocator', 'CheckBox', 'Media'):
            view = uint()
            checked(open_view(database.value, f'SELECT * FROM `{table}`', ctypes.byref(view)))
            try:
                checked(execute(view.value, 0))
                rows = []
                while True:
                    record = uint()
                    result = fetch(view.value, ctypes.byref(record))
                    if result == 259:  # ERROR_NO_MORE_ITEMS
                        break
                    checked(result)
                    try:
                        row = []
                        for index in range(1, field_count(record.value) + 1):
                            if table in ('Icon', 'Binary') and index == 2:
                                content = bytearray()
                                while True:
                                    chunk = ctypes.create_string_buffer(4096)
                                    size = wintypes.DWORD(len(chunk))
                                    checked(read_stream(record.value, index, chunk, ctypes.byref(size)))
                                    content.extend(chunk.raw[:size.value])
                                    if not size.value:
                                        break
                                row.append(hashlib.sha256(content).hexdigest())
                                continue
                            value = ctypes.create_unicode_buffer(65536)
                            size = wintypes.DWORD(len(value))
                            checked(get_string(record.value, index, value, ctypes.byref(size)))
                            row.append(value.value)
                        rows.append(row)
                    finally:
                        close(record.value)
                tables[table] = rows
            finally:
                close(view.value)
        return tables
    finally:
        close(database.value)
