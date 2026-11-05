"""Convert Metagame pickles to gzipped JSON. Usage: pkl_to_json.py SRC_DIR OUT_DIR"""
import gzip, json, pickle, sys, os, glob
from enum import Enum

src, out = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
sys.path.insert(0, src)
os.chdir(src)
import data_structures  # noqa: needed so pickle can find the classes
for _n in dir(data_structures):  # some pickles were written from a script run as __main__
    if not hasattr(sys.modules['__main__'], _n):
        setattr(sys.modules['__main__'], _n, getattr(data_structures, _n))

def card(c):
    return {"name": c.name, "count": c.count, "card_type": c.card_type}

def match(m):
    r = m.result
    return {
        "deck_id": m.deck_id,
        "opponent_deck_id": m.opponent_deck_id,
        "result": r.value if isinstance(r, Enum) else r,
        "record": {"win": m.record.win, "loss": m.record.loss, "draw": m.record.draw},
        "round": m.round,
    }

def deck(d):
    return {
        "deck_id": d.deck_id,
        "name": d.name,
        "pilot": d.pilot,
        "pilot_id": getattr(d, "pilot_id", None),
        "url": getattr(d, "_url", None),
        "tournament": d.tournament,
        "maindeck": [card(c) for c in d.maindeck.values()],
        "sideboard": [card(c) for c in d.sideboard.values()],
        "match_history": [match(m) for m in d.match_history],
    }

os.makedirs(out, exist_ok=True)
bad = []
aliases = {}
for path in sorted(glob.glob(os.path.join(src, "*.pkl"))):
    name = os.path.basename(path)[:-4]
    if os.path.islink(path):
        target = os.readlink(path)
        if os.path.exists(path):
            aliases[name] = target[:-4]
        else:
            bad.append((name, 'dangling symlink to ' + target))
        continue
    try:
        with open(path, "rb") as f:
            m = pickle.load(f)
        doc = {
            "name": name,
            "tournament_ids": list(getattr(m, "tournament_ids", [])),
            "ranked_decklist_ids": getattr(m, "ranked_decklist_ids", None),
            "decks": [deck(d) for d in getattr(m, "decks", {}).values()],
        }
        with gzip.open(os.path.join(out, name + ".json.gz"), "wt", encoding="utf-8", compresslevel=9) as f:
            json.dump(doc, f, separators=(",", ":"), ensure_ascii=False, default=str)
    except Exception as e:
        bad.append((name, repr(e)))
with open(os.path.join(out, "index.json"), "w") as f:
    json.dump(aliases, f, indent=1, sort_keys=True)
    f.write("\n")
print("skipped/failed:", bad)
