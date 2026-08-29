# -*- coding: utf-8 -*-
"""Прогонщик корпуса приёмки: один вопрос — двум серверам, ответы сверяются.

Веха 6 требует, чтобы 12 сценариев прогонялись АВТОМАТИЧЕСКИ на обоих
серверах. Ручной просмотр этого не заменяет по той же причине, по которой
корпус вообще заведён: сегодня нечем заметить, что сервер начал врать.
Регрессия индексатора не болит — она молча меняет ответы.

Два сервера отвечают по разным протоколам, и это часть работы прогонщика:

  наш     — MCP по stdio: поднимаем `gyrfalcon serve --db <индекс>`,
            говорим initialize / tools/call, читаем JSON-RPC построчно;
  второй  — сравнительный сервер по HTTP (streamable-http), если он поднят:
            открываем сессию, шлём запрос, закрываем. Нужен только для
            сверки на полноту и не обязателен: --our-only обходится без него.

Сверка идёт МНОЖЕСТВАМИ ключей, а не текстом ответа: формат выдачи у
серверов разный, а вопрос один. Расхождение печатается поимённо с примерами —
равные счётчики при разном составе выглядят как паритет, и именно так
пропускают дефекты.

Запуск:
    python run.py                    # все сценарии
    python run.py S06 S07            # выборочно
    python run.py --our-only         # без сравнительного сервера
"""
import io
import json
import os
import re
import subprocess
import sys
import time
import sqlite3
import urllib.request

sys.stdout.reconfigure(encoding="utf-8")

КОРЕНЬ = os.path.dirname(os.path.abspath(__file__))
ПРОЕКТ = os.path.abspath(os.path.join(КОРЕНЬ, "..", ".."))
БИНАРЬ = os.path.join(ПРОЕКТ, "target", "release", "gyrfalcon.exe")
ИНДЕКСЫ = {
    "bp": os.path.join(ПРОЕКТ, "data", "bp-index.db"),
    "do": os.path.join(ПРОЕКТ, "data", "do-index.db"),
    # ЕРП.УХ — самый тяжёлый корпус (29 818 модулей, 752 335 методов, индекс
    # 3,8 ГБ). У ПРЕДШЕСТВЕННИКА он не зарегистрирован, поэтому сверка с ним
    # невозможна: прогон только односторонний (--our-only). Это проверяет,
    # что сервер держит масштаб, но паритетом не является.
    "erpuh": os.path.join(ПРОЕКТ, "data", "erpuh-index.db"),
}
ЭТАЛОН_URL = "http://127.0.0.1:9000/mcp"

# --corpus=<имя> перекрывает корпус ВСЕХ сценариев. Нужен, чтобы прогнать
# тот же корпус вопросов на другой конфигурации: сценарий описывает ВОПРОС,
# а не конфигурацию, и «работает ли это на ЕРП.УХ» — законный вопрос.
# Сценарии, привязанные к своему корпусу явно (расширения на ДО), ключ
# всё равно уважают: подмена корпуса там сделает вопрос бессмысленным,
# и это видно по результату, а не молча искажает его.
ПЕРЕКРЫТЬ_КОРПУС = next(
    (a.split("=", 1)[1] for a in sys.argv if a.startswith("--corpus=")), None
)


# ─────────────────────────── наш сервер (MCP stdio) ───────────────────────────

