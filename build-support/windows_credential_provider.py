"""Build the in-process Credential Provider with static CRT and private symbols."""
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
NAME = 'OinkCredentialProvider'


def build(output, environment=None):
    output = Path(output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    env = {key.upper(): value for key, value in (os.environ if environment is None else environment).items()}
    if not shutil.which('cl.exe', path=env.get('PATH')):
        vswhere = Path(env.get('PROGRAMFILES(X86)', r'C:\Program Files (x86)')) / 'Microsoft Visual Studio/Installer/vswhere.exe'
        installation = subprocess.check_output([str(vswhere), '-latest', '-products', '*', '-requires',
                                                'Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
                                                '-property', 'installationPath'], text=True, env=env).strip()
        devcmd = Path(installation) / 'Common7/Tools/VsDevCmd.bat'
        if not installation or not devcmd.is_file():
            raise RuntimeError('MSVC x64 build tools are required')
        lines = subprocess.check_output(f'cmd.exe /d /s /c ""{devcmd}" -arch=x64 >nul && set"', text=True, env=env)
        for line in lines.splitlines():
            key, separator, value = line.partition('=')
            if separator and key:
                env[key.upper()] = value
    source = ROOT / 'crates/oink-credential-provider'
    compiler = shutil.which('cl.exe', path=env.get('PATH'))
    inspector = shutil.which('dumpbin.exe', path=env.get('PATH'))
    if not compiler or not inspector:
        raise RuntimeError('MSVC compiler and import inspector are required')
    resource_compiler = shutil.which('rc.exe', path=env.get('PATH'))
    if not resource_compiler:
        raise RuntimeError('Windows resource compiler is required')
    release_build = int(env.get('OINK_RELEASE_BUILD', '0'))
    if not 0 <= release_build < 2**31:
        raise ValueError('invalid Credential Provider release build')
    numeric = (release_build >> 24, (release_build >> 16) & 255, release_build & 65535, 0)
    version = ','.join(map(str, numeric))
    resource = output / (NAME + '.rc')
    resource.write_text(f'''#include <windows.h>
1 VERSIONINFO
FILEVERSION {version}
PRODUCTVERSION {version}
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_DLL
BEGIN
 BLOCK "StringFileInfo"
 BEGIN
  BLOCK "040904b0"
  BEGIN
   VALUE "CompanyName", "Oink"
   VALUE "FileDescription", "Oink Credential Provider"
   VALUE "FileVersion", "{'.'.join(map(str, numeric))}"
   VALUE "ProductVersion", "{'.'.join(map(str, numeric))}"
   VALUE "OriginalFilename", "OinkCredentialProvider.dll"
  END
 END
 BLOCK "VarFileInfo"
 BEGIN
  VALUE "Translation", 0x409, 1200
 END
END
''', encoding='ascii')
    resource_output = output / (NAME + '.res')
    subprocess.run([resource_compiler, '/nologo', f'/fo{resource_output}', str(resource)], env=env, check=True)
    subprocess.run([compiler, '/nologo', '/LD', '/Z7', '/EHsc', '/std:c++17', '/W4', '/MT',
                    '/experimental:deterministic', f'/pathmap:{ROOT}=oink', f'/pathmap:{output}=oink/generated',
                    str(source / 'provider.cpp'), str(resource_output), f'/Fo{output}/{NAME}.obj', '/link',
                    f'/DEF:{source}/provider.def', '/DEBUG:FULL', f'/PDB:{output}/{NAME}.pdb',
                    '/PDBALTPATH:%_PDB%', f'/OUT:{output}/{NAME}.dll', f'/IMPLIB:{output}/{NAME}.lib',
                    'ole32.lib', 'secur32.lib', 'uuid.lib', 'shlwapi.lib', 'advapi32.lib', 'wtsapi32.lib'],
                   env=env, check=True)
    imports = subprocess.check_output([inspector, '/nologo', '/dependents', str(output / (NAME + '.dll'))], env=env, text=True)
    if any(name in imports.lower() for name in ('vcruntime', 'msvcp', 'msvcr', 'concrt')):
        raise RuntimeError('Credential Provider must use static MSVC CRT')


if __name__ == '__main__':
    import sys
    build(sys.argv[1])
