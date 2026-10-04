import { AlertDialog as AlertDialogPrimitive } from "radix-ui";
import { AlertDialog, Button } from "@radix-ui/themes";
import type { ModelInfo } from "../modelDownload";
import "./ModelSection.css";

// Asked before a quality model (bytesep, HTDemucs) is selected: it adds latency that the
// lyrics in the player do not account for. `model === null` keeps the box closed; the
// selection changes only on 切换.
export function QualityTierDialog({ model, latencyMs, onConfirm, onCancel }: {
  model: ModelInfo | null;
  latencyMs: number;
  onConfirm(): void;
  onCancel(): void;
}) {
  return <AlertDialog.Root open={model !== null} onOpenChange={open => { if (!open) onCancel(); }}>
    <AlertDialog.Content maxWidth="420px" className="model-consent">
      <AlertDialog.Title>切换到高质量档</AlertDialog.Title>
      <AlertDialogPrimitive.Description asChild><div className="model-consent-body">
        <p>附加延迟约 {Math.round(latencyMs)} ms，歌词可能与播放不同步。</p>
        {model?.license.weights.includes("训练数据来源不明") && <p>训练数据来源不明，仅限非商业使用。</p>}
      </div></AlertDialogPrimitive.Description>
      <div className="model-consent-actions">
        <AlertDialog.Cancel><Button variant="soft" color="gray">取消</Button></AlertDialog.Cancel>
        <AlertDialog.Action><Button onClick={onConfirm}>切换</Button></AlertDialog.Action>
      </div>
    </AlertDialog.Content>
  </AlertDialog.Root>;
}