class НашСервер:
    """MCP-клиент по stdio. Один процесс на корпус, а не на вызов.

    Поднимать сервер заново на каждый сценарий было бы честно, но неверно:
    цена входа (tools/list) в приёмке меряется отдельно, а здесь важны
    ОТВЕТЫ. К тому же 12 запусков на индексе 3,8 ГБ — это минуты впустую.
    """

    def __init__(self, индекс):
        self.p = subprocess.Popen(
            [БИНАРЬ, "serve", "--db", индекс],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, encoding="utf-8", bufsize=1,
        )
        self._id = 0
        self._вызов("initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "acceptance", "version": "1"},
        })
        self._уведомить("notifications/initialized")

    def _уведомить(self, метод):
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": метод}) + "\n")
        self.p.stdin.flush()

    def _вызов(self, метод, params):
        self._id += 1
        зов = {"jsonrpc": "2.0", "id": self._id, "method": метод, "params": params}
        self.p.stdin.write(json.dumps(зов, ensure_ascii=False) + "\n")
        self.p.stdin.flush()
        # Читаем до ответа С НАШИМ id: сервер вправе слать уведомления,
        # и брать первую попавшуюся строку — значит однажды принять
        # чужое сообщение за ответ.
        while True:
            строка = self.p.stdout.readline()
            if not строка:
                raise RuntimeError("сервер закрыл поток, не ответив")
            try:
                d = json.loads(строка)
            except json.JSONDecodeError:
                continue
            if d.get("id") == self._id:
                if "error" in d:
                    raise RuntimeError(f"ошибка сервера: {d['error']}")
                return d["result"]

    def инструмент(self, имя, аргументы):
        r = self._вызов("tools/call", {"name": имя, "arguments": аргументы})
        куски = [c.get("text", "") for c in r.get("content", [])]
        return "\n".join(куски)

    def список_инструментов(self):
        return self._вызов("tools/list", {})["tools"]

    def закрыть(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=5)
        except Exception:
            self.p.kill()


# ─────────────────────────── эталон (MCP по HTTP) ────────────────────────────

class Эталон:
    """Клиент Python-сервера. Сессия на корпус, код исполняется в песочнице."""

    def __init__(self, проект):
        self.проект = проект
        self.session_id = None
        self._id = 0
        self._mcp_session = None

    def _зов(self, метод, params):
        self._id += 1
        тело = json.dumps({"jsonrpc": "2.0", "id": self._id,
                           "method": метод, "params": params}).encode()
        req = urllib.request.Request(ЭТАЛОН_URL, data=тело, method="POST")
        req.add_header("Content-Type", "application/json")
        req.add_header("Accept", "application/json, text/event-stream")
        if self._mcp_session:
            req.add_header("Mcp-Session-Id", self._mcp_session)
        with urllib.request.urlopen(req, timeout=120) as resp:
            sid = resp.headers.get("Mcp-Session-Id")
            if sid:
                self._mcp_session = sid
            сырое = resp.read().decode("utf-8", "replace")
        # streamable-http отвечает либо голым JSON, либо SSE-кадрами
        for строка in сырое.splitlines():
            строка = строка.strip()
            if строка.startswith("data:"):
                строка = строка[5:].strip()
            if not строка.startswith("{"):
                continue
            d = json.loads(строка)
            if d.get("id") == self._id:
                if "error" in d:
                    raise RuntimeError(f"эталон: {d['error']}")
                return d["result"]
        raise RuntimeError(f"эталон не ответил на {метод}: {сырое[:200]}")

    def открыть(self):
        self._зов("initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "acceptance", "version": "1"},
        })
        self._id += 1
        # notifications/initialized шлём без ожидания ответа
        try:
            тело = json.dumps({"jsonrpc": "2.0",
                               "method": "notifications/initialized"}).encode()
            req = urllib.request.Request(ЭТАЛОН_URL, data=тело, method="POST")
            req.add_header("Content-Type", "application/json")
            req.add_header("Accept", "application/json, text/event-stream")
            if self._mcp_session:
                req.add_header("Mcp-Session-Id", self._mcp_session)
            urllib.request.urlopen(req, timeout=30).read()
        except Exception:
            pass
        # max_execute_calls поднят намеренно: корпус — это 12 вопросов
        # в ОДНОЙ сессии, а пресет effort=low даёт лимит 10 вызовов.
        # Первый полный прогон упёрся в него на восьмом сценарии, и дальше
        # эталон отвечал «execution call limit exceeded» — что сверка
        # честно засчитала как расхождение ответов. Обрыв инструмента
        # неотличим от факта о конфигурации: ровно то, о чём стоп-условия
        # правил контура.
        r = self._зов("tools/call", {"name": "rlm_start", "arguments": {
            "query": "приёмка вехи 6", "project": self.проект, "effort": "low",
            "max_execute_calls": 60, "max_output_chars": 100000,
        }})
        текст = "".join(c.get("text", "") for c in r.get("content", []))
        d = json.loads(текст) if текст.strip().startswith("{") else {}
        внутр = d.get("result", текст)
        if isinstance(внутр, str):
            внутр = json.loads(внутр)
        self.session_id = внутр["session_id"]
        return внутр

    def исполнить(self, код):
        r = self._зов("tools/call", {"name": "rlm_execute", "arguments": {
            "session_id": self.session_id, "code": "import json\n" + код,
        }})
        текст = "".join(c.get("text", "") for c in r.get("content", []))
        try:
            d = json.loads(текст)
            вн = d.get("result", d)
            if isinstance(вн, str):
                вн = json.loads(вн)
            return вн.get("stdout", "") or вн.get("output", "") or текст
        except Exception:
            return текст

    def закрыть(self):
        if self.session_id:
            try:
                self._зов("tools/call", {"name": "rlm_end",
                                         "arguments": {"session_id": self.session_id}})
            except Exception:
                pass


