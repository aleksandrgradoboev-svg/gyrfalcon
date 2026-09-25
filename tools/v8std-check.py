"""Read-only full-text extraction audit for selected v8std articles."""

from __future__ import annotations

import argparse
import json
import runpy
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

from bs4 import BeautifulSoup


BUILD = runpy.run_path(str(Path(__file__).with_name("v8std-build.py")))


def inspect(article: dict) -> tuple[str, str | None]:
    try:
        soup = BeautifulSoup(BUILD["fetch"](article["source_url"]), "html.parser")
        blocks = BUILD["chunks"](soup, article["id"], article["title"])
        # Malformed source HTML can nest <p> in <p>; the audit then sees the
        # same numbered heading twice, while one address is correct.
        missing = set(article["numbered_ids"]) - {
            block[2] for block in blocks if block[2] is not None
        }
        if missing:
            raise RuntimeError(f"не адресованы нумерованные абзацы: {sorted(missing)}")
        return article["id"], None
    except Exception as error:
        return article["id"], str(error)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--audit", type=Path, required=True)
    parser.add_argument("--start", type=int, default=0)
    parser.add_argument("--stop", type=int, default=100000)
    parser.add_argument("--workers", type=int, default=4)
    args = parser.parse_args()
    articles = json.loads(args.audit.read_text(encoding="utf-8"))["articles"]
    selected = articles[args.start:args.stop]
    failures = []
    checked = 0
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures = [pool.submit(inspect, article) for article in selected]
        for future in as_completed(futures):
            article_id, error = future.result()
            checked += 1
            if error is not None:
                failures.append((article_id, error))
                print(f"FAILED {article_id}: {error}", flush=True)
            if checked % 50 == 0:
                print(f"checked={checked}/{len(selected)}", flush=True)
    print(f"checked={checked} failed={len(failures)} range={args.start}:{args.stop}")
    if failures:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
