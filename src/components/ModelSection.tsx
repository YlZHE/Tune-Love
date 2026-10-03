import { useEffect, useRef, useState } from "react";
import { Button, Progress } from "@radix-ui/themes";
import { useModelDownload, type ModelAction } from "../useModelDownload";
import {
  MODEL_MANIFEST, STEMGENRT_ID, approxSize, formatMiB, hasModelConsent, isValidMirrorPrefix, modelErrorText, modelTotalBytes,
  progressPercent, progressText, readMirrorPrefix, rememberModelConsent, type ModelInfo, type ModelStatus,
} from "../modelDownload";
import { ModelConsentDialog } from "./ModelConsentDialog";
import "./ModelSection.css";

type Send = ReturnType<typeof useModelDownload>["send"];

// The StemgenRT row is the one the main window's missing-model hint points at. While
// `autoEnablePending` holds, its downloads and imports ask the backend to turn de-vocal on after
// installing; the request is used up only once one of them has actually started (or found the
// model installed). A non-zero `focusSeq` change focuses the row's first usable button.
export function ModelSection({ autoEnablePending, onAutoEnableConsumed, focusSeq }: {
  autoEnablePending: boolean; onAutoEnableConsumed(): void; focusSeq: number;
}) {
  const { statuses, send } = useModelDownload();
  return <div className="model-section">
    {MODEL_MANIFEST.models.map(model => {
      const target = model.id === STEMGENRT_ID;
      return <ModelRow key={model.id} model={model} status={statuses?.find(s => s.id === model.id) ?? null} send={send}
        autoEnable={target && autoEnablePending} onAutoEnableConsumed={onAutoEnableConsumed} focusSeq={target ? focusSeq : 0} />;
    })}
  </div>;
}

