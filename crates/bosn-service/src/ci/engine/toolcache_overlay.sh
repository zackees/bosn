set -eu
store=@STORE@
generation=@GENERATION@
target=@TARGET@
upper=/var/lib/docker/bosn-tools-upper
work=/var/lib/docker/bosn-tools-work
reader="$store/.tool-generations-v1/$generation/.readers-v1.bolt"
lower="$store/.tool-generations-v1/$generation/tree"
# Check the live original reader before exposing its protected lower tree.
fd=$(tr '\000' '\n' </proc/1/environ | sed -n 's/^BOSN_TOOL_GENERATION_LEASE_FD=//p')
case "$fd" in ''|*[!0-9]*) echo 'native tool reader descriptor missing' >&2; exit 1 ;; esac
[ "$fd" -gt 2 ] || exit 1
[ -f "$reader" ] && [ ! -L "$reader" ] || exit 1
[ "$(readlink "/proc/1/fd/$fd")" = "$reader" ] || exit 1
[ "$(stat -Lc '%d:%i' "/proc/1/fd/$fd")" = "$(stat -Lc '%d:%i' "$reader")" ] || exit 1
[ -d "$lower" ] && [ ! -L "$lower" ] || exit 1
docker volume create act-toolcache >/dev/null
[ -d "$target" ] && [ ! -L "$target" ] || exit 1
[ -z "$(ls -A "$target")" ] || { echo 'native tool target is not empty' >&2; exit 1; }
[ ! -e "$upper" ] && [ ! -L "$upper" ] || exit 1
[ ! -e "$work" ] && [ ! -L "$work" ] || exit 1
mkdir "$upper" "$work"
mount -t overlay overlay -o "metacopy=on,lowerdir=$lower,upperdir=$upper,workdir=$work" "$target"
# Verify the effective kernel mount, including metacopy; a successful command
# alone does not establish the requested allocation/sharing contract.
awk -v target="$target" -v lower="$lower" -v upper="$upper" -v work="$work" '
$5 == target {
    count++
    separator = 0
    for (i = 7; i <= NF; i++) if ($i == "-") { separator = i; break }
    if (!separator || $(separator + 1) != "overlay") next
    split($6, mount_flags, ",")
    for (i in mount_flags) if (mount_flags[i] == "rw") mount_rw = 1
    split($(separator + 3), overlay_flags, ",")
    for (i in overlay_flags) {
        if (overlay_flags[i] == "rw") overlay_rw = 1
        if (overlay_flags[i] == "metacopy=on") metacopy = 1
        if (overlay_flags[i] == "lowerdir=" lower) lower_ok = 1
        if (overlay_flags[i] == "upperdir=" upper) upper_ok = 1
        if (overlay_flags[i] == "workdir=" work) work_ok = 1
    }
}
END { exit !(count == 1 && mount_rw && overlay_rw && metacopy && lower_ok && upper_ok && work_ok) }
' /proc/self/mountinfo
