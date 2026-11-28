"""Merge gzipped JSON metagames into one. Usage: merge_metagames.py OUT NAME IN...

OUT is the file written and NAME the merged metagame's name. The decks of every IN are kept in
order, each with its own `tournament`, so the merged metagame is counted per tournament as the
original's merged pickles are; `tournament_ids` and `ranked_decklist_ids` are joined. A deck id two
inputs share is an error: a match names its opponent by deck id."""
import gzip, json, sys

out, name, *sources = sys.argv[1:]
merged = {"name": name, "tournament_ids": [], "ranked_decklist_ids": [], "decks": []}
seen = set()
for path in sources:
    with gzip.open(path, "rt", encoding="utf-8") as f:
        doc = json.load(f)
    merged["tournament_ids"] += doc["tournament_ids"]
    merged["ranked_decklist_ids"] += doc.get("ranked_decklist_ids") or []
    for deck in doc["decks"]:
        if deck["deck_id"] in seen:
            sys.exit(f"{path}: deck {deck['deck_id']} is in an earlier input too")
        seen.add(deck["deck_id"])
        merged["decks"].append(deck)
with gzip.open(out, "wt", encoding="utf-8", compresslevel=9) as f:
    json.dump(merged, f, separators=(",", ":"), ensure_ascii=False)
print(f"{len(sources)} metagames, {len(merged['decks'])} decks -> {out}")
