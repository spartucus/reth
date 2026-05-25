# Reth 学习路径：从合约开发到以太坊底层

> 适用于已有合约开发经验，想深入理解以太坊底层实现的开发者

## 学习目标

通过学习 reth 的设计和实现，深入理解：
- 以太坊执行层的完整架构
- 从交易提交到状态确认的全流程
- 区块链节点的核心机制和性能优化
- 实际生产环境中的工程实践

## 前置知识检查

- ✅ Solidity 合约开发经验
- ✅ 基本的以太坊概念（账户、交易、Gas、区块）
- ✅ Web3 库使用经验（ethers.js / web3.js）
- 🔲 Rust 基础（建议先学习，但可边学边用）

---

## 章节总览

| 章节 | 主题 | 文件 | 状态 |
|------|------|------|------|
| 第0章 | 整体架构鸟瞰 | [00-high-view.md](00-high-view.md) | ✅ |
| 第1章 | 交易执行 | [01-transaction-execution.md](01-transaction-execution.md) | ✅ |
| 第2章 | 存储架构 | [02-storage-architecture.md](02-storage-architecture.md) | ✅ |
| 第3章 | Pipeline 与区块链同步 | [03-pipeline-and-sync.md](03-pipeline-and-sync.md) | ✅ |
| 第4章 | Engine API 与共识引擎 | [04-engine-api-and-consensus.md](04-engine-api-and-consensus.md) | ✅ |
| 第5章 | 交易池 | [05-transaction-pool.md](05-transaction-pool.md) | ✅ |
| 第6章 | RPC 层 | [06-rpc.md](06-rpc.md) | ✅ |
| 第7章 | 网络层与 P2P 协议 | [07-networking.md](07-networking.md) | ✅ |
| 第8章 | Payload 构建与 MEV | [08-payload-building.md](08-payload-building.md) | ✅ |
| 第9章 | 并发与异步 | [09-concurrency-and-async.md](09-concurrency-and-async.md) | ✅ |
| 附录 A | 节点启动流程 | [appendix-node-startup.md](appendix-node-startup.md) | ✅ |
| 附录 B | 有趣的设计决策 | [interesting-designs.md](interesting-designs.md) | ✅ |

---

## 阶段一：核心执行链路（1-2 周）

### 目标：理解一笔交易从提交到执行的完整路径

### 第0章：整体架构鸟瞰（半天）

**📖 学习文档：** [00-high-view.md](00-high-view.md)

在深入细节之前，先建立对 reth 整体的认知地图：主要组件、它们的职责边界、以及组件间的数据流向。

**检验点：** 能画出 reth 的顶层架构图，说出每个模块的职责

---

### 第1章：交易执行（3-4 天）

**核心问题：** 当你调用 `contract.transfer()` 时，节点内部发生了什么？

**📖 学习文档：** [01-transaction-execution.md](01-transaction-execution.md)

**关键代码路径：**
```
crates/evm/                    # EVM 执行引擎
├── src/execute.rs            # 交易执行入口
└── src/lib.rs                # EVM 配置

crates/ethereum/evm/          # 以太坊特定 EVM 配置
└── src/lib.rs
```

**动手实验：**
1. 在 `execute.rs` 中添加 tracing 日志，观察交易执行流程
2. 追踪一个简单的 ETH 转账的完整执行路径
3. 理解 Gas 计算与退还的时机

**核心概念：**
- `Transaction` → 执行 → `ExecutionResult`
- State transition function
- Gas 计算与退还
- Receipt 生成

**检验点：** 能画出一笔交易从进入节点到生成 Receipt 的完整流程图

---

### 第2章：存储架构（3-4 天）

**核心问题：** 合约的状态变量存储在哪里？State Trie 如何存储和计算？

**📖 学习文档：** [02-storage-architecture.md](02-storage-architecture.md)

