#!/usr/bin/env bash
# Typesets a deck.
#
#   build.sh DECK_DIR [--preview] [--timestamp UNIX_SECONDS]
#
# DECK_DIR holds slides.typ and the pictures it uses (made by new-deck.sh).
# Writes DECK_DIR/hizukuri-<name of DECK_DIR>.pdf. With --preview, also
# writes every page as DECK_DIR/preview/NNN.png, for looking at the result
# without a PDF viewer.
#
# Typst and the fonts come from the `slides` shell of flake.nix. Fonts
# installed on the machine are not used. The creation date written into the
# PDF is --timestamp (the time of the release's commit, by default the time
# of the last commit on HEAD), so the same sources give the same file.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"

deck=""
preview=0
timestamp=""
while (($#)); do
    case "$1" in
        --preview) preview=1 ;;
        --timestamp) timestamp="$2"; shift ;;
        -h | --help) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) [[ -z "$deck" ]] || { echo "build.sh: unexpected argument: $1" >&2; exit 2; }; deck="$1" ;;
    esac
    shift
done
[[ -n "$deck" && -f "$deck/slides.typ" ]] || { echo "usage: build.sh DECK_DIR [--preview] [--timestamp UNIX_SECONDS]" >&2; exit 2; }
deck="$(cd "$deck" && pwd)"
name="$(basename "$deck")"
[[ -n "$timestamp" ]] || timestamp="$(git -C "$root" log -1 --format=%ct)"

# The deck imports the template from its own directory, so that the
# directory holds everything the PDF was made from.
mkdir -p "$deck/lib"
cp "$here/../template/hizukuri-slides.typ" "$deck/lib/hizukuri-slides.typ"

typst=(typst compile --root "$deck" --ignore-system-fonts --creation-timestamp "$timestamp")
cd "$root"

# A slide whose content is taller than the page is an error, and so is a
# line of a code block that is wider than the block. List all of them
# before typesetting, so that they can be fixed in one pass.
overflow="$(nix develop .#slides --command typst eval --root "$deck" --ignore-system-fonts \
    --input check=true --in "$deck/slides.typ" 'query(<overflow>).map(it => it.value)')"
if [[ "$overflow" != "[]" ]]; then
    echo "build.sh: these do not fit: slides taller than the page (heights in pt)," >&2
    echo "          and lines of code blocks wider than their block:" >&2
    echo "$overflow" >&2
    exit 1
fi

nix develop .#slides --command "${typst[@]}" "$deck/slides.typ" "$deck/hizukuri-$name.pdf"
echo "wrote $deck/hizukuri-$name.pdf"
if ((preview)); then
    rm -rf "$deck/preview"
    mkdir -p "$deck/preview"
    nix develop .#slides --command "${typst[@]}" --ppi 96 "$deck/slides.typ" "$deck/preview/{0p}.png"
    echo "wrote $(find "$deck/preview" -name '*.png' | wc -l) pages to $deck/preview/"
fi
