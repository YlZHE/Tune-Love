# 发版检查清单

- [ ] 实测模型下载来源：运行 `node scripts/check-model-mirrors.mjs`。
  - 不合格的镜像从 `src-tauri/models.json` 的 `mirrors` 中删除；
  - 新增的镜像必须先通过这一检查，并且不得收录会返回网页或被安全软件拦截的服务（2026-10-03 已排除 gh-proxy.com 与 gh-proxy.net）；
  - 结果（日期、各来源结论）记入该版本的发布记录。
