# 第5章：交易池（Transaction Pool）

## 概述

交易池（mempool / transaction pool）是以太坊节点的核心组件之一，负责在交易被打包进区块前暂存并管理它们。reth 的交易池实现在 `crates/transaction-pool/` 中，设计目标是：

1. **正确性**：只向区块构建器提供当前状态下可执行的交易
2. **高效性**：快速的插入、查询和优先级排序
3. **公平性**：按矿工小费（coinbase tip）排序，高费用优先
4. **内存安全**：通过子池分类和容量限制防止内存耗尽

交易来源有三种路径：
- **Local**：用户通过 `eth_sendRawTransaction` RPC 提交
- **External**：从 P2P 网络对等节点接收
- **Private**：节点内部插入，不向外传播

---

## 架构总览

交易池由三层结构组成：

```
Pool<V, T, S>             ← 公共 API（Arc 包装，可克隆共享）
  └── PoolInner<V, T, S>  ← 核心逻辑（RwLock 保护）
        └── TxPool<T>     ← 内部交易管理（四个子池）
              ├── PendingPool<T>           ← 可立即执行
              ├── ParkedPool<BasefeeOrd>   ← BaseFee 子池
              ├── ParkedPool<QueuedOrd>    ← Queued 子池
              └── BlobTransactions<T>      ← Blob 子池（非 pending）
```

### Pool（公共 API）

`Pool<V, T, S>` 是对外暴露的接口，其中：
- `V: TransactionValidator` — 负责验证交易
- `T: TransactionOrdering` — 决定交易排序优先级
- `S: BlobStore` — 存储 EIP-4844 blob 附件

`Pool` 本身只是一个 `Arc<PoolInner<V,T,S>>` 的薄包装，支持克隆以在多处共享同一池实例。

### PoolInner（核心逻辑）

