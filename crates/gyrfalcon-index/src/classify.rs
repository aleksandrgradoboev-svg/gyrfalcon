//! Классификация модуля по его пути в выгрузке.
//!
//! Значения `category`, `module_type`, `object_name`, `form_name` совпадают
//! с прежний инструментскими один-в-один — иначе сверка «поимённо» превращается в сверку
//! с переводом. Взяты не по памяти, а из `select distinct` его живого индекса
//! (замер 28.08.2026).

/// Разбор пути модуля.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModuleInfo {
    /// Категория: `CommonModules`, `Catalogs`, `Documents`, …
    pub category: Option<String>,
    /// Имя объекта метаданных, которому принадлежит модуль.
    pub object_name: Option<String>,
    /// Вид модуля: `Module`, `ManagerModule`, `ObjectModule`, …
    pub module_type: Option<String>,
    /// Имя формы, если модуль формы.
    pub form_name: Option<String>,
    pub is_form: bool,
}

/// Разобрать относительный путь модуля.
///
/// Понимает три раскладки, встречающиеся в выгрузке:
///
/// * `Catalogs/Имя/Ext/ObjectModule.bsl` — модуль объекта;
/// * `Catalogs/Имя/Forms/ФормаСписка/Ext/Form/Module.bsl` — модуль формы;
/// * `CommonModules/Имя/Ext/Module.bsl` — общий модуль.
///
/// Путь подаётся с прямыми слэшами. Неопознанный путь даёт пустую структуру,
/// а не выдуманную категорию: неизвестность здесь честнее догадки.
pub fn classify(rel_path: &str) -> ModuleInfo {
    let parts: Vec<&str> = rel_path.split('/').collect();
    let mut info = ModuleInfo::default();

    if parts.len() < 2 {
        return info;
    }
    info.category = Some(parts[0].to_string());
    info.object_name = Some(parts[1].to_string());

    // Модуль формы: .../Forms/<Имя>/Ext/Form/Module.bsl
    if let Some(pos) = parts.iter().position(|p| *p == "Forms") {
        if let Some(form) = parts.get(pos + 1) {
            info.is_form = true;
            info.form_name = Some((*form).to_string());
            info.module_type = Some("Module".to_string());
            return info;
        }
    }

    // Прочие: вид модуля — имя файла без расширения.
    if let Some(last) = parts.last() {
        info.module_type = Some(last.trim_end_matches(".bsl").to_string());
    }
    info
}

/// Общий модуль? Только они адресуются по имени как `ИмяМодуля.Метод`.
pub fn is_common_module(info: &ModuleInfo) -> bool {
    info.category.as_deref() == Some("CommonModules")
}

/// Менеджерский модуль объекта — цель вызова `Справочники.Х.Метод`.
pub fn is_manager_module(info: &ModuleInfo) -> bool {
    info.module_type.as_deref() == Some("ManagerModule")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn общий_модуль() {
        let i = classify("CommonModules/ОбщегоНазначения/Ext/Module.bsl");
        assert_eq!(i.category.as_deref(), Some("CommonModules"));
        assert_eq!(i.object_name.as_deref(), Some("ОбщегоНазначения"));
        assert_eq!(i.module_type.as_deref(), Some("Module"));
        assert!(!i.is_form);
        assert!(is_common_module(&i));
    }

    #[test]
    fn модуль_объекта() {
        let i = classify("Catalogs/Номенклатура/Ext/ObjectModule.bsl");
        assert_eq!(i.category.as_deref(), Some("Catalogs"));
        assert_eq!(i.object_name.as_deref(), Some("Номенклатура"));
        assert_eq!(i.module_type.as_deref(), Some("ObjectModule"));
        assert!(!is_common_module(&i));
    }

    #[test]
    fn менеджерский_модуль() {
        let i = classify("Documents/РеализацияТоваров/Ext/ManagerModule.bsl");
        assert!(is_manager_module(&i));
        assert_eq!(i.object_name.as_deref(), Some("РеализацияТоваров"));
    }

    #[test]
    fn модуль_формы() {
        // Раскладка проверена по живому индексу прежнего инструмента: форма даёт
        // module_type = "Module", is_form = 1, form_name = имя формы.
        let i = classify("AccountingRegisters/Хозрасчетный/Forms/ФормаСписка/Ext/Form/Module.bsl");
        assert!(i.is_form);
        assert_eq!(i.form_name.as_deref(), Some("ФормаСписка"));
        assert_eq!(i.module_type.as_deref(), Some("Module"));
        assert_eq!(i.object_name.as_deref(), Some("Хозрасчетный"));
    }

    #[test]
    fn набор_записей_регистра() {
        let i = classify("AccumulationRegisters/ТоварыНаСкладах/Ext/RecordSetModule.bsl");
        assert_eq!(i.module_type.as_deref(), Some("RecordSetModule"));
    }

    #[test]
    fn неопознанный_путь_не_выдумывает_категорию() {
        assert_eq!(classify("Module.bsl"), ModuleInfo::default());
    }
}