# ─────────────────────────────── сверка ──────────────────────────────────────

def отфильтровать_по_виду(d, вид):
    """Оставить в выдаче `find` только строки нужного вида.

    С Р-017 (29.08.2026) наш `find` отдаёт ОДИН ранжированный список, где
    вид сущности — колонка, а не раздел. Сценарий, спрашивающий про объекты
    метаданных, обязан сравниваться с объектами: иначе в наше множество
    попадут одноимённые методы, и мы объявим «нашли больше» там, где
    просто смешали разные сущности.

    Колонки: [name, kind, score, reason, details] — kind второй.
    """
    if not isinstance(d, list):
        return d
    итог = []
    for строка in d:
        if isinstance(строка, list) and len(строка) >= 2:
            if строка[1] == вид:
                итог.append([строка[0]])
        else:
            итог.append(строка)
    return итог


def в_множество(сырое, поле=None, вид=None):
    """Ответ любого сервера — во множество сравнимых ключей.

    Тексты у серверов разные, поэтому сравнивается СОДЕРЖАНИЕ: JSON
    разбирается в кортежи, свободный текст — построчно. Регистр и пробелы
    нормализуются, потому что расхождение «Справочник.X» / «Справочник. X»
    это разница вывода, а не факта о конфигурации.

    `поле` — путь к той части ответа, которая отвечает на вопрос сценария
    (например `objects`). Без него сверка тянет в множество ВСЁ, что
    сервер приложил к ответу: у нашего `find` рядом с точными попаданиями
    лежит семантический поиск, помеченный самим сервером как несмешиваемый
    с точным. Сравнивать вопрос с ответом на другой вопрос — брак сверки,
    а не расхождение серверов.
    """
    сырое = (сырое or "").strip()
    if not сырое:
        return set()
    if сырое.startswith(("[", "{")):
        try:
            d = json.loads(сырое)
            if поле:
                for ч in поле.split("."):
                    if isinstance(d, dict) and ч in d:
                        d = d[ч]
                    else:
                        return set()
            if вид:
                d = отфильтровать_по_виду(d, вид)
            return _плоско(d)
        except json.JSONDecodeError:
            pass
    # Свободный текст: строки без служебной шапки.
    итог = set()
    for строка in сырое.splitlines():
        строка = строка.strip(" ·-—\t")
        if строка and not строка.startswith(("#", "//")):
            итог.add(_норма(строка))
    return итог


def _норма(x):
    return re.sub(r"\s+", " ", str(x)).strip().lower()


