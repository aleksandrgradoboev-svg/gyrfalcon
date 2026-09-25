"""Offline tests for delegated promotion into the single shared catalog."""

import json
import runpy
import sqlite3
import tempfile
import unittest
from pathlib import Path


PROMOTE = runpy.run_path(str(Path(__file__).with_name("v8std-promote.py")))


class PromoteTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.db, self.catalog, self.batch = (root / name for name in
                                             ("source.sqlite", "catalog.json", "batch.json"))
        conn = sqlite3.connect(self.db)
        conn.executescript("""
            CREATE TABLE articles(id TEXT,source_url TEXT,section TEXT);
            CREATE TABLE clauses(id TEXT,article_id TEXT,number TEXT,text TEXT);
            INSERT INTO articles VALUES('437','https://its.1c.ru/db/content/v8std/a','Код');
            INSERT INTO clauses VALUES('437:1','437','1.','1. Ключевые слова пишутся заглавными.');
        """)
        conn.close()
        self.catalog.write_text('{"version":1,"rules":[]}', encoding="utf-8")
        self.card = {"id": "v8std-keywords", "title": "Ключевые слова",
                     "guidance": "Писать ключевые слова запросов заглавными буквами.",
                     "scope": "Тексты запросов", "exception": "Нет в данном пункте",
                     "verification": "Проверить текст запроса", "source_anchor": "437:1",
                     "review_status": "pending"}
        self.write_batch()

    def write_batch(self) -> None:
        self.batch.write_text(json.dumps({
            "kind": "v8std-normalized-practice-candidates",
            "status": "pending-review", "cards": [self.card],
        }, ensure_ascii=False), encoding="utf-8")

    def test_apply_is_additive_and_idempotent(self) -> None:
        catalog, fresh = PROMOTE["prepare"](
            self.db, self.catalog, [self.batch], "2026-09-25")
        self.assertEqual(len(fresh), 1)
        self.assertEqual(fresh[0]["approval"]["kind"], "delegated")
        PROMOTE["apply"](self.catalog, catalog, fresh)
        catalog, fresh = PROMOTE["prepare"](
            self.db, self.catalog, [self.batch], "2026-09-25")
        self.assertEqual(fresh, [])
        self.assertEqual(len(catalog["rules"]), 1)

    def test_missing_source_is_rejected_without_catalog_change(self) -> None:
        before = self.catalog.read_text(encoding="utf-8")
        self.card["source_anchor"] = "437:missing"
        self.write_batch()
        with self.assertRaisesRegex(ValueError, "не найден исходный пункт"):
            PROMOTE["prepare"](self.db, self.catalog, [self.batch], "2026-09-25")
        self.assertEqual(self.catalog.read_text(encoding="utf-8"), before)

    def test_related_source_is_hashed_and_missing_one_rejected(self) -> None:
        self.card["related_source_anchors"] = ["437:1"]
        self.write_batch()
        _, fresh = PROMOTE["prepare"](self.db, self.catalog, [self.batch], "2026-09-25")
        self.assertEqual(fresh[0]["related_sources"][0]["anchor"], "437:1")
        self.card["related_source_anchors"] = ["437:missing"]
        self.write_batch()
        with self.assertRaisesRegex(ValueError, "не найден связанный пункт"):
            PROMOTE["prepare"](self.db, self.catalog, [self.batch], "2026-09-25")


if __name__ == "__main__":
    unittest.main()
