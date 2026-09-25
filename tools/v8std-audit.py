"""Check coverage and addressability of every official v8std article.

Only metadata, counts and hashes are written; article bodies are not cached.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import re
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timezone
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen

from bs4 import BeautifulSoup


SOURCE = re.compile(r'src="(?P<path>/db/content/v8std/src/[^"]+)"')
CLAUSE = re.compile(r"^\s*(?P<number>\d+(?:\.\d+)*\.)\s+\S")
SITE = "https://its.1c.ru"
SPACE = re.compile(r"\s+")


def body_hash(soup: BeautifulSoup) -> tuple[str, int]:
    body = copy.deepcopy(soup.body or soup)
    for tag in body.find_all(["script", "style", "noscript"]):
        tag.decompose()
    text = SPACE.sub(" ", body.get_text(" ", strip=True)).strip()
    return hashlib.sha256(text.encode("utf-8")).hexdigest(), len(text)


def fetch(url: str) -> str:
    last: Exception | None = None
    for attempt in range(3):
        try:
            request = Request(url, headers={"User-Agent": "Gyrfalcon-v8std-audit/1.0"})
            with urlopen(request, timeout=25) as response:
                encoding = response.headers.get_content_charset() or "utf-8"
                return response.read().decode(encoding)
        except (HTTPError, URLError, TimeoutError) as error:
            last = error
            time.sleep(1 + attempt)
    raise RuntimeError(f"{url}: {last}")


def inspect(article: dict) -> dict:
    result = dict(article)
    try:
        wrapper = fetch(article["url"])
        match = SOURCE.search(wrapper)
        if not match:
            raise RuntimeError("страница без адреса исходной статьи")
        source_url = SITE + quote(match.group("path"), safe="/:%")
        source_html = fetch(source_url)
        soup = BeautifulSoup(source_html, "html.parser")
        paragraphs = [p.get_text(" ", strip=True) for p in soup.find_all("p")]
        numbered = [CLAUSE.match(p).group("number") for p in paragraphs if CLAUSE.match(p)]
        normalized = "\n".join(paragraphs)
        body_sha256, body_chars = body_hash(soup)
        result.update({
            "source_url": source_url,
            "source_sha256": hashlib.sha256(normalized.encode("utf-8")).hexdigest(),
            "body_sha256": body_sha256,
            "body_chars": body_chars,
            "paragraphs": len(paragraphs),
            "numbered_paragraphs": len(numbered),
            "numbered_ids": numbered,
            "status": "readable",
        })
    except Exception as error:  # preserve exact failed article in report
        result.update({"status": "unavailable", "error": str(error)})
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--catalog", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--workers", type=int, default=3)
    parser.add_argument("--refresh", action="store_true",
                        help="Повторно проверить каждую статью, не использовать предыдущий аудит")
    args = parser.parse_args()
    if args.workers < 1 or args.workers > 4:
        parser.error("--workers: от 1 до 4")
    catalog = json.loads(args.catalog.read_text(encoding="utf-8"))
    articles = catalog["articles"]
    previous = {}
    if args.out.is_file() and not args.refresh:
        old = json.loads(args.out.read_text(encoding="utf-8"))
        previous = {row["id"]: row for row in old.get("articles", [])
                    if row.get("status") == "readable"}
    checked: list[dict] = [previous[row["id"]] for row in articles if row["id"] in previous]
    pending = [row for row in articles if row["id"] not in previous]
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures = {pool.submit(inspect, article): article for article in pending}
        for future in as_completed(futures):
            checked.append(future.result())
            if len(checked) % 50 == 0:
                print(f"Проверено статей: {len(checked)}/{len(articles)}", flush=True)
    checked.sort(key=lambda row: int(row["id"]))
    report = {
        "version": 1,
        "catalog_sha256": hashlib.sha256(args.catalog.read_bytes()).hexdigest(),
        "audited_at_utc": datetime.now(timezone.utc).isoformat(),
        "expected_articles": len(articles),
        "readable_articles": sum(row["status"] == "readable" for row in checked),
        "unavailable_articles": sum(row["status"] == "unavailable" for row in checked),
        "numbered_paragraphs": sum(row.get("numbered_paragraphs", 0) for row in checked),
        "articles": checked,
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"Статей: {report['readable_articles']}/{report['expected_articles']}; "
          f"нумерованных абзацев: {report['numbered_paragraphs']}; "
          f"ошибок: {report['unavailable_articles']}")


if __name__ == "__main__":
    main()