**关键代码路径：**
```
crates/storage/
├── db-api/src/tables/        # 数据库表定义
├── provider/                 # 状态访问接口
└── nippy-jar/                # Static Files 格式

crates/trie/
├── trie/src/trie.rs         # StateRoot 计算
└── db/src/                   # Trie 游标
```

**关键文件：**
- `crates/storage/db-api/src/tables/mod.rs` — 所有表定义（PlainAccountState, HashedAccounts, AccountsTrie）
- `crates/trie/trie/src/trie.rs` — StateRoot 增量计算算法
- `crates/storage/nippy-jar/src/lib.rs` — NippyJar 列式存储格式

**动手实验：**
1. 查看数据库表大小和 Static Files 目录
2. 观察 mmap 的内存使用（VSZ vs RSS）
3. 理解 state_root 的增量计算

**核心概念：**
- 三层存储：MDBX（热数据）+ Static Files（冷数据）
- 数据冗余：PlainAccountState → HashedAccounts → AccountsTrie
- State Trie 增量更新（只重新计算变化的分支）
- NippyJar 列式压缩存储
- Memory-mapped I/O

**检验点：**
- 能画出从 PlainAccountState 到 state_root 的完整数据流
- 能解释为什么 8GB 内存可以运行 1.4TB 数据的全节点

---

### 第3章：Pipeline 与区块链同步（4-5 天）

**核心问题：** 新节点如何从创世块同步到最新状态？

**📖 学习文档：** [03-pipeline-and-sync.md](03-pipeline-and-sync.md)

**关键代码路径：**
```
crates/stages/
├── stages/src/stages/
│   ├── headers.rs            # 区块头下载
│   ├── bodies.rs             # 区块体下载
│   ├── execution.rs          # 执行阶段
│   └── merkle.rs             # 状态根验证
└── api/src/pipeline.rs       # Pipeline 协调器

crates/engine/tree/src/       # Engine 区块树（实时同步）
```

**核心概念：**
- Staged Sync 架构（Pipeline）
- DefaultStages：Era（可选）→ Headers → Bodies → SenderRecovery → Execution → AccountHashing → StorageHashing → Merkle → TransactionLookup → IndexStorage → IndexAccount → Finish
- Unwind 机制（回滚）
- Pipeline（历史同步）vs Engine API（实时同步）的协作关系

**检验点：**
- 能画出完整的 Pipeline 架构图，标注所有 Stage 的职责
- 能解释 Unwind 机制的工作原理

---

## 阶段二：在线运行机制（3-4 周）

### 目标：理解节点在实时跟随链头时的工作方式，以及对外暴露的接口

### 第4章：Engine API 与共识引擎（4-5 天）

**核心问题：** 节点如何与共识层（信标链）通信？收到新 payload 后如何处理？

**📖 学习文档：** [04-engine-api-and-consensus.md](04-engine-api-and-consensus.md)

**关键代码路径：**
```
crates/engine/tree/src/
├── tree/mod.rs               # 核心引擎树（主逻辑）
├── engine.rs                 # EngineHandler / EngineApiRequestHandler
└── backfill.rs               # 回填同步控制

crates/consensus/common/src/
└── validation.rs             # 区块验证规则

crates/rpc/rpc-engine-api/src/
└── engine_api.rs             # Engine JSON-RPC 端点
```

**核心概念：**
- Engine API：当前实现覆盖 `newPayloadV1-V5`、`forkchoiceUpdatedV1-V4`、`getPayloadV1-V6`；V3 是 Cancun/blob 主路径，V4+ 覆盖 Prague/Amsterdam/Osaka 相关字段
- 四层架构：RPC Handler → EngineHandler → EngineApiRequestHandler → EngineTree
- BackfillSyncState（历史同步状态对实时处理的影响）
- 四个子池：Pending、BaseFee、Blob、Queued
- in-memory canonical chain（相对于 Pipeline 的持久化链）

**检验点：**
- 能追踪一个 `engine_newPayloadV3` 调用从 RPC 到入库的完整路径
- 能解释 forkchoice 更新触发的四个阶段

