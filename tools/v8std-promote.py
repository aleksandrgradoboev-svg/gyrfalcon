"""Promote source-checked delegated v8std cards into the single shared catalog.

Dry-run by default. --apply is an explicit bulk mechanical rewrite. Existing
rules are never changed or removed; a repeated identical import is a no-op.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sqlite3
import tempfile
from pathlib import Path


FIELDS = ("id", "title", "guidance", "scope", "exception", "verification",
          "source_anchor", "review_status")


def prepare(db_path: Path, catalog_path: Path, batch_paths: list[Path],
            date: str) -> tuple[dict, list[dict]]:
    catalog = json.loads(catalog_path.read_text(encoding="utf-8"))
    if catalog.get("version") != 1 or not isinstance(catalog.get("rules"), list):
        raise ValueError("некорректный общий каталог")
    existing = {rule["id"]: rule for rule in catalog["rules"]}
    if len(existing) != len(catalog["rules"]):
        raise ValueError("повтор ID в общем каталоге")
    uri = f"file:{db_path.resolve().as_posix()}?mode=ro"
    conn = sqlite3.connect(uri, uri=True)
    conn.row_factory = sqlite3.Row
    fresh = []
    seen = set()
    try:
        for path in batch_paths:
            batch = json.loads(path.read_text(encoding="utf-8"))
            if (batch.get("kind") != "v8std-normalized-practice-candidates"
                    or batch.get("status") != "pending-review"
                    or not isinstance(batch.get("cards"), list)):
                raise ValueError(f"неподходящий пакет: {path}")
            for card in batch["cards"]:
                if any(not isinstance(card.get(field), str) or not card[field].strip()
                       for field in FIELDS):
                    raise ValueError(f"пустое обязательное поле: {path}")
                if card["review_status"] != "pending" or not card["id"].startswith("v8std-"):
                    raise ValueError(f"неверный статус/ID: {card['id']}")
                if card["id"] in seen:
                    raise ValueError(f"повтор ID между пакетами: {card['id']}")
                seen.add(card["id"])
                row = conn.execute("""SELECT c.text,c.number,a.source_url,a.section
                    FROM clauses c JOIN articles a ON a.id=c.article_id
                    WHERE c.id=?""", (card["source_anchor"],)).fetchone()
                if row is None or not row["text"]:
                    raise ValueError(f"не найден исходный пункт: {card['source_anchor']}")
                if len(card["guidance"]) < 25 or len(card["verification"]) < 15:
                    raise ValueError(f"слишком общая карточка: {card['id']}")
                related_sources = []
                for anchor in card.get("related_source_anchors", []):
                    related = conn.execute("""SELECT c.text,a.source_url
                        FROM clauses c JOIN articles a ON a.id=c.article_id
                        WHERE c.id=?""", (anchor,)).fetchone()
                    if related is None or not related["text"]:
                        raise ValueError(f"не найден связанный пункт: {anchor}")
                    related_sources.append({"anchor": anchor,
                        "sha256": hashlib.sha256(related["text"].encode("utf-8")).hexdigest(),
                        "url": related["source_url"]})
                source_hash = hashlib.sha256(row["text"].encode("utf-8")).hexdigest()
                entry = {
                    "id": card["id"], "title": card["title"],
                    "guidance": card["guidance"], "scope": card["scope"],
                    "exception": card["exception"],
                    "verification": card["verification"],
                    "section": (row["number"] or "").rstrip("."),
                    "url": row["source_url"],
                    "source_anchor": card["source_anchor"],
                    "source_sha256": source_hash,
                    "accepted_batch": path.stem,
                    "approval": {"kind": "delegated", "authority": "standard",
                                 "reviewer": "gyrfalcon", "date": date,
                                 "reason": "Пользователь делегировал приёмку общего базиса"},
                }
                if related_sources:
                    entry["related_sources"] = related_sources
                if card["id"] in existing:
                    if entry != existing[card["id"]]:
                        raise ValueError(f"существующее правило отличается: {card['id']}")
                    continue
                fresh.append(entry)
    finally:
        conn.close()
    return catalog, fresh


def apply(catalog_path: Path, catalog: dict, fresh: list[dict]) -> None:
    if not fresh:
        return
    catalog["rules"].extend(fresh)
    encoded = json.dumps(catalog, ensure_ascii=False, indent=2) + "\n"
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=catalog_path.parent,
                                     prefix=".v8std-promote-", suffix=".tmp",
                                     delete=False) as handle:
        temp_path = Path(handle.name)
        handle.write(encoded)
    try:
        os.replace(temp_path, catalog_path)
    finally:
        temp_path.unlink(missing_ok=True)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", required=True, type=Path)
    parser.add_argument("--catalog", required=True, type=Path)
    parser.add_argument("--batch", required=True, type=Path, action="append")
    parser.add_argument("--date", required=True)
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args()
    catalog, fresh = prepare(args.db, args.catalog, args.batch, args.date)
    print(f"Проверено пакетов: {len(args.batch)}; новых правил: {len(fresh)}; было: {len(catalog['rules'])}")
    if args.apply:
        apply(args.catalog, catalog, fresh)
        print(f"Общий каталог: {len(catalog['rules'])} правил")
    else:
        print("Пробный проход: файл не изменён. Для включения нужно --apply")


if __name__ == "__main__":
    main()
