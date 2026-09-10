import jaJP from "./messages/ja-JP.json";
import koKR from "./messages/ko-KR.json";
import deDE from "./messages/de-DE.json";
import frFR from "./messages/fr-FR.json";
import zhCN from "./messages/zh-CN.json";
import enUS from "./messages/en-US.json";
import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";

export const LANGUAGES = [
  { value: "zh-CN", label: "简体中文" },
  { value: "en-US", label: "English" },
  { value: "ja-JP", label: "日本語" },
  { value: "ko-KR", label: "한국어" },
  { value: "de-DE", label: "Deutsch" },
  { value: "fr-FR", label: "Français" },
] as const;
export type Locale = typeof LANGUAGES[number]["value"];

const LOCALE_STORAGE_KEY = "webcodex.desktop.locale";

export type MessageKey = keyof typeof zhCN;

export const messages: Record<Locale, Record<MessageKey, string>> = {
  "zh-CN": zhCN,
  "en-US": enUS,
  "ja-JP": jaJP,
  "ko-KR": koKR,
  "de-DE": deDE,
  "fr-FR": frFR,
};

type Params = Record<string, string | number>;

type LocaleContextValue = {
  locale: Locale;
  setLocale: (locale: Locale) => void;
  t: (key: MessageKey, params?: Params) => string;
  formatTime: (timestampMs: number) => string;
};

const LocaleContext = createContext<LocaleContextValue | null>(null);

function storedLocale(): Locale {
  try {
    const value = window.localStorage.getItem(LOCALE_STORAGE_KEY);
    return LANGUAGES.find((language) => language.value === value)?.value ?? "zh-CN";
  } catch {
    return "zh-CN";
  }
}

export function LocaleProvider({ children }: { children: ReactNode }) {
  const [locale, setLocaleState] = useState<Locale>(storedLocale);

  useEffect(() => {
    document.documentElement.lang = locale;
    try {
      window.localStorage.setItem(LOCALE_STORAGE_KEY, locale);
    } catch {
      // UI preference persistence is best-effort only.
    }
  }, [locale]);

  const value = useMemo<LocaleContextValue>(() => ({
    locale,
    setLocale: setLocaleState,
    t: (key, params) => {
      let result = messages[locale][key];
      if (params) {
        for (const [name, value] of Object.entries(params)) {
          result = result.replaceAll(`{{${name}}}`, String(value));
        }
      }
      return result;
    },
    formatTime: (timestampMs) => new Intl.DateTimeFormat(locale, {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    }).format(new Date(timestampMs)),
  }), [locale]);

  return <LocaleContext.Provider value={value}>{children}</LocaleContext.Provider>;
}

export function useLocale() {
  const value = useContext(LocaleContext);
  if (!value) throw new Error("useLocale must be used inside LocaleProvider");
  return value;
}

export { LOCALE_STORAGE_KEY };
