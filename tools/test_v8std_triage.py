"""Network-free tests for the shared standards review queue."""

import runpy
import hashlib
import json
import sqlite3
import tempfile
import unittest
from pathlib import Path


TRIAGE = runpy.run_path(str(Path(__file__).with_name("v8std-triage.py")))


class TriageTests(unittest.TestCase):
    def test_legacy_approval_is_only_for_pre_batch_rules(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory, "catalog.json")
            legacy = {"id": "legacy", "source_anchor": "455:1",
                      "approval": {"kind": "legacy"}}
            path.write_text(json.dumps({"rules": [legacy]}), encoding="utf-8")
            self.assertEqual(TRIAGE["accepted_rules"](path)["455:1"][0]["id"], "legacy")
            legacy["accepted_batch"] = "002"
            path.write_text(json.dumps({"rules": [legacy]}), encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "некорректная приёмка"):
                TRIAGE["accepted_rules"](path)

    def test_every_block_is_pending_and_deterministic(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory, "shared.sqlite")
            conn = sqlite3.connect(path)
            conn.executescript("""
                CREATE TABLE meta(key TEXT,value TEXT);
                INSERT INTO meta VALUES('coverage_kind','full-body-text-verified');
                INSERT INTO meta VALUES('article_count','2');
                INSERT INTO meta VALUES('clause_count','3');
                INSERT INTO meta VALUES('audit_sha256','audit');
                CREATE TABLE articles(id TEXT,title TEXT,section TEXT,source_url TEXT,
                    clause_count INTEGER,body_sha256 TEXT);
                CREATE TABLE clauses(id TEXT,article_id TEXT,ordinal INTEGER,number TEXT,text TEXT);
                INSERT INTO articles VALUES('456','Тексты модулей','Код / Модули','https://its.1c.ru/456',2,'body1');
                INSERT INTO articles VALUES('802','Интерфейс','Формы','https://its.1c.ru/802',1,'body2');
                INSERT INTO clauses VALUES('456:intro','456',0,NULL,'Область применения: модули');
                INSERT INTO clauses VALUES('456:1','456',1,'3.','3. Программный модуль не должен содержать старый код.');
                INSERT INTO clauses VALUES('802:body','802',0,NULL,'Рекомендации по интерфейсу.');
            """)
            conn.close()
            first = TRIAGE["build_registry"](path)
            self.assertEqual(first, TRIAGE["build_registry"](path))
            self.assertEqual(first["review_units"], 3)
            self.assertEqual(first["classification_counts"]["explicit_norm_signal"], 1)
            self.assertEqual(first["classification_counts"]["unnumbered_article"], 1)
            self.assertTrue(all(item["review_status"] == "pending" for item in first["units"]))
            self.assertNotIn("text", first["units"][1])
            self.assertEqual(first["units"][1]["source_anchor"], "456:1")

    def test_modality_is_a_signal_not_an_approval(self) -> None:
        early, late = TRIAGE["signal_groups"](
            "1. Заполнение формы. Позднее рекомендуется проверить права.")
        self.assertIn("recommended", early)
        self.assertEqual(late, [])
        self.assertEqual(TRIAGE["classify"]("1.", 2, early, late),
                         "explicit_norm_signal")

    def test_nuzhno_is_a_mandatory_signal(self) -> None:
        early, late = TRIAGE["signal_groups"](
            "2. При подготовке файла нужно проверить путь на сервере.")
        self.assertIn("mandatory", early)
        self.assertEqual(late, [])

    def test_accepted_rule_requires_same_source_hash(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory, "shared.sqlite")
            conn = sqlite3.connect(path)
            conn.executescript("""
                CREATE TABLE meta(key TEXT,value TEXT);
                INSERT INTO meta VALUES('coverage_kind','full-body-text-verified');
                INSERT INTO meta VALUES('article_count','1');
                INSERT INTO meta VALUES('clause_count','1');
                INSERT INTO meta VALUES('audit_sha256','audit');
                CREATE TABLE articles(id TEXT,title TEXT,section TEXT,source_url TEXT,
                    clause_count INTEGER,body_sha256 TEXT);
                CREATE TABLE clauses(id TEXT,article_id TEXT,ordinal INTEGER,number TEXT,text TEXT);
                INSERT INTO articles VALUES('437','Запросы','Код','https://its.1c.ru/437',1,'body');
                INSERT INTO clauses VALUES('437:1','437',1,'1.','1. Правило');
            """)
            conn.close()
            rule = {"id": "rule", "source_sha256": hashlib.sha256(
                "1. Правило".encode()).hexdigest(), "url": "https://its.1c.ru/437"}
            result = TRIAGE["build_registry"](path, {"437:1": rule})
            self.assertEqual(result["accepted_units"], 1)
            self.assertEqual(result["units"][0]["review_status"], "accepted")
            second = {**rule, "id": "rule-2"}
            split = TRIAGE["build_registry"](path, {"437:1": [rule, second]})
            self.assertEqual(split["accepted_rules"], 2)
            self.assertEqual(split["accepted_units"], 1)
            self.assertEqual(split["units"][0]["approved_rule_ids"], ["rule", "rule-2"])
            audited = TRIAGE["build_registry"](path, {}, {"437:1"},
                                                 {"437:1": "Повторяет другое принятое правило"})
            self.assertEqual(audited["review_status_counts"]["reviewed_no_standalone"], 1)
            self.assertEqual(audited["units"][0]["review_reason"],
                             "Повторяет другое принятое правило")
            rule["source_sha256"] = "0" * 64
            with self.assertRaisesRegex(RuntimeError, "разошлась с источником"):
                TRIAGE["build_registry"](path, {"437:1": rule})


if __name__ == "__main__":
    unittest.main()
