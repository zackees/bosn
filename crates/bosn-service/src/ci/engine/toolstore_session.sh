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
        plan|object:*)
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
            if [ "$operation" = plan ]; then
                "$work/bin/act" cache tool-publish --from "$source" --source-quiescent \
                    --cache-server-path "$store" --max-bytes @PAYLOAD@ --plan || code=$?
            else
                expected=${operation#object:}
                case "$expected" in ''|*[!0-9a-f]*) exit 78;; esac
                [ "${#expected}" -eq 64 ] || exit 78
                "$work/bin/act" cache tool-publish --from "$source" --source-quiescent \
                    --cache-server-path "$store" --max-bytes @PAYLOAD@ --apply \
                    --expected-object "$expected" || code=$?
            fi
            finish "$code" ;;
        usage)
            if [ ! -e "$store" ] && [ -f "$pending" ] && [ ! -e "$initial" ]; then
                printf 'absent\n'; finish 0; continue
            fi
            [ -d "$store" ] && [ ! -L "$store" ] || exit 78
            code=0
            "$work/bin/act" cache tool-usage --cache-server-path "$store" \
                --max-entries 1000000 || code=$?
            finish "$code" ;;
        filesystem)
            stat -f -c %S "$cache"
            finish 0 ;;
        installs)
            source=/var/lib/docker/volumes/act-toolcache/_data
            volumes=$(docker volume ls --format '{{.Name}}' --filter 'name=^act-toolcache$') || exit 78
            if [ -z "$volumes" ]; then finish 0; continue; fi
            [ "$volumes" = act-toolcache ] || exit 78
            actual=$(docker volume inspect --format '{{.Mountpoint}}' act-toolcache) || exit 78
            [ "$actual" = "$source" ] && [ -d "$source" ] && [ ! -L "$source" ] || exit 78
            writers=$(docker ps -q --filter volume=act-toolcache) || exit 78
            [ -z "$writers" ] || { finish 75; continue; }
            (cd "$source"
                for marker in */*/*.complete; do
                    [ -f "$marker" ] && [ ! -L "$marker" ] || continue
                    dir=${marker%.complete}
                    [ -d "$dir" ] && [ ! -L "$dir" ] || continue
                    printf '%s\n' "$dir"
                done
                find . -mindepth 3 -type f -name .complete | while IFS= read -r stamp; do
                    dir=${stamp%/.complete}; printf '%s\n' "${dir#./}"
                done)
            finish 0 ;;
        retain)
            [ -f "$published" ] || exit 78
            IFS= read -r allocated || exit 78
            case "$allocated" in ''|*[!0-9]*) exit 78;; esac
            [ "${#allocated}" -le 19 ] && [ "$allocated" -gt 0 ] || exit 78
            code=0
            "$work/bin/act" cache tool-retain --apply --cache-server-path "$store" \
                --expire-before @EXPIRES@ --max-allocated-bytes "$allocated" \
                --max-candidates 128 --max-entries 1000000 --max-payload-bytes @PAYLOAD@ || code=$?
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
        selection)
            [ -f "$published" ] || exit 78
            code=0
            "$work/bin/act" cache tool-current --installs --cache-server-path "$store" \
                --max-bytes @PAYLOAD@ || code=$?
            finish "$code" ;;
        initialize|replace:*)
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
                expected=${mode#replace:}
                case "$expected" in ''|*[!0-9a-f]*) exit 78;; esac
                [ "${#expected}" -eq 64 ] || exit 78
                "$work/bin/act" cache tool-update --replace --expected-generation "$expected" \
                    --manifest "$manifest_path" --cache-server-path "$store" \
                    --max-bytes @PAYLOAD@ --apply || code=$?
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
