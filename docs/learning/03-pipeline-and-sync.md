# 第三章：Pipeline 与区块链同步

## 目录
- [Pipeline 是什么](#pipeline-是什么)
- [Staged Sync 架构](#staged-sync-架构)
- [完整的 Stage 列表](#完整的-stage-列表)
- [Pipeline 初始化流程](#pipeline-初始化流程)
- [Pipeline 启动时机](#pipeline-启动时机)
- [Pipeline 运行机制](#pipeline-运行机制)
- [Unwind 机制](#unwind-机制)
- [与 Engine API 的关系](#与-engine-api-的关系)

---

## Pipeline 是什么？

Pipeline 是 reth 中用于**区块链同步**的核心组件，采用 **Staged Sync**（分阶段同步）架构。

### 核心概念

```
Pipeline = 一系列按顺序执行的 Stage（阶段）

每个 Stage 负责同步的一个特定方面：
- Headers (区块头)
- Bodies (区块体)
- Execution (交易执行)
- Merkle Tree (状态树计算)
- ...
```

### 为什么需要 Pipeline？

传统的区块链同步方式（逐个区块同步）效率低下：

```rust
// 传统方式（低效）
for block in start..=end {
    download_block(block);
    validate_block(block);
    execute_transactions(block);
    update_state(block);
    compute_merkle_tree(block);
}
// 问题：每个区块都要完成所有步骤才能继续
```

**Staged Sync** 的优势：

```rust
// Staged Sync（高效）
// Stage 1: 批量下载所有区块头
for block in start..=end {
    download_header(block);
}

// Stage 2: 批量下载所有区块体
for block in start..=end {
    download_body(block);
}

// Stage 3: 批量执行所有交易
for block in start..=end {
    execute_transactions(block);
}

// ... 其他 Stage

// 优势：每个阶段可以优化，利用批处理、并行、缓存等技术
```

---

## Staged Sync 架构

### 整体架构图

```
┌─────────────────────────────────────────────────────────────────┐
│                        Pipeline                                  │
│                                                                   │
│  ┌──────┐   ┌──────┐   ┌────────┐   ┌──────┐   ┌────────┐      │
│  │ Era  │ → │Header│ → │ Body   │ → │Sender│ → │Execute │ →... │
│  │Stage │   │Stage │   │ Stage  │   │Stage │   │ Stage  │      │
│  └──────┘   └──────┘   └────────┘   └──────┘   └────────┘      │
│     ↓           ↓          ↓           ↓           ↓             │
│  [ERA1]    [Headers]   [Bodies]   [Senders]   [Execute]         │
│   导入      下载头      下载体      恢复签名    执行交易          │
│                                                                   │
│  每个 Stage:                                                      │
│  1. 读取上一个 Stage 的结果                                      │
│  2. 执行自己的任务                                               │
│  3. 保存 checkpoint（检查点）                                    │
│  4. 传递给下一个 Stage                                           │
└─────────────────────────────────────────────────────────────────┘
```

### Pipeline 定义

代码位置：[`crates/stages/api/src/pipeline/mod.rs`](../../crates/stages/api/src/pipeline/mod.rs#L69-L95)

```rust
/// 一个分阶段同步的 Pipeline
pub struct Pipeline<N: ProviderNodeTypes> {
    /// Provider factory - 数据库访问
    provider_factory: ProviderFactory<N>,

    /// 所有配置的 Stage，按执行顺序排列
    stages: Vec<BoxedStage<_>>,

    /// 最大同步到的区块号（可选）
    max_block: Option<BlockNumber>,

    /// Static File 生成器
    static_file_producer: StaticFileProducer<_>,

    /// Pipeline 事件发送器
    event_sender: EventSender<PipelineEvent>,

    /// Pipeline 进度跟踪
    progress: PipelineProgress,

    /// 同步目标的发送器（通知 Headers Stage）
    tip_tx: Option<watch::Sender<B256>>,

    /// 是否在 unwind 时失败
    fail_on_unwind: bool,

    // ... 其他字段
}
```

---

## 完整的 Stage 列表

### DefaultStages（默认完整同步）

代码位置：[`crates/stages/stages/src/sets.rs`](../../crates/stages/stages/src/sets.rs#L60-L84)

根据源码 [`crates/stages/stages/src/sets.rs:69-83`](../../crates/stages/stages/src/sets.rs#L69-L83)，reth 的 **DefaultStages** 包含以下 Stage：

### DefaultStages 完整列表（共 15 个 Stages）

> **重要**：根据源码 [`crates/stages/stages/src/sets.rs:69-83`](../../crates/stages/stages/src/sets.rs#L69-L83) 和 [`crates/stages/types/src/id.rs:46-62`](../../crates/stages/types/src/id.rs#L46-L62)，DefaultStages 包含 **15 个 Stage**（按执行顺序）：

| # | Stage 名称 | StageId | 作用 | 输入 | 输出 |
|---|-----------|---------|------|------|------|
| 1 | **EraStage** (可选) | `Era` | 从 ERA1 归档文件导入历史数据 | ERA1 文件 | Headers, Bodies, Receipts |
| 2 | **HeaderStage** | `Headers` | 下载区块头 | 链尖端 hash | Headers 表 |
| 3 | **BodyStage** | `Bodies` | 下载区块体（交易列表） | Headers | Bodies, Transactions 表 |
| 4 | **SenderRecoveryStage** | `SenderRecovery` | 从签名恢复交易发送者地址 | Transactions | TransactionSenders 表 |
| 5 | **ExecutionStage** | `Execution` | 执行所有交易，更新账户状态 | Bodies + Senders | PlainAccountState, PlainStorageState |
| 6 | **PruneSenderRecoveryStage** | `PruneSenderRecovery` | 修剪旧的 Sender 数据（仅 execute 时） | TransactionSenders | - |
| 7 | **MerkleStage** (unwind) | `MerkleUnwind` | 回滚时清理 Merkle 树节点 | - | - |
| 8 | **AccountHashingStage** | `AccountHashing` | 计算账户地址的 hash | PlainAccountState | HashedAccounts 表 |
| 9 | **StorageHashingStage** | `StorageHashing` | 计算存储 key 的 hash | PlainStorageState | HashedStorages 表 |
| 10 | **MerkleStage** (execute) | `MerkleExecute` | 计算 Merkle Patricia Trie（状态根） | HashedAccounts + HashedStorages | AccountsTrie, StoragesTrie 表 |
| 11 | **TransactionLookupStage** | `TransactionLookup` | 创建 tx_hash → tx_number 索引 | Transactions | TransactionHashNumbers 表 |
| 12 | **IndexStorageHistoryStage** | `IndexStorageHistory` | 创建存储历史索引 | StorageChangeSets | StoragesHistory 表 (或 RocksDB) |
| 13 | **IndexAccountHistoryStage** | `IndexAccountHistory` | 创建账户历史索引 | AccountChangeSets | AccountsHistory 表 (或 RocksDB) |
| 14 | **PruneStage** | `Prune` | 修剪其他历史数据（仅 execute 时） | - | - |
| 15 | **FinishStage** | `Finish` | 标记同步完成，清理临时数据 | - | - |

### Stage 分组（三级结构）

`DefaultStages` 在源码 [`sets.rs`](../../crates/stages/stages/src/sets.rs) 中按**三级结构**组织，顶层只有三组：

```
DefaultStages
├── OnlineStages     (需要网络)
├── OfflineStages    (离线可执行)
└── FinishStage      (完成标记)
```

**1. OnlineStages**（需要 P2P 网络）：
- `EraStage` (可选，ERA1 归档导入)
- `HeaderStage`
- `BodyStage`

**2. OfflineStages**（不需要网络，本地计算）：

`OfflineStages` 内部进一步细分为四个子组：

| 子组 | Stage |
|------|-------|
| `ExecutionStages` | SenderRecoveryStage → ExecutionStage → PruneSenderRecoveryStage |
| `HashingStages` | MerkleStage(unwind) → AccountHashingStage → StorageHashingStage → MerkleStage(execute) |
| `HistoryIndexingStages` | TransactionLookupStage → IndexStorageHistoryStage → IndexAccountHistoryStage |
| `PruneStage` | PruneStage |

**3. FinishStage**（单独一个 Stage，标记同步完成）

**MerkleStage 的两个阶段**：
- **MerkleUnwind** (StageId::MerkleUnwind)：仅在 unwind (回滚) 时执行，清理 Merkle 树
- **MerkleExecute** (StageId::MerkleExecute)：正常执行时计算状态根

**条件执行的 Stages**：
- **PruneSenderRecoveryStage** 和 **PruneStage** 只在配置了 `prune_modes` 时才会执行
- **EraStage** 只在导入 ERA1 归档文件时执行

**总数统计**：
- 必须执行：12 个 (不含 Era, PruneSenderRecovery, MerkleUnwind, Prune)
- 含可选 Era：13 个
- 含所有 Prune stages：15 个（最大值）

### 关键 Stage 详解

#### 1. HeaderStage - 下载区块头

HeaderStage 是**编排者**，它本身不发网络请求，而是通过 `HeaderDownloader` trait 抽象下载逻辑。

**三层调用链**（从上到下）：

```
HeaderStage::poll_execute_ready()          [headers.rs:242]
    │  self.downloader.poll_next_unpin(cx)
    │  (Stream trait dispatch)
    ▼
ReverseHeadersDownloader::poll_next()      [reverse_headers.rs:784]
    │  并发维护多个飞行中请求
    │  this.submit_request() / this.request_fut()
    ▼
FetchClient::get_headers_with_priority()   [fetch/client.rs:63]
    │  request_tx.send(DownloadRequest::GetBlockHeaders {...})
    ▼
Network StateMachine → ETH GetBlockHeaders 协议消息
```

**ReverseHeadersDownloader 核心机制**：

| 特性 | 说明 |
|------|------|
| 下载方向 | **倒序**（从 tip 向 local_head 方向，block number 递减） |
| 并发度 | `num_peers × 5` 个飞行中请求（`FuturesUnordered`） |
| 乱序缓冲 | 响应可能乱序到达，放入 `BinaryHeap` 排序后再验证 |
| 并行验证 | `process_next_headers` 中用 rayon 并行计算 `SealedHeader::seal_slow`（hash） |
| 批量返回 | 积累 `stream_batch_size` 个验证头后才 yield 给 HeaderStage |

**HeaderStage 数据写入路径**（先 ETL 缓冲，再顺序写入）：

```
P2P 网络 → ReverseHeadersDownloader → HeaderStage::poll_execute_ready
                                            │
                                   ┌────────┴─────────┐
                                   │  ETL Collector   │
                                   │  倒序下载，       │
                                   │  ETL 按 block   │
                                   │  number 排序    │
                                   └────────┬─────────┘
                                            │ execute() 调用 write_headers()
                                            ▼
                              ┌─────────────────────────┐
                              │ Static Files             │  Headers segment（顺序 append）
                              │ MDBX HeaderNumbers 表   │  hash → number 反向索引
                              └─────────────────────────┘
```

> **为什么要 ETL 缓冲？** Headers 倒序下载（tip → local_head），而 Static Files 是 append-only 要求升序写入。ETL Collector 做中间排序，保证写入顺序正确。

**为什么要反向下载（tip → local_head）？**

反向下载是精心设计的选择，有三个核心原因：

**1. Sync Target 是 Hash，不是 Block Number**

同步目标由共识层或 peers 给出的是 **hash**，而不是 block number：

```rust
// SyncTarget 类型
SyncTarget::Tip(B256)  // 只有 hash，不知道 number
```

必须先请求该 hash 对应的 header，获取其 block number，才能向下数。
如果正向下载，就需要提前知道 tip 的 block number，而这正是未知的。

**2. 从已知可信点建立密码学信任链**

反向下载可以从**已知正确的 tip hash** 出发，沿 `parent_hash` 一路向下验证：

```
tip_hash（已知，可信）
  ↓ 验证: header_N.hash() == tip_hash ✓
header_N.parent_hash
  ↓ 验证: header_{N-1}.hash() == header_N.parent_hash ✓
header_{N-1}.parent_hash
  ↓ ...
header_{local_head+1}.parent_hash == local_head.hash() ✓
local_head（本地已有，可信）
```

每一步通过 `parent_hash` 密码学地验证前一个 header 的真实性，最终"落地"到本地已验证的链。
正向下载则只能在**最后**才对比 tip hash，期间无法验证链的完整性。

**3. 终止条件清晰且早期发现分叉**

下载到 `local_head + 1` 时，gap 已完全填满，立刻停止（见 `headers.rs:261`）：

```rust
if header_number == local_head_number + 1 {
    self.is_etl_ready = true;
    return Poll::Ready(Ok(()))
}
```

如果网络给出的 header 链在某处断裂（`parent_hash` 不匹配），可以**立即**发现并放弃，无需等到终点。

| 维度 | 反向下载（实际） | 正向下载（假设） |
|------|---------------|---------------|
| 信任起点 | tip_hash（已知） | local_head（已知） |
| 验证时机 | 每个 header 立即验证 | 最后才能完整验证 |
| 需要 tip 的 number | 否 | 是 |
| 发现分叉时机 | 立即 | 最后 |

```rust
Input:  当前链尖端 (local_head)
        目标链尖端 (target_tip) - 通过 tip_rx watch channel 接收

Process:
1. poll_execute_ready：驱动 ReverseHeadersDownloader Stream
   - 下载+验证完所有 headers 写入 ETL（磁盘缓冲）后返回 Ready
2. execute：调用 write_headers()
   - 从 ETL 顺序读取 headers
   - 写入 Static Files (Headers segment)
   - 写入 MDBX HeaderNumbers 表

Output: Headers static file 已填充，HeaderNumbers 索引已建立
```

#### 2. BodyStage - 下载区块体

```rust
// 下载完整的区块体（交易列表）

Input:  Headers 表

Process:
1. 读取 Headers 表中的区块头
2. 通过 P2P 请求对应的区块体
3. 验证交易列表的 Merkle 根
4. 保存到 Bodies 和 Transactions 表

Output:
- Bodies 表（区块体元数据）
- Transactions 表（交易详情）
```

#### 3. SenderRecoveryStage - 恢复发送者地址

```rust
// 从 ECDSA 签名恢复交易发送者地址

Input:  Transactions 表（包含签名）

Process:
1. 对每笔交易：
   - 读取签名 (r, s, v)
   - 从签名恢复公钥
   - 从公钥派生以太坊地址
2. 并行处理（CPU 密集）

Output: TransactionSenders 表
        tx_id → sender_address
```

**为什么需要单独的 Stage？**
- ECDSA 签名恢复是 CPU 密集型操作
- 单独的 Stage 可以并行处理
- 可以利用多核 CPU 加速

#### 4. ExecutionStage - 执行交易 ⭐ 最重要

```rust
// 执行所有交易，更新世界状态

Input:
- Bodies (交易列表)
- TransactionSenders (发送者地址)
- 当前状态

Process:
1. 按区块顺序执行：
   for block in start..=end {
       for tx in block.transactions {
           // 使用 revm 执行交易
           let result = evm.execute(tx);

           // 更新账户状态
           update_account_state(result);

           // 更新存储
           update_storage(result);

           // 生成 Receipt
           generate_receipt(result);
       }
   }

Output:
- PlainAccountState (账户状态)
- PlainStorageState (合约存储)
- Receipts (交易回执)
```

**性能优化**：
- Batch 处理多个区块
- 使用 `revm` 的高性能 EVM
- 增量更新状态（不是每个 tx 都刷盘）

#### 5. MerkleStage - 计算状态根

```rust
// 计算 Merkle Patricia Trie（状态根）

Input:
- HashedAccounts (账户地址的 hash)
- HashedStorages (存储 key 的 hash)

Process:
1. 构建 State Trie:
   - 遍历所有 hashed accounts
   - 插入到 MPT 中
   - 计算根 hash

2. 构建 Storage Tries:
   - 为每个合约构建 Storage Trie
   - 计算每个合约的 storage root

Output:
- AccountsTrie 表（State Trie 节点）
- StoragesTrie 表（Storage Trie 节点）
- Block.state_root 更新
```

**优化技术**：
- 增量计算（只重新计算变更的分支）
- 并行计算多个 Storage Trie

---

## Pipeline 初始化流程

### 节点启动时的 Pipeline 准备

代码位置：[`crates/node/builder/src/launch/engine.rs`](../../crates/node/builder/src/launch/engine.rs#L130-L164)

```rust
// 步骤 1: 创建 Pipeline
let pipeline = build_networked_pipeline(
    &ctx.config().stages,
    network_client.clone(),
    ctx.consensus(),
    ctx.provider_factory().clone(),
    &ctx.task_executor(),
    metrics_tx,
    ctx.prune_config().clone(),
    max_block,
    static_file_producer,
    ctx.components().evm_config().clone(),
    exex_manager_handle.clone(),
    era_import_source,
)?;

// 步骤 2: 移动数据到 Static Files（如果需要）
pipeline.move_to_static_files()?;

// 步骤 3: 获取 Pipeline 事件流
let pipeline_events = pipeline.events();
```

### build_networked_pipeline 详解

代码位置：[`crates/node/builder/src/setup.rs`](../../crates/node/builder/src/setup.rs#L32-L135)

```rust
pub fn build_networked_pipeline<N, Client, Evm>(
    config: &StageConfig,
    client: Client,
    consensus: Arc<dyn FullConsensus<_>>,
    provider_factory: ProviderFactory<N>,
    task_executor: &TaskExecutor,
    // ... 其他参数
) -> eyre::Result<Pipeline<N>> {
    // 1. 创建 Header 下载器（反向下载，从新到旧）
    let header_downloader = ReverseHeadersDownloaderBuilder::new(config.headers)
        .build(client.clone(), consensus.clone())
        .into_task_with(task_executor);  // 转为异步任务

    // 2. 创建 Body 下载器
    let body_downloader = BodiesDownloaderBuilder::new(config.bodies)
        .build(client, consensus.clone(), provider_factory.clone())
        .into_task_with(task_executor);  // 转为异步任务

    // 3. 构建 Pipeline
    let pipeline = build_pipeline(
        provider_factory,
        config,
        header_downloader,
        body_downloader,
        consensus,
        max_block,
        metrics_tx,
        prune_config,
        static_file_producer,
        evm_config,
        exex_manager_handle,
        era_import_source,
    )?;

    Ok(pipeline)
}
```

### build_pipeline 核心逻辑

```rust
pub fn build_pipeline<N, H, B, Evm>(
    provider_factory: ProviderFactory<N>,
    stage_config: &StageConfig,
    header_downloader: H,
    body_downloader: B,
    consensus: Arc<dyn FullConsensus<_>>,
    // ... 其他参数
) -> eyre::Result<Pipeline<N>> {
    let mut builder = Pipeline::<N>::builder();

    // 设置最大区块（如果指定）
    if let Some(max_block) = max_block {
        builder = builder.with_max_block(max_block);
    }

    // 创建 tip channel（用于通知 HeaderStage 同步目标）
    let (tip_tx, tip_rx) = watch::channel(B256::ZERO);

    // 构建 Pipeline
    let pipeline = builder
        .with_tip_sender(tip_tx)           // 设置 tip sender
        .with_metrics_tx(metrics_tx)       // 设置 metrics
        .add_stages(                       // 添加所有 Stage
            DefaultStages::new(
                provider_factory.clone(),
                tip_rx,                    // Headers Stage 监听这个 channel
                consensus.clone(),
                header_downloader,         // Header 下载器
                body_downloader,           // Body 下载器
                evm_config.clone(),
                stage_config.clone(),
                prune_config.segments,
                era_import_source,
            )
            .set(ExecutionStage::new(      // 单独配置 Execution Stage
                evm_config,
                consensus,
                stage_config.execution.into(),
                stage_config.execution_external_clean_threshold(),
                exex_manager_handle,
            ))
        )
        .build(provider_factory, static_file_producer);

    Ok(pipeline)
}
```

---

## Pipeline 启动时机

Pipeline 不会在节点启动时立即运行，而是由 `ChainOrchestrator` 按需触发。

### 启动调用栈

```
reth node 命令
    │
    ▼
EngineNodeLauncher::launch_node()              [launch/engine.rs]
    │
    ├─ build_networked_pipeline()              [setup.rs:32]
    │      ├─ ReverseHeadersDownloaderBuilder::build()   → header_downloader
    │      └─ build_pipeline()                [setup.rs:80]
    │             └─ Pipeline::builder()
    │                 .add_stages(DefaultStages::new(...))
    │                 │    └─ OnlineStages::builder()
    │                 │           └─ StageSetBuilder::add_stage(HeaderStage::new(...))
    │                 .build()               ← Pipeline 构建完成，HeaderStage 已注册
    │
    ├─ EngineService::new(pipeline, ...)
    │      └─ ChainOrchestrator::new(handler, PipelineSync::new(pipeline, ...))
    │
    └─ 主循环：
           engine_service.orchestrator_mut()
               .start_backfill_sync(initial_target)  [engine.rs:294]
```

### 两种触发时机

**时机 1：节点启动时存在同步目标（首次同步或重启追块）**

```rust
// launch/engine.rs:291
if let Some(initial_target) = initial_target {
    engine_service.orchestrator_mut().start_backfill_sync(initial_target);
}
```

**时机 2：Engine API 收到 forkchoiceUpdated，发现本地落后**

`EngineApiRequestHandler` 判断需要追块，通过 `HandlerEvent::BackfillAction` 触发：

```rust
// chain.rs:117
HandlerEvent::BackfillAction(action) => {
    this.backfill_sync.on_action(action);  // BackfillAction::Start(target)
}
```

### ChainOrchestrator 架构：两种同步模式互斥

```
ChainOrchestrator::poll_next_event()
    │
    ├─ 1. 优先 poll backfill_sync（PipelineSync）：
    │        Active → 持续执行 pipeline，直到完成
    │        就绪   → 刚 spawn 了 pipeline task，返回 BackfillSyncStarted
    │
    └─ 2. 只有 backfill_sync 空闲时，才 poll handler（Engine API 树处理器）
              ↑ 保证 Pipeline 运行期间，Engine API 不会并发写库（避免死锁）
```

**Pipeline 实际执行**由 `PipelineSync::try_spawn_pipeline` 完成，用 `spawn_critical_blocking`（阻塞型 tokio task）运行：

```rust
// backfill.rs:141
self.pipeline_task_spawner.spawn_critical_blocking(
    "pipeline task",
    Box::pin(async move {
        let result = pipeline.run_as_fut(Some(target)).await;
        let _ = tx.send(result);
    }),
);
```

---

## Pipeline 运行机制

### run_loop - 核心执行循环

代码位置：[`crates/stages/api/src/pipeline/mod.rs`](../../crates/stages/api/src/pipeline/mod.rs#L223-L250)

```rust
/// 执行一轮 Pipeline（所有 Stage）
pub async fn run_loop(&mut self) -> Result<ControlFlow, PipelineError> {
    // 1. 移动数据到 Static Files
    self.move_to_static_files()?;

    let mut previous_stage = None;

    // 2. 按顺序执行每个 Stage
    for stage_index in 0..self.stages.len() {
        let stage = &self.stages[stage_index];
        let stage_id = stage.id();

        trace!(target: "sync::pipeline", stage = %stage_id, "Executing stage");

        // 3. 执行 Stage 直到完成
        let next = self.execute_stage_to_completion(
            previous_stage,
            stage_index
        ).await?;

        trace!(target: "sync::pipeline", stage = %stage_id, ?next, "Completed stage");

        // 4. 处理 Stage 的返回结果
        match next {
            ControlFlow::NoProgress { block_number } => {
                // 没有进度（已经是最新）
                if let Some(block_number) = block_number {
                    self.progress.update(block_number);
                }
            }
            ControlFlow::Continue { block_number } => {
                // 有进度，更新
                self.progress.update(block_number);
            }
            ControlFlow::Unwind { target, bad_block } => {
                // 需要 unwind（回滚）
                self.unwind(target, Some(bad_block.block.number))?;
                return Ok(ControlFlow::Unwind { target, bad_block });
            }
        }

        previous_stage = Some((stage_id, block_number));
    }

    // 5. 所有 Stage 执行完成
    Ok(ControlFlow::Continue { block_number: self.progress.minimum_block_number })
}
```

### ControlFlow - Stage 返回值

```rust
/// Stage 执行后的控制流
pub enum ControlFlow {
    /// 继续执行，有进度
    Continue {
        block_number: BlockNumber,
    },

    /// 继续执行，但没有进度（已同步到最新）
    NoProgress {
        block_number: Option<BlockNumber>,
    },

    /// 需要回滚（发现错误区块）
    Unwind {
        target: BlockNumber,        // 回滚到的目标区块
        bad_block: Box<SealedBlock>, // 错误的区块
    },
}
```

### execute_stage_to_completion - 执行单个 Stage

```rust
async fn execute_stage_to_completion(
    &mut self,
    previous_stage: Option<(StageId, BlockNumber)>,
    stage_index: usize,
) -> Result<ControlFlow, PipelineError> {
    let stage = &self.stages[stage_index];
    let stage_id = stage.id();

    // 1. 获取 Stage 的 checkpoint
    let checkpoint = provider.get_stage_checkpoint(stage_id)?
        .unwrap_or_default();

    // 2. 确定执行范围
    let target = self.max_block.unwrap_or(u64::MAX);

    // 3. 构建执行输入
    let input = ExecInput {
        target: Some(target),
        checkpoint: Some(checkpoint),
    };

    // 4. 执行 Stage
    let output = stage.execute(provider, input).await?;

    // 5. 提交结果到数据库
    provider.save_stage_checkpoint(stage_id, output.checkpoint)?;
    provider.commit()?;

    // 6. 返回控制流
    Ok(output.done)
}
```

### Pipeline 执行流程图

```
┌────────────────────────────────────────────────────────────────┐
│                     Pipeline.run_loop()                         │
└────────────────────────────────────────────────────────────────┘
                              ↓
         ┌────────────────────────────────────────┐
         │  move_to_static_files()                │
         │  (移动历史数据到静态文件)              │
         └────────────────────────────────────────┘
                              ↓
         ┌────────────────────────────────────────┐
         │  for stage in stages:                  │
         └────────────────────────────────────────┘
                              ↓
         ┌────────────────────────────────────────┐
         │  execute_stage_to_completion()         │
         │  1. 读取 checkpoint                    │
         │  2. stage.execute(input)               │
         │  3. 保存 checkpoint                    │
         │  4. commit                             │
         └────────────────────────────────────────┘
                              ↓
         ┌─────────────┬─────────────┬─────────────┐
         │  Continue   │ NoProgress  │   Unwind    │
         └─────────────┴─────────────┴─────────────┘
              │              │              │
              ↓              ↓              ↓
         更新进度       无需操作        触发回滚
         继续下一个      继续下一个      停止并 unwind
         Stage          Stage          所有 Stage
```

---

## Unwind 机制

### 什么时候需要 Unwind？

Unwind（回滚）发生在以下情况：

1. **共识错误**：执行某个区块时发现状态根不匹配
2. **DetachedHead**：本地链与主链分叉
3. **验证失败**：区块验证失败
4. **手动请求**：通过 API 请求 unwind

### Unwind 流程

```rust
/// 回滚 Pipeline 到指定区块
fn unwind(
    &mut self,
    target: BlockNumber,
    bad_block: Option<BlockNumber>,
) -> Result<(), PipelineError> {
    // 1. 反向执行所有 Stage 的 unwind
    for stage in self.stages.iter().rev() {  // 反向！
        let stage_id = stage.id();

        // 2. 构建 unwind 输入
        let input = UnwindInput {
            unwind_to: target,
            bad_block,
            // ...
        };

        // 3. 执行 Stage 的 unwind 方法
        let output = stage.unwind(provider, input)?;

        // 4. 更新 checkpoint
        provider.save_stage_checkpoint(stage_id, output.checkpoint)?;
    }

    // 5. 提交回滚
    provider.commit()?;

    Ok(())
}
```

### Unwind 示例

```
假设 Pipeline 同步到区块 1000，但发现区块 995 有问题：

执行顺序（Forward）:
  Header → Body → Sender → Execute → Merkle → ...
  (区块 1-1000)

Unwind 顺序（Backward）:
  ... → Merkle → Execute → Sender → Body → Header
  (回滚 995-1000)

每个 Stage 的 unwind 操作：
- Header: 删除区块头 995-1000
- Body: 删除区块体和交易
- Sender: 删除发送者映射
- Execute: 恢复状态到区块 994
- Merkle: 重新计算状态根到区块 994
```

---

## 与 Engine API 的关系

### Pipeline 在 Post-Merge 的角色

Post-Merge 后，区块链同步分为两个场景：

#### 场景 1：历史同步（Historical Sync）

```
┌─────────────────────────────────────────────────────────────┐
│  节点首次启动 / 需要同步大量历史区块                         │
└─────────────────────────────────────────────────────────────┘
                           ↓
              ┌────────────────────────┐
              │   Pipeline 负责        │
              │   同步历史数据         │
              └────────────────────────┘
                           ↓
      ┌────────────────────────────────────────┐
      │  1. HeaderStage: 下载区块头            │
      │  2. BodyStage: 下载区块体              │
      │  3. SenderRecoveryStage: 恢复签名      │
      │  4. ExecutionStage: 执行交易           │
      │  5. MerkleStage: 计算状态根            │
      │  6. ... 其他 Stage                     │
      └────────────────────────────────────────┘
                           ↓
              ┌────────────────────────┐
              │  同步到最新区块         │
              │  (接近链尖端)          │
              └────────────────────────┘
```

#### 场景 2：实时同步（Live Sync）

```
┌─────────────────────────────────────────────────────────────┐
│  节点已同步，接收 CL 的新区块                                │
└─────────────────────────────────────────────────────────────┘
                           ↓
              ┌────────────────────────┐
              │   Engine API 负责      │
              │   (不走 Pipeline)      │
              └────────────────────────┘
                           ↓
      ┌────────────────────────────────────────┐
      │  1. CL 通过 engine_newPayloadV3       │
      │     发送新区块                         │
      │  2. EL 直接执行验证                   │
      │     - 验证区块头                      │
      │     - 执行交易                        │
      │     - 验证状态根                      │
      │  3. 返回执行结果给 CL                 │
      │  4. CL 通过 engine_forkchoiceUpdated  │
      │     通知链头更新                      │
      └────────────────────────────────────────┘
```

### Pipeline vs Engine API

| 特性 | Pipeline | Engine API |
|------|----------|-----------|
| **使用场景** | 历史同步 | 实时同步 |
| **数据来源** | P2P 网络 | 共识层 (CL) |
| **执行方式** | 批量、分阶段 | 单个区块、即时 |
| **性能优化** | 批处理、并行 | 低延迟 |
| **是否需要 CL** | 否（可独立运行） | 是（必须） |
| **典型区块数** | 成千上万 | 1 个 |

### 代码位置对比

**Pipeline 执行**：
```rust
// crates/node/builder/src/launch/engine.rs
let pipeline = build_networked_pipeline(...);
// Pipeline 在后台运行，同步历史数据
```

**Engine API 执行**：
```rust
// crates/engine/tree/src/engine.rs
impl EngineApiTreeHandler {
    fn on_new_payload(&mut self, payload: ExecutionPayload) {
        // 直接执行单个区块
        self.execute_block(payload);
    }
}
```

---

## 完整同步流程示例：从创世块到最新区块

### 场景：全新节点首次同步

假设你启动一个全新的 reth 节点，连接到以太坊主网。让我们详细追踪整个同步过程。

#### 初始状态

```
节点启动时：
- 数据库为空（除了创世块配置）
- Local Head: Block #0 (Genesis)
- Network Head: Block #21,000,000 (假设当前链头)
- 需要同步：21,000,000 个区块
```

---

### 第一轮 Pipeline 执行

#### Stage 1: HeaderStage

**目标**：下载所有区块头

```
┌─────────────────────────────────────────────────────────────┐
│  HeaderStage 开始                                           │
└─────────────────────────────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  1. 从 tip_rx 接收同步目标         │
         │     target_tip = Block #21M        │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  2. 使用 ReverseHeadersDownloader  │
         │     从新到旧下载区块头             │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  3. P2P 网络请求                   │
         │     - 连接多个 peers               │
         │     - 批量请求 headers             │
         │     - 每批 192 个区块头            │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  4. 验证区块头                     │
         │     - 验证 PoW/PoS                 │
         │     - 验证 parent_hash 链          │
         │     - 验证时间戳                   │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  5. 保存到数据库                   │
         │     Headers 表: Block #0 → #21M    │
         │     保存 checkpoint: #21M          │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  返回: ControlFlow::Continue       │
         │        { block_number: 21_000_000 }│
         └────────────────────────────────────┘
```

**日志输出示例**：
```
INFO reth::cli: Starting reth node
INFO sync::pipeline: Stage Headers started
INFO sync::pipeline::headers: Downloading headers from 21000000 down to 0
INFO sync::pipeline::headers: Downloaded 1920000 headers (9.1% complete)
INFO sync::pipeline::headers: Downloaded 4200000 headers (20% complete)
...
INFO sync::pipeline::headers: Downloaded 21000000 headers (100% complete)
INFO sync::pipeline: Stage Headers completed in 5m32s
```

**关键点**：
- 下载是**反向**的（从新到旧），这样可以快速验证链的连续性
- 批量下载，每次请求多个区块头，减少网络往返
- 这个阶段**只**下载区块头，不下载交易数据

---

#### Stage 2: BodyStage

**目标**：下载所有区块体（交易列表）

```
┌─────────────────────────────────────────────────────────────┐
│  BodyStage 开始                                             │
└─────────────────────────────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  1. 读取 Headers 表                │
         │     已有区块头: #0 → #21M          │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  2. 使用 BodiesDownloader          │
         │     批量请求区块体                 │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  3. P2P 网络请求                   │
         │     for block in 0..21M:           │
         │       download_body(block)         │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  4. 验证区块体                     │
         │     - 验证交易列表的 Merkle 根     │
         │     - transactions_root 匹配？     │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  5. 保存到数据库                   │
         │     Bodies 表: 区块体元数据        │
         │     Transactions 表: 所有交易      │
         │     (约 2.1 亿笔交易)              │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  返回: ControlFlow::Continue       │
         │        { block_number: 21_000_000 }│
         └────────────────────────────────────┘
```

**日志输出示例**：
```
INFO sync::pipeline: Stage Bodies started
INFO sync::pipeline::bodies: Downloading bodies for 21M blocks
INFO sync::pipeline::bodies: Downloaded 500000 bodies (2.4% complete)
...
INFO sync::pipeline::bodies: Downloaded 21000000 bodies (100% complete)
INFO sync::pipeline: Stage Bodies completed in 3h15m
```

**关键点**：
- 这个阶段会下载**大量数据**（以太坊主网约 1TB+）
- 下载是正向的（从旧到新），按区块号顺序
- 空区块（如早期区块）下载很快，复杂区块（DeFi 交易多）较慢

---

#### Stage 3: SenderRecoveryStage

**目标**：从签名恢复所有交易的发送者地址

```
┌─────────────────────────────────────────────────────────────┐
│  SenderRecoveryStage 开始                                   │
└─────────────────────────────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  1. 读取 Transactions 表           │
         │     需要恢复: 2.1 亿笔交易         │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  2. 并行恢复签名                   │
         │     使用所有 CPU 核心              │
         │     for tx in transactions:        │
         │       sender = ecrecover(tx.sig)   │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  3. 保存到数据库                   │
         │     TransactionSenders 表          │
         │     tx_id → sender_address         │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  返回: ControlFlow::Continue       │
         │        { block_number: 21_000_000 }│
         └────────────────────────────────────┘
```

**日志输出示例**：
```
INFO sync::pipeline: Stage SenderRecovery started
INFO sync::pipeline::sender: Recovering senders for 210M transactions
INFO sync::pipeline::sender: Using 16 CPU cores
INFO sync::pipeline::sender: Recovered 50M senders (23.8% complete)
...
INFO sync::pipeline: Stage SenderRecovery completed in 2h45m
```

**性能关键点**：
- **CPU 密集型**：ECDSA 签名恢复需要大量计算
- **充分并行**：利用所有 CPU 核心
- **可缓存**：结果可以永久保存，不需要重复计算

---

#### Stage 4: ExecutionStage ⭐ 最重要

**目标**：执行所有交易，重建世界状态

```
┌─────────────────────────────────────────────────────────────┐
│  ExecutionStage 开始 - 这是最耗时的阶段                     │
└─────────────────────────────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  1. 初始化 EVM 执行器              │
         │     - 加载创世状态                 │
         │     - 准备 revm                    │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  2. 按区块顺序执行                 │
         │     for block in 0..21M:           │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  2.1 执行区块内所有交易            │
         │      for tx in block.txs:          │
         │        result = evm.execute(tx)    │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  2.2 更新账户状态                  │
         │      - 余额变化                    │
         │      - Nonce 增加                  │
         │      - 合约代码部署                │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  2.3 更新存储                      │
         │      - 合约 storage 变更           │
         │      - SSTORE 操作                 │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  2.4 生成 Receipt                  │
         │      - Gas 使用量                  │
         │      - Logs (Events)               │
         │      - 执行结果                    │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  3. 保存到数据库                   │
         │     PlainAccountState: 账户状态    │
         │     PlainStorageState: 合约存储    │
         │     Receipts: 交易回执             │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  返回: ControlFlow::Continue       │
         │        { block_number: 21_000_000 }│
         └────────────────────────────────────┘
```

**日志输出示例**：
```
INFO sync::pipeline: Stage Execution started
INFO sync::pipeline::execution: Executing 21M blocks with 210M transactions
INFO sync::pipeline::execution: Executed block #1000000 (4.8% complete, 15M gas/s)
INFO sync::pipeline::execution: Executed block #5000000 (23.8% complete, 18M gas/s)
...
INFO sync::pipeline::execution: Executed block #21000000 (100% complete)
INFO sync::pipeline: Stage Execution completed in 18h30m
```

**关键点**：
- **最耗时的阶段**：在普通硬件上可能需要 10-20 小时
- **CPU + 内存密集**：EVM 执行需要大量计算和状态访问
- **顺序执行**：必须按区块顺序执行（状态依赖）
- **批量提交**：不是每个区块都刷盘，而是批量提交减少 I/O

---

#### Stage 5-7: Hashing Stages

**目标**：计算账户和存储的 Keccak256 hash

```
┌─────────────────────────────────────────────────────────────┐
│  AccountHashingStage                                        │
└─────────────────────────────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  读取 PlainAccountState            │
         │  计算 keccak256(address)           │
         │  写入 HashedAccounts               │
         └────────────────────────────────────┘
                          ↓
┌─────────────────────────────────────────────────────────────┐
│  StorageHashingStage                                        │
└─────────────────────────────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  读取 PlainStorageState            │
         │  计算 keccak256(address, slot)     │
         │  写入 HashedStorages               │
         └────────────────────────────────────┘
```

**日志输出示例**：
```
INFO sync::pipeline: Stage AccountHashing completed in 45m
INFO sync::pipeline: Stage StorageHashing completed in 1h20m
```

---

#### Stage 8: MerkleStage

**目标**：计算 Merkle Patricia Trie，得到 state_root

```
┌─────────────────────────────────────────────────────────────┐
│  MerkleStage 开始                                           │
└─────────────────────────────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  1. 构建 State Trie                │
         │     - 遍历 HashedAccounts          │
         │     - 插入 MPT                     │
         │     - 计算中间节点 hash            │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  2. 构建 Storage Tries             │
         │     - 为每个合约构建 Storage Trie  │
         │     - 并行计算多个合约             │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  3. 验证 state_root                │
         │     computed_root == block.root?   │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  4. 保存 Trie 节点                 │
         │     AccountsTrie 表                │
         │     StoragesTrie 表                │
         └────────────────────────────────────┘
```

**日志输出示例**：
```
INFO sync::pipeline: Stage Merkle started
INFO sync::pipeline::merkle: Computing state root for 21M blocks
INFO sync::pipeline::merkle: Computed roots for 5M blocks (23.8%)
...
INFO sync::pipeline: Stage Merkle completed in 6h45m
```

**关键点**：
- **计算密集**：大量 Keccak256 hash 计算
- **并行优化**：多个 Storage Trie 可以并行计算
- **增量更新**：只重新计算变化的分支（见第二章）

---

#### Stage 9-11: Indexing Stages

**目标**：创建各种索引以加速查询

```
TransactionLookupStage:
  tx_hash → block_number 映射

IndexStorageHistoryStage:  ⭐ 先执行
  (address, slot) → [block_numbers] 存储变更历史

IndexAccountHistoryStage:  ⭐ 后执行
  address → [block_numbers] 账户变更历史
```

**日志输出示例**：
```
INFO sync::pipeline: Stage TransactionLookup completed in 1h15m
INFO sync::pipeline: Stage IndexStorageHistory completed in 2h30m  ⭐ 先执行
INFO sync::pipeline: Stage IndexAccountHistory completed in 3h45m  ⭐ 后执行
```

---

#### Stage 12: FinishStage

```
┌─────────────────────────────────────────────────────────────┐
│  FinishStage                                                │
│  - 标记同步完成                                             │
│  - 清理临时数据                                             │
│  - 更新节点状态                                             │
└─────────────────────────────────────────────────────────────┘
```

---

### 第一轮 Pipeline 完成

```
┌─────────────────────────────────────────────────────────────┐
│  Pipeline 第一轮执行完成                                    │
│                                                              │
│  总耗时: ~40 小时（取决于硬件）                             │
│  数据库大小: ~1.4 TB                                        │
│  状态:                                                       │
│    - Headers: ✅ 21M 个区块头                               │
│    - Bodies: ✅ 21M 个区块体                                │
│    - Execution: ✅ 210M 笔交易已执行                        │
│    - State: ✅ 完整的世界状态                               │
│    - Merkle: ✅ 所有 state_root 已验证                      │
│    - Indexes: ✅ 所有索引已创建                             │
│                                                              │
│  Local Head: Block #21,000,000                              │
│  Network Head: Block #21,000,050 (新增了 50 个区块)        │
└─────────────────────────────────────────────────────────────┘
```

---

### 第二轮 Pipeline 执行（追赶新区块）

Pipeline 会立即开始第二轮执行，同步在第一轮期间新产生的区块：

```
┌─────────────────────────────────────────────────────────────┐
│  Pipeline 第二轮开始                                        │
│  范围: Block #21,000,001 → #21,000,050                     │
└─────────────────────────────────────────────────────────────┘
                          ↓
         各 Stage 快速执行（只有 50 个区块）
         HeaderStage: 下载 50 个区块头 (5 秒)
         BodyStage: 下载 50 个区块体 (30 秒)
         SenderRecovery: 恢复 ~5000 个签名 (10 秒)
         Execution: 执行 ~5000 笔交易 (2 分钟)
         ... 其他 Stage
                          ↓
         完成时间: ~5 分钟
```

---

### 第三轮及之后：追赶模式

```
第三轮: 同步 Block #21,000,051 → #21,000,055 (5 个区块)
第四轮: 同步 Block #21,000,056 → #21,000,056 (1 个区块)
第五轮: NoProgress - 已追上网络！
```

当 Pipeline 返回 `ControlFlow::NoProgress` 时，说明已经同步到最新。

---

### 切换到实时同步（Engine API）

```
┌─────────────────────────────────────────────────────────────┐
│  Pipeline 进入空闲状态                                      │
│  Local Head == Network Head                                 │
└─────────────────────────────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  Engine API 接管                   │
         │  监听 CL 的 newPayload 调用        │
         └────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  每 12 秒接收一个新区块            │
         │  直接执行验证（不走 Pipeline）     │
         └────────────────────────────────────┘
```

从此刻起：
- **新区块**：通过 Engine API 实时处理
- **历史同步**（如果需要）：Pipeline 仍然可以在后台运行

---

### 完整时间线总结

```
时刻 T0: 节点启动
  ↓
T0 → T1 (1 小时):
  - 网络初始化
  - 发现 peers
  - 开始下载 headers
  ↓
T1 → T6 (5 小时):
  - Headers 下载完成
  - Bodies 开始下载
  ↓
T6 → T10 (4 小时):
  - Bodies 下载完成
  - SenderRecovery 完成
  ↓
T10 → T28 (18 小时):
  - Execution Stage 执行
  - 这是最慢的阶段
  ↓
T28 → T35 (7 小时):
  - Hashing Stages
  - Merkle Stage
  ↓
T35 → T40 (5 小时):
  - Indexing Stages
  - Finish
  ↓
T40 → T42 (2 小时):
  - 追赶新区块（多轮 Pipeline）
  ↓
T42+:
  - 完全同步
  - 切换到 Engine API 实时模式
```

**总耗时**：约 42 小时（在配置良好的硬件上）

---

### 中断与恢复

如果节点在同步过程中崩溃或关闭：

```
场景: 节点在 ExecutionStage 执行到 Block #5,000,000 时崩溃

重启后:
┌─────────────────────────────────────────────────────────────┐
│  1. 读取 Checkpoint                                         │
│     Headers: ✅ #21M                                        │
│     Bodies: ✅ #21M                                         │
│     SenderRecovery: ✅ #21M                                 │
│     Execution: ⚠️  #5M (未完成)                            │
└─────────────────────────────────────────────────────────────┘
                          ↓
         ┌────────────────────────────────────┐
         │  2. 从断点继续                     │
         │     ExecutionStage 从 #5M 开始     │
         │     无需重新下载 Headers/Bodies    │
         └────────────────────────────────────┘
                          ↓
         继续执行 #5M → #21M
```

**关键优势**：
- 每个 Stage 独立保存 checkpoint
- 崩溃后无需从头开始
- 只需继续未完成的 Stage

---

### 数据库状态变化

同步过程中各表的数据量变化：

| 时间点 | Headers | Bodies | Transactions | PlainAccountState | HashedAccounts | AccountsTrie |
|--------|---------|---------|--------------|-------------------|----------------|--------------|
| T0 (启动) | 0 | 0 | 0 | 0 | 0 | 0 |
| T6 (Headers完成) | 21M | 0 | 0 | 0 | 0 | 0 |
| T10 (Bodies完成) | 21M | 21M | 210M | 0 | 0 | 0 |
| T28 (Execution完成) | 21M | 21M | 210M | ~200M | 0 | 0 |
| T35 (Merkle完成) | 21M | 21M | 210M | ~200M | ~200M | ~1B nodes |
| T40 (完全同步) | 21M | 21M | 210M | ~200M | ~200M | ~1B nodes |

---

## 总结

### Pipeline 的核心价值

1. **高效同步**：
   - 批量下载
   - 分阶段处理
   - 并行优化

2. **可恢复性**：
   - Checkpoint 机制
   - 每个 Stage 独立保存进度
   - 崩溃后可从断点继续

3. **模块化**：
   - 每个 Stage 职责单一
   - 易于测试和优化
   - 可以灵活组合

4. **错误处理**：
   - Unwind 机制
   - 自动回滚到正确状态
   - 支持链重组

### Pipeline 在节点生命周期中的位置

```
节点启动
  ↓
初始化组件（Network, Pool, EVM, etc.）
  ↓
┌──────────────────────────────────────┐
│  Pipeline 准备                       │
│  - 创建所有 Stage                    │
│  - 配置下载器                        │
│  - 设置 checkpoint                   │
└──────────────────────────────────────┘
  ↓
┌──────────────────────────────────────┐
│  Pipeline 运行（后台）               │
│  - 同步历史区块                      │
│  - 从 genesis 到接近链尖端           │
└──────────────────────────────────────┘
  ↓
┌──────────────────────────────────────┐
│  Engine API 接管                     │
│  - 接收 CL 的新区块                  │
│  - 实时处理                          │
└──────────────────────────────────────┘
  ↓
节点正常运行
```

### 相关文档

- [第 0 章：高层视图](00-high-view.md) - 了解 Pipeline 在整体架构中的位置
- [第 1 章：交易执行](01-transaction-execution.md) - ExecutionStage 的详细逻辑
- [第 2 章：存储架构](02-storage-architecture.md) - Pipeline 如何使用数据库和 Static Files
- [附录：节点启动](appendix-node-startup.md) - Pipeline 初始化的详细步骤

---

**下一步建议**：

如果你想深入了解：
1. **某个特定 Stage** → 查看 `crates/stages/stages/src/stages/` 下的实现
2. **Checkpoint 机制** → 查看 `crates/stages/api/src/stage.rs`
3. **Unwind 详细逻辑** → 查看各个 Stage 的 `unwind()` 方法
4. **Pipeline 性能优化** → 查看各 Stage 的批处理和并行实现

如有疑问，随时提问！
