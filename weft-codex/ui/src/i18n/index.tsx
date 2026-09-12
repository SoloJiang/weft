import * as React from "react"

import en from "./en"
import zh from "./zh"

export type Language = "en" | "zh"
export type MessageKey = keyof typeof en
export type MessageValues = Record<string, string | number>

interface I18nValue {
  lang: Language
  t: (key: MessageKey, values?: MessageValues) => string
}

const dictionaries = { en, zh }
const I18nContext = React.createContext<I18nValue | null>(null)

export function languageFromLocale(locale: string): Language {
  return locale.toLowerCase().startsWith("zh") ? "zh" : "en"
}

export function I18nProvider({
  lang,
  children,
}: {
  lang: Language
  children: React.ReactNode
}) {
  const value = React.useMemo<I18nValue>(() => {
    const t = (key: MessageKey, values: MessageValues = {}) => {
      let text: string = dictionaries[lang][key] ?? dictionaries.en[key]
      for (const [name, replacement] of Object.entries(values)) {
        text = text.replaceAll(`{${name}}`, String(replacement))
      }
      return text
    }
    return { lang, t }
  }, [lang])

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>
}

export function useI18n(): I18nValue {
  const value = React.useContext(I18nContext)
  if (!value) {
    throw new Error("useI18n must be used inside I18nProvider")
  }
  return value
}
