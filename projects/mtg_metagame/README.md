# mtg_metagame

A Magic: The Gathering metagame analyzer, ported from the Python
`mtg_metagame_stats`: which archetypes make up a tournament field, and how each
one performs.

```sh
$ ./aipl run -- standard_26Q2_rc_japan
1441 Forest
1441 Island
916 Steam Vents
…
```

Lists every card played in a metagame, most copies first (main decks and
sideboards together; equal counts in name order). The argument is a file in
`data/` without its `.json.gz`, or one of the names in `data/index.json`. Run it
from this directory.

## Layout

| file | what it is |
|---|---|
| `main.aipl` | the command line: resolve the name, load, print |
| `metagame.aipl` | `Metagame`, `Deck`, `Card`, `Match`, `Record`; decoding them from JSON; `load_metagame` |
| `card_stats.aipl` | `card_counts`: copies played per card |
| `json.aipl` | a JSON parser and the accessors the decoder uses |

The files are unpacked by running `gzip -dc` (`execute_program`): nothing here
reads binary files or decompresses yet.

## Status

Loading and the card listing work. Win rates are whole permille until the
language has floats.

## Plan, following the Python original

- `data_structures.py` → decks, match history, metagame (loading data)
- `meta_share.py` → archetype share and win rate, Wilson confidence interval
- `matchup_effectiveness.py`, `metagame_matrix.py` → matchup tables
- scrapers (melee, spicerack, mtgo) → data import, replacing pickles with a text format

## Data

`data/` holds the Python tool's metagame pickles as gzipped JSON, one file per
tournament set (`metagame_<tournament ids>.json.gz`), converted by `tools/pkl_to_json.py`. Each
file has `tournament_ids`, `ranked_decklist_ids` and `decks`; a deck has its name,
pilot, `maindeck`/`sideboard` (cards with name, count, type) and `match_history`
(result, game record, round). `data/index.json` (plain, not gzipped) maps the descriptive names the
original used (`standard_26Q2_rc_japan`) to the file holding them. Three of those
names pointed at pickles that no longer exist and are left out.

Archetypes are not in the data: the original derives them from a canonicalization
config, which is still to be ported.
