# 第零章：全局视角 —— 一笔交易的完整旅程

> 目标：从用户发出交易，到交易被永久写入区块链，理解每个环节做了什么。
> 覆盖范围：共识层（CL）+ 执行层（EL）协作、EL 内部并发架构、高性能 EVM 链对比。

## 学习路径导航

- **[第一章：交易执行流程](./01-transaction-execution.md)** - 交易执行、Receipt 生成、区块组装
- **[第二章：存储架构](./02-storage-architecture.md)** - State Trie 存储、StaticFiles、mmap 技术

---

## 一、以太坊后 Merge 的两层架构

The Merge（2022年9月）将以太坊拆成两个独立软件，通过 **Engine API** 协作：

```
┌─────────────────────────────────────────────────────────────────┐
│                       共识层 (CL)                                │
│         Lighthouse / Prysm / Teku / Nimbus                      │
│                                                                  │
│  职责：谁来出块、验证者投票、最终确认（Finality）                  │
│  共识：PoS（验证者质押 32 ETH，RANDAO 随机选出块者）               │
│  网络：libp2p                                                    │
└──────────────────────┬──────────────────────────────────────────┘
                       │  Engine API（JWT 认证 HTTP，可跨机器）
                       │  engine_newPayloadV3
                       │  engine_forkchoiceUpdatedV3
                       │  engine_getPayloadV3
┌──────────────────────┴──────────────────────────────────────────┐
│                       执行层 (EL)                                │
│              Reth / Geth / Erigon / Nethermind                  │
│                                                                  │
│  职责：执行交易、管理状态、维护 mempool                           │
│  网络：DevP2P（交易 gossip、历史数据同步）                        │
│  接口：JSON-RPC（eth_*、debug_*、trace_* 等）                    │
└─────────────────────────────────────────────────────────────────┘
```

**关键认知**：
- CL 不执行交易，它只管"谁的块是合法的主链"
- EL 不做共识，它只管"这些交易执行后状态是什么"
- The Merge 后，EL 的 DevP2P `NewBlock` 消息被废弃，新块只通过 CL libp2p 传播
- CL 和 EL 必须显式配对，通过共享 JWT secret 认证通信（可在不同机器上），不能共用别人的

---

## 二、一笔交易的完整生命周期

### 总览图

```
用户钱包签名交易
       │
       ▼
  ┌────────────┐
  │  JSON-RPC  │  eth_sendRawTransaction
  │    Server  │  crates/rpc/rpc-eth-api/src/helpers/transaction.rs
  └─────┬──────┘
        │ 解码 + 验签 + add_transaction(Local)
        ▼
  ┌────────────┐         ┌──────────────────────────────────┐
  │  交易池    │◄────────│  P2P 网络 (DevP2P)               │
  │  mempool   │         │  其他节点广播来的交易              │
  │            │────────►│  add_external_transactions()     │
  └─────┬──────┘         └──────────────────────────────────┘
        │                         ▲
        │ 交易在池中等待            │ 广播自己收到的新交易
        │                         │
        ▼                         │
  ┌─────────────────────────────────────────────────────────┐
  │              是否轮到本节点出块？                         │
  └──────────┬──────────────────────────┬───────────────────┘
             │ 是（被 CL 选中）           │ 否
             ▼                          ▼
  ┌──────────────────┐        ┌──────────────────────────┐
  │  Payload Builder │        │   等待接收其他节点的块    │
  │  从池中取最佳交易 │        └───────────┬──────────────┘
  │  逐笔执行，组装块 │                    │ 收到新块
  └────────┬─────────┘                    │
           │ 构建完成                      ▼
           │ 返回给 CL          ┌──────────────────────┐
           │                   │   Engine API         │
           ▼                   │  engine_newPayloadV3  │
  ┌──────────────────┐         └──────────┬───────────┘
  │   CL 广播块      │                    │
  │   全网验证者投票  │                    ▼
  └──────────────────┘        ┌──────────────────────┐
                              │   Executor           │
                              │   执行所有交易         │
                              │   验证 state_root     │
                              └──────────┬───────────┘
                                         │ VALID / INVALID
                                         ▼
                              ┌──────────────────────┐
                              │   持久化到数据库       │
                              │   MDBX + 静态文件     │
                              └──────────────────────┘
```

