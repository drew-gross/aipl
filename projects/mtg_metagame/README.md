# mtg_metagame

A Magic: The Gathering metagame analyzer, ported from the Python
`mtg_metagame_stats`: which archetypes make up a tournament field, and how each
one performs.

```sh
$ ./aipl run
mtg_metagame: nothing to analyze yet
```

## Status

Scaffolding. `main.aipl` holds the first data types (`Result`, `Record`) and win
rate arithmetic, with tests. Win rates are whole permille until the language has
floats.

## Plan, following the Python original

- `data_structures.py` → decks, match history, metagame (loading data)
- `meta_share.py` → archetype share and win rate, Wilson confidence interval
- `matchup_effectiveness.py`, `metagame_matrix.py` → matchup tables
- scrapers (melee, spicerack, mtgo) → data import, replacing pickles with a text format
