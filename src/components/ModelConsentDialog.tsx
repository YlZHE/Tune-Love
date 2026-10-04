import { AlertDialog as AlertDialogPrimitive } from "radix-ui";
import { AlertDialog, Button } from "@radix-ui/themes";
import { MODEL_MANIFEST, formatMiB, modelTotalBytes, pinnedCommit, type ModelInfo } from "../modelDownload";
import "./ModelSection.css";

// "https://ghfast.top/" -> "ghfast.top"; anything unparsable is shown as written.
function hostOf(url: string): string {
  try { return new URL(url).host; } catch { return url; }
}

// Shown before a model is downloaded: where it comes from, its size and hashes, the licence
// position and which third parties may see the request. Every value comes from the manifest
// (or from the prefix the user typed); `model === null` keeps the box closed.
export function ModelConsentDialog({ model, mirrorPrefix, onAccept, onCancel }: {
  model: ModelInfo | null;
  mirrorPrefix: string;
  onAccept(): void;
  onCancel(): void;
}) {
  const commit = model ? pinnedCommit(model.files[0]?.origin ?? "") : null;
  const mirrorHosts = MODEL_MANIFEST.mirrors.map(hostOf).join("、");
  return <AlertDialog.Root open={model !== null} onOpenChange={open => { if (!open) onCancel(); }}>
    <AlertDialog.Content maxWidth="520px" className="model-consent">
      <AlertDialog.Title>下载去人声模型</AlertDialog.Title>
      {model && <AlertDialogPrimitive.Description asChild><div className="model-consent-body">
        <p>模型：{model.name}。{model.tier === "quality"
          ? `高质量去人声，有附加延迟，歌词可能与播放不同步${model.devices.cpu ? "" : "；仅支持 GPU"}`
          : "用于实时去人声"}；模型不随安装包发布，需要下载到本机。</p>
        <p>来源：{model.source}{commit && <>，固定提交 <code>{commit}</code></>}。</p>
        <div>
          <p>大小：{formatMiB(modelTotalBytes(model))} MB（{modelTotalBytes(model).toLocaleString("en-US")} 字节）</p>
          {model.files.map(f => <p key={f.file}>
            {model.files.length > 1 && <>{f.file}：{f.bytes.toLocaleString("en-US")} 字节<br /></>}
            SHA-256：<code>{f.sha256}</code>
          </p>)}
        </div>
        <p>许可：仓库代码采用 {model.license.code}；{model.license.weights === "pending"
          ? "模型权重的许可作者未单独声明，待确认。" : `模型权重采用 ${model.license.weights}。`}</p>
        {model.license.converted && <p>署名与修改：模型原作者见上方来源；已由本项目转换修改（转为 ONNX 格式并改写计算图，权重数值未改动）。{model.license.credit}</p>}
        <p>训练数据：{model.license.trainingData.join("、")}。本应用免费、非商业，请自行判断使用场景。</p>
        <p>加速：直连失败时，可能经由第三方 GitHub 加速服务下载（{mirrorHosts}{mirrorPrefix && `、你填写的 ${hostOf(mirrorPrefix)}`}），它们能看到这次下载请求；文件一律按上面的 SHA-256 校验，不符即删除。</p>
        <p>写入位置：<code>%LOCALAPPDATA%\io.github.ylzhe.tunelove\models\{model.id}\</code></p>
      </div></AlertDialogPrimitive.Description>}
      <div className="model-consent-actions">
        <AlertDialog.Cancel><Button variant="soft" color="gray">取消</Button></AlertDialog.Cancel>
        <AlertDialog.Action><Button onClick={onAccept}>同意并下载</Button></AlertDialog.Action>
      </div>
    </AlertDialog.Content>
  </AlertDialog.Root>;
}