---

## 三、各环节详细解析

### 环节 1：用户提交交易

**发生了什么：** 用户用私钥签名一笔交易，通过 `eth_sendRawTransaction` 发送给节点。

```
用户（MetaMask/ethers.js）
  └─ 构造交易：{ to, value, data, gasLimit, maxFeePerGas, nonce, ... }
  └─ 用私钥签名 → 得到 { v, r, s }
  └─ RLP 编码 → raw bytes
  └─ 调用 eth_sendRawTransaction(raw_bytes)
```

**reth 代码路径：**
```
crates/rpc/rpc-eth-api/src/helpers/transaction.rs
  └─ send_raw_transaction(bytes)             # 入口
      ├─ recover_raw_transaction(&tx)        # 解码 + ECDSA 恢复签名者地址
      └─ pool.add_transaction(
             TransactionOrigin::Local,       # 标记来自本地 RPC
             recovered_tx
         )
```

**关键细节：**
- `recover_raw_transaction` 验证签名，恢复出 `msg.sender` 地址
- `TransactionOrigin::Local` 意味着会被优先处理，且主动广播给 peers
- 返回交易哈希给用户（此时交易还未上链）

---

### 环节 2：交易池（Mempool）

**发生了什么：** 交易进入交易池，等待被选入区块。

**reth 代码路径：**
```
crates/transaction-pool/src/
  ├─ lib.rs          # Pool 公共 API
  ├─ traits.rs       # TransactionPool trait
  └─ pool/mod.rs     # 具体实现：Pool<V, T, S>
```

交易池内部分为 4 个子池：

```
// crates/transaction-pool/src/pool/state.rs:SubPool
所有待处理交易
  │
  ├─ Pending 池   ── 可立即打包：nonce 连续 + 余额足够 + base fee 满足
  │                  出块时从这里取
  │
  ├─ Queued 池    ── 有 nonce 间隙（nonce=5 到了，但 nonce=4 还没）
  │                  或余额暂时不足，等待条件满足
  │
  ├─ BaseFee 池   ── maxFeePerGas 低于当前 baseFee
  │                  baseFee 降低后自动升级到 Pending
  │
  └─ Blob 池      ── EIP-4844 blob 交易（独立的 blob 费用市场）
```

**验证内容（入池时）：**
- 签名有效性（已在 RPC 层完成）
- Nonce 下限（`tx.nonce < account.nonce` 则拒绝，即不接受已过期的 nonce）
- 每账户槽位上限（同一账户最多 16 笔交易同时在池中，超出则拒绝）
  - nonce 高多少没有直接限制，nonce 间有间隙也没关系，都进 Queued 池等待
  - 例外：EIP-4844 Blob 交易不允许任何 nonce 间隙，必须严格连续
  - 例外：EIP-7702 Delegated 账户只允许 1 笔在途（`max_inflight_delegated_slot_limit = 1`）
- 余额检查（value + gas_fee <= balance）
- Gas limit 不超过区块 gas limit
- 交易格式合法性（EIP-2718 类型检查）

> 代码：`crates/transaction-pool/src/validate/eth.rs:631`（nonce 下限），
> `crates/transaction-pool/src/pool/txpool.rs:1816`（槽位上限，`TXPOOL_MAX_ACCOUNT_SLOTS_PER_SENDER = 16`）

---

### 环节 3：P2P 传播

**发生了什么：** 节点将自己知道的交易广播给其他节点。

**reth 代码路径：**
```
crates/net/network/src/transactions/mod.rs
  └─ TransactionsManager     # 管理交易的 P2P 传播
```

**传播策略（ETH/68 协议）：**
```
新交易进入本地 Pending 池
  │
  ├─ 向部分 peers 发送完整交易（小交易，直接发）
  │  消息类型：Transactions
  │
  └─ 向其他 peers 只发送哈希（大交易，先通知）
     消息类型：NewPooledTransactionHashes68 { hash, type, size }
                    │
                    ▼
               对方节点：我没有这个交易！
                    │
                    ▼
               发送 GetPooledTransactions
                    │
                    ▼
               收到完整交易 → 入池 → 继续传播
```

