# YAT Linux Server 客户端发布说明

> 当前源码（2026-10-19）：Docker 默认前台执行 `yat-client serve`，含 Web 管理界面，容器内监听 `0.0.0.0:9909`。部署与停机细节见 [README](README.md)。以下历史版本记录不代表新原生路径协调已启用或已完成实机验收。

YAT Linux Server 客户端 (`yat-client`，目录 `yat-fe-linux`) 是面向 Linux 服务器的独立 Rust CLI/服务，与 `yat-fe` 的 Electron Linux 桌面构建不同；它提供 WireGuard 组网的能力与 Web 管理界面。支持 x86_64 / arm64 双架构，可通过 Docker 或二进制文件直接运行。

---

## 版本更新记录

### Edge 重启恢复修正 / 未发布（2026-10-26）

- Edge 断连、控制连接错误及传输超时统一交给原 supervisor 恢复：排空旧实例、重新认证入网、重建网络与路径响应订阅，保留 pair 策略。
- 自动重试间隔为 2、4、8、16、30 秒，后续封顶 30 秒持续重试；移除失败 10 次后的 30 分钟等待。TCP/TLS 建连期限 10 秒，认证期限 30 秒，native 操作保持原有排空规则。
- 授权/协调拒绝与 native 错误不会单独触发重新入网。恢复阶段日志和 Web 重试时间保留；本机回归不替代真实 Edge 重启验收，详见 [Code Plan §11](../docs/wireguard/WIREGUARD_LINUX_RESPONDER_CODE_PLAN_2026-10-24.md#11-edge-重启后的自动恢复修正2026-10-26)。

### 当前源码 / 未发布（2026-10-19）

- Linux Server 的 `start/serve/leave/import` 与 Web 操作统一交给同一 `client_owner`；同一配置目录通过永久锁文件和私有 `yat-fe-linux.sock` 排除第二个 native 配置写入者。桌面 Linux 不使用此套 Server IPC/Docker 生命周期。
- 初次连接、刷新、重连、导入替换和停机复用受控 controller。停止时先关闭写入口及 Edge 控制连接，再等待原任务与 native 清理；清理失败保留所有者并重试。普通停止保留成员资格，显式 `leave` 才退出网络。
- 两种 Linux 产品均要求内核 **≥ 5.6**；Docker 检查宿主机。旧发行版的 5.4 回移不属于本项目基线，工具安装不能替代内核支持及权限。
- 镜像使用 exec ENTRYPOINT，默认 `serve` 前台运行；可覆盖为 `start` 关闭 Web。Web 管理 API 当前无鉴权，仅在可信管理范围开放；下面示例只发布到宿主机回环地址。
- SIGKILL、崩溃和宿主机故障无法保证退出回调；由 Edge 断连/超时判定离线。清理不按进程退出码区分。
- 新路径协调具备受控实现和离线回归，但生产 Owner/PathControlSession、手动权威请求、自动稳定策略及实机验收仍未完成，能力未发布。详见[主计划 §25](../docs/wireguard/WIREGUARD_NATIVE_PATH_COORDINATION_CODE_PLAN_2026-10-17.md#25-2026-10-19-整体复核与当前完成边界)。

### v0.2.0 (2026-10-16)

**新导出格式支持**
- 支持桌面端新版 Share Package 导出格式，分为两种模式：
  - **完整证书模式**：导出包含完整的客户端证书和私钥，导入后即可直接连接
  - **令牌模式**：导出包含邀请令牌，首次连接时自动从 Edge 服务器获取证书（无需 OAuth 登录）
- 两种模式均自动生成文件名：`yat-net-{Edge标识}-{网络ID}.json`

**证书获取方式变更**
- 不再依赖 OAuth 登录获取证书
- 令牌模式下，客户端首次启动时自动通过邀请令牌向 Edge 申请证书
- 证书到期后需从桌面端重新导出新的分享包进行更新

**WireGuard 网络同步升级（历史旧选路能力，不代表新双端协调验收）**
- 采用全新的网络同步引擎，支持：
  - Relay 中继路径自动管理（无法直连时自动通过中继转发）
  - 路径选择与动态切换（直连 ↔ 中继自动切换）
  - Peer 健康检查（基于 WireGuard 握手状态判断连接质量）
  - 网络拓扑变化时自动更新 WireGuard 配置

**其他改进**
- 关机时自动清理所有 WireGuard 网络接口（支持多网络场景）
- 精简配置结构，移除不再需要的用户身份信息和 OAuth 配置

### v0.1.0

- 初始版本
- WireGuard 组网 + Web Dashboard
- Edge 连接 + 自动重连
- 证书管理与监控
- 同 NAT LAN 端点发现

---

## 快速开始

只需三步，即可将一台 Linux 机器接入 YAT WireGuard 网络：

```bash
# 1. 启动客户端（Docker 方式）
docker run --name yat-client \
  --stop-timeout 60 \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  -p 127.0.0.1:9909:9909 \
  --mount type=volume,src=yat-client-config,dst=/root/.config/yat \
  registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:latest

# 2. 打开浏览器访问 Dashboard，导入网络配置
#    http://127.0.0.1:9909（远程管理使用 SSH 端口转发）

# 3. 在 Dashboard 的 Settings 页面上传 share.json，客户端自动连接
```

> **提示**：也可以从桌面端 (macOS/Windows) 导出 `share.json`，通过 Dashboard 或 CLI 导入。

---

## 系统要求

| 项目 | 要求 |
|------|------|
| **操作系统** | Linux (x86_64 / aarch64) |
| **宿主机内核** | Linux ≥ 5.6，具备 WireGuard 内建或模块支持；5.4 回移不在支持基线内 |
| **工具** | `iproute2`、`wireguard-tools`；工具不能替代内核支持 |
| **Docker** | Docker 20.10+（Docker 方式运行时需要） |
| **权限** | `root` 或 `CAP_NET_ADMIN`（创建 WireGuard 网卡需要） |
| **网络** | 可访问 YAT Edge 服务器（默认端口由 Edge 配置决定） |
| **内存** | 最低 64 MB 可用内存 |

### Docker 运行所需的额外权限

```bash
# 保留镜像部署的设备参数；Linux 数据面使用内核 WireGuard，需要 NET_ADMIN
--cap-add NET_ADMIN \
--device /dev/net/tun
```

---

## 运行方式

### 方式一：Docker 运行（推荐）

#### 拉取镜像

```bash
# 从阿里云镜像仓库拉取（支持 amd64 / arm64）
docker pull registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:latest
```

#### 启动容器

```bash
# 基础启动（端口映射模式）
docker run --name yat-client \
  --stop-timeout 60 \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  -p 127.0.0.1:9909:9909 \
  --mount type=volume,src=yat-client-config,dst=/root/.config/yat \
  registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:latest

# 可选 host 网络：Web 显式绑定回环地址，勿直接暴露管理 API
docker run --name yat-client \
  --stop-timeout 60 \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  --network host \
  -e YAT_LISTEN=127.0.0.1:9909 \
  --mount type=volume,src=yat-client-config,dst=/root/.config/yat \
  registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:latest
```

#### 自定义端口

```bash
# 修改 Web UI 监听端口
docker run --name yat-client \
  --stop-timeout 60 \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  -p 127.0.0.1:8080:8080 \
  --mount type=volume,src=yat-client-config,dst=/root/.config/yat \
  registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:latest \
  serve --listen 0.0.0.0:8080
```

#### 持久化配置

```bash
# 挂载配置目录，避免重启后丢失
docker run --name yat-client \
  --stop-timeout 60 \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  -p 127.0.0.1:9909:9909 \
  -v ~/.config/yat:/root/.config/yat \
  registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:latest
```

#### 查看日志

```bash
docker logs -f yat-client
```

---

### 方式二：CLI 直接运行

#### 下载二进制

```bash
# 下载对应架构的二进制文件（以 x86_64 为例）
chmod +x yat-client
```

#### CLI 命令一览

| 命令 | 说明 |
|------|------|
| `yat-client import <share.json>` | 导入网络配置包 |
| `yat-client serve [--listen ADDR]` | 前台运行所有者 + Web 管理界面 |
| `yat-client start` | 前台连接模式（无 Web UI） |
| `yat-client status` | 查看当前网络状态 |
| `yat-client leave` | 离开网络并拆除 WireGuard 接口 |
| `yat-client cert-info` | 查看证书信息 |

#### 典型使用流程

```bash
# 1. 导入网络配置（从桌面端导出的 share.json）
sudo yat-client import share.json

# 2. 前台启动 Web（源码默认 0.0.0.0:9909，此处限制为回环地址）
sudo yat-client serve --listen 127.0.0.1:9909

# 或指定监听地址
sudo yat-client serve --listen 127.0.0.1:8080

# 3. 浏览器打开 Dashboard
#    http://127.0.0.1:9909
```

> **注意**：创建/更新 WireGuard 接口需要 root 或 `CAP_NET_ADMIN`；没有运行所有者时，`import` 只写配置。所有命令须使用同一身份和配置目录，运行中的导入交给现有所有者完成替换及重连。

#### 前台模式（无 Web UI）

```bash
# 适合脚本化或临时使用
sudo yat-client start
# 按 Ctrl+C 断开连接
```

#### 查看状态

```bash
sudo yat-client status
```

`status` 读取保存的成员/接口信息、applied state 和可用的原生 stats；它目前不查询运行中 owner 的控制连接状态，因此 `Connected: no (daemon mode or stale)` 不能用于判断现有服务是否离线。Web `/api/status` 读取运行中 owner 的状态，`connected` 也不代表对端业务已验证。Linux 使用内核 WireGuard，不是 boringtun/userspace 后端。

---

## Web Dashboard 功能介绍

启动 `serve` 命令后，可通过浏览器访问功能丰富的 Web 管理界面。

### Dashboard 标签页

- **Network Info** — 查看网络 ID、成员 ID、分配地址、接口名称等核心信息
- **Network Status** — 实时连接状态、在线 Peer 数量、最后同步时间
- **Peers** — 完整的 Peer 列表，包含主机名、IP 地址、Endpoint、收发流量统计
- **Certificate** — 证书详情（Subject / Issuer / 有效期 / 指纹），提供证书状态检查与重新导入提示（不自动签发新证书）
- **Latest Logs** — 最近 50 条运行日志，按 INFO / WARN / ERROR 分级着色

### Settings 标签页

- **Edge Connection** — 查看 Edge 服务器地址、端口、Edge ID
- **Identity** — 查看设备 ID、主机名
- **Import** — 拖拽上传 `share.json` 文件，由现有所有者排空旧资源、更新配置并重新连接
- **Reconnect** — 手动触发重连（断线时可用）
- **Update Address** — 动态修改设备在网络中的 IP 地址

### Dashboard 特性

- 暗色主题，YAT 绿色风格
- 5 秒自动刷新状态数据
- 支持无配置启动 — 先启动 Dashboard，再通过 Web 界面导入配置
- 证书到期自动监控 — 每小时检查，临近过期时显示提醒
- 断线自动重连 — 指数退避，当前源码持续重试且等待上限为 30 秒（旧版失败 10 次后等待 30 分钟）

---

## 配置说明

配置文件存储在 `~/.config/yat/` 目录下：

```
~/.config/yat/
├── config.json   # 主配置文件（Edge 连接 + 设备身份 + 网络配置）
├── device-id     # 本地设备 ID（稳定，跨导入不变）
├── yat-fe-linux.lock # 永久锁文件，运行中不得删除
├── yat-fe-linux.sock # 当前所有者的私有命令通道
├── import.pending   # 导入未完成时保留，阻止混合配置启动
├── state.json       # 已保存的连接/接口信息，不是实时在线证明
├── wireguard/       # 身份、配置及 applied state
└── certs/
    ├── ca.crt      # CA 证书
    ├── client.crt  # 客户端证书（完整证书模式 / 令牌引导后生成）
    └── client.key  # 客户端私钥（完整证书模式 / 令牌引导后生成）
```

---

## 架构支持

| 架构 | Docker 镜像 | 二进制文件 |
|------|-------------|-----------|
| linux/amd64 (x86_64) | ✅ | ✅ |
| linux/arm64 (aarch64) | ✅ | ✅ |

Dockerfile 支持 Alpine 3.20（musl）和 Debian bookworm（gnu）变体；gnu 构建矩阵还包含 armv7。实际可用架构以发布镜像的 manifest 为准，本轮未构建或运行镜像。

---

## 常见问题

**Q: 为什么需要 root 权限？**  
A: Linux 使用内核 WireGuard，需要创建 WG 网络接口并配置地址/路由，因此需要目标网络命名空间的 root 或 CAP_NET_ADMIN。

**Q: Docker 容器重启后配置丢失？**  
A: 请挂载持久化卷 `-v ~/.config/yat:/root/.config/yat`。

**Q: 证书过期了怎么办？**
A: 从桌面端重新导出新的分享包（完整证书模式或令牌模式），导入后自动更新证书。

**Q: 如何在后台运行？**  
A: 可交给 Docker 或 systemd 管理；应用保持前台运行。Docker 如需脱离终端可加 `-d`，停止使用 `docker stop --time 60 yat-client`，让 SIGTERM 到达 PID 1 的同一所有者；宽限期不保证清理一定完成。
