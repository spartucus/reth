# Reth的性能实现

> Reth's core goals prioritize performance by optimizing state access and I/O operations, leading to faster synchronization times and reduced operational costs. reth代码库中是如何做的?

这句话的核心含义是：Reth 认为以太坊节点的性能瓶颈**不在 EVM 执行本身，而在状态读取和磁盘 I/O**，因此围绕这一点做了大量系统性优化。

官方设计文档明确写道：

> "The biggest bottleneck in this pipeline is not the execution of the EVM interpreter itself, but rather in accessing state and managing I/O." [1](#0-0) 

交易处理的完整链路是：`RPC -> EVM -> Cache -> Codec -> DB`，优化重点集中在靠近 DB 的层。

---

## 代码库中的具体实现

### 1. MDBX 内存映射数据库 + 精细调优

Reth 使用 MDBX 作为主数据库，它通过内存映射（mmap）实现高性能 I/O。Reth 在初始化时做了针对性配置：

- **禁用 read-ahead**（`no_rdahead: true`）：因为节点的访问模式是随机读，而非顺序扫描，预读反而会降低性能。
- **启用 coalesce**：合并写操作，减少磁盘 I/O 次数。 [2](#0-1) 

### 2. Staged Sync（分阶段同步）

Reth 采用 Erigon 风格的分阶段同步流水线（`Pipeline`），将同步拆分为多个串行阶段（`HeaderStage` → `BodyStage` → `ExecutionStage` → `MerkleStage` 等）。每个阶段批量处理一段区块范围，将随机 I/O 转化为顺序 I/O，大幅提升吞吐量。 [3](#0-2) 

各阶段的具体职责见： [4](#0-3) 

### 3. 跨块执行缓存（Cross-block Execution Cache）

`ExecutionCache` 在内存中维护账户、存储槽、字节码的固定大小缓存，`PayloadExecutionCache` 则将上一个区块的缓存状态传递给下一个区块，避免重复的数据库查询。

```
ExecutionCacheInner {
    code_cache:    FixedCache<B256, Option<Bytecode>>
    storage_cache: FixedCache<(Address, StorageKey), StorageValue>
    account_cache: FixedCache<Address, Option<Account>>
}
``` [5](#0-4) [6](#0-5) 

### 4. 并行预热（Parallel Prewarming）

在实际执行区块之前，`PrewarmCacheTask` 会**并行地提前执行交易**，将即将访问的状态提前加载进缓存，使正式执行时命中缓存而非访问磁盘。 [7](#0-6) 

预热任务通过 `spawn_caching_with` 在独立线程中运行： [8](#0-7) 

### 5. 静态文件（Static Files / NippyJar）

历史上已最终确定、不再变更的数据（区块头、交易、收据等）从 MDBX 中剥离，存入基于列式压缩的 `NippyJar` 静态文件，通过 mmap 读取。这样既减小了热数据库的体积，又利用了列式存储对顺序读的优势。 [9](#0-8) [10](#0-9) 

### 6. Storage V2：冷热数据分离

Reth 2.0 默认启用 V2 存储布局，将数据按访问频率分层：

| 数据类型 | 存储位置 |
|---|---|
| 历史索引（账户/存储/交易哈希） | RocksDB |
| 账户/存储变更集、收据 | 静态文件 |
| 当前状态（热数据） | MDBX |

这使磁盘占用减少 23%~50%，同时让 MDBX 只保存热数据，提升缓存命中率。 [11](#0-10) [12](#0-11) 

### 7. 稀疏 Merkle Trie 缓存

`SparseStateTrie` 在区块间保持内存中的 trie 节点缓存，避免每个区块都从磁盘重新加载 trie 节点。配合 LFU 热槽/热账户策略（`sparse_trie_max_hot_slots`、`sparse_trie_max_hot_accounts`）控制内存占用。 [13](#0-12) 

### 8. PrefixSet 顺序访问优化

在 trie 遍历时，`PrefixSet::contains` 维护一个内部游标 `index`，利用键的有序性跳过已扫描区域，避免每次都全量扫描，显著提升增量状态根计算性能。 [14](#0-13) 

---

## 总结

```
性能优化层次：
┌─────────────────────────────────────────────────────┐
│  应用层：预热(Prewarming) + 跨块缓存(ExecutionCache) │  ← 减少 DB 访问次数
├─────────────────────────────────────────────────────┤
│  存储层：Static Files + Storage V2 冷热分离          │  ← 减小热 DB 体积
├─────────────────────────────────────────────────────┤
│  数据库层：MDBX mmap + no_rdahead + coalesce        │  ← 优化 I/O 模式
├─────────────────────────────────────────────────────┤
│  同步层：Staged Sync 批量顺序处理                    │  ← 随机→顺序 I/O
└─────────────────────────────────────────────────────┘
```

这些优化共同作用，使得同步速度更快（批量顺序 I/O）、运行时延迟更低（缓存命中避免磁盘读）、存储成本更低（冷热分离 + 压缩）。

### Citations

**File:** docs/design/goals.md (L27-37)
```markdown
The pipeline that a given transaction goes through as it’s processed is more or less the following:

RPC -> EVM -> Cache -> Codec -> DB

One of our first and foremost goals in Reth is to minimize the latency and maximize the throughput (think: request concurrency) of this pipeline.

Why? This is a win for everyone. RPC providers meet more impressive SLAs, MEV searchers become more effective, home nodes sync faster, etc.

The biggest bottleneck in this pipeline is not the execution of the EVM interpreter itself, but rather in accessing state and managing I/O. As such, we think the largest optimizations to be made are closest to the DB layer.

Ideally, we can achieve such fast runtime operation that we can avoid storing certain things (e.g., transaction receipts) on the disk, and are able to generate them on the fly, instead - minimizing disk footprint.
```

**File:** crates/storage/db/src/implementation/mdbx/mod.rs (L467-475)
```rust
        inner_env.set_flags(EnvironmentFlags {
            mode,
            // We disable readahead because it improves performance for linear scans, but
            // worsens it for random access (which is our access pattern outside of sync)
            no_rdahead: true,
            coalesce: true,
            exclusive: args.exclusive.unwrap_or_default(),
            ..Default::default()
        });
```

**File:** crates/stages/api/src/pipeline/mod.rs (L47-68)
```rust
#[cfg_attr(doc, aquamarine::aquamarine)]
/// A staged sync pipeline.
///
/// The pipeline executes queued [stages][Stage] serially. An external component determines the tip
/// of the chain and the pipeline then executes each stage in order from the current local chain tip
/// and the external chain tip. When a stage is executed, it will run until it reaches the chain
/// tip.
///
/// After the entire pipeline has been run, it will run again unless asked to stop (see
/// [`Pipeline::set_max_block`]).
///
/// `include_mmd!("docs/mermaid/pipeline.mmd`")
///
/// # Unwinding
///
/// In case of a validation error (as determined by the consensus engine) in one of the stages, the
/// pipeline will unwind the stages in reverse order of execution. It is also possible to
/// request an unwind manually (see [`Pipeline::unwind`]).
///
/// # Defaults
///
/// The [`DefaultStages`](crate::sets::DefaultStages) are used to fully sync reth.
```

**File:** docs/crates/stages.md (L1-20)
```markdown
# Stages

The `stages` lib plays a central role in syncing the node, maintaining state, updating the database and more. The stages involved in the Reth pipeline are queued up and stored within the Reth pipeline. In the default configuration, the pipeline runs the following stages in order:

- EraStage (optional, for ERA1 import)
- HeaderStage
- BodyStage
- SenderRecoveryStage
- ExecutionStage
- PruneSenderRecoveryStage (if pruning for sender recovery is enabled)
- MerkleStage (unwind)
- AccountHashingStage
- StorageHashingStage
- MerkleStage (execute)
- TransactionLookupStage
- IndexStorageHistoryStage
- IndexAccountHistoryStage
- PruneStage
- FinishStage

```

**File:** crates/engine/execution-cache/src/cached_state.rs (L661-697)
```rust
/// Execution cache used during block processing.
///
/// Optimizes state access by maintaining in-memory copies of frequently accessed
/// accounts, storage slots, and bytecode. Works in conjunction with prewarming
/// to reduce database I/O during block execution.
///
/// ## Storage Invalidation
///
/// Since EIP-6780, SELFDESTRUCT only works within the same transaction where the
/// contract was created, so we don't need to handle clearing the storage.
#[derive(Debug, Clone)]
pub struct ExecutionCache(Arc<ExecutionCacheInner>);

/// Inner state of the [`ExecutionCache`], wrapped in a single [`Arc`].
#[derive(Debug)]
struct ExecutionCacheInner {
    /// Cache for contract bytecode, keyed by code hash.
    code_cache: FixedCache<B256, Option<Bytecode>, FbBuildHasher<32>>,

    /// Flat storage cache: maps `(Address, StorageKey)` to storage value.
    storage_cache: FixedCache<(Address, StorageKey), StorageValue>,

    /// Cache for basic account information (nonce, balance, code hash).
    account_cache: FixedCache<Address, Option<Account>, FbBuildHasher<20>>,

    /// Stats handler for the code cache (shared with the cache via [`Stats`]).
    code_stats: Arc<CacheStatsHandler>,

    /// Stats handler for the storage cache (shared with the cache via [`Stats`]).
    storage_stats: Arc<CacheStatsHandler>,

    /// Stats handler for the account cache (shared with the cache via [`Stats`]).
    account_stats: Arc<CacheStatsHandler>,

    /// One-time notification when SELFDESTRUCT is encountered
    selfdestruct_encountered: Once,
}
```

**File:** crates/engine/execution-cache/src/lib.rs (L28-47)
```rust
/// A guarded, thread-safe cache of execution state that tracks the most recent block's caches.
///
/// This is the cross-block cache used to accelerate sequential payload processing.
/// When a new block arrives, its parent's cached state can be reused to avoid
/// redundant database lookups.
///
/// This process assumes that payloads are received sequentially.
///
/// ## Cache Safety
///
/// **CRITICAL**: Cache update operations require exclusive access. All concurrent cache users
/// (such as prewarming tasks) must be terminated before calling
/// [`PayloadExecutionCache::update_with_guard`], otherwise the cache may be corrupted or cleared.
#[derive(Clone, Debug, Default)]
pub struct PayloadExecutionCache {
    /// Guarded cloneable cache identified by a block hash.
    inner: Arc<Mutex<Option<SavedCache>>>,
    /// Metrics for cache operations.
    metrics: PayloadExecutionCacheMetrics,
}
```

**File:** crates/engine/tree/src/tree/payload_processor/prewarm.rs (L1-13)
```rust
//! Caching and prewarming related functionality.
//!
//! Prewarming executes transactions in parallel before the actual block execution
//! to populate the execution cache with state that will likely be accessed during
//! block processing.
//!
//! ## How Prewarming Works
//!
//! 1. Incoming transactions are split into two streams: one for prewarming (executed in parallel)
//!    and one for actual execution (executed sequentially)
//! 2. Prewarming tasks execute transactions in parallel using shared caches
//! 3. When actual block execution happens, it benefits from the warmed cache

```

**File:** crates/engine/tree/src/tree/payload_processor/mod.rs (L136-150)
```rust
    /// A pruned `SparseStateTrie`, kept around as a cache of already revealed trie nodes and to
    /// re-use allocated memory. Stored with the block hash it was computed for to enable trie
    /// preservation across sequential payload validations.
    sparse_state_trie: SharedPreservedSparseTrie,
    /// LFU hot-slot capacity: max storage slots retained across prune cycles.
    sparse_trie_max_hot_slots: usize,
    /// LFU hot-account capacity: max account addresses retained across prune cycles.
    sparse_trie_max_hot_accounts: usize,
    /// Whether sparse trie cache pruning is fully disabled.
    disable_sparse_trie_cache_pruning: bool,
    /// Whether to disable BAL-driven parallel state root computation.
    /// Only valid when BAL parallel execution is also disabled.
    disable_bal_parallel_state_root: bool,
    /// Whether BAL state prefetching during prewarm is disabled.
    disable_bal_batch_io: bool,
```

**File:** crates/engine/tree/src/tree/payload_processor/mod.rs (L493-546)
```rust
    #[instrument(level = "debug", target = "engine::tree::payload_processor", skip_all)]
    fn spawn_caching_with<P>(
        &self,
        env: ExecutionEnv<Evm>,
        transactions: mpsc::Receiver<(usize, impl ExecutableTxFor<Evm> + Clone + Send + 'static)>,
        provider_builder: StateProviderBuilder<N, P>,
        to_sparse_trie_task: Option<CrossbeamSender<StateRootMessage>>,
        parallel_bal_execution: bool,
    ) -> CacheTaskHandle<N::Receipt>
    where
        P: BlockReader + StateProviderFactory + StateReader + Clone + 'static,
    {
        let mode = if parallel_bal_execution {
            PrewarmMode::BlockAccessList(
                env.decoded_bal.clone().expect("BAL dispatch implies decoded BAL"),
            )
        } else if self.disable_transaction_prewarming ||
            env.transaction_count < SMALL_BLOCK_TX_THRESHOLD
        {
            PrewarmMode::Skipped
        } else {
            PrewarmMode::Transactions(transactions)
        };
        let saved_cache = self.disable_state_cache.not().then(|| self.cache_for(env.parent_hash));

        let executed_tx_index = Arc::new(AtomicUsize::new(0));
        // configure prewarming
        let prewarm_ctx = PrewarmContext {
            env,
            evm_config: self.evm_config.clone(),
            saved_cache: saved_cache.clone(),
            provider: provider_builder,
            metrics: PrewarmMetrics::default(),
            cache_metrics: self.cache_metrics.clone(),
            terminate_execution: Arc::new(AtomicBool::new(false)),
            executed_tx_index: Arc::clone(&executed_tx_index),
            precompile_cache_disabled: self.precompile_cache_disabled,
            precompile_cache_map: self.precompile_cache_map.clone(),
            disable_bal_parallel_state_root: self.disable_bal_parallel_state_root,
            disable_bal_batch_io: self.disable_bal_batch_io,
        };

        let (prewarm_task, to_prewarm_task) = PrewarmCacheTask::new(
            self.executor.clone(),
            self.execution_cache.clone(),
            prewarm_ctx,
            to_sparse_trie_task,
        );
        {
            let to_prewarm_task = to_prewarm_task.clone();
            self.executor.spawn_blocking_named("prewarm", move || {
                prewarm_task.run(mode, to_prewarm_task);
            });
        }
```

**File:** crates/static-file/static-file/README.md (L1-9)
```markdown
# StaticFile

## Overview

Data that has reached a finalized state and won't undergo further changes (essentially frozen) should be read without concerns about modification. This makes it unsuitable for traditional databases.

This crate aims to copy this data from the current database to multiple static files, aggregated by block ranges. At every 500_000th block, a new static file is created.

Below are four diagrams illustrating how data is served from static files to the provider. A glossary is also provided to explain the different (linked) components involved in these processes.
```

**File:** crates/storage/nippy-jar/src/lib.rs (L84-90)
```rust
/// `NippyJar` is a specialized storage format designed for immutable data.
///
/// Data is organized into a columnar format, enabling column-based compression. Data retrieval
/// entails consulting an offset list and fetching the data from file via `mmap`.
#[derive(Serialize, Deserialize)]
#[cfg_attr(test, derive(PartialEq))]
pub struct NippyJar<H = ()> {
```

**File:** crates/storage/db-api/src/models/metadata.rs (L16-92)
```rust
pub struct StorageSettings {
    /// Whether this node uses v2 storage layout.
    ///
    /// When `true`, enables all v2 storage features:
    /// - Receipts and transaction senders in static files
    /// - History indices in `RocksDB` (accounts, storages, transaction hashes)
    /// - Account and storage changesets in static files
    /// - Hashed state tables as canonical state representation
    ///
    /// When `false`, uses v1/legacy layout (everything in MDBX).
    pub storage_v2: bool,
}

impl StorageSettings {
    /// Returns the default base `StorageSettings`.
    pub const fn base() -> Self {
        Self::v2()
    }

    /// Creates `StorageSettings` for v2 nodes with all storage features enabled:
    /// - Receipts and transaction senders in static files
    /// - History indices in `RocksDB` (storages, accounts, transaction hashes)
    /// - Account and storage changesets in static files
    /// - Hashed state as canonical state representation
    ///
    /// Use this when the `--storage.v2` CLI flag is set.
    pub const fn v2() -> Self {
        Self { storage_v2: true }
    }

    /// Creates `StorageSettings` for v1/legacy nodes.
    ///
    /// This keeps all data in MDBX, matching the original storage layout.
    pub const fn v1() -> Self {
        Self { storage_v2: false }
    }

    /// Returns `true` if this node uses v2 storage layout.
    pub const fn is_v2(&self) -> bool {
        self.storage_v2
    }

    /// Whether receipts are stored in static files.
    pub const fn receipts_in_static_files(&self) -> bool {
        self.storage_v2
    }

    /// Whether transaction senders are stored in static files.
    pub const fn transaction_senders_in_static_files(&self) -> bool {
        self.storage_v2
    }

    /// Whether storages history is stored in `RocksDB`.
    pub const fn storages_history_in_rocksdb(&self) -> bool {
        self.storage_v2
    }

    /// Whether transaction hash numbers are stored in `RocksDB`.
    pub const fn transaction_hash_numbers_in_rocksdb(&self) -> bool {
        self.storage_v2
    }

    /// Whether account history is stored in `RocksDB`.
    pub const fn account_history_in_rocksdb(&self) -> bool {
        self.storage_v2
    }

    /// Whether to use hashed state tables (`HashedAccounts`/`HashedStorages`) as the canonical
    /// state representation instead of plain state tables. Implied by v2 storage layout.
    pub const fn use_hashed_state(&self) -> bool {
        self.storage_v2
    }

    /// Returns `true` if any tables are configured to be stored in `RocksDB`.
    pub const fn any_in_rocksdb(&self) -> bool {
        self.storage_v2
    }
```

**File:** docs/vocs/docs/pages/run/storage.mdx (L25-34)
```text
## Disk savings

Measured on disk at block `24,396,823` on Ethereum mainnet:

| Node Type | Legacy V1 storage | V2 storage | Savings |
| --------- | ----------------- | ---------- | ------- |
| Full      | 1.46 TB           | 1.02 TB    | **-30%** |
| Minimal   | 449 GB            | 224 GB     | **-50%** |
| Archive   | 2.99 TB           | 2.31 TB    | **-23%** |

```

**File:** crates/trie/common/src/prefix_set.rs (L189-227)
```rust
impl PrefixSet {
    /// Returns `true` if any of the keys in the set has the given prefix
    ///
    /// # Note on Mutability
    ///
    /// This method requires `&mut self` (unlike typical `contains` methods) because it maintains an
    /// internal position tracker (`self.index`) between calls. This enables significant performance
    /// optimization for sequential lookups in sorted order, which is common during trie traversal.
    ///
    /// The `index` field allows subsequent searches to start where previous ones left off,
    /// avoiding repeated full scans of the prefix array when keys are accessed in nearby ranges.
    ///
    /// This optimization was inspired by Silkworm's implementation and significantly improves
    /// incremental state root calculation performance
    /// ([see PR #2417](https://github.com/paradigmxyz/reth/pull/2417)).
    #[inline]
    pub fn contains(&mut self, prefix: &Nibbles) -> bool {
        if self.all {
            return true
        }

        while self.index > 0 && &self.keys[self.index] > prefix {
            self.index -= 1;
        }

        for (idx, key) in self.keys[self.index..].iter().enumerate() {
            if key.starts_with(prefix) {
                self.index += idx;
                return true
            }

            if key > prefix {
                self.index += idx;
                return false
            }
        }

        false
    }
```