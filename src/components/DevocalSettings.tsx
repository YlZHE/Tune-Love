import { useState } from "react";
import { Button } from "@radix-ui/themes";
import { UserSound } from "@phosphor-icons/react";
import { useDevocal } from "../useDevocal";
import { ModelSection } from "./ModelSection";
import "./DevocalSettings.css";

export function DevocalSettings() {
  const { status, release } = useDevocal();
  const [error, setError] = useState("");
  return <section className="devocal-settings" aria-labelledby="devocal-settings-title">
    <div className="color-heading"><UserSound size={21} aria-hidden="true" />
      <div><h2 id="devocal-settings-title">去人声</h2>
        <p>开启去人声后，本应用会接管当前播放器的声音输出。音量合成器里播放器那一栏接近 0 是正常的，声音由本应用发出；要调音量请调本应用。点‘释放播放器’可立即交还。</p></div>
    </div>
    <ModelSection autoEnablePending={false} onAutoEnableConsumed={() => {}} focusSeq={0} />
    <div className="devocal-settings-actions">
      <Button variant="soft" color="gray" disabled={!status.held}
        onClick={() => { setError(""); release().catch(() => setError("未能释放播放器，请重试")); }}>释放播放器</Button>
    </div>
    {error && <p role="alert" className="color-error">{error}</p>}
  </section>;
}
