#!/bin/bash
# Runs as root only through privileged-bootstrap.sh, which verifies the payload first.
set -euo pipefail
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
unset BASH_ENV ENV CDPATH
umask 022
[[ $EUID == 0 ]] || { echo 'User: run sudo -v, then the documented locked installation command.' >&2; exit 2; }
[[ "${OINK_MIC_INSTALL_VERIFIED:-}" == 1 ]] || { echo 'Run through privileged-bootstrap.sh.' >&2; exit 2; }
destination=/Library/Audio/Plug-Ins/HAL/OinkMicrophone.driver
# One-time migration exception: remove the shipped legacy bundle, never route to its UIDs.
legacy_destination=/Library/Audio/Plug-Ins/HAL/AcrossMicrophone.driver
backup_root='/Library/Application Support/Oink/AudioPluginBackups'
# Display-only progress: fixed root-owned directory, random token, atomic receipt.
progress() {
  case "$1" in verify|copy_plugin|remove_plugin|restart_audio|finish) ;; *) return 2 ;; esac
  [[ "${OINK_MIC_OPERATION_TOKEN:-}" =~ ^[a-f0-9]{32}$ ]] || return 0
  local directory=/private/var/run/oink-microphone-progress
  [[ ! -L "$directory" ]] || return 2
  if [[ ! -d "$directory" ]]; then /bin/mkdir -m 755 "$directory"; fi
  [[ "$(/usr/bin/stat -f '%u:%Lp' "$directory")" == 0:755 ]] || return 2
  local receipt="$directory/$OINK_MIC_OPERATION_TOKEN.tmp"
  (umask 022; printf '{"token":"%s","step":"%s","detail":""}\n' "$OINK_MIC_OPERATION_TOKEN" "$1" > "$receipt")
  /bin/chmod 644 "$receipt"
  /bin/mv -f "$receipt" "$directory/status.json"
  printf 'OINK_MIC_STEP %s %s\n' "$1" "$SECONDS"
}

mode="${1:-}"
progress verify
case "$mode" in
  install)
    source_bundle="${2:?Specify the built OinkMicrophone.driver}"
    [[ -d "$source_bundle" && ! -L "$source_bundle" ]] || exit 2
    codesign --verify --strict "$source_bundle"
    [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$source_bundle/Contents/Info.plist")" == com.hachiwii.oink.microphone.driver ]] || exit 2
    # Read all output: grep -q can give codesign SIGPIPE after a match under pipefail.
    codesign -dv --verbose=2 "$source_bundle" 2>&1 | /usr/bin/grep -x 'TeamIdentifier=893V37X2CW' >/dev/null || { echo 'Unexpected signing team' >&2; exit 2; }
    ;;
  uninstall) ;;
  *) echo 'Usage: install.sh install <bundle> | uninstall' >&2; exit 2 ;;
esac
if [[ "$mode" == install ]]; then progress copy_plugin; else progress remove_plugin; fi
# Reject either symlink before modifying either installation.
for previous in "$destination" "$legacy_destination"; do
  [[ ! -L "$previous" ]] || { echo 'Refusing symlink destination' >&2; exit 2; }
  if [[ -e "$previous" ]]; then
    [[ -d "$previous" ]] || { echo 'Refusing non-directory destination' >&2; exit 2; }
    if [[ "$previous" == "$destination" ]]; then
      identifier=com.hachiwii.oink.microphone.driver
    else
      identifier=com.across.microphone.driver
    fi
    # Verify every existing bundle before backups/removal, including the legacy
    # name. Never trust a data-directory receipt or the filename alone.
    [[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$previous/Contents/Info.plist")" == "$identifier" ]] || { echo 'Unexpected installed microphone bundle ID' >&2; exit 2; }
    codesign --verify --strict -R "=identifier \"$identifier\" and anchor apple generic and certificate leaf[subject.OU] = \"893V37X2CW\"" "$previous"
  fi
done
for previous in "$destination" "$legacy_destination"; do
  if [[ -e "$previous" ]]; then
    mkdir -p "$backup_root"
    backup="$backup_root/$(date -u +%Y%m%dT%H%M%SZ)-$$-$(basename "$previous")"
    ditto "$previous" "$backup"
    echo "Previous plugin preserved at: $backup"
  fi
done
if [[ "$mode" == install ]]; then
  staging="$(mktemp -d /Library/Audio/Plug-Ins/HAL/.oink-mic-XXXXXX)"
  trap 'rm -rf "$staging"' EXIT
  ditto "$source_bundle" "$staging/OinkMicrophone.driver"
  chown -R root:wheel "$staging/OinkMicrophone.driver"
  # coreaudiod reads the plugin as _coreaudiod. Set the modes explicitly: the
  # bootstrap clones its snapshot with cp under umask 077, so the copied
  # directories are 0700 and ditto would carry that into the HAL folder.
  chmod -R u=rwX,go=rX "$staging/OinkMicrophone.driver"
  codesign --verify --strict "$staging/OinkMicrophone.driver"
  rm -rf "$destination"
  mv "$staging/OinkMicrophone.driver" "$destination"
  rm -rf "$legacy_destination"
else
  rm -rf "$destination" "$legacy_destination"
fi
# User approved interruption of audio; launchd restarts the audio server.
progress restart_audio
killall coreaudiod
echo "HAL plugin $mode complete; verify device discovery before releasing the test lock."