def _плоско(d):
    итог = set()
    if isinstance(d, dict):
        d = d.get("items") or d.get("rows") or d.get("result") or list(d.values())
    if isinstance(d, list):
        for x in d:
            if isinstance(x, (list, tuple)):
                итог.add(tuple(_норма(i) for i in x))
            elif isinstance(x, dict):
                итог.add(tuple(_норма(v) for v in x.values()))
            else:
                итог.add(_норма(x))
    else:
        итог.add(_норма(d))
    return итог


def _по_первому(м):
    """Кортежи — к первому полю. Общий знаменатель двух форматов.

    Мы отдаём (имя, категория), эталон — только имя. Это разница ВЫВОДА:
    на вопрос «какие объекты называются так» оба ответили одинаково, и
    засчитывать расхождение здесь — значит мерить формат вместо факта.
    Категория при этом не теряется: она видна в самом ответе и проверяется
    отдельными сценариями, где она и есть предмет вопроса.
    """
    return {(x[0] if isinstance(x, tuple) and x else x) for x in м}


def _колонки(сырое, поле):
    """Имена колонок нашего ответа — они лежат рядом с rows."""
    try:
        d = json.loads(сырое)
    except Exception:
        return None
    if поле:
        for ч in поле.split(".")[:-1]:      # последний элемент — rows
            if isinstance(d, dict) and ч in d:
                d = d[ч]
            else:
                return None
    return d.get("columns") if isinstance(d, dict) else None


def _по_именам(м, колонки, нужные):
    """Кортеж — к подмножеству колонок, названных по ИМЕНИ.

    Позиционный ключ ломается молча: стоит поменять порядок полей в
    выборке, и сверка начнёт резать не то, продолжая показывать
    правдоподобные расхождения. На S06 это и случилось — у нас кортеж
    (объект, метод, вид), у эталона (расширение, объект, метод), а
    ключ [0,1,2] вырезал с двух сторон разное.
    """
    if not колонки:
        return м
    инд = [колонки.index(н) for н in нужные if н in колонки]
    if len(инд) != len(нужные):
        return м
    return {tuple(x[i] for i in инд) if isinstance(x, tuple) else x for x in м}


def _по_ключу(м, поля):
    """Кортеж — к подмножеству полей. Форматы разные, факт один.

    Порядок и состав колонок у серверов не совпадают (у нас перехват несёт
    адрес, у эталона нет), поэтому сверка идёт по ИНВАРИАНТНОЙ части —
    той, что обязана быть у обоих. Отброшенное при этом не пропадает из
    приёмки: оно проверяется как отдельное требование сценария (`плюс`),
    иначе мы бы прятали своё преимущество, чтобы сойтись с эталоном.
    """
    итог = set()
    for x in м:
        if isinstance(x, tuple):
            итог.add(tuple(x[i] for i in поля if i < len(x)))
        else:
            итог.add(x)
    return итог


def сверить(наш, эталон, наше_поле=None, по_первому=False, по_ключу=None,
            наши_колонки=None, эталон_колонки=None, наш_вид=None):
    a, b = в_множество(наш, наше_поле, наш_вид), в_множество(эталон)
    if по_первому:
        a, b = _по_первому(a), _по_первому(b)
    if по_ключу and наши_колонки:
        # по_ключу здесь — СПИСОК ИМЁН колонок, общих для обоих серверов.
        a = _по_именам(a, наши_колонки, по_ключу)
        b = _по_именам(b, эталон_колонки or по_ключу, по_ключу)
    elif по_ключу:
        a, b = _по_ключу(a, по_ключу), _по_ключу(b, по_ключу)
    только_наш, только_эталон = a - b, b - a
    return {
        "наш": len(a), "эталон": len(b),
        "лишних_у_нас": len(только_наш), "потеряно": len(только_эталон),
        "паритет": not только_наш and not только_эталон,
        "примеры_лишних": sorted(map(str, только_наш))[:5],
        "примеры_потерянных": sorted(map(str, только_эталон))[:5],
    }


# ─────────────────────────────── прогон ──────────────────────────────────────

