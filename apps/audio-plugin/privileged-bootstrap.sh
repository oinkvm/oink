# Embedded verbatim in the agent binary. Executed only through the OS-owned
# administrator authorization dialog, never directly from a user-writable file.
set -euo pipefail
export PATH=/usr/bin:/bin:/usr/sbin:/sbin
unset BASH_ENV ENV CDPATH
umask 077
[[ $EUID == 0 && $# == 4 ]] || exit 2
source_app="$1"
action="$2"
expected_sha="$3"
export OINK_MIC_OPERATION_TOKEN="$4"
[[ "$OINK_MIC_OPERATION_TOKEN" =~ ^[a-f0-9]{32}$ ]] || exit 2
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

progress verify
[[ "$expected_sha" =~ ^[a-f0-9]{64}$ ]] || exit 2
case "$action" in install|uninstall) ;; *) exit 2 ;; esac
[[ "$source_app" == /*.app && ! -L "$source_app" ]] || exit 2
# Copy first, then verify the immutable root-owned snapshot. Verifying only the
# original ~/Applications bundle leaves a check/execute replacement race.
stage="$(/usr/bin/mktemp -d /private/var/tmp/oink-microphone.XXXXXX)"
trap '/bin/rm -rf "$stage"' EXIT
# APFS clone preserves the verified full-app snapshot without copying its data
# blocks. Fall back on volumes that do not support clonefile.
/bin/cp -cR "$source_app" "$stage/Oink.app" 2>/dev/null || /usr/bin/ditto "$source_app" "$stage/Oink.app"
app="$stage/Oink.app"
requirement='=identifier "com.hachiwii.oink" and anchor apple generic and certificate leaf[subject.OU] = "893V37X2CW"'
/usr/bin/codesign --verify --deep --strict -R "$requirement" "$app"
[[ ! -L "$app/Contents" && ! -L "$app/Contents/Resources" ]] || exit 2
payload="$app/Contents/Resources/AudioPlugin"
[[ -d "$payload" && ! -L "$payload" && -f "$payload/install.sh" && ! -L "$payload/install.sh" ]] || exit 2
[[ -d "$payload/OinkMicrophone.driver" && ! -L "$payload/OinkMicrophone.driver" ]] || exit 2
/usr/bin/codesign --verify --strict -R '=identifier "com.hachiwii.oink.microphone.driver" and anchor apple generic and certificate leaf[subject.OU] = "893V37X2CW"' "$payload/OinkMicrophone.driver"
app_sha="$(/usr/bin/plutil -extract source_tree_sha256 raw -o - "$app/Contents/Resources/oink-build.json")"
plugin_sha="$(/usr/libexec/PlistBuddy -c 'Print :OinkGitSHA' "$payload/OinkMicrophone.driver/Contents/Info.plist")"
[[ "$app_sha" == "$plugin_sha" && "$app_sha" == "$expected_sha" ]] || { echo 'Bundled plugin revision differs from application' >&2; exit 2; }
# This marker authorizes the product path, which has no repository/test harness.
# It is not a security boundary: the OS dialog and verified root snapshot are.
OINK_MIC_INSTALL_VERIFIED=1 /bin/bash "$payload/install.sh" "$action" "$payload/OinkMicrophone.driver"
