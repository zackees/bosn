#!/bin/sh
# Read-only, non-atomic samples. Never follow symlinks outside the cache.
root=/cache
sample() {
    tag=$1
    shift
    apparent=unknown
    allocated=unknown
    if value=$(du -sbc "$@" 2>/dev/null); then
        value=$(printf '%s\n' "$value" | tail -n 1)
        apparent=${value%%[[:space:]]*}
    fi
    if value=$(du -skc "$@" 2>/dev/null); then
        value=$(printf '%s\n' "$value" | tail -n 1)
        blocks=${value%%[[:space:]]*}
        allocated=$((blocks * 1024))
    fi
    printf '%s %s %s\n' "$tag" "$apparent" "$allocated"
}
sample total "$root"
for class in tools images actions toolcache toolstore-v1 actcache; do
    if [ "$class" = tools ]; then
        set --
        for path in "$root/tools" "$root/.act-maintenance-archive-v1.tgz" "$root/.act-maintenance-archive-pending-v1.tgz"; do
            if [ -e "$path" ] || [ -L "$path" ]; then
                set -- "$@" "$path"
            fi
        done
        if [ "$#" -gt 0 ]; then
            sample tools "$@"
        else
            printf 'tools 0 0\n'
        fi
        continue
    fi
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
