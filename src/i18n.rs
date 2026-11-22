use serde::{Deserialize, Serialize};

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
  fn GetUserDefaultUILanguage() -> u16;
}

/// Unterstuetzte Oberflaechensprachen (live zur Laufzeit umschaltbar).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppLanguage {
  #[serde(alias = "de", alias = "DE", alias = "Deutsch")]
  De,
  #[serde(alias = "en", alias = "EN", alias = "English")]
  En,
}

impl Default for AppLanguage {
  fn default() -> Self {
    detect_system_language()
  }
}

impl AppLanguage {
  pub const ALL: [Self; 2] = [Self::De, Self::En];

  #[inline]
  pub fn code(&self) -> &'static str {
    match self {
      Self::De => "DE",
      Self::En => "EN",
    }
  }

  #[inline]
  pub fn display_name(&self) -> &'static str {
    match self {
      Self::De => "Deutsch",
      Self::En => "English",
    }
  }
}

/// Erkennt beim ersten Start automatisch die Windows-Anzeigesprache via `kernel32!GetUserDefaultUILanguage`.
/// Primaere Sprach-ID `0x07` (`LANG_GERMAN`) -> `AppLanguage::De`, alle anderen -> `AppLanguage::En`.
pub fn detect_system_language() -> AppLanguage {
  #[cfg(target_os = "windows")]
  {
    // SAFETY: `GetUserDefaultUILanguage` besitzt keine Parameter oder Vorbedingungen
    // und gibt lediglich die numerische Win32-LANGID des aktuellen Benutzers zurueck.
    let lang_id = unsafe { GetUserDefaultUILanguage() };
    if (lang_id & 0x03ff) == 0x07 {
      AppLanguage::De
    } else {
      AppLanguage::En
    }
  }
  #[cfg(not(target_os = "windows"))]
  {
    AppLanguage::En
  }
}

/// Waehlt verzweigungsarm zur Laufzeit zwischen deutschem und englischem UI-Text.
#[inline]
pub fn tr<'a>(lang: AppLanguage, de: &'a str, en: &'a str) -> &'a str {
  match lang {
    AppLanguage::De => de,
    AppLanguage::En => en,
  }
}
