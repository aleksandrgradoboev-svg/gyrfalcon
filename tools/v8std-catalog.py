"""Inventory the public v8std navigation tree without copying article text.

The output contains only section paths, article titles, and official URLs.
It is a coverage input, not a set of accepted programming practices.
"""

from __future__ import annotations

import argparse
import html
import json
import re
import time
from collections import deque
from datetime import datetime, timezone
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


BASE = "https://its.1c.ru"
NAV_BASE = BASE + "/db/metadata/v8std/nav_list/13/-1/"
ITEM = re.compile(
    r'<li\s+id="nav_(?P<kind>[df])(?P<id>\d+)"[^>]*>\s*'
    r'<a\s+href="(?P<href>[^"]+)"[^>]*>(?P<title>.*?)</a>\s*</li>',
    re.DOTALL,
)
TAG = re.compile(r"<[^>]+>")


def fetch(url: str) -> str:
    last: Exception | None = None
    for attempt in range(3):
        try:
            request = Request(url, headers={"User-Agent": "Gyrfalcon-v8std-inventory/1.0"})
            with urlopen(request, timeout=20) as response:
                encoding = response.headers.get_content_charset() or "utf-8"
                return response.read().decode(encoding)
        except (HTTPError, URLError, TimeoutError) as error:
            last = error
            time.sleep(1 + attempt)
    raise RuntimeError(f"Не прочитано оглавление {url}: {last}")


def crawl(delay: float) -> dict:
    queue = deque([("", [])])
    seen_paths: set[str] = set()
    sections: list[dict] = []
    articles: list[dict] = []
    while queue:
        path, parents = queue.popleft()
        if path in seen_paths:
            continue
        seen_paths.add(path)
        url = NAV_BASE + path + "_"
        page = fetch(url)
        entries = list(ITEM.finditer(page))
        if not entries:
            raise RuntimeError(f"Пустой раздел оглавления {url}")
        for match in entries:
            kind = match.group("kind")
            item_id = match.group("id")
            title = html.unescape(TAG.sub("", match.group("title"))).strip()
            href = html.unescape(match.group("href"))
            if not title or not href.startswith("/db/v8std/"):
                raise RuntimeError(f"Некорректный пункт {item_id} в {url}")
            if kind == "f":
                child_path = f"{path}{item_id}/"
                sections.append({"id": item_id, "title": title, "path": parents + [title],
                                 "url": BASE + href})
                queue.append((child_path, parents + [title]))
            else:
                articles.append({"id": item_id, "title": title, "section": parents,
                                 "url": BASE + href})
        time.sleep(delay)
    ids = [article["id"] for article in articles]
    if len(ids) != len(set(ids)):
        raise RuntimeError("Оглавление содержит повторяющиеся ID статей")
    return {
        "version": 1,
        "source": BASE + "/db/v8std",
        "navigation_source": NAV_BASE,
        "retrieved_at_utc": datetime.now(timezone.utc).isoformat(),
        "section_count": len(sections),
        "article_count": len(articles),
        "sections": sections,
        "articles": articles,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path)
    parser.add_argument("--delay", type=float, default=0.2)
    args = parser.parse_args()
    catalog = crawl(args.delay)
    print(f"Разделов: {catalog['section_count']}; статей: {catalog['article_count']}")
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(catalog, ensure_ascii=False, indent=2) + "\n",
                            encoding="utf-8")
        print(args.out.resolve())


if __name__ == "__main__":
    main()
