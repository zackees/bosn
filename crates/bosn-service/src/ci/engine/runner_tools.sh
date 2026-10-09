set -euo pipefail
# Archives from the publishers' GitHub release asset digests. The installed
# generation is saved by bosn's existing inner-.complete tool-cache protocol.
cache=/opt/hostedtoolcache/bosn-runner-tools/@TOOLS_ID@
# A cached generation is reused only while every installed file still matches
# the digests recorded when it was verified (#557): the tool cache is shared by
# every repository and every job can write to it.
if [ -f "$cache/.complete" ]; then
  if (cd "$cache" && sha256sum --quiet -c .sha256sums) >/dev/null 2>&1; then
    "$cache/bin/pwsh" --version
    "$cache/bin/gh" --version
    exit 0
  fi
  echo "bosn runner tools: the cached copy failed verification; reinstalling" >&2
  rm -rf "$cache"
fi
install=$(mktemp -d)
mkdir -p "$install/bin" "$install/powershell" "$install/github-cli"
curl -fsSL --retry 3 https://github.com/PowerShell/PowerShell/releases/download/v7.6.6/powershell-7.6.6-linux-x64.tar.gz -o "$install/powershell.tar.gz"
echo "ddbc4a2d113bbd46d283cfedcbcd117a70caefd7673f41f2b4e0000badf103bc  $install/powershell.tar.gz" | sha256sum -c -
tar -xzf "$install/powershell.tar.gz" -C "$install/powershell"
chmod +x "$install/powershell/pwsh"
curl -fsSL --retry 3 https://github.com/cli/cli/releases/download/v2.102.0/gh_2.102.0_linux_amd64.tar.gz -o "$install/github-cli.tar.gz"
echo "bb766f710eef8ede859c18578c72c327597cd4c8a85b06001b1f3843c6019386  $install/github-cli.tar.gz" | sha256sum -c -
tar -xzf "$install/github-cli.tar.gz" --strip-components=1 -C "$install/github-cli"
ln -s ../powershell/pwsh "$install/bin/pwsh"
ln -s ../github-cli/bin/gh "$install/bin/gh"
"$install/bin/pwsh" --version
"$install/bin/gh" --version
# Tarballs stay in the job container's temporary directory, not the saved
# generation; copy only the installed tools into the shared tool volume.
mkdir -p "$cache"
cp -a "$install/powershell" "$install/github-cli" "$install/bin" "$cache/"
(cd "$cache" && find powershell github-cli -type f -print0 | sort -z | xargs -0 sha256sum > .sha256sums)
touch "$cache/.complete"
