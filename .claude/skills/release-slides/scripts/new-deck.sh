#!/usr/bin/env bash
# Starts the deck of one release.
#
#   new-deck.sh OUT_DIR NAME
#
# Makes OUT_DIR/NAME with a slides.typ to fill in and the directories for
# the pictures (img/) and the scripts that capture them (scenes/). OUT_DIR
# is outside the repository. Refuses to touch a deck that exists: every
# release gets a deck of its own, and none is written over.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${1:?usage: new-deck.sh OUT_DIR NAME}"
name="${2:?usage: new-deck.sh OUT_DIR NAME}"
deck="$out/$name"

if [[ -e "$deck" ]]; then
    echo "new-deck.sh: $deck exists. A deck is not written over; choose another name." >&2
    exit 1
fi
root="$(git -C "$here" rev-parse --show-toplevel)"
case "$(realpath -m "$out")/" in
    "$root"/*) echo "new-deck.sh: $out is inside the repository. Decks are kept outside it." >&2; exit 1 ;;
esac
mkdir -p "$deck/img" "$deck/scenes" "$deck/lib"
cp "$here/../template/skeleton.typ" "$deck/slides.typ"
cp "$here/../template/hizukuri-slides.typ" "$deck/lib/hizukuri-slides.typ"
echo "made $deck"
