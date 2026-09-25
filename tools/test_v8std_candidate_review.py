"""Offline validation tests for human-reviewed standard candidate cards."""

import json
import runpy
import sqlite3
import tempfile
import unittest
from pathlib import Path


REVIEW = runpy.run_path(str(Path(__file__).with_name("v8std-candidate-review.py")))


class CandidateReviewTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.db = self.root / "source.sqlite"
        self.cards = self.root / "cards.json"
        conn = sqlite3.connect(self.db)
        conn.executescript("""
            CREATE TABLE articles(id TEXT,title TEXT,section TEXT,source_url TEXT);
            CREATE TABLE clauses(id TEXT,article_id TEXT,text TEXT);
            INSERT INTO articles VALUES('437','Запросы','Код / Запросы','https://its.1c.ru/437');
            INSERT INTO clauses VALUES('437:1','437','1. Ключевые слова заглавными.');
        """)
        conn.close()
        self.card = {
            "id": "v8std-query-keywords-uppercase",
            "title": "Ключевые слова",
            "guidance": "Писать заглавными.",
            "scope": "Запросы.",
            "exception": "Нет.",
            "verification": "Проверить запрос.",
            "source_anchor": "437:1",
            "review_status": "pending",
        }

    def write_cards(self, approved=False) -> None:
        data = {"kind": "v8std-normalized-practice-candidates",
                "cards": [self.card]}
        if approved:
            data["status"] = "accepted"
            data["approval"] = {"kind": "explicit", "reviewer": "user",
                                "date": "2026-09-25"}
        self.cards.write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")

    def test_validated_card_stays_pending(self) -> None:
        self.write_cards()
        cards = REVIEW["validate"](self.cards, self.db)
        self.assertEqual(len(cards), 1)
        self.assertEqual(cards[0]["source_anchor"], "437:1")
        report = REVIEW["render"](cards)
        self.assertIn("Принято из этого пакета: **0**", report)
        self.assertIn("https://its.1c.ru/437", report)

    def test_source_must_exist_and_card_cannot_be_approved(self) -> None:
        self.card["source_anchor"] = "437:missing"
        self.write_cards()
        with self.assertRaisesRegex(ValueError, "нет исходного блока"):
            REVIEW["validate"](self.cards, self.db)
        self.card["source_anchor"] = "437:1"
        self.card["review_status"] = "approved"
        self.write_cards()
        with self.assertRaisesRegex(ValueError, "не соответствует решению"):
            REVIEW["validate"](self.cards, self.db)
        self.card["review_status"] = "accepted"
        self.write_cards(approved=True)
        cards = REVIEW["validate"](self.cards, self.db)
        self.assertIn("Принято из этого пакета: **1**", REVIEW["render"](cards))


if __name__ == "__main__":
    unittest.main()