**为什么先发哈希？**
多个 peers 可能同时广播同一笔交易，只发哈希让接收方决定是否需要完整数据，节省带宽避免重复传输。

---

### 环节 4：出块（仅出块节点执行）

**前提：** CL 通过 RANDAO 随机选出本节点为当前 slot（12秒）的出块者。

#### 4a. CL 发出出块请求

```
CL → EL:  engine_forkchoiceUpdatedV3(
    forkchoiceState: {
        headBlockHash,      // 当前链头
        safeBlockHash,      // 安全块
        finalizedBlockHash  // 已最终确认的块
    },
    payloadAttributes: {    // 携带此字段 = 请求出块
        timestamp,
        prevRandao,
        suggestedFeeRecipient,  // 矿工费接收地址
        parentBeaconBlockRoot,
        ...
    }
)

EL → CL:  { payloadId: "0x..." }  // 返回一个 ID，稍后来取
```

**reth 代码路径：**
```
crates/rpc/rpc-engine-api/src/engine_api.rs
  └─ fork_choice_updated_v3()
      └─ validate_and_execute_forkchoice()
          └─ 若含 payloadAttributes → 触发 PayloadBuilderService
```

#### 4b. EL 构建区块

```
crates/payload/builder/src/service.rs
  └─ PayloadBuilderService（后台异步任务，持续改进中的块）
      └─ 创建 PayloadJob
          └─ EthereumPayloadBuilder::build_payload()
              crates/ethereum/payload/src/lib.rs
```

**出块过程：**
```
从交易池取最佳交易:
  pool.best_transactions_with_attributes({ basefee, blob_fee })
  → 返回按 gas price 排序的迭代器

逐笔执行:
  BlockBuilder::apply_pre_execution_changes()
    ├─ EIP-4788: 写入 parent beacon block root 到系统合约
    └─ EIP-7002: 处理提款请求

  for tx in best_transactions:
      BlockBuilder::execute_transaction(tx)

  BlockBuilder::finish()
      ├─ 计算 state_root（更新 Merkle Patricia Trie）
      ├─ 计算 receipts_root
      ├─ 计算 logs_bloom
      └─ 组装完整的 Block { header, body }
```

#### 4c. CL 取走并广播块

```
CL → EL: engine_getPayloadV3(payloadId)
EL → CL: ExecutionPayload { ... }

CL 将其包装成 BeaconBlock 并广播给全网验证者
```

---

### 环节 5：其他节点验证

#### 5a. CL 共识层验证
- 提议者签名是否有效
- 是否在正确的 slot 提议
- Attestation 数量是否足够

验证通过后，CL 把执行载荷交给 EL。

#### 5b. EL 执行验证

```
CL → EL: engine_newPayloadV3(executionPayload)

crates/rpc/rpc-engine-api/src/engine_api.rs
  └─ new_payload_v3()
      └─ beacon_consensus.new_payload()
          ↓
crates/engine/tree/src/tree/mod.rs
  └─ on_new_payload()
      ├─ 检查父块是否已知
      ├─ Executor::execute_one(block)   ← 重新执行所有交易
      ├─ 验证 state_root（与块头对比）
      ├─ 验证 receipts_root
      └─ 验证 gas_used

EL → CL: PayloadStatus { status: VALID | INVALID | SYNCING }
```

**核心原则：自己重新执行，自己计算 state_root，不信任任何人的结论。**

---

### 环节 6：Forkchoice 更新（确认主链）

足够多的验证者投票（Attest）后，CL 通知 EL 更新主链：

```
CL → EL: engine_forkchoiceUpdatedV3(
    forkchoiceState: {
        headBlockHash:      <新块>,    // 主链最新头
        safeBlockHash:      <安全块>,  // 2/3 验证者见过
        finalizedBlockHash: <最终块>,  // 不可逆转
    },
    payloadAttributes: null
)
```

reth 收到后：
```
crates/engine/tree/src/tree/mod.rs
  └─ on_forkchoice_updated()
      ├─ 更新链头指针
      ├─ 标记 safe / finalized 块
      ├─ 触发 Pruner 清理旧数据
      └─ 从交易池移除已上链的交易
```

