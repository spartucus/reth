# 第7章：网络层与 P2P 协议

> **核心问题：** 节点如何发现彼此？交易和区块如何在全球数千个节点间传播？

---

## 目录

1. [网络层全景](#1-网络层全景)
2. [协议栈：DevP2P 与 RLPX](#2-协议栈devp2p-与-rlpx)
3. [节点发现：Discv4 与 Kademlia DHT](#3-节点发现discv4-与-kademlia-dht)
4. [ETH Wire Protocol 消息系统](#4-eth-wire-protocol-消息系统)
5. [ETH 协议版本演进](#5-eth-协议版本演进)
6. [NetworkManager：网络总调度](#6-networkmanager网络总调度)
7. [会话管理：RLPX 握手全流程](#7-会话管理rlpx-握手全流程)
8. [Peer 管理与声誉系统](#8-peer-管理与声誉系统)
9. [交易传播：TransactionsManager](#9-交易传播transactionsmanager)
10. [区块传播](#10-区块传播)
11. [NetworkHandle：对外公共接口](#11-networkhandle对外公共接口)
12. [一笔交易的完整传播路径](#12-一笔交易的完整传播路径)
13. [关键设计决策](#13-关键设计决策)

---

## 1. 网络层全景

### crates/net/ 子模块结构

以太坊 P2P 网络的实现分布在 `crates/net/` 下的多个专用 crate 中：

```
crates/net/
├── network/             # 核心 P2P 网络编排（NetworkManager、会话、Peer 管理）
├── network-api/         # 公共接口 trait（NetworkInfo、Peers、FullNetwork）
├── network-types/       # Peer 声誉和配置的共享类型
├── eth-wire/            # RLPX 协议实现与握手
├── eth-wire-types/      # ETH 协议消息类型（Status、Block、Transactions）
├── p2p/                 # 通用 P2P 抽象（下载器 trait）
├── discv4/              # Discovery v4（UDP 上的 Kademlia DHT）
├── discv5/              # Discovery v5 协议
├── ecies/               # ECIES 加密（用于 RLPX 流）
├── banlist/             # IP/Peer 封禁机制
├── nat/                 # NAT 解析（对外 IP 检测）
├── dns/                 # DNS-based 节点发现
└── downloaders/         # 历史同步的区块下载器
```

### 核心组件关系

```
┌─────────────────────────────────────────────────────────┐
│                     NetworkManager                       │
│  ┌─────────────────────────────────────────────────┐   │
│  │                     Swarm                        │   │
│  │  ┌─────────────┐ ┌───────────┐ ┌─────────────┐  │   │
│  │  │SessionManager│ │Connection │ │NetworkState │  │   │
│  │  │(RLPX 握手)   │ │Listener   │ │(Peer 状态)  │  │   │
│  │  └─────────────┘ └───────────┘ └─────────────┘  │   │
│  └─────────────────────────────────────────────────┘   │
│                                                          │
│  ← NetworkHandle（command channel）→                    │
│  → NetworkEvent（event sender）→                        │
│  ↔ TransactionsManager（unbounded channel）             │
│  ↔ EthRequestHandler（bounded channel）                 │
└─────────────────────────────────────────────────────────┘
         ↑ 发现的新 Peer              ↑ UDP
    ┌────────────┐              ┌──────────────┐
    │  Discv4    │              │    Discv5    │
    │  (Kademlia)│              │  (discv5)    │
    └────────────┘              └──────────────┘
```

每个组件都运行在独立的异步任务中，通过 tokio channel 通信：
- **NetworkManager** 是有状态的无限 Future，统一驱动网络前进
- **TransactionsManager** 独立任务，专门处理交易的接收与广播
- **EthRequestHandler** 独立任务，处理来自 peer 的区块头/体请求
- **Discv4/Discv5** 独立 UDP 任务，持续发现新节点

---

## 2. 协议栈：DevP2P 与 RLPX

### 协议分层

以太坊 P2P 网络（DevP2P）建立在以下层次上：

```
应用层：  eth/69（以太坊wire协议）+ snap（快照同步）+ 自定义子协议
         ─────────────────────────────────────────────────────
复用层：  RLPx 多路复用（capability 协商 + 消息帧）
         ─────────────────────────────────────────────────────
认证层：  ECIES 加密握手（secp256k1 + AES-256-CTR + HMAC-SHA-256）
         ─────────────────────────────────────────────────────
传输层：  TCP（连接建立 + 可靠传输）
         ─────────────────────────────────────────────────────
发现层：  Discovery v4/v5（UDP，Kademlia DHT）
```

### RLPx Hello 消息

TCP 连接建立后，ECIES 加密完成，双方交换 **Hello 消息**：

```rust
// crates/net/eth-wire/src/hello.rs
pub struct HelloMessageWithProtocols {
    pub client_version: String,     // e.g., "reth/v1.0.0/linux-x86_64/rustc1.79"
    pub capabilities: Vec<Capability>, // e.g., [("eth", 69), ("snap", 1)]
    pub port: u16,
    pub id: PeerId,                 // secp256k1 公钥（64字节）
    pub protocol_version: ProtocolVersion,  // V4 或 V5（默认 V5）
}
```

**Capability 协商**：双方取 Hello 中 capabilities 的交集，确定共同支持的协议版本。如果没有共同支持的 `eth` 版本，连接断开。

### EIP-2124：ForkId 验证

Status 交换时，通过 **ForkId** 拒绝与不同链（或不同分叉）的节点建立 `eth` 会话：

```
ForkId = (hash: 当前分叉状态的CRC32校验和, next: 下一个分叉触发点)
```

例如，主网 Cancun 分叉激活后，ForkId 的 hash 会发生变化，Pre-Cancun 节点会被 Fork 验证拒绝。这确保 Goerli 节点不会意外连接到主网节点。

---

## 3. 节点发现：Discv4 与 Kademlia DHT

### 为什么需要节点发现？

互联网上分散着数千个以太坊节点，如何找到它们？答案是 **Kademlia DHT**（分布式哈希表）——一种去中心化的节点目录系统。

### Discv4 架构

发现协议使用 UDP（默认端口 30303），独立于 TCP 连接：

```rust
// crates/net/discv4/src/lib.rs:167

// 前端（可克隆，通过 channel 通信）
pub struct Discv4 {
    local_addr: SocketAddr,              // UDP 套接字地址
    to_service: mpsc::UnboundedSender<Discv4Command>,  // 命令 channel
    node_record: Arc<Mutex<NodeRecord>>, // 本节点记录（含外部 IP）
}
```

`Discv4Service` 是实际运行的后台任务，管理：
- **KBucketsTable**：Kademlia 路由表（`reth-discv4` 使用 `discv5::kbucket`）
- 外部 IP 发现（通过多个 PONG 响应投票）
- 节点鲜活性维护

### Discv4 消息协议

所有消息通过 UDP 发送，使用 secp256k1 签名（节点 ID 即公钥）：

| 消息 | 方向 | 作用 |
|------|------|------|
| **Ping** | → | 探测目标节点是否存活，包含端点信息 |
| **Pong** | ← | 响应 Ping，包含 ping hash 防重放 |
| **FindNode** | → | 查询最靠近某 target ID 的节点 |
| **Neighbours** | ← | 返回最多 16 个已知节点的 NodeRecord |
| **ENRRequest** | → | 请求 EIP-868 节点记录 |
| **ENRResponse** | ← | 返回完整 ENR（含 IP、端口、forkId 等） |

### Kademlia 路由表参数

```
MAX_PACKET_SIZE     = 1280 字节  （UDP 包大小限制）
ALPHA               = 3         （递归查找并发因子）
MAX_NODES_PER_BUCKET = 16       （每个 k-bucket 容量，即 k 值）
ENDPOINT_PROOF_EXPIRATION = 24h  （节点鲜活性过期时间）
```

### 节点查找算法

```
目标：找到网络中最靠近某节点 ID 的节点

1. 从本地路由表中选出最近的 ALPHA(3) 个节点
2. 向它们并发发送 FindNode(target)
3. 收到 Neighbours 后，向新发现的最近节点继续查询
4. 重复，直到没有更近的节点出现
5. 结果：找到的最近节点集合
```

### 外部 IP 发现

节点通过多次 Pong 响应的 `recipient.ip` 字段投票确定自己的外部 IP（NAT 后面的节点无法自知外部 IP）：
- 收到多个 Pong 后，取出现次数最多的 IP 作为外部 IP
- 更新到 `Discv4.node_record`（`Arc<Mutex<NodeRecord>>`）

### Discv5 与引导节点

**Discv5** 是更新版的发现协议，使用 ENR（Ethereum Node Records）记录节点信息，支持更丰富的元数据（如 ForkId）。reth 同时支持 discv4 和 discv5，可通过配置选择。

**引导节点（bootnodes）**：节点首次启动时通过硬编码的 bootnode 地址接入网络，随后通过 DHT 自动发现更多节点。

---

## 4. ETH Wire Protocol 消息系统

ETH Wire Protocol 是在 RLPX 上运行的应用层协议，定义了节点间交换区块和交易的消息格式。

### EthMessageID 枚举

```rust
// crates/net/eth-wire-types/src/message.rs:527

pub enum EthMessageID {
    Status               = 0x00,  // 初始握手状态
    NewBlockHashes       = 0x01,  // 广播新区块哈希
    Transactions         = 0x02,  // 广播完整交易
    GetBlockHeaders      = 0x03,  // 请求区块头
    BlockHeaders         = 0x04,  // 响应区块头
    GetBlockBodies       = 0x05,  // 请求区块体
    BlockBodies          = 0x06,  // 响应区块体
    NewBlock             = 0x07,  // 广播完整新区块（PoW）
    NewPooledTransactionHashes = 0x08,  // 广播交易哈希（eth/66+）
    GetPooledTransactions = 0x09, // 请求完整交易
    PooledTransactions   = 0x0a,  // 响应完整交易
    GetNodeData          = 0x0d,  // 请求节点数据（eth/67 废弃）
    NodeData             = 0x0e,  // 响应节点数据（eth/67 废弃）
    GetReceipts          = 0x0f,  // 请求收据
    Receipts             = 0x10,  // 响应收据
    BlockRangeUpdate     = 0x11,  // 通知可提供的区块范围（eth/69 新增）
}
```

消息分为两类：

**广播消息（主动推送，无 request ID）：**
- `NewBlockHashes`：`Vec<(B256, u64)>`，轻量的区块存在通知
- `NewBlock`：完整区块 + total_difficulty（PoW 专用，PoS 不用）
- `Transactions`：完整交易列表，直接广播给部分 peer
- `NewPooledTransactionHashes`：交易哈希通告，接收方按需拉取

**请求-响应消息（有 request ID，通过 `RequestPair<T>` 封装）：**
- `GetBlockHeaders` / `BlockHeaders`：按哈希或区块号请求头部
- `GetBlockBodies` / `BlockBodies`：按哈希请求完整区块体
- `GetPooledTransactions` / `PooledTransactions`：按哈希拉取完整交易（含 blob）
- `GetReceipts` / `Receipts`：请求交易收据

### 消息大小限制

```rust
pub const MAX_MESSAGE_SIZE: usize = 10 * 1024 * 1024;  // 10 MiB
```

交易响应的软限制为 **2 MiB**（由 `SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE` 控制），防止单次响应占用过多带宽。

---

## 5. ETH 协议版本演进

### EthVersion 枚举

```rust
// crates/net/eth-wire-types/src/version.rs:21

pub enum EthVersion {
    Eth66 = 66,  // 引入 request ID（请求-响应配对）
    Eth67 = 67,  // 移除 GetNodeData/NodeData（为 snap 同步让路）
    Eth68 = 68,  // NewPooledTransactionHashes 携带类型和大小信息
    Eth69 = 69,  // Status 去掉 total_difficulty + 新增 BlockRangeUpdate
    Eth70 = 70,  // 收据去掉 bloom filter（EIP-7688）
    Eth71 = 71,
    Eth72 = 72,
}

pub const LATEST: EthVersion = Eth69;
pub const ALL_VERSIONS: &[EthVersion] = &[Eth69, Eth68, Eth67, Eth66];  // 当前启用优先顺序
```

### 各版本关键变化

**ETH/66**：引入 `RequestPair<T>`（request_id + payload），使得请求和响应可以并发且正确配对。之前 ETH/65 的请求-响应是严格串行的。

**ETH/67**：删除 `GetNodeData`/`NodeData`（用于旧的全节点同步）。snap 协议（独立 capability）承担了快速同步的职责。

**ETH/68**：`NewPooledTransactionHashes` 从仅包含哈希列表，升级为包含 **(类型, 大小, 哈希)** 三元组：
```rust
// NewPooledTransactionHashes68（eth/68+）
pub struct NewPooledTransactionHashes68 {
    pub types: Vec<u8>,    // 交易类型字节（0=Legacy, 1=EIP-2930, 2=EIP-1559, 3=Blob）
    pub sizes: Vec<u32>,   // 每笔交易的字节大小
    pub hashes: Vec<B256>, // 交易哈希
}
```
接收方可以根据类型和大小决定是否拉取，避免拉取自己不支持的类型或过大的交易。

**ETH/69**：
- **Status 消息**去掉 `total_difficulty` 字段（PoS 下 TD 已无意义，EIP-7642）
- 新增 `BlockRangeUpdate`（`0x11`）：节点通知 peer 自己当前能提供哪个区块范围（earliest ~ latest）

**ETH/70+**：代码中已经保留 `Eth70`、`Eth71`、`Eth72` 枚举和 capability 判断辅助函数，但当前 `LATEST` / `ALL_VERSIONS` 仍只启用到 ETH/69。

### Status 消息演进

```rust
// ETH/66~68 Status（crates/net/eth-wire-types/src/status.rs）
pub struct Status {
    pub version: EthVersion,
    pub chain: Chain,           // 链 ID（防止跨链连接）
    pub total_difficulty: U256, // ← ETH/69 移除
    pub blockhash: B256,        // 最新已知区块哈希
    pub genesis: B256,          // 创世块哈希（防止跨网络连接）
    pub forkid: ForkId,         // EIP-2124 分叉 ID
}

// ETH/69+ StatusEth69（移除 total_difficulty）
pub struct StatusEth69 {
    pub version: EthVersion,
    pub chain: Chain,
    pub blockhash: B256,
    pub genesis: B256,
    pub forkid: ForkId,
}
```

---

## 6. NetworkManager：网络总调度

### 架构：无限 Future 模式

```rust
// crates/net/network/src/manager.rs:103

#[must_use = "The NetworkManager does nothing unless polled"]
pub struct NetworkManager<N: NetworkPrimitives = EthNetworkPrimitives> {
    swarm: Swarm<N>,
    handle: NetworkHandle<N>,
    from_handle_rx: UnboundedReceiverStream<NetworkHandleMessage<N>>,
    block_import: Box<dyn BlockImport<N::NewBlockPayload>>,
    event_sender: EventSender<NetworkEvent<PeerRequest<N>>>,
    to_transactions_manager: Option<UnboundedMeteredSender<NetworkTransactionEvent<N>>>,
    to_eth_request_handler: Option<mpsc::Sender<IncomingEthRequest<N>>>,
    num_active_peers: Arc<AtomicUsize>,
    metrics: NetworkMetrics,
    disconnect_metrics: DisconnectMetrics,
}
```

`NetworkManager` 实现了 `Future`，在 tokio 运行时中以无限循环方式驱动：

```
poll() 调用时：
1. 排尽 from_handle_rx（NetworkHandle 发来的命令，预算限制）
2. 排尽 Swarm（连接状态变化、新消息、会话事件，预算限制）
3. 路由 SwarmEvent：
   - SessionEstablished → 通知 TransactionsManager、发 NetworkEvent
   - SessionClosed      → 清理状态
   - ValidMessage       → 路由到对应 handler
   - BadMessage         → 扣减 peer 声誉
```

### 预算机制（Budget）

为避免某个流饥饿其他流，`poll()` 每次只消费固定数量的事件：

```rust
const DEFAULT_BUDGET_TRY_DRAIN_NETWORK_HANDLE_CHANNEL: usize = ...;
const DEFAULT_BUDGET_TRY_DRAIN_SWARM: usize = ...;
```

即：每次 poll 最多处理 N 条命令，然后切换到处理 Swarm 事件，再切回来。这防止了高负载时的"活锁"现象。

### Swarm 内部结构

`Swarm` 封装了三个子组件：

```rust
// crates/net/network/src/swarm.rs:49
pub struct Swarm<N: NetworkPrimitives = EthNetworkPrimitives> {
    incoming: ConnectionListener,    // TCP 监听器（接受入站连接）
    sessions: SessionManager<N>,     // RLPX 会话管理（握手+认证）
    state: NetworkState<N>,          // Peer 跟踪与状态
}
```

`Swarm` poll 时产生 `SwarmEvent<N>`：
- `SessionEstablished`：RLPX 握手成功，peer 通过 Status 验证
- `SessionClosed`：连接断开
- `ValidMessage { peer_id, message }`：收到 eth 消息
- `BadMessage { peer_id }`：收到格式错误的消息
- `TcpListenerError` / `OutgoingConnectionError`

---

## 7. 会话管理：RLPX 握手全流程

### SessionManager 核心结构

```rust
// crates/net/network/src/session/mod.rs:63
pub struct SessionManager<N: NetworkPrimitives> {
    secret_key: SecretKey,                   // 本节点 secp256k1 私钥
    status: UnifiedStatus,                   // 本节点的 eth Status
    hello_message: HelloMessageWithProtocols,// RLPx Hello 消息（含 capabilities）
    fork_filter: ForkFilter,                 // EIP-2124 ForkId 验证器

    pending_sessions: FxHashMap<SessionId, PendingSessionHandle>,   // 握手中的会话
    active_sessions: HashMap<PeerId, ActiveSessionHandle<N>>,       // 已认证的会话

    pending_sessions_tx: mpsc::Sender<PendingSessionEvent<N>>,
    active_session_tx: MeteredPollSender<ActiveSessionMessage<N>>,

    extra_protocols: RlpxSubProtocols,       // 额外的 RLPX 子协议（如 snap）
    local_range_info: BlockRangeInfo,        // 本节点可提供的区块范围
    handshake: Arc<dyn EthRlpxHandshake>,    // 可插拔的握手实现
}
```

### 握手完整流程

```
TCP 连接建立
    │
    ▼
ECIES 认证握手（UDP-like 数据包）
  ├─ 发起方发送 Auth message（含 nonce + 公钥签名）
  ├─ 接收方发送 Ack message（含自己的 nonce + 公钥）
  └─ 双方推导出共享会话密钥（AES + HMAC）
    │
    ▼
RLPx Hello 交换
  ├─ 双方各发一条 Hello message
  └─ 协商出共同支持的 capabilities（取交集）
    │
    ▼
eth Status 交换（针对 eth capability）
  ├─ ETH/68: Status（含 total_difficulty, genesis, forkid）
  ├─ ETH/69: StatusEth69（无 total_difficulty）
  └─ ForkFilter 验证 ForkId，不匹配则断连
    │
    ▼
Active Session（开始正常消息传输）
  - 双向多路复用 eth、snap 等 capability 消息
  - 请求-响应通过 request_id 配对
```

### 会话方向

```rust
pub enum Direction {
    Inbound,   // 远端主动连接我们（通过 TCP 监听器接收）
    Outbound,  // 我们主动连接远端（通过 PeersManager 触发）
}
```

**出站连接**由 `PeersManager` 驱动——它决定应该主动连接哪些节点（从 Discv4 发现的节点中选择）。

### 超时保护

- `pending_session_timeout`：握手超时，超时则断连（防止慢速或恶意节点占用槽位）
- `protocol_breach_request_timeout`：请求响应超时
- `initial_internal_request_timeout`：首次请求超时

### BlockRangeUpdate（ETH/69 新增）

Active Session 会定期发送 `BlockRangeUpdate` 消息，告知对方自己能提供的区块范围：

```
RANGE_UPDATE_INTERVAL 触发
    → 发送 BlockRangeUpdate { earliest_block, latest_block }
```

这让请求方在发 `GetBlockHeaders` 前就知道对方能提供哪些区块，避免无效请求。

---

## 8. Peer 管理与声誉系统

### PeersManager 核心结构

```rust
// crates/net/network/src/peers.rs:48
pub struct PeersManager {
    peers: HashMap<PeerId, Peer>,            // 所有已知 peer
    trusted_peer_ids: HashSet<PeerId>,       // 显式信任的 peer
    trusted_peers_resolver: TrustedPeersResolver,  // 信任 peer 的 DNS 解析

    queued_actions: VecDeque<PeerAction>,    // 待处理的 peer 操作
    refill_slots_interval: Interval,         // 定期尝试建立新连接
    connection_info: ConnectionInfo,         // 入站/出站连接槽位统计

    ban_list: BanList,                       // IP/PeerId 封禁名单
    backed_off_peers: HashMap<PeerId, Instant>,     // 退避中的 peer
    ban_duration: Duration,                  // 封禁持续时间
    backoff_durations: PeerBackoffDurations, // 指数退避时间

    reputation_weights: ReputationChangeWeights,
    trusted_nodes_only: bool,               // 是否只连接信任节点
    max_backoff_count: u8,                  // 最大退避次数（超过则永久丢弃）
    ip_filter: IpFilter,                    // IP 范围过滤
}
```

### 声誉数值系统

```rust
// crates/net/network-types/src/peers/reputation.rs

type Reputation = i32;

const REPUTATION_UNIT: i32 = -1024;                          // 基本单位

const DEFAULT_REPUTATION: i32 = 0;                           // 初始声誉
const BANNED_REPUTATION: i32  = 50 * REPUTATION_UNIT;        // -51200（封禁阈值）

// 扣分规则：
const REMOTE_DISCONNECT:   i32 =  4 * REPUTATION_UNIT;  //  -4096
const FAILED_TO_CONNECT:   i32 = 25 * REPUTATION_UNIT;  // -25600
const TIMEOUT:             i32 =  4 * REPUTATION_UNIT;  //  -4096
const BAD_MESSAGE:         i32 = 16 * REPUTATION_UNIT;  // -16384
const BAD_PROTOCOL:        i32 = i32::MIN;              // 立即封禁
const ALREADY_SEEN_TX:     i32 = 0;                    // 不扣分（很常见）
```

`ReputationChangeKind` 枚举包括：`BadMessage`, `BadBlock`, `BadTransactions`, `BadAnnouncement`, `Timeout`, `FailedToConnect`, `DropedTooManyPeers` 等。

**信任 peer 特殊保护**：信任 peer 每次声誉变化最多 `2 * REPUTATION_UNIT`（`MAX_TRUSTED_PEER_REPUTATION_CHANGE`），避免单次错误造成永久封禁。

### 连接槽位管理

```
入站槽位（inbound）：接受远端发来的连接
出站槽位（outbound）：主动连接远端的连接

refill_slots_interval 触发时：
  若出站槽位有空余，从 peers 中选取可连接节点，发出 PeerAction::Connect
```

### 指数退避

连接失败（非致命原因，如 TooManyPeers）的 peer 进入退避队列，等待一段时间后重试。退避时间随失败次数增长（`PeerBackoffDurations`），超过 `max_backoff_count` 则永久丢弃该 peer。

---

## 9. 交易传播：TransactionsManager

### 交易传播策略

当新交易进入本地交易池后，`TransactionsManager` 决定如何向网络广播：

**传播策略（TransactionPropagationMode）**：

```rust
// crates/net/network/src/transactions/config.rs:49

pub enum TransactionPropagationMode {
    Sqrt,       // 发完整交易给 sqrt(peer数量) 个 peer，其余只发哈希
    All,        // 向所有 peer 发完整交易
    Max(usize), // 向最多 N 个 peer 发完整交易
}

// 默认：Sqrt（以太坊规范推荐）
```

为什么用 Sqrt？这是一种优化：
- 完整交易有开销（可能几 KB），发给所有 peer 浪费带宽
- 只发哈希（几十字节），接收方按需拉取
- 对 `sqrt(n)` 个 peer 发完整交易，确保交易快速扩散（指数传播），其余 peer 通过 hash announcement 来拉取

**Ingress 策略（TransactionIngressPolicy）**：

```rust
pub enum TransactionIngressPolicy {
    All,        // 接受所有 peer 的交易（默认）
    Trusted,    // 只接受信任 peer 的交易
    None,       // 拒绝所有入站交易
}
```

这允许节点配置为"只接受白名单交易源"，用于私有 mempool 或 MEV 节点。

### 交易发送（发出方视角）

新交易入池时：

```
1. TransactionsHandle::broadcast_transactions(txs) 被调用
      ↓
2. 发 TransactionsCommand::BroadcastTransactions 到 TransactionsManager
      ↓
3. TransactionsManager.on_new_transactions(txs):
   a. 对每个 peer，检查 TransactionPropagationPolicy（是否允许传播）
   b. 按 propagation_mode 决定哪些 peer 收到完整交易
   c. 向"完整传播" peer：NetworkHandle::send_transactions(peer_id, txs)
   d. 向"哈希通告" peer：NetworkHandle::send_transactions_hashes(peer_id, hashes)
      ↓
4. NetworkManager 路由到对应 peer 的 ActiveSession
      ↓
5. Session 编码并通过 RLPX 流发送出去
```

### 交易接收与拉取（接收方视角）

收到 `NewPooledTransactionHashes68` 通告：

```
1. TransactionFetcher.on_new_pooled_transaction_hashes(hashes, peer_id)
      ↓
2. 过滤：跳过 pool 中已有的哈希
      ↓
3. 将新哈希打包进 GetPooledTransactions 请求（软限制 2 MiB）
      ↓
4. 发送 GetPooledTransactions 到 peer，等待 PooledTransactions 响应
      ↓
5. 收到 PooledTransactions → 验证 → 提交到本地交易池
```

### TransactionFetcher 并发控制

```rust
// 并发限制（crates/net/network/src/transactions/config.rs）
pub struct TransactionFetcherConfig {
    max_inflight_requests: u32,              // 全局最大并发 GetPooledTransactions 数
    max_inflight_requests_per_peer: u8,      // 每个 peer 最大并发请求数
    soft_limit_byte_size_pooled_transactions_response: usize,  // 单次响应软限制（2 MiB）
    max_capacity_cache_txns_pending_fetch: u32,  // 待拉取哈希缓存上限
}
```

未被发送的哈希缓存在 `hashes_pending_fetch`（LRU 缓存），等待有空闲 peer 时再发送。

### ETH/68 vs ETH/66 通告格式对比

| 协议 | 通告消息 | 内容 |
|------|----------|------|
| ETH/66/67 | `NewPooledTransactionHashes66` | `Vec<TxHash>` 仅哈希 |
| ETH/68+ | `NewPooledTransactionHashes68` | `(types, sizes, hashes)` 三元组 |

ETH/68 的优势：接收方可以根据 `type` 过滤（如不支持 blob 交易的节点可跳过 type=3），根据 `size` 决定优先级（先拉小交易），而无需先拉完整交易。

---

## 10. 区块传播

### PoS 时代的区块传播

**合并后（EIP-3675）**，新区块通过 **Engine API** 从 CL（共识层）传入，不再通过 devp2p 广播。所以：

```rust
// NetworkHandle::announce_block —— PoS 下是 noop
pub fn announce_block(&self, block: N::NewBlockPayload, hash: B256) {
    if self.inner.network_mode.is_pow() {
        // PoW：通过 devp2p 广播
        ...
    }
    // PoS：什么都不做
}
```

**但**，区块仍然可以通过 devp2p 被请求（`GetBlockHeaders`/`GetBlockBodies`），用于历史同步（其他节点追历史）。传播路径改变了，但查询路径保留了。

### PoW 时代的区块传播（历史参考）

```
节点接收到新的有效区块
    ↓
NetworkHandle::announce_block(block_payload, hash)
    ↓
NetworkHandleMessage::AnnounceBlock 发给 NetworkManager
    ↓
NetworkManager 路由到 Swarm 的所有 active sessions
    ↓
向所有 peer 发送 NewBlock { block, total_difficulty }
（或发 NewBlockHashes，接收方按需请求完整区块）
```

### 区块请求处理（EthRequestHandler）

`EthRequestHandler` 是独立任务，处理来自 peer 的区块数据请求：

```
peer 发来 GetBlockHeaders(request_id, start_hash, count, skip, reverse)
    ↓
EthRequestHandler 查询本地数据库
    ↓
响应 BlockHeaders(request_id, headers)
```

这是历史同步（Pipeline 的 Headers/Bodies stage）的数据来源——从已同步的节点拉取。

---

## 11. NetworkHandle：对外公共接口

### 设计：前端-后端分离

`NetworkHandle` 是 `NetworkManager` 的轻量前端，可自由克隆和跨线程共享：

```rust
// crates/net/network/src/network.rs:44
pub struct NetworkHandle<N: NetworkPrimitives = EthNetworkPrimitives> {
    inner: Arc<NetworkInner<N>>,
}

struct NetworkInner<N: NetworkPrimitives> {
    num_active_peers: Arc<AtomicUsize>,
    to_manager_tx: UnboundedSender<NetworkHandleMessage<N>>,  // 命令 channel
    secret_key: SecretKey,
    local_peer_id: PeerId,
    peers: PeersHandle,
    network_mode: NetworkMode,  // PoW 或 PoS
    is_syncing: Arc<AtomicBool>,
    initial_sync_done: Arc<AtomicBool>,
    chain_id: Arc<AtomicU64>,
    tx_gossip_disabled: bool,
    discv4: Option<Discv4>,
    discv5: Option<Discv5>,
    event_sender: EventSender<NetworkEvent<PeerRequest<N>>>,
    nat: Option<NatResolver>,
}
```

`NetworkHandle` 通过 `UnboundedSender<NetworkHandleMessage>` 向 `NetworkManager` 发送命令，所有实际操作异步在 NetworkManager 的 poll 循环中执行。

### NetworkHandleMessage 命令枚举

```rust
// crates/net/network/src/network.rs:500

pub(crate) enum NetworkHandleMessage<N: NetworkPrimitives> {
    // Peer 管理
    AddTrustedPeerId(PeerId),
    AddPeerAddress(PeerId, PeerKind, PeerAddr),
    RemovePeer(PeerId, PeerKind),
    DisconnectPeer(PeerId, Option<DisconnectReason>),

    // 区块与交易
    AnnounceBlock(N::NewBlockPayload, B256),
    SendTransaction { peer_id, msg },
    SendPooledTransactionHashes { peer_id, msg },

    // 状态更新
    StatusUpdate { head },        // 更新本节点已知最新区块
    ReputationChange(PeerId, ReputationChangeKind),

    // 查询
    GetStatus(oneshot::Sender<NetworkStatus>),
    GetPeerInfos(tx),
    GetPeerInfoById(PeerId, tx),

    // 配置
    SetNetworkState(NetworkConnectionState),  // Active / Hibernate
    AddRlpxSubProtocol(RlpxSubProtocol),
    InternalBlockRangeUpdate(BlockRangeUpdate),

    // 生命周期
    Shutdown(oneshot::Sender<()>),
    ...
}
```

### 与其他组件集成

| 组件 | 如何使用 NetworkHandle |
|------|------------------------|
| **Engine API** | `handle.update_status(head)` 更新已知最新区块 |
| **交易池** | `transactions_handle().broadcast_transactions(txs)` 广播新交易 |
| **Pipeline/同步** | 通过 `FetchClient`（实现 `HeadersClient`/`BodiesClient`）请求历史区块 |
| **RPC** | `handle.peer_infos()` 提供 `admin_peers` 等接口的数据 |
| **节点管理** | `handle.add_peer()`, `handle.remove_peer()` 动态管理 peer |

---

## 12. 一笔交易的完整传播路径

以一笔来自 DApp 的 `eth_sendRawTransaction` 为例，追踪其在 P2P 网络中的传播：

```
【发送方节点 A】

1. RPC 接收 eth_sendRawTransaction
      ↓
2. 解码 RLP，验证签名
      ↓
3. EthApi::send_raw_transaction()
      ↓
4. transaction_pool.add_transaction(Local, tx)
      ↓
5. 验证通过，tx 进入 PendingPool
      ↓
6. Pool 通知 TransactionsManager：新交易 [tx_hash]
      ↓
7. TransactionsManager.on_new_transactions([tx])：
   - 检查 40 个已连接 peer
   - Sqrt(40) ≈ 6 个 peer → 发送完整交易 (Transactions 消息)
   - 其余 34 个 peer → 发送哈希通告 (NewPooledTransactionHashes68 消息)
      ↓
8. NetworkHandle 路由到各 peer 的 ActiveSession
      ↓
9. Session 编码为 RLPX 帧，通过 TCP 发出

【接收方节点 B，收到完整交易】

10. ActiveSession 解码 Transactions 消息
       ↓
11. NetworkManager → TransactionsManager: NetworkTransactionEvent::IncomingTransactions
       ↓
12. 验证、提交到本地交易池
       ↓
13. 节点 B 再次向自己的 peer 传播（哈希通告）

【接收方节点 C，收到哈希通告】

10. 收到 NewPooledTransactionHashes68
       ↓
11. TransactionFetcher.on_new_pooled_transaction_hashes
       ↓
12. 发送 GetPooledTransactions([tx_hash]) 给节点 A
       ↓
13. 节点 A 响应 PooledTransactions([tx])
       ↓
14. 节点 C 验证、入池、继续传播
```

整个过程中，交易大约需要 **1-3 秒**在全球主网上传播到 95% 的节点（取决于 peer 数量和网络延迟）。

---

## 13. 关键设计决策

### 为什么 NetworkManager 是单个无限 Future，而不是多线程？

所有网络状态（peers 表、sessions 表、pending 状态）集中在 `NetworkManager` 内部，通过 channel 与外部通信，避免了锁竞争。这是经典的 **Actor 模式**：
- 零锁（`NetworkManager` 自身不需要 Mutex）
- 消息有序处理（channel 保证顺序）
- 易于测试和调试（状态集中在一处）

外部通过克隆 `NetworkHandle` 发命令，NetworkManager 的 poll 循环单线程处理所有状态变化。

### 为什么发现协议用 UDP，TCP 连接用另一套？

发现协议的特点：
- 需要向大量节点快速探测（Ping/Pong），无需可靠传输
- 单个探测包很小（< 1280 字节）
- 可接受丢包（Kademlia 的冗余查找会补偿）

TCP 的特点：
- 需要可靠、有序传输 RLP 消息
- 消息可能很大（10 MB 限制）
- 需要长连接维护会话状态

两者解耦使得发现可以无限制地探索更多节点，而 TCP 连接只维护少量高质量的长连接（典型值：25-50 个 peer）。

### ETH/68 的 `(type, size, hash)` 三元组：降低带宽还是保护自己？

主要目标是**过滤**，不只是带宽优化：
- 不支持某类型的节点（如旧节点不支持 blob）可以直接跳过，不用拉取后再丢弃
- 大交易（如 blob）的 size 信息让接收方可以有选择地拉取，防止带宽被大交易打满
- type + size 也帮助检测异常：size 异常大的交易可以直接 reject 不拉

### ForkId（EIP-2124）的防御效果

没有 ForkId 的问题：主网节点和测试网节点会浪费连接互相握手，然后发现数据不兼容。有了 ForkId：
- 在 **Status 交换阶段**（握手的最后一步）就能检测出不兼容
- 后续分叉（如 Dencun）激活后，未升级节点会被自动断开
- 节点可以通过 ENR 中的 ForkId 在 **发现阶段**就过滤不兼容节点

### 为什么 `EthRequestHandler` 用 bounded channel？

```rust
// manager.rs 注释：
// 即便非恶意请求处理成本低，攻击者也可以构造接近最大消息大小的请求来
// 消耗处理能力。因此使用 bounded channel，防止无界积累。
```

`TransactionsManager` 用 unbounded channel（交易通常小且处理快），而 `EthRequestHandler` 处理区块头/体请求（IO 密集、可能很慢），需要背压保护。

---

## 本章总结

| 组件 | 职责 | 实现文件 |
|------|------|----------|
| `Discv4/Discv5` | 节点发现（Kademlia DHT） | `crates/net/discv4/src/lib.rs` |
| `NetworkManager` | 网络总调度（无限 Future） | `crates/net/network/src/manager.rs` |
| `Swarm` | 连接状态管理 | `crates/net/network/src/swarm.rs` |
| `SessionManager` | RLPX 握手与会话生命周期 | `crates/net/network/src/session/mod.rs` |
| `PeersManager` | Peer 声誉与连接槽位 | `crates/net/network/src/peers.rs` |
| `TransactionsManager` | 交易传播与拉取 | `crates/net/network/src/transactions/` |
| `EthRequestHandler` | 区块数据请求服务 | `crates/net/network/src/eth_requests.rs` |
| `NetworkHandle` | 公共接口（可克隆前端） | `crates/net/network/src/network.rs` |

**关键代码路径：**
```
crates/net/
├── network/src/
│   ├── manager.rs            # NetworkManager（总调度，无限 Future）
│   ├── swarm.rs              # Swarm（连接状态）
│   ├── network.rs            # NetworkHandle + NetworkHandleMessage
│   ├── session/mod.rs        # SessionManager（RLPX 握手）
│   ├── peers.rs              # PeersManager（声誉系统）
│   └── transactions/
│       ├── mod.rs            # TransactionsManager
│       ├── fetcher.rs        # TransactionFetcher（拉取逻辑）
│       └── config.rs         # 传播策略配置
├── eth-wire-types/src/
│   ├── message.rs            # EthMessageID + EthMessage 枚举
│   ├── version.rs            # EthVersion（66-72，当前默认启用到 69）
│   └── broadcast.rs          # NewBlock, NewBlockHashes, Transactions
└── discv4/src/lib.rs         # Discv4 + Kademlia 实现
```

---

**下一章：** [第8章：Payload 构建与 MEV](08-payload-building.md)

当节点是 block proposer 时，如何从交易池选取交易、组装区块、以及 MEV-boost 如何接入？
