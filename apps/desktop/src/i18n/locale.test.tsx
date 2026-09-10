import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import {
  LOCALE_STORAGE_KEY,
  LANGUAGES,
  messages,
  type Locale,
  LocaleProvider,
  useLocale,
} from "./locale";

function LocaleProbe() {
  const { locale, setLocale, t } = useLocale();
  return (
    <label>
      {t("locale.label")}
      <select
        aria-label={t("locale.label")}
        value={locale}
        onChange={(event) => setLocale(event.target.value as typeof locale)}
      >
        {LANGUAGES.map((language) => <option key={language.value} value={language.value}>{language.label}</option>)}
      </select>
    </label>
  );
}

describe("LocaleProvider", () => {
  it.each(LANGUAGES)("persists and restores $value", async ({ value }) => {
    const first = render(<LocaleProvider><LocaleProbe /></LocaleProvider>);
    fireEvent.change(screen.getByRole("combobox"), { target: { value } });
    await waitFor(() => expect(document.documentElement.lang).toBe(value));
    expect(window.localStorage.getItem(LOCALE_STORAGE_KEY)).toBe(value);
    first.unmount();
    render(<LocaleProvider><LocaleProbe /></LocaleProvider>);
    expect(screen.getByRole("combobox")).toHaveValue(value);
    expect(screen.getByText(messages[value]["locale.label"])).toBeInTheDocument();
  });

  it("has complete catalogs with matching interpolation placeholders", () => {
    const base = messages["en-US"];
    for (const locale of Object.keys(messages) as Locale[]) {
      const catalog = messages[locale];
      expect(Object.keys(catalog).sort()).toEqual(Object.keys(base).sort());
      for (const key of Object.keys(base) as (keyof typeof base)[]) {
        expect(catalog[key].trim(), `${locale}: ${key}`).not.toBe("");
        expect(catalog[key].match(/{{[^}]+}}/g) ?? [], `${locale}: ${key}`).toEqual(base[key].match(/{{[^}]+}}/g) ?? []);
      }
    }
  });

  it("defaults to zh-CN, persists English, and restores the preference after remount", async () => {
    const first = render(
      <LocaleProvider>
        <LocaleProbe />
      </LocaleProvider>,
    );

    const chineseSelect = screen.getByRole("combobox", { name: "界面语言" });
    expect(chineseSelect).toHaveValue("zh-CN");
    await waitFor(() => expect(document.documentElement).toHaveAttribute("lang", "zh-CN"));

    fireEvent.change(chineseSelect, { target: { value: "en-US" } });
    await waitFor(() => expect(document.documentElement).toHaveAttribute("lang", "en-US"));
    expect(window.localStorage.getItem(LOCALE_STORAGE_KEY)).toBe("en-US");

    first.unmount();
    render(
      <LocaleProvider>
        <LocaleProbe />
      </LocaleProvider>,
    );
    const englishSelect = screen.getByRole("combobox", { name: "Language" });
    expect(englishSelect).toHaveValue("en-US");

    fireEvent.change(englishSelect, { target: { value: "zh-CN" } });
    await waitFor(() => expect(document.documentElement).toHaveAttribute("lang", "zh-CN"));
    expect(window.localStorage.getItem(LOCALE_STORAGE_KEY)).toBe("zh-CN");
  });
});
