import React from "react";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../hooks/useSettings";
import { Input } from "../ui/Input";
import { SettingContainer } from "../ui/SettingContainer";

interface ClipboardHistoryLimitProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const ClipboardHistoryLimit: React.FC<ClipboardHistoryLimitProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();

  const limit = getSetting("clipboard_history_limit") ?? 50;

  const handleChange = (event: React.ChangeEvent<HTMLInputElement>) => {
    const value = parseInt(event.target.value, 10);
    if (!isNaN(value) && value >= 1 && value <= 1000) {
      updateSetting("clipboard_history_limit", value);
    }
  };

  return (
    <SettingContainer
      title={t("settings.advanced.clipboardHistoryLimit.title")}
      description={t("settings.advanced.clipboardHistoryLimit.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
      layout="horizontal"
    >
      <div className="flex items-center space-x-2">
        <Input
          type="number"
          min="1"
          max="1000"
          value={limit}
          onChange={handleChange}
          disabled={isUpdating("clipboard_history_limit")}
          className="w-20"
        />
        <span className="text-sm text-text">
          {t("settings.debug.historyLimit.entries")}
        </span>
      </div>
    </SettingContainer>
  );
};
