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

`cards` lists every card played in a metagame, most copies first (main decks and
sideboards together; equal counts in name order). `share` classifies each deck into
archetypes and prints how much of the field each is. A deck counts in every archetype it
matches, so the percentages can add up to more than 100; a deck that matches none is an
`Other` archetype of its own, listed as `<name> (self-titled)` with the rest. The metagame is a file in
`data/` without its `.json.gz`, or one of the names in `data/index.json`; `--archetypes` names
a list in `data/archetypes.json` (default `Standard2026Q4RC`). Run it from this directory.

## Layout

| file | what it is |
|---|---|
| `main.aipl` | the command line: `cards` and `share` |
| `metagame.aipl` | `Metagame`, `Deck`, `Card`, `Match`, `Record`; decoding them from JSON; `load_metagame` |
| `counted.aipl` | `Counted` (a name and a count) and `ranked`, the order the listings share |
| `card_stats.aipl` | `card_counts`: copies played per card |
| `archetypes.aipl` | the classifier: `Identifier`, `archetypes_of`, `load_identifiers` |
| `meta_share.aipl` | `shares`, the percentage and the grid, `share_report` |
| `json.aipl` | accessors over the `parse_json` builtin's tree: `field`, `text_field`, `integer_field`, `list_field` |

The files are unpacked by running `gzip -dc` (`execute_program`): nothing here
reads binary files or decompresses yet.

## Status

Loading, the card listing and the metagame share report work. A record's win rate is a float,
the fraction of its decided matches won. Not yet: the win rate and confidence interval columns,
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