---

### 环节 7：持久化

```
crates/storage/
  ├─ db/              # MDBX 内存映射数据库（热数据）
  │   ├─ 最近 N 个块的区块数据
  │   ├─ 当前账户状态（余额、nonce、代码、存储）
  │   └─ 索引数据（交易哈希 → 块号等）
  │
  └─ static-file/     # 静态文件（冷数据，只追加）
      ├─ 历史区块头
      ├─ 历史区块体（交易列表）
      └─ 历史 Receipts
```

- **MDBX**：内存映射，适合频繁随机读写的热数据
- **静态文件**：顺序追加，适合不变的历史数据，查询快，压缩比高

---

## 四、EL 内部并发架构

EL 节点不是单线程程序，它是一组**相互协作的并发任务**。以 reth 为例：

### 任务管理基础设施

```
crates/tasks/src/lib.rs

TaskManager          # 监控所有任务，处理 panic，协调优雅关闭
  └─ TaskExecutor    # 任务派发接口，提供四种派发方式：
      ├─ spawn()                              # 普通异步任务
      ├─ spawn_critical()                     # 关键任务（panic 则节点退出）
      ├─ spawn_blocking()                     # 阻塞任务（独立线程池）
      └─ spawn_critical_with_graceful_shutdown_signal()  # 关键 + 优雅关闭
```

### 启动顺序

节点启动不是同时启动所有任务，而是有明确的初始化顺序：

1. **基础设施**：Rayon 配置 → TOML 配置 → 数据库 → ProviderFactory
2. **核心组件**：Network → Pool → EVM Config → Consensus → Payload Builder
3. **Pipeline 准备**：ExEx → StaticFileProducer → Pipeline → Pruner
4. **服务启动**：Events Task → RPC 服务器 → Consensus Engine

**📖 详细的 20+ 步初始化流程见：** [附录A：节点启动详细流程](./appendix-node-startup.md)

### 节点启动时的完整任务树

```
Node 启动
│
├─ [blocking] pipeline task
│    初始化时的数据库一致性检查和恢复
│    crates/node/builder/src/launch/common.rs:561
│
├─ [async]  metrics listener task ──────────────────── 监听 stages 进度，上报 Prometheus
│    crates/node/builder/src/launch/common.rs:681
│
├─ P2P 网络组（三个任务）
│   ├─ [blocking] p2p txpool handler ────────────────── 处理交易公告/请求消息
│   ├─ [blocking] p2p eth request handler ───────────── 响应 GetBlockHeaders 等请求
│   └─ [async+graceful] p2p network task ────────────── 主网络循环（peer 管理、发现）
│       crates/node/builder/src/builder/mod.rs:906-927
│       └─ 内部子服务：
│           ├─ DiscV4 Service   # UDP 节点发现
│           └─ DNS Discovery    # DNS 节点发现
│
├─ 交易池组（两个任务）
│   ├─ [async]  txpool maintenance task ─────────────── 监听链变化，维护池状态
│   └─ [async+graceful] local txpool backup task ─────── 定期将本地交易备份到磁盘
│       crates/node/builder/src/components/pool.rs:231,259
│
├─ [async]  payload builder service ─────────────────── 出块时持续构建/改进候选块
│    crates/node/builder/src/components/payload.rs:110
│
├─ RPC 服务组（服务器，非 task）
│   ├─ HTTP JSON-RPC Server     # eth_* 等接口
│   ├─ WebSocket JSON-RPC Server
│   ├─ IPC Server
│   └─ Engine Auth Server       # engine_* 接口（JWT 认证）
│       crates/node/builder/src/rpc.rs:937-948
│
├─ [async]  cache canonical blocks task ─────────────── RPC 状态缓存更新
│    crates/node/builder/src/rpc.rs:995
│
├─ [async]  consensus engine ────────────────────────── 核心主循环
│    处理 newPayload / forkchoiceUpdated，协调同步和出块
│    crates/node/builder/src/launch/engine.rs:374
│
├─ [async]  events task ─────────────────────────────── 聚合多路事件流（pipeline/pruner/静态文件）
│    crates/node/builder/src/launch/engine.rs:251
│
├─ [async]  Prometheus metrics server（可选）
│
├─ ExEx 任务组（若安装了执行扩展）
│   ├─ [async] exex (每个扩展一个) ──────────────────── 用户自定义执行扩展
│   ├─ [async] exex manager ─────────────────────────── 协调多个 ExEx
│   └─ [async] exex manager blockchain tree notifications ── 链状态变更通知转发
│       crates/node/builder/src/launch/exex.rs:114-150
│
└─ EthStats 任务组（可选，--ethstats 配置时启动）
    ├─ [async] ethstats main task
    └─ [async] ethstats event listener
```

