# mtg_metagame

A Magic: The Gathering metagame analyzer, ported from the Python
`mtg_metagame_stats`: which archetypes make up a tournament field, and how each
one performs.

```sh
$ ./aipl run -- cards standard_26Q2_rc_japan
1441 Forest
1441 Island
916 Steam Vents
…

$ ./aipl run -- share standard_26Q2_rc_japan --archetypes Standard2026Q2RC
Metagame Breakdown from standard_26Q2_rc_japan
Order by Metagame %
+-------------------------+------------+-------+
| Deck Name               | Metagame % | Count |
+=========================+============+=======+
| Prowess                 |      26.1% |    92 |
| Landfall (Selesnya)     |      13.9% |    49 |
…
```

`winrate <metagame> [--archetypes <config>] [--output <file.svg>]` writes the winrate matrix as an SVG
page (default `winrate_<metagame>.svg`) and prints where it went: a heat map with a row per archetype
(most decks first, `Other` last) and a column for each, coloured by the share of the row's decided
matches it won against the column's, green for winning and red for losing. Open it in a browser:
hovering a cell lights up its row and column and says the record underneath. A match counts toward
every pairing of one deck's archetypes and the other's, from both sides. Draws are left out, and `-`
is a pairing with no decided match or an archetype against itself.

`cards` lists every card played in a metagame, most copies first (main decks and
sideboards together; equal counts in name order). `share` classifies each deck into
archetypes and prints how much of the field each is. A deck counts in every archetype it
matches, so the percentages can add up to more than 100; the decks that match none are
one `Other` row, last whatever its size. The metagame is a file in
`data/` without its `.json.gz`, or one of the names in `data/index.json`; `--archetypes` names
a list in `data/archetypes.json` (default `Standard2026Q4RC`). Run it from this directory.

## Layout

| file | what it is |
|---|---|
| `main.aipl` | the command line: `cards`, `share` and `winrate` |
| `metagame.aipl` | `Metagame` (its decks and the matches they played, each once), `Deck`, `Card`, `Match`, `Record`; decoding them from JSON; `load_metagame` |
| `counted.aipl` | `Counted` (a name and a count) and `ranked`, the order the listings share |
| `card_stats.aipl` | `card_counts`: copies played per card |
| `archetypes.aipl` | the classifier: `Identifier`, `archetypes_of`, `load_identifiers` |
| `meta_share.aipl` | `shares`, the percentage, `share_report` (the table itself is the `tabulate` library) |
| `winrate_matrix.aipl` | `observations` (each match per archetype pairing), `winrate_matrix` |
| `winrate_svg.aipl` | `winrate_svg`: the matrix as an SVG page |
| `json.aipl` | accessors over the tree `parse` from the `json` library builds: `field`, `text_field`, `integer_field`, `list_field` |

The files are unpacked by running `gzip -dc` (`execute_program`): nothing here
reads binary files or decompresses yet.

## Status

Loading, the card listing and the metagame share report work. A record's win rate is a float,
the fraction of its decided matches won. The winrate matrix works. Not yet: the win rate and confidence interval columns in `share`,
ordering by anything but share, and the plot.

## Plan, following the Python original

- `data_structures.py` → decks, match history, metagame (loading data)
- `meta_share.py` → archetype share (done), win rate and Wilson confidence interval
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

Archetypes are not in the data: the original derives them from a canonicalization config, one
per format and season, which `tools/archetypes_to_json.py` dumps into `data/archetypes.json`
(a list of identifiers per config: cards a deck must hold, must not hold, or must hold one of,
and cards it must hold in the main deck).