def плоский_объект(корень):
    """Объект, выгруженный плоским XML: есть Category/Имя.xml, нет Category/Имя/.

    Нужен для S12. Подбирается программно, а не вписывается в сценарий:
    имя, выбранное однажды руками, устаревает вместе с конфигурацией.
    """
    for кат in ("Catalogs", "Documents", "InformationRegisters", "Reports"):
        d = os.path.join(корень, кат)
        if not os.path.isdir(d):
            continue
        for f in sorted(os.listdir(d)):
            if f.endswith(".xml"):
                имя = f[:-4]
                if not os.path.isdir(os.path.join(d, имя)):
                    return имя
    return None


def корень_выгрузки(корпус):
    """Каталог исходников, из которого собран индекс этого корпуса.

    Читается из самого индекса: он помнит, откуда собран, и путь всегда
    тот, на котором его строили. Индекса нет или запись пуста — пустая
    строка: сценарий с подстановкой не сработает, и это честнее, чем
    подставить чужой путь.
    """
    путь = ИНДЕКСЫ.get(корпус)
    if not путь or not os.path.exists(путь):
        return ""
    try:
        c = sqlite3.connect("file:" + путь.replace("\\", "/") + "?mode=ro", uri=True)
        строка = c.execute("SELECT value FROM index_meta WHERE key='source_path'").fetchone()
        c.close()
        return строка[0] if строка else ""
    except sqlite3.Error:
        return ""



