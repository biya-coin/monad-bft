# Monad 压测集群 — Ansible 多主机部署

四步流程：**本地编译初始化 → 复制二进制 → 远程后台三服务 → 本地压测远程节点**。

## 四步命令

```bash
cd script-ansible
vim inventory/hosts.ini          # 改 monad-a~d 的 IP

make ping                        # SSH 连通
make init                        # ① 本地 build + setup + 打包
make deploy                      # ② 复制 monad / biyachaind / feed + 链数据
make start                       # ③ 各机后台 biyachaind + monad + feed
make stress                      # ④ 本地 chain-stresser → 远程 RPC
```

一键部署（不含压测）：`make site`  
全流程含压测：`make all`

## 目录结构

```
script-ansible/
├── playbooks/
│   ├── site.yml              # init + deploy + start
│   ├── 01-local-init.yml     # ① 本地编译 + genesis
│   ├── 02-deploy.yml         # ② 分发二进制 + 节点数据
│   ├── 03-start.yml          # ③ 后台三服务
│   ├── 04-stress-local.yml   # ④ 本地压测 → 远程
│   ├── 05-stop.yml
│   ├── 06-status.yml
│   └── tasks/build-p2p-map.yml
├── roles/
│   ├── common/               # sysctl / 目录
│   ├── monad-bin/            # 解压 binaries.tar.gz
│   ├── monad-node-data/      # 解压 node-*.tar.gz
│   ├── monad-process/        # start-node（三服务后台）
│   └── monad-health/         # 进程校验
└── artifacts/                # init 产出
    ├── binaries.tar.gz
    └── node-{a,b,c,d}.tar.gz
```

## 各步说明

### ① 本地编译与初始化 (`01-local-init.yml`)

在 **coordinator**（本机）执行，**不需要 Docker**：

| 动作 | 说明 |
|------|------|
| 编译 | `./scripts/0_monad-stress-bench.sh build` |
| 初始化 | `MULT_RUN_NO_DOCKER=1 setup`（本机 rm/sudo 清目录） |
| 节点数 | `MONAD_NODES_LIST` 与 inventory 中 validators 一致 |
| 打包 | `artifacts/binaries.tar.gz` + `node-*.tar.gz` |

需要：本机编译环境、**jq**、inventory 中 validator IP。若 `.monad` 有 root 属主残留，脚本会尝试 `sudo rm -rf`。

### ② 复制二进制 (`02-deploy.yml`)

分发到各 validator：

| 文件 | 目标 |
|------|------|
| `monad-node` | `/usr/local/bin/` |
| `biyachaind` | `/usr/local/bin/` |
| `cosmos-txpool-feed` | `/usr/local/bin/` |
| `monad-stress-bench.sh` | `/usr/local/bin/` |
| `node-<role>.tar.gz` | `/opt/monad/.monad/` |
| `/etc/monad-bench/env` | 运行时环境 |

### ③ 后台启动三服务 (`03-start.yml`)

每台 validator 调用 `start-node`，顺序启动：

1. **biyachaind**（ABCI socket）
2. **monad-node**（P2P 共识）
3. **cosmos-txpool-feed**（Comet RPC，监听 `0.0.0.0:26657` 等）

启动顺序：**a 先**，再 b/c/d。日志：`/var/log/monad-bench/`。

### ④ 本地压测 (`04-stress-local.yml`)

在 **coordinator** 运行 `stress-all`，通过 `STRESS_NODE_ADDR_*` 将交易发往远程 RPC：

```
chain-stresser → http://192.168.2.201:26657 → monad-a
              → http://192.168.2.106:26657 → monad-b
              → http://192.168.2.110:26657 → monad-c
```

需要 coordinator 安装 **chain-stresser**，压测账户在本地 `.monad/instances/0/`。

## 前置条件

- **coordinator**：ansible、Rust/Go 编译环境、**jq**、**chain-stresser**（压测用）；**无需 Docker**
- **validators**：SSH 可达、Ubuntu/Debian
- 网络：validator 间 P2P 8000/8001 互通；coordinator → validator RPC/gRPC 可达

