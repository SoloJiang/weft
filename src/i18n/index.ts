import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import { STORAGE_KEYS } from "../lib/storageKeys";
import { en } from "./en";
import { zh } from "./zh";

export type Lang = "en" | "zh";

function initialLang(): Lang {
  const saved = localStorage.getItem(STORAGE_KEYS.lang);
  if (saved === "en" || saved === "zh") return saved;
  return navigator.language?.toLowerCase().startsWith("zh") ? "zh" : "en";
}

void i18n.use(initReactI18next).init({
  resources: { en: { translation: en }, zh: { translation: zh } },
  lng: initialLang(),
  fallbackLng: "en",
  interpolation: { escapeValue: false },
});

export function currentLang(): Lang {
  return i18n.language === "zh" ? "zh" : "en";
}

export function setLang(l: Lang) {
  localStorage.setItem(STORAGE_KEYS.lang, l);
  void i18n.changeLanguage(l);
}

export default i18n;
