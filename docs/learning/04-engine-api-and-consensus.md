# 第四章：Engine API 与共识引擎

## 目录
- [Engine API 是什么](#engine-api-是什么)
- [整体架构](#整体架构)
- [newPayload 流程](#newpayload-流程)
- [forkchoiceUpdated 流程](#forkchoiceupdated-流程)
- [区块树管理](#区块树管理)
- [区块验证规则](#区块验证规则)
- [重组（Reorg）处理](#重组reorg处理)
- [持久化机制](#持久化机制)
- [与 Pipeline 的协调](#与-pipeline-的协调)
- [完整流程示例](#完整流程示例)

---

## Engine API 是什么？

Engine API 是**以太坊执行层（EL）与共识层（CL）之间的通信接口**。Post-Merge 后，区块的产生和排序由共识层（Beacon Chain）负责，执行层负责执行和验证区块内的交易。

```
┌─────────────────────────────────────────────────────────────────┐
│                        以太坊节点架构                            │
│                                                                   │
│  ┌─────────────────┐                   ┌──────────────────────┐  │
│  │  共识层（CL）    │                   │   执行层（EL）        │  │
│  │  (Lighthouse,   │  ◄──Engine API──► │   (reth)             │  │
│  │   Prysm, etc.)  │                   │                      │  │
│  │                  │                   │                      │  │
│  │  - 出块/投票     │                   │  - 执行交易           │  │
│  │  - 共识规则      │                   │  - 管理状态           │  │
│  │  - 验证者管理    │                   │  - 维护 EVM           │  │
│  └─────────────────┘                   └──────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

### 核心 API 方法

Engine API 的主链路仍然围绕两个核心方法展开，但具体方法版本会随 fork 演进。当前代码同时实现了 JSON-RPC 方法 `engine_newPayloadV1` 到 `engine_newPayloadV5`、`engine_forkchoiceUpdatedV1` 到 `engine_forkchoiceUpdatedV4`、以及 `engine_getPayloadV1` 到 `engine_getPayloadV6`。对应的 Rust handler 是 `new_payload_v1` 到 `new_payload_v5`、`fork_choice_updated_v1` 到 `fork_choice_updated_v4`、`get_payload_v1` 到 `get_payload_v6`。

| JSON-RPC 方法 | Rust handler | 方向 | 作用 |
|------|------|------|------|
| `engine_newPayloadV1` 到 `engine_newPayloadV5` | `new_payload_v1` 到 `new_payload_v5` | CL → EL | CL 把新区块发给 EL 执行 |
| `engine_forkchoiceUpdatedV1` 到 `engine_forkchoiceUpdatedV4` | `fork_choice_updated_v1` 到 `fork_choice_updated_v4` | CL → EL | CL 告诉 EL 哪个是最新的规范链头 |
| `engine_getPayloadV1` 到 `engine_getPayloadV6` | `get_payload_v1` 到 `get_payload_v6` | CL → EL | CL 按 `payloadId` 取走正在构建或已构建的 payload |

```
每 12 秒一个 slot：
CL 确定出块者 → 区块被构建和广播
    ↓
CL 通过 engine_newPayloadV3 发送区块体给 EL
    ↓
EL 执行区块，返回 VALID/INVALID/SYNCING
    ↓
CL 通过 engine_forkchoiceUpdatedV3 更新规范链头
```

版本大致对应的新增字段：
- V1/V2：Paris/Shanghai 基础 payload 与 withdrawals。
- V3：Cancun，新增 blob versioned hashes 与 parent beacon block root。
- V4：Prague，新增 execution requests 校验。
- V5/V6：Amsterdam/Osaka 之后的 envelope 扩展，当前代码里 `getPayloadV6` 返回包含 Block Access Lists 的 envelope。

### 状态返回值

`newPayload` 可返回三种状态：

```rust
pub enum PayloadStatusEnum {
    /// 区块有效，已执行
    Valid,
    /// 区块无效，附带验证错误字符串
    Invalid { validation_error: String },
    /// 正在同步，无法立即验证
    Syncing,
    /// 已接受（区块结构合法，但尚未执行）
    Accepted,
}
```

> **注意**：`latestValidHash` 是 `PayloadStatus` 外层结构体的字段（`Option<B256>`），与 `PayloadStatusEnum` 分开存放。`Invalid` 变体只携带 `validation_error`（错误描述字符串）。

---

## 整体架构

### 四层架构

```
┌────────────────────────────────────────────────────────────────┐
│  第一层：RPC 接口                                               │
│  EngineApi (crates/rpc/rpc-engine-api/src/engine_api.rs)       │
│  - 实现 engine_newPayloadV1-V5                                  │
│  - 实现 engine_forkchoiceUpdatedV1-V4                           │
│  - 实现 engine_getPayloadV1-V6                                  │
│  - 版本相关字段验证（blob, withdrawals, requests, BAL 等）       │
└────────────────────────────┬───────────────────────────────────┘
                             │ BeaconEngineMessage (tokio channel)
                             ▼
┌────────────────────────────────────────────────────────────────┐
│  第二层：请求调度层（engine.rs）                                 │
│  EngineHandler                                                   │
│  ├── incoming_requests: 接收 RPC 层发来的 BeaconEngineMessage    │
│  ├── downloader:        P2P 按需下载缺失块                       │
│  └── handler: EngineApiRequestHandler                           │
│        ├── to_tree:   crossbeam Sender → 第三层                  │
│        └── from_tree: tokio Receiver  ← 第三层                  │
└────────────────────────────┬───────────────────────────────────┘
                             │ crossbeam channel (FromEngine)
                             ▼
┌────────────────────────────────────────────────────────────────┐
│  第三层：区块树处理器                                           │
│  EngineApiTreeHandler (crates/engine/tree/src/tree/mod.rs)     │
│  - 核心逻辑：执行区块、管理树状态、处理 reorg                    │
│  - 维护 TreeState（内存中的区块树）                              │
│  - 处理 newPayload / forkchoiceUpdated                          │
└────────────────────────────┬───────────────────────────────────┘
                             │ PersistenceHandle
                             ▼
┌────────────────────────────────────────────────────────────────┐
│  第四层：持久化层                                               │
│  StaticFileProducer + DatabaseProvider                          │
│  - 异步写入 MDBX 数据库和 Static Files                          │
│  - 独立线程，不阻塞 Engine API 响应                             │
└────────────────────────────────────────────────────────────────┘
```

### 各层详解

#### 第一层：EngineApi（RPC 接口）

代码位置：[`crates/rpc/rpc-engine-api/src/engine_api.rs`](../../crates/rpc/rpc-engine-api/src/engine_api.rs)

接收 CL 的 JSON-RPC 调用，做版本相关的字段校验后，把请求包装成 `BeaconEngineMessage` 发入 tokio channel，立即 await 对方的 `oneshot` 响应：

```rust
// 典型结构（简化）
async fn new_payload_v3(&self, payload, fork_specific_fields)
    -> RpcResult<PayloadStatus>
{
    // 1. 按 EngineApiMessageVersion 做版本字段校验
    self.payload_validator.validate_version_specific_fields(...)?;

    // 2. 发到第二层，等待响应
    let (tx, rx) = oneshot::channel();
    self.to_handler.send(BeaconEngineMessage::NewPayload { payload, tx })?;
    rx.await?   // 阻塞等待，直到第三层执行完毕
}
```

#### 第二层：EngineHandler + EngineApiRequestHandler（调度层）

代码位置：[`crates/engine/tree/src/engine.rs`](../../crates/engine/tree/src/engine.rs)

`EngineHandler` 是一个 `ChainHandler`，在 `ChainOrchestrator` 的主循环里被 poll，它轮询三个事件源：

```rust
// engine.rs:81 — EngineHandler::poll()
fn poll(&mut self, cx: &mut Context<'_>) -> Poll<HandlerEvent<Self::Event>> {
    loop {
        // 1. 先把 handler（EngineApiRequestHandler）排空
        while let Poll::Ready(ev) = self.handler.poll(cx) {
            match ev {
                RequestHandlerEvent::HandlerEvent(HandlerEvent::BackfillAction(target)) => {
                    self.downloader.on_action(DownloadAction::Clear); // 清空下载队列
                    return Poll::Ready(HandlerEvent::BackfillAction(target)); // 冒泡给 ChainOrchestrator
                }
                RequestHandlerEvent::Download(req) => {
                    self.downloader.on_action(DownloadAction::Download(req)); // 委托下载器
                }
                // ...
            }
        }

        // 2. 接收 RPC 层发来的新请求
        if let Poll::Ready(Some(req)) = self.incoming_requests.poll_next_unpin(cx) {
            self.handler.on_event(FromEngine::Request(req.into()));
            continue
        }

        // 3. 推进 P2P 下载器，把下载到的块传给 handler
        if let Poll::Ready(outcome) = self.downloader.poll(cx) {
            if let DownloadOutcome::Blocks(blocks) = outcome {
                self.handler.on_event(FromEngine::DownloadedBlocks(blocks));
            }
            continue
        }

        return Poll::Pending
    }
}
```

`EngineApiRequestHandler` 是薄薄的桥接层，把所有 `FromEngine` 事件直接发到第三层的 crossbeam channel：

```rust
// engine.rs:197
fn on_event(&mut self, event: FromEngine<Self::Request, Self::Block>) {
    let _ = self.to_tree.send(event);  // 发给 EngineApiTreeHandler
}
```

#### 第三层：EngineApiTreeHandler（区块树处理器）

代码位置：[`crates/engine/tree/src/tree/mod.rs`](../../crates/engine/tree/src/tree/mod.rs)

独立线程运行，从 crossbeam channel 接收 `FromEngine` 消息，执行核心逻辑（区块执行、树管理、reorg），结果通过 `outgoing` 发回第二层。

#### 第四层：持久化层

`EngineApiTreeHandler` 持有一个 `PersistenceHandle`（异步任务句柄），达到阈值时把已执行块批量写入 MDBX 和 Static Files，完全异步，不阻塞 Engine API 响应。

---

### EngineApiTreeHandler 结构体

代码位置：[`crates/engine/tree/src/tree/mod.rs:227`](../../crates/engine/tree/src/tree/mod.rs#L227)

```rust
pub struct EngineApiTreeHandler<N, P, T, V, C> {
    provider: P,                    // 数据库访问
    consensus: Arc<dyn FullConsensus<N>>,  // 共识验证规则
    payload_validator: V,           // Payload 版本字段验证
    state: EngineApiTreeState<N>,   // 树状态（已执行块、缓冲区、无效头）
    incoming: Receiver<FromEngine<...>>, // 接收请求
    outgoing: UnboundedSender<EngineApiEvent<N>>, // 发出事件
    persistence: PersistenceHandle<N>,   // 持久化句柄
    canonical_in_memory_state: CanonicalInMemoryState<N>, // 规范链内存状态
    payload_builder: PayloadBuilderHandle<T>, // 区块构建器（用于 payload attrs）
    evm_config: C,                  // EVM 配置
    // ...
}
```

---

## newPayload 流程

### 完整调用链

```
CL: engine_newPayloadV3(payload, fork-specific side fields)
    │
    ▼
EngineApi::new_payload_v3()                    [engine_api.rs]
    │  1. 根据版本验证 withdrawals/blob hashes/requests/BAL 等字段
    │  2. 将 RPC payload 转换为 ExecutionData
    │
    ▼ async channel
EngineHandler::poll()                          [engine.rs]
    │
    ▼
EngineApiTreeHandler::on_engine_message()      [tree/mod.rs:1415]
    │
    ▼
EngineApiTreeHandler::on_new_payload()         [tree/mod.rs:556]
    │
    ├─ 如果 backfill 正在运行（!is_idle()） ──→ try_buffer_payload() → 返回 SYNCING
    │
    └─ backfill 空闲（is_idle()） ──→ try_insert_payload() → insert_block_or_payload()
            │
            ├─ VALID：区块已成功执行并插入树
            ├─ SYNCING：父块缺失，区块已缓冲
            └─ INVALID：区块验证失败（附带 latestValidHash）
```

### on_new_payload 核心逻辑

代码位置：[`crates/engine/tree/src/tree/mod.rs:556`](../../crates/engine/tree/src/tree/mod.rs#L556)

```rust
fn on_new_payload(&mut self, payload: T::ExecutionData)
    -> Result<TreeOutcome<PayloadStatus>, InsertBlockFatalError>
{
    // 1. 检查祖先块是否无效（如果祖先块已知无效，直接返回 INVALID）
    if let Some(invalid) = self.find_invalid_ancestor(&payload) {
        let status = self.handle_invalid_ancestor_payload(payload, invalid)?;
        return Ok(TreeOutcome::new(status));
    }

    // 2. 根据 backfill 状态决定：idle → 尝试插入；否则 → 缓冲
    let status = if self.backfill_sync_state.is_idle() {
        self.try_insert_payload(payload)?   // → VALID / SYNCING / INVALID
    } else {
        self.try_buffer_payload(payload)?   // → SYNCING
    };

    // 3. 如果这个区块是当前同步目标且有效，触发 MakeCanonical 事件
    let mut outcome = TreeOutcome::new(status);
    if outcome.outcome.is_valid() && self.is_sync_target_head(block_hash) {
        if self.state.tree_state.canonical_block_hash() != block_hash {
            outcome = outcome.with_event(TreeEvent::TreeAction(
                TreeAction::MakeCanonical { sync_target_head: block_hash }
            ));
        }
    }

    Ok(outcome)
}
```

### insert_block_or_payload 核心逻辑

代码位置：[`crates/engine/tree/src/tree/mod.rs:2577`](../../crates/engine/tree/src/tree/mod.rs#L2577)

```rust
fn insert_block_or_payload(&mut self, block_id: BlockWithParent, input, execute, convert_to_block)
    -> Result<InsertPayloadOk, Err>
{
    // 1. 先检查内存中是否已存在（by hash）
    if self.state.tree_state.sealed_header_by_hash(&block_id.block.hash).is_some() {
        convert_to_block(self, input)?;
        return Ok(InsertPayloadOk::AlreadySeen(BlockStatus::Valid));
    }

    // 2. 仅当 block.number <= last_persisted 时才查数据库
    //    （新区块 number > last_persisted，跳过此步骤）
    if block_id.block.number <= self.persistence_state.last_persisted_block.number {
        if self.provider.sealed_header_by_hash(block_id.block.hash)?.is_some() {
            return Ok(InsertPayloadOk::AlreadySeen(BlockStatus::Valid));
        }
    }

    // 3. 检查父块状态是否可用（通过 state_provider_builder）
    match self.state_provider_builder(block_id.parent) {
        Ok(None) => {
            // 父块状态不可用，直接插入缓冲区（注意：不通过 buffer_block()）
            let block = convert_to_block(self, input)?;
            self.state.buffer.insert_block(block);
            return Ok(InsertPayloadOk::Inserted(BlockStatus::Disconnected { .. }));
        }
        Err(err) => return Err(...),
        Ok(Some(_)) => {}  // 父块状态就绪，继续执行
    }

    // 4. 执行区块（验证 + 执行所有交易）
    let executed = execute(&mut self.payload_validator, input, ctx)?;

    // 5. 如果父块是当前规范链头，先设置 pending block（注意：在 insert_executed 之前）
    if self.state.tree_state.canonical_block_hash() == executed.recovered_block().parent_hash() {
        self.canonical_in_memory_state.set_pending_block(executed.clone());
    }

    // 6. 插入执行结果到树状态
    self.state.tree_state.insert_executed(executed.clone());

    // 7. 发出插入事件（是否 fork 决定事件类型）
    let engine_event = if is_fork {
        ConsensusEngineEvent::ForkBlockAdded(executed, elapsed)
    } else {
        ConsensusEngineEvent::CanonicalBlockAdded(executed, elapsed)
    };
    self.emit_event(EngineApiEvent::BeaconConsensus(engine_event));

    Ok(InsertPayloadOk::Inserted(BlockStatus::Valid))
}
```

> **重要**：`try_connect_buffered_blocks()` **不在** `insert_block_or_payload` 中调用，而是在调用方 `try_insert_payload()` 中，当 `insert_payload()` 返回 `Inserted(BlockStatus::Valid)` 时才调用：
>
> ```rust
> // try_insert_payload 中：
> InsertPayloadOk::Inserted(BlockStatus::Valid) => {
>     latest_valid_hash = Some(block_hash);
>     self.try_connect_buffered_blocks(num_hash)?;  // ← 在这里
>     PayloadStatusEnum::Valid
> }
> ```

### 区块缓冲机制

当父块状态缺失时，区块进入缓冲区：

```
正常情况（父块已知）:
  parent(block N-1) 已执行 → 直接执行 block N → 插入树

缺失父块（乱序到达）:
  block N 到达，但 block N-1 未知
    ↓
  buffer_block(N)  →  存入 block_buffer
    ↓
  返回 SYNCING
    ↓
  之后 block N-1 到达，执行成功
    ↓
  try_connect_buffered_blocks(N-1.hash)
    ↓
  从 buffer 取出 N，执行它
```

---

## forkchoiceUpdated 流程

`forkchoiceUpdated` 是最复杂的方法，它告诉 EL 哪个区块是当前的规范链头（head）、安全块（safe）和最终确定块（finalized）。

### ForkchoiceState 结构

```rust
pub struct ForkchoiceState {
    /// 最新的规范链头（CL 认为的最新区块）
    pub head_block_hash: B256,
    /// 安全块（超过 2/3 验证者投票确认）
    pub safe_block_hash: B256,
    /// 最终确定块（不可回滚）
    pub finalized_block_hash: B256,
}
```

### 四阶段处理流程

代码位置：[`crates/engine/tree/src/tree/mod.rs:989`](../../crates/engine/tree/src/tree/mod.rs#L989)

```
on_forkchoice_updated(state, payload_attrs)
    │
    ├─ 阶段 1：validate_forkchoice_state()
    │    - head hash 是否为零（非法）
    │    - head 是否是已知无效块的后代
    │    - pipeline 是否正在运行（若是，返回 SYNCING）
    │
    ├─ 阶段 2：handle_canonical_head()
    │    - head 已经是当前规范链头？
    │    - 更新 safe/finalized 哈希
    │    - 如果有 payload_attrs，开始构建新区块
    │    - 返回 VALID + payloadId（如果有 attrs）
    │
    ├─ 阶段 3：apply_chain_update()
    │    - head 存在于内存树中
    │    - on_new_head()：从 head 向上走，找到与规范链的分叉点
    │    - 确定是链扩展还是 reorg
    │    - on_canonical_chain_update()：更新内存中的规范链状态
    │    - 处理 payload_attrs
    │    - 返回 VALID + payloadId
    │
    └─ 阶段 4：handle_missing_block()（兜底）
         - head 块未知，发出下载请求
         - 返回 SYNCING
```

### 阶段 1：验证

```rust
fn validate_forkchoice_state(&mut self, state: ForkchoiceState)
    -> ProviderResult<Option<OnForkChoiceUpdated>>
{
    // 零哈希意味着节点还没准备好
    if state.head_block_hash.is_zero() {
        return Ok(Some(OnForkChoiceUpdated::invalid_state()));
    }

    // 如果 head 是已知无效块的后代，直接返回 INVALID
    let lowest = self.lowest_buffered_ancestor_or(state.head_block_hash);
    if let Some(status) = self.check_invalid_ancestor(lowest)? {
        return Ok(Some(OnForkChoiceUpdated::with_invalid(status)));
    }

    // Pipeline 正在运行时返回 SYNCING（用 !is_idle()，而非 is_active()）
    if !self.backfill_sync_state.is_idle() {
        return Ok(Some(OnForkChoiceUpdated::syncing()));
    }

    Ok(None)  // 验证通过，继续
}
```

### 阶段 3：找到新规范链头

```rust
fn on_new_head(&self, new_head: BlockNumHash) -> Option<NewCanonicalChain<N>> {
    let mut new_chain = vec![];
    let mut current = new_head;

    // 从 new_head 向上走，直到找到已知的规范链节点
    loop {
        if let Some(executed) = self.state.tree_state.executed_block(current.hash) {
            new_chain.push(executed);
            current = executed.block.parent_num_hash();
        } else {
            break;  // 到达数据库中已持久化的规范链
        }
    }

    // 检查是否有 reorg（走到的节点是否还在当前规范链上）
    let old_canonical = self.canonical_blocks_between(current, canonical_head);

    if old_canonical.is_empty() {
        // 纯扩展（新块建在规范链头上）
        Some(NewCanonicalChain::Commit { new: new_chain })
    } else {
        // Reorg（新链与旧链有分叉）
        Some(NewCanonicalChain::Reorg {
            new: new_chain,
            old: old_canonical,
        })
    }
}
```

---

## 区块树管理

### TreeState 结构

代码位置：[`crates/engine/tree/src/tree/state.rs:24`](../../crates/engine/tree/src/tree/state.rs#L24)

```rust
pub struct TreeState<N: NodePrimitives> {
    /// 所有已执行块，按 hash 索引
    pub(crate) blocks_by_hash: B256Map<ExecutedBlock<N>>,

    /// 所有已执行块，按 block number 索引（可能有多个，存在 fork 时）
    pub(crate) blocks_by_number: BTreeMap<BlockNumber, Vec<ExecutedBlock<N>>>,

    /// parent_hash → child_hashes 映射（跟踪分叉）
    pub(crate) parent_to_child: B256Map<B256Set>,

    /// 当前规范链头
    pub(crate) current_canonical_head: BlockNumHash,
}
```

### 内存中的区块树示例

```
数据库（持久化）:
  ... → block 19_990_000 → block 19_990_001  (规范链，已写盘)

内存中的 TreeState（待持久化）:
                        ┌─ block 19_990_002 (fork A)
  block 19_990_001 ────┤
                        └─ block 19_990_002' (fork B)
                                 └─ block 19_990_003' (fork B 的子块)

current_canonical_head = block 19_990_002' 的 numhash
（假设 fork B 是当前规范链）
```

### 状态提供者

当需要执行一个区块时，需要其父块的状态（账户余额、合约存储等）：

```rust
fn state_provider(&self, parent_hash: B256) -> Option<StateProvider> {
    if parent_hash == canonical_db_head {
        // 父块在数据库中，直接用数据库状态
        Some(self.provider.state_by_block_hash(parent_hash))
    } else if let Some(executed) = self.tree_state.blocks_by_hash.get(&parent_hash) {
        // 父块在内存树中，用内存覆盖层
        Some(InMemoryStateProvider::new(executed.state, db_provider))
    } else {
        // 父块未知，无法执行
        None
    }
}
```

---

## 区块验证规则

### 验证时机

区块验证分两步：

```
1. Pre-execution 验证（不需要执行交易）
   - 区块头格式检查
   - Gas limit 合法性
   - Timestamp 单调递增
   - Parent hash 匹配
   - EIP 相关字段（withdrawals, blob_gas, etc.）

2. Post-execution 验证（执行完交易后）
   - state_root 匹配
   - receipts_root 匹配
   - gas_used 匹配
```

### 代码位置

[`crates/consensus/common/src/validation.rs`](../../crates/consensus/common/src/validation.rs)

```rust
/// 执行前验证（区块结构 + 各硬分叉字段）
pub fn validate_block_pre_execution<B, ChainSpec>(
    block: &SealedBlock<B>,
    chain_spec: &ChainSpec,
) -> Result<(), ConsensusError>
{
    // 验证 Post-Merge 各硬分叉字段（一个函数统一处理）
    post_merge_hardfork_fields(block, chain_spec)?;

    // 验证交易根哈希（transactions_root 匹配）
    if let Err(error) = block.ensure_transaction_root_valid() {
        return Err(ConsensusError::BodyTransactionRootDiff(error.into()))
    }

    Ok(())
}

/// post_merge_hardfork_fields 内部依次检查：
fn post_merge_hardfork_fields<B, ChainSpec>(block, chain_spec) -> Result<(), ConsensusError> {
    // 1. Ommers hash 必须等于空叔块哈希（Post-Merge 无叔块）
    let ommers_hash = block.body().calculate_ommers_root();
    if Some(block.ommers_hash()) != ommers_hash {
        return Err(ConsensusError::BodyOmmersHashDiff(...));
    }

    // 2. EIP-4895（Shanghai）：withdrawals 列表哈希匹配
    if chain_spec.is_shanghai_active_at_timestamp(block.timestamp()) {
        validate_shanghai_withdrawals(block)?;
    }

    // 3. EIP-4844（Cancun）：blob_gas_used 等于所有 blob 交易的 gas 之和
    if chain_spec.is_cancun_active_at_timestamp(block.timestamp()) {
        validate_cancun_gas(block)?;
    }

    // 4. EIP-7934（Osaka）：区块 RLP 大小不超过 MAX_RLP_BLOCK_SIZE（8MB）
    if chain_spec.is_osaka_active_at_timestamp(block.timestamp())
        && block.rlp_length() > MAX_RLP_BLOCK_SIZE
    {
        return Err(ConsensusError::BlockTooLarge { ... });
    }

    Ok(())
}
```

### 各 EIP 的验证字段

| EIP / 硬分叉 | 新增字段 | 验证内容 |
|-------------|---------|---------|
| Merge | `nonce=0, difficulty=0` | Post-merge 字段清零 |
| Shanghai (EIP-4895) | `withdrawals` | 提款列表哈希匹配 |
| Cancun (EIP-4844) | `blob_gas_used, excess_blob_gas` | Blob gas 计算正确 |
| Cancun (EIP-4788) | `parent_beacon_block_root` | Beacon root 存在且非零 |
| Osaka (EIP-7934) | `rlp_length ≤ MAX_RLP_BLOCK_SIZE` | 区块 RLP 编码大小不超过 8MB（8,388,608 字节） |

---

## 重组（Reorg）处理

### 什么是 Reorg？

当 CL 发现一条更长/更重的链时，会通过 `forkchoiceUpdated` 告知 EL 切换到新的规范链头。此时，如果新头不在当前规范链上，就触发 Reorg。

### Reorg 处理流程

```
FCU: head = block C'（不在当前规范链上）

当前状态:
  DB: A → B → C（规范链）
  内存: A → B → C' → D'（fork）

Reorg 检测（on_new_head）:
  从 D' 向上走：D' → C' → B（B 在规范链上，是分叉点）

NewCanonicalChain::Reorg {
    new: [C', D'],   // 要成为新规范链的块
    old: [C],        // 要被回退的块
}

执行 Reorg（on_canonical_chain_update）:
  1. 确保新链和旧链的块都在 TreeState 中（reinsert_reorged_blocks）
  2. canonical_in_memory_state.update_chain(Reorg { new:[C',D'], old:[C] })
     ── 切换内存规范链视图（不需要磁盘 unwind，全内存操作）
  3. notify_canon_state → 通知交易池等订阅方（附带 revert 信息）
  4. emit_event(CanonicalChainCommitted)
```

### 内存中的 Reorg

Reth 将最近的区块保持在内存中，大多数 reorg（几个块以内）完全在内存中完成，不需要磁盘 I/O：

```rust
fn on_canonical_chain_update(&mut self, chain_update: NewCanonicalChain<N>) {
    // 1. 更新 tree_state 记录的规范链头
    self.state.tree_state.set_canonical_head(chain_update.tip().num_hash());

    // 2. 提前生成链通知（用于后续 notify_canon_state）
    let tip = chain_update.tip().clone_sealed_header();
    let notification = chain_update.to_chain_notification();

    // 3. Reorg 时重新插入两条链的块（确保 tree_state 中都有记录）
    if let NewCanonicalChain::Reorg { new, old } = &chain_update {
        self.reinsert_reorged_blocks(new.clone());  // 新链块
        self.reinsert_reorged_blocks(old.clone());  // 旧链块（保留在树中，方便后续参考）
    }

    // 4. 更新内存中的规范链视图（update_chain 处理 Commit 和 Reorg 两种情况）
    self.canonical_in_memory_state.update_chain(chain_update);
    self.canonical_in_memory_state.set_canonical_head(tip.clone());

    // 5. 通知所有监听者（包括交易池、RPC 等）规范链已更新
    //    notification 中包含 reorg 信息（CommittedChain / RevertedChain）
    self.canonical_in_memory_state.notify_canon_state(notification);

    // 6. 发出引擎事件（注意：事件类型是 CanonicalChainCommitted，不是 ChainReverted）
    self.emit_event(ConsensusEngineEvent::CanonicalChainCommitted(
        Box::new(tip),
        start.elapsed(),
    ));
}
```

> **注意**：reth 不直接发出 `ChainReverted` 引擎事件。Reorg 信息通过 `notify_canon_state(notification)` 以 `CanonStateNotification` 的形式推送给订阅方（如交易池），让它们自行处理旧链上的交易。

---

## 持久化机制

### 为什么需要异步持久化？

Engine API 必须快速响应（12 秒一个 slot），但磁盘写入较慢。reth 使用**异步持久化**：区块先在内存中处理和返回响应，然后在后台写入磁盘。

```
newPayload → 执行区块（内存） → 立即返回 VALID
                 ↓
          （异步，不阻塞响应）
                 ↓
       advance_persistence() 检查阈值
                 ↓
       超过阈值 → 发送给持久化线程
                 ↓
       持久化线程写 MDBX + Static Files
                 ↓
       on_persistence_complete() 更新内存状态，释放缓存
```

### 持久化触发条件

代码位置：[`crates/engine/tree/src/tree/mod.rs`](../../crates/engine/tree/src/tree/mod.rs)

```rust
fn advance_persistence(&mut self) -> Result<(), PersistenceError> {
    // 只有在持久化任务空闲时才触发新的持久化
    if !self.persistence_state.in_progress() {
        let canonical_head = self.canonical_in_memory_state.get_canonical_head();

        // 如果内存中积累了足够多的块，触发持久化
        if self.should_persist(canonical_head) {
            self.persist_blocks(canonical_head);
        }
    }
    Ok(())
}
```

### 持久化完成后

持久化完成后，内存中对应的块可以被清理，释放内存：

```
持久化完成（on_persistence_complete）：
  1. 更新 persistence_state（记录已持久化到哪个块）
  2. 从 canonical_in_memory_state 移除已持久化的块
  3. 从 changeset_cache 清理旧的变更集
  4. 继续 advance_persistence()（可能有新的块需要持久化）
```

---

## 与 Pipeline 的协调

### 两种同步模式的互斥关系

Engine API（实时同步）和 Pipeline（历史同步）**不能同时运行**，原因是两者都需要写 MDBX 数据库，同时写会死锁。

协调者是 `ChainOrchestrator`（在第三章已介绍）：

```
ChainOrchestrator::poll_next_event()
    │
    ├─ 优先：poll backfill_sync（PipelineSync）
    │    Active → Pipeline 正在运行
    │           → Engine API 进入 SYNCING 模式（缓冲所有区块，不执行）
    │
    └─ 当 Pipeline 空闲：poll handler（Engine API 请求处理器）
             → 正常处理 newPayload / forkchoiceUpdated
```

### BackfillSyncState 在 Engine API 中的作用

```rust
// on_new_payload 中（正向逻辑）：
let status = if self.backfill_sync_state.is_idle() {
    self.try_insert_payload(payload)?   // backfill 空闲 → 执行
} else {
    self.try_buffer_payload(payload)?   // backfill 运行中 → 只缓冲，返回 SYNCING
};

// validate_forkchoice_state 中（负向逻辑）：
if !self.backfill_sync_state.is_idle() {
    // Pipeline 正在运行，返回 SYNCING
    return Ok(Some(OnForkChoiceUpdated::syncing()));
}
```

### 状态切换

```
节点启动时（落后很多）：
  Pipeline 运行 → Engine API 返回 SYNCING 给所有请求

Pipeline 完成（追上链头）：
  backfill_sync_state = Idle
  ChainOrchestrator 开始 poll handler
  Engine API 恢复正常处理 newPayload

Engine API 收到 FCU，发现落后太多：
  向 ChainOrchestrator 发出 BackfillAction::Start(target)
  Pipeline 重新启动
  Engine API 再次进入 SYNCING 模式
```

---

## 完整流程示例

### 场景：收到新区块（正常情况）

假设节点已完全同步，CL 产生了新的第 22,000,001 号区块：

```
T=0s: CL 产生 block 22,000,001（hash = 0xabcd...）

T=0.1s: CL 调用 engine_newPayloadV3(payload)
    EngineApi 验证 blob hashes ✓
    发送到 EngineApiTreeHandler

    EngineApiTreeHandler::on_new_payload():
      - 检查父块(22,000,000)状态 → 在内存中 ✓
      - 执行区块：
          执行 150 笔交易
          更新状态
          验证 state_root ✓
      - 插入到 TreeState
      - 发出 CanonicalBlockAdded 事件
      → 返回 VALID

T=0.15s: engine_newPayloadV3 返回 PayloadStatus::Valid

T=0.5s: CL 调用 engine_forkchoiceUpdatedV3(
    head = 0xabcd...,    // block 22,000,001
    safe = 0x1234...,    // block 22,000,000
    finalized = 0x5678.. // block 21,999,936
)
    EngineApiTreeHandler::on_forkchoice_updated():
      阶段1: head 非零 ✓，非无效块 ✓，Pipeline 空闲 ✓
      阶段2: head(0xabcd) != 当前规范头(0xold)，跳过
      阶段3: apply_chain_update()
        on_new_head(): 新链 = [block 22,000,001]，无 reorg
        on_canonical_chain_update(): 追加到规范链
        更新 safe/finalized
        没有 payload_attrs，不构建新区块
      → 返回 VALID, payloadId = null

T=0.55s: engine_forkchoiceUpdatedV3 返回 ForkchoiceUpdated { VALID, null }

T=0.6s: advance_persistence() 检查
    内存中积累了 N 个块，超过阈值
    → 发送给持久化线程

T=1.2s: 持久化线程写入 MDBX + Static Files
    on_persistence_complete()
    → 清理已持久化块的内存缓存
```

### 场景：收到 Reorg

```
当前规范链: ... → A(100) → B(101) → C(102) ← head
CL 发现更重的链: ... → A(100) → B'(101) → C'(102) → D'(103)

T=0: CL 发送 newPayload(B'), newPayload(C'), newPayload(D')
     每个都返回 VALID（执行成功）
     TreeState 现在包含两条链：
       B(101) 和 B'(101)
       C(102) 和 C'(102)
       D'(103)

T=1: CL 调用 FCU(head=D'(103))

     on_forkchoice_updated():
       阶段1: 验证通过
       阶段2: D'(103) 不是当前规范头，跳过
       阶段3: apply_chain_update()
         on_new_head(D'): 沿 D' → C' → B' → A 向上走
           A 在规范链上，找到分叉点
           new = [B', C', D']
           old = [B, C]（被替换的块）
         NewCanonicalChain::Reorg { new, old }
         on_canonical_chain_update():
           set_canonical_head(D'.num_hash)
           reinsert_reorged_blocks([B', C', D'])  // 确保新链在树中
           reinsert_reorged_blocks([B, C])         // 旧链块也保留在树中
           canonical_in_memory_state.update_chain(Reorg { new, old })
           canonical_in_memory_state.notify_canon_state(notification)
             └→ 交易池收到 CanonStateNotification::Revert，重新入池 B/C 的交易
           emit_event(CanonicalChainCommitted(D'.header, elapsed))
       → 返回 VALID
```

### 场景：节点落后，需要重新触发 Pipeline

```
CL 调用 FCU(head = block 22,100,000)
节点本地只有 block 21,990,000

on_forkchoice_updated():
  阶段1: 验证通过
  阶段2: head 不是规范头
  阶段3: 内存树中没有这个 block
  阶段4: handle_missing_block()
    发出 Download 事件（开始下载缺失块）
    → 返回 SYNCING

（CL 会重试，节点通过 P2P 慢慢下载中间块）

当 EngineApiRequestHandler 判断落后太多：
  发出 BackfillAction::Start(target = 22,100,000)
  → Pipeline 重新启动，快速追赶
  → Engine API 进入纯 SYNCING 模式
```

---

## 总结

### Engine API 的核心设计理念

1. **异步响应**：通过 channel 将 RPC 请求和树处理器解耦，RPC 可以快速返回

2. **内存优先**：最近的区块保持在内存中，避免频繁磁盘 I/O，实现低延迟响应

3. **四阶段 FCU**：forkchoiceUpdated 按验证→已知规范头→树内重组→未知块下载顺序处理，快速处理常见情况

4. **缓冲机制**：乱序到达的区块先缓冲，父块到达后自动连接，优雅处理网络乱序

5. **互斥保护**：通过 `backfill_sync_state` 和 `ChainOrchestrator` 确保 Pipeline 和 Engine API 不会同时写数据库

6. **错误分类**：严格区分验证错误（返回 INVALID）和内部错误（panic/fatal），保证 API 语义正确

### 关键路径性能

| 操作 | 耗时目标 | 瓶颈 |
|------|---------|------|
| newPayload（正常区块） | < 500ms | 交易执行 |
| forkchoiceUpdated | < 100ms | 内存树操作 |
| Reorg（< 10 块） | < 200ms | 内存状态回退 |
| 持久化（异步） | 1-5s | 磁盘写入 |

### 相关文档

- [第三章：Pipeline 与区块链同步](03-pipeline-and-sync.md) - Pipeline 历史同步，与 Engine API 的关系
- [第一章：交易执行](01-transaction-execution.md) - 区块内交易执行的详细逻辑
- [第二章：存储架构](02-storage-architecture.md) - 持久化层的实现细节

---

**深入阅读建议：**

1. **newPayload 执行细节** → [`crates/engine/tree/src/tree/mod.rs`](../../crates/engine/tree/src/tree/mod.rs)
2. **区块验证规则** → [`crates/consensus/common/src/validation.rs`](../../crates/consensus/common/src/validation.rs)
3. **内存状态管理** → [`crates/engine/tree/src/tree/state.rs`](../../crates/engine/tree/src/tree/state.rs)
4. **RPC 层实现** → [`crates/rpc/rpc-engine-api/src/engine_api.rs`](../../crates/rpc/rpc-engine-api/src/engine_api.rs)
