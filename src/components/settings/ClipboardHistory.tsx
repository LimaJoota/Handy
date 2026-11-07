import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface ClipboardHistoryProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const ClipboardHistory: React.FC<ClipboardHistoryProps> = React.memo(
  ({ descriptionMode = "tooltip", grouped = false }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating } = useSettings();

    const enabled = getSetting("clipboard_history") ?? false;

    return (
      <ToggleSwitch
        checked={enabled}
        onChange={(enabled) => updateSetting("clipboard_history", enabled)}
        isUpdating={isUpdating("clipboard_history")}
        label={t("settings.advanced.clipboardHistory.label")}
        description={t("settings.advanced.clipboardHistory.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  },
);
