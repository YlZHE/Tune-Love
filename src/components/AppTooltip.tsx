import WarmTooltip, { type WarmTooltipProps } from "./react-bits/WarmTooltip";

export { WarmTooltipGroup } from "./react-bits/WarmTooltip";
export type { WarmTooltipGroupHandle } from "./react-bits/WarmTooltip";

// Keep the official shared animations; centralize this app's dark palette,
// arrow-free appearance and wrapping for full song / artist / source labels.
export function AppTooltip({ content, ...props }: WarmTooltipProps) {
  return <WarmTooltip side="bottom" size="sm" gap={7}
    surfaceColor="#0a0a0a" inkColor="#f0f1f6" arrow={false}
    content={<span className="app-tooltip-label">{content}</span>} {...props} />;
}
