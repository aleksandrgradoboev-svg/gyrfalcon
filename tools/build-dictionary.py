#!/usr/bin/env python
"""Офлайн-построение словаря векторов для семантического поиска.

# Зачем отдельным скриптом, а не частью сервера

Решение Р-006 запрещает инференс во время индексации: модель, зовомая при
сборке, убила бы скорость — первый из трёх критериев замены — и привязала бы
индексатор к аптайму сервиса. Поэтому векторы считаются ЗДЕСЬ, один раз,
и кладутся в таблицу `semantic_dictionary`, откуда сервер их только читает.

Модель после прогона можно удалить: серверу она не нужна ни при сборке
индекса, ни при ответе на запрос.

# Модель

`jina-embeddings-v3` — выбрана замером 28.08.2026, а не по описанию:

| Проба | CodeRankEmbed (137M) | jina-v3 (570M) |
|---|---|---|
| `себестоимость` | 13 суб-токенов (по буквам) | 3 осмысленных куска |
| `склад ~ warehouse` | 0,013 | **0,829** |
| `покупатель ~ customer` | 0,044 | **0,848** |
| `покупатель ~ контрагент` | 0,463 | **0,671** |

CodeRankEmbed обучена на английском коде и разбирает кириллицу побуквенно —
для наших имён негодна, что и показал замер. jina-v3 многоязычная
(XLM-RoBERTa), кириллицу держит как язык и знает межъязыковые пары.

# Окружение

Код модели написан под transformers 4.x и падает на 5.x
(`all_tied_weights_keys`), а её собственный `encode` с LoRA-адаптерами роняет
процесс segfault'ом на CPU. Поэтому: отдельный venv с transformers 4.49
и прямой `forward` с mean pooling вместо `encode`.

    python -m venv .venv-embed
    .venv-embed/Scripts/python -m pip install "transformers==4.49.0" torch numpy einops
    .venv-embed/Scripts/python tools/build-dictionary.py <индекс.db>

Скрипт **дозаписывает** словарь: уже посчитанные токены пропускаются, поэтому
прерванный прогон продолжается с места остановки, а не начинается заново.
"""

import sqlite3
import sys
import time
import warnings

warnings.filterwarnings("ignore")

MODEL = "jinaai/jina-embeddings-v3"
DIM = 1024  # обязан совпадать с semantic::DIM, иначе блобы отвергнутся при загрузке
BATCH = 64


def main() -> int:
    if len(sys.argv) < 2:
        print("укажите путь к индексу: build-dictionary.py <индекс.db>", file=sys.stderr)
        return 2
    db_path = sys.argv[1]

    conn = sqlite3.connect(db_path)
    # Таблицы создаёт сервер; если их нет, индекс собран без семантики.
    есть = conn.execute(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN"
        " ('semantic_tokens','semantic_dictionary')"
    ).fetchone()[0]
    if есть != 2:
        print("в индексе нет таблиц семантики — соберите его этим сервером", file=sys.stderr)
        return 1

    готовые = {r[0] for r in conn.execute("SELECT token FROM semantic_dictionary")}
    токены = [
        r[0]
        for r in conn.execute("SELECT token FROM semantic_tokens ORDER BY df DESC")
        if r[0] not in готовые
    ]
    print(f"в словаре уже {len(готовые)}, посчитать {len(токены)}", flush=True)
    if not токены:
        return 0

    import numpy as np
    import torch
    from transformers import AutoModel, AutoTokenizer

    torch.set_num_threads(6)
    tok = AutoTokenizer.from_pretrained(MODEL, trust_remote_code=True)
    mdl = AutoModel.from_pretrained(
        MODEL, trust_remote_code=True, torch_dtype=torch.float32, use_flash_attn=False
    ).eval()
    if mdl.config.hidden_size != DIM:
        print(
            f"размерность модели {mdl.config.hidden_size} != DIM {DIM} в semantic.rs",
            file=sys.stderr,
        )
        return 1

    t0 = time.time()
    for i in range(0, len(токены), BATCH):
        часть = токены[i : i + BATCH]
        b = tok(часть, padding=True, truncation=True, max_length=32, return_tensors="pt")
        with torch.no_grad():
            out = mdl(**b)
        h = out[0] if isinstance(out, (tuple, list)) else out.last_hidden_state
        m = b["attention_mask"].unsqueeze(-1).float()
        v = ((h * m).sum(1) / m.sum(1)).numpy()
        v = v / np.linalg.norm(v, axis=1, keepdims=True)
        # int8 при полной размерности почти бесплатен: корреляция с полным
        # вектором 0,9994 (замер 28.08.2026 на 300 именах методов БП).
        q = np.clip(np.round(v * 127), -127, 127).astype(np.int8)
        conn.executemany(
            "INSERT OR REPLACE INTO semantic_dictionary(token, vector, source) VALUES (?,?,?)",
            [(t, q[j].tobytes(), "jina-embeddings-v3") for j, t in enumerate(часть)],
        )
        conn.commit()  # каждый батч: прерывание не теряет посчитанное
        сделано = i + len(часть)
        скорость = сделано / max(time.time() - t0, 1e-6)
        осталось = (len(токены) - сделано) / max(скорость, 1e-6)
        print(
            f"  {сделано}/{len(токены)}  {скорость:.1f} ток/с  осталось ~{осталось / 60:.1f} мин",
            flush=True,
        )

    всего = conn.execute("SELECT count(*) FROM semantic_dictionary").fetchone()[0]
    print(f"готово: {всего} токенов за {(time.time() - t0) / 60:.1f} мин")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
