# Synology 原生套件

Keeps 在 NAS 原生运行，这是唯一受支持的 NAS 服务部署方式。Docker 不用于部署 NAS 服务；仓库中的 Docker 流程只构建 SPK 运行时、运行隔离测试或执行明确安排的一次性 Linux worker。旧 NAS Docker Compose 方案已退役，不得重新启用。当前支持 DS1520+ 的 geminilake 架构及 DSM 7；尚未取得 Synology 官方套件审核。

## 正式发布与安装

正式 NAS 只安装 **当前 `origin/main`** 的 GitHub Actions SPK。发布变更通过 Draft PR 提交，由 `hechuan9` 批准最终提交、CI 通过后合并。套件升级时在 PR 中递增 `package/INFO` 内的套件版本号。合并后等待该 main 提交的 CI 通过，再手动打包：

```sh
gh workflow run release-nas.yml --repo ModernsDev/keeps --ref main
```

等待 **Release NAS SPK** 成功，在上传安装前下载并校验当前 main 的产物：

```sh
python3 deploy/spk/release.py fetch .build/keeps-nas-release
```

下载器固定从 `ModernsDev/keeps` 获取同一提交的成功发布产物，核对来源和 SHA-256。`source-revision` 记录 Git 提交，并随 SPK 保存。main 更新后需为新提交重新打包、下载。仓库的 `export_runtime.py` 和 `pack.py` 可继续用于隔离开发测试。

把下载目录上传到 NAS。备份完成、安装前，在发布端重新运行 `python3 deploy/spk/release.py check "$(cat .build/keeps-nas-release/source-revision)"`，并在 NAS 下载目录运行 `sha256sum -c SHA256SUMS`，再按下方升级流程执行 `synopkg install <绝对路径.spk>`。使用 `synopkg start KeepsNativeProbe` / `synopkg stop KeepsNativeProbe` 启停，使用 `sudo synopkg status KeepsNativeProbe` 查询状态。套件以 DSM 专用账号运行，使用 `Nice=10` 和 best-effort I/O 优先级 7。媒体子进程的资源上限由服务配置控制。

运行时使用私有动态加载器，不全局设置 `LD_LIBRARY_PATH`。导出器针对 Debian trixie、Perl 5.40、ImageMagick 7 的布局；随包保留依赖版权资料。

## 配置

配置位于 `/var/packages/KeepsNativeProbe/var/`，由套件账号持有，权限 0600；升级保留已有配置。

- `original-root`：原片目录的绝对路径。
- `server-url`：客户端可达的服务地址，也用于签名预览链接。
- `access-token`：随机访问令牌，不提交 Git。
- `server-overrides`：启动脚本白名单内的逐行 `KEY=value`，不执行 shell 文本。

正式资料库为 `local-library`，客户端入口及 `server-url` 使用 `https://keeps.hechuannas.synology.me`；局域网直连地址为 `http://192.168.0.50:2283`，原片 `/volume2/photo`，状态 `/volume2/@appdata/KeepsNativeProbe/production-state`。`server-url` 决定在线图片及手机离线数据库中的签名下载地址，不能配置为仅局域网可达的地址。修改后需要重启服务，并刷新手机图库副本中的旧链接；仅修改手机 API 地址不会改写已保存的图片链接。新安装的原型默认使用独立测试共享目录、`spk-probe` 和 2285，不能将其当作正式配置。

日常本地编码开启（`KEEPS_LOCAL_CACHE_ENCODING_ENABLED=1`），远端派发关闭（`KEEPS_REMOTE_WORKER_ENABLED=0`）。Linux 仅用于显式安排的[一次性批量处理](../linux-worker/README.md)。

## 升级与恢复