---

### 第5章：交易池（4-5 天）

**核心问题：** 交易如何在等待打包期间被管理、排序、选取？

**📖 学习文档：** [05-transaction-pool.md](05-transaction-pool.md)

**关键代码路径：**
```
crates/transaction-pool/src/
├── pool/mod.rs               # PoolInner：核心逻辑
├── pool/txpool.rs            # TxPool：四个子池
├── pool/state.rs             # TxState bitflags、SubPool 枚举
├── pool/pending.rs           # PendingPool（待打包）
├── pool/best.rs              # BestTransactions 迭代器
├── validate/mod.rs           # 验证结果类型
├── validate/eth.rs           # 以太坊交易验证器
├── ordering.rs               # CoinbaseTipOrdering
└── identifier.rs             # SenderId、TransactionId
```

**核心概念：**
- `Pool → PoolInner → TxPool → 4 subpools` 三层结构
- `TxState` bitflags 决定子池归属（状态驱动分类）
- `BestTransactions` 迭代器：快照 + 独立集 + 失效传播
- `on_canonical_state_change`：5 步更新流程
- blob sidecar 独立存储于 `BlobStore`

**检验点：**
- 能解释一笔 EIP-1559 交易在什么条件下从 BaseFee 子池晋升到 Pending 子池
- 能说明 `BestTransactions` 如何保证同一发送方的 nonce 严格递增

---

### 第6章：RPC 层

**核心问题：** `eth_call`、`eth_sendRawTransaction`、`debug_traceTransaction` 是如何实现的？

**📖 学习文档：** [06-rpc.md](06-rpc.md)

**关键代码路径：**
```
crates/rpc/
├── rpc-builder/src/lib.rs        # RpcModuleBuilder / RpcServerConfig
├── rpc-server-types/src/module.rs # RethRpcModule 枚举
├── rpc-eth-api/src/
│   ├── core.rs                   # EthApi trait（rpc 宏）
│   ├── helpers/mod.rs            # FullEthApi 聚合 trait
│   └── helpers/blocking_task.rs  # SpawnBlocking 两类任务池
├── rpc-eth-types/src/cache/      # EthStateCache（LRU actor）
├── rpc-layer/src/auth_layer.rs   # JWT 鉴权中间件
└── rpc-engine-api/               # Engine API（已在第4章覆盖）
```

**核心概念：**
- `RpcModuleBuilder` + `RpcServerConfig` 两个 builder 的分工
- `RethRpcModule` 枚举：按传输独立启用命名空间
- `FullEthApi` = 8 个 Eth* trait 的聚合（Load 层 + Eth 层 + Server 层）
- `SpawnBlocking`：IO 池（eth_call）vs Tracing 池（debug_trace*），独立信号量
- `EthStateCache`：Actor 模式 + MultiConsumerLruCache 合并重复请求
- Auth 服务器（8551）：JWT 鉴权，Engine API 专用通道

**检验点：**
- 能解释 `eth_call` 为什么需要 `spawn_blocking_io`，而 `eth_sendRawTransaction` 不需要
- 能画出一次 `eth_call` 请求从 HTTP 到 EVM 执行再到返回的完整路径

---

### 第7章：网络层与 P2P 协议

**核心问题：** 节点如何发现彼此？交易和区块如何在网络中传播？

**📖 学习文档：** [07-networking.md](07-networking.md)

**关键代码路径：**
```
crates/net/
├── network/src/
│   ├── manager.rs            # NetworkManager（总调度，无限 Future）
│   ├── swarm.rs              # Swarm（连接状态管理）
│   ├── network.rs            # NetworkHandle + NetworkHandleMessage
│   ├── session/mod.rs        # SessionManager（RLPX 握手全流程）
│   ├── peers.rs              # PeersManager（声誉系统）
│   └── transactions/
│       ├── mod.rs            # TransactionsManager（交易传播）
│       ├── fetcher.rs        # TransactionFetcher（拉取逻辑）
│       └── config.rs         # 传播策略（Sqrt/All/Max）
├── eth-wire-types/src/
│   ├── message.rs            # EthMessageID + 所有消息类型
│   └── version.rs            # EthVersion（66-72，当前默认启用到 69）
└── discv4/src/lib.rs         # Discv4 + Kademlia 实现
```

