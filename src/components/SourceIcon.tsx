import { useState } from "react";
import { Desktop } from "@phosphor-icons/react";

export function SourceIcon({ dataUrl }: { dataUrl?: string | null }) {
  const [failed, setFailed] = useState<string | null>(null);
  return dataUrl?.startsWith("data:image/png;base64,") && dataUrl !== failed
    ? <img className="source-icon" src={dataUrl} alt="" aria-hidden="true" draggable={false}
        onError={() => setFailed(dataUrl)} />
    : <Desktop size={14} aria-hidden="true" />;
}
