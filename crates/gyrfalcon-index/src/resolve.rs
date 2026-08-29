//! Разрешение имён вызовов в рёбра графа.
//!
//! Парсер отдаёт **имена в точке вызова**, а не рёбра. Ребро появляется, когда
//! имя разрешено в конкретное определение. Здесь это и делается.
//!
//! # Почему у ребра есть класс (решение Р-005)
//!
//! Замер прежнего инструмента 28.08.2026 показал: 82% его неразрешённых рёбер — вызовы методов
//! платформы на локальных переменных (`Заголовки.Вставить`). Их не «не сумели»
//! разрешить — их **некуда** разрешать: `Вставить` это метод соответствия
//! в платформе, а не процедура конфигурации.
//!
//! Поэтому неразрешённое ребро несёт класс, а не пустоту. Пустой `callee_key`
//! не отличает «не сумели» от «нечего разрешать» — и доля разрешённых по всем
//! рёбрам подряд меряет состав корпуса, а не работу резолвера.

use std::collections::HashMap;

/// Как получено ребро.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Метод того же модуля.
    Local,
    /// `ОбщийМодуль.Метод`.
    CommonModule,
    /// `Справочники.Х.Метод` и родня — менеджер объекта метаданных.
    ObjectManager,
    /// Метод платформы на переменной: `Заголовки.Вставить`.
    /// **Неразрешимо** без вывода типов — и это факт о языке, а не наш недочёт.
    PlatformVar,
    /// Глобальная функция платформы: `НСтр`, `СокрЛП`, `ЗначениеЗаполнено`.
    ///
    /// Вызывается без квалификатора и **не определена ни в одном модуле
    /// конфигурации** — потому что определена в платформе. Разрешать её
    /// в код конфигурации не во что, и это тоже факт о языке.
    ///
    /// Класс заведён после первого прогона на живом корпусе 28.08.2026:
    /// без него 1 194 750 таких вызовов (41% графа) сваливались в `unknown`
    /// вместе с настоящими «не опознал» — та же болезнь, против которой
    /// заведён Р-005, только этажом ниже.
    PlatformGlobal,
    /// Имя не опознано ничем из перечисленного. **Настоящий** недочёт
    /// резолвера, а не свойство языка — и потому разрешимый класс.
    Unknown,
}

impl Resolution {
    /// Строкой — так класс ложится в столбец `calls.resolution`.
    pub fn as_str(self) -> &'static str {
        match self {
            Resolution::Local => "local",
            Resolution::CommonModule => "common_module",
            Resolution::ObjectManager => "object_manager",
            Resolution::PlatformVar => "platform_var",
            Resolution::PlatformGlobal => "platform_global",
            Resolution::Unknown => "unknown",
        }
    }

    /// Можно ли это ребро разрешить в принципе.
    ///
    /// Знаменатель главного критерия вехи: доля разрешённых считается
    /// **внутри разрешимых классов**, иначе её задаёт состав корпуса.
    ///
    /// Неразрешимы два класса, и оба — по свойству языка, а не по нашей
    /// слабости: вызов на переменной (нужен вывод типов) и глобальная функция
    /// платформы (её определения в конфигурации нет вовсе).
    pub fn is_resolvable(self) -> bool {
        !matches!(self, Resolution::PlatformVar | Resolution::PlatformGlobal)
    }
}

/// Ребро графа после разрешения.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCall {
    /// Имя как в точке вызова.
    pub callee_name: String,
    /// Ключ цели: `<rel_path>::<имя в нижнем регистре>`. Формат прежний инструментский.
    pub callee_key: Option<String>,
    pub resolution: Resolution,
    pub confidence: f32,
}

impl Default for ResolvedCall {
    fn default() -> Self {
        Self {
            callee_name: String::new(),
            callee_key: None,
            resolution: Resolution::Unknown,
            confidence: 0.0,
        }
    }
}

