import { useState } from "react";
import { IconButton } from "@radix-ui/themes";
import { GearSix } from "@phosphor-icons/react";
import { AppTooltip } from "./AppTooltip";
import { openSettings } from "../openSettings";

export function SettingsButton({ onError }: { onError: (message: string) => void }) {
  const [opening, setOpening] = useState(false);
  async function open() {
    if (opening) return;
    setOpening(true);
    try { await openSettings(); } catch { onError("未能打开设置，请重试"); }
    finally { setOpening(false); }
  }
  return <AppTooltip content="设置" disabled={opening}>
    <IconButton size="1" variant="ghost" color="gray" radius="full"
      className="window-button" aria-label="设置" disabled={opening} onClick={() => void open()}>
      <GearSix size={16} />
    </IconButton>
  </AppTooltip>;
}
