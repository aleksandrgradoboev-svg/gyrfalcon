"""Build one private searchable v8std corpus for all connected projects.

Article bodies are stored only in the ignored SQLite file under data/.
The tracked catalog/audit contain references and hashes, not source text.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import re
import sqlite3
import time
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

from bs4 import BeautifulSoup, Comment, NavigableString, Tag


CLAUSE = re.compile(r"^\s*(?P<number>\d+(?:\.\d+)*\.)\s+\S")
SPACE = re.compile(r"\s+")
BREAK_MARKER = "__GYRFALCON_V8STD_DOUBLE_BR__"
SKIP_BLOCKS = {"script", "style", "noscript"}


def fetch(url: str) -> str:
    last: Exception | None = None
    for attempt in range(3):
        try:
            request = Request(url, headers={"User-Agent": "Gyrfalcon-v8std-build/1.0"})
            with urlopen(request, timeout=25) as response:
                encoding = response.headers.get_content_charset() or "utf-8"
                return response.read().decode(encoding)
        except (HTTPError, URLError, TimeoutError) as error:
            last = error
            time.sleep(1 + attempt)
    raise RuntimeError(f"Не прочитана статья {url}: {last}")


def clean(value: str) -> str:
    return SPACE.sub(" ", value).strip()


def body_parts(soup: BeautifulSoup) -> list[str]:
    """Read the whole article body in DOM order, including examples and tables."""
    parts: list[str] = []
    pending: list[str] = []
    breaks = 0

    def flush() -> None:
        if not pending:
            return
        text = clean(" ".join(pending))
        pending.clear()
        for ordinal, piece in enumerate(clean(part) for part in text.split(BREAK_MARKER)):
            if not piece:
                continue
            if ordinal == 0 or CLAUSE.match(piece):
                parts.append(piece)
            else:
                parts[-1] = clean(f"{parts[-1]} {piece}")

    def walk(node: Tag | NavigableString) -> None:
        nonlocal breaks
        if isinstance(node, Comment):
            return
        if isinstance(node, NavigableString):
            value = clean(str(node))
            if value:
                pending.append(value)
                breaks = 0
            return
        if node.name in SKIP_BLOCKS:
            return
        if node.name == "br":
            breaks += 1
            if breaks == 2:
                pending.append(BREAK_MARKER)
            return
        if node.name in {"p", "pre", "h1", "h2", "h3", "h4", "h5", "h6"}:
            flush()
        for child in node.children:
            if isinstance(child, (Tag, NavigableString)):
                walk(child)
        if node.name in {"p", "pre", "h1", "h2", "h3", "h4", "h5", "h6"}:
            flush()

    walk(soup.body or soup)
    flush()
    # A full corpus must not silently drop table cells, BSL examples or lists.
    raw = copy.deepcopy(soup.body or soup)
    for tag in raw.find_all(list(SKIP_BLOCKS)):
        tag.decompose()
    extracted = clean(" ".join(parts))
    source = clean(raw.get_text(" ", strip=True))
    if extracted != source:
        first = next((i for i, (left, right) in enumerate(zip(extracted, source))
                      if left != right), min(len(extracted), len(source)))
        raise RuntimeError("Разбор статьи потерял или переставил текст HTML: "
                           f"первое различие={first}, извлечено={len(extracted)}, "
                           f"источник={len(source)}, "
                           "фрагменты=" + repr(extracted[max(0,first-30):first+50]).encode(
                               "ascii", "backslashreplace").decode() + " / " +
                           repr(source[max(0,first-30):first+50]).encode(
                               "ascii", "backslashreplace").decode())
    return parts


def body_hash(soup: BeautifulSoup) -> tuple[str, int]:
    raw = copy.deepcopy(soup.body or soup)
    for tag in raw.find_all(list(SKIP_BLOCKS)):
        tag.decompose()
    text = clean(raw.get_text(" ", strip=True))
    return hashlib.sha256(text.encode("utf-8")).hexdigest(), len(text)


def chunks(soup: BeautifulSoup, article_id: str, title: str) -> list[tuple]:
    paragraphs = body_parts(soup)
    paragraphs = [p for p in paragraphs if p]
    blocks: list[tuple] = []
    preamble: list[str] = []
    number: str | None = None
    current: list[str] = []
    ordinal = 0

    def flush() -> None:
        nonlocal ordinal
        if current:
            ordinal += 1
            blocks.append((f"{article_id}:{ordinal}", ordinal, number,
                           title, "\n".join(current)))

    for paragraph in paragraphs:
        match = CLAUSE.match(paragraph)
        if match:
            if number is None:
                preamble.extend(current)
            else:
                flush()
            number = match.group("number")
            current = [paragraph]
        elif number is None:
            preamble.append(paragraph)
        else:
            current.append(paragraph)
    if number is not None:
        flush()
    if preamble:
        blocks.insert(0, (f"{article_id}:intro", 0, None, title, "\n".join(preamble)))
    if not blocks:
        body = soup.body or soup
        text = clean(body.get_text(" ", strip=True))
        if not text:
            raise RuntimeError(f"Статья {article_id} не содержит текста")
        blocks.append((f"{article_id}:body", 0, None, title, text))
    return blocks


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--audit", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if args.out.exists():
        parser.error(f"Общая база уже существует: {args.out}; обновление требует явной команды")
    audit = json.loads(args.audit.read_text(encoding="utf-8"))
    articles = audit["articles"]
    if len(articles) != audit["expected_articles"] or any(
        article["status"] != "readable" for article in articles
    ):
        parser.error("Аудит неполон: сначала получите все статьи")
    args.out.parent.mkdir(parents=True, exist_ok=True)
    temporary = args.out.with_name(args.out.name + ".building")
    if temporary.exists():
        parser.error(f"Временная база уже существует: {temporary}")
    conn = sqlite3.connect(temporary)
    conn.executescript("""
        CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE articles(
            id TEXT PRIMARY KEY, title TEXT NOT NULL, section TEXT NOT NULL,
            url TEXT NOT NULL, source_url TEXT NOT NULL, source_sha256 TEXT NOT NULL,
            body_sha256 TEXT NOT NULL, body_chars INTEGER NOT NULL,
            clause_count INTEGER NOT NULL
        );
        CREATE TABLE clauses(
            id TEXT PRIMARY KEY, article_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
            number TEXT, title TEXT NOT NULL, section TEXT NOT NULL,
            text TEXT NOT NULL, FOREIGN KEY(article_id) REFERENCES articles(id)
        );
        CREATE VIRTUAL TABLE clause_fts USING fts5(
            id UNINDEXED, title, section, text, tokenize='unicode61'
        );
    """)
    total = 0
    for index, article in enumerate(articles, 1):
        source_html = fetch(article["source_url"])
        soup = BeautifulSoup(source_html, "html.parser")
        paragraphs = [p.get_text(" ", strip=True) for p in soup.find_all("p")]
        digest = hashlib.sha256("\n".join(paragraphs).encode("utf-8")).hexdigest()
        if digest != article["source_sha256"]:
            raise RuntimeError(f"Статья {article['id']} изменилась после аудита; обновите аудит")
        body_digest, body_chars = body_hash(soup)
        if body_digest != article.get("body_sha256") or body_chars != article.get("body_chars"):
            raise RuntimeError(f"Полный текст статьи {article['id']} отличается от аудита")
        section = " / ".join(article["section"])
        try:
            blocks = chunks(soup, article["id"], article["title"])
        except RuntimeError as error:
            raise RuntimeError(f"Статья {article['id']}: {error}") from error
        conn.execute("INSERT INTO articles VALUES (?,?,?,?,?,?,?,?,?)",
                     (article["id"], article["title"], section, article["url"],
                      article["source_url"], digest, body_digest, body_chars, len(blocks)))
        for clause_id, ordinal, number, title, body in blocks:
            conn.execute("INSERT INTO clauses VALUES (?,?,?,?,?,?,?)",
                         (clause_id, article["id"], ordinal, number, title, section, body))
            conn.execute("INSERT INTO clause_fts VALUES (?,?,?,?)",
                         (clause_id, title, section, body))
        total += len(blocks)
        if index % 50 == 0:
            print(f"Индексировано статей: {index}/{len(articles)}", flush=True)
        time.sleep(0.1)
    conn.execute("INSERT INTO meta VALUES (?,?)", ("source", "https://its.1c.ru/db/v8std"))
    conn.execute("INSERT INTO meta VALUES (?,?)", ("audit_sha256", hashlib.sha256(args.audit.read_bytes()).hexdigest()))
    conn.execute("INSERT INTO meta VALUES (?,?)", ("article_count", str(len(articles))))
    conn.execute("INSERT INTO meta VALUES (?,?)", ("clause_count", str(total)))
    conn.execute("INSERT INTO meta VALUES (?,?)", ("coverage_kind", "full-body-text-verified"))
    conn.commit()
    conn.close()
    if args.out.exists():
        raise RuntimeError(f"Целевая база появилась во время сборки: {args.out}")
    temporary.rename(args.out)
    print(f"Общий базис: {len(articles)} статей, {total} адресуемых блоков: {args.out}")


if __name__ == "__main__":
    main()
