#!/bin/sh
# Read-only, non-atomic samples. Never follow symlinks outside the cache.
root=/cache
sample() {
    apparent=unknown
    allocated=unknown
    if value=$(du -sb "$2" 2>/dev/null); then
        apparent=${value%%[[:space:]]*}
    fi
    if value=$(du -sk "$2" 2>/dev/null); then
        blocks=${value%%[[:space:]]*}
        allocated=$((blocks * 1024))
    fi
    printf '%s %s %s\n' "$1" "$apparent" "$allocated"
}
sample total "$root"
for class in tools images actions toolcache actcache; do
    if [ -e "$root/$class" ] || [ -L "$root/$class" ]; then
        sample "$class" "$root/$class"
    else
        printf '%s 0 0\n' "$class"
    fi
done
namespaces() {
    for path in "$2"/*; do
        [ -d "$path" ] && [ ! -L "$path" ] || continue
        namespace=${path##*/}
        case "$namespace" in
            *[!0-9a-f]*) continue ;;
        esac
        [ "${#namespace}" -eq 16 ] || continue
        sample "$1:$namespace" "$path"
    done
}
# Never descend through a symlinked store root. Class totals remain inclusive.
if [ -d "$root/actcache" ] && [ ! -L "$root/actcache" ]; then
    namespaces namespace "$root/actcache"
    if [ -d "$root/actcache/cohort-v1" ] && [ ! -L "$root/actcache/cohort-v1" ]; then
        namespaces cohort-v1 "$root/actcache/cohort-v1"
    fi
fi