### 关键数据流

```
用户/P2P 交易输入
  └─► RPC Server / p2p txpool handler
        └─► 交易池（Pending/Queued/BaseFee/Blob）
              └─► payload builder service（出块时消费）
                    └─► 构建好的块 → engine_getPayload → CL

CL 新块输入
  └─► Engine Auth Server
        └─► consensus engine（主循环）
              ├─► Executor（执行验证）
              ├─► Storage（持久化）
              ├─► events task（通知各组件）
              └─► txpool maintenance（移除已上链交易）

链状态变化
  └─► events task
        ├─► network（广播新块给 peers）
        ├─► ExEx manager（通知执行扩展）
        ├─► cache canonical blocks task（更新 RPC 缓存）
        └─► Pruner / StaticFileProducer（清理和归档）
```

---

## 五、Engine API 三个核心方法

| 方法 | 调用时机 | EL 做什么 |
|------|---------|---------|
| `engine_forkchoiceUpdatedV3` | 1. 每次出新块后更新链头<br>2. 需要出块时（带 payloadAttributes） | 1. 更新主链/安全/最终化指针<br>2. 启动 PayloadBuilder 构建新块 |
| `engine_newPayloadV3` | 收到其他节点的新块时 | 重新执行所有交易，验证状态根，返回 VALID/INVALID |
| `engine_getPayloadV3` | 出块者取走已构建的块 | 返回 PayloadBuilderService 构建的最佳块 |

---

## 六、CL vs EL 职责边界

```
CL 负责：                          EL 负责：
  哪个块由合法的验证者提议？           这些交易执行后状态是什么？
  有足够验证者投票了吗？               state_root 正确吗？
  主链是哪条分叉？                    交易池里有哪些待打包交易？
  哪些块已最终确认？                  合约代码执行结果是什么？

CL 无法独立完成：                  EL 无法独立完成：
  验证交易执行的正确性                 决定哪条分叉是主链
  计算 state_root                    决定出块者是谁
  管理 mempool                       最终确认（Finality）
```

---

## 七、高性能 EVM 链的性能优化方向

以太坊主网的性能瓶颈：

```
吞吐量（TPS）   瓶颈在 EL：单线程顺序执行，约 15 TPS
出块延迟        瓶颈在 CL：50万验证者共识，12 秒/slot
Gas 上限        瓶颈在 EL：执行和状态读写速度决定上限
```

### BSC（牺牲去中心化换性能）

```
CL 优化：
  验证者 500,000 → 21 个
  BFT 共识消息量 O(n²)：21 个节点极快达成共识
  → 3 秒出块（以太坊 12 秒）

EL 优化：
  更高 Gas Limit（每块容纳更多交易）
  近期引入并行 EVM

代价：
  21 个验证者 ≈ 联盟链，去中心化程度极低
```

### Monad（技术含量最高，保留去中心化）

```
CL 优化（MonadBFT）：
  基于 HotStuff 的流水线共识，将共识与执行解耦

  普通 BFT：  [共识 N] → [执行 N] → [共识 N+1] → [执行 N+1]
  MonadBFT：  [共识 N]
                       [共识 N+1][执行 N]
                                  [共识 N+2][执行 N+1]
  → 1 秒出块

EL 优化（三大核心创新）：

  1. 并行执行（Optimistic Parallel Execution）
     传统 EVM：tx1 → tx2 → tx3（严格串行）
     Monad：   tx1 ┐
               tx2 ├─ 乐观并行，检测读写冲突
               tx3 ┘   冲突的交易重新串行执行

  2. 异步 I/O（Async State Access）
     传统：执行到 SLOAD 指令时，同步等待磁盘读取
     Monad：提前预取所有可能用到的状态，执行时零等待

  3. MonadDB（自研存储引擎）
     针对 EVM 状态的访问模式定制优化
     比通用数据库（LevelDB/MDBX）快数倍
```