/// Коллекции-менеджеры платформы: голова вызова `Справочники.Х.Метод`.
///
/// Список закрытый — это имена из платформы, а не из конфигурации,
/// и они не меняются от базы к базе.
const MANAGER_ROOTS: &[&str] = &[
    "справочники",
    "документы",
    "регистрысведений",
    "регистрынакопления",
    "регистрыбухгалтерии",
    "регистрырасчета",
    "обработки",
    "отчеты",
    "планывидовхарактеристик",
    "планысчетов",
    "планывидоврасчета",
    "перечисления",
    "бизнеспроцессы",
    "задачи",
    "планыобмена",
    "константы",
    "последовательности",
    "журналыдокументов",
];

/// Указатель на определение метода.
#[derive(Debug, Clone, Copy)]
pub struct MethodRef {
    pub method_id: i64,
    /// Экспортный метод виден снаружи модуля; неэкспортный — нет.
    pub is_export: bool,
}

/// Таблицы, по которым разрешаются имена.
///
/// Ключи в нижнем регистре: BSL нечувствителен к регистру, и `ОбщегоНазначения`
/// с `ОБЩЕГОНАЗНАЧЕНИЯ` — один модуль. `to_lowercase` берётся юникодный,
/// а не ASCII: на кириллице ASCII-вариант молча ничего не делает.
#[derive(Debug, Default)]
pub struct ResolveTables {
    /// имя общего модуля → (rel_path, методы: имя → ссылка)
    pub common_modules: HashMap<String, (String, HashMap<String, MethodRef>)>,
    /// имя объекта метаданных → (rel_path менеджерского модуля, методы)
    pub managers: HashMap<String, (String, HashMap<String, MethodRef>)>,
    /// rel_path модуля → его методы
    pub by_module: HashMap<String, HashMap<String, MethodRef>>,
    /// Все имена методов, определённых **где-либо** в конфигурации.
    ///
    /// Нужны, чтобы отличить глобальную функцию платформы (`НСтр`, `СокрЛП`)
    /// от вызова метода конфигурации, который резолвер не сумел привязать.
    /// Первое — свойство языка, второе — наш недочёт; складывать их в один
    /// класс значит прятать второе за первым.
    ///
    /// Признак измеримый, а не список из памяти: если имя не определено
    /// ни в одном из 18 230 модулей, оно определено в платформе.
    pub defined_anywhere: std::collections::HashSet<String>,
}

/// Ключ цели в прежний инструментском формате: путь и имя метода в нижнем регистре.
fn key_of(rel_path: &str, method: &str) -> String {
    format!("{rel_path}::{}", method.to_lowercase())
}

/// Разрешить одно имя вызова.
///
/// `caller_module` — путь модуля, откуда идёт вызов: нужен для локальных имён.
pub fn resolve(name: &str, caller_module: &str, t: &ResolveTables) -> ResolvedCall {
    let low = name.to_lowercase();

    let Some((head, tail)) = low.split_once('.') else {
        // Имя без точки: метод своего же модуля либо метод платформы.
        if t.by_module
            .get(caller_module)
            .and_then(|ms| ms.get(&low))
            .is_some()
        {
            return ResolvedCall {
                callee_name: name.to_string(),
                callee_key: Some(key_of(caller_module, &low)),
                resolution: Resolution::Local,
                confidence: 1.0,
            };
        }
        // Имя нигде в конфигурации не определено — значит определено
        // в платформе. Разрешать его в код конфигурации не во что.
        if !t.defined_anywhere.contains(&low) {
            return ResolvedCall {
                callee_name: name.to_string(),
                resolution: Resolution::PlatformGlobal,
                ..Default::default()
            };
        }
        // Имя в конфигурации есть, но не в этом модуле: экспортный метод
        // другого модуля, вызванный без квалификатора, обработчик формы
        // и подобное. Это НАШ недочёт, и класс это признаёт.
        return ResolvedCall {
            callee_name: name.to_string(),
            resolution: Resolution::Unknown,
            ..Default::default()
        };
    };

    // Общий модуль: ОбщегоНазначения.ЗначениеРеквизита
    if let Some((path, methods)) = t.common_modules.get(head) {
        if let Some(m) = methods.get(tail) {
            // Неэкспортный метод снаружи не виден — уверенность ниже,
            // но ребро всё равно ведёт туда, куда указывает имя.
            let conf = if m.is_export { 1.0 } else { 0.6 };
            return ResolvedCall {
                callee_name: name.to_string(),
                callee_key: Some(key_of(path, tail)),
                resolution: Resolution::CommonModule,
                confidence: conf,
            };
        }
        // Модуль известен, метода в нём нет: имя опознано, цель — нет.
        return ResolvedCall {
            callee_name: name.to_string(),
            resolution: Resolution::CommonModule,
            ..Default::default()
        };
    }

    // Менеджер объекта: Справочники.Номенклатура.НайтиПоКоду
    if MANAGER_ROOTS.contains(&head) {
        // Голова — коллекция платформы, значит середина это имя объекта,
        // а хвост — метод его менеджерского модуля.
        if let Some((obj, method)) = tail.split_once('.') {
            if let Some((path, methods)) = t.managers.get(obj) {
                if methods.contains_key(method) {
                    return ResolvedCall {
                        callee_name: name.to_string(),
                        callee_key: Some(key_of(path, method)),
                        resolution: Resolution::ObjectManager,
                        confidence: 1.0,
                    };
                }
            }
        }
        return ResolvedCall {
            callee_name: name.to_string(),
            resolution: Resolution::ObjectManager,
            ..Default::default()
        };
    }

    // Голова — не модуль и не коллекция платформы. Значит переменная,
    // и хвост — метод платформы на ней. Разрешать нечего.
    ResolvedCall {
        callee_name: name.to_string(),
        resolution: Resolution::PlatformVar,
        ..Default::default()
    }
}