def main():
    сценарии = json.load(io.open(os.path.join(КОРЕНЬ, "scenarios.json"),
                                 encoding="utf-8"))["сценарии"]
    только = [a for a in sys.argv[1:] if not a.startswith("--")]
    без_эталона = "--our-only" in sys.argv
    if только:
        сценарии = [s for s in сценарии if s["id"] in только]

    # Плоский объект подставляется в S12 по фактическому корню выгрузки.
    #
    # Корень берётся ИЗ ИНДЕКСА (index_meta.source_path), а не задаётся
    # здесь списком: индекс сам помнит, откуда собран. Прежде тут стояли
    # три абсолютных пути машины автора — прогон у любого другого человека
    # падал на первом же из них.
    for с in сценарии:
        if "@ПЛОСКИЙ@" in json.dumps(с, ensure_ascii=False):
            корпус_с = ПЕРЕКРЫТЬ_КОРПУС or с.get("корпус", "bp")
            имя = плоский_объект(корень_выгрузки(корпус_с))
            if имя:
                текст = json.dumps(с, ensure_ascii=False).replace("@ПЛОСКИЙ@", имя)
                с.update(json.loads(текст))
                с["подставлено"] = имя

    итоги = []
    наши, эталоны = {}, {}
    try:
        for с in сценарии:
            корпус = ПЕРЕКРЫТЬ_КОРПУС or с.get("корпус", "bp")
            if корпус not in наши:
                наши[корпус] = НашСервер(ИНДЕКСЫ[корпус])
            t0 = time.time()
            try:
                наш = наши[корпус].инструмент(с["наш"]["tool"], с["наш"]["args"])
            except Exception as e:
                наш = f"ОШИБКА: {e}"
            наше_время = time.time() - t0

            эт, эт_время = "", 0.0
            if not без_эталона:
                if корпус not in эталоны:
                    эталоны[корпус] = Эталон(корпус)
                    эталоны[корпус].открыть()
                t1 = time.time()
                try:
                    эт = эталоны[корпус].исполнить(с["эталон"]["code"])
                except Exception as e:
                    эт = f"ОШИБКА: {e}"
                эт_время = time.time() - t1

            обрыв = any(м in (эт or "") for м in (
                "call limit exceeded", "output truncated", "Traceback"))
            r = (сверить(наш, эт, с.get("наше_поле"), с.get("по_первому", False),
                         с.get("по_ключу"),
                         _колонки(наш, с.get("наше_поле")),
                         с.get("эталон_колонки"), с.get("наш_вид"))
                 if not без_эталона else {"паритет": None})
            if обрыв:
                # Сорванный вызов эталона — не расхождение серверов.
                # Засчитать его в свою пользу значит объявить победу там,
                # где соперник не вышел на поле.
                r["паритет"] = None
                r["обрыв_эталона"] = True
            r.update({"id": с["id"], "вопрос": с["вопрос"], "корпус": корпус,
                      "наше_время": round(наше_время, 2),
                      "эталон_время": round(эт_время, 2),
                      "наш_ответ_знаков": len(наш or ""),
                      "эталон_ответ_знаков": len(эт or "")})
            # Сценарий с ПРОВЕРЕННЫМ по первоисточнику ответом судится по
            # факту, а не по эталону. Иначе приёмка вечно красная там, где
            # расхождение уже разобрано и правы мы: «не сошлось с эталоном»
            # и «неверно» — разные вещи, и путать их значит зафиксировать
            # чужую ошибку как норму.
            эт_ответ = с.get("эталонный_ответ")
            if эт_ответ and not обрыв:
                наше_число = r.get("наш")
                r["по_первоисточнику"] = эт_ответ["значение"]
                r["паритет"] = (наше_число == эт_ответ["значение"])
                r["вердикт"] = эт_ответ["вердикт"]

            # Дополнительное требование сценария сверх паритета.
            # У S06 это адрес перехвата: сойтись с эталоном по составу мало,
            # если при этом потерять то, чем мы его превосходим. Проверка
            # смотрит, что названные колонки есть и заполнены.
            треб = с.get("требуются_колонки")
            if треб:
                кол = _колонки(наш, с.get("наше_поле")) or []
                нет = [к for к in треб if к not in кол]
                пустые = []
                if not нет:
                    строки = в_множество(наш, с.get("наше_поле"))
                    инд = [кол.index(к) for к in треб]
                    for x in строки:
                        if isinstance(x, tuple) and any(
                                (x[i] in ("", "none", "null")) for i in инд if i < len(x)):
                            пустые.append(x)
                r["доп_требование"] = {
                    "колонки": треб,
                    "отсутствуют": нет,
                    "пустых_строк": len(пустые),
                    "выполнено": not нет and not пустые,
                }
                if not r["доп_требование"]["выполнено"]:
                    r["паритет"] = False

            если_подставлено = с.get("подставлено")
            if если_подставлено:
                r["подставлено"] = если_подставлено
            итоги.append(r)

            знак = ("ФАКТ" if (r.get("паритет") and r.get("по_первоисточнику"))
                    else "OK" if r.get("паритет")
                    else ("ОБРЫВ" if r.get("обрыв_эталона")
                          else ("--" if r.get("паритет") is None else "!!")))
            print(f"[{знак}] {с['id']} {с['вопрос'][:52]:52} "
                  f"наш {r.get('наш', '?'):>5} / эталон {r.get('эталон', '?'):>5} "
                  f"({наше_время:.2f}с / {эт_время:.2f}с)")
            if not r.get("паритет") and r.get("паритет") is not None:
                # .get, а не []: в режиме --our-only и при обрыве эталона
                # сверки не было вовсе, и ключей нет. Падать на печати
                # результата — худший способ сообщить, что сверка не шла.
                for п in r.get("примеры_потерянных", []):
                    print(f"        потеряно: {п[:100]}")
                for п in r.get("примеры_лишних", []):
                    print(f"        лишнее:   {п[:100]}")
    finally:
        for с in наши.values():
            с.закрыть()
        for э in эталоны.values():
            э.закрыть()

    путь = os.path.join(КОРЕНЬ, "last-run.json")
    io.open(путь, "w", encoding="utf-8").write(
        json.dumps(итоги, ensure_ascii=False, indent=1))
    ок = sum(1 for r in итоги if r.get("паритет"))
    print(f"\nпаритет: {ок} из {len(итоги)} · подробности: {путь}")


if __name__ == "__main__":
    main()