function ModelRow({ model, status, send, autoEnable, onAutoEnableConsumed, focusSeq }: {
  model: ModelInfo; status: ModelStatus | null; send: Send;
  autoEnable: boolean; onAutoEnableConsumed(): void; focusSeq: number;
}) {
  // A rejected command is shown only while the row is still in the phase it was raised in:
  // a phase change brings fresher information than the rejection.
  const [rejection, setRejection] = useState<{ text: string; phase: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const phase = status?.phase ?? "unknown";
  // Dropped for good on a phase change, so it cannot come back if the row returns to that phase.
  useEffect(() => setRejection(null), [phase]);
  const actionError = rejection && rejection.phase === phase ? rejection.text : "";

  // One command at a time per row, so a fast double-click cannot come back as already_running.
  // A pending auto-enable rides on every download or import until one has started: a rejected
  // command, or a reply still showing the row missing or failed (a closed file picker), keeps it.
  const run = (action: ModelAction, options?: { mirrorPrefix?: string }) => {
    if (busy) return;
    setRejection(null);
    setBusy(true);
    const withAutoEnable = autoEnable && (action === "download" || action === "import");
    send(model.id, action, { ...options, autoEnable: withAutoEnable })
      .then(next => {
        const after = next?.find(s => s.id === model.id)?.phase;
        if (withAutoEnable && (after === "downloading" || after === "verifying" || after === "installed")) onAutoEnableConsumed();
      })
      .catch((e: unknown) => setRejection({ text: e instanceof Error ? e.message : modelErrorText(String(e)), phase }))
      .finally(() => setBusy(false));
  };

  // Focus waits for the first status: until then the row has no buttons.
  const row = useRef<HTMLDivElement>(null);
  const focusWanted = useRef(false);
  useEffect(() => { if (focusSeq) focusWanted.current = true; }, [focusSeq]);
  useEffect(() => {
    if (!focusWanted.current || !status) return;
    focusWanted.current = false;
    row.current?.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
  });
  // The consent box is open while this holds the prefix to list in it (null: closed).
  const [consent, setConsent] = useState<{ mirrorPrefix: string } | null>(null);
  // Only a non-empty, valid prefix is ever sent or listed.
  const savedPrefix = () => { const p = readMirrorPrefix(); return p && isValidMirrorPrefix(p) ? p : ""; };
  const sendDownload = (mirrorPrefix: string) => run("download", { mirrorPrefix });
  const showConsent = () => setConsent({ mirrorPrefix: savedPrefix() });
  // Download, resume and retry all go through here: with consent already given for this
  // model's hashes the request goes out, otherwise the box opens first.
  const startDownload = () => {
    if (busy) return;
    if (hasModelConsent(model)) sendDownload(savedPrefix()); else showConsent();
  };
  // run() takes the row's busy lock in the same tick, so there is no gap between agreeing and sending.
  // The prefix sent is the one the box disclosed when it opened, not a fresh read.
  const acceptConsent = () => { if (!consent) return; rememberModelConsent(model); setConsent(null); sendDownload(consent.mirrorPrefix); };

  const total = status && status.totalBytes > 0 ? status.totalBytes : modelTotalBytes(model);
  const partial = status ? `已下载 ${formatMiB(status.receivedBytes)} / ${formatMiB(total)} MB` : "";
  const hint = status?.sourceError && (status.phase === "downloading" || status.phase === "failed")
    ? <p className="model-row-note">{modelErrorText(status.sourceError)}</p> : null;
  // A rejected command replaces the status error until the next action.
  const failure = status?.phase === "failed" ? (actionError || (status.error ? modelErrorText(status.error) : "下载失败，请重试。")) : actionError;

  const importButton = <Button variant="ghost" color="gray" size="1" disabled={busy} onClick={() => run("import")}>从本地文件导入…</Button>;

  let body;
  if (!status) {
    body = <p className="model-row-note">正在读取模型状态…</p>;
  } else if (status.phase === "missing" && status.receivedBytes === 0) {
    const weights = model.license.weights;
    body = <>
      <div className="model-row-actions">
        <Button variant="soft" color="gray" disabled={busy} onClick={startDownload}>下载模型（{approxSize(modelTotalBytes(model))}）</Button>
        {importButton}
      </div>
      <p className="model-row-note">
        {weights === "pending" ? "许可待作者确认 · " : `权重许可：${weights} · `}
        <Button variant="ghost" color="gray" size="1" disabled={busy} onClick={showConsent}>来源与许可</Button>
      </p>
    </>;
  } else if (status.phase === "missing") {
    body = <>
      <div className="model-row-actions">
        <Button variant="soft" color="gray" disabled={busy} onClick={startDownload}>继续下载</Button>
        <Button variant="ghost" color="gray" size="1" disabled={busy} onClick={() => run("delete")}>删除已下载部分</Button>
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
        <Button variant="soft" color="gray" disabled={busy} onClick={() => run("cancel")}>取消</Button>
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
        <Button variant="soft" color="gray" disabled={busy} onClick={() => run("delete")}>删除模型</Button>
      </div>}
    </>;
  } else {
    body = <>
      <p role="alert" className="color-error">{failure}</p>
      {hint}
      <div className="model-row-actions">
        <Button variant="soft" color="gray" disabled={busy} onClick={startDownload}>重试</Button>
        {importButton}
      </div>
    </>;
  }

  // Only where a download or import can still be started.
  const autoEnableNote = autoEnable && (status?.phase === "missing" || status?.phase === "failed")
    ? <p className="model-row-note">下载完成后将自动开启去人声</p> : null;

  return <div ref={row} role="group" aria-label={model.name} className="model-row">
    <h3>{model.name}</h3>
    {body}
    {autoEnableNote}
    {actionError && status?.phase !== "failed" && <p role="alert" className="color-error">{actionError}</p>}
    <ModelConsentDialog model={consent ? model : null} mirrorPrefix={consent?.mirrorPrefix ?? ""}
      onAccept={acceptConsent} onCancel={() => setConsent(null)} />
  </div>;
}
