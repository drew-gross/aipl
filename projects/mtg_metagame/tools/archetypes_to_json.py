"""Dump the original's canonicalization configs (which cards make a deck an archetype) to JSON.
Usage: archetypes_to_json.py SRC_DIR OUT_FILE

One entry per `*Config` class of `data_structures.py`, named without the suffix
(`Standard2026Q4RC`), holding its deck identifiers in the order the class lists them. The
card lists are sets in the original, so they are written sorted."""
import json, os, sys

src, out = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
sys.path.insert(0, src)
os.chdir(src)
import data_structures

configs = {}
for name in sorted(dir(data_structures)):
    if not name.endswith("Config") or name == "CanonicalizationConfig":
        continue
    config = getattr(data_structures, name)()
    configs[name[: -len("Config")]] = [
        {
            "name": i.name,
            "cards": sorted(i.cards),
            "not_cards": sorted(i.not_cards),
            "any_cards": sorted(i.any_cards),
            "maindeck_cards": sorted(i.maindeck_cards),
        }
        for i in config.deck_identifiers
    ]

with open(out, "w") as f:
    json.dump(configs, f, indent=1, ensure_ascii=False, sort_keys=True)
    f.write("\n")
print(f"{len(configs)} configs, {sum(len(v) for v in configs.values())} identifiers -> {out}")
