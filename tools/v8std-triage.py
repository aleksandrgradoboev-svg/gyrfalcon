"""Make a deterministic, metadata-only review queue from the shared v8std DB.

This does not approve practices. Full article text remains in the ignored DB.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sqlite3
from collections import Counter
from pathlib import Path


SIGNALS = {
    "prohibition": re.compile(
        r"\b(?:не\s+допускается|не\s+следует|не\s+рекомендуется|"
        r"не\s+долж\w*|недопустим\w*|запрещ\w*|нельзя)\b", re.I),
    "mandatory": re.compile(
        r"\b(?:долж\w*|необходим\w*|обязательн\w*|требуется|нужно)\b", re.I),
    "recommended": re.compile(
        r"\b(?:следует|рекомендуется|целесообразно|желательно)\b", re.I),
    "permitted": re.compile(r"\b(?:допускается|можно|возможно)\b", re.I),
}
EXAMPLE_START = re.compile(
    r"(?:\n|\s{2,})(?:например|пример|правильно|неправильно)\s*[:.]?", re.I)
NUMBER = re.compile(r"^\s*\d+(?:\.\d+)*\.\s*")


def signal_groups(text: str) -> tuple[list[str], list[str]]:
    lead = NUMBER.sub("", text, count=1)
    example = EXAMPLE_START.search(lead)
    if example:
        lead = lead[:example.start()]
    lead = lead[:600]
    early = [name for name, pattern in SIGNALS.items() if pattern.search(lead)]
    late = [name for name, pattern in SIGNALS.items()
            if name not in early and pattern.search(text)]
    return early, late


def classify(number: str | None, article_blocks: int,
             early: list[str], late: list[str]) -> str:
    if early:
        return "explicit_norm_signal"
    if late:
        return "later_norm_signal"
    if number is not None:
        return "numbered_no_modal"
    if article_blocks == 1:
        return "unnumbered_article"
    return "intro_context"


def accepted_rules(path: Path) -> dict[str, list[dict]]:
    data = json.loads(path.read_text(encoding="utf-8"))
    accepted = {}
    for rule in data["rules"]:
        anchor = rule.get("source_anchor")
        if anchor:
            approval = rule.get("approval", {})
            approved = approval.get("kind") in {"explicit", "delegated"}
            legacy = approval.get("kind") == "legacy" and not rule.get("accepted_batch")
            if not (approved or legacy):
                raise RuntimeError(f"некорректная приёмка {anchor}")
            accepted.setdefault(anchor, []).append(rule)
    return accepted


def review_decisions(paths: list[Path]) -> tuple[set[str], dict[str, str]]:
    reviewed: set[str] = set()
    reasons: dict[str, str] = {}
    for path in paths:
        data = json.loads(path.read_text(encoding="utf-8"))
        anchors = data.get("reviewed_source_anchors")
        if not isinstance(anchors, list) or len(anchors) != len(set(anchors)):
            raise RuntimeError(f"нет уникального списка просмотренных пунктов: {path}")
        reviewed.update(anchors)
        for item in data.get("excluded_normative", []):
            anchor = item.get("source_anchor")
            reason = item.get("reason", "")
            if anchor not in anchors or not isinstance(reason, str) or len(reason) < 10:
                raise RuntimeError(f"необоснованное исключение {anchor}: {path}")
            reasons[anchor] = reason
    return reviewed, reasons


def build_registry(db: Path, accepted: dict[str, list[dict] | dict] | None = None,
                   reviewed: set[str] | None = None,
                   review_reasons: dict[str, str] | None = None) -> dict:
    accepted = accepted or {}
    reviewed = reviewed or set()
    review_reasons = review_reasons or {}
    uri = f"file:{db.resolve().as_posix()}?mode=ro"
    conn = sqlite3.connect(uri, uri=True)
    conn.row_factory = sqlite3.Row
    meta = dict(conn.execute("SELECT key,value FROM meta"))
    if meta.get("coverage_kind") != "full-body-text-verified":
        conn.close()
        raise RuntimeError("нужен корпус с проверкой полного текста")
    query = """SELECT c.id,c.article_id,c.ordinal,c.number,c.text,
                      a.title,a.section,a.source_url,a.clause_count,a.body_sha256
               FROM clauses c JOIN articles a ON a.id=c.article_id
               ORDER BY CAST(a.id AS INTEGER),c.ordinal"""
    rows = conn.execute(query).fetchall()
    article_count = conn.execute("SELECT count(*) FROM articles").fetchone()[0]
    if len(rows) != int(meta["clause_count"]) or article_count != int(meta["article_count"]):
        conn.close()
        raise RuntimeError("каталог и адресуемые блоки корпуса не совпали")
    units = []
    matched_accepted = set()
    for row in rows:
        if not row["text"].strip():
            conn.close()
            raise RuntimeError(f"пустой исходный блок {row['id']}")
        early, late = signal_groups(row["text"])
        source_hash = hashlib.sha256(row["text"].encode("utf-8")).hexdigest()
        approved = accepted.get(row["id"], [])
        approved = approved if isinstance(approved, list) else [approved]
        if approved:
            if any(rule["source_sha256"] != source_hash
                   or rule["url"] != row["source_url"] for rule in approved):
                conn.close()
                raise RuntimeError(f"принятая карточка разошлась с источником: {row['id']}")
            matched_accepted.add(row["id"])
        units.append({
            "id": f"v8std:{row['id']}",
            "source_anchor": row["id"],
            "article_id": row["article_id"],
            "article_title": row["title"],
            "section": row["section"],
            "number": row["number"],
            "source_url": row["source_url"],
            "text_sha256": source_hash,
            "article_body_sha256": row["body_sha256"],
            "text_length": len(row["text"]),
            "classification": classify(row["number"], row["clause_count"], early, late),
            "lead_signals": early,
            "later_signals": late,
            "review_status": "accepted" if approved else (
                "reviewed_no_standalone" if row["id"] in reviewed else "pending"),
            "review_reason": review_reasons.get(row["id"]),
            "approved_rule_id": approved[0]["id"] if approved else None,
            "approved_rule_ids": [rule["id"] for rule in approved],
        })
    conn.close()
    if matched_accepted != set(accepted):
        raise RuntimeError(f"нет исходных блоков для принятых правил: {set(accepted) - matched_accepted}")
    if reviewed - {unit["source_anchor"] for unit in units}:
        raise RuntimeError("в журнале просмотра есть неизвестные пункты")
    if len({unit["id"] for unit in units}) != len(units):
        raise RuntimeError("повторяются адреса блоков")
    counts = Counter(unit["classification"] for unit in units)
    status_counts = Counter(unit["review_status"] for unit in units)
    by_section = Counter(unit["section"].split(" / ")[0] for unit in units)
    return {
        "version": 1,
        "kind": "v8std-practice-triage",
        "source_audit_sha256": meta["audit_sha256"],
        "article_count": article_count,
        "review_units": len(units),
        "accepted_units": len(matched_accepted),
        "accepted_rules": sum(len(value) if isinstance(value, list) else 1
                              for value in accepted.values()),
        "reviewed_units": len(reviewed),
        "review_status_counts": dict(sorted(status_counts.items())),
        "classification_counts": dict(sorted(counts.items())),
        "top_section_counts": dict(sorted(by_section.items())),
        "note": "Сигналы модальности — только очередь для проверки, не принятые практики.",
        "units": units,
    }


def report(registry: dict) -> str:
    status = registry["review_status_counts"]
    lines = [
        "# Реестр разбора стандартов 1С",
        "",
        f"Статей: {registry['article_count']}; адресуемых единиц: {registry['review_units']}.",
        f"Принято правил: {registry['accepted_rules']} из {registry['accepted_units']} блоков; "
        f"просмотрено {registry['reviewed_units']} блоков; без отдельной карточки "
        f"{status.get('reviewed_no_standalone', 0)}, ожидают просмотра {status.get('pending', 0)}.",
        "Модальные слова не являются приёмкой.",
        "Полный текст остаётся в общем локальном SQLite, в Git не переносится.",
        "",
        "## Сигналы исходной классификации",
        "",
        "| Класс | Единиц | Значение |",
        "|---|---:|---|",
    ]
    descriptions = {
        "explicit_norm_signal": "Нормативный маркер в начале; сигнал, а не вердикт.",
        "later_norm_signal": "Маркер позднее; начало может быть определением или примером.",
        "numbered_no_modal": "Нумерованный пункт без явного маркера.",
        "unnumbered_article": "Статья без нумерации пунктов.",
        "intro_context": "Вступление статьи с другими пунктами; часто содержит область применения.",
    }
    for category, count in registry["classification_counts"].items():
        lines.append(f"| `{category}` | {count} | {descriptions[category]} |")
    lines += ["", "## Разделы", "", "| Верхний раздел | Единиц |", "|---|---:|"]
    for section, count in registry["top_section_counts"].items():
        lines.append(f"| {section} | {count} |")
    lines += [
        "", "## Правило приёмки", "",
        "1. Открыть исходный пункт по `source_anchor` в `standards_1c` и страницу ИТС.",
        "2. Сформулировать применимость, действие, исключения и проверяемый критерий.",
        "3. Отметить явное решение проверяющего. Только тогда короткая карточка попадает в общий нормализованный слой.",
        "4. Не переносить общий стандарт в проектный профиль. Проект хранит лишь собственное принятое исключение.",
        "", "ЗУП-правило о сохранении старого кода остаётся проектным исключением; пункт 456:5 не меняет общий справочник.",
        "",
    ]
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--accepted-rules", type=Path,
                        help="Единый действующий каталог; принятые карточки сверяются с источником")
    parser.add_argument("--review-batch", type=Path, action="append",
                        help="Журнал реально прочитанных пунктов и причин без отдельной карточки")
    parser.add_argument("--refresh", action="store_true",
                        help="Явно заменить ранее сгенерированную очередь")
    args = parser.parse_args()
    if not args.db.is_file():
        parser.error(f"нет корпуса {args.db}")
    if not args.refresh and (args.out.exists() or args.report.exists()):
        parser.error("очередь уже существует; обновление требует --refresh")
    reviewed, reasons = review_decisions(args.review_batch or [])
    registry = build_registry(args.db, accepted_rules(args.accepted_rules)
                              if args.accepted_rules else None, reviewed, reasons)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(registry, ensure_ascii=False, indent=2) + "\n",
                        encoding="utf-8")
    args.report.write_text(report(registry), encoding="utf-8")
    print(f"Реестр: {registry['review_units']} блоков / {registry['article_count']} статей")
    print(f"  принято: {registry['accepted_units']}")
    for category, count in registry["classification_counts"].items():
        print(f"  {category}: {count}")


if __name__ == "__main__":
    main()
