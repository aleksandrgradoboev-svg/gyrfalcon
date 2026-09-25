"""Offline coverage accounting for the shared v8std baseline."""

import hashlib
import json
import runpy
import sqlite3
import tempfile
import unittest
from pathlib import Path


REPORT = runpy.run_path(str(Path(__file__).with_name("v8std-baseline-report.py")))


class BaselineReportTests(unittest.TestCase):
    def test_counts_rules_and_distinct_source_blocks_separately(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            db, catalog = root / "source.sqlite", root / "catalog.json"
            conn = sqlite3.connect(db)
            conn.executescript("""
                CREATE TABLE articles(id TEXT,section TEXT);
                CREATE TABLE clauses(id TEXT,article_id TEXT,text TEXT);
                INSERT INTO articles VALUES('456','Код / Модули');
                INSERT INTO clauses VALUES('456:1','456','1. Первый пункт');
                INSERT INTO clauses VALUES('456:2','456','2. Второй пункт');
            """)
            conn.close()
            rule = {"id": "r1", "source_anchor": "456:1",
                    "source_sha256": hashlib.sha256("1. Первый пункт".encode()).hexdigest(),
                    "url": "https://its.1c.ru/456"}
            # The fixture schema above omits the URL; add it explicitly.
            conn = sqlite3.connect(db)
            conn.execute("ALTER TABLE articles ADD COLUMN source_url TEXT")
            conn.execute("UPDATE articles SET source_url=?", (rule["url"],))
            conn.commit()
            conn.close()
            catalog.write_text(json.dumps({"rules": [rule, {"id": "legacy"}]}),
                               encoding="utf-8")
            result = REPORT["build"](db, catalog)
            self.assertEqual(result["accepted_rules"], 2)
            self.assertEqual(result["addressed_blocks"], 1)
            self.assertEqual(result["unaddressed_blocks"], 1)
            self.assertIn("Принятых нормализованных правил: **2**",
                          REPORT["render"](result))
            reviewed = root / "review.json"
            reviewed.write_text(json.dumps({
                "reviewed_articles": ["456"],
                "reviewed_source_anchors": ["456:1", "456:2"],
                "excluded_normative": [{"source_anchor": "456:2",
                                        "reason": "Повторяет уже принятое правило"}],
            }), encoding="utf-8")
            audited = REPORT["build"](db, catalog, [reviewed])
            self.assertEqual(audited["reviewed_blocks"], 2)
            self.assertEqual(audited["unreviewed_blocks"], 0)
            self.assertEqual(audited["excluded_normative"], 1)
            reviewed.write_text(json.dumps({
                "reviewed_articles": ["456"],
                "reviewed_source_anchors": ["456:1"]}), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "пункты пропущены"):
                REPORT["build"](db, catalog, [reviewed])


if __name__ == "__main__":
    unittest.main()
