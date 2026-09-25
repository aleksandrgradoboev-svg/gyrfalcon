"""Report shared official corpus coverage versus actually accepted practices."""

from __future__ import annotations

import argparse
import hashlib
import json
import sqlite3
from collections import Counter, defaultdict
from pathlib import Path


def build(db_path: Path, catalog_path: Path,
          review_batches: list[Path] | None = None) -> dict:
    catalog = json.loads(catalog_path.read_text(encoding="utf-8"))
    uri = f"file:{db_path.resolve().as_posix()}?mode=ro"
    conn = sqlite3.connect(uri, uri=True)
    conn.row_factory = sqlite3.Row
    try:
        articles = conn.execute("SELECT id,section FROM articles").fetchall()
        blocks = conn.execute("""SELECT c.id,c.article_id,c.text,a.section,a.source_url
            FROM clauses c JOIN articles a ON a.id=c.article_id""").fetchall()
    finally:
        conn.close()
    source = {row["id"]: row for row in blocks}
    accepted = catalog["rules"]
    anchored = [rule for rule in accepted if rule.get("source_anchor")]
    for rule in anchored:
        row = source.get(rule["source_anchor"])
        if row is None or row["source_url"] != rule["url"] or hashlib.sha256(
                row["text"].encode("utf-8")).hexdigest() != rule["source_sha256"]:
            raise ValueError(f"принятое правило без актуального источника: {rule['id']}")
        for related in rule.get("related_sources", []):
            other = source.get(related["anchor"])
            if other is None or other["source_url"] != related["url"] or hashlib.sha256(
                    other["text"].encode("utf-8")).hexdigest() != related["sha256"]:
                raise ValueError(f"связанное исключение разошлось с источником: {rule['id']}")
    by_section = defaultdict(lambda: Counter())
    for article in articles:
        by_section[article["section"].split(" / ")[0]]["articles"] += 1
    for block in blocks:
        by_section[block["section"].split(" / ")[0]]["blocks"] += 1
    for rule in anchored:
        section = source[rule["source_anchor"]]["section"].split(" / ")[0]
        by_section[section]["accepted_rules"] += 1
    unique_anchors = {rule["source_anchor"] for rule in anchored}
    reviewed: set[str] = set()
    excluded: set[str] = set()
    declared_articles: set[str] = set()
    for batch_path in review_batches or []:
        batch = json.loads(batch_path.read_text(encoding="utf-8"))
        anchors = batch.get("reviewed_source_anchors")
        if not isinstance(anchors, list) or len(anchors) != len(set(anchors)):
            raise ValueError(f"нет полного списка уникальных reviewed_source_anchors: {batch_path}")
        unknown = set(anchors) - source.keys()
        if unknown:
            raise ValueError(f"неизвестные проверенные пункты: {sorted(unknown)[:3]}")
        reviewed.update(anchors)
        article_ids = batch.get("reviewed_articles", [])
        if not isinstance(article_ids, list):
            raise ValueError(f"неверный список reviewed_articles: {batch_path}")
        declared_articles.update(article_ids)
        for item in batch.get("excluded_normative", []):
            anchor = item.get("source_anchor")
            if anchor not in anchors or len(item.get("reason", "")) < 10:
                raise ValueError(f"необоснованное исключение {anchor}: {batch_path}")
            excluded.add(anchor)
    if review_batches:
        all_articles = {row["id"] for row in articles}
        if declared_articles - all_articles:
            raise ValueError("reviewed_articles содержит неизвестную статью")
        for article_id in declared_articles:
            missing = {row["id"] for row in blocks if row["article_id"] == article_id} - reviewed
            if missing:
                raise ValueError(f"статья {article_id} заявлена просмотренной, но пункты пропущены")
        for block in blocks:
            if block["id"] in reviewed:
                by_section[block["section"].split(" / ")[0]]["reviewed_blocks"] += 1
    return {
        "articles": len(articles), "blocks": len(blocks),
        "accepted_rules": len(accepted), "anchored_rules": len(anchored),
        "legacy_rules_without_anchor": len(accepted) - len(anchored),
        "addressed_blocks": len(unique_anchors),
        "unaddressed_blocks": len(blocks) - len(unique_anchors),
        "reviewed_blocks": len(reviewed) if review_batches else None,
        "unreviewed_blocks": len(blocks) - len(reviewed) if review_batches else None,
        "excluded_normative": len(excluded) if review_batches else None,
        "sections": dict(sorted(by_section.items())),
    }


def render(result: dict) -> str:
    lines = [
        "# Общий базис стандартов 1С: фактическое покрытие", "",
        f"Источник: {result['articles']} статей, {result['blocks']} адресуемых блоков.",
        f"Принятых нормализованных правил: **{result['accepted_rules']}**; "
        f"{result['anchored_rules']} имеют адрес и хеш исходного блока.",
        f"Принятые правила опираются на {result['addressed_blocks']} уникальных блоков;",
        f"{result['unaddressed_blocks']} блоков не стали основным источником отдельного принятого правила.",
        "Это не означает, что каждый такой блок содержит самостоятельную практику.",
    ]
    if result["legacy_rules_without_anchor"]:
        lines.append(f"Без адреса осталось {result['legacy_rules_without_anchor']} ранних правил.")
    if result["reviewed_blocks"] is not None:
        lines += [
            f"Аудит чтения: {result['reviewed_blocks']} блоков просмотрено, "
            f"{result['unreviewed_blocks']} ещё не просмотрено; "
            f"{result['excluded_normative']} нормативных блоков получили обоснование без новой карточки.",
        ]
    lines += ["", "| Раздел | Статей | Блоков | Просмотрено | Принятых правил |",
              "|---|---:|---:|---:|---:|"]
    for section, counts in result["sections"].items():
        lines.append(f"| {section or 'Без раздела'} | {counts['articles']} | "
                     f"{counts['blocks']} | {counts['reviewed_blocks'] if result['reviewed_blocks'] is not None else '—'} | "
                     f"{counts['accepted_rules']} |")
    lines += [
        "", "Статья 788 — журнал изменений справочника; статья 802 — указатель на материалы",
        "по интерфейсу 8.5. Оба блока просмотрены, самостоятельного правила в них нет.",
        "", "Полный текст остаётся в одном локальном SQLite; проектных копий нет.",
        "При применении правила нужно открыть полный исходный пункт по его адресу,",
        "особенно для проверки условий и исключений.", "",
    ]
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", required=True, type=Path)
    parser.add_argument("--catalog", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--review-batch", type=Path, action="append")
    parser.add_argument("--refresh", action="store_true")
    args = parser.parse_args()
    if args.out.exists() and not args.refresh:
        parser.error("отчёт существует; обновление требует --refresh")
    result = build(args.db, args.catalog, args.review_batch)
    args.out.write_text(render(result), encoding="utf-8")
    print(f"Принято {result['accepted_rules']} правил; без отдельного правила {result['unaddressed_blocks']} блоков")


if __name__ == "__main__":
    main()
