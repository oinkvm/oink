"""Build-time only acquisition. The vendor archive is redistributed unchanged."""
import hashlib
from pathlib import Path
import shutil
import urllib.request

VERSION = '45'
SHA256 = 'b950e39f01af1d04ea623c8f6d8eb9b6ea5c477c637295fabf20631c85116bfb'
URL = 'https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack45.zip'


def bundle(cache: Path, payload: Path):
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / 'VBCABLE_Driver_Pack45.zip'
    if not archive.exists():
        temporary = archive.with_suffix('.download')
        try:
            urllib.request.urlretrieve(URL, temporary)
            if hashlib.sha256(temporary.read_bytes()).hexdigest() != SHA256:
                raise RuntimeError('VB-CABLE archive hash mismatch')
            temporary.replace(archive)
        finally:
            temporary.unlink(missing_ok=True)
    if hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
        raise RuntimeError('VB-CABLE cached archive hash mismatch')
    relative = 'AudioPlugin/' + archive.name
    (payload / 'AudioPlugin').mkdir(exist_ok=True)
    shutil.copy2(archive, payload / relative)
    # Shipped 0.2.2 updaters admit the vendor ZIP, but reject a separate script.
    # The agent embeds/stages the installer; never alter the vendor archive.
    (payload / 'AudioPlugin/Microphone.ps1').unlink(missing_ok=True)
    return [relative]


def verify_extracted(manifest, extracted):
    import xml.etree.ElementTree as ET
    entries = [node for node in ET.parse(manifest).getroot().iter()
               if node.tag.rsplit('}', 1)[-1] == 'File' and node.get('Name') == 'VBCABLE_Driver_Pack45.zip']
    if len(entries) != 1:
        raise RuntimeError('MSI must contain one unchanged VB-CABLE archive')
    archive = extracted / 'File' / entries[0].get('Id')
    if hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
        raise RuntimeError('MSI VB-CABLE archive hash mismatch')
    scripts = [node for node in ET.parse(manifest).getroot().iter()
               if node.tag.rsplit('}', 1)[-1] == 'File' and node.get('Name') == 'Microphone.ps1']
    if scripts:
        raise RuntimeError('MSI must not install the embedded microphone script as a file')


def verify_cleanup(tables):
    actions = {row[0]: row for row in tables.get('CustomAction', [])}
    execute = {row[0]: row for row in tables.get('InstallExecuteSequence', [])}
    try:
        valid = (actions['CleanupMicrophone'][1:4] == ['3649', 'InstallerCA', 'CleanupMicrophone']
                 and execute['CleanupMicrophone'][1] == 'REMOVE~="ALL" AND NOT UPGRADINGPRODUCTCODE'
                 and int(execute['CheckQuiescence'][2]) < int(execute['CleanupMicrophone'][2])
                 < int(execute['CleanupUserData'][2]) < int(execute['InstallFinalize'][2])
                 and not any(row[0] == 'CleanupMicrophone' for row in tables.get('InstallUISequence', [])))
    except (KeyError, ValueError, IndexError):
        valid = False
    if not valid:
        raise RuntimeError('MSI microphone cleanup must be SYSTEM commit-only on standalone uninstall')


def cleanup_header(script):
    """Keep the same installer inside InstallerCA, independent of installed files."""
    payload = ("$Action='uninstall';$Cleanup=$true;\n" + script).encode('utf-8')
    chunks = [''.join(f'\\x{value:02x}' for value in payload[offset:offset+512])
              for offset in range(0, len(payload), 512)]
    return '#pragma once\ninline constexpr char MICROPHONE_CLEANUP[] =\n' + ''.join('"' + chunk + '"\n' for chunk in chunks) + ';\n'
