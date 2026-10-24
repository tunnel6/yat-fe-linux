# YAT Linux Server 客户端

本目录 `yat-fe-linux` 提供面向 Linux Server 的独立 Rust CLI/服务，不是 `yat-fe` 的 Electron 桌面 Linux 构建。桌面版使用 `yat-fe` → Network NAPI/Agent；本服务使用自己的 `client_owner`、私有 socket 和 Docker 前台入口，两端仅共享底层 Network/Transport/WireGuard 库。

要求 **Linux 内核 ≥ 5.6**。WireGuard 从 5.6 起进入主线内核；部分发行版有 5.4 等旧版回移，但本项目统一以 5.6 为最低支持基线。两端共用的 native apply 在写入前强制检查，server 的 `start/serve` 还会在连接 Edge 前检查。`leave` 清理和只读诊断不受新建连接门禁限制。Docker 使用宿主机内核，容器镜像中安装 `wireguard-tools` 不能替代宿主机 WireGuard 模块或内建支持；还需要 `iproute2`、`wireguard-tools` 及目标网络命名空间的 `CAP_NET_ADMIN` 权限。

内核依据：[WireGuard 官方安装说明](https://www.wireguard.com/install/)（5.6+ 内核集成）。

推荐使用 Docker 前台运行。镜像默认直接执行 `yat-client serve`，客户端作为 PID 1 接收 Docker 的 `SIGTERM`，同时启动 Web 管理界面。默认在容器内监听 `0.0.0.0:9909`，可用 `YAT_LISTEN` 或 `serve --listen <地址>` 修改。Web 管理 API 当前没有鉴权，能够读取配置/状态、导入分享包、重连和修改地址。下面仅将管理端口发布到宿主机回环地址；远程管理可通过 SSH 端口转发访问。私有 Unix socket 的权限保护不适用于 HTTP 接口。

以下命令中的 `IMAGE` 表示包含当前代码构建结果的镜像。配置目录应使用同一个持久化卷；目录包含设备身份、证书、WireGuard 状态和本地命令通道。

```bash
# 首次导入只写入配置，不启动连接；share.json 只读挂载。
docker run --rm \
  --mount type=volume,src=yat-client-config,dst=/root/.config/yat-cli \
  --mount type=bind,src="$PWD/share.json",dst=/run/yat-share.json,readonly \
  IMAGE import /run/yat-share.json

# 前台运行，使用容器自身的网络命名空间。
docker run --name yat-client \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  --stop-timeout 60 \
  --publish 127.0.0.1:9909:9909 \
  --mount type=volume,src=yat-client-config,dst=/root/.config/yat-cli \
  IMAGE
```

需要更新配置时，将分享包复制到运行中的容器，然后执行 `docker exec yat-client yat-client import <容器内文件路径>`；操作完成后删除该分享包。`docker exec yat-client yat-client leave` 显式退出网络。普通容器停止保留网络成员资格，随后可以重新连接。

同一配置目录只允许一个客户端写入 native 配置。运行中的 `start/leave/import` 命令通过该目录下的 `yat-fe-linux.sock` 发送给现有所有者；`yat-fe-linux.lock` 是永久锁文件，运行中不得删除。锁被占用而 socket 不可用时，命令返回错误，不会自行创建另一个配置写入者。配置目录权限为 `0700`，socket 和锁文件为 `0600`。不要把配置卷暴露给无关容器。

`docker stop --time 60 yat-client` 先发送 `SIGTERM`。信号只通知当前所有者：正在执行的操作完成后，关闭命令入口、停止心跳并关闭 Edge 控制连接，再通过原 controller 排空和清理 native 资源，最后回收任务并退出。清理失败时保留所有者和锁并重试。60 秒是示例宽限期，不是清理耗时保证；宽限期结束后 Docker 可以强制终止进程。

正常退出和可处理的错误走同一关闭流程，不按 0–255 退出状态码区分是否清理。进程崩溃、`SIGKILL`、Docker 强杀或宿主机故障无法保证执行退出回调，由 Edge 的断连或超时检测判定离线。普通关闭使用控制连接断开，不发送改变成员资格的 Leave 请求。

不需要 Web 管理界面时，可用 `docker run ... IMAGE start` 覆盖默认命令，并省略管理端口发布。使用 host 网络时，默认的 `0.0.0.0:9909` 会直接监听宿主机所有地址；需要本机访问时显式设置 `YAT_LISTEN=127.0.0.1:9909`。

`yat-client status` 读取磁盘状态、applied state 及可用的 native stats，目前不通过 socket 查询运行中所有者的 Edge 连接；其 `Connected: no (daemon mode or stale)` 不能用于判定现有服务离线。Web `/api/status` 使用运行中所有者的状态；其中 `connected` 表示连接/本地初次应用状态，不等于对端业务路径已经双端验证。

当前实现接入 optional 网络路径协商的 **P1 纯响应端**：平台支持时声明 `wg.path-coordination.v1`，初次 native apply 后立即装配共享执行器；网络推送唤醒同一个 supervisor，获取当前认证 snapshot 后响应 READY、执行共同 COMMIT 并回报 APPLIED/REJECTED。事件突发合并，保留 5 秒恢复兜底；Linux 不发送 CHANGE_MODE、SWITCH_PATH 或 SYNC_MODE，也没有手动 override CLI/HTTP/Web 入口。桌面端的用户选择和 Auto 提议均可由 Linux 响应。

P1 的“只响应”包含实际路径执行：收到共同 COMMIT 后迁移对端 host AllowedIPs 归属、设置 endpoint，调用 Linux 原生 WireGuard 应用并回读验证，通过后才回报 APPLIED。P2 仅增加只读状态展示。

Web 已接入最小真实路径展示：Local Path 为本地实际配置的 Direct/Relay/Unknown，Path Endpoint 来自实际承载目标虚拟 IP 的 WG peer，Path Handshake 也取同一 peer（Relay 时为 relay）的握手。按原生 AllowedIPs 最长前缀匹配，支持 IPv4/IPv6 和固定 Relay 子网；读取失败、缺路由或 owner 不明确时清空旧值，显示 Unknown / `—`，不再用 observed/LAN 候选地址代替。原 supervisor 每 5 秒兜底读取本地 runtime，无新增 Edge 请求。该展示不表示双方共同确认或业务可达，不包含选择原因和协商进度。

普通停止和重连保留已保存的 pair 策略；明确 Leave/网络删除才清理。Edge 丢失状态时，由主动端恢复策略；如果只有 Linux 保有旧策略，新计划又缺少共同策略，Linux 会拒绝并保留设置，需要主动端恢复或明确重新选择 Auto/Direct/Relay。COMMIT 后失败恢复原命令，不自动改选路径。原生初次创建后，optional 网络不能通过旧周期配置流程绕过协商改路由。

分阶段代码边界、回归结果和真实 Linux 平台验收矩阵见 [Linux 响应端 Code Plan](../docs/wireguard/WIREGUARD_LINUX_RESPONDER_CODE_PLAN_2026-10-24.md)。P2 的共同策略、pair 进度与失败原因，以及 P3 主动发起尚未实施；编译和 fixture 通过不代表已完成真实 Linux 与桌面端的双端验收。Edge 仍需启用路径协商配置，对端当前会话也须支持该能力。