**核心概念：**
- DevP2P 协议栈：UDP发现（Discv4）+ TCP（RLPX）分离设计
- RLPX 握手三阶段：ECIES 加密 → Hello/capability 协商 → eth Status（ForkId 验证）
- ETH/68：`NewPooledTransactionHashes68` = `(types, sizes, hashes)` 三元组
- ETH/69：Status 去掉 total_difficulty + 新增 `BlockRangeUpdate`
- 交易传播：`TransactionPropagationMode::Sqrt`（完整交易给 √N 个 peer，其余发哈希）
- `PeersManager`：声誉数值（`Reputation = i32`），封禁阈值 = `50 * (-1024)`
- `NetworkManager` Actor 模式：零锁、单 Future、channel 通信

**检验点：**
- 能说清楚 ETH/68 的 `(types, sizes, hashes)` 格式有什么优势
- 能追踪一笔交易从 `eth_sendRawTransaction` 到传播给 peer 的完整路径
- 能解释为什么发现协议用 UDP 而数据传输用 TCP

---

## 阶段三：区块产出与系统深度（2-3 周）

### 目标：理解 reth 如何组装新区块，以及支撑高性能的底层机制

### 第8章：Payload 构建与 MEV

**核心问题：** 区块构建器如何从交易池选取交易、组装区块？MEV-boost 如何接入？

**📖 学习文档：** [08-payload-building.md](08-payload-building.md)

**关键代码路径：**
```
crates/payload/
├── builder/src/
│   ├── traits.rs            # PayloadJob + PayloadJobGenerator + PayloadBuilder trait
│   └── service.rs           # PayloadBuilderService（无限 Future 编排器）
├── primitives/src/          # PayloadId 生成（SHA256 of payloadAttributes）
└── basic/src/lib.rs         # BasicPayloadJobGenerator + BasicPayloadJob（1s轮询，12s截止）

crates/ethereum/payload/src/lib.rs   # EthereumPayloadBuilder + 贪心选交易算法
crates/ethereum/engine-primitives/src/payload.rs  # EthBuiltPayload 结构 + V1-V6 转换
crates/rpc/rpc-api/src/mev.rs        # mev_sendBundle / mev_simBundle 接口定义
```

**核心概念：**
- `FCU(payloadAttributes)` → `PayloadBuilderService` 创建 `BasicPayloadJob` → 持续轮询（1s/次，12s 截止）
- `BasicPayloadJob`：同时是 Future（持续构建）+ 可查询（best_payload/resolve_kind）
- `EthereumPayloadBuilder` 贪心算法：按 `effective_tip_per_gas` 降序，逐笔执行检查 gas/RLP/blob 上限
- `CachedReads`：构建轮次间复用磁盘读缓存，`PrecachedState`：提前缓存新 head 状态供下一 slot 使用
- `PayloadTaskGuard`：限制并发 payload 构建任务；`CancelOnDrop`：job 结束时取消后台任务
- MEV-boost：外部构建器（Builder API），reth 提供 `mev_sendBundle`/`mev_simBundle`，无内置 relay
- `EthBuiltPayload`：含 `id, block, fees(U256), sidecars(Empty/Eip4844/Eip7594), requests(Prague+)`
- `PayloadId`：8 字节 = `SHA256(parentHash‖timestamp‖prevRandao‖feeRecipient‖withdrawals‖parentBeaconBlockRoot)[0..8]`

**检验点：**
- 能说清楚 `forkchoiceUpdated` 到 `getPayload` 之间 payload 经历的完整构建周期
- 能解释为什么 payload builder 必须始终返回一个有效 payload（哪怕是空块）
- 能说明 `CachedReads` 在多轮构建中节省了什么开销