1. 暂停独立维护进程，停止服务；使用 SQLite backup API 一致备份状态目录的 `db/control_plane.sqlite`、`db/jobs.sqlite`，检查备份，并保留当前套件与配置。
2. 安装新包，保留 library ID、token、原片和状态路径。禁止同时启动两个写入同一状态目录的服务。 若 schema 升级，安装后保持停服，以套件账号设置当前 KEEPS_ROOT 并执行 target/bin/keeps-server migrate；确认迁移成功后再启动，不依赖生产配置自动迁移。
3. 启动后核对 `/healthz`、鉴权、资产计数、缩略图下载及后台状态，再恢复维护进程。可用性与全库处理完成是两回事。
4. 回退前另存最新数据库。使用与数据库 schema 匹配的套件和配置；恢复旧数据库会丢失备份之后的整理修改，不能直接覆盖最新状态。

**schema 28 目录照片计数**

从 schema 27 升级时，按上述停服和双库备份流程运行 `keeps-server migrate`。迁移在一个 SQLite 事务内从已登记媒体回填目录照片数。仍有待安装目录清单的库，按下一节补齐清单。

**schema 27 目录迁移**

停服前完成已开始的原片发布、移动和回收。停服并备份两库后，为每个资料库盘点其追踪根（含停用的根，多个根重复传入 `--root`）。此示例适用于追踪整个原片根的单图库。

```sh
python3 scripts/audit_photo_placement.py --catalog "$KEEPS_ROOT/db/control_plane.sqlite" --library "$KEEPS_LIBRARY_ID" --root "$ORIGINAL_ROOT" --directory-manifest > inventory.json
target/bin/keeps-server migrate inventory.json
```

迁移需要套件账号的 `KEEPS_ROOT` 与 `ORIGINAL_ROOT`。先处理所有报告差异，再继续停服迁移。清单含空目录，媒体摘要与照片身份 XMP 均须匹配。多图库将各清单依次传给 `migrate`。迁移完成后，启用写入前可恢复同一时点的两库备份并恢复匹配版本的套件。启用写入后的回退须先核对迁移后已完成的文件操作。

长期备份使用 Hyper Backup 的应用备份，选择 `Keeps Native Probe`。备份目标、计划和版本保留由 Hyper Backup 任务管理；套件通过 `scripts/backup` 导出双库、四个配置文件及存在时的 `maintenance` 状态，不包含原片或可重建预览。原片共享文件夹须单独纳入任务。备份期间需要停服以保证双库一致；停止时先通知主进程，在当前步骤完成并保存断点后退出，重启自动继续未完成的精确照片任务，导出会校验 SQLite 完整性并生成校验和；临时目录应位于数据卷，不能使用空间有限的 DSM 系统分区。

当前 NAS 使用独立的 `Keeps` Hyper Backup 任务，目标为 S3 `chuan.backup/keeps-native`，每天 NAS 时间（America/Los_Angeles）01:00 运行，智能保留最多 256 个版本。原片由原有 `Amazon S3` 任务备份；两者分开调度，避免应用状态备份排在大批照片传输之后。

从 DSM 回收站把照片恢复到原历史路径后，保持 Keeps 停服并执行 `target/bin/keeps-server register-restored "$KEEPS_LIBRARY_ID" /volume2/photo/相对目录/原文件名.jpg`。可以一次传入多个明确的原片路径。已有版本摘要和相邻 XMP 的照片身份都匹配后才登记；已经清除历史记录的照片需通过 Keeps 导入。

恢复只写 Keeps 状态和配置，保留当前 `KEEPS_ROOT`，恢复前状态保存在该目录的 `backups/restore-before-*`。恢复后须核对服务健康、资料库及整理数据。尚未完成 Hyper Backup 备份和恢复验证前，不删除旧数据库备份。

`/volume2/docker/keeps/releases/` 是历史部署目录，不作为长期备份位置；其中还混有 RAW、照片和修复前副本，禁止整目录删除。旧 Docker 服务 `keeps-control-plane` 已退役并保持停止；它保存的是迁移时点的旧状态，不能启动、恢复或替代当前服务。NAS 服务只能通过本说明中的 SPK 流程部署。原片始终保留原位，不参与套件清理。

接口线程保持正常 CPU/I/O 优先级，照片处理、编码、导入发布、移动、回收及离线导出在线程级使用 `Nice=10` 和 best-effort I/O 优先级 7，避免整个套件降级拖慢元数据同步。