## SSH 密码登录

本仓库 playbook 对 validators 使用 `become: true`（sudo），密码登录需同时解决 **SSH 登录** 和 **sudo 提权**。

### 1. 安装 sshpass（控制机 / coordinator）

```bash
sudo apt install sshpass
```

### 2. 三种配法（任选其一）

**A. 运行时交互输入（最安全，不落盘）**

```bash
ansible-playbook -i inventory/hosts.ini playbooks/02-deploy.yml \
  --ask-pass --ask-become-pass
# 或 Makefile：
make deploy ANSIBLE_ARGS='--ask-pass --ask-become-pass'
```

Makefile 需加一行传递参数（见下）。

**B. 写在 inventory（内网临时测试）**

编辑 `inventory/hosts.ini` 的 `[validators:vars]`：

```ini
[validators:vars]
ansible_user=ubuntu
ansible_ssh_pass=你的SSH密码
ansible_become_pass=你的sudo密码
ansible_become_method=sudo
```

若远程用户已 `NOPASSWD: ALL`，只需 `ansible_ssh_pass`。

**C. 独立变量文件（推荐略好于明文进 hosts.ini）**

```bash
cp inventory/group_vars/validators/password.yml.example \
   inventory/group_vars/validators/password.yml
# 编辑 password.yml，不要 commit
```

Ansible 会自动加载 `group_vars/validators/*.yml`。

### 3. 单台主机不同密码

在 `hosts.ini` 里按主机覆盖：

```ini
monad-a ansible_host=192.168.2.201 ansible_ssh_pass=pass_a ansible_become_pass=pass_a
monad-b ansible_host=192.168.2.106 ansible_ssh_pass=pass_b ansible_become_pass=pass_b
```

### 4. 验证连通

```bash
ansible -i inventory/hosts.ini validators -m ping --ask-pass
# 若 inventory 已写密码则不需要 --ask-pass
```

### 5. coordinator 本机

`bench-coord` 使用 `ansible_connection=local`，**不走 SSH 密码**；只有 `validators` 组需要配。

### 6. 常见报错

| 报错 | 处理 |
|------|------|
| `sshpass` not found | `sudo apt install sshpass` |
| Permission denied (publickey) | 未配密码且未 `--ask-pass`，或密码错 |
| Missing sudo password | 加 `ansible_become_pass` 或 `--ask-become-pass` |
| Host key verification failed | 已设 `host_key_checking=False`；仍失败则手动 `ssh user@host` 接受指纹 |

## 变量 (`inventory/group_vars/all.yml`)

| 变量 | 默认 | 说明 |
|------|------|------|
| `monad_work` | `/opt/monad/.monad` | validator 链数据 |
| `bench_rpc_bind` | `0.0.0.0` | feed 对外监听 |
| `monad_port.grpc` | `19900` | 各 validator **相同** gRPC 端口 |
| `monad_port.comet` | `26657` | 各 validator **相同** Comet RPC |
| `monad_port.p2p` | `8000` | 各 validator **相同** P2P |
| `stress_rate_tps` | `1200` | 每片 TPS |
| `monad_nodes` | `a,b,c` | 须与 `hosts.ini` validators 一致 |

多主机每机一节点时，**端口统一、仅 IP 不同**（例如三台都是 `:26657`）。  
单机四节点压测才需要 a/b/c/d 差异化端口（不设 `BENCH_*_PORT` 时 bench 脚本默认行为）。

## 运维

```bash
make status          # 进程 + RPC 探测
make stop            # 停止远程三服务

# validator 手工
source /etc/monad-bench/env
tail -f /var/log/monad-bench/monad-a.log
monad-stress-bench.sh nodes
```

## 注意

- 每 validator **仅一个节点**；biyachaind 与 monad 必须同机
- 多主机**不要** `setup-ips`（单机 loopback 专用）
- `make init` 会重建 coordinator 上 `.monad/` genesis（无 Docker，本机二进制 + 链数据复制到远程）