### MegaETH（极端化架构，专为 L2 设计）

```
核心思路：异构节点，不是所有节点做同样的事

传统架构：
  每个节点 = 完整 EL + CL，全部独立执行所有交易

MegaETH 架构：
  Sequencer（1台超高配置机器）
  ├─ 执行所有交易（TB 级内存、高端 NVMe、服务器 CPU）
  ├─ 目标：100,000+ TPS
  └─ 生成执行证明

  Full Node（普通节点）
  └─ 不重新执行，验证 Sequencer 的证明即可
     同步成本极低

  Light Node
  └─ 只跟踪状态根，信任证明

代价：
  Sequencer 是单点，高度中心化
  定位是 L2（依赖以太坊 L1 保证安全）
```

### 并行 EVM：高性能链的核心难题

```
EVM 天生是串行的：

  tx_A: storage[0x1] = 100   ─┐
  tx_B: read storage[0x1]    ─┘ 必须等 tx_A 完成才能读

要并行执行，必须解决：
  1. 如何检测哪些交易会冲突（读写同一存储槽）？
  2. 冲突了怎么办？

各链的解法：
  Monad    乐观并行（假设不冲突，冲突后重新执行冲突的 tx）
  BSC      静态分析（提前扫描交易的读写集）
  Sei      显式并行（让用户/合约声明依赖关系）
  MegaETH  回避问题（单台超强机器串行执行）
```

### 性能优化维度汇总

| 优化方向 | 属于哪层 | 代表方案 | 权衡 |
|---------|---------|---------|------|
| 减少验证者数量 | CL | BSC（21个） | 丧失去中心化 |
| 流水线共识 | CL | Monad（MonadBFT） | 实现复杂度高 |
| 并行交易执行 | EL | Monad、BSC | 冲突重执行有开销 |
| 自研存储引擎 | EL | Monad（MonadDB） | 巨大工程投入 |
| 异步状态预取 | EL | Monad | 预取不准时浪费 |
| 异构节点执行 | 架构 | MegaETH（Sequencer） | 中心化单点 |
| 更高 Gas Limit | EL | 所有高性能链 | 状态膨胀风险 |

**结论：** CL 优化解决出块延迟，EL 优化解决吞吐量。以太坊主网的核心瓶颈始终在 EL 的**串行执行**，并行 EVM 是当前高性能链最核心的竞争点。

---

## 八、以太坊的四棵 Merkle Patricia Trie

每个区块头包含四个 Merkle 根，各自对应一棵树：

```
Block Header
├── state_root           ← 全局账户状态（持久化，每块变化）
│   └── 每个合约地址下有 storage_root
│       └── Storage Trie（slot → value，每个合约独立一棵）
│
├── transactions_root    ← 本块交易列表（只在该块有效）
├── receipts_root        ← 本块执行回执（只在该块有效）
└── withdrawals_root     ← 本块提款列表（Shanghai 后新增）
```

### State Trie（状态树）—— 最重要

全局账本，记录所有账户的当前状态：

```
keccak(address) → Account {
    nonce: u64,         // 该账户发出了多少笔交易
    balance: U256,      // ETH 余额（wei 为单位）
    storage_root: B256, // 指向该合约 Storage Trie 的根
    code_hash: B256,    // 合约字节码的哈希（EOA 账户为空哈希）
}
```

- **是持久化的**：每块执行完后更新，跨块保留
- 每个合约地址还有一棵独立的 **Storage Trie**（存储合约的 slot → value），`storage_root` 是这棵子树的根
- `state_root` 由 `state_root_with_updates()` 单独计算（因为最重，单独处理）

### Transactions Trie（交易树）

本块所有交易按索引构成的树：

```
0 → tx0（RLP 编码）
1 → tx1
...
N → txN
```

- **是临时的**：只描述本块，不跨块
- 用途：轻节点可通过 Merkle Proof 验证"某笔交易确实在这个块里"，无需下载整块

