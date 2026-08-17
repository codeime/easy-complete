#!/usr/bin/env bash
#
# Report memory used by every Easy Complete process.
#
# Numbers come from `footprint`, i.e. phys_footprint — the same figure Activity
# Monitor shows under "Memory" and the one that counts against system memory
# pressure. Plain RSS is not comparable: on macOS it double-counts shared pages
# and ignores compressed ones.
#
#   scripts/memory-usage.sh                    one-off table
#   scripts/memory-usage.sh --watch 5          resample every 5s
#   scripts/memory-usage.sh --watch 5 --csv m.csv   append samples to a CSV
#   scripts/memory-usage.sh --peak             add each process's peak footprint
#
set -euo pipefail

INTERVAL=""
CSV=""
SHOW_PEAK=0

usage() {
    sed -n '3,14p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --watch)
            INTERVAL="${2:-}"
            [[ -n $INTERVAL ]] || { echo "--watch needs an interval in seconds" >&2; exit 2; }
            shift 2
            ;;
        --csv)
            CSV="${2:-}"
            [[ -n $CSV ]] || { echo "--csv needs a file path" >&2; exit 2; }
            shift 2
            ;;
        --peak) SHOW_PEAK=1; shift ;;
        -h|--help) usage 0 ;;
        *) echo "unknown argument: $1" >&2; usage 2 ;;
    esac
done

# Anchored on the installed bundle layout and on cargo's output directories so a
# `cargo run` build is picked up too. `figterm` rewrites its process title to
# "<shell> (ecterm)", which is why it is matched on the title rather than a path.
#
# The negative match matters: an editor or terminal whose window happens to
# mention this project shows up in `ps` with "easy-complete" in its title.
readonly MATCH='Easy Complete\.app/Contents/(MacOS/easy-complete|Helpers/.*fig_input_method)|target/(dist|release|debug)/(easy-complete|figterm|ec_cli)|\(ecterm\)'
# An editor whose window title mentions this project shows up in `ps` with
# "easy-complete" in its command line. The paths above are specific enough on
# their own, but these are the ones actually seen in the wild.
readonly EXCLUDE='Cursor Helper|Code Helper|extension-host'

collect_pids() {
    # Snapshot first: taken inside a substitution, the greps below do not exist
    # yet and so cannot match their own pattern text.
    local snapshot
    snapshot=$(ps -Ao pid=,command=)
    printf '%s\n' "$snapshot" \
        | grep -Ev "$EXCLUDE" \
        | grep -E "$MATCH" \
        | awk '{print $1}'
}

# Turn one `footprint` run into "<pid>\t<name>\t<bytes>\t<peak>" lines.
#
# Kept to substr/index rather than a regex with capture groups: macOS ships BWK
# awk, which has no three-argument match(). Process names can contain spaces
# ("zsh (ecterm)"), so field splitting is out too.
sample() {
    footprint --noCategories -f bytes "$@" 2>/dev/null | awk '
        # "easy-complete [9070]: 64-bit    Footprint: 85281280 B (16384 ...)"
        /Footprint:/ && /\[/ {
            lb = index($0, " [")
            rb = index($0, "]:")
            if (lb > 0 && rb > lb) {
                name = substr($0, 1, lb - 1)
                pid = substr($0, lb + 2, rb - lb - 2)
                split(substr($0, index($0, "Footprint:") + 10), parts, " ")
                bytes = parts[1]
            }
            next
        }
        /phys_footprint_peak:/ {
            split($0, p, ":")
            gsub(/[^0-9]/, "", p[2])
            if (pid != "") { print pid "\t" name "\t" bytes "\t" p[2]; pid = "" }
        }
    '
}

human() {
    awk -v b="$1" 'BEGIN {
        if (b >= 1073741824) { printf "%.2f GB", b / 1073741824 }
        else if (b >= 1048576) { printf "%.1f MB", b / 1048576 }
        else { printf "%.0f KB", b / 1024 }
    }'
}

report() {
    # `mapfile` would be tidier but macOS still ships bash 3.2.
    local args=() pid
    while read -r pid; do
        [[ -n $pid ]] && args+=(-p "$pid")
    done < <(collect_pids)

    if [[ ${#args[@]} -eq 0 ]]; then
        echo "No Easy Complete processes running."
        return
    fi

    local rows total=0 count=0
    rows=$(sample "${args[@]}")
    [[ -n $rows ]] || { echo "footprint returned nothing for pids: ${args[*]}" >&2; return; }

    if [[ $SHOW_PEAK -eq 1 ]]; then
        printf '%-8s %-34s %12s %12s\n' PID PROCESS MEMORY PEAK
    else
        printf '%-8s %-34s %12s\n' PID PROCESS MEMORY
    fi

    # Desktop first, then the IME, then one line per shell session, so the row
    # order does not jump around between samples.
    while IFS=$'\t' read -r pid name bytes peak; do
        [[ -n $pid ]] || continue
        if [[ $SHOW_PEAK -eq 1 ]]; then
            printf '%-8s %-34s %12s %12s\n' "$pid" "$name" "$(human "$bytes")" "$(human "$peak")"
        else
            printf '%-8s %-34s %12s\n' "$pid" "$name" "$(human "$bytes")"
        fi
        total=$((total + bytes))
        count=$((count + 1))
        if [[ -n $CSV ]]; then
            printf '%s,%s,%s,%s,%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$pid" "$name" "$bytes" "$peak" >>"$CSV"
        fi
    done < <(sort -t $'\t' -k2,2 <<<"$rows")

    printf '%-8s %-34s %12s\n' '' "TOTAL ($count processes)" "$(human "$total")"
}

if [[ -n $CSV && ! -s $CSV ]]; then
    echo 'timestamp,pid,process,footprint_bytes,peak_bytes' >"$CSV"
fi

if [[ -z $INTERVAL ]]; then
    report
    exit 0
fi

while true; do
    printf '\n== %s ==\n' "$(date '+%H:%M:%S')"
    report
    sleep "$INTERVAL"
done