---

### 第9章：并发与异步

**核心问题：** reth 如何利用多核和异步 I/O 达到高性能？各种并发模式分别用在哪里？

**📖 学习文档：** [09-concurrency-and-async.md](09-concurrency-and-async.md)

**关键代码路径：**
```
crates/tasks/src/
├── runtime.rs       # Runtime / RuntimeBuilder：tokio + rayon + task spawning
├── lib.rs           # TaskManager（Future，监控 critical panic）+ TaskExecutor alias
├── shutdown.rs      # Signal / Shutdown / GracefulShutdown / GracefulShutdownGuard
└── pool.rs          # BlockingTaskPool（rayon 封装）+ BlockingTaskGuard（信号量限流）

crates/tasks/src/runtime.rs         # RuntimeBuilder / TokioConfig：DEFAULT_THREAD_KEEP_ALIVE=15s
crates/trie/parallel/src/root.rs    # ParallelStateRoot：并行 storage root 计算
crates/rpc/rpc-eth-api/src/helpers/blocking_task.rs  # SpawnBlocking：IO vs Tracing 两池分工
```

**核心概念：**
- tokio multi-thread runtime：`DEFAULT_THREAD_KEEP_ALIVE = 15s`（> 12s slot，避免每块重建线程）
- `Runtime`（cloneable handle）统一封装 tokio handle、critical task 监控、graceful shutdown、rayon pools；`TaskExecutor` 是 `Runtime` 的 alias
- 普通任务 = `select(on_shutdown, fut)`；关键任务 = `catch_unwind` + `TaskEvent::Panic` 上报
- `Signal` / `Shutdown(Shared<oneshot>)` / `GracefulShutdownGuard(Arc<AtomicUsize>)`
- 两类阻塞：tokio blocking pool（I/O 密集）vs rayon `BlockingTaskPool`（CPU 密集）
- `BlockingTaskPool::spawn(f)` → rayon 线程 + `oneshot` 桥接 → `BlockingTaskHandle<R: Future>`；`Runtime` 还维护 cpu/rpc/storage/named worker pools
- Channel 选型：`oneshot`（单次结果）/ `mpsc unbounded`（命令）/ `mpsc bounded`（防 DoS）/ `std::sync::mpsc::sync_channel(1)`（同步上下文结果传递）
- `parking_lot::RwLock`（同步热路径）vs `tokio::sync::Semaphore`（async 限流）
- `tokio::select!` 三种模式：主任务监控 / 信号监听 / Actor 消息循环
- 并行状态根：主线程遍历账户 trie + N 个 `spawn_blocking` 并行计算 storage root

**检验点：**
- 能说清楚 `spawn_blocking` 和 `BlockingTaskPool::spawn` 分别用在什么场景，为什么不能混用
- 能画出 `Runtime` / `TaskManager` / `GracefulShutdownGuard` 三者的协作关系
- 能解释 `ParallelStateRoot` 中为什么用 `std::sync::mpsc::sync_channel` 而非 tokio channel

---

## 附录

### 附录 A：节点启动流程

**📖 文档：** [appendix-node-startup.md](appendix-node-startup.md)

从 `reth node` 命令到各组件全部就绪的启动序列：CLI 解析、配置加载、数据库初始化、各子系统启动顺序。

### 附录 B：有趣的设计决策

**📖 文档：** [interesting-designs.md](interesting-designs.md)

reth 中值得关注的非显而易见的设计：为什么这样而不是那样？记录了若干架构决策背后的权衡。

---

## 实践建议

### 代码阅读三步法

```
第一步：模块级理解
- 阅读 src/lib.rs 和模块顶部注释
- 理解模块的职责边界
- 识别核心类型和 trait

第二步：接口级理解
- 找到主要的 public API
- 阅读文档注释（//! 和 ///）
- 查看使用示例（tests、examples）

第三步：实现级理解
- 深入关键函数实现
- 追踪数据流
- 理解边界条件处理
```

