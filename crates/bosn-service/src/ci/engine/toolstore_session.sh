set -eu
cache=@CACHE@
work=@WORK@
store="$cache/toolstore-v1"
pending="$cache/.bosn-tool-preparing-v1.json"
published="$cache/.bosn-tool-enrolled-v1.json"
initial="$cache/.bosn-tool-initial-installs-v1.json"
lock="$cache/.bosn-tool-control-v1.lock"
[ -d "$cache" ] && [ ! -L "$cache" ] || exit 78
for path in "$lock" "$pending" "$published" "$initial"; do
    [ ! -L "$path" ] && { [ ! -e "$path" ] || [ -f "$path" ]; } || exit 78
done
exec 7>>"$lock"
flock -x -n 7 || { printf 'bosn-tool-busy\n'; exit 75; }
umask 077
stage=
trap '[ -z "$stage" ] || rm -f "$stage"' EXIT
printf 'bosn-tool-ready\n'
# The host supplies canonical typed JSON. These bytes are durable authority,
# not shell code. A conflicting retry cannot replace an existing record.
publish_record() {
    target="$1"
    content="$2"
    [ ! -L "$target" ] && { [ ! -e "$target" ] || [ -f "$target" ]; } || return 78
    stage=$(mktemp "$cache/.tool-publication.XXXXXXXX") || return $?
    printf '%s' "$content" >"$stage"
    sync -f "$stage"
    if [ ! -e "$target" ]; then ln "$stage" "$target"; fi
    cmp -s "$stage" "$target" || { rm -f "$stage"; return 78; }
    sync -f "$cache"
    rm -f "$stage"
    stage=
}
finish() { printf '\nbosn-tool-end:%s\n' "$1"; }
while IFS= read -r operation; do
    case "$operation" in
        state)
            [ ! -L "$store" ] || exit 78
            if [ -f "$published" ]; then
                [ -d "$store" ] || exit 78
                printf 'published\n'; cat "$published"
            elif [ -f "$pending" ]; then
                [ ! -e "$initial" ] || [ -d "$store" ] || exit 78
                printf 'preparing\n'; cat "$pending"
            else
                [ ! -e "$store" ] && [ ! -e "$initial" ] || exit 78
                printf 'fresh\n'
            fi
            finish 0 ;;
        begin)
            [ ! -e "$published" ] && [ ! -e "$initial" ] && [ ! -e "$store" ] || exit 78
            IFS= read -r record || exit 78
            [ "${#record}" -le 4096 ] || exit 78
            publish_record "$pending" "$record"
            finish 0 ;;
        object)
            [ -f "$pending" ] || [ -f "$published" ] || exit 78
            # Paths are validated by the typed host boundary. Quoting also
            # keeps the install path separate from native CLI arguments.
            IFS= read -r source || exit 78
            [ "${#source}" -le 1024 ] || exit 78
            # A finished act process is insufficient: a detached job may still
            # own the source volume. Fail closed on a missing nested daemon.
            writers=$(docker ps -q --filter volume=act-toolcache) || exit 78
            [ -z "$writers" ] || { printf 'tool source still has live writers\n'; finish 75; continue; }
            code=0
            "$work/bin/act" cache tool-publish --from "$source" --source-quiescent \
                --cache-server-path "$store" --max-bytes @PAYLOAD@ --apply || code=$?
            finish "$code" ;;
        usage)
            [ -d "$store" ] && [ ! -L "$store" ] || exit 78
            code=0
            "$work/bin/act" cache tool-usage --cache-server-path "$store" \
                --max-entries 1000000 || code=$?
            finish "$code" ;;
        generation)
            # Recover selection-before-acknowledgement by deriving the exact
            # first generation from the immutable, durably frozen manifest.
            # Never accept an unrelated current selection as enrollment proof.
            [ -f "$pending" ] && [ -f "$initial" ] && [ ! -e "$published" ] || exit 78
            code=0
            "$work/bin/act" cache tool-generation --manifest "$initial" \
                --cache-server-path "$store" --max-bytes @PAYLOAD@ --apply || code=$?
            finish "$code" ;;
        initial)
            if [ -e "$initial" ]; then cat "$initial"; else printf 'absent\n'; fi
            finish 0 ;;
        current)
            # A missing selection has meaning only while preparing. Once
            # enrolled, native validation must establish the current state.
            selection="$store/.tool-current-v1.json"
            [ ! -L "$selection" ] || exit 78
            if [ ! -e "$selection" ] && [ -f "$pending" ] && [ ! -e "$published" ]; then
                printf 'absent\n'; finish 0
            else
                code=0
                "$work/bin/act" cache tool-current --cache-server-path "$store" \
                    --max-bytes @PAYLOAD@ || code=$?
                finish "$code"
            fi ;;
        initialize|update)
            mode="$operation"
            IFS= read -r manifest || exit 78
            [ "${#manifest}" -le 65535 ] || exit 78
            manifest_path="$work/tool-update.json"
            if [ "$mode" = initialize ]; then
                [ -f "$pending" ] && [ ! -e "$published" ] || exit 78
                # Freeze the first exact install set before selection. A
                # crash retries these objects, never an unrelated cold set.
                publish_record "$initial" "$manifest"
                manifest_path="$initial"
            else
                [ -f "$published" ] || exit 78
                printf '%s' "$manifest" >"$manifest_path"
            fi
            code=0
            if [ "$mode" = initialize ]; then
                "$work/bin/act" cache tool-update --initialize --manifest "$manifest_path" \
                    --cache-server-path "$store" --max-bytes @PAYLOAD@ --apply || code=$?
            else
                "$work/bin/act" cache tool-update --manifest "$manifest_path" \
                    --cache-server-path "$store" --max-bytes @PAYLOAD@ --apply || code=$?
            fi
            finish "$code" ;;
        acknowledge)
            [ -f "$pending" ] && [ -f "$initial" ] || exit 78
            IFS= read -r proof || exit 78
            [ "${#proof}" -le 4096 ] || exit 78
            # The host validates the actual selection before this command.
            publish_record "$published" "$proof"
            rm -f "$pending"
            sync -f "$cache"
            finish 0 ;;
        abort) exit 0 ;;
        *) exit 78 ;;
    esac
done
