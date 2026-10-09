set -euf
cache=@CACHE@
source="$cache/@CLASS@"
ledger="$cache/.bosn-@CLASS@-retirement-v1.json"
pending="$cache/.bosn-@CLASS@-intent-pending-v1"
directory=@LEASE_DIR@
[ -d "$cache" ] && [ ! -L "$cache" ] && [ -d "$directory" ] && [ ! -L "$directory" ] || exit 78
lock=@LEASE@
[ ! -L "$lock" ] && { [ ! -e "$lock" ] || [ -f "$lock" ]; } || exit 78
exec 8>>"$lock"
flock -x -n 8 || { printf 'bosn-actions-busy\n'; exit 75; }
umask 077
printf 'bosn-actions-ready\n'
finish() { printf '\nbosn-actions-end:%s\n' "$1"; }
identity() {
    [ -d "$1" ] && [ ! -L "$1" ] || return 78
    [ "$(stat -c '%d %i' "$1")" = "$2 $3" ] || return 78
}
observe() {
    printf 'cache '; stat -c '%d %i' "$cache"
    if [ ! -e "$1" ] && [ ! -L "$1" ]; then
        printf 'source absent\n'
    else
        [ -d "$1" ] && [ ! -L "$1" ] || return 78
        printf 'source '; stat -c '%d %i' "$1" | tr '\n' ' '
        size=$(du -sk "$1") || return $?
        printf '%s\n' "${size%%[[:space:]]*}"
    fi
}
argument() {
    IFS= read -r value || return 78
    set -- $value
    [ "$#" -eq 5 ] || return 78
    nonce="$1"; cache_device="$2"; cache_inode="$3"; device="$4"; inode="$5"
    case "$nonce" in ''|*[!0-9a-f-]*) return 78;; esac
    [ "${#nonce}" -eq 36 ] || return 78
    for value in "$cache_device" "$cache_inode" "$device" "$inode"; do
        case "$value" in ''|*[!0-9]*) return 78;; esac
        [ "${#value}" -le 20 ] || return 78
    done
    identity "$cache" "$cache_device" "$cache_inode" || return $?
    stage="$cache/.@CLASS@-retired-$nonce"
}
while IFS= read -r operation; do
    case "$operation" in
        observe) observe "$source"; finish 0;;
        mounts) cat /proc/self/mountinfo; finish 0;;
        ledger)
            [ ! -L "$ledger" ] || exit 78
            if [ -e "$ledger" ]; then
                [ -f "$ledger" ] && [ "$(stat -c '%a %u' "$ledger")" = '600 0' ] || exit 78
                [ "$(stat -c %s "$ledger")" -le 4096 ] || exit 78
                cat "$ledger"
            else printf 'absent\n'; fi
            finish 0;;
        prepare)
            argument || exit 78
            identity "$source" "$device" "$inode" || exit 78
            custody="$source/.bosn-@CLASS@-custody-v1"
            [ ! -L "$custody" ] || exit 78
            if [ -e "$custody" ]; then
                [ -f "$custody" ] && [ "$(stat -c '%a %u %h' "$custody")" = '600 0 1' ] || exit 78
            fi
            printf '%s' "$nonce" >"$custody"
            sync -f "$custody"
            prepared="$nonce"
            finish 0;;
        record)
            [ -n "${prepared:-}" ] || exit 78
            identity "$source" "$device" "$inode" || exit 78
            [ ! -e "$ledger" ] && [ ! -L "$ledger" ] && [ ! -L "$pending" ] || exit 78
            if [ -e "$pending" ]; then
                [ -f "$pending" ] && [ "$(stat -c '%a %u %h' "$pending")" = '600 0 1' ] || exit 78
            fi
            IFS= read -r record || exit 78
            [ "${#record}" -le 4096 ] || exit 78
            # One reserved slot is reusable after an interrupted pre-authority
            # write. No rename/deletion is allowed until the ledger is durable.
            printf '%s' "$record" >"$pending"
            sync -f "$pending"
            ln "$pending" "$ledger"
            sync -f "$cache"
            finish 0;;
        stage)
            argument || exit 78
            if [ -e "$stage" ] || [ -L "$stage" ]; then
                identity "$stage" "$device" "$inode" || exit 78
                if [ -e "$source" ] || [ -L "$source" ]; then
                    [ ! -L "$source" ] || exit 78
                    [ "$(stat -c '%d %i' "$source")" != "$device $inode" ] || exit 78
                fi
            elif [ -e "$source" ] || [ -L "$source" ]; then
                [ -d "$source" ] && [ ! -L "$source" ] || exit 78
                if [ "$(stat -c '%d %i' "$source")" = "$device $inode" ]; then
                    custody="$source/.bosn-@CLASS@-custody-v1"
                    [ -f "$custody" ] && [ ! -L "$custody" ] && [ "$(cat "$custody")" = "$nonce" ] || exit 78
                    mv -nT "$source" "$stage"
                    identity "$stage" "$device" "$inode" || exit 78
                    [ ! -e "$source" ] && [ ! -L "$source" ] || exit 78
                    sync -f "$cache"
                fi
            fi
            observe "$stage"; finish 0;;
        remove)
            argument || exit 78
            identity "$stage" "$device" "$inode" || exit 78
            rm -rf -- "$stage"
            [ ! -e "$stage" ] && [ ! -L "$stage" ] || exit 78
            sync -f "$cache"
            finish 0;;
        acknowledge)
            argument || exit 78
            [ ! -e "$stage" ] && [ ! -L "$stage" ] || exit 78
            rm -f -- "$ledger"
            sync -f "$cache"
            finish 0;;
        abort) exit 0;;
        *) exit 78;;
    esac
done