### 启动开发环境

```bash
# 编译
cargo build --release

# 启动开发节点（自动出块，无需共识层）
cargo run --release -- node --dev

# 启动测试网节点
cargo run --release -- node --chain sepolia
```

### 添加调试日志

```rust
use tracing::{debug, info};

info!(target: "my-learning", ?transaction, "处理交易");
debug!(target: "my-learning", state_root = ?root, "计算状态根");
```

启动时加 `RUST_LOG=my-learning=debug` 环境变量即可看到输出。

### 运行测试

```bash
# 运行特定 crate 的测试
cargo nextest run -p reth-transaction-pool
cargo nextest run -p reth-engine-tree

# 运行单个测试
cargo nextest run -p reth-transaction-pool -- pool::tests::test_tx_pending
```

---

## 里程碑检验

### 阶段一完成标志

✅ **能够回答：**
- 一笔转账交易从进入节点到执行完成经历了哪些步骤？
- 合约的状态变量存储在数据库的什么表？
- 为什么 reth 同时使用 MDBX 和 Static Files？
- Staged Sync 的各个 Stage 分别负责什么？

✅ **能够完成：**
- 在执行路径上添加自定义日志并观察到输出
- 说清楚 Pipeline 和 Engine API 分别负责什么场景的同步

---

### 阶段二完成标志

✅ **能够回答：**
- `engine_newPayloadV3` 调用后，payload 经历了哪些处理步骤？
- 一笔 EIP-1559 交易会落入哪个子池？在什么条件下晋升？
- `BestTransactions` 如何保证 nonce 有序且无空缺？

✅ **能够完成：**
- 画出 Engine API 四层架构图
- 说清楚交易池的 `TxState` bitflags 机制

---

### 阶段三完成标志

✅ **能够回答：**
- Payload builder 如何在 gas 限制内选取收益最大的交易集合？
- MEV-boost 的外部构建器模式如何接入 reth？

---

## 进阶方向

完成以上三个阶段后，可以选择深入的方向：

1. **EVM 优化**：研究 revm 内部，优化操作码执行，实现新的 EIP
2. **存储系统**：深入 MDBX，优化数据库模式，并行 state root 计算
3. **网络协议**：实现新的 ETH 协议版本，优化交易/区块传播
4. **MEV 基础设施**：构建或优化 block builder，研究 PBS（Proposer-Builder Separation）

### 贡献方向

- 修复 Issues（从 `good-first-issue` 开始）
- 改进文档和注释
- 添加测试用例
- 性能优化
- 实现新的 EIP

---

## 学习资源

1. **项目文档**
   - [CLAUDE.md](../../CLAUDE.md) — 架构概览（必读）
   - 各模块的 `//!` 顶部注释

2. **外部资源**
   - [Ethereum Yellow Paper](https://ethereum.github.io/yellowpaper/paper.pdf) — 规范
   - [Paradigm 博客](https://www.paradigm.xyz/writing) — reth 设计思路
   - [Engine API 规范](https://github.com/ethereum/execution-apis/tree/main/src/engine) — 官方 Engine API

3. **代码工具**
   ```bash
   # 生成并打开文档
   cargo doc --document-private-items --open

   # 搜索符号定义
   rg "struct TxState" --type rust
   rg "fn on_canonical_state_change" --type rust
   ```

4. **社区资源**
   - [Reth GitHub Discussions](https://github.com/paradigmxyz/reth/discussions)
   - [Telegram 群组](https://t.me/paradigm_reth)

---

## 最后的建议

1. **保持耐心** — 理解一个生产级系统需要时间
2. **动手实践** — 不要只是阅读，要修改和实验
3. **验证文档** — 发现文档与代码不符时，以代码为准并提 issue
4. **循序渐进** — 不要试图一次理解所有东西

祝学习顺利！