### Receipts Trie（回执树）

本块所有 Receipt 按索引构成的树：

```
0 → Receipt0 { success, cumulative_gas_used, logs }
1 → Receipt1
...
```

- **是临时的**：只描述本块，不跨块
- 用途：轻节点可验证"某笔交易的执行结果"
- `logs_bloom` 是从所有 Receipt 的 logs 聚合而来的布隆过滤器，用于快速过滤包含特定事件的块

### 三棵树在 reth 中的计算位置

```rust
// crates/ethereum/evm/src/build.rs:55-59
let transactions_root = proofs::calculate_transaction_root(&transactions);
let receipts_root = calculate_receipt_root(...);
let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs()));
// state_root 从外部传入，在 finish() 中通过 state_root_with_updates() 单独计算
```

> 传统上说"三棵树"指 Frontier 时代的 state/txs/receipts；`withdrawals_root` 是 Shanghai（2023）新增的。
> 严格来说以太坊有 **4 棵**块头树，加上每个合约隐含的 Storage Trie，总共是 4 + N 棵。

---

## 九、对应 reth 核心代码目录

```
reth/crates/
│
├── tasks/                   # 任务管理（spawn/shutdown）
│   └── src/lib.rs           # TaskManager, TaskExecutor
│
├── rpc/
│   ├── rpc-engine-api/      # Engine API（CL↔EL）
│   │   └── src/engine_api.rs
│   └── rpc-eth-api/         # eth_* RPC（用户↔EL）
│       └── src/helpers/transaction.rs
│
├── transaction-pool/        # 交易池（4个子池）
│   └── src/
│
├── net/network/             # P2P 网络
│   └── src/transactions/    # 交易传播
│
├── payload/                 # 出块
│   └── builder/src/service.rs
│
├── evm/                     # 交易执行
│   └── evm/src/execute.rs
│
├── engine/                  # 共识引擎集成
│   └── tree/src/            # newPayload / forkchoiceUpdated 处理
│
├── storage/                 # 持久化
│   ├── db/                  # MDBX（热数据）
│   └── static-file/         # 静态文件（冷数据）
│
└── node/builder/            # 节点组件组装与启动
    └── src/
        ├── builder/mod.rs   # NodeBuilder
        └── launch/
            ├── engine.rs    # 启动共识引擎和事件系统
            ├── common.rs    # 启动通用服务（metrics 等）
            └── exex.rs      # 启动执行扩展
```

---

## 十、知识检验

- [ ] The Merge 后，EL 的 DevP2P `NewBlock` 消息为什么被废弃？
- [ ] CL 和 EL 为什么必须通过 JWT 认证连接，而不能用公开接口？
- [ ] 一笔交易从用户发出到上链，经过了哪些主要环节？
- [ ] 交易池的 4 个子池分别存放什么类型的交易？
- [ ] reth 节点内部有哪几类并发任务？`spawn_critical` 和 `spawn` 有什么区别？
- [ ] `engine_newPayloadV3` 和 `engine_forkchoiceUpdatedV3` 分别在什么时候被调用？
- [ ] reth 的存储为什么分 MDBX 和静态文件两种？
- [ ] 并行 EVM 面临的核心问题是什么？Monad 和 MegaETH 分别用什么思路解决？
- [ ] BSC 的高性能主要来自哪一层的优化？代价是什么？

- [ ] 以太坊块头中有几棵 Merkle 树？各自存储什么？
- [ ] State Trie 和 Storage Trie 的关系是什么？
- [ ] 为什么 `state_root` 要单独计算，而 `transactions_root` 和 `receipts_root` 在 `assemble_block` 里一起算？

---

## 十一、下一章衔接

**第一章：交易执行流程**

深入"执行交易"这个环节：
- EVM 如何执行字节码？
- Gas 如何计算和扣除？
- Receipt 如何生成？
- `state_root` 如何计算？

**代码入口：**
```
crates/evm/evm/src/execute.rs        # Executor trait 和实现
crates/ethereum/evm/src/receipt.rs   # Receipt 生成
crates/ethereum/evm/src/build.rs     # 块头组装
```