/// Счётчики по классам — то, чем меряется контрольная точка вехи 2.
#[derive(Debug, Clone, Default)]
pub struct ResolutionStats {
    pub local: u64,
    pub common_module: u64,
    pub object_manager: u64,
    pub platform_var: u64,
    pub platform_global: u64,
    pub unknown: u64,
    /// Из всех рёбер — с непустым `callee_key`.
    pub resolved: u64,
    pub total: u64,
}

impl ResolutionStats {
    pub fn add(&mut self, c: &ResolvedCall) {
        self.total += 1;
        if c.callee_key.is_some() {
            self.resolved += 1;
        }
        match c.resolution {
            Resolution::Local => self.local += 1,
            Resolution::CommonModule => self.common_module += 1,
            Resolution::ObjectManager => self.object_manager += 1,
            Resolution::PlatformVar => self.platform_var += 1,
            Resolution::PlatformGlobal => self.platform_global += 1,
            Resolution::Unknown => self.unknown += 1,
        }
    }

    /// Слить счётчики другого воркера в свои.
    pub fn merge(&mut self, o: &ResolutionStats) {
        self.local += o.local;
        self.common_module += o.common_module;
        self.object_manager += o.object_manager;
        self.platform_var += o.platform_var;
        self.platform_global += o.platform_global;
        self.unknown += o.unknown;
        self.resolved += o.resolved;
        self.total += o.total;
    }

    /// Рёбра, которые разрешить возможно в принципе.
    ///
    /// Из знаменателя исключены оба неразрешимых класса — вызов на переменной
    /// и глобальная функция платформы. Оба неразрешимы по свойству языка,
    /// а не по слабости резолвера, и держать их в знаменателе значит мерить
    /// состав корпуса вместо своей работы.
    pub fn resolvable(&self) -> u64 {
        self.total - self.platform_var - self.platform_global
    }

    /// Доля разрешённых **внутри разрешимого** — главный критерий (Р-005).
    pub fn resolved_share_of_resolvable(&self) -> f64 {
        let r = self.resolvable();
        if r == 0 {
            return 0.0;
        }
        self.resolved as f64 / r as f64
    }

