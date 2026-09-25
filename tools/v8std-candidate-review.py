"""Validate hand-normalized v8std cards and render their acceptance sheet.

An explicit top-level approval is required before cards may be marked accepted.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sqlite3
from collections import Counter
from pathlib import Path


FIELDS = ("id", "title", "guidance", "scope", "exception", "verification",
          "source_anchor", "review_status")


def validate(cards_path: Path, db_path: Path) -> list[dict]:
    data = json.loads(cards_path.read_text(encoding="utf-8"))
    if data.get("kind") != "v8std-normalized-practice-candidates":
        raise ValueError("неверный тип набора карточек")
    approval = data.get("approval")
    accepted = data.get("status") == "accepted"
    if accepted and (not isinstance(approval, dict)
                     or approval.get("kind") != "explicit"
                     or approval.get("reviewer") != "user"
                     or not approval.get("date")):
        raise ValueError("принятый пакет требует явного решения пользователя")
    cards = data.get("cards")
    if not isinstance(cards, list) or not cards:
        raise ValueError("нет карточек")
    uri = f"file:{db_path.resolve().as_posix()}?mode=ro"
    conn = sqlite3.connect(uri, uri=True)
    conn.row_factory = sqlite3.Row
    seen_ids, seen_anchors = set(), set()
    validated = []
    try:
        for card in cards:
            if not isinstance(card, dict) or any(
                not isinstance(card.get(field), str) or not card[field].strip()
                for field in FIELDS
            ):
                raise ValueError("пустое обязательное поле карточки")
            if card["review_status"] != ("accepted" if accepted else "pending"):
                raise ValueError(f"карточка {card['id']} не соответствует решению по пакету")
            if card["id"] in seen_ids or card["source_anchor"] in seen_anchors:
                raise ValueError(f"повтор ID или источника: {card['id']}")
            seen_ids.add(card["id"])
            seen_anchors.add(card["source_anchor"])
            row = conn.execute("""SELECT c.text,a.title AS article_title,
                                      a.section,a.source_url
                               FROM clauses c JOIN articles a ON a.id=c.article_id
                               WHERE c.id=?""", (card["source_anchor"],)).fetchone()
            if row is None or not row["text"]:
                raise ValueError(f"нет исходного блока: {card['source_anchor']}")
            validated.append({
                **card,
                "article_title": row["article_title"],
                "section": row["section"],
                "source_url": row["source_url"],
                "source_sha256": hashlib.sha256(
                    row["text"].encode("utf-8")).hexdigest(),
            })
    finally:
        conn.close()
    return validated


def render(cards: list[dict]) -> str:
    sections = Counter(card["section"].split(" / ")[0] for card in cards)
    accepted = sum(card["review_status"] == "accepted" for card in cards)
    lines = [
        "# Базовые практики 1С — пакет 001", "",
        f"Карточек: **{len(cards)}**. Принято из этого пакета: **{accepted}**. Это ручные",
        "переформулировки пунктов общего справочника ИТС, не проектные правила ЗУП.",
        "Пакет принят пользователем 25.09.2026; все карточки имеют проверяемые адреса источника.",
        "Ранее действовавшие 8 общих правил этим пакетом не заменены и не продублированы.",
        "", "## Покрытие пакета", "", "| Раздел | Карточек |", "|---|---:|",
    ]
    for section, count in sorted(sections.items()):
        lines.append(f"| {section} | {count} |")
    lines += ["", "## Карточки", ""]
    for number, card in enumerate(cards, 1):
        lines += [
            f"### {number}. {card['title']} (`{card['id']}`)", "",
            f"- Практика: {card['guidance']}",
            f"- Применять: {card['scope']}",
            f"- Исключение/граница: {card['exception']}",
            f"- Как проверить: {card['verification']}",
            f"- Источник: [{card['article_title']}]({card['source_url']}), "
            f"блок `{card['source_anchor']}`, SHA-256 `{card['source_sha256'][:12]}`.",
            ("- Решение: **принято**." if card["review_status"] == "accepted"
             else "- Решение: **ожидает приёмки**."), "",
        ]
    lines += [
        "## Граница результата", "",
        "Пакет — проверенная по адресам выборка, а не завершённая нормализация всех 1821",
        "блоков. Очередь полного корпуса остаётся в `v8std-practice-triage.json`.",
        "Остальные блоки не считаются автоматически принятыми практиками.", "",
    ]
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cards", type=Path, required=True)
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--refresh", action="store_true")
    args = parser.parse_args()
    if args.out.exists() and not args.refresh:
        parser.error("отчёт уже существует; обновление требует --refresh")
    cards = validate(args.cards, args.db)
    args.out.write_text(render(cards), encoding="utf-8")
    approved = sum(card["review_status"] == "accepted" for card in cards)
    print(f"Карточек с проверенными адресами: {len(cards)}; принятых: {approved}")


if __name__ == "__main__":
    main()
