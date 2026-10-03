import { useState } from "react";
import { Button, Progress } from "@radix-ui/themes";
import { useModelDownload, type ModelAction } from "../useModelDownload";
import {
  MODEL_MANIFEST, approxSize, formatMiB, modelErrorText, modelTotalBytes, progressPercent, progressText,
  type ModelInfo, type ModelStatus,
} from "../modelDownload";
import "./ModelSection.css";

type Send = ReturnType<typeof useModelDownload>["send"];

// Props are wired by the settings page for the download-from-toggle flow; this row set only
// reads them once that flow exists.
export function ModelSection(_props: { autoEnablePending: boolean; onAutoEnableConsumed(): void; focusSeq: number }) {
  const { statuses, send } = useModelDownload();
  return <div className="model-section">
    {MODEL_MANIFEST.models.map(model =>
      <ModelRow key={model.id} model={model} status={statuses?.find(s => s.id === model.id) ?? null} send={send} />)}
  </div>;
}

function ModelRow({ model, status, send }: { model: ModelInfo; status: ModelStatus | null; send: Send }) {
  const [actionError, setActionError] = useState("");

  const run = (action: ModelAction) => {
    setActionError("");
    send(model.id, action).catch((e: unknown) => setActionError(e instanceof Error ? e.message : modelErrorText(String(e))));
  };
  // Download, resume and retry all go through here; the confirmation box and the
  // acceleration prefix are added in front of it later.
  const startDownload = () => run("download");

  const total = status && status.totalBytes > 0 ? status.totalBytes : modelTotalBytes(model);
  const partial = status ? `已下载 ${formatMiB(status.receivedBytes)} / ${formatMiB(total)} MB` : "";
  const hint = status?.sourceError && (status.phase === "downloading" || status.phase === "failed")
    ? <p className="model-row-note">{modelErrorText(status.sourceError)}</p> : null;
  // A rejected command replaces the status error until the next action.
  const failure = status?.phase === "failed" ? (actionError || modelErrorText(status.error ?? "")) : actionError;

  const importButton = <Button variant="ghost" color="gray" size="1" onClick={() => run("import")}>从本地文件导入…</Button>;

  let body;
  if (!status) {
    body = <p className="model-row-note">正在读取模型状态…</p>;
  } else if (status.phase === "missing" && status.receivedBytes === 0) {
    const weights = model.license.weights;
    body = <>
      <div className="model-row-actions">
        <Button variant="soft" color="gray" onClick={startDownload}>下载模型（{approxSize(modelTotalBytes(model))}）</Button>
        {importButton}
      </div>
      <p className="model-row-note">
        {weights === "pending" ? "许可待作者确认 · " : `权重许可：${weights}`}
        {weights === "pending" && <Button variant="ghost" color="gray" size="1">来源与许可</Button>}
      </p>
    </>;
  } else if (status.phase === "missing") {
    body = <>
      <div className="model-row-actions">
        <Button variant="soft" color="gray" onClick={startDownload}>继续下载</Button>
        <Button variant="ghost" color="gray" size="1" onClick={() => run("delete")}>删除已下载部分</Button>
        {importButton}
      </div>
      <p className="model-row-note">{partial}</p>
    </>;
  } else if (status.phase === "downloading") {
    body = <>
      <Progress value={progressPercent(status)} size="1" radius="full" aria-label={`${model.name}下载进度`} />
      <p className="model-row-note">{progressText(status)}</p>
      {hint}
      <div className="model-row-actions">
        <Button variant="soft" color="gray" onClick={() => run("cancel")}>取消</Button>
      </div>
    </>;
  } else if (status.phase === "verifying") {
    body = <p className="model-row-note">正在校验…</p>;
  } else if (status.phase === "installed") {
    const env = status.source === "env";
    body = <>
      <p className="model-row-ready">{env ? "模型已就绪（开发者环境变量）" : "模型已就绪"}</p>
      <details className="model-row-details">
        <summary>详细信息</summary>
        <dl>
          <dt>位置</dt><dd>{status.path ?? "未知"}</dd>
          <dt>大小</dt><dd>{modelTotalBytes(model).toLocaleString("en-US")} 字节</dd>
          <dt>SHA-256</dt><dd>{model.files.map(f => f.sha256).join(" ")}</dd>
        </dl>
      </details>
      {!env && <div className="model-row-actions">
        <Button variant="soft" color="gray" onClick={() => run("delete")}>删除模型</Button>
      </div>}
    </>;
  } else {
    body = <>
      <p role="alert" className="color-error">{failure}</p>
      {hint}
      <div className="model-row-actions">
        <Button variant="soft" color="gray" onClick={startDownload}>重试</Button>
        {importButton}
      </div>
    </>;
  }

  return <div role="group" aria-label={model.name} className="model-row">
    <h3>{model.name}</h3>
    {body}
    {actionError && status?.phase !== "failed" && <p role="alert" className="color-error">{actionError}</p>}
  </div>;
}