    /// Доля по всем рёбрам подряд — так считает прежний инструмент (46,6%).
    /// Оставлена только для сверки с ним один-в-один.
    pub fn resolved_share_raw(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.resolved as f64 / self.total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables() -> ResolveTables {
        let mut t = ResolveTables::default();
        let mut methods = HashMap::new();
        methods.insert(
            "значениереквизита".to_string(),
            MethodRef {
                method_id: 1,
                is_export: true,
            },
        );
        methods.insert(
            "внутренняя".to_string(),
            MethodRef {
                method_id: 2,
                is_export: false,
            },
        );
        t.common_modules.insert(
            "общегоназначения".to_string(),
            (
                "CommonModules/ОбщегоНазначения/Ext/Module.bsl".to_string(),
                methods,
            ),
        );

        let mut mgr = HashMap::new();
        mgr.insert(
            "найтипокоду".to_string(),
            MethodRef {
                method_id: 3,
                is_export: true,
            },
        );
        t.managers.insert(
            "номенклатура".to_string(),
            (
                "Catalogs/Номенклатура/Ext/ManagerModule.bsl".to_string(),
                mgr,
            ),
        );

        let mut own = HashMap::new();
        own.insert(
            "своя".to_string(),
            MethodRef {
                method_id: 4,
                is_export: false,
            },
        );
        t.by_module
            .insert("Catalogs/Товар/Ext/ObjectModule.bsl".to_string(), own);
        t
    }

    #[test]
    fn общий_модуль_разрешается() {
        let t = tables();
        let r = resolve("ОбщегоНазначения.ЗначениеРеквизита", "любой", &t);
        assert_eq!(r.resolution, Resolution::CommonModule);
        assert_eq!(
            r.callee_key.as_deref(),
            Some("CommonModules/ОбщегоНазначения/Ext/Module.bsl::значениереквизита")
        );
        assert_eq!(r.confidence, 1.0);
    }

    #[test]
    fn регистр_имени_не_важен() {
        // BSL нечувствителен к регистру. Проверяется на кириллице намеренно:
        // ASCII-вариант to_lowercase на ней молча не делает ничего.
        let t = tables();
        let r = resolve("ОБЩЕГОНАЗНАЧЕНИЯ.ЗНАЧЕНИЕРЕКВИЗИТА", "любой", &t);
        assert_eq!(r.resolution, Resolution::CommonModule);
        assert!(r.callee_key.is_some());
    }

    #[test]
    fn неэкспортный_метод_разрешается_но_с_меньшей_уверенностью() {
        let t = tables();
        let r = resolve("ОбщегоНазначения.Внутренняя", "любой", &t);
        assert!(r.callee_key.is_some());
        assert!(r.confidence < 1.0, "неэкспортный метод виден снаружи?");
    }

    #[test]
    fn менеджер_объекта_разрешается() {
        let t = tables();
        let r = resolve("Справочники.Номенклатура.НайтиПоКоду", "любой", &t);
        assert_eq!(r.resolution, Resolution::ObjectManager);
        assert_eq!(
            r.callee_key.as_deref(),
            Some("Catalogs/Номенклатура/Ext/ManagerModule.bsl::найтипокоду")
        );
    }

    #[test]
    fn локальный_вызов_разрешается() {
        let t = tables();
        let r = resolve("Своя", "Catalogs/Товар/Ext/ObjectModule.bsl", &t);
        assert_eq!(r.resolution, Resolution::Local);
        assert!(r.callee_key.is_some());
    }

    #[test]
    fn метод_платформы_на_переменной_помечается_а_не_молчит() {
        // Главный случай Р-005: 82% неразрешённых рёбер прежнего инструмента — такие.
        // Класс обязан быть проставлен, иначе «не сумели» неотличимо
        // от «разрешать нечего».
        let t = tables();
        let r = resolve("Заголовки.Вставить", "любой", &t);
        assert_eq!(r.resolution, Resolution::PlatformVar);
        assert!(r.callee_key.is_none());
        assert!(!r.resolution.is_resolvable());
    }

    #[test]
    fn доля_считается_внутри_разрешимого() {
        let mut s = ResolutionStats::default();
        let t = tables();
        // одно разрешённое и четыре неразрешимых
        s.add(&resolve("ОбщегоНазначения.ЗначениеРеквизита", "x", &t));
        for _ in 0..4 {
            s.add(&resolve("Заголовки.Вставить", "x", &t));
        }
        assert_eq!(s.total, 5);
        assert_eq!(s.platform_var, 4);
        assert_eq!(s.resolvable(), 1);
        // Внутри разрешимого — 100%, по всем подряд — 20%.
        assert!((s.resolved_share_of_resolvable() - 1.0).abs() < 1e-9);
        assert!((s.resolved_share_raw() - 0.2).abs() < 1e-9);
    }
}
