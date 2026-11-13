#!/usr/bin/env python3
"""Write the SQL that seeds one app's rows of the mako table chat_aliases.

An alias row holds the instructions and the shape of the reply on the service, so the app sends only the facts and
a model call can do one job (roadmap decision D39). The rows are data, inserted by the operator and never by a
migration. Each app keeps a folder with aliases.json (the tenant's app_id, the model, and each alias with its limits
and reply schema) and prompts/<alias>.txt (its instructions). The SQL is idempotent: running it again updates the rows.

Usage, from the monorepo root:
    python3 -I services/intelligence/scripts/gen_alias_sql.py apps/pay-day/tools/ai > pay-day-aliases.sql
    python3 -I services/intelligence/scripts/gen_alias_sql.py apps/pay-day/tools/ai --check
Apply, on the machine that deploys mako:
    npx wrangler@4.149.0 d1 execute openai-image-proxy --remote --file pay-day-aliases.sql
"""

import argparse
import json
import sys
from pathlib import Path

EM_DASH = "—"
COLUMNS = [
    "app_id", "alias", "model", "system_prompt", "response_schema", "max_tokens", "temperature", "flat_credits",
    "weekly_free", "premium_bypass", "daily_limit", "reasoning", "version",
]


def sql_string(value):
    return "'" + value.replace("'", "''") + "'"


def problems(catalog, folder):
    found = []
    seen = set()
    for entry in catalog["aliases"]:
        name = entry["alias"]
        if name in seen:
            found.append(f"{name} is listed twice")
        seen.add(name)
        path = folder / "prompts" / f"{name}.txt"
        if not path.is_file():
            found.append(f"{path} is missing")
            continue
        text = path.read_text(encoding="utf-8")
        if EM_DASH in text:
            found.append(f"{path} contains an em dash")
        if not text.strip():
            found.append(f"{path} is empty")
        schema = entry["response_schema"]
        if schema.get("type") != "object" or schema.get("additionalProperties") is not False:
            found.append(f"{name}: the reply schema must be a closed object")
        if sorted(schema.get("required", [])) != sorted(schema.get("properties", {})):
            found.append(f"{name}: every property of the reply must be required")
        if entry.get("reasoning") not in (None, "low", "medium", "high"):
            found.append(f"{name}: reasoning must be low, medium or high when it is set")
        for limit in ("max_tokens", "weekly_free", "daily_limit", "flat_credits", "version"):
            if not isinstance(entry.get(limit), int) or entry[limit] < 0:
                found.append(f"{name}: {limit} must be a whole number")
    for path in sorted((folder / "prompts").glob("*.txt")):
        if path.stem not in seen:
            found.append(f"{path} belongs to no alias in aliases.json")
    return found


def row(catalog, entry, folder):
    prompt = (folder / "prompts" / f"{entry['alias']}.txt").read_text(encoding="utf-8").strip()
    schema = json.dumps(entry["response_schema"], separators=(",", ":"), ensure_ascii=False)
    values = [
        sql_string(catalog["app_id"]), sql_string(entry["alias"]), sql_string(catalog["model"]), sql_string(prompt),
        sql_string(schema), str(entry["max_tokens"]), "NULL", str(entry["flat_credits"]), str(entry["weekly_free"]),
        "1" if entry["premium_bypass"] else "0", str(entry["daily_limit"]),
        sql_string(entry["reasoning"]) if entry.get("reasoning") else "NULL", str(entry["version"]),
    ]
    updates = ", ".join(f"{column} = excluded.{column}" for column in COLUMNS[2:])
    return (
        f"INSERT INTO chat_aliases ({', '.join(COLUMNS)})\nVALUES ({', '.join(values)})\n"
        f"ON CONFLICT(app_id, alias) DO UPDATE SET {updates};\n"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("folder", type=Path, help="the app's folder holding aliases.json and prompts/")
    parser.add_argument("--check", action="store_true", help="validate the catalog and the prompts, write nothing")
    arguments = parser.parse_args()
    folder = arguments.folder.resolve()
    catalog = json.loads((folder / "aliases.json").read_text(encoding="utf-8"))
    found = problems(catalog, folder)
    if found:
        sys.exit("\n".join(found))
    if arguments.check:
        print(f"{len(catalog['aliases'])} aliases, no problems")
        return
    sys.stdout.write("".join(row(catalog, entry, folder) for entry in catalog["aliases"]))


if __name__ == "__main__":
    main()
