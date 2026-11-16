# mtg_metagame

A Magic: The Gathering metagame analyzer, ported from the Python
`mtg_metagame_stats`: which archetypes make up a tournament field, and how each
one performs.

```sh
$ ./aipl run -- cards metagame_worlds_32
363 Forest
207 Starting Town
200 Plains
…

$ ./aipl run -- share metagame_worlds_32
Metagame Breakdown from metagame_worlds_32
Order by Metagame %
+-----------------------+------------+-------+
| Deck Name             | Metagame % | Count |
+=======================+============+=======+
| Landfall              |      21.1% |    27 |
| Boros Pixie           |      18.8% |    24 |
…
```

`winrate <metagame> [--archetypes <config>] [--output <file.svg>]` writes the winrate matrix as an SVG
page (default `winrate_<metagame>.svg`) and prints where it went: a heat map with a row per archetype
(most decks first, `Other` last) and a column for each, coloured by the share of the row's
matches it won against the column's, green for winning and red for losing. Open it in a browser:
hovering a cell lights up its row and column and fills a panel at the top right: the pairing's record, win rate and share of all matches, and a table of how often it met in each round (see below). Click a cell to lock the panel on it while the mouse moves elsewhere; the Unlock button, a second click on the cell, or Escape releases it. A match counts toward
the pairing of the two decks' archetypes, from both sides. The first column is each archetype's overall win rate against every archetype, mirror matches included (one win and one loss each, which is what they do to an expected win rate at an event). A draw counts as half a win, and `-`
is a pairing with no match or an archetype against itself.

The panel counts rounds back from the end of each tournament, so a nine-round event and a fifteen-round one line up at the top
of the bracket instead of at round 1: finals, semifinals and quarterfinals first, then the last round of the main event, the
2nd last, and so on. Each row says how many times the pairing met in that slot, what share of the metagame's matches in the
slot that is, and its record and win rate there. Metagames merging several tournaments are counted per tournament.

Hovering an archetype's name (or a sub-archetype's) shows the panel its Overall cell does, and clicking
the name locks it there. That panel ends with the archetype's decks, each pilot a link to the
decklist.

An archetype with sub-archetypes has a ▸ before its name. Click it to expand the archetype: a row
for each sub-archetype appears under it (`↳ Mono-G`), with its record against every archetype, and
its own overall and projected cells, so removing an archetype projects them too. Click the ▾ to fold
them away. The expanded archetypes are kept in the page's address with the removed ones. A
sub-archetype is a row only: the columns stay the archetypes.

Untick an archetype's checkbox, at the left of its row (or click its column heading), to remove it from the metagame, as a rotation would:
its row and column fade, and the Projected column beside Overall shows what every other archetype's win rate would be
without its matches against the removed ones, with the change in points underneath. That is the simple model: the
removed decks' matches are dropped, and nobody is assumed to switch to anything else. Hovering a Projected cell lists the
matches each removal took out of that row. Tick it again to bring it back, or use Restore all in the panel; the
removed archetypes are kept in the page's address (`#removed=1,4`), so a reload or a shared link keeps them.

The top-left links sort the rows and columns by meta share (the default), overall win rate, or projected win rate (the removed archetypes last); the choice is kept in the address too (`&sort=winrate`).

Under it the panel breaks the pairing down by the row archetype's record going into the match (`7-1`, `0-3`, ...), highest
first, with how often the pairing met at that record and how it went, so a matchup that swings between the top and the bottom
of the standings shows. The table lists every record any archetype had in the metagame, and one the pairing never met at is
greyed out, as the round table's empty rounds are, so its rows stay in the same place from pairing to pairing. The record is the player's own in the tournament so far, byes counted and draws left out, and only
numbered rounds of the main event have one; a match whose second deck has no history for the round is counted from the first
deck's side only.

`cards` lists every card played in a metagame, most copies first (main decks and
sideboards together; equal counts in name order). `share` classifies each deck into
archetypes and prints how much of the field each is. A deck counts in every archetype it
matches, so the percentages can add up to more than 100; the decks that match none are
one `Other` row, last whatever its size. The metagame is a file in
`data/` without its `.json.gz`, or one of the names in `data/index.json`; `--archetypes` names
a list in `data/archetypes.json` (default `Standard2026Q4Worlds`). Run it from this directory.

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
| `rounds.aipl` | `slot_of`: where a match's round falls counted back from the end of its tournament, `slot_label`, and the `Bracket` (record going in) of a match |
| `winrate_svg.aipl` | `winrate_svg`: the matrix as an SVG page |
| `json.aipl` | accessors over the tree `parse` from the `json` library builds: `field`, `text_field`, `integer_field`, `list_field` |

The files are unpacked by running `gzip -dc` (`execute_program`): nothing here
reads binary files or decompresses yet.

## Status

Loading, the card listing and the metagame share report work. A record's win rate is a float,
the fraction of its matches won, a draw counting as half a win. The winrate matrix works. Not yet: the win rate and confidence interval columns in `share`,
ordering by anything but share, and the plot.

## Plan, following the Python original

- `data_structures.py` → decks, match history, metagame (loading data)
- `meta_share.py` → archetype share (done), win rate and Wilson confidence interval
- `matchup_effectiveness.py`, `metagame_matrix.py` → matchup tables
- scrapers (melee, spicerack, mtgo) → data import, replacing pickles with a text format

## Data

`data/` holds the Worlds field, `metagame_worlds_32.json.gz`, the Python tool's metagame pickle
as gzipped JSON, converted by `tools/pkl_to_json.py`. Earlier metagames are not kept. Each
file has `tournament_ids`, `ranked_decklist_ids` and `decks`; a deck has its name,
pilot, `url` (where its decklist can be read), `maindeck`/`sideboard` (cards with name, count, type) and `match_history`
(result, game record, round). `data/index.json` (plain, not gzipped) maps descriptive names to the file holding
them, and is empty.

A deck is one archetype: a deck that matches identifiers of more than one name is an error naming
them. Archetypes are not in the data: the original derives them from a canonicalization config, one
per format and season, which `tools/archetypes_to_json.py` dumps into `data/archetypes.json`
(a list of identifiers per config: cards a deck must hold, must not hold, or must hold one of,
and cards it must hold in the main deck).

An identifier with a `parent` (an archetype's name) is a sub-archetype of it: it is asked only of the
decks that archetype claimed, and a deck matching more than one of an archetype's sub-archetypes
is an error, as for archetypes. A deck of the archetype that none claims is its `Other`. An
identifier may also list `not_maindeck_cards`, which it must not hold in the main deck (the
sideboard does not count). `Landfall` has two, `Selesnya` (Plains in the main deck) and `Mono-G`
(none there). Sub-archetypes show on the winrate page only, not in `share`.

`archetypes.json` holds one config, `Standard2026Q4Worlds`. It is not from the original: it is
`Standard2026Q4RC` with the archetypes of the Worlds field added, the two `Landfall` lists merged
into one, and every archetype of two decks or fewer left out (the merged `Tablet Control` among
them), so those decks are `Other`, except `Tablet Control`, which is any deck with Jeskai
Revelation and Tablet of Discovery. Rerunning `tools/archetypes_to_json.py` rewrites the file without it.
