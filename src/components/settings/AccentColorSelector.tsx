import React, { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "@/hooks/useSettings";
import {
  ACCENT_OPTIONS,
  applyAccent,
  DEFAULT_ACCENT_CUSTOM_COLOR,
  supportsSystemAccent,
} from "@/lib/utils/accent";
import type { AccentColor } from "@/bindings";

interface AccentColorSelectorProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const AccentColorSelector: React.FC<AccentColorSelectorProps> =
  React.memo(({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { settings, updateSetting } = useSettings();
    const colorInputRef = useRef<HTMLInputElement>(null);

    const currentAccent: AccentColor = settings?.accent_color ?? "pink";
    const customColor =
      settings?.accent_custom_color ?? DEFAULT_ACCENT_CUSTOM_COLOR;

    const accentOptions = ACCENT_OPTIONS.filter(
      (value) => value !== "system" || supportsSystemAccent(),
    ).map((value) => ({
      value,
      label: t(`accent.options.${value}`),
    }));

    const handleAccentChange = (value: string) => {
      const accent = value as AccentColor;
      applyAccent(accent, customColor);
      updateSetting("accent_color", accent);
    };

    // React's onChange fires on every `input` while the picker is dragged, so
    // that only previews; the native `change` fires once the pick is done.
    useEffect(() => {
      const input = colorInputRef.current;
      if (!input) return;
      const persist = () => updateSetting("accent_custom_color", input.value);
      input.addEventListener("change", persist);
      return () => input.removeEventListener("change", persist);
    }, [currentAccent, updateSetting]);

    return (
      <SettingContainer
        title={t("accent.title")}
        description={t("accent.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="flex items-center gap-2">
          {currentAccent === "custom" && (
            <input
              ref={colorInputRef}
              type="color"
              aria-label={t("accent.customLabel")}
              defaultValue={customColor}
              onChange={(e) => applyAccent("custom", e.target.value)}
              className="h-8 w-10 shrink-0 cursor-pointer rounded-md border border-line-strong bg-transparent p-0.5"
            />
          )}
          <Dropdown
            options={accentOptions}
            selectedValue={currentAccent}
            onSelect={handleAccentChange}
          />
        </div>
      </SettingContainer>
    );
  });

AccentColorSelector.displayName = "AccentColorSelector";