文件：[`crates/transaction-pool/src/pool/mod.rs:140`](crates/transaction-pool/src/pool/mod.rs#L140)

```rust
pub struct PoolInner<V, T, S>
where
    T: TransactionOrdering,
{
    /// 发送方地址 ↔ 内部数字 ID 的映射表
    identifiers: RwLock<SenderIdentifiers>,
    /// 交易验证器
    validator: V,
    /// EIP-4844 blob 附件存储
    blob_store: S,
    /// 实际的交易池（四个子池）
    pool: RwLock<TxPool<T>>,
    /// 池配置（容量上限等）
    config: PoolConfig,
    /// 事件监听器（广播状态变化）
    event_listener: RwLock<PoolEventBroadcast<T::Transaction>>,
    /// pending 交易哈希监听器
    pending_transaction_listener: RwLock<Vec<PendingTransactionHashListener>>,
    /// 新交易监听器
    transaction_listener: RwLock<Vec<TransactionListener<T::Transaction>>>,
    /// blob 附件监听器
    blob_transaction_sidecar_listener: Mutex<Vec<BlobTransactionSidecarListener>>,
    // ...
}
```

`PoolInner` 持有读写锁保护的 `TxPool<T>`。所有修改操作（插入交易、新区块处理）都需要获取写锁；读操作（查询、遍历）使用读锁。

### TxPool（四个子池）

文件：[`crates/transaction-pool/src/pool/txpool.rs:89`](crates/transaction-pool/src/pool/txpool.rs#L89)

```rust
pub struct TxPool<T: TransactionOrdering> {
    /// pending 子池：可立即打包的交易
    pending_pool: PendingPool<T>,
    /// queued 子池：有 nonce 空缺或余额不足的交易
    queued_pool: ParkedPool<QueuedOrd<T::Transaction>>,
    /// basefee 子池：maxFeePerGas < 当前 baseFee 的交易
    basefee_pool: ParkedPool<BasefeeOrd<T::Transaction>>,
    /// blob 子池：不满足 pending 条件的 EIP-4844 交易
    blob_pool: BlobTransactions<T::Transaction>,
    /// 所有交易的统一索引（用于去重、查找）
    all_transactions: AllTransactions<T::Transaction>,
    // ...
}
```

`all_transactions` 是所有子池交易的统一索引，确保同一笔交易不会同时存在于多个子池。

---

## 交易标识系统

文件：[`crates/transaction-pool/src/identifier.rs`](crates/transaction-pool/src/identifier.rs)

交易池内部使用紧凑的数字 ID 替代 20 字节的 `Address`，以节省内存并加速排序比较。

### SenderId

```rust
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SenderId(u64);
```

`SenderIdentifiers` 维护 `Address ↔ SenderId` 的双向映射，首次见到某地址时自动分配递增的数字 ID：

```rust
pub struct SenderIdentifiers {
    id: u64,                            // 下一个待分配的 ID
    address_to_id: AddressMap<SenderId>,
    sender_to_address: FxHashMap<SenderId, Address>,
}

pub fn sender_id_or_create(&mut self, addr: Address) -> SenderId {
    self.sender_id(&addr).unwrap_or_else(|| {
        let id = self.next_id();  // 自增 u64
        self.address_to_id.insert(addr, id);
        self.sender_to_address.insert(id, addr);
        id
    })
}
```

### TransactionId

```rust
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct TransactionId {
    pub sender: SenderId,
    pub nonce: u64,
}
```

`TransactionId` 是 `(发送方, nonce)` 的二元组，全局唯一地标识一笔交易。`Ord` 实现先按 `sender` 再按 `nonce` 排序，使得同一发送方的交易在 `BTreeMap` 中相邻排列，便于快速遍历依赖链。

祖先/后代关系：
```rust
// 当 tx.nonce > on_chain_nonce 时，tx 依赖 nonce-1 的前驱交易
pub fn ancestor(transaction_nonce: u64, on_chain_nonce: u64, sender: SenderId) -> Option<Self> {
    (transaction_nonce > on_chain_nonce)
        .then(|| Self::new(sender, transaction_nonce - 1))
}

pub const fn descendant(&self) -> Self {
    Self::new(self.sender, self.nonce + 1)
}
```

---

## TxState 与子池路由

### TxState 标志位

文件：[`crates/transaction-pool/src/pool/state.rs:16`](crates/transaction-pool/src/pool/state.rs#L16)

每笔交易都有一个 8 位的 `TxState`，描述其当前状态，决定它属于哪个子池：

```rust
bitflags::bitflags! {
    pub(crate) struct TxState: u8 {
        /// 所有前驱交易都在 pending 池中（无 parked 祖先）
        const NO_PARKED_ANCESTORS     = 0b10000000;
        /// 无 nonce 空缺（与链上 nonce 连续，或池中已有所有前驱）
        const NO_NONCE_GAPS           = 0b01000000;
        /// 发送方余额足以覆盖本交易及所有前驱交易的最大费用
        const ENOUGH_BALANCE          = 0b00100000;
        /// gasLimit < 区块 gasLimit
        const NOT_TOO_MUCH_GAS        = 0b00010000;
        /// maxFeePerGas >= pending 区块的 baseFee
        const ENOUGH_FEE_CAP_BLOCK    = 0b00001000;
        /// maxBlobFeePerGas >= pending 区块的 blobFee（仅 EIP-4844）
        const ENOUGH_BLOB_FEE_CAP_BLOCK = 0b00000100;
        /// 该交易是 EIP-4844 blob 交易
        const BLOB_TRANSACTION        = 0b00000010;

        // 组合标志
        const PENDING_POOL_BITS = Self::NO_PARKED_ANCESTORS.bits()
            | Self::NO_NONCE_GAPS.bits()
            | Self::ENOUGH_BALANCE.bits()
            | Self::NOT_TOO_MUCH_GAS.bits()
            | Self::ENOUGH_FEE_CAP_BLOCK.bits()
            | Self::ENOUGH_BLOB_FEE_CAP_BLOCK.bits();

        const BASE_FEE_POOL_BITS = Self::NO_PARKED_ANCESTORS.bits()
            | Self::NO_NONCE_GAPS.bits()
            | Self::ENOUGH_BALANCE.bits()
            | Self::NOT_TOO_MUCH_GAS.bits();
    }
}
```

### SubPool 枚举

```rust
#[repr(u8)]
pub enum SubPool {
    Queued  = 0,   // 最低优先级
    BaseFee = 1,
    Blob    = 2,
    Pending = 3,   // 最高优先级
}
```

数值越大代表越"接近可执行"。`SubPool::is_promoted(other)` 判断 `self > other`（数值比较）。

### 路由逻辑

文件：[`crates/transaction-pool/src/pool/state.rs:192`](crates/transaction-pool/src/pool/state.rs#L192)

```rust
impl From<TxState> for SubPool {
    fn from(value: TxState) -> Self {
        if value.is_pending() {
            // 所有 PENDING_POOL_BITS 均已置位
            Self::Pending
        } else if value.is_blob() {
            // 非 pending 的 blob 交易 → Blob 子池
            // (欠缺 basefee 或 blobfee 或其他结构性条件)
            Self::Blob
        } else if value.bits() < TxState::BASE_FEE_POOL_BITS.bits() {
            // 缺少结构性条件（nonce 空缺、余额不足等）
            Self::Queued
        } else {
            // 结构上满足条件但费用不够
            Self::BaseFee
        }
    }
}
```

路由决策可总结为：

| 条件 | 子池 |
|------|------|
| 所有 6 个 pending bits 全部置位 | **Pending** |
| BLOB_TRANSACTION 置位，但非 pending | **Blob** |
| NO_PARKED_ANCESTORS \| NO_NONCE_GAPS \| ENOUGH_BALANCE \| NOT_TOO_MUCH_GAS 全部置位 | **BaseFee**（缺费用条件）|
| 上述任意条件未满足 | **Queued** |

> **关键**：pending 的 blob 交易（所有 6 个 pending bits 置位 + BLOB_TRANSACTION）也会路由到 `Pending` 子池，而不是 `Blob` 子池。只有**非 pending** 的 blob 交易才进入 `Blob` 子池。

---

## 四个子池详解

### PendingPool（待打包池）

文件：[`crates/transaction-pool/src/pool/pending.rs:31`](crates/transaction-pool/src/pool/pending.rs#L31)

```rust
pub struct PendingPool<T: TransactionOrdering> {
    ordering: T,
    submission_id: u64,                   // 单调递增的插入序号（用于同优先级时的 FIFO 排序）
    by_id: BTreeMap<TransactionId, PendingTransaction<T>>,  // 全部交易
    independent_transactions: FxHashMap<SenderId, PendingTransaction<T>>,  // 每个发送方的最低 nonce 交易
    highest_nonces: FxHashMap<SenderId, PendingTransaction<T>>,            // 每个发送方的最高 nonce 交易
    size_of: SizeTracker,
    new_transaction_notifier: broadcast::Sender<PendingTransaction<T>>,
}
```

**核心不变量**：
- 所有交易均无 nonce 空缺（gapless nonces per sender）
- `independent_transactions` 中每个发送方只保留 nonce 最低的一笔交易
- 这些 independent 交易可以无依赖地立即执行

**独立交易**（independent）是指 nonce 恰好等于链上当前 nonce 的交易——它不依赖池中其他任何交易就可以直接执行。执行完 nonce=N 的交易后，nonce=N+1 的交易（若存在）才成为新的 independent。

### ParkedPool（停放池）

文件：[`crates/transaction-pool/src/pool/parked.rs`](crates/transaction-pool/src/pool/parked.rs)

`ParkedPool<T>` 是泛型停放池，有两种实例化：

- **`ParkedPool<BasefeeOrd>`**（BaseFee 子池）：排序按 `maxFeePerGas` 从低到高，淘汰时优先丢弃费用最低的
- **`ParkedPool<QueuedOrd>`**（Queued 子池）：排序按插入时间，淘汰时优先丢弃最老的

这两种排序策略反映了不同的淘汰逻辑：BaseFee 池里的交易费用太低是暂时性的（等 baseFee 下降），而 Queued 池里的交易则在等待前驱交易到来。

### BlobTransactions（Blob 子池）

文件：[`crates/transaction-pool/src/pool/blob.rs`](crates/transaction-pool/src/pool/blob.rs)

`BlobTransactions<T>` 专门存储非 pending 的 EIP-4844 blob 交易。blob 交易比普通交易有更严格的执行条件——不仅要满足 `maxFeePerGas >= baseFee`，还要满足 `maxBlobFeePerGas >= blobBaseFee`。

**重要**：blob 交易的 sidecar（KZG 证明和 blob 数据本身）**不**存储在 `ValidPoolTransaction` 中，而是独立存储在 `BlobStore` 里，通过交易哈希索引。这避免了 sidecar 在内存中的重复存储。

---

## 交易优先级排序

文件：[`crates/transaction-pool/src/ordering.rs`](crates/transaction-pool/src/ordering.rs)

### TransactionOrdering trait

```rust
pub trait TransactionOrdering: Debug + Send + Sync + 'static {
    /// 优先级值类型（越大优先级越高）
    type PriorityValue: Ord + Clone + Default + Debug + Send + Sync;
    type Transaction: PoolTransaction;

    /// 计算交易在当前 baseFee 下的优先级
    fn priority(
        &self,
        transaction: &Self::Transaction,
        base_fee: u64,
    ) -> Priority<Self::PriorityValue>;
}
```

### CoinbaseTipOrdering（默认实现）

```rust
pub struct CoinbaseTipOrdering<T>(PhantomData<T>);

impl<T: PoolTransaction + 'static> TransactionOrdering for CoinbaseTipOrdering<T> {
    type PriorityValue = u128;

    fn priority(&self, transaction: &T, base_fee: u64) -> Priority<u128> {
        transaction.effective_tip_per_gas(base_fee).into()
    }
}
```

`effective_tip_per_gas` 实现了 EIP-1559 的有效小费计算：

```
effective_tip = min(maxFeePerGas - baseFee, maxPriorityFeePerGas)
```

对于 Legacy 交易（没有 maxPriorityFeePerGas），计算结果即 `gasPrice - baseFee`。

`Priority<T>` 枚举包含 `Value(T)` 和 `None` 两个变体，`None` 总是小于任何 `Value`，确保无法计算优先级的交易排在最后。

---

## BestTransactions 迭代器

文件：[`crates/transaction-pool/src/pool/best.rs:85`](crates/transaction-pool/src/pool/best.rs#L85)

`BestTransactions` 是区块构建的核心接口，按优先级顺序产出可执行的交易，同时维护 nonce 依赖不变量。

```rust
pub struct BestTransactions<T: TransactionOrdering> {
    /// 迭代器创建时 PendingPool 的快照副本
    all: BTreeMap<TransactionId, PendingTransaction<T>>,
    /// 当前可立即产出的候选集（independent 集合的副本）
    independent: BTreeSet<PendingTransaction<T>>,
    /// 已标记为无效的发送方集合
    invalid: HashSet<SenderId>,
    /// 接收迭代器创建后新加入的 pending 交易
    new_transaction_receiver: Option<Receiver<PendingTransaction<T>>>,
    /// 最近产出交易的优先级（用于对新来交易的位置判断）
    last_priority: Option<Priority<T::PriorityValue>>,
    skip_blobs: bool,
}
```

### 迭代逻辑

1. 从 `independent` 集合中取出优先级最高的交易（按 `Priority<u128>` 降序，同优先级按 `submission_id` 升序）
2. 产出该交易
3. 检查该交易的后继（nonce + 1）是否在 `all` 中存在——若存在，将其加入 `independent`
4. 如此循环，始终保证产出顺序满足：**同一发送方的交易 nonce 严格递增**

```
independent: [A:nonce=5(tip=10), B:nonce=2(tip=8), C:nonce=0(tip=6)]

yield A:nonce=5(tip=10)
  → 检查 A:nonce=6 是否在 all 中？是 → 加入 independent

yield A:nonce=6(tip=9)
  → 检查 A:nonce=7 是否在 all 中？否 → independent 不变

yield B:nonce=2(tip=8)
  → 检查 B:nonce=3 → ...
```

### 失效传播

当区块构建器发现某笔交易执行失败（如余额不足），可调用 `mark_invalid(tx)`：

```rust
pub(crate) fn mark_invalid(&mut self, tx: &Arc<ValidPoolTransaction<T::Transaction>>, _: &_) {
    // 将整个发送方标记为无效
    self.invalid.insert(tx.sender_id());
}
```

一旦发送方被标记，其所有后继交易都会被跳过——因为 nonce 链已断裂，后续任何交易都无法执行。

---

## 验证流程

文件：[`crates/transaction-pool/src/validate/mod.rs:28`](crates/transaction-pool/src/validate/mod.rs#L28)

### TransactionValidationOutcome

验证结果有三种：

```rust
pub enum TransactionValidationOutcome<T: PoolTransaction> {
    /// 交易当前有效，可以入池
    Valid {
        balance: U256,            // 当前发送方余额
        state_nonce: u64,         // 当前链上 nonce
        bytecode_hash: Option<B256>,
        transaction: ValidTransaction<T>,  // 交易本体（可能附带 blob sidecar）
        propagate: bool,          // 是否允许 P2P 传播
        authorities: Option<Vec<Address>>,  // EIP-7702 授权地址
    },
    /// 交易永久无效（违反不可变约束）
    Invalid(T, InvalidPoolTransactionError),
    /// 验证过程中发生错误（数据库故障等）
    Error(TxHash, Box<dyn core::error::Error + Send + Sync>),
}
```

### ValidTransaction

```rust
pub enum ValidTransaction<T> {
    /// 普通有效交易（或 blob 交易的 sidecar 已在 BlobStore 中）
    Valid(T),
    /// 有效的 EIP-4844 交易，附带需要入库的 sidecar
    ValidWithSidecar {
        transaction: T,
        sidecar: BlobTransactionSidecarVariant,
    },
}
```

### EthTransactionValidator 检查项

文件：[`crates/transaction-pool/src/validate/eth.rs`](crates/transaction-pool/src/validate/eth.rs)

`EthTransactionValidator` 执行两类检查：

**无状态检查（stateless）**：
- 交易类型是否在当前 fork 已激活（Legacy 始终支持；EIP-2930/1559 需要 Berlin/London；EIP-4844 需要 Cancun；EIP-7702 需要 Prague）
- 交易大小 ≤ 128 KB（`DEFAULT_MAX_TX_INPUT_BYTES`）
- gasLimit ≤ 区块 gasLimit
- maxPriorityFeePerGas ≤ maxFeePerGas
- ChainId 匹配
- EIP-4844：blob 数量合法，KZG 证明有效

**有状态检查（stateful）**，需要访问状态数据库：
- 发送方账户存在，余额足够支付 `maxFeePerGas * gasLimit + value`
- 交易 nonce ≥ 链上 nonce（过期交易直接拒绝）
- 交易 nonce 不超过链上 nonce + 最大队列深度

---

## 交易入池流程

### 总体流程

文件：[`crates/transaction-pool/src/pool/mod.rs:634`](crates/transaction-pool/src/pool/mod.rs#L634)

```
add_transactions(origin, txs)
  │
  ├─1. 批量验证（异步，调用 TransactionValidator）
  │
  ├─2. 获取 TxPool 写锁
  │
  ├─3. 对每笔有效交易：
  │    a. get_sender_id(address) → SenderId（懒创建）
  │    b. TransactionId::new(sender_id, tx.nonce())
  │    c. 拆分 blob sidecar（若 ValidWithSidecar）
  │    d. 构建 ValidPoolTransaction { tx, tx_id, origin, timestamp, propagate }
  │    e. TxPool::add_transaction(tx, balance, nonce) → 计算 TxState → 路由到子池
  │
  ├─4. TxPool::discard_worst() — 超出容量时淘汰最差交易
  │
  ├─5. 释放写锁
  │
  ├─6. on_added_transaction(meta) — 将 blob sidecar 存入 BlobStore
  │
  └─7. 通知监听器（pending listeners, event listeners）
```

### TxPool::add_transaction 详解

文件：[`crates/transaction-pool/src/pool/txpool.rs:743`](crates/transaction-pool/src/pool/txpool.rs#L743)

```rust
pub(crate) fn add_transaction(
    &mut self,
    tx: ValidPoolTransaction<T::Transaction>,
    on_chain_balance: U256,
    on_chain_nonce: u64,
    on_chain_code_hash: Option<B256>,
) -> PoolResult<AddedTransaction<T::Transaction>> {
    // 1. 重复检查
    if self.contains(tx.hash()) {
        return Err(PoolError::new(*tx.hash(), PoolErrorKind::AlreadyImported))
    }

    // 2. 更新发送方的余额/nonce 信息
    self.all_transactions.sender_info
        .entry(tx.sender_id())
        .or_default()
        .update(on_chain_nonce, on_chain_balance);

    // 3. 插入到 all_transactions，计算 TxState，确定目标子池
    match self.all_transactions.insert_tx(tx, on_chain_balance, on_chain_nonce) {
        Ok(InsertOk { transaction, move_to, replaced_tx, updates, state }) => {
            // 4. 将交易加入对应子池，移除被替换的交易
            self.add_new_transaction(transaction.clone(), replaced_tx.clone(), move_to);
            // 5. 处理因新交易导致的其他交易状态变更（如解锁后继交易）
            let UpdateOutcome { promoted, discarded } = self.process_updates(updates);

            // 6. 返回结果（Pending 或 Parked）
            if move_to.is_pending() {
                Ok(AddedTransaction::Pending(AddedPendingTransaction { transaction, promoted, discarded, replaced }))
            } else {
                Ok(AddedTransaction::Parked { transaction, subpool: move_to, replaced, queued_reason })
            }
        }
        Err(err) => Err(/* 未定价/超出发送方限额/gas 超限等 */)
    }
}
```

**insert_tx 后的状态传播**：当一笔新交易入池后，可能解除对后继交易的阻塞。例如，nonce=5 的交易入池后，若池中已有 nonce=6，则 nonce=6 的 `NO_NONCE_GAPS` 标志可以置位，可能从 Queued 晋升到 BaseFee 甚至 Pending。这些级联变更通过 `updates` 列表收集，并由 `process_updates` 统一执行子池间的搬移。

---

## 新区块处理（on_canonical_state_change）

每当有新区块被规范化（链头推进），交易池必须更新状态：移除已打包的交易、更新账户余额/nonce、响应 baseFee 变化。

文件：[`crates/transaction-pool/src/pool/txpool.rs:652`](crates/transaction-pool/src/pool/txpool.rs#L652)

```rust
pub(crate) fn on_canonical_state_change(
    &mut self,
    block_info: BlockInfo,         // 新区块信息（hash, baseFee, blobFee）
    mined_transactions: Vec<TxHash>,        // 被打包进新区块的交易
    changed_senders: FxHashMap<SenderId, SenderInfo>,  // 账户状态变更
    _update_kind: PoolUpdateKind,
) -> OnNewCanonicalStateOutcome<T::Transaction> {
    // 步骤1：从所有子池中移除已挖出的交易
    for tx_hash in &mined_transactions {
        self.prune_transaction_by_hash(tx_hash);
    }

    // 步骤2：更新内部 baseFee/blobFee 但不触发子池搬移
    // 必须先于账户更新，确保账户更新时使用新的费用值
    let (prev_base_fee, prev_blob_fee) =
        self.update_pending_fees_only(block_info.pending_basefee, block_info.pending_blob_fee);

    // 步骤3：根据账户余额/nonce 变化，触发子池搬移
    // 余额增加 → 可能从 Queued 晋升；nonce 推进 → 移除过期交易，解锁后继
    let mut outcome = self.update_accounts(changed_senders);

    // 步骤4：根据 baseFee/blobFee 变化，触发费用相关的子池搬移
    // baseFee 下降 → BaseFee → Pending；baseFee 上升 → Pending → BaseFee
    self.apply_fee_updates(prev_base_fee, prev_blob_fee, &mut outcome);

    // 步骤5：更新区块信息（不再触发费用更新）
    self.all_transactions.set_block_info(block_info);

    OnNewCanonicalStateOutcome {
        block_hash,
        mined: mined_transactions,
        promoted: outcome.promoted,  // 被晋升到 Pending 的交易
        discarded: outcome.discarded, // 被丢弃的无效交易
    }
}
```

**步骤2和步骤3的顺序至关重要**：先更新费用值，再处理账户变更，确保所有账户更新都在新的费用语境下计算，避免同一笔交易因费用和账户变化被重复搬移。

处理完成后，`PoolInner` 会：
- 清理废弃的 blob sidecar（`delete_discarded_blobs`）
- 向监听器广播晋升和丢弃事件（`notify_on_new_state`）

### 子池间的搬移方向

```
baseFee 下降：  BaseFee → Pending
baseFee 上升：  Pending → BaseFee
blobFee 下降：  Blob    → Pending（若其他条件也满足）
blobFee 上升：  Pending → Blob
余额/nonce 更新：Queued  → BaseFee 或 Pending（视费用条件）
```

---

## P2P 与 RPC 集成

### RPC 路径

`eth_sendRawTransaction` 处理流程：
1. RPC 服务端解码原始字节，恢复发送方地址（`Recovered<Tx>`）
2. 调用 `pool.add_transaction(Origin::Local, tx)`
3. 验证、入池，返回交易哈希

### P2P 路径

文件：`crates/net/network/src/transactions/mod.rs`

```
P2P 网络对等节点
  ↓
Transactions 消息（完整交易体）
  或
NewPooledTransactionHashes 消息（仅哈希）
  ↓
TransactionsManager
  ├── Transactions 消息：
  │     过滤已知 → 标记 peer 持有 → 排队导入
  └── NewPooledTransactionHashes 消息：
        过滤已知 → 发送 GetPooledTransactions 请求
        → 接收 PooledTransactions 响应
        → 排队导入
  ↓
pool.add_external_transactions(Origin::External, txs)
```

`Origin::External` 的交易受到更严格的验证（如最低优先级费用检查），而 `Origin::Local` 的交易可以设置更宽松的费用阈值（由 `LocalTransactionConfig` 控制）。

### 交易传播

新的 pending 交易会通过 `pending_transaction_listener` 通知给 `TransactionsManager`，后者负责将交易哈希通过 `NewPooledTransactionHashes` 消息广播给已连接的对等节点（但仅传播 `propagate: true` 的交易）。

---

## 关键设计决策

### 为什么不直接用交易哈希作为 ID？

交易哈希是 32 字节，用于内存索引和排序开销较大。`TransactionId` 是 16 字节（`u64 + u64`），且 `BTreeMap<TransactionId, _>` 利用有序性快速遍历同一发送方的所有交易。

### 为什么 TxState 使用 bitflags？

单个 u8 值编码所有条件，条件变化时只需翻转对应位并重新计算子池归属，无需重新运行复杂逻辑。从 `TxState → SubPool` 的映射是 O(1) 操作。

### 为什么 blob sidecar 分开存储？

Blob sidecar 可能有几百 KB，而 `ValidPoolTransaction` 需要在多个子池间高效移动。分离存储后，`ValidPoolTransaction` 保持紧凑（只含哈希引用），而 sidecar 在 `BlobStore` 中只存一份。此外，在 reorg 重注入时，可以省略已存在的 sidecar（返回 `ValidTransaction::Valid` 而非 `ValidWithSidecar`）。

---

## 小结

reth 交易池的设计体现了以下核心思想：

1. **状态驱动的分类**：`TxState` bitflags 将"为什么这笔交易还不能执行"编码为可高效更新的标志位，子池归属由标志位决定，变更时精准搬移

2. **分层锁设计**：写锁只在 `TxPool` 层（实际的子池操作），blob sidecar 存储在锁外异步完成，最小化锁持有时间

3. **惰性标识映射**：`Address → SenderId(u64)` 的转换仅在交易首次进入池时发生，之后的所有操作使用紧凑数字 ID

4. **快照迭代器**：`BestTransactions` 创建时克隆 pending 池的 `by_id` 快照，迭代过程中不持锁，但通过 `new_transaction_receiver` channel 接收迭代期间新加入的交易

5. **精确的状态传播**：新区块处理时，先更新费用值再处理账户变更，避免重复搬移；`apply_fee_updates` 作为最后一步，统一处理费用驱动的子池移动

---

**下一章**：[第6章：RPC 层](06-rpc.md)
