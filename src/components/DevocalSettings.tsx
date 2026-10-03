import { useCallback, useEffect, useRef, useState } from "react";
import { Button, TextField } from "@radix-ui/themes";
import { UserSound } from "@phosphor-icons/react";
import { isValidMirrorPrefix, readMirrorPrefix, saveMirrorPrefix } from "../modelDownload";
import { useDevocal } from "../useDevocal";
import type { SettingsSectionRequest } from "../settingsSection";
import { ModelSection } from "./ModelSection";
import "./DevocalSettings.css";

// Optional prefix put in front of the GitHub address as an extra download source. Invalid
// text is flagged and never saved; an empty field clears the saved prefix.
function MirrorPrefixField() {
  const [value, setValue] = useState(readMirrorPrefix);
  const trimmed = value.trim();
  const invalid = trimmed !== "" && !isValidMirrorPrefix(trimmed);
  // Invalid text is never stored, and it also clears a previously saved prefix so the old one is not still sent.
  const commit = () => saveMirrorPrefix(invalid ? "" : trimmed);
  return <details className="model-advanced">
    <summary>高级</summary>
    <div className="model-advanced-field">
      <label htmlFor="model-mirror-prefix">下载加速前缀（可选）</label>
      <TextField.Root id="model-mirror-prefix" size="1" placeholder="https://example.com/" value={value}
        color={invalid ? "red" : undefined} aria-invalid={invalid}
        onChange={e => setValue(e.target.value)} onBlur={commit}
        onKeyDown={e => { if (e.key === "Enter") commit(); }} />
      {invalid && <p role="alert" className="color-error">须以 https:// 开头、以 / 结尾；未保存，不使用自定义前缀</p>}
    </div>
  </details>;
}

// `request` says how settings were last opened. From the main window's missing-model hint
// ("devocal-model") it scrolls here, focuses the model row and makes the row's downloads and
// imports (retries and resumes included) turn de-vocal on once the model is installed; that holds
// until the model is installed or the user cancels a download. A plain open of the already open
// window (section null) drops it, without scrolling or focusing.
export function DevocalSettings({ request = null }: { request?: SettingsSectionRequest | null }) {
  const { status, release } = useDevocal();
  const [error, setError] = useState("");
  const section = useRef<HTMLElement>(null);
  const [autoEnablePending, setAutoEnablePending] = useState(false);
  const [focusSeq, setFocusSeq] = useState(0);
  const consumeAutoEnable = useCallback(() => setAutoEnablePending(false), []);
  const seq = request?.seq ?? 0;
  const fromHint = request?.section === "devocal-model";
  useEffect(() => {
    if (!seq) return;
    setAutoEnablePending(fromHint);
    // A plain open also cancels a focus still waiting for the model status.
    if (!fromHint) { setFocusSeq(0); return; }
    section.current?.scrollIntoView({ block: "start" });
    setFocusSeq(seq);
  }, [seq, fromHint]);
  return <section ref={section} className="devocal-settings" aria-labelledby="devocal-settings-title">
    <div className="color-heading"><UserSound size={21} aria-hidden="true" />
      <div><h2 id="devocal-settings-title">去人声</h2>
        <p>开启去人声后，本应用会接管当前播放器的声音输出。音量合成器里播放器那一栏接近 0 是正常的，声音由本应用发出；要调音量请调本应用。点‘释放播放器’可立即交还。</p></div>
    </div>
    <ModelSection autoEnablePending={autoEnablePending} onAutoEnableConsumed={consumeAutoEnable} focusSeq={focusSeq} />
    <div className="devocal-settings-actions">
      <Button variant="soft" color="gray" disabled={!status.held}
        onClick={() => { setError(""); release().catch(() => setError("未能释放播放器，请重试")); }}>释放播放器</Button>
    </div>
    {error && <p role="alert" className="color-error">{error}</p>}
    <MirrorPrefixField />
  </section>;
}
