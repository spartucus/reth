# 第8章：Payload 构建与 MEV

> **核心问题：** 当节点被选为出块者时，如何从交易池选取交易、组装出最优区块？MEV-boost 如何接入？

---

## 目录

1. [为什么需要 Payload 构建？](#1-为什么需要-payload-构建)
2. [crates/payload/ 子模块结构](#2-cratespayload-子模块结构)
3. [核心 Trait 层级](#3-核心-trait-层级)
4. [PayloadBuilderService：服务编排层](#4-payloadbuilderservice服务编排层)
5. [BasicPayloadJob：持续构建循环](#5-basicpayloadjob持续构建循环)
6. [EthereumPayloadBuilder：交易选择算法](#6-ethereumpayloadbuilder交易选择算法)
7. [Payload 生命周期：从 FCU 到 getPayload](#7-payload-生命周期从-fcu-到-getpayload)
8. [PayloadId：唯一标识符生成](#8-payloadid唯一标识符生成)
9. [EthBuiltPayload 与版本转换](#9-ethbuiltpayload-与版本转换)
10. [MEV 与外部构建器](#10-mev-与外部构建器)
11. [关键性能优化](#11-关键性能优化)
12. [一次完整的出块流程](#12-一次完整的出块流程)
13. [关键设计决策](#13-关键设计决策)

---

## 1. 为什么需要 Payload 构建？

在 PoS 以太坊中，**验证者（Validator）** 由 Beacon Chain 随机选出负责出块。出块流程：

```
Beacon Chain 选出验证者 V
    │
    ▼
V 的 CL（共识层）向 EL 发送 engine_forkchoiceUpdatedV3(fcuState, payloadAttributes)
    │
    ▼
EL 开始构建 payload（装满收益最高的交易组合）
    │
    ▼
CL 在出块时刻前发送 engine_getPayloadV3(payloadId)
    │
    ▼
EL 返回已构建的 ExecutionPayload
    │
    ▼
CL 将其封装进 BeaconBlock，签名，广播
```

因此 **payload 构建** = 在有限 gas limit 内，选取能为区块构建者带来最多 MEV+手续费 的交易组合。

从 `forkchoiceUpdated` 到 `getPayload` 通常有 **6-12 秒**（一个 slot），payload builder 利用这段时间持续迭代改进区块。

---

## 2. crates/payload/ 子模块结构

```
crates/payload/
├── builder/          # PayloadBuilderService + PayloadJob/PayloadJobGenerator trait
│   └── src/
│       ├── service.rs    # PayloadBuilderService（无限 Future 编排器）
│       └── traits.rs     # PayloadJob + PayloadJobGenerator trait 定义
├── builder-primitives/   # 事件类型和错误类型
├── primitives/           # 基础 payload 类型（PayloadId、BuiltPayload 等）
├── basic/            # BasicPayloadJob + BasicPayloadJobGenerator（通用实现）
│   └── src/lib.rs
├── util/             # 工具函数
└── validator/        # Payload 验证工具

crates/ethereum/payload/  # 以太坊特定实现
└── src/lib.rs            # EthereumPayloadBuilder（交易选择、EVM 执行）

crates/ethereum/engine-primitives/
└── src/payload.rs        # EthBuiltPayload、EthPayloadBuilderAttributes
```

### 层级关系

```
EthereumPayloadBuilder（ethereum/payload）
        ↓ 实现 PayloadBuilder trait
BasicPayloadJobGenerator（payload/basic）
        ↓ 实现 PayloadJobGenerator trait
BasicPayloadJob（payload/basic）
        ↓ 实现 PayloadJob trait（同时也是 Future）
PayloadBuilderService（payload/builder）
        ↓ 编排所有 Job
PayloadBuilderHandle（公共接口，可克隆）
```

---

## 3. 核心 Trait 层级

### 3.1 PayloadJob trait（crates/payload/builder/src/traits.rs:20）

`PayloadJob` 是最核心的抽象，它同时是一个 `Future`：

```rust
// crates/payload/builder/src/traits.rs:20

pub trait PayloadJob: Future<Output = Result<(), PayloadBuilderError>> {
    type PayloadAttributes: PayloadBuilderAttributes + Debug;
    type ResolvePayloadFuture: Future<Output = Result<Self::BuiltPayload, PayloadBuilderError>>
        + Send + 'static;
    type BuiltPayload: BuiltPayload + Clone + Debug;

    /// 返回目前为止构建的最好 payload（内部使用，CL 不会直接调用）
    fn best_payload(&self) -> Result<Self::BuiltPayload, PayloadBuilderError>;

    /// 返回 payload 的构建属性
    fn payload_attributes(&self) -> Result<Self::PayloadAttributes, PayloadBuilderError>;

    /// CL 请求 payload 时调用（engine_getPayloadVx）
    /// kind: Earliest（尽快返回）或 WaitForPending（等待在建 payload）
    /// 返回: (结果 Future, 是否保持 Job 存活)
    fn resolve_kind(
        &mut self,
        kind: PayloadKind,
    ) -> (Self::ResolvePayloadFuture, KeepPayloadJobAlive);

    fn resolve(&mut self) -> (Self::ResolvePayloadFuture, KeepPayloadJobAlive) {
        self.resolve_kind(PayloadKind::Earliest)
    }
}

pub enum KeepPayloadJobAlive {
    Yes,  // resolve 后继续 poll（少用）
    No,   // resolve 后终止 Job（BasicPayloadJob 的默认行为）
}
```

**关键设计**：Job 必须随时能返回"目前最好的 payload"（哪怕是空块），确保永远不会因为构建失败而错过 slot。

### 3.2 PayloadJobGenerator trait（traits.rs:94）

```rust
pub trait PayloadJobGenerator {
    type Job: PayloadJob;

    /// forkchoiceUpdated 携带 payloadAttributes 时调用
    /// 返回一个开始构建的新 Job
    fn new_payload_job(
        &self,
        attr: <Self::Job as PayloadJob>::PayloadAttributes,
    ) -> Result<Self::Job, PayloadBuilderError>;

    /// 新区块被规范化时调用（用于预缓存状态）
    fn on_new_state<N: NodePrimitives>(&mut self, new_state: CanonStateNotification<N>) {
        let _ = new_state;
    }
}
```

### 3.3 PayloadBuilder trait（payload/basic 使用）

```rust
pub trait PayloadBuilder: Send + Sync + Clone {
    type Attributes: PayloadBuilderAttributes;
    type BuiltPayload: BuiltPayload;

    /// 实际执行一次区块构建尝试
    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError>;

    /// 当 CL 请求 payload 但当前没有 payload 时的行为策略
    fn on_missing_payload(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload>;

    /// 构建一个空 payload（无交易，紧急兜底）
    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes>,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError>;
}

pub enum BuildOutcome<Payload> {
    Better { payload: Payload, cached_reads: CachedReads }, // 比之前更好
    Aborted { fees: U256, cached_reads: CachedReads },      // 不如之前，放弃
    Freeze(Payload),     // 标记为冻结，不再继续构建
    Cancelled,           // 构建被取消（如 Job 被提前终止）
}
```

---

## 4. PayloadBuilderService：服务编排层

### 结构定义（service.rs:203）

```rust
// crates/payload/builder/src/service.rs:203

#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct PayloadBuilderService<Gen, St, T>
where
    T: PayloadTypes,
    Gen: PayloadJobGenerator,
    Gen::Job: PayloadJob<PayloadAttributes = T::PayloadBuilderAttributes>,
{
    generator: Gen,
    payload_jobs: Vec<(Gen::Job, PayloadId)>,   // 所有活跃的构建 Job

    service_tx: mpsc::UnboundedSender<PayloadServiceCommand<T>>,
    command_rx: UnboundedReceiverStream<PayloadServiceCommand<T>>,

    chain_events: St,         // 新区块规范化事件流（用于 on_new_state）
    payload_events: broadcast::Sender<Events<T>>,  // 向外广播 payload 事件

    // 缓存最近一次已解析的 payload（支持重复请求）
    cached_payload_tx: watch::Sender<Option<(PayloadId, BlockTimestamp, T::BuiltPayload)>>,
    cached_payload_rx: watch::Receiver<Option<(PayloadId, BlockTimestamp, T::BuiltPayload)>>,

    metrics: PayloadBuilderServiceMetrics,
}
```

`PayloadBuilderService` 是一个无限循环的 `Future`，与 `NetworkManager` 类似，poll 时：

```
1. 检查 chain_events：有新区块时调用 generator.on_new_state()
      ↓
2. 轮询所有活跃 payload_jobs：
   - Ready → 从 Vec 中移除（Job 完成或超时）
   - Pending → 放回 Vec
      ↓
3. 处理所有来自 PayloadBuilderHandle 的命令（command_rx）
```

### 命令类型（PayloadServiceCommand）

```rust
pub enum PayloadServiceCommand<T: PayloadTypes> {
    /// forkchoiceUpdated 带 payloadAttributes → 启动新 Job
    BuildNewPayload(T::PayloadBuilderAttributes, oneshot::Sender<Result<PayloadId, ...>>),

    /// 查询当前最好的 payload（不触发 resolve）
    BestPayload(PayloadId, oneshot::Sender<Option<Result<T::BuiltPayload, ...>>>),

    /// CL 请求最终 payload（engine_getPayload）
    Resolve(PayloadId, PayloadKind, oneshot::Sender<Option<PayloadFuture<T::BuiltPayload>>>),

    /// 查询 payload 时间戳
    PayloadTimestamp(PayloadId, oneshot::Sender<Option<Result<u64, ...>>>),

    /// 订阅 payload 事件
    Subscribe(oneshot::Sender<broadcast::Receiver<Events<T>>>),
}
```

### 与其他组件的关系

```
Engine API（CL 请求）
    │
    ▼
EngineTree（engine/tree/src/tree/mod.rs）
    │ payload_builder.send_new_payload(attributes)
    ▼
PayloadBuilderHandle（克隆，发命令）
    │ UnboundedSender<PayloadServiceCommand>
    ▼
PayloadBuilderService（接收命令，管理 Job 生命周期）
    │ generator.new_payload_job(attributes)
    ▼
BasicPayloadJobGenerator → BasicPayloadJob
    │ 每秒 spawn 一次 PayloadBuilder::try_build()
    ▼
EthereumPayloadBuilder（实际执行 EVM，选择交易）
```

---

## 5. BasicPayloadJob：持续构建循环

### 结构（payload/basic/src/lib.rs:302）

```rust
// crates/payload/basic/src/lib.rs:302

pub struct BasicPayloadJob<Tasks, Builder: PayloadBuilder> {
    config: PayloadConfig<Builder::Attributes, ...>,
    executor: Tasks,                   // 任务执行器（spawner）
    deadline: Pin<Box<Sleep>>,         // 超时 Future（slot 结束时触发）
    interval: Interval,                // 触发重新构建的定时器（默认 1s）
    best_payload: PayloadState<Builder::BuiltPayload>,
    pending_block: Option<PendingPayload<Builder::BuiltPayload>>,
    cached_reads: Option<CachedReads>, // 跨构建复用的磁盘读缓存
    payload_task_guard: PayloadTaskGuard,  // 限制并发构建任务数量
    metrics: PayloadBuilderMetrics,
    builder: Builder,
}

pub enum PayloadState<P> {
    Missing,     // 尚未构建任何 payload
    Best(P),     // 目前最好的，可能被下次构建改进
    Frozen(P),   // 冻结，不再尝试改进
}
```

### poll 循环（Future impl，lib.rs:374）

```rust
fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    // 步骤1：检查 deadline（slot 结束）
    if this.deadline.as_mut().poll(cx).is_ready() {
        return Poll::Ready(Ok(()))   // Job 完成，Service 将其移除
    }

    // 步骤2：定时器触发则发起新一轮构建
    while this.interval.poll_tick(cx).is_ready() {
        if this.pending_block.is_none() && !this.best_payload.is_frozen() {
            this.spawn_build_job();  // 在阻塞线程池 spawn 一个构建任务
        }
    }

    // 步骤3：检查进行中的构建是否完成
    if let Some(mut fut) = this.pending_block.take() {
        match fut.poll_unpin(cx) {
            Poll::Ready(Ok(BuildOutcome::Better { payload, cached_reads })) => {
                this.cached_reads = Some(cached_reads);
                this.best_payload = PayloadState::Best(payload);
            }
            Poll::Ready(Ok(BuildOutcome::Freeze(payload))) => {
                this.best_payload = PayloadState::Frozen(payload);  // 不再改进
            }
            Poll::Ready(Ok(BuildOutcome::Aborted { cached_reads, .. })) => {
                this.cached_reads = Some(cached_reads);  // 没变好，保存缓存继续
            }
            Poll::Ready(Err(error)) => {
                // 构建失败，下次 interval 再试
                this.metrics.inc_failed_payload_builds();
            }
            Poll::Pending => this.pending_block = Some(fut),
        }
    }

    Poll::Pending  // Job 继续运行直到 deadline
}
```

### spawn_build_job（lib.rs:341）

```rust
fn spawn_build_job(&mut self) {
    let (tx, rx) = oneshot::channel();
    let cancel = CancelOnDrop::default();   // drop 时自动取消
    let guard = self.payload_task_guard.clone();  // 并发任务数限制
    let cached_reads = self.cached_reads.take().unwrap_or_default();

    self.executor.spawn_blocking(Box::pin(async move {
        let _permit = guard.acquire().await;  // 等待空闲槽位（最多3个并发）
        let args = BuildArguments { cached_reads, config, cancel, best_payload };
        let result = builder.try_build(args);  // 实际构建
        let _ = tx.send(result);
    }));

    self.pending_block = Some(PendingPayload { _cancel: cancel, payload: rx });
}
```

### resolve_kind：CL 请求 payload 时

当 CL 发来 `engine_getPayloadV3` 时，Job 的 `resolve_kind` 被调用：

```
如果已有 best_payload：直接返回它
如果有 pending_block（正在构建）：
    PayloadKind::Earliest → 同时 race（pending vs 空块），谁先完成返回谁
    PayloadKind::WaitForPending → 等待 pending_block 完成
如果两者都没有：spawn 一个空块构建，紧急返回

返回后 KeepPayloadJobAlive::No → Job 被终止
```

### 超时计算（BasicPayloadJobGenerator，lib.rs:98）

```rust
// 规范要求：getPayload 调用时或 SECONDS_PER_SLOT(12s) 后停止构建
fn max_job_duration(&self, unix_timestamp: u64) -> Duration {
    let duration_until_timestamp = duration_until(unix_timestamp);
    // 防止时钟偏差，最多 3 倍 deadline
    let duration_until_timestamp = duration_until_timestamp.min(self.config.deadline * 3);
    self.config.deadline + duration_until_timestamp
}
```

默认 deadline = 12s（SLOT_DURATION），实际 Job 存活时间约 12-36s。

---

## 6. EthereumPayloadBuilder：交易选择算法

### 结构（ethereum/payload/src/lib.rs:55）

```rust
// crates/ethereum/payload/src/lib.rs:55

pub struct EthereumPayloadBuilder<Pool, Client, EvmConfig = EthEvmConfig> {
    client: Client,          // 状态访问
    pool: Pool,              // 交易池
    evm_config: EvmConfig,   // EVM 配置
    builder_config: EthereumBuilderConfig,
}

pub struct EthereumBuilderConfig {
    desired_gas_limit: u64,         // 目标 gas limit（默认 30M）
    await_payload_on_missing: bool, // CL 请求时若无 payload 是否等待（默认 true）
    max_blobs_per_block: Option<u64>, // blob 数量上限（None = 使用协议限制）
    extra_data: Bytes,              // 区块 extra_data 字段
}
```

### default_ethereum_payload：核心构建算法（lib.rs:138）

```rust
pub fn default_ethereum_payload(...) -> Result<BuildOutcome<EthBuiltPayload>, ...>
```

**完整构建流程：**

#### 步骤 1：状态初始化（lib.rs:155-174）

```rust
// 获取父区块的状态快照
let state_provider = client.state_by_block_hash(parent_header.hash())?;
let state = StateProviderDatabase::new(state_provider.as_ref());

// 用 CachedReads 包装，避免重复读磁盘
let mut db = State::builder()
    .with_database(cached_reads.as_db_mut(state))
    .with_bundle_update()
    .build();
```

#### 步骤 2：EVM 环境初始化（lib.rs:160-174）

```rust
let mut builder = evm_config.builder_for_next_block(
    &mut db,
    &parent_header,
    NextBlockEnvAttributes {
        timestamp: attributes.timestamp(),
        suggested_fee_recipient: attributes.suggested_fee_recipient(),
        prev_randao: attributes.prev_randao(),
        gas_limit: builder_config.gas_limit(parent_header.gas_limit),  // ±0.1%
        parent_beacon_block_root: attributes.parent_beacon_block_root(),
        withdrawals: Some(attributes.withdrawals().clone()),  // EIP-4895
        extra_data: builder_config.extra_data,
    },
)?;
```

#### 步骤 3：Pre-execution（lib.rs:190-193）

```rust
// 应用 withdrawals（EIP-4895）等区块前置操作
builder.apply_pre_execution_changes()?;
```

withdrawals（提款）在所有交易执行**之前**应用：直接给账户加余额，不涉及 EVM 执行。

#### 步骤 4：获取最优交易序列（lib.rs:184-187）

```rust
let mut best_txs = best_txs(BestTransactionsAttributes::new(
    base_fee,           // 当前 baseFee（用于 EIP-1559 过滤）
    blob_gas_price,     // 当前 blob gas 价格（用于 blob tx 过滤）
));
```

`best_transactions_with_attributes` 返回 `BestTransactions` 迭代器（见第5章），按 `effective_tip_per_gas` 从高到低排列，且保证同一发送方的 nonce 严格递增。

#### 步骤 5：交易选择循环（lib.rs:217-355）

```
while let Some(pool_tx) = best_txs.next():

┌─ 检查 gas limit ──────────────────────────────────────────────────┐
│  if cumulative_gas_used + tx.gas_limit > block_gas_limit:          │
│      mark_invalid(tx)  // 跳过并移除所有依赖此 tx 的后续交易         │
│      continue                                                       │
└───────────────────────────────────────────────────────────────────┘
         ↓
┌─ 取消检查 ────────────────────────────────────────────────────────┐
│  if cancel.is_cancelled():                                         │
│      return BuildOutcome::Cancelled                                │
└───────────────────────────────────────────────────────────────────┘
         ↓
┌─ Osaka RLP 大小限制（8MB）────────────────────────────────────────┐
│  estimated_size = rlp(txs_so_far) + rlp(tx) + rlp(withdrawals)    │
│                   + 1024（区块头开销）                              │
│  if is_osaka && estimated_size > MAX_RLP_BLOCK_SIZE(8MB):          │
│      mark_invalid(tx); continue                                    │
└───────────────────────────────────────────────────────────────────┘
         ↓
┌─ Blob 数量检查（EIP-4844）────────────────────────────────────────┐
│  if block_blob_count + tx_blob_count > max_blob_count:             │
│      mark_invalid(tx); continue                                    │
│  验证 sidecar 格式（Osaka: EIP-7594, 否则: EIP-4844）              │
└───────────────────────────────────────────────────────────────────┘
         ↓
┌─ 执行交易（EVM）──────────────────────────────────────────────────┐
│  match builder.execute_transaction(tx):                            │
│    NonceTooLow → skip（可能是 reorg 期间的过时 tx）                │
│    其他错误   → mark_invalid(tx) + 移除后续依赖                    │
│    Fatal 错误 → 返回 Err                                           │
│    成功       → 继续                                               │
└───────────────────────────────────────────────────────────────────┘
         ↓
┌─ 累计手续费 ──────────────────────────────────────────────────────┐
│  miner_fee = tx.effective_tip_per_gas(base_fee)                    │
│  total_fees += U256::from(miner_fee) * U256::from(gas_used)        │
│  cumulative_gas_used += gas_used                                   │
│                                                                    │
│  if 达到 max blob count:                                           │
│      best_txs.skip_blobs()  // 跳过后续所有 blob 交易              │
└───────────────────────────────────────────────────────────────────┘
```

#### 步骤 6：比较与决策（lib.rs:357-363）

```rust
// is_better_payload：当前收费 > 之前最好的 payload 的收费
if !is_better_payload(best_payload.as_ref(), total_fees) {
    return Ok(BuildOutcome::Aborted { fees: total_fees, cached_reads })
}
```

如果这次构建的 `total_fees` 不如上次，直接返回 `Aborted`，保留之前的 payload。

#### 步骤 7：完成区块（lib.rs:365-387）

```rust
let BlockBuilderOutcome { execution_result, block, .. } =
    builder.finish(state_provider.as_ref())?;

// Prague+ 的 execution requests（EIP-7685 等）
let requests = chain_spec.is_prague_active_at_timestamp(...)
    .then_some(execution_result.requests);

// Osaka 最终 RLP 大小检查（8MB）
if is_osaka && sealed_block.rlp_length() > MAX_RLP_BLOCK_SIZE {
    return Err(PayloadBuilderError::other(ConsensusError::BlockTooLarge { ... }));
}

// 构建成功！
Ok(BuildOutcome::Better { payload: EthBuiltPayload::new(...), cached_reads })
```

### Gas Limit 计算

```rust
// crates/ethereum/payload/src/config.rs:67

// 区块 gas limit 每块只能变化 ±1/1024（约 0.1%）
pub fn calculate_block_gas_limit(parent_gas_limit: u64, desired_gas_limit: u64) -> u64 {
    let delta = (parent_gas_limit / GAS_LIMIT_BOUND_DIVISOR).saturating_sub(1);
    desired_gas_limit.clamp(parent_gas_limit - delta, parent_gas_limit + delta)
}
// GAS_LIMIT_BOUND_DIVISOR = 1024
```

节点通过配置 `desired_gas_limit` 表达"希望的 gas limit"，但实际每块最多向目标移动 1/1024，防止突然变化导致网络不稳定。

---

## 7. Payload 生命周期：从 FCU 到 getPayload

### 完整时序图

```
CL                     EL（EngineTree）         PayloadBuilderService    BasicPayloadJob
 │                          │                          │                       │
 │ engine_forkchoiceUpdatedV3                          │                       │
 │ (fcuState, payloadAttrs) │                          │                       │
 │──────────────────────────▶                          │                       │
 │                          │                          │                       │
 │                          │ send_new_payload(attrs)  │                       │
 │                          │─────────────────────────▶                       │
 │                          │                          │ new_payload_job(attrs) │
 │                          │                          │───────────────────────▶
 │                          │                          │                       │
 │ {status:VALID, payloadId}│                          │          spawn_build_job()
 │◀──────────────────────── │                          │                       │
 │                          │                          │           EVM执行第1轮  │
 │                          │                          │           ← Better →   │
 │                          │                          │                       │
 │    ～6-12秒后（出块时刻）  │                          │           每1秒重新构建 │
 │                          │                          │           → Better/Aborted
 │ engine_getPayloadV3      │                          │                       │
 │ (payloadId)              │                          │                       │
 │──────────────────────────▶                          │                       │
 │                          │ payload_store.resolve(id)│                       │
 │                          │─────────────────────────▶                       │
 │                          │                          │ job.resolve_kind()    │
 │                          │                          │───────────────────────▶
 │                          │                          │   返回最好的 payload   │
 │                          │                          │◀──────────────────────
 │                          │ ExecutionPayloadEnvelopeV3│                      │
 │◀──────────────────────── │                          │                       │
 │                          │                          │                       │
 │ engine_newPayloadV3      │                          │                       │
 │ (payload)                │                          │  Job 终止             │
 │──────────────────────────▶                          │                       │
```

### PayloadKind 的影响

```rust
pub enum PayloadKind {
    /// 尽快返回（用于出块时刻）
    /// 策略：race(best_payload, empty_payload)，谁先完成返回谁
    Earliest,

    /// 等待当前构建任务完成再返回（用于预取优化）
    WaitForPending,
}
```

---

## 8. PayloadId：唯一标识符生成

`PayloadId` 是 8 字节标识符，由 `SHA256(payloadAttributes)` 的前 8 字节生成：

```rust
// crates/ethereum/engine-primitives/src/payload.rs

// 输入：parent_hash + timestamp + prev_randao + suggested_fee_recipient
//       + [withdrawals_root] + [parent_beacon_block_root]
fn payload_id(parent: &B256, attributes: &PayloadAttributes) -> PayloadId {
    use sha2::Sha256;
    let mut hasher = Sha256::new();
    hasher.update(parent.as_slice());
    hasher.update(&attributes.timestamp.to_be_bytes()[..]);
    hasher.update(attributes.prev_randao.as_slice());
    hasher.update(attributes.suggested_fee_recipient.as_slice());
    // ... 加其他字段
    let result = hasher.finalize();
    PayloadId::new(result[..8].try_into().unwrap())
}
```

相同的 `payloadAttributes` 每次生成相同的 `PayloadId`，使得 CL 可以幂等地请求同一个 payload（例如重启节点后）。

---

## 9. EthBuiltPayload 与版本转换

### EthBuiltPayload 结构（ethereum/engine-primitives/src/payload.rs:32）

```rust
pub struct EthBuiltPayload<N: NodePrimitives = EthPrimitives> {
    pub(crate) id: PayloadId,
    pub(crate) block: Arc<SealedBlock<N::Block>>,  // 已封装的区块（含 hash）
    pub(crate) fees: U256,                          // 总手续费（wei）
    pub(crate) sidecars: BlobSidecars,              // blob 侧链数据
    pub(crate) requests: Option<Requests>,           // Prague+ 执行请求
}

pub enum BlobSidecars {
    Empty,
    Eip4844(Vec<BlobTransactionSidecar>),   // Cancun~Osaka 前
    Eip7594(Vec<BlobTransactionSidecarEip7594>),  // Osaka+（EIP-7594）
}
```

### Engine API 版本映射

| 版本 | 新增字段 | 对应分叉 |
|------|---------|---------|
| **V1** | ExecutionPayloadV1（基础） | Paris（合并） |
| **V2** | + `block_value`（手续费） | Shanghai |
| **V3** | + `blobs_bundle`（EIP-4844 blob 数据） + `should_override_builder` | Cancun |
| **V4** | + `execution_requests`（Prague EIP-7685） | Prague |
| **V5** | blob 格式改为 EIP-7594 | Osaka |

`should_override_builder` 字段告知 CL："本 payload 是否比 MEV-boost 外部构建器的 payload 更好"。目前 reth 默认返回 `false`（由 CL 自行决定）。

---

## 10. MEV 与外部构建器

### reth 的原生 MEV 支持

reth 提供了两个 `mev_` 命名空间 RPC 方法（`crates/rpc/rpc-api/src/mev.rs`）：

```rust
// mev_sendBundle：向 relay 提交 bundle
async fn send_bundle(request: MevSendBundle) -> RpcResult<EthBundleHash>;

// mev_simBundle：模拟 bundle 执行（不提交）
async fn sim_bundle(bundle: MevSendBundle, overrides: SimBundleOverrides)
    -> RpcResult<SimBundleResponse>;
```

这两个接口主要面向 **Flashbots-compatible relay 集成**，让 MEV searcher 可以向本节点提交 bundle。

### MEV-Boost 架构（外部集成）

MEV-Boost 是以太坊社区的标准 PBS（Proposer-Builder Separation）方案：

```
                     ┌──────────────────────────────────────────┐
                     │              MEV-Boost 架构               │
                     └──────────────────────────────────────────┘

验证者 CL（Lighthouse/Prysm）
    │
    │ 向 MEV-Boost 而非 EL 请求 getPayload
    ▼
┌───────────────┐
│   MEV-Boost   │  ← 聚合多个 relay 的报价，选最高价的 payload
│  (中间件)      │
└───────────────┘
    │                          │
    │ 向 relay 请求报价          │ 本地 EL（reth）也构建 payload 作为兜底
    ▼                          │ （若 relay 不可用或报价更低，使用本地构建）
┌───────────────┐               │
│    Relay      │               │
│（Flashbots等） │               │
└───────────────┘               │
    │
    │ 外部 Builder 已构建好 payload
    ▼
ExecutionPayloadHeader（只有头，不含交易列表）
    │
    │ 验证者在 slot 到来时：
    │  1. 比较 MEV-Boost 报价 vs 本地 payload 的 fees
    │  2. 选更高的那个
    │  3. 对选中的 payload header 签名
    ▼
Beacon Chain 广播 SignedBeaconBlock

（交易列表在后续 reveal 阶段公开）
```

### PBS（提议者-构建者分离）的核心思想

**问题**：验证者（Proposer）理论上可以自己选交易、操纵 MEV，但这需要复杂的搜索能力。

**方案**：让专业的 **Builder**（外部构建器）竞争构建区块，Proposer 只负责"签名最高出价的区块"。这样：
- Proposer 不需要 MEV 搜索能力
- Builder 竞争使 MEV 收入最大化
- 通过 relay 的可信承诺保证"看到 header 后才能签名，签名后才能看到内容"

### reth 节点在 MEV-Boost 中的角色

1. **作为本地构建器**：用 `default_ethereum_payload` 算法构建本地 payload，作为兜底
2. **作为 Block Validator**：收到获胜的外部 payload 后，通过 `engine_newPayloadV3` 验证并执行
3. **不包含**：relay 集成、bundle 排序优化、searcher 接口等（这些由第三方工具实现）

---

## 11. 关键性能优化

### CachedReads：跨构建共享磁盘缓存

```
第1次构建：
    State 层读取 AccountA、AccountB、StorageX...（磁盘 IO）
    → 缓存到 CachedReads

第2次构建（1秒后）：
    新交易加入，但大多数 state 相同
    → 从 CachedReads 直接读取（内存 IO，快 10-100x）
    → 只有新触及的 state 才去磁盘
```

`BuildOutcome::Better` 和 `BuildOutcome::Aborted` 都会归还 `cached_reads`，确保缓存跨构建持续复用。

### PrecachedState：利用上一个区块的状态

```rust
// BasicPayloadJobGenerator::on_new_state（lib.rs:188）

// 新区块规范化时，预先缓存其 state
// 下个 slot 构建时，若父区块匹配，直接使用这份缓存
fn on_new_state<N: NodePrimitives>(&mut self, new_state: CanonStateNotification<N>) {
    // 提取新区块触及的账户、存储
    // 存入 self.pre_cached
}
```

这样新 slot 的第一次构建也能从缓存开始，而不是完全冷启动。

### PayloadTaskGuard：限制并发构建任务

```rust
pub struct BasicPayloadJobGeneratorConfig {
    interval: Duration,       // 每次尝试构建的间隔（默认 1s）
    deadline: Duration,       // Job 超时（默认 12s）
    max_payload_tasks: usize, // 最大并发构建任务数（默认 3）
}
```

同时最多 3 个 payload 的 EVM 执行在后台运行，防止因多 slot 同时构建（如 MEV 竞争）耗尽 CPU。

### CancelOnDrop：安全取消

`spawn_build_job` 使用 `CancelOnDrop` 标记。如果 Job 被提前终止（如 CL 已获取 payload），后台的 EVM 执行任务会在下次检查 `cancel.is_cancelled()` 时自动退出，不会浪费 CPU。

---

## 12. 一次完整的出块流程

以主网 Slot N 为例：

```
时间 T-0:  Beacon Chain 选出验证者 V，Slot N 开始

时间 T+0s: V 的 CL 发送 engine_forkchoiceUpdatedV3:
           { headBlockHash: ..., finalizedBlockHash: ...,
             payloadAttributes: { timestamp: T+12, feeRecipient: V_addr, ... } }

时间 T+0s: EngineTree 收到 FCU + payloadAttributes
           → 生成 PayloadId = SHA256(parent || timestamp || ...)[:8]
           → payload_builder.send_new_payload(attrs)
           → 返回 { status: VALID, payloadId: "0x1234..." }

时间 T+0s: BasicPayloadJobGenerator.new_payload_job():
           → 获取父区块头
           → 加载 PrecachedState（若匹配）
           → 创建 BasicPayloadJob，立即 spawn_build_job()

时间 T+0s: EthereumPayloadBuilder.try_build()（后台阻塞任务）:
           → 初始化 EVM（parent state + withdrawals）
           → 循环选交易：按 tip 从高到低，逐笔 EVM 执行
           → 得到第1版 payload（假设 50 ETH 手续费）
           → BuildOutcome::Better → PayloadState::Best(payload_v1)

时间 T+1s: BasicPayloadJob interval 触发，spawn_build_job()
           → 新进入池的高手续费交易被选入
           → total_fees = 52 ETH → BuildOutcome::Better
           → PayloadState::Best(payload_v2)

时间 T+2s~T+10s: 每秒重复上述过程，payload 不断改进

时间 T+11s: CL 发送 engine_getPayloadV3(payloadId = "0x1234...")
            → PayloadBuilderService 调用 job.resolve_kind(Earliest)
            → 返回 PayloadState::Best(payload_最终版)
            → CL 获得 ExecutionPayloadEnvelopeV3:
              { executionPayload, blockValue: 55 ETH, blobsBundle, ... }

时间 T+11s: BasicPayloadJob 终止（KeepPayloadJobAlive::No）

时间 T+12s: V 的 CL 将 payload 封装进 BeaconBlock，签名，广播
            → Beacon Chain 全网确认 Slot N

时间 T+12s: 其他节点的 EL 收到 engine_newPayloadV3(payload)
            → 验证执行 → engine_forkchoiceUpdatedV3 → 状态更新
```

---

## 13. 关键设计决策

### 为什么持续构建而不是构建一次？

1. **新交易不断进入 mempool**：1 秒后 mempool 中可能出现手续费更高的交易
2. **CL 可能提前请求**：某些 CL 实现在 slot 结束前就调用 getPayload，需要已有结果
3. **失败重试**：某次构建可能遇到短暂错误，需要下次重试

代价：多次 EVM 执行的 CPU 开销。这通过 `PayloadTaskGuard`（最多 3 个并发）和 `CachedReads`（减少重复 IO）来控制。

### 为什么用 `total_fees` 而不是 `gas_used` 作为优化目标？

**gas_used** 最大化会导致选很多 gas 消耗大但手续费低的交易（如批量 token 转账）。

**total_fees = Σ (effective_tip × gas_used)** 更准确地代表构建者实际获得的收益（MEV + tip）。这与 CL 的 `block_value` 对应，也是 MEV-Boost relay 竞标的基准。

### 为什么 `resolve_kind(Earliest)` 会 race 空块？

规范要求：`getPayload` 必须在 1 秒内返回（CL 的超时）。如果构建任务正在进行且短时间内不会完成，与其等待超时，不如立即返回一个空块（0 手续费但合法的区块），确保验证者不会错过 slot。

### build_empty_payload 的用途

当没有任何构建完成时的最后兜底：

```rust
fn build_empty_payload(&self, config: PayloadConfig<Self::Attributes>)
    -> Result<EthBuiltPayload, PayloadBuilderError>
```

空块包含：
- 正确的区块头（timestamp、feeRecipient、parent_hash 等）
- 正确的 withdrawals（提款必须处理）
- 空交易列表
- 零手续费

对于验证者来说，宁可出一个空块也不愿错过 slot（错过 slot 会受惩罚）。

---

## 本章总结

| 组件 | 职责 | 关键文件 |
|------|------|---------|
| `PayloadBuilderService` | 编排 Job 生命周期，响应 FCU/getPayload | `payload/builder/src/service.rs` |
| `BasicPayloadJobGenerator` | 创建 Job，管理状态预缓存 | `payload/basic/src/lib.rs:50` |
| `BasicPayloadJob` | 持续构建循环（1s interval，12s deadline） | `payload/basic/src/lib.rs:302` |
| `EthereumPayloadBuilder` | 实际 EVM 执行和交易选择 | `ethereum/payload/src/lib.rs:55` |
| `EthBuiltPayload` | 构建结果，支持 V1-V5 版本转换 | `ethereum/engine-primitives/src/payload.rs` |
| `CachedReads` | 跨构建共享磁盘读缓存 | `payload/basic/` |
| MEV-Boost | 外部构建器接入（标准 Engine API） | 第三方工具，非 reth 内置 |

**关键代码路径：**
```
crates/payload/
├── builder/src/
│   ├── service.rs            # PayloadBuilderService（无限 Future）
│   └── traits.rs             # PayloadJob + PayloadJobGenerator trait
└── basic/src/lib.rs          # BasicPayloadJob + BasicPayloadJobGenerator

crates/ethereum/payload/src/lib.rs    # EthereumPayloadBuilder
crates/ethereum/engine-primitives/src/payload.rs  # EthBuiltPayload + PayloadId

crates/rpc/rpc-api/src/mev.rs         # mev_sendBundle / mev_simBundle RPC
```

---

**下一章：** [第9章：并发与异步](09-concurrency-and-async.md)

reth 如何利用 Tokio + Rayon 的分工实现高性能并发？`spawn_blocking` 的使用时机，以及各种并发模式在 reth 中的体现。
