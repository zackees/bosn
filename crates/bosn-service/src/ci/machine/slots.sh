# The shared engine's slot table (#544), run inside the engine as
# `sh -ec <this> sh <op> [args]`. Every operation holds one lock, so a
# lease never races a retirement. /run is the engine's private tmpfs: the
# table lives and dies with the engine.
#   lease <max> <holder...>  -> "slot N" | "preparing" | "retiring" | "full"
#   ready                    -> "ok"  (the maker prepared the engine)
#   release <n>              -> "ok"
#   survey                   -> "held N <holder>"..., "idle S", flags
#   request                  -> "ok"  (retire once empty)
#   retire                   -> survey, then "begun" when none is held
d=/run/bosn-slots
mkdir -p "$d"
exec 9>"$d/.lock"
flock 9
[ -e "$d/last" ] || touch "$d/last"
op=$1
shift
case "$op" in
lease)
  max=$1
  shift
  if [ -e "$d/retiring" ]; then echo retiring; exit 0; fi
  if [ ! -e "$d/ready" ]; then echo preparing; exit 0; fi
  i=0
  while [ "$i" -lt "$max" ]; do
    if mkdir "$d/$i" 2>/dev/null; then
      echo "$*" > "$d/$i/holder"
      touch "$d/last"
      echo "slot $i"
      exit 0
    fi
    i=$((i + 1))
  done
  echo full
  ;;
release)
  case "$1" in *[!0-9]* | '') echo "bad slot" >&2; exit 2 ;; esac
  rm -rf "${d:?}/$1"
  touch "$d/last"
  echo ok
  ;;
request)
  touch "$d/requested"
  echo ok
  ;;
ready)
  touch "$d/ready"
  echo ok
  ;;
survey | retire)
  held=0
  for s in "$d"/[0-9]*; do
    [ -d "$s" ] || continue
    held=$((held + 1))
    echo "held ${s##*/} $(cat "$s/holder" 2>/dev/null)"
  done
  echo "idle $(($(date +%s) - $(stat -c %Y "$d/last")))"
  if [ -e "$d/requested" ]; then echo requested; fi
  if [ "$op" = retire ] && [ "$held" -eq 0 ]; then touch "$d/retiring"; fi
  if [ -e "$d/retiring" ]; then echo retiring; fi
  ;;
*)
  echo "unknown slot operation $op" >&2
  exit 2
  ;;
esac
